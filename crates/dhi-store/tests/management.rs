//! Management-layer integration tests: LLM profiles, guard rules, app
//! settings, task pins, and the snapshot pipelines the server feeds from
//! them.
//!
//! Every `#[sqlx::test]` runs against its own freshly created database, so
//! seeding state never leaks between tests. The server-facing mapping
//! (`LlmProfileFull` → `RawProfile`, enabled rules → `RuleSpec`) is replicated
//! here exactly as `dhi-server` performs it, so these tests double as proofs
//! that the store returns rows in the order the snapshots expect.

use chrono::Duration;
use dhi_agent::{
    LlmPool, POOL_TRIP_THRESHOLD, PoolSnapshot, ProfileFormat, RawProfile, ResolvedProfile, Role,
};
use dhi_core::GuardVerdict;
use dhi_guard::{CompiledPolicy, RuleSpec};
use dhi_store::mgmt::{
    AppSettingsRow, BootstrapSeed, GuardRuleInput, LlmProfileUpsert, SettingPatch,
};
use dhi_store::{Store, StoreError};
use sqlx::PgPool;

// ---------- fixtures ----------

fn seed() -> BootstrapSeed {
    BootstrapSeed {
        base_url: "https://api.river.ai/v1".into(),
        api_key: "sk-env-secret-9999".into(),
        planner_model: "river-planner-large".into(),
        worker_model: "river-worker-small".into(),
        planner_thinking_type: "enabled".into(),
        planner_reasoning_effort: "high".into(),
        worker_thinking_type: String::new(),
        worker_reasoning_effort: "low".into(),
        approval_timeout_secs: 900,
    }
}

fn profile(name: &str, model: &str, priority: i32, exclude: bool) -> LlmProfileUpsert {
    LlmProfileUpsert {
        id: None,
        name: name.into(),
        format: "openai".into(),
        base_url: "https://api.river.ai/v1".into(),
        model: model.into(),
        api_key: format!("sk-{name}-secret-1234"),
        max_tokens: 0,
        context_window_k: 128,
        streaming: true,
        priority,
        pool_exclude: exclude,
        thinking_type: String::new(),
        reasoning_effort: String::new(),
    }
}

fn rule(name: &str, tool: &str, action: &str, priority: i32) -> GuardRuleInput {
    GuardRuleInput {
        name: name.into(),
        match_target: "tool_name".into(),
        match_type: "exact".into(),
        pattern: tool.into(),
        action: action.into(),
        reason: format!("test rule {name}"),
        priority,
        enabled: true,
        timeout_secs: 0,
    }
}

/// The store rows the pool builder consumes, exactly as `dhi-server` maps
/// them.
async fn raw_profiles(store: &Store) -> Vec<RawProfile> {
    store
        .list_profiles_with_keys()
        .await
        .unwrap()
        .into_iter()
        .map(|p| RawProfile {
            id: p.id,
            name: p.name,
            format: p.format,
            base_url: p.base_url,
            model: p.model,
            api_key: p.api_key,
            max_tokens: p.max_tokens,
            streaming: p.streaming,
            priority: p.priority,
            pool_exclude: p.pool_exclude,
            thinking_type: p.thinking_type,
            reasoning_effort: p.reasoning_effort,
        })
        .collect()
}

/// The compiled policy from the current rows, exactly as `dhi-server`
/// rebuilds it after every mutation.
async fn compiled_from_db(store: &Store) -> CompiledPolicy {
    let rules = store.list_enabled_rules().await.unwrap();
    let settings = store.get_settings().await.unwrap();
    let specs: Vec<RuleSpec> = rules
        .iter()
        .map(|rule| RuleSpec {
            name: rule.name.clone(),
            match_target: rule.match_target.clone(),
            match_type: rule.match_type.clone(),
            pattern: rule.pattern.clone(),
            action: match rule.action.as_str() {
                "allow" => GuardVerdict::Allow,
                "ask" => GuardVerdict::Ask,
                _ => GuardVerdict::Deny,
            },
            reason: rule.reason.clone(),
            priority: rule.priority,
            timeout_secs: i64::from(rule.timeout_secs),
        })
        .collect();
    let (default, timeout) = settings
        .map(|s| {
            (
                match s.guard_default_action.as_str() {
                    "allow" => GuardVerdict::Allow,
                    "ask" => GuardVerdict::Ask,
                    _ => GuardVerdict::Deny,
                },
                i64::from(s.approval_timeout_secs),
            )
        })
        .unwrap_or((GuardVerdict::Deny, 0));
    CompiledPolicy::build("db-rules-v1", default, timeout, &specs)
}

