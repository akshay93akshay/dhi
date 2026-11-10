//! Swarm team action vocabulary and the model-visible team tool definitions.
//!
//! The coordinator and its teammates are normal agent runs. They mutate shared
//! team state only through the team tools defined here; the engine's team host
//! maps each tool call onto one [`TeamAction`] and applies it through
//! [`dhi_core::team::TeamStore`] with the caller's persisted role.
//!
//! This module is deliberately model-free: it defines the typed actions and
//! the JSON schemas the runtime registers as tools. It never calls a model and
//! never touches shared state itself.

use dhi_core::team::{
    MissionLogKind, TeamActionKind, TeamOutcomeFragmentStatus, TeamRole, TeamTask,
};
use serde::{Deserialize, Serialize};

/// Whether a run action waits for the teammate or is handed to the runner's
/// asynchronous run pool.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamRunMode {
    #[default]
    Sync,
    Async,
}

/// The coordinator's verdict on one outcome fragment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamFragmentReview {
    #[default]
    Reviewed,
    Rejected,
}

impl TeamFragmentReview {
    pub fn status(self) -> TeamOutcomeFragmentStatus {
        match self {
            Self::Reviewed => TeamOutcomeFragmentStatus::Reviewed,
            Self::Rejected => TeamOutcomeFragmentStatus::Rejected,
        }
    }
}

/// One typed team mutation.
///
/// The host builds these from tool arguments. Actions are applied in the order
/// the model emits their tools; `stop` ends the intent without a fact, while a
/// coordinator finalizes the intent's one fact through its native
/// `record_fact` tool.
// The action value is one tool call; boxing a variant would make the dispatch
// and serde form harder to follow for no measurable gain.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum TeamAction {
    /// Add one card to the shared task board.
    CreateTask {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<uuid::Uuid>,
        title: String,
        #[serde(default)]
        description: String,
        #[serde(default)]
        depends_on: Vec<uuid::Uuid>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        assignee: Option<String>,
    },
    /// Claim a ready pending task for an agent (defaults to the actor).
    ClaimTask {
        task_id: uuid::Uuid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
    },
    /// Mark one in-progress task complete with a short result summary.
    CompleteTask {
        task_id: uuid::Uuid,
        #[serde(default)]
        summary: String,
    },
    /// Mark one pending or in-progress task blocked with an actionable reason.
    BlockTask {
        task_id: uuid::Uuid,
        #[serde(default)]
        reason: String,
    },
    /// Run one teammate on a claimed task, synchronously or asynchronously.
    RunTask {
        task_id: uuid::Uuid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        #[serde(default)]
        mode: TeamRunMode,
        #[serde(default)]
        message: String,
        #[serde(default)]
        priority: i32,
    },
    /// Send a direct message to one member.
    SendMessage {
        to: String,
        #[serde(default)]
        subject: String,
        body: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<uuid::Uuid>,
    },
    /// Send one message to every member.
    Broadcast {
        #[serde(default)]
        subject: String,
        body: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<uuid::Uuid>,
    },
    /// Append one entry to the mission log.
    MissionLog {
        kind: MissionLogKind,
        summary: String,
        #[serde(default)]
        evidence: String,
        #[serde(default)]
        next_action: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<uuid::Uuid>,
    },
    /// Start one outcome with required sections.
    CreateOutcome {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<uuid::Uuid>,
        title: String,
        #[serde(default)]
        required_sections: Vec<String>,
    },
    /// Attach one drafted section to an outcome.
    AttachOutcomeFragment {
        outcome_id: uuid::Uuid,
        section: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<uuid::Uuid>,
    },
    /// Review of one outcome fragment.
    ReviewOutcomeFragment {
        fragment_id: uuid::Uuid,
        #[serde(default)]
        decision: TeamFragmentReview,
    },
    /// Finalize an outcome once every required section has a reviewed
    /// fragment. Coordinator-only.
    FinalizeOutcome { outcome_id: uuid::Uuid },
    /// Add a teammate to the roster. Coordinator-only.
    SpawnTeammate {
        agent_id: String,
        #[serde(default)]
        description: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_turns: Option<usize>,
    },
    /// Stop a teammate without deleting its history. Coordinator-only.
    ShutdownTeammate { agent_id: String },
    /// Cancel active runs and stop all teammates. Coordinator-only.
    Cleanup,
    /// End the loop without producing a fact.
    Stop { reason: String },
    /// Return the intent's one fact draft. Not used by the coordinator agent,
    /// which records its fact through `record_fact`; retained for the typed
    /// team action contract and manager-worker compatibility.
    Final {
        summary: String,
        #[serde(default)]
        detail: String,
        #[serde(default)]
        evidence: String,
        #[serde(default = "default_confidence")]
        confidence: String,
    },
}

