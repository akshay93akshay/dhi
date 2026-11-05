//! Gate A integration coverage: per-agent MCP visibility and deferred
//! schemas through the public `dhi_server::mcp` surface.
//!
//! These tests never connect a server: the planner consumes the same
//! `RegistryToolSnapshot` rows the manager produces after discovery, and the
//! meta-tools are exercised on a real Rig `ToolServer` with a fake guarded
//! sink. That proves the wiring point the host uses without a network.

use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use dhi_agent::{RunRole, RunScope, ToolSink};
use dhi_core::{ToolCallOutcome as CoreOutcome, ToolCallRequest};
use dhi_mcp_upgrade::{CALL_TOOL_NAME, LIST_TOOLS_NAME};
use dhi_server::mcp::{
    McpRolePlanner, McpVisibilityConfig, McpWorkerToolView, RegistryToolSnapshot, RolePlan,
    register_deferred_meta_tools,
};
use rig::tool::server::{ToolServer, ToolServerHandle};
use rig::tool::{ToolContext, ToolResult};
use serde_json::{Value, json};
use uuid::Uuid;

fn snapshot(server: &str, tool: &str) -> RegistryToolSnapshot {
    RegistryToolSnapshot {
        guarded_name: dhi_mcp::guarded_tool_name(server, tool).expect("a guardable test name"),
        mcp_name: tool.to_string(),
        owner: server.to_string(),
        description: format!("{tool} on {server}"),
        schema: json!({"type": "object", "properties": {}}),
    }
}

fn snapshot_many(server: &str, tools: &[&str]) -> Vec<RegistryToolSnapshot> {
    tools.iter().map(|tool| snapshot(server, tool)).collect()
}

fn config(servers: Value) -> McpVisibilityConfig {
    let file: dhi_mcp::McpFile =
        serde_json::from_value(json!({ "mcpServers": servers })).expect("config parses");
    McpVisibilityConfig::from_file(&file)
}

fn direct_names(plan: &RolePlan) -> Vec<String> {
    plan.direct
        .iter()
        .map(|tool| tool.guarded_name.clone())
        .collect()
}

fn tool_names(plan: &RolePlan) -> Vec<String> {
    plan.direct.iter().map(|tool| tool.tool.clone()).collect()
}

#[test]
fn worker_only_reporter_only_both_none_and_specialist_visibility() {
    let snapshot = [
        snapshot("worker-only", "work"),
        snapshot("reporter-only", "report"),
        snapshot("shared", "ping"),
        snapshot("nobody", "secret"),
        snapshot("research", "search"),
    ];
    let planner = McpRolePlanner::new(
        &config(json!({
            "worker-only": { "command": "npx", "visible_to": ["worker"] },
            "reporter-only": { "command": "npx", "visible_to": ["reporter"] },
            "shared": { "command": "npx", "visible_to": ["worker", "reporter"] },
            "nobody": { "command": "npx", "visible_to": [] },
            "research": { "command": "npx", "visible_to": ["specialist:researcher"] }
        })),
        &snapshot,
    );

    assert_eq!(
        direct_names(&planner.plan("worker")),
        ["mcp__shared__ping", "mcp__worker-only__work"]
    );
    assert_eq!(
        direct_names(&planner.plan("reporter")),
        ["mcp__reporter-only__report", "mcp__shared__ping"]
    );
    assert!(planner.plan("planner").direct.is_empty());
    assert!(planner.plan("digest").direct.is_empty());
    assert_eq!(
        direct_names(&planner.plan("specialist:researcher")),
        ["mcp__research__search"]
    );
    assert!(
        planner
            .plan("specialist:legal")
            .direct
            .iter()
            .all(|tool| tool.server != "research")
    );
    for role in [
        "planner",
        "worker",
        "reporter",
        "digest",
        "specialist:researcher",
    ] {
        assert!(
            planner
                .plan(role)
                .direct
                .iter()
                .all(|tool| tool.server != "nobody"),
            "{role} must not see an explicit empty visibility list"
        );
    }
}

#[test]
fn no_config_default_stays_visible_and_unknown_roles_fail_closed() {
    let planner = McpRolePlanner::new(
        &McpVisibilityConfig::default(),
        &[snapshot("plain", "fetch")],
    );
    for role in [
        "planner",
        "worker",
        "reporter",
        "digest",
        "specialist:any-name",
    ] {
        assert_eq!(
            direct_names(&planner.plan(role)),
            ["mcp__plain__fetch"],
            "{role}"
        );
    }
    for role in ["", "intern", "specialist:"] {
        let plan = planner.plan(role);
        assert!(plan.direct.is_empty(), "{role:?}");
        assert!(plan.deferred.is_empty(), "{role:?}");
    }
}

