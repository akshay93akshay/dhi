//! Deterministic aggregation of worker results into one intent fact.

use async_trait::async_trait;
use dhi_agent::FactDraft;
use serde::{Deserialize, Serialize};

use crate::{IntentPlan, WorkerResult, WorkerStatus};

/// The summary and fact draft produced from all worker results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Aggregation {
    pub summary: String,
    pub fact: FactDraft,
}

/// Combines worker results into the one result of an intent.
///
/// The trait is async so an LLM-backed aggregator can replace the
/// deterministic one later without changing either runner.
#[async_trait]
pub trait Aggregator: Send + Sync {
    async fn aggregate(
        &self,
        plan: &IntentPlan,
        results: &[WorkerResult],
    ) -> anyhow::Result<Aggregation>;
}

/// The default aggregator: no model call, stable ordering, no hidden state.
#[derive(Debug, Default, Clone, Copy)]
pub struct DeterministicAggregator;

impl DeterministicAggregator {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Aggregator for DeterministicAggregator {
    async fn aggregate(
        &self,
        plan: &IntentPlan,
        results: &[WorkerResult],
    ) -> anyhow::Result<Aggregation> {
        Ok(deterministic_aggregation(plan, results))
    }
}

/// Aggregate results in worker/subtask key order.
///
/// Successful summaries and evidence contribute to the fact. Failed and
/// skipped results are appended to the evidence so nothing is silently lost.
/// Completion order is never consulted.
pub fn deterministic_aggregation(plan: &IntentPlan, results: &[WorkerResult]) -> Aggregation {
    let sorted = sorted_results(results);

    let mut successful = Vec::new();
    let mut failed = Vec::new();
    let mut skipped = Vec::new();
    for result in &sorted {
        match result.status {
            WorkerStatus::Ok => successful.push(result),
            WorkerStatus::Failed => failed.push(result),
            WorkerStatus::Skipped => skipped.push(result),
        }
    }

    let mut summary = format!(
        "{}: {} of {} worker results succeeded",
        plan.intent,
        successful.len(),
        sorted.len()
    );
    for result in &successful {
        summary.push_str(&format!(
            "\n- {} [{}]: {}",
            result.worker_key, result.subtask_id, result.summary
        ));
    }
    if successful.is_empty() {
        summary.push_str("\n- no worker produced a finding");
    }

    let mut evidence = String::new();
    for result in &successful {
        evidence.push_str(&format!(
            "- {} [{}]: {}\n",
            result.worker_key, result.subtask_id, result.evidence
        ));
    }
    for result in &failed {
        evidence.push_str(&format!(
            "- {} [{}] failed: {}\n",
            result.worker_key,
            result.subtask_id,
            result.error.as_deref().unwrap_or("unknown error")
        ));
    }
    for result in &skipped {
        evidence.push_str(&format!(
            "- {} [{}] skipped: {}\n",
            result.worker_key, result.subtask_id, result.summary
        ));
    }
    if evidence.is_empty() {
        evidence.push_str("no worker evidence");
    }
    let evidence = evidence.trim_end().to_owned();

    let workers = plan
        .workers
        .iter()
        .map(|worker| worker.key.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let detail = format!(
        "Intent: {}\nTopology: {}\nWorkers: {}\nWorker results: {} ({} succeeded, {} failed, {} skipped)",
        plan.intent,
        plan.topology,
        workers,
        sorted.len(),
        successful.len(),
        failed.len(),
        skipped.len()
    );

    Aggregation {
        summary: summary.clone(),
        fact: FactDraft {
            summary,
            detail,
            evidence,
            // Aggregation reasons across worker outputs; it is not a direct
            // observation from one tool call.
            confidence: "inferred".to_owned(),
        },
    }
}

/// Stable result order: worker key, then subtask id, then remaining fields.
///
/// The remaining fields make the order total even if a worker somehow reports
/// the same subtask twice.
pub(crate) fn sorted_results(results: &[WorkerResult]) -> Vec<WorkerResult> {
    let mut sorted = results.to_vec();
    sorted.sort_by(|left, right| {
        left.worker_key
            .cmp(&right.worker_key)
            .then_with(|| left.subtask_id.cmp(&right.subtask_id))
            .then_with(|| left.status.rank().cmp(&right.status.rank()))
            .then_with(|| left.summary.cmp(&right.summary))
            .then_with(|| left.evidence.cmp(&right.evidence))
            .then_with(|| left.error.cmp(&right.error))
    });
    sorted
}
