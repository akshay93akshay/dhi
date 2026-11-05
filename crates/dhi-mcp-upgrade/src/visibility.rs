//! Per-agent MCP visibility.
//!
//! DHI's real roles are `planner`, `worker`, `reporter`, and `digest`.
//! Specialists are worker runs tagged `specialist:<name>`. A server declares
//! the roles that may see it; a tool inherits that list unless it carries its
//! own override. `visible_to: []` means visible to nobody — it is never
//! treated as "everyone". An unrecognized role sees no tools at all, so a typo
//! in an agent role fails closed.
//!
//! This module only filters. Enforcement requires the server to keep a
//! registry per role (or otherwise register `VisibleTool::guarded_name` only
//! for the role that owns the agent); filtering alone does not remove a tool
//! from a shared registry.

use std::fmt;
use std::str::FromStr;

use thiserror::Error;

use crate::catalog::ServerCatalog;
use crate::naming::guarded_tool_name;

/// A role that can own an agent. `specialist:<name>` is a worker run tagged
/// with the specialist's name; the name is kept verbatim after the prefix.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Role {
    Planner,
    Worker,
    Reporter,
    Digest,
    Specialist(String),
}

/// The error returned when a role string is not one of the known shapes.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error(
    "unknown MCP role {0:?}; expected \"planner\", \"worker\", \"reporter\", \"digest\", \
     or \"specialist:<name>\""
)]
pub struct RoleParseError(pub String);

impl Role {
    /// Parses a configured role. Surrounding whitespace is ignored; unknown
    /// roles return `None` (and therefore see no tools).
    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        match raw {
            "planner" => Some(Self::Planner),
            "worker" => Some(Self::Worker),
            "reporter" => Some(Self::Reporter),
            "digest" => Some(Self::Digest),
            _ => raw
                .strip_prefix("specialist:")
                .map(str::trim)
                .filter(|name| !name.is_empty() && !name.chars().any(char::is_control))
                .map(|name| Self::Specialist(name.to_string())),
        }
    }

    /// Builds a specialist role, returning `None` for an empty or invalid
    /// name.
    pub fn specialist(name: impl AsRef<str>) -> Option<Self> {
        let name = name.as_ref().trim();
        if name.is_empty() || name.chars().any(char::is_control) {
            return None;
        }
        Some(Self::Specialist(name.to_string()))
    }

    /// True when this role is explicitly listed in a configured visibility
    /// list. Entries are parsed with [`Role::parse`]; malformed entries never
    /// match anything.
    pub fn is_allowed_by(&self, visible_to: &[String]) -> bool {
        visible_to
            .iter()
            .any(|entry| Role::parse(entry).as_ref() == Some(self))
    }
}

impl fmt::Display for Role {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Planner => formatter.write_str("planner"),
            Self::Worker => formatter.write_str("worker"),
            Self::Reporter => formatter.write_str("reporter"),
            Self::Digest => formatter.write_str("digest"),
            Self::Specialist(name) => write!(formatter, "specialist:{name}"),
        }
    }
}

impl FromStr for Role {
    type Err = RoleParseError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::parse(raw).ok_or_else(|| RoleParseError(raw.to_string()))
    }
}

/// One callable tool visible to a role, flattened with its server name and the
/// canonical guarded name the server must register.
#[derive(Debug, Clone, PartialEq)]
pub struct VisibleTool {
    pub server: String,
    pub tool: String,
    /// `mcp__<server>__<tool>`, the name the guard/audit must see.
    pub guarded_name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// A tool that matched visibility but cannot be registered because its name
/// cannot produce a guarded name. It is reported, never silently registered
/// under an approximate name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedTool {
    pub server: String,
    pub tool: String,
    pub reason: &'static str,
}

/// Visible tools plus the entries skipped while building them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VisibilityReport {
    pub visible: Vec<VisibleTool>,
    pub skipped: Vec<SkippedTool>,
}

/// Returns every callable tool visible to `role` in catalog order.
///
/// An unknown role yields no tools. Without an override, an empty server
/// visibility list yields no tools; an empty per-tool override also yields
/// none. A non-empty override replaces the server list for that one tool.
/// A single unguardable tool is skipped (and reported by
/// [`visibility_report`]) rather than failing the whole role.
pub fn visible_tools(role: &str, servers: &[ServerCatalog]) -> Vec<VisibleTool> {
    visibility_report(role, servers).visible
}

/// The typed form of [`visible_tools`].
pub fn visible_tools_for_role(role: &Role, servers: &[ServerCatalog]) -> Vec<VisibleTool> {
    visibility_report_for_role(role, servers).visible
}

/// Builds the visible set and reports tools skipped because they have no
/// usable guarded name.
pub fn visibility_report(role: &str, servers: &[ServerCatalog]) -> VisibilityReport {
    let Some(role) = Role::parse(role) else {
        return VisibilityReport::default();
    };
    visibility_report_for_role(&role, servers)
}

