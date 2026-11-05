//! A minimal legacy MCP Server-Sent Events client.
//!
//! Legacy MCP (the pre-Streamable-HTTP transport) is:
//!
//! 1. `GET <url>` with `Accept: text/event-stream`;
//! 2. the server sends an `endpoint` event naming the POST URL;
//! 3. JSON-RPC requests are POSTed to that URL and responses arrive on the
//!    still-open SSE stream, correlated by numeric `id`.
//!
//! This client implements that flow, including `initialize`, `tools/list`,
//! and `tools/call`, with bounded handshake and call deadlines and support
//! for arbitrary headers (for example `Authorization`). It is not the same
//! thing as Streamable HTTP: this client requires the `GET /sse` +
//! `endpoint` handshake and never silently falls back to another transport.
//!
//! Deliberately not implemented (documented so wiring does not assume them):
//!
//! * automatic reconnection or `Last-Event-ID` resumption;
//! * server-initiated requests (sampling, roots, elicitation), which are
//!   ignored rather than answered;
//! * cancellation notifications and SSE stream shutdown (`DELETE`);
//! * OAuth discovery and dynamic client registration;
//! * JSON-RPC batches and string request ids (the client sends numeric ids);
//! * Streamable HTTP fallback.
//!
//! `tools/list` pagination is followed, capped at [`MAX_LIST_PAGES`] pages.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::bail;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::{broadcast, oneshot};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use url::Url;

/// Handshake deadline used when a config does not override it.
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// Call deadline used when a config does not override it.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// Total discovery deadline used when a config does not override it.
pub const DEFAULT_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(120);
/// The protocol version this client sends in `initialize`.
pub const LEGACY_PROTOCOL_VERSION: &str = "2024-11-05";
/// Pagination cap for `tools/list`.
pub const MAX_LIST_PAGES: usize = 100;
/// The largest number of tools discovery will accept across all pages.
pub const MAX_DISCOVERY_TOOLS: usize = 10_000;
/// The largest SSE event this client will buffer.
pub const MAX_SSE_EVENT_BYTES: usize = 1024 * 1024;
/// The largest successful POST body this client will buffer.
pub const MAX_POST_BODY_BYTES: usize = 8 * 1024 * 1024;
/// The largest HTTP error body this client will read before truncating.
const MAX_ERROR_BODY_BYTES: usize = 2 * 1024;
/// The largest HTTP error body echoed in an error.
const MAX_ERROR_BODY_CHARS: usize = 512;
/// The largest endpoint/URL echoed in an error or debug output.
const MAX_URL_DISPLAY_CHARS: usize = 256;
/// How many decoded SSE messages a transport subscriber may fall behind
/// before the stream is considered unusable. The worker forwards messages
/// immediately, so this only guards a stuck consumer.
const INBOUND_BUFFER: usize = 256;

/// Connection settings for one legacy SSE server.
#[derive(Clone)]
pub struct LegacySseConfig {
    /// The `GET /sse` URL.
    pub url: String,
    /// Additional request headers sent on both the GET and every POST.
    pub headers: BTreeMap<String, String>,
    /// Deadline for the GET, endpoint event, and initialize handshake.
    pub handshake_timeout: Duration,
    /// Deadline for one `tools/list` page or one `tools/call`.
    pub call_timeout: Duration,
    /// Total deadline for one complete `tools/list` discovery.
    pub discovery_timeout: Duration,
    /// Skip TLS verification (HTTP/SSE config equivalent).
    pub insecure: bool,
}

impl LegacySseConfig {
    /// A config with the default deadlines and no headers.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            headers: BTreeMap::new(),
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            call_timeout: DEFAULT_CALL_TIMEOUT,
            discovery_timeout: DEFAULT_DISCOVERY_TIMEOUT,
            insecure: false,
        }
    }

    /// Adds or replaces one request header.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    /// Adds `Authorization: Bearer <token>`.
    pub fn with_bearer_token(mut self, token: impl AsRef<str>) -> Self {
        self.headers.insert(
            "Authorization".to_string(),
            format!("Bearer {}", token.as_ref()),
        );
        self
    }

    /// Converts a `dhi-mcp` validated SSE server into client settings.
    pub fn from_validated(server: &dhi_mcp::ValidatedServer) -> anyhow::Result<Self> {
        match &server.transport_data {
            dhi_mcp::McpTransportData::Sse {
                url,
                headers,
                insecure,
            } => Ok(Self {
                url: url.clone(),
                headers: headers.clone(),
                handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
                call_timeout: server.call_timeout,
                discovery_timeout: DEFAULT_DISCOVERY_TIMEOUT,
                insecure: *insecure,
            }),
            _ => bail!(
                "MCP server {:?} is not configured for the legacy SSE transport",
                server.name
            ),
        }
    }
}

impl fmt::Debug for LegacySseConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let header_names: Vec<&String> = self.headers.keys().collect();
        formatter
            .debug_struct("LegacySseConfig")
            .field("url", &redact_url(&self.url))
            .field("header_names", &header_names)
            .field("handshake_timeout", &self.handshake_timeout)
            .field("call_timeout", &self.call_timeout)
            .field("discovery_timeout", &self.discovery_timeout)
            .field("insecure", &self.insecure)
            .finish()
    }
}

