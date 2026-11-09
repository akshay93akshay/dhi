//! Agent layer.
//!
//! DHI talks to agents through `AgentRuntime`. The `RigAgentRuntime` uses
//! Rig's agent runtime. `MockAgentRuntime` is deterministic and used in tests
//! and local development without an API key.
//!
//! The runtime uses two startup-owned resources: the per-role
//! [`LlmProfile`](profile::LlmProfile)s and the file-backed
//! [`PromptSet`](prompt::PromptSet), plus the per-run usage record
//! ([`AgentCallMeta`]).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rig::completion::{AssistantContent, Effort, GenerationOptions, Reasoning};
use rig::message::ToolName;
use rig::prelude::*;
use rig::tool::server::{ToolServer, ToolServerHandle};
use rig::tool::{DynamicTool, ToolContext, ToolExecutionError, ToolOutput};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tracing::{info, warn};
use uuid::Uuid;

pub mod pool;
pub mod profile;
pub mod prompt;
pub mod specialists;
pub mod tools;

pub use pool::{
    ChainEntry, LlmPool, POOL_TRIP_THRESHOLD, PoolSnapshot, PoolStatus, ProfileFormat, RawProfile,
    ResolvedProfile, RolePoolStatus,
};
pub use profile::{LlmProfile, MaxTokensField, ProfileError, Provider, Role, RoleProfiles};
pub use prompt::{INVARIANT_TAIL, PlannerVars, PromptError, PromptSet, PromptTemplate, WorkerVars};
pub use specialists::{
    DEFAULT_MANAGER_MAX_TURNS, DEFAULT_MAX_DELEGATIONS, DEFAULT_SPECIALIST_MAX_TURNS,
    DelegationLimits, MANAGER_ONLY_TOOLS, RunBudget, RuntimeSink, SpecialistEntry, SpecialistError,
    SpecialistFleet, SpecialistHandle, SpecialistReply, SpecialistRequest, SpecialistRunRecord,
    SpecialistRunner, SpecialistSpec, delegation_tool, discover_specialists, load_specialists,
    partition_by_registered_tools,
};
pub use tools::{
    GuardedToolSpec, RunRole, RunScope, ToolServerView, ToolSink, guarded_dynamic_tool,
    register_guarded_tool, register_native_tool,
};

/// The model-call ceiling for one run.
///
/// rig defaults an unset budget to one model call, and every tool-loop
/// iteration needs another call. DHI+ therefore gives one run a ceiling far
/// above any real derivation: tool use never ends a run. The task ledger
/// (`TaskCapacity::max_model_calls`) is the budget that decides when the task
/// stops.
pub const PER_RUN_MODEL_CALL_CEILING: usize = 1_000_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanInput {
    pub task_id: Uuid,
    pub goal: String,
    /// Open goals, with their full ids, so a proof can name one.
    pub goals: Vec<dhi_core::Goal>,
    /// Facts of the task, with their full ids and states.
    pub facts: Vec<dhi_core::Fact>,
    pub open_intents: usize,
    /// Recent DHI+ team failures, oldest first. The planner uses them to
    /// decide whether to retry with a different plan or a larger worker
    /// budget, or to give up. Empty for a normal first turn.
    #[serde(default)]
    pub failures: Vec<String>,
    /// The task's pinned planner profile, resolved by the engine from the
    /// task row. `None` follows the planner's default chain.
    #[serde(default)]
    pub planner_profile_id: Option<i64>,
    /// The topology the user required at task creation. `None` leaves the
    /// plan optional; a present value is a hard requirement the planner tool
    /// guard enforces before the engine sees the plan.
    #[serde(default)]
    pub required_topology: Option<dhi_core::Topology>,
    /// The exact swarm worker count the user required at task creation.
    /// `None` leaves the count to the planner. The planner tool guard and the
    /// engine both enforce a present value on a swarm plan.
    #[serde(default)]
    pub swarm_workers: Option<usize>,
    /// Pending human instructions, oldest first. They are planner-only: the
    /// engine never injects them into a worker prompt.
    #[serde(default)]
    pub instructions: Vec<String>,
}

/// What one planner call decided.
///
/// A planner either adds one intent, proves one goal with a confirmed fact,
/// or says there is no plan. There is no completion boolean.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanOutcome {
    AddIntent {
        description: String,
        /// Optional DHI+ execution plan. `None` keeps the single-worker
        /// behavior. The engine validates it, persists it, and executes it
        /// through the manager/worker or swarm runner.
        #[serde(default)]
        plan: Option<dhi_core::IntentPlan>,
    },
    ProveGoal {
        goal_id: Uuid,
        fact_id: Uuid,
        reason: String,
    },
    GiveUp {
        goal_id: Uuid,
        reason: String,
    },
    NoPlan {
        reason: String,
    },
}

/// The fixed preamble of an advisory side answer. It names the three hard
/// rules: the snapshot is frozen, the answer is advisory, and there are no
/// tools or graph writes.
pub const SIDE_QUESTION_PREAMBLE: &str = "\
You are a read-only side-question assistant for one DHI+ task. \
The task snapshot in the user message was frozen when this conversation began; \
answer only from that snapshot. You are advisory only: your answer cannot \
change the task, its goals, intents, facts, or its work graph, and it is never \
recorded as a fact. You have no tools and no engine access. If the snapshot \
does not contain the answer, say so plainly instead of guessing.";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteInput {
    pub task_id: Uuid,
    /// The claimed intent this run executes. Stamped into every in-loop tool
    /// dispatch (`RunScope`), so a guarded call can never detach from the
    /// intent that asked for it. The engine always fills it.
    pub intent_id: Uuid,
    pub intent: String,
    pub context: Vec<String>,
    /// The task's pinned worker profile. `None` follows the worker's default
    /// chain.
    #[serde(default)]
    pub worker_profile_id: Option<i64>,
    /// The planner's per-worker value, retained for the plan/audit contract:
    /// DHI+ no longer caps a run by model calls, and the task budget is the
    /// only budget.
    #[serde(default)]
    pub max_turns: Option<usize>,
}

/// One advisory side question about a task.
///
/// `context` is the frozen snapshot the server took at session creation and
/// serialized as text. There is deliberately no task id, store, tool sink,
/// or engine handle here: the answerer can read the snapshot and answer, and
/// nothing else.
#[derive(Debug, Clone)]
pub struct SideQuestionInput {
    pub context: String,
    pub question: String,
    /// The task's pinned worker profile. `None` follows the worker's default
    /// chain.
    pub worker_profile_id: Option<i64>,
}

/// One worker fact draft, recorded through the native `record_fact` tool.
///
/// The draft is the fact. `summary` becomes the claim the engine records;
/// `detail`, `evidence`, and `confidence` travel in the fact provenance. The
/// final text of the model answer is only a user-visible summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactDraft {
    pub summary: String,
    pub detail: String,
    pub evidence: String,
    pub confidence: String, // "observed" | "inferred"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteOutput {
    pub fact: FactDraft,
    /// Tools the worker wants to run. The engine routes these through the
    /// guarded executor; an empty vec means the fact stands alone.
    #[serde(default)]
    pub tool_calls: Vec<dhi_core::ToolCallRequest>,
}

/// One manager selection request: the task intent plus the planner's allowed
/// worker pool, subtasks, and limits.
///
/// The manager may choose a subset of the allowed workers and assign every
/// subtask, but it cannot invent a worker or widen the planner's limits.
#[derive(Debug, Clone)]
pub struct TeamSelectionInput {
    pub task_id: Uuid,
    pub intent_id: Uuid,
    pub intent: String,
    pub plan: dhi_core::IntentPlan,
    /// The task's pinned worker profile. `None` follows the worker's default
    /// chain.
    pub worker_profile_id: Option<i64>,
}

/// The manager's selection answer, as the model returns it.
///
/// `worker_keys` names the selected subset of the planner's allowed workers;
/// `assignments` names the one worker for each planner subtask.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagerChoice {
    pub worker_keys: Vec<String>,
    pub assignments: Vec<ManagerAssignment>,
}

/// The worker the manager chose for one subtask.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagerAssignment {
    pub subtask_id: String,
    pub worker_key: String,
}

#[async_trait]
pub trait AgentRuntime: Send + Sync {
    async fn plan(&self, input: PlanInput) -> anyhow::Result<AgentCall<PlanOutcome>>;
    async fn execute(&self, input: ExecuteInput) -> anyhow::Result<AgentCall<ExecuteOutput>>;

    /// Lets the runtime's manager choose the actual DHI+ workers and subtasks.
    ///
    /// The default keeps the planner's plan unchanged, so a runtime that does
    /// not implement a manager selects exactly what the planner proposed.
    /// This phase runs manager selection as a worker-role call; a dedicated
    /// manager role is a later refinement.
    async fn select_team(
        &self,
        input: TeamSelectionInput,
    ) -> anyhow::Result<AgentCall<dhi_core::IntentPlan>> {
        Ok(AgentCall {
            output: input.plan,
            meta: AgentCallMeta::default(),
            prompt: String::new(),
            response: String::new(),
            thinking: None,
        })
    }

    /// Answers one advisory side question from a frozen snapshot.
    ///
    /// The default refuses: a runtime that has not opted in cannot answer,
    /// and the route turns that into a persisted `error` request plus `502`.
    /// An implementation must not give the answerer tools or graph access.
    async fn side_answer(&self, _input: SideQuestionInput) -> anyhow::Result<AgentCall<String>> {
        anyhow::bail!("side questions are not supported by this agent runtime")
    }

    fn name(&self) -> &str;
}

/// One runtime call: the parsed output plus the usage record it leaves.
///
/// `output` is the unchanged [`PlanOutcome`] or [`ExecuteOutput`]; `meta`
/// carries one [`AgentCallRecord`] per completion call the runtime made, and
/// `prompt` / `response` are the text the engine stores alongside it.
#[derive(Debug, Clone)]
pub struct AgentCall<T> {
    pub output: T,
    pub meta: AgentCallMeta,
    pub prompt: String,
    pub response: String,
    /// The final reasoning text the provider returned, when it returned any.
    /// `None` for a call that exposed no reasoning; an empty string is never
    /// stored.
    pub thinking: Option<String>,
}

/// Token accounting for one completion call.
///
/// Cache tokens are kept separate because a gateway can report them
/// separately: `input_tokens` is what the call billed as input,
/// `cache_read_tokens` the part served from cache, `cache_write_tokens` the
/// part cached by this call, and `reasoning_tokens` the thinking part of the
/// output. `cost_usd` is the provider's own USD figure and nothing else; DHI
/// never derives it from token counts (contract rule 6 / deviation D13).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub reasoning_tokens: i64,
    /// Provider-reported USD only. `None` (stored as 0) when the provider is silent.
    pub cost_usd: Option<f64>,
}

impl Usage {
    /// Sum of two usage records, used to aggregate a run from its call rows.
    /// Cost stays `None` unless at least one side reported it.
    pub fn saturating_add(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            cache_read_tokens: self
                .cache_read_tokens
                .saturating_add(other.cache_read_tokens),
            cache_write_tokens: self
                .cache_write_tokens
                .saturating_add(other.cache_write_tokens),
            reasoning_tokens: self.reasoning_tokens.saturating_add(other.reasoning_tokens),
            cost_usd: match (self.cost_usd, other.cost_usd) {
                (Some(a), Some(b)) => Some(a + b),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            },
        }
    }
}

/// Per agent run: the aggregate of every completion call the run made, in
/// order, successes and failures alike.
///
/// The runtime fills it for every completion call it actually makes; a mock
/// returns no calls, so it records nothing.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AgentCallMeta {
    pub usage: Usage,
    pub calls: Vec<AgentCallRecord>,
}

/// One completion call of a run: exactly one `agent_runs` row.
///
/// Named `AgentCallRecord` so it cannot be confused with the engine-side
/// request value (`AgentCall<'a>`) the runtime receives.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentCallRecord {
    pub run_id: Uuid,
    pub call_index: u32,
    pub role: Role,
    pub model: String,
    pub profile: String,
    pub status: String,
    pub error: Option<String>,
    pub latency_ms: i64,
    pub usage: Usage,
    pub prompt_hash: String,
}

#[derive(Debug, Default, Clone)]
pub struct MockAgentRuntime;

#[async_trait]
impl AgentRuntime for MockAgentRuntime {
    /// The scripted output with no usage record: the mock makes no completion
    /// call, so there is nothing to record.
    async fn plan(&self, input: PlanInput) -> anyhow::Result<AgentCall<PlanOutcome>> {
        let confirmed = input
            .facts
            .iter()
            .find(|fact| fact.state == dhi_core::FactState::Confirmed);
        let output = match (input.goals.first(), confirmed) {
            (Some(goal), Some(fact)) => PlanOutcome::ProveGoal {
                goal_id: goal.id,
                fact_id: fact.id,
                reason: "mock planner proves from the first confirmed fact".to_string(),
            },
            _ => PlanOutcome::AddIntent {
                description: format!("Investigate the next step for: {}", input.goal),
                plan: None,
            },
        };
        Ok(mock_call(output))
    }

    async fn execute(&self, input: ExecuteInput) -> anyhow::Result<AgentCall<ExecuteOutput>> {
        Ok(mock_call(ExecuteOutput {
            fact: FactDraft {
                summary: format!("Executed intent: {}", input.intent),
                detail: String::new(),
                evidence: "mock evidence".to_string(),
                confidence: "observed".to_string(),
            },
            tool_calls: vec![],
        }))
    }

    /// Deterministic advisory answer. It names the question and the
    /// advisory-only rule so API tests can assert both without a model.
    async fn side_answer(&self, input: SideQuestionInput) -> anyhow::Result<AgentCall<String>> {
        Ok(mock_call(format!(
            "advisory: the frozen snapshot does not need a model to answer \"{}\". \
             This answer is advisory only and cannot change the task.",
            input.question
        )))
    }

    fn name(&self) -> &str {
        "mock"
    }
}

/// A mock call: the given output, no calls, no prompt or response text.
fn mock_call<T>(output: T) -> AgentCall<T> {
    AgentCall {
        output,
        meta: AgentCallMeta::default(),
        prompt: String::new(),
        response: String::new(),
        thinking: None,
    }
}

/// Maps Rig's reported usage onto DHI's token counters.
///
/// `input_tokens` and `output_tokens` are Rig's totals; cache reads and
/// writes are the cache parts of the input; reasoning is the thinking part of
/// the output. Cost is Rig's own USD figure and only when Rig reports one
/// (the cost is priced from a model catalog, which DHI does not load, so on
/// River it stays `None` and the row stores `0`). A counter Rig leaves
/// unset is zero.
pub fn usage_from_rig(usage: &rig::completion::Usage) -> Usage {
    Usage {
        input_tokens: tokens(usage.input_tokens),
        output_tokens: tokens(usage.output_tokens),
        cache_read_tokens: tokens(usage.cached_input_tokens),
        cache_write_tokens: tokens(usage.cache_creation_input_tokens),
        reasoning_tokens: tokens(usage.reasoning_tokens),
        cost_usd: usage.cost.map(|cost| cost.total),
    }
}

