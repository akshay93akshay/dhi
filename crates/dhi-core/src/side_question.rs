//! Side-question records.
//!
//! A side-question session is a read-only conversation about one task. Its
//! snapshot is frozen when the session is created and is never refreshed, so
//! answers can only describe the graph as it was at that moment. The records
//! live beside the task; they are not facts, intents, or goals.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::TaskId;

/// `side_question_sessions.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideQuestionSessionStatus {
    Open,
    Closed,
}

impl SideQuestionSessionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
        }
    }
}

/// `side_question_requests.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideQuestionStatus {
    Pending,
    Ok,
    Error,
}

impl SideQuestionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ok => "ok",
            Self::Error => "error",
        }
    }
}

/// One frozen conversation about a task. The `snapshot` is the exact JSON
/// handed to the answerer; nothing updates it after insert.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SideQuestionSession {
    pub id: uuid::Uuid,
    pub task_id: TaskId,
    pub snapshot: serde_json::Value,
    pub status: SideQuestionSessionStatus,
    pub created_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
}

/// One question and its completed answer attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SideQuestionRequest {
    pub id: uuid::Uuid,
    pub session_id: uuid::Uuid,
    pub question: String,
    pub answer: Option<String>,
    pub status: SideQuestionStatus,
    pub error: Option<String>,
    pub model: Option<String>,
    pub tokens_in: Option<i32>,
    pub tokens_out: Option<i32>,
    pub latency_ms: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

/// What a caller appends after an answer attempt: one row, already terminal.
/// A failed attempt is still persisted, with `status = error` and `error`.
#[derive(Debug, Clone)]
pub struct NewSideQuestionRequest {
    pub session_id: uuid::Uuid,
    pub question: String,
    pub answer: Option<String>,
    pub status: SideQuestionStatus,
    pub error: Option<String>,
    pub model: Option<String>,
    pub tokens_in: Option<i32>,
    pub tokens_out: Option<i32>,
    pub latency_ms: Option<i64>,
}

/// One session plus its requests, oldest request first.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SideQuestionThread {
    pub session: SideQuestionSession,
    pub requests: Vec<SideQuestionRequest>,
}
