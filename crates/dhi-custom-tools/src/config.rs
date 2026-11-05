//! The custom-tool catalog: load, validate, and cap.
//!
//! The file is deliberately forgiving: a missing file is an empty catalog, a
//! malformed entry is skipped with a warning, and only a completely
//! unreadable or unparsable file empties the whole catalog. Boot never fails
//! because of this file.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::Method;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use serde_json::{Map, Value};
use url::Url;

/// `DHI_CUSTOM_TOOLS_CONFIG` default.
pub(crate) const DEFAULT_CONFIG_PATH: &str = "ops/custom-tools.json";

/// The largest catalog file accepted.
pub(crate) const MAX_CONFIG_BYTES: usize = 64 * 1024;
/// The largest tool-call input accepted (serialized JSON).
pub(crate) const MAX_INPUT_BYTES: usize = 64 * 1024;
/// The most tools one catalog may register.
pub(crate) const MAX_TOOLS: usize = 64;
/// The largest accepted request/command timeout.
pub(crate) const MAX_TIMEOUT_SECS: u64 = 300;
/// The largest accepted response or output cap.
pub(crate) const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

pub(crate) const DEFAULT_HTTP_TIMEOUT_SECS: u64 = 30;
pub(crate) const DEFAULT_COMMAND_TIMEOUT_SECS: u64 = 30;
pub(crate) const DEFAULT_MAX_RESPONSE_BYTES: usize = 1024 * 1024;
pub(crate) const DEFAULT_MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// A model-facing description longer than this is a malformed entry.
const MAX_DESCRIPTION_CHARS: usize = 4096;

/// One validated custom tool.
#[derive(Debug, Clone)]
pub(crate) struct CustomTool {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) input_schema: Value,
    pub(crate) exec: Exec,
}

#[derive(Debug, Clone)]
pub(crate) enum Exec {
    Http(HttpExec),
    Command(CommandExec),
}

/// Executes one HTTP request per call. Fixed headers are applied verbatim and
/// are never printed by `Debug`.
#[derive(Clone)]
pub(crate) struct HttpExec {
    pub(crate) method: Method,
    pub(crate) url: Url,
    pub(crate) headers: HeaderMap,
    pub(crate) timeout: Duration,
    pub(crate) max_response_bytes: usize,
    pub(crate) allow_private: bool,
}

/// Executes one program directly (no shell) per call.
#[derive(Clone)]
pub(crate) struct CommandExec {
    pub(crate) command: String,
    pub(crate) args: Vec<String>,
    /// An explicit map replaces the inherited environment. Only `PATH` is
    /// added by the runtime; no other ambient variable is visible.
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) timeout: Duration,
    pub(crate) max_output_bytes: usize,
}

impl fmt::Debug for HttpExec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Header values can carry API keys, so only names appear here.
        let names: Vec<&str> = self.headers.keys().map(HeaderName::as_str).collect();
        formatter
            .debug_struct("HttpExec")
            .field("method", &self.method)
            .field("url", &self.url.as_str())
            .field("header_names", &names)
            .field("timeout", &self.timeout)
            .field("max_response_bytes", &self.max_response_bytes)
            .field("allow_private", &self.allow_private)
            .finish()
    }
}

impl fmt::Debug for CommandExec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Environment values can carry secrets, so only keys appear here.
        formatter
            .debug_struct("CommandExec")
            .field("command", &self.command)
            .field("args", &self.args)
            .field("env_keys", &self.env.keys().collect::<Vec<_>>())
            .field("timeout", &self.timeout)
            .field("max_output_bytes", &self.max_output_bytes)
            .finish()
    }
}

/// The validated catalog plus the human-readable reasons entries were
/// skipped. The runtime logs each warning once at load time.
#[derive(Debug, Default)]
pub(crate) struct Catalog {
    pub(crate) tools: BTreeMap<String, CustomTool>,
    pub(crate) warnings: Vec<String>,
}

/// The effective config path: `DHI_CUSTOM_TOOLS_CONFIG` wins, else
/// `ops/custom-tools.json` relative to the process working directory.
pub(crate) fn config_path() -> PathBuf {
    match std::env::var("DHI_CUSTOM_TOOLS_CONFIG") {
        Ok(raw) if !raw.trim().is_empty() => PathBuf::from(raw.trim()),
        _ => PathBuf::from(DEFAULT_CONFIG_PATH),
    }
}

