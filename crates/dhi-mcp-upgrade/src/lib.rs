//! Pure upgrade core for DHI's MCP integration.
//!
//! This crate closes five gaps without touching the running server:
//!
//! 1. [`visibility`] — per-agent role visibility with per-tool overrides.
//! 2. [`deferred`] — direct tool registration below a threshold, two meta
//!    tools at or above it, with calls mapped back to the real guarded name.
//! 3. [`legacy_sse`] — a minimal `GET /sse` + POST message client. It is
//!    deliberately separate from Streamable HTTP; see the module docs for the
//!    parts of the legacy protocol it does not implement.
//! 4. [`reload`] — a pure diff planner for live manager reloads.
//! 5. [`testkit`] — reusable in-process fake SSE and auth-protected HTTP
//!    endpoints, behind the optional `testkit` feature (enabled for tests by
//!    the self dev-dependency). These prove header and JSON-RPC plumbing, not
//!    the real rmcp client; the server integration test still drives rmcp.
//!
//! Everything here is side-effect free except [`legacy_sse`], which opens the
//! connections it is asked to open. Server wiring lives in `dhi-server` and
//! `dhi-mcp-runtime`.

pub mod catalog;
pub mod deferred;
pub mod legacy_sse;
pub mod naming;
pub mod reload;
#[cfg(feature = "testkit")]
pub mod testkit;
pub mod visibility;

pub use catalog::{CatalogTool, ServerCatalog, ToolDefinition};
pub use deferred::{
    CALL_TOOL_NAME, DEFAULT_MAX_ARGUMENT_BYTES, DeferredError, DeferredPolicy, DeferredToolset,
    DirectTool, GuardScope, GuardedCall, GuardedInvoker, LIST_TOOLS_NAME, ServerToolListing,
    ToolCallOutcome, ToolPlan, ToolSummary, plan_tools,
};
pub use naming::{
    MAX_SERVER_SEGMENT_CHARS, MAX_TOOL_NAME_CHARS, SEGMENT_SEPARATOR, TOOL_PREFIX,
    guarded_tool_name,
};
pub use reload::{
    ReloadAction, ReloadError, ReloadPlan, ServerSpec, config_fingerprint,
    config_fingerprint_validated, plan_reload,
};
pub use visibility::{
    Role, RoleParseError, SkippedTool, VisibilityReport, VisibleTool, visibility_report,
    visibility_report_for_role, visible_tools, visible_tools_for_role,
};
