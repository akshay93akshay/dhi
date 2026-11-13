//! One runtime per task, and the runtime the coding tools act on.
//!
//! [`TaskContainers::sandbox`] is the production mode. Each task runs as the
//! same macOS user as this process under `/usr/bin/sandbox-exec`, with its own
//! real host paths. There are no `/work` or `/project` aliases, no
//! `synthetic.conf`, no dedicated task user, no ACLs, and no `sudo`, so
//! nothing has to be installed and no reboot is needed. Because no path is
//! global, any number of sandbox tasks can run at the same time.
//!
//! [`TaskContainers::local`] is for tests only. It creates the same host
//! layout and uses [`Local`], so the host is still reachable and the path
//! checks in the backend are the control.
//!
//! A task's host directory is `root/<task-id>`, or `<project>/.dhi/<task-id>`
//! in project mode. On Unix the root, the task directory, and its `.home`,
//! `.tmp`, `.cargo` and `.dhi/spill` directories are created with mode `0700`;
//! each is checked to be a real directory and not a symbolic link before it is
//! used. Production must call [`shutdown`](TaskContainers::shutdown); there is
//! no async `Drop`, and a failed removal leaves its cache entry for a later
//! `shutdown` to retry.
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
use std::process::Command as StdCommand;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{anyhow, bail};
use rig_tools::workspace::{Local, Workspace};
use rig_tools::{Seen, Spill};
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

/// The search path a sandboxed task gets when the parent has none.
pub(crate) const DEFAULT_PATH: &str = "/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:/usr/local/sbin:/usr/bin:/bin:/usr/sbin:/sbin";

/// How many pidfiles one native sandbox reap processes.
///
/// Every pidfile costs a `ps` call per recorded pid, and startup reaps before
/// the listener binds, so a directory full of stale pidfiles must not delay
/// serving. Past this many, the rest are counted and reported and a later
/// `shutdown` or the next startup retries them.
const REAP_PIDFILE_LIMIT: usize = 256;

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
/// In native sandbox mode the base is the task's real host work directory,
/// the workspace is [`Local`], and the shell launcher wraps every command in
/// `sandbox-exec`. In local test mode the base is the host task directory, the
/// workspace is [`Local`], and the host is still reachable.
#[derive(Debug, Clone)]
pub struct TaskRuntime {
    /// The task's real host directory: `root/<task-id>` in legacy mode, or
    /// `<project>/.dhi/<task-id>` in project mode.
    pub host_dir: PathBuf,
    /// The directory the tools resolve paths against: the same real host task
    /// directory.
    pub base: PathBuf,
    /// Where every file operation runs.
    pub workspace: Arc<dyn Workspace>,
    /// The real host project directory when the task has one. `None` is
    /// legacy mode.
    pub project_dir: Option<PathBuf>,
    /// The [`Seen`] shared by this task's reads, writes and edits.
    pub seen: Seen,
    /// Where this task's cut output is kept.
    pub spill: Spill,
    /// The native macOS sandbox this runtime runs under, when it is one.
    pub sandbox: Option<SandboxRuntime>,
}

/// Which kind of runtime this manager builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// One native macOS sandbox per task.
    Sandbox,
    /// The host, for tests only.
    Local,
}

