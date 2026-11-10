//! Lead-loop tests: direct assignment, parallel batches, continue, ask
//! worker, final, stop, dependencies, and every loop limit.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use dhi_orchestrator::{
    CapabilityLimits, IntentPlan, LeadBudget, LeadDecision, LeadInput, LeadLimits, LeadStopped,
    LeadTask, ManagerRunner, Subtask, TeamLead, Topology, WorkerExecutor, WorkerResult, WorkerSpec,
    WorkerStatus,
};
use tokio::sync::Barrier;

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
    description: String,
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
            description: subtask.description.clone(),
        });
        let handler = Arc::clone(&self.handler);
        handler(worker.clone(), subtask.clone(), context.to_vec()).await
    }
}

struct ScriptedLead {
    decisions: Mutex<VecDeque<LeadDecision>>,
    inputs: Mutex<Vec<LeadInput>>,
}

impl ScriptedLead {
    fn new(decisions: Vec<LeadDecision>) -> Self {
        Self {
            decisions: Mutex::new(decisions.into()),
            inputs: Mutex::new(Vec::new()),
        }
    }

    fn inputs(&self) -> Vec<LeadInput> {
        self.inputs.lock().unwrap().clone()
    }
}

#[async_trait]
impl TeamLead for ScriptedLead {
    async fn review(&self, input: &LeadInput) -> anyhow::Result<LeadDecision> {
        self.inputs.lock().unwrap().push(input.clone());
        self.decisions
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("scripted lead has no decision left"))
    }
}

fn worker(key: &str) -> WorkerSpec {
    WorkerSpec {
        key: key.to_owned(),
        role: format!("{key}-role"),
        prompt: format!("prompt for {key}"),
        max_turns: 2,
    }
}

fn subtask(id: &str, worker_key: Option<&str>) -> Subtask {
    Subtask {
        id: id.to_owned(),
        description: format!("do {id}"),
        worker_key: worker_key.map(str::to_owned),
    }
}

fn lead_task(id: &str, assignee: &str) -> LeadTask {
    LeadTask {
        id: id.to_owned(),
        title: format!("title {id}"),
        description: format!("do {id}"),
        assignee: assignee.to_owned(),
        depends_on: Vec::new(),
    }
}

fn plan(workers: Vec<WorkerSpec>, subtasks: Vec<Subtask>, limits: CapabilityLimits) -> IntentPlan {
    IntentPlan {
        intent: "ship the intent".to_owned(),
        topology: Topology::ManagerWorker,
        workers,
        subtasks,
        limits,
    }
}

fn limits(max_workers: usize, max_parallel: usize, max_rounds: usize) -> CapabilityLimits {
    CapabilityLimits {
        max_workers,
        max_parallel,
        max_rounds,
        max_messages: 16,
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

fn always_ok() -> Handler {
    Arc::new(|worker, subtask, _context| Box::pin(async move { Ok(ok_from(&worker, &subtask)) }))
}

fn final_decision(summary: &str) -> LeadDecision {
    LeadDecision::Final {
        summary: summary.to_owned(),
        detail: "detail from the lead".to_owned(),
        evidence: "evidence from the lead".to_owned(),
        confidence: "inferred".to_owned(),
    }
}

#[tokio::test]
async fn initial_tasks_run_before_the_first_review_and_final_returns_one_fact() {
    let executor = Arc::new(FakeExecutor::new(always_ok()));
    let plan = plan(
        vec![worker("alpha"), worker("beta")],
        vec![subtask("s0", Some("alpha")), subtask("s1", Some("beta"))],
        limits(2, 2, 4),
    );
    let lead = Arc::new(ScriptedLead::new(vec![final_decision("the work is done")]));

    let result = ManagerRunner::new(Arc::clone(&executor))
        .with_lead(Arc::clone(&lead))
        .run(&plan)
        .await
        .unwrap();

    assert_eq!(executor.calls().len(), 2, "initial tasks run first");
    let inputs = lead.inputs();
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0].round, 1);
    assert_eq!(inputs[0].results.len(), 2);
    assert_eq!(inputs[0].tasks.len(), 2);
    assert_eq!(inputs[0].intent, "ship the intent");
    assert_eq!(inputs[0].remaining.remaining_rounds, 3);

    assert_eq!(result.worker_results.len(), 2);
    assert_eq!(result.summary, "the work is done");
    assert_eq!(result.fact.summary, "the work is done");
    assert_eq!(result.fact.detail, "detail from the lead");
    assert_eq!(result.fact.evidence, "evidence from the lead");
    assert_eq!(result.fact.confidence, "inferred");
    for result in &result.worker_results {
        assert!(result.worker_key == "alpha" || result.worker_key == "beta");
    }
    assert!(
        result
            .worker_results
            .iter()
            .all(|result| result.worker_key != "lead" && result.worker_key != "manager"),
        "the lead is not a worker"
    );
}

