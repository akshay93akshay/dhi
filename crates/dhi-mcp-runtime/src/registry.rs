//! The registry behind guarded MCP tools and the guard's dispatch backend.
//!
//! `GuardedMcpRegistry` owns the raw [`McpTool`] for every model-visible
//! guarded name and records which server owns that name. `McpBackend` reads
//! it to dispatch an allowed call; `McpManager` is the only writer.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, RwLock};

use anyhow::anyhow;
use async_trait::async_trait;
use dhi_agent::{RunRole, RunScope, ToolServerView, ToolSink};
use dhi_core::ToolCallRequest;
use dhi_guard::ToolBackend;
use dhi_mcp_upgrade::{
    CALL_TOOL_NAME, CatalogTool, DeferredError, DeferredPolicy, DeferredToolset, DirectTool,
    GuardScope, GuardedCall, GuardedInvoker, LIST_TOOLS_NAME, ServerCatalog, ServerToolListing,
    ToolCallOutcome, ToolDefinition, ToolPlan, plan_tools,
};
use rig::message::ToolName;
use rig::tool::rmcp::McpTool;
use rig::tool::server::{ToolServer, ToolServerHandle};
use rig::tool::{DynamicTool, ToolContext, ToolExecutionError, ToolOutput};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::config::{TOOL_PREFIX, ValidatedServer};

/// The default per-server threshold for deferred schemas. A server with this
/// many tools (or more) that are visible to a role exposes `mcp_list_tools`
/// and `mcp_call_tool` instead of every direct schema; a server below the
/// threshold keeps direct registration exactly as before.
pub const DEFAULT_DEFERRED_THRESHOLD: usize = dhi_mcp::DEFAULT_DEFERRED_THRESHOLD;

/// An error text keeps the model-readable part bounded: an MCP server may
/// put its whole (huge) output into an error result.
const MAX_ERROR_CHARS: usize = 2_000;

/// The registry state behind the guarded MCP tools: the model-visible
/// guarded name → the raw [`McpTool`] that performs the server call, and who
/// owns each name. The [`McpBackend`] reads; the [`McpManager`] writes.
///
/// [`McpManager`]: super::manager::McpManager
#[derive(Default)]
struct RegistryState {
    tools: HashMap<String, McpTool>,
    owners: HashMap<String, String>,
}

/// One registry entry as a read-only snapshot for catalog/visibility wiring:
/// the guarded name, the server that owns it, and the tool metadata the MCP
/// server advertised.
#[derive(Debug, Clone, PartialEq)]
pub struct RegistryToolSnapshot {
    /// The model-visible guarded name: `mcp__<server>__<tool>`.
    pub guarded_name: String,
    /// The tool name as the MCP server reports it (not guarded).
    pub mcp_name: String,
    /// The server that owns the guarded name.
    pub owner: String,
    /// The description advertised by the server (empty when absent).
    pub description: String,
    /// The input schema advertised by the server.
    pub schema: Value,
}

/// The MCP side of the shared tool registry. Writing happens only inside the
/// manager's registration paths; dispatch takes a short read and clones the
/// raw tool. Raw MCP tools live here and nowhere else — the model-visible
/// registration is always the guarded factory's tool.
#[derive(Default)]
pub struct GuardedMcpRegistry {
    state: tokio::sync::RwLock<RegistryState>,
}

impl GuardedMcpRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The guarded names currently registered across all servers, sorted.
    pub async fn names(&self) -> Vec<String> {
        let state = self.state.read().await;
        let mut names: Vec<String> = state.tools.keys().cloned().collect();
        names.sort();
        names
    }

    /// A read-only snapshot of every registered tool, sorted by guarded
    /// name, for catalog/visibility consumers. The guarded names are exactly
    /// the ones [`GuardedMcpRegistry::names`] reports.
    pub async fn snapshot(&self) -> Vec<RegistryToolSnapshot> {
        let state = self.state.read().await;
        let mut snapshot: Vec<RegistryToolSnapshot> = state
            .tools
            .iter()
            .filter_map(|(guarded_name, tool)| {
                let owner = state.owners.get(guarded_name)?.clone();
                let definition = tool.definition();
                Some(RegistryToolSnapshot {
                    guarded_name: guarded_name.clone(),
                    mcp_name: definition.name.to_string(),
                    owner,
                    description: definition
                        .description
                        .as_deref()
                        .unwrap_or_default()
                        .to_string(),
                    schema: definition.schema_as_json_value(),
                })
            })
            .collect();
        snapshot.sort_by(|left, right| left.guarded_name.cmp(&right.guarded_name));
        snapshot
    }

    /// The server that owns `name`, when the name is taken.
    pub(crate) async fn owner_of(&self, name: &str) -> Option<String> {
        self.state.read().await.owners.get(name).cloned()
    }

    pub(crate) async fn install(&self, name: String, tool: McpTool, owner: &str) {
        let mut state = self.state.write().await;
        state.owners.insert(name.clone(), owner.to_string());
        state.tools.insert(name, tool);
    }

    pub(crate) async fn remove(&self, name: &str) {
        let mut state = self.state.write().await;
        state.owners.remove(name);
        state.tools.remove(name);
    }

    pub(crate) async fn clear(&self) {
        let mut state = self.state.write().await;
        state.owners.clear();
        state.tools.clear();
    }
}

/// The guard's backend for MCP tools: the one `ToolBackend` step after an
/// `Allow` verdict (or a granted approval).
///
/// For a guarded name `mcp__<server>__<tool>` it takes the raw [`McpTool`]
/// recorded at discovery, converts it to its rmcp dynamic-tool adapter, and
/// runs it with the model's arguments. That adapter is the tested rmcp
/// mapping: arguments must be a JSON object (else invalid-args), one call is
/// bounded by the server's configured per-call timeout (with cancellation),
/// structured content and content blocks become [`ToolOutput`], an
/// `is_error` result becomes a model-readable error carrying the tool's
/// output, and transport failures become timeout/provider errors. No path
/// panics: every failure is a `ToolExecutionError`, mapped to an error so
/// the guard records the call as failed and the model reads why.
pub struct McpBackend {
    registry: Arc<GuardedMcpRegistry>,
}

