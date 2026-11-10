//! Swarm team runner tests: board, mailbox, mission log, runs, outcomes,
//! permissions, parallelism, and the final fact path.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use dhi_agent::team_tools::{TeamAction, TeamFragmentReview, TeamRunMode};
use dhi_core::team::{
    MemoryTeamStore, MissionLogKind, TeamError, TeamMember, TeamMemberSpec, TeamMemberStatus,
    TeamOutcomeStatus, TeamRole, TeamRunStatus, TeamStore, TeamTaskStatus,
};
use dhi_orchestrator::{
    Subtask, TeamActionOutcome, TeamActor, TeamRunConfig, TeamRunner, WorkerExecutor, WorkerResult,
    WorkerSpec,
};
use uuid::Uuid;

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;
type Handler =
    Arc<dyn Fn(WorkerSpec, Subtask) -> BoxFuture<anyhow::Result<WorkerResult>> + Send + Sync>;

struct FakeExecutor {
    handler: Handler,
}

impl FakeExecutor {
    fn new(handler: Handler) -> Self {
        Self { handler }
    }

    fn always_ok() -> Self {
        Self::new(Arc::new(|worker, subtask| {
            Box::pin(async move {
                Ok(WorkerResult::ok(
                    worker.key.clone(),
                    subtask.id.clone(),
                    format!("{} finished {}", worker.key, subtask.id),
                    format!("evidence from {}", worker.key),
                ))
            })
        }))
    }
}

#[async_trait]
impl WorkerExecutor for FakeExecutor {
    async fn run(
        &self,
        worker: &WorkerSpec,
        subtask: &Subtask,
        _context: &[String],
    ) -> anyhow::Result<WorkerResult> {
        let handler = Arc::clone(&self.handler);
        handler(worker.clone(), subtask.clone()).await
    }
}

fn config(task_id: Uuid, max_parallel: usize) -> TeamRunConfig {
    TeamRunConfig {
        task_id,
        intent_id: Uuid::new_v4(),
        objective: "ship the swarm runtime".to_string(),
        lead: TeamMemberSpec::lead("lead", "coordinator; owns the outcome"),
        teammates: vec![
            TeamMemberSpec::teammate("worker-1", "audits the parser", 2),
            TeamMemberSpec::teammate("worker-2", "audits the lexer", 2),
            TeamMemberSpec::teammate("worker-3", "writes the report", 2),
        ],
        max_teammates: 4,
        max_parallel,
        max_rounds: 4,
    }
}

fn runner(handler: Handler) -> (TeamRunner<FakeExecutor>, Arc<MemoryTeamStore>) {
    let store = Arc::new(MemoryTeamStore::new());
    let runner = TeamRunner::new(Arc::new(FakeExecutor::new(handler)), store.clone());
    (runner, store)
}

fn create_task(id: Uuid, title: &str) -> TeamAction {
    TeamAction::CreateTask {
        id: Some(id),
        title: title.to_string(),
        description: format!("do {title}"),
        depends_on: Vec::new(),
        assignee: None,
    }
}

fn run_task(task_id: Uuid, agent_id: &str, mode: TeamRunMode) -> TeamAction {
    TeamAction::RunTask {
        task_id,
        agent_id: Some(agent_id.to_string()),
        mode,
        message: String::new(),
        priority: 0,
    }
}

