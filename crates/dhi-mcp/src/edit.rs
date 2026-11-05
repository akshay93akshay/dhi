//! DHI-standard `mcpServers` add / edit / delete for the running server.
//!
//! The CLI historically owned config writes. This module is the shared
//! persistence layer for the live MCP API: it mutates the standard
//! `mcpServers` map in place, preserves every other top-level key (and every
//! unknown field on a server that is being edited), validates the whole
//! resulting file before touching disk, and writes atomically with mode 0600.
//!
//! The legacy `servers` array is deliberately refused: rewriting it here would
//! silently migrate a file the operator did not ask to migrate. `dhi mcp
//! migrate` remains the explicit conversion path.

use std::collections::BTreeMap;
use std::fmt;
use std::io::Write;
use std::path::Path;

use serde_json::{Map, Value, json};

use crate::config::{McpFile, McpServerEntry, McpTransportData, guarded_tool_name};

/// The known fields of one `McpServerEntry`. Unknown fields are preserved on
/// update and ignored by the runtime loader, so an editor's metadata does not
/// disappear just because DHI rewrote the file.
const KNOWN_ENTRY_FIELDS: &[&str] = &[
    "transport",
    "command",
    "args",
    "env",
    "url",
    "headers",
    "timeout_secs",
    "insecure",
    "enabled",
    "visible_to",
    "tool_visibility",
    "deferred_threshold",
];

/// Why a config edit was refused. The HTTP layer maps these to 400/404/409.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditError {
    /// The name is already present in `mcpServers`.
    Duplicate(String),
    /// The name is not present in `mcpServers`.
    Unknown(String),
    /// The entry, name, or resulting file is invalid.
    Invalid(String),
}

impl fmt::Display for EditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Duplicate(name) => write!(formatter, "MCP server {name:?} already exists"),
            Self::Unknown(name) => write!(formatter, "no MCP server named {name:?}"),
            Self::Invalid(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for EditError {}

fn invalid(message: impl Into<String>) -> EditError {
    EditError::Invalid(message.into())
}

/// Validates one entry exactly as the running server will consume it.
///
/// Beyond the config crate's field validation, this rejects commands with
/// control characters, NUL in arguments, and names that cannot produce a
/// guarded tool segment. Header and environment values are never echoed in
/// an error. Legacy SSE is accepted because the runtime now has a client for
/// it.
pub fn validate_entry(name: &str, entry: &McpServerEntry) -> Result<(), EditError> {
    let name = name.trim();
    let validated = entry
        .validate(name)
        .map_err(|error| invalid(error.to_string()))?;
    if let McpTransportData::Stdio { command, args, .. } = &validated.transport_data {
        if command.chars().any(char::is_control) {
            return Err(invalid(format!(
                "MCP server {name:?} command must not contain control characters"
            )));
        }
        if args.iter().any(|arg| arg.contains('\0')) {
            return Err(invalid(format!(
                "MCP server {name:?} arguments must not contain NUL"
            )));
        }
    }
    if guarded_tool_name(name, "tool").is_none() {
        return Err(invalid(format!(
            "MCP server {name:?} cannot produce a guarded tool name; use letters, \
             digits, '-' or '_'"
        )));
    }
    Ok(())
}

/// Adds `entry` under `name`, refusing an exact duplicate.
pub fn add_server(path: &Path, name: &str, entry: &McpServerEntry) -> Result<(), EditError> {
    let name = checked_name(name)?;
    validate_entry(&name, entry)?;
    let mut document = load_document(path)?;
    {
        let servers = servers_mut(&mut document)?;
        if servers.contains_key(&name) {
            return Err(EditError::Duplicate(name));
        }
        servers.insert(name, entry_value(entry)?);
    }
    validate_document(&document)?;
    write_document(path, &document)
}

/// Replaces the existing entry under `name`. Unknown fields on the old entry
/// are preserved; known fields are fully replaced.
pub fn update_server(path: &Path, name: &str, entry: &McpServerEntry) -> Result<(), EditError> {
    let name = checked_name(name)?;
    validate_entry(&name, entry)?;
    let mut document = load_document(path)?;
    {
        let servers = servers_mut(&mut document)?;
        let Some(previous) = servers.get(&name).cloned() else {
            return Err(EditError::Unknown(name));
        };
        let mut replacement = entry_value(entry)?;
        if let (Some(previous), Some(replacement)) =
            (previous.as_object(), replacement.as_object_mut())
        {
            for (key, value) in previous {
                if !KNOWN_ENTRY_FIELDS.contains(&key.as_str()) {
                    replacement.insert(key.clone(), value.clone());
                }
            }
        }
        servers.insert(name, replacement);
    }
    validate_document(&document)?;
    write_document(path, &document)
}

/// Removes `name`, leaving every other server and unknown key untouched.
pub fn delete_server(path: &Path, name: &str) -> Result<(), EditError> {
    let name = checked_name(name)?;
    let mut document = load_document(path)?;
    {
        let servers = servers_mut(&mut document)?;
        if servers.remove(&name).is_none() {
            return Err(EditError::Unknown(name));
        }
    }
    write_document(path, &document)
}

/// Loads a config while tolerating unknown top-level and per-entry fields.
///
/// Unknown fields are not used by the runtime, but they are preserved when a
/// file is edited. The legacy `servers` array is still parsed strictly by the
/// config crate (it is a migration input, not an editable shape).
pub fn load_lenient(path: &Path) -> anyhow::Result<Option<McpFile>> {
    let Some(value) = read_json(path)? else {
        return Ok(None);
    };
    let object = value.as_object().ok_or_else(|| {
        anyhow::anyhow!(
            "{} must contain a JSON object at the top level",
            path.display()
        )
    })?;
    let standard = object.contains_key("mcpServers");
    let legacy = object.contains_key("servers");
    if legacy && !standard {
        return McpFile::load(path);
    }
    if legacy && standard {
        anyhow::bail!(
            "{} contains both \"mcpServers\" and \"servers\"; keep exactly one shape",
            path.display()
        );
    }
    let Some(servers) = object.get("mcpServers") else {
        anyhow::bail!(
            "{} is not a recognized MCP config: expected \"mcpServers\" or the \
             legacy \"servers\" array",
            path.display()
        );
    };
    let servers = servers.as_object().ok_or_else(|| {
        anyhow::anyhow!("{}: \"mcpServers\" must be a JSON object", path.display())
    })?;
    let mut file = McpFile::default();
    for (name, value) in servers {
        file.servers
            .insert(name.clone(), entry_from_value(name, value)?);
    }
    Ok(Some(file))
}

fn checked_name(name: &str) -> Result<String, EditError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(invalid("MCP server name must not be empty"));
    }
    Ok(name.to_string())
}

