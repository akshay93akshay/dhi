//! One container per task, and the runtime the coding tools act on.
//!
//! [`TaskContainers::new`] is the production mode: every task gets its own
//! Docker container, named `dhi-task-<task-id>`, with only that task's host
//! directory mounted at `/work`. The task's `.dhi` directory and its spill
//! subdirectory are mounted again at `/work/.dhi` and `/work/.dhi/spill`
//! read-only, so the container can read spilled output but cannot replace
//! the directory the host spill writer uses. Every file operation and every
//! shell command goes through a
//! [`rig_tools::remote::Remote`] whose transport is
//! `docker exec -i dhi-task-<task-id> sh -c`, so the tools see container
//! paths and the container is the boundary. The trailing `sh -c` is needed
//! because `Remote` appends a shell line as one argument, and `docker exec`
//! executes its arguments as an argv rather than joining them into a remote
//! shell command the way `ssh` does.
//!
//! Containers this manager starts carry `dhi.managed`, `dhi.task`,
//! `dhi.root`, and `dhi.config` labels. They are the ownership check:
//! [`ensure`](TaskContainers::ensure) reuses only a running container whose
//! labels all match, refuses a container that is not managed for the task or
//! belongs to another root, and replaces a managed one whose configuration
//! changed. [`remove`](TaskContainers::remove) refuses to force-remove any
//! container that is not marked as managed for that task.
//!
//! [`TaskContainers::local`] is for tests only. It creates the same host
//! layout and uses [`Local`], so no Docker daemon is needed; there the host
//! is still reachable and the path checks in the backend are the control.
//!
//! A task's host directory is `root/<task-id>`. On Unix the root, the task
//! directory, and its `.home`, `.tmp`, `.cargo` and `.dhi/spill` directories
//! are created with mode `0700`; each is checked to be a real directory and
//! not a symbolic link before it is used. Production must call
//! [`shutdown`](TaskContainers::shutdown); there is no async `Drop`, and a
//! failed removal leaves its cache entry for a later `shutdown` to retry.
//!
//! When the server itself runs in a container and reaches the host Docker
//! daemon through its socket, the task root is a container path such as
//! `/app/workspace`, while the daemon resolves bind-mount sources on the
//! host. [`TaskContainers::with_host_root`] records the host path that
//! corresponds to the task root: `docker run` mounts use it, while directory
//! creation and checks, [`TaskRuntime::host_dir`], spill paths, cleanup, and
//! `docker exec` keep using the task root. Without a host root the task root
//! is used as before, which is only correct when the server sees the host
//! filesystem directly.
//!
//! [`TaskContainers::sandbox`] is the native macOS mode. No Docker daemon is
//! needed: each task runs as the same macOS user as this process under
//! `/usr/bin/sandbox-exec`, with its own real host paths. There are no `/work`
//! or `/project` aliases, no `synthetic.conf`, no dedicated task user, no
//! ACLs, and no `sudo`, so nothing has to be installed and no reboot is
//! needed. Because no path is global, any number of sandbox tasks can run at
//! the same time.
//!
//! Every sandboxed command is wrapped so that its session leader records its
//! pid in a server-owned pidfile below `root/pids` and stays alive while the
//! command runs. Task end kills each recorded process group whose leader
//! command line names that pidfile, and startup reaps every leftover pidfile
//! group the same way. A leader that is gone is still killed through its group
//! id while any member remains, because a group id is not reused while the
//! group is non-empty. A child that calls `setsid` leaves the group and is not
//! caught. Every command also carries `DHI_TASK_ID=<task-id>`, which is
//! diagnostic only; cleanup is pidfile-only, because matching the value in a
//! command line would hit unrelated processes that merely mention it.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command as StdCommand, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{anyhow, bail};
use rig_tools::process::Launcher;
use rig_tools::remote::Remote;
use rig_tools::workspace::{Local, Workspace};
use rig_tools::{Seen, Spill};
use sha2::{Digest, Sha256};
use tokio::process::Command;
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::{Instant, sleep};
use uuid::Uuid;

/// The search path a sandboxed task gets when the parent has none.
pub(crate) const DEFAULT_PATH: &str = "/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:/usr/local/sbin:/usr/bin:/bin:/usr/sbin:/sbin";

/// Where the one mounted task folder appears inside a container.
const CONTAINER_WORK: &str = "/work";

/// Where the selected project root is mounted read-only in project mode.
const CONTAINER_PROJECT: &str = "/project";

/// Empty read-only tmpfs that hides `<project>/.dhi` in project mode, so one
/// task can never see another task's folder through the project mount.
const PROJECT_DHI_TMPFS: &str = "/project/.dhi:ro,noexec,nosuid,size=64k";

/// Empty read-only tmpfs mounts that hide image-only paths from every task
/// process. The task data plane is `/project` and `/work`; these paths are
/// part of the task image, not task data. The Gitpod toolchain directories
/// stay visible because they hold the Node, Cargo, and Python tools. The
/// Homebrew source directory is hidden because it contains an unrelated
/// `AGENTS.md`; Homebrew packages and the Cellar stay visible, but the `brew`
/// symlink into the source directory is gone.
const HIDDEN_IMAGE_TMPFS: [&str; 4] = [
    "/home/linuxbrew/.linuxbrew/Homebrew:ro,noexec,nosuid,size=64k",
    "/root:ro,noexec,nosuid,size=64k",
    "/workspace:ro,noexec,nosuid,size=64k",
    "/opt:ro,noexec,nosuid,size=64k",
];

/// Where the task's spill directory is mounted read-only inside a container.
const CONTAINER_SPILL: &str = "/work/.dhi/spill";

/// The task's `.dhi` directory in a container. It is mounted read-only too,
/// so the container cannot rename the directory that contains the spill
/// mount and replace it with a symbolic link.
const CONTAINER_DHI: &str = "/work/.dhi";

/// The label every container this manager starts carries.
const MANAGED_LABEL: &str = "dhi.managed";

/// The label naming the task a managed container belongs to.
const TASK_LABEL: &str = "dhi.task";

/// The label holding the hash of the canonical task root.
const ROOT_LABEL: &str = "dhi.root";

/// The label holding the hash of the configuration a container was started
/// with.
const CONFIG_LABEL: &str = "dhi.config";

/// The prefix of every container this manager starts.
const CONTAINER_PREFIX: &str = "dhi-task-";

/// How long a new container gets to report that it is running.
const START_TIMEOUT: Duration = Duration::from_secs(15);

/// How long `docker run` may take, including pulling the image.
const RUN_TIMEOUT: Duration = Duration::from_secs(180);

/// How long one `docker inspect` may take.
const INSPECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long one `docker rm -f` may take.
const REMOVE_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the startup loop asks whether the container runs.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// How many pidfiles one native sandbox reap processes.
///
/// Every pidfile costs a `ps` call per recorded pid, and startup reaps before
/// the listener binds, so a directory full of stale pidfiles must not delay
/// serving. Past this many, the rest are counted and reported and a later
/// `shutdown` or the next startup retries them.
const REAP_PIDFILE_LIMIT: usize = 256;

/// How a task container is started.
///
/// The defaults are deliberately small and offline. The image must contain
/// the runtimes a task needs: the default has Python, a task that needs
/// something else must configure another image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerConfig {
    /// The image to run.
    pub image: String,
    /// The Docker network mode; `none` has no network at all.
    pub network: String,
    /// The memory limit passed to Docker.
    pub memory: String,
    /// The CPU limit passed to Docker.
    pub cpus: String,
    /// The process limit passed to Docker.
    pub pids_limit: u32,
}

impl Default for ContainerConfig {
    /// The defaults, overridden by `DHI_EXEC_IMAGE`, `DHI_EXEC_NETWORK`,
    /// `DHI_EXEC_MEMORY`, `DHI_EXEC_CPUS` and `DHI_EXEC_PIDS_LIMIT` when
    /// those are set to a non-empty value.
    fn default() -> Self {
        Self {
            image: env_or("DHI_EXEC_IMAGE", "python:3.12-slim"),
            network: env_or("DHI_EXEC_NETWORK", "none"),
            memory: env_or("DHI_EXEC_MEMORY", "2g"),
            cpus: env_or("DHI_EXEC_CPUS", "2"),
            pids_limit: std::env::var("DHI_EXEC_PIDS_LIMIT")
                .ok()
                .and_then(|value| value.trim().parse().ok())
                .filter(|limit| *limit > 0)
                .unwrap_or(512),
        }
    }
}

/// A non-empty environment value, or `fallback`.
fn env_or(name: &str, fallback: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}

/// How a native macOS sandbox is started.
///
/// The profile is passed to `/usr/bin/sandbox-exec -f`. The task runs as the
/// same macOS user as this process and uses its own real host paths, so the
/// profile is the only configuration.
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    /// The sandbox profile passed to `sandbox-exec -f`.
    pub profile: PathBuf,
}

/// The host paths and settings of one running native sandbox task.
///
/// [`TaskRuntime::sandbox`] carries this so the shell launcher can rebuild
/// the same `sandbox-exec` wrapper without trusting a task path.
#[derive(Debug, Clone)]
pub struct SandboxRuntime {
    /// The sandbox profile passed to `sandbox-exec -f`.
    pub profile: PathBuf,
    /// The task the sandboxed processes belong to. It is exported as
    /// `DHI_TASK_ID` for diagnosis; cleanup does not match it.
    pub task_id: Uuid,
    /// The task's real host work directory.
    pub host_work: PathBuf,
    /// The task's real project root, when the task has one.
    pub host_project: Option<PathBuf>,
    /// The server-owned file where the wrapper records the session-leader pid
    /// of every command the task runs. Task end kills those process groups
    /// after checking the leader command line.
    pub pidfile: PathBuf,
}

/// Everything the backend needs to run one task's tools.
///
/// In container mode [`base`](Self::base) is `/work`, the workspace is a
/// [`Remote`] over `docker exec`, and [`host_dir`](Self::host_dir) is the
/// one host directory mounted there. In native sandbox mode the base is the
/// task's real host work directory, the workspace is [`Local`], and the shell
/// launcher wraps every command in `sandbox-exec`. In local test mode the base
/// is the host task directory, the workspace is [`Local`], and the host is
/// still reachable.
#[derive(Debug, Clone)]
pub struct TaskRuntime {
    /// The task's directory on the host: `root/<task-id>` in legacy mode, or
    /// `<project>/.dhi/<task-id>` in project mode.
    pub host_dir: PathBuf,
    /// The directory the tools resolve paths against: `/work` in container
    /// mode, the real host task directory in native sandbox and local mode.
    pub base: PathBuf,
    /// Where every file operation and shell command runs.
    pub workspace: Arc<dyn Workspace>,
    /// The project root as the tools name it: `/project` in container mode,
    /// the real host project directory in native sandbox and local mode.
    /// `None` is legacy mode.
    pub project_dir: Option<PathBuf>,
    /// The [`Seen`] shared by this task's reads, writes and edits.
    pub seen: Seen,
    /// Where this task's cut output is kept.
    pub spill: Spill,
    /// Whether the tools run inside a Docker container (`true`) or directly
    /// on the host, as in native sandbox and local mode (`false`).
    pub is_container: bool,
    /// The native macOS sandbox this runtime runs under, when it is one.
    pub sandbox: Option<SandboxRuntime>,
}

/// Which kind of runtime this manager builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// One Docker container per task.
    Docker,
    /// One native macOS sandbox per task.
    Sandbox,
    /// The host, for tests only.
    Local,
}

/// Creates, caches and removes the per-task runtimes.
///
/// One instance owns the task root and the container configuration. Every
/// task gets at most one runtime and at most one running container; the
/// runtime is cached until [`remove`](Self::remove) or
/// [`shutdown`](Self::shutdown).
pub struct TaskContainers {
    root: PathBuf,
    /// The host path of `root` for Docker bind mounts, when the server does
    /// not see the host filesystem directly.
    host_root: Option<PathBuf>,
    config: ContainerConfig,
    /// The SHA-256 of the canonical root, in every container this manager
    /// starts and checks before it reuses or removes one.
    root_hash: String,
    /// The SHA-256 of the configuration that affects the container.
    config_hash: String,
    /// The `--user` value, if the platform has one.
    user: Option<String>,
    mode: Mode,
    /// The profile of native sandbox mode.
    sandbox: Option<SandboxConfig>,
    cache: Mutex<HashMap<Uuid, Arc<TaskRuntime>>>,
    /// One async mutex per task that serializes `ensure` and `remove`.
    starting: Mutex<HashMap<Uuid, Arc<AsyncMutex<()>>>>,
    /// Project folder per task. A task in this map gets
    /// `<project>/.dhi/<task-id>` as its host work directory and its real
    /// project root as the read-only project; a task without an entry keeps
    /// legacy mode.
    projects: Mutex<HashMap<Uuid, PathBuf>>,
    /// Set by `shutdown`; no new runtime starts after it.
    closed: AtomicBool,
}

impl TaskContainers {
    /// A manager of Docker task containers under `root`.
    ///
    /// The root itself is created when missing, made private on Unix, and
    /// checked not to be a symbolic link. No container starts until
    /// [`ensure`](Self::ensure) is called.
    pub fn new(root: impl Into<PathBuf>, config: ContainerConfig) -> io::Result<Self> {
        Self::build(root.into(), config, Mode::Docker, None)
    }

