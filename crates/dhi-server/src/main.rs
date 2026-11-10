use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use dhi_agent::{
    AgentRuntime, GuardedToolSpec, LlmPool, MockAgentRuntime, PromptSet, RigAgentRuntime, ToolSink,
};
use dhi_core::team::TeamStore;
use dhi_core::{GraphStore, GuardStore, ScaleApprovalGate};
use dhi_engine::Engine;
use dhi_exec::{CodingTools, ContainerConfig, TaskContainers};
use dhi_guard::{GuardedExecutor, SharedPolicy, ToolBackend};
use dhi_pty::PtyTools;
use dhi_server::{
    AppState, GuardedToolSink, SUPERVISOR_INTERVAL, TaskRunRegistry, build_guard_policy,
    build_pool_snapshot, recover_tasks_with_registry, router_with_web_ui,
    run_supervisor_with_scale,
};
use dhi_store::Store;
use dhi_store::mgmt::{BootstrapSeed, DEFAULT_APPROVAL_TIMEOUT_SECS};
use rig::tool::server::ToolServerHandle;
use serde_json::json;
use tracing::{info, warn};

/// Default approval timeout. It seeds
/// `app_settings.approval_timeout_secs` on first boot.
const APPROVAL_TTL_MINUTES: i64 = 15;

/// Engine safety ceilings. They bound a runaway loop; they are not work
/// targets. The server sets both on every engine it builds.
const DEFAULT_ENGINE_MAX_ROUNDS: usize = 64;
const DEFAULT_ENGINE_NO_PROGRESS_ROUNDS: usize = 16;

/// The first-boot seed material from the same environment variables
/// `dhi_agent::LlmProfile::from_env` reads. `None` keeps mock-mode boots
/// profile-free: env is only ever the initial fallback.
fn bootstrap_seed() -> Option<BootstrapSeed> {
    let profiles = dhi_agent::LlmProfile::from_env().ok()?;
    Some(BootstrapSeed {
        base_url: profiles.planner.base_url,
        api_key: profiles.planner.api_key,
        planner_model: profiles.planner.model,
        worker_model: profiles.worker.model,
        planner_thinking_type: profiles.planner.thinking_type,
        planner_reasoning_effort: profiles.planner.reasoning_effort,
        worker_thinking_type: profiles.worker.thinking_type,
        worker_reasoning_effort: profiles.worker.reasoning_effort,
        approval_timeout_secs: DEFAULT_APPROVAL_TIMEOUT_SECS,
    })
}

/// Loads the prompt set (`DHI_PROMPTS_DIR`, else `ops/prompts` from here or
/// an ancestor) and builds the Rig runtime on the shared pool. Any failure is
/// the caller's cue to fall back to the mock runtime.
fn build_runtime(pool: Arc<LlmPool>) -> anyhow::Result<RigAgentRuntime> {
    let prompts = PromptSet::discover(Path::new(env!("CARGO_MANIFEST_DIR")))
        .map_err(|err| anyhow::anyhow!("prompts: {err}"))?;
    Ok(RigAgentRuntime::with_pool(pool, prompts))
}

/// Loads `ops/specialists.json` (next to the prompts, same discovery) and
/// attaches the specialist fleet plus the store-backed audit sink.
///
/// Degradations are logged, never fatal: an invalid config boots without
/// delegation, and a specialist whose allowed tools did not register is
/// skipped individually while the rest attach. When MCP visibility is
/// configured, each specialist's MCP tool list is filtered to
/// `specialist:<name>` first; non-MCP tools and specialists that listed no MCP
/// tool are left exactly as configured.
fn attach_specialists(
    mut runtime: RigAgentRuntime,
    store: Arc<dyn GraphStore>,
    visibility: dhi_server::mcp::SharedMcpPlanner,
) -> RigAgentRuntime {
    let mut specs = match dhi_agent::discover_specialists(Path::new(env!("CARGO_MANIFEST_DIR"))) {
        Ok(Some(specs)) => specs,
        Ok(None) => return runtime,
        Err(err) => {
            warn!(error = %err, "specialists config invalid; delegation disabled");
            return runtime;
        }
    };
    let visibility = visibility
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for spec in &mut specs {
        let removed = visibility.filter_specialist_tools(&spec.name, &mut spec.tools);
        if removed > 0 {
            info!(
                specialist = %spec.name,
                removed,
                "specialist MCP tools filtered by role visibility"
            );
        }
    }
    let (attachable, skipped) =
        dhi_agent::partition_by_registered_tools(specs, runtime.tool_server());
    for (name, missing) in &skipped {
        warn!(
            specialist = %name,
            missing = ?missing,
            "specialist skipped: its allowed tools are not registered"
        );
    }
    if attachable.is_empty() {
        info!("no attachable specialists; the worker manages without delegation");
        return runtime;
    }
    let sink = Arc::new(dhi_server::SpecialistAuditSink::new(store));
    if let Err(err) = runtime.with_specialists(attachable, Some(sink)) {
        warn!(error = %err, "specialists failed to attach; delegation disabled");
        return runtime;
    }
    let names = runtime
        .specialists()
        .map(|fleet| fleet.names())
        .unwrap_or_default();
    info!(specialists = ?names, "specialists attached");
    runtime
}

/// Whether the Rig runtime may boot: an env-configured key historically
/// enables it, and — since the store became the declared source of truth —
/// so does ANY profile already in PostgreSQL (the failover pool is built
/// from it before this decision runs, see `main`). Requiring the env key on
/// every restart wrongly forced mock mode onto database-configured installs.
fn select_runtime(has_env_key: bool, pool_len: usize) -> bool {
    has_env_key || pool_len > 0
}

/// The env-var half of [`select_runtime`]: either well-known key spelling
/// counts, mirroring `dhi_agent::LlmProfile::from_env`.
fn env_has_llm_key() -> bool {
    std::env::var("DHI_LLM_API_KEY").is_ok_and(|k| !k.trim().is_empty())
        || std::env::var("RIVER_API_KEY").is_ok_and(|k| !k.trim().is_empty())
}

/// Routes a guarded tool call to the coding tools first, then to the existing
/// web, MCP, and sources backend.
///
/// The six coding names (`read`, `write`, `edit`, `search`, `list`, `shell`)
/// are checked before delegation. Web names, MCP names (`mcp__...`), and
/// terminal names are not coding tools, so they fall through to the wrapped
/// backend exactly as before.
struct CodingRoutingBackend {
    coding: Arc<dhi_exec::CodingBackend>,
    delegate: Arc<dyn ToolBackend>,
}

