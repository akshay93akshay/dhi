//! Deterministic in-memory store for engine tests.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;
use dhi_core::{
    Activity, AgentRun, Approval, ApprovalDecision, ApprovalId, ApprovalStatus, CoreError, Fact,
    FactId, FactState, Goal, GoalState, GraphStore, GuardDecision, GuardDecisionId, GuardStore,
    GuardVerdict, Intent, IntentId, IntentPlan, IntentState, NewSideQuestionRequest, ReopenOutcome,
    ResultRecord, ResultStatus, RunId, SideQuestionRequest, SideQuestionSession,
    SideQuestionSessionStatus, SideQuestionStatus, SideQuestionThread, SwarmMessageRecord, Task,
    TaskCapacity, TaskId, TaskState, ToolCall, ToolCallId,
};
use uuid::Uuid;

pub mod fixtures;

#[derive(Default)]
struct Inner {
    tasks: HashMap<TaskId, Task>,
    goals: HashMap<Uuid, Goal>,
    facts: HashMap<FactId, Fact>,
    /// The fact recorded for each intent. The store enforces at most one.
    fact_intents: HashMap<IntentId, FactId>,
    intents: HashMap<IntentId, Intent>,
    /// DHI+ plans by intent. Kept after completion as audit data.
    intent_plans: HashMap<IntentId, IntentPlan>,
    results: Vec<ResultRecord>,
    activities: Vec<Activity>,
    tool_calls: HashMap<ToolCallId, ToolCall>,
    guard_decisions: HashMap<GuardDecisionId, GuardDecision>,
    approvals: HashMap<ApprovalId, Approval>,
    /// `agent_runs` rows by row id: one per recorded completion call.
    agent_runs: HashMap<RunId, AgentRun>,
    side_sessions: HashMap<Uuid, SideQuestionSession>,
    side_requests: Vec<SideQuestionRequest>,
    /// Typed messages broadcast by swarm runs, append-only.
    swarm_messages: Vec<SwarmMessageRecord>,
    next_activity: i64,
    next_swarm_message: i64,
}

#[derive(Clone, Default)]
pub struct MemoryStore {
    inner: Arc<Mutex<Inner>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn seed_task(&self, goal: impl Into<String>) -> Task {
        let task = Task::new(goal);
        let origin = task.origin_fact();
        let mut inner = self.inner.lock().unwrap();
        inner.facts.insert(origin.id, origin);
        inner.tasks.insert(task.id, task.clone());
        task
    }

    /// Insert an open intent synchronously so fixtures can be built without a runtime.
    pub fn seed_intent(
        &self,
        task_id: TaskId,
        fact_id: FactId,
        description: impl Into<String>,
    ) -> Intent {
        let intent = Intent::new(task_id, fact_id, description)
            .expect("seeded intent requires a non-nil fact id");
        self.inner
            .lock()
            .expect("memory store mutex poisoned")
            .intents
            .insert(intent.id, intent.clone());
        intent
    }

    pub fn facts(&self, task_id: TaskId) -> Vec<Fact> {
        let inner = self.inner.lock().unwrap();
        inner
            .facts
            .values()
            .filter(|f| f.task_id == task_id)
            .cloned()
            .collect()
    }

    /// The one fact recorded for `intent_id`, if any.
    pub fn fact_for_intent(&self, intent_id: IntentId) -> Option<Fact> {
        let inner = self.inner.lock().unwrap();
        inner
            .fact_intents
            .get(&intent_id)
            .and_then(|fact_id| inner.facts.get(fact_id))
            .cloned()
    }

    pub fn goals(&self, task_id: TaskId) -> Vec<Goal> {
        let inner = self.inner.lock().unwrap();
        let mut goals: Vec<Goal> = inner
            .goals
            .values()
            .filter(|g| g.task_id == task_id)
            .cloned()
            .collect();
        goals.sort_by_key(|g| g.created_at);
        goals
    }

    pub fn intents(&self, task_id: TaskId) -> Vec<Intent> {
        let inner = self.inner.lock().unwrap();
        inner
            .intents
            .values()
            .filter(|i| i.task_id == task_id)
            .cloned()
            .collect()
    }

    pub fn activities(&self, task_id: TaskId) -> Vec<Activity> {
        let inner = self.inner.lock().unwrap();
        inner
            .activities
            .iter()
            .filter(|a| a.task_id == task_id)
            .cloned()
            .collect()
    }