impl McpBackend {
    pub fn new(registry: Arc<GuardedMcpRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl ToolBackend for McpBackend {
    async fn execute(&self, tool: &str, input: &Value) -> anyhow::Result<Value> {
        let raw = self
            .registry
            .state
            .read()
            .await
            .tools
            .get(tool)
            .cloned()
            .ok_or_else(|| anyhow!("no MCP tool is registered as {tool:?}"))?;
        let dynamic = DynamicTool::try_from(raw)
            .map_err(|err| anyhow!("MCP tool {tool:?} has an unusable name: {err}"))?;
        let mut context = ToolContext::new();
        let output = dynamic
            .run_with(&mut context, input.clone())
            .await
            .map_err(map_mcp_error)?;
        Ok(output_to_value(&output))
    }
}

/// Maps an rmcp tool error to the text the guard records and the model
/// reads: the operator message plus the model-visible feedback, bounded.
fn map_mcp_error(error: ToolExecutionError) -> anyhow::Error {
    let mut text = error.message().to_string();
    let feedback = match error.model_feedback() {
        Some(feedback) => feedback.to_string(),
        None => error.model_output().render(),
    };
    if !feedback.is_empty() && feedback != text {
        text.push_str(": ");
        text.push_str(&feedback);
    }
    anyhow!(short_error_text(&text))
}

/// Caps an error text, stating the truncation so nothing is silently lost.
fn short_error_text(text: &str) -> String {
    let mut chars = text.chars();
    let prefix: String = chars.by_ref().take(MAX_ERROR_CHARS).collect();
    if chars.next().is_none() {
        return prefix;
    }
    format!("{prefix}… [truncated after {MAX_ERROR_CHARS} characters]")
}

/// `ToolOutput` → the JSON value the guard stores and the model reads:
/// structured content passes through, one text block becomes `{"text": …}`,
/// and multimodal or multi-block results keep every block (a JSON array).
fn output_to_value(output: &ToolOutput) -> Value {
    if let Some(value) = output.as_json() {
        return value.clone();
    }
    if let Some(text) = output.as_text() {
        return json!({ "text": text });
    }
    serde_json::to_value(output.as_content()).unwrap_or(Value::Null)
}

/// The one `ToolBackend` the guard runs: `mcp__`-prefixed tools go to the
/// [`McpBackend`], everything else to the delegate (the Firecrawl/sources
/// backend). An unknown MCP name still fails closed — inside
/// [`McpBackend`], so the prefix routing alone never decides an outcome.
pub struct McpRoutingBackend {
    mcp: McpBackend,
    delegate: Arc<dyn ToolBackend>,
}

impl McpRoutingBackend {
    pub fn new(mcp: McpBackend, delegate: Arc<dyn ToolBackend>) -> Self {
        Self { mcp, delegate }
    }
}

#[async_trait]
impl ToolBackend for McpRoutingBackend {
    async fn execute(&self, tool: &str, input: &Value) -> anyhow::Result<Value> {
        if tool.starts_with(TOOL_PREFIX) {
            self.mcp.execute(tool, input).await
        } else {
            self.delegate.execute(tool, input).await
        }
    }
}

// ---------------------------------------------------------------------------
// Visibility and deferred schemas (Gate A)
// ---------------------------------------------------------------------------

/// One server's DHI visibility extensions, as parsed from `ops/mcp.json`.
/// Everything is optional so a standard `mcpServers` file keeps its old
/// behavior: unset visibility means every role sees the server's tools.
#[derive(Debug, Clone, PartialEq, Eq)]
struct VisibilityEntry {
    /// `None` = no visibility configured (visible to every role, including
    /// every `specialist:<name>`); `Some([])` = visible to nobody.
    visible_to: Option<Vec<String>>,
    /// The per-server deferral threshold.
    deferred_threshold: usize,
    /// Per-tool overrides keyed by the MCP tool name. Only `Some` entries are
    /// stored; an entry without `visible_to` inherits the server list.
    tool_visible_to: BTreeMap<String, Vec<String>>,
}

/// The visibility extension parsed from one MCP config file.
///
/// This deliberately lives beside the manager rather than inside the
/// validated connection config: the manager's connection shape is unchanged,
/// and visibility remains an optional presentation policy. Loading the same
/// file again is cheap and keeps `ValidatedServer` standard.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpVisibilityConfig {
    servers: BTreeMap<String, VisibilityEntry>,
}

impl McpVisibilityConfig {
    /// Builds visibility from the manager's current validated servers. This
    /// is the live-reload path: it does not re-read the file.
    pub fn from_servers(servers: &[ValidatedServer]) -> Self {
        let mut entries = BTreeMap::new();
        for server in servers {
            entries.insert(
                server.name.trim().to_string(),
                VisibilityEntry {
                    visible_to: server.visible_to.clone(),
                    deferred_threshold: server
                        .deferred_threshold
                        .unwrap_or(DEFAULT_DEFERRED_THRESHOLD),
                    tool_visible_to: server.tool_visibility.clone(),
                },
            );
        }
        Self { servers: entries }
    }

    /// Extracts visibility from an already parsed config file. Server names
    /// are trimmed exactly as `McpServerEntry::validate` trims them.
    pub fn from_file(file: &dhi_mcp::McpFile) -> Self {
        let mut servers = BTreeMap::new();
        for (name, entry) in &file.servers {
            servers.insert(
                name.trim().to_string(),
                VisibilityEntry {
                    visible_to: entry.visible_to.clone(),
                    deferred_threshold: entry
                        .deferred_threshold
                        .unwrap_or(DEFAULT_DEFERRED_THRESHOLD),
                    tool_visible_to: entry.tool_visibility.clone(),
                },
            );
        }
        Self { servers }
    }

    /// Loads `path` (usually `dhi_mcp::config_path()`). A missing path or an
    /// unreadable/invalid file yields the backward-compatible default: no
    /// visibility filter and the default threshold. The manager has already
    /// logged the config problem; this only preserves pre-visibility
    /// behavior instead of inventing a filter from a half-parsed file.
    pub fn load(path: Option<&Path>) -> Self {
        let Some(path) = path else {
            return Self::default();
        };
        match dhi_mcp::McpFile::load(path) {
            Ok(Some(file)) => Self::from_file(&file),
            Ok(None) => Self::default(),
            Err(_) => Self::default(),
        }
    }

    /// True when no server carries an extension.
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// The configured threshold for `server`, or the default.
    pub fn deferred_threshold(&self, server: &str) -> usize {
        self.servers
            .get(server.trim())
            .map(|entry| entry.deferred_threshold)
            .unwrap_or(DEFAULT_DEFERRED_THRESHOLD)
    }
}

/// One server as visibility planning sees it.
#[derive(Debug, Clone)]
struct PlannedServer {
    name: String,
    visible_to: Option<Vec<String>>,
    deferred_threshold: usize,
    tools: Vec<CatalogTool>,
}

/// The visibility/deferral plan for one role: direct guarded tools plus one
/// deferred toolset per server that crossed its threshold.
#[derive(Debug, Clone, PartialEq)]
pub struct RolePlan {
    /// The role this plan was computed for, trimmed (`worker`,
    /// `specialist:researcher`, ...). Unknown roles produce an empty plan.
    pub role: String,
    /// Tools registered directly under their guarded name.
    pub direct: Vec<DirectTool>,
    /// Per-server deferred toolsets. At least one means the meta-tools are
    /// exposed for this role.
    pub deferred: Vec<DeferredToolset>,
    /// Per-server planning errors (for example two tools that sanitize to the
    /// same guarded name). The affected server is omitted rather than widened.
    pub errors: Vec<String>,
}

impl RolePlan {
    /// True when this role sees `mcp_list_tools` / `mcp_call_tool` instead of
    /// the direct schemas of at least one server.
    pub fn is_deferred(&self) -> bool {
        !self.deferred.is_empty()
    }

    /// The directly visible guarded names.
    pub fn direct_names(&self) -> Vec<&str> {
        self.direct
            .iter()
            .map(|tool| tool.guarded_name.as_str())
            .collect()
    }

    /// Every visible guardable name: direct tools plus the two meta-tools
    /// when any server is deferred.
    pub fn allowed_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .direct
            .iter()
            .map(|tool| tool.guarded_name.clone())
            .collect();
        if self.is_deferred() {
            names.push(LIST_TOOLS_NAME.to_string());
            names.push(CALL_TOOL_NAME.to_string());
        }
        names
    }

