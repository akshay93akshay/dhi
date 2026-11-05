//! Adding a goal to a terminal task reopens it in one transaction.
//!
//! The Postgres tests run against a real database; the memory-store test runs
//! the same contract against `dhi_testkit::MemoryStore`, which keeps the two
//! implementations honest. `set_task_state_guarded` is checked alongside the
//! carve-out: it must keep refusing terminal writes.

use dhi_core::{GoalState, GraphStore, ReopenOutcome, TaskState};
use dhi_store::Store;
use dhi_testkit::MemoryStore;
use sqlx::PgPool;
use uuid::Uuid;

fn activity_kinds(rows: &[dhi_core::Activity]) -> Vec<&str> {
    rows.iter().map(|row| row.kind.as_str()).collect()
}

#[sqlx::test]
async fn reopen_moves_each_terminal_state_to_running(pool: PgPool) {
    let store = Store::new(pool);

    for (index, terminal) in [
        TaskState::Done,
        TaskState::Failed,
        TaskState::Timeout,
        TaskState::Cancelled,
    ]
    .into_iter()
    .enumerate()
    {
        let task = store
            .create_task(&format!("terminal {index}"), 4)
            .await
            .unwrap();
        GraphStore::set_task_state_guarded(&store, task.id, TaskState::Running)
            .await
            .unwrap();
        GraphStore::set_task_state_guarded(&store, task.id, terminal)
            .await
            .unwrap();
        let activity_before = GraphStore::list_activity(&store, task.id, None)
            .await
            .unwrap()
            .len();

        let outcome =
            GraphStore::reopen_task_with_goal(&store, task.id, format!("goal after {terminal}"))
                .await
                .unwrap();

        let ReopenOutcome::Reopened {
            task: reopened,
            goal,
        } = outcome
        else {
            panic!("a {terminal} task must reopen, got {outcome:?}");
        };
        assert_eq!(reopened.id, task.id);
        assert_eq!(reopened.state, TaskState::Running);
        assert_eq!(goal.task_id, task.id);
        assert_eq!(goal.description, format!("goal after {terminal}"));
        assert_eq!(goal.state, GoalState::Open);
        assert!(goal.proof_fact_id.is_none());
        assert!(!goal.id.is_nil());

        let stored = store.get_task(task.id).await.unwrap().unwrap();
        assert_eq!(stored.state, TaskState::Running);
        let goals = store.list_goals(task.id).await.unwrap();
        assert_eq!(goals.len(), 1);
        assert_eq!(goals[0].id, goal.id);

        let activity = GraphStore::list_activity(&store, task.id, None)
            .await
            .unwrap();
        assert_eq!(activity.len(), activity_before + 2);
        let added = &activity[activity.len() - 2];
        let reopened_row = &activity[activity.len() - 1];
        assert_eq!(added.kind, "goal_added");
        assert_eq!(
            added.message,
            format!("human added goal: {}", goal.description)
        );
        assert_eq!(added.payload["goal_id"], serde_json::json!(goal.id));
        assert_eq!(added.payload["actor"], "human");
        assert_eq!(reopened_row.kind, "task_reopened");
        assert_eq!(reopened_row.message, "task reopened by human: goal added");
        assert_eq!(reopened_row.payload["goal_id"], serde_json::json!(goal.id));
        assert_eq!(
            reopened_row.payload["previous_state"],
            serde_json::json!(terminal)
        );
    }
}

#[sqlx::test]
async fn reopen_running_task_is_not_terminal_and_writes_nothing(pool: PgPool) {
    let store = Store::new(pool);
    let task = store.create_task("still running", 3).await.unwrap();
    GraphStore::set_task_state_guarded(&store, task.id, TaskState::Running)
        .await
        .unwrap();
    let activity_before = GraphStore::list_activity(&store, task.id, None)
        .await
        .unwrap();

    let outcome = store
        .reopen_task_with_goal(task.id, "must not apply".to_string())
        .await
        .unwrap();

    let ReopenOutcome::NotTerminal(state) = outcome else {
        panic!("a running task must be refused, got {outcome:?}");
    };
    assert_eq!(state, TaskState::Running);
    assert!(store.list_goals(task.id).await.unwrap().is_empty());
    assert_eq!(
        GraphStore::list_activity(&store, task.id, None)
            .await
            .unwrap()
            .len(),
        activity_before.len()
    );
    assert_eq!(
        store.get_task(task.id).await.unwrap().unwrap().state,
        TaskState::Running
    );
}

