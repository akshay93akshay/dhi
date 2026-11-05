//! End-to-end tests for the `dhi mcp` CLI. Every test uses a temporary
//! config path or `DHI_MCP_CONFIG`; the repository's `ops/mcp.json` is never
//! touched.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use dhi_mcp::{ConfigShape, McpFile, McpTransport};
use tempfile::TempDir;

fn run(path: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dhi"))
        .arg("--config")
        .arg(path)
        .args(args)
        .output()
        .expect("run dhi")
}

fn run_with_env(path: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dhi"))
        .env("DHI_MCP_CONFIG", path)
        .args(args)
        .output()
        .expect("run dhi")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn assert_ok(output: &Output) {
    assert!(
        output.status.success(),
        "dhi failed: stdout={:?} stderr={:?}",
        stdout(output),
        stderr(output)
    );
}

fn assert_failed(output: &Output) {
    assert!(
        !output.status.success(),
        "dhi unexpectedly succeeded: stdout={:?} stderr={:?}",
        stdout(output),
        stderr(output)
    );
}

fn temp_config() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("mcp.json");
    (dir, path)
}

fn load(path: &Path) -> McpFile {
    McpFile::load(path)
        .expect("config parses")
        .expect("config exists")
}

#[test]
fn add_stdio_simple_path_and_list_show() {
    let (_dir, path) = temp_config();
    let output = run(
        &path,
        &["mcp", "add", "chess", "--", "uvx", "chess-mcp-server"],
    );
    assert_ok(&output);
    assert!(stdout(&output).contains("chess"));

    let file = load(&path);
    let entry = &file.servers["chess"];
    assert_eq!(entry.command.as_deref(), Some("uvx"));
    assert_eq!(entry.args, ["chess-mcp-server"]);
    assert!(entry.enabled);
    assert!(entry.transport.is_none());

    let output = run(&path, &["mcp", "list"]);
    assert_ok(&output);
    let listed = stdout(&output);
    assert!(listed.contains("chess"), "{listed}");
    assert!(listed.contains("stdio"), "{listed}");
    assert!(listed.contains("enabled"), "{listed}");
    assert!(listed.contains("uvx chess-mcp-server"), "{listed}");
    assert!(listed.contains("60s"), "{listed}");

    let output = run(&path, &["mcp", "show", "chess"]);
    assert_ok(&output);
    let shown: McpFile = serde_json::from_str(&stdout(&output)).expect("show is standard JSON");
    assert_eq!(shown, file);
}

#[test]
fn add_stdio_explicit_command_with_args_and_env() {
    let (_dir, path) = temp_config();
    let output = run(
        &path,
        &[
            "mcp",
            "add",
            "files",
            "--command",
            "npx",
            "--arg",
            "-y",
            "--arg",
            "@modelcontextprotocol/server-filesystem",
            "--env",
            "NODE_ENV=production",
            "--timeout",
            "30",
        ],
    );
    assert_ok(&output);

    let file = load(&path);
    let entry = &file.servers["files"];
    assert_eq!(entry.command.as_deref(), Some("npx"));
    assert_eq!(
        entry.args,
        ["-y", "@modelcontextprotocol/server-filesystem"]
    );
    assert_eq!(entry.env["NODE_ENV"], "production");
    assert_eq!(entry.timeout_secs, Some(30));
}

