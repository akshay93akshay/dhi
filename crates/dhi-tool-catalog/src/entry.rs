use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Maximum key length, counted in Unicode scalar values.
pub const MAX_TOOL_KEY_CHARS: usize = 128;

/// Prefix for a named specialist role, for example `specialist:chess`.
pub const SPECIALIST_PREFIX: &str = "specialist:";

/// Which source owns a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSource {
    Builtin,
    Web,
    Coding,
    Pty,
    Mcp,
    Skills,
    Custom,
}

impl ToolSource {
    /// Stable lowercase source name used by persistence and the API.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Web => "web",
            Self::Coding => "coding",
            Self::Pty => "pty",
            Self::Mcp => "mcp",
            Self::Skills => "skills",
            Self::Custom => "custom",
        }
    }
}

/// One tool as the catalog sees it.
///
/// `metadata` is deliberately non-secret: it carries descriptive,
/// source-specific values such as transport names or capability flags.
/// Providers must never put tokens, credentials, authorization headers, or
/// other secret values in it. Snapshots are returned to the API and UI
/// without redaction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCatalogEntry {
    /// Stable unique key, for example `web_search`, `mcp__server__tool`, or
    /// `load_skill`.
    pub key: String,
    pub source: ToolSource,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "inputSchema", default = "empty_schema")]
    pub input_schema: Value,
    /// Roles that see the tool by default. Empty means every recognized role:
    /// `planner`, `worker`, `reporter`, `digest`, and any
    /// `specialist:<name>`. A non-empty list is matched exactly; there is no
    /// role inheritance.
    #[serde(default)]
    pub default_roles: Vec<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub deferred: bool,
    /// Source-specific, non-secret metadata. See the type-level docs.
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
}

fn empty_schema() -> Value {
    serde_json::json!({"type": "object", "properties": {}})
}

fn default_true() -> bool {
    true
}

impl ToolCatalogEntry {
    /// A minimal, enabled, non-deferred entry with the default empty-object
    /// input schema and no role restriction.
    pub fn new(key: impl Into<String>, source: ToolSource) -> Self {
        Self {
            key: key.into(),
            source,
            description: String::new(),
            input_schema: empty_schema(),
            default_roles: Vec::new(),
            enabled: true,
            deferred: false,
            metadata: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    #[must_use]
    pub fn with_input_schema(mut self, input_schema: Value) -> Self {
        self.input_schema = input_schema;
        self
    }

    #[must_use]
    pub fn with_default_roles(mut self, default_roles: Vec<String>) -> Self {
        self.default_roles = default_roles;
        self
    }

    #[must_use]
    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    #[must_use]
    pub fn with_deferred(mut self, deferred: bool) -> Self {
        self.deferred = deferred;
        self
    }

    #[must_use]
    pub fn with_metadata(mut self, metadata: BTreeMap<String, Value>) -> Self {
        self.metadata = metadata;
        self
    }

    /// Validate the stable invariants of a catalog entry.
    ///
    /// Duplicate keys are checked by [`crate::build_catalog`], not here,
    /// because uniqueness depends on the other entries in the catalog.
    pub fn validate(&self) -> Result<(), CatalogValidationError> {
        validate_key(&self.key)?;
        if !self.input_schema.is_object() {
            return Err(CatalogValidationError::InputSchemaNotObject {
                key: self.key.clone(),
            });
        }
        for role in &self.default_roles {
            if !is_recognized_role(role) {
                return Err(CatalogValidationError::UnrecognizedRole {
                    key: self.key.clone(),
                    role: role.clone(),
                });
            }
        }
        Ok(())
    }
}

/// Validate one entry. Equivalent to [`ToolCatalogEntry::validate`].
pub fn validate_entry(entry: &ToolCatalogEntry) -> Result<(), CatalogValidationError> {
    entry.validate()
}

/// Why an entry cannot enter the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogValidationError {
    /// The key is empty or whitespace only.
    EmptyKey,
    /// The key contains a C0, C1, or other Unicode control character.
    KeyHasControlCharacters { key: String },
    /// The key exceeds [`MAX_TOOL_KEY_CHARS`] Unicode scalar values.
    KeyTooLong { key: String, length: usize },
    /// `inputSchema` is not a JSON object.
    InputSchemaNotObject { key: String },
    /// A default role is neither a base role nor `specialist:<name>`.
    UnrecognizedRole { key: String, role: String },
}

impl fmt::Display for CatalogValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyKey => formatter.write_str("empty tool key"),
            Self::KeyHasControlCharacters { key } => {
                write!(formatter, "tool key {key:?} contains control characters")
            }
            Self::KeyTooLong { key, length } => write!(
                formatter,
                "tool key {key:?} is {length} characters; maximum is {MAX_TOOL_KEY_CHARS}"
            ),
            Self::InputSchemaNotObject { key } => {
                write!(formatter, "tool {key:?} input schema must be a JSON object")
            }
            Self::UnrecognizedRole { key, role } => {
                write!(formatter, "tool {key:?} has unrecognized role {role:?}")
            }
        }
    }
}

impl std::error::Error for CatalogValidationError {}

fn validate_key(key: &str) -> Result<(), CatalogValidationError> {
    if key.trim().is_empty() {
        return Err(CatalogValidationError::EmptyKey);
    }
    if key.chars().any(char::is_control) {
        return Err(CatalogValidationError::KeyHasControlCharacters {
            key: key.to_string(),
        });
    }
    let length = key.chars().count();
    if length > MAX_TOOL_KEY_CHARS {
        return Err(CatalogValidationError::KeyTooLong {
            key: key.to_string(),
            length,
        });
    }
    Ok(())
}

