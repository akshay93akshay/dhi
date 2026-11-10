//! `swarm_scale` approval storage and the gate the team runner calls.
//!
//! A fixed swarm never uses this module. An Auto swarm may use
//! [`SWARM_AUTO_WORKERS`](dhi_core::SWARM_AUTO_WORKERS) teammates without asking;
//! a wider team opens one pending request, waits, and treats every non-granted
//! outcome as a denial.

use chrono::{DateTime, Duration, Utc};
use dhi_core::{
    ApprovalDecision, SCALE_APPROVAL_TIMEOUT_SECS, ScaleApproval, ScaleApprovalGate,
    ScaleApprovalId, ScaleApprovalRequest, ScaleApprovalStatus, TaskId, valid_swarm_worker_count,
};
use sqlx::FromRow;
use uuid::Uuid;

use super::{Store, StoreError};

/// How often a waiting gate re-reads its row. Expiry is at most this late.
const SCALE_APPROVAL_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// A pending request may not live longer than one week even if a bad settings
/// row names a larger timeout.
const SCALE_APPROVAL_MAX_TIMEOUT_SECS: u64 = 7 * 24 * 60 * 60;

const SCALE_APPROVAL_COLUMNS: &str = "id, task_id, team_id, requested_workers, current_cap, \
     reason, status, created_at, expires_at, decided_at";

#[derive(Debug, FromRow)]
struct ScaleApprovalRow {
    id: Uuid,
    task_id: Uuid,
    team_id: Option<Uuid>,
    requested_workers: i32,
    current_cap: i32,
    reason: String,
    status: String,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    decided_at: Option<DateTime<Utc>>,
}

impl TryFrom<ScaleApprovalRow> for ScaleApproval {
    type Error = StoreError;

    fn try_from(row: ScaleApprovalRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            task_id: row.task_id,
            team_id: row.team_id,
            requested_workers: usize::try_from(row.requested_workers).map_err(|_| {
                StoreError::InvalidValue(format!("requested_workers {}", row.requested_workers))
            })?,
            current_cap: usize::try_from(row.current_cap).map_err(|_| {
                StoreError::InvalidValue(format!("current_cap {}", row.current_cap))
            })?,
            reason: row.reason,
            status: parse_scale_approval_status(&row.status)?,
            created_at: row.created_at,
            expires_at: row.expires_at,
            decided_at: row.decided_at,
        })
    }
}

fn parse_scale_approval_status(status: &str) -> Result<ScaleApprovalStatus, StoreError> {
    match status {
        "pending" => Ok(ScaleApprovalStatus::Pending),
        "granted" => Ok(ScaleApprovalStatus::Granted),
        "denied" => Ok(ScaleApprovalStatus::Denied),
        "expired" => Ok(ScaleApprovalStatus::Expired),
        other => Err(StoreError::InvalidValue(format!(
            "scale approval status {other}"
        ))),
    }
}

