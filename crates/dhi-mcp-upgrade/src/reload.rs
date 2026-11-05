//! A pure plan for reloading MCP servers without restarting the process.
//!
//! The planner compares validated [`ServerSpec`] values by name, transport,
//! and a config fingerprint. It emits only the four actions the manager will
//! need: keep, connect, disconnect, and reconnect.

use std::collections::BTreeMap;

use anyhow::bail;
use dhi_mcp::{McpServerEntry, McpTransport, McpTransportData, ValidatedServer};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// The identity of one connectable server for reload purposes.
///
/// `fingerprint` should cover every config field that affects the connection
/// (command, args, env, url, headers, timeout, insecure, enabled). Use
/// [`config_fingerprint`] or [`ServerSpec::from_entry`] to build it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSpec {
    pub name: String,
    pub transport: McpTransport,
    pub fingerprint: String,
}

impl ServerSpec {
    /// A spec with a caller-supplied fingerprint.
    pub fn new(
        name: impl Into<String>,
        transport: McpTransport,
        fingerprint: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            transport,
            fingerprint: fingerprint.into(),
        }
    }

    /// A spec whose fingerprint is a SHA-256 of the canonical serialized
    /// entry. Header/environment values are hashed, never stored in the
    /// fingerprint. Disabled entries are rejected: `McpFile::validate()`
    /// excludes them, so they must never reach the planner.
    pub fn from_entry(
        name: impl Into<String>,
        transport: McpTransport,
        entry: &McpServerEntry,
    ) -> anyhow::Result<Self> {
        let name = name.into();
        if !entry.enabled {
            bail!(
                "disabled MCP server {name:?} cannot be planned; call McpFile::validate() \
                 and use only its enabled ValidatedServer values"
            );
        }
        Ok(Self {
            name,
            transport,
            fingerprint: config_fingerprint(entry)?,
        })
    }

    /// Builds a spec from the enabled, validated server shape. This is the
    /// preferred constructor: `McpFile::validate()` has already dropped
    /// disabled entries and normalized transport/config fields.
    pub fn from_validated(server: &ValidatedServer) -> anyhow::Result<Self> {
        Ok(Self {
            name: server.name.clone(),
            transport: server.transport,
            fingerprint: config_fingerprint_validated(server)?,
        })
    }
}

/// Hashes the serialized config entry with `enabled` normalized to true.
///
/// `enabled` is a lifecycle switch, not connection configuration. Disabled
/// entries are filtered before planning, so they never appear here; normalizing
/// it also keeps fingerprints stable if a caller has an otherwise-identical
/// entry whose enabled flag was toggled.
pub fn config_fingerprint(entry: &McpServerEntry) -> anyhow::Result<String> {
    let mut canonical = entry.clone();
    canonical.enabled = true;
    hash_json(&serde_json::to_value(canonical)?)
}

/// Fingerprints a validated, enabled server. This excludes `enabled` by
/// construction because [`ValidatedServer`] has no such field.
pub fn config_fingerprint_validated(server: &ValidatedServer) -> anyhow::Result<String> {
    let transport_data = match &server.transport_data {
        McpTransportData::Stdio { command, args, env } => json!({
            "kind": "stdio",
            "command": command,
            "args": args,
            "env": env,
        }),
        McpTransportData::Http {
            url,
            headers,
            insecure,
        } => json!({
            "kind": "http",
            "url": url,
            "headers": headers,
            "insecure": insecure,
        }),
        McpTransportData::Sse {
            url,
            headers,
            insecure,
        } => json!({
            "kind": "sse",
            "url": url,
            "headers": headers,
            "insecure": insecure,
        }),
    };
    hash_json(&json!({
        "call_timeout_secs": server.call_timeout.as_secs(),
        "transport_data": transport_data,
    }))
}

fn hash_json(value: &Value) -> anyhow::Result<String> {
    let bytes = serde_json::to_vec(value)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

/// One step the manager should take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReloadAction {
    /// The name, transport, and fingerprint are unchanged.
    Keep { name: String },
    /// The name is new.
    Connect { spec: ServerSpec },
    /// The name is gone.
    Disconnect { name: String },
    /// The name exists in both configs but its transport or fingerprint
    /// changed.
    Reconnect { old: ServerSpec, new: ServerSpec },
}

impl ReloadAction {
    /// The server name this action applies to.
    pub fn name(&self) -> &str {
        match self {
            Self::Keep { name } | Self::Disconnect { name } => name,
            Self::Connect { spec } | Self::Reconnect { new: spec, .. } => &spec.name,
        }
    }
}

/// The ordered set of actions for one reload.
///
/// Removals come first so their names and resources are released before new
/// connections are made. Keeps, reconnects, and connects then follow the new
/// config order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReloadPlan {
    pub actions: Vec<ReloadAction>,
}

