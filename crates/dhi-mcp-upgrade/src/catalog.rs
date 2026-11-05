//! A small, transport-neutral catalog of MCP servers and their tools.
//!
//! The catalog is what visibility and deferred-schema planning operate on. It
//! carries only the fields those decisions need: names, descriptions, input
//! schemas, and role visibility.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

use crate::naming::guarded_tool_name;

/// The input schema used when a catalog entry does not provide one.
pub fn empty_object_schema() -> Value {
    json!({"type": "object", "properties": {}})
}

fn normalize_schema(schema: Value) -> Value {
    if schema.is_null() {
        empty_object_schema()
    } else {
        schema
    }
}

fn deserialize_input_schema<'de, D>(deserializer: D) -> Result<Value, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(normalize_schema(Value::deserialize(deserializer)?))
}

/// One tool advertised by one server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogTool {
    /// The tool name as the MCP server reports it (not the guarded name).
    pub name: String,
    /// A human-readable description. May be empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// The JSON schema for the tool's arguments.
    #[serde(
        rename = "inputSchema",
        default = "empty_object_schema",
        deserialize_with = "deserialize_input_schema",
        skip_serializing_if = "Value::is_null"
    )]
    pub input_schema: Value,
    /// Per-tool role override. `None` inherits [`ServerCatalog::visible_to`];
    /// `Some([])` means visible to nobody.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_to: Option<Vec<String>>,
}

impl CatalogTool {
    /// A tool with an empty description and the default object schema.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: String::new(),
            input_schema: empty_object_schema(),
            visible_to: None,
        }
    }

    /// Sets the description.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// Sets the input schema. A null schema is normalized to the default
    /// object schema so every listed tool is callable.
    pub fn with_input_schema(mut self, schema: Value) -> Self {
        self.input_schema = normalize_schema(schema);
        self
    }

    /// Overrides the server's visibility for this tool. An empty list means
    /// the tool is visible to nobody.
    pub fn with_visible_to(mut self, roles: Vec<String>) -> Self {
        self.visible_to = Some(roles);
        self
    }

    /// The canonical guarded name for this tool on `server`, or `None` when
    /// the names cannot produce one. Callers should skip unreportable tools
    /// rather than registering an approximation.
    pub fn guarded_name(&self, server: &str) -> Option<String> {
        guarded_tool_name(server, &self.name)
    }

    /// True when this tool can be registered under a guarded name.
    pub fn is_callable(&self, server: &str) -> bool {
        self.guarded_name(server).is_some()
    }

    /// The schema with a null normalized to the default object schema.
    pub fn normalized_input_schema(&self) -> Value {
        normalize_schema(self.input_schema.clone())
    }
}

/// One MCP server and the tools it advertises.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerCatalog {
    /// The configured server name.
    pub name: String,
    /// An optional human-readable server description.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Roles that may see this server. Empty means visible to nobody.
    #[serde(default)]
    pub visible_to: Vec<String>,
    /// The server's tools, in discovery order.
    #[serde(default)]
    pub tools: Vec<CatalogTool>,
}

impl ServerCatalog {
    /// A server with no tools and no description.
    pub fn new(name: impl Into<String>, visible_to: Vec<String>, tools: Vec<CatalogTool>) -> Self {
        Self {
            name: name.into(),
            description: String::new(),
            visible_to,
            tools,
        }
    }

    /// Appends one tool.
    pub fn with_tool(mut self, tool: CatalogTool) -> Self {
        self.tools.push(tool);
        self
    }
}

/// A model-visible tool definition. For direct MCP tools `name` is the
/// guarded name; for the deferred meta-tools it is `mcp_list_tools` or
/// `mcp_call_tool`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_input_schema_is_normalized() {
        let tool: CatalogTool = serde_json::from_value(json!({
            "name": "echo",
            "inputSchema": null
        }))
        .expect("catalog tool");
        assert_eq!(tool.input_schema, empty_object_schema());

        let built = CatalogTool::new("echo").with_input_schema(Value::Null);
        assert_eq!(built.input_schema, empty_object_schema());
    }

    #[test]
    fn guarded_names_are_validated_when_building_the_catalog() {
        assert_eq!(
            CatalogTool::new("get forecast").guarded_name("my server"),
            Some("mcp__my_server__get_forecast".to_string())
        );
        assert!(!CatalogTool::new("@@@").is_callable("server"));
        assert!(!CatalogTool::new("tool").is_callable("@@@"));
    }
}
