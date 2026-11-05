//! The MCP manager: connections, discovery, guarded registration, refresh,
//! and live reload.
//!
//! Built from config before the runtime exists (the guard's backend needs its
//! registry), connected on the worker's shared `ToolServerHandle` at startup
//! with the one shared `ToolSink`. A per-server failure — a dead endpoint, a
//! handshake or discovery timeout, a tool whose name cannot be guarded — is
//! logged and skipped: the process boots, the other servers connect, and the
//! failed parts simply do not exist. Unknown tools fail closed in
//! [`McpBackend`] with `no MCP tool is registered as …`.
//!
//! A live reload compares the running config with a freshly loaded one, keeps
//! unchanged connections, retires removed/changed servers before connecting
//! their replacements, and leaves a failed replacement disconnected with a
//! clear status — never a stale guarded name.
//!
//! [`McpBackend`]: super::registry::McpBackend

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock, Weak};

use anyhow::{anyhow, bail};
use dhi_agent::{GuardedToolSpec, ToolSink};
use rig::tool::rmcp::{McpConnection, McpTool};
use rig::tool::server::ToolServerHandle;
use rmcp::model::{ClientCapabilities, ClientConfig, Implementation};
use rmcp::service::{NotificationContext, RoleClient};
use rmcp::transport::IntoTransport;
use rmcp::{ClientHandler, ServiceExt};
use tracing::{info, warn};

use super::config::{CONNECT_TIMEOUT, ValidatedServer, guarded_name};
use super::registry::{GuardedMcpRegistry, McpRolePlanner, McpVisibilityConfig, SharedMcpPlanner};
use super::reload::{ReloadFailure, ReloadReport, ServerSummary, plan_between, summarize};
use super::transport::{establish, transport_label};
use super::types::{McpServerStatus, McpToolInfo};

/// One connected server's process-lifetime record.
struct ServerSlot {
    server: ValidatedServer,
    /// Owns the rmcp running service; dropping or closing it ends the
    /// connection and its notification task.
    connection: McpConnection,
    /// The guarded names this server currently owns on the registry.
    registered: Vec<String>,
    /// Registration target, kept for `tools/list_changed` refreshes.
    handle: ToolServerHandle,
    /// The shared sink every guarded registration routes through.
    sink: Arc<dyn ToolSink>,
    /// A process-local connection identity, used by tests (and logs) to prove
    /// an unchanged server kept its live connection across a reload.
    stamp: u64,
}

/// The registration target captured by `connect_all` (or the first injected
/// connection in tests). Live reload needs it to connect new servers.
#[derive(Clone)]
struct RuntimeParts {
    handle: ToolServerHandle,
    sink: Arc<dyn ToolSink>,
}

/// Owns the MCP connections and their guarded registrations.
///
/// Built from config before the runtime exists (the guard's backend needs
/// its registry), connected on the worker's shared `ToolServerHandle` at
/// startup with the one shared `ToolSink`. A per-server failure — a dead
/// endpoint, a handshake or discovery timeout, a tool whose name cannot be
/// guarded — is logged and skipped: the process boots, the other servers
/// connect, and the failed parts simply do not exist. Unknown tools fail
/// closed in [`McpBackend`](super::registry::McpBackend) with `no MCP tool is registered as …`.
pub struct McpManager {
    registry: Arc<GuardedMcpRegistry>,
    /// The validated, enabled servers. Replaced atomically (short std lock,
    /// never held across an await) by a live reload.
    config: RwLock<Vec<ValidatedServer>>,
    /// Where reloads read the config from. `None` falls back to the standard
    /// `DHI_MCP_CONFIG` / `ops/mcp.json` discovery.
    config_path: Option<PathBuf>,
    /// One slot per CONNECTED server.
    servers: tokio::sync::RwLock<Vec<ServerSlot>>,
    /// Health/status per configured server, including failed connections.
    /// The discovered tool list here is the cache the API and dispatch see.
    statuses: tokio::sync::RwLock<BTreeMap<String, McpServerStatus>>,
    /// Serializes lifecycle work: startup connect, reload, refresh, shutdown.
    /// Notification-driven refreshes use `try_lock` and defer, so a closing
    /// connection can never deadlock waiting on its own handler.
    lifecycle: tokio::sync::Mutex<()>,
    /// The handle and sink captured at startup, used to connect servers added
    /// after boot without restarting the process.
    runtime: tokio::sync::RwLock<Option<RuntimeParts>>,
    /// Live role planner shared with worker views and deferred meta-tools.
    /// `None` until the server wires it after startup.
    role_planner: RwLock<Option<SharedMcpPlanner>>,
    next_stamp: AtomicU64,
}

