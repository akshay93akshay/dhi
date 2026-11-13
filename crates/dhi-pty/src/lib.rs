//! Persistent terminal tools for DHI.
//!
//! `dhi-pty` adds a long-lived shell (or a REPL started from it) next to the
//! one-shot `shell` coding tool in [`dhi_exec`]. The model opens a session,
//! sends lines to it, reads new output, and closes it:
//!
//! - `shell_open` starts a terminal and returns `{session_id, cwd}`;
//! - `shell_send` writes text, adding a newline when it is missing;
//! - `shell_read` returns only output produced since the previous read;
//! - `shell_close` stops the process and forgets the session;
//! - `shell_list` lists the sessions belonging to one task.
//!
//! A task may have at most [`MAX_SESSIONS_PER_TASK`] sessions, `shell_send`
//! is capped at [`MAX_SEND_BYTES`], and `shell_read` reports bytes discarded
//! by the bounded output buffer.
//!
//! # Container boundary
//!
//! The persistent terminal runs **inside** the task container, with the same
//! boundary as [`dhi_exec`]. [`PtyTools`] calls
//! [`dhi_exec::TaskContainers::ensure`]
//! and the session runs
//!
//! ```text
//! docker exec -i dhi-task-<task-id> python3 -u -c <proxy> <cwd> <pidfile>
//! ```
//!
//! where the Python proxy allocates a PTY inside the container with
//! `pty.fork()` and forwards bytes between that PTY and the exec stdin and
//! stdout. The proxy is used instead of `docker exec -t` because the host
//! side has no terminal. The task folder is the only read-write mount, and
//! the read-only `.dhi` and `.dhi/spill` mounts, dropped capabilities,
//! `no-new-privileges`, process, memory, and CPU limits, and the network
//! choice all come from [`dhi_exec::ContainerConfig`]; this crate does not
//! weaken them.
//!
//! The pidfile is `/tmp/dhi-pty/<session-id>.pid` inside the container's
//! writable tmpfs. Nothing is created below the host task directory, and no
//! host path is chmodded. The proxy writes the **shell** PID, which is also
//! the process-group id made by `pty.fork`. On close the manager reads the
//! pidfile strictly (decimal digits, greater than one) and runs
//! `kill -TERM -<pid>`, then `kill -KILL -<pid>` after a short grace period,
//! then removes the pidfile. If the container is gone or the pidfile is
//! missing the session is already dead and is forgotten. Any other cleanup
//! failure keeps the session so `shell_close` can retry it.
//!
//! `shell_open` waits for a unique readiness line from the proxy before it
//! returns a session id, so a missing `python3`, a bad `cwd`, or a broken
//! `/bin/sh` is an open error with the proxy's stderr, not a dead session.
//!
//! `cwd` must be relative and must not contain `..`; it defaults to the
//! runtime's base directory, which is `/work` in Docker mode and the real task
//! work folder in native sandbox mode. The proxy resolves it inside the
//! container with `os.chdir`, so container mode never inspects a host
//! directory.
//!
//! # Local test mode
//!
//! When [`dhi_exec::TaskRuntime::is_container`] is false the session is a local
//! `/bin/sh` with piped stdin, stdout, and stderr, started in the task
//! directory with only `PATH`, `HOME`, `TMPDIR`, `TERM=dumb`, and
//! `NO_COLOR=1`. There is no Python proxy, PTY, or pidfile. This mode exists
//! for tests; the host is still reachable.
//!
//! # Guard
//!
//! [`PtyTools`] holds a private random 32-byte secret and stamps
//! `HMAC-SHA256(secret, task_id)` into every call, together with the task id
//! from [`dhi_agent::RunScope`]. Only its own [`PtyTools::backend`] accepts
//! that MAC, and only for the same task id. The session manager keeps the
//! task id with each session, so task B cannot send to, read, or close task
//! A's session even if it knows the session id.
//!
//! Production uses only [`PtyTools`], [`PtyBackend`], [`PtyTools::close_task`]
//! and [`PtyTools::shutdown`]; the underlying session manager is not public
//! and offers no MAC-free path.
//!
//! # Shutdown
//!
//! Production must call [`PtyTools::shutdown`] to close every session and
//! refuse new ones; there is no async `Drop`, and dropping a manager without
//! it leaves its sessions running. It does not stop the task containers;
//! [`dhi_exec::TaskContainers::shutdown`] owns that.

mod backend;
mod proxy;
mod session;
mod tools;

pub use backend::PtyBackend;
pub use dhi_exec::ToolDefinition;
pub use session::{
    DEFAULT_READ_TIMEOUT_MS, MAX_READ_TIMEOUT_MS, MAX_SEND_BYTES, MAX_SESSIONS_PER_TASK, OpenReply,
    ReadReply, SessionInfo,
};
pub use tools::{PtyTools, SCOPE_MAC_FIELD, TASK_ID_FIELD, tool_definitions};
