//! Manager/worker execution.
//!
//! Two modes share the same worker executor:
//!
//! - The default runner assigns every planned subtask up front and runs them
//!   concurrently under the plan's parallel budget.
//! - [`ManagerRunner::with_lead`] replaces the one-shot manager selector with
//!   a [`TeamLead`] loop: initial tasks run, the lead reviews, and its
//!   `Continue`, `AskWorker`, `Final`, or `Stop` decision drives the next
//!   action. Assignments stay direct: `Continue` names a worker key for every
//!   task. There is no task board or mailbox.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use dhi_agent::FactDraft;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::aggregate::{deterministic_aggregation, sorted_results};
use crate::lead::TeamLead;
use crate::{
    Aggregator, DeterministicAggregator, IntentPlan, IntentResult, LeadBudget, LeadDecision,
    LeadInput, LeadLimits, LeadStopped, LeadTask, LeadWorkerResult, LeadWorkerStatus, Subtask,
    TeamSelector, Topology, WorkerExecutor, WorkerResult, WorkerSpec, WorkerStatus,
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
/// Without a lead, the assignment is fixed before any worker runs: a subtask
/// with a `worker_key` goes to that worker, otherwise it is assigned
/// round-robin by subtask index. The executor is the only place a guard or
/// audit hook belongs.
///
/// A [`TeamSelector`] may choose the actual workers and subtasks first. The
/// default keeps the planner's plan unchanged; use [`Self::with_selector`] to
/// honor a manager choice.
///
/// With a [`TeamLead`] (see [`Self::with_lead`]) the selector is skipped: the
/// lead owns every decision after the planner's initial tasks, and the runner
/// only executes, validates, and bounds the loop.
pub struct ManagerRunner<E> {
    executor: Arc<E>,
    aggregator: Arc<dyn Aggregator>,
    selector: Arc<dyn TeamSelector>,
    lead: Option<Arc<dyn TeamLead>>,
    lead_limits: Option<LeadLimits>,
}

impl<E> ManagerRunner<E> {
    /// A runner with the deterministic aggregator and the planner's plan.
    pub fn new(executor: Arc<E>) -> Self {
        Self {
            executor,
            aggregator: Arc::new(DeterministicAggregator),
            selector: Arc::new(KeepPlannerSelection),
            lead: None,
            lead_limits: None,
        }
    }

    /// A runner that uses a custom aggregator (for example an LLM one).
    pub fn with_aggregator(executor: Arc<E>, aggregator: Arc<dyn Aggregator>) -> Self {
        Self {
            executor,
            aggregator,
            selector: Arc::new(KeepPlannerSelection),
            lead: None,
            lead_limits: None,
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
            lead: None,
            lead_limits: None,
        }
    }

    /// Opts into the lead loop for manager-worker plans.
    ///
    /// The planner's plan still validates first. Its workers are the fixed
    /// pool and its subtasks are the initial tasks. The lead then decides each
    /// next step until it returns `Final` (one fact result) or `Stop` (a
    /// [`LeadStopped`] error). A lead takes precedence over a selector.
    pub fn with_lead<L>(mut self, lead: L) -> Self
    where
        L: TeamLead + 'static,
    {
        self.lead = Some(Arc::new(lead));
        self
    }

    /// Narrows the lead loop budgets. The runner derives defaults from the
    /// planner's plan limits when this is not called.
    pub fn with_lead_limits(mut self, limits: LeadLimits) -> Self {
        self.lead_limits = Some(limits);
        self
    }
}

impl<E: WorkerExecutor + 'static> ManagerRunner<E> {
    /// Validate the plan, run every subtask, and aggregate all results.
    ///
    /// One worker failure becomes a failed [`WorkerResult`]; it never cancels
    /// or discards the other results. No fact is written here.
    pub async fn run(&self, plan: &IntentPlan) -> anyhow::Result<IntentResult> {
        plan.validate()?;

        if let Some(lead) = &self.lead {
            return self.run_lead(lead.as_ref(), plan).await;
        }

        let mut selected = self.selector.select(plan).await?;
        // Limits belong to the planner. A selector may choose workers and
        // subtasks, but it cannot widen the budget the manager executes under.
        selected.limits = plan.limits;
        validate_selection(&selected)?;
        let plan = &selected;

        let assignments = assignments(plan);
        let permits = Arc::new(Semaphore::new(plan.limits.max_parallel));
        // The assigned subtask is the worker's primary instruction through the
        // executor. The broad plan intent names every worker's topic and must
        // not leak into a worker turn.
        let results = self.run_assignments(&permits, assignments, &[]).await;

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
            audit_results: Vec::new(),
        })
    }

    /// The lead loop: initial tasks, then one reviewed decision per turn.
    async fn run_lead(
        &self,
        lead: &dyn TeamLead,
        plan: &IntentPlan,
    ) -> anyhow::Result<IntentResult> {
        if plan.topology != Topology::ManagerWorker {
            anyhow::bail!(
                "the team lead only drives manager_worker plans, got {}",
                plan.topology
            );
        }
        let derived = LeadLimits::from_capabilities(&plan.limits, plan.subtasks.len());
        // A caller may tighten the loop budget, never widen the planner's.
        let limits = self
            .lead_limits
            .map(|custom| LeadLimits {
                max_rounds: custom.max_rounds.min(derived.max_rounds),
                max_lead_turns: custom.max_lead_turns.min(derived.max_lead_turns),
                max_worker_runs: custom.max_worker_runs.min(derived.max_worker_runs),
                max_parallel: custom.max_parallel.min(derived.max_parallel),
                max_workers: custom.max_workers.min(derived.max_workers),
            })
            .unwrap_or(derived);
        for (name, value) in [
            ("max_rounds", limits.max_rounds),
            ("max_lead_turns", limits.max_lead_turns),
            ("max_worker_runs", limits.max_worker_runs),
            ("max_parallel", limits.max_parallel),
            ("max_workers", limits.max_workers),
        ] {
            if value == 0 {
                anyhow::bail!("lead limit {name} must be greater than zero");
            }
        }
        if plan.workers.len() > limits.max_workers {
            anyhow::bail!(
                "the planner's {} workers exceed the lead's max_workers {}",
                plan.workers.len(),
                limits.max_workers
            );
        }

        let assignments = assignments(plan);
        if assignments.len() > limits.max_worker_runs {
            anyhow::bail!(
                "the planner's {} initial tasks exceed the lead's {} worker-run budget",
                assignments.len(),
                limits.max_worker_runs
            );
        }
        let initial_tasks: Vec<LeadTask> = assignments
            .iter()
            .map(|(worker, subtask)| lead_task_from_subtask(worker, subtask))
            .collect();

        let permits = Arc::new(Semaphore::new(limits.max_parallel));
        let mut results = self.run_assignments(&permits, assignments, &[]).await;
        let mut worker_runs = results.len();
        let mut completed: HashSet<String> = HashSet::new();
        let mut failed: HashSet<String> = HashSet::new();
        for result in &results {
            if result.status == WorkerStatus::Ok {
                completed.insert(result.subtask_id.clone());
            } else {
                failed.insert(result.subtask_id.clone());
            }
        }

        let mut tasks = initial_tasks;
        let mut seen_ids: HashSet<String> = tasks.iter().map(|task| task.id.clone()).collect();
        let mut round = 1usize;
        let mut lead_turns = 0usize;

        loop {
            if lead_turns >= limits.max_lead_turns {
                anyhow::bail!(
                    "lead did not return a final decision within {} turns",
                    limits.max_lead_turns
                );
            }
            let input = LeadInput {
                intent: plan.intent.clone(),
                workers: plan.workers.clone(),
                tasks: tasks.clone(),
                results: lead_worker_results(&results),
                round,
                limits,
                remaining: LeadBudget {
                    remaining_rounds: limits.max_rounds.saturating_sub(round),
                    // The current review consumes one turn.
                    remaining_lead_turns: limits.max_lead_turns.saturating_sub(lead_turns + 1),
                    remaining_worker_runs: limits.max_worker_runs.saturating_sub(worker_runs),
                },
            };
            let decision = lead.review(&input).await?;
            lead_turns += 1;
            decision.validate(&input)?;

            match decision {
                LeadDecision::Final {
                    summary,
                    detail,
                    evidence,
                    confidence,
                } => {
                    let evidence = if evidence.trim().is_empty() {
                        deterministic_aggregation(plan, &results).fact.evidence
                    } else {
                        evidence
                    };
                    let fact = FactDraft {
                        summary: summary.clone(),
                        detail,
                        evidence,
                        confidence,
                    };
                    tracing::debug!(
                        intent = %plan.intent,
                        round,
                        lead_turns,
                        workers = worker_runs,
                        "lead loop finalized"
                    );
                    return Ok(IntentResult {
                        summary,
                        fact,
                        worker_results: sorted_results(&results),
                        audit_results: Vec::new(),
                    });
                }
                LeadDecision::Stop { reason } => {
                    return Err(LeadStopped::new(reason, sorted_results(&results)).into());
                }
                LeadDecision::Continue {
                    tasks: next_tasks,
                    reason,
                } => {
                    // Validation proved a round, a review, and the worker runs
                    // remain. Increment before execution so the next input
                    // names the round those tasks belong to.
                    round += 1;
                    tracing::debug!(
                        intent = %plan.intent,
                        round,
                        tasks = next_tasks.len(),
                        reason = %reason,
                        "lead continued with direct assignments"
                    );
                    let (batch, executed) = self
                        .run_lead_batch(
                            &permits,
                            &plan.workers,
                            &next_tasks,
                            &mut completed,
                            &mut failed,
                        )
                        .await?;
                    worker_runs += executed;
                    results.extend(batch);
                    for task in &next_tasks {
                        seen_ids.insert(task.id.clone());
                    }
                    tasks.extend(next_tasks);
                }
                LeadDecision::AskWorker { worker_key, prompt } => {
                    // Validation proved the worker and a worker run remain.
                    let worker = plan
                        .workers
                        .iter()
                        .find(|worker| worker.key == worker_key)
                        .expect("validated ask worker")
                        .clone();
                    let id = unique_task_id(&seen_ids, worker_runs + 1);
                    seen_ids.insert(id.clone());
                    let subtask = Subtask {
                        id: id.clone(),
                        description: prompt,
                        worker_key: Some(worker_key),
                    };
                    worker_runs += 1;
                    let result = self
                        .run_assignments(&permits, vec![(worker, subtask)], &[])
                        .await
                        .into_iter()
                        .next()
                        .expect("one assignment yields one result");
                    if result.status == WorkerStatus::Ok {
                        completed.insert(id);
                    } else {
                        failed.insert(id);
                    }
                    results.push(result);
                }
            }
        }
    }

    /// Run one `Continue` batch, honoring `depends_on` in waves.
    ///
    /// Every ready task runs in parallel under the semaphore. A task whose
    /// dependency failed is recorded as skipped instead of being run. A
    /// dependency cycle is rejected by decision validation before this point;
    /// the defensive check here turns any remaining deadlock into a visible
    /// error rather than a hang.
    async fn run_lead_batch(
        &self,
        permits: &Arc<Semaphore>,
        workers: &[WorkerSpec],
        tasks: &[LeadTask],
        completed: &mut HashSet<String>,
        failed: &mut HashSet<String>,
    ) -> anyhow::Result<(Vec<WorkerResult>, usize)> {
        let mut pending = tasks.to_vec();
        let mut results = Vec::with_capacity(tasks.len());
        let mut executed = 0usize;

        while !pending.is_empty() {
            let mut ready = Vec::new();
            let mut blocked = Vec::new();
            let mut waiting = Vec::new();
            for task in pending.drain(..) {
                if let Some(dependency) = task
                    .depends_on
                    .iter()
                    .find(|dependency| failed.contains(*dependency))
                {
                    blocked.push((task, dependency.clone()));
                } else if task
                    .depends_on
                    .iter()
                    .all(|dependency| completed.contains(dependency))
                {
                    ready.push(task);
                } else {
                    waiting.push(task);
                }
            }

            for (task, dependency) in blocked {
                failed.insert(task.id.clone());
                results.push(WorkerResult::skipped(
                    task.assignee,
                    task.id,
                    format!("dependency {dependency:?} did not complete"),
                ));
            }
            if ready.is_empty() {
                if waiting.is_empty() {
                    break;
                }
                let ids = waiting
                    .iter()
                    .map(|task| task.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                anyhow::bail!("lead task dependencies cannot make progress: {ids}");
            }

            let assignments: Vec<(WorkerSpec, Subtask)> = ready
                .iter()
                .map(|task| {
                    let worker = workers
                        .iter()
                        .find(|worker| worker.key == task.assignee)
                        .expect("validated task assignee")
                        .clone();
                    (worker, subtask_from_lead_task(task))
                })
                .collect();
            executed += assignments.len();
            let batch = self.run_assignments(permits, assignments, &[]).await;
            for result in &batch {
                if result.status == WorkerStatus::Ok {
                    completed.insert(result.subtask_id.clone());
                } else {
                    failed.insert(result.subtask_id.clone());
                }
            }
            results.extend(batch);
            pending = waiting;
        }

        Ok((results, executed))
    }

    /// Spawn every assignment at once; the semaphore bounds concurrency.
    async fn run_assignments(
        &self,
        permits: &Arc<Semaphore>,
        assignments: Vec<(WorkerSpec, Subtask)>,
        context: &[String],
    ) -> Vec<WorkerResult> {
        let mut tasks = JoinSet::new();
        let mut identities = HashMap::with_capacity(assignments.len());

        for (worker, subtask) in assignments {
            let executor = Arc::clone(&self.executor);
            let permits = Arc::clone(permits);
            let context = context.to_vec();
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

        let mut results = Vec::with_capacity(identities.len());
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
        results
    }
}

/// The lead-facing snapshot of one worker result.
fn lead_worker_results(results: &[WorkerResult]) -> Vec<LeadWorkerResult> {
    results
        .iter()
        .map(|result| LeadWorkerResult {
            worker_key: result.worker_key.clone(),
            task_id: result.subtask_id.clone(),
            status: match result.status {
                WorkerStatus::Ok => LeadWorkerStatus::Ok,
                WorkerStatus::Failed => LeadWorkerStatus::Failed,
                WorkerStatus::Skipped => LeadWorkerStatus::Skipped,
            },
            summary: result.summary.clone(),
            evidence: result.evidence.clone(),
            error: result.error.clone(),
        })
        .collect()
}

fn lead_task_from_subtask(worker: &WorkerSpec, subtask: &Subtask) -> LeadTask {
    LeadTask {
        id: subtask.id.clone(),
        title: first_line(&subtask.description, &subtask.id),
        description: subtask.description.clone(),
        assignee: worker.key.clone(),
        depends_on: Vec::new(),
    }
}

fn subtask_from_lead_task(task: &LeadTask) -> Subtask {
    let description = match (
        task.title.trim().is_empty(),
        task.description.trim().is_empty(),
    ) {
        (true, true) => task.id.clone(),
        (true, false) => task.description.clone(),
        (false, true) => task.title.clone(),
        (false, false) if task.title == task.description => task.title.clone(),
        (false, false) => format!("{}\n\n{}", task.title, task.description),
    };
    Subtask {
        id: task.id.clone(),
        description,
        worker_key: Some(task.assignee.clone()),
    }
}

fn first_line(text: &str, fallback: &str) -> String {
    let line = text.lines().next().map(str::trim).unwrap_or("");
    if line.is_empty() {
        fallback.to_owned()
    } else {
        line.to_owned()
    }
}

/// A generated `AskWorker` task id that cannot collide with any seen task.
fn unique_task_id(seen: &HashSet<String>, start: usize) -> String {
    let mut index = start;
    loop {
        let id = format!("lead-ask-{index}");
        if !seen.contains(&id) {
            return id;
        }
        index += 1;
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
    // The plan validator allows a manager-worker plan to start empty because
    // the lead can create tasks dynamically. A one-shot selector has no such
    // creator, so an empty selection would silently do no work.
    if plan.subtasks.is_empty() {
        anyhow::bail!("manager selection must keep at least one subtask");
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
