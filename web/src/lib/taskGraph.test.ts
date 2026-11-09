import assert from "node:assert/strict";
import { test } from "node:test";
import {
  TASK_GRAPH_KIND_LABELS,
  TASK_GRAPH_LANE_GAP,
  TASK_GRAPH_MAX_LANE_GAP,
  TASK_GRAPH_MAX_ROW_BAND_GAP,
  TASK_GRAPH_NODE_HEIGHT,
  TASK_GRAPH_NODE_WIDTH,
  TASK_GRAPH_ROW_BAND_GAP,
  TASK_PAGE_SIZE,
  buildTaskGraph,
  filterTasksByGoal,
  layoutTaskGraph,
  paginateTasks,
  selectVisibleTaskId,
  type TaskGraphEdge,
} from "./taskGraph.ts";
import {
  TASK_GRAPH_BACKWARD_EDGE_TYPE,
  TASK_GRAPH_RELATION_LABELS,
  buildTaskGraphFlow,
  findTaskGraphFlowNode,
  taskGraphNodeText,
} from "./taskGraphFlow.ts";
import type {
  Fact,
  Goal,
  Intent,
  ResultRecord,
  Task,
  TaskWork,
} from "./types.ts";

// ---------------------------------------------------------------------------
// Task graph — derivation, edges, layout, pagination
// ---------------------------------------------------------------------------

const LOOKUP_ISO = "2026-11-01T00:00:00Z";

function taskGraphFixture(): TaskWork {
  const task: Task = {
    id: "task-1",
    goal: "Prove the sample is mate in two",
    state: "running",
    exploration_id: "exploration-1",
    origin_fact_id: "fact-origin",
    pinned_planner_profile_id: null,
    pinned_worker_profile_id: null,
    created_at: LOOKUP_ISO,
    updated_at: LOOKUP_ISO,
  };

  const goals: Goal[] = [
    {
      id: "goal-1",
      task_id: task.id,
      description: "mate in two",
      state: "met",
      proof_fact_id: "fact-proof",
      created_at: LOOKUP_ISO,
    },
    {
      id: "goal-2",
      task_id: task.id,
      description: "explain the line",
      state: "open",
      proof_fact_id: null,
      created_at: LOOKUP_ISO,
    },
  ];

  const intents: Intent[] = [
    {
      id: "intent-1",
      task_id: task.id,
      goal_id: "goal-1",
      fact_id: "fact-origin",
      description: "search the mating line",
      state: "done",
      owner: "worker-a",
      priority: 1,
      created_at: LOOKUP_ISO,
      completed_at: LOOKUP_ISO,
    },
    {
      id: "intent-2",
      task_id: task.id,
      goal_id: null,
      fact_id: "fact-origin",
      description: "unscoped side exploration",
      state: "open",
      owner: null,
      priority: 0,
      created_at: LOOKUP_ISO,
      completed_at: null,
    },
  ];

  const facts: Fact[] = [
    {
      id: "fact-origin",
      task_id: task.id,
      claim: "Origin: task started",
      state: "origin",
      provenance: { goal: task.goal },
      origin_of: null,
      created_at: LOOKUP_ISO,
    },
    {
      id: "fact-proof",
      task_id: task.id,
      claim: "The mating line exists",
      state: "confirmed",
      provenance: {
        intent_id: "intent-1",
        detail: "found a forcing line",
        evidence: "Qh5+ Kf8 Qh8#",
        confidence: "high",
      },
      origin_of: "fact-origin",
      created_at: LOOKUP_ISO,
    },
    {
      id: "fact-orphan",
      task_id: task.id,
      claim: "An observation without a producer",
      state: "confirmed",
      provenance: { intent_id: "intent-missing" },
      origin_of: "fact-missing",
      created_at: LOOKUP_ISO,
    },
  ];

  const results: ResultRecord[] = [
    {
      id: "result-1",
      task_id: task.id,
      intent_id: "intent-1",
      claim: "Line accepted",
      acceptance: "accepted",
      evidence: { line: ["Qh5+", "Kf8", "Qh8#"] },
      created_at: LOOKUP_ISO,
    },
    {
      id: "result-orphan",
      task_id: task.id,
      intent_id: "intent-missing",
      claim: "Orphan decision",
      acceptance: "pending",
      evidence: {},
      created_at: LOOKUP_ISO,
    },
  ];

  return { task, goals, intents, facts, results };
}

