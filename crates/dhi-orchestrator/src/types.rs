//! Plan and result types shared by the manager and swarm runtimes.

use dhi_agent::FactDraft;
use serde::{Deserialize, Serialize};

pub use dhi_core::{
    CapabilityLimits, IntentPlan, OrchestrationError, Subtask, Topology, WorkerSpec,
};

/// Final state of one worker execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerStatus {
    Ok,
    Failed,
    Skipped,
}

impl WorkerStatus {
    /// Stable rank used for deterministic ordering.
    pub(crate) fn rank(self) -> u8 {
        match self {
            Self::Ok => 0,
            Self::Failed => 1,
            Self::Skipped => 2,
        }
    }
}

impl std::fmt::Display for WorkerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ok => f.write_str("ok"),
            Self::Failed => f.write_str("failed"),
            Self::Skipped => f.write_str("skipped"),
        }
    }
}

/// One worker's outcome for one subtask.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerResult {
    pub worker_key: String,
    pub subtask_id: String,
    pub status: WorkerStatus,
    pub summary: String,
    pub evidence: String,
    pub error: Option<String>,
    /// Usage records for every completion call the worker made.
    #[serde(default)]
    pub call_meta: dhi_agent::AgentCallMeta,
    /// The rendered prompt the worker's runtime returned.
    #[serde(default)]
    pub prompt: String,
    /// The final response text the worker's runtime returned.
    #[serde(default)]
    pub response: String,
    /// The provider's final reasoning text, when it returned any.
    #[serde(default)]
    pub thinking: Option<String>,
}

impl WorkerResult {
    /// A successful result.
    pub fn ok(
        worker_key: impl Into<String>,
        subtask_id: impl Into<String>,
        summary: impl Into<String>,
        evidence: impl Into<String>,
    ) -> Self {
        Self {
            worker_key: worker_key.into(),
            subtask_id: subtask_id.into(),
            status: WorkerStatus::Ok,
            summary: summary.into(),
            evidence: evidence.into(),
            error: None,
            call_meta: dhi_agent::AgentCallMeta::default(),
            prompt: String::new(),
            response: String::new(),
            thinking: None,
        }
    }

    /// A failed result. The error is kept for the aggregate evidence.
    pub fn failed(
        worker_key: impl Into<String>,
        subtask_id: impl Into<String>,
        error: impl Into<String>,
    ) -> Self {
        Self {
            worker_key: worker_key.into(),
            subtask_id: subtask_id.into(),
            status: WorkerStatus::Failed,
            summary: String::new(),
            evidence: String::new(),
            error: Some(error.into()),
            call_meta: dhi_agent::AgentCallMeta::default(),
            prompt: String::new(),
            response: String::new(),
            thinking: None,
        }
    }

    /// A result the worker declined to produce.
    pub fn skipped(
        worker_key: impl Into<String>,
        subtask_id: impl Into<String>,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            worker_key: worker_key.into(),
            subtask_id: subtask_id.into(),
            status: WorkerStatus::Skipped,
            summary: summary.into(),
            evidence: String::new(),
            error: None,
            call_meta: dhi_agent::AgentCallMeta::default(),
            prompt: String::new(),
            response: String::new(),
            thinking: None,
        }
    }

    /// Whether this result contributes a finding.
    pub fn is_success(&self) -> bool {
        self.status == WorkerStatus::Ok
    }
}

/// The single result of one intent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntentResult {
    pub summary: String,
    pub fact: FactDraft,
    pub worker_results: Vec<WorkerResult>,
}