impl McpManager {
    /// An idle manager with no servers — the state after a missing or
    /// invalid config. The registry stays empty, so every dispatch fails
    /// closed with a readable error. Used by tests and degraded boots.
    pub fn idle() -> Arc<Self> {
        Self::new(Vec::new())
    }

    /// A manager for `servers` (already validated and enabled, config
    /// order). Call [`McpManager::connect_all`] on the worker's shared
    /// registry handle once the runtime exists.
    pub fn new(servers: Vec<ValidatedServer>) -> Arc<Self> {
        Self::with_config_path(servers, None)
    }

    /// A manager for `servers` that reloads from `config_path` instead of the
    /// process-wide discovery fallback.
    pub fn with_config_path(
        servers: Vec<ValidatedServer>,
        config_path: Option<PathBuf>,
    ) -> Arc<Self> {
        let statuses = servers
            .iter()
            .map(|server| (server.name.clone(), status_for(server)))
            .collect();
        Arc::new(Self {
            registry: Arc::new(GuardedMcpRegistry::new()),
            config: RwLock::new(servers),
            config_path,
            servers: tokio::sync::RwLock::new(Vec::new()),
            statuses: tokio::sync::RwLock::new(statuses),
            lifecycle: tokio::sync::Mutex::new(()),
            runtime: tokio::sync::RwLock::new(None),
            role_planner: RwLock::new(None),
            next_stamp: AtomicU64::new(1),
        })
    }

    /// Builds a manager from an explicit config file. Errors are returned,
    /// unlike [`manager_from_config`], because the API add/edit path must not
    /// silently operate on a partially loaded file.
    pub fn from_config_path(path: impl Into<PathBuf>) -> anyhow::Result<Arc<Self>> {
        let path = path.into();
        let servers = super::reload::load_managed_servers(&path)?;
        Ok(Self::with_config_path(servers, Some(path)))
    }

    /// Every configured server's health, sorted by name.
    pub async fn statuses(&self) -> Vec<McpServerStatus> {
        self.statuses.read().await.values().cloned().collect()
    }

    /// The cached tool list of one configured server; `None` only when the
    /// name is not configured at all.
    pub async fn tools(&self, name: &str) -> Option<Vec<McpToolInfo>> {
        self.statuses
            .read()
            .await
            .get(name)
            .map(|status| status.tools.clone())
    }

    /// Re-discovers one connected server's tools, updates its status, and
    /// returns the current tool list. An unknown or disconnected server is
    /// an error.
    pub async fn refresh(&self, name: &str) -> anyhow::Result<Vec<McpToolInfo>> {
        let _guard = self.lifecycle.lock().await;
        let connected = self
            .servers
            .read()
            .await
            .iter()
            .any(|slot| slot.server.name == name);
        if !connected {
            bail!("MCP server {name:?} is not connected");
        }
        self.refresh_server_locked(name).await?;
        self.tools(name)
            .await
            .ok_or_else(|| anyhow!("MCP server {name:?} has no status"))
    }

    /// Records a successful connect/refresh in the health cache.
    async fn mark_connected(&self, name: &str, tools: &[McpToolInfo]) {
        let mut statuses = self.statuses.write().await;
        if let Some(status) = statuses.get_mut(name) {
            status.connected = true;
            status.tools = tools.to_vec();
            status.last_error = None;
        }
    }

    /// Records a failed connect: the server is not usable in this process.
    async fn mark_connect_failed(&self, name: &str, error: &anyhow::Error) {
        let mut statuses = self.statuses.write().await;
        if let Some(status) = statuses.get_mut(name) {
            status.connected = false;
            status.tools.clear();
            status.last_error = Some(error.to_string());
        }
    }

    /// Records a failed refresh. The connection may still be alive, so the
    /// previous tool cache is kept and `connected` is unchanged.
    async fn mark_refresh_failed(&self, name: &str, error: &anyhow::Error) {
        let mut statuses = self.statuses.write().await;
        if let Some(status) = statuses.get_mut(name) {
            status.last_error = Some(error.to_string());
        }
    }

    /// The dispatch-side registry (shared with the
    /// [`McpBackend`](super::registry::McpBackend)).
    pub fn registry(&self) -> Arc<GuardedMcpRegistry> {
        self.registry.clone()
    }

