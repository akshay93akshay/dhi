import assert from "node:assert/strict";
import { test } from "node:test";

import {
  buildSwarmGraph,
  buildTeamMessageGraph,
  mapTeamMessages,
  SWARM_CIRCLE_LIMIT,
  SWARM_DEFAULT_EDGE_COLOR,
  SWARM_EDGE_COLORS,
  swarmEdgeColor,
  swarmNodeId,
} from "./swarmGraph.ts";
import {
  groupSwarmRuns,
  parseSwarmMessages,
  swarmRoundLabel,
} from "./workTeam.ts";
import type { SwarmRoundGroup } from "./workTeam.ts";

const RUN_ONE = "aaaaaaaa-1111-4111-8111-111111111111";
const RUN_TWO = "bbbbbbbb-2222-4222-8222-222222222222";

type RecordPayload = Record<string, unknown>;

function swarmRecord(
  id: number,
  intentId: string,
  from: string,
  kind: string,
  payload: RecordPayload,
) {
  return {
    id,
    kind,
    intent_id: intentId,
    created_at: `2026-01-01T00:00:0${id}.000Z`,
    payload: { from, ...payload },
  };
}

function fixtureMessages() {
  return parseSwarmMessages([
    swarmRecord(1, RUN_ONE, "alice", "finding", {
      type: "finding",
      summary: "Alice one",
      evidence: "shared evidence",
      subtask_id: "task:r1:alice",
    }),
    swarmRecord(2, RUN_ONE, "bob", "finding", {
      type: "finding",
      summary: "Bob one",
      subtask_id: "task:r1:bob",
    }),
    swarmRecord(3, RUN_ONE, "alice", "question", {
      type: "question",
      text: "Why?",
      subtask_id: "task:r2:alice",
    }),
    swarmRecord(4, RUN_ONE, "carol", "challenge", {
      type: "challenge",
      text: "No.",
      subtask_id: "task:r2:carol",
    }),
    swarmRecord(5, RUN_ONE, "alice", "finding", {
      type: "finding",
      summary: "Alice one",
      evidence: "shared evidence",
      subtask_id: "task:r2:alice-again",
    }),
    swarmRecord(6, RUN_TWO, "dave", "finding", {
      type: "finding",
      summary: "Dave one",
      subtask_id: "task:r1:dave",
    }),
  ]);
}

function fixtureRuns() {
  return groupSwarmRuns(fixtureMessages());
}

test("swarmEdgeColor maps known kinds and keeps unknown kinds neutral", () => {
  assert.equal(swarmEdgeColor("finding"), "#16a34a");
  assert.equal(swarmEdgeColor(" Question "), "#2563eb");
  assert.equal(swarmEdgeColor("CHALLENGE"), "#d97706");
  assert.equal(swarmEdgeColor("done"), SWARM_DEFAULT_EDGE_COLOR);
  assert.deepEqual(SWARM_EDGE_COLORS, {
    finding: "#16a34a",
    question: "#2563eb",
    challenge: "#d97706",
  });
});

test("buildSwarmGraph defaults to the first message of the round", () => {
  const runs = fixtureRuns();
  assert.equal(runs.length, 2);
  assert.deepEqual(
    runs.map((run) => run.label),
    ["Run 1", "Run 2"],
  );

  const run = runs[0];
  assert.deepEqual(
    run.rounds.map((round) => round.round),
    [1, 2],
  );

  const model = buildSwarmGraph(run, run.rounds[0]);

  assert.equal(model.layout, "circle");
  assert.equal(model.selected?.id, 1);
  assert.equal(model.sender, "alice");
  assert.deepEqual(model.recipients, ["bob", "carol"]);
  assert.equal(model.peerCount, 2);
  assert.equal(model.color, SWARM_EDGE_COLORS.finding);

  assert.deepEqual(
    model.nodes.map((node) => node.id),
    ["alice", "bob", "carol"].map(swarmNodeId),
  );
  const nodes = new Map(
    model.nodes.map((node) => [node.data.sender, node.data]),
  );
  assert.equal(nodes.get("alice")?.count, 3);
  assert.equal(nodes.get("bob")?.count, 1);
  assert.equal(nodes.get("carol")?.count, 1);
  assert.equal(nodes.get("alice")?.selected, true);
  assert.equal(nodes.get("alice")?.recipient, false);
  assert.equal(nodes.get("bob")?.recipient, true);
  assert.equal(nodes.get("carol")?.recipient, true);

  assert.deepEqual(
    model.edges.map((edge) => edge.target),
    [swarmNodeId("bob"), swarmNodeId("carol")],
  );
  assert.ok(model.edges.every((edge) => edge.source === swarmNodeId("alice")));
  assert.ok(
    model.edges.every((edge) => edge.color === SWARM_EDGE_COLORS.finding),
  );

  assert.deepEqual(
    model.chips.map((chip) => chip.label),
    ["alice · Finding", "bob · Finding"],
  );
  assert.deepEqual(
    model.chips.map((chip) => chip.selected),
    [true, false],
  );
  assert.deepEqual(
    model.chips.map((chip) => chip.id),
    [1, 2],
  );
});

