//! DHI HTTP API.
//!
//! The binary (`main.rs`) owns the process-level wiring: it builds the one
//! guard, the engine, and the supervisor that sweeps expired approvals. This
//! crate exposes the router, the app state, and the supervisor task so the
//! wiring is testable without a database.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

pub mod mcp;
mod mcp_api;
pub mod side_questions;
pub mod sources;
pub mod static_web;
pub mod task_runs;
mod tool_api;
pub mod web_providers;

pub use task_runs::TaskRunRegistry;

use async_trait::async_trait;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use dhi_agent::{AgentRuntime, LlmPool, PoolSnapshot, RawProfile, ResolvedProfile};
use dhi_core::{
    AgentRole, AgentRun, Approval, ApprovalDecision, ApprovalStatus, GraphStore, GuardStore,
    GuardVerdict, ReopenOutcome, Task, TaskCapacity, TaskId, TaskState,
};
use dhi_engine::Engine;
use dhi_guard::ToolBackend;
use dhi_guard::{CompiledPolicy, GuardedExecutor, RuleSpec, SharedPolicy, ToolExecutor};
use dhi_store::mgmt::{
    DEFAULT_APPROVAL_TIMEOUT_SECS, GuardRuleInput, GuardRuleRow, LlmProfileUpsert, SettingPatch,
};
use dhi_store::{Store, StoreError};
use futures::StreamExt;
use futures::stream::{self, Stream};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Digest;
use uuid::Uuid;

/// How often the supervisor sweeps every non-terminal task for expired
/// approvals. This is a fixed interval; nothing else drives the timer.
pub const SUPERVISOR_INTERVAL: Duration = Duration::from_secs(30);

/// How many tasks one supervisor pass inspects.
const SUPERVISOR_TASK_LIMIT: i64 = 1_000;

/// How many tasks one restart recovery pass inspects.
const RECOVERY_TASK_LIMIT: i64 = 1_000;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<dyn GraphStore>,
    pub guard_store: Arc<dyn GuardStore>,
    pub engine: Arc<Engine>,
    /// The runtime that answers advisory side questions. It is the same
    /// runtime the engine uses; the side-question route calls only
    /// `side_answer`, which has no tools and no graph access.
    pub agent: Arc<dyn AgentRuntime>,
    /// The shared LLM failover pool. Snapshot swaps after every profile or
    /// settings mutation are visible to the running engine's next agent call.
    pub pool: Arc<LlmPool>,
    /// The shared guard snapshot handle; the same one the engine's
    /// `GuardedExecutor` evaluates. Rule and settings mutations swap it.
    pub guard: SharedPolicy,
    /// The management tables (LLM profiles, guard rules, settings, task
    /// pins). `None` in tests that wire only the memory store; the management
    /// routes answer `500` until a `Store` backs them.
    pub mgmt: Option<Arc<Store>>,
    /// The MCP manager: owns the configured MCP server
    /// connections and the guarded registry entries of their discovered
    /// tools, dispatched through the one guard via [`mcp::McpBackend`].
    /// Always present; idle (no servers) when no config exists.
    pub mcp: Arc<mcp::McpManager>,
    /// The read model of every registered tool source.
    pub tool_catalog: Arc<dhi_tool_catalog::CatalogRuntime>,
    /// The write side of the catalog. `None` in memory-only tests and
    /// degraded boots without a management store.
    pub tool_control: Option<Arc<dyn dhi_tool_control::ToolControl>>,
    /// The live web provider settings and router. Tool calls read one router
    /// snapshot per call; management writes swap it without restarting.
    pub web: Arc<web_providers::WebProviderRuntime>,
    /// The loops this process is running. Stop requests flip their flags; the
    /// hard-kill monitor aborts them after a grace period.
    pub task_runs: Arc<TaskRunRegistry>,
}

/// Default page size of `GET /api/tasks`.
const DEFAULT_TASK_LIMIT: i64 = 100;

/// Largest page size a caller may ask for.
const MAX_TASK_LIMIT: i64 = 1_000;

/// Page size of the work collections a task carries.
const WORK_LIMIT: i64 = 1_000;

/// Default page size of `GET /api/tasks/{id}/swarm-messages`.
const DEFAULT_SWARM_MESSAGE_LIMIT: i64 = 200;

/// Largest page size a caller may ask for on the swarm message route.
const MAX_SWARM_MESSAGE_LIMIT: i64 = 1_000;

/// How often the `GET /api/stream` handler polls the activity log.
pub const STREAM_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Largest accepted goal description, after trimming.
pub const ADD_GOAL_MAX_CHARS: usize = 500;

/// How long a stop request waits for the engine loop to exit gracefully
/// before the monitor aborts it and tears down its runtime resources.
pub const STOP_HARD_KILL_GRACE: Duration = Duration::from_secs(5);

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/tasks", get(list_tasks).post(create_task))
        .route("/api/tasks/stop", post(stop_tasks))
        .route("/api/tasks/delete", post(delete_tasks_route))
        .route("/api/tasks/{id}", get(get_task).delete(delete_task_route))
        .route("/api/tasks/{id}/stop", post(stop_task))
        .route("/api/tasks/{id}/goals", post(add_goal_route))
        .route("/api/tasks/{id}/capacity", post(set_task_capacity_route))
        .route("/api/tasks/{id}/work", get(get_task_work))
        .route("/api/tasks/{id}/activity", get(get_task_activity))
        .route("/api/tasks/{id}/usage", get(get_task_usage))
        .route(
            "/api/tasks/{id}/swarm-messages",
            get(get_task_swarm_messages),
        )
        .route(
            "/api/tasks/{id}/side-questions",
            get(side_questions::list_side_questions).post(side_questions::ask_side_question),
        )
        .route("/api/approvals", get(list_approvals))
        .route("/api/approvals/{id}/decision", post(decide_approval))
        .route("/api/stream", get(stream_activity))
        // MCP health/status and live management (live reload).
        // Writes persist through the standard `mcpServers` config first, then
        // reload the running manager; reads and writes never expose header
        // values.
        .route(
            "/api/mcp/servers",
            get(mcp_api::list_servers).post(mcp_api::add_server),
        )
        .route(
            "/api/mcp/servers/{name}",
            put(mcp_api::update_server)
                .patch(mcp_api::update_server)
                .delete(mcp_api::delete_server),
        )
        .route("/api/mcp/servers/{name}/tools", get(mcp_api::server_tools))
        .route(
            "/api/mcp/servers/{name}/refresh",
            post(mcp_api::refresh_server),
        )
        .route("/api/mcp/reload", post(mcp_api::reload_all))
        // Tool catalog and control.
        .route("/api/tools/catalog", get(tool_api::catalog))
        .route(
            "/api/tools/control",
            get(tool_api::states).post(tool_api::apply),
        )
        // LLM profiles, failover pool, model listing.
        .route(
            "/api/llm/profiles",
            get(list_profiles_route).post(save_profile_route),
        )
        .route("/api/llm/profiles/{id}", delete(delete_profile_route))
        .route("/api/llm/profiles/active", post(activate_profile_route))
        .route("/api/llm/models", get(list_models_route))
        .route("/api/llm/pool/status", get(pool_status_route))
        .route("/api/llm/pool/reset", post(pool_reset_route))
        // Guard rules (ARTEX intercept rules, guard flavor).
        .route(
            "/api/intercept/rules",
            get(list_rules_route).post(create_rule_route),
        )
        .route(
            "/api/intercept/rules/{id}",
            put(update_rule_route).delete(delete_rule_route),
        )
        .route("/api/intercept/rules/{id}/toggle", post(toggle_rule_route))
        .route(
            "/api/settings",
            get(get_settings_route).put(put_settings_route),
        )
        // Web provider settings, write-only keys, live probe, and reload.
        .route("/api/web/providers", get(web_providers::list_providers))
        .route(
            "/api/web/providers/reload",
            post(web_providers::reload_providers),
        )
        .route(
            "/api/web/providers/{provider}",
            put(web_providers::update_provider),
        )
        .route(
            "/api/web/providers/{provider}/key",
            delete(web_providers::delete_provider_key),
        )
        .route(
            "/api/web/providers/{provider}/test",
            post(web_providers::test_provider),
        )
        .with_state(state)
}

/// The API router plus the exported web UI when an export directory exists.
///
/// `DHI_WEB_DIR` wins; otherwise `web/out` is used only when present. A
/// missing export is not fatal: the process still serves the API and logs a
/// warning.
pub fn router_with_web_ui(state: AppState) -> Router {
    match static_web::resolve_web_dir() {
        Some(dir) => router_with_web_dir(state, Some(dir)),
        None => {
            tracing::warn!(
                dir = static_web::DEFAULT_WEB_DIR,
                "web UI export not found; serving API only"
            );
            router(state)
        }
    }
}

/// The API router with the static web UI fallback, when the directory carries
/// an `index.html`. Without one the API router is returned unchanged.
pub fn router_with_web_dir(state: AppState, web_dir: Option<PathBuf>) -> Router {
    let Some(dir) = web_dir else {
        return router(state);
    };
    if !dir.join("index.html").is_file() {
        tracing::warn!(
            dir = %dir.display(),
            "web UI export has no index.html; serving API only"
        );
        return router(state);
    }
    router(state).fallback_service(static_web::StaticWeb::new(dir).into_router())
}

async fn health() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "service": "dhi",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// The one API error shape: `{ "error": "message" }`.
pub(crate) struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    pub(crate) fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: message.into(),
        }
    }

    pub(crate) fn bad_gateway(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: message.into(),
        }
    }

    pub(crate) fn internal(err: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: err.to_string(),
        }
    }

    pub(crate) fn service_unavailable(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

/// `POST /api/tasks` input. The optional pins reference `llm_profiles` ids;
/// unknown ids are `400`. Clients may spell the pins either
/// `planner_profile_id`/`worker_profile_id` (historical) or
/// `pinned_planner_profile_id`/`pinned_worker_profile_id` (the column names
/// the response echoes).
///
/// An optional `capacity` ledger is written before the engine's first run,
/// so that run already sees the limits instead of racing a later extension.
#[derive(Debug, Deserialize)]
struct CreateTaskRequest {
    goal: String,
    bound: i32,
    #[serde(alias = "pinned_planner_profile_id")]
    planner_profile_id: Option<i64>,
    #[serde(alias = "pinned_worker_profile_id")]
    worker_profile_id: Option<i64>,
    capacity: Option<TaskCapacity>,
}

/// `POST /api/tasks/{id}/goals` input.
#[derive(Debug, Deserialize)]
struct AddGoalRequest {
    description: String,
}

/// `POST /api/tasks/{id}/goals` output: the reopened task plus its new goal.
#[derive(Debug, Serialize)]
struct AddGoalResponse {
    task: Task,
    goal: dhi_core::Goal,
}

/// `POST /api/tasks/{id}/capacity` input.
///
/// Every field is optional and, when present, must be a positive integer.
/// Omitted fields keep the task's current value; there is no way to clear a
/// limit through this route.
#[derive(Debug, Default, Deserialize)]
struct TaskCapacityRequest {
    #[serde(default)]
    max_rounds: Option<i64>,
    #[serde(default)]
    max_model_calls: Option<i64>,
    #[serde(default)]
    max_wall_secs: Option<i64>,
    #[serde(default)]
    max_parallel_workers: Option<i64>,
}

/// `POST /api/tasks/stop` and `POST /api/tasks/delete` input.
#[derive(Debug, Deserialize)]
struct TaskIdsRequest {
    ids: Vec<TaskId>,
}

/// `POST /api/tasks/{id}/stop` output.
#[derive(Debug, Serialize)]
struct TaskStopResponse {
    task_id: TaskId,
    status: &'static str,
}

/// One independently refused id of a bulk stop or delete.
#[derive(Debug, Serialize)]
struct BlockedTask {
    id: TaskId,
    state: String,
    error: String,
}

/// `POST /api/tasks/stop` output.
#[derive(Debug, Serialize)]
struct StopTasksResponse {
    stopping: Vec<TaskId>,
    blocked: Vec<BlockedTask>,
}

/// `POST /api/tasks/delete` output.
#[derive(Debug, Serialize)]
struct DeleteTasksResponse {
    deleted: Vec<TaskId>,
    blocked: Vec<BlockedTask>,
}

/// `GET /api/tasks` query string.
#[derive(Debug, Default, Deserialize)]
struct ListTasksQuery {
    limit: Option<i64>,
}

/// `GET /api/tasks/{id}/swarm-messages` query string.
#[derive(Debug, Default, Deserialize)]
struct SwarmMessagesQuery {
    limit: Option<i64>,
}

/// `GET /api/stream` query string.
#[derive(Debug, Deserialize)]
struct StreamQuery {
    task_id: String,
    after: Option<i64>,
}

/// `POST /api/approvals/{id}/decision` input.
#[derive(Debug, Deserialize)]
struct ApprovalDecisionRequest {
    decision: ApprovalDecision,
}

/// `POST /api/approvals/{id}/decision` output.
#[derive(Debug, Serialize)]
struct ApprovalDecisionResponse {
    approval: Approval,
    task: Task,
}

/// One row of `GET /api/approvals`: the durable [`Approval`] plus the tool
/// name and input of its linked tool call when that row exists.
#[derive(Debug, Serialize)]
struct ApprovalItem {
    #[serde(flatten)]
    approval: Approval,
    tool_name: Option<String>,
    input: Option<Value>,
}

/// `GET /api/tasks/{id}/work` output.
#[derive(Debug, Serialize)]
struct TaskWork {
    task: Task,
    goals: Vec<dhi_core::Goal>,
    intents: Vec<dhi_core::Intent>,
    facts: Vec<dhi_core::Fact>,
    results: Vec<dhi_core::ResultRecord>,
}

fn page_limit(requested: Option<i64>) -> Result<i64, ApiError> {
    let limit = requested.unwrap_or(DEFAULT_TASK_LIMIT);
    if !(1..=MAX_TASK_LIMIT).contains(&limit) {
        return Err(ApiError::bad_request(format!(
            "limit must be between 1 and {MAX_TASK_LIMIT}"
        )));
    }
    Ok(limit)
}

/// Loads the task or yields `404`.
async fn require_task(store: &dyn GraphStore, id: TaskId) -> Result<Task, ApiError> {
    store
        .get_task(id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("task not found: {id}")))
}

/// Creates a task, then runs it in the background.
///
/// The response is the task as created (`created`), including any capacity
/// supplied in the request. A failed run is logged and never fails the HTTP
/// response: the task row keeps whatever state the run reached.
async fn create_task(
    State(state): State<AppState>,
    payload: Result<Json<CreateTaskRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Task>), ApiError> {
    let Json(body) = payload.map_err(|rejection| ApiError::bad_request(rejection.body_text()))?;

    if body.goal.trim().is_empty() {
        return Err(ApiError::bad_request("goal must not be empty"));
    }
    if body.bound < 0 {
        return Err(ApiError::bad_request("bound must not be negative"));
    }
    let capacity = body.capacity.map(validate_create_capacity).transpose()?;
    // Pin validation needs the management tables; a pin-less create stays
    // available wherever the core store is wired.
    if body.planner_profile_id.is_some() || body.worker_profile_id.is_some() {
        let mgmt = require_mgmt(&state)?;
        validate_pin(mgmt, body.planner_profile_id, "planner_profile_id").await?;
        validate_pin(mgmt, body.worker_profile_id, "worker_profile_id").await?;
    }

    let mut task = state
        .store
        .create_task_with_pins(
            body.goal,
            body.bound,
            body.planner_profile_id,
            body.worker_profile_id,
        )
        .await
        .map_err(ApiError::internal)?;

    // The ledger must be durable before the engine snapshots it at run start.
    if let Some(capacity) = capacity {
        task = state
            .store
            .set_task_capacity(task.id, capacity)
            .await
            .map_err(ApiError::internal)?;
    }

    spawn_task_loop(&state.store, &state.task_runs, &state.engine, task.id);

    Ok((StatusCode::CREATED, Json(task)))
}

/// Refuses a create-time capacity with any zero field. Negative values never
/// reach this point: the core type's unsigned fields reject them during JSON
/// deserialization, which the extractor maps to `400` as well.
fn validate_create_capacity(capacity: TaskCapacity) -> Result<TaskCapacity, ApiError> {
    for (name, is_zero) in [
        ("max_rounds", capacity.max_rounds == Some(0)),
        ("max_model_calls", capacity.max_model_calls == Some(0)),
        ("max_wall_secs", capacity.max_wall_secs == Some(0)),
        (
            "max_parallel_workers",
            capacity.max_parallel_workers == Some(0),
        ),
    ] {
        if is_zero {
            return Err(ApiError::bad_request(format!(
                "{name} must be a positive integer"
            )));
        }
    }
    Ok(capacity)
}

/// Records an unrecoverable background run error as a visible terminal
/// failure. The guarded write is first-terminal-wins, so a task the engine
/// already failed (and whose `task_failed` row exists) is left alone.
async fn fail_task_from_server(
    store: &dyn GraphStore,
    task_id: TaskId,
    reason: String,
    source: &str,
) {
    match store
        .set_task_state_guarded(task_id, TaskState::Failed)
        .await
    {
        Ok(true) => {
            if let Err(err) = store
                .append_activity(
                    task_id,
                    "task_failed",
                    reason.clone(),
                    json!({"reason": reason, "source": source}),
                )
                .await
            {
                tracing::warn!(%task_id, error = %err, "could not record task_failed activity");
            }
        }
        Ok(false) => {}
        Err(err) => {
            tracing::warn!(%task_id, error = %err, "could not mark a dead task failed");
        }
    }
}

/// Starts one task loop in the background. Creating a task and reopening one
/// share this exact spawn path, so both start the engine the same way.
///
/// The loop is gated until its abort handle is registered: a fast loop can
/// otherwise finish and remove a not-yet-inserted entry, leaving a stale
/// worker in the registry. Every exit path removes its own entry; an
/// unrecoverable run error additionally marks the task failed with a
/// `task_failed` activity row.
fn spawn_task_loop(
    store: &Arc<dyn GraphStore>,
    registry: &Arc<TaskRunRegistry>,
    engine: &Arc<Engine>,
    task_id: TaskId,
) {
    let stop = Arc::new(AtomicBool::new(false));
    let (start_tx, start_rx) = tokio::sync::oneshot::channel::<()>();
    let engine = engine.clone();
    let registry = registry.clone();
    let loop_registry = registry.clone();
    let store = store.clone();
    let loop_stop = stop.clone();
    let handle = tokio::spawn(async move {
        // `register` runs before this permit is sent; the loop cannot race
        // ahead and remove an entry that does not exist yet.
        let _ = start_rx.await;
        let result = engine.run_task_with_stop(task_id, Some(loop_stop)).await;
        if let Err(err) = result {
            tracing::warn!(%task_id, error = %err, "task run failed");
            // Mark the row failed before dropping the entry: a running task
            // must always have a registered worker.
            fail_task_from_server(
                store.as_ref(),
                task_id,
                format!("task run failed: {err}"),
                "server",
            )
            .await;
        }
        // A graceful stop and a normal completion both land here. The entry
        // goes away only after the task reached its terminal state.
        loop_registry.remove(task_id);
    });
    registry.register(task_id, stop, handle.abort_handle());
    let _ = start_tx.send(());
}

