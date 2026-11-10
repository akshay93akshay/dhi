//! PostgreSQL implementation of the Swarm team-state repository.
//!
//! The SQL here is deliberately explicit and uses runtime queries: the CI
//! environment compiles without a live database, and every terminal-state
//! guard is enforced by a conditional `UPDATE` rather than a read-then-write
//! race.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dhi_core::team::{
    MissionLogEntry, MissionLogKind, NewMissionLogEntry, NewTeamMessage, NewTeamOutcome,
    NewTeamOutcomeFragment, NewTeamRun, NewTeamTask, TeamError, TeamMember, TeamMemberStatus,
    TeamMessage, TeamMessageId, TeamOutcome, TeamOutcomeFragment, TeamOutcomeFragmentId,
    TeamOutcomeFragmentStatus, TeamOutcomeId, TeamOutcomeStatus, TeamRole, TeamRun, TeamRunId,
    TeamRunProgress, TeamRunStatus, TeamStore, TeamTask, TeamTaskId, TeamTaskStatus,
};
use sqlx::FromRow;
use uuid::Uuid;

use crate::Store;

fn storage(error: sqlx::Error) -> TeamError {
    TeamError::storage(error.to_string())
}

fn invalid(team_id: Uuid, message: impl Into<String>) -> TeamError {
    TeamError::validation(team_id, message)
}

fn parse_role(raw: &str) -> Result<TeamRole, TeamError> {
    TeamRole::parse(raw).ok_or_else(|| TeamError::storage(format!("invalid team role: {raw}")))
}

fn parse_member_status(raw: &str) -> Result<TeamMemberStatus, TeamError> {
    TeamMemberStatus::parse(raw)
        .ok_or_else(|| TeamError::storage(format!("invalid team member status: {raw}")))
}

fn parse_task_status(raw: &str) -> Result<TeamTaskStatus, TeamError> {
    TeamTaskStatus::parse(raw)
        .ok_or_else(|| TeamError::storage(format!("invalid team task status: {raw}")))
}

fn parse_mission_kind(raw: &str) -> Result<MissionLogKind, TeamError> {
    MissionLogKind::parse(raw)
        .ok_or_else(|| TeamError::storage(format!("invalid mission log kind: {raw}")))
}

fn parse_run_status(raw: &str) -> Result<TeamRunStatus, TeamError> {
    TeamRunStatus::parse(raw)
        .ok_or_else(|| TeamError::storage(format!("invalid team run status: {raw}")))
}

fn parse_outcome_status(raw: &str) -> Result<TeamOutcomeStatus, TeamError> {
    TeamOutcomeStatus::parse(raw)
        .ok_or_else(|| TeamError::storage(format!("invalid outcome status: {raw}")))
}

fn parse_fragment_status(raw: &str) -> Result<TeamOutcomeFragmentStatus, TeamError> {
    TeamOutcomeFragmentStatus::parse(raw)
        .ok_or_else(|| TeamError::storage(format!("invalid fragment status: {raw}")))
}

