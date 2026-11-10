//! Shared team state for the Swarm (Cline Teams) topology.
//!
//! A team belongs to one DHI+ task. The coordinator (persisted as
//! [`TeamRole::Lead`]) owns the outcome; teammates work the task board,
//! exchange mail, write the mission log, and attach outcome fragments. Nothing
//! in this module writes the DHI+ graph: goals, intents, facts, and task state
//! remain engine-owned.
//!
//! The [`TeamStore`] trait is the persistence seam. [`MemoryTeamStore`] is the
//! deterministic in-process implementation used by runner tests; the
//! PostgreSQL implementation lives in `dhi-store`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::TaskId;

pub type TeamId = TaskId;
pub type TeamTaskId = Uuid;
pub type TeamMessageId = i64;
pub type MissionLogId = i64;
pub type TeamRunId = Uuid;
pub type TeamOutcomeId = Uuid;
pub type TeamOutcomeFragmentId = Uuid;

/// Highest teammate count the Swarm topology accepts. The lead is extra.
pub const MAX_TEAMMATES: usize = 64;

/// One member of a team: the coordinator (persisted as [`TeamRole::Lead`]) or
/// a teammate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMember {
    pub team_id: TeamId,
    pub agent_id: String,
    pub role: TeamRole,
    pub status: TeamMemberStatus,
    pub description: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TeamMember {
    pub fn new(
        team_id: TeamId,
        agent_id: impl Into<String>,
        role: TeamRole,
        description: impl Into<String>,
    ) -> Self {
        let now = Utc::now();
        Self {
            team_id,
            agent_id: agent_id.into(),
            role,
            status: TeamMemberStatus::Active,
            description: description.into(),
            created_at: now,
            updated_at: now,
        }
    }

    pub fn is_lead(&self) -> bool {
        self.role == TeamRole::Lead
    }
}

/// The two team roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamRole {
    Lead,
    Teammate,
}

impl TeamRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lead => "lead",
            Self::Teammate => "teammate",
        }
    }

    pub const ALL: [Self; 2] = [Self::Lead, Self::Teammate];

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "lead" => Some(Self::Lead),
            "teammate" => Some(Self::Teammate),
            _ => None,
        }
    }

    /// Whether this role may perform `action`.
    ///
    /// The coordinator (persisted as [`TeamRole::Lead`]) may do everything.
    /// A teammate may create, claim, complete, and block tasks; run tasks;
    /// message peers; log mission entries; create/review outcome fragments;
    /// and stop. Spawning, shutdown, cleanup, outcome finalization, and the
    /// final fact are coordinator-only.
    pub fn can(self, action: TeamActionKind) -> bool {
        match self {
            Self::Lead => true,
            Self::Teammate => matches!(
                action,
                TeamActionKind::CreateTask
                    | TeamActionKind::ClaimTask
                    | TeamActionKind::CompleteTask
                    | TeamActionKind::BlockTask
                    | TeamActionKind::RunTask
                    | TeamActionKind::SendMessage
                    | TeamActionKind::Broadcast
                    | TeamActionKind::MissionLog
                    | TeamActionKind::CreateOutcome
                    | TeamActionKind::AttachOutcomeFragment
                    | TeamActionKind::ReviewOutcomeFragment
                    | TeamActionKind::Stop
            ),
        }
    }

    pub fn can_perform(self, action: TeamActionKind) -> bool {
        self.can(action)
    }
}

impl std::fmt::Display for TeamRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Lifecycle state of one team member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamMemberStatus {
    Active,
    Idle,
    Stopped,
}

impl TeamMemberStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Idle => "idle",
            Self::Stopped => "stopped",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "active" => Some(Self::Active),
            "idle" => Some(Self::Idle),
            "stopped" => Some(Self::Stopped),
            _ => None,
        }
    }

    pub fn is_runnable(self) -> bool {
        self != Self::Stopped
    }
}

/// One card on the shared task board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamTask {
    pub id: TeamTaskId,
    pub team_id: TeamId,
    pub title: String,
    pub description: String,
    pub status: TeamTaskStatus,
    pub created_by: String,
    pub assignee: Option<String>,
    /// Task ids that must be `completed` before this task can start.
    pub depends_on: Vec<TeamTaskId>,
    pub summary: String,
    pub blocked_reason: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TeamTask {
    pub fn new(
        team_id: TeamId,
        title: impl Into<String>,
        description: impl Into<String>,
        created_by: impl Into<String>,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            team_id,
            title: title.into(),
            description: description.into(),
            status: TeamTaskStatus::Pending,
            created_by: created_by.into(),
            assignee: None,
            depends_on: Vec::new(),
            summary: String::new(),
            blocked_reason: String::new(),
            created_at: now,
            updated_at: now,
        }
    }

    /// Dependencies that are not completed yet, including ids that are missing
    /// from `tasks` (a missing dependency can never be satisfied).
    pub fn unmet_dependencies(&self, tasks: &[TeamTask]) -> Vec<TeamTaskId> {
        self.depends_on
            .iter()
            .copied()
            .filter(|dependency| {
                !tasks
                    .iter()
                    .any(|task| task.id == *dependency && task.status == TeamTaskStatus::Completed)
            })
            .collect()
    }

    /// Dependencies that are still incomplete, as full cards for the UI's
    /// `blocked-by` list.
    pub fn blocked_by<'a>(&self, tasks: &'a [TeamTask]) -> Vec<&'a TeamTask> {
        self.depends_on
            .iter()
            .filter_map(|dependency| {
                tasks
                    .iter()
                    .find(|task| task.id == *dependency && task.status != TeamTaskStatus::Completed)
            })
            .collect()
    }

    /// A pending task whose dependencies are all completed is ready to claim.
    pub fn is_ready(&self, tasks: &[TeamTask]) -> bool {
        self.status == TeamTaskStatus::Pending && self.unmet_dependencies(tasks).is_empty()
    }

    pub fn is_completed(&self) -> bool {
        self.status == TeamTaskStatus::Completed
    }

    pub fn is_blocked(&self, tasks: &[TeamTask]) -> bool {
        self.status == TeamTaskStatus::Blocked || !self.unmet_dependencies(tasks).is_empty()
    }
}

/// Every task on the board whose dependencies are complete.
pub fn ready_team_tasks(tasks: &[TeamTask]) -> Vec<&TeamTask> {
    tasks.iter().filter(|task| task.is_ready(tasks)).collect()
}

/// Board status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamTaskStatus {
    Pending,
    InProgress,
    Blocked,
    Completed,
}

impl TeamTaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Blocked => "blocked",
            Self::Completed => "completed",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "pending" => Some(Self::Pending),
            "in_progress" => Some(Self::InProgress),
            "blocked" => Some(Self::Blocked),
            "completed" => Some(Self::Completed),
            _ => None,
        }
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Pending, Self::InProgress)
                | (Self::Pending, Self::Blocked)
                | (Self::InProgress, Self::Completed)
                | (Self::InProgress, Self::Blocked)
                | (Self::Blocked, Self::Pending)
        )
    }
}

/// One mailbox message. `to_agent_id = None` is a team broadcast.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMessage {
    pub id: TeamMessageId,
    pub team_id: TeamId,
    pub from_agent_id: String,
    pub to_agent_id: Option<String>,
    pub subject: String,
    pub body: String,
    pub task_id: Option<TeamTaskId>,
    pub sent_at: DateTime<Utc>,
    pub read_at: Option<DateTime<Utc>>,
}

impl TeamMessage {
    pub fn is_broadcast(&self) -> bool {
        self.to_agent_id.is_none()
    }

    pub fn is_unread(&self) -> bool {
        self.read_at.is_none()
    }
}

/// Mission log entry kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionLogKind {
    Progress,
    Handoff,
    Blocked,
    Decision,
    Done,
    Error,
}

impl MissionLogKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Progress => "progress",
            Self::Handoff => "handoff",
            Self::Blocked => "blocked",
            Self::Decision => "decision",
            Self::Done => "done",
            Self::Error => "error",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "progress" => Some(Self::Progress),
            "handoff" => Some(Self::Handoff),
            "blocked" => Some(Self::Blocked),
            "decision" => Some(Self::Decision),
            "done" => Some(Self::Done),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

/// One append-only mission-log entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionLogEntry {
    pub id: MissionLogId,
    pub team_id: TeamId,
    pub agent_id: String,
    pub task_id: Option<TeamTaskId>,
    pub kind: MissionLogKind,
    pub summary: String,
    pub evidence: String,
    pub next_action: String,
    pub created_at: DateTime<Utc>,
}

