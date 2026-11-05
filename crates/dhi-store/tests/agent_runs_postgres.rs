//! `agent_runs` persistence: one row per completion call, round-tripped with
//! every column, against a real PostgreSQL store.
//!
//! These prove the store side: the columns survive save/load, the
//! `checkpoint_id` FK nulls rather than cascades, and the status CHECK rejects
//! anything outside the contract vocabulary.

use chrono::Utc;
use dhi_core::{AgentRole, AgentRun, TaskState};
use dhi_store::{Store, StoreError};
use sqlx::PgPool;
use uuid::Uuid;

fn run_row(task_id: Uuid, run_id: Uuid, call_index: u32) -> AgentRun {
    let now = Utc::now();
    AgentRun {
        id: Uuid::new_v4(),
        run_id,
        call_index,
        task_id,
        intent_id: None,
        role: AgentRole::Planner,
        model: "deepseek-v4.1".to_string(),
        profile: "open_ai/deepseek-v4.1/max_tokens=4096/stream=true".to_string(),
        prompt: "system\n\nuser".to_string(),
        prompt_hash: "planner@0af7c1e9b2d3".to_string(),
        variables: serde_json::json!({"goal": "forced mate", "bound": 4}),
        response: Some("NO PLAN: nothing to add".to_string()),
        thinking: Some("the origin fact already covers the first goal".to_string()),
        tokens_in: 100,
        tokens_out: 20,
        cache_read_tokens: 60,
        cache_write_tokens: 10,
        reasoning_tokens: 5,
        cost: 0.0,
        status: "ok".to_string(),
        error: None,
        latency_ms: 412,
        checkpoint_id: None,
        created_at: now,
        updated_at: now,
    }
}

#[sqlx::test]
async fn agent_run_round_trips_every_new_column(pool: PgPool) {
    let store = Store::new(pool);
    let task = store.create_task("forced mate", 4).await.unwrap();
    let run_id = Uuid::new_v4();
    let row = run_row(task.id, run_id, 0);

    store.save_agent_run(&row).await.unwrap();
    let loaded = store
        .get_agent_run(row.id)
        .await
        .unwrap()
        .expect("row exists");

    assert_eq!(loaded.id, row.id);
    assert_eq!(loaded.run_id, run_id);
    assert_eq!(loaded.call_index, 0);
    assert_eq!(loaded.task_id, task.id);
    assert_eq!(loaded.intent_id, None);
    assert_eq!(loaded.role, AgentRole::Planner);
    assert_eq!(loaded.model, "deepseek-v4.1");
    assert_eq!(loaded.profile, row.profile);
    assert_eq!(loaded.prompt, row.prompt);
    assert_eq!(loaded.prompt_hash, row.prompt_hash);
    assert_eq!(loaded.variables, row.variables);
    assert_eq!(loaded.thinking, row.thinking);
    assert_eq!(loaded.tokens_in, 100);
    assert_eq!(loaded.tokens_out, 20);
    assert_eq!(loaded.cache_read_tokens, 60);
    assert_eq!(loaded.cache_write_tokens, 10);
    assert_eq!(loaded.reasoning_tokens, 5);
    assert_eq!(loaded.cost, 0.0);
    assert_eq!(loaded.status, "ok");
    assert_eq!(loaded.error, None);
    assert_eq!(loaded.latency_ms, 412);
    assert!(loaded.checkpoint_id.is_none());

    // The API key never reaches the row: the profile is a fingerprint only.
    assert!(!loaded.profile.contains("rv_"));
}

#[sqlx::test]
async fn blank_thinking_is_stored_as_null(pool: PgPool) {
    let store = Store::new(pool);
    let task = store.create_task("forced mate", 4).await.unwrap();
    let mut row = run_row(task.id, Uuid::new_v4(), 0);
    row.thinking = Some("   ".to_string());

    store.save_agent_run(&row).await.unwrap();
    let loaded = store.get_agent_run(row.id).await.unwrap().unwrap();
    assert_eq!(loaded.thinking, None, "blank reasoning is NULL, not \"\"");
}