#[tokio::test]
async fn board_mailbox_mission_and_permissions_share_one_state() {
    let task_id = Uuid::new_v4();
    let (runner, store) = runner(Arc::new(|worker, subtask| {
        Box::pin(async move {
            Ok(WorkerResult::ok(
                worker.key.clone(),
                subtask.id.clone(),
                "done",
                "evidence",
            ))
        })
    }));
    let session = runner.start(config(task_id, 3)).await.unwrap();
    let lead = session.lead_actor();
    let teammate = TeamActor::teammate("worker-1");

    let first = Uuid::new_v4();
    let created = session
        .apply(&lead, create_task(first, "audit the parser"))
        .await
        .unwrap();
    assert!(matches!(created, TeamActionOutcome::Task(_)));

    let claimed = session
        .apply(
            &teammate,
            TeamAction::ClaimTask {
                task_id: first,
                agent_id: None,
            },
        )
        .await
        .unwrap();
    match claimed {
        TeamActionOutcome::Task(task) => {
            assert_eq!(task.status, TeamTaskStatus::InProgress);
            assert_eq!(task.assignee.as_deref(), Some("worker-1"));
        }
        other => panic!("expected task, got {other:?}"),
    }

    // A teammate may create its own follow-up task.
    session
        .apply(&teammate, create_task(Uuid::new_v4(), "read the lexer"))
        .await
        .unwrap();

    // A teammate may not spawn, finalize, or produce the final fact.
    for forbidden in [
        TeamAction::SpawnTeammate {
            agent_id: "new-worker".to_string(),
            description: "extra".to_string(),
            max_turns: None,
        },
        TeamAction::FinalizeOutcome {
            outcome_id: Uuid::new_v4(),
        },
        TeamAction::Cleanup,
        TeamAction::Final {
            summary: "teammate cannot finalize".to_string(),
            detail: String::new(),
            evidence: String::new(),
            confidence: "inferred".to_string(),
        },
    ] {
        let kind = forbidden.kind();
        let err = session.apply(&teammate, forbidden).await.unwrap_err();
        assert!(
            matches!(
                err.downcast_ref::<TeamError>(),
                Some(TeamError::PermissionDenied { .. })
            ),
            "{kind} must be lead-only: {err}"
        );
    }

    let spawned = session
        .apply(
            &lead,
            TeamAction::SpawnTeammate {
                agent_id: "extra-worker".to_string(),
                description: "extra hands".to_string(),
                max_turns: Some(3),
            },
        )
        .await
        .unwrap();
    match spawned {
        TeamActionOutcome::Member(member) => {
            assert_eq!(member.status, TeamMemberStatus::Active);
            assert_eq!(member.role, TeamRole::Teammate);
        }
        other => panic!("expected member, got {other:?}"),
    }
    let stopped = session
        .apply(
            &lead,
            TeamAction::ShutdownTeammate {
                agent_id: "extra-worker".to_string(),
            },
        )
        .await
        .unwrap();
    match stopped {
        TeamActionOutcome::Member(member) => {
            assert_eq!(member.status, TeamMemberStatus::Stopped);
        }
        other => panic!("expected member, got {other:?}"),
    }

    let direct = session
        .apply(
            &teammate,
            TeamAction::SendMessage {
                to: "lead".to_string(),
                subject: "finding".to_string(),
                body: "the parser is clean".to_string(),
                task_id: Some(first),
            },
        )
        .await
        .unwrap();
    let direct_id = match direct {
        TeamActionOutcome::Message(message) => {
            assert!(!message.is_broadcast());
            assert!(message.is_unread());
            message.id
        }
        other => panic!("expected message, got {other:?}"),
    };
    session
        .apply(
            &teammate,
            TeamAction::Broadcast {
                subject: "status".to_string(),
                body: "one section is ready".to_string(),
                task_id: None,
            },
        )
        .await
        .unwrap();

    let lead_inbox = store.team_inbox(task_id, "lead").await.unwrap();
    assert_eq!(lead_inbox.len(), 2, "direct plus teammate broadcast");
    let sent = store.team_sent(task_id, "worker-1").await.unwrap();
    assert_eq!(sent.len(), 2);
    let read = store
        .mark_team_message_read(task_id, direct_id, "lead")
        .await
        .unwrap();
    assert!(read.read_at.is_some());

    session
        .apply(
            &teammate,
            TeamAction::MissionLog {
                kind: MissionLogKind::Progress,
                summary: "parser audit started".to_string(),
                evidence: "read src/parser.rs".to_string(),
                next_action: "finish the report".to_string(),
                task_id: Some(first),
            },
        )
        .await
        .unwrap();
    let mission = store.team_mission_log(task_id).await.unwrap();
    assert!(
        mission
            .iter()
            .any(|entry| entry.summary == "parser audit started")
    );

    // The memory store is the persistence seam: every row is re-readable from
    // the store and not only from the action return value.
    assert_eq!(store.team_members(task_id).await.unwrap().len(), 5);
    assert_eq!(store.team_tasks(task_id).await.unwrap().len(), 2);
}

#[test]
fn team_config_requires_two_teammates_and_an_in_range_cap() {
    let mut single = config(Uuid::new_v4(), 2);
    single.teammates.truncate(1);
    assert!(
        single.validate().is_err(),
        "a swarm team needs at least two teammates"
    );

    let mut too_small = config(Uuid::new_v4(), 2);
    too_small.max_teammates = 1;
    assert!(too_small.validate().is_err());

    let mut too_many = config(Uuid::new_v4(), 2);
    too_many.max_teammates = 2;
    assert!(
        too_many.validate().is_err(),
        "declared teammates cannot exceed the run cap"
    );

    let mut above_hard_cap = config(Uuid::new_v4(), 2);
    above_hard_cap.max_teammates = 65;
    assert!(above_hard_cap.validate().is_err());

    assert!(config(Uuid::new_v4(), 3).validate().is_ok());
}

