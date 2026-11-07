//! DHI core types and state machines.
//!
//! The core is domain-neutral. It knows tasks, goals, intents, facts,
//! results, guard decisions, approvals, runs, and checkpoints. It does not
//! know chess, finance, or any other domain.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

pub mod graph;
pub mod orchestration;
pub mod side_question;
pub use graph::{GraphStore, GuardStore};
pub use orchestration::{
    CapabilityLimits, IntentPlan, OrchestrationError, Subtask, Topology, WorkerSpec,
};
pub use side_question::{
    NewSideQuestionRequest, SideQuestionRequest, SideQuestionSession, SideQuestionSessionStatus,
    SideQuestionStatus, SideQuestionThread,
};

pub type TaskId = Uuid;
pub type IntentId = Uuid;
pub type FactId = Uuid;
pub type ResultId = Uuid;
pub type GoalId = Uuid;
pub type ActivityId = i64;
pub type RunId = Uuid;
pub type CheckpointId = Uuid;
pub type ToolCallId = Uuid;
pub type GuardDecisionId = Uuid;
pub type ApprovalId = Uuid;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("invalid state transition: {from} -> {to}")]
    InvalidTransition { from: String, to: String },
    #[error("intent must reference a fact")]
    IntentWithoutFact,
    #[error("task must have an origin fact")]
    MissingOriginFact,
    #[error("fact already recorded for intent {0}")]
    FactAlreadyRecordedForIntent(IntentId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Created,
    Running,
    Paused,
    Done,
    Failed,
    Timeout,
    Cancelled,
}

impl TaskState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Created, Self::Running)
                | (Self::Created, Self::Failed)
                | (Self::Created, Self::Cancelled)
                | (Self::Running, Self::Paused)
                | (Self::Running, Self::Done)
                | (Self::Running, Self::Failed)
                | (Self::Running, Self::Timeout)
                | (Self::Running, Self::Cancelled)
                | (Self::Paused, Self::Running)
                | (Self::Paused, Self::Failed)
                | (Self::Paused, Self::Timeout)
                | (Self::Paused, Self::Cancelled)
                | (Self::Done, Self::Running)
                | (Self::Failed, Self::Running)
                | (Self::Timeout, Self::Running)
                | (Self::Cancelled, Self::Running)
        )
    }

    /// `done`, `failed`, `timeout`, and `cancelled` are final until the task
    /// is revived.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Done | Self::Failed | Self::Timeout | Self::Cancelled
        )
    }
}

impl std::fmt::Display for TaskState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalState {
    Open,
    Met,
    Abandoned,
}

impl GoalState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Met => "met",
            Self::Abandoned => "abandoned",
        }
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Open, Self::Met)
                | (Self::Open, Self::Abandoned)
                | (Self::Met, Self::Open)
                | (Self::Abandoned, Self::Open)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentState {
    Open,
    Running,
    Paused,
    Done,
    Blocked,
    Exhausted,
    Stopped,
    Deleted,
}

impl IntentState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Done => "done",
            Self::Blocked => "blocked",
            Self::Exhausted => "exhausted",
            Self::Stopped => "stopped",
            Self::Deleted => "deleted",
        }
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Open, Self::Running)
                | (Self::Open, Self::Deleted)
                | (Self::Running, Self::Done)
                | (Self::Running, Self::Blocked)
                | (Self::Running, Self::Exhausted)
                | (Self::Running, Self::Stopped)
                | (Self::Running, Self::Paused)
                | (Self::Running, Self::Open)
                // A paused intent never finishes: it is unparked back to open
                // or resumed to running, then finishes from running.
                | (Self::Paused, Self::Open)
                | (Self::Paused, Self::Running)
                | (Self::Blocked, Self::Open)
                | (Self::Exhausted, Self::Open)
                | (Self::Stopped, Self::Open)
        )
    }

    /// `done`, `blocked`, `exhausted`, `stopped`, and `deleted` are final.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Done | Self::Blocked | Self::Exhausted | Self::Stopped | Self::Deleted
        )
    }
}

impl std::fmt::Display for IntentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactState {
    Origin,
    Confirmed,
    Dismissed,
}

impl FactState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Origin => "origin",
            Self::Confirmed => "confirmed",
            Self::Dismissed => "dismissed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultStatus {
    Pending,
    Accepted,
    Rejected,
}

impl ResultStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardVerdict {
    Allow,
    Deny,
    Ask,
}

impl GuardVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Ask => "ask",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Granted,
    Denied,
    Expired,
}

impl ApprovalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Granted => "granted",
            Self::Denied => "denied",
            Self::Expired => "expired",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Allow,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    Planner,
    Worker,
    Reporter,
    Digest,
}

