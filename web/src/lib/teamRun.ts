/**
 * Pure parsing/selection helpers for DHI+ `team_run` activities.
 *
 * The engine records one `team_run` activity when a manager/worker or swarm
 * intent finishes. The documented payload is:
 *
 * ```json
 * {
 *   "intent_id": "…",
 *   "topology": "manager_worker" | "swarm",
 *   "workers": [{"key": "…", "role": "…", "status": "ok|failed|skipped", "subtask": "…"}],
 *   "worker_keys": ["…"],
 *   "success": 2, "failed": 0, "skipped": 0
 * }
 * ```
 *
 * Older engine builds wrote `workers` as the worker COUNT and named the
 * workers only through `worker_keys`, so the parser accepts both shapes and
 * fills key-only worker rows from `worker_keys`. Every field is read
 * defensively: malformed activities/payloads yield `null` and malformed
 * worker entries are dropped instead of throwing. React-free so the unit
 * tests can drive it directly under `node --test`.
 */

/** Activity kind carrying a completed DHI+ team run. */
export const TEAM_RUN_KIND = "team_run";

/** The three outcomes the engine records per team worker. */
export type TeamRunWorkerStatus = "ok" | "failed" | "skipped";

/** One worker as displayed in the team-run card. */
export type TeamRunWorker = {
  key: string;
  role: string | null;
  status: TeamRunWorkerStatus | null;
  /** Subtask id/description when the payload carries one. */
  subtask: string | null;
};

/** Parsed `team_run` payload, without the surrounding activity columns. */
export type TeamRunPayload = {
  intentId: string | null;
  topology: string;
  /** Workers sorted by key for deterministic display. */
  workers: TeamRunWorker[];
  workerCount: number;
  success: number;
  failed: number;
  skipped: number;
};

/** A parsed `team_run` activity plus its activity-row metadata. */
export type TeamRun = TeamRunPayload & {
  activityId: number;
  createdAt: string;
};

type JsonRecord = { [key: string]: unknown };

function isRecord(value: unknown): value is JsonRecord {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function nonEmptyString(value: unknown): string | null {
  return typeof value === "string" && value.trim() !== "" ? value : null;
}

function nonNegativeInt(value: unknown): number | null {
  return typeof value === "number" &&
    Number.isSafeInteger(value) &&
    value >= 0
    ? value
    : null;
}

function workerStatus(value: unknown): TeamRunWorkerStatus | null {
  switch (value) {
    case "ok":
    case "failed":
    case "skipped":
      return value;
    default:
      return null;
  }
}

function parseWorker(value: unknown): TeamRunWorker | null {
  // Tolerate a bare key list as well as the documented worker objects.
  if (typeof value === "string") {
    const key = nonEmptyString(value);
    return key === null
      ? null
      : { key, role: null, status: null, subtask: null };
  }
  if (!isRecord(value)) {
    return null;
  }
  const key =
    nonEmptyString(value.key) ?? nonEmptyString(value.worker_key);
  if (key === null) {
    return null;
  }
  return {
    key,
    role: nonEmptyString(value.role),
    status: workerStatus(value.status),
    subtask:
      nonEmptyString(value.subtask) ?? nonEmptyString(value.subtask_id),
  };
}

function parseWorkerKeys(value: unknown): string[] {
  if (!Array.isArray(value)) {
    return [];
  }
  const keys: string[] = [];
  const seen = new Set<string>();
  for (const item of value) {
    const key = nonEmptyString(item);
    if (key !== null && !seen.has(key)) {
      seen.add(key);
      keys.push(key);
    }
  }
  return keys;
}

function compareStrings(a: string, b: string): number {
  return a < b ? -1 : a > b ? 1 : 0;
}

function compareNullableStrings(a: string | null, b: string | null): number {
  return compareStrings(a ?? "", b ?? "");
}

/**
 * Returns a copy sorted by worker key. Role/status/subtask act as
 * tie-breakers so duplicate keys also render deterministically.
 */
export function sortTeamWorkers(
  workers: readonly TeamRunWorker[],
): TeamRunWorker[] {
  return [...workers].sort(
    (a, b) =>
      compareStrings(a.key, b.key) ||
      compareNullableStrings(a.role, b.role) ||
      compareNullableStrings(a.status, b.status) ||
      compareNullableStrings(a.subtask, b.subtask),
  );
}

function statusCount(
  workers: readonly TeamRunWorker[],
  status: TeamRunWorkerStatus,
): number {
  return workers.filter((worker) => worker.status === status).length;
}

/**
 * Parses one `team_run` payload. Returns `null` for non-object payloads and
 * payloads without a usable topology; otherwise every field degrades to a
 * safe default (unknown worker statuses become `null`, bad counts fall back
 * to status totals, malformed worker entries are skipped).
 */
export function parseTeamRunPayload(
  payload: unknown,
): TeamRunPayload | null {
  if (!isRecord(payload)) {
    return null;
  }
  const topology = nonEmptyString(payload.topology);
  if (topology === null) {
    return null;
  }

  const workers: TeamRunWorker[] = [];
  if (Array.isArray(payload.workers)) {
    for (const item of payload.workers) {
      const worker = parseWorker(item);
      if (worker !== null) {
        workers.push(worker);
      }
    }
  }

  // `worker_keys` is authoritative for names in older payloads; append any
  // key the richer worker array did not already describe.
  const workerKeys = parseWorkerKeys(payload.worker_keys);
  const knownKeys = new Set(workers.map((worker) => worker.key));
  for (const key of workerKeys) {
    if (!knownKeys.has(key)) {
      knownKeys.add(key);
      workers.push({
        key,
        role: null,
        status: null,
        subtask: null,
      });
    }
  }

  // The old wire shape stores the count directly under `workers`; the new
  // shape stores the array there. Prefer the largest trustworthy signal so
  // a key list can never be smaller than the displayed count.
  const explicitCount = nonNegativeInt(payload.workers) ?? 0;
  const workerCount = Math.max(
    explicitCount,
    workers.length,
    workerKeys.length,
  );

  return {
    intentId: nonEmptyString(payload.intent_id),
    topology,
    workers: sortTeamWorkers(workers),
    workerCount,
    success:
      nonNegativeInt(payload.success) ?? statusCount(workers, "ok"),
    failed:
      nonNegativeInt(payload.failed) ?? statusCount(workers, "failed"),
    skipped:
      nonNegativeInt(payload.skipped) ?? statusCount(workers, "skipped"),
  };
}

/**
 * Parses one activity row, requiring kind `team_run`, a valid activity id,
 * and a parseable payload. Anything else is ignored.
 */
export function parseTeamRunActivity(activity: unknown): TeamRun | null {
  if (!isRecord(activity) || activity.kind !== TEAM_RUN_KIND) {
    return null;
  }
  const activityId = nonNegativeInt(activity.id);
  if (activityId === null) {
    return null;
  }
  const payload = parseTeamRunPayload(activity.payload);
  if (payload === null) {
    return null;
  }
  return {
    ...payload,
    activityId,
    createdAt:
      typeof activity.created_at === "string" ? activity.created_at : "",
  };
}

/**
 * Selects the most recent `team_run` from a feed timeline. Rows may be
 * hostile/unordered; malformed and unrelated rows are ignored.
 */
export function latestTeamRun(rows: readonly unknown[]): TeamRun | null {
  let latest: TeamRun | null = null;
  for (const row of rows) {
    const run = parseTeamRunActivity(row);
    if (run !== null && (latest === null || run.activityId > latest.activityId)) {
      latest = run;
    }
  }
  return latest;
}
