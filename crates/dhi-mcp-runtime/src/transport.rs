//! Transport construction for MCP servers.
//!
//! [`establish`] turns one validated server config into a connected
//! [`McpConnection`]: it builds the Streamable HTTP client, the stdio child
//! process transport, or the legacy SSE transport, then runs the manager's
//! bounded handshake through the generic `connect_shared`. Keeping this split
//! lets transport work change without touching the manager's
//! connection/discovery flow.
//!
//! Legacy SSE is the pre-Streamable-HTTP MCP transport: `GET <url>` with
//! `Accept: text/event-stream`, an `endpoint` event with the POST URL, then
//! JSON-RPC requests POSTed to that endpoint with responses arriving on the
//! still-open event stream. `dhi-mcp-upgrade`'s [`LegacySseClient`] implements
//! the wire protocol; [`LegacySseTransport`] adapts it to rmcp's
//! [`Transport`] trait so the manager's handshake, discovery, and guarded
//! dispatch paths stay exactly the same as for the other transports.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use dhi_mcp_upgrade::legacy_sse::{
    LEGACY_PROTOCOL_VERSION, LegacySseClient, LegacySseConfig, LegacySseError,
};
// rmcp 3.8 builds against reqwest 0.13 (through rig's bundled re-export),
// while the rest of dhi-server uses the workspace's reqwest 0.12. Use the
// same 0.13 type rmcp's `StreamableHttpClient` impl is written for.
use rig::rig_reqwest::reqwest::Client as HttpClient;
use rig::rig_reqwest::reqwest::header::{HeaderName, HeaderValue};
use rig::tool::rmcp::McpConnection;
use rmcp::RoleClient;
use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess, Transport};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};
use tracing::warn;

use super::config::{CONNECT_TIMEOUT, ValidatedServer};
use super::manager::McpManager;

/// The transports a config can actually connect to.
#[derive(Clone, PartialEq, Eq)]
pub enum ValidTransport {
    Http {
        url: String,
        headers: BTreeMap<String, String>,
        insecure: bool,
    },
    /// Legacy MCP SSE (`GET /sse` + `endpoint` event + POST message URL).
    Sse {
        url: String,
        headers: BTreeMap<String, String>,
        insecure: bool,
    },
    Stdio {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    },
}

/// Debug never prints header or environment *values*: those routinely carry
/// bearer tokens and API keys. Names are safe and useful for diagnostics.
impl fmt::Debug for ValidTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http {
                url,
                headers,
                insecure,
            } => formatter
                .debug_struct("Http")
                .field("url", &redacted_url(url))
                .field("header_names", &header_names(headers))
                .field("insecure", insecure)
                .finish(),
            Self::Sse {
                url,
                headers,
                insecure,
            } => formatter
                .debug_struct("Sse")
                .field("url", &redacted_url(url))
                .field("header_names", &header_names(headers))
                .field("insecure", insecure)
                .finish(),
            Self::Stdio { command, args, env } => formatter
                .debug_struct("Stdio")
                .field("command", command)
                .field("args", args)
                .field("env_names", &env.keys().collect::<Vec<_>>())
                .finish(),
        }
    }
}

fn header_names(headers: &BTreeMap<String, String>) -> Vec<&String> {
    headers.keys().collect()
}

/// Strips credentials, query, and fragment before a URL reaches Debug.
/// Query strings commonly carry SSE session ids and tokens.
fn redacted_url(raw: &str) -> String {
    let mut url = raw.split(['?', '#']).next().unwrap_or_default().to_string();
    if let Some(scheme_end) = url.find("://") {
        let authority_start = scheme_end + 3;
        if let Some(at) = url[authority_start..].find('@') {
            let authority = &url[authority_start..authority_start + at];
            if authority.contains(':') {
                url.replace_range(authority_start..authority_start + at + 1, "");
            }
        }
    }
    url
}

/// Converts configured HTTP headers into reqwest's header types, rejecting
/// names or values the HTTP stack cannot represent. The config crate already
/// refuses CR/LF (so no header injection reaches here); this catches the
/// remaining invalid characters. Values are never echoed in the error.
pub(crate) fn http_headers(
    server: &str,
    headers: &BTreeMap<String, String>,
) -> anyhow::Result<HashMap<HeaderName, HeaderValue>> {
    let mut converted = HashMap::with_capacity(headers.len());
    for (name, value) in headers {
        let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|err| {
            anyhow!("MCP server {server:?} has an invalid header name {name:?}: {err}")
        })?;
        let header_value = HeaderValue::try_from(value.as_str()).map_err(|err| {
            anyhow!("MCP server {server:?} header {name:?} has an invalid value: {err}")
        })?;
        converted.insert(header_name, header_value);
    }
    Ok(converted)
}

/// The lowercase transport label used by [`McpServerStatus`].
///
/// [`McpServerStatus`]: super::types::McpServerStatus
pub(crate) fn transport_label(transport: &ValidTransport) -> &'static str {
    match transport {
        ValidTransport::Http { .. } => "http",
        ValidTransport::Sse { .. } => "sse",
        ValidTransport::Stdio { .. } => "stdio",
    }
}