    /// A manager of native macOS sandbox tasks under `root`.
    ///
    /// No Docker daemon is used. Every task runs as the same macOS user as
    /// this process under `/usr/bin/sandbox-exec`, with its own real host
    /// paths. The profile must already exist; nothing else is installed.
    /// Because no path is global, any number of tasks may run at the same
    /// time.
    pub fn sandbox(root: impl Into<PathBuf>, config: SandboxConfig) -> io::Result<Self> {
        if !config.profile.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "sandbox profile {} does not exist",
                    config.profile.display()
                ),
            ));
        }
        Self::build(
            root.into(),
            ContainerConfig::default(),
            Mode::Sandbox,
            Some(config),
        )
    }

    /// A manager that uses [`Local`] instead of Docker. Tests only.
    ///
    /// It creates and checks the same host directory layout, so the file
    /// tools and shell run without a Docker daemon, but the host is not
    /// isolated.
    pub fn local(root: impl Into<PathBuf>) -> io::Result<Self> {
        Self::build(root.into(), ContainerConfig::default(), Mode::Local, None)
    }

    fn build(
        requested: PathBuf,
        config: ContainerConfig,
        mode: Mode,
        sandbox: Option<SandboxConfig>,
    ) -> io::Result<Self> {
        create_private_dir(&requested)?;
        let root = validate_private_dir(&requested, "task container root")?;
        let root_hash = sha256_hex(root.as_os_str().as_encoded_bytes());
        let user = container_user();
        let config_hash = config_hash(&config, user.as_deref(), &root);
        Ok(Self {
            root,
            host_root: None,
            config,
            root_hash,
            config_hash,
            user,
            mode,
            sandbox,
            cache: Mutex::new(HashMap::new()),
            starting: Mutex::new(HashMap::new()),
            projects: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
        })
    }

    /// Sets the host path that corresponds to the task root for Docker bind
    /// mounts.
    ///
    /// Use this when the server runs in a container and reaches the host
    /// Docker daemon through its socket: the daemon resolves `-v` sources on
    /// the host, not in this process. Only mount sources change; directory
    /// creation and checks, [`TaskRuntime::host_dir`], spill paths, cleanup,
    /// and `docker exec` keep using the task root. The path must be absolute
    /// and is not checked for existence because it is normally not visible
    /// from this process.
    ///
    /// Without this, bind sources are the task root, which is only correct
    /// when the server sees the host filesystem directly.
    pub fn with_host_root(mut self, host_root: impl Into<PathBuf>) -> io::Result<Self> {
        let host_root = host_root.into();
        if host_root.as_os_str().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the Docker host workspace root must not be empty",
            ));
        }
        if !host_root.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "the Docker host workspace root {} must be an absolute path",
                    host_root.display()
                ),
            ));
        }
        self.config_hash = config_hash(&self.config, self.user.as_deref(), &host_root);
        self.host_root = Some(host_root);
        Ok(self)
    }

    /// The canonical root every task directory is below.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Selects project mode for `task_id`, or clears it when `project` is
    /// `None`.
    ///
    /// The folder must be an absolute real directory and is canonicalized
    /// here. A project task's host work directory is
    /// `<project>/.dhi/<task-id>`; its container also mounts `<project>` at
    /// `/project` read-only. The mapping is set before the task's first tool
    /// call and is immutable for the task's life.
    pub fn set_task_project(&self, task_id: Uuid, project: Option<&Path>) -> io::Result<()> {
        let mut projects = self.projects.lock().unwrap_or_else(PoisonError::into_inner);
        match project {
            None => {
                projects.remove(&task_id);
                Ok(())
            }
            Some(project) => {
                if !project.is_absolute() {
                    // Keep the task in project mode even on this failure, so
                    // a later tool call cannot silently fall back to legacy
                    // workspace mode.
                    projects.insert(task_id, project.to_path_buf());
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "project folder {} must be an absolute path",
                            project.display()
                        ),
                    ));
                }
                let metadata = fs::symlink_metadata(project).map_err(|err| {
                    projects.insert(task_id, project.to_path_buf());
                    io::Error::new(
                        err.kind(),
                        format!(
                            "project folder {} cannot be inspected: {err}",
                            project.display()
                        ),
                    )
                })?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    projects.insert(task_id, project.to_path_buf());
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "project folder {} is not a real directory",
                            project.display()
                        ),
                    ));
                }
                let canonical = fs::canonicalize(project).map_err(|err| {
                    projects.insert(task_id, project.to_path_buf());
                    io::Error::new(
                        err.kind(),
                        format!(
                            "project folder {} cannot be resolved: {err}",
                            project.display()
                        ),
                    )
                })?;
                projects.insert(task_id, canonical);
                Ok(())
            }
        }
    }

    fn task_project(&self, task_id: Uuid) -> Option<PathBuf> {
        self.projects
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&task_id)
            .cloned()
    }

    /// Refuses a project read whose real target is outside the project or
    /// inside the hidden `.dhi` directory.
    ///
    /// In Docker mode this runs `realpath -e` inside the task container,
    /// because `rig-tools`' remote workspace canonicalizes `..` lexically and
    /// does not resolve symbolic links. In local and native sandbox mode
    /// `fs::canonicalize` does the same job on the host path. A missing path
    /// is left to the tool so it returns its normal not-found error. Paths
    /// outside the project are ignored.
    pub async fn verify_project_read(&self, task_id: Uuid, requested: &str) -> anyhow::Result<()> {
        if self.task_project(task_id).is_none() {
            return Ok(());
        }
        match self.mode {
            Mode::Local | Mode::Sandbox => self.verify_local_project_read(task_id, requested),
            Mode::Docker => {
                if project_request_relative(requested).is_none() {
                    return Ok(());
                }
                self.verify_docker_project_read(task_id, requested).await
            }
        }
    }

    /// The project read check for local and native sandbox mode.
    ///
    /// The task's own work directory `<project>/.dhi/<task-id>` is allowed
    /// first: it is the absolute path the task is told to use, and it lies
    /// below the hidden project `.dhi`. Every other path is resolved like the
    /// file tools resolve it (its parent is canonically resolved, a missing
    /// file is kept as its name) and must stay inside the project and out of
    /// `<project>/.dhi`. Other tasks' `.dhi` directories and the project
    /// `.dhi` root stay refused.
    fn verify_local_project_read(&self, task_id: Uuid, requested: &str) -> anyhow::Result<()> {
        let Some(project) = self.task_project(task_id) else {
            return Ok(());
        };
        let Ok(root) = fs::canonicalize(&project) else {
            // The project disappeared after task creation; the tool will
            // report the same missing path.
            return Ok(());
        };
        let work = self
            .cached(task_id)
            .and_then(|runtime| runtime.host_dir.canonicalize().ok())
            .or_else(|| fs::canonicalize(root.join(".dhi").join(task_id.to_string())).ok());
        if let Some(work) = work {
            // A relative path is resolved against the work directory by the
            // file tools, so it is resolved the same way here.
            let requested_path = Path::new(requested);
            let candidate = if requested_path.is_absolute() {
                requested_path.to_path_buf()
            } else {
                work.join(requested_path)
            };
            let resolved = resolve_like_tool(&candidate);
            if resolved.starts_with(&work) && !resolved.starts_with(work.join(".dhi")) {
                return Ok(());
            }
        }
        // `/project/...` is the container name of the project root; an
        // absolute host path below the real root names the same entry.
        let relative = match project_request_relative(requested) {
            Some(relative) => PathBuf::from(relative),
            None => {
                let requested = Path::new(requested);
                if !requested.is_absolute() {
                    return Ok(());
                }
                let Ok(relative) = requested.strip_prefix(&root) else {
                    return Ok(());
                };
                relative.to_path_buf()
            }
        };
        let candidate = if relative.as_os_str().is_empty() {
            root.clone()
        } else {
            root.join(relative)
        };
        let resolved = resolve_like_tool(&candidate);
        if !resolved.starts_with(&root) {
            bail!(
                "refusing to read {requested}: its real path {} is outside the project {}",
                resolved.display(),
                root.display()
            );
        }
        let hidden = root.join(".dhi");
        if resolved == hidden || resolved.starts_with(&hidden) {
            bail!("refusing to read {requested}: the project .dhi directory is hidden");
        }
        Ok(())
    }

    async fn verify_docker_project_read(
        &self,
        task_id: Uuid,
        requested: &str,
    ) -> anyhow::Result<()> {
        let name = container_name(task_id);
        let mut command = Command::new("docker");
        command
            .arg("exec")
            .arg(&name)
            .arg("realpath")
            .arg("-e")
            .arg("--")
            .arg(requested);
        let output = run_docker(
            command,
            INSPECT_TIMEOUT,
            &format!("resolve {requested} in container {name}"),
        )
        .await?;
        if !output.status.success() {
            // A missing path is the tool's normal not-found error. Docker or
            // exec failures are also left to the tool's own path check.
            return Ok(());
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let resolved = stdout.trim();
        if resolved.is_empty() {
            return Ok(());
        }
        let resolved = Path::new(resolved);
        let root = Path::new(CONTAINER_PROJECT);
        if resolved != root && !resolved.starts_with(root) {
            bail!(
                "refusing to read {requested}: its real path {} is outside {CONTAINER_PROJECT}",
                resolved.display()
            );
        }
        let hidden = root.join(".dhi");
        if resolved == hidden.as_path() || resolved.starts_with(&hidden) {
            bail!("refusing to read {requested}: {CONTAINER_PROJECT}/.dhi is hidden");
        }
        Ok(())
    }

    /// The Docker container name for `task_id`.
    ///
    /// This is the name [`ensure`](Self::ensure) starts and checks. It is
    /// public so a sibling block that runs a process inside the same task
    /// container can address it without duplicating the naming rule.
    pub fn container_name(&self, task_id: Uuid) -> String {
        container_name(task_id)
    }

    /// The runtime of `task_id`, started and cached on first use.
    ///
    /// The host task directory is always created first. In Docker mode this
    /// starts the task container if it is not already running, waits until
    /// it reports running, and builds a [`Remote`] over `docker exec`. In
    /// native sandbox mode it validates the task's `.dhi` and spill
    /// directories and builds a [`Local`] runtime whose shell launcher wraps
    /// every command in `sandbox-exec`. A cached runtime is returned only
    /// while its container still reports running; a missing or stopped
    /// container is replaced, and a failed check is an error rather than a
    /// stale runtime.
    pub async fn ensure(&self, task_id: Uuid) -> anyhow::Result<Arc<TaskRuntime>> {
        if self.closed.load(Ordering::SeqCst) {
            bail!("task containers are shut down; no new runtime may start");
        }
        if let Some(runtime) = self.live_cached(task_id).await? {
            return Ok(runtime);
        }

        let gate = self.gate(task_id);
        let _gate = gate.lock().await;
        if self.closed.load(Ordering::SeqCst) {
            bail!("task containers are shut down; no new runtime may start");
        }
        if let Some(runtime) = self.live_cached(task_id).await? {
            return Ok(runtime);
        }

        let project = self.task_project(task_id);
        let host_dir = match project.as_deref() {
            Some(project) => prepare_project_task_dir(project, task_id).map_err(|err| {
                anyhow!("cannot prepare the project task directory for {task_id}: {err}")
            })?,
            None => prepare_task_dir(&self.root, task_id)
                .map_err(|err| anyhow!("cannot prepare the task directory for {task_id}: {err}"))?,
        };
        let runtime = match self.mode {
            Mode::Local => self.local_runtime(host_dir, project.as_deref())?,
            Mode::Sandbox => self.sandbox_runtime(task_id, host_dir, project.as_deref())?,
            Mode::Docker => {
                self.docker_runtime(task_id, host_dir, project.as_deref())
                    .await?
            }
        };
        let runtime = Arc::new(runtime);
        self.cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(task_id, Arc::clone(&runtime));
        Ok(runtime)
    }

    /// Stops and removes `task_id`'s container or native sandbox processes,
    /// and forgets its runtime.
    ///
    /// The cache entry is dropped only after the removal succeeds, so a
    /// later [`shutdown`](Self::shutdown) can retry a failed removal. A
    /// container that never existed, or that another path already removed,
    /// is not an error. A container that this manager does not own, or that
    /// belongs to another task root, is refused and left running. Native
    /// sandbox removal kills each process group recorded in the task's
    /// pidfile, after the leader command line proves the pid is this task's,
    /// and leaves the task's files in place.
    pub async fn remove(&self, task_id: Uuid) -> anyhow::Result<()> {
        let gate = self.gate(task_id);
        let _gate = gate.lock().await;
        if self.mode == Mode::Docker {
            self.remove_container(task_id, &container_name(task_id))
                .await?;
        }
        if self.mode == Mode::Sandbox {
            self.remove_sandbox(task_id)?;
        }
        self.cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&task_id);
        Ok(())
    }

    /// Replaces `task_id`'s container: removes it, then starts a fresh one.
    ///
    /// This is how a command that outlived its own timeout is stopped: the
    /// old container, and every process in it, is gone before the new one
    /// starts. It fails if the old container cannot be removed, and leaves
    /// the cache entry in place for [`shutdown`](Self::shutdown) to retry.
    pub async fn restart(&self, task_id: Uuid) -> anyhow::Result<Arc<TaskRuntime>> {
        self.remove(task_id).await?;
        self.ensure(task_id).await
    }

    /// Stops and removes every container this manager started, and forgets
    /// every runtime. No later [`ensure`](Self::ensure) may start another.
    ///
    /// Every failure is reported; one failing container does not stop the
    /// others from being removed. A failed removal keeps its cache entry, so
    /// calling `shutdown` again retries it. A task whose `ensure` is still
    /// starting is included, and its container is removed once that start
    /// finishes.
    ///
    /// In native sandbox mode it then reaps every pidfile below `root/pids`,
    /// so a pidfile whose runtime is no longer cached does not survive a clean
    /// shutdown. A pidfile this reap cannot stop is reported as a warning and
    /// does not make a shutdown whose tasks were removed fail, just as startup
    /// continues past one.
    ///
    /// Production must call this; there is no async `Drop`.
    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.closed.store(true, Ordering::SeqCst);

        let mut task_ids: Vec<Uuid> = self
            .cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .copied()
            .collect();
        for task_id in self
            .starting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
        {
            if !task_ids.contains(task_id) {
                task_ids.push(*task_id);
            }
        }

        let mut failures = Vec::new();
        for task_id in task_ids {
            if let Err(err) = self.remove(task_id).await {
                failures.push(format!("{task_id}: {err}"));
            }
        }
        self.starting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();

        // A pidfile can outlive its cache entry, such as one left by a runtime
        // that was replaced before this call. Reap every pidfile below
        // `root/pids` once more so a clean shutdown leaves none behind. The
        // reap reports its own per-pidfile problems instead of failing, so
        // only a directory-level error can add to `failures` here.
        if self.mode == Mode::Sandbox
            && let Err(err) = self.reap_sandbox()
        {
            failures.push(format!("sandbox pidfiles: {err}"));
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(anyhow!(
                "could not remove every task container:\n{}",
                failures.join("\n")
            ))
        }
    }

    /// Removes every container this task root left behind by an earlier
    /// process.
    ///
    /// This is a startup cleanup, not part of normal task life: a process that
    /// died without calling [`shutdown`](Self::shutdown) leaves its running
    /// containers behind. A new process on the same root lists the containers
    /// carrying both `dhi.managed=1` and its own `dhi.root` hash and force
    /// removes each one, so they cannot accumulate across restarts.
    ///
    /// In native sandbox mode it reaps every pidfile below `root/pids` and
    /// kills each group it can prove is left over; at startup no task is
    /// running. Local mode has no containers and does nothing. Returns how
    /// many containers were removed. A listed container that disappears
    /// before `docker rm` runs is treated as removed; any other Docker
    /// failure is returned after the remaining ids have still been attempted.
    pub async fn reap_orphans(&self) -> anyhow::Result<usize> {
        if self.mode == Mode::Sandbox {
            // The report's warnings are printed by `reap_sandbox`; startup
            // only needs to know that the directory itself was readable.
            self.reap_sandbox()?;
            return Ok(0);
        }
        if self.mode != Mode::Docker {
            return Ok(0);
        }

        let mut command = Command::new("docker");
        command
            .arg("ps")
            .arg("-aq")
            .arg("--filter")
            .arg(format!("label={MANAGED_LABEL}=1"))
            .arg("--filter")
            .arg(format!("label={ROOT_LABEL}={}", self.root_hash));
        let output = run_docker(command, INSPECT_TIMEOUT, "list orphaned task containers").await?;
        if !output.status.success() {
            bail!(
                "cannot list orphaned task containers: docker ps exited with {}: {}",
                output.status,
                stderr_of(&output)
            );
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let ids: Vec<String> = stdout
            .lines()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .collect();

        let mut removed = 0;
        let mut failures = Vec::new();
        for id in ids {
            let mut command = Command::new("docker");
            command.arg("rm").arg("-f").arg(&id);
            let action = format!("remove orphaned container {id}");
            match run_docker(command, REMOVE_TIMEOUT, &action).await {
                Ok(output)
                    if output.status.success() || is_missing_container(&stderr_of(&output)) =>
                {
                    removed += 1;
                }
                Ok(output) => failures.push(format!(
                    "{id}: docker rm -f exited with {}: {}",
                    output.status,
                    stderr_of(&output)
                )),
                Err(err) => failures.push(format!("{id}: {err}")),
            }
        }

        if failures.is_empty() {
            Ok(removed)
        } else {
            Err(anyhow!(
                "could not remove every orphaned task container:\n{}",
                failures.join("\n")
            ))
        }
    }

    fn cached(&self, task_id: Uuid) -> Option<Arc<TaskRuntime>> {
        self.cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&task_id)
            .cloned()
    }

    /// The cached runtime of `task_id` when it can still be used.
    ///
    /// Local and native sandbox mode always reuse their cached runtime: a
    /// sandbox task has no global state that another task can retarget. In
    /// Docker mode the container is inspected first: a running container
    /// keeps the runtime, while a missing or stopped one is dropped from the
    /// cache so the caller starts a fresh one. A failed inspect is an error;
    /// the entry is kept so a later call, or `shutdown`, can retry.
    async fn live_cached(&self, task_id: Uuid) -> anyhow::Result<Option<Arc<TaskRuntime>>> {
        let Some(runtime) = self.cached(task_id) else {
            return Ok(None);
        };
        if self.mode != Mode::Docker {
            return Ok(Some(runtime));
        }

        let name = container_name(task_id);
        let running = self.inspect_running(&name).await.map_err(|err| {
            anyhow!(
                "cannot check whether the cached container {name} for task {task_id} is \
                 still running: {err}"
            )
        })?;
        if matches!(running, Some(true)) {
            return Ok(Some(runtime));
        }

        // Drop the dead entry only if it is still the one that was inspected;
        // a concurrent `ensure` may already have replaced it.
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        if cache
            .get(&task_id)
            .is_some_and(|entry| Arc::ptr_eq(entry, &runtime))
        {
            cache.remove(&task_id);
        }
        Ok(None)
    }

    fn gate(&self, task_id: Uuid) -> Arc<AsyncMutex<()>> {
        let mut starting = self.starting.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(
            starting
                .entry(task_id)
                .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
        )
    }

    fn local_runtime(
        &self,
        host_dir: PathBuf,
        project: Option<&Path>,
    ) -> anyhow::Result<TaskRuntime> {
        let spill = Spill(validate_private_dir(
            &host_dir.join(".dhi").join("spill"),
            "task spill directory",
        )?);
        Ok(TaskRuntime {
            base: host_dir.clone(),
            host_dir,
            workspace: Arc::new(Local),
            project_dir: project.map(Path::to_path_buf),
            seen: Seen::default(),
            spill,
            is_container: false,
            sandbox: None,
        })
    }

    async fn docker_runtime(
        &self,
        task_id: Uuid,
        host_dir: PathBuf,
        project: Option<&Path>,
    ) -> anyhow::Result<TaskRuntime> {
        // The `.dhi` directory and its spill subdirectory are the host paths
        // the container gets read-only, separately from the read-write task
        // folder. Validate both as private real directories before the
        // container starts, so a symbolic link planted in the task folder
        // cannot redirect the host spill writer. Mounting the parent `.dhi`
        // read-only as well keeps the container from renaming it and putting
        // a symbolic link in its place.
        let dhi = validate_private_dir(&host_dir.join(".dhi"), "task .dhi directory")
            .map_err(|err| anyhow!("cannot use the task .dhi directory: {err}"))?;
        if !dhi.starts_with(&host_dir) {
            bail!(
                "task .dhi directory {} resolved outside the task directory",
                dhi.display()
            );
        }
        let spill = validate_private_dir(&dhi.join("spill"), "task spill directory")
            .map_err(|err| anyhow!("cannot use the task spill directory: {err}"))?;
        if !spill.starts_with(&dhi) {
            bail!(
                "task spill directory {} resolved outside the task's .dhi directory",
                spill.display()
            );
        }
        self.start_container(
            task_id,
            &task_mounts(&self.root, self.host_root.as_deref(), task_id, project),
        )
        .await?;

        let name = container_name(task_id);
        // `Remote` appends `sh -c '<script>'` as one argument. Unlike `ssh`,
        // `docker exec` does not join its arguments into a command line for
        // the remote shell: it would try to execute the whole string as one
        // program name. The trailing `sh -c` makes that appended string the
        // argument to a shell inside the container.
        let transport = vec![
            OsString::from("docker"),
            OsString::from("exec"),
            OsString::from("-i"),
            OsString::from(&name),
            OsString::from("sh"),
            OsString::from("-c"),
        ];
        let workspace: Arc<dyn Workspace> = Arc::new(
            Remote::new(format!("task-{task_id}"), transport, Launcher::default())
                .map_err(|err| anyhow!("cannot build the transport to container {name}: {err}"))?,
        );
        Ok(TaskRuntime {
            base: PathBuf::from(CONTAINER_WORK),
            host_dir,
            workspace,
            project_dir: project.map(|_| PathBuf::from(CONTAINER_PROJECT)),
            seen: Seen::default(),
            spill: Spill(spill),
            is_container: true,
            sandbox: None,
        })
    }

    /// Builds the runtime of a task that runs as this process's user under
    /// `sandbox-exec`, with its own real host paths.
    ///
    /// The `.dhi` and spill directories are validated exactly like Docker
    /// mode. A private `pids` directory below the task root holds the
    /// server-owned pidfile the wrapper fills; the file itself is created by
    /// the wrapper. Nothing global changes, so any number of tasks can be
    /// prepared at the same time.
    fn sandbox_runtime(
        &self,
        task_id: Uuid,
        host_dir: PathBuf,
        project: Option<&Path>,
    ) -> anyhow::Result<TaskRuntime> {
        let config = self
            .sandbox
            .as_ref()
            .ok_or_else(|| anyhow!("native sandbox mode has no sandbox configuration"))?;

        // As in Docker mode, the host spill writer must never be redirected
        // by a symbolic link planted in the task folder.
        let dhi = validate_private_dir(&host_dir.join(".dhi"), "task .dhi directory")
            .map_err(|err| anyhow!("cannot use the task .dhi directory: {err}"))?;
        if !dhi.starts_with(&host_dir) {
            bail!(
                "task .dhi directory {} resolved outside the task directory",
                dhi.display()
            );
        }
        let spill = validate_private_dir(&dhi.join("spill"), "task spill directory")
            .map_err(|err| anyhow!("cannot use the task spill directory: {err}"))?;
        if !spill.starts_with(&dhi) {
            bail!(
                "task spill directory {} resolved outside the task's .dhi directory",
                spill.display()
            );
        }

        let pid_dir = self.root.join("pids");
        create_private_dir(&pid_dir)
            .map_err(|err| anyhow!("cannot create the sandbox pid directory: {err}"))?;
        let pid_dir = validate_private_dir(&pid_dir, "sandbox pid directory")
            .map_err(|err| anyhow!("cannot use the sandbox pid directory: {err}"))?;
        let pidfile = pid_dir.join(format!("{task_id}.pid"));

        let sandbox = SandboxRuntime {
            profile: config.profile.clone(),
            task_id,
            host_work: host_dir.clone(),
            host_project: project.map(Path::to_path_buf),
            pidfile,
        };
        Ok(TaskRuntime {
            base: host_dir.clone(),
            host_dir,
            workspace: Arc::new(Local),
            project_dir: project.map(Path::to_path_buf),
            seen: Seen::default(),
            spill: Spill(spill),
            is_container: false,
            sandbox: Some(sandbox),
        })
    }

    /// Stops every process group `task_id` recorded and forgets its pidfile.
    ///
    /// Each sandboxed command is wrapped so that its session leader writes the
    /// leader's pid to a server-owned pidfile and stays alive while the
    /// command runs. Removal kills the group of every recorded pid that the
    /// leader command line proves is this task's, and, when the leader is
    /// already gone, every group whose id still exists and therefore cannot
    /// have been reused. A pid whose leader is a different program is a reused
    /// pid and is left alone. A missing pidfile is success; the pidfile is
    /// removed afterwards. `DHI_TASK_ID` is diagnostic only; cleanup is
    /// pidfile-only, so a process that calls `setsid` and leaves its group is
    /// not found. No host paths other than the pidfile are removed. There is
    /// no task user and no `sudo`.
    fn remove_sandbox(&self, task_id: Uuid) -> anyhow::Result<()> {
        if let Some(runtime) = self.cached(task_id)
            && let Some(sandbox) = runtime.sandbox.as_ref()
        {
            kill_pidfile_group(&sandbox.pidfile).map_err(|err| {
                anyhow!(
                    "cannot stop the process group in {}: {err}",
                    sandbox.pidfile.display()
                )
            })?;
        }
        Ok(())
    }

    /// Startup cleanup for native sandbox mode.
    ///
    /// Every `*.pid` file below `root/pids` names process groups recorded by a
    /// process that died without [`shutdown`](Self::shutdown), so each one is
    /// checked and killed by [`kill_pidfile_group`] and each pidfile removed.
    /// At startup no task is running, so a group whose leader is gone can only
    /// hold left-over members. The check relies on the leader command line and
    /// on the group id still existing; a child that called `setsid` left the
    /// group and is not caught. `DHI_TASK_ID` is diagnostic only; cleanup is
    /// pidfile-only. No task user, no `sudo`, no aliases are involved.
    ///
    /// At most [`REAP_PIDFILE_LIMIT`] pidfiles are processed in one call, so a
    /// backlog cannot hold up startup before the listener binds. When more
    /// remain, a warning names how many were skipped and they are left for the
    /// next startup or [`shutdown`](Self::shutdown). Each pidfile that cannot
    /// be stopped is also counted and warned about, and processing continues,
    /// so one bad record neither stops the reap nor fails a shutdown. A
    /// missing `root/pids` directory is success with nothing processed.
    ///
    /// Returns what happened, which the callers log and tests inspect.
    fn reap_sandbox(&self) -> anyhow::Result<ReapReport> {
        let pid_dir = self.root.join("pids");
        let entries = match fs::read_dir(&pid_dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok(ReapReport::default());
            }
            Err(err) => {
                return Err(anyhow!(
                    "cannot list the sandbox pid directory {}: {err}",
                    pid_dir.display()
                ));
            }
        };
        let mut report = ReapReport::default();
        for entry in entries {
            let path = entry
                .map_err(|err| anyhow!("cannot read the sandbox pid directory: {err}"))?
                .path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("pid") {
                continue;
            }
            if report.processed >= REAP_PIDFILE_LIMIT {
                report.skipped += 1;
                continue;
            }
            report.processed += 1;
            if let Err(err) = kill_pidfile_group(&path) {
                report.failed += 1;
                eprintln!(
                    "[dhi-exec] warning: cannot stop the process group in {}: {err}",
                    path.display()
                );
            }
        }
        if report.skipped > 0 {
            eprintln!(
                "[dhi-exec] warning: reaped {} pidfiles, skipped {} beyond the cap of {}; \
                 the next startup or shutdown retries them",
                report.processed, report.skipped, REAP_PIDFILE_LIMIT
            );
        }
        if report.failed > 0 {
            eprintln!(
                "[dhi-exec] warning: {} pidfiles could not be stopped in {}",
                report.failed,
                pid_dir.display()
            );
        }
        Ok(report)
    }

    /// Builds the one `docker run` command for a task container.
    ///
    /// The command is separate from [`Self::start_container`] so tests can
    /// inspect the exact mounts, including the hidden image masks.
    fn docker_run_command(&self, name: &str, task_id: Uuid, mounts: &TaskMounts) -> Command {
        let mut command = Command::new("docker");
        command
            .arg("run")
            .arg("-d")
            .arg("--rm")
            .arg("--init")
            .arg("--name")
            .arg(name)
            .arg("--label")
            .arg(format!("{MANAGED_LABEL}=1"))
            .arg("--label")
            .arg(format!("{TASK_LABEL}={task_id}"))
            .arg("--label")
            .arg(format!("{ROOT_LABEL}={}", self.root_hash))
            .arg("--label")
            .arg(format!("{CONFIG_LABEL}={}", self.config_hash));
        if let Some(user) = &self.user {
            command.arg("--user").arg(user);
        }
        let work_mount = mount_spec(&mounts.work, CONTAINER_WORK, "rw");
        let dhi_mount = mount_spec(&mounts.dhi, CONTAINER_DHI, "ro");
        let spill_mount = mount_spec(&mounts.spill, CONTAINER_SPILL, "ro");
        command
            .arg("--read-only")
            .arg("--tmpfs")
            .arg("/tmp:rw,noexec,nosuid,size=512m");
        append_hidden_image_tmpfs(&mut command);
        command
            .arg("--cap-drop")
            .arg("ALL")
            .arg("--security-opt")
            .arg("no-new-privileges")
            .arg("--pids-limit")
            .arg(self.config.pids_limit.to_string())
            .arg("--memory")
            .arg(&self.config.memory)
            .arg("--memory-swap")
            .arg(&self.config.memory)
            .arg("--ulimit")
            .arg("nofile=1024:1024")
            .arg("--cpus")
            .arg(&self.config.cpus)
            .arg("--network")
            .arg(&self.config.network);
        if let Some(project) = &mounts.project {
            command.arg("--tmpfs").arg(PROJECT_DHI_TMPFS);
            command
                .arg("-v")
                .arg(mount_spec(project, CONTAINER_PROJECT, "ro"));
        }
        command
            .arg("-v")
            .arg(work_mount)
            .arg("-v")
            .arg(dhi_mount)
            .arg("-v")
            .arg(spill_mount)
            .arg("-w")
            .arg(CONTAINER_WORK)
            .arg("-e")
            .arg("HOME=/work/.home")
            .arg("-e")
            .arg("TMPDIR=/work/.tmp")
            .arg("-e")
            .arg("CARGO_HOME=/work/.cargo")
            .arg("-e")
            .arg("PYTHONDONTWRITEBYTECODE=1")
            .arg("--entrypoint")
            .arg("sh")
            .arg(&self.config.image)
            .arg("-c")
            .arg("trap : TERM INT; sleep infinity & wait");
        command
    }

    /// Starts the task container, reusing it only when it is a running
    /// container this manager started for this task, root, and configuration.
    ///
    /// A container with the expected name that is not managed, belongs to
    /// another task, or was started for another root is refused: it is never
    /// removed. A managed container for this task and root whose
    /// configuration differs is removed and started again.
    async fn start_container(&self, task_id: Uuid, mounts: &TaskMounts) -> anyhow::Result<()> {
        let name = container_name(task_id);
        if let Some(container) = self.inspect_container(&name).await? {
            if !is_managed_for(&container.labels, task_id) {
                bail!(
                    "container {name} already exists but is not managed for task {task_id}; \
                     refusing to reuse or remove it"
                );
            }
            if container.labels.get(ROOT_LABEL) != Some(&self.root_hash) {
                bail!(
                    "container {name} belongs to a different task root; refusing to reuse \
                     or remove it"
                );
            }
            if container.running && container.labels.get(CONFIG_LABEL) == Some(&self.config_hash) {
                return Ok(());
            }
            // It is ours, for this task and root, but stopped or started
            // with another configuration: replace it.
            self.remove_container(task_id, &name).await?;
        }

        let command = self.docker_run_command(&name, task_id, mounts);

        let output =
            match run_docker(command, RUN_TIMEOUT, &format!("start container {name}")).await {
                Ok(output) => output,
                Err(err) => {
                    // A timed-out Docker CLI may still leave the container
                    // behind. Remove only one that carries this manager's
                    // labels for this task.
                    let _ = self.remove_container(task_id, &name).await;
                    return Err(err);
                }
            };
        if !output.status.success() {
            let stderr = stderr_of(&output);
            let _ = self.remove_container(task_id, &name).await;
            bail!(
                "cannot start container {name} from image {}: docker run exited with {}: {stderr}",
                self.config.image,
                output.status
            );
        }

        self.wait_until_running(task_id, &name).await
    }

    /// Waits up to [`START_TIMEOUT`] for `name` to report running.
    async fn wait_until_running(&self, task_id: Uuid, name: &str) -> anyhow::Result<()> {
        let deadline = Instant::now() + START_TIMEOUT;
        let mut last;
        loop {
            last = match self.inspect_running(name).await {
                Ok(Some(true)) => return Ok(()),
                Ok(Some(false)) => "the container exists but is not running".to_owned(),
                Ok(None) => "the container does not exist".to_owned(),
                Err(err) => err.to_string(),
            };
            if Instant::now() >= deadline {
                let _ = self.remove_container(task_id, name).await;
                bail!(
                    "container {name} did not report running within {} seconds: {last}",
                    START_TIMEOUT.as_secs()
                );
            }
            sleep(POLL_INTERVAL).await;
        }
    }

    /// `Some(true)` when `name` runs, `Some(false)` when it exists but does
    /// not run, `None` when it does not exist.
    async fn inspect_running(&self, name: &str) -> anyhow::Result<Option<bool>> {
        let mut command = Command::new("docker");
        command
            .arg("inspect")
            .arg("-f")
            .arg("{{.State.Running}}")
            .arg(name);
        let output = run_docker(
            command,
            INSPECT_TIMEOUT,
            &format!("inspect container {name}"),
        )
        .await?;
        if output.status.success() {
            let running = String::from_utf8_lossy(&output.stdout);
            return Ok(Some(running.trim() == "true"));
        }
        let stderr = stderr_of(&output);
        if is_missing_container(&stderr) {
            Ok(None)
        } else {
            Err(anyhow!(
                "docker inspect {name} exited with {}: {stderr}",
                output.status
            ))
        }
    }

    /// The running state and labels of `name`, or `None` when it does not
    /// exist.
    async fn inspect_container(&self, name: &str) -> anyhow::Result<Option<ContainerSnapshot>> {
        let mut command = Command::new("docker");
        command
            .arg("inspect")
            .arg("-f")
            .arg("{{.State.Running}}|{{json .Config.Labels}}")
            .arg(name);
        let output = run_docker(
            command,
            INSPECT_TIMEOUT,
            &format!("inspect container {name}"),
        )
        .await?;
        if !output.status.success() {
            let stderr = stderr_of(&output);
            if is_missing_container(&stderr) {
                return Ok(None);
            }
            return Err(anyhow!(
                "docker inspect {name} exited with {}: {stderr}",
                output.status
            ));
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let (running, labels) = text.split_once('|').ok_or_else(|| {
            anyhow!("docker inspect {name} returned an unexpected format: {text}")
        })?;
        let running = match running.trim() {
            "true" => true,
            "false" => false,
            other => bail!("docker inspect {name} returned a bad running state: {other}"),
        };
        let labels = labels.trim();
        let labels: HashMap<String, String> = if labels == "null" || labels.is_empty() {
            HashMap::new()
        } else {
            serde_json::from_str(labels)
                .map_err(|err| anyhow!("docker inspect {name} returned bad labels: {err}"))?
        };
        Ok(Some(ContainerSnapshot { running, labels }))
    }

    /// Forcibly removes `name` when it is a container this manager started
    /// for `task_id`; a missing container is not an error.
    ///
    /// A container that is missing the managed label, names another task,
    /// carries no labels at all, or belongs to another root is refused and
    /// left untouched.
    async fn remove_container(&self, task_id: Uuid, name: &str) -> anyhow::Result<()> {
        let Some(container) = self.inspect_container(name).await? else {
            return Ok(());
        };
        if !is_managed_for(&container.labels, task_id) {
            bail!("refusing to remove container {name}: it is not managed for task {task_id}");
        }
        if container.labels.get(ROOT_LABEL) != Some(&self.root_hash) {
            bail!("refusing to remove container {name}: it belongs to a different task root");
        }
        let mut command = Command::new("docker");
        command.arg("rm").arg("-f").arg(name);
        let output =
            run_docker(command, REMOVE_TIMEOUT, &format!("remove container {name}")).await?;
        if output.status.success() || is_missing_container(&stderr_of(&output)) {
            return Ok(());
        }
        bail!(
            "cannot remove container {name}: docker rm -f exited with {}: {}",
            output.status,
            stderr_of(&output)
        )
    }
}

