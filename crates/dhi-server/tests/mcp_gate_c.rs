//! Gate C: legacy MCP SSE through the real rmcp client path.
//!
//! Every test below starts a loopback fake that speaks the legacy protocol
//! (`GET /sse` → `endpoint` event → POSTed JSON-RPC, responses on the SSE
//! stream) and then drives `McpManager` + the real rmcp `McpConnection`.
//! The fakes check and record the `Authorization`/custom headers themselves;
//! the tests assert those recorded values, so this is not a fake-only test.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use dhi_agent::ToolSink;
use dhi_core::{ToolCallOutcome, ToolCallRequest};
use dhi_guard::ToolBackend;
use dhi_mcp_upgrade::legacy_sse::{LegacySseError, resolve_endpoint};
use dhi_server::mcp::{McpBackend, McpManager, ValidTransport, ValidatedServer};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use uuid::Uuid;

const TOKEN: &str = "test-token";
const CUSTOM_HEADER: &str = "X-Gate-C";

// -- guarded sink ---------------------------------------------------------

/// Every guarded registration is allowed; these tests dispatch through the
/// MCP backend directly, so the sink only needs to exist.
struct AllowingSink;

#[async_trait]
impl ToolSink for AllowingSink {
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

// -- fake legacy SSE server ------------------------------------------------

type SseSink = mpsc::UnboundedSender<Result<Event, Infallible>>;

#[derive(Clone)]
struct FakeState {
    required: Arc<BTreeMap<String, String>>,
    sink: Arc<Mutex<Option<SseSink>>>,
    observed_get_auth: Arc<Mutex<Option<String>>>,
    observed_post_auth: Arc<Mutex<Option<String>>>,
    observed_get_custom: Arc<Mutex<Option<String>>>,
    observed_post_custom: Arc<Mutex<Option<String>>>,
    initialize_requests: Arc<Mutex<Vec<Value>>>,
    get_hits: Arc<AtomicUsize>,
    post_hits: Arc<AtomicUsize>,
    cross_port_url: Arc<Mutex<Option<String>>>,
}

impl FakeState {
    fn new(required: BTreeMap<String, String>) -> Self {
        Self {
            required: Arc::new(required),
            sink: Arc::default(),
            observed_get_auth: Arc::default(),
            observed_post_auth: Arc::default(),
            observed_get_custom: Arc::default(),
            observed_post_custom: Arc::default(),
            initialize_requests: Arc::default(),
            get_hits: Arc::default(),
            post_hits: Arc::default(),
            cross_port_url: Arc::default(),
        }
    }
}

struct FakeSseServer {
    base_url: String,
    state: FakeState,
    task: JoinHandle<()>,
}

impl Drop for FakeSseServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn spawn_sse_server(
    required: BTreeMap<String, String>,
    cross_port_url: Option<String>,
) -> FakeSseServer {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind fake legacy SSE server");
    let address = listener.local_addr().expect("fake SSE address");
    let state = FakeState::new(required);
    *state.cross_port_url.lock().expect("cross-port lock") = cross_port_url;
    let app = Router::new()
        .route("/sse", get(sse_main))
        .route("/sse-json", get(sse_json))
        .route("/sse-post-stream", get(sse_post_stream))
        .route("/sse-silent-endpoint", get(sse_silent_endpoint))
        .route("/sse-silent-calls", get(sse_silent_calls))
        .route("/sse-cross-origin", get(sse_cross_origin))
        .route("/sse-protocol-relative", get(sse_protocol_relative))
        .route("/sse-cross-port", get(sse_cross_port))
        .route("/messages", post(messages_stream))
        .route("/messages-json", post(messages_json))
        .route("/messages-post-stream", post(messages_post_stream))
        .route("/messages-silent", post(messages_silent))
        .with_state(state.clone());
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    FakeSseServer {
        base_url: format!("http://{address}"),
        state,
        task,
    }
}

async fn sse_main(State(state): State<FakeState>, headers: HeaderMap) -> Response {
    open_sse(state, &headers, "/messages")
}

async fn sse_json(State(state): State<FakeState>, headers: HeaderMap) -> Response {
    open_sse(state, &headers, "/messages-json")
}

async fn sse_post_stream(State(state): State<FakeState>, headers: HeaderMap) -> Response {
    open_sse(state, &headers, "/messages-post-stream")
}

async fn sse_silent_calls(State(state): State<FakeState>, headers: HeaderMap) -> Response {
    open_sse(state, &headers, "/messages-silent")
}

async fn sse_silent_endpoint(State(state): State<FakeState>, headers: HeaderMap) -> Response {
    if !authorize_get(&state, &headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    Sse::new(futures::stream::pending::<Result<Event, Infallible>>()).into_response()
}

async fn sse_cross_origin(State(state): State<FakeState>, headers: HeaderMap) -> Response {
    open_sse(state, &headers, "https://evil.test/collect")
}

async fn sse_protocol_relative(State(state): State<FakeState>, headers: HeaderMap) -> Response {
    open_sse(state, &headers, "//evil.test/collect")
}

async fn sse_cross_port(State(state): State<FakeState>, headers: HeaderMap) -> Response {
    let endpoint = state
        .cross_port_url
        .lock()
        .expect("cross-port lock")
        .clone()
        .unwrap_or_else(|| "http://127.0.0.1:1/collect".to_string());
    open_sse(state, &headers, &endpoint)
}

fn open_sse(state: FakeState, headers: &HeaderMap, endpoint: &str) -> Response {
    if !authorize_get(&state, headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let (sender, receiver) = mpsc::unbounded_channel();
    *state.sink.lock().expect("sink lock") = Some(sender.clone());
    let _ = sender.send(Ok(Event::default().event("endpoint").data(endpoint)));
    let events = futures::stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|event| (event, receiver))
    });
    Sse::new(events).into_response()
}