fn tokens(value: Option<u64>) -> i64 {
    value.map_or(0, |tokens| i64::try_from(tokens).unwrap_or(i64::MAX))
}

/// The wire settings one profile asks for. Rig's portable options cover an
/// explicit effort and `off`; a bare `enabled` has no portable spelling in rig
/// 0.45, so it becomes the provider passthrough `{"thinking":{"type":
/// "enabled"}}`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ThinkingOptions {
    pub(crate) reasoning: Option<Reasoning>,
    pub(crate) additional_params: Option<serde_json::Value>,
}

pub(crate) fn thinking_options(profile: &ResolvedProfile) -> ThinkingOptions {
    let thinking_type = profile.thinking_type.trim().to_ascii_lowercase();
    let effort = profile.reasoning_effort.trim().to_ascii_lowercase();

    // The two fields are independent, but an explicit disable wins: a
    // profile cannot ask for both no thinking and an effort level.
    if thinking_type == "disabled" {
        return ThinkingOptions {
            reasoning: Some(Reasoning::Off),
            additional_params: None,
        };
    }
    if let Some(effort) = parse_effort(&effort) {
        return ThinkingOptions {
            reasoning: Some(Reasoning::Effort(effort)),
            additional_params: None,
        };
    }
    if thinking_type == "enabled" {
        return ThinkingOptions {
            reasoning: None,
            additional_params: Some(serde_json::json!({
                "thinking": { "type": "enabled" }
            })),
        };
    }
    ThinkingOptions {
        reasoning: None,
        additional_params: None,
    }
}

fn parse_effort(raw: &str) -> Option<Effort> {
    match raw {
        "low" => Some(Effort::Low),
        "medium" => Some(Effort::Medium),
        "high" => Some(Effort::High),
        "xhigh" => Some(Effort::XHigh),
        "max" => Some(Effort::Max),
        _ => None,
    }
}

/// The reasoning text of one rig response.
///
/// A multi-turn agent run keeps reasoning from earlier turns in `messages()`;
/// the final `content()` may hold only the answer. Collect both and dedupe
/// exact repeats so provider retries do not duplicate a block.
pub(crate) fn response_thinking(response: &rig::agent::PromptResponse) -> Option<String> {
    let mut parts = Vec::new();
    for message in response.messages() {
        if let rig::message::Message::Assistant(turn) = message {
            collect_reasoning(&turn.content, &mut parts);
        }
    }
    collect_reasoning(response.content(), &mut parts);
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

fn collect_reasoning(content: &[AssistantContent], parts: &mut Vec<String>) {
    for part in content {
        if let AssistantContent::Reasoning(reasoning) = part {
            let text = reasoning.text.trim();
            if !text.is_empty() && !parts.iter().any(|existing| existing == text) {
                parts.push(text.to_string());
            }
        }
    }
}

pub(crate) fn latency_ms(started: Instant) -> i64 {
    i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX)
}

/// The security vocabulary's role for a runtime role. Only the planner and
/// the worker make model calls.
pub fn record_role(role: Role) -> dhi_core::AgentRole {
    match role {
        Role::Planner => dhi_core::AgentRole::Planner,
        Role::Worker => dhi_core::AgentRole::Worker,
    }
}

/// Applies a pure manager choice to the planner's plan.
///
/// This is the only place a model-supplied selection becomes a plan. Every
/// rule is checked before any worker runs: the choice selects a non-empty,
/// duplicate-free subset of the planner's allowed workers within
/// `max_workers`; every planner subtask has exactly one assignment; no
/// assignment names an unknown subtask or an unselected worker. The selected
/// plan keeps the planner's worker order and subtask order, and its limits.
/// The result is validated with [`dhi_core::IntentPlan::validate`].
pub fn apply_manager_choice(
    plan: &dhi_core::IntentPlan,
    choice: &ManagerChoice,
) -> anyhow::Result<dhi_core::IntentPlan> {
    if choice.worker_keys.is_empty() {
        anyhow::bail!("manager choice selected no workers");
    }
    if choice.worker_keys.len() > plan.limits.max_workers {
        anyhow::bail!(
            "manager choice selected {} workers, above max_workers {}",
            choice.worker_keys.len(),
            plan.limits.max_workers
        );
    }

    let allowed: HashSet<&str> = plan
        .workers
        .iter()
        .map(|worker| worker.key.as_str())
        .collect();
    let mut selected: HashSet<&str> = HashSet::with_capacity(choice.worker_keys.len());
    for key in &choice.worker_keys {
        if !allowed.contains(key.as_str()) {
            anyhow::bail!("manager choice names unknown worker {key:?}");
        }
        if !selected.insert(key.as_str()) {
            anyhow::bail!("manager choice repeats worker {key:?}");
        }
    }

    let subtask_ids: HashSet<&str> = plan
        .subtasks
        .iter()
        .map(|subtask| subtask.id.as_str())
        .collect();
    let mut assignments: HashMap<&str, &str> = HashMap::with_capacity(choice.assignments.len());
    for assignment in &choice.assignments {
        if !subtask_ids.contains(assignment.subtask_id.as_str()) {
            anyhow::bail!(
                "manager choice assigns unknown subtask {:?}",
                assignment.subtask_id
            );
        }
        if !selected.contains(assignment.worker_key.as_str()) {
            anyhow::bail!(
                "manager choice assigns subtask {:?} to unselected worker {:?}",
                assignment.subtask_id,
                assignment.worker_key
            );
        }
        if assignments
            .insert(
                assignment.subtask_id.as_str(),
                assignment.worker_key.as_str(),
            )
            .is_some()
        {
            anyhow::bail!(
                "manager choice assigns subtask {:?} more than once",
                assignment.subtask_id
            );
        }
    }
    for subtask in &plan.subtasks {
        if !assignments.contains_key(subtask.id.as_str()) {
            anyhow::bail!(
                "manager choice has no assignment for subtask {:?}",
                subtask.id
            );
        }
    }

    let mut selected_plan = plan.clone();
    // The planner's order is authoritative, not the model's list order.
    selected_plan.workers = plan
        .workers
        .iter()
        .filter(|worker| selected.contains(worker.key.as_str()))
        .cloned()
        .collect();
    for subtask in &mut selected_plan.subtasks {
        subtask.worker_key = assignments
            .get(subtask.id.as_str())
            .map(|key| (*key).to_string());
    }
    selected_plan.validate()?;
    Ok(selected_plan)
}

/// The manager selection prompt: the system contract plus the JSON input.
///
/// Kept free of I/O so a test can assert the exact contract without a model.
/// The input mirrors the planner's plan: `allowed_workers`, every subtask with
/// a null `worker_key` (the manager chooses), and the planner's limits, which
/// the manager may not widen.
pub fn manager_selection_prompt(plan: &dhi_core::IntentPlan) -> (String, String) {
    let system = format!(
        "You are the DHI+ manager. Choose only from the allowed_workers in the input; \
         never invent a worker. You are the selector, not a worker: do not add \
         yourself to the selection. Return JSON only, with this schema: \
         {{\"worker_keys\":[\"...\"],\"assignments\":[{{\"subtask_id\":\"...\",\"worker_key\":\"...\"}}]}}. \
         Select at most {} worker(s) (the plan's max_workers). \
         When every allowed worker has a subtask, select every allowed worker. \
         Assign every subtask exactly once, to a selected worker.",
        plan.limits.max_workers
    );
    let input = serde_json::json!({
        "intent": plan.intent,
        "allowed_workers": plan
            .workers
            .iter()
            .map(|worker| serde_json::json!({
                "key": worker.key,
                "role": worker.role,
                "prompt": worker.prompt,
                "max_turns": worker.max_turns,
            }))
            .collect::<Vec<_>>(),
        "subtasks": plan
            .subtasks
            .iter()
            .map(|subtask| serde_json::json!({
                "id": subtask.id,
                "description": subtask.description,
                "worker_key": serde_json::Value::Null,
            }))
            .collect::<Vec<_>>(),
        "limits": {
            "max_workers": plan.limits.max_workers,
            "max_parallel": plan.limits.max_parallel,
            "max_rounds": plan.limits.max_rounds,
            "max_messages": plan.limits.max_messages,
        },
    });
    (system, input.to_string())
}

/// Parses the manager's JSON choice.
///
/// Code fences are stripped when present and the first `{` through the last
/// `}` is parsed, so surrounding prose is tolerated. The parsed choice is not
/// applied here; [`apply_manager_choice`] owns every validation rule.
pub fn parse_manager_choice(text: &str) -> anyhow::Result<ManagerChoice> {
    let text = strip_code_fences(text);
    let start = text
        .find('{')
        .ok_or_else(|| anyhow::anyhow!("manager response contains no JSON object"))?;
    let end = text
        .rfind('}')
        .ok_or_else(|| anyhow::anyhow!("manager response contains no closing JSON brace"))?;
    if end < start {
        anyhow::bail!("manager response has malformed JSON braces");
    }
    serde_json::from_str(&text[start..=end])
        .map_err(|err| anyhow::anyhow!("manager response is not valid manager JSON: {err}"))
}

/// Strips one Markdown code fence, if the text is wrapped in one.
fn strip_code_fences(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(after_open) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let after_open = after_open.trim_start_matches(|c: char| c.is_ascii_alphabetic());
    let after_open = after_open
        .strip_prefix("\r\n")
        .or_else(|| after_open.strip_prefix('\n'))
        .or_else(|| after_open.strip_prefix('\r'))
        .unwrap_or(after_open);
    after_open
        .strip_suffix("```")
        .map(str::trim)
        .unwrap_or_else(|| after_open.trim())
}

/// One rendered role call: the two prompt turns plus the model and profile
/// that send them. Built without any network access, so a render is testable.
///
/// The Chat Completions route is chosen with `.chat(model)`, so River gets
/// `POST /chat/completions` whatever its model name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedCall {
    pub role: Role,
    pub system: String,
    pub user: String,
    /// The model the role's profile sends to.
    pub model: String,
    /// The role's profile fingerprint; no secret.
    pub profile: String,
    /// `planner@<hash>` or `worker@<hash>`.
    pub prompt_hash: String,
}

