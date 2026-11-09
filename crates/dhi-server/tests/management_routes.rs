//! Management route-level integration tests: the endpoints against a real
//! PostgreSQL store, including the shared-snapshot side effects (pool swap,
//! guard hot rebuild) and the pin-validation path of `POST /api/tasks`.
//!
//! `#[sqlx::test(migrations = "../dhi-store/migrations")]` applies the store's
//! migrations to a fresh database per test; the tests seed explicitly rather
//! than trusting the environment.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use dhi_agent::{LlmPool, MockAgentRuntime, PoolSnapshot};
use dhi_core::{GraphStore, GuardStore};
use dhi_engine::Engine;
use dhi_guard::{Policy, SharedPolicy};
use dhi_server::{AppState, router};
use dhi_store::Store;
use dhi_store::mgmt::BootstrapSeed;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

/// Full wiring, as `main.rs` builds it — with the mock runtime standing in
/// for the Rig agent (these tests exercise management, not completions).
async fn live_state(pool: sqlx::PgPool) -> (AppState, Arc<Store>, SharedPolicy) {
    let store = Arc::new(Store::new(pool));
    store
        .seed_defaults(&BootstrapSeed {
            base_url: "https://api.river.ai/v1".into(),
            api_key: "sk-live-9999".into(),
            planner_model: "river-planner".into(),
            worker_model: "river-worker".into(),
            planner_thinking_type: String::new(),
            planner_reasoning_effort: String::new(),
            worker_thinking_type: String::new(),
            worker_reasoning_effort: String::new(),
            approval_timeout_secs: 900,
        })
        .await
        .unwrap();

    let pool_snapshot = dhi_server::build_pool_snapshot(&store)
        .await
        .expect("pool snapshot");
    let pool = LlmPool::new(pool_snapshot);
    let guard = SharedPolicy::new(
        dhi_server::build_guard_policy(&store)
            .await
            .expect("guard policy"),
    );

    let store_trait: Arc<dyn GraphStore> = store.clone();
    let guard_store: Arc<dyn GuardStore> = store.clone();
    let engine = Arc::new(Engine::new(store_trait.clone(), Arc::new(MockAgentRuntime)));
    let state = AppState {
        store: store_trait,
        guard_store: guard_store.clone(),
        engine,
        agent: Arc::new(MockAgentRuntime),
        pool,
        guard: guard.clone(),
        mgmt: Some(store.clone()),
        mcp: dhi_server::mcp::McpManager::idle(),
        tool_catalog: Arc::new(dhi_tool_catalog::CatalogRuntime::default()),
        tool_control: None,
        web: Arc::new(dhi_server::web_providers::WebProviderRuntime::empty()),
        task_runs: Arc::new(dhi_server::TaskRunRegistry::disabled()),
        project_folders: Arc::new(dhi_server::project_folders::ProjectFolders::default()),
    };
    (state, store, guard)
}

/// The same wiring on a database the env seed never touched (mock-mode
/// bootstrap): no profiles, no settings row, no rules.
async fn bare_state(pool: sqlx::PgPool) -> (AppState, Arc<Store>) {
    let store = Arc::new(Store::new(pool));
    let pool = LlmPool::new(
        dhi_server::build_pool_snapshot(&store)
            .await
            .expect("pool snapshot"),
    );
    let guard = SharedPolicy::new(
        dhi_server::build_guard_policy(&store)
            .await
            .expect("guard policy"),
    );

    let store_trait: Arc<dyn GraphStore> = store.clone();
    let guard_store: Arc<dyn GuardStore> = store.clone();
    let engine = Arc::new(Engine::new(store_trait.clone(), Arc::new(MockAgentRuntime)));
    let state = AppState {
        store: store_trait,
        guard_store: guard_store.clone(),
        engine,
        agent: Arc::new(MockAgentRuntime),
        pool,
        guard: guard.clone(),
        mgmt: Some(store.clone()),
        mcp: dhi_server::mcp::McpManager::idle(),
        tool_catalog: Arc::new(dhi_tool_catalog::CatalogRuntime::default()),
        tool_control: None,
        web: Arc::new(dhi_server::web_providers::WebProviderRuntime::empty()),
        task_runs: Arc::new(dhi_server::TaskRunRegistry::disabled()),
        project_folders: Arc::new(dhi_server::project_folders::ProjectFolders::default()),
    };
    (state, store)
}

