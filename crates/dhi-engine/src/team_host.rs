//! The Swarm team tool host.
//!
//! A Swarm coordinator is a normal agent run. It receives the worker domain
//! tools plus every team tool, and a teammate receives the same team tools
//! except spawn, shutdown, cleanup, and outcome finalization. This module owns
//! the bridge from a Rig tool call to the shared [`TeamSession`]:
//!
//! - [`TeamService`] owns the live session, the agent runtime, ids, cap, and
//!   the stop flag.
//! - [`TeamToolView`] builds the role-scoped Rig registry; every registered
//!   tool calls [`TeamService::invoke`].
//! - [`TeammateExecutor`] runs a teammate through `AgentRuntime::execute_teammate`
//!   with its own role-scoped view, then hands the result to the existing
//!   session run-row/board logic.
//!
//! The constructor breaks the executor/session/service cycle with a
//! `OnceLock<Weak<TeamService>>` slot: the executor holds the slot, the session
//! holds the executor, the service holds the session, and the slot is filled
//! only after the service exists. The weak reference keeps the service (and
//! its session) from leaking through the executor.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use async_trait::async_trait;
use dhi_agent::team_tools::{
    TeamAction, TeamFragmentReview, TeamRunMode, team_tool_specs, teammate_task_text,
};
use dhi_agent::{AgentRuntime, ExecuteInput, ToolServerView};
use dhi_core::team::{
    MissionLogKind, TeamActionKind, TeamError, TeamMemberSpec, TeamPermissions, TeamRole, TeamRun,
    TeamSnapshot, TeamStore, TeamTask,
};
use dhi_orchestrator::{
    Subtask, TeamActionOutcome, TeamActor, TeamRunConfig, TeamRunner, TeamSession, WorkerExecutor,
    WorkerResult, WorkerSpec,
};
use rig::message::ToolName;
use rig::tool::server::{ToolServer, ToolServerHandle};
use rig::tool::{DynamicTool, ToolExecutionError, ToolOutput};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::team::worker_result_from_call;

/// The live team behind one intent.
pub struct TeamService {
    session: Arc<TeamSession<TeammateExecutor>>,
    agent: Arc<dyn AgentRuntime>,
    task_id: Uuid,
    intent_id: Uuid,
    objective: String,
    config: TeamRunConfig,
    max_teammates: usize,
    stopped: AtomicBool,
    stop_reason: Mutex<Option<String>>,
}

impl TeamService {
    /// Loads the persisted team and prepares both tool views.
    pub async fn start(
        agent: Arc<dyn AgentRuntime>,
        store: Arc<dyn TeamStore>,
        config: TeamRunConfig,
        context: Vec<String>,
        worker_profile_id: Option<i64>,
    ) -> anyhow::Result<Arc<Self>> {
        let task_id = config.task_id;
        let intent_id = config.intent_id;
        let objective = config.objective.clone();
        let max_teammates = config.max_teammates;
        let slot: Arc<OnceLock<Weak<TeamService>>> = Arc::new(OnceLock::new());
        let executor = Arc::new(TeammateExecutor {
            slot: Arc::clone(&slot),
            agent: Arc::clone(&agent),
            task_id,
            intent_id,
            objective: objective.clone(),
            context,
            worker_profile_id,
        });
        let runner = TeamRunner::new(executor, store);
        let session = Arc::new(runner.start(config.clone()).await?);
        let service = Arc::new(Self {
            session,
            agent,
            task_id,
            intent_id,
            objective,
            config,
            max_teammates,
            stopped: AtomicBool::new(false),
            stop_reason: Mutex::new(None),
        });
        slot.set(Arc::downgrade(&service))
            .map_err(|_| anyhow::anyhow!("team service cycle was initialized twice"))?;
        Ok(service)
    }

    pub fn task_id(&self) -> Uuid {
        self.task_id
    }

    pub fn intent_id(&self) -> Uuid {
        self.intent_id
    }

    pub fn objective(&self) -> &str {
        &self.objective
    }

    pub fn config(&self) -> &TeamRunConfig {
        &self.config
    }

    pub fn max_teammates(&self) -> usize {
        self.max_teammates
    }

    pub fn agent(&self) -> &Arc<dyn AgentRuntime> {
        &self.agent
    }

    /// The full coordinator view: all team tools.
    pub fn coordinator_view(self: &Arc<Self>) -> TeamToolView {
        TeamToolView::new(
            Arc::clone(self),
            TeamActor::lead(self.config.lead.agent_id.clone()),
        )
    }

