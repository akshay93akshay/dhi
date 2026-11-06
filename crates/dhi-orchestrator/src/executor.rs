//! The worker execution seam.

use async_trait::async_trait;

use crate::{Subtask, WorkerResult, WorkerSpec};

/// Executes exactly one worker turn.
///
/// This is the only seam the runners call. An implementation owns model calls,
/// tool routing, guard checks, and auditing; the runners themselves never run
/// tools and never inspect guard policy. Implementations should be safe to
/// call concurrently.
#[async_trait]
pub trait WorkerExecutor: Send + Sync {
    async fn run(
        &self,
        worker: &WorkerSpec,
        subtask: &Subtask,
        context: &[String],
    ) -> anyhow::Result<WorkerResult>;
}
