import assert from "node:assert/strict";
import { test } from "node:test";

import {
  buildCapacityPatch,
  capacityInputs,
  capacityLimitText,
  capacitySummary,
  EMPTY_CAPACITY_INPUTS,
  hasSwarmTopology,
  parseSwarmMessage,
  parseSwarmMessages,
  swarmKindLabel,
  swarmPayloadSummary,
} from "./workTeam.ts";
import type { TaskCapacity } from "./types.ts";

const TASK_ID = "00000000-0000-4000-8000-0000000000aa";
const INTENT_ID = "11111111-2222-4333-8444-555555555555";

const FULL_CAPACITY: TaskCapacity = {
  max_rounds: 80,
  max_model_calls: 120,
  max_wall_secs: 3600,
  max_parallel_workers: 4,
};

// ---------------------------------------------------------------------------
// capacityInputs / summary
// ---------------------------------------------------------------------------

test("capacityInputs seeds every input and leaves unset limits blank", () => {
  assert.deepEqual(capacityInputs(FULL_CAPACITY), {
    max_rounds: "80",
    max_model_calls: "120",
    max_wall_secs: "3600",
    max_parallel_workers: "4",
  });

  assert.deepEqual(
    capacityInputs({
      max_rounds: null,
      max_model_calls: 7,
      max_wall_secs: null,
      max_parallel_workers: null,
    }),
    {
      max_rounds: "",
      max_model_calls: "7",
      max_wall_secs: "",
      max_parallel_workers: "",
    },
  );

  assert.deepEqual(capacityInputs(null), EMPTY_CAPACITY_INPUTS);
  assert.deepEqual(capacityInputs(undefined), EMPTY_CAPACITY_INPUTS);
});

test("capacitySummary reads Engine default only for an empty ledger", () => {
  assert.equal(capacitySummary(undefined), "Engine default");
  assert.equal(capacitySummary(null), "Engine default");
  assert.equal(
    capacitySummary({
      max_rounds: null,
      max_model_calls: null,
      max_wall_secs: null,
      max_parallel_workers: null,
    }),
    "Engine default",
  );
  assert.equal(capacitySummary(FULL_CAPACITY), "80 rounds · 120 model calls · 3600s wall · 4 parallel workers");
  assert.equal(
    capacitySummary({
      max_rounds: null,
      max_model_calls: 9,
      max_wall_secs: null,
      max_parallel_workers: 2,
    }),
    "9 model calls · 2 parallel workers",
  );
});

test("capacityLimitText distinguishes a stored limit from the engine default", () => {
  assert.equal(capacityLimitText(80), "80");
  assert.equal(capacityLimitText(0), "0");
  assert.equal(capacityLimitText(null), "Engine default");
  assert.equal(capacityLimitText(undefined), "Engine default");
});

// ---------------------------------------------------------------------------
// buildCapacityPatch
// ---------------------------------------------------------------------------

test("buildCapacityPatch maps four valid inputs onto one patch", () => {
  assert.deepEqual(
    buildCapacityPatch(
      {
        max_rounds: "80",
        max_model_calls: "120",
        max_wall_secs: " 3600 ",
        max_parallel_workers: "4",
      },
      null,
    ),
    { ok: true, patch: FULL_CAPACITY },
  );
});

test("buildCapacityPatch omits blank and unchanged fields", () => {
  const current: TaskCapacity = {
    max_rounds: 80,
    max_model_calls: null,
    max_wall_secs: 3600,
    max_parallel_workers: null,
  };
  assert.deepEqual(
    buildCapacityPatch(
      {
        max_rounds: "80",
        max_model_calls: "120",
        max_wall_secs: "",
        max_parallel_workers: "  ",
      },
      current,
    ),
    { ok: true, patch: { max_model_calls: 120 } },
  );
});

test("buildCapacityPatch refuses zero, negative, fractional, and text values", () => {
  for (const raw of ["0", "-1", "1.5", "many", "1e3", "  "]) {
    const result = buildCapacityPatch({ ...EMPTY_CAPACITY_INPUTS, max_rounds: raw });
    if (raw.trim() === "") {
      assert.equal(result.ok, false);
    } else {
      assert.equal(result.ok, false);
      assert.match(
        result.ok ? "" : result.error,
        /Max rounds must be a positive whole number/,
      );
    }
  }
});

test("buildCapacityPatch rejects values beyond Number.MAX_SAFE_INTEGER", () => {
  const result = buildCapacityPatch({
    ...EMPTY_CAPACITY_INPUTS,
    max_model_calls: "9007199254740992",
  });
  assert.equal(result.ok, false);
  assert.match(result.ok ? "" : result.error, /Max model calls/);
});

test("buildCapacityPatch refuses a patch with nothing to change", () => {
  const result = buildCapacityPatch(capacityInputs(FULL_CAPACITY), FULL_CAPACITY);
  assert.equal(result.ok, false);
  assert.equal(result.ok ? "" : result.error, "No capacity changes to apply.");

  const blank = buildCapacityPatch(EMPTY_CAPACITY_INPUTS, FULL_CAPACITY);
  assert.equal(blank.ok, false);
  assert.equal(blank.ok ? "" : blank.error, "No capacity changes to apply.");
});

