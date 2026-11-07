//! Peer-to-peer swarm execution.
//!
//! The runner seeds one subtask per worker and enforces the global round,
//! message, worker, and parallelism budgets. After seeding, workers exchange
//! typed messages directly through per-worker inboxes; no manager assigns
//! follow-up work.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use tokio::sync::{Semaphore, mpsc, watch};
use tokio::task::{Id, JoinSet};

use crate::aggregate::{Aggregator, DeterministicAggregator, sorted_results};
use crate::{
    CapabilityLimits, IntentPlan, IntentResult, Subtask, WorkerExecutor, WorkerResult, WorkerSpec,
    WorkerStatus,
};

/// Largest retained context entries, including the intent line.
const MAX_CONTEXT_ENTRIES: usize = 32;

/// A message peers exchange inside a swarm.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SwarmMessage {
    /// A completed piece of work.
    Finding {
        from: String,
        subtask_id: String,
        summary: String,
        evidence: String,
    },
    /// A peer needs information or a decision from the swarm.
    Question { from: String, text: String },
    /// A peer disputes or adds a constraint to another finding.
    Challenge { from: String, text: String },
    /// A peer has no more turns.
    Done { from: String },
    /// Runtime control message; workers stop without broadcasting it.
    Shutdown,
}

/// Observable stats from the most recent swarm run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmStats {
    pub rounds: usize,
    pub messages: usize,
    pub findings: usize,
    pub completed_workers: usize,
}

/// Executes a [`Topology::Swarm`](crate::Topology::Swarm) plan.
///
/// Each worker starts with one subtask (subtasks cycle when there are fewer
/// than workers). Workers then react to peer messages, broadcast their own
/// typed findings, and record results on the shared board. The coordinator
/// only applies stop rules and shuts the swarm down; it never picks worker
/// assignments after seeding.
pub struct SwarmRunner<E> {
    executor: Arc<E>,
    aggregator: Arc<dyn Aggregator>,
    last_stats: Mutex<Option<SwarmStats>>,
    /// Every logical broadcast of the most recent run, in broadcast order.
    messages: Arc<Mutex<Vec<SwarmMessage>>>,
}