/// One tool from `tools/list`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct LegacyTool {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "inputSchema", default = "default_input_schema")]
    pub input_schema: Value,
}

impl LegacyTool {
    /// The description, or an empty slice when absent.
    pub fn description_text(&self) -> &str {
        self.description.as_deref().unwrap_or_default()
    }
}

/// A live legacy SSE connection.
///
/// Dropping the client aborts the background reader. Pending calls fail with
/// [`LegacySseError::StreamClosed`] rather than hanging.
pub struct LegacySseClient {
    config: LegacySseConfig,
    http: reqwest::Client,
    endpoint: String,
    headers: HeaderMap,
    pending: Arc<Pending>,
    /// Every decoded server message (responses, notifications, and requests).
    /// Transport consumers subscribe here; the request helpers above use
    /// `pending` instead.
    inbound: broadcast::Sender<Value>,
    next_id: AtomicU64,
    reader: JoinHandle<()>,
}

impl LegacySseClient {
    /// Opens the SSE stream, learns the message endpoint, and completes the
    /// `initialize` handshake. Every stage is bounded by
    /// [`LegacySseConfig::handshake_timeout`].
    pub async fn connect(config: LegacySseConfig) -> Result<Self, LegacySseError> {
        let client = Self::open(config).await?;
        let deadline = Instant::now() + client.config.handshake_timeout;
        client
            .initialize(deadline, client.config.handshake_timeout)
            .await?;
        Ok(client)
    }

    /// Opens the SSE stream and learns the message endpoint **without**
    /// sending `initialize`. This is the rmcp transport path: rmcp's client
    /// service performs the handshake itself over the returned client.
    pub async fn open(config: LegacySseConfig) -> Result<Self, LegacySseError> {
        if config.handshake_timeout.is_zero() {
            return Err(LegacySseError::HandshakeTimeout {
                duration: config.handshake_timeout,
            });
        }
        let lower = config.url.to_ascii_lowercase();
        if !(lower.starts_with("http://") || lower.starts_with("https://")) {
            return Err(LegacySseError::Protocol(
                "legacy SSE url must start with http:// or https://".to_string(),
            ));
        }

        let deadline = Instant::now() + config.handshake_timeout;
        let headers = build_headers(&config.headers)?;
        let http = reqwest::Client::builder()
            .danger_accept_invalid_certs(config.insecure)
            .connect_timeout(config.handshake_timeout)
            // A redirect is another origin decision; do not let a malicious
            // server bounce custom headers or bearer tokens elsewhere.
            .redirect(reqwest::redirect::Policy::none())
            .build()?;

        let mut get_headers = headers.clone();
        get_headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        let response = match timeout(
            handshake_remaining(deadline, config.handshake_timeout)?,
            http.get(&config.url).headers(get_headers).send(),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => {
                return Err(LegacySseError::HandshakeTimeout {
                    duration: config.handshake_timeout,
                });
            }
        };

        let status = response.status();
        if !status.is_success() {
            let body = match timeout(
                handshake_remaining(deadline, config.handshake_timeout)?,
                read_body_capped(response, MAX_ERROR_BODY_BYTES),
            )
            .await
            {
                Ok(Ok((body, _))) => String::from_utf8_lossy(&body).into_owned(),
                Ok(Err(error)) => return Err(error),
                Err(_) => String::new(),
            };
            return Err(LegacySseError::Http {
                status: status.as_u16(),
                url: redact_url(&config.url),
                body: truncate_for_error(&body),
            });
        }
        if !response_is_event_stream(&response) {
            return Err(LegacySseError::Protocol(
                "legacy SSE GET response is not text/event-stream; \
                 this client does not treat Streamable HTTP as legacy SSE"
                    .to_string(),
            ));
        }

        let pending = Arc::new(Pending::default());
        let (inbound, _) = broadcast::channel(INBOUND_BUFFER);
        let (endpoint_sender, endpoint_receiver) = oneshot::channel();
        let reader = tokio::spawn(read_stream(
            response,
            Some(endpoint_sender),
            pending.clone(),
            inbound.clone(),
        ));

        let endpoint_limit = match handshake_remaining(deadline, config.handshake_timeout) {
            Ok(limit) => limit,
            Err(error) => {
                reader.abort();
                return Err(error);
            }
        };
        let endpoint = match timeout(endpoint_limit, endpoint_receiver).await {
            Ok(Ok(endpoint)) => endpoint,
            Ok(Err(_)) => {
                reader.abort();
                return Err(LegacySseError::MissingEndpoint);
            }
            Err(_) => {
                reader.abort();
                return Err(LegacySseError::HandshakeTimeout {
                    duration: config.handshake_timeout,
                });
            }
        };
        let endpoint = match resolve_endpoint(&config.url, &endpoint) {
            Ok(endpoint) => endpoint,
            Err(error) => {
                reader.abort();
                return Err(error);
            }
        };

        Ok(Self {
            config,
            http,
            endpoint,
            headers,
            pending,
            inbound,
            next_id: AtomicU64::new(1),
            reader,
        })
    }

