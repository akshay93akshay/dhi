/**
 * Pure helpers for the work view's DHI+ capacity and swarm panels.
 *
 * Keep this module React-free so `workTeam.test.ts` can drive every branch
 * under `node --test`. The API shapes live in `types.ts`; nothing here
 * fetches, formats in a timezone, or touches the DOM.
 */

import { shortenId } from "./ui.ts";
import type {
  JsonObject,
  TaskCapacity,
  TaskCapacityPatch,
} from "./types.ts";

// ---------------------------------------------------------------------------
// Capacity ledger
// ---------------------------------------------------------------------------

/**
 * The four editable limits, in display order. `label` is the input label;
 * `summaryLabel` keeps the one-line current-value summary readable.
 */
export const CAPACITY_FIELDS = [
  { key: "max_rounds", label: "Max rounds", summaryLabel: "rounds" },
  {
    key: "max_model_calls",
    label: "Max model calls",
    summaryLabel: "model calls",
  },
  { key: "max_wall_secs", label: "Max wall secs", summaryLabel: "s wall" },
  {
    key: "max_parallel_workers",
    label: "Max parallel workers",
    summaryLabel: "parallel workers",
  },
] as const;

export type CapacityField = (typeof CAPACITY_FIELDS)[number]["key"];

/** Raw HTML-input state: every limit is a string, blank means "not set". */
export type CapacityInputs = Record<CapacityField, string>;

export const EMPTY_CAPACITY_INPUTS: CapacityInputs = {
  max_rounds: "",
  max_model_calls: "",
  max_wall_secs: "",
  max_parallel_workers: "",
};

/** Seeds the four inputs from a task's current ledger (null → blank). */
export function capacityInputs(
  capacity: TaskCapacity | null | undefined,
): CapacityInputs {
  return {
    max_rounds:
      capacity?.max_rounds === null || capacity?.max_rounds === undefined
        ? ""
        : String(capacity.max_rounds),
    max_model_calls:
      capacity?.max_model_calls === null ||
      capacity?.max_model_calls === undefined
        ? ""
        : String(capacity.max_model_calls),
    max_wall_secs:
      capacity?.max_wall_secs === null || capacity?.max_wall_secs === undefined
        ? ""
        : String(capacity.max_wall_secs),
    max_parallel_workers:
      capacity?.max_parallel_workers === null ||
      capacity?.max_parallel_workers === undefined
        ? ""
        : String(capacity.max_parallel_workers),
  };
}

/** Text for one limit; a null/omitted ledger falls back to the engine. */
export function capacityLimitText(
  value: number | null | undefined,
): string {
  return value === null || value === undefined ? "Engine default" : String(value);
}

/**
 * One-line summary of the current ledger. Every null limit is omitted; a
 * completely empty ledger reads "Engine default".
 */
export function capacitySummary(
  capacity: TaskCapacity | null | undefined,
): string {
  if (capacity === null || capacity === undefined) {
    return "Engine default";
  }
  const parts: string[] = [];
  for (const field of CAPACITY_FIELDS) {
    const value = capacity[field.key];
    if (value !== null && value !== undefined) {
      parts.push(
        field.key === "max_wall_secs"
          ? `${value}s wall`
          : `${value} ${field.summaryLabel}`,
      );
    }
  }
  return parts.length === 0 ? "Engine default" : parts.join(" · ");
}

/** Parsed patch or a user-facing validation message. */
export type CapacityPatchResult =
  | { ok: true; patch: TaskCapacityPatch }
  | { ok: false; error: string };

const POSITIVE_INTEGER = /^\d+$/;

/**
 * Maps the four text inputs onto a `POST /capacity` patch.
 *
 * - blank inputs are OMITTED so the server keeps the stored value (the route
 *   has no way to clear a limit);
 * - non-blank values must be positive safe integers;
 * - fields equal to `current` are omitted too, so a poll cannot make a stale
 *   draft overwrite a limit that changed elsewhere;
 * - an empty patch is refused instead of firing a no-op request.
 */
export function buildCapacityPatch(
  inputs: CapacityInputs,
  current?: TaskCapacity | null,
): CapacityPatchResult {
  const patch: TaskCapacityPatch = {};
  for (const field of CAPACITY_FIELDS) {
    const raw = inputs[field.key].trim();
    if (raw === "") {
      continue;
    }
    if (!POSITIVE_INTEGER.test(raw)) {
      return {
        ok: false,
        error: `${field.label} must be a positive whole number.`,
      };
    }
    const value = Number(raw);
    if (!Number.isSafeInteger(value) || value <= 0) {
      return {
        ok: false,
        error: `${field.label} must be a positive whole number.`,
      };
    }
    if (current?.[field.key] === value) {
      continue;
    }
    patch[field.key] = value;
  }
  if (Object.keys(patch).length === 0) {
    return { ok: false, error: "No capacity changes to apply." };
  }
  return { ok: true, patch };
}

