//! Standard MCP server configuration for DHI.
//!
//! The primary shape is the one used by the wider MCP ecosystem:
//!
//! ```json
//! {
//!   "mcpServers": {
//!     "chess": {
//!       "command": "uvx",
//!       "args": ["chess-mcp-server"]
//!     },
//!     "remote": {
//!       "url": "https://example.com/mcp",
//!       "headers": { "Authorization": "Bearer ..." },
//!       "timeout_secs": 60
//!     }
//!   }
//! }
//! ```
//!
//! [`McpFile::load`] also accepts the older DHI shape
//! (`{"servers": [{"name": ..., "transport": ...}]}`) so a config can be
//! migrated without hand editing. [`McpFile::migrate_from_legacy`] returns the
//! standard shape; it never writes, leaving that choice to the caller.
//!
//! Validation is deliberately field-level and side-effect free: it reports
//! which field is wrong, never the secret values in headers or environment
//! variables.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The config file name, looked up under `ops/` or via `DHI_MCP_CONFIG`.
pub const CONFIG_FILENAME: &str = "mcp.json";

/// Per-call deadline used when an entry does not set `timeout_secs`.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// At or above this many tools (per server), the server is exposed through
/// the `mcp_list_tools` / `mcp_call_tool` meta-tools instead of one schema per
/// tool. A server below the threshold keeps the direct registration it had
/// before deferred schemas existed. A per-entry `deferred_threshold`
/// overrides this default.
pub const DEFAULT_DEFERRED_THRESHOLD: usize = 32;

/// The guard's prefix for every MCP tool.
pub const TOOL_PREFIX: &str = "mcp__";
/// The separator between the server and tool segments.
pub const SEGMENT_SEPARATOR: &str = "__";
/// Model-visible tool names are capped at 64 characters (OpenAI's limit).
pub const MAX_TOOL_NAME_CHARS: usize = 64;
/// The server segment is capped to leave room for the tool segment in the
/// guarded name `mcp__<server>__<tool>`.
pub const MAX_SERVER_SEGMENT_CHARS: usize = 16;

/// Which transport one server speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    /// A child process speaking MCP over stdio.
    Stdio,
    /// Streamable HTTP.
    Http,
    /// Server-sent events.
    Sse,
}

impl McpTransport {
    /// The lowercase name used in JSON and CLI output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Http => "http",
            Self::Sse => "sse",
        }
    }
}

impl std::fmt::Display for McpTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One server entry in the standard `mcpServers` shape.
///
/// `command` selects stdio, `url` selects HTTP, and an explicit `transport`
/// may select any of `stdio`, `http`, or `sse`. Empty/default fields are
/// omitted when serializing, so a minimal stdio entry is just
/// `{"command": "...", "args": [...]}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerEntry {
    /// Optional explicit transport. When absent, `command` implies stdio and
    /// `url` implies HTTP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<McpTransport>,
    /// The executable for a stdio server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Arguments for a stdio server.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Extra environment for a stdio server.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// The endpoint for an HTTP or SSE server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Request headers for an HTTP or SSE server.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Per-call deadline; [`DEFAULT_CALL_TIMEOUT`] when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// HTTP/SSE only: allow plain HTTP and skip TLS verification.
    #[serde(default, skip_serializing_if = "is_false")]
    pub insecure: bool,
    /// `false` keeps the entry in the file but out of the active set.
    #[serde(default = "default_enabled", skip_serializing_if = "is_true")]
    pub enabled: bool,
    /// Roles this server is visible to. `None` inherits the fleet default;
    /// `Some(vec![])` is visible to nobody and is preserved on serialization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_to: Option<Vec<String>>,
    /// Per-tool role overrides keyed by the tool's MCP name. An absent tool
    /// inherits [`Self::visible_to`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tool_visibility: BTreeMap<String, Vec<String>>,
    /// Number of visible tools above which schemas are deferred.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred_threshold: Option<usize>,
}

impl Default for McpServerEntry {
    fn default() -> Self {
        Self {
            transport: None,
            command: None,
            args: Vec::new(),
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
            timeout_secs: None,
            insecure: false,
            enabled: true,
            visible_to: None,
            tool_visibility: BTreeMap::new(),
            deferred_threshold: None,
        }
    }
}

fn default_enabled() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// The standard MCP config root: a name-to-entry map.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpFile {
    /// The `mcpServers` map. `BTreeMap` gives stable JSON ordering.
    #[serde(rename = "mcpServers", default)]
    pub servers: BTreeMap<String, McpServerEntry>,
}

/// Which on-disk shape a config file uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigShape {
    /// The standard `mcpServers` map.
    Standard,
    /// The older DHI `servers` array.
    Legacy,
}

/// One validated, connectable server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedServer {
    /// The name as configured (trimmed).
    pub name: String,
    /// The sanitized, length-capped segment used in guarded tool names.
    pub segment: String,
    /// The transport kind, explicit or inferred.
    pub transport: McpTransport,
    /// The per-call deadline.
    pub call_timeout: Duration,
    /// The normalized transport-specific data.
    pub transport_data: McpTransportData,
    /// Roles this server is visible to, copied from the entry.
    pub visible_to: Option<Vec<String>>,
    /// Per-tool role overrides, copied from the entry.
    pub tool_visibility: BTreeMap<String, Vec<String>>,
    /// Deferred-schema threshold, copied from the entry.
    pub deferred_threshold: Option<usize>,
}

