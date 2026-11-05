//! Guarded tools inside the Rig loop.
//!
//! The model calls tools in-loop through the agent's own
//! [`ToolServer`](rig::tool::server::ToolServer) registry. Every domain tool
//! is a *guarded* [`DynamicTool`]: its callback reads the run scope
//! ([`RunScope`]) from the dispatch [`ToolContext`] — stamped by the runtime,
//! never by the model — builds a [`ToolCallRequest`], and hands it to the
//! [`ToolSink`]. The sink is the one guard (`GuardedExecutor` behind
//! `ToolExecutor`), so a tool call made inside the loop carries the same
//! records, verdicts, and blocking approvals as a direct engine call.
//!
//! Outcomes map model-visibly:
//! - `Allowed` → the JSON output the model reads.
//! - `Denied` → a `permission_denied` tool error carrying the reason, so the
//!   model sees *why* and can adjust.
//! - `Pending` → an internal error: approvals must block inside the call, so
//!   a sink that surfaces `Pending` is a bug and fails loudly.

use std::sync::Arc;

use anyhow::anyhow;
use async_trait::async_trait;
use dhi_core::{ToolCallOutcome, ToolCallRequest};
use rig::message::ToolName;
use rig::tool::server::ToolServerHandle;
use rig::tool::{ContextValue, DynamicTool, ToolContext, ToolExecutionError, ToolOutput};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The guarded path from an in-loop tool call to the one guard.
///
/// dhi-agent has no dependency on dhi-guard: the host (dhi-server) adapts its
/// executor to this trait. Object-safe and shared, so one sink serves every
/// guarded tool registration.
#[async_trait]
pub trait ToolSink: Send + Sync {
    /// Runs one guarded tool call. Blocks through any approval (the executor
    /// never returns `Pending`).
    async fn execute(
        &self,
        task_id: Uuid,
        intent_id: Uuid,
        request: ToolCallRequest,
    ) -> anyhow::Result<ToolCallOutcome>;
}

/// Identifies the intent a run executes, stamped into every tool dispatch.
///
/// The runtime inserts it into the dispatch [`ToolContext`] before the model
/// sees a single tool. A tool can therefore never learn a foreign task or
/// intent id from its arguments: the id comes only from the scope, and the
/// guard records the call under exactly that intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunScope {
    pub task_id: Uuid,
    pub intent_id: Uuid,
}

impl ContextValue for RunScope {
    const KEY: &'static str = "dhi.run_scope";
}

/// The role a dispatched tool call belongs to: `worker`, or
/// `specialist:<name>` for a delegated run.
///
/// The runtime stamps it into the dispatch [`ToolContext`] next to
/// [`RunScope`]; a role-aware tool (for example a deferred MCP meta-tool) reads
/// it to resolve the caller's own visible set. The model can never supply it:
/// only the dispatch context carries it, exactly like the run scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRole(pub String);

impl RunRole {
    /// The worker/manager role.
    pub fn worker() -> Self {
        Self("worker".to_string())
    }

    /// A specialist run tagged with its specialist name.
    pub fn specialist(name: &str) -> Self {
        Self(format!("specialist:{}", name.trim()))
    }

