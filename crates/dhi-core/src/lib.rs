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
pub mod lead;
pub mod orchestration;
pub mod project;
pub mod side_question;
pub mod team;
pub use graph::{GraphStore, GuardStore};
pub use lead::{
    LeadBudget, LeadDecision, LeadError, LeadInput, LeadLimits, LeadTask, LeadWorkerResult,
    LeadWorkerStatus,
};
pub use orchestration::{
    CapabilityLimits, IntentPlan, OrchestrationError, Subtask, Topology, WorkerSpec,
};
pub use project::{
    AGENTS_MD_FILE, AGENTS_MD_MAX_BYTES, AgentsMdContent, AgentsMdState, agents_md_path,
    inspect_agents_md, read_agents_md,
};
pub use side_question::{
    NewSideQuestionRequest, SideQuestionRequest, SideQuestionSession, SideQuestionSessionStatus,
    SideQuestionStatus, SideQuestionThread,
};
pub use team::{
    MemoryTeamStore, MissionLogEntry, MissionLogKind, NewMissionLogEntry, NewTeamMessage,
    NewTeamOutcome, NewTeamOutcomeFragment, NewTeamRun, NewTeamTask, TeamActionKind, TeamError,
    TeamMember, TeamMemberSpec, TeamMemberStatus, TeamMessage, TeamOutcome, TeamOutcomeFragment,
    TeamOutcomeFragmentStatus, TeamOutcomeStatus, TeamPermissions, TeamRole, TeamRun,
    TeamRunProgress, TeamRunStatus, TeamSnapshot, TeamStore, TeamTask, TeamTaskStatus,
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
pub type ScaleApprovalId = Uuid;

/// Smallest fixed swarm a create request accepts. N counts teammates; the
/// lead is always one extra agent.
pub const SWARM_MIN_WORKERS: usize = 2;
/// Hard cap on swarm teammates, including a granted `swarm_scale` request.
pub const SWARM_MAX_WORKERS: usize = 64;
/// Teammates an Auto swarm may use without a `swarm_scale` approval.
pub const SWARM_AUTO_WORKERS: usize = 8;
/// Default lifetime of a pending `swarm_scale` approval, in seconds. The
/// configured approval timeout wins when the settings row exists.
pub const SCALE_APPROVAL_TIMEOUT_SECS: u64 = 15 * 60;

/// True for the fixed swarm worker counts the API, planner, and runtime share.
pub fn valid_swarm_worker_count(count: usize) -> bool {
    (SWARM_MIN_WORKERS..=SWARM_MAX_WORKERS).contains(&count)
}

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

/// One task's DHI+ capacity ledger.
///
/// Every field is optional: `None` means the engine keeps its own default
/// (rounds) or applies no limit of that kind (model calls, wall time, workers).
/// The store keeps the whole value in one JSONB column so an extension is one
/// atomic row write.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskCapacity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rounds: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_model_calls: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_wall_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_parallel_workers: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub goal: String,
    pub state: TaskState,
    pub exploration_id: Uuid,
    pub origin_fact_id: FactId,
    /// Optional per-task planner profile pin. `None` follows the role default
    /// chain. Stored in `tasks.pinned_planner_profile_id`.
    pub pinned_planner_profile_id: Option<i64>,
    /// Optional per-task worker profile pin. `None` follows the role default
    /// chain. Stored in `tasks.pinned_worker_profile_id`.
    pub pinned_worker_profile_id: Option<i64>,
    /// Optional capacity limits. Stored in `tasks.capacity`.
    pub capacity: TaskCapacity,
    /// Required orchestration topology, chosen at creation and immutable
    /// afterwards. `None` lets the planner decide whether to attach a plan.
    /// Stored in `tasks.topology`.
    #[serde(default)]
    pub topology: Option<Topology>,
    /// Required swarm worker count, chosen at creation and immutable
    /// afterwards. Only meaningful with [`Topology::Swarm`]; `None` leaves the
    /// count to the planner. Stored in `tasks.swarm_workers`.
    #[serde(default)]
    pub swarm_workers: Option<usize>,
    /// Canonical project folder selected at creation. `None` is legacy mode:
    /// the task keeps its isolated folder under `DHI_WORKSPACE_ROOT`.
    #[serde(default)]
    pub project_folder: Option<String>,
    /// Uploaded browser-folder web project this task works against. `None` for
    /// host-folder and legacy tasks. Stored in `tasks.web_project_id`.
    #[serde(default)]
    pub web_project_id: Option<Uuid>,
    /// Display name of the selected web project, copied at creation for audit.
    /// Stored in `tasks.web_project_name`.
    #[serde(default)]
    pub web_project_name: Option<String>,
    /// Derived host path of the task folder. Project mode is
    /// `<project>/.dhi/<task-id>`; legacy mode leaves this `None` because the
    /// folder is rooted at `DHI_WORKSPACE_ROOT` instead of the task row.
    #[serde(default)]
    pub task_folder: Option<String>,
    /// Whether the planner receives the project's `AGENTS.md` instructions.
    #[serde(default)]
    pub load_agents_md: bool,
    /// The instruction-file state computed at creation and refreshed when the
    /// engine reads it at task start.
    #[serde(default)]
    pub agents_md_state: AgentsMdState,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Creation-time task fields the store writes as one row.
///
/// This is deliberately a small input struct: the store create call already
/// carries pins, topology, and the project/instructions selection.
#[derive(Debug, Clone, Default)]
pub struct NewTask {
    /// Optional preallocated id. The API uses it to create the project task
    /// folder before the row exists; `None` lets the store allocate one.
    pub id: Option<TaskId>,
    pub goal: String,
    pub planner_pin: Option<i64>,
    pub worker_pin: Option<i64>,
    pub topology: Option<Topology>,
    pub swarm_workers: Option<usize>,
    pub project_folder: Option<String>,
    pub web_project_id: Option<Uuid>,
    pub web_project_name: Option<String>,
    pub load_agents_md: bool,
    pub agents_md_state: AgentsMdState,
}

impl NewTask {
    pub fn new(goal: impl Into<String>) -> Self {
        Self {
            id: None,
            goal: goal.into(),
            planner_pin: None,
            worker_pin: None,
            topology: None,
            swarm_workers: None,
            project_folder: None,
            web_project_id: None,
            web_project_name: None,
            load_agents_md: false,
            agents_md_state: AgentsMdState::Disabled,
        }
    }

    pub fn with_id(mut self, id: TaskId) -> Self {
        self.id = Some(id);
        self
    }

    pub fn with_pins(mut self, planner_pin: Option<i64>, worker_pin: Option<i64>) -> Self {
        self.planner_pin = planner_pin;
        self.worker_pin = worker_pin;
        self
    }

    pub fn with_topology(mut self, topology: Option<Topology>) -> Self {
        self.topology = topology;
        self
    }

    pub fn with_swarm_workers(mut self, swarm_workers: Option<usize>) -> Self {
        self.swarm_workers = swarm_workers;
        self
    }

    pub fn with_project(
        mut self,
        project_folder: impl Into<String>,
        load_agents_md: bool,
        agents_md_state: AgentsMdState,
    ) -> Self {
        self.project_folder = Some(project_folder.into());
        self.load_agents_md = load_agents_md;
        self.agents_md_state = agents_md_state;
        self
    }

    /// Selects an uploaded web project. The caller resolves the stored root
    /// and passes the absolute project folder separately.
    pub fn with_web_project(
        mut self,
        web_project_id: Uuid,
        web_project_name: impl Into<String>,
    ) -> Self {
        self.web_project_id = Some(web_project_id);
        self.web_project_name = Some(web_project_name.into());
        self
    }
}

impl Task {
    pub fn new(goal: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            goal: goal.into(),
            state: TaskState::Created,
            exploration_id: Uuid::new_v4(),
            origin_fact_id: Uuid::new_v4(),
            pinned_planner_profile_id: None,
            pinned_worker_profile_id: None,
            capacity: TaskCapacity::default(),
            topology: None,
            swarm_workers: None,
            project_folder: None,
            web_project_id: None,
            web_project_name: None,
            task_folder: None,
            load_agents_md: false,
            agents_md_state: AgentsMdState::Disabled,
            created_at: now,
            updated_at: now,
        }
    }

    /// The project-mode task folder for `project_folder` and `task_id`.
    ///
    /// Legacy tasks have no derived folder: their workspace is rooted under
    /// `DHI_WORKSPACE_ROOT`, which the store does not own.
    pub fn task_folder_for(project_folder: Option<&str>, task_id: TaskId) -> Option<String> {
        project_folder.map(|project| {
            std::path::Path::new(project)
                .join(".dhi")
                .join(task_id.to_string())
                .to_string_lossy()
                .into_owned()
        })
    }

    /// Recomputes [`Task::task_folder`] from the current project selection.
    pub fn refresh_task_folder(&mut self) {
        self.task_folder = Self::task_folder_for(self.project_folder.as_deref(), self.id);
    }

    pub fn origin_fact(&self) -> Fact {
        Fact {
            id: self.origin_fact_id,
            task_id: self.id,
            claim: format!("Origin: {}", self.goal),
            state: FactState::Origin,
            provenance: serde_json::json!({
                "goal": self.goal,
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

/// One human instruction for the planner of a task.
///
/// Instructions are read only by the planner round that picks them up; they
/// are never injected into worker prompts. `delivered_at` is the moment the
/// planner call that saw the text returned, so a restart never re-sends a
/// delivered instruction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskInstruction {
    pub id: Uuid,
    pub task_id: TaskId,
    pub text: String,
    pub created_at: DateTime<Utc>,
    pub delivered_at: Option<DateTime<Utc>>,
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

/// Lifecycle of one `swarm_scale` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScaleApprovalStatus {
    Pending,
    Granted,
    Denied,
    Expired,
}

impl ScaleApprovalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Granted => "granted",
            Self::Denied => "denied",
            Self::Expired => "expired",
        }
    }

    /// Only a granted request raises the swarm cap. Denial and expiry are both
    /// a denial for the caller.
    pub fn is_granted(self) -> bool {
        self == Self::Granted
    }
}

impl std::fmt::Display for ScaleApprovalStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One `swarm_scale` request: an Auto swarm asking to exceed
/// [`SWARM_AUTO_WORKERS`], up to [`SWARM_MAX_WORKERS`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScaleApproval {
    pub id: ScaleApprovalId,
    pub task_id: TaskId,
    /// Team the request belongs to, when the team row already exists.
    #[serde(default)]
    pub team_id: Option<Uuid>,
    pub requested_workers: usize,
    /// The cap the caller may already use without a grant.
    pub current_cap: usize,
    pub reason: String,
    pub status: ScaleApprovalStatus,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub decided_at: Option<DateTime<Utc>>,
}

