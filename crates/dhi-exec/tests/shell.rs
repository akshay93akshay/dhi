//! The shell tool, driven through the guarded public tool registration.

use std::sync::Arc;
use std::time::{Duration, Instant};

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

    async fn shell(
        &self,
        task: Uuid,
        command: &str,
        timeout_secs: Option<u64>,
    ) -> anyhow::Result<String> {
        let mut args = json!({ "command": command });
        if let Some(secs) = timeout_secs {
            args["timeout_secs"] = json!(secs);
        }
        let result = self.call(task, "shell", args).await?;
        Ok(output(&result).to_owned())
    }
}

fn output(value: &Value) -> &str {
    value
        .get("output")
        .and_then(Value::as_str)
        .expect("a tool result is { output: <text> }")
}

#[tokio::test]
async fn shell_runs_a_command_and_returns_its_output() {
    let fixture = Fixture::new();

    let text = fixture
        .shell(Uuid::new_v4(), "printf hello", None)
        .await
        .unwrap();

    assert!(text.contains("hello"), "{text}");
    assert!(text.contains("[exit code 0]"), "{text}");
}

#[tokio::test]
async fn shell_stops_a_command_at_its_timeout() {
    let fixture = Fixture::new();
    let started = Instant::now();

    let text = fixture
        .shell(Uuid::new_v4(), "sleep 5", Some(1))
        .await
        .unwrap();

    assert!(
        started.elapsed() < Duration::from_secs(4),
        "the timeout returned after {:?}",
        started.elapsed()
    );
    assert!(
        text.contains("timed out") || text.contains("[exit code"),
        "a timeout or non-zero status is reported: {text}"
    );
    assert!(text.contains("timed out"), "{text}");
}

#[tokio::test]
async fn shell_does_not_inherit_parent_secrets() {
    const SECRET: &str = "DHI_TEST_SECRET";
    // SAFETY: only this test uses DHI_TEST_SECRET; the shell launcher removes
    // every inherited variable before it starts a command.
    unsafe { std::env::set_var(SECRET, "must-not-leak") };
    let fixture = Fixture::new();

    let text = fixture
        .shell(
            Uuid::new_v4(),
            r#"if [ -n "$DHI_TEST_SECRET" ]; then echo leaked; else echo clean; fi"#,
            None,
        )
        .await;
    // SAFETY: as above, this test set the variable.
    unsafe { std::env::remove_var(SECRET) };

    let text = text.unwrap();
    assert!(text.contains("clean"), "{text}");
    assert!(!text.contains("leaked"), "{text}");
    assert!(!text.contains("must-not-leak"), "{text}");
}

#[tokio::test]
async fn shell_keeps_cut_output_in_the_task_spill_directory() {
    let fixture = Fixture::new();
    let task = Uuid::new_v4();

    let text = fixture
        .shell(
            task,
            r#"awk 'BEGIN { for (i = 0; i < 3000; i++) print "line" i }'"#,
            None,
        )
        .await
        .unwrap();

    assert!(text.contains("lines cut"), "{text}");
    let spill = fixture
        .containers
        .ensure(task)
        .await
        .unwrap()
        .host_dir
        .join(".dhi/spill");
    let files: Vec<_> = std::fs::read_dir(&spill)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(!files.is_empty(), "the cut output is spilled in {spill:?}");
    assert!(
        files
            .iter()
            .any(|name| name.to_string_lossy().starts_with("shell-")),
        "{files:?}"
    );
}

#[tokio::test]
async fn shell_home_tmpdir_and_cargo_home_are_inside_the_task() {
    let fixture = Fixture::new();
    let task = Uuid::new_v4();

    let text = fixture
        .shell(
            task,
            r#"printf '%s\n%s\n%s' "$HOME" "$TMPDIR" "$CARGO_HOME""#,
            None,
        )
        .await
        .unwrap();

    let task_dir = fixture
        .containers
        .ensure(task)
        .await
        .unwrap()
        .host_dir
        .clone();
    let lines: Vec<&str> = text.lines().take(3).collect();
    assert_eq!(lines[0], task_dir.join(".home").to_string_lossy(), "{text}");
    assert_eq!(lines[1], task_dir.join(".tmp").to_string_lossy(), "{text}");
    assert_eq!(
        lines[2],
        task_dir.join(".cargo").to_string_lossy(),
        "{text}"
    );
}

#[tokio::test]
async fn shell_sets_a_usable_locale() {
    let fixture = Fixture::new();
    let default_locale = if cfg!(target_os = "macos") {
        "en_US.UTF-8"
    } else {
        "C.UTF-8"
    };
    let parent_locale = |name: &str| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| default_locale.to_string())
    };
    let expected_lang = parent_locale("LANG");
    let expected_lc_all = parent_locale("LC_ALL");

    let text = fixture
        .shell(
            Uuid::new_v4(),
            r#"printf '%s\n%s' "${LANG:-}" "${LC_ALL:-}""#,
            None,
        )
        .await
        .unwrap();

    let lines: Vec<&str> = text.lines().take(2).collect();
    assert_eq!(lines[0], expected_lang, "{text}");
    assert_eq!(lines[1], expected_lc_all, "{text}");
    for locale in &lines {
        assert!(!locale.trim().is_empty(), "a locale is set: {text}");
    }
    assert!(
        lines[0].to_ascii_lowercase().contains("utf") || lines[0] == "C" || lines[0] == "POSIX",
        "the locale is usable on this platform: {text}"
    );
}