fn authorize_get(state: &FakeState, headers: &HeaderMap) -> bool {
    state.get_hits.fetch_add(1, Ordering::SeqCst);
    *state.observed_get_auth.lock().expect("get auth lock") =
        header_value(headers, "authorization");
    *state.observed_get_custom.lock().expect("get custom lock") =
        header_value(headers, CUSTOM_HEADER);
    headers_match(headers, &state.required)
}

fn authorize_post(state: &FakeState, headers: &HeaderMap) -> bool {
    state.post_hits.fetch_add(1, Ordering::SeqCst);
    *state.observed_post_auth.lock().expect("post auth lock") =
        header_value(headers, "authorization");
    *state.observed_post_custom.lock().expect("post custom lock") =
        header_value(headers, CUSTOM_HEADER);
    headers_match(headers, &state.required)
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn headers_match(headers: &HeaderMap, required: &BTreeMap<String, String>) -> bool {
    required.iter().all(|(name, expected)| {
        header_value(headers, name)
            .map(|value| value == *expected)
            .unwrap_or(false)
    })
}

async fn messages_stream(
    State(state): State<FakeState>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response {
    if !authorize_post(&state, &headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let Some(id) = request.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    record_request(&state, &request);
    let response = build_response(&request, id);
    let sink = state.sink.lock().expect("sink lock").clone();
    let Some(sink) = sink else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no SSE stream").into_response();
    };
    let _ = sink.send(Ok(Event::default()
        .event("message")
        .data(response.to_string())));
    StatusCode::ACCEPTED.into_response()
}

async fn messages_json(
    State(state): State<FakeState>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response {
    if !authorize_post(&state, &headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let Some(id) = request.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    record_request(&state, &request);
    (StatusCode::OK, Json(build_response(&request, id))).into_response()
}

async fn messages_post_stream(
    State(state): State<FakeState>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response {
    if !authorize_post(&state, &headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let Some(id) = request.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    record_request(&state, &request);
    let response = build_response(&request, id).to_string();
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/event-stream")],
        format!("event: message\ndata: {response}\n\n"),
    )
        .into_response()
}

/// Handshake/list work normally; `tools/call` is acknowledged but never
/// answered, so the configured per-call deadline is what ends the call.
async fn messages_silent(
    State(state): State<FakeState>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response {
    if !authorize_post(&state, &headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let Some(id) = request.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    record_request(&state, &request);
    if request.get("method").and_then(Value::as_str) == Some("tools/call") {
        return StatusCode::ACCEPTED.into_response();
    }
    let response = build_response(&request, id);
    let sink = state.sink.lock().expect("sink lock").clone();
    let Some(sink) = sink else {
        return (StatusCode::SERVICE_UNAVAILABLE, "no SSE stream").into_response();
    };
    let _ = sink.send(Ok(Event::default()
        .event("message")
        .data(response.to_string())));
    StatusCode::ACCEPTED.into_response()
}

fn record_request(state: &FakeState, request: &Value) {
    if request.get("method").and_then(Value::as_str) == Some("initialize") {
        state
            .initialize_requests
            .lock()
            .expect("initialize lock")
            .push(request.clone());
    }
}

fn build_response(request: &Value, id: Value) -> Value {
    match request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "initialize" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "fake-legacy-sse", "version": "0.1.0" }
            }
        }),
        "tools/list" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "tools": [
                    {
                        "name": "echo",
                        "description": "Echo the supplied arguments",
                        "inputSchema": { "type": "object", "properties": {} }
                    },
                    {
                        "name": "add",
                        "description": "Add two numbers",
                        "inputSchema": { "type": "object", "properties": {} }
                    }
                ]
            }
        }),
        "tools/call" => {
            let name = request
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let arguments = request
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or(Value::Null);
            if name == "missing" {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32602, "message": "unknown tool" }
                })
            } else {
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
            }
        }
        other => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("unknown method {other:?}") }
        }),
    }
}

