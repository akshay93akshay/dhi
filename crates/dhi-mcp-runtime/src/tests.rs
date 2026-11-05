use super::config::{MAX_SERVER_SEGMENT_CHARS, MAX_TOOL_NAME_CHARS, guarded_name};
use super::transport::{http_headers, transport_label};
use super::*;
use async_trait::async_trait;
use dhi_agent::{RunScope, ToolSink};
use dhi_core::ToolCallOutcome;
use dhi_guard::ToolBackend;
use rig::tool::rmcp::McpConnection;
use rig::tool::server::{ToolServer, ToolServerHandle};
use rig::tool::{ToolContext, ToolExecutionError, ToolOutput};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, Implementation, JsonObject,
    ListToolsResult, PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerConfig,
    Tool,
};
use rmcp::service::{Peer, RequestContext};
use rmcp::{RoleServer, ServerHandler, ServiceExt};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::time::Duration;
use uuid::Uuid;

// -- fake MCP server (in process, over tokio duplex; never blocks) ------

/// An MCP server whose tool list is mutable at runtime (for the
/// list-changed test), whose calls are recorded, and whose answers can
/// be flipped to `is_error`. Mirrors the shape of rig-rmcp's own tests.
#[derive(Clone)]
struct FakeServer {
    label: &'static str,
    tools: Arc<tokio::sync::Mutex<Vec<(String, String)>>>,
    calls: Arc<tokio::sync::Mutex<Vec<(String, Value)>>>,
    fail_calls: Arc<AtomicBool>,
}

impl FakeServer {
    fn with_tools(tools: &[(&str, &str)]) -> Self {
        Self {
            label: "fake",
            tools: Arc::new(tokio::sync::Mutex::new(
                tools
                    .iter()
                    .map(|(name, description)| ((*name).to_string(), (*description).to_string()))
                    .collect(),
            )),
            calls: Arc::default(),
            fail_calls: Arc::default(),
        }
    }

    async fn set_tools(&self, tools: Vec<(&str, &str)>) {
        *self.tools.lock().await = tools
            .into_iter()
            .map(|(name, description)| (name.to_string(), description.to_string()))
            .collect();
    }

    async fn recorded_calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().await.clone()
    }

    fn fail_calls(&self) {
        self.fail_calls.store(true, AtomicOrdering::SeqCst);
    }
}

impl ServerHandler for FakeServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::LATEST)
            .with_server_info(Implementation::new(self.label, "0.1.0"))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        let tools = self.tools.lock().await.clone();
        Ok(ListToolsResult::with_all_items(
            tools
                .into_iter()
                .map(|(name, description)| {
                    Tool::new(name, description, Arc::new(JsonObject::default()))
                })
                .collect(),
        ))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
        self.calls.lock().await.push((
            request.name.to_string(),
            request.arguments.map(Value::Object).unwrap_or(Value::Null),
        ));
        if self.fail_calls.load(AtomicOrdering::SeqCst) {
            return Ok(CallToolResult::error(vec![ContentBlock::text("the tool said no")]).into());
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "{} {}",
            self.label, request.name
        ))])
        .into())
    }
}

/// The client-side transport connected to a running in-process fake
/// server, plus the server's peer handle and task. The transport is
/// returned immediately: `serve` completes only after the client
/// handshake, so the caller must connect before awaiting the peer.
/// Bounded readiness so a broken setup fails the test, not hangs.
fn start_fake_server(
    server: FakeServer,
) -> (
    (tokio::io::DuplexStream, tokio::io::DuplexStream),
    tokio::sync::oneshot::Receiver<Peer<RoleServer>>,
    tokio::task::JoinHandle<()>,
) {
    let (client_to_server, server_from_client) = tokio::io::duplex(8192);
    let (server_to_client, client_from_server) = tokio::io::duplex(8192);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let running = server
            .serve((server_from_client, server_to_client))
            .await
            .expect("fake server start");
        let _ = ready_tx.send(running.peer().clone());
        let _ = running.waiting().await;
    });
    ((client_from_server, client_to_server), ready_rx, task)
}

// -- fake guard sink ------------------------------------------------------

/// Scripts `ToolCallOutcome`s and records every guarded call.
struct FakeSink {
    outcome: tokio::sync::Mutex<Vec<anyhow::Result<ToolCallOutcome>>>,
    calls: tokio::sync::Mutex<Vec<(Uuid, Uuid, dhi_core::ToolCallRequest)>>,
}

