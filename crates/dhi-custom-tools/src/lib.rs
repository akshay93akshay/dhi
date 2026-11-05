//! DHI custom tools.
//!
//! This crate owns the custom-tool catalog, registration, and execution
//! backend. The seam is wired by `dhi-server`; this block fills the catalog
//! and the backend.
//!
//! # Configuration
//!
//! `DHI_CUSTOM_TOOLS_CONFIG` selects the file (default `ops/custom-tools.json`).
//! A missing file is an empty catalog. Malformed entries and unknown fields
//! are skipped with a warning; boot never fails because of this file. The
//! catalog is capped at 64 tools and 64 KiB.
//!
//! # Execution
//!
//! - `http` tools resolve and classify the target before connecting, reject
//!   loopback/RFC1918/link-local/unique-local/metadata addresses unless
//!   `allow_private` is true, pin the checked DNS addresses, and disable
//!   redirects. GET/DELETE arguments become query parameters; POST/PUT/PATCH
//!   arguments become the JSON body. Fixed header values never appear in
//!   errors or `Debug`.
//! - `command` tools spawn the configured program directly, with no shell.
//!   `{name}` placeholders in `args` are replaced with string input values.
//!   The environment is cleared and rebuilt from `PATH` plus the configured
//!   `env` map. Control characters are rejected, and the process is killed on
//!   timeout or drop.
//!
//! # Limitations
//!
//! - Command timeouts kill the direct child; grandchildren are not tracked.
//! - HTTP response bodies are returned as lossy UTF-8 text, not decoded by
//!   content type.

mod backend;
mod command;
mod config;
mod http;
mod util;

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::{Arc, RwLock};

use anyhow::Result;
use dhi_agent::{GuardedToolSpec, register_guarded_tool};
use dhi_guard::ToolBackend;
use dhi_tool_source::{NamedBackend, ToolSource, ToolSourceContext};
use tracing::{info, warn};

use crate::backend::CustomToolBackend;
use crate::config::{CustomTool, load_catalog};

/// The process-lifetime custom-tools runtime.
pub struct CustomToolsRuntime {
    catalog: Arc<BTreeMap<String, CustomTool>>,
    /// Catalog names registration could not install because the shared
    /// registry already owned them. `CustomToolBackend` delegates those names
    /// instead of shadowing the existing tool.
    blocked: Arc<RwLock<HashSet<String>>>,
}

impl CustomToolsRuntime {
    /// Loads `DHI_CUSTOM_TOOLS_CONFIG` (default `ops/custom-tools.json`).
    /// An absent file is an empty catalog.
    pub fn from_env() -> Arc<Self> {
        Self::from_path(config::config_path())
    }

    /// Loads a catalog from an explicit path.
    pub fn from_path(path: impl AsRef<Path>) -> Arc<Self> {
        let path = path.as_ref();
        let catalog = load_catalog(path);
        for warning in &catalog.warnings {
            warn!(warning = %warning, "custom tool catalog warning");
        }
        info!(
            path = %path.display(),
            tools = catalog.tools.len(),
            "custom tool catalog loaded"
        );
        Arc::new(Self {
            catalog: Arc::new(catalog.tools),
            blocked: Arc::new(RwLock::new(HashSet::new())),
        })
    }

    /// Catalog names in stable order, for the tool catalog.
    pub fn names(&self) -> Vec<String> {
        self.catalog.keys().cloned().collect()
    }

    /// Wraps the delegate with the custom-tools backend. An empty catalog
    /// keeps the delegate unchanged.
    pub fn backend(self: &Arc<Self>, delegate: Arc<dyn ToolBackend>) -> Arc<dyn ToolBackend> {
        if self.catalog.is_empty() {
            return delegate;
        }
        let names: Vec<String> = self.catalog.keys().cloned().collect();
        let backend = Arc::new(CustomToolBackend {
            catalog: Arc::clone(&self.catalog),
            blocked: Arc::clone(&self.blocked),
            delegate: Arc::clone(&delegate),
        });
        Arc::new(NamedBackend::new(names, backend, delegate))
    }

    /// Test/utility constructor from an already validated catalog.
    #[cfg(test)]
    fn from_catalog(tools: BTreeMap<String, CustomTool>) -> Arc<Self> {
        Arc::new(Self {
            catalog: Arc::new(tools),
            blocked: Arc::new(RwLock::new(HashSet::new())),
        })
    }
}

