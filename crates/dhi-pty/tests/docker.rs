//! Docker-backed terminal checks. Every test is ignored by default and skips
//! cleanly when Docker is unavailable:
//!
//! ```text
//! cargo test -p dhi-pty --test docker -- --ignored
//! ```

use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dhi_agent::{RunScope, ToolSink};
use dhi_core::{ToolCallOutcome, ToolCallRequest};
use dhi_exec::{ContainerConfig, TaskContainers};
use dhi_guard::ToolBackend;
use dhi_pty::{MAX_SESSIONS_PER_TASK, PtyBackend, PtyTools};
use rig::tool::ToolContext;
use rig::tool::server::{ToolServer, ToolServerHandle};
use serde_json::{Value, json};
use uuid::Uuid;

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

struct Fixture {
    _temp: tempfile::TempDir,
    containers: Arc<TaskContainers>,
    tools: PtyTools,
    handle: ToolServerHandle,
    tasks: Mutex<Vec<Uuid>>,
}

impl Fixture {
    /// A Docker fixture, or `None` when Docker is not available.
    async fn docker() -> Option<Self> {
        if !docker_available().await {
            eprintln!("skipping: Docker is not available");
            return None;
        }
        let temp = tempfile::tempdir().unwrap();
        let containers =
            Arc::new(TaskContainers::new(temp.path(), ContainerConfig::default()).unwrap());
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
        Some(Self {
            _temp: temp,
            containers,
            tools,
            handle,
            tasks: Mutex::new(Vec::new()),
        })
    }

    fn task(&self) -> Uuid {
        let task = Uuid::new_v4();
        self.tasks.lock().unwrap().push(task);
        task
    }

    async fn call(&self, task_id: Uuid, tool: &str, args: Value) -> anyhow::Result<Value> {
        call(&self.handle, task_id, tool, args).await
    }

    async fn open(&self, task: Uuid) -> Uuid {
        let opened = self.call(task, "shell_open", json!({})).await.unwrap();
        assert_eq!(
            opened.get("cwd").and_then(Value::as_str),
            Some("/work"),
            "the terminal starts at the task root"
        );
        session_id(&opened)
    }

    async fn send(&self, task: Uuid, session: Uuid, text: &str) {
        self.call(
            task,
            "shell_send",
            json!({ "session_id": session, "text": text }),
        )
        .await
        .unwrap();
    }

    async fn read(&self, task: Uuid, session: Uuid, timeout_ms: u64) -> anyhow::Result<Value> {
        self.call(
            task,
            "shell_read",
            json!({ "session_id": session, "timeout_ms": timeout_ms }),
        )
        .await
    }

    async fn close(&self, task: Uuid, session: Uuid) {
        self.call(task, "shell_close", json!({ "session_id": session }))
            .await
            .unwrap();
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
            let reply = self.read(task, session, 1_000).await.unwrap();
            seen.push_str(
                reply
                    .get("output")
                    .and_then(Value::as_str)
                    .expect("read returns output"),
            );
            if seen.contains(needle) {
                return seen;
            }
            if reply.get("closed").and_then(Value::as_bool) == Some(true) {
                break;
            }
        }
        panic!("no {needle:?} in terminal output: {seen:?}");
    }

    async fn shutdown(&self) {
        self.tools.shutdown().await.unwrap();
        self.containers.shutdown().await.unwrap();
    }
}

