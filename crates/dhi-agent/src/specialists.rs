//! Worker-manager and specialists.
//!
//! The worker becomes a **manager**: alongside `record_fact` and the guarded
//! domain tools, its registry carries one delegation tool per configured
//! specialist (`delegate_to_<name>`, typed args `{ task, context, expected }`).
//! Each specialist is a Rig agent with its OWN registry holding only its
//! allowed guarded tools — the same registrations, the same one guard, the
//! same run scope (a specialist's tool calls record under the manager's
//! claimed intent). Depth is structural: specialist registries are built
//! from guarded tools only and can never contain delegation tools, so
//! delegation depth is exactly 1.
//!
//! The manager keeps `record_fact` for itself; specialists never see it. A
//! specialist's reply is a string the manager reads and must still turn into
//! its own `record_fact` — delegation never records a fact, never finishes an
//! intent, and never proves a goal.
//!
//! Usage: every specialist completion call is reported to a [`RuntimeSink`]
//! (implemented in dhi-server over the store — dhi-agent holds no store), one
//! `agent_runs` row tagged `specialist:<name>`, plus `agent_delegated` /
//! `agent_returned` activity events.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use anyhow::anyhow;
use async_trait::async_trait;
use rig::prelude::*;
use rig::tool::server::{ToolServer, ToolServerHandle};
use rig::tool::{DynamicTool, ToolContext, ToolExecutionError, ToolOutput};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{info, warn};
use uuid::Uuid;

use crate::tools::RunScope;
use crate::{ProfileFormat, ResolvedProfile, Role, RoleProfiles, Usage, usage_from_rig};

/// Default delegations one manager run may make.
pub const DEFAULT_MAX_DELEGATIONS: usize = 8;
/// Default model-call budget for one manager run.
pub const DEFAULT_MANAGER_MAX_TURNS: usize = 12;
/// Default model-call budget for one specialist run (per entry overridable).
pub const DEFAULT_SPECIALIST_MAX_TURNS: usize = 6;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Delegation limits, resolved from the environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DelegationLimits {
    /// How many delegations one manager (worker) run may make.
    pub max_delegations: usize,
    /// The model-call budget of one manager run while specialists exist.
    pub manager_max_turns: usize,
}

impl Default for DelegationLimits {
    fn default() -> Self {
        Self {
            max_delegations: DEFAULT_MAX_DELEGATIONS,
            manager_max_turns: DEFAULT_MANAGER_MAX_TURNS,
        }
    }
}

impl DelegationLimits {
    /// `DHI_MAX_DELEGATIONS` (default 8) and `DHI_MANAGER_MAX_TURNS`
    /// (default 12). An invalid value keeps the default and warns.
    pub fn from_env() -> Self {
        let parse = |key: &str, default: usize| -> usize {
            match std::env::var(key) {
                Ok(raw) => raw.trim().parse().unwrap_or_else(|_| {
                    warn!("invalid {key}={raw}; using default {default}");
                    default
                }),
                Err(_) => default,
            }
        };
        Self {
            max_delegations: parse("DHI_MAX_DELEGATIONS", DEFAULT_MAX_DELEGATIONS),
            manager_max_turns: parse("DHI_MANAGER_MAX_TURNS", DEFAULT_MANAGER_MAX_TURNS),
        }
    }
}