#[test]
fn per_tool_override_and_specialist_filtering() {
    let snapshot = [
        snapshot("mixed", "worker-tool"),
        snapshot("mixed", "planner-tool"),
        snapshot("mixed", "hidden-tool"),
    ];
    let planner = McpRolePlanner::new(
        &config(json!({
            "mixed": {
                "command": "npx",
                "visible_to": ["worker"],
                "tool_visibility": {
                    "planner-tool": ["planner"],
                    "hidden-tool": []
                }
            }
        })),
        &snapshot,
    );
    assert_eq!(tool_names(&planner.plan("worker")), ["worker-tool"]);
    assert_eq!(tool_names(&planner.plan("planner")), ["planner-tool"]);
    assert!(planner.plan("reporter").direct.is_empty());

    let mut spec = vec![
        "mcp__mixed__worker-tool".to_string(),
        "mcp__mixed__planner-tool".to_string(),
        "web_search".to_string(),
    ];
    let removed = planner.filter_specialist_tools("someone", &mut spec);
    assert_eq!(removed, 2, "hidden MCP names are removed");
    assert_eq!(spec, ["web_search"], "non-MCP tools are untouched");
}

#[test]
fn a_forty_tool_server_is_direct_below_the_threshold_and_deferred_above_it() {
    let tools: Vec<String> = (0..40).map(|index| format!("tool_{index:02}")).collect();
    let refs: Vec<&str> = tools.iter().map(String::as_str).collect();
    let snapshot = snapshot_many("big", &refs);

    let below = McpRolePlanner::new(
        &config(json!({"big": {"command": "npx", "deferred_threshold": 41}})),
        &snapshot,
    )
    .plan("worker");
    assert!(!below.is_deferred());
    assert_eq!(below.definitions().len(), 40);
    assert_eq!(below.definitions()[0].name, "mcp__big__tool_00");

    let above = McpRolePlanner::new(
        &config(json!({"big": {"command": "npx", "deferred_threshold": 32}})),
        &snapshot,
    )
    .plan("worker");
    assert!(above.is_deferred());
    assert_eq!(
        above
            .definitions()
            .iter()
            .map(|definition| definition.name.as_str())
            .collect::<Vec<_>>(),
        [LIST_TOOLS_NAME, CALL_TOOL_NAME]
    );
    assert_eq!(above.deferred_tool_count(), 40);
    assert_eq!(above.list()[0].tools.len(), 40);
    assert_eq!(
        above
            .call_tool("big", "tool_39", json!({"n": 39}))
            .expect("visible tool resolves")
            .guarded_name,
        "mcp__big__tool_39"
    );
}

#[test]
fn unknown_hidden_and_oversized_meta_calls_fail_closed() {
    let tools: Vec<String> = (0..10).map(|index| format!("tool_{index}")).collect();
    let refs: Vec<&str> = tools.iter().map(String::as_str).collect();
    let planner = McpRolePlanner::new(
        &config(json!({
            "big": {
                "command": "npx",
                "deferred_threshold": 5,
                "tool_visibility": { "tool_4": ["planner"] }
            }
        })),
        &snapshot_many("big", &refs),
    );
    let plan = planner.plan("worker");
    assert!(plan.is_deferred());

    assert!(plan.call_tool("missing", "tool_0", json!({})).is_err());
    assert!(plan.call_tool("big", "missing", json!({})).is_err());
    assert!(
        plan.call_tool("big", "tool_4", json!({})).is_err(),
        "a hidden per-tool override is indistinguishable from an unknown tool"
    );
    assert!(
        plan.call_tool("big", "tool_0", json!({"blob": "x".repeat(70 * 1024)}))
            .is_err(),
        "the 64 KiB argument cap is enforced"
    );
}

// -- worker view + meta-tool dispatch through a fake guard -----------------

#[derive(Default)]
struct FakeSink {
    calls: Mutex<Vec<(Uuid, Uuid, ToolCallRequest)>>,
    outcomes: Mutex<Vec<CoreOutcome>>,
}

impl FakeSink {
    fn with_outcomes(outcomes: Vec<CoreOutcome>) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            outcomes: Mutex::new(outcomes),
        })
    }

    fn calls(&self) -> Vec<(Uuid, Uuid, ToolCallRequest)> {
        self.calls.lock().expect("calls lock").clone()
    }
}

#[async_trait]
impl ToolSink for FakeSink {
    async fn execute(
        &self,
        task_id: Uuid,
        intent_id: Uuid,
        request: ToolCallRequest,
    ) -> anyhow::Result<CoreOutcome> {
        self.calls
            .lock()
            .expect("calls lock")
            .push((task_id, intent_id, request));
        let mut outcomes = self.outcomes.lock().expect("outcomes lock");
        if outcomes.is_empty() {
            Ok(CoreOutcome::Allowed {
                output: json!({"ok": true}),
            })
        } else {
            Ok(outcomes.remove(0))
        }
    }
}