    /// Every persisted swarm message of `task_id`, oldest first by id. The
    /// engine test reads the swarm audit log from here.
    pub fn swarm_messages(&self, task_id: TaskId) -> Vec<SwarmMessageRecord> {
        let inner = self.inner.lock().unwrap();
        let mut messages: Vec<SwarmMessageRecord> = inner
            .swarm_messages
            .iter()
            .filter(|message| message.task_id == task_id)
            .cloned()
            .collect();
        messages.sort_by_key(|message| message.id);
        messages
    }

    /// Every recorded `agent_runs` row of `task_id`, oldest first. Tests read
    /// usage from here; there is no other in-memory accessor.
    pub fn agent_runs(&self, task_id: TaskId) -> Vec<AgentRun> {
        let inner = self.inner.lock().unwrap();
        let mut runs: Vec<AgentRun> = inner
            .agent_runs
            .values()
            .filter(|r| r.task_id == task_id)
            .cloned()
            .collect();
        runs.sort_by_key(|r| (r.created_at, r.call_index));
        runs
    }

    /// Every recorded guarded tool call of `task_id`. Side-question tests use
    /// this to prove a question creates none.
    pub fn tool_calls(&self, task_id: TaskId) -> Vec<ToolCall> {
        let inner = self.inner.lock().unwrap();
        let mut calls: Vec<ToolCall> = inner
            .tool_calls
            .values()
            .filter(|call| call.task_id == task_id)
            .cloned()
            .collect();
        calls.sort_by_key(|call| (call.created_at, call.id));
        calls
    }

    /// Every stored guard decision. Side-question tests use this to prove a
    /// question evaluates no policy.
    pub fn guard_decisions(&self) -> Vec<GuardDecision> {
        let inner = self.inner.lock().unwrap();
        let mut decisions: Vec<GuardDecision> = inner.guard_decisions.values().cloned().collect();
        decisions.sort_by_key(|decision| (decision.created_at, decision.id));
        decisions
    }
}

#[async_trait]
impl GraphStore for MemoryStore {
    /// Creates a task the way the Postgres store does: the task row, its
    /// origin fact, and one `task_created` activity in a single step.
    async fn create_task(&self, goal: String) -> anyhow::Result<Task> {
        let task = Task::new(goal);
        let origin = task.origin_fact();
        let mut inner = self.inner.lock().unwrap();
        inner.facts.insert(origin.id, origin);
        inner.tasks.insert(task.id, task.clone());
        inner.next_activity += 1;
        let activity = Activity {
            id: inner.next_activity,
            task_id: task.id,
            kind: "task_created".to_string(),
            message: format!("Task created: {}", task.goal),
            payload: serde_json::json!({}),
            created_at: Utc::now(),
        };
        inner.activities.push(activity);
        Ok(task)
    }

    /// Memory twin of the pinned create: the pins ride on the task.
    async fn create_task_with_pins(
        &self,
        goal: String,
        planner_pin: Option<i64>,
        worker_pin: Option<i64>,
    ) -> anyhow::Result<Task> {
        let task = self.create_task(goal).await?;
        let mut inner = self.inner.lock().unwrap();
        let stored = inner
            .tasks
            .get_mut(&task.id)
            .expect("the task create just inserted the row");
        stored.pinned_planner_profile_id = planner_pin;
        stored.pinned_worker_profile_id = worker_pin;
        drop(inner);
        let mut task = task;
        task.pinned_planner_profile_id = planner_pin;
        task.pinned_worker_profile_id = worker_pin;
        Ok(task)
    }

    async fn get_task(&self, id: TaskId) -> anyhow::Result<Option<Task>> {
        Ok(self.inner.lock().unwrap().tasks.get(&id).cloned())
    }

    async fn list_tasks(&self, limit: i64) -> anyhow::Result<Vec<Task>> {
        let inner = self.inner.lock().unwrap();
        let mut tasks: Vec<Task> = inner.tasks.values().cloned().collect();
        tasks.sort_by_key(|t| std::cmp::Reverse(t.created_at));
        tasks.truncate(limit.max(0) as usize);
        Ok(tasks)
    }

