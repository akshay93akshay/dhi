//! Persistent terminal sessions inside a task runtime.
//!
//! A session is one long-lived `/bin/sh`. In container mode it runs behind
//! the Python PTY proxy in [`crate::proxy`], reached with
//! `docker exec -i dhi-task-<task-id> python3 -u -c <proxy> <cwd> <pidfile>`.
//! In local test mode it is a plain `/bin/sh` with piped stdio, started in
//! the task directory with a minimal environment. In native macOS sandbox
//! mode it is also a `/bin/sh` with piped stdio, started as the same macOS
//! user through `/usr/bin/env ... /usr/bin/sandbox-exec`. The container
//! boundary, mounts, and limits are inherited from
//! [`dhi_exec::TaskContainers`]; this module only adds the process and its
//! output buffer.
//!
//! Every session stores its task id. Lookups take the task id the caller
//! was authorized for, so task B cannot address task A's session even when
//! it knows the session id.
//!
//! # Container process cleanup
//!
//! The proxy writes the shell PID to `/tmp/dhi-pty/<session-id>.pid`, which
//! is in the container's writable tmpfs. Nothing is created or permissioned
//! on the host outside the task mount. Close kills the shell's process group
//! with `kill -TERM -<pid>` and, after a short grace period,
//! `kill -KILL -<pid>`. The pid is read back strictly as decimal digits and
//! must be greater than one, so a corrupt pidfile can never turn into
//! `kill -1` or `kill 0`.
//!
//! A native sandbox session is a host process: `spawn_sandbox` calls `setsid`
//! in the child, so the `/usr/bin/env ... sandbox-exec` wrapper is a
//! process-group leader and close signals the whole group with
//! `kill(-pid, ...)`. That stops `sandbox-exec`, the shell, and any jobs it
//! started. Local and Docker sessions signal only their direct child.
//!
//! A session is removed from the map only after cleanup succeeds, except when
//! the container is gone or the pidfile is missing: those mean the session is
//! already dead. A cleanup error keeps the session so a later `shell_close`
//! can retry it.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{anyhow, bail};
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, Notify, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout, timeout_at};
use uuid::Uuid;

use dhi_exec::{SandboxRuntime, TaskContainers, TaskRuntime, sandbox_prefix};

use crate::proxy::{PYTHON_PROXY, READY_MARKER};

/// The largest number of bytes a session keeps, including output a caller
/// has already read but that has not been displaced yet.
const BUFFER_CAP: usize = 256 * 1024;

/// The maximum number of live sessions one task may have.
pub const MAX_SESSIONS_PER_TASK: usize = 4;

/// The largest accepted `shell_send` text, in bytes.
pub const MAX_SEND_BYTES: usize = 64 * 1024;

/// How long [`PtySessions::close`] waits for the shell to exit after `exit`.
const CLOSE_WAIT: Duration = Duration::from_secs(1);

/// How long close waits for a busy stdin writer before it stops the process.
const CLOSE_STDIN_WAIT: Duration = Duration::from_millis(250);

/// How long close waits for the `exit` line itself to be written.
const CLOSE_STDIN_WRITE: Duration = Duration::from_millis(250);

/// How long [`PtySessions::send`] may hold the stdin mutex for one write.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// How long close waits for the shell to exit after `SIGTERM`.
const TERMINATE_WAIT: Duration = Duration::from_millis(500);

/// How long close waits for the shell to exit after `SIGKILL`.
const KILL_WAIT: Duration = Duration::from_secs(1);

/// How long close waits for the output readers to drain after the process
/// has stopped. A background child can keep a pipe open, so this is bounded.
const READER_CLOSE_WAIT: Duration = Duration::from_secs(2);

/// How long the container-side pidfile cleanup may take.
const CONTAINER_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

/// How long `shell_open` waits for the proxy's readiness marker.
const READY_TIMEOUT: Duration = Duration::from_secs(5);

/// How much pre-marker stdout is held while looking for the marker.
const READY_PENDING_MAX: usize = 64 * 1024;

/// The default `shell_read` timeout, in milliseconds.
pub const DEFAULT_READ_TIMEOUT_MS: u64 = 1_000;

/// The largest accepted `shell_read` timeout, in milliseconds.
pub const MAX_READ_TIMEOUT_MS: u64 = 30_000;

/// The default `PATH` for a local test shell when the parent has none.
const DEFAULT_PATH: &str =
    "/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

/// `SIGTERM`, used to stop a local terminal process politely.
#[cfg(unix)]
const TERMINATE_SIGNAL: i32 = libc::SIGTERM;
#[cfg(not(unix))]
const TERMINATE_SIGNAL: i32 = 15;

/// `SIGKILL`, used when `SIGTERM` did not stop the process.
#[cfg(unix)]
const KILL_SIGNAL: i32 = libc::SIGKILL;
#[cfg(not(unix))]
const KILL_SIGNAL: i32 = 9;

/// The reply of `PtySessions::open`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OpenReply {
    /// The id used by every later call for this session.
    pub session_id: Uuid,
    /// The directory the shell started in.
    pub cwd: String,
}

/// The reply of `PtySessions::read`.
///
/// Decoding is lossy: an incomplete UTF-8 sequence at a read boundary is
/// replaced with `U+FFFD` rather than held back. Bytes that had to be evicted
/// from the bounded buffer before a read are counted in `dropped_bytes`, and
/// `truncated` is true when that count is non-zero.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReadReply {
    /// Output produced since the previous read. Empty when the timeout
    /// elapsed with no new output.
    pub output: String,
    /// Whether the shell process has exited. Output from a background child
    /// may still arrive after this becomes true.
    pub closed: bool,
    /// Unread bytes evicted from the bounded buffer since the previous read.
    pub dropped_bytes: u64,
    /// Whether this read lost output to the bounded buffer.
    pub truncated: bool,
}

