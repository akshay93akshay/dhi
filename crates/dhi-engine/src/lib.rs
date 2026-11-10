//! DHI task engine.
//!
//! The engine drives one task:
//! plan -> one intent -> one worker -> guarded tools -> fact -> wake planner.
//! Completion is a proof: a confirmed fact makes a goal `met`, and the task is
//! `done` only when every goal is `met`.
//!
//! Tool calls reach the backend only through the guarded `ToolExecutor`. The
//! loop hibernates on an in-process `Notify` wake signal or a heartbeat.
//! There is no durable event stream and no cross-process wake: a restarted
//! engine re-reads PostgreSQL and relies on the heartbeat plus
//! `ResetRunningIntents` recovery. Activity rows record what happened.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use dhi_agent::{AgentCall, AgentCallRecord, AgentRuntime, ExecuteInput, PlanInput, PlanOutcome};
use dhi_core::{
    AgentRun, ApprovalDecision, Fact, FactState, Goal, GoalState, GraphStore, GuardStore, Intent,
    IntentPlan, IntentState, Task, TaskCapacity, TaskState, ToolCallOutcome, ToolCallRequest,
    Topology,
};
use dhi_guard::ToolExecutor;
use dhi_orchestrator::{
    IntentResult, ManagerRunner, SwarmMessage, SwarmRunner, TeamSelector, WorkerStatus,
};
use tokio::sync::Notify;
use tracing::warn;
use uuid::Uuid;

mod team;
mod team_host;

use team::RuntimeWorkerExecutor;
use team_host::TeamService;

const DEFAULT_HEARTBEAT: Duration = Duration::from_secs(30);

/// Consecutive idle planner ticks an engine tolerates before it gives up.
/// Idle waiting never spends a round, so without this bound a planner that
/// always answers `done: false` with no intents would hibernate forever.
const DEFAULT_MAX_IDLE_TICKS: usize = 120;

/// Consecutive spent rounds without a new confirmed fact before the engine
/// stops a task. This is a safety ceiling, not a work target.
const DEFAULT_MAX_NO_PROGRESS_ROUNDS: usize = 16;

/// Backoff after losing a claim race, so an engine never busy-spins on the
/// same open intent.
const CLAIM_BACKOFF: Duration = Duration::from_millis(50);

/// True when the server asked this run to stop. A missing flag means the
/// caller did not wire cancellation (`run_task`).
fn stop_requested(stop: Option<&Arc<AtomicBool>>) -> bool {
    stop.is_some_and(|flag| flag.load(Ordering::SeqCst))
}

/// Confirmed facts are the engine's progress measure; the task's origin fact
/// is not confirmed.
fn confirmed_fact_count(facts: &[Fact]) -> usize {
    facts
        .iter()
        .filter(|fact| fact.state == FactState::Confirmed)
        .count()
}

/// Caps one `tool_allowed` activity payload's tool value preview. The durable
/// `tool_calls` row keeps the full value; the feed only shows a short preview.
fn activity_preview(value: &serde_json::Value) -> serde_json::Value {
    const MAX_CHARS: usize = 500;
    let text = match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    if text.chars().count() <= MAX_CHARS {
        value.clone()
    } else {
        serde_json::json!({
            "truncated": true,
            "preview": text.chars().take(MAX_CHARS).collect::<String>(),
        })
    }
}

/// Enforce the task's creation-time topology on the planner's plan.
///
/// The planner tool guard already rejects most violations; this second check
/// protects every other `AgentRuntime` and fails the task with a visible
/// reason instead of executing a plan the user did not ask for.
fn enforce_required_topology(
    required: Option<Topology>,
    plan: Option<IntentPlan>,
) -> Result<Option<IntentPlan>, String> {
    match required {
        None => Ok(plan),
        Some(Topology::Single) => Ok(None),
        Some(required) => match plan {
            Some(plan) if plan.topology == required => Ok(Some(plan)),
            Some(plan) => Err(format!(
                "required topology {required}, but the planner declared {}",
                plan.topology
            )),
            None => Err(format!(
                "required topology {required}, but the planner supplied no plan"
            )),
        },
    }
}

/// Enforce a user-required swarm worker count on a plan about to execute.
///
/// The planner tool guard already rejects a mismatched `add_intent` plan; this
/// second check protects every other `AgentRuntime`, a restarted engine that
/// loaded a persisted plan, and a plan that bypassed the tool. The lead/team
/// runtimes let a plan start with no subtasks (up to one per teammate), so the
/// worker count is exact while subtasks may be `0..=N`.
fn enforce_swarm_worker_count(required: Option<usize>, plan: &IntentPlan) -> Result<(), String> {
    let Some(required) = required else {
        // Auto still has a hard floor: the plan validation requires two
        // teammates, and the engine enforces the same rule before running a
        // runtime-supplied or persisted plan.
        if plan.topology == Topology::Swarm && plan.workers.len() < dhi_core::SWARM_MIN_WORKERS {
            return Err(format!(
                "swarm topology requires at least {} teammates, got {}; refusing to run the \
                 swarm",
                dhi_core::SWARM_MIN_WORKERS,
                plan.workers.len()
            ));
        }
        return Ok(());
    };
    if !dhi_core::valid_swarm_worker_count(required) {
        return Err(format!(
            "required swarm worker count {required} is outside {}..={}; refusing to run the \
             swarm",
            dhi_core::SWARM_MIN_WORKERS,
            dhi_core::SWARM_MAX_WORKERS
        ));
    }
    if plan.topology != Topology::Swarm {
        return Ok(());
    }
    if plan.workers.len() != required || plan.subtasks.len() > required {
        return Err(format!(
            "required swarm worker count {required}, but the plan declares {} workers and {} \
             subtasks; refusing to run the swarm",
            plan.workers.len(),
            plan.subtasks.len()
        ));
    }
    Ok(())
}

/// Fact context for one DHI+ subworker.
///
/// The origin fact is the task's full goal text and names every worker and
/// topic, so a subworker that reads it can drift onto a peer's topic. Only
/// confirmed facts are shared context; the assigned subtask belongs in the
/// worker's intent, not here.
fn team_fact_context(facts: &[Fact]) -> Vec<String> {
    facts
        .iter()
        .filter(|fact| fact.state == FactState::Confirmed)
        .map(|fact| fact.claim.clone())
        .collect()
}

/// True when a store rejected a second fact for one intent.
///
/// Both stores surface `CoreError::FactAlreadyRecordedForIntent`; the
/// PostgreSQL store wraps it in the `facts_one_per_intent` unique-index error.
fn is_duplicate_intent_fact(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<dhi_core::CoreError>()
            .is_some_and(|err| matches!(err, dhi_core::CoreError::FactAlreadyRecordedForIntent(_)))
    })
}

/// The `agent_runs` row one recorded call becomes.
///
/// The row id is the record's `run_id` and the run it belongs to is the same
/// value, so `call_index` is 0. There is no checkpoint. `variables` is empty
/// because the engine keeps no rendered variables; the rendered prompt is
/// stored in full instead. `worker_key` names the DHI+ subworker that made the
/// call; the planner and the single-worker path pass their own value.
fn agent_run<T>(
    task_id: Uuid,
    intent_id: Option<Uuid>,
    worker_key: Option<&str>,
    record: &AgentCallRecord,
    call: &AgentCall<T>,
) -> AgentRun {
    let now = Utc::now();
    AgentRun {
        id: record.run_id,
        run_id: record.run_id,
        call_index: record.call_index,
        task_id,
        intent_id,
        worker_key: worker_key.map(str::to_owned),
        role: dhi_agent::record_role(record.role),
        model: record.model.clone(),
        profile: record.profile.clone(),
        prompt: call.prompt.clone(),
        prompt_hash: record.prompt_hash.clone(),
        variables: serde_json::json!({}),
        response: Some(call.response.clone()),
        thinking: call.thinking.clone().filter(|text| !text.trim().is_empty()),
        tokens_in: record.usage.input_tokens,
        tokens_out: record.usage.output_tokens,
        cache_read_tokens: record.usage.cache_read_tokens,
        cache_write_tokens: record.usage.cache_write_tokens,
        reasoning_tokens: record.usage.reasoning_tokens,
        cost: record.usage.cost_usd.unwrap_or(0.0),
        status: record.status.clone(),
        error: record.error.clone(),
        latency_ms: record.latency_ms,
        checkpoint_id: None,
        created_at: now,
        updated_at: now,
    }
}

/// Records every completion call a runtime made for one lead/manager turn.
///
/// Usage accounting is best effort: a failed save is logged and never fails
/// the decision the model already produced.
async fn record_runtime_calls<T>(
    store: &dyn GraphStore,
    task_id: Uuid,
    intent_id: Option<Uuid>,
    worker_key: &str,
    call: &AgentCall<T>,
) {
    for record in &call.meta.calls {
        let run = agent_run(task_id, intent_id, Some(worker_key), record, call);
        if let Err(err) = store.save_agent_run(&run).await {
            warn!(
                run_id = %record.run_id,
                role = record.role.as_str(),
                task_id = %task_id,
                "could not record {worker_key} agent run: {err}"
            );
        }
    }
}

/// Evidence for the one fact a team run records.
///
/// The deterministic aggregator already lists failed and skipped results;
/// the explicit failed-worker line keeps the failure visible even if an
/// aggregator implementation only returns successful evidence.
fn team_evidence(result: &IntentResult) -> String {
    let mut evidence = result.fact.evidence.trim().to_owned();
    let failed: Vec<String> = result
        .worker_results
        .iter()
        .filter(|worker| worker.status == WorkerStatus::Failed)
        .map(|worker| match worker.error.as_deref() {
            Some(error) if !error.is_empty() => format!("{} ({error})", worker.worker_key),
            _ => format!("{} (unknown error)", worker.worker_key),
        })
        .collect();
    if !failed.is_empty() {
        if !evidence.is_empty() {
            evidence.push('\n');
        }
        evidence.push_str(&format!("failed workers: {}", failed.join(", ")));
    }
    if evidence.is_empty() {
        evidence = "no worker evidence".to_owned();
    }
    evidence
}

/// Fact provenance for a plan-executed intent.
fn team_provenance(intent_id: Uuid, plan: &IntentPlan, result: &IntentResult) -> serde_json::Value {
    let workers: Vec<serde_json::Value> = result
        .worker_results
        .iter()
        .map(|worker_result| {
            let role = plan
                .workers
                .iter()
                .find(|worker| worker.key == worker_result.worker_key)
                .map(|worker| worker.role.clone())
                .unwrap_or_default();
            let subtask = plan
                .subtasks
                .iter()
                .find(|subtask| subtask.id == worker_result.subtask_id)
                .map(|subtask| subtask.description.clone())
                .unwrap_or_else(|| worker_result.subtask_id.clone());
            serde_json::json!({
                "key": worker_result.worker_key,
                "role": role,
                "status": worker_result.status.to_string(),
                "subtask": subtask,
                "error": worker_result.error,
            })
        })
        .collect();

    serde_json::json!({
        "intent_id": intent_id,
        "detail": result.fact.detail,
        "evidence": team_evidence(result),
        "confidence": result.fact.confidence,
        "topology": plan.topology.as_str(),
        "workers": workers,
    })
}

/// Narrow a plan's parallel budget to the task's DHI+ capacity.
///
/// Only `max_parallel` is constrained; the other ledger fields are enforced
/// by the engine's own capacity checks. A zero value is rejected at the API,
/// but ignoring it here keeps a corrupt ledger from making a plan invalid.
fn apply_task_capacity(
    mut plan: dhi_core::IntentPlan,
    capacity: dhi_core::TaskCapacity,
) -> dhi_core::IntentPlan {
    if let Some(max_parallel_workers) = capacity.max_parallel_workers.filter(|limit| *limit > 0) {
        plan.limits.max_parallel = plan.limits.max_parallel.min(max_parallel_workers);
    }
    plan
}

/// The plan the team runner actually executes.
///
/// A swarm runs every teammate at once, so its `max_parallel` is the team
/// size and the DHI+ task capacity never throttles it below that. For a fixed
/// user count N, the plan limits are raised to admit N even when the planner
/// left the default `max_workers` (8). Auto leaves `max_workers` alone: the
/// planner's own cap (8 without approval) still decides how wide the team may
/// be. Manager/worker plans keep the existing capacity clamp.
fn effective_team_plan(
    mut plan: dhi_core::IntentPlan,
    task: &dhi_core::Task,
) -> dhi_core::IntentPlan {
    if plan.topology == Topology::Swarm {
        match task.swarm_workers {
            Some(workers) if workers > 0 => {
                plan.limits.max_workers = plan.limits.max_workers.max(workers);
                plan.limits.max_parallel = workers;
            }
            _ => {
                let workers = plan.workers.len();
                if workers > 0 {
                    plan.limits.max_parallel = workers;
                }
            }
        }
        return plan;
    }
    apply_task_capacity(plan, task.capacity)
}

/// How often a waiting engine checks the task stop flag while the scale gate
/// is pending. The gate itself polls its row every 500 ms.
const SCALE_GATE_STOP_POLL: Duration = Duration::from_millis(200);

/// Truncates a swarm plan to the teammate cap an Auto swarm may actually use.
///
/// The team runner takes its roster from the plan's workers; subtasks are not
/// seeds in that runtime, but they are trimmed with the roster so a plan never
/// names a removed teammate.
fn cap_swarm_plan(mut plan: dhi_core::IntentPlan, cap: usize) -> dhi_core::IntentPlan {
    let cap = cap.clamp(1, dhi_core::SWARM_MAX_WORKERS);
    if plan.workers.len() > cap {
        plan.workers.truncate(cap);
    }
    let workers = plan.workers.len();
    if plan.subtasks.len() > workers {
        plan.subtasks.truncate(workers);
    }
    if workers > 0 {
        plan.limits.max_workers = plan.limits.max_workers.max(workers);
        plan.limits.max_parallel = workers;
    }
    plan
}

/// Reads the selected project's one `AGENTS.md` for a task start.
///
/// The engine calls this once per `run_task`, before the first planner round.
/// A missing file is `not_found` and adds no prompt section. A read failure
/// is logged, recorded as `not_found`, and never blocks the task. The state is
/// written back only when it changed, so a file that grew past the cap after
/// creation becomes `truncated`.
async fn load_project_instructions(
    store: &dyn GraphStore,
    task: &mut dhi_core::Task,
) -> Option<String> {
    if !task.load_agents_md {
        return None;
    }
    let folder = task.project_folder.as_deref()?;
    match dhi_core::read_agents_md(std::path::Path::new(folder)) {
        Ok(content) => {
            if task.agents_md_state != content.state {
                if let Err(err) = store.set_task_agents_md_state(task.id, content.state).await {
                    tracing::warn!(
                        task_id = %task.id,
                        error = %err,
                        "could not record the AGENTS.md state"
                    );
                }
                task.agents_md_state = content.state;
            }
            let mut text = content.text;
            if content.state == dhi_core::AgentsMdState::Truncated {
                text.push_str(
                    "\n\n[AGENTS.md was longer than 32 KiB and the text above was truncated.]",
                );
            }
            content.state.has_instructions().then_some(text)
        }
        Err(err) => {
            tracing::warn!(
                task_id = %task.id,
                folder,
                error = %err,
                "could not read the project AGENTS.md; continuing without it"
            );
            if task.agents_md_state != dhi_core::AgentsMdState::NotFound {
                if let Err(err) = store
                    .set_task_agents_md_state(task.id, dhi_core::AgentsMdState::NotFound)
                    .await
                {
                    tracing::warn!(
                        task_id = %task.id,
                        error = %err,
                        "could not record the missing AGENTS.md state"
                    );
                }
                task.agents_md_state = dhi_core::AgentsMdState::NotFound;
            }
            None
        }
    }
}

/// The persisted `kind` of one typed swarm message.
///
/// `Shutdown` is runtime control and has no persisted kind; a swarm run never
/// logs it, but the defensive `None` keeps the mapping total.
fn swarm_message_kind(message: &SwarmMessage) -> Option<&'static str> {
    match message {
        SwarmMessage::Finding { .. } => Some("finding"),
        SwarmMessage::Question { .. } => Some("question"),
        SwarmMessage::Challenge { .. } => Some("challenge"),
        SwarmMessage::Done { .. } => Some("done"),
        SwarmMessage::Shutdown => None,
    }
}

/// Chooses the DHI+ manager's actual workers through the task's agent runtime.
///
/// This phase runs manager selection on the worker role and the task's pinned
/// worker profile; a dedicated manager role is a later refinement. The
/// runtime call is recorded here because the manager runner owns only the
/// selected plan, not the completion call that produced it. A failed selection
/// is returned so the task fails visibly.
struct RuntimeTeamSelector {
    store: Arc<dyn GraphStore>,
    agent: Arc<dyn AgentRuntime>,
    task_id: Uuid,
    intent_id: Uuid,
    intent: String,
    worker_profile_id: Option<i64>,
    /// Completion calls the manager selection made. The runner owns only the
    /// selected plan, so the engine reads this after the run.
    model_calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl TeamSelector for RuntimeTeamSelector {
    async fn select(&self, plan: &IntentPlan) -> anyhow::Result<IntentPlan> {
        let call = self
            .agent
            .select_team(dhi_agent::TeamSelectionInput {
                task_id: self.task_id,
                intent_id: self.intent_id,
                intent: self.intent.clone(),
                plan: plan.clone(),
                worker_profile_id: self.worker_profile_id,
            })
            .await
            .map_err(|err| anyhow::anyhow!("manager selection failed: {err}"))?;

        self.model_calls
            .fetch_add(call.meta.calls.len(), Ordering::SeqCst);

        for record in &call.meta.calls {
            let run = agent_run(
                self.task_id,
                Some(self.intent_id),
                Some("manager"),
                record,
                &call,
            );
            if let Err(err) = self.store.save_agent_run(&run).await {
                warn!(
                    run_id = %record.run_id,
                    role = record.role.as_str(),
                    task_id = %self.task_id,
                    "could not record manager agent run: {err}"
                );
            }
        }
        Ok(call.output)
    }
}

/// Runs one manager-worker lead review through the task's agent runtime.
///
/// The lead is a planner-role model call on the task's pinned planner profile.
/// Every completion record becomes an `agent_runs` row with worker key
/// `manager`, and the call count feeds the task's model-call ledger.
struct RuntimeTeamLead {
    store: Arc<dyn GraphStore>,
    agent: Arc<dyn AgentRuntime>,
    task_id: Uuid,
    intent_id: Uuid,
    planner_profile_id: Option<i64>,
    model_calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl dhi_orchestrator::TeamLead for RuntimeTeamLead {
    async fn review(
        &self,
        input: &dhi_orchestrator::LeadInput,
    ) -> anyhow::Result<dhi_orchestrator::LeadDecision> {
        let call = self
            .agent
            .review_lead(dhi_agent::LeadReviewInput {
                task_id: self.task_id,
                intent_id: self.intent_id,
                input: input.clone(),
                planner_profile_id: self.planner_profile_id,
            })
            .await
            .map_err(|err| anyhow::anyhow!("manager lead review failed: {err}"))?;

        self.model_calls
            .fetch_add(call.meta.calls.len(), Ordering::SeqCst);
        record_runtime_calls(
            self.store.as_ref(),
            self.task_id,
            Some(self.intent_id),
            "manager",
            &call,
        )
        .await;
        Ok(call.output)
    }
}

pub struct Engine {
    store: Arc<dyn GraphStore>,
    agent: Arc<dyn AgentRuntime>,
    /// The one guard. The policy lives inside the executor, at the
    /// tool-call point; the engine never evaluates policy itself.
    executor: Option<Arc<dyn ToolExecutor>>,
    guard_store: Option<Arc<dyn GuardStore>>,
    /// In-process wake only. `wake()` nudges this engine; it does not reach
    /// any other process.
    notify: Arc<Notify>,
    heartbeat: Duration,
    max_rounds: usize,
    max_idle_ticks: usize,
    /// Consecutive spent rounds allowed without a new confirmed fact.
    max_no_progress_rounds: usize,
    /// Optional manager choice. `None` keeps `ManagerRunner::new` and the
    /// planner's plan unchanged, so the default path is untouched.
    manager_selector: Option<Arc<dyn TeamSelector>>,
    /// True when the agent runtime implements the manager-worker lead review.
    lead_runtime: bool,
    /// Cline Teams persistence. `None` falls back to the legacy peer swarm.
    team_store: Option<Arc<dyn dhi_core::team::TeamStore>>,
    /// `swarm_scale` gate for an Auto swarm above the approval-free cap.
    scale_gate: Option<Arc<dyn dhi_core::ScaleApprovalGate>>,
    /// In-memory cache of DHI+ plans keyed by the intent the planner created.
    ///
    /// Every planned intent also has a durable `intent_plans` row. The cache is
    /// dropped after the intent finishes; a restarted engine loads the plan
    /// from the store when it claims the intent again.
    plans: Mutex<HashMap<Uuid, IntentPlan>>,
}

#[derive(Debug, Clone)]
pub struct EngineResult {
    pub task: Task,
    pub rounds: usize,
    pub facts: usize,
    pub intents: usize,
}

impl Engine {
    pub fn new(store: Arc<dyn GraphStore>, agent: Arc<dyn AgentRuntime>) -> Self {
        Self {
            store,
            agent,
            executor: None,
            guard_store: None,
            notify: Arc::new(Notify::new()),
            heartbeat: DEFAULT_HEARTBEAT,
            max_rounds: 16,
            max_idle_ticks: DEFAULT_MAX_IDLE_TICKS,
            max_no_progress_rounds: DEFAULT_MAX_NO_PROGRESS_ROUNDS,
            manager_selector: None,
            lead_runtime: false,
            team_store: None,
            scale_gate: None,
            plans: Mutex::new(HashMap::new()),
        }
    }

    /// Remembers a planner-proposed execution plan for its new intent.
    fn remember_plan(&self, intent_id: Uuid, plan: IntentPlan) {
        self.plans
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(intent_id, plan);
    }

    /// The cached plan attached to `intent_id`, if this engine still has it.
    fn plan_for(&self, intent_id: Uuid) -> Option<IntentPlan> {
        self.plans
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&intent_id)
            .cloned()
    }

    /// Drops the in-memory cache entry once its intent can no longer be
    /// executed. The persisted plan row stays as audit data.
    fn forget_plan(&self, intent_id: Uuid) {
        self.plans
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&intent_id);
    }

    /// Records the runtime calls of a team result batch and returns how many
    /// completion calls it counted against the model-call ledger.
    async fn record_result_calls(
        &self,
        task_id: Uuid,
        intent_id: Uuid,
        workers: &[&dhi_orchestrator::WorkerResult],
    ) -> usize {
        let mut calls = 0;
        for worker in workers {
            if worker.call_meta.calls.is_empty() {
                continue;
            }
            calls += worker.call_meta.calls.len();
            let call: AgentCall<()> = AgentCall {
                output: (),
                meta: worker.call_meta.clone(),
                prompt: worker.prompt.clone(),
                response: worker.response.clone(),
                thinking: worker.thinking.clone(),
            };
            self.record_agent_calls(
                task_id,
                Some(intent_id),
                Some(worker.worker_key.as_str()),
                &call,
            )
            .await;
        }
        calls
    }

    /// Ends one intent after a lead/team `Stop` without failing the task.
    ///
    /// The intent moves to `stopped`, the cached plan is dropped, and one
    /// `team_stopped` activity row records the reason. The task stays running
    /// so the planner can choose another step.
    async fn stop_intent_for_team(
        &self,
        task_id: Uuid,
        intent_id: Uuid,
        reason: &str,
        worker_results: &[dhi_orchestrator::WorkerResult],
    ) -> anyhow::Result<()> {
        let refs: Vec<&dhi_orchestrator::WorkerResult> = worker_results.iter().collect();
        let _ = self.record_result_calls(task_id, intent_id, &refs).await;
        self.store
            .finish_intent(intent_id, IntentState::Stopped)
            .await?;
        self.forget_plan(intent_id);
        self.log_activity(
            task_id,
            "team_stopped",
            format!("team stopped the intent: {reason}"),
            serde_json::json!({
                "intent_id": intent_id,
                "reason": reason,
                "workers": worker_results.len(),
            }),
        )
        .await?;
        Ok(())
    }

    /// Applies the `swarm_scale` gate to an Auto swarm plan.
    ///
    /// A fixed count never calls the gate. For Auto, a plan of at most
    /// [`dhi_core::SWARM_AUTO_WORKERS`] runs unchanged; a wider plan asks for
    /// one approval, waits with stop-flag checks, and is capped to the granted
    /// count. Denial, expiry, or a missing gate keeps the approval-free cap.
    async fn apply_scale_gate(
        &self,
        task: &Task,
        plan: &IntentPlan,
        stop: Option<&Arc<AtomicBool>>,
    ) -> anyhow::Result<IntentPlan> {
        if plan.topology != Topology::Swarm || task.swarm_workers.is_some() {
            return Ok(plan.clone());
        }

        let requested = plan.workers.len().min(dhi_core::SWARM_MAX_WORKERS);
        if requested <= dhi_core::SWARM_AUTO_WORKERS {
            return Ok(plan.clone());
        }

        let Some(gate) = self.scale_gate.as_ref() else {
            return Ok(cap_swarm_plan(plan.clone(), dhi_core::SWARM_AUTO_WORKERS));
        };

        if stop_requested(stop) {
            anyhow::bail!("stop requested before requesting swarm_scale approval");
        }

        let wait = gate.request_and_wait_scale_approval(dhi_core::ScaleApprovalRequest {
            task_id: task.id,
            team_id: Some(task.id),
            requested_workers: requested,
            current_cap: dhi_core::SWARM_AUTO_WORKERS,
            reason: format!(
                "auto swarm requested {requested} teammates; the approval-free cap is {}",
                dhi_core::SWARM_AUTO_WORKERS
            ),
            timeout_secs: None,
        });

        // The gate waits until a person decides or the request expires. Poll
        // the task stop flag alongside it so a stopped task never hangs here.
        tokio::pin!(wait);
        let decided = loop {
            tokio::select! {
                result = &mut wait => break result?,
                _ = tokio::time::sleep(SCALE_GATE_STOP_POLL) => {
                    if stop_requested(stop) {
                        anyhow::bail!("stop requested while waiting for swarm_scale approval");
                    }
                }
            }
        };

        let cap = decided.effective_cap().min(dhi_core::SWARM_MAX_WORKERS);
        Ok(cap_swarm_plan(plan.clone(), cap))
    }

    /// The maximum active teammates this run may have.
    ///
    /// Fixed N is exactly N. Auto keeps the approval-free cap of 8 even when
    /// the planner seeds fewer, and uses the post-gate plan size after an
    /// allow or denial. The hard cap is 64.
    fn effective_teammate_cap(task: &Task, plan: &IntentPlan) -> usize {
        match task.swarm_workers {
            Some(workers) => workers,
            None => plan.workers.len().max(dhi_core::SWARM_AUTO_WORKERS),
        }
        .clamp(dhi_core::SWARM_MIN_WORKERS, dhi_core::SWARM_MAX_WORKERS)
    }

    /// Builds the team config with the engine's effective teammate cap.
    ///
    /// The cap is fixed N for a fixed swarm, 8 for an ungranted Auto swarm,
    /// and the granted N after an allow. `TeamRunConfig` enforces it on seeded
    /// teammates and on later coordinator spawns, so the coordinator cannot
    /// widen the team past the engine's decision.
    fn team_run_config(
        task_id: Uuid,
        intent_id: Uuid,
        plan: &IntentPlan,
        teammate_cap: usize,
    ) -> dhi_orchestrator::TeamRunConfig {
        let cap = teammate_cap.clamp(dhi_core::SWARM_MIN_WORKERS, dhi_core::SWARM_MAX_WORKERS);
        let mut config = dhi_orchestrator::TeamRunConfig::from_plan(task_id, intent_id, plan)
            .with_max_teammates(cap);
        if config.teammates.len() > cap {
            config.teammates.truncate(cap);
        }
        // The team contract is all N teammates in parallel, including Auto
        // teammates the coordinator spawns after the seed wave. The active
        // cap is the only parallelism bound that matters here.
        config.max_parallel = cap;
        config
    }

    /// Runs a non-single plan through its topology's runner.
    ///
    /// Returns the run result, the effective plan (the planner's plan narrowed
    /// by the task's DHI+ capacity, or widened to the fixed swarm size), and
    /// whether a zero-worker final result is valid. The stored plan is never
    /// changed. The caller owns the model-call counter so a lead/team `Stop`
    /// still charges the calls it made before ending the intent.
    #[allow(clippy::too_many_arguments)]
    async fn run_team_plan(
        &self,
        task_id: Uuid,
        task: &Task,
        intent: &Intent,
        plan: &IntentPlan,
        facts: &[Fact],
        model_calls: &Arc<AtomicUsize>,
        stop: Option<&Arc<AtomicBool>>,
    ) -> anyhow::Result<(IntentResult, IntentPlan, bool)> {
        let plan = effective_team_plan(plan.clone(), task);
        let plan = self.apply_scale_gate(task, &plan, stop).await?;
        let executor = Arc::new(RuntimeWorkerExecutor {
            agent: Arc::clone(&self.agent),
            task_id,
            intent_id: intent.id,
            context: team_fact_context(facts),
            worker_profile_id: task.pinned_worker_profile_id,
        });
        let (result, allow_empty_result) = match plan.topology {
            Topology::ManagerWorker => {
                if let Some(selector) = self.manager_selector.as_ref() {
                    // A test (or host) override always wins over the runtime
                    // manager and the lead. It reports no selection meta, so
                    // it counts zero.
                    (
                        ManagerRunner::with_selector(executor, Arc::clone(selector))
                            .run(&plan)
                            .await,
                        false,
                    )
                } else if self.lead_runtime {
                    let lead = RuntimeTeamLead {
                        store: Arc::clone(&self.store),
                        agent: Arc::clone(&self.agent),
                        task_id,
                        intent_id: intent.id,
                        planner_profile_id: task.pinned_planner_profile_id,
                        model_calls: Arc::clone(model_calls),
                    };
                    (
                        ManagerRunner::new(executor)
                            .with_lead(lead)
                            .run(&plan)
                            .await,
                        true,
                    )
                } else {
                    let selector = RuntimeTeamSelector {
                        store: Arc::clone(&self.store),
                        agent: Arc::clone(&self.agent),
                        task_id,
                        intent_id: intent.id,
                        intent: plan.intent.clone(),
                        worker_profile_id: task.pinned_worker_profile_id,
                        model_calls: Arc::clone(model_calls),
                    };
                    (
                        ManagerRunner::with_selector(executor, selector)
                            .run(&plan)
                            .await,
                        false,
                    )
                }
            }
            Topology::Swarm => match self.team_store.as_ref() {
                Some(store) => {
                    let teammate_cap = Self::effective_teammate_cap(task, &plan);
                    let config = Self::team_run_config(task_id, intent.id, &plan, teammate_cap);
                    let mut context = team_fact_context(facts);
                    context.push(format!(
                        "Coordinator id: {} (the persisted team role is `lead`; it is the coordinator)",
                        config.lead.agent_id
                    ));
                    context.push(format!(
                        "Teammate cap: {} active teammates; the coordinator is extra",
                        config.max_teammates
                    ));
                    context.push(
                        "A stopped teammate stays stopped until you call team_spawn_teammate again."
                            .to_string(),
                    );
                    let roster = config
                        .teammates
                        .iter()
                        .map(|teammate| {
                            format!("{}: {}", teammate.agent_id, teammate.description.trim())
                        })
                        .collect::<Vec<_>>()
                        .join("; ");
                    context.push(format!("Seeded roster: {roster}"));
                    let service = TeamService::start(
                        Arc::clone(&self.agent),
                        Arc::clone(store),
                        config.clone(),
                        context.clone(),
                        task.pinned_worker_profile_id,
                    )
                    .await?;
                    let view: Arc<dyn dhi_agent::ToolServerView> =
                        Arc::new(service.coordinator_view());
                    let executed = self
                        .agent
                        .execute_coordinator(
                            ExecuteInput {
                                task_id,
                                intent_id: intent.id,
                                intent: config.objective.clone(),
                                context,
                                worker_profile_id: task.pinned_worker_profile_id,
                                max_turns: None,
                                agent_id: None,
                                role: None,
                                task: None,
                            },
                            view,
                        )
                        .await;
                    let call = match executed {
                        Ok(call) => call,
                        Err(err) => {
                            if service.is_stopped() {
                                return Err(service.stopped().await.into());
                            }
                            service.cleanup().await;
                            return Err(err);
                        }
                    };
                    // The coordinator is a normal worker-role call; record
                    // its model usage under `coordinator` and charge it to
                    // the task's model-call ledger.
                    model_calls.fetch_add(call.meta.calls.len(), Ordering::SeqCst);
                    record_runtime_calls(
                        self.store.as_ref(),
                        task_id,
                        Some(intent.id),
                        "coordinator",
                        &call,
                    )
                    .await;
                    if service.is_stopped() {
                        return Err(service.stopped().await.into());
                    }
                    // One fact, from the coordinator's `record_fact`; the
                    // settled teammate results travel as provenance and
                    // capacity evidence. A teammate stop that lands while an
                    // async run is settling still wins: no fact is recorded.
                    let result = service.finish(call.output.fact).await;
                    if service.is_stopped() {
                        return Err(service.stopped().await.into());
                    }
                    service.cleanup().await;
                    (result, true)
                }
                None => {
                    // Legacy fallback for hosts without team persistence. The
                    // plan is still capped by the scale gate above, and a
                    // zero-worker final is not valid for a peer swarm.
                    let runner = SwarmRunner::new(executor);
                    let result = runner.run(&plan).await;
                    self.persist_swarm_messages(task_id, intent.id, runner.messages())
                        .await;
                    (result, false)
                }
            },
            Topology::Single => unreachable!("single plans use the worker path"),
        };
        Ok((result?, plan, allow_empty_result))
    }

