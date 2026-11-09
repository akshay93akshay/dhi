//! The [`dhi_guard::ToolBackend`] that runs the `rig-tools` implementations
//! for one task.
//!
//! A call must carry the per-task MAC stamped by
//! [`CodingTools`](crate::CodingTools) and the task id from its [`RunScope`].
//! The MAC is `HMAC-SHA256(secret, task_id)`; the backend recomputes it for
//! the task id in the request and compares before any tool runs, so a MAC
//! leaked for one task cannot authorize another, and a coding tool
//! registered through the generic [`dhi_agent::guarded_dynamic_tool`] path
//! cannot forge a task.
//!
//! In production the call is resolved to that task's container runtime and
//! every tool goes through its [`Remote`](rig_tools::remote::Remote)
//! workspace. The file tools use `/work` as their base and the shell starts
//! in `/work`; the container sees the task folder there, with `.dhi` and
//! `.dhi/spill` mounted read-only inside it. A project task can also read its
//! project root through `/project`; read-style tools are confined to that
//! root, and writes stay confined to `/work`.
//!
//! Local test mode uses [`Local`] instead. There the file tools are confined
//! to the host task directory and `write`/`edit` also refuse a target, or a
//! parent below the task directory, that is a symbolic link, including a
//! dangling one. The shell starts in the task directory with a scrubbed
//! environment, but it is **not** an OS sandbox: it runs as the service user
//! and can reach what that user can reach.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail};
use async_trait::async_trait;
use dhi_guard::ToolBackend;
use rig::tool::{Tool, ToolContext};
use rig_core::tool::ExitStatus;
use rig_tools::process::Launcher;
use rig_tools::workspace::{Local, Workspace};
use rig_tools::{
    Edit, EditArgs, List, ListArgs, Read, ReadArgs, Search, SearchArgs, ShellArgs, Write, WriteArgs,
};
use serde::de::DeserializeOwned;
use uuid::Uuid;

use crate::container::{TaskContainers, TaskRuntime};
use crate::tools::{SCOPE_MAC_FIELD, TASK_ID_FIELD, shell_template, task_mac_matches};

/// Runs the coding tools in per-task runtimes.
///
/// The backend is the narrow end of the guard's funnel: the guard decides
/// whether a call may run, then hands it here with the trusted task id the
/// guarded wrapper stamped into the request. Every call is made through the
/// matching `rig-tools` implementation, against that task's runtime.
#[derive(Clone)]
pub struct CodingBackend {
    containers: Arc<TaskContainers>,
    secret: [u8; 32],
}

impl CodingBackend {
    /// A backend over `containers` that accepts only a MAC made with
    /// `secret`. Built by [`CodingTools::backend`](crate::CodingTools::backend).
    pub(crate) fn new(containers: Arc<TaskContainers>, secret: [u8; 32]) -> Self {
        Self { containers, secret }
    }