/// The typed form of [`visibility_report`].
pub fn visibility_report_for_role(role: &Role, servers: &[ServerCatalog]) -> VisibilityReport {
    let mut report = VisibilityReport::default();
    for server in servers {
        for tool in &server.tools {
            // An override replaces the server list for this tool, so a single
            // tool can be shared with a role that cannot see the rest of the
            // server. Without an override the tool inherits the server list.
            let roles = tool
                .visible_to
                .as_deref()
                .unwrap_or(server.visible_to.as_slice());
            if !role.is_allowed_by(roles) {
                continue;
            }
            let Some(guarded_name) = guarded_tool_name(&server.name, &tool.name) else {
                report.skipped.push(SkippedTool {
                    server: server.name.clone(),
                    tool: tool.name.clone(),
                    reason: "tool or server name has no usable characters for a guarded name",
                });
                continue;
            };
            report.visible.push(VisibleTool {
                server: server.name.clone(),
                tool: tool.name.clone(),
                guarded_name,
                description: tool.description.clone(),
                input_schema: tool.normalized_input_schema(),
            });
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::CatalogTool;
    use serde_json::json;

    fn tool(name: &str) -> CatalogTool {
        CatalogTool::new(name)
    }

    fn server(name: &str, visible_to: &[&str], tools: Vec<CatalogTool>) -> ServerCatalog {
        ServerCatalog::new(
            name,
            visible_to.iter().map(|role| (*role).to_string()).collect(),
            tools,
        )
    }

    fn names(role: &str, servers: &[ServerCatalog]) -> Vec<(String, String)> {
        visible_tools(role, servers)
            .into_iter()
            .map(|tool| (tool.server, tool.tool))
            .collect()
    }

    #[test]
    fn real_roles_see_only_their_own_servers() {
        let servers = vec![
            server("planner-tools", &["planner"], vec![tool("plan")]),
            server("worker-tools", &["worker"], vec![tool("work")]),
            server("reporter-tools", &["reporter"], vec![tool("report")]),
            server("digest-tools", &["digest"], vec![tool("summarize")]),
        ];

        for (role, expected_server, expected_tool) in [
            ("planner", "planner-tools", "plan"),
            ("worker", "worker-tools", "work"),
            ("reporter", "reporter-tools", "report"),
            ("digest", "digest-tools", "summarize"),
        ] {
            assert_eq!(
                names(role, &servers),
                [(expected_server.to_string(), expected_tool.to_string())],
                "role {role}"
            );
            for other in ["planner", "worker", "reporter", "digest"] {
                if other != role {
                    assert!(
                        names(other, &servers)
                            .iter()
                            .all(|(server, _)| server != expected_server),
                        "{other} must not see {expected_server}"
                    );
                }
            }
        }
    }

    #[test]
    fn specialist_is_a_worker_run_tag() {
        let servers = vec![server(
            "research-tools",
            &["specialist:researcher"],
            vec![tool("search")],
        )];
        assert_eq!(
            names("specialist:researcher", &servers),
            [("research-tools".into(), "search".into())]
        );
        assert!(names("worker", &servers).is_empty());
        assert!(names("specialist:legal", &servers).is_empty());
    }

    #[test]
    fn guarded_name_is_part_of_the_visible_bridge() {
        let servers = vec![server("my server", &["worker"], vec![tool("get forecast")])];
        let visible = visible_tools("worker", &servers);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].guarded_name, "mcp__my_server__get_forecast");
        assert_eq!(
            visible[0].input_schema,
            json!({"type": "object", "properties": {}})
        );
    }

    #[test]
    fn both_roles_see_a_shared_server() {
        let servers = vec![server(
            "shared",
            &["worker", "reporter"],
            vec![tool("ping")],
        )];
        assert_eq!(
            names("worker", &servers),
            [("shared".into(), "ping".into())]
        );
        assert_eq!(
            names("reporter", &servers),
            [("shared".into(), "ping".into())]
        );
    }

    #[test]
    fn empty_visibility_is_visible_to_nobody() {
        let servers = vec![server("private", &[], vec![tool("secret")])];
        for role in [
            "planner",
            "worker",
            "reporter",
            "digest",
            "specialist:finance",
        ] {
            assert!(names(role, &servers).is_empty(), "{role}");
        }
    }

    #[test]
    fn per_tool_override_replaces_server_visibility() {
        let servers = vec![server(
            "mixed",
            &["worker"],
            vec![
                tool("worker-tool"),
                tool("planner-tool").with_visible_to(vec!["planner".into()]),
                tool("nobody-tool").with_visible_to(Vec::new()),
            ],
        )];

        assert_eq!(
            names("worker", &servers),
            [("mixed".into(), "worker-tool".into())]
        );
        assert_eq!(
            names("planner", &servers),
            [("mixed".into(), "planner-tool".into())]
        );
    }

    #[test]
    fn a_tool_can_override_into_a_role_the_server_does_not_allow() {
        let servers = vec![server(
            "worker-server",
            &["worker"],
            vec![tool("planner-gift").with_visible_to(vec!["planner".into()])],
        )];
        assert!(names("worker", &servers).is_empty());
        assert_eq!(
            names("planner", &servers),
            [("worker-server".into(), "planner-gift".into())]
        );
    }

    #[test]
    fn unknown_role_sees_no_tools() {
        let servers = vec![server(
            "shared",
            &["planner", "worker", "reporter", "digest"],
            vec![tool("ping")],
        )];
        assert!(names("intern", &servers).is_empty());
        assert!(names("", &servers).is_empty());
        assert!(names("specialist:", &servers).is_empty());
    }

    #[test]
    fn one_bad_tool_is_skipped_not_fatal() {
        let servers = vec![server(
            "fake",
            &["worker"],
            vec![tool("good"), tool("@@@"), tool("also-good")],
        )];
        let report = visibility_report("worker", &servers);
        assert_eq!(
            report
                .visible
                .iter()
                .map(|tool| tool.tool.as_str())
                .collect::<Vec<_>>(),
            ["good", "also-good"]
        );
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].tool, "@@@");
        assert!(report.skipped[0].reason.contains("guarded name"));
    }
}
