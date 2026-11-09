//! Per-task coding tools for DHI.
//!
//! `dhi-exec` is a thin adapter over [`rig_tools`]: the tools themselves
//! (`read`, `write`, `edit`, `search`, `list`, `shell`) come from that crate.
//! This crate adds what DHI needs:
//!
//! - one container per task ([`TaskContainers`]);
//! - the [`CodingTools`] facade that stamps the trusted task id and a per-task
//!   MAC into every guarded request;
//! - guarded dynamic tools registered on a Rig server
//!   ([`CodingTools::register_tools`]);
//! - a [`dhi_guard::ToolBackend`] that runs the `rig-tools` implementation
//!   for that task ([`CodingBackend`]).
//!
//! # Design B: one container per task
//!
//! Every task gets its own Docker container, named `dhi-task-<task-id>`. In
//! legacy mode the only host paths visible in it are the task's own folder,
//! `root/<task-id>`, mounted at `/work` read-write, and its private `.dhi`
//! folder and `.dhi/spill` folder, mounted at `/work/.dhi` and
//! `/work/.dhi/spill` read-only. In project mode the task folder is
//! `<project>/.dhi/<task-id>` at `/work`, and the selected project root is
//! mounted read-only at `/project`. The nested read-only mounts are what keep
//! the container from replacing the spill folder, or the `.dhi` folder that
//! contains it, with a symbolic link and making the host spill writer write
//! elsewhere. The container runs read-only apart from `/work` and a tmpfs at
//! `/tmp`, drops all capabilities, sets `no-new-privileges`, limits memory,
//! memory-plus-swap, CPUs, open files, and process count, and runs as the
//! invoking user (on Unix). It has no Docker socket, no host root, no host
//! home, and no repository write mount, and no host environment secrets are
//! passed in.
//!
//! All coding tools run **inside** that container. The file tools and the
//! shell use a [`rig_tools::remote::Remote`] workspace whose transport is
//! `docker exec -i dhi-task-<task-id> sh -c`, so:
//!
//! - every file operation is a short `sh` script run in the container;
//! - `read`, `write`, `edit`, `search` and `list` resolve paths against
//!   `/work`;
//! - `shell` starts in `/work`, with `HOME=/work/.home`,
//!   `CARGO_HOME=/work/.cargo`, and `TMPDIR=/work/.tmp`. The temporary
//!   directory is under the read-write mount so tools that execute temporary
//!   binaries work; `/tmp` stays a `noexec` tmpfs for general use.
//!
//! The container is the boundary: the host is not visible from it beyond the
//! mounted task folder. Output the tools cut is spilled on the host to
//! `root/<task-id>/.dhi/spill`, which the container sees read-only at
//! `/work/.dhi/spill`; the path shown to the model is translated from the
//! host path so the model can read it back inside the container.
//!
//! A `shell` call that times out, or that is killed for producing too much
//! output, restarts the task container after its output is captured. That
//! kills every process the command left in the old container. A background
//! process started by a command that exits normally can still survive until
//! the next restart or until the task is removed.
//!
//! Containers carry `dhi.managed`, `dhi.task`, `dhi.root`, and `dhi.config`
//! labels. A running container is reused only when all four match. A
//! container with the derived name that is not managed for this task, or that
//! belongs to a different root, is refused and left untouched; a managed
//! container whose configuration changed is replaced.
//!
//! # Guard
//!
//! The guard stays outside the container and records every call. The task id
//! comes from [`dhi_agent::RunScope`], which the runtime stamps into the
//! dispatch context. [`CodingTools`] holds a private random secret and stamps
//! `HMAC-SHA256(secret, task_id)`; only its own [`CodingTools::backend`]
//! accepts that MAC, and only for the same task id. The secret has no public
//! accessor and is never shown to the model. A coding tool registered through
//! the generic [`dhi_agent::guarded_dynamic_tool`] path does not carry a MAC
//! and is refused before it runs.
//!
//! # Images and network
//!
//! The image must contain the runtimes a task needs. The default,
//! `python:3.12-slim`, has Python; a task that needs another language or
//! toolchain must configure a different image with `DHI_EXEC_IMAGE`.
//! Network is disabled by default (`--network none`); a task that needs the
//! network must opt in with `DHI_EXEC_NETWORK`. Memory, CPU, and process
//! limits are configurable through [`ContainerConfig`] or the matching
//! `DHI_EXEC_*` environment variables.
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
//! private host layout but uses [`rig_tools::workspace::Local`] instead of
//! Docker. In that mode the host is still reachable and the file-tool path
//! checks and symlink checks are the control; `shell` is not an OS sandbox.

mod backend;
mod container;
mod tools;

pub use backend::CodingBackend;
pub use container::{ContainerConfig, TaskContainers, TaskRuntime, prepare_project_task_dir};
pub use tools::{CodingTools, SCOPE_MAC_FIELD, TASK_ID_FIELD, ToolDefinition, tool_definitions};
