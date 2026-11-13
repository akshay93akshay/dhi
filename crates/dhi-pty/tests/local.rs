//! Local test mode: the public terminal tools over a plain `/bin/sh`, with
//! no sandbox and no wrapper.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dhi_agent::{RunScope, ToolSink};
use dhi_core::{ToolCallOutcome, ToolCallRequest};
use dhi_exec::TaskContainers;
use dhi_guard::ToolBackend;
use dhi_pty::{PtyBackend, PtyTools, SCOPE_MAC_FIELD, TASK_ID_FIELD, tool_definitions};
use rig::tool::ToolContext;
use rig::tool::server::{ToolServer, ToolServerHandle};
use serde_json::{Value, json};
use uuid::Uuid;

struct Fixture {
    _temp: tempfile::TempDir,
    containers: Arc<TaskContainers>,
    tools: PtyTools,
    handle: ToolServerHandle,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let containers = Arc::new(TaskContainers::local(temp.path()).unwrap());
        let tools = PtyTools::new(Arc::clone(&containers));
        let handle = ToolServer::new().run();
        tools
            .register_tools(
                &handle,
                Arc::new(BackendSink {
                    backend: tools.backend(),
                }),
            )
            .unwrap();
        Self {
            _temp: temp,
            containers,
            tools,
            handle,
        }
    }

    async fn call(&self, task_id: Uuid, tool: &str, args: Value) -> anyhow::Result<Value> {
        call(&self.handle, task_id, tool, args).await
    }

    async fn open(&self, task: Uuid, cwd: Option<&str>) -> anyhow::Result<Value> {
        let args = match cwd {
            Some(cwd) => json!({ "cwd": cwd }),
            None => json!({}),
        };
        self.call(task, "shell_open", args).await
    }

    async fn send(&self, task: Uuid, session: Uuid, text: &str) -> anyhow::Result<Value> {
        self.call(
            task,
            "shell_send",
            json!({ "session_id": session, "text": text }),
        )
        .await
    }

    async fn read(&self, task: Uuid, session: Uuid, timeout_ms: u64) -> anyhow::Result<Value> {
        self.call(
            task,
            "shell_read",
            json!({ "session_id": session, "timeout_ms": timeout_ms }),
        )
        .await
    }

    async fn close(&self, task: Uuid, session: Uuid) -> anyhow::Result<Value> {
        self.call(task, "shell_close", json!({ "session_id": session }))
            .await
    }

    async fn list(&self, task: Uuid) -> anyhow::Result<Vec<Value>> {
        let listed = self.call(task, "shell_list", json!({})).await?;
        Ok(listed
            .get("sessions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    async fn read_until(
        &self,
        task: Uuid,
        session: Uuid,
        needle: &str,
        timeout: Duration,
    ) -> String {
        let deadline = Instant::now() + timeout;
        let mut seen = String::new();
        while Instant::now() < deadline {
            let reply = self
                .read(task, session, 1_000)
                .await
                .unwrap_or_else(|err| panic!("read failed: {err}"));
            seen.push_str(output(&reply));
            if seen.contains(needle) {
                return seen;
            }
            if is_closed(&reply) {
                break;
            }
        }
        panic!("no {needle:?} in terminal output: {seen:?}");
    }

    async fn shutdown(&self) {
        self.tools.shutdown().await.unwrap();
    }
}

struct BackendSink {
    backend: PtyBackend,
}

#[async_trait]
impl ToolSink for BackendSink {
    async fn execute(
        &self,
        _task_id: Uuid,
        _intent_id: Uuid,
        request: ToolCallRequest,
    ) -> anyhow::Result<ToolCallOutcome> {
        let output = self.backend.execute(&request.tool, &request.input).await?;
        Ok(ToolCallOutcome::Allowed { output })
    }
}

#[derive(Default)]
struct RecordingSink {
    calls: Mutex<Vec<ToolCallRequest>>,
}

impl RecordingSink {
    fn calls(&self) -> Vec<ToolCallRequest> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl ToolSink for RecordingSink {
    async fn execute(
        &self,
        _task_id: Uuid,
        _intent_id: Uuid,
        request: ToolCallRequest,
    ) -> anyhow::Result<ToolCallOutcome> {
        self.calls.lock().unwrap().push(request);
        Ok(ToolCallOutcome::Allowed {
            output: json!({ "ok": true }),
        })
    }
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

async fn call(
    handle: &ToolServerHandle,
    task_id: Uuid,
    tool: &str,
    args: Value,
) -> anyhow::Result<Value> {
    let mut context = scope_context(task_id);
    let result = handle
        .toolset()
        .execute(tool, args.to_string(), &mut context)
        .await;
    if result.is_error() {
        let message = result
            .error()
            .map(|error| error.message().to_owned())
            .unwrap_or_else(|| format!("{result:?}"));
        return Err(anyhow::anyhow!("{tool} failed: {message}"));
    }
    result
        .output()
        .as_json()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("{tool} returned no JSON output"))
}

fn session_id(value: &Value) -> Uuid {
    value
        .get("session_id")
        .and_then(Value::as_str)
        .expect("the tool returns a session id")
        .parse()
        .expect("the session id is a UUID")
}

fn output(value: &Value) -> &str {
    value
        .get("output")
        .and_then(Value::as_str)
        .expect("read returns output")
}

fn is_closed(value: &Value) -> bool {
    value.get("closed").and_then(Value::as_bool) == Some(true)
}

fn dropped_bytes(value: &Value) -> u64 {
    value
        .get("dropped_bytes")
        .and_then(Value::as_u64)
        .expect("read returns dropped_bytes")
}

fn truncated(value: &Value) -> bool {
    value
        .get("truncated")
        .and_then(Value::as_bool)
        .expect("read returns truncated")
}

#[tokio::test]
async fn a_session_runs_a_command_and_closes() {
    let fixture = Fixture::new();
    let task = Uuid::new_v4();

    let opened = fixture.open(task, None).await.unwrap();
    let session = session_id(&opened);
    let runtime = fixture.containers.ensure(task).await.unwrap();
    assert_eq!(
        opened.get("cwd").and_then(Value::as_str),
        Some(runtime.base.to_string_lossy().as_ref())
    );

    let sent = fixture.send(task, session, "echo hello").await.unwrap();
    assert_eq!(
        sent.get("bytes_sent").and_then(Value::as_u64),
        Some("echo hello\n".len() as u64)
    );

    let seen = fixture
        .read_until(task, session, "hello", Duration::from_secs(10))
        .await;
    assert!(seen.contains("hello"), "{seen}");

    fixture.close(task, session).await.unwrap();
    assert!(fixture.list(task).await.unwrap().is_empty());
    fixture.shutdown().await;
}

#[tokio::test]
async fn send_adds_a_newline_when_the_text_has_none() {
    let fixture = Fixture::new();
    let task = Uuid::new_v4();
    let opened = fixture.open(task, None).await.unwrap();
    let session = session_id(&opened);

    let sent = fixture
        .send(task, session, "echo runs-without-newline")
        .await
        .unwrap();
    assert_eq!(
        sent.get("bytes_sent").and_then(Value::as_u64),
        Some("echo runs-without-newline\n".len() as u64)
    );

    // Local mode does not echo input, so seeing the text proves the shell
    // executed the line after the wrapper added its newline.
    let seen = fixture
        .read_until(
            task,
            session,
            "runs-without-newline",
            Duration::from_secs(10),
        )
        .await;
    assert!(seen.contains("runs-without-newline"), "{seen}");

    fixture.shutdown().await;
}

#[tokio::test]
async fn list_shows_only_the_sessions_of_that_task() {
    let fixture = Fixture::new();
    let task_a = Uuid::new_v4();
    let task_b = Uuid::new_v4();

    let first = fixture.open(task_a, None).await.unwrap();
    let second = fixture.open(task_a, None).await.unwrap();
    let foreign = fixture.open(task_b, None).await.unwrap();

    let listed = fixture.list(task_a).await.unwrap();
    assert_eq!(listed.len(), 2);
    let ids: Vec<&str> = listed
        .iter()
        .filter_map(|info| info.get("session_id").and_then(Value::as_str))
        .collect();
    let first_id = session_id(&first).to_string();
    let second_id = session_id(&second).to_string();
    let foreign_id = session_id(&foreign).to_string();
    assert!(ids.contains(&first_id.as_str()));
    assert!(ids.contains(&second_id.as_str()));
    assert!(!ids.contains(&foreign_id.as_str()));
    assert!(
        listed
            .iter()
            .all(|info| info.get("alive") == Some(&json!(true)))
    );

    let other = fixture.list(task_b).await.unwrap();
    assert_eq!(other.len(), 1);
    assert_eq!(
        other[0].get("session_id"),
        Some(&json!(session_id(&foreign).to_string()))
    );

    fixture.shutdown().await;
}

#[tokio::test]
async fn task_b_cannot_send_read_or_close_task_as_session() {
    let fixture = Fixture::new();
    let task_a = Uuid::new_v4();
    let task_b = Uuid::new_v4();
    let opened = fixture.open(task_a, None).await.unwrap();
    let session = session_id(&opened);

    let send = fixture
        .send(task_b, session, "echo stolen")
        .await
        .expect_err("task B cannot send to task A's session");
    assert!(send.to_string().contains("another task"), "{send}");

    let read = fixture
        .read(task_b, session, 100)
        .await
        .expect_err("task B cannot read task A's session");
    assert!(read.to_string().contains("another task"), "{read}");

    let close = fixture
        .close(task_b, session)
        .await
        .expect_err("task B cannot close task A's session");
    assert!(close.to_string().contains("another task"), "{close}");

    // The refused calls did not disturb the owner.
    fixture
        .send(task_a, session, "echo still-owned")
        .await
        .unwrap();
    let seen = fixture
        .read_until(task_a, session, "still-owned", Duration::from_secs(10))
        .await;
    assert!(seen.contains("still-owned"), "{seen}");

    fixture.shutdown().await;
}

#[tokio::test]
async fn an_unknown_session_fails_for_every_operation() {
    let fixture = Fixture::new();
    let task = Uuid::new_v4();
    let unknown = Uuid::new_v4();

    for err in [
        fixture
            .send(task, unknown, "echo x")
            .await
            .expect_err("unknown send"),
        fixture
            .read(task, unknown, 10)
            .await
            .expect_err("unknown read"),
        fixture
            .close(task, unknown)
            .await
            .expect_err("unknown close"),
    ] {
        assert!(err.to_string().contains("no terminal session"), "{err}");
    }

    assert!(fixture.list(task).await.unwrap().is_empty());
    fixture.shutdown().await;
}

#[tokio::test]
async fn the_backend_refuses_a_missing_or_wrong_mac() {
    let fixture = Fixture::new();
    let backend = fixture.tools.backend();
    let task = Uuid::new_v4();

    let missing = backend
        .execute(
            "shell_open",
            &json!({ TASK_ID_FIELD: task.to_string(), "cwd": null }),
        )
        .await
        .expect_err("a call without a MAC cannot run");
    assert!(missing.to_string().contains(SCOPE_MAC_FIELD), "{missing}");
    assert!(
        !fixture.containers.root().join(task.to_string()).exists(),
        "the refused open did no session work"
    );

    let wrong = backend
        .execute(
            "shell_open",
            &json!({
                TASK_ID_FIELD: task.to_string(),
                SCOPE_MAC_FIELD: "00".repeat(32),
                "cwd": null
            }),
        )
        .await
        .expect_err("a call with a foreign MAC cannot run");
    assert!(wrong.to_string().contains(SCOPE_MAC_FIELD), "{wrong}");
    assert!(
        !fixture.containers.root().join(task.to_string()).exists(),
        "the refused open did no session work"
    );
}

#[tokio::test]
async fn a_mac_for_task_a_does_not_authorize_task_b() {
    let fixture = Fixture::new();
    let task_a = Uuid::new_v4();
    let task_b = Uuid::new_v4();
    let handle = ToolServer::new().run();
    let sink = Arc::new(RecordingSink::default());
    fixture.tools.register_tools(&handle, sink.clone()).unwrap();

    call(&handle, task_a, "shell_list", json!({}))
        .await
        .unwrap();
    let captured = sink.calls().remove(0);
    let mac_a = captured
        .input
        .get(SCOPE_MAC_FIELD)
        .and_then(Value::as_str)
        .expect("the wrapper stamped a MAC")
        .to_owned();

    let backend = fixture.tools.backend();
    let err = backend
        .execute(
            "shell_open",
            &json!({
                TASK_ID_FIELD: task_b.to_string(),
                SCOPE_MAC_FIELD: mac_a,
                "cwd": null
            }),
        )
        .await
        .expect_err("task A's MAC is not task B's");
    assert!(err.to_string().contains("does not authorize"), "{err}");
    assert!(err.to_string().contains(&task_b.to_string()), "{err}");
    assert!(
        !fixture.containers.root().join(task_b.to_string()).exists(),
        "the refused open did no session work"
    );
}

#[tokio::test]
async fn the_wrapper_overwrites_a_model_supplied_task_id_and_mac() {
    let fixture = Fixture::new();
    let task = Uuid::new_v4();
    let forged_task = Uuid::new_v4();
    let forged_mac = "00".repeat(32);
    let handle = ToolServer::new().run();
    let sink = Arc::new(RecordingSink::default());
    fixture.tools.register_tools(&handle, sink.clone()).unwrap();

    let mut arguments = json!({});
    arguments[TASK_ID_FIELD] = json!(forged_task.to_string());
    arguments[SCOPE_MAC_FIELD] = json!(forged_mac.clone());
    call(&handle, task, "shell_list", arguments).await.unwrap();
    call(&handle, task, "shell_list", json!({})).await.unwrap();

    let calls = sink.calls();
    assert_eq!(calls.len(), 2);
    for request in &calls {
        assert_eq!(
            request.input.get(TASK_ID_FIELD),
            Some(&json!(task.to_string())),
            "the scope task id wins"
        );
        assert_ne!(
            request.input.get(SCOPE_MAC_FIELD),
            Some(&json!(forged_mac)),
            "the wrapper MAC wins"
        );
    }
    assert_eq!(
        calls[0].input.get(SCOPE_MAC_FIELD),
        calls[1].input.get(SCOPE_MAC_FIELD),
        "the MAC is stable for one task under one facade"
    );
}

#[tokio::test]
async fn registration_adds_five_tools_and_rejects_a_collision() {
    let fixture = Fixture::new();
    let handle = ToolServer::new().run();
    let registered = fixture
        .tools
        .register_tools(&handle, Arc::new(RecordingSink::default()))
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
    assert_eq!(tool_definitions().len(), 5);

    let colliding = ToolServer::new().run();
    let existing = rig::tool::DynamicTool::new(
        rig::message::ToolName::new("shell_open").unwrap(),
        "an existing shell_open",
        json!({ "type": "object" }),
        |_args| Box::pin(async { Ok(rig::tool::ToolOutput::text("existing")) }),
    );
    colliding.add_dynamic_tool(existing);
    let err = fixture
        .tools
        .register_tools(&colliding, Arc::new(RecordingSink::default()))
        .expect_err("an existing name is a collision");
    assert!(err.to_string().contains("already registered"), "{err}");
    assert_eq!(
        colliding.toolset().names().collect::<Vec<_>>(),
        ["shell_open"],
        "nothing was registered after the collision"
    );
}

#[tokio::test]
async fn the_guarded_tools_drive_a_session_end_to_end() {
    let fixture = Fixture::new();
    let task = Uuid::new_v4();

    let opened = fixture.open(task, None).await.unwrap();
    let session = session_id(&opened);
    assert!(opened.get("cwd").and_then(Value::as_str).is_some());

    let sent = fixture
        .send(task, session, "echo tool-hello")
        .await
        .unwrap();
    assert_eq!(
        sent.get("bytes_sent").and_then(Value::as_u64),
        Some("echo tool-hello\n".len() as u64)
    );

    let seen = fixture
        .read_until(task, session, "tool-hello", Duration::from_secs(10))
        .await;
    assert!(seen.contains("tool-hello"), "{seen}");

    let listed = fixture.list(task).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].get("session_id"),
        Some(&json!(session.to_string()))
    );
    assert_eq!(listed[0].get("alive"), Some(&json!(true)));

    let closed = fixture.close(task, session).await.unwrap();
    assert_eq!(closed.get("closed").and_then(Value::as_bool), Some(true));

    assert!(fixture.list(task).await.unwrap().is_empty());

    let err = fixture
        .close(task, session)
        .await
        .expect_err("closing an already-closed session is an unknown session");
    assert!(err.to_string().contains("no terminal session"), "{err}");
}

