//! Guarded tool executor.
//!
//! `GuardedExecutor` is the only path from an agent to a tool. It records
//! every call before evaluating policy, records the verdict, and only then
//! executes the backend. An `Ask` verdict blocks the tool call: the worker
//! waits in-process for a decision or the deadline, then continues in the
//! same task loop. The approval row is the durable record; the waiter is
//! only the in-process wake.
//!
//! The policy lives here and nowhere else: this is the one guard, evaluated
//! at the tool-call point. The intent id is stamped by the caller, so every
//! tool call is linked to the intent that asked for it.

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use dhi_core::{
    Approval, ApprovalDecision, ApprovalId, ApprovalStatus, GuardStore, IntentId, TaskId,
    ToolCallId, ToolCallOutcome, ToolCallRequest,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, oneshot};

use crate::{Policy, SharedPolicy};

/// In-process wake for a worker blocked on an approval.
type Waiter = oneshot::Sender<()>;

/// Actual implementation of a tool. The executor never reaches a backend
/// without a recorded call and an `Allow` verdict.
#[async_trait]
pub trait ToolBackend: Send + Sync {
    async fn execute(
        &self,
        tool: &str,
        input: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value>;
}

/// Guarded entry point for tool calls and approval resolution.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Records, evaluates, and (when allowed) executes a tool call.
    ///
    /// The intent id comes from the caller, never from the model, so a call
    /// can never be detached from the intent that requested it. An `Ask`
    /// verdict blocks here until the approval is decided or expires, then
    /// either runs the backend or returns `Denied`. This call never returns
    /// `Pending`.
    async fn execute(
        &self,
        task_id: TaskId,
        intent_id: IntentId,
        request: ToolCallRequest,
    ) -> anyhow::Result<ToolCallOutcome>;

    /// Applies a human decision to a pending approval and wakes its waiter.
    ///
    /// This only resolves the durable row; it never executes the backend and
    /// never finishes an intent. Returns `None` when the approval does not
    /// exist or is no longer pending.
    async fn decide(
        &self,
        approval_id: ApprovalId,
        decision: ApprovalDecision,
    ) -> anyhow::Result<Option<Approval>>;

    /// Expires every pending approval of `task_id` whose expiry has passed,
    /// wakes each waiter it finds, and returns the rows it changed. The
    /// supervisor sweeps on a fixed interval so a waiting approval never
    /// depends on a person checking the queue.
    async fn sweep_expired(&self, task_id: TaskId) -> anyhow::Result<Vec<Approval>>;
}

/// Default guard executor backed by a `GuardStore` and a `ToolBackend`.
///
/// The policy lives behind a [`SharedPolicy`] handle: the executor always
/// evaluates the current snapshot, and the server swaps in a freshly compiled
/// snapshot after every rule or settings mutation. A snapshot whose settings
/// timeout is unset (`0`) keeps the construction `approval_ttl`, so the
/// legacy `new` constructor behaves exactly as before.
pub struct GuardedExecutor {
    store: Arc<dyn GuardStore>,
    policy: SharedPolicy,
    backend: Arc<dyn ToolBackend>,
    approval_ttl: Duration,
    /// Serializes expiry: a deadline timer and [`ToolExecutor::sweep_expired`]
    /// never expire the same approval at the same time.
    expiry_lock: Arc<Mutex<()>>,
    /// Workers blocked on an approval, keyed by approval id. The row is the
    /// durable state; this map is only the in-process wake. A decision or
    /// expiry removes the sender and wakes the receiver.
    waiters: Arc<Mutex<HashMap<ApprovalId, Waiter>>>,
}

impl GuardedExecutor {
    pub fn new(
        store: Arc<dyn GuardStore>,
        policy: Policy,
        backend: Arc<dyn ToolBackend>,
        approval_ttl: Duration,
    ) -> Self {
        Self::with_policy(
            store,
            SharedPolicy::from_policy(&policy, 0),
            backend,
            approval_ttl,
        )
    }

