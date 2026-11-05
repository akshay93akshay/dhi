//! Deferred MCP schemas and the two meta-tools.
//!
//! Below the policy threshold every visible tool is registered directly under
//! its guarded name. At or above it the model sees only:
//!
//! * `mcp_list_tools` — visible server/tool names with short descriptions;
//! * `mcp_call_tool` — `{ server, tool, arguments }`.
//!
//! A successful `mcp_call_tool` resolves to a [`GuardedCall`] whose
//! `guarded_name` is the real `mcp__<server>__<tool>` name, so the guard and
//! audit see the actual tool rather than the meta-tool.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use thiserror::Error;

use crate::catalog::{ServerCatalog, ToolDefinition};
use crate::visibility::{VisibleTool, visible_tools};

/// The name of the listing meta-tool.
pub const LIST_TOOLS_NAME: &str = "mcp_list_tools";
/// The name of the calling meta-tool.
pub const CALL_TOOL_NAME: &str = "mcp_call_tool";
/// The largest serialized argument object accepted by `mcp_call_tool`.
pub const DEFAULT_MAX_ARGUMENT_BYTES: usize = 64 * 1024;
/// The longest server or tool name accepted in a deferred call.
pub const MAX_LOOKUP_NAME_CHARS: usize = 256;
/// Descriptions returned by `mcp_list_tools` are capped at this many chars.
pub const SHORT_DESCRIPTION_CHARS: usize = 160;

/// When to stop registering tools directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeferredPolicy {
    /// At or above this many visible tools, expose the two meta-tools.
    pub threshold: usize,
}

impl DeferredPolicy {
    /// A policy with the given threshold.
    pub const fn new(threshold: usize) -> Self {
        Self { threshold }
    }

    /// True when `tool_count` reaches the threshold.
    pub const fn is_deferred(self, tool_count: usize) -> bool {
        tool_count >= self.threshold
    }
}

impl Default for DeferredPolicy {
    fn default() -> Self {
        Self { threshold: 20 }
    }
}

/// A tool registered directly under its guarded name.
#[derive(Debug, Clone, PartialEq)]
pub struct DirectTool {
    pub server: String,
    pub tool: String,
    /// The model-visible name, `mcp__<server>__<tool>`.
    pub guarded_name: String,
    pub description: String,
    pub input_schema: Value,
}

impl DirectTool {
    /// The model-visible definition.
    pub fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.guarded_name.clone(),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
        }
    }
}

/// The result of planning exposure for one agent role.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolPlan {
    /// Register these guarded tools directly. Empty means expose nothing.
    Direct(Vec<DirectTool>),
    /// Expose only the two meta-tools. Calls are resolved through
    /// [`DeferredToolset`].
    Deferred(DeferredToolset),
}

impl ToolPlan {
    /// Every model-visible tool definition: direct tools, or the two meta
    /// tools.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        match self {
            Self::Direct(tools) => tools.iter().map(DirectTool::definition).collect(),
            Self::Deferred(toolset) => toolset.definitions(),
        }
    }

    /// True when the meta-tools are exposed.
    pub fn is_deferred(&self) -> bool {
        matches!(self, Self::Deferred(_))
    }

    /// The directly registered tools, when this plan is direct.
    pub fn direct_tools(&self) -> Option<&[DirectTool]> {
        match self {
            Self::Direct(tools) => Some(tools),
            Self::Deferred(_) => None,
        }
    }

    /// The deferred toolset, when this plan is deferred.
    pub fn deferred(&self) -> Option<&DeferredToolset> {
        match self {
            Self::Direct(_) => None,
            Self::Deferred(toolset) => Some(toolset),
        }
    }
}

/// One server in `mcp_list_tools` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerToolListing {
    pub server: String,
    pub tools: Vec<ToolSummary>,
}

/// One tool in `mcp_list_tools` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSummary {
    pub name: String,
    pub description: String,
}