/// One entry of `PtySessions::list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionInfo {
    /// The session id.
    pub session_id: Uuid,
    /// The directory the shell started in.
    pub cwd: String,
    /// Whether the shell process is still running.
    pub alive: bool,
    /// Unread bytes currently buffered.
    pub buffered_bytes: usize,
}

/// Owns every persistent terminal session for a task container manager.
///
/// Production must call [`shutdown`](Self::shutdown); there is no async
/// `Drop`, and dropping the manager without it leaves running sessions
/// behind. `shutdown` is what closes every session deliberately and reports
/// failures.
pub(crate) struct PtySessions {
    containers: Arc<TaskContainers>,
    sessions: Mutex<HashMap<Uuid, Arc<Session>>>,
    /// Serializes session starts so the per-task limit is exact.
    opening: Mutex<()>,
    /// Set by [`shutdown`](PtySessions::shutdown); no new session starts
    /// after it.
    closed: AtomicBool,
}

impl PtySessions {
    /// A manager over `containers`. No process starts until
    /// [`open`](Self::open) is called.
    pub(crate) fn new(containers: Arc<TaskContainers>) -> Self {
        Self {
            containers,
            sessions: Mutex::new(HashMap::new()),
            opening: Mutex::new(()),
            closed: AtomicBool::new(false),
        }
    }

