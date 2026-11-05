//! Fact-based proof on the in-memory store.
//!
//! The engine relies on these rules: a proof names a confirmed fact from the
//! same task, an origin fact cannot prove anything, and the store reports when
//! every goal is met.

use dhi_core::{FactState, GoalState, GraphStore};
use dhi_testkit::MemoryStore;

#[tokio::test]
async fn prove_goal_requires_confirmed_fact() {
    let store = MemoryStore::new();
    let task = store.seed_task("origin cannot prove", 3);
    let goal = store
        .add_goal(task.id, "reach the goal".to_string())
        .await
        .unwrap();

    let err = store
        .prove_goal(task.id, goal.id, task.origin_fact_id, "origin".to_string())
        .await
        .expect_err("an origin fact is not confirmed");

    assert!(err.to_string().contains("confirmed"), "{err}");
    let goals = store.goals(task.id);
    assert_eq!(goals[0].state, GoalState::Open);
    assert!(
        store
            .activities(task.id)
            .iter()
            .all(|activity| activity.kind != "goal_proven")
    );
}

#[tokio::test]
async fn prove_goal_marks_goal_met() {
    let store = MemoryStore::new();
    let task = store.seed_task("prove it", 3);
    let goal = store
        .add_goal(task.id, "reach the goal".to_string())
        .await
        .unwrap();
    let fact = store
        .record_fact(
            task.id,
            "confirmed evidence".to_string(),
            serde_json::json!({}),
        )
        .await
        .unwrap();
    assert_eq!(fact.state, FactState::Confirmed);

    let all_met = store
        .prove_goal(task.id, goal.id, fact.id, "the evidence".to_string())
        .await
        .unwrap();

    assert!(all_met);
    assert_eq!(store.goals(task.id)[0].state, GoalState::Met);
}

#[tokio::test]
async fn prove_goal_records_proof_fact_id() {
    let store = MemoryStore::new();
    let task = store.seed_task("record the proof", 3);
    let goal = store
        .add_goal(task.id, "reach the goal".to_string())
        .await
        .unwrap();
    let fact = store
        .record_fact(
            task.id,
            "confirmed evidence".to_string(),
            serde_json::json!({}),
        )
        .await
        .unwrap();

    store
        .prove_goal(task.id, goal.id, fact.id, "the evidence".to_string())
        .await
        .unwrap();

    let goal = &store.goals(task.id)[0];
    assert_eq!(goal.proof_fact_id, Some(fact.id));
    let proven = store
        .activities(task.id)
        .into_iter()
        .find(|activity| activity.kind == "goal_proven")
        .expect("a proof leaves an activity row");
    assert_eq!(proven.payload["goal_id"], goal.id.to_string());
    assert_eq!(proven.payload["fact_id"], fact.id.to_string());
    assert_eq!(proven.payload["reason"], "the evidence");
}

#[tokio::test]
async fn prove_goal_rejects_fact_from_another_task() {
    let store = MemoryStore::new();
    let task = store.seed_task("task A", 3);
    let other = store.seed_task("task B", 3);
    let goal = store
        .add_goal(task.id, "reach the goal".to_string())
        .await
        .unwrap();
    let foreign = store
        .record_fact(
            other.id,
            "another task's evidence".to_string(),
            serde_json::json!({}),
        )
        .await
        .unwrap();

    let err = store
        .prove_goal(task.id, goal.id, foreign.id, "foreign evidence".to_string())
        .await
        .expect_err("a fact from another task cannot prove this goal");

    assert!(err.to_string().contains("does not belong"), "{err}");
    assert_eq!(store.goals(task.id)[0].state, GoalState::Open);
}

#[tokio::test]
async fn all_goals_met_sets_task_done() {
    let store = MemoryStore::new();
    let task = store.seed_task("prove both", 3);
    let first = store
        .add_goal(task.id, "first goal".to_string())
        .await
        .unwrap();
    let second = store
        .add_goal(task.id, "second goal".to_string())
        .await
        .unwrap();
    let fact = store
        .record_fact(
            task.id,
            "confirmed evidence".to_string(),
            serde_json::json!({}),
        )
        .await
        .unwrap();

    assert!(
        !store
            .prove_goal(task.id, first.id, fact.id, "first proof".to_string())
            .await
            .unwrap(),
        "one met goal is not all goals"
    );
    assert!(
        store
            .prove_goal(task.id, second.id, fact.id, "second proof".to_string())
            .await
            .unwrap(),
        "the last proof reports every goal met"
    );
    assert!(
        store
            .goals(task.id)
            .iter()
            .all(|goal| goal.state == GoalState::Met)
    );
}
