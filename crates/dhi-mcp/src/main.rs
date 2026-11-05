//! `dhi` — the DHI command-line tools.
//!
//! For now this binary exposes the self-contained `dhi mcp` configuration
//! commands. It never connects to an MCP server and never touches the server
//! process.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail};
use clap::{Args, Parser, Subcommand};
use dhi_mcp::{ConfigShape, McpFile, McpServerEntry, config_path};

#[derive(Debug, Parser)]
#[command(
    name = "dhi",
    version,
    about = "DHI command-line tools",
    disable_help_subcommand = true
)]
struct Cli {
    /// MCP config file (default: DHI_MCP_CONFIG, else ops/mcp.json upward)
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Manage MCP server configuration
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
}

#[derive(Debug, Subcommand)]
enum McpCommand {
    /// List configured MCP servers
    List {
        /// Print a JSON array instead of one line per server
        #[arg(long)]
        json: bool,
    },
    /// Print one server as standard JSON
    Show {
        /// Server name
        name: String,
    },
    /// Add an MCP server (creates the config file when missing)
    Add(AddArgs),
    /// Remove an MCP server
    Remove {
        /// Server name
        name: String,
    },
    /// Enable an MCP server
    Enable {
        /// Server name
        name: String,
    },
    /// Disable an MCP server
    Disable {
        /// Server name
        name: String,
    },
    /// Convert the legacy `servers` array to `mcpServers`
    Migrate,
}

#[derive(Debug, Args)]
struct AddArgs {
    /// Server name (the key in mcpServers)
    name: String,

    /// Command and arguments for a stdio server, after `--`
    #[arg(last = true, value_name = "COMMAND", num_args = 1..)]
    trailing: Vec<String>,

    /// URL for an HTTP server
    #[arg(long, value_name = "URL")]
    url: Option<String>,

    /// Command for a stdio server
    #[arg(long, value_name = "COMMAND")]
    command: Option<String>,

    /// Argument for a stdio server (repeatable)
    #[arg(long = "arg", value_name = "ARG", allow_hyphen_values = true)]
    arg: Vec<String>,

    /// HTTP header KEY=VALUE (repeatable)
    #[arg(long = "header", value_name = "KEY=VALUE")]
    header: Vec<String>,

    /// Environment variable KEY=VALUE for a stdio server (repeatable)
    #[arg(long = "env", value_name = "KEY=VALUE")]
    env: Vec<String>,

    /// Per-call timeout in seconds
    #[arg(long, value_name = "SECS")]
    timeout: Option<u64>,

    /// HTTP only: allow plain HTTP and skip TLS verification
    #[arg(long)]
    insecure: bool,

    /// Create the server disabled
    #[arg(long)]
    disabled: bool,

    /// Replace an existing server with the same name
    #[arg(long)]
    force: bool,
}

#[derive(serde::Serialize)]
struct ListRow {
    name: String,
    transport: &'static str,
    enabled: bool,
    target: String,
    timeout_secs: u64,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let path = resolve_config(cli.config)?;
    match cli.command {
        Command::Mcp { command } => run_mcp(&path, command),
    }
}

fn resolve_config(explicit: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    config_path()
        .ok_or_else(|| anyhow!("no MCP config found; pass --config PATH or set DHI_MCP_CONFIG"))
}

fn run_mcp(path: &Path, command: McpCommand) -> anyhow::Result<()> {
    match command {
        McpCommand::List { json } => list(path, json),
        McpCommand::Show { name } => show(path, &name),
        McpCommand::Add(args) => add(path, args),
        McpCommand::Remove { name } => remove(path, &name),
        McpCommand::Enable { name } => set_enabled(path, &name, true),
        McpCommand::Disable { name } => set_enabled(path, &name, false),
        McpCommand::Migrate => migrate(path),
    }
}

fn load_existing(path: &Path) -> anyhow::Result<McpFile> {
    McpFile::load(path)?.ok_or_else(|| anyhow!("no MCP config at {}", path.display()))
}