impl CodingRoutingBackend {
    fn new(coding: Arc<dhi_exec::CodingBackend>, delegate: Arc<dyn ToolBackend>) -> Self {
        Self { coding, delegate }
    }
}

#[async_trait]
impl ToolBackend for CodingRoutingBackend {
    async fn execute(
        &self,
        tool: &str,
        input: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        if dhi_exec::CodingBackend::is_coding_tool(tool) {
            self.coding.execute(tool, input).await
        } else {
            self.delegate.execute(tool, input).await
        }
    }
}

/// Routes a guarded call to the persistent terminal tools first, then to the
/// coding, web, MCP, and sources backends.
///
/// The five terminal names (`shell_open`, `shell_send`, `shell_read`,
/// `shell_close`, `shell_list`) are checked before delegation, so a session
/// always reaches the PTY backend that owns its scope secret. Every other
/// name falls through unchanged.
struct PtyRoutingBackend {
    pty: Arc<dhi_pty::PtyBackend>,
    delegate: Arc<dyn ToolBackend>,
}

impl PtyRoutingBackend {
    fn new(pty: Arc<dhi_pty::PtyBackend>, delegate: Arc<dyn ToolBackend>) -> Self {
        Self { pty, delegate }
    }
}

#[async_trait]
impl ToolBackend for PtyRoutingBackend {
    async fn execute(
        &self,
        tool: &str,
        input: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        if dhi_pty::PtyBackend::is_pty_tool(tool) {
            self.pty.execute(tool, input).await
        } else {
            self.delegate.execute(tool, input).await
        }
    }
}

/// Routes the stable web names to the one [`dhi_web::WebRouter`], then hands
/// every other call to the MCP/sources chain unchanged.
///
/// `web_search`, `web_read`, and `web_research` are the web tools.
/// `web_parse` remains as a compatibility alias for `web_read`: the router
/// detects PDF/DOCX targets inside its read path, so both names share one
/// implementation. A missing router (configuration failed at boot) fails the
/// web names closed; the server still serves every other tool.
struct WebRoutingBackend {
    runtime: Arc<dhi_server::web_providers::WebProviderRuntime>,
    delegate: Arc<dyn ToolBackend>,
}

impl WebRoutingBackend {
    fn new(
        runtime: Arc<dhi_server::web_providers::WebProviderRuntime>,
        delegate: Arc<dyn ToolBackend>,
    ) -> Self {
        Self { runtime, delegate }
    }

    fn router(&self, tool: &str) -> anyhow::Result<Arc<dhi_web::WebRouter>> {
        self.runtime.router().ok_or_else(|| {
            anyhow::anyhow!(
                "no tool backend configured for {tool} (the web provider router is unavailable)"
            )
        })
    }
}

fn is_web_tool(tool: &str) -> bool {
    matches!(
        tool,
        "web_search" | "web_read" | "web_research" | "web_parse"
    )
}

#[async_trait]
impl ToolBackend for WebRoutingBackend {
    async fn execute(
        &self,
        tool: &str,
        input: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        if !is_web_tool(tool) {
            return self.delegate.execute(tool, input).await;
        }
        match tool {
            "web_search" => {
                let request = dhi_web::parse_search_input(input).map_err(web_error)?;
                let results = self
                    .router(tool)?
                    .search(request)
                    .await
                    .map_err(web_error)?;
                serialize_web(tool, results)
            }
            // `web_parse` is a compatibility alias: documents are detected by
            // the router's read path, so both names call `read`.
            "web_read" | "web_parse" => {
                let request = dhi_web::parse_read_input(input).map_err(web_error)?;
                let result = self.router(tool)?.read(request).await.map_err(web_error)?;
                serialize_web(tool, result)
            }
            "web_research" => {
                let request = dhi_web::parse_research_input(input).map_err(web_error)?;
                let result = self
                    .router(tool)?
                    .research(request)
                    .await
                    .map_err(web_error)?;
                serialize_web(tool, result)
            }
            _ => unreachable!("is_web_tool checked the name"),
        }
    }
}

/// Converts a provider-neutral error to the guard's `anyhow` shape without
/// losing the provider, HTTP status, or model-readable message.
fn web_error(error: dhi_web::WebError) -> anyhow::Error {
    let mut context = Vec::new();
    if let Some(provider) = error.provider.as_deref() {
        context.push(provider.to_string());
    }
    if let Some(status) = error.status {
        context.push(format!("HTTP {status}"));
    }
    if context.is_empty() {
        anyhow::anyhow!("{}", error.message())
    } else {
        anyhow::anyhow!("{}: {}", context.join(" "), error.message())
    }
}

fn serialize_web<T: serde::Serialize>(tool: &str, value: T) -> anyhow::Result<serde_json::Value> {
    serde_json::to_value(value)
        .map_err(|err| anyhow::anyhow!("{tool}: could not encode the result: {err}"))
}

/// The compatibility `web_parse` schema: the old `{url}` shape, backed by
/// the router's read path.
fn web_parse_spec() -> GuardedToolSpec {
    GuardedToolSpec::new(
        "web_parse",
        "Compatibility alias for web_read: parse a public PDF or DOCX document \
         URL and return its Markdown (capped, with truncation stated). \
         Documents are detected by the provider router; use web_read for \
         ordinary HTML pages.",
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "A public http(s) PDF or DOCX URL."
                }
            },
            "required": ["url"],
            "additionalProperties": false
        }),
    )
}

/// The four model-facing web specs: the three stable `dhi-web` tools plus the
/// compatibility alias.
fn web_tool_specs() -> Vec<GuardedToolSpec> {
    let mut specs = dhi_web::web_tool_specs()
        .into_iter()
        .map(|spec| GuardedToolSpec::new(&spec.name, &spec.description, spec.parameters))
        .collect::<Vec<_>>();
    specs.push(web_parse_spec());
    specs
}

/// The names [`register_web_tools`] registered, for tests and logs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WebRegistration {
    registered: Vec<String>,
}

