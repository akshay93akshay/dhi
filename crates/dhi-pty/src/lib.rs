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
//! # Sandbox boundary
//!
//! The persistent terminal runs inside the task's own runtime, with the same
//! boundary as [`dhi_exec`]. [`PtyTools`] calls
//! [`dhi_exec::TaskContainers::ensure`] and the session runs as one
//! `/bin/sh` with piped stdin, stdout, and stderr.
//!
//! In native macOS sandbox mode the shell starts as the same macOS user as
//! the server through
//! `/usr/bin/env ... /usr/bin/sandbox-exec -f <profile> ...`, with the task's
//! real host paths, the same wrapper [`dhi_exec::sandbox_prefix`] builds for
//! the `shell` coding tool. The child calls `setsid`, so the wrapper leads a
//! process group; close signals that whole group with `kill -TERM -<pid>` and,
//! after a short grace period, `kill -KILL -<pid>`, which stops
//! `sandbox-exec`, the shell, and any jobs it started. The profile, not this
//! crate, is the control that confines the terminal.
//!
//! `cwd` must be relative and must not contain `..`; it defaults to the
//! runtime's base directory, which is the real task work folder. The host
//! directory is checked before the shell starts, so a missing directory or a
//! link out of the task directory is an open error.
//!
//! # Local test mode
//!
//! A runtime without a sandbox is a local `/bin/sh` with piped stdin, stdout,
//! and stderr, started in the task directory with only `PATH`, `HOME`,
//! `TMPDIR`, `TERM=dumb`, and `NO_COLOR=1`. It is not an OS sandbox. This mode
//! exists for tests; the host is still reachable.
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
//! it leaves its sessions running. It does not stop the task runtimes;
//! [`dhi_exec::TaskContainers::shutdown`] owns that.

mod backend;
mod session;
mod tools;

pub use backend::PtyBackend;
pub use dhi_exec::ToolDefinition;
pub use session::{
    DEFAULT_READ_TIMEOUT_MS, MAX_READ_TIMEOUT_MS, MAX_SEND_BYTES, MAX_SESSIONS_PER_TASK, OpenReply,
    ReadReply, SessionInfo,
};
pub use tools::{PtyTools, SCOPE_MAC_FIELD, TASK_ID_FIELD, tool_definitions};