#[tokio::test]
async fn empty_initial_plan_starts_with_the_lead_and_accepts_continue() {
    let executor = Arc::new(FakeExecutor::new(always_ok()));
    let plan = plan(vec![worker("alpha")], vec![], limits(1, 1, 2));
    let lead = Arc::new(ScriptedLead::new(vec![
        LeadDecision::Continue {
            tasks: vec![lead_task("t1", "alpha")],
            reason: "the lead creates the first task".to_owned(),
        },
        final_decision("done from an empty seed"),
    ]));

    let result = ManagerRunner::new(Arc::clone(&executor))
        .with_lead(Arc::clone(&lead))
        .run(&plan)
        .await
        .unwrap();

    let inputs = lead.inputs();
    assert_eq!(inputs.len(), 2);
    assert!(inputs[0].tasks.is_empty());
    assert!(inputs[0].results.is_empty());
    assert_eq!(inputs[0].round, 1);
    assert_eq!(inputs[1].round, 2);
    assert_eq!(inputs[1].tasks.len(), 1);
    assert_eq!(inputs[1].results.len(), 1);
    assert_eq!(executor.calls().len(), 1);
    assert_eq!(result.worker_results.len(), 1);
}

#[tokio::test]
async fn continue_assigns_directly_and_runs_the_batch_in_parallel() {
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(2));
    let handler: Handler = {
        let active = Arc::clone(&active);
        let max_active = Arc::clone(&max_active);
        let barrier = Arc::clone(&barrier);
        Arc::new(move |worker, subtask, _context| {
            let active = Arc::clone(&active);
            let max_active = Arc::clone(&max_active);
            let barrier = Arc::clone(&barrier);
            Box::pin(async move {
                if subtask.id == "t1" || subtask.id == "t2" {
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active.fetch_max(now, Ordering::SeqCst);
                    barrier.wait().await;
                    active.fetch_sub(1, Ordering::SeqCst);
                }
                Ok(ok_from(&worker, &subtask))
            })
        })
    };
    let executor = Arc::new(FakeExecutor::new(handler));
    let plan = plan(
        vec![worker("alpha"), worker("beta"), worker("gamma")],
        vec![subtask("s0", Some("alpha"))],
        limits(3, 3, 4),
    );
    let lead = Arc::new(ScriptedLead::new(vec![
        LeadDecision::Continue {
            tasks: vec![lead_task("t1", "beta"), {
                let mut task = lead_task("t2", "gamma");
                task.depends_on = vec!["s0".to_owned()];
                task
            }],
            reason: "two more checks".to_owned(),
        },
        final_decision("the continued work is done"),
    ]));

    let result = ManagerRunner::new(Arc::clone(&executor))
        .with_lead(Arc::clone(&lead))
        .run(&plan)
        .await
        .unwrap();

    let calls = executor.calls();
    assert_eq!(calls.len(), 3);
    assert!(
        calls
            .iter()
            .any(|call| call.worker_key == "beta" && call.subtask_id == "t1")
    );
    assert!(
        calls
            .iter()
            .any(|call| call.worker_key == "gamma" && call.subtask_id == "t2")
    );
    assert_eq!(
        max_active.load(Ordering::SeqCst),
        2,
        "the continue batch ran in parallel"
    );

    let inputs = lead.inputs();
    assert_eq!(inputs.len(), 2);
    assert_eq!(inputs[1].round, 2);
    assert_eq!(inputs[1].tasks.len(), 3);
    assert_eq!(inputs[1].tasks[0].id, "s0");
    assert_eq!(inputs[1].tasks[1].assignee, "beta");
    assert_eq!(inputs[1].results.len(), 3);
    assert_eq!(result.worker_results.len(), 3);
}