#[test]
fn add_http_with_headers_and_flags() {
    let (_dir, path) = temp_config();
    let output = run(
        &path,
        &[
            "mcp",
            "add",
            "remote",
            "--url",
            "https://example.com/mcp",
            "--header",
            "Authorization=Bearer token",
            "--header",
            "X-Test=1",
            "--timeout",
            "60",
            "--insecure",
            "--disabled",
        ],
    );
    assert_ok(&output);

    let file = load(&path);
    let entry = &file.servers["remote"];
    assert_eq!(entry.url.as_deref(), Some("https://example.com/mcp"));
    assert_eq!(entry.headers["Authorization"], "Bearer token");
    assert_eq!(entry.headers["X-Test"], "1");
    assert_eq!(entry.timeout_secs, Some(60));
    assert!(entry.insecure);
    assert!(!entry.enabled);
    assert!(entry.command.is_none());

    let output = run(&path, &["mcp", "list"]);
    assert_ok(&output);
    let listed = stdout(&output);
    assert!(listed.contains("remote"), "{listed}");
    assert!(listed.contains("http"), "{listed}");
    assert!(listed.contains("disabled"), "{listed}");
    assert!(listed.contains("https://example.com/mcp"), "{listed}");
    assert!(listed.contains("60s"), "{listed}");

    let output = run(&path, &["mcp", "list", "--json"]);
    assert_ok(&output);
    let rows: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("JSON list");
    let rows = rows.as_array().expect("array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], "remote");
    assert_eq!(rows[0]["transport"], "http");
    assert_eq!(rows[0]["enabled"], false);
    assert_eq!(rows[0]["target"], "https://example.com/mcp");
    assert_eq!(rows[0]["timeout_secs"], 60);
}

#[test]
fn duplicate_add_requires_force() {
    let (_dir, path) = temp_config();
    assert_ok(&run(
        &path,
        &["mcp", "add", "chess", "--", "uvx", "chess-mcp-server"],
    ));

    let output = run(&path, &["mcp", "add", "chess", "--", "other", "server"]);
    assert_failed(&output);
    assert!(
        stderr(&output).contains("already exists"),
        "{}",
        stderr(&output)
    );
    assert_eq!(
        load(&path).servers["chess"].command.as_deref(),
        Some("uvx"),
        "a refused add leaves the existing entry alone"
    );

    let output = run(
        &path,
        &["mcp", "add", "chess", "--force", "--", "other", "server"],
    );
    assert_ok(&output);
    assert_eq!(
        load(&path).servers["chess"].command.as_deref(),
        Some("other")
    );
    assert_eq!(load(&path).servers["chess"].args, ["server"]);
}

#[test]
fn disable_enable_remove_round_trip() {
    let (_dir, path) = temp_config();
    assert_ok(&run(
        &path,
        &["mcp", "add", "chess", "--", "uvx", "chess-mcp-server"],
    ));

    assert_ok(&run(&path, &["mcp", "disable", "chess"]));
    assert!(!load(&path).servers["chess"].enabled);
    let listed = stdout(&run(&path, &["mcp", "list"]));
    assert!(listed.contains("disabled"), "{listed}");

    assert_ok(&run(&path, &["mcp", "enable", "chess"]));
    assert!(load(&path).servers["chess"].enabled);

    assert_ok(&run(&path, &["mcp", "remove", "chess"]));
    assert!(load(&path).servers.is_empty());

    let output = run(&path, &["mcp", "remove", "chess"]);
    assert_failed(&output);
    assert!(
        stderr(&output).contains("no MCP server named"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn migrate_legacy_writes_standard_shape_and_backup() {
    let (_dir, path) = temp_config();
    let legacy = r#"{
      "servers": [
        {
          "name": "example",
          "transport": "stdio",
          "command": "npx",
          "args": ["-y", "server"],
          "env": { "NODE_ENV": "production" },
          "enabled": false,
          "timeout_secs": 60
        },
        {
          "name": "remote",
          "transport": "http",
          "url": "https://example.com/mcp",
          "timeout_secs": 30
        }
      ]
    }"#;
    std::fs::write(&path, legacy).expect("write legacy");

    let output = run(&path, &["mcp", "migrate"]);
    assert_ok(&output);
    assert!(stdout(&output).contains("migrated"), "{}", stdout(&output));

    assert_eq!(
        McpFile::shape(&path).expect("shape"),
        Some(ConfigShape::Standard)
    );
    let backup = PathBuf::from(format!("{}.bak", path.display()));
    assert_eq!(
        std::fs::read_to_string(&backup).expect("backup"),
        legacy,
        "the .bak copy is the original legacy file"
    );

    let file = load(&path);
    let example = &file.servers["example"];
    assert_eq!(example.command.as_deref(), Some("npx"));
    assert_eq!(example.args, ["-y", "server"]);
    assert_eq!(example.env["NODE_ENV"], "production");
    assert!(!example.enabled);
    assert_eq!(example.timeout_secs, Some(60));
    let remote = &file.servers["remote"];
    assert_eq!(remote.url.as_deref(), Some("https://example.com/mcp"));
    assert_eq!(remote.timeout_secs, Some(30));

    // A second migrate is a no-op and keeps the backup intact.
    let output = run(&path, &["mcp", "migrate"]);
    assert_ok(&output);
    assert!(
        stdout(&output).contains("already uses"),
        "{}",
        stdout(&output)
    );
    assert_eq!(std::fs::read_to_string(&backup).expect("backup"), legacy);
}

#[test]
fn migrate_refuses_unknown_shape_without_writing() {
    let (_dir, path) = temp_config();
    let unknown = r#"{"unrelated": true}"#;
    std::fs::write(&path, unknown).expect("write unknown");

    let output = run(&path, &["mcp", "migrate"]);
    assert_failed(&output);
    assert!(
        stderr(&output).contains("not a recognized MCP config"),
        "{}",
        stderr(&output)
    );
    assert_eq!(std::fs::read_to_string(&path).expect("read"), unknown);
    assert!(
        !PathBuf::from(format!("{}.bak", path.display())).exists(),
        "an unknown shape is not backed up or rewritten"
    );
}

#[test]
fn add_refuses_legacy_until_migrated() {
    let (_dir, path) = temp_config();
    std::fs::write(
        &path,
        r#"{"servers":[{"name":"old","transport":"stdio","command":"npx"}]}"#,
    )
    .expect("write legacy");

    let output = run(&path, &["mcp", "add", "new", "--", "uvx", "server"]);
    assert_failed(&output);
    assert!(stderr(&output).contains("migrate"), "{}", stderr(&output));
    assert_eq!(
        McpFile::shape(&path).expect("shape"),
        Some(ConfigShape::Legacy)
    );
}

#[test]
fn config_flag_works_before_and_after_the_subcommand() {
    let (_dir, path) = temp_config();
    assert_ok(&run(
        &path,
        &["mcp", "add", "chess", "--", "uvx", "chess-mcp-server"],
    ));

    let output = Command::new(env!("CARGO_BIN_EXE_dhi"))
        .args(["mcp", "list", "--config"])
        .arg(&path)
        .output()
        .expect("run dhi");
    assert_ok(&output);
    assert!(stdout(&output).contains("chess"), "{}", stdout(&output));
}

#[test]
fn dhi_mcp_config_env_is_honored() {
    let (_dir, path) = temp_config();
    let output = run_with_env(&path, &["mcp", "add", "env-server", "--", "npx", "server"]);
    assert_ok(&output);
    assert!(path.is_file(), "the env path is created");
    assert_eq!(
        load(&path).servers["env-server"].command.as_deref(),
        Some("npx")
    );

    let output = run_with_env(&path, &["mcp", "list"]);
    assert_ok(&output);
    assert!(
        stdout(&output).contains("env-server"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn add_creates_missing_config_and_parent_directories() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("ops").join("nested").join("mcp.json");
    assert!(!path.exists());

    let output = run(
        &path,
        &["mcp", "add", "chess", "--", "uvx", "chess-mcp-server"],
    );
    assert_ok(&output);
    assert!(path.is_file());
    assert_eq!(load(&path).servers["chess"].command.as_deref(), Some("uvx"));
}

#[test]
fn errors_do_not_echo_header_or_env_values() {
    let (_dir, path) = temp_config();
    let output = run(
        &path,
        &[
            "mcp",
            "add",
            "bad",
            "--url",
            "https://example.com/mcp",
            "--header",
            "Authorization=Bearer super-secret\r\nInjected: yes",
        ],
    );
    assert_failed(&output);
    let message = stderr(&output);
    assert!(!message.contains("super-secret"), "{message}");
    assert!(!message.contains("Injected"), "{message}");
    assert!(message.contains("Authorization"), "{message}");
    assert!(!path.exists(), "a failed add does not create a config");

    assert_ok(&run(
        &path,
        &[
            "mcp",
            "add",
            "remote",
            "--url",
            "https://example.com/mcp",
            "--header",
            "Authorization=Bearer super-secret",
        ],
    ));
    let output = run(
        &path,
        &["mcp", "add", "remote", "--url", "https://example.com/other"],
    );
    assert_failed(&output);
    assert!(
        !stderr(&output).contains("super-secret"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn missing_file_list_is_empty_and_show_errors() {
    let (_dir, path) = temp_config();
    let output = run(&path, &["mcp", "list"]);
    assert_ok(&output);
    assert!(stdout(&output).is_empty());
    assert!(
        stderr(&output).contains("no MCP config"),
        "{}",
        stderr(&output)
    );

    let output = run(&path, &["mcp", "show", "chess"]);
    assert_failed(&output);
    assert!(
        stderr(&output).contains("no MCP config"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn show_reports_unknown_names_without_inventing_one() {
    let (_dir, path) = temp_config();
    assert_ok(&run(
        &path,
        &["mcp", "add", "chess", "--", "uvx", "chess-mcp-server"],
    ));
    let output = run(&path, &["mcp", "show", "missing"]);
    assert_failed(&output);
    let message = stderr(&output);
    assert!(message.contains("no MCP server named"), "{message}");
    assert!(message.contains("chess"), "{message}");
}

#[test]
fn explicit_sse_is_readable_through_the_cli() {
    let (_dir, path) = temp_config();
    std::fs::write(
        &path,
        r#"{
          "mcpServers": {
            "events": {
              "transport": "sse",
              "url": "https://example.com/sse"
            }
          }
        }"#,
    )
    .expect("write config");

    let output = run(&path, &["mcp", "list"]);
    assert_ok(&output);
    assert!(stdout(&output).contains("sse"), "{}", stdout(&output));

    let file = load(&path);
    let servers = file.validate().expect("SSE validates");
    assert_eq!(servers[0].transport, McpTransport::Sse);
}
