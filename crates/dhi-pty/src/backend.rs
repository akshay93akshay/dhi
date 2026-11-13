//! The [`dhi_guard::ToolBackend`] that runs the persistent terminal tools
//! for one task.
//!
//! A call must carry the per-task MAC stamped by
//! [`PtyTools`](crate::PtyTools) and the task id from its [`RunScope`].
//! The MAC is `HMAC-SHA256(secret, task_id)`; the backend recomputes it for
//! the task id in the request and compares before any session work, so a
//! MAC leaked for one task cannot authorize another, and a terminal tool
//! registered through the generic [`dhi_agent::guarded_dynamic_tool`] path
//! cannot forge a task.
//!
//! The session itself lives in the task's runtime: a `/bin/sh` under
//! `sandbox-exec` in production, or a local `/bin/sh` in test mode.

use std::sync::Arc;

use anyhow::anyhow;
use async_trait::async_trait;
use dhi_guard::ToolBackend;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::session::{DEFAULT_READ_TIMEOUT_MS, MAX_READ_TIMEOUT_MS, PtySessions};
use crate::tools::{SCOPE_MAC_FIELD, TASK_ID_FIELD, task_mac_matches};

/// Runs the terminal tools against one session manager.
#[derive(Clone)]
pub struct PtyBackend {
    sessions: Arc<PtySessions>,
    secret: [u8; 32],
}

impl PtyBackend {
    /// A backend over `sessions` that accepts only a MAC made with `secret`.
    /// Built by [`PtyTools::backend`](crate::PtyTools::backend).
    pub(crate) fn new(sessions: Arc<PtySessions>, secret: [u8; 32]) -> Self {
        Self { sessions, secret }
    }

    /// The tool names this backend serves.
    pub const TOOL_NAMES: &'static [&'static str] = &[
        "shell_open",
        "shell_send",
        "shell_read",
        "shell_close",
        "shell_list",
    ];

    /// Whether `name` is a terminal tool this backend serves.
    pub fn is_pty_tool(name: &str) -> bool {
        Self::TOOL_NAMES.contains(&name)
    }

    /// Runs `tool` for `task_id` after the caller checked the scope MAC.
    ///
    /// Each tool returns one JSON object. The public path is the
    /// [`ToolBackend`] implementation.
    pub(crate) async fn execute_for_task(
        &self,
        task_id: Uuid,
        tool: &str,
        input: Value,
    ) -> anyhow::Result<Value> {
        match tool {
            "shell_open" => {
                let args: OpenArgs = parse_args(tool, input)?;
                let reply = self.sessions.open(task_id, args.cwd).await?;
                Ok(json!({
                    "session_id": reply.session_id.to_string(),
                    "cwd": reply.cwd,
                }))
            }
            "shell_send" => {
                let args: SendArgs = parse_args(tool, input)?;
                let bytes = self
                    .sessions
                    .send(task_id, args.session_id, &args.text)
                    .await?;
                Ok(json!({
                    "session_id": args.session_id.to_string(),
                    "bytes_sent": bytes,
                }))
            }
            "shell_read" => {
                let args: ReadArgs = parse_args(tool, input)?;
                let timeout = args
                    .timeout_ms
                    .unwrap_or(DEFAULT_READ_TIMEOUT_MS)
                    .min(MAX_READ_TIMEOUT_MS);
                let reply = self
                    .sessions
                    .read(task_id, args.session_id, timeout)
                    .await?;
                Ok(json!({
                    "session_id": args.session_id.to_string(),
                    "output": reply.output,
                    "closed": reply.closed,
                    "dropped_bytes": reply.dropped_bytes,
                    "truncated": reply.truncated,
                }))
            }
            "shell_close" => {
                let args: CloseArgs = parse_args(tool, input)?;
                self.sessions.close(task_id, args.session_id).await?;
                Ok(json!({
                    "session_id": args.session_id.to_string(),
                    "closed": true,
                }))
            }
            "shell_list" => {
                let _: EmptyArgs = parse_args(tool, input)?;
                let sessions = self.sessions.list(task_id).await?;
                Ok(json!({ "sessions": sessions }))
            }
            _ => Err(anyhow!("no terminal tool named {tool}")),
        }
    }
}

#[async_trait]
impl ToolBackend for PtyBackend {
    async fn execute(&self, tool: &str, input: &Value) -> anyhow::Result<Value> {
        if !Self::is_pty_tool(tool) {
            return Err(anyhow!("no terminal tool named {tool}"));
        }
        let Value::Object(mut input) = input.clone() else {
            return Err(anyhow!(
                "terminal tool {tool} has no task scope: its arguments are not an object"
            ));
        };

        // The MAC is the capability that proves this request came from the
        // matching `PtyTools` wrapper for this task. It is checked before any
        // session work. A MAC issued for task A is not valid for task B
        // because the task id is part of the HMAC input.
        let mac = input
            .remove(SCOPE_MAC_FIELD)
            .and_then(|raw| raw.as_str().map(str::to_owned))
            .ok_or_else(|| {
                anyhow!("terminal tool {tool} has no trusted scope: {SCOPE_MAC_FIELD} is missing")
            })?;

        let raw_task_id = input.remove(TASK_ID_FIELD).ok_or_else(|| {
            anyhow!("terminal tool {tool} has no task scope: {TASK_ID_FIELD} is missing")
        })?;
        let task_id = raw_task_id
            .as_str()
            .and_then(|raw| Uuid::parse_str(raw).ok())
            .ok_or_else(|| {
                anyhow!("terminal tool {tool} has no task scope: {TASK_ID_FIELD} is not a task id")
            })?;

        if !task_mac_matches(&self.secret, task_id, &mac) {
            return Err(anyhow!(
                "terminal tool {tool} has no trusted scope: {SCOPE_MAC_FIELD} does not \
                 authorize task {task_id}"
            ));
        }

        self.execute_for_task(task_id, tool, Value::Object(input))
            .await
    }
}

#[derive(Debug, Deserialize)]
struct OpenArgs {
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SendArgs {
    session_id: Uuid,
    text: String,
}

#[derive(Debug, Deserialize)]
struct ReadArgs {
    session_id: Uuid,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct CloseArgs {
    session_id: Uuid,
}

#[derive(Debug, Deserialize)]
struct EmptyArgs {}

/// Parses a tool's arguments, naming the tool when the shape is wrong.
fn parse_args<T: for<'de> Deserialize<'de>>(tool: &str, input: Value) -> anyhow::Result<T> {
    serde_json::from_value(input).map_err(|err| anyhow!("invalid arguments for {tool}: {err}"))
}