// ---------- 10. seeding ----------

/// First boot seeds two profiles, the settings row, and the nineteen compiled
/// allow rules (four chess MCP tools, four web tools, six coding tools, five
/// terminal tools); a second boot is a no-op even with different env values,
/// so operator edits survive restarts.
#[sqlx::test]
async fn seed_defaults_idempotent_across_boots(pool: PgPool) {
    let store = Store::new(pool);
    store.seed_defaults(&seed()).await.unwrap();

    let profiles = store.list_profiles().await.unwrap();
    assert_eq!(profiles.len(), 2, "one default profile per role");
    let names: Vec<&str> = profiles.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["planner-default", "worker-default"]);
    assert_eq!(profiles[0].model, "river-planner-large");
    assert_eq!(profiles[1].model, "river-worker-small");
    // Seeded profiles sit at priority 0 and stream.
    assert!(profiles.iter().all(|p| p.priority == 0 && p.streaming));
    // The env-resolved thinking controls travel into the seeded rows.
    assert_eq!(profiles[0].thinking_type, "enabled");
    assert_eq!(profiles[0].reasoning_effort, "high");
    assert_eq!(profiles[1].thinking_type, "");
    assert_eq!(profiles[1].reasoning_effort, "low");

    let settings = store
        .get_settings()
        .await
        .unwrap()
        .expect("settings row seeded");
    assert_eq!(settings.planner_profile_id, profiles[0].id);
    assert_eq!(settings.worker_profile_id, profiles[1].id);
    assert_eq!(settings.approval_timeout_secs, 900);
    assert_eq!(settings.guard_default_action, "deny");

    let rules = store.list_rules().await.unwrap();
    assert_eq!(
        rules.len(),
        19,
        "the nineteen compiled allow rules seed (chess + web + coding + terminal tools)"
    );
    assert!(
        rules
            .iter()
            .all(|r| r.enabled && r.priority == 0 && r.timeout_secs == 0)
    );
    let seeded_names: Vec<&str> = rules.iter().map(|r| r.pattern.as_str()).collect();
    for tool in [
        "mcp__chess__createGame",
        "mcp__chess__joinGame",
        "mcp__chess__waitForNextTurn",
        "mcp__chess__finishTurn",
        "web_search",
        "web_read",
        "web_research",
        "web_parse",
        "read",
        "write",
        "edit",
        "search",
        "list",
        "shell",
        "shell_open",
        "shell_send",
        "shell_read",
        "shell_close",
        "shell_list",
    ] {
        assert!(
            seeded_names.contains(&tool),
            "{tool} seeds: {seeded_names:?}"
        );
    }

    // Second boot with different env: nothing changes.
    let changed = BootstrapSeed {
        base_url: "https://elsewhere.example/v1".into(),
        api_key: "sk-other-key".into(),
        planner_model: "other-model".into(),
        worker_model: "other-worker".into(),
        planner_thinking_type: "disabled".into(),
        planner_reasoning_effort: String::new(),
        worker_thinking_type: "enabled".into(),
        worker_reasoning_effort: "max".into(),
        approval_timeout_secs: 60,
    };
    store.seed_defaults(&changed).await.unwrap();
    assert_eq!(store.list_profiles().await.unwrap().len(), 2);
    let again = store.get_settings().await.unwrap().unwrap();
    assert_eq!(again.approval_timeout_secs, 900, "seed never overwrites");
    assert_eq!(again.planner_profile_id, settings.planner_profile_id);
    assert_eq!(store.list_rules().await.unwrap().len(), 19);
}

