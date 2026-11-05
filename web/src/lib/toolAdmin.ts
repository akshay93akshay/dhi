/**
 * Pure helpers for the tool administration pages (`/tools`, `/mcp`,
 * `/skills`, `/custom-tools`).
 *
 * Everything here is a plain function with no React, no `fetch`, and no DOM
 * dependency, so `toolAdmin.test.ts` can exercise it under `node --test`.
 * Types come from `./types`; there is no import from `./api`, so the module
 * cannot create a runtime cycle with the API client.
 */

import type {
  McpReloadReport,
  McpServerInput,
  McpTransportKind,
  ToolCatalogEntry,
  ToolSource,
  ToolState,
} from "./types.ts";

/** Every `dhi_tool_catalog::ToolSource`, in a stable display order. */
export const TOOL_SOURCES: ReadonlyArray<ToolSource> = [
  "builtin",
  "web",
  "coding",
  "pty",
  "mcp",
  "skills",
  "custom",
];

/** The four non-specialist roles recognized by the catalog and control store. */
export const BASE_ROLES: ReadonlyArray<string> = [
  "planner",
  "worker",
  "reporter",
  "digest",
];

const SPECIALIST_PREFIX = "specialist:";
const CONTROL_CHARS = /[\u0000-\u001f\u007f-\u009f]/;

function hasControlCharacters(value: string): boolean {
  return CONTROL_CHARS.test(value);
}

/**
 * Mirrors `dhi_tool_catalog::is_recognized_role`: one of the four base roles
 * or a `specialist:<name>` with a non-empty, control-free name.
 */
export function isRecognizedRole(role: string): boolean {
  if (BASE_ROLES.includes(role)) {
    return true;
  }
  if (!role.startsWith(SPECIALIST_PREFIX)) {
    return false;
  }
  const name = role.slice(SPECIALIST_PREFIX.length);
  return name.trim() !== "" && !hasControlCharacters(name);
}

/**
 * Catalog visibility semantics: an empty role list means every recognized
 * role; a non-empty list is exact. Unrecognized roles see nothing.
 */
export function isVisibleToRole(tool: ToolCatalogEntry, role: string): boolean {
  if (!isRecognizedRole(role)) {
    return false;
  }
  return tool.default_roles.length === 0 || tool.default_roles.includes(role);
}

/**
 * Stable role ordering: the four base roles first, then specialists
 * alphabetically, then any other unique roles alphabetically.
 */
export function sortRoles(roles: ReadonlyArray<string>): string[] {
  const unique = [...new Set(roles.map((role) => role.trim()).filter((role) => role !== ""))];
  const base = BASE_ROLES.filter((role) => unique.includes(role));
  const specialists = unique
    .filter((role) => role.startsWith(SPECIALIST_PREFIX))
    .sort();
  const others = unique
    .filter(
      (role) =>
        !BASE_ROLES.includes(role) && !role.startsWith(SPECIALIST_PREFIX),
    )
    .sort();
  return [...base, ...specialists, ...others];
}

/**
 * Roles offered by the role filter and the visibility editor: the four base
 * roles plus every specialist role that appears in the current catalog.
 */
export function availableRoles(
  tools: ReadonlyArray<ToolCatalogEntry>,
): string[] {
  const roles = new Set<string>(BASE_ROLES);
  for (const tool of tools) {
    for (const role of tool.default_roles) {
      if (isRecognizedRole(role)) {
        roles.add(role);
      }
    }
  }
  return sortRoles([...roles]);
}

/** Human-readable role list; the empty list means "all recognized roles". */
export function formatToolRoles(roles: ReadonlyArray<string>): string {
  return roles.length === 0 ? "All roles" : roles.join(", ");
}

export type EnabledFilter = "all" | "enabled" | "disabled";

export type ToolFilterState = {
  source: ToolSource | "all";
  enabled: EnabledFilter;
  role: string | "all";
};

/**
 * Applies the three catalog filters. Order is preserved from the API, which
 * already returns entries sorted by key.
 */
export function filterCatalogTools(
  tools: ReadonlyArray<ToolCatalogEntry>,
  filters: ToolFilterState,
): ToolCatalogEntry[] {
  return tools.filter((tool) => {
    if (filters.source !== "all" && tool.source !== filters.source) {
      return false;
    }
    if (filters.enabled === "enabled" && !tool.enabled) {
      return false;
    }
    if (filters.enabled === "disabled" && tool.enabled) {
      return false;
    }
    if (filters.role !== "all" && !isVisibleToRole(tool, filters.role)) {
      return false;
    }
    return true;
  });
}

/**
 * Merges the full `ToolState` list returned by a control write into the
 * catalog rows. Unknown keys in `states` are ignored; tools with no state
 * keep their current values.
 */
