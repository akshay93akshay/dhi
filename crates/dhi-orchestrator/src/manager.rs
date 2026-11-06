//! Manager/worker execution: assign every subtask up front, then run them
//! concurrently under the plan's parallel budget.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::aggregate::sorted_results;
use crate::{
    Aggregator, DeterministicAggregator, IntentPlan, IntentResult, Subtask, WorkerExecutor,
    WorkerResult, WorkerSpec,
};

/// Executes a manager/worker [`IntentPlan`].
///
/// The assignment is fixed before any worker runs: a subtask with a
/// `worker_key` goes to that worker, otherwise it is assigned round-robin by
/// subtask index. The executor is the only place a guard or audit hook belongs.
pub struct ManagerRunner<E> {
    executor: Arc<E>,
    aggregator: Arc<dyn Aggregator>,
}

impl<E> ManagerRunner<E> {
    /// A runner with the deterministic aggregator.
    pub fn new(executor: Arc<E>) -> Self {
        Self {
            executor,
            aggregator: Arc::new(DeterministicAggregator),
        }
    }

    /// A runner that uses a custom aggregator (for example an LLM one).
    pub fn with_aggregator(executor: Arc<E>, aggregator: Arc<dyn Aggregator>) -> Self {
        Self {
            executor,
            aggregator,
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