impl Store {
    /// Opens one pending `swarm_scale` request.
    ///
    /// `requested_workers` must be a valid fixed swarm count (2..=64) and
    /// above `current_cap`; otherwise the request changes no cap and is
    /// rejected. The lifetime comes from `timeout_secs`, then the configured
    /// approval timeout, then the fixed 15 minute default.
    pub async fn create_scale_approval(
        &self,
        request: &ScaleApprovalRequest,
    ) -> Result<ScaleApproval, StoreError> {
        if !valid_swarm_worker_count(request.requested_workers) {
            return Err(StoreError::Validation(format!(
                "requested_workers must be between {} and {}",
                dhi_core::SWARM_MIN_WORKERS,
                dhi_core::SWARM_MAX_WORKERS
            )));
        }
        if request.current_cap == 0 || request.current_cap > dhi_core::SWARM_MAX_WORKERS {
            return Err(StoreError::Validation(format!(
                "current_cap must be between 1 and {}",
                dhi_core::SWARM_MAX_WORKERS
            )));
        }
        if request.requested_workers <= request.current_cap {
            return Err(StoreError::Validation(format!(
                "requested_workers {} must exceed current_cap {}",
                request.requested_workers, request.current_cap
            )));
        }

        let timeout_secs = match request.timeout_secs {
            Some(secs) => secs,
            None => match self.get_settings().await? {
                Some(settings) => u64::try_from(settings.approval_timeout_secs.max(1))
                    .unwrap_or(SCALE_APPROVAL_TIMEOUT_SECS),
                None => SCALE_APPROVAL_TIMEOUT_SECS,
            },
        }
        .min(SCALE_APPROVAL_MAX_TIMEOUT_SECS);

        let now = Utc::now();
        let approval = ScaleApproval {
            id: Uuid::new_v4(),
            task_id: request.task_id,
            team_id: request.team_id,
            requested_workers: request.requested_workers,
            current_cap: request.current_cap,
            reason: request.reason.trim().to_owned(),
            status: ScaleApprovalStatus::Pending,
            created_at: now,
            expires_at: now + Duration::seconds(i64::try_from(timeout_secs).unwrap_or(i64::MAX)),
            decided_at: None,
        };

        sqlx::query(
            "INSERT INTO scale_approvals
             (id, task_id, team_id, requested_workers, current_cap, reason, status,
              created_at, expires_at, decided_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(approval.id)
        .bind(approval.task_id)
        .bind(approval.team_id)
        .bind(i32::try_from(approval.requested_workers).unwrap_or(i32::MAX))
        .bind(i32::try_from(approval.current_cap).unwrap_or(i32::MAX))
        .bind(&approval.reason)
        .bind(approval.status.as_str())
        .bind(approval.created_at)
        .bind(approval.expires_at)
        .bind(approval.decided_at)
        .execute(&self.pool)
        .await?;

        Ok(approval)
    }

    /// One request by id, whatever its status.
    pub async fn get_scale_approval(
        &self,
        approval_id: ScaleApprovalId,
    ) -> Result<Option<ScaleApproval>, StoreError> {
        let row = sqlx::query_as::<_, ScaleApprovalRow>(&format!(
            "SELECT {SCALE_APPROVAL_COLUMNS} FROM scale_approvals WHERE id = $1"
        ))
        .bind(approval_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(ScaleApproval::try_from).transpose()
    }

    /// Every pending request of every task, oldest first. The UI queue and the
    /// supervisor both read this view.
    pub async fn list_pending_scale_approvals(&self) -> Result<Vec<ScaleApproval>, StoreError> {
        let rows = sqlx::query_as::<_, ScaleApprovalRow>(&format!(
            "SELECT {SCALE_APPROVAL_COLUMNS}
             FROM scale_approvals
             WHERE status = 'pending'
             ORDER BY created_at, id"
        ))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(ScaleApproval::try_from).collect()
    }

    /// Every pending request of one task, oldest first.
    pub async fn list_pending_scale_approvals_for_task(
        &self,
        task_id: TaskId,
    ) -> Result<Vec<ScaleApproval>, StoreError> {
        let rows = sqlx::query_as::<_, ScaleApprovalRow>(&format!(
            "SELECT {SCALE_APPROVAL_COLUMNS}
             FROM scale_approvals
             WHERE task_id = $1 AND status = 'pending'
             ORDER BY created_at, id"
        ))
        .bind(task_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(ScaleApproval::try_from).collect()
    }

    /// Resolves one pending request.
    ///
    /// `None` means the row is missing or no longer pending (including the
    /// case where it expired before the decision landed). A pending row past
    /// its deadline is marked `expired` and returned; expiry is a denial.
    pub async fn decide_scale_approval(
        &self,
        approval_id: ScaleApprovalId,
        decision: ApprovalDecision,
    ) -> Result<Option<ScaleApproval>, StoreError> {
        let mut tx = self.pool.begin().await?;
        let existing = sqlx::query_as::<_, ScaleApprovalRow>(&format!(
            "SELECT {SCALE_APPROVAL_COLUMNS} FROM scale_approvals WHERE id = $1 FOR UPDATE"
        ))
        .bind(approval_id)
        .fetch_optional(&mut *tx)
        .await?;

        let Some(existing) = existing else {
            tx.rollback().await?;
            return Ok(None);
        };
        if existing.status != ScaleApprovalStatus::Pending.as_str() {
            tx.rollback().await?;
            return Ok(None);
        }

        let expired = existing.expires_at < Utc::now();
        let status = if expired {
            ScaleApprovalStatus::Expired
        } else {
            match decision {
                ApprovalDecision::Allow => ScaleApprovalStatus::Granted,
                ApprovalDecision::Deny => ScaleApprovalStatus::Denied,
            }
        };

        let row = sqlx::query_as::<_, ScaleApprovalRow>(&format!(
            "UPDATE scale_approvals
             SET status = $2,
                 decided_at = CASE WHEN $2 = 'expired' THEN NULL ELSE now() END
             WHERE id = $1
             RETURNING {SCALE_APPROVAL_COLUMNS}"
        ))
        .bind(approval_id)
        .bind(status.as_str())
        .fetch_one(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(Some(ScaleApproval::try_from(row)?))
    }

    /// Marks every pending request past `expires_at` as `expired` and returns
    /// the rows it changed. Expiry is not a human decision, so `decided_at`
    /// stays null.
    ///
    /// Candidate rows are locked in one transaction, so a concurrent decision
    /// and an expiry cannot both win on the same row.
    pub async fn expire_pending_scale_approvals(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Vec<ScaleApproval>, StoreError> {
        let mut tx = self.pool.begin().await?;
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM scale_approvals
             WHERE status = 'pending' AND expires_at < $1
             FOR UPDATE",
        )
        .bind(now)
        .fetch_all(&mut *tx)
        .await?;

        if ids.is_empty() {
            tx.commit().await?;
            return Ok(Vec::new());
        }

        let rows = sqlx::query_as::<_, ScaleApprovalRow>(&format!(
            "UPDATE scale_approvals
             SET status = 'expired', decided_at = NULL
             WHERE id = ANY($1)
             RETURNING {SCALE_APPROVAL_COLUMNS}"
        ))
        .bind(&ids)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        rows.into_iter().map(ScaleApproval::try_from).collect()
    }

    /// Blocks until the request leaves `pending`.
    ///
    /// The row is re-read every [`SCALE_APPROVAL_POLL_INTERVAL`]. A pending row
    /// past its deadline is expired before the re-read, so the wait always
    /// ends. A missing row is an error: the caller must treat it as a denial.
    pub async fn wait_for_scale_approval(
        &self,
        approval_id: ScaleApprovalId,
    ) -> Result<ScaleApproval, StoreError> {
        wait_for_scale_approval(self, approval_id).await
    }
}

async fn wait_for_scale_approval(
    store: &Store,
    approval_id: ScaleApprovalId,
) -> Result<ScaleApproval, StoreError> {
    loop {
        let Some(approval) = store.get_scale_approval(approval_id).await? else {
            return Err(StoreError::NotFound(format!(
                "scale approval {approval_id}"
            )));
        };
        if approval.status != ScaleApprovalStatus::Pending {
            return Ok(approval);
        }
        let now = Utc::now();
        if now >= approval.expires_at {
            store.expire_pending_scale_approvals(now).await?;
            continue;
        }
        tokio::time::sleep(SCALE_APPROVAL_POLL_INTERVAL).await;
    }
}

#[async_trait::async_trait]
impl ScaleApprovalGate for Store {
    async fn request_scale_approval(
        &self,
        request: ScaleApprovalRequest,
    ) -> anyhow::Result<ScaleApproval> {
        let approval = self.create_scale_approval(&request).await?;
        Ok(approval)
    }

    async fn wait_for_scale_approval(
        &self,
        approval_id: ScaleApprovalId,
    ) -> anyhow::Result<ScaleApproval> {
        let approval = wait_for_scale_approval(self, approval_id).await?;
        Ok(approval)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use sqlx::PgPool;

    fn request(
        task_id: TaskId,
        requested: usize,
        timeout_secs: Option<u64>,
    ) -> ScaleApprovalRequest {
        ScaleApprovalRequest {
            task_id,
            team_id: None,
            requested_workers: requested,
            current_cap: dhi_core::SWARM_AUTO_WORKERS,
            reason: "auto swarm needs more teammates".to_string(),
            timeout_secs,
        }
    }

    #[sqlx::test]
    async fn create_and_list_pending_scale_approvals(pool: PgPool) {
        let store = Store::new(pool);
        let task = store.create_task("scale a swarm").await.unwrap();

        let approval = store
            .create_scale_approval(&request(task.id, 32, Some(600)))
            .await
            .unwrap();
        assert_eq!(approval.status, ScaleApprovalStatus::Pending);
        assert_eq!(approval.requested_workers, 32);
        assert_eq!(approval.current_cap, 8);
        assert!(approval.decided_at.is_none());
        assert_eq!(
            approval.expires_at - approval.created_at,
            Duration::seconds(600)
        );

        let pending = store.list_pending_scale_approvals().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, approval.id);

        let for_task = store
            .list_pending_scale_approvals_for_task(task.id)
            .await
            .unwrap();
        assert_eq!(for_task.len(), 1);
        assert_eq!(for_task[0].id, approval.id);

        let fetched = store
            .get_scale_approval(approval.id)
            .await
            .unwrap()
            .expect("the created request exists");
        assert_eq!(fetched.id, approval.id);
        assert_eq!(fetched.task_id, approval.task_id);
        assert_eq!(fetched.team_id, approval.team_id);
        assert_eq!(fetched.requested_workers, approval.requested_workers);
        assert_eq!(fetched.current_cap, approval.current_cap);
        assert_eq!(fetched.reason, approval.reason);
        assert_eq!(fetched.status, approval.status);
        assert_eq!(fetched.decided_at, approval.decided_at);
    }

    #[sqlx::test]
    async fn allow_and_deny_are_terminal(pool: PgPool) {
        let store = Store::new(pool);
        let task = store.create_task("decide a scale request").await.unwrap();

        let allowed = store
            .create_scale_approval(&request(task.id, 16, Some(600)))
            .await
            .unwrap();
        let granted = store
            .decide_scale_approval(allowed.id, ApprovalDecision::Allow)
            .await
            .unwrap()
            .expect("the request was pending");
        assert_eq!(granted.status, ScaleApprovalStatus::Granted);
        assert!(granted.decided_at.is_some());
        assert_eq!(granted.effective_cap(), 16);
        assert!(
            store
                .decide_scale_approval(allowed.id, ApprovalDecision::Deny)
                .await
                .unwrap()
                .is_none(),
            "a decided request cannot be decided again"
        );

        let denied_request = store
            .create_scale_approval(&request(task.id, 24, Some(600)))
            .await
            .unwrap();
        let denied = store
            .decide_scale_approval(denied_request.id, ApprovalDecision::Deny)
            .await
            .unwrap()
            .expect("the request was pending");
        assert_eq!(denied.status, ScaleApprovalStatus::Denied);
        assert!(denied.decided_at.is_some());
        assert_eq!(denied.effective_cap(), dhi_core::SWARM_AUTO_WORKERS);
    }

    #[sqlx::test]
    async fn expiry_is_a_denial_and_never_sets_decided_at(pool: PgPool) {
        let store = Store::new(pool);
        let task = store.create_task("expire a scale request").await.unwrap();

        let approval = store
            .create_scale_approval(&request(task.id, 64, Some(0)))
            .await
            .unwrap();
        let expired = store
            .expire_pending_scale_approvals(Utc::now() + Duration::seconds(1))
            .await
            .unwrap();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].id, approval.id);
        assert_eq!(expired[0].status, ScaleApprovalStatus::Expired);
        assert!(expired[0].decided_at.is_none());
        assert_eq!(
            expired[0].effective_cap(),
            dhi_core::SWARM_AUTO_WORKERS,
            "expiry counts as a denial"
        );

        assert!(
            store
                .decide_scale_approval(approval.id, ApprovalDecision::Allow)
                .await
                .unwrap()
                .is_none(),
            "an expired request can no longer be granted"
        );
    }

    #[sqlx::test]
    async fn decision_past_the_deadline_expires_instead_of_granting(pool: PgPool) {
        let store = Store::new(pool);
        let task = store.create_task("late scale decision").await.unwrap();

        let approval = store
            .create_scale_approval(&request(task.id, 12, Some(0)))
            .await
            .unwrap();
        let decided = store
            .decide_scale_approval(approval.id, ApprovalDecision::Allow)
            .await
            .unwrap()
            .expect("the row was still pending");
        assert_eq!(decided.status, ScaleApprovalStatus::Expired);
        assert!(decided.decided_at.is_none());
    }

    #[sqlx::test]
    async fn request_validation_rejects_out_of_range_counts(pool: PgPool) {
        let store = Store::new(pool);
        let task = store.create_task("validate a scale request").await.unwrap();

        for invalid in [0, 1, 65, 100] {
            let err = store
                .create_scale_approval(&request(task.id, invalid, Some(60)))
                .await
                .expect_err("out-of-range counts are rejected");
            assert!(matches!(err, StoreError::Validation(_)), "{err}");
        }

        let not_wider = request(task.id, 8, Some(60));
        assert!(
            store.create_scale_approval(&not_wider).await.is_err(),
            "a request must ask for a wider team than the current cap"
        );
    }

    #[sqlx::test]
    async fn gate_request_and_wait_returns_expiry_as_denial(pool: PgPool) {
        let store = Store::new(pool);
        let task = store.create_task("wait on a scale gate").await.unwrap();

        let approved = ScaleApprovalGate::request_and_wait_scale_approval(
            &store,
            request(task.id, 20, Some(0)),
        )
        .await
        .unwrap();
        assert_eq!(approved.status, ScaleApprovalStatus::Expired);
        assert!(!approved.status.is_granted());
    }
}