export function mergeToolStates(
  tools: ReadonlyArray<ToolCatalogEntry>,
  states: ReadonlyArray<ToolState>,
): ToolCatalogEntry[] {
  const byKey = new Map(states.map((state) => [state.key, state]));
  return tools.map((tool) => {
    const state = byKey.get(tool.key);
    return state === undefined
      ? tool
      : { ...tool, enabled: state.enabled, default_roles: [...state.roles] };
  });
}

const TOOL_DISPLAY_LABELS: Readonly<Record<string, string>> = {
  read: "Read — read a file",
  write: "Write — write a file",
  edit: "Edit — replace exact text in one file",
  search: "Search — search files",
  list: "List — list files",
  shell: "Shell — run a command",
  shell_open: "Shell open — open a terminal",
  shell_send: "Shell send — send terminal input",
  shell_read: "Shell read — read terminal output",
  shell_close: "Shell close — close a terminal",
  shell_list: "Shell list — list terminals",
  web_search: "Web search — search the web",
  web_read: "Web read — read a page",
  web_parse: "Web parse — parse a document",
  web_research: "Web research — answer with sources",
};

const TOOL_SOURCE_WORDS: Readonly<Record<ToolSource, string>> = {
  builtin: "built-in tool",
  web: "web tool",
  coding: "coding tool",
  pty: "terminal tool",
  mcp: "MCP tool",
  skills: "skill",
  custom: "custom tool",
};

/**
 * Short one-line label for a catalog row. The built-in tools have exact
 * labels; anything else (an MCP tool, skill, or custom tool) is humanized
 * from the last path-like segment of the key and tagged with its source.
 * Descriptions are never used.
 */
export function toolDisplayLabel(key: string, source: ToolSource): string {
  const known = TOOL_DISPLAY_LABELS[key];
  if (known !== undefined) {
    return known;
  }
  const segment =
    key
      .split(/__|[:/.]/)
      .map((part) => part.trim())
      .filter((part) => part !== "")
      .pop() ?? "";
  const words = segment.replace(/[_-]+/g, " ").replace(/\s+/g, " ").trim();
  const humanized =
    words === "" ? "Tool" : words.charAt(0).toUpperCase() + words.slice(1);
  return `${humanized} · ${TOOL_SOURCE_WORDS[source]}`;
}

/** Adds or removes one role and returns a stable, deduplicated order. */
export function toggleRoleSelection(
  roles: ReadonlyArray<string>,
  role: string,
  selected: boolean,
): string[] {
  return sortRoles(
    selected ? [...roles, role] : roles.filter((candidate) => candidate !== role),
  );
}

export type SpecialistRoleResult =
  | { ok: true; roles: string[] }
  | { ok: false; error: string };

/**
 * Adds a `specialist:<name>` role. A bare name and an already-prefixed
 * `specialist:<name>` are both accepted; the result is deduplicated.
 */
export function addSpecialistRole(
  roles: ReadonlyArray<string>,
  raw: string,
): SpecialistRoleResult {
  const trimmed = raw.trim();
  if (trimmed === "") {
    return { ok: false, error: "Enter a specialist name." };
  }
  if (hasControlCharacters(trimmed)) {
    return { ok: false, error: "Specialist name must not contain control characters." };
  }
  const name = trimmed.startsWith(SPECIALIST_PREFIX)
    ? trimmed.slice(SPECIALIST_PREFIX.length).trim()
    : trimmed;
  if (name === "") {
    return { ok: false, error: "Enter a specialist name." };
  }
  return { ok: true, roles: sortRoles([...roles, `${SPECIALIST_PREFIX}${name}`]) };
}

export type KeyValueParseResult =
  | { ok: true; values: Record<string, string> }
  | { ok: false; error: string };

/**
 * Parses `KEY=VALUE` lines used for HTTP headers and stdio environment
 * variables. Blank lines are ignored; the value may contain `=`; both key and
 * value are trimmed; duplicate keys are rejected rather than silently
 * overwritten. `label` appears in the error text ("Headers", "Environment").
 */
export function parseKeyValueLines(
  raw: string,
  label: string,
): KeyValueParseResult {
  const values = Object.create(null) as Record<string, string>;
  const lines = raw.split(/\r?\n/);
  for (let index = 0; index < lines.length; index += 1) {
    const line = lines[index].trim();
    if (line === "") {
      continue;
    }
    const equals = line.indexOf("=");
    if (equals <= 0) {
      return {
        ok: false,
        error: `${label} line ${index + 1}: expected KEY=VALUE.`,
      };
    }
    const key = line.slice(0, equals).trim();
    if (key === "") {
      return {
        ok: false,
        error: `${label} line ${index + 1}: key must not be empty.`,
      };
    }
    if (Object.prototype.hasOwnProperty.call(values, key)) {
      return { ok: false, error: `${label}: duplicate key ${JSON.stringify(key)}.` };
    }
    values[key] = line.slice(equals + 1).trim();
  }
  return { ok: true, values: { ...values } };
}