    /// The model-visible definitions for this role.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions: Vec<ToolDefinition> =
            self.direct.iter().map(DirectTool::definition).collect();
        if let Some(toolset) = self.deferred.first() {
            definitions.extend(toolset.definitions());
        }
        definitions
    }

    /// The deferred servers as `mcp_list_tools` reports them. Direct tools are
    /// already visible by schema, so only deferred tools need discovery.
    pub fn list(&self) -> Vec<ServerToolListing> {
        self.deferred
            .iter()
            .flat_map(DeferredToolset::list)
            .collect()
    }

    /// The number of tools behind the deferred meta-tools.
    pub fn deferred_tool_count(&self) -> usize {
        self.deferred
            .iter()
            .map(|toolset| toolset.tools().len())
            .sum()
    }

    /// Resolves one `mcp_call_tool` input to the real guarded name. Unknown
    /// servers, unknown tools, tools hidden by visibility, and non-object or
    /// oversized arguments all fail closed.
    pub fn call_from_input(&self, input: &Value) -> Result<GuardedCall, DeferredError> {
        let input: MetaCallInput = serde_json::from_value(input.clone())
            .map_err(|error| DeferredError::InvalidInput(error.to_string()))?;
        self.call_tool(&input.server, &input.tool, input.arguments)
    }

    /// Resolves one meta-tool call against this role's deferred sets. A
    /// server that is visible only through direct schemas is not addressable
    /// here; the model calls that tool by its guarded name instead.
    pub fn call_tool(
        &self,
        server: &str,
        tool: &str,
        arguments: Value,
    ) -> Result<GuardedCall, DeferredError> {
        let Some(toolset) = self
            .deferred
            .iter()
            .find(|toolset| toolset.tools().iter().any(|entry| entry.server == server))
        else {
            return Err(DeferredError::UnknownServer(server.to_string()));
        };
        toolset.call_tool(server, tool, arguments)
    }

    /// Executes a resolved meta-tool call through the supplied guarded
    /// invoker. Resolution failures are `DeferredError`; backend failures
    /// become [`ToolCallOutcome::TransportError`], distinct from a policy
    /// decision.
    pub async fn execute(
        &self,
        scope: &GuardScope,
        input: &Value,
        invoker: &dyn GuardedInvoker,
    ) -> Result<ToolCallOutcome, DeferredError> {
        let call = self.call_from_input(input)?;
        match invoker
            .invoke(scope, &call.guarded_name, call.arguments)
            .await
        {
            Ok(outcome) => Ok(outcome),
            Err(error) => Ok(ToolCallOutcome::TransportError {
                message: error.to_string(),
            }),
        }
    }
}

/// Plans direct or deferred exposure per role from the configured servers and
/// the tools the manager discovered.
///
/// A server without `visible_to` is expanded to the queried role, which is the
/// backward-compatible "visible to everyone" behavior (specialist roles are
/// open-ended, so they cannot be enumerated in a static list). An explicit
/// empty list stays empty: it is visible to nobody. Unknown roles therefore
/// see nothing.
#[derive(Debug, Clone, Default)]
pub struct McpRolePlanner {
    servers: Vec<PlannedServer>,
    all_mcp_names: BTreeSet<String>,
}

impl McpRolePlanner {
    /// Builds the planner from the config extensions and the registry
    /// snapshot (the manager's discovered tools, including descriptions and
    /// schemas).
    pub fn new(config: &McpVisibilityConfig, snapshot: &[RegistryToolSnapshot]) -> Self {
        let mut grouped: BTreeMap<String, Vec<&RegistryToolSnapshot>> = BTreeMap::new();
        for tool in snapshot {
            grouped.entry(tool.owner.clone()).or_default().push(tool);
        }
        let mut servers = Vec::with_capacity(grouped.len());
        for (name, tools) in grouped {
            let entry = config.servers.get(&name);
            let catalog_tools = tools
                .iter()
                .map(|tool| {
                    let mut catalog_tool = CatalogTool::new(tool.mcp_name.clone())
                        .with_description(tool.description.clone())
                        .with_input_schema(tool.schema.clone());
                    if let Some(roles) =
                        entry.and_then(|entry| entry.tool_visible_to.get(&tool.mcp_name))
                    {
                        catalog_tool = catalog_tool.with_visible_to(roles.clone());
                    }
                    catalog_tool
                })
                .collect();
            servers.push(PlannedServer {
                name: name.clone(),
                visible_to: entry.and_then(|entry| entry.visible_to.clone()),
                deferred_threshold: entry
                    .map(|entry| entry.deferred_threshold)
                    .unwrap_or(DEFAULT_DEFERRED_THRESHOLD),
                tools: catalog_tools,
            });
        }
        Self {
            servers,
            all_mcp_names: snapshot
                .iter()
                .map(|tool| tool.guarded_name.clone())
                .collect(),
        }
    }

    /// Builds the planner from a config path plus the current snapshot.
    pub fn from_config_path(path: Option<&Path>, snapshot: &[RegistryToolSnapshot]) -> Self {
        Self::new(&McpVisibilityConfig::load(path), snapshot)
    }

    /// True when at least one configured server could produce a deferred
    /// plan for some role. The meta-tools are registered once when this is
    /// true; each role's view includes them only when its own plan defers.
    pub fn may_defer(&self) -> bool {
        self.servers.iter().any(|server| {
            !server.tools.is_empty() && server.tools.len() >= server.deferred_threshold
        })
    }

    /// Every discovered guarded MCP name. Used to build role views and to
    /// filter specialist tool lists without touching non-MCP tools.
    pub fn all_guarded_names(&self) -> &BTreeSet<String> {
        &self.all_mcp_names
    }

    /// The plan for one role. Unknown roles get an empty plan (fail closed).
    pub fn plan(&self, role: &str) -> RolePlan {
        let role = role.trim();
        let defaults: Vec<String> = if dhi_mcp_upgrade::Role::parse(role).is_some() {
            vec![role.to_string()]
        } else {
            Vec::new()
        };
        let mut plan = RolePlan {
            role: role.to_string(),
            direct: Vec::new(),
            deferred: Vec::new(),
            errors: Vec::new(),
        };
        for server in &self.servers {
            let visible_to = server
                .visible_to
                .clone()
                .unwrap_or_else(|| defaults.clone());
            let catalog = ServerCatalog::new(server.name.clone(), visible_to, server.tools.clone());
            match plan_tools(
                role,
                std::slice::from_ref(&catalog),
                DeferredPolicy::new(server.deferred_threshold),
            ) {
                Ok(ToolPlan::Direct(tools)) => plan.direct.extend(tools),
                Ok(ToolPlan::Deferred(toolset)) => plan.deferred.push(toolset),
                Err(error) => plan.errors.push(format!("{}: {error}", server.name)),
            }
        }
        plan
    }

    /// Filters one specialist's configured tool list in place so it carries
    /// only MCP tools visible to `specialist:<name>`.
    ///
    /// Non-MCP tools are never touched. A specialist that listed no MCP tool
    /// stays unchanged (visibility never widens an allow-list); one that
    /// listed direct MCP names on a deferred server gets the two meta-tools
    /// instead. Hidden MCP names are removed.
    pub fn filter_specialist_tools(&self, specialist: &str, tools: &mut Vec<String>) -> usize {
        let role = format!("specialist:{}", specialist.trim());
        let plan = self.plan(&role);
        let direct: BTreeSet<&str> = plan.direct_names().into_iter().collect();
        let deferred: BTreeSet<&str> = plan
            .deferred
            .iter()
            .flat_map(DeferredToolset::tools)
            .map(|tool| tool.guarded_name.as_str())
            .collect();
        let is_meta = |tool: &str| tool == LIST_TOOLS_NAME || tool == CALL_TOOL_NAME;
        let touches_mcp = tools
            .iter()
            .any(|tool| self.all_mcp_names.contains(tool) || is_meta(tool));
        if !touches_mcp {
            return 0;
        }

        let before = tools.len();
        let mut referenced_deferred = false;
        let mut referenced_meta = false;
        tools.retain(|tool| {
            if is_meta(tool) {
                if plan.is_deferred() {
                    referenced_meta = true;
                }
                return plan.is_deferred();
            }
            if !self.all_mcp_names.contains(tool) {
                return true;
            }
            if direct.contains(tool.as_str()) {
                return true;
            }
            if deferred.contains(tool.as_str()) {
                referenced_deferred = true;
            }
            false
        });
        let removed = before.saturating_sub(tools.len());
        if plan.is_deferred() && (referenced_deferred || referenced_meta) {
            for name in [LIST_TOOLS_NAME, CALL_TOOL_NAME] {
                if !tools.iter().any(|tool| tool == name) {
                    tools.push(name.to_string());
                }
            }
        }
        removed
    }
}

