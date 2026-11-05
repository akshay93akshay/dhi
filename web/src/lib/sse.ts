import type { Activity, JsonObject } from "./types";

/**
 * Typed wrapper around the `GET /api/stream?task_id=<uuid>&after=<id>` SSE
 * endpoint (axum `Sse`):
 *
 * - first frame: `event: hello` + `{"task_id": "..."}`
 * - then per new activity row: `event: activity` + `id: <n>` + the row JSON
 * - keep-alive comments hold the connection open while idle
 *
 * The connection auto-resumes. On a transparent platform retry the browser
 * re-requests the original URL and sends its `Last-Event-ID` header — the
 * DHI server ignores that header and filters by the `after` query param, so
 * replayed rows are dropped client-side by the cursor dedupe. When the
 * platform gives up (`readyState === CLOSED`) the wrapper rebuilds the URL
 * with `after=<last seen id>` and reconnects after a short backoff.
 *
 * Parsing is split into exported pure functions (`parseEventId`,
 * `parseHelloData`, `parseActivityEvent`, `parseFrame`) so frames can be
 * unit-tested without a live stream. Malformed frames never throw: they
 * are dropped.
 */

/** Delay before an explicit reconnect after the platform gives up, in ms. */
export const TASK_STREAM_RESUME_DELAY_MS = 1000;

const ES_CLOSED = 2;

export type TaskStreamOptions = {
  /** Cursor to resume from; only rows with `id > after` are delivered. Default 0. */
  after?: number;
  /** Fires once per (re)connection with the streamed task id. */
  onHello?: (taskId: string) => void;
  /** Fires per new activity row, in ascending `id` order, no duplicates. */
  onActivity?: (activity: Activity) => void;
  /** Observability hook for connection losses. */
  onError?: (error: unknown) => void;
  /** Overrides {@link TASK_STREAM_RESUME_DELAY_MS} (mostly for tests). */
  resumeDelayMs?: number;
};

/**
 * Opens the task's activity stream; returns an unsubscribe function that
 * closes the connection, cancels any pending reconnect, and stops delivery.
 * Throws synchronously where `EventSource` is unavailable (SSR, tests).
 */
export function connectTaskStream(
  taskId: string,
  opts: TaskStreamOptions = {},
): () => void {
  if (typeof globalThis.EventSource === "undefined") {
    throw new Error("EventSource is unavailable in this environment");
  }

  // Highest activity id delivered so far; doubles as the dedupe floor and
  // the `after` value on explicit rebuilds.
  let cursor = opts.after ?? 0;
  let disposed = false;
  let es: EventSource | null = null;
  let resumeTimer: ReturnType<typeof setTimeout> | null = null;
  const resumeDelayMs = opts.resumeDelayMs ?? TASK_STREAM_RESUME_DELAY_MS;

  const open = (): void => {
    if (disposed) {
      return;
    }
    const afterParam = cursor > 0 ? `&after=${cursor}` : "";
    es = new EventSource(
      `/api/stream?task_id=${encodeURIComponent(taskId)}${afterParam}`,
    );

    es.addEventListener("hello", (ev: MessageEvent<string>) => {
      const parsed = parseHelloData(typeof ev.data === "string" ? ev.data : "");
      if (parsed !== null) {
        opts.onHello?.(parsed);
      }
    });

    es.addEventListener("activity", (ev: MessageEvent<string>) => {
      const eventId = parseEventId(
        typeof ev.lastEventId === "string" ? ev.lastEventId : null,
      );
      const activity = parseActivityEvent(
        typeof ev.data === "string" ? ev.data : "",
        eventId,
      );
      if (activity === null) {
        return; // malformed row: tolerated, dropped
      }
      if (activity.id <= cursor) {
        return; // replayed row on a transparent reconnect: dedupe
      }
      cursor = activity.id;
      opts.onActivity?.(activity);
    });

    es.onerror = () => {
      opts.onError?.(new Error(`task stream disconnected for task ${taskId}`));
      if (disposed || es === null || es.readyState === ES_CLOSED) {
        // The platform gave up: rebuild the URL from our cursor.
        if (es !== null) {
          es.close();
          es = null;
        }
        scheduleReconnect();
      }
      // While CONNECTING the platform retries transparently; replayed rows
      // are dropped by the cursor dedupe above.
    };
  };

  const scheduleReconnect = (): void => {
    if (disposed || resumeTimer !== null) {
      return;
    }
    resumeTimer = setTimeout(() => {
      resumeTimer = null;
      open();
    }, resumeDelayMs);
  };

  open();

  return () => {
    disposed = true;
    if (resumeTimer !== null) {
      clearTimeout(resumeTimer);
      resumeTimer = null;
    }
    if (es !== null) {
      es.close();
      es = null;
    }
  };
}