#[derive(Debug, FromRow)]
struct TeamMemberRow {
    team_id: Uuid,
    agent_id: String,
    role: String,
    status: String,
    description: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<TeamMemberRow> for TeamMember {
    type Error = TeamError;

    fn try_from(row: TeamMemberRow) -> Result<Self, Self::Error> {
        Ok(Self {
            team_id: row.team_id,
            agent_id: row.agent_id,
            role: parse_role(&row.role)?,
            status: parse_member_status(&row.status)?,
            description: row.description,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

#[derive(Debug, FromRow)]
struct TeamTaskRow {
    id: Uuid,
    team_id: Uuid,
    title: String,
    description: String,
    status: String,
    created_by: String,
    assignee: Option<String>,
    depends_on: Vec<Uuid>,
    summary: String,
    blocked_reason: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<TeamTaskRow> for TeamTask {
    type Error = TeamError;

    fn try_from(row: TeamTaskRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            team_id: row.team_id,
            title: row.title,
            description: row.description,
            status: parse_task_status(&row.status)?,
            created_by: row.created_by,
            assignee: row.assignee,
            depends_on: row.depends_on,
            summary: row.summary,
            blocked_reason: row.blocked_reason,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

#[derive(Debug, FromRow)]
struct TeamMessageRow {
    id: i64,
    team_id: Uuid,
    from_agent_id: String,
    to_agent_id: Option<String>,
    subject: String,
    body: String,
    task_id: Option<Uuid>,
    sent_at: DateTime<Utc>,
    read_at: Option<DateTime<Utc>>,
}

impl From<TeamMessageRow> for TeamMessage {
    fn from(row: TeamMessageRow) -> Self {
        Self {
            id: row.id,
            team_id: row.team_id,
            from_agent_id: row.from_agent_id,
            to_agent_id: row.to_agent_id,
            subject: row.subject,
            body: row.body,
            task_id: row.task_id,
            sent_at: row.sent_at,
            read_at: row.read_at,
        }
    }
}

#[derive(Debug, FromRow)]
struct MissionLogRow {
    id: i64,
    team_id: Uuid,
    agent_id: String,
    task_id: Option<Uuid>,
    kind: String,
    summary: String,
    evidence: String,
    next_action: String,
    created_at: DateTime<Utc>,
}

impl TryFrom<MissionLogRow> for MissionLogEntry {
    type Error = TeamError;

    fn try_from(row: MissionLogRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            team_id: row.team_id,
            agent_id: row.agent_id,
            task_id: row.task_id,
            kind: parse_mission_kind(&row.kind)?,
            summary: row.summary,
            evidence: row.evidence,
            next_action: row.next_action,
            created_at: row.created_at,
        })
    }
}

#[derive(Debug, FromRow)]
struct TeamRunRow {
    id: Uuid,
    team_id: Uuid,
    agent_id: String,
    task_id: Option<Uuid>,
    status: String,
    message: String,
    priority: i32,
    retry_count: i32,
    max_retries: i32,
    next_attempt_at: Option<DateTime<Utc>>,
    continue_conversation: bool,
    started_at: Option<DateTime<Utc>>,
    ended_at: Option<DateTime<Utc>>,
    lease_owner: Option<String>,
    heartbeat_at: Option<DateTime<Utc>>,
    last_progress_at: Option<DateTime<Utc>>,
    last_progress_message: String,
    current_activity: String,
    result: Option<String>,
    error: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<TeamRunRow> for TeamRun {
    type Error = TeamError;

    fn try_from(row: TeamRunRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            team_id: row.team_id,
            agent_id: row.agent_id,
            task_id: row.task_id,
            status: parse_run_status(&row.status)?,
            message: row.message,
            priority: row.priority,
            retry_count: row.retry_count,
            max_retries: row.max_retries,
            next_attempt_at: row.next_attempt_at,
            continue_conversation: row.continue_conversation,
            started_at: row.started_at,
            ended_at: row.ended_at,
            lease_owner: row.lease_owner,
            heartbeat_at: row.heartbeat_at,
            last_progress_at: row.last_progress_at,
            last_progress_message: row.last_progress_message,
            current_activity: row.current_activity,
            result: row.result,
            error: row.error,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

#[derive(Debug, FromRow)]
struct TeamOutcomeRow {
    id: Uuid,
    team_id: Uuid,
    title: String,
    status: String,
    required_sections: Vec<String>,
    created_by: String,
    created_at: DateTime<Utc>,
    finalized_at: Option<DateTime<Utc>>,
}

impl TryFrom<TeamOutcomeRow> for TeamOutcome {
    type Error = TeamError;

    fn try_from(row: TeamOutcomeRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            team_id: row.team_id,
            title: row.title,
            status: parse_outcome_status(&row.status)?,
            required_sections: row.required_sections,
            created_by: row.created_by,
            created_at: row.created_at,
            finalized_at: row.finalized_at,
        })
    }
}

#[derive(Debug, FromRow)]
struct TeamOutcomeFragmentRow {
    id: Uuid,
    team_id: Uuid,
    outcome_id: Uuid,
    section: String,
    source_agent_id: String,
    source_run_id: Option<Uuid>,
    content: String,
    status: String,
    reviewed_by: Option<String>,
    reviewed_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

impl TryFrom<TeamOutcomeFragmentRow> for TeamOutcomeFragment {
    type Error = TeamError;

    fn try_from(row: TeamOutcomeFragmentRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            team_id: row.team_id,
            outcome_id: row.outcome_id,
            section: row.section,
            source_agent_id: row.source_agent_id,
            source_run_id: row.source_run_id,
            content: row.content,
            status: parse_fragment_status(&row.status)?,
            reviewed_by: row.reviewed_by,
            reviewed_at: row.reviewed_at,
            created_at: row.created_at,
        })
    }
}

const MEMBER_COLUMNS: &str = "team_id, agent_id, role, status, description, created_at, updated_at";
const TASK_COLUMNS: &str = "id, team_id, title, description, status, created_by, assignee, \
     depends_on, summary, blocked_reason, created_at, updated_at";
const MESSAGE_COLUMNS: &str =
    "id, team_id, from_agent_id, to_agent_id, subject, body, task_id, sent_at, read_at";
const MISSION_COLUMNS: &str =
    "id, team_id, agent_id, task_id, kind, summary, evidence, next_action, created_at";
const RUN_COLUMNS: &str = "id, team_id, agent_id, task_id, status, message, priority, \
     retry_count, max_retries, next_attempt_at, continue_conversation, started_at, ended_at, \
     lease_owner, heartbeat_at, last_progress_at, last_progress_message, current_activity, \
     result, error, created_at, updated_at";
const OUTCOME_COLUMNS: &str = "id, team_id, title, status, required_sections, created_by, \
     created_at, finalized_at";
const FRAGMENT_COLUMNS: &str = "id, team_id, outcome_id, section, source_agent_id, source_run_id, \
     content, status, reviewed_by, reviewed_at, created_at";

#[async_trait]
impl TeamStore for Store {
    async fn upsert_team_member(&self, member: &TeamMember) -> Result<TeamMember, TeamError> {
        if member.agent_id.trim().is_empty() {
            return Err(invalid(member.team_id, "member agent_id must not be empty"));
        }
        let sql = format!(
            "INSERT INTO team_members (team_id, agent_id, role, status, description, created_at, \
             updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7) \
             ON CONFLICT (team_id, agent_id) DO UPDATE SET role = EXCLUDED.role, \
             status = EXCLUDED.status, description = EXCLUDED.description, \
             updated_at = now() RETURNING {MEMBER_COLUMNS}"
        );
        let row = sqlx::query_as::<_, TeamMemberRow>(&sql)
            .bind(member.team_id)
            .bind(&member.agent_id)
            .bind(member.role.as_str())
            .bind(member.status.as_str())
            .bind(&member.description)
            .bind(member.created_at)
            .bind(member.updated_at)
            .fetch_one(self.pool())
            .await
            .map_err(storage)?;
        row.try_into()
    }

    async fn set_team_member_status(
        &self,
        team_id: Uuid,
        agent_id: &str,
        status: TeamMemberStatus,
    ) -> Result<TeamMember, TeamError> {
        let sql = format!(
            "UPDATE team_members SET status = $3, updated_at = now() \
             WHERE team_id = $1 AND agent_id = $2 RETURNING {MEMBER_COLUMNS}"
        );
        let row = sqlx::query_as::<_, TeamMemberRow>(&sql)
            .bind(team_id)
            .bind(agent_id)
            .bind(status.as_str())
            .fetch_optional(self.pool())
            .await
            .map_err(storage)?
            .ok_or_else(|| TeamError::not_found(team_id, format!("member {agent_id}")))?;
        row.try_into()
    }

    async fn team_member(
        &self,
        team_id: Uuid,
        agent_id: &str,
    ) -> Result<Option<TeamMember>, TeamError> {
        let sql = format!(
            "SELECT {MEMBER_COLUMNS} FROM team_members WHERE team_id = $1 AND agent_id = $2"
        );
        let row = sqlx::query_as::<_, TeamMemberRow>(&sql)
            .bind(team_id)
            .bind(agent_id)
            .fetch_optional(self.pool())
            .await
            .map_err(storage)?;
        row.map(TeamMember::try_from).transpose()
    }

    async fn team_members(&self, team_id: Uuid) -> Result<Vec<TeamMember>, TeamError> {
        let sql = format!(
            "SELECT {MEMBER_COLUMNS} FROM team_members WHERE team_id = $1 ORDER BY agent_id"
        );
        let rows = sqlx::query_as::<_, TeamMemberRow>(&sql)
            .bind(team_id)
            .fetch_all(self.pool())
            .await
            .map_err(storage)?;
        rows.into_iter().map(TeamMember::try_from).collect()
    }

    async fn create_team_task(&self, task: &NewTeamTask) -> Result<TeamTask, TeamError> {
        if task.title.trim().is_empty() {
            return Err(invalid(task.team_id, "team task title must not be empty"));
        }
        let id = task.id.unwrap_or_else(Uuid::new_v4);
        if task.depends_on.contains(&id) {
            return Err(invalid(task.team_id, "team task cannot depend on itself"));
        }
        let existing = self.team_tasks(task.team_id).await?;
        for dependency in &task.depends_on {
            if !existing.iter().any(|candidate| candidate.id == *dependency) {
                return Err(invalid(
                    task.team_id,
                    format!("unknown dependency {dependency}"),
                ));
            }
        }
        let sql = format!(
            "INSERT INTO team_tasks (id, team_id, title, description, status, created_by, \
             assignee, depends_on, summary, blocked_reason, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, 'pending', $5, $6, $7, '', '', now(), now()) \
             RETURNING {TASK_COLUMNS}"
        );
        let row = sqlx::query_as::<_, TeamTaskRow>(&sql)
            .bind(id)
            .bind(task.team_id)
            .bind(task.title.trim())
            .bind(task.description.trim())
            .bind(&task.created_by)
            .bind(&task.assignee)
            .bind(task.depends_on.clone())
            .fetch_one(self.pool())
            .await
            .map_err(storage)?;
        row.try_into()
    }

    async fn team_task(
        &self,
        team_id: Uuid,
        task_id: TeamTaskId,
    ) -> Result<Option<TeamTask>, TeamError> {
        let sql = format!("SELECT {TASK_COLUMNS} FROM team_tasks WHERE team_id = $1 AND id = $2");
        let row = sqlx::query_as::<_, TeamTaskRow>(&sql)
            .bind(team_id)
            .bind(task_id)
            .fetch_optional(self.pool())
            .await
            .map_err(storage)?;
        row.map(TeamTask::try_from).transpose()
    }

    async fn team_tasks(&self, team_id: Uuid) -> Result<Vec<TeamTask>, TeamError> {
        let sql = format!(
            "SELECT {TASK_COLUMNS} FROM team_tasks WHERE team_id = $1 \
             ORDER BY created_at, id"
        );
        let rows = sqlx::query_as::<_, TeamTaskRow>(&sql)
            .bind(team_id)
            .fetch_all(self.pool())
            .await
            .map_err(storage)?;
        rows.into_iter().map(TeamTask::try_from).collect()
    }

    async fn claim_team_task(
        &self,
        team_id: Uuid,
        task_id: TeamTaskId,
        agent_id: &str,
    ) -> Result<TeamTask, TeamError> {
        let task = self
            .team_task(team_id, task_id)
            .await?
            .ok_or_else(|| TeamError::not_found(team_id, format!("team task {task_id}")))?;
        if task.status != TeamTaskStatus::Pending {
            return Err(TeamError::conflict(
                team_id,
                format!("team task {task_id} is {}", task.status.as_str()),
            ));
        }
        let all = self.team_tasks(team_id).await?;
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
        let sql = format!(
            "UPDATE team_tasks SET status = 'in_progress', assignee = $3, updated_at = now() \
             WHERE team_id = $1 AND id = $2 AND status = 'pending' \
             AND (assignee IS NULL OR assignee = $3) RETURNING {TASK_COLUMNS}"
        );
        let row = sqlx::query_as::<_, TeamTaskRow>(&sql)
            .bind(team_id)
            .bind(task_id)
            .bind(agent_id)
            .fetch_optional(self.pool())
            .await
            .map_err(storage)?
            .ok_or_else(|| {
                TeamError::conflict(
                    team_id,
                    format!("team task {task_id} was claimed by another member"),
                )
            })?;
        row.try_into()
    }

    async fn complete_team_task(
        &self,
        team_id: Uuid,
        task_id: TeamTaskId,
        summary: &str,
    ) -> Result<TeamTask, TeamError> {
        let sql = format!(
            "UPDATE team_tasks SET status = 'completed', summary = $3, \
             blocked_reason = '', updated_at = now() \
             WHERE team_id = $1 AND id = $2 AND status = 'in_progress' \
             RETURNING {TASK_COLUMNS}"
        );
        let row = sqlx::query_as::<_, TeamTaskRow>(&sql)
            .bind(team_id)
            .bind(task_id)
            .bind(summary)
            .fetch_optional(self.pool())
            .await
            .map_err(storage)?;
        match row {
            Some(row) => row.try_into(),
            None => Err(transition_conflict(
                self.team_task(team_id, task_id).await?,
                team_id,
                task_id,
                "complete",
            )),
        }
    }

    async fn block_team_task(
        &self,
        team_id: Uuid,
        task_id: TeamTaskId,
        reason: &str,
    ) -> Result<TeamTask, TeamError> {
        let sql = format!(
            "UPDATE team_tasks SET status = 'blocked', blocked_reason = $3, updated_at = now() \
             WHERE team_id = $1 AND id = $2 AND status IN ('pending', 'in_progress') \
             RETURNING {TASK_COLUMNS}"
        );
        let row = sqlx::query_as::<_, TeamTaskRow>(&sql)
            .bind(team_id)
            .bind(task_id)
            .bind(reason)
            .fetch_optional(self.pool())
            .await
            .map_err(storage)?;
        match row {
            Some(row) => row.try_into(),
            None => Err(transition_conflict(
                self.team_task(team_id, task_id).await?,
                team_id,
                task_id,
                "block",
            )),
        }
    }

    async fn reopen_team_task(
        &self,
        team_id: Uuid,
        task_id: TeamTaskId,
    ) -> Result<TeamTask, TeamError> {
        let sql = format!(
            "UPDATE team_tasks SET status = 'pending', assignee = NULL, blocked_reason = '', \
             updated_at = now() WHERE team_id = $1 AND id = $2 AND status = 'blocked' \
             RETURNING {TASK_COLUMNS}"
        );
        let row = sqlx::query_as::<_, TeamTaskRow>(&sql)
            .bind(team_id)
            .bind(task_id)
            .fetch_optional(self.pool())
            .await
            .map_err(storage)?;
        match row {
            Some(row) => row.try_into(),
            None => Err(transition_conflict(
                self.team_task(team_id, task_id).await?,
                team_id,
                task_id,
                "reopen",
            )),
        }
    }

    async fn send_team_message(&self, message: &NewTeamMessage) -> Result<TeamMessage, TeamError> {
        if message.to_agent_id.is_none() {
            return Err(invalid(
                message.team_id,
                "a direct message needs a recipient; use broadcast for a team-wide message",
            ));
        }
        self.insert_message(message).await
    }

    async fn broadcast_team_message(
        &self,
        message: &NewTeamMessage,
    ) -> Result<TeamMessage, TeamError> {
        let mut broadcast = message.clone();
        broadcast.to_agent_id = None;
        self.insert_message(&broadcast).await
    }

    async fn team_inbox(
        &self,
        team_id: Uuid,
        agent_id: &str,
    ) -> Result<Vec<TeamMessage>, TeamError> {
        let sql = format!(
            "SELECT {MESSAGE_COLUMNS} FROM team_messages WHERE team_id = $1 \
             AND (to_agent_id = $2 OR (to_agent_id IS NULL AND from_agent_id <> $2)) \
             ORDER BY sent_at, id"
        );
        let rows = sqlx::query_as::<_, TeamMessageRow>(&sql)
            .bind(team_id)
            .bind(agent_id)
            .fetch_all(self.pool())
            .await
            .map_err(storage)?;
        Ok(rows.into_iter().map(TeamMessage::from).collect())
    }

    async fn team_sent(
        &self,
        team_id: Uuid,
        agent_id: &str,
    ) -> Result<Vec<TeamMessage>, TeamError> {
        let sql = format!(
            "SELECT {MESSAGE_COLUMNS} FROM team_messages \
             WHERE team_id = $1 AND from_agent_id = $2 ORDER BY sent_at, id"
        );
        let rows = sqlx::query_as::<_, TeamMessageRow>(&sql)
            .bind(team_id)
            .bind(agent_id)
            .fetch_all(self.pool())
            .await
            .map_err(storage)?;
        Ok(rows.into_iter().map(TeamMessage::from).collect())
    }

    async fn team_messages(&self, team_id: Uuid) -> Result<Vec<TeamMessage>, TeamError> {
        let sql = format!(
            "SELECT {MESSAGE_COLUMNS} FROM team_messages \
             WHERE team_id = $1 ORDER BY sent_at, id"
        );
        let rows = sqlx::query_as::<_, TeamMessageRow>(&sql)
            .bind(team_id)
            .fetch_all(self.pool())
            .await
            .map_err(storage)?;
        Ok(rows.into_iter().map(TeamMessage::from).collect())
    }

    async fn mark_team_message_read(
        &self,
        team_id: Uuid,
        message_id: TeamMessageId,
        agent_id: &str,
    ) -> Result<TeamMessage, TeamError> {
        let sql = format!(
            "UPDATE team_messages SET read_at = COALESCE(read_at, now()) \
             WHERE team_id = $1 AND id = $2 AND (to_agent_id = $3 OR to_agent_id IS NULL) \
             RETURNING {MESSAGE_COLUMNS}"
        );
        let row = sqlx::query_as::<_, TeamMessageRow>(&sql)
            .bind(team_id)
            .bind(message_id)
            .bind(agent_id)
            .fetch_optional(self.pool())
            .await
            .map_err(storage)?;
        match row {
            Some(row) => Ok(row.into()),
            None => Err(TeamError::not_found(
                team_id,
                format!("message {message_id} in {agent_id}'s mailbox"),
            )),
        }
    }

    async fn append_mission_log(
        &self,
        entry: &NewMissionLogEntry,
    ) -> Result<MissionLogEntry, TeamError> {
        if entry.summary.trim().is_empty() {
            return Err(invalid(
                entry.team_id,
                "mission log summary must not be empty",
            ));
        }
        if let Some(task_id) = entry.task_id
            && self.team_task(entry.team_id, task_id).await?.is_none()
        {
            return Err(invalid(
                entry.team_id,
                format!("unknown team task {task_id}"),
            ));
        }
        let sql = format!(
            "INSERT INTO team_mission_log (team_id, agent_id, task_id, kind, summary, \
             evidence, next_action) VALUES ($1, $2, $3, $4, $5, $6, $7) \
             RETURNING {MISSION_COLUMNS}"
        );
        let row = sqlx::query_as::<_, MissionLogRow>(&sql)
            .bind(entry.team_id)
            .bind(&entry.agent_id)
            .bind(entry.task_id)
            .bind(entry.kind.as_str())
            .bind(&entry.summary)
            .bind(&entry.evidence)
            .bind(&entry.next_action)
            .fetch_one(self.pool())
            .await
            .map_err(storage)?;
        row.try_into()
    }

    async fn team_mission_log(&self, team_id: Uuid) -> Result<Vec<MissionLogEntry>, TeamError> {
        let sql = format!(
            "SELECT {MISSION_COLUMNS} FROM team_mission_log WHERE team_id = $1 \
             ORDER BY created_at, id"
        );
        let rows = sqlx::query_as::<_, MissionLogRow>(&sql)
            .bind(team_id)
            .fetch_all(self.pool())
            .await
            .map_err(storage)?;
        rows.into_iter().map(MissionLogEntry::try_from).collect()
    }

    async fn enqueue_team_run(&self, run: &NewTeamRun) -> Result<TeamRun, TeamError> {
        if run.max_retries < 0 {
            return Err(invalid(run.team_id, "max_retries must not be negative"));
        }
        let id = run.id.unwrap_or_else(Uuid::new_v4);
        let sql = format!(
            "INSERT INTO team_runs (id, team_id, agent_id, task_id, status, message, priority, \
             retry_count, max_retries, next_attempt_at, continue_conversation, created_at, \
             updated_at) VALUES ($1, $2, $3, $4, 'queued', $5, $6, 0, $7, $8, $9, now(), now()) \
             RETURNING {RUN_COLUMNS}"
        );
        let row = sqlx::query_as::<_, TeamRunRow>(&sql)
            .bind(id)
            .bind(run.team_id)
            .bind(&run.agent_id)
            .bind(run.task_id)
            .bind(&run.message)
            .bind(run.priority)
            .bind(run.max_retries)
            .bind(run.next_attempt_at)
            .bind(run.continue_conversation)
            .fetch_one(self.pool())
            .await
            .map_err(storage)?;
        row.try_into()
    }

    async fn start_team_run(
        &self,
        team_id: Uuid,
        run_id: TeamRunId,
        lease_owner: &str,
    ) -> Result<TeamRun, TeamError> {
        let sql = format!(
            "UPDATE team_runs SET status = 'running', started_at = COALESCE(started_at, now()), \
             lease_owner = $3, heartbeat_at = now(), last_progress_at = now(), \
             current_activity = message, updated_at = now() \
             WHERE team_id = $1 AND id = $2 AND status IN ('queued', 'interrupted') \
             RETURNING {RUN_COLUMNS}"
        );
        let row = sqlx::query_as::<_, TeamRunRow>(&sql)
            .bind(team_id)
            .bind(run_id)
            .bind(lease_owner)
            .fetch_optional(self.pool())
            .await
            .map_err(storage)?;
        match row {
            Some(row) => row.try_into(),
            None => Err(run_conflict(
                self.team_run(team_id, run_id).await?,
                team_id,
                run_id,
                "start",
            )),
        }
    }

    async fn heartbeat_team_run(
        &self,
        team_id: Uuid,
        run_id: TeamRunId,
        progress: &TeamRunProgress,
    ) -> Result<TeamRun, TeamError> {
        let sql = format!(
            "UPDATE team_runs SET heartbeat_at = now(), last_progress_at = now(), \
             last_progress_message = $4, current_activity = $5, updated_at = now() \
             WHERE team_id = $1 AND id = $2 AND status = 'running' AND lease_owner = $3 \
             RETURNING {RUN_COLUMNS}"
        );
        let row = sqlx::query_as::<_, TeamRunRow>(&sql)
            .bind(team_id)
            .bind(run_id)
            .bind(&progress.lease_owner)
            .bind(&progress.message)
            .bind(&progress.activity)
            .fetch_optional(self.pool())
            .await
            .map_err(storage)?;
        match row {
            Some(row) => row.try_into(),
            None => Err(run_conflict(
                self.team_run(team_id, run_id).await?,
                team_id,
                run_id,
                "heartbeat",
            )),
        }
    }

    async fn finish_team_run(
        &self,
        team_id: Uuid,
        run_id: TeamRunId,
        status: TeamRunStatus,
        result: Option<&str>,
        error: Option<&str>,
    ) -> Result<TeamRun, TeamError> {
        if !status.is_terminal() {
            return Err(invalid(
                team_id,
                format!(
                    "run finish status must be terminal, got {}",
                    status.as_str()
                ),
            ));
        }
        let sql = format!(
            "UPDATE team_runs SET status = $3, ended_at = now(), \
             result = COALESCE($4::TEXT, result), error = COALESCE($5::TEXT, error), \
             updated_at = now() \
             WHERE team_id = $1 AND id = $2 AND status IN ('queued', 'running', 'interrupted') \
             RETURNING {RUN_COLUMNS}"
        );
        let row = sqlx::query_as::<_, TeamRunRow>(&sql)
            .bind(team_id)
            .bind(run_id)
            .bind(status.as_str())
            .bind(result)
            .bind(error)
            .fetch_optional(self.pool())
            .await
            .map_err(storage)?;
        if let Some(row) = row {
            return row.try_into();
        }
        // Already terminal: finish is idempotent so a racing cancellation can
        // never overwrite a completed row.
        self.team_run(team_id, run_id)
            .await?
            .ok_or_else(|| TeamError::not_found(team_id, format!("team run {run_id}")))
    }

    async fn requeue_team_run(
        &self,
        team_id: Uuid,
        run_id: TeamRunId,
        message: Option<&str>,
    ) -> Result<TeamRun, TeamError> {
        let sql = format!(
            "UPDATE team_runs SET status = 'queued', retry_count = retry_count + 1, \
             message = COALESCE($3::TEXT, message), started_at = NULL, ended_at = NULL, \
             lease_owner = NULL, heartbeat_at = NULL, last_progress_at = NULL, \
             last_progress_message = '', current_activity = '', next_attempt_at = now(), \
             error = NULL, updated_at = now() \
             WHERE team_id = $1 AND id = $2 AND status IN ('failed', 'interrupted') \
             AND retry_count < max_retries RETURNING {RUN_COLUMNS}"
        );
        let row = sqlx::query_as::<_, TeamRunRow>(&sql)
            .bind(team_id)
            .bind(run_id)
            .bind(message)
            .fetch_optional(self.pool())
            .await
            .map_err(storage)?;
        match row {
            Some(row) => row.try_into(),
            None => Err(run_conflict(
                self.team_run(team_id, run_id).await?,
                team_id,
                run_id,
                "retry",
            )),
        }
    }

    async fn team_run(
        &self,
        team_id: Uuid,
        run_id: TeamRunId,
    ) -> Result<Option<TeamRun>, TeamError> {
        let sql = format!("SELECT {RUN_COLUMNS} FROM team_runs WHERE team_id = $1 AND id = $2");
        let row = sqlx::query_as::<_, TeamRunRow>(&sql)
            .bind(team_id)
            .bind(run_id)
            .fetch_optional(self.pool())
            .await
            .map_err(storage)?;
        row.map(TeamRun::try_from).transpose()
    }

    async fn team_runs(&self, team_id: Uuid) -> Result<Vec<TeamRun>, TeamError> {
        let sql = format!(
            "SELECT {RUN_COLUMNS} FROM team_runs WHERE team_id = $1 \
             ORDER BY created_at DESC, id"
        );
        let rows = sqlx::query_as::<_, TeamRunRow>(&sql)
            .bind(team_id)
            .fetch_all(self.pool())
            .await
            .map_err(storage)?;
        rows.into_iter().map(TeamRun::try_from).collect()
    }

    async fn create_team_outcome(
        &self,
        outcome: &NewTeamOutcome,
    ) -> Result<TeamOutcome, TeamError> {
        if outcome.title.trim().is_empty() {
            return Err(invalid(outcome.team_id, "outcome title must not be empty"));
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
        let id = outcome.id.unwrap_or_else(Uuid::new_v4);
        let sql = format!(
            "INSERT INTO team_outcomes (id, team_id, title, status, required_sections, \
             created_by, created_at) VALUES ($1, $2, $3, 'draft', $4, $5, now()) \
             RETURNING {OUTCOME_COLUMNS}"
        );
        let row = sqlx::query_as::<_, TeamOutcomeRow>(&sql)
            .bind(id)
            .bind(outcome.team_id)
            .bind(outcome.title.trim())
            .bind(required_sections.clone())
            .bind(&outcome.created_by)
            .fetch_one(self.pool())
            .await
            .map_err(storage)?;
        row.try_into()
    }

    async fn team_outcome(
        &self,
        team_id: Uuid,
        outcome_id: TeamOutcomeId,
    ) -> Result<Option<TeamOutcome>, TeamError> {
        let sql =
            format!("SELECT {OUTCOME_COLUMNS} FROM team_outcomes WHERE team_id = $1 AND id = $2");
        let row = sqlx::query_as::<_, TeamOutcomeRow>(&sql)
            .bind(team_id)
            .bind(outcome_id)
            .fetch_optional(self.pool())
            .await
            .map_err(storage)?;
        row.map(TeamOutcome::try_from).transpose()
    }

    async fn team_outcomes(&self, team_id: Uuid) -> Result<Vec<TeamOutcome>, TeamError> {
        let sql = format!(
            "SELECT {OUTCOME_COLUMNS} FROM team_outcomes WHERE team_id = $1 \
             ORDER BY created_at DESC, id"
        );
        let rows = sqlx::query_as::<_, TeamOutcomeRow>(&sql)
            .bind(team_id)
            .fetch_all(self.pool())
            .await
            .map_err(storage)?;
        rows.into_iter().map(TeamOutcome::try_from).collect()
    }

    async fn attach_outcome_fragment(
        &self,
        fragment: &NewTeamOutcomeFragment,
    ) -> Result<TeamOutcomeFragment, TeamError> {
        if fragment.section.trim().is_empty() {
            return Err(invalid(
                fragment.team_id,
                "outcome fragment section must not be empty",
            ));
        }
        if fragment.content.trim().is_empty() {
            return Err(invalid(
                fragment.team_id,
                "outcome fragment content must not be empty",
            ));
        }
        let outcome = self
            .team_outcome(fragment.team_id, fragment.outcome_id)
            .await?
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
            return Err(invalid(
                fragment.team_id,
                format!(
                    "section {:?} is not part of outcome {}",
                    fragment.section, outcome.id
                ),
            ));
        }
        let id = fragment.id.unwrap_or_else(Uuid::new_v4);
        let sql = format!(
            "INSERT INTO team_outcome_fragments (id, team_id, outcome_id, section, \
             source_agent_id, source_run_id, content, status, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, 'draft', now()) \
             RETURNING {FRAGMENT_COLUMNS}"
        );
        let row = sqlx::query_as::<_, TeamOutcomeFragmentRow>(&sql)
            .bind(id)
            .bind(fragment.team_id)
            .bind(fragment.outcome_id)
            .bind(fragment.section.trim())
            .bind(&fragment.source_agent_id)
            .bind(fragment.source_run_id)
            .bind(fragment.content.trim())
            .fetch_one(self.pool())
            .await
            .map_err(storage)?;
        row.try_into()
    }

    async fn team_outcome_fragment(
        &self,
        team_id: Uuid,
        fragment_id: TeamOutcomeFragmentId,
    ) -> Result<Option<TeamOutcomeFragment>, TeamError> {
        let sql = format!(
            "SELECT {FRAGMENT_COLUMNS} FROM team_outcome_fragments \
             WHERE team_id = $1 AND id = $2"
        );
        let row = sqlx::query_as::<_, TeamOutcomeFragmentRow>(&sql)
            .bind(team_id)
            .bind(fragment_id)
            .fetch_optional(self.pool())
            .await
            .map_err(storage)?;
        row.map(TeamOutcomeFragment::try_from).transpose()
    }

    async fn team_outcome_fragments(
        &self,
        team_id: Uuid,
        outcome_id: Option<TeamOutcomeId>,
    ) -> Result<Vec<TeamOutcomeFragment>, TeamError> {
        let sql = format!(
            "SELECT {FRAGMENT_COLUMNS} FROM team_outcome_fragments WHERE team_id = $1 \
             AND ($2::UUID IS NULL OR outcome_id = $2) ORDER BY created_at, id"
        );
        let rows = sqlx::query_as::<_, TeamOutcomeFragmentRow>(&sql)
            .bind(team_id)
            .bind(outcome_id)
            .fetch_all(self.pool())
            .await
            .map_err(storage)?;
        rows.into_iter()
            .map(TeamOutcomeFragment::try_from)
            .collect()
    }

    async fn review_outcome_fragment(
        &self,
        team_id: Uuid,
        fragment_id: TeamOutcomeFragmentId,
        status: TeamOutcomeFragmentStatus,
        reviewed_by: &str,
    ) -> Result<TeamOutcomeFragment, TeamError> {
        if !matches!(
            status,
            TeamOutcomeFragmentStatus::Reviewed | TeamOutcomeFragmentStatus::Rejected
        ) {
            return Err(invalid(
                team_id,
                "review must mark a fragment reviewed or rejected",
            ));
        }
        let mut tx = self.pool().begin().await.map_err(storage)?;
        let sql = format!(
            "UPDATE team_outcome_fragments SET status = $3, reviewed_by = $4, \
             reviewed_at = now() WHERE team_id = $1 AND id = $2 AND status = 'draft' \
             AND EXISTS (SELECT 1 FROM team_outcomes o WHERE o.team_id = $1 \
             AND o.id = team_outcome_fragments.outcome_id AND o.status <> 'finalized') \
             RETURNING {FRAGMENT_COLUMNS}"
        );
        let row = sqlx::query_as::<_, TeamOutcomeFragmentRow>(&sql)
            .bind(team_id)
            .bind(fragment_id)
            .bind(status.as_str())
            .bind(reviewed_by)
            .fetch_optional(&mut *tx)
            .await
            .map_err(storage)?;
        let row = row.ok_or_else(|| {
            TeamError::conflict(
                team_id,
                format!("outcome fragment {fragment_id} is missing or already reviewed"),
            )
        })?;
        sqlx::query(
            "UPDATE team_outcomes SET status = 'in_review' \
             WHERE team_id = $1 AND id = $2 AND status = 'draft'",
        )
        .bind(team_id)
        .bind(row.outcome_id)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        tx.commit().await.map_err(storage)?;
        row.try_into()
    }

    async fn finalize_team_outcome(
        &self,
        team_id: Uuid,
        outcome_id: TeamOutcomeId,
    ) -> Result<TeamOutcome, TeamError> {
        let outcome = self
            .team_outcome(team_id, outcome_id)
            .await?
            .ok_or_else(|| TeamError::not_found(team_id, format!("outcome {outcome_id}")))?;
        if outcome.status == TeamOutcomeStatus::Finalized {
            return Ok(outcome);
        }
        let fragments = self
            .team_outcome_fragments(team_id, Some(outcome_id))
            .await?;
        let missing = outcome.missing_sections(&fragments);
        if !missing.is_empty() {
            return Err(invalid(
                team_id,
                format!(
                    "outcome {outcome_id} cannot finalize; reviewed sections missing: {}",
                    missing.join(", ")
                ),
            ));
        }
        let sql = format!(
            "UPDATE team_outcomes SET status = 'finalized', finalized_at = now() \
             WHERE team_id = $1 AND id = $2 AND status IN ('draft', 'in_review') \
             RETURNING {OUTCOME_COLUMNS}"
        );
        let row = sqlx::query_as::<_, TeamOutcomeRow>(&sql)
            .bind(team_id)
            .bind(outcome_id)
            .fetch_optional(self.pool())
            .await
            .map_err(storage)?;
        match row {
            Some(row) => row.try_into(),
            None => self
                .team_outcome(team_id, outcome_id)
                .await?
                .ok_or_else(|| TeamError::not_found(team_id, format!("outcome {outcome_id}"))),
        }
    }
}

impl Store {
    async fn insert_message(&self, message: &NewTeamMessage) -> Result<TeamMessage, TeamError> {
        if message.body.trim().is_empty() {
            return Err(invalid(message.team_id, "message body must not be empty"));
        }
        if let Some(task_id) = message.task_id
            && self.team_task(message.team_id, task_id).await?.is_none()
        {
            return Err(invalid(
                message.team_id,
                format!("unknown team task {task_id}"),
            ));
        }
        if let Some(to) = &message.to_agent_id
            && self.team_member(message.team_id, to).await?.is_none()
        {
            return Err(invalid(message.team_id, format!("unknown recipient {to}")));
        }
        let sql = format!(
            "INSERT INTO team_messages (team_id, from_agent_id, to_agent_id, subject, body, \
             task_id) VALUES ($1, $2, $3, $4, $5, $6) RETURNING {MESSAGE_COLUMNS}"
        );
        let row = sqlx::query_as::<_, TeamMessageRow>(&sql)
            .bind(message.team_id)
            .bind(&message.from_agent_id)
            .bind(&message.to_agent_id)
            .bind(message.subject.trim())
            .bind(&message.body)
            .bind(message.task_id)
            .fetch_one(self.pool())
            .await
            .map_err(storage)?;
        Ok(row.into())
    }
}

fn transition_conflict(
    task: Option<TeamTask>,
    team_id: Uuid,
    task_id: TeamTaskId,
    verb: &str,
) -> TeamError {
    match task {
        Some(task) => TeamError::conflict(
            team_id,
            format!(
                "team task {task_id} is {}; cannot {verb}",
                task.status.as_str()
            ),
        ),
        None => TeamError::not_found(team_id, format!("team task {task_id}")),
    }
}

fn run_conflict(run: Option<TeamRun>, team_id: Uuid, run_id: TeamRunId, verb: &str) -> TeamError {
    match run {
        Some(run) => TeamError::conflict(
            team_id,
            format!(
                "team run {run_id} is {}; cannot {verb}",
                run.status.as_str()
            ),
        ),
        None => TeamError::not_found(team_id, format!("team run {run_id}")),
    }
}
