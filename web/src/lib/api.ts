import type {
  Activity,
  AddGoalResponse,
  AgentRun,
  ApprovalDecision,
  ApprovalDecisionResponse,
  ApprovalItem,
  ApprovalsResponse,
  AppSettingsView,
  AskSideQuestionResponse,
  CreateTaskInput,
  DeleteResponse,
  DeleteTasksResponse,
  GuardRule,
  GuardRuleInput,
  Health,
  Instruction,
  InstructionsResponse,
  LlmProfile,
  McpReloadReport,
  McpReloadResponse,
  McpServerDeleteResponse,
  McpServerInput,
  McpServerRefreshResponse,
  McpServersResponse,
  McpServerWriteResponse,
  ModelsResponse,
  OkResponse,
  PoolResetInput,
  PoolStatus,
  ProfilesResponse,
  ProjectFolder,
  ProjectFoldersResponse,
  RulesResponse,
  SaveProfileInput,
  SaveProfileResponse,
  SettingsPatch,
  SideQuestionSession,
  SideQuestionSessionsResponse,
  StopTaskResponse,
  StopTasksResponse,
  SwarmMessageRecord,
  SwarmMessagesResponse,
  Task,
  TaskActivityResponse,
  TaskCapacityPatch,
  TaskListResponse,
  TaskUsageResponse,
  TaskWork,
  ToggleRuleResponse,
  ToolCatalogResponse,
  ToolControlAction,
  ToolControlResponse,
  ToolState,
  WebProviderId,
  WebProviderTestResult,
  WebProviderUpdate,
  WebProviderView,
  WebProvidersResponse,
} from "./types";

/**
 * One client-side API failure. `status` is the HTTP status for non-2xx
 * responses; `0` covers network and malformed-response failures.
 */
export class ApiError extends Error {
  readonly status: number;

  constructor(status: number, message: string) {
    super(message);
    this.name = "ApiError";
    this.status = status;
  }

  /** `400 Bad Request` — malformed body, bad enum, bad query parameter. */
  isValidation(): boolean {
    return this.status === 400;
  }

  /** `404 Not Found` — unknown task/approval/profile/rule/settings target. */
  isNotFound(): boolean {
    return this.status === 404;
  }

  /** `409 Conflict` — non-pending approval, in-use profile, …. */
  isConflict(): boolean {
    return this.status === 409;
  }
}

/**
 * Derives the thrown message from an error response: the server's
 * `{ "error": "message" }` envelope when present, otherwise the raw body
 * (trimmed) when it has content, otherwise a plain status line.
 */
async function errorMessage(response: Response): Promise<string> {
  let text = "";
  try {
    text = await response.text();
  } catch {
    // Unreadable body; fall through to the status line.
  }
  if (text !== "") {
    try {
      const parsed = JSON.parse(text) as { error?: unknown };
      if (typeof parsed?.error === "string" && parsed.error.trim() !== "") {
        return parsed.error;
      }
    } catch {
      // Not JSON — carry the raw body as the message below.
    }
    const trimmed = text.trim();
    if (trimmed !== "") {
      return trimmed.length > 300 ? trimmed.slice(0, 300) : trimmed;
    }
  }
  return `HTTP ${response.status}`;
}

/**
 * The one fetch helper: relative `/api/...` (same-origin; the dev server
 * proxies it), JSON in and out, non-2xx and network/parse failures thrown
 * as `ApiError`.
 */
async function request<T>(path: string, init?: RequestInit): Promise<T> {
  let response: Response;
  try {
    response = await fetch(`/api${path}`, {
      ...init,
      headers: {
        "content-type": "application/json",
        ...init?.headers,
      },
    });
  } catch (cause) {
    const message = cause instanceof Error ? cause.message : String(cause);
    throw new ApiError(0, `network error calling /api${path}: ${message}`);
  }

  if (!response.ok) {
    throw new ApiError(response.status, await errorMessage(response));
  }

  // `204 No Content` and `205 Reset Content` carry no body; parsing them as
  // JSON would turn a successful delete into a client-side failure.
  if (response.status === 204 || response.status === 205) {
    return undefined as T;
  }

  try {
    return (await response.json()) as T;
  } catch {
    throw new ApiError(0, `malformed JSON response from /api${path}`);
  }
}