    /// The resolved JSON-RPC POST endpoint learned from the `endpoint` event.
    /// It may contain a session id; do not log it verbatim.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The connection settings (headers in the returned value are the
    /// configured headers, not the internal header map).
    pub fn config(&self) -> &LegacySseConfig {
        &self.config
    }

    /// Subscribes to every message event on the SSE stream. Used by the rmcp
    /// transport adapter, which correlates JSON-RPC responses itself.
    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.inbound.subscribe()
    }

    /// Stops the background SSE reader. Pending requests fail with
    /// [`LegacySseError::StreamClosed`]; a transport consumer sees its
    /// subscription end.
    pub fn close(&self) {
        self.reader.abort();
    }

    /// Posts one raw JSON-RPC message (request or notification). Returns the
    /// response when the server answers directly on the POST instead of the
    /// SSE stream; `None` means the response (if any) will arrive on the
    /// stream. The caller owns the deadline.
    ///
    /// Unlike the `tools/list`/`tools/call` helpers, this keeps the rmcp
    /// request id exactly as given and accepts a `text/event-stream` POST
    /// response body in addition to direct JSON.
    pub async fn send_message(&self, message: &Value) -> Result<Option<Value>, LegacySseError> {
        let response = self.post_json(message).await?;
        let status = response.status();
        if !status.is_success() {
            let (body, _) = read_body_capped(response, MAX_ERROR_BODY_BYTES).await?;
            let body = String::from_utf8_lossy(&body);
            return Err(LegacySseError::Http {
                status: status.as_u16(),
                url: redact_url(&self.endpoint),
                body: truncate_for_error(&body),
            });
        }
        let content_type = response_content_type(&response);
        let (body, truncated) = read_body_capped(response, MAX_POST_BODY_BYTES).await?;
        if truncated {
            return Err(LegacySseError::ResponseTooLarge {
                max: MAX_POST_BODY_BYTES,
            });
        }
        parse_post_message_body(&body, content_type.as_deref(), status, message.get("id"))
    }

    /// Calls `tools/list`, following `nextCursor` pages under one total
    /// discovery deadline and a hard tool-count cap.
    pub async fn list_tools(&self) -> Result<Vec<LegacyTool>, LegacySseError> {
        let discovery = async {
            let mut tools = Vec::new();
            let mut cursor: Option<String> = None;
            for _ in 0..MAX_LIST_PAGES {
                let params = match &cursor {
                    Some(cursor) => json!({ "cursor": cursor }),
                    None => json!({}),
                };
                let result = self
                    .request(
                        "tools/list",
                        params,
                        self.config.call_timeout,
                        Deadline::Call,
                    )
                    .await?;
                let page: ListToolsPage = serde_json::from_value(result)?;
                if tools.len().saturating_add(page.tools.len()) > MAX_DISCOVERY_TOOLS {
                    return Err(LegacySseError::TooManyTools {
                        max: MAX_DISCOVERY_TOOLS,
                    });
                }
                tools.extend(page.tools);
                match page.next_cursor {
                    Some(next) if !next.is_empty() => {
                        if cursor.as_deref() == Some(next.as_str()) {
                            return Err(LegacySseError::Protocol(
                                "tools/list repeated the same cursor".to_string(),
                            ));
                        }
                        cursor = Some(next);
                    }
                    _ => return Ok(tools),
                }
            }
            Err(LegacySseError::Protocol(format!(
                "tools/list exceeded {MAX_LIST_PAGES} pages"
            )))
        };

        match timeout(self.config.discovery_timeout, discovery).await {
            Ok(result) => result,
            Err(_) => Err(LegacySseError::DiscoveryTimeout {
                duration: self.config.discovery_timeout,
            }),
        }
    }

    /// Calls one tool. Returns the raw JSON-RPC `result` so the caller can
    /// apply the same content mapping as the main MCP manager.
    pub async fn call_tool(&self, tool: &str, arguments: Value) -> Result<Value, LegacySseError> {
        if tool.trim().is_empty() || tool.chars().any(char::is_control) {
            return Err(LegacySseError::InvalidToolName);
        }
        if !arguments.is_object() {
            return Err(LegacySseError::ArgumentsNotObject);
        }
        self.request(
            "tools/call",
            json!({ "name": tool, "arguments": arguments }),
            self.config.call_timeout,
            Deadline::Call,
        )
        .await
    }

    async fn initialize(&self, deadline: Instant, total: Duration) -> Result<(), LegacySseError> {
        let params = json!({
            "protocolVersion": LEGACY_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": "dhi-mcp-upgrade",
                "version": env!("CARGO_PKG_VERSION")
            }
        });
        let limit = handshake_remaining(deadline, total)?;
        self.request("initialize", params, limit, Deadline::Handshake)
            .await?;

        // Best effort: a server that accepts the initialization but not the
        // notification is still usable for tools. The notification stays
        // inside the same overall handshake deadline.
        let Ok(limit) = handshake_remaining(deadline, total) else {
            return Ok(());
        };
        let notification = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        });
        let _ = timeout(limit, async {
            let _response = self.post_json(&notification).await?;
            Ok::<(), LegacySseError>(())
        })
        .await;
        Ok(())
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        limit: Duration,
        deadline: Deadline,
    ) -> Result<Value, LegacySseError> {
        if limit.is_zero() {
            return Err(deadline.error(limit));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending
            .responses
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, sender);
        // The guard removes the id on every exit path, including cancellation
        // when the caller drops this future (for example a shorter timeout).
        let _guard = PendingGuard {
            pending: Arc::clone(&self.pending),
            id,
        };
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });

        let outcome = timeout(limit, async {
            let response = self.post_json(&payload).await?;
            let status = response.status();
            if !status.is_success() {
                let (body, _) = read_body_capped(response, MAX_ERROR_BODY_BYTES).await?;
                let body = String::from_utf8_lossy(&body);
                return Err(LegacySseError::Http {
                    status: status.as_u16(),
                    url: redact_url(&self.endpoint),
                    body: truncate_for_error(&body),
                });
            }
            let content_type = response_content_type(&response);
            let (body, truncated) = read_body_capped(response, MAX_POST_BODY_BYTES).await?;
            if truncated {
                return Err(LegacySseError::ResponseTooLarge {
                    max: MAX_POST_BODY_BYTES,
                });
            }
            match classify_post_body(&body, content_type.as_deref(), status, id)? {
                PostBody::Response(value) => Ok(value),
                PostBody::Acknowledgment => match receiver.await {
                    Ok(result) => result,
                    Err(_) => Err(LegacySseError::StreamClosed),
                },
            }
        })
        .await;

        match outcome {
            Ok(result) => result,
            Err(_) => Err(deadline.error(limit)),
        }
    }

    async fn post_json(&self, payload: &Value) -> Result<reqwest::Response, LegacySseError> {
        let mut headers = self.headers.clone();
        // Legacy SSE delivers JSON-RPC responses on the GET stream. The POST
        // itself only advertises JSON so a direct response is unambiguous.
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        self.http
            .post(&self.endpoint)
            .headers(headers)
            .json(payload)
            .send()
            .await
            .map_err(LegacySseError::from)
    }
}