/// One teammate run: queued, running, or terminal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamRun {
    pub id: TeamRunId,
    pub team_id: TeamId,
    pub agent_id: String,
    pub task_id: Option<TeamTaskId>,
    pub status: TeamRunStatus,
    pub message: String,
    pub priority: i32,
    pub retry_count: i32,
    pub max_retries: i32,
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub continue_conversation: bool,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub lease_owner: Option<String>,
    pub heartbeat_at: Option<DateTime<Utc>>,
    pub last_progress_at: Option<DateTime<Utc>>,
    pub last_progress_message: String,
    pub current_activity: String,
    pub result: Option<String>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TeamRun {
    pub fn is_terminal(&self) -> bool {
        self.status.is_terminal()
    }

    pub fn is_active(&self) -> bool {
        matches!(self.status, TeamRunStatus::Queued | TeamRunStatus::Running)
    }
}

/// Run lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamRunStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl TeamRunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "queued" => Some(Self::Queued),
            "running" => Some(Self::Running),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            "interrupted" => Some(Self::Interrupted),
            _ => None,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Queued, Self::Running)
                | (Self::Queued, Self::Failed)
                | (Self::Queued, Self::Cancelled)
                | (Self::Running, Self::Completed)
                | (Self::Running, Self::Failed)
                | (Self::Running, Self::Cancelled)
                | (Self::Running, Self::Interrupted)
                | (Self::Interrupted, Self::Queued)
                | (Self::Interrupted, Self::Running)
                | (Self::Interrupted, Self::Cancelled)
        )
    }
}

/// Heartbeat update for one running teammate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamRunProgress {
    pub lease_owner: String,
    pub message: String,
    pub activity: String,
}

impl TeamRunProgress {
    pub fn new(
        lease_owner: impl Into<String>,
        message: impl Into<String>,
        activity: impl Into<String>,
    ) -> Self {
        Self {
            lease_owner: lease_owner.into(),
            message: message.into(),
            activity: activity.into(),
        }
    }
}

/// A team outcome under construction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamOutcome {
    pub id: TeamOutcomeId,
    pub team_id: TeamId,
    pub title: String,
    pub status: TeamOutcomeStatus,
    /// Sections the finalized outcome must cover with reviewed fragments.
    pub required_sections: Vec<String>,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub finalized_at: Option<DateTime<Utc>>,
}

impl TeamOutcome {
    pub fn new(team_id: TeamId, title: impl Into<String>, created_by: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            team_id,
            title: title.into(),
            status: TeamOutcomeStatus::Draft,
            required_sections: Vec::new(),
            created_by: created_by.into(),
            created_at: Utc::now(),
            finalized_at: None,
        }
    }

    /// Required sections that have no `reviewed` fragment yet.
    ///
    /// An outcome with no required sections still needs at least one reviewed
    /// fragment: an empty final artifact is never acceptable.
    pub fn missing_sections(&self, fragments: &[TeamOutcomeFragment]) -> Vec<String> {
        let reviewed = |section: &str| {
            fragments.iter().any(|fragment| {
                fragment.status == TeamOutcomeFragmentStatus::Reviewed
                    && fragment.section.as_str() == section
            })
        };

        if self.required_sections.is_empty() {
            if fragments
                .iter()
                .any(|fragment| fragment.status == TeamOutcomeFragmentStatus::Reviewed)
            {
                Vec::new()
            } else {
                vec!["<any reviewed section>".to_string()]
            }
        } else {
            self.required_sections
                .iter()
                .filter(|section| !reviewed(section.as_str()))
                .cloned()
                .collect()
        }
    }

    pub fn is_ready_to_finalize(&self, fragments: &[TeamOutcomeFragment]) -> bool {
        self.status != TeamOutcomeStatus::Finalized && self.missing_sections(fragments).is_empty()
    }
}

/// Outcome lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamOutcomeStatus {
    Draft,
    InReview,
    Finalized,
}

impl TeamOutcomeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::InReview => "in_review",
            Self::Finalized => "finalized",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "draft" => Some(Self::Draft),
            "in_review" => Some(Self::InReview),
            "finalized" => Some(Self::Finalized),
            _ => None,
        }
    }
}

/// One drafted section of an outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamOutcomeFragment {
    pub id: TeamOutcomeFragmentId,
    pub team_id: TeamId,
    pub outcome_id: TeamOutcomeId,
    pub section: String,
    pub source_agent_id: String,
    pub source_run_id: Option<TeamRunId>,
    pub content: String,
    pub status: TeamOutcomeFragmentStatus,
    pub reviewed_by: Option<String>,
    pub reviewed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Fragment review state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamOutcomeFragmentStatus {
    Draft,
    Reviewed,
    Rejected,
}

impl TeamOutcomeFragmentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Reviewed => "reviewed",
            Self::Rejected => "rejected",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "draft" => Some(Self::Draft),
            "reviewed" => Some(Self::Reviewed),
            "rejected" => Some(Self::Rejected),
            _ => None,
        }
    }
}

/// The typed team action vocabulary the tool host applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamActionKind {
    CreateTask,
    ClaimTask,
    CompleteTask,
    BlockTask,
    RunTask,
    SendMessage,
    Broadcast,
    MissionLog,
    CreateOutcome,
    AttachOutcomeFragment,
    ReviewOutcomeFragment,
    FinalizeOutcome,
    SpawnTeammate,
    ShutdownTeammate,
    Cleanup,
    Stop,
    Final,
}

impl TeamActionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CreateTask => "create_task",
            Self::ClaimTask => "claim_task",
            Self::CompleteTask => "complete_task",
            Self::BlockTask => "block_task",
            Self::RunTask => "run_task",
            Self::SendMessage => "send_message",
            Self::Broadcast => "broadcast",
            Self::MissionLog => "mission_log",
            Self::CreateOutcome => "create_outcome",
            Self::AttachOutcomeFragment => "attach_outcome_fragment",
            Self::ReviewOutcomeFragment => "review_outcome_fragment",
            Self::FinalizeOutcome => "finalize_outcome",
            Self::SpawnTeammate => "spawn_teammate",
            Self::ShutdownTeammate => "shutdown_teammate",
            Self::Cleanup => "cleanup",
            Self::Stop => "stop",
            Self::Final => "final",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "create_task" => Some(Self::CreateTask),
            "claim_task" => Some(Self::ClaimTask),
            "complete_task" => Some(Self::CompleteTask),
            "block_task" => Some(Self::BlockTask),
            "run_task" => Some(Self::RunTask),
            "send_message" => Some(Self::SendMessage),
            "broadcast" => Some(Self::Broadcast),
            "mission_log" => Some(Self::MissionLog),
            "create_outcome" => Some(Self::CreateOutcome),
            "attach_outcome_fragment" => Some(Self::AttachOutcomeFragment),
            "review_outcome_fragment" => Some(Self::ReviewOutcomeFragment),
            "finalize_outcome" => Some(Self::FinalizeOutcome),
            "spawn_teammate" => Some(Self::SpawnTeammate),
            "shutdown_teammate" => Some(Self::ShutdownTeammate),
            "cleanup" => Some(Self::Cleanup),
            "stop" => Some(Self::Stop),
            "final" => Some(Self::Final),
            _ => None,
        }
    }

    /// Actions only the coordinator may perform.
    pub fn is_lead_only(self) -> bool {
        !TeamRole::Teammate.can(self)
    }
}

impl std::fmt::Display for TeamActionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Role-based permission check used before any action is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TeamPermissions {
    pub role: TeamRole,
}

impl TeamPermissions {
    pub fn new(role: TeamRole) -> Self {
        Self { role }
    }

    pub fn can(&self, action: TeamActionKind) -> bool {
        self.role.can(action)
    }

    pub fn require(&self, action: TeamActionKind) -> Result<(), TeamError> {
        if self.can(action) {
            Ok(())
        } else {
            Err(TeamError::PermissionDenied {
                role: self.role,
                action,
            })
        }
    }
}

/// A member declaration used to start or extend a team.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMemberSpec {
    pub agent_id: String,
    pub role: TeamRole,
    pub description: String,
    pub max_turns: usize,
}

impl TeamMemberSpec {
    /// The coordinator spec. The persisted role is `lead` for compatibility;
    /// this member is the Swarm coordinator agent.
    pub fn lead(agent_id: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            agent_id: agent_id.into(),
            role: TeamRole::Lead,
            description: description.into(),
            max_turns: 8,
        }
    }

    pub fn teammate(
        agent_id: impl Into<String>,
        description: impl Into<String>,
        max_turns: usize,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            role: TeamRole::Teammate,
            description: description.into(),
            max_turns,
        }
    }

    pub fn member(&self, team_id: TeamId) -> TeamMember {
        TeamMember::new(
            team_id,
            self.agent_id.clone(),
            self.role,
            self.description.clone(),
        )
    }
}