fn default_confidence() -> String {
    "inferred".to_string()
}

impl TeamAction {
    pub fn kind(&self) -> TeamActionKind {
        match self {
            Self::CreateTask { .. } => TeamActionKind::CreateTask,
            Self::ClaimTask { .. } => TeamActionKind::ClaimTask,
            Self::CompleteTask { .. } => TeamActionKind::CompleteTask,
            Self::BlockTask { .. } => TeamActionKind::BlockTask,
            Self::RunTask { .. } => TeamActionKind::RunTask,
            Self::SendMessage { .. } => TeamActionKind::SendMessage,
            Self::Broadcast { .. } => TeamActionKind::Broadcast,
            Self::MissionLog { .. } => TeamActionKind::MissionLog,
            Self::CreateOutcome { .. } => TeamActionKind::CreateOutcome,
            Self::AttachOutcomeFragment { .. } => TeamActionKind::AttachOutcomeFragment,
            Self::ReviewOutcomeFragment { .. } => TeamActionKind::ReviewOutcomeFragment,
            Self::FinalizeOutcome { .. } => TeamActionKind::FinalizeOutcome,
            Self::SpawnTeammate { .. } => TeamActionKind::SpawnTeammate,
            Self::ShutdownTeammate { .. } => TeamActionKind::ShutdownTeammate,
            Self::Cleanup => TeamActionKind::Cleanup,
            Self::Stop { .. } => TeamActionKind::Stop,
            Self::Final { .. } => TeamActionKind::Final,
        }
    }
}

/// One model-visible team tool definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamToolSpec {
    pub name: String,
    pub description: String,
    pub schema: serde_json::Value,
}

impl TeamToolSpec {
    pub fn new(name: &str, description: &str, schema: serde_json::Value) -> Self {
        Self {
            name: name.to_string(),
            description: description.to_string(),
            schema,
        }
    }
}

fn uuid_schema(description: &str) -> serde_json::Value {
    serde_json::json!({ "type": "string", "format": "uuid", "description": description })
}

fn string_schema(description: &str) -> serde_json::Value {
    serde_json::json!({ "type": "string", "description": description })
}

fn empty_schema() -> serde_json::Value {
    serde_json::json!({ "type": "object", "properties": {} })
}

fn status_schema() -> serde_json::Value {
    empty_schema()
}

fn task_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "operation": {
                "type": "string",
                "enum": ["create", "list", "claim", "complete", "block"],
                "description": "Board operation to apply."
            },
            "id": uuid_schema("Optional task id for create."),
            "title": string_schema("Short task title shown on the board."),
            "description": string_schema("What the assignee must do."),
            "depends_on": {
                "type": "array",
                "items": { "type": "string", "format": "uuid" },
                "description": "Task ids that must be completed first."
            },
            "assignee": string_schema("Optional teammate agent id."),
            "task_id": uuid_schema("Board task id for claim, complete, or block."),
            "summary": string_schema("Short result summary for complete."),
            "reason": string_schema("Actionable reason for block.")
        },
        "required": ["operation"]
    })
}

fn run_task_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "task_id": uuid_schema("Board task id."),
            "agent_id": string_schema("Teammate to run. Required; never the caller."),
            "mode": { "type": "string", "enum": ["sync", "async"], "description": "Async runs continue while the coordinator keeps working." },
            "message": string_schema("Optional extra instruction for the run."),
            "priority": { "type": "integer", "description": "Higher runs first when the runner queues work." }
        },
        "required": ["task_id", "agent_id"]
    })
}

fn list_runs_schema() -> serde_json::Value {
    empty_schema()
}

fn await_runs_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "run_ids": {
                "type": "array",
                "items": { "type": "string", "format": "uuid" },
                "description": "Runs to await. Omit to await every run this team started."
            },
            "run_id": uuid_schema("A single run to await.")
        }
    })
}

fn cancel_run_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "run_id": uuid_schema("Run id to cancel.")
        },
        "required": ["run_id"]
    })
}

fn message_schema(broadcast: bool) -> serde_json::Value {
    let mut properties = serde_json::json!({
        "subject": string_schema("Short subject line."),
        "body": string_schema("The message body."),
        "task_id": uuid_schema("Optional related board task.")
    });
    if !broadcast {
        properties["to"] = string_schema("Recipient agent id.");
    }
    let required = if broadcast {
        serde_json::json!(["body"])
    } else {
        serde_json::json!(["to", "body"])
    };
    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required
    })
}

fn read_mailbox_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "box": { "type": "string", "enum": ["inbox", "sent"], "description": "Mailbox to read; defaults to inbox." },
            "mark_read": { "type": "boolean", "description": "Mark returned inbox messages read." }
        }
    })
}

