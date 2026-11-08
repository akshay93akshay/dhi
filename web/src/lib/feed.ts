/**
 * Pure logic and headless orchestration for the live activity feed.
 *
 * The wire truth (crates/dhi-server/src/lib.rs `stream_activity`): the first
 * SSE frame is `event: hello`, then one `event: activity` frame per new row
 * carrying `id: <n>` and the serialized `Activity` row, with keep-alive
 * comments while idle. Rows arrive ascending; a reconnect replays whatever
 * the server still considers "new" (the `after` query param is the only
 * resume filter — the server ignores `Last-Event-ID`).
 *
 * Layers, all JavaScript-object logic with no React and no DOM:
 *
 * - `mergeTimeline` — THE sequencing primitive. Given history rows, newly
 *   arrived rows, and the max-seen cursor, it produces the deduped,
 *   ascending timeline plus the advanced cursor. Idempotent against the
 *   overlap/replay races of a refresh (history + stream) and reconnects.
 * - `capTimeline` — client-side DOM protection (the history route is
 *   unbounded); the live and recent views both render only the newest ten.
 * - `activityKindView` / `activityActor` — kind strings as served are shown
 *   verbatim; known kinds additionally map onto the shared badge color
 *   vocabulary and a coarse actor label. Unknown/hostile kinds never crash.
 * - `activityToolIo` / `previewFeedText` — one combined input/output cell for
 *   `tool_allowed` rows, with a short collapsed preview and a full-text
 *   expansion; old rows without `input` render output only.
 * - `advanceFeedConnection` — the connection-indicator state machine
 *   (connecting/live/reconnecting → polling fallback after N error streak).
 * - `createLiveFeedController` — the headless feed controller the work-view
 *   hook binds to React state: seeds from history, opens the stream with
 *   `after=<max historical id>`, appends stream rows, degrades to activity
 *   polling after persistent stream errors, periodically retries the stream,
 *   and heals the timeline on tab-visible transitions.
 *
 * Kept framework-free so `feed.test.ts` can drive every path under
 * `node --test` with a stubbed loader and a scripted EventSource fake.
 */

import { getTaskActivity } from "./api.ts";
import { formatLocalClock } from "./localTime.ts";
import { connectTaskStream, TASK_STREAM_RESUME_DELAY_MS } from "./sse.ts";
import type { Activity } from "./types.ts";

// ---------------------------------------------------------------------------
// Tunables (surfaced for tests and docs)
// ---------------------------------------------------------------------------

/**
 * Rows shown by the live work-view feed. The task detail's compact list uses
 * the same window: exactly the newest ten events.
 */
export const FEED_LIVE_COUNT = 10;

/** Rows shown by the task detail page's compact "Recent activity" list. */
export const FEED_RECENT_COUNT = FEED_LIVE_COUNT;

/** Collapsed preview length for tool input/output text. */
export const FEED_TOOL_PREVIEW_CHARS = 280;

/** Consecutive stream error events after which the feed falls back to polling. */
export const FEED_FALLBACK_ERROR_STREAK = 5;

/** Poll cadence while the stream is degraded (matches the 5B page cadence). */
export const FEED_POLL_INTERVAL_MS = 5000;

/** How often the fallback polls attempt to win the stream back. */
export const FEED_STREAM_RETRY_MS = 30_000;

/** Debounce for work-graph refreshes triggered by live activity rows. */
export const FEED_GRAPH_REFRESH_DEBOUNCE_MS = 2500;

// ---------------------------------------------------------------------------
// Timeline sequencing (pure)
// ---------------------------------------------------------------------------

/** Result of {@link mergeTimeline}: the merged timeline plus the new cursor. */
export type TimelineMergeResult = {
  /** Rows ascending by `id`, deduplicated, all with `id > cursor` (pre-merge). */
  rows: Activity[];
  /**
   * The max-seen activity id: `max(cursor, every valid id observed)`. It
   * advances even for duplicate deliveries — the cursor means "seen", not
   * "stored" — and never decreases.
   */
  cursor: number;
};