#[tokio::test]
async fn stopped_teammate_can_be_reactivated_on_resume() {
    let task_id = Uuid::new_v4();
    let (runner, store) = runner(FakeExecutor::always_ok().handler);
    let config = config(task_id, 3);
    let session = runner.start(config.clone()).await.unwrap();
    let lead = session.lead_actor();
    session
        .apply(
            &lead,
            TeamAction::ShutdownTeammate {
                agent_id: "worker-2".to_string(),
            },
        )
        .await
        .unwrap();
    drop(session);
    assert_eq!(
        store
            .team_member(task_id, "worker-2")
            .await
            .unwrap()
            .unwrap()
            .status,
        TeamMemberStatus::Stopped
    );

    let resumed = runner.start(config).await.unwrap();
    let lead = resumed.lead_actor();
    let task = Uuid::new_v4();
    resumed
        .apply(&lead, create_task(task, "resumed work"))
        .await
        .unwrap();

    let error = resumed
        .apply(&lead, run_task(task, "worker-2", TeamRunMode::Sync))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("not active"),
        "a stopped teammate is not runnable until spawned again: {error}"
    );

    let spawned = resumed
        .apply(
            &lead,
            TeamAction::SpawnTeammate {
                agent_id: "worker-2".to_string(),
                description: "reactivated on resume".to_string(),
                max_turns: Some(3),
            },
        )
        .await
        .unwrap();
    match spawned {
        TeamActionOutcome::Member(member) => {
            assert_eq!(member.status, TeamMemberStatus::Active);
        }
        other => panic!("expected member, got {other:?}"),
    }

    let run = resumed
        .apply(&lead, run_task(task, "worker-2", TeamRunMode::Sync))
        .await
        .unwrap();
    match run {
        TeamActionOutcome::Run { run, result } => {
            assert_eq!(run.status, TeamRunStatus::Completed);
            assert!(result.expect("resumed run result").is_success());
        }
        other => panic!("expected run, got {other:?}"),
    }
}

#[tokio::test]
async fn stored_team_above_cap_is_capped_and_replaceable() {
    let task_id = Uuid::new_v4();
    let store = Arc::new(MemoryTeamStore::new());
    for agent_id in ["worker-1", "worker-2", "worker-3", "worker-4"] {
        store
            .upsert_team_member(&TeamMember::new(
                task_id,
                agent_id,
                TeamRole::Teammate,
                "stored teammate",
            ))
            .await
            .unwrap();
    }
    let team_store: Arc<dyn TeamStore> = store.clone();
    let runner = TeamRunner::new(Arc::new(FakeExecutor::always_ok()), team_store);
    let config = TeamRunConfig {
        task_id,
        intent_id: Uuid::new_v4(),
        objective: "resume a larger stored team".to_string(),
        lead: TeamMemberSpec::lead("lead", "owns the outcome"),
        teammates: vec![
            TeamMemberSpec::teammate("worker-1", "stored worker", 2),
            TeamMemberSpec::teammate("worker-2", "stored worker", 2),
        ],
        max_teammates: 2,
        max_parallel: 2,
        max_rounds: 2,
    };
    let session = runner.start(config).await.unwrap();
    let lead = session.lead_actor();

    let members = store.team_members(task_id).await.unwrap();
    let active = members
        .iter()
        .filter(|member| member.role == TeamRole::Teammate && member.status.is_runnable())
        .count();
    assert_eq!(active, 2, "stored extras above the cap are stopped");
    for member in &members {
        if member.agent_id == "worker-3" || member.agent_id == "worker-4" {
            assert_eq!(member.status, TeamMemberStatus::Stopped);
        }
    }

    let error = session
        .apply(
            &lead,
            TeamAction::SpawnTeammate {
                agent_id: "worker-5".to_string(),
                description: "cannot exceed the cap".to_string(),
                max_turns: None,
            },
        )
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("maximum 2 active teammates"),
        "spawn must use the run cap: {error}"
    );

    session
        .apply(
            &lead,
            TeamAction::ShutdownTeammate {
                agent_id: "worker-2".to_string(),
            },
        )
        .await
        .unwrap();
    let replacement = session
        .apply(
            &lead,
            TeamAction::SpawnTeammate {
                agent_id: "worker-3".to_string(),
                description: "replacement".to_string(),
                max_turns: None,
            },
        )
        .await
        .unwrap();
    match replacement {
        TeamActionOutcome::Member(member) => {
            assert_eq!(member.status, TeamMemberStatus::Active);
        }
        other => panic!("expected member, got {other:?}"),
    }
    let members = store.team_members(task_id).await.unwrap();
    assert_eq!(
        members
            .iter()
            .filter(|member| member.role == TeamRole::Teammate && member.status.is_runnable())
            .count(),
        2
    );
    assert_eq!(
        members
            .iter()
            .find(|member| member.agent_id == "worker-2")
            .unwrap()
            .status,
        TeamMemberStatus::Stopped
    );
    assert_eq!(
        members
            .iter()
            .find(|member| member.agent_id == "worker-4")
            .unwrap()
            .status,
        TeamMemberStatus::Stopped
    );
}