/// A resolved `mcp_call_tool` request.
#[derive(Debug, Clone, PartialEq)]
pub struct GuardedCall {
    /// The real guarded name the guard/audit must see.
    pub guarded_name: String,
    pub server: String,
    pub tool: String,
    pub arguments: Map<String, Value>,
}

/// Trusted execution scope supplied by the caller, never by the model. The
/// server must pass the task and intent ids it already uses for the guard's
/// audit trail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardScope {
    pub task_id: String,
    pub intent_id: String,
}

impl GuardScope {
    pub fn new(task_id: impl Into<String>, intent_id: impl Into<String>) -> Self {
        Self {
            task_id: task_id.into(),
            intent_id: intent_id.into(),
        }
    }
}

/// What happened when a deferred call reached the guard.
///
/// `Denied` and `Pending` are first-class results: the caller must be able to
/// ask for approval rather than having a pending call collapsed into an
/// error. `TransportError` keeps backend failure distinct from policy.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolCallOutcome {
    Allowed { output: Value },
    Denied { reason: String },
    Pending { approval_id: String },
    TransportError { message: String },
}

/// The two meta-tools plus the visible tool set they resolve against.
#[derive(Debug, Clone, PartialEq)]
pub struct DeferredToolset {
    policy: DeferredPolicy,
    tools: Vec<VisibleTool>,
}

impl DeferredToolset {
    /// Builds a deferred toolset from an already visibility-filtered list.
    /// Crate-private so every constructed toolset came through [`plan_tools`]
    /// and therefore passed guarded-name collision checks.
    pub(crate) fn new(tools: Vec<VisibleTool>, policy: DeferredPolicy) -> Self {
        Self { policy, tools }
    }

    /// The visible tools backing this toolset.
    pub fn tools(&self) -> &[VisibleTool] {
        &self.tools
    }

    /// The policy that produced this toolset.
    pub fn policy(&self) -> DeferredPolicy {
        self.policy
    }