/// Everything one team member sees: roster, board, mailbox, mission log,
/// outcomes, fragments, and runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamSnapshot {
    pub team_id: TeamId,
    pub objective: String,
    pub members: Vec<TeamMember>,
    pub tasks: Vec<TeamTask>,
    pub inbox: Vec<TeamMessage>,
    pub mission_log: Vec<MissionLogEntry>,
    pub outcomes: Vec<TeamOutcome>,
    pub fragments: Vec<TeamOutcomeFragment>,
    pub runs: Vec<TeamRun>,
}

/// Creation payload for one board task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTeamTask {
    pub id: Option<TeamTaskId>,
    pub team_id: TeamId,
    pub title: String,
    pub description: String,
    pub created_by: String,
    pub assignee: Option<String>,
    pub depends_on: Vec<TeamTaskId>,
}

impl NewTeamTask {
    pub fn new(team_id: TeamId, title: impl Into<String>, created_by: impl Into<String>) -> Self {
        Self {
            id: None,
            team_id,
            title: title.into(),
            description: String::new(),
            created_by: created_by.into(),
            assignee: None,
            depends_on: Vec::new(),
        }
    }

    pub fn with_id(mut self, id: TeamTaskId) -> Self {
        self.id = Some(id);
        self
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    pub fn with_assignee(mut self, assignee: impl Into<String>) -> Self {
        self.assignee = Some(assignee.into());
        self
    }

    pub fn with_dependencies(mut self, depends_on: Vec<TeamTaskId>) -> Self {
        self.depends_on = depends_on;
        self
    }
}

/// Creation payload for one mailbox message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTeamMessage {
    pub team_id: TeamId,
    pub from_agent_id: String,
    pub to_agent_id: Option<String>,
    pub subject: String,
    pub body: String,
    pub task_id: Option<TeamTaskId>,
}

impl NewTeamMessage {
    pub fn direct(
        team_id: TeamId,
        from_agent_id: impl Into<String>,
        to_agent_id: impl Into<String>,
        subject: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            team_id,
            from_agent_id: from_agent_id.into(),
            to_agent_id: Some(to_agent_id.into()),
            subject: subject.into(),
            body: body.into(),
            task_id: None,
        }
    }

    pub fn broadcast(
        team_id: TeamId,
        from_agent_id: impl Into<String>,
        subject: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            team_id,
            from_agent_id: from_agent_id.into(),
            to_agent_id: None,
            subject: subject.into(),
            body: body.into(),
            task_id: None,
        }
    }

    pub fn with_task(mut self, task_id: TeamTaskId) -> Self {
        self.task_id = Some(task_id);
        self
    }
}

/// Creation payload for one mission-log entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMissionLogEntry {
    pub team_id: TeamId,
    pub agent_id: String,
    pub task_id: Option<TeamTaskId>,
    pub kind: MissionLogKind,
    pub summary: String,
    pub evidence: String,
    pub next_action: String,
}

impl NewMissionLogEntry {
    pub fn new(
        team_id: TeamId,
        agent_id: impl Into<String>,
        kind: MissionLogKind,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            team_id,
            agent_id: agent_id.into(),
            task_id: None,
            kind,
            summary: summary.into(),
            evidence: String::new(),
            next_action: String::new(),
        }
    }

    pub fn with_task(mut self, task_id: TeamTaskId) -> Self {
        self.task_id = Some(task_id);
        self
    }

    pub fn with_evidence(mut self, evidence: impl Into<String>) -> Self {
        self.evidence = evidence.into();
        self
    }

    pub fn with_next_action(mut self, next_action: impl Into<String>) -> Self {
        self.next_action = next_action.into();
        self
    }
}

/// Creation payload for one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTeamRun {
    pub id: Option<TeamRunId>,
    pub team_id: TeamId,
    pub agent_id: String,
    pub task_id: Option<TeamTaskId>,
    pub message: String,
    pub priority: i32,
    pub max_retries: i32,
    pub continue_conversation: bool,
    pub next_attempt_at: Option<DateTime<Utc>>,
}

impl NewTeamRun {
    pub fn new(team_id: TeamId, agent_id: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            id: None,
            team_id,
            agent_id: agent_id.into(),
            task_id: None,
            message: message.into(),
            priority: 0,
            max_retries: 0,
            continue_conversation: false,
            next_attempt_at: None,
        }
    }

    pub fn with_id(mut self, id: TeamRunId) -> Self {
        self.id = Some(id);
        self
    }

    pub fn with_task(mut self, task_id: TeamTaskId) -> Self {
        self.task_id = Some(task_id);
        self
    }

    pub fn with_priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }

    pub fn with_retries(mut self, max_retries: i32) -> Self {
        self.max_retries = max_retries;
        self
    }

    pub fn continue_conversation(mut self, continue_conversation: bool) -> Self {
        self.continue_conversation = continue_conversation;
        self
    }

    pub fn with_next_attempt_at(mut self, next_attempt_at: DateTime<Utc>) -> Self {
        self.next_attempt_at = Some(next_attempt_at);
        self
    }
}

/// Creation payload for one outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTeamOutcome {
    pub id: Option<TeamOutcomeId>,
    pub team_id: TeamId,
    pub title: String,
    pub required_sections: Vec<String>,
    pub created_by: String,
}

impl NewTeamOutcome {
    pub fn new(team_id: TeamId, title: impl Into<String>, created_by: impl Into<String>) -> Self {
        Self {
            id: None,
            team_id,
            title: title.into(),
            required_sections: Vec::new(),
            created_by: created_by.into(),
        }
    }

    pub fn with_id(mut self, id: TeamOutcomeId) -> Self {
        self.id = Some(id);
        self
    }

    pub fn with_sections(mut self, required_sections: Vec<String>) -> Self {
        self.required_sections = required_sections;
        self
    }
}

/// Creation payload for one outcome fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTeamOutcomeFragment {
    pub id: Option<TeamOutcomeFragmentId>,
    pub team_id: TeamId,
    pub outcome_id: TeamOutcomeId,
    pub section: String,
    pub source_agent_id: String,
    pub source_run_id: Option<TeamRunId>,
    pub content: String,
}

impl NewTeamOutcomeFragment {
    pub fn new(
        team_id: TeamId,
        outcome_id: TeamOutcomeId,
        section: impl Into<String>,
        source_agent_id: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            id: None,
            team_id,
            outcome_id,
            section: section.into(),
            source_agent_id: source_agent_id.into(),
            source_run_id: None,
            content: content.into(),
        }
    }

    pub fn with_id(mut self, id: TeamOutcomeFragmentId) -> Self {
        self.id = Some(id);
        self
    }

    pub fn with_run(mut self, run_id: TeamRunId) -> Self {
        self.source_run_id = Some(run_id);
        self
    }
}

/// Typed team persistence failure.
#[derive(Debug, Error)]
pub enum TeamError {
    #[error("team {team_id}: {what} was not found")]
    NotFound { team_id: TeamId, what: String },
    #[error("team {team_id}: conflict: {message}")]
    Conflict { team_id: TeamId, message: String },
    #[error("team {team_id}: invalid request: {message}")]
    Validation { team_id: TeamId, message: String },
    #[error("team action {action} is not allowed for role {role}")]
    PermissionDenied {
        role: TeamRole,
        action: TeamActionKind,
    },
    #[error("team store failure: {message}")]
    Storage { message: String },
}

impl TeamError {
    pub fn not_found(team_id: TeamId, what: impl Into<String>) -> Self {
        Self::NotFound {
            team_id,
            what: what.into(),
        }
    }

    pub fn conflict(team_id: TeamId, message: impl Into<String>) -> Self {
        Self::Conflict {
            team_id,
            message: message.into(),
        }
    }

    pub fn validation(team_id: TeamId, message: impl Into<String>) -> Self {
        Self::Validation {
            team_id,
            message: message.into(),
        }
    }

    pub fn storage(message: impl Into<String>) -> Self {
        Self::Storage {
            message: message.into(),
        }
    }
}

/// Persistence contract for a team's shared state.
///
/// Every mutation is a single logical operation. A store must reject invalid
/// status transitions instead of silently overwriting a terminal row.
#[async_trait]
pub trait TeamStore: Send + Sync {
    async fn upsert_team_member(&self, member: &TeamMember) -> Result<TeamMember, TeamError>;
    async fn set_team_member_status(
        &self,
        team_id: TeamId,
        agent_id: &str,
        status: TeamMemberStatus,
    ) -> Result<TeamMember, TeamError>;
    async fn team_member(
        &self,
        team_id: TeamId,
        agent_id: &str,
    ) -> Result<Option<TeamMember>, TeamError>;
    async fn team_members(&self, team_id: TeamId) -> Result<Vec<TeamMember>, TeamError>;