/// Loads a catalog from `path`. A missing file is an empty catalog.
pub(crate) fn load_catalog(path: &Path) -> Catalog {
    let mut catalog = Catalog::default();
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return catalog,
        Err(err) => {
            catalog
                .warnings
                .push(format!("cannot stat {}: {err}", path.display()));
            return catalog;
        }
    };
    if metadata.len() > MAX_CONFIG_BYTES as u64 {
        catalog.warnings.push(format!(
            "{} is {} bytes; the custom-tool catalog is capped at {} bytes and was ignored",
            path.display(),
            metadata.len(),
            MAX_CONFIG_BYTES
        ));
        return catalog;
    }
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) => {
            catalog
                .warnings
                .push(format!("cannot read {}: {err}", path.display()));
            return catalog;
        }
    };
    match parse_catalog(&raw) {
        Ok(parsed) => parsed,
        Err(err) => {
            catalog.warnings.push(format!(
                "{} is not a valid catalog: {err:#}",
                path.display()
            ));
            catalog
        }
    }
}

/// Parses and validates one catalog document.
pub(crate) fn parse_catalog(raw: &str) -> Result<Catalog> {
    if raw.len() > MAX_CONFIG_BYTES {
        bail!("catalog is larger than {MAX_CONFIG_BYTES} bytes");
    }
    let file: CatalogFile =
        serde_json::from_str(raw).context("catalog is not valid JSON with a tools object")?;
    let mut catalog = Catalog::default();
    for (name, value) in file.tools {
        if catalog.tools.len() >= MAX_TOOLS {
            catalog.warnings.push(format!(
                "custom tool {name:?} skipped: the catalog already has the maximum of {MAX_TOOLS} tools"
            ));
            continue;
        }
        if catalog.tools.contains_key(&name) {
            catalog
                .warnings
                .push(format!("custom tool {name:?} skipped: duplicate tool name"));
            continue;
        }
        match validate_entry(&name, &value) {
            Ok(tool) => {
                catalog.tools.insert(name, tool);
            }
            Err(err) => catalog
                .warnings
                .push(format!("custom tool {name:?} skipped: {err:#}")),
        }
    }
    Ok(catalog)
}

fn validate_entry(name: &str, value: &Value) -> Result<CustomTool> {
    validate_name(name)?;
    let entry = value
        .as_object()
        .ok_or_else(|| anyhow!("the tool entry must be a JSON object"))?;
    let description = entry
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| anyhow!("description must be a non-empty string"))?;
    if description.chars().count() > MAX_DESCRIPTION_CHARS {
        bail!("description must be at most {MAX_DESCRIPTION_CHARS} characters");
    }
    let input_schema = entry
        .get("inputSchema")
        .ok_or_else(|| anyhow!("inputSchema is required"))?;
    let schema_object = input_schema
        .as_object()
        .ok_or_else(|| anyhow!("inputSchema must be a JSON object"))?;
    if let Some(kind) = schema_object.get("type")
        && kind.as_str() != Some("object")
    {
        bail!("inputSchema.type must be \"object\"");
    }
    if serde_json::to_vec(input_schema)
        .context("inputSchema does not serialize")?
        .len()
        > MAX_INPUT_BYTES
    {
        bail!("inputSchema must be at most {MAX_INPUT_BYTES} bytes");
    }
    let exec = validate_exec(
        entry
            .get("exec")
            .ok_or_else(|| anyhow!("exec is required"))?,
    )?;
    Ok(CustomTool {
        name: name.to_string(),
        description: description.to_string(),
        input_schema: input_schema.clone(),
        exec,
    })
}

/// `[A-Za-z0-9_-]{1,64}`, the only names the guarded registry can carry
/// without escaping.
pub(crate) fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        bail!("tool names must be 1 to 64 characters");
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        bail!("tool names may contain only ASCII letters, digits, '-' and '_'");
    }
    Ok(())
}

fn validate_exec(value: &Value) -> Result<Exec> {
    let exec = value
        .as_object()
        .ok_or_else(|| anyhow!("exec must be a JSON object"))?;
    match exec.get("kind").and_then(Value::as_str) {
        Some("http") => Ok(Exec::Http(validate_http(exec)?)),
        Some("command") => Ok(Exec::Command(validate_command(exec)?)),
        Some(other) => bail!("exec.kind must be \"http\" or \"command\"; got {other:?}"),
        None => bail!("exec.kind must be \"http\" or \"command\""),
    }
}