/// What one native sandbox reap of `root/pids` did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ReapReport {
    /// The pidfiles that were examined and had their groups stopped.
    processed: usize,
    /// The pidfiles left for the next reap because of [`REAP_PIDFILE_LIMIT`].
    skipped: usize,
    /// The pidfiles that were examined but could not be stopped.
    failed: usize,
}

/// What one `docker inspect` said about a named container.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ContainerSnapshot {
    /// Whether the container is running.
    running: bool,
    /// Every label on the container, empty when it has none.
    labels: HashMap<String, String>,
}

/// Whether `labels` say this manager started a container for `task_id`.
fn is_managed_for(labels: &HashMap<String, String>, task_id: Uuid) -> bool {
    let expected = task_id.to_string();
    labels.get(MANAGED_LABEL).map(String::as_str) == Some("1")
        && labels.get(TASK_LABEL) == Some(&expected)
}

/// The `--user` value for containers on this platform.
fn container_user() -> Option<String> {
    #[cfg(unix)]
    {
        // SAFETY: getuid and getgid have no preconditions and only read this
        // process's ids.
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        Some(format!("{uid}:{gid}"))
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Adds the image-only path masks to one `docker run` command.
fn append_hidden_image_tmpfs(command: &mut Command) {
    for hidden in HIDDEN_IMAGE_TMPFS {
        command.arg("--tmpfs").arg(hidden);
    }
}

/// The hash of the configuration that changes what a container is.
///
/// The hidden image masks are part of the hash: an upgrade that adds a mask
/// must replace a reused container started by the old configuration.
fn config_hash(config: &ContainerConfig, user: Option<&str>, mount_root: &Path) -> String {
    sha256_hex(&config_hash_material(config, user, mount_root))
}

fn config_hash_material(
    config: &ContainerConfig,
    user: Option<&str>,
    mount_root: &Path,
) -> Vec<u8> {
    let mut material = format!(
        "image={}\nnetwork={}\nmemory={}\ncpus={}\npids={}\nuser={}\n",
        config.image,
        config.network,
        config.memory,
        config.cpus,
        config.pids_limit,
        user.unwrap_or("")
    )
    .into_bytes();
    material.extend_from_slice(b"hidden_image_tmpfs=\n");
    for hidden in HIDDEN_IMAGE_TMPFS {
        material.extend_from_slice(hidden.as_bytes());
        material.push(b'\n');
    }
    material.extend_from_slice(b"mount_root=");
    material.extend_from_slice(mount_root.as_os_str().as_encoded_bytes());
    material
}

/// The lowercase hexadecimal SHA-256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let digest = Sha256::digest(bytes);
    let mut text = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(text, "{byte:02x}").expect("writing to a String cannot fail");
    }
    text
}