#[derive(Debug, Error)]
pub enum RigConfigError {
    #[error("missing environment variable: {0}")]
    Missing(&'static str),
    #[error(transparent)]
    Profile(#[from] ProfileError),
    #[error(transparent)]
    Prompt(#[from] PromptError),
}

#[derive(Debug, Clone)]
pub struct RigConfig {
    pub base_url: String,
    pub api_key: String,
    pub planner_model: String,
    pub worker_model: String,
}

impl RigConfig {
    pub fn from_env() -> Result<Self, RigConfigError> {
        let profiles = LlmProfile::from_env()?;
        Ok(Self::from_profiles(&profiles))
    }

    fn from_profiles(profiles: &RoleProfiles) -> Self {
        Self {
            base_url: profiles.planner.base_url.clone(),
            api_key: profiles.planner.api_key.clone(),
            planner_model: profiles.planner.model.clone(),
            worker_model: profiles.worker.model.clone(),
        }
    }
}

/// The per-attempt tool wiring: the dispatch scope, the shared registry, or
/// neither for a side question (which is the point: no tools at all).
#[derive(Clone, Default)]
struct ToolSetup {
    context: Option<ToolContext>,
    registry: Option<ToolServerHandle>,
}

/// A private copy of one guarded registry for a single completion attempt.
///
/// Rig's builder merges its `dynamic_tool` registrations into the handle it is
/// given. The per-attempt tools (`record_fact`, delegation tools with a fresh
/// budget) must not mutate the shared registry: two concurrent workers would
/// otherwise replace each other's tool of the same name, and a fact could be
/// recorded into the wrong worker's sink. Cloned registrations share the
/// original handler, so guarded tools still dispatch to the host's one sink.
fn private_tool_server(shared: &ToolServerHandle) -> ToolServerHandle {
    let toolset = shared.toolset();
    let mut server = ToolServer::new();
    for (_, tool) in toolset.iter() {
        server = server.registered_tool(tool.clone());
    }
    server.run()
}

pub struct RigAgentRuntime {
    /// Legacy role profiles: env-resolved defaults, the fallback render
    /// models, and the per-role configured `max_turns` values (retained for
    /// config compatibility; they no longer cap a run).
    profiles: RoleProfiles,
    /// The shared failover pool. Calls resolve their candidate chain here.
    pool: Arc<LlmPool>,
    prompts: PromptSet,
    /// The guarded path for in-loop tool calls. `None` keeps the runtime
    /// tool-less (any registered guarded tool then fails its dispatch, which
    /// is why the host always pairs registration with a sink).
    tool_sink: Option<Arc<dyn ToolSink>>,
    /// The worker's tool registry, built once. The host registers guarded
    /// tools onto this handle; worker agents attach it every run.
    tool_server: ToolServerHandle,
    /// An optional role-scoped view of the shared registry. When installed,
    /// a worker run attaches only the view's tools; the shared registry still
    /// owns every registration (and remains the source for specialists).
    worker_tool_view: Option<Arc<dyn crate::tools::ToolServerView>>,
    /// The specialist fleet, when configured. `execute()` builds one
    /// delegation tool per specialist with a fresh per-attempt budget.
    specialists: Option<Arc<SpecialistFleet>>,
    /// Delegation limits (how many delegations one manager run may make).
    limits: DelegationLimits,
}

impl RigAgentRuntime {
    /// Builds the runtime from the environment profiles and the given prompt
    /// set. Missing config and a bad template are startup errors. The pool is
    /// a single-candidate-per-role snapshot: behavior without a store.
    pub fn from_env(prompts: PromptSet) -> Result<Self, RigConfigError> {
        let profiles = LlmProfile::from_env()?;
        profiles.provider_gate()?;
        Ok(Self::with_parts(profiles, prompts))
    }

    /// The startup-owned pieces, with no I/O: a synthetic env pool with one
    /// candidate per role (no ids, no breaker bookkeeping).
    pub fn with_parts(profiles: RoleProfiles, prompts: PromptSet) -> Self {
        let resolved = |base: &LlmProfile| ResolvedProfile {
            id: 0,
            name: String::new(),
            format: ProfileFormat::Openai,
            base_url: base.base_url.clone(),
            model: base.model.clone(),
            api_key: base.api_key.clone(),
            max_tokens: 0,
            streaming: base.stream,
            thinking_type: base.thinking_type.clone(),
            reasoning_effort: base.reasoning_effort.clone(),
        };
        let snapshot =
            PoolSnapshot::single(resolved(&profiles.planner), resolved(&profiles.worker));
        Self {
            profiles,
            pool: LlmPool::new(snapshot),
            prompts,
            tool_sink: None,
            tool_server: ToolServer::new().run(),
            worker_tool_view: None,
            specialists: None,
            limits: DelegationLimits::default(),
        }
    }

    /// The existing constructor, kept for callers that already resolved a
    /// [`RigConfig`] and for tests. The profiles and prompts take the crate
    /// defaults.
    pub fn new(config: RigConfig) -> Self {
        let mut profiles = RoleProfiles::default();
        profiles.planner.base_url = config.base_url.clone();
        profiles.planner.api_key = config.api_key.clone();
        profiles.worker.base_url = config.base_url.clone();
        profiles.worker.api_key = config.api_key.clone();
        profiles.planner.model = config.planner_model.clone();
        profiles.worker.model = config.worker_model.clone();
        Self::with_parts(profiles, fallback_prompts())
    }

    /// The production constructor: the shared pool the server built from the
    /// store (and swaps on mutation) plus the prompt set.
    pub fn with_pool(pool: Arc<LlmPool>, prompts: PromptSet) -> Self {
        // Legacy accessors and per-role `max_turns` values stay env-shaped;
        // `max_turns` is not a database column.
        let snapshot = pool.load();
        let head = |role: Role| -> Option<ResolvedProfile> {
            match role {
                Role::Planner => snapshot.planner.first(),
                Role::Worker => snapshot.worker.first(),
            }
            .cloned()
        };
        let mut profiles = RoleProfiles::default();
        for (role, profile) in [
            (Role::Planner, &mut profiles.planner),
            (Role::Worker, &mut profiles.worker),
        ] {
            if let Some(model) = snapshot.head_model(role) {
                profile.model = model;
            }
            if let Some(head) = head(role) {
                profile.base_url = head.base_url;
                profile.api_key = head.api_key;
                profile.stream = head.streaming;
            }
        }
        Self {
            profiles,
            pool,
            prompts,
            tool_sink: None,
            tool_server: ToolServer::new().run(),
            worker_tool_view: None,
            specialists: None,
            limits: DelegationLimits::default(),
        }
    }

    /// The planner's profile: the model `plan` renders against (chain head).
    pub fn planner_profile(&self) -> &LlmProfile {
        &self.profiles.planner
    }

    /// The worker's profile: the model `execute` renders against (chain head).
    pub fn worker_profile(&self) -> &LlmProfile {
        &self.profiles.worker
    }

    /// The shared pool this runtime resolves candidate chains through.
    pub fn pool(&self) -> &Arc<LlmPool> {
        &self.pool
    }

    pub fn prompts(&self) -> &PromptSet {
        &self.prompts
    }

    /// Attaches the guarded tool sink. The host calls this once at startup,
    /// before any registration, so a registered tool always has a guard.
    pub fn with_tool_sink(mut self, sink: Arc<dyn ToolSink>) -> Self {
        self.tool_sink = Some(sink);
        self
    }

    /// Installs a role-scoped view for worker runs. The host uses it to keep
    /// hidden MCP tools out of the worker's advertised tool list while the
    /// shared registry still owns every registration (specialists build their
    /// own subsets from that shared registry). Without a view the worker
    /// attaches the full shared registry, exactly as before.
    pub fn with_worker_tool_view(mut self, view: Arc<dyn crate::tools::ToolServerView>) -> Self {
        self.worker_tool_view = Some(view);
        self
    }

    /// The worker's shared tool registry. The host registers guarded tools
    /// here (`add_dynamic_tool` with [`guarded_dynamic_tool`]); every worker
    /// agent attaches this handle, so registrations appear on the next run.
    pub fn tool_server(&self) -> &ToolServerHandle {
        &self.tool_server
    }

    /// The guarded sink, when the host provided one.
    pub fn tool_sink(&self) -> Option<&Arc<dyn ToolSink>> {
        self.tool_sink.as_ref()
    }

    /// Attaches the specialist fleet: one delegation tool per specialist on
    /// the worker (manager) registry, each specialist holding only its
    /// allowed guarded tools. Unknown tool names in a spec are startup
    /// errors; an empty spec list attaches nothing.
    pub fn with_specialists(
        &mut self,
        specs: Vec<SpecialistSpec>,
        sink: Option<Arc<dyn RuntimeSink>>,
    ) -> anyhow::Result<()> {
        if specs.is_empty() {
            return Ok(());
        }
        let fleet = SpecialistFleet::new(
            specs,
            &self.tool_server,
            Arc::clone(&self.pool),
            self.profiles.clone(),
            sink,
            self.limits,
        )
        .map_err(|err| anyhow::anyhow!("specialists: {err}"))?;
        info!(
            manager_max_turns = fleet.manager_max_turns(),
            max_delegations = fleet.max_delegations(),
            "specialist delegation limits resolved"
        );
        self.specialists = Some(Arc::new(fleet));
        Ok(())
    }

    /// The specialist fleet, when configured.
    pub fn specialists(&self) -> Option<&Arc<SpecialistFleet>> {
        self.specialists.as_ref()
    }

    /// Renders the planner call from a [`PlanInput`], with no network access.
    pub fn render_plan(&self, input: &PlanInput) -> Result<RenderedCall, PromptError> {
        let vars = PlannerVars {
            goal: input.goal.clone(),
            goals: goal_lines(&input.goals),
            facts: fact_lines(&input.facts),
            open_intents: input.open_intents,
            failures: failure_lines(&input.failures),
            required_topology: input
                .required_topology
                .map_or_else(|| "none".to_string(), |topology| topology.to_string()),
            swarm_workers: input
                .swarm_workers
                .map_or_else(|| "none".to_string(), |count| count.to_string()),
            instructions: instruction_lines(&input.instructions),
        };
        let (system, user) = self.prompts.render_planner(&vars)?;
        Ok(self.rendered(Role::Planner, system, user))
    }

    /// Renders the worker call from an [`ExecuteInput`]. The intent is in the
    /// system turn, the context in the user turn.
    pub fn render_execute(&self, input: &ExecuteInput) -> Result<RenderedCall, PromptError> {
        let vars = WorkerVars {
            intent: input.intent.clone(),
            context: bullet_list(&input.context),
        };
        let (system, user) = self.prompts.render_worker(&vars)?;
        Ok(self.rendered(Role::Worker, system, user))
    }

    /// Renders one manager selection call: the manager contract in the system
    /// turn and the planner's JSON plan in the user turn.
    ///
    /// The call runs on the worker role and worker pool in this phase; a
    /// dedicated manager role is a later refinement.
    pub fn render_manager_selection(&self, plan: &dhi_core::IntentPlan) -> RenderedCall {
        let (system, user) = manager_selection_prompt(plan);
        let mut call = self.rendered(Role::Worker, system, user);
        call.prompt_hash = format!("manager@{}", render_hash(&call));
        call
    }

    /// Renders one side-question call: the advisory preamble in the system
    /// turn, the frozen snapshot plus question in the user turn. No tool
    /// context is built here; [`AgentRuntime::side_answer`] also attaches no
    /// registry.
    pub fn render_side_question(&self, input: &SideQuestionInput) -> RenderedCall {
        let snapshot = self.pool.load();
        let model = snapshot
            .head_model(Role::Worker)
            .unwrap_or_else(|| self.profiles.worker.model.clone());
        let mut call = RenderedCall {
            role: Role::Worker,
            system: SIDE_QUESTION_PREAMBLE.to_string(),
            user: format!(
                "Frozen task snapshot:\n{}\n\nQuestion:\n{}",
                input.context, input.question
            ),
            model,
            profile: self.profiles.worker.fingerprint(),
            prompt_hash: String::new(),
        };
        call.prompt_hash = format!("side@{}", render_hash(&call));
        call
    }

    fn rendered(&self, role: Role, system: String, user: String) -> RenderedCall {
        // The rendered model is the role chain's head at render time; a
        // failover may complete on a later candidate, and the record then
        // names the candidate that actually answered.
        let snapshot = self.pool.load();
        let model = snapshot
            .head_model(role)
            .unwrap_or_else(|| self.profiles.get(role).model.clone());
        RenderedCall {
            role,
            system,
            user,
            model,
            profile: self.profiles.get(role).fingerprint(),
            prompt_hash: self.prompts.get(role).version(role),
        }
    }

    fn openai_client(&self, profile: &ResolvedProfile) -> rig::providers::openai::OpenAI {
        openai_client(profile)
    }

    /// Anthropic-format client. Profiles store an OpenAI-style base URL, so a
    /// trailing `/v1` is stripped: rig posts to `{base}/v1/messages` itself.
    fn anthropic_client(&self, profile: &ResolvedProfile) -> rig::providers::anthropic::Anthropic {
        anthropic_client(profile)
    }

    /// One completion attempt on one pool candidate. Rig runs the model's tool
    /// loop to its own end under [`PER_RUN_MODEL_CALL_CEILING`]; DHI+ budgets
    /// at task level. The system turn is the rendered preamble; the user turn
    /// is the rendered user prompt. `tool_context` is the dispatch context every
    /// in-loop tool call of this attempt sees (the worker's `RunScope`); the
    /// shared `tool_server` handle carries the registered guarded tools.
    /// Returns the final text, the Rig response (for its usage), and how long
    /// the call took, so the caller can record exactly one `AgentCallRecord`
    /// naming THIS candidate.
    async fn complete_candidate(
        &self,
        candidate: &ResolvedProfile,
        call: &RenderedCall,
        tools: Vec<DynamicTool>,
        tool_context: Option<ToolContext>,
        tool_server: Option<ToolServerHandle>,
    ) -> anyhow::Result<(String, rig::agent::PromptResponse, i64)> {
        let mut builder = match candidate.format {
            ProfileFormat::Openai => {
                AgentBuilder::new(self.openai_client(candidate).chat(candidate.model.clone()))
            }
            ProfileFormat::Anthropic => AgentBuilder::new(
                self.anthropic_client(candidate)
                    .completion(candidate.model.clone()),
            ),
        }
        .preamble(call.system.clone());
        // The guarded registry: every host registration is live for this
        // attempt, alongside the per-attempt native tools. A side question
        // passes `None`, so the answerer has NO tools at all.
        //
        // Rig merges the builder's per-attempt tools into the handle passed
        // here. Passing the shared registry directly would therefore install
        // each worker's `record_fact` (and delegation budget) into the same
        // global map, and concurrent workers would overwrite each other. Copy
        // the guarded registrations into a private registry first; cloned
        // registrations share their handler, so the guard/audit sink is still
        // the one host-owned instance.
        if let Some(server) = tool_server {
            builder = builder.tool_server_handle(private_tool_server(&server));
        }
        for tool in tools {
            builder = builder.dynamic_tool(tool);
        }
        // The output cap travels with the profile when it set one (0 = the
        // endpoint default applies; Anthropic defaults its own minimum).
        if candidate.max_tokens > 0 {
            builder = builder.max_tokens(candidate.max_tokens.max(0) as u64);
        }
        // Thinking travels with the profile: either a portable rig reasoning
        // level, or the provider passthrough for a bare `enabled`.
        let thinking = thinking_options(candidate);
        if let Some(reasoning) = thinking.reasoning {
            builder = builder.options(GenerationOptions::new().reasoning(reasoning));
        }
        if let Some(params) = thinking.additional_params {
            builder = builder.additional_params(params);
        }
        // `streaming=false` is accepted and stored; the rig agent prompt path
        // is request/response in this integration, so no wire toggle exists
        // to flip yet (documented limitation).
        let agent = builder.build();
        let started = Instant::now();
        let runner = agent.prompt(call.user.clone());
        let runner = match tool_context {
            Some(context) => runner.tool_context(context),
            None => runner,
        };
        match runner.max_turns(PER_RUN_MODEL_CALL_CEILING).await {
            Ok(response) => {
                let latency = latency_ms(started);
                Ok((response.output().to_string(), response, latency))
            }
            Err(err) => {
                warn!(
                    role = call.role.as_str(),
                    profile_id = candidate.id,
                    model = %candidate.model,
                    prompt_hash = call.prompt_hash,
                    latency_ms = latency_ms(started),
                    "agent call failed: {err}"
                );
                Err(anyhow::Error::new(err))
            }
        }
    }

    /// Walks the role's candidate chain for one logical step.
    ///
    /// The chain resolves from the shared pool (pin first when the task
    /// pinned a profile, then the role default chain, tripped profiles
    /// skipped). The first candidate whose completion call succeeds wins: its
    /// breaker resets and its identity is returned for the usage record. A
    /// failed call (connection, auth, 5xx — any completion error) advances to
    /// the next candidate; each remaining profile gets exactly one retry, so
    /// there are no loops. When every candidate fails, the last error is
    /// returned.
    async fn complete_via_pool<S>(
        &self,
        role: Role,
        pinned: Option<i64>,
        call: &RenderedCall,
        tool_setup: ToolSetup,
        attempt_tools: impl Fn() -> anyhow::Result<(S, Vec<DynamicTool>)>,
    ) -> anyhow::Result<(String, rig::agent::PromptResponse, i64, ResolvedProfile, S)>
    where
        S: Send,
    {
        // A transient provider failure (429/5xx/network) gets bounded,
        // exponential backoff before the whole role chain is retried. The
        // breaker is only spent on permanent failures: rate limits say
        // "come back later", not "this profile is broken".
        const RETRY_PASSES: usize = 3;
        let mut last_err: Option<anyhow::Error> = None;
        for pass in 0..RETRY_PASSES {
            let chain = self.pool.chain(role, pinned);
            if chain.is_empty() {
                break;
            }
            let mut any_retryable = false;
            for candidate in &chain {
                // Fresh tools per attempt: the builder consumes them, and the
                // sink they bind belongs to exactly one attempt.
                let (sink, tools) = attempt_tools()?;
                match self
                    .complete_candidate(
                        candidate,
                        call,
                        tools,
                        tool_setup.context.clone(),
                        tool_setup.registry.clone(),
                    )
                    .await
                {
                    Ok((text, response, latency)) => {
                        if candidate.id != 0 {
                            self.pool.record_success(candidate.id);
                        }
                        return Ok((text, response, latency, candidate.clone(), sink));
                    }
                    Err(err) => {
                        let (retryable, rate_limited) = classify_completion_error(&err);
                        warn!(
                            role = role.as_str(),
                            profile_id = candidate.id,
                            model = %candidate.model,
                            pass = pass + 1,
                            retryable,
                            rate_limited,
                            "pool candidate failed; advancing to the next profile: {err}"
                        );
                        if !retryable && candidate.id != 0 {
                            self.pool.record_failure(candidate.id);
                        }
                        any_retryable |= retryable;
                        last_err = Some(err);
                    }
                }
            }
            if !any_retryable || pass + 1 == RETRY_PASSES {
                break;
            }
            tokio::time::sleep(retry_backoff(pass)).await;
        }
        Err(last_err.unwrap_or_else(|| {
            anyhow::anyhow!("the {role} pool is empty: no runnable LLM profile is configured")
        }))
    }

    /// The one usage record a successful completion call leaves, naming the
    /// candidate that answered.
    fn call_record(
        &self,
        role: Role,
        candidate: &ResolvedProfile,
        prompt_hash: &str,
        response: &rig::agent::PromptResponse,
        latency_ms: i64,
    ) -> AgentCallRecord {
        AgentCallRecord {
            run_id: Uuid::new_v4(),
            call_index: 0,
            role,
            model: candidate.model.clone(),
            profile: candidate.tag(),
            status: "ok".to_string(),
            error: None,
            latency_ms,
            usage: usage_from_rig(&response.usage()),
            prompt_hash: prompt_hash.to_string(),
        }
    }
}

/// Classifies one completion error: `(retryable, rate_limited)`.
///
/// The provider's own verdict wins (`ProviderResponseError::is_retryable`);
/// a 429 is always flagged so the breaker is not spent on an account quota
/// that a later retry can clear. Transport-ish strings are the fallback for
/// errors wrapped beyond the provider type.
fn classify_completion_error(err: &anyhow::Error) -> (bool, bool) {
    if let Some(provider) = err.downcast_ref::<rig::ProviderResponseError>() {
        let rate_limited = provider.status.map(|status| status.as_u16()) == Some(429);
        return (provider.is_retryable(), rate_limited);
    }
    let text = format!("{err:#}").to_ascii_lowercase();
    let rate_limited = text.contains("429") || text.contains("rate limit");
    let retryable = rate_limited
        || text.contains("timed out")
        || text.contains("timeout")
        || text.contains("connection")
        || text.contains("temporarily")
        || text.contains("status 5");
    (retryable, rate_limited)
}

/// 5 s, 15 s, 30 s between whole-chain retry passes: account-level rate
/// limits are usually minute-windowed, so short retries only burn calls.
fn retry_backoff(pass: usize) -> Duration {
    const STEPS_MS: [u64; 3] = [5_000, 15_000, 30_000];
    Duration::from_millis(STEPS_MS[pass.min(STEPS_MS.len() - 1)])
}

/// Strips one trailing `/v1` segment (and trailing slashes) from an API base,
/// for Anthropic-format providers whose base excludes it.
fn strip_v1_suffix(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    trimmed
        .strip_suffix("/v1")
        .map(str::to_string)
        .unwrap_or_else(|| trimmed.to_string())
}

/// OpenAI-format client for one candidate. The fleet reuses these builders,
/// so specialists resolve through the same pool semantics.
pub(crate) fn openai_client(profile: &ResolvedProfile) -> rig::providers::openai::OpenAI {
    rig::providers::openai::OpenAIConfig::new(profile.api_key.clone())
        .with_base_url(profile.base_url.clone())
        .client()
}

/// Anthropic-format client. Profiles store an OpenAI-style base URL, so a
/// trailing `/v1` is stripped: rig posts to `{base}/v1/messages` itself.
pub(crate) fn anthropic_client(profile: &ResolvedProfile) -> rig::providers::anthropic::Anthropic {
    rig::providers::anthropic::AnthropicConfig::new(profile.api_key.clone())
        .with_base_url(strip_v1_suffix(&profile.base_url))
        .client()
}

/// Short hash of the two rendered turns, for logging without the text.
pub fn render_hash(call: &RenderedCall) -> String {
    let mut hasher = Sha256::new();
    hasher.update(call.system.as_bytes());
    hasher.update(b"\n--\n");
    hasher.update(call.user.as_bytes());
    hasher.finalize()[..6]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn bullet_list(items: &[String]) -> String {
    items
        .iter()
        .map(|item| format!("- {item}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One line per open goal, with the full id a proof must name.
fn goal_lines(goals: &[dhi_core::Goal]) -> String {
    goals
        .iter()
        .map(|goal| {
            format!(
                "- [{}] {} {}",
                goal.state.as_str(),
                goal.id,
                goal.description
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One line per fact, with the full id a proof must name and its state.
fn fact_lines(facts: &[dhi_core::Fact]) -> String {
    facts
        .iter()
        .map(|fact| format!("- [{}] {} {}", fact.state.as_str(), fact.id, fact.claim))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One line per recent team failure. `none` keeps the prompt explicit when
/// the planner has not seen a failure yet.
fn failure_lines(failures: &[String]) -> String {
    if failures.is_empty() {
        "none".to_string()
    } else {
        bullet_list(failures)
    }
}

/// One line per pending human instruction. `none` keeps the section explicit
/// when the planner has no instruction to apply.
fn instruction_lines(instructions: &[String]) -> String {
    if instructions.is_empty() {
        "none".to_string()
    } else {
        bullet_list(instructions)
    }
}

/// Minimal templates for [`RigAgentRuntime::new`], which resolves no file. A
/// bad constant is a bug in this crate, not configuration.
fn fallback_prompts() -> PromptSet {
    let planner = PromptTemplate::parse(
        "# Planner prompt\n\nYou are the DHI+ planner.\n\n{{goal}}\n\nFacts:\n\n{{facts}}\n"
            .to_string(),
        Role::Planner,
    );
    let worker = PromptTemplate::parse(
        "# Worker prompt\n\nYou are the DHI+ worker.\n\n{{intent}}\n\nContext:\n\n{{context}}\n"
            .to_string(),
        Role::Worker,
    );
    match (planner, worker) {
        (Ok(planner), Ok(worker)) => PromptSet { planner, worker },
        (Err(err), _) | (_, Err(err)) => {
            warn!("fallback prompt is invalid: {err}");
            PromptSet {
                planner: PromptTemplate {
                    source: String::new(),
                    hash: String::new(),
                },
                worker: PromptTemplate {
                    source: String::new(),
                    hash: String::new(),
                },
            }
        }
    }
}

/// The planner's native tool sink.
///
/// At most one tool call is accepted. The first call wins; any second call is a
/// tool error, whether it repeats the same tool or switches to the other one.
/// The planner toolkit has no graph store and no executor, so a tool call can
/// only propose one typed outcome.
#[derive(Default)]
struct PlannerSink {
    outcome: Arc<Mutex<Option<PlanOutcome>>>,
    /// Enforced inside the `add_intent` tool closure so a plan-less or
    /// mismatched call is a tool error the model can retry in the same run.
    required_topology: Option<dhi_core::Topology>,
    /// The user-required swarm worker count. When present, a swarm plan must
    /// declare exactly this many workers and this many subtasks.
    swarm_workers: Option<usize>,
}

impl PlannerSink {
    fn with_requirements(
        required_topology: Option<dhi_core::Topology>,
        swarm_workers: Option<usize>,
    ) -> Self {
        Self {
            outcome: Arc::default(),
            required_topology,
            swarm_workers,
        }
    }

    fn finish(&self, text: &str) -> PlanOutcome {
        if let Ok(slot) = self.outcome.lock()
            && let Some(outcome) = slot.as_ref()
        {
            return outcome.clone();
        }
        let text = text.trim();
        match text.strip_prefix("NO PLAN:") {
            Some(reason) => PlanOutcome::NoPlan {
                reason: reason.trim().to_string(),
            },
            None => PlanOutcome::NoPlan {
                reason: text.to_string(),
            },
        }
    }

    fn tools(&self) -> anyhow::Result<Vec<DynamicTool>> {
        let required_topology = self.required_topology;
        let swarm_workers = self.swarm_workers;
        let add_name = ToolName::new("add_intent")
            .map_err(|err| anyhow::anyhow!("invalid planner tool name: {err}"))?;
        let prove_name = ToolName::new("prove_goal")
            .map_err(|err| anyhow::anyhow!("invalid planner tool name: {err}"))?;
        let give_name = ToolName::new("give_up")
            .map_err(|err| anyhow::anyhow!("invalid planner tool name: {err}"))?;

        let add_sink = self.outcome.clone();
        let add_intent = DynamicTool::new(
            add_name,
            "Add exactly one intent for this task. The optional plan is data \
             only: it declares topology, workers, subtasks, and budgets, and \
             cannot name tools or bypass engine limits.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "description": {
                        "type": "string",
                        "description": "One sentence describing the work to do."
                    },
                    "plan": {
                        "type": "object",
                        "description": "Optional DHI+ execution plan. Omit it for the current single-worker behavior.",
                        "properties": {
                            "intent": {
                                "type": "string",
                                "description": "Intent text. Defaults to the description when empty."
                            },
                            "topology": {
                                "type": "string",
                                "enum": ["single", "manager_worker", "swarm"]
                            },
                            "workers": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "key": {
                                            "type": "string",
                                            "description": "Unique worker key referenced by subtasks."
                                        },
                                        "role": {
                                            "type": "string",
                                            "description": "The task worker's role. For manager_worker, never use `manager`: the runtime manager is the selector, not a worker."
                                        },
                                        "prompt": {"type": "string"},
                                        "max_turns": {
                                            "type": "integer",
                                            "minimum": 1,
                                            "description": "Retained for the plan/audit contract. DHI+ no longer caps a run by model calls; the task budget is the only budget."
                                        }
                                    },
                                    "required": ["key", "role", "prompt", "max_turns"],
                                    "additionalProperties": false
                                }
                            },
                            "subtasks": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "id": {"type": "string"},
                                        "description": {"type": "string"},
                                        "worker_key": {
                                            "type": ["string", "null"],
                                            "description": "An optional worker key; omitted or null lets the runner assign round-robin."
                                        }
                                    },
                                    "required": ["id", "description"],
                                    "additionalProperties": false
                                }
                            },
                            "limits": {
                                "type": "object",
                                "description": "Omitted fields use the runtime defaults.",
                                "properties": {
                                    "max_workers": {"type": "integer", "minimum": 1},
                                    "max_parallel": {"type": "integer", "minimum": 1},
                                    "max_rounds": {"type": "integer", "minimum": 1},
                                    "max_messages": {"type": "integer", "minimum": 1}
                                },
                                "additionalProperties": false
                            }
                        },
                        "required": ["topology", "workers", "subtasks"],
                        "additionalProperties": false
                    }
                },
                "required": ["description"],
                "additionalProperties": false
            }),
            move |args| {
                let sink = add_sink.clone();
                Box::pin(async move {
                    let description = args
                        .get("description")
                        .and_then(serde_json::Value::as_str)
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                        .ok_or_else(|| {
                            ToolExecutionError::invalid_args(
                                "description must be a non-empty string",
                            )
                        })?;
                    let plan = if required_topology == Some(dhi_core::Topology::Single) {
                        // Single is an instruction to the engine: a supplied
                        // plan is ignored entirely, even if malformed.
                        None
                    } else {
                        let plan = parse_plan_arg(&args, description)?;
                        let plan = enforce_required_topology(required_topology, plan)?;
                        enforce_swarm_worker_count(swarm_workers, plan)?
                    };
                    record_plan(
                        &sink,
                        PlanOutcome::AddIntent {
                            description: description.to_string(),
                            plan,
                        },
                    )
                })
            },
        );

        let prove_sink = self.outcome.clone();
        let prove_goal = DynamicTool::new(
            prove_name,
            "Prove one open goal with a confirmed fact from the same task.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "goal_id": {
                        "type": "string",
                        "description": "Full id of an open goal in the snapshot."
                    },
                    "fact_id": {
                        "type": "string",
                        "description": "Full id of a confirmed fact in the snapshot."
                    },
                    "reason": {
                        "type": "string",
                        "description": "One sentence naming the evidence."
                    }
                },
                "required": ["goal_id", "fact_id", "reason"],
                "additionalProperties": false
            }),
            move |args| {
                let sink = prove_sink.clone();
                Box::pin(async move {
                    let goal_id = parse_uuid_arg(&args, "goal_id")?;
                    let fact_id = parse_uuid_arg(&args, "fact_id")?;
                    let reason = args
                        .get("reason")
                        .and_then(serde_json::Value::as_str)
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                        .ok_or_else(|| {
                            ToolExecutionError::invalid_args("reason must be a non-empty string")
                        })?;
                    record_plan(
                        &sink,
                        PlanOutcome::ProveGoal {
                            goal_id,
                            fact_id,
                            reason: reason.to_string(),
                        },
                    )
                })
            },
        );

        let give_sink = self.outcome.clone();
        let give_up = DynamicTool::new(
            give_name,
            "Abandon one open goal when the evidence proves it cannot be met. \
             This ends the task with this reason; do not keep adding intents.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "goal_id": {
                        "type": "string",
                        "description": "Full id of an open goal in the snapshot."
                    },
                    "reason": {
                        "type": "string",
                        "description": "One sentence explaining why the goal cannot be met."
                    }
                },
                "required": ["goal_id", "reason"],
                "additionalProperties": false
            }),
            move |args| {
                let sink = give_sink.clone();
                Box::pin(async move {
                    let goal_id = parse_uuid_arg(&args, "goal_id")?;
                    let reason = args
                        .get("reason")
                        .and_then(serde_json::Value::as_str)
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                        .ok_or_else(|| {
                            ToolExecutionError::invalid_args("reason must be a non-empty string")
                        })?;
                    record_plan(
                        &sink,
                        PlanOutcome::GiveUp {
                            goal_id,
                            reason: reason.to_string(),
                        },
                    )
                })
            },
        );

        Ok(vec![add_intent, prove_goal, give_up])
    }
}