    /// The tool names this backend serves.
    pub const TOOL_NAMES: &'static [&'static str] =
        &["read", "write", "edit", "search", "list", "shell"];

    /// Whether `name` is a coding tool this backend serves.
    pub fn is_coding_tool(name: &str) -> bool {
        Self::TOOL_NAMES.contains(&name)
    }

    /// Runs `tool` for `task_id` in that task's runtime.
    ///
    /// The caller is crate code that already checked the scope MAC; the
    /// public path is the [`ToolBackend`] implementation. The returned value
    /// is the tool's text output as a JSON string.
    pub(crate) async fn execute_for_task(
        &self,
        task_id: Uuid,
        tool: &str,
        input: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let runtime = self.containers.ensure(task_id).await?;
        let mut context = ToolContext::new();
        let output = match tool {
            "read" => {
                let mut args: ReadArgs = parse_args(tool, input)?;
                self.containers
                    .verify_project_read(task_id, &args.path)
                    .await?;
                let (base, path) = read_base_and_path(&runtime, &args.path);
                args.path = path;
                Read {
                    workspace: Arc::clone(&runtime.workspace),
                    base: Some(base),
                    confine: true,
                    seen: runtime.seen.clone(),
                }
                .call(&mut context, args)
                .await?
            }
            "write" => {
                let args: WriteArgs = parse_args(tool, input)?;
                if !runtime.is_container {
                    checked_target(&runtime.host_dir, &args.path, "write")?;
                }
                Write {
                    workspace: Arc::clone(&runtime.workspace),
                    base: Some(runtime.base.clone()),
                    confine: true,
                    seen: runtime.seen.clone(),
                }
                .call(&mut context, args)
                .await?
            }
            "edit" => {
                let args: EditArgs = parse_args(tool, input)?;
                if !runtime.is_container {
                    checked_target(&runtime.host_dir, &args.path, "edit")?;
                }
                Edit {
                    workspace: Arc::clone(&runtime.workspace),
                    base: Some(runtime.base.clone()),
                    confine: true,
                    seen: runtime.seen.clone(),
                }
                .call(&mut context, args)
                .await?
            }
            "search" => {
                let mut args: SearchArgs = parse_args(tool, input)?;
                if let Some(path) = args.path.as_deref() {
                    self.containers.verify_project_read(task_id, path).await?;
                }
                let (base, path) = read_base_and_path_option(&runtime, args.path);
                args.path = path;
                Search {
                    workspace: Arc::clone(&runtime.workspace),
                    base: Some(base),
                    confine: true,
                }
                .call(&mut context, args)
                .await?
            }
            "list" => {
                let mut args: ListArgs = parse_args(tool, input)?;
                if let Some(path) = args.path.as_deref() {
                    self.containers.verify_project_read(task_id, path).await?;
                }
                let (base, path) = read_base_and_path_option(&runtime, args.path);
                args.path = path;
                List {
                    workspace: Arc::clone(&runtime.workspace),
                    base: Some(base),
                    confine: true,
                }
                .call(&mut context, args)
                .await?
            }
            "shell" => {
                let args: ShellArgs = parse_args(tool, input)?;
                let mut shell = shell_template();
                shell.workspace = Arc::clone(&runtime.workspace);
                shell.launcher = shell_launcher(&runtime);
                shell.spill = Some(runtime.spill.clone());
                // `Shell::call` hides the status in its context. `run`
                // returns it, so a timeout or an output-limit kill can be
                // followed by a container restart that kills anything the
                // stopped command left running in the container.
                let ran = shell.run(args).await?;
                let mut output = translate_spill(ran.output, &runtime);
                if runtime.is_container
                    && matches!(ran.status, ExitStatus::TimedOut | ExitStatus::Killed)
                    && let Err(err) = self.containers.restart(task_id).await
                {
                    output.push_str(&format!(
                        "\n[dhi-exec: the command was stopped but its task container \
                         could not be restarted: {err}]"
                    ));
                }
                output
            }
            _ => return Err(anyhow!("no coding tool named {tool}")),
        };
        Ok(serde_json::Value::String(output))
    }
}

