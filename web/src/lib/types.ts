/**
 * Typed DTOs mirroring the serde wire format the DHI server emits.
 *
 * Sources of truth (checked against `cc2c3dc`):
 * - Route/status contract: `crates/dhi-server/src/lib.rs`.
 * - Shapes: `crates/dhi-core/src/lib.rs` (domain DTOs, `snake_case` enums)
 *   and `crates/dhi-server/src/lib.rs` (handler envelopes, flattened
 *   `ApprovalItem`, `ProfileDto`, `SettingsView`).
 * - Pool views: `crates/dhi-agent/src/pool.rs` (`ChainEntry`/`PoolStatus`).
 * - Management rows: `crates/dhi-store/src/mgmt.rs` (`GuardRuleRow`).
 *
 * Conventions:
 * - Field names are the exact serde-emitted snake_case spellings.
 * - Enums are the lowercase string literals `#[serde(rename_all =
 *   "snake_case")]` / `"lowercase"` produce.
 * - Timestamps are RFC 3339 strings with VARIABLE sub-second precision:
 *   `chrono` serializes fractional seconds only when nonzero
 *   (`2026-10-30T00:00:00.123456789Z` vs `2026-10-30T00:00:00Z`), and the
 *   management routes (`/api/llm/profiles`, `/api/intercept/rules`) emit
 *   `to_rfc3339()` output spelled with a `+00:00` offset instead of `Z`.
 *   Never slice these strings positionally.
 * - Free-form JSON arrives as objects and is typed `JsonObject` below; the
 *   two documented producer conventions live on `Fact.provenance` and
 *   `Activity.payload`.
 */

/** A free-form JSON object, as `serde_json::Value` objects come over the wire. */
export type JsonObject = { [key: string]: unknown };

// ---------------------------------------------------------------------------
// Enum unions (every value the Rust enums can serialize)
// ---------------------------------------------------------------------------

/** `dhi_core::TaskState` — 7 values. */
export type TaskState =
  | "created"
  | "running"
  | "paused"
  | "done"
  | "failed"
  | "timeout"
  | "cancelled";

/** `dhi_core::GoalState` — 3 values. */
export type GoalState = "open" | "met" | "abandoned";

/** `dhi_core::IntentState` — 8 values. */
export type IntentState =
  | "open"
  | "running"
  | "paused"
  | "done"
  | "blocked"
  | "exhausted"
  | "stopped"
  | "deleted";

/** `dhi_core::FactState` — 3 values. */
export type FactState = "origin" | "confirmed" | "dismissed";

/** `dhi_core::ResultStatus` — 3 values. */
export type ResultStatus = "pending" | "accepted" | "rejected";

/**
 * `dhi_core::GuardVerdict` — 3 values. Serves as a guard rule's `action`
 * and as `app_settings.guard_default_action` (the verdict applied to tool
 * calls no rule matches).
 */
export type GuardAction = "allow" | "deny" | "ask";

/** `dhi_core::ApprovalStatus` — 4 values. */
export type ApprovalStatus = "pending" | "granted" | "denied" | "expired";

/**
 * Task-level execution topology chosen at creation. `null`/omitted means the
 * planner decides; the planner's eventual choice also appears as
 * `topology` on the latest `team_run` activity.
 */
export type TaskTopology = "single" | "manager_worker" | "swarm";

/** `dhi_core::ApprovalDecision` — the body of `POST /api/approvals/{id}/decision`. */
export type ApprovalDecision = "allow" | "deny";

/** `dhi_core::AgentRole` — 4 values (no `AgentKind` exists in core). */
export type AgentRole = "planner" | "worker" | "reporter" | "digest";

/**
 * Completion-call outcome for `AgentRun.status`. The core stores a plain
 * string; `dhi-core` documents exactly these 8 values.
 */
export type AgentRunStatus =
  | "running"
  | "ok"
  | "error"
  | "exhausted"
  | "timed_out"
  | "truncated"
  | "no_output"
  | "aborted";

/** Wire format of an LLM profile (`dhi_agent::ProfileFormat`, 2 values). */
export type ProfileFormat = "openai" | "anthropic";

/**
 * Optional provider thinking control. `""` means the field is not sent;
 * `disabled`/`enabled` map to the provider's thinking switch.
 */