    /// Wires the live role planner used by worker views and the deferred
    /// meta-tools. The server calls this once after building the planner.
    pub fn set_role_planner(&self, planner: SharedMcpPlanner) {
        *self
            .role_planner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(planner);
    }

    /// Rebuilds the shared role planner from the manager's current validated
    /// config and discovered tools. Called after a reload or a tool refresh so
    /// new servers become visible without a process restart.
    async fn refresh_role_planner(&self) {
        let planner = self
            .role_planner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(planner) = planner else {
            return;
        };
        let visibility = McpVisibilityConfig::from_servers(&self.config_snapshot());
        let snapshot = self.registry.snapshot().await;
        let next = McpRolePlanner::new(&visibility, &snapshot);
        *planner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = next;
    }

    /// How many servers the config asked to connect (connected or not).
    pub fn configured_servers(&self) -> usize {
        self.config_snapshot().len()
    }

    /// Every configured server in config order (connected or not). This is a
    /// clone of the validated config the manager currently holds, for callers
    /// that plan lifecycle work over the configured set.
    pub fn server_specs(&self) -> Vec<ValidatedServer> {
        self.config_snapshot()
    }

    /// The config file a reload would read: the explicit path captured at
    /// construction, else the standard `DHI_MCP_CONFIG` / `ops/mcp.json`
    /// discovery.
    pub fn config_path(&self) -> Option<PathBuf> {
        self.config_path.clone().or_else(dhi_mcp::config_path)
    }

    /// A masked summary of one configured server (no header values leak).
    pub fn server_summary(&self, name: &str) -> Option<ServerSummary> {
        self.config_snapshot()
            .iter()
            .find(|server| server.name == name)
            .map(summarize)
    }

    /// Masked summaries of every configured server, config order.
    pub fn server_summaries(&self) -> Vec<ServerSummary> {
        self.config_snapshot().iter().map(summarize).collect()
    }

    /// The process-local identity of one connected server's connection.
    /// An unchanged server keeps its stamp across a reload.
    pub async fn connection_stamp(&self, name: &str) -> Option<u64> {
        self.servers
            .read()
            .await
            .iter()
            .find(|slot| slot.server.name == name)
            .map(|slot| slot.stamp)
    }

    /// Connects every configured server in config order, registering its
    /// discovered tools as guarded tools on `handle` through `sink`.
    /// Idempotent per server name: an already-connected server is skipped.
    pub async fn connect_all(self: &Arc<Self>, handle: &ToolServerHandle, sink: Arc<dyn ToolSink>) {
        let _guard = self.lifecycle.lock().await;
        *self.runtime.write().await = Some(RuntimeParts {
            handle: handle.clone(),
            sink: sink.clone(),
        });
        let servers = self.config_snapshot();
        for server in &servers {
            {
                let slots = self.servers.read().await;
                if slots.iter().any(|slot| slot.server.name == server.name) {
                    info!(server = %server.name, "MCP server already connected; skipped");
                    continue;
                }
            }
            match self.connect_one_locked(handle, server, sink.clone()).await {
                Ok(count) => info!(
                    server = %server.name,
                    tools = count,
                    call_timeout_secs = server.call_timeout.as_secs(),
                    "MCP server connected; every discovered tool is registered as a \
                     guarded tool"
                ),
                Err(error) => warn!(
                    server = %server.name,
                    error = %error,
                    "MCP server not connected; its tools are unavailable in this process"
                ),
            }
        }
    }

    /// Connects one server: build the transport, run the (bounded)
    /// handshake, discover tools, register them guarded. The connection is
    /// kept in the manager for the process lifetime. Returns how many tools
    /// registered.
    #[cfg(test)]
    pub(crate) async fn connect_one(
        self: &Arc<Self>,
        handle: &ToolServerHandle,
        server: &ValidatedServer,
        sink: Arc<dyn ToolSink>,
    ) -> anyhow::Result<usize> {
        let _guard = self.lifecycle.lock().await;
        self.connect_one_locked(handle, server, sink).await
    }

    /// The locked half of [`McpManager::connect_one`].
    async fn connect_one_locked(
        self: &Arc<Self>,
        handle: &ToolServerHandle,
        server: &ValidatedServer,
        sink: Arc<dyn ToolSink>,
    ) -> anyhow::Result<usize> {
        match self.connect_one_inner(handle, server, sink).await {
            Ok(count) => Ok(count),
            Err(error) => {
                self.mark_connect_failed(&server.name, &error).await;
                Err(error)
            }
        }
    }

