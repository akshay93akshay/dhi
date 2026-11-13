//! Per-task coding tools for DHI.
//!
//! `dhi-exec` is a thin adapter over [`rig_tools`]: the tools themselves
//! (`read`, `write`, `edit`, `search`, `list`, `shell`) come from that crate.
//! This crate adds what DHI needs:
//!
//! - one native sandbox runtime per task ([`TaskContainers`]);
//! - the [`CodingTools`] facade that stamps the trusted task id and a per-task
//!   MAC into every guarded request;
//! - guarded dynamic tools registered on a Rig server
//!   ([`CodingTools::register_tools`]);
//! - a [`dhi_guard::ToolBackend`] that runs the `rig-tools` implementation
//!   for that task ([`CodingBackend`]).
//!
//! # One runtime per task
//!
//! Every task gets its own runtime under a task root. In native sandbox mode
//! a task is the same macOS user as the server, confined by
//! `/usr/bin/sandbox-exec` with the configured profile. Its files live at
//! real host paths: `root/<task-id>` in legacy mode, or
//! `<project>/.dhi/<task-id>` with the real project root in project mode.
//! There are no `/work` or `/project` aliases, no `synthetic.conf`, no
//! dedicated task user, no ACLs, and no `sudo`, so nothing has to be
//! installed and no reboot is needed.
//!
//! The coding tools act on those real host paths. The file tools use
//! [`rig_tools::workspace::Local`] against the task's work directory, and a
//! read of the task's real project root is allowed for a project task.
//! `shell` starts in the work directory, with `HOME=<work>/.home`,
//! `CARGO_HOME=<work>/.cargo`, and `TMPDIR=<work>/.tmp`, behind the
//! `sandbox-exec` wrapper.
//!
//! The profile is the boundary: it must let a task read and write its own
//! work directory, read its project root without seeing `<project>/.dhi`,
//! read but not write its spill directory, and reach nothing else. Output the
//! tools cut is spilled on the host to `<work>/.dhi/spill`.
//!
//! A `shell` call that times out, or that is killed for producing too much
//! output, restarts the task runtime after its output is captured. That kills
//! every process group the old runtime recorded. A background process started
//! by a command that exits normally can still survive until the next restart
//! or until the task is removed.
//!
//! # Guard
//!
//! The guard stays outside the sandbox and records every call. The task id
//! comes from [`dhi_agent::RunScope`], which the runtime stamps into the
//! dispatch context. [`CodingTools`] holds a private random secret and stamps
//! `HMAC-SHA256(secret, task_id)`; only its own [`CodingTools::backend`]
//! accepts that MAC, and only for the same task id. The secret has no public
//! accessor and is never shown to the model. A coding tool registered through
//! the generic [`dhi_agent::guarded_dynamic_tool`] path does not carry a MAC
//! and is refused before it runs.
//!
//! # Shutdown
//!
//! Production must call [`TaskContainers::shutdown`] when the manager is no
//! longer used; there is deliberately no async `Drop`. A failed removal keeps
//! the cached runtime so `shutdown` can retry it, and `shutdown` reports the
//! failure instead of claiming success.
//!
//! # Local test mode
//!
//! [`TaskContainers::local`] exists only for tests. It creates the same
//! private host layout and runs the tools with
//! [`rig_tools::workspace::Local`] and no `sandbox-exec`. In that mode the
//! host is still reachable and the file-tool path checks and symlink checks
//! are the control; `shell` is not an OS sandbox.
//!
//! # Native macOS sandbox
//!
//! [`TaskContainers::sandbox`] is the production mode. Because each task names
//! its own paths, any number of sandbox tasks can run at the same time.
//!
//! Every sandboxed command is wrapped to record its session-leader pid in a
//! server-owned pidfile below `root/pids`. [`TaskContainers::remove`] kills
//! each verified pidfile group and removes the file, and
//! [`TaskContainers::reap_orphans`] kills every leftover pidfile group at
//! startup, when no task is running. This reaches Apple platform binaries,
//! whose environment `ps` hides. A group is killed only when its leader's
//! command line still names `dhi-pids` and the pidfile, so a reused pid is
//! left alone. `DHI_TASK_ID=<task-id>` is carried for diagnosis only and is
//! not a cleanup mechanism; a child that deliberately calls `setsid` leaves
//! the group and can survive.

mod backend;
mod container;
mod tools;

pub use backend::CodingBackend;
pub use container::{
    SandboxConfig, SandboxRuntime, TaskContainers, TaskRuntime, prepare_project_task_dir,
    sandbox_prefix,
};
pub use tools::{CodingTools, SCOPE_MAC_FIELD, TASK_ID_FIELD, ToolDefinition, tool_definitions};
