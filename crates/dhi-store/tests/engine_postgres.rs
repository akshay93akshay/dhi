//! Engine tests that run against a real PostgreSQL store.
//!
//! These prove the engine only depends on the `GraphStore` contract and that
//! the PostgreSQL implementation satisfies it end to end.

use std::sync::Arc;

use dhi_agent::MockAgentRuntime;
use dhi_core::TaskState;
use dhi_engine::Engine;
use dhi_store::Store;
use sqlx::PgPool;
use uuid::Uuid;

#[sqlx::test]
async fn engine_runs_task_against_postgres(pool: PgPool) {
    let store = Arc::new(Store::new(pool));
    let task = store.create_task("forced mate", 4).await.unwrap();

    let engine = Engine::new(store.clone(), Arc::new(MockAgentRuntime));
    let result = engine.run_task(task.id).await.unwrap();

    assert_eq!(result.task.state, TaskState::Done);

    let persisted = store.get_task(task.id).await.unwrap().expect("task exists");
    assert_eq!(persisted.state, TaskState::Done);

    let facts = store.list_facts(task.id).await.unwrap();
    assert!(
        facts.len() >= 2,
        "expected the origin fact plus at least one worked fact, got {}",
        facts.len()
    );
    assert_eq!(
        count(store.pool(), "results", task.id).await,
        0,
        "completion is a fact proof, not a result row"
    );
    assert!(
        count(store.pool(), "activity", task.id).await >= 1,
        "the engine must record activity"
    );
    let goals = store.list_goals(task.id).await.unwrap();
    assert_eq!(goals.len(), 1);
    assert_eq!(goals[0].state, dhi_core::GoalState::Met);
    assert!(goals[0].proof_fact_id.is_some());
}

async fn count(pool: &PgPool, table: &str, task_id: Uuid) -> i64 {
    // `table` is a compile-time literal supplied by this test file only.
    let sql = format!("SELECT count(*) FROM {table} WHERE task_id = $1");
    sqlx::query_scalar(&sql)
        .bind(task_id)
        .fetch_one(pool)
        .await
        .unwrap()
}