impl fmt::Debug for LegacySseClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LegacySseClient")
            .field("endpoint", &redact_url(&self.endpoint))
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Drop for LegacySseClient {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

#[derive(Debug, Clone, Copy)]
enum Deadline {
    Handshake,
    Call,
}

impl Deadline {
    fn error(self, duration: Duration) -> LegacySseError {
        match self {
            Self::Handshake => LegacySseError::HandshakeTimeout { duration },
            Self::Call => LegacySseError::CallTimeout { duration },
        }
    }
}

#[derive(Debug, Default)]
struct Pending {
    responses: Mutex<HashMap<u64, oneshot::Sender<Result<Value, LegacySseError>>>>,
}

impl Pending {
    fn fail_all(&self, message: &str) {
        let mut responses = self
            .responses
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (_, sender) in responses.drain() {
            let _ = sender.send(Err(LegacySseError::Transport(message.to_string())));
        }
    }
}

/// Removes a pending request id synchronously on drop, including when the
/// request future is cancelled by a shorter outer timeout.
struct PendingGuard {
    pending: Arc<Pending>,
    id: u64,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        let _ = self
            .pending
            .responses
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.id);
    }
}

#[derive(Debug, Deserialize)]
struct ListToolsPage {
    #[serde(default)]
    tools: Vec<LegacyTool>,
    #[serde(rename = "nextCursor", default)]
    next_cursor: Option<String>,
}

fn default_input_schema() -> Value {
    json!({ "type": "object", "properties": {} })
}

fn build_headers(headers: &BTreeMap<String, String>) -> Result<HeaderMap, LegacySseError> {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        let header_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| LegacySseError::InvalidHeaderName(name.clone()))?;
        let header_value = HeaderValue::from_str(value)
            .map_err(|_| LegacySseError::InvalidHeaderValue(name.clone()))?;
        map.insert(header_name, header_value);
    }
    Ok(map)
}

fn handshake_remaining(deadline: Instant, total: Duration) -> Result<Duration, LegacySseError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(LegacySseError::HandshakeTimeout { duration: total });
    }
    Ok(remaining)
}

fn response_content_type(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_ascii_lowercase())
}

fn response_is_event_stream(response: &reqwest::Response) -> bool {
    response_content_type(response)
        .map(|value| value.contains("text/event-stream"))
        .unwrap_or(false)
}

/// Reads at most `max` bytes. The boolean is true when the body was longer and
/// the read stopped at the cap, before any truncation/formatting.
async fn read_body_capped(
    mut response: reqwest::Response,
    max: usize,
) -> Result<(Vec<u8>, bool), LegacySseError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() >= max {
            return Ok((body, true));
        }
        let remaining = max - body.len();
        if chunk.len() > remaining {
            body.extend_from_slice(&chunk[..remaining]);
            return Ok((body, true));
        }
        body.extend_from_slice(&chunk);
    }
    Ok((body, false))
}

enum PostBody {
    Response(Value),
    Acknowledgment,
}

