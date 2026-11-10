//! The same-origin static web UI fallback and its task deep-link aliases.
//!
//! Every test uses a tempdir export and the DB-free `MemoryStore` app state;
//! no PostgreSQL or environment mutation is required.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use dhi_agent::{AgentRuntime, LlmPool, MockAgentRuntime};
use dhi_core::{GraphStore, GuardStore};
use dhi_engine::Engine;
use dhi_guard::{Policy, SharedPolicy};
use dhi_server::mcp::McpManager;
use dhi_server::web_providers::WebProviderRuntime;
use dhi_server::{AppState, TaskRunRegistry, router_with_web_dir};
use dhi_testkit::MemoryStore;
use http_body_util::BodyExt;
use serde_json::Value;
use tempfile::TempDir;
use tower::ServiceExt;

const TASK_ID: &str = "00000000-0000-4000-8000-000000000001";

fn app_state() -> AppState {
    let store = Arc::new(MemoryStore::new());
    let store_trait: Arc<dyn GraphStore> = store.clone();
    let guard_store: Arc<dyn GuardStore> = store;
    let agent: Arc<dyn AgentRuntime> = Arc::new(MockAgentRuntime);
    let engine = Arc::new(Engine::new(store_trait.clone(), agent.clone()));
    AppState {
        store: store_trait,
        guard_store,
        engine,
        agent,
        pool: Arc::new(LlmPool::default()),
        guard: SharedPolicy::from_policy(&Policy::chess(), 900),
        mgmt: None,
        mcp: McpManager::idle(),
        tool_catalog: Arc::new(dhi_tool_catalog::CatalogRuntime::default()),
        tool_control: None,
        web: Arc::new(WebProviderRuntime::empty()),
        task_runs: Arc::new(TaskRunRegistry::disabled()),
        project_folders: Arc::new(dhi_server::project_folders::ProjectFolders::default()),
        web_projects: Arc::new(dhi_server::web_projects::WebProjects::default()),
        web_project_store: None,
        team_store: None,
        scale_gate: None,
    }
}

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().expect("fixture parent")).expect("fixture dir");
    fs::write(path, contents).expect("fixture file");
}

/// One complete export plus a sibling secret the static router must never
/// reach through a traversal path.
fn fixture() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("web");
    write(&root, "index.html", "INDEX SHELL");
    write(&root, "tasks/index.html", "TASKS SHELL");
    write(&root, "tasks/index.txt", "TASKS PAYLOAD");
    write(&root, "tasks/preview/index.html", "DETAIL SHELL");
    write(&root, "tasks/preview/work/index.html", "WORK SHELL");
    write(&root, "tasks/preview/graph/index.html", "GRAPH SHELL");
    write(&root, "tasks/preview/index.txt", "DETAIL PAYLOAD");
    write(&root, "tasks/preview/work/index.txt", "WORK PAYLOAD");
    write(&root, "tasks/preview/graph/index.txt", "GRAPH PAYLOAD");
    write(
        &root,
        "tasks/preview/__next.tasks.$d$id.__PAGE__.txt",
        "DETAIL NEXT PAYLOAD",
    );
    write(
        &root,
        "tasks/preview/work/__next.tasks.$d$id.work.__PAGE__.txt",
        "WORK NEXT PAYLOAD",
    );
    write(
        &root,
        "tasks/preview/graph/__next.tasks.$d$id.graph.__PAGE__.txt",
        "GRAPH NEXT PAYLOAD",
    );
    write(&root, "_next/static/chunks/app.js", "console.log('app');");
    fs::write(dir.path().join("secret.txt"), "TOP SECRET").expect("sibling secret");
    (dir, root)
}

async fn send(app: Router, uri: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
    let response = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .expect("request");
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec();
    (status, headers, body)
}

fn body_text(body: &[u8]) -> String {
    String::from_utf8_lossy(body).into_owned()
}

fn content_type(headers: &HeaderMap) -> &str {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
}

fn app_with(root: &Path) -> Router {
    router_with_web_dir(app_state(), Some(root.to_path_buf()))
}

#[tokio::test]
async fn api_health_is_not_shadowed_by_the_static_fallback() {
    let (_dir, root) = fixture();
    let (status, headers, body) = send(app_with(&root), "/api/health").await;

    assert_eq!(status, StatusCode::OK);
    assert!(content_type(&headers).contains("application/json"));
    let json: Value = serde_json::from_slice(&body).expect("health json");
    assert_eq!(json["status"], "ok");
}

