//! PostgreSQL round-trip tests for the Swarm team repository.
//!
//! Each `#[sqlx::test]` gets its own database. The test asserts that every
//! team record survives a write/read cycle and that the board, mailbox, run,
//! and outcome state machines reject invalid transitions.

use dhi_core::team::{
    MissionLogKind, NewMissionLogEntry, NewTeamMessage, NewTeamOutcome, NewTeamOutcomeFragment,
    NewTeamRun, NewTeamTask, TeamMember, TeamMemberStatus, TeamOutcomeFragmentStatus,
    TeamOutcomeStatus, TeamRole, TeamRunProgress, TeamRunStatus, TeamStore, TeamTaskStatus,
};
use dhi_store::Store;
use sqlx::PgPool;
use uuid::Uuid;

#[sqlx::test]
async fn team_state_round_trips(pool: PgPool) {
    let store = Store::new(pool);
    let task = store.create_task("swarm task").await.unwrap();
    let team_id = task.id;

    let lead = store
        .upsert_team_member(&TeamMember::new(
            team_id,
            "lead",
            TeamRole::Lead,
            "owns the outcome",
        ))
        .await
        .unwrap();
    assert_eq!(lead.role, TeamRole::Lead);
    let worker = store
        .upsert_team_member(&TeamMember::new(
            team_id,
            "worker-1",
            TeamRole::Teammate,
            "audits the parser",
        ))
        .await
        .unwrap();
    assert_eq!(worker.status, TeamMemberStatus::Active);

    let first_id = Uuid::new_v4();
    let first = store
        .create_team_task(
            &NewTeamTask::new(team_id, "audit the lexer", "lead")
                .with_id(first_id)
                .with_description("read the lexer"),
        )
        .await
        .unwrap();
    let second = store
        .create_team_task(
            &NewTeamTask::new(team_id, "audit the parser", "lead")
                .with_dependencies(vec![first.id]),
        )
        .await
        .unwrap();
    assert_eq!(first.status, TeamTaskStatus::Pending);
    assert!(!second.is_ready(&store.team_tasks(team_id).await.unwrap()));
    assert!(
        store
            .claim_team_task(team_id, second.id, "worker-1")
            .await
            .is_err(),
        "a dependent task cannot be claimed"
    );

    let claimed = store
        .claim_team_task(team_id, first.id, "worker-1")
        .await
        .unwrap();
    assert_eq!(claimed.status, TeamTaskStatus::InProgress);
    assert_eq!(claimed.assignee.as_deref(), Some("worker-1"));
    let completed = store
        .complete_team_task(team_id, first.id, "the lexer is sound")
        .await
        .unwrap();
    assert_eq!(completed.status, TeamTaskStatus::Completed);
    assert_eq!(completed.summary, "the lexer is sound");
    let ready = store.team_tasks(team_id).await.unwrap();
    assert!(
        ready
            .iter()
            .find(|row| row.id == second.id)
            .expect("second task")
            .is_ready(&ready)
    );

    let blocked = store
        .create_team_task(&NewTeamTask::new(team_id, "blocked task", "lead"))
        .await
        .unwrap();
    let blocked = store
        .block_team_task(team_id, blocked.id, "waiting on a decision")
        .await
        .unwrap();
    assert_eq!(blocked.status, TeamTaskStatus::Blocked);
    assert_eq!(blocked.blocked_reason, "waiting on a decision");
    let reopened = store.reopen_team_task(team_id, blocked.id).await.unwrap();
    assert_eq!(reopened.status, TeamTaskStatus::Pending);

    let direct = store
        .send_team_message(&NewTeamMessage::direct(
            team_id,
            "worker-1",
            "lead",
            "finding",
            "the parser is clean",
        ))
        .await
        .unwrap();
    assert!(!direct.is_broadcast());
    assert!(direct.is_unread());
    let broadcast = store
        .broadcast_team_message(&NewTeamMessage::broadcast(
            team_id,
            "lead",
            "status",
            "one section is ready",
        ))
        .await
        .unwrap();
    assert!(broadcast.is_broadcast());

    let lead_inbox = store.team_inbox(team_id, "lead").await.unwrap();
    assert!(
        lead_inbox.iter().any(|message| message.id == direct.id),
        "direct message reaches the lead"
    );
    let worker_inbox = store.team_inbox(team_id, "worker-1").await.unwrap();
    assert!(
        worker_inbox
            .iter()
            .any(|message| message.id == broadcast.id),
        "broadcast reaches the teammate"
    );
    let sent = store.team_sent(team_id, "worker-1").await.unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].id, direct.id);

    let read = store
        .mark_team_message_read(team_id, direct.id, "lead")
        .await
        .unwrap();
    assert!(read.read_at.is_some());
    assert!(!read.is_unread());

    let log = store
        .append_mission_log(
            &NewMissionLogEntry::new(team_id, "worker-1", MissionLogKind::Progress, "lexer done")
                .with_task(first.id)
                .with_evidence("read src/lexer.rs")
                .with_next_action("start the parser"),
        )
        .await
        .unwrap();
    assert_eq!(log.kind, MissionLogKind::Progress);
    assert_eq!(store.team_mission_log(team_id).await.unwrap().len(), 1);

    let run = store
        .enqueue_team_run(
            &NewTeamRun::new(team_id, "worker-1", "audit the parser")
                .with_task(second.id)
                .with_priority(3)
                .with_retries(1),
        )
        .await
        .unwrap();
    assert_eq!(run.status, TeamRunStatus::Queued);
    let started = store
        .start_team_run(team_id, run.id, "lease-1")
        .await
        .unwrap();
    assert_eq!(started.status, TeamRunStatus::Running);
    let heartbeat = store
        .heartbeat_team_run(
            team_id,
            run.id,
            &TeamRunProgress::new("lease-1", "half way", "reading the parser"),
        )
        .await
        .unwrap();
    assert_eq!(heartbeat.last_progress_message, "half way");
    assert_eq!(heartbeat.current_activity, "reading the parser");
    let finished = store
        .finish_team_run(
            team_id,
            run.id,
            TeamRunStatus::Completed,
            Some("the parser is clean"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(finished.status, TeamRunStatus::Completed);
    assert!(finished.ended_at.is_some());

    let retry = store
        .enqueue_team_run(
            &NewTeamRun::new(team_id, "worker-1", "retry me")
                .with_task(second.id)
                .with_retries(1),
        )
        .await
        .unwrap();
    store
        .start_team_run(team_id, retry.id, "lease-2")
        .await
        .unwrap();
    store
        .finish_team_run(
            team_id,
            retry.id,
            TeamRunStatus::Failed,
            None,
            Some("temporary failure"),
        )
        .await
        .unwrap();
    let requeued = store
        .requeue_team_run(team_id, retry.id, Some("retry with a new prompt"))
        .await
        .unwrap();
    assert_eq!(requeued.status, TeamRunStatus::Queued);
    assert_eq!(requeued.retry_count, 1);

    let outcome = store
        .create_team_outcome(
            &NewTeamOutcome::new(team_id, "parser report", "lead")
                .with_sections(vec!["summary".to_string(), "evidence".to_string()]),
        )
        .await
        .unwrap();
    assert_eq!(outcome.status, TeamOutcomeStatus::Draft);
    let summary = store
        .attach_outcome_fragment(&NewTeamOutcomeFragment::new(
            team_id,
            outcome.id,
            "summary",
            "worker-1",
            "the parser is clean",
        ))
        .await
        .unwrap();
    let evidence = store
        .attach_outcome_fragment(&NewTeamOutcomeFragment::new(
            team_id,
            outcome.id,
            "evidence",
            "worker-1",
            "read src/parser.rs",
        ))
        .await
        .unwrap();
    assert_eq!(summary.status, TeamOutcomeFragmentStatus::Draft);

    store
        .review_outcome_fragment(
            team_id,
            summary.id,
            TeamOutcomeFragmentStatus::Reviewed,
            "lead",
        )
        .await
        .unwrap();
    let in_review = store
        .team_outcome(team_id, outcome.id)
        .await
        .unwrap()
        .expect("outcome exists");
    assert_eq!(in_review.status, TeamOutcomeStatus::InReview);
    assert!(
        store
            .finalize_team_outcome(team_id, outcome.id)
            .await
            .is_err(),
        "unreviewed evidence blocks finalization"
    );
    store
        .review_outcome_fragment(
            team_id,
            evidence.id,
            TeamOutcomeFragmentStatus::Reviewed,
            "lead",
        )
        .await
        .unwrap();
    let finalized = store
        .finalize_team_outcome(team_id, outcome.id)
        .await
        .unwrap();
    assert_eq!(finalized.status, TeamOutcomeStatus::Finalized);
    assert!(finalized.finalized_at.is_some());

    // Re-load every family from the database, not from the values just
    // returned: this is the persistence round trip.
    assert_eq!(store.team_members(team_id).await.unwrap().len(), 2);
    assert_eq!(store.team_tasks(team_id).await.unwrap().len(), 3);
    assert_eq!(store.team_inbox(team_id, "lead").await.unwrap().len(), 1);
    assert_eq!(store.team_mission_log(team_id).await.unwrap().len(), 1);
    assert_eq!(store.team_runs(team_id).await.unwrap().len(), 2);
    assert_eq!(
        store
            .team_outcome_fragments(team_id, None)
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(store.team_outcomes(team_id).await.unwrap().len(), 1);
}