function edgeKey(edge: TaskGraphEdge): string {
  return `${edge.from} ${edge.relation} ${edge.to}`;
}

test("buildTaskGraph derives the five node kinds without a task node", () => {
  const model = buildTaskGraph(taskGraphFixture());
  assert.deepEqual(
    model.nodes.map((node) => node.kind),
    [
      "origin",
      "goal",
      "goal",
      "intent",
      "intent",
      "fact",
      "fact",
      "result",
      "result",
    ],
  );
  // The origin fact is the root; no task node exists.
  const kinds = new Set<string>(model.nodes.map((node) => node.kind));
  assert.ok(!kinds.has("task"));
  const origin = model.nodes.find((node) => node.kind === "origin");
  assert.equal(origin?.id, "fact:fact-origin");
  assert.equal(origin?.entityId, "fact-origin");
  assert.equal(origin?.label, "Origin: task started");
  assert.equal(origin?.state, "origin");
  // Exactly one origin node is the root; has_goal edges leave that root.
  const origins = model.nodes.filter((node) => node.kind === "origin");
  assert.equal(origins.length, 1);
  assert.ok(
    model.edges
      .filter((edge) => edge.relation === "has_goal")
      .every((edge) => edge.from === origins[0]?.id),
  );
  // Every id is namespaced, so collections cannot collide.
  assert.ok(model.nodes.every((node) => node.id.includes(":")));
});

test("task graph nodes keep the full label for natural card wrapping", () => {
  const work = taskGraphFixture();
  const longGoal =
    "Prove that every legal king move after the sacrifice still leads to a forced mate in two, including the quiet waiting move and the long diagonal retreat";
  const longIntent =
    "Trace the entire forcing sequence through both defensive knight blocks without dropping the thread when the queen checks from the far side of the board";
  work.goals[0]!.description = longGoal;
  work.intents[0]!.description = longIntent;

  const model = buildTaskGraph(work);
  const labelOf = (id: string): string =>
    model.nodes.find((node) => node.id === id)?.label ?? "";

  // The model never clamps, ellipsizes, or shortens the source text.
  assert.equal(labelOf("goal:goal-1"), longGoal);
  assert.equal(labelOf("intent:intent-1"), longIntent);
  assert.ok(model.nodes.every((node) => !node.label.includes("…")));
  assert.ok(model.nodes.every((node) => !node.label.includes("...")));
});

test("buildTaskGraph derives only the documented linear edges and suppresses orphans", () => {
  const model = buildTaskGraph(taskGraphFixture());
  assert.deepEqual(
    model.edges.map(edgeKey).sort(),
    [
      "fact:fact-origin has_goal goal:goal-1",
      "fact:fact-origin has_goal goal:goal-2",
      "fact:fact-origin spawns intent:intent-2",
      "fact:fact-proof derived_from fact:fact-origin",
      "goal:goal-1 spawns intent:intent-1",
      "intent:intent-1 yields fact:fact-proof",
      "intent:intent-1 yields result:result-1",
    ].sort(),
  );
  const relations = new Set<string>(model.edges.map((edge) => edge.relation));
  assert.ok(!relations.has("proves"));
  assert.ok(!relations.has("supports"));
  assert.ok(!relations.has("starts_with"));
  assert.ok(
    model.edges.every(
      (edge) =>
        !edge.from.startsWith("task:") && !edge.to.startsWith("task:"),
    ),
  );
  // Orphan references never become edges: no missing nodes, no stray links.
  assert.ok(
    model.edges.every(
      (edge) =>
        !edge.from.includes("missing") && !edge.to.includes("missing"),
    ),
  );
  assert.equal(
    model.edges.filter((edge) => edge.from === "fact:fact-orphan").length,
    0,
  );
  assert.equal(
    model.edges.filter((edge) => edge.to === "result:result-orphan").length,
    0,
  );
});

