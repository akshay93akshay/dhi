//! Guarded MCP tool names.
//!
//! The canonical implementation lives in [`dhi_mcp::guarded_tool_name`] so
//! config validation, registration, deferred dispatch, and audit all agree.
//! This module re-exports it for the upgrade core; do not add a local copy.

pub use dhi_mcp::{
    MAX_SERVER_SEGMENT_CHARS, MAX_TOOL_NAME_CHARS, SEGMENT_SEPARATOR, TOOL_PREFIX,
    guarded_tool_name,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn re_exported_builder_has_the_server_shape() {
        assert_eq!(
            guarded_tool_name("weather", "get_forecast").as_deref(),
            Some("mcp__weather__get_forecast")
        );
        assert_eq!(
            guarded_tool_name("my server", "get forecast").as_deref(),
            Some("mcp__my_server__get_forecast")
        );
    }

    #[test]
    fn caps_and_rejects_unusable_names() {
        let name = guarded_tool_name(&"s".repeat(30), "tool").expect("usable name");
        assert_eq!(name, format!("mcp__{}__tool", "s".repeat(16)));
        let long = guarded_tool_name("server", &"t".repeat(200)).expect("usable name");
        assert_eq!(long.chars().count(), MAX_TOOL_NAME_CHARS);
        assert_eq!(guarded_tool_name("", "tool"), None);
        assert_eq!(guarded_tool_name("server", ""), None);
    }
}
