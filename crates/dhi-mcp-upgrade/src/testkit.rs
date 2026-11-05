//! Reusable, in-process fake MCP endpoints for integration tests.
//!
//! This module is behind the optional `testkit` feature, which the crate's
//! self dev-dependency enables for tests. Production builds keep it off.
//!
//! The fakes prove header plumbing and JSON-RPC dispatch. They do **not** prove
//! that the real rmcp client works: the dhi-server integration test must drive
//! rmcp against this endpoint (or a copy of it) and assert the rmcp transcript.
//!
//! Nothing here contacts an external host. Each server binds `127.0.0.1:0` and
//! is aborted when its handle is dropped. Authorization failures never echo the
//! token.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

/// One tool advertised by the fake SSE server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeTool {
    pub name: String,
    pub description: String,
}

/// The tools every fake SSE server advertises.
pub fn default_tools() -> Vec<FakeTool> {
    vec![
        FakeTool {
            name: "echo".to_string(),
            description: "Echo the supplied arguments".to_string(),
        },
        FakeTool {
            name: "add".to_string(),
            description: "Add two numbers".to_string(),
        },
    ]
}

#[derive(Clone)]
struct StreamSink {
    sender: mpsc::UnboundedSender<Result<Event, Infallible>>,
    silent_calls: bool,
    delay_first_call: Arc<AtomicBool>,
}

#[derive(Clone)]
struct SseState {
    required_headers: Arc<BTreeMap<String, String>>,
    tools: Arc<Vec<FakeTool>>,
    sink: Arc<Mutex<Option<StreamSink>>>,
}

/// A loopback legacy SSE server.
pub struct FakeSseServer {
    pub base_url: String,
    task: JoinHandle<()>,
}

impl Drop for FakeSseServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Starts a fake SSE server that requires every header in `required_headers`
/// on both the GET stream and every POST.
///
/// Routes:
/// * `/sse` — normal handshake and SSE responses;
/// * `/sse-silent-endpoint` — stream opens, endpoint never arrives;
/// * `/sse-silent-calls` — handshake works, `tools/call` gets no response;
/// * `/sse-delayed-calls` — first `tools/call` is delayed for cancellation
///   tests;
/// * `/sse-post-json` — POST replies with a direct JSON-RPC response;
/// * `/sse-post-unexpected` — POST replies with `text/event-stream`.
pub async fn spawn_sse_server(required_headers: BTreeMap<String, String>) -> FakeSseServer {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind fake SSE server");
    let address = listener.local_addr().expect("fake SSE address");
    let state = SseState {
        required_headers: Arc::new(required_headers),
        tools: Arc::new(default_tools()),
        sink: Arc::new(Mutex::new(None)),
    };
    let app = Router::new()
        .route("/sse", get(sse))
        .route("/sse-silent-endpoint", get(sse_silent_endpoint))
        .route("/sse-silent-calls", get(sse_silent_calls))
        .route("/sse-delayed-calls", get(sse_delayed_calls))
        .route("/sse-post-json", get(sse_post_json))
        .route("/sse-post-unexpected", get(sse_post_unexpected))
        .route("/messages", post(messages))
        .route("/messages-json", post(messages_json))
        .route("/messages-unexpected", post(messages_unexpected))
        .with_state(state);
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    FakeSseServer {
        base_url: format!("http://{address}"),
        task,
    }
}

async fn sse(State(state): State<SseState>, headers: HeaderMap) -> Response {
    open_sse(
        state,
        headers,
        false,
        false,
        false,
        "/messages?session=fake",
    )
    .await
}

async fn sse_silent_endpoint(State(state): State<SseState>, headers: HeaderMap) -> Response {
    if !headers_match(&headers, &state.required_headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    Sse::new(stream::pending::<Result<Event, Infallible>>()).into_response()
}

async fn sse_silent_calls(State(state): State<SseState>, headers: HeaderMap) -> Response {
    open_sse(state, headers, false, true, false, "/messages?session=fake").await
}

async fn sse_delayed_calls(State(state): State<SseState>, headers: HeaderMap) -> Response {
    open_sse(state, headers, false, false, true, "/messages?session=fake").await
}

async fn sse_post_json(State(state): State<SseState>, headers: HeaderMap) -> Response {
    open_sse(
        state,
        headers,
        false,
        false,
        false,
        "/messages-json?session=fake",
    )
    .await
}

async fn sse_post_unexpected(State(state): State<SseState>, headers: HeaderMap) -> Response {
    open_sse(
        state,
        headers,
        false,
        false,
        false,
        "/messages-unexpected?session=fake",
    )
    .await
}

async fn open_sse(
    state: SseState,
    headers: HeaderMap,
    silent_endpoint: bool,
    silent_calls: bool,
    delay_first_call: bool,
    endpoint: &'static str,
) -> Response {
    if !headers_match(&headers, &state.required_headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    if silent_endpoint {
        return Sse::new(stream::pending::<Result<Event, Infallible>>()).into_response();
    }

    let (sender, receiver) = mpsc::unbounded_channel();
    *state.sink.lock().await = Some(StreamSink {
        sender: sender.clone(),
        silent_calls,
        delay_first_call: Arc::new(AtomicBool::new(delay_first_call)),
    });
    let _ = sender.send(Ok(Event::default().event("endpoint").data(endpoint)));
    let events = stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|event| (event, receiver))
    });
    Sse::new(events).into_response()
}

