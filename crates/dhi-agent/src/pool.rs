//! LLM failover pool: the shared runtime snapshot, per-profile breakers, and
//! the chain-resolution rules.
//!
//! ARTEX parity, reduced to DHI's two roles:
//! - the snapshot's role chain is the role's active profile first, then every
//!   other profile by priority DESC (id ASC for stability) that is not
//!   `pool_exclude`, de-duplicated;
//! - the active profile always heads its chain, `pool_exclude` or not;
//! - a task pin is honored even when the pinned profile is tripped or
//!   excluded: the pin heads the chain and failover continues down the role
//!   chain minus duplicates;
//! - a tripped profile is skipped by chain resolution until a reset; the pin
//!   exception above is deliberate (pin wins).
//!
//! Breaker state is in-memory only, like ARTEX's `llmHealth`: consecutive
//! failures trip a profile after [`POOL_TRIP_THRESHOLD`] misses, a success
//! resets it, and `POST /api/llm/pool/reset` clears it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use serde::{Deserialize, Serialize};

use crate::profile::Role;

/// Consecutive failures before a profile trips and is skipped by chain
/// resolution. A success resets the count; `POST /api/llm/pool/reset` clears
/// it.
pub const POOL_TRIP_THRESHOLD: u32 = 3;

/// Wire format of a resolved profile. Mirrors the `llm_profiles.format`
/// CHECK (`openai` | `anthropic`); anything else from the store is treated as
/// `openai`, keeping a drifted row servable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProfileFormat {
    Openai,
    Anthropic,
}

impl ProfileFormat {
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "anthropic" => Self::Anthropic,
            _ => Self::Openai,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::Anthropic => "anthropic",
        }
    }
}

/// One profile resolved for client construction, secrets included. Lives only
/// inside the snapshot and the completion call; never serialized to a route.
#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedProfile {
    pub id: i64,
    pub name: String,
    pub format: ProfileFormat,
    /// API base, e.g. `https://api.river.ai/v1`. Rig appends the route.
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    /// Output cap for one reply; 0 = no cap is sent (endpoint default).
    pub max_tokens: i64,
    pub streaming: bool,
    /// `''` (send nothing) | `disabled` | `enabled`.
    pub thinking_type: String,
    /// `''` (send nothing) | `low` | `medium` | `high` | `xhigh` | `max`.
    pub reasoning_effort: String,
}

/// The API key is a secret: `Debug` output carries the placeholder only.
impl std::fmt::Debug for ResolvedProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedProfile")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("format", &self.format)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &"<redacted>")
            .field("max_tokens", &self.max_tokens)
            .field("streaming", &self.streaming)
            .field("thinking_type", &self.thinking_type)
            .field("reasoning_effort", &self.reasoning_effort)
            .finish()
    }
}

impl ResolvedProfile {
    /// Secret-free fingerprint including the profile identity, recorded as
    /// `agent_runs.profile`. Synthetic profiles (id 0, no name — the
    /// env-built fallback) keep the bare fingerprint.
    pub fn tag(&self) -> String {
        let base = format!(
            "{}/{}/max_tokens={}/stream={}",
            self.format.as_str(),
            self.model,
            self.max_tokens.max(0),
            self.streaming
        );
        if self.id == 0 && self.name.is_empty() {
            base
        } else {
            format!("{base}|{}#{}", self.name, self.id)
        }
    }
}

/// One store row as the pool builder consumes it. Plain fields keep
/// `dhi-agent` free of a `dhi-store` dependency; the server maps rows.
#[derive(Clone)]
pub struct RawProfile {
    pub id: i64,
    pub name: String,
    pub format: String,
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub max_tokens: i32,
    pub streaming: bool,
    pub priority: i32,
    pub pool_exclude: bool,
    pub thinking_type: String,
    pub reasoning_effort: String,
}