/// A mock-mode boot (empty env) seeds the rules only: profiles and settings
/// stay unset, so the management routes degrade honestly instead of pointing
/// at profiles that do not exist.
#[sqlx::test]
async fn mock_boot_seeds_rules_only(pool: PgPool) {
    let store = Store::new(pool);
    store
        .seed_defaults(&BootstrapSeed {
            base_url: String::new(),
            api_key: String::new(),
            planner_model: String::new(),
            worker_model: String::new(),
            planner_thinking_type: String::new(),
            planner_reasoning_effort: String::new(),
            worker_thinking_type: String::new(),
            worker_reasoning_effort: String::new(),
            approval_timeout_secs: 900,
        })
        .await
        .unwrap();

    assert!(store.list_profiles().await.unwrap().is_empty());
    assert!(store.get_settings().await.unwrap().is_none());
    assert_eq!(store.list_rules().await.unwrap().len(), 19);
    // A later proper boot still initializes everything it can.
    store.seed_defaults(&seed()).await.unwrap();
    assert_eq!(store.list_profiles().await.unwrap().len(), 2);
    assert!(store.get_settings().await.unwrap().is_some());
}

// ---------- 1 + 2. api_key masking, save roundtrip, validation ----------

/// `api_key` is write-only: reads return the row without the key and with the
/// `…tail` hint; only the client-construction path sees the secret. Saving a
/// profile round-trips every operator-editable field.
#[sqlx::test]
async fn api_key_is_write_only_hint_roundtrips(pool: PgPool) {
    let store = Store::new(pool);
    store.seed_defaults(&seed()).await.unwrap();

    let id = store
        .save_profile(&profile("backup", "river-worker-xl", 500, false))
        .await
        .unwrap();

    for out in store.list_profiles().await.unwrap() {
        // Structurally impossible to leak: `LlmProfileOut` has no key field.
        let hint = if out.id == id { "…1234" } else { "…9999" };
        assert_eq!(out.api_key_hint, hint, "hint is the key's last four");
    }
    let saved = store
        .list_profiles()
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.id == id)
        .expect("saved row listed");
    assert_eq!(saved.name, "backup");
    assert_eq!(saved.model, "river-worker-xl");
    assert_eq!(saved.priority, 500);
    assert!(!saved.pool_exclude);
    assert!(saved.streaming);
    assert_eq!(saved.context_window_k, 128);

    // The write-side view carries the secret for client construction only.
    let full = store.get_profile_with_key(id).await.unwrap().unwrap();
    assert_eq!(full.api_key, "sk-backup-secret-1234");

    // Updating with an empty key keeps the stored key and hint.
    let mut edit = profile("backup", "river-worker-xxl", 500, false);
    edit.id = Some(id);
    edit.api_key = String::new();
    store.save_profile(&edit).await.unwrap();
    let reloaded = store.get_profile_with_key(id).await.unwrap().unwrap();
    assert_eq!(reloaded.model, "river-worker-xxl");
    assert_eq!(reloaded.api_key, "sk-backup-secret-1234", "key survives");
    assert_eq!(reloaded.api_key_hint, "…1234");
}

/// The thinking controls round-trip through every read shape and default to
/// the empty string when the caller omits them.
#[sqlx::test]
async fn profile_thinking_controls_roundtrip(pool: PgPool) {
    let store = Store::new(pool);
    let mut upsert = profile("thinking", "river-thinking", 0, false);
    upsert.thinking_type = "enabled".into();
    upsert.reasoning_effort = "xhigh".into();
    let id = store.save_profile(&upsert).await.unwrap();

    let out = store
        .list_profiles()
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.id == id)
        .expect("saved row listed");
    assert_eq!(out.thinking_type, "enabled");
    assert_eq!(out.reasoning_effort, "xhigh");

    let full = store.get_profile_with_key(id).await.unwrap().unwrap();
    assert_eq!(full.thinking_type, "enabled");
    assert_eq!(full.reasoning_effort, "xhigh");

    // A save that sends nothing clears both knobs: empty means "send no
    // thinking settings", not "inherit the old ones".
    let mut plain = profile("thinking", "river-thinking", 0, false);
    plain.id = Some(id);
    plain.api_key = String::new();
    store.save_profile(&plain).await.unwrap();
    let reloaded = store.get_profile_with_key(id).await.unwrap().unwrap();
    assert_eq!(reloaded.thinking_type, "");
    assert_eq!(reloaded.reasoning_effort, "");
}