    async fn set_task_state_guarded(&self, id: TaskId, state: TaskState) -> anyhow::Result<bool> {
        let mut inner = self.inner.lock().unwrap();
        let Some(task) = inner.tasks.get_mut(&id) else {
            return Ok(false);
        };
        if task.state.is_terminal() {
            return Ok(false);
        }
        task.state = state;
        task.updated_at = Utc::now();
        Ok(true)
    }

    async fn resume_paused_task(&self, id: TaskId) -> anyhow::Result<bool> {
        let mut inner = self.inner.lock().unwrap();
        let Some(task) = inner.tasks.get_mut(&id) else {
            return Ok(false);
        };
        if task.state != TaskState::Paused {
            return Ok(false);
        }
        task.state = TaskState::Running;
        task.updated_at = Utc::now();
        Ok(true)
    }

    async fn set_task_capacity(
        &self,
        task_id: TaskId,
        capacity: TaskCapacity,
    ) -> anyhow::Result<Task> {
        let mut inner = self.inner.lock().unwrap();
        let Some(task) = inner.tasks.get_mut(&task_id) else {
            anyhow::bail!("task not found: {task_id}");
        };
        task.capacity = capacity;
        task.updated_at = Utc::now();
        Ok(task.clone())
    }

    async fn add_goal(&self, task_id: TaskId, description: String) -> anyhow::Result<Goal> {
        let goal = Goal {
            id: Uuid::new_v4(),
            task_id,
            description,
            state: GoalState::Open,
            proof_fact_id: None,
            created_at: Utc::now(),
        };
        self.inner
            .lock()
            .unwrap()
            .goals
            .insert(goal.id, goal.clone());
        Ok(goal)
    }

    async fn list_goals(&self, task_id: TaskId) -> anyhow::Result<Vec<Goal>> {
        let inner = self.inner.lock().unwrap();
        let mut goals: Vec<Goal> = inner
            .goals
            .values()
            .filter(|g| g.task_id == task_id)
            .cloned()
            .collect();
        goals.sort_by_key(|g| g.created_at);
        Ok(goals)
    }

    /// One lock covers the state check, goal insert, both activity rows, and
    /// the reopen, so no observer sees a half-reopened task.
    async fn reopen_task_with_goal(
        &self,
        task_id: TaskId,
        description: String,
    ) -> anyhow::Result<ReopenOutcome> {
        let mut inner = self.inner.lock().unwrap();
        let Some(previous_state) = inner.tasks.get(&task_id).map(|task| task.state) else {
            return Ok(ReopenOutcome::NotFound);
        };
        if !previous_state.is_terminal() {
            return Ok(ReopenOutcome::NotTerminal(previous_state));
        }

        let now = Utc::now();
        let goal = Goal {
            id: Uuid::new_v4(),
            task_id,
            description,
            state: GoalState::Open,
            proof_fact_id: None,
            created_at: now,
        };
        inner.goals.insert(goal.id, goal.clone());

        inner.next_activity += 1;
        let activity = Activity {
            id: inner.next_activity,
            task_id,
            kind: "goal_added".to_string(),
            message: format!("human added goal: {}", goal.description),
            payload: serde_json::json!({"goal_id": goal.id, "actor": "human"}),
            created_at: now,
        };
        inner.activities.push(activity);

        let task = inner
            .tasks
            .get_mut(&task_id)
            .expect("the task was present under the same lock");
        task.state = TaskState::Running;
        task.updated_at = Utc::now();
        let task = task.clone();

        inner.next_activity += 1;
        let activity = Activity {
            id: inner.next_activity,
            task_id,
            kind: "task_reopened".to_string(),
            message: "task reopened by human: goal added".to_string(),
            payload: serde_json::json!({
                "goal_id": goal.id,
                "previous_state": previous_state,
            }),
            created_at: Utc::now(),
        };
        inner.activities.push(activity);

        Ok(ReopenOutcome::Reopened { task, goal })
    }

