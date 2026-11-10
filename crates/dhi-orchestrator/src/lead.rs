//! The manager-worker lead seam.
//!
//! A [`TeamLead`] replaces the one-shot [`TeamSelector`](crate::TeamSelector)
//! for a manager-worker plan. The runner executes the planner's initial
//! tasks, then asks the lead for one decision, applies it, and repeats until
//! the lead returns `Final` or `Stop`. The lead never runs a worker turn and
//! never appears in the worker results.

use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

use crate::{LeadDecision, LeadInput, WorkerResult};

/// Decides the next action of a manager-worker lead loop.
///
/// `review` runs after the planner's plan validates and after each execution
/// batch. The runner owns every safety check: it re-validates the decision
/// against the loop budgets, rejects unknown workers or task ids, executes
/// only pool workers, and bounds concurrency with `max_parallel`.
#[async_trait]
pub trait TeamLead: Send + Sync {
    async fn review(&self, input: &LeadInput) -> anyhow::Result<LeadDecision>;
}

/// Lets an `Arc` of a lead be shared across runners without cloning its state.
#[async_trait]
impl<T: TeamLead + ?Sized> TeamLead for Arc<T> {
    async fn review(&self, input: &LeadInput) -> anyhow::Result<LeadDecision> {
        (**self).review(input).await
    }
}

/// The typed error a `Stop` decision produces.
///
/// A stop is not a successful intent result: no fact is written. The worker
/// results collected before the stop travel with the error so the caller can
/// inspect the evidence and record the failure visibly.
#[derive(Debug, Error)]
#[error("lead stopped the intent: {reason}")]
pub struct LeadStopped {
    pub reason: String,
    pub worker_results: Vec<WorkerResult>,
}

impl LeadStopped {
    pub fn new(reason: impl Into<String>, worker_results: Vec<WorkerResult>) -> Self {
        Self {
            reason: reason.into(),
            worker_results,
        }
    }
}