/// Adds a goal to a terminal task and reopens it.
///
/// This is the only route that may move a terminal task back to `running`.
/// The store performs the goal insert, both activity rows, and the state
/// change in one transaction; the engine loop is then respawned exactly as
/// `create_task` does. A non-terminal task is `409` and is left untouched.
async fn add_goal_route(
    State(state): State<AppState>,
    Path(id): Path<TaskId>,
    Json(body): Json<AddGoalRequest>,
) -> Result<(StatusCode, Json<AddGoalResponse>), ApiError> {
    let description = dhi_followups::validate_goal(&body.description).map_err(|err| {
        ApiError::bad_request(match err {
            "goal description is required" => "description must not be empty".to_string(),
            _ => format!("description must be at most {ADD_GOAL_MAX_CHARS} characters"),
        })
    })?;

    let outcome = state
        .store
        .reopen_task_with_goal(id, description.to_string())
        .await
        .map_err(ApiError::internal)?;

    match outcome {
        ReopenOutcome::NotFound => Err(ApiError::not_found(format!("task not found: {id}"))),
        ReopenOutcome::NotTerminal(current) => Err(ApiError::conflict(format!(
            "task {id} is {current}; only a done, failed, timeout, or cancelled task can be reopened"
        ))),
        ReopenOutcome::Reopened { task, goal } => {
            spawn_task_loop(&state.store, &state.task_runs, &state.engine, task.id);
            Ok((StatusCode::OK, Json(AddGoalResponse { task, goal })))
        }
    }
}

/// `POST /api/tasks/{id}/capacity`.
///
/// Extends or lowers one task's capacity ledger. Omitted fields keep their
/// stored value. A paused task is moved back to `running` and its loop is
/// respawned, exactly like a reopened task. Terminal tasks are refused: their
/// ledger is audit data, not a lever to revive them.
async fn set_task_capacity_route(
    State(state): State<AppState>,
    Path(id): Path<TaskId>,
    payload: Result<Json<TaskCapacityRequest>, JsonRejection>,
) -> Result<Json<Task>, ApiError> {
    let Json(body) = payload.map_err(|rejection| ApiError::bad_request(rejection.body_text()))?;

    let task = require_task(state.store.as_ref(), id).await?;
    if task.state.is_terminal() {
        return Err(ApiError::conflict(format!(
            "task {id} is {}; its capacity cannot be extended",
            task.state
        )));
    }

    let capacity = TaskCapacity {
        max_rounds: positive_capacity("max_rounds", body.max_rounds)?.or(task.capacity.max_rounds),
        max_model_calls: positive_capacity("max_model_calls", body.max_model_calls)?
            .or(task.capacity.max_model_calls),
        max_wall_secs: positive_wall_secs(body.max_wall_secs)?.or(task.capacity.max_wall_secs),
        max_parallel_workers: positive_capacity("max_parallel_workers", body.max_parallel_workers)?
            .or(task.capacity.max_parallel_workers),
    };

    let mut updated = state
        .store
        .set_task_capacity(id, capacity)
        .await
        .map_err(ApiError::internal)?;

    // Only a paused task gets a new loop. A running task reads its ledger at
    // run start, so an extension applies to its next run.
    if updated.state == TaskState::Paused
        && state
            .store
            .set_task_state_guarded(id, TaskState::Running)
            .await
            .map_err(ApiError::internal)?
    {
        updated.state = TaskState::Running;
        spawn_task_loop(&state.store, &state.task_runs, &state.engine, id);
    }

    Ok(Json(updated))
}

fn positive_capacity(name: &str, value: Option<i64>) -> Result<Option<usize>, ApiError> {
    match value {
        None => Ok(None),
        Some(value) if value > 0 => usize::try_from(value)
            .map(Some)
            .map_err(|_| ApiError::bad_request(format!("{name} must be a positive integer"))),
        Some(_) => Err(ApiError::bad_request(format!(
            "{name} must be a positive integer"
        ))),
    }
}

fn positive_wall_secs(value: Option<i64>) -> Result<Option<u64>, ApiError> {
    match value {
        None => Ok(None),
        Some(value) if value > 0 => u64::try_from(value)
            .map(Some)
            .map_err(|_| ApiError::bad_request("max_wall_secs must be a positive integer")),
        Some(_) => Err(ApiError::bad_request(
            "max_wall_secs must be a positive integer",
        )),
    }
}

// ---------------------------------------------------------------------------
// Task cancellation and deletion
// ---------------------------------------------------------------------------

/// `POST /api/tasks/{id}/stop`.
///
/// A live loop gets its cooperative stop flag set and answers `202`; the hard
/// kill happens in the background after [`STOP_HARD_KILL_GRACE`]. An active
/// task with no registered loop is an orphan from a restart: it is marked
/// cancelled immediately with a `task_stopped` activity row.
async fn stop_task(
    State(state): State<AppState>,
    Path(id): Path<TaskId>,
) -> Result<(StatusCode, Json<TaskStopResponse>), ApiError> {
    let task = require_task(state.store.as_ref(), id).await?;
    if task.state.is_terminal() {
        return Err(ApiError::conflict(format!(
            "task {id} is {}; it cannot be stopped",
            task.state
        )));
    }
    stop_one_task(&state, &task).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(TaskStopResponse {
            task_id: id,
            status: "stopping",
        }),
    ))
}

async fn stop_one_task(state: &AppState, task: &Task) -> Result<(), ApiError> {
    if state.task_runs.request_stop(task.id) {
        spawn_hard_kill_monitor(state.store.clone(), state.task_runs.clone(), task.id);
        return Ok(());
    }

    // Orphan: no worker exists to observe the flag, so cancel it now.
    let changed = state
        .store
        .set_task_state_guarded(task.id, TaskState::Cancelled)
        .await
        .map_err(ApiError::internal)?;
    if changed {
        state
            .store
            .append_activity(
                task.id,
                "task_stopped",
                "task cancelled: stop requested".to_string(),
                json!({"source": "stop_request", "orphan": true}),
            )
            .await
            .map_err(ApiError::internal)?;
    }
    Ok(())
}

/// Waits out the cooperative grace period, then hard-kills whatever is still
/// registered. `abort_and_remove` is the single-winner check: a loop that
/// exits and removes its own entry makes this a no-op.
fn spawn_hard_kill_monitor(
    store: Arc<dyn GraphStore>,
    registry: Arc<TaskRunRegistry>,
    task_id: TaskId,
) {
    spawn_hard_kill_monitor_after(store, registry, task_id, STOP_HARD_KILL_GRACE);
}

fn spawn_hard_kill_monitor_after(
    store: Arc<dyn GraphStore>,
    registry: Arc<TaskRunRegistry>,
    task_id: TaskId,
    grace: Duration,
) {
    tokio::spawn(async move {
        tokio::time::sleep(grace).await;
        if !registry.abort_and_remove(task_id) {
            return;
        }
        let (pty, containers) = registry.cleanup_handles();
        if let Some(pty) = pty
            && let Err(err) = pty.close_task(task_id).await
        {
            tracing::warn!(%task_id, error = %err, "hard kill: closing terminals failed");
        }
        if let Some(containers) = containers
            && let Err(err) = containers.remove(task_id).await
        {
            tracing::warn!(%task_id, error = %err, "hard kill: removing container failed");
        }
        match store
            .set_task_state_guarded(task_id, TaskState::Cancelled)
            .await
        {
            Ok(true) => {
                if let Err(err) = store
                    .append_activity(
                        task_id,
                        "task_stopped_hard_kill",
                        "task cancelled: hard kill after the stop grace period".to_string(),
                        json!({
                            "source": "stop_request",
                            "hard_kill": true,
                            "grace_seconds": grace.as_secs(),
                        }),
                    )
                    .await
                {
                    tracing::warn!(%task_id, error = %err, "hard kill: activity write failed");
                }
            }
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(%task_id, error = %err, "hard kill: marking cancelled failed");
            }
        }
    });
}

fn blocked_task(id: TaskId, state: impl Into<String>, error: impl Into<String>) -> BlockedTask {
    BlockedTask {
        id,
        state: state.into(),
        error: error.into(),
    }
}

/// `POST /api/tasks/stop`. Every id is processed independently: one bad id
/// never prevents another task from stopping.
async fn stop_tasks(
    State(state): State<AppState>,
    Json(body): Json<TaskIdsRequest>,
) -> Json<StopTasksResponse> {
    let mut stopping = Vec::new();
    let mut blocked = Vec::new();
    for id in body.ids {
        let task = match state.store.get_task(id).await {
            Ok(Some(task)) => task,
            Ok(None) => {
                blocked.push(blocked_task(id, "unknown", format!("task not found: {id}")));
                continue;
            }
            Err(err) => {
                blocked.push(blocked_task(id, "unknown", err.to_string()));
                continue;
            }
        };
        if task.state.is_terminal() {
            blocked.push(blocked_task(
                id,
                task.state.as_str(),
                format!("task {id} is {}; it cannot be stopped", task.state),
            ));
            continue;
        }
        match stop_one_task(&state, &task).await {
            Ok(()) => stopping.push(id),
            Err(err) => blocked.push(blocked_task(id, task.state.as_str(), err.message)),
        }
    }
    Json(StopTasksResponse { stopping, blocked })
}