/// The container name of `task_id`.
fn container_name(task_id: Uuid) -> String {
    format!("{CONTAINER_PREFIX}{task_id}")
}

/// A Docker `-v` value: `source:destination:mode`, with the host path kept
/// as an `OsString` so a non-UTF-8 path is not mangled.
fn mount_spec(source: &Path, destination: &str, mode: &str) -> OsString {
    let mut spec = source.as_os_str().to_os_string();
    spec.push(":");
    spec.push(destination);
    spec.push(":");
    spec.push(mode);
    spec
}

/// The host bind-mount sources of one task container.
///
/// `root` is the task root as this process sees it. When `host_root` is set,
/// it is the path the Docker daemon sees for the same root, so every source
/// is rebuilt below it; the in-container destinations never change.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskMounts {
    /// The task work directory, mounted read-write at [`CONTAINER_WORK`].
    work: PathBuf,
    /// The task `.dhi` directory, mounted read-only at [`CONTAINER_DHI`].
    dhi: PathBuf,
    /// The task spill directory, mounted read-only at [`CONTAINER_SPILL`].
    spill: PathBuf,
    /// The selected project root, mounted read-only at [`CONTAINER_PROJECT`].
    project: Option<PathBuf>,
}

/// The path below `/project`, or `None` when `requested` is not a `/project`
/// path. `/project` itself is the empty relative path; `/project-x` is not a
/// match.
fn project_request_relative(requested: &str) -> Option<&str> {
    let rest = requested.strip_prefix(CONTAINER_PROJECT)?;
    if !rest.is_empty() && !rest.starts_with('/') {
        return None;
    }
    Some(rest.trim_start_matches('/'))
}

/// Resolves `path` the way the `Local` file tools resolve it: the longest
/// existing ancestor is canonicalized, so symbolic links in existing
/// components are followed, and the still-missing tail is joined back on with
/// `..` resolved lexically. A missing file therefore keeps its name while a
/// link on the way is still seen.
fn resolve_like_tool(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let real = absolute.ancestors().find_map(|ancestor| {
        let real = fs::canonicalize(ancestor).ok()?;
        Some((real, absolute.strip_prefix(ancestor).ok()?))
    });
    let (mut out, rest) = real.unwrap_or((PathBuf::new(), &absolute));
    for component in rest.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// The host paths Docker mounts for `task_id`.
///
/// Deployment requirement: project bind sources are the absolute folder the
/// user selected through the host file browser. The server process and the
/// Docker daemon must therefore see the project folder at the same absolute
/// path. `DHI_PROJECT_FOLDERS` is legacy task metadata, not the selector.
/// Unlike the workspace root, project folders have no host-root remapping
/// variable; configure the same path on both sides.
fn task_mounts(
    root: &Path,
    host_root: Option<&Path>,
    task_id: Uuid,
    project: Option<&Path>,
) -> TaskMounts {
    let (work, project) = match project {
        Some(project) => (
            project.join(".dhi").join(task_id.to_string()),
            Some(project.to_path_buf()),
        ),
        None => (host_root.unwrap_or(root).join(task_id.to_string()), None),
    };
    TaskMounts {
        dhi: work.join(".dhi"),
        spill: work.join(".dhi").join("spill"),
        work,
        project,
    }
}

/// The diagnostic environment variable every sandboxed command carries.
///
/// Its value names the task, for operators. It is never used to find
/// processes: cleanup is pidfile-only, because matching argv text can hit an
/// unrelated process that merely mentions the value.
const DHI_TASK_ID_ENV: &str = "DHI_TASK_ID=";

/// The positive process ids in a pidfile, one per line, deduplicated and in
/// first-seen order. A line that is not an integer is skipped; a value of 1
/// or less is refused because it can never name a task's process group.
fn parse_pidfile(text: &str) -> Vec<i32> {
    let mut pids = Vec::new();
    for pid in text
        .lines()
        .filter_map(|line| line.trim().parse::<i32>().ok())
        .filter(|pid| *pid > 1)
    {
        if !pids.contains(&pid) {
            pids.push(pid);
        }
    }
    pids
}

/// The command line of the process `pid` reads, or `None` when no such
/// process is left.
///
/// This runs `/bin/ps` from the server, outside the sandbox, because a
/// sandboxed command cannot read the process table. Only the command line is
/// asked for; a pid that does not exist is reported by `ps` as a failed run
/// with no output, while a real one always prints at least one line.
#[cfg(unix)]
fn leader_command(pid: i32) -> io::Result<Option<String>> {
    let output = StdCommand::new("/bin/ps")
        .arg("-ww")
        .arg("-o")
        .arg("command=")
        .arg("-p")
        .arg(pid.to_string())
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let command = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if command.is_empty() {
        Ok(None)
    } else {
        Ok(Some(command))
    }
}

/// Leader checks are unavailable on this platform.
#[cfg(not(unix))]
fn leader_command(pid: i32) -> io::Result<Option<String>> {
    let _ = pid;
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "reading a process command line is not supported on this platform",
    ))
}