function isUsableActivityRow(value: unknown): value is Activity {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    return false;
  }
  const id = (value as { id?: unknown }).id;
  return typeof id === "number" && Number.isSafeInteger(id) && id >= 0;
}

/**
 * Merges the accumulated timeline with newly arrived rows under the
 * max-seen cursor:
 *
 * - `history` (the timeline so far, e.g. the seeded/fetched rows) is
 *   absorbed AT FACE VALUE — carried rows are never evicted by the rising
 *   cursor;
 * - `incoming` obeys the floor: rows with `id <= cursor` are dropped
 *   (already-seen: the classic reconnect-replay / history-overlap case);
 * - hostile shapes (nulls, primitives, arrays, missing/garbage ids) are
 *   dropped without throwing; rows with a sane `id` survive even if their
 *   other fields are junk — the renderer defends against those;
 * - duplicate ids keep the FIRST occurrence (history beats re-delivery);
 * - the result is ascending by `id`, so out-of-order arrival reorders safely.
 *
 * The persistence guarantee: rebuilding from a fresh history fetch plus the
 * live stream after a page refresh reproduces the same timeline — same ids,
 * same order, no gaps (the stream is opened with `after=<max historical id>`
 * so the only overlap is bounded by the connect race, and the cursor guard
 * absorbs it). Batches respect the wire's ascending-above-`after` shape;
 * the cursor only ever advances.
 */
export function mergeTimeline(
  history: readonly unknown[],
  incoming: readonly unknown[],
  cursor: number,
): TimelineMergeResult {
  const baseCursor =
    Number.isFinite(cursor) && cursor > 0 ? Math.floor(cursor) : 0;
  const byId = new Map<number, Activity>();
  let maxSeen = baseCursor;

  for (const row of history) {
    if (!isUsableActivityRow(row)) {
      continue;
    }
    if (row.id > maxSeen) {
      maxSeen = row.id;
    }
    if (!byId.has(row.id)) {
      byId.set(row.id, row);
    }
  }
  for (const row of incoming) {
    if (!isUsableActivityRow(row)) {
      continue;
    }
    if (row.id > maxSeen) {
      maxSeen = row.id;
    }
    if (row.id <= baseCursor) {
      continue; // already accounted for at or below the resume floor
    }
    if (!byId.has(row.id)) {
      byId.set(row.id, row);
    }
  }

  const rows = [...byId.values()].sort((a, b) => a.id - b.id);
  return { rows, cursor: maxSeen };
}

/** Result of {@link capTimeline}. */
export type TimelineCap = {
  /** The newest `max` rows (ascending, input order preserved). */
  kept: Activity[];
  /** How many OLDER rows were dropped from rendering. */
  hidden: number;
};

/**
 * Keeps the newest `max` rows for rendering and counts the rest as hidden.
 * Protects the DOM from the unbounded history response; the feed never shows
 * a hidden-count line — the window is simply the newest ten.
 */
export function capTimeline(
  rows: readonly Activity[],
  max: number = FEED_LIVE_COUNT,
): TimelineCap {
  const cap = Number.isSafeInteger(max) && max > 0 ? max : FEED_LIVE_COUNT;
  if (rows.length <= cap) {
    return { kept: rows.slice(), hidden: 0 };
  }
  return { kept: rows.slice(rows.length - cap), hidden: rows.length - cap };
}

// ---------------------------------------------------------------------------
// Kind / actor presentation
// ---------------------------------------------------------------------------

/**
 * Known activity kinds (as emitted by `dhi-engine` / `dhi-store`) mapped
 * onto the shared badge `data-state` vocabulary from globals.css. Unknown
 * kinds get NO state attribute — the neutral badge style — and their raw
 * string as the label.
 */