fn entry_value(entry: &McpServerEntry) -> Result<Value, EditError> {
    serde_json::to_value(entry)
        .map_err(|error| invalid(format!("could not serialize MCP server entry: {error}")))
}

/// Parses one entry, dropping unknown fields for validation while leaving the
/// on-disk document untouched (the caller writes the original value).
fn entry_from_value(name: &str, value: &Value) -> anyhow::Result<McpServerEntry> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("MCP server {name:?} must be a JSON object"))?;
    let filtered: Map<String, Value> = object
        .iter()
        .filter(|(key, _)| KNOWN_ENTRY_FIELDS.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    serde_json::from_value(Value::Object(filtered))
        .map_err(|error| anyhow::anyhow!("MCP server {name:?} is invalid: {error}"))
}

/// Validates every server in the raw document through the same typed rules
/// the manager uses, including sanitized-name collisions between entries.
fn validate_document(document: &Value) -> Result<(), EditError> {
    let servers = document
        .get("mcpServers")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("\"mcpServers\" must be a JSON object"))?;
    let mut file = McpFile::default();
    for (name, value) in servers {
        let entry = entry_from_value(name, value).map_err(|error| invalid(error.to_string()))?;
        file.servers.insert(name.clone(), entry);
    }
    file.validate()
        .map_err(|error| invalid(error.to_string()))?;
    Ok(())
}

fn load_document(path: &Path) -> Result<Value, EditError> {
    let Some(value) = read_json(path).map_err(|error| invalid(error.to_string()))? else {
        return Ok(json!({ "mcpServers": {} }));
    };
    let object = value
        .as_object()
        .ok_or_else(|| invalid(format!("{} must contain a JSON object", path.display())))?;
    let standard = object.contains_key("mcpServers");
    let legacy = object.contains_key("servers");
    if legacy && !standard {
        return Err(invalid(format!(
            "{} uses the legacy \"servers\" shape; run `dhi mcp migrate` first",
            path.display()
        )));
    }
    if legacy && standard {
        return Err(invalid(format!(
            "{} contains both \"mcpServers\" and \"servers\"; keep exactly one shape",
            path.display()
        )));
    }
    let mut document = value;
    let object = document
        .as_object_mut()
        .expect("the top-level object was checked above");
    if !standard {
        object.insert("mcpServers".to_string(), json!({}));
    }
    if !object.get("mcpServers").is_some_and(Value::is_object) {
        return Err(invalid(format!(
            "{}: \"mcpServers\" must be a JSON object",
            path.display()
        )));
    }
    Ok(document)
}

fn servers_mut(document: &mut Value) -> Result<&mut Map<String, Value>, EditError> {
    document
        .get_mut("mcpServers")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| invalid("\"mcpServers\" must be a JSON object"))
}