    /// The snapshot-driven constructor: the executor evaluates whatever the
    /// handle currently holds, and hot rebuilds appear on the next call.
    pub fn with_policy(
        store: Arc<dyn GuardStore>,
        policy: SharedPolicy,
        backend: Arc<dyn ToolBackend>,
        approval_ttl: Duration,
    ) -> Self {
        Self {
            store,
            policy,
            backend,
            approval_ttl,
            expiry_lock: Arc::new(Mutex::new(())),
            waiters: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The shared handle the server uses to publish rebuilt snapshots.
    pub fn policy_handle(&self) -> SharedPolicy {
        self.policy.clone()
    }

    /// The ask-approval budget for one verdict: the matched rule's timeout,
    /// else the snapshot's settings default, else this executor's own
    /// construction TTL verbatim (sub-second precision intact — the
    /// prescription is only converted to seconds when it actually applies).
    fn approval_ttl_for(
        &self,
        snapshot: &crate::CompiledPolicy,
        decision: &crate::Decision,
    ) -> Duration {
        let prescribed = snapshot.prescribed_approval_secs(decision);
        if prescribed > 0 {
            Duration::seconds(prescribed)
        } else {
            self.approval_ttl
        }
    }

    /// Expires due approvals of `task_id` and wakes every waiter behind one
    /// of the rows it changed.
    ///
    /// The expiry lock keeps the deadline timer and the supervisor sweep from
    /// expiring the same row at once. Waking happens after the lock is
    /// released, so a waiter can re-read the store without deadlocking.
    async fn expire_due(
        store: Arc<dyn GuardStore>,
        waiters: Arc<Mutex<HashMap<ApprovalId, Waiter>>>,
        expiry_lock: Arc<Mutex<()>>,
        task_id: TaskId,
        now: DateTime<Utc>,
    ) -> Result<Vec<Approval>> {
        let expired = {
            let _sweeping = expiry_lock.lock().await;
            store
                .expire_pending_approvals(task_id, now)
                .await
                .map_err(|err| err.context("failed to expire pending approvals"))?
        };
        for approval in &expired {
            let waiter = waiters.lock().await.remove(&approval.id);
            if let Some(waiter) = waiter {
                let _ = waiter.send(());
            }
        }
        Ok(expired)
    }

    /// Registers this worker as the waiter for an approval. The receiver is
    /// returned before the caller re-reads the row, closing the race with a
    /// decision that lands between insert and wait.
    async fn register_waiter(&self, approval_id: ApprovalId) -> oneshot::Receiver<()> {
        let (sender, receiver) = oneshot::channel();
        self.waiters.lock().await.insert(approval_id, sender);
        receiver
    }

    /// Removes a waiter without waking it. Used once the worker has resolved
    /// the approval and is about to continue.
    async fn drop_waiter(&self, approval_id: ApprovalId) {
        self.waiters.lock().await.remove(&approval_id);
    }

    /// Runs the backend for an already-recorded call and completes the record.
    async fn run_backend(
        &self,
        tool_call_id: ToolCallId,
        tool: &str,
        input: &serde_json::Value,
    ) -> Result<ToolCallOutcome> {
        match self.backend.execute(tool, input).await {
            Ok(output) => {
                self.store
                    .complete_tool_call(tool_call_id, Some(output.clone()), "ok")
                    .await?;
                Ok(ToolCallOutcome::Allowed { output })
            }
            Err(err) => {
                if let Err(store_err) = self
                    .store
                    .complete_tool_call(tool_call_id, None, "error")
                    .await
                {
                    return Err(err.context(format!(
                        "tool failed and the call record could not be completed: {store_err:#}"
                    )));
                }
                Err(err)
            }
        }
    }

    /// Completes the record behind an approval with the given terminal status.
    async fn complete_approval_call(&self, tool_call_id: ToolCallId, status: &str) -> Result<()> {
        self.store
            .complete_tool_call(tool_call_id, None, status)
            .await
    }

    /// Arms the per-approval timer for an approval that was just parked.
    ///
    /// The task sleeps until `expires_at`, expires the task's due approvals,
    /// and wakes every waiter behind one of them. Expiry therefore happens at
    /// the deadline, not at the next supervisor pass: the 30-second supervisor
    /// remains a backstop for a restarted process or a lost timer task.
    ///
    /// The timer never touches an intent. The worker blocked inside the tool
    /// call wakes, re-reads the expired row, and finishes the call as denied.
    ///
    /// Best effort: every failure is logged, never raised, so a timer problem
    /// can never break the tool-call path.
    fn arm_expiry_timer(&self, approval: Approval) {
        let store = self.store.clone();
        let waiters = self.waiters.clone();
        let approval_id = approval.id;
        let task_id = approval.task_id;
        let expires_at = approval.expires_at;
        // Held for the lifetime of the timer task. A running sweep holds it
        // while it expires and consumes its rows, so expiry and the backstop
        // can never both expire the same approval.
        let expiry_lock = self.expiry_lock.clone();
        tokio::spawn(async move {
            let wait = expires_at.signed_duration_since(Utc::now());
            if let Ok(std_duration) = wait.to_std() {
                tokio::time::sleep(std_duration).await;
            }
            // The clock is read after the wake, not before the call: a
            // deadline crossed during the last microsecond is then due.
            if let Err(err) =
                Self::expire_due(store, waiters, expiry_lock, task_id, Utc::now()).await
            {
                tracing::warn!(
                    %task_id,
                    %approval_id,
                    error = %err,
                    "approval expiry timer could not expire the task; the supervisor sweeps"
                );
            }
        });
    }

    /// Waits for an approval to leave `pending`, either because a person
    /// decided it or because its deadline passed. The row is always re-read
    /// after the wake, so the stored status is the single source of truth.
    async fn wait_for_terminal(
        &self,
        approval: Approval,
        mut receiver: oneshot::Receiver<()>,
    ) -> Result<Approval> {
        let mut current = approval;
        loop {
            match current.status {
                ApprovalStatus::Pending => {}
                _ => return Ok(current),
            }

            let now = Utc::now();
            if now >= current.expires_at {
                // Expire at the deadline. If the clock lands exactly on
                // `expires_at`, the store's strict comparison needs one more
                // pass; the loop below retries after a short sleep.
                Self::expire_due(
                    self.store.clone(),
                    self.waiters.clone(),
                    self.expiry_lock.clone(),
                    current.task_id,
                    now,
                )
                .await?;
            } else {
                let wait = (current.expires_at - now)
                    .to_std()
                    .unwrap_or(std::time::Duration::ZERO);
                // A wake means a decision or an expiry wrote the row first; a
                // closed channel means the sender was dropped. Either way the
                // row is re-read below.
                let _ = tokio::time::timeout(wait, &mut receiver).await;
            }

            current = self
                .store
                .get_approval(current.id)
                .await
                .map_err(|err| err.context("failed to re-read approval"))?
                .ok_or_else(|| anyhow!("approval {} disappeared while waiting", current.id))?;

            if current.status == ApprovalStatus::Pending {
                // The deadline passed but the store has not expired the row
                // yet (or a spurious wake arrived early). Sleep briefly and
                // re-check instead of ever returning `Pending`.
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        }
    }

    /// Loads the tool call attached to an approval, failing closed when absent.
    async fn load_approval_call(&self, approval: &Approval) -> Result<dhi_core::ToolCall> {
        self.store
            .get_tool_call(approval.tool_call_id)
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "tool call {} for approval {} is missing",
                    approval.tool_call_id,
                    approval.id
                )
            })
    }
}

#[async_trait]
impl ToolExecutor for GuardedExecutor {
    async fn sweep_expired(&self, task_id: TaskId) -> Result<Vec<Approval>> {
        Self::expire_due(
            self.store.clone(),
            self.waiters.clone(),
            self.expiry_lock.clone(),
            task_id,
            Utc::now(),
        )
        .await
    }