/// Builds the configured transport for `server` and completes the bounded
/// rmcp handshake. Every transport kind goes through the same
/// [`McpManager::connect_shared`] path, so startup has one timeout and one
/// client-info/notification handler regardless of transport.
pub(crate) async fn establish(
    manager: &Arc<McpManager>,
    server: &ValidatedServer,
) -> anyhow::Result<McpConnection> {
    match &server.transport {
        ValidTransport::Http {
            url,
            headers,
            insecure,
        } => {
            let mut builder = HttpClient::builder();
            if *insecure {
                builder = builder.danger_accept_invalid_certs(true);
            }
            let client = builder.build().map_err(|err| {
                anyhow!(
                    "MCP server {:?} HTTP client could not be built: {err}",
                    server.name
                )
            })?;
            let config = StreamableHttpClientTransportConfig::with_uri(url.clone())
                .custom_headers(http_headers(&server.name, headers)?);
            let transport = StreamableHttpClientTransport::with_client(client, config);
            manager.connect_shared(server, transport).await
        }
        ValidTransport::Sse {
            url,
            headers,
            insecure,
        } => {
            // `open` performs only the GET + `endpoint` handshake; rmcp's
            // service sends `initialize` through the adapter below, exactly
            // as it does for any other transport. The core client bounds each
            // stage; the outer timeout also covers a stalled TLS/connect setup.
            let config = LegacySseConfig {
                url: url.clone(),
                headers: headers.clone(),
                handshake_timeout: CONNECT_TIMEOUT,
                call_timeout: server.call_timeout,
                discovery_timeout: CONNECT_TIMEOUT,
                insecure: *insecure,
            };
            let client = tokio::time::timeout(CONNECT_TIMEOUT, LegacySseClient::open(config))
                .await
                .map_err(|_| {
                    anyhow!(
                        "MCP server {:?} legacy SSE handshake timed out after {}s",
                        server.name,
                        CONNECT_TIMEOUT.as_secs()
                    )
                })?
                .map_err(|err| {
                    anyhow!(
                        "MCP server {:?} legacy SSE connection failed: {err}",
                        server.name
                    )
                })?;
            let transport = LegacySseTransport::new(client, CONNECT_TIMEOUT, server.call_timeout);
            manager.connect_shared(server, transport).await
        }
        ValidTransport::Stdio { command, args, env } => {
            let mut command = tokio::process::Command::new(command);
            command.args(args).envs(env);
            let transport = TokioChildProcess::new(command)
                .map_err(|err| anyhow!("MCP server {:?} could not spawn: {err}", server.name))?;
            manager.connect_shared(server, transport).await
        }
    }
}

/// An rmcp client transport over one legacy SSE connection.
///
/// Outbound JSON-RPC messages are POSTed to the endpoint learned from the
/// `endpoint` event. Inbound messages arrive on the GET stream and are
/// correlated by rmcp itself (the service matches response ids); a direct
/// POST response is forwarded on the same inbound path so both server styles
/// work. Every outbound message is bounded by the configured handshake or
/// call deadline.
struct LegacySseTransport {
    client: Arc<LegacySseClient>,
    inbound: broadcast::Receiver<Value>,
    direct: mpsc::UnboundedReceiver<Value>,
    direct_sender: mpsc::UnboundedSender<Value>,
    handshake_timeout: Duration,
    call_timeout: Duration,
    closed: bool,
}

impl LegacySseTransport {
    fn new(client: LegacySseClient, handshake_timeout: Duration, call_timeout: Duration) -> Self {
        let inbound = client.subscribe();
        let (direct_sender, direct) = mpsc::unbounded_channel();
        Self {
            client: Arc::new(client),
            inbound,
            direct,
            direct_sender,
            handshake_timeout,
            call_timeout,
            closed: false,
        }
    }
}