test("proves is a marker on goals and facts, never an edge", () => {
  const model = buildTaskGraph(taskGraphFixture());
  const goal1 = model.nodes.find((node) => node.id === "goal:goal-1");
  assert.ok(goal1 !== undefined && goal1.kind === "goal");
  assert.equal(goal1.proofFactId, "fact-proof");

  const goal2 = model.nodes.find((node) => node.id === "goal:goal-2");
  assert.ok(goal2 !== undefined && goal2.kind === "goal");
  assert.equal(goal2.proofFactId, null);

  const proofFact = model.nodes.find((node) => node.id === "fact:fact-proof");
  assert.ok(proofFact !== undefined && proofFact.kind === "fact");
  assert.equal(proofFact.provesGoal, true);

  const origin = model.nodes.find((node) => node.kind === "origin");
  assert.ok(origin !== undefined && origin.kind === "origin");
  assert.equal(origin.provesGoal, false);

  // The marker never becomes a relation or an edge-count entry.
  const relations = new Set<string>(model.edges.map((edge) => edge.relation));
  assert.ok(!relations.has("proves"));
  assert.equal(model.edges.length, 7);
});

test("buildTaskGraph never draws duplicate nodes or edges", () => {
  const work = taskGraphFixture();
  work.goals = [...work.goals, work.goals[0]!];
  work.intents = [...work.intents, work.intents[0]!];
  work.facts = [...work.facts, work.facts[1]!];
  const model = buildTaskGraph(work);

  const nodeIds = model.nodes.map((node) => node.id);
  assert.equal(new Set(nodeIds).size, nodeIds.length);

  const keys = model.edges.map(edgeKey);
  assert.equal(new Set(keys).size, keys.length);
  assert.equal(
    model.edges.filter((edge) => edge.relation === "has_goal").length,
    2,
  );
});

test("an empty task yields no nodes; origin-only adds just the root", () => {
  const empty = taskGraphFixture();
  empty.goals = [];
  empty.intents = [];
  empty.facts = [];
  empty.results = [];
  const noWork = buildTaskGraph(empty);
  assert.deepEqual(noWork.nodes, []);
  assert.equal(noWork.edges.length, 0);

  const origin = taskGraphFixture().facts[0]!;
  const originOnly = buildTaskGraph({
    ...empty,
    facts: [origin],
  });
  assert.deepEqual(
    originOnly.nodes.map((node) => node.kind),
    ["origin"],
  );
  // There is no starts_with edge; the origin is simply the root.
  assert.equal(originOnly.edges.length, 0);

  const goalOnly = buildTaskGraph({
    ...empty,
    goals: [taskGraphFixture().goals[0]!],
  });
  assert.deepEqual(
    goalOnly.nodes.map((node) => node.kind),
    ["goal"],
  );
  // No origin endpoint means the has_goal edge is suppressed as an orphan.
  assert.equal(goalOnly.edges.length, 0);
});

test("layoutTaskGraph puts the five kinds in fixed columns and centres the origin", () => {
  const model = buildTaskGraph(taskGraphFixture());
  const layout = layoutTaskGraph(model.nodes);
  const at = (id: string) => layout.positions.get(id)!;
  const xs = [
    at("fact:fact-origin").x,
    at("goal:goal-1").x,
    at("intent:intent-1").x,
    at("fact:fact-proof").x,
    at("result:result-1").x,
  ];
  assert.equal(new Set(xs).size, 5);
  assert.deepEqual(xs, [...xs].sort((a, b) => a - b));
  assert.equal(layout.positions.size, model.nodes.length);
  assert.equal(layout.columnCenters.length, 5);
  assert.equal(layout.nodeWidth, TASK_GRAPH_NODE_WIDTH);
  assert.ok(
    layout.nodeWidth >= 200 && layout.nodeWidth <= 210,
    "cards stay in the 200–210px reading width",
  );
  assert.equal(layout.nodeHeight, TASK_GRAPH_NODE_HEIGHT);
  assert.equal(layout.nodeHeights.size, model.nodes.length);

  const centerY = (id: string): number =>
    at(id).y + layout.nodeHeight / 2;
  assert.ok(
    Math.abs(centerY("fact:fact-origin") - layout.height / 2) < 0.001,
  );
  assert.equal(
    layout.lanes.filter((lane) => lane.kind === "goal").length,
    2,
  );
  assert.ok(layout.lanes.some((lane) => lane.kind === "root"));
});