/// Whether `role` is a base role or a well-formed `specialist:<name>` role.
///
/// Matching is case-sensitive. A specialist name must be non-empty after
/// trimming and must not contain control characters.
pub fn is_recognized_role(role: &str) -> bool {
    matches!(role, "planner" | "worker" | "reporter" | "digest")
        || role
            .strip_prefix(SPECIALIST_PREFIX)
            .is_some_and(is_valid_specialist_name)
}

/// Build a `specialist:<name>` role, or `None` for an invalid name.
pub fn specialist_role(name: &str) -> Option<String> {
    is_valid_specialist_name(name).then(|| format!("{SPECIALIST_PREFIX}{name}"))
}

fn is_valid_specialist_name(name: &str) -> bool {
    !name.trim().is_empty() && !name.chars().any(char::is_control)
}

/// Stable base role list for a default-visible entry.
///
/// Specialist roles are dynamic and therefore not included. An entry with an
/// empty `default_roles` list is visible to every recognized role, including
/// every specialist.
pub fn all_roles() -> BTreeSet<&'static str> {
    ["planner", "worker", "reporter", "digest"]
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(key: &str) -> ToolCatalogEntry {
        ToolCatalogEntry::new(key, ToolSource::Builtin)
    }

    fn roles(roles: &[&str]) -> Vec<String> {
        roles.iter().map(|role| (*role).to_string()).collect()
    }

    #[test]
    fn default_entry_is_valid_and_has_stable_defaults() {
        let entry = entry("web_search");
        assert!(entry.enabled);
        assert!(!entry.deferred);
        assert!(entry.default_roles.is_empty());
        assert_eq!(entry.input_schema["type"], "object");
        assert!(entry.metadata.is_empty());
        entry.validate().expect("valid entry");
    }

    #[test]
    fn keys_must_be_non_empty_without_control_characters() {
        assert_eq!(entry("").validate(), Err(CatalogValidationError::EmptyKey));
        assert_eq!(
            entry("   ").validate(),
            Err(CatalogValidationError::EmptyKey)
        );
        assert!(matches!(
            entry("bad\nkey").validate(),
            Err(CatalogValidationError::KeyHasControlCharacters { .. })
        ));
        assert!(matches!(
            entry("bad\u{7f}key").validate(),
            Err(CatalogValidationError::KeyHasControlCharacters { .. })
        ));
    }

    #[test]
    fn keys_are_limited_to_128_unicode_scalar_values() {
        let accepted = "é".repeat(MAX_TOOL_KEY_CHARS);
        entry(&accepted)
            .validate()
            .expect("128 characters are allowed");

        let rejected = "é".repeat(MAX_TOOL_KEY_CHARS + 1);
        assert_eq!(
            entry(&rejected).validate(),
            Err(CatalogValidationError::KeyTooLong {
                key: rejected,
                length: MAX_TOOL_KEY_CHARS + 1,
            })
        );
    }

    #[test]
    fn input_schema_must_be_a_json_object() {
        for schema in [json!(["not", "an", "object"]), json!("text"), Value::Null] {
            assert_eq!(
                entry("tool").with_input_schema(schema).validate(),
                Err(CatalogValidationError::InputSchemaNotObject {
                    key: "tool".to_string()
                })
            );
        }

        entry("tool")
            .with_input_schema(json!({"type": "object"}))
            .validate()
            .expect("object schema");
    }

    #[test]
    fn base_and_specialist_roles_are_recognized() {
        for role in all_roles() {
            assert!(is_recognized_role(role), "{role} should be recognized");
        }
        assert!(is_recognized_role("specialist:chess"));
        assert!(is_recognized_role("specialist:reviewer-2"));

        entry("tool")
            .with_default_roles(roles(&["planner", "worker", "specialist:chess"]))
            .validate()
            .expect("recognized roles");
    }

    #[test]
    fn unknown_malformed_and_miscased_roles_are_rejected() {
        for role in [
            "",
            "Planner",
            "specialist:",
            "specialist:   ",
            "specialist:bad\nname",
            "intern",
        ] {
            assert!(
                !is_recognized_role(role),
                "{role:?} should not be recognized"
            );
            assert_eq!(
                entry("tool")
                    .with_default_roles(vec![role.to_string()])
                    .validate(),
                Err(CatalogValidationError::UnrecognizedRole {
                    key: "tool".to_string(),
                    role: role.to_string(),
                })
            );
        }
    }

    #[test]
    fn specialist_role_helper_validates_its_input() {
        assert_eq!(
            specialist_role("chess"),
            Some("specialist:chess".to_string())
        );
        assert_eq!(specialist_role(""), None);
        assert_eq!(specialist_role("   "), None);
        assert_eq!(specialist_role("bad\nname"), None);
    }

    #[test]
    fn entry_serializes_stable_catalog_keys() {
        let value = serde_json::to_value(entry("web_search")).expect("json");
        assert_eq!(value["key"], "web_search");
        assert_eq!(value["source"], "builtin");
        assert_eq!(value["enabled"], true);
        assert_eq!(value["inputSchema"]["type"], "object");
        assert_eq!(value["deferred"], false);
    }

    #[test]
    fn deserialization_applies_safe_defaults() {
        let entry: ToolCatalogEntry =
            serde_json::from_value(json!({"key": "mcp__github__search", "source": "mcp"}))
                .expect("entry");
        assert!(entry.enabled);
        assert!(!entry.deferred);
        assert!(entry.default_roles.is_empty());
        assert_eq!(entry.input_schema["type"], "object");
        assert!(entry.metadata.is_empty());
    }

    #[test]
    fn all_roles_is_the_stable_base_set() {
        assert_eq!(
            all_roles().into_iter().collect::<Vec<_>>(),
            ["digest", "planner", "reporter", "worker"]
        );
    }
}