#[tokio::test]
async fn shutdown_closes_every_session() {
    let fixture = Fixture::new();
    let task = Uuid::new_v4();
    let other = Uuid::new_v4();
    let first = fixture.open(task, None).await.unwrap();
    let second = fixture.open(task, None).await.unwrap();
    let third = fixture.open(other, None).await.unwrap();

    fixture.shutdown().await;

    assert!(fixture.list(task).await.unwrap().is_empty());
    assert!(fixture.list(other).await.unwrap().is_empty());
    for (owner, session) in [
        (task, session_id(&first)),
        (task, session_id(&second)),
        (other, session_id(&third)),
    ] {
        let err = fixture
            .send(owner, session, "echo after-shutdown")
            .await
            .expect_err("shutdown removed the session");
        assert!(err.to_string().contains("no terminal session"), "{err}");
    }

    // Shutdown is safe to call again, and no later session starts.
    fixture.shutdown().await;
    let err = fixture
        .open(Uuid::new_v4(), None)
        .await
        .expect_err("a shut-down manager starts nothing");
    assert!(err.to_string().contains("shut down"), "{err}");
}

#[tokio::test]
async fn close_task_closes_only_that_tasks_sessions() {
    let fixture = Fixture::new();
    let task_a = Uuid::new_v4();
    let task_b = Uuid::new_v4();
    let first = fixture.open(task_a, None).await.unwrap();
    let second = fixture.open(task_a, None).await.unwrap();
    let other = fixture.open(task_b, None).await.unwrap();

    fixture.tools.close_task(task_a).await.unwrap();
    assert!(fixture.list(task_a).await.unwrap().is_empty());
    let remaining = fixture.list(task_b).await.unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(
        remaining[0].get("session_id"),
        Some(&json!(session_id(&other).to_string()))
    );

    // Closing a task with no sessions succeeds.
    fixture.tools.close_task(task_a).await.unwrap();

    for session in [session_id(&first), session_id(&second)] {
        let err = fixture
            .send(task_a, session, "echo gone")
            .await
            .expect_err("close_task removed the session");
        assert!(err.to_string().contains("no terminal session"), "{err}");
    }

    fixture.shutdown().await;
}

