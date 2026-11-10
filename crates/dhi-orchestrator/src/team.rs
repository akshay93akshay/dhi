//! Swarm team runner: shared team state plus the teammate run loop.
//!
//! The runner owns no DHI+ graph state. It applies coordinator/teammate actions
//! to a [`TeamStore`], runs teammates through the same [`WorkerExecutor`] the
//! manager and peer swarm use, and returns one [`IntentResult`] with one
//! [`FactDraft`]. The engine remains the only writer of goals, intents, facts,
//! and task state.
//!
//! Team state is scoped to one task. [`TeamRunner::start`] loads the existing
//! team for the task (or seeds it on first use), so a restarted engine resumes
//! the same board, mailbox, mission log, outcomes, and runs.
//!
//! The Swarm coordinator is a normal agent run hosted by the engine's
//! `TeamService`; this module deliberately has no model seam and no JSON
//! decision loop.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use dhi_agent::FactDraft;
use dhi_agent::team_tools::{TeamAction, TeamRunMode};
use dhi_core::IntentPlan;
use dhi_core::team::{
    MAX_TEAMMATES, MissionLogEntry, NewMissionLogEntry, NewTeamMessage, NewTeamOutcome,
    NewTeamOutcomeFragment, NewTeamRun, NewTeamTask, TeamError, TeamMember, TeamMemberSpec,
    TeamMemberStatus, TeamMessage, TeamOutcome, TeamOutcomeFragment, TeamOutcomeFragmentStatus,
    TeamPermissions, TeamRole, TeamRun, TeamRunProgress, TeamRunStatus, TeamSnapshot, TeamStore,
    TeamTask,
};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::{IntentResult, Subtask, WorkerExecutor, WorkerResult, WorkerSpec};

/// Default model-turn budget for a teammate created without an explicit one.
pub const DEFAULT_TEAMMATE_TURNS: usize = 4;

/// The typed error a team `stop` action produces.
///
/// A stop is not a successful intent result: no fact is written. The worker
/// results collected before the stop travel with the error so the engine can
/// record the failure and inspect the evidence.
#[derive(Debug, thiserror::Error)]
#[error("team coordinator stopped the intent: {reason}")]
pub struct TeamStopped {
    pub reason: String,
    pub worker_results: Vec<WorkerResult>,
}

impl TeamStopped {
    pub fn new(reason: impl Into<String>, worker_results: Vec<WorkerResult>) -> Self {
        Self {
            reason: reason.into(),
            worker_results,
        }
    }
}

/// Everything one team run needs.
#[derive(Debug, Clone)]
pub struct TeamRunConfig {
    /// The DHI+ task this team belongs to. It is also the team id.
    pub task_id: Uuid,
    /// The claimed intent the final fact will satisfy.
    pub intent_id: Uuid,
    pub objective: String,
    pub lead: TeamMemberSpec,
    pub teammates: Vec<TeamMemberSpec>,
    /// Active teammate cap for this run: fixed N, or the Auto 8 / granted
    /// `swarm_scale` cap. The coordinator is extra and never counts here.
    pub max_teammates: usize,
    /// Maximum teammates allowed to execute at the same time.
    pub max_parallel: usize,
    /// Maximum coordinator turns retained in the run config.
    pub max_rounds: usize,
}

impl TeamRunConfig {
    /// Builds the runner config from the planner's swarm plan.
    ///
    /// The plan's workers become teammates; the coordinator is the extra agent
    /// the contract reserves outside the worker count. The plan's limits supply
    /// the parallel and round budgets.
    pub fn from_plan(task_id: Uuid, intent_id: Uuid, plan: &IntentPlan) -> Self {
        let lead = TeamMemberSpec::lead(
            "lead",
            "Swarm coordinator; owns the outcome and the intent fact",
        );
        let teammates = plan
            .workers
            .iter()
            .map(|worker| {
                let description = if worker.prompt.trim().is_empty() {
                    worker.role.clone()
                } else {
                    worker.prompt.clone()
                };
                TeamMemberSpec::teammate(worker.key.clone(), description, worker.max_turns.max(1))
            })
            .collect();
        Self {
            task_id,
            intent_id,
            objective: plan.intent.clone(),
            lead,
            teammates,
            // The plan's max_workers is the planner's allowed team size: N
            // for a fixed count, the approved cap for Auto. Clamp it to the
            // user contract so a malformed plan can never widen the team.
            max_teammates: plan.limits.max_workers.clamp(2, MAX_TEAMMATES),
            max_parallel: plan.limits.max_parallel.max(1),
            max_rounds: plan.limits.max_rounds.max(1),
        }
    }

