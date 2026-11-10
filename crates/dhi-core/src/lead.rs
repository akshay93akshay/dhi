//! Lead-loop contract for the manager-worker topology.
//!
//! The lead is the runtime decision maker of a manager-worker intent. It sees
//! the intent, the worker pool, every task assigned so far, every worker
//! result, the round, the configured limits, and the remaining budget.
//! It returns exactly one [`LeadDecision`]. The orchestrator executes that
//! decision and calls the lead again; this module contains only types and
//! validation, never a runtime or a model call.
//!
//! The lead is not a worker. It never appears in the worker pool, never
//! produces a worker result, and never writes a fact itself. A `Final`
//! decision supplies the one fact draft of the intent; the worker results are
//! its evidence.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{CapabilityLimits, WorkerSpec};

/// Runtime budgets of one lead loop.
///
/// The orchestrator derives these from the planner's [`CapabilityLimits`] and
/// may tighten them with `ManagerRunner::with_lead_limits`. Every field must be
/// greater than zero; a decision that would consume a budget past its
/// remaining value is rejected before any worker runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LeadLimits {
    /// Maximum number of task rounds, including the planner's initial round.
    pub max_rounds: usize,
    /// Maximum number of lead reviews.
    pub max_lead_turns: usize,
    /// Maximum total worker executions across all rounds and `AskWorker` calls.
    pub max_worker_runs: usize,
    /// Maximum workers running at once.
    pub max_parallel: usize,
    /// Size of the fixed worker pool. The lead can only assign existing keys.
    pub max_workers: usize,
}

impl Default for LeadLimits {
    fn default() -> Self {
        Self::from_capabilities(&CapabilityLimits::default(), 0)
    }
}

impl LeadLimits {
    /// Derive the loop budget from the planner's plan limits.
    ///
    /// A lead gets two reviews per round: one after each round plus room for a
    /// narrow `AskWorker`. The worker-run budget is the plan's worker x round
    /// product, but never below the initial task count, so a valid plan can
    /// always run its first round.
    pub fn from_capabilities(limits: &CapabilityLimits, initial_tasks: usize) -> Self {
        let max_rounds = limits.max_rounds.max(1);
        let max_workers = limits.max_workers.max(1);
        Self {
            max_rounds,
            max_lead_turns: max_rounds.saturating_mul(2).max(1),
            max_worker_runs: max_workers
                .saturating_mul(max_rounds)
                .max(initial_tasks)
                .max(1),
            max_parallel: limits.max_parallel.max(1),
            max_workers,
        }
    }
}

/// The budget left when the lead receives one [`LeadInput`].
///
/// The counters exclude the current review: `remaining_lead_turns` is what
/// remains after this review returns. A `Continue` or `AskWorker` therefore
/// requires a non-zero value, because the loop must call the lead again to
/// hear the result.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeadBudget {
    pub remaining_rounds: usize,
    pub remaining_lead_turns: usize,
    pub remaining_worker_runs: usize,
}

/// One directly assigned unit of work.
///
/// `assignee` is a worker key from the fixed pool. `depends_on` optionally
/// names task ids from this batch or from earlier results; the runner waits
/// for those tasks before it runs this one and skips it when a dependency
/// failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeadTask {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub assignee: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

/// The status of one worker execution as the lead sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeadWorkerStatus {
    Ok,
    Failed,
    Skipped,
}

impl LeadWorkerStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

impl std::fmt::Display for LeadWorkerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The worker-result view the lead receives.
///
/// This is a snapshot of the orchestrator's `WorkerResult`, kept in core so
/// the runtime in `dhi-agent` can build the prompt without depending on the
/// orchestrator crate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeadWorkerResult {
    pub worker_key: String,
    pub task_id: String,
    pub status: LeadWorkerStatus,
    pub summary: String,
    pub evidence: String,
    pub error: Option<String>,
}

/// The exact input of one lead review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeadInput {
    /// The intent the plan executes.
    pub intent: String,
    /// The planner's fixed worker pool. The lead may only name these keys.
    pub workers: Vec<WorkerSpec>,
    /// The tasks assigned so far: the planner's initial tasks first, then the
    /// tasks of every accepted `Continue`, in assignment order.
    pub tasks: Vec<LeadTask>,
    /// Every worker result collected so far, in execution order.
    pub results: Vec<LeadWorkerResult>,
    /// One-based task round. The planner's initial tasks are round 1.
    pub round: usize,
    /// The loop budgets.
    pub limits: LeadLimits,
    /// What remains before this review's decision.
    pub remaining: LeadBudget,
}