test("selecting a message broadcasts from its sender and marks repeats", () => {
  const run = fixtureRuns()[0];
  const model = buildSwarmGraph(run, run.rounds[1], 4);

  assert.equal(model.selected?.id, 4);
  assert.equal(model.sender, "carol");
  assert.deepEqual(model.recipients, ["alice", "bob"]);
  assert.equal(model.color, SWARM_EDGE_COLORS.challenge);
  assert.equal(model.chips.length, 3);
  assert.deepEqual(
    model.chips.map((chip) => chip.id),
    [3, 4, 5],
  );
  assert.deepEqual(
    model.chips.map((chip) => chip.selected),
    [false, true, false],
  );
  assert.deepEqual(
    model.chips.map((chip) => chip.repeat),
    [false, false, true],
  );

  assert.equal(model.edges.length, 2);
  assert.ok(model.edges.every((edge) => edge.source === swarmNodeId("carol")));
  assert.ok(
    model.edges.every(
      (edge) => edge.color === SWARM_EDGE_COLORS.challenge,
    ),
  );
  assert.deepEqual(
    model.edges.map((edge) => edge.target),
    [swarmNodeId("alice"), swarmNodeId("bob")],
  );

  const nodes = new Map(
    model.nodes.map((node) => [node.data.sender, node.data]),
  );
  assert.equal(nodes.get("carol")?.selected, true);
  assert.equal(nodes.get("alice")?.recipient, true);
  assert.equal(nodes.get("bob")?.recipient, true);
});

test("an unknown or cross-round selected id falls back to the first chip", () => {
  const run = fixtureRuns()[0];
  const model = buildSwarmGraph(run, run.rounds[1], 9999);

  assert.equal(model.selected?.id, 3);
  assert.equal(model.sender, "alice");
  assert.deepEqual(model.recipients, ["bob", "carol"]);
  assert.equal(model.chips[0].selected, true);
  assert.equal(model.edges.length, 2);
});

test("seed and numbered rounds use the same list and graph labels", () => {
  const views = parseSwarmMessages([
    swarmRecord(1, RUN_ONE, "alice", "finding", {
      type: "finding",
      summary: "seed finding",
    }),
    swarmRecord(2, RUN_ONE, "bob", "finding", {
      type: "finding",
      summary: "peer finding",
      subtask_id: "task:r2:peer:alice",
    }),
  ]);
  const run = groupSwarmRuns(views)[0];
  assert.ok(run);
  // The seed turn runs before round 2 and stays first in the selector.
  assert.deepEqual(
    run.rounds.map((round) => round.round),
    [null, 2],
  );
  assert.deepEqual(
    run.rounds.map((round) => round.label),
    ["Round 1 (seed)", "Round 2"],
  );

  const seed = buildSwarmGraph(run, run.rounds[0]!);
  const roundTwo = buildSwarmGraph(run, run.rounds[1]!);
  assert.equal(seed.roundLabel, run.rounds[0]!.label);
  assert.equal(seed.roundLabel, swarmRoundLabel(null));
  assert.equal(seed.roundLabel, "Round 1 (seed)");
  assert.equal(roundTwo.roundLabel, run.rounds[1]!.label);
  assert.equal(roundTwo.roundLabel, swarmRoundLabel(2));
  assert.equal(roundTwo.roundLabel, "Round 2");
});

test("an empty round has no selection, chips, or broadcast edges", () => {
  const run = fixtureRuns()[0];
  const emptyRound: SwarmRoundGroup = {
    round: 1,
    label: swarmRoundLabel(1),
    senders: [],
    counts: { total: 0, workers: 0, rounds: 0, byKind: {} },
  };
  const model = buildSwarmGraph(run, emptyRound);

  assert.equal(model.selected, null);
  assert.equal(model.sender, null);
  assert.deepEqual(model.recipients, []);
  assert.deepEqual(model.chips, []);
  assert.deepEqual(model.edges, []);
  assert.equal(model.nodes.length, 3);
  assert.ok(model.nodes.every((node) => !node.data.selected));
  assert.ok(model.nodes.every((node) => !node.data.recipient));
  assert.equal(model.color, SWARM_DEFAULT_EDGE_COLOR);
});

function runWithWorkers(count: number) {
  const records = Array.from({ length: count }, (_, index) =>
    swarmRecord(
      index + 1,
      RUN_ONE,
      `worker-${index}`,
      "finding",
      {
        type: "finding",
        summary: `finding ${index}`,
        subtask_id: `task:r1:worker-${index}`,
      },
    ),
  );
  const runs = groupSwarmRuns(parseSwarmMessages(records));
  return { run: runs[0], round: runs[0].rounds[0] };
}