    /// Starts a persistent shell for `task_id`.
    ///
    /// `cwd` must be a relative path below the task root, or absent for the
    /// root itself. Absolute paths and any `..` component are refused before
    /// a process starts. In container mode `cwd` is a relative container path
    /// below the task mount; the proxy resolves and enters it, so the host
    /// never inspects a container path.
    ///
    /// A task may have at most [`MAX_SESSIONS_PER_TASK`] sessions. After
    /// [`shutdown`](Self::shutdown) no new session starts.
    pub(crate) async fn open(
        &self,
        task_id: Uuid,
        cwd: Option<String>,
    ) -> anyhow::Result<OpenReply> {
        if self.closed.load(Ordering::SeqCst) {
            bail!("terminal sessions are shut down; no new session may start");
        }

        // One start at a time makes the session-limit check exact without
        // holding the session map across process startup.
        let _opening = self.opening.lock().await;
        if self.closed.load(Ordering::SeqCst) {
            bail!("terminal sessions are shut down; no new session may start");
        }
        {
            let sessions = self.sessions.lock().await;
            let count = sessions
                .values()
                .filter(|session| session.task_id == task_id)
                .count();
            if count >= MAX_SESSIONS_PER_TASK {
                bail!(
                    "task {task_id} already has {count} terminal sessions; the maximum is \
                     {MAX_SESSIONS_PER_TASK}. Close one with shell_close before opening another"
                );
            }
        }

        let runtime = self.containers.ensure(task_id).await?;
        let cwd_path = resolve_cwd(&runtime, cwd.as_deref())?;

        let session_id = Uuid::new_v4();
        // The Docker proxy is the only mode with a readiness handshake and a
        // container-side pidfile. A native sandbox is a host process and uses
        // pipes exactly like local test mode.
        let uses_container_proxy = runtime.is_container && runtime.sandbox.is_none();
        let (mut child, container, target) = if let Some(sandbox) = &runtime.sandbox {
            let child = spawn_sandbox(sandbox, &cwd_path).map_err(|err| {
                anyhow!(
                    "cannot start a sandboxed terminal in {}: {err}",
                    cwd_path.display()
                )
            })?;
            let target = format!("sandbox {}", cwd_path.display());
            (child, None, target)
        } else if uses_container_proxy {
            let name = self.containers.container_name(task_id);
            let pidfile = format!("/tmp/dhi-pty/{session_id}.pid");
            let child = spawn_container(&name, &cwd_path, &pidfile)
                .map_err(|err| anyhow!("cannot start a terminal in container {name}: {err}"))?;
            let target = format!("container {name}");
            (child, Some(ContainerClose { name, pidfile }), target)
        } else {
            let child = spawn_local(&runtime, &cwd_path).map_err(|err| {
                anyhow!(
                    "cannot start a local terminal in {}: {err}",
                    cwd_path.display()
                )
            })?;
            let target = format!("{}", cwd_path.display());
            (child, None, target)
        };

        let pid = child.id().ok_or_else(|| {
            anyhow!("the terminal process in {target} exited before its process id was known")
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("the terminal process in {target} did not provide stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("the terminal process in {target} did not provide stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("the terminal process in {target} did not provide stderr"))?;

        let cwd_text = cwd_path.to_string_lossy().into_owned();
        let kill_group = runtime.sandbox.is_some();
        let session = Arc::new(Session {
            id: session_id,
            task_id,
            cwd: cwd_text.clone(),
            container,
            stdin: Mutex::new(Some(stdin)),
            pid,
            kill_group,
            buffer: Mutex::new(Buffer::default()),
            notify: Notify::new(),
            exited: AtomicBool::new(false),
            readers_open: AtomicUsize::new(2),
            readers_done: AtomicBool::new(false),
            readers: Mutex::new(Vec::new()),
            close_lock: Mutex::new(()),
        });

        let mut ready = None;
        let stdout_reader = if uses_container_proxy {
            let (ready_tx, ready_rx) = oneshot::channel();
            ready = Some(ready_rx);
            spawn_ready_reader(stdout, Arc::clone(&session), ready_tx)
        } else {
            spawn_reader(stdout, Arc::clone(&session))
        };
        let stderr_reader = spawn_reader(stderr, Arc::clone(&session));
        *session.readers.lock().await = vec![stdout_reader, stderr_reader];
        // The waiter owns the child, reaps it, and is the authority for
        // `closed`/`alive`. `kill_on_drop` still covers a runtime shutdown.
        drop(spawn_waiter(child, Arc::clone(&session)));

        if let Some(ready) = ready {
            match timeout(READY_TIMEOUT, ready).await {
                Ok(Ok(true)) => {}
                Ok(Ok(false)) | Ok(Err(_)) => {
                    let details = abort_with_details(&session).await;
                    bail!(
                        "the terminal process in {target} exited before it became ready{details}"
                    );
                }
                Err(_) => {
                    let details = abort_with_details(&session).await;
                    bail!(
                        "the terminal process in {target} did not become ready within {} \
                         seconds{details}",
                        READY_TIMEOUT.as_secs()
                    );
                }
            }
        }

        {
            // `shutdown` sets the flag before it walks the map. Holding the
            // map lock here makes the insert atomic with that decision: the
            // new session either lands in the map and is drained, or sees
            // the closed flag and is stopped.
            let mut sessions = self.sessions.lock().await;
            if self.closed.load(Ordering::SeqCst) {
                drop(sessions);
                let _ = close_session(&session).await;
                bail!("terminal sessions are shut down; the new session was stopped");
            }
            sessions.insert(session_id, Arc::clone(&session));
        }

        tracing::debug!(%task_id, %session_id, cwd = %cwd_text, "opened terminal session");
        Ok(OpenReply {
            session_id,
            cwd: cwd_text,
        })
    }

    /// Sends `text` to a session, appending a newline when it does not end
    /// with one. Returns the number of bytes written, including the newline.
    ///
    /// The text is capped at [`MAX_SEND_BYTES`]. The stdin mutex is held only
    /// for the write, and the write is bounded by [`SEND_TIMEOUT`]; a slow or
    /// stopped terminal cannot keep the mutex forever.
    pub(crate) async fn send(
        &self,
        task_id: Uuid,
        session_id: Uuid,
        text: &str,
    ) -> anyhow::Result<usize> {
        if text.len() > MAX_SEND_BYTES {
            bail!(
                "shell_send text is {} bytes; the maximum is {MAX_SEND_BYTES} bytes (64 KiB)",
                text.len()
            );
        }
        let session = self.session(task_id, session_id).await?;
        if session.exited.load(Ordering::SeqCst) {
            bail!("terminal session {session_id} has closed");
        }

        let mut payload = text.to_owned();
        if !payload.ends_with('\n') {
            payload.push('\n');
        }
        let bytes = payload.len();

        let write = async {
            let mut stdin = session.stdin.lock().await;
            let Some(writer) = stdin.as_mut() else {
                bail!("terminal session {session_id} is closed");
            };
            writer
                .write_all(payload.as_bytes())
                .await
                .map_err(|err| anyhow!("cannot write to terminal session {session_id}: {err}"))?;
            writer
                .flush()
                .await
                .map_err(|err| anyhow!("cannot flush terminal session {session_id}: {err}"))?;
            Ok::<(), anyhow::Error>(())
        };

        match timeout(SEND_TIMEOUT, write).await {
            Ok(result) => result?,
            Err(_) => bail!(
                "timed out after {} seconds writing to terminal session {session_id}; the \
                 terminal is not reading input",
                SEND_TIMEOUT.as_secs()
            ),
        }
        Ok(bytes)
    }

    /// Reads output produced since the previous read.
    ///
    /// Waits up to `timeout_ms` (clamped to [`MAX_READ_TIMEOUT_MS`]) for
    /// output, or returns early when the session closes.
    pub(crate) async fn read(
        &self,
        task_id: Uuid,
        session_id: Uuid,
        timeout_ms: u64,
    ) -> anyhow::Result<ReadReply> {
        let session = self.session(task_id, session_id).await?;
        let wait = Duration::from_millis(timeout_ms.min(MAX_READ_TIMEOUT_MS));
        let deadline = Instant::now() + wait;

        loop {
            // Register the waiter before looking at the buffer. A reader
            // or the child waiter that acts between the check and the wait
            // then still wakes this future.
            let notified = session.notify.notified();
            tokio::pin!(notified);
            let _ = notified.as_mut().enable();

            let closed = session.exited.load(Ordering::SeqCst);
            let (output, dropped) = {
                let mut buffer = session.buffer.lock().await;
                buffer.take()
            };
            if !output.is_empty() || dropped > 0 || closed {
                return Ok(read_reply(output, dropped, closed));
            }
            if wait.is_zero() || Instant::now() >= deadline {
                return Ok(read_reply(Vec::new(), 0, closed));
            }

            if timeout_at(deadline, notified.as_mut()).await.is_err() {
                // Check once more: output could have arrived at the deadline.
                let closed = session.exited.load(Ordering::SeqCst);
                let (output, dropped) = {
                    let mut buffer = session.buffer.lock().await;
                    buffer.take()
                };
                return Ok(read_reply(output, dropped, closed));
            }
        }
    }

    /// Closes a session and stops its process.
    ///
    /// Closing a session whose process already exited succeeds. An unknown
    /// session, or one owned by another task, is an error. If the container
    /// cleanup fails, the session stays in the map so the call may be
    /// retried; it is removed only after cleanup succeeds or the session is
    /// found to be already dead.
    pub(crate) async fn close(&self, task_id: Uuid, session_id: Uuid) -> anyhow::Result<()> {
        let session = self.session(task_id, session_id).await?;
        self.close_arc(&session).await
    }

    /// Closes every session that belongs to `task_id`.
    pub(crate) async fn close_task(&self, task_id: Uuid) -> anyhow::Result<()> {
        let targets: Vec<Arc<Session>> = {
            let sessions = self.sessions.lock().await;
            sessions
                .values()
                .filter(|session| session.task_id == task_id)
                .cloned()
                .collect()
        };

        let mut failures = Vec::new();
        for session in targets {
            if let Err(err) = self.close_arc(&session).await {
                failures.push(format!("{}: {err}", session.id));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(anyhow!(
                "could not close every terminal session of task {task_id}:\n{}",
                failures.join("\n")
            ))
        }
    }

    /// The sessions that belong to `task_id`, and only those. At most
    /// [`MAX_SESSIONS_PER_TASK`] are ever returned.
    pub(crate) async fn list(&self, task_id: Uuid) -> anyhow::Result<Vec<SessionInfo>> {
        let sessions = self.sessions.lock().await;
        let mut list = Vec::new();
        for session in sessions
            .values()
            .filter(|session| session.task_id == task_id)
        {
            let buffered_bytes = {
                let buffer = session.buffer.lock().await;
                buffer.unread_len()
            };
            list.push(SessionInfo {
                session_id: session.id,
                cwd: session.cwd.clone(),
                alive: !session.exited.load(Ordering::SeqCst),
                buffered_bytes,
            });
        }
        drop(sessions);
        list.sort_by_key(|info| info.session_id);
        list.truncate(MAX_SESSIONS_PER_TASK);
        Ok(list)
    }

    /// Closes every session this manager owns and refuses any new session.
    /// It does not stop the task containers; that remains
    /// [`TaskContainers::shutdown`]'s job.
    ///
    /// A session whose cleanup fails stays in the map, so a later call
    /// retries it.
    pub(crate) async fn shutdown(&self) -> anyhow::Result<()> {
        self.closed.store(true, Ordering::SeqCst);
        let sessions: Vec<Arc<Session>> = {
            let map = self.sessions.lock().await;
            map.values().cloned().collect()
        };

        let mut failures = Vec::new();
        for session in sessions {
            if let Err(err) = self.close_arc(&session).await {
                failures.push(format!("{}: {err}", session.id));
            }
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(anyhow!(
                "could not close every terminal session:\n{}",
                failures.join("\n")
            ))
        }
    }

    /// Stops one already-resolved session and removes it only on success.
    async fn close_arc(&self, session: &Arc<Session>) -> anyhow::Result<()> {
        let result = close_session(session).await;
        if result.is_ok() {
            let mut sessions = self.sessions.lock().await;
            if sessions
                .get(&session.id)
                .is_some_and(|present| Arc::ptr_eq(present, session))
            {
                sessions.remove(&session.id);
            }
        }
        result
    }

    /// Looks a session up for `task_id`, refusing a foreign session.
    async fn session(&self, task_id: Uuid, session_id: Uuid) -> anyhow::Result<Arc<Session>> {
        let sessions = self.sessions.lock().await;
        match sessions.get(&session_id) {
            Some(session) if session.task_id == task_id => Ok(Arc::clone(session)),
            Some(_) => bail!("terminal session {session_id} belongs to another task"),
            None => bail!("no terminal session {session_id} exists"),
        }
    }
}

/// The process state and buffered output of one session.
struct Session {
    /// The session id, kept here for cleanup error messages.
    id: Uuid,
    /// The task this session belongs to. Lookups compare it.
    task_id: Uuid,
    /// The directory shown in `open` and `list`.
    cwd: String,
    /// The container-side pidfile, when the proxy is used.
    container: Option<ContainerClose>,
    /// The shell's stdin. Taking and dropping it closes the pipe.
    stdin: Mutex<Option<ChildStdin>>,
    /// The OS process id of `/bin/sh` or the local `docker exec` client.
    pid: u32,
    /// Whether `pid` is the leader of a process group this session owns.
    /// Native sandbox sessions start under `setsid`, so close must signal
    /// the whole group to stop `sandbox-exec`, the shell, and its jobs.
    /// Local and Docker sessions signal only their direct child.
    kill_group: bool,
    /// Output produced by both readers and not yet returned by `read`.
    buffer: Mutex<Buffer>,
    /// Wakes `read`, close and the child waiter.
    notify: Notify,
    /// Set by the child waiter when the local process has been reaped.
    exited: AtomicBool,
    /// How many reader tasks have not ended yet.
    readers_open: AtomicUsize,
    /// Set when both readers ended or close stopped waiting for them.
    readers_done: AtomicBool,
    /// The stdout and stderr reader tasks, awaited by close.
    readers: Mutex<Vec<JoinHandle<()>>>,
    /// Makes concurrent closes of one session serial.
    close_lock: Mutex<()>,
}

/// What close needs to stop the process inside a container.
struct ContainerClose {
    /// The task container's name.
    name: String,
    /// The pidfile the proxy wrote, as a container path.
    pidfile: String,
}

/// A bounded byte buffer with a read cursor.
#[derive(Debug, Default)]
struct Buffer {
    bytes: Vec<u8>,
    /// Everything below this index has already been returned by `take`.
    read: usize,
    /// Unread bytes evicted since the previous `take`.
    dropped_since_take: u64,
}

impl Buffer {
    /// Appends output, dropping the oldest bytes when the cap is exceeded.
    fn append(&mut self, data: &[u8]) {
        self.bytes.extend_from_slice(data);
        let overflow = self.bytes.len().saturating_sub(BUFFER_CAP);
        if overflow > 0 {
            let unread_dropped = overflow.saturating_sub(self.read);
            self.dropped_since_take = self
                .dropped_since_take
                .saturating_add(unread_dropped as u64);
            self.bytes.drain(..overflow);
            self.read = self.read.saturating_sub(overflow);
        }
    }

    /// Returns every byte after the read cursor and advances the cursor.
    /// The second value is the number of unread bytes evicted since the
    /// previous call.
    fn take(&mut self) -> (Vec<u8>, u64) {
        let dropped = std::mem::take(&mut self.dropped_since_take);
        if self.read >= self.bytes.len() {
            return (Vec::new(), dropped);
        }
        let output = self.bytes[self.read..].to_vec();
        self.read = self.bytes.len();
        (output, dropped)
    }

    /// The number of bytes [`take`](Self::take) would return.
    fn unread_len(&self) -> usize {
        self.bytes.len().saturating_sub(self.read)
    }
}

/// Builds a `ReadReply` from raw output and the eviction count.
fn read_reply(output: Vec<u8>, dropped: u64, closed: bool) -> ReadReply {
    ReadReply {
        output: String::from_utf8_lossy(&output).into_owned(),
        closed,
        dropped_bytes: dropped,
        truncated: dropped > 0,
    }
}

/// Starts the container-side proxy.
fn spawn_container(name: &str, cwd: &Path, pidfile: &str) -> io::Result<Child> {
    let mut command = Command::new("docker");
    command
        .arg("exec")
        .arg("-i")
        .arg(name)
        .arg("python3")
        .arg("-u")
        .arg("-c")
        .arg(PYTHON_PROXY)
        .arg(cwd)
        .arg(pidfile)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command.spawn()
}

/// Starts the local test shell with piped stdio and a scrubbed environment.
fn spawn_local(runtime: &TaskRuntime, cwd: &Path) -> io::Result<Child> {
    let path = std::env::var_os("PATH")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| OsString::from(DEFAULT_PATH));
    let mut command = Command::new("/bin/sh");
    command
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env_clear()
        .env("PATH", path)
        .env("HOME", runtime.host_dir.join(".home"))
        .env("TMPDIR", runtime.host_dir.join(".tmp"))
        .env("TERM", "dumb")
        .env("NO_COLOR", "1");
    command.spawn()
}

/// Starts one `/bin/sh` inside the native macOS sandbox with piped stdio.
///
/// There is no PTY on the host path, so stdin, stdout, and stderr are pipes
/// exactly like local test mode. The prefix comes from
/// [`dhi_exec::sandbox_prefix`], so the terminal and the file/shell tools
/// share one wrapper; it is `/usr/bin/env PATH=... HOME=... TMPDIR=...
/// CARGO_HOME=... GIT_CONFIG_NOSYSTEM=1 TERM=dumb NO_COLOR=1
/// DHI_TASK_ID=... /usr/bin/sandbox-exec -f <profile> -D PROJECT=... -D
/// WORK=... -D SPILL=...`. There is no `sudo` and no task user: the sandbox
/// runs as the same macOS user as the server. Only `PATH` is kept on the
/// process itself; the prefix sets the rest of the environment.
///
/// The child is a session and process-group leader (`setsid`), so
/// [`terminate_local`] can signal the whole `sandbox-exec`/`sh` tree instead
/// of only the wrapper. macOS has no `/usr/bin/setsid`, so the group is
/// created in the child just before `exec`.
fn spawn_sandbox(runtime: &SandboxRuntime, cwd: &Path) -> io::Result<Child> {
    let mut prefix = sandbox_prefix(runtime);
    prefix.push(OsString::from("/bin/sh"));
    let mut command = Command::new(&prefix[0]);
    command
        .args(&prefix[1..])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env_clear()
        .env("PATH", DEFAULT_PATH);
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn()
}

/// Spawns one reader that appends to the session buffer and wakes readers.
fn spawn_reader<R>(mut reader: R, session: Arc<Session>) -> JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut chunk = [0_u8; 16 * 1024];
        loop {
            match reader.read(&mut chunk).await {
                Ok(0) => break,
                Ok(count) => append_output(&session, &chunk[..count]).await,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        reader_finished(&session).await;
    })
}

/// Spawns the container stdout reader: it holds stdout back until the
/// readiness marker arrives, strips that marker, and then behaves like a
/// normal reader. `false` means the stream ended before the marker.
fn spawn_ready_reader<R>(
    mut reader: R,
    session: Arc<Session>,
    ready: oneshot::Sender<bool>,
) -> JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut ready = Some(ready);
        let mut pending = Vec::new();
        let mut synced = false;
        let mut chunk = [0_u8; 16 * 1024];

