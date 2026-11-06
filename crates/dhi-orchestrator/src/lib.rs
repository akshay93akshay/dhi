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
//! Aggregation goes through the small [`Aggregator`] trait. The default
//! [`DeterministicAggregator`] orders results by worker and subtask key, not
//! completion order; an LLM aggregator can replace it with
//! [`ManagerRunner::with_aggregator`] or [`SwarmRunner::with_aggregator`].
//!
//! Plan validation is typed by [`OrchestrationError`] and covers topology
//! worker counts, non-empty subtasks, unique worker keys, known assigned
//! workers, and non-zero limits.

mod aggregate;
mod executor;
mod manager;
mod swarm;
mod types;

pub use aggregate::{Aggregation, Aggregator, DeterministicAggregator};
pub use executor::WorkerExecutor;
pub use manager::ManagerRunner;
pub use swarm::{SwarmMessage, SwarmRunner, SwarmStats};
pub use types::{
    CapabilityLimits, IntentPlan, IntentResult, OrchestrationError, Subtask, Topology,
    WorkerResult, WorkerSpec, WorkerStatus,
};
