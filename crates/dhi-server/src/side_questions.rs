//! Advisory side questions about one task.
//!
//! A session freezes the task's graph as JSON when it is created. Every
//! answer is produced from that stored snapshot, never from a fresh read, so
//! facts recorded after the session opened cannot enter the conversation.
//! The answerer receives only the snapshot text and the question: it gets no
//! store handle, no tools, and no engine. Side questions write only the two
//! side-question tables; they never append activity or mutate the work graph.

use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Path, State};
use chrono::{DateTime, Utc};
use dhi_agent::SideQuestionInput;
use dhi_core::{
    GraphStore, NewSideQuestionRequest, SideQuestionRequest, SideQuestionSessionStatus,
    SideQuestionStatus, Task, TaskId,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{ApiError, AppState, require_task};

/// How long an answerer may take before the route records an error.
pub const SIDE_QUESTION_TIMEOUT: Duration = Duration::from_secs(120);

pub use dhi_followups::{SIDE_QUESTION_ACTIVITY_LIMIT, SIDE_QUESTION_MAX_CHARS};

/// `POST /api/tasks/{id}/side-questions` input.
#[derive(Debug, Deserialize)]
pub(crate) struct AskSideQuestionRequest {
    pub question: String,
    /// Appends to this open session. Omitted creates a new session with a new
    /// frozen snapshot, even when another session is open.
    pub session_id: Option<Uuid>,
}

/// `POST /api/tasks/{id}/side-questions` output.
#[derive(Debug, Serialize)]
pub(crate) struct AskSideQuestionResponse {
    pub session_id: Uuid,
    pub request: SideQuestionRequest,
    pub answer: String,
}

/// One session in the list response: the snapshot is intentionally omitted.
#[derive(Debug, Serialize)]
pub(crate) struct SessionView {
    id: Uuid,
    created_at: DateTime<Utc>,
    closed_at: Option<DateTime<Utc>>,
    status: SideQuestionSessionStatus,
    snapshot_summary: Value,
    requests: Vec<SideQuestionRequest>,
}

#[derive(Debug, Serialize)]
pub(crate) struct SessionsResponse {
    sessions: Vec<SessionView>,
}

/// Builds the JSON frozen at session creation:
/// `{task: {id, goal, state, bound, pins, created_at}, goals, facts, activity}`.
///
/// Goals carry their full row (including the proof fact id); facts carry the
/// advisory-relevant trio (`claim`, `state`, `provenance`); activity is the
/// last [`SIDE_QUESTION_ACTIVITY_LIMIT`] rows, including only what the
/// activity row itself holds.
pub async fn build_side_question_snapshot(
    store: &dyn GraphStore,
    task: &Task,
) -> anyhow::Result<Value> {
    let goals = store.list_goals(task.id).await?;
    let facts = store.list_facts(task.id).await?;
    let activity = store.list_activity(task.id, None).await?;
    Ok(dhi_followups::build_snapshot(
        task, &goals, &facts, &activity,
    ))
}

/// A short, non-sensitive description of what the snapshot froze.
fn snapshot_summary(snapshot: &Value) -> Value {
    dhi_followups::snapshot_summary(snapshot)
}

/// `GET /api/tasks/{id}/side-questions`: every session, newest first, each
/// with its question/answer rows oldest first. The full snapshot stays in the
/// store; the list carries only `snapshot_summary`.
pub(crate) async fn list_side_questions(
    State(state): State<AppState>,
    Path(id): Path<TaskId>,
) -> Result<Json<SessionsResponse>, ApiError> {
    require_task(state.store.as_ref(), id).await?;
    let threads = state
        .store
        .list_side_question_sessions(id)
        .await
        .map_err(ApiError::internal)?;
    let sessions = threads
        .into_iter()
        .map(|thread| SessionView {
            id: thread.session.id,
            created_at: thread.session.created_at,
            closed_at: thread.session.closed_at,
            status: thread.session.status,
            snapshot_summary: snapshot_summary(&thread.session.snapshot),
            requests: thread.requests,
        })
        .collect();
    Ok(Json(SessionsResponse { sessions }))
}

/// `POST /api/tasks/{id}/side-questions`: asks one question.
///
/// With `session_id`, appends to that session (404 unknown or another task's,
/// 409 closed). Without it, creates a new session with a fresh frozen
/// snapshot. A model failure or the 120 s timeout is persisted as an `error`
/// request and answered `502`; the session remains usable.
pub(crate) async fn ask_side_question(
    State(state): State<AppState>,
    Path(id): Path<TaskId>,
    Json(body): Json<AskSideQuestionRequest>,
) -> Result<Json<AskSideQuestionResponse>, ApiError> {
    let task = require_task(state.store.as_ref(), id).await?;
    let question = dhi_followups::validate_question(&body.question).map_err(|err| {
        ApiError::bad_request(match err {
            "question is required" => "question must not be empty".to_string(),
            _ => format!("question must be at most {SIDE_QUESTION_MAX_CHARS} characters"),
        })
    })?;

    let session = match body.session_id {
        Some(session_id) => {
            let session = state
                .store
                .get_side_question_session(session_id)
                .await
                .map_err(ApiError::internal)?
                .filter(|session| session.task_id == id)
                .ok_or_else(|| {
                    ApiError::not_found(format!("side-question session not found: {session_id}"))
                })?;
            if session.status != SideQuestionSessionStatus::Open {
                return Err(ApiError::conflict(format!(
                    "side-question session {session_id} is closed"
                )));
            }
            session
        }
        None => {
            let snapshot = build_side_question_snapshot(state.store.as_ref(), &task)
                .await
                .map_err(ApiError::internal)?;
            state
                .store
                .create_side_question_session(id, snapshot)
                .await
                .map_err(ApiError::internal)?
        }
    };

    // The frozen snapshot is the only context the answerer sees. It is read
    // back from the session row, never rebuilt from live tables.
    let context = serde_json::to_string_pretty(&session.snapshot).map_err(ApiError::internal)?;
    let input = SideQuestionInput {
        context,
        question: question.clone(),
        worker_profile_id: task.pinned_worker_profile_id,
    };

    let started = Instant::now();
    let answered =
        tokio::time::timeout(SIDE_QUESTION_TIMEOUT, state.agent.side_answer(input)).await;

    match answered {
        Ok(Ok(call)) => {
            let elapsed = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
            let record = call.meta.calls.last();
            let request = state
                .store
                .append_side_question(NewSideQuestionRequest {
                    session_id: session.id,
                    question,
                    answer: Some(call.output.clone()),
                    status: SideQuestionStatus::Ok,
                    error: None,
                    model: record.map(|call| call.model.clone()),
                    tokens_in: Some(as_i32(call.meta.usage.input_tokens)),
                    tokens_out: Some(as_i32(call.meta.usage.output_tokens)),
                    latency_ms: Some(record.map_or(elapsed, |call| call.latency_ms)),
                })
                .await
                .map_err(ApiError::internal)?;
            Ok(Json(AskSideQuestionResponse {
                session_id: session.id,
                request,
                answer: call.output,
            }))
        }
        Ok(Err(err)) => {
            let message = format!("side-question answer failed: {err:#}");
            persist_error(&state, session.id, question, &message, started).await?;
            Err(ApiError::bad_gateway(message))
        }
        Err(_) => {
            let message = format!(
                "side-question answer timed out after {} seconds",
                SIDE_QUESTION_TIMEOUT.as_secs()
            );
            persist_error(&state, session.id, question, &message, started).await?;
            Err(ApiError::bad_gateway(message))
        }
    }
}

/// Records a failed attempt, then hands the caller the `502` to return.
async fn persist_error(
    state: &AppState,
    session_id: Uuid,
    question: String,
    message: &str,
    started: Instant,
) -> Result<(), ApiError> {
    let elapsed = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    state
        .store
        .append_side_question(NewSideQuestionRequest {
            session_id,
            question,
            answer: None,
            status: SideQuestionStatus::Error,
            error: Some(message.to_string()),
            model: None,
            tokens_in: None,
            tokens_out: None,
            latency_ms: Some(elapsed),
        })
        .await
        .map_err(ApiError::internal)?;
    Ok(())
}

fn as_i32(value: i64) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}
