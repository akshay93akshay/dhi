/**
 * Pure helpers for the approvals queue page.
 *
 * Framework-free and dependency-light (types + `ApiError` only, mirroring
 * `ui.ts`) so `approvals.test.ts` can drive every path under `node --test`.
 *
 * Wire truth (checked against crates/dhi-server/src/lib.rs):
 * - `GET /api/approvals` serves ONLY rows with `status = 'pending'`
 *   (`dhi-store` `list_pending_approvals_all`: `WHERE status = 'pending'
 *   ORDER BY created_at, id`), flattened with `tool_name`/`input` from the
 *   linked tool-call row — both `null` when that row is missing. Terminal
 *   approvals (granted/denied/expired) never appear in the response.
 * - `POST /api/approvals/{id}/decision` takes `{ decision: "allow" |
 *   "deny" }` (`dhi_core::ApprovalDecision`, serde snake_case) and answers
 *   `404` (unknown id) or `409 Conflict` (no longer pending — decided or
 *   expired race, `decide_approval` re-reads and reports the row's current
 *   state when a concurrent decision/expiry won).
 */

import type { ApprovalDecision, ApprovalItem } from "./types.ts";
import {
  isScaleApprovalPending,
  SCALE_APPROVAL_DEFAULT_CAP,
  scaleApprovalStatusLabel,
  type ScaleApproval,
} from "./teamTypes.ts";

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/** Coalescing window for post-decision list refetches (rapid multi-decide). */
export const APPROVAL_REFRESH_DEBOUNCE_MS = 1500;

/** How long a freshly arrived pending card keeps its highlight. */
export const APPROVAL_ARRIVAL_HIGHLIGHT_MS = 2500;

/** Maximum key/value lines rendered for one tool input (DOM protection). */
export const TOOL_INPUT_MAX_LINES = 50;

/** Explicit note for the null-input case — never render a blank card body. */
export const MISSING_TOOL_INPUT_NOTE =
  "input unavailable (tool-call record missing)";

// ---------------------------------------------------------------------------
// Decision request builder
// ---------------------------------------------------------------------------

/** Result of {@link buildDecisionRequest}. */
export type DecisionRequest =
  | { ok: true; path: string; body: { decision: ApprovalDecision } }
  | { ok: false; error: string };

/**
 * Validates and shapes the `POST /api/approvals/{id}/decision` request.
 *
 * - `decision` must be exactly `"allow"` or `"deny"` (the wire enum
 *   `dhi_core::ApprovalDecision`; anything else — including the *statuses*
 *   `"granted"`/`"denied"`, a common mix-up — is rejected with a message).
 *   Accepted typed-wide (`unknown`) because validating a runtime value is
 *   the whole point.
 * - `id` must be a non-empty string after trimming; it is embedded
 *   path-escaped.
 *
 * The page treats a non-ok result as a programming-shaped bug (the buttons
 * only offer the two valid values), so the error path mainly guards tests
 * and future callers.
 */
export function buildDecisionRequest(
  id: unknown,
  decision: unknown,
): DecisionRequest {
  const trimmedId = typeof id === "string" ? id.trim() : "";
  if (trimmedId === "") {
    return { ok: false, error: "Approval id is required." };
  }
  if (decision !== "allow" && decision !== "deny") {
    return {
      ok: false,
      error: `decision must be "allow" or "deny", got: ${
        typeof decision === "string" ? JSON.stringify(decision) : typeof decision
      }`,
    };
  }
  return {
    ok: true,
    path: `/approvals/${encodeURIComponent(trimmedId)}/decision`,
    body: { decision },
  };
}

// ---------------------------------------------------------------------------
// Scale approval request builder
// ---------------------------------------------------------------------------

/** Result of {@link buildScaleDecisionRequest}. */
export type ScaleDecisionRequest =
  | { ok: true; path: string; body: { decision: ApprovalDecision } }
  | { ok: false; error: string };

/**
 * Validates and shapes `POST /api/scale-approvals/{id}/decision`.
 *
 * Wire truth: the body is the same `{ decision: "allow" | "deny" }` enum as a
 * tool approval. The preview card only offers those two typed values, so the
 * rejection branch guards tests and future callers.
 */
export function buildScaleDecisionRequest(
  id: unknown,
  decision: unknown,
): ScaleDecisionRequest {
  const trimmedId = typeof id === "string" ? id.trim() : "";
  if (trimmedId === "") {
    return { ok: false, error: "Scale approval id is required." };
  }
  if (decision !== "allow" && decision !== "deny") {
    return {
      ok: false,
      error: `decision must be "allow" or "deny", got: ${
        typeof decision === "string" ? JSON.stringify(decision) : typeof decision
      }`,
    };
  }
  return {
    ok: true,
    path: `/scale-approvals/${encodeURIComponent(trimmedId)}/decision`,
    body: { decision },
  };
}