#[async_trait]
impl ToolBackend for CodingBackend {
    async fn execute(
        &self,
        tool: &str,
        input: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        if !Self::is_coding_tool(tool) {
            return Err(anyhow!("no coding tool named {tool}"));
        }
        let serde_json::Value::Object(mut input) = input.clone() else {
            return Err(anyhow!(
                "coding tool {tool} has no task scope: its arguments are not an object"
            ));
        };

        // The MAC is the capability that proves this request came from the
        // matching `CodingTools` wrapper for this task. It is checked before
        // any tool runs. A MAC issued for task A is not valid for task B
        // because the task id is part of the HMAC input.
        let mac = input
            .remove(SCOPE_MAC_FIELD)
            .and_then(|raw| raw.as_str().map(str::to_owned))
            .ok_or_else(|| {
                anyhow!("coding tool {tool} has no trusted scope: {SCOPE_MAC_FIELD} is missing")
            })?;

        let raw_task_id = input.remove(TASK_ID_FIELD).ok_or_else(|| {
            anyhow!("coding tool {tool} has no task scope: {TASK_ID_FIELD} is missing")
        })?;
        let task_id = raw_task_id
            .as_str()
            .and_then(|raw| Uuid::parse_str(raw).ok())
            .ok_or_else(|| {
                anyhow!("coding tool {tool} has no task scope: {TASK_ID_FIELD} is not a task id")
            })?;

        if !task_mac_matches(&self.secret, task_id, &mac) {
            return Err(anyhow!(
                "coding tool {tool} has no trusted scope: {SCOPE_MAC_FIELD} does not \
                 authorize task {task_id}"
            ));
        }

        let output = self
            .execute_for_task(task_id, tool, serde_json::Value::Object(input))
            .await?;
        let serde_json::Value::String(output) = output else {
            return Err(anyhow!(
                "coding tool {tool} returned a non-text output: {output}"
            ));
        };
        Ok(serde_json::json!({ "output": output }))
    }
}

/// Parses a tool's arguments, naming the tool when the shape is wrong.
fn parse_args<T: DeserializeOwned>(tool: &str, input: serde_json::Value) -> anyhow::Result<T> {
    serde_json::from_value(input).map_err(|err| anyhow!("invalid arguments for {tool}: {err}"))
}

/// Maps a `/project/...` read path to the project root as this runtime names
/// it, with a path relative to that root.
///
/// A legacy task has no project root. A path that merely starts with the same
/// letters (`/project-x`) does not match. `..` stays in the relative path and
/// is refused by the tool's confinement check, so it cannot escape the root.
fn project_read_target(runtime: &TaskRuntime, requested: &str) -> Option<(PathBuf, String)> {
    let root = runtime.project_dir.as_ref()?;
    let rest = requested.strip_prefix("/project")?;
    if !rest.is_empty() && !rest.starts_with('/') {
        return None;
    }
    let relative = rest.trim_start_matches('/');
    Some((
        root.clone(),
        if relative.is_empty() {
            ".".to_string()
        } else {
            relative.to_string()
        },
    ))
}

/// The base and path a read-style tool uses. Reads inside `/project` are
/// confined to the project root; every other read stays confined to `/work`.
fn read_base_and_path(runtime: &TaskRuntime, requested: &str) -> (PathBuf, String) {
    project_read_target(runtime, requested)
        .unwrap_or_else(|| (runtime.base.clone(), requested.to_string()))
}

/// [`read_base_and_path`] for tools whose path is optional.
fn read_base_and_path_option(
    runtime: &TaskRuntime,
    requested: Option<String>,
) -> (PathBuf, Option<String>) {
    match requested {
        Some(requested) => {
            let (base, path) = read_base_and_path(runtime, &requested);
            (base, Some(path))
        }
        None => (runtime.base.clone(), None),
    }
}

/// Resolves and validates a local `write` or `edit` target.
///
/// `Local::place` confines the path to the task directory, but it follows
/// existing links. The extra checks refuse a final target that is a symlink
/// (including a dangling one) and any symlink component below the task
/// directory on the requested path, even when that link points back inside.
///
/// Container mode does not run these checks: its paths are container paths,
/// and the container boundary is the control.
fn checked_target(task_dir: &Path, requested: &str, action: &str) -> anyhow::Result<PathBuf> {
    let workspace: &dyn Workspace = &Local;
    let resolved = workspace
        .place(Some(task_dir), true, requested)
        .map_err(|err| anyhow!("{action} {requested}: {err}"))?;
    reject_symlink(&resolved, requested, action)?;
    reject_symlink_components(task_dir, requested, action)?;
    Ok(resolved)
}

