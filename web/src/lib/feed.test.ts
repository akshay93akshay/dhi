import assert from "node:assert/strict";
import { afterEach, test } from "node:test";
import {
  FEED_FALLBACK_ERROR_STREAK,
  FEED_GRAPH_REFRESH_DEBOUNCE_MS,
  FEED_LIVE_COUNT,
  FEED_RECENT_COUNT,
  FEED_STREAM_RETRY_MS,
  FEED_TOOL_PREVIEW_CHARS,
  activityActor,
  activityKindView,
  activityToolIo,
  advanceFeedConnection,
  capTimeline,
  createLiveFeedController,
  formatFeedTime,
  formatToolValue,
  mergeTimeline,
  prettyJson,
  previewFeedText,
  toolIoFullText,
  type FeedSignal,
  type LiveFeedSnapshot,
} from "./feed.ts";
import type { Activity } from "./types.ts";

const TASK_ID = "00000000-0000-4000-8000-0000000000aa";

const sleep = (ms: number) =>
  new Promise<void>((resolve) => setTimeout(resolve, ms));

function act(id: number, overrides: Partial<Activity> = {}): Activity {
  return {
    id,
    task_id: TASK_ID,
    kind: `kind_${id}`,
    message: `event ${id}`,
    payload: { n: id },
    created_at: "2026-10-30T00:00:00Z",
    ...overrides,
  };
}

const ids = (rows: readonly Activity[]): number[] => rows.map((r) => r.id);

// ---------------------------------------------------------------------------
// mergeTimeline — the pure sequencing primitive
// ---------------------------------------------------------------------------

test("mergeTimeline stitches history and live rows ascending with a rising cursor", () => {
  const merged = mergeTimeline([act(1), act(2)], [act(3), act(4)], 0);
  assert.deepEqual(ids(merged.rows), [1, 2, 3, 4]);
  assert.equal(merged.cursor, 4);

  // Starting mid-log keeps the same guarantee.
  const resumed = mergeTimeline([act(3)], [act(4)], 2);
  assert.deepEqual(ids(resumed.rows), [3, 4]);
  assert.equal(resumed.cursor, 4);
});

test("mergeTimeline dedupes overlaps: history wins and replays vanish", () => {
  // History 1..3 plus a stream replay of 2..4 (classic reconnect overlap).
  const merged = mergeTimeline(
    [act(1), act(2), act(3)],
    [act(2), act(3), act(4)],
    0,
  );
  assert.deepEqual(ids(merged.rows), [1, 2, 3, 4]);
  assert.equal(merged.cursor, 4);

  // Duplicate ids within one batch keep the FIRST occurrence.
  const duped = mergeTimeline(
    [],
    [act(7, { message: "first" }), act(7, { message: "second" })],
    0,
  );
  assert.deepEqual(ids(duped.rows), [7]);
  assert.equal(duped.rows[0]?.message, "first");
  // The cursor counts SEEN rows: a duplicate still advances it.
  assert.equal(duped.cursor, 7);
});

test("mergeTimeline accepts out-of-order and interleaved arrivals", () => {
  const merged = mergeTimeline([], [act(9), act(4), act(7), act(4), act(2)], 0);
  assert.deepEqual(ids(merged.rows), [2, 4, 7, 9]);
  assert.equal(merged.cursor, 9);

  // Arrival sequences in wire shape (batches ascend above their floor)
  // converge to the same timeline regardless of how they're chunked.
  const a = mergeTimeline([], [act(5)], 0).rows; // seed brings 5
  const b = mergeTimeline(a, [act(3)], 0).rows; // a straggler lands (above floor 0)
  const c = mergeTimeline(b, [act(8)], 5).rows; // resume at 5 delivers 8
  const d = mergeTimeline(c, [act(6)], 5).rows; // 6 arrives on the 5-resumed stream
  assert.deepEqual(ids(d), [3, 5, 6, 8]);
});

test("mergeTimeline handles cursor-only starts and empty inputs", () => {
  assert.deepEqual(mergeTimeline([], [], 12), { rows: [], cursor: 12 });
  // INCOMING rows obey the cursor floor (the dedupe guard)…
  assert.deepEqual(ids(mergeTimeline([], [act(1), act(2), act(3)], 2).rows), [
    3,
  ]);
  // …while the CARRIED TIMELINE passes through untouched (never evicted).
  assert.deepEqual(
    ids(mergeTimeline([act(1), act(2)], [act(3)], 2).rows),
    [1, 2, 3],
  );
  // The cursor still remembers floors even with no usable rows at all.
  assert.equal(mergeTimeline([], [], 5).cursor, 5);
});

