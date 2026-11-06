//! PostgreSQL store for DHI.
//!
//! This crate owns the 24-table schema and the SQL used by the engine and API.
//! Every invariant that can be enforced by a constraint is enforced here.

use chrono::{DateTime, Utc};
use dhi_core::{
    Activity, AgentRole, AgentRun, Approval, ApprovalDecision, ApprovalId, ApprovalStatus,
    CheckpointId, CoreError, Fact, FactState, Goal, GoalState, GuardDecision, GuardVerdict, Intent,
    IntentId, IntentPlan, IntentState, NewSideQuestionRequest, ReopenOutcome, ResultRecord,
    ResultStatus, RunId, SideQuestionRequest, SideQuestionSession, SideQuestionSessionStatus,
    SideQuestionStatus, SideQuestionThread, Task, TaskId, TaskState, ToolCall, ToolCallId,
};
use sqlx::{FromRow, PgPool};
use std::collections::HashMap;
use thiserror::Error;
use uuid::Uuid;

mod graph_impl;
pub mod mgmt;

pub use mgmt::{
    AppSettingsRow, BootstrapSeed, GuardRuleInput, GuardRuleRow, LlmProfileFull, LlmProfileOut,
    SettingPatch, WebProviderSettingFull, WebProviderSettingRow, WebProviderSettingUpsert,
};

#[derive(Debug, Error)]
pub enum StoreError {
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error(transparent)]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error(transparent)]
    Core(#[from] dhi_core::CoreError),
    #[error("invalid stored value: {0}")]
    InvalidValue(String),
    #[error("proof rejected: {0}")]
    ProofRejected(String),
    /// A management row (profile, rule, setting) does not exist. Maps to 404.
    #[error("not found: {0}")]
    NotFound(String),
    /// A management row is still referenced (active default or task pin).
    /// Maps to 409.
    #[error("in use: {0}")]
    InUse(String),
    /// A management write failed request validation. Maps to 400.
    #[error("{0}")]
    Validation(String),
}

