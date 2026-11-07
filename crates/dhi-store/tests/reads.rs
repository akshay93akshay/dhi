//! Read methods on the storage contract.
//!
//! The API reads task history and the approval queue through `GraphStore`.
//! These tests pin the contract each implementation must honor: all intent
//! states, oldest-first ordering, `limit`, and pending-only approvals across
//! every task.

use chrono::Duration;
use dhi_core::{ApprovalStatus, GraphStore, GuardStore, IntentState, ResultStatus, TaskId};
use dhi_store::Store;
use dhi_testkit::MemoryStore;
use sqlx::PgPool;
use uuid::Uuid;

/// `list_intents` returns every state, not just `open`, oldest first.
#[tokio::test]
async fn list_intents_returns_all_states() {
    let store = MemoryStore::new();
    let task = store.seed_task("forced mate");
    let origin = task.origin_fact_id;

    let a = store.seed_intent(task.id, origin, "a");
    let b = store.seed_intent(task.id, origin, "b");
    let c = store.seed_intent(task.id, origin, "c");

    let claimed = GraphStore::claim_intent(&store, a.id, "worker-1")
        .await
        .unwrap()
        .expect("open intent can be claimed");
    assert_eq!(claimed.state, IntentState::Running);
    GraphStore::finish_intent(&store, a.id, IntentState::Done)
        .await
        .unwrap();
    // `b` stays open; `c` is parked as paused.
    let paused = GraphStore::claim_intent(&store, c.id, "worker-1")
        .await
        .unwrap()
        .expect("open intent can be claimed");
    assert_eq!(paused.state, IntentState::Running);
    GraphStore::finish_intent(&store, c.id, IntentState::Paused)
        .await
        .unwrap();

    let all = GraphStore::list_intents(&store, task.id, 100)
        .await
        .unwrap();
    let states: Vec<IntentState> = all.iter().map(|i| i.state).collect();
    assert_eq!(
        states,
        vec![IntentState::Done, IntentState::Open, IntentState::Paused],
        "every state is returned"
    );
    assert_eq!(
        all.iter().map(|i| i.id).collect::<Vec<_>>(),
        vec![a.id, b.id, c.id],
        "rows are oldest first"
    );

    let limited = GraphStore::list_intents(&store, task.id, 2).await.unwrap();
    assert_eq!(limited.len(), 2, "limit is honored");

    let other = store.seed_task("another task");
    assert!(
        GraphStore::list_intents(&store, other.id, 100)
            .await
            .unwrap()
            .is_empty(),
        "another task sees none of these intents"
    );
}

/// `list_results` reads back the recorded results oldest first.
#[tokio::test]
async fn list_results_returns_rows() {
    let store = MemoryStore::new();
    let task = store.seed_task("forced mate");
    let intent = store.seed_intent(task.id, task.origin_fact_id, "mate in one");

    let first = GraphStore::record_result(
        &store,
        task.id,
        intent.id,
        "first result".to_string(),
        ResultStatus::Pending,
        serde_json::json!({"n": 1}),
    )
    .await
    .unwrap();
    let second = GraphStore::record_result(
        &store,
        task.id,
        intent.id,
        "second result".to_string(),
        ResultStatus::Accepted,
        serde_json::json!({"n": 2}),
    )
    .await
    .unwrap();

    let results = GraphStore::list_results(&store, task.id, 100)
        .await
        .unwrap();
    assert_eq!(
        results.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![first.id, second.id],
        "results are oldest first"
    );
    assert_eq!(results[1].claim, "second result");
    assert_eq!(results[1].acceptance, ResultStatus::Accepted);
    assert_eq!(results[1].evidence, serde_json::json!({"n": 2}));

    let limited = GraphStore::list_results(&store, task.id, 1).await.unwrap();
    assert_eq!(limited.len(), 1, "limit is honored");
}

/// `list_agent_runs` reads back the saved completion-call rows.
#[tokio::test]
async fn list_agent_runs_returns_rows() {
    let store = MemoryStore::new();
    let task = store.seed_task("forced mate");
    let run = dhi_testkit::fixtures::agent_run(task.id, Uuid::new_v4(), 0);

    GraphStore::save_agent_run(&store, &run).await.unwrap();

    let runs = GraphStore::list_agent_runs(&store, task.id).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, run.id);
    assert_eq!(runs[0].call_index, 0);

    let other = store.seed_task("another task");
    assert!(
        GraphStore::list_agent_runs(&store, other.id)
            .await
            .unwrap()
            .is_empty(),
        "agent runs are scoped by task"
    );
}

/// `list_pending_approvals_all` covers every task and only pending rows.
#[tokio::test]
async fn list_pending_approvals_all_returns_pending_only() {
    let store = MemoryStore::new();
    let (task_a, tool_a) = memory_tool_call(&store, "task A").await;
    let (task_b, tool_b) = memory_tool_call(&store, "task B").await;

    let pending = create_approval(&store, task_a, tool_a, "pending").await;
    let decided = create_approval(&store, task_b, tool_b, "decided").await;
    GuardStore::decide_approval(&store, decided.id, dhi_core::ApprovalDecision::Deny)
        .await
        .unwrap();

    let all = GuardStore::list_pending_approvals_all(&store)
        .await
        .unwrap();
    assert_eq!(all.len(), 1, "decided approvals are excluded");
    assert_eq!(all[0].id, pending.id);
    assert_eq!(all[0].status, ApprovalStatus::Pending);
    assert_eq!(
        all[0].task_id, task_a,
        "the pending approval is from the first task"
    );
}

// ---------- PostgreSQL ----------