    /// Overrides the active teammate cap for this run.
    ///
    /// The engine sets this from the task's swarm worker count and the
    /// current `swarm_scale` grant: fixed N uses N, Auto uses 8 unless a
    /// granted approval raised it. The value must stay in `2..=MAX_TEAMMATES`.
    pub fn with_max_teammates(mut self, max_teammates: usize) -> Self {
        self.max_teammates = max_teammates;
        self
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.objective.trim().is_empty() {
            anyhow::bail!("team objective must not be empty");
        }
        if self.lead.role != TeamRole::Lead {
            anyhow::bail!("team coordinator spec must use the lead role");
        }
        if self.lead.agent_id.trim().is_empty() {
            anyhow::bail!("team coordinator agent id must not be empty");
        }
        if !(2..=MAX_TEAMMATES).contains(&self.max_teammates) {
            anyhow::bail!(
                "team max_teammates must be between 2 and {MAX_TEAMMATES}, got {}",
                self.max_teammates
            );
        }
        if self.teammates.len() < 2 {
            anyhow::bail!("team requires at least 2 teammates; the coordinator is extra");
        }
        if self.teammates.len() > self.max_teammates {
            anyhow::bail!(
                "team declares {} teammates, above max_teammates {}",
                self.teammates.len(),
                self.max_teammates
            );
        }
        if self.max_parallel == 0 {
            anyhow::bail!("team max_parallel must be greater than zero");
        }
        if self.max_rounds == 0 {
            anyhow::bail!("team max_rounds must be greater than zero");
        }
        let mut seen = HashSet::with_capacity(self.teammates.len() + 1);
        if !seen.insert(self.lead.agent_id.as_str()) {
            anyhow::bail!("team coordinator agent id is declared more than once");
        }
        for teammate in &self.teammates {
            if teammate.role != TeamRole::Teammate {
                anyhow::bail!(
                    "team member {:?} is not declared as a teammate",
                    teammate.agent_id
                );
            }
            if teammate.agent_id.trim().is_empty() {
                anyhow::bail!("team member agent id must not be empty");
            }
            if teammate.agent_id == self.lead.agent_id {
                anyhow::bail!(
                    "teammate {:?} collides with the coordinator agent id",
                    teammate.agent_id
                );
            }
            if !seen.insert(teammate.agent_id.as_str()) {
                anyhow::bail!(
                    "team member {:?} is declared more than once",
                    teammate.agent_id
                );
            }
        }
        Ok(())
    }
}

/// One acting member, checked against the persisted roster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamActor {
    pub agent_id: String,
    pub role: TeamRole,
}

impl TeamActor {
    pub fn lead(agent_id: impl Into<String>) -> Self {
        Self {
            agent_id: agent_id.into(),
            role: TeamRole::Lead,
        }
    }

    pub fn teammate(agent_id: impl Into<String>) -> Self {
        Self {
            agent_id: agent_id.into(),
            role: TeamRole::Teammate,
        }
    }
}

/// What one applied action produced.
// The success variants carry the full persisted row (and, for a sync run, the
// worker result). Boxing one variant would only add indirection to a one-shot
// action result.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum TeamActionOutcome {
    Member(TeamMember),
    Members(Vec<TeamMember>),
    Task(TeamTask),
    Message(TeamMessage),
    MissionLog(MissionLogEntry),
    Run {
        run: TeamRun,
        result: Option<WorkerResult>,
    },
    Outcome(TeamOutcome),
    Fragment(TeamOutcomeFragment),
    Cleanup,
    Stopped {
        reason: String,
    },
    Final {
        summary: String,
        detail: String,
        evidence: String,
        confidence: String,
    },
    None,
}

struct PendingRun {
    agent_id: String,
    task_id: Option<Uuid>,
    handle: JoinHandle<anyhow::Result<(TeamRun, WorkerResult)>>,
}

/// One live team for one task.
pub struct TeamSession<E> {
    executor: Arc<E>,
    store: Arc<dyn TeamStore>,
    semaphore: Arc<Semaphore>,
    config: TeamRunConfig,
    roster: Mutex<HashMap<String, WorkerSpec>>,
    pending: Mutex<HashMap<Uuid, PendingRun>>,
    results: Mutex<Vec<WorkerResult>>,
}

