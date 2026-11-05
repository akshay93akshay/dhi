//! Live reload helpers: the pure planner boundary, config loading, and the
//! report the manager and API share.
//!
//! The actual connection swap lives on [`McpManager`] in `manager.rs`; this
//! module keeps the comparison rules and the externally visible report small
//! and testable without a live server.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context;
use dhi_mcp::{McpTransport, McpTransportData};
use dhi_mcp_upgrade::reload::{ReloadPlan, ServerSpec, plan_reload};
use serde::Serialize;

use super::config::ValidatedServer;
use super::transport::ValidTransport;

/// One server that could not connect during a reload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReloadFailure {
    pub name: String,
    pub error: String,
}

/// What one live reload did. Server names are as configured; failures keep
/// the manager's clear status error and never leave a guarded name behind.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReloadReport {
    /// Servers present in both configs with the same transport and
    /// fingerprint. Their connections and guarded names were left alone.
    pub kept: Vec<String>,
    /// Newly configured servers that connected.
    pub connected: Vec<String>,
    /// Servers removed from the config (their connections and guarded names
    /// were retired).
    pub disconnected: Vec<String>,
    /// Servers whose transport or fingerprint changed and connected again.
    pub reconnected: Vec<String>,
    /// Servers that were configured but failed to connect. They stay
    /// disconnected and own no guarded names.
    pub failed: Vec<ReloadFailure>,
    /// Whether a `ToolServerHandle` and sink were attached. When false, the
    /// manager can still apply the config but cannot connect new servers.
    pub runtime_attached: bool,
    /// The number of enabled servers in the new config.
    pub configured: usize,
}

impl ReloadReport {
    /// The failure for one server, when its connect failed.
    pub fn failure_for(&self, name: &str) -> Option<&ReloadFailure> {
        self.failed.iter().find(|failure| failure.name == name)
    }

    pub fn has_failures(&self) -> bool {
        !self.failed.is_empty()
    }
}

/// A sanitized view of one configured server for API responses. HTTP header
/// values are masked by [`mask_headers`]; stdio environments are never
/// included, only the command line in `target`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServerSummary {
    pub name: String,
    pub transport: &'static str,
    pub enabled: bool,
    pub target: String,
    pub timeout_secs: u64,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
}

/// Loads the enabled, connectable servers from `path` through the same
/// validation the boot path uses. Unknown keys are tolerated and preserved by
/// the editor; a broken entry is a hard error so a reload never silently
/// drops a server.
pub(crate) fn load_managed_servers(path: &Path) -> anyhow::Result<Vec<ValidatedServer>> {
    let Some(file) = dhi_mcp::edit::load_lenient(path)
        .with_context(|| format!("could not load MCP config {}", path.display()))?
    else {
        return Ok(Vec::new());
    };
    let validated = file
        .validate()
        .with_context(|| format!("MCP config {} is invalid", path.display()))?;
    let mut servers = Vec::with_capacity(validated.len());
    for server in validated {
        let name = server.name.clone();
        let converted = ValidatedServer::try_from(server)
            .with_context(|| format!("MCP server {name:?} is not usable"))?;
        servers.push(converted);
    }
    Ok(servers)
}

/// Builds the reload planner's identity for one already-validated server.
/// The fingerprint covers every connection field and is stable across map
/// ordering; header values are hashed, never stored.
pub(crate) fn spec_of(server: &ValidatedServer) -> anyhow::Result<ServerSpec> {
    let (transport, transport_data) = match &server.transport {
        ValidTransport::Http {
            url,
            headers,
            insecure,
        } => (
            McpTransport::Http,
            McpTransportData::Http {
                url: url.clone(),
                headers: headers.clone(),
                insecure: *insecure,
            },
        ),
        ValidTransport::Sse {
            url,
            headers,
            insecure,
        } => (
            McpTransport::Sse,
            McpTransportData::Sse {
                url: url.clone(),
                headers: headers.clone(),
                insecure: *insecure,
            },
        ),
        ValidTransport::Stdio { command, args, env } => (
            McpTransport::Stdio,
            McpTransportData::Stdio {
                command: command.clone(),
                args: args.clone(),
                env: env
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
            },
        ),
    };
    let validated = dhi_mcp::ValidatedServer {
        name: server.name.clone(),
        segment: server.segment.clone(),
        transport,
        call_timeout: server.call_timeout,
        transport_data,
        visible_to: server.visible_to.clone(),
        tool_visibility: server.tool_visibility.clone(),
        deferred_threshold: server.deferred_threshold,
    };
    ServerSpec::from_validated(&validated)
}