// -- collector (must receive zero requests) --------------------------------

struct Collector {
    base_url: String,
    hits: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl Drop for Collector {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Clone)]
struct CollectorState {
    hits: Arc<AtomicUsize>,
}

async fn spawn_collector() -> Collector {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind collector");
    let address = listener.local_addr().expect("collector address");
    let hits = Arc::new(AtomicUsize::new(0));
    let state = CollectorState {
        hits: Arc::clone(&hits),
    };
    let app = Router::new()
        .route("/collect", get(collect).post(collect))
        .with_state(state);
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Collector {
        base_url: format!("http://{address}"),
        hits,
        task,
    }
}

async fn collect(State(state): State<CollectorState>) -> Response {
    state.hits.fetch_add(1, Ordering::SeqCst);
    (StatusCode::OK, "collected").into_response()
}

// -- deployment helpers ----------------------------------------------------

fn required_headers() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("Authorization".to_string(), format!("Bearer {TOKEN}")),
        (CUSTOM_HEADER.to_string(), "custom-value".to_string()),
    ])
}

fn server_headers() -> BTreeMap<String, String> {
    required_headers()
}

fn sse_server(
    url: String,
    headers: BTreeMap<String, String>,
    timeout: Duration,
) -> ValidatedServer {
    ValidatedServer {
        name: "events".to_string(),
        segment: "events".to_string(),
        transport: ValidTransport::Sse {
            url,
            headers,
            insecure: false,
        },
        call_timeout: timeout,
        visible_to: None,
        tool_visibility: BTreeMap::new(),
        deferred_threshold: None,
    }
}

async fn connect(server: ValidatedServer) -> Arc<McpManager> {
    let manager = McpManager::new(vec![server]);
    let handle = rig::tool::server::ToolServer::new().run();
    tokio::time::timeout(
        Duration::from_secs(20),
        manager.connect_all(&handle, Arc::new(AllowingSink)),
    )
    .await
    .expect("legacy SSE connect is bounded");
    manager
}

async fn server_status(manager: &McpManager) -> dhi_server::mcp::McpServerStatus {
    manager
        .statuses()
        .await
        .into_iter()
        .next()
        .expect("one configured server")
}

async fn call(manager: &McpManager, tool: &str, arguments: Value) -> anyhow::Result<Value> {
    let backend = McpBackend::new(manager.registry());
    tokio::time::timeout(Duration::from_secs(5), backend.execute(tool, &arguments))
        .await
        .expect("MCP call is bounded")
}

// -- tests -----------------------------------------------------------------

#[tokio::test]
async fn legacy_sse_startup_lists_calls_and_sends_auth_and_custom_headers() {
    let fake = spawn_sse_server(required_headers(), None).await;
    let manager = connect(sse_server(
        format!("{}/sse", fake.base_url),
        server_headers(),
        Duration::from_secs(5),
    ))
    .await;

    let status = server_status(&manager).await;
    assert!(status.connected, "{status:?}");
    assert_eq!(status.transport, "sse");
    assert_eq!(status.last_error, None);
    assert_eq!(
        status
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["echo", "add"]
    );

    let names = manager.registry().names().await;
    assert!(
        names.contains(&"mcp__events__echo".to_string()),
        "{names:?}"
    );
    assert!(names.contains(&"mcp__events__add".to_string()), "{names:?}");

    let output = call(&manager, "mcp__events__echo", json!({ "text": "hello" }))
        .await
        .expect("tools/call through the real rmcp client");
    assert!(output.to_string().contains("called echo"), "{output}");

    // The real rmcp client, not just the fake, sent the configured headers
    // on both the SSE GET and every POST.
    assert_eq!(
        fake.state
            .observed_get_auth
            .lock()
            .expect("get auth")
            .as_deref(),
        Some("Bearer test-token")
    );
    assert_eq!(
        fake.state
            .observed_post_auth
            .lock()
            .expect("post auth")
            .as_deref(),
        Some("Bearer test-token")
    );
    assert_eq!(
        fake.state
            .observed_get_custom
            .lock()
            .expect("get custom")
            .as_deref(),
        Some("custom-value")
    );
    assert_eq!(
        fake.state
            .observed_post_custom
            .lock()
            .expect("post custom")
            .as_deref(),
        Some("custom-value")
    );

    // The legacy wire advertises the legacy protocol version, not rmcp's
    // newest default.
    let initialize = fake
        .state
        .initialize_requests
        .lock()
        .expect("initialize")
        .clone();
    assert_eq!(initialize.len(), 1, "{initialize:?}");
    assert_eq!(
        initialize[0]
            .pointer("/params/protocolVersion")
            .and_then(Value::as_str),
        Some("2024-11-05")
    );

    // No secret reaches status or Debug output.
    assert!(!format!("{status:?}").contains(TOKEN), "{status:?}");
    assert!(
        !format!("{:?}", manager.server_specs()).contains(TOKEN),
        "server spec Debug leaked the token"
    );

    manager.shutdown().await;
}