    /// The `mcp_list_tools` definition.
    pub fn list_tool_definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: LIST_TOOLS_NAME.to_string(),
            description: "List the MCP servers and tools this agent can call. \
                          Returns server and tool names with short descriptions."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    /// The `mcp_call_tool` definition.
    pub fn call_tool_definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: CALL_TOOL_NAME.to_string(),
            description: "Call one MCP tool by server and tool name. The call is \
                          routed to the real guarded tool so policy and audit see it."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string", "minLength": 1 },
                    "tool": { "type": "string", "minLength": 1 },
                    "arguments": { "type": "object" }
                },
                "required": ["server", "tool"],
                "additionalProperties": false
            }),
        }
    }

    /// The two meta-tool definitions.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        vec![self.list_tool_definition(), self.call_tool_definition()]
    }

    /// The visible catalog as `mcp_list_tools` reports it. Servers keep the
    /// order they first appear in; descriptions are collapsed and capped.
    pub fn list(&self) -> Vec<ServerToolListing> {
        let mut listings: Vec<ServerToolListing> = Vec::new();
        for tool in &self.tools {
            let summary = ToolSummary {
                name: tool.tool.clone(),
                description: short_description(&tool.description),
            };
            if let Some(listing) = listings
                .iter_mut()
                .find(|listing| listing.server == tool.server)
            {
                listing.tools.push(summary);
            } else {
                listings.push(ServerToolListing {
                    server: tool.server.clone(),
                    tools: vec![summary],
                });
            }
        }
        listings
    }

    /// Resolves one meta-tool call to the real guarded name.
    ///
    /// Unknown servers and tools fail closed. Tools hidden by visibility are
    /// indistinguishable from tools that do not exist.
    pub fn call_tool(
        &self,
        server: &str,
        tool: &str,
        arguments: Value,
    ) -> Result<GuardedCall, DeferredError> {
        validate_lookup_name("server", server)?;
        validate_lookup_name("tool", tool)?;

        let Value::Object(arguments) = arguments else {
            return Err(DeferredError::ArgumentsNotObject);
        };
        let size = Value::Object(arguments.clone()).to_string().len();
        if size > DEFAULT_MAX_ARGUMENT_BYTES {
            return Err(DeferredError::ArgumentsTooLarge {
                size,
                max: DEFAULT_MAX_ARGUMENT_BYTES,
            });
        }

        if !self.tools.iter().any(|entry| entry.server == server) {
            return Err(DeferredError::UnknownServer(server.to_string()));
        }
        let entry = self
            .tools
            .iter()
            .find(|entry| entry.server == server && entry.tool == tool)
            .ok_or_else(|| DeferredError::UnknownTool {
                server: server.to_string(),
                tool: tool.to_string(),
            })?;

        Ok(GuardedCall {
            guarded_name: entry.guarded_name.clone(),
            server: server.to_string(),
            tool: tool.to_string(),
            arguments,
        })
    }

    /// Parses a raw `mcp_call_tool` input object and resolves it.
    pub fn call_from_input(&self, input: &Value) -> Result<GuardedCall, DeferredError> {
        let input: CallToolInput = serde_json::from_value(input.clone())
            .map_err(|error| DeferredError::InvalidInput(error.to_string()))?;
        self.call_tool(&input.server, &input.tool, input.arguments)
    }

    /// Resolves `input` and hands the real guarded name and trusted scope to
    /// `invoker`. Resolution errors are returned as [`DeferredError`]; a
    /// backend failure is represented as [`ToolCallOutcome::TransportError`]
    /// so callers never confuse it with a policy decision.
    pub async fn execute(
        &self,
        scope: &GuardScope,
        input: &Value,
        invoker: &dyn GuardedInvoker,
    ) -> Result<ToolCallOutcome, DeferredError> {
        let call = self.call_from_input(input)?;
        match invoker
            .invoke(scope, &call.guarded_name, call.arguments)
            .await
        {
            Ok(outcome) => Ok(outcome),
            Err(error) => Ok(ToolCallOutcome::TransportError {
                message: error.to_string(),
            }),
        }
    }
}

/// The next step after resolving a deferred call: run the real guarded tool.
///
/// Integration MUST route this through the existing `ToolSink`/guard with the
/// supplied [`GuardScope`] and the real `guarded_name`; it must not call the
/// server backend directly and must not record `mcp_call_tool` in the audit
/// trail. The guard/audit record is for `mcp__<server>__<tool>`.
#[async_trait]
pub trait GuardedInvoker: Send + Sync {
    /// Runs `guarded_name` with `arguments`, exactly as the guard would,
    /// returning the guard's policy outcome.
    async fn invoke(
        &self,
        scope: &GuardScope,
        guarded_name: &str,
        arguments: Map<String, Value>,
    ) -> anyhow::Result<ToolCallOutcome>;
}

/// Plans direct registration or deferred meta-tools for `role`.
///
/// Unknown roles and catalogs with no visible tools produce an empty direct
/// plan, so no meta-tools are exposed when there is nothing to list. A single
/// unguardable tool is skipped by visibility rather than failing the role;
/// two different tools that collide on the same guarded name remain a hard
/// planning error because routing them would be ambiguous.
pub fn plan_tools(
    role: &str,
    servers: &[ServerCatalog],
    policy: DeferredPolicy,
) -> Result<ToolPlan, DeferredError> {
    let visible = visible_tools(role, servers);
    if visible.is_empty() {
        return Ok(ToolPlan::Direct(Vec::new()));
    }
    // Both registration modes ultimately address tools by guarded name, so a
    // collision is rejected before either plan is produced.
    validate_guarded_names(&visible)?;
    if !policy.is_deferred(visible.len()) {
        return Ok(ToolPlan::Direct(direct_plan(visible)));
    }
    Ok(ToolPlan::Deferred(DeferredToolset::new(visible, policy)))
}

