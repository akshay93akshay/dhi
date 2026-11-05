//! Small shared types exposed by the MCP status API and the registry.

use serde::Serialize;

/// One discovered MCP tool as the status API reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
}

/// The health record of one configured server. The discovered tool list is
/// the cache: there is no second on-disk cache.
#[derive(Debug, Clone, Serialize)]
pub struct McpServerStatus {
    pub name: String,
    /// `"stdio"` or `"http"`.
    pub transport: String,
    pub enabled: bool,
    pub connected: bool,
    pub tools: Vec<McpToolInfo>,
    pub last_error: Option<String>,
    pub call_timeout_secs: u64,
}