impl<E: WorkerExecutor + 'static> TeamSession<E> {
    pub fn config(&self) -> &TeamRunConfig {
        &self.config
    }

    pub fn team_id(&self) -> Uuid {
        self.config.task_id
    }

    pub fn lead_actor(&self) -> TeamActor {
        TeamActor::lead(self.config.lead.agent_id.clone())
    }

    /// Reads the shared state the coordinator sees before acting.
    pub async fn snapshot(&self) -> anyhow::Result<TeamSnapshot> {
        self.snapshot_for(&self.config.lead.agent_id).await
    }

    /// Reads the shared state one member sees, with that member's inbox.
    pub async fn snapshot_for(&self, agent_id: &str) -> anyhow::Result<TeamSnapshot> {
        let team_id = self.config.task_id;
        Ok(TeamSnapshot {
            team_id,
            objective: self.config.objective.clone(),
            members: self.store.team_members(team_id).await?,
            tasks: self.store.team_tasks(team_id).await?,
            inbox: self.store.team_inbox(team_id, agent_id).await?,
            mission_log: self.store.team_mission_log(team_id).await?,
            outcomes: self.store.team_outcomes(team_id).await?,
            fragments: self.store.team_outcome_fragments(team_id, None).await?,
            runs: self.store.team_runs(team_id).await?,
        })
    }

    /// The board cards, in store order.
    pub async fn tasks(&self) -> anyhow::Result<Vec<TeamTask>> {
        Ok(self.store.team_tasks(self.config.task_id).await?)
    }

    /// Every teammate run, in store order.
    pub async fn runs(&self) -> anyhow::Result<Vec<TeamRun>> {
        Ok(self.store.team_runs(self.config.task_id).await?)
    }

    /// One teammate run, when it belongs to this team.
    pub async fn run(&self, run_id: Uuid) -> anyhow::Result<Option<TeamRun>> {
        Ok(self.store.team_run(self.config.task_id, run_id).await?)
    }

    /// One member's inbox.
    pub async fn inbox(&self, agent_id: &str) -> anyhow::Result<Vec<TeamMessage>> {
        Ok(self.store.team_inbox(self.config.task_id, agent_id).await?)
    }

    /// One member's sent mailbox.
    pub async fn sent(&self, agent_id: &str) -> anyhow::Result<Vec<TeamMessage>> {
        Ok(self.store.team_sent(self.config.task_id, agent_id).await?)
    }

    /// Marks one message read for its recipient.
    pub async fn mark_message_read(
        &self,
        message_id: dhi_core::team::TeamMessageId,
        agent_id: &str,
    ) -> anyhow::Result<TeamMessage> {
        Ok(self
            .store
            .mark_team_message_read(self.config.task_id, message_id, agent_id)
            .await?)
    }

    /// The append-only mission log.
    pub async fn mission_log(&self) -> anyhow::Result<Vec<MissionLogEntry>> {
        Ok(self.store.team_mission_log(self.config.task_id).await?)
    }

    /// Every outcome, in store order.
    pub async fn outcomes(&self) -> anyhow::Result<Vec<TeamOutcome>> {
        Ok(self.store.team_outcomes(self.config.task_id).await?)
    }

    /// Every outcome fragment, in store order.
    pub async fn outcome_fragments(&self) -> anyhow::Result<Vec<TeamOutcomeFragment>> {
        Ok(self
            .store
            .team_outcome_fragments(self.config.task_id, None)
            .await?)
    }

    /// Applies one action after checking the acting member's persisted role.
    pub async fn apply(
        &self,
        actor: &TeamActor,
        action: TeamAction,
    ) -> anyhow::Result<TeamActionOutcome> {
        self.verify_actor(actor).await?;
        TeamPermissions::new(actor.role).require(action.kind())?;
        if actor.role == TeamRole::Lead
            && actor.agent_id.as_str() != self.config.lead.agent_id.as_str()
        {
            anyhow::bail!(
                "actor {:?} claims the coordinator role but is not the configured coordinator",
                actor.agent_id
            );
        }

        match action {
            TeamAction::CreateTask {
                id,
                title,
                description,
                depends_on,
                assignee,
            } => {
                if actor.role == TeamRole::Teammate
                    && assignee
                        .as_deref()
                        .is_some_and(|to| to != actor.agent_id.as_str())
                {
                    anyhow::bail!("teammates may only assign a new task to themselves");
                }
                if let Some(assignee) = &assignee {
                    self.member(assignee).await?;
                }
                let new = NewTeamTask {
                    id,
                    team_id: self.config.task_id,
                    title,
                    description,
                    created_by: actor.agent_id.clone(),
                    assignee,
                    depends_on,
                };
                Ok(TeamActionOutcome::Task(
                    self.store.create_team_task(&new).await?,
                ))
            }
            TeamAction::ClaimTask { task_id, agent_id } => {
                let target = self.target(actor, agent_id)?;
                self.require_runnable_member(&target).await?;
                Ok(TeamActionOutcome::Task(
                    self.store
                        .claim_team_task(self.config.task_id, task_id, &target)
                        .await?,
                ))
            }
            TeamAction::CompleteTask { task_id, summary } => {
                self.require_own_task(actor, task_id).await?;
                let summary = if summary.trim().is_empty() {
                    "completed".to_string()
                } else {
                    summary.trim().to_string()
                };
                Ok(TeamActionOutcome::Task(
                    self.store
                        .complete_team_task(self.config.task_id, task_id, &summary)
                        .await?,
                ))
            }
            TeamAction::BlockTask { task_id, reason } => {
                self.require_own_task(actor, task_id).await?;
                let reason = if reason.trim().is_empty() {
                    "blocked".to_string()
                } else {
                    reason.trim().to_string()
                };
                Ok(TeamActionOutcome::Task(
                    self.store
                        .block_team_task(self.config.task_id, task_id, &reason)
                        .await?,
                ))
            }
            TeamAction::RunTask {
                task_id,
                agent_id,
                mode,
                message,
                priority,
            } => {
                let target = self.run_target(actor, agent_id)?;
                self.require_runnable_member(&target).await?;
                let member = self.member(&target).await?;
                if member.role != TeamRole::Teammate {
                    anyhow::bail!("only teammates run as workers; {target:?} is the coordinator");
                }
                let task = self.claim_for_run(&target, task_id).await?;
                let worker = self.worker_spec(&target).await?;
                let subtask = Subtask {
                    id: task.id.to_string(),
                    description: task_description(&task),
                    worker_key: Some(target.clone()),
                };
                let message = if message.trim().is_empty() {
                    task.title.clone()
                } else {
                    message
                };
                let new = NewTeamRun::new(self.config.task_id, target.clone(), message)
                    .with_task(task.id)
                    .with_priority(priority);
                let queued = self.store.enqueue_team_run(&new).await?;
                let run_id = queued.id;

                if mode == TeamRunMode::Sync {
                    let (run, result) = run_worker(
                        Arc::clone(&self.executor),
                        Arc::clone(&self.store),
                        Arc::clone(&self.semaphore),
                        self.config.task_id,
                        worker,
                        subtask,
                        run_id,
                    )
                    .await?;
                    self.push_result(result.clone());
                    Ok(TeamActionOutcome::Run {
                        run,
                        result: Some(result),
                    })
                } else {
                    let handle = tokio::spawn(run_worker(
                        Arc::clone(&self.executor),
                        Arc::clone(&self.store),
                        Arc::clone(&self.semaphore),
                        self.config.task_id,
                        worker,
                        subtask,
                        run_id,
                    ));
                    self.pending
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .insert(
                            run_id,
                            PendingRun {
                                agent_id: target,
                                task_id: Some(task.id),
                                handle,
                            },
                        );
                    Ok(TeamActionOutcome::Run {
                        run: queued,
                        result: None,
                    })
                }
            }
            TeamAction::SendMessage {
                to,
                subject,
                body,
                task_id,
            } => {
                let mut message = NewTeamMessage::direct(
                    self.config.task_id,
                    actor.agent_id.clone(),
                    to,
                    subject,
                    body,
                );
                if let Some(task_id) = task_id {
                    message = message.with_task(task_id);
                }
                Ok(TeamActionOutcome::Message(
                    self.store.send_team_message(&message).await?,
                ))
            }
            TeamAction::Broadcast {
                subject,
                body,
                task_id,
            } => {
                let mut message = NewTeamMessage::broadcast(
                    self.config.task_id,
                    actor.agent_id.clone(),
                    subject,
                    body,
                );
                if let Some(task_id) = task_id {
                    message = message.with_task(task_id);
                }
                Ok(TeamActionOutcome::Message(
                    self.store.broadcast_team_message(&message).await?,
                ))
            }
            TeamAction::MissionLog {
                kind,
                summary,
                evidence,
                next_action,
                task_id,
            } => {
                let mut entry = NewMissionLogEntry::new(
                    self.config.task_id,
                    actor.agent_id.clone(),
                    kind,
                    summary,
                )
                .with_evidence(evidence)
                .with_next_action(next_action);
                if let Some(task_id) = task_id {
                    entry = entry.with_task(task_id);
                }
                Ok(TeamActionOutcome::MissionLog(
                    self.store.append_mission_log(&entry).await?,
                ))
            }
            TeamAction::CreateOutcome {
                id,
                title,
                required_sections,
            } => {
                let new = NewTeamOutcome {
                    id,
                    team_id: self.config.task_id,
                    title,
                    required_sections,
                    created_by: actor.agent_id.clone(),
                };
                Ok(TeamActionOutcome::Outcome(
                    self.store.create_team_outcome(&new).await?,
                ))
            }
            TeamAction::AttachOutcomeFragment {
                outcome_id,
                section,
                content,
                run_id,
            } => {
                if let Some(run_id) = run_id
                    && self
                        .store
                        .team_run(self.config.task_id, run_id)
                        .await?
                        .is_none()
                {
                    anyhow::bail!("outcome fragment names an unknown run");
                }
                let mut new = NewTeamOutcomeFragment::new(
                    self.config.task_id,
                    outcome_id,
                    section,
                    actor.agent_id.clone(),
                    content,
                );
                if let Some(run_id) = run_id {
                    new = new.with_run(run_id);
                }
                Ok(TeamActionOutcome::Fragment(
                    self.store.attach_outcome_fragment(&new).await?,
                ))
            }
            TeamAction::ReviewOutcomeFragment {
                fragment_id,
                decision,
            } => Ok(TeamActionOutcome::Fragment(
                self.store
                    .review_outcome_fragment(
                        self.config.task_id,
                        fragment_id,
                        decision.status(),
                        &actor.agent_id,
                    )
                    .await?,
            )),
            TeamAction::FinalizeOutcome { outcome_id } => Ok(TeamActionOutcome::Outcome(
                self.store
                    .finalize_team_outcome(self.config.task_id, outcome_id)
                    .await?,
            )),
            TeamAction::SpawnTeammate {
                agent_id,
                description,
                max_turns,
            } => self.spawn_teammate(&agent_id, description, max_turns).await,
            TeamAction::ShutdownTeammate { agent_id } => {
                if agent_id.as_str() == self.config.lead.agent_id.as_str() {
                    anyhow::bail!("the coordinator cannot shut itself down");
                }
                let member = self
                    .store
                    .set_team_member_status(
                        self.config.task_id,
                        &agent_id,
                        TeamMemberStatus::Stopped,
                    )
                    .await?;
                for run in self.store.team_runs(self.config.task_id).await? {
                    if run.agent_id.as_str() == agent_id.as_str() && !run.is_terminal() {
                        let _ = self.cancel_run(run.id).await;
                    }
                }
                self.roster
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&agent_id);
                Ok(TeamActionOutcome::Member(member))
            }
            TeamAction::Cleanup => {
                self.cleanup(actor).await?;
                Ok(TeamActionOutcome::Cleanup)
            }
            TeamAction::Stop { reason } => Ok(TeamActionOutcome::Stopped { reason }),
            TeamAction::Final {
                summary,
                detail,
                evidence,
                confidence,
            } => Ok(TeamActionOutcome::Final {
                summary,
                detail,
                evidence,
                confidence,
            }),
        }
    }

    /// Whether this session still has an unawaited asynchronous run.
    pub fn has_pending_runs(&self) -> bool {
        !self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty()
    }

    /// Waits for one asynchronous run, returning its worker result when the
    /// handle is still known to this session.
    pub async fn await_run(&self, run_id: Uuid) -> anyhow::Result<Option<WorkerResult>> {
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&run_id);
        let Some(pending) = pending else {
            return Ok(None);
        };
        match pending.handle.await {
            Ok(Ok((_run, result))) => {
                self.push_result(result.clone());
                Ok(Some(result))
            }
            Ok(Err(error)) => {
                let message = error.to_string();
                let task_id = pending
                    .task_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "<team>".to_string());
                let _ = self
                    .store
                    .finish_team_run(
                        self.config.task_id,
                        run_id,
                        TeamRunStatus::Failed,
                        None,
                        Some(message.as_str()),
                    )
                    .await;
                if let Some(task_id) = pending.task_id {
                    let _ = self
                        .store
                        .block_team_task(self.config.task_id, task_id, &message)
                        .await;
                }
                let result = WorkerResult::failed(pending.agent_id, task_id, message);
                self.push_result(result.clone());
                Ok(Some(result))
            }
            Err(join) => {
                let reason = if join.is_panic() {
                    format!("team run task panicked: {join}")
                } else {
                    format!("team run task cancelled: {join}")
                };
                let task_id = pending
                    .task_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "<team>".to_string());
                let _ = self
                    .store
                    .finish_team_run(
                        self.config.task_id,
                        run_id,
                        TeamRunStatus::Failed,
                        None,
                        Some(reason.as_str()),
                    )
                    .await;
                if let Some(task_id) = pending.task_id {
                    let _ = self
                        .store
                        .block_team_task(self.config.task_id, task_id, &reason)
                        .await;
                }
                let result = WorkerResult::failed(pending.agent_id, task_id, reason);
                self.push_result(result.clone());
                Ok(Some(result))
            }
        }
    }

    /// Waits for every asynchronous run created by this session.
    pub async fn await_all(&self) -> anyhow::Result<Vec<WorkerResult>> {
        self.await_all_except(None).await
    }

    /// Waits for every asynchronous run except those started by
    /// `exclude_agent`. A teammate that awaits work must never await its own
    /// run: that handle is the task currently executing the tool call.
    pub async fn await_all_except(
        &self,
        exclude_agent: Option<&str>,
    ) -> anyhow::Result<Vec<WorkerResult>> {
        let run_ids: Vec<Uuid> = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|(_, pending)| {
                exclude_agent.is_none_or(|agent_id| pending.agent_id.as_str() != agent_id)
            })
            .map(|(run_id, _)| *run_id)
            .collect();
        for run_id in run_ids {
            self.await_run(run_id).await?;
        }
        Ok(self
            .results
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone())
    }

    /// Cancels one asynchronous run and records the terminal status.
    pub async fn cancel_run(&self, run_id: Uuid) -> anyhow::Result<TeamRun> {
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&run_id);
        if let Some(pending) = pending {
            pending.handle.abort();
        }
        let run = self
            .store
            .team_run(self.config.task_id, run_id)
            .await?
            .ok_or_else(|| {
                TeamError::not_found(self.config.task_id, format!("team run {run_id}"))
            })?;
        let run = if run.is_terminal() {
            run
        } else {
            self.store
                .finish_team_run(
                    self.config.task_id,
                    run_id,
                    TeamRunStatus::Cancelled,
                    None,
                    Some("cancelled by the coordinator"),
                )
                .await?
        };
        if let Some(task_id) = run.task_id
            && let Some(task) = self.store.team_task(self.config.task_id, task_id).await?
            && task.status == dhi_core::team::TeamTaskStatus::InProgress
        {
            let _ = self
                .store
                .block_team_task(self.config.task_id, task_id, "run cancelled")
                .await;
        }
        Ok(run)
    }

    /// Builds the intent's one result from the coordinator's fact draft.
    pub async fn finish(
        &self,
        summary: String,
        detail: String,
        evidence: String,
        confidence: String,
    ) -> anyhow::Result<IntentResult> {
        if summary.trim().is_empty() {
            anyhow::bail!("team final summary must not be empty");
        }
        let mut worker_results = self.await_all().await?;
        worker_results.sort_by(|left, right| {
            left.worker_key
                .cmp(&right.worker_key)
                .then_with(|| left.subtask_id.cmp(&right.subtask_id))
        });
        let evidence = if evidence.trim().is_empty() {
            self.collect_evidence().await?
        } else {
            evidence
        };
        let confidence = if confidence.trim().is_empty() {
            "inferred".to_string()
        } else {
            confidence
        };

        let _ = self
            .store
            .append_mission_log(
                &NewMissionLogEntry::new(
                    self.config.task_id,
                    self.config.lead.agent_id.clone(),
                    dhi_core::team::MissionLogKind::Done,
                    format!("Team finalized: {summary}"),
                )
                .with_evidence(evidence.clone()),
            )
            .await;

        Ok(IntentResult {
            summary: summary.clone(),
            fact: FactDraft {
                summary,
                detail,
                evidence,
                confidence,
            },
            worker_results,
            audit_results: Vec::new(),
        })
    }

    async fn collect_evidence(&self) -> anyhow::Result<String> {
        let mut lines = Vec::new();
        for entry in self.store.team_mission_log(self.config.task_id).await? {
            lines.push(format!(
                "- [{}] {}: {}",
                entry.kind.as_str(),
                entry.agent_id,
                entry.summary
            ));
        }
        for outcome in self.store.team_outcomes(self.config.task_id).await? {
            lines.push(format!(
                "- outcome {:?} is {}",
                outcome.title,
                outcome.status.as_str()
            ));
            for fragment in self
                .store
                .team_outcome_fragments(self.config.task_id, Some(outcome.id))
                .await?
                .into_iter()
                .filter(|fragment| fragment.status == TeamOutcomeFragmentStatus::Reviewed)
            {
                lines.push(format!(
                    "  - {} [{}]: {}",
                    fragment.source_agent_id, fragment.section, fragment.content
                ));
            }
        }
        if lines.is_empty() {
            lines.push("the team produced no recorded evidence".to_string());
        }
        Ok(lines.join("\n"))
    }

    async fn verify_actor(&self, actor: &TeamActor) -> anyhow::Result<()> {
        let member = self
            .store
            .team_member(self.config.task_id, &actor.agent_id)
            .await?
            .ok_or_else(|| {
                TeamError::not_found(
                    self.config.task_id,
                    format!("team member {}", actor.agent_id),
                )
            })?;
        if member.role != actor.role {
            anyhow::bail!(
                "actor {:?} claims role {} but the persisted role is {}",
                actor.agent_id,
                actor.role.as_str(),
                member.role.as_str()
            );
        }
        if !member.status.is_runnable() {
            anyhow::bail!("team member {:?} is not active", actor.agent_id);
        }
        Ok(())
    }

    async fn member(&self, agent_id: &str) -> anyhow::Result<TeamMember> {
        let member = self
            .store
            .team_member(self.config.task_id, agent_id)
            .await?
            .ok_or_else(|| {
                TeamError::not_found(self.config.task_id, format!("team member {agent_id}"))
            })?;
        Ok(member)
    }

    async fn require_runnable_member(&self, agent_id: &str) -> anyhow::Result<()> {
        let member = self.member(agent_id).await?;
        if !member.status.is_runnable() {
            anyhow::bail!("team member {agent_id:?} is not active");
        }
        Ok(())
    }

    /// A teammate may complete or block only a task assigned to itself; the
    /// coordinator may act on any card.
    async fn require_own_task(&self, actor: &TeamActor, task_id: Uuid) -> anyhow::Result<()> {
        if actor.role == TeamRole::Lead {
            return Ok(());
        }
        let task = self
            .store
            .team_task(self.config.task_id, task_id)
            .await?
            .ok_or_else(|| {
                TeamError::not_found(self.config.task_id, format!("team task {task_id}"))
            })?;
        if task.assignee.as_deref() != Some(actor.agent_id.as_str()) {
            anyhow::bail!("teammates may only complete or block their own claimed tasks");
        }
        Ok(())
    }

    fn target(&self, actor: &TeamActor, requested: Option<String>) -> anyhow::Result<String> {
        let target = requested.unwrap_or_else(|| actor.agent_id.clone());
        if actor.role == TeamRole::Teammate && target != actor.agent_id {
            anyhow::bail!("teammates may only act on their own tasks and runs");
        }
        Ok(target)
    }

    /// The target of a `RunTask`.
    ///
    /// The coordinator may start any teammate's run. A teammate may start a
    /// peer's run, but never a nested run of itself: that would recursively
    /// re-enter the run that is currently making the tool call.
    fn run_target(&self, actor: &TeamActor, requested: Option<String>) -> anyhow::Result<String> {
        let target = requested.unwrap_or_else(|| actor.agent_id.clone());
        if actor.role == TeamRole::Teammate && target == actor.agent_id {
            anyhow::bail!("a teammate cannot start a nested run of itself");
        }
        Ok(target)
    }

    async fn claim_for_run(&self, agent_id: &str, task_id: Uuid) -> anyhow::Result<TeamTask> {
        let task = self
            .store
            .team_task(self.config.task_id, task_id)
            .await?
            .ok_or_else(|| {
                TeamError::not_found(self.config.task_id, format!("team task {task_id}"))
            })?;
        match task.status {
            dhi_core::team::TeamTaskStatus::Completed => {
                anyhow::bail!("team task {task_id} is already completed")
            }
            dhi_core::team::TeamTaskStatus::Blocked => {
                anyhow::bail!("team task {task_id} is blocked")
            }
            dhi_core::team::TeamTaskStatus::Pending => {
                if let Some(assignee) = &task.assignee
                    && assignee.as_str() != agent_id
                {
                    anyhow::bail!("team task {task_id} is assigned to {assignee}");
                }
                let tasks = self.store.team_tasks(self.config.task_id).await?;
                let unmet = task.unmet_dependencies(&tasks);
                if !unmet.is_empty() {
                    anyhow::bail!("team task {task_id} is blocked by {unmet:?}");
                }
                Ok(self
                    .store
                    .claim_team_task(self.config.task_id, task_id, agent_id)
                    .await?)
            }
            dhi_core::team::TeamTaskStatus::InProgress => {
                if let Some(assignee) = &task.assignee
                    && assignee.as_str() != agent_id
                {
                    anyhow::bail!("team task {task_id} is assigned to {assignee}");
                }
                Ok(task)
            }
        }
    }

    async fn worker_spec(&self, agent_id: &str) -> anyhow::Result<WorkerSpec> {
        if let Some(worker) = self
            .roster
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(agent_id)
            .cloned()
        {
            return Ok(worker);
        }
        let member = self.member(agent_id).await?;
        Ok(WorkerSpec {
            key: agent_id.to_string(),
            role: member.role.as_str().to_string(),
            prompt: member.description.clone(),
            max_turns: DEFAULT_TEAMMATE_TURNS,
        })
    }

    async fn spawn_teammate(
        &self,
        agent_id: &str,
        description: String,
        max_turns: Option<usize>,
    ) -> anyhow::Result<TeamActionOutcome> {
        if agent_id.trim().is_empty() {
            anyhow::bail!("spawned teammate needs an agent id");
        }
        if agent_id == self.config.lead.agent_id.as_str() {
            anyhow::bail!("cannot spawn a teammate with the coordinator agent id");
        }
        let existing = self
            .store
            .team_member(self.config.task_id, agent_id)
            .await?;
        if let Some(existing) = &existing
            && existing.role != TeamRole::Teammate
        {
            anyhow::bail!("{agent_id:?} already exists with a non-teammate role");
        }
        let members = self.store.team_members(self.config.task_id).await?;
        let active_teammates = members
            .iter()
            .filter(|member| member.role == TeamRole::Teammate && member.status.is_runnable())
            .count();
        let already_active = existing
            .as_ref()
            .is_some_and(|member| member.role == TeamRole::Teammate && member.status.is_runnable());
        if !already_active && active_teammates >= self.config.max_teammates {
            anyhow::bail!(
                "team already has the maximum {} active teammates",
                self.config.max_teammates
            );
        }
        let max_turns = max_turns.unwrap_or(DEFAULT_TEAMMATE_TURNS).max(1);
        let description = if description.trim().is_empty() {
            "Swarm teammate".to_string()
        } else {
            description
        };
        let spec = TeamMemberSpec::teammate(agent_id, description, max_turns);
        let member = self
            .store
            .upsert_team_member(&spec.member(self.config.task_id))
            .await?;
        self.roster
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                agent_id.to_string(),
                WorkerSpec {
                    key: agent_id.to_string(),
                    role: TeamRole::Teammate.as_str().to_string(),
                    prompt: member.description.clone(),
                    max_turns,
                },
            );
        Ok(TeamActionOutcome::Member(member))
    }

    /// Cancels active runs, stops every teammate, and writes one decision
    /// entry to the mission log. Best effort at the end of a run.
    pub async fn cleanup(&self, actor: &TeamActor) -> anyhow::Result<()> {
        let run_ids: Vec<Uuid> = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keys()
            .copied()
            .collect();
        for run_id in run_ids {
            let _ = self.cancel_run(run_id).await;
        }
        for run in self.store.team_runs(self.config.task_id).await? {
            if !run.is_terminal() {
                let _ = self
                    .store
                    .finish_team_run(
                        self.config.task_id,
                        run.id,
                        TeamRunStatus::Cancelled,
                        None,
                        Some("team cleanup"),
                    )
                    .await;
            }
        }
        for member in self.store.team_members(self.config.task_id).await? {
            if member.role == TeamRole::Teammate {
                let _ = self
                    .store
                    .set_team_member_status(
                        self.config.task_id,
                        &member.agent_id,
                        TeamMemberStatus::Stopped,
                    )
                    .await;
            }
        }
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let _ = self
            .store
            .append_mission_log(&NewMissionLogEntry::new(
                self.config.task_id,
                actor.agent_id.clone(),
                dhi_core::team::MissionLogKind::Decision,
                "Team cleanup: active runs cancelled and teammates stopped",
            ))
            .await;
        Ok(())
    }

    /// Every settled worker result collected by this session.
    pub fn results(&self) -> Vec<WorkerResult> {
        self.results
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn push_result(&self, result: WorkerResult) {
        self.results
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(result);
    }
}