    /// The role as the canonical string (`worker`, `specialist:<name>`).
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl ContextValue for RunRole {
    const KEY: &'static str = "dhi.run_role";
}

/// A source of the tool registry one agent run attaches.
///
/// The host installs a view when a role must see only a filtered subset of the
/// shared registry (for example per-role MCP visibility). The default runtime
/// attaches its full shared registry. A view is consulted per `execute` call,
/// so it always reads the registry as it exists at dispatch time.
pub trait ToolServerView: Send + Sync {
    /// The tool registry for the next agent run.
    fn tool_server(&self) -> rig::tool::server::ToolServerHandle;
}

/// Builds one guarded tool: the model-visible door to the sink.
///
/// The callback reads [`RunScope`] from the dispatch context (an internal
/// error when absent — every guarded dispatch is stamped by the runtime),
/// wraps the model's arguments into a [`ToolCallRequest`] named `name`, and
/// maps the sink outcome as documented on the module.
pub fn guarded_dynamic_tool(
    name: &str,
    description: &str,
    schema: serde_json::Value,
    sink: Arc<dyn ToolSink>,
) -> anyhow::Result<DynamicTool> {
    let tool_name =
        ToolName::new(name).map_err(|err| anyhow!("invalid tool name {name:?}: {err}"))?;
    let tool_label = name.to_string();
    Ok(DynamicTool::new_with_context(
        tool_name,
        description,
        schema,
        move |context: &mut ToolContext, args: serde_json::Value| {
            let sink = Arc::clone(&sink);
            let tool = tool_label.clone();
            Box::pin(async move {
                let scope: RunScope = context.require().map_err(|err| {
                    ToolExecutionError::other(format!(
                        "guarded tool dispatch is missing its run scope: {err}"
                    ))
                })?;
                let request = ToolCallRequest { tool, input: args };
                match sink.execute(scope.task_id, scope.intent_id, request).await {
                    Ok(ToolCallOutcome::Allowed { output }) => Ok(ToolOutput::json(output)),
                    Ok(ToolCallOutcome::Denied { reason }) => {
                        Err(ToolExecutionError::permission_denied(reason))
                    }
                    Ok(ToolCallOutcome::Pending { approval_id }) => {
                        Err(ToolExecutionError::other(format!(
                            "tool call is parked behind approval {approval_id}; \
                             approvals must block inside the tool call"
                        )))
                    }
                    Err(err) => Err(ToolExecutionError::other(err.to_string())),
                }
            })
        },
    ))
}

/// The registry is open: these helpers exist for convenience, not as a gate.
/// Anything the host can register through `ToolServerHandle` directly
/// (`add_tool`, `add_dynamic_tool`, `add_registered_tool`, MCP-managed sets)
/// is legitimate; `register_native_tool` is the plain pass-through.
pub fn register_native_tool(handle: &ToolServerHandle, tool: DynamicTool) {
    handle.add_dynamic_tool(tool);
}

/// The definition of one guarded tool: name, model-facing description, and
/// the JSON schema of its arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardedToolSpec {
    pub name: String,
    pub description: String,
    pub schema: serde_json::Value,
}

impl GuardedToolSpec {
    pub fn new(name: &str, description: &str, schema: serde_json::Value) -> Self {
        Self {
            name: name.to_string(),
            description: description.to_string(),
            schema,
        }
    }
}