#[sqlx::test]
async fn agent_run_reopens_as_running_then_finishes(pool: PgPool) {
    let store = Store::new(pool);
    let task = store.create_task("forced mate", 4).await.unwrap();
    let run_id = Uuid::new_v4();

    let mut row = run_row(task.id, run_id, 0);
    row.status = "running".to_string();
    row.response = None;
    row.latency_ms = 0;
    store.save_agent_run(&row).await.unwrap();

    let mut finished = row.clone();
    finished.status = "error".to_string();
    finished.error = Some("connect: refused".to_string());
    finished.updated_at = Utc::now();
    store.save_agent_run(&finished).await.unwrap();

    let loaded = store.get_agent_run(row.id).await.unwrap().unwrap();
    assert_eq!(loaded.status, "error");
    assert_eq!(loaded.error.as_deref(), Some("connect: refused"));

    // A retry is a second row with the same run_id.
    let retry = run_row(task.id, run_id, 1);
    store.save_agent_run(&retry).await.unwrap();
    let runs = store.list_agent_runs(task.id).await.unwrap();
    assert_eq!(runs.len(), 2);
    assert!(runs.iter().all(|r| r.run_id == run_id));
    let mut indexes: Vec<u32> = runs.iter().map(|r| r.call_index).collect();
    indexes.sort_unstable();
    assert_eq!(indexes, vec![0, 1]);
}

#[sqlx::test]
async fn agent_run_status_check_rejects_unknown_status(pool: PgPool) {
    let store = Store::new(pool);
    let task = store.create_task("forced mate", 4).await.unwrap();
    let mut row = run_row(task.id, Uuid::new_v4(), 0);
    row.status = "bogus".to_string();

    // Rejected locally before the database sees it...
    let err = store
        .save_agent_run(&row)
        .await
        .expect_err("status rejected");
    assert!(matches!(err, StoreError::InvalidValue(_)));

    // ...and by the constraint when the store is bypassed.
    let raw = sqlx::query(
        "INSERT INTO agent_runs (id, run_id, task_id, role, model, prompt, status)
         VALUES ($1, $1, $2, 'planner', 'm', 'p', 'bogus')",
    )
    .bind(Uuid::new_v4())
    .bind(task.id)
    .execute(store.pool())
    .await;
    let code = match raw {
        Err(sqlx::Error::Database(db)) => db.code().map(|c| c.into_owned()),
        other => panic!("expected a CHECK violation, got {other:?}"),
    };
    assert_eq!(code.as_deref(), Some("23514"));
}

#[sqlx::test]
async fn checkpoint_pointer_nulls_instead_of_cascading(pool: PgPool) {
    let store = Store::new(pool);
    let task = store.create_task("forced mate", 4).await.unwrap();
    let run_id = Uuid::new_v4();
    let mut row = run_row(task.id, run_id, 0);
    store.save_agent_run(&row).await.unwrap();

    let checkpoint = store
        .save_checkpoint(row.id, serde_json::json!({"format": 1}))
        .await
        .unwrap();
    row.checkpoint_id = Some(checkpoint);
    store.save_agent_run(&row).await.unwrap();
    let linked = store.get_agent_run(row.id).await.unwrap().unwrap();
    assert_eq!(linked.checkpoint_id, Some(checkpoint));

    sqlx::query("DELETE FROM checkpoints WHERE id = $1")
        .bind(checkpoint)
        .execute(store.pool())
        .await
        .unwrap();

    let after = store
        .get_agent_run(row.id)
        .await
        .unwrap()
        .expect("the run outlives its pruned checkpoint");
    assert!(after.checkpoint_id.is_none());
}

#[sqlx::test]
async fn agent_run_requires_an_existing_task(pool: PgPool) {
    let store = Store::new(pool);
    let row = run_row(Uuid::new_v4(), Uuid::new_v4(), 0);
    assert!(store.save_agent_run(&row).await.is_err());
}

#[sqlx::test]
async fn terminal_task_still_has_its_run_rows(pool: PgPool) {
    // The record is written before the task finishes and survives it.
    let store = Store::new(pool);
    let task = store.create_task("forced mate", 4).await.unwrap();
    store
        .save_agent_run(&run_row(task.id, Uuid::new_v4(), 0))
        .await
        .unwrap();
    store
        .set_task_state_guarded(task.id, TaskState::Done)
        .await
        .unwrap();
    assert_eq!(store.list_agent_runs(task.id).await.unwrap().len(), 1);
}
