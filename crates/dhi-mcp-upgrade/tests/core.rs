use dhi_mcp::McpTransport;
use dhi_mcp_upgrade::{
    CatalogTool, DeferredPolicy, ReloadAction, ServerCatalog, ServerSpec, ToolPlan, plan_reload,
    plan_tools, visible_tools,
};
use serde_json::json;

fn forty_tool_catalog() -> Vec<ServerCatalog> {
    vec![ServerCatalog::new(
        "fake",
        vec!["worker".to_string()],
        (0..40)
            .map(|index| CatalogTool::new(format!("tool_{index:02}")))
            .collect(),
    )]
}

#[test]
fn visibility_and_deferred_plans_work_through_the_public_api() {
    let catalog = forty_tool_catalog();
    assert_eq!(visible_tools("worker", &catalog).len(), 40);
    assert!(visible_tools("researcher", &catalog).is_empty());
    assert!(visible_tools("intern", &catalog).is_empty());

    let direct = plan_tools("worker", &catalog, DeferredPolicy::new(41)).expect("direct plan");
    assert!(matches!(direct, ToolPlan::Direct(_)));
    assert_eq!(direct.definitions().len(), 40);
    assert_eq!(
        direct.definitions()[0].name,
        "mcp__fake__tool_00".to_string()
    );

    let deferred = plan_tools("worker", &catalog, DeferredPolicy::new(20)).expect("deferred plan");
    let toolset = deferred.deferred().expect("meta-tools");
    assert_eq!(toolset.definitions().len(), 2);
    assert_eq!(
        toolset
            .call_tool("fake", "tool_39", json!({}))
            .expect("resolve")
            .guarded_name,
        "mcp__fake__tool_39"
    );
}

#[test]
fn reload_plan_works_through_the_public_api() {
    let old = vec![
        ServerSpec::new("a", McpTransport::Stdio, "one"),
        ServerSpec::new("b", McpTransport::Http, "two"),
    ];
    let new = vec![
        ServerSpec::new("b", McpTransport::Http, "changed"),
        ServerSpec::new("c", McpTransport::Stdio, "three"),
    ];
    let plan = plan_reload(&old, &new).expect("reload plan");
    assert!(matches!(
        plan.actions.as_slice(),
        [
            ReloadAction::Disconnect { name: a },
            ReloadAction::Reconnect { .. },
            ReloadAction::Connect { .. },
        ] if a == "a"
    ));
}