impl ToolSource for CustomToolsRuntime {
    fn name(&self) -> &'static str {
        "custom-tools"
    }

    fn register(&self, context: &ToolSourceContext<'_>) -> Result<Vec<String>> {
        let mut registered = Vec::new();
        for tool in self.catalog.values() {
            if context.handle.toolset().contains(&tool.name) {
                warn!(
                    tool = %tool.name,
                    "custom tool skipped: a tool with that name is already registered"
                );
                self.blocked
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(tool.name.clone());
                continue;
            }
            match register_guarded_tool(
                context.handle,
                GuardedToolSpec::new(&tool.name, &tool.description, tool.input_schema.clone()),
                Arc::clone(&context.sink),
            ) {
                Ok(()) => {
                    self.blocked
                        .write()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&tool.name);
                    registered.push(tool.name.clone());
                }
                Err(err) => {
                    warn!(
                        tool = %tool.name,
                        error = %err,
                        "custom tool registration failed; skipped"
                    );
                    self.blocked
                        .write()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .insert(tool.name.clone());
                }
            }
        }
        Ok(registered)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use axum::Router;
    use axum::body::Body;
    use axum::extract::State;
    use axum::http::{Request, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::any;
    use dhi_agent::{GuardedToolSpec, ToolSink};
    use dhi_core::{ToolCallOutcome, ToolCallRequest};
    use dhi_guard::ToolBackend;
    use serde_json::{Value, json};
    use tokio::sync::mpsc;
    use uuid::Uuid;

    use super::*;

    fn runtime_from_json(value: Value) -> Arc<CustomToolsRuntime> {
        let catalog = config::parse_catalog(&value.to_string()).expect("valid catalog");
        assert_eq!(
            catalog.warnings.len(),
            0,
            "unexpected warnings: {:?}",
            catalog.warnings
        );
        assert!(!catalog.tools.is_empty());
        CustomToolsRuntime::from_catalog(catalog.tools)
    }

    fn command_tool(command: &str, args: Value) -> Value {
        json!({
            "description": "test command",
            "inputSchema": {"type": "object"},
            "exec": {"kind": "command", "command": command, "args": args}
        })
    }

    fn http_tool(url: &str, method: &str, extra: Value) -> Value {
        let mut exec = json!({
            "kind": "http",
            "method": method,
            "url": url,
            "allow_private": true
        });
        if let (Some(exec), Some(extra)) = (exec.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                exec.insert(key.clone(), value.clone());
            }
        }
        json!({
            "description": "test http",
            "inputSchema": {"type": "object"},
            "exec": exec
        })
    }

    struct MarkerBackend;

    #[async_trait]
    impl ToolBackend for MarkerBackend {
        async fn execute(&self, tool: &str, input: &Value) -> Result<Value> {
            Ok(json!({"delegate": tool, "input": input}))
        }
    }

    struct NoopSink;

    #[async_trait]
    impl ToolSink for NoopSink {
        async fn execute(
            &self,
            _task_id: Uuid,
            _intent_id: Uuid,
            _request: ToolCallRequest,
        ) -> anyhow::Result<ToolCallOutcome> {
            Ok(ToolCallOutcome::Allowed { output: json!({}) })
        }
    }

    #[tokio::test]
    async fn missing_catalog_keeps_the_delegate_unchanged() {
        let directory = tempfile::tempdir().expect("tempdir");
        let runtime = CustomToolsRuntime::from_path(directory.path().join("absent.json"));
        let backend = runtime.backend(Arc::new(MarkerBackend));
        let output = backend
            .execute("anything", &json!({"x": 1}))
            .await
            .expect("delegate");
        assert_eq!(output["delegate"], "anything");
    }

    #[tokio::test]
    async fn malformed_catalog_file_keeps_the_delegate_unchanged() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("broken.json");
        std::fs::write(&path, "{not json").expect("write");
        let runtime = CustomToolsRuntime::from_path(&path);
        let backend = runtime.backend(Arc::new(MarkerBackend));
        let output = backend
            .execute("anything", &json!({}))
            .await
            .expect("delegate");
        assert_eq!(output["delegate"], "anything");
    }

    #[tokio::test]
    async fn unknown_tool_reaches_the_delegate() {
        let runtime = runtime_from_json(json!({
            "tools": {"echo": command_tool("echo", json!(["{message}"]))}
        }));
        let backend = runtime.backend(Arc::new(MarkerBackend));
        let output = backend
            .execute("other", &json!({"x": 1}))
            .await
            .expect("delegate");
        assert_eq!(output, json!({"delegate": "other", "input": {"x": 1}}));
    }

    #[tokio::test]
    async fn non_object_input_fails_closed() {
        let runtime = runtime_from_json(json!({
            "tools": {"echo": command_tool("echo", json!(["{message}"]))}
        }));
        let backend = runtime.backend(Arc::new(MarkerBackend));
        let error = backend
            .execute("echo", &json!("not an object"))
            .await
            .expect_err("non-object input");
        assert!(error.to_string().contains("JSON object"), "{error}");
    }

    #[tokio::test]
    async fn registration_skips_an_existing_name_and_keeps_its_dispatch() {
        let runtime = runtime_from_json(json!({
            "tools": {"echo": command_tool("echo", json!(["{message}"]))}
        }));
        let handle = rig::tool::server::ToolServer::new().run();
        let existing_sink: Arc<dyn ToolSink> = Arc::new(NoopSink);
        register_guarded_tool(
            &handle,
            GuardedToolSpec::new("echo", "existing", json!({"type": "object"})),
            Arc::clone(&existing_sink),
        )
        .expect("pre-register");
        let registered = runtime
            .register(&ToolSourceContext {
                handle: &handle,
                sink: existing_sink,
            })
            .expect("registration");
        assert!(registered.is_empty());

        let backend = runtime.backend(Arc::new(MarkerBackend));
        let output = backend.execute("echo", &json!({})).await.expect("delegate");
        assert_eq!(output["delegate"], "echo");
    }

    #[tokio::test]
    async fn registration_returns_the_catalog_names() {
        let runtime = runtime_from_json(json!({
            "tools": {
                "echo": command_tool("echo", json!(["{message}"])),
                "weather": http_tool("https://example.com/weather", "GET", json!({}))
            }
        }));
        let handle = rig::tool::server::ToolServer::new().run();
        let registered = runtime
            .register(&ToolSourceContext {
                handle: &handle,
                sink: Arc::new(NoopSink),
            })
            .expect("registration");
        assert_eq!(registered, ["echo", "weather"]);
        assert!(handle.toolset().contains("echo"));
        assert!(handle.toolset().contains("weather"));
    }

    // -- fake HTTP server ---------------------------------------------------

    #[derive(Debug)]
    struct Captured {
        method: String,
        uri: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl Captured {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(header, _)| header.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        }
    }

    #[derive(Clone)]
    struct FakeState {
        tx: mpsc::UnboundedSender<Captured>,
    }

    async fn fake_handler(State(state): State<FakeState>, request: Request<Body>) -> Response {
        let method = request.method().to_string();
        let uri = request.uri().to_string();
        let path = request.uri().path().to_string();
        let headers = request
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    value.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        let body = axum::body::to_bytes(request.into_body(), 1024 * 1024)
            .await
            .unwrap_or_default()
            .to_vec();
        let _ = state.tx.send(Captured {
            method,
            uri,
            headers,
            body: body.clone(),
        });
        match path.as_str() {
            "/slow" => {
                tokio::time::sleep(Duration::from_secs(2)).await;
                (StatusCode::OK, "late").into_response()
            }
            "/big" => (StatusCode::OK, "x".repeat(10_000)).into_response(),
            "/echo" => {
                (StatusCode::OK, String::from_utf8_lossy(&body).into_owned()).into_response()
            }
            _ => (StatusCode::OK, [("x-fake", "yes")], "sunny").into_response(),
        }
    }

    async fn fake_server() -> (String, mpsc::UnboundedReceiver<Captured>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let app = Router::new()
            .fallback(any(fake_handler))
            .with_state(FakeState { tx });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{address}"), rx)
    }

    #[tokio::test]
    async fn http_get_appends_query_parameters_and_returns_the_body() {
        let (base, mut requests) = fake_server().await;
        let runtime = runtime_from_json(json!({
            "tools": {
                "weather": http_tool(
                    &format!("{base}/weather"),
                    "GET",
                    json!({"headers": {"X-Key": "secret-value"}})
                )
            }
        }));
        let backend = runtime.backend(Arc::new(MarkerBackend));
        let output = backend
            .execute("weather", &json!({"city": "Paris", "days": 3}))
            .await
            .expect("http get");

        assert_eq!(output["status"], 200);
        assert_eq!(output["body"], "sunny");
        assert_eq!(output["headers"]["x-fake"], "yes");
        assert_eq!(output["truncated"], false);

        let captured = requests.recv().await.expect("request");
        assert_eq!(captured.method, "GET");
        assert!(captured.uri.contains("city=Paris"), "{}", captured.uri);
        assert!(captured.uri.contains("days=3"), "{}", captured.uri);
        assert_eq!(captured.header("x-key"), Some("secret-value"));
        assert!(captured.body.is_empty());
    }

    #[tokio::test]
    async fn http_post_sends_the_argument_object_as_json() {
        let (base, mut requests) = fake_server().await;
        let runtime = runtime_from_json(json!({
            "tools": {
                "echo": http_tool(
                    &format!("{base}/echo"),
                    "POST",
                    json!({"headers": {"X-Key": "secret-value"}})
                )
            }
        }));
        let backend = runtime.backend(Arc::new(MarkerBackend));
        let input = json!({"message": "hello", "count": 2});
        let output = backend.execute("echo", &input).await.expect("http post");

        assert_eq!(output["status"], 200);
        let echoed: Value =
            serde_json::from_str(output["body"].as_str().expect("body")).expect("json");
        assert_eq!(echoed, input);

        let captured = requests.recv().await.expect("request");
        assert_eq!(captured.method, "POST");
        assert_eq!(captured.header("content-type"), Some("application/json"));
        let sent: Value = serde_json::from_slice(&captured.body).expect("body json");
        assert_eq!(sent, input);
    }

    #[tokio::test]
    async fn private_target_is_rejected_without_opt_in() {
        let (base, mut requests) = fake_server().await;
        let runtime = runtime_from_json(json!({
            "tools": {
                "weather": {
                    "description": "test http",
                    "inputSchema": {"type": "object"},
                    "exec": {"kind": "http", "method": "GET", "url": format!("{base}/weather")}
                }
            }
        }));
        let backend = runtime.backend(Arc::new(MarkerBackend));
        let error = backend
            .execute("weather", &json!({}))
            .await
            .expect_err("private target");
        assert!(error.to_string().contains("private"), "{error}");
        assert!(requests.try_recv().is_err(), "no request should be sent");
    }

    #[tokio::test]
    async fn http_timeout_is_clear() {
        let (base, _requests) = fake_server().await;
        let runtime = runtime_from_json(json!({
            "tools": {
                "slow": http_tool(
                    &format!("{base}/slow"),
                    "GET",
                    json!({"timeout_secs": 1})
                )
            }
        }));
        let backend = runtime.backend(Arc::new(MarkerBackend));
        let error = backend
            .execute("slow", &json!({}))
            .await
            .expect_err("timeout");
        assert!(error.to_string().contains("timed out"), "{error}");
    }

    #[tokio::test]
    async fn http_response_body_is_capped() {
        let (base, _requests) = fake_server().await;
        let runtime = runtime_from_json(json!({
            "tools": {
                "big": http_tool(
                    &format!("{base}/big"),
                    "GET",
                    json!({"max_response_bytes": 100})
                )
            }
        }));
        let backend = runtime.backend(Arc::new(MarkerBackend));
        let output = backend.execute("big", &json!({})).await.expect("capped");
        let body = output["body"].as_str().expect("body");
        assert_eq!(body.len(), 100);
        assert_eq!(output["truncated"], true);
    }

    #[tokio::test]
    async fn header_values_never_appear_in_errors() {
        let runtime = runtime_from_json(json!({
            "tools": {
                "weather": {
                    "description": "test http",
                    "inputSchema": {"type": "object"},
                    "exec": {
                        "kind": "http",
                        "method": "GET",
                        "url": "http://127.0.0.1:9/weather",
                        "allow_private": true,
                        "headers": {"X-Key": "super-secret-value"}
                    }
                }
            }
        }));
        let backend = runtime.backend(Arc::new(MarkerBackend));
        let error = backend
            .execute("weather", &json!({}))
            .await
            .expect_err("connection refused");
        let text = format!("{error:#}");
        assert!(!text.contains("super-secret-value"), "{text}");
    }

    #[tokio::test]
    async fn command_echo_runs_without_a_shell() {
        let runtime = runtime_from_json(json!({
            "tools": {"echo": command_tool("echo", json!(["{message}"]))}
        }));
        let backend = runtime.backend(Arc::new(MarkerBackend));
        let output = backend
            .execute("echo", &json!({"message": "hello"}))
            .await
            .expect("echo");
        assert_eq!(output["success"], true);
        assert_eq!(output["exit_code"], 0);
        assert_eq!(output["stdout"], "hello\n");
        assert_eq!(output["truncated"], false);
    }

    #[tokio::test]
    async fn command_placeholders_substitute_string_values() {
        let runtime = runtime_from_json(json!({
            "tools": {
                "pair": command_tool("printf", json!(["%s-%s", "{left}", "{right}"]))
            }
        }));
        let backend = runtime.backend(Arc::new(MarkerBackend));
        let output = backend
            .execute("pair", &json!({"left": "one", "right": "two"}))
            .await
            .expect("printf");
        assert_eq!(output["stdout"], "one-two");
    }

    #[tokio::test]
    async fn command_timeout_is_clear() {
        let runtime = runtime_from_json(json!({
            "tools": {
                "slow": {
                    "description": "test command",
                    "inputSchema": {"type": "object"},
                    "exec": {
                        "kind": "command",
                        "command": "sleep",
                        "args": ["5"],
                        "timeout_secs": 1
                    }
                }
            }
        }));
        let backend = runtime.backend(Arc::new(MarkerBackend));
        let error = backend
            .execute("slow", &json!({}))
            .await
            .expect_err("timeout");
        assert!(error.to_string().contains("timed out"), "{error}");
    }

    #[tokio::test]
    async fn command_output_is_capped() {
        let runtime = runtime_from_json(json!({
            "tools": {
                "big": {
                    "description": "test command",
                    "inputSchema": {"type": "object"},
                    "exec": {
                        "kind": "command",
                        "command": "printf",
                        "args": ["x".repeat(100)],
                        "max_output_bytes": 10
                    }
                }
            }
        }));
        let backend = runtime.backend(Arc::new(MarkerBackend));
        let output = backend.execute("big", &json!({})).await.expect("capped");
        assert_eq!(output["stdout"].as_str().expect("stdout").len(), 10);
        assert_eq!(output["truncated"], true);
    }

    #[tokio::test]
    async fn substituted_control_characters_are_rejected() {
        let runtime = runtime_from_json(json!({
            "tools": {"echo": command_tool("echo", json!(["{message}"]))}
        }));
        let backend = runtime.backend(Arc::new(MarkerBackend));
        let error = backend
            .execute("echo", &json!({"message": "bad\u{0001}value"}))
            .await
            .expect_err("control character");
        assert!(error.to_string().contains("control characters"), "{error}");
    }

    #[tokio::test]
    async fn command_environment_is_cleared_and_fixed() {
        let runtime = runtime_from_json(json!({
            "tools": {
                "env": {
                    "description": "test command",
                    "inputSchema": {"type": "object"},
                    "exec": {
                        "kind": "command",
                        "command": "env",
                        "args": [],
                        "env": {"DHI_TEST": "fixed"}
                    }
                }
            }
        }));
        let backend = runtime.backend(Arc::new(MarkerBackend));
        let output = backend.execute("env", &json!({})).await.expect("env");
        let stdout = output["stdout"].as_str().expect("stdout");
        assert_eq!(stdout.lines().count(), 2, "inherited env leaked: {stdout}");
        assert!(
            stdout
                .lines()
                .any(|line| line.starts_with("PATH=/usr/local/bin:")),
            "{stdout}"
        );
        assert!(
            stdout.lines().any(|line| line == "DHI_TEST=fixed"),
            "{stdout}"
        );
    }

    #[tokio::test]
    async fn input_larger_than_sixty_four_kib_fails_closed() {
        let runtime = runtime_from_json(json!({
            "tools": {"echo": command_tool("echo", json!(["{message}"]))}
        }));
        let backend = runtime.backend(Arc::new(MarkerBackend));
        let error = backend
            .execute("echo", &json!({"message": "x".repeat(70 * 1024)}))
            .await
            .expect_err("oversized input");
        assert!(error.to_string().contains("cap"), "{error}");
    }
}