/// The one write both planner tools share: the first call wins, and every
/// later call is a tool error.
fn record_plan(
    slot: &Mutex<Option<PlanOutcome>>,
    outcome: PlanOutcome,
) -> Result<ToolOutput, ToolExecutionError> {
    let mut slot = slot
        .lock()
        .map_err(|_| ToolExecutionError::other("planner sink lock poisoned"))?;
    if slot.is_some() {
        return Err(ToolExecutionError::invalid_args(
            "only one planner tool call is allowed: add_intent, prove_goal, and give_up are mutually exclusive",
        ));
    }
    *slot = Some(outcome);
    Ok(ToolOutput::text("planner tool call accepted"))
}

fn parse_uuid_arg(args: &serde_json::Value, key: &str) -> Result<Uuid, ToolExecutionError> {
    let raw = args
        .get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| ToolExecutionError::invalid_args(format!("{key} must be a string")))?;
    Uuid::parse_str(raw)
        .map_err(|_| ToolExecutionError::invalid_args(format!("{key} must be a UUID")))
}

/// Enforce the selector's required topology on an `add_intent` plan.
///
/// `single` is an instruction to the engine, not a plan: any supplied plan is
/// ignored. `manager_worker` and `swarm` require a plan whose topology matches
/// exactly; a missing or mismatched plan is an invalid-args tool error so the
/// model retries in the same run.
fn enforce_required_topology(
    required: Option<dhi_core::Topology>,
    plan: Option<dhi_core::IntentPlan>,
) -> Result<Option<dhi_core::IntentPlan>, ToolExecutionError> {
    match required {
        None => Ok(plan),
        Some(dhi_core::Topology::Single) => Ok(None),
        Some(required) => match plan {
            Some(plan) if plan.topology == required => Ok(Some(plan)),
            Some(plan) => Err(ToolExecutionError::invalid_args(format!(
                "required topology {required}, but the plan declares {}; retry with a {required} plan",
                plan.topology
            ))),
            None => Err(ToolExecutionError::invalid_args(format!(
                "required topology {required} needs a plan with topology {required}; retry with a plan"
            ))),
        },
    }
}

