/**
 * Pure UI helpers for the dashboard, task detail, and work view.
 *
 * Everything here is a plain function with no React, no `fetch`, and no
 * DOM dependency, so the unit tests (`ui.test.ts`) can exercise it directly
 * under `node --test`. Types come from `./types`; the `ApiError` import is
 * class-only (api.ts never imports this module, so there is no cycle).
 */

import { ApiError } from "./api.ts";
import type {
  AgentsMdState,
  CreateTaskInput,
  Health,
  JsonObject,
  TaskTopology,
} from "./types.ts";

/** The three visual states of the DHI brand-status block. */
export type ServiceState = "healthy" | "error" | "loading";

/** Map the health response and the last fetch error to one visual state. */
export function serviceState(
  health: Health | null,
  error: string | null,
): ServiceState {
  if (health?.status === "ok") {
    return "healthy";
  }
  return error !== null || health !== null ? "error" : "loading";
}

/** Short visible status text. No service name and no extra sentence. */
export function serviceStatusText(state: ServiceState): string {
  return state === "healthy"
    ? "Status: Healthy"
    : state === "error"
      ? "Status: Error"
      : "Status: Loading";
}

// ---------------------------------------------------------------------------
// Identity formatting
// ---------------------------------------------------------------------------

/**
 * Short display form of a UUID: the first 8 characters (the first hex group,
 * dash excluded). Strings of 8 characters or fewer pass through unchanged.
 */
export function shortenId(id: string): string {
  return id.length <= 8 ? id : id.slice(0, 8);
}

/**
 * The real task id carried by a task route pathname.
 *
 * The static export contains only the `preview` placeholder, so `useParams()`
 * can report `preview` on a real `/tasks/<id>` URL. The browser pathname still
 * carries the actual id; this reads it for the detail, work, and graph routes.
 * Returns `null` for the placeholder, empty segments, and non-task paths.
 */
export function taskIdFromPathname(pathname: string): string | null {
  const trimmed = pathname.replace(/\/+$/, "");
  const match = /^\/tasks\/([^/]+)(?:\/(work|graph))?$/.exec(trimmed);
  if (match === null) {
    return null;
  }
  let id: string;
  try {
    id = decodeURIComponent(match[1]);
  } catch {
    return null;
  }
  return id === "" || id === "preview" ? null : id;
}

// ---------------------------------------------------------------------------
// Provenance rendering
// ---------------------------------------------------------------------------

/** Discriminated result of {@link renderProvenance}. */
export type ProvenanceView =
  | { kind: "worker"; lines: string[] }
  | { kind: "origin"; lines: string[] }
  | { kind: "raw"; lines: string[] };

/** Placeholder for keys whose value is `undefined` (never valid JSON). */
const MISSING_VALUE = "—";

function stringifyValue(value: unknown): string {
  if (value === null) {
    return "null";
  }
  switch (typeof value) {
    case "string":
      return value === "" ? "(empty)" : value;
    case "number":
    case "bigint":
    case "boolean":
      return String(value);
    case "undefined":
      return MISSING_VALUE;
    default:
      // Objects/arrays/functions: compact JSON when possible, never a throw.
      try {
        return JSON.stringify(value) ?? MISSING_VALUE;
      } catch {
        return String(value);
      }
  }
}

function hasOwn(object: JsonObject, key: string): boolean {
  return Object.prototype.hasOwnProperty.call(object, key);
}

/**
 * Turns a `Fact.provenance` object into display lines, detecting the two
 * documented producer conventions by shape (see `types.ts`):
 *
 * - **origin** — task creation writes `{ "goal": ... }`;
 *   detected when the `goal` key is present.
 * - **worker** — intent completion writes
 *   `{ "intent_id", "detail", "evidence", "confidence" }` (`detail` may be
 *   `""`); detected when any of `intent_id`/`detail`/`evidence` is present.
 * - **raw** — anything else: the object pretty-printed as JSON. Unexpected
 *   shapes (extra keys, wrong value types, nested objects) are tolerated:
 *   this function never throws.
 *
 * When a value carries both shapes' keys the origin convention wins; only
 * recognized keys are surfaced for `worker`/`origin`, everything else shows
 * through `raw`.
 */