fn register_dummy(handle: &ToolServerHandle, name: &str, sink: Arc<dyn ToolSink>) {
    dhi_agent::register_guarded_tool(
        handle,
        dhi_agent::GuardedToolSpec::new(name, "test tool", json!({"type": "object"})),
        sink,
    )
    .expect("dummy tool registers");
}

async fn call_meta(handle: &ToolServerHandle, name: &str, input: Value, role: &str) -> ToolResult {
    let toolset = handle.toolset();
    let tool = toolset.get(name).expect("meta-tool exists");
    let mut context = ToolContext::new();
    context
        .insert(RunScope {
            task_id: Uuid::from_u128(1),
            intent_id: Uuid::from_u128(2),
        })
        .expect("scope");
    context.insert(RunRole(role.to_string())).expect("role");
    tool.execute(input.to_string(), &mut context).await
}

#[tokio::test]
async fn the_worker_view_advertises_only_visible_mcp_names_and_keeps_other_sources() {
    let handle = ToolServer::new().run();
    let sink: Arc<dyn ToolSink> = FakeSink::with_outcomes(Vec::new());
    register_dummy(&handle, "mcp__worker-only__work", Arc::clone(&sink));
    register_dummy(&handle, "mcp__reporter-only__report", Arc::clone(&sink));
    register_dummy(&handle, "record_fact", Arc::clone(&sink));

    let planner = McpRolePlanner::new(
        &config(json!({
            "worker-only": { "command": "npx", "visible_to": ["worker"] },
            "reporter-only": { "command": "npx", "visible_to": ["reporter"] }
        })),
        &[
            snapshot("worker-only", "work"),
            snapshot("reporter-only", "report"),
        ],
    );
    let view = McpWorkerToolView::new(&handle, Arc::new(RwLock::new(planner)));
    let mut names = view.visible_names();
    names.sort();
    assert_eq!(names, ["mcp__worker-only__work", "record_fact"]);
}

#[tokio::test]
async fn mcp_call_tool_routes_the_real_guarded_name_through_the_sink() {
    let tools: Vec<String> = (0..40).map(|index| format!("tool_{index:02}")).collect();
    let refs: Vec<&str> = tools.iter().map(String::as_str).collect();
    let planner = Arc::new(RwLock::new(McpRolePlanner::new(
        &McpVisibilityConfig::default(),
        &snapshot_many("big", &refs),
    )));
    assert!(
        planner.read().expect("planner lock").may_defer(),
        "40 tools cross the default threshold"
    );

    let sink = FakeSink::with_outcomes(vec![
        CoreOutcome::Allowed {
            output: json!({"echo": "real"}),
        },
        CoreOutcome::Denied {
            reason: "rule deny-mcp: blocked".to_string(),
        },
    ]);
    let handle = ToolServer::new().run();
    let sink_trait: Arc<dyn ToolSink> = sink.clone();
    register_deferred_meta_tools(&handle, Arc::clone(&planner), sink_trait)
        .expect("meta-tools register");

    let result = call_meta(
        &handle,
        CALL_TOOL_NAME,
        json!({"server": "big", "tool": "tool_07", "arguments": {"q": 7}}),
        "worker",
    )
    .await;
    assert!(result.is_success(), "{result:?}");
    assert_eq!(result.output().as_json(), Some(&json!({"echo": "real"})));

    let calls = sink.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, Uuid::from_u128(1));
    assert_eq!(calls[0].1, Uuid::from_u128(2));
    assert_eq!(
        calls[0].2.tool, "mcp__big__tool_07",
        "the audit row names the real guarded tool, not mcp_call_tool"
    );
    assert_eq!(calls[0].2.input["q"], 7);

    let listing = call_meta(&handle, LIST_TOOLS_NAME, json!({}), "worker").await;
    assert!(listing.is_success(), "{listing:?}");
    let output = listing.output().as_json().expect("structured output");
    assert_eq!(output["servers"][0]["server"], "big");
    assert_eq!(output["servers"][0]["tools"].as_array().unwrap().len(), 40);

    let denied = call_meta(
        &handle,
        CALL_TOOL_NAME,
        json!({"server": "big", "tool": "tool_08", "arguments": {}}),
        "worker",
    )
    .await;
    assert!(
        denied.is_error_kind(rig::tool::ToolErrorKind::PermissionDenied),
        "the guard's denial reaches the model: {denied:?}"
    );
    assert_eq!(sink.calls().len(), 2);
}