/// One entry of `ops/specialists.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecialistEntry {
    /// Registry name; becomes the tool `delegate_to_<name>`.
    pub name: String,
    /// The manager-facing description of when to delegate.
    pub description: String,
    /// Preamble file name under `ops/prompts/specialists/`.
    pub preamble_file: String,
    /// The guarded tools this specialist may call. Everything else is
    /// invisible to it.
    pub tools: Vec<String>,
    /// Model-call budget for one specialist run. `None` uses the default.
    #[serde(default)]
    pub max_turns: Option<usize>,
    /// Optional pinned profile for the specialist's own LLM calls.
    #[serde(default)]
    pub worker_profile_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct SpecialistFile {
    specialists: Vec<SpecialistEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecialistSpec {
    pub name: String,
    pub description: String,
    /// The resolved preamble text.
    pub preamble: String,
    pub tools: Vec<String>,
    pub max_turns: usize,
    pub worker_profile_id: Option<i64>,
}

#[derive(Debug, Error)]
pub enum SpecialistError {
    #[error("specialists config {path}: {message}")]
    Io { path: String, message: String },
    #[error("specialists config {path} is not valid JSON: {message}")]
    Parse { path: String, message: String },
    #[error("specialist {name:?}: {message}")]
    Invalid { name: String, message: String },
}

/// The worker's (manager's) reporting tool. Specialists never carry it: a
/// delegation returns a finding the manager reads, and only the manager
/// records facts (one `record_fact` per intent, in `WorkerSink`).
pub const MANAGER_ONLY_TOOLS: &[&str] = &["record_fact"];

fn is_manager_only(tool: &str) -> bool {
    MANAGER_ONLY_TOOLS.contains(&tool)
}

fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Loads and validates the specialist registry from one JSON file plus the
/// preamble directory. The mechanism accepts arbitrary entries: name,
/// description, tools, budgets, and pinned profile are all data.
pub fn load_specialists(
    config_path: &Path,
    prompts_dir: &Path,
) -> Result<Vec<SpecialistSpec>, SpecialistError> {
    let path = config_path.display().to_string();
    let raw = std::fs::read_to_string(config_path).map_err(|err| SpecialistError::Io {
        path: path.clone(),
        message: err.to_string(),
    })?;
    let file: SpecialistFile =
        serde_json::from_str(&raw).map_err(|err| SpecialistError::Parse {
            path: path.clone(),
            message: err.to_string(),
        })?;

    let mut seen = BTreeMap::new();
    let mut specs = Vec::with_capacity(file.specialists.len());
    for entry in file.specialists {
        validate_entry(&entry)?;
        if seen.insert(entry.name.clone(), true).is_some() {
            return Err(SpecialistError::Invalid {
                name: entry.name.clone(),
                message: "duplicate specialist name".to_string(),
            });
        }
        let preamble_path = prompts_dir.join(&entry.preamble_file);
        let preamble =
            std::fs::read_to_string(&preamble_path).map_err(|err| SpecialistError::Io {
                path: preamble_path.display().to_string(),
                message: err.to_string(),
            })?;
        if preamble.trim().is_empty() {
            return Err(SpecialistError::Invalid {
                name: entry.name.clone(),
                message: format!("preamble file {} is empty", entry.preamble_file),
            });
        }
        specs.push(SpecialistSpec {
            name: entry.name,
            description: entry.description,
            preamble,
            tools: entry.tools,
            max_turns: entry.max_turns.unwrap_or(DEFAULT_SPECIALIST_MAX_TURNS),
            worker_profile_id: entry.worker_profile_id,
        });
    }
    Ok(specs)
}

fn validate_entry(entry: &SpecialistEntry) -> Result<(), SpecialistError> {
    if !is_valid_name(&entry.name) {
        return Err(SpecialistError::Invalid {
            name: entry.name.clone(),
            message: "name must be lowercase letters, digits, and underscores".to_string(),
        });
    }
    if entry.description.trim().is_empty() {
        return Err(SpecialistError::Invalid {
            name: entry.name.clone(),
            message: "description must be a non-empty string".to_string(),
        });
    }
    for tool in &entry.tools {
        if is_manager_only(tool) {
            return Err(SpecialistError::Invalid {
                name: entry.name.clone(),
                message: format!(
                    "{tool:?} is the manager's reporting tool; specialists never \
                     record facts (delegation returns findings, the manager records)"
                ),
            });
        }
        if tool.starts_with("delegate_to_") {
            return Err(SpecialistError::Invalid {
                name: entry.name.clone(),
                message: format!(
                    "a specialist cannot delegate ({tool:?} is a delegation tool); \
                     delegation depth is 1"
                ),
            });
        }
    }
    match entry.max_turns {
        Some(0) => Err(SpecialistError::Invalid {
            name: entry.name.clone(),
            message: "max_turns must be at least 1".to_string(),
        }),
        _ => Ok(()),
    }
}

/// Discovers the specialist registry the way `PromptSet::discover` finds the
/// prompts: `DHI_PROMPTS_DIR` selects the prompts directory when set, else
/// the nearest ancestor of `start` with an `ops/prompts` directory. The
/// config is `<ops>/specialists.json`; preambles live under
/// `<prompts>/specialists/`.
///
/// Returns `Ok(None)` when there is no ops directory or no config file (a
/// deployment without specialists runs its manager ungated — no delegation
/// tools, no behavior change), and `Err` when a config exists but is invalid.
pub fn discover_specialists(start: &Path) -> Result<Option<Vec<SpecialistSpec>>, SpecialistError> {
    let Some(prompts_dir) = crate::profile::find_prompts_dir(start) else {
        return Ok(None);
    };
    let Some(ops_dir) = prompts_dir.parent() else {
        return Ok(None);
    };
    let config = ops_dir.join("specialists.json");
    if !config.is_file() {
        return Ok(None);
    }
    load_specialists(&config, &prompts_dir.join("specialists")).map(Some)
}

/// Splits specs into those whose allowed tools are ALL registered on the
/// shared registry (attachable) and those with at least one unregistered
/// tool (skipped, with the missing names for the log). A deployment whose
/// optional tools (Firecrawl, MCP) are not configured therefore runs its
/// remaining specialists instead of refusing to boot; the attached half still
/// goes through the strict [`SpecialistFleet::new`] build.
pub fn partition_by_registered_tools(
    specs: Vec<SpecialistSpec>,
    shared: &ToolServerHandle,
) -> (Vec<SpecialistSpec>, Vec<(String, Vec<String>)>) {
    let registered: BTreeSet<String> = shared.toolset().names().map(str::to_string).collect();
    let mut attachable = Vec::new();
    let mut skipped = Vec::new();
    for spec in specs {
        let missing: Vec<String> = spec
            .tools
            .iter()
            .filter(|tool| !registered.contains(*tool))
            .cloned()
            .collect();
        if missing.is_empty() {
            attachable.push(spec);
        } else {
            skipped.push((spec.name, missing));
        }
    }
    (attachable, skipped)
}

// ---------------------------------------------------------------------------
// Audit: the sink the host implements (no store dependency here)
// ---------------------------------------------------------------------------

/// One specialist completion call, as the sink records it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpecialistRunRecord {
    /// Fresh id for the delegation call (the `agent_runs.run_id`).
    pub run_id: Uuid,
    pub task_id: Uuid,
    pub intent_id: Uuid,
    /// The specialist name; the host tags the row `specialist:<name>`.
    pub specialist: String,
    pub model: String,
    /// `<provider>/<model>/max_tokens=<n>/stream=<bool>`; no secret. Empty on
    /// the error path when no candidate answered.
    pub profile: String,
    pub prompt_text: String,
    pub response: Option<String>,
    /// The provider's final reasoning text, when it exposed any.
    pub thinking: Option<String>,
    pub usage: Usage,
    pub status: String,
    pub error: Option<String>,
    pub latency_ms: i64,
}

/// Where specialist usage and runtime events go. dhi-server implements this
/// over the store; dhi-agent holds no store. Record methods are best effort:
/// implementors log failures; a failed audit write never breaks a run.
#[async_trait]
pub trait RuntimeSink: Send + Sync {
    /// One specialist completion call → one `agent_runs` row tagged
    /// `specialist:<name>`.
    async fn record_specialist_run(&self, record: SpecialistRunRecord);
    /// Runtime activity events (`agent_delegated`, `agent_returned`).
    async fn record_event(
        &self,
        task_id: Uuid,
        kind: &str,
        message: String,
        payload: serde_json::Value,
    ) {
        let _ = (task_id, kind, message, payload);
    }
}