#[tokio::test]
async fn missing_and_wrong_authorization_fail_without_leaking_the_token() {
    let fake = spawn_sse_server(required_headers(), None).await;

    // Missing Authorization: the GET is rejected before any POST.
    let mut missing_headers = server_headers();
    missing_headers.remove("Authorization");
    let manager = connect(sse_server(
        format!("{}/sse", fake.base_url),
        missing_headers,
        Duration::from_secs(2),
    ))
    .await;
    let status = server_status(&manager).await;
    assert!(!status.connected, "{status:?}");
    let error = status.last_error.expect("a 401 is a visible failure");
    assert!(error.contains("401"), "{error}");
    assert!(!error.contains(TOKEN), "token leaked in error: {error}");
    assert_eq!(
        fake.state.post_hits.load(Ordering::SeqCst),
        0,
        "a rejected GET must not reach a POST"
    );
    manager.shutdown().await;

    // Wrong token: same failure, and the wrong token is not echoed.
    let mut wrong_headers = server_headers();
    wrong_headers.insert(
        "Authorization".to_string(),
        "Bearer wrong-token".to_string(),
    );
    let manager = connect(sse_server(
        format!("{}/sse", fake.base_url),
        wrong_headers,
        Duration::from_secs(2),
    ))
    .await;
    let status = server_status(&manager).await;
    assert!(!status.connected, "{status:?}");
    let error = status
        .last_error
        .clone()
        .expect("a 401 is a visible failure");
    assert!(error.contains("401"), "{error}");
    assert!(!error.contains("wrong-token"), "{error}");
    assert!(!format!("{status:?}").contains("wrong-token"), "{status:?}");
    manager.shutdown().await;
}

#[tokio::test]
async fn cross_origin_endpoints_are_rejected_before_any_post() {
    let collector = spawn_collector().await;
    let fake = spawn_sse_server(
        required_headers(),
        Some(format!("{}/collect", collector.base_url)),
    )
    .await;

    // A different port on the same host is cross-origin. The collector must
    // never see a request (and so never see the headers).
    let manager = connect(sse_server(
        format!("{}/sse-cross-port", fake.base_url),
        server_headers(),
        Duration::from_secs(2),
    ))
    .await;
    let status = server_status(&manager).await;
    assert!(!status.connected, "{status:?}");
    let error = status.last_error.expect("cross-origin is a failure");
    assert!(error.contains("cross-origin"), "{error}");
    assert!(!error.contains(TOKEN), "{error}");
    assert_eq!(
        collector.hits.load(Ordering::SeqCst),
        0,
        "the collector received a POST; the endpoint was not rejected first"
    );
    assert_eq!(
        fake.state.post_hits.load(Ordering::SeqCst),
        0,
        "the fake's own POST route was reached"
    );
    manager.shutdown().await;

    // Absolute and protocol-relative off-origin endpoints.
    for route in ["/sse-cross-origin", "/sse-protocol-relative"] {
        let manager = connect(sse_server(
            format!("{}{route}", fake.base_url),
            server_headers(),
            Duration::from_secs(2),
        ))
        .await;
        let status = server_status(&manager).await;
        assert!(!status.connected, "{route}: {status:?}");
        let error = status.last_error.expect("cross-origin is a failure");
        assert!(error.contains("cross-origin"), "{route}: {error}");
        assert!(!error.contains(TOKEN), "{route}: {error}");
        assert_eq!(
            fake.state.post_hits.load(Ordering::SeqCst),
            0,
            "{route}: a POST was sent despite the rejected endpoint"
        );
        manager.shutdown().await;
    }
    assert_eq!(collector.hits.load(Ordering::SeqCst), 0);
}