fn classify_post_body(
    body: &[u8],
    content_type: Option<&str>,
    status: reqwest::StatusCode,
    expected_id: u64,
) -> Result<PostBody, LegacySseError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(PostBody::Acknowledgment);
    }
    let content_type = content_type.unwrap_or_default();
    let text = String::from_utf8_lossy(body);
    let value = if content_type.contains("application/json") {
        Some(serde_json::from_str::<Value>(&text).map_err(|_| {
            LegacySseError::Protocol(format!(
                "legacy SSE POST declared application/json but returned invalid JSON ({} bytes)",
                body.len()
            ))
        })?)
    } else if content_type.is_empty()
        || status == reqwest::StatusCode::ACCEPTED
        || content_type.contains("text/plain")
    {
        serde_json::from_str::<Value>(&text).ok()
    } else {
        return Err(LegacySseError::UnexpectedContentType {
            content_type: truncate_for_error(content_type),
        });
    };

    match value {
        Some(value) if value.get("id").is_some() => {
            rpc_result(&value, expected_id).map(PostBody::Response)
        }
        _ => Ok(PostBody::Acknowledgment),
    }
}

/// Parses a POST response for the transport path. Accepted shapes are a
/// direct JSON-RPC response, an SSE body carrying `message` events, or an
/// empty/`202`/`text/plain` acknowledgement.
fn parse_post_message_body(
    body: &[u8],
    content_type: Option<&str>,
    status: reqwest::StatusCode,
    expected_id: Option<&Value>,
) -> Result<Option<Value>, LegacySseError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    let content_type = content_type.unwrap_or_default();
    if content_type.contains("application/json") {
        let value = serde_json::from_slice::<Value>(body).map_err(|_| {
            LegacySseError::Protocol(format!(
                "legacy SSE POST declared application/json but returned invalid JSON ({} bytes)",
                body.len()
            ))
        })?;
        return response_value(value, expected_id);
    }
    if content_type.contains("text/event-stream") {
        let mut parser = SseParser::default();
        let mut events = parser.push(body)?;
        if let Some(event) = parser.finish() {
            events.push(event);
        }
        let mut response = None;
        for event in events {
            if event.event != "message" {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(&event.data) else {
                continue;
            };
            if value.get("method").is_some() || value.get("id").is_none() {
                continue;
            }
            if let Some(value) = response_value(value, expected_id)? {
                response = Some(value);
            }
        }
        return Ok(response);
    }
    if content_type.is_empty()
        || status == reqwest::StatusCode::ACCEPTED
        || content_type.contains("text/plain")
    {
        let Ok(value) = serde_json::from_slice::<Value>(body) else {
            return Ok(None);
        };
        return response_value(value, expected_id);
    }
    Err(LegacySseError::UnexpectedContentType {
        content_type: truncate_for_error(content_type),
    })
}

/// Validates a direct/embedded JSON-RPC response against the request id.
/// A body without an id is an acknowledgement, not a response.
fn response_value(
    value: Value,
    expected_id: Option<&Value>,
) -> Result<Option<Value>, LegacySseError> {
    let Some(actual_id) = value.get("id") else {
        return Ok(None);
    };
    let matches = expected_id
        .map(|expected| json_ids_match(expected, actual_id))
        .unwrap_or(true);
    if !matches {
        let expected = expected_id.expect("matches is false only with an expected id");
        return Err(LegacySseError::Protocol(format!(
            "JSON-RPC response id {actual_id} does not match request id {expected}"
        )));
    }
    Ok(Some(value))
}

/// Numeric ids may be encoded as numbers or numeric strings on the wire.
fn json_ids_match(expected: &Value, actual: &Value) -> bool {
    if expected == actual {
        return true;
    }
    let numeric = |value: &Value| {
        value
            .as_u64()
            .or_else(|| value.as_str().and_then(|text| text.parse::<u64>().ok()))
    };
    match (numeric(expected), numeric(actual)) {
        (Some(expected), Some(actual)) => expected == actual,
        _ => false,
    }
}

fn jsonrpc_id(value: &Value) -> Option<u64> {
    value.get("id").and_then(Value::as_u64)
}

fn rpc_result(value: &Value, expected_id: u64) -> Result<Value, LegacySseError> {
    let object = value
        .as_object()
        .ok_or_else(|| LegacySseError::Protocol("JSON-RPC response is not an object".into()))?;
    let id = object.get("id").and_then(Value::as_u64);
    if id != Some(expected_id) {
        return Err(LegacySseError::Protocol(format!(
            "JSON-RPC response id {id:?} does not match request id {expected_id}"
        )));
    }
    if let Some(error) = object.get("error") {
        // Some servers include `"error": null` next to a valid result.
        if !error.is_null() {
            let code = error.get("code").and_then(Value::as_i64).unwrap_or(-32000);
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown JSON-RPC error");
            return Err(LegacySseError::Rpc {
                code,
                message: truncate_for_error(message),
            });
        }
    }
    object
        .get("result")
        .cloned()
        .ok_or_else(|| LegacySseError::Protocol("response has neither result nor error".into()))
}

fn truncate_for_error(text: &str) -> String {
    let mut output: String = text.chars().take(MAX_ERROR_BODY_CHARS).collect();
    if text.chars().count() > MAX_ERROR_BODY_CHARS {
        output.push('…');
    }
    output
}