test("mergeTimeline reproduces the timeline after a simulated refresh (no gaps, no dupes)", () => {
  // Before refresh: history 1..3 was loaded, stream brought 4, then 5.
  const live = mergeTimeline(
    mergeTimeline([act(1), act(2), act(3)], [act(4)], 0).rows,
    [act(5)],
    4,
  );
  // After refresh: a brand-new history fetch returns 1..5, stream replays 5.
  const refreshed = mergeTimeline(
    [act(1), act(2), act(3), act(4), act(5)],
    [act(5)],
    0,
  );
  assert.deepEqual(refreshed, live);
  assert.deepEqual(ids(refreshed.rows), [1, 2, 3, 4, 5]);
});

test("mergeTimeline filters hostile shapes without crashing", () => {
  const hostile = [
    null,
    undefined,
    42,
    "activity",
    true,
    [],
    { /* missing id */ kind: "x" },
    { id: -1 },
    { id: 1.5 },
    { id: "7" },
    { id: Number.NaN },
    { id: Number.POSITIVE_INFINITY },
    { id: 2 ** 53 }, // exceeds MAX_SAFE_INTEGER → unusable cursor
    { id: 0 }, // usable; as INCOMING it would hit the inclusive floor
    // Structurally-sane row with junk fields — merge keeps it, render defends.
    { id: 11, kind: 42, message: null, payload: "junk", created_at: 7 },
  ] as const;
  const incoming = [hostile[13], hostile[14], act(12)] as const;
  const merged = mergeTimeline(hostile, incoming, 0);
  // Row 0 survives via the HISTORY slot (carried timelines pass through at
  // face value) while its INCOMING duplicate is floor-dropped; the junk row
  // survives once (re-delivery absorbed by first-wins); every
  // primitive/array/garbage shape is discarded silently; 12 appends.
  assert.deepEqual(ids(merged.rows), [0, 11, 12]);
  assert.equal(merged.cursor, 12);
  // Structurally-sane-but-junk fields survive (the renderer defends).
  const junk = merged.rows[1];
  assert.ok(junk);
  assert.equal(junk.kind, 42);
  assert.equal(junk.message, null);
});