impl AgentRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planner => "planner",
            Self::Worker => "worker",
            Self::Reporter => "reporter",
            Self::Digest => "digest",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub goal: String,
    pub bound: i32,
    pub state: TaskState,
    pub exploration_id: Uuid,
    pub origin_fact_id: FactId,
    /// Optional per-task planner profile pin. `None` follows the role default
    /// chain. Stored in `tasks.pinned_planner_profile_id`.
    pub pinned_planner_profile_id: Option<i64>,
    /// Optional per-task worker profile pin. `None` follows the role default
    /// chain. Stored in `tasks.pinned_worker_profile_id`.
    pub pinned_worker_profile_id: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Task {
    pub fn new(goal: impl Into<String>, bound: i32) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            goal: goal.into(),
            bound,
            state: TaskState::Created,
            exploration_id: Uuid::new_v4(),
            origin_fact_id: Uuid::new_v4(),
            pinned_planner_profile_id: None,
            pinned_worker_profile_id: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn origin_fact(&self) -> Fact {
        Fact {
            id: self.origin_fact_id,
            task_id: self.id,
            claim: format!("Origin: {}", self.goal),
            state: FactState::Origin,
            provenance: serde_json::json!({
                "goal": self.goal,
                "bound": self.bound,
            }),
            origin_of: None,
            created_at: self.created_at,
        }
    }

    pub fn transition(&mut self, next: TaskState) -> Result<(), CoreError> {
        if !self.state.can_transition_to(next) {
            return Err(CoreError::InvalidTransition {
                from: self.state.to_string(),
                to: next.to_string(),
            });
        }
        self.state = next;
        self.updated_at = Utc::now();
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Goal {
    pub id: GoalId,
    pub task_id: TaskId,
    pub description: String,
    pub state: GoalState,
    /// The confirmed fact that proved this goal, once a planner proves it.
    pub proof_fact_id: Option<FactId>,
    pub created_at: DateTime<Utc>,
}

/// The domain result of adding a goal to a terminal task.
///
/// `Reopened` is the only successful shape: it carries the task already moved
/// back to `running` and the new open goal. A task that is not `done`,
/// `failed`, `timeout`, or `cancelled` is refused with `NotTerminal`, so this
/// operation can never reopen a live task by accident.
// The success value carries the two rows the API returns; boxing them would
// only add indirection to a one-shot operation. The error variants are tiny.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum ReopenOutcome {
    Reopened { task: Task, goal: Goal },
    NotFound,
    NotTerminal(TaskState),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Intent {
    pub id: IntentId,
    pub task_id: TaskId,
    pub goal_id: Option<GoalId>,
    pub fact_id: FactId,
    pub description: String,
    pub state: IntentState,
    pub owner: Option<String>,
    pub priority: i32,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

impl Intent {
    pub fn new(
        task_id: TaskId,
        fact_id: FactId,
        description: impl Into<String>,
    ) -> Result<Self, CoreError> {
        if fact_id.is_nil() {
            return Err(CoreError::IntentWithoutFact);
        }
        Ok(Self {
            id: Uuid::new_v4(),
            task_id,
            goal_id: None,
            fact_id,
            description: description.into(),
            state: IntentState::Open,
            owner: None,
            priority: 0,
            created_at: Utc::now(),
            completed_at: None,
        })
    }

    pub fn claim(&mut self, owner: impl Into<String>) -> Result<(), CoreError> {
        if self.state != IntentState::Open {
            return Err(CoreError::InvalidTransition {
                from: self.state.to_string(),
                to: IntentState::Running.to_string(),
            });
        }
        self.state = IntentState::Running;
        self.owner = Some(owner.into());
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fact {
    pub id: FactId,
    pub task_id: TaskId,
    pub claim: String,
    pub state: FactState,
    pub provenance: serde_json::Value,
    pub origin_of: Option<FactId>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultRecord {
    pub id: ResultId,
    pub task_id: TaskId,
    pub intent_id: IntentId,
    pub claim: String,
    pub acceptance: ResultStatus,
    pub evidence: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Activity {
    pub id: ActivityId,
    pub task_id: TaskId,
    pub kind: String,
    pub message: String,
    pub payload: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

/// One typed message broadcast during a DHI+ swarm run.
///
/// The payload is the serialized `SwarmMessage` and `kind` is its typed name
/// (`finding`, `question`, `challenge`, `done`). Rows are append-only audit
/// data; they live until their task is deleted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmMessageRecord {
    pub id: i64,
    pub task_id: TaskId,
    pub intent_id: IntentId,
    pub kind: String,
    pub payload: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub task_id: TaskId,
    /// The intent that requested the call. Stamped by the caller before the
    /// model sees the tool; a tool call can never be detached from its intent.
    pub intent_id: IntentId,
    pub tool_name: String,
    pub input: serde_json::Value,
    pub output: Option<serde_json::Value>,
    pub status: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRequest {
    pub tool: String,
    pub input: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolCallOutcome {
    Allowed { output: serde_json::Value },
    Denied { reason: String },
    Pending { approval_id: ApprovalId },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuardDecision {
    pub id: GuardDecisionId,
    pub tool_call_id: ToolCallId,
    pub verdict: GuardVerdict,
    pub matched_rule: Option<String>,
    pub rule_version: Option<String>,
    pub reason: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Approval {
    pub id: ApprovalId,
    pub task_id: TaskId,
    pub tool_call_id: ToolCallId,
    pub guard_decision_id: GuardDecisionId,
    pub status: ApprovalStatus,
    pub reason: String,
    pub expires_at: DateTime<Utc>,
    pub decided_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRun {
    /// Durable row identity: one row per completion call.
    pub id: RunId,
    /// The agent run this call belongs to; retries share it.
    pub run_id: RunId,
    /// Position of this call inside the run, from 0.
    pub call_index: u32,
    pub task_id: TaskId,
    pub intent_id: Option<IntentId>,
    pub role: AgentRole,
    pub model: String,
    /// `<provider>/<model>/max_tokens=<n>/stream=<bool>`, no secret.
    pub profile: String,
    pub prompt: String,
    pub prompt_hash: String,
    /// The typed variables the prompt was rendered from, for reconstruction.
    pub variables: serde_json::Value,
    pub response: Option<String>,
    /// The provider's final reasoning text for this call. `None` means the
    /// model returned none; an empty string is never stored.
    pub thinking: Option<String>,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub reasoning_tokens: i64,
    /// Provider-reported USD only; `0` when the provider is silent.
    pub cost: f64,
    /// One of `AgentRunStatus`: `running`, `ok`, `error`, `exhausted`,
    /// `timed_out`, `truncated`, `no_output`, `aborted`.
    pub status: String,
    pub error: Option<String>,
    pub latency_ms: i64,
    pub checkpoint_id: Option<CheckpointId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: CheckpointId,
    pub agent_run_id: RunId,
    pub state: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_starts_with_one_exploration_and_origin_fact() {
        let task = Task::new("forced mate", 4);
        assert_eq!(task.state, TaskState::Created);
        assert_eq!(task.origin_fact().state, FactState::Origin);
        assert!(!task.exploration_id.is_nil());
    }

    #[test]
    fn task_transition_is_checked() {
        let mut task = Task::new("goal", 1);
        task.transition(TaskState::Running).unwrap();
        task.transition(TaskState::Done).unwrap();
        assert!(task.transition(TaskState::Paused).is_err());
    }

    #[test]
    fn cancelled_is_terminal_and_can_reopen() {
        assert_eq!(TaskState::Cancelled.as_str(), "cancelled");
        assert_eq!(
            serde_json::to_string(&TaskState::Cancelled).unwrap(),
            "\"cancelled\""
        );
        assert!(TaskState::Cancelled.is_terminal());
        assert!(TaskState::Cancelled.can_transition_to(TaskState::Running));

        for state in [TaskState::Created, TaskState::Running, TaskState::Paused] {
            assert!(
                state.can_transition_to(TaskState::Cancelled),
                "{state} must be stoppable"
            );
        }
        for state in [TaskState::Done, TaskState::Failed, TaskState::Timeout] {
            assert!(
                !state.can_transition_to(TaskState::Cancelled),
                "{state} is already terminal"
            );
        }
    }

    #[test]
    fn intent_requires_a_fact() {
        let task = Task::new("goal", 1);
        assert!(Intent::new(task.id, Uuid::nil(), "move").is_err());
        let intent = Intent::new(task.id, task.origin_fact_id, "move").unwrap();
        assert_eq!(intent.state, IntentState::Open);
    }

    #[test]
    fn intent_claim_is_single_step() {
        let task = Task::new("goal", 1);
        let mut intent = Intent::new(task.id, task.origin_fact_id, "move").unwrap();
        intent.claim("worker-1").unwrap();
        assert_eq!(intent.state, IntentState::Running);
        assert!(intent.claim("worker-2").is_err());
    }

    #[test]
    fn paused_intent_can_only_unpark() {
        assert!(IntentState::Paused.can_transition_to(IntentState::Open));
        assert!(IntentState::Paused.can_transition_to(IntentState::Running));
        assert!(!IntentState::Paused.can_transition_to(IntentState::Stopped));
        assert!(!IntentState::Paused.can_transition_to(IntentState::Done));
        assert!(!IntentState::Paused.can_transition_to(IntentState::Blocked));
        assert!(!IntentState::Paused.can_transition_to(IntentState::Deleted));
    }

    #[test]
    fn terminal_intent_states_are_final() {
        for state in [
            IntentState::Done,
            IntentState::Blocked,
            IntentState::Exhausted,
            IntentState::Stopped,
            IntentState::Deleted,
        ] {
            assert!(state.is_terminal(), "{state} must be terminal");
        }
        for state in [IntentState::Open, IntentState::Running, IntentState::Paused] {
            assert!(!state.is_terminal(), "{state} must not be terminal");
        }
    }
}
