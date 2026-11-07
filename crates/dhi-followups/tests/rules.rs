use chrono::{Duration, Utc};
use dhi_core::{Activity, Fact, FactState, Goal, GoalState, Task, TaskState};
use dhi_followups::{
    ADD_GOAL_MAX_CHARS, SIDE_QUESTION_ACTIVITY_LIMIT, SIDE_QUESTION_MAX_CHARS, build_snapshot,
    can_reopen, fresh_evidence, snapshot_summary, validate_goal, validate_question,
};
use serde_json::json;
use uuid::Uuid;

fn task() -> Task {
    Task::new("block test")
}

#[test]
fn fresh_evidence_requires_a_newer_fact() {
    let goal_at = Utc::now();
    assert!(fresh_evidence(goal_at + Duration::seconds(1), goal_at));
    assert!(!fresh_evidence(goal_at, goal_at));
    assert!(!fresh_evidence(goal_at - Duration::seconds(1), goal_at));
}

#[test]
fn only_terminal_tasks_can_reopen() {
    for state in [TaskState::Done, TaskState::Failed, TaskState::Timeout] {
        assert!(can_reopen(state), "{state:?}");
    }
    for state in [TaskState::Created, TaskState::Running, TaskState::Paused] {
        assert!(!can_reopen(state), "{state:?}");
    }
}

#[test]
fn question_and_goal_validation_trim_and_cap() {
    assert_eq!(validate_question("  why?  ").as_deref(), Ok("why?"));
    assert!(validate_question("   ").is_err());
    assert!(validate_question(&"q".repeat(SIDE_QUESTION_MAX_CHARS + 1)).is_err());
    assert!(validate_question(&"q".repeat(SIDE_QUESTION_MAX_CHARS)).is_ok());

    assert_eq!(validate_goal("  do it  ").as_deref(), Ok("do it"));
    assert!(validate_goal("").is_err());
    assert!(validate_goal(&"g".repeat(ADD_GOAL_MAX_CHARS + 1)).is_err());
    assert!(validate_goal(&"g".repeat(ADD_GOAL_MAX_CHARS)).is_ok());
}

#[test]
fn snapshot_freezes_the_graph_and_caps_activity() {
    let task = task();
    let now = Utc::now();
    let goal = Goal {
        id: Uuid::new_v4(),
        task_id: task.id,
        description: "first goal".to_string(),
        state: GoalState::Open,
        proof_fact_id: None,
        created_at: now,
    };
    let fact = Fact {
        id: Uuid::new_v4(),
        task_id: task.id,
        claim: "evidence".to_string(),
        state: FactState::Confirmed,
        provenance: json!({"detail": "kept"}),
        origin_of: None,
        created_at: now,
    };
    let activity: Vec<Activity> = (1..=55)
        .map(|id| Activity {
            id,
            task_id: task.id,
            kind: "test".to_string(),
            message: format!("row {id}"),
            payload: json!({}),
            created_at: now,
        })
        .collect();

    let snapshot = build_snapshot(&task, std::slice::from_ref(&goal), &[fact], &activity);
    assert_eq!(snapshot["task"]["goal"], "block test");
    assert_eq!(snapshot["task"]["state"], "created");
    assert_eq!(snapshot["goals"].as_array().map(Vec::len), Some(1));
    assert_eq!(snapshot["facts"].as_array().map(Vec::len), Some(1));
    let rows = snapshot["activity"].as_array().expect("activity array");
    assert_eq!(rows.len(), SIDE_QUESTION_ACTIVITY_LIMIT);
    assert_eq!(rows[0]["id"], 6, "only the newest rows are kept");

    let summary = snapshot_summary(&snapshot);
    assert_eq!(summary["goal"], "block test");
    assert_eq!(summary["goals"], 1);
    assert_eq!(summary["facts"], 1);
    assert_eq!(summary["activity"], SIDE_QUESTION_ACTIVITY_LIMIT);
}