fn read_json(path: &Path) -> anyhow::Result<Option<Value>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(anyhow::anyhow!(
                "could not read {}: {error}",
                path.display()
            ));
        }
    };
    let value = serde_json::from_str(&raw)
        .map_err(|error| anyhow::anyhow!("{} is not valid JSON: {error}", path.display()))?;
    Ok(Some(value))
}

fn write_document(path: &Path, document: &Value) -> Result<(), EditError> {
    let mut bytes = serde_json::to_vec_pretty(document)
        .map_err(|error| invalid(format!("could not serialize MCP config: {error}")))?;
    bytes.push(b'\n');
    atomic_write(path, &bytes).map_err(|error| invalid(error.to_string()))
}

/// Atomic private write. The temporary file is created next to the target, is
/// mode 0600 on Unix, is fsynced, and replaces the target with one rename.
fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|error| anyhow::anyhow!("could not create {}: {error}", parent.display()))?;

    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| anyhow::anyhow!("could not create a temporary file: {error}"))?;
    temp.write_all(bytes)
        .map_err(|error| anyhow::anyhow!("could not write the temporary file: {error}"))?;
    temp.flush()
        .map_err(|error| anyhow::anyhow!("could not flush the temporary file: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|error| anyhow::anyhow!("could not make the config private: {error}"))?;
    }
    temp.as_file()
        .sync_all()
        .map_err(|error| anyhow::anyhow!("could not sync the temporary file: {error}"))?;
    temp.persist(path).map_err(|error| {
        anyhow::anyhow!("could not replace {}: {}", path.display(), error.error)
    })?;
    Ok(())
}

/// Header names whose values must never be returned by the API.
pub fn is_secret_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "authorization",
        "proxy-authorization",
        "cookie",
        "set-cookie",
        "token",
        "secret",
        "api-key",
        "apikey",
        "access-key",
        "credential",
        "password",
    ]
    .iter()
    .any(|needle| name.contains(needle))
}