// ---------------------------------------------------------------------------
// The delegation runner seam: testable without an LLM
// ---------------------------------------------------------------------------

/// One delegation request as the tool hands it over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecialistRequest {
    pub name: String,
    pub task: String,
    pub context: String,
    pub expected: String,
    /// The manager run's scope, read from the dispatch context — never from
    /// model arguments.
    pub scope: RunScope,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpecialistReply {
    pub response: String,
    pub usage: Usage,
}

/// Runs one specialist. The real implementation drives the pool; tests use a
/// fake. Errors are model-readable tool errors.
#[async_trait]
pub trait SpecialistRunner: Send + Sync {
    async fn run(&self, request: SpecialistRequest) -> Result<SpecialistReply, ToolExecutionError>;
}

/// The in-run delegation counter. One per manager attempt: fresh tools bind
/// a fresh budget, so a retry starts clean.
#[derive(Debug)]
pub struct RunBudget {
    remaining: AtomicUsize,
    limit: usize,
}

impl RunBudget {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            remaining: AtomicUsize::new(limit),
            limit,
        })
    }

    /// Spends one delegation, or fails with a model-readable error.
    pub fn spend(self: &Arc<Self>) -> Result<(), ToolExecutionError> {
        let mut current = self.remaining.load(Ordering::SeqCst);
        loop {
            if current == 0 {
                return Err(ToolExecutionError::permission_denied(format!(
                    "delegation budget exhausted: at most {} delegations per run",
                    self.limit
                )));
            }
            match self.remaining.compare_exchange(
                current,
                current - 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => current = actual,
            }
        }
    }

    pub fn remaining(&self) -> usize {
        self.remaining.load(Ordering::SeqCst)
    }
}

/// Builds one delegation tool for a specialist: typed args
/// `{ task, context, expected }`, name `delegate_to_<name>`. The callback
/// reads the run scope from the dispatch context, spends the budget, and
/// turns runner failures and denials into model-readable tool errors.
pub fn delegation_tool(
    spec: &SpecialistSpec,
    runner: Arc<dyn SpecialistRunner>,
    budget: Arc<RunBudget>,
) -> anyhow::Result<DynamicTool> {
    let tool_name = rig::message::ToolName::new(format!("delegate_to_{}", spec.name))
        .map_err(|err| anyhow!("invalid delegation tool name: {err}"))?;
    // Cloned up front: the callback must own everything it touches ('static).
    let specialist_name = spec.name.clone();
    Ok(DynamicTool::new_with_context(
        tool_name,
        spec.description.clone(),
        serde_json::json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "One concrete instruction for the specialist."
                },
                "context": {
                    "type": "string",
                    "description": "What the specialist needs to know. May be empty."
                },
                "expected": {
                    "type": "string",
                    "description": "What a useful answer looks like (shape, focus, length)."
                }
            },
            "required": ["task", "expected"],
            "additionalProperties": false
        }),
        move |context: &mut ToolContext, args: serde_json::Value| {
            let runner = Arc::clone(&runner);
            let budget = Arc::clone(&budget);
            let specialist_name = specialist_name.clone();
            Box::pin(async move {
                let scope: RunScope = context.require().map_err(|err| {
                    ToolExecutionError::other(format!(
                        "delegation dispatch is missing its run scope: {err}"
                    ))
                })?;
                // Validate the model's arguments BEFORE spending the budget:
                // a malformed call must not consume one delegation.
                let task = required_text(&args, "task")?;
                let expected = required_text(&args, "expected")?;
                let context_text = args
                    .get("context")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string();
                budget.spend()?;
                match runner
                    .run(SpecialistRequest {
                        name: specialist_name.clone(),
                        task,
                        context: context_text,
                        expected,
                        scope,
                    })
                    .await
                {
                    Ok(reply) => Ok(ToolOutput::text(reply.response)),
                    Err(err) => Err(err),
                }
            })
        },
    ))
}

fn required_text(args: &serde_json::Value, key: &str) -> Result<String, ToolExecutionError> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            ToolExecutionError::invalid_args(format!("{key} must be a non-empty string"))
        })
}

// ---------------------------------------------------------------------------
// The real runner: specialist agents over the shared pool
// ---------------------------------------------------------------------------

/// One specialist's live pieces: its spec and its own registry (only the
/// allowed guarded tools).
pub struct SpecialistHandle {
    pub spec: SpecialistSpec,
    tools: ToolServerHandle,
}

impl SpecialistHandle {
    /// Builds the subset registry from the shared one. A name the shared
    /// registry does not carry is a startup error; the manager's reporting
    /// tool and delegation tools can never get in (structural depth 1,
    /// manager-only `record_fact`).
    pub fn build(spec: SpecialistSpec, shared: &ToolServerHandle) -> Result<Self, SpecialistError> {
        for tool in &spec.tools {
            if is_manager_only(tool) {
                return Err(SpecialistError::Invalid {
                    name: spec.name.clone(),
                    message: format!(
                        "{tool} is the manager's reporting tool; specialists never \
                         record facts"
                    ),
                });
            }
            if tool.starts_with("delegate_to_") {
                return Err(SpecialistError::Invalid {
                    name: spec.name.clone(),
                    message: format!(
                        "{tool} is a delegation tool; a specialist cannot delegate \
                         (delegation depth is 1)"
                    ),
                });
            }
        }
        let shared_toolset = shared.toolset();
        let mut builder = ToolServer::new();
        for tool in &spec.tools {
            match shared_toolset.get(tool) {
                Some(registered) => {
                    builder = builder.registered_tool(registered.clone());
                }
                None => {
                    return Err(SpecialistError::Invalid {
                        name: spec.name.clone(),
                        message: format!(
                            "allowed tool {tool:?} is not registered; specialists can only \
                             call registered guarded tools"
                        ),
                    });
                }
            }
        }
        Ok(Self {
            spec,
            tools: builder.run(),
        })
    }

