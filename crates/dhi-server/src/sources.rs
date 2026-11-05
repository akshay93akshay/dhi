//! Sources: the guard's closed delegate.
//!
//! The worker's registry is open: a host may register native tools or guarded
//! tools at any time. No native arithmetic tool ships. Web research is
//! registered by the server through `dhi-web`, and MCP servers live in
//! [`crate::mcp`], both behind the one guard.
//!
//! [`SourcesBackend`] is the delegate behind the web/MCP routing backends. It
//! owns no tools, so every delegated call is a mis-registration and fails
//! closed.

use anyhow::anyhow;
use async_trait::async_trait;
use dhi_guard::ToolBackend;
use rig::tool::server::ToolServerHandle;
use serde_json::Value;

/// The names registered by [`register_sources`], for tests and logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRegistration {
    pub registered: Vec<String>,
}

/// Registers the native tool sources onto the worker's shared registry.
/// No native arithmetic tool registers; guarded web and coding tools are
/// registered separately by their own modules. The registry stays open —
/// these helpers are conveniences, not gates.
pub fn register_sources(_handle: &ToolServerHandle) -> anyhow::Result<SourceRegistration> {
    Ok(SourceRegistration {
        registered: Vec::new(),
    })
}

/// The guard's closed delegate behind the web and MCP routing backends. No
/// tool is served here; any call fails closed like the ship-with-no-backend
/// behavior.
#[derive(Debug, Default, Clone, Copy)]
pub struct SourcesBackend;

impl SourcesBackend {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ToolBackend for SourcesBackend {
    async fn execute(&self, tool: &str, _input: &Value) -> anyhow::Result<Value> {
        Err(anyhow!("no tool backend configured for {tool}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn registration_adds_no_native_tool() {
        let handle = rig::tool::server::ToolServer::new().run();
        let registration = register_sources(&handle).unwrap();
        assert!(registration.registered.is_empty());
        let defs = handle.tool_defs(None).await.unwrap();
        assert!(defs.is_empty());
    }

    #[tokio::test]
    async fn backend_fails_closed() {
        let backend = SourcesBackend::new();
        let err = backend
            .execute("unknown_tool", &json!({}))
            .await
            .expect_err("the closed delegate serves nothing");
        assert!(
            err.to_string().contains("no tool backend configured"),
            "{err}"
        );
    }
}
