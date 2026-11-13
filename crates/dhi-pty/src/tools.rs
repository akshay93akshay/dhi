//! Model-facing terminal tool definitions, the [`PtyTools`] facade, and
//! guarded registration.

use std::sync::Arc;

use anyhow::anyhow;
use dhi_agent::{RunScope, ToolSink};
use dhi_core::{ToolCallOutcome, ToolCallRequest};
use dhi_exec::{TaskContainers, ToolDefinition};
use hmac::{Hmac, Mac};
use rig::completion::ToolAnnotations;
use rig::message::ToolName;
use rig::tool::server::ToolServerHandle;
use rig::tool::{DynamicTool, ToolContext, ToolExecutionError, ToolOutput};
use serde_json::json;
use sha2::Sha256;
use uuid::Uuid;

use crate::backend::PtyBackend;
use crate::session::PtySessions;

pub use dhi_exec::{SCOPE_MAC_FIELD, TASK_ID_FIELD};

/// HMAC-SHA256, the MAC the wrapper stamps and the backend checks.
type HmacSha256 = Hmac<Sha256>;

/// A fresh 32-byte secret. Two UUID v4 values give 32 random bytes without a
/// new random-number dependency.
fn random_secret() -> [u8; 32] {
    let mut secret = [0_u8; 32];
    secret[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    secret[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    secret
}

/// The MAC of `task_id` under `secret`.
pub(crate) fn task_mac(secret: &[u8; 32], task_id: Uuid) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts a key of any length");
    mac.update(task_id.as_bytes());
    hex(&mac.finalize().into_bytes())
}

/// Whether `presented` is the MAC of `task_id` under `secret`, compared in
/// constant time by the HMAC implementation.
pub(crate) fn task_mac_matches(secret: &[u8; 32], task_id: Uuid, presented: &str) -> bool {
    let Some(presented) = decode_hex(presented) else {
        return false;
    };
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts a key of any length");
    mac.update(task_id.as_bytes());
    mac.verify_slice(&presented).is_ok()
}

/// The lowercase hexadecimal form of `bytes`.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(text, "{byte:02x}").expect("writing to a String cannot fail");
    }
    text
}

/// Decodes lowercase or uppercase hexadecimal text, or `None` when it is not
/// an even-length run of hex digits.
fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.as_chunks::<2>().0 {
        let high = hex_digit(pair[0])?;
        let low = hex_digit(pair[1])?;
        decoded.push(high << 4 | low);
    }
    Some(decoded)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Owns the terminal sessions and the trusted scope secret for one runtime.
///
/// The facade registers the five guarded terminal tools with both reserved
/// scope fields stamped from the dispatch [`RunScope`] and its own secret,
/// and it hands out the only backend that accepts MACs made with that
/// secret. The secret has no public accessor.
pub struct PtyTools {
    sessions: Arc<PtySessions>,
    secret: [u8; 32],
}

impl PtyTools {
    /// A facade over `containers` with a fresh random scope secret.
    pub fn new(containers: Arc<TaskContainers>) -> Self {
        Self {
            sessions: Arc::new(PtySessions::new(containers)),
            secret: random_secret(),
        }
    }

    /// The MAC of `task_id`, for crate tests only. It is never sent to the
    /// model and is not part of the public API.
    #[cfg(test)]
    pub(crate) fn mac(&self, task_id: Uuid) -> String {
        task_mac(&self.secret, task_id)
    }

    /// A backend that accepts only calls carrying this facade's MAC.
    pub fn backend(&self) -> PtyBackend {
        PtyBackend::new(Arc::clone(&self.sessions), self.secret)
    }

    /// Closes every terminal session of `task_id`.
    ///
    /// This is the production way to stop a task's terminals before its
    /// runtime is removed. A session whose process cleanup fails stays
    /// registered so the call can be retried.
    pub async fn close_task(&self, task_id: Uuid) -> anyhow::Result<()> {
        self.sessions.close_task(task_id).await
    }