/// `DELETE /api/tasks/{id}`. Only terminal tasks are deletable, and never
/// while a loop is still registered.
async fn delete_task_route(
    State(state): State<AppState>,
    Path(id): Path<TaskId>,
) -> Result<StatusCode, ApiError> {
    let task = require_task(state.store.as_ref(), id).await?;
    if state.task_runs.contains(task.id) || !task.state.is_terminal() {
        return Err(ApiError::conflict("stop the task first"));
    }
    let mgmt = require_mgmt(&state)?;
    if !mgmt.delete_task(id).await.map_err(ApiError::from)? {
        return Err(ApiError::conflict("stop the task first"));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/tasks/delete`. Each id is checked independently; a blocked id
/// carries its current state and the reason it was refused.
async fn delete_tasks_route(
    State(state): State<AppState>,
    Json(body): Json<TaskIdsRequest>,
) -> Result<Json<DeleteTasksResponse>, ApiError> {
    let mut deleted = Vec::new();
    let mut blocked = Vec::new();
    for id in body.ids {
        let task = match state.store.get_task(id).await {
            Ok(Some(task)) => task,
            Ok(None) => {
                blocked.push(blocked_task(id, "unknown", format!("task not found: {id}")));
                continue;
            }
            Err(err) => {
                blocked.push(blocked_task(id, "unknown", err.to_string()));
                continue;
            }
        };
        if state.task_runs.contains(task.id) || !task.state.is_terminal() {
            blocked.push(blocked_task(
                id,
                task.state.as_str(),
                "stop the task first".to_string(),
            ));
            continue;
        }
        let mgmt = require_mgmt(&state)?;
        match mgmt.delete_task(id).await {
            Ok(true) => deleted.push(id),
            Ok(false) => blocked.push(blocked_task(
                id,
                task.state.as_str(),
                "task is no longer deletable",
            )),
            Err(err) => blocked.push(blocked_task(id, task.state.as_str(), err.to_string())),
        }
    }
    Ok(Json(DeleteTasksResponse { deleted, blocked }))
}

// ---------------------------------------------------------------------------
// Management: LLM profiles, failover pool, guard rules, settings. The store
// is the write-side source of truth; live policy and pool state ride the
// shared snapshots (`AppState.pool`, `AppState.guard`) and every mutating
// route rebuilds them.
// ---------------------------------------------------------------------------

impl From<StoreError> for ApiError {
    fn from(err: StoreError) -> Self {
        match &err {
            StoreError::NotFound(_) => ApiError::not_found(err.to_string()),
            StoreError::InUse(_) => ApiError::conflict(err.to_string()),
            StoreError::Validation(_) => ApiError::bad_request(err.to_string()),
            _ => ApiError::internal(err),
        }
    }
}

/// The management store behind the routes, or a `500` explaining the wiring
/// gap (test states built on the memory store without a `Store`).
fn require_mgmt(state: &AppState) -> Result<&Arc<Store>, ApiError> {
    state
        .mgmt
        .as_ref()
        .ok_or_else(|| ApiError::internal("no management store configured for this app state"))
}

/// A task pin must name an existing profile; `None` (unset) is always fine.
async fn validate_pin(mgmt: &Store, pin: Option<i64>, field: &str) -> Result<(), ApiError> {
    let Some(id) = pin else {
        return Ok(());
    };
    if mgmt.profile_exists(id).await.map_err(ApiError::internal)? {
        return Ok(());
    }
    Err(ApiError::bad_request(format!(
        "{field} references unknown llm profile {id}"
    )))
}

/// The pool snapshot the shared [`LlmPool`] resolves chains from: role
/// defaults first, then every non-excluded profile by priority DESC / id ASC.
pub async fn build_pool_snapshot(mgmt: &Store) -> anyhow::Result<PoolSnapshot> {
    let rows = mgmt.list_profiles_with_keys().await?;
    let settings = mgmt.get_settings().await?;
    let raw: Vec<RawProfile> = rows
        .iter()
        .map(|p| RawProfile {
            id: p.id,
            name: p.name.clone(),
            format: p.format.clone(),
            base_url: p.base_url.clone(),
            model: p.model.clone(),
            api_key: p.api_key.clone(),
            max_tokens: p.max_tokens,
            streaming: p.streaming,
            priority: p.priority,
            pool_exclude: p.pool_exclude,
            thinking_type: p.thinking_type.clone(),
            reasoning_effort: p.reasoning_effort.clone(),
        })
        .collect();
    let (planner_active, worker_active) = settings
        .map(|s| (s.planner_profile_id, s.worker_profile_id))
        .unwrap_or((0, 0));
    Ok(PoolSnapshot::from_rows(&raw, planner_active, worker_active))
}

/// The compiled guard snapshot: enabled rules by priority DESC plus the
/// settings default action and approval timeout.
pub async fn build_guard_policy(mgmt: &Store) -> anyhow::Result<CompiledPolicy> {
    let rules = mgmt.list_enabled_rules().await?;
    let settings = mgmt.get_settings().await?;
    let specs: Vec<RuleSpec> = rules
        .iter()
        .map(|rule| RuleSpec {
            name: rule.name.clone(),
            match_target: rule.match_target.clone(),
            match_type: rule.match_type.clone(),
            pattern: rule.pattern.clone(),
            action: parse_verdict(&rule.action),
            reason: rule.reason.clone(),
            priority: rule.priority,
            timeout_secs: i64::from(rule.timeout_secs),
        })
        .collect();
    let (default, timeout) = settings
        .map(|s| {
            (
                parse_verdict(&s.guard_default_action),
                i64::from(s.approval_timeout_secs),
            )
        })
        .unwrap_or((GuardVerdict::Deny, 0));
    Ok(CompiledPolicy::build(
        "db-rules-v1",
        default,
        timeout,
        &specs,
    ))
}

fn parse_verdict(raw: &str) -> GuardVerdict {
    match raw {
        "allow" => GuardVerdict::Allow,
        "ask" => GuardVerdict::Ask,
        _ => GuardVerdict::Deny,
    }
}

/// Rebuilds and publishes the pool snapshot from the store.
async fn refresh_pool(state: &AppState, mgmt: &Store) -> Result<(), ApiError> {
    let snapshot = build_pool_snapshot(mgmt)
        .await
        .map_err(ApiError::internal)?;
    state.pool.swap(snapshot);
    Ok(())
}

/// Rebuilds and publishes the guard snapshot from the store. Also used after
/// settings writes: they change the default action and the approval timeout.
async fn refresh_guard(state: &AppState, mgmt: &Store) -> Result<(), ApiError> {
    let policy = build_guard_policy(mgmt).await.map_err(ApiError::internal)?;
    state.guard.swap(policy);
    Ok(())
}

// ---------- llm profiles ----------

/// The flattened profile DTO a read route returns: no `api_key`, hint only.
#[derive(Debug, Serialize)]
struct ProfileDto {
    id: i64,
    name: String,
    format: String,
    base_url: String,
    model: String,
    api_key_hint: String,
    max_tokens: i32,
    context_window_k: i32,
    streaming: bool,
    priority: i32,
    pool_exclude: bool,
    thinking_type: String,
    reasoning_effort: String,
    created_at: String,
    updated_at: String,
}

fn profile_dtos(rows: Vec<dhi_store::mgmt::LlmProfileOut>) -> Vec<ProfileDto> {
    rows.into_iter()
        .map(|p| ProfileDto {
            id: p.id,
            name: p.name,
            format: p.format,
            base_url: p.base_url,
            model: p.model,
            api_key_hint: p.api_key_hint,
            max_tokens: p.max_tokens,
            context_window_k: p.context_window_k,
            streaming: p.streaming,
            priority: p.priority,
            pool_exclude: p.pool_exclude,
            thinking_type: p.thinking_type,
            reasoning_effort: p.reasoning_effort,
            created_at: p.created_at.to_rfc3339(),
            updated_at: p.updated_at.to_rfc3339(),
        })
        .collect()
}

/// `GET /api/llm/profiles`: every profile without its key.
async fn list_profiles_route(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let mgmt = require_mgmt(&state)?;
    let profiles = mgmt.list_profiles().await.map_err(ApiError::from)?;
    Ok(Json(json!({ "profiles": profile_dtos(profiles) })))
}

/// `POST /api/llm/profiles` input. `api_key` is write-only; on edit an absent
/// key keeps the stored one.
#[derive(Deserialize)]
struct SaveProfileRequest {
    id: Option<i64>,
    name: String,
    format: String,
    #[serde(default)]
    base_url: String,
    model: String,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    max_tokens: i32,
    #[serde(default)]
    context_window_k: i32,
    #[serde(default = "default_true")]
    streaming: bool,
    #[serde(default)]
    priority: i32,
    #[serde(default)]
    pool_exclude: bool,
    #[serde(default)]
    thinking_type: String,
    #[serde(default)]
    reasoning_effort: String,
}

/// The submitted key is a secret; never let `Debug` logs echo it.
impl std::fmt::Debug for SaveProfileRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SaveProfileRequest")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("format", &self.format)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("max_tokens", &self.max_tokens)
            .field("context_window_k", &self.context_window_k)
            .field("streaming", &self.streaming)
            .field("priority", &self.priority)
            .field("pool_exclude", &self.pool_exclude)
            .field("thinking_type", &self.thinking_type)
            .field("reasoning_effort", &self.reasoning_effort)
            .finish()
    }
}

fn default_true() -> bool {
    true
}

/// `POST /api/llm/profiles`: create (no `id`) or edit (`id` present).
/// Side effect: the LLM pool snapshot is rebuilt either way — any key, model,
/// priority, or `pool_exclude` edit reshapes the failover chains.
async fn save_profile_route(
    State(state): State<AppState>,
    Json(body): Json<SaveProfileRequest>,
) -> Result<Json<Value>, ApiError> {
    let mgmt = require_mgmt(&state)?;
    let upsert = LlmProfileUpsert {
        id: body.id,
        name: body.name,
        format: body.format,
        base_url: body.base_url,
        model: body.model,
        api_key: body.api_key.unwrap_or_default(),
        max_tokens: body.max_tokens,
        context_window_k: body.context_window_k,
        streaming: body.streaming,
        priority: body.priority,
        pool_exclude: body.pool_exclude,
        thinking_type: body.thinking_type,
        reasoning_effort: body.reasoning_effort,
    };
    let id = mgmt.save_profile(&upsert).await.map_err(ApiError::from)?;
    refresh_pool(&state, mgmt).await?;
    Ok(Json(json!({ "id": id })))
}

/// `DELETE /api/llm/profiles/{id}`: `404` unknown, `409` when the profile is
/// an active role default or pinned by any task. The breaker state of a
/// deleted profile is meaningless, so it is cleared alongside the snapshot
/// refresh (ARTEX `s.llmHealth.Reset(id)`).
async fn delete_profile_route(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    let mgmt = require_mgmt(&state)?;
    mgmt.delete_profile(id).await.map_err(ApiError::from)?;
    refresh_pool(&state, mgmt).await?;
    state.pool.reset(Some(id));
    Ok(Json(json!({ "deleted": id })))
}

/// `POST /api/llm/profiles/active` input.
#[derive(Debug, Deserialize)]
struct ActivateProfileRequest {
    id: i64,
    role: String,
}

/// `POST /api/llm/profiles/active`: points one role's default at a profile
/// and rebuilds the pool snapshot (the activated profile now heads the chain).
async fn activate_profile_route(
    State(state): State<AppState>,
    Json(body): Json<ActivateProfileRequest>,
) -> Result<Json<Value>, ApiError> {
    let mgmt = require_mgmt(&state)?;
    if body.role != "planner" && body.role != "worker" {
        return Err(ApiError::bad_request(format!(
            "unknown role {:?}: must be \"planner\" or \"worker\"",
            body.role
        )));
    }
    mgmt.set_active_profile(&body.role, body.id)
        .await
        .map_err(ApiError::from)?;
    refresh_pool(&state, mgmt).await?;
    Ok(Json(json!({ "ok": true })))
}

/// `GET /api/llm/models?profile_id=N`: proxies `GET {base_url}/models` with
/// the profile's bearer key. OpenAI-format only. The key is never logged nor
/// returned; upstream failures are `502`.
#[derive(Debug, Deserialize)]
struct ModelsQuery {
    profile_id: Option<i64>,
}

async fn list_models_route(
    State(state): State<AppState>,
    Query(query): Query<ModelsQuery>,
) -> Result<Json<Value>, ApiError> {
    let mgmt = require_mgmt(&state)?.clone();
    let profile = match query.profile_id {
        Some(id) => mgmt
            .get_profile_with_key(id)
            .await
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found(format!("llm profile {id}")))?,
        None => {
            let settings = mgmt.get_settings().await.map_err(ApiError::internal)?;
            let planner_id = settings
                .map(|s| s.planner_profile_id)
                .ok_or_else(|| ApiError::not_found("app settings are not initialized"))?;
            mgmt.get_profile_with_key(planner_id)
                .await
                .map_err(ApiError::internal)?
                .ok_or_else(|| ApiError::not_found(format!("llm profile {planner_id}")))?
        }
    };
    let resolved = ResolvedProfile {
        id: profile.id,
        name: profile.name.clone(),
        format: dhi_agent::ProfileFormat::parse(&profile.format),
        base_url: profile.base_url.clone(),
        model: profile.model.clone(),
        api_key: profile.api_key.clone(),
        max_tokens: i64::from(profile.max_tokens),
        streaming: profile.streaming,
        thinking_type: profile.thinking_type.clone(),
        reasoning_effort: profile.reasoning_effort.clone(),
    };
    if resolved.format != dhi_agent::ProfileFormat::Openai {
        return Err(ApiError::bad_request(
            "model listing supports the openai format only",
        ));
    }
    if resolved.api_key.trim().is_empty() {
        return Err(ApiError::bad_request(
            "the profile has no api key; save one first",
        ));
    }
    if resolved.base_url.trim().is_empty() {
        return Err(ApiError::bad_request("the profile has no base_url"));
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|err| ApiError::internal(format!("http client: {err}")))?;
    let key = resolved.api_key.trim();
    let url = format!("{}/models", resolved.base_url.trim_end_matches('/'));
    let response = client
        .get(&url)
        .bearer_auth(key)
        .send()
        .await
        .map_err(|err| {
            ApiError::bad_gateway(redact(format!("model list request failed: {err}"), key))
        })?;
    let status = response.status();
    let body: Value = response.json().await.map_err(|err| {
        ApiError::bad_gateway(redact(
            format!("model list response was not JSON: {err}"),
            key,
        ))
    })?;
    if !status.is_success() {
        return Err(ApiError::bad_gateway(format!(
            "model list returned {status}: {}",
            short_message(&body, key)
        )));
    }
    let models: Vec<String> = body
        .get("data")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("id").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Ok(Json(json!({ "models": models })))
}

/// The truncated upstream snippet a 502 carries, with every occurrence of the
/// profile's API key redacted BEFORE truncation: gateways occasionally echo
/// the bearer token back inside an error payload, and a cut could otherwise
/// split the key beyond recognition.
fn short_message(body: &Value, secret: &str) -> String {
    let text = body.to_string();
    let text = if secret.is_empty() {
        text
    } else {
        text.replace(secret, "[redacted]")
    };
    text.chars().take(200).collect()
}

/// Scrubs every occurrence of `secret` from an error message. Empty secrets
/// pass through untouched.
fn redact(text: String, secret: &str) -> String {
    if secret.is_empty() {
        text
    } else {
        text.replace(secret, "[redacted]")
    }
}

// ---------- failover pool ----------

/// `GET /api/llm/pool/status`: chain order plus per-profile breaker state.
async fn pool_status_route(State(state): State<AppState>) -> Json<Value> {
    Json(
        serde_json::to_value(state.pool.status()).unwrap_or_else(|_| {
            json!({
                "planner": {"chain": []},
                "worker": {"chain": []},
            })
        }),
    )
}

/// `POST /api/llm/pool/reset` input: `id > 0` clears one breaker, `0`/omitted
/// clears every profile.
#[derive(Debug, Deserialize, Default)]
struct PoolResetRequest {
    #[serde(default)]
    id: i64,
}

async fn pool_reset_route(
    State(state): State<AppState>,
    body: Option<Json<PoolResetRequest>>,
) -> Json<Value> {
    let id = body.map(|Json(body)| body.id).unwrap_or(0);
    state.pool.reset((id > 0).then_some(id));
    Json(
        serde_json::to_value(state.pool.status()).unwrap_or_else(|_| {
            json!({
                "planner": {"chain": []},
                "worker": {"chain": []},
            })
        }),
    )
}

// ---------- guard rules ----------

/// `GET /api/intercept/rules`: every rule, evaluation order.
async fn list_rules_route(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let mgmt = require_mgmt(&state)?;
    let rules = mgmt.list_rules().await.map_err(ApiError::from)?;
    Ok(Json(json!({ "rules": rules })))
}

/// The write body of a guard rule (create or full replace).
#[derive(Debug, Deserialize)]
struct RuleRequest {
    name: String,
    match_target: String,
    match_type: String,
    pattern: String,
    action: String,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    priority: i32,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    timeout_secs: i32,
}

impl TryFrom<RuleRequest> for GuardRuleInput {
    type Error = ApiError;

    fn try_from(request: RuleRequest) -> Result<Self, ApiError> {
        let input = Self {
            name: request.name,
            match_target: request.match_target,
            match_type: request.match_type,
            pattern: request.pattern,
            action: request.action,
            reason: request.reason,
            priority: request.priority,
            enabled: request.enabled,
            timeout_secs: request.timeout_secs,
        };
        input.validate().map_err(ApiError::from)?;
        // Regex patterns are validated at the API layer; the snapshot builder
        // merely skips a pattern that slipped through.
        if input.match_type == "regex" {
            regex::Regex::new(input.pattern.trim())
                .map_err(|err| ApiError::bad_request(format!("invalid regex pattern: {err}")))?;
        }
        Ok(input)
    }
}

/// `POST /api/intercept/rules`: creates one rule; the guard snapshot is
/// rebuilt so the next evaluation sees it.
async fn create_rule_route(
    State(state): State<AppState>,
    Json(body): Json<RuleRequest>,
) -> Result<Json<GuardRuleRow>, ApiError> {
    let mgmt = require_mgmt(&state)?;
    let input = GuardRuleInput::try_from(body)?;
    let rule = mgmt.create_rule(&input).await.map_err(ApiError::from)?;
    refresh_guard(&state, mgmt).await?;
    Ok(Json(rule))
}

/// `PUT /api/intercept/rules/{id}`: full replace; `404` unknown.
async fn update_rule_route(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<RuleRequest>,
) -> Result<Json<GuardRuleRow>, ApiError> {
    let mgmt = require_mgmt(&state)?;
    let input = GuardRuleInput::try_from(body)?;
    let rule = mgmt.update_rule(id, &input).await.map_err(ApiError::from)?;
    refresh_guard(&state, mgmt).await?;
    Ok(Json(rule))
}

/// `DELETE /api/intercept/rules/{id}`: `404` unknown.
async fn delete_rule_route(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    let mgmt = require_mgmt(&state)?;
    mgmt.delete_rule(id).await.map_err(ApiError::from)?;
    refresh_guard(&state, mgmt).await?;
    Ok(Json(json!({ "deleted": id })))
}

/// `POST /api/intercept/rules/{id}/toggle` input.
#[derive(Debug, Deserialize)]
struct ToggleRuleRequest {
    enabled: bool,
}

async fn toggle_rule_route(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<ToggleRuleRequest>,
) -> Result<Json<Value>, ApiError> {
    let mgmt = require_mgmt(&state)?;
    let rule = mgmt
        .toggle_rule(id, body.enabled)
        .await
        .map_err(ApiError::from)?;
    refresh_guard(&state, mgmt).await?;
    Ok(Json(json!({ "ok": true, "enabled": rule.enabled })))
}

// ---------- settings ----------

/// `GET /api/settings` output. `llm_key_set` is true when BOTH active
/// profiles carry a non-empty key. Hints only: no key ever leaves the server.
///
/// Before the first profile exists (no seeded settings row) this answers
/// `200` with null roles and empty hints — ARTEX's settings endpoint always
/// 200s, and a permanent 404 would strand a mock-mode boot's first client.
#[derive(Debug, Serialize)]
struct SettingsView {
    planner_profile: Option<RoleSummary>,
    worker_profile: Option<RoleSummary>,
    llm_key_set: bool,
    planner_api_key_hint: String,
    worker_api_key_hint: String,
    approval_timeout_secs: i32,
    guard_default_action: String,
}

#[derive(Debug, Serialize)]
struct RoleSummary {
    id: i64,
    name: String,
    model: String,
}

impl SettingsView {
    async fn load(mgmt: &Store) -> Result<Self, ApiError> {
        let Some(settings) = mgmt.get_settings().await.map_err(ApiError::internal)? else {
            return Ok(Self::unseeded());
        };
        let planner = mgmt
            .get_profile_with_key(settings.planner_profile_id)
            .await
            .map_err(ApiError::internal)?
            .ok_or_else(|| {
                ApiError::not_found(format!("llm profile {}", settings.planner_profile_id))
            })?;
        let worker = mgmt
            .get_profile_with_key(settings.worker_profile_id)
            .await
            .map_err(ApiError::internal)?
            .ok_or_else(|| {
                ApiError::not_found(format!("llm profile {}", settings.worker_profile_id))
            })?;
        Ok(Self {
            planner_profile: Some(RoleSummary {
                id: planner.id,
                name: planner.name.clone(),
                model: planner.model.clone(),
            }),
            worker_profile: Some(RoleSummary {
                id: worker.id,
                name: worker.name.clone(),
                model: worker.model.clone(),
            }),
            llm_key_set: !planner.api_key.trim().is_empty() && !worker.api_key.trim().is_empty(),
            planner_api_key_hint: planner.api_key_hint,
            worker_api_key_hint: worker.api_key_hint,
            approval_timeout_secs: settings.approval_timeout_secs,
            guard_default_action: settings.guard_default_action,
        })
    }

    /// The all-null view an empty management database gets, with the same
    /// defaults `seed_defaults` and [`Store::save_profile`] would have seeded.
    fn unseeded() -> Self {
        Self {
            planner_profile: None,
            worker_profile: None,
            llm_key_set: false,
            planner_api_key_hint: String::new(),
            worker_api_key_hint: String::new(),
            approval_timeout_secs: DEFAULT_APPROVAL_TIMEOUT_SECS,
            guard_default_action: "deny".to_string(),
        }
    }
}

/// `PUT /api/settings` input: every field optional (partial merge).
#[derive(Debug, Deserialize, Default)]
struct UpdateSettingsRequest {
    planner_profile_id: Option<i64>,
    worker_profile_id: Option<i64>,
    approval_timeout_secs: Option<i32>,
    /// `allow` | `deny` | `ask` (400 otherwise) — the default verdict for
    /// tool calls no rule matches.
    guard_default_action: Option<String>,
}

/// `GET /api/settings`.
async fn get_settings_route(State(state): State<AppState>) -> Result<Json<SettingsView>, ApiError> {
    let mgmt = require_mgmt(&state)?;
    Ok(Json(SettingsView::load(mgmt).await?))
}

/// `PUT /api/settings`: partial merge, then rebuild BOTH snapshots — the
/// guard default action / approval timeout ride the guard snapshot, the role
/// activations reshape the pool chains.
async fn put_settings_route(
    State(state): State<AppState>,
    Json(body): Json<UpdateSettingsRequest>,
) -> Result<Json<SettingsView>, ApiError> {
    let mgmt = require_mgmt(&state)?;
    let patch = SettingPatch {
        planner_profile_id: body.planner_profile_id,
        worker_profile_id: body.worker_profile_id,
        approval_timeout_secs: body.approval_timeout_secs,
        guard_default_action: body.guard_default_action,
    };
    mgmt.update_settings(&patch).await.map_err(ApiError::from)?;
    refresh_guard(&state, mgmt).await?;
    refresh_pool(&state, mgmt).await?;
    Ok(Json(SettingsView::load(mgmt).await?))
}

async fn list_tasks(
    State(state): State<AppState>,
    Query(query): Query<ListTasksQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = page_limit(query.limit)?;
    let tasks = state
        .store
        .list_tasks(limit)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({ "tasks": tasks })))
}

async fn get_task(
    State(state): State<AppState>,
    Path(id): Path<TaskId>,
) -> Result<Json<Task>, ApiError> {
    let task = require_task(state.store.as_ref(), id).await?;
    Ok(Json(task))
}

async fn get_task_work(
    State(state): State<AppState>,
    Path(id): Path<TaskId>,
) -> Result<Json<TaskWork>, ApiError> {
    let task = require_task(state.store.as_ref(), id).await?;
    let store = state.store.as_ref();
    let goals = store.list_goals(id).await.map_err(ApiError::internal)?;
    let intents = store
        .list_intents(id, WORK_LIMIT)
        .await
        .map_err(ApiError::internal)?;
    let facts = store.list_facts(id).await.map_err(ApiError::internal)?;
    let results = store
        .list_results(id, WORK_LIMIT)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(TaskWork {
        task,
        goals,
        intents,
        facts,
        results,
    }))
}

async fn get_task_activity(
    State(state): State<AppState>,
    Path(id): Path<TaskId>,
) -> Result<Json<Value>, ApiError> {
    require_task(state.store.as_ref(), id).await?;
    let activity = state
        .store
        .list_activity(id, None)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({ "activity": activity })))
}

/// `GET /api/tasks/{id}/usage`: every completion call the task made.
async fn get_task_usage(
    State(state): State<AppState>,
    Path(id): Path<TaskId>,
) -> Result<Json<Value>, ApiError> {
    require_task(state.store.as_ref(), id).await?;
    let runs = state
        .store
        .list_agent_runs(id)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({ "runs": runs })))
}

/// `GET /api/tasks/{id}/swarm-messages`: the DHI+ swarm audit log of a task,
/// ordered by id. `limit` defaults to 200 and is clamped to `1..=1000`.
async fn get_task_swarm_messages(
    State(state): State<AppState>,
    Path(id): Path<TaskId>,
    Query(query): Query<SwarmMessagesQuery>,
) -> Result<Json<Value>, ApiError> {
    require_task(state.store.as_ref(), id).await?;
    let limit = query
        .limit
        .unwrap_or(DEFAULT_SWARM_MESSAGE_LIMIT)
        .clamp(1, MAX_SWARM_MESSAGE_LIMIT);
    let messages = state
        .store
        .list_swarm_messages(id, limit)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({ "messages": messages })))
}

/// `GET /api/approvals`: the pending approval queue of every task, enriched
/// with the tool name and input of each linked tool call.
///
/// A missing tool call row leaves `tool_name` and `input` as `null` rather
/// than failing the queue read.
async fn list_approvals(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let approvals = state
        .guard_store
        .list_pending_approvals_all()
        .await
        .map_err(ApiError::internal)?;
    let mut items = Vec::with_capacity(approvals.len());
    for approval in approvals {
        let call = state
            .guard_store
            .get_tool_call(approval.tool_call_id)
            .await
            .map_err(ApiError::internal)?;
        let (tool_name, input) = match call {
            Some(call) => (Some(call.tool_name), Some(call.input)),
            None => (None, None),
        };
        items.push(ApprovalItem {
            approval,
            tool_name,
            input,
        });
    }
    Ok(Json(json!({ "approvals": items })))
}

/// `POST /api/approvals/{id}/decision`: resolves a pending approval and
/// wakes the worker waiting on it.
///
/// The decision only resolves the durable row; the worker continues inside its
/// own `run_task` and runs the tool (or records the denial). This route never
/// executes the tool, never finishes an intent, and never pauses or restarts
/// the task. `404` means the approval is unknown; `409` means it is no longer
/// pending.
async fn decide_approval(
    State(state): State<AppState>,
    Path(id): Path<dhi_core::ApprovalId>,
    Json(body): Json<ApprovalDecisionRequest>,
) -> Result<Json<ApprovalDecisionResponse>, ApiError> {
    let approval = state
        .guard_store
        .get_approval(id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("approval not found: {id}")))?;
    if approval.status != ApprovalStatus::Pending {
        return Err(ApiError::conflict(format!(
            "approval {id} is not pending: {}",
            approval.status.as_str()
        )));
    }
    let task_id = approval.task_id;

    let resolved = state
        .engine
        .decide_approval(id, body.decision)
        .await
        .map_err(ApiError::internal)?;
    let Some(approval) = resolved else {
        // Another decision or the expiry timer won between the read above and
        // the write. Report the row's current state rather than pretending the
        // decision applied.
        return match state
            .guard_store
            .get_approval(id)
            .await
            .map_err(ApiError::internal)?
        {
            None => Err(ApiError::not_found(format!("approval not found: {id}"))),
            Some(current) => Err(ApiError::conflict(format!(
                "approval {id} is not pending: {}",
                current.status.as_str()
            ))),
        };
    };
    if !matches!(
        approval.status,
        ApprovalStatus::Granted | ApprovalStatus::Denied
    ) {
        return Err(ApiError::conflict(format!(
            "approval {id} is not pending: {}",
            approval.status.as_str()
        )));
    }

    let task = require_task(state.store.as_ref(), task_id).await?;
    Ok(Json(ApprovalDecisionResponse { approval, task }))
}

/// `GET /api/stream?task_id=<uuid>&after=<id>`: the task's activity as it lands.
///
/// Answers with `text/event-stream`. The first frame is a `hello` naming the
/// task. After that the handler polls the append-only activity log every
/// [`STREAM_POLL_INTERVAL`] and emits one `activity` event per row with an id
/// greater than both `after` (exclusive resume point) and the last id sent,
/// sending every new row found on each poll in ascending `id` order.
/// Keep-alive comments hold the connection open while nothing happens.
async fn stream_activity(
    State(state): State<AppState>,
    Query(query): Query<StreamQuery>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let task_id: TaskId = query
        .task_id
        .parse()
        .map_err(|_| ApiError::bad_request(format!("invalid task_id: {}", query.task_id)))?;
    require_task(state.store.as_ref(), task_id).await?;

    let after = query.after.unwrap_or(0).max(0);
    let store = state.store.clone();
    let hello = Event::default()
        .event("hello")
        .data(json!({ "task_id": task_id.to_string() }).to_string());

    let polls = stream::unfold(
        (store, task_id, after),
        |(store, task_id, mut last_sent)| async move {
            loop {
                tokio::time::sleep(STREAM_POLL_INTERVAL).await;
                let rows = match store.list_activity(task_id, None).await {
                    Ok(rows) => rows,
                    Err(err) => {
                        tracing::warn!(%task_id, error = %err, "activity stream poll failed");
                        continue;
                    }
                };
                let mut frames = Vec::new();
                for activity in rows {
                    if activity.id <= last_sent {
                        continue;
                    }
                    last_sent = activity.id;
                    let frame = Event::default()
                        .event("activity")
                        .id(last_sent.to_string())
                        .data(
                            serde_json::to_string(&activity)
                                .unwrap_or_else(|err| format!(r#"{{"error":"{err}"}}"#)),
                        );
                    frames.push(Ok(frame));
                }
                if !frames.is_empty() {
                    return Some((stream::iter(frames), (store, task_id, last_sent)));
                }
            }
        },
    )
    .flatten();
    let body = stream::once(async move { Ok(hello) }).chain(polls);

    Ok(Sse::new(body).keep_alive(KeepAlive::default()))
}

/// The tool backend the server ships with: none.
///
/// Every tool call therefore fails closed with `no tool backend configured`
/// instead of reaching an unguarded tool. A domain backend replaces it when
/// one is configured.
pub struct NoopBackend;

#[async_trait]
impl ToolBackend for NoopBackend {
    async fn execute(
        &self,
        _tool: &str,
        _input: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        Err(anyhow::anyhow!("no tool backend configured"))
    }
}

/// The guarded executor as the agent layer's in-loop [`dhi_agent::ToolSink`].
///
/// A thin adapter on purpose: every in-loop tool call keeps the exact
/// post-hoc semantics — record call, evaluate the shared policy snapshot,
/// block on `Ask` until a decision or expiry, then run the backend or deny.
/// The agent crate never depends on the guard; the server owns the join.
pub struct GuardedToolSink {
    executor: Arc<GuardedExecutor>,
    activity: Option<Arc<dyn dhi_core::GraphStore>>,
}

impl GuardedToolSink {
    /// Wraps the one guard executor. Without an activity store the sink is
    /// silent (tests and hosts that only need dispatch).
    pub fn new(executor: Arc<GuardedExecutor>) -> Self {
        Self {
            executor,
            activity: None,
        }
    }

    /// Wraps the guard and mirrors each in-loop outcome onto the activity
    /// feed: `tool_allowed` / `tool_denied`, with the same shape the engine's
    /// post-hoc path writes. Best effort — a failed append never breaks the
    /// tool call.
    pub fn with_activity(
        executor: Arc<GuardedExecutor>,
        activity: Arc<dyn dhi_core::GraphStore>,
    ) -> Self {
        Self {
            executor,
            activity: Some(activity),
        }
    }
}

/// Caps one activity payload's output preview. The durable `tool_calls` row
/// keeps the full output; the feed only needs the step and a short preview.
fn activity_output(output: &serde_json::Value) -> serde_json::Value {
    const MAX_CHARS: usize = 500;
    let text = match output {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    if text.chars().count() <= MAX_CHARS {
        output.clone()
    } else {
        json!({
            "truncated": true,
            "preview": text.chars().take(MAX_CHARS).collect::<String>(),
        })
    }
}

#[async_trait]
impl dhi_agent::ToolSink for GuardedToolSink {
    async fn execute(
        &self,
        task_id: TaskId,
        intent_id: Uuid,
        request: dhi_core::ToolCallRequest,
    ) -> anyhow::Result<dhi_core::ToolCallOutcome> {
        let tool = request.tool.clone();
        let outcome = self.executor.execute(task_id, intent_id, request).await?;
        if let Some(store) = &self.activity {
            let event = match &outcome {
                dhi_core::ToolCallOutcome::Allowed { output } => Some((
                    "tool_allowed",
                    format!("tool {tool} executed"),
                    json!({
                        "intent_id": intent_id,
                        "tool": tool,
                        "output": activity_output(output),
                    }),
                )),
                dhi_core::ToolCallOutcome::Denied { reason } => Some((
                    "tool_denied",
                    format!("tool {tool} denied: {reason}"),
                    json!({
                        "intent_id": intent_id,
                        "tool": tool,
                        "reason": reason,
                    }),
                )),
                dhi_core::ToolCallOutcome::Pending { .. } => None,
            };
            if let Some((kind, message, payload)) = event
                && let Err(err) = store.append_activity(task_id, kind, message, payload).await
            {
                tracing::warn!(
                    %task_id,
                    tool = %tool,
                    kind,
                    error = %err,
                    "tool activity event failed to record; continuing"
                );
            }
        }
        Ok(outcome)
    }
}

/// The specialist audit as the agent layer's [`dhi_agent::RuntimeSink`].
///
/// One specialist completion call becomes exactly one `agent_runs` row:
/// role `worker`, tagged `specialist:<name>` via `variables` (the `role`
/// column is the typed security vocabulary, so the tag rides `variables`).
/// `agent_delegated` / `agent_returned` become activity rows. Both writes
/// are best effort: a failed audit write logs and never breaks a run — the
/// contract the sink trait hands dhi-agent.
pub struct SpecialistAuditSink(Arc<dyn GraphStore>);

impl SpecialistAuditSink {
    /// Wraps the store the same engine and API already use.
    pub fn new(store: Arc<dyn GraphStore>) -> Self {
        Self(store)
    }
}

#[async_trait]
impl dhi_agent::RuntimeSink for SpecialistAuditSink {
    async fn record_specialist_run(&self, record: dhi_agent::SpecialistRunRecord) {
        let run = agent_run_for_specialist(&record);
        if let Err(err) = self.0.save_agent_run(&run).await {
            tracing::warn!(
                specialist = %record.specialist,
                error = %err,
                "specialist usage row failed to record; continuing"
            );
        }
    }

    async fn record_event(
        &self,
        task_id: TaskId,
        kind: &str,
        message: String,
        payload: serde_json::Value,
    ) {
        if let Err(err) = self
            .0
            .append_activity(task_id, kind, message, payload)
            .await
        {
            tracing::warn!(
                task_id = %task_id,
                kind = %kind,
                error = %err,
                "specialist activity event failed to record; continuing"
            );
        }
    }
}

/// The mapping: one specialist completion call → one `agent_runs` row.
///
/// `run_id` ties every retry of the delegation call together (fresh per
/// delegation, like every completion row); the row id is fresh. The tag
/// `specialist:<name>` lives in `variables` next to the plain name.
fn agent_run_for_specialist(record: &dhi_agent::SpecialistRunRecord) -> AgentRun {
    let now = chrono::Utc::now();
    let mut hasher = sha2::Sha256::new();
    sha2::Digest::update(&mut hasher, record.prompt_text.as_bytes());
    let prompt_hash: String = sha2::Digest::finalize(hasher)[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    AgentRun {
        id: Uuid::new_v4(),
        run_id: record.run_id,
        call_index: 0,
        task_id: record.task_id,
        intent_id: Some(record.intent_id),
        worker_key: Some(format!("specialist:{}", record.specialist)),
        role: AgentRole::Worker,
        model: record.model.clone(),
        profile: record.profile.clone(),
        prompt: record.prompt_text.clone(),
        prompt_hash,
        variables: json!({
            "specialist": record.specialist,
            "tag": format!("specialist:{}", record.specialist),
        }),
        response: record.response.clone(),
        thinking: record.thinking.clone(),
        tokens_in: record.usage.input_tokens,
        tokens_out: record.usage.output_tokens,
        cache_read_tokens: record.usage.cache_read_tokens,
        cache_write_tokens: record.usage.cache_write_tokens,
        reasoning_tokens: record.usage.reasoning_tokens,
        cost: record.usage.cost_usd.unwrap_or(0.0),
        status: record.status.clone(),
        error: record.error.clone(),
        latency_ms: record.latency_ms,
        checkpoint_id: None,
        created_at: now,
        updated_at: now,
    }
}

/// One supervisor pass: list tasks and sweep every non-terminal one.
///
/// An individual task failure is logged and skipped; it never stops the pass
/// or the supervisor. Returns how many tasks were swept.
pub async fn sweep_all(store: &Arc<dyn GraphStore>, engine: &Engine) -> anyhow::Result<usize> {
    let tasks = store.list_tasks(SUPERVISOR_TASK_LIMIT).await?;
    let mut swept = 0;
    for task in tasks {
        if task.state.is_terminal() {
            continue;
        }
        match engine.sweep_approvals(task.id).await {
            Ok(()) => swept += 1,
            Err(err) => {
                tracing::warn!(
                    task_id = %task.id,
                    state = %task.state,
                    error = %err,
                    "approval sweep failed; moving on to the next task"
                );
            }
        }
    }
    Ok(swept)
}

/// One restart recovery pass, run once before the process serves traffic.
///
/// Steps, in order:
/// 1. Deny every pending approval. The waiter that belonged to the previous
///    process is gone; nothing can resolve the approval now. Each linked tool
///    call is completed as `denied`, and an `approval_denied` activity row
///    names the approval and the reason `service restart`.
/// 2. Reset every `running` intent to `open`, clearing its owner, so the
///    released work can be claimed again.
/// 3. Restart the task loop of every non-terminal task except `paused`, each
///    in its own spawned task. A failed run is logged and never fails startup.
/// 4. Registry-aware recovery additionally fails any remaining `running` task
///    that has no registered loop, so no row is left with a dead worker.
///
/// Returns how many task loops were restarted. A failure while denying an
/// approval or resetting intents aborts recovery: the caller must not serve a
/// half-recovered process. A crashed worker's transcript is not resumed; the
/// restarted task starts a new run from its durable state.
pub async fn recover_tasks(
    store: &Arc<dyn GraphStore>,
    guard_store: &Arc<dyn GuardStore>,
    engine: &Arc<Engine>,
) -> anyhow::Result<usize> {
    recover_inner(store, guard_store, engine, None).await
}

/// Registry-aware restart recovery used by `main.rs`.
///
/// It performs the same approval and intent recovery as [`recover_tasks`],
/// but every restarted loop is registered with its stop flag and abort
/// handle. After registration a reconciliation pass fails any task that is
/// still `running` without a registry entry, so a restart can never leave a
/// `running` row with no worker.
pub async fn recover_tasks_with_registry(
    store: &Arc<dyn GraphStore>,
    guard_store: &Arc<dyn GuardStore>,
    engine: &Arc<Engine>,
    registry: &Arc<TaskRunRegistry>,
) -> anyhow::Result<usize> {
    recover_inner(store, guard_store, engine, Some(registry)).await
}

async fn recover_inner(
    store: &Arc<dyn GraphStore>,
    guard_store: &Arc<dyn GuardStore>,
    engine: &Arc<Engine>,
    registry: Option<&Arc<TaskRunRegistry>>,
) -> anyhow::Result<usize> {
    let pending = guard_store.list_pending_approvals_all().await?;
    let mut denied = 0;
    for approval in pending {
        let resolved = guard_store
            .decide_approval(approval.id, ApprovalDecision::Deny)
            .await?;
        if resolved.is_none() {
            anyhow::bail!(
                "approval {} is no longer pending during restart recovery",
                approval.id
            );
        }
        guard_store
            .complete_tool_call(approval.tool_call_id, None, "denied")
            .await?;
        store
            .append_activity(
                approval.task_id,
                "approval_denied",
                format!("approval {} denied: service restart", approval.id),
                json!({
                    "approval_id": approval.id,
                    "tool_call_id": approval.tool_call_id,
                    "reason": "service restart",
                }),
            )
            .await?;
        denied += 1;
    }
    tracing::info!(
        denied,
        "denied pending approvals left by a previous process"
    );

    let reset = store.reset_running_intents().await?;
    tracing::info!(reset, "reset running intents left by a previous process");

    let tasks = store.list_tasks(RECOVERY_TASK_LIMIT).await?;
    let mut restarted = 0;
    for task in tasks {
        if task.state.is_terminal() || task.state == TaskState::Paused {
            continue;
        }
        match registry {
            Some(registry) => spawn_task_loop(store, registry, engine, task.id),
            None => {
                let engine = engine.clone();
                let store = store.clone();
                let task_id = task.id;
                tokio::spawn(async move {
                    if let Err(err) = engine.run_task(task_id).await {
                        tracing::warn!(%task_id, error = %err, "restarted task run failed");
                        fail_task_from_server(
                            store.as_ref(),
                            task_id,
                            format!("task run failed: {err}"),
                            "recovery",
                        )
                        .await;
                    }
                });
            }
        }
        restarted += 1;
    }
    tracing::info!(restarted, "restarted non-terminal task loops");

    if let Some(registry) = registry {
        let reconciled = reconcile_running_tasks(store, registry).await?;
        if reconciled > 0 {
            tracing::warn!(
                reconciled,
                "failed running tasks that have no registered loop after recovery"
            );
        }
    }
    Ok(restarted)
}

/// Fails every task that is still `running` without a registered loop.
///
/// This is the restart reconciliation pass: a `running` row is a promise that
/// a worker exists. If the registry has no entry, no loop can make progress,
/// so the row becomes `failed` with a `task_failed` activity row instead of
/// hanging forever.
pub(crate) async fn reconcile_running_tasks(
    store: &Arc<dyn GraphStore>,
    registry: &TaskRunRegistry,
) -> anyhow::Result<usize> {
    let tasks = store.list_tasks(RECOVERY_TASK_LIMIT).await?;
    let mut failed = 0;
    for task in tasks {
        if task.state != TaskState::Running || registry.contains(task.id) {
            continue;
        }
        if store
            .set_task_state_guarded(task.id, TaskState::Failed)
            .await?
        {
            store
                .append_activity(
                    task.id,
                    "task_failed",
                    "task was running after restart with no worker".to_string(),
                    json!({
                        "reason": "task was running after restart with no worker",
                        "source": "recovery",
                    }),
                )
                .await?;
            failed += 1;
        }
    }
    if failed > 0 {
        tracing::warn!(failed, "reconciled orphaned running tasks as failed");
    }
    Ok(failed)
}

/// Runs the approval supervisor on a fixed interval until the process exits.
///
/// Every [`SUPERVISOR_INTERVAL`] it sweeps each non-terminal task, so an
/// overdue approval expires without a human decision and without anyone
/// checking the queue. Errors are logged, never fatal.
pub async fn run_supervisor(store: Arc<dyn GraphStore>, engine: Arc<Engine>, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    // The first tick fires immediately; skip work there and wait a full
    // interval so startup is not delayed by a sweep.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        match sweep_all(&store, &engine).await {
            Ok(swept) => tracing::debug!(swept, "approval supervisor pass complete"),
            Err(err) => tracing::warn!(error = %err, "approval supervisor pass failed"),
        }
    }
}

/// Runs a fixed number of supervisor passes, for tests that must not sleep.
#[cfg(test)]
async fn run_supervisor_passes(
    store: &Arc<dyn GraphStore>,
    engine: &Engine,
    passes: usize,
) -> anyhow::Result<()> {
    for _ in 0..passes {
        sweep_all(store, engine).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use dhi_agent::{MockAgentRuntime, RuntimeSink as _, ToolSink as _};
    use dhi_core::{ApprovalDecision, ApprovalStatus, GuardStore, TaskState};
    use dhi_guard::{GuardedExecutor, Policy};
    use dhi_testkit::MemoryStore;
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    use uuid::Uuid;

    fn test_guard() -> SharedPolicy {
        SharedPolicy::from_policy(&Policy::chess(), 900)
    }

    fn test_state() -> AppState {
        let store = Arc::new(MemoryStore::new());
        let agent: Arc<dyn AgentRuntime> = Arc::new(MockAgentRuntime);
        let engine = Arc::new(Engine::new(store.clone(), agent.clone()));
        let guard_store: Arc<dyn GuardStore> = store.clone();
        AppState {
            store,
            guard_store,
            engine,
            agent,
            pool: Arc::new(LlmPool::default()),
            guard: test_guard(),
            mgmt: None,
            mcp: crate::mcp::McpManager::idle(),
            tool_catalog: Arc::new(dhi_tool_catalog::CatalogRuntime::default()),
            tool_control: None,
            web: Arc::new(crate::web_providers::WebProviderRuntime::empty()),
            task_runs: Arc::new(TaskRunRegistry::disabled()),
        }
    }

    #[tokio::test]
    async fn tool_catalog_route_returns_registered_entries() {
        let mut state = test_state();
        let entry = dhi_tool_catalog::ToolCatalogEntry::new(
            "web_search",
            dhi_tool_catalog::ToolSource::Web,
        )
        .with_description("search");
        state.tool_catalog = Arc::new(dhi_tool_catalog::CatalogRuntime::new(vec![Arc::new(
            dhi_tool_catalog::StaticProvider::new("test", vec![entry]),
        )]));
        let app = router(state);
        let (status, body) = send(app, get("/api/tools/catalog")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["tools"][0]["key"], "web_search");
        assert_eq!(body["tools"][0]["source"], "web");
    }

    #[tokio::test]
    async fn tool_control_without_a_store_returns_503() {
        let app = router(test_state());
        let (status, body) = send(app, get("/api/tools/control")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body["error"].is_string());
    }

    #[tokio::test]
    async fn health_returns_ok() {
        let app = router(test_state());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ok");
    }

    #[tokio::test]
    async fn mcp_servers_route_returns_empty_for_idle_manager() {
        let (status, json) = send(router(test_state()), get_uri("/api/mcp/servers")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json, json!({ "servers": [] }));
    }

    #[tokio::test]
    async fn mcp_tools_route_unknown_server_is_404() {
        let (status, json) = send(
            router(test_state()),
            get_uri("/api/mcp/servers/missing/tools"),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(
            json["error"]
                .as_str()
                .unwrap_or_default()
                .contains("missing"),
            "{json}"
        );
    }

    #[tokio::test]
    async fn mcp_refresh_route_unknown_server_is_404() {
        let request = Request::builder()
            .method("POST")
            .uri("/api/mcp/servers/missing/refresh")
            .body(Body::empty())
            .unwrap();
        let (status, json) = send(router(test_state()), request).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(
            json["error"]
                .as_str()
                .unwrap_or_default()
                .contains("missing"),
            "{json}"
        );
    }

    #[tokio::test]
    async fn noop_backend_fails_closed() {
        let err = NoopBackend
            .execute("finishTurn", &json!({}))
            .await
            .expect_err("the server ships with no tool backend");
        assert_eq!(err.to_string(), "no tool backend configured");
    }

    /// The in-loop sink adapter keeps the one-guard semantics: an allowed
    /// tool runs and returns `Allowed` with the backend's output; an
    /// unlisted tool is denied with the policy's reason. Identical shape to
    /// the post-hoc executor path (which dhi-guard tests exhaustively).
    #[tokio::test]
    async fn guarded_tool_sink_maps_executor_outcomes() {
        let store = Arc::new(MemoryStore::new());
        let policy = Policy {
            version: "sink-test-v1".to_string(),
            default: GuardVerdict::Deny,
            rules: vec![dhi_guard::Rule {
                name: "allow-createGame".to_string(),
                tool: "createGame".to_string(),
                action: GuardVerdict::Allow,
                reason: "start a new chess game".to_string(),
            }],
        };
        let executor = Arc::new(GuardedExecutor::new(
            store.clone(),
            policy,
            Arc::new(FakeBackend),
            chrono::Duration::minutes(5),
        ));
        let sink = GuardedToolSink::new(executor);
        let task_id = Uuid::new_v4();
        let intent_id = Uuid::new_v4();

        let allowed = sink
            .execute(
                task_id,
                intent_id,
                dhi_core::ToolCallRequest {
                    tool: "createGame".to_string(),
                    input: json!({ "color": "white" }),
                },
            )
            .await
            .expect("the guarded call records and executes");
        let dhi_core::ToolCallOutcome::Allowed { output } = allowed else {
            panic!("createGame is allowed, got {allowed:?}");
        };
        assert_eq!(output["tool"], "createGame");

        let denied = sink
            .execute(
                task_id,
                intent_id,
                dhi_core::ToolCallRequest {
                    tool: "shell".to_string(),
                    input: json!({}),
                },
            )
            .await
            .expect("a denial is a verdict, not a transport error");
        let dhi_core::ToolCallOutcome::Denied { reason } = denied else {
            panic!("an unlisted tool is denied, got {denied:?}");
        };
        assert!(
            reason.contains("no matching rule"),
            "the denial carries the policy's reason: {reason}"
        );
    }

    /// In-loop guarded calls mirror their outcome onto the activity feed:
    /// one `tool_allowed` per allowed call, one `tool_denied` per denial,
    /// with the tool name and intent id. The durable `tool_calls` and
    /// `guard_decisions` rows are unchanged.
    #[tokio::test]
    async fn guarded_tool_sink_mirrors_outcomes_to_activity() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("sink activity", 1);
        let policy = Policy {
            version: "sink-activity-v1".to_string(),
            default: GuardVerdict::Deny,
            rules: vec![dhi_guard::Rule {
                name: "allow-createGame".to_string(),
                tool: "createGame".to_string(),
                action: GuardVerdict::Allow,
                reason: "start a new chess game".to_string(),
            }],
        };
        let executor = Arc::new(GuardedExecutor::new(
            store.clone(),
            policy,
            Arc::new(FakeBackend),
            chrono::Duration::minutes(5),
        ));
        let sink = GuardedToolSink::with_activity(executor, store.clone());
        let intent_id = Uuid::new_v4();

        sink.execute(
            task.id,
            intent_id,
            dhi_core::ToolCallRequest {
                tool: "createGame".to_string(),
                input: json!({ "color": "white" }),
            },
        )
        .await
        .expect("allowed call");
        sink.execute(
            task.id,
            intent_id,
            dhi_core::ToolCallRequest {
                tool: "shell".to_string(),
                input: json!({}),
            },
        )
        .await
        .expect("denied call");

        let activities = store.activities(task.id);
        let allowed = activities
            .iter()
            .find(|activity| activity.kind == "tool_allowed")
            .expect("tool_allowed activity");
        assert_eq!(allowed.payload["tool"], "createGame");
        assert_eq!(allowed.payload["intent_id"], intent_id.to_string());
        let denied = activities
            .iter()
            .find(|activity| activity.kind == "tool_denied")
            .expect("tool_denied activity");
        assert_eq!(denied.payload["tool"], "shell");
        assert!(
            denied.payload["reason"]
                .as_str()
                .unwrap_or_default()
                .contains("no matching rule"),
            "{denied:?}"
        );
    }

    /// The 502 snippet the `/api/llm/models` proxy returns is scrubbed of the
    /// resolved profile's key BEFORE truncation (upstreams occasionally echo
    /// the bearer token back).
    #[test]
    fn upstream_error_snippets_are_redacted_before_truncation() {
        let key = "sk-upstream-echo-9999";
        let body = json!({
            "aaa_echo": format!("bad bearer: {key}"),
            "filler": "x".repeat(300),
        });

        let message = short_message(&body, key);
        assert!(message.contains("[redacted]"), "{message}");
        assert!(!message.contains(key), "{message}");
        assert!(
            message.chars().count() <= 200,
            "still truncated after redaction"
        );

        // An empty secret passes through untouched.
        let plain = short_message(&body, "");
        assert!(plain.contains("bad bearer"), "{plain}");
    }

    /// A paused intent behind an overdue approval is closed by the supervisor
    /// alone: no decision, no engine loop, no human.
    #[tokio::test]
    async fn supervisor_sweeps_expired_approvals() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("deploy without a reviewer", 3);
        let intent = store.seed_intent(task.id, task.origin_fact_id, "ship it");

        // Park the intent and open an approval that already ran out.
        GraphStore::set_task_state_guarded(store.as_ref(), task.id, TaskState::Running)
            .await
            .unwrap();
        let claimed = store
            .claim_intent(intent.id, "worker-1")
            .await
            .unwrap()
            .expect("the seeded intent is claimable");
        store
            .finish_intent(claimed.id, dhi_core::IntentState::Paused)
            .await
            .unwrap();
        // A tool call stamped with the paused intent, behind an overdue approval.
        let call = GuardStore::record_tool_call(
            store.as_ref(),
            task.id,
            claimed.id,
            "finishTurn".to_string(),
            json!({}),
        )
        .await
        .unwrap();
        let decision = GuardStore::record_guard_decision(
            store.as_ref(),
            call.id,
            dhi_core::GuardVerdict::Ask,
            None,
            None,
            "needs a person".to_string(),
        )
        .await
        .unwrap();
        GuardStore::create_approval(
            store.as_ref(),
            task.id,
            call.id,
            decision.id,
            "needs a person".to_string(),
            chrono::Utc::now() - chrono::Duration::minutes(1),
        )
        .await
        .unwrap();
        assert_eq!(
            GuardStore::list_pending_approvals(store.as_ref(), task.id)
                .await
                .unwrap()
                .len(),
            1
        );

        let guard_store: Arc<dyn dhi_core::GuardStore> = store.clone();
        let executor = Arc::new(GuardedExecutor::new(
            store.clone(),
            Policy::chess(),
            Arc::new(NoopBackend),
            chrono::Duration::minutes(15),
        ));
        let engine = Arc::new(
            Engine::new(store.clone(), Arc::new(MockAgentRuntime))
                .with_executor(executor, guard_store.clone()),
        );
        let store_trait: Arc<dyn GraphStore> = store.clone();

        run_supervisor_passes(&store_trait, &engine, 1)
            .await
            .unwrap();

        assert!(
            GuardStore::list_pending_approvals(store.as_ref(), task.id)
                .await
                .unwrap()
                .is_empty(),
            "the supervisor expires an overdue approval without a decision"
        );
        let intents = store.intents(task.id);
        assert_eq!(intents.len(), 1);
        assert_eq!(
            intents[0].state,
            dhi_core::IntentState::Blocked,
            "the paused intent behind the expired approval closes as blocked"
        );
        let activities = store.activities(task.id);
        assert!(
            activities.iter().any(|a| a.kind == "approval_expired"),
            "the sweep leaves an activity row"
        );
    }

    /// The supervisor skips terminal tasks and keeps going after a failure.
    #[tokio::test]
    async fn supervisor_skips_terminal_tasks() {
        let store = Arc::new(MemoryStore::new());
        let live = store.seed_task("live", 2);
        let done = store.seed_task("already done", 2);
        GraphStore::set_task_state_guarded(store.as_ref(), done.id, TaskState::Done)
            .await
            .unwrap();

        let engine = Arc::new(Engine::new(store.clone(), Arc::new(MockAgentRuntime)));
        let store_trait: Arc<dyn GraphStore> = store.clone();
        let swept = sweep_all(&store_trait, &engine).await.unwrap();
        assert_eq!(swept, 1, "only the non-terminal task is swept");
        assert!(store.get_task(live.id).await.unwrap().is_some());
    }

    /// A decided approval is never expired by a later supervisor pass.
    #[tokio::test]
    async fn supervisor_leaves_decided_approvals_alone() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("decided in time", 3);
        GraphStore::set_task_state_guarded(store.as_ref(), task.id, TaskState::Running)
            .await
            .unwrap();
        let approval = GuardStore::create_approval(
            store.as_ref(),
            task.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            "approve".to_string(),
            chrono::Utc::now() + chrono::Duration::minutes(5),
        )
        .await
        .unwrap();
        GuardStore::decide_approval(store.as_ref(), approval.id, ApprovalDecision::Deny)
            .await
            .unwrap()
            .expect("a pending approval can be denied");

        let guard_store: Arc<dyn dhi_core::GuardStore> = store.clone();
        let executor = Arc::new(GuardedExecutor::new(
            store.clone(),
            Policy::chess(),
            Arc::new(NoopBackend),
            chrono::Duration::minutes(15),
        ));
        let engine = Arc::new(
            Engine::new(store.clone(), Arc::new(MockAgentRuntime))
                .with_executor(executor, guard_store),
        );
        let store_trait: Arc<dyn GraphStore> = store.clone();
        run_supervisor_passes(&store_trait, &engine, 2)
            .await
            .unwrap();

        let stored = GuardStore::get_approval(store.as_ref(), approval.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.status, ApprovalStatus::Denied);
    }

    /// The supervisor runs on a fixed interval; nothing else drives expiry.
    #[test]
    fn supervisor_interval_is_fixed() {
        assert_eq!(SUPERVISOR_INTERVAL, Duration::from_secs(30));
    }

    /// Planner/worker that never makes progress. A restarted task reaches
    /// `Timeout` on its first idle planner tick.
    struct NoPlanAgent;

    #[async_trait]
    impl dhi_agent::AgentRuntime for NoPlanAgent {
        async fn plan(
            &self,
            _input: dhi_agent::PlanInput,
        ) -> anyhow::Result<dhi_agent::AgentCall<dhi_agent::PlanOutcome>> {
            Ok(dhi_agent::AgentCall {
                output: dhi_agent::PlanOutcome::NoPlan {
                    reason: "nothing to do".to_string(),
                },
                meta: dhi_agent::AgentCallMeta::default(),
                prompt: String::new(),
                response: String::new(),
                thinking: None,
            })
        }

        async fn execute(
            &self,
            _input: dhi_agent::ExecuteInput,
        ) -> anyhow::Result<dhi_agent::AgentCall<dhi_agent::ExecuteOutput>> {
            anyhow::bail!("the no-plan agent has no work to execute")
        }

        fn name(&self) -> &str {
            "no-plan"
        }
    }

    /// Planner that blocks long enough for a recovery test to observe the
    /// registry entry while the restarted loop is still live.
    struct HangingAgent;

    #[async_trait]
    impl dhi_agent::AgentRuntime for HangingAgent {
        async fn plan(
            &self,
            _input: dhi_agent::PlanInput,
        ) -> anyhow::Result<dhi_agent::AgentCall<dhi_agent::PlanOutcome>> {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(dhi_agent::AgentCall {
                output: dhi_agent::PlanOutcome::NoPlan {
                    reason: "never reached".to_string(),
                },
                meta: dhi_agent::AgentCallMeta::default(),
                prompt: String::new(),
                response: String::new(),
                thinking: None,
            })
        }

        async fn execute(
            &self,
            _input: dhi_agent::ExecuteInput,
        ) -> anyhow::Result<dhi_agent::AgentCall<dhi_agent::ExecuteOutput>> {
            anyhow::bail!("the hanging agent has no work to execute")
        }

        fn name(&self) -> &str {
            "hanging"
        }
    }

    /// Engine that expires on its first idle planner tick, so a restarted task
    /// settles promptly in a test.
    fn recovery_engine(store: &Arc<MemoryStore>) -> Arc<Engine> {
        let store_trait: Arc<dyn GraphStore> = store.clone();
        Arc::new(Engine::new(store_trait, Arc::new(NoPlanAgent)).with_max_idle_ticks(1))
    }

    /// Waits for a spawned task loop to settle. The bound keeps a broken run
    /// from hanging the test.
    async fn wait_for_task_state(store: &MemoryStore, id: TaskId, expected: TaskState) {
        let settled = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if store
                    .get_task(id)
                    .await
                    .expect("read the task")
                    .is_some_and(|task| task.state == expected)
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(settled.is_ok(), "task {id} did not reach {expected}");
    }

    #[tokio::test]
    async fn recover_denies_pending_approvals() {
        let store = Arc::new(MemoryStore::new());
        let first_task = store.seed_task("wait for review", 3);
        let second_task = store.seed_task("wait for another review", 3);
        let first = seed_pending_approval(&store, &first_task).await;
        let second = seed_pending_approval(&store, &second_task).await;

        let store_trait: Arc<dyn GraphStore> = store.clone();
        let guard_store: Arc<dyn GuardStore> = store.clone();
        let engine = recovery_engine(&store);
        recover_tasks(&store_trait, &guard_store, &engine)
            .await
            .unwrap();

        for approval in [&first, &second] {
            let stored = GuardStore::get_approval(store.as_ref(), approval.id)
                .await
                .unwrap()
                .expect("the approval still exists");
            assert_eq!(stored.status, ApprovalStatus::Denied);
            assert!(stored.decided_at.is_some());
        }
        assert!(
            GuardStore::list_pending_approvals_all(store.as_ref())
                .await
                .unwrap()
                .is_empty(),
            "recovery leaves no pending approval behind"
        );
        for (task, approval) in [(&first_task, &first), (&second_task, &second)] {
            let denied = store
                .activities(task.id)
                .into_iter()
                .find(|activity| activity.kind == "approval_denied")
                .expect("recovery leaves an approval_denied row");
            assert_eq!(denied.payload["approval_id"], approval.id.to_string());
            assert_eq!(denied.payload["reason"], "service restart");
        }
    }

    #[tokio::test]
    async fn recover_completes_denied_tool_calls() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("deny the stale call", 3);
        let approval = seed_pending_approval(&store, &task).await;
        let before = GuardStore::get_tool_call(store.as_ref(), approval.tool_call_id)
            .await
            .unwrap()
            .expect("the seeded tool call exists");
        assert_eq!(before.status, "pending");

        let store_trait: Arc<dyn GraphStore> = store.clone();
        let guard_store: Arc<dyn GuardStore> = store.clone();
        let engine = recovery_engine(&store);
        recover_tasks(&store_trait, &guard_store, &engine)
            .await
            .unwrap();

        let call = GuardStore::get_tool_call(store.as_ref(), approval.tool_call_id)
            .await
            .unwrap()
            .expect("the tool call still exists");
        assert_eq!(call.status, "denied");
        assert!(call.output.is_none());
    }

    #[tokio::test]
    async fn recover_resets_running_intents() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("abandoned work", 4);
        GraphStore::set_task_state_guarded(store.as_ref(), task.id, TaskState::Running)
            .await
            .unwrap();
        let first = store.seed_intent(task.id, task.origin_fact_id, "first");
        let second = store.seed_intent(task.id, task.origin_fact_id, "second");
        for intent in [&first, &second] {
            store
                .claim_intent(intent.id, "worker-1")
                .await
                .unwrap()
                .expect("the seeded intent is claimable");
        }
        // Paused tasks are not restarted: this test observes the intent reset
        // without a task loop racing to claim the released work.
        GraphStore::set_task_state_guarded(store.as_ref(), task.id, TaskState::Paused)
            .await
            .unwrap();

        let store_trait: Arc<dyn GraphStore> = store.clone();
        let guard_store: Arc<dyn GuardStore> = store.clone();
        let engine = recovery_engine(&store);
        let restarted = recover_tasks(&store_trait, &guard_store, &engine)
            .await
            .unwrap();
        assert_eq!(restarted, 0, "a paused task is not restarted");

        let intents = store.intents(task.id);
        assert_eq!(intents.len(), 2);
        for intent in intents {
            assert_eq!(intent.state, dhi_core::IntentState::Open);
            assert!(intent.owner.is_none());
        }
    }

    #[tokio::test]
    async fn recover_restarts_nonterminal_tasks() {
        let store = Arc::new(MemoryStore::new());
        let running = store.seed_task("resume running", 2);
        let created = store.seed_task("resume created", 2);
        GraphStore::set_task_state_guarded(store.as_ref(), running.id, TaskState::Running)
            .await
            .unwrap();

        let store_trait: Arc<dyn GraphStore> = store.clone();
        let guard_store: Arc<dyn GuardStore> = store.clone();
        let engine = recovery_engine(&store);
        let restarted = recover_tasks(&store_trait, &guard_store, &engine)
            .await
            .unwrap();
        assert_eq!(restarted, 2);

        wait_for_task_state(&store, running.id, TaskState::Timeout).await;
        wait_for_task_state(&store, created.id, TaskState::Timeout).await;
        for task in [running, created] {
            assert!(
                store
                    .activities(task.id)
                    .iter()
                    .any(|activity| activity.kind == "engine_timeout"),
                "the restarted loop ran to its idle bound"
            );
        }
    }

    #[tokio::test]
    async fn recover_skips_terminal_tasks() {
        let store = Arc::new(MemoryStore::new());
        let live = store.seed_task("still live", 2);
        GraphStore::set_task_state_guarded(store.as_ref(), live.id, TaskState::Running)
            .await
            .unwrap();
        let mut terminal = Vec::new();
        for (goal, state) in [
            ("already done", TaskState::Done),
            ("already failed", TaskState::Failed),
            ("already timed out", TaskState::Timeout),
            ("already cancelled", TaskState::Cancelled),
        ] {
            let task = store.seed_task(goal, 2);
            GraphStore::set_task_state_guarded(store.as_ref(), task.id, state)
                .await
                .unwrap();
            terminal.push((task, state));
        }

        let store_trait: Arc<dyn GraphStore> = store.clone();
        let guard_store: Arc<dyn GuardStore> = store.clone();
        let engine = recovery_engine(&store);
        let restarted = recover_tasks(&store_trait, &guard_store, &engine)
            .await
            .unwrap();
        assert_eq!(restarted, 1, "only the non-terminal task is restarted");

        wait_for_task_state(&store, live.id, TaskState::Timeout).await;
        for (task, state) in terminal {
            assert_eq!(
                store.get_task(task.id).await.unwrap().unwrap().state,
                state,
                "a terminal task is never restarted"
            );
            assert!(
                !store
                    .activities(task.id)
                    .iter()
                    .any(|activity| activity.kind == "engine_timeout"),
                "a terminal task runs no loop"
            );
        }
    }

    #[tokio::test]
    async fn recover_skips_paused_tasks() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("paused on purpose", 2);
        GraphStore::set_task_state_guarded(store.as_ref(), task.id, TaskState::Running)
            .await
            .unwrap();
        GraphStore::set_task_state_guarded(store.as_ref(), task.id, TaskState::Paused)
            .await
            .unwrap();

        let store_trait: Arc<dyn GraphStore> = store.clone();
        let guard_store: Arc<dyn GuardStore> = store.clone();
        let engine = recovery_engine(&store);
        let restarted = recover_tasks(&store_trait, &guard_store, &engine)
            .await
            .unwrap();
        assert_eq!(restarted, 0);
        assert_eq!(
            store.get_task(task.id).await.unwrap().unwrap().state,
            TaskState::Paused
        );
        assert!(
            !store
                .activities(task.id)
                .iter()
                .any(|activity| activity.kind == "engine_timeout"),
            "a paused task runs no loop"
        );
    }

    #[tokio::test]
    async fn recover_with_registry_registers_restarted_loops() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("resume with a live worker", 2);
        GraphStore::set_task_state_guarded(store.as_ref(), task.id, TaskState::Running)
            .await
            .unwrap();
        let store_trait: Arc<dyn GraphStore> = store.clone();
        let guard_store: Arc<dyn GuardStore> = store.clone();
        let engine = Arc::new(Engine::new(store_trait.clone(), Arc::new(HangingAgent)));
        let registry = Arc::new(TaskRunRegistry::new());

        let restarted = recover_tasks_with_registry(&store_trait, &guard_store, &engine, &registry)
            .await
            .unwrap();

        assert_eq!(restarted, 1);
        assert!(
            registry.contains(task.id),
            "recovery must register the restarted loop"
        );
        assert_eq!(
            store.get_task(task.id).await.unwrap().unwrap().state,
            TaskState::Running,
            "reconciliation must leave a registered running task alone"
        );
        registry.abort_and_remove(task.id);
    }

    #[tokio::test]
    async fn reconciliation_fails_running_tasks_without_a_worker() {
        let store = Arc::new(MemoryStore::new());
        let orphan = store.seed_task("running with no loop", 2);
        GraphStore::set_task_state_guarded(store.as_ref(), orphan.id, TaskState::Running)
            .await
            .unwrap();
        let store_trait: Arc<dyn GraphStore> = store.clone();
        let registry = TaskRunRegistry::new();

        let failed = reconcile_running_tasks(&store_trait, &registry)
            .await
            .unwrap();

        assert_eq!(failed, 1);
        assert_eq!(
            store.get_task(orphan.id).await.unwrap().unwrap().state,
            TaskState::Failed
        );
        let activity = store
            .activities(orphan.id)
            .into_iter()
            .find(|activity| activity.kind == "task_failed")
            .expect("a reconciled task leaves a task_failed row");
        assert_eq!(activity.payload["source"], "recovery");
    }

    #[tokio::test]
    async fn spawn_task_loop_removes_its_registry_entry_on_exit() {
        let store = Arc::new(MemoryStore::new());
        let registry = Arc::new(TaskRunRegistry::new());
        let mut state = test_state_with_store(store.clone());
        state.task_runs = registry.clone();
        let app = router(state);

        let (status, json) = create_one(app, "finish and deregister").await;
        assert_eq!(status, StatusCode::CREATED, "{json}");
        let id: Uuid = json["id"].as_str().unwrap().parse().unwrap();
        wait_for_task_state(&store, id, TaskState::Done).await;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while registry.contains(id) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the finished loop never removed its registry entry"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn server_failure_helper_marks_a_dead_loop_failed() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("dead without a loop", 2);
        GraphStore::set_task_state_guarded(store.as_ref(), task.id, TaskState::Running)
            .await
            .unwrap();

        fail_task_from_server(
            store.as_ref(),
            task.id,
            "task run failed: store is gone".to_string(),
            "server",
        )
        .await;

        assert_eq!(
            store.get_task(task.id).await.unwrap().unwrap().state,
            TaskState::Failed
        );
        let activity = store
            .activities(task.id)
            .into_iter()
            .find(|activity| activity.kind == "task_failed")
            .expect("task_failed activity");
        assert_eq!(activity.payload["source"], "server");
    }

    fn test_state_with_store(store: Arc<MemoryStore>) -> AppState {
        let store_trait: Arc<dyn GraphStore> = store.clone();
        let agent: Arc<dyn AgentRuntime> = Arc::new(MockAgentRuntime);
        let engine = Arc::new(Engine::new(store_trait.clone(), agent.clone()));
        let guard_store: Arc<dyn GuardStore> = store;
        AppState {
            store: store_trait,
            guard_store,
            engine,
            agent,
            pool: Arc::new(LlmPool::default()),
            guard: test_guard(),
            mgmt: None,
            mcp: crate::mcp::McpManager::idle(),
            tool_catalog: Arc::new(dhi_tool_catalog::CatalogRuntime::default()),
            tool_control: None,
            web: Arc::new(crate::web_providers::WebProviderRuntime::empty()),
            task_runs: Arc::new(TaskRunRegistry::disabled()),
        }
    }

    /// A fake tool backend so an approved call actually executes on resume.
    struct FakeBackend;

    #[async_trait]
    impl ToolBackend for FakeBackend {
        async fn execute(
            &self,
            tool: &str,
            input: &serde_json::Value,
        ) -> anyhow::Result<serde_json::Value> {
            Ok(json!({ "tool": tool, "input": input }))
        }
    }

    /// State with a guard-wired engine (`GuardedExecutor`) and the same store
    /// behind both `GraphStore` and `GuardStore`, as `main.rs` wires it.
    fn guarded_state(store: Arc<MemoryStore>) -> AppState {
        let store_trait: Arc<dyn GraphStore> = store.clone();
        let guard_store: Arc<dyn GuardStore> = store;
        let executor = Arc::new(GuardedExecutor::new(
            guard_store.clone(),
            Policy::chess(),
            Arc::new(FakeBackend),
            chrono::Duration::minutes(15),
        ));
        let engine = Arc::new(
            Engine::new(store_trait.clone(), Arc::new(MockAgentRuntime))
                .with_executor(executor, guard_store.clone()),
        );
        AppState {
            store: store_trait,
            guard_store,
            engine,
            agent: Arc::new(MockAgentRuntime),
            pool: Arc::new(LlmPool::default()),
            guard: test_guard(),
            mgmt: None,
            mcp: crate::mcp::McpManager::idle(),
            tool_catalog: Arc::new(dhi_tool_catalog::CatalogRuntime::default()),
            tool_control: None,
            web: Arc::new(crate::web_providers::WebProviderRuntime::empty()),
            task_runs: Arc::new(TaskRunRegistry::disabled()),
        }
    }

    /// Parks a seeded intent behind a pending approval, the way an `Ask`
    /// verdict at the tool-call point leaves it. The intent stays `running`:
    /// blocking approvals never pause it.
    async fn seed_pending_approval(store: &MemoryStore, task: &Task) -> Approval {
        let intent = store.seed_intent(task.id, task.origin_fact_id, "await a decision");
        GraphStore::set_task_state_guarded(store, task.id, TaskState::Running)
            .await
            .unwrap();
        let claimed = store
            .claim_intent(intent.id, "worker-1")
            .await
            .unwrap()
            .expect("the seeded intent is claimable");
        let call = GuardStore::record_tool_call(
            store,
            task.id,
            claimed.id,
            "finishTurn".to_string(),
            json!({ "move": "Qh5" }),
        )
        .await
        .unwrap();
        let decision = GuardStore::record_guard_decision(
            store,
            call.id,
            dhi_core::GuardVerdict::Ask,
            None,
            None,
            "needs a person".to_string(),
        )
        .await
        .unwrap();
        GuardStore::create_approval(
            store,
            task.id,
            call.id,
            decision.id,
            "needs a person".to_string(),
            chrono::Utc::now() + chrono::Duration::minutes(5),
        )
        .await
        .unwrap()
    }

    fn post_json(uri: &str, body: Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    async fn send(app: Router, request: Request<Body>) -> (StatusCode, Value) {
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json = if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&body).unwrap()
        };
        (status, json)
    }

    fn post_task(goal: &str, bound: i32) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/api/tasks")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "goal": goal, "bound": bound }).to_string(),
            ))
            .unwrap()
    }

    fn post_task_with_capacity(goal: &str, bound: i32, capacity: Value) -> Request<Body> {
        post_json(
            "/api/tasks",
            json!({ "goal": goal, "bound": bound, "capacity": capacity }),
        )
    }

    fn post_goal(task_id: TaskId, description: &str) -> Request<Body> {
        post_json(
            &format!("/api/tasks/{task_id}/goals"),
            json!({ "description": description }),
        )
    }

    fn post_capacity(task_id: TaskId, body: Value) -> Request<Body> {
        post_json(&format!("/api/tasks/{task_id}/capacity"), body)
    }

    /// Moves a freshly seeded task to one terminal state through the public
    /// guard, exactly as an engine run would.
    async fn make_terminal(store: &MemoryStore, task: &Task, state: TaskState) {
        GraphStore::set_task_state_guarded(store, task.id, TaskState::Running)
            .await
            .unwrap();
        GraphStore::set_task_state_guarded(store, task.id, state)
            .await
            .unwrap();
    }

    fn get_uri(uri: &str) -> Request<Body> {
        Request::builder().uri(uri).body(Body::empty()).unwrap()
    }

    async fn create_one(app: Router, goal: &str) -> (StatusCode, Value) {
        send(app, post_task(goal, 3)).await
    }

    // -----------------------------------------------------------------------
    // Side questions
    // -----------------------------------------------------------------------

    /// A task with one goal, one recorded fact, and a little activity. Seeded
    /// directly so no background engine loop moves it while a test runs.
    async fn side_task(store: &MemoryStore) -> Task {
        let task = store.seed_task("explain the finished task", 3);
        GraphStore::add_goal(store, task.id, "one goal".into())
            .await
            .unwrap();
        store
            .record_fact(
                task.id,
                "a confirmed fact".to_string(),
                json!({"source": "test"}),
            )
            .await
            .unwrap();
        store
            .append_activity(task.id, "note", "one note".to_string(), json!({"n": 1}))
            .await
            .unwrap();
        task
    }

    fn post_side(task_id: TaskId, body: Value) -> Request<Body> {
        post_json(&format!("/api/tasks/{task_id}/side-questions"), body)
    }

    fn get_side(task_id: TaskId) -> Request<Body> {
        get_uri(&format!("/api/tasks/{task_id}/side-questions"))
    }

    #[tokio::test]
    async fn side_snapshot_freezes_task_goals_facts_and_last_50_activity() {
        let store = Arc::new(MemoryStore::new());
        let task = side_task(&store).await;
        for n in 1..=60 {
            store
                .append_activity(task.id, "note", format!("row {n}"), json!({"n": n}))
                .await
                .unwrap();
        }

        let snapshot = super::side_questions::build_side_question_snapshot(store.as_ref(), &task)
            .await
            .unwrap();

        assert_eq!(snapshot["task"]["id"], json!(task.id));
        assert_eq!(snapshot["task"]["goal"], "explain the finished task");
        assert_eq!(snapshot["task"]["state"], "created");
        assert_eq!(snapshot["task"]["bound"], 3);
        assert_eq!(snapshot["task"]["pins"]["planner_profile_id"], Value::Null);
        assert_eq!(snapshot["goals"].as_array().unwrap().len(), 1);
        assert_eq!(snapshot["goals"][0]["description"], "one goal");
        assert_eq!(snapshot["facts"].as_array().unwrap().len(), 2);
        assert!(
            snapshot["facts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|fact| fact["claim"] == "a confirmed fact" && fact["state"] == "confirmed")
        );

        let activity = snapshot["activity"].as_array().unwrap();
        assert_eq!(activity.len(), 50);
        assert_eq!(activity[0]["message"], "row 11");
        assert_eq!(activity[49]["message"], "row 60");
        assert!(
            activity.iter().all(|row| row.get("output").is_none()),
            "activity rows carry no tool output beyond their payload"
        );
    }

    #[tokio::test]
    async fn side_question_create_append_and_list() {
        let store = Arc::new(MemoryStore::new());
        let task = side_task(&store).await;
        let activity_before = store.activities(task.id).len();
        let facts_before = store.facts(task.id).len();
        let goals_before = store.goals(task.id).len();
        let intents_before = store.intents(task.id).len();
        let app = router(test_state_with_store(store.clone()));

        let (status, body) = send(
            app.clone(),
            post_side(task.id, json!({"question": "What happened?"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let session_id = body["session_id"].as_str().unwrap().to_string();
        assert_eq!(body["request"]["status"], "ok");
        assert_eq!(body["request"]["question"], "What happened?");
        assert_eq!(body["request"]["answer"], body["answer"]);
        let answer = body["answer"].as_str().unwrap();
        assert!(answer.contains("What happened?"), "{answer}");
        assert!(answer.contains("advisory"), "{answer}");

        // The side question writes no work-graph row, no activity, and no
        // guarded call or decision.
        assert_eq!(store.activities(task.id).len(), activity_before);
        assert_eq!(store.facts(task.id).len(), facts_before);
        assert_eq!(store.goals(task.id).len(), goals_before);
        assert_eq!(store.intents(task.id).len(), intents_before);
        assert!(store.tool_calls(task.id).is_empty());
        assert!(store.guard_decisions().is_empty());

        let (status, list) = send(app.clone(), get_side(task.id)).await;
        assert_eq!(status, StatusCode::OK, "{list}");
        let sessions = list["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["id"], session_id);
        assert_eq!(sessions[0]["status"], "open");
        assert!(
            sessions[0].get("snapshot").is_none(),
            "list omits the snapshot"
        );
        assert_eq!(sessions[0]["snapshot_summary"]["goals"], 1);
        assert_eq!(sessions[0]["snapshot_summary"]["facts"], 2);
        assert_eq!(sessions[0]["requests"].as_array().unwrap().len(), 1);

        // Appending reuses the same frozen session.
        let (status, body) = send(
            app.clone(),
            post_side(
                task.id,
                json!({"question": "And the evidence?", "session_id": session_id}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["session_id"], session_id);
        let (_, list) = send(app.clone(), get_side(task.id)).await;
        assert_eq!(list["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(list["sessions"][0]["requests"].as_array().unwrap().len(), 2);

        // A question without a session id always opens a NEW frozen session,
        // even while one is open; the previous session closes.
        let (status, body) = send(
            app.clone(),
            post_side(task.id, json!({"question": "Fresh eyes?"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_ne!(body["session_id"], session_id);
        let (_, list) = send(app, get_side(task.id)).await;
        let sessions = list["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0]["status"], "open");
        assert_eq!(sessions[1]["status"], "closed");
    }

    #[tokio::test]
    async fn side_question_validates_input_and_resolves_sessions() {
        let store = Arc::new(MemoryStore::new());
        let task = side_task(&store).await;
        let other = side_task(&store).await;
        let app = router(test_state_with_store(store.clone()));

        let long = "x".repeat(4_001);
        for question in ["", "   ", long.as_str()] {
            let (status, body) = send(
                app.clone(),
                post_side(task.id, json!({"question": question})),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert!(body["error"].is_string());
        }

        let (status, _) = send(app.clone(), get_side(Uuid::new_v4())).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = send(
            app.clone(),
            post_side(Uuid::new_v4(), json!({"question": "hi"})),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, body) = send(
            app.clone(),
            post_side(task.id, json!({"question": "first"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let first: Uuid = body["session_id"].as_str().unwrap().parse().unwrap();

        // Unknown session id and another task's session are both 404.
        let (status, _) = send(
            app.clone(),
            post_side(
                task.id,
                json!({"question": "unknown", "session_id": Uuid::new_v4()}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = send(
            app.clone(),
            post_side(
                other.id,
                json!({"question": "wrong task", "session_id": first}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // A newer session closes the first; appending to it is 409.
        let (status, _) = send(
            app.clone(),
            post_side(task.id, json!({"question": "new session"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) = send(
            app,
            post_side(
                task.id,
                json!({"question": "too late", "session_id": first}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
    }

    /// An answerer that always fails, for the persisted-502 path.
    struct FailingSideAgent;

    #[async_trait]
    impl AgentRuntime for FailingSideAgent {
        async fn plan(
            &self,
            _input: dhi_agent::PlanInput,
        ) -> anyhow::Result<dhi_agent::AgentCall<dhi_agent::PlanOutcome>> {
            anyhow::bail!("the failing side agent never plans")
        }

        async fn execute(
            &self,
            _input: dhi_agent::ExecuteInput,
        ) -> anyhow::Result<dhi_agent::AgentCall<dhi_agent::ExecuteOutput>> {
            anyhow::bail!("the failing side agent never executes")
        }

        async fn side_answer(
            &self,
            _input: dhi_agent::SideQuestionInput,
        ) -> anyhow::Result<dhi_agent::AgentCall<String>> {
            anyhow::bail!("provider unavailable")
        }

        fn name(&self) -> &str {
            "failing-side"
        }
    }

    #[tokio::test]
    async fn side_question_answer_failure_is_persisted_and_returns_502() {
        let store = Arc::new(MemoryStore::new());
        let task = side_task(&store).await;
        let mut state = test_state_with_store(store.clone());
        state.agent = Arc::new(FailingSideAgent);
        let app = router(state);
        let activity_before = store.activities(task.id).len();

        let (status, body) = send(
            app.clone(),
            post_side(task.id, json!({"question": "Why did it fail?"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("provider unavailable"),
            "{body}"
        );

        let (status, list) = send(app, get_side(task.id)).await;
        assert_eq!(status, StatusCode::OK);
        let request = &list["sessions"][0]["requests"][0];
        assert_eq!(request["status"], "error");
        assert!(request["answer"].is_null());
        assert!(
            request["error"]
                .as_str()
                .unwrap()
                .contains("provider unavailable")
        );
        assert_eq!(store.activities(task.id).len(), activity_before);
    }

    #[tokio::test]
    async fn side_question_snapshot_does_not_change_when_new_facts_land() {
        let store = Arc::new(MemoryStore::new());
        let task = side_task(&store).await;
        let app = router(test_state_with_store(store.clone()));

        let (status, body) = send(
            app.clone(),
            post_side(task.id, json!({"question": "before"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let session_id: Uuid = body["session_id"].as_str().unwrap().parse().unwrap();

        store
            .record_fact(task.id, "a fact learned later".to_string(), json!({}))
            .await
            .unwrap();

        let (status, _) = send(
            app.clone(),
            post_side(
                task.id,
                json!({"question": "after", "session_id": session_id}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let stored = store
            .get_side_question_session(session_id)
            .await
            .unwrap()
            .unwrap();
        let claims: Vec<&str> = stored.snapshot["facts"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|fact| fact["claim"].as_str())
            .collect();
        assert!(claims.contains(&"a confirmed fact"));
        assert!(
            !claims.contains(&"a fact learned later"),
            "the session snapshot must stay frozen"
        );

        let (_, list) = send(app, get_side(task.id)).await;
        assert_eq!(list["sessions"][0]["snapshot_summary"]["facts"], 2);
    }

    #[tokio::test]
    async fn create_task_returns_201() {
        let store = Arc::new(MemoryStore::new());
        let app = router(test_state_with_store(store.clone()));
        let (status, json) = create_one(app, "mate in two").await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(json["goal"], "mate in two");
        assert_eq!(json["bound"], 3);
        assert!(json["id"].is_string());
        assert!(json["state"].is_string());
        let id: Uuid = json["id"].as_str().unwrap().parse().unwrap();
        assert!(store.get_task(id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn create_task_rejects_bad_input() {
        let app = router(test_state());
        let (status, json) = send(app, post_task("  ", 1)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(json["error"].is_string());
    }

    #[tokio::test]
    async fn create_task_with_capacity_applies_it_before_the_first_run() {
        let store = Arc::new(MemoryStore::new());
        let app = router(test_state_with_store(store.clone()));

        let (status, json) = send(
            app,
            post_task_with_capacity("pause after one round", 3, json!({ "max_rounds": 1 })),
        )
        .await;

        assert_eq!(status, StatusCode::CREATED, "{json}");
        assert_eq!(json["capacity"]["max_rounds"], 1, "{json}");
        assert!(
            json["capacity"]["max_model_calls"].is_null(),
            "omitted limits stay unset: {json}"
        );
        let id: Uuid = json["id"].as_str().unwrap().parse().unwrap();

        // One round is enough for the mock planner to add its first intent;
        // the engine must see the create-time limit and pause before claiming.
        wait_for_task_state(&store, id, TaskState::Paused).await;
        let stored = store.get_task(id).await.unwrap().unwrap();
        assert_eq!(stored.capacity.max_rounds, Some(1));
        let exhausted = store
            .activities(id)
            .into_iter()
            .find(|row| row.kind == "capacity_exhausted")
            .expect("the first run must observe the create-time capacity");
        assert_eq!(exhausted.payload["reason"], "rounds");
        assert_eq!(exhausted.payload["max_rounds"], 1);
    }

    #[tokio::test]
    async fn create_task_without_capacity_keeps_the_default_ledger() {
        let store = Arc::new(MemoryStore::new());
        let app = router(test_state_with_store(store.clone()));

        let (status, json) = create_one(app, "no capacity needed").await;

        assert_eq!(status, StatusCode::CREATED, "{json}");
        assert_eq!(json["capacity"], json!({}), "{json}");
        let id: Uuid = json["id"].as_str().unwrap().parse().unwrap();
        wait_for_task_state(&store, id, TaskState::Done).await;
        assert_eq!(
            store.get_task(id).await.unwrap().unwrap().capacity,
            TaskCapacity::default()
        );
    }

    #[tokio::test]
    async fn create_task_rejects_non_positive_capacity() {
        let store = Arc::new(MemoryStore::new());
        let app = router(test_state_with_store(store.clone()));

        for capacity in [
            json!({ "max_rounds": 0 }),
            json!({ "max_model_calls": 0 }),
            json!({ "max_wall_secs": 0 }),
            json!({ "max_parallel_workers": 0 }),
            json!({ "max_rounds": -1 }),
            json!({ "max_model_calls": -1 }),
            json!({ "max_wall_secs": -1 }),
            json!({ "max_parallel_workers": -1 }),
        ] {
            let (status, json) = send(
                app.clone(),
                post_task_with_capacity("invalid capacity", 3, capacity),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
            assert!(json["error"].is_string(), "{json}");
        }

        assert!(
            store.list_tasks(100).await.unwrap().is_empty(),
            "a refused create must not insert a task"
        );
    }

    // -----------------------------------------------------------------------
    // Task cancellation
    // -----------------------------------------------------------------------

    fn stop_request(uri: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .body(Body::empty())
            .unwrap()
    }

    fn delete_request(uri: &str) -> Request<Body> {
        Request::builder()
            .method("DELETE")
            .uri(uri)
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn stop_unknown_task_is_404() {
        let app = router(test_state());
        let id = Uuid::new_v4();
        let (status, json) = send(app, stop_request(&format!("/api/tasks/{id}/stop"))).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{json}");
    }

    #[tokio::test]
    async fn stop_terminal_task_is_409() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("already done", 2);
        make_terminal(&store, &task, TaskState::Done).await;
        let app = router(test_state_with_store(store));

        let (status, json) = send(app, stop_request(&format!("/api/tasks/{}/stop", task.id))).await;

        assert_eq!(status, StatusCode::CONFLICT, "{json}");
        assert!(
            json["error"].as_str().unwrap_or_default().contains("done"),
            "{json}"
        );
    }

    #[tokio::test]
    async fn stop_orphan_cancels_immediately() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("orphan", 2);
        GraphStore::set_task_state_guarded(store.as_ref(), task.id, TaskState::Running)
            .await
            .unwrap();
        let app = router(test_state_with_store(store.clone()));

        let (status, json) = send(app, stop_request(&format!("/api/tasks/{}/stop", task.id))).await;

        assert_eq!(status, StatusCode::ACCEPTED, "{json}");
        assert_eq!(json["task_id"], task.id.to_string());
        assert_eq!(json["status"], "stopping");
        assert_eq!(
            store.get_task(task.id).await.unwrap().unwrap().state,
            TaskState::Cancelled
        );
        assert!(
            store
                .activities(task.id)
                .iter()
                .any(|activity| activity.kind == "task_stopped")
        );
    }

    #[tokio::test]
    async fn stop_registered_task_sets_the_flag_without_waiting() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("live loop", 2);
        GraphStore::set_task_state_guarded(store.as_ref(), task.id, TaskState::Running)
            .await
            .unwrap();
        let registry = Arc::new(TaskRunRegistry::new());
        let mut state = test_state_with_store(store.clone());
        state.task_runs = registry.clone();
        let app = router(state);

        let stop = Arc::new(AtomicBool::new(false));
        let handle = tokio::spawn(std::future::pending::<()>());
        assert!(registry.register(task.id, stop.clone(), handle.abort_handle()));

        let (status, json) = send(app, stop_request(&format!("/api/tasks/{}/stop", task.id))).await;

        assert_eq!(status, StatusCode::ACCEPTED, "{json}");
        assert_eq!(json["status"], "stopping");
        assert!(stop.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(
            store.get_task(task.id).await.unwrap().unwrap().state,
            TaskState::Running,
            "the HTTP response does not wait for the loop to exit"
        );
        assert!(registry.contains(task.id));
        registry.abort_and_remove(task.id);
    }

    #[tokio::test]
    async fn hard_kill_monitor_aborts_and_marks_cancelled() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("ignores its stop flag", 2);
        GraphStore::set_task_state_guarded(store.as_ref(), task.id, TaskState::Running)
            .await
            .unwrap();
        let store_trait: Arc<dyn GraphStore> = store.clone();
        let registry = Arc::new(TaskRunRegistry::new());
        let handle = tokio::spawn(std::future::pending::<()>());
        registry.register(
            task.id,
            Arc::new(AtomicBool::new(true)),
            handle.abort_handle(),
        );

        spawn_hard_kill_monitor_after(
            store_trait,
            registry.clone(),
            task.id,
            Duration::from_millis(10),
        );

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if store.get_task(task.id).await.unwrap().unwrap().state == TaskState::Cancelled {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the hard-kill monitor never marked the task cancelled"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!registry.contains(task.id), "the monitor removes the entry");
        assert!(handle.is_finished(), "the monitor aborts the loop");
        assert!(
            store
                .activities(task.id)
                .iter()
                .any(|activity| activity.kind == "task_stopped_hard_kill")
        );
    }

    #[tokio::test]
    async fn bulk_stop_reports_stopping_and_blocked_independently() {
        let store = Arc::new(MemoryStore::new());
        let active = store.seed_task("stop me", 2);
        let terminal = store.seed_task("already done", 2);
        let missing = Uuid::new_v4();
        GraphStore::set_task_state_guarded(store.as_ref(), active.id, TaskState::Running)
            .await
            .unwrap();
        make_terminal(&store, &terminal, TaskState::Done).await;
        let app = router(test_state_with_store(store.clone()));

        let (status, json) = send(
            app,
            post_json(
                "/api/tasks/stop",
                json!({"ids": [active.id, terminal.id, missing]}),
            ),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["stopping"], json!([active.id]));
        let blocked = json["blocked"].as_array().unwrap();
        assert_eq!(blocked.len(), 2, "{json}");
        assert!(
            blocked
                .iter()
                .any(|row| row["id"] == terminal.id.to_string() && row["state"] == "done")
        );
        assert!(
            blocked
                .iter()
                .any(|row| row["id"] == missing.to_string() && row["state"] == "unknown")
        );
        assert_eq!(
            store.get_task(active.id).await.unwrap().unwrap().state,
            TaskState::Cancelled
        );
    }

    #[tokio::test]
    async fn stop_offers_no_cancelled_transition_from_terminal_states() {
        for terminal in [TaskState::Done, TaskState::Failed, TaskState::Timeout] {
            assert!(!terminal.can_transition_to(TaskState::Cancelled));
        }
    }

    // -----------------------------------------------------------------------
    // Task deletion
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn delete_unknown_task_is_404() {
        let app = router(test_state());
        let id = Uuid::new_v4();
        let (status, json) = send(app, delete_request(&format!("/api/tasks/{id}"))).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{json}");
    }

    #[tokio::test]
    async fn delete_active_task_is_409_with_the_frozen_message() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("still running", 2);
        GraphStore::set_task_state_guarded(store.as_ref(), task.id, TaskState::Running)
            .await
            .unwrap();
        let app = router(test_state_with_store(store.clone()));

        let (status, json) = send(app, delete_request(&format!("/api/tasks/{}", task.id))).await;

        assert_eq!(status, StatusCode::CONFLICT, "{json}");
        assert_eq!(json["error"], "stop the task first");
        assert!(store.get_task(task.id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn delete_refuses_a_terminal_task_while_its_loop_is_registered() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("terminal but registered", 2);
        make_terminal(&store, &task, TaskState::Done).await;
        let registry = Arc::new(TaskRunRegistry::new());
        let mut state = test_state_with_store(store.clone());
        state.task_runs = registry.clone();
        let app = router(state);

        let handle = tokio::spawn(std::future::pending::<()>());
        registry.register(
            task.id,
            Arc::new(AtomicBool::new(false)),
            handle.abort_handle(),
        );

        let (status, json) = send(app, delete_request(&format!("/api/tasks/{}", task.id))).await;

        assert_eq!(status, StatusCode::CONFLICT, "{json}");
        assert_eq!(json["error"], "stop the task first");
        assert!(store.get_task(task.id).await.unwrap().is_some());
        registry.abort_and_remove(task.id);
    }

    // -----------------------------------------------------------------------
    // Add-goal guarded reopen
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn add_goal_reopens_a_done_task_and_respawns_the_engine() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("reopen me", 3);
        make_terminal(&store, &task, TaskState::Done).await;
        let app = router(test_state_with_store(store.clone()));

        let (status, json) = send(app, post_goal(task.id, "a new goal")).await;

        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["task"]["id"], task.id.to_string());
        assert_eq!(
            json["task"]["state"], "running",
            "the response carries the reopened task"
        );
        assert_eq!(json["goal"]["task_id"], task.id.to_string());
        assert_eq!(json["goal"]["description"], "a new goal");
        assert_eq!(json["goal"]["state"], "open");
        assert!(json["goal"]["id"].is_string());

        let goal_id = json["goal"]["id"].as_str().unwrap();
        let activities = store.activities(task.id);
        let added = activities
            .iter()
            .find(|row| row.kind == "goal_added")
            .expect("goal_added activity");
        assert_eq!(added.message, "human added goal: a new goal");
        assert_eq!(added.payload["actor"], "human");
        assert_eq!(added.payload["goal_id"], goal_id);
        let reopened = activities
            .iter()
            .find(|row| row.kind == "task_reopened")
            .expect("task_reopened activity");
        assert_eq!(reopened.message, "task reopened by human: goal added");
        assert_eq!(reopened.payload["goal_id"], goal_id);
        assert_eq!(reopened.payload["previous_state"], "done");

        // The background loop is the create-task spawn path: without it the
        // task would stay `running` forever. The mock planner finishes it.
        wait_for_task_state(&store, task.id, TaskState::Done).await;
        assert!(
            store
                .activities(task.id)
                .iter()
                .any(|row| row.kind == "intent_claimed"),
            "the respawned engine must do work"
        );
    }

    #[tokio::test]
    async fn add_goal_rejects_a_non_terminal_task_with_its_state() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("still live", 3);
        GraphStore::set_task_state_guarded(store.as_ref(), task.id, TaskState::Running)
            .await
            .unwrap();
        let activity_before = store.activities(task.id).len();
        let app = router(test_state_with_store(store.clone()));

        let (status, json) = send(app, post_goal(task.id, "too early")).await;

        assert_eq!(status, StatusCode::CONFLICT, "{json}");
        assert!(
            json["error"]
                .as_str()
                .is_some_and(|message| message.contains("running")),
            "the conflict names the current state: {json}"
        );
        assert_eq!(
            store.get_task(task.id).await.unwrap().unwrap().state,
            TaskState::Running
        );
        assert!(store.goals(task.id).is_empty());
        assert_eq!(store.activities(task.id).len(), activity_before);
    }

    #[tokio::test]
    async fn add_goal_validates_the_description() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("validate goals", 3);
        make_terminal(&store, &task, TaskState::Done).await;
        let app = router(test_state_with_store(store.clone()));

        let (status, json) = send(app.clone(), post_goal(task.id, "   ")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
        assert!(json["error"].is_string());

        let too_long = "x".repeat(ADD_GOAL_MAX_CHARS + 1);
        let (status, json) = send(app, post_goal(task.id, &too_long)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
        assert!(
            json["error"]
                .as_str()
                .is_some_and(|message| message.contains("500")),
            "{json}"
        );

        // Rejected input never mutates the task.
        assert_eq!(
            store.get_task(task.id).await.unwrap().unwrap().state,
            TaskState::Done
        );
        assert!(store.goals(task.id).is_empty());
        assert!(
            !store
                .activities(task.id)
                .iter()
                .any(|row| row.kind == "task_reopened")
        );
    }

    #[tokio::test]
    async fn add_goal_unknown_task_returns_404() {
        let store = Arc::new(MemoryStore::new());
        let app = router(test_state_with_store(store));
        let missing = Uuid::new_v4();

        let (status, json) = send(app, post_goal(missing, "nobody home")).await;

        assert_eq!(status, StatusCode::NOT_FOUND, "{json}");
        assert!(json["error"].is_string());
    }

    #[tokio::test]
    async fn add_goal_mutates_only_the_target_task() {
        let store = Arc::new(MemoryStore::new());
        let target = store.seed_task("reopen only me", 3);
        let other = store.seed_task("leave me alone", 3);
        make_terminal(&store, &target, TaskState::Failed).await;
        make_terminal(&store, &other, TaskState::Done).await;
        let other_before = store.get_task(other.id).await.unwrap().unwrap();
        let other_activity_before = store.activities(other.id).len();
        let other_goals_before = store.goals(other.id).len();

        let app = router(test_state_with_store(store.clone()));
        let (status, json) = send(app, post_goal(target.id, "one more thing")).await;
        assert_eq!(status, StatusCode::OK, "{json}");

        let other_after = store.get_task(other.id).await.unwrap().unwrap();
        assert_eq!(other_after.state, TaskState::Done);
        assert_eq!(other_after.updated_at, other_before.updated_at);
        assert_eq!(store.activities(other.id).len(), other_activity_before);
        assert_eq!(store.goals(other.id).len(), other_goals_before);
        assert!(
            store
                .activities(target.id)
                .iter()
                .any(|row| row.kind == "task_reopened"),
            "the target task was reopened"
        );
        assert_eq!(store.goals(target.id).len(), 1);
    }

    // -----------------------------------------------------------------------
    // Task capacity extension
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn capacity_extension_merges_omitted_fields() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("extend my capacity", 3);
        GraphStore::set_task_capacity(
            store.as_ref(),
            task.id,
            TaskCapacity {
                max_rounds: Some(5),
                max_wall_secs: Some(60),
                ..TaskCapacity::default()
            },
        )
        .await
        .unwrap();
        let app = router(test_state_with_store(store.clone()));

        let (status, json) = send(
            app,
            post_capacity(task.id, json!({ "max_rounds": 80, "max_model_calls": 120 })),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["id"], task.id.to_string());
        assert_eq!(json["state"], "created");
        assert_eq!(json["capacity"]["max_rounds"], 80);
        assert_eq!(json["capacity"]["max_model_calls"], 120);
        assert_eq!(
            json["capacity"]["max_wall_secs"], 60,
            "an omitted field keeps its stored value"
        );
        assert!(json["capacity"]["max_parallel_workers"].is_null());

        let stored = store.get_task(task.id).await.unwrap().unwrap();
        assert_eq!(stored.capacity.max_rounds, Some(80));
        assert_eq!(stored.capacity.max_model_calls, Some(120));
        assert_eq!(stored.capacity.max_wall_secs, Some(60));
    }

    #[tokio::test]
    async fn capacity_extension_resumes_a_paused_task() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("resume me", 3);
        GraphStore::set_task_state_guarded(store.as_ref(), task.id, TaskState::Running)
            .await
            .unwrap();
        GraphStore::set_task_state_guarded(store.as_ref(), task.id, TaskState::Paused)
            .await
            .unwrap();
        let app = router(test_state_with_store(store.clone()));

        let (status, json) = send(app, post_capacity(task.id, json!({ "max_rounds": 80 }))).await;

        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(
            json["state"], "running",
            "extending a paused task restarts its loop"
        );
        assert_eq!(json["capacity"]["max_rounds"], 80);
        wait_for_task_state(&store, task.id, TaskState::Done).await;
        assert!(
            store
                .activities(task.id)
                .iter()
                .any(|row| row.kind == "intent_claimed"),
            "the respawned engine must do work"
        );
    }

    #[tokio::test]
    async fn capacity_extension_rejects_non_positive_values() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("validate capacity", 3);
        let app = router(test_state_with_store(store.clone()));

        for body in [
            json!({ "max_rounds": 0 }),
            json!({ "max_model_calls": -1 }),
            json!({ "max_wall_secs": 0 }),
            json!({ "max_parallel_workers": -2 }),
        ] {
            let (status, json) = send(app.clone(), post_capacity(task.id, body)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
            assert!(json["error"].is_string());
        }

        let malformed = Request::builder()
            .method("POST")
            .uri(format!("/api/tasks/{}/capacity", task.id))
            .header("content-type", "application/json")
            .body(Body::from("{not json"))
            .unwrap();
        let (status, json) = send(app.clone(), malformed).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");

        let wrong_shape = post_capacity(task.id, json!({ "max_rounds": "many" }));
        let (status, json) = send(app, wrong_shape).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");

        assert_eq!(
            store.get_task(task.id).await.unwrap().unwrap().capacity,
            TaskCapacity::default(),
            "rejected input never mutates the ledger"
        );
    }

    #[tokio::test]
    async fn capacity_extension_unknown_task_returns_404() {
        let store = Arc::new(MemoryStore::new());
        let app = router(test_state_with_store(store));
        let missing = Uuid::new_v4();

        let (status, json) = send(app, post_capacity(missing, json!({ "max_rounds": 80 }))).await;

        assert_eq!(status, StatusCode::NOT_FOUND, "{json}");
        assert!(json["error"].is_string());
    }

    #[tokio::test]
    async fn capacity_extension_rejects_terminal_tasks() {
        for state in [
            TaskState::Done,
            TaskState::Failed,
            TaskState::Timeout,
            TaskState::Cancelled,
        ] {
            let store = Arc::new(MemoryStore::new());
            let task = store.seed_task("terminal capacity", 3);
            make_terminal(&store, &task, state).await;
            let app = router(test_state_with_store(store.clone()));

            let (status, json) =
                send(app, post_capacity(task.id, json!({ "max_rounds": 80 }))).await;

            assert_eq!(status, StatusCode::CONFLICT, "{state}: {json}");
            let stored = store.get_task(task.id).await.unwrap().unwrap();
            assert_eq!(stored.state, state);
            assert_eq!(
                stored.capacity,
                TaskCapacity::default(),
                "{state}: a refused extension must not write the ledger"
            );
        }
    }

    #[tokio::test]
    async fn list_tasks_returns_created_task() {
        let store = Arc::new(MemoryStore::new());
        let created = store.seed_task("seeded", 2);
        let app = router(test_state_with_store(store));
        let (status, json) = send(app, get_uri("/api/tasks?limit=100")).await;
        assert_eq!(status, StatusCode::OK);
        let tasks = json["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["id"], created.id.to_string());
    }

    #[tokio::test]
    async fn get_task_returns_task() {
        let store = Arc::new(MemoryStore::new());
        let created = store.seed_task("find the fork", 4);
        let app = router(test_state_with_store(store));
        let (status, json) = send(app, get_uri(&format!("/api/tasks/{}", created.id))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["id"], created.id.to_string());
        assert_eq!(json["goal"], "find the fork");
    }

    #[tokio::test]
    async fn get_task_work_returns_collections() {
        let store = Arc::new(MemoryStore::new());
        let created = store.seed_task("clip the king", 3);
        let store_trait: Arc<dyn GraphStore> = store.clone();
        let goal = GraphStore::add_goal(store_trait.as_ref(), created.id, "reach mate".into())
            .await
            .unwrap();
        store.seed_intent(created.id, created.origin_fact_id, "try Qh5");

        let app = router(test_state_with_store(store));
        let (status, json) = send(app, get_uri(&format!("/api/tasks/{}/work", created.id))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["task"]["id"], created.id.to_string());
        let goals = json["goals"].as_array().unwrap();
        assert_eq!(goals.len(), 1);
        assert_eq!(goals[0]["id"], goal.id.to_string());
        assert_eq!(json["intents"].as_array().unwrap().len(), 1);
        // The origin fact plus nothing else.
        assert_eq!(json["facts"].as_array().unwrap().len(), 1);
        assert!(
            json["results"].as_array().unwrap().is_empty(),
            "the engine writes no result rows; completion is a fact proof"
        );
    }

    #[tokio::test]
    async fn get_task_activity_returns_activity() {
        let store = Arc::new(MemoryStore::new());
        let created = store.seed_task("watch the back rank", 2);
        let store_trait: Arc<dyn GraphStore> = store.clone();
        GraphStore::append_activity(
            store_trait.as_ref(),
            created.id,
            "note",
            "hi".into(),
            json!({}),
        )
        .await
        .unwrap();

        let app = router(test_state_with_store(store));
        let (status, json) =
            send(app, get_uri(&format!("/api/tasks/{}/activity", created.id))).await;
        assert_eq!(status, StatusCode::OK);
        let activity = json["activity"].as_array().unwrap();
        assert_eq!(activity.len(), 1);
        assert_eq!(activity[0]["kind"], "note");
    }

    #[tokio::test]
    async fn get_task_swarm_messages_orders_by_id() {
        let store = Arc::new(MemoryStore::new());
        let created = store.seed_task("split the swarm", 4);
        let intent = store.seed_intent(created.id, created.origin_fact_id, "run the swarm");
        let store_trait: Arc<dyn GraphStore> = store.clone();
        for index in 0..3 {
            GraphStore::append_swarm_message(
                store_trait.as_ref(),
                created.id,
                intent.id,
                "finding",
                json!({ "index": index }),
            )
            .await
            .unwrap();
        }

        let app = router(test_state_with_store(store));
        let (status, json) = send(
            app,
            get_uri(&format!("/api/tasks/{}/swarm-messages", created.id)),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let messages = json["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        let ids: Vec<i64> = messages
            .iter()
            .map(|message| message["id"].as_i64().unwrap())
            .collect();
        assert!(
            ids.windows(2).all(|pair| pair[0] < pair[1]),
            "messages are ordered by id: {ids:?}"
        );
        for (index, message) in messages.iter().enumerate() {
            assert_eq!(message["task_id"], created.id.to_string());
            assert_eq!(message["intent_id"], intent.id.to_string());
            assert_eq!(message["kind"], "finding");
            assert_eq!(message["payload"]["index"], index as i64);
            assert!(message["created_at"].is_string());
        }
    }

    #[tokio::test]
    async fn get_task_swarm_messages_empty_task_returns_empty_list() {
        let store = Arc::new(MemoryStore::new());
        let created = store.seed_task("no swarm yet", 2);

        let app = router(test_state_with_store(store));
        let (status, json) = send(
            app,
            get_uri(&format!("/api/tasks/{}/swarm-messages", created.id)),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["messages"], json!([]));
    }

    #[tokio::test]
    async fn get_task_swarm_messages_clamps_limit() {
        let store = Arc::new(MemoryStore::new());
        let created = store.seed_task("many messages", 2);
        let intent = store.seed_intent(created.id, created.origin_fact_id, "run the swarm");
        let store_trait: Arc<dyn GraphStore> = store.clone();
        for index in 0..1_005 {
            GraphStore::append_swarm_message(
                store_trait.as_ref(),
                created.id,
                intent.id,
                "finding",
                json!({ "index": index }),
            )
            .await
            .unwrap();
        }

        let app = router(test_state_with_store(store));

        let (status, json) = send(
            app.clone(),
            get_uri(&format!("/api/tasks/{}/swarm-messages", created.id)),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            json["messages"].as_array().unwrap().len(),
            200,
            "limit defaults to 200"
        );

        let (status, json) = send(
            app.clone(),
            get_uri(&format!("/api/tasks/{}/swarm-messages?limit=0", created.id)),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            json["messages"].as_array().unwrap().len(),
            1,
            "limit below the range clamps to 1"
        );

        let (status, json) = send(
            app,
            get_uri(&format!(
                "/api/tasks/{}/swarm-messages?limit=5000",
                created.id
            )),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            json["messages"].as_array().unwrap().len(),
            1_000,
            "limit above the range clamps to 1000"
        );
    }

    #[tokio::test]
    async fn missing_task_returns_404() {
        let app = router(test_state());
        for suffix in ["", "/work", "/activity", "/swarm-messages"] {
            let (status, json) = send(
                app.clone(),
                get_uri(&format!("/api/tasks/{}{suffix}", Uuid::new_v4())),
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "suffix {suffix:?}");
            assert!(json["error"].is_string());
        }
    }
    #[tokio::test]
    async fn list_pending_approvals_returns_rows() {
        let store = Arc::new(MemoryStore::new());
        let pending = store.seed_task("wait for review", 3);
        let decided = store.seed_task("already reviewed", 3);
        let kept = seed_pending_approval(&store, &pending).await;
        let closed = seed_pending_approval(&store, &decided).await;
        GuardStore::decide_approval(store.as_ref(), closed.id, ApprovalDecision::Deny)
            .await
            .unwrap()
            .expect("a pending approval can be denied");

        let app = router(guarded_state(store));
        let (status, json) = send(app, get_uri("/api/approvals")).await;
        assert_eq!(status, StatusCode::OK);
        let approvals = json["approvals"].as_array().unwrap();
        assert_eq!(approvals.len(), 1, "only pending approvals are listed");
        assert_eq!(approvals[0]["id"], kept.id.to_string());
        assert_eq!(approvals[0]["task_id"], pending.id.to_string());
        assert_eq!(approvals[0]["status"], "pending");
        assert_eq!(approvals[0]["tool_name"], "finishTurn");
        assert_eq!(approvals[0]["input"], json!({ "move": "Qh5" }));
    }

    #[tokio::test]
    async fn list_approvals_missing_tool_call_returns_null_detail() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("missing tool call row", 3);
        let approval = GuardStore::create_approval(
            store.as_ref(),
            task.id,
            Uuid::new_v4(),
            Uuid::new_v4(),
            "needs a person".to_string(),
            chrono::Utc::now() + chrono::Duration::minutes(5),
        )
        .await
        .unwrap();

        let app = router(guarded_state(store));
        let (status, json) = send(app, get_uri("/api/approvals")).await;
        assert_eq!(status, StatusCode::OK);
        let approvals = json["approvals"].as_array().unwrap();
        assert_eq!(approvals.len(), 1);
        assert_eq!(approvals[0]["id"], approval.id.to_string());
        assert!(approvals[0]["tool_name"].is_null());
        assert!(approvals[0]["input"].is_null());
    }

    /// The decision route resolves the approval and returns the task still
    /// `running`. It never executes the tool or finishes the intent.
    #[tokio::test]
    async fn approval_decision_allow_returns_granted_and_running_task() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("let the tool run", 3);
        let approval = seed_pending_approval(&store, &task).await;

        let app = router(guarded_state(store.clone()));
        let (status, json) = send(
            app,
            post_json(
                &format!("/api/approvals/{}/decision", approval.id),
                json!({ "decision": "allow" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["approval"]["id"], approval.id.to_string());
        assert_eq!(json["approval"]["status"], "granted");
        assert_eq!(json["task"]["id"], task.id.to_string());
        assert_eq!(json["task"]["state"], "running");

        // The route only resolves the row: the intent stays running and the
        // tool call is left for the worker that owns the wait.
        let intents = store.intents(task.id);
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].state, dhi_core::IntentState::Running);
        let call = GuardStore::get_tool_call(store.as_ref(), approval.tool_call_id)
            .await
            .unwrap()
            .expect("the seeded tool call exists");
        assert!(call.output.is_none());
        assert_eq!(call.status, "pending");
        assert_eq!(
            store.facts(task.id).len(),
            1,
            "the route must not record a tool fact"
        );

        let stored = GuardStore::get_approval(store.as_ref(), approval.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.status, ApprovalStatus::Granted);
        assert_eq!(
            store.get_task(task.id).await.unwrap().unwrap().state,
            TaskState::Running
        );
    }

    /// Only the first decision wins; a second call returns `409`.
    #[tokio::test]
    async fn second_decision_returns_409() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("decide once", 3);
        let approval = seed_pending_approval(&store, &task).await;
        let app = router(guarded_state(store.clone()));

        let (status, json) = send(
            app.clone(),
            post_json(
                &format!("/api/approvals/{}/decision", approval.id),
                json!({ "decision": "allow" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json["approval"]["status"], "granted");

        let (status, json) = send(
            app,
            post_json(
                &format!("/api/approvals/{}/decision", approval.id),
                json!({ "decision": "deny" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{json}");
        assert!(json["error"].is_string());

        let stored = GuardStore::get_approval(store.as_ref(), approval.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.status,
            ApprovalStatus::Granted,
            "the second decision must not overwrite the first"
        );
    }

    #[tokio::test]
    async fn approval_decision_missing_returns_404() {
        let app = router(guarded_state(Arc::new(MemoryStore::new())));
        let (status, json) = send(
            app,
            post_json(
                &format!("/api/approvals/{}/decision", Uuid::new_v4()),
                json!({ "decision": "allow" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(json["error"].is_string());
    }

    #[tokio::test]
    async fn approval_decision_not_pending_returns_409() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("already handled", 3);
        let approval = seed_pending_approval(&store, &task).await;
        GuardStore::decide_approval(store.as_ref(), approval.id, ApprovalDecision::Deny)
            .await
            .unwrap()
            .expect("a pending approval can be denied");

        let app = router(guarded_state(store));
        let (status, json) = send(
            app,
            post_json(
                &format!("/api/approvals/{}/decision", approval.id),
                json!({ "decision": "allow" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(json["error"].is_string());
    }

    /// An approval that is still stored as `pending` when the decision route
    /// reads it, but whose `expires_at` has already passed by the time
    /// `decide_approval` runs in the store, transitions to `expired` and
    /// returns `409` instead of `200`.
    #[tokio::test]
    async fn approval_decision_expired_race_returns_409() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("expired before decision", 3);
        let intent = store.seed_intent(task.id, task.origin_fact_id, "await a decision");
        GraphStore::set_task_state_guarded(store.as_ref(), task.id, TaskState::Running)
            .await
            .unwrap();
        let claimed = store
            .claim_intent(intent.id, "worker-1")
            .await
            .unwrap()
            .expect("the seeded intent is claimable");
        let call = GuardStore::record_tool_call(
            store.as_ref(),
            task.id,
            claimed.id,
            "finishTurn".to_string(),
            json!({ "move": "Qh5" }),
        )
        .await
        .unwrap();
        let decision = GuardStore::record_guard_decision(
            store.as_ref(),
            call.id,
            dhi_core::GuardVerdict::Ask,
            None,
            None,
            "needs a person".to_string(),
        )
        .await
        .unwrap();
        let approval = GuardStore::create_approval(
            store.as_ref(),
            task.id,
            call.id,
            decision.id,
            "needs a person".to_string(),
            chrono::Utc::now() - chrono::Duration::seconds(1),
        )
        .await
        .unwrap();
        assert_eq!(approval.status, ApprovalStatus::Pending);

        let app = router(guarded_state(store.clone()));
        let (status, json) = send(
            app,
            post_json(
                &format!("/api/approvals/{}/decision", approval.id),
                json!({ "decision": "allow" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{json}");
        assert!(
            json["error"]
                .as_str()
                .is_some_and(|msg| msg.contains("expired")),
            "unexpected error: {json}"
        );

        let stored = GuardStore::get_approval(store.as_ref(), approval.id)
            .await
            .unwrap()
            .expect("the approval row still exists");
        assert_eq!(stored.status, ApprovalStatus::Expired);
        assert!(stored.decided_at.is_none());
    }

    #[tokio::test]
    async fn task_usage_returns_runs() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("count the calls", 2);
        let mut run = dhi_core::AgentRun {
            id: Uuid::new_v4(),
            run_id: Uuid::new_v4(),
            call_index: 0,
            task_id: task.id,
            intent_id: None,
            worker_key: None,
            role: dhi_core::AgentRole::Worker,
            model: "test-model".to_string(),
            profile: "fake/test-model/max_tokens=1/stream=false".to_string(),
            prompt: "prompt".to_string(),
            prompt_hash: "hash".to_string(),
            variables: json!({}),
            response: Some("reply".to_string()),
            thinking: Some("checked the call count".to_string()),
            tokens_in: 10,
            tokens_out: 5,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            cost: 0.0,
            status: "ok".to_string(),
            error: None,
            latency_ms: 3,
            checkpoint_id: None,
            created_at: task.created_at,
            updated_at: task.created_at,
        };
        GraphStore::save_agent_run(store.as_ref(), &run)
            .await
            .unwrap();
        run.id = Uuid::new_v4();
        run.call_index = 1;
        GraphStore::save_agent_run(store.as_ref(), &run)
            .await
            .unwrap();

        let app = router(guarded_state(store));
        let (status, json) = send(
            app.clone(),
            get_uri(&format!("/api/tasks/{}/usage", task.id)),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let runs = json["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0]["call_index"], 0);
        assert_eq!(runs[1]["call_index"], 1);
        assert_eq!(runs[0]["tokens_in"], 10);
        assert_eq!(runs[0]["thinking"], "checked the call count");

        let (status, json) = send(
            app,
            get_uri(&format!("/api/tasks/{}/usage", Uuid::new_v4())),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(json["error"].is_string());
    }

    fn stream_uri(task_id: impl std::fmt::Display, after: Option<i64>) -> String {
        match after {
            Some(after) => format!("/api/stream?task_id={task_id}&after={after}"),
            None => format!("/api/stream?task_id={task_id}"),
        }
    }

    fn get(uri: &str) -> Request<Body> {
        Request::builder().uri(uri).body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn stream_returns_event_stream_content_type() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("stream it", 2);
        let app = router(test_state_with_store(store));
        let response = app.oneshot(get(&stream_uri(task.id, None))).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .expect("the stream sets a content type")
            .to_str()
            .unwrap();
        assert!(
            content_type.starts_with("text/event-stream"),
            "unexpected content type: {content_type}"
        );
    }

    #[tokio::test]
    async fn stream_missing_task_returns_404() {
        let app = router(test_state());
        let (status, json) = send(app, get(&stream_uri(Uuid::new_v4(), None))).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(json["error"].is_string());
    }

    #[tokio::test]
    async fn stream_rejects_bad_uuid() {
        let app = router(test_state());
        let (status, json) = send(app, get("/api/stream?task_id=not-a-uuid")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(json["error"].is_string());
    }

    #[tokio::test]
    async fn stream_sends_initial_hello() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("say hello", 2);
        let app = router(test_state_with_store(store));
        let response = app.oneshot(get(&stream_uri(task.id, None))).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body().into_data_stream();

        let frame = tokio::time::timeout(Duration::from_secs(2), body.next())
            .await
            .expect("the first frame arrives promptly")
            .expect("the stream yields a frame")
            .unwrap();
        let frame = String::from_utf8(frame.to_vec()).unwrap();
        assert!(frame.starts_with("event: hello"), "frame: {frame:?}");
        assert!(
            frame.contains(&task.id.to_string()),
            "the hello names the task: {frame:?}"
        );
    }

    #[tokio::test]
    async fn stream_sends_every_new_activity_row_per_poll() {
        let store = Arc::new(MemoryStore::new());
        let task = store.seed_task("burst of activity", 2);
        let store_trait: Arc<dyn GraphStore> = store.clone();
        let skipped = GraphStore::append_activity(
            store_trait.as_ref(),
            task.id,
            "before_cursor",
            "skip me".into(),
            json!({ "seq": 1 }),
        )
        .await
        .unwrap();
        for seq in 2..=4 {
            GraphStore::append_activity(
                store_trait.as_ref(),
                task.id,
                "step",
                format!("step {seq}"),
                json!({ "seq": seq }),
            )
            .await
            .unwrap();
        }

        let app = router(test_state_with_store(store));
        let response = app
            .oneshot(get(&stream_uri(task.id, Some(skipped.id))))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body().into_data_stream();

        let hello = tokio::time::timeout(Duration::from_secs(2), body.next())
            .await
            .expect("hello arrives promptly")
            .expect("stream yields hello")
            .unwrap();
        let hello = String::from_utf8(hello.to_vec()).unwrap();
        assert!(hello.starts_with("event: hello"), "hello: {hello:?}");

        // The first poll fires after STREAM_POLL_INTERVAL (1s) and must emit
        // all three new rows in a single burst rather than one row per second.
        let first = tokio::time::timeout(Duration::from_millis(1600), body.next())
            .await
            .expect("first activity row arrives on the first poll")
            .expect("stream yields first activity")
            .unwrap();
        let second = tokio::time::timeout(Duration::from_millis(200), body.next())
            .await
            .expect("second activity row arrives in the same poll burst")
            .expect("stream yields second activity")
            .unwrap();
        let third = tokio::time::timeout(Duration::from_millis(200), body.next())
            .await
            .expect("third activity row arrives in the same poll burst")
            .expect("stream yields third activity")
            .unwrap();

        let frames = [first, second, third]
            .into_iter()
            .map(|bytes| String::from_utf8(bytes.to_vec()).unwrap())
            .collect::<Vec<_>>();
        for (idx, expected_id) in [2_i64, 3, 4].into_iter().enumerate() {
            let frame = &frames[idx];
            assert!(
                frame.contains("event: activity"),
                "frame {idx} is an activity event: {frame:?}"
            );
            assert!(
                frame.contains(&format!("id: {expected_id}")),
                "frame {idx} carries id {expected_id}: {frame:?}"
            );
            assert!(
                frame.contains(&format!("step {expected_id}")),
                "frame {idx} carries message step {expected_id}: {frame:?}"
            );
        }
    }

    // ------------------------------------------------------------------
    // Specialist audit sink
    // ------------------------------------------------------------------

    fn specialist_record(task_id: Uuid, intent_id: Uuid) -> dhi_agent::SpecialistRunRecord {
        dhi_agent::SpecialistRunRecord {
            run_id: Uuid::new_v4(),
            task_id,
            intent_id,
            specialist: "researcher".to_string(),
            model: "glm-4.7".to_string(),
            profile: "openai/glm-4.7/max_tokens=0/stream=false".to_string(),
            prompt_text: "Task: read the page\n\nExpected: the move list".to_string(),
            response: Some("1.d4 d5 2.c4".to_string()),
            thinking: Some("the page lists the opening moves".to_string()),
            usage: dhi_agent::Usage {
                input_tokens: 120,
                output_tokens: 24,
                cache_read_tokens: 3,
                cache_write_tokens: 5,
                reasoning_tokens: 7,
                cost_usd: Some(0.25),
            },
            status: "ok".to_string(),
            error: None,
            latency_ms: 432,
        }
    }

    #[tokio::test]
    async fn a_specialist_run_becomes_one_tagged_agent_run_row() {
        let store = Arc::new(MemoryStore::new());
        let sink = SpecialistAuditSink::new(store.clone());
        let (task_id, intent_id) = (Uuid::new_v4(), Uuid::new_v4());
        let record = specialist_record(task_id, intent_id);
        sink.record_specialist_run(record.clone()).await;

        let rows = store.agent_runs(task_id);
        assert_eq!(rows.len(), 1, "one delegation call = one row");
        let row = &rows[0];
        assert_eq!(row.run_id, record.run_id);
        assert_eq!(row.intent_id, Some(intent_id));
        assert_eq!(
            row.role,
            AgentRole::Worker,
            "specialists run the worker role"
        );
        assert_eq!(row.model, "glm-4.7");
        assert_eq!(row.profile, record.profile);
        assert_eq!(row.prompt, record.prompt_text);
        assert!(!row.prompt_hash.is_empty(), "the row carries a prompt hash");
        assert_eq!(row.response.as_deref(), Some("1.d4 d5 2.c4"));
        assert_eq!(
            row.thinking.as_deref(),
            Some("the page lists the opening moves")
        );
        assert_eq!(row.tokens_in, 120);
        assert_eq!(row.tokens_out, 24);
        assert_eq!(row.cache_read_tokens, 3);
        assert_eq!(row.cache_write_tokens, 5);
        assert_eq!(row.reasoning_tokens, 7);
        assert!((row.cost - 0.25).abs() < 1e-9);
        assert_eq!(row.status, "ok");
        assert_eq!(row.error, None);
        assert_eq!(row.latency_ms, 432);
        // The tag lives in variables: the plain name and `specialist:<name>`.
        assert_eq!(row.variables["specialist"], "researcher");
        assert_eq!(row.variables["tag"], "specialist:researcher");
    }

    #[tokio::test]
    async fn a_failed_specialist_run_records_the_error_row() {
        let store = Arc::new(MemoryStore::new());
        let sink = SpecialistAuditSink::new(store.clone());
        let (task_id, intent_id) = (Uuid::new_v4(), Uuid::new_v4());
        let mut record = specialist_record(task_id, intent_id);
        record.response = None;
        record.status = "error".to_string();
        record.error = Some("specialist call failed: connection refused".to_string());
        record.profile = String::new();
        let expected = record.clone();
        sink.record_specialist_run(record).await;

        let rows = store.agent_runs(task_id);
        assert_eq!(rows.len(), 1, "failures are audited too");
        let row = &rows[0];
        assert_eq!(row.status, "error");
        assert_eq!(row.error, expected.error);
        assert_eq!(row.response, None);
        assert_eq!(row.variables["tag"], "specialist:researcher");
        assert_eq!(row.intent_id, Some(intent_id));
    }

    #[tokio::test]
    async fn delegation_events_become_activity_rows() {
        let store = Arc::new(MemoryStore::new());
        let sink = SpecialistAuditSink::new(store.clone());
        let task_id = Uuid::new_v4();
        sink.record_event(
            task_id,
            "agent_delegated",
            "delegated to specialist researcher".to_string(),
            json!({ "specialist": "researcher" }),
        )
        .await;
        sink.record_event(
            task_id,
            "agent_returned",
            "specialist researcher returned".to_string(),
            json!({ "specialist": "researcher", "status": "ok" }),
        )
        .await;

        let kinds: Vec<String> = store
            .activities(task_id)
            .into_iter()
            .map(|activity| activity.kind)
            .collect();
        assert_eq!(kinds, vec!["agent_delegated", "agent_returned"]);
    }
}
