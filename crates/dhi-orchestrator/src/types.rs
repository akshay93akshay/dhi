//! Plan and result types shared by the manager and swarm runtimes.

use std::collections::HashSet;

use dhi_agent::FactDraft;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// How a plan distributes work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Topology {
    /// One worker handles the whole intent.
    Single,
    /// A manager assigns subtasks to workers up front.
    ManagerWorker,
    /// Peers seed one task each and exchange findings directly.
    Swarm,
}

impl Topology {
    /// Stable snake-case name used in errors and serialized plans.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::ManagerWorker => "manager_worker",
            Self::Swarm => "swarm",
        }
    }
}

impl std::fmt::Display for Topology {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Runtime budgets for one intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityLimits {
    pub max_workers: usize,
    pub max_parallel: usize,
    pub max_rounds: usize,
    pub max_messages: usize,
}

impl Default for CapabilityLimits {
    fn default() -> Self {
        Self {
            max_workers: 8,
            max_parallel: 4,
            max_rounds: 4,
            max_messages: 64,
        }
    }
}

/// One worker declaration in a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerSpec {
    pub key: String,
    pub role: String,
    pub prompt: String,
    pub max_turns: usize,
}

/// One unit of work a worker may execute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subtask {
    pub id: String,
    pub description: String,
    /// `None` lets the runner assign the subtask round-robin.
    pub worker_key: Option<String>,
}

/// The complete plan for one intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentPlan {
    pub intent: String,
    pub topology: Topology,
    pub workers: Vec<WorkerSpec>,
    pub subtasks: Vec<Subtask>,
    pub limits: CapabilityLimits,
}

impl IntentPlan {
    /// Reject a plan the runners cannot execute safely.
    pub fn validate(&self) -> Result<(), OrchestrationError> {
        if self.limits.max_workers == 0 {
            return Err(OrchestrationError::ZeroMaxWorkers);
        }
        if self.limits.max_parallel == 0 {
            return Err(OrchestrationError::ZeroMaxParallel);
        }
        if self.limits.max_rounds == 0 {
            return Err(OrchestrationError::ZeroMaxRounds);
        }
        if self.subtasks.is_empty() {
            return Err(OrchestrationError::NoSubtasks);
        }

        match self.topology {
            Topology::Single if self.workers.len() != 1 => {
                return Err(OrchestrationError::SingleRequiresOneWorker {
                    topology: self.topology,
                    actual: self.workers.len(),
                });
            }
            Topology::ManagerWorker | Topology::Swarm
                if self.workers.is_empty() || self.workers.len() > self.limits.max_workers =>
            {
                return Err(OrchestrationError::WorkerCountOutOfRange {
                    topology: self.topology,
                    max_workers: self.limits.max_workers,
                    actual: self.workers.len(),
                });
            }
            _ => {}
        }

        let mut keys = HashSet::with_capacity(self.workers.len());
        for worker in &self.workers {
            if !keys.insert(worker.key.as_str()) {
                return Err(OrchestrationError::DuplicateWorkerKey {
                    key: worker.key.clone(),
                });
            }
        }

        for subtask in &self.subtasks {
            if let Some(key) = subtask.worker_key.as_deref() {
                if !keys.contains(key) {
                    return Err(OrchestrationError::UnknownWorker {
                        subtask_id: subtask.id.clone(),
                        worker_key: key.to_owned(),
                    });
                }
            }
        }

        Ok(())
    }
}

/// Typed plan validation failure.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum OrchestrationError {
    #[error("max_workers must be greater than zero")]
    ZeroMaxWorkers,
    #[error("max_parallel must be greater than zero")]
    ZeroMaxParallel,
    #[error("max_rounds must be greater than zero")]
    ZeroMaxRounds,
    #[error("plan must contain at least one subtask")]
    NoSubtasks,
    #[error("{topology} topology requires exactly one worker, got {actual}")]
    SingleRequiresOneWorker { topology: Topology, actual: usize },
    #[error("{topology} topology requires 1..={max_workers} workers, got {actual}")]
    WorkerCountOutOfRange {
        topology: Topology,
        max_workers: usize,
        actual: usize,
    },
    #[error("worker key {key:?} is declared more than once")]
    DuplicateWorkerKey { key: String },
    #[error("subtask {subtask_id:?} names unknown worker {worker_key:?}")]
    UnknownWorker {
        subtask_id: String,
        worker_key: String,
    },
}

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerResult {
    pub worker_key: String,
    pub subtask_id: String,
    pub status: WorkerStatus,
    pub summary: String,
    pub evidence: String,
    pub error: Option<String>,
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
        }
    }

    /// Whether this result contributes a finding.
    pub fn is_success(&self) -> bool {
        self.status == WorkerStatus::Ok
    }
}

/// The single result of one intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentResult {
    pub summary: String,
    pub fact: FactDraft,
    pub worker_results: Vec<WorkerResult>,
}