// ---------------------------------------------------------------------------
// Tasks, work, activity, usage, approvals
// ---------------------------------------------------------------------------

export function getHealth(): Promise<Health> {
  return request<Health>("/health");
}

/** Unwraps the `{ folders: [...] }` envelope of `GET /api/project-folders`. */
export function listProjectFolders(): Promise<ProjectFolder[]> {
  return request<ProjectFoldersResponse>("/project-folders").then(
    (response) => response.folders,
  );
}

export function listTasks(opts?: { limit?: number }): Promise<Task[]> {
  const path =
    opts?.limit === undefined ? "/tasks" : `/tasks?limit=${opts.limit}`;
  return request<TaskListResponse>(path).then((response) => response.tasks);
}

export function createTask(input: CreateTaskInput): Promise<Task> {
  return request<Task>("/tasks", {
    method: "POST",
    body: JSON.stringify({ ...input }),
  });
}

export function getTask(id: string): Promise<Task> {
  return request<Task>(`/tasks/${id}`);
}

/** `202` means the stop was accepted and the engine work is winding down. */
export function stopTask(id: string): Promise<StopTaskResponse> {
  return request<StopTaskResponse>(`/tasks/${id}/stop`, {
    method: "POST",
  });
}

/**
 * Stops every listed task. The response reports which tasks are stopping and
 * which were blocked (with the blocking state and message).
 */
export function stopTasks(ids: string[]): Promise<StopTasksResponse> {
  return request<StopTasksResponse>("/tasks/stop", {
    method: "POST",
    body: JSON.stringify({ ids }),
  });
}

/** Deletes one task; the server answers `204 No Content`. */
export function deleteTask(id: string): Promise<void> {
  return request<void>(`/tasks/${id}`, {
    method: "DELETE",
  });
}

/** Deletes every listed task and reports per-task outcomes. */
export function deleteTasks(ids: string[]): Promise<DeleteTasksResponse> {
  return request<DeleteTasksResponse>("/tasks/delete", {
    method: "POST",
    body: JSON.stringify({ ids }),
  });
}

/**
 * Adds a goal to a terminal task and reopens it
 * (`done|failed|timeout|cancelled` → `running`). `409` means the task is not
 * terminal; `400` is invalid text.
 */
export function addGoal(
  taskId: string,
  description: string,
): Promise<AddGoalResponse> {
  return request<AddGoalResponse>(`/tasks/${taskId}/goals`, {
    method: "POST",
    body: JSON.stringify({ description }),
  });
}

export function getTaskWork(id: string): Promise<TaskWork> {
  return request<TaskWork>(`/tasks/${id}/work`);
}

/**
 * Extends or lowers one task's capacity ledger. Omitted fields keep their
 * stored value. A paused task resumes; `409` means the task is terminal and
 * its ledger can no longer be changed.
 */
export function updateTaskCapacity(
  taskId: string,
  patch: TaskCapacityPatch,
): Promise<Task> {
  return request<Task>(`/tasks/${taskId}/capacity`, {
    method: "POST",
    body: JSON.stringify({ ...patch }),
  });
}

/** Unwraps the `{ activity: [...] }` envelope. */
export function getTaskActivity(id: string): Promise<Activity[]> {
  return request<TaskActivityResponse>(`/tasks/${id}/activity`).then(
    (response) => response.activity,
  );
}

/**
 * Unwraps the `{ messages: [...] }` envelope. The server defaults `limit`
 * to 200 and clamps it to `1..=1000`.
 */