export type ThinkingType = "" | "disabled" | "enabled";

/**
 * Optional reasoning-effort control. `""` means the field is not sent;
 * the remaining values are sent verbatim.
 */
export type ReasoningEffort = "" | "low" | "medium" | "high" | "xhigh" | "max";

/** What a guard rule matches against — 2 values. */
export type MatchTarget = "tool_name" | "tool_input";

/** How a guard rule matches its pattern — 2 values. */
export type MatchType = "exact" | "regex";

/**
 * The two roles the server resolves pools/settings/activations for. Wider
 * than nothing narrower than `AgentRole` because `POST
 * /api/llm/profiles/active` answers `400` for anything but these two.
 */
export type LlmRole = "planner" | "worker";

// ---------------------------------------------------------------------------
// Domain entities (serde shapes of `dhi-core`)
// ---------------------------------------------------------------------------

/** `GET /api/health`. */
export type Health = {
  status: string;
  service: string;
  version: string;
};

/**
 * Per-task capacity ledger. Each field is independently nullable: `null`
 * means that dimension keeps the engine default. The server only stores
 * positive values (`POST /api/tasks/{id}/capacity` rejects `<= 0`).
 */
export type TaskCapacity = {
  max_rounds: number | null;
  max_model_calls: number | null;
  max_wall_secs: number | null;
  max_parallel_workers: number | null;
};

/**
 * `POST /api/tasks/{id}/capacity` body. Omitted fields keep the stored
 * value; there is no way to clear a limit back to `null` through the API.
 */
export type TaskCapacityPatch = Partial<{
  max_rounds: number;
  max_model_calls: number;
  max_wall_secs: number;
  max_parallel_workers: number;
}>;

export type Task = {
  id: string;
  goal: string;
  state: TaskState;
  exploration_id: string;
  origin_fact_id: string;
  /** `llm_profiles.id` this task's planner role is pinned to, or `null`. */
  pinned_planner_profile_id: number | null;
  /** `llm_profiles.id` this task's worker role is pinned to, or `null`. */
  pinned_worker_profile_id: number | null;
  /**
   * Capacity limits. Older servers omit the field entirely; every limit is
   * then treated as the engine default.
   */
  capacity?: TaskCapacity;
  /**
   * Execution topology pinned at creation (`single`, `manager_worker`,
   * `swarm`), or `null`/omitted when the planner decides. Older servers omit
   * the field; the latest `team_run` activity is the fallback signal.
   */
  topology?: TaskTopology | null;
  created_at: string;
  updated_at: string;
};

export type Goal = {
  id: string;
  task_id: string;
  description: string;
  state: GoalState;
  /** The confirmed fact that proved this goal, once a planner proves it. */
  proof_fact_id: string | null;
  created_at: string;
};

export type Intent = {
  id: string;
  task_id: string;
  goal_id: string | null;
  fact_id: string;
  description: string;
  state: IntentState;
  /** The worker that claimed the intent, or `null` while unclaimed. */
  owner: string | null;
  priority: number;
  created_at: string;
  completed_at: string | null;
};

export type Fact = {
  id: string;
  task_id: string;
  claim: string;
  state: FactState;
  /**
   * Free-form provenance object with two producer conventions:
   * - origin fact (written by task creation): `{ "goal": <task goal> }`;
   * - worker fact (written on intent completion): `{ "intent_id": <uuid>,
   *   "detail": <string>, "evidence": <string>, "confidence": <string> }`.
   */
  provenance: JsonObject;
  /** The fact this fact refines, for chained observations. */
  origin_of: string | null;
  created_at: string;
};

export type ResultRecord = {
  id: string;
  task_id: string;
  intent_id: string;
  claim: string;
  acceptance: ResultStatus;
  /** Free-form JSON supporting the acceptance decision. */
  evidence: JsonObject;
  created_at: string;
};

