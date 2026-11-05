//! Small extension seam for tool sources.
//!
//! A source registers its guarded tools on the worker's shared registry. A
//! source that also needs dispatch provides a `NamedBackend` wrapper around
//! the existing backend chain. Both `dhi-skills` and `dhi-custom-tools` use
//! this seam, so `dhi-server` wires them once and never needs a per-feature
//! edit.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use dhi_agent::ToolSink;
use dhi_guard::ToolBackend;
use rig::tool::server::ToolServerHandle;

/// One registration call for a source.
pub struct ToolSourceContext<'a> {
    pub handle: &'a ToolServerHandle,
    pub sink: Arc<dyn ToolSink>,
}

/// A source of guarded tools. Registration is synchronous because every
/// source turns a local catalog into `DynamicTool` registrations; remote
/// discovery stays in the MCP runtime.
pub trait ToolSource: Send + Sync {
    fn name(&self) -> &'static str;
    fn register(&self, context: &ToolSourceContext<'_>) -> Result<Vec<String>>;
}

/// The outcome of one source registration. A source error is isolated: the
/// server logs it and keeps the other sources.
pub struct SourceRegistration {
    pub source: &'static str,
    pub tools: Vec<String>,
    pub error: Option<String>,
}

/// A set of sources registered in a stable order.
pub struct ToolSourceRegistry {
    sources: Vec<Arc<dyn ToolSource>>,
}

impl ToolSourceRegistry {
    pub fn new(sources: Vec<Arc<dyn ToolSource>>) -> Self {
        Self { sources }
    }

    pub fn register_all(&self, context: &ToolSourceContext<'_>) -> Vec<SourceRegistration> {
        self.sources
            .iter()
            .map(|source| match source.register(context) {
                Ok(tools) => SourceRegistration {
                    source: source.name(),
                    tools,
                    error: None,
                },
                Err(error) => SourceRegistration {
                    source: source.name(),
                    tools: Vec::new(),
                    error: Some(format!("{error:#}")),
                },
            })
            .collect()
    }
}

/// Routes a fixed set of tool names to one backend and everything else to the
/// delegate. This keeps extension backends out of `dhi-server` routing.
pub struct NamedBackend {
    names: HashSet<String>,
    backend: Arc<dyn ToolBackend>,
    delegate: Arc<dyn ToolBackend>,
}

impl NamedBackend {
    pub fn new(
        names: impl IntoIterator<Item = String>,
        backend: Arc<dyn ToolBackend>,
        delegate: Arc<dyn ToolBackend>,
    ) -> Self {
        Self {
            names: names.into_iter().collect(),
            backend,
            delegate,
        }
    }

    pub fn empty(delegate: Arc<dyn ToolBackend>) -> Self {
        Self {
            names: HashSet::new(),
            backend: delegate.clone(),
            delegate,
        }
    }
}

#[async_trait]
impl ToolBackend for NamedBackend {
    async fn execute(
        &self,
        tool: &str,
        input: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        if self.names.contains(tool) {
            self.backend.execute(tool, input).await
        } else {
            self.delegate.execute(tool, input).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dhi_core::{ToolCallOutcome, ToolCallRequest};
    use serde_json::json;
    use uuid::Uuid;

    struct NoopSink;

    #[async_trait]
    impl ToolSink for NoopSink {
        async fn execute(
            &self,
            _task_id: Uuid,
            _intent_id: Uuid,
            _request: ToolCallRequest,
        ) -> anyhow::Result<ToolCallOutcome> {
            Ok(ToolCallOutcome::Allowed { output: json!({}) })
        }
    }

    struct FixedSource {
        name: &'static str,
        tools: Vec<String>,
        fail: bool,
    }

    impl ToolSource for FixedSource {
        fn name(&self) -> &'static str {
            self.name
        }

        fn register(&self, _context: &ToolSourceContext<'_>) -> Result<Vec<String>> {
            if self.fail {
                anyhow::bail!("deliberate failure");
            }
            Ok(self.tools.clone())
        }
    }

    struct EchoBackend;

    #[async_trait]
    impl ToolBackend for EchoBackend {
        async fn execute(
            &self,
            tool: &str,
            _input: &serde_json::Value,
        ) -> anyhow::Result<serde_json::Value> {
            Ok(json!({"tool": tool}))
        }
    }

    #[test]
    fn source_errors_do_not_drop_other_sources() {
        let sources: Vec<Arc<dyn ToolSource>> = vec![
            Arc::new(FixedSource {
                name: "ok",
                tools: vec!["one".to_string()],
                fail: false,
            }),
            Arc::new(FixedSource {
                name: "bad",
                tools: Vec::new(),
                fail: true,
            }),
        ];
        let registry = ToolSourceRegistry::new(sources);
        let handle = rig::tool::server::ToolServer::new().run();
        let context = ToolSourceContext {
            handle: &handle,
            sink: Arc::new(NoopSink),
        };
        let results = registry.register_all(&context);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].tools, ["one"]);
        assert!(results[0].error.is_none());
        assert!(
            results[1]
                .error
                .as_deref()
                .unwrap_or("")
                .contains("deliberate")
        );
    }

    #[tokio::test]
    async fn named_backend_routes_only_its_names() {
        let delegate = Arc::new(EchoBackend);
        let backend = NamedBackend::new(["own".to_string()], Arc::new(EchoBackend), delegate);
        assert_eq!(
            backend.execute("own", &json!({})).await.expect("own"),
            json!({"tool": "own"})
        );
        assert_eq!(
            backend.execute("other", &json!({})).await.expect("other"),
            json!({"tool": "other"})
        );
    }
}
