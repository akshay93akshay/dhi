//! Manager worker selection: the seam between the planner's allowed pool and
//! the workers the manager actually runs.
//!
//! The planner owns the intent, the allowed worker pool, and the capability
//! limits. A [`TeamSelector`] may choose a subset of that pool and reassign
//! subtasks, but the [`ManagerRunner`](crate::ManagerRunner) validates the
//! result before any worker call: the topology stays `manager_worker`, the
//! worker count stays within the planner's `max_workers`, subtasks stay
//! non-empty, worker keys stay unique, and every assigned subtask names a
//! selected worker.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{CapabilityLimits, IntentPlan, Subtask, WorkerSpec};

/// Chooses the manager's actual workers and subtasks inside the planner's
/// limits.
///
/// `select` runs after the planner's plan validates and before any assignment
/// or worker call. The default runner keeps the planner's plan unchanged, so a
/// selector is opt-in through
/// [`ManagerRunner::with_selector`](crate::ManagerRunner::with_selector).
///
/// [`ManagerPrompt`] is the structured input a future LLM-backed selector
/// receives; see its docs for the exact fields.
#[async_trait]
pub trait TeamSelector: Send + Sync {
    async fn select(&self, plan: &IntentPlan) -> anyhow::Result<IntentPlan>;
}

/// Lets an `Arc` of a selector be passed wherever a selector is accepted, so a
/// caller can share one selector across runners without cloning its state.
#[async_trait]
impl<T: TeamSelector + ?Sized> TeamSelector for Arc<T> {
    async fn select(&self, plan: &IntentPlan) -> anyhow::Result<IntentPlan> {
        (**self).select(plan).await
    }
}

/// The deterministic selector.
///
/// It proves the runner honors a manager choice without a model call:
///
/// - When there are fewer subtasks than workers, only the workers that would
///   receive a subtask survive.
/// - When there are more subtasks than `max_workers`, the worker pool is
///   capped at `max_workers` and every subtask gets an explicit worker by
///   round-robin.
/// - The selected worker count never exceeds `max_workers`.
/// - Subtasks keep their original order and identity.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BoundedSelector;

impl BoundedSelector {
    /// A selector that bounds the pool deterministically.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl TeamSelector for BoundedSelector {
    async fn select(&self, plan: &IntentPlan) -> anyhow::Result<IntentPlan> {
        Ok(bounded_selection(plan))
    }
}

/// The exact input a future LLM manager receives.
///
/// This type is the contract for the LLM seam; it is not itself a model call.
/// A selector implementation would render or serialize this value, ask the
/// model to pick workers/subtasks, parse the answer, and return the resulting
/// [`IntentPlan`]. The runner then validates that plan exactly as it validates
/// [`BoundedSelector`]'s output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerPrompt {
    /// The intent the plan executes.
    pub intent: String,
    /// The planner's allowed worker pool. A manager may choose from these
    /// specs; it may not invent a new worker.
    pub allowed_workers: Vec<WorkerSpec>,
    /// The subtasks the planner proposed, in planner order.
    pub subtasks: Vec<Subtask>,
    /// The planner's capability budget. The manager may not widen it.
    pub limits: CapabilityLimits,
    /// Worker slots left under `limits.max_workers` after the allowed worker
    /// pool: `max_workers.saturating_sub(allowed_workers.len())`.
    pub remaining_capacity: usize,
}

impl ManagerPrompt {
    /// Build the LLM-facing input from the planner's plan.
    pub fn new(plan: &IntentPlan) -> Self {
        Self {
            intent: plan.intent.clone(),
            allowed_workers: plan.workers.clone(),
            subtasks: plan.subtasks.clone(),
            limits: plan.limits,
            remaining_capacity: plan.limits.max_workers.saturating_sub(plan.workers.len()),
        }
    }
}

impl From<&IntentPlan> for ManagerPrompt {
    fn from(plan: &IntentPlan) -> Self {
        Self::new(plan)
    }
}