#[tokio::test]
async fn sync_and_async_runs_await_and_cancel_transition_through_the_store() {
    let task_id = Uuid::new_v4();
    let handler: Handler = Arc::new(|worker, subtask| {
        Box::pin(async move {
            if worker.key == "worker-3" {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Ok(WorkerResult::ok(
                worker.key.clone(),
                subtask.id.clone(),
                "completed",
                "evidence",
            ))
        })
    });
    let (runner, store) = runner(handler);
    let session = runner.start(config(task_id, 3)).await.unwrap();
    let lead = session.lead_actor();

    let sync_task = Uuid::new_v4();
    session
        .apply(&lead, create_task(sync_task, "sync work"))
        .await
        .unwrap();
    let outcome = session
        .apply(&lead, run_task(sync_task, "worker-1", TeamRunMode::Sync))
        .await
        .unwrap();
    match outcome {
        TeamActionOutcome::Run { run, result } => {
            assert_eq!(run.status, TeamRunStatus::Completed);
            let result = result.expect("sync run returns its result");
            assert!(result.is_success());
            assert_eq!(result.worker_key, "worker-1");
        }
        other => panic!("expected run, got {other:?}"),
    }
    let board = store.team_tasks(task_id).await.unwrap();
    assert_eq!(
        board
            .iter()
            .find(|task| task.id == sync_task)
            .unwrap()
            .status,
        TeamTaskStatus::Completed
    );

    let async_task = Uuid::new_v4();
    session
        .apply(&lead, create_task(async_task, "async work"))
        .await
        .unwrap();
    let queued = session
        .apply(&lead, run_task(async_task, "worker-2", TeamRunMode::Async))
        .await
        .unwrap();
    let run_id = match queued {
        TeamActionOutcome::Run { run, result } => {
            assert_eq!(run.status, TeamRunStatus::Queued);
            assert!(result.is_none());
            run.id
        }
        other => panic!("expected run, got {other:?}"),
    };
    let awaited = session
        .await_run(run_id)
        .await
        .unwrap()
        .expect("run result");
    assert!(awaited.is_success());
    let run = store.team_run(task_id, run_id).await.unwrap().unwrap();
    assert_eq!(run.status, TeamRunStatus::Completed);
    assert!(run.ended_at.is_some());
    assert!(
        session.await_run(run_id).await.unwrap().is_none(),
        "a run is awaited once"
    );

    let cancelled_task = Uuid::new_v4();
    session
        .apply(&lead, create_task(cancelled_task, "slow work"))
        .await
        .unwrap();
    let slow = session
        .apply(
            &lead,
            run_task(cancelled_task, "worker-3", TeamRunMode::Async),
        )
        .await
        .unwrap();
    let slow_id = match slow {
        TeamActionOutcome::Run { run, .. } => run.id,
        other => panic!("expected run, got {other:?}"),
    };
    let cancelled = session.cancel_run(slow_id).await.unwrap();
    assert_eq!(cancelled.status, TeamRunStatus::Cancelled);
    assert!(
        session.await_run(slow_id).await.unwrap().is_none(),
        "cancelled handle is no longer pending"
    );
    let board = store.team_tasks(task_id).await.unwrap();
    assert_eq!(
        board
            .iter()
            .find(|task| task.id == cancelled_task)
            .unwrap()
            .status,
        TeamTaskStatus::Blocked
    );
}

#[tokio::test]
async fn outcome_fragments_need_review_before_finalize() {
    let task_id = Uuid::new_v4();
    let (runner, store) = runner(FakeExecutor::always_ok().handler);
    let session = runner.start(config(task_id, 3)).await.unwrap();
    let lead = session.lead_actor();
    let teammate = TeamActor::teammate("worker-1");

    let outcome = session
        .apply(
            &lead,
            TeamAction::CreateOutcome {
                id: None,
                title: "parser report".to_string(),
                required_sections: vec!["summary".to_string(), "evidence".to_string()],
            },
        )
        .await
        .unwrap();
    let outcome_id = match outcome {
        TeamActionOutcome::Outcome(outcome) => outcome.id,
        other => panic!("expected outcome, got {other:?}"),
    };

    let summary = session
        .apply(
            &teammate,
            TeamAction::AttachOutcomeFragment {
                outcome_id,
                section: "summary".to_string(),
                content: "the parser is clean".to_string(),
                run_id: None,
            },
        )
        .await
        .unwrap();
    let summary_id = match summary {
        TeamActionOutcome::Fragment(fragment) => fragment.id,
        other => panic!("expected fragment, got {other:?}"),
    };
    let evidence = session
        .apply(
            &teammate,
            TeamAction::AttachOutcomeFragment {
                outcome_id,
                section: "evidence".to_string(),
                content: "read src/parser.rs".to_string(),
                run_id: None,
            },
        )
        .await
        .unwrap();
    let evidence_id = match evidence {
        TeamActionOutcome::Fragment(fragment) => fragment.id,
        other => panic!("expected fragment, got {other:?}"),
    };

    // A teammate may review a fragment; only finalization is coordinator-only.
    session
        .apply(
            &teammate,
            TeamAction::ReviewOutcomeFragment {
                fragment_id: summary_id,
                decision: TeamFragmentReview::Reviewed,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .team_outcome(task_id, outcome_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        TeamOutcomeStatus::InReview
    );
    assert!(
        session
            .apply(&lead, TeamAction::FinalizeOutcome { outcome_id })
            .await
            .is_err(),
        "one unreviewed required section must block finalize"
    );

    session
        .apply(
            &lead,
            TeamAction::ReviewOutcomeFragment {
                fragment_id: evidence_id,
                decision: TeamFragmentReview::Reviewed,
            },
        )
        .await
        .unwrap();
    let finalized = session
        .apply(&lead, TeamAction::FinalizeOutcome { outcome_id })
        .await
        .unwrap();
    match finalized {
        TeamActionOutcome::Outcome(outcome) => {
            assert_eq!(outcome.status, TeamOutcomeStatus::Finalized);
            assert!(outcome.finalized_at.is_some());
        }
        other => panic!("expected outcome, got {other:?}"),
    }
}

#[tokio::test]
async fn parallel_teammates_respect_the_parallel_limit() {
    let task_id = Uuid::new_v4();
    let active = Arc::new(AtomicUsize::new(0));
    let max_seen = Arc::new(AtomicUsize::new(0));
    let handler: Handler = {
        let active = Arc::clone(&active);
        let max_seen = Arc::clone(&max_seen);
        Arc::new(move |worker, subtask| {
            let active = Arc::clone(&active);
            let max_seen = Arc::clone(&max_seen);
            Box::pin(async move {
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                max_seen.fetch_max(current, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(60)).await;
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(WorkerResult::ok(
                    worker.key.clone(),
                    subtask.id.clone(),
                    "done",
                    "evidence",
                ))
            })
        })
    };
    let (runner, store) = runner(handler);
    let session = runner.start(config(task_id, 3)).await.unwrap();
    let lead = session.lead_actor();

    let mut run_ids = Vec::new();
    for (index, worker) in ["worker-1", "worker-2", "worker-3"].iter().enumerate() {
        let task = Uuid::new_v4();
        session
            .apply(&lead, create_task(task, &format!("parallel {index}")))
            .await
            .unwrap();
        let outcome = session
            .apply(&lead, run_task(task, worker, TeamRunMode::Async))
            .await
            .unwrap();
        match outcome {
            TeamActionOutcome::Run { run, .. } => run_ids.push(run.id),
            other => panic!("expected run, got {other:?}"),
        }
    }

    let result = session
        .finish(
            "parallel work completed".to_string(),
            "three teammates".to_string(),
            "all tasks completed".to_string(),
            "observed".to_string(),
        )
        .await
        .unwrap();
    assert_eq!(result.worker_results.len(), 3);
    assert!(
        max_seen.load(Ordering::SeqCst) >= 2,
        "teammates must overlap under the parallel limit"
    );
    for run_id in run_ids {
        let run = store.team_run(task_id, run_id).await.unwrap().unwrap();
        assert_eq!(run.status, TeamRunStatus::Completed);
    }
}