    async fn execute(
        &self,
        task_id: TaskId,
        intent_id: IntentId,
        request: ToolCallRequest,
    ) -> Result<ToolCallOutcome> {
        // Record first: nothing may run without a durable call record.
        let tool_call = self
            .store
            .record_tool_call(
                task_id,
                intent_id,
                request.tool.clone(),
                request.input.clone(),
            )
            .await
            .map_err(|err| err.context("failed to record tool call"))?;

        // Evaluate under the current snapshot: clone the Arc out, then drop
        // the lock guard before any await.
        let snapshot = self.policy.load();
        let decision = snapshot.evaluate(&request.tool, &request.input);
        let approval_ttl = self.approval_ttl_for(&snapshot, &decision);

        let guard_decision = self
            .store
            .record_guard_decision(
                tool_call.id,
                decision.verdict,
                decision.matched_rule,
                Some(decision.rule_version),
                decision.reason.clone(),
            )
            .await
            .map_err(|err| err.context("failed to record guard decision"))?;

        match decision.verdict {
            dhi_core::GuardVerdict::Allow => {
                self.run_backend(tool_call.id, &request.tool, &request.input)
                    .await
            }
            dhi_core::GuardVerdict::Deny => {
                self.complete_approval_call(tool_call.id, "denied").await?;
                Ok(ToolCallOutcome::Denied {
                    reason: decision.reason,
                })
            }
            dhi_core::GuardVerdict::Ask => {
                let approval = self
                    .store
                    .create_approval(
                        task_id,
                        tool_call.id,
                        guard_decision.id,
                        decision.reason,
                        Utc::now() + approval_ttl,
                    )
                    .await
                    .map_err(|err| err.context("failed to create approval"))?;
                self.complete_approval_call(tool_call.id, "pending").await?;
                // The call is durably pending: arm the deadline timer, then
                // register this worker before re-reading the row. A decision
                // that lands between create and insert is therefore either
                // seen by the re-read or delivered through the waiter.
                self.arm_expiry_timer(approval.clone());
                let receiver = self.register_waiter(approval.id).await;
                let current = match self.store.get_approval(approval.id).await {
                    Ok(Some(current)) => current,
                    Ok(None) => {
                        self.drop_waiter(approval.id).await;
                        return Err(anyhow!("approval {} vanished after creation", approval.id));
                    }
                    Err(err) => {
                        self.drop_waiter(approval.id).await;
                        return Err(err.context("failed to re-read approval"));
                    }
                };
                let resolved = self.wait_for_terminal(current, receiver).await;
                self.drop_waiter(approval.id).await;
                let resolved = resolved?;

                match resolved.status {
                    ApprovalStatus::Granted => {
                        // One-guard rule: an approved call runs under the
                        // verdict recorded at call time. The stored grant is
                        // the authorization; policy is not re-evaluated here.
                        let tool_call = self.load_approval_call(&resolved).await?;
                        self.run_backend(tool_call.id, &tool_call.tool_name, &tool_call.input)
                            .await
                    }
                    ApprovalStatus::Denied => {
                        self.complete_approval_call(tool_call.id, "denied").await?;
                        Ok(ToolCallOutcome::Denied {
                            reason: resolved.reason,
                        })
                    }
                    ApprovalStatus::Expired => {
                        self.complete_approval_call(tool_call.id, "expired").await?;
                        Ok(ToolCallOutcome::Denied {
                            reason: format!("approval expired: {}", resolved.reason),
                        })
                    }
                    ApprovalStatus::Pending => Err(anyhow!(
                        "approval {} is still pending after the wait; refusing to surface Pending",
                        resolved.id
                    )),
                }
            }
        }
    }