impl<E> SwarmRunner<E> {
    /// A runner with the deterministic synthesizer.
    pub fn new(executor: Arc<E>) -> Self {
        Self {
            executor,
            aggregator: Arc::new(DeterministicAggregator),
            last_stats: Mutex::new(None),
            messages: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// A runner that uses a custom synthesizer (for example an LLM one).
    pub fn with_aggregator(executor: Arc<E>, aggregator: Arc<dyn Aggregator>) -> Self {
        Self {
            executor,
            aggregator,
            last_stats: Mutex::new(None),
            messages: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Stats from the most recent completed run, if any.
    pub fn stats(&self) -> Option<SwarmStats> {
        *self
            .last_stats
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Typed messages broadcast during the most recent run, in broadcast
    /// order. The runtime control [`SwarmMessage::Shutdown`] is never recorded
    /// because workers stop without broadcasting it.
    pub fn messages(&self) -> Vec<SwarmMessage> {
        self.messages
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl<E: WorkerExecutor + 'static> SwarmRunner<E> {
    /// Validate the plan, seed the swarm, and synthesize one intent result.
    ///
    /// One worker failure becomes a failed [`WorkerResult`] plus a peer
    /// challenge; the remaining workers keep contributing. No fact is written
    /// here.
    pub async fn run(&self, plan: &IntentPlan) -> anyhow::Result<IntentResult> {
        plan.validate()?;

        self.messages
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let messages = Arc::clone(&self.messages);

        let worker_count = plan.workers.len();
        let mut senders = Vec::with_capacity(worker_count);
        let mut inboxes = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let (sender, receiver) = mpsc::unbounded_channel();
            senders.push(sender);
            inboxes.push(Some(receiver));
        }

        let peers: Vec<Vec<mpsc::UnboundedSender<SwarmMessage>>> = (0..worker_count)
            .map(|worker_index| {
                senders
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| *index != worker_index)
                    .map(|(_, sender)| sender.clone())
                    .collect()
            })
            .collect();

        let board = Arc::new(Mutex::new(BoardState::default()));
        let permits = Arc::new(Semaphore::new(plan.limits.max_parallel));
        let (round_tx, round_rx) = watch::channel(0u64);
        let (stop_tx, stop_rx) = watch::channel(false);
        let (report_tx, mut report_rx) = mpsc::unbounded_channel::<WorkerReport>();
        let mut tasks = JoinSet::new();
        let mut identities: HashMap<Id, (String, String)> = HashMap::with_capacity(worker_count);

        for (index, worker) in plan.workers.iter().cloned().enumerate() {
            let seed = plan.subtasks[index % plan.subtasks.len()].clone();
            let inbox = inboxes[index].take().expect("swarm inbox exists");
            let worker_peers = peers[index].clone();
            let worker_reports = report_tx.clone();
            let executor = Arc::clone(&self.executor);
            let worker_permits = Arc::clone(&permits);
            let worker_board = Arc::clone(&board);
            let worker_messages = Arc::clone(&messages);
            let worker_rounds = round_rx.clone();
            let worker_stop = stop_rx.clone();
            let intent = plan.intent.clone();
            let limits = plan.limits;
            let identity = (worker.key.clone(), seed.id.clone());

            let handle = tasks.spawn(run_worker(
                WorkerContext {
                    executor,
                    permits: worker_permits,
                    board: worker_board,
                    messages: worker_messages,
                    rounds: worker_rounds,
                    stop: worker_stop,
                    reports: worker_reports,
                    intent,
                    limits,
                },
                worker,
                seed,
                inbox,
                worker_peers,
            ));
            identities.insert(handle.id(), identity);
        }
        drop(report_tx);

        let mut dead: BTreeSet<String> = BTreeSet::new();
        let mut round = 1u64;
        let mut reason = None;

        loop {
            wait_for_round(
                &mut report_rx,
                &mut tasks,
                &mut identities,
                &board,
                worker_count,
                round,
                &mut dead,
            )
            .await;

            if dead.len() >= worker_count {
                reason = Some(StopReason::AllDone);
                break;
            }

            let stopped = {
                let state = board_lock(&board);
                stop_reason(&state, round, &plan.limits, worker_count, dead.len())
            };
            if let Some(stopped) = stopped {
                reason = Some(stopped);
                break;
            }

            round += 1;
            if round_tx.send(round).is_err() {
                break;
            }
        }

        let _ = stop_tx.send(true);
        for sender in &senders {
            let _ = sender.send(SwarmMessage::Shutdown);
        }
        while tasks.join_next().await.is_some() {}

        let (results, stats) = {
            let state = board_lock(&board);
            let stats = SwarmStats {
                rounds: round as usize,
                messages: state.messages,
                findings: state.findings,
                completed_workers: state.done.union(&dead).count(),
            };
            (sorted_results(&state.results), stats)
        };
        *self
            .last_stats
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(stats);

        tracing::debug!(
            intent = %plan.intent,
            workers = worker_count,
            rounds = stats.rounds,
            messages = stats.messages,
            findings = stats.findings,
            reason = ?reason,
            "swarm plan completed"
        );

        let aggregation = self.aggregator.aggregate(plan, &results).await?;
        Ok(IntentResult {
            summary: aggregation.summary,
            fact: aggregation.fact,
            worker_results: results,
        })
    }
}

#[derive(Debug)]
struct WorkerReport {
    worker_key: String,
    round: u64,
}

#[derive(Debug, Default)]
struct BoardState {
    results: Vec<WorkerResult>,
    messages: usize,
    findings: usize,
    last_finding_round: u64,
    done: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopReason {
    MaxRounds,
    MaxMessages,
    NoNewFindings,
    AllDone,
}

fn stop_reason(
    state: &BoardState,
    round: u64,
    limits: &CapabilityLimits,
    workers: usize,
    dead: usize,
) -> Option<StopReason> {
    if state.messages >= limits.max_messages {
        return Some(StopReason::MaxMessages);
    }
    if round >= limits.max_rounds as u64 {
        return Some(StopReason::MaxRounds);
    }
    if state.done.len() + dead >= workers {
        return Some(StopReason::AllDone);
    }
    if round.saturating_sub(state.last_finding_round) >= 2 {
        return Some(StopReason::NoNewFindings);
    }
    None
}

/// Wait until every live worker reports the current round, or until workers
/// disappear. Joining a task here also catches panics so the coordinator
/// cannot wait forever.
async fn wait_for_round(
    reports: &mut mpsc::UnboundedReceiver<WorkerReport>,
    tasks: &mut JoinSet<()>,
    identities: &mut HashMap<Id, (String, String)>,
    board: &Mutex<BoardState>,
    workers: usize,
    round: u64,
    dead: &mut BTreeSet<String>,
) {
    let mut reported = BTreeSet::new();
    while reported.len() + dead.len() < workers {
        tokio::select! {
            report = reports.recv() => {
                match report {
                    Some(report)
                        if report.round == round && !dead.contains(&report.worker_key) =>
                    {
                        reported.insert(report.worker_key);
                    }
                    Some(_) => {}
                    None => {
                        // Every worker sender is gone. Any identity left is a
                        // task that ended without reporting.
                        for (_, (worker_key, _)) in identities.drain() {
                            dead.insert(worker_key);
                        }
                        return;
                    }
                }
            }
            joined = tasks.join_next_with_id() => {
                match joined {
                    Some(Ok((id, ()))) => {
                        if let Some((worker_key, _)) = identities.remove(&id) {
                            reported.remove(&worker_key);
                            dead.insert(worker_key);
                        }
                    }
                    Some(Err(error)) => {
                        let (worker_key, subtask_id) = identities
                            .remove(&error.id())
                            .unwrap_or_else(|| ("<unknown>".to_owned(), "<unknown>".to_owned()));
                        let reason = if error.is_panic() {
                            format!("swarm worker task panicked: {error}")
                        } else {
                            format!("swarm worker task cancelled: {error}")
                        };
                        board_lock(board)
                            .results
                            .push(WorkerResult::failed(worker_key.clone(), subtask_id, reason));
                        reported.remove(&worker_key);
                        dead.insert(worker_key);
                    }
                    None => return,
                }
            }
        }
    }
}

/// Shared runtime handles handed to every swarm worker task.
struct WorkerContext<E> {
    executor: Arc<E>,
    permits: Arc<Semaphore>,
    board: Arc<Mutex<BoardState>>,
    messages: Arc<Mutex<Vec<SwarmMessage>>>,
    rounds: watch::Receiver<u64>,
    stop: watch::Receiver<bool>,
    reports: mpsc::UnboundedSender<WorkerReport>,
    intent: String,
    limits: CapabilityLimits,
}

async fn run_worker<E: WorkerExecutor + 'static>(
    mut ctx: WorkerContext<E>,
    worker: WorkerSpec,
    seed: Subtask,
    mut inbox: mpsc::UnboundedReceiver<SwarmMessage>,
    peers: Vec<mpsc::UnboundedSender<SwarmMessage>>,
) {
    let mut context = vec![format!("intent: {}", ctx.intent)];
    let mut turns = 0usize;
    let mut last_round = 1u64;
    let mut done = false;

    if worker.max_turns == 0 {
        mark_done(
            &worker.key,
            &ctx.board,
            &ctx.messages,
            &peers,
            ctx.limits.max_messages,
        );
        done = true;
    } else {
        let result = execute(
            ctx.executor.as_ref(),
            &worker,
            &seed,
            &context,
            &ctx.permits,
            &ctx.board,
            &ctx.messages,
            &peers,
            1,
            ctx.limits.max_messages,
        )
        .await;
        turns += 1;
        push_result_context(&mut context, &result);
        if turns >= worker.max_turns {
            mark_done(
                &worker.key,
                &ctx.board,
                &ctx.messages,
                &peers,
                ctx.limits.max_messages,
            );
            done = true;
        }
    }
    let _ = ctx.reports.send(WorkerReport {
        worker_key: worker.key.clone(),
        round: 1,
    });

    loop {
        if *ctx.stop.borrow() {
            break;
        }

        tokio::select! {
            biased;
            changed = ctx.stop.changed() => {
                if changed.is_err() || *ctx.stop.borrow() {
                    break;
                }
            }
            changed = ctx.rounds.changed() => {
                if changed.is_err() {
                    break;
                }
                let round = *ctx.rounds.borrow();
                if round <= last_round {
                    continue;
                }
                last_round = round;
                if *ctx.stop.borrow() {
                    break;
                }

                if !done {
                    match inbox.try_recv() {
                        Ok(SwarmMessage::Shutdown) => break,
                        Ok(message) => {
                            context.push(describe_message(&message));
                            trim_context(&mut context);

                            if let Some(subtask) = reaction_subtask(&seed, round, &message) {
                                if worker.max_turns > turns {
                                    let result = execute(
                                        ctx.executor.as_ref(),
                                        &worker,
                                        &subtask,
                                        &context,
                                        &ctx.permits,
                                        &ctx.board,
                                        &ctx.messages,
                                        &peers,
                                        round,
                                        ctx.limits.max_messages,
                                    )
                                    .await;
                                    turns += 1;
                                    push_result_context(&mut context, &result);
                                    if turns >= worker.max_turns {
                                        mark_done(
                                            &worker.key,
                                            &ctx.board,
                                            &ctx.messages,
                                            &peers,
                                            ctx.limits.max_messages,
                                        );
                                        done = true;
                                    }
                                } else {
                                    mark_done(
                                        &worker.key,
                                        &ctx.board,
                                        &ctx.messages,
                                        &peers,
                                        ctx.limits.max_messages,
                                    );
                                    done = true;
                                }
                            } else if let SwarmMessage::Done { from } = &message {
                                board_lock(&ctx.board).done.insert(from.clone());
                            }
                        }
                        Err(mpsc::error::TryRecvError::Empty) => {}
                        Err(mpsc::error::TryRecvError::Disconnected) => break,
                    }
                }

                let _ = ctx.reports.send(WorkerReport {
                    worker_key: worker.key.clone(),
                    round,
                });
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute<E: WorkerExecutor>(
    executor: &E,
    worker: &WorkerSpec,
    subtask: &Subtask,
    context: &[String],
    permits: &Semaphore,
    board: &Mutex<BoardState>,
    messages: &Mutex<Vec<SwarmMessage>>,
    peers: &[mpsc::UnboundedSender<SwarmMessage>],
    round: u64,
    max_messages: usize,
) -> WorkerResult {
    let _permit = permits
        .acquire()
        .await
        .expect("swarm semaphore is never closed");
    let result = match executor.run(worker, subtask, context).await {
        Ok(mut result) => {
            // Identity belongs to the runtime, not the executor.
            result.worker_key = worker.key.clone();
            result.subtask_id = subtask.id.clone();
            result
        }
        Err(error) => {
            WorkerResult::failed(worker.key.clone(), subtask.id.clone(), error.to_string())
        }
    };
    record_result(board, messages, peers, &result, round, max_messages);
    result
}

fn record_result(
    board: &Mutex<BoardState>,
    messages: &Mutex<Vec<SwarmMessage>>,
    peers: &[mpsc::UnboundedSender<SwarmMessage>],
    result: &WorkerResult,
    round: u64,
    max_messages: usize,
) {
    {
        let mut state = board_lock(board);
        state.results.push(result.clone());
        if result.status == WorkerStatus::Ok {
            state.findings += 1;
            state.last_finding_round = state.last_finding_round.max(round);
        }
    }

    match result.status {
        WorkerStatus::Ok => {
            broadcast(
                board,
                messages,
                peers,
                SwarmMessage::Finding {
                    from: result.worker_key.clone(),
                    subtask_id: result.subtask_id.clone(),
                    summary: result.summary.clone(),
                    evidence: result.evidence.clone(),
                },
                max_messages,
            );
        }
        WorkerStatus::Failed => {
            broadcast(
                board,
                messages,
                peers,
                SwarmMessage::Challenge {
                    from: result.worker_key.clone(),
                    text: result
                        .error
                        .clone()
                        .unwrap_or_else(|| "worker failed without an error".to_owned()),
                },
                max_messages,
            );
        }
        // A skipped result is recorded but nothing is announced: the worker
        // declined this turn and remains available for later peer messages.
        WorkerStatus::Skipped => {}
    }
}

fn mark_done(
    worker_key: &str,
    board: &Mutex<BoardState>,
    messages: &Mutex<Vec<SwarmMessage>>,
    peers: &[mpsc::UnboundedSender<SwarmMessage>],
    max_messages: usize,
) {
    {
        let mut state = board_lock(board);
        if !state.done.insert(worker_key.to_owned()) {
            return;
        }
    }
    broadcast(
        board,
        messages,
        peers,
        SwarmMessage::Done {
            from: worker_key.to_owned(),
        },
        max_messages,
    );
}

/// Count and deliver one logical message. The board owns the budget, so a
/// broadcast can never push the swarm past `max_messages`. Every delivered
/// message is also appended to the run log in broadcast order.
fn broadcast(
    board: &Mutex<BoardState>,
    messages: &Mutex<Vec<SwarmMessage>>,
    peers: &[mpsc::UnboundedSender<SwarmMessage>],
    message: SwarmMessage,
    max_messages: usize,
) -> bool {
    {
        let mut state = board_lock(board);
        if state.messages >= max_messages {
            return false;
        }
        state.messages += 1;
    }
    messages
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(message.clone());
    for peer in peers {
        let _ = peer.send(message.clone());
    }
    true
}

fn reaction_subtask(seed: &Subtask, round: u64, message: &SwarmMessage) -> Option<Subtask> {
    match message {
        SwarmMessage::Finding {
            from,
            subtask_id,
            summary,
            ..
        } => Some(Subtask {
            id: format!("{}:r{round}:peer:{from}", seed.id),
            description: format!("Build on {from}'s finding from {subtask_id}: {summary}"),
            worker_key: None,
        }),
        SwarmMessage::Question { from, text } => Some(Subtask {
            id: format!("{}:r{round}:question:{from}", seed.id),
            description: format!("Answer {from}'s question: {text}"),
            worker_key: None,
        }),
        SwarmMessage::Challenge { from, text } => Some(Subtask {
            id: format!("{}:r{round}:challenge:{from}", seed.id),
            description: format!("Address {from}'s challenge: {text}"),
            worker_key: None,
        }),
        SwarmMessage::Done { .. } | SwarmMessage::Shutdown => None,
    }
}

fn describe_message(message: &SwarmMessage) -> String {
    match message {
        SwarmMessage::Finding {
            from,
            subtask_id,
            summary,
            ..
        } => format!("finding from {from} [{subtask_id}]: {summary}"),
        SwarmMessage::Question { from, text } => format!("question from {from}: {text}"),
        SwarmMessage::Challenge { from, text } => format!("challenge from {from}: {text}"),
        SwarmMessage::Done { from } => format!("done from {from}"),
        SwarmMessage::Shutdown => "shutdown".to_owned(),
    }
}

fn push_result_context(context: &mut Vec<String>, result: &WorkerResult) {
    if !result.summary.is_empty() {
        context.push(format!(
            "own result [{}]: {}",
            result.subtask_id, result.summary
        ));
        trim_context(context);
    }
}

fn trim_context(context: &mut Vec<String>) {
    while context.len() > MAX_CONTEXT_ENTRIES {
        context.remove(1);
    }
}

fn board_lock(board: &Mutex<BoardState>) -> MutexGuard<'_, BoardState> {
    board
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
