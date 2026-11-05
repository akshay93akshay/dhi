//! The `load_skill` meta-tool and its backend.
//!
//! The model-visible tool is always guarded: its callback reads the trusted
//! run role from the dispatch context, stamps it into the sink request under
//! [`TRUSTED_ROLE_KEY`] (overwriting any model-supplied value), and calls the
//! existing sink. The backend never trusts a role from the model.

use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use dhi_agent::{RunRole, RunScope, ToolSink};
use dhi_core::{ToolCallOutcome, ToolCallRequest};
use dhi_guard::ToolBackend;
use rig::message::ToolName;
use rig::tool::{DynamicTool, ToolContext, ToolExecutionError, ToolOutput};
use serde_json::{Value, json};
use tracing::info;

use crate::catalog::{SkillCatalog, is_recognized_role};

/// The model-visible tool name.
pub const LOAD_SKILL: &str = "load_skill";
/// The private sink-request key carrying the trusted dispatch role. Model
/// arguments are overwritten at this key before the guard sees the call.
pub const TRUSTED_ROLE_KEY: &str = "__dhi_role";

const MAX_LIST_DESCRIPTION_CHARS: usize = 160;

/// The model-visible definition: one optional `name` string, nothing else.
pub fn load_skill_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "A skill name to load. Omit to list the skills visible to this run."
            }
        },
        "additionalProperties": false
    })
}

/// Builds the guarded `load_skill` tool. Every call goes through `sink`, so
/// the guard and its audit trail see the tool exactly like any other.
pub fn load_skill_tool(sink: Arc<dyn ToolSink>) -> Result<DynamicTool> {
    let tool_name = ToolName::new(LOAD_SKILL)
        .map_err(|error| anyhow!("invalid tool name {LOAD_SKILL:?}: {error}"))?;
    Ok(DynamicTool::new_with_context(
        tool_name,
        "Load a skill's full instructions by name. Call without a name to list \
         the skills visible to the current role.",
        load_skill_schema(),
        move |context: &mut ToolContext, args: Value| {
            let sink = Arc::clone(&sink);
            Box::pin(async move {
                let scope: RunScope = context.require().map_err(|error| {
                    ToolExecutionError::other(format!(
                        "load_skill dispatch is missing its run scope: {error}"
                    ))
                })?;
                let role: RunRole = context.require().map_err(|error| {
                    ToolExecutionError::other(format!(
                        "load_skill dispatch is missing its run role: {error}"
                    ))
                })?;

                let mut input = args;
                match &mut input {
                    Value::Object(fields) => {
                        fields.insert(
                            TRUSTED_ROLE_KEY.to_string(),
                            Value::String(role.as_str().to_string()),
                        );
                    }
                    _ => {
                        input = json!({ TRUSTED_ROLE_KEY: role.as_str() });
                    }
                }

                let request = ToolCallRequest {
                    tool: LOAD_SKILL.to_string(),
                    input,
                };
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
                    Err(error) => Err(ToolExecutionError::other(error.to_string())),
                }
            })
        },
    ))
}

/// The dispatch half of `load_skill`. It only reads the role the guarded
/// callback stamped into the request; model arguments never carry authority.
pub struct SkillsBackend {
    catalog: Arc<SkillCatalog>,
}

impl SkillsBackend {
    pub fn new(catalog: impl Into<Arc<SkillCatalog>>) -> Self {
        Self {
            catalog: catalog.into(),
        }
    }
}

#[async_trait]
impl ToolBackend for SkillsBackend {
    async fn execute(&self, tool: &str, input: &Value) -> Result<Value> {
        if tool != LOAD_SKILL {
            bail!("skills backend cannot execute {tool:?}");
        }

        let role = input
            .get(TRUSTED_ROLE_KEY)
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("load_skill is missing its trusted run role"))?;
        if !is_recognized_role(role) {
            // An unknown role sees none. This matches the visibility rule and
            // avoids accidentally widening an allow-list on a new role name.
            info!(role, "load_skill called with an unrecognized role");
        }