test("goals align with their intents/facts and results align with their intent row", () => {
  const model = buildTaskGraph(taskGraphFixture());
  const layout = layoutTaskGraph(model.nodes);
  const at = (id: string) => layout.positions.get(id)!;
  const centerY = (id: string): number =>
    at(id).y + layout.nodeHeight / 2;

  const lane = layout.lanes.find((entry) => entry.id === "goal:goal-1");
  assert.ok(lane !== undefined);
  assert.equal(centerY("goal:goal-1"), lane.center);
  assert.equal(centerY("intent:intent-1"), lane.center);
  assert.equal(centerY("fact:fact-proof"), lane.center);
  assert.equal(centerY("result:result-1"), centerY("intent:intent-1"));

  const rootRow = layout.rows.find(
    (row) => row.intentId === "intent:intent-2",
  );
  assert.ok(rootRow !== undefined);
  assert.equal(rootRow.laneId, "root");
});

test("layoutTaskGraph stacks multiple nodes in one lane with clear spacing", () => {
  const work = taskGraphFixture();
  const intent = work.intents[0]!;
  const fact = work.facts[1]!;
  work.intents = [
    ...work.intents,
    { ...intent, id: "intent-3", description: "second branch" },
  ];
  work.facts = [
    ...work.facts,
    {
      ...fact,
      id: "fact-3",
      claim: "Second fact",
      provenance: { intent_id: "intent-3" },
      origin_of: null,
    },
  ];
  const model = buildTaskGraph(work);
  const layout = layoutTaskGraph(model.nodes);
  const rowA = layout.rows.find(
    (row) => row.intentId === "intent:intent-1",
  );
  const rowB = layout.rows.find(
    (row) => row.intentId === "intent:intent-3",
  );
  assert.ok(rowA !== undefined && rowB !== undefined);
  assert.ok(rowB.top >= rowA.top + rowA.height);
  const lane = layout.lanes.find((entry) => entry.id === "goal:goal-1");
  assert.ok(lane !== undefined);
  assert.ok(lane.height > layout.nodeHeight * 2);
});

test("layoutTaskGraph grows rows, lanes, and stacks from measured heights", () => {
  const work = taskGraphFixture();
  const intent = work.intents[0]!;
  const fact = work.facts[1]!;
  work.intents = [
    ...work.intents,
    { ...intent, id: "intent-3", description: "second branch" },
  ];
  work.facts = [
    ...work.facts,
    {
      ...fact,
      id: "fact-3",
      claim: "Second fact",
      provenance: { intent_id: "intent-3" },
      origin_of: null,
    },
  ];

  const model = buildTaskGraph(work);
  const heights = new Map<string, number>([
    ["intent:intent-1", 180],
    ["fact:fact-proof", 72],
    ["result:result-1", 64],
    ["goal:goal-1", 120],
  ]);
  const layout = layoutTaskGraph(model.nodes, heights);
  const at = (id: string) => layout.positions.get(id)!;
  const rowA = layout.rows.find(
    (row) => row.intentId === "intent:intent-1",
  );
  const rowB = layout.rows.find(
    (row) => row.intentId === "intent:intent-3",
  );
  assert.ok(rowA !== undefined && rowB !== undefined);

  // The wrapper intent sets the row height; its fact/result stay centred.
  assert.equal(rowA.height, 180);
  assert.equal(rowA.center, at("intent:intent-1").y + 180 / 2);
  assert.equal(rowA.center, at("fact:fact-proof").y + 72 / 2);
  assert.equal(rowA.center, at("result:result-1").y + 64 / 2);

  // The next row starts after the measured row plus the usual band gap.
  assert.equal(rowB.top, rowA.top + rowA.height + TASK_GRAPH_ROW_BAND_GAP);
  assert.equal(rowB.height, layout.nodeHeight);

  const lane = layout.lanes.find((entry) => entry.id === "goal:goal-1");
  assert.ok(lane !== undefined);
  assert.equal(
    lane.height,
    rowA.height + rowB.height + TASK_GRAPH_ROW_BAND_GAP,
  );
  // The goal card uses its own measured height while staying lane-centred.
  assert.equal(at("goal:goal-1").y + 120 / 2, lane.center);

  const nextLane = layout.lanes[layout.lanes.indexOf(lane) + 1];
  assert.ok(nextLane !== undefined);
  assert.equal(nextLane.top, lane.top + lane.height + TASK_GRAPH_LANE_GAP);

  // A taller fact stack raises its row and the canvas, without moving origin
  // off the vertical centre.
  const taller = layoutTaskGraph(
    model.nodes,
    new Map([["fact:fact-proof", 300]]),
  );
  const tallerRow = taller.rows.find(
    (row) => row.intentId === "intent:intent-1",
  );
  assert.ok(tallerRow !== undefined);
  assert.equal(tallerRow.height, 300);
  assert.ok(taller.height > layout.height);
  assert.equal(
    taller.positions.get("fact:fact-origin")!.y +
      taller.nodeHeights.get("fact:fact-origin")! / 2,
    taller.height / 2,
  );
});