/// The transport-specific part of a validated server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpTransportData {
    /// A stdio child process.
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    },
    /// A streamable HTTP endpoint.
    Http {
        url: String,
        headers: BTreeMap<String, String>,
        insecure: bool,
    },
    /// A server-sent-events endpoint.
    Sse {
        url: String,
        headers: BTreeMap<String, String>,
        insecure: bool,
    },
}

impl ValidatedServer {
    /// A short human-readable target for CLI output: the URL, or the stdio
    /// command with its arguments.
    pub fn target(&self) -> String {
        match &self.transport_data {
            McpTransportData::Stdio { command, args, .. } => {
                let mut target = command.clone();
                for arg in args {
                    target.push(' ');
                    target.push_str(arg);
                }
                target
            }
            McpTransportData::Http { url, .. } | McpTransportData::Sse { url, .. } => url.clone(),
        }
    }
}

impl McpServerEntry {
    /// Validates one entry under `name`, normalizing transport, command/URL,
    /// timeout, and transport-specific fields.
    ///
    /// Every field error names the server and the field, never a header or
    /// environment value.
    pub fn validate(&self, name: &str) -> anyhow::Result<ValidatedServer> {
        let name = name.trim();
        if name.is_empty() {
            bail!("MCP server name must not be empty");
        }
        let sanitized = sanitize_segment(name);
        if sanitized.is_empty() {
            bail!(
                "MCP server name {name:?} has no usable characters after sanitization; \
                 use letters, digits, '-' or '_'"
            );
        }
        let segment = sanitized.chars().take(MAX_SERVER_SEGMENT_CHARS).collect();

        let call_timeout = match self.timeout_secs {
            Some(0) => {
                bail!("MCP server {name:?} has timeout_secs = 0; use a positive number of seconds")
            }
            Some(secs) => Duration::from_secs(secs),
            None => DEFAULT_CALL_TIMEOUT,
        };

        let has_command = self.command.is_some();
        let has_url = self.url.is_some();
        if has_command && has_url {
            bail!("MCP server {name:?} sets both command and url; use exactly one");
        }
        if !has_command && !has_url {
            bail!(
                "MCP server {name:?} sets neither command nor url; a stdio server \
                 needs command and an HTTP/SSE server needs url"
            );
        }

        let transport = match self.transport {
            Some(transport) => transport,
            None if has_command => McpTransport::Stdio,
            None => McpTransport::Http,
        };

        let transport_data = match transport {
            McpTransport::Stdio => {
                if has_url {
                    bail!(
                        "MCP server {name:?} uses transport \"stdio\" but sets url; \
                         stdio requires command"
                    );
                }
                if !self.headers.is_empty() {
                    bail!(
                        "MCP server {name:?} sets headers for a stdio transport; \
                         headers are only valid for http or sse"
                    );
                }
                if self.insecure {
                    bail!(
                        "MCP server {name:?} sets insecure for a stdio transport; \
                         insecure is only valid for http or sse"
                    );
                }
                let command = self.command.as_deref().unwrap_or_default().trim();
                if command.is_empty() {
                    bail!("MCP server {name:?} has an empty command");
                }
                validate_env(name, &self.env)?;
                McpTransportData::Stdio {
                    command: command.to_string(),
                    args: self.args.clone(),
                    env: self.env.clone(),
                }
            }
            McpTransport::Http | McpTransport::Sse => {
                if has_command {
                    bail!(
                        "MCP server {name:?} uses transport \"{transport}\" but sets \
                         command; use url instead"
                    );
                }
                if !self.args.is_empty() {
                    bail!(
                        "MCP server {name:?} sets args for an {transport} transport; \
                         args are only valid for stdio"
                    );
                }
                if !self.env.is_empty() {
                    bail!(
                        "MCP server {name:?} sets env for an {transport} transport; \
                         env is only valid for stdio"
                    );
                }
                let url = self.url.as_deref().unwrap_or_default().trim();
                if url.is_empty() {
                    bail!("MCP server {name:?} has an empty url");
                }
                validate_url(url).map_err(|err| anyhow!("MCP server {name:?} url: {err}"))?;
                let headers = validate_headers(name, &self.headers)?;
                if transport == McpTransport::Sse {
                    McpTransportData::Sse {
                        url: url.to_string(),
                        headers,
                        insecure: self.insecure,
                    }
                } else {
                    McpTransportData::Http {
                        url: url.to_string(),
                        headers,
                        insecure: self.insecure,
                    }
                }
            }
        };

        Ok(ValidatedServer {
            name: name.to_string(),
            segment,
            transport,
            call_timeout,
            transport_data,
            visible_to: self.visible_to.clone(),
            tool_visibility: self.tool_visibility.clone(),
            deferred_threshold: self.deferred_threshold,
        })
    }
}