fn validate_guarded_names(tools: &[VisibleTool]) -> Result<(), DeferredError> {
    let mut guarded: BTreeMap<String, (String, String)> = BTreeMap::new();
    for tool in tools {
        if let Some((first_server, first_tool)) = guarded.get(&tool.guarded_name) {
            return Err(DeferredError::DuplicateGuardedName {
                name: tool.guarded_name.clone(),
                first_server: first_server.clone(),
                first_tool: first_tool.clone(),
                second_server: tool.server.clone(),
                second_tool: tool.tool.clone(),
            });
        }
        guarded.insert(
            tool.guarded_name.clone(),
            (tool.server.clone(), tool.tool.clone()),
        );
    }
    Ok(())
}

fn direct_plan(tools: Vec<VisibleTool>) -> Vec<DirectTool> {
    tools
        .into_iter()
        .map(|tool| DirectTool {
            server: tool.server,
            tool: tool.tool,
            guarded_name: tool.guarded_name,
            description: tool.description,
            input_schema: tool.input_schema,
        })
        .collect()
}

fn validate_lookup_name(kind: &'static str, name: &str) -> Result<(), DeferredError> {
    let reason = if name.is_empty() {
        Some("must not be empty")
    } else if name.trim() != name {
        Some("must not have leading or trailing whitespace")
    } else if name.chars().any(char::is_control) {
        Some("must not contain control characters")
    } else if name.chars().count() > MAX_LOOKUP_NAME_CHARS {
        Some("is too long")
    } else {
        None
    };
    match reason {
        Some(reason) => Err(DeferredError::InvalidName {
            kind,
            name: name.to_string(),
            reason,
        }),
        None => Ok(()),
    }
}

fn short_description(description: &str) -> String {
    let collapsed = description.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= SHORT_DESCRIPTION_CHARS {
        return collapsed;
    }
    let mut short: String = collapsed
        .chars()
        .take(SHORT_DESCRIPTION_CHARS.saturating_sub(1))
        .collect();
    short.push('…');
    short
}