impl Default for FakeSink {
    fn default() -> Self {
        Self::allowing()
    }
}

impl FakeSink {
    fn n_of(outcomes: Vec<anyhow::Result<ToolCallOutcome>>) -> Self {
        Self {
            outcome: tokio::sync::Mutex::new(outcomes),
            calls: tokio::sync::Mutex::new(Vec::new()),
        }
    }

    fn allowing() -> Self {
        Self::n_of(vec![Ok(ToolCallOutcome::Allowed {
            output: json!({ "ok": true }),
        })])
    }

    fn denying(reason: &str) -> Self {
        Self::n_of(vec![Ok(ToolCallOutcome::Denied {
            reason: reason.to_string(),
        })])
    }

    fn pending() -> Self {
        Self::n_of(vec![Ok(ToolCallOutcome::Pending {
            approval_id: Uuid::new_v4(),
        })])
    }

    async fn recorded(&self) -> Vec<(Uuid, Uuid, dhi_core::ToolCallRequest)> {
        self.calls.lock().await.clone()
    }
}

#[async_trait]
impl ToolSink for FakeSink {
    async fn execute(
        &self,
        task_id: Uuid,
        intent_id: Uuid,
        request: dhi_core::ToolCallRequest,
    ) -> anyhow::Result<ToolCallOutcome> {
        self.calls.lock().await.push((task_id, intent_id, request));
        self.outcome.lock().await.pop().unwrap_or_else(|| {
            Ok(ToolCallOutcome::Allowed {
                output: json!({ "ok": true }),
            })
        })
    }
}

// -- deployment helper ---------------------------------------------------

fn http_config(name: &str, url: &str) -> ValidatedServer {
    ValidatedServer {
        name: name.to_string(),
        segment: sanitize_segment(name),
        transport: ValidTransport::Http {
            url: url.to_string(),
            headers: BTreeMap::new(),
            insecure: false,
        },
        call_timeout: Duration::from_secs(5),
        visible_to: None,
        tool_visibility: BTreeMap::new(),
        deferred_threshold: None,
    }
}

fn config_entry(entry: Value) -> dhi_mcp::McpServerEntry {
    serde_json::from_value(entry).expect("a standard mcpServers entry parses")
}

fn convert_entry(name: &str, entry: Value) -> ValidatedServer {
    let validated = config_entry(entry)
        .validate(name)
        .expect("the config entry validates");
    ValidatedServer::try_from(validated).expect("the transport converts")
}