/// Upserts reject malformed requests (`400` class) and clamp a negative
/// `max_tokens` to "no cap"; updating an unknown id is `404`.
#[sqlx::test]
async fn save_profile_upsert_roundtrip_and_validation(pool: PgPool) {
    let store = Store::new(pool);

    let mut ok = profile("main", "river-planner-large", 0, false);
    ok.max_tokens = -5;
    let id = store.save_profile(&ok).await.unwrap();
    assert_eq!(
        store
            .get_profile_with_key(id)
            .await
            .unwrap()
            .unwrap()
            .max_tokens,
        0,
        "negative max_tokens clamps to 0 (no cap)"
    );

    let mut unnamed = profile("  ", "m", 0, false);
    assert!(matches!(
        store.save_profile(&unnamed).await,
        Err(StoreError::Validation(_))
    ));
    unnamed.name = "bad-format".into();
    unnamed.format = "ollama".into();
    assert!(matches!(
        store.save_profile(&unnamed).await,
        Err(StoreError::Validation(_))
    ));
    unnamed.format = "anthropic".into();
    unnamed.model = " ".into();
    assert!(matches!(
        store.save_profile(&unnamed).await,
        Err(StoreError::Validation(_))
    ));

    let mut ghost = profile("ghost", "m", 0, false);
    ghost.id = Some(424_242);
    assert!(matches!(
        store.save_profile(&ghost).await,
        Err(StoreError::NotFound(_))
    ));
    // The id swap really edits: unknown update leaves the table untouched.
    assert_eq!(store.list_profiles().await.unwrap().len(), 1);
}

// ---------- 7. delete guards ----------

/// Deleting an unknown profile is `404`; deleting an active role default or a
/// task-pinned profile is `409` (`InUse`); a free profile deletes.
#[sqlx::test]
async fn profile_delete_guards_404_conflict(pool: PgPool) {
    let store = Store::new(pool);
    store.seed_defaults(&seed()).await.unwrap();
    let spare = store
        .save_profile(&profile("spare", "river-worker-m", 10, false))
        .await
        .unwrap();

    let settings = store.get_settings().await.unwrap().unwrap();
    assert!(matches!(
        store.delete_profile(424_242).await,
        Err(StoreError::NotFound(_))
    ));
    assert!(matches!(
        store.delete_profile(settings.planner_profile_id).await,
        Err(StoreError::InUse(_)),
    ));
    assert!(matches!(
        store.delete_profile(settings.worker_profile_id).await,
        Err(StoreError::InUse(_)),
    ));
    // A task pin also protects a non-active profile.
    let task = store
        .create_task_with_pins("pinned", Some(spare), None, None, None)
        .await
        .unwrap();
    assert!(matches!(
        store.delete_profile(spare).await,
        Err(StoreError::InUse(_))
    ));
    // Free profile: deletes.
    let transient = store
        .save_profile(&profile("transient", "m", 0, false))
        .await
        .unwrap();
    store.delete_profile(transient).await.unwrap();
    assert!(!store.profile_exists(transient).await.unwrap());
    let _ = task;
}

// ---------- 6. task pins ----------