const KIND_TONES: Readonly<Record<string, string>> = {
  task_created: "created",
  intents_added: "open",
  intent_claimed: "running",
  intent_done: "done",
  goal_proven: "met",
  tool_allowed: "ok",
  tool_denied: "denied",
  approval_denied: "denied",
  approval_expired: "timeout",
  intent_orphaned: "blocked",
  engine_timeout: "timeout",
  capacity_exhausted: "exhausted",
  task_done: "done",
};

/**
 * Coarse actor attribution per kind. The wire has no actor field
 * (`Activity` = id/task_id/kind/message/payload/created_at), so this is a
 * derivation from the well-known kinds, displayed as a quiet label.
 * Unknown kinds yield `null` — no actor chip is rendered.
 */
const KIND_ACTORS: Readonly<Record<string, "planner" | "worker" | "system">> = {
  intents_added: "planner",
  goal_proven: "planner",
  intent_claimed: "worker",
  intent_done: "worker",
  task_created: "system",
  task_done: "system",
  tool_allowed: "system",
  tool_denied: "system",
  approval_denied: "system",
  approval_expired: "system",
  intent_orphaned: "system",
  engine_timeout: "system",
  capacity_exhausted: "system",
};

/** Renderable view of one activity kind: badge label plus optional tone. */
export type ActivityKindView = {
  label: string;
  /** A `data-state` token from the shared badge vocabulary, or `null`. */
  tone: string | null;
};

/** Stringifies anything defensively; used for kind labels. Never throws. */
function safeKindLabel(kind: unknown): string {
  if (typeof kind === "string") {
    return kind === "" ? "event" : kind;
  }
  if (typeof kind === "number" || typeof kind === "boolean") {
    return String(kind);
  }
  if (kind === null || kind === undefined) {
    return "event";
  }
  try {
    const json = JSON.stringify(kind);
    if (typeof json === "string" && json !== "") {
      return json;
    }
  } catch {
    // Fall through to String().
  }
  try {
    return String(kind);
  } catch {
    return "event";
  }
}

/**
 * Maps an activity kind onto its badge presentation. Falls back to the raw
 * kind string (or a benign placeholder for non-string junk) with no tone;
 * hostile input never crashes.
 */
export function activityKindView(kind: unknown): ActivityKindView {
  const label = safeKindLabel(kind);
  const tone =
    typeof kind === "string" &&
    Object.prototype.hasOwnProperty.call(KIND_TONES, kind)
      ? KIND_TONES[kind]
      : null;
  return { label, tone: tone ?? null };
}

/**
 * Actor label for a kind (`planner` / `worker` / `system`), or `null` when
 * the kind is unknown — "if present", per the feed spec.
 */
export function activityActor(
  kind: unknown,
): "planner" | "worker" | "system" | null {
  if (typeof kind !== "string") {
    return null;
  }
  return Object.prototype.hasOwnProperty.call(KIND_ACTORS, kind)
    ? KIND_ACTORS[kind]
    : null;
}

/** Short system-local clock (`HH:MM:SS`) for feed rows; unparseable input passes through. */
export function formatFeedTime(iso: string): string {
  return formatLocalClock(iso);
}

/** Pretty-prints a payload for the expandable JSON block. Never throws. */
export function prettyJson(value: unknown): string {
  try {
    return JSON.stringify(value, null, 2) ?? String(value);
  } catch {
    try {
      return String(value);
    } catch {
      return "[unprintable]";
    }
  }
}

/** Whether a payload object has anything to expand (empty/default → no). */
export function hasVisiblePayload(payload: unknown): boolean {
  return (
    typeof payload === "object" &&
    payload !== null &&
    !Array.isArray(payload) &&
    Object.keys(payload as JsonObjectLike).length > 0
  );
}

type JsonObjectLike = { [key: string]: unknown };

