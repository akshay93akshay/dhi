//! Storage contract used by the engine.
//!
//! The engine depends on these traits, not on PostgreSQL. That keeps the
//! engine testable with an in-memory store.

use crate::{
    Activity, AgentRun, Approval, ApprovalDecision, ApprovalId, Fact, FactId, Goal, GoalId,
    GuardDecision, GuardDecisionId, GuardVerdict, Intent, IntentId, IntentPlan, IntentState,
    NewSideQuestionRequest, ReopenOutcome, ResultRecord, ResultStatus, SideQuestionRequest,
    SideQuestionSession, SideQuestionThread, SwarmMessageRecord, Task, TaskCapacity, TaskId,
    TaskState, ToolCall, ToolCallId,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::future::Future;
use std::pin::Pin;
use uuid::Uuid;

#[async_trait]
pub trait GraphStore: Send + Sync {
    /// Creates a task with its origin fact and one `task_created` activity.
    async fn create_task(&self, goal: String, bound: i32) -> anyhow::Result<Task>;

    /// Like [`GraphStore::create_task`] with optional per-task role pins.
    /// `None` keeps the task on the role's default chain; a present id must
    /// reference an existing `llm_profiles` row (the caller validates).
    async fn create_task_with_pins(
        &self,
        goal: String,
        bound: i32,
        planner_pin: Option<i64>,
        worker_pin: Option<i64>,
    ) -> anyhow::Result<Task>;

    async fn get_task(&self, id: TaskId) -> anyhow::Result<Option<Task>>;
    async fn list_tasks(&self, limit: i64) -> anyhow::Result<Vec<Task>>;

    /// Sets a task state. This is the only public way to write a task state.
    ///
    /// The first terminal state wins: once the stored state is `done`,
    /// `failed`, or `timeout`, no further write applies, including a
    /// non-terminal target such as `running`. Returns whether a row changed.
    async fn set_task_state_guarded(&self, id: TaskId, state: TaskState) -> anyhow::Result<bool>;

    /// Replaces one task's capacity ledger and returns the updated task.
    ///
    /// An unknown task is a normal error (not a panic). There is no terminal
    /// guard here; the API refuses terminal tasks before calling this.
    async fn set_task_capacity(
        &self,
        task_id: TaskId,
        capacity: TaskCapacity,
    ) -> anyhow::Result<Task>;

    async fn add_goal(&self, task_id: TaskId, description: String) -> anyhow::Result<Goal>;
    async fn list_goals(&self, task_id: TaskId) -> anyhow::Result<Vec<Goal>>;

    /// Adds one open goal to a terminal task and reopens it, in one
    /// transaction.
    ///
    /// This is the ONLY path that may move `done`, `failed`, or `timeout`
    /// back to `running`, and it does so deliberately: the new goal needs a
    /// live engine loop. It appends `goal_added` (actor `human`) and
    /// `task_reopened` (with the previous state) in that order. A task that
    /// is missing returns `NotFound`; one that is not terminal returns
    /// `NotTerminal(state)` and is left untouched. `set_task_state_guarded`
    /// keeps its first-terminal-wins rule and is never loosened.
    async fn reopen_task_with_goal(
        &self,
        task_id: TaskId,
        description: String,
    ) -> anyhow::Result<ReopenOutcome>;

    /// Proves one goal with a confirmed fact from the same task.
    ///
    /// The store rejects a fact that is missing from the task or not
    /// `confirmed`, and a goal that is missing from the task or `abandoned`.
    /// On success the goal becomes `met` with its `proof_fact_id` set, and a
    /// `goal_proven` activity row records the proof. Returns `true` when every
    /// goal of the task is now `met`.
    async fn prove_goal(
        &self,
        task_id: TaskId,
        goal_id: GoalId,
        fact_id: FactId,
        reason: String,
    ) -> anyhow::Result<bool>;

    async fn add_intent(
        &self,
        task_id: TaskId,
        fact_id: FactId,
        description: String,
    ) -> anyhow::Result<Intent>;
    async fn list_open_intents(&self, task_id: TaskId, limit: i64) -> anyhow::Result<Vec<Intent>>;

    /// Every intent of `task_id` in any state, oldest first, at most `limit`
    /// rows. The API reads the full intent history through this; the engine
    /// still claims work through `list_open_intents`.
    async fn list_intents(&self, task_id: TaskId, limit: i64) -> anyhow::Result<Vec<Intent>>;

    /// Upserts the DHI+ plan attached to `intent_id`.
    ///
    /// The plan is audit data: it stays after the intent completes and is only
    /// removed with its intent. Re-saving the same intent replaces the plan.
    async fn save_intent_plan(&self, intent_id: IntentId, plan: &IntentPlan) -> anyhow::Result<()>;

    /// The DHI+ plan attached to `intent_id`, or `None` when the intent was
    /// added without one or no plan row exists.
    async fn get_intent_plan(&self, intent_id: IntentId) -> anyhow::Result<Option<IntentPlan>>;

    /// Appends one typed message broadcast while a swarm executed `intent_id`.
    ///
    /// The payload is the serialized message and `kind` its typed name. Rows
    /// are append-only audit data and are removed with their task.
    async fn append_swarm_message(
        &self,
        task_id: TaskId,
        intent_id: IntentId,
        kind: &str,
        payload: serde_json::Value,
    ) -> anyhow::Result<()>;

    /// Every swarm message of `task_id`, ordered by id, at most `limit` rows.
    async fn list_swarm_messages(
        &self,
        task_id: TaskId,
        limit: i64,
    ) -> anyhow::Result<Vec<SwarmMessageRecord>>;

    async fn claim_intent(
        &self,
        intent_id: IntentId,
        owner: &str,
    ) -> anyhow::Result<Option<Intent>>;

    /// Unparks a paused intent: `paused -> running`. Returns `None` when the
    /// intent is missing or not paused.
    async fn resume_intent(
        &self,
        intent_id: IntentId,
        owner: &str,
    ) -> anyhow::Result<Option<Intent>>;

    /// Moves a running intent to a terminal state.
    ///
    /// Only `running` intents can finish. A paused intent never finishes; it
    /// must be resumed first.
    async fn finish_intent(&self, intent_id: IntentId, state: IntentState) -> anyhow::Result<()>;

    /// Resumes and finishes a paused intent in one atomic operation:
    /// `paused -> running -> terminal`. The owner is recorded with the
    /// resume.
    ///
    /// Returns the final intent, or `None` when the intent is missing or no
    /// longer paused. Exactly one concurrent caller can win; the losers see
    /// the intent already out of `paused` and get `None`. There is no window
    /// in which the intent is left `running` under this owner.
    async fn finish_paused_intent(
        &self,
        intent_id: IntentId,
        owner: &str,
        status: IntentState,
    ) -> anyhow::Result<Option<Intent>>;

    /// Blocks every paused intent of `task_id` that no longer has a `pending`
    /// approval behind its tool call. Such an intent is orphaned: nothing can
    /// decide it, so it would stay paused forever.
    ///
    /// Each orphan moves `paused -> running -> blocked` in one atomic
    /// operation. A paused intent that still has a pending approval is left
    /// alone. Returns the number of intents blocked.
    async fn block_orphaned_paused_intents(&self, task_id: TaskId) -> anyhow::Result<usize>;

    /// Restart recovery: moves every `running` intent back to `open` and
    /// clears its owner. A process that dies mid-intent leaves a `running`
    /// row nobody owns; the next start releases it so a worker can claim it
    /// again. Returns the number of rows reset.
    async fn reset_running_intents(&self) -> anyhow::Result<usize>;

    async fn record_fact(
        &self,
        task_id: TaskId,
        claim: String,
        provenance: serde_json::Value,
    ) -> anyhow::Result<Fact>;

    /// Records a confirmed fact bound to exactly one intent.
    ///
    /// Unlike [`GraphStore::record_fact`], the fact names its intent and the
    /// store enforces at most one such fact per intent. A second fact returns
    /// [`dhi_core::CoreError::FactAlreadyRecordedForIntent`]; a missing intent
    /// is rejected by the foreign key.
    async fn record_fact_for_intent(
        &self,
        task_id: TaskId,
        intent_id: IntentId,
        claim: String,
        provenance: serde_json::Value,
    ) -> anyhow::Result<Fact>;

    async fn list_facts(&self, task_id: TaskId) -> anyhow::Result<Vec<Fact>>;

    async fn record_result(
        &self,
        task_id: TaskId,
        intent_id: IntentId,
        claim: String,
        acceptance: ResultStatus,
        evidence: serde_json::Value,
    ) -> anyhow::Result<ResultRecord>;

    /// Every result of `task_id`, oldest first, at most `limit` rows.
    async fn list_results(&self, task_id: TaskId, limit: i64) -> anyhow::Result<Vec<ResultRecord>>;

    async fn append_activity(
        &self,
        task_id: TaskId,
        kind: &str,
        message: String,
        payload: serde_json::Value,
    ) -> anyhow::Result<Activity>;

    /// Oldest-first activity of `task_id`, optionally only one `kind`. The
    /// record is append-only and read-only; this only reads it back.
    async fn list_activity(
        &self,
        task_id: TaskId,
        kind: Option<&str>,
    ) -> anyhow::Result<Vec<Activity>>;

    /// Upserts one `agent_runs` row: one per real completion call.
    ///
    /// The future is written out rather than taken through `async_trait` so
    /// a test double that only records rows does not need its own dependency
    /// on the macro; every implementation is `async fn` either way.
    fn save_agent_run<'a>(
        &'a self,
        run: &'a AgentRun,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

    /// Every completion-call row of `task_id`, oldest first.
    async fn list_agent_runs(&self, task_id: TaskId) -> anyhow::Result<Vec<AgentRun>>;

    /// Creates a side-question session with an already-built frozen snapshot.
    /// Any open session of the same task is closed first, so a task has at
    /// most one open conversation.
    async fn create_side_question_session(
        &self,
        task_id: TaskId,
        snapshot: serde_json::Value,
    ) -> anyhow::Result<SideQuestionSession>;

    /// The newest open side-question session of a task, if one exists.
    async fn get_open_side_question_session(
        &self,
        task_id: TaskId,
    ) -> anyhow::Result<Option<SideQuestionSession>>;

    /// One session by id, including its frozen snapshot. `None` when the
    /// session does not exist.
    async fn get_side_question_session(
        &self,
        session_id: Uuid,
    ) -> anyhow::Result<Option<SideQuestionSession>>;

    /// Every side-question session of a task, newest first, each with its
    /// requests oldest first.
    async fn list_side_question_sessions(
        &self,
        task_id: TaskId,
    ) -> anyhow::Result<Vec<SideQuestionThread>>;

    /// Appends one completed question/answer attempt to a session.
    async fn append_side_question(
        &self,
        request: NewSideQuestionRequest,
    ) -> anyhow::Result<SideQuestionRequest>;
}

/// Persistence for the guard choke point.
///
/// Every tool call goes through here: record the call, record the guard
/// verdict, then complete the call. An `Ask` verdict also creates an approval.
///
/// The intent id is mandatory and comes from the caller, never from the model.
#[async_trait]
pub trait GuardStore: Send + Sync {
    async fn record_tool_call(
        &self,
        task_id: TaskId,
        intent_id: IntentId,
        tool: String,
        input: serde_json::Value,
    ) -> anyhow::Result<ToolCall>;

    async fn get_tool_call(&self, tool_call_id: ToolCallId) -> anyhow::Result<Option<ToolCall>>;

    async fn complete_tool_call(
        &self,
        tool_call_id: ToolCallId,
        output: Option<serde_json::Value>,
        status: &str,
    ) -> anyhow::Result<()>;

    async fn record_guard_decision(
        &self,
        tool_call_id: ToolCallId,
        verdict: GuardVerdict,
        matched_rule: Option<String>,
        rule_version: Option<String>,
        reason: String,
    ) -> anyhow::Result<GuardDecision>;

    async fn create_approval(
        &self,
        task_id: TaskId,
        tool_call_id: ToolCallId,
        guard_decision_id: GuardDecisionId,
        reason: String,
        expires_at: DateTime<Utc>,
    ) -> anyhow::Result<Approval>;

    async fn get_approval(&self, approval_id: ApprovalId) -> anyhow::Result<Option<Approval>>;

    async fn list_pending_approvals(&self, task_id: TaskId) -> anyhow::Result<Vec<Approval>>;

    /// Every pending approval of every task, oldest first. The approval queue
    /// is not scoped by task; a decision still names one approval id.
    async fn list_pending_approvals_all(&self) -> anyhow::Result<Vec<Approval>>;

    /// Marks every pending approval of `task_id` whose `expires_at` is before
    /// `now` as `expired` and returns the rows it changed.
    ///
    /// Expiry is scoped by task: one supervisor sweep covers exactly one
    /// task, and no other task's approvals move. A timer calls this; DHI
    /// never waits for a person to poll the queue.
    async fn expire_pending_approvals(
        &self,
        task_id: TaskId,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Vec<Approval>>;

    /// Every `expired` approval of `task_id`, oldest first. A sweep uses it to
    /// record an expiry its per-approval timer performed.
    async fn list_expired_approvals(&self, task_id: TaskId) -> anyhow::Result<Vec<Approval>>;

    /// Blocks the intent behind an approval that expired. The intent is moved
    /// `paused -> running -> blocked` in one atomic statement, with the given
    /// owner.
    ///
    /// Returns whether the intent moved. `Ok(false)` means the approval, its
    /// tool call, or its intent is missing, or the intent was not paused
    /// (already resumed, already blocked, or claimed by somebody else).
    async fn block_intent_for_approval(
        &self,
        approval_id: ApprovalId,
        owner: &str,
    ) -> anyhow::Result<bool>;

    /// Decides a pending approval. Returns `None` when the approval does not
    /// exist or is no longer pending.
    async fn decide_approval(
        &self,
        approval_id: ApprovalId,
        decision: ApprovalDecision,
    ) -> anyhow::Result<Option<Approval>>;
}