    /// The role-scoped teammate view: spawn, shutdown, cleanup, and outcome
    /// finalization are absent.
    pub fn teammate_view(self: &Arc<Self>, agent_id: &str) -> TeamToolView {
        TeamToolView::new(Arc::clone(self), TeamActor::teammate(agent_id))
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    pub fn stop_reason(&self) -> Option<String> {
        self.stop_reason
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Every settled worker result collected by this session.
    pub fn worker_results(&self) -> Vec<WorkerResult> {
        self.session.results()
    }

    /// Applies one already-parsed action after the shared permission check.
    pub async fn apply(
        &self,
        actor: &TeamActor,
        action: TeamAction,
    ) -> anyhow::Result<TeamActionOutcome> {
        TeamPermissions::new(actor.role).require(action.kind())?;
        self.session.apply(actor, action).await
    }

    /// Dispatches one model tool call.
    ///
    /// Permission is checked twice: the tool registry is role-filtered, and
    /// the action path checks `TeamRole::can` before the session checks the
    /// persisted member again. Spawn goes through the session's
    /// `max_teammates` cap, so the host can never widen the team.
    pub async fn invoke(
        &self,
        actor: &TeamActor,
        tool: &str,
        args: Value,
    ) -> Result<Value, ToolExecutionError> {
        match tool {
            "team_status" => {
                let snapshot = self
                    .session
                    .snapshot_for(&actor.agent_id)
                    .await
                    .map_err(other_error)?;
                Ok(json!({
                    "team_id": snapshot.team_id,
                    "objective": snapshot.objective,
                    "max_teammates": self.max_teammates,
                    "stopped": self.is_stopped(),
                    "stop_reason": self.stop_reason(),
                    "members": snapshot.members,
                    "tasks": snapshot.tasks,
                    "inbox": snapshot.inbox,
                    "mission_log": snapshot.mission_log,
                    "outcomes": snapshot.outcomes,
                    "fragments": snapshot.fragments,
                    "runs": snapshot.runs,
                }))
            }
            "team_task" => self.team_task(actor, args).await,
            "team_run_task" => {
                let task_id = required_uuid(&args, "task_id")?;
                // A run always names its teammate. Defaulting to the caller
                // would make the coordinator run itself and let a teammate
                // nest a second copy of its own run.
                let agent_id = Some(required_string(&args, "agent_id")?);
                let mode = match optional_string(&args, "mode")?.as_deref() {
                    Some("async") => TeamRunMode::Async,
                    Some("sync") | None => TeamRunMode::Sync,
                    Some(other) => {
                        return Err(invalid_args(format!(
                            "mode must be `sync` or `async`, got {other:?}"
                        )));
                    }
                };
                let action = TeamAction::RunTask {
                    task_id,
                    agent_id,
                    mode,
                    message: optional_string(&args, "message")?.unwrap_or_default(),
                    priority: optional_i32(&args, "priority")?.unwrap_or(0),
                };
                outcome_json(self.apply(actor, action).await.map_err(apply_error)?)
            }
            "team_list_runs" => {
                let runs = self.session.runs().await.map_err(other_error)?;
                Ok(json!({ "runs": runs }))
            }
            "team_await_runs" => self.await_runs(actor, &args).await,
            "team_cancel_run" => {
                let run_id = required_uuid(&args, "run_id")?;
                if actor.role == TeamRole::Teammate {
                    let run = self.session.run(run_id).await.map_err(other_error)?;
                    if run
                        .as_ref()
                        .is_some_and(|run| run.agent_id != actor.agent_id)
                    {
                        return Err(ToolExecutionError::permission_denied(
                            "teammates may only cancel their own runs",
                        ));
                    }
                }
                let run = self.session.cancel_run(run_id).await.map_err(other_error)?;
                Ok(json!({ "run": run }))
            }
            "team_send_message" => {
                let action = TeamAction::SendMessage {
                    to: required_string(&args, "to")?,
                    subject: optional_string(&args, "subject")?.unwrap_or_default(),
                    body: required_string(&args, "body")?,
                    task_id: optional_uuid(&args, "task_id")?,
                };
                outcome_json(self.apply(actor, action).await.map_err(apply_error)?)
            }
            "team_broadcast" => {
                let action = TeamAction::Broadcast {
                    subject: optional_string(&args, "subject")?.unwrap_or_default(),
                    body: required_string(&args, "body")?,
                    task_id: optional_uuid(&args, "task_id")?,
                };
                outcome_json(self.apply(actor, action).await.map_err(apply_error)?)
            }
            "team_read_mailbox" => self.read_mailbox(actor, &args).await,
            "team_mission_log" => {
                let kind_raw = required_string(&args, "kind")?;
                let kind = MissionLogKind::parse(&kind_raw).ok_or_else(|| {
                    invalid_args(format!(
                        "kind must be progress, handoff, blocked, decision, done, or error, got {kind_raw:?}"
                    ))
                })?;
                let action = TeamAction::MissionLog {
                    kind,
                    summary: required_string(&args, "summary")?,
                    evidence: optional_string(&args, "evidence")?.unwrap_or_default(),
                    next_action: optional_string(&args, "next_action")?.unwrap_or_default(),
                    task_id: optional_uuid(&args, "task_id")?,
                };
                outcome_json(self.apply(actor, action).await.map_err(apply_error)?)
            }
            "team_create_outcome" => {
                let action = TeamAction::CreateOutcome {
                    id: optional_uuid(&args, "id")?,
                    title: required_string(&args, "title")?,
                    required_sections: string_array(&args, "required_sections")?,
                };
                outcome_json(self.apply(actor, action).await.map_err(apply_error)?)
            }
            "team_attach_outcome_fragment" => {
                let action = TeamAction::AttachOutcomeFragment {
                    outcome_id: required_uuid(&args, "outcome_id")?,
                    section: required_string(&args, "section")?,
                    content: required_string(&args, "content")?,
                    run_id: optional_uuid(&args, "run_id")?,
                };
                outcome_json(self.apply(actor, action).await.map_err(apply_error)?)
            }
            "team_review_outcome_fragment" => {
                let decision = match optional_string(&args, "decision")?.as_deref() {
                    Some("rejected") => TeamFragmentReview::Rejected,
                    Some("reviewed") | None => TeamFragmentReview::Reviewed,
                    Some(other) => {
                        return Err(invalid_args(format!(
                            "decision must be `reviewed` or `rejected`, got {other:?}"
                        )));
                    }
                };
                let action = TeamAction::ReviewOutcomeFragment {
                    fragment_id: required_uuid(&args, "fragment_id")?,
                    decision,
                };
                outcome_json(self.apply(actor, action).await.map_err(apply_error)?)
            }
            "team_finalize_outcome" => {
                let action = TeamAction::FinalizeOutcome {
                    outcome_id: required_uuid(&args, "outcome_id")?,
                };
                outcome_json(self.apply(actor, action).await.map_err(apply_error)?)
            }
            "team_list_outcomes" => {
                let outcomes = self.session.outcomes().await.map_err(other_error)?;
                let fragments = self
                    .session
                    .outcome_fragments()
                    .await
                    .map_err(other_error)?;
                Ok(json!({ "outcomes": outcomes, "fragments": fragments }))
            }
            "team_spawn_teammate" => {
                let action = TeamAction::SpawnTeammate {
                    agent_id: required_string(&args, "agent_id")?,
                    description: optional_string(&args, "description")?.unwrap_or_default(),
                    max_turns: optional_usize(&args, "max_turns")?,
                };
                // The session enforces this run's active cap. Check it once
                // more here so a direct service caller cannot bypass spawn.
                let members = self.session.snapshot().await.map_err(other_error)?.members;
                let agent_id = match &action {
                    TeamAction::SpawnTeammate { agent_id, .. } => agent_id.as_str(),
                    _ => unreachable!(),
                };
                let already_active = members.iter().any(|member| {
                    member.agent_id.as_str() == agent_id
                        && member.role == TeamRole::Teammate
                        && member.status.is_runnable()
                });
                let active = members
                    .iter()
                    .filter(|member| {
                        member.role == TeamRole::Teammate && member.status.is_runnable()
                    })
                    .count();
                if !already_active && active >= self.max_teammates {
                    return Err(other_error(TeamError::validation(
                        self.task_id,
                        format!(
                            "team already has the maximum {} active teammates",
                            self.max_teammates
                        ),
                    )));
                }
                outcome_json(self.apply(actor, action).await.map_err(apply_error)?)
            }
            "team_shutdown_teammate" => {
                let action = TeamAction::ShutdownTeammate {
                    agent_id: required_string(&args, "agent_id")?,
                };
                outcome_json(self.apply(actor, action).await.map_err(apply_error)?)
            }
            "team_cleanup" => outcome_json(
                self.apply(actor, TeamAction::Cleanup)
                    .await
                    .map_err(apply_error)?,
            ),
            "team_stop" => {
                let reason = required_string(&args, "reason")?;
                let outcome = self
                    .apply(
                        actor,
                        TeamAction::Stop {
                            reason: reason.clone(),
                        },
                    )
                    .await
                    .map_err(apply_error)?;
                self.stopped.store(true, Ordering::SeqCst);
                *self
                    .stop_reason
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(reason);
                outcome_json(outcome)
            }
            other => Err(invalid_args(format!("unknown team tool {other:?}"))),
        }
    }

    async fn team_task(&self, actor: &TeamActor, args: Value) -> Result<Value, ToolExecutionError> {
        let operation = required_string(&args, "operation")?;
        match operation.as_str() {
            "list" => {
                let tasks = self.session.tasks().await.map_err(other_error)?;
                Ok(json!({ "tasks": tasks }))
            }
            "create" => {
                let action = TeamAction::CreateTask {
                    id: optional_uuid(&args, "id")?,
                    title: required_string(&args, "title")?,
                    description: optional_string(&args, "description")?.unwrap_or_default(),
                    depends_on: uuid_array(&args, "depends_on")?,
                    assignee: optional_string(&args, "assignee")?,
                };
                outcome_json(self.apply(actor, action).await.map_err(apply_error)?)
            }
            "claim" => {
                let action = TeamAction::ClaimTask {
                    task_id: required_uuid(&args, "task_id")?,
                    agent_id: optional_string(&args, "agent_id")?,
                };
                outcome_json(self.apply(actor, action).await.map_err(apply_error)?)
            }
            "complete" => {
                let action = TeamAction::CompleteTask {
                    task_id: required_uuid(&args, "task_id")?,
                    summary: optional_string(&args, "summary")?.unwrap_or_default(),
                };
                outcome_json(self.apply(actor, action).await.map_err(apply_error)?)
            }
            "block" => {
                let action = TeamAction::BlockTask {
                    task_id: required_uuid(&args, "task_id")?,
                    reason: optional_string(&args, "reason")?.unwrap_or_default(),
                };
                outcome_json(self.apply(actor, action).await.map_err(apply_error)?)
            }
            other => Err(invalid_args(format!(
                "team_task operation must be create, list, claim, complete, or block, got {other:?}"
            ))),
        }
    }

    async fn await_runs(
        &self,
        actor: &TeamActor,
        args: &Value,
    ) -> Result<Value, ToolExecutionError> {
        let mut ids = uuid_array(args, "run_ids")?;
        if let Some(run_id) = optional_uuid(args, "run_id")? {
            ids.push(run_id);
        }
        if actor.role == TeamRole::Teammate {
            for run_id in &ids {
                let run = self.session.run(*run_id).await.map_err(other_error)?;
                if run
                    .as_ref()
                    .is_some_and(|run| run.agent_id == actor.agent_id)
                {
                    return Err(ToolExecutionError::permission_denied(
                        "teammates cannot await their own run",
                    ));
                }
            }
        }
        let mut results = Vec::new();
        if ids.is_empty() {
            let exclude = (actor.role == TeamRole::Teammate).then_some(actor.agent_id.as_str());
            results = self
                .session
                .await_all_except(exclude)
                .await
                .map_err(other_error)?
                .into_iter()
                .map(|result| {
                    serde_json::to_value(result)
                        .map_err(|err| other_error(anyhow::anyhow!("serialize run result: {err}")))
                })
                .collect::<Result<Vec<_>, _>>()?;
        } else {
            for run_id in &ids {
                if let Some(result) = self.session.await_run(*run_id).await.map_err(other_error)? {
                    results.push(serde_json::to_value(result).map_err(|err| {
                        other_error(anyhow::anyhow!("serialize run result: {err}"))
                    })?);
                }
            }
        }
        let runs = if ids.is_empty() {
            self.session.runs().await.map_err(other_error)?
        } else {
            let mut runs = Vec::new();
            for run_id in &ids {
                if let Some(run) = self.session.run(*run_id).await.map_err(other_error)? {
                    runs.push(run);
                }
            }
            runs
        };
        Ok(json!({
            "results": results,
            "runs": runs,
            "pending": self.session.has_pending_runs(),
        }))
    }

    async fn read_mailbox(
        &self,
        actor: &TeamActor,
        args: &Value,
    ) -> Result<Value, ToolExecutionError> {
        let box_name = optional_string(args, "box")?.unwrap_or_else(|| "inbox".to_string());
        let sent = match box_name.as_str() {
            "inbox" => false,
            "sent" => true,
            other => {
                return Err(invalid_args(format!(
                    "box must be `inbox` or `sent`, got {other:?}"
                )));
            }
        };
        let mark_read = optional_bool(args, "mark_read")?.unwrap_or(false);
        let mut messages = if sent {
            self.session
                .sent(&actor.agent_id)
                .await
                .map_err(other_error)?
        } else {
            self.session
                .inbox(&actor.agent_id)
                .await
                .map_err(other_error)?
        };
        if mark_read && !sent {
            let unread: Vec<i64> = messages
                .iter()
                .filter(|message| message.is_unread())
                .map(|message| message.id)
                .collect();
            for message_id in unread {
                let updated = self
                    .session
                    .mark_message_read(message_id, &actor.agent_id)
                    .await
                    .map_err(other_error)?;
                if let Some(message) = messages.iter_mut().find(|message| message.id == message_id)
                {
                    *message = updated;
                }
            }
        }
        Ok(json!({ "box": if sent { "sent" } else { "inbox" }, "messages": messages }))
    }

    /// Makes one fact result from the coordinator's record and the settled
    /// worker results.
    pub async fn finish(
        &self,
        fact: dhi_agent::FactDraft,
    ) -> anyhow::Result<dhi_orchestrator::IntentResult> {
        self.session
            .finish(fact.summary, fact.detail, fact.evidence, fact.confidence)
            .await
    }

    /// Best-effort cleanup after a final, stop, or error.
    pub async fn cleanup(&self) {
        let actor = TeamActor::lead(self.config.lead.agent_id.clone());
        let _ = self.session.cleanup(&actor).await;
    }

    /// Snapshot accessor used by tests and the engine's stop path.
    pub async fn snapshot(&self) -> anyhow::Result<TeamSnapshot> {
        self.session.snapshot().await
    }

    /// Every run row, used by the engine's provenance and stop handling.
    pub async fn runs(&self) -> anyhow::Result<Vec<TeamRun>> {
        self.session.runs().await
    }

    /// The board, used by tests and resumability checks.
    pub async fn tasks(&self) -> anyhow::Result<Vec<TeamTask>> {
        self.session.tasks().await
    }

    /// The persisted roster, used by tests.
    pub async fn members(&self) -> anyhow::Result<Vec<dhi_core::team::TeamMember>> {
        Ok(self.session.snapshot().await?.members)
    }

    /// The active spawn cap for this run.
    pub fn coordinator_spec(&self) -> TeamMemberSpec {
        self.config.lead.clone()
    }

    /// The result of a coordinator `team_stop`, after cancelling active runs.
    pub async fn stopped(&self) -> dhi_orchestrator::TeamStopped {
        self.cleanup().await;
        dhi_orchestrator::TeamStopped::new(
            self.stop_reason()
                .unwrap_or_else(|| "team stopped".to_string()),
            self.worker_results(),
        )
    }
}

/// The Rig registry a coordinator or teammate run attaches.
pub struct TeamToolView {
    handle: ToolServerHandle,
}

impl TeamToolView {
    pub fn new(service: Arc<TeamService>, actor: TeamActor) -> Self {
        let mut server = ToolServer::new();
        for spec in team_tool_specs(actor.role) {
            let name = spec.name.clone();
            let tool_name = ToolName::new(&spec.name)
                .expect("the team tool vocabulary uses valid Rig tool names");
            let service = Arc::clone(&service);
            let actor = actor.clone();
            let tool = DynamicTool::new(
                tool_name,
                spec.description,
                spec.schema,
                move |args: Value| {
                    let service = Arc::clone(&service);
                    let actor = actor.clone();
                    let name = name.clone();
                    Box::pin(async move {
                        service
                            .invoke(&actor, &name, args)
                            .await
                            .map(ToolOutput::json)
                    })
                },
            );
            server = server.dynamic_tool(tool);
        }
        Self {
            handle: server.run(),
        }
    }
}

impl ToolServerView for TeamToolView {
    fn tool_server(&self) -> ToolServerHandle {
        self.handle.clone()
    }
}

/// The teammate run seam. The slot breaks the construction cycle between the
/// service, its session, and this executor.
pub struct TeammateExecutor {
    slot: Arc<OnceLock<Weak<TeamService>>>,
    agent: Arc<dyn AgentRuntime>,
    task_id: Uuid,
    intent_id: Uuid,
    objective: String,
    context: Vec<String>,
    worker_profile_id: Option<i64>,
}

#[async_trait]
impl WorkerExecutor for TeammateExecutor {
    async fn run(
        &self,
        worker: &WorkerSpec,
        subtask: &Subtask,
        runner_context: &[String],
    ) -> anyhow::Result<WorkerResult> {
        let service = self
            .slot
            .get()
            .and_then(|weak| weak.upgrade())
            .ok_or_else(|| anyhow::anyhow!("team service is not initialized"))?;
        let mut context = self.context.clone();
        context.extend(runner_context.iter().cloned());
        let view: Arc<dyn ToolServerView> = Arc::new(service.teammate_view(&worker.key));
        let executed = self
            .agent
            .execute_teammate(
                ExecuteInput {
                    task_id: self.task_id,
                    intent_id: self.intent_id,
                    intent: self.objective.clone(),
                    context,
                    worker_profile_id: self.worker_profile_id,
                    max_turns: Some(worker.max_turns),
                    agent_id: Some(worker.key.clone()),
                    role: Some(worker.role.clone()),
                    task: Some(subtask.description.clone()),
                },
                view,
            )
            .await;
        Ok(worker_result_from_call(worker, subtask, executed))
    }
}

fn outcome_json(outcome: TeamActionOutcome) -> Result<Value, ToolExecutionError> {
    let value = match outcome {
        TeamActionOutcome::Member(member) => json!({ "member": member }),
        TeamActionOutcome::Members(members) => json!({ "members": members }),
        TeamActionOutcome::Task(task) => json!({ "task": task }),
        TeamActionOutcome::Message(message) => json!({ "message": message }),
        TeamActionOutcome::MissionLog(entry) => json!({ "mission_log": entry }),
        TeamActionOutcome::Run { run, result } => json!({ "run": run, "result": result }),
        TeamActionOutcome::Outcome(outcome) => json!({ "outcome": outcome }),
        TeamActionOutcome::Fragment(fragment) => json!({ "fragment": fragment }),
        TeamActionOutcome::Cleanup => json!({ "cleanup": true }),
        TeamActionOutcome::Stopped { reason } => {
            json!({ "stopped": true, "reason": reason })
        }
        TeamActionOutcome::Final {
            summary,
            detail,
            evidence,
            confidence,
        } => json!({
            "final": {
                "summary": summary,
                "detail": detail,
                "evidence": evidence,
                "confidence": confidence,
            }
        }),
        TeamActionOutcome::None => Value::Null,
    };
    Ok(value)
}

fn invalid_args(message: impl Into<String>) -> ToolExecutionError {
    ToolExecutionError::invalid_args(message)
}

fn other_error(error: impl std::fmt::Display) -> ToolExecutionError {
    ToolExecutionError::other(error.to_string())
}

/// Maps a session failure onto the model-visible error kind. Permission
/// failures stay distinguishable so a model can correct its action.
fn apply_error(error: anyhow::Error) -> ToolExecutionError {
    if let Some(TeamError::PermissionDenied { .. }) = error.downcast_ref::<TeamError>() {
        return ToolExecutionError::permission_denied(error.to_string());
    }
    ToolExecutionError::other(error.to_string())
}

fn required_string(args: &Value, key: &str) -> Result<String, ToolExecutionError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .ok_or_else(|| invalid_args(format!("{key} must be a non-empty string")))
}

fn optional_string(args: &Value, key: &str) -> Result<Option<String>, ToolExecutionError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(invalid_args(format!("{key} must be a string"))),
    }
}