#[sqlx::test]
async fn reopen_unknown_task_is_not_found(pool: PgPool) {
    let store = Store::new(pool);
    let missing = Uuid::new_v4();

    let outcome = store
        .reopen_task_with_goal(missing, "no task".to_string())
        .await
        .unwrap();

    assert!(matches!(outcome, ReopenOutcome::NotFound));
}

#[sqlx::test]
async fn reopen_twice_after_done_again(pool: PgPool) {
    let store = Store::new(pool);
    let task = store.create_task("reopen twice", 4).await.unwrap();
    GraphStore::set_task_state_guarded(&store, task.id, TaskState::Running)
        .await
        .unwrap();
    GraphStore::set_task_state_guarded(&store, task.id, TaskState::Done)
        .await
        .unwrap();

    let first = store
        .reopen_task_with_goal(task.id, "first added goal".to_string())
        .await
        .unwrap();
    let ReopenOutcome::Reopened {
        goal: first_goal, ..
    } = first
    else {
        panic!("the first reopen must succeed");
    };

    // The engine can finish the task again; a second human goal reopens it.
    GraphStore::set_task_state_guarded(&store, task.id, TaskState::Done)
        .await
        .unwrap();
    let second = store
        .reopen_task_with_goal(task.id, "second added goal".to_string())
        .await
        .unwrap();
    let ReopenOutcome::Reopened {
        task: reopened,
        goal: second_goal,
    } = second
    else {
        panic!("the second reopen must succeed");
    };

    assert_eq!(reopened.state, TaskState::Running);
    let goals = store.list_goals(task.id).await.unwrap();
    assert_eq!(goals.len(), 2);
    assert!(goals.iter().any(|goal| goal.id == first_goal.id));
    assert!(goals.iter().any(|goal| goal.id == second_goal.id));

    let reopened_rows: Vec<_> = GraphStore::list_activity(&store, task.id, Some("task_reopened"))
        .await
        .unwrap();
    assert_eq!(reopened_rows.len(), 2);
    assert!(
        reopened_rows
            .iter()
            .all(|row| row.payload["previous_state"] == "done")
    );
    let all_activity = GraphStore::list_activity(&store, task.id, None)
        .await
        .unwrap();
    let kinds = activity_kinds(&all_activity);
    assert_eq!(
        kinds.iter().filter(|kind| **kind == "goal_added").count(),
        2
    );
}

#[tokio::test]
async fn memory_store_reopen_matches_the_postgres_contract() {
    let store = MemoryStore::new();

    for terminal in [
        TaskState::Done,
        TaskState::Failed,
        TaskState::Timeout,
        TaskState::Cancelled,
    ] {
        let task = store.seed_task(format!("memory {terminal}"), 2);
        GraphStore::set_task_state_guarded(&store, task.id, TaskState::Running)
            .await
            .unwrap();
        GraphStore::set_task_state_guarded(&store, task.id, terminal)
            .await
            .unwrap();
        let before = store.activities(task.id).len();

        let outcome = GraphStore::reopen_task_with_goal(
            &store,
            task.id,
            format!("memory goal after {terminal}"),
        )
        .await
        .unwrap();
        let ReopenOutcome::Reopened {
            task: reopened,
            goal,
        } = outcome
        else {
            panic!("a {terminal} memory task must reopen, got {outcome:?}");
        };
        assert_eq!(reopened.state, TaskState::Running);
        assert_eq!(goal.state, GoalState::Open);
        assert_eq!(store.goals(task.id).len(), 1);
        assert_eq!(store.goals(task.id)[0].id, goal.id);

        let activities = store.activities(task.id);
        assert_eq!(activities.len(), before + 2);
        let added = &activities[activities.len() - 2];
        let reopened_row = &activities[activities.len() - 1];
        assert_eq!(added.kind, "goal_added");
        assert_eq!(added.payload["actor"], "human");
        assert_eq!(added.payload["goal_id"], serde_json::json!(goal.id));
        assert_eq!(reopened_row.kind, "task_reopened");
        assert_eq!(
            reopened_row.payload["previous_state"],
            serde_json::json!(terminal)
        );
    }

    let running = store.seed_task("memory running", 2);
    GraphStore::set_task_state_guarded(&store, running.id, TaskState::Running)
        .await
        .unwrap();
    let outcome =
        GraphStore::reopen_task_with_goal(&store, running.id, "must not apply".to_string())
            .await
            .unwrap();
    assert!(matches!(
        outcome,
        ReopenOutcome::NotTerminal(TaskState::Running)
    ));
    assert!(store.goals(running.id).is_empty());

    let outcome = GraphStore::reopen_task_with_goal(&store, Uuid::new_v4(), "no task".to_string())
        .await
        .unwrap();
    assert!(matches!(outcome, ReopenOutcome::NotFound));
}
