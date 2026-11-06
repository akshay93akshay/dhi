//! Integration tests for the PostgreSQL store.
//!
//! Every `#[sqlx::test]` runs against its own freshly created database, so
//! these tests never share state. `DATABASE_URL` must point at a role that is
//! allowed to create databases.

use dhi_core::{
    CapabilityLimits, CoreError, FactState, IntentPlan, IntentState, Subtask, TaskState, Topology,
    WorkerSpec,
};
use dhi_store::{Store, StoreError};
use sqlx::PgPool;
use uuid::Uuid;

/// A task is born with exactly one fact: its origin fact.
#[sqlx::test]
async fn create_task_writes_one_origin_fact(pool: PgPool) {
    let store = Store::new(pool);

    let task = store.create_task("forced mate", 4).await.unwrap();
    assert_eq!(task.state, TaskState::Created);

    let persisted = store.get_task(task.id).await.unwrap().expect("task exists");
    assert_eq!(persisted.state, TaskState::Created);

    let facts = store.list_facts(task.id).await.unwrap();
    assert_eq!(facts.len(), 1, "a new task must have exactly one fact");
    assert_eq!(facts[0].id, task.origin_fact_id);
    assert_eq!(facts[0].state, FactState::Origin);
    assert_eq!(facts[0].task_id, task.id);
}

/// The claim is a single atomic `UPDATE ... WHERE state = 'open'`.
#[sqlx::test]
async fn concurrent_claim_has_one_winner(pool: PgPool) {
    let store = Store::new(pool);
    let task = store.create_task("forced mate", 4).await.unwrap();
    let intent = store
        .add_intent(task.id, task.origin_fact_id, "find the mating move")
        .await
        .unwrap();

    let (first, second) = tokio::join!(
        store.claim_intent(intent.id, "worker-1"),
        store.claim_intent(intent.id, "worker-2"),
    );
    let first = first.unwrap();
    let second = second.unwrap();

    assert_ne!(
        first.is_some(),
        second.is_some(),
        "exactly one concurrent claim must win"
    );
    let claimed = first.or(second).expect("one claim won");
    assert_eq!(claimed.state, IntentState::Running);

    let (state, owner): (String, Option<String>) =
        sqlx::query_as("SELECT state, owner FROM intents WHERE id = $1")
            .bind(intent.id)
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(state, "running");
    assert!(
        matches!(owner.as_deref(), Some("worker-1") | Some("worker-2")),
        "owner must be one of the two workers, got {owner:?}"
    );
}

/// Eight concurrent claimants race for one intent; exactly one owns it.
#[sqlx::test]
async fn concurrent_claim_with_eight_workers_has_one_winner(pool: PgPool) {
    let store = Store::new(pool);
    let task = store.create_task("forced mate", 4).await.unwrap();
    let intent = store
        .add_intent(task.id, task.origin_fact_id, "find the mating move")
        .await
        .unwrap();

    let owners: Vec<String> = (0..8).map(|i| format!("worker-{i}")).collect();
    let results = futures::future::join_all(
        owners
            .iter()
            .map(|owner| store.claim_intent(intent.id, owner.as_str())),
    )
    .await;

    let mut winners = Vec::new();
    for (owner, result) in owners.iter().zip(results) {
        if let Some(claimed) = result.unwrap() {
            winners.push((owner, claimed));
        }
    }

    assert_eq!(
        winners.len(),
        1,
        "exactly one of the eight claims must win, got {}",
        winners.len()
    );
    let (winner_owner, claimed) = &winners[0];
    assert_eq!(claimed.state, IntentState::Running);
    assert_eq!(
        claimed.owner.as_deref(),
        Some(winner_owner.as_str()),
        "returned owner must be the winning worker"
    );

    let persisted_owner: Option<String> =
        sqlx::query_scalar("SELECT owner FROM intents WHERE id = $1")
            .bind(intent.id)
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(
        persisted_owner.as_deref(),
        Some(winner_owner.as_str()),
        "persisted owner must be the winning worker"
    );
}

/// An intent may not point at a fact that does not exist.
#[sqlx::test]
async fn intent_with_unknown_fact_is_rejected(pool: PgPool) {
    let store = Store::new(pool);
    let task = store.create_task("forced mate", 4).await.unwrap();

    let err = store
        .add_intent(task.id, Uuid::new_v4(), "investigate a missing fact")
        .await
        .expect_err("an unknown fact id must be rejected");

    assert_eq!(
        store_sqlstate(&err).as_deref(),
        Some("23503"),
        "expected a foreign key violation, got {err:?}"
    );
}

/// An intent without a fact is rejected before it reaches the database.
#[sqlx::test]
async fn intent_with_nil_fact_is_rejected(pool: PgPool) {
    let store = Store::new(pool);
    let task = store.create_task("forced mate", 4).await.unwrap();

    let err = store
        .add_intent(task.id, Uuid::nil(), "investigate nothing")
        .await
        .expect_err("a nil fact id must be rejected");

    assert!(
        matches!(err, StoreError::Core(CoreError::IntentWithoutFact)),
        "expected IntentWithoutFact, got {err:?}"
    );
}

/// An intent may not reference a fact from another task.
#[sqlx::test]
async fn intent_cannot_reference_another_tasks_fact(pool: PgPool) {
    let store = Store::new(pool);
    let task_a = store.create_task("task A", 4).await.unwrap();
    let task_b = store.create_task("task B", 4).await.unwrap();
    let fact_a = store.list_facts(task_a.id).await.unwrap().remove(0);

    let err = store
        .add_intent(task_b.id, fact_a.id, "use another task's fact")
        .await
        .expect_err("a cross-task fact must be rejected");

    assert_eq!(
        store_sqlstate(&err).as_deref(),
        Some("23503"),
        "expected a foreign key violation, got {err:?}"
    );
}