export function getSwarmMessages(
  taskId: string,
  limit = 200,
): Promise<SwarmMessageRecord[]> {
  return request<SwarmMessagesResponse>(
    `/tasks/${taskId}/swarm-messages?limit=${limit}`,
  ).then((response) => response.messages);
}

/**
 * Unwraps the `{ instructions: [...] }` envelope. Instructions are
 * planner-only notes; the worker prompts never receive them.
 */
export function listInstructions(taskId: string): Promise<Instruction[]> {
  return request<InstructionsResponse>(
    `/tasks/${taskId}/instructions`,
  ).then((response) => response.instructions);
}

/** Queues one planner instruction and returns the created row. */
export function sendInstruction(
  taskId: string,
  text: string,
): Promise<Instruction> {
  return request<Instruction>(`/tasks/${taskId}/instructions`, {
    method: "POST",
    body: JSON.stringify({ text }),
  });
}

/** Unwraps the `{ runs: [...] }` envelope. */
export function getTaskUsage(id: string): Promise<AgentRun[]> {
  return request<TaskUsageResponse>(`/tasks/${id}/usage`).then(
    (response) => response.runs,
  );
}

// ---------------------------------------------------------------------------
// Side questions (advisory; frozen snapshot; no tools)
// ---------------------------------------------------------------------------

/** Unwraps the `{ sessions: [...] }` envelope (newest session first). */
export function listSideQuestions(
  taskId: string,
): Promise<SideQuestionSession[]> {
  return request<SideQuestionSessionsResponse>(
    `/tasks/${taskId}/side-questions`,
  ).then((response) => response.sessions);
}

/**
 * Asks one advisory question. With `sessionId` the server appends to that
 * open session; without it it opens a new session with a fresh frozen
 * snapshot. `409` means the session closed; `502` means the answerer failed
 * (the failed attempt is still persisted server-side).
 */
export function askSideQuestion(
  taskId: string,
  question: string,
  sessionId?: string,
): Promise<AskSideQuestionResponse> {
  const body =
    sessionId === undefined ? { question } : { question, session_id: sessionId };
  return request<AskSideQuestionResponse>(
    `/tasks/${taskId}/side-questions`,
    {
      method: "POST",
      body: JSON.stringify(body),
    },
  );
}

/** Unwraps the `{ approvals: [...] }` envelope (flattened, enriched rows). */
export function listApprovals(): Promise<ApprovalItem[]> {
  return request<ApprovalsResponse>("/approvals").then(
    (response) => response.approvals,
  );
}

export function decideApproval(
  id: string,
  decision: ApprovalDecision,
): Promise<ApprovalDecisionResponse> {
  return request<ApprovalDecisionResponse>(
    `/approvals/${id}/decision`,
    {
      method: "POST",
      body: JSON.stringify({ decision }),
    },
  );
}

// ---------------------------------------------------------------------------
// LLM profiles, models, failover pool
// ---------------------------------------------------------------------------

/** Unwraps the `{ profiles: [...] }` envelope (no `api_key`, hint only). */
export function listProfiles(): Promise<LlmProfile[]> {
  return request<ProfilesResponse>("/llm/profiles").then(
    (response) => response.profiles,
  );
}

/**
 * Creates (`input.id` absent) or edits (`input.id` present) one profile.
 * An absent/empty `api_key` on edit keeps the stored key; the key is
 * never returned — the next `listProfiles` shows its hint.
 */
export function saveProfile(input: SaveProfileInput): Promise<SaveProfileResponse> {
  return request<SaveProfileResponse>("/llm/profiles", {
    method: "POST",
    body: JSON.stringify({ ...input }),
  });
}

export function deleteProfile(id: number): Promise<DeleteResponse> {
  return request<DeleteResponse>(`/llm/profiles/${id}`, {
    method: "DELETE",
  });
}

/**
 * Lists the upstream model ids. Defaults to the planner's active profile;
 * upstream failures come back as `502` with a redacted snippet.
 */