/// Plans a reload between two server sets.
pub(crate) fn plan_between(
    old: &[ValidatedServer],
    new: &[ValidatedServer],
) -> anyhow::Result<ReloadPlan> {
    let old: Vec<ServerSpec> = old.iter().map(spec_of).collect::<anyhow::Result<_>>()?;
    let new: Vec<ServerSpec> = new.iter().map(spec_of).collect::<anyhow::Result<_>>()?;
    Ok(plan_reload(&old, &new)?)
}

/// A sanitized summary for API list/edit responses. Public because the
/// HTTP API crate builds its responses from it.
pub fn summarize(server: &ValidatedServer) -> ServerSummary {
    let (transport, target, headers) = match &server.transport {
        ValidTransport::Http { url, headers, .. } => ("http", url.clone(), mask_headers(headers)),
        ValidTransport::Sse { url, headers, .. } => ("sse", url.clone(), mask_headers(headers)),
        ValidTransport::Stdio { command, args, .. } => {
            let mut target = command.clone();
            for arg in args {
                target.push(' ');
                target.push_str(arg);
            }
            ("stdio", target, BTreeMap::new())
        }
    };
    ServerSummary {
        name: server.name.clone(),
        transport,
        enabled: true,
        target,
        timeout_secs: server.call_timeout.as_secs(),
        headers,
    }
}

/// Masks secret header values, preserving non-secret headers unchanged.
pub(crate) fn mask_headers(headers: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    dhi_mcp::edit::mask_headers(headers)
}