/// Pins persist on the task row, and a pin resolves a `pool_exclude` profile
/// the failover chains never mention.
#[sqlx::test]
async fn task_pins_persist_and_resolve_through_pool(pool: PgPool) {
    let store = Store::new(pool);
    store.seed_defaults(&seed()).await.unwrap();
    let boxed = store
        .save_profile(&profile("boxed", "river-boxed", 10_000, true))
        .await
        .unwrap();
    let settings = store.get_settings().await.unwrap().unwrap();

    let task = store
        .create_task_with_pins(
            "mate in two",
            Some(boxed),
            Some(settings.worker_profile_id),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(task.pinned_planner_profile_id, Some(boxed));
    assert_eq!(
        task.pinned_worker_profile_id,
        Some(settings.worker_profile_id)
    );

    let persisted = store.get_task(task.id).await.unwrap().unwrap();
    assert_eq!(persisted.pinned_planner_profile_id, Some(boxed));
    assert_eq!(
        persisted.pinned_worker_profile_id,
        Some(settings.worker_profile_id)
    );

    let snapshot = PoolSnapshot::from_rows(
        &raw_profiles(&store).await,
        settings.planner_profile_id,
        settings.worker_profile_id,
    );
    assert!(
        !snapshot.planner.iter().any(|p| p.id == boxed),
        "an excluded profile never appears in the failover chain"
    );
    let pinned: Vec<i64> = snapshot.by_id.keys().copied().collect();
    assert!(pinned.contains(&boxed), "pins resolve through by_id");

    let pool = LlmPool::new(snapshot);
    let head = pool.chain(Role::Planner, Some(boxed)).remove(0);
    assert_eq!(head.id, boxed, "the pin wins the chain head");
    assert_eq!(
        head.tag(),
        format!("openai/river-boxed/max_tokens=0/stream=true|boxed#{boxed}")
    );
}

// ---------- 4. chain order ----------

/// Chains are: the role's active default first (even when `pool_exclude`),
/// then every other reachable profile by priority DESC / id ASC, exclusions
/// dropped. With a pin, the tail is the canonical priority order.
#[sqlx::test]
async fn pool_chain_order_active_first_priority_desc_excludes(pool: PgPool) {
    let store = Store::new(pool);
    store.seed_defaults(&seed()).await.unwrap();

    let original = store.get_settings().await.unwrap().unwrap();
    let old_worker = original.worker_profile_id;

    // Sculpt the pool: a very high priority, a tie pair, an excluded profile,
    // and a negative-priority caboose.
    let low = store
        .save_profile(&profile("low", "m-low", -5, false))
        .await
        .unwrap();
    let high = store
        .save_profile(&profile("high", "m-high", 100, false))
        .await
        .unwrap();
    let boxed = store
        .save_profile(&profile("boxed", "m-boxed", 1_000, true))
        .await
        .unwrap();
    let tie_a = store
        .save_profile(&profile("tie-a", "m-tie", 50, false))
        .await
        .unwrap();
    let tie_b = store
        .save_profile(&profile("tie-b", "m-tie", 50, false))
        .await
        .unwrap();
    assert!(tie_a < tie_b, "ties break by id ASC (insertion order)");

    store.set_active_profile("worker", high).await.unwrap();
    let settings = store.get_settings().await.unwrap().unwrap();

    let snapshot = PoolSnapshot::from_rows(
        &raw_profiles(&store).await,
        settings.planner_profile_id,
        settings.worker_profile_id,
    );

    let worker_ids: Vec<i64> = snapshot.worker.iter().map(|p| p.id).collect();
    let planner_ids: Vec<i64> = snapshot.planner.iter().map(|p| p.id).collect();

    // Worker chain: active head, then priority DESC (ties by id ASC).
    // The excluded profile is absent despite owning the highest priority.
    let expected_tail = [tie_a, tie_b, settings.planner_profile_id, old_worker, low];
    assert_eq!(
        worker_ids,
        [vec![high], expected_tail.to_vec()].concat(),
        "worker chain is active head then priority DESC with ties by id ASC"
    );
    assert!(!worker_ids.contains(&boxed));
    assert!(
        !planner_ids.contains(&boxed),
        "exclusions are role-agnostic"
    );

    // Planner chain: planner default heads, then priority DESC; the worker's
    // active profile re-joins the tail where its priority places it.
    assert_eq!(
        planner_ids,
        [
            vec![settings.planner_profile_id],
            vec![high],
            vec![tie_a, tie_b, old_worker, low]
        ]
        .concat()
    );
    assert_eq!(
        worker_ids,
        [
            vec![high],
            vec![tie_a, tie_b, settings.planner_profile_id, old_worker, low]
        ]
        .concat()
    );

    // Pinned resolution: pinned head, then the canonical priority order
    // (including the worker's active profile, exclusions gone, pin removed).
    let pool = LlmPool::new(snapshot);
    let pinned: Vec<i64> = pool
        .chain(Role::Planner, Some(low))
        .iter()
        .map(|p| p.id)
        .collect();
    assert_eq!(
        pinned,
        [
            vec![low, high],
            vec![tie_a, tie_b, settings.planner_profile_id, old_worker]
        ]
        .concat(),
        "pin heads; the tail follows canonical priority order"
    );
}

// ---------- 5. breaker ----------

/// Through a store-fed pool: the third consecutive failure trips, resolution
/// skips the tripped profile, and a pin still forces it first.
#[sqlx::test]
async fn breaker_trips_third_failure_pin_wins(pool: PgPool) {
    let store = Store::new(pool);
    store.seed_defaults(&seed()).await.unwrap();
    let backup = store
        .save_profile(&profile("backup", "m-backup", 10, false))
        .await
        .unwrap();
    let settings = store.get_settings().await.unwrap().unwrap();
    let planner = settings.planner_profile_id;

    let snapshot = PoolSnapshot::from_rows(
        &raw_profiles(&store).await,
        planner,
        settings.worker_profile_id,
    );
    let pool = LlmPool::new(snapshot);

    let worker = settings.worker_profile_id;
    let plain: Vec<i64> = pool
        .chain(Role::Planner, None)
        .iter()
        .map(|p| p.id)
        .collect();
    assert_eq!(plain, vec![planner, backup, worker]);

    for _ in 1..POOL_TRIP_THRESHOLD {
        assert!(!pool.record_failure(planner));
    }
    assert!(pool.record_failure(planner), "the third failure trips");
    assert!(pool.is_tripped(planner));

    let demoted: Vec<i64> = pool
        .chain(Role::Planner, None)
        .iter()
        .map(|p| p.id)
        .collect();
    assert_eq!(
        demoted,
        vec![backup, worker],
        "unpinned resolution skips the trip"
    );

    let pinned: Vec<i64> = pool
        .chain(Role::Planner, Some(planner))
        .iter()
        .map(|p| p.id)
        .collect();
    assert_eq!(
        pinned,
        vec![planner, backup, worker],
        "pin wins over the trip"
    );

    pool.record_success(planner);
    assert!(!pool.is_tripped(planner), "success resets the breaker");
    assert_eq!(
        pool.chain(Role::Planner, None)
            .iter()
            .map(|p| p.id)
            .collect::<Vec<i64>>(),
        vec![planner, backup, worker]
    );
}

// ---------- 3. hot rebuild ----------

/// Rule mutations change what the rebuilt snapshot decides, without any
/// process restart: create denies, disabling falls back to the settings
/// default, and re-enabling restores enforcement.
#[sqlx::test]
async fn rule_mutations_rebuild_compiled_policy(pool: PgPool) {
    let store = Store::new(pool);
    store.seed_defaults(&seed()).await.unwrap();

    // No rule covers `uncovered` yet: the seeded settings default applies.
    let before = compiled_from_db(&store).await;
    assert_eq!(
        before.evaluate("uncovered", &serde_json::json!({})).verdict,
        GuardVerdict::Deny
    );
    assert_eq!(
        before
            .evaluate("mcp__chess__createGame", &serde_json::json!({}))
            .verdict,
        GuardVerdict::Allow,
        "seeded allow rules stay in force"
    );

    let created = store
        .create_rule(&rule("deny-uncovered", "uncovered", "deny", 500))
        .await
        .unwrap();
    let after_create = compiled_from_db(&store).await;
    let decision = after_create.evaluate("uncovered", &serde_json::json!({}));
    assert_eq!(decision.verdict, GuardVerdict::Deny);
    assert_eq!(decision.matched_rule.as_deref(), Some("deny-uncovered"));

    store.toggle_rule(created.id, false).await.unwrap();
    let after_disable = compiled_from_db(&store).await;
    assert_eq!(
        after_disable
            .evaluate("uncovered", &serde_json::json!({}))
            .verdict,
        GuardVerdict::Deny,
        "disabled rules vanish from the snapshot; the default action resumes"
    );

    store.toggle_rule(created.id, true).await.unwrap();
    store.delete_rule(created.id).await.unwrap();
    let after_delete = compiled_from_db(&store).await;
    let cleaned = after_delete.evaluate("uncovered", &serde_json::json!({}));
    assert_eq!(cleaned.matched_rule, None, "deletion is observable");
    assert_eq!(
        after_delete.prescribed_approval_secs(&cleaned),
        900,
        "the settings approval budget rides the snapshot"
    );
}

// ---------- 8. rules CRUD ----------

/// Create, full replace, toggle, and list order; unknown ids are `404` and
/// malformed inputs are rejected before hitting SQL.
#[sqlx::test]
async fn guard_rules_crud_toggle_and_evaluation_order(pool: PgPool) {
    let store = Store::new(pool);
    store.seed_defaults(&seed()).await.unwrap();

    let quiet = store
        .create_rule(&rule("quiet", "mcp__chess__waitForNextTurn", "ask", 100))
        .await
        .unwrap();
    assert!(quiet.enabled && quiet.timeout_secs == 0);

    let loud = rule("loud", "mcp__chess__waitForNextTurn", "allow", 200);
    let replaced = store.update_rule(quiet.id, &loud).await.unwrap();
    assert_eq!(replaced.name, "loud");
    assert_eq!(replaced.action, "allow");
    assert_eq!(replaced.priority, 200);

    // Evaluation order: priority DESC, then id ASC.
    let listed = store.list_rules().await.unwrap();
    let keyed: Vec<(i32, i64)> = listed.iter().map(|r| (-r.priority, r.id)).collect();
    let mut sorted = keyed.clone();
    sorted.sort();
    assert_eq!(keyed, sorted, "list_rules returns evaluation order");

    // Disabled rules disappear from the snapshot builder's feed.
    store.toggle_rule(replaced.id, false).await.unwrap();
    let enabled = store.list_enabled_rules().await.unwrap();
    assert!(!enabled.iter().any(|r| r.id == replaced.id));

    assert!(matches!(
        store.update_rule(424_242, &loud).await,
        Err(StoreError::NotFound(_))
    ));
    assert!(matches!(
        store.toggle_rule(424_242, true).await,
        Err(StoreError::NotFound(_))
    ));
    assert!(matches!(
        store.delete_rule(424_242).await,
        Err(StoreError::NotFound(_))
    ));

    for bad in [
        rule("", "tool", "allow", 0),  // empty name
        rule("x", "tool", "maybe", 0), // bad action
        GuardRuleInput {
            // bad match_target
            match_target: "headers".into(),
            ..rule("x", "tool", "allow", 0)
        },
        GuardRuleInput {
            // bad match_type
            match_type: "prefix".into(),
            ..rule("x", "tool", "allow", 0)
        },
        GuardRuleInput {
            // empty pattern
            pattern: "  ".into(),
            ..rule("x", "tool", "allow", 0)
        },
        GuardRuleInput {
            // negative timeout
            timeout_secs: -1,
            ..rule("x", "tool", "allow", 0)
        },
    ] {
        assert!(
            matches!(
                store.create_rule(&bad).await,
                Err(StoreError::Validation(_))
            ),
            "expected validation rejection for {:?}",
            bad.name
        );
    }
}

// ---------- 9. settings ----------

/// `update_settings` is a partial merge; unknown profile references are
/// `404`, non-positive timeouts are rejected, and the singleton id keeps the
/// table at exactly one row.
#[sqlx::test]
async fn settings_partial_merge_and_validation(pool: PgPool) {
    let store = Store::new(pool);
    store.seed_defaults(&seed()).await.unwrap();
    let backup = store
        .save_profile(&profile("backup", "m-backup", 5, false))
        .await
        .unwrap();

    let original = store.get_settings().await.unwrap().unwrap();
    let patched: AppSettingsRow = store
        .update_settings(&SettingPatch {
            approval_timeout_secs: Some(600),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(patched.approval_timeout_secs, 600);
    assert_eq!(patched.planner_profile_id, original.planner_profile_id);
    assert_eq!(patched.worker_profile_id, original.worker_profile_id);

    // Merge the worker role onto another profile; the planner id sticks.
    let moved: AppSettingsRow = store
        .update_settings(&SettingPatch {
            worker_profile_id: Some(backup),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(moved.worker_profile_id, backup);
    assert_eq!(moved.planner_profile_id, original.planner_profile_id);
    assert_eq!(moved.approval_timeout_secs, 600);

    assert!(matches!(
        store
            .update_settings(&SettingPatch {
                planner_profile_id: Some(424_242),
                ..Default::default()
            })
            .await,
        Err(StoreError::NotFound(_))
    ));
    assert!(matches!(
        store
            .update_settings(&SettingPatch {
                approval_timeout_secs: Some(0),
                ..Default::default()
            })
            .await,
        Err(StoreError::Validation(_))
    ));

    // Unknown id on `set_active_profile` is 404; both roles reach it.
    assert!(matches!(
        store.set_active_profile("planner", 424_242).await,
        Err(StoreError::NotFound(_))
    ));
    assert!(matches!(
        store.set_active_profile("settings", backup).await,
        Err(StoreError::Validation(_))
    ));
    store.set_active_profile("planner", backup).await.unwrap();
    assert_eq!(
        store
            .get_settings()
            .await
            .unwrap()
            .unwrap()
            .planner_profile_id,
        backup
    );
}

// ---------- 10. save_profile zero-id insert + settings bootstrap ----------

/// ARTEX parity (`db/config.go` SaveProfile): `id == 0` (and negatives) mean
/// "new row". The insert also bootstraps the `app_settings` singleton when it
/// does not exist yet — both role defaults point at the first inserted
/// profile — and `ON CONFLICT DO NOTHING` keeps an existing settings row
/// (env-seeded or operator-made) authoritative for later inserts.
#[sqlx::test]
async fn save_profile_zero_id_inserts_and_bootstraps_the_settings_singleton(pool: PgPool) {
    let store = Store::new(pool);

    // A mock-mode database: no env seed ran, so no settings row exists.
    assert!(store.get_settings().await.unwrap().is_none());

    let first = store
        .save_profile(&LlmProfileUpsert {
            id: Some(0),
            name: "first".into(),
            format: "openai".into(),
            base_url: "https://api.river.ai/v1".into(),
            model: "m-first".into(),
            api_key: "sk-first".into(),
            max_tokens: 0,
            context_window_k: 0,
            streaming: true,
            priority: 0,
            pool_exclude: false,
            thinking_type: String::new(),
            reasoning_effort: String::new(),
        })
        .await
        .unwrap();
    assert!(first > 0, "zero id inserts, it does not address row 0");

    let settings = store
        .get_settings()
        .await
        .unwrap()
        .expect("the insert bootstrapped the settings singleton");
    assert_eq!(settings.planner_profile_id, first);
    assert_eq!(settings.worker_profile_id, first);
    assert_eq!(settings.approval_timeout_secs, 900);
    assert_eq!(settings.guard_default_action, "deny");

    // A negative id inserts too (same `<= 0` rule).
    let negative = store
        .save_profile(&LlmProfileUpsert {
            id: Some(-2),
            ..profile("negative", "m-neg", 0, false)
        })
        .await
        .unwrap();
    assert!(negative > 0);

    // Later inserts never overwrite the singleton (DO NOTHING).
    let second = store
        .save_profile(&profile("second", "m-second", 5, false))
        .await
        .unwrap();
    let after = store.get_settings().await.unwrap().unwrap();
    assert_eq!(after.planner_profile_id, first, "the first insert won");
    assert_eq!(after.worker_profile_id, first);
    assert_ne!(second, first);
}

/// `guard_default_action` is a validated settings patch field: `allow`,
/// `deny`, `ask` persist; anything else is a 400-shaped validation error.
#[sqlx::test]
async fn settings_patch_guard_default_action_validates_and_persists(pool: PgPool) {
    let store = Store::new(pool);
    store.seed_defaults(&seed()).await.unwrap();

    let patched = store
        .update_settings(&SettingPatch {
            guard_default_action: Some("ask".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(patched.guard_default_action, "ask");
    assert_eq!(
        store
            .get_settings()
            .await
            .unwrap()
            .unwrap()
            .guard_default_action,
        "ask",
        "the write persists"
    );

    // Whitespace around the enum is tolerated; unknown values are rejected.
    let spaced = store
        .update_settings(&SettingPatch {
            guard_default_action: Some("  allow ".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(spaced.guard_default_action, "allow");

    for bad in ["maybe", ""] {
        assert!(
            matches!(
                store
                    .update_settings(&SettingPatch {
                        guard_default_action: Some(bad.into()),
                        ..Default::default()
                    })
                    .await,
                Err(StoreError::Validation(_))
            ),
            "expected validation rejection for {bad:?}"
        );
    }
}

// ---------- formatting guard ----------

/// The resolved tag is the stability contract behind `agent_runs.profile`:
/// named profiles append `name#id`, synthetic env profiles stay bare.
#[test]
fn resolved_tag_contract() {
    let named = ResolvedProfile {
        id: 7,
        name: "backup".into(),
        format: ProfileFormat::Anthropic,
        base_url: "https://api.anthropic.com".into(),
        model: "claude-x".into(),
        api_key: "sk-1".into(),
        max_tokens: 4096,
        streaming: false,
        thinking_type: "enabled".into(),
        reasoning_effort: "high".into(),
    };
    assert_eq!(
        named.tag(),
        "anthropic/claude-x/max_tokens=4096/stream=false|backup#7"
    );
    let synthetic = ResolvedProfile {
        id: 0,
        name: String::new(),
        ..named
    };
    assert_eq!(
        synthetic.tag(),
        "anthropic/claude-x/max_tokens=4096/stream=false"
    );
    // Approval budgets are compared as whole seconds in the store.
    assert_eq!(Duration::minutes(15).num_seconds(), 900);
}