/** Queue summary for one scale request: `12 requested · cap 8`. */
export function scaleApprovalSummary(
  approval: Pick<ScaleApproval, "requested_workers" | "current_cap" | "status">,
): string {
  const cap =
    approval.current_cap > 0
      ? approval.current_cap
      : SCALE_APPROVAL_DEFAULT_CAP;
  return `${approval.requested_workers} requested · cap ${cap} · ${scaleApprovalStatusLabel(
    approval.status,
  ).toLowerCase()}`;
}

// ---------------------------------------------------------------------------
// Unified queue
// ---------------------------------------------------------------------------

/** One row in the shared approvals queue, tagged by its approval family. */
export type ApprovalQueueRow =
  | {
      kind: "tool";
      key: string;
      createdAt: string;
      item: ApprovalItem;
    }
  | {
      kind: "scale";
      key: string;
      createdAt: string;
      item: ScaleApproval;
    };

/** Stable React key / inflight key for one queue row. */
export function approvalQueueKey(kind: "tool" | "scale", id: string): string {
  return `${kind}:${id}`;
}

/**
 * Merges pending tool approvals and pending scale approvals into one queue,
 * oldest first. Decided scale rows are ignored; the tool route already only
 * serves pending rows. Missing timestamps sort last.
 */
export function buildApprovalQueue(
  toolApprovals: readonly ApprovalItem[],
  scaleApprovals: readonly ScaleApproval[],
): ApprovalQueueRow[] {
  const rows: ApprovalQueueRow[] = [];
  for (const item of toolApprovals) {
    if (item.status !== "pending") {
      continue;
    }
    rows.push({
      kind: "tool",
      key: approvalQueueKey("tool", item.id),
      createdAt: item.created_at,
      item,
    });
  }
  for (const item of scaleApprovals) {
    if (!isScaleApprovalPending(item)) {
      continue;
    }
    rows.push({
      kind: "scale",
      key: approvalQueueKey("scale", item.id),
      createdAt: item.created_at ?? "",
      item,
    });
  }
  return rows.sort((a, b) => {
    if (a.createdAt === "" && b.createdAt !== "") {
      return 1;
    }
    if (b.createdAt === "" && a.createdAt !== "") {
      return -1;
    }
    if (a.createdAt !== b.createdAt) {
      return a.createdAt < b.createdAt ? -1 : 1;
    }
    return a.key < b.key ? -1 : a.key > b.key ? 1 : 0;
  });
}

// ---------------------------------------------------------------------------
// Age formatting ("waiting 2m")
// ---------------------------------------------------------------------------

/**
 * Humanized elapsed time between `createdAt` and `now` (both RFC 3339
 * strings or `Date`s; `now` defaults to the current time), rounded DOWN to
 * whole units: `< 60s` → `Ns`, `< 1h` → `Nm`, `< 1d` → `Nh`, else `Nd`.
 *
 * Tolerances:
 * - a FUTURE-dated `createdAt` (client/server clock skew) clamps to `0s`;
 * - an unparseable `createdAt` returns `"?"` — never throws, never shows a
 *   bogus `NaN`.
 *
 * The page renders this as `waiting {age}` from the approval's `created_at`.
 */
export function formatApprovalAge(
  createdAt: string | Date,
  now: string | Date = new Date(),
): string {
  const start = toDate(createdAt);
  const end = toDate(now);
  if (start === null || end === null) {
    return "?";
  }
  const elapsedMs = end.getTime() - start.getTime();
  // Future-dated rows (clock skew) read as "just started", not negative.
  const elapsedSec = Math.max(0, Math.floor(elapsedMs / 1000));
  if (elapsedSec < 60) {
    return `${elapsedSec}s`;
  }
  const minutes = Math.floor(elapsedSec / 60);
  if (minutes < 60) {
    return `${minutes}m`;
  }
  const hours = Math.floor(minutes / 60);
  if (hours < 24) {
    return `${hours}h`;
  }
  return `${Math.floor(hours / 24)}d`;
}

function toDate(value: string | Date): Date | null {
  const date = value instanceof Date ? value : new Date(value);
  return Number.isNaN(date.getTime()) ? null : date;
}

// ---------------------------------------------------------------------------
// Tool input rendering (hostile-shape-safe)
// ---------------------------------------------------------------------------

/** Renderable view of one `ApprovalItem.input`. */
export type ToolInputView =
  /** `null`/`undefined` — the linked tool-call row is missing. */
  | { kind: "missing"; lines: []; note: string }
  /** Plain object → `key: value` lines in key order. */
  | { kind: "object"; lines: string[] }
  /** Array → `[index]: value` lines. */
  | { kind: "array"; lines: string[] }
  /** Raw string, shown verbatim (empty string noted). */
  | { kind: "text"; lines: [string] }
  /** Numbers/booleans/bigints/other primitives (incl. exotic fallbacks). */
  | { kind: "scalar"; lines: [string] };