/// Creates, caches and removes the per-task runtimes.
///
/// One instance owns the task root and the sandbox configuration. Every task
/// gets at most one runtime; the runtime is cached until
/// [`remove`](Self::remove) or [`shutdown`](Self::shutdown).
pub struct TaskContainers {
    root: PathBuf,
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
    /// A manager of native macOS sandbox tasks under `root`.
    ///
    /// Every task runs as the same macOS user as this process under
    /// `/usr/bin/sandbox-exec`, with its own real host paths. The profile must
    /// already exist; nothing else is installed. Because no path is global,
    /// any number of tasks may run at the same time.
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
        Self::build(root.into(), Mode::Sandbox, Some(config))
    }

    /// A manager that runs tasks directly on the host. Tests only.
    ///
    /// It creates and checks the same host directory layout, so the file
    /// tools and shell run without a sandbox, but the host is not isolated.
    pub fn local(root: impl Into<PathBuf>) -> io::Result<Self> {
        Self::build(root.into(), Mode::Local, None)
    }

    fn build(requested: PathBuf, mode: Mode, sandbox: Option<SandboxConfig>) -> io::Result<Self> {
        create_private_dir(&requested)?;
        let root = validate_private_dir(&requested, "task root")?;
        Ok(Self {
            root,
            mode,
            sandbox,
            cache: Mutex::new(HashMap::new()),
            starting: Mutex::new(HashMap::new()),
            projects: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
        })
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
    /// `<project>/.dhi/<task-id>`. The mapping is set before the task's first
    /// tool call and is immutable for the task's life.
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
    /// `fs::canonicalize` resolves symbolic links on the host path, which the
    /// file tools do not do for a confined read. A missing path is left to the
    /// tool so it returns its normal not-found error. Paths outside the
    /// project are ignored.
    pub async fn verify_project_read(&self, task_id: Uuid, requested: &str) -> anyhow::Result<()> {
        if self.task_project(task_id).is_none() {
            return Ok(());
        }
        self.verify_local_project_read(task_id, requested)
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
        // An absolute path below the real project root names the same entry.
        let requested_path = Path::new(requested);
        if !requested_path.is_absolute() {
            return Ok(());
        }
        let Ok(relative) = requested_path.strip_prefix(&root) else {
            return Ok(());
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

    /// The runtime of `task_id`, started and cached on first use.
    ///
    /// The host task directory is always created first. In native sandbox
    /// mode it validates the task's `.dhi` and spill directories and builds a
    /// [`Local`] runtime whose shell launcher wraps every command in
    /// `sandbox-exec`. A cached runtime is always reused: a sandbox task has no
    /// global state that another task can retarget.
    pub async fn ensure(&self, task_id: Uuid) -> anyhow::Result<Arc<TaskRuntime>> {
        if self.closed.load(Ordering::SeqCst) {
            bail!("task runtimes are shut down; no new runtime may start");
        }
        if let Some(runtime) = self.cached(task_id) {
            return Ok(runtime);
        }

        let gate = self.gate(task_id);
        let _gate = gate.lock().await;
        if self.closed.load(Ordering::SeqCst) {
            bail!("task runtimes are shut down; no new runtime may start");
        }
        if let Some(runtime) = self.cached(task_id) {
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
        };
        let runtime = Arc::new(runtime);
        self.cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(task_id, Arc::clone(&runtime));
        Ok(runtime)
    }

    /// Stops `task_id`'s native sandbox processes and forgets its runtime.
    ///
    /// The cache entry is dropped only after the removal succeeds, so a later
    /// [`shutdown`](Self::shutdown) can retry a failed removal. Native sandbox
    /// removal kills each process group recorded in the task's pidfile, after
    /// the leader command line proves the pid is this task's, and leaves the
    /// task's files in place.
    pub async fn remove(&self, task_id: Uuid) -> anyhow::Result<()> {
        let gate = self.gate(task_id);
        let _gate = gate.lock().await;
        if self.mode == Mode::Sandbox {
            self.remove_sandbox(task_id)?;
        }
        self.cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&task_id);
        Ok(())
    }

    /// Replaces `task_id`'s runtime: removes it, then starts a fresh one.
    ///
    /// This is how a command that outlived its own timeout is stopped: every
    /// process group the old runtime recorded is killed before the new runtime
    /// exists. It fails if the old runtime cannot be removed, and leaves the
    /// cache entry in place for [`shutdown`](Self::shutdown) to retry.
    pub async fn restart(&self, task_id: Uuid) -> anyhow::Result<Arc<TaskRuntime>> {
        self.remove(task_id).await?;
        self.ensure(task_id).await
    }

    /// Stops every runtime this manager started, and forgets every runtime.
    /// No later [`ensure`](Self::ensure) may start another.
    ///
    /// Every failure is reported; one failing runtime does not stop the others
    /// from being removed. A failed removal keeps its cache entry, so calling
    /// `shutdown` again retries it. A task whose `ensure` is still starting is
    /// included, and its process groups are removed once that start finishes.
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
                "could not remove every task runtime:\n{}",
                failures.join("\n")
            ))
        }
    }

    /// Removes everything this task root left behind by an earlier process.
    ///
    /// This is a startup cleanup, not part of normal task life: a process that
    /// died without calling [`shutdown`](Self::shutdown) leaves its process
    /// groups behind. In native sandbox mode it reaps every pidfile below
    /// `root/pids` and kills each group it can prove is left over; at startup
    /// no task is running. Local mode has nothing to reap. Returns how many
    /// groups were stopped; a pidfile that cannot be stopped is reported as a
    /// warning by the reap instead of failing it.
    pub async fn reap_orphans(&self) -> anyhow::Result<usize> {
        if self.mode != Mode::Sandbox {
            return Ok(0);
        }
        // The report's warnings are printed by `reap_sandbox`; startup only
        // needs to know that the directory itself was readable.
        let report = self.reap_sandbox()?;
        Ok(report.processed.saturating_sub(report.failed))
    }

    fn cached(&self, task_id: Uuid) -> Option<Arc<TaskRuntime>> {
        self.cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&task_id)
            .cloned()
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
            sandbox: None,
        })
    }

    /// Builds the runtime of a task that runs as this process's user under
    /// `sandbox-exec`, with its own real host paths.
    ///
    /// The `.dhi` and spill directories are validated as private real
    /// directories, so the host spill writer is never redirected by a symbolic
    /// link planted in the task folder. A private `pids` directory below the
    /// task root holds the server-owned pidfile the wrapper fills; the file
    /// itself is created by the wrapper. Nothing global changes, so any number
    /// of tasks can be prepared at the same time.
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