test("capTimeline keeps the newest window and counts hidden older rows", () => {
  const rows = Array.from({ length: 12 }, (_, i) => act(i + 1));
  const capped = capTimeline(rows, 5);
  assert.deepEqual(ids(capped.kept), [8, 9, 10, 11, 12]);
  assert.equal(capped.hidden, 7);

  const live = capTimeline(rows, FEED_LIVE_COUNT);
  assert.deepEqual(ids(live.kept), [3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
  assert.equal(live.hidden, 2);
  assert.deepEqual(capTimeline([], FEED_RECENT_COUNT), { kept: [], hidden: 0 });
  // Degenerate caps fall back to the live window instead of hiding everything.
  assert.deepEqual(
    ids(capTimeline(rows, 0).kept),
    ids(rows.slice(-FEED_LIVE_COUNT)),
  );
});

// ---------------------------------------------------------------------------
// Kind / actor / time presentation
// ---------------------------------------------------------------------------

test("activityKindView maps served kinds onto the badge vocabulary", () => {
  assert.deepEqual(activityKindView("task_created"), {
    label: "task_created",
    tone: "created",
  });
  assert.deepEqual(activityKindView("intent_claimed"), {
    label: "intent_claimed",
    tone: "running",
  });
  assert.deepEqual(activityKindView("tool_denied"), {
    label: "tool_denied",
    tone: "denied",
  });
  assert.deepEqual(activityKindView("engine_timeout"), {
    label: "engine_timeout",
    tone: "timeout",
  });
  assert.deepEqual(activityKindView("goal_proven"), {
    label: "goal_proven",
    tone: "met",
  });
});

test("activityKindView falls back to the raw kind string for unknown kinds, never crashing", () => {
  assert.deepEqual(activityKindView("totally_custom_kind"), {
    label: "totally_custom_kind",
    tone: null,
  });
  // Hostile non-string kinds degrade to printable placeholders.
  assert.deepEqual(activityKindView(42), { label: "42", tone: null });
  assert.deepEqual(activityKindView(["odd"]), { label: '["odd"]', tone: null });
  assert.deepEqual(activityKindView({ weird: true }), {
    label: '{"weird":true}',
    tone: null,
  });
  assert.deepEqual(activityKindView(null), { label: "event", tone: null });
  assert.deepEqual(activityKindView(undefined), { label: "event", tone: null });
  assert.deepEqual(activityKindView(""), { label: "event", tone: null });
});

test("activityActor classifies known kinds and returns null for others", () => {
  assert.equal(activityActor("intents_added"), "planner");
  assert.equal(activityActor("goal_proven"), "planner");
  assert.equal(activityActor("intent_claimed"), "worker");
  assert.equal(activityActor("intent_done"), "worker");
  assert.equal(activityActor("task_created"), "system");
  assert.equal(activityActor("approval_expired"), "system");
  assert.equal(activityActor("who_knows"), null);
  assert.equal(activityActor(42), null);
});

test("formatFeedTime renders short local clocks and passes junk through", () => {
  const pad = (n: number): string => String(n).padStart(2, "0");
  const localClock = (iso: string): string => {
    const date = new Date(iso);
    return `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(
      date.getSeconds(),
    )}`;
  };
  const stamps = [
    "2026-10-30T07:05:09Z",
    "2026-10-30T07:05:09.75+00:00",
    "2026-01-01T23:59:59+02:00",
  ];
  for (const iso of stamps) {
    assert.equal(formatFeedTime(iso), localClock(iso));
  }
  assert.equal(formatFeedTime("garbage"), "garbage");
});

test("prettyJson expands payloads readably and survives junk", () => {
  assert.equal(prettyJson({ a: 1 }), '{\n  "a": 1\n}');
  const cyclic: Record<string, unknown> = {};
  cyclic.self = cyclic;
  assert.match(prettyJson(cyclic), /\[object Object\]|circular/);
});

test("formatToolValue keeps its string, truncation, and JSON contract", () => {
  assert.equal(formatToolValue("ls -la"), "ls -la");
  assert.equal(formatToolValue({ command: "ls" }), '{\n  "command": "ls"\n}');
  assert.equal(
    formatToolValue({ truncated: true, preview: "first 500 chars" }),
    "first 500 chars\n… (truncated by the server)",
  );
  assert.equal(formatToolValue(null), null);
  assert.equal(formatToolValue(undefined), null);
});

test("activityToolIo extracts a shell command and its output", () => {
  const fresh = activityToolIo(
    act(1, {
      kind: "tool_allowed",
      payload: {
        tool: "shell",
        input: {
          command: "npm test",
          cwd: "/repo",
          __dhi_task_id: "task-1",
          __dhi_scope_mac: "mac",
        },
        output: "all tests passed",
      },
    }),
  );
  assert.deepEqual(fresh, {
    tool: "shell",
    command: "npm test",
    content: null,
    output: "all tests passed",
    details: null,
  });
  assert.equal(
    toolIoFullText(fresh!),
    "Tool:\nshell\n\nCommand:\nnpm test\n\nOutput:\nall tests passed",
  );

  // Output objects with a string `output` unwrap before the generic formatter.
  const wrapped = activityToolIo(
    act(2, {
      kind: "tool_allowed",
      payload: {
        tool: "shell",
        input: { command: "ls" },
        output: { output: "a.txt\nb.txt" },
      },
    }),
  );
  assert.equal(wrapped?.command, "ls");
  assert.equal(wrapped?.output, "a.txt\nb.txt");

  // Older rows stored the raw input string; it still reads as the command.
  const legacy = activityToolIo(
    act(3, {
      kind: "tool_allowed",
      payload: { tool: "shell", input: "ls -la", output: "total 0" },
    }),
  );
  assert.equal(legacy?.command, "ls -la");
  assert.equal(legacy?.output, "total 0");
});

test("activityToolIo separates write file path, content, and result", () => {
  const io = activityToolIo(
    act(4, {
      kind: "tool_allowed",
      payload: {
        tool: "write",
        input: { path: "src/app.ts", content: "export {};\n" },
        output: "Wrote src/app.ts",
      },
    }),
  );
  assert.deepEqual(io, {
    tool: "write",
    command: "src/app.ts",
    content: "export {};\n",
    output: "Wrote src/app.ts",
    details: null,
  });

  // Alternate field names and `{ message }` results are accepted too.
  const alternate = activityToolIo(
    act(5, {
      kind: "tool_allowed",
      payload: {
        tool: "write",
        input: { file_path: "notes.md", text: "hello" },
        output: { message: "saved" },
      },
    }),
  );
  assert.deepEqual(alternate, {
    tool: "write",
    command: "notes.md",
    content: "hello",
    output: "saved",
    details: null,
  });
});

test("activityToolIo treats read and list paths as the command", () => {
  const read = activityToolIo(
    act(6, {
      kind: "tool_allowed",
      payload: {
        tool: "read",
        input: { path: "README.md", offset: 2 },
        output: "# DHI",
      },
    }),
  );
  assert.deepEqual(read, {
    tool: "read",
    command: "README.md",
    content: null,
    output: "# DHI",
    details: null,
  });

  const list = activityToolIo(
    act(7, {
      kind: "tool_allowed",
      payload: { tool: "list", input: { path: "src" }, output: "a.ts\nb.ts" },
    }),
  );
  assert.equal(list?.command, "src");
  assert.equal(list?.output, "a.ts\nb.ts");
});

test("activityToolIo strips __dhi metadata and keeps compact details for other tools", () => {
  const io = activityToolIo(
    act(8, {
      kind: "tool_allowed",
      payload: {
        tool: "lookup",
        input: {
          query: "rust",
          __dhi_task_id: "task-1",
          __dhi_scope_mac: "mac",
          __dhi: true,
          options: { __dhi_role: "worker", limit: 3 },
        },
        output: "found",
      },
    }),
  );
  assert.deepEqual(io, {
    tool: "lookup",
    command: null,
    content: null,
    output: "found",
    details: 'query: rust\noptions: {"limit":3}',
  });
  assert.doesNotMatch(toolIoFullText(io!), /__dhi/);

  const other = activityToolIo(
    act(9, {
      kind: "tool_allowed",
      payload: {
        tool: "createGame",
        input: { color: "white", level: 3, timed: false },
        output: { result: { game_id: "g-1" } },
      },
    }),
  );
  assert.equal(other?.details, "color: white\nlevel: 3\ntimed: false");
  assert.match(other?.output ?? "", /g-1/);
});

test("activityToolIo keeps output-only rows and joins the full copy text", () => {
  // Older rows predate the contract's `input`; output alone still renders.
  const old = activityToolIo(
    act(10, {
      kind: "tool_allowed",
      payload: { tool: "shell", output: { truncated: true, preview: "ok" } },
    }),
  );
  assert.deepEqual(old, {
    tool: "shell",
    command: null,
    content: null,
    output: "ok\n… (truncated by the server)",
    details: null,
  });
  assert.equal(
    toolIoFullText(old!),
    "Tool:\nshell\n\nOutput:\nok\n… (truncated by the server)",
  );

  assert.equal(
    toolIoFullText({
      tool: "write",
      command: "a.txt",
      content: "hi",
      output: "ok",
      details: "mode: overwrite",
    }),
    "Tool:\nwrite\n\nCommand:\na.txt\n\nContent:\nhi\n\nOutput:\nok\n\nDetails:\nmode: overwrite",
  );
});

test("activityToolIo returns null when a row has no useful tool cell", () => {
  // Non-tool activity keeps the generic payload expansion.
  assert.equal(
    activityToolIo(act(11, { kind: "task_created", payload: { goal: "g" } })),
    null,
  );
  // A tool row carrying only reserved metadata has nothing to show.
  assert.equal(
    activityToolIo(
      act(12, {
        kind: "tool_allowed",
        payload: {
          input: { __dhi_task_id: "task-1" },
          __dhi_scope_mac: "mac",
        },
      }),
    ),
    null,
  );
});

test("previewFeedText truncates long input/output and flags expansion", () => {
  assert.deepEqual(previewFeedText("short"), {
    preview: "short",
    truncated: false,
  });
  const long = "x".repeat(FEED_TOOL_PREVIEW_CHARS + 20);
  const preview = previewFeedText(long);
  assert.equal(preview.truncated, true);
  assert.equal(preview.preview.length, FEED_TOOL_PREVIEW_CHARS);
  assert.match(preview.preview, /…$/);
  assert.deepEqual(previewFeedText("abcdef", 4), {
    preview: "abc…",
    truncated: true,
  });
  // Degenerate caps fall back to the documented preview length.
  assert.equal(
    previewFeedText(long, 0).preview.length,
    FEED_TOOL_PREVIEW_CHARS,
  );
});

// ---------------------------------------------------------------------------
// advanceFeedConnection — indicator state machine
// ---------------------------------------------------------------------------

test("advanceFeedConnection goes live on hello from every state", () => {
  for (const prev of [
    "connecting",
    "live",
    "reconnecting",
    "polling",
  ] as const) {
    assert.deepEqual(advanceFeedConnection(prev, "hello", 3, 5), {
      state: "live",
      fallbackTriggered: false,
    });
  }
});

test("advanceFeedConnection escalates to polling only at the streak limit", () => {
  assert.deepEqual(advanceFeedConnection("connecting", "error", 1, 5), {
    state: "reconnecting",
    fallbackTriggered: false,
  });
  assert.deepEqual(advanceFeedConnection("live", "error", 4, 5), {
    state: "reconnecting",
    fallbackTriggered: false,
  });
  assert.deepEqual(advanceFeedConnection("reconnecting", "error", 5, 5), {
    state: "polling",
    fallbackTriggered: true,
  });
  // Deep into an episode the streak keeps counting (no regression to amber).
  assert.deepEqual(advanceFeedConnection("reconnecting", "error", 9, 5), {
    state: "polling",
    fallbackTriggered: true,
  });
  // Errors while already polling never re-trigger the fallback…
  assert.deepEqual(advanceFeedConnection("polling", "error", 6, 5), {
    state: "polling",
    fallbackTriggered: false,
  });
  // …and a degenerate limit disables the fallback entirely.
  assert.deepEqual(advanceFeedConnection("live", "error", 50, 0), {
    state: "reconnecting",
    fallbackTriggered: false,
  });
  assert.equal(FEED_FALLBACK_ERROR_STREAK, 5);
});

// ---------------------------------------------------------------------------
// Live-feed controller — history seed → after-cursor stream → fallback,
// driven through the REAL sse.ts wrapper against a scripted EventSource fake
// (the api.test.ts fake pattern, plus URL recording and connecting-state
// errors).
// ---------------------------------------------------------------------------

type Listener = (ev: { data?: string; lastEventId?: string }) => void;

class FakeEventSource {
  static instances: FakeEventSource[] = [];
  static reset(): void {
    FakeEventSource.instances = [];
  }
  static get last(): FakeEventSource | undefined {
    return FakeEventSource.instances[FakeEventSource.instances.length - 1];
  }

  static CLOSED = 2;
  static OPEN = 1;
  static CONNECTING = 0;

  url: string;
  readyState = FakeEventSource.CONNECTING;
  closed = false;
  onerror: (() => void) | null = null;
  private listeners = new Map<string, Listener[]>();

  constructor(url: string) {
    this.url = url;
    FakeEventSource.instances.push(this);
  }

  addEventListener(type: string, listener: Listener): void {
    const list = this.listeners.get(type) ?? [];
    list.push(listener);
    this.listeners.set(type, list);
  }

  close(): void {
    this.closed = true;
    this.readyState = FakeEventSource.CLOSED;
  }

  emit(type: string, data: string, lastEventId = ""): void {
    for (const listener of this.listeners.get(type) ?? []) {
      listener({ data, lastEventId });
    }
  }

  /** Transient platform failure (readyState stays CONNECTING): no rebuild. */
  errorWhileConnecting(): void {
    this.onerror?.();
  }

  /** Hard failure: the platform gave up, prompting an explicit resume. */
  failAsClosed(): void {
    this.readyState = FakeEventSource.CLOSED;
    this.onerror?.();
  }
}

const originalEventSource = globalThis.EventSource;

afterEach(() => {
  globalThis.EventSource = originalEventSource;
});

function useFakeEventSource(): void {
  globalThis.EventSource = FakeEventSource as unknown as typeof EventSource;
}

/** Scripted history loader: each call consumes the next entry (tail repeats). */
function scriptedLoader(script: Array<Activity[] | Error>): {
  load: (taskId: string) => Promise<Activity[]>;
  callCount: () => number;
} {
  let index = 0;
  let calls = 0;
  return {
    callCount: () => calls,
    load: async (_taskId: string) => {
      calls += 1;
      const step = script[Math.min(index, script.length - 1)];
      index += 1;
      if (step instanceof Error) {
        throw step;
      }
      return step;
    },
  };
}

function makeHarness(
  script: Array<Activity[] | Error>,
  extra: Parameters<typeof createLiveFeedController>[1] = {},
) {
  useFakeEventSource();
  FakeEventSource.reset();
  const loader = scriptedLoader(script);
  const signals: FeedSignal[] = [];
  const patches: LiveFeedSnapshot[] = [];
  const controller = createLiveFeedController(TASK_ID, {
    loadActivity: loader.load,
    pollIntervalMs: 5,
    streamRetryMs: 10,
    streamResumeDelayMs: 5,
    onSignal: (signal) => signals.push(signal),
    ...extra,
  });
  patches.push(structuredClone(controller.snapshot()));
  const unsubscribe = controller.subscribe((snapshot) =>
    patches.push(structuredClone({ ...snapshot, rows: [...snapshot.rows] })),
  );
  controller.start();
  return { controller, loader, signals, patches, unsubscribe };
}

const lastSnapshot = (harness: ReturnType<typeof makeHarness>) =>
  harness.controller.snapshot();

test("controller seeds from history, opens the stream with after=<max>, and appends deduped rows", async () => {
  const harness = makeHarness([[act(1), act(2), act(3)]]);
  await sleep(20);

  // History seeded first; the stream URL already resumes from the cursor.
  let snapshot = lastSnapshot(harness);
  assert.deepEqual(ids(snapshot.rows), [1, 2, 3]);
  assert.equal(snapshot.cursor, 3);
  assert.equal(snapshot.connection, "connecting");
  const stream = FakeEventSource.last;
  assert.ok(stream);
  assert.equal(
    stream.url,
    `/api/stream?task_id=${TASK_ID}&after=3`,
    "the wrapper resume trick: first open already skips the seeded rows",
  );

  // hello flips the indicator; activity rows append; a replay of id 4
  // vanishes; the late/backwards id 3 is dropped by the cursor guard.
  stream.emit("hello", JSON.stringify({ task_id: TASK_ID }));
  stream.emit("activity", JSON.stringify(act(4)), "4");
  stream.emit("activity", JSON.stringify(act(5)), "5");
  stream.emit("activity", JSON.stringify(act(4)), "4");
  stream.emit("activity", JSON.stringify(act(3)), "3");
  await sleep(10);

  snapshot = lastSnapshot(harness);
  assert.deepEqual(ids(snapshot.rows), [1, 2, 3, 4, 5]);
  assert.equal(snapshot.cursor, 5);
  assert.equal(snapshot.connection, "live");

  // Keep-alives are COMMENT frames: the EventSource never fires events for
  // them, so the code under test sees literally nothing — assert zero state
  // churn over an idle window (the parse-level counterpart lives in
  // api.test.ts: parseFrame(": keep-alive") → ignored).
  const patchesBefore = harness.patches.length;
  const rowsBefore = ids(snapshot.rows);
  await sleep(15);
  assert.equal(harness.patches.length, patchesBefore);
  assert.deepEqual(ids(lastSnapshot(harness).rows), rowsBefore);

  // Unknown future events are likewise inert.
  stream.emit("something_new", '{"x":1}');
  await sleep(5);
  assert.equal(harness.patches.length, patchesBefore);
  assert.deepEqual(ids(lastSnapshot(harness).rows), rowsBefore);

  harness.controller.dispose();
});

test("controller resumes with after=<cursor> after a hard disconnect and restores the live state", async () => {
  const harness = makeHarness([[act(1), act(2), act(3)]]);
  await sleep(20);

  const first = FakeEventSource.last;
  assert.ok(first);
  first.emit("hello", JSON.stringify({ task_id: TASK_ID }));
  first.emit("activity", JSON.stringify(act(4)), "4");
  await sleep(5);

  first.failAsClosed(); // platform gave up → wrapper rebuilds from its cursor
  await sleep(5);
  assert.equal(lastSnapshot(harness).connection, "reconnecting");

  await sleep(25); // resume delay (5 ms) + margin
  const second = FakeEventSource.last;
  assert.notEqual(second, first);
  assert.equal(second?.url, `/api/stream?task_id=${TASK_ID}&after=4`);

  second?.emit("hello", JSON.stringify({ task_id: TASK_ID }));
  assert.equal(lastSnapshot(harness).connection, "live");
  // Replay of 4 is dropped by the wrapper's cursor; 6 appends; backwards 2
  // never passes the wrapper either.
  second?.emit("activity", JSON.stringify(act(4)), "4");
  second?.emit("activity", JSON.stringify(act(6)), "6");
  second?.emit("activity", JSON.stringify(act(2)), "2");
  await sleep(10);
  assert.deepEqual(ids(lastSnapshot(harness).rows), [1, 2, 3, 4, 6]);
  assert.equal(lastSnapshot(harness).cursor, 6);

  harness.controller.dispose();
});

test("controller falls back to polling after persistent stream errors and wins the stream back", async () => {
  const harness = makeHarness([
    [act(1), act(2)],
    [act(1), act(2), act(3)],
    [act(1), act(2), act(3), act(4)],
  ]);
  await sleep(20);

  const stream = FakeEventSource.last;
  assert.ok(stream);
  // Five consecutive error events (transparent CONNECTING retries) exhaust
  // the streak and flip the indicator to the polling fallback.
  for (let i = 0; i < FEED_FALLBACK_ERROR_STREAK; i += 1) {
    stream.errorWhileConnecting();
  }
  assert.equal(lastSnapshot(harness).connection, "polling");
  assert.ok(
    harness.signals.some(
      (signal) =>
        signal.kind === "connection" && signal.connection === "polling",
    ),
    "the owning page hears about the fallback (poll suspension decision)",
  );

  // The parked stream's connection is closed; the loader now drives rows.
  assert.equal(stream.closed, true);
  const polledCalls = harness.loader.callCount();
  assert.ok(
    polledCalls >= 2,
    `immediate + interval polls ran (got ${polledCalls})`,
  );
  await sleep(15);
  assert.deepEqual(ids(lastSnapshot(harness).rows), [1, 2, 3, 4]);

  // The retry timer reopens the stream from the CURRENT cursor…
  await sleep(20);
  const retry = FakeEventSource.last;
  assert.notEqual(retry, stream);
  assert.equal(retry?.url, `/api/stream?task_id=${TASK_ID}&after=4`);
  // …while the indicator stays grey-"polling" (quiet retry, no flashing).
  assert.equal(lastSnapshot(harness).connection, "polling");

  // hello wins the stream back: polling stops, live returns.
  retry?.emit("hello", JSON.stringify({ task_id: TASK_ID }));
  assert.equal(lastSnapshot(harness).connection, "live");
  assert.ok(
    harness.signals.some(
      (signal) => signal.kind === "connection" && signal.connection === "live",
    ),
  );
  const callsAtLive = harness.loader.callCount();
  await sleep(25);
  assert.equal(
    harness.loader.callCount(),
    callsAtLive,
    "the polling interval is torn down once the stream is live",
  );

  // A row landing during the polled window is not duplicated by the stream.
  retry?.emit("activity", JSON.stringify(act(4)), "4");
  retry?.emit("activity", JSON.stringify(act(7)), "7");
  await sleep(10);
  assert.deepEqual(ids(lastSnapshot(harness).rows), [1, 2, 3, 4, 7]);

  harness.controller.dispose();
});

test("controller polls immediately on visibility while polling and skips ticks while hidden", async () => {
  let visible = true;
  const harness = makeHarness([[act(1)], [act(1), act(2)]], {
    isVisible: () => visible,
  });
  await sleep(20);

  const stream = FakeEventSource.last;
  assert.ok(stream);
  for (let i = 0; i < FEED_FALLBACK_ERROR_STREAK; i += 1) {
    stream.errorWhileConnecting();
  }
  await sleep(15); // immediate tick + a couple of interval ticks (visible)
  const callsVisible = harness.loader.callCount();

  visible = false;
  await sleep(20); // interval ticks all bounce off isVisible
  const callsHidden = harness.loader.callCount();
  assert.equal(callsHidden, callsVisible);

  controller_visibility: {
    harness.controller.visibility(true); // regaining focus ticks at once
    await sleep(10);
  }
  assert.ok(harness.loader.callCount() > callsHidden);

  harness.controller.dispose();
});

test("controller heals the timeline from history when the tab becomes visible while live", async () => {
  const harness = makeHarness([
    [act(1), act(2), act(3)],
    [act(1), act(2), act(3)],
  ]);
  await sleep(20);

  const stream = FakeEventSource.last;
  assert.ok(stream);
  stream.emit("hello", JSON.stringify({ task_id: TASK_ID }));
  stream.emit("activity", JSON.stringify(act(3)), "3"); // wrapper drops (<=3)
  await sleep(5);
  assert.deepEqual(ids(lastSnapshot(harness).rows), [1, 2, 3]);

  // Visible again: the healing refetch returns the SAME rows (whatever the
  // hidden period produced server-side) and merges idempotently — no gap,
  // no duplicate, cursor unmoved.
  const callsBeforeHeal = harness.loader.callCount();
  harness.controller.visibility(true);
  await sleep(10);
  assert.equal(harness.loader.callCount(), callsBeforeHeal + 1);
  const snapshot = lastSnapshot(harness);
  assert.deepEqual(ids(snapshot.rows), [1, 2, 3]);
  assert.equal(snapshot.cursor, 3);
  assert.equal(snapshot.connection, "live");

  harness.controller.dispose();
});

test("controller surfaces a history-seed failure but keeps streaming from scratch", async () => {
  const harness = makeHarness([new Error("db unavailable"), [act(1), act(2)]]);
  await sleep(20);

  let snapshot = lastSnapshot(harness);
  assert.match(snapshot.historyError ?? "", /db unavailable/);
  assert.deepEqual(snapshot.rows, []);
  // The stream still opened, with after=0 (no after param at all).
  const stream = FakeEventSource.last;
  assert.ok(stream);
  assert.equal(stream.url, `/api/stream?task_id=${TASK_ID}`);

  // The server serves every row above after=0 in ascending order, so the
  // stream itself reconstructs what the failed seed could not deliver.
  stream.emit("hello", JSON.stringify({ task_id: TASK_ID }));
  stream.emit("activity", JSON.stringify(act(1)), "1");
  stream.emit("activity", JSON.stringify(act(2)), "2");
  await sleep(5);
  assert.deepEqual(ids(lastSnapshot(harness).rows), [1, 2]);

  // A later healing load overlaps the cursor and merges idempotently.
  harness.controller.visibility(true);
  await sleep(10);
  snapshot = lastSnapshot(harness);
  assert.deepEqual(ids(snapshot.rows), [1, 2]);
  assert.equal(snapshot.historyError, null);
  assert.equal(snapshot.cursor, 2);

  harness.controller.dispose();
});

test("controller stops cleanly on dispose", async () => {
  const harness = makeHarness([[act(1), act(2)]]);
  await sleep(20);
  const stream = FakeEventSource.last;
  assert.ok(stream);
  stream.emit("hello", JSON.stringify({ task_id: TASK_ID }));

  harness.controller.dispose();
  const snapshotAtDispose = structuredClone(lastSnapshot(harness));
  const instancesAtDispose = FakeEventSource.instances.length;
  const patchesAtDispose = harness.patches.length;

  // Late stream events, hard failures, signals and visibility are all inert.
  stream.emit("activity", JSON.stringify(act(5)), "5");
  stream.failAsClosed();
  harness.controller.visibility(true);
  await sleep(25);
  assert.equal(FakeEventSource.instances.length, instancesAtDispose);
  assert.equal(harness.patches.length, patchesAtDispose);
  const settled = lastSnapshot(harness);
  assert.deepEqual(settled.rows, snapshotAtDispose.rows);
  assert.equal(settled.cursor, snapshotAtDispose.cursor);
  assert.equal(settled.connection, snapshotAtDispose.connection);
  assert.equal(stream.closed, true);
});

test("feed tunables keep the documented cadences", () => {
  assert.equal(FEED_LIVE_COUNT, 10);
  assert.equal(FEED_RECENT_COUNT, 10);
  assert.equal(FEED_TOOL_PREVIEW_CHARS, 280);
  assert.equal(FEED_STREAM_RETRY_MS, 30_000);
  assert.equal(FEED_GRAPH_REFRESH_DEBOUNCE_MS, 2500);
});