impl McpFile {
    /// Reads a config file. A missing file is `Ok(None)`; an unreadable,
    /// malformed, or unrecognized file is an error. Both the standard
    /// `mcpServers` shape and the legacy `servers` array are accepted.
    pub fn load(path: &Path) -> anyhow::Result<Option<Self>> {
        let Some(value) = read_json(path)? else {
            return Ok(None);
        };
        match shape_of(&value, path)? {
            ConfigShape::Standard => {
                let file = serde_json::from_value(value).with_context(|| {
                    format!("{} is not a valid standard MCP config", path.display())
                })?;
                Ok(Some(file))
            }
            ConfigShape::Legacy => {
                let legacy: LegacyFile = serde_json::from_value(value).with_context(|| {
                    format!("{} is not a valid legacy MCP config", path.display())
                })?;
                Ok(Some(legacy.into_file()?))
            }
        }
    }

    /// Reports which shape a file uses without converting it. A missing file
    /// is `Ok(None)`; a file with neither shape, or both shapes, is an error.
    pub fn shape(path: &Path) -> anyhow::Result<Option<ConfigShape>> {
        let Some(value) = read_json(path)? else {
            return Ok(None);
        };
        Ok(Some(shape_of(&value, path)?))
    }

    /// Writes the standard shape atomically, with `BTreeMap` ordering and
    /// mode 0600 on Unix. Missing parent directories are created.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let mut bytes = serde_json::to_string_pretty(self)
            .map_err(|err| anyhow!("could not serialize MCP config: {err}"))?
            .into_bytes();
        bytes.push(b'\n');
        atomic_write(path, &bytes)
    }

    /// Converts the legacy `servers` array to the standard `mcpServers` map.
    /// A file that is already standard is returned unchanged, making this
    /// idempotent. A missing or unrecognized file is an error.
    pub fn migrate_from_legacy(path: &Path) -> anyhow::Result<Self> {
        let Some(value) = read_json(path)? else {
            bail!("no MCP config at {}", path.display());
        };
        match shape_of(&value, path)? {
            ConfigShape::Legacy => {
                let legacy: LegacyFile = serde_json::from_value(value).with_context(|| {
                    format!("{} is not a valid legacy MCP config", path.display())
                })?;
                legacy.into_file()
            }
            ConfigShape::Standard => {
                let file = serde_json::from_value(value).with_context(|| {
                    format!("{} is not a valid standard MCP config", path.display())
                })?;
                Ok(file)
            }
        }
    }

    /// Validates every entry, including disabled ones, and returns the
    /// enabled set in name order. Two enabled servers whose names sanitize to
    /// the same tool segment are rejected.
    pub fn validate(&self) -> anyhow::Result<Vec<ValidatedServer>> {
        let mut validated = Vec::new();
        let mut seen_segments: BTreeMap<String, String> = BTreeMap::new();
        for (name, entry) in &self.servers {
            let server = entry.validate(name)?;
            if !entry.enabled {
                continue;
            }
            if let Some(previous) = seen_segments.get(&server.segment) {
                bail!(
                    "MCP servers {previous:?} and {name:?} both sanitize to the tool \
                     segment {:?}",
                    server.segment
                );
            }
            seen_segments.insert(server.segment.clone(), name.clone());
            validated.push(server);
        }
        Ok(validated)
    }

    /// Copies `path` to `backup` atomically, with mode 0600 on Unix. Used by
    /// `dhi mcp migrate` before replacing a legacy file.
    pub fn backup_to(path: &Path, backup: &Path) -> anyhow::Result<()> {
        let raw = std::fs::read(path)
            .with_context(|| format!("could not read {} for backup", path.display()))?;
        atomic_write(backup, &raw)
    }
}

/// Maps a name to a tool-name segment: every character outside
/// `[A-Za-z0-9-]` becomes `_`, underscore runs collapse to one, and leading
/// or trailing underscores/hyphens are trimmed. An input with no usable
/// characters sanitizes to an empty string (callers treat that as invalid).
pub fn sanitize_segment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_underscore = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' {
            out.push(ch);
            last_underscore = false;
        } else if !last_underscore {
            out.push('_');
            last_underscore = true;
        }
    }
    out.trim_matches(['_', '-']).to_string()
}

/// Builds the one canonical guarded name for an MCP tool:
/// `mcp__<server>__<tool>`.
///
/// This is the function every transport, registration, deferred dispatch, and
/// audit path must use. It mirrors the running server's naming rule exactly:
/// the server is sanitized, capped at [`MAX_SERVER_SEGMENT_CHARS`], and
/// sanitized again (so a cap that lands on `-` or `_` does not leave a
/// trailing separator character); the tool is sanitized and truncated so the
/// whole name stays within [`MAX_TOOL_NAME_CHARS`].
///
/// Returns `None` when either side has no usable characters.
pub fn guarded_tool_name(server: &str, tool: &str) -> Option<String> {
    let server_segment: String = sanitize_segment(server)
        .chars()
        .take(MAX_SERVER_SEGMENT_CHARS)
        .collect();
    // The second pass is deliberate: truncating a 17+ character segment can
    // expose a trailing '-' or '_', and the server's guarded_name trims it.
    let server_segment = sanitize_segment(&server_segment);
    if server_segment.is_empty() {
        return None;
    }
    let tool_segment = sanitize_segment(tool);
    if tool_segment.is_empty() {
        return None;
    }
    let base = format!("{TOOL_PREFIX}{server_segment}{SEGMENT_SEPARATOR}");
    let remaining = MAX_TOOL_NAME_CHARS.saturating_sub(base.chars().count());
    if remaining == 0 {
        return None;
    }
    let tool_segment: String = tool_segment.chars().take(remaining).collect();
    Some(format!("{base}{tool_segment}"))
}