async fn send(app: axum::Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap_or(Value::Null)
    };
    (status, json)
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn post_json(uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn put_json(uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn delete(uri: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

// ---------- profiles: masking + CRUD + activation ----------

/// `POST /api/llm/profiles` saves, `GET /api/llm/profiles` never echoes the
/// key (hint only, and the response DTO has no `api_key` field at all);
/// deletes answer 404 / 409 per the guard rules.
#[sqlx::test(migrations = "../dhi-store/migrations")]
async fn profile_routes_mask_the_api_key(pg: sqlx::PgPool) {
    let (state, store, _guard) = live_state(pg).await;
    let app = router(state);

    let (status, body) = send(
        app.clone(),
        post_json(
            "/api/llm/profiles",
            json!({
                "name": "backup",
                "format": "openai",
                "base_url": "https://api.backup.ai/v1",
                "model": "river-xl",
                "api_key": "sk-backup-secret-1234",
                "priority": 100,
                "thinking_type": "enabled",
                "reasoning_effort": "high"
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let backup_id = body["id"].as_i64().expect("id returned");

    let (status, body) = send(app.clone(), get("/api/llm/profiles")).await;
    assert_eq!(status, StatusCode::OK);
    let profiles = body["profiles"].as_array().unwrap();
    assert_eq!(profiles.len(), 3, "two seeded defaults plus the backup");
    let backup = profiles
        .iter()
        .find(|p| p["id"].as_i64() == Some(backup_id))
        .unwrap();
    assert_eq!(backup["api_key_hint"], "…1234");
    assert_eq!(backup["priority"], 100);
    assert_eq!(backup["thinking_type"], "enabled");
    assert_eq!(backup["reasoning_effort"], "high");
    assert!(
        backup.get("api_key").is_none(),
        "the response DTO has no api_key field at all: {backup}"
    );

    // Malformed saves are 400s with the single error shape.
    let (status, body) = send(
        app.clone(),
        post_json(
            "/api/llm/profiles",
            json!({ "name": "x", "format": "ollama", "model": "m" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].is_string());

    let (status, body) = send(
        app.clone(),
        post_json(
            "/api/llm/profiles",
            json!({
                "name": "x",
                "format": "openai",
                "model": "m",
                "thinking_type": "on"
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].is_string());

    // Active defaults refuse deletion (409); unknown ids are 404.
    let settings = store.get_settings().await.unwrap().unwrap();
    let (status, _) = send(
        app.clone(),
        delete(&format!(
            "/api/llm/profiles/{}",
            settings.planner_profile_id
        )),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, body) = send(app.clone(), delete("/api/llm/profiles/424242")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].is_string());

    let (status, body) = send(
        app.clone(),
        delete(&format!("/api/llm/profiles/{backup_id}")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["deleted"], backup_id);
}

/// Activation re-heads the pool chain, visible through `/api/llm/pool/status`
/// without touching the process: the shared snapshot did the moving.
#[sqlx::test(migrations = "../dhi-store/migrations")]
async fn activate_profile_reheads_the_shared_pool(pg: sqlx::PgPool) {
    let (state, store, _guard) = live_state(pg).await;
    let backup = store
        .save_profile(&dhi_store::mgmt::LlmProfileUpsert {
            id: None,
            name: "primary".into(),
            format: "openai".into(),
            base_url: "https://primary.ai/v1".into(),
            model: "river-primary".into(),
            api_key: "sk-primary".into(),
            max_tokens: 0,
            context_window_k: 0,
            streaming: true,
            priority: 900,
            pool_exclude: false,
            thinking_type: String::new(),
            reasoning_effort: String::new(),
        })
        .await
        .unwrap();
    let app = router(state);

    let (status, body) = send(app.clone(), get("/api/llm/pool/status")).await;
    assert_eq!(status, StatusCode::OK);
    let head = body["planner"]["chain"][0]["id"].as_i64().unwrap();
    assert_ne!(head, backup, "activation has not happened yet");
    assert!(body["planner"]["chain"][0]["tripped"].is_boolean());

    let (status, body) = send(
        app.clone(),
        post_json(
            "/api/llm/profiles/active",
            json!({ "id": backup, "role": "planner" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = send(app.clone(), get("/api/llm/pool/status")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["planner"]["chain"][0]["id"].as_i64(),
        Some(backup),
        "the activated profile heads the planner chain"
    );
    // The persisted settings row agrees.
    assert_eq!(
        store
            .get_settings()
            .await
            .unwrap()
            .unwrap()
            .planner_profile_id,
        backup
    );

    // Errors: unknown profile 404, unknown role 400.
    let (status, _) = send(
        app.clone(),
        post_json(
            "/api/llm/profiles/active",
            json!({ "id": 424242, "role": "planner" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(
        app.clone(),
        post_json(
            "/api/llm/profiles/active",
            json!({ "id": backup, "role": "judge" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Breaker reset answers with the (unchanged-shaped) status document.
    let (status, body) = send(app, post_json("/api/llm/pool/reset", json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["planner"]["chain"].is_array());
}

// ---------- task pins ----------

/// `POST /api/tasks` accepts known pins (persisted, echoed, delete-guarding),
/// rejects unknown ones with 400, and refuses to run without a body `goal`.
#[sqlx::test(migrations = "../dhi-store/migrations")]
async fn task_creation_validates_role_pins(pg: sqlx::PgPool) {
    let (state, store, _guard) = live_state(pg).await;
    let spare = store
        .save_profile(&dhi_store::mgmt::LlmProfileUpsert {
            id: None,
            name: "spare".into(),
            format: "anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            model: "claude-x".into(),
            api_key: "sk-anthropic".into(),
            max_tokens: 0,
            context_window_k: 0,
            streaming: true,
            priority: 0,
            pool_exclude: true,
            thinking_type: String::new(),
            reasoning_effort: String::new(),
        })
        .await
        .unwrap();
    let app = router(state);

    // Unknown pin: 400.
    let (status, body) = send(
        app.clone(),
        post_json(
            "/api/tasks",
            json!({ "goal": "mate in two", "planner_profile_id": 424242 }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("424242"));

    // Known pins: created, persisted, echoed.
    let (status, body) = send(
        app.clone(),
        post_json(
            "/api/tasks",
            json!({
                "goal": "fork the queen",
                "planner_profile_id": spare,
                "worker_profile_id": spare
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["pinned_planner_profile_id"], spare);
    assert_eq!(body["pinned_worker_profile_id"], spare);
    let task_id: uuid::Uuid = body["id"].as_str().unwrap().parse().unwrap();
    let persisted = store.get_task(task_id).await.unwrap().unwrap();
    assert_eq!(persisted.pinned_planner_profile_id, Some(spare));

    // The `pinned_*` spellings are accepted aliases of the same fields.
    let (status, body) = send(
        app.clone(),
        post_json(
            "/api/tasks",
            json!({
                "goal": "alias spelling",
                "pinned_planner_profile_id": spare,
                "pinned_worker_profile_id": spare
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["pinned_planner_profile_id"], spare);
    assert_eq!(body["pinned_worker_profile_id"], spare);

    // The pinned (and pool-excluded) profile refuses deletion: 409.
    let (status, _) = send(app, delete(&format!("/api/llm/profiles/{spare}"))).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

// ---------- guard rules: CRUD + hot rebuild ----------

/// A rule created through the route is enforced by the shared snapshot on the
/// very next evaluation; disable/enable/delete/remove each reshape it. Bad
/// actions and broken regexes are 400s.
#[sqlx::test(migrations = "../dhi-store/migrations")]
async fn rule_routes_hot_rebuild_the_shared_guard(pg: sqlx::PgPool) {
    let (state, _, guard) = live_state(pg).await;
    let app = router(state);

    let (status, rule) = send(
        app.clone(),
        post_json(
            "/api/intercept/rules",
            json!({
                "name": "block-rm",
                "match_target": "tool_name",
                "match_type": "exact",
                "pattern": "rm",
                "action": "deny",
                "reason": "no deletions",
                "priority": 700
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rule}");
    let rule_id = rule["id"].as_i64().unwrap();

    let shared = guard.clone();
    // Simulate an in-flight executor call against the swapped snapshot.
    let blocked = {
        let snapshot = shared.load();
        let decision = snapshot.evaluate("rm", &json!({}));
        assert_eq!(decision.matched_rule.as_deref(), Some("block-rm"));
        decision.verdict
    };
    assert_eq!(blocked, dhi_core::GuardVerdict::Deny);

    // Full replace flips the verdict.
    let (status, _) = send(
        app.clone(),
        put_json(
            &format!("/api/intercept/rules/{rule_id}"),
            json!({
                "name": "ask-rm",
                "match_target": "tool_name",
                "match_type": "exact",
                "pattern": "rm",
                "action": "ask",
                "reason": "humans decide",
                "priority": 700
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        shared.load().evaluate("rm", &json!({})).verdict,
        dhi_core::GuardVerdict::Ask
    );

    // Disabling drops the rule; the settings default resumes.
    let (status, _) = send(
        app.clone(),
        post_json(
            &format!("/api/intercept/rules/{rule_id}/toggle"),
            json!({ "enabled": false }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        shared.load().evaluate("rm", &json!({})).verdict,
        dhi_core::GuardVerdict::Deny,
        "the seeded default action applies again"
    );

    // Validation: unknown action and a broken regex are 400s.
    for bad in [
        json!({
            "name": "bad-action", "match_target": "tool_name", "match_type": "exact",
            "pattern": "rm", "action": "maybe"
        }),
        json!({
            "name": "bad-regex", "match_target": "tool_input", "match_type": "regex",
            "pattern": "(unclosed", "action": "deny"
        }),
    ] {
        let (status, body) = send(app.clone(), post_json("/api/intercept/rules", bad)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body["error"].is_string());
    }

    // Delete; the route answers 200 and later evaluation sees nothing.
    let (status, body) = send(
        app.clone(),
        delete(&format!("/api/intercept/rules/{rule_id}")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["deleted"], rule_id);
    let (status, body) = send(
        app.clone(),
        put_json(
            "/api/intercept/rules/424242",
            json!({
                "name": "ghost", "match_target": "tool_name", "match_type": "exact",
                "pattern": "x", "action": "allow"
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].is_string());

    let (status, body) = send(app, get("/api/intercept/rules")).await;
    assert_eq!(status, StatusCode::OK);
    let rules = body["rules"].as_array().unwrap();
    assert!(!rules.iter().any(|r| r["id"].as_i64() == Some(rule_id)));
    assert_eq!(rules.len(), 19, "the seeded allow rules remain");
}

// ---------- settings ----------

/// `GET /api/settings` exposes summaries and hints only; `PUT` merges
/// partially and rebuilds both snapshots (observable via the approval budget
/// the guard snapshot now prescribes).
#[sqlx::test(migrations = "../dhi-store/migrations")]
async fn settings_routes_merge_and_stay_secret_free(pg: sqlx::PgPool) {
    let (state, store, guard) = live_state(pg).await;
    let app = router(state);

    let (status, body) = send(app.clone(), get("/api/settings")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["planner_profile"]["name"], "planner-default");
    assert_eq!(body["worker_profile"]["name"], "worker-default");
    assert_eq!(body["llm_key_set"], true, "both seeded profiles carry keys");
    assert_eq!(body["approval_timeout_secs"], 900);
    assert_eq!(body["guard_default_action"], "deny");
    assert_eq!(body["planner_api_key_hint"], "…9999");
    assert!(
        body.get("api_key").is_none(),
        "no key ever leaves the server"
    );

    let shared = guard.clone();

    let (status, body) = send(
        app.clone(),
        put_json("/api/settings", json!({ "approval_timeout_secs": 60 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["approval_timeout_secs"], 60);
    assert_eq!(
        store
            .get_settings()
            .await
            .unwrap()
            .unwrap()
            .approval_timeout_secs,
        60
    );

    // The preset budget rides the rebuilt snapshot (no rule override).
    let unmatched = shared.load().evaluate("nothing-matches-this", &json!({}));
    assert_eq!(
        shared.load().prescribed_approval_secs(&unmatched),
        60,
        "the settings timeout governs snapshot-built policies"
    );

    // Merge: role references stick when omitted.
    let planner = body["planner_profile"]["id"].as_i64().unwrap();
    let worker = body["worker_profile"]["id"].as_i64().unwrap();
    let (status, body) = send(
        app.clone(),
        put_json("/api/settings", json!({ "planner_profile_id": worker })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["planner_profile"]["id"], worker);
    assert_eq!(body["worker_profile"]["id"], worker);
    assert_eq!(body["approval_timeout_secs"], 60);
    let _ = planner;

    // Validation: 0-second budget 400, unknown profile 404.
    let (status, _) = send(
        app.clone(),
        put_json("/api/settings", json!({ "approval_timeout_secs": 0 })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = send(
        app,
        put_json("/api/settings", json!({ "worker_profile_id": 424242 })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------- settings bootstrap ----------

/// `GET /api/settings` answers `200` with null roles on an unseeded
/// database instead of a permanent `404`.
#[sqlx::test(migrations = "../dhi-store/migrations")]
async fn settings_view_answers_before_any_profile_exists(pg: sqlx::PgPool) {
    let (state, _store) = bare_state(pg).await;
    let app = router(state);

    let (status, body) = send(app.clone(), get("/api/settings")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["planner_profile"].is_null());
    assert!(body["worker_profile"].is_null());
    assert_eq!(body["llm_key_set"], false);
    assert_eq!(body["planner_api_key_hint"], "");
    assert_eq!(body["worker_api_key_hint"], "");
    assert_eq!(body["approval_timeout_secs"], 900);
    assert_eq!(body["guard_default_action"], "deny");
}

/// Creating the FIRST profile through the API bootstraps the settings
/// singleton in the same transaction: both role defaults point at it (the
/// `ON CONFLICT DO NOTHING` insert wins on an empty `app_settings`), and the
/// rebuilt pool heads both chains with it. Later inserts never steal the
/// roles — the singleton already exists.
#[sqlx::test(migrations = "../dhi-store/migrations")]
async fn creating_the_first_profile_bootstraps_both_role_defaults(pg: sqlx::PgPool) {
    let (state, _store) = bare_state(pg).await;
    let app = router(state);

    let (status, body) = send(
        app.clone(),
        post_json(
            "/api/llm/profiles",
            json!({
                "name": "solo",
                "format": "openai",
                "base_url": "https://api.river.ai/v1",
                "model": "river-solo",
                "api_key": "sk-solo-9999"
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let solo_id = body["id"].as_i64().expect("id returned");

    let (status, body) = send(app.clone(), get("/api/settings")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["planner_profile"]["id"], solo_id,
        "the first insert becomes the planner default"
    );
    assert_eq!(
        body["worker_profile"]["id"], solo_id,
        "the first insert becomes the worker default"
    );
    assert_eq!(body["llm_key_set"], true);
    assert_eq!(body["planner_api_key_hint"], "…9999");
    assert_eq!(body["worker_api_key_hint"], "…9999");
    assert_eq!(body["approval_timeout_secs"], 900);
    assert_eq!(body["guard_default_action"], "deny");

    // The refreshed pool agrees: the bootstrapped default heads both chains.
    let (status, body) = send(app.clone(), get("/api/llm/pool/status")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["planner"]["chain"][0]["id"].as_i64(), Some(solo_id));
    assert_eq!(body["worker"]["chain"][0]["id"].as_i64(), Some(solo_id));

    // A second profile leaves the singleton untouched (DO NOTHING).
    let (status, body) = send(
        app.clone(),
        post_json(
            "/api/llm/profiles",
            json!({
                "name": "second",
                "format": "openai",
                "base_url": "https://api.river.ai/v1",
                "model": "river-second",
                "api_key": "sk-second-8888",
                "priority": 100
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = send(app, get("/api/settings")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["planner_profile"]["id"], solo_id,
        "later inserts never overwrite the roles"
    );
}

/// `PUT /api/settings` accepts `guard_default_action` (400 on anything but
/// `allow`/`deny`/`ask`); the value persists, shows in `GET`, and the rebuilt
/// guard snapshot enforces it as the unmatched-call verdict.
#[sqlx::test(migrations = "../dhi-store/migrations")]
async fn settings_put_guard_default_action_reaches_the_snapshot(pg: sqlx::PgPool) {
    let (state, store, guard) = live_state(pg).await;
    let app = router(state);

    let (status, body) = send(
        app.clone(),
        put_json("/api/settings", json!({ "guard_default_action": "ask" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["guard_default_action"], "ask");

    // Persisted…
    assert_eq!(
        store
            .get_settings()
            .await
            .unwrap()
            .unwrap()
            .guard_default_action,
        "ask"
    );
    // …surfaced by GET…
    let (status, body) = send(app.clone(), get("/api/settings")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["guard_default_action"], "ask");
    // …and enforced by the hot-swapped snapshot.
    let unmatched = guard.load().evaluate("nothing-matches-this", &json!({}));
    assert_eq!(
        unmatched.verdict,
        dhi_core::GuardVerdict::Ask,
        "the settings default action governs unmatched calls"
    );

    // Merge semantics: a timeout patch leaves the action alone.
    let (status, body) = send(
        app.clone(),
        put_json("/api/settings", json!({ "approval_timeout_secs": 60 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["guard_default_action"], "ask");

    // Validation: unknown actions are 400s with the one error shape.
    for bad in ["maybe", ""] {
        let (status, body) = send(
            app.clone(),
            put_json("/api/settings", json!({ "guard_default_action": bad })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body["error"].is_string());
    }
}

/// ARTEX parity (`db/config.go` SaveProfile): `"id": 0` on the profile upsert
/// inserts instead of addressing row 0 (which used to 404 permanently).
#[sqlx::test(migrations = "../dhi-store/migrations")]
async fn save_profile_accepts_zero_id_as_insert(pg: sqlx::PgPool) {
    let (state, _store, _guard) = live_state(pg).await;
    let app = router(state);

    let (status, body) = send(
        app.clone(),
        post_json(
            "/api/llm/profiles",
            json!({
                "id": 0,
                "name": "zero-insert",
                "format": "openai",
                "base_url": "https://api.river.ai/v1",
                "model": "river-zero",
                "api_key": "sk-zero-7777"
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let new_id = body["id"].as_i64().expect("id returned");
    assert_ne!(new_id, 0, "a zero id inserts a fresh row");

    let (status, body) = send(app, get("/api/llm/profiles")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["id"].as_i64() == Some(new_id)),
        "the inserted profile is listed: {body}"
    );
}

/// Deleting a profile clears its breaker state along with the row (ARTEX
/// `s.llmHealth.Reset(id)`): the id vanishes from the status view and a stale
/// trip cannot linger in the breaker map.
#[sqlx::test(migrations = "../dhi-store/migrations")]
async fn deleting_a_profile_clears_its_breaker_state(pg: sqlx::PgPool) {
    let (state, _store, _guard) = live_state(pg).await;
    let pool = state.pool.clone();
    let spare = {
        let mgmt = state.mgmt.as_ref().expect("mgmt store");
        mgmt.save_profile(&dhi_store::mgmt::LlmProfileUpsert {
            id: None,
            name: "doomed".into(),
            format: "openai".into(),
            base_url: "https://doomed.ai/v1".into(),
            model: "river-doomed".into(),
            api_key: "sk-doomed".into(),
            max_tokens: 0,
            context_window_k: 0,
            streaming: true,
            priority: 500,
            pool_exclude: false,
            thinking_type: String::new(),
            reasoning_effort: String::new(),
        })
        .await
        .unwrap()
    };
    // Refresh the shared snapshot so the doomed profile joins the chain,
    // exactly as any mutating route would.
    {
        let mgmt = state.mgmt.as_ref().expect("mgmt store").clone();
        let snapshot = dhi_server::build_pool_snapshot(&mgmt)
            .await
            .expect("pool snapshot");
        pool.swap(snapshot);
    }

    for _ in 0..dhi_agent::POOL_TRIP_THRESHOLD {
        pool.record_failure(spare);
    }
    assert!(pool.is_tripped(spare), "setup: the profile is tripped");
    let tripped_listing = pool.status();
    assert!(
        tripped_listing
            .planner
            .chain
            .iter()
            .any(|entry| entry.id == spare && entry.tripped),
        "setup: the status view reports the trip"
    );

    let app = router(state);
    let (status, body) = send(app, delete(&format!("/api/llm/profiles/{spare}"))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["deleted"], spare);

    assert!(
        !pool.is_tripped(spare),
        "the delete route reset the deleted profile's breaker"
    );
}

/// Deleting a terminal task removes every task-owned row through the real
/// store; bulk delete refuses the active task independently.
#[sqlx::test(migrations = "../dhi-store/migrations")]
async fn task_delete_removes_terminal_tasks_only(pg: sqlx::PgPool) {
    let (state, store, _guard) = live_state(pg).await;
    let done = store.create_task("delete done").await.unwrap();
    let cancelled = store.create_task("delete cancelled").await.unwrap();
    let active = store.create_task("keep active").await.unwrap();
    for (id, terminal) in [
        (done.id, dhi_core::TaskState::Done),
        (cancelled.id, dhi_core::TaskState::Cancelled),
    ] {
        GraphStore::set_task_state_guarded(store.as_ref(), id, dhi_core::TaskState::Running)
            .await
            .unwrap();
        GraphStore::set_task_state_guarded(store.as_ref(), id, terminal)
            .await
            .unwrap();
    }

    let app = router(state);

    let (status, body) = send(app.clone(), delete(&format!("/api/tasks/{}", done.id))).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert!(body.is_null(), "204 has no body");
    assert!(store.get_task(done.id).await.unwrap().is_none());

    let (status, body) = send(
        app,
        post_json(
            "/api/tasks/delete",
            json!({"ids": [cancelled.id, active.id, Uuid::new_v4()]}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["deleted"], json!([cancelled.id]));
    let blocked = body["blocked"].as_array().unwrap();
    assert_eq!(blocked.len(), 2, "{body}");
    assert!(
        blocked
            .iter()
            .any(|row| row["id"] == active.id.to_string() && row["state"] == "created")
    );
    assert!(blocked.iter().any(|row| {
        row["state"] == "unknown"
            && row["error"]
                .as_str()
                .unwrap_or_default()
                .starts_with("task not found")
    }));
    assert!(store.get_task(cancelled.id).await.unwrap().is_none());
    assert!(store.get_task(active.id).await.unwrap().is_some());
}

/// A state without a management store answers 500 on management routes while
/// the core routes keep working.
#[sqlx::test(migrations = "../dhi-store/migrations")]
async fn management_routes_degrade_without_mgmt_store(pg: sqlx::PgPool) {
    let store = Arc::new(Store::new(pg));
    let store_trait: Arc<dyn GraphStore> = store.clone();
    let guard_store: Arc<dyn GuardStore> = store.clone();
    let engine = Arc::new(Engine::new(store_trait.clone(), Arc::new(MockAgentRuntime)));
    let state = AppState {
        store: store_trait,
        guard_store,
        engine,
        agent: Arc::new(MockAgentRuntime),
        pool: LlmPool::new(PoolSnapshot::default()),
        guard: SharedPolicy::from_policy(&Policy::chess(), 900),
        mgmt: None,
        mcp: dhi_server::mcp::McpManager::idle(),
        tool_catalog: Arc::new(dhi_tool_catalog::CatalogRuntime::default()),
        tool_control: None,
        web: Arc::new(dhi_server::web_providers::WebProviderRuntime::empty()),
        task_runs: Arc::new(dhi_server::TaskRunRegistry::disabled()),
        project_folders: Arc::new(dhi_server::project_folders::ProjectFolders::default()),
    };
    let app = router(state);

    let (status, body) = send(app.clone(), get("/api/llm/profiles")).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(body["error"].is_string());

    let (status, _) = send(app, get("/api/health")).await;
    assert_eq!(status, StatusCode::OK, "core routes are unaffected");
}
