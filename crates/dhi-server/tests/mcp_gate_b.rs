//! Gate B: live MCP manager reload plus the add/edit/delete HTTP API.
//!
//! Every test runs against a loopback fake MCP server (Streamable HTTP over a
//! real TCP listener) and a real `rmcp` client through the manager. No test
//! contacts an external host.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use dhi_agent::{AgentRuntime, LlmPool, MockAgentRuntime, ToolSink};
use dhi_core::{GraphStore, GuardStore, ToolCallOutcome, ToolCallRequest};
use dhi_engine::Engine;
use dhi_guard::{Policy, SharedPolicy};
use dhi_mcp::{McpFile, McpServerEntry};
use dhi_server::mcp::{McpManager, McpRolePlanner, McpWorkerToolView};
use dhi_server::{AppState, router};
use dhi_testkit::MemoryStore;
use http_body_util::BodyExt;
use rig::tool::server::{ToolServer, ToolServerHandle};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tower::ServiceExt;
use uuid::Uuid;

// -- loopback fake MCP server (real Streamable HTTP, real rmcp client) ------

#[derive(Clone)]
struct FakeState {
    token: Arc<String>,
    tools: Arc<tokio::sync::Mutex<Vec<(String, String)>>>,
}

/// A fake MCP endpoint with a mutable tool list and a required bearer token.
/// It answers JSON-RPC POSTs with `application/json`, which is exactly the
/// shape rmcp's Streamable HTTP client accepts.
struct FakeMcp {
    base_url: String,
    tools: Arc<tokio::sync::Mutex<Vec<(String, String)>>>,
    task: JoinHandle<()>,
}

impl Drop for FakeMcp {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeMcp {
    fn url(&self) -> String {
        format!("{}/mcp", self.base_url)
    }

    async fn set_tools(&self, tools: &[(&str, &str)]) {
        *self.tools.lock().await = tools
            .iter()
            .map(|(name, description)| ((*name).to_string(), (*description).to_string()))
            .collect();
    }
}

async fn spawn_fake_mcp(token: &str) -> FakeMcp {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind fake MCP");
    let address = listener.local_addr().expect("fake address");
    let tools = Arc::new(tokio::sync::Mutex::new(vec![(
        "echo".to_string(),
        "Echo the arguments".to_string(),
    )]));
    let state = FakeState {
        token: Arc::new(token.to_string()),
        tools: tools.clone(),
    };
    let app = Router::new()
        .route("/mcp", post(fake_mcp_handler))
        .with_state(state);
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    FakeMcp {
        base_url: format!("http://{address}"),
        tools,
        task,
    }
}

async fn fake_mcp_handler(
    State(state): State<FakeState>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response {
    let expected = format!("Bearer {}", state.token);
    let authorized = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(|value| value == expected)
        .unwrap_or(false);
    if !authorized {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let Some(id) = request.get("id").cloned() else {
        // notifications/initialized and friends
        return StatusCode::ACCEPTED.into_response();
    };
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let result = match method {
        "initialize" => json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "gate-b-fake", "version": "0.1.0" }
        }),
        "ping" => json!({}),
        "tools/list" => {
            let tools = state.tools.lock().await;
            json!({
                "tools": tools
                    .iter()
                    .map(|(name, description)| json!({
                        "name": name,
                        "description": description,
                        "inputSchema": { "type": "object", "properties": {} }
                    }))
                    .collect::<Vec<_>>()
            })
        }
        "tools/call" => {
            let name = request
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            json!({
                "content": [{ "type": "text", "text": format!("called {name}") }],
                "isError": false
            })
        }
        _ => {
            return (
                StatusCode::OK,
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": format!("unknown method {method:?}") }
                })),
            )
                .into_response();
        }
    };
    (
        StatusCode::OK,
        Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })),
    )
        .into_response()
}

// -- tool sink --------------------------------------------------------------

struct NoopSink;

#[async_trait]
impl ToolSink for NoopSink {
    async fn execute(
        &self,
        _task_id: Uuid,
        _intent_id: Uuid,
        _request: ToolCallRequest,
    ) -> anyhow::Result<ToolCallOutcome> {
        Ok(ToolCallOutcome::Allowed {
            output: json!({ "ok": true }),
        })
    }
}

// -- deployment + config helpers -------------------------------------------

fn http_entry(url: &str, token: &str, timeout_secs: u64) -> McpServerEntry {
    McpServerEntry {
        url: Some(url.to_string()),
        headers: BTreeMap::from([("Authorization".to_string(), format!("Bearer {token}"))]),
        timeout_secs: Some(timeout_secs),
        ..McpServerEntry::default()
    }
}