/// Enforce the user's required swarm worker count on an `add_intent` plan.
///
/// A swarm seeds exactly one subtask per worker, so both counts must equal the
/// required value. A mismatch is an invalid-args tool error: the model can
/// retry with the right shape in the same run.
fn enforce_swarm_worker_count(
    required: Option<usize>,
    plan: Option<dhi_core::IntentPlan>,
) -> Result<Option<dhi_core::IntentPlan>, ToolExecutionError> {
    let Some(required) = required else {
        return Ok(plan);
    };
    let Some(plan) = plan else {
        return Ok(None);
    };
    if plan.topology != dhi_core::Topology::Swarm {
        return Ok(Some(plan));
    }
    if plan.workers.len() != required || plan.subtasks.len() != required {
        return Err(ToolExecutionError::invalid_args(format!(
            "required swarm worker count {required}, but the plan declares {} workers and {} \
             subtasks; retry with exactly {required} workers and {required} subtasks",
            plan.workers.len(),
            plan.subtasks.len()
        )));
    }
    Ok(Some(plan))
}

/// Parse and validate the optional planner plan.
///
/// The plan is data only: the tool schema exposes no tool names, and
/// [`dhi_core::IntentPlan::validate`] rejects anything the runners cannot
/// execute. An omitted or empty intent defaults to the description, and
/// missing limit fields fall back to [`dhi_core::CapabilityLimits::default`].
fn parse_plan_arg(
    args: &serde_json::Value,
    description: &str,
) -> Result<Option<dhi_core::IntentPlan>, ToolExecutionError> {
    let Some(raw) = args.get("plan") else {
        return Ok(None);
    };
    if raw.is_null() {
        return Ok(None);
    }
    let mut plan_value = raw.clone();
    let object = plan_value
        .as_object_mut()
        .ok_or_else(|| ToolExecutionError::invalid_args("plan must be an object"))?;
    let intent_is_empty = object
        .get("intent")
        .and_then(serde_json::Value::as_str)
        .is_none_or(|text| text.trim().is_empty());
    if intent_is_empty {
        object.insert(
            "intent".to_owned(),
            serde_json::Value::String(description.to_owned()),
        );
    }
    let plan: dhi_core::IntentPlan = serde_json::from_value(plan_value)
        .map_err(|err| ToolExecutionError::invalid_args(format!("invalid plan: {err}")))?;
    plan.validate()
        .map_err(|err| ToolExecutionError::invalid_args(format!("invalid plan: {err}")))?;
    Ok(Some(plan))
}

/// The exact [`WorkerSink::finish`] error that means the model never called
/// `record_fact`. Only this error earns the one retry in
/// [`AgentRuntime::execute`].
const MISSING_RECORD_FACT: &str = "the worker must call record_fact";

/// The system-turn reminder appended for the one retry after a worker
/// answered without calling `record_fact`.
const RECORD_FACT_RETRY_INSTRUCTION: &str = "CRITICAL: Your previous attempt did not call record_fact. You MUST call record_fact now with the complete result. Do not answer with prose.";

/// Builds the one retry call: the original render with the CRITICAL reminder
/// appended to its system turn and a distinct `+retry` prompt hash, so the
/// retry's usage record cannot be mistaken for the first attempt's.
fn retry_with_record_fact(call: &RenderedCall) -> RenderedCall {
    let mut retry = call.clone();
    retry.system = format!("{}\n\n{}", retry.system, RECORD_FACT_RETRY_INSTRUCTION);
    retry.prompt_hash = format!("{}+retry", call.prompt_hash);
    retry
}

/// Whether this is the exact "worker answered with prose" error. Every other
/// failure (pool, provider, tool, lock) is returned without a retry.
fn is_missing_record_fact(err: &anyhow::Error) -> bool {
    err.to_string() == MISSING_RECORD_FACT
}

/// The worker's native tool sink.
///
/// Exactly one `record_fact` call is accepted. The first draft wins; a second
/// call is a tool error and leaves the first draft intact. The worker has no
/// other output mechanism: if the model answers without calling the tool, the
/// run fails instead of treating its prose as a fact.
#[derive(Default)]
struct WorkerSink {
    draft: Arc<Mutex<Option<FactDraft>>>,
}

impl WorkerSink {
    /// The one draft the worker recorded, or an error when it never called
    /// `record_fact`. Prose is never a fallback.
    fn finish(&self) -> anyhow::Result<FactDraft> {
        let draft = self
            .draft
            .lock()
            .map_err(|_| anyhow::anyhow!("worker sink lock poisoned"))?
            .clone();
        draft.ok_or_else(|| anyhow::anyhow!(MISSING_RECORD_FACT))
    }

    fn tools(&self) -> anyhow::Result<Vec<DynamicTool>> {
        let name = ToolName::new("record_fact")
            .map_err(|err| anyhow::anyhow!("invalid worker tool name: {err}"))?;
        let sink = self.draft.clone();
        let record_fact = DynamicTool::new(
            name,
            "Record exactly one fact draft for the intent. This is the only result.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "summary": {
                        "type": "string",
                        "description": "One sentence the user can read; the engine records it as the fact claim."
                    },
                    "detail": {
                        "type": "string",
                        "description": "What you found, in the detail the intent needs. May be empty."
                    },
                    "evidence": {
                        "type": "string",
                        "description": "What you directly observed or used, named concretely."
                    },
                    "confidence": {
                        "type": "string",
                        "enum": ["observed", "inferred"],
                        "description": "observed only for what you directly saw; inferred for a conclusion."
                    }
                },
                "required": ["summary", "evidence", "confidence"],
                "additionalProperties": false
            }),
            move |args| {
                let sink = sink.clone();
                Box::pin(async move {
                    let summary = required_text_arg(&args, "summary")?;
                    let evidence = required_text_arg(&args, "evidence")?;
                    let confidence = required_text_arg(&args, "confidence")?;
                    if confidence != "observed" && confidence != "inferred" {
                        return Err(ToolExecutionError::invalid_args(
                            "confidence must be `observed` or `inferred`",
                        ));
                    }
                    let detail = args
                        .get("detail")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    record_worker_fact(
                        &sink,
                        FactDraft {
                            summary: summary.to_string(),
                            detail,
                            evidence: evidence.to_string(),
                            confidence: confidence.to_string(),
                        },
                    )
                })
            },
        );
        Ok(vec![record_fact])
    }
}

fn required_text_arg<'a>(
    args: &'a serde_json::Value,
    key: &str,
) -> Result<&'a str, ToolExecutionError> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| {
            ToolExecutionError::invalid_args(format!("{key} must be a non-empty string"))
        })
}

/// The worker's one write: the first call wins, and every later call is a
/// tool error that leaves the first draft intact.
fn record_worker_fact(
    slot: &Mutex<Option<FactDraft>>,
    draft: FactDraft,
) -> Result<ToolOutput, ToolExecutionError> {
    let mut slot = slot
        .lock()
        .map_err(|_| ToolExecutionError::other("worker sink lock poisoned"))?;
    if slot.is_some() {
        return Err(ToolExecutionError::invalid_args(
            "only one record_fact call is allowed per intent",
        ));
    }
    *slot = Some(draft);
    Ok(ToolOutput::text("fact draft recorded"))
}

#[async_trait]
impl AgentRuntime for RigAgentRuntime {
    async fn plan(&self, input: PlanInput) -> anyhow::Result<AgentCall<PlanOutcome>> {
        let call = self.render_plan(&input)?;
        let required_topology = input.required_topology;
        let swarm_workers = input.swarm_workers;
        // The planner runs no guarded tools: no scope, no registry churn.
        let (text, response, latency, candidate, sink) = self
            .complete_via_pool(
                Role::Planner,
                input.planner_profile_id,
                &call,
                ToolSetup::default(),
                || {
                    let sink = PlannerSink::with_requirements(required_topology, swarm_workers);
                    let tools = sink.tools()?;
                    Ok((sink, tools))
                },
            )
            .await?;
        let thinking = response_thinking(&response);
        let record = self.call_record(
            Role::Planner,
            &candidate,
            &call.prompt_hash,
            &response,
            latency,
        );
        Ok(AgentCall {
            output: sink.finish(&text),
            meta: AgentCallMeta {
                usage: record.usage,
                calls: vec![record],
            },
            prompt: call.user,
            response: text,
            thinking,
        })
    }

    /// Executes one intent on the worker role.
    ///
    /// A worker that answers without calling `record_fact` gets exactly one
    /// retry with a CRITICAL system reminder. Only the retry's completion is
    /// recorded: the failed first attempt's prose and tokens are discarded.
    async fn execute(&self, input: ExecuteInput) -> anyhow::Result<AgentCall<ExecuteOutput>> {
        let call = self.render_execute(&input)?;
        // The run scope: stamped by the runtime before the model sees any
        // tool, so every in-loop guarded call carries the claimed intent.
        // Domain tools run inside the Rig loop through the shared registry;
        // the post-hoc path stays empty for the Rig runtime.
        let mut tool_context = ToolContext::new();
        tool_context
            .insert(RunScope {
                task_id: input.task_id,
                intent_id: input.intent_id,
            })
            .map_err(|err| anyhow::anyhow!("failed to stamp the run scope: {err}"))?;
        tool_context
            .insert(crate::tools::RunRole::worker())
            .map_err(|err| anyhow::anyhow!("failed to stamp the run role: {err}"))?;
        let worker_registry = match &self.worker_tool_view {
            Some(view) => view.tool_server(),
            None => self.tool_server.clone(),
        };
        let tool_setup = ToolSetup {
            context: Some(tool_context),
            registry: Some(worker_registry),
        };
        // The planner's per-worker `max_turns` is retained in `input` for the
        // plan/audit contract only: DHI+ no longer caps a run by model calls,
        // and the task budget is the only budget.
        let attempt_tools = || {
            let sink = WorkerSink::default();
            let mut tools = sink.tools()?;
            // The manager's delegation tools: one per specialist, sharing a
            // budget that is fresh for this attempt.
            if let Some(fleet) = &self.specialists {
                let budget = RunBudget::new(fleet.max_delegations());
                tools.extend(fleet.delegation_tools(budget)?);
            }
            Ok((sink, tools))
        };
        let (text, response, latency, candidate, sink) = self
            .complete_via_pool(
                Role::Worker,
                input.worker_profile_id,
                &call,
                tool_setup.clone(),
                &attempt_tools,
            )
            .await?;
        // A worker that answered with prose never called `record_fact`, so the
        // attempt produced no fact. Retry exactly once with a CRITICAL reminder
        // in the system turn, a fresh sink, and fresh tools; the same tool
        // context and registry remain attached. Only this exact finish error
        // triggers the retry — pool and provider failures already returned
        // above, and a locked sink is a runtime bug, not model behavior.
        let (text, response, latency, candidate, fact, recorded_call) = match sink.finish() {
            Ok(fact) => (text, response, latency, candidate, fact, call),
            Err(err) if is_missing_record_fact(&err) => {
                let retry_call = retry_with_record_fact(&call);
                let (retry_text, retry_response, retry_latency, retry_candidate, retry_sink) = self
                    .complete_via_pool(
                        Role::Worker,
                        input.worker_profile_id,
                        &retry_call,
                        tool_setup,
                        &attempt_tools,
                    )
                    .await?;
                // If the retry also fails, its error is returned.
                let fact = retry_sink.finish()?;
                // The retry is a second completion call, and its usage is the
                // one recorded. The failed first attempt's tokens are not
                // counted (nor is its prose used): only a fact-bearing call
                // is billed to the run.
                (
                    retry_text,
                    retry_response,
                    retry_latency,
                    retry_candidate,
                    fact,
                    retry_call,
                )
            }
            Err(err) => return Err(err),
        };
        let thinking = response_thinking(&response);
        let record = self.call_record(
            Role::Worker,
            &candidate,
            &recorded_call.prompt_hash,
            &response,
            latency,
        );
        Ok(AgentCall {
            output: ExecuteOutput {
                fact,
                tool_calls: vec![],
            },
            meta: AgentCallMeta {
                usage: record.usage,
                calls: vec![record],
            },
            prompt: recorded_call.user,
            response: text,
            thinking,
        })
    }

    /// Chooses the actual DHI+ workers and subtasks through one worker-role
    /// completion call.
    ///
    /// The manager gets no tools and the task's pinned worker
    /// profile. Its answer is parsed and applied by
    /// [`apply_manager_choice`]; a parse or validation failure is returned to
    /// the engine, never silently replaced by the planner's plan. A dedicated
    /// manager role is a later refinement.
    async fn select_team(
        &self,
        input: TeamSelectionInput,
    ) -> anyhow::Result<AgentCall<dhi_core::IntentPlan>> {
        let call = self.render_manager_selection(&input.plan);
        let (text, response, latency, candidate, _sink) = self
            .complete_via_pool(
                Role::Worker,
                input.worker_profile_id,
                &call,
                ToolSetup::default(),
                || Ok::<_, anyhow::Error>(((), Vec::new())),
            )
            .await?;
        let thinking = response_thinking(&response);
        let choice = parse_manager_choice(&text)
            .and_then(|choice| apply_manager_choice(&input.plan, &choice))
            .map_err(|err| {
                anyhow::anyhow!(
                    "manager selection failed for intent {}: {err}",
                    input.intent_id
                )
            })?;
        let record = self.call_record(
            Role::Worker,
            &candidate,
            &call.prompt_hash,
            &response,
            latency,
        );
        Ok(AgentCall {
            output: choice,
            meta: AgentCallMeta {
                usage: record.usage,
                calls: vec![record],
            },
            prompt: call.user,
            response: text,
            thinking,
        })
    }

    /// Answers one advisory side question.
    ///
    /// It uses the worker-role failover chain (the task's worker pin first)
    /// but deliberately differs from `execute` in every capability boundary:
    /// no shared tool registry is attached, no native tools are built, and no
    /// `RunScope`/`ToolContext` is stamped. The only input is the frozen
    /// snapshot text and the question.
    async fn side_answer(&self, input: SideQuestionInput) -> anyhow::Result<AgentCall<String>> {
        let call = self.render_side_question(&input);
        let (text, response, latency, candidate, _sink) = self
            .complete_via_pool(
                Role::Worker,
                input.worker_profile_id,
                &call,
                ToolSetup::default(),
                || Ok::<_, anyhow::Error>(((), Vec::new())),
            )
            .await?;
        let thinking = response_thinking(&response);
        let record = self.call_record(
            Role::Worker,
            &candidate,
            &call.prompt_hash,
            &response,
            latency,
        );
        Ok(AgentCall {
            output: text.clone(),
            meta: AgentCallMeta {
                usage: record.usage,
                calls: vec![record],
            },
            prompt: call.user,
            response: text,
            thinking,
        })
    }