// ---------------------------------------------------------------------------
// Swarm messages
// ---------------------------------------------------------------------------

/** One persisted swarm message prepared for compact display. */
export type SwarmMessageView = {
  id: number;
  kind: string;
  /** `payload.from` (or `payload.sender`) when the message carries one. */
  from: string | null;
  intentId: string;
  /** `shortenId(intentId)`; empty when the record has no intent id. */
  intentShort: string;
  createdAt: string;
  /** Human summary derived from the typed payload. */
  summary: string;
};

function isRecord(value: unknown): value is JsonObject {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function nonEmptyString(value: unknown): string | null {
  return typeof value === "string" && value.trim() !== "" ? value : null;
}

function compactJson(value: unknown): string {
  const maxChars = 240;
  try {
    const text = JSON.stringify(value);
    if (text === undefined) {
      return String(value);
    }
    return text.length > maxChars
      ? `${text.slice(0, maxChars - 1)}…`
      : text;
  } catch {
    return "[unprintable payload]";
  }
}

const SWARM_KIND_LABELS: Record<string, string> = {
  finding: "Finding",
  question: "Question",
  challenge: "Challenge",
  done: "Done",
};

/** Display label for a persisted kind; unknown kinds stay verbatim. */
export function swarmKindLabel(kind: string): string {
  const trimmed = kind.trim();
  if (trimmed === "") {
    return "Message";
  }
  return SWARM_KIND_LABELS[trimmed.toLowerCase()] ?? trimmed;
}

/**
 * Compact summary of one serialized `SwarmMessage`:
 * findings use `summary` (plus their subtask id), questions/challenges use
 * `text`, done messages read "done", and anything unfamiliar degrades to
 * compact JSON instead of throwing.
 */
export function swarmPayloadSummary(payload: unknown): string {
  if (!isRecord(payload)) {
    return compactJson(payload);
  }

  const summary = nonEmptyString(payload.summary);
  if (summary !== null) {
    const subtask = nonEmptyString(payload.subtask_id);
    return subtask === null ? summary : `${summary} · subtask ${subtask}`;
  }

  const text =
    nonEmptyString(payload.text) ?? nonEmptyString(payload.message);
  if (text !== null) {
    return text;
  }

  if (payload.type === "done") {
    return "done";
  }

  return compactJson(payload);
}

/**
 * Parses one API record into a display view. Rows without a usable numeric
 * id are rejected (React keys and ordering depend on it); every other field
 * degrades to a safe default.
 */
export function parseSwarmMessage(record: unknown): SwarmMessageView | null {
  if (!isRecord(record)) {
    return null;
  }
  const id = record.id;
  if (
    typeof id !== "number" ||
    !Number.isSafeInteger(id) ||
    id < 0
  ) {
    return null;
  }
  const kind = nonEmptyString(record.kind) ?? "message";
  const intentId = nonEmptyString(record.intent_id) ?? "";
  const payload = isRecord(record.payload) ? record.payload : {};
  return {
    id,
    kind,
    from: nonEmptyString(payload.from) ?? nonEmptyString(payload.sender),
    intentId,
    intentShort: shortenId(intentId),
    createdAt:
      typeof record.created_at === "string" ? record.created_at : "",
    summary: swarmPayloadSummary(payload),
  };
}

/** Parses every valid row, preserving server order (ascending id). */
export function parseSwarmMessages(
  records: readonly unknown[],
): SwarmMessageView[] {
  const views: SwarmMessageView[] = [];
  for (const record of records) {
    const view = parseSwarmMessage(record);
    if (view !== null) {
      views.push(view);
    }
  }
  return views;
}

/**
 * True when a task's facts include a DHI+ team fact recorded by a swarm run.
 * The engine writes `{ "topology": "swarm" | "manager_worker", ... }` into the
 * one fact a team plan produces; this is the only task-level topology signal
 * exposed by the current work API.
 */
export function hasSwarmTopology(
  facts: readonly { provenance?: unknown }[],
): boolean {
  return facts.some(
    (fact) => isRecord(fact.provenance) && fact.provenance.topology === "swarm",
  );
}