        loop {
            match reader.read(&mut chunk).await {
                Ok(0) => {
                    if !synced {
                        if !pending.is_empty() {
                            append_output(&session, &pending).await;
                        }
                        if let Some(sender) = ready.take() {
                            let _ = sender.send(false);
                        }
                    }
                    break;
                }
                Ok(count) => {
                    if synced {
                        append_output(&session, &chunk[..count]).await;
                        continue;
                    }

                    pending.extend_from_slice(&chunk[..count]);
                    if let Some(position) = find_bytes(&pending, READY_MARKER) {
                        if position > 0 {
                            append_output(&session, &pending[..position]).await;
                        }
                        let after = position + READY_MARKER.len();
                        if after < pending.len() {
                            append_output(&session, &pending[after..]).await;
                        }
                        pending.clear();
                        synced = true;
                        if let Some(sender) = ready.take() {
                            let _ = sender.send(true);
                        }
                    } else if pending.len() > READY_PENDING_MAX {
                        // Keep enough of the tail that a marker split across
                        // reads is still found; everything older is output.
                        let keep = READY_MARKER.len().saturating_sub(1);
                        let flush = pending.len().saturating_sub(keep);
                        append_output(&session, &pending[..flush]).await;
                        pending.drain(..flush);
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    if !synced {
                        if !pending.is_empty() {
                            append_output(&session, &pending).await;
                        }
                        if let Some(sender) = ready.take() {
                            let _ = sender.send(false);
                        }
                    }
                    break;
                }
            }
        }

        reader_finished(&session).await;
    })
}

