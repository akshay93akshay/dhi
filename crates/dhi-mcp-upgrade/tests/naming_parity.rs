//! Parity with the naming rule currently implemented privately in
//! `dhi-server/src/mcp.rs`.
//!
//! `dhi-server` cannot be edited in this worktree. This reference copy mirrors
//! its algorithm (sanitize -> cap server -> sanitize again) so the canonical
//! `dhi-mcp` function is checked against the server's behavior. Integration
//! must replace the server's private copy with `dhi_mcp::guarded_tool_name`.

use dhi_mcp::{
    MAX_SERVER_SEGMENT_CHARS, MAX_TOOL_NAME_CHARS, SEGMENT_SEPARATOR, TOOL_PREFIX,
    guarded_tool_name, sanitize_segment,
};

fn server_reference_guarded_name(server: &str, tool: &str) -> Option<String> {
    let segment: String = sanitize_segment(server)
        .chars()
        .take(MAX_SERVER_SEGMENT_CHARS)
        .collect();
    let server_segment = sanitize_segment(&segment);
    if server_segment.is_empty() {
        return None;
    }
    let tool_segment = sanitize_segment(tool);
    if tool_segment.is_empty() {
        return None;
    }
    let base = format!("{TOOL_PREFIX}{server_segment}{SEGMENT_SEPARATOR}");
    let remaining = MAX_TOOL_NAME_CHARS.saturating_sub(base.chars().count());
    if remaining == 0 {
        return None;
    }
    Some(format!(
        "{base}{}",
        tool_segment.chars().take(remaining).collect::<String>()
    ))
}

#[test]
fn long_server_names_match_the_server_reference() {
    let names = [
        "a".repeat(16),
        "a".repeat(17),
        "a".repeat(30),
        format!("{}a", "a".repeat(15)),
        format!("{}{}", "a".repeat(15), "-"),
        format!("{}{}", "a".repeat(15), "_"),
        format!("{}{}", "a".repeat(15), " "),
        format!("{}-b", "a".repeat(15)),
        format!("{}_b", "a".repeat(15)),
        format!("{} b", "a".repeat(15)),
        format!("{}-{}", "a".repeat(15), "b".repeat(14)),
        format!("{}_{}", "a".repeat(15), "b".repeat(14)),
        format!("{} {}", "a".repeat(15), "b".repeat(14)),
    ];

    for name in names {
        let canonical = guarded_tool_name(&name, "tool");
        let reference = server_reference_guarded_name(&name, "tool");
        assert_eq!(canonical, reference, "server name {name:?}");
    }
}

#[test]
fn separator_at_the_cap_does_not_leave_a_trailing_dash_or_underscore() {
    for name in [
        format!("{}-b", "a".repeat(15)),
        format!("{}_b", "a".repeat(15)),
        format!("{} b", "a".repeat(15)),
    ] {
        assert_eq!(
            guarded_tool_name(&name, "tool").as_deref(),
            Some("mcp__aaaaaaaaaaaaaaa__tool"),
            "server name {name:?}"
        );
    }
}