async fn read_stream(
    mut response: reqwest::Response,
    endpoint_sender: Option<oneshot::Sender<String>>,
    pending: Arc<Pending>,
    inbound: broadcast::Sender<Value>,
) {
    let mut endpoint_sender = endpoint_sender;
    let mut parser = SseParser::default();
    loop {
        match response.chunk().await {
            Ok(Some(bytes)) => match parser.push(&bytes) {
                Ok(events) => {
                    for event in events {
                        handle_event(event, &mut endpoint_sender, &pending, &inbound);
                    }
                }
                Err(error) => {
                    pending.fail_all(&error.to_string());
                    return;
                }
            },
            Ok(None) => {
                if let Some(event) = parser.finish() {
                    handle_event(event, &mut endpoint_sender, &pending, &inbound);
                }
                pending.fail_all("legacy SSE stream ended");
                return;
            }
            Err(error) => {
                pending.fail_all(&LegacySseError::from(error).to_string());
                return;
            }
        }
    }
}

fn handle_event(
    event: SseEvent,
    endpoint_sender: &mut Option<oneshot::Sender<String>>,
    pending: &Pending,
    inbound: &broadcast::Sender<Value>,
) {
    if event.event == "endpoint" {
        if let Some(sender) = endpoint_sender.take() {
            let _ = sender.send(event.data);
        }
        return;
    }
    if event.event != "message" {
        return;
    }
    let Ok(value) = serde_json::from_str::<Value>(&event.data) else {
        return;
    };
    // Transport consumers (rmcp) correlate ids themselves and also need
    // server-initiated requests and notifications. Sending to a channel with
    // no subscribers is a no-op.
    let _ = inbound.send(value.clone());
    // A server-initiated request also has an id; never mistake it for the
    // response to one of our calls.
    if value.get("method").is_some() {
        return;
    }
    let Some(id) = jsonrpc_id(&value) else {
        return;
    };
    let mut responses = pending
        .responses
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(sender) = responses.remove(&id) {
        let _ = sender.send(rpc_result(&value, id));
    }
}

/// Resolves the `endpoint` event against the configured SSE URL and enforces
/// the same-origin rule: same scheme (no `https` → `http` downgrade), host,
/// and port. Exposed for transport tests; the client always uses it before
/// the first POST, so a hostile endpoint never receives the configured
/// headers.
pub fn resolve_endpoint(base: &str, endpoint: &str) -> Result<String, LegacySseError> {
    let trimmed = endpoint.trim();
    if trimmed.is_empty() {
        return Err(LegacySseError::InvalidEndpoint {
            endpoint: redact_url(trimmed),
            reason: "is empty",
        });
    }
    if trimmed
        .chars()
        .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Err(LegacySseError::InvalidEndpoint {
            endpoint: redact_url(trimmed),
            reason: "contains whitespace or control characters",
        });
    }

    let base_url = Url::parse(base).map_err(|_| LegacySseError::InvalidEndpoint {
        endpoint: redact_url(base),
        reason: "configured SSE URL is invalid",
    })?;
    if base_url.scheme() != "http" && base_url.scheme() != "https" {
        return Err(LegacySseError::InvalidEndpoint {
            endpoint: redact_url(base),
            reason: "configured SSE URL is not http or https",
        });
    }

    let lower = trimmed.to_ascii_lowercase();
    let resolved = if lower.starts_with("http://") || lower.starts_with("https://") {
        Url::parse(trimmed)
    } else {
        // Protocol-relative endpoints resolve to the base scheme, then fail
        // the host check below if they point elsewhere.
        base_url.join(trimmed)
    }
    .map_err(|_| LegacySseError::InvalidEndpoint {
        endpoint: redact_url(trimmed),
        reason: "is not a valid URL",
    })?;

    if !resolved.username().is_empty() || resolved.password().is_some() {
        return Err(LegacySseError::InvalidEndpoint {
            endpoint: redact_url(resolved.as_str()),
            reason: "must not contain credentials",
        });
    }

    if resolved.scheme() != base_url.scheme() {
        let reason = if base_url.scheme() == "https" && resolved.scheme() == "http" {
            "refuses an https-to-http scheme downgrade"
        } else {
            "scheme does not match the configured SSE URL"
        };
        return Err(LegacySseError::CrossOriginEndpoint {
            endpoint: redact_url(resolved.as_str()),
            reason,
        });
    }
    if resolved.host_str() != base_url.host_str()
        || resolved.port_or_known_default() != base_url.port_or_known_default()
    {
        return Err(LegacySseError::CrossOriginEndpoint {
            endpoint: redact_url(resolved.as_str()),
            reason: "host or port differs from the configured SSE URL",
        });
    }
    Ok(resolved.to_string())
}

/// Removes credentials, query, and fragment before a URL reaches Debug or an
/// error. Query strings commonly carry SSE session ids.
fn redact_url(raw: &str) -> String {
    let parsed = Url::parse(raw).or_else(|_| Url::parse(&format!("https:{raw}")));
    let mut redacted = match parsed {
        Ok(mut url) => {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_query(None);
            url.set_fragment(None);
            url.to_string()
        }
        Err(_) => raw.split(['?', '#']).next().unwrap_or_default().to_string(),
    };
    if redacted.chars().count() > MAX_URL_DISPLAY_CHARS {
        redacted = redacted
            .chars()
            .take(MAX_URL_DISPLAY_CHARS.saturating_sub(1))
            .collect();
        redacted.push('…');
    }
    if redacted.is_empty() {
        "<empty>".to_string()
    } else {
        redacted
    }
}