/// Shared, refreshable role planner. The manager replaces the inner planner
/// after a live reload; worker views and meta-tools read the current value on
/// every dispatch.
pub type SharedMcpPlanner = Arc<RwLock<McpRolePlanner>>;

/// The worker's filtered view of the shared registry.
///
/// The shared registry remains the manager's master: every discovered guarded
/// tool is registered there, and specialists build their own subsets from it.
/// The worker attaches this view instead, so hidden MCP tools never appear in
/// its advertised tool list. Non-MCP tools (web, coding, PTY, sources) always
/// pass through.
#[derive(Clone)]
pub struct McpWorkerToolView {
    master: ToolServerHandle,
    planner: SharedMcpPlanner,
}

impl McpWorkerToolView {
    /// Builds the worker view from the master registry and the shared planner.
    pub fn new(master: &ToolServerHandle, planner: SharedMcpPlanner) -> Self {
        Self {
            master: master.clone(),
            planner,
        }
    }

    /// The names this view exposes right now.
    pub fn visible_names(&self) -> Vec<String> {
        self.tool_server()
            .toolset()
            .names()
            .map(str::to_string)
            .collect()
    }
}

impl ToolServerView for McpWorkerToolView {
    fn tool_server(&self) -> ToolServerHandle {
        let planner = self
            .planner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let plan = planner.plan("worker");
        let allowed_mcp: BTreeSet<&str> = plan
            .direct
            .iter()
            .map(|tool| tool.guarded_name.as_str())
            .collect();
        let hides_meta = planner.may_defer();
        let expose_meta = plan.is_deferred();
        drop(planner);

        let shared = self.master.toolset();
        let mut builder = ToolServer::new();
        for name in shared.names() {
            let is_meta = name == LIST_TOOLS_NAME || name == CALL_TOOL_NAME;
            if name.starts_with(TOOL_PREFIX) {
                if !allowed_mcp.contains(name) {
                    continue;
                }
            } else if is_meta && hides_meta && !expose_meta {
                continue;
            }
            if let Some(tool) = shared.get(name) {
                builder = builder.registered_tool(tool.clone());
            }
        }
        builder.run()
    }
}

/// The invoker behind `mcp_call_tool`: converts the trusted [`GuardScope`]
/// back to the runtime's `RunScope` ids and runs the real guarded name through
/// the one [`ToolSink`]. The sink's guard/audit therefore sees
/// `mcp__<server>__<tool>`, never `mcp_call_tool`.
struct SinkGuardedInvoker {
    sink: Arc<dyn ToolSink>,
}

#[async_trait]
impl GuardedInvoker for SinkGuardedInvoker {
    async fn invoke(
        &self,
        scope: &GuardScope,
        guarded_name: &str,
        arguments: Map<String, Value>,
    ) -> anyhow::Result<ToolCallOutcome> {
        let task_id = uuid::Uuid::parse_str(&scope.task_id)
            .map_err(|error| anyhow!("invalid task id in the deferred call scope: {error}"))?;
        let intent_id = uuid::Uuid::parse_str(&scope.intent_id)
            .map_err(|error| anyhow!("invalid intent id in the deferred call scope: {error}"))?;
        let request = ToolCallRequest {
            tool: guarded_name.to_string(),
            input: Value::Object(arguments),
        };
        let outcome = self.sink.execute(task_id, intent_id, request).await?;
        Ok(match outcome {
            dhi_core::ToolCallOutcome::Allowed { output } => ToolCallOutcome::Allowed { output },
            dhi_core::ToolCallOutcome::Denied { reason } => ToolCallOutcome::Denied { reason },
            dhi_core::ToolCallOutcome::Pending { approval_id } => ToolCallOutcome::Pending {
                approval_id: approval_id.to_string(),
            },
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MetaCallInput {
    server: String,
    tool: String,
    #[serde(default = "empty_arguments")]
    arguments: Value,
}

fn empty_arguments() -> Value {
    Value::Object(Map::new())
}

/// The `mcp_list_tools` definition. It mirrors
/// [`DeferredToolset::list_tool_definition`] because a role may have no
/// deferred toolset at registration time (another role still needs the tool).
fn list_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: LIST_TOOLS_NAME.to_string(),
        description: "List the MCP servers and tools this agent can call. \
                      Returns server and tool names with short descriptions."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
    }
}

/// The `mcp_call_tool` definition; mirrors
/// [`DeferredToolset::call_tool_definition`] for the same reason.
fn call_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: CALL_TOOL_NAME.to_string(),
        description: "Call one MCP tool by server and tool name. The call is \
                      routed to the real guarded tool so policy and audit see it."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "server": { "type": "string", "minLength": 1 },
                "tool": { "type": "string", "minLength": 1 },
                "arguments": { "type": "object" }
            },
            "required": ["server", "tool"],
            "additionalProperties": false
        }),
    }
}

/// Reads the trusted role stamped by the runtime. A dispatch without one
/// fails closed: a deferred call can never choose its own visibility.
fn context_role(context: &ToolContext) -> Result<String, ToolExecutionError> {
    let role: RunRole = context.require().map_err(|error| {
        ToolExecutionError::other(format!(
            "deferred MCP tool dispatch is missing its run role: {error}"
        ))
    })?;
    let role = role.as_str().trim().to_string();
    if role.is_empty() {
        return Err(ToolExecutionError::other(
            "deferred MCP tool dispatch has an empty run role",
        ));
    }
    Ok(role)
}