    async fn decide(
        &self,
        approval_id: ApprovalId,
        decision: ApprovalDecision,
    ) -> Result<Option<Approval>> {
        let approval = self
            .store
            .decide_approval(approval_id, decision)
            .await
            .map_err(|err| err.context("failed to decide approval"))?;

        let Some(approval) = approval else {
            return Ok(None);
        };

        // The row is the durable record; the waiter only wakes the worker.
        // The backend runs in `execute` after it re-reads this decision.
        let waiter = self.waiters.lock().await.remove(&approval_id);
        if let Some(waiter) = waiter {
            let _ = waiter.send(());
        }
        Ok(Some(approval))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use dhi_core::{
        Approval, ApprovalDecision, ApprovalId, ApprovalStatus, GuardDecision, GuardDecisionId,
        GuardVerdict, Intent, IntentState, TaskId as CoreTaskId, ToolCall, ToolCallId,
        ToolCallRequest,
    };
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration as StdDuration;
    use uuid::Uuid;

    use crate::executor::ToolBackend;

    #[derive(Default)]
    struct StoreState {
        tool_calls: HashMap<ToolCallId, ToolCall>,
        guard_decisions: Vec<GuardDecision>,
        approvals: HashMap<ApprovalId, Approval>,
        intents: HashMap<IntentId, Intent>,
    }

    struct FakeStore {
        state: Mutex<StoreState>,
        /// Simulates a decision that lands after the approval row is created
        /// but before the worker registers its waiter.
        allow_on_create: bool,
    }

    impl FakeStore {
        fn new() -> Self {
            Self {
                state: Mutex::new(StoreState::default()),
                allow_on_create: false,
            }
        }

        fn with_early_allow() -> Self {
            Self {
                state: Mutex::new(StoreState::default()),
                allow_on_create: true,
            }
        }

        fn tool_calls(&self) -> Vec<ToolCall> {
            self.state
                .lock()
                .unwrap()
                .tool_calls
                .values()
                .cloned()
                .collect()
        }

        fn guard_decisions(&self) -> Vec<GuardDecision> {
            self.state.lock().unwrap().guard_decisions.clone()
        }

        fn approvals(&self) -> Vec<Approval> {
            self.state
                .lock()
                .unwrap()
                .approvals
                .values()
                .cloned()
                .collect()
        }

        fn intent(&self, intent_id: IntentId) -> Option<Intent> {
            self.state.lock().unwrap().intents.get(&intent_id).cloned()
        }

        /// Moves an approval's deadline without going through the clock. A
        /// sweep test uses it to make a fresh row due now.
        fn set_approval_expiry(&self, approval_id: ApprovalId, expires_at: DateTime<Utc>) {
            let mut state = self.state.lock().unwrap();
            let approval = state
                .approvals
                .get_mut(&approval_id)
                .expect("the approval exists");
            approval.expires_at = expires_at;
        }
    }

    /// Seeds a paused intent, the legacy state an old approval left behind.
    fn paused_intent(store: &FakeStore, task_id: CoreTaskId) -> Intent {
        let mut intent = Intent::new(task_id, Uuid::new_v4(), "write the file")
            .expect("a test intent needs a non-nil fact id");
        intent.state = IntentState::Paused;
        store
            .state
            .lock()
            .unwrap()
            .intents
            .insert(intent.id, intent.clone());
        intent
    }

    #[async_trait]
    impl GuardStore for FakeStore {
        async fn record_tool_call(
            &self,
            task_id: TaskId,
            intent_id: IntentId,
            tool: String,
            input: serde_json::Value,
        ) -> anyhow::Result<ToolCall> {
            let call = ToolCall {
                id: Uuid::new_v4(),
                task_id,
                intent_id,
                tool_name: tool,
                input,
                output: None,
                status: "recorded".to_string(),
                created_at: Utc::now(),
            };
            self.state
                .lock()
                .unwrap()
                .tool_calls
                .insert(call.id, call.clone());
            Ok(call)
        }

        async fn get_tool_call(
            &self,
            tool_call_id: ToolCallId,
        ) -> anyhow::Result<Option<ToolCall>> {
            Ok(self
                .state
                .lock()
                .unwrap()
                .tool_calls
                .get(&tool_call_id)
                .cloned())
        }

        async fn complete_tool_call(
            &self,
            tool_call_id: ToolCallId,
            output: Option<serde_json::Value>,
            status: &str,
        ) -> anyhow::Result<()> {
            let mut state = self.state.lock().unwrap();
            let call = state
                .tool_calls
                .get_mut(&tool_call_id)
                .ok_or_else(|| anyhow!("unknown tool call {tool_call_id}"))?;
            call.output = output;
            call.status = status.to_string();
            Ok(())
        }

        async fn record_guard_decision(
            &self,
            tool_call_id: ToolCallId,
            verdict: GuardVerdict,
            matched_rule: Option<String>,
            rule_version: Option<String>,
            reason: String,
        ) -> anyhow::Result<GuardDecision> {
            let decision = GuardDecision {
                id: Uuid::new_v4(),
                tool_call_id,
                verdict,
                matched_rule,
                rule_version,
                reason,
                created_at: Utc::now(),
            };
            self.state
                .lock()
                .unwrap()
                .guard_decisions
                .push(decision.clone());
            Ok(decision)
        }

        async fn create_approval(
            &self,
            task_id: TaskId,
            tool_call_id: ToolCallId,
            guard_decision_id: GuardDecisionId,
            reason: String,
            expires_at: DateTime<Utc>,
        ) -> anyhow::Result<Approval> {
            let mut approval = Approval {
                id: Uuid::new_v4(),
                task_id,
                tool_call_id,
                guard_decision_id,
                status: ApprovalStatus::Pending,
                reason,
                expires_at,
                decided_at: None,
                created_at: Utc::now(),
            };
            if self.allow_on_create {
                approval.status = ApprovalStatus::Granted;
                approval.decided_at = Some(Utc::now());
            }
            self.state
                .lock()
                .unwrap()
                .approvals
                .insert(approval.id, approval.clone());
            Ok(approval)
        }

        async fn get_approval(&self, approval_id: ApprovalId) -> anyhow::Result<Option<Approval>> {
            Ok(self
                .state
                .lock()
                .unwrap()
                .approvals
                .get(&approval_id)
                .cloned())
        }

        async fn list_pending_approvals(&self, task_id: TaskId) -> anyhow::Result<Vec<Approval>> {
            Ok(self
                .state
                .lock()
                .unwrap()
                .approvals
                .values()
                .filter(|a| a.task_id == task_id && a.status == ApprovalStatus::Pending)
                .cloned()
                .collect())
        }

        async fn list_pending_approvals_all(&self) -> anyhow::Result<Vec<Approval>> {
            Ok(self
                .state
                .lock()
                .unwrap()
                .approvals
                .values()
                .filter(|a| a.status == ApprovalStatus::Pending)
                .cloned()
                .collect())
        }

        async fn expire_pending_approvals(
            &self,
            task_id: TaskId,
            now: DateTime<Utc>,
        ) -> anyhow::Result<Vec<Approval>> {
            let mut state = self.state.lock().unwrap();
            let mut expired = Vec::new();
            for approval in state.approvals.values_mut() {
                if approval.task_id == task_id
                    && approval.status == ApprovalStatus::Pending
                    && approval.expires_at < now
                {
                    approval.status = ApprovalStatus::Expired;
                    approval.decided_at = None;
                    expired.push(approval.clone());
                }
            }
            Ok(expired)
        }

        async fn list_expired_approvals(&self, task_id: TaskId) -> anyhow::Result<Vec<Approval>> {
            Ok(self
                .state
                .lock()
                .unwrap()
                .approvals
                .values()
                .filter(|a| a.task_id == task_id && a.status == ApprovalStatus::Expired)
                .cloned()
                .collect())
        }

        async fn block_intent_for_approval(
            &self,
            approval_id: ApprovalId,
            owner: &str,
        ) -> anyhow::Result<bool> {
            let mut state = self.state.lock().unwrap();
            let intent_id = state
                .approvals
                .get(&approval_id)
                .and_then(|a| state.tool_calls.get(&a.tool_call_id))
                .map(|tc| tc.intent_id);
            let Some(intent_id) = intent_id else {
                return Ok(false);
            };
            let Some(intent) = state.intents.get_mut(&intent_id) else {
                return Ok(false);
            };
            if intent.state != IntentState::Paused {
                return Ok(false);
            }
            intent.state = IntentState::Running;
            intent.owner = Some(owner.to_string());
            intent.state = IntentState::Blocked;
            intent.completed_at = Some(Utc::now());
            Ok(true)
        }

        async fn decide_approval(
            &self,
            approval_id: ApprovalId,
            decision: ApprovalDecision,
        ) -> anyhow::Result<Option<Approval>> {
            let mut state = self.state.lock().unwrap();
            let Some(approval) = state.approvals.get_mut(&approval_id) else {
                return Ok(None);
            };
            if approval.status != ApprovalStatus::Pending {
                return Ok(None);
            }
            if approval.expires_at < Utc::now() {
                approval.status = ApprovalStatus::Expired;
                approval.decided_at = None;
                return Ok(Some(approval.clone()));
            }
            approval.status = match decision {
                ApprovalDecision::Allow => ApprovalStatus::Granted,
                ApprovalDecision::Deny => ApprovalStatus::Denied,
            };
            approval.decided_at = Some(Utc::now());
            Ok(Some(approval.clone()))
        }
    }

    struct FakeBackend {
        calls: AtomicUsize,
    }

    impl FakeBackend {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ToolBackend for FakeBackend {
        async fn execute(
            &self,
            tool: &str,
            input: &serde_json::Value,
        ) -> anyhow::Result<serde_json::Value> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(json!({ "tool": tool, "input": input }))
        }
    }

    fn policy() -> Policy {
        Policy {
            version: "test-v1".to_string(),
            default: GuardVerdict::Deny,
            rules: vec![
                crate::Rule {
                    name: "allow-read".to_string(),
                    tool: "read_file".to_string(),
                    action: GuardVerdict::Allow,
                    reason: "reads are safe".to_string(),
                },
                crate::Rule {
                    name: "ask-write".to_string(),
                    tool: "write_file".to_string(),
                    action: GuardVerdict::Ask,
                    reason: "writes need approval".to_string(),
                },
            ],
        }
    }

    fn executor(store: &Arc<FakeStore>, backend: &Arc<FakeBackend>) -> GuardedExecutor {
        GuardedExecutor::new(
            store.clone(),
            policy(),
            backend.clone(),
            Duration::minutes(5),
        )
    }

    fn request(tool: &str) -> ToolCallRequest {
        ToolCallRequest {
            tool: tool.to_string(),
            input: json!({ "path": "/tmp/data" }),
        }
    }

    /// Waits until the executor has parked a call behind a pending approval.
    async fn wait_for_pending(store: &FakeStore, task_id: TaskId) -> Approval {
        let deadline = tokio::time::Instant::now() + StdDuration::from_secs(5);
        loop {
            if let Some(approval) = store
                .list_pending_approvals(task_id)
                .await
                .unwrap()
                .into_iter()
                .next()
            {
                return approval;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the call never parked behind an approval"
            );
            tokio::time::sleep(StdDuration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn allow_executes_backend_and_records_decision() {
        let store = Arc::new(FakeStore::new());
        let backend = Arc::new(FakeBackend::new());
        let executor = executor(&store, &backend);

        let outcome = executor
            .execute(Uuid::new_v4(), Uuid::new_v4(), request("read_file"))
            .await
            .unwrap();

        let ToolCallOutcome::Allowed { output } = outcome else {
            panic!("expected Allowed, got {outcome:?}");
        };
        assert_eq!(output["tool"], "read_file");
        assert_eq!(backend.calls(), 1);

        let calls = store.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].status, "ok");
        assert_eq!(calls[0].output, Some(output));

        let decisions = store.guard_decisions();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].verdict, GuardVerdict::Allow);
        assert_eq!(decisions[0].matched_rule.as_deref(), Some("allow-read"));
        assert_eq!(decisions[0].rule_version.as_deref(), Some("test-v1"));
    }