#[derive(Debug)]
struct SseEvent {
    event: String,
    data: String,
}

#[derive(Debug, Default)]
struct SseParser {
    buffer: Vec<u8>,
    event: Option<String>,
    data: String,
    has_data: bool,
}

impl SseParser {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, LegacySseError> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(position) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut line: Vec<u8> = self.buffer.drain(..=position).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.is_empty() {
                if let Some(event) = self.dispatch() {
                    events.push(event);
                }
                continue;
            }
            let line = String::from_utf8_lossy(&line);
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = match line.split_once(':') {
                Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
                None => (line.as_ref(), ""),
            };
            match field {
                "event" => self.event = Some(value.to_string()),
                "data" => {
                    if self.has_data {
                        self.data.push('\n');
                    }
                    self.data.push_str(value);
                    self.has_data = true;
                    if self.data.len() > MAX_SSE_EVENT_BYTES {
                        return Err(LegacySseError::Protocol(format!(
                            "SSE event exceeded {MAX_SSE_EVENT_BYTES} bytes"
                        )));
                    }
                }
                _ => {}
            }
        }
        if self.buffer.len() > MAX_SSE_EVENT_BYTES {
            return Err(LegacySseError::Protocol(format!(
                "SSE event exceeded {MAX_SSE_EVENT_BYTES} bytes"
            )));
        }
        Ok(events)
    }

    fn finish(&mut self) -> Option<SseEvent> {
        self.dispatch()
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        let data = std::mem::take(&mut self.data);
        let has_data = std::mem::take(&mut self.has_data);
        let event = self.event.take().unwrap_or_else(|| "message".to_string());
        if !has_data {
            return None;
        }
        Some(SseEvent { event, data })
    }
}

/// Everything that can go wrong on a legacy SSE connection.
#[derive(Debug, Error)]
pub enum LegacySseError {
    #[error("legacy SSE handshake timed out after {duration:?}")]
    HandshakeTimeout { duration: Duration },
    #[error("legacy SSE call timed out after {duration:?}")]
    CallTimeout { duration: Duration },
    #[error("legacy SSE discovery timed out after {duration:?}")]
    DiscoveryTimeout { duration: Duration },
    #[error("legacy SSE discovery exceeded the {max}-tool cap")]
    TooManyTools { max: usize },
    #[error("legacy SSE stream closed before a response arrived")]
    StreamClosed,
    #[error("legacy SSE stream failed: {0}")]
    Transport(String),
    #[error("legacy SSE server did not send an endpoint event")]
    MissingEndpoint,
    #[error("invalid legacy SSE endpoint {endpoint:?}: {reason}")]
    InvalidEndpoint {
        endpoint: String,
        reason: &'static str,
    },
    #[error("legacy SSE endpoint {endpoint} is cross-origin: {reason}")]
    CrossOriginEndpoint {
        endpoint: String,
        reason: &'static str,
    },
    #[error("legacy SSE POST returned a body larger than {max} bytes")]
    ResponseTooLarge { max: usize },
    #[error("legacy SSE POST returned unexpected Content-Type {content_type:?}")]
    UnexpectedContentType { content_type: String },
    #[error("HTTP {status} from {url}: {body}")]
    Http {
        status: u16,
        url: String,
        body: String,
    },
    #[error("JSON-RPC error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("invalid JSON-RPC response: {0}")]
    Protocol(String),
    #[error("invalid JSON from legacy SSE server: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid HTTP header name {0:?}")]
    InvalidHeaderName(String),
    #[error("invalid value for HTTP header {0:?}")]
    InvalidHeaderValue(String),
    #[error("invalid MCP tool name")]
    InvalidToolName,
    #[error("MCP tool arguments must be a JSON object")]
    ArgumentsNotObject,
}

impl From<reqwest::Error> for LegacySseError {
    fn from(error: reqwest::Error) -> Self {
        // reqwest formats the request URL into its Display output; that URL
        // can carry a bearer/session query. Keep it out of errors.
        Self::Transport(error.without_url().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_endpoint_and_message_events_across_chunks() {
        let mut parser = SseParser::default();
        let mut events = parser
            .push(b"event: endpoint\ndata: /messages?session=1\n\n")
            .expect("parse");
        events.extend(
            parser
                .push(b"event: message\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,")
                .expect("parse"),
        );
        events.extend(
            parser
                .push(b"\"result\":{\"ok\":true}}\r\n\r\n")
                .expect("parse"),
        );

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event, "endpoint");
        assert_eq!(events[0].data, "/messages?session=1");
        assert_eq!(events[1].event, "message");
        assert_eq!(
            serde_json::from_str::<Value>(&events[1].data).expect("json")["result"]["ok"],
            true
        );
    }

    #[test]
    fn parses_multiline_data_and_comments() {
        let mut parser = SseParser::default();
        let events = parser
            .push(b": keepalive\nevent: message\ndata: first\ndata: second\n\n")
            .expect("parse");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "first\nsecond");
    }