async fn messages(
    State(state): State<SseState>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response {
    if !headers_match(&headers, &state.required_headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let Some(id) = request.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let Some(sink) = state.sink.lock().await.clone() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no SSE stream").into_response();
    };
    if sink.silent_calls && method == "tools/call" {
        return StatusCode::ACCEPTED.into_response();
    }
    if method == "tools/call" && sink.delay_first_call.swap(false, Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let response = build_response(&state.tools, method, id, &request);
    let _ = sink.sender.send(Ok(Event::default()
        .event("message")
        .data(response.to_string())));
    StatusCode::ACCEPTED.into_response()
}

async fn messages_json(
    State(state): State<SseState>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response {
    if !headers_match(&headers, &state.required_headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let Some(id) = request.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    (
        StatusCode::OK,
        Json(build_response(&state.tools, method, id, &request)),
    )
        .into_response()
}

async fn messages_unexpected(
    State(state): State<SseState>,
    headers: HeaderMap,
    _request: Json<Value>,
) -> Response {
    if !headers_match(&headers, &state.required_headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/event-stream")],
        "event: message\ndata: {\"jsonrpc\":\"2.0\"}\n\n",
    )
        .into_response()
}

fn build_response(tools: &[FakeTool], method: &str, id: Value, request: &Value) -> Value {
    match method {
        "initialize" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "fake-sse", "version": "0.1.0" }
            }
        }),
        "tools/list" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "tools": tools
                    .iter()
                    .map(|tool| json!({
                        "name": tool.name,
                        "description": tool.description,
                        "inputSchema": { "type": "object", "properties": {} }
                    }))
                    .collect::<Vec<_>>()
            }
        }),
        "tools/call" => {
            let name = request
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if tools.iter().any(|tool| tool.name == name) {
                let arguments = request
                    .pointer("/params/arguments")
                    .cloned()
                    .unwrap_or(Value::Null);
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{
                            "type": "text",
                            "text": format!("called {name} with {arguments}")
                        }],
                        "isError": false
                    }
                })
            } else {
                jsonrpc_error(id, -32602, format!("unknown tool {name:?}"))
            }
        }
        other => jsonrpc_error(id, -32601, format!("unknown method {other:?}")),
    }
}

fn jsonrpc_error(id: Value, code: i64, message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

fn headers_match(headers: &HeaderMap, required: &BTreeMap<String, String>) -> bool {
    required.iter().all(|(name, expected)| {
        headers
            .get(name.as_str())
            .and_then(|value| value.to_str().ok())
            .map(|value| value == expected)
            .unwrap_or(false)
    })
}

#[derive(Clone)]
struct AuthState {
    token: Arc<String>,
}

/// A loopback HTTP MCP endpoint used to exercise bearer authentication and
/// JSON-RPC method dispatch.
///
/// This endpoint is deliberately small. It proves the client sent the expected
/// `Authorization` and `Accept` headers and that missing/wrong tokens fail; the
/// server integration test must still drive the real rmcp client.
pub struct FakeAuthMcp {
    pub base_url: String,
    task: JoinHandle<()>,
}

impl Drop for FakeAuthMcp {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Starts the auth-protected endpoint at `POST /mcp`.
pub async fn spawn_auth_mcp(token: &str) -> FakeAuthMcp {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind fake auth server");
    let address = listener.local_addr().expect("fake auth address");
    let app = Router::new()
        .route("/mcp", post(auth_mcp))
        .with_state(AuthState {
            token: Arc::new(token.to_string()),
        });
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    FakeAuthMcp {
        base_url: format!("http://{address}"),
        task,
    }
}

async fn auth_mcp(
    State(state): State<AuthState>,
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
        return (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response();
    }
    let accepts_json = headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_ascii_lowercase().contains("application/json"))
        .unwrap_or(false);
    if !accepts_json {
        return (
            StatusCode::NOT_ACCEPTABLE,
            "expected Accept: application/json",
        )
            .into_response();
    }

    let Some(id) = request.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let response = match method {
        "initialize" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "auth-fake", "version": "0.1.0" }
            }
        }),
        "tools/list" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "tools": [{
                    "name": "echo",
                    "description": "Echo arguments",
                    "inputSchema": { "type": "object", "properties": {} }
                }]
            }
        }),
        "tools/call" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "content": [{ "type": "text", "text": "authorized" }],
                "isError": false
            }
        }),
        other => jsonrpc_error(id, -32601, format!("unknown method {other:?}")),
    };
    (StatusCode::OK, Json(response)).into_response()
}
