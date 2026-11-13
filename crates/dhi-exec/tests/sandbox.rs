//! Native macOS sandbox checks.
//!
//! These tests need no `sudo`, no task user, no `/work` or `/project` alias,
//! and no reboot: a sandbox task runs as the same macOS user as the test and
//! uses its own real host paths.
//!
//! The tests below `sandbox-exec` run the real `/usr/bin/sandbox-exec` with
//! the repository profile, `ops/macos/dhi-task.sb`, so the OS boundary itself
//! is checked and not only the configuration this crate builds. They skip,
//! with a clear message, on a machine without `sandbox-exec`, and when the
//! profile is not next to the crate.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use dhi_exec::{SandboxConfig, TaskContainers};
#[cfg(unix)]
use uuid::Uuid;

/// The repository profile, resolved from this crate's manifest directory.
fn repo_profile() -> Option<PathBuf> {
    let profile = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../ops/macos/dhi-task.sb")
        .canonicalize()
        .ok()?;
    profile.is_file().then_some(profile)
}

/// The profile for a test that only needs a valid file.
#[cfg(unix)]
fn test_config(temp: &tempfile::TempDir) -> SandboxConfig {
    let profile = temp.path().join("task.sb");
    fs::write(&profile, "(version 1)\n(allow default)\n").unwrap();
    SandboxConfig { profile }
}

/// The canonical form of `path`, created and resolved first, because the
/// sandbox profile matches the real path (`/private/tmp`, not `/tmp`).
#[cfg(unix)]
fn real_dir(path: &Path) -> PathBuf {
    fs::create_dir_all(path).unwrap();
    fs::canonicalize(path).unwrap()
}

/// `path` as one argument to `/bin/sh -c`.
#[cfg(unix)]
fn quoted(path: &Path) -> String {
    format!("'{}'", path.display())
}

/// One command under the real `sandbox-exec` with the repository profile and
/// the task's real paths.
#[cfg(unix)]
fn sandboxed(profile: &Path, project: &Path, work: &Path, pidfile: &Path, line: &str) -> Output {
    Command::new("/usr/bin/sandbox-exec")
        .arg("-f")
        .arg(profile)
        .arg("-D")
        .arg(format!("PROJECT={}", project.display()))
        .arg("-D")
        .arg(format!("WORK={}", work.display()))
        .arg("-D")
        .arg(format!(
            "SPILL={}",
            work.join(".dhi").join("spill").display()
        ))
        .arg("-D")
        .arg(format!("PIDFILE={}", pidfile.display()))
        .arg("/bin/sh")
        .arg("-c")
        .arg(line)
        .output()
        .expect("cannot run /usr/bin/sandbox-exec")
}

/// What `/usr/bin/sandbox-exec` and `fork` do when another thread holds the
/// address-space lock (`vm_fork_init`), as the parallel test binary can.
const SPAWN_LOCK_FAILURE: &str = "Resource temporarily unavailable";

/// Runs `check` until it does not fail on the spawn lock.
///
/// A `sandbox-exec` that cannot get the lock reports it on stderr, which
/// first surfaces as a failed status assertion; a `spawn` under `catch_unwind`
/// reports it directly.
#[cfg(target_os = "macos")]
fn retry_spawn_lock(mut check: impl FnMut()) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(&mut check)) {
            Ok(()) => return,
            Err(payload) => {
                let message = payload
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| {
                        payload
                            .downcast_ref::<&str>()
                            .map(|message| message.to_string())
                    })
                    .unwrap_or_default();
                if !message.contains(SPAWN_LOCK_FAILURE) {
                    std::panic::panic_any(payload);
                }
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "sandbox-exec could not get the spawn lock"
        );
    }
}

