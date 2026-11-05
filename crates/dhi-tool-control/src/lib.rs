//! Control actions for the tool catalog.
//!
//! The catalog is the read model. This crate owns the write model: enable or
//! disable a tool, set role visibility, and record skill usage. The write
//! model persists through `0008_tool_control.sql`; the types here are the
//! frozen interface.

use std::collections::BTreeSet;

use async_trait::async_trait;
use dhi_tool_catalog::ToolCatalogEntry;
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

/// One requested change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ToolControlAction {
    SetEnabled { key: String, enabled: bool },
    SetVisibility { key: String, roles: Vec<String> },
}

/// Current control state for one tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolState {
    pub key: String,
    pub enabled: bool,
    pub roles: Vec<String>,
}

/// One skill load, for the usage ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillUsageRecord {
    pub skill: String,
    pub role: String,
    pub task_id: Option<String>,
    pub intent_id: Option<String>,
    pub found: bool,
}

/// Control failures that callers can map to API status codes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolControlError {
    #[error("unknown tool {0:?}")]
    UnknownTool(String),
    #[error("invalid control action: {0}")]
    Invalid(String),
    #[error("control store failed: {0}")]
    Store(String),
}

/// The write side of the catalog. Implementations must make each action
/// durable before returning success.
#[async_trait]
pub trait ToolControl: Send + Sync {
    /// Applies one action and returns the full control state after it commits.
    async fn apply(&self, action: ToolControlAction) -> Result<Vec<ToolState>, ToolControlError>;
    async fn states(&self) -> Result<Vec<ToolState>, ToolControlError>;
    async fn record_skill_usage(&self, record: &SkillUsageRecord) -> Result<(), ToolControlError>;
}

/// PostgreSQL-backed control store.
///
/// The catalog is seeded elsewhere. This type only changes catalog state, so
/// an action for a key that the catalog does not contain is rejected with
/// [`ToolControlError::UnknownTool`] instead of creating a row.
#[derive(Clone, Debug)]
pub struct PgToolControl {
    pool: PgPool,
}

