//! Manager/worker execution: assign every subtask up front, then run them
//! concurrently under the plan's parallel budget.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::aggregate::sorted_results;
use crate::{
    Aggregator, DeterministicAggregator, IntentPlan, IntentResult, Subtask, TeamSelector, Topology,
    WorkerExecutor, WorkerResult, WorkerSpec,
};

/// Keeps the planner's plan unchanged: the default runner behavior.
#[derive(Debug, Default, Clone, Copy)]
struct KeepPlannerSelection;

#[async_trait]
impl TeamSelector for KeepPlannerSelection {
    async fn select(&self, plan: &IntentPlan) -> anyhow::Result<IntentPlan> {
        Ok(plan.clone())
    }
}

/// Executes a manager/worker [`IntentPlan`].
///
/// The assignment is fixed before any worker runs: a subtask with a
/// `worker_key` goes to that worker, otherwise it is assigned round-robin by
/// subtask index. The executor is the only place a guard or audit hook belongs.
///
/// A [`TeamSelector`] may choose the actual workers and subtasks first. The
/// default keeps the planner's plan unchanged; use [`Self::with_selector`] to
/// honor a manager choice.
pub struct ManagerRunner<E> {
    executor: Arc<E>,
    aggregator: Arc<dyn Aggregator>,
    selector: Arc<dyn TeamSelector>,
}

impl<E> ManagerRunner<E> {
    /// A runner with the deterministic aggregator and the planner's plan.
    pub fn new(executor: Arc<E>) -> Self {
        Self {
            executor,
            aggregator: Arc::new(DeterministicAggregator),
            selector: Arc::new(KeepPlannerSelection),
        }
    }

    /// A runner that uses a custom aggregator (for example an LLM one).
    pub fn with_aggregator(executor: Arc<E>, aggregator: Arc<dyn Aggregator>) -> Self {
        Self {
            executor,
            aggregator,
            selector: Arc::new(KeepPlannerSelection),
        }
    }

    /// A runner that lets `selector` choose the actual workers and subtasks.
    ///
    /// The selector runs after the planner's plan validates and before any
    /// assignment or worker call. Its output is re-validated against the
    /// planner's limits; an invalid selection fails the run without calling
    /// the executor.
    pub fn with_selector<S>(executor: Arc<E>, selector: S) -> Self
    where
        S: TeamSelector + 'static,
    {
        Self {
            executor,
            aggregator: Arc::new(DeterministicAggregator),
            selector: Arc::new(selector),
        }
    }
}

impl<E: WorkerExecutor + 'static> ManagerRunner<E> {
    /// Validate the plan, run every subtask, and aggregate all results.
    ///
    /// One worker failure becomes a failed [`WorkerResult`]; it never cancels
    /// or discards the other results. No fact is written here.
    pub async fn run(&self, plan: &IntentPlan) -> anyhow::Result<IntentResult> {
        plan.validate()?;

        let mut selected = self.selector.select(plan).await?;
        // Limits belong to the planner. A selector may choose workers and
        // subtasks, but it cannot widen the budget the manager executes under.
        selected.limits = plan.limits;
        validate_selection(&selected)?;
        let plan = &selected;

        let assignments = assignments(plan);
        let context = vec![format!("intent: {}", plan.intent)];
        let permits = Arc::new(Semaphore::new(plan.limits.max_parallel));
        let mut tasks = JoinSet::new();
        let mut identities = HashMap::with_capacity(assignments.len());

        for (worker, subtask) in assignments {
            let executor = Arc::clone(&self.executor);
            let permits = Arc::clone(&permits);
            let context = context.clone();
            let task_worker = worker.clone();
            let task_subtask = subtask.clone();

            let handle = tasks.spawn(async move {
                let _permit = permits
                    .acquire()
                    .await
                    .expect("manager semaphore is never closed");
                match executor.run(&task_worker, &task_subtask, &context).await {
                    Ok(mut result) => {
                        // The runner owns identity; the executor only owns the
                        // outcome. This keeps aggregation stable even if an
                        // implementation returns a mismatched key.
                        result.worker_key = task_worker.key.clone();
                        result.subtask_id = task_subtask.id.clone();
                        result
                    }
                    Err(error) => WorkerResult::failed(
                        task_worker.key.clone(),
                        task_subtask.id.clone(),
                        error.to_string(),
                    ),
                }
            });
            identities.insert(handle.id(), (worker.key, subtask.id));
        }

        let mut results = Vec::with_capacity(plan.subtasks.len());
        while let Some(joined) = tasks.join_next_with_id().await {
            match joined {
                Ok((id, result)) => {
                    identities.remove(&id);
                    results.push(result);
                }
                Err(error) => {
                    let (worker_key, subtask_id) = identities
                        .remove(&error.id())
                        .unwrap_or_else(|| ("<unknown>".to_owned(), "<unknown>".to_owned()));
                    let reason = if error.is_panic() {
                        format!("worker task panicked: {error}")
                    } else {
                        format!("worker task cancelled: {error}")
                    };
                    results.push(WorkerResult::failed(worker_key, subtask_id, reason));
                }
            }
        }

        let results = sorted_results(&results);
        let aggregation = self.aggregator.aggregate(plan, &results).await?;
        tracing::debug!(
            intent = %plan.intent,
            workers = plan.workers.len(),
            results = results.len(),
            "manager plan completed"
        );

        Ok(IntentResult {
            summary: aggregation.summary,
            fact: aggregation.fact,
            worker_results: results,
        })
    }
}

/// Reject a manager selection the runner cannot execute safely.
///
/// The planner's limits were restored before this check, so
/// [`IntentPlan::validate`] enforces the planner's `max_workers`, not a limit
/// the selector supplied. The topology check is explicit because the generic
/// plan validator would also accept `single` and `swarm` plans.
fn validate_selection(plan: &IntentPlan) -> anyhow::Result<()> {
    if plan.topology != Topology::ManagerWorker {
        anyhow::bail!(
            "manager selector must keep the manager_worker topology, got {}",
            plan.topology
        );
    }
    plan.validate()?;
    Ok(())
}

/// Fixed assignment: explicit keys first, otherwise round-robin by subtask
/// index across all declared workers.
fn assignments(plan: &IntentPlan) -> Vec<(WorkerSpec, Subtask)> {
    plan.subtasks
        .iter()
        .enumerate()
        .map(|(index, subtask)| {
            let worker = match subtask.worker_key.as_deref() {
                Some(key) => plan
                    .workers
                    .iter()
                    .find(|worker| worker.key == key)
                    .expect("validated subtask worker key"),
                None => &plan.workers[index % plan.workers.len()],
            };
            (worker.clone(), subtask.clone())
        })
        .collect()
}