/// Registers the web tools on the worker's shared registry as guarded tools.
///
/// A missing router means the web tools were disabled at boot: nothing is
/// registered and the reason was already logged. `web_parse` is skipped when
/// another source already registered that name, so the existing tool is never
/// replaced.
async fn register_web_tools(
    handle: &ToolServerHandle,
    sink: Arc<dyn ToolSink>,
    router: Option<Arc<dhi_web::WebRouter>>,
) -> anyhow::Result<WebRegistration> {
    let Some(router) = router else {
        info!("web tools not registered: the web provider router is unavailable");
        return Ok(WebRegistration {
            registered: Vec::new(),
        });
    };
    let existing = handle
        .tool_defs(None)
        .await?
        .iter()
        .map(|definition| definition.name.as_str().to_string())
        .collect::<HashSet<_>>();
    let mut registered = Vec::new();
    for spec in web_tool_specs() {
        if spec.name == "web_parse" && existing.contains(&spec.name) {
            info!("compatibility web_parse not registered: another source already owns that name");
            continue;
        }
        let name = spec.name.clone();
        dhi_agent::register_guarded_tool(handle, spec, sink.clone())?;
        registered.push(name);
    }
    info!(
        tools = ?registered,
        providers = ?router.provider_order(),
        research_providers = ?router.research_provider_order(),
        "web provider tools registered behind the guard"
    );
    Ok(WebRegistration { registered })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "dhi=info,tower_http=info".into()),
        )
        .init();

    // Validate the project folder list before touching the database: an
    // invalid DHI_PROJECT_FOLDERS fails startup with the offending entry.
    let project_folders = Arc::new(dhi_server::project_folders::ProjectFolders::from_env()?);
    info!(
        folders = ?project_folders.paths(),
        "project folders configured"
    );
    // The host file browser is opt-in. A configured root must already exist or
    // be creatable, be a readable directory, and must not be a symlink. It is
    // the only source of create-time project folders and AGENTS.md paths.
    let file_browser = Arc::new(dhi_server::file_browser::FileBrowser::from_env()?);
    info!(
        root = ?file_browser.root(),
        enabled = file_browser.enabled(),
        "host file browser configured"
    );
    // Browser-folder web projects are opt-in. A configured root must already
    // exist or be creatable, be writable, and must not be a symlink. The same
    // absolute path must be visible to this process and the Docker daemon:
    // each web project's folder is passed to the existing project mount
    // unchanged.
    let web_projects = Arc::new(dhi_server::web_projects::WebProjects::from_env()?);
    info!(
        root = ?web_projects.root(),
        enabled = web_projects.enabled(),
        "web project storage configured"
    );

    let app_dsn = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://dhi_app:dhi_app@127.0.0.1:5432/dhi".to_string());
    let migration_dsn = std::env::var("MIGRATION_DATABASE_URL").unwrap_or_else(|_| app_dsn.clone());

    let migration_store = Store::connect(&migration_dsn).await?;
    migration_store.migrate().await?;
    // First-boot seed: profiles from the env, the settings row, and the
    // compiled policy's rules as guard_rules rows. Idempotent — a later boot
    // with a populated database changes nothing, so PostgreSQL becomes the
    // source of truth after the first run.
    match bootstrap_seed() {
        Some(seed) => {
            migration_store.seed_defaults(&seed).await?;
            info!("management tables migrated and (first boot) seeded");
        }
        None => {
            warn!(
                "no LLM configuration in the environment; seeding guard rules only \
                 (llm profiles and app settings stay unset)"
            );
            migration_store
                .seed_defaults(&BootstrapSeed {
                    base_url: String::new(),
                    api_key: String::new(),
                    planner_model: String::new(),
                    worker_model: String::new(),
                    planner_thinking_type: String::new(),
                    planner_reasoning_effort: String::new(),
                    worker_thinking_type: String::new(),
                    worker_reasoning_effort: String::new(),
                    approval_timeout_secs: 900,
                })
                .await?;
        }
    }
    drop(migration_store);

    let store = Arc::new(Store::connect(&app_dsn).await?);
    info!("database ready");

    // The shared failover pool and the shared guard snapshot: built once from
    // the store and swapped by the management routes thereafter.
    let pool = LlmPool::new(build_pool_snapshot(&store).await?);
    let guard = SharedPolicy::new(build_guard_policy(&store).await?);

    // Live-River behavior survives unchanged: with the same env configuration
    // the Rig runtime now resolves its candidate chain through the pool
    // (whose seeded head is the seeded default profile). PostgreSQL is the
    // source of truth: any profile in the pool boots the Rig runtime even
    // without env keys — `seed_defaults` ran above, so a first env-configured
    // boot leaves profiles behind for every later keyless restart.
    let has_env_key = env_has_llm_key();
    let pool_len = pool.load().by_id.len();
    // The one guard: the policy lives in the shared snapshot, evaluated at
    // the tool-call point. The backend serves the configured sources: the
    // web provider router, MCP servers, and the closed sources fallback;
    // everything else fails closed with
    // `no tool backend configured`. Built BEFORE the agent runtime: the same
    // executor doubles as the in-loop `ToolSink`, so guarded tools run
    // through identical semantics whether the model calls them in the loop
    // or (mock mode / engine tests) the engine runs them post-hoc.
    let guard_store: Arc<dyn GuardStore> = store.clone();
    // Web tools: one provider router behind the three stable
    // names. Settings load from `web_provider_settings` first, then
    // DHI_WEB_* plus the existing Firecrawl env/config-file settings.
    // A database failure degrades to env/file; a configuration failure boots
    // without web tools, while the rest of the server serves normally.
    let web_runtime = Arc::new(dhi_server::web_providers::WebProviderRuntime::new(Some(
        store.clone(),
    )));
    match web_runtime.reload().await {
        Ok(providers) => {
            info!(
                providers = ?providers.search_read_order,
                research_providers = ?providers.research_order,
                "web provider router configured; web tools will register"
            );
        }
        Err(err) => {
            warn!(
                error = %err,
                "web provider settings load failed; using environment/file fallback"
            );
            match web_runtime.reload_env_fallback().await {
                Ok(providers) => info!(
                    providers = ?providers.search_read_order,
                    research_providers = ?providers.research_order,
                    "web provider router configured from env/file; web tools will register"
                ),
                Err(err) => {
                    warn!(error = %err, "web provider router unavailable; web tools disabled");
                }
            }
        }
    }
    // MCP servers: config load + validation happen here, degrading
    // with logs; connections and guarded registration happen below, once the
    // worker's shared registry exists. The dispatch half (McpBackend) rides
    // the same registry behind the guard's one backend.
    let mcp = dhi_server::mcp::manager_from_config();
    // Extension boxes: skills and custom tools. Both register through the
    // small `dhi-tool-source` seam and wrap the backend chain, so neither
    // extension edits this server file.
    let skills_runtime = dhi_skills::SkillsRuntime::from_env();
    let custom_tools_runtime = dhi_custom_tools::CustomToolsRuntime::from_env();
    // Coding tools: one Docker container per task, rooted at
    // DHI_WORKSPACE_ROOT (default `workspace`). Containers left by a process
    // that died without shutdown are cleaned up best effort here, before any
    // task runs; a Docker failure only warns and the server still boots.
    let workspace_root =
        std::env::var("DHI_WORKSPACE_ROOT").unwrap_or_else(|_| "workspace".to_string());
    // The server normally runs in its own container and reaches the host
    // Docker daemon through /var/run/docker.sock. The daemon resolves bind
    // mount sources on the host, so it needs the real host path of
    // DHI_WORKSPACE_ROOT: DHI_WORKSPACE_HOST_ROOT. When it is unset the task
    // root is used as before, which is correct only when this process sees
    // the host filesystem directly (local development and tests).
    let workspace_host_root = std::env::var("DHI_WORKSPACE_HOST_ROOT")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let mut task_containers =
        TaskContainers::new(workspace_root.clone(), ContainerConfig::default())?;
    if let Some(host_root) = &workspace_host_root {
        task_containers = task_containers.with_host_root(host_root.clone())?;
    } else {
        warn!(
            workspace_root = %workspace_root,
            "DHI_WORKSPACE_HOST_ROOT is unset; task container bind mounts use the task root \
             as their host path, which is only correct when this process sees the host \
             filesystem directly"
        );
    }
    info!(
        workspace_root = %workspace_root,
        workspace_host_root = %workspace_host_root.as_deref().unwrap_or(workspace_root.as_str()),
        "task container workspace roots configured"
    );
    let task_containers = Arc::new(task_containers);
    if let Err(err) = task_containers.reap_orphans().await {
        warn!(error = %err, "coding container orphan cleanup failed; continuing");
    }
    let coding = Arc::new(CodingTools::new(task_containers.clone()));
    let coding_backend = Arc::new(coding.backend());
    // Persistent terminals: the PTY sessions share the same task
    // containers as the coding tools, so a terminal sees the task workspace
    // and the container is removed only after its terminals are closed.
    let pty = Arc::new(PtyTools::new(task_containers.clone()));
    let pty_backend = Arc::new(pty.backend());
    // The registry owns the hard-kill cleanup handles: a loop that ignores
    // its cooperative stop for five seconds is aborted and its terminals and
    // container are closed under the same task id.
    let task_runs = Arc::new(TaskRunRegistry::with_cleanup(
        pty.clone(),
        task_containers.clone(),
    ));
    // The PTY names route first, then the coding names. The web names route
    // next, then the extension boxes (skills, custom tools), then MCP
    // `mcp__` names; every remaining name reaches the closed sources
    // fallback.
    let mcp_backend = Arc::new(dhi_server::mcp::McpRoutingBackend::new(
        dhi_server::mcp::McpBackend::new(mcp.registry()),
        Arc::new(dhi_server::sources::SourcesBackend::new()),
    ));
    let custom_backend = custom_tools_runtime.backend(mcp_backend);
    let skill_backend = skills_runtime.backend(custom_backend);
    let backend = Arc::new(PtyRoutingBackend::new(
        pty_backend,
        Arc::new(CodingRoutingBackend::new(
            coding_backend,
            Arc::new(WebRoutingBackend::new(web_runtime.clone(), skill_backend)),
        )),
    ));
    let executor = Arc::new(GuardedExecutor::with_policy(
        store.clone(),
        guard.clone(),
        backend,
        chrono::Duration::minutes(APPROVAL_TTL_MINUTES),
    ));
    let tool_sink = Arc::new(GuardedToolSink::with_activity(
        executor.clone(),
        store.clone(),
    ));
    let mut tool_catalog = Arc::new(dhi_tool_catalog::CatalogRuntime::default());
    let mut tool_control: Option<Arc<dyn dhi_tool_control::ToolControl>> = None;

    let agent: Arc<dyn AgentRuntime> = match build_runtime(pool.clone()) {
        Ok(runtime) if select_runtime(has_env_key, pool_len) => {
            info!("using Rig agent runtime (pool-backed)");
            // Sources register onto the worker's shared registry. No native
            // arithmetic tool ships; web and coding tools register below.
            let mut registration = dhi_server::sources::register_sources(runtime.tool_server())?;
            // Register the web tools AFTER the existing sources, on the same
            // shared registry and through the same guarded sink. A failed
            // router leaves this as an info log with no registrations.
            let web = register_web_tools(
                runtime.tool_server(),
                tool_sink.clone(),
                web_runtime.router(),
            )
            .await?;
            registration.registered.extend(web.registered);
            // Coding tools register after the existing sources, on the same
            // shared registry and through the same guarded sink. Every call
            // still takes the one guard path; the wrapper stamps the task id
            // and MAC from the dispatch scope.
            let coding_registration =
                coding.register_tools(runtime.tool_server(), tool_sink.clone())?;
            registration.registered.extend(coding_registration);
            // Persistent terminal tools register last, on the same shared
            // registry and through the same guarded sink. They route to the
            // PTY backend, which checks the task-scoped MAC stamped by their
            // wrapper before touching a session.
            let pty_registration = pty.register_tools(runtime.tool_server(), tool_sink.clone())?;
            registration.registered.extend(pty_registration);
            let extension_sources: Vec<Arc<dyn dhi_tool_source::ToolSource>> =
                vec![skills_runtime.clone(), custom_tools_runtime.clone()];
            for result in dhi_tool_source::ToolSourceRegistry::new(extension_sources).register_all(
                &dhi_tool_source::ToolSourceContext {
                    handle: runtime.tool_server(),
                    sink: tool_sink.clone(),
                },
            ) {
                if let Some(error) = result.error {
                    warn!(
                        source = result.source,
                        error = %error,
                        "tool source registration failed"
                    );
                } else {
                    registration.registered.extend(result.tools);
                }
            }
            info!(tools = ?registration.registered, "tool sources registered");
            // MCP servers connect last so a dead endpoint cannot delay the
            // other registrations; per-server failures are logged and skipped.
            if mcp.configured_servers() > 0 {
                mcp.clone()
                    .connect_all(runtime.tool_server(), tool_sink.clone())
                    .await;
            }
            // Visibility and deferred schemas are planned from the manager's
            // discovered snapshot. The manager still registers every tool on
            // the master registry; role views decide what each agent sees.
            let mcp_config_path = dhi_mcp::config_path();
            let mcp_planner = Arc::new(RwLock::new(
                dhi_server::mcp::McpRolePlanner::from_config_path(
                    mcp_config_path.as_deref(),
                    &mcp.registry().snapshot().await,
                ),
            ));
            mcp.set_role_planner(mcp_planner.clone());
            if dhi_server::mcp::register_deferred_meta_tools(
                runtime.tool_server(),
                mcp_planner.clone(),
                tool_sink.clone(),
            )? {
                info!(
                    "deferred MCP schemas enabled: mcp_list_tools and mcp_call_tool \
                     are registered for roles at or above their server threshold"
                );
            }
            let definitions = runtime
                .tool_server()
                .tool_defs(None)
                .await
                .unwrap_or_default();
            let final_tools = definitions
                .iter()
                .map(|definition| definition.name.as_str().to_string())
                .collect::<Vec<_>>();
            info!(tools = ?final_tools, "final tool list");

            let custom_names: std::collections::BTreeSet<String> =
                custom_tools_runtime.names().into_iter().collect();
            let entries: Vec<dhi_tool_catalog::ToolCatalogEntry> = definitions
                .iter()
                .map(|definition| {
                    let key = definition.name.as_str();
                    let source = if custom_names.contains(key) {
                        dhi_tool_catalog::ToolSource::Custom
                    } else if key.starts_with("mcp__") {
                        dhi_tool_catalog::ToolSource::Mcp
                    } else if key.starts_with("web_") {
                        dhi_tool_catalog::ToolSource::Web
                    } else if key.starts_with("shell_") {
                        dhi_tool_catalog::ToolSource::Pty
                    } else if matches!(key, "read" | "write" | "edit" | "search" | "list") {
                        dhi_tool_catalog::ToolSource::Coding
                    } else if key == "load_skill" {
                        dhi_tool_catalog::ToolSource::Skills
                    } else {
                        dhi_tool_catalog::ToolSource::Builtin
                    };
                    dhi_tool_catalog::ToolCatalogEntry::new(key, source)
                        .with_description(definition.description.clone())
                        .with_input_schema(definition.parameters.clone())
                })
                .collect();
            if let Err(error) = dhi_tool_control::seed_catalog(store.pool(), &entries).await {
                warn!(error = %error, "tool catalog seed failed; control API disabled");
            } else {
                tool_control = Some(Arc::new(dhi_tool_control::PgToolControl::new(
                    store.pool().clone(),
                )));
            }
            tool_catalog = Arc::new(dhi_tool_catalog::CatalogRuntime::new(vec![Arc::new(
                dhi_tool_catalog::StaticProvider::new("runtime", entries),
            )]));
            let mut runtime = runtime.with_tool_sink(tool_sink);
            let no_guarded = mcp_planner
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .all_guarded_names()
                .is_empty();
            if !no_guarded {
                let worker_view = dhi_server::mcp::McpWorkerToolView::new(
                    runtime.tool_server(),
                    mcp_planner.clone(),
                );
                let visible = worker_view.visible_names();
                info!(
                    tools = ?visible,
                    "worker MCP view resolved; hidden MCP schemas are not advertised"
                );
                runtime = runtime.with_worker_tool_view(Arc::new(worker_view));
            }
            Arc::new(attach_specialists(
                runtime,
                store.clone(),
                mcp_planner.clone(),
            ))
        }
        Ok(_) | Err(_) => {
            warn!("Rig agent runtime unavailable; using mock agent runtime");
            Arc::new(MockAgentRuntime)
        }
    };

    info!(
        max_rounds = DEFAULT_ENGINE_MAX_ROUNDS,
        no_progress_rounds = DEFAULT_ENGINE_NO_PROGRESS_ROUNDS,
        "engine round budgets configured"
    );
    let team_store: Arc<dyn TeamStore> = store.clone();
    let scale_gate: Arc<dyn ScaleApprovalGate> = store.clone();
    let web_project_store: Arc<dyn dhi_server::web_projects::WebProjectStore> = store.clone();
    let lead_runtime = agent.name() == "rig";
    let mut engine = Engine::new(store.clone(), agent.clone())
        .with_executor(executor, guard_store.clone())
        .with_max_rounds(DEFAULT_ENGINE_MAX_ROUNDS)
        .with_max_no_progress_rounds(DEFAULT_ENGINE_NO_PROGRESS_ROUNDS)
        .with_scale_gate(scale_gate.clone());
    if lead_runtime {
        // The lead/team runtimes are opt-in: the mock runtime refuses both
        // reviews, so a degraded boot keeps the selector and peer-swarm paths.
        engine = engine
            .with_lead_runtime()
            .with_team_store(team_store.clone());
    }
    let engine = Arc::new(engine);

    // Restart recovery: deny stale approvals, release running intents, and
    // restart every non-terminal task loop. Nothing serves traffic until it
    // succeeds; a recovery failure aborts startup instead of leaving a
    // half-recovered process.
    let graph_store: Arc<dyn GraphStore> = store.clone();
    let restarted =
        recover_tasks_with_registry(&graph_store, &guard_store, &engine, &task_runs).await?;
    info!(
        restarted,
        "restart recovery complete: denied pending approvals, reset running intents, restarted task loops"
    );

    // The approval supervisor: a fixed-interval sweep of every non-terminal
    // task plus the global `swarm_scale` queue, so an overdue approval expires
    // without a human decision.
    tokio::spawn(run_supervisor_with_scale(
        store.clone(),
        engine.clone(),
        Some(store.clone()),
        SUPERVISOR_INTERVAL,
    ));

    // The terminal reaper: once a task is terminal, its persistent terminals
    // are closed and its task container is removed. The host work folder is
    // kept. Failures are logged and retried on the next pass.
    tokio::spawn(run_terminal_reaper(
        store.clone(),
        pty.clone(),
        task_containers.clone(),
    ));

    let state = AppState {
        store: store.clone(),
        guard_store,
        engine,
        agent,
        pool,
        guard,
        mgmt: Some(store),
        mcp,
        tool_catalog,
        tool_control,
        web: web_runtime,
        task_runs,
        project_folders,
        file_browser,
        web_projects,
        web_project_store: Some(web_project_store),
        team_store: Some(team_store),
        scale_gate: Some(scale_gate),
    };

    let bind = std::env::var("DHI_BIND").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    info!("DHI listening on http://{bind}");
    // SIGTERM (`scripts/dhi-service.sh stop`) and Ctrl-C stop the HTTP server
    // first, then close every terminal session and remove every coding
    // container this process started.
    axum::serve(listener, router_with_web_ui(state))
        .with_graceful_shutdown(shutdown_signal(task_containers, pty))
        .await?;
    Ok(())
}