fn validate_http(exec: &Map<String, Value>) -> Result<HttpExec> {
    let raw_method = exec
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("GET")
        .to_ascii_uppercase();
    let method = match raw_method.as_str() {
        "GET" => Method::GET,
        "DELETE" => Method::DELETE,
        "POST" => Method::POST,
        "PUT" => Method::PUT,
        "PATCH" => Method::PATCH,
        other => bail!("http method must be GET, DELETE, POST, PUT, or PATCH; got {other:?}"),
    };
    let raw_url = exec
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("http url must be a string"))?;
    let url = Url::parse(raw_url).with_context(|| format!("invalid http url {raw_url:?}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("http url must use http or https");
    }
    if url.host_str().is_none() {
        bail!("http url must include a host");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("http url must not include credentials");
    }
    let headers = match exec.get("headers") {
        Some(value) => parse_headers(value)?,
        None => HeaderMap::new(),
    };
    let timeout_secs = read_capped_u64(
        exec,
        "timeout_secs",
        DEFAULT_HTTP_TIMEOUT_SECS,
        MAX_TIMEOUT_SECS,
    )?;
    let max_response_bytes = read_capped_usize(
        exec,
        "max_response_bytes",
        DEFAULT_MAX_RESPONSE_BYTES,
        MAX_OUTPUT_BYTES,
    )?;
    let allow_private = match exec.get("allow_private") {
        Some(value) => value
            .as_bool()
            .ok_or_else(|| anyhow!("allow_private must be a boolean"))?,
        None => false,
    };
    Ok(HttpExec {
        method,
        url,
        headers,
        timeout: Duration::from_secs(timeout_secs),
        max_response_bytes,
        allow_private,
    })
}

fn parse_headers(value: &Value) -> Result<HeaderMap> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("http headers must be an object of string values"))?;
    let mut headers = HeaderMap::new();
    for (name, value) in object {
        let header_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| anyhow!("http header name {name:?} is invalid"))?;
        let raw_value = value
            .as_str()
            .ok_or_else(|| anyhow!("http header {name:?} must be a string"))?;
        let header_value = HeaderValue::from_str(raw_value)
            .map_err(|_| anyhow!("http header {name:?} has an invalid value"))?;
        headers.insert(header_name, header_value);
    }
    Ok(headers)
}

fn validate_command(exec: &Map<String, Value>) -> Result<CommandExec> {
    let command = exec
        .get("command")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| anyhow!("command must be a non-empty string"))?;
    reject_control("command", command)?;
    let args = match exec.get("args") {
        Some(value) => value
            .as_array()
            .ok_or_else(|| anyhow!("command args must be an array of strings"))?
            .iter()
            .enumerate()
            .map(|(index, arg)| {
                let arg = arg
                    .as_str()
                    .ok_or_else(|| anyhow!("command args[{index}] must be a string"))?;
                reject_control("command argument", arg)?;
                Ok(arg.to_string())
            })
            .collect::<Result<Vec<_>>>()?,
        None => Vec::new(),
    };
    let env = match exec.get("env") {
        Some(value) => {
            let object = value
                .as_object()
                .ok_or_else(|| anyhow!("command env must be an object of string values"))?;
            let mut env = BTreeMap::new();
            for (key, value) in object {
                if key.is_empty() || key.contains('=') {
                    bail!(
                        "command environment variable names must be non-empty and contain no '='"
                    );
                }
                reject_control("environment variable name", key)?;
                let value = value.as_str().ok_or_else(|| {
                    anyhow!("command environment variable {key:?} must be a string")
                })?;
                reject_control("environment variable value", value)?;
                env.insert(key.clone(), value.to_string());
            }
            env
        }
        None => BTreeMap::new(),
    };
    let timeout_secs = read_capped_u64(
        exec,
        "timeout_secs",
        DEFAULT_COMMAND_TIMEOUT_SECS,
        MAX_TIMEOUT_SECS,
    )?;
    let max_output_bytes = read_capped_usize(
        exec,
        "max_output_bytes",
        DEFAULT_MAX_OUTPUT_BYTES,
        MAX_OUTPUT_BYTES,
    )?;
    Ok(CommandExec {
        command: command.to_string(),
        args,
        env,
        timeout: Duration::from_secs(timeout_secs),
        max_output_bytes,
    })
}

fn read_capped_u64(exec: &Map<String, Value>, field: &str, default: u64, max: u64) -> Result<u64> {
    let value = match exec.get(field) {
        Some(value) => value
            .as_u64()
            .ok_or_else(|| anyhow!("{field} must be a positive integer"))?,
        None => default,
    };
    if value == 0 || value > max {
        bail!("{field} must be between 1 and {max}");
    }
    Ok(value)
}