    /// Persists the typed broadcast log of one swarm run.
    ///
    /// `Shutdown` is runtime control, never a logical broadcast, so it is
    /// skipped. A failed save is logged and swallowed: swarm audit data must
    /// never fail the intent that produced it.
    async fn persist_swarm_messages(
        &self,
        task_id: Uuid,
        intent_id: Uuid,
        messages: Vec<SwarmMessage>,
    ) {
        for message in messages {
            let Some(kind) = swarm_message_kind(&message) else {
                continue;
            };
            let payload = match serde_json::to_value(&message) {
                Ok(payload) => payload,
                Err(err) => {
                    tracing::warn!(
                        task_id = %task_id,
                        intent_id = %intent_id,
                        kind,
                        error = %err,
                        "failed to serialize swarm message"
                    );
                    continue;
                }
            };
            if let Err(err) = self
                .store
                .append_swarm_message(task_id, intent_id, kind, payload)
                .await
            {
                tracing::warn!(
                    task_id = %task_id,
                    intent_id = %intent_id,
                    kind,
                    error = %err,
                    "failed to persist swarm message"
                );
            }
        }
    }

    pub fn with_max_rounds(mut self, max_rounds: usize) -> Self {
        self.max_rounds = max_rounds;
        self
    }

    /// How many consecutive spent rounds may pass without a new confirmed
    /// fact before the task fails. Idle ticks are not rounds and never count.
    /// A zero value is treated as one so the guard can never be disabled.
    pub fn with_max_no_progress_rounds(mut self, max_no_progress_rounds: usize) -> Self {
        self.max_no_progress_rounds = max_no_progress_rounds.max(1);
        self
    }

    /// How many consecutive idle planner ticks are tolerated before the task
    /// is timed out. Idle waits do not spend rounds, so this is the only bound
    /// on a planner that never reports work or completion.
    pub fn with_max_idle_ticks(mut self, max_idle_ticks: usize) -> Self {
        self.max_idle_ticks = max_idle_ticks;
        self
    }

    /// Attaches the guarded tool choke point. Required before a task whose
    /// worker emits tool calls can run.
    pub fn with_executor(
        mut self,
        executor: Arc<dyn ToolExecutor>,
        guard_store: Arc<dyn GuardStore>,
    ) -> Self {
        self.executor = Some(executor);
        self.guard_store = Some(guard_store);
        self
    }

    /// Lets a manager selector choose the actual workers and subtasks of
    /// every manager/worker plan. The planner's plan still validates first and
    /// the selected plan is re-validated against the planner's limits.
    ///
    /// This is a local selection seam; it adds no model API. A manager plan
    /// without a selector keeps the default `ManagerRunner::new` path.
    pub fn with_manager_selector<S>(mut self, selector: S) -> Self
    where
        S: TeamSelector + 'static,
    {
        self.manager_selector = Some(Arc::new(selector));
        self
    }

    /// Declares that the agent runtime implements `review_lead`.
    ///
    /// Without this flag the engine keeps the one-shot selector path, because
    /// the default `AgentRuntime::review_lead` refuses. The binary enables it
    /// for the Rig runtime; tests that script lead decisions enable it too.
    pub fn with_lead_runtime(mut self) -> Self {
        self.lead_runtime = true;
        self
    }

    /// Attaches Cline Teams persistence. With a team store the swarm topology
    /// runs through the coordinator-agent team host; without one it keeps the
    /// legacy peer [`SwarmRunner`].
    pub fn with_team_store(mut self, store: Arc<dyn dhi_core::team::TeamStore>) -> Self {
        self.team_store = Some(store);
        self
    }

    /// Attaches the `swarm_scale` approval gate. Without a gate an Auto swarm
    /// above the approval-free cap is capped at that cap.
    pub fn with_scale_gate(mut self, gate: Arc<dyn dhi_core::ScaleApprovalGate>) -> Self {
        self.scale_gate = Some(gate);
        self
    }

    /// How long an idle engine waits before it re-checks work on its own.
    pub fn with_heartbeat(mut self, heartbeat: Duration) -> Self {
        self.heartbeat = heartbeat;
        self
    }

    /// Wakes this engine if it is hibernating on the notify.
    ///
    /// In-process only. A different process, or a restart, cannot observe
    /// this signal; those paths rely on the heartbeat and on
    /// `ResetRunningIntents` recovery.
    ///
    /// One permit is stored so a wake that arrives just before the engine
    /// starts waiting is not lost.
    pub fn wake(&self) {
        self.notify.notify_one();
    }

    /// Appends activity; a thin helper to keep call sites flat.
    async fn log_activity(
        &self,
        task_id: Uuid,
        kind: &str,
        message: String,
        payload: serde_json::Value,
    ) -> anyhow::Result<()> {
        self.store
            .append_activity(task_id, kind, message, payload)
            .await?;
        Ok(())
    }

    /// Records one `agent_runs` row per completion call the runtime made.
    ///
    /// The planner has no intent (`None`); the worker's row names the intent
    /// it was claimed for. `worker_key` is `None` for planner calls, the
    /// subworker key for a DHI+ team call, and `worker-1` for the normal
    /// single-worker path. `prompt` and `response` are the texts the runtime
    /// returned. A failed save is logged and swallowed: usage accounting is
    /// never a reason to fail a plan or lose a worker's fact.
    async fn record_agent_calls<T>(
        &self,
        task_id: Uuid,
        intent_id: Option<Uuid>,
        worker_key: Option<&str>,
        call: &AgentCall<T>,
    ) {
        for record in &call.meta.calls {
            let run = agent_run(task_id, intent_id, worker_key, record, call);
            if let Err(err) = self.store.save_agent_run(&run).await {
                warn!(
                    run_id = %record.run_id,
                    role = record.role.as_str(),
                    task_id = %task_id,
                    "could not record agent run: {err}"
                );
            }
        }
    }

    /// Applies a task state with first-terminal-wins semantics and returns
    /// `true` when the row actually changed. There is no unguarded setter.
    async fn set_task_state_guarded(
        &self,
        task_id: Uuid,
        state: TaskState,
    ) -> anyhow::Result<bool> {
        self.store.set_task_state_guarded(task_id, state).await
    }

    /// Records a visible terminal failure: guarded transition plus one
    /// `task_failed` activity row carrying the reason. Every engine failure
    /// path (provider error, give_up, round cap) goes through here, so the
    /// dashboard can never show a dead loop as
    /// `running`.
    async fn fail_task(
        &self,
        task_id: Uuid,
        task: &mut Task,
        reason: String,
        source: &str,
    ) -> anyhow::Result<()> {
        if self
            .set_task_state_guarded(task_id, TaskState::Failed)
            .await?
        {
            task.state = TaskState::Failed;
        }
        self.log_activity(
            task_id,
            "task_failed",
            reason.clone(),
            serde_json::json!({"reason": reason, "source": source}),
        )
        .await?;
        Ok(())
    }

    /// Fails a task whose intent already has its one fact.
    ///
    /// The intent is never finished `done`: it moves to `stopped`, and the
    /// duplicate reason is recorded on the task.
    async fn fail_duplicate_intent_fact(
        &self,
        task_id: Uuid,
        task: &mut Task,
        intent_id: Uuid,
    ) -> anyhow::Result<String> {
        let reason = format!("fact already recorded for intent {intent_id}");
        self.store
            .finish_intent(intent_id, IntentState::Stopped)
            .await?;
        self.forget_plan(intent_id);
        self.fail_task(task_id, task, reason.clone(), "worker")
            .await?;
        Ok(reason)
    }

    /// Charges one spent round to the no-progress ledger and returns whether
    /// the configured limit is reached. Idle/heartbeat ticks never call this.
    fn note_no_progress(
        &self,
        confirmed: usize,
        last_confirmed: &mut usize,
        no_progress_rounds: &mut usize,
    ) -> bool {
        if confirmed > *last_confirmed {
            *no_progress_rounds = 0;
        } else {
            *no_progress_rounds += 1;
        }
        *last_confirmed = confirmed;
        *no_progress_rounds >= self.max_no_progress_rounds
    }

    /// Records the `no_progress` activity row, fails the task with source
    /// `engine`, and returns the reason for the caller to propagate.
    async fn fail_no_progress(
        &self,
        task_id: Uuid,
        task: &mut Task,
        no_progress_rounds: usize,
        rounds: usize,
    ) -> anyhow::Result<String> {
        let reason = format!(
            "engine made no progress for {} rounds",
            self.max_no_progress_rounds
        );
        self.log_activity(
            task_id,
            "no_progress",
            reason.clone(),
            serde_json::json!({
                "rounds": no_progress_rounds,
                "limit": self.max_no_progress_rounds,
                "round": rounds,
            }),
        )
        .await?;
        self.fail_task(task_id, task, reason.clone(), "engine")
            .await?;
        Ok(reason)
    }

    /// Records an `approval_expired` activity row for an expired approval,
    /// unless one already names it. A repeated sweep is therefore a no-op.
    async fn record_expiry(
        &self,
        task_id: Uuid,
        approval: &dhi_core::Approval,
    ) -> anyhow::Result<()> {
        let approval_id = approval.id.to_string();
        let already = self
            .store
            .list_activity(task_id, Some("approval_expired"))
            .await?
            .iter()
            .any(|activity| activity.payload["approval_id"] == approval_id);
        if already {
            return Ok(());
        }
        self.log_activity(
            task_id,
            "approval_expired",
            format!(
                "approval {} expired; the waiting worker was denied",
                approval.id
            ),
            serde_json::json!({
                "approval_id": approval.id,
                "tool_call_id": approval.tool_call_id,
                "expires_at": approval.expires_at,
                "decided_at": approval.decided_at,
            }),
        )
        .await?;
        Ok(())
    }

    /// Expires approvals of `task_id` whose timer has run out, wakes every
    /// worker waiting on one of them, and records an `approval_expired`
    /// activity row.
    ///
    /// One timer path only: `GuardedExecutor` expires each approval at its
    /// exact deadline. This sweep is the backstop for a restarted process or
    /// a lost timer task, and the only path that records the expiry in the
    /// activity stream, so it runs even after the timer has already won.
    /// The `dhi-server` supervisor calls it on every non-terminal task on a
    /// fixed interval, and the engine also calls it on idle and on heartbeat.
    /// Expiry is scoped by task: a sweep for one task never touches another.
    pub async fn sweep_approvals(&self, task_id: Uuid) -> anyhow::Result<()> {
        // Expire what is due now and wake any worker blocked inside the tool
        // call. The executor owns the in-process waiters; the guard-store
        // fallback serves an engine wired without one.
        let expired = match (self.executor.as_ref(), self.guard_store.as_ref()) {
            (Some(executor), _) => executor.sweep_expired(task_id).await?,
            (None, Some(guard_store)) => {
                guard_store
                    .expire_pending_approvals(task_id, Utc::now())
                    .await?
            }
            (None, None) => return Ok(()),
        };
        for approval in &expired {
            if approval.task_id == task_id {
                self.record_expiry(task_id, approval).await?;
            }
        }
        // An expiry the per-approval timer performed is recorded here too,
        // exactly once.
        if let Some(guard_store) = self.guard_store.as_ref() {
            for approval in guard_store.list_expired_approvals(task_id).await? {
                self.record_expiry(task_id, &approval).await?;
            }
        }

        // Legacy task-pause cleanup: a paused intent whose approval is gone
        // can never be decided, so leaving it paused would hang the task.
        // Blocking approvals never produces this state; older rows might.
        let blocked = self.store.block_orphaned_paused_intents(task_id).await?;
        if blocked > 0 {
            self.log_activity(
                task_id,
                "intent_orphaned",
                format!("{blocked} paused intent(s) had no pending approval; blocked"),
                serde_json::json!({"blocked": blocked}),
            )
            .await?;
        }
        self.wake();
        Ok(())
    }