    async fn create_team_task(&self, task: &NewTeamTask) -> Result<TeamTask, TeamError>;
    async fn team_task(
        &self,
        team_id: TeamId,
        task_id: TeamTaskId,
    ) -> Result<Option<TeamTask>, TeamError>;
    async fn team_tasks(&self, team_id: TeamId) -> Result<Vec<TeamTask>, TeamError>;
    async fn claim_team_task(
        &self,
        team_id: TeamId,
        task_id: TeamTaskId,
        agent_id: &str,
    ) -> Result<TeamTask, TeamError>;
    async fn complete_team_task(
        &self,
        team_id: TeamId,
        task_id: TeamTaskId,
        summary: &str,
    ) -> Result<TeamTask, TeamError>;
    async fn block_team_task(
        &self,
        team_id: TeamId,
        task_id: TeamTaskId,
        reason: &str,
    ) -> Result<TeamTask, TeamError>;
    async fn reopen_team_task(
        &self,
        team_id: TeamId,
        task_id: TeamTaskId,
    ) -> Result<TeamTask, TeamError>;

    async fn send_team_message(&self, message: &NewTeamMessage) -> Result<TeamMessage, TeamError>;
    async fn broadcast_team_message(
        &self,
        message: &NewTeamMessage,
    ) -> Result<TeamMessage, TeamError>;
    async fn team_inbox(
        &self,
        team_id: TeamId,
        agent_id: &str,
    ) -> Result<Vec<TeamMessage>, TeamError>;
    async fn team_sent(
        &self,
        team_id: TeamId,
        agent_id: &str,
    ) -> Result<Vec<TeamMessage>, TeamError>;
    /// Every message in the team's mailbox, both directions, oldest first.
    async fn team_messages(&self, team_id: TeamId) -> Result<Vec<TeamMessage>, TeamError>;
    async fn mark_team_message_read(
        &self,
        team_id: TeamId,
        message_id: TeamMessageId,
        agent_id: &str,
    ) -> Result<TeamMessage, TeamError>;

    async fn append_mission_log(
        &self,
        entry: &NewMissionLogEntry,
    ) -> Result<MissionLogEntry, TeamError>;
    async fn team_mission_log(&self, team_id: TeamId) -> Result<Vec<MissionLogEntry>, TeamError>;

    async fn enqueue_team_run(&self, run: &NewTeamRun) -> Result<TeamRun, TeamError>;
    async fn start_team_run(
        &self,
        team_id: TeamId,
        run_id: TeamRunId,
        lease_owner: &str,
    ) -> Result<TeamRun, TeamError>;
    async fn heartbeat_team_run(
        &self,
        team_id: TeamId,
        run_id: TeamRunId,
        progress: &TeamRunProgress,
    ) -> Result<TeamRun, TeamError>;
    async fn finish_team_run(
        &self,
        team_id: TeamId,
        run_id: TeamRunId,
        status: TeamRunStatus,
        result: Option<&str>,
        error: Option<&str>,
    ) -> Result<TeamRun, TeamError>;
    async fn requeue_team_run(
        &self,
        team_id: TeamId,
        run_id: TeamRunId,
        message: Option<&str>,
    ) -> Result<TeamRun, TeamError>;
    async fn team_run(
        &self,
        team_id: TeamId,
        run_id: TeamRunId,
    ) -> Result<Option<TeamRun>, TeamError>;
    async fn team_runs(&self, team_id: TeamId) -> Result<Vec<TeamRun>, TeamError>;

    async fn create_team_outcome(&self, outcome: &NewTeamOutcome)
    -> Result<TeamOutcome, TeamError>;
    async fn team_outcome(
        &self,
        team_id: TeamId,
        outcome_id: TeamOutcomeId,
    ) -> Result<Option<TeamOutcome>, TeamError>;
    async fn team_outcomes(&self, team_id: TeamId) -> Result<Vec<TeamOutcome>, TeamError>;
    async fn attach_outcome_fragment(
        &self,
        fragment: &NewTeamOutcomeFragment,
    ) -> Result<TeamOutcomeFragment, TeamError>;
    async fn team_outcome_fragment(
        &self,
        team_id: TeamId,
        fragment_id: TeamOutcomeFragmentId,
    ) -> Result<Option<TeamOutcomeFragment>, TeamError>;
    async fn team_outcome_fragments(
        &self,
        team_id: TeamId,
        outcome_id: Option<TeamOutcomeId>,
    ) -> Result<Vec<TeamOutcomeFragment>, TeamError>;
    async fn review_outcome_fragment(
        &self,
        team_id: TeamId,
        fragment_id: TeamOutcomeFragmentId,
        status: TeamOutcomeFragmentStatus,
        reviewed_by: &str,
    ) -> Result<TeamOutcomeFragment, TeamError>;
    async fn finalize_team_outcome(
        &self,
        team_id: TeamId,
        outcome_id: TeamOutcomeId,
    ) -> Result<TeamOutcome, TeamError>;
}

#[derive(Debug, Default)]
struct MemoryTeamState {
    members: HashMap<(TeamId, String), TeamMember>,
    tasks: HashMap<TeamTaskId, TeamTask>,
    messages: Vec<TeamMessage>,
    next_message_id: TeamMessageId,
    mission_log: Vec<MissionLogEntry>,
    next_mission_log_id: MissionLogId,
    runs: HashMap<TeamRunId, TeamRun>,
    outcomes: HashMap<TeamOutcomeId, TeamOutcome>,
    fragments: HashMap<TeamOutcomeFragmentId, TeamOutcomeFragment>,
}

/// Deterministic in-process [`TeamStore`] used by runner and type tests.
#[derive(Debug, Clone, Default)]
pub struct MemoryTeamStore {
    state: Arc<Mutex<MemoryTeamState>>,
}

impl MemoryTeamStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, MemoryTeamState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Every stored member, every team. Tests use this to prove a failed
    /// action left no partial row.
    pub fn all_members(&self) -> Vec<TeamMember> {
        let state = self.lock();
        let mut members: Vec<TeamMember> = state.members.values().cloned().collect();
        members.sort_by(|left, right| {
            left.team_id
                .cmp(&right.team_id)
                .then_with(|| left.agent_id.cmp(&right.agent_id))
        });
        members
    }
}

#[async_trait]
impl TeamStore for MemoryTeamStore {
    async fn upsert_team_member(&self, member: &TeamMember) -> Result<TeamMember, TeamError> {
        if member.agent_id.trim().is_empty() {
            return Err(TeamError::validation(
                member.team_id,
                "member agent_id must not be empty",
            ));
        }
        let mut state = self.lock();
        let key = (member.team_id, member.agent_id.clone());
        let stored = match state.members.get(&key) {
            Some(existing) => {
                let mut updated = existing.clone();
                updated.role = member.role;
                updated.status = member.status;
                updated.description = member.description.clone();
                updated.updated_at = Utc::now();
                updated
            }
            None => member.clone(),
        };
        state.members.insert(key, stored.clone());
        Ok(stored)
    }

    async fn set_team_member_status(
        &self,
        team_id: TeamId,
        agent_id: &str,
        status: TeamMemberStatus,
    ) -> Result<TeamMember, TeamError> {
        let mut state = self.lock();
        let member = state
            .members
            .get_mut(&(team_id, agent_id.to_string()))
            .ok_or_else(|| TeamError::not_found(team_id, format!("member {agent_id}")))?;
        member.status = status;
        member.updated_at = Utc::now();
        Ok(member.clone())
    }

    async fn team_member(
        &self,
        team_id: TeamId,
        agent_id: &str,
    ) -> Result<Option<TeamMember>, TeamError> {
        Ok(self
            .lock()
            .members
            .get(&(team_id, agent_id.to_string()))
            .cloned())
    }

    async fn team_members(&self, team_id: TeamId) -> Result<Vec<TeamMember>, TeamError> {
        let state = self.lock();
        let mut members: Vec<TeamMember> = state
            .members
            .values()
            .filter(|member| member.team_id == team_id)
            .cloned()
            .collect();
        members.sort_by(|left, right| left.agent_id.cmp(&right.agent_id));
        Ok(members)
    }