/// The Swarm team runner.
pub struct TeamRunner<E> {
    executor: Arc<E>,
    store: Arc<dyn TeamStore>,
}

impl<E: WorkerExecutor + 'static> TeamRunner<E> {
    pub fn new(executor: Arc<E>, store: Arc<dyn TeamStore>) -> Self {
        Self { executor, store }
    }

    /// Loads the team for the task (seeding it on first use) and returns a
    /// session for explicit action application in tests or an embedding host.
    pub async fn start(&self, config: TeamRunConfig) -> anyhow::Result<TeamSession<E>> {
        config.validate()?;
        let team_id = config.task_id;
        let existing = self.store.team_members(team_id).await?;

        if let Some(member) = existing
            .iter()
            .find(|member| member.agent_id.as_str() == config.lead.agent_id.as_str())
        {
            if member.role != TeamRole::Lead {
                anyhow::bail!(
                    "team member {:?} already exists with role {}; config declares the coordinator",
                    member.agent_id,
                    member.role.as_str()
                );
            }
            if !member.status.is_runnable() {
                self.store
                    .set_team_member_status(team_id, &member.agent_id, TeamMemberStatus::Active)
                    .await?;
            }
        } else {
            self.store
                .upsert_team_member(&config.lead.member(team_id))
                .await?;
        }

        // Seed only missing config teammates. An existing stopped teammate
        // stays stopped until the coordinator explicitly spawns it again; a reopened
        // task can therefore reuse it or replace it with a new one.
        for spec in &config.teammates {
            if let Some(member) = existing
                .iter()
                .find(|member| member.agent_id.as_str() == spec.agent_id.as_str())
            {
                if member.role != TeamRole::Teammate {
                    anyhow::bail!(
                        "team member {:?} already exists with role {}; config declares teammate",
                        member.agent_id,
                        member.role.as_str()
                    );
                }
                continue;
            }
            self.store.upsert_team_member(&spec.member(team_id)).await?;
        }

        let stored_members = self.store.team_members(team_id).await?;
        let mut roster = HashMap::new();

        // Active config teammates have first claim on the roster slots.
        for spec in &config.teammates {
            let Some(member) = stored_members.iter().find(|member| {
                member.role == TeamRole::Teammate
                    && member.agent_id.as_str() == spec.agent_id.as_str()
            }) else {
                continue;
            };
            if !member.status.is_runnable() {
                continue;
            }
            roster.insert(
                spec.agent_id.clone(),
                WorkerSpec {
                    key: spec.agent_id.clone(),
                    role: TeamRole::Teammate.as_str().to_string(),
                    prompt: spec.description.clone(),
                    max_turns: spec.max_turns.max(1),
                },
            );
        }

        // Fill the remaining slots from active/idle stored teammates. Members
        // beyond the active cap are stopped, so a run created with a larger
        // team cannot leak its extra workers into this one.
        for member in &stored_members {
            if member.role != TeamRole::Teammate
                || !member.status.is_runnable()
                || roster.contains_key(member.agent_id.as_str())
            {
                continue;
            }
            if roster.len() >= config.max_teammates {
                self.store
                    .set_team_member_status(team_id, &member.agent_id, TeamMemberStatus::Stopped)
                    .await?;
                continue;
            }
            roster.insert(
                member.agent_id.clone(),
                WorkerSpec {
                    key: member.agent_id.clone(),
                    role: TeamRole::Teammate.as_str().to_string(),
                    prompt: member.description.clone(),
                    max_turns: DEFAULT_TEAMMATE_TURNS,
                },
            );
        }

        Ok(TeamSession {
            executor: Arc::clone(&self.executor),
            store: Arc::clone(&self.store),
            semaphore: Arc::new(Semaphore::new(config.max_parallel)),
            config,
            roster: Mutex::new(roster),
            pending: Mutex::new(HashMap::new()),
            results: Mutex::new(Vec::new()),
        })
    }
}