/// Creates `root/<task-id>` and its private subdirectories, then validates
/// every one of them.
fn prepare_task_dir(root: &Path, task_id: Uuid) -> io::Result<PathBuf> {
    prepare_task_dir_at(&root.join(task_id.to_string()))
}

/// Creates a project-mode task folder at `<project>/.dhi/<task-id>`.
///
/// The project's `.dhi` directory and its `.gitignore` (containing `*`) are
/// created when absent. An existing `.gitignore` is never overwritten. The
/// task folder must be writable by the task's own user; a failure is reported
/// with the folder path and uid/gid.
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

/// Proves the task's user can write in the task folder.
///
/// A sandbox task runs as the same macOS user as this process, so a probe
/// written here is exactly the access the task gets.
fn check_task_folder_writable(dir: &Path) -> io::Result<()> {
    let probe = dir.join(format!(".dhi-write-check-{}", Uuid::new_v4()));
    let user = platform_user();
    if let Err(err) = fs::write(&probe, b"ok") {
        return Err(io::Error::new(
            err.kind(),
            format!(
                "task folder {} is not writable by the task user ({user}): {err}",
                dir.display()
            ),
        ));
    }
    fs::remove_file(&probe).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "task folder {} is not writable by the task user ({user}): \
                 cannot remove the write probe: {err}",
                dir.display()
            ),
        )
    })
}

/// The `uid:gid` of the user this process, and every sandbox task, runs as.
fn platform_user() -> String {
    #[cfg(unix)]
    {
        // SAFETY: getuid and getgid have no preconditions and only read this
        // process's ids.
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        format!("{uid}:{gid}")
    }
    #[cfg(not(unix))]
    {
        "the task user".to_owned()
    }
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
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn local_task_dir_creates_the_private_layout() {
        let temp = tempfile::tempdir().unwrap();
        let containers = TaskContainers::local(temp.path().join("tasks")).unwrap();
        let task = Uuid::new_v4();

        let runtime = containers.ensure(task).await.unwrap();

        assert_eq!(runtime.host_dir, containers.root().join(task.to_string()));
        assert_eq!(runtime.base, runtime.host_dir);
        assert_eq!(runtime.workspace.host(), None);
        assert!(runtime.sandbox.is_none());
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
        assert_eq!(
            runtime.project_dir.as_deref(),
            Some(canonical_project.as_path())
        );
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

        let root = fs::canonicalize(&project).unwrap();
        containers
            .verify_project_read(task, &root.join("ok.txt").to_string_lossy())
            .await
            .expect("a regular project file is allowed");
        containers
            .verify_project_read(task, &root.join("missing.txt").to_string_lossy())
            .await
            .expect("a missing path is left to the tool");
        containers
            .verify_project_read(task, "/project-x/anything")
            .await
            .expect("a path outside the project is ignored");

        for path in [
            root.join("escape.txt").to_string_lossy().into_owned(),
            format!("{}/../outside.txt", root.display()),
            root.join(".dhi/sibling.txt").to_string_lossy().into_owned(),
        ] {
            let err = containers
                .verify_project_read(task, &path)
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

    #[tokio::test]
    async fn local_reap_has_nothing_to_do() {
        let temp = tempfile::tempdir().unwrap();
        let containers = TaskContainers::local(temp.path()).unwrap();
        let task = Uuid::new_v4();
        containers.ensure(task).await.unwrap();

        assert_eq!(containers.reap_orphans().await.unwrap(), 0);
        containers.shutdown().await.unwrap();
    }

    /// A profile file so a sandbox manager can be built.
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

        assert_eq!(
            containers.reap_orphans().await.unwrap(),
            REAP_PIDFILE_LIMIT,
            "startup reports the pidfiles it processed"
        );

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
        assert_eq!(containers.reap_orphans().await.unwrap(), 0);
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
            String::from_utf8_lossy(&built.stderr)
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