    /// Closes every terminal session and refuses any new one.
    ///
    /// Production calls this when the runtime stops, before
    /// [`TaskContainers::shutdown`]. It does not stop task runtimes.
    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.sessions.shutdown().await
    }

    /// Registers one guarded [`DynamicTool`] per terminal tool on `handle`.
    ///
    /// Registration fails if any of the five names already exists on
    /// `handle`; an existing tool is not overwritten.
    pub fn register_tools(
        &self,
        handle: &ToolServerHandle,
        sink: Arc<dyn ToolSink>,
    ) -> anyhow::Result<Vec<String>> {
        register_guarded_tools(handle, sink, self.secret)
    }
}

/// The five persistent terminal tools.
///
/// The schemas are the model-facing contract the backend parses. Their
/// descriptions state the persistence, the appended newline, the
/// new-output-only read, the task ownership, and the need to close.
pub fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "shell_open".to_owned(),
            description: "Open a persistent terminal (an interactive shell) inside this task's \
                          runtime. The terminal keeps running between calls, so its working \
                          directory, environment, and running programs survive. A task may have \
                          at most four terminals. The session belongs to one task only. Close it \
                          with shell_close when done."
                .to_owned(),
            schema: json!({
                "type": "object",
                "properties": {
                    "cwd": {
                        "type": "string",
                        "description": "Directory to start in, relative to the task root. \
                                        Absolute paths and any '..' component are rejected. \
                                        Defaults to the task root."
                    }
                },
                "additionalProperties": false
            }),
            annotations: ToolAnnotations::default()
                .with_title("Open Terminal")
                .with_read_only(false)
                .with_destructive(false)
                .with_idempotent(false)
                .with_open_world(true),
        },
        ToolDefinition {
            name: "shell_send".to_owned(),
            description: "Send text to a persistent terminal session opened with shell_open. A \
                          newline is appended when the text does not end with one, so the shell \
                          runs the line. The text is capped at 64 KiB. The terminal is \
                          persistent and the session belongs to one task only. Close it with \
                          shell_close when done."
                .to_owned(),
            schema: json!({
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "The session id returned by shell_open."
                    },
                    "text": {
                        "type": "string",
                        "description": "The text to send. A trailing newline is added when the \
                                        text does not already have one."
                    }
                },
                "required": ["session_id", "text"],
                "additionalProperties": false
            }),
            annotations: ToolAnnotations::default()
                .with_title("Send to Terminal")
                .with_read_only(false)
                .with_destructive(true)
                .with_idempotent(false)
                .with_open_world(true),
        },
        ToolDefinition {
            name: "shell_read".to_owned(),
            description: "Read new output from a persistent terminal session opened with \
                          shell_open. Only output not returned by an earlier shell_read is \
                          returned. Waits up to timeout_ms (default 1000, maximum 30000) for \
                          output, and reports whether the session has closed. When the bounded \
                          buffer had to discard unread output, dropped_bytes is non-zero and \
                          truncated is true. The session belongs to one task only. Close it \
                          with shell_close when done."
                .to_owned(),
            schema: json!({
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "The session id returned by shell_open."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 30000,
                        "description": "How long to wait for new output, in milliseconds. \
                                        Defaults to 1000 and is capped at 30000."
                    }
                },
                "required": ["session_id"],
                "additionalProperties": false
            }),
            annotations: ToolAnnotations::default()
                .with_title("Read Terminal")
                .with_read_only(true)
                .with_destructive(false)
                .with_open_world(false),
        },
        ToolDefinition {
            name: "shell_close".to_owned(),
            description: "Close a persistent terminal session opened with shell_open and stop \
                          its process. Closing a session that already closed succeeds; an \
                          unknown session is an error. The session belongs to one task only."
                .to_owned(),
            schema: json!({
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "The session id returned by shell_open."
                    }
                },
                "required": ["session_id"],
                "additionalProperties": false
            }),
            annotations: ToolAnnotations::default()
                .with_title("Close Terminal")
                .with_read_only(false)
                .with_destructive(true)
                .with_idempotent(true)
                .with_open_world(false),
        },
        ToolDefinition {
            name: "shell_list".to_owned(),
            description: "List the persistent terminal sessions belonging to this task, with \
                          their working directory, whether they are alive, and how many unread \
                          bytes are buffered. At most four sessions are shown. Other tasks' \
                          sessions are never shown. Close a session with shell_close when done."
                .to_owned(),
            schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            annotations: ToolAnnotations::default()
                .with_title("List Terminals")
                .with_read_only(true)
                .with_destructive(false)
                .with_open_world(false),
        },
    ]
}