    #[tokio::test]
    async fn deny_does_not_execute_backend() {
        let store = Arc::new(FakeStore::new());
        let backend = Arc::new(FakeBackend::new());
        let executor = executor(&store, &backend);

        let outcome = executor
            .execute(Uuid::new_v4(), Uuid::new_v4(), request("shell"))
            .await
            .unwrap();

        assert!(matches!(outcome, ToolCallOutcome::Denied { .. }));
        assert_eq!(backend.calls(), 0);
        assert_eq!(store.tool_calls()[0].status, "denied");
        assert_eq!(store.guard_decisions()[0].verdict, GuardVerdict::Deny);
    }

    #[tokio::test]
    async fn ask_creates_pending_approval_and_blocks() {
        let store = Arc::new(FakeStore::new());
        let backend = Arc::new(FakeBackend::new());
        let executor = Arc::new(executor(&store, &backend));
        let task_id = Uuid::new_v4();

        let run = tokio::spawn({
            let executor = executor.clone();
            async move {
                executor
                    .execute(task_id, Uuid::new_v4(), request("write_file"))
                    .await
            }
        });

        let pending = wait_for_pending(&store, task_id).await;
        assert_eq!(backend.calls(), 0);
        assert_eq!(store.tool_calls()[0].status, "pending");
        assert!(pending.expires_at > Utc::now());
        assert_eq!(store.guard_decisions()[0].verdict, GuardVerdict::Ask);

        // Resolve the waiter so the spawned call does not outlive the test.
        let resolved = executor
            .decide(pending.id, ApprovalDecision::Deny)
            .await
            .unwrap()
            .expect("the approval is pending");
        assert_eq!(resolved.status, ApprovalStatus::Denied);

        let outcome = run.await.unwrap().unwrap();
        assert!(matches!(outcome, ToolCallOutcome::Denied { .. }));
        assert_eq!(backend.calls(), 0);
    }