test("minHeight spreads lane gaps and keeps every lane aligned", () => {
  const model = buildTaskGraph(taskGraphFixture());
  const base = layoutTaskGraph(model.nodes);
  const targetHeight = base.height + 150;
  const filled = layoutTaskGraph(model.nodes, undefined, {
    minHeight: targetHeight,
  });

  assert.equal(filled.height, targetHeight);
  assert.equal(filled.width, base.width, "height never changes canvas width");

  const baseGap =
    base.lanes[1]!.top - (base.lanes[0]!.top + base.lanes[0]!.height);
  const filledGap =
    filled.lanes[1]!.top - (filled.lanes[0]!.top + filled.lanes[0]!.height);
  assert.equal(baseGap, TASK_GRAPH_LANE_GAP);
  assert.equal(filledGap, TASK_GRAPH_LANE_GAP + 50);
  assert.ok(filledGap <= TASK_GRAPH_MAX_LANE_GAP);

  // The goal, its intent, and its fact remain centred in the same lane.
  const goalLane = filled.lanes[0]!;
  const goalCenter =
    filled.positions.get("goal:goal-1")!.y +
    filled.nodeHeights.get("goal:goal-1")! / 2;
  const intentCenter =
    filled.positions.get("intent:intent-1")!.y +
    filled.nodeHeights.get("intent:intent-1")! / 2;
  const factCenter =
    filled.positions.get("fact:fact-proof")!.y +
    filled.nodeHeights.get("fact:fact-proof")! / 2;
  assert.equal(goalCenter, goalLane.center);
  assert.equal(intentCenter, goalLane.center);
  assert.equal(factCenter, goalLane.center);

  // Origin stays on the full canvas centre, not a lane centre.
  assert.equal(
    filled.positions.get("fact:fact-origin")!.y +
      filled.nodeHeights.get("fact:fact-origin")! / 2,
    filled.height / 2,
  );
});