fn default_arguments() -> Value {
    Value::Object(Map::new())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CallToolInput {
    server: String,
    tool: String,
    #[serde(default = "default_arguments")]
    arguments: Value,
}

/// Why a deferred plan or call could not be produced.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DeferredError {
    #[error("invalid MCP {kind} name {name:?}: {reason}")]
    InvalidName {
        kind: &'static str,
        name: String,
        reason: &'static str,
    },
    #[error("unknown MCP server {0:?}")]
    UnknownServer(String),
    #[error("unknown MCP tool {tool:?} on server {server:?}")]
    UnknownTool { server: String, tool: String },
    #[error(
        "MCP tools {first_server:?}/{first_tool:?} and {second_server:?}/{second_tool:?} \
         share guarded name {name:?}"
    )]
    DuplicateGuardedName {
        name: String,
        first_server: String,
        first_tool: String,
        second_server: String,
        second_tool: String,
    },
    #[error("mcp_call_tool arguments must be a JSON object")]
    ArgumentsNotObject,
    #[error("mcp_call_tool arguments are {size} bytes, exceeding the {max}-byte limit")]
    ArgumentsTooLarge { size: usize, max: usize },
    #[error("invalid mcp_call_tool input: {0}")]
    InvalidInput(String),
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::catalog::CatalogTool;

    fn catalog_with(count: usize, visible_to: &[&str]) -> Vec<ServerCatalog> {
        let tools = (0..count)
            .map(|index| CatalogTool::new(format!("tool_{index:02}")))
            .collect();
        vec![ServerCatalog::new(
            "fake",
            visible_to.iter().map(|role| (*role).to_string()).collect(),
            tools,
        )]
    }

    fn deferred(plan: &ToolPlan) -> &DeferredToolset {
        plan.deferred().expect("plan is deferred")
    }

    #[test]
    fn below_threshold_registers_every_tool_directly() {
        let plan = plan_tools(
            "worker",
            &catalog_with(40, &["worker"]),
            DeferredPolicy::new(41),
        )
        .expect("plan");
        let direct = plan.direct_tools().expect("direct plan");
        assert_eq!(direct.len(), 40);
        assert!(!plan.is_deferred());
        assert_eq!(plan.definitions().len(), 40);
        assert_eq!(direct[0].guarded_name, "mcp__fake__tool_00");
        assert_eq!(direct[39].guarded_name, "mcp__fake__tool_39");
    }

    #[test]
    fn at_or_above_threshold_exposes_only_two_meta_tools() {
        for threshold in [20, 40] {
            let plan = plan_tools(
                "worker",
                &catalog_with(40, &["worker"]),
                DeferredPolicy::new(threshold),
            )
            .expect("plan");
            let definitions = plan.definitions();
            assert_eq!(definitions.len(), 2, "threshold {threshold}");
            assert_eq!(definitions[0].name, LIST_TOOLS_NAME);
            assert_eq!(definitions[1].name, CALL_TOOL_NAME);
            assert_eq!(deferred(&plan).tools().len(), 40);
        }
    }

    #[test]
    fn call_maps_to_the_real_guarded_name() {
        let plan = plan_tools(
            "worker",
            &catalog_with(40, &["worker"]),
            DeferredPolicy::new(10),
        )
        .expect("plan");
        let call = deferred(&plan)
            .call_tool("fake", "tool_07", json!({"query": "x"}))
            .expect("call resolves");
        assert_eq!(call.guarded_name, "mcp__fake__tool_07");
        assert_eq!(call.server, "fake");
        assert_eq!(call.tool, "tool_07");
        assert_eq!(call.arguments["query"], "x");
    }

    enum FakeResult {
        Allowed(Value),
        Denied(&'static str),
        Pending(&'static str),
        Transport(&'static str),
    }

    struct FixedInvoker {
        seen: Mutex<Vec<(GuardScope, String)>>,
        result: FakeResult,
    }

    impl FixedInvoker {
        fn new(result: FakeResult) -> Self {
            Self {
                seen: Mutex::new(Vec::new()),
                result,
            }
        }
    }

    #[async_trait]
    impl GuardedInvoker for FixedInvoker {
        async fn invoke(
            &self,
            scope: &GuardScope,
            guarded_name: &str,
            _arguments: Map<String, Value>,
        ) -> anyhow::Result<ToolCallOutcome> {
            self.seen
                .lock()
                .expect("lock")
                .push((scope.clone(), guarded_name.to_string()));
            match &self.result {
                FakeResult::Allowed(output) => Ok(ToolCallOutcome::Allowed {
                    output: output.clone(),
                }),
                FakeResult::Denied(reason) => Ok(ToolCallOutcome::Denied {
                    reason: (*reason).to_string(),
                }),
                FakeResult::Pending(approval_id) => Ok(ToolCallOutcome::Pending {
                    approval_id: (*approval_id).to_string(),
                }),
                FakeResult::Transport(message) => Err(anyhow::anyhow!((*message).to_string())),
            }
        }
    }

    fn execute_with(result: FakeResult) -> (ToolCallOutcome, Vec<(GuardScope, String)>) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let plan = plan_tools(
                "worker",
                &catalog_with(40, &["worker"]),
                DeferredPolicy::new(10),
            )
            .expect("plan");
            let toolset = deferred(&plan);
            let invoker = FixedInvoker::new(result);
            let scope = GuardScope::new("task-1", "intent-2");
            let outcome = toolset
                .execute(
                    &scope,
                    &json!({
                        "server": "fake",
                        "tool": "tool_03",
                        "arguments": {"n": 3}
                    }),
                    &invoker,
                )
                .await
                .expect("execute");
            (outcome, invoker.seen.into_inner().expect("lock"))
        })
    }

    #[test]
    fn guard_outcomes_are_preserved_and_scoped() {
        let (allowed, seen) = execute_with(FakeResult::Allowed(json!({"arguments": {"n": 3}})));
        assert_eq!(
            allowed,
            ToolCallOutcome::Allowed {
                output: json!({"arguments": {"n": 3}})
            }
        );
        assert_eq!(
            seen,
            [(
                GuardScope::new("task-1", "intent-2"),
                "mcp__fake__tool_03".to_string()
            )]
        );

        let (denied, seen) = execute_with(FakeResult::Denied("policy says no"));
        assert_eq!(
            denied,
            ToolCallOutcome::Denied {
                reason: "policy says no".to_string()
            }
        );
        assert_eq!(seen[0].1, "mcp__fake__tool_03");

        let (pending, seen) = execute_with(FakeResult::Pending("approval-9"));
        assert_eq!(
            pending,
            ToolCallOutcome::Pending {
                approval_id: "approval-9".to_string()
            }
        );
        assert_eq!(seen[0].0, GuardScope::new("task-1", "intent-2"));

        let (transport, seen) = execute_with(FakeResult::Transport("connection reset"));
        assert_eq!(
            transport,
            ToolCallOutcome::TransportError {
                message: "connection reset".to_string()
            }
        );
        assert_eq!(seen[0].1, "mcp__fake__tool_03");
    }

    #[test]
    fn unknown_server_and_tool_fail_closed() {
        let plan = plan_tools(
            "worker",
            &catalog_with(40, &["worker"]),
            DeferredPolicy::new(10),
        )
        .expect("plan");
        let toolset = deferred(&plan);

        assert_eq!(
            toolset
                .call_tool("missing", "tool_00", json!({}))
                .expect_err("unknown server"),
            DeferredError::UnknownServer("missing".to_string())
        );
        assert_eq!(
            toolset
                .call_tool("fake", "missing", json!({}))
                .expect_err("unknown tool"),
            DeferredError::UnknownTool {
                server: "fake".to_string(),
                tool: "missing".to_string()
            }
        );
        assert!(matches!(
            toolset.call_tool("", "tool_00", json!({})),
            Err(DeferredError::InvalidName { .. })
        ));
        assert!(matches!(
            toolset.call_tool("fake\n", "tool_00", json!({})),
            Err(DeferredError::InvalidName { .. })
        ));
    }

    #[test]
    fn visibility_is_respected_by_planning_and_calls() {
        let catalog = catalog_with(40, &["planner"]);

        let worker = plan_tools("worker", &catalog, DeferredPolicy::new(10)).expect("plan");
        assert!(worker.direct_tools().expect("empty direct").is_empty());
        assert_eq!(worker.definitions().len(), 0);

        let planner = plan_tools("planner", &catalog, DeferredPolicy::new(10)).expect("plan");
        let toolset = deferred(&planner);
        assert_eq!(toolset.tools().len(), 40);
        assert_eq!(toolset.list()[0].tools.len(), 40);
        assert_eq!(
            toolset
                .call_tool("fake", "tool_01", json!({}))
                .expect("visible call")
                .guarded_name,
            "mcp__fake__tool_01"
        );

        let unknown = plan_tools("intern", &catalog, DeferredPolicy::new(10)).expect("plan");
        assert_eq!(unknown.definitions().len(), 0);
    }

    #[test]
    fn hidden_per_tool_override_is_indistinguishable_from_unknown() {
        let catalog = vec![ServerCatalog::new(
            "fake",
            vec!["worker".to_string()],
            (0..10)
                .map(|index| {
                    let tool = CatalogTool::new(format!("tool_{index}"));
                    if index == 4 {
                        tool.with_visible_to(vec!["planner".to_string()])
                    } else {
                        tool
                    }
                })
                .collect(),
        )];
        let plan = plan_tools("worker", &catalog, DeferredPolicy::new(5)).expect("plan");
        let toolset = deferred(&plan);
        assert_eq!(toolset.tools().len(), 9);
        assert_eq!(
            toolset
                .call_tool("fake", "tool_4", json!({}))
                .expect_err("hidden tool"),
            DeferredError::UnknownTool {
                server: "fake".to_string(),
                tool: "tool_4".to_string()
            }
        );
    }

    #[test]
    fn argument_object_size_is_capped() {
        let plan = plan_tools(
            "worker",
            &catalog_with(40, &["worker"]),
            DeferredPolicy::new(10),
        )
        .expect("plan");
        let too_large = json!({ "blob": "x".repeat(DEFAULT_MAX_ARGUMENT_BYTES) });
        assert!(matches!(
            deferred(&plan).call_tool("fake", "tool_00", too_large),
            Err(DeferredError::ArgumentsTooLarge { .. })
        ));
        assert_eq!(
            deferred(&plan)
                .call_tool("fake", "tool_00", json!([]))
                .expect_err("array is not an object"),
            DeferredError::ArgumentsNotObject
        );
    }

    #[test]
    fn list_uses_short_descriptions_and_server_grouping() {
        let catalog = vec![
            ServerCatalog::new(
                "alpha",
                vec!["worker".to_string()],
                vec![
                    CatalogTool::new("one").with_description("  keep   it  short  "),
                    CatalogTool::new("two").with_description("d".repeat(500)),
                ],
            ),
            ServerCatalog::new(
                "beta",
                vec!["worker".to_string()],
                vec![CatalogTool::new("three")],
            ),
        ];
        let plan = plan_tools("worker", &catalog, DeferredPolicy::new(1)).expect("plan");
        let listing = deferred(&plan).list();
        assert_eq!(listing.len(), 2);
        assert_eq!(listing[0].server, "alpha");
        assert_eq!(listing[0].tools[0].description, "keep it short");
        assert_eq!(
            listing[0].tools[1].description.chars().count(),
            SHORT_DESCRIPTION_CHARS
        );
        assert_eq!(listing[1].server, "beta");
    }

    #[test]
    fn no_visible_tools_never_exposes_meta_tools() {
        for threshold in [0, 1] {
            let plan = plan_tools("worker", &[], DeferredPolicy::new(threshold)).expect("plan");
            assert_eq!(plan.definitions().len(), 0);
        }
    }

    #[test]
    fn unguardable_tools_are_skipped_not_fatal() {
        let mut tools: Vec<CatalogTool> = (0..40)
            .map(|index| CatalogTool::new(format!("tool_{index:02}")))
            .collect();
        tools.push(CatalogTool::new("@@@"));
        let catalog = vec![ServerCatalog::new(
            "fake",
            vec!["worker".to_string()],
            tools,
        )];

        let deferred_plan =
            plan_tools("worker", &catalog, DeferredPolicy::new(10)).expect("deferred plan");
        assert_eq!(deferred(&deferred_plan).tools().len(), 40);
        assert_eq!(deferred(&deferred_plan).list()[0].tools.len(), 40);

        let direct_plan =
            plan_tools("worker", &catalog, DeferredPolicy::new(100)).expect("direct plan");
        assert_eq!(direct_plan.definitions().len(), 40);
    }

    #[test]
    fn duplicate_guarded_names_are_rejected() {
        let catalog = vec![ServerCatalog::new(
            "fake",
            vec!["worker".to_string()],
            vec![CatalogTool::new("a b"), CatalogTool::new("a_b")],
        )];
        for threshold in [10, 1] {
            assert!(matches!(
                plan_tools("worker", &catalog, DeferredPolicy::new(threshold)),
                Err(DeferredError::DuplicateGuardedName { .. })
            ));
        }
    }
}