#[tokio::test]
async fn cwd_is_confined_to_the_task_directory() {
    let fixture = Fixture::new();
    let task = Uuid::new_v4();
    let runtime = fixture.containers.ensure(task).await.unwrap();
    std::fs::create_dir(runtime.host_dir.join("sub")).unwrap();

    let opened = fixture.open(task, Some("sub")).await.unwrap();
    let session = session_id(&opened);
    assert_eq!(
        opened.get("cwd").and_then(Value::as_str),
        Some(runtime.host_dir.join("sub").to_string_lossy().as_ref())
    );
    fixture.send(task, session, "pwd").await.unwrap();
    let seen = fixture
        .read_until(task, session, "sub", Duration::from_secs(10))
        .await;
    assert!(seen.contains("sub"), "{seen}");

    for refused in ["/etc", "..", "../outside", "a/../b"] {
        let err = fixture
            .open(task, Some(refused))
            .await
            .expect_err("an absolute or parent cwd must be refused");
        assert!(
            err.to_string().contains("relative") || err.to_string().contains(".."),
            "{refused}: {err}"
        );
    }

    let err = fixture
        .open(task, Some("missing"))
        .await
        .expect_err("a missing local cwd must be refused");
    assert!(err.to_string().contains("cannot be used"), "{err}");

    fixture.close(task, session).await.unwrap();
    assert!(fixture.list(task).await.unwrap().is_empty());
    fixture.shutdown().await;
}