    /// ARTEX parity (`intercept/intercept.go` defaultMessage): a rule stored
    /// with an empty reason still produces a readable approval — the fallback
    /// message naming the rule rides the decision, the guard-decision row,
    /// and the approval payload alike.
    #[tokio::test]
    async fn ask_rule_with_empty_reason_names_itself_in_the_approval_payload() {
        let store = Arc::new(FakeStore::new());
        let backend = Arc::new(FakeBackend::new());
        let shared = SharedPolicy::new(crate::CompiledPolicy::build(
            "silent-rules",
            GuardVerdict::Deny,
            0,
            &[crate::RuleSpec {
                name: "ask-quietly".to_string(),
                match_target: "tool_name".to_string(),
                match_type: "exact".to_string(),
                pattern: "write_file".to_string(),
                action: GuardVerdict::Ask,
                reason: String::new(),
                priority: 0,
                timeout_secs: 0,
            }],
        ));
        let executor = Arc::new(GuardedExecutor::with_policy(
            store.clone(),
            shared,
            backend.clone(),
            Duration::minutes(5),
        ));
        let task_id = Uuid::new_v4();

        let run = tokio::spawn({
            let executor = executor.clone();
            async move {
                executor
                    .execute(task_id, Uuid::new_v4(), request("write_file"))
                    .await
            }
        });

        let pending = wait_for_pending(&store, task_id).await;
        assert_eq!(
            pending.reason, "matched rule ask-quietly",
            "the approval payload carries the fallback message"
        );
        assert_eq!(
            store.guard_decisions()[0].reason,
            "matched rule ask-quietly"
        );

        let resolved = executor
            .decide(pending.id, ApprovalDecision::Deny)
            .await
            .unwrap()
            .expect("the approval is pending");
        assert_eq!(resolved.reason, "matched rule ask-quietly");

        let outcome = run.await.unwrap().unwrap();
        assert!(matches!(outcome, ToolCallOutcome::Denied { .. }));
        assert_eq!(backend.calls(), 0);
    }

    #[tokio::test]
    async fn approve_executes_and_returns_allowed() {
        let store = Arc::new(FakeStore::new());
        let backend = Arc::new(FakeBackend::new());
        let executor = Arc::new(executor(&store, &backend));
        let task_id = Uuid::new_v4();

        let run = tokio::spawn({
            let executor = executor.clone();
            async move {
                executor
                    .execute(task_id, Uuid::new_v4(), request("write_file"))
                    .await
            }
        });
        let pending = wait_for_pending(&store, task_id).await;

        let resolved = executor
            .decide(pending.id, ApprovalDecision::Allow)
            .await
            .unwrap()
            .expect("the approval is pending");
        assert_eq!(resolved.status, ApprovalStatus::Granted);
        assert_eq!(
            backend.calls(),
            0,
            "decide only resolves the row; the waiting worker runs the backend"
        );

        let outcome = tokio::time::timeout(StdDuration::from_secs(5), run)
            .await
            .expect("the waiting worker wakes")
            .unwrap()
            .unwrap();
        assert!(matches!(outcome, ToolCallOutcome::Allowed { .. }));
        assert_eq!(backend.calls(), 1);
        assert_eq!(store.tool_calls()[0].status, "ok");
        assert_eq!(store.approvals()[0].status, ApprovalStatus::Granted);
    }

    #[tokio::test]
    async fn deny_approval_returns_denied() {
        let store = Arc::new(FakeStore::new());
        let backend = Arc::new(FakeBackend::new());
        let executor = Arc::new(executor(&store, &backend));
        let task_id = Uuid::new_v4();

        let run = tokio::spawn({
            let executor = executor.clone();
            async move {
                executor
                    .execute(task_id, Uuid::new_v4(), request("write_file"))
                    .await
            }
        });
        let pending = wait_for_pending(&store, task_id).await;

        let resolved = executor
            .decide(pending.id, ApprovalDecision::Deny)
            .await
            .unwrap()
            .expect("the approval is pending");
        assert_eq!(resolved.status, ApprovalStatus::Denied);

        let outcome = tokio::time::timeout(StdDuration::from_secs(5), run)
            .await
            .expect("the waiting worker wakes")
            .unwrap()
            .unwrap();
        assert!(matches!(outcome, ToolCallOutcome::Denied { .. }));
        assert_eq!(backend.calls(), 0);
        assert_eq!(store.tool_calls()[0].status, "denied");
        assert_eq!(store.approvals()[0].status, ApprovalStatus::Denied);
    }

    #[tokio::test]
    async fn expired_approval_does_not_execute() {
        let store = Arc::new(FakeStore::new());
        let backend = Arc::new(FakeBackend::new());
        let executor = Arc::new(GuardedExecutor::new(
            store.clone(),
            policy(),
            backend.clone(),
            Duration::milliseconds(50),
        ));
        let task_id = Uuid::new_v4();

        let run = tokio::spawn({
            let executor = executor.clone();
            async move {
                executor
                    .execute(task_id, Uuid::new_v4(), request("write_file"))
                    .await
            }
        });
        let pending = wait_for_pending(&store, task_id).await;

        let outcome = tokio::time::timeout(StdDuration::from_secs(5), run)
            .await
            .expect("the worker wakes at the deadline")
            .unwrap()
            .unwrap();

        assert!(
            matches!(outcome, ToolCallOutcome::Denied { .. }),
            "an expired approval must be denied, got {outcome:?}"
        );
        assert_eq!(backend.calls(), 0, "the backend must never run");
        assert_eq!(store.tool_calls()[0].status, "expired");

        let approvals = store.approvals();
        assert_eq!(approvals.len(), 1);
        assert_eq!(approvals[0].id, pending.id);
        assert_eq!(approvals[0].status, ApprovalStatus::Expired);
        assert!(
            approvals[0].decided_at.is_none(),
            "expiry is not a human decision"
        );
    }

    /// A decision that lands before the waiter is registered is not lost: the
    /// executor re-reads the row after registration and sees the grant.
    #[tokio::test]
    async fn decision_before_waiter_registration_is_not_lost() {
        let store = Arc::new(FakeStore::with_early_allow());
        let backend = Arc::new(FakeBackend::new());
        let executor = executor(&store, &backend);

        let outcome = tokio::time::timeout(
            StdDuration::from_secs(1),
            executor.execute(Uuid::new_v4(), Uuid::new_v4(), request("write_file")),
        )
        .await
        .expect("an already-decided approval must not block")
        .unwrap();

        assert!(matches!(outcome, ToolCallOutcome::Allowed { .. }));
        assert_eq!(backend.calls(), 1);
        assert_eq!(store.tool_calls()[0].status, "ok");
        assert_eq!(store.approvals()[0].status, ApprovalStatus::Granted);
    }