#[tokio::test]
async fn ask_worker_runs_one_pool_worker_with_the_lead_prompt() {
    let executor = Arc::new(FakeExecutor::new(always_ok()));
    let plan = plan(
        vec![worker("alpha"), worker("beta")],
        vec![subtask("s0", Some("alpha"))],
        limits(2, 2, 4),
    );
    let lead = Arc::new(ScriptedLead::new(vec![
        LeadDecision::AskWorker {
            worker_key: "beta".to_owned(),
            prompt: "check the edge case".to_owned(),
        },
        LeadDecision::Final {
            summary: "the edge case is covered".to_owned(),
            detail: String::new(),
            evidence: String::new(),
            confidence: "observed".to_owned(),
        },
    ]));

    let result = ManagerRunner::new(Arc::clone(&executor))
        .with_lead(Arc::clone(&lead))
        .run(&plan)
        .await
        .unwrap();

    let calls = executor.calls();
    assert_eq!(calls.len(), 2);
    let ask = calls
        .iter()
        .find(|call| call.worker_key == "beta")
        .expect("the ask worker ran");
    assert_eq!(ask.description, "check the edge case");
    let inputs = lead.inputs();
    assert_eq!(inputs[1].results.len(), 2);
    assert_eq!(inputs[1].results[1].worker_key, "beta");
    assert_eq!(result.worker_results.len(), 2);
    assert_eq!(result.fact.confidence, "observed");
    // The lead supplied no evidence, so the runner falls back to the worker
    // evidence rather than dropping it.
    assert!(result.fact.evidence.contains("evidence from beta"));
}

#[tokio::test]
async fn stop_is_a_visible_typed_error_with_the_worker_results() {
    let executor = Arc::new(FakeExecutor::new(always_ok()));
    let plan = plan(
        vec![worker("alpha")],
        vec![subtask("s0", Some("alpha"))],
        limits(1, 1, 4),
    );
    let lead = Arc::new(ScriptedLead::new(vec![LeadDecision::Stop {
        reason: "the source is unavailable".to_owned(),
    }]));

    let error = ManagerRunner::new(executor)
        .with_lead(lead)
        .run(&plan)
        .await
        .unwrap_err();

    assert!(
        error.to_string().contains("the source is unavailable"),
        "{error}"
    );
    let stopped = error
        .downcast::<LeadStopped>()
        .expect("a typed stopped error");
    assert_eq!(stopped.reason, "the source is unavailable");
    assert_eq!(stopped.worker_results.len(), 1);
}

#[tokio::test]
async fn dependency_on_a_failed_task_is_skipped_not_run() {
    let handler: Handler = Arc::new(|worker, subtask, _context| {
        Box::pin(async move {
            if subtask.id == "seed" {
                anyhow::bail!("seed exploded")
            }
            Ok(ok_from(&worker, &subtask))
        })
    });
    let executor = Arc::new(FakeExecutor::new(handler));
    let plan = plan(
        vec![worker("alpha"), worker("beta"), worker("gamma")],
        vec![subtask("seed", Some("alpha"))],
        limits(3, 3, 4),
    );
    let lead = Arc::new(ScriptedLead::new(vec![
        LeadDecision::Continue {
            tasks: vec![
                {
                    let mut task = lead_task("t2", "beta");
                    task.depends_on = vec!["seed".to_owned()];
                    task
                },
                lead_task("t3", "gamma"),
            ],
            reason: "follow up".to_owned(),
        },
        final_decision("done with the skip noted"),
    ]));

    let result = ManagerRunner::new(Arc::clone(&executor))
        .with_lead(lead)
        .run(&plan)
        .await
        .unwrap();

    let calls = executor.calls();
    assert_eq!(
        calls.len(),
        2,
        "the blocked task never reaches the executor"
    );
    assert!(!calls.iter().any(|call| call.subtask_id == "t2"));
    let blocked = result
        .worker_results
        .iter()
        .find(|result| result.subtask_id == "t2")
        .expect("the blocked task has a result");
    assert_eq!(blocked.status, WorkerStatus::Skipped);
    assert!(blocked.summary.contains("did not complete"));
}