export function renderProvenance(provenance: JsonObject): ProvenanceView {
  if (hasOwn(provenance, "goal")) {
    return {
      kind: "origin",
      lines: [`Goal: ${stringifyValue(provenance.goal)}`],
    };
  }
  if (
    hasOwn(provenance, "intent_id") ||
    hasOwn(provenance, "detail") ||
    hasOwn(provenance, "evidence")
  ) {
    const lines: string[] = [];
    if (hasOwn(provenance, "intent_id")) {
      lines.push(`Intent: ${stringifyValue(provenance.intent_id)}`);
    }
    if (hasOwn(provenance, "detail")) {
      lines.push(`Detail: ${stringifyValue(provenance.detail)}`);
    }
    if (hasOwn(provenance, "evidence")) {
      lines.push(`Evidence: ${stringifyValue(provenance.evidence)}`);
    }
    if (hasOwn(provenance, "confidence")) {
      lines.push(`Confidence: ${stringifyValue(provenance.confidence)}`);
    }
    return { kind: "worker", lines };
  }
  let pretty: string;
  try {
    pretty = JSON.stringify(provenance, null, 2) ?? "";
  } catch {
    pretty = String(provenance);
  }
  return { kind: "raw", lines: [pretty] };
}

/** Provenance labels whose values are model-authored detail (not ids/enums). */
const PROVENANCE_DETAIL_LABELS = new Set(["Goal", "Detail", "Evidence"]);

/**
 * Copy text for the fact-detail block: the joined Goal/Detail/Evidence lines
 * below the claim. Intent ids and Confidence stay out of the copied detail;
 * raw JSON has no recognized detail convention and returns `null`.
 */
export function provenanceDetailText(view: ProvenanceView): string | null {
  if (view.kind === "raw") {
    return null;
  }
  const detail = view.lines.filter((line) => {
    const separator = line.indexOf(": ");
    return (
      separator !== -1 &&
      PROVENANCE_DETAIL_LABELS.has(line.slice(0, separator))
    );
  });
  return detail.length === 0 ? null : detail.join("\n");
}

// ---------------------------------------------------------------------------
// Create-task form logic
// ---------------------------------------------------------------------------

/** Select value that means "no pin — use the role's pool default". */
export const PIN_UNSET = "";

/**
 * Validates the goal field: trimmed non-empty text is required (the server
 * rejects empty-after-trim goals with `400`). Returns an error message, or
 * `null` when the goal is usable.
 */
export function validateGoal(raw: string): string | null {
  return raw.trim() === "" ? "Goal is required." : null;
}

/**
 * Maps a pin `<select>` value onto a profile id. `""` (or any non-nonnegative-
 * integer text) means "no pin" → `null`; digit strings become ids.
 */
export function parsePin(raw: string): number | null {
  const trimmed = raw.trim();
  return /^\d+$/.test(trimmed) ? Number(trimmed) : null;
}

/**
 * Human label for a task's `agents_md_state`, or `null` for legacy tasks and
 * older servers that omit the field.
 */
export function agentsMdStateLabel(
  state: AgentsMdState | null | undefined,
): string | null {
  switch (state) {
    case "loaded":
      return "AGENTS.md loaded";
    case "truncated":
      return "AGENTS.md truncated";
    case "not_found":
      return "No AGENTS.md";
    case "disabled":
      return "AGENTS.md off";
    default:
      return null;
  }
}

/** Badge tone matching {@link agentsMdStateLabel}. */
export function agentsMdStateVariant(
  state: AgentsMdState | null | undefined,
): "success" | "warning" | "muted" {
  switch (state) {
    case "loaded":
      return "success";
    case "truncated":
      return "warning";
    default:
      return "muted";
  }
}

