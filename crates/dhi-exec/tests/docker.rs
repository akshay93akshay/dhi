//! Docker-backed design B checks. Every test is ignored by default and
//! skips cleanly when Docker is unavailable:
//!
//! ```text
//! cargo test -p dhi-exec --test docker -- --ignored
//! ```

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dhi_agent::{RunScope, ToolSink};
use dhi_core::{ToolCallOutcome, ToolCallRequest};
use dhi_exec::{CodingBackend, CodingTools, ContainerConfig, TaskContainers};
use dhi_guard::ToolBackend;
use rig::tool::ToolContext;
use rig::tool::server::{ToolServer, ToolServerHandle};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// The sink the host builds around the coding backend.
struct BackendSink {
    backend: CodingBackend,
}

#[async_trait]
impl ToolSink for BackendSink {
    async fn execute(
        &self,
        _task_id: Uuid,
        _intent_id: Uuid,
        request: ToolCallRequest,
    ) -> anyhow::Result<ToolCallOutcome> {
        let output = self.backend.execute(&request.tool, &request.input).await?;
        Ok(ToolCallOutcome::Allowed { output })
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    containers: Arc<TaskContainers>,
    handle: ToolServerHandle,
    tasks: Mutex<Vec<Uuid>>,
}

impl Fixture {
    /// A Docker fixture, or `None` when Docker is not available.
    async fn docker() -> Option<Self> {
        if !docker_available().await {
            eprintln!("skipping: Docker is not available");
            return None;
        }
        let temp = tempfile::tempdir().unwrap();
        let containers =
            Arc::new(TaskContainers::new(temp.path(), ContainerConfig::default()).unwrap());
        let tools = CodingTools::new(Arc::clone(&containers));
        let handle = ToolServer::new().run();
        tools
            .register_tools(
                &handle,
                Arc::new(BackendSink {
                    backend: tools.backend(),
                }),
            )
            .unwrap();
        Some(Self {
            _temp: temp,
            containers,
            handle,
            tasks: Mutex::new(Vec::new()),
        })
    }

    fn task(&self) -> Uuid {
        let task = Uuid::new_v4();
        self.tasks.lock().unwrap().push(task);
        task
    }

    async fn call(&self, task_id: Uuid, tool: &str, args: Value) -> anyhow::Result<Value> {
        let mut context = ToolContext::new();
        context
            .insert(RunScope {
                task_id,
                intent_id: Uuid::new_v4(),
            })
            .unwrap();
        let result = self
            .handle
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
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("{tool} returned no JSON output"))
    }

    async fn shell(&self, task: Uuid, command: &str) -> anyhow::Result<String> {
        let result = self
            .call(task, "shell", json!({ "command": command }))
            .await?;
        Ok(output(&result).to_owned())
    }
}