/**
 * Parses an SSE `id:` field into the activity cursor. Accepts unsigned
 * decimal integers (whitespace tolerated); anything else is `null`.
 */
export function parseEventId(raw: string | null | undefined): number | null {
  if (raw === null || raw === undefined) {
    return null;
  }
  const trimmed = raw.trim();
  if (!/^[0-9]+$/.test(trimmed)) {
    return null;
  }
  const parsed = Number(trimmed);
  return Number.isSafeInteger(parsed) ? parsed : null;
}

function parseJsonObject(data: string): Record<string, unknown> | null {
  try {
    const parsed: unknown = JSON.parse(data);
    if (
      parsed !== null &&
      typeof parsed === "object" &&
      !Array.isArray(parsed)
    ) {
      return parsed as Record<string, unknown>;
    }
    return null;
  } catch {
    return null;
  }
}

/**
 * Parses a `hello` frame's data (`{"task_id": "..."}`) into the task id,
 * or `null` when the frame is malformed.
 */
export function parseHelloData(data: string): string | null {
  const parsed = parseJsonObject(data);
  const taskId = parsed?.task_id;
  return typeof taskId === "string" && taskId !== "" ? taskId : null;
}

/**
 * Parses one `activity` frame's data into an {@link Activity} row. `eventId`
 * (from the SSE `id:` field / `lastEventId`) takes precedence over the
 * JSON's own `id`. Returns `null` on anything unusable — callers drop it
 * and keep streaming.
 */
export function parseActivityEvent(
  data: string,
  eventId: number | null,
): Activity | null {
  const parsed = parseJsonObject(data);
  if (parsed === null) {
    return null;
  }
  const jsonId =
    typeof parsed.id === "number" && Number.isSafeInteger(parsed.id)
      ? parsed.id
      : null;
  const id = eventId ?? jsonId;
  if (id === null || id < 0) {
    return null;
  }
  const taskId = parsed.task_id;
  const kind = parsed.kind;
  const message = parsed.message;
  const createdAt = parsed.created_at;
  if (
    typeof taskId !== "string" ||
    typeof kind !== "string" ||
    typeof message !== "string" ||
    typeof createdAt !== "string"
  ) {
    return null;
  }
  const payload: JsonObject =
    parsed.payload !== null &&
    parsed.payload !== undefined &&
    typeof parsed.payload === "object"
      ? (parsed.payload as JsonObject)
      : {};
  return {
    id,
    task_id: taskId,
    kind,
    message,
    payload,
    created_at: createdAt,
  };
}

/** Discriminated result of {@link parseFrame}. */
export type ParsedFrame =
  | { kind: "hello"; taskId: string }
  | { kind: "activity"; activity: Activity }
  /** Nothing actionable: keep-alive comments, blanks, unknown events. */
  | { kind: "ignored" };

/**
 * Parses one raw SSE frame (a chunk of `field: value` lines, terminated by
 * a blank line — a trailing blank is tolerated). Comments (`: keep-alive`)
 * and unrecognized events yield `{ kind: "ignored" }`; malformed payloads
 * yield `null`. Multi-line `data:` values are joined with `\n`.
 */
export function parseFrame(frame: string): ParsedFrame | null {
  let event: string | null = null;
  let id: number | null = null;
  const dataLines: string[] = [];

  for (const line of frame.split(/\r?\n/)) {
    if (line === "" || line.startsWith(":")) {
      continue; // blank separator or SSE comment (keep-alive)
    }
    const colon = line.indexOf(":");
    const field = colon === -1 ? line : line.slice(0, colon);
    let value = colon === -1 ? "" : line.slice(colon + 1);
    if (value.startsWith(" ")) {
      value = value.slice(1);
    }
    switch (field) {
      case "event":
        event = value;
        break;
      case "id":
        id = parseEventId(value);
        break;
      case "data":
        dataLines.push(value);
        break;
      default:
        break; // other fields (retry:, …) are ignored
    }
  }

  if (dataLines.length === 0) {
    return { kind: "ignored" };
  }
  const data = dataLines.join("\n");
  if (event === "hello") {
    const taskId = parseHelloData(data);
    return taskId === null ? null : { kind: "hello", taskId };
  }
  if (event === "activity" || event === null) {
    const activity = parseActivityEvent(data, id);
    if (activity !== null) {
      return { kind: "activity", activity };
    }
    if (event === "activity") {
      return null;
    }
  }
  return { kind: "ignored" };
}
