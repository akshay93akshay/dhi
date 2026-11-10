//! `feat/provider-keys` route-level tests against a real PostgreSQL store.
//!
//! One serial test owns the process environment for its duration: the web key
//! variables are cleared so the database row semantics are unambiguous, and
//! every value is restored afterwards.

use std::ffi::OsString;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use dhi_agent::{LlmPool, MockAgentRuntime};
use dhi_core::{GraphStore, GuardStore};
use dhi_engine::Engine;
use dhi_guard::{Policy, SharedPolicy};
use dhi_server::web_providers::WebProviderRuntime;
use dhi_server::{AppState, router};
use dhi_store::Store;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const WEB_ENV: [&str; 6] = [
    "TINYFISH_API_KEY",
    "PARALLEL_API_KEY",
    "FIRECRAWL_API_KEY",
    "FIRECRAWL_BASE_URL",
    "DHI_WEB_PROVIDER",
    "DHI_CONFIG",
];

async fn state(pool: sqlx::PgPool) -> (axum::Router, Arc<Store>) {
    let store = Arc::new(Store::new(pool));
    let store_trait: Arc<dyn GraphStore> = store.clone();
    let guard_store: Arc<dyn GuardStore> = store.clone();
    let agent: Arc<dyn dhi_agent::AgentRuntime> = Arc::new(MockAgentRuntime);
    let engine = Arc::new(Engine::new(store_trait.clone(), agent.clone()));
    let web = Arc::new(WebProviderRuntime::new(Some(store.clone())));
    web.reload().await.expect("empty settings load");
    let state = AppState {
        store: store_trait,
        guard_store,
        engine,
        agent,
        pool: Arc::new(LlmPool::default()),
        guard: SharedPolicy::from_policy(&Policy::chess(), 900),
        mgmt: Some(store.clone()),
        mcp: dhi_server::mcp::McpManager::idle(),
        tool_catalog: Arc::new(dhi_tool_catalog::CatalogRuntime::default()),
        tool_control: None,
        web,
        task_runs: Arc::new(dhi_server::TaskRunRegistry::disabled()),
        project_folders: Arc::new(dhi_server::project_folders::ProjectFolders::default()),
        file_browser: Arc::new(dhi_server::file_browser::FileBrowser::default()),
        web_projects: Arc::new(dhi_server::web_projects::WebProjects::default()),
        web_project_store: None,
        team_store: None,
        scale_gate: None,
    };
    (router(state), store)
}

async fn send(app: axum::Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = app.oneshot(request).await.expect("request");
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let json = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap_or(Value::Null)
    };
    (status, json)
}

fn request(method: &str, uri: &str, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder().method(method).uri(uri);
    match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("request"),
        None => builder.body(Body::empty()).expect("request"),
    }
}