/// The bounded selection algorithm.
fn bounded_selection(plan: &IntentPlan) -> IntentPlan {
    let mut selected = plan.clone();
    let max_workers = selected.limits.max_workers;
    if max_workers == 0 || selected.workers.is_empty() || selected.subtasks.is_empty() {
        // Invalid input is left untouched for the runner's validation to
        // report; the selector itself never invents an execution.
        return selected;
    }

    let workers = selected.workers.len();
    // The worker each subtask would use under the manager's existing fixed
    // assignment: an explicit key stays, an unkeyed subtask is round-robin by
    // its index. This is the "assigned subtask" set for the filtering rule.
    let targets: Vec<Option<String>> = selected
        .subtasks
        .iter()
        .enumerate()
        .map(|(index, subtask)| match subtask.worker_key.as_deref() {
            Some(key) if selected.workers.iter().any(|worker| worker.key == key) => {
                Some(key.to_owned())
            }
            Some(_) => None,
            None => selected
                .workers
                .get(index % workers)
                .map(|worker| worker.key.clone()),
        })
        .collect();

    let mut kept = if selected.subtasks.len() < workers {
        let used: HashSet<&str> = targets.iter().flatten().map(String::as_str).collect();
        selected
            .workers
            .iter()
            .filter(|worker| used.contains(worker.key.as_str()))
            .cloned()
            .collect::<Vec<_>>()
    } else {
        selected.workers.clone()
    };

    // The planner's limit is absolute, even when the pool or the subtask list
    // is larger.
    kept.truncate(max_workers);
    if kept.is_empty() {
        // The input named no usable worker (for example an unknown explicit
        // key). Fall back to the front of the planner's pool so the bounded
        // output still respects the cap; the runner's validation will report
        // anything else that is wrong.
        kept = selected.workers.iter().take(max_workers).cloned().collect();
    }
    if kept.is_empty() {
        return selected;
    }

    let kept_keys: HashSet<&str> = kept.iter().map(|worker| worker.key.as_str()).collect();
    let kept_len = kept.len();
    for (index, subtask) in selected.subtasks.iter_mut().enumerate() {
        let target = targets[index]
            .as_deref()
            .filter(|key| kept_keys.contains(key));
        let worker_key = match target {
            Some(key) => key.to_owned(),
            None => kept[index % kept_len].key.clone(),
        };
        subtask.worker_key = Some(worker_key);
    }
    selected.workers = kept;
    selected
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Topology;

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
            description: format!("do {id}"),
            worker_key: worker_key.map(str::to_owned),
        }
    }

    fn plan(workers: Vec<WorkerSpec>, subtasks: Vec<Subtask>, max_workers: usize) -> IntentPlan {
        IntentPlan {
            intent: "ship it".to_owned(),
            topology: Topology::ManagerWorker,
            workers,
            subtasks,
            limits: CapabilityLimits {
                max_workers,
                ..CapabilityLimits::default()
            },
        }
    }

    #[tokio::test]
    async fn fewer_subtasks_drops_idle_workers() {
        let plan = plan(
            vec![worker("a"), worker("b"), worker("c")],
            vec![subtask("s0", Some("a")), subtask("s1", Some("b"))],
            3,
        );
        let selected = BoundedSelector::new().select(&plan).await.unwrap();
        assert_eq!(
            selected
                .workers
                .iter()
                .map(|worker| worker.key.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(selected.subtasks, plan.subtasks);
    }

    #[tokio::test]
    async fn more_subtasks_than_max_workers_caps_and_round_robins() {
        let plan = plan(
            vec![worker("a"), worker("b"), worker("c"), worker("d")],
            vec![
                subtask("s0", None),
                subtask("s1", None),
                subtask("s2", None),
                subtask("s3", None),
                subtask("s4", None),
            ],
            2,
        );
        let selected = BoundedSelector::new().select(&plan).await.unwrap();
        assert_eq!(selected.workers.len(), 2);
        assert_eq!(
            selected
                .workers
                .iter()
                .map(|worker| worker.key.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(
            selected
                .subtasks
                .iter()
                .map(|subtask| subtask.worker_key.as_deref().unwrap())
                .collect::<Vec<_>>(),
            ["a", "b", "a", "b", "a"]
        );
    }

    #[tokio::test]
    async fn subtask_order_and_ids_are_preserved() {
        let plan = plan(
            vec![worker("a"), worker("b"), worker("c")],
            vec![
                subtask("first", Some("c")),
                subtask("second", None),
                subtask("third", Some("a")),
            ],
            2,
        );
        let selected = BoundedSelector::new().select(&plan).await.unwrap();
        assert_eq!(
            selected
                .subtasks
                .iter()
                .map(|subtask| subtask.id.as_str())
                .collect::<Vec<_>>(),
            ["first", "second", "third"]
        );
        assert_eq!(selected.workers.len(), 2);
        for subtask in &selected.subtasks {
            assert!(
                selected
                    .workers
                    .iter()
                    .any(|worker| Some(worker.key.as_str()) == subtask.worker_key.as_deref())
            );
        }
    }

    #[test]
    fn manager_prompt_names_the_llm_input() {
        let plan = plan(
            vec![worker("a"), worker("b")],
            vec![subtask("s0", Some("a"))],
            4,
        );
        let prompt = ManagerPrompt::from(&plan);
        assert_eq!(prompt.intent, "ship it");
        assert_eq!(prompt.allowed_workers, plan.workers);
        assert_eq!(prompt.subtasks, plan.subtasks);
        assert_eq!(prompt.limits, plan.limits);
        assert_eq!(prompt.remaining_capacity, 2);
    }
}