    /// The transport-building half of [`McpManager::connect_one`]; status is
    /// recorded once by the caller. Transport construction lives in
    /// [`establish`], which owns the per-transport match.
    async fn connect_one_inner(
        self: &Arc<Self>,
        handle: &ToolServerHandle,
        server: &ValidatedServer,
        sink: Arc<dyn ToolSink>,
    ) -> anyhow::Result<usize> {
        let connection = establish(self, server).await?;
        self.finish_connect_locked(handle, server, sink, connection)
            .await
    }

    /// Discovery plus guarded registration for an established connection,
    /// then the process-lifetime slot. Split from [`McpManager::connect_one`]
    /// so tests can inject a transport.
    #[cfg(test)]
    pub(crate) async fn finish_connect(
        self: &Arc<Self>,
        handle: &ToolServerHandle,
        server: &ValidatedServer,
        sink: Arc<dyn ToolSink>,
        connection: McpConnection,
    ) -> anyhow::Result<usize> {
        let _guard = self.lifecycle.lock().await;
        self.finish_connect_locked(handle, server, sink, connection)
            .await
    }

    async fn finish_connect_locked(
        self: &Arc<Self>,
        handle: &ToolServerHandle,
        server: &ValidatedServer,
        sink: Arc<dyn ToolSink>,
        connection: McpConnection,
    ) -> anyhow::Result<usize> {
        {
            let mut runtime = self.runtime.write().await;
            if runtime.is_none() {
                *runtime = Some(RuntimeParts {
                    handle: handle.clone(),
                    sink: sink.clone(),
                });
            }
        }
        let tools = connection
            .tools()
            .await
            .map_err(|err| anyhow!("MCP server {:?} tool discovery failed: {err}", server.name))?;
        let tool_infos = tool_infos(&tools);
        let sink_for_refresh = sink.clone();
        let registered = register_batch(&self.registry, handle, server, &[], sink, tools).await;
        let count = registered.len();
        let stamp = self.next_stamp.fetch_add(1, Ordering::Relaxed);
        self.servers.write().await.push(ServerSlot {
            server: server.clone(),
            connection,
            registered,
            handle: handle.clone(),
            sink: sink_for_refresh,
            stamp,
        });
        self.mark_connected(&server.name, &tool_infos).await;
        Ok(count)
    }

    /// The bounded handshake for any rmcp client transport (streamable
    /// HTTP, a stdio child process, or an in-process duplex in tests). A
    /// dead or unreachable transport must not stall startup; tool-list
    /// discovery afterwards is bounded by rmcp's own 30 s refresh deadline.
    pub(crate) async fn connect_shared<T, E, A>(
        self: &Arc<Self>,
        server: &ValidatedServer,
        transport: T,
    ) -> anyhow::Result<McpConnection>
    where
        T: IntoTransport<RoleClient, E, A>,
        E: std::error::Error + Send + Sync + 'static,
    {
        let handler = GuardedMcpHandler {
            client_config: client_info(),
            manager: Arc::downgrade(self),
            server_name: server.name.clone(),
        };
        let service = match tokio::time::timeout(
            CONNECT_TIMEOUT,
            ServiceExt::serve(handler, transport),
        )
        .await
        {
            Ok(Ok(service)) => service,
            Ok(Err(error)) => bail!("handshake failed: {error}"),
            Err(_) => bail!("handshake timed out after {}s", CONNECT_TIMEOUT.as_secs()),
        };
        Ok(McpConnection::new(service))
    }

    /// Re-fetches one server's tool list and re-runs the guarded
    /// registration: replaced names are re-registered (last wins on the
    /// registry), removed names are retired, stale registry entries go.
    /// Called from the rmcp notification handler on
    /// `notifications/tools/list_changed`.
    async fn refresh_server_locked(&self, server_name: &str) -> anyhow::Result<()> {
        let (server, connection, handle, sink, previously) = {
            let slots = self.servers.read().await;
            let slot = slots
                .iter()
                .find(|slot| slot.server.name == server_name)
                .ok_or_else(|| anyhow!("MCP server {server_name:?} is not connected"))?;
            (
                slot.server.clone(),
                slot.connection.clone(),
                slot.handle.clone(),
                slot.sink.clone(),
                slot.registered.clone(),
            )
        };
        let tools = match connection.tools().await {
            Ok(tools) => tools,
            Err(err) => {
                let error = anyhow!("MCP server {server_name:?} tool re-discovery failed: {err}");
                self.mark_refresh_failed(server_name, &error).await;
                return Err(error);
            }
        };
        let tool_infos = tool_infos(&tools);
        let registered =
            register_batch(&self.registry, &handle, &server, &previously, sink, tools).await;
        let mut slots = self.servers.write().await;
        if let Some(slot) = slots
            .iter_mut()
            .find(|slot| slot.server.name == server_name)
        {
            slot.registered = registered;
        }
        drop(slots);
        self.mark_connected(server_name, &tool_infos).await;
        self.refresh_role_planner().await;
        Ok(())
    }

