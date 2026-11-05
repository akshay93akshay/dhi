//! ARTEX-parity management tables: LLM profiles, guard rules, app settings,
//! and web provider settings.
//!
//! These writes are the write-side source of truth. Live reads for policy and
//! pool resolution go through the in-memory snapshots in `dhi-agent` and
//! `dhi-guard`, which the server rebuilds after every mutation here.
//!
//! Semantics copied from ARTEX (`artex-global/db/config.go`,
//! `server/server_mgmt.go`), reduced to what DHI uses:
//! - `api_key` is write-only: no read method selects it except
//!   [`Store::get_profile_with_key`] and [`Store::list_profiles_with_keys`],
//!   which feed client construction only.
//! - The key hint is `…<last 4>` computed on save (ARTEX style).
//! - Deleting a profile that is an active default or is pinned by any task
//!   is refused (`InUse` → HTTP 409).
//! - Guards evaluate enabled rules by priority DESC; first match wins.

use chrono::{DateTime, Utc};

use crate::StoreError;

/// The last-4 key hint: `…abcd`. Keys shorter than four characters produce an
/// empty hint (mirrors ARTEX, which declines to leak a tiny key).
pub fn key_hint(api_key: &str) -> String {
    let key = api_key.trim();
    if key.chars().count() >= 4 {
        let tail: String = key
            .chars()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("…{tail}")
    } else {
        String::new()
    }
}

fn valid_format(format: &str) -> Result<(), StoreError> {
    match format {
        "openai" | "anthropic" => Ok(()),
        other => Err(StoreError::Validation(format!(
            "unknown format {other:?}: must be \"openai\" or \"anthropic\""
        ))),
    }
}

fn valid_thinking_type(value: &str) -> Result<(), StoreError> {
    match value {
        "" | "disabled" | "enabled" => Ok(()),
        other => Err(StoreError::Validation(format!(
            "unknown thinking_type {other:?}: must be \"\", \"disabled\", or \"enabled\""
        ))),
    }
}

fn valid_reasoning_effort(value: &str) -> Result<(), StoreError> {
    match value {
        "" | "low" | "medium" | "high" | "xhigh" | "max" => Ok(()),
        other => Err(StoreError::Validation(format!(
            "unknown reasoning_effort {other:?}: must be one of \"\", \"low\", \
             \"medium\", \"high\", \"xhigh\", or \"max\""
        ))),
    }
}

/// One row of `llm_profiles` without its API key. This is the only shape a
/// read route ever sees.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct LlmProfileOut {
    pub id: i64,
    pub name: String,
    pub format: String,
    pub base_url: String,
    pub model: String,
    pub api_key_hint: String,
    pub max_tokens: i32,
    pub context_window_k: i32,
    pub streaming: bool,
    pub priority: i32,
    pub pool_exclude: bool,
    pub thinking_type: String,
    pub reasoning_effort: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// One profile with its API key. Never serialized to an HTTP response; used
/// only to build provider clients and the `/models` proxy call.
#[derive(Clone, sqlx::FromRow)]
pub struct LlmProfileFull {
    pub id: i64,
    pub name: String,
    pub format: String,
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub api_key_hint: String,
    pub max_tokens: i32,
    pub context_window_k: i32,
    pub streaming: bool,
    pub priority: i32,
    pub pool_exclude: bool,
    pub thinking_type: String,
    pub reasoning_effort: String,
}

/// The API key is a secret: it never reaches `Debug` output (logs, test
/// failures, panic messages).
impl std::fmt::Debug for LlmProfileFull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmProfileFull")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("format", &self.format)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &"<redacted>")
            .field("api_key_hint", &self.api_key_hint)
            .field("max_tokens", &self.max_tokens)
            .field("context_window_k", &self.context_window_k)
            .field("streaming", &self.streaming)
            .field("priority", &self.priority)
            .field("pool_exclude", &self.pool_exclude)
            .field("thinking_type", &self.thinking_type)
            .field("reasoning_effort", &self.reasoning_effort)
            .finish()
    }
}

/// Write payload for [`Store::save_profile`]. `id = None` or `id <= 0`
/// inserts (ARTEX treats 0 as new); `Some(id > 0)` updates that row. An
/// empty `api_key` on update keeps the stored key.
#[derive(Clone)]
pub struct LlmProfileUpsert {
    pub id: Option<i64>,
    pub name: String,
    pub format: String,
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub max_tokens: i32,
    pub context_window_k: i32,
    pub streaming: bool,
    pub priority: i32,
    pub pool_exclude: bool,
    pub thinking_type: String,
    pub reasoning_effort: String,
}

impl std::fmt::Debug for LlmProfileUpsert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmProfileUpsert")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("format", &self.format)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &"<redacted>")
            .field("max_tokens", &self.max_tokens)
            .field("context_window_k", &self.context_window_k)
            .field("streaming", &self.streaming)
            .field("priority", &self.priority)
            .field("pool_exclude", &self.pool_exclude)
            .field("thinking_type", &self.thinking_type)
            .field("reasoning_effort", &self.reasoning_effort)
            .finish()
    }
}

impl LlmProfileUpsert {
    /// Request-level validation. SQL CHECKs re-enforce the format enum.
    pub fn validate(&self) -> Result<(), StoreError> {
        valid_format(&self.format)?;
        valid_thinking_type(&self.thinking_type)?;
        valid_reasoning_effort(&self.reasoning_effort)?;
        if self.name.trim().is_empty() {
            return Err(StoreError::Validation("name must not be empty".into()));
        }
        if self.model.trim().is_empty() {
            return Err(StoreError::Validation("model must not be empty".into()));
        }
        if self.max_tokens < 0 {
            // Negative caps are meaningless; clamp like ARTEX instead of 400.
            return Err(StoreError::Validation(
                "max_tokens must not be negative (send 0 for no cap)".into(),
            ));
        }
        Ok(())
    }
}