impl Drop for Fixture {
    /// Best-effort cleanup even when an assertion fails.
    fn drop(&mut self) {
        for task in self.tasks.lock().unwrap().iter() {
            let name = format!("dhi-task-{task}");
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", &name])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

async fn call(
    handle: &ToolServerHandle,
    task_id: Uuid,
    tool: &str,
    args: Value,
) -> anyhow::Result<Value> {
    let mut context = ToolContext::new();
    context
        .insert(RunScope {
            task_id,
            intent_id: Uuid::new_v4(),
        })
        .unwrap();
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

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn pidfile_path(session: Uuid) -> String {
    format!("/tmp/dhi-pty/{session}.pid")
}

async fn docker_available() -> bool {
    let check = tokio::process::Command::new("docker")
        .arg("info")
        .arg("--format")
        .arg("{{.ServerVersion}}")
        .output();
    matches!(
        tokio::time::timeout(Duration::from_secs(15), check).await,
        Ok(Ok(output)) if output.status.success()
    )
}

/// Whether `task`'s container exists and is running.
async fn container_running(task: Uuid) -> bool {
    let name = format!("dhi-task-{task}");
    let check = tokio::process::Command::new("docker")
        .args(["inspect", "-f", "{{.State.Running}}", &name])
        .output();
    matches!(
        tokio::time::timeout(Duration::from_secs(10), check).await,
        Ok(Ok(output))
            if output.status.success()
                && String::from_utf8_lossy(&output.stdout).trim() == "true"
    )
}

async fn container_id(task: Uuid) -> Option<String> {
    let name = format!("dhi-task-{task}");
    let check = tokio::process::Command::new("docker")
        .args(["inspect", "-f", "{{.Id}}", &name])
        .output();
    match tokio::time::timeout(Duration::from_secs(10), check).await {
        Ok(Ok(output)) if output.status.success() => {
            Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        }
        _ => None,
    }
}

async fn docker_exec(task: Uuid, script: &str) -> std::process::Output {
    let name = format!("dhi-task-{task}");
    tokio::process::Command::new("docker")
        .args(["exec", "-i", &name, "sh", "-c", script])
        .output()
        .await
        .expect("docker exec runs")
}

/// The contents of a container file, or `None` when it is missing.
async fn container_cat(task: Uuid, path: &str) -> Option<String> {
    let output = docker_exec(task, &format!("cat {}", shell_quote(path))).await;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        None
    }
}

async fn container_path_exists(task: Uuid, path: &str) -> bool {
    docker_exec(task, &format!("test -e {}", shell_quote(path)))
        .await
        .status
        .success()
}

/// Waits for the proxy to write the session pidfile, then returns the shell
/// PID. The pidfile lives in the container tmpfs, never on the host.
async fn wait_for_pidfile(task: Uuid, session: Uuid) -> u32 {
    let path = pidfile_path(session);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(text) = container_cat(task, &path).await
            && let Ok(pid) = text.parse::<u32>()
        {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "the pidfile {path} was not written"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn process_alive(task: Uuid, pid: u32) -> bool {
    let script = format!("test -d /proc/{pid} && echo alive || echo gone");
    let output = docker_exec(task, &script).await;
    String::from_utf8_lossy(&output.stdout).contains("alive")
}

/// Whether the test's `sleep 30` remains in the container. The container's
/// own entrypoint runs `sleep infinity`, which must not count.
async fn sleep_process_alive(task: Uuid) -> bool {
    docker_exec(
        task,
        "for d in /proc/[0-9]*; do \
           c=$(tr '\\0' ' ' < \"$d/cmdline\" 2>/dev/null); \
           case \"$c\" in 'sleep 30 '*) exit 0;; esac; \
         done; exit 1",
    )
    .await
    .status
    .success()
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn a_terminal_starts_in_work_and_runs_commands() {
    let Some(fixture) = Fixture::docker().await else {
        return;
    };
    let task = fixture.task();

    let session = fixture.open(task).await;
    let listed = fixture.list(task).await.unwrap();
    assert_eq!(listed[0].get("cwd"), Some(&json!("/work")));
    assert_eq!(listed[0].get("alive"), Some(&json!(true)));

    fixture.send(task, session, "pwd").await;
    let output = fixture
        .read_until(task, session, "/work", Duration::from_secs(20))
        .await;
    assert!(output.contains("/work"), "{output}");

    fixture.close(task, session).await;
    assert!(fixture.list(task).await.unwrap().is_empty());
    fixture.shutdown().await;
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn a_python_repl_stays_alive_between_calls() {
    let Some(fixture) = Fixture::docker().await else {
        return;
    };
    let task = fixture.task();
    let session = fixture.open(task).await;

    fixture.send(task, session, "python3").await;
    fixture
        .read_until(task, session, ">>>", Duration::from_secs(20))
        .await;

    fixture.send(task, session, "print(2 + 3)").await;
    let first = fixture
        .read_until(task, session, "5", Duration::from_secs(20))
        .await;
    assert!(first.contains('5'), "{first}");

    // The REPL itself survived the previous call: this is the same
    // interpreter, not a new one-shot command.
    fixture.send(task, session, "print(4 + 5)").await;
    let second = fixture
        .read_until(task, session, "9", Duration::from_secs(20))
        .await;
    assert!(second.contains('9'), "{second}");

    fixture.close(task, session).await;
    fixture.shutdown().await;
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn close_kills_the_container_side_process_group_and_removes_its_pidfile() {
    let Some(fixture) = Fixture::docker().await else {
        return;
    };
    let task = fixture.task();

    // A background child shares the shell's process group and keeps running
    // after the shell goes away; closing the group must kill it too.
    let session = fixture.open(task).await;
    fixture
        .send(task, session, "sleep 30 & echo BG-''STARTED")
        .await;
    fixture
        .read_until(task, session, "BG-STARTED", Duration::from_secs(20))
        .await;
    assert!(
        sleep_process_alive(task).await,
        "the background sleep started"
    );
    let pid = wait_for_pidfile(task, session).await;
    assert!(process_alive(task, pid).await, "the shell is running");

    fixture.close(task, session).await;
    assert!(
        !container_path_exists(task, &pidfile_path(session)).await,
        "close removed the container-side pidfile"
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    while process_alive(task, pid).await || sleep_process_alive(task).await {
        assert!(
            Instant::now() < deadline,
            "the shell process group survived close"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // A foreground sleep is in the same process group as the shell.
    let foreground = fixture.open(task).await;
    fixture.send(task, foreground, "sleep 30").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        sleep_process_alive(task).await,
        "the foreground sleep started"
    );
    fixture.close(task, foreground).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while sleep_process_alive(task).await {
        assert!(
            Instant::now() < deadline,
            "the foreground sleep survived close"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    fixture.shutdown().await;
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn two_tasks_get_separate_sessions() {
    let Some(fixture) = Fixture::docker().await else {
        return;
    };
    let task_a = fixture.task();
    let task_b = fixture.task();
    let session_a = fixture.open(task_a).await;
    let session_b = fixture.open(task_b).await;
    assert_ne!(session_a, session_b);

    // The quotes keep the echoed command line from containing the marker
    // the shell actually prints.
    fixture.send(task_a, session_a, "echo from-''task-a").await;
    let output = fixture
        .read_until(task_a, session_a, "from-task-a", Duration::from_secs(20))
        .await;
    assert!(output.contains("from-task-a"), "{output}");

    for (tool, args) in [
        (
            "shell_send",
            json!({ "session_id": session_a, "text": "echo stolen" }),
        ),
        (
            "shell_read",
            json!({ "session_id": session_a, "timeout_ms": 100 }),
        ),
        ("shell_close", json!({ "session_id": session_a })),
    ] {
        let err = fixture
            .call(task_b, tool, args)
            .await
            .expect_err("task B cannot address task A's session");
        assert!(err.to_string().contains("another task"), "{err}");
    }

    let listed = fixture.list(task_b).await.unwrap();
    assert_eq!(listed.len(), 1, "task B sees only its own session");
    assert_eq!(
        listed[0].get("session_id"),
        Some(&json!(session_b.to_string()))
    );

    // Owners can still drive and close their own sessions.
    fixture.send(task_a, session_a, "echo still-a").await;
    fixture
        .read_until(task_a, session_a, "still-a", Duration::from_secs(20))
        .await;
    fixture.tools.close_task(task_a).await.unwrap();
    assert!(fixture.list(task_a).await.unwrap().is_empty());
    assert_eq!(fixture.list(task_b).await.unwrap().len(), 1);
    fixture.close(task_b, session_b).await;
    fixture.shutdown().await;
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn the_container_survives_a_close_and_a_second_session_opens() {
    let Some(fixture) = Fixture::docker().await else {
        return;
    };
    let task = fixture.task();
    let first = fixture.open(task).await;
    let first_id = container_id(task)
        .await
        .expect("the first container exists");
    fixture.close(task, first).await;

    assert!(
        container_running(task).await,
        "closing a session does not stop the task container"
    );

    let second = fixture.open(task).await;
    fixture.send(task, second, "pwd").await;
    let output = fixture
        .read_until(task, second, "/work", Duration::from_secs(20))
        .await;
    assert!(output.contains("/work"), "{output}");
    assert_eq!(
        container_id(task).await.as_deref(),
        Some(first_id.as_str()),
        "the second session reused the same task container"
    );

    fixture.close(task, second).await;
    fixture.shutdown().await;
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn the_container_boundary_is_inherited() {
    let Some(fixture) = Fixture::docker().await else {
        return;
    };
    let task = fixture.task();
    let session = fixture.open(task).await;
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"));
    // The result words are assembled from variables so the PTY's echo of the
    // command line cannot itself satisfy the markers. `BOUNDARY-done` is the
    // only string that appears after the whole script has run.
    let command = format!(
        "w=writable; r=readonly; v=visible; b=blocked; \
         touch /etc/dhi-pty-probe 2>/dev/null && echo etc-$w || echo etc-$r; \
         touch /work/.dhi/spill/dhi-pty-probe 2>/dev/null && echo spill-$w || \
         echo spill-$r; \
         test -e {} && echo repository-$v || echo repository-$b; \
         m=done; echo BOUNDARY-$m",
        shell_quote(&repository.to_string_lossy())
    );
    fixture.send(task, session, &command).await;

    let output = fixture
        .read_until(task, session, "BOUNDARY-done", Duration::from_secs(20))
        .await;
    assert!(output.contains("etc-readonly"), "{output}");
    assert!(output.contains("spill-readonly"), "{output}");
    assert!(output.contains("repository-blocked"), "{output}");
    assert!(!output.contains("etc-writable"), "{output}");
    assert!(!output.contains("spill-writable"), "{output}");
    assert!(!output.contains("repository-visible"), "{output}");

    fixture.close(task, session).await;
    fixture.shutdown().await;
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn cwd_accepts_a_subdirectory_and_rejects_absolute_parent_and_missing_paths() {
    let Some(fixture) = Fixture::docker().await else {
        return;
    };
    let task = fixture.task();
    let runtime = fixture.containers.ensure(task).await.unwrap();
    let mkdir = docker_exec(task, "mkdir -p /work/sub").await;
    assert!(mkdir.status.success(), "created /work/sub");

    let opened = fixture
        .call(task, "shell_open", json!({ "cwd": "sub" }))
        .await
        .unwrap();
    assert_eq!(opened.get("cwd"), Some(&json!("/work/sub")));
    let session = session_id(&opened);
    fixture.send(task, session, "pwd").await;
    let output = fixture
        .read_until(task, session, "/work/sub", Duration::from_secs(20))
        .await;
    assert!(output.contains("/work/sub"), "{output}");
    fixture.close(task, session).await;
    assert!(fixture.list(task).await.unwrap().is_empty());

    for refused in ["/etc", "..", "../outside", "a/../b"] {
        let err = fixture
            .call(task, "shell_open", json!({ "cwd": refused }))
            .await
            .expect_err("an absolute or parent cwd is refused");
        let text = err.to_string();
        assert!(
            text.contains("relative") || text.contains(".."),
            "{refused}: {text}"
        );
        assert!(
            fixture.list(task).await.unwrap().is_empty(),
            "no session was created for {refused}"
        );
    }

    let err = fixture
        .call(task, "shell_open", json!({ "cwd": "missing" }))
        .await
        .expect_err("a missing container cwd fails the readiness handshake");
    let text = err.to_string();
    assert!(
        text.contains("No such file")
            || text.contains("not found")
            || text.contains("cannot")
            || text.contains("did not become ready"),
        "{text}"
    );
    assert!(
        !text.contains(runtime.host_dir.to_string_lossy().as_ref()),
        "a container cwd error must not reveal the host directory: {text}"
    );
    assert!(
        fixture.list(task).await.unwrap().is_empty(),
        "the failed open returned no session"
    );

    // The failures did not leave the manager unusable.
    let recovered = fixture.open(task).await;
    fixture.close(task, recovered).await;
    fixture.shutdown().await;
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn exit_is_reported_as_closed() {
    let Some(fixture) = Fixture::docker().await else {
        return;
    };
    let task = fixture.task();
    let session = fixture.open(task).await;
    fixture.send(task, session, "exit").await;

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut closed = false;
    while Instant::now() < deadline {
        let reply = fixture.read(task, session, 1_000).await.unwrap();
        if reply.get("closed").and_then(Value::as_bool) == Some(true) {
            closed = true;
            break;
        }
    }
    assert!(closed, "shell_read reports the shell exit");

    let listed = fixture.list(task).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].get("alive"), Some(&json!(false)));

    fixture.close(task, session).await;
    assert!(fixture.list(task).await.unwrap().is_empty());
    fixture.shutdown().await;
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn output_larger_than_the_buffer_reports_truncation() {
    let Some(fixture) = Fixture::docker().await else {
        return;
    };
    let task = fixture.task();
    let session = fixture.open(task).await;
    fixture.send(task, session, "yes x | head -c 307200").await;

    // Do not read while output is produced: watch the reported unread size
    // until the buffer has visibly reached its cap, then read once.
    let cap = 256 * 1024;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let listed = fixture.list(task).await.unwrap();
        let buffered = listed
            .first()
            .and_then(|info| info.get("buffered_bytes"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if buffered >= cap as u64 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the output buffer never filled; last size {buffered}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let reply = fixture.read(task, session, 100).await.unwrap();
    assert_eq!(
        reply.get("truncated").and_then(Value::as_bool),
        Some(true),
        "{reply:?}"
    );
    let dropped = reply
        .get("dropped_bytes")
        .and_then(Value::as_u64)
        .expect("read returns dropped_bytes");
    assert!(dropped > 0, "{reply:?}");
    let output = reply
        .get("output")
        .and_then(Value::as_str)
        .expect("read returns output");
    assert!(
        output.len() <= cap,
        "the returned output respects the cap: {}",
        output.len()
    );

    fixture.close(task, session).await;
    fixture.shutdown().await;
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn a_task_is_limited_to_four_sessions() {
    let Some(fixture) = Fixture::docker().await else {
        return;
    };
    let task = fixture.task();
    let mut sessions = Vec::new();
    for _ in 0..MAX_SESSIONS_PER_TASK {
        sessions.push(fixture.open(task).await);
    }

    let err = fixture
        .call(task, "shell_open", json!({}))
        .await
        .expect_err("a fifth session is refused");
    assert!(err.to_string().contains("maximum is 4"), "{err}");
    assert_eq!(
        fixture.list(task).await.unwrap().len(),
        MAX_SESSIONS_PER_TASK
    );

    fixture.close(task, sessions[0]).await;
    assert_eq!(
        fixture.list(task).await.unwrap().len(),
        MAX_SESSIONS_PER_TASK - 1
    );
    let replacement = fixture.open(task).await;
    assert!(!sessions.contains(&replacement));
    assert_eq!(
        fixture.list(task).await.unwrap().len(),
        MAX_SESSIONS_PER_TASK
    );

    for session in sessions.into_iter().skip(1).chain([replacement]) {
        fixture.close(task, session).await;
    }
    fixture.shutdown().await;
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn an_invalid_pidfile_keeps_the_session_and_a_missing_one_is_already_dead() {
    let Some(fixture) = Fixture::docker().await else {
        return;
    };
    let task = fixture.task();

    let session = fixture.open(task).await;
    let path = pidfile_path(session);
    let pid = wait_for_pidfile(task, session).await;
    let write = docker_exec(
        task,
        &format!("printf '%s' not-a-pid > {}", shell_quote(&path)),
    )
    .await;
    assert!(write.status.success(), "planted an invalid pidfile");

    let err = fixture
        .call(task, "shell_close", json!({ "session_id": session }))
        .await
        .expect_err("an invalid pidfile is a cleanup error");
    assert!(
        err.to_string().contains("invalid") || err.to_string().contains("pidfile"),
        "{err}"
    );
    assert_eq!(
        fixture.list(task).await.unwrap().len(),
        1,
        "the session is kept for a retry"
    );

    // Restore a valid pid; the retry can finish cleanup and remove it.
    let restore = docker_exec(task, &format!("printf '%s' {pid} > {}", shell_quote(&path))).await;
    assert!(restore.status.success(), "restored the pidfile");
    fixture.close(task, session).await;
    assert!(fixture.list(task).await.unwrap().is_empty());

    // A missing pidfile means the proxy never started a shell, or an earlier
    // close already cleaned it up; close succeeds and forgets the session.
    let missing = fixture.open(task).await;
    let missing_path = pidfile_path(missing);
    let _ = wait_for_pidfile(task, missing).await;
    let removed = docker_exec(task, &format!("rm -f {}", shell_quote(&missing_path))).await;
    assert!(removed.status.success(), "removed the pidfile");
    fixture.close(task, missing).await;
    assert!(fixture.list(task).await.unwrap().is_empty());

    fixture.shutdown().await;
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn a_large_send_and_close_finish_within_a_bounded_time() {
    let Some(fixture) = Fixture::docker().await else {
        return;
    };
    let task = fixture.task();
    let session = fixture.open(task).await;
    let text = "x".repeat(64 * 1024);

    let send = fixture.call(
        task,
        "shell_send",
        json!({ "session_id": session, "text": text }),
    );
    let close = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        fixture
            .call(task, "shell_close", json!({ "session_id": session }))
            .await
    };

    let (send_result, close_result) =
        tokio::time::timeout(Duration::from_secs(15), async { tokio::join!(send, close) })
            .await
            .expect("a large send and a concurrent close both finish");
    assert!(
        close_result.is_ok(),
        "close finished and removed the session even while send held stdin: {close_result:?}"
    );
    // The send may succeed or lose the race with close; either way it must
    // have returned rather than leaving the stdin mutex held.
    let _ = send_result;
    assert!(fixture.list(task).await.unwrap().is_empty());

    fixture.shutdown().await;
}
