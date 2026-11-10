# dhi-orchestrator

The orchestration foundation for DHI+ intents. This crate turns one
`IntentPlan` into one `IntentResult` and one aggregated `FactDraft`.

## Layers

- **Planner**: outside this crate. It chooses the topology, workers, subtasks,
  and capability limits that make up an `IntentPlan`.
- **Runner**: this crate. `ManagerRunner` executes a manager/worker plan;
  `ManagerRunner::with_lead` adds the direct-assignment manager loop.
  `TeamRunner` loads and seeds the shared Cline Teams state used by the Swarm
  coordinator agent. `SwarmRunner` remains the legacy peer-to-peer fallback
  for hosts without team persistence. Runners validate the plan first and
  return every seed result, even when one worker fails.
- **WorkerExecutor**: the execution seam. A runner calls only this trait.
  Guard checks, auditing, tool routing, and model calls belong in the
  implementation, not in the runners.
- **TeamSelector**: the manager's worker-choice seam. It runs after the
  planner's plan validates and before any assignment. `ManagerRunner::new`
  keeps the planner's plan unchanged; `with_selector` opts into a choice.
  `BoundedSelector` is the deterministic selector; the engine's
  `RuntimeTeamSelector` is the live LLM manager. `ManagerPrompt` names the
  exact input the manager receives.
- **Aggregator**: combines worker results into the single `IntentResult`
  summary and `FactDraft`. `DeterministicAggregator` orders results by worker
  and subtask key so output does not depend on completion order. An LLM
  aggregator can replace it through `with_aggregator`.

## Invariants

- One intent produces one `IntentResult` and one fact draft. Runners never
  write facts; the caller persists the returned draft.
- A swarm's aggregate reads only each worker's seed turn. Peer-reaction turns
  are returned in `audit_results` for the engine's capacity, usage, failure,
  and post-hoc tool accounting; they never enter the fact.
- A failed worker does not discard other workers' results. Seed failures
  appear in the aggregate evidence; reaction failures appear in the team-run
  audit payload.
- `max_workers`, `max_parallel`, `max_rounds`, and `max_messages` are enforced
  by the runtime. A selector cannot widen the planner's limits.
- A selected manager plan keeps the `manager_worker` topology, a non-empty
  worker pool within `max_workers`, unique worker keys, and subtasks whose
  worker keys all exist. A manager plan may start with zero subtasks when the
  lead is used; the lead assigns follow-up work. Invalid output fails before
  any worker runs.
- A Swarm team starts from the plan's worker roster. The coordinator agent
  owns the intent and calls the team tools; teammates may create, claim,
  complete, and block cards, message peers, log mission entries, and create or
  review outcome fragments. Spawn, shutdown, cleanup, and outcome finalization
  stay coordinator-only. `SwarmRunner` is the legacy peer fallback and is not
  the Swarm coordinator contract.

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