/// One row of `guard_rules`.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct GuardRuleRow {
    pub id: i64,
    pub name: String,
    pub match_target: String,
    pub match_type: String,
    pub pattern: String,
    pub action: String,
    pub reason: String,
    pub priority: i32,
    pub enabled: bool,
    pub timeout_secs: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Write payload for a guard rule (create or full replace).
#[derive(Debug, Clone)]
pub struct GuardRuleInput {
    pub name: String,
    pub match_target: String,
    pub match_type: String,
    pub pattern: String,
    pub action: String,
    pub reason: String,
    pub priority: i32,
    pub enabled: bool,
    pub timeout_secs: i32,
}

impl GuardRuleInput {
    /// Enum + emptiness checks. Regex patterns are validated at the API layer
    /// by `dhi-guard`'s compiler; SQL CHECKs re-enforce the enums.
    pub fn validate(&self) -> Result<(), StoreError> {
        if self.name.trim().is_empty() {
            return Err(StoreError::Validation("name must not be empty".into()));
        }
        if !matches!(self.match_target.as_str(), "tool_name" | "tool_input") {
            return Err(StoreError::Validation(format!(
                "unknown match_target {:?}: must be \"tool_name\" or \"tool_input\"",
                self.match_target
            )));
        }
        if !matches!(self.match_type.as_str(), "exact" | "regex") {
            return Err(StoreError::Validation(format!(
                "unknown match_type {:?}: must be \"exact\" or \"regex\"",
                self.match_type
            )));
        }
        if self.pattern.trim().is_empty() {
            return Err(StoreError::Validation("pattern must not be empty".into()));
        }
        if !matches!(self.action.as_str(), "allow" | "deny" | "ask") {
            return Err(StoreError::Validation(format!(
                "unknown action {:?}: must be \"allow\", \"deny\", or \"ask\"",
                self.action
            )));
        }
        if self.timeout_secs < 0 {
            return Err(StoreError::Validation(
                "timeout_secs must not be negative (0 = app default)".into(),
            ));
        }
        Ok(())
    }
}

/// One `web_provider_settings` row WITHOUT its API key. This is the shape
/// management reads expose; keys are write-only.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct WebProviderSettingRow {
    pub provider: String,
    pub api_key_hint: Option<String>,
    pub base_url: Option<String>,
    pub timeout_secs: Option<i64>,
    pub enabled: bool,
    pub priority: i32,
    pub updated_at: DateTime<Utc>,
}

/// One web provider row with its API key. Never serialized to an HTTP
/// response; used only by the router builder.
#[derive(Clone, sqlx::FromRow)]
pub struct WebProviderSettingFull {
    pub provider: String,
    pub api_key: Option<String>,
    pub api_key_hint: Option<String>,
    pub base_url: Option<String>,
    pub timeout_secs: Option<i64>,
    pub enabled: bool,
    pub priority: i32,
    pub updated_at: DateTime<Utc>,
}

impl std::fmt::Debug for WebProviderSettingFull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebProviderSettingFull")
            .field("provider", &self.provider)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("api_key_hint", &self.api_key_hint)
            .field("base_url", &self.base_url)
            .field("timeout_secs", &self.timeout_secs)
            .field("enabled", &self.enabled)
            .field("priority", &self.priority)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

/// Partial write for one web provider setting.
///
/// `api_key = None` and `api_key = Some("")` both keep the stored key. A
/// non-empty key replaces it and recomputes the hint. `base_url = None` keeps
/// the stored override; `Some("")` clears it. `None` for the remaining fields
/// keeps the stored value (or the column default on insert).
#[derive(Clone)]
pub struct WebProviderSettingUpsert {
    pub provider: String,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub timeout_secs: Option<u64>,
    pub enabled: Option<bool>,
}

impl std::fmt::Debug for WebProviderSettingUpsert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebProviderSettingUpsert")
            .field("provider", &self.provider)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("base_url", &self.base_url)
            .field("timeout_secs", &self.timeout_secs)
            .field("enabled", &self.enabled)
            .finish()
    }
}

impl WebProviderSettingUpsert {
    pub fn validate(&self) -> Result<(), StoreError> {
        valid_web_provider(&self.provider)?;
        if let Some(seconds) = self.timeout_secs
            && !(1..=3_600).contains(&seconds)
        {
            return Err(StoreError::Validation(
                "timeout_secs must be between 1 and 3600".into(),
            ));
        }
        Ok(())
    }
}

fn valid_web_provider(provider: &str) -> Result<(), StoreError> {
    match provider {
        "tinyfish" | "parallel" | "firecrawl" => Ok(()),
        other => Err(StoreError::Validation(format!(
            "unknown web provider {other:?}: must be tinyfish, parallel, or firecrawl"
        ))),
    }
}

fn web_provider_priority(provider: &str) -> i32 {
    match provider {
        "tinyfish" => 1,
        "parallel" => 2,
        "firecrawl" => 3,
        _ => 0,
    }
}

/// The display hint stored beside a web provider key: `…` plus at most the
/// last four characters. Unlike [`key_hint`], a key shorter than four
/// characters still gets a hint (the contract allows "up to" four).
fn web_key_hint(api_key: &str) -> Option<String> {
    let key = api_key.trim();
    if key.is_empty() {
        return None;
    }
    let tail: String = key
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Some(format!("…{tail}"))
}

const WEB_PROVIDER_SETTING_OUT_COLS: &str =
    "provider, api_key_hint, base_url, timeout_secs, enabled, priority, updated_at";
const WEB_PROVIDER_SETTING_FULL_COLS: &str =
    "provider, api_key, api_key_hint, base_url, timeout_secs, enabled, priority, updated_at";

/// The singleton `app_settings` row.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct AppSettingsRow {
    pub planner_profile_id: i64,
    pub worker_profile_id: i64,
    pub approval_timeout_secs: i32,
    pub guard_default_action: String,
    pub updated_at: DateTime<Utc>,
}