/**
 * Splits the server summary's stdio `target` into a command and arguments.
 * The summary joins command and args with single spaces, so an argument that
 * itself contains whitespace cannot be reconstructed; callers should warn
 * that complex command lines must be re-entered on edit.
 */
export function splitStdioTarget(target: string): {
  command: string;
  args: string[];
} {
  const tokens = target.trim().split(/\s+/).filter((token) => token !== "");
  return { command: tokens[0] ?? "", args: tokens.slice(1) };
}

/** Raw MCP add/edit form state (HTML inputs hand every value to us as text). */
export type McpServerFormValues = {
  name: string;
  transport: McpTransportKind;
  command: string;
  args: string;
  env: string;
  url: string;
  headers: string;
  timeoutSecs: string;
  insecure: boolean;
  enabled: boolean;
};

export type McpServerInputResult =
  | { ok: true; input: McpServerInput }
  | { ok: false; error: string };

/**
 * Validates the MCP form and builds the request body. `mode: "edit"` omits
 * the name because the path is authoritative on `PATCH`. A blank timeout is
 * omitted (server default: 60 s); a supplied timeout must be positive.
 * Stdio uses newline-separated arguments (one argument per line), and both
 * env and headers use {@link parseKeyValueLines}.
 */
export function buildMcpServerInput(
  values: McpServerFormValues,
  mode: "add" | "edit",
): McpServerInputResult {
  const input: McpServerInput = {
    transport: values.transport,
    enabled: values.enabled,
  };

  if (mode === "add") {
    const name = values.name.trim();
    if (name === "") {
      return { ok: false, error: "Server name is required." };
    }
    if (hasControlCharacters(name)) {
      return {
        ok: false,
        error: "Server name must not contain control characters.",
      };
    }
    input.name = name;
  }

  const timeoutText = values.timeoutSecs.trim();
  if (timeoutText !== "") {
    if (!/^\d+$/.test(timeoutText) || Number(timeoutText) === 0) {
      return {
        ok: false,
        error:
          "Timeout must be a positive whole number of seconds, or blank for the 60 s default.",
      };
    }
    input.timeout_secs = Number(timeoutText);
  }

  if (values.transport === "stdio") {
    const command = values.command.trim();
    if (command === "") {
      return { ok: false, error: "Command is required for a stdio server." };
    }
    if (hasControlCharacters(command)) {
      return {
        ok: false,
        error: "Command must not contain control characters.",
      };
    }
    input.command = command;
    const args = values.args
      .split(/\r?\n/)
      .map((arg) => arg.trim())
      .filter((arg) => arg !== "");
    if (args.length > 0) {
      input.args = args;
    }
    const env = parseKeyValueLines(values.env, "Environment");
    if (!env.ok) {
      return env;
    }
    if (Object.keys(env.values).length > 0) {
      input.env = env.values;
    }
    return { ok: true, input };
  }

  const url = values.url.trim();
  if (url === "") {
    return {
      ok: false,
      error: "URL is required for an HTTP or SSE server.",
    };
  }
  if (!/^https?:\/\//i.test(url)) {
    return { ok: false, error: "URL must start with http:// or https://." };
  }
  input.url = url;
  const headers = parseKeyValueLines(values.headers, "Headers");
  if (!headers.ok) {
    return headers;
  }
  if (Object.keys(headers.values).length > 0) {
    input.headers = headers.values;
  }
  if (values.insecure) {
    input.insecure = true;
  }
  return { ok: true, input };
}

/** One-line summary of a live reload report for the MCP page. */
export function describeMcpReload(report: McpReloadReport): string {
  const parts = [`${report.configured} configured`];
  if (report.connected.length > 0) {
    parts.push(`${report.connected.length} connected`);
  }
  if (report.reconnected.length > 0) {
    parts.push(`${report.reconnected.length} reconnected`);
  }
  if (report.disconnected.length > 0) {
    parts.push(`${report.disconnected.length} disconnected`);
  }
  if (report.kept.length > 0) {
    parts.push(`${report.kept.length} kept`);
  }
  if (report.failed.length > 0) {
    parts.push(`${report.failed.length} failed`);
  }
  if (!report.runtime_attached) {
    parts.push("runtime not attached");
  }
  return parts.join(" · ");
}
