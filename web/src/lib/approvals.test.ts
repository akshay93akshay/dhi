import assert from "node:assert/strict";
import { afterEach, test } from "node:test";
import {
  APPROVAL_REFRESH_DEBOUNCE_MS,
  MISSING_TOOL_INPUT_NOTE,
  TOOL_INPUT_MAX_LINES,
  buildDecisionRequest,
  formatApprovalAge,
  renderToolInput,
} from "./approvals.ts";
import { ApiError, decideApproval, listApprovals } from "./api.ts";
import type { ApprovalItem } from "./types.ts";

const originalFetch = globalThis.fetch;

afterEach(() => {
  globalThis.fetch = originalFetch;
});

/** Stubs `globalThis.fetch`, recording every call for assertions. */
function stubFetch(
  respond: (url: string, init?: RequestInit) => Response | Promise<Response>,
): { urls: string[]; calls: Array<{ url: string; init?: RequestInit }> } {
  const recorder = {
    urls: [] as string[],
    calls: [] as Array<{ url: string; init?: RequestInit }>,
  };
  globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    recorder.urls.push(url);
    recorder.calls.push({ url, init });
    return respond(url, init);
  }) as typeof fetch;
  return recorder;
}

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

/** A wire-shaped pending approval, mirroring the api.md example row. */
function makeApproval(overrides: Partial<ApprovalItem> = {}): ApprovalItem {
  return {
    id: "00000000-0000-4000-8000-000000000007",
    task_id: "00000000-0000-4000-8000-000000000001",
    tool_call_id: "00000000-0000-4000-8000-000000000008",
    guard_decision_id: "00000000-0000-4000-8000-000000000009",
    status: "pending",
    reason: "needs a person",
    expires_at: "2026-10-30T00:15:00Z",
    decided_at: null,
    created_at: "2026-10-30T00:00:00Z",
    tool_name: "finishTurn",
    input: { move: "Qh5" },
    ...overrides,
  };
}

// ---------------------------------------------------------------------------
// renderToolInput — the input renderer
// ---------------------------------------------------------------------------

test("renderToolInput renders objects as key/value lines", () => {
  const view = renderToolInput({ move: "Qh5", depth: 3, prompt: true });
  assert.equal(view.kind, "object");
  assert.deepEqual(view.lines, ["move: Qh5", "depth: 3", "prompt: true"]);
});

test("renderToolInput renders nested values compactly per line", () => {
  const view = renderToolInput({
    board: { fen: "k7/8/8 w" },
    history: ["e2e4", "e7e5"],
    note: null,
  });
  assert.deepEqual(view.lines, [
    "board: {\"fen\":\"k7/8/8 w\"}",
    "history: [\"e2e4\",\"e7e5\"]",
    "note: null",
  ]);
});

test("renderToolInput renders arrays as indexed lines", () => {
  const view = renderToolInput(["a", 2, { three: 3 }]);
  assert.equal(view.kind, "array");
  assert.deepEqual(view.lines, [
    "[0]: a",
    "[1]: 2",
    "[2]: {\"three\":3}",
  ]);
});

test("renderToolInput shows strings verbatim and scalars as one line", () => {
  assert.deepEqual(renderToolInput("make mating move"), {
    kind: "text",
    lines: ["make mating move"],
  });
  assert.deepEqual(renderToolInput(""), { kind: "text", lines: ["(empty)"] });
  assert.deepEqual(renderToolInput(42), { kind: "scalar", lines: ["42"] });
  assert.deepEqual(renderToolInput(false), { kind: "scalar", lines: ["false"] });
});

test("renderToolInput maps null/undefined to the explicit missing note", () => {
  for (const empty of [null, undefined]) {
    const view = renderToolInput(empty);
    assert.equal(view.kind, "missing");
    assert.equal(view.note, MISSING_TOOL_INPUT_NOTE);
    assert.deepEqual(view.lines, []);
  }
  assert.equal(MISSING_TOOL_INPUT_NOTE.includes("tool-call"), true);
});

