//! MCP as a first-class, guarded tool source.
//!
//! This crate owns the MCP runtime: config conversion, connections, guarded
//! discovery and registration, dispatch, and status. `dhi-server` re-exports
//! it from its `mcp` module so `dhi_server::mcp::*` paths keep working.
//!
//! Model Context Protocol servers configured in the standard `mcpServers`
//! shape (or the legacy `servers` array) of `ops/mcp.json` (see
//! `ops/mcp.example.json`) are loaded through [`dhi_mcp`] and connected at
//! startup by the [`McpManager`],
//! their tools are discovered through the rmcp client
//! ([`rig::tool::rmcp`]), and each discovered tool is registered on the
//! worker's shared registry ONLY as a guarded tool named
//! `mcp__<server>__<tool>` — built with the guarded-tool registration
//! machinery ([`dhi_agent::register_guarded_tool`]). A raw MCP tool is never attached
//! to an agent: every invocation travels model call → guarded `DynamicTool`
//! → the one `ToolSink` ([`dhi_agent::ToolSink`] over the guard's
//! `GuardedExecutor`: record → policy → allow/deny/ask → audit) →
//! [`McpBackend`] → that server's `call_tool`.
//!
//! Lifecycle: connections are held for the process lifetime. When the
//! fetched rmcp API supports it — it does, via a custom
//! [`rmcp::ClientHandler`] — `notifications/tools/list_changed` re-registers
//! the server's tools through the same guarded path (replace + remove, never
//! a raw registration). Per-server failures degrade with a warning and never
//! kill the server process; [`McpManager::shutdown`] closes every connection
//! and retires its guarded registrations.
//!
//! Health lives in the manager's status cache (the discovered tool list is
//! the cache) and is exposed by the read-only `/api/mcp/servers` routes;
//! `POST .../refresh` re-runs discovery. Config changes stay with the
//! `dhi mcp` CLI and apply on restart.
//!
//! Dispatch mapping: the model's arguments go to the server's `call_tool`
//! verbatim; rmcp's tested tool adapter turns a successful result into a
//! [`rig::tool::ToolOutput`] (structured content passes through, text becomes
//! `{"text": …}`), and an `is_error` result, a timeout (the server's
//! configured per-call deadline — distinct from approval TTLs), or a
//! transport failure becomes a model-readable tool error. No path panics.
//!
//! # Module ownership contract (parallel MCP wiring gates)
//!
//! - **Gate A** owns `registry.rs` (plus `config.rs` visibility fields,
//!   `dhi-server/src/main.rs` registration, and `dhi-agent` specs).
//! - **Gate B** owns `manager.rs`, `reload.rs`, and `dhi-server/src/lib.rs` routes.
//! - **Gate C** owns `transport.rs`.
//! - No gate edits `lib.rs` except additive re-exports if unavoidable.

mod config;
mod manager;
mod registry;
mod transport;
mod types;

pub mod reload;

#[cfg(test)]
mod tests;

pub use config::{CONNECT_TIMEOUT, TOOL_PREFIX, ValidatedServer, sanitize_segment};
pub use manager::{McpManager, manager_from_config};
pub use registry::{
    DEFAULT_DEFERRED_THRESHOLD, GuardedMcpRegistry, McpBackend, McpRolePlanner, McpRoutingBackend,
    McpVisibilityConfig, McpWorkerToolView, RegistryToolSnapshot, RolePlan, SharedMcpPlanner,
    register_deferred_meta_tools,
};
pub use reload::{ReloadFailure, ReloadReport, ServerSummary};
pub use transport::ValidTransport;
pub use types::{McpServerStatus, McpToolInfo};