/// Executes one teammate run end to end: lease, worker turn, persisted run
/// row, board update, and an automatic mission-log entry.
async fn run_worker<E: WorkerExecutor + 'static>(
    executor: Arc<E>,
    store: Arc<dyn TeamStore>,
    semaphore: Arc<Semaphore>,
    team_id: Uuid,
    worker: WorkerSpec,
    subtask: Subtask,
    run_id: Uuid,
) -> anyhow::Result<(TeamRun, WorkerResult)> {
    let _permit = semaphore
        .acquire()
        .await
        .expect("team semaphore is never closed");
    let lease_owner = format!("{team_id}:{run_id}");
    let started = store.start_team_run(team_id, run_id, &lease_owner).await?;
    let task_id = started.task_id;

    let _ = store
        .heartbeat_team_run(
            team_id,
            run_id,
            &TeamRunProgress::new(
                &lease_owner,
                "worker turn started",
                format!("running {}", worker.key),
            ),
        )
        .await;

    let result = match executor.run(&worker, &subtask, &[]).await {
        Ok(mut result) => {
            result.worker_key = worker.key.clone();
            result.subtask_id = subtask.id.clone();
            result
        }
        Err(error) => {
            WorkerResult::failed(worker.key.clone(), subtask.id.clone(), error.to_string())
        }
    };

    let status = if result.is_success() {
        TeamRunStatus::Completed
    } else {
        TeamRunStatus::Failed
    };
    let error_text = result.error.clone();
    let run = store
        .finish_team_run(
            team_id,
            run_id,
            status,
            Some(result.summary.as_str()),
            error_text.as_deref(),
        )
        .await?;
    // A concurrent cancel wins the terminal row. The aborted worker may still
    // return a result, but it must not complete or block the board after the
    // run was cancelled.
    if run.status != status {
        return Ok((run, result));
    }

    if let Some(task_id) = task_id {
        if result.is_success() {
            let summary = if result.summary.trim().is_empty() {
                "run completed"
            } else {
                result.summary.as_str()
            };
            if let Err(error) = store.complete_team_task(team_id, task_id, summary).await {
                tracing::warn!(team_id = %team_id, task_id = %task_id, "team board complete failed: {error}");
            }
        } else {
            let reason = result
                .error
                .clone()
                .unwrap_or_else(|| "worker run failed".to_string());
            if let Err(error) = store.block_team_task(team_id, task_id, &reason).await {
                tracing::warn!(team_id = %team_id, task_id = %task_id, "team board block failed: {error}");
            }
        }
    }

    let (kind, summary) = if result.is_success() {
        (
            dhi_core::team::MissionLogKind::Done,
            format!("{} completed {}", worker.key, subtask.id),
        )
    } else {
        (
            dhi_core::team::MissionLogKind::Error,
            format!(
                "{} failed {}: {}",
                worker.key,
                subtask.id,
                result.error.as_deref().unwrap_or("unknown error")
            ),
        )
    };
    let mut entry = NewMissionLogEntry::new(team_id, worker.key.clone(), kind, summary)
        .with_evidence(result.evidence.clone());
    if let Some(task_id) = task_id {
        entry = entry.with_task(task_id);
    }
    let _ = store.append_mission_log(&entry).await;

    Ok((run, result))
}

fn task_description(task: &TeamTask) -> String {
    if task.description.trim().is_empty() {
        task.title.clone()
    } else {
        format!("{}\n\n{}", task.title, task.description)
    }
}