/// Masks secret header values, preserving the header names and every
/// non-secret header unchanged.
pub fn mask_headers(headers: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    headers
        .iter()
        .map(|(name, value)| {
            if is_secret_header(name) {
                (name.clone(), "***".to_string())
            } else {
                (name.clone(), value.clone())
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpTransport;
    use std::path::PathBuf;

    fn temp_config(contents: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("mcp.json");
        std::fs::write(&path, contents).expect("write config");
        (dir, path)
    }

    fn stdio(command: &str) -> McpServerEntry {
        McpServerEntry {
            command: Some(command.to_string()),
            ..McpServerEntry::default()
        }
    }

    fn http(url: &str) -> McpServerEntry {
        McpServerEntry {
            url: Some(url.to_string()),
            ..McpServerEntry::default()
        }
    }

    #[cfg(unix)]
    fn file_mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    }

    #[test]
    fn add_preserves_unknown_keys_and_other_servers() {
        let (_dir, path) = temp_config(
            r#"{
              "$schema": "dhi.mcp.v1",
              "mcpServers": {
                "keep": { "command": "npx", "x-note": "keep-me" },
                "other": { "url": "https://other.test/mcp" }
              }
            }"#,
        );

        add_server(
            &path,
            "new",
            &McpServerEntry {
                url: Some("https://new.test/mcp".to_string()),
                headers: BTreeMap::from([(
                    "Authorization".to_string(),
                    "Bearer secret".to_string(),
                )]),
                ..McpServerEntry::default()
            },
        )
        .expect("add");

        let raw = std::fs::read_to_string(&path).expect("read");
        assert!(raw.contains("\"$schema\""), "{raw}");
        assert!(raw.contains("\"x-note\""), "{raw}");
        assert!(raw.contains("\"keep-me\""), "{raw}");
        assert!(raw.contains("\"other\""), "{raw}");
        assert!(raw.contains("\"new\""), "{raw}");
        assert!(raw.contains("\"Authorization\""), "{raw}");

        let file = load_lenient(&path).expect("loads").expect("config exists");
        assert_eq!(file.servers.len(), 3);
        assert_eq!(file.servers["keep"].command.as_deref(), Some("npx"));

        #[cfg(unix)]
        assert_eq!(file_mode(&path), 0o600);
    }

    #[test]
    fn duplicate_unknown_and_invalid_are_distinct_errors() {
        let (_dir, path) = temp_config(r#"{"mcpServers":{"a":{"command":"npx"}}}"#);

        assert_eq!(
            add_server(&path, "a", &stdio("node")),
            Err(EditError::Duplicate("a".to_string()))
        );
        assert_eq!(
            delete_server(&path, "missing"),
            Err(EditError::Unknown("missing".to_string()))
        );
        assert_eq!(
            update_server(&path, "missing", &stdio("npx")),
            Err(EditError::Unknown("missing".to_string()))
        );
        assert!(matches!(
            add_server(&path, "@@@", &stdio("npx")),
            Err(EditError::Invalid(_))
        ));
        assert!(matches!(
            add_server(&path, "bad", &http("ftp://example.test")),
            Err(EditError::Invalid(_))
        ));
        assert!(matches!(
            add_server(&path, "bad", &stdio("npx\0evil")),
            Err(EditError::Invalid(_))
        ));
    }

    #[test]
    fn adds_legacy_sse_entries() {
        let (_dir, path) = temp_config(r#"{"mcpServers":{}}"#);
        add_server(
            &path,
            "sse",
            &McpServerEntry {
                transport: Some(McpTransport::Sse),
                url: Some("https://example.test/sse".to_string()),
                headers: BTreeMap::from([(
                    "Authorization".to_string(),
                    "Bearer test-token".to_string(),
                )]),
                ..McpServerEntry::default()
            },
        )
        .expect("legacy SSE entry is editable");

        let file = load_lenient(&path).expect("loads").expect("config exists");
        assert_eq!(file.servers["sse"].transport, Some(McpTransport::Sse));
        assert_eq!(
            file.servers["sse"].headers.get("Authorization"),
            Some(&"Bearer test-token".to_string())
        );
    }

    #[test]
    fn update_preserves_unknown_entry_fields_and_known_fields_replace() {
        let (_dir, path) =
            temp_config(r#"{"mcpServers":{"a":{"command":"npx","args":["old"],"x-note":"keep"}}}"#);
        update_server(
            &path,
            "a",
            &McpServerEntry {
                command: Some("node".to_string()),
                args: vec!["new".to_string()],
                ..McpServerEntry::default()
            },
        )
        .expect("update");
        let raw = std::fs::read_to_string(&path).expect("read");
        assert!(raw.contains("\"x-note\""), "{raw}");
        assert!(raw.contains("\"node\""), "{raw}");
        assert!(!raw.contains("\"old\""), "{raw}");
        let file = load_lenient(&path).expect("loads").expect("exists");
        assert_eq!(file.servers["a"].command.as_deref(), Some("node"));
        assert_eq!(file.servers["a"].args, ["new"]);
    }

    #[test]
    fn delete_removes_only_the_named_server() {
        let (_dir, path) = temp_config(
            r#"{"$schema":"x","mcpServers":{"a":{"command":"npx"},"b":{"command":"node"}}}"#,
        );
        delete_server(&path, "a").expect("delete");
        let raw = std::fs::read_to_string(&path).expect("read");
        assert!(!raw.contains("\"a\""), "{raw}");
        assert!(raw.contains("\"b\""), "{raw}");
        assert!(raw.contains("\"$schema\""), "{raw}");
    }

    #[test]
    fn sanitized_name_collisions_are_refused() {
        let (_dir, path) = temp_config(r#"{"mcpServers":{"my_server":{"command":"npx"}}}"#);
        let error = add_server(&path, "my server", &stdio("node")).expect_err("collision");
        assert!(error.to_string().contains("both sanitize"), "{error}");
    }

    #[test]
    fn legacy_shape_is_refused_for_editing() {
        let (_dir, path) = temp_config(r#"{"servers":[{"name":"a","command":"npx"}]}"#);
        let error = add_server(&path, "b", &stdio("node")).expect_err("legacy");
        assert!(error.to_string().contains("migrate"), "{error}");
    }

    #[test]
    fn missing_file_is_created_as_standard_shape() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("nested").join("mcp.json");
        add_server(&path, "a", &stdio("npx")).expect("add");
        let file = McpFile::load(&path).expect("loads").expect("exists");
        assert_eq!(file.servers.len(), 1);
        assert_eq!(file.servers["a"].command.as_deref(), Some("npx"));
        #[cfg(unix)]
        assert_eq!(file_mode(&path), 0o600);
    }

    #[test]
    fn masks_secret_headers_without_echoing_values() {
        let headers = BTreeMap::from([
            ("Authorization".to_string(), "Bearer token".to_string()),
            ("X-Api-Key".to_string(), "key".to_string()),
            ("X-Tenant".to_string(), "acme".to_string()),
        ]);
        let masked = mask_headers(&headers);
        assert_eq!(masked["Authorization"], "***");
        assert_eq!(masked["X-Api-Key"], "***");
        assert_eq!(masked["X-Tenant"], "acme");
        assert!(!masked.values().any(|value| value.contains("token")));
        assert!(is_secret_header("proxy-authorization"));
        assert!(is_secret_header("X-Access-Token"));
        assert!(!is_secret_header("Content-Type"));
    }
}