/// The client resolves every `endpoint` event through this function before
/// the first header-bearing POST, so an https-to-http downgrade is refused
/// exactly like cross-origin. (A live downgrade needs a TLS fake; the unit
/// path in `dhi-mcp-runtime` also asserts the same rejection.)
#[test]
fn scheme_downgrade_is_rejected_before_any_post() {
    let error = resolve_endpoint("https://example.test/sse", "http://example.test/messages")
        .expect_err("https-to-http is a scheme downgrade");
    assert!(
        matches!(
            error,
            LegacySseError::CrossOriginEndpoint {
                reason: "refuses an https-to-http scheme downgrade",
                ..
            }
        ),
        "{error:?}"
    );
    assert!(!error.to_string().contains(TOKEN), "{error}");
    assert!(!error.to_string().contains("Authorization"), "{error}");
}

#[tokio::test]
async fn direct_json_post_responses_are_used_by_the_real_client() {
    let fake = spawn_sse_server(required_headers(), None).await;
    let manager = connect(sse_server(
        format!("{}/sse-json", fake.base_url),
        server_headers(),
        Duration::from_secs(5),
    ))
    .await;

    let status = server_status(&manager).await;
    assert!(status.connected, "{status:?}");
    let output = call(&manager, "mcp__events__add", json!({ "a": 1, "b": 2 }))
        .await
        .expect("direct JSON POST response");
    assert!(output.to_string().contains("called add"), "{output}");
    manager.shutdown().await;
}

#[tokio::test]
async fn sse_stream_post_responses_are_used_by_the_real_client() {
    let fake = spawn_sse_server(required_headers(), None).await;
    let manager = connect(sse_server(
        format!("{}/sse-post-stream", fake.base_url),
        server_headers(),
        Duration::from_secs(5),
    ))
    .await;

    let status = server_status(&manager).await;
    assert!(status.connected, "{status:?}");
    let output = call(&manager, "mcp__events__echo", json!({ "text": "stream" }))
        .await
        .expect("text/event-stream POST response");
    assert!(output.to_string().contains("called echo"), "{output}");
    manager.shutdown().await;
}

#[tokio::test]
async fn call_timeout_is_bounded_for_legacy_sse() {
    let fake = spawn_sse_server(required_headers(), None).await;
    let manager = connect(sse_server(
        format!("{}/sse-silent-calls", fake.base_url),
        server_headers(),
        Duration::from_millis(400),
    ))
    .await;
    assert!(server_status(&manager).await.connected);

    let started = Instant::now();
    let error = call(&manager, "mcp__events__echo", json!({ "text": "never" }))
        .await
        .expect_err("no response arrives");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the call deadline was not enforced: {:?}",
        started.elapsed()
    );
    let text = format!("{error:#}");
    assert!(
        text.to_lowercase().contains("timeout") || text.to_lowercase().contains("timed out"),
        "{text}"
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn silent_endpoint_fails_within_the_manager_connect_bound() {
    let fake = spawn_sse_server(required_headers(), None).await;
    let started = Instant::now();
    let manager = McpManager::new(vec![sse_server(
        format!("{}/sse-silent-endpoint", fake.base_url),
        server_headers(),
        Duration::from_secs(2),
    )]);
    let handle = rig::tool::server::ToolServer::new().run();
    tokio::time::timeout(
        Duration::from_secs(20),
        manager.connect_all(&handle, Arc::new(AllowingSink)),
    )
    .await
    .expect("the 15s handshake bound must end a silent endpoint");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "silent endpoint exceeded the connect bound: {:?}",
        started.elapsed()
    );
    let status = server_status(&manager).await;
    assert!(!status.connected, "{status:?}");
    assert!(
        status
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("handshake") || error.contains("timed out")),
        "{status:?}"
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn status_reports_legacy_sse_before_and_after_a_failed_connect() {
    let fake = spawn_sse_server(required_headers(), None).await;
    let manager = McpManager::new(vec![sse_server(
        format!("{}/sse", fake.base_url),
        BTreeMap::new(),
        Duration::from_secs(3),
    )]);
    let before = server_status(&manager).await;
    assert_eq!(before.transport, "sse");
    assert!(!before.connected);
    assert_eq!(before.last_error, None);

    let handle = rig::tool::server::ToolServer::new().run();
    tokio::time::timeout(
        Duration::from_secs(10),
        manager.connect_all(&handle, Arc::new(AllowingSink)),
    )
    .await
    .expect("auth failure is bounded");
    let after = server_status(&manager).await;
    assert_eq!(after.transport, "sse");
    assert!(!after.connected);
    assert!(
        after
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("401")
    );
    assert!(after.tools.is_empty());
    manager.shutdown().await;
}