    /// `decide` only resolves the durable row. With no waiter it must not run
    /// the backend and must not complete the tool call.
    #[tokio::test]
    async fn decide_only_resolves_the_approval() {
        let store = Arc::new(FakeStore::new());
        let backend = Arc::new(FakeBackend::new());
        let executor = executor(&store, &backend);
        let task_id = Uuid::new_v4();
        let call = store
            .record_tool_call(
                task_id,
                Uuid::new_v4(),
                "write_file".to_string(),
                json!({ "path": "/tmp/data" }),
            )
            .await
            .unwrap();
        let decision = store
            .record_guard_decision(
                call.id,
                GuardVerdict::Ask,
                Some("ask-write".to_string()),
                Some("test-v1".to_string()),
                "writes need approval".to_string(),
            )
            .await
            .unwrap();
        let approval = store
            .create_approval(
                task_id,
                call.id,
                decision.id,
                "writes need approval".to_string(),
                Utc::now() + Duration::minutes(5),
            )
            .await
            .unwrap();

        let resolved = executor
            .decide(approval.id, ApprovalDecision::Allow)
            .await
            .unwrap()
            .expect("the approval is pending");

        assert_eq!(resolved.status, ApprovalStatus::Granted);
        assert_eq!(backend.calls(), 0);
        assert_eq!(
            store.tool_calls()[0].status,
            "recorded",
            "decide must leave the call for the waiting worker"
        );
    }

    /// A second decision finds the row terminal and returns `None`.
    #[tokio::test]
    async fn second_decision_returns_none() {
        let store = Arc::new(FakeStore::new());
        let backend = Arc::new(FakeBackend::new());
        let executor = Arc::new(executor(&store, &backend));
        let task_id = Uuid::new_v4();

        let run = tokio::spawn({
            let executor = executor.clone();
            async move {
                executor
                    .execute(task_id, Uuid::new_v4(), request("write_file"))
                    .await
            }
        });
        let pending = wait_for_pending(&store, task_id).await;

        assert!(
            executor
                .decide(pending.id, ApprovalDecision::Allow)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            executor
                .decide(pending.id, ApprovalDecision::Deny)
                .await
                .unwrap()
                .is_none(),
            "only the first decision wins"
        );

        let outcome = tokio::time::timeout(StdDuration::from_secs(5), run)
            .await
            .expect("the waiting worker wakes")
            .unwrap()
            .unwrap();
        assert!(matches!(outcome, ToolCallOutcome::Allowed { .. }));
        assert_eq!(store.approvals()[0].status, ApprovalStatus::Granted);
    }

    /// The supervisor sweep expires a due approval and wakes the worker
    /// blocked on it, without touching an intent.
    #[tokio::test]
    async fn sweep_expired_wakes_waiter() {
        let store = Arc::new(FakeStore::new());
        let backend = Arc::new(FakeBackend::new());
        let executor = Arc::new(executor(&store, &backend));
        let task_id = Uuid::new_v4();

        let run = tokio::spawn({
            let executor = executor.clone();
            async move {
                executor
                    .execute(task_id, Uuid::new_v4(), request("write_file"))
                    .await
            }
        });
        let pending = wait_for_pending(&store, task_id).await;
        store.set_approval_expiry(pending.id, Utc::now() - Duration::seconds(1));

        let swept = executor.sweep_expired(task_id).await.unwrap();
        assert_eq!(swept.len(), 1);
        assert_eq!(swept[0].id, pending.id);
        assert_eq!(swept[0].status, ApprovalStatus::Expired);
        assert!(
            swept[0].decided_at.is_none(),
            "expiry is not a human decision"
        );

        let outcome = tokio::time::timeout(StdDuration::from_secs(5), run)
            .await
            .expect("the sweep wakes the waiting worker")
            .unwrap()
            .unwrap();
        assert!(matches!(outcome, ToolCallOutcome::Denied { .. }));
        assert_eq!(backend.calls(), 0);
        assert_eq!(store.tool_calls()[0].status, "expired");
    }

    #[tokio::test]
    async fn sweep_expires_only_past_due_approvals() {
        let store = Arc::new(FakeStore::new());
        let backend = Arc::new(FakeBackend::new());
        let executor = executor(&store, &backend);
        let task_id = Uuid::new_v4();

        let mut created = Vec::new();
        for expires_at in [
            Utc::now() + Duration::minutes(5),
            Utc::now() - Duration::minutes(1),
        ] {
            let call = store
                .record_tool_call(task_id, Uuid::new_v4(), "write_file".to_string(), json!({}))
                .await
                .unwrap();
            let decision = store
                .record_guard_decision(
                    call.id,
                    GuardVerdict::Ask,
                    None,
                    None,
                    "needs a person".to_string(),
                )
                .await
                .unwrap();
            created.push(
                store
                    .create_approval(
                        task_id,
                        call.id,
                        decision.id,
                        "needs a person".to_string(),
                        expires_at,
                    )
                    .await
                    .unwrap(),
            );
        }

        let swept = executor.sweep_expired(task_id).await.unwrap();
        assert_eq!(swept.len(), 1, "only the overdue approval expires");
        assert_eq!(swept[0].id, created[1].id);
        assert_eq!(
            store
                .get_approval(created[0].id)
                .await
                .unwrap()
                .unwrap()
                .status,
            ApprovalStatus::Pending,
            "a fresh approval must not expire"
        );

        // A second sweep is idempotent: the row is no longer pending.
        assert!(executor.sweep_expired(task_id).await.unwrap().is_empty());
        assert_eq!(backend.calls(), 0, "the backend must never run");
    }