/// Replaces the process web environment with a clean slate; the returned
/// guard restores every value.
struct EnvGuard(Vec<(&'static str, Option<OsString>)>);

impl EnvGuard {
    fn clear() -> Self {
        let mut previous = Vec::new();
        for name in WEB_ENV {
            previous.push((name, std::env::var_os(name)));
            unsafe {
                std::env::remove_var(name);
            }
        }
        // Point the config file at a path that does not exist.
        let missing = std::env::temp_dir().join("dhi-web-provider-test-missing.json");
        unsafe {
            std::env::set_var("DHI_CONFIG", &missing);
        }
        Self(previous)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in &self.0 {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
}

#[sqlx::test(migrations = "../dhi-store/migrations")]
async fn provider_routes_are_database_authoritative_and_never_return_keys(pool: sqlx::PgPool) {
    let _env = EnvGuard::clear();
    let (app, _store) = state(pool).await;

    let (status, body) = send(app.clone(), request("GET", "/api/web/providers", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["search_read_order"], json!([]));
    assert_eq!(body["research_order"], json!([]));
    assert_eq!(
        body["providers"]
            .as_array()
            .expect("providers")
            .iter()
            .map(|provider| provider["provider"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["tinyfish", "parallel", "firecrawl"]
    );
    for provider in body["providers"].as_array().expect("providers") {
        assert_eq!(provider["configured"], false);
        assert_eq!(provider["key_hint"], Value::Null);
        assert_eq!(provider["source"], "none");
        assert_eq!(provider["status"], "missing_key");
        assert_eq!(provider["timeout_secs"], 60);
        assert_eq!(provider["enabled"], true);
    }

    // A saved key is write-only: only the stored hint is returned.
    let (status, body) = send(
        app.clone(),
        request(
            "PUT",
            "/api/web/providers/tinyfish",
            Some(json!({
                "api_key": "tf-live-secret-1a2b",
                "base_url": "http://127.0.0.1:9999",
                "timeout_secs": 30,
                "enabled": true
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["provider"], "tinyfish");
    assert_eq!(body["display_name"], "TinyFish");
    assert_eq!(body["configured"], true);
    assert_eq!(body["key_hint"], "…1a2b");
    assert_eq!(body["source"], "database");
    assert_eq!(body["base_url"], Value::Null, "TinyFish is fixed");
    assert_eq!(body["timeout_secs"], 30);
    assert_eq!(body["enabled"], true);
    assert_eq!(body["status"], "ready");
    assert!(
        !body.to_string().contains("tf-live-secret-1a2b"),
        "the response leaked a raw provider key"
    );

    let (_, body) = send(app.clone(), request("GET", "/api/web/providers", None)).await;
    assert_eq!(body["search_read_order"], json!(["tinyfish"]));
    assert_eq!(body["providers"][0]["key_hint"], "…1a2b");

    // Clearing the key keeps the row authoritative even with an env key.
    unsafe {
        std::env::set_var("TINYFISH_API_KEY", "tf-env-must-not-return");
    }
    let (status, body) = send(
        app.clone(),
        request("DELETE", "/api/web/providers/tinyfish/key", None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["configured"], false);
    assert_eq!(body["key_hint"], Value::Null);
    assert_eq!(body["source"], "none");
    assert_eq!(body["status"], "missing_key");

    // An empty PUT key keeps the cleared (null) key: it does not resurrect
    // the environment key either.
    let (status, body) = send(
        app.clone(),
        request(
            "PUT",
            "/api/web/providers/tinyfish",
            Some(json!({ "api_key": "", "timeout_secs": 45 })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["configured"], false);
    assert_eq!(body["source"], "none");
    assert_eq!(body["key_hint"], Value::Null);
    assert_eq!(body["timeout_secs"], 45);

    // Parallel keeps the built-in research order.
    let (status, body) = send(
        app.clone(),
        request(
            "PUT",
            "/api/web/providers/parallel",
            Some(json!({
                "api_key": "par-live-secret-5678",
                "base_url": "http://127.0.0.1:9091",
                "timeout_secs": 15,
                "enabled": true
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["key_hint"], "…5678");
    assert_eq!(body["base_url"], "http://127.0.0.1:9091");
    assert_eq!(body["timeout_secs"], 15);
    assert_eq!(body["status"], "ready");

    // A keyless local Firecrawl is usable without inventing a key source.
    let (status, body) = send(
        app.clone(),
        request(
            "PUT",
            "/api/web/providers/firecrawl",
            Some(json!({ "base_url": "http://127.0.0.1:3002" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["configured"], true);
    assert_eq!(body["source"], "none");
    assert_eq!(body["key_hint"], Value::Null);
    assert_eq!(body["base_url"], "http://127.0.0.1:3002");
    assert_eq!(body["status"], "ready");

    let (status, body) = send(app.clone(), request("GET", "/api/web/providers", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["search_read_order"], json!(["parallel", "firecrawl"]));
    assert_eq!(body["research_order"], json!(["parallel"]));

    // Reload returns the GET shape and swaps live.
    let (status, body) = send(
        app.clone(),
        request("POST", "/api/web/providers/reload", None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["search_read_order"], json!(["parallel", "firecrawl"]));
    assert_eq!(body["research_order"], json!(["parallel"]));

    // Unknown providers, invalid bodies, and missing keys are 404/400/400.
    let (status, _) = send(
        app.clone(),
        request(
            "PUT",
            "/api/web/providers/bing",
            Some(json!({ "api_key": "x" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    for body in [
        json!({ "timeout_secs": 0 }),
        json!({ "timeout_secs": 3_601 }),
        json!({ "base_url": "ftp://example.com" }),
        json!({ "base_url": "http://exa mple.com" }),
    ] {
        let (status, _) = send(
            app.clone(),
            request("PUT", "/api/web/providers/tinyfish", Some(body)),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    let (status, _) = send(
        app.clone(),
        request("POST", "/api/web/providers/tinyfish/test", None),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Nothing stored or returned ever contains a raw key.
    let (_, body) = send(app, request("GET", "/api/web/providers", None)).await;
    let json = body.to_string();
    for secret in [
        "tf-live-secret-1a2b",
        "par-live-secret-5678",
        "tf-env-must-not-return",
    ] {
        assert!(!json.contains(secret), "response leaked a raw provider key");
    }
}