/// Refuses `path` itself when it is a symbolic link, dangling or not.
fn reject_symlink(path: &Path, requested: &str, action: &str) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => bail!(
            "refusing to {action} {requested}: {} is a symbolic link",
            path.display()
        ),
        Ok(_) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(anyhow!("cannot inspect {}: {err}", path.display())),
    }
}

/// Walks the requested path below the task directory and refuses any symlink
/// component, including one that resolves to another place inside the task.
fn reject_symlink_components(task_dir: &Path, requested: &str, action: &str) -> anyhow::Result<()> {
    let given = Path::new(requested);
    let relative = if given.is_absolute() {
        let Ok(relative) = given.strip_prefix(task_dir) else {
            // `place` already proved the resolved path is inside; there is no
            // lexical path below the task directory to walk.
            return Ok(());
        };
        relative
    } else {
        given
    };

    let mut current = task_dir.to_path_buf();
    for component in relative.components() {
        match component {
            Component::Normal(part) => {
                current.push(part);
                match fs::symlink_metadata(&current) {
                    Ok(metadata) if metadata.file_type().is_symlink() => bail!(
                        "refusing to {action} {requested}: {} is a symbolic link",
                        current.display()
                    ),
                    Ok(_) => {}
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                    Err(err) => {
                        return Err(anyhow!("cannot inspect {}: {err}", current.display()));
                    }
                }
            }
            Component::CurDir => {}
            Component::ParentDir => {
                current.pop();
            }
            Component::RootDir | Component::Prefix(_) => {}
        }
    }
    Ok(())
}

/// Parent variables the local shell may keep. Everything else is removed
/// first.
const INHERITED_VARIABLES: &[&str] = &[
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
];

/// The search path used when the parent has none.
const DEFAULT_PATH: &str = "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

/// The launcher every `shell` call uses.
///
/// In container mode the command runs inside the task container: `HOME`,
/// `TMPDIR` and `CARGO_HOME` are below `/work`, so tools that execute
/// temporary binaries work even though the root filesystem is read-only and
/// `/tmp` is a `noexec` tmpfs. No host environment is inherited into the
/// container.
///
/// In local test mode the parent environment is removed wholesale, then a
/// small allowlist is preserved: certificate and proxy settings, the locale,
/// and a Rust toolchain location. `HOME`, `TMPDIR`, and `CARGO_HOME` point
/// into the task directory, and `PATH` keeps the host's search path (or a
/// conservative default). Secrets and host-specific handles
/// (`SSH_AUTH_SOCK`, `DHI_*`, API keys, database URLs) are not on the
/// allowlist. This is environment hygiene, not isolation.
fn shell_launcher(runtime: &TaskRuntime) -> Launcher {
    if runtime.is_container {
        let mut launcher = Launcher::default()
            .in_dir(runtime.base.clone())
            .with_env("HOME", runtime.base.join(".home"))
            .with_env("TMPDIR", runtime.base.join(".tmp"))
            .with_env("CARGO_HOME", runtime.base.join(".cargo"))
            .with_env("TERM", "dumb")
            .with_env("NO_COLOR", "1")
            .with_env("CI", "1")
            .with_env("PYTHONDONTWRITEBYTECODE", "1");
        // The same launcher starts the host `docker` transport. Give that
        // process the host's home back so Docker Desktop can find its
        // context and configuration; the command inside the container still
        // gets `HOME=/work/.home` from the launcher's environment, which
        // `Remote::spawn` passes into the container explicitly.
        if let Some(home) = parent_var("HOME") {
            let mut assignment = OsString::from("HOME=");
            assignment.push(home);
            launcher = launcher.wrapped([OsString::from("env"), assignment]);
        }
        return launcher;
    }

    local_shell_launcher(&runtime.base)
}

