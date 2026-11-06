# dhi-orchestrator

The orchestration foundation for DHI+ intents. This crate turns one
`IntentPlan` into one `IntentResult` and one aggregated `FactDraft`.

## Layers

- **Planner**: outside this crate. It chooses the topology, workers, subtasks,
  and capability limits that make up an `IntentPlan`.
- **Runner**: this crate. `ManagerRunner` executes a manager/worker plan;
  `SwarmRunner` executes a peer-to-peer swarm plan. Both validate the plan
  first and return every worker result, even when one worker fails.
- **WorkerExecutor**: the execution seam. A runner calls only this trait.
  Guard checks, auditing, tool routing, and model calls belong in the
  implementation, not in the runners.
- **Aggregator**: combines worker results into the single `IntentResult`
  summary and `FactDraft`. `DeterministicAggregator` orders results by worker
  and subtask key so output does not depend on completion order. An LLM
  aggregator can replace it through `with_aggregator`.

## Invariants

- One intent produces one `IntentResult` and one fact draft. Runners never
  write facts; the caller persists the returned draft.
- A failed worker does not discard other workers' results. Failures appear in
  the aggregate evidence.
- `max_workers`, `max_parallel`, `max_rounds`, and `max_messages` are enforced
  by the runtime.
- The manager assigns subtasks up front. The swarm seeds one task per worker
  and then peers exchange typed `SwarmMessage`s without a central manager
  choosing every step.

## Example

```rust
use std::sync::Arc;

use dhi_orchestrator::{IntentPlan, ManagerRunner, WorkerExecutor};

async fn run<E>(executor: Arc<E>, plan: &IntentPlan) -> anyhow::Result<()>
where
    E: WorkerExecutor + 'static,
{
    let runner = ManagerRunner::new(executor);
    let result = runner.run(plan).await?;
    let _fact = result.fact;
    Ok(())
}
```