/// Whether `command` is the pidfile wrapper that leads the group of `pidfile`.
///
/// The wrapper keeps its own command line while it waits for the command, so
/// it carries both the `dhi-pids` name and the pidfile path. A leader for
/// another pidfile, or any other process that reused the pid, does not match.
fn is_pidfile_leader(command: &str, pidfile: &Path) -> bool {
    let pidfile = pidfile.to_string_lossy();
    !pidfile.is_empty() && command.contains("dhi-pids") && command.contains(pidfile.as_ref())
}

/// Sends `SIGKILL` to the process group `pid` leads. A group that no longer
/// exists is not an error.
#[cfg(unix)]
fn kill_process_group(pid: i32) -> io::Result<()> {
    if pid <= 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to kill process group {pid}"),
        ));
    }
    // SAFETY: kill takes plain integers; a negative pid names a process group
    // and has no memory-safety preconditions. ESRCH is ignored below.
    let result = unsafe { libc::kill(-pid, libc::SIGKILL) };
    if result == -1 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        return Err(err);
    }
    Ok(())
}

/// Asks whether the process group `pid` leads still has a member, with
/// signal 0.
#[cfg(unix)]
fn process_group_exists(pid: i32) -> io::Result<bool> {
    if pid <= 1 {
        return Ok(false);
    }
    // SAFETY: kill with signal 0 only checks the group and has no
    // memory-safety preconditions.
    let result = unsafe { libc::kill(-pid, 0) };
    if result == 0 {
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        // Any live member the caller may not signal still proves the group.
        Some(libc::EPERM) => Ok(true),
        Some(libc::ESRCH) => Ok(false),
        _ => Err(err),
    }
}

/// Whether the group of `pid` still exists.
#[cfg(not(unix))]
fn process_group_exists(pid: i32) -> io::Result<bool> {
    let _ = pid;
    Ok(false)
}

/// The id of the process group `pid` belongs to, from the server's process
/// list outside the sandbox.
#[cfg(all(unix, target_os = "macos", test))]
fn process_group_of(pid: i32) -> Option<i32> {
    let output = StdCommand::new("/bin/ps")
        .arg("-o")
        .arg("pgid=")
        .arg("-p")
        .arg(pid.to_string())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

/// Killing process groups is unavailable on this platform.
#[cfg(not(unix))]
fn kill_process_group(pid: i32) -> io::Result<()> {
    let _ = pid;
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "killing process groups is not supported on this platform",
    ))
}

/// Kills every process group recorded in `pidfile` that the record can still
/// be proved to name, then removes the pidfile.
///
/// Each recorded pid is only a hint: the system may have reused it since the
/// command started. So a pid is acted on only after one of two checks:
///
/// - its leader exists and its command line names this pidfile's `dhi-pids`
///   wrapper, so the group is this task's and is killed;
/// - its leader is gone, but a process group with that id still exists. A
///   group id is not reused while any process is still in the group, so the
///   remaining members are the ones the leader left behind, and the group is
///   killed. A group that signal 0 reports as missing is already dead.
///
/// A leader that exists but does not name this pidfile is a reused pid and is
/// left alone: it belongs to someone else. A missing pidfile is success.
fn kill_pidfile_group(pidfile: &Path) -> io::Result<()> {
    let text = match fs::read_to_string(pidfile) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    for pid in parse_pidfile(&text) {
        match leader_command(pid)? {
            Some(command) if is_pidfile_leader(&command, pidfile) => {
                kill_process_group(pid)?;
            }
            Some(_) => {}
            None => {
                if process_group_exists(pid)? {
                    kill_process_group(pid)?;
                }
            }
        }
    }
    match fs::remove_file(pidfile) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// The prefix that runs one command under `sandbox-exec`, as the same macOS
/// user as this process and against the task's real host paths.
///
/// `/usr/bin/env` sets the environment the task should see before
/// `sandbox-exec` starts, so no parent variable leaks in and `HOME`, `TMPDIR`
/// and `CARGO_HOME` stay below the task's work directory. `DHI_TASK_ID` is
/// inherited by every child as a diagnostic; it is not used to find processes.
///
/// The profile is passed `PIDFILE` and the command runs through a wrapper
/// shell that appends its session-leader pid to that server-owned file and
/// then waits for the command instead of `exec`ing it. Because the wrapper
/// stays alive, the leader's command line keeps `dhi-pids <pidfile>`, so the
/// server can prove which group a recorded pid names. `Launcher` and the PTY
/// session both start the wrapper in a new session, so the pid names the
/// whole process group.
///
/// The shell launcher lets the `Shell` tool append `sh -c <line>` itself; a
/// sibling block such as the PTY session can append its own program instead.
/// The wrapper shifts its own arguments, so both cases run the appended
/// program unchanged.
///
/// The profile receives `PROJECT`, `WORK`, `SPILL` and `PIDFILE` as literal
/// paths and matches real paths, so [`canonical_path`] gives each one no `.`
/// or `..` component. A runtime path that is relative is a contract
/// violation and panics before any process starts; in host paths there is no
/// `/../` to test, so it is nothing a rendered string could produce.
pub fn sandbox_prefix(runtime: &SandboxRuntime) -> Vec<OsString> {
    let project = canonical_path(
        runtime
            .host_project
            .as_deref()
            .unwrap_or(&runtime.host_work),
        "sandbox project path",
    );
    let work = canonical_path(&runtime.host_work, "sandbox work path");
    let spill = canonical_path(&work.join(".dhi"), "sandbox spill path");
    let pidfile = canonical_path(&runtime.pidfile, "sandbox pidfile");
    let home = canonical_path(&work.join(".home"), "sandbox home path");
    let tmp = canonical_path(&work.join(".tmp"), "sandbox tmp path");
    let cargo = canonical_path(&work.join(".cargo"), "sandbox cargo path");
    vec![
        OsString::from("/usr/bin/env"),
        OsString::from(format!("PATH={DEFAULT_PATH}")),
        OsString::from(format!("HOME={}", home.display())),
        OsString::from(format!("TMPDIR={}", tmp.display())),
        OsString::from(format!("CARGO_HOME={}", cargo.display())),
        OsString::from("GIT_CONFIG_NOSYSTEM=1"),
        OsString::from("TERM=dumb"),
        OsString::from("NO_COLOR=1"),
        OsString::from(format!("{DHI_TASK_ID_ENV}{}", runtime.task_id)),
        OsString::from("/usr/bin/sandbox-exec"),
        OsString::from("-f"),
        runtime.profile.as_os_str().to_os_string(),
        OsString::from("-D"),
        OsString::from(format!("PROJECT={}", project.display())),
        OsString::from("-D"),
        OsString::from(format!("WORK={}", work.display())),
        OsString::from("-D"),
        OsString::from(format!("SPILL={}", spill.display())),
        OsString::from("-D"),
        OsString::from(format!("PIDFILE={}", pidfile.display())),
        OsString::from("/bin/sh"),
        OsString::from("-c"),
        OsString::from("printf '%s\\n' \"$$\" >> \"$1\"; shift; \"$@\""),
        OsString::from("dhi-pids"),
        pidfile.into_os_string(),
    ]
}

/// `path` with no `.` or `..` component.
///
/// The path is resolved lexically: `.` is dropped and `..` pops the last
/// normal component, the way the kernel resolves them, so `/a/../b` becomes
/// `/b` and `..` at the root stays at the root. A path that already is
/// canonical therefore is returned unchanged, and a new path can never
/// contain `/../` or `/.`, because `/` never appears inside a component.
///
/// The runtime paths are the real host paths, already canonical; this makes
/// the contract explicit for any caller. A relative path is a contract
/// violation, because the profile matches absolute real paths, so it panics
/// with `label` before any process starts.
fn canonical_path(path: &Path, label: &str) -> PathBuf {
    assert!(
        path.is_absolute(),
        "{label} {} must be an absolute path",
        path.display()
    );
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
            Component::Prefix(_) => unreachable!("a Unix path has no prefix component"),
        }
    }
    debug_assert!(normalized.is_absolute(), "{label} lost its root");
    normalized
}

/// Runs `command`, failing clearly when Docker cannot be started or does not
/// finish within `timeout`.
async fn run_docker(
    mut command: Command,
    timeout: Duration,
    action: &str,
) -> anyhow::Result<Output> {
    command.kill_on_drop(true);
    match tokio::time::timeout(timeout, command.output()).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(err)) => Err(anyhow!("cannot run docker to {action}: {err}")),
        Err(_) => Err(anyhow!(
            "docker did not finish trying to {action} within {} seconds",
            timeout.as_secs()
        )),
    }
}

/// Whether Docker's error text says the named container does not exist.
fn is_missing_container(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("no such container") || stderr.contains("no such object")
}

fn stderr_of(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if stderr.is_empty() {
        "(no error output)".to_owned()
    } else {
        stderr
    }
}

/// Creates `root/<task-id>` and its private subdirectories, then validates
/// every one of them.
fn prepare_task_dir(root: &Path, task_id: Uuid) -> io::Result<PathBuf> {
    prepare_task_dir_at(&root.join(task_id.to_string()))
}

/// Creates a project-mode task folder at `<project>/.dhi/<task-id>`.
///
/// The project's `.dhi` directory and its `.gitignore` (containing `*`) are
/// created when absent. An existing `.gitignore` is never overwritten. The
/// task folder must be writable by the task container user; a failure is
/// reported with the folder path and uid/gid.
pub fn prepare_project_task_dir(project: &Path, task_id: Uuid) -> io::Result<PathBuf> {
    let metadata = fs::symlink_metadata(project).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "project folder {} cannot be inspected: {err}",
                project.display()
            ),
        )
    })?;
    if metadata.file_type().is_symlink() {
        return Err(integrity_error(format!(
            "project folder {} is a symbolic link; refusing to use it",
            project.display()
        )));
    }
    if !metadata.is_dir() {
        return Err(integrity_error(format!(
            "project folder {} is not a directory",
            project.display()
        )));
    }
    let project = fs::canonicalize(project)?;

    let dhi_path = project.join(".dhi");
    create_private_dir(&dhi_path)?;
    let dhi = validate_private_dir(&dhi_path, "project .dhi directory").map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("cannot use the project .dhi directory: {err}"),
        )
    })?;
    if !dhi.starts_with(&project) {
        return Err(integrity_error(format!(
            "project .dhi directory {} resolved outside the project {}",
            dhi.display(),
            project.display()
        )));
    }
    create_gitignore_if_absent(&dhi)?;

    let expected = dhi.join(task_id.to_string());
    let dir = prepare_task_dir_at(&expected)?;
    if !dir.starts_with(&dhi) {
        return Err(integrity_error(format!(
            "project task folder {} resolved outside {}",
            dir.display(),
            dhi.display()
        )));
    }
    check_task_folder_writable(&dir)?;
    Ok(dir)
}

/// Creates and validates one task directory with its private layout.
fn prepare_task_dir_at(expected: &Path) -> io::Result<PathBuf> {
    create_private_dir(expected)?;
    let dir = validate_private_dir(expected, "task directory")?;
    if dir != expected {
        return Err(integrity_error(format!(
            "task directory {} resolved to {}, not {}",
            expected.display(),
            dir.display(),
            expected.display()
        )));
    }

    for below in [".home", ".tmp", ".cargo", ".dhi", ".dhi/spill"] {
        let path = dir.join(below);
        create_private_dir(&path)?;
        let canonical = validate_private_dir(&path, "task workspace directory")?;
        if !canonical.starts_with(&dir) {
            return Err(integrity_error(format!(
                "task workspace directory {} resolved outside the task directory to {}",
                path.display(),
                canonical.display()
            )));
        }
    }
    Ok(dir)
}

/// Writes `*` into `<dhi>/.gitignore` only when the file does not exist.
fn create_gitignore_if_absent(dhi: &Path) -> io::Result<()> {
    let path = dhi.join(".gitignore");
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            if let Err(err) = file.write_all(b"*\n") {
                drop(file);
                let _ = fs::remove_file(&path);
                return Err(io::Error::new(
                    err.kind(),
                    format!("cannot write {}: {err}", path.display()),
                ));
            }
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(io::Error::new(
            err.kind(),
            format!("cannot create {}: {err}", path.display()),
        )),
    }
}

/// Proves the task container user can write in the task folder.
fn check_task_folder_writable(dir: &Path) -> io::Result<()> {
    let probe = dir.join(format!(".dhi-write-check-{}", Uuid::new_v4()));
    let user = container_user().unwrap_or_else(|| "the container user".to_string());
    if let Err(err) = fs::write(&probe, b"ok") {
        return Err(io::Error::new(
            err.kind(),
            format!(
                "task folder {} is not writable by the task container user ({user}): {err}",
                dir.display()
            ),
        ));
    }
    fs::remove_file(&probe).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "task folder {} is not writable by the task container user ({user}): \
                 cannot remove the write probe: {err}",
                dir.display()
            ),
        )
    })
}

/// Creates `path` and any missing parent with private permissions on Unix.
fn create_private_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;

        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
    }
}