#[sqlx::test]
async fn postgres_list_intents_returns_all_states(pool: PgPool) {
    let store = Store::new(pool);
    let task = store.create_task("forced mate").await.unwrap();

    let done = store
        .add_intent(task.id, task.origin_fact_id, "done intent")
        .await
        .unwrap();
    let open = store
        .add_intent(task.id, task.origin_fact_id, "open intent")
        .await
        .unwrap();
    store.claim_intent(done.id, "worker-1").await.unwrap();
    store
        .finish_intent(done.id, IntentState::Done)
        .await
        .unwrap();
    // Distinct, ordered timestamps so "oldest first" is unambiguous.
    sqlx::query("UPDATE intents SET created_at = now() - interval '2 minutes' WHERE id = $1")
        .bind(done.id)
        .execute(store.pool())
        .await
        .unwrap();

    let all = GraphStore::list_intents(&store, task.id, 100)
        .await
        .unwrap();
    assert_eq!(
        all.iter().map(|i| i.state).collect::<Vec<_>>(),
        vec![IntentState::Done, IntentState::Open]
    );
    assert_eq!(
        all.iter().map(|i| i.id).collect::<Vec<_>>(),
        vec![done.id, open.id]
    );

    assert_eq!(
        GraphStore::list_intents(&store, task.id, 1)
            .await
            .unwrap()
            .len(),
        1,
        "limit is honored"
    );
}

#[sqlx::test]
async fn postgres_list_results_returns_rows(pool: PgPool) {
    let store = Store::new(pool);
    let task = store.create_task("forced mate").await.unwrap();
    let intent = store
        .add_intent(task.id, task.origin_fact_id, "mate in one")
        .await
        .unwrap();

    let first = store
        .record_result(
            task.id,
            intent.id,
            "first result".to_string(),
            ResultStatus::Pending,
            serde_json::json!({"n": 1}),
        )
        .await
        .unwrap();
    let second = store
        .record_result(
            task.id,
            intent.id,
            "second result".to_string(),
            ResultStatus::Accepted,
            serde_json::json!({"n": 2}),
        )
        .await
        .unwrap();
    sqlx::query("UPDATE results SET created_at = now() - interval '2 minutes' WHERE id = $1")
        .bind(first.id)
        .execute(store.pool())
        .await
        .unwrap();

    let results = GraphStore::list_results(&store, task.id, 100)
        .await
        .unwrap();
    assert_eq!(
        results.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![first.id, second.id]
    );
    assert_eq!(results[1].acceptance, ResultStatus::Accepted);
    assert_eq!(results[1].evidence, serde_json::json!({"n": 2}));
}

#[sqlx::test]
async fn postgres_list_agent_runs_returns_rows(pool: PgPool) {
    let store = Store::new(pool);
    let task = store.create_task("forced mate").await.unwrap();
    let run = dhi_testkit::fixtures::agent_run(task.id, Uuid::new_v4(), 0);
    GraphStore::save_agent_run(&store, &run).await.unwrap();

    let runs = GraphStore::list_agent_runs(&store, task.id).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, run.id);
}

#[sqlx::test]
async fn postgres_list_pending_approvals_all_returns_pending_only(pool: PgPool) {
    let store = Store::new(pool);
    let (task_a, tool_a) = postgres_tool_call(&store, "task A").await;
    let (task_b, tool_b) = postgres_tool_call(&store, "task B").await;

    let pending_a = create_approval(&store, task_a, tool_a, "pending on A").await;
    let pending_b = create_approval(&store, task_b, tool_b, "pending on B").await;
    let decided = create_approval(&store, task_b, tool_b, "decided on B").await;
    GuardStore::decide_approval(&store, decided.id, dhi_core::ApprovalDecision::Allow)
        .await
        .unwrap();
    sqlx::query("UPDATE approvals SET created_at = now() - interval '2 minutes' WHERE id = $1")
        .bind(pending_a.id)
        .execute(store.pool())
        .await
        .unwrap();

    let all = GuardStore::list_pending_approvals_all(&store)
        .await
        .unwrap();
    assert_eq!(
        all.iter().map(|a| a.id).collect::<Vec<_>>(),
        vec![pending_a.id, pending_b.id],
        "pending approvals of every task, oldest first"
    );
    assert!(all.iter().all(|a| a.status == ApprovalStatus::Pending));
}

// ---------- helpers ----------

async fn memory_tool_call(store: &MemoryStore, goal: &str) -> (TaskId, Uuid) {
    let task = store.seed_task(goal);
    let intent = store.seed_intent(task.id, task.origin_fact_id, "act");
    let call = GuardStore::record_tool_call(
        store,
        task.id,
        intent.id,
        "shell".to_string(),
        serde_json::json!({"cmd": "ls"}),
    )
    .await
    .unwrap();
    (task.id, call.id)
}

async fn postgres_tool_call(store: &Store, goal: &str) -> (TaskId, Uuid) {
    let task = store.create_task(goal).await.unwrap();
    let intent = store
        .add_intent(task.id, task.origin_fact_id, "act")
        .await
        .unwrap();
    let call = GuardStore::record_tool_call(
        store,
        task.id,
        intent.id,
        "shell".to_string(),
        serde_json::json!({"cmd": "ls"}),
    )
    .await
    .unwrap();
    (task.id, call.id)
}

async fn create_approval<S: GuardStore>(
    store: &S,
    task_id: TaskId,
    tool_call_id: Uuid,
    reason: &str,
) -> dhi_core::Approval {
    let decision = GuardStore::record_guard_decision(
        store,
        tool_call_id,
        dhi_core::GuardVerdict::Ask,
        None,
        None,
        "needs a person".to_string(),
    )
    .await
    .unwrap();
    GuardStore::create_approval(
        store,
        task_id,
        tool_call_id,
        decision.id,
        reason.to_string(),
        chrono::Utc::now() + Duration::minutes(5),
    )
    .await
    .unwrap()
}