fn write_config(path: &Path, servers: &[(&str, McpServerEntry)]) {
    let mut file = McpFile::default();
    for (name, entry) in servers {
        file.servers.insert((*name).to_string(), entry.clone());
    }
    file.save(path).expect("save config");
}

async fn deploy(
    path: &Path,
    servers: &[(&str, McpServerEntry)],
) -> (Arc<McpManager>, ToolServerHandle) {
    write_config(path, servers);
    let manager = McpManager::from_config_path(path.to_path_buf()).expect("manager");
    let handle = ToolServer::new().run();
    manager.connect_all(&handle, Arc::new(NoopSink)).await;
    (manager, handle)
}

fn app_state(manager: Arc<McpManager>) -> AppState {
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
        mcp: manager,
        tool_catalog: Arc::new(dhi_tool_catalog::CatalogRuntime::default()),
        tool_control: None,
        web: Arc::new(dhi_server::web_providers::WebProviderRuntime::empty()),
        task_runs: Arc::new(dhi_server::TaskRunRegistry::disabled()),
        project_folders: Arc::new(dhi_server::project_folders::ProjectFolders::default()),
        web_projects: Arc::new(dhi_server::web_projects::WebProjects::default()),
        web_project_store: None,
        team_store: None,
        scale_gate: None,
    }
}

async fn guarded_defs(handle: &ToolServerHandle) -> Vec<String> {
    handle
        .tool_defs(None)
        .await
        .expect("tool defs")
        .into_iter()
        .map(|definition| definition.name.to_string())
        .collect()
}

async fn send(app: Router, request: Request<Body>) -> (StatusCode, Value) {
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

fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .body(Body::empty())
        .expect("request")
}

fn json_request(method: &str, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request")
}

fn empty_request(method: &str, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .expect("request")
}

// -- manager reload tests ---------------------------------------------------

#[tokio::test]
async fn live_reload_keeps_unchanged_connections_and_adds_new_servers() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("mcp.json");
    let alpha = spawn_fake_mcp("alpha-token").await;
    let beta = spawn_fake_mcp("beta-token").await;

    let (manager, handle) = deploy(
        &path,
        &[("alpha", http_entry(&alpha.url(), "alpha-token", 60))],
    )
    .await;
    let manager_identity = manager.clone();
    assert_eq!(
        manager.registry().names().await,
        vec!["mcp__alpha__echo".to_string()]
    );
    let alpha_stamp = manager
        .connection_stamp("alpha")
        .await
        .expect("alpha connected");

    // Unchanged: connection and guarded names stay exactly as they were.
    let report = manager.reload_from_disk().await.expect("unchanged reload");
    assert_eq!(report.kept, ["alpha"]);
    assert!(report.connected.is_empty());
    assert!(report.reconnected.is_empty());
    assert_eq!(
        manager.connection_stamp("alpha").await,
        Some(alpha_stamp),
        "an unchanged server keeps its live connection"
    );
    assert!(Arc::ptr_eq(&manager, &manager_identity), "same manager");

    // Add beta: alpha stays live, beta connects and registers its tool.
    write_config(
        &path,
        &[
            ("alpha", http_entry(&alpha.url(), "alpha-token", 60)),
            ("beta", http_entry(&beta.url(), "beta-token", 60)),
        ],
    );
    let report = manager.reload_from_disk().await.expect("add reload");
    assert_eq!(report.connected, ["beta"]);
    assert_eq!(report.kept, ["alpha"]);
    assert!(report.failed.is_empty(), "{report:?}");
    assert_eq!(manager.connection_stamp("alpha").await, Some(alpha_stamp));
    let beta_stamp = manager
        .connection_stamp("beta")
        .await
        .expect("beta connected");
    assert_ne!(beta_stamp, alpha_stamp);
    let names = manager.registry().names().await;
    assert!(names.contains(&"mcp__alpha__echo".to_string()), "{names:?}");
    assert!(names.contains(&"mcp__beta__echo".to_string()), "{names:?}");

    // Edit beta (timeout change) after changing the server's tools: it
    // reconnects, the old guarded name goes, the new one appears.
    beta.set_tools(&[("new_tool", "The replacement tool")])
        .await;
    write_config(
        &path,
        &[
            ("alpha", http_entry(&alpha.url(), "alpha-token", 60)),
            ("beta", http_entry(&beta.url(), "beta-token", 7)),
        ],
    );
    let report = manager.reload_from_disk().await.expect("edit reload");
    assert_eq!(report.reconnected, ["beta"]);
    assert_eq!(report.kept, ["alpha"]);
    assert!(report.failed.is_empty(), "{report:?}");
    assert_eq!(
        manager.connection_stamp("alpha").await,
        Some(alpha_stamp),
        "the untouched server did not reconnect"
    );
    assert_ne!(
        manager.connection_stamp("beta").await,
        Some(beta_stamp),
        "the changed server reconnected"
    );
    let names = manager.registry().names().await;
    assert!(
        names.contains(&"mcp__beta__new_tool".to_string()),
        "{names:?}"
    );
    assert!(
        !names.contains(&"mcp__beta__echo".to_string()),
        "the replaced guarded name must be gone: {names:?}"
    );
    let defs = guarded_defs(&handle).await;
    assert!(
        defs.contains(&"mcp__beta__new_tool".to_string()),
        "{defs:?}"
    );
    assert!(!defs.contains(&"mcp__beta__echo".to_string()), "{defs:?}");

    // Delete beta: connection closed, statuses and guarded names gone.
    write_config(
        &path,
        &[("alpha", http_entry(&alpha.url(), "alpha-token", 60))],
    );
    let report = manager.reload_from_disk().await.expect("delete reload");
    assert_eq!(report.disconnected, ["beta"]);
    assert_eq!(manager.connection_stamp("beta").await, None);
    assert_eq!(manager.tools("beta").await, None);
    assert!(
        manager
            .statuses()
            .await
            .iter()
            .all(|status| status.name != "beta")
    );
    let names = manager.registry().names().await;
    assert_eq!(names, vec!["mcp__alpha__echo".to_string()]);
    assert_eq!(manager.connection_stamp("alpha").await, Some(alpha_stamp));

    manager.shutdown().await;
}