/// Appends output and wakes every waiter.
async fn append_output(session: &Session, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    {
        let mut buffer = session.buffer.lock().await;
        buffer.append(data);
    }
    session.notify.notify_waiters();
}

/// Records that one reader ended; the last one marks the readers done.
async fn reader_finished(session: &Session) {
    if session.readers_open.fetch_sub(1, Ordering::SeqCst) == 1 {
        session.readers_done.store(true, Ordering::SeqCst);
        session.notify.notify_waiters();
    }
}

/// Finds `needle` in `haystack`.
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Owns the child, reaps it, and is the authority for `closed` and `alive`.
fn spawn_waiter(mut child: Child, session: Arc<Session>) -> JoinHandle<()> {
    tokio::spawn(async move {
        match child.wait().await {
            Ok(status) => {
                tracing::debug!(session_id = %session.id, ?status, "terminal process exited");
            }
            Err(err) => {
                tracing::warn!(session_id = %session.id, %err, "cannot wait for terminal process");
            }
        }
        mark_exited(&session);
    })
}

/// Sets the process-exit flag and wakes readers and close.
fn mark_exited(session: &Session) {
    session.exited.store(true, Ordering::SeqCst);
    session.notify.notify_waiters();
}

/// Waits up to `wait` for the child waiter to report the process gone.
async fn wait_for_exit(session: &Session, wait: Duration) -> bool {
    if session.exited.load(Ordering::SeqCst) {
        return true;
    }
    timeout(wait, async {
        loop {
            let notified = session.notify.notified();
            tokio::pin!(notified);
            let _ = notified.as_mut().enable();
            if session.exited.load(Ordering::SeqCst) {
                break;
            }
            notified.await;
        }
    })
    .await
    .is_ok()
}