/// `exploration_id` is unique at the database level.
#[sqlx::test]
async fn duplicate_exploration_is_rejected(pool: PgPool) {
    let store = Store::new(pool);
    let task = store.create_task("first exploration", 4).await.unwrap();

    let err = sqlx::query(
        "INSERT INTO tasks (id, goal, bound, state, exploration_id, origin_fact_id)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(Uuid::new_v4())
    .bind("second exploration")
    .bind(4)
    .bind(TaskState::Created.as_str())
    .bind(task.exploration_id)
    .bind(Uuid::new_v4())
    .execute(store.pool())
    .await
    .expect_err("duplicate exploration_id must be rejected");

    assert_eq!(
        sqlstate(&err).as_deref(),
        Some("23505"),
        "expected a unique violation, got {err:?}"
    );
}

/// Migrations from an empty database create the full schema, idempotently.
/// The schema includes tool control (`tool_catalog`, `tool_visibility`,
/// `skill_usage`), web-provider settings (`web_provider_settings`), and
/// persisted DHI+ plans (`intent_plans`).
#[sqlx::test(migrations = false)]
async fn migration_creates_24_tables_and_is_idempotent_from_clean_database(pool: PgPool) {
    assert_eq!(base_table_count(&pool).await, 0);

    let store = Store::new(pool);
    store.migrate().await.expect("first migrate succeeds");
    assert_eq!(base_table_count(store.pool()).await, 24);

    store.migrate().await.expect("second migrate succeeds");
    assert_eq!(base_table_count(store.pool()).await, 24);
}

/// A DHI+ plan round-trips through JSONB, an upsert replaces it, and an
/// intent without a plan (or an unknown id) reads back `None`.
#[sqlx::test]
async fn intent_plan_round_trips_upserts_and_unknown_returns_none(pool: PgPool) {
    let store = Store::new(pool);
    let task = store.create_task("team task", 4).await.unwrap();
    let intent = store
        .add_intent(task.id, task.origin_fact_id, "team work")
        .await
        .unwrap();

    assert!(
        store.get_intent_plan(intent.id).await.unwrap().is_none(),
        "an intent without a plan has no row"
    );
    assert!(
        store
            .get_intent_plan(Uuid::new_v4())
            .await
            .unwrap()
            .is_none(),
        "an unknown intent has no plan"
    );

    let first = sample_intent_plan("first plan");
    store.save_intent_plan(intent.id, &first).await.unwrap();
    assert_eq!(store.get_intent_plan(intent.id).await.unwrap(), Some(first));

    let second = sample_intent_plan("replacement plan");
    store.save_intent_plan(intent.id, &second).await.unwrap();
    assert_eq!(
        store.get_intent_plan(intent.id).await.unwrap(),
        Some(second),
        "saving again replaces the plan for the same intent"
    );

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM intent_plans WHERE intent_id = $1")
        .bind(intent.id)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(rows, 1, "the upsert keeps exactly one row per intent");
}

/// Two appends add exactly two rows.
#[sqlx::test]
async fn activity_appends_accumulate(pool: PgPool) {
    let store = Store::new(pool);
    let task = store.create_task("forced mate", 4).await.unwrap();

    let before = activity_count(store.pool(), task.id).await;
    store
        .append_activity(task.id, "note", "first note", serde_json::json!({"n": 1}))
        .await
        .unwrap();
    store
        .append_activity(task.id, "note", "second note", serde_json::json!({"n": 2}))
        .await
        .unwrap();
    let after = activity_count(store.pool(), task.id).await;

    assert_eq!(after, before + 2);
}

fn sample_intent_plan(intent: &str) -> IntentPlan {
    IntentPlan {
        intent: intent.to_string(),
        topology: Topology::ManagerWorker,
        workers: vec![
            WorkerSpec {
                key: "alpha".to_string(),
                role: "researcher".to_string(),
                prompt: "research".to_string(),
                max_turns: 1,
            },
            WorkerSpec {
                key: "beta".to_string(),
                role: "critic".to_string(),
                prompt: "critique".to_string(),
                max_turns: 1,
            },
        ],
        subtasks: vec![
            Subtask {
                id: "s1".to_string(),
                description: "subtask one".to_string(),
                worker_key: Some("alpha".to_string()),
            },
            Subtask {
                id: "s2".to_string(),
                description: "subtask two".to_string(),
                worker_key: Some("beta".to_string()),
            },
        ],
        limits: CapabilityLimits::default(),
    }
}

async fn base_table_count(pool: &PgPool) -> i64 {
    // `_sqlx_migrations` is bookkeeping, not part of the DHI schema.
    sqlx::query_scalar(
        "SELECT count(*)
         FROM information_schema.tables
         WHERE table_schema = 'public'
           AND table_type = 'BASE TABLE'
           AND table_name <> '_sqlx_migrations'",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn activity_count(pool: &PgPool, task_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM activity WHERE task_id = $1")
        .bind(task_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn sqlstate(err: &sqlx::Error) -> Option<String> {
    match err {
        sqlx::Error::Database(db) => db.code().map(|code| code.into_owned()),
        _ => None,
    }
}

fn store_sqlstate(err: &StoreError) -> Option<String> {
    match err {
        StoreError::Sqlx(err) => sqlstate(err),
        _ => None,
    }
}
