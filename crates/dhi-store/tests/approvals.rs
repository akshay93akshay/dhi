//! Guard-choke-point and approval storage tests.
//!
//! Each `#[sqlx::test]` gets its own database. The memory-store tests run the
//! same assertions against `dhi_testkit::MemoryStore`, which keeps the two
//! implementations honest against the same contract.

use chrono::{Duration, Utc};
use dhi_core::{
    Approval, ApprovalDecision, ApprovalStatus, GraphStore, GuardStore, GuardVerdict, IntentState,
    TaskId,
};
use dhi_store::{Store, StoreError};
use dhi_testkit::MemoryStore;
use sqlx::PgPool;
use uuid::Uuid;

#[sqlx::test]
async fn approval_flow_records_and_decides(pool: PgPool) {
    let store = Store::new(pool);
    let (task, intent) = seeded_task_and_intent(&store).await;

    let call = GuardStore::record_tool_call(
        &store,
        task.id,
        intent.id,
        "shell".to_string(),
        serde_json::json!({"cmd": "ls"}),
    )
    .await
    .unwrap();
    assert_eq!(call.status, "pending");
    assert!(call.output.is_none());
    assert_eq!(
        call.intent_id, intent.id,
        "the tool call is linked to its intent at record time"
    );

    let fetched = GuardStore::get_tool_call(&store, call.id)
        .await
        .unwrap()
        .expect("the recorded call exists");
    assert_eq!(fetched.id, call.id);
    assert_eq!(fetched.tool_name, "shell");
    assert_eq!(fetched.input, serde_json::json!({"cmd": "ls"}));
    assert_eq!(fetched.intent_id, intent.id);

    let decision = GuardStore::record_guard_decision(
        &store,
        call.id,
        GuardVerdict::Ask,
        Some("requires-human".to_string()),
        Some("v1".to_string()),
        "needs a person".to_string(),
    )
    .await
    .unwrap();
    assert_eq!(decision.tool_call_id, call.id);
    assert_eq!(decision.verdict, GuardVerdict::Ask);
    assert_eq!(decision.matched_rule.as_deref(), Some("requires-human"));

    let approval = GuardStore::create_approval(
        &store,
        task.id,
        call.id,
        decision.id,
        "approve the shell call".to_string(),
        Utc::now() + Duration::minutes(5),
    )
    .await
    .unwrap();
    assert_eq!(approval.status, ApprovalStatus::Pending);
    assert!(approval.decided_at.is_none());

    let pending = GuardStore::list_pending_approvals(&store, task.id)
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, approval.id);

    let decided = GuardStore::decide_approval(&store, approval.id, ApprovalDecision::Allow)
        .await
        .unwrap()
        .expect("a pending approval can be decided");
    assert_eq!(decided.status, ApprovalStatus::Granted);
    assert!(decided.decided_at.is_some());

    assert!(
        GuardStore::list_pending_approvals(&store, task.id)
            .await
            .unwrap()
            .is_empty(),
        "a decided approval is no longer pending"
    );
    assert!(
        GuardStore::decide_approval(&store, approval.id, ApprovalDecision::Allow)
            .await
            .unwrap()
            .is_none(),
        "an approval can only be decided once"
    );

    let denied = create_pending_approval(&store, task.id, intent.id, "second call").await;
    let decided = GuardStore::decide_approval(&store, denied.id, ApprovalDecision::Deny)
        .await
        .unwrap()
        .expect("a pending approval can be denied");
    assert_eq!(decided.status, ApprovalStatus::Denied);
    assert!(decided.decided_at.is_some());

    assert!(
        GuardStore::decide_approval(&store, Uuid::new_v4(), ApprovalDecision::Allow)
            .await
            .unwrap()
            .is_none(),
        "an unknown approval returns None"
    );

    GuardStore::complete_tool_call(
        &store,
        call.id,
        Some(serde_json::json!({"ok": true})),
        "succeeded",
    )
    .await
    .unwrap();
    let completed = GuardStore::get_tool_call(&store, call.id)
        .await
        .unwrap()
        .expect("the completed call exists");
    assert_eq!(completed.status, "succeeded");
    assert_eq!(completed.output, Some(serde_json::json!({"ok": true})));
}