test("renderToolInput collapses keyless exotic objects to one JSON line", () => {
  const date = new Date("2026-10-30T00:00:00Z");
  const view = renderToolInput(date);
  assert.equal(view.kind, "object");
  assert.deepEqual(view.lines, [JSON.stringify(date)]);
});

test("renderToolInput survives hostile shapes and never throws", () => {
  // Circular structure: JSON.stringify throws, String fallback applies.
  const circular: { [key: string]: unknown } = { name: "loop" };
  circular.self = circular;
  const loopView = renderToolInput(circular);
  assert.equal(loopView.kind, "object");
  assert.equal(loopView.lines[0], "name: loop");
  assert.equal(typeof loopView.lines[1], "string");

  // Throwing getters degrade to the unprintable marker.
  const trap = {
    get boom(): string {
      throw new Error("getter exploded");
    },
  };
  assert.deepEqual(renderToolInput(trap).lines, ["boom: [unprintable]"]);

  // Deeply weird nested junk stays one defensible line per key.
  const weird = renderToolInput({
    a: [[[[{ deep: true }]]]],
    b: undefined,
    c: Number.NaN,
    d: BigInt("9007199254740993"),
  });
  assert.equal(weird.kind, "object");
  assert.equal(weird.lines.length, 4);

  // Exotic top-level hostiles do not throw either.
  assert.equal(typeof renderToolInput(Symbol("x")).lines[0], "string");
  const fnHostile: unknown = () => 1;
  assert.equal(typeof renderToolInput(fnHostile).lines[0], "string");

  // Giant arrays/object get capped for the DOM.
  const big: unknown[] = Array.from({ length: TOOL_INPUT_MAX_LINES + 10 }, (_, i) => i);
  const capped = renderToolInput(big);
  assert.equal(capped.kind, "array");
  assert.equal(capped.lines.length, TOOL_INPUT_MAX_LINES + 1);
  assert.match(capped.lines[TOOL_INPUT_MAX_LINES], /^\+\d+ more items? hidden$/);

  const bigObject = Object.fromEntries(
    Array.from({ length: TOOL_INPUT_MAX_LINES + 2 }, (_, i) => [`k${i}`, i]),
  );
  const cappedObject = renderToolInput(bigObject);
  assert.equal(cappedObject.lines.length, TOOL_INPUT_MAX_LINES + 1);
});

// ---------------------------------------------------------------------------
// formatApprovalAge — "waiting 2m"
// ---------------------------------------------------------------------------

test("formatApprovalAge formats seconds/minutes/hours/days boundaries", () => {
  const now = new Date("2026-10-30T12:00:00Z");
  const ago = (ms: number): string =>
    formatApprovalAge(new Date(now.getTime() - ms), now);

  assert.equal(ago(0), "0s");
  assert.equal(ago(999), "0s");
  assert.equal(ago(59_000), "59s");
  assert.equal(ago(60_000), "1m"); // minute boundary
  assert.equal(ago(119_000), "1m");
  assert.equal(ago(3_599_000), "59m");
  assert.equal(ago(3_600_000), "1h"); // hour boundary
  assert.equal(ago(7_199_000), "1h");
  assert.equal(ago(86_399_000), "23h");
  assert.equal(ago(86_400_000), "1d"); // day boundary
  assert.equal(ago(86_400_000 * 2.5), "2d");

  // RFC 3339 strings work as well as Dates.
  assert.equal(formatApprovalAge("2026-10-30T11:00:00Z", now), "1h");
});

test("formatApprovalAge tolerates future dates and junk input", () => {
  const now = new Date("2026-10-30T12:00:00Z");
  // Clock skew: a five-minute-future created_at clamps to 0s, never "-5m".
  assert.equal(formatApprovalAge(new Date(now.getTime() + 300_000), now), "0s");
  // Unparseable stamps get "?" rather than NaN.
  assert.equal(formatApprovalAge("not-a-date", now), "?");
  assert.equal(formatApprovalAge(now, "also-not-a-date"), "?");
});

