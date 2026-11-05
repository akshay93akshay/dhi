//! Standard MCP configuration for DHI.
//!
//! This crate owns the `mcpServers` config model and the `dhi mcp` CLI. It
//! deliberately does not connect to servers: the existing `dhi-server` MCP
//! client remains in place until it is wired to this model.
//!
//! See [`McpFile`] for the standard shape, [`McpFile::migrate_from_legacy`]
//! for the older DHI `servers` array, and [`config_path`] for config
//! discovery (`DHI_MCP_CONFIG`, else `ops/mcp.json` upward from the crate).

mod config;
pub mod edit;

pub use config::{
    CONFIG_FILENAME, ConfigShape, DEFAULT_CALL_TIMEOUT, DEFAULT_DEFERRED_THRESHOLD,
    MAX_SERVER_SEGMENT_CHARS, MAX_TOOL_NAME_CHARS, McpFile, McpServerEntry, McpTransport,
    McpTransportData, SEGMENT_SEPARATOR, TOOL_PREFIX, ValidatedServer, config_path,
    guarded_tool_name, sanitize_segment,
};
pub use edit::{
    EditError, add_server, delete_server, is_secret_header, load_lenient, mask_headers,
    update_server, validate_entry,
};