    fn name(&self) -> &str {
        "rig"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompts() -> PromptSet {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ops/prompts");
        PromptSet::load(&dir).expect("ops/prompts loads")
    }

    /// A runtime built from explicit profiles: no environment, no network.
    fn runtime_with(planner_model: &str, worker_model: &str) -> RigAgentRuntime {
        let mut profiles = RoleProfiles::default();
        profiles.planner.base_url = "https://river.example/v1".to_string();
        profiles.planner.api_key = "secret-key".to_string();
        profiles.planner.model = planner_model.to_string();
        profiles.worker.base_url = "https://river.example/v1".to_string();
        profiles.worker.api_key = "secret-key".to_string();
        profiles.worker.model = worker_model.to_string();
        RigAgentRuntime::with_parts(profiles, prompts())
    }

    fn test_specialist(name: &str) -> SpecialistSpec {
        SpecialistSpec {
            name: name.to_string(),
            description: format!("the {name} specialist"),
            preamble: format!("# {name}\n\nDo the {name} work."),
            tools: Vec::new(),
            max_turns: DEFAULT_SPECIALIST_MAX_TURNS,
            worker_profile_id: None,
        }
    }

    fn plan_input() -> PlanInput {
        let task = dhi_core::Task::new("forced mate in 4");
        let origin = task.origin_fact();
        let goal = dhi_core::Goal {
            id: Uuid::new_v4(),
            task_id: task.id,
            description: "prove a forced mate".to_string(),
            state: dhi_core::GoalState::Open,
            proof_fact_id: None,
            created_at: task.created_at,
        };
        PlanInput {
            task_id: task.id,
            goal: task.goal,
            goals: vec![goal],
            facts: vec![origin],
            open_intents: 1,
            failures: vec![],
            planner_profile_id: None,
            required_topology: None,
            swarm_workers: None,
            instructions: vec![],
        }
    }

    fn execute_input() -> ExecuteInput {
        ExecuteInput {
            task_id: Uuid::new_v4(),
            intent_id: Uuid::new_v4(),
            intent: "Check whether the bishop pin holds".to_string(),
            context: vec!["fact 89abcdef: bishop on c1".to_string()],
            worker_profile_id: None,
            max_turns: None,
        }
    }

    #[test]
    fn planner_render_uses_the_existing_variables() {
        let runtime = runtime_with("river-planner", "river-worker");
        let input = plan_input();
        let goal_id = input.goals[0].id.to_string();
        let fact_id = input.facts[0].id.to_string();
        let call = runtime.render_plan(&input).expect("renders");
        assert_eq!(call.role, Role::Planner);
        assert!(call.user.contains("forced mate in 4"), "{}", call.user);
        assert!(call.user.contains(&goal_id), "{}", call.user);
        assert!(call.user.contains(&fact_id), "{}", call.user);
        assert!(call.user.contains("Open intents: 1"), "{}", call.user);
        assert!(
            call.user.contains("Recent failed intents"),
            "the planner prompt shows the failure section: {}",
            call.user
        );
        assert!(
            call.user.lines().any(|line| line.trim() == "none"),
            "an empty failure list renders as none: {}",
            call.user
        );
        assert!(call.system.contains("add_intent"), "{}", call.system);
        assert!(call.system.contains("prove_goal"), "{}", call.system);
        assert!(call.system.contains("NO PLAN:"), "{}", call.system);
        assert!(call.system.ends_with(INVARIANT_TAIL));
        assert!(!call.system.contains(&fact_id));
        assert!(!call.system.contains("{{"));
        assert!(!call.user.contains("{{"));
        assert_eq!(
            call.prompt_hash,
            runtime.prompts().planner.version(Role::Planner)
        );
    }

    #[test]
    fn planner_render_bullets_recent_team_failures() {
        let runtime = runtime_with("river-planner", "river-worker");
        let mut input = plan_input();
        input.failures =
            vec!["DHI+ manager_worker team produced no successful worker result".to_string()];
        let call = runtime.render_plan(&input).expect("renders");
        assert!(
            call.user
                .contains("- DHI+ manager_worker team produced no successful worker result"),
            "{}",
            call.user
        );
        assert!(
            !call
                .user
                .contains("Recent failed intents, oldest first:\n\nnone"),
            "a non-empty failure list must not render as none: {}",
            call.user
        );
    }

    #[test]
    fn worker_render_puts_intent_in_system_and_context_in_user() {
        let runtime = runtime_with("river-planner", "river-worker");
        let call = runtime.render_execute(&execute_input()).expect("renders");
        assert_eq!(call.role, Role::Worker);
        assert!(
            call.system.contains("Check whether the bishop pin holds"),
            "{}",
            call.system
        );
        assert!(!call.user.contains("Check whether the bishop pin holds"));
        assert!(call.user.contains("bishop on c1"), "{}", call.user);
        assert!(call.system.ends_with(INVARIANT_TAIL));
        assert_eq!(render_hash(&call).len(), 12);
    }

    #[test]
    fn side_question_render_is_advisory_and_carries_the_frozen_snapshot() {
        let runtime = runtime_with("planner-x", "worker-y");
        let call = runtime.render_side_question(&SideQuestionInput {
            context: "{\"task\":{\"goal\":\"frozen goal\"}}".to_string(),
            question: "Why did this finish?".to_string(),
            worker_profile_id: None,
        });

        assert_eq!(call.role, Role::Worker);
        assert_eq!(call.model, "worker-y");
        assert!(call.system.contains("advisory only"), "{}", call.system);
        assert!(call.system.contains("no tools"), "{}", call.system);
        assert!(call.system.contains("frozen"), "{}", call.system);
        assert!(call.user.contains("frozen goal"), "{}", call.user);
        assert!(call.user.contains("Why did this finish?"), "{}", call.user);
        assert!(
            call.prompt_hash.starts_with("side@"),
            "{}",
            call.prompt_hash
        );
        assert_eq!(render_hash(&call).len(), 12);
    }

    #[tokio::test]
    async fn mock_side_answer_is_deterministic_and_advisory() {
        let answered = MockAgentRuntime
            .side_answer(SideQuestionInput {
                context: "{\"task\":{\"goal\":\"frozen goal\"}}".to_string(),
                question: "What happened?".to_string(),
                worker_profile_id: None,
            })
            .await
            .expect("the mock answers");
        assert!(answered.output.contains("What happened?"));
        assert!(answered.output.contains("advisory"));
        assert!(answered.meta.calls.is_empty(), "the mock makes no call");
    }

    #[test]
    fn runtime_uses_the_role_profiles_models() {
        let runtime = runtime_with("planner-x", "worker-y");

        let planned = runtime.render_plan(&plan_input()).expect("renders");
        assert_eq!(planned.model, "planner-x");
        assert_eq!(planned.model, runtime.planner_profile().model);
        assert_eq!(
            planned.profile,
            "open_ai/planner-x/max_tokens=4096/stream=true"
        );

        let executed = runtime.render_execute(&execute_input()).expect("renders");
        assert_eq!(executed.model, "worker-y");
        assert_eq!(executed.model, runtime.worker_profile().model);
        assert_eq!(
            executed.profile,
            "open_ai/worker-y/max_tokens=8192/stream=true"
        );

        // The fingerprint carries no secret.
        assert!(!planned.profile.contains("secret-key"));
        assert!(!executed.profile.contains("secret-key"));
    }

    #[test]
    fn fallback_runtime_keeps_both_role_models() {
        let runtime = RigAgentRuntime::new(RigConfig {
            base_url: "https://river.example/v1".to_string(),
            api_key: "secret-key".to_string(),
            planner_model: "p".to_string(),
            worker_model: "w".to_string(),
        });
        assert_eq!(runtime.planner_profile().model, "p");
        assert_eq!(runtime.worker_profile().model, "w");
        let call = runtime.render_plan(&plan_input()).expect("renders");
        assert_eq!(call.model, "p");
    }

    #[test]
    fn delegation_limits_use_the_compiled_defaults() {
        let mut runtime = runtime_with("planner-x", "worker-y");
        runtime
            .with_specialists(vec![test_specialist("researcher")], None)
            .expect("specialists attach");

        let fleet = runtime.specialists().expect("a fleet is attached");
        assert_eq!(fleet.manager_max_turns(), DEFAULT_MANAGER_MAX_TURNS);
        assert_eq!(fleet.max_delegations(), DEFAULT_MAX_DELEGATIONS);
    }

    #[test]
    fn worker_profile_budget_is_unchanged_without_specialists() {
        let runtime = runtime_with("planner-x", "worker-y");

        assert!(runtime.specialists().is_none());
        assert_eq!(runtime.worker_profile().max_turns, 24);
    }

    #[test]
    fn with_pool_resolves_the_same_delegation_limits() {
        let mut runtime = RigAgentRuntime::with_pool(Arc::new(LlmPool::default()), prompts());
        runtime
            .with_specialists(vec![test_specialist("researcher")], None)
            .expect("specialists attach");

        let fleet = runtime.specialists().expect("a fleet is attached");
        assert_eq!(fleet.manager_max_turns(), DEFAULT_MANAGER_MAX_TURNS);
        assert_eq!(fleet.max_delegations(), DEFAULT_MAX_DELEGATIONS);
    }

    #[test]
    fn no_plan_text_becomes_no_plan() {
        let sink = PlannerSink::default();
        assert_eq!(
            sink.finish("NO PLAN: every open goal already has an intent"),
            PlanOutcome::NoPlan {
                reason: "every open goal already has an intent".to_string()
            }
        );
        assert_eq!(
            sink.finish("there is nothing useful to add"),
            PlanOutcome::NoPlan {
                reason: "there is nothing useful to add".to_string()
            }
        );
    }

    #[test]
    fn transient_provider_errors_are_retried_with_backoff() {
        let rate = anyhow::anyhow!(
            "ProviderResponseError: status 429 Too Many Requests: account rate limit"
        );
        assert_eq!(classify_completion_error(&rate), (true, true));
        let down = anyhow::anyhow!("error sending request: connection reset");
        assert_eq!(classify_completion_error(&down), (true, false));
        let auth = anyhow::anyhow!("status 401 Unauthorized");
        assert_eq!(classify_completion_error(&auth), (false, false));
        assert_eq!(retry_backoff(0), Duration::from_millis(5_000));
        assert_eq!(retry_backoff(1), Duration::from_millis(15_000));
        assert_eq!(retry_backoff(2), Duration::from_millis(30_000));
        assert_eq!(retry_backoff(9), Duration::from_millis(30_000));
    }

    #[tokio::test]
    async fn planner_tools_accept_one_call_and_reject_the_second() {
        let sink = PlannerSink::default();
        let tools = sink.tools().expect("planner tools build");
        assert_eq!(tools.len(), 3);
        assert_eq!(tools[0].name().as_str(), "add_intent");
        assert_eq!(tools[1].name().as_str(), "prove_goal");
        assert_eq!(tools[2].name().as_str(), "give_up");

        tools[0]
            .run(serde_json::json!({"description": "check the pin"}))
            .await
            .expect("the first tool call is accepted");
        assert!(
            tools[0]
                .run(serde_json::json!({"description": "check it again"}))
                .await
                .is_err(),
            "a second add_intent is a tool error"
        );
        assert!(
            tools[1]
                .run(serde_json::json!({
                    "goal_id": Uuid::new_v4(),
                    "fact_id": Uuid::new_v4(),
                    "reason": "the fact proves it",
                }))
                .await
                .is_err(),
            "prove_goal after add_intent is a tool error"
        );
        assert!(
            tools[2]
                .run(serde_json::json!({
                    "goal_id": Uuid::new_v4(),
                    "reason": "the evidence rules it out",
                }))
                .await
                .is_err(),
            "give_up after add_intent is a tool error"
        );
        assert_eq!(
            sink.finish("ignored after a tool call"),
            PlanOutcome::AddIntent {
                description: "check the pin".to_string(),
                plan: None,
            }
        );
    }

    #[tokio::test]
    async fn add_intent_without_plan_still_works() {
        let sink = PlannerSink::default();
        let tools = sink.tools().expect("planner tools build");
        tools[0]
            .run(serde_json::json!({"description": "check the pin"}))
            .await
            .expect("add_intent without a plan is accepted");
        assert_eq!(
            sink.finish("ignored after a tool call"),
            PlanOutcome::AddIntent {
                description: "check the pin".to_string(),
                plan: None,
            }
        );
    }

    #[tokio::test]
    async fn add_intent_with_manager_worker_plan_parses_and_validates() {
        let sink = PlannerSink::default();
        let tools = sink.tools().expect("planner tools build");
        tools[0]
            .run(serde_json::json!({
                "description": "review the patch",
                "plan": {
                    "topology": "manager_worker",
                    "workers": [
                        {
                            "key": "worker-a",
                            "role": "reviewer",
                            "prompt": "review the patch",
                            "max_turns": 8
                        },
                        {
                            "key": "worker-b",
                            "role": "tester",
                            "prompt": "test the patch",
                            "max_turns": 4
                        }
                    ],
                    "subtasks": [
                        {
                            "id": "sub-1",
                            "description": "review the change",
                            "worker_key": "worker-a"
                        },
                        {
                            "id": "sub-2",
                            "description": "run the tests",
                            "worker_key": "worker-b"
                        }
                    ],
                    "limits": {"max_workers": 3}
                }
            }))
            .await
            .expect("a valid manager-worker plan is accepted");

        let PlanOutcome::AddIntent {
            description,
            plan: Some(plan),
        } = sink.finish("ignored after a tool call")
        else {
            panic!("expected an add_intent outcome with a plan");
        };
        assert_eq!(description, "review the patch");
        assert_eq!(plan.intent, "review the patch");
        assert_eq!(plan.topology, dhi_core::Topology::ManagerWorker);
        assert_eq!(plan.workers.len(), 2);
        assert_eq!(plan.workers[0].key, "worker-a");
        assert_eq!(plan.workers[1].max_turns, 4);
        assert_eq!(plan.subtasks.len(), 2);
        assert_eq!(plan.subtasks[1].worker_key.as_deref(), Some("worker-b"));
        assert_eq!(
            plan.limits,
            dhi_core::CapabilityLimits {
                max_workers: 3,
                ..dhi_core::CapabilityLimits::default()
            }
        );
        plan.validate().expect("the accepted plan validates");
    }

    #[tokio::test]
    async fn add_intent_requires_a_matching_plan_when_topology_is_required() {
        for required in [dhi_core::Topology::ManagerWorker, dhi_core::Topology::Swarm] {
            let sink = PlannerSink::with_requirements(Some(required), None);
            let tools = sink.tools().expect("planner tools build");
            let err = tools[0]
                .run(serde_json::json!({"description": "do it"}))
                .await
                .expect_err("a missing plan must be a tool error");
            assert!(
                format!("{err}").contains("required topology"),
                "{required}: {err}"
            );
        }

        let sink = PlannerSink::with_requirements(Some(dhi_core::Topology::Swarm), None);
        let tools = sink.tools().expect("planner tools build");
        let err = tools[0]
            .run(serde_json::json!({
                "description": "review the patch",
                "plan": {
                    "topology": "manager_worker",
                    "workers": [{
                        "key": "worker-a",
                        "role": "reviewer",
                        "prompt": "review the patch",
                        "max_turns": 4
                    }],
                    "subtasks": [{
                        "id": "sub-1",
                        "description": "review the change",
                        "worker_key": "worker-a"
                    }]
                }
            }))
            .await
            .expect_err("a mismatched plan must be a tool error");
        assert!(
            format!("{err}").contains("required topology swarm"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn add_intent_accepts_a_matching_plan_when_topology_is_required() {
        let sink = PlannerSink::with_requirements(Some(dhi_core::Topology::ManagerWorker), None);
        let tools = sink.tools().expect("planner tools build");
        tools[0]
            .run(serde_json::json!({
                "description": "review the patch",
                "plan": {
                    "topology": "manager_worker",
                    "workers": [{
                        "key": "worker-a",
                        "role": "reviewer",
                        "prompt": "review the patch",
                        "max_turns": 4
                    }],
                    "subtasks": [{
                        "id": "sub-1",
                        "description": "review the change",
                        "worker_key": "worker-a"
                    }]
                }
            }))
            .await
            .expect("a matching plan is accepted");

        let PlanOutcome::AddIntent {
            plan: Some(plan), ..
        } = sink.finish("ignored after a tool call")
        else {
            panic!("expected a plan");
        };
        assert_eq!(plan.topology, dhi_core::Topology::ManagerWorker);
    }

    #[tokio::test]
    async fn add_intent_ignores_a_plan_when_single_is_required() {
        let sink = PlannerSink::with_requirements(Some(dhi_core::Topology::Single), None);
        let tools = sink.tools().expect("planner tools build");
        tools[0]
            .run(serde_json::json!({
                "description": "just do it",
                "plan": {
                    "topology": "swarm",
                    "workers": [{
                        "key": "worker-a",
                        "role": "reviewer",
                        "prompt": "review the patch",
                        "max_turns": 4
                    }],
                    "subtasks": [{
                        "id": "sub-1",
                        "description": "review the change",
                        "worker_key": "worker-a"
                    }]
                }
            }))
            .await
            .expect("single accepts the call");

        assert_eq!(
            sink.finish("ignored after a tool call"),
            PlanOutcome::AddIntent {
                description: "just do it".to_string(),
                plan: None,
            },
            "single must ignore the supplied plan"
        );

        let sink = PlannerSink::with_requirements(Some(dhi_core::Topology::Single), None);
        let tools = sink.tools().expect("planner tools build");
        tools[0]
            .run(serde_json::json!({
                "description": "just do it",
                "plan": {"topology": "mesh"}
            }))
            .await
            .expect("single ignores even a malformed plan");
        assert_eq!(
            sink.finish("ignored after a tool call"),
            PlanOutcome::AddIntent {
                description: "just do it".to_string(),
                plan: None,
            }
        );
    }

    #[test]
    fn render_plan_states_the_required_topology() {
        let runtime = runtime_with("planner-x", "worker-y");
        let mut input = plan_input();
        input.required_topology = Some(dhi_core::Topology::Swarm);

        let call = runtime.render_plan(&input).expect("renders");

        assert!(
            call.user.contains("Required topology: swarm"),
            "the user turn names the selector's topology: {}",
            call.user
        );
        assert!(
            call.system.contains("Required topology"),
            "the protocol explains the requirement: {}",
            call.system
        );
    }

    #[test]
    fn render_plan_states_the_swarm_worker_count_and_user_instructions() {
        let runtime = runtime_with("planner-x", "worker-y");
        let mut input = plan_input();
        input.required_topology = Some(dhi_core::Topology::Swarm);
        input.swarm_workers = Some(4);
        input.instructions = vec![
            "focus on the endgame".to_string(),
            "name the exact move".to_string(),
        ];

        let call = runtime.render_plan(&input).expect("renders");

        assert!(
            call.user.contains("Required swarm worker count: 4"),
            "the user turn states the required count: {}",
            call.user
        );
        assert!(
            call.user.contains("## User instructions"),
            "the user instructions section is rendered: {}",
            call.user
        );
        assert!(
            call.user.contains("- focus on the endgame")
                && call.user.contains("- name the exact move"),
            "instructions are rendered oldest first: {}",
            call.user
        );
        assert!(
            call.system.contains("required swarm worker count"),
            "the protocol explains the count rule: {}",
            call.system
        );

        input.swarm_workers = None;
        input.instructions.clear();
        let call = runtime.render_plan(&input).expect("renders");
        assert!(call.user.contains("Required swarm worker count: none"));
        assert!(
            call.user.lines().any(|line| line.trim() == "none"),
            "an empty instruction list renders as none: {}",
            call.user
        );
    }

    #[tokio::test]
    async fn add_intent_rejects_a_swarm_plan_with_the_wrong_worker_count() {
        let plan_args = |workers: usize, subtasks: usize| {
            let workers: Vec<serde_json::Value> = (0..workers)
                .map(|n| {
                    serde_json::json!({
                        "key": format!("worker-{n}"),
                        "role": "researcher",
                        "prompt": "do the work",
                        "max_turns": 4
                    })
                })
                .collect();
            let subtasks: Vec<serde_json::Value> = (0..subtasks)
                .map(|n| {
                    serde_json::json!({
                        "id": format!("sub-{n}"),
                        "description": "do the work",
                        "worker_key": format!("worker-{n}")
                    })
                })
                .collect();
            serde_json::json!({
                "description": "split the research",
                "plan": {
                    "topology": "swarm",
                    "workers": workers,
                    "subtasks": subtasks
                }
            })
        };

        for (workers, subtasks) in [(2, 2), (4, 3), (3, 2)] {
            let sink = PlannerSink::with_requirements(Some(dhi_core::Topology::Swarm), Some(3));
            let tools = sink.tools().expect("planner tools build");
            let err = tools[0]
                .run(plan_args(workers, subtasks))
                .await
                .expect_err("a mismatched swarm count must be a tool error");
            assert!(
                format!("{err}").contains("required swarm worker count 3"),
                "{workers}/{subtasks}: {err}"
            );
            assert!(
                format!("{err}").contains("retry with exactly 3 workers and 3 subtasks"),
                "{workers}/{subtasks}: {err}"
            );
        }

        let sink = PlannerSink::with_requirements(Some(dhi_core::Topology::Swarm), Some(3));
        let tools = sink.tools().expect("planner tools build");
        tools[0]
            .run(plan_args(3, 3))
            .await
            .expect("a plan with the exact count is accepted on retry");
        let PlanOutcome::AddIntent {
            plan: Some(plan), ..
        } = sink.finish("ignored after a tool call")
        else {
            panic!("expected a plan");
        };
        assert_eq!(plan.workers.len(), 3);
        assert_eq!(plan.subtasks.len(), 3);
    }

    #[tokio::test]
    async fn add_intent_rejects_invalid_plans() {
        let worker = serde_json::json!({
            "key": "worker-a",
            "role": "reviewer",
            "prompt": "review the patch",
            "max_turns": 8
        });
        let subtask = serde_json::json!({
            "id": "sub-1",
            "description": "review the change",
            "worker_key": "worker-a"
        });

        let invalid_topology = serde_json::json!({
            "description": "review the patch",
            "plan": {
                "topology": "mesh",
                "workers": [worker.clone()],
                "subtasks": [subtask.clone()]
            }
        });
        let zero_workers = serde_json::json!({
            "description": "review the patch",
            "plan": {
                "topology": "manager_worker",
                "workers": [],
                "subtasks": [subtask.clone()]
            }
        });
        let unknown_worker = serde_json::json!({
            "description": "review the patch",
            "plan": {
                "topology": "manager_worker",
                "workers": [worker],
                "subtasks": [{
                    "id": "sub-1",
                    "description": "review the change",
                    "worker_key": "ghost"
                }]
            }
        });

        for args in [invalid_topology, zero_workers, unknown_worker] {
            let sink = PlannerSink::default();
            let tools = sink.tools().expect("planner tools build");
            assert!(
                tools[0].run(args).await.is_err(),
                "an invalid plan must be a tool error"
            );
        }
    }

    #[test]
    fn plan_outcome_round_trips_with_plan() {
        let outcome = PlanOutcome::AddIntent {
            description: "review the patch".to_string(),
            plan: Some(dhi_core::IntentPlan {
                intent: "review the patch".to_string(),
                topology: dhi_core::Topology::Swarm,
                workers: vec![dhi_core::WorkerSpec {
                    key: "worker-a".to_string(),
                    role: "reviewer".to_string(),
                    prompt: "review the patch".to_string(),
                    max_turns: 3,
                }],
                subtasks: vec![dhi_core::Subtask {
                    id: "sub-1".to_string(),
                    description: "review the change".to_string(),
                    worker_key: Some("worker-a".to_string()),
                }],
                limits: dhi_core::CapabilityLimits::default(),
            }),
        };
        let encoded = serde_json::to_string(&outcome).unwrap();
        let decoded: PlanOutcome = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, outcome);

        let legacy: PlanOutcome =
            serde_json::from_str(r#"{"AddIntent":{"description":"check the pin"}}"#).unwrap();
        assert_eq!(
            legacy,
            PlanOutcome::AddIntent {
                description: "check the pin".to_string(),
                plan: None,
            }
        );
    }

    #[tokio::test]
    async fn give_up_is_a_first_call_outcome() {
        let sink = PlannerSink::default();
        let tools = sink.tools().expect("planner tools build");
        let goal_id = Uuid::new_v4();
        tools[2]
            .run(serde_json::json!({
                "goal_id": goal_id,
                "reason": "the confirmed evidence rules this goal out",
            }))
            .await
            .expect("the first give_up call is accepted");
        assert_eq!(
            sink.finish("ignored after a tool call"),
            PlanOutcome::GiveUp {
                goal_id,
                reason: "the confirmed evidence rules this goal out".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn mock_planner_is_deterministic() {
        let agent = MockAgentRuntime;
        let input = plan_input();
        let out = agent.plan(input).await.unwrap();
        assert!(matches!(out.output, PlanOutcome::AddIntent { .. }));
        assert!(
            out.meta.calls.is_empty(),
            "the mock makes no completion call to record"
        );
        assert_eq!(out.meta.usage, Usage::default());
    }

    #[tokio::test]
    async fn mock_planner_proves_from_the_first_confirmed_fact() {
        let agent = MockAgentRuntime;
        let mut input = plan_input();
        let goal_id = input.goals[0].id;
        let mut confirmed = input.facts[0].clone();
        confirmed.id = Uuid::new_v4();
        confirmed.state = dhi_core::FactState::Confirmed;
        confirmed.claim = "the bishop can reach g5".to_string();
        let fact_id = confirmed.id;
        input.facts.push(confirmed);

        let out = agent.plan(input).await.unwrap();
        assert_eq!(
            out.output,
            PlanOutcome::ProveGoal {
                goal_id,
                fact_id,
                reason: "mock planner proves from the first confirmed fact".to_string()
            }
        );
    }

    #[tokio::test]
    async fn mock_worker_keeps_its_output() {
        let agent = MockAgentRuntime;
        let out = agent
            .execute(ExecuteInput {
                task_id: Uuid::new_v4(),
                intent_id: Uuid::new_v4(),
                intent: "check the pin".to_string(),
                context: vec![],
                worker_profile_id: None,
                max_turns: None,
            })
            .await
            .unwrap();
        assert_eq!(
            out.output.fact,
            FactDraft {
                summary: "Executed intent: check the pin".to_string(),
                detail: String::new(),
                evidence: "mock evidence".to_string(),
                confidence: "observed".to_string(),
            }
        );
        assert!(out.output.tool_calls.is_empty());
        assert!(out.meta.calls.is_empty());
    }

    fn manager_worker(key: &str) -> dhi_core::WorkerSpec {
        dhi_core::WorkerSpec {
            key: key.to_string(),
            role: "role".to_string(),
            prompt: format!("act as {key}"),
            max_turns: 8,
        }
    }

    fn manager_subtask(id: &str) -> dhi_core::Subtask {
        dhi_core::Subtask {
            id: id.to_string(),
            description: format!("do {id}"),
            worker_key: None,
        }
    }

    fn manager_plan() -> dhi_core::IntentPlan {
        dhi_core::IntentPlan {
            intent: "review the patch".to_string(),
            topology: dhi_core::Topology::ManagerWorker,
            workers: vec![
                manager_worker("alpha"),
                manager_worker("beta"),
                manager_worker("gamma"),
            ],
            subtasks: vec![manager_subtask("s1"), manager_subtask("s2")],
            limits: dhi_core::CapabilityLimits {
                max_workers: 2,
                ..dhi_core::CapabilityLimits::default()
            },
        }
    }

    fn assignment(subtask_id: &str, worker_key: &str) -> ManagerAssignment {
        ManagerAssignment {
            subtask_id: subtask_id.to_string(),
            worker_key: worker_key.to_string(),
        }
    }

    #[test]
    fn manager_choice_selects_workers_and_assigns_every_subtask() {
        let plan = manager_plan();
        let choice = ManagerChoice {
            worker_keys: vec!["alpha".to_string(), "gamma".to_string()],
            assignments: vec![assignment("s2", "gamma"), assignment("s1", "alpha")],
        };

        let selected = apply_manager_choice(&plan, &choice).expect("the choice applies");

        assert_eq!(
            selected
                .workers
                .iter()
                .map(|worker| worker.key.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "gamma"]
        );
        assert_eq!(
            selected
                .subtasks
                .iter()
                .map(|subtask| (subtask.id.as_str(), subtask.worker_key.as_deref().unwrap()))
                .collect::<Vec<_>>(),
            [("s1", "alpha"), ("s2", "gamma")]
        );
        assert_eq!(selected.limits, plan.limits, "limits stay the planner's");
        selected.validate().expect("the selected plan validates");
    }

    #[test]
    fn manager_choice_rejects_an_unknown_worker() {
        let plan = manager_plan();
        let choice = ManagerChoice {
            worker_keys: vec!["ghost".to_string()],
            assignments: vec![assignment("s1", "ghost"), assignment("s2", "ghost")],
        };

        let err = apply_manager_choice(&plan, &choice).expect_err("ghost is not allowed");
        assert!(err.to_string().contains("unknown worker"), "{err}");
    }

    #[test]
    fn manager_choice_rejects_a_missing_subtask_assignment() {
        let plan = manager_plan();
        let choice = ManagerChoice {
            worker_keys: vec!["alpha".to_string()],
            assignments: vec![assignment("s1", "alpha")],
        };

        let err = apply_manager_choice(&plan, &choice).expect_err("s2 has no assignment");
        assert!(
            err.to_string().contains("no assignment for subtask \"s2\""),
            "{err}"
        );
    }

    #[test]
    fn manager_choice_rejects_a_duplicate_assignment() {
        let plan = manager_plan();
        let choice = ManagerChoice {
            worker_keys: vec!["alpha".to_string()],
            assignments: vec![
                assignment("s1", "alpha"),
                assignment("s1", "alpha"),
                assignment("s2", "alpha"),
            ],
        };

        let err = apply_manager_choice(&plan, &choice).expect_err("s1 is assigned twice");
        assert!(
            err.to_string().contains("more than once"),
            "duplicate assignment must be rejected: {err}"
        );
    }

    #[test]
    fn manager_choice_rejects_excess_workers() {
        let plan = manager_plan();
        let choice = ManagerChoice {
            worker_keys: vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()],
            assignments: vec![assignment("s1", "alpha"), assignment("s2", "beta")],
        };

        let err = apply_manager_choice(&plan, &choice).expect_err("max_workers is two");
        assert!(err.to_string().contains("above max_workers"), "{err}");
    }

    #[test]
    fn manager_choice_keeps_the_planners_worker_order() {
        let mut plan = manager_plan();
        plan.limits.max_workers = 3;
        let choice = ManagerChoice {
            worker_keys: vec!["gamma".to_string(), "alpha".to_string(), "beta".to_string()],
            assignments: vec![assignment("s1", "gamma"), assignment("s2", "alpha")],
        };

        let selected = apply_manager_choice(&plan, &choice).expect("the choice applies");

        assert_eq!(
            selected
                .workers
                .iter()
                .map(|worker| worker.key.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "beta", "gamma"],
            "the selected plan follows the planner's worker order"
        );
        assert_eq!(
            selected
                .subtasks
                .iter()
                .map(|subtask| subtask.id.as_str())
                .collect::<Vec<_>>(),
            ["s1", "s2"],
            "subtask order is preserved"
        );
    }

    #[test]
    fn manager_selection_prompt_names_workers_limits_and_schema() {
        let plan = manager_plan();
        let (system, user) = manager_selection_prompt(&plan);

        assert!(system.contains("DHI+ manager"), "{system}");
        assert!(system.contains("allowed_workers"), "{system}");
        assert!(system.contains("JSON only"), "{system}");
        assert!(
            system.contains(
                "{\"worker_keys\":[\"...\"],\"assignments\":[{\"subtask_id\":\"...\",\"worker_key\":\"...\"}]}"
            ),
            "{system}"
        );
        assert!(system.contains("max_workers"), "{system}");
        assert!(system.contains("every subtask exactly once"), "{system}");

        let input: serde_json::Value = serde_json::from_str(&user).expect("the input is JSON");
        assert_eq!(input["intent"], "review the patch");
        assert_eq!(input["allowed_workers"][0]["key"], "alpha");
        assert_eq!(input["allowed_workers"][0]["max_turns"], 8);
        assert_eq!(input["subtasks"][0]["worker_key"], serde_json::Value::Null);
        assert_eq!(input["limits"]["max_workers"], 2);
        assert_eq!(input["limits"]["max_parallel"], 4);
    }

    #[test]
    fn manager_choice_parses_from_a_fenced_fixture_and_applies() {
        let fixture = "Here is the choice:\n```json\n\
            {\"worker_keys\":[\"beta\"],\"assignments\":[\
            {\"subtask_id\":\"s1\",\"worker_key\":\"beta\"},\
            {\"subtask_id\":\"s2\",\"worker_key\":\"beta\"}]}\n```\n";
        let choice = parse_manager_choice(fixture).expect("the fenced JSON parses");
        assert_eq!(choice.worker_keys, ["beta"]);
        assert_eq!(choice.assignments.len(), 2);

        let selected = apply_manager_choice(&manager_plan(), &choice).expect("the choice applies");
        assert_eq!(selected.workers.len(), 1);
        assert_eq!(selected.workers[0].key, "beta");
        assert!(
            selected
                .subtasks
                .iter()
                .all(|subtask| subtask.worker_key.as_deref() == Some("beta"))
        );
    }

    #[test]
    fn manager_choice_rejects_non_json_text() {
        let err = parse_manager_choice("I choose alpha.").expect_err("there is no JSON object");
        assert!(err.to_string().contains("no JSON object"), "{err}");
    }

    #[tokio::test]
    async fn mock_select_team_leaves_the_plan_unchanged() {
        let plan = manager_plan();
        let call = MockAgentRuntime
            .select_team(TeamSelectionInput {
                task_id: Uuid::new_v4(),
                intent_id: Uuid::new_v4(),
                intent: plan.intent.clone(),
                plan: plan.clone(),
                worker_profile_id: None,
            })
            .await
            .expect("the default selection succeeds");

        assert_eq!(call.output, plan);
        assert!(call.meta.calls.is_empty());
        assert!(call.prompt.is_empty());
        assert!(call.response.is_empty());
        assert_eq!(call.thinking, None);
    }

    #[tokio::test]
    async fn worker_fact_cannot_be_recorded_twice() {
        let sink = WorkerSink::default();
        let tools = sink.tools().expect("worker tools build");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name().as_str(), "record_fact");

        tools[0]
            .run(serde_json::json!({
                "summary": "the pin holds",
                "detail": "the bishop cannot move",
                "evidence": "read the position",
                "confidence": "observed",
            }))
            .await
            .expect("the first record_fact call is accepted");
        assert!(
            tools[0]
                .run(serde_json::json!({
                    "summary": "a replacement fact",
                    "evidence": "different evidence",
                    "confidence": "inferred",
                }))
                .await
                .is_err(),
            "a second record_fact call is a tool error"
        );
        assert_eq!(
            sink.finish().expect("the first draft stands"),
            FactDraft {
                summary: "the pin holds".to_string(),
                detail: "the bishop cannot move".to_string(),
                evidence: "read the position".to_string(),
                confidence: "observed".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn worker_without_record_fact_is_an_error() {
        let sink = WorkerSink::default();
        let err = sink
            .finish()
            .expect_err("prose without a record_fact call is not a fact");
        assert!(err.to_string().contains("record_fact"), "{err}");
    }

    #[test]
    fn record_fact_retry_appends_the_instruction_and_marks_the_hash() {
        let runtime = runtime_with("river-planner", "river-worker");
        let call = runtime.render_execute(&execute_input()).expect("renders");
        let retry = retry_with_record_fact(&call);

        assert_eq!(retry.role, call.role);
        assert_eq!(retry.user, call.user, "only the system turn changes");
        assert_eq!(
            retry.system,
            format!("{}\n\n{}", call.system, RECORD_FACT_RETRY_INSTRUCTION)
        );
        assert!(
            retry
                .system
                .contains("CRITICAL: Your previous attempt did not call record_fact"),
            "{}",
            retry.system
        );
        assert_eq!(retry.prompt_hash, format!("{}+retry", call.prompt_hash));
        assert_ne!(retry.prompt_hash, call.prompt_hash);
    }

    #[test]
    fn only_the_missing_record_fact_error_earns_a_retry() {
        let missing = WorkerSink::default()
            .finish()
            .expect_err("an empty sink never recorded a fact");
        assert!(is_missing_record_fact(&missing));

        for other in [
            anyhow::anyhow!("worker sink lock poisoned"),
            anyhow::anyhow!("provider unavailable"),
        ] {
            assert!(
                !is_missing_record_fact(&other),
                "{other} must not trigger the record_fact retry"
            );
        }
    }

    #[test]
    fn private_tool_servers_isolate_per_attempt_registrations() {
        let shared = ToolServer::new().run();
        let first = private_tool_server(&shared);
        let second = private_tool_server(&shared);

        let sink = WorkerSink::default();
        for tool in sink.tools().expect("worker tools build") {
            first.add_dynamic_tool(tool);
        }

        assert!(
            first.toolset().get("record_fact").is_some(),
            "the attempt registry owns its record_fact"
        );
        assert!(
            second.toolset().get("record_fact").is_none(),
            "a concurrent attempt must not see another attempt's record_fact"
        );
        assert!(
            shared.toolset().get("record_fact").is_none(),
            "per-attempt tools never mutate the shared guarded registry"
        );
    }

    #[tokio::test]
    async fn worker_fact_requires_evidence_and_a_known_confidence() {
        let sink = WorkerSink::default();
        let tools = sink.tools().expect("worker tools build");

        assert!(
            tools[0]
                .run(serde_json::json!({
                    "summary": "no evidence",
                    "confidence": "observed",
                }))
                .await
                .is_err(),
            "evidence is required"
        );
        assert!(
            tools[0]
                .run(serde_json::json!({
                    "summary": "a guess",
                    "evidence": "none",
                    "confidence": "certain",
                }))
                .await
                .is_err(),
            "confidence must be observed or inferred"
        );
        assert!(sink.finish().is_err(), "neither call recorded a draft");
    }

    #[test]
    fn rig_usage_maps_to_dhi_usage() {
        let usage = rig::completion::Usage::new()
            .input_tokens(1200u64)
            .output_tokens(340u64)
            .cached_input_tokens(800u64)
            .cache_creation_input_tokens(15u64)
            .reasoning_tokens(120u64);
        let mapped = usage_from_rig(&usage);
        assert_eq!(mapped.input_tokens, 1200);
        assert_eq!(mapped.output_tokens, 340);
        assert_eq!(mapped.cache_read_tokens, 800);
        assert_eq!(mapped.cache_write_tokens, 15);
        assert_eq!(mapped.reasoning_tokens, 120);
        assert_eq!(
            mapped.cost_usd, None,
            "Rig without a catalog price reports no cost"
        );
    }

    #[test]
    fn rig_unset_counters_map_to_zero() {
        let mapped = usage_from_rig(&rig::completion::Usage::new());
        assert_eq!(mapped, Usage::default());
    }

    #[test]
    fn record_role_matches_the_security_vocabulary() {
        assert_eq!(record_role(Role::Planner), dhi_core::AgentRole::Planner);
        assert_eq!(record_role(Role::Worker), dhi_core::AgentRole::Worker);
    }

    #[test]
    fn usage_default_is_zero_and_unpriced() {
        let usage = Usage::default();
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.cost_usd, None);
    }

    #[test]
    fn usage_aggregates_but_never_derives_cost() {
        let first = Usage {
            input_tokens: 100,
            output_tokens: 20,
            cache_read_tokens: 60,
            cache_write_tokens: 10,
            reasoning_tokens: 5,
            cost_usd: None,
        };
        let second = Usage {
            input_tokens: 1,
            cost_usd: Some(0.0012),
            ..Usage::default()
        };
        let summed = first.saturating_add(second);
        assert_eq!(summed.input_tokens, 101);
        assert_eq!(summed.output_tokens, 20);
        assert_eq!(summed.cache_read_tokens, 60);
        assert_eq!(summed.cache_write_tokens, 10);
        assert_eq!(summed.reasoning_tokens, 5);
        assert_eq!(
            summed.cost_usd,
            Some(0.0012),
            "cost is provider-reported or absent, never computed"
        );
        assert_eq!(
            Usage::default().saturating_add(Usage::default()).cost_usd,
            None
        );
    }

    fn thinking_candidate(thinking_type: &str, reasoning_effort: &str) -> ResolvedProfile {
        ResolvedProfile {
            id: 1,
            name: "thinking".to_string(),
            format: ProfileFormat::Openai,
            base_url: "https://river.example/v1".to_string(),
            model: "river".to_string(),
            api_key: "secret".to_string(),
            max_tokens: 0,
            streaming: true,
            thinking_type: thinking_type.to_string(),
            reasoning_effort: reasoning_effort.to_string(),
        }
    }

    #[test]
    fn thinking_options_map_empty_effort_and_disabled() {
        let empty = thinking_options(&thinking_candidate("", ""));
        assert_eq!(empty.reasoning, None);
        assert_eq!(empty.additional_params, None);

        let disabled = thinking_options(&thinking_candidate("disabled", ""));
        assert_eq!(disabled.reasoning, Some(Reasoning::Off));
        assert_eq!(disabled.additional_params, None);

        // An explicit disable wins over an effort level.
        let disabled_with_effort = thinking_options(&thinking_candidate("disabled", "max"));
        assert_eq!(disabled_with_effort.reasoning, Some(Reasoning::Off));
        assert_eq!(disabled_with_effort.additional_params, None);
    }

    #[test]
    fn thinking_options_map_every_effort() {
        for (raw, expected) in [
            ("low", Effort::Low),
            ("medium", Effort::Medium),
            ("high", Effort::High),
            ("xhigh", Effort::XHigh),
            ("max", Effort::Max),
        ] {
            let mapped = thinking_options(&thinking_candidate("", raw));
            assert_eq!(mapped.reasoning, Some(Reasoning::Effort(expected)));
            assert_eq!(mapped.additional_params, None);
        }
    }

    #[test]
    fn thinking_options_enabled_without_effort_uses_provider_params() {
        let enabled = thinking_options(&thinking_candidate("enabled", ""));
        assert_eq!(enabled.reasoning, None);
        assert_eq!(
            enabled.additional_params,
            Some(serde_json::json!({"thinking": {"type": "enabled"}}))
        );

        // With an effort the portable rig level is enough.
        let enabled_high = thinking_options(&thinking_candidate("enabled", "high"));
        assert_eq!(
            enabled_high.reasoning,
            Some(Reasoning::Effort(Effort::High))
        );
        assert_eq!(enabled_high.additional_params, None);
    }

    #[test]
    fn response_thinking_extracts_only_reasoning_parts() {
        let response: rig::agent::PromptResponse = serde_json::from_value(serde_json::json!({
            "usage": {},
            "content": [
                {"type": "reasoning", "text": "check the pin"},
                {"type": "text", "text": "the bishop holds"},
            ],
        }))
        .unwrap();
        assert_eq!(
            response_thinking(&response),
            Some("check the pin".to_string())
        );

        let no_reasoning: rig::agent::PromptResponse = serde_json::from_value(
            serde_json::json!({"usage": {}, "content": [{"type": "text", "text": "done"}]}),
        )
        .unwrap();
        assert_eq!(response_thinking(&no_reasoning), None);

        let blank: rig::agent::PromptResponse = serde_json::from_value(serde_json::json!({
            "usage": {},
            "content": [{"type": "reasoning", "text": "   "}],
        }))
        .unwrap();
        assert_eq!(
            response_thinking(&blank),
            None,
            "blank reasoning is stored as NULL, not an empty string"
        );

        let transcript: rig::agent::PromptResponse = serde_json::from_value(serde_json::json!({
            "usage": {},
            "content": [{"type": "text", "text": "answer"}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "question"}]},
                {"role": "assistant", "content": [
                    {"type": "reasoning", "text": "first turn"},
                    {"type": "text", "text": "working"},
                ]},
                {"role": "assistant", "content": [
                    {"type": "reasoning", "text": "first turn"},
                    {"type": "text", "text": "answer"},
                ]},
            ],
        }))
        .unwrap();
        assert_eq!(
            response_thinking(&transcript),
            Some("first turn".to_string()),
            "multi-turn reasoning is read from messages and deduped"
        );
    }

    #[test]
    fn agent_call_meta_roundtrips() {
        let meta = AgentCallMeta {
            usage: Usage {
                input_tokens: 7,
                ..Usage::default()
            },
            calls: vec![AgentCallRecord {
                run_id: Uuid::new_v4(),
                call_index: 0,
                role: Role::Planner,
                model: "m".to_string(),
                profile: "open_ai/m/max_tokens=4096/stream=true".to_string(),
                status: "ok".to_string(),
                error: None,
                latency_ms: 12,
                usage: Usage::default(),
                prompt_hash: "planner@abc".to_string(),
            }],
        };
        let json = serde_json::to_string(&meta).unwrap();
        assert!(json.contains("\"role\":\"planner\""));
        assert_eq!(serde_json::from_str::<AgentCallMeta>(&json).unwrap(), meta);
    }
}