/// Stops one session, then drains its readers.
async fn close_session(session: &Session) -> anyhow::Result<()> {
    let _closing = session.close_lock.lock().await;

    // Ask the shell to leave. Holding the stdin mutex is bounded: a send in
    // progress must not make close wait forever. The write itself is bounded
    // too, for a reader that is not consuming input.
    if let Ok(mut stdin) = timeout(CLOSE_STDIN_WAIT, session.stdin.lock()).await
        && let Some(mut writer) = stdin.take()
    {
        let _ = timeout(CLOSE_STDIN_WRITE, async {
            writer.write_all(b"exit\n").await?;
            writer.flush().await
        })
        .await;
    }

    let mut failures = Vec::new();

    if !wait_for_exit(session, CLOSE_WAIT).await
        && let Err(err) = terminate_local(session).await
    {
        failures.push(format!("cannot stop the terminal process: {err}"));
    }

    if let Some(container) = &session.container
        && let Err(err) = reap_container_session(container).await
    {
        failures.push(err.to_string());
    }

    drain_readers(session).await;

    if failures.is_empty() {
        tracing::debug!(session_id = %session.id, "closed terminal session");
        Ok(())
    } else {
        Err(anyhow!(
            "cannot close terminal session {}: {}; the session was kept so shell_close can \
             retry",
            session.id,
            failures.join("; ")
        ))
    }
}

/// Terminates the session's child by its OS process id. A native sandbox
/// child is a process-group leader, so its whole group is signalled; local
/// and Docker sessions signal only their direct child. The waiter still owns
/// the `Child` and reaps it.
async fn terminate_local(session: &Session) -> anyhow::Result<()> {
    if session.exited.load(Ordering::SeqCst) {
        return Ok(());
    }
    if session.pid <= 1 {
        bail!("refusing to signal invalid process id {}", session.pid);
    }
    let target = if session.kill_group {
        "process group"
    } else {
        "process"
    };

    match send_signal(session.pid, TERMINATE_SIGNAL, session.kill_group) {
        Ok(()) => {}
        Err(err) if is_missing_process(&err) => {
            mark_exited(session);
            return Ok(());
        }
        Err(err) => bail!("cannot send SIGTERM to {target} {}: {err}", session.pid),
    }
    if wait_for_exit(session, TERMINATE_WAIT).await {
        return Ok(());
    }

    match send_signal(session.pid, KILL_SIGNAL, session.kill_group) {
        Ok(()) => {}
        Err(err) if is_missing_process(&err) => {
            mark_exited(session);
            return Ok(());
        }
        Err(err) => bail!("cannot send SIGKILL to {target} {}: {err}", session.pid),
    }
    if wait_for_exit(session, KILL_WAIT).await {
        Ok(())
    } else {
        bail!(
            "{target} {} did not exit within {} second after SIGKILL",
            session.pid,
            KILL_WAIT.as_secs()
        )
    }
}