/// The API key is a secret: `Debug` output carries the placeholder only.
impl std::fmt::Debug for RawProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawProfile")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("format", &self.format)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &"<redacted>")
            .field("max_tokens", &self.max_tokens)
            .field("streaming", &self.streaming)
            .field("priority", &self.priority)
            .field("pool_exclude", &self.pool_exclude)
            .field("thinking_type", &self.thinking_type)
            .field("reasoning_effort", &self.reasoning_effort)
            .finish()
    }
}

impl RawProfile {
    fn resolved(&self) -> ResolvedProfile {
        ResolvedProfile {
            id: self.id,
            name: self.name.clone(),
            format: ProfileFormat::parse(&self.format),
            base_url: self.base_url.trim().to_string(),
            model: self.model.trim().to_string(),
            api_key: self.api_key.clone(),
            max_tokens: i64::from(self.max_tokens),
            streaming: self.streaming,
            thinking_type: self.thinking_type.trim().to_ascii_lowercase(),
            reasoning_effort: self.reasoning_effort.trim().to_ascii_lowercase(),
        }
    }

    fn priority(&self) -> i32 {
        self.priority
    }
}

/// Immutable runtime snapshot: per-role chains plus every profile by id, so a
/// task pin resolves even when the pinned profile is `pool_exclude` (and thus
/// absent from the chains). `fallback` is the canonical priority-ordered
/// sequence the pinned-tail resolution walks (see [`LlmPool::chain`]).
#[derive(Debug, Clone, Default)]
pub struct PoolSnapshot {
    pub planner: Vec<ResolvedProfile>,
    pub worker: Vec<ResolvedProfile>,
    pub by_id: HashMap<i64, ResolvedProfile>,
    /// Every reachable profile, priority DESC / id ASC, exclusions removed.
    /// Empty for the synthetic single-profile snapshot, where pinning never
    /// reaches a fallback anyway.
    pub fallback: Vec<ResolvedProfile>,
}

impl PoolSnapshot {
    /// Builds both role chains from the store rows:
    /// active profile first (always, `pool_exclude` notwithstanding), then
    /// every other non-excluded profile by priority DESC / id ASC,
    /// de-duplicated. Both roles draw from the same role-eligible universe,
    /// exactly like ARTEX's provider pool.
    ///
    /// ARTEX parity (`db/config.go` PoolProfiles): a profile without an API
    /// key cannot serve a request, so it never occupies a chain tail or the
    /// canonical fallback — but it stays in `by_id`, so pinning to it still
    /// resolves (pin wins) and an active keyless default still heads its own
    /// chain.
    pub fn from_rows(rows: &[RawProfile], planner_active: i64, worker_active: i64) -> Self {
        let mut by_id: HashMap<i64, ResolvedProfile> = HashMap::new();
        let mut excluded: HashMap<i64, bool> = HashMap::new();
        let mut keyless: HashMap<i64, bool> = HashMap::new();
        for row in rows {
            let resolved = row.resolved();
            if resolved.model.is_empty() {
                continue;
            }
            by_id.entry(resolved.id).or_insert_with(|| resolved.clone());
            excluded.entry(resolved.id).or_insert(row.pool_exclude);
            keyless
                .entry(resolved.id)
                .or_insert_with(|| row.api_key.trim().is_empty());
        }
        let mut ordered: Vec<ResolvedProfile> = by_id.values().cloned().collect();
        let priority_of = |id: i64| -> i32 {
            rows.iter()
                .find(|row| row.id == id)
                .map(RawProfile::priority)
                .unwrap_or(0)
        };
        ordered.sort_by_key(|profile| (std::cmp::Reverse(priority_of(profile.id)), profile.id));

        let chain = |active: i64| -> Vec<ResolvedProfile> {
            let mut chain: Vec<ResolvedProfile> = Vec::new();
            if let Some(head) = by_id.get(&active) {
                chain.push(head.clone());
            }
            for profile in &ordered {
                if profile.id == active
                    || excluded.get(&profile.id).copied().unwrap_or(false)
                    || keyless.get(&profile.id).copied().unwrap_or(false)
                    || chain.iter().any(|seen| seen.id == profile.id)
                {
                    continue;
                }
                chain.push(profile.clone());
            }
            chain
        };

        Self {
            planner: chain(planner_active),
            worker: chain(worker_active),
            fallback: ordered
                .iter()
                .filter(|profile| {
                    !excluded.get(&profile.id).copied().unwrap_or(false)
                        && !keyless.get(&profile.id).copied().unwrap_or(false)
                })
                .cloned()
                .collect(),
            by_id,
        }
    }

