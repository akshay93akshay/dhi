//! Runtime bridge from a DHI+ worker turn to one agent execution.
//!
//! The orchestrator runners own assignment, message exchange, and
//! aggregation. This adapter only turns one `(worker, subtask)` turn into an
//! `ExecuteInput`, calls the runtime once, and reports the fact the worker
//! produced. It never records a fact and never touches the store.

use std::sync::Arc;

use async_trait::async_trait;
use dhi_agent::{AgentRuntime, ExecuteInput};
use dhi_orchestrator::{Subtask, WorkerExecutor, WorkerResult, WorkerSpec};
use uuid::Uuid;

/// Executes one DHI+ worker through the task's `AgentRuntime`.
pub(crate) struct RuntimeWorkerExecutor {
    pub agent: Arc<dyn AgentRuntime>,
    pub task_id: Uuid,
    pub intent_id: Uuid,
    /// The fact context at claim time. The runner adds its own per-turn
    /// context (intent line, peer messages) on top.
    pub context: Vec<String>,
    pub worker_profile_id: Option<i64>,
}

#[async_trait]
impl WorkerExecutor for RuntimeWorkerExecutor {
    async fn run(
        &self,
        worker: &WorkerSpec,
        subtask: &Subtask,
        runner_context: &[String],
    ) -> anyhow::Result<WorkerResult> {
        let mut context = self.context.clone();
        context.extend(runner_context.iter().cloned());
        // The role and prompt shape the worker's turn just as the manager's
        // assignment does; without them every worker would see the same bare
        // intent text.
        context.push(format!("worker role: {} ({})", worker.key, worker.role));
        if !worker.prompt.trim().is_empty() {
            context.push(format!("worker instructions: {}", worker.prompt.trim()));
        }

        let executed = self
            .agent
            .execute(ExecuteInput {
                task_id: self.task_id,
                intent_id: self.intent_id,
                intent: subtask.description.clone(),
                context,
                worker_profile_id: self.worker_profile_id,
            })
            .await;

        match executed {
            Ok(call) => {
                let dhi_agent::AgentCall {
                    output,
                    meta,
                    prompt,
                    response,
                    thinking,
                } = call;
                let fact = output.fact;
                let evidence = fact_evidence(&fact);
                let mut result = WorkerResult::ok(
                    worker.key.clone(),
                    subtask.id.clone(),
                    fact.summary,
                    evidence,
                );
                // Audit is best-effort data: an empty field never fails the
                // worker, and the engine skips results with no call records.
                result.call_meta = meta;
                result.prompt = prompt;
                result.response = response;
                result.thinking = thinking;
                Ok(result)
            }
            // One failing worker never aborts the team: the runners turn this
            // into a failed result and the remaining workers keep running.
            Err(error) => Ok(WorkerResult::failed(
                worker.key.clone(),
                subtask.id.clone(),
                error.to_string(),
            )),
        }
    }
}

/// Combines the fact fields the aggregate retains into one evidence string.
fn fact_evidence(fact: &dhi_agent::FactDraft) -> String {
    let mut evidence = Vec::new();
    if !fact.detail.trim().is_empty() {
        evidence.push(fact.detail.trim().to_owned());
    }
    if !fact.evidence.trim().is_empty() {
        evidence.push(fact.evidence.trim().to_owned());
    }
    if !fact.confidence.trim().is_empty() {
        evidence.push(format!("confidence: {}", fact.confidence.trim()));
    }
    evidence.join("\n")
}