/// The fingerprint helper is re-exported for tests that build specs directly
/// from config entries. The manager itself fingerprints validated servers so
/// old and new sides always use the same canonical shape.
#[cfg(test)]
fn entry_fingerprint(entry: &dhi_mcp::McpServerEntry) -> anyhow::Result<String> {
    dhi_mcp_upgrade::reload::config_fingerprint(entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dhi_mcp::McpServerEntry;
    use dhi_mcp_upgrade::reload::ReloadAction;

    fn entry(command: &str, args: &[&str], enabled: bool) -> McpServerEntry {
        McpServerEntry {
            command: Some(command.to_string()),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            enabled,
            ..McpServerEntry::default()
        }
    }

    fn spec(name: &str, entry: &McpServerEntry) -> ServerSpec {
        let transport = if entry.url.is_some() {
            McpTransport::Http
        } else {
            McpTransport::Stdio
        };
        ServerSpec::from_entry(name, transport, entry).expect("spec")
    }

    fn plan(old: &[(&str, McpServerEntry)], new: &[(&str, McpServerEntry)]) -> ReloadPlan {
        let old: Vec<ServerSpec> = old.iter().map(|(name, entry)| spec(name, entry)).collect();
        let new: Vec<ServerSpec> = new.iter().map(|(name, entry)| spec(name, entry)).collect();
        plan_reload(&old, &new).expect("plan")
    }

    #[test]
    fn unchanged_servers_are_kept() {
        let entry = entry("npx", &["server"], true);
        let plan = plan(
            &[("a", entry.clone()), ("b", entry.clone())],
            &[("a", entry.clone()), ("b", entry)],
        );
        assert_eq!(
            plan.actions,
            [
                ReloadAction::Keep {
                    name: "a".to_string()
                },
                ReloadAction::Keep {
                    name: "b".to_string()
                },
            ]
        );
    }

    #[test]
    fn added_servers_connect() {
        let plan = plan(
            &[("a", entry("npx", &["a"], true))],
            &[
                ("a", entry("npx", &["a"], true)),
                ("b", entry("npx", &["b"], true)),
            ],
        );
        assert_eq!(plan.actions.len(), 2);
        assert!(matches!(
            &plan.actions[1],
            ReloadAction::Connect { spec } if spec.name == "b"
        ));
    }

    #[test]
    fn removed_servers_disconnect_before_anything_else() {
        let plan = plan(
            &[
                ("a", entry("npx", &["a"], true)),
                ("b", entry("npx", &["b"], true)),
            ],
            &[("b", entry("npx", &["b"], true))],
        );
        assert!(matches!(
            &plan.actions[0],
            ReloadAction::Disconnect { name } if name == "a"
        ));
        assert!(matches!(
            &plan.actions[1],
            ReloadAction::Keep { name } if name == "b"
        ));
    }

    #[test]
    fn changed_config_reconnects() {
        let plan = plan(
            &[("a", entry("npx", &["old"], true))],
            &[("a", entry("npx", &["new"], true))],
        );
        assert!(matches!(
            &plan.actions[0],
            ReloadAction::Reconnect { old, new } if old.name == "a" && new.name == "a"
        ));
    }

    #[test]
    fn transport_change_reconnects() {
        let old = McpServerEntry {
            command: Some("npx".to_string()),
            ..McpServerEntry::default()
        };
        let new = McpServerEntry {
            url: Some("https://example.test/mcp".to_string()),
            ..McpServerEntry::default()
        };
        let plan = plan(&[("a", old)], &[("a", new)]);
        assert!(matches!(
            &plan.actions[0],
            ReloadAction::Reconnect { old, new }
                if old.transport == McpTransport::Stdio && new.transport == McpTransport::Http
        ));
    }

    #[test]
    fn reordering_keeps_every_server_in_the_new_order() {
        let plan = plan(
            &[
                ("a", entry("npx", &["a"], true)),
                ("b", entry("npx", &["b"], true)),
                ("c", entry("npx", &["c"], true)),
            ],
            &[
                ("c", entry("npx", &["c"], true)),
                ("a", entry("npx", &["a"], true)),
                ("b", entry("npx", &["b"], true)),
            ],
        );
        let names: Vec<&str> = plan.actions.iter().map(ReloadAction::name).collect();
        assert_eq!(names, ["c", "a", "b"]);
        assert!(
            plan.actions
                .iter()
                .all(|action| matches!(action, ReloadAction::Keep { .. }))
        );
    }

    #[test]
    fn duplicate_and_empty_names_are_rejected() {
        let entry = entry("npx", &["a"], true);
        let duplicate = vec![spec("a", &entry), spec("a", &entry)];
        let error = plan_reload(&duplicate, &[]).expect_err("duplicate");
        assert!(error.to_string().contains("duplicate"), "{error}");

        let empty = vec![ServerSpec::new("", McpTransport::Stdio, "f")];
        let error = plan_reload(&[], &empty).expect_err("empty");
        assert!(error.to_string().contains("empty"), "{error}");
    }

    #[test]
    fn disabled_entries_never_reach_the_planner() {
        let disabled = entry("npx", &["a"], false);
        assert_eq!(
            entry_fingerprint(&disabled).expect("fingerprint"),
            entry_fingerprint(&entry("npx", &["a"], true)).expect("fingerprint"),
            "enabled is a lifecycle switch, not connection config"
        );
        let error = ServerSpec::from_entry("a", McpTransport::Stdio, &disabled)
            .expect_err("disabled entries are filtered before planning");
        assert!(error.to_string().contains("disabled"), "{error}");
    }

    #[test]
    fn fingerprint_changes_with_connection_fields() {
        let first = entry("npx", &["a"], true);
        let mut second = first.clone();
        second.args = vec!["b".to_string()];
        assert_ne!(
            entry_fingerprint(&first).expect("fingerprint"),
            entry_fingerprint(&second).expect("fingerprint")
        );
        second.args = first.args.clone();
        second.timeout_secs = Some(30);
        assert_ne!(
            entry_fingerprint(&first).expect("fingerprint"),
            entry_fingerprint(&second).expect("fingerprint")
        );
    }

    #[test]
    fn summaries_mask_secret_headers() {
        let server = ValidatedServer {
            name: "remote".to_string(),
            segment: "remote".to_string(),
            transport: ValidTransport::Http {
                url: "https://example.test/mcp".to_string(),
                headers: BTreeMap::from([
                    ("Authorization".to_string(), "Bearer secret".to_string()),
                    ("X-Tenant".to_string(), "acme".to_string()),
                ]),
                insecure: false,
            },
            call_timeout: std::time::Duration::from_secs(17),
            visible_to: None,
            tool_visibility: BTreeMap::new(),
            deferred_threshold: None,
        };
        let summary = summarize(&server);
        assert_eq!(summary.transport, "http");
        assert_eq!(summary.headers["Authorization"], "***");
        assert_eq!(summary.headers["X-Tenant"], "acme");
        assert!(!format!("{summary:?}").contains("Bearer secret"));
        assert_eq!(summary.timeout_secs, 17);
    }

    #[test]
    fn loads_unknown_keys_leniently() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{
              "$schema": "dhi.mcp.v1",
              "mcpServers": {
                "ok": { "command": "npx", "x-note": "keep" },
                "off": { "command": "node", "enabled": false }
              }
            }"#,
        )
        .expect("write");
        let file = dhi_mcp::edit::load_lenient(&path)
            .expect("loads")
            .expect("exists");
        assert_eq!(file.servers.len(), 2);
        assert_eq!(file.servers["ok"].command.as_deref(), Some("npx"));

        let servers = load_managed_servers(&path).expect("managed servers");
        assert_eq!(servers.len(), 1, "disabled entries are filtered");
        assert_eq!(servers[0].name, "ok");
    }
}