    #[test]
    fn resolves_same_origin_root_and_relative_endpoints() {
        let base = "https://example.test/api/sse?x=1";
        assert_eq!(
            resolve_endpoint(base, "https://example.test/messages").expect("absolute"),
            "https://example.test/messages"
        );
        assert_eq!(
            resolve_endpoint(base, "/messages?session=1").expect("root"),
            "https://example.test/messages?session=1"
        );
        assert_eq!(
            resolve_endpoint(base, "messages?session=1").expect("relative"),
            "https://example.test/api/messages?session=1"
        );
        assert_eq!(
            resolve_endpoint(base, "?session=1").expect("query"),
            "https://example.test/api/sse?session=1"
        );
    }

    #[test]
    fn rejects_invalid_endpoints() {
        assert!(matches!(
            resolve_endpoint("https://example.test/sse", ""),
            Err(LegacySseError::InvalidEndpoint { .. })
        ));
        assert!(matches!(
            resolve_endpoint("https://example.test/sse", "/bad path"),
            Err(LegacySseError::InvalidEndpoint { .. })
        ));
    }

    #[test]
    fn rejects_cross_origin_and_scheme_downgrade_endpoints() {
        for endpoint in ["https://evil.test/collect", "//evil.test/collect"] {
            let error = resolve_endpoint("https://example.test/sse?session=secret", endpoint)
                .expect_err("cross-origin endpoint");
            assert!(
                matches!(error, LegacySseError::CrossOriginEndpoint { .. }),
                "{endpoint}: {error:?}"
            );
            assert!(!error.to_string().contains("secret"), "{error}");
        }

        let downgrade =
            resolve_endpoint("https://example.test/sse", "http://example.test/messages")
                .expect_err("downgrade");
        assert!(
            matches!(
                downgrade,
                LegacySseError::CrossOriginEndpoint {
                    reason: "refuses an https-to-http scheme downgrade",
                    ..
                }
            ),
            "{downgrade:?}"
        );
    }

    #[test]
    fn validates_jsonrpc_errors_and_ids() {
        let error = rpc_result(
            &json!({"jsonrpc": "2.0", "id": 7, "error": {"code": -32601, "message": "nope"}}),
            7,
        )
        .expect_err("RPC error");
        assert!(matches!(error, LegacySseError::Rpc { code: -32601, .. }));

        assert!(matches!(
            rpc_result(&json!({"jsonrpc": "2.0", "id": 8, "result": {}}), 7),
            Err(LegacySseError::Protocol(_))
        ));
        assert_eq!(
            rpc_result(
                &json!({"jsonrpc": "2.0", "id": 7, "result": {"ok": true}}),
                7
            )
            .expect("result"),
            json!({"ok": true})
        );
        assert_eq!(
            rpc_result(
                &json!({
                    "jsonrpc": "2.0",
                    "id": 7,
                    "error": null,
                    "result": {"ok": true}
                }),
                7
            )
            .expect("null error with result is success"),
            json!({"ok": true})
        );
    }

    #[test]
    fn pending_guard_removes_the_id_on_drop() {
        let pending = Arc::new(Pending::default());
        let (sender, _receiver) = oneshot::channel();
        pending.responses.lock().expect("lock").insert(7, sender);
        {
            let _guard = PendingGuard {
                pending: Arc::clone(&pending),
                id: 7,
            };
        }
        assert!(pending.responses.lock().expect("lock").is_empty());
    }

    #[test]
    fn converts_a_validated_sse_server_only() {
        let sse = dhi_mcp::ValidatedServer {
            name: "events".to_string(),
            segment: "events".to_string(),
            transport: dhi_mcp::McpTransport::Sse,
            call_timeout: Duration::from_secs(9),
            transport_data: dhi_mcp::McpTransportData::Sse {
                url: "https://example.test/sse".to_string(),
                headers: BTreeMap::from([(
                    "Authorization".to_string(),
                    "Bearer test-token".to_string(),
                )]),
                insecure: true,
            },
            visible_to: None,
            tool_visibility: BTreeMap::new(),
            deferred_threshold: None,
        };
        let config = LegacySseConfig::from_validated(&sse).expect("SSE config");
        assert_eq!(config.url, "https://example.test/sse");
        assert_eq!(config.call_timeout, Duration::from_secs(9));
        assert!(config.insecure);
        assert_eq!(config.headers["Authorization"], "Bearer test-token");

        let http = dhi_mcp::ValidatedServer {
            transport: dhi_mcp::McpTransport::Http,
            transport_data: dhi_mcp::McpTransportData::Http {
                url: "https://example.test/mcp".to_string(),
                headers: BTreeMap::new(),
                insecure: false,
            },
            ..sse
        };
        assert!(LegacySseConfig::from_validated(&http).is_err());
    }

    #[test]
    fn debug_does_not_print_secrets_or_session_ids() {
        let config =
            LegacySseConfig::new("https://user:pass@example.test/sse?session=super-secret-session")
                .with_bearer_token("top-secret");
        let debug = format!("{config:?}");
        assert!(!debug.contains("top-secret"), "{debug}");
        assert!(!debug.contains("pass"), "{debug}");
        assert!(!debug.contains("super-secret-session"), "{debug}");
        assert!(debug.contains("Authorization"), "{debug}");
        assert!(debug.contains("example.test"), "{debug}");
    }
}