    /// Reloads the configured file, applying its enabled servers to the live
    /// manager. The config is replaced even when a server cannot connect:
    /// the failed server is left disconnected with its error in `statuses`.
    pub async fn reload_from_disk(self: &Arc<Self>) -> anyhow::Result<ReloadReport> {
        let path = self.config_path().ok_or_else(|| {
            anyhow!(
                "no MCP config path is configured; set DHI_MCP_CONFIG or place \
                 ops/mcp.json in the working tree"
            )
        })?;
        let servers = super::reload::load_managed_servers(&path)?;
        self.reload_servers(servers).await
    }

    /// Applies an already validated server set to the live manager.
    pub async fn reload_servers(
        self: &Arc<Self>,
        new_servers: Vec<ValidatedServer>,
    ) -> anyhow::Result<ReloadReport> {
        let _guard = self.lifecycle.lock().await;
        let old_servers = self.config_snapshot();
        let plan = plan_between(&old_servers, &new_servers)?;
        let runtime = self.runtime.read().await.clone();
        let old_statuses = self.statuses.read().await.clone();

        let mut report = ReloadReport {
            runtime_attached: runtime.is_some(),
            configured: new_servers.len(),
            ..ReloadReport::default()
        };

        // Keep the health record (and its tool cache) for untouched servers;
        // every added/changed server starts disconnected until its connect
        // attempt fills the cache.
        let kept: HashSet<&str> = plan
            .actions
            .iter()
            .filter_map(|action| match action {
                dhi_mcp_upgrade::reload::ReloadAction::Keep { name } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        let mut statuses = BTreeMap::new();
        for server in &new_servers {
            let status = if kept.contains(server.name.as_str()) {
                old_statuses
                    .get(&server.name)
                    .cloned()
                    .unwrap_or_else(|| status_for(server))
            } else {
                status_for(server)
            };
            statuses.insert(server.name.clone(), status);
        }
        *self.statuses.write().await = statuses;
        *self
            .config
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = new_servers.clone();

        for action in &plan.actions {
            match action {
                dhi_mcp_upgrade::reload::ReloadAction::Keep { name } => {
                    report.kept.push(name.clone());
                }
                dhi_mcp_upgrade::reload::ReloadAction::Disconnect { name } => {
                    self.disconnect_slot_locked(name).await;
                    report.disconnected.push(name.clone());
                }
                dhi_mcp_upgrade::reload::ReloadAction::Connect { spec } => {
                    let server = server_by_name(&new_servers, &spec.name)?;
                    match self.connect_server_locked(server, &runtime).await {
                        Ok(_) => report.connected.push(spec.name.clone()),
                        Err(error) => report.failed.push(ReloadFailure {
                            name: spec.name.clone(),
                            error: error.to_string(),
                        }),
                    }
                }
                dhi_mcp_upgrade::reload::ReloadAction::Reconnect { new, .. } => {
                    self.disconnect_slot_locked(&new.name).await;
                    let server = server_by_name(&new_servers, &new.name)?;
                    match self.connect_server_locked(server, &runtime).await {
                        Ok(_) => report.reconnected.push(new.name.clone()),
                        Err(error) => report.failed.push(ReloadFailure {
                            name: new.name.clone(),
                            error: error.to_string(),
                        }),
                    }
                }
            }
        }
        self.refresh_role_planner().await;
        Ok(report)
    }

    /// Connects one server through the captured runtime, recording a failed
    /// attempt in the status cache (never leaving old guarded names).
    async fn connect_server_locked(
        self: &Arc<Self>,
        server: &ValidatedServer,
        runtime: &Option<RuntimeParts>,
    ) -> anyhow::Result<usize> {
        let Some(runtime) = runtime else {
            let error = anyhow!(
                "MCP runtime is not attached; {:?} cannot connect in this process",
                server.name
            );
            self.mark_connect_failed(&server.name, &error).await;
            return Err(error);
        };
        self.connect_one_locked(&runtime.handle, server, runtime.sink.clone())
            .await
    }

    /// Removes one connected server: its guarded catalog entries first, then
    /// its registry entries, then its connection. Returns whether a live
    /// slot existed.
    async fn disconnect_slot_locked(&self, name: &str) -> bool {
        let slot = {
            let mut slots = self.servers.write().await;
            slots
                .iter()
                .position(|slot| slot.server.name == name)
                .map(|index| slots.remove(index))
        };
        let Some(slot) = slot else {
            return false;
        };
        for guarded in &slot.registered {
            slot.handle.remove_tool(guarded);
            self.registry.remove(guarded).await;
        }
        slot.connection.close().await;
        true
    }

    /// Clean shutdown: retires every guarded registration from the shared
    /// registry and closes every connection. Dropping the manager without
    /// this also works at process exit; the explicit close is for orderly
    /// teardown before then.
    pub async fn shutdown(&self) {
        let _guard = self.lifecycle.lock().await;
        let slots: Vec<ServerSlot> = {
            let mut slots = self.servers.write().await;
            std::mem::take(&mut *slots)
        };
        for slot in &slots {
            for name in &slot.registered {
                slot.handle.remove_tool(name);
            }
            slot.connection.close().await;
        }
        self.registry.clear().await;
        for status in self.statuses.write().await.values_mut() {
            status.connected = false;
            status.tools.clear();
        }
        *self.runtime.write().await = None;
    }

    /// A short std-lock read; never held across an await.
    fn config_snapshot(&self) -> Vec<ValidatedServer> {
        self.config
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

/// The status record for a server that has not connected yet.
fn status_for(server: &ValidatedServer) -> McpServerStatus {
    McpServerStatus {
        name: server.name.clone(),
        transport: transport_label(&server.transport).to_string(),
        enabled: true,
        connected: false,
        tools: Vec::new(),
        last_error: None,
        call_timeout_secs: server.call_timeout.as_secs(),
    }
}

fn server_by_name<'a>(
    servers: &'a [ValidatedServer],
    name: &str,
) -> anyhow::Result<&'a ValidatedServer> {
    servers
        .iter()
        .find(|server| server.name == name)
        .ok_or_else(|| anyhow!("reload plan references unknown MCP server {name:?}"))
}

/// The status-cache view of discovered tools: MCP name plus description.
fn tool_infos(tools: &[McpTool]) -> Vec<McpToolInfo> {
    tools
        .iter()
        .map(|tool| {
            let definition = tool.definition();
            McpToolInfo {
                name: definition.name.to_string(),
                description: definition
                    .description
                    .as_deref()
                    .unwrap_or_default()
                    .to_string(),
            }
        })
        .collect()
}

/// Builds one guarded registration batch from discovered tools and applies
/// it: new and changed names are (re-)registered, names this server owned
/// before but no longer lists are retired. Names owned by ANOTHER server are
/// refused with a warning (sanitization collisions). Returns the names now
/// owned by this server.
async fn register_batch(
    registry: &Arc<GuardedMcpRegistry>,
    handle: &ToolServerHandle,
    server: &ValidatedServer,
    previously_owned: &[String],
    sink: Arc<dyn ToolSink>,
    tools: Vec<McpTool>,
) -> Vec<String> {
    let mut registered = Vec::new();
    let mut batch: HashSet<String> = HashSet::new();
    for tool in tools {
        let mcp_name = tool.definition().name.to_string();
        let Some(name) = guarded_name(&server.segment, &mcp_name) else {
            warn!(
                server = %server.name,
                tool = %mcp_name,
                "MCP tool skipped: its name has no usable characters after sanitization"
            );
            continue;
        };
        if let Some(owner) = registry.owner_of(&name).await
            && owner != server.name
        {
            warn!(
                server = %server.name,
                tool = %mcp_name,
                guarded_name = %name,
                owner = %owner,
                "MCP tool skipped: its guarded name is owned by another server"
            );
            continue;
        }
        if !batch.insert(name.clone()) {
            warn!(
                server = %server.name,
                tool = %mcp_name,
                guarded_name = %name,
                "MCP tool skipped: another tool of this server sanitizes to the \
                 same guarded name"
            );
            continue;
        }
        let description = tool
            .definition()
            .description
            .as_deref()
            .filter(|text| !text.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("MCP tool {mcp_name:?} on MCP server {:?}.", server.name));
        let schema = tool.definition().schema_as_json_value();
        let tool = tool.with_timeout(server.call_timeout);
        match dhi_agent::register_guarded_tool(
            handle,
            GuardedToolSpec::new(&name, &description, schema),
            sink.clone(),
        ) {
            Ok(()) => {
                registry.install(name.clone(), tool, &server.name).await;
                registered.push(name);
            }
            Err(err) => warn!(
                server = %server.name,
                tool = %mcp_name,
                error = %err,
                "MCP tool registration failed; skipped"
            ),
        }
    }
    // Retire names this server owned before that are gone or no longer
    // registerable. The registry keeps them dispatchable until this runs, so
    // a refresh never opens a not-found window for surviving names.
    for stale in previously_owned {
        if registered.contains(stale) {
            continue;
        }
        handle.remove_tool(stale);
        registry.remove(stale).await;
        info!(
            server = %server.name,
            tool = %stale,
            "MCP tool retired after a tool-list refresh"
        );
    }
    registered
}

/// The client info every MCP handshake presents: a dhi-server implementation
/// with process version, default capabilities.
fn client_info() -> ClientConfig {
    ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("dhi-server", env!("CARGO_PKG_VERSION")),
    )
}

/// The rmcp client handler for one server: answers the handshake with the
/// client info and keeps the guarded registrations current on
/// `notifications/tools/list_changed`. It holds the manager only weakly: the
/// handler lives inside the connection's service task, and the manager must
/// always be able to drop (and so drop the connections) without waiting for
/// it.
struct GuardedMcpHandler {
    client_config: ClientConfig,
    manager: Weak<McpManager>,
    server_name: String,
}

impl ClientHandler for GuardedMcpHandler {
    fn get_info(&self) -> ClientConfig {
        self.client_config.clone()
    }