export type Activity = {
  /** Append-only cursor; also the SSE `id:` field. */
  id: number;
  task_id: string;
  /** Free-form kind string (`task_created`, `intent_claimed`, …). */
  kind: string;
  message: string;
  /**
   * Free-form payload object, convention-dependent:
   * - `tool_allowed`: `{ "intent_id": ..., "tool": ..., "input": ...,
   *   "output": ..., "worker_key"?: ... }` — one row per tool call; `input`
   *   is absent on older rows;
   * - `tool_denied`: `{ "intent_id": ..., "tool": ..., "reason": ... }`;
   * - `capacity_exhausted`: `{ "reason": "model_calls" | "wall_time" |
   *   "rounds", "rounds": ..., "model_calls": ..., "elapsed_secs": ...,
   *   "max_model_calls": ..., "max_wall_secs": ..., "max_rounds": ... }`;
   * - `approval_denied`: `{ "approval_id": ..., "tool_call_id": ...,
   *   "reason": ... }`; workers/steps attach their own extras.
   */
  payload: JsonObject;
  created_at: string;
};

/**
 * One typed broadcast persisted by a DHI+ swarm run. `kind` is one of
 * `finding`, `question`, `challenge`, `done`; `payload` is the serialized
 * `SwarmMessage` (`{ type, from, ... }`). Rows are append-only audit data,
 * ordered by ascending `id`.
 */
export type SwarmMessageRecord = {
  id: number;
  task_id: string;
  intent_id: string;
  kind: string;
  payload: JsonObject;
  created_at: string;
};

/** The durable approval row (`dhi_core::Approval`), unenriched. */
export type Approval = {
  id: string;
  task_id: string;
  tool_call_id: string;
  guard_decision_id: string;
  status: ApprovalStatus;
  reason: string;
  expires_at: string;
  decided_at: string | null;
  created_at: string;
};

/**
 * One row of `GET /api/approvals`: the flattened `Approval` fields plus the
 * tool name and input of the linked `tool_calls` row — both `null` when the
 * linked tool call row is missing (the server flattens, so there is no
 * nested `approval` key).
 */
export type ApprovalItem = Approval & {
  tool_name: string | null;
  /** Any JSON value; `null` when the linked tool call row is missing. */
  input: unknown | null;
};

export type AgentRun = {
  /** Durable row identity: one row per completion call. */
  id: string;
  /** The agent run this call belongs to; retries share it. */
  run_id: string;
  /** Position of this call inside the run, from 0. */
  call_index: number;
  task_id: string;
  intent_id: string | null;
  role: AgentRole;
  model: string;
  /**
   * Secret-free fingerprint
   * `<format>/<model>/max_tokens=<n>/stream=<bool>|<name>#<id>`.
   */
  profile: string;
  prompt: string;
  prompt_hash: string;
  /** The typed variables the prompt was rendered from. */
  variables: JsonObject;
  response: string | null;
  /** Provider-returned thinking text, when the profile requested it. */
  thinking: string | null;
  tokens_in: number;
  tokens_out: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  reasoning_tokens: number;
  /** Provider-reported USD only; `0` when the provider is silent. */
  cost: number;
  status: AgentRunStatus;
  error: string | null;
  latency_ms: number;
  checkpoint_id: string | null;
  created_at: string;
  updated_at: string;
};

// ---------------------------------------------------------------------------
// Side questions
// ---------------------------------------------------------------------------
/** `side_question_sessions.status`. */
export type SideQuestionSessionStatus = "open" | "closed";

/** `side_question_requests.status`. */
export type SideQuestionRequestStatus = "pending" | "ok" | "error";

/**
 * `GET /api/tasks/{id}/side-questions` — the short view of the frozen
 * snapshot. The full snapshot is never sent by the list route.
 */
export type SideQuestionSnapshotSummary = {
  goal: string;
  state: string;
  goals: number;
  facts: number;
  activity: number;
};

/** One persisted question/answer attempt. */
export type SideQuestionRequest = {
  id: string;
  session_id: string;
  question: string;
  answer: string | null;
  status: SideQuestionRequestStatus;
  error: string | null;
  model: string | null;
  tokens_in: number | null;
  tokens_out: number | null;
  latency_ms: number | null;
  created_at: string;
  completed_at: string | null;
};

/**
 * One side-question session, newest first in the list response. A locally
 * created session (after a successful ask) may not have a summary yet, so
 * `snapshot_summary` is nullable.
 */
export type SideQuestionSession = {
  id: string;
  created_at: string;
  closed_at: string | null;
  status: SideQuestionSessionStatus;
  snapshot_summary: SideQuestionSnapshotSummary | null;
  requests: SideQuestionRequest[];
};