impl ScaleApproval {
    /// The teammate cap this outcome allows. Only a grant raises it to the
    /// requested count; pending, denied, and expired all keep the current cap.
    pub fn effective_cap(&self) -> usize {
        if self.status.is_granted() {
            self.requested_workers
        } else {
            self.current_cap
        }
    }
}

/// Input for one `swarm_scale` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScaleApprovalRequest {
    pub task_id: TaskId,
    #[serde(default)]
    pub team_id: Option<Uuid>,
    pub requested_workers: usize,
    pub current_cap: usize,
    pub reason: String,
    /// `None` uses the configured approval timeout, then
    /// [`SCALE_APPROVAL_TIMEOUT_SECS`].
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// The seam the team runner calls for `swarm_scale`.
///
/// `request_scale_approval` persists one pending row. `wait_for_scale_approval`
/// blocks until the row is granted, denied, or expired. Every non-granted
/// outcome must keep the swarm at [`SWARM_AUTO_WORKERS`]; a fixed swarm never
/// calls this gate at all.
///
/// Implemented by `dhi_store::Store`; the team runtime only needs this
/// `dhi-core` trait, not the PostgreSQL store type.
#[async_trait::async_trait]
pub trait ScaleApprovalGate: Send + Sync {
    /// Creates one pending `swarm_scale` request.
    async fn request_scale_approval(
        &self,
        request: ScaleApprovalRequest,
    ) -> anyhow::Result<ScaleApproval>;