#[tokio::test]
async fn unknown_api_paths_answer_json_not_html() {
    let (_dir, root) = fixture();
    let app = app_with(&root);

    for uri in ["/api", "/api/", "/api/not-a-route"] {
        let (status, headers, body) = send(app.clone(), uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        assert!(content_type(&headers).contains("application/json"), "{uri}");
        let json: Value = serde_json::from_slice(&body).expect("error json");
        assert_eq!(json["error"], "not found", "{uri}");
        assert!(!body_text(&body).contains("INDEX SHELL"), "{uri}");
    }
}

#[tokio::test]
async fn root_serves_the_exported_index() {
    let (_dir, root) = fixture();
    let (status, headers, body) = send(app_with(&root), "/").await;

    assert_eq!(status, StatusCode::OK);
    assert!(content_type(&headers).contains("text/html"));
    assert!(body_text(&body).contains("INDEX SHELL"));
}

#[tokio::test]
async fn real_task_deep_links_serve_the_preview_shells_without_redirects() {
    let (_dir, root) = fixture();
    let app = app_with(&root);
    let cases = [
        (format!("/tasks/{TASK_ID}"), "DETAIL SHELL"),
        (format!("/tasks/{TASK_ID}/"), "DETAIL SHELL"),
        (format!("/tasks/{TASK_ID}/work"), "WORK SHELL"),
        (format!("/tasks/{TASK_ID}/work/"), "WORK SHELL"),
        (format!("/tasks/{TASK_ID}/graph"), "GRAPH SHELL"),
        (format!("/tasks/{TASK_ID}/graph/"), "GRAPH SHELL"),
    ];

    for (uri, marker) in cases {
        let (status, headers, body) = send(app.clone(), &uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert!(
            headers.get(header::LOCATION).is_none(),
            "{uri} must not redirect"
        );
        assert!(content_type(&headers).contains("text/html"), "{uri}");
        assert!(body_text(&body).contains(marker), "{uri}");
    }
}

#[tokio::test]
async fn tasks_routes_serve_the_task_list_shell() {
    let (_dir, root) = fixture();
    let app = app_with(&root);

    for uri in ["/tasks", "/tasks/"] {
        let (status, headers, body) = send(app.clone(), uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert!(content_type(&headers).contains("text/html"), "{uri}");
        assert_eq!(body_text(&body), "TASKS SHELL", "{uri}");
    }
}

#[tokio::test]
async fn task_list_aliases_serve_the_task_list_payload_not_the_detail_placeholder() {
    let (_dir, root) = fixture();
    let app = app_with(&root);

    let (status, headers, body) = send(app.clone(), "/tasks/index.txt").await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type(&headers).contains("text/plain"));
    assert_eq!(body_text(&body), "TASKS PAYLOAD");
    assert!(!body_text(&body).contains("DETAIL PAYLOAD"));

    let (status, headers, body) = send(app.clone(), "/tasks/index.html").await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type(&headers).contains("text/html"));
    assert_eq!(body_text(&body), "TASKS SHELL");

    // A real task id still resolves to the detail preview, not the task list.
    let (status, headers, body) = send(app, &format!("/tasks/{TASK_ID}.txt")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type(&headers).contains("text/plain"));
    assert_eq!(body_text(&body), "DETAIL PAYLOAD");
}

#[tokio::test]
async fn next_payload_aliases_serve_the_matching_preview_text() {
    let (_dir, root) = fixture();
    let app = app_with(&root);
    let cases = [
        (format!("/tasks/{TASK_ID}.txt"), "DETAIL PAYLOAD"),
        (format!("/tasks/{TASK_ID}/index.txt"), "DETAIL PAYLOAD"),
        (format!("/tasks/{TASK_ID}/work.txt"), "WORK PAYLOAD"),
        (format!("/tasks/{TASK_ID}/work/index.txt"), "WORK PAYLOAD"),
        (format!("/tasks/{TASK_ID}/graph.txt"), "GRAPH PAYLOAD"),
        (format!("/tasks/{TASK_ID}/graph/index.txt"), "GRAPH PAYLOAD"),
        (
            format!("/tasks/{TASK_ID}/__next.tasks.$d$id.__PAGE__.txt"),
            "DETAIL NEXT PAYLOAD",
        ),
        (
            format!("/tasks/{TASK_ID}/work/__next.tasks.$d$id.work.__PAGE__.txt"),
            "WORK NEXT PAYLOAD",
        ),
        (
            format!("/tasks/{TASK_ID}/graph/__next.tasks.$d$id.graph.__PAGE__.txt"),
            "GRAPH NEXT PAYLOAD",
        ),
    ];

    for (uri, marker) in cases {
        let (status, headers, body) = send(app.clone(), &uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert!(content_type(&headers).contains("text/plain"), "{uri}");
        assert_eq!(body_text(&body), marker, "{uri}");
    }
}

#[tokio::test]
async fn missing_web_dir_keeps_the_api_and_leaves_root_unhandled() {
    let app = router_with_web_dir(app_state(), None);

    let (health_status, _, health_body) = send(app.clone(), "/api/health").await;
    assert_eq!(health_status, StatusCode::OK);
    let json: Value = serde_json::from_slice(&health_body).expect("health json");
    assert_eq!(json["status"], "ok");

    let (root_status, _, _) = send(app, "/").await;
    assert_eq!(root_status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn path_traversal_never_serves_a_sibling_secret() {
    let (_dir, root) = fixture();
    let app = app_with(&root);

    for uri in ["/%2e%2e/secret.txt", "/../secret.txt"] {
        let (_, _, body) = send(app.clone(), uri).await;
        assert!(
            !body_text(&body).contains("TOP SECRET"),
            "{uri} escaped the export root"
        );
    }
}

#[tokio::test]
async fn next_static_assets_are_served_with_a_javascript_content_type() {
    let (_dir, root) = fixture();
    let (status, headers, body) = send(app_with(&root), "/_next/static/chunks/app.js").await;

    assert_eq!(status, StatusCode::OK);
    assert!(content_type(&headers).contains("javascript"));
    assert_eq!(body_text(&body), "console.log('app');");
}