// ---------------------------------------------------------------------------
// Management DTOs (llm profiles, guard rules, settings, pool)
// ---------------------------------------------------------------------------

export type RoleSummary = {
  id: number;
  name: string;
  model: string;
};

/**
 * Read shape of one LLM profile (server `ProfileDto`). `api_key` is
 * WRITE-ONLY: no read ever carries it, only the `api_key_hint`
 * (`…last4`, empty when the stored key is shorter than four characters).
 * There are no role-pointer/`is_default` fields — activation lives in
 * `app_settings` and surfaces via `/api/settings`.
 */
export type LlmProfile = {
  id: number;
  name: string;
  format: ProfileFormat;
  base_url: string;
  model: string;
  api_key_hint: string;
  max_tokens: number;
  context_window_k: number;
  /** Empty means the provider-specific thinking field is not sent. */
  thinking_type: ThinkingType;
  /** Empty means the provider-specific effort field is not sent. */
  reasoning_effort: ReasoningEffort;
  streaming: boolean;
  priority: number;
  pool_exclude: boolean;
  created_at: string;
  updated_at: string;
};

/**
 * One guard rule (`dhi_store::GuardRuleRow`), in evaluation order
 * (priority DESC, then id ASC) when listed.
 */
export type GuardRule = {
  id: number;
  name: string;
  match_target: MatchTarget;
  match_type: MatchType;
  pattern: string;
  action: GuardAction;
  reason: string;
  priority: number;
  enabled: boolean;
  /**
   * Per-rule ask-approval budget in seconds; `0` falls back to
   * `app_settings.approval_timeout_secs`.
   */
  timeout_secs: number;
  created_at: string;
  updated_at: string;
};

/**
 * `GET`/`PUT /api/settings` view (server `SettingsView`). Always `200`:
 * before the first profile exists the roles are `null`, hints empty,
 * `llm_key_set: false`, and the defaults `approval_timeout_secs: 900` /
 * `guard_default_action: "deny"` are reported.
 */
export type AppSettingsView = {
  planner_profile: RoleSummary | null;
  worker_profile: RoleSummary | null;
  /** True when BOTH active profiles carry a non-empty key. */
  llm_key_set: boolean;
  planner_api_key_hint: string;
  worker_api_key_hint: string;
  approval_timeout_secs: number;
  guard_default_action: GuardAction;
};

/** One chain entry: identity plus breaker state; no secrets. */
export type ChainEntry = {
  id: number;
  name: string;
  model: string;
  /** Tripped profiles are skipped by resolution until a reset. */
  tripped: boolean;
  consecutive_failures: number;
};

export type RolePoolStatus = {
  chain: ChainEntry[];
};

/**
 * `GET /api/llm/pool/status` (and the response of `POST
 * /api/llm/pool/reset`). Chain order: the role's active default first, then
 * every other non-excluded profile by priority DESC / id ASC.
 */
export type PoolStatus = {
  planner: RolePoolStatus;
  worker: RolePoolStatus;
};

/** `GET /api/llm/models` — upstream `data[].id`s, in upstream order. */
export type ModelsResponse = {
  models: string[];
};

// ---------------------------------------------------------------------------
// Web search providers (Settings v2)
// ---------------------------------------------------------------------------

/** The three router providers, in their fixed display order. */
export type WebProviderId = "tinyfish" | "parallel" | "firecrawl";

/** Where the effective configuration for a provider came from. */
export type WebProviderSource =
  | "database"
  | "environment"
  | "config_file"
  | "none";

/** Whether the router can use a provider right now. */
export type WebProviderStatus = "ready" | "missing_key" | "disabled";

/**
 * One sanitized web provider (`GET /api/web/providers` `providers[]`).
 * `api_key` is WRITE-ONLY: reads carry only `key_hint` (null or `…last4`).
 */
export type WebProviderView = {
  provider: WebProviderId;
  display_name: string;
  configured: boolean;
  key_hint: string | null;
  source: WebProviderSource;
  /** Effective base URL, or `null` when the provider has no override. */
  base_url: string | null;
  timeout_secs: number;
  enabled: boolean;
  status: WebProviderStatus;
};