    /// The capacity limit this run has already reached, if any.
    ///
    /// Checked only between steps. A check in the middle of a model call would
    /// pause a task whose claimed intent has no worker to resume it.
    fn capacity_pause_reason(
        wall_limit: Option<u64>,
        model_call_limit: Option<usize>,
        started: std::time::Instant,
        model_calls: usize,
    ) -> Option<&'static str> {
        if wall_limit.is_some_and(|limit| started.elapsed().as_secs() >= limit) {
            return Some("wall_time");
        }
        if model_call_limit.is_some_and(|limit| model_calls >= limit) {
            return Some("model_calls");
        }
        None
    }

    /// Pauses a task at a capacity boundary.
    ///
    /// This is a normal stop, not a failure: no intent is finished and no
    /// `task_failed` row is written. Extending the capacity and restarting the
    /// loop resumes the open work.
    #[allow(clippy::too_many_arguments)]
    async fn pause_for_capacity(
        &self,
        task_id: Uuid,
        mut task: Task,
        capacity: TaskCapacity,
        reason: &str,
        rounds: usize,
        model_calls: usize,
        started: std::time::Instant,
    ) -> anyhow::Result<EngineResult> {
        if self
            .set_task_state_guarded(task_id, TaskState::Paused)
            .await?
        {
            task.state = TaskState::Paused;
        }
        self.log_activity(
            task_id,
            "capacity_exhausted",
            format!("task paused: {reason} capacity exhausted"),
            serde_json::json!({
                "reason": reason,
                "rounds": rounds,
                "model_calls": model_calls,
                "elapsed_secs": started.elapsed().as_secs(),
                "max_rounds": capacity.max_rounds,
                "max_model_calls": capacity.max_model_calls,
                "max_wall_secs": capacity.max_wall_secs,
            }),
        )
        .await?;
        Ok(EngineResult {
            task,
            rounds,
            facts: self.store.list_facts(task_id).await?.len(),
            intents: self.store.list_open_intents(task_id, 1_000).await?.len(),
        })
    }

    /// Ends a run as `cancelled` and reports it as a normal result.
    ///
    /// The guarded write is first-terminal-wins: if completion raced with the
    /// stop, the task keeps its terminal state. Any intent a worker left
    /// `running` is closed as `stopped`, so a later add-goal reopen does not
    /// leave unclaimable work behind.
    async fn stopped_result(
        &self,
        task_id: Uuid,
        mut task: Task,
        rounds: usize,
        source: &str,
    ) -> anyhow::Result<EngineResult> {
        if self
            .set_task_state_guarded(task_id, TaskState::Cancelled)
            .await?
        {
            task.state = TaskState::Cancelled;
            for intent in self.store.list_intents(task_id, 1_000).await? {
                if intent.state == IntentState::Running {
                    self.store
                        .finish_intent(intent.id, IntentState::Stopped)
                        .await?;
                    self.forget_plan(intent.id);
                }
            }
            self.log_activity(
                task_id,
                "task_stopped",
                format!("task cancelled: {source}"),
                serde_json::json!({"source": source, "round": rounds}),
            )
            .await?;
        }
        Ok(EngineResult {
            task,
            rounds,
            facts: self.store.list_facts(task_id).await?.len(),
            intents: self.store.list_open_intents(task_id, 1_000).await?.len(),
        })
    }

    /// Runs one task once. This is the cancellation-free entry point used by
    /// tests and one-shot callers; the server always passes a stop flag via
    /// [`Engine::run_task_with_stop`].
    pub async fn run_task(&self, task_id: Uuid) -> anyhow::Result<EngineResult> {
        self.run_task_with_stop(task_id, None).await
    }

    /// Runs one task and observes a server-owned stop flag.
    ///
    /// The flag is checked at the top of every loop round, before a planner
    /// result is applied, before an idle wait, and after a worker call. A
    /// long model or tool call may not observe the flag immediately; the
    /// server's hard-kill monitor is the bound for that case.
    ///
    /// A stop is a normal outcome: the task becomes `cancelled`, one
    /// `task_stopped` activity row is appended, and the result is `Ok`. It
    /// never goes through the failure path.
    pub async fn run_task_with_stop(
        &self,
        task_id: Uuid,
        stop: Option<Arc<AtomicBool>>,
    ) -> anyhow::Result<EngineResult> {
        let mut task = self
            .store
            .get_task(task_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("task not found: {task_id}"))?;

        if task.state == TaskState::Created {
            self.store
                .set_task_state_guarded(task_id, TaskState::Running)
                .await?;
            task.state = TaskState::Running;
        }

        if self.store.list_goals(task_id).await?.is_empty() {
            self.store.add_goal(task_id, task.goal.clone()).await?;
        }

        // Project instructions are read exactly once per task start. The
        // worker never receives them: only PlanInput carries the text.
        let project_instructions = load_project_instructions(self.store.as_ref(), &mut task).await;

        let mut rounds = 0;
        let started = std::time::Instant::now();
        let mut model_calls = 0;
        // Consecutive spent rounds without a new confirmed fact. The baseline
        // is the task's current confirmed count, so a reopened task does not
        // count facts from its previous epoch as new progress.
        let mut no_progress_rounds = 0;
        let mut last_confirmed = confirmed_fact_count(&self.store.list_facts(task_id).await?);
        // Consecutive idle planner ticks. Reset to zero on every real
        // intent, so only a genuinely stuck task ever exhausts this budget.
        let mut idle_ticks = 0;
        loop {
            // Re-read the ledger every round. An extension made while this
            // loop is live (for example during a long team run) applies at
            // the next boundary instead of waiting for a new engine run.
            if let Some(fresh) = self.store.get_task(task_id).await? {
                task = fresh;
            }
            let capacity = task.capacity;
            let round_limit = capacity.max_rounds.unwrap_or(self.max_rounds);
            let model_call_limit = capacity.max_model_calls;
            let wall_limit = capacity.max_wall_secs;

            if stop_requested(stop.as_ref()) {
                return self
                    .stopped_result(
                        task_id,
                        task,
                        rounds,
                        "stop requested before the next round",
                    )
                    .await;
            }
            if let Some(reason) =
                Self::capacity_pause_reason(wall_limit, model_call_limit, started, model_calls)
            {
                return self
                    .pause_for_capacity(
                        task_id,
                        task,
                        capacity,
                        reason,
                        rounds,
                        model_calls,
                        started,
                    )
                    .await;
            }
            let open = self.store.list_open_intents(task_id, 8).await?;
            if open.is_empty() {
                // Nothing to claim is idle time. Expire any overdue approval
                // before asking the planner, so a decision that never comes
                // cannot keep an intent paused past its timer.
                self.sweep_approvals(task_id).await?;
                let facts: Vec<Fact> = self.store.list_facts(task_id).await?;
                let goals: Vec<Goal> = self
                    .store
                    .list_goals(task_id)
                    .await?
                    .into_iter()
                    .filter(|goal| goal.state == GoalState::Open)
                    .collect();
                // Both a team step failure and a single-worker call failure
                // return to the planner as a visible recent failure. Merge
                // them chronologically and keep the newest lines so the
                // prompt stays bounded.
                let mut failed = self
                    .store
                    .list_activity(task_id, Some("team_failed"))
                    .await?;
                failed.extend(
                    self.store
                        .list_activity(task_id, Some("intent_failed"))
                        .await?,
                );
                failed.sort_by_key(|activity| activity.id);
                let mut failures: Vec<String> = failed
                    .into_iter()
                    .map(|activity| activity.message)
                    .collect();
                if failures.len() > 8 {
                    failures = failures.split_off(failures.len() - 8);
                }
                // Before an expensive step: a planner call is a model call.
                if let Some(reason) =
                    Self::capacity_pause_reason(wall_limit, model_call_limit, started, model_calls)
                {
                    return self
                        .pause_for_capacity(
                            task_id,
                            task,
                            capacity,
                            reason,
                            rounds,
                            model_calls,
                            started,
                        )
                        .await;
                }
                // Pending human instructions are planner-only input. They are
                // re-read every round; delivered rows never come back.
                let pending_instructions: Vec<dhi_core::TaskInstruction> = self
                    .store
                    .list_instructions(task_id)
                    .await?
                    .into_iter()
                    .filter(|instruction| instruction.delivered_at.is_none())
                    .collect();
                let instruction_texts: Vec<String> = pending_instructions
                    .iter()
                    .map(|instruction| instruction.text.clone())
                    .collect();
                let pending_instruction_ids: Vec<Uuid> = pending_instructions
                    .iter()
                    .map(|instruction| instruction.id)
                    .collect();
                let plan = match self
                    .agent
                    .plan(PlanInput {
                        task_id,
                        goal: task.goal.clone(),
                        goals,
                        facts: facts.clone(),
                        open_intents: 0,
                        failures,
                        planner_profile_id: task.pinned_planner_profile_id,
                        required_topology: task.topology,
                        swarm_workers: task.swarm_workers,
                        instructions: instruction_texts,
                        project_instructions: project_instructions.clone(),
                    })
                    .await
                {
                    Ok(call) => call,
                    Err(err) => {
                        if stop_requested(stop.as_ref()) {
                            return self
                                .stopped_result(
                                    task_id,
                                    task,
                                    rounds,
                                    "stop requested during the planner call",
                                )
                                .await;
                        }
                        self.fail_task(
                            task_id,
                            &mut task,
                            format!("planner call failed: {err}"),
                            "planner",
                        )
                        .await?;
                        return Err(err);
                    }
                };
                // Usage is recorded before the output is read; a failed save
                // warns and the plan still applies. Only calls that produced
                // `AgentCallMeta` are counted: a runtime call that fails before
                // reporting its metadata is invisible to the ledger.
                let planner_calls = plan.meta.calls.len();
                self.record_agent_calls(task_id, None, None, &plan).await;
                model_calls += planner_calls;
                let plan = plan.output;

                // The planner round consumed every pending instruction it was
                // given, no matter whether it produced a plan. Marking only
                // those ids after the call keeps a restart from re-injecting
                // old text while leaving an instruction added mid-call pending
                // for the next round.
                if !pending_instructions.is_empty() {
                    let delivered = self
                        .store
                        .mark_instructions_delivered_ids(task_id, &pending_instruction_ids)
                        .await?;
                    self.log_activity(
                        task_id,
                        "instruction_delivered",
                        format!("planner received {delivered} instruction(s)"),
                        serde_json::json!({
                            "count": delivered,
                            "instruction_ids": pending_instruction_ids,
                            "round": rounds,
                        }),
                    )
                    .await?;
                }

                if stop_requested(stop.as_ref()) {
                    return self
                        .stopped_result(
                            task_id,
                            task,
                            rounds,
                            "stop requested before applying the planner result",
                        )
                        .await;
                }

                match plan {
                    PlanOutcome::NoPlan { .. } => {
                        // No work and no wake pending: hibernate until the
                        // notify fires or the heartbeat ticks, sweep again, and
                        // re-check. Idle waiting is not work, so it never spends
                        // a round; it spends the separate idle budget instead.
                        idle_ticks += 1;
                        if idle_ticks >= self.max_idle_ticks {
                            // A terminal state always lands here: the task was
                            // not terminal before it went idle.
                            self.set_task_state_guarded(task_id, TaskState::Timeout)
                                .await?;
                            task.state = TaskState::Timeout;
                            self.log_activity(
                                task_id,
                                "engine_timeout",
                                format!(
                                    "engine idle for {idle_ticks} ticks (limit {})",
                                    self.max_idle_ticks
                                ),
                                serde_json::json!({
                                    "idle_ticks": idle_ticks,
                                    "max_idle_ticks": self.max_idle_ticks,
                                    "round": rounds,
                                }),
                            )
                            .await?;
                            return Ok(EngineResult {
                                task,
                                rounds,
                                facts: facts.len(),
                                intents: self.store.list_open_intents(task_id, 1000).await?.len(),
                            });
                        }
                        if stop_requested(stop.as_ref()) {
                            return self
                                .stopped_result(
                                    task_id,
                                    task,
                                    rounds,
                                    "stop requested before the idle wait",
                                )
                                .await;
                        }
                        tokio::select! {
                            _ = self.notify.notified() => {}
                            _ = tokio::time::sleep(self.heartbeat) => {}
                        }
                        // The heartbeat is the timer that expires approvals; a
                        // fresh sweep keeps a waiting intent moving even when the
                        // planner has nothing to add.
                        self.sweep_approvals(task_id).await?;
                        continue;
                    }
                    PlanOutcome::GiveUp { goal_id, reason } => {
                        self.log_activity(
                            task_id,
                            "goal_abandoned",
                            format!("planner abandoned goal {goal_id}: {reason}"),
                            serde_json::json!({"goal_id": goal_id, "reason": reason}),
                        )
                        .await?;
                        let message = format!("goal {goal_id} abandoned by planner: {reason}");
                        self.fail_task(task_id, &mut task, message.clone(), "give_up")
                            .await?;
                        anyhow::bail!("{message}");
                    }
                    PlanOutcome::AddIntent { description, plan } => {
                        let plan = match enforce_required_topology(task.topology, plan) {
                            Ok(plan) => plan,
                            Err(reason) => {
                                self.fail_task(task_id, &mut task, reason.clone(), "topology")
                                    .await?;
                                anyhow::bail!("{reason}");
                            }
                        };
                        if rounds >= round_limit {
                            if capacity.max_rounds.is_some() {
                                return self
                                    .pause_for_capacity(
                                        task_id,
                                        task,
                                        capacity,
                                        "rounds",
                                        rounds,
                                        model_calls,
                                        started,
                                    )
                                    .await;
                            }
                            let reason =
                                format!("engine exceeded max rounds ({})", self.max_rounds);
                            self.fail_task(task_id, &mut task, reason.clone(), "engine")
                                .await?;
                            anyhow::bail!("{reason}");
                        }
                        let intent = self
                            .store
                            .add_intent(task_id, task.origin_fact_id, description)
                            .await?;
                        let has_plan = plan.is_some();
                        if let Some(plan) = plan {
                            // A planned intent is durable before execution: a
                            // restarted engine loads the plan from the store.
                            self.store.save_intent_plan(intent.id, &plan).await?;
                            // The map stays a cache for the current process.
                            self.remember_plan(intent.id, plan);
                        }
                        // Only real work spends budget: a new intent is work.
                        rounds += 1;
                        idle_ticks = 0;
                        self.log_activity(
                            task_id,
                            "intents_added",
                            "planner added one intent".to_string(),
                            serde_json::json!({
                                "round": rounds,
                                "intent_id": intent.id,
                                "has_plan": has_plan,
                            }),
                        )
                        .await?;
                        // Adding an intent is a spent round, but only a new
                        // confirmed fact counts as progress. A worker fact
                        // from the previous round resets the ledger here.
                        let confirmed_now = confirmed_fact_count(&facts);
                        if self.note_no_progress(
                            confirmed_now,
                            &mut last_confirmed,
                            &mut no_progress_rounds,
                        ) {
                            let reason = self
                                .fail_no_progress(task_id, &mut task, no_progress_rounds, rounds)
                                .await?;
                            anyhow::bail!("{reason}");
                        }
                        // The new work may already be claimable: nudge
                        // ourselves.
                        self.wake();
                        continue;
                    }
                    PlanOutcome::ProveGoal {
                        goal_id,
                        fact_id,
                        reason,
                    } => {
                        if rounds >= round_limit {
                            if capacity.max_rounds.is_some() {
                                return self
                                    .pause_for_capacity(
                                        task_id,
                                        task,
                                        capacity,
                                        "rounds",
                                        rounds,
                                        model_calls,
                                        started,
                                    )
                                    .await;
                            }
                            let reason =
                                format!("engine exceeded max rounds ({})", self.max_rounds);
                            self.fail_task(task_id, &mut task, reason.clone(), "engine")
                                .await?;
                            anyhow::bail!("{reason}");
                        }

                        // The store rejects a missing or unconfirmed fact, a
                        // goal from another task, and an abandoned goal. A
                        // rejected proof never marks anything met, and it
                        // must not leave the task running with no loop: the
                        // rejection is a visible terminal failure.
                        let all_met = match self
                            .store
                            .prove_goal(task_id, goal_id, fact_id, reason.clone())
                            .await
                        {
                            Ok(all_met) => all_met,
                            Err(err) if err.to_string().starts_with("proof rejected:") => {
                                let reason = err.to_string();
                                self.log_activity(
                                    task_id,
                                    "proof_rejected",
                                    reason.clone(),
                                    serde_json::json!({
                                        "goal_id": goal_id,
                                        "fact_id": fact_id,
                                        "reason": reason,
                                    }),
                                )
                                .await?;
                                self.fail_task(task_id, &mut task, reason, "proof").await?;
                                return Ok(EngineResult {
                                    task,
                                    rounds,
                                    facts: facts.len(),
                                    intents: self
                                        .store
                                        .list_open_intents(task_id, 1000)
                                        .await?
                                        .len(),
                                });
                            }
                            Err(err) => return Err(err),
                        };
                        rounds += 1;
                        idle_ticks = 0;
                        // A successful proof moves a goal to met even though
                        // it adds no fact: it is real progress, so it resets
                        // the no-progress ledger.
                        no_progress_rounds = 0;
                        last_confirmed = confirmed_fact_count(&facts);

                        if all_met {
                            self.store
                                .set_task_state_guarded(task_id, TaskState::Done)
                                .await?;
                            task.state = TaskState::Done;
                            self.log_activity(
                                task_id,
                                "task_done",
                                format!("every goal is met; {reason}"),
                                serde_json::json!({
                                    "round": rounds,
                                    "goal_id": goal_id,
                                    "fact_id": fact_id,
                                }),
                            )
                            .await?;
                            return Ok(EngineResult {
                                task,
                                rounds,
                                facts: facts.len(),
                                intents: self.store.list_open_intents(task_id, 1000).await?.len(),
                            });
                        }
                        continue;
                    }
                }
            }

            if rounds >= round_limit {
                if capacity.max_rounds.is_some() {
                    return self
                        .pause_for_capacity(
                            task_id,
                            task,
                            capacity,
                            "rounds",
                            rounds,
                            model_calls,
                            started,
                        )
                        .await;
                }
                let reason = format!("engine exceeded max rounds ({})", self.max_rounds);
                self.fail_task(task_id, &mut task, reason.clone(), "engine")
                    .await?;
                anyhow::bail!("{reason}");
            }

            let intent = open[0].clone();
            let Some(claimed) = self.store.claim_intent(intent.id, "worker-1").await? else {
                // Another worker won the race for this intent. Back off
                // briefly instead of busy-spinning on the same row.
                tokio::time::sleep(CLAIM_BACKOFF).await;
                continue;
            };
            // A claimed-and-executed intent is real work, so it spends budget.
            rounds += 1;
            idle_ticks = 0;

            self.log_activity(
                task_id,
                "intent_claimed",
                format!("worker-1 claimed: {}", claimed.description),
                serde_json::json!({"intent_id": claimed.id}),
            )
            .await?;

            let facts = self.store.list_facts(task_id).await?;

            // The cache carries plans from this process; a restarted engine
            // loads the durable row instead. A missing plan keeps the
            // single-worker fallback for intents the planner added without one.
            let plan = match self.plan_for(claimed.id) {
                Some(plan) => Some(plan),
                None => self.store.get_intent_plan(claimed.id).await?,
            };

            if let Some(plan) = plan.filter(|plan| plan.topology != Topology::Single) {
                // A required count is enforced once more before any worker
                // starts: a persisted or runtime-supplied plan that bypassed
                // the planner tool guard still must match the user's shape.
                if let Err(reason) = enforce_swarm_worker_count(task.swarm_workers, &plan) {
                    self.store
                        .finish_intent(claimed.id, IntentState::Stopped)
                        .await?;
                    self.forget_plan(claimed.id);
                    self.fail_task(task_id, &mut task, reason.clone(), "swarm_workers")
                        .await?;
                    anyhow::bail!("{reason}");
                }
                let plan_model_calls = Arc::new(AtomicUsize::new(0));
                let (result, team_plan, allow_empty_result) = match self
                    .run_team_plan(
                        task_id,
                        &task,
                        &claimed,
                        &plan,
                        &facts,
                        &plan_model_calls,
                        stop.as_ref(),
                    )
                    .await
                {
                    Ok(outcome) => {
                        model_calls += plan_model_calls.load(Ordering::SeqCst);
                        outcome
                    }
                    Err(err) => {
                        // Count the coordinator/team calls before any stop or
                        // failure path; those model calls were spent.
                        model_calls += plan_model_calls.load(Ordering::SeqCst);
                        if stop_requested(stop.as_ref()) {
                            return self
                                .stopped_result(
                                    task_id,
                                    task,
                                    rounds,
                                    "stop requested during the team run",
                                )
                                .await;
                        }
                        // A lead/team Stop is not a failure: the intent ends
                        // without a fact and the planner chooses the next step.
                        if let Some(stopped) = err.downcast_ref::<dhi_orchestrator::LeadStopped>() {
                            model_calls += stopped
                                .worker_results
                                .iter()
                                .map(|worker| worker.call_meta.calls.len())
                                .sum::<usize>();
                            self.stop_intent_for_team(
                                task_id,
                                claimed.id,
                                &stopped.reason,
                                &stopped.worker_results,
                            )
                            .await?;
                            continue;
                        }
                        if let Some(stopped) = err.downcast_ref::<dhi_orchestrator::TeamStopped>() {
                            model_calls += stopped
                                .worker_results
                                .iter()
                                .map(|worker| worker.call_meta.calls.len())
                                .sum::<usize>();
                            self.stop_intent_for_team(
                                task_id,
                                claimed.id,
                                &stopped.reason,
                                &stopped.worker_results,
                            )
                            .await?;
                            continue;
                        }
                        let reason = format!("worker/engine: {err}");
                        self.store
                            .finish_intent(claimed.id, IntentState::Stopped)
                            .await?;
                        self.forget_plan(claimed.id);
                        self.fail_task(task_id, &mut task, reason.clone(), "worker")
                            .await?;
                        anyhow::bail!("{reason}");
                    }
                };

                // Each worker's runtime call is its own agent run. The
                // runner-owned aggregate fact is recorded below, so usage is
                // saved first. A worker with no completion record (a mock, or
                // a failure before the call was made) is skipped.
                //
                // Failed calls that produced no `AgentCallMeta` cannot be
                // counted; the ledger only sees calls a runtime reported.
                // Seed turns are the aggregate fact; reaction turns are
                // audit/capacity work. Both are real completion calls, so
                // both count against `max_model_calls` and become
                // `agent_runs` rows.
                let all_results: Vec<&dhi_orchestrator::WorkerResult> = result
                    .worker_results
                    .iter()
                    .chain(result.audit_results.iter())
                    .collect();
                model_calls += self
                    .record_result_calls(task_id, claimed.id, &all_results)
                    .await;

                if stop_requested(stop.as_ref()) {
                    return self
                        .stopped_result(task_id, task, rounds, "stop requested after the team run")
                        .await;
                }

                // DHI+ workers never execute post-hoc tool calls themselves.
                // Every request from every worker passes the same guard as the
                // single-worker path: an `Ask` blocks inside the call, a
                // denial is audited and does not run, and the aggregate still
                // records its fact. An executor error fails the task before
                // the team fact exists.
                for worker in &all_results {
                    if worker.tool_calls.is_empty() {
                        continue;
                    }
                    if let Err(err) = self
                        .run_tool_calls_for_worker(
                            task_id,
                            claimed.id,
                            Some(worker.worker_key.as_str()),
                            &worker.tool_calls,
                        )
                        .await
                    {
                        let reason = format!("worker/engine: {err}");
                        self.store
                            .finish_intent(claimed.id, IntentState::Stopped)
                            .await?;
                        self.forget_plan(claimed.id);
                        self.fail_task(task_id, &mut task, reason.clone(), "worker")
                            .await?;
                        anyhow::bail!("{reason}");
                    }
                }

                let successes = result
                    .worker_results
                    .iter()
                    .filter(|worker| worker.status == WorkerStatus::Ok)
                    .count();
                let failures = result
                    .worker_results
                    .iter()
                    .filter(|worker| worker.status == WorkerStatus::Failed)
                    .count();
                let skipped = result
                    .worker_results
                    .iter()
                    .filter(|worker| worker.status == WorkerStatus::Skipped)
                    .count();

                if successes == 0 && !allow_empty_result {
                    // A worker failure is evidence for the planner, not an
                    // automatic task failure. Record a rejected result and a
                    // `team_failed` row, stop this intent, and return to the
                    // planner. The planner can retry with a different plan or
                    // a larger worker budget, or give up. The host guards
                    // (no-progress, rounds, capacity) still cap the loop.
                    let worker_details: Vec<serde_json::Value> = result
                        .worker_results
                        .iter()
                        .map(|worker| {
                            let role = team_plan
                                .workers
                                .iter()
                                .find(|spec| spec.key == worker.worker_key)
                                .map(|spec| spec.role.clone())
                                .unwrap_or_default();
                            let subtask = team_plan
                                .subtasks
                                .iter()
                                .find(|subtask| subtask.id == worker.subtask_id)
                                .map(|subtask| subtask.description.clone())
                                .unwrap_or_else(|| worker.subtask_id.clone());
                            serde_json::json!({
                                "key": worker.worker_key,
                                "role": role,
                                "status": worker.status.to_string(),
                                "subtask": subtask,
                                "error": worker.error,
                            })
                        })
                        .collect();
                    let detail = result
                        .worker_results
                        .iter()
                        .map(|worker| {
                            let error = worker
                                .error
                                .as_deref()
                                .unwrap_or("worker failed without an error");
                            format!("{} [{}]: {error}", worker.worker_key, worker.subtask_id)
                        })
                        .collect::<Vec<_>>()
                        .join("; ");
                    let reason = format!(
                        "DHI+ {} team produced no successful worker result ({detail})",
                        plan.topology
                    );
                    self.store
                        .record_result(
                            task_id,
                            claimed.id,
                            reason.clone(),
                            dhi_core::ResultStatus::Rejected,
                            serde_json::json!({
                                "topology": plan.topology.as_str(),
                                "workers": result.worker_results.len(),
                                "success": successes,
                                "failed": failures,
                                "skipped": skipped,
                                "worker_details": worker_details.clone(),
                            }),
                        )
                        .await?;
                    self.store
                        .finish_intent(claimed.id, IntentState::Stopped)
                        .await?;
                    self.forget_plan(claimed.id);
                    self.log_activity(
                        task_id,
                        "team_failed",
                        reason.clone(),
                        serde_json::json!({
                            "intent_id": claimed.id,
                            "topology": plan.topology.as_str(),
                            "workers": result.worker_results.len(),
                            "success": successes,
                            "failed": failures,
                            "skipped": skipped,
                            "worker_details": worker_details,
                        }),
                    )
                    .await?;
                    self.wake();
                    continue;
                }

                self.log_activity(
                    task_id,
                    "team_run",
                    format!(
                        "{} team finished: {successes} succeeded, {failures} failed, \
                         {skipped} skipped",
                        plan.topology
                    ),
                    serde_json::json!({
                        "intent_id": claimed.id,
                        "topology": plan.topology.as_str(),
                        "workers": result.worker_results.len(),
                        "worker_keys": result
                            .worker_results
                            .iter()
                            .map(|worker| worker.worker_key.as_str())
                            .collect::<Vec<_>>(),
                        "worker_details": result
                            .worker_results
                            .iter()
                            .map(|worker| {
                                let role = team_plan
                                    .workers
                                    .iter()
                                    .find(|spec| spec.key == worker.worker_key)
                                    .map(|spec| spec.role.clone())
                                    .unwrap_or_default();
                                let subtask = team_plan
                                    .subtasks
                                    .iter()
                                    .find(|subtask| subtask.id == worker.subtask_id)
                                    .map(|subtask| subtask.description.clone())
                                    .unwrap_or_else(|| worker.subtask_id.clone());
                                serde_json::json!({
                                    "key": worker.worker_key,
                                    "role": role,
                                    "status": worker.status.to_string(),
                                    "subtask": subtask,
                                    "error": worker.error,
                                })
                            })
                            .collect::<Vec<_>>(),
                        "max_parallel": team_plan.limits.max_parallel,
                        "success": successes,
                        "failed": failures,
                        "skipped": skipped,
                        "manager_model_calls": plan_model_calls.load(Ordering::SeqCst),
                        "reactions": result.audit_results.len(),
                        "reaction_failures": result
                            .audit_results
                            .iter()
                            .filter(|worker| worker.status == WorkerStatus::Failed)
                            .map(|worker| serde_json::json!({
                                "key": worker.worker_key,
                                "subtask": worker.subtask_id,
                                "error": worker.error,
                            }))
                            .collect::<Vec<_>>(),
                    }),
                )
                .await?;

                let provenance = team_provenance(claimed.id, &team_plan, &result);
                let fact = match self
                    .store
                    .record_fact_for_intent(
                        task_id,
                        claimed.id,
                        result.fact.summary.clone(),
                        provenance,
                    )
                    .await
                {
                    Ok(fact) => fact,
                    Err(err) if is_duplicate_intent_fact(&err) => {
                        let reason = self
                            .fail_duplicate_intent_fact(task_id, &mut task, claimed.id)
                            .await?;
                        anyhow::bail!("{reason}");
                    }
                    Err(err) => return Err(err),
                };
                self.store
                    .finish_intent(claimed.id, IntentState::Done)
                    .await?;
                self.forget_plan(claimed.id);
                self.log_activity(
                    task_id,
                    "intent_done",
                    format!("intent complete: {}", claimed.description),
                    serde_json::json!({
                        "intent_id": claimed.id,
                        "fact_id": fact.id,
                        "topology": plan.topology.as_str(),
                    }),
                )
                .await?;

                let updated_facts = self.store.list_facts(task_id).await?;
                let confirmed_now = confirmed_fact_count(&updated_facts);
                if self.note_no_progress(
                    confirmed_now,
                    &mut last_confirmed,
                    &mut no_progress_rounds,
                ) {
                    let reason = self
                        .fail_no_progress(task_id, &mut task, no_progress_rounds, rounds)
                        .await?;
                    anyhow::bail!("{reason}");
                }
                self.wake();
                continue;
            }

            let executed = match self
                .agent
                .execute(ExecuteInput {
                    task_id,
                    // The claimed intent: every in-loop guarded call the
                    // runtime stamps is recorded under exactly this intent.
                    intent_id: claimed.id,
                    intent: claimed.description.clone(),
                    context: facts.iter().map(|f| f.claim.clone()).collect(),
                    worker_profile_id: task.pinned_worker_profile_id,
                    max_turns: None,
                    agent_id: None,
                    role: None,
                    task: None,
                })
                .await
            {
                Ok(call) => call,
                Err(err) => {
                    if stop_requested(stop.as_ref()) {
                        return self
                            .stopped_result(
                                task_id,
                                task,
                                rounds,
                                "stop requested during the worker call",
                            )
                            .await;
                    }
                    // A worker call failure is evidence for the planner, not
                    // an automatic task failure. Exhaust this intent and
                    // return to the planner, which can retry with a different
                    // plan or give up. The host guards (no-progress, rounds,
                    // capacity) still cap the loop.
                    let error = err.to_string();
                    self.store
                        .finish_intent(claimed.id, IntentState::Exhausted)
                        .await?;
                    self.forget_plan(claimed.id);
                    self.log_activity(
                        task_id,
                        "intent_failed",
                        format!("worker call failed: {error}"),
                        serde_json::json!({"intent_id": claimed.id, "error": error}),
                    )
                    .await?;
                    continue;
                }
            };
            // The row names the intent the worker was claimed for. A failed
            // save warns and the worker's fact is still written. A failed call
            // that returned no `AgentCallMeta` is not counted at all.
            model_calls += executed.meta.calls.len();
            self.record_agent_calls(task_id, Some(claimed.id), Some("worker-1"), &executed)
                .await;
            let output = executed.output;

            if stop_requested(stop.as_ref()) {
                return self
                    .stopped_result(
                        task_id,
                        task,
                        rounds,
                        "stop requested after the worker call",
                    )
                    .await;
            }

            // Guarded tool calls: every request passes the single choke point.
            // An `Ask` blocks inside the call until a decision or expiry; a
            // denial is logged and the round continues normally.
            if !output.tool_calls.is_empty()
                && let Err(err) = self
                    .run_tool_calls(task_id, claimed.id, &output.tool_calls)
                    .await
            {
                self.forget_plan(claimed.id);
                return Err(err);
            }

            // Worker completion is this fact draft. The summary becomes the
            // claim; detail, evidence, and confidence travel in provenance. It
            // never carries acceptance and never sets the task done; only a
            // planner proof does that.
            let provenance = serde_json::json!({
                "intent_id": claimed.id,
                "detail": output.fact.detail.clone(),
                "evidence": output.fact.evidence.clone(),
                "confidence": output.fact.confidence.clone(),
            });
            let fact = match self
                .store
                .record_fact_for_intent(
                    task_id,
                    claimed.id,
                    output.fact.summary.clone(),
                    provenance,
                )
                .await
            {
                Ok(fact) => fact,
                Err(err) if is_duplicate_intent_fact(&err) => {
                    let reason = self
                        .fail_duplicate_intent_fact(task_id, &mut task, claimed.id)
                        .await?;
                    anyhow::bail!("{reason}");
                }
                Err(err) => return Err(err),
            };

            self.store
                .finish_intent(claimed.id, IntentState::Done)
                .await?;
            self.forget_plan(claimed.id);
            self.log_activity(
                task_id,
                "intent_done",
                format!("intent complete: {}", claimed.description),
                serde_json::json!({"intent_id": claimed.id, "fact_id": fact.id}),
            )
            .await?;
            // A worker round that confirms a fact is progress. Re-read the
            // count rather than assuming the stored state: a store may accept
            // a draft fact without confirming it.
            let updated_facts = self.store.list_facts(task_id).await?;
            let confirmed_now = confirmed_fact_count(&updated_facts);
            if self.note_no_progress(confirmed_now, &mut last_confirmed, &mut no_progress_rounds) {
                let reason = self
                    .fail_no_progress(task_id, &mut task, no_progress_rounds, rounds)
                    .await?;
                anyhow::bail!("{reason}");
            }
            self.wake();
        }
    }

    /// Runs every requested tool call through the guarded executor.
    ///
    /// An `Ask` blocks here until the approval is decided or expires; the
    /// same worker then either runs the tool or receives a denial. A denial
    /// is logged and the round continues, so the worker's fact is still
    /// recorded. `Pending` is not a pause: a custom executor that returns it
    /// is a bug and fails the round.
    async fn run_tool_calls(
        &self,
        task_id: Uuid,
        intent_id: Uuid,
        requests: &[ToolCallRequest],
    ) -> anyhow::Result<()> {
        self.run_tool_calls_for_worker(task_id, intent_id, None, requests)
            .await
    }

    /// The one guarded path for tool calls, with the DHI+ subworker stamped
    /// on the engine's activity rows when there is one.
    ///
    /// The guard itself is intent-scoped: `ToolExecutor::execute` only takes
    /// a task and an intent, so the worker key enriches the audit context and
    /// never changes the verdict or the backend call.
    async fn run_tool_calls_for_worker(
        &self,
        task_id: Uuid,
        intent_id: Uuid,
        worker_key: Option<&str>,
        requests: &[ToolCallRequest],
    ) -> anyhow::Result<()> {
        let executor = self.executor.as_ref().ok_or_else(|| {
            anyhow::anyhow!("intent {intent_id} requested tool calls but no executor is configured")
        })?;

        for request in requests {
            let tool = request.tool.clone();
            match executor
                .execute(task_id, intent_id, request.clone())
                .await?
            {
                ToolCallOutcome::Allowed { output } => {
                    let message = match worker_key {
                        Some(worker_key) => {
                            format!("worker {worker_key} tool {tool} executed")
                        }
                        None => format!("tool {tool} executed"),
                    };
                    let mut payload = serde_json::json!({
                        "intent_id": intent_id,
                        "tool": tool,
                        "input": activity_preview(&request.input),
                        "output": output,
                    });
                    if let Some(worker_key) = worker_key {
                        payload["worker_key"] = serde_json::json!(worker_key);
                    }
                    self.log_activity(task_id, "tool_allowed", message, payload)
                        .await?;
                }
                ToolCallOutcome::Denied { reason } => {
                    // The worker continues in the same task loop. The denial
                    // is recorded, but the intent is not blocked and the task
                    // does not stop.
                    let message = match worker_key {
                        Some(worker_key) => {
                            format!("worker {worker_key} tool {tool} denied: {reason}")
                        }
                        None => format!("tool {tool} denied: {reason}"),
                    };
                    let mut payload = serde_json::json!({
                        "intent_id": intent_id,
                        "tool": tool,
                        "reason": reason,
                    });
                    if let Some(worker_key) = worker_key {
                        payload["worker_key"] = serde_json::json!(worker_key);
                    }
                    self.log_activity(task_id, "tool_denied", message, payload)
                        .await?;
                }
                ToolCallOutcome::Pending { approval_id } => {
                    return Err(anyhow::anyhow!(
                        "executor returned Pending for approval {approval_id}; \
                         approvals must block inside the tool call"
                    ));
                }
            }
        }
        Ok(())
    }

    /// Resolves a pending approval and wakes the worker waiting on it.
    ///
    /// The worker continues inside its own `run_task` loop; this never
    /// executes the tool, never finishes an intent, and never pauses a task.
    /// Returns `None` when the approval does not exist or is no longer
    /// pending.
    pub async fn decide_approval(
        &self,
        approval_id: Uuid,
        decision: ApprovalDecision,
    ) -> anyhow::Result<Option<dhi_core::Approval>> {
        let executor = self.executor.as_ref().ok_or_else(|| {
            anyhow::anyhow!("no executor configured; cannot decide approval {approval_id}")
        })?;
        executor.decide(approval_id, decision).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::team_intent;
    use dhi_agent::{AgentCallRecord, MockAgentRuntime, PlanOutcome, Role};
    use dhi_core::team::TeamStore;
    use dhi_core::{
        Activity, AgentRole, AgentsMdState, Approval, ApprovalStatus, FactId, FactState, Goal,
        GoalId, GoalState, GuardVerdict, Intent, IntentId, NewSideQuestionRequest, ReopenOutcome,
        ResultRecord, ResultStatus, SideQuestionRequest, SideQuestionSession, SideQuestionThread,
        SwarmMessageRecord, TaskId,
    };
    use dhi_guard::Policy;
    use dhi_guard::executor::{GuardedExecutor, ToolBackend, ToolExecutor};
    use dhi_orchestrator::{CapabilityLimits, Subtask, TeamSelector, WorkerSpec};
    use dhi_testkit::MemoryStore;
    use serde_json::json;
    use std::sync::Mutex;
    use tokio::time::Instant;

    // ---------- test doubles ----------

    /// Scripted planner/worker. Pops the next scripted response in order.
    /// `plan` falls through to `NoPlan` when its script runs dry so an idle
    /// engine still terminates; `execute` errors so a mis-scoped test fails
    /// loudly instead of recording phantom facts.
    /// One scripted planner step. `ProveFirstConfirmed` is a test-only way to
    /// script a proof after the worker without knowing the fact id up front;
    /// it still produces a real `PlanOutcome::ProveGoal` naming that fact.
    enum ScriptedPlan {
        Outcome(PlanOutcome),
        ProveFirstConfirmed {
            goal_id: Uuid,
        },
        /// A planner call that fails, to test the visible-failure path.
        Fail(String),
    }

    struct ScriptedAgent {
        plans: Mutex<Vec<ScriptedPlan>>,
        executes: Mutex<Vec<dhi_agent::ExecuteOutput>>,
        plan_inputs: Mutex<Vec<PlanInput>>,
    }

    impl ScriptedAgent {
        /// Scripts are listed in call order: first element answers the first
        /// planner/worker call.
        fn new(plans: Vec<PlanOutcome>, executes: Vec<dhi_agent::ExecuteOutput>) -> Self {
            Self::with_plans(
                plans.into_iter().map(ScriptedPlan::Outcome).collect(),
                executes,
            )
        }

        /// A completion test scripts an explicit proof after the worker; it
        /// never scripts a boolean.
        fn with_plans(plans: Vec<ScriptedPlan>, executes: Vec<dhi_agent::ExecuteOutput>) -> Self {
            let mut plans = plans;
            let mut executes = executes;
            plans.reverse();
            executes.reverse();
            Self {
                plans: Mutex::new(plans),
                executes: Mutex::new(executes),
                plan_inputs: Mutex::new(Vec::new()),
            }
        }

        fn plan_inputs(&self) -> Vec<PlanInput> {
            self.plan_inputs.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl AgentRuntime for ScriptedAgent {
        async fn plan(
            &self,
            input: PlanInput,
        ) -> anyhow::Result<dhi_agent::AgentCall<PlanOutcome>> {
            self.plan_inputs.lock().unwrap().push(input.clone());
            let output = match self.plans.lock().unwrap().pop() {
                Some(ScriptedPlan::Outcome(plan)) => plan,
                Some(ScriptedPlan::ProveFirstConfirmed { goal_id }) => {
                    let goal_created_at = input
                        .goals
                        .iter()
                        .find(|goal| goal.id == goal_id)
                        .map(|goal| goal.created_at);
                    let fact = input
                        .facts
                        .iter()
                        .rfind(|fact| {
                            fact.state == FactState::Confirmed
                                && goal_created_at.is_none_or(|goal_at| fact.created_at > goal_at)
                        })
                        .or_else(|| {
                            input
                                .facts
                                .iter()
                                .find(|fact| fact.state == FactState::Confirmed)
                        })
                        .ok_or_else(|| anyhow::anyhow!("no confirmed fact to prove"))?;
                    prove_plan(goal_id, fact.id)
                }
                Some(ScriptedPlan::Fail(message)) => return Err(anyhow::anyhow!(message)),
                None => idle_plan(),
            };
            Ok(unrecorded(output))
        }

        async fn execute(
            &self,
            _input: ExecuteInput,
        ) -> anyhow::Result<dhi_agent::AgentCall<dhi_agent::ExecuteOutput>> {
            Ok(unrecorded(self.executes.lock().unwrap().pop().ok_or_else(
                || anyhow::anyhow!("execute script exhausted"),
            )?))
        }

        fn name(&self) -> &str {
            "scripted"
        }
    }

    /// Scripted planner/worker runtime that also answers manager lead reviews.
    struct LeadAgent {
        planner: ScriptedAgent,
        leads: Mutex<Vec<dhi_core::LeadDecision>>,
        review_inputs: Mutex<Vec<dhi_agent::LeadReviewInput>>,
        record_leads: bool,
    }

    impl LeadAgent {
        fn new(
            plans: Vec<ScriptedPlan>,
            executes: Vec<dhi_agent::ExecuteOutput>,
            leads: Vec<dhi_core::LeadDecision>,
        ) -> Self {
            let mut leads = leads;
            leads.reverse();
            Self {
                planner: ScriptedAgent::with_plans(plans, executes),
                leads: Mutex::new(leads),
                review_inputs: Mutex::new(Vec::new()),
                record_leads: false,
            }
        }

        fn recorded(mut self) -> Self {
            self.record_leads = true;
            self
        }

        fn review_inputs(&self) -> Vec<dhi_agent::LeadReviewInput> {
            self.review_inputs.lock().unwrap().clone()
        }

        fn plan_inputs(&self) -> Vec<PlanInput> {
            self.planner.plan_inputs()
        }
    }

    #[async_trait::async_trait]
    impl AgentRuntime for LeadAgent {
        async fn plan(
            &self,
            input: PlanInput,
        ) -> anyhow::Result<dhi_agent::AgentCall<PlanOutcome>> {
            self.planner.plan(input).await
        }

        async fn execute(
            &self,
            input: ExecuteInput,
        ) -> anyhow::Result<dhi_agent::AgentCall<dhi_agent::ExecuteOutput>> {
            self.planner.execute(input).await
        }

        async fn review_lead(
            &self,
            input: dhi_agent::LeadReviewInput,
        ) -> anyhow::Result<dhi_agent::AgentCall<dhi_core::LeadDecision>> {
            self.review_inputs.lock().unwrap().push(input);
            let decision = self
                .leads
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| anyhow::anyhow!("lead script exhausted"))?;
            if !self.record_leads {
                return Ok(unrecorded(decision));
            }
            let record = call_record(Role::Planner, "lead-m");
            Ok(dhi_agent::AgentCall {
                output: decision,
                meta: dhi_agent::AgentCallMeta {
                    usage: record.usage,
                    calls: vec![record],
                },
                prompt: "manager lead prompt".to_string(),
                response: "manager lead response".to_string(),
                thinking: Some("manager lead reasoning".to_string()),
            })
        }

        fn name(&self) -> &str {
            "lead-scripted"
        }
    }

    /// One coordinator tool script: calls to dispatch, then the fact the
    /// script returns through `record_fact`.
    #[derive(Clone)]
    struct CoordinatorScript {
        calls: Vec<(&'static str, serde_json::Value)>,
        summary: &'static str,
        record: bool,
    }

    /// One teammate tool script.
    #[derive(Clone)]
    struct TeammateScript {
        calls: Vec<(&'static str, serde_json::Value)>,
        summary: &'static str,
        record: bool,
    }

    /// Scripted planner plus coordinator/teammate runs. The coordinator and
    /// teammate paths call the real host tools through the view they receive,
    /// so the tests exercise the tool loop, not a JSON decision.
    struct CoordinatorAgent {
        planner: ScriptedAgent,
        coordinator: Mutex<Option<CoordinatorScript>>,
        teammate: Mutex<Option<TeammateScript>>,
        coordinator_inputs: Mutex<Vec<ExecuteInput>>,
        teammate_inputs: Mutex<Vec<ExecuteInput>>,
    }

    impl CoordinatorAgent {
        fn new(
            plans: Vec<ScriptedPlan>,
            coordinator: CoordinatorScript,
            teammate: TeammateScript,
        ) -> Self {
            Self {
                planner: ScriptedAgent::with_plans(plans, vec![]),
                coordinator: Mutex::new(Some(coordinator)),
                teammate: Mutex::new(Some(teammate)),
                coordinator_inputs: Mutex::new(Vec::new()),
                teammate_inputs: Mutex::new(Vec::new()),
            }
        }

        fn coordinator_inputs(&self) -> Vec<ExecuteInput> {
            self.coordinator_inputs.lock().unwrap().clone()
        }

        fn teammate_inputs(&self) -> Vec<ExecuteInput> {
            self.teammate_inputs.lock().unwrap().clone()
        }
    }

    /// Calls one tool from the registry the runtime gave this run.
    async fn call_team_tool(
        team_tools: &Arc<dyn dhi_agent::ToolServerView>,
        name: &str,
        args: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let handle = team_tools.tool_server();
        let toolset = handle.toolset();
        let tool = toolset
            .get(name)
            .unwrap_or_else(|| panic!("team tool {name} is registered"))
            .clone();
        let mut context = rig::tool::ToolContext::new();
        let result = tool.execute(args.to_string(), &mut context).await;
        if !result.is_success() {
            anyhow::bail!("team tool {name} failed: {result:?}");
        }
        Ok(result.output().as_json().cloned().unwrap_or_default())
    }

    #[async_trait::async_trait]
    impl AgentRuntime for CoordinatorAgent {
        async fn plan(
            &self,
            input: PlanInput,
        ) -> anyhow::Result<dhi_agent::AgentCall<PlanOutcome>> {
            self.planner.plan(input).await
        }

        async fn execute(
            &self,
            _input: ExecuteInput,
        ) -> anyhow::Result<dhi_agent::AgentCall<dhi_agent::ExecuteOutput>> {
            anyhow::bail!("a coordinator test never uses the single-worker execute path")
        }

        async fn execute_coordinator(
            &self,
            input: ExecuteInput,
            team_tools: Arc<dyn dhi_agent::ToolServerView>,
        ) -> anyhow::Result<dhi_agent::AgentCall<dhi_agent::ExecuteOutput>> {
            self.coordinator_inputs.lock().unwrap().push(input.clone());
            let script = self
                .coordinator
                .lock()
                .unwrap()
                .take()
                .ok_or_else(|| anyhow::anyhow!("coordinator script exhausted"))?;
            for (name, args) in &script.calls {
                call_team_tool(&team_tools, name, args.clone()).await?;
            }
            let output = execute_output(script.summary, vec![]);
            if !script.record {
                return Ok(unrecorded(output));
            }
            let record = call_record(Role::Worker, "coordinator-m");
            Ok(dhi_agent::AgentCall {
                output,
                meta: dhi_agent::AgentCallMeta {
                    usage: record.usage,
                    calls: vec![record],
                },
                prompt: input.intent.clone(),
                response: script.summary.to_string(),
                thinking: Some("coordinator reasoning".to_string()),
            })
        }

        async fn execute_teammate(
            &self,
            input: ExecuteInput,
            team_tools: Arc<dyn dhi_agent::ToolServerView>,
        ) -> anyhow::Result<dhi_agent::AgentCall<dhi_agent::ExecuteOutput>> {
            self.teammate_inputs.lock().unwrap().push(input.clone());
            let script = self
                .teammate
                .lock()
                .unwrap()
                .take()
                .ok_or_else(|| anyhow::anyhow!("teammate script exhausted"))?;
            for (name, args) in &script.calls {
                call_team_tool(&team_tools, name, args.clone()).await?;
            }
            let output = execute_output(script.summary, vec![]);
            if !script.record {
                return Ok(unrecorded(output));
            }
            let record = call_record(Role::Worker, "teammate-m");
            Ok(dhi_agent::AgentCall {
                output,
                meta: dhi_agent::AgentCallMeta {
                    usage: record.usage,
                    calls: vec![record],
                },
                prompt: input.task.clone().unwrap_or_else(|| input.intent.clone()),
                response: script.summary.to_string(),
                thinking: Some("teammate reasoning".to_string()),
            })
        }

        fn name(&self) -> &str {
            "coordinator-scripted"
        }
    }

    /// Wraps a scripted planner and adds a new instruction during its first
    /// planner call, simulating a human typing while the model is thinking.
    struct MidPlanInstructionAgent {
        inner: ScriptedAgent,
        store: Arc<MemoryStore>,
        task_id: TaskId,
        added: Mutex<bool>,
    }

    #[async_trait::async_trait]
    impl AgentRuntime for MidPlanInstructionAgent {
        async fn plan(
            &self,
            input: PlanInput,
        ) -> anyhow::Result<dhi_agent::AgentCall<PlanOutcome>> {
            let add = {
                let mut added = self.added.lock().unwrap();
                let add = !*added;
                *added = true;
                add
            };
            if add {
                GraphStore::add_instruction(
                    self.store.as_ref(),
                    self.task_id,
                    "late instruction".to_string(),
                )
                .await?;
            }
            self.inner.plan(input).await
        }

        async fn execute(
            &self,
            input: ExecuteInput,
        ) -> anyhow::Result<dhi_agent::AgentCall<dhi_agent::ExecuteOutput>> {
            self.inner.execute(input).await
        }

        fn name(&self) -> &str {
            "mid-plan-instruction"
        }
    }

    /// What one subtask's worker turn should do.
    #[derive(Clone)]
    enum TeamWorker {
        Success(&'static str),
        /// Success that also reports one completion call, like a real runtime.
        Recorded(&'static str),
        /// Success that returns post-hoc tool calls for the engine to guard.
        Calls(&'static str, Vec<ToolCallRequest>),
        Failure(&'static str),
    }

    /// Scripted planner plus per-subtask worker scripts. Worker responses are
    /// keyed by the subtask description, so the manager and swarm runners can
    /// execute in any order without making the test order-dependent.
    struct TeamAgent {
        planner: ScriptedAgent,
        workers: Mutex<HashMap<String, TeamWorker>>,
        calls: Mutex<Vec<ExecuteInput>>,
    }

    impl TeamAgent {
        fn new(plans: Vec<ScriptedPlan>, workers: Vec<(&str, TeamWorker)>) -> Self {
            Self {
                planner: ScriptedAgent::with_plans(plans, vec![]),
                workers: Mutex::new(
                    workers
                        .into_iter()
                        .map(|(subtask, script)| (subtask.to_string(), script))
                        .collect(),
                ),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<ExecuteInput> {
            self.calls.lock().unwrap().clone()
        }

        fn plan_inputs(&self) -> Vec<PlanInput> {
            self.planner.plan_inputs()
        }
    }

    /// The subtask description a team intent names, so worker scripts can stay
    /// keyed by description.
    fn assigned_subtask(intent: &str) -> &str {
        intent
            .lines()
            .find_map(|line| line.strip_prefix("Assigned subtask: "))
            .unwrap_or(intent)
    }

    #[async_trait::async_trait]
    impl AgentRuntime for TeamAgent {
        async fn plan(
            &self,
            input: PlanInput,
        ) -> anyhow::Result<dhi_agent::AgentCall<PlanOutcome>> {
            self.planner.plan(input).await
        }

        async fn execute(
            &self,
            input: ExecuteInput,
        ) -> anyhow::Result<dhi_agent::AgentCall<dhi_agent::ExecuteOutput>> {
            self.calls.lock().unwrap().push(input.clone());
            let subtask = assigned_subtask(&input.intent);
            let script = self
                .workers
                .lock()
                .unwrap()
                .get(subtask)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unexpected team subtask: {subtask}"))?;
            match script {
                TeamWorker::Success(summary) => Ok(unrecorded(execute_output(summary, vec![]))),
                TeamWorker::Recorded(summary) => {
                    let record = call_record(Role::Worker, "team-worker-m");
                    Ok(dhi_agent::AgentCall {
                        output: execute_output(summary, vec![]),
                        meta: dhi_agent::AgentCallMeta {
                            usage: record.usage,
                            calls: vec![record],
                        },
                        prompt: format!("team prompt for {subtask}"),
                        response: summary.to_string(),
                        thinking: Some("team worker reasoning".to_string()),
                    })
                }
                TeamWorker::Calls(summary, tool_calls) => {
                    Ok(unrecorded(execute_output(summary, tool_calls)))
                }
                TeamWorker::Failure(error) => Err(anyhow::anyhow!(error)),
            }
        }

        fn name(&self) -> &str {
            "team"
        }
    }

    /// Planner plus a worker that records every turn, including swarm
    /// reactions. Used to prove reaction usage reaches capacity and
    /// `agent_runs`.
    struct AllTurnsAgent {
        planner: ScriptedAgent,
        calls: Mutex<usize>,
    }

    impl AllTurnsAgent {
        fn new(plans: Vec<ScriptedPlan>) -> Self {
            Self {
                planner: ScriptedAgent::with_plans(plans, vec![]),
                calls: Mutex::new(0),
            }
        }

        fn calls(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }

    #[async_trait::async_trait]
    impl AgentRuntime for AllTurnsAgent {
        async fn plan(
            &self,
            input: PlanInput,
        ) -> anyhow::Result<dhi_agent::AgentCall<PlanOutcome>> {
            self.planner.plan(input).await
        }

        async fn execute(
            &self,
            input: ExecuteInput,
        ) -> anyhow::Result<dhi_agent::AgentCall<dhi_agent::ExecuteOutput>> {
            let turn = {
                let mut calls = self.calls.lock().unwrap();
                *calls += 1;
                *calls
            };
            let record = call_record(Role::Worker, "team-worker-m");
            Ok(dhi_agent::AgentCall {
                output: execute_output(&format!("swarm finding {turn}"), vec![]),
                meta: dhi_agent::AgentCallMeta {
                    usage: record.usage,
                    calls: vec![record],
                },
                prompt: input.intent.clone(),
                response: format!("swarm finding {turn}"),
                thinking: None,
            })
        }

        fn name(&self) -> &str {
            "all-turns"
        }
    }

    /// Planner/worker that never makes progress: the planner always answers
    /// `NoPlan` and the worker refuses to run.
    struct IdleAgent;

    #[async_trait::async_trait]
    impl AgentRuntime for IdleAgent {
        async fn plan(
            &self,
            _input: PlanInput,
        ) -> anyhow::Result<dhi_agent::AgentCall<PlanOutcome>> {
            Ok(unrecorded(idle_plan()))
        }

        async fn execute(
            &self,
            _input: ExecuteInput,
        ) -> anyhow::Result<dhi_agent::AgentCall<dhi_agent::ExecuteOutput>> {
            anyhow::bail!("idle agent has no work to execute")
        }

        fn name(&self) -> &str {
            "idle"
        }
    }

    /// Planner that flips the engine's stop flag as it returns a scripted
    /// result, so a test can observe the check points deterministically.
    struct StoppingAgent {
        stop: Arc<AtomicBool>,
        plan: PlanOutcome,
    }

    #[async_trait::async_trait]
    impl AgentRuntime for StoppingAgent {
        async fn plan(
            &self,
            _input: PlanInput,
        ) -> anyhow::Result<dhi_agent::AgentCall<PlanOutcome>> {
            self.stop.store(true, Ordering::SeqCst);
            Ok(unrecorded(self.plan.clone()))
        }

        async fn execute(
            &self,
            _input: ExecuteInput,
        ) -> anyhow::Result<dhi_agent::AgentCall<dhi_agent::ExecuteOutput>> {
            anyhow::bail!("the stopping agent has no work to execute")
        }

        fn name(&self) -> &str {
            "stopping"
        }
    }

    /// Fake tool backend: returns a canned JSON value and counts invocations.
    struct FakeBackend {
        calls: std::sync::atomic::AtomicUsize,
        output: serde_json::Value,
    }

    impl FakeBackend {
        fn new(output: serde_json::Value) -> Self {
            Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
                output,
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl ToolBackend for FakeBackend {
        async fn execute(
            &self,
            tool: &str,
            input: &serde_json::Value,
        ) -> anyhow::Result<serde_json::Value> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(json!({"tool": tool, "input": input, "result": self.output}))
        }
    }

    /// A backend that fails after the guard allowed the call, so the engine
    /// sees the executor's error path.
    struct FailingBackend;

    #[async_trait::async_trait]
    impl ToolBackend for FailingBackend {
        async fn execute(
            &self,
            _tool: &str,
            _input: &serde_json::Value,
        ) -> anyhow::Result<serde_json::Value> {
            anyhow::bail!("backend exploded")
        }
    }

    /// Wraps an output in a call that records no usage: the plain scripted
    /// doubles make no real completion call.
    fn unrecorded<T>(output: T) -> dhi_agent::AgentCall<T> {
        dhi_agent::AgentCall {
            output,
            meta: dhi_agent::AgentCallMeta::default(),
            prompt: String::new(),
            response: String::new(),
            thinking: None,
        }
    }

    /// A role's call record with non-zero tokens: planner 400/120, worker
    /// 250/80.
    fn call_record(role: Role, model: &str) -> AgentCallRecord {
        let (input, output, cache_read, reasoning) = match role {
            Role::Planner => (400, 120, 300, 10),
            Role::Worker => (250, 80, 100, 5),
        };
        AgentCallRecord {
            run_id: Uuid::new_v4(),
            call_index: 0,
            role,
            model: model.to_string(),
            profile: format!("open_ai/{model}/max_tokens=4096/stream=true"),
            status: "ok".to_string(),
            error: None,
            latency_ms: 17,
            usage: dhi_agent::Usage {
                input_tokens: input,
                output_tokens: output,
                cache_read_tokens: cache_read,
                cache_write_tokens: 0,
                reasoning_tokens: reasoning,
                cost_usd: None,
            },
            prompt_hash: format!("{}@test", role.as_str()),
        }
    }

    /// An agent whose every call reports one usage record, like a real
    /// runtime. The planner records the goal in its prompt so a test can
    /// check the stored text, and both roles report the fact they saw.
    struct RecordingAgent {
        runs: Mutex<usize>,
    }

    impl RecordingAgent {
        fn new() -> Self {
            Self {
                runs: Mutex::new(0),
            }
        }

        fn next_intent(&self) -> String {
            let mut runs = self.runs.lock().unwrap();
            *runs += 1;
            format!("recorded step {runs}")
        }
    }

    #[async_trait::async_trait]
    impl AgentRuntime for RecordingAgent {
        async fn plan(
            &self,
            input: PlanInput,
        ) -> anyhow::Result<dhi_agent::AgentCall<PlanOutcome>> {
            // One intent in total. Once the worker has recorded a confirmed
            // fact, the next planner call proves the open goal from it. Every
            // test run is therefore two planner calls and one worker call.
            let confirmed = input
                .facts
                .iter()
                .find(|fact| fact.state == FactState::Confirmed);
            let output = match (input.goals.first(), confirmed) {
                (Some(goal), Some(fact)) => PlanOutcome::ProveGoal {
                    goal_id: goal.id,
                    fact_id: fact.id,
                    reason: "recorded proof".to_string(),
                },
                _ => single_intent_plan(&self.next_intent()),
            };
            let record = call_record(Role::Planner, "planner-m");
            Ok(dhi_agent::AgentCall {
                meta: dhi_agent::AgentCallMeta {
                    usage: record.usage,
                    calls: vec![record],
                },
                prompt: format!("plan for {}", input.goal),
                response: "planner tool call".to_string(),
                thinking: Some("recorded planner reasoning".to_string()),
                output,
            })
        }

        async fn execute(
            &self,
            input: ExecuteInput,
        ) -> anyhow::Result<dhi_agent::AgentCall<dhi_agent::ExecuteOutput>> {
            let record = call_record(Role::Worker, "worker-m");
            let fact = format!("recorded fact: {}", input.intent);
            Ok(dhi_agent::AgentCall {
                output: execute_output(&fact, vec![]),
                meta: dhi_agent::AgentCallMeta {
                    usage: record.usage,
                    calls: vec![record],
                },
                prompt: format!("do {}", input.intent),
                response: fact.clone(),
                thinking: Some("recorded worker reasoning".to_string()),
            })
        }

        fn name(&self) -> &str {
            "recording"
        }
    }

    fn single_intent_plan(description: &str) -> PlanOutcome {
        PlanOutcome::AddIntent {
            description: description.to_string(),
            plan: None,
        }
    }

    fn prove_plan(goal_id: Uuid, fact_id: Uuid) -> PlanOutcome {
        PlanOutcome::ProveGoal {
            goal_id,
            fact_id,
            reason: "scripted proof".to_string(),
        }
    }

    fn idle_plan() -> PlanOutcome {
        PlanOutcome::NoPlan {
            reason: "nothing to do yet".to_string(),
        }
    }

    fn team_worker(key: &str, role: &str) -> WorkerSpec {
        WorkerSpec {
            key: key.to_string(),
            role: role.to_string(),
            prompt: format!("act as {role}"),
            max_turns: 1,
        }
    }

    fn team_subtask(id: &str, description: &str, worker_key: &str) -> Subtask {
        Subtask {
            id: id.to_string(),
            description: description.to_string(),
            worker_key: Some(worker_key.to_string()),
        }
    }

    /// A two-worker plan with one explicitly assigned subtask per worker.
    fn two_worker_plan(intent: &str, topology: Topology) -> IntentPlan {
        IntentPlan {
            intent: intent.to_string(),
            topology,
            workers: vec![
                team_worker("alpha", "researcher"),
                team_worker("beta", "critic"),
            ],
            subtasks: vec![
                team_subtask("s1", "subtask one", "alpha"),
                team_subtask("s2", "subtask two", "beta"),
            ],
            limits: CapabilityLimits::default(),
        }
    }

    fn planned_intent(description: &str, plan: IntentPlan) -> PlanOutcome {
        PlanOutcome::AddIntent {
            description: description.to_string(),
            plan: Some(plan),
        }
    }

    /// Completed team intent plus the planner proof of its one fact.
    fn team_completion_plans(
        description: &str,
        plan: IntentPlan,
        goal_id: Uuid,
    ) -> Vec<ScriptedPlan> {
        vec![
            ScriptedPlan::Outcome(planned_intent(description, plan)),
            ScriptedPlan::ProveFirstConfirmed { goal_id },
        ]
    }

    fn fact_draft(summary: &str) -> dhi_agent::FactDraft {
        dhi_agent::FactDraft {
            summary: summary.to_string(),
            detail: String::new(),
            evidence: "test evidence".to_string(),
            confidence: "observed".to_string(),
        }
    }

    fn execute_output(fact: &str, tool_calls: Vec<ToolCallRequest>) -> dhi_agent::ExecuteOutput {
        execute_output_with(fact_draft(fact), tool_calls)
    }

    fn execute_output_with(
        fact: dhi_agent::FactDraft,
        tool_calls: Vec<ToolCallRequest>,
    ) -> dhi_agent::ExecuteOutput {
        dhi_agent::ExecuteOutput { fact, tool_calls }
    }

    /// Seeds the one open goal a scripted proof names.
    async fn seed_goal(store: &MemoryStore, task: &Task) -> Goal {
        store.add_goal(task.id, task.goal.clone()).await.unwrap()
    }

    /// The plan script of a completing run: add one intent, then prove the
    /// named goal from the confirmed fact the worker recorded.
    fn completion_plans(description: &str, goal_id: Uuid) -> Vec<ScriptedPlan> {
        vec![
            ScriptedPlan::Outcome(single_intent_plan(description)),
            ScriptedPlan::ProveFirstConfirmed { goal_id },
        ]
    }

    fn request(tool: &str, input: serde_json::Value) -> ToolCallRequest {
        ToolCallRequest {
            tool: tool.to_string(),
            input,
        }
    }

    /// Policy where the named tool gets `verdict` and everything else falls
    /// through to the deny default.
    fn verdict_policy(tool: &str, verdict: GuardVerdict) -> Policy {
        Policy {
            version: "engine-test-v1".to_string(),
            default: GuardVerdict::Deny,
            rules: vec![dhi_guard::Rule {
                name: format!("rule-{}-{tool}", verdict.as_str()),
                tool: tool.to_string(),
                action: verdict,
                reason: format!("test rule: {} for {tool}", verdict.as_str()),
            }],
        }
    }

    fn guarded_engine(
        store: &Arc<MemoryStore>,
        agent: Arc<dyn AgentRuntime>,
        policy: Policy,
        backend: Arc<dyn ToolBackend>,
    ) -> Engine {
        guarded_engine_with_ttl(store, agent, policy, backend, chrono::Duration::minutes(5))
    }

    fn guarded_engine_with_ttl(
        store: &Arc<MemoryStore>,
        agent: Arc<dyn AgentRuntime>,
        policy: Policy,
        backend: Arc<dyn ToolBackend>,
        approval_ttl: chrono::Duration,
    ) -> Engine {
        let executor = GuardedExecutor::new(store.clone(), policy, backend, approval_ttl);
        Engine::new(store.clone(), agent)
            .with_executor(Arc::new(executor), store.clone())
            .with_heartbeat(Duration::from_millis(20))
    }

    fn fact(claim: &str, state: FactState) -> Fact {
        Fact {
            id: Uuid::new_v4(),
            task_id: Uuid::new_v4(),
            claim: claim.to_string(),
            state,
            provenance: json!({}),
            origin_of: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn team_fact_context_drops_origin_and_keeps_confirmed_facts() {
        let facts = vec![
            fact(
                "origin goal naming every worker and topic",
                FactState::Origin,
            ),
            fact("alpha confirmed its finding", FactState::Confirmed),
            fact("a dismissed finding", FactState::Dismissed),
            fact("beta confirmed its finding", FactState::Confirmed),
        ];

        assert_eq!(
            team_fact_context(&facts),
            vec![
                "alpha confirmed its finding".to_string(),
                "beta confirmed its finding".to_string(),
            ]
        );
    }

    #[test]
    fn team_intent_names_the_assigned_subtask_and_worker_prompt() {
        let worker = team_worker("alpha", "researcher");
        let subtask = team_subtask("s1", "check the northern route", "alpha");

        let intent = team_intent(&worker, &subtask);

        assert_eq!(
            intent,
            "Assigned subtask: check the northern route\n\n\
             Worker role: researcher\n\
             Worker instruction: act as researcher"
        );
    }

    // ---------- existing behavior ----------

    #[tokio::test]
    async fn mock_engine_completes_a_task() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("forced mate");
        let engine = Engine::new(store.clone(), Arc::new(MockAgentRuntime));
        let result = engine.run_task(task.id).await.unwrap();
        assert_eq!(result.task.state, TaskState::Done);
        assert!(!store.facts(task.id).is_empty());
        let intents = store.intents(task.id);
        assert_eq!(intents.len(), 1, "the mock plans one intent");
        let recorded = store
            .fact_for_intent(intents[0].id)
            .expect("the single intent records one fact");
        assert_ne!(recorded.id, task.origin_fact_id);
        // Activity rows are the record of what happened, replacing the old
        // event stream: claims, facts, proofs, and completion all leave a row.
        let activities = store.activities(task.id);
        assert!(
            activities.iter().any(|a| a.kind == "intent_claimed"),
            "a claim must leave an activity row"
        );
        assert!(
            activities.iter().any(|a| a.kind == "intent_done"),
            "finished work must leave an activity row"
        );
    }

    #[tokio::test]
    async fn stop_flag_before_the_first_round_cancels_the_task() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("stop before work");
        let stop = Arc::new(AtomicBool::new(true));
        let engine = Engine::new(store.clone(), Arc::new(MockAgentRuntime));

        let result = engine
            .run_task_with_stop(task.id, Some(stop))
            .await
            .unwrap();

        assert_eq!(result.task.state, TaskState::Cancelled);
        assert_eq!(
            store.get_task(task.id).await.unwrap().unwrap().state,
            TaskState::Cancelled
        );
        let activities = store.activities(task.id);
        assert!(
            activities
                .iter()
                .any(|activity| activity.kind == "task_stopped"),
            "a stop must leave a task_stopped row"
        );
        assert!(
            !activities
                .iter()
                .any(|activity| activity.kind == "task_failed"),
            "a stop is not a failure"
        );
    }

    #[tokio::test]
    async fn stop_during_plan_is_checked_before_the_result_is_applied() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("stop during plan");
        let stop = Arc::new(AtomicBool::new(false));
        let engine = Engine::new(
            store.clone(),
            Arc::new(StoppingAgent {
                stop: stop.clone(),
                plan: single_intent_plan("must never be added"),
            }),
        );

        let result = engine
            .run_task_with_stop(task.id, Some(stop))
            .await
            .unwrap();

        assert_eq!(result.task.state, TaskState::Cancelled);
        assert_eq!(store.intents(task.id).len(), 0, "the plan was not applied");
        assert!(
            store
                .activities(task.id)
                .iter()
                .any(|activity| activity.kind == "task_stopped")
        );
    }

    #[tokio::test]
    async fn engine_fails_a_plan_that_does_not_match_the_required_topology() {
        let store = Arc::new(MemoryStore::new());
        let task = GraphStore::create_task_with_pins(
            store.as_ref(),
            dhi_core::NewTask::new("split the work").with_topology(Some(Topology::Swarm)),
        )
        .await
        .unwrap();
        let agent = Arc::new(ScriptedAgent::new(
            vec![planned_intent(
                "split the work",
                two_worker_plan("split the work", Topology::ManagerWorker),
            )],
            vec![],
        ));
        let engine = Engine::new(store.clone(), agent);

        let err = engine
            .run_task(task.id)
            .await
            .expect_err("a mismatched plan fails the task");
        assert!(err.to_string().contains("required topology swarm"), "{err}");

        let row = store.get_task(task.id).await.unwrap().unwrap();
        assert_eq!(row.state, TaskState::Failed);
        let failed = store
            .activities(task.id)
            .into_iter()
            .find(|activity| activity.kind == "task_failed")
            .expect("task_failed activity");
        assert_eq!(failed.payload["source"], "topology");
        assert!(
            failed.payload["reason"]
                .as_str()
                .unwrap_or_default()
                .contains("manager_worker"),
            "{failed:?}"
        );
        assert!(
            store.intents(task.id).is_empty(),
            "the mismatched plan must not create an intent"
        );
    }

    #[tokio::test]
    async fn engine_ignores_a_plan_when_single_is_required() {
        let store = Arc::new(MemoryStore::new());
        let task = GraphStore::create_task_with_pins(
            store.as_ref(),
            dhi_core::NewTask::new("just do it").with_topology(Some(Topology::Single)),
        )
        .await
        .unwrap();
        let goal = seed_goal(&store, &task).await;
        let agent = Arc::new(ScriptedAgent::with_plans(
            vec![
                ScriptedPlan::Outcome(planned_intent(
                    "do it single",
                    two_worker_plan("do it single", Topology::Swarm),
                )),
                ScriptedPlan::ProveFirstConfirmed { goal_id: goal.id },
            ],
            vec![execute_output("done single", vec![])],
        ));
        let engine = Engine::new(store.clone(), agent);

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Done);
        assert_eq!(result.task.topology, Some(Topology::Single));
        let intents = store.intents(task.id);
        assert_eq!(intents.len(), 1);
        assert_eq!(
            store.get_intent_plan(intents[0].id).await.unwrap(),
            None,
            "single ignores the planner's plan"
        );
    }

    #[tokio::test]
    async fn planner_receives_the_task_topology() {
        let store = Arc::new(MemoryStore::new());
        let task = GraphStore::create_task_with_pins(
            store.as_ref(),
            dhi_core::NewTask::new("swarm task").with_topology(Some(Topology::Swarm)),
        )
        .await
        .unwrap();
        let goal = seed_goal(&store, &task).await;
        let agent = Arc::new(ScriptedAgent::with_plans(
            vec![ScriptedPlan::ProveFirstConfirmed { goal_id: goal.id }],
            vec![],
        ));
        let engine = Engine::new(store.clone(), Arc::clone(&agent) as Arc<dyn AgentRuntime>);

        let _ = engine.run_task(task.id).await;

        assert_eq!(
            agent.plan_inputs()[0].required_topology,
            Some(Topology::Swarm)
        );
    }

    #[tokio::test]
    async fn planner_receives_the_project_agents_md_once() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("AGENTS.md"),
            "PROJECT-MARKER: keep the public API stable",
        )
        .unwrap();
        let store = Arc::new(MemoryStore::new());
        let task = GraphStore::create_task_with_pins(
            store.as_ref(),
            dhi_core::NewTask::new("project task").with_project(
                project.display().to_string(),
                true,
                AgentsMdState::Loaded,
            ),
        )
        .await
        .unwrap();
        let goal = seed_goal(&store, &task).await;
        store
            .record_fact(task.id, "confirmed project evidence".to_string(), json!({}))
            .await
            .unwrap();
        let agent = Arc::new(ScriptedAgent::with_plans(
            vec![
                ScriptedPlan::Outcome(idle_plan()),
                ScriptedPlan::ProveFirstConfirmed { goal_id: goal.id },
            ],
            vec![],
        ));
        let engine = Engine::new(store.clone(), Arc::clone(&agent) as Arc<dyn AgentRuntime>)
            .with_heartbeat(Duration::from_millis(1))
            .with_max_idle_ticks(2);

        let _ = engine.run_task(task.id).await;

        let inputs = agent.plan_inputs();
        assert!(!inputs.is_empty(), "the planner ran");
        assert_eq!(
            inputs[0].project_instructions.as_deref(),
            Some("PROJECT-MARKER: keep the public API stable"),
            "the planner receives the bounded AGENTS.md text"
        );
        assert!(
            inputs[1..]
                .iter()
                .all(|input| input.project_instructions.as_deref()
                    == Some("PROJECT-MARKER: keep the public API stable")),
            "later planner rounds reuse the text read at task start"
        );
        assert_eq!(
            store
                .get_task(task.id)
                .await
                .unwrap()
                .expect("task exists")
                .agents_md_state,
            AgentsMdState::Loaded
        );
    }

    #[tokio::test]
    async fn an_oversized_agents_md_is_truncated_and_marked_in_the_planner_input() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("AGENTS.md"),
            "a".repeat(dhi_core::AGENTS_MD_MAX_BYTES + 64),
        )
        .unwrap();
        let store = Arc::new(MemoryStore::new());
        let task = GraphStore::create_task_with_pins(
            store.as_ref(),
            dhi_core::NewTask::new("project task").with_project(
                project.display().to_string(),
                true,
                AgentsMdState::Loaded,
            ),
        )
        .await
        .unwrap();
        let goal = seed_goal(&store, &task).await;
        store
            .record_fact(
                task.id,
                "confirmed truncated-file evidence".to_string(),
                json!({}),
            )
            .await
            .unwrap();
        let agent = Arc::new(ScriptedAgent::with_plans(
            vec![ScriptedPlan::ProveFirstConfirmed { goal_id: goal.id }],
            vec![],
        ));
        let engine = Engine::new(store.clone(), Arc::clone(&agent) as Arc<dyn AgentRuntime>);

        let _ = engine.run_task(task.id).await;

        let input = agent
            .plan_inputs()
            .into_iter()
            .next()
            .expect("the planner ran");
        let text = input.project_instructions.expect("truncated text is sent");
        let content = text
            .split("\n\n[AGENTS.md")
            .next()
            .expect("the instruction text");
        assert_eq!(content.len(), dhi_core::AGENTS_MD_MAX_BYTES);
        assert!(content.bytes().all(|byte| byte == b'a'));
        assert!(
            text.contains("was longer than 32 KiB"),
            "the prompt marks truncation: {text}"
        );
        assert_eq!(
            store
                .get_task(task.id)
                .await
                .unwrap()
                .expect("task exists")
                .agents_md_state,
            AgentsMdState::Truncated
        );
    }

    #[tokio::test]
    async fn a_missing_agents_md_is_not_found_and_does_not_block_the_plan() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let store = Arc::new(MemoryStore::new());
        let task = GraphStore::create_task_with_pins(
            store.as_ref(),
            dhi_core::NewTask::new("project task").with_project(
                project.display().to_string(),
                true,
                AgentsMdState::Loaded,
            ),
        )
        .await
        .unwrap();
        let goal = seed_goal(&store, &task).await;
        store
            .record_fact(
                task.id,
                "confirmed missing-file evidence".to_string(),
                json!({}),
            )
            .await
            .unwrap();
        let agent = Arc::new(ScriptedAgent::with_plans(
            vec![ScriptedPlan::ProveFirstConfirmed { goal_id: goal.id }],
            vec![],
        ));
        let engine = Engine::new(store.clone(), Arc::clone(&agent) as Arc<dyn AgentRuntime>);

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Done);
        assert_eq!(agent.plan_inputs()[0].project_instructions, None);
        assert_eq!(
            store
                .get_task(task.id)
                .await
                .unwrap()
                .expect("task exists")
                .agents_md_state,
            AgentsMdState::NotFound
        );
    }

    #[tokio::test]
    async fn planner_receives_pending_instructions_and_delivered_ones_are_not_resent() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("apply the human instruction");
        GraphStore::add_instruction(
            store.as_ref(),
            task.id,
            "focus on the endgame first".to_string(),
        )
        .await
        .unwrap();
        let agent = Arc::new(ScriptedAgent::new(vec![idle_plan(), idle_plan()], vec![]));
        let engine = Engine::new(store.clone(), Arc::clone(&agent) as Arc<dyn AgentRuntime>)
            .with_heartbeat(Duration::from_millis(1))
            .with_max_idle_ticks(2);

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Timeout);
        let inputs = agent.plan_inputs();
        assert_eq!(inputs.len(), 2, "two planner rounds ran");
        assert_eq!(
            inputs[0].instructions,
            vec!["focus on the endgame first".to_string()],
            "the first planner round sees the pending instruction"
        );
        assert!(
            inputs[1].instructions.is_empty(),
            "a delivered instruction is never re-sent"
        );
        let stored = store.instructions(task.id);
        assert_eq!(stored.len(), 1);
        assert!(
            stored[0].delivered_at.is_some(),
            "the planner round marked the instruction delivered"
        );
        assert_eq!(
            store
                .activities(task.id)
                .into_iter()
                .filter(|activity| activity.kind == "instruction_delivered")
                .count(),
            1,
            "one planner round appends one delivery activity"
        );
    }

    #[tokio::test]
    async fn instruction_added_during_the_planner_call_waits_for_the_next_round() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("apply both instructions");
        GraphStore::add_instruction(store.as_ref(), task.id, "first".to_string())
            .await
            .unwrap();
        let agent = Arc::new(MidPlanInstructionAgent {
            inner: ScriptedAgent::new(vec![idle_plan(), idle_plan()], vec![]),
            store: store.clone(),
            task_id: task.id,
            added: Mutex::new(false),
        });
        let engine = Engine::new(store.clone(), Arc::clone(&agent) as Arc<dyn AgentRuntime>)
            .with_heartbeat(Duration::from_millis(1))
            .with_max_idle_ticks(2);

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Timeout);
        let inputs = agent.inner.plan_inputs();
        assert_eq!(inputs.len(), 2, "two planner rounds ran");
        assert_eq!(
            inputs[0].instructions,
            vec!["first".to_string()],
            "the first planner round sees the first instruction"
        );
        assert_eq!(
            inputs[1].instructions,
            vec!["late instruction".to_string()],
            "an instruction added mid-call waits for the next round instead of being marked delivered unseen"
        );
        let stored = store.instructions(task.id);
        assert_eq!(stored.len(), 2);
        assert!(
            stored.iter().all(|row| row.delivered_at.is_some()),
            "both instructions were delivered once their planner rounds read them"
        );
    }

    #[tokio::test]
    async fn engine_refuses_a_swarm_plan_with_the_wrong_worker_count() {
        let store = Arc::new(MemoryStore::new());
        let task = GraphStore::create_task_with_pins(
            store.as_ref(),
            dhi_core::NewTask::new("split the research")
                .with_topology(Some(Topology::Swarm))
                .with_swarm_workers(Some(3)),
        )
        .await
        .unwrap();
        let agent = Arc::new(ScriptedAgent::new(
            vec![planned_intent(
                "split the research",
                two_worker_plan("split the research", Topology::Swarm),
            )],
            vec![],
        ));
        let engine = Engine::new(store.clone(), Arc::clone(&agent) as Arc<dyn AgentRuntime>);

        let err = engine
            .run_task(task.id)
            .await
            .expect_err("a mismatched swarm count fails the task");
        assert!(
            err.to_string().contains("required swarm worker count 3"),
            "{err}"
        );

        assert_eq!(
            agent.plan_inputs()[0].swarm_workers,
            Some(3),
            "the planner input carries the user's required count"
        );
        let row = store.get_task(task.id).await.unwrap().unwrap();
        assert_eq!(row.state, TaskState::Failed);
        let failed = store
            .activities(task.id)
            .into_iter()
            .find(|activity| activity.kind == "task_failed")
            .expect("task_failed activity");
        assert_eq!(failed.payload["source"], "swarm_workers");
        assert!(
            failed.payload["reason"]
                .as_str()
                .unwrap_or_default()
                .contains("2 workers and 2 subtasks"),
            "{failed:?}"
        );
        let intents = store.intents(task.id);
        assert_eq!(intents.len(), 1);
        assert_eq!(
            intents[0].state,
            IntentState::Stopped,
            "the refused intent must not stay running"
        );
        assert!(
            !store
                .activities(task.id)
                .iter()
                .any(|activity| activity.kind == "team_run"),
            "no worker may start on a refused swarm"
        );
    }

    #[tokio::test]
    async fn planner_give_up_fails_the_task_with_a_visible_reason() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("impossible task");
        let goal = seed_goal(&store, &task).await;
        let agent = Arc::new(ScriptedAgent::new(
            vec![PlanOutcome::GiveUp {
                goal_id: goal.id,
                reason: "the confirmed evidence rules it out".to_string(),
            }],
            vec![],
        ));
        let engine = Engine::new(store.clone(), agent);
        let err = engine
            .run_task(task.id)
            .await
            .expect_err("give_up ends the task");
        assert!(err.to_string().contains("abandoned"), "{err}");

        let row = store.get_task(task.id).await.unwrap().unwrap();
        assert_eq!(row.state, TaskState::Failed);
        let activities = store.activities(task.id);
        let kinds: Vec<&str> = activities.iter().map(|a| a.kind.as_str()).collect();
        assert!(kinds.contains(&"goal_abandoned"), "{kinds:?}");
        assert!(kinds.contains(&"task_failed"), "{kinds:?}");
        let failed = activities
            .iter()
            .find(|a| a.kind == "task_failed")
            .expect("failure row");
        assert_eq!(failed.payload["source"], "give_up");
        assert!(
            failed.payload["reason"]
                .as_str()
                .unwrap_or_default()
                .contains("rules it out")
        );
    }

    #[tokio::test]
    async fn reopened_task_runs_the_new_goal() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("reopened task");
        // The old fact proved the old goal; the new goal starts from a fresh
        // intent after the human reopens the task.
        store
            .record_fact(
                task.id,
                "the old proof".to_string(),
                serde_json::json!({"epoch": "old"}),
            )
            .await
            .unwrap();
        GraphStore::set_task_state_guarded(store.as_ref(), task.id, TaskState::Running)
            .await
            .unwrap();
        GraphStore::set_task_state_guarded(store.as_ref(), task.id, TaskState::Done)
            .await
            .unwrap();

        let outcome =
            GraphStore::reopen_task_with_goal(store.as_ref(), task.id, "the new goal".to_string())
                .await
                .unwrap();
        let dhi_core::ReopenOutcome::Reopened { goal, .. } = outcome else {
            panic!("the terminal task must reopen, got {outcome:?}");
        };

        let engine = Engine::new(
            store.clone(),
            Arc::new(ScriptedAgent::with_plans(
                vec![
                    ScriptedPlan::Outcome(single_intent_plan("work for the new goal")),
                    ScriptedPlan::ProveFirstConfirmed { goal_id: goal.id },
                ],
                vec![execute_output("the new fact", vec![])],
            )),
        );
        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Done);
        let activities = store.activities(task.id);
        assert!(
            activities.iter().any(|a| a.kind == "intents_added"),
            "the reopened task must add an intent for the new goal"
        );
        assert!(
            !activities.iter().any(|a| a.kind == "task_failed"),
            "the reopened task must not fail"
        );
    }

    #[tokio::test]
    async fn planner_failure_marks_the_task_failed_and_records_it() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("provider failure");
        let agent = Arc::new(ScriptedAgent::with_plans(
            vec![ScriptedPlan::Fail("provider exploded".to_string())],
            vec![],
        ));
        let engine = Engine::new(store.clone(), agent);
        let err = engine
            .run_task(task.id)
            .await
            .expect_err("the planner failure propagates");
        assert!(err.to_string().contains("provider exploded"), "{err}");

        let row = store.get_task(task.id).await.unwrap().unwrap();
        assert_eq!(row.state, TaskState::Failed);
        let failed = store
            .activities(task.id)
            .into_iter()
            .find(|a| a.kind == "task_failed")
            .expect("failure row");
        assert_eq!(failed.payload["source"], "planner");
        assert!(
            failed.payload["reason"]
                .as_str()
                .unwrap_or_default()
                .contains("provider exploded")
        );
    }

    // ---------- agent run usage ----------

    #[tokio::test]
    async fn planner_usage_is_recorded() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("record usage");
        let engine = Engine::new(store.clone(), Arc::new(RecordingAgent::new()));
        engine.run_task(task.id).await.unwrap();

        let runs = store.agent_runs(task.id);
        let planner: Vec<_> = runs
            .iter()
            .filter(|r| r.role == AgentRole::Planner)
            .collect();
        assert_eq!(
            planner.len(),
            2,
            "one planner call to add the intent, one to prove the goal"
        );
        for row in &planner {
            assert_eq!(row.tokens_in, 400);
            assert_eq!(row.tokens_out, 120);
            assert_eq!(row.cache_read_tokens, 300);
            assert_eq!(row.reasoning_tokens, 10);
            assert_eq!(row.model, "planner-m");
            assert_eq!(row.prompt_hash, "planner@test");
            assert_eq!(row.prompt, "plan for record usage");
            assert_eq!(
                row.thinking.as_deref(),
                Some("recorded planner reasoning"),
                "the planner call's reasoning lands on the row"
            );
            assert!(
                row.intent_id.is_none(),
                "the planner is not attached to an intent"
            );
            assert!(
                row.worker_key.is_none(),
                "a planner call has no subworker identity"
            );
            assert_eq!(row.call_index, 0);
            assert_eq!(row.id, row.run_id);
        }
    }

    #[tokio::test]
    async fn worker_usage_is_recorded() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("record usage");
        let engine = Engine::new(store.clone(), Arc::new(RecordingAgent::new()));
        engine.run_task(task.id).await.unwrap();

        let runs = store.agent_runs(task.id);
        let worker: Vec<_> = runs
            .iter()
            .filter(|r| r.role == AgentRole::Worker)
            .collect();
        assert_eq!(worker.len(), 1, "one worker call, one row");
        let row = worker[0];
        assert_eq!(row.tokens_in, 250);
        assert_eq!(row.tokens_out, 80);
        assert_eq!(row.cache_read_tokens, 100);
        assert_eq!(row.model, "worker-m");
        assert!(row.prompt.starts_with("do recorded step 1"));
        assert_eq!(row.thinking.as_deref(), Some("recorded worker reasoning"));
        assert!(
            row.response
                .as_deref()
                .is_some_and(|r| r.starts_with("recorded fact")),
            "{:?}",
            row.response
        );

        let id = row.intent_id.expect("the worker row names its intent");
        let intents = store.intents(task.id);
        assert!(
            intents.iter().any(|i| i.id == id),
            "the row's intent exists on the task"
        );
        assert_eq!(
            row.worker_key.as_deref(),
            Some("worker-1"),
            "a normal single-worker call records its worker identity"
        );
    }

    #[tokio::test]
    async fn agent_run_has_tokens_and_status() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("record usage");
        let engine = Engine::new(store.clone(), Arc::new(RecordingAgent::new()));
        engine.run_task(task.id).await.unwrap();

        let runs = store.agent_runs(task.id);
        assert!(!runs.is_empty());
        for row in &runs {
            assert_eq!(row.status, "ok", "{:?}", row.role);
            assert!(row.error.is_none());
            assert_eq!(row.task_id, task.id);
            assert_eq!(row.variables, serde_json::json!({}));
            assert_eq!(row.cost, 0.0, "River reports no price");
            assert!(row.checkpoint_id.is_none());
            assert!(row.latency_ms >= 0);
            assert!(
                row.tokens_in > 0,
                "{:?} recorded real input tokens",
                row.role
            );
            assert!(
                row.tokens_out > 0,
                "{:?} recorded real output tokens",
                row.role
            );
            assert!(!row.profile.is_empty());
        }
    }

    #[tokio::test]
    async fn mock_agent_records_no_usage() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("forced mate");
        let engine = Engine::new(store.clone(), Arc::new(MockAgentRuntime));
        engine.run_task(task.id).await.unwrap();
        assert!(
            store.agent_runs(task.id).is_empty(),
            "the mock makes no completion call, so there is no agent run"
        );
    }

    #[tokio::test]
    async fn claim_is_atomic() {
        let store = MemoryStore::new();
        let task = store.seed_task("goal");
        let intent = store
            .add_intent(task.id, task.origin_fact_id, "one step".to_string())
            .await
            .unwrap();
        let first = store.claim_intent(intent.id, "worker-1").await.unwrap();
        let second = store.claim_intent(intent.id, "worker-2").await.unwrap();
        assert!(first.is_some());
        assert!(second.is_none());
    }

    // ---------- guarded tool calls ----------

    #[tokio::test]
    async fn allow_tool_call_records_fact() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("open a game");
        let goal = seed_goal(&store, &task).await;
        let backend = Arc::new(FakeBackend::new(json!({"game_id": "g-1"})));
        let engine = guarded_engine(
            &store,
            Arc::new(ScriptedAgent::with_plans(
                completion_plans("create the game", goal.id),
                vec![execute_output(
                    "the game started",
                    vec![request("createGame", json!({"color": "white"}))],
                )],
            )),
            Policy::chess(),
            backend.clone(),
        );

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Done);
        assert_eq!(backend.calls(), 1);
        // Origin fact plus the worker fact recorded after the tool ran.
        let facts = store.facts(task.id);
        assert_eq!(facts.len(), 2);
        assert!(facts.iter().any(|f| f.claim == "the game started"));

        let activities = store.activities(task.id);
        assert!(activities.iter().any(|a| a.kind == "tool_allowed"
            && a.payload["tool"] == "createGame"
            && a.payload["input"] == json!({"color": "white"})
            && a.payload["output"]["result"]["game_id"] == "g-1"));

        let intents = store.intents(task.id);
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].state, IntentState::Done);
    }

    /// A denial no longer stops the round: the worker continues, the intent
    /// finishes normally, and the task does not stall.
    #[tokio::test]
    async fn deny_tool_call_continues_intent() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("wipe the disk");
        let goal = seed_goal(&store, &task).await;
        let backend = Arc::new(FakeBackend::new(json!({})));
        let engine = guarded_engine(
            &store,
            Arc::new(ScriptedAgent::with_plans(
                completion_plans("wipe it", goal.id),
                vec![execute_output(
                    "the denial was handled",
                    vec![request("rmrf", json!({"path": "/"}))],
                )],
            )),
            verdict_policy("rmrf", GuardVerdict::Deny),
            backend.clone(),
        );

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Done);
        assert_eq!(backend.calls(), 0);
        // Origin fact plus the worker fact: the round continued past the denial.
        let facts = store.facts(task.id);
        assert_eq!(facts.len(), 2);
        assert!(facts.iter().any(|f| f.claim == "the denial was handled"));

        let intents = store.intents(task.id);
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].state, IntentState::Done);

        let activities = store.activities(task.id);
        assert!(
            activities
                .iter()
                .any(|a| a.kind == "tool_denied" && a.payload["tool"] == "rmrf")
        );
    }

    /// The Rig runtime runs its guarded tools in-loop and returns no post-hoc
    /// requests. An empty post-hoc list with the executor wired must run
    /// nothing through the guard: no backend calls, no tool activity.
    #[tokio::test]
    async fn empty_post_hoc_tool_calls_run_nothing_through_the_guard() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("only record the fact");
        let goal = seed_goal(&store, &task).await;
        let backend = Arc::new(FakeBackend::new(json!({"should": "never run"})));
        let engine = guarded_engine(
            &store,
            Arc::new(ScriptedAgent::with_plans(
                completion_plans("look it up", goal.id),
                vec![execute_output("nothing to call", vec![])],
            )),
            Policy::chess(),
            backend.clone(),
        );

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Done);
        assert_eq!(backend.calls(), 0, "no post-hoc call reached the backend");
        let activities = store.activities(task.id);
        assert!(
            !activities
                .iter()
                .any(|a| a.kind == "tool_allowed" || a.kind == "tool_denied"),
            "no tool verdict activity without a request: {activities:?}"
        );
        let facts = store.facts(task.id);
        assert!(facts.iter().any(|f| f.claim == "nothing to call"));
    }

    /// A custom executor that still returns `Pending`. The engine must treat
    /// that as a bug, never as a task pause.
    struct PendingExecutor;

    #[async_trait::async_trait]
    impl ToolExecutor for PendingExecutor {
        async fn execute(
            &self,
            _task_id: dhi_core::TaskId,
            _intent_id: dhi_core::IntentId,
            _request: ToolCallRequest,
        ) -> anyhow::Result<ToolCallOutcome> {
            Ok(ToolCallOutcome::Pending {
                approval_id: Uuid::new_v4(),
            })
        }

        async fn decide(
            &self,
            _approval_id: Uuid,
            _decision: ApprovalDecision,
        ) -> anyhow::Result<Option<Approval>> {
            Ok(None)
        }

        async fn sweep_expired(&self, _task_id: dhi_core::TaskId) -> anyhow::Result<Vec<Approval>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn pending_outcome_from_custom_executor_is_an_error() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("never pause on pending");
        let backend = Arc::new(FakeBackend::new(json!({"ok": true})));
        let engine = Engine::new(
            store.clone(),
            Arc::new(ScriptedAgent::new(
                vec![single_intent_plan("call a tool")],
                vec![execute_output(
                    "must not land",
                    vec![request("deploy", json!({}))],
                )],
            )),
        )
        .with_executor(Arc::new(PendingExecutor), store.clone());

        let err = engine.run_task(task.id).await.unwrap_err();

        assert!(
            err.to_string().contains("must block inside the tool call"),
            "a Pending outcome is an executor bug, got {err}"
        );
        assert_eq!(backend.calls(), 0);
        assert_eq!(
            store.facts(task.id).len(),
            1,
            "the round stops before recording the worker fact"
        );
        let intents = store.intents(task.id);
        assert_eq!(intents.len(), 1);
        assert_eq!(
            intents[0].state,
            IntentState::Running,
            "a Pending outcome must not pause the intent"
        );
    }

    /// Waits until the worker has parked its call behind a pending approval.
    async fn wait_for_pending_approval(store: &MemoryStore, task_id: Uuid) -> Approval {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(approval) = store
                .list_pending_approvals(task_id)
                .await
                .unwrap()
                .into_iter()
                .next()
            {
                return approval;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the worker never parked behind an approval"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn spawn_run(
        engine: &Arc<Engine>,
        task_id: Uuid,
    ) -> tokio::task::JoinHandle<anyhow::Result<EngineResult>> {
        let engine = engine.clone();
        tokio::spawn(async move { engine.run_task(task_id).await })
    }

    async fn join_run(
        handle: tokio::task::JoinHandle<anyhow::Result<EngineResult>>,
    ) -> EngineResult {
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("the task loop must continue after the approval resolves")
            .expect("the run task must not panic")
            .expect("the run must not fail")
    }

    /// Allow: the same `run_task` keeps running, the tool executes, the fact
    /// is recorded, and the task reaches `done`.
    #[tokio::test]
    async fn approval_allow_continues_same_run() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("deploy the release");
        let task_id = task.id;
        let goal = seed_goal(&store, &task).await;
        let backend = Arc::new(FakeBackend::new(json!({"deployed": true})));
        let engine = Arc::new(guarded_engine(
            &store,
            Arc::new(ScriptedAgent::with_plans(
                completion_plans("ship it", goal.id),
                vec![execute_output(
                    "the release shipped",
                    vec![request("deploy", json!({"env": "prod"}))],
                )],
            )),
            verdict_policy("deploy", GuardVerdict::Ask),
            backend.clone(),
        ));

        let run = spawn_run(&engine, task_id);
        let approval = wait_for_pending_approval(&store, task_id).await;
        assert_eq!(
            store.get_task(task_id).await.unwrap().unwrap().state,
            TaskState::Running
        );
        assert_eq!(backend.calls(), 0);
        assert_eq!(store.facts(task_id).len(), 1, "only the origin fact");

        let resolved = engine
            .decide_approval(approval.id, ApprovalDecision::Allow)
            .await
            .unwrap()
            .expect("the approval is pending");
        assert_eq!(resolved.status, ApprovalStatus::Granted);

        let result = join_run(run).await;
        assert_eq!(result.task.state, TaskState::Done);
        assert_eq!(backend.calls(), 1);

        let facts = store.facts(task_id);
        assert_eq!(facts.len(), 2);
        assert!(facts.iter().any(|f| f.claim == "the release shipped"));

        let intents = store.intents(task_id);
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].state, IntentState::Done);

        let activities = store.activities(task_id);
        assert!(
            activities
                .iter()
                .any(|a| a.kind == "tool_allowed" && a.payload["tool"] == "deploy")
        );
    }

    /// The task does not stay `running` once the approved worker finishes.
    #[tokio::test]
    async fn task_does_not_stay_running_after_allow() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("finish after approval");
        let task_id = task.id;
        let goal = seed_goal(&store, &task).await;
        let backend = Arc::new(FakeBackend::new(json!({"ok": true})));
        let engine = Arc::new(guarded_engine(
            &store,
            Arc::new(ScriptedAgent::with_plans(
                completion_plans("do it", goal.id),
                vec![execute_output(
                    "approved work finished",
                    vec![request("deploy", json!({}))],
                )],
            )),
            verdict_policy("deploy", GuardVerdict::Ask),
            backend.clone(),
        ));

        let run = spawn_run(&engine, task_id);
        let approval = wait_for_pending_approval(&store, task_id).await;
        engine
            .decide_approval(approval.id, ApprovalDecision::Allow)
            .await
            .unwrap()
            .expect("the approval is pending");
        let result = join_run(run).await;

        assert_eq!(result.task.state, TaskState::Done);
        assert_eq!(
            store.get_task(task_id).await.unwrap().unwrap().state,
            TaskState::Done
        );
        assert_eq!(backend.calls(), 1);
    }

    /// An `Ask` leaves the intent `running` while the worker waits; it never
    /// pauses the intent or the task.
    #[tokio::test]
    async fn no_paused_intent_on_ask() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("wait without pausing");
        let task_id = task.id;
        let goal = seed_goal(&store, &task).await;
        let backend = Arc::new(FakeBackend::new(json!({"ok": true})));
        let engine = Arc::new(guarded_engine(
            &store,
            Arc::new(ScriptedAgent::with_plans(
                completion_plans("do it", goal.id),
                vec![execute_output(
                    "waited and finished",
                    vec![request("deploy", json!({}))],
                )],
            )),
            verdict_policy("deploy", GuardVerdict::Ask),
            backend.clone(),
        ));

        let run = spawn_run(&engine, task_id);
        let approval = wait_for_pending_approval(&store, task_id).await;
        // Give the executor a moment to register its waiter and settle.
        tokio::time::sleep(Duration::from_millis(10)).await;

        let intents = store.intents(task_id);
        assert_eq!(intents.len(), 1);
        assert_eq!(
            intents[0].state,
            IntentState::Running,
            "a waiting approval must not pause the intent"
        );
        assert_eq!(
            store.get_task(task_id).await.unwrap().unwrap().state,
            TaskState::Running
        );

        engine
            .decide_approval(approval.id, ApprovalDecision::Deny)
            .await
            .unwrap()
            .expect("the approval is pending");
        let result = join_run(run).await;
        assert_eq!(result.task.state, TaskState::Done);
        assert!(
            store
                .intents(task_id)
                .iter()
                .all(|intent| intent.state != IntentState::Paused),
            "no approval path leaves a paused intent"
        );
    }

    /// Deny: the worker receives a denial, the round continues, and the task
    /// does not stay stuck.
    #[tokio::test]
    async fn approval_deny_continues_same_run() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("deny and continue");
        let task_id = task.id;
        let goal = seed_goal(&store, &task).await;
        let backend = Arc::new(FakeBackend::new(json!({"deployed": true})));
        let engine = Arc::new(guarded_engine(
            &store,
            Arc::new(ScriptedAgent::with_plans(
                completion_plans("ship it", goal.id),
                vec![execute_output(
                    "the worker continued after denial",
                    vec![request("deploy", json!({"env": "prod"}))],
                )],
            )),
            verdict_policy("deploy", GuardVerdict::Ask),
            backend.clone(),
        ));

        let run = spawn_run(&engine, task_id);
        let approval = wait_for_pending_approval(&store, task_id).await;

        let resolved = engine
            .decide_approval(approval.id, ApprovalDecision::Deny)
            .await
            .unwrap()
            .expect("the approval is pending");
        assert_eq!(resolved.status, ApprovalStatus::Denied);

        let result = join_run(run).await;
        assert_eq!(result.task.state, TaskState::Done);
        assert_eq!(backend.calls(), 0, "a denied tool never runs");

        let intents = store.intents(task_id);
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].state, IntentState::Done);

        let facts = store.facts(task_id);
        assert!(
            facts
                .iter()
                .any(|f| f.claim == "the worker continued after denial"),
            "the worker's fact still lands after a denial"
        );
        let activities = store.activities(task_id);
        assert!(
            activities
                .iter()
                .any(|a| a.kind == "tool_denied" && a.payload["tool"] == "deploy"),
            "the denial is recorded"
        );
    }

    /// Expiry: the timer wakes the worker, the approval is expired, and the
    /// same `run_task` continues to `done`.
    #[tokio::test]
    async fn approval_expiry_continues_same_run() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("deploy without a reviewer");
        let task_id = task.id;
        let goal = seed_goal(&store, &task).await;
        let backend = Arc::new(FakeBackend::new(json!({"deployed": true})));
        let engine = Arc::new(guarded_engine_with_ttl(
            &store,
            Arc::new(ScriptedAgent::with_plans(
                completion_plans("ship it", goal.id),
                vec![execute_output(
                    "the worker continued after expiry",
                    vec![request("deploy", json!({"env": "prod"}))],
                )],
            )),
            verdict_policy("deploy", GuardVerdict::Ask),
            backend.clone(),
            chrono::Duration::milliseconds(50),
        ));

        let run = spawn_run(&engine, task_id);
        let approval = wait_for_pending_approval(&store, task_id).await;
        assert_eq!(
            store.intents(task_id)[0].state,
            IntentState::Running,
            "the waiting intent is running, not paused"
        );

        let result = join_run(run).await;
        assert_eq!(result.task.state, TaskState::Done);
        assert_eq!(backend.calls(), 0, "an expired tool never runs");

        let stored = store
            .get_approval(approval.id)
            .await
            .unwrap()
            .expect("the approval row exists");
        assert_eq!(stored.status, ApprovalStatus::Expired);
        assert!(stored.decided_at.is_none(), "expiry is not a decision");

        let intents = store.intents(task_id);
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].state, IntentState::Done);
    }

    /// The supervisor sweep is a backstop for a lost timer: it expires the
    /// row, wakes the worker, and the task still finishes.
    #[tokio::test]
    async fn approval_sweep_continues_same_run() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("sweep the wait");
        let task_id = task.id;
        let goal = seed_goal(&store, &task).await;
        let backend = Arc::new(FakeBackend::new(json!({"ok": true})));
        let engine = Arc::new(guarded_engine_with_ttl(
            &store,
            Arc::new(ScriptedAgent::with_plans(
                completion_plans("ship it", goal.id),
                vec![execute_output(
                    "the swept worker continued",
                    vec![request("deploy", json!({}))],
                )],
            )),
            verdict_policy("deploy", GuardVerdict::Ask),
            backend.clone(),
            chrono::Duration::milliseconds(50),
        ));

        let run = spawn_run(&engine, task_id);
        let approval = wait_for_pending_approval(&store, task_id).await;
        // Let the deadline pass, then drive the sweep explicitly.
        tokio::time::sleep(Duration::from_millis(75)).await;
        engine.sweep_approvals(task_id).await.unwrap();

        let result = join_run(run).await;
        assert_eq!(result.task.state, TaskState::Done);
        assert_eq!(backend.calls(), 0);

        let stored = store
            .get_approval(approval.id)
            .await
            .unwrap()
            .expect("the approval row exists");
        assert_eq!(stored.status, ApprovalStatus::Expired);

        // The sweep records the expiry once, in the new wording: the waiting
        // worker was denied, not an intent blocked.
        engine.sweep_approvals(task_id).await.unwrap();
        let expires: Vec<_> = store
            .activities(task_id)
            .into_iter()
            .filter(|a| a.kind == "approval_expired")
            .collect();
        assert_eq!(expires.len(), 1);
        assert!(
            expires[0].message.contains("waiting worker was denied"),
            "unexpected expiry message: {}",
            expires[0].message
        );
        assert!(!expires[0].message.contains("intent is blocked"));
    }

    /// `Engine::decide_approval` resolves the row and wakes a waiter; it never
    /// finishes an intent and never runs the tool itself.
    #[tokio::test]
    async fn decide_approval_only_resolves_the_row() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("decide only");
        let backend = Arc::new(FakeBackend::new(json!({"ok": true})));
        let engine = guarded_engine(
            &store,
            Arc::new(IdleAgent),
            verdict_policy("deploy", GuardVerdict::Ask),
            backend.clone(),
        );
        let intent = store.seed_intent(task.id, task.origin_fact_id, "wait for a person");
        store
            .claim_intent(intent.id, "worker-1")
            .await
            .unwrap()
            .expect("the seeded intent is claimable");
        let call = GuardStore::record_tool_call(
            store.as_ref(),
            task.id,
            intent.id,
            "deploy".to_string(),
            json!({}),
        )
        .await
        .unwrap();
        let decision = GuardStore::record_guard_decision(
            store.as_ref(),
            call.id,
            GuardVerdict::Ask,
            None,
            None,
            "needs a person".to_string(),
        )
        .await
        .unwrap();
        let approval = GuardStore::create_approval(
            store.as_ref(),
            task.id,
            call.id,
            decision.id,
            "needs a person".to_string(),
            Utc::now() + chrono::Duration::minutes(5),
        )
        .await
        .unwrap();

        let resolved = engine
            .decide_approval(approval.id, ApprovalDecision::Allow)
            .await
            .unwrap()
            .expect("the approval is pending");
        assert_eq!(resolved.status, ApprovalStatus::Granted);
        assert_eq!(backend.calls(), 0);
        assert_eq!(
            store.intents(task.id)[0].state,
            IntentState::Running,
            "a decision must not finish the intent"
        );
        assert!(
            store.list_results(task.id, 10).await.unwrap().is_empty(),
            "a decision must not record a result"
        );
    }

    #[tokio::test]
    async fn engine_does_not_overwrite_a_terminal_task_state() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("finish once");
        store
            .set_task_state_guarded(task.id, TaskState::Failed)
            .await
            .unwrap();

        let changed = store
            .set_task_state_guarded(task.id, TaskState::Done)
            .await
            .unwrap();

        assert!(!changed, "a second terminal status must not apply");
        assert_eq!(
            store.get_task(task.id).await.unwrap().unwrap().state,
            TaskState::Failed
        );
    }

    /// A 1 ms approval timer, so a sweep taken just after the call is
    /// reliably past it.
    fn tiny_ttl() -> chrono::Duration {
        chrono::Duration::milliseconds(1)
    }

    /// Two tasks waiting on separate approvals both continue after expiry and
    /// neither stays stuck.
    #[tokio::test]
    async fn expiry_does_not_leave_two_tasks_stuck() {
        let store = Arc::new(MemoryStore::new());
        let backend = Arc::new(FakeBackend::new(json!({"deployed": true})));
        let policy = verdict_policy("deploy", GuardVerdict::Ask);
        let task_a = store.seed_task("deploy A");
        let task_b = store.seed_task("deploy B");

        let mut runs = Vec::new();
        for task in [&task_a, &task_b] {
            let goal = seed_goal(&store, task).await;
            let engine = Arc::new(guarded_engine_with_ttl(
                &store,
                Arc::new(ScriptedAgent::with_plans(
                    completion_plans("ship it", goal.id),
                    vec![execute_output(
                        "the worker continued",
                        vec![request("deploy", json!({"env": "prod"}))],
                    )],
                )),
                policy.clone(),
                backend.clone(),
                tiny_ttl(),
            ));
            runs.push(spawn_run(&engine, task.id));
        }

        for run in runs {
            let result = join_run(run).await;
            assert_eq!(result.task.state, TaskState::Done);
        }
        assert_eq!(backend.calls(), 0, "an undecided tool never runs");

        for task_id in [task_a.id, task_b.id] {
            assert!(
                store
                    .list_pending_approvals(task_id)
                    .await
                    .unwrap()
                    .is_empty(),
                "no approval may stay pending in {task_id}"
            );
            let intents = store.intents(task_id);
            assert_eq!(intents.len(), 1);
            assert_eq!(intents[0].state, IntentState::Done);
            assert!(
                intents
                    .iter()
                    .all(|intent| intent.state != IntentState::Paused),
                "expiry must not pause an intent in {task_id}"
            );
        }
    }

    // ---------- exact expiry and atomic finish ----------

    /// The approval timer expires at the deadline on its own and wakes the
    /// worker. No supervisor sweep and no idling engine runs while it waits.
    #[tokio::test]
    async fn approval_timer_expires_at_deadline_without_sweep() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("deploy without a reviewer");
        let task_id = task.id;
        let goal = seed_goal(&store, &task).await;
        let backend = Arc::new(FakeBackend::new(json!({"deployed": true})));
        let engine = Arc::new(guarded_engine_with_ttl(
            &store,
            Arc::new(ScriptedAgent::with_plans(
                completion_plans("ship it", goal.id),
                vec![execute_output(
                    "the timer woke the worker",
                    vec![request("deploy", json!({"env": "prod"}))],
                )],
            )),
            verdict_policy("deploy", GuardVerdict::Ask),
            backend.clone(),
            chrono::Duration::milliseconds(50),
        ));

        let run = spawn_run(&engine, task_id);
        let approval = wait_for_pending_approval(&store, task_id).await;
        assert_eq!(
            store.intents(task_id)[0].state,
            IntentState::Running,
            "a waiting approval must not pause the intent"
        );

        let result = join_run(run).await;
        assert_eq!(result.task.state, TaskState::Done);
        assert_eq!(backend.calls(), 0);

        let stored = store
            .get_approval(approval.id)
            .await
            .unwrap()
            .expect("the approval row exists");
        assert_eq!(stored.status, ApprovalStatus::Expired);
        assert_eq!(store.intents(task_id)[0].state, IntentState::Done);
    }

    /// Two timers race to finish one paused intent. Only one may win, and the
    /// final state is the winner's.
    #[tokio::test]
    async fn finish_paused_intent_is_atomic() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("race the finish");
        let intent = store
            .add_intent(task.id, task.origin_fact_id, "one step".to_string())
            .await
            .unwrap();
        store
            .claim_intent(intent.id, "worker-1")
            .await
            .unwrap()
            .expect("an open intent is claimable");
        store
            .finish_intent(intent.id, IntentState::Paused)
            .await
            .unwrap();

        let (first, second) = tokio::join!(
            store.finish_paused_intent(intent.id, "approval", IntentState::Done),
            store.finish_paused_intent(intent.id, "expiry", IntentState::Blocked),
        );
        let first = first.unwrap();
        let second = second.unwrap();
        assert_eq!(
            usize::from(first.is_some()) + usize::from(second.is_some()),
            1,
            "exactly one finish may win the race"
        );
        let winner = first.or(second).expect("one finish wins");
        let stored = store.intents(task.id).remove(0);
        assert_eq!(
            stored.state, winner.state,
            "the final state is the one the winner asked for"
        );
        assert!(stored.state.is_terminal());
        assert!(stored.completed_at.is_some());

        // A non-paused intent never finishes again.
        assert!(
            store
                .finish_paused_intent(intent.id, "approval", IntentState::Done)
                .await
                .unwrap()
                .is_none(),
            "a finished intent cannot finish twice"
        );
        let open = store
            .add_intent(task.id, task.origin_fact_id, "still open".to_string())
            .await
            .unwrap();
        assert!(
            store
                .finish_paused_intent(open.id, "approval", IntentState::Done)
                .await
                .unwrap()
                .is_none(),
            "a non-paused intent returns None"
        );
        let open_intent = store
            .intents(task.id)
            .into_iter()
            .find(|i| i.id == open.id)
            .expect("the open intent exists");
        assert_eq!(open_intent.state, IntentState::Open);
    }

    /// A paused intent whose approval is gone can never be decided. The sweep
    /// blocks it instead of leaving the task paused forever.
    #[tokio::test]
    async fn orphaned_paused_intent_is_blocked() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("orphaned pause");
        let task_id = task.id;
        let intent = store
            .add_intent(task.id, task.origin_fact_id, "one step".to_string())
            .await
            .unwrap();
        store
            .claim_intent(intent.id, "worker-1")
            .await
            .unwrap()
            .expect("an open intent is claimable");
        store
            .finish_intent(intent.id, IntentState::Paused)
            .await
            .unwrap();
        // A tool call with no approval at all: nothing can decide it.
        GuardStore::record_tool_call(
            store.as_ref(),
            task_id,
            intent.id,
            "deploy".to_string(),
            json!({}),
        )
        .await
        .unwrap();

        assert_eq!(
            store.block_orphaned_paused_intents(task_id).await.unwrap(),
            1
        );
        let stored = store.intents(task_id).remove(0);
        assert_eq!(stored.state, IntentState::Blocked);
        assert!(stored.completed_at.is_some());
        // Idempotent: nothing is paused any more.
        assert_eq!(
            store.block_orphaned_paused_intents(task_id).await.unwrap(),
            0
        );
    }

    /// A paused intent that still has a pending approval is not orphaned and
    /// must stay paused.
    /// A legacy paused intent that still has a pending approval is not
    /// orphaned. Blocking approvals never create this state; the cleanup pass
    /// must still leave older rows alone while their approval is pending.
    #[tokio::test]
    async fn paused_intent_with_pending_approval_is_not_blocked() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("waiting in time");
        let task_id = task.id;
        let backend = Arc::new(FakeBackend::new(json!({})));
        let engine = guarded_engine(
            &store,
            Arc::new(IdleAgent),
            verdict_policy("deploy", GuardVerdict::Ask),
            backend,
        );
        let intent = store.seed_intent(task.id, task.origin_fact_id, "legacy pause");
        store
            .set_task_state_guarded(task.id, TaskState::Running)
            .await
            .unwrap();
        store
            .claim_intent(intent.id, "worker-1")
            .await
            .unwrap()
            .expect("the seeded intent is claimable");
        store
            .finish_intent(intent.id, IntentState::Paused)
            .await
            .unwrap();
        let call = GuardStore::record_tool_call(
            store.as_ref(),
            task_id,
            intent.id,
            "deploy".to_string(),
            json!({}),
        )
        .await
        .unwrap();
        let decision = GuardStore::record_guard_decision(
            store.as_ref(),
            call.id,
            GuardVerdict::Ask,
            None,
            None,
            "needs a person".to_string(),
        )
        .await
        .unwrap();
        let approval = GuardStore::create_approval(
            store.as_ref(),
            task_id,
            call.id,
            decision.id,
            "needs a person".to_string(),
            Utc::now() + chrono::Duration::minutes(5),
        )
        .await
        .unwrap();

        engine.sweep_approvals(task_id).await.unwrap();

        assert_eq!(
            store.intents(task_id)[0].state,
            IntentState::Paused,
            "an intent still waiting on a pending approval must stay paused"
        );
        let decided = store
            .decide_approval(approval.id, ApprovalDecision::Allow)
            .await
            .unwrap()
            .expect("the pending approval is still decidable");
        assert_eq!(decided.status, ApprovalStatus::Granted);
    }

    // ---------- round budget ----------
    #[tokio::test]
    async fn engine_idle_wait_does_not_consume_round_budget() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("wait for work");
        let goal = seed_goal(&store, &task).await;

        // Five idle planner ticks, more than the whole round budget, before
        // any real work appears.
        let mut plans: Vec<ScriptedPlan> =
            (0..5).map(|_| ScriptedPlan::Outcome(idle_plan())).collect();
        plans.push(ScriptedPlan::Outcome(single_intent_plan(
            "do the real work",
        )));
        plans.push(ScriptedPlan::ProveFirstConfirmed { goal_id: goal.id });

        let engine = Engine::new(
            store.clone(),
            Arc::new(ScriptedAgent::with_plans(
                plans,
                vec![execute_output("the work is done", vec![])],
            )),
        )
        .with_heartbeat(Duration::from_millis(10))
        .with_max_rounds(3);

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Done);
        // One round to add the intent, one to claim and execute it, one to
        // prove the goal. The five idle heartbeats in between spend nothing.
        assert_eq!(result.rounds, 3);
    }

    #[tokio::test]
    async fn max_rounds_stops_a_task_at_the_configured_value() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("round cap");
        let engine = Engine::new(
            store.clone(),
            Arc::new(ScriptedAgent::new(
                vec![
                    single_intent_plan("first"),
                    single_intent_plan("second"),
                    single_intent_plan("third"),
                ],
                vec![
                    execute_output("fact one", vec![]),
                    execute_output("fact two", vec![]),
                    execute_output("fact three", vec![]),
                ],
            )),
        )
        .with_max_rounds(3)
        .with_max_no_progress_rounds(16);

        let err = engine
            .run_task(task.id)
            .await
            .expect_err("the round cap ends the task");
        assert!(
            err.to_string().contains("engine exceeded max rounds (3)"),
            "{err}"
        );

        let row = store.get_task(task.id).await.unwrap().unwrap();
        assert_eq!(row.state, TaskState::Failed);
        let failed = store
            .activities(task.id)
            .into_iter()
            .find(|activity| activity.kind == "task_failed")
            .expect("failure row");
        assert_eq!(failed.payload["source"], "engine");
    }

    /// A per-task round ledger pauses the task instead of failing it, and the
    /// pause happens before the open intent is claimed.
    #[tokio::test]
    async fn capacity_round_limit_pauses_before_claiming() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("pause on rounds");
        let goal = seed_goal(&store, &task).await;
        GraphStore::set_task_capacity(
            store.as_ref(),
            task.id,
            TaskCapacity {
                max_rounds: Some(1),
                ..TaskCapacity::default()
            },
        )
        .await
        .unwrap();
        let engine = Engine::new(
            store.clone(),
            Arc::new(ScriptedAgent::with_plans(
                completion_plans("first", goal.id),
                vec![execute_output("first fact", vec![])],
            )),
        )
        .with_max_rounds(16);

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Paused);
        assert_eq!(result.rounds, 1);
        assert_eq!(
            store.get_task(task.id).await.unwrap().unwrap().state,
            TaskState::Paused
        );
        let exhausted = store
            .activities(task.id)
            .into_iter()
            .find(|activity| activity.kind == "capacity_exhausted")
            .expect("capacity_exhausted activity");
        assert_eq!(exhausted.payload["reason"], "rounds");
        assert_eq!(exhausted.payload["rounds"], 1);
        assert_eq!(exhausted.payload["max_rounds"], 1);
        assert!(
            !store
                .activities(task.id)
                .iter()
                .any(|activity| activity.kind == "task_failed"),
            "a capacity pause must not fail the task"
        );
        assert_eq!(
            store.intents(task.id)[0].state,
            IntentState::Open,
            "the pause comes before the intent is claimed"
        );
    }

    /// Extending the ledger and restarting the loop finishes the paused work.
    #[tokio::test]
    async fn capacity_extension_completes_a_paused_task() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("extend after pause");
        let goal = seed_goal(&store, &task).await;
        GraphStore::set_task_capacity(
            store.as_ref(),
            task.id,
            TaskCapacity {
                max_rounds: Some(1),
                ..TaskCapacity::default()
            },
        )
        .await
        .unwrap();
        let engine = Engine::new(
            store.clone(),
            Arc::new(ScriptedAgent::with_plans(
                completion_plans("first", goal.id),
                vec![execute_output("first fact", vec![])],
            )),
        )
        .with_max_rounds(16);

        let paused = engine.run_task(task.id).await.unwrap();
        assert_eq!(paused.task.state, TaskState::Paused);

        GraphStore::set_task_capacity(
            store.as_ref(),
            task.id,
            TaskCapacity {
                max_rounds: Some(16),
                ..TaskCapacity::default()
            },
        )
        .await
        .unwrap();
        assert!(
            GraphStore::set_task_state_guarded(store.as_ref(), task.id, TaskState::Running)
                .await
                .unwrap()
        );

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Done);
        assert_eq!(result.facts, 2, "the origin fact plus the worker fact");
        assert!(
            store
                .activities(task.id)
                .iter()
                .any(|activity| activity.kind == "capacity_exhausted")
        );
    }

    /// Model calls reported by the planner count against the ledger.
    #[tokio::test]
    async fn capacity_model_call_limit_pauses_a_task() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("pause on model calls");
        GraphStore::set_task_capacity(
            store.as_ref(),
            task.id,
            TaskCapacity {
                max_model_calls: Some(1),
                ..TaskCapacity::default()
            },
        )
        .await
        .unwrap();
        let engine =
            Engine::new(store.clone(), Arc::new(RecordingAgent::new())).with_max_rounds(16);

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Paused);
        assert_eq!(result.rounds, 1, "the planner added one intent");
        let exhausted = store
            .activities(task.id)
            .into_iter()
            .find(|activity| activity.kind == "capacity_exhausted")
            .expect("capacity_exhausted activity");
        assert_eq!(exhausted.payload["reason"], "model_calls");
        assert_eq!(exhausted.payload["model_calls"], 1);
        assert_eq!(exhausted.payload["max_model_calls"], 1);
    }

    /// A zero wall budget is already exhausted at the top of the first loop:
    /// the task pauses without making a model call and without sleeping.
    #[tokio::test]
    async fn capacity_wall_limit_pauses_a_task() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("pause on wall time");
        GraphStore::set_task_capacity(
            store.as_ref(),
            task.id,
            TaskCapacity {
                max_wall_secs: Some(0),
                ..TaskCapacity::default()
            },
        )
        .await
        .unwrap();
        let engine = Engine::new(store.clone(), Arc::new(IdleAgent));

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Paused);
        assert_eq!(result.rounds, 0);
        let exhausted = store
            .activities(task.id)
            .into_iter()
            .find(|activity| activity.kind == "capacity_exhausted")
            .expect("capacity_exhausted activity");
        assert_eq!(exhausted.payload["reason"], "wall_time");
        assert_eq!(exhausted.payload["max_wall_secs"], 0);
    }

    #[tokio::test]
    async fn no_progress_guard_does_not_stop_a_task_that_confirms_facts() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("steady progress");
        let goal = seed_goal(&store, &task).await;
        let engine = Engine::new(
            store.clone(),
            Arc::new(ScriptedAgent::with_plans(
                vec![
                    ScriptedPlan::Outcome(single_intent_plan("step one")),
                    ScriptedPlan::Outcome(single_intent_plan("step two")),
                    ScriptedPlan::Outcome(single_intent_plan("step three")),
                    ScriptedPlan::ProveFirstConfirmed { goal_id: goal.id },
                ],
                vec![
                    execute_output("fact one", vec![]),
                    execute_output("fact two", vec![]),
                    execute_output("fact three", vec![]),
                ],
            )),
        )
        .with_max_rounds(16)
        .with_max_no_progress_rounds(2);

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Done);
        assert_eq!(result.facts, 4, "the origin fact plus three worker facts");
        assert!(
            !store
                .activities(task.id)
                .iter()
                .any(|activity| activity.kind == "no_progress"),
            "facts across rounds are progress"
        );
    }

    /// A store whose worker facts are accepted but never persisted. It
    /// simulates a worker that records no facts, so the confirmed count can
    /// never move and a planner that keeps adding intents is genuinely stuck.
    struct FactlessStore {
        inner: Arc<MemoryStore>,
    }

    #[async_trait::async_trait]
    impl GraphStore for FactlessStore {
        async fn create_task(&self, goal: String) -> anyhow::Result<Task> {
            self.inner.create_task(goal).await
        }

        async fn create_task_with_pins(&self, task: dhi_core::NewTask) -> anyhow::Result<Task> {
            self.inner.create_task_with_pins(task).await
        }

        async fn get_task(&self, id: TaskId) -> anyhow::Result<Option<Task>> {
            self.inner.get_task(id).await
        }

        async fn list_tasks(&self, limit: i64) -> anyhow::Result<Vec<Task>> {
            self.inner.list_tasks(limit).await
        }

        async fn set_task_state_guarded(
            &self,
            id: TaskId,
            state: TaskState,
        ) -> anyhow::Result<bool> {
            self.inner.set_task_state_guarded(id, state).await
        }

        async fn set_task_capacity(
            &self,
            task_id: TaskId,
            capacity: TaskCapacity,
        ) -> anyhow::Result<Task> {
            self.inner.set_task_capacity(task_id, capacity).await
        }

        async fn add_goal(&self, task_id: TaskId, description: String) -> anyhow::Result<Goal> {
            self.inner.add_goal(task_id, description).await
        }

        async fn list_goals(&self, task_id: TaskId) -> anyhow::Result<Vec<Goal>> {
            self.inner.list_goals(task_id).await
        }

        async fn reopen_task_with_goal(
            &self,
            task_id: TaskId,
            description: String,
        ) -> anyhow::Result<ReopenOutcome> {
            self.inner.reopen_task_with_goal(task_id, description).await
        }

        async fn prove_goal(
            &self,
            task_id: TaskId,
            goal_id: GoalId,
            fact_id: FactId,
            reason: String,
        ) -> anyhow::Result<bool> {
            self.inner
                .prove_goal(task_id, goal_id, fact_id, reason)
                .await
        }

        async fn add_intent(
            &self,
            task_id: TaskId,
            fact_id: FactId,
            description: String,
        ) -> anyhow::Result<Intent> {
            self.inner.add_intent(task_id, fact_id, description).await
        }

        async fn list_open_intents(
            &self,
            task_id: TaskId,
            limit: i64,
        ) -> anyhow::Result<Vec<Intent>> {
            self.inner.list_open_intents(task_id, limit).await
        }

        async fn list_intents(&self, task_id: TaskId, limit: i64) -> anyhow::Result<Vec<Intent>> {
            self.inner.list_intents(task_id, limit).await
        }

        async fn save_intent_plan(
            &self,
            intent_id: IntentId,
            plan: &IntentPlan,
        ) -> anyhow::Result<()> {
            self.inner.save_intent_plan(intent_id, plan).await
        }

        async fn get_intent_plan(&self, intent_id: IntentId) -> anyhow::Result<Option<IntentPlan>> {
            self.inner.get_intent_plan(intent_id).await
        }

        async fn append_swarm_message(
            &self,
            task_id: TaskId,
            intent_id: IntentId,
            kind: &str,
            payload: serde_json::Value,
        ) -> anyhow::Result<()> {
            self.inner
                .append_swarm_message(task_id, intent_id, kind, payload)
                .await
        }

        async fn list_swarm_messages(
            &self,
            task_id: TaskId,
            limit: i64,
        ) -> anyhow::Result<Vec<SwarmMessageRecord>> {
            self.inner.list_swarm_messages(task_id, limit).await
        }

        async fn claim_intent(
            &self,
            intent_id: IntentId,
            owner: &str,
        ) -> anyhow::Result<Option<Intent>> {
            self.inner.claim_intent(intent_id, owner).await
        }

        async fn resume_intent(
            &self,
            intent_id: IntentId,
            owner: &str,
        ) -> anyhow::Result<Option<Intent>> {
            self.inner.resume_intent(intent_id, owner).await
        }

        async fn finish_intent(
            &self,
            intent_id: IntentId,
            state: IntentState,
        ) -> anyhow::Result<()> {
            self.inner.finish_intent(intent_id, state).await
        }

        async fn finish_paused_intent(
            &self,
            intent_id: IntentId,
            owner: &str,
            status: IntentState,
        ) -> anyhow::Result<Option<Intent>> {
            self.inner
                .finish_paused_intent(intent_id, owner, status)
                .await
        }

        async fn block_orphaned_paused_intents(&self, task_id: TaskId) -> anyhow::Result<usize> {
            self.inner.block_orphaned_paused_intents(task_id).await
        }

        async fn reset_running_intents(&self) -> anyhow::Result<usize> {
            self.inner.reset_running_intents().await
        }

        async fn record_fact(
            &self,
            task_id: TaskId,
            claim: String,
            provenance: serde_json::Value,
        ) -> anyhow::Result<Fact> {
            // Accepted but never persisted or confirmed: the confirmed count
            // cannot move.
            Ok(Fact {
                id: Uuid::new_v4(),
                task_id,
                claim,
                state: FactState::Origin,
                provenance,
                origin_of: None,
                created_at: Utc::now(),
            })
        }

        async fn record_fact_for_intent(
            &self,
            task_id: TaskId,
            _intent_id: IntentId,
            claim: String,
            provenance: serde_json::Value,
        ) -> anyhow::Result<Fact> {
            // Same as `record_fact`: accepted but never persisted, so the
            // confirmed count still cannot move.
            Ok(Fact {
                id: Uuid::new_v4(),
                task_id,
                claim,
                state: FactState::Origin,
                provenance,
                origin_of: None,
                created_at: Utc::now(),
            })
        }

        async fn list_facts(&self, task_id: TaskId) -> anyhow::Result<Vec<Fact>> {
            self.inner.list_facts(task_id).await
        }

        async fn record_result(
            &self,
            task_id: TaskId,
            intent_id: IntentId,
            claim: String,
            acceptance: ResultStatus,
            evidence: serde_json::Value,
        ) -> anyhow::Result<ResultRecord> {
            self.inner
                .record_result(task_id, intent_id, claim, acceptance, evidence)
                .await
        }

        async fn list_results(
            &self,
            task_id: TaskId,
            limit: i64,
        ) -> anyhow::Result<Vec<ResultRecord>> {
            self.inner.list_results(task_id, limit).await
        }

        async fn append_activity(
            &self,
            task_id: TaskId,
            kind: &str,
            message: String,
            payload: serde_json::Value,
        ) -> anyhow::Result<Activity> {
            self.inner
                .append_activity(task_id, kind, message, payload)
                .await
        }

        async fn list_activity(
            &self,
            task_id: TaskId,
            kind: Option<&str>,
        ) -> anyhow::Result<Vec<Activity>> {
            self.inner.list_activity(task_id, kind).await
        }

        fn save_agent_run<'a>(
            &'a self,
            run: &'a AgentRun,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>>
        {
            self.inner.save_agent_run(run)
        }

        async fn list_agent_runs(&self, task_id: TaskId) -> anyhow::Result<Vec<AgentRun>> {
            self.inner.list_agent_runs(task_id).await
        }

        async fn create_side_question_session(
            &self,
            task_id: TaskId,
            snapshot: serde_json::Value,
        ) -> anyhow::Result<SideQuestionSession> {
            self.inner
                .create_side_question_session(task_id, snapshot)
                .await
        }

        async fn get_open_side_question_session(
            &self,
            task_id: TaskId,
        ) -> anyhow::Result<Option<SideQuestionSession>> {
            self.inner.get_open_side_question_session(task_id).await
        }

        async fn get_side_question_session(
            &self,
            session_id: Uuid,
        ) -> anyhow::Result<Option<SideQuestionSession>> {
            self.inner.get_side_question_session(session_id).await
        }

        async fn list_side_question_sessions(
            &self,
            task_id: TaskId,
        ) -> anyhow::Result<Vec<SideQuestionThread>> {
            self.inner.list_side_question_sessions(task_id).await
        }

        async fn append_side_question(
            &self,
            request: NewSideQuestionRequest,
        ) -> anyhow::Result<SideQuestionRequest> {
            self.inner.append_side_question(request).await
        }
    }

    #[tokio::test]
    async fn no_progress_guard_stops_a_planner_that_adds_intents_without_facts() {
        let inner = Arc::new(MemoryStore::new());
        let task = inner.seed_task("planner spin");
        let plans: Vec<ScriptedPlan> = (0..10)
            .map(|i| ScriptedPlan::Outcome(single_intent_plan(&format!("intent {i}"))))
            .collect();
        let executes: Vec<dhi_agent::ExecuteOutput> = (0..10)
            .map(|i| execute_output(&format!("ignored fact {i}"), vec![]))
            .collect();
        let engine = Engine::new(
            Arc::new(FactlessStore {
                inner: inner.clone(),
            }),
            Arc::new(ScriptedAgent::with_plans(plans, executes)),
        )
        .with_max_rounds(100)
        .with_max_no_progress_rounds(3);

        let err = engine
            .run_task(task.id)
            .await
            .expect_err("the no-progress guard stops the spin");
        assert!(
            err.to_string()
                .contains("engine made no progress for 3 rounds"),
            "{err}"
        );

        let row = inner.get_task(task.id).await.unwrap().unwrap();
        assert_eq!(row.state, TaskState::Failed);
        let activities = inner.activities(task.id);
        let no_progress = activities
            .iter()
            .find(|activity| activity.kind == "no_progress")
            .expect("the guard leaves an activity row before failing");
        assert_eq!(no_progress.payload["limit"], 3);
        let failed = activities
            .iter()
            .find(|activity| activity.kind == "task_failed")
            .expect("failure row");
        assert_eq!(failed.payload["source"], "engine");
        assert_eq!(
            failed.payload["reason"],
            "engine made no progress for 3 rounds"
        );
    }

    // ---------- idle budget ----------

    #[tokio::test]
    async fn engine_does_not_idle_forever() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("planner that never decides");
        let engine = Engine::new(store.clone(), Arc::new(IdleAgent))
            .with_heartbeat(Duration::from_millis(10))
            .with_max_idle_ticks(3);

        // `IdleAgent` always returns `NoPlan`, so this engine has no path to
        // progress. The idle budget must terminate it.
        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Timeout);
        // Idle ticks are not rounds: no intent was ever added or claimed.
        assert_eq!(result.rounds, 0);
        assert_eq!(
            store.get_task(task.id).await.unwrap().unwrap().state,
            TaskState::Timeout
        );
        assert!(
            store
                .activities(task.id)
                .iter()
                .any(|a| a.kind == "engine_timeout" && a.payload["idle_ticks"] == 3)
        );
    }

    // ---------- guard bypass ----------

    #[tokio::test]
    async fn worker_tool_calls_cannot_reach_backend_without_executor() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("open a game");
        let backend = Arc::new(FakeBackend::new(json!({"game_id": "g-1"})));
        // Deliberately no `with_executor`: there is no guarded choke point.
        let engine = Engine::new(
            store.clone(),
            Arc::new(ScriptedAgent::new(
                vec![single_intent_plan("create the game")],
                vec![execute_output(
                    "the game started",
                    vec![request("createGame", json!({"color": "white"}))],
                )],
            )),
        );

        let err = engine.run_task(task.id).await.unwrap_err();

        assert!(err.to_string().contains("no executor"));
        // The backend is unreachable, so it was never invoked.
        assert_eq!(backend.calls(), 0);
        // Only the origin fact exists: the worker fact never passes the guard.
        let facts = store.facts(task.id);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].id, task.origin_fact_id);
    }

    // ---------- fact-based completion ----------

    /// Planner text cannot finish a task, whatever it says.
    #[tokio::test]
    async fn planner_cannot_set_done() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("text cannot finish");
        let plans = vec![
            PlanOutcome::NoPlan {
                reason: "done".to_string(),
            };
            3
        ];
        let engine = Engine::new(store.clone(), Arc::new(ScriptedAgent::new(plans, vec![])))
            .with_heartbeat(Duration::from_millis(10))
            .with_max_idle_ticks(2);

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Timeout);
        assert_ne!(result.task.state, TaskState::Done);
    }

    /// A worker fact never completes the task by itself; only a planner proof
    /// does. The worker has no acceptance channel.
    #[tokio::test]
    async fn worker_cannot_set_accepted() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("worker truth is a fact");
        let goal = seed_goal(&store, &task).await;
        let engine = Engine::new(
            store.clone(),
            Arc::new(ScriptedAgent::new(
                vec![single_intent_plan("do the work"), idle_plan()],
                vec![execute_output("the work produced a fact", vec![])],
            )),
        )
        .with_heartbeat(Duration::from_millis(10))
        .with_max_idle_ticks(1);

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Timeout);
        let goals = store.goals(task.id);
        assert_eq!(goals[0].id, goal.id);
        assert_eq!(goals[0].state, GoalState::Open);
        assert!(
            store.list_results(task.id, 10).await.unwrap().is_empty(),
            "the worker path writes no result row"
        );
        let worker_fact = store
            .facts(task.id)
            .into_iter()
            .find(|fact| fact.claim == "the work produced a fact")
            .expect("the worker fact is recorded");
        assert!(
            worker_fact.provenance["intent_id"].is_string(),
            "the worker fact carries its intent provenance"
        );
    }

    /// The worker records its evidence and confidence, not just a claim.
    #[tokio::test]
    async fn worker_fact_carries_evidence_and_confidence() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("record evidence");
        let engine = Engine::new(
            store.clone(),
            Arc::new(ScriptedAgent::new(
                vec![single_intent_plan("check the claim"), idle_plan()],
                vec![execute_output_with(
                    dhi_agent::FactDraft {
                        summary: "the claim holds".to_string(),
                        detail: String::new(),
                        evidence: "observed on the board".to_string(),
                        confidence: "inferred".to_string(),
                    },
                    vec![],
                )],
            )),
        )
        .with_heartbeat(Duration::from_millis(10))
        .with_max_idle_ticks(1);

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Timeout);
        let fact = store
            .facts(task.id)
            .into_iter()
            .find(|fact| fact.claim == "the claim holds")
            .expect("the worker fact is recorded");
        assert!(
            fact.provenance["intent_id"].is_string(),
            "the worker fact carries its intent provenance"
        );
        assert_eq!(fact.provenance["evidence"], "observed on the board");
        assert_eq!(fact.provenance["confidence"], "inferred");
        assert_eq!(fact.provenance["detail"], "");
        assert!(
            store.list_results(task.id, 10).await.unwrap().is_empty(),
            "the worker path writes no result row"
        );
    }

    /// The detail field survives the store round trip unchanged.
    #[tokio::test]
    async fn worker_fact_detail_is_preserved() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("record detail");
        let detail = "the first line\nand the second line";
        let engine = Engine::new(
            store.clone(),
            Arc::new(ScriptedAgent::new(
                vec![single_intent_plan("inspect the claim"), idle_plan()],
                vec![execute_output_with(
                    dhi_agent::FactDraft {
                        summary: "the detail survives".to_string(),
                        detail: detail.to_string(),
                        evidence: "read the source".to_string(),
                        confidence: "observed".to_string(),
                    },
                    vec![],
                )],
            )),
        )
        .with_heartbeat(Duration::from_millis(10))
        .with_max_idle_ticks(1);

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Timeout);
        let fact = store
            .facts(task.id)
            .into_iter()
            .find(|fact| fact.claim == "the detail survives")
            .expect("the worker fact is recorded");
        assert_eq!(fact.provenance["detail"], detail);
        assert_eq!(fact.provenance["evidence"], "read the source");
        assert_eq!(fact.provenance["confidence"], "observed");
    }

    /// An origin fact is not confirmed and cannot prove a goal. The rejected
    /// proof is a visible terminal failure, never a silent loop exit.
    #[tokio::test]
    async fn prove_goal_requires_confirmed_fact() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("origin cannot prove");
        let goal = seed_goal(&store, &task).await;
        let engine = Engine::new(
            store.clone(),
            Arc::new(ScriptedAgent::new(
                vec![PlanOutcome::ProveGoal {
                    goal_id: goal.id,
                    fact_id: task.origin_fact_id,
                    reason: "origin is enough".to_string(),
                }],
                vec![],
            )),
        );

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Failed);
        assert_eq!(store.goals(task.id)[0].state, GoalState::Open);
        assert_eq!(
            store.get_task(task.id).await.unwrap().unwrap().state,
            TaskState::Failed
        );
        let activities = store.activities(task.id);
        let rejected = activities
            .iter()
            .find(|activity| activity.kind == "proof_rejected")
            .expect("proof_rejected activity");
        assert_eq!(rejected.payload["goal_id"], goal.id.to_string());
        assert_eq!(rejected.payload["fact_id"], task.origin_fact_id.to_string());
        let failed = activities
            .iter()
            .find(|activity| activity.kind == "task_failed")
            .expect("task_failed activity");
        assert_eq!(failed.payload["source"], "proof");
        assert!(
            failed.payload["reason"]
                .as_str()
                .unwrap_or_default()
                .starts_with("proof rejected:"),
            "{failed:?}"
        );
    }

    /// Proving one of two goals leaves the task running and records the fact.
    #[tokio::test]
    async fn prove_goal_marks_goal_met() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("prove one of two");
        let first = seed_goal(&store, &task).await;
        let second = store
            .add_goal(task.id, "a second goal".to_string())
            .await
            .unwrap();
        let fact = store
            .record_fact(task.id, "confirmed evidence".to_string(), json!({}))
            .await
            .unwrap();
        let engine = Engine::new(
            store.clone(),
            Arc::new(ScriptedAgent::new(
                vec![prove_plan(first.id, fact.id), idle_plan()],
                vec![],
            )),
        )
        .with_heartbeat(Duration::from_millis(10))
        .with_max_idle_ticks(1);

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Timeout);
        let goals = store.goals(task.id);
        let first = goals.iter().find(|goal| goal.id == first.id).unwrap();
        assert_eq!(first.state, GoalState::Met);
        assert_eq!(first.proof_fact_id, Some(fact.id));
        let second = goals.iter().find(|goal| goal.id == second.id).unwrap();
        assert_eq!(second.state, GoalState::Open);
    }

    /// A proof of the last open goal sets the task done.
    #[tokio::test]
    async fn all_goals_met_sets_task_done() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("prove the only goal");
        let goal = seed_goal(&store, &task).await;
        let fact = store
            .record_fact(task.id, "confirmed evidence".to_string(), json!({}))
            .await
            .unwrap();
        let engine = Engine::new(
            store.clone(),
            Arc::new(ScriptedAgent::new(
                vec![prove_plan(goal.id, fact.id)],
                vec![],
            )),
        );

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Done);
        assert_eq!(
            store.get_task(task.id).await.unwrap().unwrap().state,
            TaskState::Done
        );
        assert_eq!(store.goals(task.id)[0].state, GoalState::Met);
        assert!(
            store
                .activities(task.id)
                .iter()
                .any(|activity| activity.kind == "goal_proven"
                    && activity.payload["goal_id"] == goal.id.to_string()
                    && activity.payload["fact_id"] == fact.id.to_string())
        );
    }

    /// A confirmed fact from another task cannot prove this task's goal. The
    /// rejection fails the task visibly instead of leaving it running.
    #[tokio::test]
    async fn fact_from_another_task_cannot_prove_goal() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("task A");
        let other = store.seed_task("task B");
        let goal = seed_goal(&store, &task).await;
        let foreign_fact = store
            .record_fact(other.id, "another task's evidence".to_string(), json!({}))
            .await
            .unwrap();
        let engine = Engine::new(
            store.clone(),
            Arc::new(ScriptedAgent::new(
                vec![prove_plan(goal.id, foreign_fact.id)],
                vec![],
            )),
        );

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Failed);
        assert_eq!(store.goals(task.id)[0].state, GoalState::Open);
        assert_eq!(
            store.get_task(task.id).await.unwrap().unwrap().state,
            TaskState::Failed
        );
        assert!(
            store
                .activities(task.id)
                .iter()
                .any(|activity| activity.kind == "proof_rejected")
        );
    }

    #[test]
    fn no_model_field_named_done() {
        for outcome in [
            PlanOutcome::AddIntent {
                description: "work".to_string(),
                plan: None,
            },
            PlanOutcome::ProveGoal {
                goal_id: Uuid::new_v4(),
                fact_id: Uuid::new_v4(),
                reason: "evidence".to_string(),
            },
            PlanOutcome::NoPlan {
                reason: "nothing".to_string(),
            },
        ] {
            let json = serde_json::to_string(&outcome).unwrap();
            assert!(!json.contains("\"done\""), "{json}");
        }
    }

    #[test]
    fn no_model_field_named_accepted() {
        let output = dhi_agent::ExecuteOutput {
            fact: fact_draft("a fact"),
            tool_calls: vec![],
        };
        let json = serde_json::to_value(&output).unwrap();
        assert!(json.get("accepted").is_none(), "{json}");
        assert!(json.get("result").is_none(), "{json}");
    }

    #[test]
    fn task_capacity_clamps_the_plan_parallel_budget() {
        let mut plan = two_worker_plan("capacity", Topology::ManagerWorker);
        plan.limits.max_parallel = 3;

        let clamped = apply_task_capacity(
            plan.clone(),
            TaskCapacity {
                max_parallel_workers: Some(1),
                ..TaskCapacity::default()
            },
        );

        assert_eq!(clamped.limits.max_parallel, 1);
        assert_eq!(plan.limits.max_parallel, 3, "the input plan is unchanged");
    }

    #[test]
    fn task_capacity_without_parallel_workers_keeps_the_plan_budget() {
        let mut plan = two_worker_plan("capacity", Topology::ManagerWorker);
        plan.limits.max_parallel = 3;

        let unchanged = apply_task_capacity(plan.clone(), TaskCapacity::default());

        assert_eq!(unchanged, plan);
    }

    #[test]
    fn task_capacity_zero_parallel_workers_is_ignored() {
        let mut plan = two_worker_plan("capacity", Topology::ManagerWorker);
        plan.limits.max_parallel = 3;

        let unchanged = apply_task_capacity(
            plan.clone(),
            TaskCapacity {
                max_parallel_workers: Some(0),
                ..TaskCapacity::default()
            },
        );

        assert_eq!(unchanged, plan);
    }

    /// A swarm plan with one explicitly assigned subtask per worker.
    fn swarm_plan(worker_count: usize) -> IntentPlan {
        let workers: Vec<WorkerSpec> = (0..worker_count)
            .map(|index| team_worker(&format!("w{index}"), "researcher"))
            .collect();
        let subtasks: Vec<Subtask> = (0..worker_count)
            .map(|index| {
                team_subtask(
                    &format!("s{index}"),
                    &format!("subtask {index}"),
                    &format!("w{index}"),
                )
            })
            .collect();
        IntentPlan {
            intent: "swarm".to_string(),
            topology: Topology::Swarm,
            workers,
            subtasks,
            limits: CapabilityLimits::default(),
        }
    }

    fn swarm_task(workers: Option<usize>) -> Task {
        let mut task = Task::new("swarm");
        task.topology = Some(Topology::Swarm);
        task.swarm_workers = workers;
        // A narrow DHI+ capacity must never throttle a swarm below its team.
        task.capacity.max_parallel_workers = Some(1);
        task
    }

    #[test]
    fn fixed_swarm_counts_two_eight_and_sixty_four_are_accepted() {
        for workers in [2, 8, 64] {
            let plan = swarm_plan(workers);
            assert!(
                enforce_swarm_worker_count(Some(workers), &plan).is_ok(),
                "{workers} workers must be accepted"
            );

            let effective = effective_team_plan(plan, &swarm_task(Some(workers)));
            assert_eq!(
                effective.limits.max_parallel, workers,
                "a fixed swarm runs every teammate at once"
            );
            assert!(
                effective.limits.max_workers >= workers,
                "max_workers must admit all {workers} teammates"
            );
            assert!(
                effective.validate().is_ok(),
                "the normalized {workers}-worker plan must validate"
            );
        }
    }

    #[test]
    fn fixed_swarm_counts_one_and_sixty_five_are_rejected() {
        for workers in [1, 65] {
            let err = enforce_swarm_worker_count(Some(workers), &swarm_plan(2))
                .expect_err("out-of-range worker counts are rejected");
            assert!(err.contains("outside"), "{err}");
        }
    }

    #[test]
    fn auto_swarm_runs_every_declared_teammate_at_once() {
        let effective = effective_team_plan(swarm_plan(6), &swarm_task(None));

        assert_eq!(effective.limits.max_parallel, 6);
        assert_eq!(effective.limits.max_workers, 8);
        assert!(effective.validate().is_ok());
    }

    #[test]
    fn auto_swarm_never_widens_the_approval_free_cap_by_itself() {
        let unapproved = effective_team_plan(swarm_plan(10), &swarm_task(None));

        assert_eq!(
            unapproved.limits.max_workers, 8,
            "Auto must keep the planner's max_workers; only a granted scale approval may widen it"
        );
        assert!(
            unapproved.validate().is_err(),
            "a ten-worker Auto plan without a raised max_workers must not run"
        );
    }

    #[test]
    fn manager_worker_still_honors_the_task_parallel_capacity() {
        let mut task = Task::new("managed");
        task.topology = Some(Topology::ManagerWorker);
        task.capacity.max_parallel_workers = Some(1);

        let effective =
            effective_team_plan(two_worker_plan("managed", Topology::ManagerWorker), &task);

        assert_eq!(effective.limits.max_parallel, 1);
    }

    // ---------- DHI+ plan execution ----------

    /// Scripted team agent whose runtime manager keeps only alpha and assigns
    /// every subtask to it. Every other call delegates to the scripted team
    /// agent.
    struct SelectingTeamAgent {
        team: TeamAgent,
    }

    impl SelectingTeamAgent {
        fn new(plans: Vec<ScriptedPlan>, workers: Vec<(&str, TeamWorker)>) -> Self {
            Self {
                team: TeamAgent::new(plans, workers),
            }
        }

        fn calls(&self) -> Vec<ExecuteInput> {
            self.team.calls()
        }
    }

    #[async_trait::async_trait]
    impl AgentRuntime for SelectingTeamAgent {
        async fn plan(
            &self,
            input: PlanInput,
        ) -> anyhow::Result<dhi_agent::AgentCall<PlanOutcome>> {
            self.team.plan(input).await
        }

        async fn execute(
            &self,
            input: ExecuteInput,
        ) -> anyhow::Result<dhi_agent::AgentCall<dhi_agent::ExecuteOutput>> {
            self.team.execute(input).await
        }

        async fn select_team(
            &self,
            input: dhi_agent::TeamSelectionInput,
        ) -> anyhow::Result<dhi_agent::AgentCall<IntentPlan>> {
            let mut selected = input.plan;
            selected.workers.retain(|worker| worker.key == "alpha");
            for subtask in &mut selected.subtasks {
                subtask.worker_key = Some("alpha".to_string());
            }
            let record = call_record(Role::Worker, "manager-m");
            Ok(dhi_agent::AgentCall {
                output: selected,
                meta: dhi_agent::AgentCallMeta {
                    usage: record.usage,
                    calls: vec![record],
                },
                prompt: "manager prompt".to_string(),
                response: "manager response".to_string(),
                thinking: Some("manager reasoning".to_string()),
            })
        }

        fn name(&self) -> &str {
            "selecting"
        }
    }

    struct AlphaOnlySelector;

    #[async_trait::async_trait]
    impl TeamSelector for AlphaOnlySelector {
        async fn select(&self, plan: &IntentPlan) -> anyhow::Result<IntentPlan> {
            let mut selected = plan.clone();
            selected.workers.retain(|worker| worker.key == "alpha");
            for subtask in &mut selected.subtasks {
                subtask.worker_key = Some("alpha".to_owned());
            }
            Ok(selected)
        }
    }

    #[tokio::test]
    async fn engine_honors_an_optional_manager_selector() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("team selector");
        let goal = seed_goal(&store, &task).await;
        let plan = two_worker_plan("team selector", Topology::ManagerWorker);
        let agent = Arc::new(TeamAgent::new(
            team_completion_plans("team selector", plan, goal.id),
            vec![
                ("subtask one", TeamWorker::Success("finding one")),
                ("subtask two", TeamWorker::Success("finding two")),
            ],
        ));
        let engine =
            Engine::new(store.clone(), agent.clone()).with_manager_selector(AlphaOnlySelector);

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Done);
        let calls = agent.calls();
        assert_eq!(calls.len(), 2);
        assert!(
            calls
                .iter()
                .all(|call| call.intent.contains("Worker role: researcher")),
            "both subtasks ran through the selected worker"
        );
        let team_run = store
            .activities(task.id)
            .into_iter()
            .find(|activity| activity.kind == "team_run")
            .expect("team_run activity");
        assert_eq!(team_run.payload["worker_keys"], json!(["alpha", "alpha"]));
        assert_eq!(
            team_run.payload["manager_model_calls"], 0,
            "an explicit selector reports no runtime manager calls"
        );
    }

    #[tokio::test]
    async fn engine_uses_the_runtime_manager_selection() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("runtime manager");
        let goal = seed_goal(&store, &task).await;
        // The planner offers alpha and beta but starts the only subtask on
        // beta; the runtime manager selects alpha only.
        let mut plan = two_worker_plan("runtime manager", Topology::ManagerWorker);
        plan.subtasks.truncate(1);
        plan.subtasks[0].worker_key = Some("beta".to_string());
        let agent = Arc::new(SelectingTeamAgent::new(
            team_completion_plans("runtime manager", plan, goal.id),
            vec![("subtask one", TeamWorker::Recorded("alpha finding"))],
        ));
        let engine = Engine::new(store.clone(), agent.clone());

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Done);
        let calls = agent.calls();
        assert_eq!(calls.len(), 1, "only the selected worker runs");
        assert!(
            calls[0].intent.contains("Worker role: researcher"),
            "the selected worker executed the subtask"
        );

        let team_run = store
            .activities(task.id)
            .into_iter()
            .find(|activity| activity.kind == "team_run")
            .expect("team_run activity");
        assert_eq!(team_run.payload["worker_keys"], json!(["alpha"]));
        assert_eq!(team_run.payload["success"], 1);
        assert_eq!(team_run.payload["manager_model_calls"], 1);

        let runs = store.agent_runs(task.id);
        let mut keys: Vec<_> = runs
            .iter()
            .filter_map(|run| run.worker_key.clone())
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["alpha", "manager"]);

        let intent_id = store.intents(task.id)[0].id;
        let manager = runs
            .iter()
            .find(|run| run.worker_key.as_deref() == Some("manager"))
            .expect("manager agent run");
        assert_eq!(manager.intent_id, Some(intent_id));
        assert_eq!(manager.prompt, "manager prompt");
        assert_eq!(manager.response.as_deref(), Some("manager response"));
        assert_eq!(manager.thinking.as_deref(), Some("manager reasoning"));
    }

    /// A manager selection reports its completion call through the selector
    /// counter, which the engine reads after the runner returns the plan.
    #[tokio::test]
    async fn runtime_manager_selection_counts_its_model_calls() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("manager model calls");
        let agent = Arc::new(SelectingTeamAgent::new(Vec::new(), Vec::new()));
        let model_calls = Arc::new(AtomicUsize::new(0));
        let selector = RuntimeTeamSelector {
            store,
            agent,
            task_id: task.id,
            intent_id: Uuid::new_v4(),
            intent: "manager model calls".to_string(),
            worker_profile_id: None,
            model_calls: Arc::clone(&model_calls),
        };
        let plan = two_worker_plan("manager model calls", Topology::ManagerWorker);

        let selected = selector.select(&plan).await.unwrap();

        assert_eq!(model_calls.load(Ordering::SeqCst), 1);
        assert_eq!(selected.workers.len(), 1, "the selected plan is returned");
        assert_eq!(selected.subtasks[0].worker_key.as_deref(), Some("alpha"));
    }

    /// The manager's reported call spends the task's model budget: the next
    /// capacity check pauses the task even though the planner and worker
    /// reported no calls of their own.
    #[tokio::test]
    async fn capacity_model_call_limit_counts_the_manager_selection() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("manager capacity");
        let goal = seed_goal(&store, &task).await;
        GraphStore::set_task_capacity(
            store.as_ref(),
            task.id,
            TaskCapacity {
                max_model_calls: Some(1),
                ..TaskCapacity::default()
            },
        )
        .await
        .unwrap();
        let mut plan = two_worker_plan("manager capacity", Topology::ManagerWorker);
        plan.subtasks.truncate(1);
        let agent = Arc::new(SelectingTeamAgent::new(
            team_completion_plans("manager capacity", plan, goal.id),
            vec![("subtask one", TeamWorker::Success("manager finding"))],
        ));
        let engine = Engine::new(store.clone(), agent.clone());

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Paused);
        assert_eq!(result.rounds, 2, "the planner added and the team claimed");
        let exhausted = store
            .activities(task.id)
            .into_iter()
            .find(|activity| activity.kind == "capacity_exhausted")
            .expect("capacity_exhausted activity");
        assert_eq!(exhausted.payload["reason"], "model_calls");
        assert_eq!(exhausted.payload["model_calls"], 1);
        assert_eq!(exhausted.payload["max_model_calls"], 1);
        let team_run = store
            .activities(task.id)
            .into_iter()
            .find(|activity| activity.kind == "team_run")
            .expect("team_run activity");
        assert_eq!(team_run.payload["manager_model_calls"], 1);
        assert_eq!(team_run.payload["success"], 1);
        assert_eq!(
            store.intents(task.id)[0].state,
            IntentState::Done,
            "the pause comes after the manager/worker round finished"
        );
    }

    /// A task's parallel capacity narrows a manager plan without stopping it,
    /// and the stored planner plan keeps its original budget.
    #[tokio::test]
    async fn capacity_clamps_manager_worker_parallelism() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("capacity team");
        let goal = seed_goal(&store, &task).await;
        let mut plan = two_worker_plan("capacity team", Topology::ManagerWorker);
        plan.limits.max_parallel = 3;
        GraphStore::set_task_capacity(
            store.as_ref(),
            task.id,
            TaskCapacity {
                max_parallel_workers: Some(1),
                ..TaskCapacity::default()
            },
        )
        .await
        .unwrap();
        let agent = Arc::new(TeamAgent::new(
            team_completion_plans("capacity team", plan.clone(), goal.id),
            vec![
                ("subtask one", TeamWorker::Success("finding one")),
                ("subtask two", TeamWorker::Success("finding two")),
            ],
        ));
        let engine = Engine::new(store.clone(), agent.clone());

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Done);
        assert_eq!(agent.calls().len(), 2, "both subtasks still run");
        let team_run = store
            .activities(task.id)
            .into_iter()
            .find(|activity| activity.kind == "team_run")
            .expect("team_run activity");
        assert_eq!(team_run.payload["max_parallel"], 1);

        let intent_id = store.intents(task.id)[0].id;
        let stored = store
            .get_intent_plan(intent_id)
            .await
            .unwrap()
            .expect("stored planner plan");
        assert_eq!(
            stored.limits.max_parallel, 3,
            "capacity applies to the run, not the stored plan"
        );
    }

    #[tokio::test]
    async fn manager_worker_plan_records_exactly_one_fact() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("team task");
        let goal = seed_goal(&store, &task).await;
        let plan = two_worker_plan("team work", Topology::ManagerWorker);
        let agent = Arc::new(TeamAgent::new(
            team_completion_plans("team work", plan, goal.id),
            vec![
                ("subtask one", TeamWorker::Success("finding one")),
                ("subtask two", TeamWorker::Success("finding two")),
            ],
        ));
        let engine = Engine::new(store.clone(), agent.clone());

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Done);
        assert_eq!(agent.calls().len(), 2, "both workers ran once");

        let facts = store.facts(task.id);
        assert_eq!(facts.len(), 2, "origin fact plus exactly one team fact");
        let team_fact = facts
            .iter()
            .find(|fact| fact.id != task.origin_fact_id)
            .expect("the team fact");
        assert!(team_fact.claim.contains("finding one"), "{team_fact:?}");
        assert!(team_fact.claim.contains("finding two"), "{team_fact:?}");
        assert_eq!(team_fact.provenance["topology"], "manager_worker");
        assert_eq!(team_fact.provenance["workers"].as_array().unwrap().len(), 2);

        let intents = store.intents(task.id);
        assert_eq!(intents.len(), 1);
        assert_eq!(
            store.fact_for_intent(intents[0].id).map(|fact| fact.id),
            Some(team_fact.id),
            "the team fact is bound to its one intent"
        );
        assert_eq!(intents[0].state, IntentState::Done);
        assert!(
            store.swarm_messages(task.id).is_empty(),
            "manager/worker plans persist no swarm messages"
        );
        assert_eq!(team_fact.provenance["intent_id"], intents[0].id.to_string());
        assert!(
            store
                .get_intent_plan(intents[0].id)
                .await
                .unwrap()
                .is_some(),
            "the plan is persisted before execution and kept as audit data"
        );

        let team_run = store
            .activities(task.id)
            .into_iter()
            .find(|activity| activity.kind == "team_run")
            .expect("team_run activity");
        assert_eq!(team_run.payload["topology"], "manager_worker");
        assert_eq!(team_run.payload["success"], 2);
        assert_eq!(team_run.payload["failed"], 0);
        assert!(engine.plans.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn manager_worker_call_usage_is_recorded_as_one_agent_run() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("team usage");
        let goal = seed_goal(&store, &task).await;
        let plan = two_worker_plan("team usage", Topology::ManagerWorker);
        let agent = Arc::new(TeamAgent::new(
            team_completion_plans("team usage", plan, goal.id),
            vec![
                ("subtask one", TeamWorker::Recorded("recorded team finding")),
                ("subtask two", TeamWorker::Success("plain finding")),
            ],
        ));
        let engine = Engine::new(store.clone(), agent);

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Done);
        let runs = store.agent_runs(task.id);
        let workers: Vec<_> = runs
            .iter()
            .filter(|run| run.role == AgentRole::Worker)
            .collect();
        assert_eq!(
            workers.len(),
            1,
            "only the worker that reported a completion call is recorded"
        );
        let row = workers[0];
        assert_eq!(row.tokens_in, 250);
        assert_eq!(row.tokens_out, 80);
        assert_eq!(row.cache_read_tokens, 100);
        assert_eq!(row.reasoning_tokens, 5);
        assert_eq!(row.model, "team-worker-m");
        assert_eq!(row.prompt, "team prompt for subtask one");
        assert_eq!(row.response.as_deref(), Some("recorded team finding"));
        assert_eq!(row.thinking.as_deref(), Some("team worker reasoning"));
        assert_eq!(row.worker_key.as_deref(), Some("alpha"));
        let intent_id = row.intent_id.expect("the team worker row names its intent");
        assert_eq!(
            store.intents(task.id)[0].id,
            intent_id,
            "the row is attached to the team intent"
        );

        let team_run = store
            .activities(task.id)
            .into_iter()
            .find(|activity| activity.kind == "team_run")
            .expect("team_run activity");
        assert_eq!(
            team_run.payload["worker_keys"],
            json!(["alpha", "beta"]),
            "the activity names the workers that ran"
        );
    }

    #[tokio::test]
    async fn manager_worker_runs_record_distinct_worker_keys() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("team identity");
        let goal = seed_goal(&store, &task).await;
        let plan = two_worker_plan("team identity", Topology::ManagerWorker);
        let agent = Arc::new(TeamAgent::new(
            team_completion_plans("team identity", plan, goal.id),
            vec![
                ("subtask one", TeamWorker::Recorded("finding one")),
                ("subtask two", TeamWorker::Recorded("finding two")),
            ],
        ));
        let engine = Engine::new(store.clone(), agent);

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Done);
        let intent_id = store.intents(task.id)[0].id;
        let runs = store.agent_runs(task.id);
        let workers: Vec<_> = runs
            .iter()
            .filter(|run| run.role == AgentRole::Worker)
            .collect();
        assert_eq!(workers.len(), 2, "one row per recorded subworker call");

        let mut keys: Vec<_> = workers
            .iter()
            .map(|run| run.worker_key.as_deref().expect("a worker key"))
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["alpha", "beta"]);
        assert!(
            workers.iter().all(|run| run.intent_id == Some(intent_id)),
            "every subworker row names the team intent"
        );
    }

    #[tokio::test]
    async fn team_worker_tool_calls_run_once_through_the_guard() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("team tool call");
        let goal = seed_goal(&store, &task).await;
        let plan = two_worker_plan("team tool call", Topology::ManagerWorker);
        let agent = Arc::new(TeamAgent::new(
            team_completion_plans("team tool call", plan, goal.id),
            vec![
                (
                    "subtask one",
                    TeamWorker::Calls(
                        "the tool ran",
                        vec![request("createGame", json!({"color": "white"}))],
                    ),
                ),
                ("subtask two", TeamWorker::Success("plain finding")),
            ],
        ));
        let backend = Arc::new(FakeBackend::new(json!({"game_id": "g-1"})));
        let engine = guarded_engine(
            &store,
            agent,
            verdict_policy("createGame", GuardVerdict::Allow),
            backend.clone(),
        );

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Done);
        assert_eq!(backend.calls(), 1, "the allowed call ran exactly once");
        let calls = store.tool_calls(task.id);
        assert_eq!(calls.len(), 1);
        let intent_id = store.intents(task.id)[0].id;
        assert_eq!(calls[0].intent_id, intent_id);
        assert_eq!(calls[0].tool_name, "createGame");
        assert_eq!(calls[0].status, "ok");

        let allowed = store
            .activities(task.id)
            .into_iter()
            .find(|activity| activity.kind == "tool_allowed")
            .expect("tool_allowed activity");
        assert_eq!(allowed.payload["worker_key"], "alpha");
        assert_eq!(allowed.payload["intent_id"], intent_id.to_string());
        let facts = store.facts(task.id);
        assert_eq!(facts.len(), 2, "origin fact plus one team fact");
    }

    #[tokio::test]
    async fn team_worker_denied_tool_call_is_audited_and_never_runs() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("denied team tool");
        let goal = seed_goal(&store, &task).await;
        let plan = two_worker_plan("denied team tool", Topology::ManagerWorker);
        let agent = Arc::new(TeamAgent::new(
            team_completion_plans("denied team tool", plan, goal.id),
            vec![
                (
                    "subtask one",
                    TeamWorker::Calls(
                        "the denial was handled",
                        vec![request("rmrf", json!({"path": "/"}))],
                    ),
                ),
                ("subtask two", TeamWorker::Success("plain finding")),
            ],
        ));
        let backend = Arc::new(FakeBackend::new(json!({"should": "never run"})));
        let engine = guarded_engine(
            &store,
            agent,
            verdict_policy("rmrf", GuardVerdict::Deny),
            backend.clone(),
        );

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Done);
        assert_eq!(
            backend.calls(),
            0,
            "a denied call never reaches the backend"
        );
        let calls = store.tool_calls(task.id);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].status, "denied");
        assert_eq!(calls[0].intent_id, store.intents(task.id)[0].id);

        let denied = store
            .activities(task.id)
            .into_iter()
            .find(|activity| activity.kind == "tool_denied")
            .expect("tool_denied activity");
        assert_eq!(denied.payload["tool"], "rmrf");
        assert_eq!(denied.payload["worker_key"], "alpha");
        assert_eq!(denied.payload["intent_id"], calls[0].intent_id.to_string());

        let facts = store.facts(task.id);
        assert_eq!(
            facts.len(),
            2,
            "the denial is audited but the team fact is still recorded"
        );
        assert_eq!(store.intents(task.id)[0].state, IntentState::Done);
    }

    #[tokio::test]
    async fn team_tool_call_error_fails_the_task_without_the_team_fact() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("failing team tool");
        let goal = seed_goal(&store, &task).await;
        let plan = two_worker_plan("failing team tool", Topology::ManagerWorker);
        let agent = Arc::new(TeamAgent::new(
            team_completion_plans("failing team tool", plan, goal.id),
            vec![
                (
                    "subtask one",
                    TeamWorker::Calls("must not land", vec![request("deploy", json!({}))]),
                ),
                ("subtask two", TeamWorker::Success("plain finding")),
            ],
        ));
        let engine = guarded_engine(
            &store,
            agent,
            verdict_policy("deploy", GuardVerdict::Allow),
            Arc::new(FailingBackend),
        );

        let err = engine
            .run_task(task.id)
            .await
            .expect_err("an executor error fails the task");

        assert!(err.to_string().contains("backend exploded"), "{err}");
        assert_eq!(
            store.get_task(task.id).await.unwrap().unwrap().state,
            TaskState::Failed
        );
        let facts = store.facts(task.id);
        assert_eq!(facts.len(), 1, "only the origin fact exists");
        assert_eq!(facts[0].id, task.origin_fact_id);
        let intents = store.intents(task.id);
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].state, IntentState::Stopped);
        let calls = store.tool_calls(task.id);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].status, "error");
        let failed = store
            .activities(task.id)
            .into_iter()
            .find(|activity| activity.kind == "task_failed")
            .expect("task_failed activity");
        assert_eq!(failed.payload["source"], "worker");
        assert!(
            failed.payload["reason"]
                .as_str()
                .unwrap_or_default()
                .contains("backend exploded")
        );
    }

    #[tokio::test]
    async fn team_tool_calls_cannot_reach_backend_without_executor() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("unguarded team tool");
        let plan = two_worker_plan("unguarded team tool", Topology::ManagerWorker);
        let agent = Arc::new(TeamAgent::new(
            vec![ScriptedPlan::Outcome(planned_intent(
                "unguarded team tool",
                plan,
            ))],
            vec![
                (
                    "subtask one",
                    TeamWorker::Calls("must not land", vec![request("deploy", json!({}))]),
                ),
                ("subtask two", TeamWorker::Success("plain finding")),
            ],
        ));
        let backend = Arc::new(FakeBackend::new(json!({"should": "never run"})));
        let engine = Engine::new(store.clone(), agent);

        let err = engine.run_task(task.id).await.unwrap_err();

        assert!(err.to_string().contains("no executor"), "{err}");
        assert_eq!(backend.calls(), 0);
        let facts = store.facts(task.id);
        assert_eq!(facts.len(), 1, "only the origin fact exists");
        assert_eq!(facts[0].id, task.origin_fact_id);
        assert_eq!(
            store.get_task(task.id).await.unwrap().unwrap().state,
            TaskState::Failed
        );
        assert_eq!(store.intents(task.id)[0].state, IntentState::Stopped);
    }

    #[tokio::test]
    async fn partially_failed_team_records_one_fact_naming_the_failure() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("partial team");
        let goal = seed_goal(&store, &task).await;
        let plan = two_worker_plan("partial work", Topology::ManagerWorker);
        let agent = Arc::new(TeamAgent::new(
            team_completion_plans("partial work", plan, goal.id),
            vec![
                ("subtask one", TeamWorker::Success("the surviving finding")),
                ("subtask two", TeamWorker::Failure("beta exploded")),
            ],
        ));
        let engine = Engine::new(store.clone(), agent);

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Done);
        let facts = store.facts(task.id);
        assert_eq!(facts.len(), 2, "origin fact plus one aggregated fact");
        let team_fact = facts
            .iter()
            .find(|fact| fact.id != task.origin_fact_id)
            .expect("the team fact");
        assert!(team_fact.claim.contains("the surviving finding"));
        let evidence = team_fact.provenance["evidence"]
            .as_str()
            .expect("evidence text");
        assert!(evidence.contains("beta"), "{evidence}");
        assert!(evidence.contains("beta exploded"), "{evidence}");
        let workers = team_fact.provenance["workers"].as_array().unwrap();
        let failed = workers
            .iter()
            .find(|worker| worker["key"] == "beta")
            .expect("beta result");
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["error"], "beta exploded");

        let team_run = store
            .activities(task.id)
            .into_iter()
            .find(|activity| activity.kind == "team_run")
            .expect("team_run activity");
        assert_eq!(team_run.payload["success"], 1);
        assert_eq!(team_run.payload["failed"], 1);
        assert_eq!(store.intents(task.id)[0].state, IntentState::Done);
    }

    #[tokio::test]
    async fn zero_success_team_returns_to_the_planner_and_recovers() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("failed team");
        let goal = seed_goal(&store, &task).await;
        let doomed = two_worker_plan("doomed work", Topology::ManagerWorker);
        let retry = IntentPlan {
            intent: "recovered work".to_string(),
            topology: Topology::ManagerWorker,
            workers: vec![team_worker("gamma", "researcher")],
            subtasks: vec![team_subtask("s3", "recovered subtask", "gamma")],
            limits: CapabilityLimits::default(),
        };
        let agent = Arc::new(TeamAgent::new(
            vec![
                ScriptedPlan::Outcome(planned_intent("doomed work", doomed)),
                ScriptedPlan::Outcome(planned_intent("recovered work", retry)),
                ScriptedPlan::ProveFirstConfirmed { goal_id: goal.id },
            ],
            vec![
                ("subtask one", TeamWorker::Failure("alpha exploded")),
                ("subtask two", TeamWorker::Failure("beta exploded")),
                (
                    "recovered subtask",
                    TeamWorker::Success("the retry found it"),
                ),
            ],
        ));
        let engine = Engine::new(store.clone(), agent.clone());

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(
            result.task.state,
            TaskState::Done,
            "a failed team returns to the planner; it does not fail the task"
        );
        let intents = store.intents(task.id);
        assert_eq!(intents.len(), 2, "the planner added a second intent");
        let failed_intent = intents
            .iter()
            .find(|intent| intent.description == "doomed work")
            .expect("the failed intent");
        let retried_intent = intents
            .iter()
            .find(|intent| intent.description == "recovered work")
            .expect("the retried intent");
        assert_eq!(failed_intent.state, IntentState::Stopped);
        assert_eq!(retried_intent.state, IntentState::Done);

        let activities = store.activities(task.id);
        assert!(
            !activities
                .iter()
                .any(|activity| activity.kind == "task_failed"),
            "no host guard fired, so the task must not fail"
        );
        let team_failed = activities
            .iter()
            .find(|activity| activity.kind == "team_failed")
            .expect("team_failed activity");
        assert_eq!(
            team_failed.payload["intent_id"],
            failed_intent.id.to_string()
        );
        assert_eq!(team_failed.payload["success"], 0);
        let details = team_failed.payload["worker_details"]
            .as_array()
            .expect("worker details");
        assert_eq!(details.len(), 2);
        assert!(
            details
                .iter()
                .any(|worker| worker["error"] == "alpha exploded"),
            "{details:?}"
        );

        let results = store.list_results(task.id, 10).await.unwrap();
        assert_eq!(results.len(), 1, "one rejected result for the failed team");
        assert_eq!(results[0].intent_id, failed_intent.id);
        assert_eq!(results[0].acceptance, ResultStatus::Rejected);
        assert!(
            results[0].claim.contains("no successful worker result"),
            "{}",
            results[0].claim
        );

        let planner_inputs = agent.plan_inputs();
        assert_eq!(
            planner_inputs.len(),
            3,
            "the planner decides after the failure and after the recovery"
        );
        assert!(
            planner_inputs[0].failures.is_empty(),
            "the first plan has no prior failure"
        );
        assert!(
            planner_inputs[1].failures.iter().any(|failure| {
                failure.contains("no successful worker result")
                    && failure.contains("alpha exploded")
                    && failure.contains("beta exploded")
            }),
            "{:?}",
            planner_inputs[1].failures
        );

        let facts = store.facts(task.id);
        assert_eq!(facts.len(), 2, "origin fact plus the recovered team fact");
        assert!(
            facts
                .iter()
                .any(|fact| fact.claim.contains("the retry found it")),
            "{facts:?}"
        );
    }

    #[tokio::test]
    async fn worker_call_failure_returns_to_the_planner() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("failed worker");
        let goal = seed_goal(&store, &task).await;
        let doomed = IntentPlan {
            intent: "doomed work".to_string(),
            topology: Topology::Single,
            workers: vec![],
            subtasks: vec![],
            limits: CapabilityLimits::default(),
        };
        let retry = IntentPlan {
            intent: "recovered work".to_string(),
            topology: Topology::Single,
            workers: vec![],
            subtasks: vec![],
            limits: CapabilityLimits::default(),
        };
        let agent = Arc::new(TeamAgent::new(
            vec![
                ScriptedPlan::Outcome(planned_intent("doomed work", doomed)),
                ScriptedPlan::Outcome(planned_intent("recovered work", retry)),
                ScriptedPlan::ProveFirstConfirmed { goal_id: goal.id },
            ],
            vec![
                ("doomed work", TeamWorker::Failure("worker exploded")),
                ("recovered work", TeamWorker::Success("the retry found it")),
            ],
        ));
        let engine = Engine::new(store.clone(), agent.clone());

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(
            result.task.state,
            TaskState::Done,
            "a failed worker returns to the planner; it does not fail the task"
        );
        let intents = store.intents(task.id);
        assert_eq!(intents.len(), 2, "the planner added a second intent");
        let failed_intent = intents
            .iter()
            .find(|intent| intent.description == "doomed work")
            .expect("the failed intent");
        let retried_intent = intents
            .iter()
            .find(|intent| intent.description == "recovered work")
            .expect("the retried intent");
        assert_eq!(failed_intent.state, IntentState::Exhausted);
        assert_eq!(retried_intent.state, IntentState::Done);

        let activities = store.activities(task.id);
        assert!(
            !activities
                .iter()
                .any(|activity| activity.kind == "task_failed"),
            "no host guard fired, so the task must not fail"
        );
        let intent_failed: Vec<_> = activities
            .iter()
            .filter(|activity| activity.kind == "intent_failed")
            .collect();
        assert_eq!(intent_failed.len(), 1, "one failed worker call");
        assert_eq!(
            intent_failed[0].payload["intent_id"],
            failed_intent.id.to_string()
        );
        assert!(
            intent_failed[0].message.contains("worker exploded"),
            "{}",
            intent_failed[0].message
        );

        let planner_inputs = agent.plan_inputs();
        assert_eq!(
            planner_inputs.len(),
            3,
            "the planner decides after the failure and after the recovery"
        );
        assert!(
            planner_inputs[1]
                .failures
                .iter()
                .any(|failure| failure.contains("worker exploded")),
            "{:?}",
            planner_inputs[1].failures
        );
    }

    #[tokio::test]
    async fn team_workers_receive_exactly_the_planners_turn_budget() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("exact team turns");
        let goal = seed_goal(&store, &task).await;
        let mut plan = two_worker_plan("exact team turns", Topology::ManagerWorker);
        // Alpha is below the old four-turn floor; beta proves the exact value
        // is not clamped to some other ceiling either.
        plan.workers[0].max_turns = 2;
        plan.workers[1].max_turns = 9;
        let agent = Arc::new(TeamAgent::new(
            team_completion_plans("exact team turns", plan, goal.id),
            vec![
                ("subtask one", TeamWorker::Success("finding one")),
                ("subtask two", TeamWorker::Success("finding two")),
            ],
        ));
        let engine = Engine::new(store.clone(), agent.clone());

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Done);
        let calls = agent.calls();
        assert_eq!(calls.len(), 2);
        for call in &calls {
            let expected = match assigned_subtask(&call.intent) {
                "subtask one" => 2,
                "subtask two" => 9,
                other => panic!("unexpected worker subtask: {other}"),
            };
            assert_eq!(
                call.max_turns,
                Some(expected),
                "the planner's budget reaches the worker unchanged"
            );
        }
    }

    #[tokio::test]
    async fn invalid_plan_fails_the_task_with_a_worker_engine_reason() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("broken plan");
        let plan = IntentPlan {
            intent: "broken work".to_string(),
            topology: Topology::ManagerWorker,
            workers: vec![],
            subtasks: vec![team_subtask("s1", "unassigned work", "missing")],
            limits: CapabilityLimits::default(),
        };
        let agent = Arc::new(TeamAgent::new(
            vec![ScriptedPlan::Outcome(planned_intent("broken work", plan))],
            vec![],
        ));
        let engine = Engine::new(store.clone(), agent);

        let err = engine
            .run_task(task.id)
            .await
            .expect_err("an invalid plan fails the task");
        assert!(err.to_string().contains("worker/engine"), "{err}");

        assert_eq!(
            store.get_task(task.id).await.unwrap().unwrap().state,
            TaskState::Failed
        );
        assert_eq!(store.facts(task.id).len(), 1, "only the origin fact exists");
        assert_eq!(store.intents(task.id)[0].state, IntentState::Stopped);
        assert!(engine.plans.lock().unwrap().is_empty());
        let failed = store
            .activities(task.id)
            .into_iter()
            .find(|activity| activity.kind == "task_failed")
            .expect("task_failed activity");
        assert_eq!(failed.payload["source"], "worker");
        assert!(
            failed.payload["reason"]
                .as_str()
                .unwrap_or_default()
                .contains("worker/engine")
        );
    }

    #[tokio::test]
    async fn swarm_plan_records_one_fact() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("swarm task");
        let goal = seed_goal(&store, &task).await;
        let plan = two_worker_plan("swarm work", Topology::Swarm);
        let agent = Arc::new(TeamAgent::new(
            team_completion_plans("swarm work", plan, goal.id),
            vec![
                ("subtask one", TeamWorker::Success("swarm finding one")),
                ("subtask two", TeamWorker::Success("swarm finding two")),
            ],
        ));
        let engine = Engine::new(store.clone(), agent.clone());

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Done);
        assert_eq!(agent.calls().len(), 2, "each peer ran its seed once");
        let facts = store.facts(task.id);
        assert_eq!(facts.len(), 2, "origin fact plus exactly one team fact");
        let team_fact = facts
            .iter()
            .find(|fact| fact.id != task.origin_fact_id)
            .expect("the team fact");
        assert_eq!(team_fact.provenance["topology"], "swarm");
        assert_eq!(team_fact.provenance["workers"].as_array().unwrap().len(), 2);
        let team_run = store
            .activities(task.id)
            .into_iter()
            .find(|activity| activity.kind == "team_run")
            .expect("team_run activity");
        assert_eq!(team_run.payload["topology"], "swarm");
        assert_eq!(team_run.payload["success"], 2);
        let intents = store.intents(task.id);
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].state, IntentState::Done);

        let messages = store.swarm_messages(task.id);
        assert!(
            !messages.is_empty(),
            "a swarm run persists its typed broadcast log"
        );
        assert!(
            messages
                .iter()
                .all(|message| message.intent_id == intents[0].id),
            "every swarm message belongs to the plan's intent"
        );
        assert!(
            messages.iter().any(|message| message.kind == "finding"),
            "seed findings are persisted"
        );
        assert!(
            messages.iter().all(|message| matches!(
                message.kind.as_str(),
                "finding" | "question" | "challenge" | "done"
            )),
            "only typed logical messages are persisted"
        );
        assert!(
            messages
                .iter()
                .all(|message| message.payload["type"].is_string()),
            "the payload is the serialized typed message"
        );
    }

    /// Reaction turns are real model calls: they must count against
    /// `max_model_calls`, become `agent_runs`, and never enter the fact.
    #[tokio::test]
    async fn swarm_reactions_count_against_capacity_and_become_agent_runs() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("swarm reaction capacity");
        let goal = seed_goal(&store, &task).await;
        let mut plan = two_worker_plan("swarm reaction capacity", Topology::Swarm);
        for worker in &mut plan.workers {
            worker.max_turns = 2;
        }
        plan.limits.max_rounds = 2;
        plan.limits.max_messages = 32;
        GraphStore::set_task_capacity(
            store.as_ref(),
            task.id,
            TaskCapacity {
                max_model_calls: Some(3),
                ..TaskCapacity::default()
            },
        )
        .await
        .unwrap();
        let agent = Arc::new(AllTurnsAgent::new(team_completion_plans(
            "swarm reaction capacity",
            plan,
            goal.id,
        )));
        let engine = Engine::new(store.clone(), agent.clone()).with_max_rounds(16);

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Paused);
        assert_eq!(agent.calls(), 4, "two seeds plus two reactions");
        let team_run = store
            .activities(task.id)
            .into_iter()
            .find(|activity| activity.kind == "team_run")
            .expect("team_run activity");
        assert_eq!(team_run.payload["success"], 2);
        assert_eq!(
            team_run.payload["reactions"], 2,
            "reaction turns are counted in the run summary"
        );
        let exhausted = store
            .activities(task.id)
            .into_iter()
            .find(|activity| activity.kind == "capacity_exhausted")
            .expect("capacity_exhausted activity");
        assert_eq!(
            exhausted.payload["model_calls"], 4,
            "all four worker calls are charged"
        );
        let runs = store.agent_runs(task.id);
        let worker_runs: Vec<_> = runs
            .iter()
            .filter(|run| {
                run.worker_key.as_deref() == Some("alpha")
                    || run.worker_key.as_deref() == Some("beta")
            })
            .collect();
        assert_eq!(
            worker_runs.len(),
            4,
            "each seed and reaction has one agent_runs row"
        );
        let facts = store.facts(task.id);
        assert_eq!(facts.len(), 2, "origin plus exactly one team fact");
        let fact = facts
            .iter()
            .find(|fact| fact.id != task.origin_fact_id)
            .expect("team fact");
        assert!(
            !fact.claim.contains("swarm finding 3") && !fact.claim.contains("swarm finding 4"),
            "reaction turns never enter the fact: {}",
            fact.claim
        );
    }

    #[tokio::test]
    async fn no_plan_intent_keeps_the_single_worker_path() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("single path");
        let goal = seed_goal(&store, &task).await;
        let agent = Arc::new(ScriptedAgent::with_plans(
            completion_plans("single work", goal.id),
            vec![execute_output("single worker finding", vec![])],
        ));
        let engine = Engine::new(store.clone(), agent);

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Done);
        let facts = store.facts(task.id);
        assert_eq!(facts.len(), 2, "origin fact plus one worker fact");
        let worker_fact = facts
            .iter()
            .find(|fact| fact.id != task.origin_fact_id)
            .expect("the worker fact");
        assert_eq!(worker_fact.claim, "single worker finding");
        assert!(
            worker_fact.provenance.get("topology").is_none(),
            "the single path writes no team provenance"
        );
        assert!(
            !store
                .activities(task.id)
                .iter()
                .any(|activity| activity.kind == "team_run"),
            "the single path never runs a team"
        );
        assert!(engine.plans.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn missing_plan_on_resume_falls_back_to_single_worker() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("resume fallback");
        let goal = seed_goal(&store, &task).await;
        // Simulate a planned intent added by a previous process: its activity
        // marker survives but its plan row is gone, so the new engine's load
        // returns `None`.
        let intent = store
            .add_intent(task.id, task.origin_fact_id, "resumed work".to_string())
            .await
            .unwrap();
        store
            .append_activity(
                task.id,
                "intents_added",
                "planner added one intent".to_string(),
                json!({"round": 1, "intent_id": intent.id, "has_plan": true}),
            )
            .await
            .unwrap();

        let agent = Arc::new(ScriptedAgent::with_plans(
            vec![ScriptedPlan::ProveFirstConfirmed { goal_id: goal.id }],
            vec![execute_output("fallback finding", vec![])],
        ));
        let engine = Engine::new(store.clone(), agent);

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Done);
        let facts = store.facts(task.id);
        let fallback = facts
            .iter()
            .find(|fact| fact.claim == "fallback finding")
            .expect("the single worker ran");
        assert!(
            fallback.provenance.get("topology").is_none(),
            "a resumed intent without its plan uses single-worker provenance"
        );
        assert_eq!(store.intents(task.id)[0].state, IntentState::Done);
    }

    #[tokio::test]
    async fn restarted_engine_loads_the_persisted_plan() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("restart task");
        let goal = seed_goal(&store, &task).await;
        let plan = two_worker_plan("restart work", Topology::ManagerWorker);
        let intent = store
            .add_intent(task.id, task.origin_fact_id, "restart work".to_string())
            .await
            .unwrap();
        // A previous process persisted the planner's plan before it died.
        store.save_intent_plan(intent.id, &plan).await.unwrap();
        // The restarted engine starts with an empty in-memory cache.
        let previous = Engine::new(store.clone(), Arc::new(ScriptedAgent::new(vec![], vec![])));
        drop(previous);

        let agent = Arc::new(TeamAgent::new(
            vec![ScriptedPlan::ProveFirstConfirmed { goal_id: goal.id }],
            vec![
                ("subtask one", TeamWorker::Success("finding one")),
                ("subtask two", TeamWorker::Success("finding two")),
            ],
        ));
        let engine = Engine::new(store.clone(), agent.clone());

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Done);
        assert_eq!(
            agent.calls().len(),
            2,
            "the persisted manager-worker plan ran both workers"
        );
        let facts = store.facts(task.id);
        assert_eq!(facts.len(), 2, "origin fact plus exactly one team fact");
        let team_fact = facts
            .iter()
            .find(|fact| fact.id != task.origin_fact_id)
            .expect("the team fact");
        assert_eq!(team_fact.provenance["topology"], "manager_worker");
        assert_eq!(store.intents(task.id)[0].state, IntentState::Done);
        assert_eq!(
            store.get_intent_plan(intent.id).await.unwrap(),
            Some(plan),
            "the plan row survives completion as audit data"
        );
        assert!(
            engine.plans.lock().unwrap().is_empty(),
            "completion drops only the in-memory cache"
        );
    }

    #[tokio::test]
    async fn completed_plan_is_removed_from_the_engine_map() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("cleanup task");
        let plan = two_worker_plan("cleanup work", Topology::ManagerWorker);
        let agent = Arc::new(TeamAgent::new(
            vec![
                ScriptedPlan::Outcome(planned_intent("cleanup work", plan)),
                ScriptedPlan::Outcome(idle_plan()),
            ],
            vec![
                ("subtask one", TeamWorker::Success("one")),
                ("subtask two", TeamWorker::Success("two")),
            ],
        ));
        let engine = Engine::new(store.clone(), agent)
            .with_heartbeat(Duration::from_millis(10))
            .with_max_idle_ticks(1);

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Timeout);
        assert_eq!(store.intents(task.id)[0].state, IntentState::Done);
        assert!(
            engine.plans.lock().unwrap().is_empty(),
            "a finished intent drops its plan"
        );
    }

    // ---------- heartbeat ----------

    #[tokio::test]
    async fn duplicate_fact_fails_the_single_worker_task_cleanly() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("duplicate fact");
        let intent = store.seed_intent(task.id, task.origin_fact_id, "the only intent");
        let first = store
            .record_fact_for_intent(
                task.id,
                intent.id,
                "already recorded".to_string(),
                json!({}),
            )
            .await
            .unwrap();

        let engine = Engine::new(store.clone(), Arc::new(MockAgentRuntime));
        let err = engine
            .run_task(task.id)
            .await
            .expect_err("a duplicate intent fact fails the task");

        let reason = format!("fact already recorded for intent {}", intent.id);
        assert!(err.to_string().contains(&reason), "{err}");
        assert_eq!(
            store.get_task(task.id).await.unwrap().unwrap().state,
            TaskState::Failed
        );
        assert_eq!(
            store.intents(task.id)[0].state,
            IntentState::Stopped,
            "the intent must not finish done"
        );
        let facts = store.facts(task.id);
        assert_eq!(facts.len(), 2, "origin fact plus the one intent fact");
        assert_eq!(store.fact_for_intent(intent.id).unwrap().id, first.id);

        let failed = store
            .activities(task.id)
            .into_iter()
            .find(|activity| activity.kind == "task_failed")
            .expect("task_failed activity");
        assert_eq!(failed.payload["source"], "worker");
        assert!(
            failed.payload["reason"].as_str().unwrap().contains(&reason),
            "{failed:?}"
        );
    }

    #[tokio::test]
    async fn duplicate_fact_in_a_planned_intent_fails_the_task_cleanly() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("duplicate planned fact");
        let plan = two_worker_plan("team work", Topology::ManagerWorker);
        let intent = store.seed_intent(task.id, task.origin_fact_id, "team work");
        store.save_intent_plan(intent.id, &plan).await.unwrap();
        let first = store
            .record_fact_for_intent(
                task.id,
                intent.id,
                "already recorded".to_string(),
                json!({}),
            )
            .await
            .unwrap();

        let agent = Arc::new(TeamAgent::new(
            vec![],
            vec![
                ("subtask one", TeamWorker::Success("finding one")),
                ("subtask two", TeamWorker::Success("finding two")),
            ],
        ));
        let engine = Engine::new(store.clone(), agent.clone());
        let err = engine
            .run_task(task.id)
            .await
            .expect_err("a duplicate planned fact fails the task");

        let reason = format!("fact already recorded for intent {}", intent.id);
        assert!(err.to_string().contains(&reason), "{err}");
        assert_eq!(
            store.get_task(task.id).await.unwrap().unwrap().state,
            TaskState::Failed
        );
        assert_eq!(
            store.intents(task.id)[0].state,
            IntentState::Stopped,
            "the planned intent must not finish done"
        );
        let facts = store.facts(task.id);
        assert_eq!(facts.len(), 2, "origin fact plus the one intent fact");
        assert_eq!(store.fact_for_intent(intent.id).unwrap().id, first.id);
        assert_eq!(
            store.get_intent_plan(intent.id).await.unwrap(),
            Some(plan),
            "the plan row survives as audit data"
        );
        assert!(engine.plans.lock().unwrap().is_empty());
        assert_eq!(agent.calls().len(), 2, "both planned workers still ran");
    }

    #[tokio::test]
    async fn heartbeat_wakes_idle_engine() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("wait then finish");
        let goal = seed_goal(&store, &task).await;
        let heartbeat = Duration::from_millis(50);
        let engine = Engine::new(
            store.clone(),
            Arc::new(ScriptedAgent::with_plans(
                vec![
                    ScriptedPlan::Outcome(idle_plan()),
                    ScriptedPlan::Outcome(single_intent_plan("finish the work")),
                    ScriptedPlan::ProveFirstConfirmed { goal_id: goal.id },
                ],
                vec![execute_output("the work is done", vec![])],
            )),
        )
        .with_heartbeat(heartbeat)
        .with_max_rounds(3);

        let started = Instant::now();
        let result = engine.run_task(task.id).await.unwrap();
        let elapsed = started.elapsed();

        assert_eq!(result.task.state, TaskState::Done);
        // The idle heartbeat spends nothing; the three real steps do.
        assert_eq!(result.rounds, 3);
        // The idle round must actually hibernate for at least one heartbeat.
        assert!(elapsed >= heartbeat);
    }

    // ---------- manager-worker lead wiring ----------

    async fn manager_task(store: &MemoryStore, goal: &str) -> (Task, Goal) {
        let task = GraphStore::create_task_with_pins(
            store,
            dhi_core::NewTask::new(goal).with_topology(Some(Topology::ManagerWorker)),
        )
        .await
        .unwrap();
        let goal = store.add_goal(task.id, task.goal.clone()).await.unwrap();
        (task, goal)
    }

    #[tokio::test]
    async fn manager_lead_can_start_with_no_initial_subtasks() {
        let store = Arc::new(MemoryStore::new());
        let (task, goal) = manager_task(&store, "empty initial plan").await;
        let mut plan = two_worker_plan("empty initial plan", Topology::ManagerWorker);
        plan.subtasks.clear();
        let agent = Arc::new(LeadAgent::new(
            vec![
                ScriptedPlan::Outcome(planned_intent("empty initial plan", plan)),
                ScriptedPlan::ProveFirstConfirmed { goal_id: goal.id },
            ],
            vec![],
            vec![dhi_core::LeadDecision::Final {
                summary: "empty-plan lead summary".to_string(),
                detail: String::new(),
                evidence: "the lead planned directly".to_string(),
                confidence: "observed".to_string(),
            }],
        ));
        let engine = Engine::new(store.clone(), agent.clone()).with_lead_runtime();

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Done);
        assert_eq!(agent.review_inputs().len(), 1);
        assert!(
            store
                .facts(task.id)
                .iter()
                .any(|fact| fact.claim == "empty-plan lead summary")
        );
    }

    #[tokio::test]
    async fn manager_lead_final_records_the_intent_fact() {
        let store = Arc::new(MemoryStore::new());
        let (task, goal) = manager_task(&store, "lead the work").await;
        let agent = Arc::new(
            LeadAgent::new(
                vec![
                    ScriptedPlan::Outcome(planned_intent(
                        "lead the work",
                        two_worker_plan("lead the work", Topology::ManagerWorker),
                    )),
                    ScriptedPlan::ProveFirstConfirmed { goal_id: goal.id },
                ],
                vec![
                    execute_output("alpha finding", vec![]),
                    execute_output("beta finding", vec![]),
                ],
                vec![dhi_core::LeadDecision::Final {
                    summary: "lead final summary".to_string(),
                    detail: "the lead merged both findings".to_string(),
                    evidence: "alpha and beta".to_string(),
                    confidence: "observed".to_string(),
                }],
            )
            .recorded(),
        );
        let engine = Engine::new(store.clone(), agent.clone()).with_lead_runtime();

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Done);
        let facts = store.facts(task.id);
        assert!(
            facts.iter().any(|fact| fact.claim == "lead final summary"),
            "the lead's final decision supplies the fact: {facts:?}"
        );
        let runs = store.agent_runs(task.id);
        assert!(
            runs.iter()
                .any(|run| run.worker_key.as_deref() == Some("manager")),
            "the lead call is an agent run under manager"
        );
        assert_eq!(agent.review_inputs().len(), 1);
    }

    #[tokio::test]
    async fn manager_lead_continue_runs_direct_assignments() {
        let store = Arc::new(MemoryStore::new());
        let (task, goal) = manager_task(&store, "continue the lead").await;
        let agent = Arc::new(
            LeadAgent::new(
                vec![
                    ScriptedPlan::Outcome(planned_intent(
                        "continue the lead",
                        two_worker_plan("continue the lead", Topology::ManagerWorker),
                    )),
                    ScriptedPlan::ProveFirstConfirmed { goal_id: goal.id },
                ],
                vec![
                    execute_output("alpha initial", vec![]),
                    execute_output("beta initial", vec![]),
                    execute_output("alpha follow-up", vec![]),
                ],
                vec![
                    dhi_core::LeadDecision::Continue {
                        tasks: vec![dhi_core::LeadTask {
                            id: "follow-1".to_string(),
                            title: "follow up".to_string(),
                            description: "check the edge case".to_string(),
                            assignee: "alpha".to_string(),
                            depends_on: Vec::new(),
                        }],
                        reason: "one more check".to_string(),
                    },
                    dhi_core::LeadDecision::Final {
                        summary: "continued lead summary".to_string(),
                        detail: String::new(),
                        evidence: "three worker turns".to_string(),
                        confidence: "observed".to_string(),
                    },
                ],
            )
            .recorded(),
        );
        let engine = Engine::new(store.clone(), agent.clone()).with_lead_runtime();

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Done);
        assert_eq!(agent.review_inputs().len(), 2);
        assert!(
            store
                .facts(task.id)
                .iter()
                .any(|fact| fact.claim == "continued lead summary")
        );
    }

    #[tokio::test]
    async fn manager_lead_ask_worker_runs_one_pool_turn() {
        let store = Arc::new(MemoryStore::new());
        let (task, goal) = manager_task(&store, "ask a worker").await;
        let agent = Arc::new(LeadAgent::new(
            vec![
                ScriptedPlan::Outcome(planned_intent(
                    "ask a worker",
                    two_worker_plan("ask a worker", Topology::ManagerWorker),
                )),
                ScriptedPlan::ProveFirstConfirmed { goal_id: goal.id },
            ],
            vec![
                execute_output("alpha initial", vec![]),
                execute_output("beta initial", vec![]),
                execute_output("beta asked", vec![]),
            ],
            vec![
                dhi_core::LeadDecision::AskWorker {
                    worker_key: "beta".to_string(),
                    prompt: "check the edge case".to_string(),
                },
                dhi_core::LeadDecision::Final {
                    summary: "asked lead summary".to_string(),
                    detail: String::new(),
                    evidence: "asked beta".to_string(),
                    confidence: "inferred".to_string(),
                },
            ],
        ));
        let engine = Engine::new(store.clone(), agent.clone()).with_lead_runtime();

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Done);
        assert_eq!(agent.review_inputs().len(), 2);
    }

    #[tokio::test]
    async fn manager_lead_stop_returns_to_the_planner_without_failing_the_task() {
        let store = Arc::new(MemoryStore::new());
        let (task, _goal) = manager_task(&store, "stop the lead").await;
        let agent = Arc::new(LeadAgent::new(
            vec![
                ScriptedPlan::Outcome(planned_intent(
                    "stop the lead",
                    two_worker_plan("stop the lead", Topology::ManagerWorker),
                )),
                ScriptedPlan::Outcome(idle_plan()),
            ],
            vec![
                execute_output("alpha initial", vec![]),
                execute_output("beta initial", vec![]),
            ],
            vec![dhi_core::LeadDecision::Stop {
                reason: "the source is unavailable".to_string(),
            }],
        ));
        let engine = Engine::new(store.clone(), agent.clone())
            .with_lead_runtime()
            .with_heartbeat(Duration::from_millis(10))
            .with_max_idle_ticks(1);

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Timeout);
        assert_ne!(result.task.state, TaskState::Failed);
        let intents = store.intents(task.id);
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].state, IntentState::Stopped);
        let stopped = store
            .activities(task.id)
            .into_iter()
            .find(|activity| activity.kind == "team_stopped")
            .expect("team_stopped activity");
        assert_eq!(stopped.payload["reason"], "the source is unavailable");
        assert!(
            agent.plan_inputs().len() >= 2,
            "the planner is consulted again after a stop"
        );
    }

    #[tokio::test]
    async fn manager_lead_stop_counts_the_lead_call_against_capacity() {
        let store = Arc::new(MemoryStore::new());
        let (task, _goal) = manager_task(&store, "stop and charge the lead").await;
        GraphStore::set_task_capacity(
            store.as_ref(),
            task.id,
            dhi_core::TaskCapacity {
                max_model_calls: Some(1),
                ..dhi_core::TaskCapacity::default()
            },
        )
        .await
        .unwrap();
        let mut plan = two_worker_plan("stop and charge the lead", Topology::ManagerWorker);
        plan.subtasks.clear();
        let agent = Arc::new(
            LeadAgent::new(
                vec![ScriptedPlan::Outcome(planned_intent(
                    "stop and charge the lead",
                    plan,
                ))],
                vec![],
                vec![dhi_core::LeadDecision::Stop {
                    reason: "stop after one review".to_string(),
                }],
            )
            .recorded(),
        );
        let engine = Engine::new(store.clone(), agent)
            .with_lead_runtime()
            .with_heartbeat(Duration::from_millis(10))
            .with_max_idle_ticks(1);

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(
            result.task.state,
            TaskState::Paused,
            "the stopped lead's model call must reach the capacity ledger"
        );
        let paused = store
            .activities(task.id)
            .into_iter()
            .find(|activity| activity.kind == "capacity_exhausted")
            .expect("capacity_exhausted activity");
        assert_eq!(paused.payload["reason"], "model_calls");
        assert_eq!(paused.payload["model_calls"], 1);
    }

    // ---------- Cline Teams wiring ----------

    fn team_plan(worker_count: usize, subtasks: usize) -> IntentPlan {
        let mut plan = swarm_plan(worker_count);
        plan.subtasks.truncate(subtasks);
        plan.limits.max_rounds = 3;
        plan
    }

    #[tokio::test]
    async fn coordinator_tool_loop_records_one_fact_and_persists_team_state() {
        let store = Arc::new(MemoryStore::new());
        let task = GraphStore::create_task_with_pins(
            store.as_ref(),
            dhi_core::NewTask::new("run the team").with_topology(Some(Topology::Swarm)),
        )
        .await
        .unwrap();
        let goal = store.add_goal(task.id, task.goal.clone()).await.unwrap();
        let plan = team_plan(2, 0);
        let team_store: Arc<dyn TeamStore> = Arc::new(dhi_core::team::MemoryTeamStore::new());
        let board_task = Uuid::new_v4();
        let outcome_id = Uuid::new_v4();
        let coordinator = CoordinatorScript {
            calls: vec![
                (
                    "team_task",
                    json!({
                        "operation": "create",
                        "id": board_task,
                        "title": "collect evidence",
                        "description": "deep dive",
                        "assignee": "w0"
                    }),
                ),
                (
                    "team_run_task",
                    json!({
                        "task_id": board_task,
                        "agent_id": "w0",
                        "mode": "sync",
                        "message": "run it"
                    }),
                ),
                (
                    "team_send_message",
                    json!({
                        "to": "w0",
                        "subject": "handoff",
                        "body": "please collect",
                        "task_id": board_task
                    }),
                ),
                (
                    "team_broadcast",
                    json!({"subject": "status", "body": "one card is running"}),
                ),
                (
                    "team_mission_log",
                    json!({
                        "kind": "progress",
                        "summary": "coordinator started the board",
                        "evidence": "created one card"
                    }),
                ),
                (
                    "team_create_outcome",
                    json!({
                        "id": outcome_id,
                        "title": "report",
                        "required_sections": ["summary", "evidence"]
                    }),
                ),
                (
                    "team_attach_outcome_fragment",
                    json!({
                        "outcome_id": outcome_id,
                        "section": "summary",
                        "content": "the team collected evidence"
                    }),
                ),
                (
                    "team_attach_outcome_fragment",
                    json!({
                        "outcome_id": outcome_id,
                        "section": "evidence",
                        "content": "w0 completed collect evidence"
                    }),
                ),
            ],
            summary: "coordinator final summary",
            record: true,
        };
        let teammate = TeammateScript {
            calls: vec![
                (
                    "team_send_message",
                    json!({"to": "lead", "subject": "finding", "body": "the parser is clean"}),
                ),
                (
                    "team_mission_log",
                    json!({
                        "kind": "progress",
                        "summary": "teammate note",
                        "evidence": "read the parser"
                    }),
                ),
            ],
            summary: "teammate finding",
            record: true,
        };
        let agent = Arc::new(CoordinatorAgent::new(
            vec![
                ScriptedPlan::Outcome(planned_intent("run the team", plan)),
                ScriptedPlan::ProveFirstConfirmed { goal_id: goal.id },
            ],
            coordinator,
            teammate,
        ));
        let engine = Engine::new(store.clone(), agent.clone()).with_team_store(team_store.clone());

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Done);
        let facts = store.facts(task.id);
        let confirmed: Vec<&dhi_core::Fact> = facts
            .iter()
            .filter(|fact| fact.state == FactState::Confirmed)
            .collect();
        assert_eq!(confirmed.len(), 1, "one coordinator fact for the intent");
        assert_eq!(confirmed[0].claim, "coordinator final summary");
        assert_eq!(store.intents(task.id).len(), 1);
        assert_eq!(store.intents(task.id)[0].state, IntentState::Done);

        let tasks = team_store.team_tasks(task.id).await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].status, dhi_core::team::TeamTaskStatus::Completed);
        let lead_sent = team_store.team_sent(task.id, "lead").await.unwrap();
        assert_eq!(lead_sent.len(), 2, "direct handoff plus broadcast");
        assert_eq!(lead_sent[0].subject, "handoff");
        let teammate_sent = team_store.team_sent(task.id, "w0").await.unwrap();
        assert_eq!(teammate_sent.len(), 1);
        assert_eq!(teammate_sent[0].subject, "finding");
        assert!(
            !team_store
                .team_mission_log(task.id)
                .await
                .unwrap()
                .is_empty()
        );
        let runs = team_store.team_runs(task.id).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, dhi_core::team::TeamRunStatus::Completed);
        let outcomes = team_store.team_outcomes(task.id).await.unwrap();
        assert_eq!(outcomes.len(), 1);
        let fragments = team_store
            .team_outcome_fragments(task.id, Some(outcome_id))
            .await
            .unwrap();
        assert_eq!(fragments.len(), 2);

        let agent_runs = store.agent_runs(task.id);
        assert!(
            agent_runs
                .iter()
                .any(|run| run.worker_key.as_deref() == Some("coordinator")),
            "the coordinator call is recorded under its own worker key"
        );
        assert!(
            agent_runs
                .iter()
                .any(|run| run.worker_key.as_deref() == Some("w0")),
            "the teammate call is recorded"
        );
        assert_eq!(
            agent_runs
                .iter()
                .filter(|run| run.worker_key.as_deref() == Some("coordinator"))
                .count(),
            1,
            "one coordinator completion call"
        );
        let coordinator_inputs = agent.coordinator_inputs();
        assert_eq!(coordinator_inputs.len(), 1);
        assert_eq!(coordinator_inputs[0].intent, "swarm");
        assert!(
            coordinator_inputs[0]
                .context
                .iter()
                .any(|line| line.contains("Teammate cap: 8")),
            "the coordinator sees its cap: {:?}",
            coordinator_inputs[0].context
        );
        let teammate_inputs = agent.teammate_inputs();
        assert_eq!(teammate_inputs.len(), 1);
        assert_eq!(teammate_inputs[0].agent_id.as_deref(), Some("w0"));
        assert!(
            teammate_inputs[0]
                .task
                .as_deref()
                .is_some_and(|task| task.contains("collect evidence")),
            "the teammate sees its assigned task: {:?}",
            teammate_inputs[0].task
        );
    }

    #[tokio::test]
    async fn coordinator_fact_with_zero_worker_results_is_valid() {
        let store = Arc::new(MemoryStore::new());
        let task = GraphStore::create_task_with_pins(
            store.as_ref(),
            dhi_core::NewTask::new("finalize without workers").with_topology(Some(Topology::Swarm)),
        )
        .await
        .unwrap();
        let goal = store.add_goal(task.id, task.goal.clone()).await.unwrap();
        let plan = team_plan(2, 0);
        let team_store: Arc<dyn TeamStore> = Arc::new(dhi_core::team::MemoryTeamStore::new());
        let agent = Arc::new(CoordinatorAgent::new(
            vec![
                ScriptedPlan::Outcome(planned_intent("finalize without workers", plan)),
                ScriptedPlan::ProveFirstConfirmed { goal_id: goal.id },
            ],
            CoordinatorScript {
                calls: Vec::new(),
                summary: "zero-worker coordinator fact",
                record: true,
            },
            TeammateScript {
                calls: Vec::new(),
                summary: "unused",
                record: false,
            },
        ));
        let engine = Engine::new(store.clone(), agent).with_team_store(team_store.clone());

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Done);
        assert!(
            store
                .facts(task.id)
                .iter()
                .any(|fact| fact.claim == "zero-worker coordinator fact"),
            "a coordinator record_fact with no worker results is valid"
        );
        assert!(
            team_store.team_runs(task.id).await.unwrap().is_empty(),
            "no teammate ran"
        );
        assert_eq!(
            team_store.team_members(task.id).await.unwrap().len(),
            3,
            "the coordinator plus two seeded teammates exist"
        );
    }

    #[tokio::test]
    async fn legacy_peer_swarm_rejects_a_zero_seed_plan() {
        let store = Arc::new(MemoryStore::new());
        let task = GraphStore::create_task_with_pins(
            store.as_ref(),
            dhi_core::NewTask::new("zero seed peer swarm").with_topology(Some(Topology::Swarm)),
        )
        .await
        .unwrap();
        let mut plan = swarm_plan(2);
        plan.subtasks.clear();
        let agent = Arc::new(ScriptedAgent::new(
            vec![
                planned_intent("zero seed peer swarm", plan),
                PlanOutcome::NoPlan {
                    reason: "nothing left".to_string(),
                },
            ],
            vec![],
        ));
        let engine = Engine::new(store.clone(), agent);

        let err = engine
            .run_task(task.id)
            .await
            .expect_err("the legacy peer swarm cannot run without a seed");
        assert!(err.to_string().contains("seed subtask"), "{err}");
        assert_eq!(
            store.get_task(task.id).await.unwrap().unwrap().state,
            TaskState::Failed
        );
        assert!(
            store
                .facts(task.id)
                .iter()
                .all(|fact| fact.state != FactState::Confirmed),
            "a zero-worker peer swarm must not produce a fact"
        );
    }

    #[tokio::test]
    async fn coordinator_stop_returns_to_the_planner_without_a_fact() {
        let store = Arc::new(MemoryStore::new());
        let task = GraphStore::create_task_with_pins(
            store.as_ref(),
            dhi_core::NewTask::new("stop the team").with_topology(Some(Topology::Swarm)),
        )
        .await
        .unwrap();
        let plan = team_plan(2, 0);
        let team_store: Arc<dyn TeamStore> = Arc::new(dhi_core::team::MemoryTeamStore::new());
        let agent = Arc::new(CoordinatorAgent::new(
            vec![
                ScriptedPlan::Outcome(planned_intent("stop the team", plan)),
                ScriptedPlan::Outcome(idle_plan()),
            ],
            CoordinatorScript {
                calls: vec![("team_stop", json!({"reason": "the team is blocked"}))],
                summary: "this fact must never be recorded",
                record: true,
            },
            TeammateScript {
                calls: Vec::new(),
                summary: "unused",
                record: false,
            },
        ));
        let engine = Engine::new(store.clone(), agent)
            .with_team_store(team_store)
            .with_heartbeat(Duration::from_millis(10))
            .with_max_idle_ticks(1);

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(result.task.state, TaskState::Timeout);
        assert_eq!(store.intents(task.id)[0].state, IntentState::Stopped);
        assert!(
            store
                .facts(task.id)
                .iter()
                .all(|fact| fact.claim != "this fact must never be recorded"),
            "a coordinator stop records no fact"
        );
        let stopped = store
            .activities(task.id)
            .into_iter()
            .find(|activity| activity.kind == "team_stopped")
            .expect("team_stopped activity");
        assert_eq!(stopped.payload["reason"], "the team is blocked");
        assert!(
            store
                .agent_runs(task.id)
                .iter()
                .any(|run| run.worker_key.as_deref() == Some("coordinator")),
            "the coordinator call is still recorded"
        );
    }

    #[tokio::test]
    async fn coordinator_stop_counts_the_call_against_capacity() {
        let store = Arc::new(MemoryStore::new());
        let task = GraphStore::create_task_with_pins(
            store.as_ref(),
            dhi_core::NewTask::new("stop and charge the team").with_topology(Some(Topology::Swarm)),
        )
        .await
        .unwrap();
        GraphStore::set_task_capacity(
            store.as_ref(),
            task.id,
            dhi_core::TaskCapacity {
                max_model_calls: Some(1),
                ..dhi_core::TaskCapacity::default()
            },
        )
        .await
        .unwrap();
        let plan = team_plan(2, 0);
        let team_store: Arc<dyn TeamStore> = Arc::new(dhi_core::team::MemoryTeamStore::new());
        let agent = Arc::new(CoordinatorAgent::new(
            vec![ScriptedPlan::Outcome(planned_intent(
                "stop and charge the team",
                plan,
            ))],
            CoordinatorScript {
                calls: vec![("team_stop", json!({"reason": "stop after one turn"}))],
                summary: "ignored",
                record: true,
            },
            TeammateScript {
                calls: Vec::new(),
                summary: "unused",
                record: false,
            },
        ));
        let engine = Engine::new(store.clone(), agent)
            .with_team_store(team_store)
            .with_heartbeat(Duration::from_millis(10))
            .with_max_idle_ticks(1);

        let result = engine.run_task(task.id).await.unwrap();

        assert_eq!(
            result.task.state,
            TaskState::Paused,
            "the stopped coordinator's model call must reach the capacity ledger"
        );
        let paused = store
            .activities(task.id)
            .into_iter()
            .find(|activity| activity.kind == "capacity_exhausted")
            .expect("capacity_exhausted activity");
        assert_eq!(paused.payload["reason"], "model_calls");
        assert_eq!(paused.payload["model_calls"], 1);
    }

    // ---------- scale gate ----------

    struct FakeScaleGate {
        requests: Mutex<Vec<dhi_core::ScaleApprovalRequest>>,
        status: dhi_core::ScaleApprovalStatus,
    }

    impl FakeScaleGate {
        fn new(status: dhi_core::ScaleApprovalStatus) -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
                status,
            }
        }

        fn requests(&self) -> Vec<dhi_core::ScaleApprovalRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl dhi_core::ScaleApprovalGate for FakeScaleGate {
        async fn request_scale_approval(
            &self,
            request: dhi_core::ScaleApprovalRequest,
        ) -> anyhow::Result<dhi_core::ScaleApproval> {
            self.requests.lock().unwrap().push(request.clone());
            Ok(dhi_core::ScaleApproval {
                id: Uuid::new_v4(),
                task_id: request.task_id,
                team_id: request.team_id,
                requested_workers: request.requested_workers,
                current_cap: request.current_cap,
                reason: request.reason,
                status: dhi_core::ScaleApprovalStatus::Pending,
                created_at: Utc::now(),
                expires_at: Utc::now() + chrono::Duration::minutes(1),
                decided_at: None,
            })
        }

        async fn wait_for_scale_approval(
            &self,
            _approval_id: Uuid,
        ) -> anyhow::Result<dhi_core::ScaleApproval> {
            let request = self
                .requests
                .lock()
                .unwrap()
                .last()
                .cloned()
                .expect("the gate was asked first");
            Ok(dhi_core::ScaleApproval {
                id: Uuid::new_v4(),
                task_id: request.task_id,
                team_id: request.team_id,
                requested_workers: request.requested_workers,
                current_cap: request.current_cap,
                reason: request.reason,
                status: self.status,
                created_at: Utc::now(),
                expires_at: Utc::now() + chrono::Duration::minutes(1),
                decided_at: None,
            })
        }
    }

    fn auto_swarm_task(workers: Option<usize>) -> Task {
        let mut task = Task::new("gate the swarm");
        task.topology = Some(Topology::Swarm);
        task.swarm_workers = workers;
        task
    }

    #[test]
    fn effective_teammate_cap_matches_the_swarm_contract() {
        let auto_task = auto_swarm_task(None);
        assert_eq!(
            Engine::effective_teammate_cap(&auto_task, &swarm_plan(3)),
            dhi_core::SWARM_AUTO_WORKERS,
            "Auto may still grow to 8 even when the plan seeds fewer"
        );
        assert_eq!(
            Engine::effective_teammate_cap(&auto_task, &swarm_plan(10)),
            10,
            "a granted Auto plan keeps its final size"
        );
        assert_eq!(
            Engine::effective_teammate_cap(&auto_task, &swarm_plan(100)),
            dhi_core::SWARM_MAX_WORKERS
        );

        let fixed_task = auto_swarm_task(Some(12));
        assert_eq!(
            Engine::effective_teammate_cap(&fixed_task, &swarm_plan(12)),
            12
        );
    }

    #[test]
    fn team_config_never_exceeds_the_effective_teammate_cap() {
        let task_id = Uuid::new_v4();
        let intent_id = Uuid::new_v4();
        let plan = swarm_plan(12);

        let fixed = Engine::team_run_config(task_id, intent_id, &plan, 12);
        assert_eq!(fixed.max_teammates, 12);
        assert_eq!(fixed.teammates.len(), 12);

        let auto = Engine::team_run_config(task_id, intent_id, &plan, 8);
        assert_eq!(auto.max_teammates, dhi_core::SWARM_AUTO_WORKERS);
        assert_eq!(auto.teammates.len(), dhi_core::SWARM_AUTO_WORKERS);

        let granted = Engine::team_run_config(task_id, intent_id, &plan, 10);
        assert_eq!(granted.max_teammates, 10);
        assert_eq!(granted.teammates.len(), 10);

        let hard_cap = Engine::team_run_config(task_id, intent_id, &plan, 100);
        assert_eq!(hard_cap.max_teammates, dhi_core::SWARM_MAX_WORKERS);
    }

    #[tokio::test]
    async fn fixed_swarm_never_asks_for_scale_approval() {
        let gate = Arc::new(FakeScaleGate::new(dhi_core::ScaleApprovalStatus::Granted));
        let engine = Engine::new(Arc::new(MemoryStore::new()), Arc::new(MockAgentRuntime))
            .with_scale_gate(gate.clone());
        let plan = effective_team_plan(swarm_plan(10), &auto_swarm_task(Some(10)));

        let effective = engine
            .apply_scale_gate(&auto_swarm_task(Some(10)), &plan, None)
            .await
            .unwrap();

        assert_eq!(effective.workers.len(), 10);
        assert_eq!(effective.limits.max_parallel, 10);
        assert!(gate.requests().is_empty());
    }

    #[tokio::test]
    async fn auto_swarm_of_eight_never_asks_for_scale_approval() {
        let gate = Arc::new(FakeScaleGate::new(dhi_core::ScaleApprovalStatus::Granted));
        let engine = Engine::new(Arc::new(MemoryStore::new()), Arc::new(MockAgentRuntime))
            .with_scale_gate(gate.clone());
        let task = auto_swarm_task(None);
        let plan = effective_team_plan(swarm_plan(8), &task);

        let effective = engine.apply_scale_gate(&task, &plan, None).await.unwrap();

        assert_eq!(effective.workers.len(), 8);
        assert_eq!(effective.limits.max_parallel, 8);
        assert!(gate.requests().is_empty());
    }

    #[tokio::test]
    async fn auto_swarm_above_eight_uses_a_granted_cap() {
        let gate = Arc::new(FakeScaleGate::new(dhi_core::ScaleApprovalStatus::Granted));
        let engine = Engine::new(Arc::new(MemoryStore::new()), Arc::new(MockAgentRuntime))
            .with_scale_gate(gate.clone());
        let task = auto_swarm_task(None);
        let plan = effective_team_plan(swarm_plan(10), &task);

        let effective = engine.apply_scale_gate(&task, &plan, None).await.unwrap();

        assert_eq!(effective.workers.len(), 10);
        assert_eq!(effective.limits.max_parallel, 10);
        let requests = gate.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].requested_workers, 10);
        assert_eq!(requests[0].current_cap, dhi_core::SWARM_AUTO_WORKERS);
        let config =
            Engine::team_run_config(task.id, Uuid::new_v4(), &effective, effective.workers.len());
        assert_eq!(config.max_teammates, 10);
    }

    #[tokio::test]
    async fn auto_swarm_above_eight_is_capped_when_denied() {
        let gate = Arc::new(FakeScaleGate::new(dhi_core::ScaleApprovalStatus::Denied));
        let engine = Engine::new(Arc::new(MemoryStore::new()), Arc::new(MockAgentRuntime))
            .with_scale_gate(gate.clone());
        let task = auto_swarm_task(None);
        let plan = effective_team_plan(swarm_plan(10), &task);

        let effective = engine.apply_scale_gate(&task, &plan, None).await.unwrap();

        assert_eq!(effective.workers.len(), dhi_core::SWARM_AUTO_WORKERS);
        assert_eq!(effective.limits.max_parallel, dhi_core::SWARM_AUTO_WORKERS);
        assert_eq!(gate.requests().len(), 1);
        let config =
            Engine::team_run_config(task.id, Uuid::new_v4(), &effective, effective.workers.len());
        assert_eq!(config.max_teammates, dhi_core::SWARM_AUTO_WORKERS);
    }

    #[tokio::test]
    async fn auto_swarm_above_eight_is_capped_when_expired() {
        let gate = Arc::new(FakeScaleGate::new(dhi_core::ScaleApprovalStatus::Expired));
        let engine = Engine::new(Arc::new(MemoryStore::new()), Arc::new(MockAgentRuntime))
            .with_scale_gate(gate);
        let task = auto_swarm_task(None);
        let plan = effective_team_plan(swarm_plan(12), &task);

        let effective = engine.apply_scale_gate(&task, &plan, None).await.unwrap();

        assert_eq!(effective.workers.len(), dhi_core::SWARM_AUTO_WORKERS);
        assert_eq!(effective.limits.max_parallel, dhi_core::SWARM_AUTO_WORKERS);
    }

    struct NeverDecidedGate;

    #[async_trait::async_trait]
    impl dhi_core::ScaleApprovalGate for NeverDecidedGate {
        async fn request_scale_approval(
            &self,
            request: dhi_core::ScaleApprovalRequest,
        ) -> anyhow::Result<dhi_core::ScaleApproval> {
            Ok(dhi_core::ScaleApproval {
                id: Uuid::new_v4(),
                task_id: request.task_id,
                team_id: request.team_id,
                requested_workers: request.requested_workers,
                current_cap: request.current_cap,
                reason: request.reason,
                status: dhi_core::ScaleApprovalStatus::Pending,
                created_at: Utc::now(),
                expires_at: Utc::now() + chrono::Duration::minutes(1),
                decided_at: None,
            })
        }

        async fn wait_for_scale_approval(
            &self,
            _approval_id: Uuid,
        ) -> anyhow::Result<dhi_core::ScaleApproval> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn scale_gate_wait_honors_the_stop_flag() {
        let engine = Engine::new(Arc::new(MemoryStore::new()), Arc::new(MockAgentRuntime))
            .with_scale_gate(Arc::new(NeverDecidedGate));
        let task = auto_swarm_task(None);
        let plan = effective_team_plan(swarm_plan(10), &task);
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        let err = engine
            .apply_scale_gate(&task, &plan, Some(&stop))
            .await
            .expect_err("stop must interrupt the gate wait");

        assert!(
            err.to_string().contains("stop requested"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn auto_swarm_requires_at_least_two_workers() {
        for workers in [0, 1] {
            let err = enforce_swarm_worker_count(None, &swarm_plan(workers))
                .expect_err("an Auto swarm below the minimum must be rejected");
            assert!(err.contains("at least 2"), "{err}");
        }
        assert!(enforce_swarm_worker_count(None, &swarm_plan(2)).is_ok());
    }

    #[test]
    fn fixed_swarm_accepts_zero_to_n_subtasks() {
        for workers in [2, 8, 64] {
            let mut plan = swarm_plan(workers);
            plan.subtasks.clear();
            assert!(
                enforce_swarm_worker_count(Some(workers), &plan).is_ok(),
                "a {workers}-worker team may start with no subtasks"
            );
            plan.subtasks = swarm_plan(workers)
                .subtasks
                .into_iter()
                .take(workers / 2)
                .collect();
            assert!(
                enforce_swarm_worker_count(Some(workers), &plan).is_ok(),
                "a {workers}-worker team may start with partial subtasks"
            );
        }
    }
}