#[tokio::test]
async fn no_host_pty_directory_is_created() {
    let fixture = Fixture::new();
    let task = Uuid::new_v4();
    let runtime = fixture.containers.ensure(task).await.unwrap();
    let opened = fixture.open(task, None).await.unwrap();

    assert!(
        !runtime.host_dir.join(".pty").exists(),
        "the terminal keeps no pidfile below the model-writable task directory"
    );

    fixture.close(task, session_id(&opened)).await.unwrap();
    fixture.shutdown().await;
}

#[tokio::test]
async fn exit_is_reported_as_closed() {
    let fixture = Fixture::new();
    let task = Uuid::new_v4();
    let opened = fixture.open(task, None).await.unwrap();
    let session = session_id(&opened);
    fixture.send(task, session, "exit").await.unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut closed = false;
    while Instant::now() < deadline {
        let reply = fixture.read(task, session, 1_000).await.unwrap();
        if is_closed(&reply) {
            closed = true;
            break;
        }
    }
    assert!(closed, "the session reports that the shell exited");

    let listed = fixture.list(task).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].get("alive"), Some(&json!(false)));

    // Close still succeeds for a session that already closed.
    fixture.close(task, session).await.unwrap();
    assert!(fixture.list(task).await.unwrap().is_empty());
    fixture.shutdown().await;
}