    async fn prove_goal(
        &self,
        task_id: TaskId,
        goal_id: dhi_core::GoalId,
        fact_id: FactId,
        reason: String,
    ) -> anyhow::Result<bool> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("memory store mutex poisoned"))?;

        let fact = inner
            .facts
            .get(&fact_id)
            .filter(|fact| fact.task_id == task_id)
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!("proof rejected: fact {fact_id} does not belong to task {task_id}")
            })?;
        if fact.state != FactState::Confirmed {
            anyhow::bail!(
                "proof rejected: fact {fact_id} is {}, not confirmed",
                fact.state.as_str()
            );
        }

        let goal = inner
            .goals
            .get(&goal_id)
            .filter(|goal| goal.task_id == task_id)
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!("proof rejected: goal {goal_id} does not belong to task {task_id}")
            })?;
        if goal.state == GoalState::Abandoned {
            anyhow::bail!("proof rejected: goal {goal_id} is abandoned");
        }
        if !dhi_followups::fresh_evidence(fact.created_at, goal.created_at) {
            anyhow::bail!(
                "proof rejected: fact {fact_id} was recorded before goal {goal_id}; fresh evidence is required"
            );
        }

        let goal = inner
            .goals
            .get_mut(&goal_id)
            .ok_or_else(|| anyhow::anyhow!("proof rejected: goal {goal_id} disappeared"))?;
        goal.state = GoalState::Met;
        goal.proof_fact_id = Some(fact_id);

        inner.next_activity += 1;
        let activity = Activity {
            id: inner.next_activity,
            task_id,
            kind: "goal_proven".to_string(),
            message: format!("goal {goal_id} proven by fact {fact_id}"),
            payload: serde_json::json!({
                "goal_id": goal_id,
                "fact_id": fact_id,
                "reason": reason,
            }),
            created_at: Utc::now(),
        };
        inner.activities.push(activity);

        Ok(inner
            .goals
            .values()
            .filter(|goal| goal.task_id == task_id)
            .all(|goal| goal.state == GoalState::Met))
    }

    async fn add_intent(
        &self,
        task_id: TaskId,
        fact_id: FactId,
        description: String,
    ) -> anyhow::Result<Intent> {
        let intent = Intent::new(task_id, fact_id, description)?;
        self.inner
            .lock()
            .unwrap()
            .intents
            .insert(intent.id, intent.clone());
        Ok(intent)
    }

    async fn list_open_intents(&self, task_id: TaskId, limit: i64) -> anyhow::Result<Vec<Intent>> {
        let inner = self.inner.lock().unwrap();
        let mut intents: Vec<Intent> = inner
            .intents
            .values()
            .filter(|i| i.task_id == task_id && i.state == IntentState::Open)
            .cloned()
            .collect();
        intents.sort_by_key(|i| (std::cmp::Reverse(i.priority), i.created_at));
        intents.truncate(limit.max(0) as usize);
        Ok(intents)
    }

    async fn list_intents(&self, task_id: TaskId, limit: i64) -> anyhow::Result<Vec<Intent>> {
        let inner = self.inner.lock().unwrap();
        let mut intents: Vec<Intent> = inner
            .intents
            .values()
            .filter(|i| i.task_id == task_id)
            .cloned()
            .collect();
        intents.sort_by_key(|i| (i.created_at, i.id));
        intents.truncate(limit.max(0) as usize);
        Ok(intents)
    }

    async fn save_intent_plan(&self, intent_id: IntentId, plan: &IntentPlan) -> anyhow::Result<()> {
        self.inner
            .lock()
            .unwrap()
            .intent_plans
            .insert(intent_id, plan.clone());
        Ok(())
    }

    async fn get_intent_plan(&self, intent_id: IntentId) -> anyhow::Result<Option<IntentPlan>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .intent_plans
            .get(&intent_id)
            .cloned())
    }

    async fn append_swarm_message(
        &self,
        task_id: TaskId,
        intent_id: IntentId,
        kind: &str,
        payload: serde_json::Value,
    ) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        // Mirror the PostgreSQL foreign keys: both rows must exist.
        if !inner.tasks.contains_key(&task_id) {
            anyhow::bail!("swarm message references missing task {task_id}");
        }
        if !inner.intents.contains_key(&intent_id) {
            anyhow::bail!("swarm message references missing intent {intent_id}");
        }
        inner.next_swarm_message += 1;
        let record = SwarmMessageRecord {
            id: inner.next_swarm_message,
            task_id,
            intent_id,
            kind: kind.to_string(),
            payload,
            created_at: Utc::now(),
        };
        inner.swarm_messages.push(record);
        Ok(())
    }

    async fn list_swarm_messages(
        &self,
        task_id: TaskId,
        limit: i64,
    ) -> anyhow::Result<Vec<SwarmMessageRecord>> {
        let mut messages = self.swarm_messages(task_id);
        messages.truncate(limit.max(0) as usize);
        Ok(messages)
    }

    async fn claim_intent(
        &self,
        intent_id: IntentId,
        owner: &str,
    ) -> anyhow::Result<Option<Intent>> {
        let mut inner = self.inner.lock().unwrap();
        let Some(intent) = inner.intents.get_mut(&intent_id) else {
            return Ok(None);
        };
        if intent.state != IntentState::Open {
            return Ok(None);
        }
        intent.state = IntentState::Running;
        intent.owner = Some(owner.to_string());
        Ok(Some(intent.clone()))
    }

    async fn resume_intent(
        &self,
        intent_id: IntentId,
        owner: &str,
    ) -> anyhow::Result<Option<Intent>> {
        let mut inner = self.inner.lock().unwrap();
        let Some(intent) = inner.intents.get_mut(&intent_id) else {
            return Ok(None);
        };
        if intent.state != IntentState::Paused {
            return Ok(None);
        }
        intent.state = IntentState::Running;
        intent.owner = Some(owner.to_string());
        Ok(Some(intent.clone()))
    }

    async fn finish_intent(&self, intent_id: IntentId, state: IntentState) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let Some(intent) = inner.intents.get_mut(&intent_id) else {
            return Ok(());
        };
        // A paused intent never finishes directly; it must be resumed first.
        if intent.state != IntentState::Running {
            return Ok(());
        }
        intent.state = state;
        intent.completed_at = Some(Utc::now());
        Ok(())
    }

    async fn finish_paused_intent(
        &self,
        intent_id: IntentId,
        owner: &str,
        status: IntentState,
    ) -> anyhow::Result<Option<Intent>> {
        let mut inner = self.inner.lock().unwrap();
        let Some(intent) = inner.intents.get_mut(&intent_id) else {
            return Ok(None);
        };
        // One lock for both steps: paused -> running -> terminal, with no
        // window where a second caller can observe `running`.
        if intent.state != IntentState::Paused {
            return Ok(None);
        }
        intent.state = IntentState::Running;
        intent.owner = Some(owner.to_string());
        intent.state = status;
        intent.completed_at = Some(Utc::now());
        Ok(Some(intent.clone()))
    }

    async fn block_orphaned_paused_intents(&self, task_id: TaskId) -> anyhow::Result<usize> {
        let mut inner = self.inner.lock().unwrap();
        // One lock covers inspection and the update, so no approval can appear
        // between the check and the block.
        let orphaned: Vec<IntentId> = {
            let Inner {
                intents,
                tool_calls,
                approvals,
                ..
            } = &*inner;
            intents
                .values()
                .filter(|i| i.task_id == task_id && i.state == IntentState::Paused)
                .filter(|i| {
                    let calls: Vec<&ToolCall> = tool_calls
                        .values()
                        .filter(|tc| tc.intent_id == i.id)
                        .collect();
                    let has_pending = calls.iter().any(|tc| {
                        approvals
                            .values()
                            .any(|a| a.tool_call_id == tc.id && a.status == ApprovalStatus::Pending)
                    });
                    !calls.is_empty() && !has_pending
                })
                .map(|i| i.id)
                .collect()
        };

        let mut blocked = 0;
        for intent_id in orphaned {
            let Some(intent) = inner.intents.get_mut(&intent_id) else {
                continue;
            };
            intent.state = IntentState::Running;
            intent.state = IntentState::Blocked;
            intent.completed_at = Some(Utc::now());
            blocked += 1;
        }
        Ok(blocked)
    }

    async fn reset_running_intents(&self) -> anyhow::Result<usize> {
        let mut inner = self.inner.lock().unwrap();
        let mut reset = 0;
        for intent in inner.intents.values_mut() {
            if intent.state == IntentState::Running {
                intent.state = IntentState::Open;
                intent.owner = None;
                reset += 1;
            }
        }
        Ok(reset)
    }

    async fn record_fact(
        &self,
        task_id: TaskId,
        claim: String,
        provenance: serde_json::Value,
    ) -> anyhow::Result<Fact> {
        let fact = Fact {
            id: Uuid::new_v4(),
            task_id,
            claim,
            state: FactState::Confirmed,
            provenance,
            origin_of: None,
            created_at: Utc::now(),
        };
        self.inner
            .lock()
            .unwrap()
            .facts
            .insert(fact.id, fact.clone());
        Ok(fact)
    }

    async fn record_fact_for_intent(
        &self,
        task_id: TaskId,
        intent_id: IntentId,
        claim: String,
        provenance: serde_json::Value,
    ) -> anyhow::Result<Fact> {
        let mut inner = self.inner.lock().unwrap();
        // Mirror the PostgreSQL foreign key: the intent must exist.
        if !inner.intents.contains_key(&intent_id) {
            anyhow::bail!("fact references missing intent {intent_id}");
        }
        // Mirror `facts_one_per_intent`: one fact per intent, ever.
        if inner.fact_intents.contains_key(&intent_id) {
            return Err(CoreError::FactAlreadyRecordedForIntent(intent_id).into());
        }
        let fact = Fact {
            id: Uuid::new_v4(),
            task_id,
            claim,
            state: FactState::Confirmed,
            provenance,
            origin_of: None,
            created_at: Utc::now(),
        };
        inner.facts.insert(fact.id, fact.clone());
        inner.fact_intents.insert(intent_id, fact.id);
        Ok(fact)
    }

    async fn list_facts(&self, task_id: TaskId) -> anyhow::Result<Vec<Fact>> {
        Ok(self.facts(task_id))
    }

    async fn record_result(
        &self,
        task_id: TaskId,
        intent_id: IntentId,
        claim: String,
        acceptance: ResultStatus,
        evidence: serde_json::Value,
    ) -> anyhow::Result<ResultRecord> {
        let result = ResultRecord {
            id: Uuid::new_v4(),
            task_id,
            intent_id,
            claim,
            acceptance,
            evidence,
            created_at: Utc::now(),
        };
        self.inner.lock().unwrap().results.push(result.clone());
        Ok(result)
    }

    async fn list_results(&self, task_id: TaskId, limit: i64) -> anyhow::Result<Vec<ResultRecord>> {
        let inner = self.inner.lock().unwrap();
        let mut results: Vec<ResultRecord> = inner
            .results
            .iter()
            .filter(|r| r.task_id == task_id)
            .cloned()
            .collect();
        results.sort_by_key(|r| (r.created_at, r.id));
        results.truncate(limit.max(0) as usize);
        Ok(results)
    }

    async fn append_activity(
        &self,
        task_id: TaskId,
        kind: &str,
        message: String,
        payload: serde_json::Value,
    ) -> anyhow::Result<Activity> {
        let mut inner = self.inner.lock().unwrap();
        inner.next_activity += 1;
        let activity = Activity {
            id: inner.next_activity,
            task_id,
            kind: kind.to_string(),
            message,
            payload,
            created_at: Utc::now(),
        };
        inner.activities.push(activity.clone());
        Ok(activity)
    }

    async fn list_activity(
        &self,
        task_id: TaskId,
        kind: Option<&str>,
    ) -> anyhow::Result<Vec<Activity>> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .activities
            .iter()
            .filter(|a| a.task_id == task_id && kind.is_none_or(|kind| a.kind == kind))
            .cloned()
            .collect())
    }

    fn save_agent_run<'a>(
        &'a self,
        run: &'a AgentRun,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            self.inner
                .lock()
                .unwrap()
                .agent_runs
                .insert(run.id, run.clone());
            Ok(())
        })
    }

    async fn list_agent_runs(&self, task_id: TaskId) -> anyhow::Result<Vec<AgentRun>> {
        Ok(self.agent_runs(task_id))
    }

    async fn create_side_question_session(
        &self,
        task_id: TaskId,
        snapshot: serde_json::Value,
    ) -> anyhow::Result<SideQuestionSession> {
        let now = Utc::now();
        let mut inner = self.inner.lock().unwrap();
        // One open conversation per task, matching the Postgres store.
        for session in inner.side_sessions.values_mut() {
            if session.task_id == task_id && session.status == SideQuestionSessionStatus::Open {
                session.status = SideQuestionSessionStatus::Closed;
                session.closed_at = Some(now);
            }
        }
        let session = SideQuestionSession {
            id: Uuid::new_v4(),
            task_id,
            snapshot,
            status: SideQuestionSessionStatus::Open,
            created_at: now,
            closed_at: None,
        };
        inner.side_sessions.insert(session.id, session.clone());
        Ok(session)
    }

    async fn get_open_side_question_session(
        &self,
        task_id: TaskId,
    ) -> anyhow::Result<Option<SideQuestionSession>> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .side_sessions
            .values()
            .filter(|session| {
                session.task_id == task_id && session.status == SideQuestionSessionStatus::Open
            })
            .max_by_key(|session| (session.created_at, session.id))
            .cloned())
    }

    async fn get_side_question_session(
        &self,
        session_id: Uuid,
    ) -> anyhow::Result<Option<SideQuestionSession>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .side_sessions
            .get(&session_id)
            .cloned())
    }

    async fn list_side_question_sessions(
        &self,
        task_id: TaskId,
    ) -> anyhow::Result<Vec<SideQuestionThread>> {
        let inner = self.inner.lock().unwrap();
        let mut sessions: Vec<SideQuestionSession> = inner
            .side_sessions
            .values()
            .filter(|session| session.task_id == task_id)
            .cloned()
            .collect();
        sessions.sort_by_key(|session| (std::cmp::Reverse(session.created_at), session.id));

        let mut threads = Vec::with_capacity(sessions.len());
        for session in sessions {
            let mut requests: Vec<SideQuestionRequest> = inner
                .side_requests
                .iter()
                .filter(|request| request.session_id == session.id)
                .cloned()
                .collect();
            requests.sort_by_key(|request| (request.created_at, request.id));
            threads.push(SideQuestionThread { session, requests });
        }
        Ok(threads)
    }

    async fn append_side_question(
        &self,
        request: NewSideQuestionRequest,
    ) -> anyhow::Result<SideQuestionRequest> {
        let now = Utc::now();
        let request = SideQuestionRequest {
            id: Uuid::new_v4(),
            session_id: request.session_id,
            question: request.question,
            answer: request.answer,
            status: request.status,
            error: request.error,
            model: request.model,
            tokens_in: request.tokens_in,
            tokens_out: request.tokens_out,
            latency_ms: request.latency_ms,
            created_at: now,
            completed_at: match request.status {
                SideQuestionStatus::Pending => None,
                SideQuestionStatus::Ok | SideQuestionStatus::Error => Some(now),
            },
        };
        self.inner
            .lock()
            .unwrap()
            .side_requests
            .push(request.clone());
        Ok(request)
    }
}