fn read_capped_usize(
    exec: &Map<String, Value>,
    field: &str,
    default: usize,
    max: usize,
) -> Result<usize> {
    let value = read_capped_u64(exec, field, default as u64, max as u64)?;
    usize::try_from(value).map_err(|_| anyhow!("{field} is out of range"))
}

/// Rejects C0/C1 control characters (including NUL): direct spawning never
/// needs them, and they hide arguments from logs.
pub(crate) fn reject_control(what: &str, text: &str) -> Result<()> {
    if text.chars().any(char::is_control) {
        bail!("{what} must not contain control characters or NUL");
    }
    Ok(())
}

/// A duplicate-preserving view of the `tools` map.
///
/// `serde_json::Value` collapses duplicate object keys, so a custom visitor is
/// the only way to report `{"tools":{"x":{...},"x":{...}}}` instead of
/// silently keeping the last entry.
#[derive(Deserialize)]
struct CatalogFile {
    #[serde(default, deserialize_with = "deserialize_tool_entries")]
    tools: Vec<(String, Value)>,
}

fn deserialize_tool_entries<'de, D>(deserializer: D) -> Result<Vec<(String, Value)>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct EntriesVisitor;

    impl<'de> serde::de::Visitor<'de> for EntriesVisitor {
        type Value = Vec<(String, Value)>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("an object mapping tool names to definitions")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            let mut entries = Vec::new();
            while let Some((name, value)) = map.next_entry::<String, Value>()? {
                entries.push((name, value));
            }
            Ok(entries)
        }
    }

    deserializer.deserialize_map(EntriesVisitor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn http_tool(url: &str) -> Value {
        json!({
            "description": "Get weather",
            "inputSchema": {"type": "object", "properties": {"city": {"type": "string"}}},
            "exec": {"kind": "http", "method": "GET", "url": url}
        })
    }

    fn command_tool() -> Value {
        json!({
            "description": "Echo",
            "inputSchema": {"type": "object"},
            "exec": {"kind": "command", "command": "echo", "args": ["{message}"]}
        })
    }

    #[test]
    fn parses_http_and_command_tools() {
        let raw = json!({
            "tools": {
                "weather": http_tool("https://api.example.com/weather"),
                "echo": command_tool()
            }
        })
        .to_string();
        let catalog = parse_catalog(&raw).expect("valid catalog");
        assert_eq!(catalog.tools.len(), 2);
        let weather = &catalog.tools["weather"];
        assert_eq!(weather.description, "Get weather");
        match &weather.exec {
            Exec::Http(exec) => {
                assert_eq!(exec.method, Method::GET);
                assert_eq!(exec.timeout, Duration::from_secs(30));
                assert_eq!(exec.max_response_bytes, DEFAULT_MAX_RESPONSE_BYTES);
                assert!(!exec.allow_private);
            }
            other => panic!("expected http exec, got {other:?}"),
        }
        match &catalog.tools["echo"].exec {
            Exec::Command(exec) => {
                assert_eq!(exec.command, "echo");
                assert_eq!(exec.args, ["{message}"]);
            }
            other => panic!("expected command exec, got {other:?}"),
        }
    }

    #[test]
    fn missing_file_is_an_empty_catalog() {
        let dir = tempfile::tempdir().expect("tempdir");
        let catalog = load_catalog(&dir.path().join("absent.json"));
        assert!(catalog.tools.is_empty());
        assert!(catalog.warnings.is_empty());
    }

    #[test]
    fn malformed_entries_are_skipped() {
        let raw = json!({
            "tools": {
                "good": command_tool(),
                "bad name": command_tool(),
                "bad-schema": {
                    "description": "x",
                    "inputSchema": [],
                    "exec": {"kind": "http", "url": "https://example.com"}
                },
                "bad-kind": {
                    "description": "x",
                    "inputSchema": {"type": "object"},
                    "exec": {"kind": "shell", "command": "sh"}
                },
                "too-slow": {
                    "description": "x",
                    "inputSchema": {"type": "object"},
                    "exec": {"kind": "http", "url": "https://example.com", "timeout_secs": 301}
                },
                "too-big": {
                    "description": "x",
                    "inputSchema": {"type": "object"},
                    "exec": {"kind": "http", "url": "https://example.com", "max_response_bytes": 4194305}
                }
            }
        })
        .to_string();
        let catalog = parse_catalog(&raw).expect("valid document");
        assert_eq!(
            catalog.tools.keys().map(String::as_str).collect::<Vec<_>>(),
            ["good"]
        );
        assert_eq!(catalog.warnings.len(), 5);
        assert!(
            catalog
                .warnings
                .iter()
                .any(|warning| warning.contains("bad name"))
        );
        assert!(
            catalog
                .warnings
                .iter()
                .any(|warning| warning.contains("timeout_secs"))
        );
    }

    #[test]
    fn duplicate_tool_names_are_reported() {
        let raw = format!(
            r#"{{"tools":{{"echo":{},"echo":{}}}}}"#,
            command_tool(),
            command_tool()
        );
        let catalog = parse_catalog(&raw).expect("valid document");
        assert_eq!(catalog.tools.len(), 1);
        assert!(
            catalog
                .warnings
                .iter()
                .any(|warning| warning.contains("duplicate tool name"))
        );
    }

    #[test]
    fn catalog_is_capped_at_sixty_four_tools() {
        let mut tools = Map::new();
        for index in 0..(MAX_TOOLS + 1) {
            tools.insert(format!("tool_{index}"), command_tool());
        }
        let raw = json!({"tools": tools}).to_string();
        let catalog = parse_catalog(&raw).expect("valid document");
        assert_eq!(catalog.tools.len(), MAX_TOOLS);
        assert_eq!(catalog.warnings.len(), 1);
        assert!(catalog.warnings[0].contains("maximum"));
    }

    #[test]
    fn oversized_catalog_is_ignored() {
        let raw = "x".repeat(MAX_CONFIG_BYTES + 1);
        let error = parse_catalog(&raw).expect_err("oversized catalog");
        assert!(error.to_string().contains("larger"));
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("huge.json");
        fs::write(&path, &raw).expect("write");
        let catalog = load_catalog(&path);
        assert!(catalog.tools.is_empty());
        assert!(catalog.warnings[0].contains("capped"));
    }

    #[test]
    fn debug_never_prints_header_values() {
        let raw = json!({
            "tools": {
                "weather": {
                    "description": "Get weather",
                    "inputSchema": {"type": "object"},
                    "exec": {
                        "kind": "http",
                        "url": "https://api.example.com/weather",
                        "headers": {"X-Key": "super-secret-value"}
                    }
                }
            }
        })
        .to_string();
        let catalog = parse_catalog(&raw).expect("valid catalog");
        let debug = format!("{:?}", catalog.tools["weather"]);
        assert!(debug.contains("x-key"));
        assert!(!debug.contains("super-secret-value"));
    }

    #[test]
    fn command_control_characters_are_rejected() {
        for exec in [
            json!({"kind": "command", "command": "echo\u{0007}"}),
            json!({"kind": "command", "command": "echo", "args": ["bad\narg"]}),
            json!({"kind": "command", "command": "echo", "env": {"A": "bad\u{0000}"}}),
        ] {
            let raw = json!({
                "tools": {
                    "bad": {
                        "description": "x",
                        "inputSchema": {"type": "object"},
                        "exec": exec
                    }
                }
            })
            .to_string();
            let catalog = parse_catalog(&raw).expect("valid document");
            assert!(
                catalog.tools.is_empty(),
                "control characters must skip the entry"
            );
        }
    }

    #[test]
    fn names_are_bounded_and_restricted() {
        for name in ["", "has space", "slash/name", &"a".repeat(65)] {
            assert!(validate_name(name).is_err(), "{name:?} should be invalid");
        }
        for name in ["weather", "a", "A-Z_0", &"a".repeat(64)] {
            validate_name(name).expect("valid name");
        }
    }

    #[test]
    fn non_object_schema_is_skipped() {
        let raw = json!({
            "tools": {
                "bad": {
                    "description": "x",
                    "inputSchema": "nope",
                    "exec": {"kind": "command", "command": "echo"}
                }
            }
        })
        .to_string();
        let catalog = parse_catalog(&raw).expect("valid document");
        assert!(catalog.tools.is_empty());
        assert!(catalog.warnings[0].contains("inputSchema"));
    }

    #[test]
    fn parses_outside_the_working_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("custom-tools.json");
        fs::write(
            &path,
            json!({"tools": {"echo": command_tool()}}).to_string(),
        )
        .expect("write");
        let catalog = load_catalog(&path);
        assert_eq!(catalog.tools.len(), 1);
    }
}