export function listModels(profileId?: number): Promise<ModelsResponse> {
  const path =
    profileId === undefined
      ? "/llm/models"
      : `/llm/models?profile_id=${profileId}`;
  return request<ModelsResponse>(path);
}

export function getPoolStatus(): Promise<PoolStatus> {
  return request<PoolStatus>("/llm/pool/status");
}

/** Clears breaker state: omit the id (or pass 0) to reset every profile. */
export function resetPool(id?: number): Promise<PoolStatus> {
  return request<PoolStatus>("/llm/pool/reset", {
    method: "POST",
    body: JSON.stringify({ id: id ?? 0 } satisfies PoolResetInput),
  });
}

// ---------------------------------------------------------------------------
// Web search providers (Settings v2)
// ---------------------------------------------------------------------------

export function getWebProviders(): Promise<WebProvidersResponse> {
  return request<WebProvidersResponse>("/web/providers");
}

/**
 * Partial provider update. A blank/omitted `api_key` keeps the stored key;
 * an empty `base_url` clears the override. Returns just that provider's DTO.
 */
export function saveWebProvider(
  provider: WebProviderId,
  input: WebProviderUpdate,
): Promise<WebProviderView> {
  return request<WebProviderView>(
    `/web/providers/${encodeURIComponent(provider)}`,
    {
      method: "PUT",
      body: JSON.stringify({ ...input }),
    },
  );
}

/** Clears the stored key and returns that provider's fresh DTO. */
export function deleteWebProviderKey(
  provider: WebProviderId,
): Promise<WebProviderView> {
  return request<WebProviderView>(
    `/web/providers/${encodeURIComponent(provider)}/key`,
    { method: "DELETE" },
  );
}

/** One live connection probe; failures arrive as a `502` error envelope. */
export function testWebProvider(
  provider: WebProviderId,
): Promise<WebProviderTestResult> {
  return request<WebProviderTestResult>(
    `/web/providers/${encodeURIComponent(provider)}/test`,
    { method: "POST" },
  );
}

/** Re-reads provider configuration and returns the full router view. */
export function reloadWebProviders(): Promise<WebProvidersResponse> {
  return request<WebProvidersResponse>("/web/providers/reload", {
    method: "POST",
  });
}

// ---------------------------------------------------------------------------
// Tool catalog and control
// ---------------------------------------------------------------------------

/**
 * Reads the startup catalog snapshot with control overrides merged. Source
 * failures are isolated in `errors`; `control_error` is non-null when the
 * catalog loaded but the control store did not.
 */
export function getToolCatalog(): Promise<ToolCatalogResponse> {
  return request<ToolCatalogResponse>("/tools/catalog");
}

/** Unwraps the `{ tools: [...] }` envelope of `GET /api/tools/control`. */
export function getToolControl(): Promise<ToolState[]> {
  return request<ToolControlResponse>("/tools/control").then(
    (response) => response.tools,
  );
}

/**
 * Applies one control action and unwraps the full state list. `404` means an
 * unknown tool, `400` an invalid role, and `503` a missing control store.
 */
export function applyToolControl(
  action: ToolControlAction,
): Promise<ToolState[]> {
  return request<ToolControlResponse>("/tools/control", {
    method: "POST",
    body: JSON.stringify({ ...action }),
  }).then((response) => response.tools);
}

export function setToolEnabled(
  key: string,
  enabled: boolean,
): Promise<ToolState[]> {
  return applyToolControl({ action: "set_enabled", key, enabled });
}

/**
 * Replaces one tool's visibility set. An empty list clears every visibility
 * row; the catalog then reports an empty `default_roles`, which means every
 * recognized role.
 */
export function setToolVisibility(
  key: string,
  roles: string[],
): Promise<ToolState[]> {
  return applyToolControl({ action: "set_visibility", key, roles });
}

