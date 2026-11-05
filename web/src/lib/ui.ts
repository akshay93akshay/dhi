/**
 * Pure UI helpers for the dashboard, task detail, and work view.
 *
 * Everything here is a plain function with no React, no `fetch`, and no
 * DOM dependency, so the unit tests (`ui.test.ts`) can exercise it directly
 * under `node --test`. Types come from `./types`; the `ApiError` import is
 * class-only (api.ts never imports this module, so there is no cycle).
 */

import { ApiError } from "./api.ts";
import type { CreateTaskInput, Health, JsonObject } from "./types.ts";

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
 * - **origin** — task creation writes `{ "goal": ..., "bound": ... }`;
 *   detected when both `goal` and `bound` keys are present.
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
  if (hasOwn(provenance, "goal") && hasOwn(provenance, "bound")) {
    return {
      kind: "origin",
      lines: [
        `Goal: ${stringifyValue(provenance.goal)}`,
        `Bound: ${stringifyValue(provenance.bound)}`,
      ],
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

/** Raw create-form state (HTML inputs hand us strings). */
export type CreateFormValues = {
  goal: string;
  bound: string;
  plannerPin: string;
  workerPin: string;
};

export type CreateFormResult =
  | { ok: true; input: CreateTaskInput }
  | { ok: false; error: string };

/**
 * Pure form → request-body mapper for `POST /api/tasks`.
 *
 * - the goal is trimmed; empty (after trim) refuses submission;
 * - `bound` must be a whole number ≥ 0 — the server has no default for the
 *   field (`CreateTaskRequest.bound: i32`), so the form always supplies one;
 * - pins map through {@link parsePin}; unset pins are OMITTED from the body
 *   entirely so the server defaults apply (matching the "omits absent pin
 *   fields" behavior covered in api.test.ts).
 */
export function buildCreateTaskInput(
  values: CreateFormValues,
): CreateFormResult {
  const goalError = validateGoal(values.goal);
  if (goalError !== null) {
    return { ok: false, error: goalError };
  }
  const boundTrimmed = values.bound.trim();
  if (!/^\d+$/.test(boundTrimmed)) {
    return { ok: false, error: "Bound must be a whole number of 0 or more." };
  }
  const input: CreateTaskInput = {
    goal: values.goal.trim(),
    bound: Number(boundTrimmed),
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
