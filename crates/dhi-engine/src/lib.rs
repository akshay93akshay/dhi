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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::Utc;
use dhi_agent::{AgentCall, AgentCallRecord, AgentRuntime, ExecuteInput, PlanInput, PlanOutcome};
use dhi_core::{
    AgentRun, ApprovalDecision, Fact, FactState, Goal, GoalState, GraphStore, GuardStore,
    IntentState, Task, TaskState, ToolCallOutcome, ToolCallRequest,
};
use dhi_guard::ToolExecutor;
use tokio::sync::Notify;
use tracing::warn;
use uuid::Uuid;

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

/// The `agent_runs` row one recorded call becomes.
///
/// The row id is the record's `run_id` and the run it belongs to is the same
/// value, so `call_index` is 0. There is no checkpoint. `variables` is empty
/// because the engine keeps no rendered variables; the rendered prompt is
/// stored in full instead.
fn agent_run<T>(
    task_id: Uuid,
    intent_id: Option<Uuid>,
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
    /// it was claimed for. `prompt` and `response` are the texts the runtime
    /// returned. A failed save is logged and swallowed: usage accounting is
    /// never a reason to fail a plan or lose a worker's fact.
    async fn record_agent_calls<T>(
        &self,
        task_id: Uuid,
        intent_id: Option<Uuid>,
        call: &AgentCall<T>,
    ) {
        for record in &call.meta.calls {
            let run = agent_run(task_id, intent_id, record, call);
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
    /// path (provider error, bound exhaustion, give_up, round cap) goes
    /// through here, so the dashboard can never show a dead loop as
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

        let mut rounds = 0;
        // Consecutive spent rounds without a new confirmed fact. The baseline
        // is the task's current confirmed count, so a reopened task does not
        // count facts from its previous epoch as new progress.
        let mut no_progress_rounds = 0;
        let mut last_confirmed = confirmed_fact_count(&self.store.list_facts(task_id).await?);
        // Consecutive idle planner ticks. Reset to zero on every real
        // intent, so only a genuinely stuck task ever exhausts this budget.
        let mut idle_ticks = 0;
        loop {
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
                let plan = match self
                    .agent
                    .plan(PlanInput {
                        task_id,
                        goal: task.goal.clone(),
                        bound: task.bound,
                        goals,
                        facts: facts.clone(),
                        open_intents: 0,
                        planner_profile_id: task.pinned_planner_profile_id,
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
                // warns and the plan still applies.
                self.record_agent_calls(task_id, None, &plan).await;
                let plan = plan.output;

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
                    PlanOutcome::AddIntent { description } => {
                        if rounds >= self.max_rounds {
                            let reason =
                                format!("engine exceeded max rounds ({})", self.max_rounds);
                            self.fail_task(task_id, &mut task, reason.clone(), "engine")
                                .await?;
                            anyhow::bail!("{reason}");
                        }
                        // The bound is the task's evidence budget: once the
                        // confirmed facts reach it, more intents are not
                        // allowed. A proof of an existing fact is still fine.
                        //
                        // A reopened task starts a new evidence epoch: the
                        // facts that proved the old goals must not exhaust the
                        // budget for the new goal. Only facts recorded after
                        // the latest `task_reopened` row count. A task that
                        // was never reopened has no such row, so every
                        // confirmed fact counts as before.
                        let reopened_at = self
                            .store
                            .list_activity(task_id, Some("task_reopened"))
                            .await?
                            .last()
                            .map(|activity| activity.created_at);
                        let confirmed = self
                            .store
                            .list_facts(task_id)
                            .await?
                            .iter()
                            .filter(|fact| fact.state == FactState::Confirmed)
                            .filter(|fact| reopened_at.is_none_or(|at| fact.created_at > at))
                            .count();
                        if confirmed >= task.bound.max(1) as usize {
                            let reason = format!(
                                "evidence bound exhausted: {confirmed} confirmed facts for bound {}",
                                task.bound
                            );
                            self.fail_task(task_id, &mut task, reason.clone(), "bound")
                                .await?;
                            anyhow::bail!("{reason}");
                        }

                        self.store
                            .add_intent(task_id, task.origin_fact_id, description)
                            .await?;
                        // Only real work spends budget: a new intent is work.
                        rounds += 1;
                        idle_ticks = 0;
                        self.log_activity(
                            task_id,
                            "intents_added",
                            "planner added one intent".to_string(),
                            serde_json::json!({"round": rounds}),
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
                        if rounds >= self.max_rounds {
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

            if rounds >= self.max_rounds {
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
                    self.fail_task(
                        task_id,
                        &mut task,
                        format!("worker call failed: {err}"),
                        "worker",
                    )
                    .await?;
                    return Err(err);
                }
            };
            // The row names the intent the worker was claimed for. A failed
            // save warns and the worker's fact is still written.
            self.record_agent_calls(task_id, Some(claimed.id), &executed)
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
            if !output.tool_calls.is_empty() {
                self.run_tool_calls(task_id, claimed.id, &output.tool_calls)
                    .await?;
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
            let fact = self
                .store
                .record_fact(task_id, output.fact.summary.clone(), provenance)
                .await?;

            self.store
                .finish_intent(claimed.id, IntentState::Done)
                .await?;
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
                    self.log_activity(
                        task_id,
                        "tool_allowed",
                        format!("tool {tool} executed"),
                        serde_json::json!({
                            "intent_id": intent_id,
                            "tool": tool,
                            "output": output,
                        }),
                    )
                    .await?;
                }
                ToolCallOutcome::Denied { reason } => {
                    // The worker continues in the same task loop. The denial
                    // is recorded, but the intent is not blocked and the task
                    // does not stop.
                    self.log_activity(
                        task_id,
                        "tool_denied",
                        format!("tool {tool} denied: {reason}"),
                        serde_json::json!({
                            "intent_id": intent_id,
                            "tool": tool,
                            "reason": reason,
                        }),
                    )
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
    use dhi_agent::{AgentCallRecord, MockAgentRuntime, PlanOutcome, Role};
    use dhi_core::{
        Activity, AgentRole, Approval, ApprovalStatus, FactId, FactState, Goal, GoalId, GoalState,
        GuardVerdict, Intent, IntentId, NewSideQuestionRequest, ReopenOutcome, ResultRecord,
        ResultStatus, SideQuestionRequest, SideQuestionSession, SideQuestionThread, TaskId,
    };
    use dhi_guard::Policy;
    use dhi_guard::executor::{GuardedExecutor, ToolBackend, ToolExecutor};
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
            }
        }
    }

    #[async_trait::async_trait]
    impl AgentRuntime for ScriptedAgent {
        async fn plan(
            &self,
            input: PlanInput,
        ) -> anyhow::Result<dhi_agent::AgentCall<PlanOutcome>> {
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

    // ---------- existing behavior ----------

    #[tokio::test]
    async fn mock_engine_completes_a_task() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("forced mate", 4);
        let engine = Engine::new(store.clone(), Arc::new(MockAgentRuntime));
        let result = engine.run_task(task.id).await.unwrap();
        assert_eq!(result.task.state, TaskState::Done);
        assert!(!store.facts(task.id).is_empty());
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
        let task = store.seed_task("stop before work", 3);
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
        let task = store.seed_task("stop during plan", 3);
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
    async fn planner_give_up_fails_the_task_with_a_visible_reason() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("impossible task", 3);
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
    async fn evidence_bound_stops_another_intent_and_marks_the_task_failed() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("bound task", 1);
        let agent = Arc::new(ScriptedAgent::new(
            vec![
                single_intent_plan("first intent"),
                single_intent_plan("second intent"),
            ],
            vec![execute_output("the first fact", vec![])],
        ));
        let engine = Engine::new(store.clone(), agent);
        let err = engine
            .run_task(task.id)
            .await
            .expect_err("the bound refuses a second intent");
        assert!(err.to_string().contains("bound"), "{err}");

        let row = store.get_task(task.id).await.unwrap().unwrap();
        assert_eq!(row.state, TaskState::Failed);
        let failed = store
            .activities(task.id)
            .into_iter()
            .find(|a| a.kind == "task_failed")
            .expect("failure row");
        assert_eq!(failed.payload["source"], "bound");
    }

    #[tokio::test]
    async fn reopened_task_starts_a_new_evidence_epoch() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("reopened bound", 1);
        // The old fact proved the old goal. It must not spend the new goal's
        // bound after the human reopens the task.
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
            "the old fact must not exhaust the new epoch's bound"
        );
        assert!(
            !activities
                .iter()
                .any(|a| a.kind == "task_failed" && a.payload["source"] == "bound"),
            "the bound must not reject the first new intent"
        );
    }

    #[tokio::test]
    async fn planner_failure_marks_the_task_failed_and_records_it() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("provider failure", 2);
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
        let task = store.seed_task("record usage", 2);
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
            assert_eq!(row.call_index, 0);
            assert_eq!(row.id, row.run_id);
        }
    }

    #[tokio::test]
    async fn worker_usage_is_recorded() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("record usage", 2);
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

        let intent_id = row.intent_id.expect("the worker row names its intent");
        let intents = store.intents(task.id);
        assert!(
            intents.iter().any(|i| i.id == intent_id),
            "the row's intent exists on the task"
        );
    }

    #[tokio::test]
    async fn agent_run_has_tokens_and_status() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("record usage", 2);
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
        let task = store.seed_task("forced mate", 4);
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
        let task = store.seed_task("goal", 1);
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
        let task = store.seed_task("open a game", 4);
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
        let task = store.seed_task("wipe the disk", 2);
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
        let task = store.seed_task("only record the fact", 2);
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
        let task = store.seed_task("never pause on pending", 3);
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
        let task = store.seed_task("deploy the release", 3);
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
        let task = store.seed_task("finish after approval", 3);
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
        let task = store.seed_task("wait without pausing", 3);
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
        let task = store.seed_task("deny and continue", 3);
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
        let task = store.seed_task("deploy without a reviewer", 3);
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
        let task = store.seed_task("sweep the wait", 3);
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
        let task = store.seed_task("decide only", 3);
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
        let task = store.seed_task("finish once", 2);
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
        let task_a = store.seed_task("deploy A", 3);
        let task_b = store.seed_task("deploy B", 3);

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
        let task = store.seed_task("deploy without a reviewer", 3);
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
        let task = store.seed_task("race the finish", 2);
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
        let task = store.seed_task("orphaned pause", 2);
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
        let task = store.seed_task("waiting in time", 2);
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
        let task = store.seed_task("wait for work", 2);
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
        let task = store.seed_task("round cap", 100);
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

    #[tokio::test]
    async fn no_progress_guard_does_not_stop_a_task_that_confirms_facts() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("steady progress", 100);
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
        async fn create_task(&self, goal: String, bound: i32) -> anyhow::Result<Task> {
            self.inner.create_task(goal, bound).await
        }

        async fn create_task_with_pins(
            &self,
            goal: String,
            bound: i32,
            planner_pin: Option<i64>,
            worker_pin: Option<i64>,
        ) -> anyhow::Result<Task> {
            self.inner
                .create_task_with_pins(goal, bound, planner_pin, worker_pin)
                .await
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
        let task = inner.seed_task("planner spin", 100);
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
        let task = store.seed_task("planner that never decides", 2);
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
        let task = store.seed_task("open a game", 4);
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
        let task = store.seed_task("text cannot finish", 2);
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
        let task = store.seed_task("worker truth is a fact", 2);
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
        let task = store.seed_task("record evidence", 2);
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
        let task = store.seed_task("record detail", 2);
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
        let task = store.seed_task("origin cannot prove", 2);
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
        let task = store.seed_task("prove one of two", 2);
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
        let task = store.seed_task("prove the only goal", 2);
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
        let task = store.seed_task("task A", 2);
        let other = store.seed_task("task B", 2);
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

    // ---------- heartbeat ----------

    #[tokio::test]
    async fn heartbeat_wakes_idle_engine() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("wait then finish", 2);
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
}