fn required_uuid(args: &Value, key: &str) -> Result<Uuid, ToolExecutionError> {
    let raw = required_string(args, key)?;
    Uuid::parse_str(&raw).map_err(|_| invalid_args(format!("{key} must be a UUID")))
}

fn optional_uuid(args: &Value, key: &str) -> Result<Option<Uuid>, ToolExecutionError> {
    match optional_string(args, key)? {
        Some(raw) => Uuid::parse_str(&raw)
            .map(Some)
            .map_err(|_| invalid_args(format!("{key} must be a UUID"))),
        None => Ok(None),
    }
}

fn uuid_array(args: &Value, key: &str) -> Result<Vec<Uuid>, ToolExecutionError> {
    let Some(value) = args.get(key) else {
        return Ok(Vec::new());
    };
    let Value::Array(items) = value else {
        return Err(invalid_args(format!("{key} must be an array of UUIDs")));
    };
    items
        .iter()
        .map(|item| {
            let Some(raw) = item.as_str() else {
                return Err(invalid_args(format!("{key} must be an array of UUIDs")));
            };
            Uuid::parse_str(raw).map_err(|_| invalid_args(format!("{key} must contain UUIDs")))
        })
        .collect()
}

fn string_array(args: &Value, key: &str) -> Result<Vec<String>, ToolExecutionError> {
    let Some(value) = args.get(key) else {
        return Ok(Vec::new());
    };
    let Value::Array(items) = value else {
        return Err(invalid_args(format!("{key} must be an array of strings")));
    };
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_string)
                .ok_or_else(|| invalid_args(format!("{key} must be an array of strings")))
        })
        .collect()
}

