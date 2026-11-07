import assert from "node:assert/strict";
import { test } from "node:test";
import { ApiError } from "./api.ts";
import {
  PIN_UNSET,
  buildCreateTaskInput,
  describeApiError,
  parsePin,
  renderProvenance,
  serviceState,
  serviceStatusText,
  shortenId,
  taskIdFromPathname,
  validateGoal,
} from "./ui.ts";

// ---------------------------------------------------------------------------
// shortenId
// ---------------------------------------------------------------------------

test("shortenId keeps the first uuid hex group", () => {
  assert.equal(shortenId("00000000-0000-4000-8000-000000000001"), "00000000");
  assert.equal(shortenId("abcd"), "abcd");
  assert.equal(shortenId("12345678"), "12345678");
  assert.equal(shortenId("123456789"), "12345678");
  assert.equal(shortenId(""), "");
});

// ---------------------------------------------------------------------------
// taskIdFromPathname
// ---------------------------------------------------------------------------

const TASK_ID = "00000000-0000-4000-8000-000000000001";

test("taskIdFromPathname reads detail, work, and graph routes", () => {
  assert.equal(taskIdFromPathname(`/tasks/${TASK_ID}`), TASK_ID);
  assert.equal(taskIdFromPathname(`/tasks/${TASK_ID}/work`), TASK_ID);
  assert.equal(taskIdFromPathname(`/tasks/${TASK_ID}/graph`), TASK_ID);
});

test("taskIdFromPathname ignores trailing slashes", () => {
  assert.equal(taskIdFromPathname(`/tasks/${TASK_ID}/`), TASK_ID);
  assert.equal(taskIdFromPathname(`/tasks/${TASK_ID}/work/`), TASK_ID);
  assert.equal(taskIdFromPathname(`/tasks/${TASK_ID}/graph/`), TASK_ID);
});

test("taskIdFromPathname ignores the preview placeholder", () => {
  assert.equal(taskIdFromPathname("/tasks/preview"), null);
  assert.equal(taskIdFromPathname("/tasks/preview/"), null);
  assert.equal(taskIdFromPathname("/tasks/preview/work/"), null);
  assert.equal(taskIdFromPathname("/tasks/preview/graph"), null);
});

test("taskIdFromPathname returns null outside task routes", () => {
  assert.equal(taskIdFromPathname("/"), null);
  assert.equal(taskIdFromPathname("/tasks"), null);
  assert.equal(taskIdFromPathname(`/tasks/${TASK_ID}/other`), null);
  assert.equal(taskIdFromPathname("/api/health"), null);
  assert.equal(taskIdFromPathname(""), null);
});

test("taskIdFromPathname decodes the id segment", () => {
  assert.equal(taskIdFromPathname("/tasks/task%20one"), "task one");
  assert.equal(taskIdFromPathname("/tasks/%70review"), null);
});

// ---------------------------------------------------------------------------
// validateGoal / parsePin / buildCreateTaskInput
// ---------------------------------------------------------------------------

test("validateGoal rejects empty and whitespace-only goals", () => {
  assert.equal(validateGoal(""), "Goal is required.");
  assert.equal(validateGoal("   \n\t "), "Goal is required.");
  assert.equal(validateGoal("mate in two"), null);
  assert.equal(validateGoal(" padded "), null);
});

test("parsePin maps select values onto ids or null", () => {
  assert.equal(parsePin(PIN_UNSET), null);
  assert.equal(parsePin(""), null);
  assert.equal(parsePin("7"), 7);
  assert.equal(parsePin(" 3 "), 3);
  assert.equal(parsePin("0"), 0);
  assert.equal(parsePin("abc"), null);
  assert.equal(parsePin("-2"), null);
  assert.equal(parsePin("2.5"), null);
});

test("buildCreateTaskInput omits unset pins and trims the goal", () => {
  const result = buildCreateTaskInput({
    goal: "  mate in two  ",
    plannerPin: "",
    workerPin: "",
  });
  assert.ok(result.ok);
  // Exactly `goal`: absent keys, matching the server defaults path.
  assert.deepEqual(Object.keys(result.input).sort(), ["goal"]);
  assert.deepEqual(result.input, { goal: "mate in two" });
});

test("buildCreateTaskInput carries numeric pins into the canonical fields", () => {
  const result = buildCreateTaskInput({
    goal: "g",
    plannerPin: "7",
    workerPin: "9",
  });
  assert.ok(result.ok);
  assert.deepEqual(result.input, {
    goal: "g",
    planner_profile_id: 7,
    worker_profile_id: 9,
  });
});

test("buildCreateTaskInput refuses an empty goal without submitting", () => {
  for (const goal of ["", "   "]) {
    const result = buildCreateTaskInput({
      goal,
      plannerPin: "1",
      workerPin: "",
    });
    assert.deepEqual(result, { ok: false, error: "Goal is required." });
  }
});

// ---------------------------------------------------------------------------
// renderProvenance
// ---------------------------------------------------------------------------