#[derive(Clone)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn connect(dsn: &str) -> Result<Self, StoreError> {
        let pool = PgPool::connect(dsn).await?;
        Ok(Self { pool })
    }

    pub async fn migrate(&self) -> Result<(), StoreError> {
        sqlx::migrate!("./migrations").run(&self.pool).await?;
        Ok(())
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn create_task(&self, goal: &str, bound: i32) -> Result<Task, StoreError> {
        self.create_task_with_pins(goal, bound, None, None).await
    }

    /// Pins are validated by the caller (`profile_exists`); the SQL foreign
    /// keys enforce the same invariant.
    pub async fn create_task_with_pins(
        &self,
        goal: &str,
        bound: i32,
        planner_pin: Option<i64>,
        worker_pin: Option<i64>,
    ) -> Result<Task, StoreError> {
        let mut task = Task::new(goal, bound);
        // The returned task must describe the persisted row, pins included.
        task.pinned_planner_profile_id = planner_pin;
        task.pinned_worker_profile_id = worker_pin;
        let origin = task.origin_fact();
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "INSERT INTO tasks (id, goal, bound, state, exploration_id, origin_fact_id, \
             pinned_planner_profile_id, pinned_worker_profile_id, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(task.id)
        .bind(&task.goal)
        .bind(task.bound)
        .bind(task.state.as_str())
        .bind(task.exploration_id)
        .bind(task.origin_fact_id)
        .bind(planner_pin)
        .bind(worker_pin)
        .bind(task.created_at)
        .bind(task.updated_at)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO facts (id, task_id, claim, state, provenance, origin_of, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(origin.id)
        .bind(origin.task_id)
        .bind(&origin.claim)
        .bind(origin.state.as_str())
        .bind(&origin.provenance)
        .bind(origin.origin_of)
        .bind(origin.created_at)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO activity (task_id, kind, message, payload)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(task.id)
        .bind("task_created")
        .bind(format!("Task created: {}", task.goal))
        .bind(serde_json::json!({"bound": task.bound}))
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(task)
    }

    pub async fn get_task(&self, id: Uuid) -> Result<Option<Task>, StoreError> {
        let row = sqlx::query_as::<_, TaskRow>(
            "SELECT id, goal, bound, state, exploration_id, origin_fact_id, \
                    pinned_planner_profile_id, pinned_worker_profile_id, created_at, updated_at
             FROM tasks WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(Task::try_from).transpose()
    }

    pub async fn list_tasks(&self, limit: i64) -> Result<Vec<Task>, StoreError> {
        let rows = sqlx::query_as::<_, TaskRow>(
            "SELECT id, goal, bound, state, exploration_id, origin_fact_id, \
                    pinned_planner_profile_id, pinned_worker_profile_id, created_at, updated_at
             FROM tasks ORDER BY created_at DESC LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(Task::try_from).collect()
    }

    /// First terminal status wins. Once the stored state is `done`,
    /// `failed`, `timeout`, or `cancelled`, no write applies — not even a
    /// non-terminal target. This is the only public way to write a task
    /// state.
    ///
    /// Returns whether a row changed.
    pub async fn set_task_state_guarded(
        &self,
        id: Uuid,
        state: TaskState,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "UPDATE tasks SET state = $2, updated_at = now()
             WHERE id = $1
               AND state NOT IN ('done', 'failed', 'timeout', 'cancelled')",
        )
        .bind(id)
        .bind(state.as_str())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Deletes one terminal task and every task-owned row that cascades from
    /// it. A non-terminal or unknown task is left untouched and returns
    /// `false`. The API refuses a task whose loop is still registered, so a
    /// delete never runs under a live writer.
    pub async fn delete_task(&self, id: TaskId) -> Result<bool, StoreError> {
        let mut tx = self.pool.begin().await?;
        let state: Option<String> =
            sqlx::query_scalar("SELECT state FROM tasks WHERE id = $1 FOR UPDATE")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some(state) = state else {
            tx.rollback().await?;
            return Ok(false);
        };
        if !matches!(state.as_str(), "done" | "failed" | "timeout" | "cancelled") {
            tx.rollback().await?;
            return Ok(false);
        }
        // Remove intents first: `intents.fact_id` restricts deleting facts,
        // while the other task-owned rows cascade from `tasks`. Doing this in
        // one transaction keeps a failed delete from leaving a half task.
        sqlx::query("DELETE FROM intents WHERE task_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM tasks WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Deletes every terminal task in `ids`, returning the ids that were
    /// actually removed. Unknown and non-terminal ids are ignored
    /// independently; a caller that needs per-id diagnostics checks them
    /// before calling.
    pub async fn delete_tasks(&self, ids: &[TaskId]) -> Result<Vec<TaskId>, StoreError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut tx = self.pool.begin().await?;
        let deletable: Vec<TaskId> = sqlx::query_scalar(
            "SELECT id FROM tasks
             WHERE id = ANY($1)
               AND state IN ('done', 'failed', 'timeout', 'cancelled')
             FOR UPDATE",
        )
        .bind(ids)
        .fetch_all(&mut *tx)
        .await?;
        if deletable.is_empty() {
            tx.rollback().await?;
            return Ok(Vec::new());
        }
        // The row locks above keep an add-goal reopen from racing this
        // delete. Intents go first because their fact foreign key restricts
        // deleting facts; every other task-owned row cascades from `tasks`.
        sqlx::query("DELETE FROM intents WHERE task_id = ANY($1)")
            .bind(&deletable)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM tasks WHERE id = ANY($1)")
            .bind(&deletable)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(deletable)
    }

    pub async fn add_goal(
        &self,
        task_id: Uuid,
        description: impl Into<String>,
    ) -> Result<Goal, StoreError> {
        let goal = Goal {
            id: Uuid::new_v4(),
            task_id,
            description: description.into(),
            state: GoalState::Open,
            proof_fact_id: None,
            created_at: Utc::now(),
        };
        sqlx::query(
            "INSERT INTO work_goals (id, task_id, description, state, proof_fact_id, created_at)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(goal.id)
        .bind(goal.task_id)
        .bind(&goal.description)
        .bind(goal.state.as_str())
        .bind(goal.proof_fact_id)
        .bind(goal.created_at)
        .execute(&self.pool)
        .await?;
        Ok(goal)
    }

    pub async fn list_goals(&self, task_id: Uuid) -> Result<Vec<Goal>, StoreError> {
        let rows = sqlx::query_as::<_, GoalRow>(
            "SELECT id, task_id, description, state, proof_fact_id, created_at
             FROM work_goals WHERE task_id = $1 ORDER BY created_at",
        )
        .bind(task_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(Goal::try_from).collect()
    }

    /// Adds one open goal to a terminal task and reopens it, in one
    /// transaction. See [`dhi_core::GraphStore::reopen_task_with_goal`].
    ///
    /// The task row is locked first, so two concurrent calls cannot both
    /// observe a terminal state: the winner reopens, the loser sees `running`
    /// and gets `NotTerminal`. `set_task_state_guarded` keeps refusing every
    /// terminal write; this is the deliberate carve-out.
    pub async fn reopen_task_with_goal(
        &self,
        task_id: Uuid,
        description: impl Into<String>,
    ) -> Result<ReopenOutcome, StoreError> {
        let mut tx = self.pool.begin().await?;

        let row = sqlx::query_as::<_, TaskRow>(
            "SELECT id, goal, bound, state, exploration_id, origin_fact_id, \
                    pinned_planner_profile_id, pinned_worker_profile_id, created_at, updated_at
             FROM tasks WHERE id = $1 FOR UPDATE",
        )
        .bind(task_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            tx.rollback().await?;
            return Ok(ReopenOutcome::NotFound);
        };
        let previous_state = Task::try_from(row)?.state;
        if !dhi_followups::can_reopen(previous_state) {
            tx.rollback().await?;
            return Ok(ReopenOutcome::NotTerminal(previous_state));
        }

        let goal = Goal {
            id: Uuid::new_v4(),
            task_id,
            description: description.into(),
            state: GoalState::Open,
            proof_fact_id: None,
            created_at: Utc::now(),
        };
        sqlx::query(
            "INSERT INTO work_goals (id, task_id, description, state, proof_fact_id, created_at)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(goal.id)
        .bind(goal.task_id)
        .bind(&goal.description)
        .bind(goal.state.as_str())
        .bind(goal.proof_fact_id)
        .bind(goal.created_at)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO activity (task_id, kind, message, payload)
             VALUES ($1, 'goal_added', $2, $3)",
        )
        .bind(task_id)
        .bind(format!("human added goal: {}", goal.description))
        .bind(serde_json::json!({"goal_id": goal.id, "actor": "human"}))
        .execute(&mut *tx)
        .await?;

        let row = sqlx::query_as::<_, TaskRow>(
            "UPDATE tasks SET state = 'running', updated_at = now()
             WHERE id = $1
             RETURNING id, goal, bound, state, exploration_id, origin_fact_id, \
                       pinned_planner_profile_id, pinned_worker_profile_id, created_at, updated_at",
        )
        .bind(task_id)
        .fetch_one(&mut *tx)
        .await?;
        let task = Task::try_from(row)?;

        sqlx::query(
            "INSERT INTO activity (task_id, kind, message, payload)
             VALUES ($1, 'task_reopened', $2, $3)",
        )
        .bind(task_id)
        .bind("task reopened by human: goal added")
        .bind(serde_json::json!({
            "goal_id": goal.id,
            "previous_state": previous_state,
        }))
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(ReopenOutcome::Reopened { task, goal })
    }

    /// Proves one goal with a confirmed fact from the same task, in one
    /// transaction. See [`dhi_core::GraphStore::prove_goal`].
    pub async fn prove_goal(
        &self,
        task_id: Uuid,
        goal_id: Uuid,
        fact_id: Uuid,
        reason: impl Into<String>,
    ) -> Result<bool, StoreError> {
        let reason = reason.into();
        let mut tx = self.pool.begin().await?;

        let fact_row: Option<(String, chrono::DateTime<Utc>)> = sqlx::query_as(
            "SELECT state, created_at FROM facts WHERE id = $1 AND task_id = $2 FOR UPDATE",
        )
        .bind(fact_id)
        .bind(task_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((fact_state, fact_created_at)) = fact_row else {
            tx.rollback().await?;
            return Err(StoreError::ProofRejected(format!(
                "fact {fact_id} does not belong to task {task_id}"
            )));
        };
        if fact_state != FactState::Confirmed.as_str() {
            tx.rollback().await?;
            return Err(StoreError::ProofRejected(format!(
                "fact {fact_id} is {fact_state}, not confirmed"
            )));
        }

        let goal_row: Option<(String, chrono::DateTime<Utc>)> = sqlx::query_as(
            "SELECT state, created_at FROM work_goals WHERE id = $1 AND task_id = $2 FOR UPDATE",
        )
        .bind(goal_id)
        .bind(task_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((goal_state, goal_created_at)) = goal_row else {
            tx.rollback().await?;
            return Err(StoreError::ProofRejected(format!(
                "goal {goal_id} does not belong to task {task_id}"
            )));
        };
        if goal_state == GoalState::Abandoned.as_str() {
            tx.rollback().await?;
            return Err(StoreError::ProofRejected(format!(
                "goal {goal_id} is abandoned"
            )));
        }
        if !dhi_followups::fresh_evidence(fact_created_at, goal_created_at) {
            tx.rollback().await?;
            return Err(StoreError::ProofRejected(format!(
                "fact {fact_id} was recorded before goal {goal_id}; a goal needs fresh evidence"
            )));
        }

        sqlx::query(
            "UPDATE work_goals
             SET state = 'met', proof_fact_id = $2
             WHERE id = $1 AND task_id = $3",
        )
        .bind(goal_id)
        .bind(fact_id)
        .bind(task_id)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO activity (task_id, kind, message, payload)
             VALUES ($1, 'goal_proven', $2, $3)",
        )
        .bind(task_id)
        .bind(format!("goal {goal_id} proven by fact {fact_id}"))
        .bind(serde_json::json!({
            "goal_id": goal_id,
            "fact_id": fact_id,
            "reason": reason,
        }))
        .execute(&mut *tx)
        .await?;

        let unmet: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM work_goals WHERE task_id = $1 AND state <> 'met'",
        )
        .bind(task_id)
        .fetch_one(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(unmet == 0)
    }

    pub async fn add_intent(
        &self,
        task_id: Uuid,
        fact_id: Uuid,
        description: impl Into<String>,
    ) -> Result<Intent, StoreError> {
        let intent = Intent::new(task_id, fact_id, description)?;
        sqlx::query(
            "INSERT INTO intents (id, task_id, goal_id, fact_id, description, state, owner, priority, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(intent.id)
        .bind(intent.task_id)
        .bind(intent.goal_id)
        .bind(intent.fact_id)
        .bind(&intent.description)
        .bind(intent.state.as_str())
        .bind(&intent.owner)
        .bind(intent.priority)
        .bind(intent.created_at)
        .execute(&self.pool)
        .await?;
        Ok(intent)
    }

    pub async fn list_open_intents(
        &self,
        task_id: Uuid,
        limit: i64,
    ) -> Result<Vec<Intent>, StoreError> {
        let rows = sqlx::query_as::<_, IntentRow>(
            "SELECT id, task_id, goal_id, fact_id, description, state, owner, priority, created_at, completed_at
             FROM intents
             WHERE task_id = $1 AND state = 'open'
             ORDER BY priority DESC, created_at
             LIMIT $2",
        )
        .bind(task_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(Intent::try_from).collect()
    }

    /// Every intent of `task_id` in any state, oldest first, at most `limit`
    /// rows. `created_at` is not unique, so `id` breaks ties into a stable
    /// order.
    pub async fn list_intents(&self, task_id: Uuid, limit: i64) -> Result<Vec<Intent>, StoreError> {
        let rows = sqlx::query_as::<_, IntentRow>(
            "SELECT id, task_id, goal_id, fact_id, description, state, owner, priority, created_at, completed_at
             FROM intents
             WHERE task_id = $1
             ORDER BY created_at, id
             LIMIT $2",
        )
        .bind(task_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(Intent::try_from).collect()
    }

    /// Upserts the DHI+ plan attached to `intent_id`.
    ///
    /// The plan row is audit data: it stays after the intent completes and is
    /// only removed with the intent (or its task).
    pub async fn save_intent_plan(
        &self,
        intent_id: IntentId,
        plan: &IntentPlan,
    ) -> Result<(), StoreError> {
        let plan = serde_json::to_value(plan)
            .map_err(|err| StoreError::InvalidValue(format!("intent plan: {err}")))?;
        sqlx::query(
            "INSERT INTO intent_plans (intent_id, plan)
             VALUES ($1, $2)
             ON CONFLICT (intent_id) DO UPDATE SET plan = EXCLUDED.plan",
        )
        .bind(intent_id)
        .bind(plan)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The DHI+ plan attached to `intent_id`. `None` when the intent has no
    /// plan row, including an unknown intent.
    pub async fn get_intent_plan(
        &self,
        intent_id: IntentId,
    ) -> Result<Option<IntentPlan>, StoreError> {
        let plan: Option<serde_json::Value> =
            sqlx::query_scalar("SELECT plan FROM intent_plans WHERE intent_id = $1")
                .bind(intent_id)
                .fetch_optional(&self.pool)
                .await?;
        plan.map(|plan| {
            serde_json::from_value(plan)
                .map_err(|err| StoreError::InvalidValue(format!("intent plan: {err}")))
        })
        .transpose()
    }

    /// Atomic claim. Only one worker can move an intent from open to running.
    pub async fn claim_intent(
        &self,
        intent_id: Uuid,
        owner: &str,
    ) -> Result<Option<Intent>, StoreError> {
        let row = sqlx::query_as::<_, IntentRow>(
            "UPDATE intents
             SET state = 'running', owner = $2
             WHERE id = $1 AND state = 'open'
             RETURNING id, task_id, goal_id, fact_id, description, state, owner, priority, created_at, completed_at",
        )
        .bind(intent_id)
        .bind(owner)
        .fetch_optional(&self.pool)
        .await?;
        row.map(Intent::try_from).transpose()
    }

    /// Unparks a paused intent back to `running`. Only `paused` intents move,
    /// so a resume can never steal another worker's claim.
    pub async fn resume_intent(
        &self,
        intent_id: Uuid,
        owner: &str,
    ) -> Result<Option<Intent>, StoreError> {
        let row = sqlx::query_as::<_, IntentRow>(
            "UPDATE intents
             SET state = 'running', owner = $2
             WHERE id = $1 AND state = 'paused'
             RETURNING id, task_id, goal_id, fact_id, description, state, owner, priority, created_at, completed_at",
        )
        .bind(intent_id)
        .bind(owner)
        .fetch_optional(&self.pool)
        .await?;
        row.map(Intent::try_from).transpose()
    }

    /// Finishes a running intent. A paused intent cannot finish directly: it
    /// is left untouched and must be resumed first.
    pub async fn finish_intent(
        &self,
        intent_id: Uuid,
        state: IntentState,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE intents SET state = $2, completed_at = now()
             WHERE id = $1 AND state = 'running'",
        )
        .bind(intent_id)
        .bind(state.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Resumes and finishes a paused intent in one transaction.
    ///
    /// The transaction resumes exactly one `paused` row (`paused -> running`,
    /// with the owner), and only a row that actually resumed is finished
    /// (`running -> status`). Two concurrent callers cannot both resume the
    /// row, so exactly one gets the final intent back and the loser gets
    /// `None`. The row is never left `running` outside the transaction.
    pub async fn finish_paused_intent(
        &self,
        intent_id: Uuid,
        owner: &str,
        status: IntentState,
    ) -> Result<Option<Intent>, StoreError> {
        let mut tx = self.pool.begin().await?;
        let resumed: Option<(Uuid,)> = sqlx::query_as(
            "UPDATE intents
             SET state = 'running', owner = $2
             WHERE id = $1 AND state = 'paused'
             RETURNING id",
        )
        .bind(intent_id)
        .bind(owner)
        .fetch_optional(&mut *tx)
        .await?;

        let Some((id,)) = resumed else {
            tx.commit().await?;
            return Ok(None);
        };

        let row = sqlx::query_as::<_, IntentRow>(
            "UPDATE intents
             SET state = $2, completed_at = now()
             WHERE id = $1 AND state = 'running'
             RETURNING id, task_id, goal_id, fact_id, description, state, owner, priority,
                       created_at, completed_at",
        )
        .bind(id)
        .bind(status.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        row.map(Intent::try_from).transpose()
    }

    /// Blocks every paused intent of `task_id` whose tool call has no pending
    /// approval. Each one moves `paused -> running -> blocked` in one
    /// transaction. Returns how many intents were blocked.
    pub async fn block_orphaned_paused_intents(&self, task_id: Uuid) -> Result<usize, StoreError> {
        let mut tx = self.pool.begin().await?;
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "UPDATE intents i
             SET state = 'running'
             WHERE i.task_id = $1
               AND i.state = 'paused'
               AND EXISTS (
                   SELECT 1 FROM tool_calls tc WHERE tc.intent_id = i.id
               )
               AND NOT EXISTS (
                   SELECT 1
                   FROM tool_calls tc
                   JOIN approvals a ON a.tool_call_id = tc.id
                   WHERE tc.intent_id = i.id AND a.status = 'pending'
               )
             RETURNING i.id",
        )
        .bind(task_id)
        .fetch_all(&mut *tx)
        .await?;

        if ids.is_empty() {
            tx.commit().await?;
            return Ok(0);
        }

        let result = sqlx::query(
            "UPDATE intents
             SET state = 'blocked', completed_at = now()
             WHERE id = ANY($1) AND state = 'running'",
        )
        .bind(&ids)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result.rows_affected() as usize)
    }

    /// Restart recovery: releases every `running` intent back to `open` with
    /// no owner. Returns how many rows were reset.
    pub async fn reset_running_intents(&self) -> Result<usize, StoreError> {
        let result =
            sqlx::query("UPDATE intents SET state = 'open', owner = NULL WHERE state = 'running'")
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() as usize)
    }

    pub async fn record_fact(
        &self,
        task_id: Uuid,
        claim: impl Into<String>,
        provenance: serde_json::Value,
    ) -> Result<Fact, StoreError> {
        let fact = Fact {
            id: Uuid::new_v4(),
            task_id,
            claim: claim.into(),
            state: FactState::Confirmed,
            provenance,
            origin_of: None,
            created_at: Utc::now(),
        };
        sqlx::query(
            "INSERT INTO facts (id, task_id, claim, state, provenance, origin_of, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(fact.id)
        .bind(fact.task_id)
        .bind(&fact.claim)
        .bind(fact.state.as_str())
        .bind(&fact.provenance)
        .bind(fact.origin_of)
        .bind(fact.created_at)
        .execute(&self.pool)
        .await?;
        Ok(fact)
    }

    /// Records one confirmed fact bound to `intent_id`.
    ///
    /// `facts_one_per_intent` rejects a second fact for the same intent; the
    /// caller sees [`CoreError::FactAlreadyRecordedForIntent`]. A missing
    /// intent is rejected by the foreign key.
    pub async fn record_fact_for_intent(
        &self,
        task_id: Uuid,
        intent_id: Uuid,
        claim: impl Into<String>,
        provenance: serde_json::Value,
    ) -> Result<Fact, StoreError> {
        let fact = Fact {
            id: Uuid::new_v4(),
            task_id,
            claim: claim.into(),
            state: FactState::Confirmed,
            provenance,
            origin_of: None,
            created_at: Utc::now(),
        };
        let inserted = sqlx::query(
            "INSERT INTO facts (id, task_id, intent_id, claim, state, provenance, origin_of, \
             created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(fact.id)
        .bind(fact.task_id)
        .bind(intent_id)
        .bind(&fact.claim)
        .bind(fact.state.as_str())
        .bind(&fact.provenance)
        .bind(fact.origin_of)
        .bind(fact.created_at)
        .execute(&self.pool)
        .await;

        match inserted {
            Ok(_) => Ok(fact),
            Err(sqlx::Error::Database(err)) if err.is_unique_violation() => Err(StoreError::Core(
                CoreError::FactAlreadyRecordedForIntent(intent_id),
            )),
            Err(err) => Err(StoreError::Sqlx(err)),
        }
    }

    pub async fn list_facts(&self, task_id: Uuid) -> Result<Vec<Fact>, StoreError> {
        let rows = sqlx::query_as::<_, FactRow>(
            "SELECT id, task_id, claim, state, provenance, origin_of, created_at
             FROM facts WHERE task_id = $1 ORDER BY created_at",
        )
        .bind(task_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(Fact::try_from).collect()
    }

    pub async fn record_result(
        &self,
        task_id: Uuid,
        intent_id: Uuid,
        claim: impl Into<String>,
        acceptance: ResultStatus,
        evidence: serde_json::Value,
    ) -> Result<ResultRecord, StoreError> {
        let result = ResultRecord {
            id: Uuid::new_v4(),
            task_id,
            intent_id,
            claim: claim.into(),
            acceptance,
            evidence,
            created_at: Utc::now(),
        };
        sqlx::query(
            "INSERT INTO results (id, task_id, intent_id, claim, acceptance, evidence, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(result.id)
        .bind(result.task_id)
        .bind(result.intent_id)
        .bind(&result.claim)
        .bind(result.acceptance.as_str())
        .bind(&result.evidence)
        .bind(result.created_at)
        .execute(&self.pool)
        .await?;
        Ok(result)
    }

    /// Every result of `task_id`, oldest first, at most `limit` rows.
    pub async fn list_results(
        &self,
        task_id: Uuid,
        limit: i64,
    ) -> Result<Vec<ResultRecord>, StoreError> {
        let rows = sqlx::query_as::<_, ResultRow>(
            "SELECT id, task_id, intent_id, claim, acceptance, evidence, created_at
             FROM results
             WHERE task_id = $1
             ORDER BY created_at, id
             LIMIT $2",
        )
        .bind(task_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(ResultRecord::try_from).collect()
    }

    pub async fn append_activity(
        &self,
        task_id: Uuid,
        kind: &str,
        message: impl Into<String>,
        payload: serde_json::Value,
    ) -> Result<Activity, StoreError> {
        let row = sqlx::query_as::<_, ActivityRow>(
            "INSERT INTO activity (task_id, kind, message, payload)
             VALUES ($1, $2, $3, $4)
             RETURNING id, task_id, kind, message, payload, created_at",
        )
        .bind(task_id)
        .bind(kind)
        .bind(message.into())
        .bind(payload)
        .fetch_one(&self.pool)
        .await?;
        Ok(Activity::from(row))
    }

    pub async fn list_activity(
        &self,
        task_id: Uuid,
        kind: Option<&str>,
    ) -> Result<Vec<Activity>, StoreError> {
        let rows = sqlx::query_as::<_, ActivityRow>(
            "SELECT id, task_id, kind, message, payload, created_at
             FROM activity
             WHERE task_id = $1 AND ($2::text IS NULL OR kind = $2)
             ORDER BY id",
        )
        .bind(task_id)
        .bind(kind)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Activity::from).collect())
    }

    pub async fn record_tool_call(
        &self,
        task_id: Uuid,
        intent_id: IntentId,
        tool_name: &str,
        input: serde_json::Value,
    ) -> Result<ToolCall, StoreError> {
        let call = ToolCall {
            id: Uuid::new_v4(),
            task_id,
            intent_id,
            tool_name: tool_name.to_string(),
            input,
            output: None,
            status: "pending".to_string(),
            created_at: Utc::now(),
        };
        sqlx::query(
            "INSERT INTO tool_calls (id, task_id, intent_id, tool_name, input, output, status, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(call.id)
        .bind(call.task_id)
        .bind(call.intent_id)
        .bind(&call.tool_name)
        .bind(&call.input)
        .bind(&call.output)
        .bind(&call.status)
        .bind(call.created_at)
        .execute(&self.pool)
        .await?;
        Ok(call)
    }

    pub async fn record_guard_decision(
        &self,
        tool_call_id: Uuid,
        verdict: GuardVerdict,
        matched_rule: Option<String>,
        rule_version: Option<String>,
        reason: impl Into<String>,
    ) -> Result<GuardDecision, StoreError> {
        let decision = GuardDecision {
            id: Uuid::new_v4(),
            tool_call_id,
            verdict,
            matched_rule,
            rule_version,
            reason: reason.into(),
            created_at: Utc::now(),
        };
        sqlx::query(
            "INSERT INTO guard_decisions (id, tool_call_id, verdict, matched_rule, rule_version, reason, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(decision.id)
        .bind(decision.tool_call_id)
        .bind(decision.verdict.as_str())
        .bind(&decision.matched_rule)
        .bind(&decision.rule_version)
        .bind(&decision.reason)
        .bind(decision.created_at)
        .execute(&self.pool)
        .await?;
        Ok(decision)
    }

    pub async fn create_approval(
        &self,
        task_id: Uuid,
        tool_call_id: Uuid,
        guard_decision_id: Uuid,
        reason: impl Into<String>,
        expires_at: DateTime<Utc>,
    ) -> Result<Approval, StoreError> {
        let approval = Approval {
            id: Uuid::new_v4(),
            task_id,
            tool_call_id,
            guard_decision_id,
            status: ApprovalStatus::Pending,
            reason: reason.into(),
            expires_at,
            decided_at: None,
            created_at: Utc::now(),
        };
        sqlx::query(
            "INSERT INTO approvals (id, task_id, tool_call_id, guard_decision_id, status, reason, expires_at, decided_at, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(approval.id)
        .bind(approval.task_id)
        .bind(approval.tool_call_id)
        .bind(approval.guard_decision_id)
        .bind(approval.status.as_str())
        .bind(&approval.reason)
        .bind(approval.expires_at)
        .bind(approval.decided_at)
        .bind(approval.created_at)
        .execute(&self.pool)
        .await?;
        Ok(approval)
    }

    pub async fn get_tool_call(
        &self,
        tool_call_id: ToolCallId,
    ) -> Result<Option<ToolCall>, StoreError> {
        let row = sqlx::query_as::<_, ToolCallRow>(
            "SELECT id, task_id, intent_id, tool_name, input, output, status, created_at
             FROM tool_calls WHERE id = $1",
        )
        .bind(tool_call_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(ToolCall::from))
    }

    pub async fn complete_tool_call(
        &self,
        tool_call_id: ToolCallId,
        output: Option<serde_json::Value>,
        status: &str,
    ) -> Result<(), StoreError> {
        sqlx::query("UPDATE tool_calls SET output = $2, status = $3 WHERE id = $1")
            .bind(tool_call_id)
            .bind(output)
            .bind(status)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn get_approval(
        &self,
        approval_id: ApprovalId,
    ) -> Result<Option<Approval>, StoreError> {
        let row = sqlx::query_as::<_, ApprovalRow>(
            "SELECT id, task_id, tool_call_id, guard_decision_id, status, reason, expires_at,
                    decided_at, created_at
             FROM approvals WHERE id = $1",
        )
        .bind(approval_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(Approval::try_from).transpose()
    }

    pub async fn list_pending_approvals(&self, task_id: Uuid) -> Result<Vec<Approval>, StoreError> {
        let rows = sqlx::query_as::<_, ApprovalRow>(
            "SELECT id, task_id, tool_call_id, guard_decision_id, status, reason, expires_at,
                    decided_at, created_at
             FROM approvals
             WHERE task_id = $1 AND status = 'pending'
             ORDER BY created_at",
        )
        .bind(task_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(Approval::try_from).collect()
    }

    /// Every pending approval of every task, oldest first. `created_at` is
    /// not unique, so `id` breaks ties into a stable order.
    pub async fn list_pending_approvals_all(&self) -> Result<Vec<Approval>, StoreError> {
        let rows = sqlx::query_as::<_, ApprovalRow>(
            "SELECT id, task_id, tool_call_id, guard_decision_id, status, reason, expires_at,
                    decided_at, created_at
             FROM approvals
             WHERE status = 'pending'
             ORDER BY created_at, id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(Approval::try_from).collect()
    }

    pub async fn list_expired_approvals(&self, task_id: Uuid) -> Result<Vec<Approval>, StoreError> {
        let rows = sqlx::query_as::<_, ApprovalRow>(
            "SELECT id, task_id, tool_call_id, guard_decision_id, status, reason, expires_at,
                    decided_at, created_at
             FROM approvals
             WHERE task_id = $1 AND status = 'expired'
             ORDER BY created_at",
        )
        .bind(task_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(Approval::try_from).collect()
    }

    /// Marks every pending approval of one task past `expires_at` as
    /// `expired` and returns the rows it changed. Expiry is not a human
    /// decision, so `decided_at` stays null.
    ///
    /// The candidate rows are locked in one transaction before the update, so
    /// a concurrent `decide_approval` and an expiry cannot both win on the
    /// same row, and `decided_at` is never corrupted.
    pub async fn expire_pending_approvals(
        &self,
        task_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<Vec<Approval>, StoreError> {
        let mut tx = self.pool.begin().await?;
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM approvals
             WHERE task_id = $1 AND status = 'pending' AND expires_at < $2
             FOR UPDATE",
        )
        .bind(task_id)
        .bind(now)
        .fetch_all(&mut *tx)
        .await?;

        if ids.is_empty() {
            tx.commit().await?;
            return Ok(Vec::new());
        }

        let rows = sqlx::query_as::<_, ApprovalRow>(
            "UPDATE approvals
             SET status = 'expired', decided_at = NULL
             WHERE id = ANY($1)
             RETURNING id, task_id, tool_call_id, guard_decision_id, status, reason, expires_at,
                       decided_at, created_at",
        )
        .bind(&ids)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        rows.into_iter().map(Approval::try_from).collect()
    }

    /// Blocks the intent behind an approval, `paused -> running -> blocked`,
    /// in one transaction with the given owner.
    ///
    /// Returns whether the intent moved: `false` when the approval, its tool
    /// call, or the intent is missing, or the intent was not paused.
    pub async fn block_intent_for_approval(
        &self,
        approval_id: ApprovalId,
        owner: &str,
    ) -> Result<bool, StoreError> {
        let mut tx = self.pool.begin().await?;
        let resumed: Option<(Uuid,)> = sqlx::query_as(
            "UPDATE intents
             SET state = 'running', owner = $2
             WHERE id = (
                 SELECT tc.intent_id
                 FROM approvals a
                 JOIN tool_calls tc ON tc.id = a.tool_call_id
                 WHERE a.id = $1
             )
               AND state = 'paused'
             RETURNING id",
        )
        .bind(approval_id)
        .bind(owner)
        .fetch_optional(&mut *tx)
        .await?;

        let Some((intent_id,)) = resumed else {
            tx.commit().await?;
            return Ok(false);
        };

        let result = sqlx::query(
            "UPDATE intents
             SET state = 'blocked', completed_at = now()
             WHERE id = $1 AND state = 'running'",
        )
        .bind(intent_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result.rows_affected() > 0)
    }

    /// Only a pending approval can move to `granted`, `denied`, or `expired`.
    ///
    /// The row is locked for the duration of the transaction so two concurrent
    /// decisions cannot both observe `pending` and both write a decision.
    pub async fn decide_approval(
        &self,
        approval_id: ApprovalId,
        decision: ApprovalDecision,
    ) -> Result<Option<Approval>, StoreError> {
        let mut tx = self.pool.begin().await?;
        let existing = sqlx::query_as::<_, ApprovalRow>(
            "SELECT id, task_id, tool_call_id, guard_decision_id, status, reason, expires_at,
                    decided_at, created_at
             FROM approvals WHERE id = $1 FOR UPDATE",
        )
        .bind(approval_id)
        .fetch_optional(&mut *tx)
        .await?;

        let Some(existing) = existing else {
            tx.rollback().await?;
            return Ok(None);
        };
        if existing.status != ApprovalStatus::Pending.as_str() {
            tx.rollback().await?;
            return Ok(None);
        }

        let expired = existing.expires_at < Utc::now();
        let status = if expired {
            ApprovalStatus::Expired
        } else {
            match decision {
                ApprovalDecision::Allow => ApprovalStatus::Granted,
                ApprovalDecision::Deny => ApprovalStatus::Denied,
            }
        };

        let row = sqlx::query_as::<_, ApprovalRow>(
            "UPDATE approvals
             SET status = $2,
                 decided_at = CASE WHEN $2 = 'expired' THEN NULL ELSE now() END
             WHERE id = $1
             RETURNING id, task_id, tool_call_id, guard_decision_id, status, reason, expires_at,
                       decided_at, created_at",
        )
        .bind(approval_id)
        .bind(status.as_str())
        .fetch_one(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(Some(Approval::try_from(row)?))
    }

    /// Upserts one `agent_runs` row (one per completion call).
    ///
    /// Kept as the `ON CONFLICT (id) DO UPDATE` writer so an existing caller
    /// cannot silently break; `insert_agent_run` / `finish_agent_run` will
    /// wrap it when the runtime needs a "running" row before the HTTP call.
    pub async fn save_agent_run(&self, run: &AgentRun) -> Result<(), StoreError> {
        parse_agent_run_status(&run.status)?;
        sqlx::query(
            "INSERT INTO agent_runs
             (id, run_id, call_index, task_id, intent_id, role, model, profile, prompt, prompt_hash,
              variables, response, thinking, tokens_in, tokens_out, cache_read_tokens,
              cache_write_tokens, reasoning_tokens, cost, status, error, latency_ms, checkpoint_id,
              created_at, updated_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24,$25)
             ON CONFLICT (id) DO UPDATE SET
               run_id = EXCLUDED.run_id,
               call_index = EXCLUDED.call_index,
               profile = EXCLUDED.profile,
               prompt_hash = EXCLUDED.prompt_hash,
               variables = EXCLUDED.variables,
               response = EXCLUDED.response,
               thinking = EXCLUDED.thinking,
               tokens_in = EXCLUDED.tokens_in,
               tokens_out = EXCLUDED.tokens_out,
               cache_read_tokens = EXCLUDED.cache_read_tokens,
               cache_write_tokens = EXCLUDED.cache_write_tokens,
               reasoning_tokens = EXCLUDED.reasoning_tokens,
               cost = EXCLUDED.cost,
               status = EXCLUDED.status,
               error = EXCLUDED.error,
               latency_ms = EXCLUDED.latency_ms,
               checkpoint_id = EXCLUDED.checkpoint_id,
               updated_at = now()",
        )
        .bind(run.id)
        .bind(run.run_id)
        .bind(i32::try_from(run.call_index).unwrap_or(i32::MAX))
        .bind(run.task_id)
        .bind(run.intent_id)
        .bind(run.role.as_str())
        .bind(&run.model)
        .bind(&run.profile)
        .bind(&run.prompt)
        .bind(&run.prompt_hash)
        .bind(&run.variables)
        .bind(&run.response)
        // Empty reasoning is stored as NULL, never as an empty string.
        .bind(
            run.thinking
                .as_deref()
                .filter(|text| !text.trim().is_empty()),
        )
        .bind(run.tokens_in)
        .bind(run.tokens_out)
        .bind(run.cache_read_tokens)
        .bind(run.cache_write_tokens)
        .bind(run.reasoning_tokens)
        .bind(run.cost)
        .bind(&run.status)
        .bind(&run.error)
        .bind(run.latency_ms)
        .bind(run.checkpoint_id)
        .bind(run.created_at)
        .bind(run.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_agent_run(&self, id: Uuid) -> Result<Option<AgentRun>, StoreError> {
        let row = sqlx::query_as::<_, AgentRunRow>(
            "SELECT id, run_id, call_index, task_id, intent_id, role, model, profile, prompt,
                    prompt_hash, variables, response, thinking, tokens_in, tokens_out,
                    cache_read_tokens, cache_write_tokens, reasoning_tokens, cost, status, error,
                    latency_ms, checkpoint_id, created_at, updated_at
             FROM agent_runs WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(AgentRun::try_from).transpose()
    }

    /// Every completion-call row of one task, oldest first.
    pub async fn list_agent_runs(&self, task_id: Uuid) -> Result<Vec<AgentRun>, StoreError> {
        let rows = sqlx::query_as::<_, AgentRunRow>(
            "SELECT id, run_id, call_index, task_id, intent_id, role, model, profile, prompt,
                    prompt_hash, variables, response, thinking, tokens_in, tokens_out,
                    cache_read_tokens, cache_write_tokens, reasoning_tokens, cost, status, error,
                    latency_ms, checkpoint_id, created_at, updated_at
             FROM agent_runs WHERE task_id = $1 ORDER BY created_at, call_index",
        )
        .bind(task_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(AgentRun::try_from).collect()
    }

    /// Creates a side-question session with the caller's frozen snapshot.
    /// Any open session of the same task is closed in the same transaction,
    /// so a task has at most one open conversation.
    pub async fn create_side_question_session(
        &self,
        task_id: Uuid,
        snapshot: serde_json::Value,
    ) -> Result<SideQuestionSession, StoreError> {
        let id = Uuid::new_v4();
        let created_at = Utc::now();
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "UPDATE side_question_sessions
             SET status = 'closed', closed_at = now()
             WHERE task_id = $1 AND status = 'open'",
        )
        .bind(task_id)
        .execute(&mut *tx)
        .await?;

        let row = sqlx::query_as::<_, SideQuestionSessionRow>(
            "INSERT INTO side_question_sessions (id, task_id, snapshot, status, created_at)
             VALUES ($1, $2, $3, 'open', $4)
             RETURNING id, task_id, snapshot, status, created_at, closed_at",
        )
        .bind(id)
        .bind(task_id)
        .bind(snapshot)
        .bind(created_at)
        .fetch_one(&mut *tx)
        .await?;

        tx.commit().await?;
        SideQuestionSession::try_from(row)
    }

    /// The newest open side-question session of a task, if one exists.
    pub async fn get_open_side_question_session(
        &self,
        task_id: Uuid,
    ) -> Result<Option<SideQuestionSession>, StoreError> {
        let row = sqlx::query_as::<_, SideQuestionSessionRow>(
            "SELECT id, task_id, snapshot, status, created_at, closed_at
             FROM side_question_sessions
             WHERE task_id = $1 AND status = 'open'
             ORDER BY created_at DESC, id DESC
             LIMIT 1",
        )
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(SideQuestionSession::try_from).transpose()
    }

    /// One side-question session by id, including its frozen snapshot.
    pub async fn get_side_question_session(
        &self,
        session_id: Uuid,
    ) -> Result<Option<SideQuestionSession>, StoreError> {
        let row = sqlx::query_as::<_, SideQuestionSessionRow>(
            "SELECT id, task_id, snapshot, status, created_at, closed_at
             FROM side_question_sessions WHERE id = $1",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(SideQuestionSession::try_from).transpose()
    }

    /// Every session of a task, newest first, each with its requests oldest
    /// first. The full snapshot travels with the session; the API strips it
    /// from list responses.
    pub async fn list_side_question_sessions(
        &self,
        task_id: Uuid,
    ) -> Result<Vec<SideQuestionThread>, StoreError> {
        let session_rows = sqlx::query_as::<_, SideQuestionSessionRow>(
            "SELECT id, task_id, snapshot, status, created_at, closed_at
             FROM side_question_sessions
             WHERE task_id = $1
             ORDER BY created_at DESC, id DESC",
        )
        .bind(task_id)
        .fetch_all(&self.pool)
        .await?;

        if session_rows.is_empty() {
            return Ok(Vec::new());
        }

        let ids: Vec<Uuid> = session_rows.iter().map(|row| row.id).collect();
        let request_rows = sqlx::query_as::<_, SideQuestionRequestRow>(
            "SELECT id, session_id, question, answer, status, error, model,
                    tokens_in, tokens_out, latency_ms, created_at, completed_at
             FROM side_question_requests
             WHERE session_id = ANY($1)
             ORDER BY created_at, id",
        )
        .bind(&ids)
        .fetch_all(&self.pool)
        .await?;

        let mut requests: HashMap<Uuid, Vec<SideQuestionRequest>> = HashMap::new();
        for row in request_rows {
            let request = SideQuestionRequest::try_from(row)?;
            requests
                .entry(request.session_id)
                .or_default()
                .push(request);
        }

        let mut threads = Vec::with_capacity(session_rows.len());
        for row in session_rows {
            let session = SideQuestionSession::try_from(row)?;
            let requests = requests.remove(&session.id).unwrap_or_default();
            threads.push(SideQuestionThread { session, requests });
        }
        Ok(threads)
    }

    /// Appends one completed question/answer attempt to a session.
    pub async fn append_side_question(
        &self,
        request: NewSideQuestionRequest,
    ) -> Result<SideQuestionRequest, StoreError> {
        let id = Uuid::new_v4();
        let created_at = Utc::now();
        let completed_at = match request.status {
            SideQuestionStatus::Pending => None,
            SideQuestionStatus::Ok | SideQuestionStatus::Error => Some(created_at),
        };
        let row = sqlx::query_as::<_, SideQuestionRequestRow>(
            "INSERT INTO side_question_requests
             (id, session_id, question, answer, status, error, model, tokens_in,
              tokens_out, latency_ms, created_at, completed_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
             RETURNING id, session_id, question, answer, status, error, model,
                       tokens_in, tokens_out, latency_ms, created_at, completed_at",
        )
        .bind(id)
        .bind(request.session_id)
        .bind(&request.question)
        .bind(&request.answer)
        .bind(request.status.as_str())
        .bind(&request.error)
        .bind(&request.model)
        .bind(request.tokens_in)
        .bind(request.tokens_out)
        .bind(request.latency_ms)
        .bind(created_at)
        .bind(completed_at)
        .fetch_one(&self.pool)
        .await?;
        SideQuestionRequest::try_from(row)
    }

    pub async fn save_checkpoint(
        &self,
        agent_run_id: Uuid,
        state: serde_json::Value,
    ) -> Result<Uuid, StoreError> {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO checkpoints (id, agent_run_id, state, created_at)
             VALUES ($1, $2, $3, now())",
        )
        .bind(id)
        .bind(agent_run_id)
        .bind(state)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }
}

#[derive(Debug, FromRow)]
struct TaskRow {
    id: Uuid,
    goal: String,
    bound: i32,
    state: String,
    exploration_id: Uuid,
    origin_fact_id: Uuid,
    pinned_planner_profile_id: Option<i64>,
    pinned_worker_profile_id: Option<i64>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<TaskRow> for Task {
    type Error = StoreError;
    fn try_from(r: TaskRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: r.id,
            goal: r.goal,
            bound: r.bound,
            state: parse_task_state(&r.state)?,
            exploration_id: r.exploration_id,
            origin_fact_id: r.origin_fact_id,
            pinned_planner_profile_id: r.pinned_planner_profile_id,
            pinned_worker_profile_id: r.pinned_worker_profile_id,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
    }
}

#[derive(Debug, FromRow)]
struct GoalRow {
    id: Uuid,
    task_id: Uuid,
    description: String,
    state: String,
    proof_fact_id: Option<Uuid>,
    created_at: DateTime<Utc>,
}

impl TryFrom<GoalRow> for Goal {
    type Error = StoreError;
    fn try_from(r: GoalRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: r.id,
            task_id: r.task_id,
            description: r.description,
            state: parse_goal_state(&r.state)?,
            proof_fact_id: r.proof_fact_id,
            created_at: r.created_at,
        })
    }
}

#[derive(Debug, FromRow)]
struct IntentRow {
    id: Uuid,
    task_id: Uuid,
    goal_id: Option<Uuid>,
    fact_id: Uuid,
    description: String,
    state: String,
    owner: Option<String>,
    priority: i32,
    created_at: DateTime<Utc>,
    completed_at: Option<DateTime<Utc>>,
}

impl TryFrom<IntentRow> for Intent {
    type Error = StoreError;
    fn try_from(r: IntentRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: r.id,
            task_id: r.task_id,
            goal_id: r.goal_id,
            fact_id: r.fact_id,
            description: r.description,
            state: parse_intent_state(&r.state)?,
            owner: r.owner,
            priority: r.priority,
            created_at: r.created_at,
            completed_at: r.completed_at,
        })
    }
}

#[derive(Debug, FromRow)]
struct FactRow {
    id: Uuid,
    task_id: Uuid,
    claim: String,
    state: String,
    provenance: serde_json::Value,
    origin_of: Option<Uuid>,
    created_at: DateTime<Utc>,
}

impl TryFrom<FactRow> for Fact {
    type Error = StoreError;
    fn try_from(r: FactRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: r.id,
            task_id: r.task_id,
            claim: r.claim,
            state: parse_fact_state(&r.state)?,
            provenance: r.provenance,
            origin_of: r.origin_of,
            created_at: r.created_at,
        })
    }
}

#[derive(Debug, FromRow)]
struct ResultRow {
    id: Uuid,
    task_id: Uuid,
    intent_id: IntentId,
    claim: String,
    acceptance: String,
    evidence: serde_json::Value,
    created_at: DateTime<Utc>,
}

impl TryFrom<ResultRow> for ResultRecord {
    type Error = StoreError;
    fn try_from(r: ResultRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: r.id,
            task_id: r.task_id,
            intent_id: r.intent_id,
            claim: r.claim,
            acceptance: parse_result_status(&r.acceptance)?,
            evidence: r.evidence,
            created_at: r.created_at,
        })
    }
}

#[derive(Debug, FromRow)]
struct ActivityRow {
    id: i64,
    task_id: Uuid,
    kind: String,
    message: String,
    payload: serde_json::Value,
    created_at: DateTime<Utc>,
}

impl From<ActivityRow> for Activity {
    fn from(r: ActivityRow) -> Self {
        Self {
            id: r.id,
            task_id: r.task_id,
            kind: r.kind,
            message: r.message,
            payload: r.payload,
            created_at: r.created_at,
        }
    }
}

#[derive(Debug, FromRow)]
struct ToolCallRow {
    id: ToolCallId,
    task_id: Uuid,
    intent_id: IntentId,
    tool_name: String,
    input: serde_json::Value,
    output: Option<serde_json::Value>,
    status: String,
    created_at: DateTime<Utc>,
}

impl From<ToolCallRow> for ToolCall {
    fn from(r: ToolCallRow) -> Self {
        Self {
            id: r.id,
            task_id: r.task_id,
            intent_id: r.intent_id,
            tool_name: r.tool_name,
            input: r.input,
            output: r.output,
            status: r.status,
            created_at: r.created_at,
        }
    }
}

#[derive(Debug, FromRow)]
struct ApprovalRow {
    id: ApprovalId,
    task_id: Uuid,
    tool_call_id: ToolCallId,
    guard_decision_id: Uuid,
    status: String,
    reason: String,
    expires_at: DateTime<Utc>,
    decided_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

impl TryFrom<ApprovalRow> for Approval {
    type Error = StoreError;
    fn try_from(r: ApprovalRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: r.id,
            task_id: r.task_id,
            tool_call_id: r.tool_call_id,
            guard_decision_id: r.guard_decision_id,
            status: parse_approval_status(&r.status)?,
            reason: r.reason,
            expires_at: r.expires_at,
            decided_at: r.decided_at,
            created_at: r.created_at,
        })
    }
}

#[derive(Debug, FromRow)]
struct AgentRunRow {
    id: RunId,
    run_id: RunId,
    call_index: i32,
    task_id: TaskId,
    intent_id: Option<IntentId>,
    role: String,
    model: String,
    profile: String,
    prompt: String,
    prompt_hash: String,
    variables: serde_json::Value,
    response: Option<String>,
    thinking: Option<String>,
    tokens_in: i64,
    tokens_out: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    reasoning_tokens: i64,
    cost: f64,
    status: String,
    error: Option<String>,
    latency_ms: i64,
    checkpoint_id: Option<CheckpointId>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<AgentRunRow> for AgentRun {
    type Error = StoreError;
    fn try_from(r: AgentRunRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: r.id,
            run_id: r.run_id,
            call_index: u32::try_from(r.call_index).map_err(|_| {
                StoreError::InvalidValue(format!("negative call_index {}", r.call_index))
            })?,
            task_id: r.task_id,
            intent_id: r.intent_id,
            role: parse_agent_role(&r.role)?,
            model: r.model,
            profile: r.profile,
            prompt: r.prompt,
            prompt_hash: r.prompt_hash,
            variables: r.variables,
            response: r.response,
            thinking: r.thinking,
            tokens_in: r.tokens_in,
            tokens_out: r.tokens_out,
            cache_read_tokens: r.cache_read_tokens,
            cache_write_tokens: r.cache_write_tokens,
            reasoning_tokens: r.reasoning_tokens,
            cost: r.cost,
            status: r.status,
            error: r.error,
            latency_ms: r.latency_ms,
            checkpoint_id: r.checkpoint_id,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
    }
}

#[derive(Debug, FromRow)]
struct SideQuestionSessionRow {
    id: Uuid,
    task_id: Uuid,
    snapshot: serde_json::Value,
    status: String,
    created_at: DateTime<Utc>,
    closed_at: Option<DateTime<Utc>>,
}

impl TryFrom<SideQuestionSessionRow> for SideQuestionSession {
    type Error = StoreError;

    fn try_from(r: SideQuestionSessionRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: r.id,
            task_id: r.task_id,
            snapshot: r.snapshot,
            status: parse_side_question_session_status(&r.status)?,
            created_at: r.created_at,
            closed_at: r.closed_at,
        })
    }
}

#[derive(Debug, FromRow)]
struct SideQuestionRequestRow {
    id: Uuid,
    session_id: Uuid,
    question: String,
    answer: Option<String>,
    status: String,
    error: Option<String>,
    model: Option<String>,
    tokens_in: Option<i32>,
    tokens_out: Option<i32>,
    latency_ms: Option<i64>,
    created_at: DateTime<Utc>,
    completed_at: Option<DateTime<Utc>>,
}

impl TryFrom<SideQuestionRequestRow> for SideQuestionRequest {
    type Error = StoreError;

    fn try_from(r: SideQuestionRequestRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: r.id,
            session_id: r.session_id,
            question: r.question,
            answer: r.answer,
            status: parse_side_question_status(&r.status)?,
            error: r.error,
            model: r.model,
            tokens_in: r.tokens_in,
            tokens_out: r.tokens_out,
            latency_ms: r.latency_ms,
            created_at: r.created_at,
            completed_at: r.completed_at,
        })
    }
}

fn parse_side_question_session_status(s: &str) -> Result<SideQuestionSessionStatus, StoreError> {
    match s {
        "open" => Ok(SideQuestionSessionStatus::Open),
        "closed" => Ok(SideQuestionSessionStatus::Closed),
        other => Err(StoreError::InvalidValue(format!(
            "side question session status {other}"
        ))),
    }
}

fn parse_side_question_status(s: &str) -> Result<SideQuestionStatus, StoreError> {
    match s {
        "pending" => Ok(SideQuestionStatus::Pending),
        "ok" => Ok(SideQuestionStatus::Ok),
        "error" => Ok(SideQuestionStatus::Error),
        other => Err(StoreError::InvalidValue(format!(
            "side question status {other}"
        ))),
    }
}

/// The contract's `AgentRunStatus` vocabulary, including `truncated`,
/// `no_output`, and `aborted`. The database enforces the same set with
/// `agent_runs_status_check`; this rejects a bad value before the round trip.
fn parse_agent_run_status(s: &str) -> Result<&str, StoreError> {
    match s {
        "running" | "ok" | "error" | "exhausted" | "timed_out" | "truncated" | "no_output"
        | "aborted" => Ok(s),
        other => Err(StoreError::InvalidValue(format!(
            "agent run status {other}"
        ))),
    }
}

fn parse_agent_role(s: &str) -> Result<AgentRole, StoreError> {
    match s {
        "planner" => Ok(AgentRole::Planner),
        "worker" => Ok(AgentRole::Worker),
        "reporter" => Ok(AgentRole::Reporter),
        "digest" => Ok(AgentRole::Digest),
        other => Err(StoreError::InvalidValue(format!("agent role {other}"))),
    }
}

fn parse_task_state(s: &str) -> Result<TaskState, StoreError> {
    match s {
        "created" => Ok(TaskState::Created),
        "running" => Ok(TaskState::Running),
        "paused" => Ok(TaskState::Paused),
        "done" => Ok(TaskState::Done),
        "failed" => Ok(TaskState::Failed),
        "timeout" => Ok(TaskState::Timeout),
        "cancelled" => Ok(TaskState::Cancelled),
        other => Err(StoreError::InvalidValue(format!("task state {other}"))),
    }
}

fn parse_goal_state(s: &str) -> Result<GoalState, StoreError> {
    match s {
        "open" => Ok(GoalState::Open),
        "met" => Ok(GoalState::Met),
        "abandoned" => Ok(GoalState::Abandoned),
        other => Err(StoreError::InvalidValue(format!("goal state {other}"))),
    }
}

fn parse_intent_state(s: &str) -> Result<IntentState, StoreError> {
    match s {
        "open" => Ok(IntentState::Open),
        "running" => Ok(IntentState::Running),
        "paused" => Ok(IntentState::Paused),
        "done" => Ok(IntentState::Done),
        "blocked" => Ok(IntentState::Blocked),
        "exhausted" => Ok(IntentState::Exhausted),
        "stopped" => Ok(IntentState::Stopped),
        "deleted" => Ok(IntentState::Deleted),
        other => Err(StoreError::InvalidValue(format!("intent state {other}"))),
    }
}

fn parse_fact_state(s: &str) -> Result<FactState, StoreError> {
    match s {
        "origin" => Ok(FactState::Origin),
        "confirmed" => Ok(FactState::Confirmed),
        "dismissed" => Ok(FactState::Dismissed),
        other => Err(StoreError::InvalidValue(format!("fact state {other}"))),
    }
}

fn parse_result_status(s: &str) -> Result<ResultStatus, StoreError> {
    match s {
        "pending" => Ok(ResultStatus::Pending),
        "accepted" => Ok(ResultStatus::Accepted),
        "rejected" => Ok(ResultStatus::Rejected),
        other => Err(StoreError::InvalidValue(format!("result status {other}"))),
    }
}

fn parse_approval_status(s: &str) -> Result<ApprovalStatus, StoreError> {
    match s {
        "pending" => Ok(ApprovalStatus::Pending),
        "granted" => Ok(ApprovalStatus::Granted),
        "denied" => Ok(ApprovalStatus::Denied),
        "expired" => Ok(ApprovalStatus::Expired),
        other => Err(StoreError::InvalidValue(format!("approval status {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_state_parser_rejects_unknown() {
        assert!(parse_task_state("nope").is_err());
        assert_eq!(parse_task_state("running").unwrap(), TaskState::Running);
        assert_eq!(parse_task_state("cancelled").unwrap(), TaskState::Cancelled);
    }

    #[test]
    fn intent_state_parser_rejects_unknown() {
        assert!(parse_intent_state("nope").is_err());
        assert_eq!(parse_intent_state("open").unwrap(), IntentState::Open);
    }

    #[test]
    fn agent_run_status_parser_accepts_the_contract_vocabulary() {
        for status in [
            "running",
            "ok",
            "error",
            "exhausted",
            "timed_out",
            "truncated",
            "no_output",
            "aborted",
        ] {
            assert_eq!(parse_agent_run_status(status).unwrap(), status);
        }
        assert!(matches!(
            parse_agent_run_status("bogus"),
            Err(StoreError::InvalidValue(_))
        ));
    }

    #[test]
    fn agent_role_parser_roundtrips() {
        for role in [
            AgentRole::Planner,
            AgentRole::Worker,
            AgentRole::Reporter,
            AgentRole::Digest,
        ] {
            assert_eq!(parse_agent_role(role.as_str()).unwrap(), role);
        }
        assert!(parse_agent_role("orchestrator").is_err());
    }
}