#[tokio::test]
async fn failed_reconnect_removes_old_names_and_reports_a_clear_status() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("mcp.json");
    let alpha = spawn_fake_mcp("alpha-token").await;

    let (manager, handle) = deploy(
        &path,
        &[("alpha", http_entry(&alpha.url(), "alpha-token", 60))],
    )
    .await;
    assert!(
        manager
            .registry()
            .names()
            .await
            .contains(&"mcp__alpha__echo".to_string())
    );

    write_config(
        &path,
        &[(
            "alpha",
            http_entry("http://127.0.0.1:1/mcp", "alpha-token", 60),
        )],
    );
    let report = manager.reload_from_disk().await.expect("reload applies");
    assert_eq!(report.reconnected, Vec::<String>::new());
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].name, "alpha");
    assert!(manager.connection_stamp("alpha").await.is_none());
    assert!(
        manager.registry().names().await.is_empty(),
        "old guarded names must not survive a failed reconnect"
    );
    assert!(
        !guarded_defs(&handle)
            .await
            .contains(&"mcp__alpha__echo".to_string()),
        "the catalog entry must be retired"
    );
    let status = manager
        .statuses()
        .await
        .into_iter()
        .find(|status| status.name == "alpha")
        .expect("alpha status");
    assert!(!status.connected);
    assert!(status.tools.is_empty());
    assert!(
        status
            .last_error
            .as_deref()
            .is_some_and(|error| !error.is_empty()),
        "{status:?}"
    );

    // A later fix reconnects without a process restart.
    write_config(
        &path,
        &[("alpha", http_entry(&alpha.url(), "alpha-token", 60))],
    );
    let report = manager.reload_from_disk().await.expect("repair reload");
    assert_eq!(report.reconnected, ["alpha"]);
    assert!(manager.connection_stamp("alpha").await.is_some());
    assert!(
        manager
            .registry()
            .names()
            .await
            .contains(&"mcp__alpha__echo".to_string())
    );

    manager.shutdown().await;
}

#[tokio::test]
async fn a_failed_addition_leaves_no_stale_tools() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("mcp.json");
    let alpha = spawn_fake_mcp("alpha-token").await;
    let (manager, handle) = deploy(
        &path,
        &[("alpha", http_entry(&alpha.url(), "alpha-token", 60))],
    )
    .await;

    write_config(
        &path,
        &[
            ("alpha", http_entry(&alpha.url(), "alpha-token", 60)),
            (
                "dead",
                http_entry("http://127.0.0.1:1/mcp", "dead-token", 60),
            ),
        ],
    );
    let report = manager.reload_from_disk().await.expect("reload applies");
    assert_eq!(report.connected, Vec::<String>::new());
    assert_eq!(report.kept, ["alpha"]);
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].name, "dead");
    assert_eq!(
        manager.registry().names().await,
        vec!["mcp__alpha__echo".to_string()]
    );
    assert!(
        !guarded_defs(&handle)
            .await
            .iter()
            .any(|name| name.starts_with("mcp__dead__")),
        "a failed addition must register nothing"
    );
    let status = manager
        .statuses()
        .await
        .into_iter()
        .find(|status| status.name == "dead")
        .expect("dead status");
    assert!(!status.connected);
    assert!(status.last_error.is_some());

    manager.shutdown().await;
}