/// Where the MCP config lives: `DHI_MCP_CONFIG` wins verbatim, else the
/// nearest `ops/mcp.json` above this crate's directory.
pub fn config_path() -> Option<PathBuf> {
    if let Ok(raw) = std::env::var("DHI_MCP_CONFIG") {
        let raw = raw.trim();
        if !raw.is_empty() {
            return Some(PathBuf::from(raw));
        }
    }
    find_upwards(Path::new(env!("CARGO_MANIFEST_DIR")), CONFIG_FILENAME)
}

/// Walks `start` and its ancestors for `ops/<filename>`.
fn find_upwards(start: &Path, filename: &str) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        let candidate = dir.join("ops").join(filename);
        if candidate.is_file() {
            return Some(candidate);
        }
        current = dir.parent();
    }
    None
}

// ---------------------------------------------------------------------------
// Legacy shape
// ---------------------------------------------------------------------------

/// The older DHI root: `{"servers": [...]}`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyFile {
    #[serde(default)]
    servers: Vec<LegacyServer>,
}

/// One entry of the older DHI shape. `url` was not part of the shipped
/// examples but the current `dhi-server` parser understands it, so migration
/// preserves it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyServer {
    name: String,
    #[serde(default)]
    transport: Option<McpTransport>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    insecure: bool,
    #[serde(default = "default_enabled")]
    enabled: bool,
}

impl LegacyFile {
    fn into_file(self) -> anyhow::Result<McpFile> {
        let mut servers = BTreeMap::new();
        for server in self.servers {
            let name = server.name.trim().to_string();
            if servers.contains_key(&name) {
                bail!("legacy MCP config defines server {name:?} more than once");
            }
            let transport = normalized_transport(
                server.transport,
                server.command.as_deref(),
                server.url.as_deref(),
            );
            let entry = McpServerEntry {
                transport,
                command: server.command,
                args: server.args,
                env: server.env,
                url: server.url,
                headers: server.headers,
                timeout_secs: server.timeout_secs,
                insecure: server.insecure,
                enabled: server.enabled,
                visible_to: None,
                tool_visibility: BTreeMap::new(),
                deferred_threshold: None,
            };
            servers.insert(name, entry);
        }
        Ok(McpFile { servers })
    }
}

/// Drops an explicit transport when inference already produces the same
/// kind, so migrated stdio/HTTP entries stay minimal. An explicit `sse`
/// remains because a URL alone infers `http`.
fn normalized_transport(
    explicit: Option<McpTransport>,
    command: Option<&str>,
    url: Option<&str>,
) -> Option<McpTransport> {
    let inferred = match (command, url) {
        (Some(_), None) => Some(McpTransport::Stdio),
        (None, Some(_)) => Some(McpTransport::Http),
        _ => None,
    };
    match (explicit, inferred) {
        (Some(explicit), Some(inferred)) if explicit == inferred => None,
        (explicit, _) => explicit,
    }
}

// ---------------------------------------------------------------------------
// Parsing + validation helpers
// ---------------------------------------------------------------------------

fn read_json(path: &Path) -> anyhow::Result<Option<Value>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(anyhow!("could not read {}: {err}", path.display())),
    };
    let value = serde_json::from_str(&raw)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;
    Ok(Some(value))
}

fn shape_of(value: &Value, path: &Path) -> anyhow::Result<ConfigShape> {
    let object = value.as_object().ok_or_else(|| {
        anyhow!(
            "{} must contain a JSON object at the top level",
            path.display()
        )
    })?;
    let standard = object.contains_key("mcpServers");
    let legacy = object.contains_key("servers");
    match (standard, legacy) {
        (true, false) => Ok(ConfigShape::Standard),
        (false, true) => Ok(ConfigShape::Legacy),
        (true, true) => bail!(
            "{} contains both \"mcpServers\" and \"servers\"; keep exactly one shape",
            path.display()
        ),
        (false, false) => bail!(
            "{} is not a recognized MCP config: expected \"mcpServers\" or the \
             legacy \"servers\" array",
            path.display()
        ),
    }
}

fn validate_env(name: &str, env: &BTreeMap<String, String>) -> anyhow::Result<()> {
    for (key, value) in env {
        if key.is_empty() {
            bail!("MCP server {name:?} has an environment variable with an empty name");
        }
        if key.contains('=') {
            bail!("MCP server {name:?} environment variable {key:?} must not contain '='");
        }
        if key.contains('\0') || value.contains('\0') {
            bail!("MCP server {name:?} environment variable {key:?} must not contain NUL");
        }
    }
    Ok(())
}

fn validate_headers(
    name: &str,
    headers: &BTreeMap<String, String>,
) -> anyhow::Result<BTreeMap<String, String>> {
    for (header_name, value) in headers {
        if header_name.trim().is_empty() {
            bail!("MCP server {name:?} has a header with an empty name");
        }
        if header_name.contains('\r') || header_name.contains('\n') {
            bail!("MCP server {name:?} header name {header_name:?} must not contain CR or LF");
        }
        if value.contains('\r') || value.contains('\n') {
            bail!("MCP server {name:?} header {header_name:?} value must not contain CR or LF");
        }
    }
    Ok(headers.clone())
}