/** Like `ui.ts`'s MISSING_VALUE placeholder, duplicated to stay standalone. */
const UNDEFINED_VALUE = "—";
/** Last-resort stringifier output (mirrors `feed.ts` `prettyJson`). */
const UNPRINTABLE = "[unprintable]";

/** Defensive one-line stringify for leaf values; never throws. */
function stringifyLeaf(value: unknown): string {
  if (value === null) {
    return "null";
  }
  switch (typeof value) {
    case "string":
      return value === "" ? "(empty)" : value;
    case "number":
    case "boolean":
      return String(value);
    case "bigint":
      return `${value.toString()}n`;
    case "undefined":
      return UNDEFINED_VALUE;
    default: {
      // Nested objects/arrays/everything else: compact JSON when possible.
      try {
        return JSON.stringify(value) ?? UNPRINTABLE;
      } catch {
        // Circular or JSON-hostile structure — degrade, never throw.
        try {
          return String(value);
        } catch {
          return UNPRINTABLE;
        }
      }
    }
  }
}

/**
 * Turns an `ApprovalItem.input` (any JSON value the server hands over, or
 * junk) into display lines:
 *
 * - `null`/`undefined` → the explicit `{ kind: "missing" }` note (never a
 *   blank body — the tool-call record backing the input is gone);
 * - plain objects → one `key: value` line per own enumerable key (insertion
 *   order), leaf values via the defensive stringifier;
 * - arrays → one `[i]: value` line per index;
 * - strings → the raw string verbatim (empty string → `(empty)`);
 * - other scalars → `String(value)` (`bigint` suffixed `n`);
 * - exotic objects (Date, class instances, maps/set-like with no enumerable
 *   own keys) collapse to a single compact-JSON line;
 * - hostile inputs (circular structures, throwing getters, symbols, frozen
 *   proxies…) degrade to fallback lines — this function NEVER throws;
 * - more than {@link TOOL_INPUT_MAX_LINES} lines are cut with a
 *   `+N more … hidden` tail (DOM protection, mirroring the task list row cap).
 */
export function renderToolInput(input: unknown): ToolInputView {
  if (input === null || input === undefined) {
    return { kind: "missing", lines: [], note: MISSING_TOOL_INPUT_NOTE };
  }

  let lines: string[];
  if (typeof input === "string") {
    return { kind: "text", lines: [input === "" ? "(empty)" : input] };
  } else if (Array.isArray(input)) {
    lines = arrayLines(input as readonly unknown[]);
    // An empty array would otherwise render a blank body.
    if (lines.length === 0) {
      lines = ["(empty)"];
    }
  } else if (typeof input === "object") {
    const record = input as { [key: string]: unknown };
    const keys = safeKeys(record);
    if (keys.length === 0) {
      // No enumerable own keys (Date, Map/Set-ish, exotic instance): fall
      // back to one compact-JSON line so the card is never blank.
      lines = [compactJson(input)];
    } else {
      lines = keys.map((key) => `${key}: ${stringifyLeaf(readKey(record, key))}`);
    }
  } else {
    // number / boolean / bigint (symbol/function degrade below).
    return { kind: "scalar", lines: [stringifyLeaf(input)] };
  }

  if (lines.length > TOOL_INPUT_MAX_LINES) {
    const hidden = lines.length - TOOL_INPUT_MAX_LINES;
    lines = [
      ...lines.slice(0, TOOL_INPUT_MAX_LINES),
      `+${hidden} more item${hidden === 1 ? "" : "s"} hidden`,
    ];
  }

  if (Array.isArray(input)) {
    return { kind: "array", lines };
  }
  if (typeof input === "object") {
    return { kind: "object", lines };
  }
  // Unreachable given the branches above; kept as a typed last resort.
  return { kind: "scalar", lines: [stringifyLeaf(input)] };
}

/** Own enumerable keys, tolerant of exotic hosts; never throws. */
function safeKeys(record: { [key: string]: unknown }): string[] {
  try {
    return Object.keys(record);
  } catch {
    return [];
  }
}

/** Property read wrapped so a throwing getter cannot crash rendering. */
function readKey(
  record: { [key: string]: unknown },
  key: string,
): unknown {
  try {
    return record[key];
  } catch {
    return UNPRINTABLE;
  }
}

/** Index lines for an array; length beyond the dense prefix is respected. */
function arrayLines(input: readonly unknown[]): string[] {
  const lines: string[] = [];
  const length = typeof input.length === "number" ? input.length : 0;
  for (let index = 0; index < length; index++) {
    let value: unknown;
    try {
      value = input[index];
    } catch {
      value = UNPRINTABLE;
    }
    lines.push(`[${index}]: ${stringifyLeaf(value)}`);
  }
  return lines;
}

/** Compact JSON with the same triple fallback as `stringifyLeaf`. */
function compactJson(value: unknown): string {
  try {
    return JSON.stringify(value) ?? UNPRINTABLE;
  } catch {
    try {
      return String(value);
    } catch {
      return UNPRINTABLE;
    }
  }
}