#[sqlx::test]
async fn expired_approval_is_marked_expired(pool: PgPool) {
    let store = Store::new(pool);
    let (task, intent) = seeded_task_and_intent(&store).await;

    let call = GuardStore::record_tool_call(
        &store,
        task.id,
        intent.id,
        "shell".to_string(),
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let decision = GuardStore::record_guard_decision(
        &store,
        call.id,
        GuardVerdict::Ask,
        None,
        None,
        "needs a person".to_string(),
    )
    .await
    .unwrap();
    let approval = GuardStore::create_approval(
        &store,
        task.id,
        call.id,
        decision.id,
        "expired before the decision".to_string(),
        Utc::now() - Duration::minutes(1),
    )
    .await
    .unwrap();

    let decided = GuardStore::decide_approval(&store, approval.id, ApprovalDecision::Allow)
        .await
        .unwrap()
        .expect("an expired pending approval still returns its row");
    assert_eq!(decided.status, ApprovalStatus::Expired);
    assert!(
        decided.decided_at.is_none(),
        "expiry is not a human decision"
    );

    let stored = GuardStore::get_approval(&store, approval.id)
        .await
        .unwrap()
        .expect("the approval row exists");
    assert_eq!(stored.status, ApprovalStatus::Expired);
    assert!(stored.decided_at.is_none());

    assert!(
        GuardStore::list_pending_approvals(&store, task.id)
            .await
            .unwrap()
            .is_empty(),
        "an expired approval is no longer pending"
    );
    assert!(
        GuardStore::decide_approval(&store, approval.id, ApprovalDecision::Deny)
            .await
            .unwrap()
            .is_none(),
        "an expired approval cannot be decided again"
    );
}

#[sqlx::test]
async fn concurrent_decide_has_one_winner(pool: PgPool) {
    let store = Store::new(pool);
    let (task, intent) = seeded_task_and_intent(&store).await;
    let approval =
        create_pending_approval(&store, task.id, intent.id, "race for the same approval").await;

    // Eight decisions race on one pending row. The store locks the row, so the
    // first writer wins and the rest observe a non-pending status.
    let results = futures::future::join_all(
        (0..8).map(|_| GuardStore::decide_approval(&store, approval.id, ApprovalDecision::Allow)),
    )
    .await;
    let results: Vec<_> = results
        .into_iter()
        .map(|result| result.expect("a concurrent decision must not error"))
        .collect();

    assert_eq!(
        results.len(),
        8,
        "every concurrent decision returns a row or None"
    );

    let winners: Vec<_> = results.iter().flatten().collect();
    assert_eq!(winners.len(), 1, "exactly one decision may win the race");
    assert_eq!(winners[0].id, approval.id);
    assert_eq!(winners[0].status, ApprovalStatus::Granted);
    assert!(
        winners[0].decided_at.is_some(),
        "the winning decision is a human decision"
    );

    assert_eq!(
        results.iter().filter(|result| result.is_none()).count(),
        7,
        "the losing decisions return None"
    );

    let granted: i64 =
        sqlx::query_scalar("SELECT count(*) FROM approvals WHERE status = 'granted'")
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(granted, 1, "only one row may be granted");

    let pending: i64 =
        sqlx::query_scalar("SELECT count(*) FROM approvals WHERE status = 'pending'")
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(pending, 0, "the approval is no longer pending");
}

#[sqlx::test]
async fn expire_pending_approvals_is_scoped_by_task(pool: PgPool) {
    let store = Store::new(pool);
    let (task_a, intent_a) = seeded_task_and_intent(&store).await;
    let path_b = store.create_task("second task", 4).await.unwrap();
    let intent_b = store
        .add_intent(path_b.id, path_b.origin_fact_id, "call a tool")
        .await
        .unwrap();

    let overdue_a = create_pending_approval_with_ttl(
        &store,
        task_a.id,
        intent_a.id,
        "overdue in A",
        Utc::now() - Duration::seconds(1),
    )
    .await;
    let overdue_b = create_pending_approval_with_ttl(
        &store,
        path_b.id,
        intent_b.id,
        "overdue in B",
        Utc::now() - Duration::seconds(1),
    )
    .await;

    let swept = GuardStore::expire_pending_approvals(&store, task_a.id, Utc::now())
        .await
        .unwrap();
    assert_eq!(swept.len(), 1);
    assert_eq!(swept[0].id, overdue_a.id);
    assert_eq!(swept[0].task_id, task_a.id);

    let other = GuardStore::get_approval(&store, overdue_b.id)
        .await
        .unwrap()
        .expect("the other approval exists");
    assert_eq!(
        other.status,
        ApprovalStatus::Pending,
        "a sweep for one task must not expire another task's approval"
    );

    let swept = GuardStore::expire_pending_approvals(&store, path_b.id, Utc::now())
        .await
        .unwrap();
    assert_eq!(swept.len(), 1);
    assert_eq!(swept[0].id, overdue_b.id);
}

#[sqlx::test]
async fn concurrent_expire_and_decide_have_one_winner(pool: PgPool) {
    let store = Store::new(pool);
    let (task, intent) = seeded_task_and_intent(&store).await;
    let approval = create_pending_approval_with_ttl(
        &store,
        task.id,
        intent.id,
        "expires while a person decides",
        Utc::now() + Duration::seconds(2),
    )
    .await;

    // The sweep and a human decision race on the same pending row. The row is
    // locked in both paths, so exactly one writer wins. The approval is still
    // valid for the decision, while the sweep is given a clock past the TTL.
    let (swept, decided) = tokio::join!(
        GuardStore::expire_pending_approvals(&store, task.id, Utc::now() + Duration::minutes(1)),
        GuardStore::decide_approval(&store, approval.id, ApprovalDecision::Allow),
    );
    let swept = swept.expect("the sweep must not error");
    let decided = decided.expect("the decision must not error");

    let sweep_won = swept.iter().any(|a| a.id == approval.id);
    let decide_won = decided
        .as_ref()
        .is_some_and(|a| a.status == ApprovalStatus::Granted);
    assert!(
        sweep_won ^ decide_won,
        "exactly one of expiry and decision may win (swept={sweep_won}, decided={decide_won})"
    );

    let stored = GuardStore::get_approval(&store, approval.id)
        .await
        .unwrap()
        .expect("the approval row exists");
    match stored.status {
        ApprovalStatus::Expired => assert!(
            stored.decided_at.is_none(),
            "expiry is not a human decision and must not stamp decided_at"
        ),
        ApprovalStatus::Granted => assert!(
            stored.decided_at.is_some(),
            "a granted approval records when it was decided"
        ),
        other => panic!("unexpected status after the race: {other:?}"),
    }
    assert_eq!(
        stored.status,
        if sweep_won {
            ApprovalStatus::Expired
        } else {
            ApprovalStatus::Granted
        },
        "the stored status matches the winner"
    );
    assert_eq!(
        stored.decided_at.is_some(),
        decide_won,
        "decided_at belongs only to the decision winner"
    );
}

#[sqlx::test]
async fn terminal_task_rejects_every_public_write(pool: PgPool) {
    let store = Store::new(pool);
    let task = store.create_task("settle once", 4).await.unwrap();
    assert!(
        GraphStore::set_task_state_guarded(&store, task.id, dhi_core::TaskState::Running)
            .await
            .unwrap()
    );
    assert!(
        store
            .set_task_state_guarded(task.id, dhi_core::TaskState::Done)
            .await
            .unwrap()
    );

    // The trait surface has exactly one setter. Every target it can be given
    // is refused once the stored state is terminal, including non-terminal
    // ones such as `running` and `paused`.
    for target in [
        dhi_core::TaskState::Created,
        dhi_core::TaskState::Running,
        dhi_core::TaskState::Paused,
        dhi_core::TaskState::Failed,
        dhi_core::TaskState::Timeout,
        dhi_core::TaskState::Cancelled,
    ] {
        let changed = GraphStore::set_task_state_guarded(&store, task.id, target)
            .await
            .unwrap();
        assert!(!changed, "a terminal task must refuse a write to {target}");
    }
    // The inherent method refuses the same way.
    assert!(
        !store
            .set_task_state_guarded(task.id, dhi_core::TaskState::Running)
            .await
            .unwrap()
    );

    let state: String = sqlx::query_scalar("SELECT state FROM tasks WHERE id = $1")
        .bind(task.id)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(state, "done", "the first terminal status stays");
}

#[sqlx::test]
async fn reset_running_intents_releases_abandoned_work(pool: PgPool) {
    let store = Store::new(pool);
    let task_a = store.create_task("restart A", 4).await.unwrap();
    let task_b = store.create_task("restart B", 4).await.unwrap();
    let running_a = store
        .add_intent(task_a.id, task_a.origin_fact_id, "in flight")
        .await
        .unwrap();
    let paused_a = store
        .add_intent(task_a.id, task_a.origin_fact_id, "waiting on a person")
        .await
        .unwrap();
    let running_b = store
        .add_intent(task_b.id, task_b.origin_fact_id, "in flight elsewhere")
        .await
        .unwrap();

    GraphStore::claim_intent(&store, running_a.id, "worker-1")
        .await
        .unwrap()
        .expect("an open intent is claimable");
    GraphStore::claim_intent(&store, paused_a.id, "worker-2")
        .await
        .unwrap()
        .expect("an open intent is claimable");
    GraphStore::finish_intent(&store, paused_a.id, IntentState::Paused)
        .await
        .unwrap();
    GraphStore::claim_intent(&store, running_b.id, "worker-3")
        .await
        .unwrap()
        .expect("an open intent is claimable");

    let reset = GraphStore::reset_running_intents(&store).await.unwrap();
    assert_eq!(reset, 2, "both running intents across tasks are reset");

    let open_a = GraphStore::list_open_intents(&store, task_a.id, 10)
        .await
        .unwrap();
    let released = open_a
        .iter()
        .find(|i| i.id == running_a.id)
        .expect("the released intent is open again");
    assert!(
        released.owner.is_none(),
        "restart recovery clears the dead owner"
    );
    assert!(
        !open_a.iter().any(|i| i.id == paused_a.id),
        "a paused intent is not restart recovery's business"
    );

    let open_b = GraphStore::list_open_intents(&store, task_b.id, 10)
        .await
        .unwrap();
    assert!(open_b.iter().any(|i| i.id == running_b.id));

    // A second reset has nothing left to do.
    assert_eq!(GraphStore::reset_running_intents(&store).await.unwrap(), 0);
}

#[tokio::test]
async fn memory_store_resets_running_intents_on_restart() {
    let store = MemoryStore::new();
    let task = store.seed_task("restart", 4);
    let running = store.seed_intent(task.id, task.origin_fact_id, "in flight");
    GraphStore::claim_intent(&store, running.id, "worker-1")
        .await
        .unwrap()
        .expect("an open intent is claimable");

    assert_eq!(GraphStore::reset_running_intents(&store).await.unwrap(), 1);
    let intent = store
        .intents(task.id)
        .into_iter()
        .find(|i| i.id == running.id)
        .expect("the intent exists");
    assert_eq!(intent.state, IntentState::Open);
    assert!(intent.owner.is_none());
}

#[sqlx::test]
async fn expire_pending_approvals_is_timer_driven(pool: PgPool) {
    let store = Store::new(pool);
    let (task, intent) = seeded_task_and_intent(&store).await;

    let due = create_pending_approval_with_ttl(
        &store,
        task.id,
        intent.id,
        "overdue",
        Utc::now() - Duration::seconds(1),
    )
    .await;
    let later = create_pending_approval_with_ttl(
        &store,
        task.id,
        intent.id,
        "not due yet",
        Utc::now() + Duration::minutes(5),
    )
    .await;

    // Nothing is due yet at `now - 1 minute`.
    let swept =
        GuardStore::expire_pending_approvals(&store, task.id, Utc::now() - Duration::minutes(1))
            .await
            .unwrap();
    assert!(swept.is_empty(), "future approvals must not expire");

    let swept = GuardStore::expire_pending_approvals(&store, task.id, Utc::now())
        .await
        .unwrap();
    assert_eq!(swept.len(), 1, "only the overdue approval expires");
    assert_eq!(swept[0].id, due.id);
    assert_eq!(swept[0].status, ApprovalStatus::Expired);
    assert!(
        swept[0].decided_at.is_none(),
        "expiry is not a human decision"
    );

    let stored = GuardStore::get_approval(&store, later.id)
        .await
        .unwrap()
        .expect("the other approval exists");
    assert_eq!(stored.status, ApprovalStatus::Pending);

    // A second sweep is idempotent.
    assert!(
        GuardStore::expire_pending_approvals(&store, task.id, Utc::now())
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test]
async fn tool_call_requires_an_existing_intent(pool: PgPool) {
    let store = Store::new(pool);
    let task = store.create_task("guard", 4).await.unwrap();

    let err = GuardStore::record_tool_call(
        &store,
        task.id,
        Uuid::new_v4(),
        "shell".to_string(),
        serde_json::json!({}),
    )
    .await
    .expect_err("a tool call without a real intent must be rejected");

    assert_eq!(
        sqlstate(&err).as_deref(),
        Some("23503"),
        "a missing intent must be a foreign key violation, got {err:?}"
    );
    let message = format!("{err:#}");
    assert!(
        message.contains("tool_calls_intent_id_fkey"),
        "the link is enforced by the tool call's intent foreign key, got {message}"
    );
}

#[sqlx::test]
async fn paused_intent_cannot_finish_directly(pool: PgPool) {
    let store = Store::new(pool);
    let (task, intent) = seeded_task_and_intent(&store).await;

    let claimed = GraphStore::claim_intent(&store, intent.id, "worker-1")
        .await
        .unwrap()
        .expect("an open intent is claimable");
    assert_eq!(claimed.state, IntentState::Running);

    GraphStore::finish_intent(&store, intent.id, IntentState::Paused)
        .await
        .unwrap();
    let paused: String = sqlx::query_scalar("SELECT state FROM intents WHERE id = $1")
        .bind(intent.id)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(paused, "paused");

    // A paused intent cannot finish directly; it must be resumed first.
    GraphStore::finish_intent(&store, intent.id, IntentState::Done)
        .await
        .unwrap();
    let still_paused: String = sqlx::query_scalar("SELECT state FROM intents WHERE id = $1")
        .bind(intent.id)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(still_paused, "paused");
    assert!(
        !IntentState::Paused.can_transition_to(IntentState::Stopped),
        "paused -> stopped is not a legal intent transition"
    );

    let resumed = GraphStore::resume_intent(&store, intent.id, "expiry")
        .await
        .unwrap()
        .expect("a paused intent resumes");
    assert_eq!(resumed.state, IntentState::Running);
    assert_eq!(resumed.owner.as_deref(), Some("expiry"));
    assert!(
        GraphStore::resume_intent(&store, intent.id, "expiry")
            .await
            .unwrap()
            .is_none(),
        "only a paused intent can resume"
    );

    GraphStore::finish_intent(&store, intent.id, IntentState::Blocked)
        .await
        .unwrap();
    let blocked: String = sqlx::query_scalar("SELECT state FROM intents WHERE id = $1")
        .bind(intent.id)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(blocked, "blocked");
    assert_eq!(task.id, intent.task_id);
}

#[sqlx::test]
async fn terminal_status_is_first_writer_wins(pool: PgPool) {
    let store = Store::new(pool);
    let task = store.create_task("settle once", 4).await.unwrap();
    GraphStore::set_task_state_guarded(&store, task.id, dhi_core::TaskState::Running)
        .await
        .unwrap();

    let changed = GraphStore::set_task_state_guarded(&store, task.id, dhi_core::TaskState::Done)
        .await
        .unwrap();
    assert!(changed, "running -> done applies");

    let changed = GraphStore::set_task_state_guarded(&store, task.id, dhi_core::TaskState::Failed)
        .await
        .unwrap();
    assert!(!changed, "done -> failed must not apply");

    let changed = GraphStore::set_task_state_guarded(&store, task.id, dhi_core::TaskState::Timeout)
        .await
        .unwrap();
    assert!(!changed, "done -> timeout must not apply");

    let state: String = sqlx::query_scalar("SELECT state FROM tasks WHERE id = $1")
        .bind(task.id)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(state, "done", "the first terminal status stays");
}

#[tokio::test]
async fn memory_store_implements_the_same_guard_flow() {
    let store = MemoryStore::new();
    let task = store.seed_task("guard", 4);
    let intent = store.seed_intent(task.id, task.origin_fact_id, "call a tool");

    let call = GuardStore::record_tool_call(
        &store,
        task.id,
        intent.id,
        "shell".to_string(),
        serde_json::json!({"cmd": "ls"}),
    )
    .await
    .unwrap();
    let decision = GuardStore::record_guard_decision(
        &store,
        call.id,
        GuardVerdict::Ask,
        None,
        None,
        "needs a person".to_string(),
    )
    .await
    .unwrap();

    let approval = GuardStore::create_approval(
        &store,
        task.id,
        call.id,
        decision.id,
        "approve".to_string(),
        Utc::now() + Duration::minutes(5),
    )
    .await
    .unwrap();
    assert_eq!(
        GuardStore::list_pending_approvals(&store, task.id)
            .await
            .unwrap()
            .len(),
        1
    );

    let decided = GuardStore::decide_approval(&store, approval.id, ApprovalDecision::Deny)
        .await
        .unwrap()
        .expect("a pending approval can be decided");
    assert_eq!(decided.status, ApprovalStatus::Denied);
    assert!(decided.decided_at.is_some());
    assert!(
        GuardStore::decide_approval(&store, approval.id, ApprovalDecision::Deny)
            .await
            .unwrap()
            .is_none()
    );

    let expired = GuardStore::create_approval(
        &store,
        task.id,
        call.id,
        decision.id,
        "expired".to_string(),
        Utc::now() - Duration::minutes(1),
    )
    .await
    .unwrap();
    let expired = GuardStore::decide_approval(&store, expired.id, ApprovalDecision::Allow)
        .await
        .unwrap()
        .expect("an expired approval still returns its row");
    assert_eq!(expired.status, ApprovalStatus::Expired);
    assert!(expired.decided_at.is_none());

    // The memory store sweeps due approvals the same way as PostgreSQL.
    let due = GuardStore::create_approval(
        &store,
        task.id,
        call.id,
        decision.id,
        "due".to_string(),
        Utc::now() - Duration::seconds(1),
    )
    .await
    .unwrap();
    let swept = GuardStore::expire_pending_approvals(&store, task.id, Utc::now())
        .await
        .unwrap();
    assert_eq!(swept.len(), 1);
    assert_eq!(swept[0].id, due.id);
    assert_eq!(swept[0].status, ApprovalStatus::Expired);
}

#[tokio::test]
async fn memory_store_enforces_the_paused_and_terminal_rules() {
    let store = MemoryStore::new();
    let task = store.seed_task("rules", 4);
    let intent = store.seed_intent(task.id, task.origin_fact_id, "one step");

    GraphStore::claim_intent(&store, intent.id, "worker-1")
        .await
        .unwrap()
        .expect("an open intent is claimable");
    GraphStore::finish_intent(&store, intent.id, IntentState::Paused)
        .await
        .unwrap();
    GraphStore::finish_intent(&store, intent.id, IntentState::Done)
        .await
        .unwrap();
    assert_eq!(
        store.intents(task.id)[0].state,
        IntentState::Paused,
        "a paused intent cannot finish directly"
    );
    let resumed = GraphStore::resume_intent(&store, intent.id, "expiry")
        .await
        .unwrap()
        .expect("a paused intent resumes");
    assert_eq!(resumed.state, IntentState::Running);

    GraphStore::set_task_state_guarded(&store, task.id, dhi_core::TaskState::Running)
        .await
        .unwrap();
    assert!(
        GraphStore::set_task_state_guarded(&store, task.id, dhi_core::TaskState::Done)
            .await
            .unwrap()
    );
    assert!(
        !GraphStore::set_task_state_guarded(&store, task.id, dhi_core::TaskState::Timeout)
            .await
            .unwrap()
    );
    assert_eq!(
        store.get_task(task.id).await.unwrap().unwrap().state,
        dhi_core::TaskState::Done
    );
}

/// Resume and finish in one statement: `paused -> running -> done`, with no
/// intermediate `running` row left behind.
#[sqlx::test]
async fn finish_paused_intent_moves_paused_to_terminal(pool: PgPool) {
    let store = Store::new(pool);
    let (task, intent) = seeded_task_and_intent(&store).await;
    GraphStore::claim_intent(&store, intent.id, "worker-1")
        .await
        .unwrap()
        .expect("an open intent is claimable");
    GraphStore::finish_intent(&store, intent.id, IntentState::Paused)
        .await
        .unwrap();

    let finished =
        GraphStore::finish_paused_intent(&store, intent.id, "approval", IntentState::Done)
            .await
            .unwrap()
            .expect("a paused intent finishes atomically");
    assert_eq!(finished.state, IntentState::Done);
    assert_eq!(finished.owner.as_deref(), Some("approval"));
    assert!(
        finished.completed_at.is_some(),
        "the final state records when it finished"
    );

    let stored: String = sqlx::query_scalar("SELECT state FROM intents WHERE id = $1")
        .bind(intent.id)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(stored, "done");

    // A finished intent cannot finish twice, and an open one never can.
    assert!(
        GraphStore::finish_paused_intent(&store, intent.id, "approval", IntentState::Done)
            .await
            .unwrap()
            .is_none()
    );
    let open = store
        .add_intent(task.id, task.origin_fact_id, "still open")
        .await
        .unwrap();
    assert!(
        GraphStore::finish_paused_intent(&store, open.id, "approval", IntentState::Done)
            .await
            .unwrap()
            .is_none(),
        "a non-paused intent returns None"
    );
    assert_eq!(
        GraphStore::list_open_intents(&store, task.id, 10)
            .await
            .unwrap()
            .iter()
            .filter(|i| i.id == open.id)
            .count(),
        1
    );
}

/// The resume and the finish happen in one statement, so two concurrent
/// finishers cannot both win.
#[sqlx::test]
async fn finish_paused_intent_is_atomic(pool: PgPool) {
    let store = Store::new(pool);
    let (task, intent) = seeded_task_and_intent(&store).await;
    GraphStore::claim_intent(&store, intent.id, "worker-1")
        .await
        .unwrap()
        .expect("an open intent is claimable");
    GraphStore::finish_intent(&store, intent.id, IntentState::Paused)
        .await
        .unwrap();

    let results = futures::future::join_all((0..8).map(|i| {
        let status = if i % 2 == 0 {
            IntentState::Done
        } else {
            IntentState::Blocked
        };
        GraphStore::finish_paused_intent(&store, intent.id, "approval", status)
    }))
    .await;

    let winners: Vec<_> = results
        .into_iter()
        .filter_map(|result| result.expect("a concurrent finish must not error"))
        .collect();
    assert_eq!(winners.len(), 1, "exactly one finish may win the race");

    let stored: (String, Option<chrono::DateTime<Utc>>) =
        sqlx::query_as("SELECT state, completed_at FROM intents WHERE id = $1")
            .bind(intent.id)
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(
        stored.0,
        winners[0].state.as_str(),
        "the final state is the winner's"
    );
    assert!(stored.1.is_some(), "the winner stamps completed_at");
    assert!(winners[0].state.is_terminal());
    assert_eq!(task.id, intent.task_id);
}

/// The expiry owner blocks a paused intent in one atomic move.
#[sqlx::test]
async fn block_intent_for_approval_moves_paused_to_blocked(pool: PgPool) {
    let store = Store::new(pool);
    let (task, intent) = seeded_task_and_intent(&store).await;
    GraphStore::claim_intent(&store, intent.id, "worker-1")
        .await
        .unwrap()
        .expect("an open intent is claimable");
    GraphStore::finish_intent(&store, intent.id, IntentState::Paused)
        .await
        .unwrap();
    let approval = create_pending_approval_with_ttl(
        &store,
        task.id,
        intent.id,
        "expired",
        Utc::now() - Duration::seconds(1),
    )
    .await;

    assert!(
        GuardStore::block_intent_for_approval(&store, approval.id, "expiry")
            .await
            .unwrap(),
        "a paused intent behind an approval blocks"
    );
    let stored: String = sqlx::query_scalar("SELECT state FROM intents WHERE id = $1")
        .bind(intent.id)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(stored, "blocked");

    assert!(
        !GuardStore::block_intent_for_approval(&store, approval.id, "expiry")
            .await
            .unwrap(),
        "the intent is no longer paused"
    );
    assert!(
        !GuardStore::block_intent_for_approval(&store, Uuid::new_v4(), "expiry")
            .await
            .unwrap(),
        "an unknown approval returns false"
    );
}

/// Concurrent blockers on one approval: the move is atomic, so exactly one
/// reports a change.
#[sqlx::test]
async fn block_intent_for_approval_is_atomic(pool: PgPool) {
    let store = Store::new(pool);
    let (task, intent) = seeded_task_and_intent(&store).await;
    GraphStore::claim_intent(&store, intent.id, "worker-1")
        .await
        .unwrap()
        .expect("an open intent is claimable");
    GraphStore::finish_intent(&store, intent.id, IntentState::Paused)
        .await
        .unwrap();
    let approval = create_pending_approval_with_ttl(
        &store,
        task.id,
        intent.id,
        "racing timers",
        Utc::now() - Duration::seconds(1),
    )
    .await;

    let results = futures::future::join_all((0..2).map(|i| {
        let owner = if i == 0 { "expiry" } else { "supervisor" };
        GuardStore::block_intent_for_approval(&store, approval.id, owner)
    }))
    .await;
    let results: Vec<bool> = results
        .into_iter()
        .map(|result| result.expect("a concurrent block must not error"))
        .collect();

    assert_eq!(
        results.iter().filter(|moved| **moved).count(),
        1,
        "exactly one caller may block the intent: {results:?}"
    );
    let stored: String = sqlx::query_scalar("SELECT state FROM intents WHERE id = $1")
        .bind(intent.id)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(stored, "blocked");
}

/// A paused intent whose tool call has no pending approval is orphaned: the
/// pass blocks it. One that still waits on a pending approval is left alone.
#[sqlx::test]
async fn orphaned_paused_intent_is_blocked(pool: PgPool) {
    let store = Store::new(pool);
    let (task, waiting) = seeded_task_and_intent(&store).await;
    for intent in [&waiting] {
        GraphStore::claim_intent(&store, intent.id, "worker-1")
            .await
            .unwrap()
            .expect("an open intent is claimable");
        GraphStore::finish_intent(&store, intent.id, IntentState::Paused)
            .await
            .unwrap();
    }
    // The waiting intent has a pending approval: not orphaned.
    create_pending_approval(&store, task.id, waiting.id, "still waiting").await;

    // An orphan: a paused intent whose tool call has no approval at all.
    let orphan = store
        .add_intent(task.id, task.origin_fact_id, "orphaned")
        .await
        .unwrap();
    GraphStore::claim_intent(&store, orphan.id, "worker-2")
        .await
        .unwrap()
        .expect("an open intent is claimable");
    GraphStore::finish_intent(&store, orphan.id, IntentState::Paused)
        .await
        .unwrap();
    GuardStore::record_tool_call(
        &store,
        task.id,
        orphan.id,
        "shell".to_string(),
        serde_json::json!({}),
    )
    .await
    .unwrap();

    let blocked = GraphStore::block_orphaned_paused_intents(&store, task.id)
        .await
        .unwrap();
    assert_eq!(blocked, 1, "only the orphan is blocked");

    let states: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, state FROM intents WHERE task_id = $1")
            .bind(task.id)
            .fetch_all(store.pool())
            .await
            .unwrap();
    let state_of = |id: Uuid| {
        states
            .iter()
            .find(|(row_id, _)| *row_id == id)
            .map(|(_, state)| state.as_str())
            .expect("the intent exists")
    };
    assert_eq!(state_of(orphan.id), "blocked");
    assert_eq!(
        state_of(waiting.id),
        "paused",
        "an intent still waiting on a pending approval must stay paused"
    );

    // Idempotent: nothing paused is orphaned any more.
    assert_eq!(
        GraphStore::block_orphaned_paused_intents(&store, task.id)
            .await
            .unwrap(),
        0
    );
}

/// The memory store follows the same three rules.
#[tokio::test]
async fn memory_store_finishes_atomically_and_blocks_orphans() {
    let store = MemoryStore::new();
    let task = store.seed_task("memory rules", 4);
    let intent = store.seed_intent(task.id, task.origin_fact_id, "one step");
    GraphStore::claim_intent(&store, intent.id, "worker-1")
        .await
        .unwrap()
        .expect("an open intent is claimable");
    GraphStore::finish_intent(&store, intent.id, IntentState::Paused)
        .await
        .unwrap();

    let finished =
        GraphStore::finish_paused_intent(&store, intent.id, "approval", IntentState::Done)
            .await
            .unwrap()
            .expect("a paused intent finishes atomically");
    assert_eq!(finished.state, IntentState::Done);
    assert!(finished.completed_at.is_some());
    assert!(
        GraphStore::finish_paused_intent(&store, intent.id, "approval", IntentState::Done)
            .await
            .unwrap()
            .is_none(),
        "a finished intent cannot finish twice"
    );

    // An orphan with a tool call but no approval is blocked.
    let orphan = store.seed_intent(task.id, task.origin_fact_id, "orphan");
    GraphStore::claim_intent(&store, orphan.id, "worker-2")
        .await
        .unwrap()
        .expect("an open intent is claimable");
    GraphStore::finish_intent(&store, orphan.id, IntentState::Paused)
        .await
        .unwrap();
    GuardStore::record_tool_call(
        &store,
        task.id,
        orphan.id,
        "shell".to_string(),
        serde_json::json!({}),
    )
    .await
    .unwrap();
    assert_eq!(
        GraphStore::block_orphaned_paused_intents(&store, task.id)
            .await
            .unwrap(),
        1
    );
    let blocked = store
        .intents(task.id)
        .into_iter()
        .find(|i| i.id == orphan.id)
        .expect("the orphan exists");
    assert_eq!(blocked.state, IntentState::Blocked);
}

/// SQLSTATE of the database error at the root of an `anyhow` chain.
fn sqlstate(err: &anyhow::Error) -> Option<String> {
    let store_error = err.downcast_ref::<StoreError>()?;
    match store_error {
        StoreError::Sqlx(sqlx::Error::Database(db)) => db.code().map(|code| code.into_owned()),
        _ => None,
    }
}

async fn seeded_task_and_intent(store: &Store) -> (dhi_core::Task, dhi_core::Intent) {
    let task = store.create_task("guard", 4).await.unwrap();
    let intent = store
        .add_intent(task.id, task.origin_fact_id, "call a tool")
        .await
        .unwrap();
    (task, intent)
}

/// Seed a second tool call with an `Ask` decision and a pending approval.
async fn create_pending_approval(
    store: &Store,
    task_id: TaskId,
    intent_id: dhi_core::IntentId,
    reason: &str,
) -> Approval {
    create_pending_approval_with_ttl(
        store,
        task_id,
        intent_id,
        reason,
        Utc::now() + Duration::minutes(5),
    )
    .await
}

async fn create_pending_approval_with_ttl(
    store: &Store,
    task_id: TaskId,
    intent_id: dhi_core::IntentId,
    reason: &str,
    expires_at: chrono::DateTime<Utc>,
) -> Approval {
    let call = GuardStore::record_tool_call(
        store,
        task_id,
        intent_id,
        "shell".to_string(),
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let decision = GuardStore::record_guard_decision(
        store,
        call.id,
        GuardVerdict::Ask,
        None,
        None,
        reason.to_string(),
    )
    .await
    .unwrap();
    GuardStore::create_approval(
        store,
        task_id,
        call.id,
        decision.id,
        reason.to_string(),
        expires_at,
    )
    .await
    .unwrap()
}