/// Registers the two deferred meta-tools on the master registry. Returns
/// `true` when they were installed.
///
/// Registration happens once, at startup, when any server could defer for any
/// role. The tools resolve per call from the `RunRole` stamped by the runtime,
/// so a worker and every specialist share the same pair while each sees only
/// its own visible set.
pub fn register_deferred_meta_tools(
    handle: &ToolServerHandle,
    planner: SharedMcpPlanner,
    sink: Arc<dyn ToolSink>,
) -> anyhow::Result<bool> {
    if !planner
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .may_defer()
    {
        return Ok(false);
    }
    let toolset = handle.toolset();
    if toolset.contains(LIST_TOOLS_NAME) || toolset.contains(CALL_TOOL_NAME) {
        tracing::warn!(
            "deferred MCP meta-tools not registered: {LIST_TOOLS_NAME:?} or \
             {CALL_TOOL_NAME:?} is already registered by another source"
        );
        return Ok(false);
    }

    let list_definition = list_tool_definition();
    let list_name = ToolName::new(list_definition.name.clone())
        .map_err(|error| anyhow!("invalid tool name {LIST_TOOLS_NAME:?}: {error}"))?;
    let list_planner = Arc::clone(&planner);
    let list_tool = DynamicTool::new_with_context(
        list_name,
        list_definition.description,
        list_definition.input_schema,
        move |context: &mut ToolContext, _arguments: Value| {
            let planner = Arc::clone(&list_planner);
            Box::pin(async move {
                let role = context_role(context)?;
                let plan = planner
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .plan(&role);
                if !plan.is_deferred() {
                    return Err(ToolExecutionError::other(format!(
                        "mcp_list_tools is not exposed to role {role:?}"
                    )));
                }
                Ok(ToolOutput::json(json!({ "servers": plan.list() })))
            })
        },
    );

    let invoker: Arc<dyn GuardedInvoker> = Arc::new(SinkGuardedInvoker { sink });
    let call_definition = call_tool_definition();
    let call_name = ToolName::new(call_definition.name.clone())
        .map_err(|error| anyhow!("invalid tool name {CALL_TOOL_NAME:?}: {error}"))?;
    let call_planner = planner;
    let call_tool = DynamicTool::new_with_context(
        call_name,
        call_definition.description,
        call_definition.input_schema,
        move |context: &mut ToolContext, arguments: Value| {
            let planner = Arc::clone(&call_planner);
            let invoker = Arc::clone(&invoker);
            Box::pin(async move {
                let role = context_role(context)?;
                let plan = planner
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .plan(&role);
                if !plan.is_deferred() {
                    return Err(ToolExecutionError::other(format!(
                        "mcp_call_tool is not exposed to role {role:?}"
                    )));
                }
                let scope: RunScope = context.require().map_err(|error| {
                    ToolExecutionError::other(format!(
                        "deferred MCP tool dispatch is missing its run scope: {error}"
                    ))
                })?;
                let guard_scope =
                    GuardScope::new(scope.task_id.to_string(), scope.intent_id.to_string());
                match plan
                    .execute(&guard_scope, &arguments, invoker.as_ref())
                    .await
                {
                    Ok(ToolCallOutcome::Allowed { output }) => Ok(ToolOutput::json(output)),
                    Ok(ToolCallOutcome::Denied { reason }) => {
                        Err(ToolExecutionError::permission_denied(reason))
                    }
                    Ok(ToolCallOutcome::Pending { approval_id }) => {
                        Err(ToolExecutionError::other(format!(
                            "MCP tool call is parked behind approval {approval_id}; \
                         approvals block inside the guarded call"
                        )))
                    }
                    Ok(ToolCallOutcome::TransportError { message }) => Err(
                        ToolExecutionError::other(format!("MCP transport error: {message}")),
                    ),
                    Err(error) => Err(ToolExecutionError::invalid_args(error.to_string())),
                }
            })
        },
    );

    handle.add_dynamic_tool(list_tool);
    handle.add_dynamic_tool(call_tool);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use dhi_core::ToolCallOutcome as CoreOutcome;
    use rig::tool::server::ToolServer;

    use super::*;
    use crate::config::guarded_name;

    fn snapshot(server: &str, tool: &str) -> RegistryToolSnapshot {
        RegistryToolSnapshot {
            guarded_name: guarded_name(server, tool).expect("a guardable test name"),
            mcp_name: tool.to_string(),
            owner: server.to_string(),
            description: format!("{tool} on {server}"),
            schema: json!({"type": "object", "properties": {}}),
        }
    }

    fn snapshot_many(server: &str, tools: &[&str]) -> Vec<RegistryToolSnapshot> {
        tools.iter().map(|tool| snapshot(server, tool)).collect()
    }

    fn config(servers: Value) -> McpVisibilityConfig {
        let file: dhi_mcp::McpFile = serde_json::from_value(json!({ "mcpServers": servers }))
            .expect("the extension config parses");
        McpVisibilityConfig::from_file(&file)
    }

    fn direct_names(plan: &RolePlan) -> Vec<String> {
        let mut names: Vec<String> = plan
            .direct
            .iter()
            .map(|tool| tool.guarded_name.clone())
            .collect();
        names.sort();
        names
    }

    fn guardable(plan: &RolePlan) -> Vec<String> {
        plan.direct.iter().map(|tool| tool.tool.clone()).collect()
    }

    #[test]
    fn worker_reporter_both_none_and_specialist_visibility() {
        let snapshot = [
            snapshot("worker-only", "work"),
            snapshot("reporter-only", "report"),
            snapshot("shared", "ping"),
            snapshot("nobody", "secret"),
            snapshot("research", "search"),
        ];
        let config = config(json!({
            "worker-only": { "command": "npx", "visible_to": ["worker"] },
            "reporter-only": { "command": "npx", "visible_to": ["reporter"] },
            "shared": { "command": "npx", "visible_to": ["worker", "reporter"] },
            "nobody": { "command": "npx", "visible_to": [] },
            "research": { "command": "npx", "visible_to": ["specialist:researcher"] }
        }));
        let planner = McpRolePlanner::new(&config, &snapshot);

        assert_eq!(
            direct_names(&planner.plan("worker")),
            ["mcp__shared__ping", "mcp__worker-only__work"]
        );
        assert_eq!(
            direct_names(&planner.plan("reporter")),
            ["mcp__reporter-only__report", "mcp__shared__ping"]
        );
        assert!(planner.plan("planner").direct.is_empty());
        assert!(planner.plan("digest").direct.is_empty());
        assert_eq!(
            direct_names(&planner.plan("specialist:researcher")),
            ["mcp__research__search"]
        );
        assert!(
            planner
                .plan("specialist:legal")
                .direct
                .iter()
                .all(|tool| tool.server != "research")
        );
        for role in [
            "planner",
            "worker",
            "reporter",
            "digest",
            "specialist:researcher",
        ] {
            assert!(
                planner
                    .plan(role)
                    .direct
                    .iter()
                    .all(|tool| tool.server != "nobody"),
                "{role} must not see the explicit empty visibility server"
            );
        }
    }

    #[test]
    fn no_config_visibility_stays_visible_to_every_existing_role() {
        let snapshot = [snapshot("plain", "fetch")];
        let planner = McpRolePlanner::new(&McpVisibilityConfig::default(), &snapshot);
        for role in [
            "planner",
            "worker",
            "reporter",
            "digest",
            "specialist:any-name",
        ] {
            assert_eq!(
                direct_names(&planner.plan(role)),
                ["mcp__plain__fetch"],
                "{role}"
            );
        }
    }

    #[test]
    fn unknown_role_sees_nothing() {
        let snapshot = [snapshot("plain", "fetch")];
        let config = config(json!({
            "plain": { "command": "npx", "visible_to": ["intern"] }
        }));
        let planner = McpRolePlanner::new(&config, &snapshot);
        for role in ["", " ", "intern", "specialist:", "specialist:   "] {
            let plan = planner.plan(role);
            assert!(plan.direct.is_empty(), "{role:?}");
            assert!(plan.deferred.is_empty(), "{role:?}");
        }
        assert!(planner.plan("worker").direct.is_empty());
    }

    #[test]
    fn per_tool_override_replaces_the_server_list() {
        let snapshot = [
            snapshot("mixed", "worker-tool"),
            snapshot("mixed", "planner-tool"),
            snapshot("mixed", "hidden-tool"),
        ];
        let config = config(json!({
            "mixed": {
                "command": "npx",
                "visible_to": ["worker"],
                "tool_visibility": {
                    "planner-tool": ["planner"],
                    "hidden-tool": []
                }
            }
        }));
        let planner = McpRolePlanner::new(&config, &snapshot);

        assert_eq!(guardable(&planner.plan("worker")), ["worker-tool"]);
        assert_eq!(guardable(&planner.plan("planner")), ["planner-tool"]);
        assert!(planner.plan("reporter").direct.is_empty());
        assert!(
            planner
                .plan("worker")
                .direct
                .iter()
                .all(|tool| tool.tool != "hidden-tool")
        );
    }

    #[test]
    fn forty_tool_server_defers_at_or_above_threshold_and_stays_direct_below() {
        let tools: Vec<String> = (0..40).map(|index| format!("tool_{index:02}")).collect();
        let tool_refs: Vec<&str> = tools.iter().map(String::as_str).collect();
        let snapshot = snapshot_many("big", &tool_refs);

        let below = config(json!({
            "big": { "command": "npx", "deferred_threshold": 41 }
        }));
        let direct = McpRolePlanner::new(&below, &snapshot).plan("worker");
        assert!(!direct.is_deferred());
        assert_eq!(direct.definitions().len(), 40);
        assert_eq!(direct.definitions()[0].name, "mcp__big__tool_00");

        let at = config(json!({
            "big": { "command": "npx", "deferred_threshold": 40 }
        }));
        let deferred = McpRolePlanner::new(&at, &snapshot).plan("worker");
        assert!(deferred.is_deferred());
        assert_eq!(deferred.definitions().len(), 2);
        assert_eq!(deferred.definitions()[0].name, LIST_TOOLS_NAME);
        assert_eq!(deferred.definitions()[1].name, CALL_TOOL_NAME);
        assert_eq!(deferred.deferred_tool_count(), 40);
        let listing = deferred.list();
        assert_eq!(listing.len(), 1);
        assert_eq!(listing[0].server, "big");
        assert_eq!(listing[0].tools.len(), 40);
        assert_eq!(
            deferred
                .call_tool("big", "tool_17", json!({}))
                .expect("a visible tool resolves")
                .guarded_name,
            "mcp__big__tool_17"
        );
    }

    #[test]
    fn the_default_threshold_is_thirty_two_tools() {
        let thirty_one: Vec<String> = (0..31).map(|index| format!("tool_{index:02}")).collect();
        let thirty_two: Vec<String> = (0..32).map(|index| format!("tool_{index:02}")).collect();
        let ones: Vec<&str> = thirty_one.iter().map(String::as_str).collect();
        let twos: Vec<&str> = thirty_two.iter().map(String::as_str).collect();
        let config = McpVisibilityConfig::default();

        let below = McpRolePlanner::new(&config, &snapshot_many("srv", &ones)).plan("worker");
        assert!(!below.is_deferred());
        assert_eq!(below.direct.len(), 31);

        let at = McpRolePlanner::new(&config, &snapshot_many("srv", &twos)).plan("worker");
        assert!(at.is_deferred());
        assert_eq!(at.deferred_tool_count(), 32);
    }

    #[test]
    fn meta_tool_definitions_match_the_core_definitions() {
        let tools: Vec<String> = (0..40).map(|index| format!("tool_{index:02}")).collect();
        let refs: Vec<&str> = tools.iter().map(String::as_str).collect();
        let planner = McpRolePlanner::new(
            &McpVisibilityConfig::default(),
            &snapshot_many("big", &refs),
        );
        let plan = planner.plan("worker");
        let core = plan.deferred.first().expect("a deferred toolset");
        let role_definitions = plan.definitions();
        let meta_definitions = core.definitions();
        assert_eq!(role_definitions, meta_definitions);
        assert_eq!(
            role_definitions
                .iter()
                .map(|definition| definition.name.as_str())
                .collect::<Vec<_>>(),
            [LIST_TOOLS_NAME, CALL_TOOL_NAME]
        );
    }

    // -- meta-tool flow -----------------------------------------------------

    #[derive(Default)]
    struct FakeSink {
        calls: Mutex<Vec<(uuid::Uuid, uuid::Uuid, ToolCallRequest)>>,
        outcomes: Mutex<VecDeque<anyhow::Result<CoreOutcome>>>,
    }

    impl FakeSink {
        fn with_outcomes(outcomes: Vec<anyhow::Result<CoreOutcome>>) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                outcomes: Mutex::new(outcomes.into()),
            })
        }

        fn calls(&self) -> Vec<(uuid::Uuid, uuid::Uuid, ToolCallRequest)> {
            self.calls.lock().expect("calls lock").clone()
        }
    }

    #[async_trait]
    impl ToolSink for FakeSink {
        async fn execute(
            &self,
            task_id: uuid::Uuid,
            intent_id: uuid::Uuid,
            request: ToolCallRequest,
        ) -> anyhow::Result<CoreOutcome> {
            self.calls
                .lock()
                .expect("calls lock")
                .push((task_id, intent_id, request));
            self.outcomes
                .lock()
                .expect("outcomes lock")
                .pop_front()
                .unwrap_or_else(|| {
                    Ok(CoreOutcome::Allowed {
                        output: json!({"ok": true}),
                    })
                })
        }
    }

    fn forty_tool_planner(threshold: usize) -> SharedMcpPlanner {
        let tools: Vec<String> = (0..40).map(|index| format!("tool_{index:02}")).collect();
        let refs: Vec<&str> = tools.iter().map(String::as_str).collect();
        Arc::new(RwLock::new(McpRolePlanner::new(
            &config(json!({
                "big": {
                    "command": "npx",
                    "visible_to": ["worker"],
                    "deferred_threshold": threshold
                }
            })),
            &snapshot_many("big", &refs),
        )))
    }

    async fn call_tool(
        handle: &ToolServerHandle,
        name: &str,
        input: Value,
        role: Option<&str>,
    ) -> rig::tool::ToolResult {
        let toolset = handle.toolset();
        let tool = toolset.get(name).expect("the meta-tool is registered");
        let mut context = ToolContext::new();
        context
            .insert(RunScope {
                task_id: uuid::Uuid::from_u128(1),
                intent_id: uuid::Uuid::from_u128(2),
            })
            .expect("scope");
        if let Some(role) = role {
            context.insert(RunRole(role.to_string())).expect("role");
        }
        tool.execute(input.to_string(), &mut context).await
    }

    async fn meta_deployment(
        threshold: usize,
        sink: Arc<FakeSink>,
    ) -> (ToolServerHandle, SharedMcpPlanner) {
        let handle = ToolServer::new().run();
        let planner = forty_tool_planner(threshold);
        register_deferred_meta_tools(&handle, Arc::clone(&planner), sink)
            .expect("meta-tools register");
        (handle, planner)
    }

    #[tokio::test]
    async fn mcp_call_tool_maps_to_the_real_guarded_name_and_scope() {
        let sink = FakeSink::with_outcomes(vec![Ok(CoreOutcome::Allowed {
            output: json!({"echo": "tool-07"}),
        })]);
        let (handle, _) = meta_deployment(10, Arc::clone(&sink)).await;

        let result = call_tool(
            &handle,
            CALL_TOOL_NAME,
            json!({"server": "big", "tool": "tool_07", "arguments": {"query": "x"}}),
            Some("worker"),
        )
        .await;
        assert!(result.is_success(), "{result:?}");
        assert_eq!(result.output().as_json(), Some(&json!({"echo": "tool-07"})));

        let calls = sink.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, uuid::Uuid::from_u128(1));
        assert_eq!(calls[0].1, uuid::Uuid::from_u128(2));
        assert_eq!(
            calls[0].2.tool, "mcp__big__tool_07",
            "the guard/audit sees the real tool, never mcp_call_tool"
        );
        assert_eq!(calls[0].2.input["query"], "x");
    }

    #[tokio::test]
    async fn a_denied_real_tool_is_a_refusal_and_never_executes_again() {
        let sink = FakeSink::with_outcomes(vec![Ok(CoreOutcome::Denied {
            reason: "rule deny-mcp: nope".to_string(),
        })]);
        let (handle, _) = meta_deployment(10, Arc::clone(&sink)).await;

        let result = call_tool(
            &handle,
            CALL_TOOL_NAME,
            json!({"server": "big", "tool": "tool_03", "arguments": {}}),
            Some("worker"),
        )
        .await;
        assert!(
            result.is_error_kind(rig::tool::ToolErrorKind::PermissionDenied),
            "{result:?}"
        );
        let text = result.error().map(ToString::to_string).unwrap_or_default();
        assert!(text.contains("deny-mcp"), "{text}");
        assert_eq!(
            sink.calls().len(),
            1,
            "the guard decides, not the meta-tool"
        );
    }

    #[tokio::test]
    async fn a_pending_real_tool_is_not_collapsed_into_a_success_or_second_call() {
        let sink = FakeSink::with_outcomes(vec![Ok(CoreOutcome::Pending {
            approval_id: uuid::Uuid::from_u128(9),
        })]);
        let (handle, _) = meta_deployment(10, Arc::clone(&sink)).await;

        let result = call_tool(
            &handle,
            CALL_TOOL_NAME,
            json!({"server": "big", "tool": "tool_04", "arguments": {}}),
            Some("worker"),
        )
        .await;
        assert!(!result.is_success(), "{result:?}");
        let text = result.error().map(ToString::to_string).unwrap_or_default();
        assert!(text.contains("approval") && text.contains("9"), "{text}");
        assert_eq!(
            sink.calls().len(),
            1,
            "a pending approval never becomes an immediate retry"
        );
    }

    #[tokio::test]
    async fn a_transport_error_is_distinct_from_a_policy_denial() {
        let sink = FakeSink::with_outcomes(vec![Err(anyhow!("connection reset by peer"))]);
        let (handle, _) = meta_deployment(10, Arc::clone(&sink)).await;

        let result = call_tool(
            &handle,
            CALL_TOOL_NAME,
            json!({"server": "big", "tool": "tool_05", "arguments": {}}),
            Some("worker"),
        )
        .await;
        assert!(!result.is_success());
        let text = result.error().map(ToString::to_string).unwrap_or_default();
        assert!(text.contains("transport error"), "{text}");
        assert!(text.contains("connection reset"), "{text}");
        assert_eq!(sink.calls().len(), 1);
    }

    #[tokio::test]
    async fn unknown_hidden_and_oversized_calls_fail_closed_without_reaching_the_guard() {
        let tools: Vec<String> = (0..10).map(|index| format!("tool_{index}")).collect();
        let refs: Vec<&str> = tools.iter().map(String::as_str).collect();
        let config = config(json!({
            "big": {
                "command": "npx",
                "deferred_threshold": 5,
                "tool_visibility": { "tool_4": ["planner"] }
            }
        }));
        let planner = Arc::new(RwLock::new(McpRolePlanner::new(
            &config,
            &snapshot_many("big", &refs),
        )));
        let sink = FakeSink::with_outcomes(Vec::new());
        let sink_trait: Arc<dyn ToolSink> = sink.clone();
        let handle = ToolServer::new().run();
        register_deferred_meta_tools(&handle, planner, sink_trait).expect("meta register");

        for (server, tool, label) in [
            ("missing", "tool_0", "unknown server"),
            ("big", "missing", "unknown tool"),
            ("big", "tool_4", "hidden tool"),
        ] {
            let result = call_tool(
                &handle,
                CALL_TOOL_NAME,
                json!({"server": server, "tool": tool, "arguments": {}}),
                Some("worker"),
            )
            .await;
            assert!(!result.is_success(), "{label}: {result:?}");
            let text = result.error().map(ToString::to_string).unwrap_or_default();
            assert!(text.contains("unknown MCP"), "{label}: {text}");
        }

        let oversized = json!({
            "server": "big",
            "tool": "tool_0",
            "arguments": { "blob": "x".repeat(70 * 1024) }
        });
        let result = call_tool(&handle, CALL_TOOL_NAME, oversized, Some("worker")).await;
        assert!(!result.is_success());
        let text = result.error().map(ToString::to_string).unwrap_or_default();
        assert!(text.contains("exceeding the 65536-byte limit"), "{text}");
        assert!(sink.calls().is_empty(), "nothing reached the guard");
    }

    #[tokio::test]
    async fn a_dispatch_without_a_role_cannot_use_the_meta_tools() {
        let sink = FakeSink::with_outcomes(Vec::new());
        let (handle, _) = meta_deployment(10, Arc::clone(&sink)).await;
        let result = call_tool(
            &handle,
            CALL_TOOL_NAME,
            json!({"server": "big", "tool": "tool_00", "arguments": {}}),
            None,
        )
        .await;
        assert!(!result.is_success());
        let text = result.error().map(ToString::to_string).unwrap_or_default();
        assert!(text.contains("run role"), "{text}");
        assert!(sink.calls().is_empty());
    }

    #[tokio::test]
    async fn mcp_list_tools_reports_only_the_role_visible_set() {
        let sink = FakeSink::with_outcomes(Vec::new());
        let (handle, _) = meta_deployment(10, Arc::clone(&sink)).await;

        let result = call_tool(&handle, LIST_TOOLS_NAME, json!({}), Some("worker")).await;
        assert!(result.is_success(), "{result:?}");
        let output = result.output().as_json().expect("structured output");
        assert_eq!(output["servers"][0]["server"], "big");
        assert_eq!(
            output["servers"][0]["tools"]
                .as_array()
                .expect("tools")
                .len(),
            40
        );

        let reporter = call_tool(&handle, LIST_TOOLS_NAME, json!({}), Some("reporter")).await;
        assert!(
            !reporter.is_success(),
            "a server visible only to the worker is not listed for the reporter"
        );
    }

    // -- worker/specialist views -------------------------------------------

    #[tokio::test]
    async fn the_worker_view_hides_mcp_tools_but_keeps_every_other_source() {
        let handle = ToolServer::new().run();
        let sink: Arc<dyn ToolSink> = FakeSink::with_outcomes(Vec::new());
        for (server, tool) in [("worker-only", "work"), ("reporter-only", "report")] {
            dhi_agent::register_guarded_tool(
                &handle,
                dhi_agent::GuardedToolSpec::new(
                    &guarded_name(server, tool).expect("guardable"),
                    "test tool",
                    json!({"type": "object"}),
                ),
                Arc::clone(&sink),
            )
            .expect("register");
        }
        dhi_agent::register_guarded_tool(
            &handle,
            dhi_agent::GuardedToolSpec::new(
                "record_fact",
                "a non-MCP tool",
                json!({"type": "object"}),
            ),
            Arc::clone(&sink),
        )
        .expect("register");

        let snapshot = [
            snapshot("worker-only", "work"),
            snapshot("reporter-only", "report"),
        ];
        let config = config(json!({
            "worker-only": { "command": "npx", "visible_to": ["worker"] },
            "reporter-only": { "command": "npx", "visible_to": ["reporter"] }
        }));
        let planner = Arc::new(RwLock::new(McpRolePlanner::new(&config, &snapshot)));
        let view = McpWorkerToolView::new(&handle, planner);
        let mut names = view.visible_names();
        names.sort();
        assert_eq!(names, ["mcp__worker-only__work", "record_fact"]);
    }

    #[tokio::test]
    async fn a_deferred_worker_view_exposes_only_the_meta_pair_plus_non_mcp_tools() {
        let handle = ToolServer::new().run();
        let sink: Arc<dyn ToolSink> = FakeSink::with_outcomes(Vec::new());
        let tools: Vec<String> = (0..40).map(|index| format!("tool_{index:02}")).collect();
        for tool in &tools {
            dhi_agent::register_guarded_tool(
                &handle,
                dhi_agent::GuardedToolSpec::new(
                    &guarded_name("big", tool).expect("guardable"),
                    "test tool",
                    json!({"type": "object"}),
                ),
                Arc::clone(&sink),
            )
            .expect("register");
        }
        dhi_agent::register_guarded_tool(
            &handle,
            dhi_agent::GuardedToolSpec::new("record_fact", "a non-MCP tool", json!({})),
            Arc::clone(&sink),
        )
        .expect("register");

        let refs: Vec<&str> = tools.iter().map(String::as_str).collect();
        let planner = Arc::new(RwLock::new(McpRolePlanner::new(
            &McpVisibilityConfig::default(),
            &snapshot_many("big", &refs),
        )));
        register_deferred_meta_tools(&handle, Arc::clone(&planner), sink).expect("register");
        let view = McpWorkerToolView::new(&handle, planner);
        let mut names = view.visible_names();
        names.sort();
        assert_eq!(names, ["mcp_call_tool", "mcp_list_tools", "record_fact"]);
    }

    #[tokio::test]
    async fn worker_view_reads_the_live_planner_after_replacement() {
        let handle = ToolServer::new().run();
        let sink: Arc<dyn ToolSink> = FakeSink::with_outcomes(Vec::new());
        for (server, tool) in [("alpha", "one"), ("beta", "two")] {
            dhi_agent::register_guarded_tool(
                &handle,
                dhi_agent::GuardedToolSpec::new(
                    &guarded_name(server, tool).expect("guardable"),
                    "test tool",
                    json!({"type": "object"}),
                ),
                Arc::clone(&sink),
            )
            .expect("register");
        }

        let first = Arc::new(RwLock::new(McpRolePlanner::new(
            &config(json!({"alpha": {"command": "npx"}})),
            &[snapshot("alpha", "one")],
        )));
        let view = McpWorkerToolView::new(&handle, Arc::clone(&first));
        assert_eq!(view.visible_names(), ["mcp__alpha__one"]);

        *first.write().expect("planner lock") = McpRolePlanner::new(
            &config(json!({
                "alpha": {"command": "npx"},
                "beta": {"command": "npx"}
            })),
            &[snapshot("alpha", "one"), snapshot("beta", "two")],
        );

        let mut names = view.visible_names();
        names.sort();
        assert_eq!(names, ["mcp__alpha__one", "mcp__beta__two"]);
    }

    #[test]
    fn specialist_tools_are_filtered_without_widening_the_allow_list() {
        let tools: Vec<String> = (0..10).map(|index| format!("tool_{index}")).collect();
        let refs: Vec<&str> = tools.iter().map(String::as_str).collect();
        let config = config(json!({
            "research": {
                "command": "npx",
                "visible_to": ["specialist:researcher"],
                "deferred_threshold": 5
            }
        }));
        let planner = McpRolePlanner::new(&config, &snapshot_many("research", &refs));

        let mut researcher = vec![
            "mcp__research__tool_0".to_string(),
            "mcp__research__tool_1".to_string(),
            "record_fact".to_string(),
        ];
        let removed = planner.filter_specialist_tools("researcher", &mut researcher);
        assert_eq!(removed, 2, "both direct MCP names are replaced");
        assert_eq!(
            researcher,
            ["record_fact", "mcp_list_tools", "mcp_call_tool"]
        );

        let mut legal = vec![
            "mcp__research__tool_0".to_string(),
            "record_fact".to_string(),
        ];
        planner.filter_specialist_tools("legal", &mut legal);
        assert_eq!(legal, ["record_fact"]);

        let mut unrelated = vec!["web_search".to_string()];
        assert_eq!(
            planner.filter_specialist_tools("researcher", &mut unrelated),
            0
        );
        assert_eq!(unrelated, ["web_search"]);

        let mut worker = vec!["some_worker_tool".to_string()];
        assert_eq!(
            planner.filter_specialist_tools("researcher", &mut worker),
            0
        );
        assert_eq!(worker, ["some_worker_tool"]);
    }

    #[test]
    fn a_mixed_specialist_keeps_direct_tools_and_defers_only_the_big_server() {
        let direct_tools: Vec<String> = (0..3).map(|index| format!("tool_{index}")).collect();
        let big_tools: Vec<String> = (0..10).map(|index| format!("tool_{index}")).collect();
        let direct_refs: Vec<&str> = direct_tools.iter().map(String::as_str).collect();
        let big_refs: Vec<&str> = big_tools.iter().map(String::as_str).collect();
        let mut snapshot = snapshot_many("direct", &direct_refs);
        snapshot.extend(snapshot_many("big", &big_refs));
        let config = config(json!({
            "direct": {
                "command": "npx",
                "visible_to": ["specialist:researcher"],
                "deferred_threshold": 10
            },
            "big": {
                "command": "npx",
                "visible_to": ["specialist:researcher"],
                "deferred_threshold": 5
            }
        }));
        let planner = McpRolePlanner::new(&config, &snapshot);

        let mut specialist = vec![
            "mcp__direct__tool_0".to_string(),
            "mcp__big__tool_0".to_string(),
            "record_fact".to_string(),
        ];
        let removed = planner.filter_specialist_tools("researcher", &mut specialist);
        assert_eq!(removed, 1, "only the deferred server's tool is replaced");
        assert_eq!(
            specialist,
            [
                "mcp__direct__tool_0",
                "record_fact",
                "mcp_list_tools",
                "mcp_call_tool"
            ],
            "the below-threshold server stays direct"
        );

        let mut direct_only = vec!["mcp__direct__tool_1".to_string(), "record_fact".to_string()];
        assert_eq!(
            planner.filter_specialist_tools("researcher", &mut direct_only),
            0
        );
        assert_eq!(
            direct_only,
            ["mcp__direct__tool_1", "record_fact"],
            "a spec that never referenced the deferred server gains no meta-tools"
        );
    }

    #[test]
    fn a_direct_specialist_keeps_only_visible_mcp_tools() {
        let tools: Vec<String> = (0..10).map(|index| format!("tool_{index}")).collect();
        let refs: Vec<&str> = tools.iter().map(String::as_str).collect();
        let config = config(json!({
            "shared": {
                "command": "npx",
                "visible_to": ["specialist:researcher"],
                "deferred_threshold": 100,
                "tool_visibility": { "tool_1": [] }
            }
        }));
        let planner = McpRolePlanner::new(&config, &snapshot_many("shared", &refs));
        let mut specialist = vec![
            "mcp__shared__tool_0".to_string(),
            "mcp__shared__tool_1".to_string(),
            "record_fact".to_string(),
        ];
        planner.filter_specialist_tools("researcher", &mut specialist);
        assert_eq!(specialist, ["mcp__shared__tool_0", "record_fact"]);
    }
}