// ---------------------------------------------------------------------------
// MCP servers (live management)
// ---------------------------------------------------------------------------

/** Lists live server health plus the masked config summaries when present. */
export function getMcpServers(): Promise<McpServersResponse> {
  return request<McpServersResponse>("/mcp/servers");
}

/**
 * Adds one server. The file is persisted before the live reload; `409` is a
 * duplicate name, `400` invalid, and `502` "saved but did not connect".
 */
export function addMcpServer(
  input: McpServerInput,
): Promise<McpServerWriteResponse> {
  return request<McpServerWriteResponse>("/mcp/servers", {
    method: "POST",
    body: JSON.stringify({ ...input }),
  });
}

/**
 * Replaces one server entry (the server treats PUT and PATCH identically).
 * `404` is an unknown name; `502` means the write persisted but the target
 * failed to connect.
 */
export function updateMcpServer(
  name: string,
  input: McpServerInput,
): Promise<McpServerWriteResponse> {
  return request<McpServerWriteResponse>(
    `/mcp/servers/${encodeURIComponent(name)}`,
    {
      method: "PATCH",
      body: JSON.stringify({ ...input }),
    },
  );
}

/** Deletes one server, then reloads. `404` is an unknown name. */
export function deleteMcpServer(
  name: string,
): Promise<McpServerDeleteResponse> {
  return request<McpServerDeleteResponse>(
    `/mcp/servers/${encodeURIComponent(name)}`,
    { method: "DELETE" },
  );
}

/**
 * Re-discovers one connected server's tools. `404` is an unknown name and
 * `502` a discovery failure.
 */
export function refreshMcpServer(
  name: string,
): Promise<McpServerRefreshResponse> {
  return request<McpServerRefreshResponse>(
    `/mcp/servers/${encodeURIComponent(name)}/refresh`,
    { method: "POST" },
  );
}

/** Re-reads the MCP config from disk and unwraps its reload report. */
export function reloadMcpServers(): Promise<McpReloadReport> {
  return request<McpReloadResponse>("/mcp/reload", {
    method: "POST",
  }).then((response) => response.reload);
}

// ---------------------------------------------------------------------------
// Guard rules
// ---------------------------------------------------------------------------

/** Unwraps the `{ rules: [...] }` envelope (evaluation order). */
export function listRules(): Promise<GuardRule[]> {
  return request<RulesResponse>("/intercept/rules").then(
    (response) => response.rules,
  );
}

/** Creates one rule; the bare row (no envelope) comes back. */
export function createRule(input: GuardRuleInput): Promise<GuardRule> {
  return request<GuardRule>("/intercept/rules", {
    method: "POST",
    body: JSON.stringify({ ...input }),
  });
}

/** Full replace; the bare updated row comes back. */
export function updateRule(
  id: number,
  input: GuardRuleInput,
): Promise<GuardRule> {
  return request<GuardRule>(`/intercept/rules/${id}`, {
    method: "PUT",
    body: JSON.stringify({ ...input }),
  });
}

export function toggleRule(
  id: number,
  enabled: boolean,
): Promise<ToggleRuleResponse> {
  return request<ToggleRuleResponse>(`/intercept/rules/${id}/toggle`, {
    method: "POST",
    body: JSON.stringify({ enabled }),
  });
}

export function deleteRule(id: number): Promise<DeleteResponse> {
  return request<DeleteResponse>(`/intercept/rules/${id}`, {
    method: "DELETE",
  });
}

// ---------------------------------------------------------------------------
// App settings
// ---------------------------------------------------------------------------

export function getSettings(): Promise<AppSettingsView> {
  return request<AppSettingsView>("/settings");
}

/** Partial merge; the merged view comes back (same shape as `getSettings`). */
export function updateSettings(
  patch: SettingsPatch,
): Promise<AppSettingsView> {
  return request<AppSettingsView>("/settings", {
    method: "PUT",
    body: JSON.stringify({ ...patch }),
  });
}