/// Checks that `path` is a real directory, gives it private permissions on
/// Unix, and returns its canonical path.
fn validate_private_dir(path: &Path, label: &str) -> io::Result<PathBuf> {
    let metadata = fs::symlink_metadata(path).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("{label} {} cannot be inspected: {err}", path.display()),
        )
    })?;
    if metadata.file_type().is_symlink() {
        return Err(integrity_error(format!(
            "{label} {} is a symbolic link; refusing to use it",
            path.display()
        )));
    }
    if !metadata.is_dir() {
        return Err(integrity_error(format!(
            "{label} {} is not a directory",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    fs::canonicalize(path)
}

/// The error for a path that failed its integrity check.
fn integrity_error(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Saves the named variables and restores them when dropped, so a test
    /// that changes the environment leaves nothing behind.
    struct EnvGuard(Vec<(&'static str, Option<OsString>)>);

    impl EnvGuard {
        fn new(names: &[&'static str]) -> Self {
            Self(
                names
                    .iter()
                    .map(|name| (*name, std::env::var_os(name)))
                    .collect(),
            )
        }

        fn clear(&self) {
            for (name, _) in &self.0 {
                // SAFETY: the caller is serial and the guard restores it.
                unsafe { std::env::remove_var(name) };
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (name, value) in &self.0 {
                // SAFETY: the guard restores exactly what it saved.
                match value {
                    Some(value) => unsafe { std::env::set_var(name, value) },
                    None => unsafe { std::env::remove_var(name) },
                }
            }
        }
    }

    #[tokio::test]
    async fn local_task_dir_creates_the_private_layout() {
        let temp = tempfile::tempdir().unwrap();
        let containers = TaskContainers::local(temp.path().join("tasks")).unwrap();
        let task = Uuid::new_v4();

        let runtime = containers.ensure(task).await.unwrap();

        assert_eq!(runtime.host_dir, containers.root().join(task.to_string()));
        assert_eq!(runtime.base, runtime.host_dir);
        assert!(!runtime.is_container);
        assert_eq!(runtime.workspace.host(), None);
        assert!(runtime.host_dir.join(".home").is_dir());
        assert!(runtime.host_dir.join(".tmp").is_dir());
        assert!(runtime.host_dir.join(".cargo").is_dir());
        assert!(runtime.host_dir.join(".dhi/spill").is_dir());
    }

    #[test]
    fn the_root_is_canonical() {
        let temp = tempfile::tempdir().unwrap();
        let requested = temp.path().join("nested").join("tasks");
        let containers = TaskContainers::local(&requested).unwrap();

        assert_eq!(
            containers.root(),
            fs::canonicalize(&requested).unwrap().as_path()
        );
        assert!(containers.root().is_absolute());
    }

    #[cfg(unix)]
    #[test]
    fn the_private_layout_is_0700() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let containers = TaskContainers::local(temp.path().join("tasks")).unwrap();
        let task = Uuid::new_v4();
        let runtime = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(containers.ensure(task))
            .unwrap();

        for path in [
            containers.root().to_path_buf(),
            runtime.host_dir.clone(),
            runtime.host_dir.join(".home"),
            runtime.host_dir.join(".tmp"),
            runtime.host_dir.join(".cargo"),
            runtime.host_dir.join(".dhi"),
            runtime.host_dir.join(".dhi/spill"),
        ] {
            let mode = fs::symlink_metadata(&path).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o077,
                0,
                "{} is not private: mode {mode:o}",
                path.display()
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlinked_task_directory_is_rejected() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let containers = TaskContainers::local(temp.path().join("tasks")).unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let task = Uuid::new_v4();
        let task_dir = containers.root().join(task.to_string());
        symlink(&outside, &task_dir).unwrap();

        let err = containers
            .ensure(task)
            .await
            .expect_err("a symlinked task directory is refused");
        assert!(err.to_string().contains("symbolic link"), "{err}");
        assert_eq!(
            fs::read_dir(&outside).unwrap().count(),
            0,
            "nothing was created through the symlink"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinked_workspace_directories_are_rejected() {
        use std::os::unix::fs::symlink;

        for below in [".home", ".tmp", ".cargo", ".dhi"] {
            let temp = tempfile::tempdir().unwrap();
            let containers = TaskContainers::local(temp.path().join("tasks")).unwrap();
            let outside = temp.path().join("outside");
            fs::create_dir_all(&outside).unwrap();
            let task = Uuid::new_v4();
            let task_dir = containers.root().join(task.to_string());
            fs::create_dir_all(&task_dir).unwrap();
            symlink(&outside, task_dir.join(below)).unwrap();

            let err = containers.ensure(task).await.unwrap_err();
            assert!(err.to_string().contains("symbolic link"), "{below}: {err}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlinked_spill_directory_is_rejected() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let containers = TaskContainers::local(temp.path().join("tasks")).unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let task = Uuid::new_v4();
        let task_dir = containers.root().join(task.to_string());
        fs::create_dir_all(task_dir.join(".dhi")).unwrap();
        symlink(&outside, task_dir.join(".dhi/spill")).unwrap();

        let err = containers
            .ensure(task)
            .await
            .expect_err("a symlinked spill directory is refused");
        assert!(err.to_string().contains("symbolic link"), "{err}");
    }

    #[tokio::test]
    async fn seen_is_separate_between_tasks() {
        let temp = tempfile::tempdir().unwrap();
        let containers = TaskContainers::local(temp.path()).unwrap();
        let task = Uuid::new_v4();
        let path = Path::new("/a/file.txt");

        let first = containers.ensure(task).await.unwrap();
        let second = containers.ensure(task).await.unwrap();
        assert!(Arc::ptr_eq(&first, &second), "one runtime per task");
        first.seen.saw_whole(path, "hello");

        let (_, text) = second
            .seen
            .text(path)
            .expect("the same task shares one Seen");
        assert_eq!(&*text, "hello");

        let other = containers.ensure(Uuid::new_v4()).await.unwrap();
        assert!(
            other.seen.text(path).is_none(),
            "a different task has its own Seen"
        );
    }

    #[tokio::test]
    async fn spill_points_into_the_task_directory() {
        let temp = tempfile::tempdir().unwrap();
        let containers = TaskContainers::local(temp.path()).unwrap();
        let task = Uuid::new_v4();

        let runtime = containers.ensure(task).await.unwrap();
        assert_eq!(runtime.spill.0, runtime.host_dir.join(".dhi/spill"));
    }

    #[tokio::test]
    async fn project_mode_creates_the_task_folder_and_gitignore_without_deleting_on_shutdown() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("AGENTS.md"), "project rules").unwrap();
        let task = Uuid::new_v4();
        let containers = TaskContainers::local(temp.path().join("tasks")).unwrap();
        containers.set_task_project(task, Some(&project)).unwrap();

        let runtime = containers.ensure(task).await.unwrap();
        let canonical_project = fs::canonicalize(&project).unwrap();
        let expected = canonical_project.join(".dhi").join(task.to_string());
        assert_eq!(runtime.host_dir, expected);
        assert_eq!(runtime.base, expected);
        assert!(expected.join(".home").is_dir());
        assert!(expected.join(".dhi/spill").is_dir());

        let gitignore = canonical_project.join(".dhi").join(".gitignore");
        assert_eq!(fs::read_to_string(&gitignore).unwrap(), "*\n");

        // An existing .gitignore is never overwritten.
        fs::write(&gitignore, "custom\n").unwrap();
        containers.remove(task).await.unwrap();
        containers.ensure(task).await.unwrap();
        assert_eq!(fs::read_to_string(&gitignore).unwrap(), "custom\n");

        containers.shutdown().await.unwrap();
        assert!(expected.is_dir(), "shutdown keeps the task folder");
        assert!(
            expected.join(".dhi/spill").is_dir(),
            "shutdown keeps the task layout"
        );
        assert_eq!(fs::read_to_string(&gitignore).unwrap(), "custom\n");
        assert_eq!(
            fs::read_to_string(canonical_project.join("AGENTS.md")).unwrap(),
            "project rules"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlinked_project_dot_dhi_is_rejected() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, project.join(".dhi")).unwrap();
        let task = Uuid::new_v4();

        let err =
            prepare_project_task_dir(&project, task).expect_err("a symlinked .dhi is refused");
        assert!(err.to_string().contains("symbolic link"), "{err}");
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn an_unusable_project_folder_never_falls_back_to_legacy_mode() {
        let temp = tempfile::tempdir().unwrap();
        let containers = TaskContainers::local(temp.path().join("tasks")).unwrap();
        let task = Uuid::new_v4();
        let missing = temp.path().join("missing-project");

        let err = containers
            .set_task_project(task, Some(&missing))
            .expect_err("a missing project folder is refused");
        assert!(err.to_string().contains("cannot be inspected"), "{err}");

        let err = containers
            .ensure(task)
            .await
            .expect_err("the task stays in project mode and fails");
        assert!(err.to_string().contains("project"), "{err}");
        assert!(
            !containers.root().join(task.to_string()).exists(),
            "no legacy task folder may be created for a project task"
        );
    }

    #[test]
    fn project_mounts_use_the_project_task_folder_and_read_only_root() {
        let task = Uuid::new_v4();
        let root = Path::new("/app/workspace");
        let host = Path::new("/srv/dhi/workspace");
        let project = Path::new("/srv/repos/build");

        let mounts = task_mounts(root, Some(host), task, Some(project));

        assert_eq!(mounts.work, project.join(".dhi").join(task.to_string()));
        assert_eq!(
            mounts.dhi,
            project.join(".dhi").join(task.to_string()).join(".dhi")
        );
        assert_eq!(
            mounts.spill,
            project
                .join(".dhi")
                .join(task.to_string())
                .join(".dhi")
                .join("spill")
        );
        assert_eq!(mounts.project.as_deref(), Some(project));
        assert_eq!(PROJECT_DHI_TMPFS, "/project/.dhi:ro,noexec,nosuid,size=64k");
        assert!(
            PROJECT_DHI_TMPFS.starts_with("/project/.dhi:ro"),
            "the sibling task folders are hidden by a read-only tmpfs"
        );
        assert_ne!(
            mounts.work,
            host.join(task.to_string()),
            "project mode never falls back to the legacy workspace root"
        );
    }

    #[test]
    fn image_only_paths_are_hidden_with_read_only_tmpfs() {
        let paths: Vec<&str> = HIDDEN_IMAGE_TMPFS
            .iter()
            .map(|spec| spec.split(':').next().expect("path before options"))
            .collect();
        assert_eq!(
            paths,
            [
                "/home/linuxbrew/.linuxbrew/Homebrew",
                "/root",
                "/workspace",
                "/opt"
            ]
        );
        for spec in HIDDEN_IMAGE_TMPFS {
            assert!(spec.contains(":ro,"), "must be read-only: {spec}");
            assert!(spec.contains("noexec"), "must not execute: {spec}");
            assert!(spec.contains("nosuid"), "must not use suid: {spec}");
        }
    }

    #[test]
    fn docker_run_command_includes_hidden_image_masks() {
        let temp = tempfile::tempdir().unwrap();
        let containers = TaskContainers::local(temp.path().join("tasks")).unwrap();
        let task = Uuid::new_v4();
        let mounts = task_mounts(containers.root(), None, task, None);
        let command = containers.docker_run_command(&container_name(task), task, &mounts);
        let args: Vec<String> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        for spec in HIDDEN_IMAGE_TMPFS {
            assert!(
                args.windows(2)
                    .any(|pair| pair[0] == "--tmpfs" && pair[1] == spec),
                "missing {spec} in {args:?}"
            );
        }
    }

    #[test]
    fn config_hash_includes_the_hidden_image_masks() {
        let material = config_hash_material(
            &ContainerConfig::default(),
            None,
            Path::new("/app/workspace"),
        );
        let text = String::from_utf8_lossy(&material);
        assert!(text.contains("hidden_image_tmpfs="));
        for spec in HIDDEN_IMAGE_TMPFS {
            assert!(text.contains(spec), "missing {spec}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn project_read_verification_refuses_symlinks_and_hidden_dhi() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let outside = temp.path().join("outside.txt");
        fs::create_dir_all(project.join(".dhi")).unwrap();
        fs::write(project.join("ok.txt"), "ok\n").unwrap();
        fs::write(&outside, "secret\n").unwrap();
        fs::write(project.join(".dhi").join("sibling.txt"), "sibling\n").unwrap();
        symlink(&outside, project.join("escape.txt")).unwrap();

        let containers = TaskContainers::local(temp.path().join("tasks")).unwrap();
        let task = Uuid::new_v4();
        containers.set_task_project(task, Some(&project)).unwrap();

        containers
            .verify_project_read(task, "/project/ok.txt")
            .await
            .expect("a regular project file is allowed");
        containers
            .verify_project_read(task, "/project/missing.txt")
            .await
            .expect("a missing path is left to the tool");
        containers
            .verify_project_read(task, "/project-x/anything")
            .await
            .expect("a path outside /project is ignored");

        for path in [
            "/project/escape.txt",
            "/project/../outside.txt",
            "/project/.dhi/sibling.txt",
        ] {
            let err = containers
                .verify_project_read(task, path)
                .await
                .expect_err("a project read may not escape or enter .dhi");
            let text = err.to_string();
            assert!(
                text.contains("outside") || text.contains("hidden"),
                "{path}: {text}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn project_read_verification_allows_the_tasks_own_work_directory() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let containers = TaskContainers::local(temp.path().join("tasks")).unwrap();
        let task = Uuid::new_v4();
        let other = Uuid::new_v4();
        containers.set_task_project(task, Some(&project)).unwrap();

        let runtime = containers.ensure(task).await.unwrap();
        let work = runtime.host_dir.clone();
        fs::write(work.join("note.md"), "own note\n").unwrap();
        containers.set_task_project(other, Some(&project)).unwrap();
        let other_work = containers.ensure(other).await.unwrap().host_dir.clone();
        fs::write(other_work.join("secret.md"), "other note\n").unwrap();

        for path in [
            work.join("note.md").to_string_lossy().into_owned(),
            // The file tools resolve a relative path against the work base
            // too, but only when its target is real; a relative name that
            // happens to name a project file is still checked as a project
            // path.
            work.join("missing.md").to_string_lossy().into_owned(),
        ] {
            containers
                .verify_project_read(task, &path)
                .await
                .unwrap_or_else(|err| panic!("{path} is the task's own work: {err}"));
        }
        containers
            .verify_project_read(task, &work.to_string_lossy())
            .await
            .expect("the work directory itself is allowed");
        // A relative name is resolved against the work directory, so a
        // relative path that does not climb out of it is allowed as well.
        containers
            .verify_project_read(task, "note.md")
            .await
            .expect("a relative work path is allowed");

        let root = fs::canonicalize(&project).unwrap();
        for path in [
            root.join(".dhi").to_string_lossy().into_owned(),
            other_work.join("secret.md").to_string_lossy().into_owned(),
            other_work.to_string_lossy().into_owned(),
            work.join(".dhi/spill/x").to_string_lossy().into_owned(),
        ] {
            containers
                .verify_project_read(task, &path)
                .await
                .expect_err("another task's .dhi area stays hidden");
        }

        // A missing file under the project `.dhi` is resolved through its
        // existing parent and still refused.
        let err = containers
            .verify_project_read(
                task,
                &format!("{}/other/missing.md", root.join(".dhi").display()),
            )
            .await
            .expect_err("a missing file below .dhi is still hidden");
        assert!(err.to_string().contains("hidden"), "{err}");
    }

    #[test]
    fn docker_mounts_follow_the_host_root() {
        let task = Uuid::new_v4();
        let root = Path::new("/app/workspace");
        let host = Path::new("/srv/dhi/workspace");

        let mounts = task_mounts(root, Some(host), task, None);

        assert_eq!(mounts.work, host.join(task.to_string()));
        assert_eq!(mounts.dhi, host.join(task.to_string()).join(".dhi"));
        assert_eq!(
            mounts.spill,
            host.join(task.to_string()).join(".dhi").join("spill")
        );
        assert_eq!(mounts.project, None);
    }

    #[test]
    fn docker_mounts_without_a_host_root_use_the_task_root() {
        let task = Uuid::new_v4();
        let root = Path::new("/app/workspace");

        let mounts = task_mounts(root, None, task, None);

        assert_eq!(mounts.work, root.join(task.to_string()));
        assert_eq!(mounts.dhi, root.join(task.to_string()).join(".dhi"));
        assert_eq!(mounts.spill, root.join(task.to_string()).join(".dhi/spill"));
        assert_eq!(mounts.project, None);
    }

    #[test]
    fn a_host_root_must_be_an_absolute_non_empty_path() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        for invalid in ["", "relative/workspace"] {
            let containers = TaskContainers::new(&root, ContainerConfig::default()).unwrap();
            let err = match containers.with_host_root(invalid) {
                Ok(_) => panic!("{invalid:?} is not an absolute host root"),
                Err(err) => err,
            };
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{invalid:?}");
        }
    }

    #[test]
    fn a_host_root_changes_the_container_configuration() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        let plain = TaskContainers::new(&root, ContainerConfig::default()).unwrap();
        let host_root = temp.path().join("host-workspace");
        let mapped = TaskContainers::new(&root, ContainerConfig::default())
            .unwrap()
            .with_host_root(&host_root)
            .unwrap();

        assert_eq!(mapped.host_root.as_deref(), Some(host_root.as_path()));
        assert_ne!(plain.config_hash, mapped.config_hash);
        assert_eq!(
            mapped.config_hash,
            config_hash(
                &mapped.config,
                mapped.user.as_deref(),
                mapped.host_root.as_deref().unwrap()
            )
        );
    }

    #[tokio::test]
    async fn remove_drops_the_cached_runtime() {
        let temp = tempfile::tempdir().unwrap();
        let containers = TaskContainers::local(temp.path()).unwrap();
        let task = Uuid::new_v4();

        let first = containers.ensure(task).await.unwrap();
        containers.remove(task).await.unwrap();
        let second = containers.ensure(task).await.unwrap();
        assert!(!Arc::ptr_eq(&first, &second), "remove dropped the cache");
    }

    const CONFIG_VARIABLES: [&str; 5] = [
        "DHI_EXEC_IMAGE",
        "DHI_EXEC_NETWORK",
        "DHI_EXEC_MEMORY",
        "DHI_EXEC_CPUS",
        "DHI_EXEC_PIDS_LIMIT",
    ];

    #[tokio::test]
    async fn shutdown_forgets_runtimes_and_refuses_new_ones() {
        let temp = tempfile::tempdir().unwrap();
        let containers = TaskContainers::local(temp.path()).unwrap();
        let task = Uuid::new_v4();

        containers.ensure(task).await.unwrap();
        containers.shutdown().await.unwrap();

        let err = containers
            .ensure(task)
            .await
            .expect_err("a shut-down manager starts nothing");
        assert!(err.to_string().contains("shut down"), "{err}");
    }

    /// A profile file so a sandbox manager can be built without Docker.
    fn sandbox_config_in(temp: &Path) -> SandboxConfig {
        let profile = temp.join("task.sb");
        fs::write(&profile, "(version 1)\n(allow default)\n").unwrap();
        SandboxConfig { profile }
    }

    /// A pidfile value that no live process has, so a reap examines it and
    /// signals nothing.
    const ABSENT_PID: &str = "999999";

    /// Writes `name` pidfiles below `root/pids`, each naming a leader that is
    /// gone with its group already empty, so a reap may remove it at once.
    fn stale_pidfiles(root: &Path, name: impl Fn(usize) -> String, count: usize) {
        let dir = root.join("pids");
        fs::create_dir_all(&dir).unwrap();
        for index in 0..count {
            // A pid far above the pid ceiling names no live process and no
            // live group, so the record is handled without signalling.
            fs::write(dir.join(name(index)), "999999\n").unwrap();
        }
    }

    /// The `*.pid` files still below `root/pids`.
    fn remaining_pidfiles(root: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(root.join("pids"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".pid"))
            .collect();
        names.sort();
        names
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn startup_reap_stops_at_the_pidfile_cap() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        let containers = TaskContainers::sandbox(&root, sandbox_config_in(temp.path())).unwrap();
        let total = REAP_PIDFILE_LIMIT + 3;
        stale_pidfiles(root.as_path(), |index| format!("{index:04}.pid"), total);

        containers.reap_orphans().await.unwrap();

        let remaining = remaining_pidfiles(root.as_path());
        assert_eq!(
            remaining.len(),
            total - REAP_PIDFILE_LIMIT,
            "exactly the cap is processed per startup: {remaining:?}"
        );
        assert_eq!(
            REAP_PIDFILE_LIMIT, 256,
            "the startup bound on pidfiles is 256"
        );
        // The cap is a count, not a name range: directory order differs, so
        // the files past the cap are the ones not processed. A second reap
        // drains the backlog instead of losing it.
        let second = containers.reap_sandbox().unwrap();
        assert_eq!(
            second,
            ReapReport {
                processed: 3,
                skipped: 0,
                failed: 0
            },
            "the next startup finishes the backlog"
        );
        assert!(remaining_pidfiles(root.as_path()).is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_missing_pid_directory_is_success() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        let containers = TaskContainers::sandbox(&root, sandbox_config_in(temp.path())).unwrap();

        assert!(
            !root.join("pids").exists() || remaining_pidfiles(root.as_path()).is_empty(),
            "the layout holds no pidfile to reap"
        );
        assert_eq!(
            containers.reap_sandbox().unwrap(),
            ReapReport::default(),
            "nothing to process is not an error"
        );
        containers.reap_orphans().await.unwrap();
        containers.shutdown().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_pidfile_that_cannot_be_read_is_reported_and_does_not_stop_the_reap() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        let containers = TaskContainers::sandbox(&root, sandbox_config_in(temp.path())).unwrap();
        let pids = root.join("pids");
        fs::create_dir_all(&pids).unwrap();
        // A directory named `x.pid` cannot be read as a pidfile.
        fs::create_dir(pids.join("broken.pid")).unwrap();
        fs::write(pids.join("stale.pid"), format!("{ABSENT_PID}\n")).unwrap();

        let report = containers.reap_sandbox().unwrap();

        assert_eq!(report.processed, 2, "both pidfiles were examined");
        assert_eq!(report.failed, 1, "the unreadable one is counted");
        assert!(
            !pids.join("stale.pid").exists(),
            "the readable one is reaped"
        );
        // The failure is reported, not returned, so shutdown still succeeds.
        containers.shutdown().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_reaps_a_pidfile_whose_runtime_is_no_longer_cached() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        let containers = TaskContainers::sandbox(&root, sandbox_config_in(temp.path())).unwrap();
        let task = Uuid::new_v4();
        let runtime = containers.ensure(task).await.unwrap();
        let pidfile = runtime
            .sandbox
            .as_ref()
            .expect("a sandbox runtime")
            .pidfile
            .clone();
        // The task's cache entry is forgotten, as a replaced runtime leaves it,
        // while a stale pidfile still names a group that is already empty.
        fs::write(&pidfile, format!("{ABSENT_PID}\n")).unwrap();
        containers
            .cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();

        containers.shutdown().await.unwrap();

        assert!(
            !pidfile.exists(),
            "a clean shutdown leaves no pidfile behind: {}",
            pidfile.display()
        );
    }

    #[test]
    #[serial_test::serial]
    fn the_container_config_defaults_are_small_and_offline() {
        let guard = EnvGuard::new(&CONFIG_VARIABLES);
        guard.clear();

        let config = ContainerConfig::default();

        assert_eq!(config.image, "python:3.12-slim");
        assert_eq!(config.network, "none");
        assert_eq!(config.memory, "2g");
        assert_eq!(config.cpus, "2");
        assert_eq!(config.pids_limit, 512);
        assert_eq!(
            container_name(Uuid::nil()),
            "dhi-task-00000000-0000-0000-0000-000000000000"
        );
    }

    #[test]
    #[serial_test::serial]
    fn the_container_config_reads_environment_overrides() {
        let _guard = EnvGuard::new(&CONFIG_VARIABLES);
        // SAFETY: this test is serial; the guard restores every value.
        unsafe {
            std::env::set_var("DHI_EXEC_IMAGE", "example/image:test");
            std::env::set_var("DHI_EXEC_NETWORK", "bridge");
            std::env::set_var("DHI_EXEC_MEMORY", "512m");
            std::env::set_var("DHI_EXEC_CPUS", "0.5");
            std::env::set_var("DHI_EXEC_PIDS_LIMIT", "64");
        }

        let config = ContainerConfig::default();
        assert_eq!(config.image, "example/image:test");
        assert_eq!(config.network, "bridge");
        assert_eq!(config.memory, "512m");
        assert_eq!(config.cpus, "0.5");
        assert_eq!(config.pids_limit, 64);
    }

    #[test]
    #[serial_test::serial]
    fn a_bad_pids_limit_falls_back_to_the_default() {
        let _guard = EnvGuard::new(&["DHI_EXEC_PIDS_LIMIT"]);
        // SAFETY: this test is serial and the guard restores the value.
        unsafe { std::env::set_var("DHI_EXEC_PIDS_LIMIT", "not-a-number") };

        let config = ContainerConfig::default();

        assert_eq!(config.pids_limit, 512);
    }

    fn sandbox_fixture() -> SandboxRuntime {
        SandboxRuntime {
            profile: PathBuf::from("/etc/dhi/task.sb"),
            task_id: Uuid::nil(),
            host_work: PathBuf::from("/var/dhi/tasks/t1"),
            host_project: Some(PathBuf::from("/Users/example/project")),
            pidfile: PathBuf::from("/var/dhi/tasks/pids/t1.pid"),
        }
    }

    fn sandbox_definitions(prefix: &[OsString]) -> Vec<String> {
        let values: Vec<String> = prefix
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        values
            .iter()
            .enumerate()
            .filter(|(index, _)| *index > 0 && values[*index - 1] == "-D")
            .map(|(_, value)| value.clone())
            .collect()
    }

    #[test]
    fn sandbox_prefix_uses_the_same_user_and_real_paths() {
        let sandbox = sandbox_fixture();

        let prefix = sandbox_prefix(&sandbox);
        let args: Vec<String> = prefix
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        assert_eq!(args[0], "/usr/bin/env");
        assert!(!args.iter().any(|arg| arg == "sudo"), "no sudo: {args:?}");
        assert!(
            !args.iter().any(|arg| arg == "-u"),
            "no task user switch: {args:?}"
        );
        assert!(
            args.contains(&"DHI_TASK_ID=00000000-0000-0000-0000-000000000000".to_owned()),
            "DHI_TASK_ID is exported for diagnosis only: {args:?}"
        );
        assert!(args.contains(&"PATH=/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:/usr/local/sbin:/usr/bin:/bin:/usr/sbin:/sbin".to_owned()));
        assert!(args.contains(&"HOME=/var/dhi/tasks/t1/.home".to_owned()));
        assert!(args.contains(&"TMPDIR=/var/dhi/tasks/t1/.tmp".to_owned()));
        assert!(args.contains(&"CARGO_HOME=/var/dhi/tasks/t1/.cargo".to_owned()));
        assert!(args.contains(&"GIT_CONFIG_NOSYSTEM=1".to_owned()));
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "-f" && pair[1] == "/etc/dhi/task.sb"),
            "the profile is passed to sandbox-exec: {args:?}"
        );
        assert!(
            args.contains(&"/usr/bin/sandbox-exec".to_owned()),
            "sandbox-exec runs the command: {args:?}"
        );
        assert_eq!(
            sandbox_definitions(&prefix),
            [
                "PROJECT=/Users/example/project",
                "WORK=/var/dhi/tasks/t1",
                "SPILL=/var/dhi/tasks/t1/.dhi",
                "PIDFILE=/var/dhi/tasks/pids/t1.pid"
            ]
        );
        assert!(
            args.windows(4).any(|window| {
                window[0] == "/bin/sh"
                    && window[1] == "-c"
                    && window[2] == "printf '%s\\n' \"$$\" >> \"$1\"; shift; \"$@\""
                    && window[3] == "dhi-pids"
            }),
            "the wrapper records the session leader and waits for the command: {args:?}"
        );
        assert!(
            !args.iter().any(|arg| arg.contains("exec \"$@\"")),
            "the wrapper must not exec, so the leader keeps its command line: {args:?}"
        );
        assert_eq!(
            args[args.len() - 2..],
            ["dhi-pids", "/var/dhi/tasks/pids/t1.pid"],
            "the wrapper waits for the command the Shell tool appends after the pidfile: {args:?}"
        );

        let legacy = SandboxRuntime {
            host_project: None,
            ..sandbox.clone()
        };
        assert_eq!(
            sandbox_definitions(&sandbox_prefix(&legacy))[0],
            "PROJECT=/var/dhi/tasks/t1"
        );
    }

    /// The paths the profile matches are canonical: no argument carries a
    /// `/.` or `/../` segment, whatever components the runtime was built with.
    #[test]
    fn sandbox_prefix_hands_the_profile_canonical_paths() {
        let sandbox = SandboxRuntime {
            profile: PathBuf::from("/etc/dhi/task.sb"),
            task_id: Uuid::nil(),
            // `Path::new` keeps `..` (only `.` is dropped while parsing), and
            // the doubled slash stays until a component walk.
            host_work: PathBuf::from("/var/dhi/tasks/t1/..//t1"),
            host_project: Some(PathBuf::from("/Users/example/../example/project")),
            pidfile: PathBuf::from("/var/dhi/tasks/../tasks/pids/t1.pid"),
        };

        let prefix = sandbox_prefix(&sandbox);
        let args: Vec<String> = prefix
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        // The runtime this test builds really is not canonical, so the
        // checks below would fail without the normalization.
        assert!(
            sandbox
                .host_work
                .components()
                .any(|component| component == Component::ParentDir),
            "the fixture must start out non-canonical"
        );

        // The profile-bound paths are what the profile matches, so they are
        // the ones that must be canonical. HOME, TMPDIR and CARGO_HOME
        // legitimately end in a dot directory (`/.home`).
        assert_eq!(
            sandbox_definitions(&prefix),
            [
                "PROJECT=/Users/example/project",
                "WORK=/var/dhi/tasks/t1",
                "SPILL=/var/dhi/tasks/t1/.dhi",
                "PIDFILE=/var/dhi/tasks/pids/t1.pid"
            ]
        );
        let definitions = sandbox_definitions(&prefix);
        for argument in &definitions {
            assert!(!argument.contains("/./"), "{argument} is not canonical");
            assert!(!argument.contains("/../"), "{argument} is not canonical");
        }
        assert!(args.contains(&"HOME=/var/dhi/tasks/t1/.home".to_owned()));
        assert!(args.contains(&"TMPDIR=/var/dhi/tasks/t1/.tmp".to_owned()));
        assert!(args.contains(&"CARGO_HOME=/var/dhi/tasks/t1/.cargo".to_owned()));
        assert_eq!(
            args.last().map(String::as_str),
            Some("/var/dhi/tasks/pids/t1.pid"),
            "the wrapper records into the canonical pidfile: {args:?}"
        );
    }

    /// `PROJECT`, `WORK`, `SPILL` and `PIDFILE` carry no `.` or `..`
    /// component, whatever path spelling a caller built them from.
    #[test]
    fn sandbox_paths_have_no_dot_or_parent_component() {
        let project = Path::new("/Users/example/../example/./project");
        let work = Path::new("//var//dhi/tasks/t1/../t1/./");
        let pidfile = Path::new("/var/dhi/tasks/./pids/../pids/t1.pid");
        for (path, label) in [(project, "project"), (work, "work"), (pidfile, "pidfile")] {
            let normalized = canonical_path(path, label);
            assert!(
                normalized.components().all(|component| matches!(
                    component,
                    Component::RootDir | Component::Normal(_)
                )),
                "{path:?} became {normalized:?}, which has a dot or parent component"
            );
        }
        assert_eq!(
            canonical_path(project, "project"),
            Path::new("/Users/example/project")
        );
        assert_eq!(canonical_path(work, "work"), Path::new("/var/dhi/tasks/t1"));
        assert_eq!(
            canonical_path(pidfile, "pidfile"),
            Path::new("/var/dhi/tasks/pids/t1.pid")
        );
        assert_eq!(canonical_path(Path::new("/"), "root"), Path::new("/"));
        assert_eq!(
            canonical_path(Path::new("/a/../.."), "root"),
            Path::new("/")
        );
    }

    #[test]
    #[should_panic(expected = "must be an absolute path")]
    fn a_relative_sandbox_path_is_a_contract_violation() {
        canonical_path(Path::new("var/dhi/tasks"), "work");
    }

    #[test]
    fn parse_pidfile_keeps_distinct_positive_pids_only() {
        assert_eq!(parse_pidfile("123\n456\n"), [123, 456]);
        assert_eq!(parse_pidfile(" 42 \n\n0\n1\n-5\nnot-a-pid\n7"), [42, 7]);
        assert_eq!(parse_pidfile("7\n7\n8\n7\n"), [7, 8], "pids are deduped");
        assert_eq!(parse_pidfile(""), Vec::<i32>::new());
    }

    #[test]
    fn only_the_pidfile_wrapper_is_a_matching_leader() {
        let pidfile = Path::new("/var/dhi/tasks/pids/t1.pid");
        let wrapper = "sh -c printf \"%s\\n\" \"$$\" >> \"$1\"; shift; \"$@\" dhi-pids /var/dhi/tasks/pids/t1.pid /bin/sleep 30";
        assert!(is_pidfile_leader(wrapper, pidfile));
        assert!(
            !is_pidfile_leader(&wrapper.replace("t1.pid", "t2.pid"), pidfile),
            "another task's pidfile is not this pidfile"
        );
        assert!(
            !is_pidfile_leader("/bin/sleep 30", pidfile),
            "an unrelated command is never a leader"
        );
        assert!(!is_pidfile_leader(wrapper, Path::new("")));
    }

    /// Kills and reaps the child on every path, so a failed assertion does not
    /// leave a `sleep` behind.
    #[cfg(unix)]
    struct ChildGuard(std::process::Child);

    #[cfg(unix)]
    impl ChildGuard {
        fn child(&mut self) -> &mut std::process::Child {
            &mut self.0
        }

        /// Waits up to five seconds for the child to exit.
        fn wait_exit(&mut self, what: &str) {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let exited = self
                    .child()
                    .try_wait()
                    .expect("cannot poll the test child")
                    .is_some();
                if exited {
                    return;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "{what} is still running"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    #[cfg(unix)]
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if let Ok(Some(_)) = self.0.try_wait() {
                return;
            }
            // Kill the whole group as well as the leader, so a test that
            // failed mid-way does not leave a grandchild behind.
            // SAFETY: a negative pid names the child's own process group.
            unsafe { libc::kill(-(self.0.id() as i32), libc::SIGKILL) };
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Kills the group of a left-over process on every path, so a failed
    /// test does not keep it alive.
    #[cfg(unix)]
    struct GroupCleanup(i32);

    #[cfg(unix)]
    impl Drop for GroupCleanup {
        fn drop(&mut self) {
            if self.0 > 1 {
                let _ = kill_process_group(self.0);
            }
        }
    }

    /// Spawns `program` in a new session, so the child leads its own process
    /// group.
    fn spawn_session_leader(program: &str, args: &[&str]) -> ChildGuard {
        use std::os::unix::process::CommandExt;

        let mut command = StdCommand::new(program);
        command.args(args);
        // SAFETY: setsid is async-signal-safe and runs in the child before
        // exec, so the child becomes the leader of a new process group.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        ChildGuard(command.spawn().expect("cannot start the session leader"))
    }

    /// A wrapper command line as [`sandbox_prefix`] builds it, minus
    /// `sandbox-exec`: the wrapper writes its pid and waits for the command
    /// as a child, so its command line keeps `dhi-pids <pidfile>`.
    #[cfg(unix)]
    fn wrapper_args<'a>(pidfile: &'a str, command: &[&'a str]) -> Vec<&'a str> {
        let mut args = vec![
            "-c",
            "printf '%s\\n' \"$$\" >> \"$1\"; shift; \"$@\"",
            "dhi-pids",
            pidfile,
        ];
        args.extend_from_slice(command);
        args
    }

    /// Starts a session leader, retrying while the system refuses.
    ///
    /// The test binary runs tests in parallel threads, and a child started
    /// from a thread cannot call `setsid` while another thread holds the
    /// internal `posix_spawn` lock, so `spawn` may fail with `EAGAIN`. A
    /// retry after a short wait is the documented workaround.
    #[cfg(unix)]
    fn guard_started(what: &str, mut start: impl FnMut() -> ChildGuard) -> ChildGuard {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(&mut start)) {
                Ok(guard) => return guard,
                Err(payload) => {
                    let message = payload
                        .downcast_ref::<String>()
                        .cloned()
                        .unwrap_or_else(|| "session start failed".to_owned());
                    if !message.contains("Resource temporarily unavailable") {
                        std::panic::panic_any(message);
                    }
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{what} could not get a session lock"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn kill_pidfile_group_stops_a_group_led_by_the_pidfile_wrapper() {
        let temp = tempfile::tempdir().unwrap();
        let pidfile = temp.path().join("task.pid");
        kill_pidfile_group(&temp.path().join("missing.pid")).expect("a missing pidfile is fine");

        let mut guard = guard_started("the pidfile wrapper", || {
            spawn_session_leader(
                "/bin/sh",
                &wrapper_args(pidfile.to_str().unwrap(), &["/bin/sleep", "30"]),
            )
        });
        let pid = guard.child().id() as i32;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while fs::read_to_string(&pidfile).map_or(true, |text| text.trim() != pid.to_string()) {
            assert!(
                std::time::Instant::now() < deadline,
                "the wrapper did not record its pid"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let command = leader_command(pid).unwrap().expect("the leader is alive");
        assert!(
            is_pidfile_leader(&command, &pidfile),
            "the leader keeps its wrapper command line: {command}"
        );

        kill_pidfile_group(&pidfile).expect("the recorded group is killed");

        guard.wait_exit("the pidfile wrapper");
        assert!(
            !pidfile.exists(),
            "kill_pidfile_group removes the pidfile it read"
        );
    }

    /// A small C program that produces the leader-gone case exactly: it
    /// starts a session, its leader forks a grandchild that keeps the group
    /// and reports its pid to `<argv[1]>`, waits until the report is written,
    /// and then exits.
    ///
    /// The pipe is what makes the case reliable: without it the leader can
    /// exit and the whole program return before the grandchild has even
    /// opened its file. `sh` is not used for this at all, because a macOS
    /// shell reaps and kills its background jobs before it exits, so the
    /// group would already be empty.
    #[cfg(target_os = "macos")]
    const LEADER_GONE_SOURCE: &str = r#"
#include <fcntl.h>
#include <stdio.h>
#include <sys/wait.h>
#include <unistd.h>
int main(int argc, char **argv) {
    (void)argc;
    int ready[2];
    if (pipe(ready) != 0) {
        return 1;
    }
    pid_t leader = fork();
    if (leader == 0) {
        setsid();
        pid_t grandchild = fork();
        if (grandchild == 0) {
            close(ready[0]);
            int fd = open(argv[1], O_WRONLY | O_CREAT | O_TRUNC, 0644);
            if (fd >= 0) {
                dprintf(fd, "%d\n", getpid());
                close(fd);
            }
            if (write(ready[1], "r", 1) != 1) {
                _exit(2);
            }
            close(ready[1]);
            close(0);
            close(1);
            close(2);
            sleep(30);
            _exit(0);
        }
        close(ready[1]);
        char ready_byte;
        if (read(ready[0], &ready_byte, 1) != 1) {
            _exit(3);
        }
        _exit(0);
    }
    close(ready[1]);
    char ready_byte;
    if (read(ready[0], &ready_byte, 1) != 1) {
        return 1;
    }
    int status;
    waitpid(leader, &status, 0);
    return 0;
}
"#;

    /// Compiles [`LEADER_GONE_SOURCE`] into `dir` and returns its path.
    ///
    /// A compiler or build failure panics: every macOS host has `cc`, and a
    /// helper that does not build would make the test pass without ever
    /// producing the leader-gone case.
    #[cfg(target_os = "macos")]
    fn build_leader_gone(dir: &Path) -> PathBuf {
        let source = dir.join("leader_gone.c");
        fs::write(&source, LEADER_GONE_SOURCE).unwrap();
        let binary = dir.join("leader_gone");
        let built = StdCommand::new("/usr/bin/cc")
            .arg("-O0")
            .arg("-o")
            .arg(&binary)
            .arg(&source)
            .output()
            .expect("cannot run /usr/bin/cc to build the leader-gone helper");
        assert!(
            built.status.success(),
            "cannot build the leader-gone helper: {}",
            stderr_of(&built)
        );
        assert!(binary.is_file(), "the leader-gone helper was not written");
        eprintln!("leader-gone helper built at {}", binary.display());
        binary
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn kill_pidfile_group_stops_a_group_whose_leader_is_gone() {
        let temp = tempfile::tempdir().unwrap();
        let helper = build_leader_gone(temp.path());
        let pidfile = temp.path().join("task.pid");
        let grandchild_file = temp.path().join("grandchild.pid");

        let status = StdCommand::new(&helper)
            .arg(&grandchild_file)
            .status()
            .expect("cannot start the leader-gone helper");
        assert!(status.success(), "the leader-gone helper failed: {status}");
        let grandchild: i32 = fs::read_to_string(&grandchild_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut cleanup = GroupCleanup(grandchild);
        let leader = process_group_of(grandchild).expect("the grandchild's group id");
        eprintln!("leader-gone helper left grandchild {grandchild} in group {leader}, leader gone");
        // The grandchild kept the leader's process group, so the group id is
        // the dead leader's pid, not the grandchild's own pid.
        assert_eq!(process_group_of(grandchild), Some(leader));
        assert!(
            process_group_exists(leader).unwrap(),
            "the grandchild keeps the leader's group alive"
        );
        assert_eq!(
            leader_command(leader).unwrap(),
            None,
            "the recorded leader has exited"
        );

        // A real pidfile names the leader that is gone, while its group lives.
        fs::write(&pidfile, format!("{leader}\n{leader}\n1\n0\n")).unwrap();

        kill_pidfile_group(&pidfile).expect("the left-over group is killed");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while process_group_exists(leader).unwrap() {
            assert!(
                std::time::Instant::now() < deadline,
                "the grandchild is still in its group"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        // The recorded group is empty, so the grandchild itself is gone too.
        assert!(
            leader_command(grandchild).unwrap().is_none(),
            "the grandchild was killed with its group"
        );
        cleanup.0 = 0;
        assert!(!pidfile.exists(), "the pidfile is removed");
    }

    #[cfg(unix)]
    #[test]
    fn a_reused_pid_whose_leader_is_not_the_wrapper_is_left_alone() {
        let temp = tempfile::tempdir().unwrap();
        let pidfile = temp.path().join("task.pid");

        // A live process in its own session whose command line says nothing
        // about `dhi-pids`: the pid of a group that was reused after the
        // recorded leader died.
        let mut guard = guard_started("a foreign session leader", || {
            spawn_session_leader("/bin/sleep", &["30"])
        });
        fs::write(&pidfile, format!("{}\n", guard.child().id())).unwrap();

        kill_pidfile_group(&pidfile).expect("a foreign leader is not an error");

        assert!(
            guard.child().try_wait().unwrap().is_none(),
            "a live pid whose leader is not the pidfile wrapper must not be killed"
        );
        assert!(
            !pidfile.exists(),
            "the pidfile is removed even when its record is not acted on"
        );
    }
}