impl ReloadPlan {
    /// Diffs two configs. Duplicate or empty names are rejected.
    pub fn between(old: &[ServerSpec], new: &[ServerSpec]) -> Result<Self, ReloadError> {
        let old_by_name = index(old, "old")?;
        let new_by_name = index(new, "new")?;

        let mut actions = Vec::new();
        for spec in old {
            if !new_by_name.contains_key(&spec.name) {
                actions.push(ReloadAction::Disconnect {
                    name: spec.name.clone(),
                });
            }
        }
        for spec in new {
            match old_by_name.get(&spec.name) {
                None => actions.push(ReloadAction::Connect { spec: spec.clone() }),
                Some(previous)
                    if previous.transport == spec.transport
                        && previous.fingerprint == spec.fingerprint =>
                {
                    actions.push(ReloadAction::Keep {
                        name: spec.name.clone(),
                    });
                }
                Some(previous) => actions.push(ReloadAction::Reconnect {
                    old: (*previous).clone(),
                    new: spec.clone(),
                }),
            }
        }
        Ok(Self { actions })
    }

    /// True when there is nothing to do.
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    /// The number of actions.
    pub fn len(&self) -> usize {
        self.actions.len()
    }
}

/// Diffs two configs without constructing a [`ReloadPlan`] value first.
pub fn plan_reload(old: &[ServerSpec], new: &[ServerSpec]) -> Result<ReloadPlan, ReloadError> {
    ReloadPlan::between(old, new)
}

fn index<'a>(
    specs: &'a [ServerSpec],
    side: &'static str,
) -> Result<BTreeMap<String, &'a ServerSpec>, ReloadError> {
    let mut by_name = BTreeMap::new();
    for spec in specs {
        if spec.name.trim().is_empty() {
            return Err(ReloadError::EmptyName { side });
        }
        if spec.fingerprint.trim().is_empty() {
            return Err(ReloadError::EmptyFingerprint {
                side,
                name: spec.name.clone(),
            });
        }
        if by_name.insert(spec.name.clone(), spec).is_some() {
            return Err(ReloadError::DuplicateName {
                side,
                name: spec.name.clone(),
            });
        }
    }
    Ok(by_name)
}

