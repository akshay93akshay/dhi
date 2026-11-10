//! DHI+ orchestration foundation.
//!
//! One intent becomes one [`IntentPlan`]. The planner outside this crate
//! chooses that plan: topology, workers, subtasks, and capability limits.
//! This crate owns execution:
//!
//! - [`ManagerRunner`] executes a manager/worker plan with up-front
//!   assignments and bounded concurrency.
//! - [`SwarmRunner`] executes a peer-to-peer plan: one seed task per worker,
//!   then direct [`SwarmMessage`] exchange with no central assignment.
//!
//! Both runners call only [`WorkerExecutor`]. Guard checks, auditing, tool
//! routing, and model calls belong in that implementation; the runners never
//! call tools themselves. Both return one [`IntentResult`] with one aggregated
//! [`dhi_agent::FactDraft`] and never write a fact. The caller persists the
//! draft.
//!
//! A manager plan may pass through a [`TeamSelector`] before assignment. The
//! planner declares the allowed worker pool and limits; the selector chooses
//! the actual workers and subtasks inside them. [`ManagerRunner::new`] keeps
//! the planner's plan unchanged, [`ManagerRunner::with_selector`] opts into a
//! choice, and [`BoundedSelector`] is the deterministic testable selector.
//!
//! [`ManagerRunner::with_lead`] instead replaces the one-shot selection with
//! the manager-worker lead loop. The planner's subtasks run first, then a
//! [`TeamLead`] returns one [`LeadDecision`] per turn: `Continue` runs
//! directly assigned tasks, `AskWorker` runs one pool worker, `Final` supplies
//! the intent's one fact, and `Stop` fails the intent visibly. The runner caps
//! lead turns, task rounds, and total worker runs, and rejects any decision
//! that would exceed them.
//!
//! The Swarm topology uses [`TeamRunner`] only for shared state and teammate
//! runs. The coordinator is a normal agent run hosted by the engine's team
//! tool service; there is no model seam or JSON decision loop here.
//!
//! Aggregation goes through the small [`Aggregator`] trait. The default
//! [`DeterministicAggregator`] orders results by worker and subtask key, not
//! completion order; an LLM aggregator can replace it with
//! [`ManagerRunner::with_aggregator`] or [`SwarmRunner::with_aggregator`].
//!
//! Plan validation is typed by [`OrchestrationError`] and covers topology
//! worker counts, single-topology subtasks, unique worker keys, known assigned
//! workers, and non-zero limits. Manager-worker and swarm plans may start with
//! zero seed subtasks; their lead/team creates work after validation. The
//! runner re-validates the selected plan against the planner's limits before
//! any worker runs.

mod aggregate;
mod executor;
mod lead;
mod manager;
mod selector;
mod swarm;
mod team;
mod types;

pub use aggregate::{Aggregation, Aggregator, DeterministicAggregator};
pub use executor::WorkerExecutor;
pub use lead::{LeadStopped, TeamLead};
pub use manager::ManagerRunner;
pub use selector::{BoundedSelector, ManagerPrompt, TeamSelector};
pub use swarm::{SwarmMessage, SwarmRunner, SwarmStats};
pub use team::{TeamActionOutcome, TeamActor, TeamRunConfig, TeamRunner, TeamSession, TeamStopped};
pub use types::{
    CapabilityLimits, IntentPlan, IntentResult, LeadBudget, LeadDecision, LeadError, LeadInput,
    LeadLimits, LeadTask, LeadWorkerResult, LeadWorkerStatus, OrchestrationError, Subtask,
    Topology, WorkerResult, WorkerSpec, WorkerStatus,
};