/// Loads a file that is about to be rewritten. The legacy shape is refused:
/// rewriting it here would silently migrate a file the operator did not ask
/// to migrate.
fn load_editable(path: &Path) -> anyhow::Result<Option<McpFile>> {
    match McpFile::shape(path)? {
        None => Ok(None),
        Some(ConfigShape::Legacy) => bail!(
            "{} uses the legacy \"servers\" shape; run `dhi mcp migrate` first",
            path.display()
        ),
        Some(ConfigShape::Standard) => {
            let file = McpFile::load(path)?
                .ok_or_else(|| anyhow!("{} disappeared while it was being read", path.display()))?;
            Ok(Some(file))
        }
    }
}

fn load_required_editable(path: &Path) -> anyhow::Result<McpFile> {
    load_editable(path)?.ok_or_else(|| anyhow!("no MCP config at {}", path.display()))
}

fn unknown_server(file: &McpFile, name: &str) -> anyhow::Error {
    if file.servers.is_empty() {
        return anyhow!("no MCP server named {name:?}; the config has no servers");
    }
    let available = file
        .servers
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    anyhow!("no MCP server named {name:?}; available: {available}")
}

fn list(path: &Path, json: bool) -> anyhow::Result<()> {
    let file = match McpFile::load(path)? {
        Some(file) => file,
        None => {
            if json {
                println!("[]");
            } else {
                eprintln!("no MCP config at {}; no servers configured", path.display());
            }
            return Ok(());
        }
    };

    let rows = file
        .servers
        .iter()
        .map(|(name, entry)| {
            let server = entry.validate(name)?;
            Ok(ListRow {
                name: name.clone(),
                transport: server.transport.as_str(),
                enabled: entry.enabled,
                target: server.target(),
                timeout_secs: server.call_timeout.as_secs(),
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no MCP servers configured in {}", path.display());
        return Ok(());
    }
    for row in rows {
        let state = if row.enabled { "enabled" } else { "disabled" };
        println!(
            "{}\t{}\t{}\t{}\t{}s",
            row.name, row.transport, state, row.target, row.timeout_secs
        );
    }
    Ok(())
}

fn show(path: &Path, name: &str) -> anyhow::Result<()> {
    let file = load_existing(path)?;
    let entry = file
        .servers
        .get(name)
        .ok_or_else(|| unknown_server(&file, name))?;
    entry.validate(name)?;
    let mut one = McpFile::default();
    one.servers.insert(name.to_string(), entry.clone());
    println!("{}", serde_json::to_string_pretty(&one)?);
    Ok(())
}

fn add(path: &Path, args: AddArgs) -> anyhow::Result<()> {
    let name = args.name.trim().to_string();
    if name.is_empty() {
        bail!("MCP server name must not be empty");
    }

    let mut file = load_editable(path)?.unwrap_or_default();
    if file.servers.contains_key(&name) && !args.force {
        bail!(
            "MCP server {name:?} already exists in {}; pass --force to replace it",
            path.display()
        );
    }

    let entry = build_entry(&args, &name)?;
    entry.validate(&name)?;
    file.servers.insert(name.clone(), entry);
    file.save(path)?;
    println!("added MCP server {name:?} to {}", path.display());
    Ok(())
}

fn build_entry(args: &AddArgs, name: &str) -> anyhow::Result<McpServerEntry> {
    let trailing = !args.trailing.is_empty();
    let explicit_command = args.command.is_some();
    let has_url = args.url.is_some();
    let modes = [trailing, explicit_command, has_url]
        .into_iter()
        .filter(|selected| *selected)
        .count();
    if modes == 0 {
        bail!(
            "MCP server {name:?} needs a target: use --url URL, --command COMMAND, \
             or `-- COMMAND [ARGS...]`"
        );
    }
    if modes > 1 {
        bail!(
            "MCP server {name:?}: choose exactly one of --url, --command, or \
             `-- COMMAND [ARGS...]`"
        );
    }

    if has_url {
        if !args.arg.is_empty() {
            bail!("--arg is only valid with a stdio server");
        }
        if !args.env.is_empty() {
            bail!("--env is only valid with a stdio server");
        }
        let headers = parse_pairs(&args.header, "header")?;
        Ok(McpServerEntry {
            url: args.url.clone(),
            headers,
            timeout_secs: args.timeout,
            insecure: args.insecure,
            enabled: !args.disabled,
            ..McpServerEntry::default()
        })
    } else {
        if !args.header.is_empty() {
            bail!("--header is only valid with --url (an HTTP server)");
        }
        if args.insecure {
            bail!("--insecure is only valid with --url (an HTTP server)");
        }
        let (command, command_args) = if trailing {
            if !args.arg.is_empty() {
                bail!(
                    "MCP server {name:?}: --arg is only valid with --command; \
                     trailing arguments already follow `--`"
                );
            }
            let mut iter = args.trailing.iter();
            let Some(command) = iter.next() else {
                unreachable!("trailing is non-empty");
            };
            (command.clone(), iter.cloned().collect::<Vec<_>>())
        } else {
            let Some(command) = args.command.clone() else {
                unreachable!("one stdio mode is selected");
            };
            (command, args.arg.clone())
        };
        let env = parse_pairs(&args.env, "environment variable")?;
        Ok(McpServerEntry {
            command: Some(command),
            args: command_args,
            env,
            timeout_secs: args.timeout,
            enabled: !args.disabled,
            ..McpServerEntry::default()
        })
    }
}

fn parse_pairs(values: &[String], label: &str) -> anyhow::Result<BTreeMap<String, String>> {
    let mut pairs = BTreeMap::new();
    for raw in values {
        let Some((key, value)) = raw.split_once('=') else {
            bail!("{label} must be KEY=VALUE");
        };
        if key.is_empty() {
            bail!("{label} name must not be empty");
        }
        if pairs.insert(key.to_string(), value.to_string()).is_some() {
            bail!("duplicate {label} {key:?}");
        }
    }
    Ok(pairs)
}

fn remove(path: &Path, name: &str) -> anyhow::Result<()> {
    let mut file = load_required_editable(path)?;
    if file.servers.remove(name).is_none() {
        return Err(unknown_server(&file, name));
    }
    file.save(path)?;
    println!("removed MCP server {name:?} from {}", path.display());
    Ok(())
}

fn set_enabled(path: &Path, name: &str, enabled: bool) -> anyhow::Result<()> {
    let mut file = load_required_editable(path)?;
    let Some(entry) = file.servers.get_mut(name) else {
        return Err(unknown_server(&file, name));
    };
    if enabled {
        entry.validate(name)?;
    }
    entry.enabled = enabled;
    file.save(path)?;
    let state = if enabled { "enabled" } else { "disabled" };
    println!("{state} MCP server {name:?} in {}", path.display());
    Ok(())
}

fn migrate(path: &Path) -> anyhow::Result<()> {
    match McpFile::shape(path)? {
        None => bail!("no MCP config at {}", path.display()),
        Some(ConfigShape::Standard) => {
            println!(
                "{} already uses the standard \"mcpServers\" shape; nothing to do",
                path.display()
            );
            Ok(())
        }
        Some(ConfigShape::Legacy) => {
            let file = McpFile::migrate_from_legacy(path)?;
            file.validate().map_err(|error| {
                anyhow!(
                    "legacy config at {} is not valid after migration: {error}",
                    path.display()
                )
            })?;
            let backup = backup_path(path);
            McpFile::backup_to(path, &backup)?;
            file.save(path)?;
            println!(
                "migrated {} to \"mcpServers\" (backup at {})",
                path.display(),
                backup.display()
            );
            Ok(())
        }
    }
}

fn backup_path(path: &Path) -> PathBuf {
    let mut backup = path.as_os_str().to_os_string();
    backup.push(".bak");
    PathBuf::from(backup)
}