    async fn create_team_task(&self, task: &NewTeamTask) -> Result<TeamTask, TeamError> {
        if task.title.trim().is_empty() {
            return Err(TeamError::validation(
                task.team_id,
                "team task title must not be empty",
            ));
        }
        let mut state = self.lock();
        let id = task.id.unwrap_or_else(Uuid::new_v4);
        if state.tasks.contains_key(&id) {
            return Err(TeamError::conflict(
                task.team_id,
                format!("team task {id} already exists"),
            ));
        }
        if task.depends_on.contains(&id) {
            return Err(TeamError::validation(
                task.team_id,
                "team task cannot depend on itself",
            ));
        }
        for dependency in &task.depends_on {
            match state.tasks.get(dependency) {
                Some(existing) if existing.team_id == task.team_id => {}
                _ => {
                    return Err(TeamError::validation(
                        task.team_id,
                        format!("unknown dependency {dependency}"),
                    ));
                }
            }
        }
        let now = Utc::now();
        let stored = TeamTask {
            id,
            team_id: task.team_id,
            title: task.title.trim().to_string(),
            description: task.description.trim().to_string(),
            status: TeamTaskStatus::Pending,
            created_by: task.created_by.clone(),
            assignee: task.assignee.clone(),
            depends_on: task.depends_on.clone(),
            summary: String::new(),
            blocked_reason: String::new(),
            created_at: now,
            updated_at: now,
        };
        state.tasks.insert(id, stored.clone());
        Ok(stored)
    }

    async fn team_task(
        &self,
        team_id: TeamId,
        task_id: TeamTaskId,
    ) -> Result<Option<TeamTask>, TeamError> {
        Ok(self
            .lock()
            .tasks
            .get(&task_id)
            .filter(|task| task.team_id == team_id)
            .cloned())
    }

    async fn team_tasks(&self, team_id: TeamId) -> Result<Vec<TeamTask>, TeamError> {
        let state = self.lock();
        let mut tasks: Vec<TeamTask> = state
            .tasks
            .values()
            .filter(|task| task.team_id == team_id)
            .cloned()
            .collect();
        tasks.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(tasks)
    }

    async fn claim_team_task(
        &self,
        team_id: TeamId,
        task_id: TeamTaskId,
        agent_id: &str,
    ) -> Result<TeamTask, TeamError> {
        let mut state = self.lock();
        let task = state
            .tasks
            .get(&task_id)
            .filter(|task| task.team_id == team_id)
            .cloned()
            .ok_or_else(|| TeamError::not_found(team_id, format!("team task {task_id}")))?;
        if task.status != TeamTaskStatus::Pending {
            return Err(TeamError::conflict(
                team_id,
                format!("team task {task_id} is {}", task.status.as_str()),
            ));
        }
        let all: Vec<TeamTask> = state
            .tasks
            .values()
            .filter(|candidate| candidate.team_id == team_id)
            .cloned()
            .collect();
        let unmet = task.unmet_dependencies(&all);
        if !unmet.is_empty() {
            return Err(TeamError::conflict(
                team_id,
                format!("team task {task_id} is blocked by {unmet:?}"),
            ));
        }
        if let Some(assignee) = &task.assignee
            && assignee.as_str() != agent_id
        {
            return Err(TeamError::conflict(
                team_id,
                format!("team task {task_id} is assigned to {assignee}"),
            ));
        }
        let stored = state.tasks.get_mut(&task_id).expect("task exists");
        stored.status = TeamTaskStatus::InProgress;
        stored.assignee = Some(agent_id.to_string());
        stored.updated_at = Utc::now();
        Ok(stored.clone())
    }

    async fn complete_team_task(
        &self,
        team_id: TeamId,
        task_id: TeamTaskId,
        summary: &str,
    ) -> Result<TeamTask, TeamError> {
        let mut state = self.lock();
        let task = state
            .tasks
            .get(&task_id)
            .filter(|task| task.team_id == team_id)
            .cloned()
            .ok_or_else(|| TeamError::not_found(team_id, format!("team task {task_id}")))?;
        if task.status != TeamTaskStatus::InProgress {
            return Err(TeamError::conflict(
                team_id,
                format!(
                    "team task {task_id} is {}; only an in-progress task can complete",
                    task.status.as_str()
                ),
            ));
        }
        let stored = state.tasks.get_mut(&task_id).expect("task exists");
        stored.status = TeamTaskStatus::Completed;
        stored.summary = summary.to_string();
        stored.blocked_reason.clear();
        stored.updated_at = Utc::now();
        Ok(stored.clone())
    }

    async fn block_team_task(
        &self,
        team_id: TeamId,
        task_id: TeamTaskId,
        reason: &str,
    ) -> Result<TeamTask, TeamError> {
        let mut state = self.lock();
        let task = state
            .tasks
            .get(&task_id)
            .filter(|task| task.team_id == team_id)
            .cloned()
            .ok_or_else(|| TeamError::not_found(team_id, format!("team task {task_id}")))?;
        if !matches!(
            task.status,
            TeamTaskStatus::Pending | TeamTaskStatus::InProgress
        ) {
            return Err(TeamError::conflict(
                team_id,
                format!(
                    "team task {task_id} is {}; only a pending or in-progress task can block",
                    task.status.as_str()
                ),
            ));
        }
        let stored = state.tasks.get_mut(&task_id).expect("task exists");
        stored.status = TeamTaskStatus::Blocked;
        stored.blocked_reason = reason.to_string();
        stored.updated_at = Utc::now();
        Ok(stored.clone())
    }

    async fn reopen_team_task(
        &self,
        team_id: TeamId,
        task_id: TeamTaskId,
    ) -> Result<TeamTask, TeamError> {
        let mut state = self.lock();
        let task = state
            .tasks
            .get(&task_id)
            .filter(|task| task.team_id == team_id)
            .cloned()
            .ok_or_else(|| TeamError::not_found(team_id, format!("team task {task_id}")))?;
        if task.status != TeamTaskStatus::Blocked {
            return Err(TeamError::conflict(
                team_id,
                format!(
                    "team task {task_id} is {}; only a blocked task can reopen",
                    task.status.as_str()
                ),
            ));
        }
        let stored = state.tasks.get_mut(&task_id).expect("task exists");
        stored.status = TeamTaskStatus::Pending;
        stored.assignee = None;
        stored.blocked_reason.clear();
        stored.updated_at = Utc::now();
        Ok(stored.clone())
    }

    async fn send_team_message(&self, message: &NewTeamMessage) -> Result<TeamMessage, TeamError> {
        if message.to_agent_id.is_none() {
            return Err(TeamError::validation(
                message.team_id,
                "a direct message needs a recipient; use broadcast for a team-wide message",
            ));
        }
        self.store_message(message)
    }

    async fn broadcast_team_message(
        &self,
        message: &NewTeamMessage,
    ) -> Result<TeamMessage, TeamError> {
        let mut broadcast = message.clone();
        broadcast.to_agent_id = None;
        self.store_message(&broadcast)
    }