    /// Waits until the request leaves `pending`. Expiry counts as a denial.
    async fn wait_for_scale_approval(
        &self,
        approval_id: ScaleApprovalId,
    ) -> anyhow::Result<ScaleApproval>;

    /// Requests one approval and waits for its outcome.
    async fn request_and_wait_scale_approval(
        &self,
        request: ScaleApprovalRequest,
    ) -> anyhow::Result<ScaleApproval> {
        let approval = self.request_scale_approval(request).await?;
        self.wait_for_scale_approval(approval.id).await
    }
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
    /// DHI+ subworker identity that made this call; `None` for planner and
    /// single-worker calls.
    #[serde(default)]
    pub worker_key: Option<String>,
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
        let task = Task::new("forced mate");
        assert_eq!(task.state, TaskState::Created);
        assert_eq!(task.origin_fact().state, FactState::Origin);
        assert!(!task.exploration_id.is_nil());
    }

    #[test]
    fn new_task_defaults_to_legacy_mode() {
        let task = Task::new("legacy");
        assert_eq!(task.project_folder, None);
        assert_eq!(task.task_folder, None);
        assert!(!task.load_agents_md);
        assert_eq!(task.agents_md_state, AgentsMdState::Disabled);

        let mut project = Task::new("project");
        project.project_folder = Some("/srv/project".to_string());
        project.refresh_task_folder();
        let expected = std::path::Path::new("/srv/project")
            .join(".dhi")
            .join(project.id.to_string())
            .to_string_lossy()
            .into_owned();
        assert_eq!(project.task_folder.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn task_transition_is_checked() {
        let mut task = Task::new("goal");
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
        let task = Task::new("goal");
        assert!(Intent::new(task.id, Uuid::nil(), "move").is_err());
        let intent = Intent::new(task.id, task.origin_fact_id, "move").unwrap();
        assert_eq!(intent.state, IntentState::Open);
    }

    #[test]
    fn intent_claim_is_single_step() {
        let task = Task::new("goal");
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