/// Sends one signal to `pid`, or to the process group led by `pid` when
/// `group` is true.
fn send_signal(pid: u32, signal: i32, group: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        let pid = libc::pid_t::try_from(pid).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "process id does not fit pid_t")
        })?;
        let target = if group { -pid } else { pid };
        // SAFETY: kill has no preconditions beyond a valid signal number and
        // target; both are checked values from this process's own child. A
        // negative target addresses the process group whose id is `pid`.
        if unsafe { libc::kill(target, signal) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, signal, group);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "terminating a terminal process is only implemented on Unix",
        ))
    }
}

/// Whether a signal failed because the process is already gone.
fn is_missing_process(err: &io::Error) -> bool {
    #[cfg(unix)]
    {
        err.raw_os_error() == Some(libc::ESRCH)
    }
    #[cfg(not(unix))]
    {
        err.kind() == io::ErrorKind::NotFound
    }
}

/// Waits a bounded time for both readers, then aborts what is left. A
/// background child can hold a pipe open after the shell exits.
async fn drain_readers(session: &Session) {
    let handles: Vec<JoinHandle<()>> = {
        let mut readers = session.readers.lock().await;
        readers.drain(..).collect()
    };
    let deadline = Instant::now() + READER_CLOSE_WAIT;
    for mut handle in handles {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || timeout(remaining, &mut handle).await.is_err() {
            handle.abort();
        }
    }
    session.readers_done.store(true, Ordering::SeqCst);
}

/// Closes a session whose readiness handshake failed and returns its output
/// (normally the proxy's stderr) for the error message.
async fn abort_with_details(session: &Session) -> String {
    let _ = close_session(session).await;
    let (bytes, _) = {
        let mut buffer = session.buffer.lock().await;
        buffer.take()
    };
    let text = String::from_utf8_lossy(&bytes);
    let text = text.trim();
    if text.is_empty() {
        " (no error output)".to_owned()
    } else {
        format!(": {text}")
    }
}

/// Removes the container-side process group and its pidfile.
async fn reap_container_session(container: &ContainerClose) -> anyhow::Result<()> {
    let pidfile = shell_quote(&container.pidfile);
    let script = format!(
        "f={pidfile}\n\
         [ -f \"$f\" ] || exit 3\n\
         p=$(cat \"$f\")\n\
         case \"$p\" in *[!0-9]*|'') exit 4;; esac\n\
         [ \"$p\" -gt 1 ] || exit 5\n\
         kill -TERM -\"$p\" 2>/dev/null || kill -TERM \"$p\" 2>/dev/null || true\n\
         sleep 0.2\n\
         kill -KILL -\"$p\" 2>/dev/null || kill -KILL \"$p\" 2>/dev/null || true\n\
         rm -f \"$f\"\n"
    );
    let mut command = Command::new("docker");
    command
        .arg("exec")
        .arg("-i")
        .arg(&container.name)
        .arg("sh")
        .arg("-c")
        .arg(script)
        .kill_on_drop(true);

    match timeout(CONTAINER_CLEANUP_TIMEOUT, command.output()).await {
        Ok(Ok(output)) if output.status.success() => Ok(()),
        Ok(Ok(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            match output.status.code() {
                // The pidfile is gone: the proxy never started a shell, or
                // an earlier close already removed it.
                Some(3) => Ok(()),
                Some(4) | Some(5) => Err(anyhow!(
                    "cannot clean up the terminal process in container {}: the pidfile {} is \
                     invalid; the session was kept so shell_close can retry",
                    container.name,
                    container.pidfile
                )),
                _ if container_is_gone(&stderr) => Ok(()),
                _ => Err(anyhow!(
                    "cannot clean up the terminal process in container {}: docker exec exited \
                     with {}: {}",
                    container.name,
                    output.status,
                    if stderr.is_empty() {
                        "(no error output)"
                    } else {
                        stderr.as_str()
                    }
                )),
            }
        }
        Ok(Err(err)) => Err(anyhow!(
            "cannot run docker to clean up the terminal process in container {}: {err}",
            container.name
        )),
        Err(_) => Err(anyhow!(
            "docker did not finish cleaning up the terminal process in container {} within \
             {} seconds",
            container.name,
            CONTAINER_CLEANUP_TIMEOUT.as_secs()
        )),
    }
}

/// Whether Docker reported that the container no longer exists or runs.
fn container_is_gone(stderr: &str) -> bool {
    let text = stderr.to_ascii_lowercase();
    text.contains("no such container") || text.contains("is not running")
}

/// Resolves the requested `cwd` to a path below the task base.
///
/// Only relative paths without a `..` component are accepted. In container
/// mode the result is a container path and no host metadata is consulted:
/// `cwd` is handed to the proxy, which calls `os.chdir` inside the
/// container. Native sandbox mode uses the real host work folder, so the
/// same host checks as local mode apply. Local mode additionally checks the
/// host directory and refuses a symbolic link that resolves outside the task
/// directory.
fn resolve_cwd(runtime: &TaskRuntime, requested: Option<&str>) -> anyhow::Result<PathBuf> {
    let relative = relative_cwd(requested)?;
    let normalized = if relative.as_os_str().is_empty() {
        runtime.base.clone()
    } else {
        runtime.base.join(&relative)
    };
    if !normalized.starts_with(&runtime.base) {
        bail!(
            "terminal cwd {} is outside the task directory",
            normalized.display()
        );
    }
    if runtime.is_container {
        return Ok(normalized);
    }

    let metadata = fs::metadata(&normalized).map_err(|err| {
        anyhow!(
            "terminal cwd {} cannot be used: {err}",
            normalized.display()
        )
    })?;
    if !metadata.is_dir() {
        bail!("terminal cwd {} is not a directory", normalized.display());
    }
    let canonical = fs::canonicalize(&normalized)?;
    let canonical_base = fs::canonicalize(&runtime.host_dir)?;
    if !canonical.starts_with(&canonical_base) {
        bail!(
            "terminal cwd {} resolves outside the task directory",
            normalized.display()
        );
    }
    Ok(normalized)
}

/// Validates and normalizes a relative `cwd`, without touching the
/// filesystem. `None` and the empty string mean the task root.
fn relative_cwd(requested: Option<&str>) -> anyhow::Result<PathBuf> {
    let Some(text) = requested.map(str::trim).filter(|text| !text.is_empty()) else {
        return Ok(PathBuf::new());
    };
    let mut relative = PathBuf::new();
    for component in Path::new(text).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => relative.push(part),
            Component::ParentDir => {
                bail!("terminal cwd must not contain a '..' component: {text}");
            }
            Component::RootDir | Component::Prefix(_) => {
                bail!("terminal cwd must be relative, not absolute: {text}");
            }
        }
    }
    Ok(relative)
}