/// Requires an absolute `http://` or `https://` URL with a non-empty host.
/// The parser is deliberately small: no dependency is needed for the checks
/// this config asks for, and no secret is ever echoed in an error.
fn validate_url(raw: &str) -> anyhow::Result<()> {
    if raw.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
        bail!("must not contain whitespace or control characters");
    }
    let (scheme, rest) = raw
        .split_once("://")
        .ok_or_else(|| anyhow!("must start with http:// or https://"))?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        bail!("must start with http:// or https://");
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() {
        bail!("must include a host");
    }
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    let host = if let Some(after_bracket) = host_port.strip_prefix('[') {
        let end = after_bracket
            .find(']')
            .ok_or_else(|| anyhow!("has an invalid IPv6 host"))?;
        &after_bracket[..end]
    } else {
        host_port.split(':').next().unwrap_or_default()
    };
    if host.is_empty() {
        bail!("must include a host");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Atomic private writes
// ---------------------------------------------------------------------------

fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("could not create {}", parent.display()))?;

    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("could not create a temporary file in {}", parent.display()))?;
    temp.write_all(bytes)
        .with_context(|| format!("could not write the temporary file for {}", path.display()))?;
    temp.flush()
        .with_context(|| format!("could not flush the temporary file for {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("could not make {} private", path.display()))?;
    }
    temp.as_file()
        .sync_all()
        .with_context(|| format!("could not sync the temporary file for {}", path.display()))?;
    temp.persist(path)
        .map_err(|err| anyhow!("could not replace {}: {}", path.display(), err.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_config(contents: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("mcp.json");
        std::fs::write(&path, contents).expect("write config");
        (dir, path)
    }

    fn parse(contents: &str) -> McpFile {
        let (_dir, path) = temp_config(contents);
        McpFile::load(&path)
            .expect("config reads")
            .expect("config exists")
    }

    fn validated(contents: &str) -> anyhow::Result<Vec<ValidatedServer>> {
        parse(contents).validate()
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
    fn parses_the_chess_readme_shape() {
        let file = parse(
            r#"{
              "mcpServers": {
                "chess": {
                  "command": "uvx",
                  "args": ["chess-mcp-server"]
                }
              }
            }"#,
        );
        assert_eq!(file.servers.len(), 1);
        let entry = &file.servers["chess"];
        assert_eq!(entry.command.as_deref(), Some("uvx"));
        assert_eq!(entry.args, ["chess-mcp-server"]);
        assert!(entry.enabled, "enabled defaults true");
        assert!(entry.transport.is_none(), "transport is inferred");

        let servers = file.validate().expect("chess entry validates");
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].transport, McpTransport::Stdio);
        assert_eq!(servers[0].call_timeout, DEFAULT_CALL_TIMEOUT);
        assert_eq!(servers[0].target(), "uvx chess-mcp-server");
        assert_eq!(
            servers[0].transport_data,
            McpTransportData::Stdio {
                command: "uvx".to_string(),
                args: vec!["chess-mcp-server".to_string()],
                env: BTreeMap::new(),
            }
        );
    }

    #[test]
    fn parses_remote_http_with_headers_and_insecure() {
        let file = parse(
            r#"{
              "mcpServers": {
                "remote": {
                  "url": "https://example.com/mcp",
                  "headers": { "Authorization": "Bearer token" },
                  "timeout_secs": 60,
                  "insecure": true
                }
              }
            }"#,
        );
        let servers = file.validate().expect("remote entry validates");
        assert_eq!(servers[0].transport, McpTransport::Http);
        assert_eq!(servers[0].call_timeout, Duration::from_secs(60));
        let McpTransportData::Http {
            url,
            headers,
            insecure,
        } = &servers[0].transport_data
        else {
            panic!("expected HTTP");
        };
        assert_eq!(url, "https://example.com/mcp");
        assert_eq!(headers["Authorization"], "Bearer token");
        assert!(insecure);
    }

    #[test]
    fn parses_explicit_sse() {
        let file = parse(
            r#"{
              "mcpServers": {
                "events": {
                  "transport": "sse",
                  "url": "https://example.com/sse",
                  "headers": { "X-Key": "value" }
                }
              }
            }"#,
        );
        let servers = file.validate().expect("SSE entry validates");
        assert_eq!(servers[0].transport, McpTransport::Sse);
        match &servers[0].transport_data {
            McpTransportData::Sse {
                url,
                headers,
                insecure,
            } => {
                assert_eq!(url, "https://example.com/sse");
                assert_eq!(headers["X-Key"], "value");
                assert!(!insecure);
            }
            other => panic!("expected SSE, got {other:?}"),
        }
    }

    #[test]
    fn absent_visibility_fields_round_trip_unchanged() {
        let file = parse(
            r#"{
              "mcpServers": {
                "chess": {
                  "command": "uvx",
                  "args": ["chess-mcp-server"]
                }
              }
            }"#,
        );
        let serialized = serde_json::to_value(&file).expect("serialize");
        let entry = &serialized["mcpServers"]["chess"];
        assert!(entry.get("visible_to").is_none(), "{serialized}");
        assert!(entry.get("tool_visibility").is_none(), "{serialized}");
        assert!(entry.get("deferred_threshold").is_none(), "{serialized}");

        let reparsed: McpFile = serde_json::from_value(serialized).expect("deserialize");
        assert_eq!(reparsed, file);
        assert_eq!(reparsed.servers["chess"].visible_to, None);
        assert!(reparsed.servers["chess"].tool_visibility.is_empty());
        assert_eq!(reparsed.servers["chess"].deferred_threshold, None);

        let servers = file.validate().expect("validates");
        assert_eq!(servers[0].visible_to, None);
        assert!(servers[0].tool_visibility.is_empty());
        assert_eq!(servers[0].deferred_threshold, None);
    }

    #[test]
    fn explicit_empty_visibility_is_preserved() {
        let file = parse(
            r#"{
              "mcpServers": {
                "locked": {
                  "url": "https://example.com/mcp",
                  "visible_to": []
                }
              }
            }"#,
        );
        assert_eq!(file.servers["locked"].visible_to, Some(Vec::new()));

        let raw = serde_json::to_string(&file).expect("serialize");
        assert!(raw.contains("\"visible_to\":[]"), "{raw}");
        let reloaded: McpFile = serde_json::from_str(&raw).expect("deserialize");
        assert_eq!(reloaded, file);

        let servers = file.validate().expect("validates");
        assert_eq!(servers[0].visible_to, Some(Vec::new()));
    }

    #[test]
    fn tool_visibility_and_deferred_threshold_round_trip() {
        let file = parse(
            r#"{
              "mcpServers": {
                "search": {
                  "url": "https://example.com/mcp",
                  "visible_to": ["worker", "researcher"],
                  "tool_visibility": {
                    "web.search": ["researcher"],
                    "fetch": []
                  },
                  "deferred_threshold": 24
                }
              }
            }"#,
        );
        let entry = &file.servers["search"];
        assert_eq!(
            entry.visible_to,
            Some(vec!["worker".to_string(), "researcher".to_string()])
        );
        assert_eq!(
            entry.tool_visibility["web.search"],
            vec!["researcher".to_string()]
        );
        assert!(entry.tool_visibility["fetch"].is_empty());
        assert_eq!(entry.deferred_threshold, Some(24));

        let servers = file.validate().expect("validates");
        assert_eq!(servers[0].visible_to, entry.visible_to);
        assert_eq!(servers[0].tool_visibility, entry.tool_visibility);
        assert_eq!(servers[0].deferred_threshold, Some(24));

        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("mcp.json");
        file.save(&path).expect("save");
        let loaded = McpFile::load(&path).expect("load").expect("config exists");
        assert_eq!(loaded, file);
    }

    #[test]
    fn infers_transport_from_command_or_url() {
        let stdio =
            validated(r#"{"mcpServers":{"s":{"command":"npx"}}}"#).expect("stdio validates");
        assert_eq!(stdio[0].transport, McpTransport::Stdio);

        let http = validated(r#"{"mcpServers":{"h":{"url":"https://example.com/mcp"}}}"#)
            .expect("http validates");
        assert_eq!(http[0].transport, McpTransport::Http);

        let explicit_http = validated(
            r#"{"mcpServers":{"h":{"transport":"http","url":"https://example.com/mcp"}}}"#,
        )
        .expect("explicit http validates");
        assert_eq!(explicit_http[0].transport, McpTransport::Http);
    }

    #[test]
    fn rejects_invalid_entries_with_named_fields() {
        for (body, expected) in [
            (
                r#"{"mcpServers":{"x":{"command":"npx","url":"https://x.test"}}}"#,
                "both command and url",
            ),
            (r#"{"mcpServers":{"x":{}}}"#, "neither command nor url"),
            (r#"{"mcpServers":{"x":{"url":"ftp://x.test"}}}"#, "http://"),
            (r#"{"mcpServers":{"x":{"url":"https:///mcp"}}}"#, "host"),
            (
                r#"{"mcpServers":{"x":{"url":"https://x.test","headers":{"X-Test":"a\r\nb"}}}}"#,
                "CR or LF",
            ),
            (
                r#"{"mcpServers":{"x":{"url":"https://x.test","headers":{"X-Test":"ok","Bad\nName":"v"}}}}"#,
                "CR or LF",
            ),
            (
                r#"{"mcpServers":{"x":{"command":"npx","timeout_secs":0}}}"#,
                "positive",
            ),
            (
                r#"{"mcpServers":{"":{"command":"npx"}}}"#,
                "must not be empty",
            ),
            (
                r#"{"mcpServers":{"@@@":{"command":"npx"}}}"#,
                "no usable characters",
            ),
            (
                r#"{"mcpServers":{"x":{"command":"npx","insecure":true}}}"#,
                "insecure",
            ),
            (
                r#"{"mcpServers":{"x":{"url":"https://x.test","env":{"A":"b"}}}}"#,
                "env",
            ),
            (
                r#"{"mcpServers":{"x":{"command":"npx","headers":{"A":"b"}}}}"#,
                "headers",
            ),
            (
                r#"{"mcpServers":{"x":{"url":"https://x.test","args":["a"]}}}"#,
                "args",
            ),
            (
                r#"{"mcpServers":{"x":{"transport":"http","command":"npx"}}}"#,
                "use url",
            ),
            (
                r#"{"mcpServers":{"x":{"transport":"http","url":"https://x.test","timeout_secs":0}}}"#,
                "positive",
            ),
        ] {
            let error = validated(body).expect_err("entry must be rejected");
            let message = error.to_string();
            assert!(
                message.contains(expected),
                "{body}: expected {expected:?} in {message:?}"
            );
        }
    }

    #[test]
    fn rejects_duplicate_sanitized_segments() {
        let error = validated(
            r#"{
              "mcpServers": {
                "my server": { "url": "https://a.test/mcp" },
                "my_server": { "url": "https://b.test/mcp" }
              }
            }"#,
        )
        .expect_err("duplicate segments are rejected");
        assert!(error.to_string().contains("both sanitize"), "{error}");
    }

    #[test]
    fn validate_returns_only_enabled_servers_but_checks_all() {
        let file = parse(
            r#"{
              "mcpServers": {
                "off": { "command": "npx", "enabled": false },
                "on": { "url": "https://x.test/mcp" }
              }
            }"#,
        );
        let servers = file.validate().expect("both entries are valid");
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "on");

        let file = parse(
            r#"{
              "mcpServers": {
                "off": { "command": "npx", "enabled": false, "timeout_secs": 0 }
              }
            }"#,
        );
        assert!(
            file.validate().is_err(),
            "disabled entries are still checked for field errors"
        );
    }

    #[test]
    fn save_load_round_trip_is_stable_and_private() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("nested").join("mcp.json");
        let mut file = McpFile::default();
        file.servers.insert(
            "remote".to_string(),
            McpServerEntry {
                url: Some("https://example.com/mcp".to_string()),
                headers: BTreeMap::from([("Authorization".to_string(), "Bearer t".to_string())]),
                ..McpServerEntry::default()
            },
        );
        file.servers.insert(
            "chess".to_string(),
            McpServerEntry {
                command: Some("uvx".to_string()),
                args: vec!["chess-mcp-server".to_string()],
                ..McpServerEntry::default()
            },
        );
        file.servers.insert(
            "off".to_string(),
            McpServerEntry {
                command: Some("npx".to_string()),
                enabled: false,
                ..McpServerEntry::default()
            },
        );

        file.save(&path).expect("save");
        let raw = std::fs::read_to_string(&path).expect("read saved config");
        assert!(raw.contains("\"mcpServers\""));
        assert!(raw.ends_with('\n'));
        assert!(!raw.contains("\"enabled\": true"), "defaults are omitted");
        assert!(raw.contains("\"enabled\": false"));
        let chess = raw.find("\"chess\"").expect("chess key");
        let off = raw.find("\"off\"").expect("off key");
        let remote = raw.find("\"remote\"").expect("remote key");
        assert!(chess < off && off < remote, "BTreeMap ordering is stable");

        let loaded = McpFile::load(&path).expect("load").expect("config exists");
        assert_eq!(loaded, file);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(file_mode(&path), 0o600, "saved config is private");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
                .expect("loosen saved mode");
            file.save(&path).expect("overwrite");
            assert_eq!(
                file_mode(&path),
                0o600,
                "an overwrite restores the private mode"
            );
        }
    }

    #[test]
    fn missing_file_is_none() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("missing.json");
        assert!(McpFile::load(&path).expect("load missing").is_none());
        assert!(McpFile::shape(&path).expect("shape missing").is_none());
        assert!(McpFile::migrate_from_legacy(&path).is_err());
    }

    #[test]
    fn legacy_migration_preserves_fields() {
        let (_dir, path) = temp_config(
            r#"{
              "servers": [
                {
                  "name": "example",
                  "transport": "stdio",
                  "command": "npx",
                  "args": ["-y", "server"],
                  "env": { "NODE_ENV": "production" },
                  "enabled": false,
                  "timeout_secs": 60
                },
                {
                  "name": "remote",
                  "transport": "http",
                  "url": "https://example.com/mcp",
                  "timeout_secs": 30
                },
                {
                  "name": "events",
                  "transport": "sse",
                  "url": "https://example.com/sse",
                  "headers": { "Authorization": "Bearer t" }
                }
              ]
            }"#,
        );

        assert_eq!(
            McpFile::shape(&path).expect("shape"),
            Some(ConfigShape::Legacy)
        );
        let file = McpFile::migrate_from_legacy(&path).expect("migrate");
        assert_eq!(file.servers.len(), 3);

        let example = &file.servers["example"];
        assert_eq!(example.command.as_deref(), Some("npx"));
        assert_eq!(example.args, ["-y", "server"]);
        assert_eq!(example.env["NODE_ENV"], "production");
        assert!(!example.enabled);
        assert_eq!(example.timeout_secs, Some(60));
        assert!(example.transport.is_none(), "redundant stdio is normalized");

        let remote = &file.servers["remote"];
        assert_eq!(remote.url.as_deref(), Some("https://example.com/mcp"));
        assert_eq!(remote.timeout_secs, Some(30));
        assert!(remote.transport.is_none(), "redundant http is normalized");

        let events = &file.servers["events"];
        assert_eq!(events.transport, Some(McpTransport::Sse));
        assert_eq!(events.headers["Authorization"], "Bearer t");

        file.save(&path).expect("write standard shape");
        let raw = std::fs::read_to_string(&path).expect("read");
        assert!(raw.contains("\"mcpServers\""));
        assert!(!raw.contains("\"servers\": ["));
        let loaded = McpFile::load(&path)
            .expect("load migrated")
            .expect("migrated file exists");
        assert_eq!(loaded, file);
    }

    #[test]
    fn unknown_shapes_are_errors_not_migrations() {
        for body in [
            r#"{"foo":[]}"#,
            r#"{}"#,
            r#"{"mcpServers":{},"servers":[]}"#,
            r#"[]"#,
            r#"{"servers":{}}"#,
        ] {
            let (_dir, path) = temp_config(body);
            assert!(
                McpFile::load(&path).is_err(),
                "{body} must not load silently"
            );
        }
    }

    #[test]
    fn migrates_standard_files_idempotently() {
        let body = r#"{"mcpServers":{"chess":{"command":"uvx","args":["chess-mcp-server"]}}}"#;
        let (_dir, path) = temp_config(body);
        assert_eq!(
            McpFile::shape(&path).expect("shape"),
            Some(ConfigShape::Standard)
        );
        let file = McpFile::migrate_from_legacy(&path).expect("standard is returned");
        assert_eq!(file.servers["chess"].command.as_deref(), Some("uvx"));
    }

    #[test]
    fn saves_backups_privately() {
        let dir = tempfile::tempdir().expect("temp dir");
        let source = dir.path().join("source.json");
        let backup = dir.path().join("source.json.bak");
        std::fs::write(&source, "legacy content").expect("write source");
        McpFile::backup_to(&source, &backup).expect("backup");
        assert_eq!(
            std::fs::read_to_string(&backup).expect("read backup"),
            "legacy content"
        );
        #[cfg(unix)]
        assert_eq!(file_mode(&backup), 0o600);
    }

    #[test]
    fn sanitizes_names_into_segments() {
        assert_eq!(sanitize_segment("chess"), "chess");
        assert_eq!(sanitize_segment("my server"), "my_server");
        assert_eq!(sanitize_segment(" a / b "), "a_b");
        assert_eq!(sanitize_segment("---"), "");
        assert_eq!(sanitize_segment("@@@"), "");
    }

    #[test]
    fn builds_canonical_guarded_names() {
        assert_eq!(
            guarded_tool_name("weather", "get_forecast").as_deref(),
            Some("mcp__weather__get_forecast")
        );
        assert_eq!(
            guarded_tool_name("my server", "get forecast").as_deref(),
            Some("mcp__my_server__get_forecast")
        );
        let capped = guarded_tool_name(&"s".repeat(30), "tool").expect("capped name");
        assert_eq!(capped, format!("mcp__{}__tool", "s".repeat(16)));
        let long_tool =
            guarded_tool_name("server", &"t".repeat(200)).expect("usable long tool name");
        assert_eq!(long_tool.chars().count(), MAX_TOOL_NAME_CHARS);
    }

    #[test]
    fn trims_a_cap_that_lands_on_a_separator_character() {
        assert_eq!(
            guarded_tool_name("aaaaaaaaaaaaaaa-p", "tool").as_deref(),
            Some("mcp__aaaaaaaaaaaaaaa__tool")
        );
        assert_eq!(
            guarded_tool_name("aaaaaaaaaaaaaaa_p", "tool").as_deref(),
            Some("mcp__aaaaaaaaaaaaaaa__tool")
        );
        assert_eq!(
            guarded_tool_name("aaaaaaaaaaaaaaa p", "tool").as_deref(),
            Some("mcp__aaaaaaaaaaaaaaa__tool")
        );
    }

    #[test]
    fn finds_ops_config_upwards() {
        let dir = tempfile::tempdir().expect("temp dir");
        let nested = dir.path().join("a").join("b");
        let ops = nested.join("ops");
        std::fs::create_dir_all(&ops).expect("ops dir");
        let config = ops.join(CONFIG_FILENAME);
        std::fs::write(&config, "{}").expect("write config");

        assert_eq!(
            find_upwards(&nested.join("crate"), CONFIG_FILENAME),
            Some(config)
        );
        assert_eq!(
            find_upwards(&dir.path().join("elsewhere"), CONFIG_FILENAME),
            None
        );
    }

    #[test]
    fn header_and_env_errors_do_not_echo_values() {
        let file = parse(
            r#"{"mcpServers":{"x":{"url":"https://x.test","headers":{"Authorization":"secret\r\nInjected: yes"}}}}"#,
        );
        let error = file.validate().expect_err("CRLF is refused").to_string();
        assert!(!error.contains("secret"), "{error}");
        assert!(!error.contains("Injected"), "{error}");
        assert!(error.contains("Authorization"), "{error}");

        let file = parse(
            r#"{"mcpServers":{"x":{"command":"npx","env":{"TOKEN":"top-secret\u0000value"}}}}"#,
        );
        let error = file.validate().expect_err("NUL is refused").to_string();
        assert!(!error.contains("top-secret"), "{error}");
        assert!(error.contains("TOKEN"), "{error}");
    }
}