        let name = input
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty());

        match name {
            None => {
                let skills = self.catalog.visible_to(role);
                info!(
                    role,
                    skills = skills.len(),
                    "load_skill listed the visible catalog"
                );
                let listed: Vec<Value> = skills
                    .iter()
                    .map(|skill| {
                        json!({
                            "name": skill.name,
                            "description": short_description(&skill.description),
                        })
                    })
                    .collect();
                Ok(json!({ "skills": listed }))
            }
            Some(name) => {
                let skill = self
                    .catalog
                    .get(name)
                    .ok_or_else(|| anyhow!("unknown skill {name:?}"))?;
                if !skill.is_visible_to(role) {
                    info!(role, skill = name, "load_skill denied an invisible skill");
                    bail!("skill {name:?} is not visible to role {role:?}");
                }
                info!(role, skill = name, "load_skill loaded a skill");
                Ok(json!({
                    "name": skill.name,
                    "description": skill.description,
                    "instructions": skill.instructions,
                }))
            }
        }
    }
}

fn short_description(description: &str) -> String {
    let description = description.trim();
    if description.chars().count() <= MAX_LIST_DESCRIPTION_CHARS {
        return description.to_string();
    }
    let mut shortened: String = description
        .chars()
        .take(MAX_LIST_DESCRIPTION_CHARS - 3)
        .collect();
    shortened.push_str("...");
    shortened
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;

    use dhi_agent::{RunRole, RunScope};
    use dhi_core::{ToolCallOutcome, ToolCallRequest};
    use rig::tool::ToolContext;
    use rig::tool::server::ToolServer;
    use tempfile::TempDir;
    use uuid::Uuid;

    use crate::catalog::SkillCatalog;
    use crate::{SkillsRuntime, ToolSource};
    use dhi_tool_source::ToolSourceContext;

    #[derive(Default)]
    struct RecordingSink {
        calls: Mutex<Vec<(Uuid, Uuid, ToolCallRequest)>>,
        output: Value,
    }

    impl RecordingSink {
        fn allows(output: Value) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                output,
            }
        }

        fn calls(&self) -> Vec<(Uuid, Uuid, ToolCallRequest)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ToolSink for RecordingSink {
        async fn execute(
            &self,
            task_id: Uuid,
            intent_id: Uuid,
            request: ToolCallRequest,
        ) -> Result<ToolCallOutcome> {
            self.calls
                .lock()
                .unwrap()
                .push((task_id, intent_id, request));
            Ok(ToolCallOutcome::Allowed {
                output: self.output.clone(),
            })
        }
    }

    struct EchoBackend;

    #[async_trait]
    impl ToolBackend for EchoBackend {
        async fn execute(&self, tool: &str, _input: &Value) -> Result<Value> {
            Ok(json!({ "tool": tool }))
        }
    }

    fn write_skill(root: &std::path::Path, directory: &str, contents: &str) {
        let directory = root.join(directory);
        fs::create_dir_all(&directory).expect("skill directory");
        fs::write(directory.join("SKILL.md"), contents).expect("SKILL.md");
    }

    fn catalog_root() -> TempDir {
        let root = tempfile::tempdir().expect("temporary skills directory");
        write_skill(
            root.path(),
            "rust-review",
            "---\n\
             name: rust-review\n\
             description: Review Rust changes\n\
             visible_to: [worker, specialist:researcher]\n\
             ---\n\
             Read the diff carefully.\n",
        );
        write_skill(
            root.path(),
            "reporter-notes",
            "---\n\
             description: Notes for the reporter\n\
             visible_to: [reporter]\n\
             ---\n\
             Reporter-only instructions.\n",
        );
        write_skill(
            root.path(),
            "hidden",
            "---\n\
             description: Nobody sees this\n\
             visible_to: []\n\
             ---\n\
             Hidden instructions.\n",
        );
        write_skill(
            root.path(),
            "open",
            "---\n\
             description: Open to every recognized role\n\
             ---\n\
             Open instructions.\n",
        );
        root
    }

    async fn backend_list(runtime: &Arc<SkillsRuntime>, role: &str) -> Value {
        let backend = runtime.backend(Arc::new(EchoBackend));
        backend
            .execute(LOAD_SKILL, &json!({ TRUSTED_ROLE_KEY: role }))
            .await
            .expect("list succeeds")
    }

    fn listed_names(output: &Value) -> Vec<String> {
        output["skills"]
            .as_array()
            .expect("skills array")
            .iter()
            .map(|skill| {
                skill["name"]
                    .as_str()
                    .expect("listed skill name")
                    .to_string()
            })
            .collect()
    }

    #[tokio::test]
    async fn lists_the_visible_catalog_and_loads_full_instructions() {
        let root = catalog_root();
        let runtime = SkillsRuntime::from_dir(root.path());

        let list = backend_list(&runtime, "worker").await;
        assert_eq!(listed_names(&list), ["open", "rust-review"]);
        assert_eq!(list["skills"][1]["description"], "Review Rust changes");

        let backend = runtime.backend(Arc::new(EchoBackend));
        let loaded = backend
            .execute(
                LOAD_SKILL,
                &json!({ "name": "rust-review", TRUSTED_ROLE_KEY: "worker" }),
            )
            .await
            .expect("visible skill loads");
        assert_eq!(loaded["name"], "rust-review");
        assert_eq!(loaded["description"], "Review Rust changes");
        assert_eq!(loaded["instructions"], "Read the diff carefully.");
    }

    #[tokio::test]
    async fn visibility_matches_worker_reporter_specialist_and_none() {
        let root = catalog_root();
        let runtime = SkillsRuntime::from_dir(root.path());

        assert_eq!(
            listed_names(&backend_list(&runtime, "worker").await),
            ["open", "rust-review"]
        );
        assert_eq!(
            listed_names(&backend_list(&runtime, "reporter").await),
            ["open", "reporter-notes"]
        );
        assert_eq!(
            listed_names(&backend_list(&runtime, "specialist:researcher").await),
            ["open", "rust-review"]
        );
        assert_eq!(
            listed_names(&backend_list(&runtime, "specialist:other").await),
            ["open"]
        );
        assert!(listed_names(&backend_list(&runtime, "intern").await).is_empty());

        let backend = runtime.backend(Arc::new(EchoBackend));
        for (role, skill) in [
            ("reporter", "rust-review"),
            ("specialist:other", "rust-review"),
            ("worker", "reporter-notes"),
            ("worker", "hidden"),
            ("intern", "open"),
        ] {
            let error = backend
                .execute(
                    LOAD_SKILL,
                    &json!({ "name": skill, TRUSTED_ROLE_KEY: role }),
                )
                .await
                .expect_err("invisible skills do not load");
            assert!(
                error.to_string().contains("not visible"),
                "{role:?} must not see {skill:?}: {error}"
            );
        }
    }

    #[tokio::test]
    async fn unknown_skills_are_errors() {
        let root = catalog_root();
        let runtime = SkillsRuntime::from_dir(root.path());
        let backend = runtime.backend(Arc::new(EchoBackend));
        let error = backend
            .execute(
                LOAD_SKILL,
                &json!({ "name": "missing", TRUSTED_ROLE_KEY: "worker" }),
            )
            .await
            .expect_err("an unknown skill is an error");
        assert!(error.to_string().contains("unknown skill"));
    }

    #[tokio::test]
    async fn trusted_role_overwrites_a_model_supplied_role() {
        let sink = Arc::new(RecordingSink::allows(json!({ "ok": true })));
        let tool = load_skill_tool(sink.clone()).expect("load_skill builds");

        let task_id = Uuid::new_v4();
        let intent_id = Uuid::new_v4();
        let mut context = ToolContext::new();
        context
            .insert(RunScope { task_id, intent_id })
            .expect("scope serializes");
        context.insert(RunRole::worker()).expect("role serializes");

        tool.run_with(
            &mut context,
            json!({ "name": "rust-review", TRUSTED_ROLE_KEY: "reporter" }),
        )
        .await
        .expect("the sink allows the call");

        let calls = sink.calls();
        assert_eq!(calls.len(), 1);
        let (seen_task, seen_intent, request) = &calls[0];
        assert_eq!((*seen_task, *seen_intent), (task_id, intent_id));
        assert_eq!(request.tool, LOAD_SKILL);
        assert_eq!(request.input["name"], "rust-review");
        assert_eq!(
            request.input[TRUSTED_ROLE_KEY], "worker",
            "the dispatch role wins over the model's value"
        );
    }

    #[tokio::test]
    async fn a_dispatch_without_a_role_cannot_reach_the_sink() {
        let sink = Arc::new(RecordingSink::allows(json!({})));
        let tool = load_skill_tool(sink.clone()).expect("load_skill builds");
        let mut context = ToolContext::new();
        context
            .insert(RunScope {
                task_id: Uuid::new_v4(),
                intent_id: Uuid::new_v4(),
            })
            .expect("scope serializes");

        let error = tool
            .run_with(&mut context, json!({ "name": "rust-review" }))
            .await
            .expect_err("a missing trusted role is an internal error");
        assert!(error.to_string().contains("run role"));
        assert!(sink.calls().is_empty());
    }

    #[tokio::test]
    async fn registration_installs_one_guarded_tool_and_routes_other_names() {
        let root = catalog_root();
        let runtime = SkillsRuntime::from_dir(root.path());
        let handle = ToolServer::new().run();
        let sink = Arc::new(RecordingSink::allows(json!({ "ok": true })));

        let registered = runtime
            .register(&ToolSourceContext {
                handle: &handle,
                sink: sink.clone(),
            })
            .expect("registration succeeds");
        assert_eq!(registered, [LOAD_SKILL]);
        let snapshot = handle.snapshot();
        assert_eq!(
            snapshot.names().collect::<Vec<_>>(),
            [LOAD_SKILL],
            "registration exposes only the guarded meta-tool"
        );
        assert!(snapshot.contains(LOAD_SKILL));
        let definition = snapshot
            .definitions()
            .iter()
            .find(|definition| definition.name.as_str() == LOAD_SKILL)
            .expect("definition");
        let schema = &definition.parameters;
        let properties = schema["properties"].as_object().expect("properties");
        assert_eq!(
            properties.keys().map(String::as_str).collect::<Vec<_>>(),
            ["name"]
        );
        assert!(schema.get("required").is_none());
        assert_eq!(schema["additionalProperties"], false);

        let backend = runtime.backend(Arc::new(EchoBackend));
        assert_eq!(
            backend
                .execute("other", &json!({}))
                .await
                .expect("delegate"),
            json!({ "tool": "other" })
        );

        assert!(
            sink.calls().is_empty(),
            "registration alone must not call the sink"
        );
    }

    #[tokio::test]
    async fn an_existing_load_skill_is_logged_and_skipped_not_replaced() {
        let root = catalog_root();
        let runtime = SkillsRuntime::from_dir(root.path());
        let handle = ToolServer::new().run();
        handle
            .add_dynamic_tool(load_skill_tool(Arc::new(RecordingSink::allows(json!({})))).unwrap());
        let before = handle.snapshot().len();

        let registered = runtime
            .register(&ToolSourceContext {
                handle: &handle,
                sink: Arc::new(RecordingSink::allows(json!({}))),
            })
            .expect("an existing tool is not a registration failure");
        assert!(registered.is_empty());
        assert_eq!(handle.snapshot().len(), before);
    }

    #[tokio::test]
    async fn an_empty_catalog_registers_nothing_and_keeps_the_delegate() {
        let root = tempfile::tempdir().expect("temporary directory");
        let runtime = SkillsRuntime::from_dir(root.path());
        let handle = ToolServer::new().run();
        let sink = Arc::new(RecordingSink::allows(json!({})));

        let registered = runtime
            .register(&ToolSourceContext {
                handle: &handle,
                sink,
            })
            .expect("empty registration succeeds");
        assert!(registered.is_empty());
        assert!(handle.snapshot().is_empty());

        let backend = runtime.backend(Arc::new(EchoBackend));
        assert_eq!(
            backend
                .execute(LOAD_SKILL, &json!({}))
                .await
                .expect("delegate"),
            json!({ "tool": LOAD_SKILL })
        );
    }

    #[test]
    fn visibility_helpers_cover_every_recognized_role() {
        let catalog = SkillCatalog::from_dir(catalog_root().path());
        let open = catalog.get("open").expect("open skill");
        for role in [
            "planner",
            "worker",
            "reporter",
            "digest",
            "specialist:researcher",
        ] {
            assert!(open.is_visible_to(role), "{role} sees an open skill");
        }
        assert!(!open.is_visible_to("intern"));
        assert!(!open.is_visible_to("specialist:"));
    }
}