    /// The per-approval timer expires the approval at its deadline and wakes
    /// the worker. No supervisor, no heartbeat, no sweep by hand.
    #[tokio::test]
    async fn approval_expires_at_deadline_without_sweep() {
        let store = Arc::new(FakeStore::new());
        let backend = Arc::new(FakeBackend::new());
        let task_id = Uuid::new_v4();
        let executor = Arc::new(GuardedExecutor::new(
            store.clone(),
            policy(),
            backend.clone(),
            Duration::milliseconds(50),
        ));

        let run = tokio::spawn({
            let executor = executor.clone();
            async move {
                executor
                    .execute(task_id, Uuid::new_v4(), request("write_file"))
                    .await
            }
        });
        let pending = wait_for_pending(&store, task_id).await;

        let outcome = tokio::time::timeout(StdDuration::from_secs(5), run)
            .await
            .expect("the timer must wake the worker at the deadline")
            .unwrap()
            .unwrap();
        assert!(matches!(outcome, ToolCallOutcome::Denied { .. }));

        let approval = store
            .get_approval(pending.id)
            .await
            .unwrap()
            .expect("the approval row exists");
        assert_eq!(
            approval.status,
            ApprovalStatus::Expired,
            "the timer must expire the approval at its deadline"
        );
        assert!(
            approval.decided_at.is_none(),
            "expiry is not a human decision"
        );
        assert_eq!(store.tool_calls()[0].status, "expired");
        assert_eq!(backend.calls(), 0, "the backend must never run");
    }

    /// Two callers racing to block one intent: the move is atomic, so exactly
    /// one of them reports a change. The legacy store helper is unchanged.
    #[tokio::test]
    async fn block_intent_for_approval_is_atomic() {
        let store = Arc::new(FakeStore::new());
        let task_id = Uuid::new_v4();
        let intent = paused_intent(&store, task_id);
        let call = store
            .record_tool_call(task_id, intent.id, "write_file".to_string(), json!({}))
            .await
            .unwrap();
        let decision = store
            .record_guard_decision(
                call.id,
                GuardVerdict::Ask,
                None,
                None,
                "needs a person".to_string(),
            )
            .await
            .unwrap();
        let approval = store
            .create_approval(
                task_id,
                call.id,
                decision.id,
                "needs a person".to_string(),
                Utc::now() - Duration::seconds(1),
            )
            .await
            .unwrap();

        let (first, second) = tokio::join!(
            store.block_intent_for_approval(approval.id, "expiry"),
            store.block_intent_for_approval(approval.id, "supervisor"),
        );
        let first = first.expect("the first block must not error");
        let second = second.expect("the second block must not error");
        assert_eq!(
            usize::from(first) + usize::from(second),
            1,
            "exactly one caller may block the intent (first={first}, second={second})"
        );
        assert_eq!(store.intent(intent.id).unwrap().state, IntentState::Blocked);
    }

    #[tokio::test]
    async fn sweep_is_scoped_to_one_task() {
        let store = Arc::new(FakeStore::new());
        let backend = Arc::new(FakeBackend::new());
        let executor = executor(&store, &backend);
        let task_a = Uuid::new_v4();
        let task_b = Uuid::new_v4();

        let mut created = Vec::new();
        for task_id in [task_a, task_b] {
            let call = store
                .record_tool_call(task_id, Uuid::new_v4(), "write_file".to_string(), json!({}))
                .await
                .unwrap();
            let decision = store
                .record_guard_decision(
                    call.id,
                    GuardVerdict::Ask,
                    None,
                    None,
                    "needs a person".to_string(),
                )
                .await
                .unwrap();
            let approval = store
                .create_approval(
                    task_id,
                    call.id,
                    decision.id,
                    "overdue".to_string(),
                    Utc::now() - Duration::minutes(1),
                )
                .await
                .unwrap();
            created.push(approval);
        }

        // Sweeping one task leaves the other task's overdue approval pending.
        let swept = executor.sweep_expired(task_a).await.unwrap();
        assert_eq!(swept.len(), 1);
        assert_eq!(swept[0].task_id, task_a);
        assert_eq!(
            store
                .get_approval(created[1].id)
                .await
                .unwrap()
                .unwrap()
                .status,
            ApprovalStatus::Pending,
            "a sweep for one task must not expire another task's approval"
        );
    }

    /// A swapped snapshot governs the next evaluation without rebuilding the
    /// executor: hot rebuild is observable between two tool calls.
    #[tokio::test]
    async fn policy_swap_applies_to_the_next_execution() {
        let store = Arc::new(FakeStore::new());
        let backend = Arc::new(FakeBackend::new());
        let shared = SharedPolicy::from_policy(&policy(), 0);
        let executor = GuardedExecutor::with_policy(
            store.clone(),
            shared.clone(),
            backend.clone(),
            Duration::minutes(5),
        );

        let denied = executor
            .execute(Uuid::new_v4(), Uuid::new_v4(), request("shell"))
            .await
            .unwrap();
        assert!(matches!(denied, ToolCallOutcome::Denied { .. }));

        // Hot rebuild: allow `shell` with a 90-second ask budget.
        shared.swap(crate::CompiledPolicy::build(
            "hot-v2",
            crate::GuardVerdict::Deny,
            900,
            &[crate::RuleSpec {
                name: "ask-shell".to_string(),
                match_target: "tool_name".to_string(),
                match_type: "exact".to_string(),
                pattern: "shell".to_string(),
                action: crate::GuardVerdict::Allow,
                reason: "hot rule".to_string(),
                priority: 0,
                timeout_secs: 90,
            }],
        ));

        let allowed = executor
            .execute(Uuid::new_v4(), Uuid::new_v4(), request("shell"))
            .await
            .unwrap();
        assert!(matches!(allowed, ToolCallOutcome::Allowed { .. }));
        let last = store
            .guard_decisions()
            .last()
            .cloned()
            .expect("two decisions recorded");
        assert_eq!(last.matched_rule.as_deref(), Some("ask-shell"));
        assert_eq!(last.rule_version.as_deref(), Some("hot-v2"));
    }
}