    /// A snapshot with exactly one synthetic candidate per role (no ids, no
    /// breaker bookkeeping): the env-only runtime's pool.
    pub fn single(planner: ResolvedProfile, worker: ResolvedProfile) -> Self {
        Self {
            planner: vec![planner],
            worker: vec![worker],
            fallback: Vec::new(),
            by_id: HashMap::new(),
        }
    }

    /// The chain head's model for rendering, or `None` before any snapshot.
    pub fn head_model(&self, role: Role) -> Option<String> {
        let chain = match role {
            Role::Planner => &self.planner,
            Role::Worker => &self.worker,
        };
        chain.first().map(|profile| profile.model.clone())
    }
}

/// One entry of the pool-status chain view: identity plus breaker state,
/// secrets excluded.
#[derive(Debug, Clone, Serialize)]
pub struct ChainEntry {
    pub id: i64,
    pub name: String,
    pub model: String,
    pub tripped: bool,
    pub consecutive_failures: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct RolePoolStatus {
    pub chain: Vec<ChainEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PoolStatus {
    pub planner: RolePoolStatus,
    pub worker: RolePoolStatus,
}

#[derive(Debug, Clone, Default)]
struct Breaker {
    consecutive_failures: u32,
    tripped: bool,
}

/// The shared pool manager: the snapshot the runtime consults plus the
/// in-memory breaker bookkeeping. The server swaps the snapshot after every
/// profile/settings mutation; the engine reads it per agent call.
#[derive(Default)]
pub struct LlmPool {
    snapshot: RwLock<Arc<PoolSnapshot>>,
    breakers: Mutex<HashMap<i64, Breaker>>,
}

impl LlmPool {
    pub fn new(snapshot: PoolSnapshot) -> Arc<Self> {
        Arc::new(Self {
            snapshot: RwLock::new(Arc::new(snapshot)),
            breakers: Mutex::new(HashMap::new()),
        })
    }

    /// Publishes a rebuilt snapshot atomically. In-flight calls keep the
    /// snapshot they resolved; the next call sees the new chains.
    pub fn swap(&self, snapshot: PoolSnapshot) {
        if let Ok(mut guard) = self.snapshot.write() {
            *guard = Arc::new(snapshot);
        }
    }

    /// The current snapshot. A poisoned lock hands back the last good
    /// snapshot: pool resolution must not lose its chains.
    pub fn load(&self) -> Arc<PoolSnapshot> {
        match self.snapshot.read() {
            Ok(guard) => Arc::clone(&guard),
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        }
    }

    /// The completion candidates for one logical step, in order.
    ///
    /// Without a pin: the role chain minus tripped profiles. With a pin: the
    /// pinned profile heads the chain regardless of trip or exclusion state
    /// (pin wins), followed by the canonical priority-ordered fallback minus
    /// the pin and minus tripped profiles. An unknown pin id degrades to the
    /// role chain.
    pub fn chain(&self, role: Role, pinned: Option<i64>) -> Vec<ResolvedProfile> {
        let snapshot = self.load();
        let role_chain = match role {
            Role::Planner => &snapshot.planner,
            Role::Worker => &snapshot.worker,
        };
        match pinned {
            Some(pin) => {
                let mut chain = Vec::new();
                if let Some(head) = snapshot.by_id.get(&pin) {
                    chain.push(head.clone());
                }
                let tail = if snapshot.fallback.is_empty() {
                    role_chain
                } else {
                    &snapshot.fallback
                };
                for profile in tail {
                    if profile.id != pin && !self.is_tripped(profile.id) {
                        chain.push(profile.clone());
                    }
                }
                chain
            }
            None => role_chain
                .iter()
                .filter(|profile| !self.is_tripped(profile.id))
                .cloned()
                .collect(),
        }
    }

    /// Whether the profile currently trips the chain resolution.
    pub fn is_tripped(&self, id: i64) -> bool {
        self.breakers
            .lock()
            .map(|breakers| breakers.get(&id).is_some_and(|breaker| breaker.tripped))
            .unwrap_or(false)
    }

    /// A completed call: resets the consecutive-failure count and clears a
    /// trip.
    pub fn record_success(&self, id: i64) {
        if let Ok(mut breakers) = self.breakers.lock() {
            breakers.insert(id, Breaker::default());
        }
    }

    /// A failed call. Returns whether this failure tripped the profile.
    pub fn record_failure(&self, id: i64) -> bool {
        let Ok(mut breakers) = self.breakers.lock() else {
            return false;
        };
        let breaker = breakers.entry(id).or_default();
        breaker.consecutive_failures = breaker.consecutive_failures.saturating_add(1);
        if breaker.consecutive_failures >= POOL_TRIP_THRESHOLD && !breaker.tripped {
            breaker.tripped = true;
            return true;
        }
        false
    }

    /// Clears breaker state for one profile, or for every profile when
    /// `id` is `None` (ARTEX id=0 semantics).
    pub fn reset(&self, id: Option<i64>) {
        let Ok(mut breakers) = self.breakers.lock() else {
            return;
        };
        match id {
            Some(id) => {
                breakers.remove(&id);
            }
            None => breakers.clear(),
        }
    }

    /// The status view both pool routes return: chain order per role with
    /// each profile's breaker state.
    pub fn status(&self) -> PoolStatus {
        let snapshot = self.load();
        let view = |chain: &[ResolvedProfile]| RolePoolStatus {
            chain: chain
                .iter()
                .map(|profile| {
                    let (tripped, consecutive_failures) = {
                        let guard = self.breakers.lock().ok();
                        match guard
                            .as_deref()
                            .and_then(|breakers| breakers.get(&profile.id))
                        {
                            Some(breaker) => (breaker.tripped, breaker.consecutive_failures),
                            None => (false, 0),
                        }
                    };
                    ChainEntry {
                        id: profile.id,
                        name: profile.name.clone(),
                        model: profile.model.clone(),
                        tripped,
                        consecutive_failures,
                    }
                })
                .collect(),
        };
        PoolStatus {
            planner: view(&snapshot.planner),
            worker: view(&snapshot.worker),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(id: i64, name: &str, model: &str, priority: i32, pool_exclude: bool) -> RawProfile {
        RawProfile {
            id,
            name: name.to_string(),
            format: "openai".to_string(),
            base_url: "https://gw.example/v1".to_string(),
            model: model.to_string(),
            api_key: "key".to_string(),
            max_tokens: 0,
            streaming: true,
            priority,
            pool_exclude,
            thinking_type: String::new(),
            reasoning_effort: String::new(),
        }
    }

    #[test]
    fn chain_orders_default_first_then_priority_desc_without_excluded() {
        let rows = [
            raw(1, "planner-default", "p1", 0, false),
            raw(2, "fast", "p2", 100, false),
            raw(3, "slow", "p3", 10, false),
            raw(4, "boxed", "p4", 1000, true),
        ];
        let snapshot = PoolSnapshot::from_rows(&rows, 1, 1);
        let planner: Vec<i64> = snapshot.planner.iter().map(|p| p.id).collect();
        assert_eq!(
            planner,
            vec![1, 2, 3],
            "active head, then priority DESC, excluded row skipped"
        );
        // Role-eligible means both roles draw from the same universe.
        let worker: Vec<i64> = snapshot.worker.iter().map(|p| p.id).collect();
        assert_eq!(worker, vec![1, 2, 3]);
    }

    #[test]
    fn chain_is_deduplicated_and_tie_breaks_by_id() {
        let rows = [
            raw(7, "tie-b", "m7", 50, false),
            raw(3, "tie-a", "m3", 50, false),
        ];
        let snapshot = PoolSnapshot::from_rows(&rows, 3, 3);
        let planner: Vec<i64> = snapshot.planner.iter().map(|p| p.id).collect();
        assert_eq!(
            planner,
            vec![3, 7],
            "equal priority breaks to lower id; ids dedup"
        );
    }

    #[test]
    fn active_profile_heads_the_chain_even_when_excluded() {
        let rows = [
            raw(1, "boxed-default", "p1", 0, true),
            raw(2, "open", "p2", 5, false),
        ];
        let snapshot = PoolSnapshot::from_rows(&rows, 1, 1);
        assert_eq!(
            snapshot.planner[0].id, 1,
            "active head, pool_exclude notwithstanding"
        );
        assert_eq!(snapshot.planner.len(), 2, "…followed by the open profile");
    }

    #[test]
    fn snapshot_keeps_every_profile_by_id_for_pins() {
        let rows = [raw(1, "a", "m1", 0, false), raw(9, "boxed", "m9", 0, true)];
        let snapshot = PoolSnapshot::from_rows(&rows, 1, 1);
        assert!(
            snapshot.by_id.contains_key(&9),
            "pins must reach excluded profiles"
        );
        assert!(!snapshot.planner.iter().any(|p| p.id == 9));
    }

    /// ARTEX parity (`db/config.go` PoolProfiles): a profile without an API
    /// key cannot serve a request, so it is absent from every chain tail and
    /// the fallback — but stays in `by_id`, and a pin still resolves it (pin
    /// wins) with the canonical fallback tail behind it.
    #[test]
    fn keyless_profiles_are_barred_from_chains_but_resolvable_by_pin() {
        let mut rows = [
            raw(1, "a", "m1", 0, false),
            raw(2, "keyless", "m2", 100, false),
            raw(3, "c", "m3", 10, false),
        ];
        rows[1].api_key = "   ".into();
        let snapshot = PoolSnapshot::from_rows(&rows, 1, 1);

        let planner: Vec<i64> = snapshot.planner.iter().map(|p| p.id).collect();
        assert_eq!(planner, vec![1, 3], "the keyless tail member is omitted");
        let worker: Vec<i64> = snapshot.worker.iter().map(|p| p.id).collect();
        assert_eq!(worker, vec![1, 3]);
        let fallback: Vec<i64> = snapshot.fallback.iter().map(|p| p.id).collect();
        assert_eq!(
            fallback,
            vec![3, 1],
            "the keyless profile leaves the priority-ordered fallback"
        );
        assert!(
            snapshot.by_id.contains_key(&2),
            "by_id keeps the keyless profile so pins still resolve"
        );

        let pool = LlmPool::new(snapshot);
        let pinned: Vec<i64> = pool
            .chain(Role::Planner, Some(2))
            .iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(
            pinned,
            vec![2, 3, 1],
            "pinning the keyless profile still heads the chain, canonical tail behind"
        );
    }

    #[test]
    fn resolved_and_raw_debug_redact_the_key() {
        let mut row = raw(5, "secret-holder", "m5", 0, false);
        row.api_key = "sk-pool-secret-4321".into();
        let resolved = row.resolved();

        let resolved_debug = format!("{resolved:?}");
        assert!(resolved_debug.contains("<redacted>"), "{resolved_debug}");
        assert!(
            !resolved_debug.contains("sk-pool-secret-4321"),
            "{resolved_debug}"
        );

        let row_debug = format!("{row:?}");
        assert!(row_debug.contains("<redacted>"), "{row_debug}");
        assert!(!row_debug.contains("sk-pool-secret-4321"), "{row_debug}");
    }

    #[test]
    fn breaker_trips_at_threshold_and_reset_clears_it() {
        let pool = LlmPool::new(PoolSnapshot {
            planner: vec![raw(1, "a", "m1", 0, false).resolved()],
            worker: vec![raw(2, "b", "m2", 0, false).resolved()],
            ..PoolSnapshot::default()
        });
        assert!(!pool.is_tripped(1));
        assert!(!pool.record_failure(1));
        assert!(!pool.record_failure(1));
        assert!(pool.record_failure(1), "third consecutive failure trips");
        assert!(pool.is_tripped(1));
        assert_eq!(
            pool.chain(Role::Planner, None).len(),
            0,
            "a tripped chain head is skipped by resolution"
        );

        pool.record_success(1);
        assert!(!pool.is_tripped(1), "success resets the breaker");
        assert_eq!(pool.chain(Role::Planner, None).len(), 1);

        for _ in 0..POOL_TRIP_THRESHOLD {
            pool.record_failure(1);
        }
        assert!(pool.is_tripped(1));
        pool.reset(Some(1));
        assert!(!pool.is_tripped(1), "explicit reset clears one profile");
        for _ in 0..POOL_TRIP_THRESHOLD {
            pool.record_failure(1);
        }
        pool.reset(None);
        assert!(!pool.is_tripped(1), "global reset clears every profile");
    }

    #[test]
    fn pin_heads_the_chain_and_survives_trip_and_exclusion() {
        let rows = [
            raw(1, "a", "m1", 0, false),
            raw(2, "b", "m2", 100, false),
            raw(9, "boxed", "m9", 1000, true),
        ];
        let snapshot = PoolSnapshot::from_rows(&rows, 1, 1);
        let pool = LlmPool::new(snapshot);

        // An excluded profile still resolves when pinned, ahead of the chain.
        let pinned: Vec<i64> = pool
            .chain(Role::Planner, Some(9))
            .iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(pinned, vec![9, 2, 1]);

        // A tripped profile is demoted, not skipped, when pinned.
        for _ in 0..POOL_TRIP_THRESHOLD {
            pool.record_failure(2);
        }
        let pinned_tripped: Vec<i64> = pool
            .chain(Role::Planner, Some(2))
            .iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(pinned_tripped, vec![2, 1], "pin wins over trip");

        // Unpinned resolution skips the tripped profile.
        let unpinned: Vec<i64> = pool
            .chain(Role::Planner, None)
            .iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(unpinned, vec![1]);
    }

    #[test]
    fn swapped_snapshot_governs_new_chains() {
        let pool = LlmPool::new(PoolSnapshot::from_rows(
            &[raw(1, "a", "m1", 0, false)],
            1,
            1,
        ));
        assert_eq!(pool.status().planner.chain.len(), 1);
        pool.swap(PoolSnapshot::from_rows(
            &[raw(1, "a", "m1", 0, false), raw(2, "b", "m2", 100, false)],
            2,
            2,
        ));
        let status = pool.status();
        assert_eq!(status.planner.chain.len(), 2);
        assert_eq!(status.planner.chain[0].id, 2, "activation swaps the head");
    }

    #[test]
    fn status_carries_no_secrets_and_reports_breakers() {
        let rows = [raw(1, "secret-holder", "m1", 0, false)];
        let pool = LlmPool::new(PoolSnapshot::from_rows(&rows, 1, 1));
        pool.record_failure(1);
        pool.record_failure(1);
        let json = serde_json::to_string(&pool.status()).unwrap();
        assert!(
            !json.contains("\"key\""),
            "the status view must not leak credentials"
        );
        let status = pool.status();
        assert_eq!(status.planner.chain[0].consecutive_failures, 2);
        assert!(!status.planner.chain[0].tripped);
    }

    #[test]
    fn resolved_tag_names_profile_without_secrets() {
        let profile = raw(3, "worker-pool", "glm", 0, false).resolved();
        let tag = profile.tag();
        assert_eq!(tag, "openai/glm/max_tokens=0/stream=true|worker-pool#3");
        assert!(!tag.contains("\"key\""));
        let synthetic = ResolvedProfile {
            id: 0,
            name: String::new(),
            ..raw(0, "", "glm", 0, false).resolved()
        };
        assert_eq!(
            synthetic.tag(),
            "openai/glm/max_tokens=0/stream=true",
            "env-built fallback keeps the bare fingerprint"
        );
    }
}
