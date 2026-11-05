//! Command custom-tool execution.
//!
//! Programs are spawned directly, never through a shell. The child sees only
//! a fixed `PATH` plus the explicitly configured environment map. A timeout
//! kills the process tree on the direct child (grandchildren are outside this
//! block's scope), and stdout/stderr are drained with a hard byte cap.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::config::{CommandExec, reject_control};
use crate::util::object_input;

/// The only ambient value a command inherits. Everything else is cleared.
const FIXED_PATH: &str = "/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin";

pub(crate) async fn execute(tool: &str, exec: &CommandExec, input: &Value) -> Result<Value> {
    let object = object_input(tool, input)?;
    let args = substitute_args(tool, &exec.args, object)?;
    for arg in &args {
        reject_control("command argument", arg).map_err(|err| {
            anyhow!("custom tool {tool:?} after substituting placeholders: {err:#}")
        })?;
    }

    let mut command = Command::new(&exec.command);
    command
        .args(&args)
        .env_clear()
        .env("PATH", FIXED_PATH)
        .envs(&exec.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command.spawn().map_err(|err| {
        anyhow!(
            "custom tool {tool:?} could not start command {:?}: {err}",
            exec.command
        )
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("custom tool {tool:?} command stdout was not captured"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("custom tool {tool:?} command stderr was not captured"))?;
    let mut stdout_task = tokio::spawn(read_capped(stdout, exec.max_output_bytes));
    let mut stderr_task = tokio::spawn(read_capped(stderr, exec.max_output_bytes));
    let deadline = tokio::time::Instant::now() + exec.timeout;

    let status = match tokio::time::timeout_at(deadline, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(err)) => {
            let _ = child.kill().await;
            stdout_task.abort();
            stderr_task.abort();
            return Err(anyhow!(
                "custom tool {tool:?} command {:?} failed: {err}",
                exec.command
            ));
        }
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            stdout_task.abort();
            stderr_task.abort();
            return Err(anyhow!(
                "custom tool {tool:?} command {:?} timed out after {}s",
                exec.command,
                exec.timeout.as_secs()
            ));
        }
    };

    // Reader joins share the same deadline: a grandchild that inherited a pipe
    // must not be able to hold the tool call open after the direct child exits.
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    let collected = tokio::time::timeout(remaining.max(Duration::from_millis(1)), async {
        let (stdout, stdout_truncated) = (&mut stdout_task)
            .await
            .map_err(|err| anyhow!("custom tool {tool:?} stdout reader failed: {err}"))??;
        let (stderr, stderr_truncated) = (&mut stderr_task)
            .await
            .map_err(|err| anyhow!("custom tool {tool:?} stderr reader failed: {err}"))??;
        Ok::<_, anyhow::Error>((stdout, stdout_truncated, stderr, stderr_truncated))
    })
    .await;

    match collected {
        Ok(Ok((stdout, stdout_truncated, stderr, stderr_truncated))) => Ok(result(
            status.code(),
            status.success(),
            stdout,
            stderr,
            stdout_truncated || stderr_truncated,
        )),
        Ok(Err(err)) => {
            stdout_task.abort();
            stderr_task.abort();
            Err(err)
        }
        Err(_) => {
            stdout_task.abort();
            stderr_task.abort();
            Err(anyhow!(
                "custom tool {tool:?} command {:?} timed out after {}s while collecting output",
                exec.command,
                exec.timeout.as_secs()
            ))
        }
    }
}

fn result(
    exit_code: Option<i32>,
    success: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
) -> Value {
    let mut result = Map::new();
    result.insert(
        "exit_code".to_string(),
        exit_code.map(Value::from).unwrap_or(Value::Null),
    );
    result.insert("success".to_string(), Value::Bool(success));
    result.insert(
        "stdout".to_string(),
        Value::String(String::from_utf8_lossy(&stdout).into_owned()),
    );
    result.insert(
        "stderr".to_string(),
        Value::String(String::from_utf8_lossy(&stderr).into_owned()),
    );
    result.insert("truncated".to_string(), Value::Bool(truncated));
    Value::Object(result)
}

/// Reads to EOF while retaining at most `max` bytes. Draining is deliberate:
/// a full pipe would otherwise block the child until its timeout.
async fn read_capped<R>(mut reader: R, max: usize) -> std::io::Result<(Vec<u8>, bool)>
where
    R: AsyncRead + Unpin,
{
    let mut retained = Vec::new();
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = max.saturating_sub(retained.len());
        if read > remaining {
            retained.extend_from_slice(&buffer[..remaining]);
            truncated = true;
        } else {
            retained.extend_from_slice(&buffer[..read]);
        }
    }
    Ok((retained, truncated))
}

/// Replaces `{name}` placeholders in configured arguments with the string
/// value of that input field. A missing or non-string value fails the call
/// rather than silently spawning with a literal placeholder.
fn substitute_args(
    tool: &str,
    templates: &[String],
    input: &Map<String, Value>,
) -> Result<Vec<String>> {
    templates
        .iter()
        .map(|template| substitute_arg(tool, template, input))
        .collect()
}

fn substitute_arg(tool: &str, template: &str, input: &Map<String, Value>) -> Result<String> {
    let mut result = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        result.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            // An unmatched brace is literal text.
            result.push_str(&rest[start..]);
            return Ok(result);
        };
        let name = &after[..end];
        if name.is_empty() || name.contains('{') {
            // Not a placeholder; keep the brace as text.
            result.push('{');
            rest = after;
            continue;
        }
        let Some(value) = input.get(name).and_then(Value::as_str) else {
            bail!("custom tool {tool:?} placeholder {{{name}}} requires a string argument");
        };
        result.push_str(value);
        rest = &after[end + 1..];
    }
    result.push_str(rest);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn input(value: Value) -> Map<String, Value> {
        value.as_object().expect("object").clone()
    }

    #[test]
    fn substitutes_named_placeholders() {
        let args = substitute_args(
            "echo",
            &["--message={message}".to_string(), "{name}".to_string()],
            &input(json!({"message": "hello", "name": "world"})),
        )
        .expect("substitution");
        assert_eq!(args, ["--message=hello", "world"]);
    }

    #[test]
    fn rejects_missing_and_non_string_placeholders() {
        let missing = substitute_args("echo", &["{message}".to_string()], &input(json!({})))
            .expect_err("missing");
        assert!(missing.to_string().contains("{message}"));
        let wrong_type = substitute_args(
            "echo",
            &["{count}".to_string()],
            &input(json!({"count": 3})),
        )
        .expect_err("non-string");
        assert!(wrong_type.to_string().contains("{count}"));
    }

    #[test]
    fn unmatched_braces_stay_literal() {
        let args = substitute_args(
            "echo",
            &["{unterminated".to_string(), "plain".to_string()],
            &input(json!({})),
        )
        .expect("substitution");
        assert_eq!(args, ["{unterminated", "plain"]);
    }

    #[tokio::test]
    async fn reads_at_most_the_cap() {
        let data = [b'x'; 100];
        let (retained, truncated) = read_capped(&data[..], 10).await.expect("read");
        assert_eq!(retained, b"xxxxxxxxxx");
        assert!(truncated);
    }
}