#[tokio::test]
async fn continue_above_the_worker_run_budget_is_rejected_before_any_extra_worker() {
    let executor = Arc::new(FakeExecutor::new(always_ok()));
    let plan = plan(
        vec![worker("alpha"), worker("beta")],
        vec![subtask("s0", Some("alpha"))],
        limits(2, 2, 4),
    );
    let lead = Arc::new(ScriptedLead::new(vec![LeadDecision::Continue {
        tasks: vec![lead_task("t1", "alpha"), lead_task("t2", "beta")],
        reason: "too much".to_owned(),
    }]));

    let error = ManagerRunner::new(Arc::clone(&executor))
        .with_lead(lead)
        .with_lead_limits(LeadLimits {
            max_rounds: 4,
            max_lead_turns: 8,
            max_worker_runs: 2,
            max_parallel: 2,
            max_workers: 2,
        })
        .run(&plan)
        .await
        .unwrap_err();

    assert!(
        error.to_string().contains("only 1 worker runs remain"),
        "{error}"
    );
    assert_eq!(executor.calls().len(), 1, "only the initial task ran");
}

#[tokio::test]
async fn continue_above_the_round_budget_is_rejected() {
    let executor = Arc::new(FakeExecutor::new(always_ok()));
    let plan = plan(
        vec![worker("alpha")],
        vec![subtask("s0", Some("alpha"))],
        limits(1, 1, 4),
    );
    let lead = Arc::new(ScriptedLead::new(vec![LeadDecision::Continue {
        tasks: vec![lead_task("t1", "alpha")],
        reason: "one more".to_owned(),
    }]));

    let error = ManagerRunner::new(executor)
        .with_lead(lead)
        .with_lead_limits(LeadLimits {
            max_rounds: 1,
            max_lead_turns: 4,
            max_worker_runs: 8,
            max_parallel: 1,
            max_workers: 1,
        })
        .run(&plan)
        .await
        .unwrap_err();

    assert!(
        error.to_string().contains("no task rounds remain"),
        "{error}"
    );
}

#[tokio::test]
async fn lead_turn_limit_rejects_an_endless_ask_worker_loop() {
    let executor = Arc::new(FakeExecutor::new(always_ok()));
    let plan = plan(
        vec![worker("alpha")],
        vec![subtask("s0", Some("alpha"))],
        limits(1, 1, 4),
    );
    let lead = Arc::new(ScriptedLead::new(vec![
        LeadDecision::AskWorker {
            worker_key: "alpha".to_owned(),
            prompt: "first probe".to_owned(),
        },
        LeadDecision::AskWorker {
            worker_key: "alpha".to_owned(),
            prompt: "second probe".to_owned(),
        },
    ]));

    let error = ManagerRunner::new(Arc::clone(&executor))
        .with_lead(lead)
        .with_lead_limits(LeadLimits {
            max_rounds: 4,
            max_lead_turns: 2,
            max_worker_runs: 8,
            max_parallel: 1,
            max_workers: 1,
        })
        .run(&plan)
        .await
        .unwrap_err();

    assert!(
        error.to_string().contains("no lead turns remain"),
        "{error}"
    );
    assert_eq!(
        executor.calls().len(),
        2,
        "initial task plus the one permitted ask"
    );
}

#[tokio::test]
async fn initial_tasks_above_the_worker_run_budget_are_rejected() {
    let executor = Arc::new(FakeExecutor::new(always_ok()));
    let plan = plan(
        vec![worker("alpha"), worker("beta")],
        vec![subtask("s0", Some("alpha")), subtask("s1", Some("beta"))],
        limits(2, 2, 4),
    );
    let lead = Arc::new(ScriptedLead::new(vec![final_decision("never reached")]));

    let error = ManagerRunner::new(Arc::clone(&executor))
        .with_lead(lead)
        .with_lead_limits(LeadLimits {
            max_rounds: 4,
            max_lead_turns: 8,
            max_worker_runs: 1,
            max_parallel: 2,
            max_workers: 2,
        })
        .run(&plan)
        .await
        .unwrap_err();

    assert!(error.to_string().contains("exceed the lead's 1"), "{error}");
    assert!(executor.calls().is_empty(), "no worker ran");
}

#[test]
fn lead_budget_defaults_are_serializable() {
    let budget = LeadBudget {
        remaining_rounds: 1,
        remaining_lead_turns: 2,
        remaining_worker_runs: 3,
    };
    let value = serde_json::to_value(budget).unwrap();
    assert_eq!(value["remaining_worker_runs"], 3);
}
