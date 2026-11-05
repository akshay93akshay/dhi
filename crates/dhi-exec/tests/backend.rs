//! The file tools, driven through the guarded public tool registration.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use dhi_agent::{RunScope, ToolSink};
use dhi_core::{ToolCallOutcome, ToolCallRequest};
use dhi_exec::{CodingBackend, CodingTools, TaskContainers};
use dhi_guard::ToolBackend;
use rig::tool::ToolContext;
use rig::tool::server::{ToolServer, ToolServerHandle};
use serde_json::{Value, json};
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
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let containers = Arc::new(TaskContainers::local(temp.path()).unwrap());
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
        Self {
            _temp: temp,
            containers,
            handle,
        }
    }

    fn root(&self) -> &Path {
        self.containers.root()
    }

    async fn task_dir(&self, task: Uuid) -> PathBuf {
        self.containers.ensure(task).await.unwrap().host_dir.clone()
    }

    /// Calls a tool the way the guarded wrapper does: through the registered
    /// tool with the run scope in the dispatch context.
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
}

fn output(value: &Value) -> &str {
    value
        .get("output")
        .and_then(Value::as_str)
        .expect("a tool result is { output: <text> }")
}

#[tokio::test]
async fn write_then_read_returns_the_text() {
    let fixture = Fixture::new();
    let task = Uuid::new_v4();

    fixture
        .call(
            task,
            "write",
            json!({ "path": "notes.txt", "content": "hello\n" }),
        )
        .await
        .unwrap();

    let read = fixture
        .call(task, "read", json!({ "path": "notes.txt" }))
        .await
        .unwrap();

    assert_eq!(output(&read), "hello\n");
}

#[tokio::test]
async fn edit_changes_one_exact_match_and_a_missing_text_is_an_error() {
    let fixture = Fixture::new();
    let task = Uuid::new_v4();
    fixture
        .call(
            task,
            "write",
            json!({ "path": "edit.txt", "content": "one\ntwo\nthree\n" }),
        )
        .await
        .unwrap();

    let edited = fixture
        .call(
            task,
            "edit",
            json!({
                "path": "edit.txt",
                "edits": [{ "old_text": "two", "new_text": "2" }]
            }),
        )
        .await
        .unwrap();
    assert!(output(&edited).contains("Edited edit.txt"));

    let read = fixture
        .call(task, "read", json!({ "path": "edit.txt" }))
        .await
        .unwrap();
    assert_eq!(output(&read), "one\n2\nthree\n");

    let err = fixture
        .call(
            task,
            "edit",
            json!({
                "path": "edit.txt",
                "edits": [{ "old_text": "not there", "new_text": "x" }]
            }),
        )
        .await
        .expect_err("a missing old_text is refused");
    assert!(err.to_string().contains("not found"), "{err}");
}

#[tokio::test]
async fn search_finds_a_regex_match() {
    let fixture = Fixture::new();
    let task = Uuid::new_v4();
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
}

#[tokio::test]
async fn list_lists_a_file() {
    let fixture = Fixture::new();
    let task = Uuid::new_v4();
    fixture
        .call(
            task,
            "write",
            json!({ "path": "listed.txt", "content": "here\n" }),
        )
        .await
        .unwrap();

    let listed = fixture
        .call(task, "list", json!({ "path": "." }))
        .await
        .unwrap();

    assert!(output(&listed).contains("listed.txt"), "{listed}");
}

#[tokio::test]
async fn paths_outside_the_task_directory_are_refused() {
    let fixture = Fixture::new();
    let task = Uuid::new_v4();
    let outside = fixture.root().join("outside.txt");

    let relative = fixture
        .call(
            task,
            "write",
            json!({ "path": "../outside.txt", "content": "escaped\n" }),
        )
        .await
        .expect_err("../outside is outside the task directory");
    assert!(relative.to_string().contains("outside"), "{relative}");

    let absolute = fixture
        .call(
            task,
            "write",
            json!({ "path": outside.to_string_lossy(), "content": "escaped\n" }),
        )
        .await
        .expect_err("an absolute path outside the task directory is refused");
    assert!(absolute.to_string().contains("outside"), "{absolute}");

    assert!(
        !outside.exists(),
        "no refused write may reach outside the task directory"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_dangling_symlink_cannot_write_outside_the_task() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let task = Uuid::new_v4();
    let task_dir = fixture.task_dir(task).await;
    let outside = fixture.root().join("outside.txt");
    symlink(&outside, task_dir.join("dangling.txt")).unwrap();

    let err = fixture
        .call(
            task,
            "write",
            json!({ "path": "dangling.txt", "content": "escaped\n" }),
        )
        .await
        .expect_err("a dangling symlink is not a write target");
    assert!(err.to_string().contains("symbolic link"), "{err}");
    assert!(
        !outside.exists(),
        "the write did not follow the dangling symlink outside the task"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_to_a_sibling_task_file_cannot_be_written() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let task_a = Uuid::new_v4();
    let task_b = Uuid::new_v4();
    let dir_b = fixture.task_dir(task_b).await;
    std::fs::write(dir_b.join("secret.txt"), "from b\n").unwrap();
    let dir_a = fixture.task_dir(task_a).await;
    symlink(dir_b.join("secret.txt"), dir_a.join("link.txt")).unwrap();

    let err = fixture
        .call(
            task_a,
            "write",
            json!({ "path": "link.txt", "content": "overwritten\n" }),
        )
        .await
        .expect_err("a symlink into another task is not a write target");
    let text = err.to_string();
    assert!(
        text.contains("symbolic link") || text.contains("outside"),
        "{text}"
    );
    assert_eq!(
        std::fs::read_to_string(dir_b.join("secret.txt")).unwrap(),
        "from b\n",
        "task B's file was not changed"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_parent_inside_the_task_is_refused() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new();
    let task = Uuid::new_v4();
    let task_dir = fixture.task_dir(task).await;
    std::fs::create_dir_all(task_dir.join("real")).unwrap();
    symlink(task_dir.join("real"), task_dir.join("alias")).unwrap();

    let err = fixture
        .call(
            task,
            "write",
            json!({ "path": "alias/inside.txt", "content": "no\n" }),
        )
        .await
        .expect_err("a symlinked parent is refused even inside the task");
    assert!(err.to_string().contains("symbolic link"), "{err}");
    assert!(!task_dir.join("real/inside.txt").exists());
}

#[tokio::test]
async fn two_tasks_are_isolated() {
    let fixture = Fixture::new();
    let task_a = Uuid::new_v4();
    let task_b = Uuid::new_v4();

    fixture
        .call(
            task_a,
            "write",
            json!({ "path": "shared-name.txt", "content": "from a\n" }),
        )
        .await
        .unwrap();

    let read_a = fixture
        .call(task_a, "read", json!({ "path": "shared-name.txt" }))
        .await
        .unwrap();
    assert_eq!(output(&read_a), "from a\n");

    let read_b = fixture
        .call(task_b, "read", json!({ "path": "shared-name.txt" }))
        .await
        .expect_err("task B cannot read task A's file");
    assert!(read_b.to_string().contains("no such file"), "{read_b}");
}