fn optional_i32(args: &Value, key: &str) -> Result<Option<i32>, ToolExecutionError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_i64()
            .and_then(|value| i32::try_from(value).ok())
            .map(Some)
            .ok_or_else(|| invalid_args(format!("{key} must be an integer"))),
    }
}

fn optional_usize(args: &Value, key: &str) -> Result<Option<usize>, ToolExecutionError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| *value > 0)
            .map(Some)
            .ok_or_else(|| invalid_args(format!("{key} must be a positive integer"))),
    }
}

fn optional_bool(args: &Value, key: &str) -> Result<Option<bool>, ToolExecutionError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_bool()
            .map(Some)
            .ok_or_else(|| invalid_args(format!("{key} must be a boolean"))),
    }
}

/// A task card's text, used by the teammate executor and tests.
pub fn task_text(task: &TeamTask) -> String {
    teammate_task_text(task)
}

/// The action kind a tool mutates, for permission checks in tests and hosts.
pub fn tool_action_kind(tool: &str) -> Option<TeamActionKind> {
    match tool {
        "team_spawn_teammate" => Some(TeamActionKind::SpawnTeammate),
        "team_shutdown_teammate" => Some(TeamActionKind::ShutdownTeammate),
        "team_run_task" => Some(TeamActionKind::RunTask),
        "team_send_message" => Some(TeamActionKind::SendMessage),
        "team_broadcast" => Some(TeamActionKind::Broadcast),
        "team_mission_log" => Some(TeamActionKind::MissionLog),
        "team_create_outcome" => Some(TeamActionKind::CreateOutcome),
        "team_attach_outcome_fragment" => Some(TeamActionKind::AttachOutcomeFragment),
        "team_review_outcome_fragment" => Some(TeamActionKind::ReviewOutcomeFragment),
        "team_finalize_outcome" => Some(TeamActionKind::FinalizeOutcome),
        "team_cleanup" => Some(TeamActionKind::Cleanup),
        "team_stop" => Some(TeamActionKind::Stop),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dhi_agent::MockAgentRuntime;
    use dhi_core::team::MemoryTeamStore;
    use serde_json::{Value, json};
    use std::time::Duration;

    fn config(task_id: Uuid, cap: usize) -> TeamRunConfig {
        TeamRunConfig {
            task_id,
            intent_id: Uuid::new_v4(),
            objective: "ship the swarm runtime".to_string(),
            lead: TeamMemberSpec::lead("lead", "coordinator; owns the outcome"),
            teammates: vec![
                TeamMemberSpec::teammate("w0", "collects evidence", 2),
                TeamMemberSpec::teammate("w1", "checks the evidence", 2),
            ],
            max_teammates: cap,
            max_parallel: cap,
            max_rounds: 4,
        }
    }

    async fn start_service(config: TeamRunConfig, store: Arc<dyn TeamStore>) -> Arc<TeamService> {
        let agent: Arc<dyn AgentRuntime> = Arc::new(MockAgentRuntime);
        TeamService::start(
            agent,
            store,
            config,
            vec!["fact 89abcdef: a confirmed fact".to_string()],
            None,
        )
        .await
        .expect("team starts")
    }

    async fn call(view: &TeamToolView, name: &str, args: Value) -> Result<Value, String> {
        let handle = view.tool_server();
        let toolset = handle.toolset();
        let tool = toolset
            .get(name)
            .ok_or_else(|| format!("{name} is not registered"))?
            .clone();
        let mut context = rig::tool::ToolContext::new();
        let result = tool.execute(args.to_string(), &mut context).await;
        if result.is_success() {
            Ok(result.output().as_json().cloned().unwrap_or(Value::Null))
        } else {
            Err(result
                .error()
                .or_else(|| result.refusal())
                .map(ToString::to_string)
                .unwrap_or_else(|| format!("{result:?}")))
        }
    }

    #[tokio::test]
    async fn coordinator_tool_loop_spawns_board_messages_runs_and_outcomes() {
        let task_id = Uuid::new_v4();
        let store: Arc<dyn TeamStore> = Arc::new(MemoryTeamStore::new());
        let service = start_service(config(task_id, 3), store.clone()).await;
        let view = service.coordinator_view();

        let spawned = call(
            &view,
            "team_spawn_teammate",
            json!({"agent_id": "w2", "description": "writes the report"}),
        )
        .await
        .unwrap();
        assert_eq!(spawned["member"]["agent_id"], "w2");

        let task_id_num = Uuid::new_v4();
        let created = call(
            &view,
            "team_task",
            json!({
                "operation": "create",
                "id": task_id_num,
                "title": "collect evidence",
                "description": "deep dive",
                "assignee": "w0"
            }),
        )
        .await
        .unwrap();
        assert_eq!(created["task"]["title"], "collect evidence");

        let run = call(
            &view,
            "team_run_task",
            json!({"task_id": task_id_num, "agent_id": "w0", "mode": "sync"}),
        )
        .await
        .unwrap();
        assert_eq!(run["run"]["status"], "completed");
        assert_eq!(run["result"]["worker_key"], "w0");

        let runs = call(&view, "team_list_runs", json!({})).await.unwrap();
        assert_eq!(runs["runs"].as_array().unwrap().len(), 1);

        call(
            &view,
            "team_send_message",
            json!({"to": "w0", "subject": "handoff", "body": "please collect"}),
        )
        .await
        .unwrap();
        call(
            &view,
            "team_broadcast",
            json!({"subject": "status", "body": "one card is running"}),
        )
        .await
        .unwrap();
        let mailbox = call(&view, "team_read_mailbox", json!({"box": "sent"}))
            .await
            .unwrap();
        assert_eq!(mailbox["messages"].as_array().unwrap().len(), 2);

        call(
            &view,
            "team_mission_log",
            json!({"kind": "progress", "summary": "board started"}),
        )
        .await
        .unwrap();

        let outcome_id = Uuid::new_v4();
        call(
            &view,
            "team_create_outcome",
            json!({
                "id": outcome_id,
                "title": "report",
                "required_sections": ["summary", "evidence"]
            }),
        )
        .await
        .unwrap();
        let fragment = call(
            &view,
            "team_attach_outcome_fragment",
            json!({
                "outcome_id": outcome_id,
                "section": "summary",
                "content": "the team collected evidence"
            }),
        )
        .await
        .unwrap();
        let fragment_id = fragment["fragment"]["id"].as_str().unwrap().to_string();
        call(
            &view,
            "team_review_outcome_fragment",
            json!({"fragment_id": fragment_id, "decision": "reviewed"}),
        )
        .await
        .unwrap();
        call(
            &view,
            "team_attach_outcome_fragment",
            json!({
                "outcome_id": outcome_id,
                "section": "evidence",
                "content": "w0 read the parser"
            }),
        )
        .await
        .unwrap();
        let list = call(&view, "team_list_outcomes", json!({})).await.unwrap();
        assert_eq!(list["outcomes"].as_array().unwrap().len(), 1);
        assert_eq!(list["fragments"].as_array().unwrap().len(), 2);
        let status = call(&view, "team_status", json!({})).await.unwrap();
        assert_eq!(status["members"].as_array().unwrap().len(), 4);

        let result = service
            .finish(dhi_agent::FactDraft {
                summary: "coordinator fact".to_string(),
                detail: "one fact".to_string(),
                evidence: "board and mission".to_string(),
                confidence: "observed".to_string(),
            })
            .await
            .unwrap();
        assert_eq!(result.fact.summary, "coordinator fact");
        assert_eq!(result.worker_results.len(), 1);
        service.cleanup().await;
    }

    #[tokio::test]
    async fn teammate_views_mutate_shared_state_and_cannot_finalize() {
        let task_id = Uuid::new_v4();
        let store: Arc<dyn TeamStore> = Arc::new(MemoryTeamStore::new());
        let service = start_service(config(task_id, 3), store.clone()).await;
        let teammate = service.teammate_view("w0");

        let names: Vec<String> = teammate
            .tool_server()
            .toolset()
            .names()
            .map(str::to_string)
            .collect();
        for coordinator_only in [
            "team_spawn_teammate",
            "team_shutdown_teammate",
            "team_finalize_outcome",
            "team_cleanup",
        ] {
            assert!(
                !names.iter().any(|name| name == coordinator_only),
                "{coordinator_only} leaked to a teammate view"
            );
        }

        let created = call(
            &teammate,
            "team_task",
            json!({
                "operation": "create",
                "title": "teammate card",
                "assignee": "w0"
            }),
        )
        .await
        .unwrap();
        let card = created["task"]["id"].as_str().unwrap().to_string();
        call(
            &teammate,
            "team_task",
            json!({"operation": "claim", "task_id": card}),
        )
        .await
        .unwrap();
        let completed = call(
            &teammate,
            "team_task",
            json!({"operation": "complete", "task_id": card, "summary": "done"}),
        )
        .await
        .unwrap();
        assert_eq!(completed["task"]["status"], "completed");

        call(
            &teammate,
            "team_send_message",
            json!({"to": "lead", "subject": "finding", "body": "clean"}),
        )
        .await
        .unwrap();
        call(
            &teammate,
            "team_mission_log",
            json!({"kind": "done", "summary": "teammate finished"}),
        )
        .await
        .unwrap();

        let outcome = call(
            &teammate,
            "team_create_outcome",
            json!({"title": "teammate report", "required_sections": ["summary"]}),
        )
        .await
        .unwrap();
        let outcome_id = outcome["outcome"]["id"].as_str().unwrap().to_string();
        let fragment = call(
            &teammate,
            "team_attach_outcome_fragment",
            json!({
                "outcome_id": outcome_id,
                "section": "summary",
                "content": "teammate summary"
            }),
        )
        .await
        .unwrap();
        let fragment_id = fragment["fragment"]["id"].as_str().unwrap().to_string();
        call(
            &teammate,
            "team_review_outcome_fragment",
            json!({"fragment_id": fragment_id, "decision": "reviewed"}),
        )
        .await
        .unwrap();

        assert!(
            call(
                &teammate,
                "team_finalize_outcome",
                json!({"outcome_id": outcome_id})
            )
            .await
            .is_err(),
            "the finalize tool is not registered for a teammate"
        );
        assert!(
            call(&teammate, "team_spawn_teammate", json!({"agent_id": "w3"}))
                .await
                .is_err(),
            "the spawn tool is not registered for a teammate"
        );
        service.cleanup().await;
    }

    #[tokio::test]
    async fn coordinator_status_and_stop_flag() {
        let task_id = Uuid::new_v4();
        let store: Arc<dyn TeamStore> = Arc::new(MemoryTeamStore::new());
        let service = start_service(config(task_id, 3), store).await;
        let view = service.coordinator_view();

        call(&view, "team_stop", json!({"reason": "no honest fact"}))
            .await
            .unwrap();
        assert!(service.is_stopped());
        assert_eq!(service.stop_reason().as_deref(), Some("no honest fact"));
        let stopped = service.stopped().await;
        assert_eq!(stopped.reason, "no honest fact");
    }

    #[tokio::test]
    async fn spawn_cap_is_enforced_in_the_host_and_session() {
        let task_id = Uuid::new_v4();
        let store: Arc<dyn TeamStore> = Arc::new(MemoryTeamStore::new());
        let service = start_service(config(task_id, 2), store.clone()).await;
        let view = service.coordinator_view();

        let error = call(
            &view,
            "team_spawn_teammate",
            json!({"agent_id": "w2", "description": "one too many"}),
        )
        .await
        .unwrap_err();
        assert!(
            error.contains("maximum 2 active teammates"),
            "spawn must fail at the run cap: {error}"
        );
        assert_eq!(service.members().await.unwrap().len(), 3);

        call(&view, "team_shutdown_teammate", json!({"agent_id": "w0"}))
            .await
            .unwrap();
        call(
            &view,
            "team_spawn_teammate",
            json!({"agent_id": "w2", "description": "replacement"}),
        )
        .await
        .unwrap();
        assert_eq!(service.members().await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn persisted_team_resumes_board_and_cap() {
        let task_id = Uuid::new_v4();
        let store: Arc<dyn TeamStore> = Arc::new(MemoryTeamStore::new());
        let first = start_service(config(task_id, 3), store.clone()).await;
        let view = first.coordinator_view();
        let card = Uuid::new_v4();
        call(
            &view,
            "team_task",
            json!({
                "operation": "create",
                "id": card,
                "title": "persisted card",
                "assignee": "w0"
            }),
        )
        .await
        .unwrap();
        drop(first);

        let resumed = start_service(config(task_id, 3), store).await;
        let tasks = resumed.tasks().await.unwrap();
        assert!(tasks.iter().any(|task| task.id == card));
        assert_eq!(resumed.members().await.unwrap().len(), 3);
        resumed.cleanup().await;
    }

    struct SlowTeammate;

    #[async_trait::async_trait]
    impl AgentRuntime for SlowTeammate {
        async fn plan(
            &self,
            input: dhi_agent::PlanInput,
        ) -> anyhow::Result<dhi_agent::AgentCall<dhi_agent::PlanOutcome>> {
            MockAgentRuntime.plan(input).await
        }

        async fn execute(
            &self,
            input: ExecuteInput,
        ) -> anyhow::Result<dhi_agent::AgentCall<dhi_agent::ExecuteOutput>> {
            MockAgentRuntime.execute(input).await
        }

        async fn execute_teammate(
            &self,
            input: ExecuteInput,
            _team_tools: Arc<dyn ToolServerView>,
        ) -> anyhow::Result<dhi_agent::AgentCall<dhi_agent::ExecuteOutput>> {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            MockAgentRuntime.execute(input).await
        }

        fn name(&self) -> &str {
            "slow-teammate"
        }
    }

    #[tokio::test]
    async fn await_runs_waits_for_async_work_and_cancel_run_aborts_it() {
        let task_id = Uuid::new_v4();
        let store: Arc<dyn TeamStore> = Arc::new(MemoryTeamStore::new());
        let agent: Arc<dyn AgentRuntime> = Arc::new(SlowTeammate);
        let service = TeamService::start(agent, store, config(task_id, 3), Vec::new(), None)
            .await
            .unwrap();
        let view = service.coordinator_view();

        let quick = Uuid::new_v4();
        call(
            &view,
            "team_task",
            json!({"operation": "create", "id": quick, "title": "quick", "assignee": "w0"}),
        )
        .await
        .unwrap();
        let quick_run = call(
            &view,
            "team_run_task",
            json!({"task_id": quick, "agent_id": "w0", "mode": "async"}),
        )
        .await
        .unwrap();
        let quick_id = quick_run["run"]["id"].as_str().unwrap().to_string();
        let awaited = call(&view, "team_await_runs", json!({"run_ids": [quick_id]}))
            .await
            .unwrap();
        assert_eq!(awaited["runs"].as_array().unwrap().len(), 1);
        assert_eq!(awaited["runs"][0]["status"], "completed");

        let slow = Uuid::new_v4();
        call(
            &view,
            "team_task",
            json!({"operation": "create", "id": slow, "title": "slow", "assignee": "w1"}),
        )
        .await
        .unwrap();
        let slow_run = call(
            &view,
            "team_run_task",
            json!({"task_id": slow, "agent_id": "w1", "mode": "async"}),
        )
        .await
        .unwrap();
        let slow_id = slow_run["run"]["id"].as_str().unwrap().to_string();
        let cancelled = call(&view, "team_cancel_run", json!({"run_id": slow_id}))
            .await
            .unwrap();
        assert_eq!(cancelled["run"]["status"], "cancelled");
        service.cleanup().await;
    }
}