/// Registers one guarded tool on the shared registry: the model calls it
/// in-loop, and every call is routed through `sink` (the one guard) with the
/// dispatch's `RunScope`. Errors here are registration errors (an invalid
/// name); tool failures surface to the model at call time.
pub fn register_guarded_tool(
    handle: &ToolServerHandle,
    spec: GuardedToolSpec,
    sink: Arc<dyn ToolSink>,
) -> anyhow::Result<()> {
    let tool = guarded_dynamic_tool(&spec.name, &spec.description, spec.schema, sink)?;
    handle.add_dynamic_tool(tool);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records every call it sees and replays a scripted outcome.
    #[derive(Default)]
    struct FakeSink {
        calls: Mutex<Vec<(Uuid, Uuid, ToolCallRequest)>>,
        outcome: Mutex<Option<anyhow::Result<ToolCallOutcome>>>,
    }

    impl FakeSink {
        fn allows_with(output: serde_json::Value) -> Self {
            Self {
                outcome: Mutex::new(Some(Ok(ToolCallOutcome::Allowed { output }))),
                ..Self::default()
            }
        }

        fn denies_with(reason: &str) -> Self {
            Self {
                outcome: Mutex::new(Some(Ok(ToolCallOutcome::Denied {
                    reason: reason.to_string(),
                }))),
                ..Self::default()
            }
        }

        fn calls(&self) -> Vec<(Uuid, Uuid, ToolCallRequest)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ToolSink for FakeSink {
        async fn execute(
            &self,
            task_id: Uuid,
            intent_id: Uuid,
            request: ToolCallRequest,
        ) -> anyhow::Result<ToolCallOutcome> {
            self.calls
                .lock()
                .unwrap()
                .push((task_id, intent_id, request));
            self.outcome
                .lock()
                .unwrap()
                .take()
                .expect("the fake sink scripts exactly one call")
        }
    }

    fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" }
            },
            "required": ["query"]
        })
    }

    fn tool(sink: Arc<FakeSink>) -> DynamicTool {
        guarded_dynamic_tool("web_read", "an example guarded tool", schema(), sink)
            .expect("a valid tool name builds")
    }

    #[tokio::test]
    async fn scope_is_stamped_from_the_context_not_the_model_arguments() {
        let sink = Arc::new(FakeSink::allows_with(serde_json::json!({
            "text": "the page says hello",
        })));
        let tool = tool(sink.clone());

        let task_id = Uuid::new_v4();
        let intent_id = Uuid::new_v4();
        let mut context = ToolContext::new();
        context
            .insert(RunScope { task_id, intent_id })
            .expect("the scope serializes");

        // The model tries to smuggle a different task and intent through its
        // arguments. Neither field reaches the sink: the scope wins.
        let output = tool
            .run_with(
                &mut context,
                serde_json::json!({ "query": "hello", "task_id": Uuid::new_v4(), "intent_id": Uuid::new_v4() }),
            )
            .await
            .expect("an allowed call returns output");

        assert_eq!(
            output.as_json().and_then(|value| value.get("text")),
            Some(&serde_json::json!("the page says hello"))
        );
        let calls = sink.calls();
        assert_eq!(calls.len(), 1);
        let (seen_task, seen_intent, request) = &calls[0];
        assert_eq!((*seen_task, *seen_intent), (task_id, intent_id));
        assert_eq!(request.tool, "web_read");
        assert_eq!(
            request
                .input
                .get("query")
                .and_then(serde_json::Value::as_str),
            Some("hello"),
            "the model's actual arguments are the request input"
        );
    }

    #[tokio::test]
    async fn a_dispatch_without_a_scope_is_an_internal_error() {
        let sink = Arc::new(FakeSink::allows_with(serde_json::json!({})));
        let tool = tool(sink);

        let err = tool
            .run_with(&mut ToolContext::new(), serde_json::json!({ "query": "x" }))
            .await
            .expect_err("an unstamped dispatch cannot reach the sink");
        assert!(
            err.to_string().contains("run scope"),
            "the error names the missing scope: {err}"
        );
    }

    #[tokio::test]
    async fn an_allowed_call_returns_the_output_the_model_reads() {
        let sink = Arc::new(FakeSink::allows_with(serde_json::json!({
            "url": "https://example.test",
            "title": "Example",
        })));
        let tool = tool(sink);
        let mut context = ToolContext::new();
        context
            .insert(RunScope {
                task_id: Uuid::new_v4(),
                intent_id: Uuid::new_v4(),
            })
            .unwrap();

        let output = tool
            .run_with(&mut context, serde_json::json!({ "query": "x" }))
            .await
            .expect("allowed calls succeed");
        assert_eq!(
            output.as_json(),
            Some(&serde_json::json!({
                "url": "https://example.test",
                "title": "Example",
            }))
        );
    }

    #[tokio::test]
    async fn a_denied_call_is_a_permission_error_the_model_can_read() {
        let sink = Arc::new(FakeSink::denies_with(
            "rule deny-shell: shells are not allowed",
        ));
        let tool = tool(sink);
        let mut context = ToolContext::new();
        context
            .insert(RunScope {
                task_id: Uuid::new_v4(),
                intent_id: Uuid::new_v4(),
            })
            .unwrap();

        let err = tool
            .run_with(&mut context, serde_json::json!({ "query": "x" }))
            .await
            .expect_err("a denial is a tool error");
        let text = err.to_string();
        assert!(
            text.contains("deny-shell"),
            "the reason travels to the model: {text}"
        );
        // Model-visible output carries the same reason, not a bare apology.
        let feedback = err.model_feedback().unwrap_or_default();
        assert!(
            feedback.contains("shells are not allowed") || text.contains("shells are not allowed"),
            "the model sees the denial reason: {text} / {feedback}"
        );
    }

    #[tokio::test]
    async fn a_pending_outcome_is_defensively_an_internal_error() {
        let sink = Arc::new(FakeSink::default());
        *sink.outcome.lock().unwrap() = Some(Ok(ToolCallOutcome::Pending {
            approval_id: Uuid::new_v4(),
        }));
        let tool = tool(sink);
        let mut context = ToolContext::new();
        context
            .insert(RunScope {
                task_id: Uuid::new_v4(),
                intent_id: Uuid::new_v4(),
            })
            .unwrap();

        let err = tool
            .run_with(&mut context, serde_json::json!({}))
            .await
            .expect_err("Pending must never surface as success");
        assert!(
            err.to_string().contains("approvals must block"),
            "the pending error states the invariant: {err}"
        );
    }

    #[tokio::test]
    async fn a_sink_error_becomes_a_tool_error() {
        let sink = Arc::new(FakeSink::default());
        *sink.outcome.lock().unwrap() =
            Some(Err(anyhow!("no tool backend configured for example")));
        let tool = tool(sink);
        let mut context = ToolContext::new();
        context
            .insert(RunScope {
                task_id: Uuid::new_v4(),
                intent_id: Uuid::new_v4(),
            })
            .unwrap();

        let err = tool
            .run_with(&mut context, serde_json::json!({}))
            .await
            .expect_err("backend failures surface as tool errors");
        assert!(err.to_string().contains("no tool backend"), "{err}");
    }

    #[test]
    fn an_empty_tool_name_is_refused_at_registration() {
        let sink = Arc::new(FakeSink::default());
        let err = guarded_dynamic_tool("", "d", schema(), sink)
            .expect_err("an empty name is not a tool name");
        assert!(err.to_string().contains("invalid tool name"), "{err}");
    }

    /// The registry is open and mutable at runtime: registration is visible
    /// to the next tool-definitions read, and removal drops only its own
    /// entry.
    #[tokio::test]
    async fn registry_add_and_remove_are_visible_at_runtime() {
        let handle = rig::tool::server::ToolServer::new().run();

        // One guarded registration through the helper, one native pass-through.
        let spec = GuardedToolSpec::new(
            "temporary_tool",
            "evaluate an arithmetic expression",
            serde_json::json!({
                "type": "object",
                "properties": { "expression": { "type": "string" } },
                "required": ["expression"]
            }),
        );
        register_guarded_tool(&handle, spec, Arc::new(FakeSink::default()))
            .expect("a valid spec registers");
        let native = DynamicTool::new(
            ToolName::new("record_fact").expect("valid"),
            "the worker's fact draft",
            serde_json::json!({}),
            |_args| Box::pin(async { Ok(ToolOutput::text("ok")) }),
        );
        register_native_tool(&handle, native);

        let defs = handle.tool_defs(None).await.expect("definitions read");
        let names: Vec<String> = defs
            .iter()
            .map(|def| def.name.as_str().to_string())
            .collect();
        assert!(names.contains(&"temporary_tool".to_string()), "{names:?}");
        assert!(names.contains(&"record_fact".to_string()), "{names:?}");

        // Removal is exact: only the removed name disappears.
        handle.remove_tool("temporary_tool");
        let defs = handle.tool_defs(None).await.expect("definitions read");
        let names: Vec<String> = defs
            .iter()
            .map(|def| def.name.as_str().to_string())
            .collect();
        assert!(!names.contains(&"temporary_tool".to_string()), "{names:?}");
        assert!(names.contains(&"record_fact".to_string()), "{names:?}");
    }

    #[test]
    fn register_guarded_tool_rejects_an_invalid_spec() {
        let handle = rig::tool::server::ToolServer::new().run();
        let err = register_guarded_tool(
            &handle,
            GuardedToolSpec::new("", "no name", serde_json::json!({})),
            Arc::new(FakeSink::default()),
        )
        .expect_err("an empty name is a registration error");
        assert!(err.to_string().contains("invalid tool name"), "{err}");
    }
}