/// Quotes `text` as one shell word.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_buffer_drops_old_bytes_at_the_cap_and_keeps_the_cursor_valid() {
        let mut buffer = Buffer::default();
        buffer.append(&vec![b'a'; BUFFER_CAP]);
        assert_eq!(buffer.unread_len(), BUFFER_CAP);
        let (first, dropped) = buffer.take();
        assert_eq!(first.len(), BUFFER_CAP);
        assert_eq!(dropped, 0);
        assert_eq!(buffer.unread_len(), 0);

        buffer.append(&vec![b'b'; 1024]);
        assert_eq!(buffer.unread_len(), 1024);
        buffer.append(&vec![b'c'; BUFFER_CAP]);
        assert_eq!(buffer.bytes.len(), BUFFER_CAP);
        assert_eq!(buffer.unread_len(), BUFFER_CAP);

        let (output, dropped) = buffer.take();
        assert_eq!(output.len(), BUFFER_CAP);
        assert!(dropped > 0, "the unread b bytes were evicted");
        assert!(buffer.bytes.ends_with(&vec![b'c'; 1024]));
        assert_eq!(output, buffer.bytes.clone(), "the newest bytes are kept");
        assert_eq!(buffer.unread_len(), 0);
    }

    #[test]
    fn an_oversized_chunk_counts_only_unread_dropped_bytes() {
        let mut buffer = Buffer::default();
        buffer.append(&vec![b'x'; BUFFER_CAP + 4096]);

        assert_eq!(buffer.bytes.len(), BUFFER_CAP);
        assert_eq!(buffer.unread_len(), BUFFER_CAP);
        let (output, dropped) = buffer.take();
        assert_eq!(output.len(), BUFFER_CAP);
        assert_eq!(dropped, 4096);
    }

    #[test]
    fn dropping_already_read_bytes_is_not_truncation() {
        let mut buffer = Buffer::default();
        buffer.append(&vec![b'a'; BUFFER_CAP]);
        let _ = buffer.take();

        buffer.append(&vec![b'b'; 1024]);
        let (output, dropped) = buffer.take();
        assert_eq!(output, vec![b'b'; 1024]);
        assert_eq!(dropped, 0, "only previously returned bytes were evicted");
    }

    #[test]
    fn cwd_accepts_relative_paths_without_parent_components() {
        assert_eq!(relative_cwd(None).unwrap(), PathBuf::new());
        assert_eq!(relative_cwd(Some("")).unwrap(), PathBuf::new());
        assert_eq!(relative_cwd(Some(".")).unwrap(), PathBuf::new());
        assert_eq!(relative_cwd(Some("sub")).unwrap(), PathBuf::from("sub"));
        assert_eq!(relative_cwd(Some("./a//b")).unwrap(), PathBuf::from("a/b"));
    }

    #[test]
    fn cwd_rejects_absolute_paths_and_parent_components() {
        for refused in ["/etc", "/", "../x", "a/../b", "a/..", ".."] {
            assert!(
                relative_cwd(Some(refused)).is_err(),
                "{refused} must be refused"
            );
        }
    }

    #[test]
    fn shell_quoting_keeps_one_word() {
        assert_eq!(shell_quote("/tmp/dhi-pty/a.pid"), "'/tmp/dhi-pty/a.pid'");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn the_readiness_marker_is_found_across_a_split() {
        let mut split = b"prefix\x1eDHI".to_vec();
        split.extend_from_slice(b"_PTY_READY\x1e\nsuffix");
        assert_eq!(find_bytes(&split, READY_MARKER), Some(6));
        assert_eq!(find_bytes(b"nothing here", READY_MARKER), None);
    }
}