/** `GET /api/web/providers` / `POST /api/web/providers/reload`. */
export type WebProvidersResponse = {
  search_read_order: WebProviderId[];
  research_order: WebProviderId[];
  /** Fixed order: tinyfish, parallel, firecrawl. */
  providers: WebProviderView[];
};

/**
 * `PUT /api/web/providers/{provider}` body — every field optional.
 * A blank/omitted `api_key` keeps the stored key (DELETE clears it);
 * an empty `base_url` clears the override and restores the default.
 */
export type WebProviderUpdate = {
  api_key?: string;
  base_url?: string;
  timeout_secs?: number;
  enabled?: boolean;
};

/** `POST /api/web/providers/{provider}/test` success payload. */
export type WebProviderTestResult = {
  ok: true;
  provider: WebProviderId;
  message: string;
  latency_ms: number;
};

// ---------------------------------------------------------------------------
// Tool catalog and control
// ---------------------------------------------------------------------------

/**
 * Which source owns a tool (`dhi_tool_catalog::ToolSource`, snake_case).
 * The catalog is built at server startup, so MCP tools added through a live
 * reload do not appear here until the next process start.
 */
export type ToolSource =
  | "builtin"
  | "web"
  | "coding"
  | "pty"
  | "mcp"
  | "skills"
  | "custom";

/**
 * One `GET /api/tools/catalog` row (`ToolCatalogEntry`). Control overrides
 * are already merged by key when a control store is available: `enabled` and
 * `default_roles` reflect the persisted state, not only the startup defaults.
 *
 * An empty `default_roles` list means EVERY recognized role (`planner`,
 * `worker`, `reporter`, `digest`, and any `specialist:<name>`). It never
 * means "visible to nobody".
 */
export type ToolCatalogEntry = {
  key: string;
  source: ToolSource;
  description: string;
  /** The catalog serializes this field as camelCase `inputSchema`. */
  inputSchema: JsonObject;
  default_roles: string[];
  enabled: boolean;
  deferred: boolean;
  /** Source-specific, non-secret descriptive metadata. */
  metadata: JsonObject;
};

/** One catalog source that failed while the snapshot was built. */
export type CatalogSourceError = {
  source: string;
  error: string;
};

/**
 * `GET /api/tools/catalog`. `control_error` is non-null when the catalog
 * itself loaded but the control store failed; all write routes will answer
 * `503` in that state.
 */
export type ToolCatalogResponse = {
  tools: ToolCatalogEntry[];
  errors: CatalogSourceError[];
  control_error: string | null;
};

/** Persisted enabled/visibility state for one tool. */
export type ToolState = {
  key: string;
  enabled: boolean;
  roles: string[];
};

/** `GET /api/tools/control` / `POST /api/tools/control` response envelope. */
export type ToolControlResponse = {
  tools: ToolState[];
};

/**
 * One `POST /api/tools/control` action, tagged by `action`. Only `set_enabled`
 * and `set_visibility` are used by the ops UI.
 */
export type ToolControlAction =
  | { action: "set_enabled"; key: string; enabled: boolean }
  | { action: "set_visibility"; key: string; roles: string[] };

// ---------------------------------------------------------------------------
// MCP servers (live management)
// ---------------------------------------------------------------------------

/** One discovered MCP tool in a server status (no schema). */
export type McpToolInfo = {
  name: string;
  description: string;
};

/** One server's live health record (`GET /api/mcp/servers` `servers[]`). */
export type McpServerStatus = {
  name: string;
  /** `"stdio"`, `"http"`, or legacy `"sse"`. */
  transport: string;
  enabled: boolean;
  connected: boolean;
  tools: McpToolInfo[];
  last_error: string | null;
  call_timeout_secs: number;
};

/**
 * One sanitized configured server (`GET /api/mcp/servers` `config[]`).
 * Header VALUES are already masked (`"***"` for secret headers) by the
 * server; non-secret values may be present. The UI still renders header
 * names only.
 */
export type McpServerSummary = {
  name: string;
  transport: string;
  enabled: boolean;
  /** HTTP/SSE URL or the stdio command line (command + args joined). */
  target: string;
  timeout_secs: number;
  /** Omitted when the server has no headers. */
  headers?: Record<string, string>;
};