/// Partial update for [`Store::update_settings`]: `None` keeps the stored value.
#[derive(Debug, Clone, Default)]
pub struct SettingPatch {
    pub planner_profile_id: Option<i64>,
    pub worker_profile_id: Option<i64>,
    pub approval_timeout_secs: Option<i32>,
    /// Replacement default action for unmatched tool calls: `allow`, `deny`,
    /// or `ask` (validated, otherwise 400).
    pub guard_default_action: Option<String>,
}

/// The approval-expiry default the engine ships with (`APPROVAL_TTL_MINUTES`
/// = 15 in the server binary). [`Store::save_profile`] seeds the settings
/// singleton with it; the same value backs `BootstrapSeed` on first boot.
pub const DEFAULT_APPROVAL_TIMEOUT_SECS: i32 = 900;

/// First-boot seed material: the same values `dhi_agent::LlmProfile::from_env`
/// resolves for the live-River runtime. The store never reads the environment.
#[derive(Clone)]
pub struct BootstrapSeed {
    pub base_url: String,
    pub api_key: String,
    pub planner_model: String,
    pub worker_model: String,
    /// The planner profile's `''` | `disabled` | `enabled`.
    pub planner_thinking_type: String,
    /// The planner profile's `''` | `low` | `medium` | `high` | `xhigh` | `max`.
    pub planner_reasoning_effort: String,
    /// The worker profile's `''` | `disabled` | `enabled`.
    pub worker_thinking_type: String,
    /// The worker profile's `''` | `low` | `medium` | `high` | `xhigh` | `max`.
    pub worker_reasoning_effort: String,
    /// The engine's current approval-expiry default ([`DEFAULT_APPROVAL_TIMEOUT_SECS`]).
    pub approval_timeout_secs: i32,
}

impl std::fmt::Debug for BootstrapSeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BootstrapSeed")
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("planner_model", &self.planner_model)
            .field("worker_model", &self.worker_model)
            .field("planner_thinking_type", &self.planner_thinking_type)
            .field("planner_reasoning_effort", &self.planner_reasoning_effort)
            .field("worker_thinking_type", &self.worker_thinking_type)
            .field("worker_reasoning_effort", &self.worker_reasoning_effort)
            .field("approval_timeout_secs", &self.approval_timeout_secs)
            .finish()
    }
}

const PROFILE_OUT_COLS: &str = "id, name, format, base_url, model, api_key_hint, max_tokens, \
     context_window_k, streaming, priority, pool_exclude, thinking_type, reasoning_effort, \
     created_at, updated_at";
const PROFILE_FULL_COLS: &str = "id, name, format, base_url, model, api_key, api_key_hint, \
     max_tokens, context_window_k, streaming, priority, pool_exclude, thinking_type, \
     reasoning_effort";

impl super::Store {
    // ---------- profiles ----------

    /// Every profile, WITHOUT the API key. Ordered by id for stable diffs.
    pub async fn list_profiles(&self) -> Result<Vec<LlmProfileOut>, StoreError> {
        let sql = format!("SELECT {PROFILE_OUT_COLS} FROM llm_profiles ORDER BY id");
        let rows = sqlx::query_as::<_, LlmProfileOut>(&sql)
            .fetch_all(self.pool())
            .await?;
        Ok(rows)
    }

    /// Every profile WITH keys, priority DESC / id ASC — the order the pool
    /// chain builder consumes. Write-side only; never serialized outward.
    pub async fn list_profiles_with_keys(&self) -> Result<Vec<LlmProfileFull>, StoreError> {
        let sql =
            format!("SELECT {PROFILE_FULL_COLS} FROM llm_profiles ORDER BY priority DESC, id ASC");
        let rows = sqlx::query_as::<_, LlmProfileFull>(&sql)
            .fetch_all(self.pool())
            .await?;
        Ok(rows)
    }

    /// One profile with its key, for client construction (`/api/llm/models`
    /// proxy). `None` when the id is unknown.
    pub async fn get_profile_with_key(
        &self,
        id: i64,
    ) -> Result<Option<LlmProfileFull>, StoreError> {
        let sql = format!("SELECT {PROFILE_FULL_COLS} FROM llm_profiles WHERE id = $1");
        let row = sqlx::query_as::<_, LlmProfileFull>(&sql)
            .bind(id)
            .fetch_optional(self.pool())
            .await?;
        Ok(row)
    }

    /// Whether the profile id exists. Used to validate task pins (400 on
    /// unknown) and settings references.
    pub async fn profile_exists(&self, id: i64) -> Result<bool, StoreError> {
        let exists: Option<(i64,)> = sqlx::query_as("SELECT id FROM llm_profiles WHERE id = $1")
            .bind(id)
            .fetch_optional(self.pool())
            .await?;
        Ok(exists.is_some())
    }