// ---------------------------------------------------------------------------
// buildDecisionRequest — the decision request builder
// ---------------------------------------------------------------------------

test("buildDecisionRequest builds the POST body and endpoint path", () => {
  const allow = buildDecisionRequest(
    "00000000-0000-4000-8000-000000000007",
    "allow",
  );
  assert.deepEqual(allow, {
    ok: true,
    path: "/approvals/00000000-0000-4000-8000-000000000007/decision",
    body: { decision: "allow" },
  });

  const deny = buildDecisionRequest("a1", "deny");
  assert.deepEqual(deny, {
    ok: true,
    path: "/approvals/a1/decision",
    body: { decision: "deny" },
  });
});

test("buildDecisionRequest rejects invalid decision values and blank ids", () => {
  // The status spellings are NOT valid decisions (common mix-up).
  for (const bad of ["granted", "denied", "ALLOW", "", "yes"]) {
    const result = buildDecisionRequest("a1", bad);
    assert.equal(result.ok, false, `expected rejection for ${bad}`);
    if (!result.ok) {
      assert.match(result.error, /"allow" or "deny"/);
    }
  }
  const nonString = buildDecisionRequest("a1", 42);
  assert.equal(nonString.ok, false);
  // Junk ids refuse to build a path, too.
  assert.equal(buildDecisionRequest(7, "allow").ok, false);
  // Blank/whitespace ids refuse to build a path.
  for (const blank of ["", "   "]) {
    assert.equal(buildDecisionRequest(blank, "allow").ok, false);
  }
});

// ---------------------------------------------------------------------------
// API level — decideApproval statuses, listApprovals null-detail roundtrip
// ---------------------------------------------------------------------------

test("decideApproval maps 409 to ApiError with isConflict()", async () => {
  stubFetch(() =>
    jsonResponse(
      { error: "approval a1 is not pending: granted" },
      409,
    ),
  );

  const err = await decideApproval("a1", "allow").then(
    () => null,
    (caught: unknown) => caught,
  );
  assert.ok(err instanceof ApiError);
  assert.equal(err.status, 409);
  assert.equal(err.isConflict(), true);
  assert.equal(err.message, "approval a1 is not pending: granted");
  // The denial path races identically; ensure the helper alone suffices.
  assert.equal(new ApiError(409, "x").isConflict(), true);
});

test("decideApproval maps 404 to ApiError with isNotFound()", async () => {
  stubFetch(() =>
    jsonResponse({ error: "approval not found: a1" }, 404),
  );

  const err = await decideApproval("a1", "deny").then(
    () => null,
    (caught: unknown) => caught,
  );
  assert.ok(err instanceof ApiError);
  assert.equal(err.status, 404);
  assert.equal(err.isNotFound(), true);
  assert.equal(err.isConflict(), false);
});

test("listApprovals round-trips null tool_name/input rows untouched", async () => {
  const missing = makeApproval({ tool_name: null, input: null });
  const recorder = stubFetch(() => jsonResponse({ approvals: [missing] }));

  const approvals = await listApprovals();
  assert.equal(recorder.urls[0], "/api/approvals");
  assert.equal(approvals.length, 1);
  assert.equal(approvals[0].tool_name, null);
  assert.equal(approvals[0].input, null);
  // The null detail flows straight into the renderer's missing branch.
  assert.equal(renderToolInput(approvals[0].input).kind, "missing");
});

test("listApprovals tolerates an empty queue envelope", async () => {
  stubFetch(() => jsonResponse({ approvals: [] }));
  const approvals = await listApprovals();
  assert.deepEqual(approvals, []);
});

// ---------------------------------------------------------------------------
// Tunables sanity (guards against accidental drift in page wiring)
// ---------------------------------------------------------------------------

test("approval tunables keep sane floors", () => {
  assert.ok(APPROVAL_REFRESH_DEBOUNCE_MS >= 500);
  assert.ok(TOOL_INPUT_MAX_LINES >= 10);
});
