import assert from "node:assert/strict";
import { test } from "node:test";
import {
  latestTeamRun,
  parseTeamRunActivity,
  parseTeamRunPayload,
  sortTeamWorkers,
  TEAM_RUN_KIND,
  type TeamRunWorker,
} from "./teamRun.ts";

const TASK_ID = "00000000-0000-4000-8000-0000000000aa";

function activity(
  id: number,
  payload: unknown,
  overrides: Record<string, unknown> = {},
): Record<string, unknown> {
  return {
    id,
    task_id: TASK_ID,
    kind: TEAM_RUN_KIND,
    message: "manager_worker team finished: 2 succeeded, 0 failed, 0 skipped",
    payload,
    created_at: "2026-11-07T10:00:00Z",
    ...overrides,
  };
}

function worker(overrides: Partial<TeamRunWorker> = {}): TeamRunWorker {
  return {
    key: "alpha",
    role: "researcher",
    status: "ok",
    subtask: "s1",
    ...overrides,
  };
}

const RICH_PAYLOAD = {
  intent_id: "intent-1",
  topology: "manager_worker",
  workers: [
    {
      key: "beta",
      role: "critic",
      status: "failed",
      subtask: "s2",
    },
    {
      key: "alpha",
      role: "researcher",
      status: "ok",
      subtask: "s1",
    },
  ],
  worker_keys: ["beta", "alpha"],
  success: 1,
  failed: 1,
  skipped: 0,
};

// ---------------------------------------------------------------------------
// parseTeamRunPayload
// ---------------------------------------------------------------------------

test("parseTeamRunPayload reads the documented worker array and sorts by key", () => {
  assert.deepEqual(parseTeamRunPayload(RICH_PAYLOAD), {
    intentId: "intent-1",
    topology: "manager_worker",
    workers: [
      worker(),
      worker({ key: "beta", role: "critic", status: "failed", subtask: "s2" }),
    ],
    workerCount: 2,
    success: 1,
    failed: 1,
    skipped: 0,
  });
});

test("parseTeamRunPayload fills key-only workers from the older worker_keys payload", () => {
  assert.deepEqual(
    parseTeamRunPayload({
      intent_id: "intent-2",
      topology: "swarm",
      workers: 2,
      worker_keys: ["beta", "alpha"],
      success: 2,
      failed: 0,
      skipped: 0,
    }),
    {
      intentId: "intent-2",
      topology: "swarm",
      workers: [
        worker({ role: null, status: null, subtask: null }),
        worker({
          key: "beta",
          role: null,
          status: null,
          subtask: null,
        }),
      ],
      workerCount: 2,
      success: 2,
      failed: 0,
      skipped: 0,
    },
  );
});

test("parseTeamRunPayload reads numeric workers with worker_details", () => {
  const parsed = parseTeamRunPayload({
    intent_id: "intent-3",
    topology: "manager_worker",
    workers: 2,
    worker_details: [
      { key: "beta", role: "critic", status: "failed", subtask: "s2" },
      { key: "alpha", role: "researcher", status: "ok", subtask: "s1" },
    ],
    worker_keys: ["beta", "alpha"],
    success: 1,
    failed: 1,
    skipped: 0,
  });
  assert.deepEqual(parsed?.workers, [
    worker(),
    worker({ key: "beta", role: "critic", status: "failed", subtask: "s2" }),
  ]);
  assert.equal(parsed?.workerCount, 2);
  assert.equal(parsed?.success, 1);
  assert.equal(parsed?.failed, 1);
});

test("parseTeamRunPayload merges worker_keys without duplicating described workers", () => {
  const parsed = parseTeamRunPayload({
    topology: "swarm",
    workers: [{ key: "alpha", role: "peer", status: "ok", subtask: "s1" }],
    worker_keys: ["alpha", "gamma", ""],
    success: 2,
  });
  assert.deepEqual(
    parsed?.workers.map((entry) => entry.key),
    ["alpha", "gamma"],
  );
  assert.equal(parsed?.workerCount, 2);
  assert.equal(parsed?.workers[1]?.role, null);
  assert.equal(parsed?.workers[1]?.status, null);
});