/// One lead decision.
///
/// Serialized with a `decision` tag: `final`, `continue`, `ask_worker`, or
/// `stop`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum LeadDecision {
    /// The intent is complete. This decision supplies the intent's one fact.
    Final {
        summary: String,
        #[serde(default)]
        detail: String,
        #[serde(default)]
        evidence: String,
        /// `observed` or `inferred`.
        confidence: String,
    },
    /// Run another round of directly assigned tasks.
    Continue {
        tasks: Vec<LeadTask>,
        reason: String,
    },
    /// Run one pool worker with a specific prompt and return its result.
    AskWorker { worker_key: String, prompt: String },
    /// End the intent without a fact. The caller sees a stopped error.
    Stop { reason: String },
}

impl LeadDecision {
    /// Reject a decision the runner cannot execute safely.
    ///
    /// This is the shared validation point for the live runtime and the
    /// runner. It checks the decision's shape, worker keys, task ids,
    /// dependencies, dependency cycles, and every remaining budget.
    pub fn validate(&self, input: &LeadInput) -> Result<(), LeadError> {
        match self {
            Self::Final {
                summary,
                confidence,
                ..
            } => {
                if summary.trim().is_empty() {
                    return Err(LeadError::EmptySummary);
                }
                if confidence != "observed" && confidence != "inferred" {
                    return Err(LeadError::InvalidConfidence(confidence.clone()));
                }
            }
            Self::Continue { tasks, .. } => {
                if tasks.is_empty() {
                    return Err(LeadError::NoTasks);
                }
                if input.remaining.remaining_rounds == 0 {
                    return Err(LeadError::NoRemainingRounds);
                }
                if input.remaining.remaining_lead_turns == 0 {
                    return Err(LeadError::NoRemainingTurns);
                }
                if tasks.len() > input.remaining.remaining_worker_runs {
                    return Err(LeadError::WorkerRunsExceeded {
                        requested: tasks.len(),
                        remaining: input.remaining.remaining_worker_runs,
                    });
                }

                let workers: HashSet<&str> = input
                    .workers
                    .iter()
                    .map(|worker| worker.key.as_str())
                    .collect();
                let result_ids: HashSet<&str> = input
                    .results
                    .iter()
                    .map(|result| result.task_id.as_str())
                    .collect();
                let mut batch_ids = HashSet::with_capacity(tasks.len());
                for task in tasks {
                    if task.id.trim().is_empty() {
                        return Err(LeadError::EmptyTaskId);
                    }
                    if result_ids.contains(task.id.as_str()) || !batch_ids.insert(task.id.as_str())
                    {
                        return Err(LeadError::DuplicateTaskId(task.id.clone()));
                    }
                    if task.title.trim().is_empty() && task.description.trim().is_empty() {
                        return Err(LeadError::EmptyTaskText {
                            task_id: task.id.clone(),
                        });
                    }
                    if task.assignee.trim().is_empty() {
                        return Err(LeadError::EmptyAssignee {
                            task_id: task.id.clone(),
                        });
                    }
                    if !workers.contains(task.assignee.as_str()) {
                        return Err(LeadError::UnknownWorker {
                            task_id: task.id.clone(),
                            worker_key: task.assignee.clone(),
                        });
                    }
                    for dependency in &task.depends_on {
                        if dependency == &task.id {
                            return Err(LeadError::SelfDependency {
                                task_id: task.id.clone(),
                            });
                        }
                        if !result_ids.contains(dependency.as_str())
                            && !batch_ids.contains(dependency.as_str())
                            && !tasks.iter().any(|candidate| &candidate.id == dependency)
                        {
                            return Err(LeadError::UnknownDependency {
                                task_id: task.id.clone(),
                                dependency: dependency.clone(),
                            });
                        }
                    }
                }
                if let Some(task_id) = cycle_member(tasks) {
                    return Err(LeadError::DependencyCycle(task_id));
                }
            }
            Self::AskWorker { worker_key, prompt } => {
                if input.remaining.remaining_worker_runs == 0 {
                    return Err(LeadError::NoRemainingRuns);
                }
                if input.remaining.remaining_lead_turns == 0 {
                    return Err(LeadError::NoRemainingTurns);
                }
                if worker_key.trim().is_empty() {
                    return Err(LeadError::EmptyWorkerKey);
                }
                if !input.workers.iter().any(|worker| worker.key == *worker_key) {
                    return Err(LeadError::UnknownAskWorker {
                        worker_key: worker_key.clone(),
                    });
                }
                if prompt.trim().is_empty() {
                    return Err(LeadError::EmptyPrompt);
                }
            }
            Self::Stop { reason } => {
                if reason.trim().is_empty() {
                    return Err(LeadError::EmptyStopReason);
                }
            }
        }
        Ok(())
    }
}