/// Registers the five terminal tools, refusing to overwrite an existing name.
fn register_guarded_tools(
    handle: &ToolServerHandle,
    sink: Arc<dyn ToolSink>,
    secret: [u8; 32],
) -> anyhow::Result<Vec<String>> {
    let definitions = tool_definitions();
    let toolset = handle.toolset();
    for definition in &definitions {
        if toolset.contains(&definition.name) {
            return Err(anyhow!(
                "terminal tool {:?} is already registered on this tool server; refusing to \
                 overwrite it",
                definition.name
            ));
        }
    }

    let mut names = Vec::with_capacity(definitions.len());
    for ToolDefinition {
        name,
        description,
        schema,
        annotations,
    } in definitions
    {
        let tool = guarded_tool(
            &name,
            description,
            schema,
            annotations,
            Arc::clone(&sink),
            secret,
        )?;
        handle.add_dynamic_tool(tool);
        names.push(name);
    }
    Ok(names)
}

/// Builds the model-visible door to the guard.
///
/// The callback reads [`RunScope`] from the dispatch context, overwrites both
/// reserved scope fields, and maps the sink outcome exactly as
/// [`dhi_agent::guarded_dynamic_tool`] does.
fn guarded_tool(
    name: &str,
    description: String,
    schema: serde_json::Value,
    annotations: ToolAnnotations,
    sink: Arc<dyn ToolSink>,
    secret: [u8; 32],
) -> anyhow::Result<DynamicTool> {
    let tool_name =
        ToolName::new(name).map_err(|err| anyhow!("invalid tool name {name:?}: {err}"))?;
    let tool_label = name.to_string();
    Ok(DynamicTool::new_with_context(
        tool_name,
        description,
        schema,
        move |context: &mut ToolContext, args: serde_json::Value| {
            let sink = Arc::clone(&sink);
            let tool = tool_label.clone();
            Box::pin(async move {
                let scope: RunScope = context.require().map_err(|err| {
                    ToolExecutionError::other(format!(
                        "guarded tool dispatch is missing its run scope: {err}"
                    ))
                })?;
                let serde_json::Value::Object(mut input) = args else {
                    return Err(ToolExecutionError::invalid_args(format!(
                        "{tool} expects a JSON object of arguments"
                    )));
                };
                // The scope always wins: both reserved fields in the model's
                // arguments are replaced before the guard sees them. The MAC
                // is bound to the scope's task id.
                input.insert(
                    TASK_ID_FIELD.to_owned(),
                    serde_json::Value::String(scope.task_id.to_string()),
                );
                input.insert(
                    SCOPE_MAC_FIELD.to_owned(),
                    serde_json::Value::String(task_mac(&secret, scope.task_id)),
                );
                let request = ToolCallRequest {
                    tool,
                    input: serde_json::Value::Object(input),
                };
                match sink.execute(scope.task_id, scope.intent_id, request).await {
                    Ok(ToolCallOutcome::Allowed { output }) => Ok(ToolOutput::json(output)),
                    Ok(ToolCallOutcome::Denied { reason }) => {
                        Err(ToolExecutionError::permission_denied(reason))
                    }
                    Ok(ToolCallOutcome::Pending { approval_id }) => {
                        Err(ToolExecutionError::other(format!(
                            "tool call is parked behind approval {approval_id}; \
                             approvals must block inside the tool call"
                        )))
                    }
                    Err(err) => Err(ToolExecutionError::other(err.to_string())),
                }
            })
        },
    )
    .with_annotations(annotations))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use dhi_guard::ToolBackend;
    use rig::tool::ToolErrorKind;
    use rig::tool::server::ToolServer;
    use uuid::Uuid;

    use super::*;

    #[derive(Default)]
    struct FakeSink {
        calls: Mutex<Vec<(Uuid, Uuid, ToolCallRequest)>>,
    }

    #[async_trait]
    impl ToolSink for FakeSink {
        async fn execute(
            &self,
            task_id: Uuid,
            intent_id: Uuid,
            request: ToolCallRequest,
        ) -> anyhow::Result<ToolCallOutcome> {
            self.calls
                .lock()
                .unwrap()
                .push((task_id, intent_id, request));
            Ok(ToolCallOutcome::Allowed {
                output: json!({ "ok": true }),
            })
        }
    }

    fn fixture() -> (tempfile::TempDir, Arc<TaskContainers>, PtyTools) {
        let temp = tempfile::tempdir().unwrap();
        let containers = Arc::new(TaskContainers::local(temp.path()).unwrap());
        let tools = PtyTools::new(Arc::clone(&containers));
        (temp, containers, tools)
    }

    fn scope_context(task_id: Uuid) -> ToolContext {
        let mut context = ToolContext::new();
        context
            .insert(RunScope {
                task_id,
                intent_id: Uuid::new_v4(),
            })
            .unwrap();
        context
    }

    #[test]
    fn definitions_cover_the_five_terminal_tools() {
        let definitions = tool_definitions();
        let names: Vec<&str> = definitions
            .iter()
            .map(|definition| definition.name.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "shell_open",
                "shell_send",
                "shell_read",
                "shell_close",
                "shell_list"
            ]
        );
        for definition in &definitions {
            assert_eq!(
                definition.schema.get("type"),
                Some(&json!("object")),
                "{} has an object schema",
                definition.name
            );
        }
    }

    #[test]
    fn descriptions_state_the_required_contract() {
        let definitions = tool_definitions();
        let description = |name: &str| {
            definitions
                .iter()
                .find(|definition| definition.name == name)
                .unwrap_or_else(|| panic!("{name} is defined"))
                .description
                .to_ascii_lowercase()
        };
        for name in ["shell_open", "shell_send", "shell_read", "shell_close"] {
            assert!(
                description(name).contains("persistent") || description(name).contains("session"),
                "{name} describes the persistent session: {}",
                description(name)
            );
        }
        assert!(description("shell_send").contains("newline"));
        assert!(description("shell_read").contains("only output not returned"));
        assert!(description("shell_open").contains("one task only"));
        assert!(description("shell_close").contains("one task only"));
    }

    #[test]
    fn annotations_match_what_each_tool_does() {
        let definitions = tool_definitions();
        let by_name = |name: &str| {
            definitions
                .iter()
                .find(|definition| definition.name == name)
                .unwrap_or_else(|| panic!("{name} is defined"))
        };
        assert!(by_name("shell_read").annotations.read_only());
        assert!(by_name("shell_list").annotations.read_only());
        assert!(!by_name("shell_send").annotations.read_only());
        assert!(by_name("shell_send").annotations.destructive());
        assert!(by_name("shell_close").annotations.idempotent());
    }

    #[tokio::test]
    async fn register_tools_adds_five_guarded_tools() {
        let (_temp, _containers, tools) = fixture();
        let handle = ToolServer::new().run();
        let registered = tools
            .register_tools(&handle, Arc::new(FakeSink::default()))
            .unwrap();
        assert_eq!(
            registered,
            [
                "shell_open",
                "shell_send",
                "shell_read",
                "shell_close",
                "shell_list"
            ]
        );

        let definitions = handle.tool_defs(None).await.unwrap();
        let names: Vec<&str> = definitions
            .iter()
            .map(|definition| definition.name.as_str())
            .collect();
        assert_eq!(names, registered);
    }

    #[tokio::test]
    async fn a_pre_registered_name_is_a_collision() {
        let (_temp, _containers, tools) = fixture();
        let handle = ToolServer::new().run();
        let existing = DynamicTool::new(
            ToolName::new("shell_open").unwrap(),
            "an existing shell_open",
            json!({ "type": "object" }),
            |_args| Box::pin(async { Ok(ToolOutput::text("existing")) }),
        );
        handle.add_dynamic_tool(existing);

        let err = tools
            .register_tools(&handle, Arc::new(FakeSink::default()))
            .expect_err("an existing name is a collision");
        let text = err.to_string();
        assert!(text.contains("shell_open"), "{text}");
        assert!(text.contains("already registered"), "{text}");
        let toolset = handle.toolset();
        assert_eq!(
            toolset.names().collect::<Vec<_>>(),
            ["shell_open"],
            "no terminal tool was registered after the collision"
        );
    }

    #[tokio::test]
    async fn the_scope_overwrites_the_model_task_id_and_mac() {
        let (_temp, _containers, tools) = fixture();
        let handle = ToolServer::new().run();
        let sink = Arc::new(FakeSink::default());
        tools.register_tools(&handle, sink.clone()).unwrap();

        let task_id = Uuid::new_v4();
        let forged_task = Uuid::new_v4();
        let forged_mac = "00".repeat(32);
        let mut arguments = json!({});
        arguments[TASK_ID_FIELD] = json!(forged_task.to_string());
        arguments[SCOPE_MAC_FIELD] = json!(forged_mac.clone());
        let mut context = scope_context(task_id);

        let result = handle
            .toolset()
            .execute("shell_list", arguments.to_string(), &mut context)
            .await;
        assert!(result.is_success(), "{result:?}");

        let calls = sink.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let request = &calls[0].2;
        assert_eq!(
            request.input.get(TASK_ID_FIELD),
            Some(&json!(task_id.to_string()))
        );
        assert_ne!(
            request.input.get(TASK_ID_FIELD),
            Some(&json!(forged_task.to_string()))
        );
        assert_eq!(
            request.input.get(SCOPE_MAC_FIELD),
            Some(&json!(tools.mac(task_id)))
        );
        assert_ne!(request.input.get(SCOPE_MAC_FIELD), Some(&json!(forged_mac)));
    }

    #[tokio::test]
    async fn a_missing_mac_fails_before_any_session_work() {
        let (temp, containers, tools) = fixture();
        let backend = tools.backend();
        let task = Uuid::new_v4();
        let input = json!({
            TASK_ID_FIELD: task.to_string(),
            "cwd": null,
            "session_id": Uuid::new_v4().to_string(),
            "text": "echo leaked"
        });
        let err = backend
            .execute("shell_open", &input)
            .await
            .expect_err("a call without the MAC cannot run");
        assert!(err.to_string().contains(SCOPE_MAC_FIELD), "{err}");
        assert!(
            !containers.root().join(task.to_string()).exists(),
            "no task directory was created"
        );
        drop(temp);
    }

    #[tokio::test]
    async fn a_wrong_mac_fails_before_any_session_work() {
        let (temp, containers, tools) = fixture();
        let backend = tools.backend();
        let task = Uuid::new_v4();
        let mut input = json!({
            TASK_ID_FIELD: task.to_string(),
            "cwd": null
        });
        input[SCOPE_MAC_FIELD] = json!("ff".repeat(32));
        let err = backend
            .execute("shell_open", &input)
            .await
            .expect_err("a call with a foreign MAC cannot run");
        assert!(err.to_string().contains(SCOPE_MAC_FIELD), "{err}");
        assert!(
            !containers.root().join(task.to_string()).exists(),
            "no task directory was created"
        );
        drop(temp);
    }

    #[tokio::test]
    async fn a_mac_for_task_a_does_not_authorize_task_b() {
        let (_temp, _containers, tools) = fixture();
        let backend = tools.backend();
        let task_a = Uuid::new_v4();
        let task_b = Uuid::new_v4();
        let mut input = json!({
            TASK_ID_FIELD: task_b.to_string(),
            "cwd": null
        });
        input[SCOPE_MAC_FIELD] = json!(tools.mac(task_a));

        let err = backend
            .execute("shell_open", &input)
            .await
            .expect_err("task A's MAC is not task B's");
        assert!(err.to_string().contains(SCOPE_MAC_FIELD), "{err}");
        assert!(err.to_string().contains(&task_b.to_string()), "{err}");
    }

    #[tokio::test]
    async fn a_denied_call_is_a_permission_error_with_the_reason() {
        let (_temp, _containers, tools) = fixture();
        let handle = ToolServer::new().run();
        struct DenyingSink;
        #[async_trait]
        impl ToolSink for DenyingSink {
            async fn execute(
                &self,
                _task_id: Uuid,
                _intent_id: Uuid,
                _request: ToolCallRequest,
            ) -> anyhow::Result<ToolCallOutcome> {
                Ok(ToolCallOutcome::Denied {
                    reason: "rule deny-terminal: terminals are not allowed".to_owned(),
                })
            }
        }
        tools
            .register_tools(&handle, Arc::new(DenyingSink))
            .unwrap();
        let mut context = scope_context(Uuid::new_v4());
        let result = handle
            .toolset()
            .execute("shell_list", json!({}).to_string(), &mut context)
            .await;
        assert!(result.is_error(), "{result:?}");
        let error = result.error().expect("a denial is an error");
        assert_eq!(error.kind(), ToolErrorKind::PermissionDenied);
        assert!(error.message().contains("deny-terminal"), "{error}");
    }

    #[tokio::test]
    async fn a_pending_outcome_is_an_internal_error() {
        let (_temp, _containers, tools) = fixture();
        let handle = ToolServer::new().run();
        struct PendingSink;
        #[async_trait]
        impl ToolSink for PendingSink {
            async fn execute(
                &self,
                _task_id: Uuid,
                _intent_id: Uuid,
                _request: ToolCallRequest,
            ) -> anyhow::Result<ToolCallOutcome> {
                Ok(ToolCallOutcome::Pending {
                    approval_id: Uuid::new_v4(),
                })
            }
        }
        tools
            .register_tools(&handle, Arc::new(PendingSink))
            .unwrap();
        let mut context = scope_context(Uuid::new_v4());
        let result = handle
            .toolset()
            .execute("shell_list", json!({}).to_string(), &mut context)
            .await;
        assert!(result.is_error(), "{result:?}");
        assert!(
            result
                .error()
                .is_some_and(|error| error.message().contains("approvals must block")),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn non_object_arguments_are_invalid() {
        let (_temp, _containers, tools) = fixture();
        let handle = ToolServer::new().run();
        tools
            .register_tools(&handle, Arc::new(FakeSink::default()))
            .unwrap();
        let mut context = scope_context(Uuid::new_v4());
        let result = handle
            .toolset()
            .execute(
                "shell_list",
                json!("not an object").to_string(),
                &mut context,
            )
            .await;
        assert!(result.is_error(), "{result:?}");
        assert_eq!(
            result.error().map(ToolExecutionError::kind),
            Some(ToolErrorKind::InvalidArgs)
        );
    }

    #[test]
    fn decode_hex_rejects_bad_macs() {
        assert_eq!(decode_hex("00ff"), Some(vec![0, 255]));
        assert_eq!(decode_hex("0"), None);
        assert_eq!(decode_hex("zz"), None);
        assert_eq!(decode_hex(""), Some(Vec::new()));
    }

    #[test]
    fn the_mac_is_bound_to_the_task() {
        let secret = [7_u8; 32];
        let task_a = Uuid::new_v4();
        let task_b = Uuid::new_v4();
        let mac = task_mac(&secret, task_a);
        assert!(task_mac_matches(&secret, task_a, &mac));
        assert!(!task_mac_matches(&secret, task_b, &mac));
        assert!(!task_mac_matches(&[8_u8; 32], task_a, &mac));
        assert!(!task_mac_matches(&secret, task_a, "not-hex"));
    }
}