// -- API tests --------------------------------------------------------------

#[tokio::test]
async fn api_add_edit_delete_reload_and_masking() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("mcp.json");
    let alpha = spawn_fake_mcp("alpha-token").await;
    let beta = spawn_fake_mcp("beta-token").await;

    // The file carries an unknown top-level key and an unknown entry field on
    // a disabled server; both must survive every API write.
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&json!({
            "$schema": "dhi.mcp.v1",
            "mcpServers": {
                "alpha": {
                    "url": alpha.url(),
                    "headers": {
                        "Authorization": "Bearer alpha-token",
                        "X-Tenant": "acme"
                    },
                    "timeout_secs": 60
                },
                "parked": {
                    "command": "npx",
                    "x-note": "keep-me",
                    "enabled": false
                }
            }
        }))
        .expect("json"),
    )
    .expect("write raw config");

    let manager = McpManager::from_config_path(path.clone()).expect("manager");
    let handle = ToolServer::new().run();
    manager.connect_all(&handle, Arc::new(NoopSink)).await;
    let app = router(app_state(manager.clone()));

    // List: status plus a masked config view.
    let (status, body) = send(app.clone(), get("/api/mcp/servers")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["servers"][0]["name"], "alpha");
    assert_eq!(body["config"][0]["name"], "alpha");
    assert_eq!(body["config"][0]["headers"]["Authorization"], "***");
    assert_eq!(body["config"][0]["headers"]["X-Tenant"], "acme");
    assert!(!body.to_string().contains("alpha-token"), "{body}");

    // PATCH is accepted for an edit; an unchanged body keeps the live
    // connection and returns the masked summary.
    let (status, body) = send(
        app.clone(),
        json_request(
            "PATCH",
            "/api/mcp/servers/alpha",
            json!({
                "url": alpha.url(),
                "headers": {
                    "Authorization": "Bearer alpha-token",
                    "X-Tenant": "acme"
                },
                "timeout_secs": 60
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["server"]["headers"]["Authorization"], "***");
    assert!(!body.to_string().contains("alpha-token"), "{body}");
    assert_eq!(body["reload"]["kept"], json!(["alpha"]));

    // Add beta (secret header must be persisted but never returned).
    let (status, body) = send(
        app.clone(),
        json_request(
            "POST",
            "/api/mcp/servers",
            json!({
                "name": "beta",
                "url": beta.url(),
                "headers": {
                    "Authorization": "Bearer beta-token",
                    "X-Tenant": "beta-tenant"
                },
                "timeout_secs": 60
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["server"]["name"], "beta");
    assert_eq!(body["server"]["headers"]["Authorization"], "***");
    assert_eq!(body["server"]["headers"]["X-Tenant"], "beta-tenant");
    assert!(!body.to_string().contains("beta-token"), "{body}");
    assert_eq!(body["reload"]["connected"], json!(["beta"]));
    assert!(
        manager
            .registry()
            .names()
            .await
            .contains(&"mcp__beta__echo".to_string())
    );

    let raw = std::fs::read_to_string(&path).expect("read config");
    assert!(raw.contains("\"$schema\""), "{raw}");
    assert!(raw.contains("\"x-note\""), "{raw}");
    assert!(raw.contains("keep-me"), "{raw}");
    assert!(raw.contains("\"beta\""), "{raw}");
    assert!(raw.contains("beta-token"), "the secret is persisted: {raw}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "API writes keep the config private");
    }

    // Duplicate, invalid, and missing-target errors.
    let (status, _) = send(
        app.clone(),
        json_request(
            "POST",
            "/api/mcp/servers",
            json!({"name": "beta", "command": "npx"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, body) = send(
        app.clone(),
        json_request(
            "POST",
            "/api/mcp/servers",
            json!({"name": "@@@", "command": "npx"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = send(
        app.clone(),
        json_request("POST", "/api/mcp/servers", json!({"name": "missing"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = send(
        app.clone(),
        json_request(
            "POST",
            "/api/mcp/servers",
            json!({"name": "sse", "transport": "sse", "url": "https://example.test/sse"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    let (status, _) = send(
        app.clone(),
        json_request("DELETE", "/api/mcp/servers/sse", json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = send(
        app.clone(),
        json_request(
            "POST",
            "/api/mcp/servers",
            Value::String("not an object".into()),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // Edit beta: new tools appear, the old guarded name is retired, and the
    // connection stamp changes.
    let beta_stamp = manager.connection_stamp("beta").await.expect("beta");
    beta.set_tools(&[("new_tool", "Replacement")]).await;
    let (status, body) = send(
        app.clone(),
        json_request(
            "PUT",
            "/api/mcp/servers/beta",
            json!({
                "url": beta.url(),
                "headers": { "Authorization": "Bearer beta-token" },
                "timeout_secs": 7
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["server"]["name"], "beta");
    assert!(!body.to_string().contains("beta-token"), "{body}");
    assert_eq!(body["reload"]["reconnected"], json!(["beta"]));
    assert_ne!(manager.connection_stamp("beta").await, Some(beta_stamp));
    let names = manager.registry().names().await;
    assert!(
        names.contains(&"mcp__beta__new_tool".to_string()),
        "{names:?}"
    );
    assert!(!names.contains(&"mcp__beta__echo".to_string()), "{names:?}");

    // Unknown edit, name mismatch, and unknown delete.
    let (status, _) = send(
        app.clone(),
        json_request("PUT", "/api/mcp/servers/missing", json!({"command": "npx"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) = send(
        app.clone(),
        json_request(
            "PUT",
            "/api/mcp/servers/beta",
            json!({"name": "other", "command": "npx"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, _) = send(
        app.clone(),
        empty_request("DELETE", "/api/mcp/servers/missing"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Add a dead server: the write succeeds but the connect failure is 502.
    let (status, body) = send(
        app.clone(),
        json_request(
            "POST",
            "/api/mcp/servers",
            json!({"name": "dead", "url": "http://127.0.0.1:1/mcp"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert!(
        manager
            .statuses()
            .await
            .iter()
            .any(|status| status.name == "dead"
                && !status.connected
                && status.last_error.is_some())
    );
    let (status, _) = send(
        app.clone(),
        empty_request("DELETE", "/api/mcp/servers/dead"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Delete beta: connection, status, and guarded names all go.
    let (status, body) = send(
        app.clone(),
        empty_request("DELETE", "/api/mcp/servers/beta"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["deleted"], "beta");
    assert_eq!(manager.connection_stamp("beta").await, None);
    assert!(
        manager
            .registry()
            .names()
            .await
            .iter()
            .all(|name| !name.starts_with("mcp__beta__"))
    );
    assert!(
        !std::fs::read_to_string(&path)
            .expect("read")
            .contains("\"beta\"")
    );
    assert!(
        std::fs::read_to_string(&path)
            .expect("read")
            .contains("keep-me")
    );

    // Explicit reload from disk.
    let (status, body) = send(app.clone(), empty_request("POST", "/api/mcp/reload")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["reload"]["kept"], json!(["alpha"]));
    assert_eq!(body["reload"]["runtime_attached"], true);
    assert_eq!(body["reload"]["configured"], 1);

    // Invalid JSON still has the one JSON error shape.
    let (status, body) = send(
        app.clone(),
        Request::builder()
            .method("POST")
            .uri("/api/mcp/servers")
            .header("content-type", "application/json")
            .body(Body::from("{not json"))
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["error"].is_string(), "{body}");

    manager.shutdown().await;
}

#[tokio::test]
async fn live_add_refreshes_the_worker_view_without_restart() {
    let alpha = spawn_fake_mcp("alpha-token").await;
    let beta = spawn_fake_mcp("beta-token").await;
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("mcp.json");
    let (manager, handle) = deploy(
        &path,
        &[("alpha", http_entry(&alpha.url(), "alpha-token", 60))],
    )
    .await;

    let planner = Arc::new(RwLock::new(McpRolePlanner::from_config_path(
        Some(&path),
        &manager.registry().snapshot().await,
    )));
    manager.set_role_planner(Arc::clone(&planner));
    let view = McpWorkerToolView::new(&handle, planner);
    assert!(
        view.visible_names()
            .contains(&"mcp__alpha__echo".to_string()),
        "{:?}",
        view.visible_names()
    );

    let app = router(app_state(manager.clone()));
    let (status, body) = send(
        app,
        json_request(
            "POST",
            "/api/mcp/servers",
            json!({
                "name": "beta",
                "url": beta.url(),
                "headers": { "Authorization": "Bearer beta-token" },
                "timeout_secs": 60
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(
        manager
            .registry()
            .names()
            .await
            .contains(&"mcp__beta__echo".to_string())
    );

    let mut names = view.visible_names();
    names.sort();
    assert_eq!(names, ["mcp__alpha__echo", "mcp__beta__echo"]);

    manager.shutdown().await;
}
