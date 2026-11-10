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

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Output;
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

/// Everything the backend needs to run one task's tools.
///
/// In container mode [`base`](Self::base) is `/work`, the workspace is a
/// [`Remote`] over `docker exec`, and [`host_dir`](Self::host_dir) is the
/// one host directory mounted there. In local test mode the base is the
/// host task directory, the workspace is [`Local`], and the host is still
/// reachable.
#[derive(Debug, Clone)]
pub struct TaskRuntime {
    /// The task's directory on the host: `root/<task-id>` in legacy mode, or
    /// `<project>/.dhi/<task-id>` in project mode.
    pub host_dir: PathBuf,
    /// The directory the tools resolve paths against: `/work` in container
    /// mode, the host task directory in local mode.
    pub base: PathBuf,
    /// Where every file operation and shell command runs.
    pub workspace: Arc<dyn Workspace>,
    /// The project root as the tools name it: `/project` in container mode,
    /// the host project directory in local test mode. `None` is legacy mode.
    pub project_dir: Option<PathBuf>,
    /// The [`Seen`] shared by this task's reads, writes and edits.
    pub seen: Seen,
    /// Where this task's cut output is kept.
    pub spill: Spill,
    /// Whether the tools run in a container (`true`) or locally (`false`).
    pub is_container: bool,
}

/// Which kind of runtime this manager builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// One Docker container per task.
    Docker,
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
    cache: Mutex<HashMap<Uuid, Arc<TaskRuntime>>>,
    /// One async mutex per task that serializes `ensure` and `remove`.
    starting: Mutex<HashMap<Uuid, Arc<AsyncMutex<()>>>>,
    /// Project folder per task. A task in this map gets
    /// `<project>/.dhi/<task-id>` as its host work directory and a read-only
    /// `/project` mount; a task without an entry keeps legacy mode.
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
        Self::build(root.into(), config, Mode::Docker)
    }

    /// A manager that uses [`Local`] instead of Docker. Tests only.
    ///
    /// It creates and checks the same host directory layout, so the file
    /// tools and shell run without a Docker daemon, but the host is not
    /// isolated.
    pub fn local(root: impl Into<PathBuf>) -> io::Result<Self> {
        Self::build(root.into(), ContainerConfig::default(), Mode::Local)
    }

    fn build(requested: PathBuf, config: ContainerConfig, mode: Mode) -> io::Result<Self> {
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

    /// Refuses a `/project` read whose real target is outside the project or
    /// inside the hidden `.dhi` directory.
    ///
    /// In Docker mode this runs `realpath -e` inside the task container,
    /// because `rig-tools`' remote workspace canonicalizes `..` lexically and
    /// does not resolve symbolic links. In local mode `fs::canonicalize` does
    /// the same job on the host path. A missing path is left to the tool so it
    /// returns its normal not-found error. Paths outside `/project` are
    /// ignored.
    pub async fn verify_project_read(&self, task_id: Uuid, requested: &str) -> anyhow::Result<()> {
        if self.task_project(task_id).is_none() {
            return Ok(());
        }
        if project_request_relative(requested).is_none() {
            return Ok(());
        }
        match self.mode {
            Mode::Local => self.verify_local_project_read(task_id, requested),
            Mode::Docker => self.verify_docker_project_read(task_id, requested).await,
        }
    }

    fn verify_local_project_read(&self, task_id: Uuid, requested: &str) -> anyhow::Result<()> {
        let Some(project) = self.task_project(task_id) else {
            return Ok(());
        };
        let Some(relative) = project_request_relative(requested) else {
            return Ok(());
        };
        let Ok(root) = fs::canonicalize(&project) else {
            // The project disappeared after task creation; the tool will
            // report the same missing path.
            return Ok(());
        };
        let candidate = if relative.is_empty() {
            root.clone()
        } else {
            root.join(relative)
        };
        let resolved = match fs::canonicalize(&candidate) {
            Ok(resolved) => resolved,
            // A missing path is the file tool's normal not-found error.
            Err(_) => return Ok(()),
        };
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
    /// it reports running, and builds a [`Remote`] over `docker exec`. A
    /// cached runtime is returned only while its container still reports
    /// running; a missing or stopped container is replaced, and a failed
    /// check is an error rather than a stale runtime.
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

    /// Stops and removes `task_id`'s container, and forgets its runtime.
    ///
    /// The cache entry is dropped only after Docker confirms the removal, so
    /// a later [`shutdown`](Self::shutdown) can retry a failed removal. A
    /// container that never existed, or that another path already removed, is
    /// not an error. A container that this manager does not own, or that
    /// belongs to another task root, is refused and left running.
    pub async fn remove(&self, task_id: Uuid) -> anyhow::Result<()> {
        let gate = self.gate(task_id);
        let _gate = gate.lock().await;
        if self.mode == Mode::Docker {
            self.remove_container(task_id, &container_name(task_id))
                .await?;
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
    /// Local mode has no containers and does nothing. Returns how many
    /// containers were removed. A listed container that disappears before
    /// `docker rm` runs is treated as removed; any other Docker failure is
    /// returned after the remaining ids have still been attempted.
    pub async fn reap_orphans(&self) -> anyhow::Result<usize> {
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
    /// Local mode always reuses its cached runtime. In Docker mode the
    /// container is inspected first: a running container keeps the runtime,
    /// while a missing or stopped one is dropped from the cache so the caller
    /// starts a fresh one. A failed inspect is an error; the entry is kept so
    /// a later call, or `shutdown`, can retry.
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
        })
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
}