/** `GET /api/mcp/servers` — statuses plus optional masked config. */
export type McpServersResponse = {
  servers: McpServerStatus[];
  config?: McpServerSummary[];
};

/** One server that failed to connect during a live reload. */
export type McpReloadFailure = {
  name: string;
  error: string;
};

/** `ReloadReport` returned by every MCP write and `POST /api/mcp/reload`. */
export type McpReloadReport = {
  kept: string[];
  connected: string[];
  disconnected: string[];
  reconnected: string[];
  failed: McpReloadFailure[];
  runtime_attached: boolean;
  configured: number;
};

/** `POST /api/mcp/servers` and `PATCH /api/mcp/servers/{name}` response. */
export type McpServerWriteResponse = {
  server: McpServerSummary;
  reload: McpReloadReport;
};

/** `DELETE /api/mcp/servers/{name}` response. */
export type McpServerDeleteResponse = {
  deleted: string;
  reload: McpReloadReport;
};

/** `POST /api/mcp/servers/{name}/refresh` response. */
export type McpServerRefreshResponse = {
  server: string;
  tools: McpToolInfo[];
};

/** `POST /api/mcp/reload` response. */
export type McpReloadResponse = {
  reload: McpReloadReport;
};

/**
 * `POST /api/mcp/servers` body and `PATCH /api/mcp/servers/{name}` body.
 * The write routes replace the whole entry: known fields not sent are reset
 * to their defaults, so callers must include every field they want to keep.
 * `env`, `visible_to`, `tool_visibility`, and `deferred_threshold` are
 * accepted for completeness, but the list summary does not expose them, so
 * the UI only sends the fields it can construct.
 */
export type McpTransportKind = "stdio" | "http" | "sse";

export type McpServerInput = {
  name?: string;
  transport?: McpTransportKind;
  command?: string;
  args?: string[];
  env?: Record<string, string>;
  url?: string;
  headers?: Record<string, string>;
  timeout_secs?: number;
  insecure?: boolean;
  enabled?: boolean;
  visible_to?: string[] | null;
  tool_visibility?: Record<string, string[]>;
  deferred_threshold?: number;
};

// ---------------------------------------------------------------------------
// Response envelopes (exactly as the handlers wrap)
// ---------------------------------------------------------------------------

export type TaskListResponse = {
  tasks: Task[];
};

/** `POST /api/tasks/{id}/stop` — accepted, engine work stopping. */
export type StopTaskResponse = {
  task_id: string;
  status: "stopping";
};

/** `POST /api/tasks/stop` — per-task outcomes for a bulk stop request. */
export type StopTasksResponse = {
  stopping: string[];
  blocked: { id: string; state: string; error: string }[];
};

/** `POST /api/tasks/delete` — per-task outcomes for a bulk delete request. */
export type DeleteTasksResponse = {
  deleted: string[];
  blocked: { id: string; state: string; error: string }[];
};

/**
 * `GET /api/tasks/{id}/work` — the task plus its work-graph collections.
 * There is no `digests` collection: the handler serves exactly
 * `task`, `goals`, `intents`, `facts`, `results`.
 */
export type TaskWork = {
  task: Task;
  goals: Goal[];
  intents: Intent[];
  facts: Fact[];
  results: ResultRecord[];
};

/** `GET /api/tasks/{id}/activity` — rows ordered by `id` ascending. */
export type TaskActivityResponse = {
  activity: Activity[];
};

/** `GET /api/tasks/{id}/swarm-messages` — rows ordered by `id` ascending. */
export type SwarmMessagesResponse = {
  messages: SwarmMessageRecord[];
};

/** `GET /api/tasks/{id}/usage` — completion calls ordered by `(created_at, call_index)`. */
export type TaskUsageResponse = {
  runs: AgentRun[];
};

/** `GET /api/tasks/{id}/side-questions` — newest session first. */
export type SideQuestionSessionsResponse = {
  sessions: SideQuestionSession[];
};

/**
 * `POST /api/tasks/{id}/goals` — the terminal task is reopened to `running`
 * and the new open goal is returned with it.
 */
export type AddGoalResponse = {
  task: Task;
  goal: Goal;
};