    /// The names visible in this specialist's own registry.
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.toolset().names().map(str::to_string).collect()
    }

    pub fn tool_server(&self) -> &ToolServerHandle {
        &self.tools
    }
}

/// Everything one specialist run needs, owned by the fleet so a delegation
/// tool can carry an `Arc` of it without lifting the whole runtime.
pub struct SpecialistFleet {
    handles: Vec<SpecialistHandle>,
    pool: Arc<crate::pool::LlmPool>,
    profiles: RoleProfiles,
    sink: Option<Arc<dyn RuntimeSink>>,
    limits: DelegationLimits,
}

impl SpecialistFleet {
    /// Builds the fleet from resolved specs and the shared guarded registry.
    pub fn new(
        specs: Vec<SpecialistSpec>,
        shared: &ToolServerHandle,
        pool: Arc<crate::pool::LlmPool>,
        profiles: RoleProfiles,
        sink: Option<Arc<dyn RuntimeSink>>,
        limits: DelegationLimits,
    ) -> Result<Self, SpecialistError> {
        let mut handles = Vec::with_capacity(specs.len());
        for spec in specs {
            handles.push(SpecialistHandle::build(spec, shared)?);
        }
        Ok(Self {
            handles,
            pool,
            profiles,
            sink,
            limits,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }

    /// The specialist names, for logging and tests.
    pub fn names(&self) -> Vec<String> {
        self.handles
            .iter()
            .map(|handle| handle.spec.name.clone())
            .collect()
    }

    /// The per-run delegation cap this fleet was built with.
    pub fn max_delegations(&self) -> usize {
        self.limits.max_delegations
    }

    /// The manager run's turn budget this fleet was built with.
    pub fn manager_max_turns(&self) -> usize {
        self.limits.manager_max_turns
    }

    /// The delegation tools for one manager attempt, sharing one budget.
    pub fn delegation_tools(
        self: &Arc<Self>,
        budget: Arc<RunBudget>,
    ) -> anyhow::Result<Vec<DynamicTool>> {
        let fleet = Arc::clone(self);
        let runner: Arc<dyn SpecialistRunner> = fleet;
        let mut tools = Vec::with_capacity(self.handles.len());
        for handle in &self.handles {
            tools.push(delegation_tool(
                &handle.spec,
                Arc::clone(&runner),
                Arc::clone(&budget),
            )?);
        }
        Ok(tools)
    }

    /// The tool names the manager sees (the delegation tools only; the
    /// guarded tools live on the shared registry).
    pub fn delegation_tool_names(&self) -> Vec<String> {
        self.handles
            .iter()
            .map(|handle| format!("delegate_to_{}", handle.spec.name))
            .collect()
    }

    /// The resolved handle of one specialist, or `None`.
    fn handle(&self, name: &str) -> Option<&SpecialistHandle> {
        self.handles.iter().find(|handle| handle.spec.name == name)
    }
}

#[async_trait]
impl SpecialistRunner for SpecialistFleet {
    /// Runs one specialist over the role's candidate chain. Usage lands in
    /// the runtime sink as one record tagged `specialist:<name>`; events
    /// bracket the run. Every failure is a model-readable tool error.
    async fn run(&self, request: SpecialistRequest) -> Result<SpecialistReply, ToolExecutionError> {
        let Some(handle) = self.handle(&request.name) else {
            return Err(ToolExecutionError::not_found(format!(
                "no specialist named {:?}",
                request.name
            )));
        };
        let started = Instant::now();
        let run_id = Uuid::new_v4();
        let user = render_specialist_prompt(&request);
        self.emit(
            request.scope.task_id,
            "agent_delegated",
            format!("delegated to specialist {}", request.name),
            serde_json::json!({
                "specialist": request.name,
                "intent_id": request.scope.intent_id,
                "task": request.task,
            }),
        )
        .await;

        let result = self
            .run_specialist(handle, &user, request.scope.clone())
            .await;

        let reply = match result {
            Ok((text, response, latency, candidate)) => {
                let usage = usage_from_rig(&response.usage());
                let thinking = crate::response_thinking(&response);
                self.record(SpecialistRunRecord {
                    run_id,
                    task_id: request.scope.task_id,
                    intent_id: request.scope.intent_id,
                    specialist: request.name.clone(),
                    model: candidate.model.clone(),
                    profile: candidate.tag(),
                    prompt_text: user.clone(),
                    response: Some(text.clone()),
                    thinking,
                    usage,
                    status: "ok".to_string(),
                    error: None,
                    latency_ms: latency,
                })
                .await;
                self.emit(
                    request.scope.task_id,
                    "agent_returned",
                    format!("specialist {} returned", request.name),
                    serde_json::json!({
                        "specialist": request.name,
                        "status": "ok",
                        "response_chars": text.chars().count(),
                    }),
                )
                .await;
                SpecialistReply {
                    response: text,
                    usage: usage_from_rig(&response.usage()),
                }
            }
            Err(err) => {
                let message = err.to_string();
                self.record(SpecialistRunRecord {
                    run_id,
                    task_id: request.scope.task_id,
                    intent_id: request.scope.intent_id,
                    specialist: request.name.clone(),
                    model: self
                        .pool
                        .load()
                        .head_model(Role::Worker)
                        .unwrap_or_else(|| self.profiles.worker.model.clone()),
                    profile: String::new(),
                    prompt_text: user.clone(),
                    response: None,
                    thinking: None,
                    usage: Usage::default(),
                    status: "error".to_string(),
                    error: Some(message.clone()),
                    latency_ms: crate::latency_ms(started),
                })
                .await;
                self.emit(
                    request.scope.task_id,
                    "agent_returned",
                    format!("specialist {} failed", request.name),
                    serde_json::json!({
                        "specialist": request.name,
                        "status": "error",
                        "error": message.clone(),
                    }),
                )
                .await;
                return Err(ToolExecutionError::other(format!(
                    "specialist {} failed: {message}",
                    request.name
                )));
            }
        };
        Ok(reply)
    }
}

impl SpecialistFleet {
    async fn emit(&self, task_id: Uuid, kind: &str, message: String, payload: serde_json::Value) {
        if let Some(sink) = &self.sink {
            sink.record_event(task_id, kind, message, payload).await;
        } else {
            info!(%kind, %task_id, %message, "specialist event (no sink)");
        }
    }

    async fn record(&self, record: SpecialistRunRecord) {
        if let Some(sink) = &self.sink {
            sink.record_specialist_run(record).await;
        }
    }

    /// One specialist completion: the role chain, the specialist's own
    /// registry, its preamble, its budget, and this run's scope.
    async fn run_specialist(
        &self,
        handle: &SpecialistHandle,
        user: &str,
        scope: RunScope,
    ) -> anyhow::Result<(String, rig::agent::PromptResponse, i64, ResolvedProfile)> {
        let mut tool_context = ToolContext::new();
        tool_context
            .insert(scope)
            .map_err(|err| anyhow!("failed to stamp the specialist run scope: {err}"))?;
        tool_context
            .insert(crate::tools::RunRole::specialist(&handle.spec.name))
            .map_err(|err| anyhow!("failed to stamp the specialist run role: {err}"))?;

        let chain = self.pool.chain(Role::Worker, handle.spec.worker_profile_id);
        let mut last_err: Option<anyhow::Error> = None;
        for candidate in &chain {
            match self
                .complete_candidate(candidate, handle, user, tool_context.clone())
                .await
            {
                Ok((text, response, latency)) => {
                    if candidate.id != 0 {
                        self.pool.record_success(candidate.id);
                    }
                    return Ok((text, response, latency, candidate.clone()));
                }
                Err(err) => {
                    warn!(
                        specialist = %handle.spec.name,
                        profile_id = candidate.id,
                        model = %candidate.model,
                        "specialist candidate failed: {err}"
                    );
                    if candidate.id != 0 {
                        self.pool.record_failure(candidate.id);
                    }
                    last_err = Some(err);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| {
            anyhow!(
                "the worker pool is empty: no runnable LLM profile for specialist {}",
                handle.spec.name
            )
        }))
    }

    async fn complete_candidate(
        &self,
        candidate: &ResolvedProfile,
        handle: &SpecialistHandle,
        user: &str,
        tool_context: ToolContext,
    ) -> anyhow::Result<(String, rig::agent::PromptResponse, i64)> {
        let builder = match candidate.format {
            ProfileFormat::Openai => {
                AgentBuilder::new(crate::openai_client(candidate).chat(candidate.model.clone()))
            }
            ProfileFormat::Anthropic => AgentBuilder::new(
                crate::anthropic_client(candidate).completion(candidate.model.clone()),
            ),
        }
        .preamble(handle.spec.preamble.clone())
        // ONLY the specialist's own registry: its allowed guarded tools, no
        // delegation tools, no record_fact.
        .tool_server_handle(handle.tools.clone());
        // The output cap travels with the profile when it set one.
        let builder = if candidate.max_tokens > 0 {
            builder.max_tokens(candidate.max_tokens.max(0) as u64)
        } else {
            builder
        };
        // Thinking travels with the profile, exactly like the manager/planner
        // calls: portable effort/off or the provider passthrough for enabled.
        let mut builder = builder;
        let thinking = crate::thinking_options(candidate);
        if let Some(reasoning) = thinking.reasoning {
            builder =
                builder.options(rig::completion::GenerationOptions::new().reasoning(reasoning));
        }
        if let Some(params) = thinking.additional_params {
            builder = builder.additional_params(params);
        }
        let agent = builder.build();
        let started = Instant::now();
        let response = agent
            .prompt(user.to_string())
            .tool_context(tool_context)
            .max_turns(handle.spec.max_turns)
            .await
            .map_err(|err| {
                warn!(specialist = %handle.spec.name, "specialist call failed: {err}");
                anyhow::Error::new(err)
            })?;
        let latency = crate::latency_ms(started);
        Ok((response.output().to_string(), response, latency))
    }
}

/// The specialist's user turn: the task, the context lines, the expectation.
/// The preamble carries the specialist's own role; the user turn carries the
/// work.
fn render_specialist_prompt(request: &SpecialistRequest) -> String {
    let mut prompt = format!("Task: {}\n\n", request.task);
    if !request.context.is_empty() {
        prompt.push_str("Context:\n");
        for line in request.context.lines() {
            prompt.push_str("- ");
            prompt.push_str(line.trim());
            prompt.push('\n');
        }
        prompt.push('\n');
    }
    prompt.push_str("Expected:\n");
    prompt.push_str(&request.expected);
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::sync::Mutex;

    fn spec(name: &str, tools: &[&str]) -> SpecialistSpec {
        SpecialistSpec {
            name: name.to_string(),
            description: format!("the {name} specialist"),
            preamble: format!("# {name}\n\nDo the {name} work."),
            tools: tools.iter().map(|tool| tool.to_string()).collect(),
            max_turns: 4,
            worker_profile_id: None,
        }
    }

    #[derive(Default)]
    struct RecordingSink {
        records: Mutex<Vec<SpecialistRunRecord>>,
        events: Mutex<Vec<(Uuid, String)>>,
    }

    #[async_trait]
    impl RuntimeSink for RecordingSink {
        async fn record_specialist_run(&self, record: SpecialistRunRecord) {
            self.records.lock().unwrap().push(record);
        }
        async fn record_event(
            &self,
            task_id: Uuid,
            kind: &str,
            _message: String,
            _payload: serde_json::Value,
        ) {
            self.events
                .lock()
                .unwrap()
                .push((task_id, kind.to_string()));
        }
    }

    /// A runner that records what it receives and replays a scripted result.
    struct FakeRunner {
        requests: Mutex<Vec<SpecialistRequest>>,
        outcome: Mutex<Result<SpecialistReply, ToolExecutionError>>,
    }

    impl FakeRunner {
        fn ok(response: &str) -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
                outcome: Mutex::new(Ok(SpecialistReply {
                    response: response.to_string(),
                    usage: Usage::default(),
                })),
            }
        }

        fn fails(message: &str) -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
                outcome: Mutex::new(Err(ToolExecutionError::other(message.to_string()))),
            }
        }
    }

    #[async_trait]
    impl SpecialistRunner for FakeRunner {
        async fn run(
            &self,
            request: SpecialistRequest,
        ) -> Result<SpecialistReply, ToolExecutionError> {
            self.requests.lock().unwrap().push(request);
            self.outcome
                .lock()
                .unwrap()
                .as_ref()
                .map(|reply| reply.clone())
                .map_err(|err| err.clone())
        }
    }

    fn dispatch_scope() -> (Uuid, Uuid) {
        (Uuid::new_v4(), Uuid::new_v4())
    }

    async fn call_delegation_tool_scoped(
        task_id: Uuid,
        intent_id: Uuid,
        tool: &DynamicTool,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolExecutionError> {
        let mut context = ToolContext::new();
        context
            .insert(RunScope { task_id, intent_id })
            .expect("scope serializes");
        tool.run_with(&mut context, args).await
    }

    async fn call_delegation_tool(
        tool: &DynamicTool,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolExecutionError> {
        let (task_id, intent_id) = dispatch_scope();
        call_delegation_tool_scoped(task_id, intent_id, tool, args).await
    }

    #[tokio::test]
    async fn delegation_tool_passes_typed_args_and_the_dispatch_scope() {
        let (task_id, intent_id) = dispatch_scope();
        let runner = Arc::new(FakeRunner::ok("the bishop pin holds"));
        let budget = RunBudget::new(2);
        let tool = delegation_tool(&spec("researcher", &[]), runner.clone(), budget).unwrap();

        let output = call_delegation_tool_scoped(
            task_id,
            intent_id,
            &tool,
            serde_json::json!({
                "task": "check the pin",
                "context": "the bishop is on c1",
                "expected": "one sentence about the pin",
            }),
        )
        .await
        .expect("a successful delegation returns the reply");
        assert_eq!(output.as_text(), Some("the bishop pin holds"));

        {
            // Scoped: the guard must be dropped before the next delegation,
            // or the FakeRunner's mutex deadlocks the second call.
            let requests = runner.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            let request = &requests[0];
            assert_eq!(request.name, "researcher");
            assert_eq!(request.task, "check the pin");
            assert_eq!(request.context, "the bishop is on c1");
            assert_eq!(request.expected, "one sentence about the pin");
            assert_eq!(
                request.scope,
                RunScope { task_id, intent_id },
                "the scope comes from the dispatch context, never the model args"
            );
        }

        // The model cannot smuggle a scope in its arguments.
        let smuggled = call_delegation_tool(
            &tool,
            serde_json::json!({
                "task": "x",
                "expected": "y",
                "scope": { "task_id": Uuid::new_v4(), "intent_id": Uuid::new_v4() },
            }),
        )
        .await;
        assert!(smuggled.is_ok(), "extra args are ignored, not trusted");
    }

    #[tokio::test]
    async fn the_delegation_budget_stops_the_nth_excess_call() {
        let runner = Arc::new(FakeRunner::ok("fine"));
        let budget = RunBudget::new(2);
        let tool = delegation_tool(&spec("researcher", &[]), runner, budget).unwrap();

        let args = serde_json::json!({ "task": "check the page", "expected": "a finding" });
        assert!(call_delegation_tool(&tool, args.clone()).await.is_ok());
        assert!(call_delegation_tool(&tool, args.clone()).await.is_ok());
        let err = call_delegation_tool(&tool, args)
            .await
            .expect_err("the third delegation exceeds the budget");
        assert!(
            err.to_string().contains("budget"),
            "the error names the budget: {err}"
        );
    }

    #[tokio::test]
    async fn a_delegation_failure_is_a_model_readable_tool_error() {
        let runner = Arc::new(FakeRunner::fails("specialist timed out"));
        let tool = delegation_tool(&spec("researcher", &[]), runner, RunBudget::new(8)).unwrap();
        let err = call_delegation_tool(&tool, serde_json::json!({ "task": "t", "expected": "e" }))
            .await
            .expect_err("runner failures surface as tool errors");
        assert!(err.to_string().contains("timed out"), "{err}");
    }

    #[tokio::test]
    async fn missing_required_args_are_invalid_args_errors() {
        let tool = delegation_tool(
            &spec("researcher", &[]),
            Arc::new(FakeRunner::ok("x")),
            RunBudget::new(1),
        )
        .unwrap();
        let err = call_delegation_tool(&tool, serde_json::json!({ "task": "t" }))
            .await
            .expect_err("`expected` is required");
        assert!(err.to_string().contains("expected"), "{err}");
        let err = call_delegation_tool(&tool, serde_json::json!({}))
            .await
            .expect_err("`task` is required");
        assert!(err.to_string().contains("task"), "{err}");
    }

    #[tokio::test]
    async fn a_dispatch_without_a_scope_cannot_delegate() {
        let tool = delegation_tool(
            &spec("researcher", &[]),
            Arc::new(FakeRunner::ok("x")),
            RunBudget::new(1),
        )
        .unwrap();
        let err = tool
            .run(serde_json::json!({ "task": "t", "expected": "e" }))
            .await
            .expect_err("an unstamped dispatch cannot delegate");
        assert!(err.to_string().contains("run scope"), "{err}");
    }

    #[test]
    fn budget_accounting_is_precise() {
        let budget = RunBudget::new(1);
        assert!(budget.spend().is_ok());
        let err = budget.spend().expect_err("budget spent");
        assert!(err.to_string().contains("at most 1"), "{err}");
    }

    #[test]
    fn subset_isolation_builds_the_specialist_registry_from_allowed_tools_only() {
        // The shared registry: three guarded tools a manager grows.
        let shared = ToolServer::new().run();
        for name in ["web_search", "web_read", "other_tool"] {
            register_test_tool(&shared, name);
        }

        let handle =
            SpecialistHandle::build(spec("researcher", &["web_search", "web_read"]), &shared)
                .expect("a valid subset");
        let names = handle.tool_names();
        assert!(names.contains(&"web_search".to_string()), "{names:?}");
        assert!(names.contains(&"web_read".to_string()), "{names:?}");
        assert!(
            !names.contains(&"other_tool".to_string()),
            "the specialist only sees its allowed tools: {names:?}"
        );
        // Depth 1 is structural: no delegation tool can be in the subset.
        assert!(
            !names.iter().any(|name| name.starts_with("delegate_to_")),
            "specialists never carry delegation tools: {names:?}"
        );
    }

    #[test]
    fn an_allowed_tool_the_registry_does_not_have_is_a_config_error() {
        let shared = ToolServer::new().run();
        register_test_tool(&shared, "other_tool");
        let err = match SpecialistHandle::build(spec("researcher", &["web_search"]), &shared) {
            Err(err) => err,
            Ok(_) => panic!("web_search is not registered"),
        };
        let SpecialistError::Invalid { name, message } = err else {
            panic!("expected invalid, got {err:?}");
        };
        assert_eq!(name, "researcher");
        assert!(message.contains("web_search"), "{message}");
    }

    #[test]
    fn the_manager_s_record_fact_never_enters_a_specialist_registry() {
        // Even registered on the shared registry, a code-built (or JSON)
        // spec listing it is refused at build time.
        let shared = ToolServer::new().run();
        register_test_tool(&shared, "other_tool");
        register_test_tool(&shared, "record_fact");
        let err = match SpecialistHandle::build(
            spec("researcher", &["other_tool", "record_fact"]),
            &shared,
        ) {
            Err(err) => err,
            Ok(_) => panic!("record_fact is manager-only"),
        };
        let message = err.to_string();
        assert!(message.contains("record_fact"), "{message}");
        assert!(message.contains("manager"), "{message}");

        // Delegation tools are refused at the same door: depth 1.
        let err =
            match SpecialistHandle::build(spec("researcher", &["delegate_to_researcher"]), &shared)
            {
                Err(err) => err,
                Ok(_) => panic!("a specialist cannot delegate"),
            };
        assert!(err.to_string().contains("depth is 1"), "{err}");
    }

    #[test]
    fn partition_separates_attachable_specs_from_unregistered_tool_sets() {
        let shared = ToolServer::new().run();
        register_test_tool(&shared, "web_search");
        register_test_tool(&shared, "web_read");

        let (attachable, skipped) = partition_by_registered_tools(
            vec![
                spec("researcher", &["web_search", "web_read"]),
                spec("historian", &["archive_search"]),
            ],
            &shared,
        );

        assert_eq!(attachable.len(), 1, "researcher's tools all registered");
        assert_eq!(attachable[0].name, "researcher");
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].0, "historian");
        assert_eq!(
            skipped[0].1,
            vec!["archive_search".to_string()],
            "the missing names travel for the log line"
        );
    }

    #[test]
    #[serial]
    fn discovery_finds_the_registry_and_degrades_to_none_without_one() {
        // The walk honors DHI_PROMPTS_DIR; pin the env for the test's span.
        let saved = std::env::var("DHI_PROMPTS_DIR").ok();
        // Edition 2024: `remove_var` is unsafe because env is global.
        unsafe { std::env::remove_var("DHI_PROMPTS_DIR") };

        let dir = std::env::temp_dir().join(format!("dhi-spec-discover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let prompts = dir.join("ops").join("prompts").join("specialists");
        std::fs::create_dir_all(&prompts).unwrap();
        std::fs::write(
            prompts.join("researcher.md"),
            "# Researcher\n\nYou research.\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("ops").join("specialists.json"),
            serde_json::json!({
                "specialists": [
                    {
                        "name": "researcher",
                        "description": "reads pages",
                        "preamble_file": "researcher.md",
                        "tools": ["web_search", "web_read"]
                    }
                ]
            })
            .to_string(),
        )
        .unwrap();

        // Found from a nested working directory; preambles resolved.
        let deep = dir.join("some").join("nested");
        std::fs::create_dir_all(&deep).unwrap();
        let specs = discover_specialists(&deep)
            .expect("discovery succeeds")
            .expect("the config is found");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "researcher");
        assert_eq!(specs[0].preamble, "# Researcher\n\nYou research.\n");
        assert_eq!(
            specs[0].tools,
            vec!["web_search".to_string(), "web_read".to_string()]
        );

        // An invalid config is an error, not a silent skip.
        std::fs::write(dir.join("ops").join("specialists.json"), "{ nope").unwrap();
        assert!(discover_specialists(&deep).is_err());

        // No config file next to the prompts: Ok(None) — no specialists.
        std::fs::remove_file(dir.join("ops").join("specialists.json")).unwrap();
        assert!(matches!(discover_specialists(&deep), Ok(None)));

        // No ops directory at all: Ok(None) too.
        let empty = std::env::temp_dir().join(format!("dhi-spec-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&empty);
        std::fs::create_dir_all(&empty).unwrap();
        assert!(matches!(discover_specialists(&empty), Ok(None)));

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&empty);
        if let Some(value) = saved {
            unsafe { std::env::set_var("DHI_PROMPTS_DIR", value) };
        }
    }

    #[test]
    fn config_validation_rejects_bad_entries() {
        let dir = std::env::temp_dir().join(format!("dhi-specialists-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let prompts = dir.join("prompts");
        std::fs::create_dir_all(&prompts).unwrap();
        std::fs::write(
            prompts.join("researcher.md"),
            "# Researcher\n\nYou research.\n",
        )
        .unwrap();

        let config = dir.join("specialists.json");
        std::fs::write(
            &config,
            serde_json::json!({
                "specialists": [
                    {
                        "name": "researcher",
                        "description": "reads pages",
                        "preamble_file": "researcher.md",
                        "tools": ["web_search", "web_read"],
                        "max_turns": 8
                    },
                    // An arbitrary extra entry: the mechanism accepts any data.
                    {
                        "name": "historian",
                        "description": "remembers past tasks",
                        "preamble_file": "researcher.md",
                        "tools": [],
                        "worker_profile_id": 12
                    }
                ]
            })
            .to_string(),
        )
        .unwrap();

        let specs = load_specialists(&config, &prompts).expect("a valid file loads");
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, "researcher");
        assert_eq!(specs[0].preamble, "# Researcher\n\nYou research.\n");
        assert_eq!(specs[0].max_turns, 8);
        assert!(specs[1].tools.is_empty(), "empty tool lists are allowed");
        assert_eq!(specs[1].worker_profile_id, Some(12));
        assert_eq!(
            specs[1].max_turns, DEFAULT_SPECIALIST_MAX_TURNS,
            "max_turns defaults per entry"
        );

        // Duplicate names fail.
        std::fs::write(
            &config,
            serde_json::json!({
                "specialists": [
                    { "name": "dup", "description": "a", "preamble_file": "researcher.md", "tools": [] },
                    { "name": "dup", "description": "b", "preamble_file": "researcher.md", "tools": [] }
                ]
            })
            .to_string(),
        )
        .unwrap();
        let err = load_specialists(&config, &prompts).expect_err("duplicates fail");
        assert!(err.to_string().contains("duplicate"), "{err}");

        // A specialist that tries to delegate is refused (depth 1).
        std::fs::write(
            &config,
            serde_json::json!({
                "specialists": [
                    { "name": "bad", "description": "a", "preamble_file": "researcher.md",
                      "tools": ["delegate_to_researcher"] }
                ]
            })
            .to_string(),
        )
        .unwrap();
        let err = load_specialists(&config, &prompts).expect_err("delegation in specialists fails");
        assert!(err.to_string().contains("depth is 1"), "{err}");

        // A missing preamble file is a startup error.
        std::fs::write(
            &config,
            serde_json::json!({
                "specialists": [
                    { "name": "ghost", "description": "a", "preamble_file": "ghost.md", "tools": [] }
                ]
            })
            .to_string(),
        )
        .unwrap();
        let err = load_specialists(&config, &prompts).expect_err("missing preamble fails");
        assert!(err.to_string().contains("ghost.md"), "{err}");

        // Invalid name characters are refused.
        std::fs::write(
            &config,
            serde_json::json!({
                "specialists": [
                    { "name": "Has Space", "description": "a", "preamble_file": "researcher.md", "tools": [] }
                ]
            })
            .to_string(),
        )
        .unwrap();
        let err = load_specialists(&config, &prompts).expect_err("bad names fail");
        assert!(err.to_string().contains("lowercase"), "{err}");

        // Missing file.
        let err =
            load_specialists(&dir.join("nope.json"), &prompts).expect_err("missing config fails");
        assert!(err.to_string().contains("nope.json"), "{err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn register_test_tool(handle: &ToolServerHandle, name: &str) {
        let spec = crate::tools::GuardedToolSpec::new(
            name,
            "a test tool",
            serde_json::json!({ "type": "object", "properties": {} }),
        );
        struct NeverSink;
        #[async_trait]
        impl crate::tools::ToolSink for NeverSink {
            async fn execute(
                &self,
                _task_id: Uuid,
                _intent_id: Uuid,
                _request: dhi_core::ToolCallRequest,
            ) -> anyhow::Result<dhi_core::ToolCallOutcome> {
                anyhow::bail!("test sink")
            }
        }
        crate::tools::register_guarded_tool(handle, spec, Arc::new(NeverSink))
            .expect("test registration");
    }

    #[tokio::test]
    async fn usage_tagging_reaches_the_sink_through_the_fleet_runner() {
        // The fleet path up to the LLM call is covered by the real runner's
        // completion; here the tagging seam is proven with a stubbed chain
        // empty: the failure branch records an error row and returns a
        // model-readable error.
        let shared = ToolServer::new().run();
        register_test_tool(&shared, "other_tool");
        let sink = Arc::new(RecordingSink::default());
        let profiles = RoleProfiles::default();
        let fleet = Arc::new(
            SpecialistFleet::new(
                vec![spec("researcher", &["other_tool"])],
                &shared,
                Arc::new(crate::pool::LlmPool::default()),
                profiles,
                Some(Arc::clone(&sink) as Arc<dyn RuntimeSink>),
                DelegationLimits::default(),
            )
            .expect("the fleet builds"),
        );

        let request = SpecialistRequest {
            name: "researcher".to_string(),
            task: "look".to_string(),
            context: String::new(),
            expected: "text".to_string(),
            scope: RunScope {
                task_id: Uuid::new_v4(),
                intent_id: Uuid::new_v4(),
            },
        };
        let err = fleet
            .run(request)
            .await
            .expect_err("an empty pool cannot run an LLM call");
        assert!(err.to_string().contains("failed"), "{err}");

        let records = sink.records.lock().unwrap();
        assert_eq!(records.len(), 1, "one specialist usage row");
        assert_eq!(records[0].specialist, "researcher");
        assert_eq!(records[0].status, "error");
        assert!(records[0].error.is_some());
        let events = sink.events.lock().unwrap();
        let kinds: Vec<&str> = events.iter().map(|(_, kind)| kind.as_str()).collect();
        assert!(kinds.contains(&"agent_delegated"), "{kinds:?}");
        assert!(kinds.contains(&"agent_returned"), "{kinds:?}");
    }
}