    async fn on_tool_list_changed(&self, context: NotificationContext<RoleClient>) {
        let _ = context; // The refresh re-uses the manager's stored connection.
        let Some(manager) = self.manager.upgrade() else {
            return; // The manager is shutting down; nothing to refresh.
        };
        // Never wait on a reload while holding a connection's service task:
        // a concurrent shutdown closes that connection and would deadlock.
        // If a reload is in progress, defer until it finishes.
        if let Ok(_guard) = manager.lifecycle.try_lock() {
            if let Err(error) = manager.refresh_server_locked(&self.server_name).await {
                warn!(
                    server = %self.server_name,
                    error = %error,
                    "MCP tool list refresh failed; keeping the previous registrations"
                );
            }
            return;
        }
        let manager = manager.clone();
        let server_name = self.server_name.clone();
        tokio::spawn(async move {
            let _guard = manager.lifecycle.lock().await;
            if let Err(error) = manager.refresh_server_locked(&server_name).await {
                warn!(
                    server = %server_name,
                    error = %error,
                    "MCP tool list refresh failed; keeping the previous registrations"
                );
            }
        });
    }
}

/// Discovers `ops/mcp.json` (`DHI_MCP_CONFIG` wins), validates the entries,
/// and builds the manager. Every degradation is logged and non-fatal: a
/// missing file is info at most, a malformed file or invalid entry a
/// warning — the server still boots, just without the offending servers.
pub fn manager_from_config() -> Arc<McpManager> {
    let Some(path) = dhi_mcp::config_path() else {
        info!(
            "no MCP config found (looked for ops/mcp.json, or set DHI_MCP_CONFIG); \
             MCP tools disabled"
        );
        return McpManager::idle();
    };
    match dhi_mcp::edit::load_lenient(&path) {
        Err(error) => {
            warn!(
                path = %path.display(),
                error = %error,
                "MCP config invalid; booting without MCP tools"
            );
            McpManager::with_config_path(Vec::new(), Some(path))
        }
        Ok(None) => {
            info!(
                path = %path.display(),
                "MCP config is empty or absent; MCP tools disabled"
            );
            McpManager::with_config_path(Vec::new(), Some(path))
        }
        Ok(Some(file)) => {
            let validated = match file.validate() {
                Ok(validated) => validated,
                Err(error) => {
                    warn!(
                        path = %path.display(),
                        error = %error,
                        "MCP config invalid; booting without MCP tools"
                    );
                    return McpManager::with_config_path(Vec::new(), Some(path));
                }
            };
            let mut servers = Vec::with_capacity(validated.len());
            for server in validated {
                let name = server.name.clone();
                match ValidatedServer::try_from(server) {
                    Ok(server) => servers.push(server),
                    Err(error) => warn!(
                        server = %name,
                        error = %error,
                        "MCP server entry skipped; fix the MCP config to use this server"
                    ),
                }
            }
            if servers.is_empty() {
                info!("no enabled MCP servers available; MCP tools disabled");
            }
            McpManager::with_config_path(servers, Some(path))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn stdio_server(name: &str, command: &str) -> ValidatedServer {
        ValidatedServer {
            name: name.to_string(),
            segment: name.to_string(),
            transport: crate::transport::ValidTransport::Stdio {
                command: command.to_string(),
                args: Vec::new(),
                env: std::collections::HashMap::new(),
            },
            call_timeout: std::time::Duration::from_secs(5),
            visible_to: None,
            tool_visibility: std::collections::BTreeMap::new(),
            deferred_threshold: None,
        }
    }

    fn write_config(path: &Path, servers: &str) {
        std::fs::write(path, format!(r#"{{"mcpServers":{{{servers}}}}}"#)).expect("write config");
    }

    #[test]
    fn new_manager_has_disconnected_status_per_server() {
        let manager = McpManager::new(vec![stdio_server("alpha", "npx")]);
        assert_eq!(manager.configured_servers(), 1);
        assert_eq!(manager.server_specs()[0].name, "alpha");
        assert_eq!(
            manager.server_summary("alpha").expect("summary").transport,
            "stdio"
        );
        assert!(manager.server_summary("missing").is_none());
    }

    #[tokio::test]
    async fn reload_without_runtime_records_a_clear_failure() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("mcp.json");
        write_config(&path, r#""alpha":{"command":"npx"}"#);
        let manager = McpManager::from_config_path(path.clone()).expect("manager");
        assert_eq!(manager.connection_stamp("alpha").await, None);

        write_config(
            &path,
            r#""alpha":{"command":"npx"},"beta":{"command":"node"}"#,
        );
        let report = manager.reload_from_disk().await.expect("reload");
        assert!(!report.runtime_attached);
        assert_eq!(report.kept, ["alpha"]);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].name, "beta");
        assert!(report.failed[0].error.contains("runtime is not attached"));

        assert_eq!(manager.configured_servers(), 2);
        let status = manager
            .statuses()
            .await
            .into_iter()
            .find(|status| status.name == "beta")
            .expect("beta status");
        assert!(!status.connected);
        assert!(
            status
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("runtime is not attached"))
        );
        assert_eq!(manager.tools("beta").await, Some(Vec::new()));
        assert!(manager.registry().names().await.is_empty());
    }

    #[tokio::test]
    async fn reload_rejects_an_invalid_file_without_changing_the_manager() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("mcp.json");
        write_config(&path, r#""alpha":{"command":"npx"}"#);
        let manager = McpManager::from_config_path(path.clone()).expect("manager");

        std::fs::write(&path, "{not json").expect("break config");
        assert!(manager.reload_from_disk().await.is_err());
        assert_eq!(manager.configured_servers(), 1);
        assert_eq!(manager.server_specs()[0].name, "alpha");
        assert_eq!(manager.statuses().await.len(), 1);
    }

    #[tokio::test]
    async fn removing_a_server_removes_its_status_and_tools() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("mcp.json");
        write_config(&path, r#""alpha":{"command":"npx"}"#);
        let manager = McpManager::from_config_path(path.clone()).expect("manager");
        write_config(&path, "");
        let report = manager.reload_from_disk().await.expect("reload");
        assert_eq!(report.disconnected, ["alpha"]);
        assert!(manager.statuses().await.is_empty());
        assert_eq!(manager.tools("alpha").await, None);
    }

    #[tokio::test]
    async fn duplicate_names_are_refused_before_anything_changes() {
        let manager = McpManager::new(vec![stdio_server("alpha", "npx")]);
        let duplicate = vec![stdio_server("alpha", "npx"), stdio_server("alpha", "node")];
        assert!(manager.reload_servers(duplicate).await.is_err());
        assert_eq!(manager.configured_servers(), 1);
        assert_eq!(
            manager.server_specs()[0].transport,
            stdio_server("alpha", "npx").transport
        );
    }
}