/// Why a reload plan could not be built.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ReloadError {
    #[error("duplicate MCP server name {name:?} in the {side} config")]
    DuplicateName { side: &'static str, name: String },
    #[error("empty MCP server name in the {side} config")]
    EmptyName { side: &'static str },
    #[error("MCP server {name:?} in the {side} config has an empty fingerprint")]
    EmptyFingerprint { side: &'static str, name: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, transport: McpTransport, fingerprint: &str) -> ServerSpec {
        ServerSpec::new(name, transport, fingerprint)
    }

    fn stdio(name: &str, fingerprint: &str) -> ServerSpec {
        spec(name, McpTransport::Stdio, fingerprint)
    }

    #[test]
    fn unchanged_servers_are_kept() {
        let old = vec![stdio("a", "1"), stdio("b", "2")];
        let plan = plan_reload(&old, &old).expect("plan");
        assert_eq!(
            plan.actions,
            [
                ReloadAction::Keep { name: "a".into() },
                ReloadAction::Keep { name: "b".into() },
            ]
        );
        assert_eq!(plan.len(), 2);
        assert!(!plan.is_empty());
    }

    #[test]
    fn added_servers_connect() {
        let old = vec![stdio("a", "1")];
        let new = vec![stdio("a", "1"), stdio("b", "2")];
        let plan = plan_reload(&old, &new).expect("plan");
        assert_eq!(
            plan.actions,
            [
                ReloadAction::Keep { name: "a".into() },
                ReloadAction::Connect {
                    spec: stdio("b", "2")
                },
            ]
        );
    }

    #[test]
    fn removed_servers_disconnect_first() {
        let old = vec![stdio("a", "1"), stdio("b", "2")];
        let new = vec![stdio("b", "2")];
        let plan = plan_reload(&old, &new).expect("plan");
        assert_eq!(
            plan.actions,
            [
                ReloadAction::Disconnect { name: "a".into() },
                ReloadAction::Keep { name: "b".into() },
            ]
        );
    }

    #[test]
    fn changed_fingerprint_reconnects() {
        let old = vec![stdio("a", "old")];
        let new = vec![stdio("a", "new")];
        let plan = plan_reload(&old, &new).expect("plan");
        assert_eq!(
            plan.actions,
            [ReloadAction::Reconnect {
                old: stdio("a", "old"),
                new: stdio("a", "new"),
            }]
        );
    }

    #[test]
    fn changed_transport_reconnects_even_with_the_same_fingerprint() {
        let old = vec![spec("a", McpTransport::Stdio, "same")];
        let new = vec![spec("a", McpTransport::Http, "same")];
        let plan = plan_reload(&old, &new).expect("plan");
        assert!(matches!(
            plan.actions.as_slice(),
            [ReloadAction::Reconnect { .. }]
        ));
    }

    #[test]
    fn reordering_keeps_every_server_in_the_new_order() {
        let old = vec![stdio("a", "1"), stdio("b", "2"), stdio("c", "3")];
        let new = vec![stdio("c", "3"), stdio("a", "1"), stdio("b", "2")];
        let plan = plan_reload(&old, &new).expect("plan");
        let names: Vec<&str> = plan.actions.iter().map(ReloadAction::name).collect();
        assert_eq!(names, ["c", "a", "b"]);
        assert!(
            plan.actions
                .iter()
                .all(|action| matches!(action, ReloadAction::Keep { .. }))
        );
    }

    #[test]
    fn empty_configs_produce_no_actions() {
        let plan = plan_reload(&[], &[]).expect("plan");
        assert!(plan.is_empty());
        assert_eq!(plan.len(), 0);
    }

    #[test]
    fn duplicate_and_empty_names_are_rejected() {
        let duplicate = vec![stdio("a", "1"), stdio("a", "2")];
        assert_eq!(
            plan_reload(&duplicate, &[]).expect_err("duplicate"),
            ReloadError::DuplicateName {
                side: "old",
                name: "a".into()
            }
        );
        assert_eq!(
            plan_reload(&[], &[stdio(" ", "1")]).expect_err("empty"),
            ReloadError::EmptyName { side: "new" }
        );
    }

    #[test]
    fn fingerprint_changes_with_config_and_is_order_stable() {
        let mut first = McpServerEntry {
            command: Some("npx".to_string()),
            args: vec!["server".to_string()],
            env: BTreeMap::new(),
            headers: BTreeMap::new(),
            ..McpServerEntry::default()
        };
        first.env.insert("Z".to_string(), "last".to_string());
        first.env.insert("A".to_string(), "first".to_string());
        first.headers.insert("X-Zed".to_string(), "z".to_string());
        first.headers.insert("X-Alpha".to_string(), "a".to_string());

        let mut second = first.clone();
        second.env.clear();
        second.env.insert("A".to_string(), "first".to_string());
        second.env.insert("Z".to_string(), "last".to_string());
        second.headers.clear();
        second
            .headers
            .insert("X-Alpha".to_string(), "a".to_string());
        second.headers.insert("X-Zed".to_string(), "z".to_string());

        assert_eq!(
            config_fingerprint(&first).expect("fingerprint"),
            config_fingerprint(&second).expect("fingerprint"),
            "BTreeMap ordering must not change the fingerprint"
        );

        first.timeout_secs = Some(30);
        assert_ne!(
            config_fingerprint(&first).expect("fingerprint"),
            config_fingerprint(&second).expect("fingerprint")
        );

        let from_entry = ServerSpec::from_entry("a", McpTransport::Stdio, &second).expect("spec");
        assert_eq!(
            from_entry.fingerprint,
            config_fingerprint(&second).expect("fingerprint")
        );
    }

    #[test]
    fn validated_servers_have_a_fingerprint_without_enabled() {
        let server = ValidatedServer {
            name: "a".to_string(),
            segment: "a".to_string(),
            transport: McpTransport::Stdio,
            call_timeout: std::time::Duration::from_secs(9),
            transport_data: McpTransportData::Stdio {
                command: "npx".to_string(),
                args: vec!["server".to_string()],
                env: BTreeMap::new(),
            },
            visible_to: None,
            tool_visibility: BTreeMap::new(),
            deferred_threshold: None,
        };
        let spec = ServerSpec::from_validated(&server).expect("spec");
        assert_eq!(spec.name, "a");
        assert_eq!(spec.transport, McpTransport::Stdio);
        assert!(!spec.fingerprint.is_empty());
        assert_eq!(
            spec.fingerprint,
            config_fingerprint_validated(&server).expect("fingerprint")
        );
    }

    #[test]
    fn disabled_entries_and_empty_fingerprints_are_rejected() {
        let disabled = McpServerEntry {
            command: Some("npx".to_string()),
            enabled: false,
            ..McpServerEntry::default()
        };
        let enabled = McpServerEntry {
            enabled: true,
            ..disabled.clone()
        };
        assert_eq!(
            config_fingerprint(&disabled).expect("disabled fingerprint"),
            config_fingerprint(&enabled).expect("enabled fingerprint"),
            "enabled is a lifecycle switch, not connection config"
        );
        let error = ServerSpec::from_entry("off", McpTransport::Stdio, &disabled)
            .expect_err("disabled entry");
        assert!(error.to_string().contains("disabled"), "{error}");

        assert_eq!(
            plan_reload(&[stdio("a", "")], &[]).expect_err("empty fingerprint"),
            ReloadError::EmptyFingerprint {
                side: "old",
                name: "a".to_string()
            }
        );
    }
}