/** Raw create-form state (HTML inputs hand us strings). */
export type CreateFormValues = {
  goal: string;
  plannerPin: string;
  workerPin: string;
  /** Create-form topology value; omitted/blank means "let the planner decide". */
  topology?: string;
  /** Create-form worker count; blank means Auto (send `null`). */
  swarmWorkers?: string;
  /**
   * Create-form Auto toggle. Defaults to `true` when omitted, so a caller that
   * does not mention Auto gets the planner-decides width.
   */
  swarmWorkersAuto?: boolean;
  /** Selected absolute project folder on the DHI+ host; required. */
  projectFolder?: string;
  /** Selected absolute AGENTS.md path; blank sends `null`. Independent. */
  agentsMdPath?: string;
};

/** Select options for the create-task topology picker, in display order. */
export const TOPOLOGY_OPTIONS = [
  { value: "", label: "Let planner decide" },
  { value: "single", label: "Single agent" },
  { value: "manager_worker", label: "Multi agent" },
  { value: "swarm", label: "Swarm" },
] as const;

/**
 * Swarm width bounds. `N` counts teammates only; the coordinator is one extra
 * agent. A fixed N is exact and needs no approval. Auto lets the coordinator
 * choose up to
 * {@link SWARM_WORKERS_AUTO_MAX} without a scale approval.
 */
export const SWARM_WORKERS_MIN = 2;
export const SWARM_WORKERS_MAX = 64;
export const SWARM_WORKERS_AUTO_MAX = 8;

/** Create-form help line shown under the swarm worker controls. */
export const SWARM_WORKERS_HELP_TEXT =
  "N teammates + 1 coordinator. All N run in parallel.";

/** Task-header text for a swarm that lets the coordinator choose its width. */
export const SWARM_WORKERS_AUTO_LABEL =
  `Auto (coordinator decides, max ${SWARM_WORKERS_AUTO_MAX} without approval)`;

/** Whether the create form must show the swarm worker controls. */
export function showsSwarmWorkersField(
  topology: string | null | undefined,
): boolean {
  return parseTopology(topology) === "swarm";
}

/**
 * Human label for a stored swarm width. `N` counts teammates, so the coordinator is
 * always shown as an extra agent. Returns `null` when no width is pinned.
 */
export function swarmWorkersLabel(
  value: number | null | undefined,
): string | null {
  return value === null || value === undefined ? null : `${value} + coordinator`;
}

/**
 * Full swarm-width description for a task header: `N + coordinator` for a pinned
 * count (including legacy 2–8 tasks), the Auto explanation for a swarm with no
 * pinned count, and `null` for non-swarm tasks with no pinned count.
 */
export function swarmWorkersSummary(
  topology: TaskTopology | null,
  value: number | null | undefined,
): string | null {
  if (value !== null && value !== undefined) {
    return swarmWorkersLabel(value);
  }
  return topology === "swarm" ? SWARM_WORKERS_AUTO_LABEL : null;
}

/**
 * Validation for the create-form worker count. Blank and `auto` are valid
 * (Auto sends `null`); anything else must be a whole number from 2 to 64.
 * Returns a user-facing message, or `null` when the value is accepted.
 */
export function validateSwarmWorkers(
  raw: string | null | undefined,
): string | null {
  const trimmed = (raw ?? "").trim();
  if (trimmed === "" || trimmed.toLowerCase() === "auto") {
    return null;
  }
  if (/^\d+$/.test(trimmed)) {
    const value = Number(trimmed);
    if (value >= SWARM_WORKERS_MIN && value <= SWARM_WORKERS_MAX) {
      return null;
    }
  }
  return `Swarm workers must be a whole number between ${SWARM_WORKERS_MIN} and ${SWARM_WORKERS_MAX}, or Auto.`;
}

/**
 * Maps the create-form worker input onto a swarm width. Blank (and `auto`)
 * means "the planner decides" → `null`; only whole numbers 2–64 are accepted,
 * everything else degrades to `null` (use {@link validateSwarmWorkers} to
 * reject out-of-range input before submitting).
 */
export function parseSwarmWorkers(
  raw: string | null | undefined,
): number | null {
  const trimmed = (raw ?? "").trim().toLowerCase();
  if (trimmed === "" || trimmed === "auto" || !/^\d+$/.test(trimmed)) {
    return null;
  }
  const value = Number(trimmed);
  return value >= SWARM_WORKERS_MIN && value <= SWARM_WORKERS_MAX
    ? value
    : null;
}