impl Drop for Fixture {
    /// Best-effort cleanup even when an assertion fails. The tests still
    /// exercise `remove` and `shutdown` explicitly.
    fn drop(&mut self) {
        for task in self.tasks.lock().unwrap().iter() {
            let name = format!("dhi-task-{task}");
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", &name])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

fn output(value: &Value) -> &str {
    value
        .get("output")
        .and_then(Value::as_str)
        .expect("a tool result is { output: <text> }")
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

async fn docker_available() -> bool {
    let check = tokio::process::Command::new("docker")
        .arg("info")
        .arg("--format")
        .arg("{{.ServerVersion}}")
        .output();
    matches!(
        tokio::time::timeout(Duration::from_secs(15), check).await,
        Ok(Ok(output)) if output.status.success()
    )
}

async fn container_exists(task: Uuid) -> bool {
    let name = format!("dhi-task-{task}");
    let check = tokio::process::Command::new("docker")
        .args(["inspect", "-f", "{{.State.Running}}", &name])
        .output();
    matches!(
        tokio::time::timeout(Duration::from_secs(10), check).await,
        Ok(Ok(output)) if output.status.success()
    )
}

/// Whether `task`'s container exists and is running.
async fn container_running(task: Uuid) -> bool {
    let name = format!("dhi-task-{task}");
    let check = tokio::process::Command::new("docker")
        .args(["inspect", "-f", "{{.State.Running}}", &name])
        .output();
    matches!(
        tokio::time::timeout(Duration::from_secs(10), check).await,
        Ok(Ok(output))
            if output.status.success()
                && String::from_utf8_lossy(&output.stdout).trim() == "true"
    )
}

/// The id of `task`'s container, or `None` when it does not exist.
async fn container_id(task: Uuid) -> Option<String> {
    let name = format!("dhi-task-{task}");
    let check = tokio::process::Command::new("docker")
        .args(["inspect", "-f", "{{.Id}}", &name])
        .output();
    match tokio::time::timeout(Duration::from_secs(10), check).await {
        Ok(Ok(output)) if output.status.success() => {
            Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        }
        _ => None,
    }
}

/// The full `docker inspect` object of `task`'s container.
async fn inspect(task: Uuid) -> Value {
    let name = format!("dhi-task-{task}");
    let output = tokio::process::Command::new("docker")
        .args(["inspect", &name])
        .output()
        .await
        .expect("docker inspect runs");
    assert!(
        output.status.success(),
        "docker inspect {name}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut value: Value = serde_json::from_slice(&output.stdout).expect("docker inspect is JSON");
    value
        .as_array_mut()
        .and_then(|entries| (entries.len() == 1).then(|| entries.remove(0)))
        .expect("one container is inspected")
}

/// Runs `docker` with `args`, failing the test on a non-zero status.
async fn docker(args: &[&str]) -> std::process::Output {
    let output = tokio::process::Command::new("docker")
        .args(args)
        .output()
        .await
        .expect("docker runs");
    assert!(
        output.status.success(),
        "docker {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// A container a test starts outside `TaskContainers`, removed on drop.
struct ForeignContainer {
    name: String,
}

impl ForeignContainer {
    /// Starts a plain container under `name` with no `dhi.*` labels.
    async fn start(name: String, image: &str) -> Self {
        docker(&[
            "run",
            "-d",
            "--rm",
            "--name",
            &name,
            "--network",
            "none",
            "--entrypoint",
            "sh",
            image,
            "-c",
            "sleep infinity",
        ])
        .await;
        Self { name }
    }

    /// Starts a container that carries only the labels a manager uses for
    /// ownership, not the root or config labels.
    async fn start_managed_like(name: String, image: &str, task: Uuid, root: &Path) -> Self {
        let root_label = format!(
            "dhi.root={}",
            hex(&Sha256::digest(root.as_os_str().as_encoded_bytes()))
        );
        let task_label = format!("dhi.task={task}");
        docker(&[
            "run",
            "-d",
            "--rm",
            "--name",
            &name,
            "--label",
            "dhi.managed=1",
            "--label",
            &task_label,
            "--label",
            &root_label,
            "--network",
            "none",
            "--entrypoint",
            "sh",
            image,
            "-c",
            "sleep infinity",
        ])
        .await;
        Self { name }
    }
}

impl Drop for ForeignContainer {
    fn drop(&mut self) {
        let _ = std::process::Command::new("docker")
            .args(["rm", "-f", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Lowercase hexadecimal, the form `dhi.root` uses.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(text, "{byte:02x}").expect("writing to a String cannot fail");
    }
    text
}

/// The `shell-*.txt` names directly in `dir`, for checking that a host spill
/// never landed outside the task.
fn shell_spills_in(dir: &Path) -> BTreeSet<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return BTreeSet::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("shell-") && name.ends_with(".txt"))
        })
        .collect()
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn shell_pwd_is_work() {
    let Some(fixture) = Fixture::docker().await else {
        return;
    };
    let task = fixture.task();

    let text = fixture.shell(task, "pwd").await.unwrap();

    assert!(
        text.lines().any(|line| line.trim() == "/work"),
        "the shell starts in /work: {text}"
    );
    assert!(text.contains("[exit code 0]"), "{text}");
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn write_then_read_works_inside_the_container() {
    let Some(fixture) = Fixture::docker().await else {
        return;
    };
    let task = fixture.task();

    fixture
        .call(
            task,
            "write",
            json!({ "path": "notes.txt", "content": "hello from the container\n" }),
        )
        .await
        .unwrap();

    let read = fixture
        .call(task, "read", json!({ "path": "notes.txt" }))
        .await
        .unwrap();
    assert_eq!(output(&read), "hello from the container\n");

    let runtime = fixture.containers.ensure(task).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(runtime.host_dir.join("notes.txt")).unwrap(),
        "hello from the container\n",
        "the one mount is the task folder"
    );
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn a_host_file_outside_the_task_folder_is_not_readable() {
    let Some(fixture) = Fixture::docker().await else {
        return;
    };
    let task = fixture.task();
    let outside = fixture._temp.path().join("outside-secret.txt");
    std::fs::write(&outside, "host secret\n").unwrap();

    let command = format!(
        "test -e {} && echo visible || echo blocked",
        shell_quote(&outside.to_string_lossy())
    );
    let text = fixture.shell(task, &command).await.unwrap();
    assert!(text.contains("blocked"), "{text}");
    assert!(!text.contains("visible"), "{text}");

    let err = fixture
        .call(task, "read", json!({ "path": outside.to_string_lossy() }))
        .await
        .expect_err("the host path is outside /work");
    assert!(err.to_string().contains("outside"), "{err}");
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn task_a_cannot_read_task_bs_file() {
    let Some(fixture) = Fixture::docker().await else {
        return;
    };
    let task_a = fixture.task();
    let task_b = fixture.task();

    fixture
        .call(
            task_a,
            "write",
            json!({ "path": "only-a.txt", "content": "from a\n" }),
        )
        .await
        .unwrap();

    let err = fixture
        .call(task_b, "read", json!({ "path": "only-a.txt" }))
        .await
        .expect_err("task B has its own /work");
    assert!(err.to_string().contains("no such file"), "{err}");

    let text = fixture
        .shell(
            task_b,
            "test -e /work/only-a.txt && echo visible || echo blocked",
        )
        .await
        .unwrap();
    assert!(text.contains("blocked"), "{text}");
    assert!(!text.contains("visible"), "{text}");
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn shell_cannot_see_the_host_repository() {
    let Some(fixture) = Fixture::docker().await else {
        return;
    };
    let task = fixture.task();
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"));

    let command = format!(
        "test -e {} && echo visible || echo blocked",
        shell_quote(&repository.to_string_lossy())
    );
    let text = fixture.shell(task, &command).await.unwrap();
    assert!(text.contains("blocked"), "{text}");
    assert!(!text.contains("visible"), "{text}");
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn shell_has_no_network_by_default() {
    let Some(fixture) = Fixture::docker().await else {
        return;
    };
    let task = fixture.task();

    let text = fixture
        .shell(
            task,
            r#"python3 -c "import socket; socket.create_connection(('1.1.1.1', 443), timeout=2)""#,
        )
        .await
        .unwrap();

    assert!(text.contains("[exit code 1]"), "{text}");
    assert!(
        text.contains("Network is unreachable") || text.contains("OSError"),
        "{text}"
    );
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn cut_shell_output_names_a_container_spill_path() {
    let Some(fixture) = Fixture::docker().await else {
        return;
    };
    let task = fixture.task();
    let host_dir = fixture
        .containers
        .ensure(task)
        .await
        .unwrap()
        .host_dir
        .to_string_lossy()
        .into_owned();

    let text = fixture
        .shell(
            task,
            r#"awk 'BEGIN { for (i = 0; i < 3000; i++) print "line" i }'"#,
        )
        .await
        .unwrap();

    assert!(text.contains(".dhi/spill/shell-"), "{text}");
    assert!(
        !text.contains(&host_dir),
        "the host spill path is translated to /work: {text}"
    );

    let read = fixture
        .call(task, "read", json!({ "path": ".dhi/spill/shell-1.txt" }))
        .await
        .unwrap();
    assert!(output(&read).contains("line0"), "{read}");
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn remove_and_shutdown_stop_the_containers() {
    let Some(fixture) = Fixture::docker().await else {
        return;
    };
    let task_a = fixture.task();
    let task_b = fixture.task();
    let task_c = fixture.task();

    for task in [task_a, task_b, task_c] {
        fixture.shell(task, "pwd").await.unwrap();
        assert!(container_exists(task).await, "task {task} started");
    }

    fixture.containers.remove(task_a).await.unwrap();
    assert!(!container_exists(task_a).await, "remove stopped task A");

    fixture.containers.shutdown().await.unwrap();
    assert!(!container_exists(task_b).await, "shutdown stopped task B");
    assert!(!container_exists(task_c).await, "shutdown stopped task C");
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn project_mounts_are_read_only_and_hide_dhi() {
    let Some(fixture) = Fixture::docker().await else {
        return;
    };
    let project = fixture._temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("README.md"), "project text\n").unwrap();
    let task = fixture.task();
    fixture
        .containers
        .set_task_project(task, Some(&project))
        .unwrap();

    let command = format!(
        "test -e /project/README.md && echo readme; \
         touch /project/dhi-probe 2>/dev/null && echo project-writable || echo project-readonly; \
         entries=$(ls -A /project/.dhi 2>/dev/null); \
         [ -z \"$entries\" ] && echo dhi-hidden || echo dhi-visible; \
         test -e /project/.dhi/{} && echo sibling-visible || echo sibling-hidden",
        task
    );
    let text = fixture.shell(task, &command).await.unwrap();
    assert!(text.contains("readme"), "{text}");
    assert!(text.contains("project-readonly"), "{text}");
    assert!(text.contains("dhi-hidden"), "{text}");
    assert!(text.contains("sibling-hidden"), "{text}");

    let info = inspect(task).await;
    assert!(
        info["HostConfig"]["Tmpfs"]["/project/.dhi"]
            .as_str()
            .is_some_and(|options| options.contains("ro")),
        "the .dhi tmpfs is read-only: {}",
        info["HostConfig"]["Tmpfs"]
    );
    let mounts = info["Mounts"].as_array().unwrap();
    let project_mounts: Vec<&Value> = mounts
        .iter()
        .filter(|mount| mount["Destination"] == "/project")
        .collect();
    assert_eq!(project_mounts.len(), 1, "{mounts:?}");
    assert_eq!(project_mounts[0]["Type"], "bind");
    assert_eq!(project_mounts[0]["RW"], false);
    assert_eq!(
        project_mounts[0]["Source"],
        std::fs::canonicalize(&project)
            .unwrap()
            .to_string_lossy()
            .as_ref()
    );

    let read = fixture
        .call(task, "read", json!({ "path": "/project/README.md" }))
        .await
        .unwrap();
    assert!(output(&read).contains("project text"), "{read}");

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("/etc/hostname", project.join("escape.txt")).unwrap();
        let err = fixture
            .call(task, "read", json!({ "path": "/project/escape.txt" }))
            .await
            .expect_err("a project symlink escape is refused");
        assert!(err.to_string().contains("outside"), "{err}");
    }

    fixture.containers.remove(task).await.unwrap();
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn a_container_cannot_replace_the_read_only_spill_mount() {
    let Some(fixture) = Fixture::docker().await else {
        return;
    };
    let task = fixture.task();
    let runtime = fixture.containers.ensure(task).await.unwrap();
    let host_spill = runtime.host_dir.join(".dhi/spill");
    let spill_before = runtime
        .spill
        .0
        .read_dir()
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect::<BTreeSet<_>>();
    let tmp_before = shell_spills_in(Path::new("/tmp"));
    let temp_before = shell_spills_in(&std::env::temp_dir());

    // The container tries to replace the host spill directory with a
    // symbolic link. The nested read-only mounts keep both commands from
    // touching the host directory, and keep `.dhi` itself from being moved
    // aside and replaced.
    let attack = fixture
        .shell(
            task,
            "rm -rf /work/.dhi/spill; ln -s /tmp /work/.dhi/spill; \
             mv /work/.dhi /work/.dhi-moved 2>/dev/null; ln -s /tmp /work/.dhi 2>/dev/null; true",
        )
        .await
        .unwrap();
    assert!(!attack.contains("Traceback"), "{attack}");

    let shape = fixture
        .shell(
            task,
            "if [ -L /work/.dhi/spill ]; then echo symlink; \
             elif [ -d /work/.dhi/spill ]; then echo directory; \
             else echo missing; fi; \
             touch /work/.dhi/spill/probe 2>/dev/null && echo writable || echo readonly; \
             if [ -L /work/.dhi ]; then echo dhi-symlink; else echo dhi-ok; fi; \
             if [ -e /work/.dhi-moved ]; then echo moved; else echo not-moved; fi",
        )
        .await
        .unwrap();
    assert!(shape.contains("directory"), "{shape}");
    assert!(!shape.contains("symlink"), "{shape}");
    assert!(shape.contains("readonly"), "{shape}");
    assert!(shape.contains("dhi-ok"), "{shape}");
    assert!(shape.contains("not-moved"), "{shape}");

    // Force a cut, which makes the host spill writer create a file.
    let text = fixture
        .shell(
            task,
            r#"awk 'BEGIN { for (i = 0; i < 3000; i++) print "line" i }'"#,
        )
        .await
        .unwrap();
    assert!(text.contains("lines cut"), "{text}");
    assert!(text.contains(".dhi/spill/shell-"), "{text}");

    let metadata = std::fs::symlink_metadata(&host_spill).unwrap();
    assert!(
        !metadata.file_type().is_symlink(),
        "the host spill directory stayed a real directory"
    );
    assert!(metadata.is_dir(), "{} is a directory", host_spill.display());
    let host_dhi = host_spill.parent().unwrap();
    let dhi_metadata = std::fs::symlink_metadata(host_dhi).unwrap();
    assert!(
        !dhi_metadata.file_type().is_symlink(),
        "the host .dhi directory stayed a real directory"
    );
    assert!(
        dhi_metadata.is_dir(),
        "{} is a directory",
        host_dhi.display()
    );
    assert!(
        !runtime.host_dir.join(".dhi-moved").exists(),
        "the container did not move .dhi"
    );
    let spill_after = std::fs::read_dir(&host_spill)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect::<BTreeSet<_>>();
    assert!(
        spill_after.iter().any(|path| path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("shell-") && name.ends_with(".txt"))),
        "the host spill file is under {}: {spill_after:?}",
        host_spill.display()
    );
    assert!(
        spill_after
            .difference(&spill_before)
            .all(|path| path.parent().is_some_and(|parent| parent == host_spill)),
        "every new spill file is in the task's host spill directory"
    );
    assert_eq!(
        shell_spills_in(Path::new("/tmp")),
        tmp_before,
        "no spill file was written through a symlink into /tmp"
    );
    assert_eq!(
        shell_spills_in(&std::env::temp_dir()),
        temp_before,
        "no spill file was written outside the task"
    );
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn ensure_rejects_a_symlinked_spill_directory_before_starting_a_container() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let containers =
        TaskContainers::new(temp.path().join("tasks"), ContainerConfig::default()).unwrap();
    let task = Uuid::new_v4();
    let task_dir = containers.root().join(task.to_string());
    std::fs::create_dir_all(task_dir.join(".dhi")).unwrap();
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    symlink(&outside, task_dir.join(".dhi/spill")).unwrap();

    let err = containers
        .ensure(task)
        .await
        .expect_err("a symlinked spill directory is refused before a container starts");
    assert!(err.to_string().contains("symbolic link"), "{err}");
    assert!(!container_exists(task).await, "no container was started");
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn search_list_and_edit_work_in_the_container_and_after_a_restart() {
    let Some(fixture) = Fixture::docker().await else {
        return;
    };
    let task = fixture.task();

    fixture
        .call(
            task,
            "write",
            json!({
                "path": "src/lib.rs",
                "content": "fn main() {}\nlet needle = 42;\n"
            }),
        )
        .await
        .unwrap();

    let found = fixture
        .call(task, "search", json!({ "pattern": r"needle\s*=\s*42" }))
        .await
        .unwrap();
    assert!(output(&found).contains("needle = 42"), "{found}");

    let listed = fixture
        .call(task, "list", json!({ "path": "." }))
        .await
        .unwrap();
    assert!(output(&listed).contains("src"), "{listed}");

    let edited = fixture
        .call(
            task,
            "edit",
            json!({
                "path": "src/lib.rs",
                "edits": [{ "old_text": "needle = 42", "new_text": "answer = 7" }]
            }),
        )
        .await
        .unwrap();
    assert!(output(&edited).contains("src/lib.rs"), "{edited}");

    let first_id = container_id(task).await.expect("the task container exists");
    fixture.containers.restart(task).await.unwrap();
    let second_id = container_id(task)
        .await
        .expect("the restarted task container exists");
    assert_ne!(first_id, second_id, "restart replaced the container");

    let found = fixture
        .call(task, "search", json!({ "pattern": "answer" }))
        .await
        .unwrap();
    assert!(output(&found).contains("answer = 7"), "{found}");
    let listed = fixture
        .call(task, "list", json!({ "path": "src" }))
        .await
        .unwrap();
    assert!(output(&listed).contains("lib.rs"), "{listed}");
    let edited = fixture
        .call(
            task,
            "edit",
            json!({
                "path": "src/lib.rs",
                "edits": [{ "old_text": "answer = 7", "new_text": "answer = 9" }]
            }),
        )
        .await
        .unwrap();
    assert!(output(&edited).contains("src/lib.rs"), "{edited}");
    let read = fixture
        .call(task, "read", json!({ "path": "src/lib.rs" }))
        .await
        .unwrap();
    assert!(output(&read).contains("answer = 9"), "{read}");
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn a_shell_timeout_kills_the_command_and_restarts_the_container() {
    let Some(fixture) = Fixture::docker().await else {
        return;
    };
    let task = fixture.task();
    fixture.shell(task, "pwd").await.unwrap();

    let started = Instant::now();
    let result = fixture
        .call(
            task,
            "shell",
            json!({ "command": "sleep 30", "timeout_secs": 1 }),
        )
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "the timed-out call returned after {elapsed:?}"
    );
    let text = output(&result);
    assert!(text.contains("timed out"), "{text}");

    // The command's `sh` and its `sleep` must be gone. `ps` and `pgrep` are
    // not in the default image, so read `/proc` directly and compare the
    // whole command line, not a substring of this check script.
    let survivors = fixture
        .shell(
            task,
            r#"found=0; for p in /proc/[0-9]*; do \
                 args=$(tr '\0' ' ' < "$p/cmdline" 2>/dev/null); \
                 [ "$args" = "sleep 30 " ] && found=1; \
               done; \
               [ "$found" = 1 ] && echo survivor || echo none"#,
        )
        .await
        .unwrap();
    assert!(
        survivors.contains("none"),
        "no sleep 30 survived: {survivors}"
    );
    assert!(!survivors.contains("survivor"), "{survivors}");

    let text = fixture.shell(task, "pwd").await.unwrap();
    assert!(text.lines().any(|line| line.trim() == "/work"), "{text}");
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn container_flags_and_environment_are_the_hardened_ones() {
    const SECRET: &str = "DHI_EXEC_TEST_HOST_SECRET";
    // SAFETY: this test is serial; every Docker test in this binary runs one
    // at a time and no other test reads this name.
    unsafe { std::env::set_var(SECRET, "must-not-leak") };
    let Some(fixture) = Fixture::docker().await else {
        unsafe { std::env::remove_var(SECRET) };
        return;
    };
    let task = fixture.task();
    let config = ContainerConfig::default();

    let text = fixture
        .shell(
            task,
            "printf '%s\\n%s\\n%s\\n' \"$TMPDIR\" \"$HOME\" \"$CARGO_HOME\"; \
             printenv DHI_EXEC_TEST_HOST_SECRET || echo secret-absent",
        )
        .await
        .unwrap();
    // SAFETY: as above; the test set it.
    unsafe { std::env::remove_var(SECRET) };
    assert!(text.contains("/work/.tmp"), "{text}");
    assert!(text.contains("/work/.home"), "{text}");
    assert!(text.contains("/work/.cargo"), "{text}");
    assert!(text.contains("secret-absent"), "{text}");
    assert!(!text.contains("must-not-leak"), "{text}");

    let info = inspect(task).await;
    let host = &info["HostConfig"];
    let labels = &info["Config"]["Labels"];
    let runtime = fixture.containers.ensure(task).await.unwrap();
    let root_hash = hex(&Sha256::digest(
        fixture.containers.root().as_os_str().as_encoded_bytes(),
    ));

    assert_eq!(labels["dhi.managed"], "1");
    assert_eq!(labels["dhi.task"], task.to_string());
    assert_eq!(labels["dhi.root"], root_hash);
    assert!(
        labels["dhi.config"]
            .as_str()
            .is_some_and(|value| value.len() == 64),
        "{labels}"
    );

    assert_eq!(host["ReadonlyRootfs"], true);
    assert!(
        host["CapDrop"]
            .as_array()
            .is_some_and(|caps| caps.iter().any(|cap| cap == "ALL")),
        "{host}"
    );
    assert!(
        host["SecurityOpt"]
            .as_array()
            .is_some_and(|opts| opts.iter().any(|opt| opt
                .as_str()
                .is_some_and(|opt| opt.contains("no-new-privileges")))),
        "{host}"
    );
    assert_eq!(host["NetworkMode"], "none");
    let memory = host["Memory"].as_i64().unwrap();
    assert!(memory > 0, "{host}");
    assert_eq!(host["MemorySwap"].as_i64(), Some(memory), "{host}");
    assert!(
        host["NanoCpus"].as_i64().is_some_and(|cpus| cpus > 0),
        "{host}"
    );
    assert_eq!(
        host["PidsLimit"].as_i64(),
        Some(i64::from(config.pids_limit))
    );
    assert_eq!(host["Init"], true);
    assert_eq!(host["AutoRemove"], true);
    #[cfg(unix)]
    {
        // SAFETY: getuid and getgid only read this process's ids.
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        assert_eq!(info["Config"]["User"], format!("{uid}:{gid}"));
    }
    assert!(
        host["Ulimits"].as_array().is_some_and(|limits| limits
            .iter()
            .any(|limit| limit["Name"] == "nofile"
                && limit["Soft"] == 1024
                && limit["Hard"] == 1024)),
        "{host}"
    );
    assert!(
        host["Tmpfs"]["/tmp"]
            .as_str()
            .is_some_and(|opts| opts.contains("noexec")),
        "{host}"
    );

    let env: Vec<&str> = info["Config"]["Env"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    for expected in [
        "HOME=/work/.home",
        "TMPDIR=/work/.tmp",
        "CARGO_HOME=/work/.cargo",
    ] {
        assert!(env.contains(&expected), "{expected} is in {env:?}");
    }
    assert!(
        !env.iter()
            .any(|entry| entry.starts_with("DHI_EXEC_TEST_HOST_SECRET=")),
        "the host secret is not in the container environment: {env:?}"
    );

    let mounts = info["Mounts"].as_array().unwrap();
    let work: Vec<&Value> = mounts
        .iter()
        .filter(|mount| mount["Destination"] == "/work")
        .collect();
    assert_eq!(work.len(), 1, "exactly one mount at /work: {mounts:?}");
    assert_eq!(work[0]["Type"], "bind");
    assert_eq!(work[0]["RW"], true);
    assert_eq!(
        work[0]["Source"],
        runtime.host_dir.to_string_lossy().as_ref()
    );
    let spill: Vec<&Value> = mounts
        .iter()
        .filter(|mount| mount["Destination"] == "/work/.dhi/spill")
        .collect();
    assert_eq!(spill.len(), 1, "one nested mount at the spill path");
    assert_eq!(spill[0]["Type"], "bind");
    assert_eq!(spill[0]["RW"], false);
    assert_eq!(
        spill[0]["Source"],
        runtime.spill.0.to_string_lossy().as_ref()
    );
    let dhi: Vec<&Value> = mounts
        .iter()
        .filter(|mount| mount["Destination"] == "/work/.dhi")
        .collect();
    assert_eq!(dhi.len(), 1, "one nested mount at the .dhi path");
    assert_eq!(dhi[0]["RW"], false, "the .dhi parent is read-only too");

    let text = fixture
        .shell(
            task,
            "touch /etc/dhi-probe 2>/dev/null && echo etc-writable || echo etc-readonly; \
             touch /root/dhi-probe 2>/dev/null && echo root-writable || echo root-readonly; \
             touch /work/dhi-probe && echo work-writable || echo work-readonly; \
             touch /work/.tmp/dhi-probe && echo tmpdir-writable || echo tmpdir-readonly",
        )
        .await
        .unwrap();
    assert!(text.contains("etc-readonly"), "{text}");
    assert!(text.contains("root-readonly"), "{text}");
    assert!(text.contains("work-writable"), "{text}");
    assert!(text.contains("tmpdir-writable"), "{text}");
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn a_matching_running_container_is_reused() {
    let Some(fixture) = Fixture::docker().await else {
        return;
    };
    let task = fixture.task();
    fixture.shell(task, "pwd").await.unwrap();
    let first_id = container_id(task).await.expect("the task container exists");

    let other = TaskContainers::new(fixture.containers.root(), ContainerConfig::default()).unwrap();
    other.ensure(task).await.unwrap();
    let second_id = container_id(task).await.expect("the task container exists");
    assert_eq!(
        first_id, second_id,
        "a matching running container is reused"
    );

    other.shutdown().await.unwrap();
    assert!(!container_exists(task).await);
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn ensure_replaces_a_dead_cached_container() {
    let Some(fixture) = Fixture::docker().await else {
        return;
    };
    let task = fixture.task();
    let name = format!("dhi-task-{task}");

    let first = fixture.containers.ensure(task).await.unwrap();
    let first_id = container_id(task).await.expect("the task container exists");
    assert!(
        container_running(task).await,
        "the first task container is running"
    );

    docker(&["kill", &name]).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while container_running(task).await {
        assert!(
            Instant::now() < deadline,
            "the killed task container did not stop"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let second = fixture.containers.ensure(task).await.unwrap();

    assert!(
        !Arc::ptr_eq(&first, &second),
        "ensure did not return the dead cached runtime"
    );
    assert!(
        container_running(task).await,
        "ensure started a running task container"
    );
    let second_id = container_id(task)
        .await
        .expect("the recovered task container exists");
    assert_ne!(first_id, second_id, "ensure replaced the dead container");

    let text = fixture.shell(task, "echo recovered").await.unwrap();
    assert!(text.contains("recovered"), "{text}");
    assert!(text.contains("[exit code 0]"), "{text}");

    fixture.containers.remove(task).await.unwrap();
    assert!(
        !container_exists(task).await,
        "remove cleaned up the recovered container"
    );
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn a_changed_configuration_restarts_the_container() {
    let Some(fixture) = Fixture::docker().await else {
        return;
    };
    let task = fixture.task();
    fixture.shell(task, "pwd").await.unwrap();
    let first_id = container_id(task).await.expect("the task container exists");
    let first = inspect(task).await;

    let default = ContainerConfig::default();
    let cpus = if default.cpus == "1" { "2" } else { "1" }.to_owned();
    let changed = ContainerConfig { cpus, ..default };
    let other = TaskContainers::new(fixture.containers.root(), changed).unwrap();
    other.ensure(task).await.unwrap();

    let second_id = container_id(task).await.expect("the task container exists");
    assert_ne!(first_id, second_id, "a config mismatch restarts");
    let second = inspect(task).await;
    assert_ne!(
        first["Config"]["Labels"]["dhi.config"], second["Config"]["Labels"]["dhi.config"],
        "the config label records the change"
    );

    other.shutdown().await.unwrap();
    assert!(!container_exists(task).await);
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn a_container_from_another_root_is_refused_and_not_removed() {
    let Some(fixture) = Fixture::docker().await else {
        return;
    };
    let task = fixture.task();
    fixture.shell(task, "pwd").await.unwrap();
    let running_id = container_id(task).await.expect("the task container exists");

    let other_root = tempfile::tempdir().unwrap();
    let other = TaskContainers::new(other_root.path(), ContainerConfig::default()).unwrap();
    let err = other
        .ensure(task)
        .await
        .expect_err("another root's container is refused");
    assert!(err.to_string().contains("different task root"), "{err}");
    assert_eq!(
        container_id(task).await.as_deref(),
        Some(running_id.as_str()),
        "the other root's container was not removed"
    );
    let err = other
        .remove(task)
        .await
        .expect_err("remove also refuses another root's container");
    assert!(err.to_string().contains("different task root"), "{err}");
    assert_eq!(
        container_id(task).await.as_deref(),
        Some(running_id.as_str()),
        "remove did not remove the other root's container"
    );
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn a_container_without_the_managed_label_is_refused_and_not_removed() {
    let Some(fixture) = Fixture::docker().await else {
        return;
    };
    let task = fixture.task();
    let name = format!("dhi-task-{task}");
    let foreign = ForeignContainer::start(name.clone(), &ContainerConfig::default().image).await;

    let err = fixture
        .containers
        .ensure(task)
        .await
        .expect_err("an unmanaged container is refused");
    assert!(err.to_string().contains("not managed"), "{err}");
    assert!(
        container_exists(task).await,
        "the unmanaged container was not removed"
    );

    let err = fixture
        .containers
        .remove(task)
        .await
        .expect_err("remove refuses an unmanaged container");
    assert!(err.to_string().contains("not managed"), "{err}");
    assert!(
        container_exists(task).await,
        "remove did not remove the unmanaged container"
    );
    drop(foreign);
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn remove_keeps_the_cache_until_docker_removal_succeeds() {
    let Some(fixture) = Fixture::docker().await else {
        return;
    };
    let task = fixture.task();
    let name = format!("dhi-task-{task}");
    fixture.shell(task, "pwd").await.unwrap();
    let image = ContainerConfig::default().image;

    // Replace the managed container with an unmanaged one. Removal must fail
    // and must leave the runtime cached for a later shutdown to retry.
    docker(&["rm", "-f", &name]).await;
    let foreign = ForeignContainer::start(name.clone(), &image).await;
    let err = fixture
        .containers
        .remove(task)
        .await
        .expect_err("a foreign container is not removed");
    assert!(err.to_string().contains("not managed"), "{err}");
    assert!(container_exists(task).await);

    // Put a managed container back under the same name. If the failed
    // removal had dropped the cache entry, shutdown would have nothing to
    // retry and would leave this container running.
    drop(foreign);
    let managed =
        ForeignContainer::start_managed_like(name.clone(), &image, task, fixture.containers.root())
            .await;
    fixture.containers.shutdown().await.unwrap();
    assert!(
        !container_exists(task).await,
        "shutdown retried and removed the cached task container"
    );
    drop(managed);
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn task_isolation_still_holds_after_restarts() {
    let Some(fixture) = Fixture::docker().await else {
        return;
    };
    let task_a = fixture.task();
    let task_b = fixture.task();

    fixture
        .call(
            task_a,
            "write",
            json!({ "path": "only-a.txt", "content": "from a\n" }),
        )
        .await
        .unwrap();
    fixture
        .call(
            task_b,
            "write",
            json!({ "path": "only-b.txt", "content": "from b\n" }),
        )
        .await
        .unwrap();

    fixture.containers.restart(task_a).await.unwrap();
    fixture.containers.restart(task_b).await.unwrap();

    let read_a = fixture
        .call(task_a, "read", json!({ "path": "only-a.txt" }))
        .await
        .unwrap();
    assert_eq!(output(&read_a), "from a\n");
    let err = fixture
        .call(task_a, "read", json!({ "path": "only-b.txt" }))
        .await
        .expect_err("task A cannot read task B's file");
    assert!(err.to_string().contains("no such file"), "{err}");
    let text = fixture
        .shell(
            task_a,
            "test -e /work/only-b.txt && echo visible || echo blocked",
        )
        .await
        .unwrap();
    assert!(text.contains("blocked"), "{text}");
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn reap_orphans_removes_containers_left_by_a_previous_manager() {
    if !docker_available().await {
        eprintln!("skipping: Docker is not available");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("tasks");
    let task = Uuid::new_v4();

    {
        let first = TaskContainers::new(&root, ContainerConfig::default()).unwrap();
        first.ensure(task).await.unwrap();
        assert!(
            container_exists(task).await,
            "the first manager started its container"
        );
        // Dropped without `shutdown`: the container is an orphan.
    }
    assert!(
        container_exists(task).await,
        "the container outlives the manager that started it"
    );

    let second = TaskContainers::new(&root, ContainerConfig::default()).unwrap();
    assert_eq!(
        second.reap_orphans().await.unwrap(),
        1,
        "startup cleanup removed the previous process's container"
    );
    assert!(
        !container_exists(task).await,
        "the orphaned container is gone"
    );
    assert_eq!(
        second.reap_orphans().await.unwrap(),
        0,
        "a second cleanup finds nothing"
    );
    second.shutdown().await.unwrap();
}

#[tokio::test]
#[ignore = "requires Docker"]
#[serial_test::serial]
async fn shutdown_reports_failure_while_a_cached_container_cannot_be_removed() {
    let Some(fixture) = Fixture::docker().await else {
        return;
    };
    let task = fixture.task();
    let name = format!("dhi-task-{task}");
    let image = ContainerConfig::default().image;
    fixture.shell(task, "pwd").await.unwrap();

    // Replace the managed container with an unmanaged one. The manager still
    // has the task cached, but cannot force-remove a container it does not
    // own, so shutdown must fail rather than report success.
    docker(&["rm", "-f", &name]).await;
    let foreign = ForeignContainer::start(name.clone(), &image).await;
    let err = fixture
        .containers
        .shutdown()
        .await
        .expect_err("shutdown does not report success while the cache entry remains");
    assert!(
        err.to_string()
            .contains("could not remove every task container"),
        "{err}"
    );
    assert!(
        container_exists(task).await,
        "the foreign container was not removed"
    );
    drop(foreign);
}
