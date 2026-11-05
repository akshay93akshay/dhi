//! Config conversion: the standard `mcpServers` shape (the legacy `servers`
//! array is accepted by `dhi-mcp`) into the client's validated server type,
//! plus the guarded-name rules every MCP tool name goes through.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::bail;

use super::transport::ValidTransport;

/// The guarded-name prefix for every MCP tool.
pub const TOOL_PREFIX: &str = "mcp__";
/// The separator between the server segment and the tool segment. Sanitized
/// segments never contain a double underscore, so the shape is unambiguous.
const SEGMENT_SEPARATOR: &str = "__";
/// Model-visible tool names are capped at 64 characters (OpenAI's limit).
pub(crate) const MAX_TOOL_NAME_CHARS: usize = 64;
/// The server segment alone is capped (config validation enforces this);
/// kept here for the naming tests.
#[cfg(test)]
pub(crate) const MAX_SERVER_SEGMENT_CHARS: usize = 16;
/// Deadline for the initial handshake with one server. Tool-list discovery
/// afterwards is bounded by rmcp's own refresh deadline (30 s).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// One validated, connectable server.
#[derive(Debug, Clone)]
pub struct ValidatedServer {
    /// The name as configured (logs and the notification handler).
    pub name: String,
    /// The sanitized, length-capped segment used in guarded tool names.
    pub segment: String,
    pub transport: ValidTransport,
    /// Deadline for one `tools/call` — the MCP transport timeout, never an
    /// approval TTL.
    pub call_timeout: Duration,
    /// Roles this server is visible to. `None` inherits the fleet default;
    /// `Some(vec![])` is visible to nobody.
    pub visible_to: Option<Vec<String>>,
    /// Per-tool role overrides keyed by the tool's MCP name. An absent tool
    /// inherits [`Self::visible_to`].
    pub tool_visibility: BTreeMap<String, Vec<String>>,
    /// Number of visible tools above which schemas are deferred.
    pub deferred_threshold: Option<usize>,
}

impl TryFrom<dhi_mcp::ValidatedServer> for ValidatedServer {
    type Error = anyhow::Error;

    /// Converts the config crate's validated entry into the client's shape.
    /// Legacy SSE keeps its own transport kind: it is never silently mapped
    /// to Streamable HTTP.
    fn try_from(server: dhi_mcp::ValidatedServer) -> anyhow::Result<Self> {
        if server.segment.is_empty() {
            bail!(
                "MCP server {:?} has a name that sanitizes to no usable tool segment",
                server.name
            );
        }
        let transport = match server.transport_data {
            dhi_mcp::McpTransportData::Stdio { command, args, env } => ValidTransport::Stdio {
                command,
                args,
                env: env.into_iter().collect(),
            },
            dhi_mcp::McpTransportData::Http {
                url,
                headers,
                insecure,
            } => ValidTransport::Http {
                url,
                headers,
                insecure,
            },
            dhi_mcp::McpTransportData::Sse {
                url,
                headers,
                insecure,
            } => ValidTransport::Sse {
                url,
                headers,
                insecure,
            },
        };
        Ok(Self {
            name: server.name,
            segment: server.segment,
            transport,
            call_timeout: server.call_timeout,
            visible_to: server.visible_to,
            tool_visibility: server.tool_visibility,
            deferred_threshold: server.deferred_threshold,
        })
    }
}

/// Maps a config value to a tool-name segment: every character outside
/// `[A-Za-z0-9-]` becomes `_`, underscore runs collapse to one, and leading
/// or trailing underscores/hyphens are trimmed. Collapsed runs guarantee a
/// segment never contains `"__"`, which keeps the `mcp__<server>__<tool>`
/// shape unambiguous. An input with no usable characters sanitizes to an
/// empty string (callers treat that as invalid).
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

/// Truncates an already-sanitized segment to `max` chars. Truncation cannot
/// create an internal underscore run (the input has none); a possible
/// trailing one does not affect the separator rule.
fn capped_segment(segment: &str, max: usize) -> String {
    segment.chars().take(max).collect()
}

/// The guarded name of one MCP tool: `mcp__<server>__<tool>`, the tool
/// segment truncated to keep the whole name within [`MAX_TOOL_NAME_CHARS`].
/// `None` when the tool's MCP name has no usable characters after
/// sanitization or the server segment already exhausts the budget.
pub(crate) fn guarded_name(server_segment: &str, mcp_tool_name: &str) -> Option<String> {
    let server_segment = sanitize_segment(server_segment);
    if server_segment.is_empty() {
        return None;
    }
    let tool_segment = sanitize_segment(mcp_tool_name);
    if tool_segment.is_empty() {
        return None;
    }
    let base = format!("{TOOL_PREFIX}{server_segment}{SEGMENT_SEPARATOR}");
    let remaining = MAX_TOOL_NAME_CHARS.saturating_sub(base.chars().count());
    if remaining == 0 {
        return None;
    }
    Some(format!(
        "{base}{}",
        capped_segment(&tool_segment, remaining)
    ))
}
