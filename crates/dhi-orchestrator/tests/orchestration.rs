use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use dhi_orchestrator::{
    Aggregator, CapabilityLimits, DeterministicAggregator, IntentPlan, ManagerRunner,
    OrchestrationError, Subtask, SwarmMessage, SwarmRunner, Topology, WorkerExecutor, WorkerResult,
    WorkerSpec, WorkerStatus,
};
use tokio::sync::{Barrier, watch};

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;
type Handler = Arc<
    dyn Fn(WorkerSpec, Subtask, Vec<String>) -> BoxFuture<anyhow::Result<WorkerResult>>
        + Send
        + Sync,
>;

#[derive(Debug, Clone)]
struct Call {
    worker_key: String,
    subtask_id: String,
    context: Vec<String>,
}

struct FakeExecutor {
    handler: Handler,
    calls: Arc<Mutex<Vec<Call>>>,
}

impl FakeExecutor {
    fn new(handler: Handler) -> Self {
        Self {
            handler,
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl WorkerExecutor for FakeExecutor {
    async fn run(
        &self,
        worker: &WorkerSpec,
        subtask: &Subtask,
        context: &[String],
    ) -> anyhow::Result<WorkerResult> {
        self.calls.lock().unwrap().push(Call {
            worker_key: worker.key.clone(),
            subtask_id: subtask.id.clone(),
            context: context.to_vec(),
        });
        let handler = Arc::clone(&self.handler);
        handler(worker.clone(), subtask.clone(), context.to_vec()).await
    }
}

fn worker(key: &str, max_turns: usize) -> WorkerSpec {
    WorkerSpec {
        key: key.to_owned(),
        role: format!("{key}-role"),
        prompt: format!("prompt for {key}"),
        max_turns,
    }
}

fn subtask(id: &str, worker_key: Option<&str>) -> Subtask {
    Subtask {
        id: id.to_owned(),
        description: format!("do {id}"),
        worker_key: worker_key.map(str::to_owned),
    }
}

fn limits(
    max_workers: usize,
    max_parallel: usize,
    max_rounds: usize,
    max_messages: usize,
) -> CapabilityLimits {
    CapabilityLimits {
        max_workers,
        max_parallel,
        max_rounds,
        max_messages,
    }
}

fn plan(
    topology: Topology,
    workers: Vec<WorkerSpec>,
    subtasks: Vec<Subtask>,
    limits: CapabilityLimits,
) -> IntentPlan {
    IntentPlan {
        intent: "ship the intent".to_owned(),
        topology,
        workers,
        subtasks,
        limits,
    }
}

fn ok_from(worker: &WorkerSpec, subtask: &Subtask) -> WorkerResult {
    WorkerResult::ok(
        worker.key.clone(),
        subtask.id.clone(),
        format!("summary from {}", worker.key),
        format!("evidence from {}", worker.key),
    )
}

fn always_ok_handler() -> Handler {
    Arc::new(|worker, subtask, _context| Box::pin(async move { Ok(ok_from(&worker, &subtask)) }))
}

#[tokio::test]
async fn manager_runs_three_workers_with_bounded_parallelism() {
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let arrivals = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(2));

    let handler: Handler = {
        let active = Arc::clone(&active);
        let max_active = Arc::clone(&max_active);
        let arrivals = Arc::clone(&arrivals);
        let barrier = Arc::clone(&barrier);
        Arc::new(move |worker, subtask, _context| {
            let active = Arc::clone(&active);
            let max_active = Arc::clone(&max_active);
            let arrivals = Arc::clone(&arrivals);
            let barrier = Arc::clone(&barrier);
            Box::pin(async move {
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                max_active.fetch_max(now, Ordering::SeqCst);
                // The first two calls overlap; the semaphore must hold the
                // third out until one of them finishes.
                if arrivals.fetch_add(1, Ordering::SeqCst) < 2 {
                    barrier.wait().await;
                }
                tokio::task::yield_now().await;
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(ok_from(&worker, &subtask))
            })
        })
    };

    let executor = Arc::new(FakeExecutor::new(handler));
    let plan = plan(
        Topology::ManagerWorker,
        vec![worker("a", 2), worker("b", 2), worker("c", 2)],
        vec![
            subtask("s0", Some("a")),
            subtask("s1", Some("b")),
            subtask("s2", Some("c")),
        ],
        limits(3, 2, 4, 16),
    );

    let result = ManagerRunner::new(Arc::clone(&executor))
        .run(&plan)
        .await
        .unwrap();

    assert_eq!(result.worker_results.len(), 3);
    assert!(
        result
            .worker_results
            .iter()
            .all(|result| result.status == WorkerStatus::Ok)
    );
    assert_eq!(max_active.load(Ordering::SeqCst), 2);
    assert!(result.fact.summary.contains("ship the intent"));
    assert_eq!(result.fact.confidence, "inferred");
    assert_eq!(executor.calls().len(), 3);
}

#[tokio::test]
async fn manager_keeps_other_results_when_one_worker_fails() {
    let handler: Handler = Arc::new(|worker, subtask, _context| {
        Box::pin(async move {
            if worker.key == "b" {
                anyhow::bail!("b exploded")
            }
            Ok(ok_from(&worker, &subtask))
        })
    });
    let executor = Arc::new(FakeExecutor::new(handler));
    let plan = plan(
        Topology::ManagerWorker,
        vec![worker("a", 2), worker("b", 2), worker("c", 2)],
        vec![
            subtask("s0", Some("a")),
            subtask("s1", Some("b")),
            subtask("s2", Some("c")),
        ],
        limits(3, 3, 4, 16),
    );

    let result = ManagerRunner::new(executor).run(&plan).await.unwrap();

    assert_eq!(result.worker_results.len(), 3);
    let failed = result
        .worker_results
        .iter()
        .find(|result| result.worker_key == "b")
        .unwrap();
    assert_eq!(failed.status, WorkerStatus::Failed);
    assert_eq!(failed.error.as_deref(), Some("b exploded"));
    assert!(result.fact.evidence.contains("b exploded"));
    assert!(result.fact.evidence.contains("[s1] failed"));
    assert!(result.fact.summary.contains("- a [s0]"));
    assert!(result.fact.summary.contains("- c [s2]"));
    assert!(!result.fact.summary.contains("b exploded"));
}

#[tokio::test]
async fn manager_rejects_too_many_workers() {
    let executor = Arc::new(FakeExecutor::new(always_ok_handler()));
    let plan = plan(
        Topology::ManagerWorker,
        vec![worker("a", 2), worker("b", 2), worker("c", 2)],
        vec![subtask("s0", None)],
        limits(2, 2, 3, 8),
    );

    let error = ManagerRunner::new(executor).run(&plan).await.unwrap_err();
    let typed = error.downcast_ref::<OrchestrationError>().unwrap();
    assert_eq!(
        *typed,
        OrchestrationError::WorkerCountOutOfRange {
            topology: Topology::ManagerWorker,
            max_workers: 2,
            actual: 3,
        }
    );
}

#[tokio::test]
async fn manager_assigns_unkeyed_subtasks_round_robin() {
    let executor = Arc::new(FakeExecutor::new(always_ok_handler()));
    let plan = plan(
        Topology::ManagerWorker,
        vec![worker("a", 2), worker("b", 2), worker("c", 2)],
        vec![
            subtask("s0", None),
            subtask("s1", None),
            subtask("s2", None),
            subtask("s3", None),
            subtask("s4", None),
        ],
        limits(3, 3, 4, 32),
    );

    let result = ManagerRunner::new(Arc::clone(&executor))
        .run(&plan)
        .await
        .unwrap();

    let mut assigned = result
        .worker_results
        .iter()
        .map(|result| (result.subtask_id.clone(), result.worker_key.clone()))
        .collect::<Vec<_>>();
    assigned.sort();
    assert_eq!(
        assigned,
        vec![
            ("s0".to_owned(), "a".to_owned()),
            ("s1".to_owned(), "b".to_owned()),
            ("s2".to_owned(), "c".to_owned()),
            ("s3".to_owned(), "a".to_owned()),
            ("s4".to_owned(), "b".to_owned()),
        ]
    );
    let mut called = executor
        .calls()
        .into_iter()
        .map(|call| (call.subtask_id, call.worker_key))
        .collect::<Vec<_>>();
    called.sort();
    assert_eq!(called, assigned);
}

#[tokio::test]
async fn swarm_findings_flow_and_stop_after_two_quiet_rounds() {
    let handler: Handler = Arc::new(|worker, subtask, _context| {
        Box::pin(async move {
            if subtask.id.contains(":r") {
                Ok(WorkerResult::skipped(
                    worker.key.clone(),
                    subtask.id.clone(),
                    "reaction skipped",
                ))
            } else {
                Ok(ok_from(&worker, &subtask))
            }
        })
    });
    let executor = Arc::new(FakeExecutor::new(handler));
    let plan = plan(
        Topology::Swarm,
        vec![worker("a", 10), worker("b", 10), worker("c", 10)],
        vec![
            subtask("seed-0", None),
            subtask("seed-1", None),
            subtask("seed-2", None),
        ],
        limits(3, 2, 10, 128),
    );

    let runner = SwarmRunner::new(Arc::clone(&executor));
    let result = runner.run(&plan).await.unwrap();
    let stats = runner.stats().expect("stats recorded");

    assert_eq!(stats.findings, 3, "only the three seed findings are new");
    assert_eq!(stats.rounds, 3, "rounds 2 and 3 add no findings");
    assert_eq!(
        result
            .worker_results
            .iter()
            .filter(|result| result.status == WorkerStatus::Ok)
            .count(),
        3
    );
    assert!(executor.calls().iter().any(|call| {
        call.context
            .iter()
            .any(|line| line.starts_with("finding from"))
    }));
    assert!(result.fact.summary.contains("- a [seed-0]"));
    assert!(result.fact.summary.contains("- b [seed-1]"));
    assert!(result.fact.summary.contains("- c [seed-2]"));
}

#[tokio::test]
async fn swarm_stops_at_max_rounds() {
    let executor = Arc::new(FakeExecutor::new(always_ok_handler()));
    let plan = plan(
        Topology::Swarm,
        vec![worker("a", 10), worker("b", 10), worker("c", 10)],
        vec![
            subtask("seed-0", None),
            subtask("seed-1", None),
            subtask("seed-2", None),
        ],
        limits(3, 3, 3, 256),
    );

    let runner = SwarmRunner::new(executor);
    let result = runner.run(&plan).await.unwrap();
    let stats = runner.stats().expect("stats recorded");

    assert_eq!(stats.rounds, 3);
    assert!(stats.findings >= 6, "seed plus round-two findings flow");
    assert!(result.fact.summary.contains("ship the intent"));
}

#[tokio::test]
async fn swarm_keeps_other_findings_when_one_worker_fails() {
    let handler: Handler = Arc::new(|worker, subtask, _context| {
        Box::pin(async move {
            if subtask.id.contains(":r") {
                return Ok(WorkerResult::skipped(
                    worker.key.clone(),
                    subtask.id.clone(),
                    "reaction skipped",
                ));
            }
            if worker.key == "b" {
                anyhow::bail!("swarm b exploded")
            }
            Ok(ok_from(&worker, &subtask))
        })
    });
    let executor = Arc::new(FakeExecutor::new(handler));
    let plan = plan(
        Topology::Swarm,
        vec![worker("a", 10), worker("b", 10), worker("c", 10)],
        vec![
            subtask("seed-0", None),
            subtask("seed-1", None),
            subtask("seed-2", None),
        ],
        limits(3, 2, 6, 128),
    );

    let runner = SwarmRunner::new(executor);
    let result = runner.run(&plan).await.unwrap();
    let stats = runner.stats().expect("stats recorded");

    let failed = result
        .worker_results
        .iter()
        .find(|result| result.worker_key == "b" && result.subtask_id == "seed-1")
        .unwrap();
    assert_eq!(failed.status, WorkerStatus::Failed);
    assert_eq!(failed.error.as_deref(), Some("swarm b exploded"));
    assert!(result.fact.evidence.contains("swarm b exploded"));
    assert!(result.fact.summary.contains("- a [seed-0]"));
    assert!(result.fact.summary.contains("- c [seed-2]"));
    assert_eq!(stats.findings, 2);
}

#[tokio::test]
async fn swarm_respects_max_messages() {
    let executor = Arc::new(FakeExecutor::new(always_ok_handler()));
    let plan = plan(
        Topology::Swarm,
        vec![worker("a", 10), worker("b", 10), worker("c", 10)],
        vec![
            subtask("seed-0", None),
            subtask("seed-1", None),
            subtask("seed-2", None),
        ],
        limits(3, 3, 10, 2),
    );

    let runner = SwarmRunner::new(executor);
    let result = runner.run(&plan).await.unwrap();
    let stats = runner.stats().expect("stats recorded");

    assert_eq!(stats.messages, 2);
    assert_eq!(stats.rounds, 1, "message budget stops after the seed round");
    assert_eq!(result.worker_results.len(), 3);
    assert_eq!(
        result
            .worker_results
            .iter()
            .filter(|r| r.is_success())
            .count(),
        3
    );
}

#[tokio::test]
async fn aggregation_orders_by_worker_and_subtask_not_completion() {
    let (release_tx, release_rx) = watch::channel(false);
    let completed = Arc::new(Mutex::new(Vec::new()));

    let handler: Handler = {
        let release_tx = release_tx;
        let release_rx = release_rx;
        let completed = Arc::clone(&completed);
        Arc::new(move |worker, subtask, _context| {
            let release_tx = release_tx.clone();
            let release_rx = release_rx.clone();
            let completed = Arc::clone(&completed);
            Box::pin(async move {
                match worker.key.as_str() {
                    "c" => {
                        release_tx.send(true).unwrap();
                    }
                    "a" => {
                        let mut release_rx = release_rx;
                        if !*release_rx.borrow() {
                            release_rx.changed().await.unwrap();
                        }
                    }
                    _ => {}
                }
                completed.lock().unwrap().push(worker.key.clone());
                Ok(ok_from(&worker, &subtask))
            })
        })
    };
    let executor = Arc::new(FakeExecutor::new(handler));
    let plan = plan(
        Topology::ManagerWorker,
        vec![worker("a", 2), worker("b", 2), worker("c", 2)],
        vec![
            subtask("s0", Some("a")),
            subtask("s1", Some("b")),
            subtask("s2", Some("c")),
        ],
        limits(3, 3, 4, 16),
    );

    let result = ManagerRunner::new(executor).run(&plan).await.unwrap();

    let completion = completed.lock().unwrap().clone();
    let c_at = completion.iter().position(|key| key == "c").unwrap();
    let a_at = completion.iter().position(|key| key == "a").unwrap();
    assert!(c_at < a_at, "worker a must finish after worker c");
    assert_eq!(completion.last().map(String::as_str), Some("a"));

    let summary = &result.fact.summary;
    let a_line = summary.find("- a [s0]").unwrap();
    let b_line = summary.find("- b [s1]").unwrap();
    let c_line = summary.find("- c [s2]").unwrap();
    assert!(a_line < b_line && b_line < c_line);

    let reversed = result
        .worker_results
        .iter()
        .rev()
        .cloned()
        .collect::<Vec<_>>();
    let forward = DeterministicAggregator
        .aggregate(&plan, &result.worker_results)
        .await
        .unwrap();
    let backward = DeterministicAggregator
        .aggregate(&plan, &reversed)
        .await
        .unwrap();
    assert_eq!(forward, backward);
}

#[test]
fn plan_validation_is_typed() {
    let single = plan(
        Topology::Single,
        vec![worker("a", 1), worker("b", 1)],
        vec![subtask("s0", None)],
        limits(2, 2, 2, 8),
    );
    assert_eq!(
        single.validate(),
        Err(OrchestrationError::SingleRequiresOneWorker {
            topology: Topology::Single,
            actual: 2,
        })
    );

    let no_workers = plan(
        Topology::ManagerWorker,
        vec![],
        vec![subtask("s0", None)],
        limits(2, 2, 2, 8),
    );
    assert_eq!(
        no_workers.validate(),
        Err(OrchestrationError::WorkerCountOutOfRange {
            topology: Topology::ManagerWorker,
            max_workers: 2,
            actual: 0,
        })
    );

    let duplicate = plan(
        Topology::ManagerWorker,
        vec![worker("a", 1), worker("a", 1)],
        vec![subtask("s0", None)],
        limits(3, 2, 2, 8),
    );
    assert_eq!(
        duplicate.validate(),
        Err(OrchestrationError::DuplicateWorkerKey {
            key: "a".to_owned(),
        })
    );

    let no_subtasks = plan(
        Topology::ManagerWorker,
        vec![worker("a", 1)],
        vec![],
        limits(2, 2, 2, 8),
    );
    assert_eq!(no_subtasks.validate(), Err(OrchestrationError::NoSubtasks));

    let unknown = plan(
        Topology::ManagerWorker,
        vec![worker("a", 1)],
        vec![subtask("s0", Some("ghost"))],
        limits(2, 2, 2, 8),
    );
    assert_eq!(
        unknown.validate(),
        Err(OrchestrationError::UnknownWorker {
            subtask_id: "s0".to_owned(),
            worker_key: "ghost".to_owned(),
        })
    );

    let zero_workers = plan(
        Topology::ManagerWorker,
        vec![worker("a", 1)],
        vec![subtask("s0", None)],
        limits(0, 2, 2, 8),
    );
    assert_eq!(
        zero_workers.validate(),
        Err(OrchestrationError::ZeroMaxWorkers)
    );

    let zero_parallel = plan(
        Topology::ManagerWorker,
        vec![worker("a", 1)],
        vec![subtask("s0", None)],
        limits(2, 0, 2, 8),
    );
    assert_eq!(
        zero_parallel.validate(),
        Err(OrchestrationError::ZeroMaxParallel)
    );

    let zero_rounds = plan(
        Topology::ManagerWorker,
        vec![worker("a", 1)],
        vec![subtask("s0", None)],
        limits(2, 2, 0, 8),
    );
    assert_eq!(
        zero_rounds.validate(),
        Err(OrchestrationError::ZeroMaxRounds)
    );
}

#[test]
fn plans_and_swarm_messages_round_trip() {
    let plan = plan(
        Topology::Swarm,
        vec![worker("a", 3)],
        vec![subtask("s0", None)],
        limits(2, 1, 2, 8),
    );
    let encoded = serde_json::to_string(&plan).unwrap();
    let decoded: IntentPlan = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, plan);

    let messages = [
        SwarmMessage::Finding {
            from: "a".to_owned(),
            subtask_id: "s0".to_owned(),
            summary: "summary".to_owned(),
            evidence: "evidence".to_owned(),
        },
        SwarmMessage::Question {
            from: "a".to_owned(),
            text: "why?".to_owned(),
        },
        SwarmMessage::Challenge {
            from: "a".to_owned(),
            text: "no".to_owned(),
        },
        SwarmMessage::Done {
            from: "a".to_owned(),
        },
        SwarmMessage::Shutdown,
    ];
    for message in messages {
        let encoded = serde_json::to_string(&message).unwrap();
        let decoded: SwarmMessage = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, message);
    }
}