#[tokio::test]
async fn a_shell_that_exits_with_a_background_child_still_reports_closed() {
    let fixture = Fixture::new();
    let task = Uuid::new_v4();
    let opened = fixture.open(task, None).await.unwrap();
    let session = session_id(&opened);

    // The background child inherits stdout and stderr, so the reader tasks
    // stay open after /bin/sh exits. `closed` is based on the child waiter,
    // not on reader EOF, and must become true anyway.
    fixture
        .send(task, session, "(trap '' HUP; sleep 5) &")
        .await
        .unwrap();
    fixture.send(task, session, "exit").await.unwrap();

    let deadline = Instant::now() + Duration::from_secs(4);
    let mut closed = false;
    while Instant::now() < deadline {
        let reply = fixture.read(task, session, 500).await.unwrap();
        if is_closed(&reply) {
            closed = true;
            break;
        }
    }
    assert!(
        closed,
        "the shell exit is reported even while the background child holds its pipes"
    );

    // Close is bounded even though the reader cannot reach EOF yet.
    fixture.close(task, session).await.unwrap();
    assert!(fixture.list(task).await.unwrap().is_empty());
    fixture.shutdown().await;
}

#[tokio::test]
async fn output_larger_than_the_buffer_reports_truncation() {
    let fixture = Fixture::new();
    let task = Uuid::new_v4();
    let opened = fixture.open(task, None).await.unwrap();
    let session = session_id(&opened);

    fixture
        .send(task, session, "yes x | head -c 307200")
        .await
        .unwrap();
    // Do not read while the command runs: the buffer must overflow on its
    // own so the read reports the loss.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let reply = fixture.read(task, session, 100).await.unwrap();
    assert!(truncated(&reply), "the read reports the truncation");
    assert!(
        dropped_bytes(&reply) > 0,
        "the evicted unread bytes are counted: {}",
        dropped_bytes(&reply)
    );
    assert!(
        output(&reply).len() <= 256 * 1024,
        "the buffer never returns more than its cap"
    );

    fixture.close(task, session).await.unwrap();
    fixture.shutdown().await;
}