impl Transport<RoleClient> for LegacySseTransport {
    type Error = LegacySseTransportError;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let client = Arc::clone(&self.client);
        let direct = self.direct_sender.clone();
        let handshake_timeout = self.handshake_timeout;
        let call_timeout = self.call_timeout;
        async move {
            let mut payload =
                serde_json::to_value(&item).map_err(LegacySseTransportError::Encode)?;
            let method = payload
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let is_handshake = method == "initialize" || method == "notifications/initialized";
            if method == "initialize"
                && let Some(params) = payload.get_mut("params").and_then(Value::as_object_mut)
            {
                // A legacy SSE server speaks a pre-Streamable-HTTP protocol
                // version. rmcp's client info defaults to the newest version;
                // the legacy wire advertises the legacy version instead.
                params.insert(
                    "protocolVersion".to_string(),
                    Value::String(LEGACY_PROTOCOL_VERSION.to_string()),
                );
            }
            let limit = if is_handshake {
                handshake_timeout
            } else {
                call_timeout
            };
            let phase = if is_handshake { "handshake" } else { "call" };
            let outcome = tokio::time::timeout(limit, client.send_message(&payload))
                .await
                .map_err(|_| LegacySseTransportError::Timeout {
                    phase,
                    duration: limit,
                })?
                .map_err(LegacySseTransportError::Sse)?;
            if let Some(response) = outcome {
                // Unbounded: the response is received before rmcp starts
                // waiting, so a bounded channel could deadlock the POST.
                let _ = direct.send(response);
            }
            Ok(())
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        if self.closed {
            return None;
        }
        loop {
            tokio::select! {
                direct = self.direct.recv() => match direct {
                    Some(value) => {
                        if let Some(message) = decode_inbound(value) {
                            return Some(message);
                        }
                    }
                    None => return None,
                },
                event = self.inbound.recv() => match event {
                    Ok(value) => {
                        if let Some(message) = decode_inbound(value) {
                            return Some(message);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        // Losing a response would still let rmcp time the
                        // request out; losing a notification only affects
                        // freshness. Keep the stream usable and warn.
                        warn!(
                            skipped,
                            "legacy SSE transport fell behind; dropped server messages"
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                },
            }
        }
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.closed = true;
        self.client.close();
        std::future::ready(Ok(()))
    }
}

fn decode_inbound(value: Value) -> Option<RxJsonRpcMessage<RoleClient>> {
    match serde_json::from_value(value) {
        Ok(message) => Some(message),
        Err(error) => {
            // A legacy server may emit non-JSON-RPC or newer-protocol
            // messages. Drop just that message; the stream stays usable.
            warn!(
                error = %error,
                "legacy SSE server sent a message the rmcp client cannot decode; dropped"
            );
            None
        }
    }
}

/// Errors from the legacy SSE rmcp transport. Everything is redacted: the
/// core client never puts header values or session query strings in errors.
#[derive(Debug)]
pub(crate) enum LegacySseTransportError {
    Sse(LegacySseError),
    Encode(serde_json::Error),
    Timeout {
        phase: &'static str,
        duration: Duration,
    },
}

impl fmt::Display for LegacySseTransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sse(error) => write!(formatter, "legacy SSE transport failed: {error}"),
            Self::Encode(error) => {
                write!(
                    formatter,
                    "legacy SSE message could not be encoded: {error}"
                )
            }
            Self::Timeout { phase, duration } => {
                write!(formatter, "legacy SSE {phase} timed out after {duration:?}")
            }
        }
    }
}

impl std::error::Error for LegacySseTransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sse(error) => Some(error),
            Self::Encode(error) => Some(error),
            Self::Timeout { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dhi_mcp_upgrade::legacy_sse::{LegacySseError, resolve_endpoint};

    fn headers() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("Authorization".to_string(), "Bearer top-secret".to_string()),
            ("X-Tenant".to_string(), "acme".to_string()),
        ])
    }

    #[test]
    fn transport_label_marks_legacy_sse() {
        let transport = ValidTransport::Sse {
            url: "https://example.test/sse".to_string(),
            headers: BTreeMap::new(),
            insecure: false,
        };
        assert_eq!(transport_label(&transport), "sse");
    }

    #[test]
    fn debug_lists_header_names_but_never_values() {
        let transport = ValidTransport::Sse {
            url: "https://example.test/sse?session=secret-session".to_string(),
            headers: headers(),
            insecure: true,
        };
        let debug = format!("{transport:?}");
        assert!(debug.contains("Authorization"), "{debug}");
        assert!(debug.contains("X-Tenant"), "{debug}");
        assert!(!debug.contains("top-secret"), "{debug}");
        assert!(
            !debug.contains("secret-session"),
            "url query/session id leaked: {debug}"
        );

        let stdio = ValidTransport::Stdio {
            command: "npx".to_string(),
            args: vec!["server".to_string()],
            env: HashMap::from([("API_KEY".to_string(), "top-secret".to_string())]),
        };
        let debug = format!("{stdio:?}");
        assert!(debug.contains("API_KEY"), "{debug}");
        assert!(!debug.contains("top-secret"), "{debug}");
    }

    #[test]
    fn legacy_endpoint_rejects_cross_origin_and_scheme_downgrade() {
        for endpoint in ["https://evil.test/collect", "//evil.test/collect"] {
            let error = resolve_endpoint("https://example.test/sse", endpoint)
                .expect_err("cross-origin endpoint must be rejected");
            assert!(
                matches!(error, LegacySseError::CrossOriginEndpoint { .. }),
                "{endpoint}: {error:?}"
            );
            assert!(!error.to_string().contains("Bearer"), "{error}");
        }

        let downgrade =
            resolve_endpoint("https://example.test/sse", "http://example.test/messages")
                .expect_err("https-to-http downgrade must be rejected");
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

        let different_port = resolve_endpoint(
            "https://example.test:8443/sse",
            "https://example.test/messages",
        )
        .expect_err("a different port is a different origin");
        assert!(
            matches!(
                different_port,
                LegacySseError::CrossOriginEndpoint {
                    reason: "host or port differs from the configured SSE URL",
                    ..
                }
            ),
            "{different_port:?}"
        );
    }
}