impl PgToolControl {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The pool used for catalog reads. Useful to callers that need to seed
    /// catalog rows or inspect raw control state.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

/// Inserts or refreshes catalog metadata at startup.
///
/// Existing control state is preserved: `enabled` is not overwritten on a
/// conflict, and visibility rows are untouched. Only descriptive metadata,
/// schema, source, default roles, deferred flag, and catalog metadata refresh.
pub async fn seed_catalog(
    pool: &PgPool,
    entries: &[ToolCatalogEntry],
) -> Result<(), ToolControlError> {
    let mut tx = pool.begin().await.map_err(store_error)?;
    for entry in entries {
        let metadata =
            serde_json::to_value(&entry.metadata).unwrap_or_else(|_| serde_json::json!({}));
        sqlx::query(
            "INSERT INTO tool_catalog
                 (key, source, description, input_schema, default_roles, enabled, deferred, metadata)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (key) DO UPDATE SET
                 source = EXCLUDED.source,
                 description = EXCLUDED.description,
                 input_schema = EXCLUDED.input_schema,
                 default_roles = EXCLUDED.default_roles,
                 deferred = EXCLUDED.deferred,
                 metadata = EXCLUDED.metadata,
                 updated_at = now()",
        )
        .bind(&entry.key)
        .bind(entry.source.as_str())
        .bind(&entry.description)
        .bind(&entry.input_schema)
        .bind(&entry.default_roles)
        .bind(entry.enabled)
        .bind(entry.deferred)
        .bind(metadata)
        .execute(&mut *tx)
        .await
        .map_err(store_error)?;
    }
    tx.commit().await.map_err(store_error)
}

#[derive(sqlx::FromRow)]
struct ToolStateRow {
    key: String,
    enabled: bool,
    roles: Vec<String>,
}

#[async_trait]
impl ToolControl for PgToolControl {
    async fn apply(&self, action: ToolControlAction) -> Result<Vec<ToolState>, ToolControlError> {
        match action {
            ToolControlAction::SetEnabled { key, enabled } => {
                let mut tx = self.pool.begin().await.map_err(store_error)?;
                let updated = sqlx::query(
                    "UPDATE tool_catalog
                     SET enabled = $2, updated_at = now()
                     WHERE key = $1",
                )
                .bind(&key)
                .bind(enabled)
                .execute(&mut *tx)
                .await
                .map_err(store_error)?;
                if updated.rows_affected() == 0 {
                    return Err(ToolControlError::UnknownTool(key));
                }
                let states = fetch_states(&mut tx).await?;
                tx.commit().await.map_err(store_error)?;
                Ok(states)
            }
            ToolControlAction::SetVisibility { key, roles } => {
                // Validate before touching rows: a bad role must not leave a
                // half-replaced visibility set behind.
                let roles = normalize_roles(roles)?;
                let mut tx = self.pool.begin().await.map_err(store_error)?;
                let exists: bool =
                    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tool_catalog WHERE key = $1)")
                        .bind(&key)
                        .fetch_one(&mut *tx)
                        .await
                        .map_err(store_error)?;
                if !exists {
                    return Err(ToolControlError::UnknownTool(key));
                }
                sqlx::query("DELETE FROM tool_visibility WHERE tool_key = $1")
                    .bind(&key)
                    .execute(&mut *tx)
                    .await
                    .map_err(store_error)?;
                for role in &roles {
                    sqlx::query("INSERT INTO tool_visibility (tool_key, role) VALUES ($1, $2)")
                        .bind(&key)
                        .bind(role)
                        .execute(&mut *tx)
                        .await
                        .map_err(store_error)?;
                }
                let states = fetch_states(&mut tx).await?;
                tx.commit().await.map_err(store_error)?;
                Ok(states)
            }
        }
    }

    async fn states(&self) -> Result<Vec<ToolState>, ToolControlError> {
        let mut conn = self.pool.acquire().await.map_err(store_error)?;
        fetch_states(&mut conn).await
    }

