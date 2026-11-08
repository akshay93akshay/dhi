/**
 * Pure helpers for the work view's DHI+ capacity and swarm panels.
 *
 * Keep this module React-free so `workTeam.test.ts` can drive every branch
 * under `node --test`. The API shapes live in `types.ts`; nothing here
 * fetches, formats in a timezone, or touches the DOM.
 */

import { shortenId } from "./ui.ts";
import { latestTeamRun } from "./teamRun.ts";
import type {
  JsonObject,
  Task,
  TaskCapacity,
  TaskCapacityPatch,
  TaskTopology,
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
export function capacityLimitText(value: number | null | undefined): string {
  return value === null || value === undefined
    ? "Engine default"
    : String(value);
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
// Topology
// ---------------------------------------------------------------------------

/** Narrows untrusted JSON to the three wire topology values. */
export function normalizeTopology(value: unknown): TaskTopology | null {
  switch (value) {
    case "single":
    case "manager_worker":
    case "swarm":
      return value;
    default:
      return null;
  }
}

/**
 * Effective topology for a task: the value pinned on the task when present,
 * otherwise the latest `team_run` activity's choice. Returns `null` while the
 * planner has not chosen yet.
 */
export function resolveTaskTopology(
  taskTopology: unknown,
  activities: readonly unknown[],
): TaskTopology | null {
  const pinned = normalizeTopology(taskTopology);
  if (pinned !== null) {
    return pinned;
  }
  return normalizeTopology(latestTeamRun(activities)?.topology);
}

/**
 * Whether the task used a swarm. A topology pinned at creation is
 * authoritative (it is immutable), so an explicit `single`/`manager_worker`
 * never shows the Swarm tab. Only tasks created before the field existed fall
 * back to the latest `team_run`.
 */
export function taskUsesSwarm(
  taskTopology: unknown,
  activities: readonly unknown[],
): boolean {
  const pinned = normalizeTopology(taskTopology);
  if (pinned !== null) {
    return pinned === "swarm";
  }
  return normalizeTopology(latestTeamRun(activities)?.topology) === "swarm";
}

// ---------------------------------------------------------------------------
// Capacity-exhaustion notice
// ---------------------------------------------------------------------------

/** One parsed `capacity_exhausted` activity. */
export type CapacityExhaustedEvent = {
  activityId: number;
  reason: string;
  rounds: number | null;
  modelCalls: number | null;
  elapsedSecs: number | null;
  maxRounds: number | null;
  maxModelCalls: number | null;
  maxWallSecs: number | null;
};

/** Renderable capacity-pause notice for the work view. */
export type CapacityPauseNotice = {
  activityId: number;
  reason: string;
  /** Human limit name: "model calls", "wall time", or "rounds". */
  label: string;
  used: number | null;
  limit: number | null;
  /** E.g. `Capacity exhausted: model calls 24 / 24`. */
  text: string;
};

function nonNegativeInt(value: unknown): number | null {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0
    ? value
    : null;
}

function optionalNonNegativeInt(value: unknown): number | null {
  return nonNegativeInt(value);
}

/**
 * Parses one `capacity_exhausted` payload. The engine writes `reason` plus
 * both the used counters (`rounds`, `model_calls`, `elapsed_secs`) and the
 * configured maxima; older rows may omit the maxima.
 */
export function parseCapacityExhausted(
  activity: unknown,
): CapacityExhaustedEvent | null {
  if (
    typeof activity !== "object" ||
    activity === null ||
    Array.isArray(activity)
  ) {
    return null;
  }
  const row = activity as {
    id?: unknown;
    kind?: unknown;
    payload?: unknown;
  };
  if (row.kind !== "capacity_exhausted") {
    return null;
  }
  const activityId = nonNegativeInt(row.id);
  if (activityId === null) {
    return null;
  }
  const payload =
    typeof row.payload === "object" &&
    row.payload !== null &&
    !Array.isArray(row.payload)
      ? (row.payload as JsonObject)
      : {};
  const reason =
    typeof payload.reason === "string" && payload.reason.trim() !== ""
      ? payload.reason.trim()
      : "";
  if (reason === "") {
    return null;
  }
  return {
    activityId,
    reason,
    rounds: optionalNonNegativeInt(payload.rounds),
    modelCalls: optionalNonNegativeInt(payload.model_calls),
    elapsedSecs: optionalNonNegativeInt(payload.elapsed_secs),
    maxRounds: optionalNonNegativeInt(payload.max_rounds),
    maxModelCalls: optionalNonNegativeInt(payload.max_model_calls),
    maxWallSecs: optionalNonNegativeInt(payload.max_wall_secs),
  };
}

/** Selects the highest-id `capacity_exhausted` row from a feed. */
export function latestCapacityExhausted(
  activities: readonly unknown[],
): CapacityExhaustedEvent | null {
  let latest: CapacityExhaustedEvent | null = null;
  for (const activity of activities) {
    const parsed = parseCapacityExhausted(activity);
    if (
      parsed !== null &&
      (latest === null || parsed.activityId > latest.activityId)
    ) {
      latest = parsed;
    }
  }
  return latest;
}

const CAPACITY_REASON_LABELS: Readonly<Record<string, string>> = {
  model_calls: "model calls",
  wall_time: "wall time",
  rounds: "rounds",
};

function capacityLimit(
  event: CapacityExhaustedEvent,
  taskCapacity: TaskCapacity | null | undefined,
): { label: string; used: number | null; limit: number | null } {
  switch (event.reason) {
    case "model_calls":
      return {
        label: CAPACITY_REASON_LABELS.model_calls,
        used: event.modelCalls,
        limit: event.maxModelCalls ?? taskCapacity?.max_model_calls ?? null,
      };
    case "wall_time":
      return {
        label: CAPACITY_REASON_LABELS.wall_time,
        used: event.elapsedSecs,
        limit: event.maxWallSecs ?? taskCapacity?.max_wall_secs ?? null,
      };
    case "rounds":
      return {
        label: CAPACITY_REASON_LABELS.rounds,
        used: event.rounds,
        limit: event.maxRounds ?? taskCapacity?.max_rounds ?? null,
      };
    default:
      return {
        label: event.reason,
        used: null,
        limit: null,
      };
  }
}

function capacityNumber(value: number | null, reason: string): string | null {
  if (value === null) {
    return null;
  }
  return reason === "wall_time" ? `${value}s` : String(value);
}

/**
 * Notice for the capacity bar: only while the task is `paused`, and only when
 * a `capacity_exhausted` activity explains why. A resumed/terminal task hides
 * the bar even if old exhaustion rows remain in the feed.
 */
export function capacityPauseNotice(
  task: Pick<Task, "state" | "capacity">,
  activities: readonly unknown[],
): CapacityPauseNotice | null {
  if (task.state !== "paused") {
    return null;
  }
  const event = latestCapacityExhausted(activities);
  if (event === null) {
    return null;
  }
  const { label, used, limit } = capacityLimit(event, task.capacity);
  const usedText = capacityNumber(used, event.reason);
  const limitText = capacityNumber(limit, event.reason);
  const valueText =
    usedText !== null || limitText !== null
      ? `${usedText ?? "?"} / ${limitText ?? "?"}`
      : null;
  return {
    activityId: event.activityId,
    reason: event.reason,
    label,
    used,
    limit,
    text:
      valueText === null
        ? `Capacity exhausted: ${label}`
        : `Capacity exhausted: ${label} ${valueText}`,
  };
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
  /** Untruncated message text used by the expandable detail. */
  fullText: string;
  /** Swarm round inferred from `payload.round` or a `:rN:` subtask id. */
  round: number | null;
  /** The worked subtask when the payload carries one. */
  subtaskId: string | null;
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
    return text.length > maxChars ? `${text.slice(0, maxChars - 1)}…` : text;
  } catch {
    return "[unprintable payload]";
  }
}

function fullJson(value: unknown): string {
  try {
    return JSON.stringify(value, null, 2) ?? String(value);
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

  const text = nonEmptyString(payload.text) ?? nonEmptyString(payload.message);
  if (text !== null) {
    return text;
  }

  if (payload.type === "done") {
    return "done";
  }

  return compactJson(payload);
}

/**
 * Untruncated display text for one serialized `SwarmMessage`. Findings show
 * their summary plus evidence; questions/challenges show their text; done
 * messages read "Done"; unfamiliar shapes fall back to pretty JSON.
 */
export function swarmPayloadFullText(payload: unknown): string {
  if (!isRecord(payload)) {
    return typeof payload === "string" ? payload : fullJson(payload);
  }
  const summary = nonEmptyString(payload.summary);
  if (summary !== null) {
    const evidence = nonEmptyString(payload.evidence);
    return evidence === null ? summary : `${summary}\n\nEvidence:\n${evidence}`;
  }
  const text = nonEmptyString(payload.text) ?? nonEmptyString(payload.message);
  if (text !== null) {
    return text;
  }
  if (payload.type === "done") {
    return "Done";
  }
  return fullJson(payload);
}

/** Extracts `round` from `payload.round` or a `…:r<N>:…` subtask id. */
export function swarmMessageRound(payload: unknown): number | null {
  if (!isRecord(payload)) {
    return null;
  }
  const explicit = nonNegativeInt(payload.round);
  if (explicit !== null) {
    return explicit;
  }
  const subtaskId = nonEmptyString(payload.subtask_id);
  if (subtaskId === null) {
    return null;
  }
  const match = /:r(\d+):/.exec(subtaskId);
  if (match === null) {
    return null;
  }
  const round = Number(match[1]);
  return Number.isSafeInteger(round) && round >= 0 ? round : null;
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
  if (typeof id !== "number" || !Number.isSafeInteger(id) || id < 0) {
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
    createdAt: typeof record.created_at === "string" ? record.created_at : "",
    summary: swarmPayloadSummary(payload),
    fullText: swarmPayloadFullText(payload),
    round: swarmMessageRound(payload),
    subtaskId: nonEmptyString(payload.subtask_id),
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

/** Counts used by the swarm panel header and group labels. */
export type SwarmCounts = {
  total: number;
  workers: number;
  rounds: number;
  byKind: Record<string, number>;
};

/** Counts one message list by kind, worker, and round. */
export function swarmMessageCounts(
  messages: readonly SwarmMessageView[],
): SwarmCounts {
  const workers = new Set<string>();
  const rounds = new Set<number>();
  const byKind: Record<string, number> = {};
  for (const message of messages) {
    workers.add(message.from ?? "unknown");
    if (message.round !== null) {
      rounds.add(message.round);
    }
    byKind[message.kind] = (byKind[message.kind] ?? 0) + 1;
  }
  return {
    total: messages.length,
    workers: workers.size,
    rounds: rounds.size,
    byKind,
  };
}

/** One round's messages for one worker. */
export type SwarmRoundGroup = {
  round: number | null;
  messages: SwarmMessageView[];
  counts: SwarmCounts;
};

/** One worker's messages, split by swarm round. */
export type SwarmWorkerGroup = {
  worker: string;
  rounds: SwarmRoundGroup[];
  counts: SwarmCounts;
};

function compareRounds(a: number | null, b: number | null): number {
  if (a === b) {
    return 0;
  }
  if (a === null) {
    return 1;
  }
  if (b === null) {
    return -1;
  }
  return a - b;
}

function compareStrings(a: string, b: string): number {
  return a < b ? -1 : a > b ? 1 : 0;
}

/**
 * Groups parsed messages by sender ("worker") and then round. Unknown senders
 * become `unknown`; messages without a derivable round are grouped under
 * `round: null` (rendered as "Round ?"). Groups sort worker alphabetically,
 * rounds ascending (unknown last), and messages by id.
 */
export function groupSwarmMessages(
  messages: readonly SwarmMessageView[],
): SwarmWorkerGroup[] {
  const workers = new Map<string, Map<number | null, SwarmMessageView[]>>();
  for (const message of messages) {
    const worker = message.from ?? "unknown";
    let rounds = workers.get(worker);
    if (rounds === undefined) {
      rounds = new Map();
      workers.set(worker, rounds);
    }
    const bucket = rounds.get(message.round);
    if (bucket === undefined) {
      rounds.set(message.round, [message]);
    } else {
      bucket.push(message);
    }
  }

  const groups: SwarmWorkerGroup[] = [];
  for (const [worker, roundMap] of workers) {
    const roundGroups: SwarmRoundGroup[] = [];
    for (const [round, bucket] of roundMap) {
      const ordered = [...bucket].sort((a, b) => a.id - b.id);
      roundGroups.push({
        round,
        messages: ordered,
        counts: swarmMessageCounts(ordered),
      });
    }
    roundGroups.sort((a, b) => compareRounds(a.round, b.round));
    const all = roundGroups.flatMap((group) => group.messages);
    groups.push({
      worker,
      rounds: roundGroups,
      counts: swarmMessageCounts(all),
    });
  }
  groups.sort((a, b) => compareStrings(a.worker, b.worker));
  return groups;
}

/**
 * True when a task's facts include a DHI+ team fact recorded by a swarm run.
 * The engine writes `{ "topology": "swarm" | "manager_worker", ... }` into the
 * one fact a team plan produces. This is a legacy fallback for work rows
 * fetched before `Task.topology` existed; new views prefer
 * {@link resolveTaskTopology}.
 */
export function hasSwarmTopology(
  facts: readonly { provenance?: unknown }[],
): boolean {
  return facts.some(
    (fact) => isRecord(fact.provenance) && fact.provenance.topology === "swarm",
  );
}