test("minHeight spreads row bands, then pads a single row", () => {
  const work = taskGraphFixture();
  work.goals = [work.goals[0]!];
  work.facts = work.facts.filter((fact) => fact.id !== "fact-orphan");
  work.results = work.results.filter(
    (result) => result.id !== "result-orphan",
  );
  const model = buildTaskGraph(work);
  const base = layoutTaskGraph(model.nodes);
  assert.equal(base.lanes.length, 1);

  // Two intents share the sole goal lane, so the extra height goes to its
  // row-band gap before anything reaches padding.
  const targetHeight = base.height + 20;
  const filled = layoutTaskGraph(model.nodes, undefined, {
    minHeight: targetHeight,
  });
  const rowA = filled.rows.find(
    (row) => row.intentId === "intent:intent-1",
  )!;
  const rowB = filled.rows.find(
    (row) => row.intentId === "intent:intent-2",
  )!;
  const baseRowA = base.rows.find(
    (row) => row.intentId === "intent:intent-1",
  )!;
  const baseRowB = base.rows.find(
    (row) => row.intentId === "intent:intent-2",
  )!;

  assert.equal(filled.height, targetHeight);
  assert.equal(filled.width, base.width);
  assert.equal(
    baseRowB.top - (baseRowA.top + baseRowA.height),
    TASK_GRAPH_ROW_BAND_GAP,
  );
  assert.equal(
    rowB.top - (rowA.top + rowA.height),
    TASK_GRAPH_ROW_BAND_GAP + 20,
  );
  assert.equal(filled.lanes[0]!.center, filled.height / 2);
  assert.equal(
    filled.positions.get("fact:fact-proof")!.y +
      filled.nodeHeights.get("fact:fact-proof")! / 2,
    rowA.center,
  );
  assert.equal(
    filled.positions.get("result:result-1")!.y +
      filled.nodeHeights.get("result:result-1")! / 2,
    rowA.center,
  );
  assert.equal(
    filled.positions.get("fact:fact-origin")!.y +
      filled.nodeHeights.get("fact:fact-origin")! / 2,
    filled.height / 2,
  );

  // A huge target caps the visual row gap; the rest becomes symmetric padding.
  const hugeTarget = base.height + 1_000;
  const huge = layoutTaskGraph(model.nodes, undefined, {
    minHeight: hugeTarget,
  });
  const hugeRowA = huge.rows.find(
    (row) => row.intentId === "intent:intent-1",
  )!;
  const hugeRowB = huge.rows.find(
    (row) => row.intentId === "intent:intent-2",
  )!;
  assert.equal(huge.height, hugeTarget);
  assert.equal(
    hugeRowB.top - (hugeRowA.top + hugeRowA.height),
    TASK_GRAPH_MAX_ROW_BAND_GAP,
  );

  // With one lane and one row there are no gaps to grow, so the whole extra
  // becomes padding above and below while the row stays centred.
  work.intents = [work.intents[0]!];
  const singleModel = buildTaskGraph(work);
  const singleBase = layoutTaskGraph(singleModel.nodes);
  const padded = layoutTaskGraph(singleModel.nodes, undefined, {
    minHeight: singleBase.height + 120,
  });
  const paddedLane = padded.lanes[0]!;
  assert.equal(padded.height, singleBase.height + 120);
  assert.equal(padded.width, singleBase.width);
  assert.equal(paddedLane.top, (padded.height - paddedLane.height) / 2);
  assert.equal(paddedLane.center, padded.height / 2);
  assert.equal(
    padded.positions.get("fact:fact-origin")!.y +
      padded.nodeHeights.get("fact:fact-origin")! / 2,
    padded.height / 2,
  );
});

test("buildTaskGraphFlow maps every node onto its fixed layout position", () => {
  const work = taskGraphFixture();
  const model = buildTaskGraph(work);
  const layout = layoutTaskGraph(model.nodes);
  const flow = buildTaskGraphFlow(model, layout);

  assert.equal(flow.nodes.length, model.nodes.length);
  assert.equal(flow.selected, null);
  assert.deepEqual(
    new Set(flow.nodes.map((node) => node.type)),
    new Set(["origin", "goal", "intent", "fact", "result"]),
  );

  for (const [index, source] of model.nodes.entries()) {
    const mapped = flow.nodes[index]!;
    assert.equal(mapped.id, source.id);
    assert.equal(mapped.type, source.kind);
    assert.deepEqual(mapped.position, layout.positions.get(source.id));
    assert.equal(mapped.data.kind, source.kind);
    assert.equal(mapped.data.kindLabel, TASK_GRAPH_KIND_LABELS[source.kind]);
    assert.equal(mapped.data.label, source.label);
    assert.equal(mapped.data.state, source.state);
    assert.equal(mapped.data.entityId, source.entityId);
    assert.equal(mapped.data.nodeId, source.id);
    assert.equal(mapped.draggable, false);
    assert.equal(mapped.selectable, true);
    // MiniMap needs the declared dimensions, not just the inline style.
    assert.equal(mapped.width, layout.nodeWidth);
    assert.equal(mapped.height, layout.nodeHeight);
    assert.equal(mapped.style?.width, layout.nodeWidth);
    assert.equal(mapped.style?.height, layout.nodeHeight);
  }
});