test("buildCapacityPatch treats omitted current limits as changed", () => {
  const result = buildCapacityPatch(
    { ...EMPTY_CAPACITY_INPUTS, max_parallel_workers: "4" },
    undefined,
  );
  assert.deepEqual(result, {
    ok: true,
    patch: { max_parallel_workers: 4 },
  });
});

// ---------------------------------------------------------------------------
// swarm display helpers
// ---------------------------------------------------------------------------

test("parseSwarmMessage renders a finding with sender, short intent, and subtask", () => {
  const view = parseSwarmMessage({
    id: 12,
    task_id: TASK_ID,
    intent_id: INTENT_ID,
    kind: "finding",
    payload: {
      type: "finding",
      from: "worker-a",
      subtask_id: "s1",
      summary: "the rook lift wins",
      evidence: "Ra8+",
    },
    created_at: "2026-11-07T10:00:00Z",
  });

  assert.deepEqual(view, {
    id: 12,
    kind: "finding",
    from: "worker-a",
    intentId: INTENT_ID,
    intentShort: "11111111",
    createdAt: "2026-11-07T10:00:00Z",
    summary: "the rook lift wins · subtask s1",
  });
});

test("parseSwarmMessage handles question, challenge, done, and sender aliases", () => {
  assert.equal(
    parseSwarmMessage({
      id: 1,
      kind: "question",
      intent_id: INTENT_ID,
      payload: { sender: "worker-b", text: "which file?" },
    })?.summary,
    "which file?",
  );
  assert.equal(
    parseSwarmMessage({
      id: 2,
      kind: "challenge",
      intent_id: INTENT_ID,
      payload: { from: "worker-c", text: "the king can escape" },
    })?.from,
    "worker-c",
  );
  assert.equal(
    parseSwarmMessage({
      id: 3,
      kind: "done",
      intent_id: INTENT_ID,
      payload: { type: "done", from: "worker-d" },
    })?.summary,
    "done",
  );
});

test("swarmPayloadSummary prefers summary, then text/message, then compact JSON", () => {
  assert.equal(
    swarmPayloadSummary({ summary: "found it", subtask_id: "" }),
    "found it",
  );
  assert.equal(swarmPayloadSummary({ message: "note" }), "note");
  assert.equal(swarmPayloadSummary({ answer: 42 }), '{"answer":42}');
  assert.equal(swarmPayloadSummary("plain"), '"plain"');
  assert.equal(swarmPayloadSummary(null), "null");

  const circular: Record<string, unknown> = {};
  circular.self = circular;
  assert.equal(swarmPayloadSummary(circular), "[unprintable payload]");
});

test("swarmPayloadSummary truncates very large fallback JSON", () => {
  const long = swarmPayloadSummary({ blob: "x".repeat(500) });
  assert.equal(long.length, 240);
  assert.match(long, /…$/);
});

test("swarmKindLabel names known kinds and keeps unknown kinds verbatim", () => {
  assert.equal(swarmKindLabel("finding"), "Finding");
  assert.equal(swarmKindLabel("DONE"), "Done");
  assert.equal(swarmKindLabel("custom-kind"), "custom-kind");
  assert.equal(swarmKindLabel("  "), "Message");
});

test("parseSwarmMessage rejects rows without a usable id and degrades the rest", () => {
  for (const record of [
    null,
    "row",
    {},
    { id: "1" },
    { id: -1 },
    { id: 1.5 },
  ]) {
    assert.equal(parseSwarmMessage(record), null);
  }

  const view = parseSwarmMessage({
    id: 0,
    kind: "",
    payload: null,
    created_at: 7,
  });
  assert.deepEqual(view, {
    id: 0,
    kind: "message",
    from: null,
    intentId: "",
    intentShort: "",
    createdAt: "",
    summary: "{}",
  });
});

test("parseSwarmMessages filters malformed rows and preserves server order", () => {
  const views = parseSwarmMessages([
    { id: 2, kind: "done", intent_id: INTENT_ID, payload: { type: "done" } },
    { id: "bad" },
    { id: 1, kind: "question", intent_id: INTENT_ID, payload: { text: "why?" } },
  ]);
  assert.deepEqual(
    views.map((view) => view.id),
    [2, 1],
  );
});

test("hasSwarmTopology finds a swarm team fact and tolerates malformed facts", () => {
  assert.equal(hasSwarmTopology([]), false);
  assert.equal(
    hasSwarmTopology([
      { provenance: { topology: "manager_worker" } },
      { provenance: { topology: "swarm" } },
    ]),
    true,
  );
  assert.equal(
    hasSwarmTopology([
      {},
      { provenance: null },
      { provenance: "swarm" },
      { provenance: { topology: "SWARM" } },
    ]),
    false,
  );
});