#[tokio::test]
async fn shell_send_rejects_more_than_64_kib() {
    let fixture = Fixture::new();
    let task = Uuid::new_v4();
    let opened = fixture.open(task, None).await.unwrap();
    let session = session_id(&opened);

    let too_large = "x".repeat(64 * 1024 + 1);
    let err = fixture
        .send(task, session, &too_large)
        .await
        .expect_err("an oversized send is refused");
    assert!(err.to_string().contains("maximum is"), "{err}");

    // The refused send did not damage the session.
    fixture
        .send(task, session, "echo still-alive")
        .await
        .unwrap();
    let seen = fixture
        .read_until(task, session, "still-alive", Duration::from_secs(10))
        .await;
    assert!(seen.contains("still-alive"), "{seen}");

    fixture.shutdown().await;
}

#[tokio::test]
async fn a_task_is_limited_to_four_sessions() {
    let fixture = Fixture::new();
    let task = Uuid::new_v4();
    let other = Uuid::new_v4();
    let mut sessions = Vec::new();
    for _ in 0..4 {
        sessions.push(session_id(&fixture.open(task, None).await.unwrap()));
    }

    let err = fixture
        .open(task, None)
        .await
        .expect_err("a fifth session is refused");
    assert!(err.to_string().contains("maximum is 4"), "{err}");
    assert_eq!(fixture.list(task).await.unwrap().len(), 4);

    // Another task still has its own allowance.
    let foreign = session_id(&fixture.open(other, None).await.unwrap());

    fixture.close(task, sessions[0]).await.unwrap();
    assert_eq!(fixture.list(task).await.unwrap().len(), 3);
    let replacement = session_id(&fixture.open(task, None).await.unwrap());
    assert_ne!(replacement, foreign);
    assert_eq!(fixture.list(task).await.unwrap().len(), 4);

    fixture.shutdown().await;
}