test("buildTaskGraphFlow keeps edges as labelled smoothstep arrows", () => {
  const work = taskGraphFixture();
  const model = buildTaskGraph(work);
  const layout = layoutTaskGraph(model.nodes);
  const flow = buildTaskGraphFlow(model, layout);

  assert.equal(flow.edges.length, model.edges.length);
  assert.deepEqual(
    new Set(flow.edges.map((edge) => edge.type)),
    new Set(["smoothstep", TASK_GRAPH_BACKWARD_EDGE_TYPE]),
  );
  for (const [index, source] of model.edges.entries()) {
    const mapped = flow.edges[index]!;
    assert.equal(mapped.id, source.id);
    assert.equal(mapped.source, source.from);
    assert.equal(mapped.target, source.to);
    assert.equal(mapped.label, TASK_GRAPH_RELATION_LABELS[source.relation]);
    assert.equal(mapped.data?.relation, source.relation);
    assert.equal(
      mapped.data?.relationLabel,
      TASK_GRAPH_RELATION_LABELS[source.relation],
    );
    const marker = mapped.markerEnd as { type?: string };
    assert.equal(marker.type, "arrowclosed");
  }

  // Backward fact → origin uses the top corridor, above every node.
  const backward = flow.edges.find(
    (edge) => edge.id === "derived_from:fact:fact-proof->fact:fact-origin",
  );
  assert.ok(backward !== undefined);
  assert.equal(backward.type, TASK_GRAPH_BACKWARD_EDGE_TYPE);
  assert.match(backward.data?.path ?? "", /^M /);
  assert.ok((backward.data?.labelY ?? Number.POSITIVE_INFINITY) < 80);
  assert.equal(typeof backward.data?.labelX, "number");

  // Wire names are spelled for the inspector/edge labels, without underscores.
  assert.equal(TASK_GRAPH_RELATION_LABELS.has_goal, "has goal");
  assert.equal(TASK_GRAPH_RELATION_LABELS.spawns, "spawns");
  assert.equal(TASK_GRAPH_RELATION_LABELS.yields, "yields");
  assert.equal(TASK_GRAPH_RELATION_LABELS.derived_from, "derived from");
  assert.ok(
    flow.edges.every(
      (edge) => typeof edge.label === "string" && !edge.label.includes("_"),
    ),
  );
});

test("flow edges pick handles that route around the goal and fact columns", () => {
  const work = taskGraphFixture();
  work.facts.push({
    ...work.facts[1]!,
    id: "fact-derived",
    claim: "A fact derived from the proof fact",
    origin_of: "fact-proof",
  });
  const model = buildTaskGraph(work);
  const flow = buildTaskGraphFlow(model, layoutTaskGraph(model.nodes));
  const edge = (id: string) => flow.edges.find((entry) => entry.id === id);

  // Ordinary forward edges connect right to left.
  assert.deepEqual(
    {
      source: edge(
        "has_goal:fact:fact-origin->goal:goal-1",
      )?.sourceHandle,
      target: edge(
        "has_goal:fact:fact-origin->goal:goal-1",
      )?.targetHandle,
    },
    { source: "out-right", target: "in-left" },
  );
  // A goal-less intent is fed from the origin bottom so the wire clears the
  // goal card instead of disappearing behind it.
  assert.deepEqual(
    {
      source: edge(
        "spawns:fact:fact-origin->intent:intent-2",
      )?.sourceHandle,
      target: edge(
        "spawns:fact:fact-origin->intent:intent-2",
      )?.targetHandle,
    },
    { source: "out-bottom", target: "in-left" },
  );
  // Backward fact → origin ignores the handles and renders the top corridor;
  // same-column fact → fact loops through the fact/result gap.
  const backwardEdge = edge(
    "derived_from:fact:fact-proof->fact:fact-origin",
  );
  assert.equal(backwardEdge?.type, TASK_GRAPH_BACKWARD_EDGE_TYPE);
  assert.equal(backwardEdge?.data?.path?.includes("M ") ?? false, true);
  assert.deepEqual(
    {
      source: edge(
        "derived_from:fact:fact-derived->fact:fact-proof",
      )?.sourceHandle,
      target: edge(
        "derived_from:fact:fact-derived->fact:fact-proof",
      )?.targetHandle,
    },
    { source: "out-right", target: "in-right" },
  );
});