/// The host launcher local test mode uses.
fn local_shell_launcher(task_dir: &Path) -> Launcher {
    let parent_names = std::env::vars_os()
        .map(|(name, _)| name)
        .collect::<Vec<_>>();
    let mut launcher = Launcher::default()
        .in_dir(task_dir)
        .without_env(parent_names);

    for name in INHERITED_VARIABLES {
        if let Some(value) = parent_var(name) {
            launcher = launcher.with_env(name, value);
        }
    }
    if let Some(rustup) = parent_var("RUSTUP_HOME") {
        launcher = launcher.with_env("RUSTUP_HOME", rustup);
    } else if let Some(home) = parent_var("HOME") {
        let rustup = Path::new(&home).join(".rustup");
        if rustup.is_dir() {
            launcher = launcher.with_env("RUSTUP_HOME", rustup);
        }
    }

    let locale = default_locale();
    if parent_var("LANG").is_none() {
        launcher = launcher.with_env("LANG", locale);
    }
    if parent_var("LC_ALL").is_none() {
        launcher = launcher.with_env("LC_ALL", locale);
    }

    launcher
        .with_env("HOME", task_dir.join(".home"))
        .with_env("TMPDIR", task_dir.join(".tmp"))
        .with_env("CARGO_HOME", task_dir.join(".cargo"))
        .with_env(
            "PATH",
            parent_var("PATH").unwrap_or_else(|| OsString::from(DEFAULT_PATH)),
        )
        .with_env("TERM", "dumb")
        .with_env("NO_COLOR", "1")
        .with_env("CI", "1")
        .with_env("PYTHONDONTWRITEBYTECODE", "1")
}

/// Rewrites the host spill path in cut output to the container path.
///
/// The spill file itself lives on the host below the mounted task directory,
/// so it is the same file inside the container at `runtime.base`. The model
/// sees container paths, so the path printed in the cut notice must be the
/// container path it can `read`.
fn translate_spill(output: String, runtime: &TaskRuntime) -> String {
    if !runtime.is_container {
        return output;
    }
    let host = runtime.host_dir.to_string_lossy();
    let guest = runtime.base.to_string_lossy();
    output.replace(host.as_ref(), guest.as_ref())
}

