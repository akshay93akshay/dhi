//! Deletion only removes terminal tasks and their owned rows.
//!
//! Every `#[sqlx::test]` gets a fresh database, so the counts below describe
//! exactly one task.

use dhi_core::{GraphStore, GuardStore, TaskState};
use dhi_store::Store;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

async fn count(pool: &PgPool, table: &str, task_id: Uuid) -> i64 {
    // `table` is a compile-time literal supplied by this file only.
    let sql = format!("SELECT count(*) FROM {table} WHERE task_id = $1");
    sqlx::query_scalar(&sql)
        .bind(task_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn make_terminal(store: &Store, task_id: Uuid, state: TaskState) {
    GraphStore::set_task_state_guarded(store, task_id, TaskState::Running)
        .await
        .unwrap();
    GraphStore::set_task_state_guarded(store, task_id, state)
        .await
        .unwrap();
}

#[sqlx::test]
async fn delete_task_cascades_every_task_owned_row(pool: PgPool) {
    let store = Store::new(pool);
    let task = store.create_task("delete me", 4).await.unwrap();
    let goal = store.add_goal(task.id, "one goal").await.unwrap();
    store
        .add_goal(task.id, "another goal".to_string())
        .await
        .unwrap();
    let intent = store
        .add_intent(task.id, task.origin_fact_id, "do the work")
        .await
        .unwrap();
    let fact = store
        .record_fact(task.id, "confirmed evidence".to_string(), json!({}))
        .await
        .unwrap();
    store
        .prove_goal(task.id, goal.id, fact.id, "test proof".to_string())
        .await
        .unwrap();
    store
        .record_result(
            task.id,
            intent.id,
            "a result".to_string(),
            dhi_core::ResultStatus::Accepted,
            json!({}),
        )
        .await
        .unwrap();
    let call = GuardStore::record_tool_call(
        &store,
        task.id,
        intent.id,
        "finishTurn".to_string(),
        json!({}),
    )
    .await
    .unwrap();
    let decision = GuardStore::record_guard_decision(
        &store,
        call.id,
        dhi_core::GuardVerdict::Ask,
        None,
        None,
        "needs a person".to_string(),
    )
    .await
    .unwrap();
    GuardStore::create_approval(
        &store,
        task.id,
        call.id,
        decision.id,
        "needs a person".to_string(),
        chrono::Utc::now() + chrono::Duration::minutes(5),
    )
    .await
    .unwrap();
    assert_eq!(goal.task_id, task.id);
    assert_eq!(fact.task_id, task.id);

    make_terminal(&store, task.id, TaskState::Done).await;
    assert!(store.delete_task(task.id).await.unwrap());

    assert!(store.get_task(task.id).await.unwrap().is_none());
    for table in [
        "work_goals",
        "facts",
        "intents",
        "results",
        "activity",
        "tool_calls",
        "approvals",
        "agent_runs",
    ] {
        assert_eq!(
            count(store.pool(), table, task.id).await,
            0,
            "{table} must cascade with its task"
        );
    }
}

#[sqlx::test]
async fn delete_task_refuses_active_and_unknown_tasks(pool: PgPool) {
    let store = Store::new(pool);
    let task = store.create_task("still active", 4).await.unwrap();
    let intent = store
        .add_intent(task.id, task.origin_fact_id, "live work")
        .await
        .unwrap();

    assert!(!store.delete_task(task.id).await.unwrap());
    assert!(store.get_task(task.id).await.unwrap().is_some());
    assert_eq!(
        store.list_intents(task.id, 10).await.unwrap().len(),
        1,
        "a refused delete must not touch the task's rows"
    );
    assert_eq!(
        store.list_intents(task.id, 10).await.unwrap()[0].id,
        intent.id
    );
    assert!(!store.delete_task(Uuid::new_v4()).await.unwrap());
}

#[sqlx::test]
async fn cancelled_is_deletable(pool: PgPool) {
    let store = Store::new(pool);
    let task = store.create_task("cancel then delete", 2).await.unwrap();
    make_terminal(&store, task.id, TaskState::Cancelled).await;

    assert!(store.delete_task(task.id).await.unwrap());
    assert!(store.get_task(task.id).await.unwrap().is_none());
    assert_eq!(count(store.pool(), "activity", task.id).await, 0);
}

#[sqlx::test]
async fn delete_tasks_removes_only_terminal_ids(pool: PgPool) {
    let store = Store::new(pool);
    let done = store.create_task("done", 2).await.unwrap();
    let cancelled = store.create_task("cancelled", 2).await.unwrap();
    let active = store.create_task("active", 2).await.unwrap();
    make_terminal(&store, done.id, TaskState::Done).await;
    make_terminal(&store, cancelled.id, TaskState::Cancelled).await;
    make_terminal(&store, active.id, TaskState::Running).await;

    let deleted = store
        .delete_tasks(&[done.id, cancelled.id, active.id, Uuid::new_v4()])
        .await
        .unwrap();

    assert_eq!(deleted.len(), 2);
    assert!(deleted.contains(&done.id));
    assert!(deleted.contains(&cancelled.id));
    assert!(store.get_task(done.id).await.unwrap().is_none());
    assert!(store.get_task(cancelled.id).await.unwrap().is_none());
    assert!(store.get_task(active.id).await.unwrap().is_some());
}