test("selected task graph node lookup marks exactly one mapped node", () => {
  const work = taskGraphFixture();
  const model = buildTaskGraph(work);
  const layout = layoutTaskGraph(model.nodes);
  const selectedId = "fact:fact-proof";

  const flow = buildTaskGraphFlow(model, layout, selectedId, work);
  assert.equal(flow.selected?.id, selectedId);
  assert.equal(flow.selected?.data.selected, true);
  assert.equal(
    flow.nodes.filter((node) => node.data.selected).length,
    1,
  );
  assert.equal(
    findTaskGraphFlowNode(flow.nodes, selectedId)?.id,
    selectedId,
  );
  assert.equal(findTaskGraphFlowNode(flow.nodes, "fact:missing"), null);
  assert.equal(findTaskGraphFlowNode(flow.nodes, null), null);

  // A stale selection never smears selection flags onto another node.
  const stale = buildTaskGraphFlow(model, layout, "fact:missing");
  assert.equal(stale.selected, null);
  assert.ok(stale.nodes.every((node) => !node.data.selected));
});

test("mapped fact nodes resolve shortened labels from the work snapshot", () => {
  const work = taskGraphFixture();
  const longClaim =
    "The forcing line survives every legal king retreat, including the quiet waiting move and the long diagonal escape";
  const sourceFact = work.facts.find((fact) => fact.id === "fact-proof")!;
  sourceFact.claim = longClaim;

  const model = buildTaskGraph(work);
  const factNode = model.nodes.find((node) => node.id === "fact:fact-proof");
  assert.ok(factNode !== undefined);

  // A shortened card label must not leak into the inspector text.
  factNode.label = "The forcing line…";
  assert.equal(taskGraphNodeText(factNode, work), longClaim);
  assert.equal(taskGraphNodeText(factNode, null), "The forcing line…");

  const flow = buildTaskGraphFlow(
    model,
    layoutTaskGraph(model.nodes),
    "fact:fact-proof",
    work,
  );
  assert.equal(flow.selected?.data.label, longClaim);
  assert.equal(flow.selected?.ariaLabel, `Fact: ${longClaim}`);
});

test("paginateTasks produces five-row pages with clamped bounds", () => {
  assert.equal(TASK_PAGE_SIZE, 5);
  const roster = Array.from({ length: 47 }, (_, index) => `task-${index + 1}`);

  const first = paginateTasks(roster, 1);
  assert.deepEqual(first.items, [
    "task-1",
    "task-2",
    "task-3",
    "task-4",
    "task-5",
  ]);
  assert.equal(first.page, 1);
  assert.equal(first.pageCount, 10);
  assert.equal(first.total, 47);
  assert.equal(first.start, 1);
  assert.equal(first.end, 5);

  const last = paginateTasks(roster, 10);
  assert.deepEqual(last.items, ["task-46", "task-47"]);
  assert.equal(last.start, 46);
  assert.equal(last.end, 47);

  // Stale/out-of-range page state clamps instead of showing an empty page.
  assert.equal(paginateTasks(roster, 0).page, 1);
  assert.equal(paginateTasks(roster, 99).page, 10);
  assert.deepEqual(paginateTasks(roster, 99).items, last.items);

  const empty = paginateTasks([], 3);
  assert.deepEqual(empty.items, []);
  assert.equal(empty.page, 1);
  assert.equal(empty.pageCount, 1);
  assert.equal(empty.start, 0);
  assert.equal(empty.end, 0);
});

test("search spans the whole roster; missing selections pick the page's first task", () => {
  const base = taskGraphFixture().task;
  const roster: Task[] = Array.from({ length: 47 }, (_, index) => ({
    ...base,
    id: `task-${index + 1}`,
    goal:
      index === 46
        ? "the needle is only on the last page"
        : `goal number ${index + 1}`,
  }));

  const filtered = filterTasksByGoal(roster, "  NEEDLE ");
  assert.deepEqual(
    filtered.map((task) => task.id),
    ["task-47"],
  );
  const searchedPage = paginateTasks(filtered, 1);
  assert.equal(searchedPage.start, 1);
  assert.equal(searchedPage.end, 1);
  assert.equal(searchedPage.pageCount, 1);

  const firstPage = paginateTasks(roster, 1);
  assert.equal(
    selectVisibleTaskId(firstPage.items, "task-3"),
    "task-3",
  );
  // A selection from another page (or a search result) falls back to row one.
  assert.equal(
    selectVisibleTaskId(firstPage.items, "task-47"),
    "task-1",
  );
  assert.equal(selectVisibleTaskId(firstPage.items, null), "task-1");
  assert.equal(selectVisibleTaskId([], "task-1"), null);
});