/// One manager connected to one in-process fake server over duplex,
/// with its guarded registrations live on a private registry handle.
struct FakeDeployment {
    manager: Arc<McpManager>,
    handle: ToolServerHandle,
    sink: Arc<FakeSink>,
    backend: McpBackend,
    server: FakeServer,
    connection: McpConnection,
    running: Peer<RoleServer>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeDeployment {
    /// Clean teardown: manager shutdown (connections closed, guarded
    /// registrations retired) plus a bounded wait for the fake server.
    async fn shutdown(self) {
        self.manager.shutdown().await;
        let _ = tokio::time::timeout(Duration::from_secs(2), self.task).await;
    }
}

async fn deploy(name: &str, tools: &[(&str, &str)]) -> FakeDeployment {
    deploy_connected(
        name,
        FakeServer::with_tools(tools),
        Arc::new(FakeSink::allowing()),
    )
    .await
}

async fn deploy_with_sink(name: &str, sink: Arc<FakeSink>) -> FakeDeployment {
    deploy_connected(name, FakeServer::with_tools(&[("fetch", "fetch")]), sink).await
}

async fn deploy_denying(name: &str, reason: &str) -> FakeDeployment {
    deploy_with_sink(name, Arc::new(FakeSink::denying(reason))).await
}

async fn deploy_pending(name: &str) -> FakeDeployment {
    deploy_with_sink(name, Arc::new(FakeSink::pending())).await
}

/// Connects `server` through the manager exactly as `connect_one` does,
/// with an injected transport (the in-process duplex pair).
async fn deploy_connected(name: &str, server: FakeServer, sink: Arc<FakeSink>) -> FakeDeployment {
    let (transport, peer_rx, task) = start_fake_server(server.clone());
    let validated = http_config(name, "http://127.0.0.1:1/mcp");
    let manager = McpManager::new(vec![validated.clone()]);
    let handle = ToolServer::new().run();
    let connection = manager
        .connect_shared(&validated, transport)
        .await
        .expect("handshake with the fake server");
    let running = tokio::time::timeout(Duration::from_secs(5), peer_rx)
        .await
        .expect("fake server becomes ready promptly")
        .expect("fake server readiness");
    tokio::time::timeout(
        Duration::from_secs(5),
        manager.finish_connect(&handle, &validated, sink.clone(), connection.clone()),
    )
    .await
    .expect("discovery and registration complete promptly")
    .expect("discovery and registration succeed");
    let backend = McpBackend::new(manager.registry());
    FakeDeployment {
        manager,
        handle,
        sink,
        backend,
        server,
        connection,
        running,
        task,
    }
}

async fn guarded_defs(handle: &ToolServerHandle) -> Vec<String> {
    handle
        .tool_defs(None)
        .await
        .expect("definitions read")
        .into_iter()
        .map(|definition| definition.name.to_string())
        .collect()
}

async fn dispatch(backend: &McpBackend, name: &str, input: Value) -> anyhow::Result<Value> {
    tokio::time::timeout(Duration::from_secs(5), backend.execute(name, &input))
        .await
        .expect("dispatch bounded")
}

/// The closed delegate behind the routing backend in these tests. No tool
/// is served here; any call fails closed like the ship-with-no-backend
/// behavior.
struct ClosedDelegate;

#[async_trait]
impl ToolBackend for ClosedDelegate {
    async fn execute(&self, tool: &str, _input: &Value) -> anyhow::Result<Value> {
        Err(anyhow::anyhow!("no tool backend configured for {tool}"))
    }
}

fn dead_delegate() -> Arc<dyn ToolBackend> {
    Arc::new(ClosedDelegate)
}

// -- config conversion tests ----------------------------------------------

/// The README's chess shape (`command` + `args`, transport inferred)
/// converts to a connectable stdio server.
#[test]
fn config_converts_chess_stdio() {
    let server = convert_entry(
        "chess",
        json!({ "command": "uvx", "args": ["chess-mcp-server"] }),
    );
    assert_eq!(server.name, "chess");
    assert_eq!(server.segment, "chess");
    assert_eq!(server.call_timeout, dhi_mcp::DEFAULT_CALL_TIMEOUT);
    assert_eq!(
        server.transport,
        ValidTransport::Stdio {
            command: "uvx".to_string(),
            args: vec!["chess-mcp-server".to_string()],
            env: HashMap::new(),
        }
    );
}

/// A remote HTTP entry keeps its headers, timeout, and insecure flag.
#[test]
fn config_converts_remote_http_with_headers_and_insecure() {
    let server = convert_entry(
        "remote",
        json!({
            "url": "https://example.com/mcp",
            "headers": { "Authorization": "Bearer token", "X-Tenant": "acme" },
            "timeout_secs": 17,
            "insecure": true
        }),
    );
    assert_eq!(server.name, "remote");
    assert_eq!(server.call_timeout, Duration::from_secs(17));
    let ValidTransport::Http {
        url,
        headers,
        insecure,
    } = &server.transport
    else {
        panic!("expected an HTTP server");
    };
    assert_eq!(url, "https://example.com/mcp");
    assert_eq!(headers["Authorization"], "Bearer token");
    assert_eq!(headers["X-Tenant"], "acme");
    assert!(insecure);
}

/// Visibility and deferred-schema fields are copied from the shared config
/// entry into the runtime's validated server without interpretation here.
#[test]
fn config_conversion_carries_visibility_fields() {
    let server = convert_entry(
        "remote",
        json!({
            "url": "https://example.com/mcp",
            "visible_to": [],
            "tool_visibility": { "fetch": ["worker"], "search": [] },
            "deferred_threshold": 12
        }),
    );
    assert_eq!(server.visible_to, Some(Vec::new()));
    assert_eq!(server.tool_visibility["fetch"], vec!["worker".to_string()]);
    assert!(server.tool_visibility["search"].is_empty());
    assert_eq!(server.deferred_threshold, Some(12));
}

/// `insecure` defaults to false and is only a TLS/auth switch; it is not
/// a transport selector.
#[test]
fn config_converts_http_secure_by_default() {
    let server = convert_entry("secure", json!({ "url": "https://example.com/mcp" }));
    let ValidTransport::Http { insecure, .. } = server.transport else {
        panic!("expected an HTTP server");
    };
    assert!(!insecure);
}

/// The config crate rejects CR/LF; conversion still refuses header names
/// and values reqwest cannot represent, without echoing the value.
#[test]
fn config_conversion_rejects_invalid_headers() {
    let headers = BTreeMap::from([
        ("Bad Header".to_string(), "value".to_string()),
        ("X-Ok".to_string(), "value".to_string()),
    ]);
    let error = http_headers("remote", &headers).expect_err("a space is not a header name");
    assert!(error.to_string().contains("invalid header name"), "{error}");

    let headers = BTreeMap::from([("X-Test".to_string(), "top-secret\u{0007}value".to_string())]);
    let error = http_headers("remote", &headers).expect_err("BEL is not a header value");
    let message = error.to_string();
    assert!(message.contains("invalid value"), "{message}");
    assert!(!message.contains("top-secret"), "{message}");

    let headers = BTreeMap::from([("X-Test".to_string(), "value".to_string())]);
    let converted = http_headers("remote", &headers).expect("valid headers convert");
    assert_eq!(converted.len(), 1);
}

/// dhi-mcp's parsed SSE entry becomes the legacy SSE transport, keeping its
/// URL, headers, TLS switch, and call timeout; it never maps to Streamable
/// HTTP and is labelled distinctly in status.
#[test]
fn legacy_sse_converts_to_its_own_transport() {
    let sse = dhi_mcp::ValidatedServer {
        name: "events".to_string(),
        segment: "events".to_string(),
        transport: dhi_mcp::McpTransport::Sse,
        call_timeout: Duration::from_secs(9),
        transport_data: dhi_mcp::McpTransportData::Sse {
            url: "https://example.com/sse".to_string(),
            headers: BTreeMap::from([("Authorization".to_string(), "Bearer token".to_string())]),
            insecure: true,
        },
        visible_to: None,
        tool_visibility: BTreeMap::new(),
        deferred_threshold: None,
    };
    let server = ValidatedServer::try_from(sse).expect("legacy SSE converts");
    assert_eq!(server.call_timeout, Duration::from_secs(9));
    assert_eq!(
        server.transport,
        ValidTransport::Sse {
            url: "https://example.com/sse".to_string(),
            headers: BTreeMap::from([("Authorization".to_string(), "Bearer token".to_string(),)]),
            insecure: true,
        }
    );
    assert_eq!(transport_label(&server.transport), "sse");
    let debug = format!("{:?}", server.transport);
    assert!(!debug.contains("Bearer token"), "{debug}");
}

#[test]
fn names_sanitize_and_cap() {
    assert_eq!(sanitize_segment("Weather Data.v2"), "Weather_Data_v2");
    assert_eq!(sanitize_segment("a__b"), "a_b");
    assert_eq!(sanitize_segment("-x-"), "x");
    assert_eq!(sanitize_segment("UPPER-case_9"), "UPPER-case_9");
    assert_eq!(sanitize_segment("天气预报"), "", "no usable characters");

    assert_eq!(
        guarded_name("weather", "get_forecast").as_deref(),
        Some("mcp__weather__get_forecast")
    );
    assert_eq!(guarded_name("weather", "###"), None, "unusable tool name");
    assert_eq!(guarded_name("###", "tool"), None, "unusable server name");

    // The whole name stays within 64 chars: the tool segment takes the cut.
    let long_tool = "t".repeat(80);
    let name = guarded_name("weather", &long_tool).expect("a long tool still names");
    assert_eq!(name.chars().count(), MAX_TOOL_NAME_CHARS);
    assert!(name.starts_with("mcp__weather__"));

    // Config validation caps the server segment itself.
    let server = convert_entry(
        "a-very-long-server-name-over-16-chars",
        json!({ "url": "https://s.test" }),
    );
    assert_eq!(server.segment.chars().count(), MAX_SERVER_SEGMENT_CHARS);
    assert!(server.segment.starts_with("a-very-long-serv"));
}

// -- status cache tests ----------------------------------------------------

/// A configured server starts disconnected with no cached tools, and is
/// visible before any connection attempt.
#[tokio::test]
async fn configured_server_starts_disconnected_without_tools() {
    let manager = McpManager::new(vec![http_config("chess", "https://example.com/mcp")]);
    let statuses = manager.statuses().await;
    assert_eq!(statuses.len(), 1);
    let status = &statuses[0];
    assert_eq!(status.name, "chess");
    assert_eq!(status.transport, "http");
    assert!(status.enabled);
    assert!(!status.connected);
    assert!(status.tools.is_empty());
    assert_eq!(status.last_error, None);
    assert_eq!(status.call_timeout_secs, 5);
    assert_eq!(manager.tools("chess").await, Some(Vec::new()));
    assert_eq!(manager.tools("missing").await, None);
    assert!(manager.refresh("missing").await.is_err());
}

/// A successful connect fills the status cache with the discovered tools.
#[tokio::test]
async fn successful_connect_updates_status_and_tools() {
    let deps = deploy(
        "fake",
        &[("fetch", "Fetch a thing"), ("web.search", "Search")],
    )
    .await;
    let statuses = deps.manager.statuses().await;
    assert_eq!(statuses.len(), 1);
    assert!(statuses[0].connected);
    assert_eq!(statuses[0].last_error, None);
    assert_eq!(
        statuses[0].tools,
        vec![
            McpToolInfo {
                name: "fetch".to_string(),
                description: "Fetch a thing".to_string(),
            },
            McpToolInfo {
                name: "web.search".to_string(),
                description: "Search".to_string(),
            },
        ]
    );
    deps.shutdown().await;
}

/// The explicit refresh path re-discovers tools, updates the status
/// cache, and returns the current list.
#[tokio::test]
async fn refresh_updates_status_and_returns_tools() {
    let deps = deploy("fake", &[("fetch", "Fetch")]).await;
    deps.server
        .set_tools(vec![("fetch", "Fetch"), ("extra", "Extra")])
        .await;
    let tools = deps
        .manager
        .refresh("fake")
        .await
        .expect("refresh succeeds");
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(names, ["fetch", "extra"]);
    let statuses = deps.manager.statuses().await;
    assert_eq!(statuses[0].tools, tools);
    assert!(statuses[0].connected);
    assert_eq!(statuses[0].last_error, None);
    deps.shutdown().await;
}

// -- discovery + registration ----------------------------------------------

/// Discovery registers ONLY the guarded, prefixed names: no raw MCP tool
/// ever lands on the agent registry.
#[tokio::test]
async fn discovery_registers_only_guarded_names() {
    let deps = deploy(
        "fake",
        &[("fetch", "Fetch a thing"), ("web.search", "Search")],
    )
    .await;
    let defs = guarded_defs(&deps.handle).await;
    assert!(defs.contains(&"mcp__fake__fetch".to_string()), "{defs:?}");
    // The dot sanitizes to an underscore in the model-visible name.
    assert!(
        defs.contains(&"mcp__fake__web_search".to_string()),
        "{defs:?}"
    );
    assert_eq!(deps.manager.registry().names().await, defs);
    assert!(
        !defs.contains(&"fetch".to_string()) && !defs.contains(&"web.search".to_string()),
        "raw MCP names must not be registered: {defs:?}"
    );
    deps.shutdown().await;
}

/// Every call through the registry's guarded entry routes through the
/// one sink: allowed output reaches the model, the recorded call keeps
/// the guarded name and the model's raw arguments, the scope comes from
/// the stamped context, denials carry the reason, and a pending outcome
/// is defensively an internal error.
#[tokio::test]
async fn guarded_mapping_allow_deny_and_pending() {
    // Allowed.
    let deps = deploy("fake", &[("fetch", "fetch")]).await;
    let result = execute_guarded(&deps, r#"{"x": 1}"#).await;
    let output = result.expect("an allowed call succeeds");
    assert_eq!(output.as_json(), Some(&json!({ "ok": true })));
    let (task_id, intent_id, request) = deps.sink.recorded().await.remove(0);
    assert_eq!(request.tool, "mcp__fake__fetch");
    assert_eq!(request.input, json!({ "x": 1 }));
    assert_ne!(task_id, Uuid::nil());
    assert_ne!(intent_id, Uuid::nil());
    deps.shutdown().await;

    // Denied: the guard's reason travels to the model.
    let deps = deploy_denying("fake", "rule deny-mcp: mcp is not allowed").await;
    let err = execute_guarded(&deps, r#"{"x": 1}"#)
        .await
        .expect_err("a denial is an error");
    assert!(err.to_string().contains("deny-mcp"), "{err}");
    deps.shutdown().await;

    // Pending is defensively an internal error.
    let deps = deploy_pending("fake").await;
    let err = execute_guarded(&deps, r#"{"x": 1}"#)
        .await
        .expect_err("pending must never surface as success");
    assert!(err.to_string().contains("approvals must block"), "{err}");
    deps.shutdown().await;

    // A dispatch without the runtime's stamp is an internal error.
    let deps = deploy("fake", &[("fetch", "fetch")]).await;
    let err = deps
        .handle
        .toolset()
        .execute(
            "mcp__fake__fetch",
            r#"{"x": 1}"#.to_owned(),
            &mut ToolContext::new(),
        )
        .await
        .into_result()
        .expect_err("no scope, no sink");
    assert!(err.to_string().contains("run scope"), "{err}");
    deps.shutdown().await;
}

/// Runs one registered guarded tool through the registry with a stamped
/// run scope, exactly as the runtime dispatches in-loop tools.
async fn execute_guarded(
    deps: &FakeDeployment,
    args: &str,
) -> Result<ToolOutput, ToolExecutionError> {
    let mut context = ToolContext::new();
    context
        .insert(RunScope {
            task_id: Uuid::new_v4(),
            intent_id: Uuid::new_v4(),
        })
        .expect("the scope serializes");
    deps.handle
        .toolset()
        .execute("mcp__fake__fetch", args.to_owned(), &mut context)
        .await
        .into_result()
}

// -- dispatch tests ----------------------------------------------------------

/// The guarded sink's backend calls the server's `call_tool` with the
/// MCP name (not the prefixed model name) and the model's arguments.
#[tokio::test]
async fn dispatch_sends_the_mcp_name_with_the_model_arguments() {
    let deps = deploy("fake", &[("fetch", "fetch"), ("web.search", "search")]).await;
    let out = dispatch(
        &deps.backend,
        "mcp__fake__fetch",
        json!({ "query": "hello" }),
    )
    .await
    .expect("allowed dispatch through the backend");
    assert_eq!(out, json!({ "text": "fake fetch" }));
    // A tool whose MCP name needed sanitization keeps its real name on
    // the wire.
    dispatch(
        &deps.backend,
        "mcp__fake__web_search",
        json!({ "q": "rust" }),
    )
    .await
    .expect("sanitized-name dispatch");
    let calls = deps.server.recorded_calls().await;
    assert_eq!(
        calls,
        vec![
            ("fetch".to_string(), json!({ "query": "hello" })),
            ("web.search".to_string(), json!({ "q": "rust" })),
        ]
    );
    deps.shutdown().await;
}

/// `is_error` results and unknown names fail with model-readable errors
/// — never a panic, never a false success.
#[tokio::test]
async fn error_results_and_unknown_names_fail_readably() {
    let deps = deploy("fake", &[("fetch", "fetch")]).await;

    deps.server.fail_calls();
    let err = dispatch(&deps.backend, "mcp__fake__fetch", json!({}))
        .await
        .expect_err("an is_error result is an error");
    let text = format!("{err:#}");
    assert!(text.contains("reported an execution error"), "{text}");
    assert!(text.contains("the tool said no"), "{text}");

    let err = dispatch(&deps.backend, "mcp__fake__nope", json!({}))
        .await
        .expect_err("an unknown name fails closed");
    assert!(
        err.to_string().contains("no MCP tool is registered as"),
        "{err}"
    );
    deps.shutdown().await;
}

/// After the connection is closed, dispatch degrades to an error.
#[tokio::test]
async fn dispatch_after_close_fails_cleanly() {
    let deps = deploy("fake", &[("fetch", "fetch")]).await;
    deps.connection.close().await;
    let err = tokio::time::timeout(
        Duration::from_secs(5),
        deps.backend.execute("mcp__fake__fetch", &json!({})),
    )
    .await
    .expect("bounded")
    .expect_err("a closed transport cannot answer");
    assert!(!err.to_string().is_empty(), "{err}");
    deps.shutdown().await;
}

/// The routing backend hands `mcp__` names to the MCP backend and
/// everything else to the delegate; both fail closed when empty.
#[tokio::test]
async fn routing_backend_splits_mcp_from_the_delegate() {
    let registry = Arc::new(GuardedMcpRegistry::new());
    let mcp = McpBackend::new(registry);
    let routing = McpRoutingBackend::new(mcp, dead_delegate());
    let err = routing
        .execute("mcp__nothing__tool", &json!({}))
        .await
        .expect_err("an empty MCP registry fails closed");
    assert!(err.to_string().contains("no MCP tool"), "{err}");
    let err = routing
        .execute("unknown_tool", &json!({}))
        .await
        .expect_err("the delegate serves nothing");
    assert!(err.to_string().contains("no tool backend"), "{err}");
}

/// A server that cannot connect degrades with a bounded failure: the
/// manager stays alive, nothing registers, later dispatch fails closed.
#[tokio::test]
async fn a_dead_server_degrades_without_killing_the_manager() {
    let validated = http_config("dead", "http://127.0.0.1:1/mcp");
    let manager = McpManager::new(vec![validated.clone()]);
    let handle = ToolServer::new().run();
    let started = std::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        manager.connect_one(&handle, &validated, Arc::new(FakeSink::allowing())),
    )
    .await
    .expect("the failure is bounded, not a hang");
    assert!(result.is_err(), "port 1 refuses connections: {result:?}");
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "the failure came back fast: {:?}",
        started.elapsed()
    );
    assert!(manager.registry().names().await.is_empty());
    let statuses = manager.statuses().await;
    assert_eq!(statuses.len(), 1);
    assert!(
        !statuses[0].connected,
        "a failed connect stays disconnected"
    );
    assert!(
        statuses[0].last_error.is_some(),
        "the failure is visible in the status cache"
    );
    assert!(statuses[0].tools.is_empty());
    assert!(manager.refresh("dead").await.is_err());
    let backend = McpBackend::new(manager.registry());
    let err = backend
        .execute("mcp__dead__tool", &json!({}))
        .await
        .expect_err("nothing registered, nothing dispatchable");
    assert!(err.to_string().contains("no MCP tool"), "{err}");
}

// -- shutdown -----------------------------------------------------------------

/// Shutdown retires the guarded registrations and closes the
/// connections; the fake server's task then ends on its own.
#[tokio::test]
async fn shutdown_closes_connections_and_retires_registrations() {
    let deps = deploy("fake", &[("fetch", "fetch")]).await;
    assert!(!guarded_defs(&deps.handle).await.is_empty());
    deps.manager.shutdown().await;
    assert!(deps.manager.registry().names().await.is_empty());
    assert!(
        guarded_defs(&deps.handle).await.is_empty(),
        "no guarded MCP tool survives shutdown"
    );
    assert!(deps.connection.is_closed(), "the connection is closed");
    tokio::time::timeout(Duration::from_secs(2), deps.task)
        .await
        .expect("the fake server ends when the client closes")
        .expect("clean server exit");
}

// -- lifecycle: notifications/tools/list_changed ------------------------------

/// A `notifications/tools/list_changed` re-runs the guarded registration:
/// new tools appear as guarded names, removed tools are retired, and no
/// raw tool ever registers.
#[tokio::test]
async fn list_changed_refreshes_the_guarded_registrations() {
    let deps = deploy("fake", &[("fetch", "fetch")]).await;
    assert!(
        guarded_defs(&deps.handle)
            .await
            .contains(&"mcp__fake__fetch".to_string())
    );

    // Add a tool and notify.
    deps.server
        .set_tools(vec![("fetch", "fetch"), ("extra", "Extra")])
        .await;
    deps.running
        .notify_tool_list_changed()
        .await
        .expect("notification sent");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let defs = guarded_defs(&deps.handle).await;
            if defs.contains(&"mcp__fake__extra".to_string()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the refresh registers the new guarded tool");
    let defs = guarded_defs(&deps.handle).await;
    assert!(defs.contains(&"mcp__fake__fetch".to_string()), "{defs:?}");
    assert!(
        !defs.contains(&"extra".to_string()),
        "no raw tools: {defs:?}"
    );

    // Remove it again.
    deps.server.set_tools(vec![("fetch", "fetch")]).await;
    deps.running
        .notify_tool_list_changed()
        .await
        .expect("notification sent");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let defs = guarded_defs(&deps.handle).await;
            if !defs.contains(&"mcp__fake__extra".to_string()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the refresh retires the removed tool");
    assert_eq!(
        deps.manager.registry().names().await,
        vec!["mcp__fake__fetch".to_string()]
    );
    deps.shutdown().await;
}