#[async_trait]
impl GuardStore for MemoryStore {
    async fn record_tool_call(
        &self,
        task_id: TaskId,
        intent_id: IntentId,
        tool: String,
        input: serde_json::Value,
    ) -> anyhow::Result<ToolCall> {
        let call = ToolCall {
            id: Uuid::new_v4(),
            task_id,
            intent_id,
            tool_name: tool,
            input,
            output: None,
            status: "pending".to_string(),
            created_at: Utc::now(),
        };
        self.inner
            .lock()
            .unwrap()
            .tool_calls
            .insert(call.id, call.clone());
        Ok(call)
    }

    async fn get_tool_call(&self, tool_call_id: ToolCallId) -> anyhow::Result<Option<ToolCall>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .tool_calls
            .get(&tool_call_id)
            .cloned())
    }

    async fn complete_tool_call(
        &self,
        tool_call_id: ToolCallId,
        output: Option<serde_json::Value>,
        status: &str,
    ) -> anyhow::Result<()> {
        if let Some(call) = self.inner.lock().unwrap().tool_calls.get_mut(&tool_call_id) {
            call.output = output;
            call.status = status.to_string();
        }
        Ok(())
    }

    async fn record_guard_decision(
        &self,
        tool_call_id: ToolCallId,
        verdict: GuardVerdict,
        matched_rule: Option<String>,
        rule_version: Option<String>,
        reason: String,
    ) -> anyhow::Result<GuardDecision> {
        let decision = GuardDecision {
            id: Uuid::new_v4(),
            tool_call_id,
            verdict,
            matched_rule,
            rule_version,
            reason,
            created_at: Utc::now(),
        };
        self.inner
            .lock()
            .unwrap()
            .guard_decisions
            .insert(decision.id, decision.clone());
        Ok(decision)
    }

    async fn create_approval(
        &self,
        task_id: TaskId,
        tool_call_id: ToolCallId,
        guard_decision_id: GuardDecisionId,
        reason: String,
        expires_at: chrono::DateTime<Utc>,
    ) -> anyhow::Result<Approval> {
        let approval = Approval {
            id: Uuid::new_v4(),
            task_id,
            tool_call_id,
            guard_decision_id,
            status: ApprovalStatus::Pending,
            reason,
            expires_at,
            decided_at: None,
            created_at: Utc::now(),
        };
        self.inner
            .lock()
            .unwrap()
            .approvals
            .insert(approval.id, approval.clone());
        Ok(approval)
    }

    async fn get_approval(&self, approval_id: ApprovalId) -> anyhow::Result<Option<Approval>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .approvals
            .get(&approval_id)
            .cloned())
    }

    async fn list_pending_approvals(&self, task_id: TaskId) -> anyhow::Result<Vec<Approval>> {
        let inner = self.inner.lock().unwrap();
        let mut approvals: Vec<Approval> = inner
            .approvals
            .values()
            .filter(|a| a.task_id == task_id && a.status == ApprovalStatus::Pending)
            .cloned()
            .collect();
        approvals.sort_by_key(|a| a.created_at);
        Ok(approvals)
    }

    async fn list_pending_approvals_all(&self) -> anyhow::Result<Vec<Approval>> {
        let inner = self.inner.lock().unwrap();
        let mut approvals: Vec<Approval> = inner
            .approvals
            .values()
            .filter(|a| a.status == ApprovalStatus::Pending)
            .cloned()
            .collect();
        approvals.sort_by_key(|a| (a.created_at, a.id));
        Ok(approvals)
    }

    async fn expire_pending_approvals(
        &self,
        task_id: TaskId,
        now: chrono::DateTime<Utc>,
    ) -> anyhow::Result<Vec<Approval>> {
        let mut inner = self.inner.lock().unwrap();
        let mut expired = Vec::new();
        for approval in inner.approvals.values_mut() {
            if approval.task_id == task_id
                && approval.status == ApprovalStatus::Pending
                && approval.expires_at < now
            {
                approval.status = ApprovalStatus::Expired;
                approval.decided_at = None;
                expired.push(approval.clone());
            }
        }
        expired.sort_by_key(|a| a.created_at);
        Ok(expired)
    }

    async fn list_expired_approvals(&self, task_id: TaskId) -> anyhow::Result<Vec<Approval>> {
        let inner = self.inner.lock().unwrap();
        let mut approvals: Vec<Approval> = inner
            .approvals
            .values()
            .filter(|a| a.task_id == task_id && a.status == ApprovalStatus::Expired)
            .cloned()
            .collect();
        approvals.sort_by_key(|a| a.created_at);
        Ok(approvals)
    }

    async fn block_intent_for_approval(
        &self,
        approval_id: ApprovalId,
        owner: &str,
    ) -> anyhow::Result<bool> {
        let mut inner = self.inner.lock().unwrap();
        // One lock: resolve the linked intent, then paused -> running ->
        // blocked with no observable `running` window.
        let intent_id = inner
            .approvals
            .get(&approval_id)
            .and_then(|a| inner.tool_calls.get(&a.tool_call_id))
            .map(|tc| tc.intent_id);
        let Some(intent_id) = intent_id else {
            return Ok(false);
        };
        let Some(intent) = inner.intents.get_mut(&intent_id) else {
            return Ok(false);
        };
        if intent.state != IntentState::Paused {
            return Ok(false);
        }
        intent.state = IntentState::Running;
        intent.owner = Some(owner.to_string());
        intent.state = IntentState::Blocked;
        intent.completed_at = Some(Utc::now());
        Ok(true)
    }

    async fn decide_approval(
        &self,
        approval_id: ApprovalId,
        decision: ApprovalDecision,
    ) -> anyhow::Result<Option<Approval>> {
        let mut inner = self.inner.lock().unwrap();
        let Some(approval) = inner.approvals.get_mut(&approval_id) else {
            return Ok(None);
        };
        if approval.status != ApprovalStatus::Pending {
            return Ok(None);
        }

        if approval.expires_at < Utc::now() {
            approval.status = ApprovalStatus::Expired;
            approval.decided_at = None;
        } else {
            approval.status = match decision {
                ApprovalDecision::Allow => ApprovalStatus::Granted,
                ApprovalDecision::Deny => ApprovalStatus::Denied,
            };
            approval.decided_at = Some(Utc::now());
        }
        Ok(Some(approval.clone()))
    }
}