fn mission_log_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "kind": { "type": "string", "enum": ["progress", "handoff", "blocked", "decision", "done", "error"] },
            "summary": string_schema("One sentence the coordinator and UI read."),
            "evidence": string_schema("What you observed or produced."),
            "next_action": string_schema("What should happen next."),
            "task_id": uuid_schema("Optional related board task.")
        },
        "required": ["kind", "summary"]
    })
}

fn create_outcome_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "id": uuid_schema("Optional outcome id."),
            "title": string_schema("Outcome title."),
            "required_sections": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Sections the final artifact must cover with reviewed fragments."
            }
        },
        "required": ["title"]
    })
}

fn attach_fragment_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "outcome_id": uuid_schema("Outcome id."),
            "section": string_schema("Required section this fragment fills."),
            "content": string_schema("The drafted content."),
            "run_id": uuid_schema("Optional run that produced it.")
        },
        "required": ["outcome_id", "section", "content"]
    })
}

fn review_fragment_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "fragment_id": uuid_schema("Outcome fragment id."),
            "decision": { "type": "string", "enum": ["reviewed", "rejected"] }
        },
        "required": ["fragment_id"]
    })
}

fn finalize_outcome_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "outcome_id": uuid_schema("Outcome id to finalize.")
        },
        "required": ["outcome_id"]
    })
}

fn list_outcomes_schema() -> serde_json::Value {
    empty_schema()
}

fn spawn_teammate_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "agent_id": string_schema("Stable teammate id."),
            "description": string_schema("Role and responsibility."),
            "max_turns": { "type": "integer", "minimum": 1 }
        },
        "required": ["agent_id"]
    })
}

fn shutdown_teammate_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "agent_id": string_schema("Teammate id to stop.")
        },
        "required": ["agent_id"]
    })
}

fn stop_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "reason": string_schema("Why no fact can be produced.")
        },
        "required": ["reason"]
    })
}

/// Every tool a member of `role` may call in-loop.
///
/// The coordinator (persisted as `TeamRole::Lead`) gets every team tool. A
/// teammate gets the same set minus spawn, shutdown, cleanup, and outcome
/// finalization.
pub fn team_tool_specs(role: TeamRole) -> Vec<TeamToolSpec> {
    let mut tools = vec![
        TeamToolSpec::new(
            "team_status",
            "Read the shared team state: roster, board, runs, mailbox, mission log, and outcomes.",
            status_schema(),
        ),
        TeamToolSpec::new(
            "team_task",
            "Create, list, claim, complete, or block one card on the shared task board.",
            task_schema(),
        ),
        TeamToolSpec::new(
            "team_run_task",
            "Run a teammate on a board task, synchronously or asynchronously.",
            run_task_schema(),
        ),
        TeamToolSpec::new(
            "team_list_runs",
            "List every teammate run and its status.",
            list_runs_schema(),
        ),
        TeamToolSpec::new(
            "team_await_runs",
            "Wait for one or more asynchronous teammate runs to finish.",
            await_runs_schema(),
        ),
        TeamToolSpec::new(
            "team_cancel_run",
            "Cancel one queued or running teammate run.",
            cancel_run_schema(),
        ),
        TeamToolSpec::new(
            "team_send_message",
            "Send a direct message to one team member.",
            message_schema(false),
        ),
        TeamToolSpec::new(
            "team_broadcast",
            "Send one message to every team member.",
            message_schema(true),
        ),
        TeamToolSpec::new(
            "team_read_mailbox",
            "Read your inbox or sent mailbox.",
            read_mailbox_schema(),
        ),
        TeamToolSpec::new(
            "team_mission_log",
            "Append one progress, handoff, blocked, decision, done, or error entry.",
            mission_log_schema(),
        ),
        TeamToolSpec::new(
            "team_create_outcome",
            "Start the outcome the team must finalize.",
            create_outcome_schema(),
        ),
        TeamToolSpec::new(
            "team_attach_outcome_fragment",
            "Attach one drafted section to an outcome.",
            attach_fragment_schema(),
        ),
        TeamToolSpec::new(
            "team_review_outcome_fragment",
            "Review or reject one outcome fragment.",
            review_fragment_schema(),
        ),
        TeamToolSpec::new(
            "team_list_outcomes",
            "List outcomes and their fragments.",
            list_outcomes_schema(),
        ),
        TeamToolSpec::new(
            "team_stop",
            "Stop the team without producing a fact.",
            stop_schema(),
        ),
    ];
    if role == TeamRole::Lead {
        tools.extend([
            TeamToolSpec::new(
                "team_spawn_teammate",
                "Add one teammate to the team roster, within the active cap.",
                spawn_teammate_schema(),
            ),
            TeamToolSpec::new(
                "team_shutdown_teammate",
                "Stop one teammate without deleting its history.",
                shutdown_teammate_schema(),
            ),
            TeamToolSpec::new(
                "team_finalize_outcome",
                "Finalize an outcome after every required section is reviewed.",
                finalize_outcome_schema(),
            ),
            TeamToolSpec::new(
                "team_cleanup",
                "Cancel active runs and stop all teammates.",
                empty_schema(),
            ),
        ]);
    }
    tools
}