/// How often the terminal reaper checks for tasks that finished.
const TERMINAL_REAPER_INTERVAL: Duration = Duration::from_secs(30);

/// Closes the persistent terminals and removes the task container of every
/// task that reached a terminal state.
///
/// The host work folder is intentionally left in place: it is the task's
/// durable workspace, not a runtime resource. A failure is logged and the
/// next pass retries, because both `close_task` and `remove` keep their state
/// on error.
async fn run_terminal_reaper(
    store: Arc<Store>,
    pty: Arc<PtyTools>,
    containers: Arc<TaskContainers>,
) {
    let mut ticker = tokio::time::interval(TERMINAL_REAPER_INTERVAL);
    // The first tick fires immediately; skip work there and wait a full
    // interval so startup is not delayed by a sweep.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        let tasks = match store.list_tasks(1000).await {
            Ok(tasks) => tasks,
            Err(err) => {
                warn!(error = %err, "terminal reaper: listing tasks failed");
                continue;
            }
        };
        for task in tasks {
            if !task.state.is_terminal() {
                continue;
            }
            if let Err(err) = pty.close_task(task.id).await {
                warn!(
                    task_id = %task.id,
                    error = %err,
                    "terminal reaper: closing terminal sessions failed"
                );
            }
            if let Err(err) = containers.remove(task.id).await {
                warn!(
                    task_id = %task.id,
                    error = %err,
                    "terminal reaper: removing the task container failed"
                );
            }
        }
    }
}