test("six or fewer workers use a circle with distinct positions", () => {
  const { run, round } = runWithWorkers(SWARM_CIRCLE_LIMIT);
  const model = buildSwarmGraph(run, round);

  assert.equal(model.layout, "circle");
  assert.equal(model.nodes.length, SWARM_CIRCLE_LIMIT);
  const positions = new Set(
    model.nodes.map((node) => `${node.position.x},${node.position.y}`),
  );
  assert.equal(positions.size, SWARM_CIRCLE_LIMIT);
});

test("more than six workers switch to a centered grid", () => {
  const workerCount = SWARM_CIRCLE_LIMIT + 1;
  const { run, round } = runWithWorkers(workerCount);
  const model = buildSwarmGraph(run, round);

  assert.equal(model.layout, "grid");
  assert.equal(model.nodes.length, workerCount);
  const positions = new Set(
    model.nodes.map((node) => `${node.position.x},${node.position.y}`),
  );
  assert.equal(positions.size, workerCount);

  const xs = model.nodes.map((node) => node.position.x);
  const ys = model.nodes.map((node) => node.position.y);
  assert.equal(Math.min(...xs) + Math.max(...xs), 0);
  assert.equal(Math.min(...ys) + Math.max(...ys), 0);
});

// ---------------------------------------------------------------------------
// Team messages: direct and broadcast graph mapping
// ---------------------------------------------------------------------------

function teamMessage(
  id: number,
  from: string,
  to: string | null,
  subject: string,
) {
  return {
    id,
    team_id: "team-1",
    from_agent_id: from,
    to_agent_id: to,
    subject,
    body: `${subject} body`,
    task_id: null,
    created_at: `2026-11-10T10:00:0${id}Z`,
    read_at: null,
  };
}

const TEAM_MESSAGES = [
  teamMessage(1, "coordinator", "w1", "Direct one"),
  teamMessage(2, "w1", null, "Broadcast one"),
  teamMessage(3, "w2", "coordinator", "Reply"),
];

test("team graph maps a direct message to exactly its recipient", () => {
  const model = buildTeamMessageGraph(TEAM_MESSAGES, 1);

  assert.equal(model.selected?.id, "1");
  assert.equal(model.sender, "coordinator");
  assert.deepEqual(model.recipients, ["w1"]);
  assert.equal(model.broadcast, false);
  assert.equal(model.layout, "circle");

  assert.deepEqual(
    model.nodes.map((node) => node.data.sender),
    ["coordinator", "w1", "w2"],
  );
  assert.deepEqual(
    model.edges.map((edge) => [edge.source, edge.target, edge.kind]),
    [[swarmNodeId("coordinator"), swarmNodeId("w1"), "direct"]],
  );

  const nodes = new Map(
    model.nodes.map((node) => [node.data.sender, node.data]),
  );
  assert.equal(nodes.get("coordinator")?.selected, true);
  assert.equal(nodes.get("w1")?.recipient, true);
  assert.equal(nodes.get("w2")?.recipient, false);
  assert.equal(nodes.get("coordinator")?.count, 1);
  assert.equal(nodes.get("w1")?.count, 1);
  assert.equal(nodes.get("w2")?.count, 1);
});

test("team graph fans a broadcast out to every other participant", () => {
  const model = buildTeamMessageGraph(TEAM_MESSAGES, 2);

  assert.equal(model.selected?.id, "2");
  assert.equal(model.sender, "w1");
  assert.equal(model.broadcast, true);
  assert.deepEqual(model.recipients, ["coordinator", "w2"]);
  assert.deepEqual(
    model.edges.map((edge) => [edge.source, edge.target, edge.kind]),
    [
      [swarmNodeId("w1"), swarmNodeId("coordinator"), "broadcast"],
      [swarmNodeId("w1"), swarmNodeId("w2"), "broadcast"],
    ],
  );

  assert.deepEqual(
    model.chips.map((chip) => [chip.message.from, chip.message.to]),
    [
      ["coordinator", "w1"],
      ["w1", null],
      ["w2", "coordinator"],
    ],
  );
  assert.deepEqual(
    model.chips.map((chip) => chip.selected),
    [false, true, false],
  );
});

test("team graph defaults to the first message and keeps broadcast labels", () => {
  const model = buildTeamMessageGraph(TEAM_MESSAGES);

  assert.equal(model.selected?.id, "1");
  assert.equal(model.chips[0]?.selected, true);
  assert.equal(model.chips[1]?.label, "w1 → all · Broadcast one");
  assert.equal(model.chips[0]?.label, "coordinator → w1 · Direct one");
});

test("team graph parses aliases and ignores unusable rows", () => {
  const views = mapTeamMessages([
    {
      id: "m1",
      from: "alice",
      to: "all",
      subject: "Hello",
      body: "Team update",
    },
    { id: "", from_agent_id: "ignored" },
  ]);
  assert.deepEqual(views, [
    {
      id: "m1",
      from: "alice",
      to: null,
      subject: "Hello",
      body: "Team update",
      sentAt: "",
      broadcast: true,
    },
  ]);
});