/** `POST /api/tasks/{id}/side-questions` — the new request plus its answer. */
export type AskSideQuestionResponse = {
  session_id: string;
  request: SideQuestionRequest;
  answer: string;
};

/** `GET /api/approvals` — pending approvals, oldest first. */
export type ApprovalsResponse = {
  approvals: ApprovalItem[];
};

/** `GET /api/llm/profiles` — every profile, id order. */
export type ProfilesResponse = {
  profiles: LlmProfile[];
};

/** `GET /api/intercept/rules` — every rule, evaluation order. */
export type RulesResponse = {
  rules: GuardRule[];
};

/**
 * `POST /api/approvals/{id}/decision` — NOTE: the `approval` here is the
 * plain core row WITHOUT the enrichment `GET /api/approvals` adds; only the
 * list endpoint carries `tool_name`/`input`.
 */
export type ApprovalDecisionResponse = {
  approval: Approval;
  task: Task;
};

/** `POST /api/llm/profiles` — id of the created or edited profile. */
export type SaveProfileResponse = {
  id: number;
};

/** `DELETE /api/llm/profiles/{id}` and `DELETE /api/intercept/rules/{id}`. */
export type DeleteResponse = {
  deleted: number;
};

/** `POST /api/llm/profiles/active` — bare acknowledgment. */
export type OkResponse = {
  ok: true;
};

/** `POST /api/intercept/rules/{id}/toggle`. */
export type ToggleRuleResponse = {
  ok: true;
  enabled: boolean;
};

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

/**
 * `POST /api/tasks` body. `goal` is the origin input (there is no `title`
 * field). The pin spellings `planner_profile_id`/`worker_profile_id` are
 * canonical; the server also accepts the `pinned_*` column-name aliases.
 * A pin must name an existing profile (`400` otherwise) and protects it
 * from deletion (`409`).
 */
export type CreateTaskInput = {
  goal: string;
  planner_profile_id?: number | null;
  worker_profile_id?: number | null;
  capacity?: TaskCapacity;
  /**
   * `null` (or omitted) lets the planner decide; otherwise pins the task to
   * one execution topology.
   */
  topology?: TaskTopology | null;
};

/**
 * `POST /api/llm/profiles` body: create (`id` absent) or edit (`id`
 * present). `api_key` is write-only and OPTIONAL — an absent/empty key on
 * edit keeps the stored key and hint; it is never echoed back. Absent
 * numeric/boolean fields take the server defaults (`max_tokens: 0`,
 * `context_window_k: 0`, `thinking_type: ""`, `reasoning_effort: ""`,
 * `streaming: true`, `priority: 0`, `pool_exclude: false`). `name`,
 * `format`, and `model` are required.
 */
export type SaveProfileInput = {
  id?: number | null;
  name: string;
  format: ProfileFormat;
  base_url?: string;
  model: string;
  api_key?: string;
  max_tokens?: number;
  context_window_k?: number;
  thinking_type?: ThinkingType;
  reasoning_effort?: ReasoningEffort;
  streaming?: boolean;
  priority?: number;
  pool_exclude?: boolean;
};

/**
 * `POST /api/intercept/rules` and `PUT /api/intercept/rules/{id}` body
 * (full replace). `regex` patterns must compile (`400` with the regex
 * error otherwise); `tool_input` `exact` matching is case-insensitive
 * substring containment against the serialized input JSON.
 */
export type GuardRuleInput = {
  name: string;
  match_target: MatchTarget;
  match_type: MatchType;
  pattern: string;
  action: GuardAction;
  reason?: string;
  priority?: number;
  enabled?: boolean;
  timeout_secs?: number;
};

/**
 * `PUT /api/settings` body — partial merge. Omitted fields keep their
 * stored value; `null` role ids behave the same as omission (the server
 * folds both to "unset/keep"), `approval_timeout_secs` must be `>= 1`
 * (`400` otherwise), and unknown profile references are `404`.
 */
export type SettingsPatch = {
  planner_profile_id?: number | null;
  worker_profile_id?: number | null;
  approval_timeout_secs?: number;
  guard_default_action?: GuardAction;
};

/**
 * `POST /api/llm/pool/reset` body: `id: 0` (or an absent id) resets every
 * profile's breaker; `id: N` resets one.
 */
export type PoolResetInput = {
  id?: number;
};