/// Waits for Ctrl-C or SIGTERM, then closes the persistent terminals and
/// shuts the task containers down.
///
/// Terminals close first: their processes run inside the task containers, so
/// removing a container under a live session would leave the session manager
/// with a dead runtime it still has to clean up. `shutdown` is what prevents
/// orphaned containers when the service is stopped normally;
/// [`TaskContainers::reap_orphans`] covers the cases where the process died
/// before this could run.
async fn shutdown_signal(containers: Arc<TaskContainers>, pty: Arc<PtyTools>) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    if let Err(err) = pty.shutdown().await {
        warn!(error = %err, "terminal session shutdown failed");
    }
    if let Err(err) = containers.shutdown().await {
        warn!(error = %err, "coding container shutdown failed");
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::ffi::OsString;
    use std::sync::Mutex;

    use axum::body::Bytes;
    use axum::extract::State;
    use axum::http::{StatusCode, Uri};
    use axum::response::{IntoResponse, Response};
    use dhi_core::{ToolCallOutcome, ToolCallRequest};
    use dhi_web::{ParallelConfig, Secret, WebProvider};
    use rig::tool::server::ToolServer;
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;

    /// The audit's decision matrix: env keys boot Rig as before, and so does
    /// any populated pool — mock mode is only for a genuinely empty setup.
    #[test]
    fn select_runtime_matrix() {
        assert!(select_runtime(true, 0), "env key, empty pool -> Rig");
        assert!(
            select_runtime(false, 2),
            "keyless boot, profiles in PG -> Rig"
        );
        assert!(
            !select_runtime(false, 0),
            "keyless boot, empty pool -> Mock"
        );
        assert!(select_runtime(true, 1), "env key plus one profile -> Rig");
    }

    // -- local provider stub -------------------------------------------------

    #[derive(Clone)]
    struct StubReply {
        status: StatusCode,
        body: Value,
    }

    impl StubReply {
        fn ok(body: Value) -> Self {
            Self {
                status: StatusCode::OK,
                body,
            }
        }

        fn status(status: u16, body: Value) -> Self {
            Self {
                status: StatusCode::from_u16(status).expect("valid status"),
                body,
            }
        }
    }

    #[derive(Clone)]
    struct WebStub {
        search: StubReply,
        scrape: StubReply,
        research: StubReply,
        requests: Arc<Mutex<Vec<(String, Value)>>>,
    }

    impl WebStub {
        fn requests(&self) -> Vec<(String, Value)> {
            self.requests.lock().unwrap().clone()
        }
    }

    async fn stub_handler(State(stub): State<Arc<WebStub>>, uri: Uri, body: Bytes) -> Response {
        let body = serde_json::from_slice(&body).unwrap_or(Value::Null);
        stub.requests
            .lock()
            .unwrap()
            .push((uri.path().to_string(), body));
        let reply = match uri.path() {
            "/v2/search" => &stub.search,
            "/v2/scrape" => &stub.scrape,
            _ => &stub.research,
        };
        (reply.status, axum::Json(reply.body.clone())).into_response()
    }

    async fn spawn_stub(
        search: StubReply,
        scrape: StubReply,
        research: StubReply,
    ) -> (String, Arc<WebStub>) {
        let stub = Arc::new(WebStub {
            search,
            scrape,
            research,
            requests: Arc::new(Mutex::new(Vec::new())),
        });
        let app = axum::Router::new()
            .fallback(stub_handler)
            .with_state(stub.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a local stub port");
        let address = listener.local_addr().expect("stub address");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("stub server runs");
        });
        (format!("http://{address}"), stub)
    }

    fn search_body() -> Value {
        json!({
            "success": true,
            "data": {
                "web": [
                    {
                        "url": "https://example.com/rust",
                        "title": "Rust",
                        "description": "A systems language",
                    },
                    {
                        "url": "https://example.com/second",
                        "title": "Second",
                        "snippet": "Another result",
                    }
                ]
            }
        })
    }

    fn scrape_body(markdown: &str) -> Value {
        json!({
            "success": true,
            "data": {
                "markdown": markdown,
                "metadata": { "title": "Example", "statusCode": 200 }
            }
        })
    }

    fn okay_research() -> StubReply {
        StubReply::ok(json!({
            "run_id": "run-1",
            "output": { "type": "text", "content": "unused" }
        }))
    }

    fn firecrawl_router(base: &str) -> Arc<dhi_web::WebRouter> {
        let config = dhi_web::FirecrawlConfig::new(base, None, Duration::from_secs(5))
            .with_allow_private_targets(true);
        let provider = dhi_web::FirecrawlProvider::new(config).expect("firecrawl provider");
        Arc::new(
            dhi_web::WebRouter::new(
                vec![Arc::new(provider) as Arc<dyn WebProvider>],
                vec!["firecrawl".to_string()],
                Vec::new(),
                Duration::ZERO,
            )
            .with_allow_private_targets(true),
        )
    }

    fn parallel_router(base: &str) -> Arc<dhi_web::WebRouter> {
        let config = ParallelConfig::new(
            Secret::new("par-test-secret").expect("secret"),
            Duration::from_secs(5),
        )
        .with_base_url(base)
        .with_allow_private_targets(true);
        let provider = dhi_web::ParallelProvider::new(config).expect("parallel provider");
        Arc::new(
            dhi_web::WebRouter::new(
                vec![Arc::new(provider) as Arc<dyn WebProvider>],
                Vec::new(),
                vec!["parallel".to_string()],
                Duration::ZERO,
            )
            .with_allow_private_targets(true),
        )
    }

    fn web_runtime(
        router: Arc<dhi_web::WebRouter>,
    ) -> Arc<dhi_server::web_providers::WebProviderRuntime> {
        Arc::new(dhi_server::web_providers::WebProviderRuntime::with_router(
            router,
        ))
    }

    struct MarkerDelegate;

    #[async_trait]
    impl ToolBackend for MarkerDelegate {
        async fn execute(
            &self,
            _tool: &str,
            _input: &serde_json::Value,
        ) -> anyhow::Result<serde_json::Value> {
            Ok(json!({ "delegate": true }))
        }
    }

    // -- backend dispatch ----------------------------------------------------

    #[tokio::test]
    async fn web_search_goes_through_the_router_and_returns_normalized_results() {
        let (base, stub) = spawn_stub(
            StubReply::ok(search_body()),
            StubReply::ok(scrape_body("unused")),
            okay_research(),
        )
        .await;
        let backend = WebRoutingBackend::new(
            web_runtime(firecrawl_router(&base)),
            Arc::new(MarkerDelegate),
        );

        let output = backend
            .execute("web_search", &json!({ "query": "rust", "limit": 2 }))
            .await
            .expect("search returns");

        let results = output.as_array().expect("serialized result array");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["title"], "Rust");
        assert_eq!(results[0]["url"], "https://example.com/rust");
        assert_eq!(results[0]["snippet"], "A systems language");
        assert_eq!(results[0]["provider"], "firecrawl");

        let requests = stub.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0, "/v2/search");
        assert_eq!(requests[0].1["query"], "rust");
        assert_eq!(requests[0].1["limit"], 2);
    }

    #[tokio::test]
    async fn web_read_returns_markdown_from_the_router() {
        let (base, stub) = spawn_stub(
            StubReply::ok(search_body()),
            StubReply::ok(scrape_body("# Example\n\nBody.")),
            okay_research(),
        )
        .await;
        let backend = WebRoutingBackend::new(
            web_runtime(firecrawl_router(&base)),
            Arc::new(MarkerDelegate),
        );
        let url = format!("{base}/page");

        let output = backend
            .execute("web_read", &json!({ "url": url }))
            .await
            .expect("read returns");

        assert_eq!(output["markdown"], "# Example\n\nBody.");
        assert_eq!(output["title"], "Example");
        assert_eq!(output["provider"], "firecrawl");
        let requests = stub.requests();
        assert_eq!(requests[0].0, "/v2/scrape");
        assert_eq!(requests[0].1["url"], url);
    }

    #[tokio::test]
    async fn web_parse_is_an_alias_for_read_and_detects_documents_in_the_router() {
        let (base, stub) = spawn_stub(
            StubReply::ok(search_body()),
            StubReply::ok(scrape_body("# Report\n\nDocument body.")),
            okay_research(),
        )
        .await;
        let backend = WebRoutingBackend::new(
            web_runtime(firecrawl_router(&base)),
            Arc::new(MarkerDelegate),
        );
        let url = format!("{base}/report.pdf");

        let output = backend
            .execute("web_parse", &json!({ "url": url }))
            .await
            .expect("parse alias returns");

        assert_eq!(output["markdown"], "# Report\n\nDocument body.");
        let requests = stub.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0, "/v2/scrape");
        assert_eq!(requests[0].1["url"], url);
        assert_eq!(
            requests[0].1["parsers"],
            json!([{ "type": "pdf", "mode": "auto" }]),
            "the router's read path detects the PDF"
        );
    }

    #[tokio::test]
    async fn web_research_maps_provider_errors_without_losing_the_message() {
        let (base, _stub) = spawn_stub(
            StubReply::ok(search_body()),
            StubReply::ok(scrape_body("unused")),
            StubReply::status(500, json!({ "error": "research backend exploded" })),
        )
        .await;
        let backend = WebRoutingBackend::new(
            web_runtime(parallel_router(&base)),
            Arc::new(MarkerDelegate),
        );

        let err = backend
            .execute("web_research", &json!({ "question": "why?" }))
            .await
            .expect_err("the 500 fails the call");
        let message = err.to_string();
        assert!(message.contains("parallel"), "{message}");
        assert!(message.contains("HTTP 500"), "{message}");
        assert!(message.contains("research backend exploded"), "{message}");
    }

    #[tokio::test]
    async fn web_research_without_parallel_returns_the_clear_no_provider_error() {
        let backend = WebRoutingBackend::new(
            web_runtime(firecrawl_router("http://127.0.0.1:3002")),
            Arc::new(MarkerDelegate),
        );
        let err = backend
            .execute("web_research", &json!({ "question": "why?" }))
            .await
            .expect_err("Firecrawl cannot research");
        let message = err.to_string();
        assert!(message.contains("web_research"), "{message}");
        assert!(message.contains("PARALLEL_API_KEY"), "{message}");
    }

    #[tokio::test]
    async fn unknown_names_delegate_and_a_missing_router_fails_web_names_closed() {
        let backend = WebRoutingBackend::new(
            Arc::new(dhi_server::web_providers::WebProviderRuntime::empty()),
            Arc::new(MarkerDelegate),
        );
        let output = backend
            .execute("other_tool", &json!({ "query": "x" }))
            .await
            .expect("other tools delegate");
        assert_eq!(output["delegate"], true);

        let err = backend
            .execute("web_search", &json!({ "query": "x" }))
            .await
            .expect_err("no router, no web call");
        assert!(err.to_string().contains("web provider router"), "{err}");
    }

    // -- registration --------------------------------------------------------

    struct NeverSink;

    #[async_trait]
    impl ToolSink for NeverSink {
        async fn execute(
            &self,
            _task_id: Uuid,
            _intent_id: Uuid,
            _request: ToolCallRequest,
        ) -> anyhow::Result<ToolCallOutcome> {
            unreachable!("registration tests never dispatch")
        }
    }

    fn never_sink() -> Arc<dyn ToolSink> {
        Arc::new(NeverSink)
    }

    #[tokio::test]
    async fn registration_adds_four_unique_web_tools_with_stable_schemas() {
        let handle = ToolServer::new().run();
        let registration = register_web_tools(
            &handle,
            never_sink(),
            Some(firecrawl_router("http://127.0.0.1:3002")),
        )
        .await
        .expect("the web tools register");

        assert_eq!(
            registration.registered,
            vec!["web_search", "web_read", "web_research", "web_parse"]
        );
        let defs = handle.tool_defs(None).await.expect("definitions");
        let names = defs
            .iter()
            .map(|definition| definition.name.as_str().to_string())
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 4, "no duplicate registrations: {names:?}");
        let unique = names.iter().collect::<HashSet<_>>();
        assert_eq!(unique.len(), 4, "{names:?}");

        let by_name = defs
            .iter()
            .map(|definition| (definition.name.as_str(), definition))
            .collect::<HashMap<_, _>>();
        assert_eq!(
            by_name["web_search"].parameters["required"],
            json!(["query"])
        );
        assert_eq!(by_name["web_read"].parameters["required"], json!(["url"]));
        assert_eq!(
            by_name["web_research"].parameters["required"],
            json!(["question"])
        );
        assert_eq!(
            by_name["web_parse"].parameters["required"],
            json!(["url"]),
            "the compatibility alias keeps the old schema"
        );
        assert!(
            by_name["web_parse"].parameters["properties"]
                .get("objective")
                .is_none(),
            "web_parse accepts only the old url argument"
        );
    }

    #[tokio::test]
    async fn absent_router_registers_no_web_tools() {
        let handle = ToolServer::new().run();
        let registration = register_web_tools(&handle, never_sink(), None)
            .await
            .expect("a missing router is not fatal");
        assert!(registration.registered.is_empty());
        assert!(
            handle
                .tool_defs(None)
                .await
                .expect("definitions")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn an_existing_web_parse_is_never_replaced() {
        let handle = ToolServer::new().run();
        dhi_agent::register_guarded_tool(
            &handle,
            GuardedToolSpec::new(
                "web_parse",
                "another source owns this name",
                json!({
                    "type": "object",
                    "properties": { "url": { "type": "string" } },
                    "required": ["url"]
                }),
            ),
            never_sink(),
        )
        .expect("the existing tool registers");

        let registration = register_web_tools(
            &handle,
            never_sink(),
            Some(firecrawl_router("http://127.0.0.1:3002")),
        )
        .await
        .expect("the remaining web tools register");

        assert_eq!(
            registration.registered,
            vec!["web_search", "web_read", "web_research"]
        );
        let defs = handle.tool_defs(None).await.expect("definitions");
        let parse = defs
            .iter()
            .find(|definition| definition.name.as_str() == "web_parse")
            .expect("web_parse stays registered");
        assert_eq!(parse.description, "another source owns this name");
        assert_eq!(defs.len(), 4, "no duplicate web_parse: {defs:?}");
    }

    // -- config bridge -------------------------------------------------------

    struct EnvRestore {
        values: Vec<(String, Option<OsString>)>,
    }

    impl EnvRestore {
        fn capture(names: &[&str]) -> Self {
            Self {
                values: names
                    .iter()
                    .map(|name| ((*name).to_string(), std::env::var_os(name)))
                    .collect(),
            }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (name, value) in &self.values {
                // SAFETY: this test is `serial` and restores every value.
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(name, value),
                        None => std::env::remove_var(name),
                    }
                }
            }
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn the_existing_firecrawl_config_file_still_builds_a_router() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"firecrawl":{"apiKey":"fc-config-secret","timeoutSecs":9}}"#,
        )
        .expect("write config");

        let _restore = EnvRestore::capture(&[
            "DHI_CONFIG",
            "DHI_WEB_PROVIDER",
            "FIRECRAWL_BASE_URL",
            "FIRECRAWL_API_KEY",
            "FIRECRAWL_TIMEOUT_SECS",
            "DHI_WEB_TIMEOUT_SECS",
            "DHI_WEB_CACHE_TTL_SECS",
            "DHI_WEB_OPERATION_TIMEOUT_SECS",
            "DHI_WEB_RESEARCH_TIMEOUT_SECS",
            "DHI_WEB_ALLOW_PRIVATE_TARGETS",
            "TINYFISH_FETCH_TIMEOUT_SECS",
        ]);
        // SAFETY: this test is `serial` and the guard restores every value.
        unsafe {
            std::env::set_var("DHI_CONFIG", &path);
            std::env::set_var("DHI_WEB_PROVIDER", "firecrawl");
            for name in [
                "FIRECRAWL_BASE_URL",
                "FIRECRAWL_API_KEY",
                "FIRECRAWL_TIMEOUT_SECS",
                "DHI_WEB_TIMEOUT_SECS",
                "DHI_WEB_CACHE_TTL_SECS",
                "DHI_WEB_OPERATION_TIMEOUT_SECS",
                "DHI_WEB_RESEARCH_TIMEOUT_SECS",
                "DHI_WEB_ALLOW_PRIVATE_TARGETS",
                "TINYFISH_FETCH_TIMEOUT_SECS",
            ] {
                std::env::remove_var(name);
            }
        }

        let router = dhi_web::WebRouter::from_env().expect("the config file builds a router");
        assert_eq!(router.provider_order(), ["firecrawl"]);
        assert!(router.research_provider_order().is_empty());
        assert!(router.skipped_providers().is_empty());
        assert_eq!(router.cache_ttl(), Duration::from_secs(300));
    }
}
