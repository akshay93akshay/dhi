//! Fact-based proof on PostgreSQL.
//!
//! `prove_goal` runs in one transaction: it locks the fact and goal rows,
//! rejects a fact that is not confirmed or belongs to another task, marks the
//! goal met, appends `goal_proven`, and reports whether every goal is met.

use dhi_core::{FactState, GoalState, GraphStore};
use dhi_store::Store;
use serde_json::json;
use sqlx::PgPool;
use std::time::Duration;

#[sqlx::test]
async fn prove_goal_rejects_a_fact_older_than_the_goal(pool: PgPool) {
    let store = Store::new(pool);
    let task = store.create_task("fresh evidence", 3).await.unwrap();
    let old_fact = GraphStore::record_fact(
        &store,
        task.id,
        "an old confirmed fact".to_string(),
        json!({"epoch": "old"}),
    )
    .await
    .unwrap();
    // A new goal must be newer than the old fact for the rule to bite.
    std::thread::sleep(Duration::from_millis(10));
    let goal = GraphStore::add_goal(&store, task.id, "a human-added goal".into())
        .await
        .unwrap();

    let err = GraphStore::prove_goal(
        &store,
        task.id,
        goal.id,
        old_fact.id,
        "old evidence".to_string(),
    )
    .await
    .expect_err("an old fact must not prove a newer goal");

    assert!(err.to_string().contains("fresh evidence"), "{err}");
    let goals = store.list_goals(task.id).await.unwrap();
    assert_eq!(
        goals.iter().find(|row| row.id == goal.id).unwrap().state,
        GoalState::Open
    );
    assert!(
        store
            .list_activity(task.id, Some("goal_proven"))
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test]
async fn prove_goal_accepts_a_fact_recorded_after_the_goal(pool: PgPool) {
    let store = Store::new(pool);
    let task = store
        .create_task("fresh evidence accepted", 3)
        .await
        .unwrap();
    let goal = GraphStore::add_goal(&store, task.id, "a human-added goal".into())
        .await
        .unwrap();
    std::thread::sleep(Duration::from_millis(10));
    let new_fact = GraphStore::record_fact(
        &store,
        task.id,
        "fresh confirmed evidence".to_string(),
        json!({"epoch": "new"}),
    )
    .await
    .unwrap();

    GraphStore::prove_goal(
        &store,
        task.id,
        goal.id,
        new_fact.id,
        "fresh evidence".to_string(),
    )
    .await
    .expect("a newer fact proves the goal");

    let goals = store.list_goals(task.id).await.unwrap();
    let proven = goals.iter().find(|row| row.id == goal.id).unwrap();
    assert_eq!(proven.state, GoalState::Met);
    assert_eq!(proven.proof_fact_id, Some(new_fact.id));
}

#[sqlx::test]
async fn prove_goal_requires_confirmed_fact(pool: PgPool) {
    let store = Store::new(pool);
    let task = store.create_task("origin cannot prove", 3).await.unwrap();
    let goal = GraphStore::add_goal(&store, task.id, "reach the goal".into())
        .await
        .unwrap();

    let err = GraphStore::prove_goal(
        &store,
        task.id,
        goal.id,
        task.origin_fact_id,
        "origin".to_string(),
    )
    .await
    .expect_err("an origin fact is not confirmed");

    assert!(err.to_string().contains("confirmed"), "{err}");
    let goals = store.list_goals(task.id).await.unwrap();
    assert_eq!(goals[0].state, GoalState::Open);
    assert!(
        store
            .list_activity(task.id, Some("goal_proven"))
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test]
async fn prove_goal_marks_goal_met(pool: PgPool) {
    let store = Store::new(pool);
    let task = store.create_task("prove it", 3).await.unwrap();
    let goal = GraphStore::add_goal(&store, task.id, "reach the goal".into())
        .await
        .unwrap();
    let fact = store
        .record_fact(task.id, "confirmed evidence", serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(fact.state, FactState::Confirmed);

    let all_met = GraphStore::prove_goal(
        &store,
        task.id,
        goal.id,
        fact.id,
        "the evidence".to_string(),
    )
    .await
    .unwrap();

    assert!(all_met);
    assert_eq!(
        store.list_goals(task.id).await.unwrap()[0].state,
        GoalState::Met
    );
}

#[sqlx::test]
async fn prove_goal_records_proof_fact_id(pool: PgPool) {
    let store = Store::new(pool);
    let task = store.create_task("record the proof", 3).await.unwrap();
    let goal = GraphStore::add_goal(&store, task.id, "reach the goal".into())
        .await
        .unwrap();
    let fact = store
        .record_fact(task.id, "confirmed evidence", serde_json::json!({}))
        .await
        .unwrap();

    GraphStore::prove_goal(
        &store,
        task.id,
        goal.id,
        fact.id,
        "the evidence".to_string(),
    )
    .await
    .unwrap();

    let goal = &store.list_goals(task.id).await.unwrap()[0];
    assert_eq!(goal.proof_fact_id, Some(fact.id));
    let proven = store
        .list_activity(task.id, Some("goal_proven"))
        .await
        .unwrap();
    assert_eq!(proven.len(), 1);
    assert_eq!(proven[0].payload["goal_id"], goal.id.to_string());
    assert_eq!(proven[0].payload["fact_id"], fact.id.to_string());
    assert_eq!(proven[0].payload["reason"], "the evidence");
}

#[sqlx::test]
async fn prove_goal_rejects_fact_from_another_task(pool: PgPool) {
    let store = Store::new(pool);
    let task = store.create_task("task A", 3).await.unwrap();
    let other = store.create_task("task B", 3).await.unwrap();
    let goal = GraphStore::add_goal(&store, task.id, "reach the goal".into())
        .await
        .unwrap();
    let foreign = store
        .record_fact(other.id, "another task's evidence", serde_json::json!({}))
        .await
        .unwrap();

    let err = GraphStore::prove_goal(
        &store,
        task.id,
        goal.id,
        foreign.id,
        "foreign evidence".to_string(),
    )
    .await
    .expect_err("a fact from another task cannot prove this goal");

    assert!(err.to_string().contains("does not belong"), "{err}");
    assert_eq!(
        store.list_goals(task.id).await.unwrap()[0].state,
        GoalState::Open
    );
}

#[sqlx::test]
async fn all_goals_met_sets_task_done(pool: PgPool) {
    let store = Store::new(pool);
    let task = store.create_task("prove both", 3).await.unwrap();
    let first = GraphStore::add_goal(&store, task.id, "first goal".into())
        .await
        .unwrap();
    let second = GraphStore::add_goal(&store, task.id, "second goal".into())
        .await
        .unwrap();
    let fact = store
        .record_fact(task.id, "confirmed evidence", serde_json::json!({}))
        .await
        .unwrap();

    assert!(
        !GraphStore::prove_goal(
            &store,
            task.id,
            first.id,
            fact.id,
            "first proof".to_string()
        )
        .await
        .unwrap(),
        "one met goal is not all goals"
    );
    assert!(
        GraphStore::prove_goal(
            &store,
            task.id,
            second.id,
            fact.id,
            "second proof".to_string()
        )
        .await
        .unwrap(),
        "the last proof reports every goal met"
    );
    assert!(
        store
            .list_goals(task.id)
            .await
            .unwrap()
            .iter()
            .all(|goal| goal.state == GoalState::Met)
    );
}