    /// Inserts (`id` absent, or `id <= 0` — ARTEX treats 0 as new) or updates
    /// (`Some(id > 0)`) one profile. Computes the key hint; an empty key on
    /// update keeps the stored key and hint. A negative `max_tokens` is
    /// clamped to 0 (no cap), like ARTEX. Returns the row id.
    ///
    /// An INSERT bootstraps the `app_settings` singleton when it does not
    /// exist yet (same transaction): both role defaults point at the new
    /// profile with [`DEFAULT_APPROVAL_TIMEOUT_SECS`] and `deny`. Without
    /// this, a database that never saw the env seed (mock-mode boot) could
    /// never reach `GET /api/settings`.
    pub async fn save_profile(&self, upsert: &LlmProfileUpsert) -> Result<i64, StoreError> {
        let mut upsert = upsert.clone();
        if upsert.max_tokens < 0 {
            upsert.max_tokens = 0;
        }
        upsert.validate()?;
        let pool = self.pool();
        // ARTEX parity: a missing id, 0, or a negative means "new row";
        // only a positive id updates.
        let Some(id) = upsert.id.filter(|id| *id > 0) else {
            let mut tx = pool.begin().await?;
            let hint = key_hint(&upsert.api_key);
            let row: (i64,) = sqlx::query_as(
                "INSERT INTO llm_profiles
                   (name, format, base_url, model, api_key, api_key_hint, max_tokens,
                    context_window_k, streaming, priority, pool_exclude, thinking_type,
                    reasoning_effort)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
                 RETURNING id",
            )
            .bind(upsert.name.trim())
            .bind(&upsert.format)
            .bind(upsert.base_url.trim())
            .bind(upsert.model.trim())
            .bind(upsert.api_key.trim())
            .bind(hint)
            .bind(upsert.max_tokens)
            .bind(upsert.context_window_k)
            .bind(upsert.streaming)
            .bind(upsert.priority)
            .bind(upsert.pool_exclude)
            .bind(&upsert.thinking_type)
            .bind(&upsert.reasoning_effort)
            .fetch_one(&mut *tx)
            .await?;
            let new_id = row.0;
            // First profile ever: point both role defaults at it. A settings
            // row that already exists (env-seeded or operator-made) wins —
            // `ON CONFLICT DO NOTHING` never overwrites operator edits.
            sqlx::query(
                "INSERT INTO app_settings
                   (id, planner_profile_id, worker_profile_id, approval_timeout_secs,
                    guard_default_action)
                 VALUES (1, $1, $1, $2, 'deny')
                 ON CONFLICT (id) DO NOTHING",
            )
            .bind(new_id)
            .bind(DEFAULT_APPROVAL_TIMEOUT_SECS)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(new_id);
        };
        let updated = if upsert.api_key.trim().is_empty() {
            sqlx::query(
                "UPDATE llm_profiles SET
                   name = $2, format = $3, base_url = $4, model = $5,
                   max_tokens = $6, context_window_k = $7, streaming = $8,
                   priority = $9, pool_exclude = $10, thinking_type = $11,
                   reasoning_effort = $12, updated_at = now()
                 WHERE id = $1",
            )
            .bind(id)
            .bind(upsert.name.trim())
            .bind(&upsert.format)
            .bind(upsert.base_url.trim())
            .bind(upsert.model.trim())
            .bind(upsert.max_tokens)
            .bind(upsert.context_window_k)
            .bind(upsert.streaming)
            .bind(upsert.priority)
            .bind(upsert.pool_exclude)
            .bind(&upsert.thinking_type)
            .bind(&upsert.reasoning_effort)
            .execute(pool)
            .await?
        } else {
            let hint = key_hint(&upsert.api_key);
            sqlx::query(
                "UPDATE llm_profiles SET
                   name = $2, format = $3, base_url = $4, model = $5,
                   api_key = $6, api_key_hint = $7,
                   max_tokens = $8, context_window_k = $9, streaming = $10,
                   priority = $11, pool_exclude = $12, thinking_type = $13,
                   reasoning_effort = $14, updated_at = now()
                 WHERE id = $1",
            )
            .bind(id)
            .bind(upsert.name.trim())
            .bind(&upsert.format)
            .bind(upsert.base_url.trim())
            .bind(upsert.model.trim())
            .bind(upsert.api_key.trim())
            .bind(hint)
            .bind(upsert.max_tokens)
            .bind(upsert.context_window_k)
            .bind(upsert.streaming)
            .bind(upsert.priority)
            .bind(upsert.pool_exclude)
            .bind(&upsert.thinking_type)
            .bind(&upsert.reasoning_effort)
            .execute(pool)
            .await?
        };
        if updated.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("llm profile {id}")));
        }
        Ok(id)
    }

    /// Deletes a profile unless it is an active default for a role or pinned
    /// by any task. `NotFound` when missing, `InUse` when referenced.
    pub async fn delete_profile(&self, id: i64) -> Result<(), StoreError> {
        let pool = self.pool();
        let mut tx = pool.begin().await?;

        let exists: Option<(i64,)> =
            sqlx::query_as("SELECT id FROM llm_profiles WHERE id = $1 FOR UPDATE")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        if exists.is_none() {
            tx.rollback().await?;
            return Err(StoreError::NotFound(format!("llm profile {id}")));
        }

        let pinned: (i64, i64) = sqlx::query_as(
            "SELECT
               (SELECT count(*) FROM app_settings
                 WHERE planner_profile_id = $1 OR worker_profile_id = $1),
               (SELECT count(*) FROM tasks
                 WHERE pinned_planner_profile_id = $1 OR pinned_worker_profile_id = $1)",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        if pinned.0 > 0 {
            tx.rollback().await?;
            return Err(StoreError::InUse(
                "profile is the active default for a role; activate another profile first".into(),
            ));
        }
        if pinned.1 > 0 {
            tx.rollback().await?;
            return Err(StoreError::InUse(
                "profile is pinned by one or more tasks; unpin them first".into(),
            ));
        }

        sqlx::query("DELETE FROM llm_profiles WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    // ---------- settings ----------

    /// The singleton settings row, or `None` before the first seed.
    pub async fn get_settings(&self) -> Result<Option<AppSettingsRow>, StoreError> {
        let row = sqlx::query_as::<_, AppSettingsRow>(
            "SELECT planner_profile_id, worker_profile_id, approval_timeout_secs, \
                    guard_default_action, updated_at
             FROM app_settings WHERE id = 1",
        )
        .fetch_optional(self.pool())
        .await?;
        Ok(row)
    }

    /// Point one role's active default at `id`. `NotFound` when the profile
    /// (or the settings row) is missing.
    pub async fn set_active_profile(&self, role: &str, id: i64) -> Result<(), StoreError> {
        let column = match role {
            "planner" => "planner_profile_id",
            "worker" => "worker_profile_id",
            other => {
                return Err(StoreError::Validation(format!(
                    "unknown role {other:?}: must be \"planner\" or \"worker\""
                )));
            }
        };
        let pool = self.pool();
        if !self.profile_exists(id).await? {
            return Err(StoreError::NotFound(format!("llm profile {id}")));
        }
        // `column` comes from the whitelist above, never from user input.
        let sql = format!("UPDATE app_settings SET {column} = $1, updated_at = now() WHERE id = 1");
        let result = sqlx::query(&sql).bind(id).execute(pool).await?;
        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound(
                "app settings are not initialized".into(),
            ));
        }
        Ok(())
    }

    /// Partial settings merge. Validates profile references, the positive
    /// timeout, and the `guard_default_action` enum, then persists. Runs as
    /// one transaction that locks the singleton row (`SELECT … FOR UPDATE`),
    /// so concurrent admins merge against the committed value instead of
    /// racing a stale read (last committer wins per locked write, and no
    /// torn read-modify-write). Returns the merged row as committed.
    pub async fn update_settings(
        &self,
        patch: &SettingPatch,
    ) -> Result<AppSettingsRow, StoreError> {
        if let Some(secs) = patch.approval_timeout_secs
            && secs < 1
        {
            return Err(StoreError::Validation(
                "approval_timeout_secs must be at least 1".into(),
            ));
        }
        let patch_action = match patch.guard_default_action.as_deref().map(str::trim) {
            None => None,
            Some(action @ ("allow" | "deny" | "ask")) => Some(action.to_string()),
            Some(other) => {
                return Err(StoreError::Validation(format!(
                    "unknown guard_default_action {other:?}: must be \"allow\", \"deny\", or \"ask\""
                )));
            }
        };

        let mut tx = self.pool().begin().await?;

        let current: Option<AppSettingsRow> = sqlx::query_as(
            "SELECT planner_profile_id, worker_profile_id, approval_timeout_secs, \
             guard_default_action, updated_at
             FROM app_settings WHERE id = 1 FOR UPDATE",
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(current) = current else {
            return Err(StoreError::NotFound(
                "app settings are not initialized".into(),
            ));
        };

        let planner = patch
            .planner_profile_id
            .unwrap_or(current.planner_profile_id);
        let worker = patch.worker_profile_id.unwrap_or(current.worker_profile_id);
        let timeout = patch
            .approval_timeout_secs
            .unwrap_or(current.approval_timeout_secs);
        let guard_default_action = patch_action.unwrap_or(current.guard_default_action);

        let planner_exists: Option<(i64,)> =
            sqlx::query_as("SELECT id FROM llm_profiles WHERE id = $1")
                .bind(planner)
                .fetch_optional(&mut *tx)
                .await?;
        if planner_exists.is_none() {
            return Err(StoreError::NotFound(format!("llm profile {planner}")));
        }
        let worker_exists: Option<(i64,)> =
            sqlx::query_as("SELECT id FROM llm_profiles WHERE id = $1")
                .bind(worker)
                .fetch_optional(&mut *tx)
                .await?;
        if worker_exists.is_none() {
            return Err(StoreError::NotFound(format!("llm profile {worker}")));
        }

        let merged: AppSettingsRow = sqlx::query_as(
            "UPDATE app_settings
             SET planner_profile_id = $1, worker_profile_id = $2,
                 approval_timeout_secs = $3, guard_default_action = $4, updated_at = now()
             WHERE id = 1
             RETURNING planner_profile_id, worker_profile_id, approval_timeout_secs, \
                       guard_default_action, updated_at",
        )
        .bind(planner)
        .bind(worker)
        .bind(timeout)
        .bind(&guard_default_action)
        .fetch_one(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(merged)
    }

    // ---------- web provider settings ----------

    /// Every saved web provider setting, WITHOUT its API key. Ordered by the
    /// documented priority (tinyfish, parallel, firecrawl).
    pub async fn list_web_provider_settings(
        &self,
    ) -> Result<Vec<WebProviderSettingRow>, StoreError> {
        let sql = format!(
            "SELECT {WEB_PROVIDER_SETTING_OUT_COLS} FROM web_provider_settings \
             ORDER BY priority, provider"
        );
        sqlx::query_as::<_, WebProviderSettingRow>(&sql)
            .fetch_all(self.pool())
            .await
            .map_err(Into::into)
    }

    /// Every saved web provider setting WITH keys — the router builder's
    /// input. Write-side only; never serialized outward.
    pub async fn list_web_provider_settings_with_keys(
        &self,
    ) -> Result<Vec<WebProviderSettingFull>, StoreError> {
        let sql = format!(
            "SELECT {WEB_PROVIDER_SETTING_FULL_COLS} FROM web_provider_settings \
             ORDER BY priority, provider"
        );
        sqlx::query_as::<_, WebProviderSettingFull>(&sql)
            .fetch_all(self.pool())
            .await
            .map_err(Into::into)
    }

    /// Inserts or partially updates one web provider setting. A row that
    /// exists is authoritative for its key: `None`/empty keeps the stored key,
    /// while [`Store::clear_web_provider_key`] removes it. Returns the row
    /// without the key.
    pub async fn upsert_web_provider_setting(
        &self,
        upsert: &WebProviderSettingUpsert,
    ) -> Result<WebProviderSettingRow, StoreError> {
        upsert.validate()?;
        let provider = upsert.provider.trim().to_ascii_lowercase();
        valid_web_provider(&provider)?;

        let mut tx = self.pool().begin().await?;
        let existing: Option<WebProviderSettingFull> = sqlx::query_as(
            "SELECT provider, api_key, api_key_hint, base_url, timeout_secs, enabled, \
                    priority, updated_at
             FROM web_provider_settings WHERE provider = $1 FOR UPDATE",
        )
        .bind(&provider)
        .fetch_optional(&mut *tx)
        .await?;

        let replacement_key = upsert
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty());
        let (api_key, api_key_hint) = match replacement_key {
            Some(key) => (Some(key.to_string()), web_key_hint(key)),
            None => (
                existing.as_ref().and_then(|row| row.api_key.clone()),
                existing.as_ref().and_then(|row| row.api_key_hint.clone()),
            ),
        };
        let base_url = match &upsert.base_url {
            Some(raw) => {
                let trimmed = raw.trim();
                (!trimmed.is_empty()).then(|| trimmed.to_string())
            }
            None => existing.as_ref().and_then(|row| row.base_url.clone()),
        };
        let timeout_secs = match upsert.timeout_secs {
            Some(seconds) => Some(
                i64::try_from(seconds)
                    .map_err(|_| StoreError::Validation("timeout_secs is out of range".into()))?,
            ),
            None => existing.as_ref().and_then(|row| row.timeout_secs),
        };
        let enabled = upsert
            .enabled
            .or_else(|| existing.as_ref().map(|row| row.enabled))
            .unwrap_or(true);
        let priority = web_provider_priority(&provider);

        let sql = format!(
            "INSERT INTO web_provider_settings
               (provider, api_key, api_key_hint, base_url, timeout_secs, enabled, priority)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (provider) DO UPDATE SET
               api_key = EXCLUDED.api_key,
               api_key_hint = EXCLUDED.api_key_hint,
               base_url = EXCLUDED.base_url,
               timeout_secs = EXCLUDED.timeout_secs,
               enabled = EXCLUDED.enabled,
               priority = EXCLUDED.priority,
               updated_at = now()
             RETURNING {WEB_PROVIDER_SETTING_OUT_COLS}"
        );
        let row = sqlx::query_as::<_, WebProviderSettingRow>(&sql)
            .bind(&provider)
            .bind(&api_key)
            .bind(&api_key_hint)
            .bind(&base_url)
            .bind(timeout_secs)
            .bind(enabled)
            .bind(priority)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(row)
    }

    /// Clears the stored key and hint but keeps the row (and every other
    /// setting). An absent row is created with a null key so an environment
    /// key cannot silently come back: the row is authoritative as soon as it
    /// exists.
    pub async fn clear_web_provider_key(
        &self,
        provider: &str,
    ) -> Result<WebProviderSettingRow, StoreError> {
        let provider = provider.trim().to_ascii_lowercase();
        valid_web_provider(&provider)?;
        let priority = web_provider_priority(&provider);
        let sql = format!(
            "INSERT INTO web_provider_settings
               (provider, api_key, api_key_hint, priority)
             VALUES ($1, NULL, NULL, $2)
             ON CONFLICT (provider) DO UPDATE SET
               api_key = NULL,
               api_key_hint = NULL,
               updated_at = now()
             RETURNING {WEB_PROVIDER_SETTING_OUT_COLS}"
        );
        sqlx::query_as::<_, WebProviderSettingRow>(&sql)
            .bind(&provider)
            .bind(priority)
            .fetch_one(self.pool())
            .await
            .map_err(Into::into)
    }

    // ---------- guard rules ----------

    /// Every rule, ordered by priority DESC then id ASC (evaluation order).
    pub async fn list_rules(&self) -> Result<Vec<GuardRuleRow>, StoreError> {
        sqlx::query_as::<_, GuardRuleRow>(
            "SELECT id, name, match_target, match_type, pattern, action, reason, priority, \
                    enabled, timeout_secs, created_at, updated_at
             FROM guard_rules ORDER BY priority DESC, id ASC",
        )
        .fetch_all(self.pool())
        .await
        .map_err(Into::into)
    }

    /// Enabled rules in evaluation order — the snapshot builder's input.
    pub async fn list_enabled_rules(&self) -> Result<Vec<GuardRuleRow>, StoreError> {
        sqlx::query_as::<_, GuardRuleRow>(
            "SELECT id, name, match_target, match_type, pattern, action, reason, priority, \
                    enabled, timeout_secs, created_at, updated_at
             FROM guard_rules WHERE enabled ORDER BY priority DESC, id ASC",
        )
        .fetch_all(self.pool())
        .await
        .map_err(Into::into)
    }

    /// Inserts one rule. Returns the new row.
    pub async fn create_rule(&self, input: &GuardRuleInput) -> Result<GuardRuleRow, StoreError> {
        input.validate()?;
        let row = sqlx::query_as::<_, GuardRuleRow>(
            "INSERT INTO guard_rules
               (name, match_target, match_type, pattern, action, reason, priority, enabled,
                timeout_secs)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             RETURNING id, name, match_target, match_type, pattern, action, reason, priority, \
                       enabled, timeout_secs, created_at, updated_at",
        )
        .bind(input.name.trim())
        .bind(&input.match_target)
        .bind(&input.match_type)
        .bind(input.pattern.trim())
        .bind(&input.action)
        .bind(&input.reason)
        .bind(input.priority)
        .bind(input.enabled)
        .bind(input.timeout_secs)
        .fetch_one(self.pool())
        .await?;
        Ok(row)
    }

    /// Full replace of one rule. `NotFound` when the id is unknown.
    pub async fn update_rule(
        &self,
        id: i64,
        input: &GuardRuleInput,
    ) -> Result<GuardRuleRow, StoreError> {
        input.validate()?;
        let row = sqlx::query_as::<_, GuardRuleRow>(
            "UPDATE guard_rules SET
               name = $2, match_target = $3, match_type = $4, pattern = $5, action = $6,
               reason = $7, priority = $8, enabled = $9, timeout_secs = $10, updated_at = now()
             WHERE id = $1
             RETURNING id, name, match_target, match_type, pattern, action, reason, priority, \
                       enabled, timeout_secs, created_at, updated_at",
        )
        .bind(id)
        .bind(input.name.trim())
        .bind(&input.match_target)
        .bind(&input.match_type)
        .bind(input.pattern.trim())
        .bind(&input.action)
        .bind(&input.reason)
        .bind(input.priority)
        .bind(input.enabled)
        .bind(input.timeout_secs)
        .fetch_optional(self.pool())
        .await?
        .ok_or_else(|| StoreError::NotFound(format!("guard rule {id}")))?;
        Ok(row)
    }

    /// Flips one rule's enabled flag. Returns the updated row.
    pub async fn toggle_rule(&self, id: i64, enabled: bool) -> Result<GuardRuleRow, StoreError> {
        sqlx::query_as::<_, GuardRuleRow>(
            "UPDATE guard_rules SET enabled = $2, updated_at = now() WHERE id = $1
             RETURNING id, name, match_target, match_type, pattern, action, reason, priority, \
                       enabled, timeout_secs, created_at, updated_at",
        )
        .bind(id)
        .bind(enabled)
        .fetch_optional(self.pool())
        .await?
        .ok_or_else(|| StoreError::NotFound(format!("guard rule {id}")))
    }

    /// Deletes one rule. `NotFound` when the id is unknown.
    pub async fn delete_rule(&self, id: i64) -> Result<(), StoreError> {
        let result = sqlx::query("DELETE FROM guard_rules WHERE id = $1")
            .bind(id)
            .execute(self.pool())
            .await?;
        if result.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("guard rule {id}")));
        }
        Ok(())
    }

    // ---------- first-boot seed ----------

    /// Seeds the management tables exactly once, on an empty database:
    /// - one `openai` profile per role (`planner-default` / `worker-default`)
    ///   from the runtime's env-resolved values, when `llm_profiles` is empty;
    /// - the singleton `app_settings` row pointing the roles at those profiles
    ///   with the caller's approval timeout and `deny` as the guard default
    ///   (today's compiled-policy default);
    /// - the nineteen compiled-policy allow rules as `guard_rules` rows (exact
    ///   `tool_name` matches, priority 0, enabled, timeout 0): the four chess
    ///   MCP rules (`mcp__chess__createGame`, `mcp__chess__joinGame`,
    ///   `mcp__chess__waitForNextTurn`, `mcp__chess__finishTurn`), the four web
    ///   tools (web_search, web_read, web_research, web_parse), the six
    ///   Docker-per-task coding tools (read, write, edit, search, list,
    ///   shell), and the five persistent terminal tools (shell_open,
    ///   shell_send, shell_read, shell_close, shell_list).
    ///
    /// A second boot is a no-op: existing rows are never overwritten, so
    /// operator edits survive restarts. When the process has no LLM config
    /// (mock mode), profile/settings seeding is skipped; rules still seed.
    pub async fn seed_defaults(&self, seed: &BootstrapSeed) -> Result<(), StoreError> {
        let pool = self.pool();
        let mut tx = pool.begin().await?;

        let profile_count: i64 = sqlx::query_scalar("SELECT count(*) FROM llm_profiles")
            .fetch_one(&mut *tx)
            .await?;
        let mut planner_id: Option<i64> = None;
        let mut worker_id: Option<i64> = None;

        if profile_count == 0
            && !seed.base_url.trim().is_empty()
            && !seed.api_key.trim().is_empty()
            && !seed.planner_model.trim().is_empty()
            && !seed.worker_model.trim().is_empty()
        {
            let hint = key_hint(&seed.api_key);
            let (pid, wid): (i64, i64) = sqlx::query_as(
                "WITH p AS (
                   INSERT INTO llm_profiles
                     (name, format, base_url, model, api_key, api_key_hint, thinking_type,
                      reasoning_effort)
                   VALUES ('planner-default', 'openai', $1, $2, $3, $4, $6, $7) RETURNING id
                 ), w AS (
                   INSERT INTO llm_profiles
                     (name, format, base_url, model, api_key, api_key_hint, thinking_type,
                      reasoning_effort)
                   VALUES ('worker-default', 'openai', $1, $5, $3, $4, $8, $9) RETURNING id
                 ) SELECT p.id, w.id FROM p, w",
            )
            .bind(seed.base_url.trim())
            .bind(seed.planner_model.trim())
            .bind(seed.api_key.trim())
            .bind(hint)
            .bind(seed.worker_model.trim())
            .bind(&seed.planner_thinking_type)
            .bind(&seed.planner_reasoning_effort)
            .bind(&seed.worker_thinking_type)
            .bind(&seed.worker_reasoning_effort)
            .fetch_one(&mut *tx)
            .await?;
            planner_id = Some(pid);
            worker_id = Some(wid);
        }

        let settings_count: i64 = sqlx::query_scalar("SELECT count(*) FROM app_settings")
            .fetch_one(&mut *tx)
            .await?;
        if settings_count == 0 {
            // Fall back to any existing profile rows when the env seed did not
            // run (operator-managed database): lowest ids keep this stable.
            let (fallback_planner, fallback_worker): (Option<i64>, Option<i64>) = sqlx::query_as(
                "SELECT
                   (SELECT min(id) FROM llm_profiles),
                   (SELECT min(id) FROM llm_profiles)",
            )
            .fetch_one(&mut *tx)
            .await?;
            let planner = planner_id.or(fallback_planner);
            let worker = worker_id.or(fallback_worker);
            if let (Some(planner), Some(worker)) = (planner, worker) {
                sqlx::query(
                    "INSERT INTO app_settings
                       (id, planner_profile_id, worker_profile_id, approval_timeout_secs,
                        guard_default_action)
                     VALUES (1, $1, $2, $3, 'deny')",
                )
                .bind(planner)
                .bind(worker)
                .bind(seed.approval_timeout_secs)
                .execute(&mut *tx)
                .await?;
            }
        }

        let rule_count: i64 = sqlx::query_scalar("SELECT count(*) FROM guard_rules")
            .fetch_one(&mut *tx)
            .await?;
        if rule_count == 0 {
            for (tool, reason) in [
                ("mcp__chess__createGame", "start a new chess game"),
                ("mcp__chess__joinGame", "join an existing chess game"),
                (
                    "mcp__chess__waitForNextTurn",
                    "read the board after the opponent moves",
                ),
                ("mcp__chess__finishTurn", "submit a legal move"),
                // Web tools are read-only: search, read a page, research a
                // question, and the web_parse compatibility alias. Registered
                // only when the provider router is configured.
                ("web_search", "search the web through the provider router"),
                ("web_read", "read one web page or document as Markdown"),
                (
                    "web_research",
                    "research a question through Parallel and return cited sources",
                ),
                (
                    "web_parse",
                    "parse a PDF/DOCX document through the compatibility alias",
                ),
                // Coding tools run inside one Docker container per task; the
                // guard still records every call.
                ("read", "read a file in the task workspace"),
                ("write", "write a file in the task workspace"),
                ("edit", "edit a file in the task workspace"),
                ("search", "search files in the task workspace"),
                ("list", "list files in the task workspace"),
                ("shell", "run a command in the task container"),
                // Persistent terminals. One Python PTY proxy per session
                // inside the same task container; the guard still records
                // every call.
                ("shell_open", "open a persistent terminal"),
                ("shell_send", "send text to a persistent terminal"),
                ("shell_read", "read new output from a persistent terminal"),
                ("shell_close", "close a persistent terminal"),
                ("shell_list", "list persistent terminals"),
            ] {
                sqlx::query(
                    "INSERT INTO guard_rules
                       (name, match_target, match_type, pattern, action, reason, priority,
                        enabled, timeout_secs)
                     VALUES ($1, 'tool_name', 'exact', $2, 'allow', $3, 0, true, 0)",
                )
                .bind(format!("allow-{tool}"))
                .bind(tool)
                .bind(reason)
                .execute(&mut *tx)
                .await?;
            }
        }

        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_hint_is_last_four_characters() {
        assert_eq!(key_hint("sk-abcdef123456"), "…3456");
        assert_eq!(key_hint(""), "");
        assert_eq!(key_hint("abc"), "");
        assert_eq!(key_hint("abcd"), "…abcd");
        assert!(
            !key_hint("sk-abcdef123456").contains("sk-ab"),
            "hint never leaks the head"
        );
    }

    #[test]
    fn profile_full_debug_redacts_the_key() {
        let full = LlmProfileFull {
            id: 1,
            name: "planner-default".into(),
            format: "openai".into(),
            base_url: "https://api.river.ai/v1".into(),
            model: "river-planner".into(),
            api_key: "sk-live-secret-9999".into(),
            api_key_hint: "…9999".into(),
            max_tokens: 0,
            context_window_k: 0,
            streaming: true,
            priority: 0,
            pool_exclude: false,
            thinking_type: "enabled".into(),
            reasoning_effort: "high".into(),
        };
        let debug = format!("{full:?}");
        assert!(debug.contains("<redacted>"), "{debug}");
        assert!(!debug.contains("sk-live-secret-9999"), "{debug}");
    }

    #[test]
    fn upsert_and_seed_debug_redact_the_key() {
        let upsert = LlmProfileUpsert {
            id: None,
            name: "backup".into(),
            format: "openai".into(),
            base_url: "https://api.backup.ai/v1".into(),
            model: "river-xl".into(),
            api_key: "sk-upsert-secret-7777".into(),
            max_tokens: 0,
            context_window_k: 0,
            streaming: true,
            priority: 0,
            pool_exclude: false,
            thinking_type: String::new(),
            reasoning_effort: "medium".into(),
        };
        let upsert_debug = format!("{upsert:?}");
        assert!(upsert_debug.contains("<redacted>"), "{upsert_debug}");
        assert!(!upsert_debug.contains("sk-upsert-secret-7777"));

        let seed = BootstrapSeed {
            base_url: "https://api.river.ai/v1".into(),
            api_key: "sk-seed-secret-5555".into(),
            planner_model: "river-planner".into(),
            worker_model: "river-worker".into(),
            planner_thinking_type: "enabled".into(),
            planner_reasoning_effort: "high".into(),
            worker_thinking_type: String::new(),
            worker_reasoning_effort: "low".into(),
            approval_timeout_secs: DEFAULT_APPROVAL_TIMEOUT_SECS,
        };
        let seed_debug = format!("{seed:?}");
        assert!(seed_debug.contains("<redacted>"), "{seed_debug}");
        assert!(!seed_debug.contains("sk-seed-secret-5555"));
    }

    fn upsert_with_thinking(thinking_type: &str, reasoning_effort: &str) -> LlmProfileUpsert {
        LlmProfileUpsert {
            id: None,
            name: "thinking".into(),
            format: "openai".into(),
            base_url: "https://api.river.ai/v1".into(),
            model: "river".into(),
            api_key: "secret".into(),
            max_tokens: 0,
            context_window_k: 0,
            streaming: true,
            priority: 0,
            pool_exclude: false,
            thinking_type: thinking_type.into(),
            reasoning_effort: reasoning_effort.into(),
        }
    }

    #[test]
    fn profile_thinking_fields_accept_the_documented_enums() {
        for thinking_type in ["", "disabled", "enabled"] {
            upsert_with_thinking(thinking_type, "").validate().unwrap();
        }
        for effort in ["", "low", "medium", "high", "xhigh", "max"] {
            upsert_with_thinking("", effort).validate().unwrap();
        }
        // The two knobs are independent.
        upsert_with_thinking("disabled", "max").validate().unwrap();
    }

    #[test]
    fn profile_thinking_fields_reject_unknown_values() {
        for bad in ["on", "true", "DISABLED"] {
            assert!(matches!(
                upsert_with_thinking(bad, "").validate(),
                Err(StoreError::Validation(_))
            ));
        }
        for bad in ["minimal", "extreme", "HIGH"] {
            assert!(matches!(
                upsert_with_thinking("", bad).validate(),
                Err(StoreError::Validation(_))
            ));
        }
    }

    #[test]
    fn the_default_timeout_constant_matches_the_engine_binary() {
        // `main.rs` derives its TTL from APPROVAL_TTL_MINUTES = 15; both must
        // agree because the settings singleton seeds from one or the other.
        assert_eq!(DEFAULT_APPROVAL_TIMEOUT_SECS, 15 * 60);
    }
}