    async fn record_skill_usage(&self, record: &SkillUsageRecord) -> Result<(), ToolControlError> {
        let skill = normalize_name("skill", &record.skill)?;
        let role = normalize_role(&record.role)?;
        let task_id = parse_optional_uuid("task_id", record.task_id.as_deref())?;
        let intent_id = parse_optional_uuid("intent_id", record.intent_id.as_deref())?;
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        sqlx::query(
            "INSERT INTO skill_usage (skill, role, task_id, intent_id, found)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(&skill)
        .bind(&role)
        .bind(task_id)
        .bind(intent_id)
        .bind(record.found)
        .execute(&mut *tx)
        .await
        .map_err(store_error)?;
        tx.commit().await.map_err(store_error)?;
        Ok(())
    }
}

async fn fetch_states(conn: &mut PgConnection) -> Result<Vec<ToolState>, ToolControlError> {
    let rows: Vec<ToolStateRow> = sqlx::query_as(
        "SELECT c.key,
                c.enabled,
                COALESCE(
                    array_agg(v.role) FILTER (WHERE v.role IS NOT NULL),
                    ARRAY[]::text[]
                ) AS roles
         FROM tool_catalog c
         LEFT JOIN tool_visibility v ON v.tool_key = c.key
         GROUP BY c.key, c.enabled",
    )
    .fetch_all(conn)
    .await
    .map_err(store_error)?;
    let mut states: Vec<ToolState> = rows
        .into_iter()
        .map(|row| {
            let mut roles = row.roles;
            roles.sort();
            ToolState {
                key: row.key,
                enabled: row.enabled,
                roles,
            }
        })
        .collect();
    // Stable order independent of the database collation.
    states.sort_by(|left, right| left.key.cmp(&right.key));
    Ok(states)
}

/// Recognized roles are the four base roles plus `specialist:<name>`.
/// Surrounding whitespace is ignored; the stored role is canonical.
fn normalize_role(raw: &str) -> Result<String, ToolControlError> {
    if raw.chars().any(char::is_control) {
        return Err(ToolControlError::Invalid(
            "role contains control characters".to_string(),
        ));
    }
    let role = raw.trim();
    if role.is_empty() {
        return Err(ToolControlError::Invalid(
            "role must not be empty".to_string(),
        ));
    }
    match role {
        "planner" => Ok("planner".to_string()),
        "worker" => Ok("worker".to_string()),
        "reporter" => Ok("reporter".to_string()),
        "digest" => Ok("digest".to_string()),
        _ => {
            let name = role
                .strip_prefix("specialist:")
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .ok_or_else(invalid_role)?;
            Ok(format!("specialist:{name}"))
        }
    }
}

/// Normalizes and deduplicates a visibility list. An empty list is valid and
/// means the tool is visible to nobody.
fn normalize_roles(roles: Vec<String>) -> Result<BTreeSet<String>, ToolControlError> {
    let mut normalized = BTreeSet::new();
    for role in roles {
        normalized.insert(normalize_role(&role)?);
    }
    Ok(normalized)
}

fn invalid_role() -> ToolControlError {
    ToolControlError::Invalid(
        "unknown role; expected planner, worker, reporter, digest, or specialist:<name>"
            .to_string(),
    )
}

fn normalize_name(field: &str, raw: &str) -> Result<String, ToolControlError> {
    if raw.chars().any(char::is_control) {
        return Err(ToolControlError::Invalid(format!(
            "{field} contains control characters"
        )));
    }
    let value = raw.trim();
    if value.is_empty() {
        return Err(ToolControlError::Invalid(format!(
            "{field} must not be empty"
        )));
    }
    Ok(value.to_string())
}

fn parse_optional_uuid(field: &str, raw: Option<&str>) -> Result<Option<Uuid>, ToolControlError> {
    match raw {
        None => Ok(None),
        Some(value) => Uuid::parse_str(value.trim())
            .map(Some)
            .map_err(|_| ToolControlError::Invalid(format!("{field} must be a UUID"))),
    }
}

/// Database failures never carry bound values or raw server messages into
/// errors; only a stable SQLSTATE or a generic category is exposed.
fn store_error(error: sqlx::Error) -> ToolControlError {
    let detail = match &error {
        sqlx::Error::Database(database) => match database.code() {
            Some(code) => format!("database error {code}"),
            None => "database error".to_string(),
        },
        sqlx::Error::PoolTimedOut => "database pool timed out".to_string(),
        sqlx::Error::PoolClosed => "database pool closed".to_string(),
        sqlx::Error::Io(_) => "database connection failed".to_string(),
        sqlx::Error::RowNotFound => "row not found".to_string(),
        _ => "database operation failed".to_string(),
    };
    ToolControlError::Store(detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_round_trip_with_a_stable_tag() {
        let value = serde_json::to_value(ToolControlAction::SetEnabled {
            key: "web_search".to_string(),
            enabled: false,
        })
        .expect("json");
        assert_eq!(value["action"], "set_enabled");
        assert_eq!(value["key"], "web_search");
        assert_eq!(value["enabled"], false);

        let action: ToolControlAction = serde_json::from_value(value).expect("action");
        assert_eq!(
            action,
            ToolControlAction::SetEnabled {
                key: "web_search".to_string(),
                enabled: false
            }
        );
    }

    #[test]
    fn visibility_shapes_are_stable() {
        let action = ToolControlAction::SetVisibility {
            key: "echo_custom".to_string(),
            roles: vec!["worker".to_string()],
        };
        let value = serde_json::to_value(&action).expect("json");
        assert_eq!(value["action"], "set_visibility");
        assert_eq!(value["roles"][0], "worker");
    }
}