/// One task id that participates in a dependency cycle inside one batch.
fn cycle_member(tasks: &[LeadTask]) -> Option<String> {
    let mut remaining: HashSet<&str> = tasks.iter().map(|task| task.id.as_str()).collect();
    loop {
        if remaining.is_empty() {
            return None;
        }
        let ready: Vec<&str> = tasks
            .iter()
            .filter(|task| remaining.contains(task.id.as_str()))
            .filter(|task| {
                task.depends_on
                    .iter()
                    .all(|dependency| !remaining.contains(dependency.as_str()))
            })
            .map(|task| task.id.as_str())
            .collect();
        if ready.is_empty() {
            return remaining.iter().next().map(|id| (*id).to_owned());
        }
        for id in ready {
            remaining.remove(id);
        }
    }
}

/// A rejected lead decision.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LeadError {
    #[error("lead limit {name} must be greater than zero")]
    ZeroLimit { name: &'static str },
    #[error("lead final summary is empty")]
    EmptySummary,
    #[error("lead final confidence must be `observed` or `inferred`, got {0:?}")]
    InvalidConfidence(String),
    #[error("lead continue must name at least one task")]
    NoTasks,
    #[error("lead continue requested but no task rounds remain")]
    NoRemainingRounds,
    #[error("lead continue names {requested} tasks but only {remaining} worker runs remain")]
    WorkerRunsExceeded { requested: usize, remaining: usize },
    #[error("lead task id is empty")]
    EmptyTaskId,
    #[error("lead task id {0:?} is declared more than once in one batch")]
    DuplicateTaskId(String),
    #[error("lead task {task_id:?} needs a title or a description")]
    EmptyTaskText { task_id: String },
    #[error("lead task {task_id:?} needs an assignee")]
    EmptyAssignee { task_id: String },
    #[error("lead task {task_id:?} names unknown worker {worker_key:?}")]
    UnknownWorker { task_id: String, worker_key: String },
    #[error("lead task {task_id:?} names unknown dependency {dependency:?}")]
    UnknownDependency { task_id: String, dependency: String },
    #[error("lead task {task_id:?} depends on itself")]
    SelfDependency { task_id: String },
    #[error("lead task dependencies contain a cycle through {0:?}")]
    DependencyCycle(String),
    #[error("lead ask_worker needs a worker key")]
    EmptyWorkerKey,
    #[error("lead ask_worker names unknown worker {worker_key:?}")]
    UnknownAskWorker { worker_key: String },
    #[error("lead ask_worker needs a non-empty prompt")]
    EmptyPrompt,
    #[error("lead ask_worker requested but no worker runs remain")]
    NoRemainingRuns,
    #[error("lead decision requested but no lead turns remain")]
    NoRemainingTurns,
    #[error("lead stop needs a reason")]
    EmptyStopReason,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worker(key: &str) -> WorkerSpec {
        WorkerSpec {
            key: key.to_owned(),
            role: "worker".to_owned(),
            prompt: "do work".to_owned(),
            max_turns: 1,
        }
    }

    fn input() -> LeadInput {
        LeadInput {
            intent: "ship it".to_owned(),
            workers: vec![worker("alpha"), worker("beta")],
            tasks: vec![],
            results: vec![],
            round: 1,
            limits: LeadLimits::default(),
            remaining: LeadBudget {
                remaining_rounds: 3,
                remaining_lead_turns: 4,
                remaining_worker_runs: 8,
            },
        }
    }

    fn task(id: &str, assignee: &str) -> LeadTask {
        LeadTask {
            id: id.to_owned(),
            title: format!("do {id}"),
            description: String::new(),
            assignee: assignee.to_owned(),
            depends_on: vec![],
        }
    }

    #[test]
    fn decision_wire_shape_is_tagged() {
        let final_decision = LeadDecision::Final {
            summary: "done".to_owned(),
            detail: String::new(),
            evidence: "worker alpha".to_owned(),
            confidence: "observed".to_owned(),
        };
        let value = serde_json::to_value(&final_decision).unwrap();
        assert_eq!(value["decision"], "final");
        assert_eq!(value["summary"], "done");

        let ask = LeadDecision::AskWorker {
            worker_key: "alpha".to_owned(),
            prompt: "check the edge case".to_owned(),
        };
        let value = serde_json::to_value(&ask).unwrap();
        assert_eq!(value["decision"], "ask_worker");
    }

    #[test]
    fn continue_accepts_direct_assignment_with_dependencies() {
        let mut second = task("t2", "beta");
        second.depends_on = vec!["t1".to_owned()];
        let decision = LeadDecision::Continue {
            tasks: vec![task("t1", "alpha"), second],
            reason: "two independent checks".to_owned(),
        };
        decision.validate(&input()).unwrap();
    }

    #[test]
    fn continue_rejects_unknown_workers_and_dependencies() {
        let decision = LeadDecision::Continue {
            tasks: vec![task("t1", "ghost")],
            reason: "bad".to_owned(),
        };
        assert_eq!(
            decision.validate(&input()).unwrap_err(),
            LeadError::UnknownWorker {
                task_id: "t1".to_owned(),
                worker_key: "ghost".to_owned(),
            }
        );

        let mut dependent = task("t2", "alpha");
        dependent.depends_on = vec!["missing".to_owned()];
        let decision = LeadDecision::Continue {
            tasks: vec![dependent],
            reason: "bad".to_owned(),
        };
        assert_eq!(
            decision.validate(&input()).unwrap_err(),
            LeadError::UnknownDependency {
                task_id: "t2".to_owned(),
                dependency: "missing".to_owned(),
            }
        );
    }

    #[test]
    fn continue_rejects_cycles_and_excess_worker_runs() {
        let mut first = task("t1", "alpha");
        first.depends_on = vec!["t2".to_owned()];
        let mut second = task("t2", "beta");
        second.depends_on = vec!["t1".to_owned()];
        let decision = LeadDecision::Continue {
            tasks: vec![first, second],
            reason: "cycle".to_owned(),
        };
        assert!(matches!(
            decision.validate(&input()).unwrap_err(),
            LeadError::DependencyCycle(_)
        ));

        let mut small = input();
        small.remaining.remaining_worker_runs = 1;
        let decision = LeadDecision::Continue {
            tasks: vec![task("t1", "alpha"), task("t2", "beta")],
            reason: "too many".to_owned(),
        };
        assert_eq!(
            decision.validate(&small).unwrap_err(),
            LeadError::WorkerRunsExceeded {
                requested: 2,
                remaining: 1,
            }
        );
    }

    #[test]
    fn final_and_stop_validate_their_text() {
        let final_decision = LeadDecision::Final {
            summary: String::new(),
            detail: String::new(),
            evidence: String::new(),
            confidence: "observed".to_owned(),
        };
        assert_eq!(
            final_decision.validate(&input()).unwrap_err(),
            LeadError::EmptySummary
        );

        let final_decision = LeadDecision::Final {
            summary: "done".to_owned(),
            detail: String::new(),
            evidence: String::new(),
            confidence: "guessed".to_owned(),
        };
        assert_eq!(
            final_decision.validate(&input()).unwrap_err(),
            LeadError::InvalidConfidence("guessed".to_owned())
        );

        let stop = LeadDecision::Stop {
            reason: String::new(),
        };
        assert_eq!(
            stop.validate(&input()).unwrap_err(),
            LeadError::EmptyStopReason
        );
    }

    #[test]
    fn ask_worker_requires_a_pool_worker_and_a_run() {
        let ask = LeadDecision::AskWorker {
            worker_key: "ghost".to_owned(),
            prompt: "check".to_owned(),
        };
        assert_eq!(
            ask.validate(&input()).unwrap_err(),
            LeadError::UnknownAskWorker {
                worker_key: "ghost".to_owned(),
            }
        );

        let mut spent = input();
        spent.remaining.remaining_worker_runs = 0;
        let ask = LeadDecision::AskWorker {
            worker_key: "alpha".to_owned(),
            prompt: "check".to_owned(),
        };
        assert_eq!(
            ask.validate(&spent).unwrap_err(),
            LeadError::NoRemainingRuns
        );
    }

    #[test]
    fn limits_derive_from_plan_limits() {
        let limits = CapabilityLimits {
            max_workers: 3,
            max_parallel: 2,
            max_rounds: 5,
            max_messages: 16,
        };
        let lead = LeadLimits::from_capabilities(&limits, 4);
        assert_eq!(lead.max_rounds, 5);
        assert_eq!(lead.max_lead_turns, 10);
        assert_eq!(lead.max_worker_runs, 15);
        assert_eq!(lead.max_parallel, 2);
    }
}
