//! Plan types shared by the planner and the orchestration runtimes.
//!
//! The planner proposes one [`IntentPlan`] per intent: topology, workers,
//! subtasks, and capability limits. Validation is typed by
//! [`OrchestrationError`] and covers topology worker counts, single-worker
//! subtasks, unique worker keys, known assigned workers, and non-zero limits.
//! Manager-worker and swarm plans may start with zero subtasks: the lead or
//! team creates work after the plan validates.

use std::collections::HashSet;

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
///
/// Missing fields fall back to [`CapabilityLimits::default`] so a planner may
/// supply only the limits it wants to tighten.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
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
    /// Seed tasks. Manager-worker and swarm plans may leave this empty: the
    /// lead or team creates tasks dynamically after the plan validates.
    #[serde(default)]
    pub subtasks: Vec<Subtask>,
    #[serde(default)]
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
        if self.topology == Topology::Single && self.subtasks.is_empty() {
            return Err(OrchestrationError::NoSubtasks);
        }

        match self.topology {
            Topology::Single if self.workers.len() != 1 => {
                return Err(OrchestrationError::SingleRequiresOneWorker {
                    topology: self.topology,
                    actual: self.workers.len(),
                });
            }
            Topology::ManagerWorker
                if self.workers.is_empty() || self.workers.len() > self.limits.max_workers =>
            {
                return Err(OrchestrationError::WorkerCountOutOfRange {
                    topology: self.topology,
                    max_workers: self.limits.max_workers,
                    actual: self.workers.len(),
                });
            }
            Topology::Swarm
                if self.workers.len() < crate::SWARM_MIN_WORKERS
                    || self.workers.len() > self.limits.max_workers =>
            {
                return Err(OrchestrationError::SwarmWorkerCountOutOfRange {
                    min_workers: crate::SWARM_MIN_WORKERS,
                    max_workers: self.limits.max_workers,
                    actual: self.workers.len(),
                });
            }
            _ => {}
        }

        let mut keys = HashSet::with_capacity(self.workers.len());
        for worker in &self.workers {
            if worker.max_turns == 0 {
                return Err(OrchestrationError::ZeroWorkerTurns {
                    worker_key: worker.key.clone(),
                });
            }
            if !keys.insert(worker.key.as_str()) {
                return Err(OrchestrationError::DuplicateWorkerKey {
                    key: worker.key.clone(),
                });
            }
            // The manager is the runtime selector, not a worker. A plan that
            // declares a `manager` worker makes the selector choose itself and
            // adds a fourth result for a three-worker intent. Reject it
            // instead of silently running a wider team than the goal asked
            // for.
            if self.topology == Topology::ManagerWorker
                && (worker.key.eq_ignore_ascii_case("manager")
                    || worker.role.eq_ignore_ascii_case("manager"))
            {
                return Err(OrchestrationError::ManagerIsNotAWorker {
                    worker_key: worker.key.clone(),
                });
            }
        }

        for subtask in &self.subtasks {
            if let Some(key) = subtask.worker_key.as_deref()
                && !keys.contains(key)
            {
                return Err(OrchestrationError::UnknownWorker {
                    subtask_id: subtask.id.clone(),
                    worker_key: key.to_owned(),
                });
            }
        }

        if self.topology == Topology::Swarm {
            // A swarm seeds one turn per worker. More subtasks than workers
            // would be silently dropped by the runner, so reject the plan
            // instead of confirming a fact that omits planned work.
            if self.subtasks.len() > self.workers.len() {
                return Err(OrchestrationError::SwarmSubtaskCount {
                    workers: self.workers.len(),
                    subtasks: self.subtasks.len(),
                });
            }
            // A worker can seed only one subtask. Duplicate explicit
            // assignments would leave one of the declared subtasks unseeded.
            let mut assigned = HashSet::with_capacity(self.subtasks.len());
            for subtask in &self.subtasks {
                if let Some(key) = subtask.worker_key.as_deref()
                    && !assigned.insert(key)
                {
                    return Err(OrchestrationError::SwarmDuplicateAssignment {
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
    #[error("single topology requires at least one subtask")]
    NoSubtasks,
    #[error("{topology} topology requires exactly one worker, got {actual}")]
    SingleRequiresOneWorker { topology: Topology, actual: usize },
    #[error("{topology} topology requires 1..={max_workers} workers, got {actual}")]
    WorkerCountOutOfRange {
        topology: Topology,
        max_workers: usize,
        actual: usize,
    },
    #[error("swarm topology requires {min_workers}..={max_workers} workers, got {actual}")]
    SwarmWorkerCountOutOfRange {
        min_workers: usize,
        max_workers: usize,
        actual: usize,
    },
    #[error("worker {worker_key:?} has zero max_turns")]
    ZeroWorkerTurns { worker_key: String },
    #[error("worker key {key:?} is declared more than once")]
    DuplicateWorkerKey { key: String },
    #[error(
        "manager_worker worker {worker_key:?} uses the reserved manager role; \
         the manager is the runtime selector, not a worker"
    )]
    ManagerIsNotAWorker { worker_key: String },
    #[error("subtask {subtask_id:?} names unknown worker {worker_key:?}")]
    UnknownWorker {
        subtask_id: String,
        worker_key: String,
    },
    #[error(
        "swarm topology has {subtasks} subtasks for {workers} workers; a swarm can seed at most one subtask per worker"
    )]
    SwarmSubtaskCount { workers: usize, subtasks: usize },
    #[error("swarm worker {worker_key:?} is assigned more than one seed subtask")]
    SwarmDuplicateAssignment { worker_key: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worker(key: &str) -> WorkerSpec {
        WorkerSpec {
            key: key.to_owned(),
            role: "role".to_owned(),
            prompt: "prompt".to_owned(),
            max_turns: 1,
        }
    }

    fn subtask(id: &str, worker_key: Option<&str>) -> Subtask {
        Subtask {
            id: id.to_owned(),
            description: "do it".to_owned(),
            worker_key: worker_key.map(str::to_owned),
        }
    }

    #[test]
    fn limits_deserialize_with_defaults() {
        let limits: CapabilityLimits = serde_json::from_str("{\"max_workers\": 2}").unwrap();
        assert_eq!(
            limits,
            CapabilityLimits {
                max_workers: 2,
                ..CapabilityLimits::default()
            }
        );

        let plan: IntentPlan = serde_json::from_value(serde_json::json!({
            "intent": "ship it",
            "topology": "single",
            "workers": [{
                "key": "a",
                "role": "worker",
                "prompt": "do it",
                "max_turns": 1
            }],
            "subtasks": [{"id": "s0", "description": "do it"}]
        }))
        .unwrap();
        assert_eq!(plan.limits, CapabilityLimits::default());
    }

    #[test]
    fn manager_worker_and_swarm_may_start_with_zero_seed_subtasks() {
        for topology in [Topology::ManagerWorker, Topology::Swarm] {
            let plan = IntentPlan {
                intent: "ship it".to_owned(),
                topology,
                workers: vec![worker("a"), worker("b")],
                subtasks: vec![],
                limits: CapabilityLimits::default(),
            };
            assert_eq!(plan.validate(), Ok(()), "{topology} may start empty");
        }
    }

    #[test]
    fn fixed_swarm_nine_with_raised_limits_validates_empty() {
        let workers = (0..9)
            .map(|index| worker(&format!("w{index}")))
            .collect::<Vec<_>>();
        let plan = IntentPlan {
            intent: "ship it".to_owned(),
            topology: Topology::Swarm,
            workers,
            subtasks: vec![],
            limits: CapabilityLimits {
                max_workers: 9,
                max_parallel: 9,
                ..CapabilityLimits::default()
            },
        };
        assert_eq!(plan.validate(), Ok(()));
    }

    #[test]
    fn swarm_requires_at_least_two_workers() {
        let plan = IntentPlan {
            intent: "ship it".to_owned(),
            topology: Topology::Swarm,
            workers: vec![worker("a")],
            subtasks: vec![],
            limits: CapabilityLimits::default(),
        };
        assert_eq!(
            plan.validate(),
            Err(OrchestrationError::SwarmWorkerCountOutOfRange {
                min_workers: crate::SWARM_MIN_WORKERS,
                max_workers: CapabilityLimits::default().max_workers,
                actual: 1,
            })
        );
    }

    #[test]
    fn manager_worker_still_accepts_one_worker() {
        let plan = IntentPlan {
            intent: "ship it".to_owned(),
            topology: Topology::ManagerWorker,
            workers: vec![worker("a")],
            subtasks: vec![],
            limits: CapabilityLimits::default(),
        };
        assert_eq!(plan.validate(), Ok(()));
    }

    #[test]
    fn single_still_requires_a_subtask() {
        let plan = IntentPlan {
            intent: "ship it".to_owned(),
            topology: Topology::Single,
            workers: vec![worker("a")],
            subtasks: vec![],
            limits: CapabilityLimits::default(),
        };
        assert_eq!(plan.validate(), Err(OrchestrationError::NoSubtasks));
    }

    #[test]
    fn plan_without_subtasks_field_deserializes_as_empty() {
        let plan: IntentPlan = serde_json::from_value(serde_json::json!({
            "intent": "ship it",
            "topology": "manager_worker",
            "workers": [{
                "key": "a",
                "role": "worker",
                "prompt": "do it",
                "max_turns": 1
            }]
        }))
        .unwrap();
        assert!(plan.subtasks.is_empty());
        assert_eq!(plan.validate(), Ok(()));
    }

    #[test]
    fn topology_is_snake_case() {
        assert_eq!(
            serde_json::to_string(&Topology::ManagerWorker).unwrap(),
            "\"manager_worker\""
        );
    }

    #[test]
    fn validation_rejects_a_zero_turn_worker() {
        let mut worker = worker("a");
        worker.max_turns = 0;
        let plan = IntentPlan {
            intent: "ship it".to_owned(),
            topology: Topology::ManagerWorker,
            workers: vec![worker],
            subtasks: vec![subtask("s0", None)],
            limits: CapabilityLimits::default(),
        };
        assert_eq!(
            plan.validate(),
            Err(OrchestrationError::ZeroWorkerTurns {
                worker_key: "a".to_owned(),
            })
        );
    }

    #[test]
    fn swarm_rejects_more_subtasks_than_workers() {
        let plan = IntentPlan {
            intent: "ship it".to_owned(),
            topology: Topology::Swarm,
            workers: vec![worker("a"), worker("b")],
            subtasks: vec![
                subtask("s0", None),
                subtask("s1", None),
                subtask("s2", None),
            ],
            limits: CapabilityLimits::default(),
        };
        assert_eq!(
            plan.validate(),
            Err(OrchestrationError::SwarmSubtaskCount {
                workers: 2,
                subtasks: 3,
            })
        );
    }

    #[test]
    fn swarm_rejects_duplicate_seed_assignments() {
        let plan = IntentPlan {
            intent: "ship it".to_owned(),
            topology: Topology::Swarm,
            workers: vec![worker("a"), worker("b")],
            subtasks: vec![subtask("s0", Some("a")), subtask("s1", Some("a"))],
            limits: CapabilityLimits::default(),
        };
        assert_eq!(
            plan.validate(),
            Err(OrchestrationError::SwarmDuplicateAssignment {
                worker_key: "a".to_owned(),
            })
        );
    }

    #[test]
    fn manager_worker_rejects_a_manager_worker() {
        let mut manager = worker("manager");
        manager.role = "manager".to_owned();
        let plan = IntentPlan {
            intent: "ship it".to_owned(),
            topology: Topology::ManagerWorker,
            workers: vec![worker("alpha"), manager],
            subtasks: vec![subtask("s0", Some("alpha"))],
            limits: CapabilityLimits::default(),
        };
        assert_eq!(
            plan.validate(),
            Err(OrchestrationError::ManagerIsNotAWorker {
                worker_key: "manager".to_owned(),
            })
        );
    }

    #[test]
    fn validation_rejects_unknown_worker() {
        let plan = IntentPlan {
            intent: "ship it".to_owned(),
            topology: Topology::ManagerWorker,
            workers: vec![worker("a")],
            subtasks: vec![subtask("s0", Some("ghost"))],
            limits: CapabilityLimits::default(),
        };
        assert_eq!(
            plan.validate(),
            Err(OrchestrationError::UnknownWorker {
                subtask_id: "s0".to_owned(),
                worker_key: "ghost".to_owned(),
            })
        );
    }
}