test("renderProvenance recognizes the origin convention { goal }", () => {
  const view = renderProvenance({ goal: "mate in two" });
  assert.deepEqual(view, {
    kind: "origin",
    lines: ["Goal: mate in two"],
  });
});

test("renderProvenance recognizes the worker convention with all four keys", () => {
  const view = renderProvenance({
    intent_id: "i1",
    detail: "checked Qh5 line",
    evidence: "stockfish eval +1.2",
    confidence: "observed",
  });
  assert.deepEqual(view, {
    kind: "worker",
    lines: [
      "Intent: i1",
      "Detail: checked Qh5 line",
      "Evidence: stockfish eval +1.2",
      "Confidence: observed",
    ],
  });
});

test("renderProvenance tolerates worker rows with an empty detail", () => {
  const view = renderProvenance({
    intent_id: "i2",
    detail: "",
    evidence: "board position after 1.Qh5",
    confidence: "inferred",
  });
  assert.ok(view.kind === "worker");
  assert.deepEqual(view.lines, [
    "Intent: i2",
    "Detail: (empty)",
    "Evidence: board position after 1.Qh5",
    "Confidence: inferred",
  ]);
});

test("renderProvenance treats partial worker shapes as worker, skipping gaps", () => {
  assert.deepEqual(renderProvenance({ intent_id: "i3" }), {
    kind: "worker",
    lines: ["Intent: i3"],
  });
  assert.deepEqual(
    renderProvenance({ evidence: "eval +0.4", confidence: "observed" }),
    {
      kind: "worker",
      lines: ["Evidence: eval +0.4", "Confidence: observed"],
    },
  );
});

test("renderProvenance prefers the origin convention when both shapes appear", () => {
  const view = renderProvenance({
    goal: "g",
    intent_id: "i4",
    evidence: "e",
  });
  assert.equal(view.kind, "origin");
  assert.deepEqual(view.lines, ["Goal: g"]);
});

test("renderProvenance falls back to raw JSON for unknown shapes", () => {
  const empty = renderProvenance({});
  assert.deepEqual(empty, { kind: "raw", lines: ["{}"] });

  const exotic = renderProvenance({
    trace: { nested: [1, 2] },
    notes: "kept verbatim",
    count: 2,
    flag: null,
  });
  assert.equal(exotic.kind, "raw");
  assert.equal(exotic.lines.length, 1);
  assert.equal(
    exotic.lines[0],
    JSON.stringify(
      { trace: { nested: [1, 2] }, notes: "kept verbatim", count: 2, flag: null },
      null,
      2,
    ),
  );
});

test("renderProvenance never crashes on hostile value types", () => {
  // Goal arriving as the wrong type, arrays as evidence.
  const oddTypes = renderProvenance({
    goal: 42,
  } as never);
  assert.deepEqual(oddTypes, { kind: "origin", lines: ["Goal: 42"] });

  const nested = renderProvenance({
    intent_id: "i5",
    evidence: { moves: ["e2e4", "e7e5"], depth: 3 },
  });
  assert.ok(nested.kind === "worker");
  assert.equal(
    nested.lines[1],
    `Evidence: ${JSON.stringify({ moves: ["e2e4", "e7e5"], depth: 3 })}`,
  );

  // A null confidence stringifies as JSON null, not the word undefined.
  const nulls = renderProvenance({ intent_id: "i6", confidence: null });
  assert.ok(nulls.kind === "worker");
  assert.equal(nulls.lines[1], "Confidence: null");
});

// ---------------------------------------------------------------------------
// describeApiError
// ---------------------------------------------------------------------------

test("describeApiError words each failure class distinctly", () => {
  assert.match(describeApiError(new ApiError(0, "socket hung up")), /^Network error:/);
  assert.match(
    describeApiError(new ApiError(400, "goal must not be empty")),
    /^Invalid request:/,
  );
  assert.match(describeApiError(new ApiError(404, "task not found: x")), /^Not found:/);
  assert.equal(
    describeApiError(new ApiError(500, "boom")),
    "boom (HTTP 500)",
  );
  assert.equal(describeApiError(new Error("plain")), "plain");
  assert.equal(describeApiError("just a string"), "just a string");
});

// ---------------------------------------------------------------------------
// serviceState
// ---------------------------------------------------------------------------

test("serviceState maps health and errors to the three brand states", () => {
  const healthy = { status: "ok", service: "dhi", version: "0.1.0" };
  const degraded = { status: "degraded", service: "dhi", version: "0.1.0" };
  assert.equal(serviceState(null, null), "loading");
  assert.equal(serviceState(healthy, null), "healthy");
  assert.equal(serviceState(degraded, null), "error");
  assert.equal(serviceState(null, "network down"), "error");
  assert.equal(serviceStatusText("healthy"), "Status: Healthy");
  assert.equal(serviceStatusText("error"), "Status: Error");
  assert.equal(serviceStatusText("loading"), "Status: Loading");
});