/// The text one task card contributes to a teammate's run intent.
pub fn teammate_task_text(task: &TeamTask) -> String {
    if task.description.trim().is_empty() {
        task.title.clone()
    } else {
        format!("{}\n\n{}", task.title, task.description)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinator_gets_every_team_tool() {
        let specs = team_tool_specs(TeamRole::Lead);
        let names: Vec<&str> = specs.iter().map(|tool| tool.name.as_str()).collect();
        for expected in [
            "team_spawn_teammate",
            "team_shutdown_teammate",
            "team_status",
            "team_task",
            "team_run_task",
            "team_list_runs",
            "team_await_runs",
            "team_cancel_run",
            "team_send_message",
            "team_broadcast",
            "team_read_mailbox",
            "team_mission_log",
            "team_create_outcome",
            "team_attach_outcome_fragment",
            "team_review_outcome_fragment",
            "team_finalize_outcome",
            "team_list_outcomes",
            "team_cleanup",
            "team_stop",
        ] {
            assert!(
                names.contains(&expected),
                "coordinator is missing {expected}"
            );
        }
    }

    #[test]
    fn teammate_tools_exclude_coordinator_only_tools() {
        let specs = team_tool_specs(TeamRole::Teammate);
        let names: Vec<&str> = specs.iter().map(|tool| tool.name.as_str()).collect();
        for coordinator_only in [
            "team_spawn_teammate",
            "team_shutdown_teammate",
            "team_finalize_outcome",
            "team_cleanup",
        ] {
            assert!(
                !names.contains(&coordinator_only),
                "{coordinator_only} leaked to a teammate"
            );
        }
        for shared in [
            "team_task",
            "team_run_task",
            "team_create_outcome",
            "team_review_outcome_fragment",
            "team_send_message",
        ] {
            assert!(names.contains(&shared), "teammate is missing {shared}");
        }
    }

    #[test]
    fn every_action_has_a_kind_and_round_trips() {
        let actions = [
            TeamAction::CreateTask {
                id: None,
                title: "t".into(),
                description: String::new(),
                depends_on: Vec::new(),
                assignee: None,
            },
            TeamAction::ClaimTask {
                task_id: uuid::Uuid::new_v4(),
                agent_id: None,
            },
            TeamAction::CompleteTask {
                task_id: uuid::Uuid::new_v4(),
                summary: "done".into(),
            },
            TeamAction::BlockTask {
                task_id: uuid::Uuid::new_v4(),
                reason: "blocked".into(),
            },
            TeamAction::RunTask {
                task_id: uuid::Uuid::new_v4(),
                agent_id: None,
                mode: TeamRunMode::Async,
                message: String::new(),
                priority: 0,
            },
            TeamAction::SendMessage {
                to: "a".into(),
                subject: "s".into(),
                body: "b".into(),
                task_id: None,
            },
            TeamAction::Broadcast {
                subject: "s".into(),
                body: "b".into(),
                task_id: None,
            },
            TeamAction::MissionLog {
                kind: MissionLogKind::Progress,
                summary: "s".into(),
                evidence: String::new(),
                next_action: String::new(),
                task_id: None,
            },
            TeamAction::CreateOutcome {
                id: None,
                title: "o".into(),
                required_sections: vec!["summary".into()],
            },
            TeamAction::AttachOutcomeFragment {
                outcome_id: uuid::Uuid::new_v4(),
                section: "summary".into(),
                content: "c".into(),
                run_id: None,
            },
            TeamAction::ReviewOutcomeFragment {
                fragment_id: uuid::Uuid::new_v4(),
                decision: TeamFragmentReview::Reviewed,
            },
            TeamAction::FinalizeOutcome {
                outcome_id: uuid::Uuid::new_v4(),
            },
            TeamAction::SpawnTeammate {
                agent_id: "a".into(),
                description: String::new(),
                max_turns: None,
            },
            TeamAction::ShutdownTeammate {
                agent_id: "a".into(),
            },
            TeamAction::Cleanup,
            TeamAction::Stop { reason: "r".into() },
            TeamAction::Final {
                summary: "s".into(),
                detail: String::new(),
                evidence: String::new(),
                confidence: "inferred".into(),
            },
        ];
        for action in actions {
            assert_eq!(
                TeamActionKind::parse(action.kind().as_str()),
                Some(action.kind()),
                "{action:?}"
            );
            let json = serde_json::to_string(&action).unwrap();
            let parsed: TeamAction = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, action);
        }
    }
}