/** Human label for a task topology (or `null` for "planner decides"). */
export function topologyLabel(topology: TaskTopology | null): string | null {
  switch (topology) {
    case "single":
      return "Single agent";
    case "manager_worker":
      return "Multi agent";
    case "swarm":
      return "Swarm";
    default:
      return null;
  }
}

/**
 * Maps the create-form topology `<select>` value onto the wire enum.
 * Anything blank or unrecognized means the planner decides (`null`).
 */
export function parseTopology(
  raw: string | null | undefined,
): TaskTopology | null {
  switch ((raw ?? "").trim().toLowerCase()) {
    case "single":
      return "single";
    case "manager_worker":
      return "manager_worker";
    case "swarm":
      return "swarm";
    default:
      return null;
  }
}

export type CreateFormResult =
  | { ok: true; input: CreateTaskInput }
  | { ok: false; error: string };

/**
 * Pure form → request-body mapper for `POST /api/tasks`.
 *
 * - the goal is trimmed; empty (after trim) refuses submission;
 * - the project folder is required: a blank trimmed value refuses submission
 *   before any other field is parsed;
 * - pins map through {@link parsePin}; unset pins are OMITTED from the body
 *   entirely so the server defaults apply (matching the "omits absent pin
 *   fields" behavior covered in api.test.ts);
 * - topology is ALWAYS sent: `null` for "let the planner decide", otherwise
 *   the selected enum value;
 * - `swarm_workers` is ALWAYS sent: a fixed 2–64 count only when the topology
 *   is `swarm` and Auto is off; otherwise (non-swarm, Auto, or blank) `null`.
 *   Out-of-range fixed counts refuse submission with a validation error.
 * - `project_folder` is ALWAYS sent: the trimmed selected folder.
 * - `agents_md_path` is ALWAYS sent: the trimmed selected AGENTS.md path, or
 *   `null` when no file is selected. It is independent of the folder.
 */
export function buildCreateTaskInput(
  values: CreateFormValues,
): CreateFormResult {
  const goalError = validateGoal(values.goal);
  if (goalError !== null) {
    return { ok: false, error: goalError };
  }
  const projectFolder = (values.projectFolder ?? "").trim();
  if (projectFolder === "") {
    return { ok: false, error: "Select a project folder." };
  }
  const topology = parseTopology(values.topology);
  let swarmWorkers: number | null = null;
  if (topology === "swarm" && values.swarmWorkersAuto === false) {
    const workersError = validateSwarmWorkers(values.swarmWorkers);
    if (workersError !== null) {
      return { ok: false, error: workersError };
    }
    swarmWorkers = parseSwarmWorkers(values.swarmWorkers);
  }
  const agentsMdPath = (values.agentsMdPath ?? "").trim();
  const input: CreateTaskInput = {
    goal: values.goal.trim(),
    topology,
    swarm_workers: swarmWorkers,
    project_folder: projectFolder,
    agents_md_path: agentsMdPath === "" ? null : agentsMdPath,
  };
  const planner = parsePin(values.plannerPin);
  if (planner !== null) {
    input.planner_profile_id = planner;
  }
  const worker = parsePin(values.workerPin);
  if (worker !== null) {
    input.worker_profile_id = worker;
  }
  return { ok: true, input };
}

// ---------------------------------------------------------------------------
// Error presentation
// ---------------------------------------------------------------------------

/**
 * One formatter for every failure path so pages agree on wording. Network
 * failures (`status 0`), validation failures (`400`), and missing targets
 * (`404`) get explicit prefixes; other `ApiError`s carry the server message
 * plus the HTTP status. Unknown throwables degrade to `String(err)`.
 */
export function describeApiError(err: unknown): string {
  if (err instanceof ApiError) {
    if (err.status === 0) {
      return `Network error: ${err.message}`;
    }
    if (err.isValidation()) {
      return `Invalid request: ${err.message}`;
    }
    if (err.isNotFound()) {
      return `Not found: ${err.message}`;
    }
    return `${err.message} (HTTP ${err.status})`;
  }
  if (err instanceof Error) {
    return err.message;
  }
  return String(err);
}