function isJsonObject(value: unknown): value is JsonObjectLike {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function hasOwn(object: JsonObjectLike, key: string): boolean {
  return Object.prototype.hasOwnProperty.call(object, key);
}

/** One tool call's combined input/output text, ready for a single feed cell. */
export type FeedToolIo = {
  /** `payload.input` formatted for display, or `null` on old rows. */
  input: string | null;
  /** `payload.output` formatted for display, or `null` when absent. */
  output: string | null;
};

/**
 * Formats one tool value. Strings pass through; server-capped output objects
 * (`{ truncated: true, preview: "…" }`) show their preview; every other JSON
 * value is pretty-printed. `null`/missing values mean "no such side".
 */
export function formatToolValue(value: unknown): string | null {
  if (value === null || value === undefined) {
    return null;
  }
  if (typeof value === "string") {
    return value;
  }
  if (
    isJsonObject(value) &&
    value.truncated === true &&
    typeof value.preview === "string"
  ) {
    return `${value.preview}\n… (truncated by the server)`;
  }
  return prettyJson(value);
}

/**
 * Extracts the combined input/output cell for a `tool_allowed` activity.
 * Rows written before the contract gained `input` simply return output only.
 * Non-tool activity without either key returns `null` so its payload keeps
 * the generic JSON expansion.
 */
export function activityToolIo(activity: Activity): FeedToolIo | null {
  if (!isJsonObject(activity.payload)) {
    return null;
  }
  const payload = activity.payload;
  const hasInput = hasOwn(payload, "input");
  const hasOutput = hasOwn(payload, "output");
  const looksLikeToolCall =
    activity.kind === "tool_allowed" || hasOwn(payload, "tool");
  if (!looksLikeToolCall && !hasInput && !hasOutput) {
    return null;
  }
  const input = hasInput ? formatToolValue(payload.input) : null;
  const output = hasOutput ? formatToolValue(payload.output) : null;
  if (input === null && output === null) {
    return null;
  }
  return { input, output };
}

/** Result of {@link previewFeedText}: collapsed text plus a truncation flag. */
export type FeedTextPreview = {
  preview: string;
  truncated: boolean;
};

/**
 * Truncates long tool text for the collapsed cell. The returned preview ends
 * with an ellipsis when expanded text exists, which is the expand cue.
 */
export function previewFeedText(
  text: string,
  maxChars: number = FEED_TOOL_PREVIEW_CHARS,
): FeedTextPreview {
  const cap =
    Number.isSafeInteger(maxChars) && maxChars > 0
      ? maxChars
      : FEED_TOOL_PREVIEW_CHARS;
  if (text.length <= cap) {
    return { preview: text, truncated: false };
  }
  return { preview: `${text.slice(0, cap - 1)}…`, truncated: true };
}

/** Full combined text copied from the expanded tool cell. */
export function toolIoFullText(io: FeedToolIo): string {
  const parts: string[] = [];
  if (io.input !== null) {
    parts.push(`Input:\n${io.input}`);
  }
  if (io.output !== null) {
    parts.push(`Output:\n${io.output}`);
  }
  return parts.join("\n\n");
}

// ---------------------------------------------------------------------------
// Connection indicator state machine (pure)
// ---------------------------------------------------------------------------

/**
 * Indicator states for the live log header:
 * - `connecting` — stream requested, hello not yet seen;
 * - `live` — hello seen (green pulsing dot);
 * - `reconnecting` — errors below the fallback streak (amber dot);
 * - `polling` — degraded: stream parked, activity polling sustains the log
 *   (grey dot; the stream is retried periodically).
 */
export type FeedConnection = "connecting" | "live" | "reconnecting" | "polling";

/** One observable stream event. */
export type FeedClockEvent = "hello" | "error";

/** Result of {@link advanceFeedConnection}. */
export type FeedConnectionAdvance = {
  state: FeedConnection;
  /** True when THIS transition crossed into the polling fallback. */
  fallbackTriggered: boolean;
};

/**
 * Advances the connection indicator. `hello` always wins (any state → live;
 * the server sends hello on every successful (re)connection). `error` from
 * an active stream escalates: below {@link FEED_FALLBACK_ERROR_STREAK}
 * consecutive errors the state is `reconnecting` (the wrapper is still
 * retrying — transparent platform retries or explicit `after`-rebuilt
 * resumes); at the limit the feed parks the stream and switches to
 * `polling`. Errors while already `polling` stay `polling` (each one just
 * schedules the next stream retry).
 *
 * The error streak is cumulative across reconnect attempts WITHIN a
 * degradation episode — resetting it per attempt would defeat the fallback
 * when each rebuild fails exactly once. It is zeroed on every `hello`.
 */
export function advanceFeedConnection(
  prev: FeedConnection,
  event: FeedClockEvent,
  errorStreak: number,
  streakLimit: number,
): FeedConnectionAdvance {
  if (event === "hello") {
    return { state: "live", fallbackTriggered: false };
  }
  if (prev === "polling") {
    return { state: "polling", fallbackTriggered: false };
  }
  if (
    Number.isSafeInteger(streakLimit) &&
    streakLimit > 0 &&
    errorStreak >= streakLimit
  ) {
    return { state: "polling", fallbackTriggered: true };
  }
  return { state: "reconnecting", fallbackTriggered: false };
}

// ---------------------------------------------------------------------------
// Headless live-feed controller
// ---------------------------------------------------------------------------

/** Signals the controller emits to interested owners (the work view). */
export type FeedSignal =
  | { kind: "connection"; connection: FeedConnection }
  /** Stream rows were appended — the owning page may refresh its graph. */
  | { kind: "live-rows" };

/** Immutable snapshot pushed to subscribers (mirrors the hook's state). */
export type LiveFeedSnapshot = {
  rows: Activity[];
  cursor: number;
  connection: FeedConnection;
  /** Failure of the INITIAL history seed; the feed still streams. */
  historyError: string | null;
  /** Failure of a fallback-poll tick. */
  pollError: string | null;
};

/** Streaming dependency, shaped exactly like the `sse.ts` wrapper's needs. */
export type StreamConnect = (
  taskId: string,
  after: number,
  handlers: {
    onHello: (taskId: string) => void;
    onActivity: (activity: Activity) => void;
    onError: () => void;
  },
) => () => void;

export type LiveFeedDeps = {
  /** History/loader used for seeding, healing, and fallback polling. */
  loadActivity?: (taskId: string) => Promise<Activity[]>;
  /** Stream opener; defaults to the `sse.ts` wrapper. */
  connect?: StreamConnect;
  pollIntervalMs?: number;
  streamRetryMs?: number;
  errorStreakLimit?: number;
  /** Passthrough to the wrapper's resume backoff (tests shrink this). */
  streamResumeDelayMs?: number;
  describeError?: (err: unknown) => string;
  /** Visibilities gate polling ticks; defaults to the document state. */
  isVisible?: () => boolean;
  /** Signal sink for the owning page (see {@link FeedSignal}). */
  onSignal?: (signal: FeedSignal) => void;
};

export type LiveFeedController = {
  /** Seeds from history, then opens the stream. Idempotent. */
  start(): void;
  /** Stops everything: stream, timers, listener delivery. */
  dispose(): void;
  /** Registers a snapshot listener; returns the unsubscriber. */
  subscribe(listener: (snapshot: LiveFeedSnapshot) => void): () => void;
  /** Notified on tab visibility transitions (see the 5B visibility stance). */
  visibility(visible: boolean): void;
  /** Current snapshot, for late subscribers and tests. */
  snapshot(): LiveFeedSnapshot;
};

function defaultDescribeError(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

function defaultIsVisible(): boolean {
  return (
    typeof document === "undefined" || document.visibilityState !== "hidden"
  );
}

function makeDefaultConnect(resumeDelayMs: number | undefined): StreamConnect {
  return (taskId, after, handlers) =>
    connectTaskStream(taskId, {
      after,
      onHello: handlers.onHello,
      onActivity: handlers.onActivity,
      onError: () => handlers.onError(),
      ...(resumeDelayMs === undefined ? {} : { resumeDelayMs }),
    });
}

/**
 * Builds the headless controller owning the ENTIRE live-feed lifecycle:
 *
 * 1. `start()` fetches history (`getTaskActivity`) and seeds the timeline;
 * 2. the stream opens with `after = <max historical id>`, so server rows
 *    and stream rows overlap as little as possible;
 * 3. every batch — history, stream row, heal, poll — funnels through
 *    {@link mergeTimeline}'s cursor guard, making duplication impossible;
 * 4. `FEED_FALLBACK_ERROR_STREAK` consecutive stream errors park the stream
 *    and start polling the activity route; while polling, the stream is
 *    retried every `FEED_STREAM_RETRY_MS` until a hello wins it back;
 * 5. on becoming visible: polling ticks immediately; a live stream gets a
 *    healing history refetch (merges are idempotent, so this only repairs
 *    hypothetical hidden-tab gaps);
 * 6. `dispose()` tears down timers, the stream, and listener delivery.
 *
 * Keep-alive comment frames never reach these callbacks (the EventSource
 * only fires named events), so idle connections cause zero state churn.
 */
export function createLiveFeedController(
  taskId: string,
  deps: LiveFeedDeps = {},
): LiveFeedController {
  const load = deps.loadActivity ?? getTaskActivity;
  const connect = deps.connect ?? makeDefaultConnect(deps.streamResumeDelayMs);
  const pollIntervalMs = deps.pollIntervalMs ?? FEED_POLL_INTERVAL_MS;
  const streamRetryMs = deps.streamRetryMs ?? FEED_STREAM_RETRY_MS;
  const errorStreakLimit = deps.errorStreakLimit ?? FEED_FALLBACK_ERROR_STREAK;
  const describeError = deps.describeError ?? defaultDescribeError;
  const isVisible = deps.isVisible ?? defaultIsVisible;

  const listeners = new Set<(snapshot: LiveFeedSnapshot) => void>();

  let rows: Activity[] = [];
  let cursor = 0;
  let connection: FeedConnection = "connecting";
  let historyError: string | null = null;
  let pollError: string | null = null;

  let started = false;
  let disposed = false;
  let polling = false;
  let streak = 0;
  let unsubStream: (() => void) | null = null;
  let pollTimer: ReturnType<typeof setInterval> | null = null;
  let retryTimer: ReturnType<typeof setTimeout> | null = null;

  const snapshot = (): LiveFeedSnapshot => ({
    rows,
    cursor,
    connection,
    historyError,
    pollError,
  });

  const emit = (): void => {
    if (disposed) {
      return;
    }
    const snap = snapshot();
    for (const listener of listeners) {
      listener(snap);
    }
  };

  const setConnection = (next: FeedConnection): void => {
    if (next === connection) {
      return;
    }
    connection = next;
    deps.onSignal?.({ kind: "connection", connection: next });
    emit();
  };

  const applyBatch = (
    batch: readonly unknown[],
    source: "stream" | "load",
  ): void => {
    const merged = mergeTimeline(rows, batch, cursor);
    const grew = merged.rows.length > rows.length;
    rows = merged.rows;
    cursor = merged.cursor;
    if (source === "stream" && grew) {
      deps.onSignal?.({ kind: "live-rows" });
    }
    emit();
  };

  const pollTick = async (): Promise<void> => {
    if (disposed) {
      return;
    }
    try {
      const history = await load(taskId);
      if (disposed) {
        return;
      }
      applyBatch(history, "load");
      historyError = null;
      pollError = null;
      emit();
    } catch (err) {
      if (disposed) {
        return;
      }
      pollError = describeError(err);
      emit();
    }
  };

  /** Quiet history refetch while the stream is up; repairs hidden-tab gaps. */
  const healWithHistory = async (): Promise<void> => {
    if (disposed) {
      return;
    }
    try {
      const history = await load(taskId);
      if (disposed) {
        return;
      }
      applyBatch(history, "load");
      historyError = null;
      emit();
    } catch {
      // A healing miss is not worth an error panel — the stream itself and
      // the next visibility/poll cycle keep the log honest.
    }
  };

  const clearPollTimers = (): void => {
    if (pollTimer !== null) {
      clearInterval(pollTimer);
      pollTimer = null;
    }
    if (retryTimer !== null) {
      clearTimeout(retryTimer);
      retryTimer = null;
    }
  };

  const scheduleStreamRetry = (): void => {
    if (retryTimer !== null || disposed) {
      return;
    }
    retryTimer = setTimeout(() => {
      retryTimer = null;
      if (!disposed && polling) {
        openStream();
      }
    }, streamRetryMs);
  };

  const enterPolling = (): void => {
    if (polling || disposed) {
      return;
    }
    polling = true;
    streak = 0;
    if (unsubStream !== null) {
      unsubStream();
      unsubStream = null;
    }
    setConnection("polling");
    void pollTick();
    pollTimer = setInterval(() => {
      if (disposed || !isVisible()) {
        return;
      }
      void pollTick();
    }, pollIntervalMs);
    scheduleStreamRetry();
  };

  const handleHello = (): void => {
    if (disposed) {
      return;
    }
    streak = 0;
    if (polling) {
      polling = false;
      clearPollTimers();
      pollError = null;
    }
    setConnection("live");
  };

  const handleActivity = (row: Activity): void => {
    if (disposed) {
      return;
    }
    applyBatch([row], "stream");
  };

  const handleStreamError = (): void => {
    if (disposed) {
      return;
    }
    streak += 1;
    const advance = advanceFeedConnection(
      connection,
      "error",
      streak,
      errorStreakLimit,
    );
    setConnection(advance.state);
    if (advance.fallbackTriggered) {
      enterPolling();
    } else if (advance.state === "polling") {
      // A retry attempt from the fallback failed — schedule the next one.
      scheduleStreamRetry();
    }
  };

  const openStream = (): void => {
    if (disposed) {
      return;
    }
    if (unsubStream !== null) {
      unsubStream();
      unsubStream = null;
    }
    // While the fallback polls, quiet retries do not flash "connecting".
    if (connection !== "polling") {
      setConnection("connecting");
    }
    unsubStream = connect(taskId, cursor, {
      onHello: handleHello,
      onActivity: handleActivity,
      onError: handleStreamError,
    });
  };

  const start = (): void => {
    if (started || disposed) {
      return;
    }
    started = true;
    void (async () => {
      try {
        const history = await load(taskId);
        if (disposed) {
          return;
        }
        applyBatch(history, "load");
        historyError = null;
        pollError = null;
        emit();
      } catch (err) {
        if (disposed) {
          return;
        }
        // A failed seed must not block the live path: the stream still
        // opens with after=0 and the next load (heal/poll) retries.
        historyError = describeError(err);
        emit();
      }
      openStream();
    })();
  };

  const visibility = (visible: boolean): void => {
    if (disposed || !started || !visible) {
      return;
    }
    if (polling) {
      void pollTick();
    } else {
      void healWithHistory();
    }
  };

  const dispose = (): void => {
    disposed = true;
    clearPollTimers();
    if (unsubStream !== null) {
      unsubStream();
      unsubStream = null;
    }
    polling = false;
    listeners.clear();
  };

  const subscribe = (
    listener: (snapshot: LiveFeedSnapshot) => void,
  ): (() => void) => {
    listeners.add(listener);
    return () => {
      listeners.delete(listener);
    };
  };

  return { start, dispose, subscribe, visibility, snapshot };
}

/** Resume delay passthrough re-exported for tests convenience. */
export { TASK_STREAM_RESUME_DELAY_MS };