/// The stderr of a failed sandbox command, for a readable assertion.
#[cfg(unix)]
fn failed(output: &Output) -> String {
    format!(
        "status {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

#[cfg(unix)]
#[test]
fn a_sandbox_manager_validates_its_private_root() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let requested = temp.path().join("nested").join("tasks");
    let containers = TaskContainers::sandbox(&requested, test_config(&temp)).unwrap();

    assert_eq!(
        containers.root(),
        fs::canonicalize(&requested).unwrap().as_path()
    );
    let mode = fs::metadata(containers.root())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o077, 0, "the root is private: mode {mode:o}");
}

#[cfg(unix)]
#[test]
fn a_sandbox_manager_refuses_a_symlinked_root() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let outside = temp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let requested = temp.path().join("tasks");
    symlink(&outside, &requested).unwrap();

    let err = match TaskContainers::sandbox(&requested, test_config(&temp)) {
        Ok(_) => panic!("a symlinked root is refused"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("symbolic link"), "{err}");
}

#[test]
fn a_sandbox_manager_requires_the_profile_to_exist() {
    let temp = tempfile::tempdir().unwrap();
    let config = SandboxConfig {
        profile: temp.path().join("missing.sb"),
    };

    let err = match TaskContainers::sandbox(temp.path().join("tasks"), config) {
        Ok(_) => panic!("a missing profile is refused"),
        Err(err) => err,
    };
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    assert!(err.to_string().contains("does not exist"), "{err}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_project_task_uses_its_real_paths() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    fs::create_dir_all(&project).unwrap();
    let containers =
        TaskContainers::sandbox(temp.path().join("tasks"), test_config(&temp)).unwrap();
    let task = Uuid::new_v4();
    containers.set_task_project(task, Some(&project)).unwrap();

    let runtime = containers.ensure(task).await.unwrap();

    let canonical_project = fs::canonicalize(&project).unwrap();
    let expected = canonical_project.join(".dhi").join(task.to_string());
    assert_eq!(runtime.base, expected);
    assert_eq!(runtime.host_dir, expected);
    assert_eq!(
        runtime.project_dir.as_deref(),
        Some(canonical_project.as_path())
    );
    assert!(
        runtime.sandbox.is_some(),
        "sandbox mode runs under sandbox-exec"
    );
    let sandbox = runtime.sandbox.as_ref().expect("a sandbox runtime");
    assert_eq!(sandbox.task_id, task);
    assert_eq!(sandbox.host_work, expected);
    assert_eq!(
        sandbox.host_project.as_deref(),
        Some(canonical_project.as_path())
    );
    assert_eq!(
        sandbox.pidfile,
        containers.root().join("pids").join(format!("{task}.pid"))
    );
    assert_eq!(
        runtime.workspace.host(),
        None,
        "the file tools run on the host"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_legacy_task_uses_the_task_root() {
    let temp = tempfile::tempdir().unwrap();
    let containers =
        TaskContainers::sandbox(temp.path().join("tasks"), test_config(&temp)).unwrap();
    let task = Uuid::new_v4();

    let runtime = containers.ensure(task).await.unwrap();

    assert_eq!(runtime.base, containers.root().join(task.to_string()));
    assert_eq!(runtime.host_dir, runtime.base);
    assert_eq!(runtime.project_dir, None);
    assert!(runtime.sandbox.is_some());
    let sandbox = runtime.sandbox.as_ref().expect("a sandbox runtime");
    assert_eq!(sandbox.task_id, task);
    assert_eq!(sandbox.host_project, None);
    assert_eq!(
        sandbox.pidfile,
        containers.root().join("pids").join(format!("{task}.pid"))
    );
}

#[cfg(unix)]
#[tokio::test]
async fn two_project_tasks_run_at_the_same_time() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    fs::create_dir_all(&project).unwrap();
    let containers =
        TaskContainers::sandbox(temp.path().join("tasks"), test_config(&temp)).unwrap();
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    containers.set_task_project(first, Some(&project)).unwrap();
    containers.set_task_project(second, Some(&project)).unwrap();

    let first_runtime = containers.ensure(first).await.unwrap();
    let second_runtime = containers.ensure(second).await.unwrap();

    assert_ne!(
        first_runtime.base, second_runtime.base,
        "every task has its own work directory"
    );
    assert!(first_runtime.base.is_dir());
    assert!(second_runtime.base.is_dir());
    assert_eq!(first_runtime.sandbox.as_ref().unwrap().task_id, first);
    assert_eq!(second_runtime.sandbox.as_ref().unwrap().task_id, second);
    assert_ne!(
        first_runtime.sandbox.as_ref().unwrap().pidfile,
        second_runtime.sandbox.as_ref().unwrap().pidfile,
        "every task records its process groups in its own pidfile"
    );

    containers.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// The real OS boundary, through the repository profile.
// ---------------------------------------------------------------------------

/// The real profile refuses a project write, allows a work write, refuses a
/// spill write, a hardlink of a project file and a write through a symbolic
/// link in the work folder into the project.
#[cfg(target_os = "macos")]
#[test]
fn the_real_profile_confines_writes_to_the_work_folder() {
    use std::os::unix::fs::symlink;

    let Some(profile) = repo_profile() else {
        eprintln!(
            "skipping the_real_profile_confines_writes_to_the_work_folder: \
             ops/macos/dhi-task.sb is missing; run from the repository"
        );
        return;
    };
    if !Path::new("/usr/bin/sandbox-exec").exists() {
        eprintln!(
            "skipping the_real_profile_confines_writes_to_the_work_folder: \
             /usr/bin/sandbox-exec does not exist on this machine"
        );
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let project = real_dir(&temp.path().join("project"));
    let work = real_dir(&project.join(".dhi").join("task-1"));
    let spill = real_dir(&work.join(".dhi/spill"));
    let pidfile = temp.path().join("task.pid");
    fs::write(project.join("README.md"), "project text\n").unwrap();
    fs::write(work.join("note.md"), "work note\n").unwrap();
    symlink(project.join("README.md"), work.join("link.md")).unwrap();

    let run = |line: &str| sandboxed(&profile, &project, &work, &pidfile, line);

    // The work folder is writable.
    retry_spawn_lock(|| {
        let output = run(&format!(
            "printf 'w\\n' > {}",
            quoted(&work.join("written.txt"))
        ));
        assert!(output.status.success(), "{}", failed(&output));
    });
    assert_eq!(fs::read_to_string(work.join("written.txt")).unwrap(), "w\n");

    // The project is readable but not writable.
    retry_spawn_lock(|| {
        let output = run(&format!("cat {}", quoted(&project.join("README.md"))));
        assert!(output.status.success(), "{}", failed(&output));
        let output = run(&format!(
            "printf 'p\\n' > {}",
            quoted(&project.join("planted.txt"))
        ));
        assert!(!output.status.success(), "the project is read-only");
    });
    assert!(!project.join("planted.txt").exists());

    // The spill folder is read-only.
    retry_spawn_lock(|| {
        let output = run(&format!(
            "printf 's\\n' > {}",
            quoted(&spill.join("cut.txt"))
        ));
        assert!(!output.status.success(), "the spill is read-only");
    });
    assert!(!spill.join("cut.txt").exists());

    // A hardlink of a project file into the work folder is refused.
    retry_spawn_lock(|| {
        let output = run(&format!(
            "ln {} {}",
            quoted(&project.join("README.md")),
            quoted(&work.join("hard.md"))
        ));
        assert!(!output.status.success(), "linking must be denied");
    });
    assert!(!work.join("hard.md").exists());

    // Writing through a link in the work folder into the project is refused.
    retry_spawn_lock(|| {
        let output = run(&format!(
            "printf 'through\\n' > {}",
            quoted(&work.join("link.md"))
        ));
        assert!(!output.status.success(), "the link target is the project");
    });
    assert_eq!(
        fs::read_to_string(project.join("README.md")).unwrap(),
        "project text\n"
    );
}

/// Two real sandbox commands run at once in separate work folders, and both
/// output files land.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn two_real_sandbox_commands_run_in_parallel() {
    let Some(profile) = repo_profile() else {
        eprintln!(
            "skipping two_real_sandbox_commands_run_in_parallel: \
             ops/macos/dhi-task.sb is missing; run from the repository"
        );
        return;
    };
    if !Path::new("/usr/bin/sandbox-exec").exists() {
        eprintln!(
            "skipping two_real_sandbox_commands_run_in_parallel: \
             /usr/bin/sandbox-exec does not exist on this machine"
        );
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let project = real_dir(&temp.path().join("project"));
    let first = real_dir(&project.join(".dhi").join("task-a"));
    let second = real_dir(&project.join(".dhi").join("task-b"));
    let first_pidfile = temp.path().join("task-a.pid");
    let second_pidfile = temp.path().join("task-b.pid");
    let first_line = "sleep 0.5; printf 'a\\n' > out.txt";
    let second_line = "printf 'b\\n' > out.txt";

    let spawn = |work: &Path, pidfile: &Path, line: &str| {
        let mut command = Command::new("/usr/bin/sandbox-exec");
        command
            .arg("-f")
            .arg(&profile)
            .arg("-D")
            .arg(format!("PROJECT={}", project.display()))
            .arg("-D")
            .arg(format!("WORK={}", work.display()))
            .arg("-D")
            .arg(format!("SPILL={}", work.join(".dhi/spill").display()))
            .arg("-D")
            .arg(format!("PIDFILE={}", pidfile.display()))
            .arg("/bin/sh")
            .arg("-c")
            .arg(line)
            .current_dir(work)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.spawn().expect("cannot start /usr/bin/sandbox-exec")
    };
    let (first_child, second_child) = {
        let mut children = None;
        retry_spawn_lock(|| {
            children = Some((
                spawn(&first, &first_pidfile, first_line),
                spawn(&second, &second_pidfile, second_line),
            ));
        });
        children.unwrap()
    };
    let first_out = tokio::task::spawn_blocking(move || first_child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    let second_out = tokio::task::spawn_blocking(move || second_child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(first_out.status.success(), "{}", failed(&first_out));
    assert!(second_out.status.success(), "{}", failed(&second_out));

    assert_eq!(fs::read_to_string(first.join("out.txt")).unwrap(), "a\n");
    assert_eq!(fs::read_to_string(second.join("out.txt")).unwrap(), "b\n");
}

// ---------------------------------------------------------------------------
// The coding tools against a real project task.
// ---------------------------------------------------------------------------

/// The sink the host builds around the coding backend.
#[cfg(unix)]
use dhi_guard::ToolBackend as _;

#[cfg(unix)]
struct BackendSink {
    backend: dhi_exec::CodingBackend,
}

#[cfg(unix)]
#[async_trait::async_trait]
impl dhi_agent::ToolSink for BackendSink {
    async fn execute(
        &self,
        _task_id: Uuid,
        _intent_id: Uuid,
        request: dhi_core::ToolCallRequest,
    ) -> anyhow::Result<dhi_core::ToolCallOutcome> {
        let output = self.backend.execute(&request.tool, &request.input).await?;
        Ok(dhi_core::ToolCallOutcome::Allowed { output })
    }
}

/// Calls a tool the way the guarded wrapper does: through the registered
/// tool, with the run scope in the dispatch context.
#[cfg(unix)]
async fn call(
    handle: &rig::tool::server::ToolServerHandle,
    task: Uuid,
    tool: &str,
    args: serde_json::Value,
) -> anyhow::Result<String> {
    let mut context = rig::tool::ToolContext::new();
    context
        .insert(dhi_agent::RunScope {
            task_id: task,
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
        .and_then(|value| value.get("output"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("{tool} returned no text output"))
}

/// A sandbox project task reads, lists and searches its own absolute work
/// folder, still cannot read another task's folder or leave the project
/// through a link, and still reads a project file by its absolute path.
#[cfg(unix)]
#[tokio::test]
async fn a_project_task_reads_its_own_work_folder_by_absolute_path() {
    use std::os::unix::fs::symlink;
    use std::sync::Arc;

    let temp = tempfile::tempdir().unwrap();
    let project = real_dir(&temp.path().join("project"));
    fs::write(project.join("README.md"), "project text\n").unwrap();
    let containers =
        Arc::new(TaskContainers::sandbox(temp.path().join("tasks"), test_config(&temp)).unwrap());
    let task = Uuid::new_v4();
    let other = Uuid::new_v4();
    // Both tasks are in the same project, so the only difference between
    // them is their own `.dhi` folder.
    containers.set_task_project(task, Some(&project)).unwrap();
    containers.set_task_project(other, Some(&project)).unwrap();
    let tools = dhi_exec::CodingTools::new(Arc::clone(&containers));
    let handle = rig::tool::server::ToolServer::new().run();
    tools
        .register_tools(
            &handle,
            Arc::new(BackendSink {
                backend: tools.backend(),
            }),
        )
        .unwrap();

    let work = containers.ensure(task).await.unwrap().host_dir.clone();
    containers.ensure(other).await.unwrap();
    fs::write(work.join("note.md"), "own note\n").unwrap();
    let note = work.join("note.md");

    // `read` with the real absolute work path.
    let text = call(
        &handle,
        task,
        "read",
        serde_json::json!({ "path": note.to_string_lossy() }),
    )
    .await
    .unwrap_or_else(|err| panic!("{} is the task's own work file: {err}", note.display()));
    assert_eq!(text, "own note\n");

    // `list` and `search` with the absolute work directory.
    let work_arg = work.to_string_lossy().to_string();
    let listed = call(
        &handle,
        task,
        "list",
        serde_json::json!({ "path": work_arg, "hidden": true }),
    )
    .await
    .unwrap_or_else(|err| panic!("{work_arg} is the task's own work folder: {err}"));
    assert!(listed.contains("note.md"), "{listed}");
    let found = call(
        &handle,
        task,
        "search",
        serde_json::json!({ "pattern": "own note", "path": work_arg, "hidden": true }),
    )
    .await
    .unwrap_or_else(|err| panic!("{work_arg} is searchable: {err}"));
    assert!(found.contains("own note"), "{found}");

    // A project file is still readable by its absolute path.
    let readme = fs::canonicalize(&project).unwrap().join("README.md");
    let text = call(
        &handle,
        task,
        "read",
        serde_json::json!({ "path": readme.to_string_lossy() }),
    )
    .await
    .expect("a project file is readable");
    assert_eq!(text, "project text\n");

    // Another task's `.dhi` folder stays refused, even a missing file in it.
    let other_work = containers.ensure(other).await.unwrap().host_dir.clone();
    fs::write(other_work.join("secret.md"), "other note\n").unwrap();
    for path in [other_work.join("secret.md"), other_work.join("missing.md")] {
        let err = call(
            &handle,
            task,
            "read",
            serde_json::json!({ "path": path.to_string_lossy() }),
        )
        .await
        .expect_err("another task's work folder is hidden");
        assert!(err.to_string().contains("hidden"), "{err}");
    }

    // The project `.dhi` root itself stays hidden.
    let err = call(
        &handle,
        task,
        "read",
        serde_json::json!({ "path": project.join(".dhi").to_string_lossy() }),
    )
    .await
    .expect_err("the project .dhi root is hidden");
    assert!(err.to_string().contains("hidden"), "{err}");

    // A link in the work folder that leaves the project is still refused.
    let outside = temp.path().join("outside.txt");
    fs::write(&outside, "outside secret\n").unwrap();
    symlink(&outside, work.join("escape.txt")).unwrap();
    let escape = work.join("escape.txt");
    let err = call(
        &handle,
        task,
        "read",
        serde_json::json!({ "path": escape.to_string_lossy() }),
    )
    .await
    .expect_err("a link out of the project is refused");
    assert!(err.to_string().contains("outside"), "{err}");

    containers.shutdown().await.unwrap();
}
