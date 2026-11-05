//! The custom-tool dispatch backend.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, RwLock};

use anyhow::{Result, bail};
use async_trait::async_trait;
use dhi_guard::ToolBackend;
use serde_json::Value;

use crate::command;
use crate::config::{CustomTool, Exec};
use crate::http;

/// Dispatches the catalog. `NamedBackend` sends every catalog name here; the
/// blocked set covers a name that registration had to skip because another
/// tool already owned it, so that name keeps reaching the delegate.
pub(crate) struct CustomToolBackend {
    pub(crate) catalog: Arc<BTreeMap<String, CustomTool>>,
    pub(crate) blocked: Arc<RwLock<HashSet<String>>>,
    pub(crate) delegate: Arc<dyn ToolBackend>,
}

#[async_trait]
impl ToolBackend for CustomToolBackend {
    async fn execute(&self, tool: &str, input: &Value) -> Result<Value> {
        let Some(config) = self.catalog.get(tool) else {
            bail!("no custom tool is configured as {tool:?}");
        };
        let blocked = self
            .blocked
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(tool);
        if blocked {
            return self.delegate.execute(tool, input).await;
        }
        match &config.exec {
            Exec::Http(exec) => http::execute(tool, exec, input).await,
            Exec::Command(exec) => command::execute(tool, exec, input).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Delegate;

    #[async_trait]
    impl ToolBackend for Delegate {
        async fn execute(&self, tool: &str, _input: &Value) -> Result<Value> {
            Ok(json!({"delegate": tool}))
        }
    }

    #[tokio::test]
    async fn unknown_tools_fail_closed() {
        let backend = CustomToolBackend {
            catalog: Arc::new(BTreeMap::new()),
            blocked: Arc::new(RwLock::new(HashSet::new())),
            delegate: Arc::new(Delegate),
        };
        let error = backend
            .execute("missing", &json!({}))
            .await
            .expect_err("unknown tool");
        assert!(error.to_string().contains("no custom tool"));
    }
}