    async fn team_inbox(
        &self,
        team_id: TeamId,
        agent_id: &str,
    ) -> Result<Vec<TeamMessage>, TeamError> {
        let state = self.lock();
        let mut inbox: Vec<TeamMessage> = state
            .messages
            .iter()
            .filter(|message| {
                message.team_id == team_id
                    && (message.to_agent_id.as_deref() == Some(agent_id)
                        || (message.is_broadcast() && message.from_agent_id.as_str() != agent_id))
            })
            .cloned()
            .collect();
        inbox.sort_by(|left, right| {
            left.sent_at
                .cmp(&right.sent_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(inbox)
    }

    async fn team_sent(
        &self,
        team_id: TeamId,
        agent_id: &str,
    ) -> Result<Vec<TeamMessage>, TeamError> {
        let state = self.lock();
        let mut sent: Vec<TeamMessage> = state
            .messages
            .iter()
            .filter(|message| {
                message.team_id == team_id && message.from_agent_id.as_str() == agent_id
            })
            .cloned()
            .collect();
        sent.sort_by(|left, right| {
            left.sent_at
                .cmp(&right.sent_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(sent)
    }

    async fn team_messages(&self, team_id: TeamId) -> Result<Vec<TeamMessage>, TeamError> {
        let state = self.lock();
        let mut messages: Vec<TeamMessage> = state
            .messages
            .iter()
            .filter(|message| message.team_id == team_id)
            .cloned()
            .collect();
        messages.sort_by(|left, right| {
            left.sent_at
                .cmp(&right.sent_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(messages)
    }

    async fn mark_team_message_read(
        &self,
        team_id: TeamId,
        message_id: TeamMessageId,
        agent_id: &str,
    ) -> Result<TeamMessage, TeamError> {
        let mut state = self.lock();
        let message = state
            .messages
            .iter_mut()
            .find(|message| message.team_id == team_id && message.id == message_id)
            .ok_or_else(|| TeamError::not_found(team_id, format!("message {message_id}")))?;
        let recipient = message.to_agent_id.as_deref();
        if recipient != Some(agent_id) && !message.is_broadcast() {
            return Err(TeamError::conflict(
                team_id,
                format!("message {message_id} is not addressed to {agent_id}"),
            ));
        }
        if message.read_at.is_none() {
            message.read_at = Some(Utc::now());
        }
        Ok(message.clone())
    }

    async fn append_mission_log(
        &self,
        entry: &NewMissionLogEntry,
    ) -> Result<MissionLogEntry, TeamError> {
        if entry.summary.trim().is_empty() {
            return Err(TeamError::validation(
                entry.team_id,
                "mission log summary must not be empty",
            ));
        }
        let mut state = self.lock();
        if let Some(task_id) = entry.task_id
            && !state
                .tasks
                .get(&task_id)
                .is_some_and(|task| task.team_id == entry.team_id)
        {
            return Err(TeamError::validation(
                entry.team_id,
                format!("unknown team task {task_id}"),
            ));
        }
        state.next_mission_log_id += 1;
        let stored = MissionLogEntry {
            id: state.next_mission_log_id,
            team_id: entry.team_id,
            agent_id: entry.agent_id.clone(),
            task_id: entry.task_id,
            kind: entry.kind,
            summary: entry.summary.clone(),
            evidence: entry.evidence.clone(),
            next_action: entry.next_action.clone(),
            created_at: Utc::now(),
        };
        state.mission_log.push(stored.clone());
        Ok(stored)
    }

    async fn team_mission_log(&self, team_id: TeamId) -> Result<Vec<MissionLogEntry>, TeamError> {
        let state = self.lock();
        let mut log: Vec<MissionLogEntry> = state
            .mission_log
            .iter()
            .filter(|entry| entry.team_id == team_id)
            .cloned()
            .collect();
        log.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(log)
    }

    async fn enqueue_team_run(&self, run: &NewTeamRun) -> Result<TeamRun, TeamError> {
        let mut state = self.lock();
        let id = run.id.unwrap_or_else(Uuid::new_v4);
        if state.runs.contains_key(&id) {
            return Err(TeamError::conflict(
                run.team_id,
                format!("team run {id} already exists"),
            ));
        }
        if run.max_retries < 0 {
            return Err(TeamError::validation(
                run.team_id,
                "max_retries must not be negative",
            ));
        }
        let now = Utc::now();
        let stored = TeamRun {
            id,
            team_id: run.team_id,
            agent_id: run.agent_id.clone(),
            task_id: run.task_id,
            status: TeamRunStatus::Queued,
            message: run.message.clone(),
            priority: run.priority,
            retry_count: 0,
            max_retries: run.max_retries,
            next_attempt_at: run.next_attempt_at,
            continue_conversation: run.continue_conversation,
            started_at: None,
            ended_at: None,
            lease_owner: None,
            heartbeat_at: None,
            last_progress_at: None,
            last_progress_message: String::new(),
            current_activity: String::new(),
            result: None,
            error: None,
            created_at: now,
            updated_at: now,
        };
        state.runs.insert(id, stored.clone());
        Ok(stored)
    }

    async fn start_team_run(
        &self,
        team_id: TeamId,
        run_id: TeamRunId,
        lease_owner: &str,
    ) -> Result<TeamRun, TeamError> {
        let mut state = self.lock();
        let run = state
            .runs
            .get_mut(&run_id)
            .filter(|run| run.team_id == team_id)
            .ok_or_else(|| TeamError::not_found(team_id, format!("team run {run_id}")))?;
        if !matches!(
            run.status,
            TeamRunStatus::Queued | TeamRunStatus::Interrupted
        ) {
            return Err(TeamError::conflict(
                team_id,
                format!(
                    "team run {run_id} is {}; only a queued or interrupted run can start",
                    run.status.as_str()
                ),
            ));
        }
        let now = Utc::now();
        run.status = TeamRunStatus::Running;
        run.started_at = Some(run.started_at.unwrap_or(now));
        run.lease_owner = Some(lease_owner.to_string());
        run.heartbeat_at = Some(now);
        run.last_progress_at = Some(now);
        run.current_activity = run.message.clone();
        run.updated_at = now;
        Ok(run.clone())
    }

    async fn heartbeat_team_run(
        &self,
        team_id: TeamId,
        run_id: TeamRunId,
        progress: &TeamRunProgress,
    ) -> Result<TeamRun, TeamError> {
        let mut state = self.lock();
        let run = state
            .runs
            .get_mut(&run_id)
            .filter(|run| run.team_id == team_id)
            .ok_or_else(|| TeamError::not_found(team_id, format!("team run {run_id}")))?;
        if run.status != TeamRunStatus::Running {
            return Err(TeamError::conflict(
                team_id,
                format!(
                    "team run {run_id} is {}; only a running run can heartbeat",
                    run.status.as_str()
                ),
            ));
        }
        if run.lease_owner.as_deref() != Some(progress.lease_owner.as_str()) {
            return Err(TeamError::conflict(
                team_id,
                format!("team run {run_id} is leased by another owner"),
            ));
        }
        let now = Utc::now();
        run.heartbeat_at = Some(now);
        run.last_progress_at = Some(now);
        run.last_progress_message = progress.message.clone();
        run.current_activity = progress.activity.clone();
        run.updated_at = now;
        Ok(run.clone())
    }

    async fn finish_team_run(
        &self,
        team_id: TeamId,
        run_id: TeamRunId,
        status: TeamRunStatus,
        result: Option<&str>,
        error: Option<&str>,
    ) -> Result<TeamRun, TeamError> {
        if !status.is_terminal() {
            return Err(TeamError::validation(
                team_id,
                format!(
                    "run finish status must be terminal, got {}",
                    status.as_str()
                ),
            ));
        }
        let mut state = self.lock();
        let run = state
            .runs
            .get_mut(&run_id)
            .filter(|run| run.team_id == team_id)
            .ok_or_else(|| TeamError::not_found(team_id, format!("team run {run_id}")))?;
        if run.status.is_terminal() {
            return Ok(run.clone());
        }
        let now = Utc::now();
        run.status = status;
        run.ended_at = Some(now);
        run.result = result.map(str::to_string).or_else(|| run.result.clone());
        run.error = error.map(str::to_string).or_else(|| run.error.clone());
        run.updated_at = now;
        Ok(run.clone())
    }

    async fn requeue_team_run(
        &self,
        team_id: TeamId,
        run_id: TeamRunId,
        message: Option<&str>,
    ) -> Result<TeamRun, TeamError> {
        let mut state = self.lock();
        let run = state
            .runs
            .get_mut(&run_id)
            .filter(|run| run.team_id == team_id)
            .ok_or_else(|| TeamError::not_found(team_id, format!("team run {run_id}")))?;
        if run.status.is_terminal() && run.status != TeamRunStatus::Failed {
            return Err(TeamError::conflict(
                team_id,
                format!(
                    "team run {run_id} is {}; only a failed or interrupted run can retry",
                    run.status.as_str()
                ),
            ));
        }
        if run.status != TeamRunStatus::Failed && run.status != TeamRunStatus::Interrupted {
            return Err(TeamError::conflict(
                team_id,
                format!(
                    "team run {run_id} is {}; only a failed or interrupted run can retry",
                    run.status.as_str()
                ),
            ));
        }
        if run.retry_count >= run.max_retries {
            return Err(TeamError::conflict(
                team_id,
                format!("team run {run_id} exhausted {} retries", run.max_retries),
            ));
        }
        let now = Utc::now();
        run.status = TeamRunStatus::Queued;
        run.retry_count += 1;
        if let Some(message) = message {
            run.message = message.to_string();
        }
        run.started_at = None;
        run.ended_at = None;
        run.lease_owner = None;
        run.heartbeat_at = None;
        run.last_progress_at = None;
        run.last_progress_message.clear();
        run.current_activity.clear();
        run.next_attempt_at = Some(now);
        run.error = None;
        run.updated_at = now;
        Ok(run.clone())
    }

    async fn team_run(
        &self,
        team_id: TeamId,
        run_id: TeamRunId,
    ) -> Result<Option<TeamRun>, TeamError> {
        Ok(self
            .lock()
            .runs
            .get(&run_id)
            .filter(|run| run.team_id == team_id)
            .cloned())
    }

    async fn team_runs(&self, team_id: TeamId) -> Result<Vec<TeamRun>, TeamError> {
        let state = self.lock();
        let mut runs: Vec<TeamRun> = state
            .runs
            .values()
            .filter(|run| run.team_id == team_id)
            .cloned()
            .collect();
        runs.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| right.id.cmp(&left.id))
        });
        Ok(runs)
    }

    async fn create_team_outcome(
        &self,
        outcome: &NewTeamOutcome,
    ) -> Result<TeamOutcome, TeamError> {
        if outcome.title.trim().is_empty() {
            return Err(TeamError::validation(
                outcome.team_id,
                "outcome title must not be empty",
            ));
        }
        let mut state = self.lock();
        let id = outcome.id.unwrap_or_else(Uuid::new_v4);
        if state.outcomes.contains_key(&id) {
            return Err(TeamError::conflict(
                outcome.team_id,
                format!("outcome {id} already exists"),
            ));
        }
        let mut required_sections = Vec::new();
        for section in &outcome.required_sections {
            let section = section.trim();
            if !section.is_empty()
                && !required_sections
                    .iter()
                    .any(|seen: &String| seen.as_str() == section)
            {
                required_sections.push(section.to_string());
            }
        }
        let stored = TeamOutcome {
            id,
            team_id: outcome.team_id,
            title: outcome.title.trim().to_string(),
            status: TeamOutcomeStatus::Draft,
            required_sections,
            created_by: outcome.created_by.clone(),
            created_at: Utc::now(),
            finalized_at: None,
        };
        state.outcomes.insert(id, stored.clone());
        Ok(stored)
    }

    async fn team_outcome(
        &self,
        team_id: TeamId,
        outcome_id: TeamOutcomeId,
    ) -> Result<Option<TeamOutcome>, TeamError> {
        Ok(self
            .lock()
            .outcomes
            .get(&outcome_id)
            .filter(|outcome| outcome.team_id == team_id)
            .cloned())
    }

    async fn team_outcomes(&self, team_id: TeamId) -> Result<Vec<TeamOutcome>, TeamError> {
        let state = self.lock();
        let mut outcomes: Vec<TeamOutcome> = state
            .outcomes
            .values()
            .filter(|outcome| outcome.team_id == team_id)
            .cloned()
            .collect();
        outcomes.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| right.id.cmp(&left.id))
        });
        Ok(outcomes)
    }

    async fn attach_outcome_fragment(
        &self,
        fragment: &NewTeamOutcomeFragment,
    ) -> Result<TeamOutcomeFragment, TeamError> {
        if fragment.section.trim().is_empty() {
            return Err(TeamError::validation(
                fragment.team_id,
                "outcome fragment section must not be empty",
            ));
        }
        if fragment.content.trim().is_empty() {
            return Err(TeamError::validation(
                fragment.team_id,
                "outcome fragment content must not be empty",
            ));
        }
        let mut state = self.lock();
        let outcome = state
            .outcomes
            .get(&fragment.outcome_id)
            .filter(|outcome| outcome.team_id == fragment.team_id)
            .cloned()
            .ok_or_else(|| {
                TeamError::not_found(fragment.team_id, format!("outcome {}", fragment.outcome_id))
            })?;
        if outcome.status == TeamOutcomeStatus::Finalized {
            return Err(TeamError::conflict(
                fragment.team_id,
                format!("outcome {} is already finalized", outcome.id),
            ));
        }
        if !outcome.required_sections.is_empty()
            && !outcome
                .required_sections
                .iter()
                .any(|section| section.as_str() == fragment.section.trim())
        {
            return Err(TeamError::validation(
                fragment.team_id,
                format!(
                    "section {:?} is not part of outcome {}",
                    fragment.section, outcome.id
                ),
            ));
        }
        let id = fragment.id.unwrap_or_else(Uuid::new_v4);
        if state.fragments.contains_key(&id) {
            return Err(TeamError::conflict(
                fragment.team_id,
                format!("outcome fragment {id} already exists"),
            ));
        }
        let stored = TeamOutcomeFragment {
            id,
            team_id: fragment.team_id,
            outcome_id: fragment.outcome_id,
            section: fragment.section.trim().to_string(),
            source_agent_id: fragment.source_agent_id.clone(),
            source_run_id: fragment.source_run_id,
            content: fragment.content.trim().to_string(),
            status: TeamOutcomeFragmentStatus::Draft,
            reviewed_by: None,
            reviewed_at: None,
            created_at: Utc::now(),
        };
        state.fragments.insert(id, stored.clone());
        Ok(stored)
    }

    async fn team_outcome_fragment(
        &self,
        team_id: TeamId,
        fragment_id: TeamOutcomeFragmentId,
    ) -> Result<Option<TeamOutcomeFragment>, TeamError> {
        Ok(self
            .lock()
            .fragments
            .get(&fragment_id)
            .filter(|fragment| fragment.team_id == team_id)
            .cloned())
    }

    async fn team_outcome_fragments(
        &self,
        team_id: TeamId,
        outcome_id: Option<TeamOutcomeId>,
    ) -> Result<Vec<TeamOutcomeFragment>, TeamError> {
        let state = self.lock();
        let mut fragments: Vec<TeamOutcomeFragment> = state
            .fragments
            .values()
            .filter(|fragment| {
                fragment.team_id == team_id
                    && outcome_id.is_none_or(|outcome_id| fragment.outcome_id == outcome_id)
            })
            .cloned()
            .collect();
        fragments.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(fragments)
    }

    async fn review_outcome_fragment(
        &self,
        team_id: TeamId,
        fragment_id: TeamOutcomeFragmentId,
        status: TeamOutcomeFragmentStatus,
        reviewed_by: &str,
    ) -> Result<TeamOutcomeFragment, TeamError> {
        if !matches!(
            status,
            TeamOutcomeFragmentStatus::Reviewed | TeamOutcomeFragmentStatus::Rejected
        ) {
            return Err(TeamError::validation(
                team_id,
                "review must mark a fragment reviewed or rejected",
            ));
        }
        let mut state = self.lock();
        let mut fragment = state
            .fragments
            .get(&fragment_id)
            .filter(|fragment| fragment.team_id == team_id)
            .cloned()
            .ok_or_else(|| {
                TeamError::not_found(team_id, format!("outcome fragment {fragment_id}"))
            })?;
        let outcome = state
            .outcomes
            .get(&fragment.outcome_id)
            .filter(|outcome| outcome.team_id == team_id)
            .cloned()
            .ok_or_else(|| {
                TeamError::not_found(team_id, format!("outcome {}", fragment.outcome_id))
            })?;
        if outcome.status == TeamOutcomeStatus::Finalized {
            return Err(TeamError::conflict(
                team_id,
                format!("outcome {} is already finalized", outcome.id),
            ));
        }
        if fragment.status != TeamOutcomeFragmentStatus::Draft {
            return Err(TeamError::conflict(
                team_id,
                format!(
                    "outcome fragment {fragment_id} is already {}",
                    fragment.status.as_str()
                ),
            ));
        }
        fragment.status = status;
        fragment.reviewed_by = Some(reviewed_by.to_string());
        fragment.reviewed_at = Some(Utc::now());
        let outcome_id = fragment.outcome_id;
        let stored = fragment.clone();
        state.fragments.insert(fragment_id, fragment);
        if let Some(outcome) = state.outcomes.get_mut(&outcome_id)
            && outcome.status == TeamOutcomeStatus::Draft
        {
            outcome.status = TeamOutcomeStatus::InReview;
        }
        Ok(stored)
    }

    async fn finalize_team_outcome(
        &self,
        team_id: TeamId,
        outcome_id: TeamOutcomeId,
    ) -> Result<TeamOutcome, TeamError> {
        let mut state = self.lock();
        let outcome = state
            .outcomes
            .get(&outcome_id)
            .filter(|outcome| outcome.team_id == team_id)
            .cloned()
            .ok_or_else(|| TeamError::not_found(team_id, format!("outcome {outcome_id}")))?;
        if outcome.status == TeamOutcomeStatus::Finalized {
            return Ok(outcome);
        }
        let fragments: Vec<TeamOutcomeFragment> = state
            .fragments
            .values()
            .filter(|fragment| fragment.team_id == team_id && fragment.outcome_id == outcome_id)
            .cloned()
            .collect();
        let missing = outcome.missing_sections(&fragments);
        if !missing.is_empty() {
            return Err(TeamError::validation(
                team_id,
                format!(
                    "outcome {outcome_id} cannot finalize; reviewed sections missing: {}",
                    missing.join(", ")
                ),
            ));
        }
        let now = Utc::now();
        let stored = state.outcomes.get_mut(&outcome_id).expect("outcome exists");
        stored.status = TeamOutcomeStatus::Finalized;
        stored.finalized_at = Some(now);
        Ok(stored.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: Uuid, status: TeamTaskStatus) -> TeamTask {
        let mut task = TeamTask::new(Uuid::new_v4(), "task", "", "lead");
        task.id = id;
        task.status = status;
        task
    }

    #[test]
    fn ready_and_blocked_follow_dependencies() {
        let team_id = Uuid::new_v4();
        let first = task(Uuid::new_v4(), TeamTaskStatus::Completed);
        let mut second = task(Uuid::new_v4(), TeamTaskStatus::Pending);
        second.depends_on = vec![first.id];
        let tasks = vec![first.clone(), second.clone()];

        assert!(second.is_ready(&tasks));
        assert!(second.unmet_dependencies(&tasks).is_empty());
        assert_eq!(ready_team_tasks(&tasks), vec![&second]);

        let missing = Uuid::new_v4();
        let mut blocked = task(Uuid::new_v4(), TeamTaskStatus::Pending);
        blocked.depends_on = vec![first.id, missing];
        let tasks = vec![first, blocked.clone()];
        assert!(!blocked.is_ready(&tasks));
        assert_eq!(blocked.unmet_dependencies(&tasks), vec![missing]);
        assert!(blocked.is_blocked(&tasks));
        assert!(
            ready_team_tasks(&tasks)
                .iter()
                .all(|candidate| candidate.id != blocked.id)
        );
    }

    #[test]
    fn board_status_transitions_are_typed() {
        assert!(TeamTaskStatus::Pending.can_transition_to(TeamTaskStatus::InProgress));
        assert!(TeamTaskStatus::InProgress.can_transition_to(TeamTaskStatus::Completed));
        assert!(TeamTaskStatus::InProgress.can_transition_to(TeamTaskStatus::Blocked));
        assert!(TeamTaskStatus::Blocked.can_transition_to(TeamTaskStatus::Pending));
        assert!(!TeamTaskStatus::Completed.can_transition_to(TeamTaskStatus::Pending));
    }

    #[test]
    fn lead_may_do_everything_and_teammate_is_bounded() {
        for action in [
            TeamActionKind::CreateTask,
            TeamActionKind::ClaimTask,
            TeamActionKind::CompleteTask,
            TeamActionKind::BlockTask,
            TeamActionKind::RunTask,
            TeamActionKind::SendMessage,
            TeamActionKind::Broadcast,
            TeamActionKind::MissionLog,
            TeamActionKind::CreateOutcome,
            TeamActionKind::AttachOutcomeFragment,
            TeamActionKind::ReviewOutcomeFragment,
            TeamActionKind::FinalizeOutcome,
            TeamActionKind::SpawnTeammate,
            TeamActionKind::ShutdownTeammate,
            TeamActionKind::Cleanup,
            TeamActionKind::Stop,
            TeamActionKind::Final,
        ] {
            assert!(TeamRole::Lead.can(action), "{action:?}");
        }

        let teammate = TeamPermissions::new(TeamRole::Teammate);
        for action in [
            TeamActionKind::CreateTask,
            TeamActionKind::ClaimTask,
            TeamActionKind::CompleteTask,
            TeamActionKind::BlockTask,
            TeamActionKind::RunTask,
            TeamActionKind::SendMessage,
            TeamActionKind::Broadcast,
            TeamActionKind::MissionLog,
            TeamActionKind::CreateOutcome,
            TeamActionKind::AttachOutcomeFragment,
            TeamActionKind::ReviewOutcomeFragment,
            TeamActionKind::Stop,
        ] {
            assert!(teammate.can(action), "{action:?}");
        }
        for action in [
            TeamActionKind::FinalizeOutcome,
            TeamActionKind::SpawnTeammate,
            TeamActionKind::ShutdownTeammate,
            TeamActionKind::Cleanup,
            TeamActionKind::Final,
        ] {
            assert!(!teammate.can(action), "{action:?}");
            assert!(action.is_lead_only());
            let err = teammate.require(action).expect_err("lead-only action");
            assert!(matches!(
                err,
                TeamError::PermissionDenied {
                    role: TeamRole::Teammate,
                    ..
                }
            ));
        }
    }

    #[test]
    fn finalize_gate_needs_one_reviewed_fragment_per_section() {
        let team_id = Uuid::new_v4();
        let mut outcome = TeamOutcome::new(team_id, "report", "lead");
        outcome.required_sections = vec!["summary".into(), "evidence".into()];

        let mut fragment = TeamOutcomeFragment {
            id: Uuid::new_v4(),
            team_id,
            outcome_id: outcome.id,
            section: "summary".into(),
            source_agent_id: "worker".into(),
            source_run_id: None,
            content: "done".into(),
            status: TeamOutcomeFragmentStatus::Reviewed,
            reviewed_by: Some("lead".into()),
            reviewed_at: Some(Utc::now()),
            created_at: Utc::now(),
        };

        assert_eq!(
            outcome.missing_sections(&[fragment.clone()]),
            vec!["evidence".to_string()]
        );
        assert!(!outcome.is_ready_to_finalize(&[fragment.clone()]));

        let mut second = fragment.clone();
        second.id = Uuid::new_v4();
        second.section = "evidence".into();
        let both = vec![fragment, second];
        assert!(outcome.missing_sections(&both).is_empty());
        assert!(outcome.is_ready_to_finalize(&both));

        outcome.status = TeamOutcomeStatus::Finalized;
        assert!(!outcome.is_ready_to_finalize(&both));
    }

    #[test]
    fn outcome_without_required_sections_needs_a_reviewed_fragment() {
        let team_id = Uuid::new_v4();
        let outcome = TeamOutcome::new(team_id, "report", "lead");
        assert!(!outcome.missing_sections(&[]).is_empty());
        assert!(!outcome.is_ready_to_finalize(&[]));
    }

    #[test]
    fn run_status_transitions_are_typed() {
        assert!(TeamRunStatus::Queued.can_transition_to(TeamRunStatus::Running));
        assert!(TeamRunStatus::Running.can_transition_to(TeamRunStatus::Interrupted));
        assert!(TeamRunStatus::Interrupted.can_transition_to(TeamRunStatus::Running));
        assert!(TeamRunStatus::Running.can_transition_to(TeamRunStatus::Completed));
        assert!(!TeamRunStatus::Completed.can_transition_to(TeamRunStatus::Running));
        assert!(TeamRunStatus::Completed.is_terminal());
        assert!(TeamRunStatus::Cancelled.is_terminal());
        assert!(!TeamRunStatus::Interrupted.is_terminal());
    }

    #[test]
    fn enums_round_trip_through_serde() {
        for role in TeamRole::ALL {
            let json = serde_json::to_string(&role).unwrap();
            let parsed: TeamRole = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, role);
            assert_eq!(TeamRole::parse(role.as_str()), Some(role));
        }
        for status in [
            TeamTaskStatus::Pending,
            TeamTaskStatus::InProgress,
            TeamTaskStatus::Blocked,
            TeamTaskStatus::Completed,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let parsed: TeamTaskStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, status);
            assert_eq!(TeamTaskStatus::parse(status.as_str()), Some(status));
        }
        for kind in [
            MissionLogKind::Progress,
            MissionLogKind::Handoff,
            MissionLogKind::Blocked,
            MissionLogKind::Decision,
            MissionLogKind::Done,
            MissionLogKind::Error,
        ] {
            assert_eq!(MissionLogKind::parse(kind.as_str()), Some(kind));
        }
    }
}

impl MemoryTeamStore {
    fn store_message(&self, message: &NewTeamMessage) -> Result<TeamMessage, TeamError> {
        if message.body.trim().is_empty() {
            return Err(TeamError::validation(
                message.team_id,
                "message body must not be empty",
            ));
        }
        let mut state = self.lock();
        if let Some(task_id) = message.task_id
            && !state
                .tasks
                .get(&task_id)
                .is_some_and(|task| task.team_id == message.team_id)
        {
            return Err(TeamError::validation(
                message.team_id,
                format!("unknown team task {task_id}"),
            ));
        }
        if let Some(to) = &message.to_agent_id
            && !state.members.contains_key(&(message.team_id, to.clone()))
        {
            return Err(TeamError::validation(
                message.team_id,
                format!("unknown recipient {to}"),
            ));
        }
        state.next_message_id += 1;
        let stored = TeamMessage {
            id: state.next_message_id,
            team_id: message.team_id,
            from_agent_id: message.from_agent_id.clone(),
            to_agent_id: message.to_agent_id.clone(),
            subject: message.subject.trim().to_string(),
            body: message.body.clone(),
            task_id: message.task_id,
            sent_at: Utc::now(),
            read_at: None,
        };
        state.messages.push(stored.clone());
        Ok(stored)
    }
}