test("parseTeamRunPayload ignores malformed payloads and malformed workers", () => {
  for (const payload of [
    null,
    undefined,
    42,
    "team_run",
    [],
    {},
    { topology: 7 },
    { topology: "   " },
  ]) {
    assert.equal(parseTeamRunPayload(payload), null);
  }

  const parsed = parseTeamRunPayload({
    topology: "swarm",
    workers: [
      null,
      7,
      { key: "" },
      { role: "critic" },
      { key: "gamma", role: 5, status: "mystery", subtask: false },
      "delta",
    ],
    worker_keys: [null, 3],
    success: -1,
  });
  assert.deepEqual(parsed, {
    intentId: null,
    topology: "swarm",
    workers: [
      worker({ key: "delta", role: null, status: null, subtask: null }),
      worker({ key: "gamma", role: null, status: null, subtask: null }),
    ],
    workerCount: 2,
    success: 0,
    failed: 0,
    skipped: 0,
  });
});

test("parseTeamRunPayload derives counts from statuses when totals are absent", () => {
  const parsed = parseTeamRunPayload({
    topology: "swarm",
    workers: [
      { key: "a", status: "ok" },
      { key: "b", status: "failed" },
      { key: "c", status: "skipped" },
      { key: "d", status: "ok" },
    ],
  });
  assert.equal(parsed?.success, 2);
  assert.equal(parsed?.failed, 1);
  assert.equal(parsed?.skipped, 1);
  assert.equal(parsed?.workerCount, 4);
});

test("parseTeamRunPayload accepts subtask_id and worker_key aliases", () => {
  const parsed = parseTeamRunPayload({
    topology: "manager_worker",
    workers: [
      {
        worker_key: "alpha",
        role: "researcher",
        status: "ok",
        subtask_id: "s1",
      },
    ],
  });
  assert.deepEqual(parsed?.workers, [worker()]);
});

// ---------------------------------------------------------------------------
// parseTeamRunActivity / latestTeamRun
// ---------------------------------------------------------------------------

test("parseTeamRunActivity requires the team_run kind, a valid id, and a payload", () => {
  assert.deepEqual(parseTeamRunActivity(activity(4, RICH_PAYLOAD)), {
    activityId: 4,
    createdAt: "2026-11-07T10:00:00Z",
    intentId: "intent-1",
    topology: "manager_worker",
    workers: [
      worker(),
      worker({ key: "beta", role: "critic", status: "failed", subtask: "s2" }),
    ],
    workerCount: 2,
    success: 1,
    failed: 1,
    skipped: 0,
  });

  assert.equal(
    parseTeamRunActivity(activity(4, RICH_PAYLOAD, { kind: "task_done" })),
    null,
  );
  assert.equal(
    parseTeamRunActivity(activity(4, RICH_PAYLOAD, { id: "4" })),
    null,
  );
  assert.equal(
    parseTeamRunActivity(activity(4, RICH_PAYLOAD, { id: -1 })),
    null,
  );
  assert.equal(parseTeamRunActivity(activity(4, null)), null);
  assert.equal(parseTeamRunActivity(null), null);
});

test("latestTeamRun picks the highest-id valid run and ignores the rest", () => {
  const rows = [
    activity(1, RICH_PAYLOAD),
    { id: 2, kind: "team_run", payload: "junk", created_at: "" },
    activity(3, { topology: "" }),
    activity(5, { ...RICH_PAYLOAD, topology: "swarm" }),
    activity(4, RICH_PAYLOAD),
    { id: 6, kind: "task_done", payload: RICH_PAYLOAD },
  ];
  const latest = latestTeamRun(rows);
  assert.equal(latest?.activityId, 5);
  assert.equal(latest?.topology, "swarm");
  assert.equal(latestTeamRun([]), null);
  assert.equal(
    latestTeamRun([{ id: 1, kind: "team_run", payload: {}, created_at: "" }]),
    null,
  );
});

// ---------------------------------------------------------------------------
// sortTeamWorkers
// ---------------------------------------------------------------------------

test("sortTeamWorkers returns a sorted copy with deterministic tie-breaks", () => {
  const input = [
    worker({ key: "gamma", role: "peer", status: "ok" }),
    worker({ key: "alpha", role: "critic", status: "failed" }),
    worker({ key: "alpha", role: "critic", status: "ok" }),
    worker({ key: "alpha", role: "researcher", status: "ok" }),
  ];
  assert.deepEqual(
    sortTeamWorkers(input).map(
      (entry) => `${entry.key}/${entry.role}/${entry.status}`,
    ),
    [
      "alpha/critic/failed",
      "alpha/critic/ok",
      "alpha/researcher/ok",
      "gamma/peer/ok",
    ],
  );
  // The input is untouched.
  assert.deepEqual(
    input.map((entry) => entry.key),
    ["gamma", "alpha", "alpha", "alpha"],
  );
  assert.deepEqual(sortTeamWorkers([]), []);
});