fn parent_var(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

fn default_locale() -> &'static str {
    if cfg!(target_os = "macos") {
        "en_US.UTF-8"
    } else {
        "C.UTF-8"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::CodingTools;

    fn fixture() -> (
        tempfile::TempDir,
        Arc<TaskContainers>,
        CodingTools,
        CodingBackend,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let containers = Arc::new(TaskContainers::local(temp.path()).unwrap());
        let tools = CodingTools::new(Arc::clone(&containers));
        let backend = tools.backend();
        (temp, containers, tools, backend)
    }

    fn scoped_input(
        tools: &CodingTools,
        task_id: Uuid,
        mut args: serde_json::Value,
    ) -> serde_json::Value {
        let object = args.as_object_mut().expect("test arguments are an object");
        object.insert(
            TASK_ID_FIELD.to_owned(),
            serde_json::json!(task_id.to_string()),
        );
        object.insert(
            SCOPE_MAC_FIELD.to_owned(),
            serde_json::json!(tools.mac(task_id)),
        );
        args
    }

    async fn task_dir(containers: &TaskContainers, task_id: Uuid) -> PathBuf {
        containers.ensure(task_id).await.unwrap().host_dir.clone()
    }

    #[test]
    fn the_tool_names_are_the_six_coding_tools() {
        assert_eq!(
            CodingBackend::TOOL_NAMES,
            ["read", "write", "edit", "search", "list", "shell"]
        );
        for name in CodingBackend::TOOL_NAMES {
            assert!(CodingBackend::is_coding_tool(name));
        }
        assert!(!CodingBackend::is_coding_tool("delete"));
        assert!(!CodingBackend::is_coding_tool("move"));
    }

    #[tokio::test]
    async fn missing_task_scope_fails() {
        let (_temp, _containers, tools, backend) = fixture();
        let mut input = serde_json::json!({ "path": "a.txt" });
        input[SCOPE_MAC_FIELD] = serde_json::json!(tools.mac(Uuid::new_v4()));
        let err = backend
            .execute("read", &input)
            .await
            .expect_err("a call without a task id cannot run");
        let text = err.to_string();
        assert!(text.contains("read"), "{text}");
        assert!(text.contains("task scope"), "{text}");
    }

    #[tokio::test]
    async fn an_invalid_task_scope_fails() {
        let (_temp, _containers, tools, backend) = fixture();
        let mut input = serde_json::json!({ "path": "a.txt" });
        input[SCOPE_MAC_FIELD] = serde_json::json!(tools.mac(Uuid::new_v4()));
        input[TASK_ID_FIELD] = serde_json::json!("not-a-uuid");
        let err = backend
            .execute("read", &input)
            .await
            .expect_err("an invalid task id cannot run");
        let text = err.to_string();
        assert!(text.contains("read"), "{text}");
        assert!(text.contains("task scope"), "{text}");
    }

    #[tokio::test]
    async fn a_missing_mac_fails_before_the_tool_runs() {
        let (_temp, containers, _tools, backend) = fixture();
        let task = Uuid::new_v4();
        let mut input = serde_json::json!({ "path": "a.txt", "content": "leaked\n" });
        input[TASK_ID_FIELD] = serde_json::json!(task.to_string());

        let err = backend
            .execute("write", &input)
            .await
            .expect_err("a call without the MAC cannot run");
        assert!(err.to_string().contains(SCOPE_MAC_FIELD), "{err}");
        assert!(
            !containers
                .root()
                .join(task.to_string())
                .join("a.txt")
                .exists(),
            "the refused write did not run"
        );
    }

    #[tokio::test]
    async fn a_wrong_mac_fails_before_the_tool_runs() {
        let (_temp, containers, _tools, backend) = fixture();
        let task = Uuid::new_v4();
        let mut input = serde_json::json!({ "path": "a.txt", "content": "leaked\n" });
        input[TASK_ID_FIELD] = serde_json::json!(task.to_string());
        // A well-formed 32-byte hex string that is not this task's MAC.
        input[SCOPE_MAC_FIELD] = serde_json::json!("00".repeat(32));

        let err = backend
            .execute("write", &input)
            .await
            .expect_err("a call with a foreign MAC cannot run");
        assert!(err.to_string().contains(SCOPE_MAC_FIELD), "{err}");
        assert!(
            !containers
                .root()
                .join(task.to_string())
                .join("a.txt")
                .exists(),
            "the refused write did not run"
        );
    }

    #[tokio::test]
    async fn a_mac_for_task_a_does_not_authorize_task_b() {
        let (_temp, containers, tools, backend) = fixture();
        let task_a = Uuid::new_v4();
        let task_b = Uuid::new_v4();
        let mut input = serde_json::json!({ "path": "b.txt", "content": "leaked\n" });
        input[TASK_ID_FIELD] = serde_json::json!(task_b.to_string());
        input[SCOPE_MAC_FIELD] = serde_json::json!(tools.mac(task_a));

        let err = backend
            .execute("write", &input)
            .await
            .expect_err("task A's MAC is not task B's");
        assert!(err.to_string().contains(SCOPE_MAC_FIELD), "{err}");
        assert!(
            !containers
                .root()
                .join(task_b.to_string())
                .join("b.txt")
                .exists(),
            "the refused write did not run"
        );
    }

    #[tokio::test]
    async fn non_object_arguments_fail() {
        let (_temp, _containers, _tools, backend) = fixture();
        let err = backend
            .execute("read", &serde_json::json!("a.txt"))
            .await
            .expect_err("a non-object call cannot carry a task id");
        assert!(err.to_string().contains("task scope"), "{err}");
    }

    #[tokio::test]
    async fn an_unknown_tool_fails() {
        let (_temp, _containers, _tools, backend) = fixture();
        let err = backend
            .execute("delete", &serde_json::json!({}))
            .await
            .expect_err("delete is not a coding tool");
        assert!(
            err.to_string().contains("no coding tool named delete"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn the_guarded_path_wraps_the_raw_text() {
        let (_temp, _containers, tools, backend) = fixture();
        let task = Uuid::new_v4();

        let raw = backend
            .execute_for_task(
                task,
                "write",
                serde_json::json!({ "path": "raw.txt", "content": "raw text\n" }),
            )
            .await
            .unwrap();
        assert!(raw.is_string(), "the raw path returns the tool text: {raw}");

        let read = backend
            .execute_for_task(task, "read", serde_json::json!({ "path": "raw.txt" }))
            .await
            .unwrap();
        assert_eq!(read, serde_json::json!("raw text\n"));

        let guarded = backend
            .execute(
                "read",
                &scoped_input(&tools, task, serde_json::json!({ "path": "raw.txt" })),
            )
            .await
            .unwrap();
        assert_eq!(guarded, serde_json::json!({ "output": read }));
    }

    #[tokio::test]
    async fn project_reads_use_project_root_and_writes_stay_in_work() {
        let (temp, containers, _tools, backend) = fixture();
        let project = temp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("README.md"), "project text\n").unwrap();
        let task = Uuid::new_v4();
        containers.set_task_project(task, Some(&project)).unwrap();

        let read = backend
            .execute_for_task(
                task,
                "read",
                serde_json::json!({ "path": "/project/README.md" }),
            )
            .await
            .unwrap();
        assert_eq!(read, serde_json::json!("project text\n"));

        let listed = backend
            .execute_for_task(task, "list", serde_json::json!({ "path": "/project" }))
            .await
            .unwrap();
        assert!(
            listed.as_str().unwrap_or_default().contains("README.md"),
            "{listed}"
        );

        let err = backend
            .execute_for_task(
                task,
                "write",
                serde_json::json!({ "path": "/project/new.txt", "content": "no\n" }),
            )
            .await
            .expect_err("/project is read-only");
        assert!(
            err.to_string().contains("outside") || err.to_string().contains("permission"),
            "{err}"
        );
        assert!(!project.join("new.txt").exists());

        // A project read cannot escape upward through `..`.
        let err = backend
            .execute_for_task(
                task,
                "read",
                serde_json::json!({ "path": "/project/../AGENTS.md" }),
            )
            .await
            .expect_err("the project read is confined to /project");
        assert!(err.to_string().contains("outside"), "{err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn project_reads_reject_a_symlink_escape_before_the_tool_runs() {
        use std::os::unix::fs::symlink;

        let (temp, containers, _tools, backend) = fixture();
        let project = temp.path().join("project");
        let outside = temp.path().join("outside.txt");
        fs::create_dir_all(&project).unwrap();
        fs::write(&outside, "secret\n").unwrap();
        symlink(&outside, project.join("escape.txt")).unwrap();
        let task = Uuid::new_v4();
        containers.set_task_project(task, Some(&project)).unwrap();

        let err = backend
            .execute_for_task(
                task,
                "read",
                serde_json::json!({ "path": "/project/escape.txt" }),
            )
            .await
            .expect_err("the symlink target is outside /project");
        assert!(err.to_string().contains("outside"), "{err}");
    }

    #[tokio::test]
    async fn the_task_directory_is_created_before_the_tool_runs() {
        let (_temp, containers, _tools, backend) = fixture();
        let task = Uuid::new_v4();

        let dir = task_dir(&containers, task).await;
        assert!(dir.is_dir());

        backend
            .execute_for_task(
                task,
                "write",
                serde_json::json!({ "path": "a.txt", "content": "a\n" }),
            )
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "a\n");
    }
}
