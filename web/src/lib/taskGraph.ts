/**
 * Pure task-work graph derivation and layout for the task graph page.
 *
 * Source: `GET /api/tasks/{id}/work` (`TaskWork`). The graph is a strict
 * projection of the work records the server returns — nothing is inferred
 * from activity or any other source. Every helper here is plain data
 * logic (no React, no DOM, no `fetch`) so the unit tests can exercise it
 * directly under `node --test`.
 *
 * The origin fact is the single root. There is deliberately no separate task
 * node: a task is the container that owns one origin, and the graph starts
 * where the work starts. Node identity is `"<kind>:<uuid>"`, which keeps the
 * five work collections disjoint even if a hostile payload repeats an id.
 * Edges are emitted only when BOTH endpoints exist in the derived node set:
 * a fact claiming a missing intent, or a result pointing at a missing intent,
 * is silently ignored (an orphan edge is not a graph edge).
 *
 * "Proves" is a marker, not a relation. A goal records the proof fact id it
 * carries, and a fact records whether any goal in the snapshot names it as
 * proof. No `proves` edge is emitted, so it never inflates the edge count.
 *
 * Layout is pure as well: callers may pass measured card heights, and the lane
 * and row geometry grows from them without reading the DOM here.
 */

import type {
  Fact,
  Goal,
  Intent,
  ResultRecord,
  Task,
  TaskWork,
} from "./types.ts";

// ---------------------------------------------------------------------------
// Graph shapes
// ---------------------------------------------------------------------------

/** The five visual node kinds: origin root, then work records. */
export type TaskGraphNodeKind =
  | "origin"
  | "goal"
  | "intent"
  | "fact"
  | "result";

/** The four relations the linear task graph draws. */
export type TaskGraphRelation =
  | "has_goal"
  | "spawns"
  | "yields"
  | "derived_from";

type TaskGraphNodeBase = {
  /** Namespaced node id (`origin:<uuid>`, `fact:<uuid>`, …). */
  id: string;
  /** Raw entity id from the work payload. */
  entityId: string;
  /** Full, untruncated text — cards wrap it naturally, never clamp it. */
  label: string;
  /** The relevant state field (goal state, result acceptance…). */
  state: string;
  createdAt: string;
};

export type TaskGraphNode =
  | (TaskGraphNodeBase & {
      kind: "origin";
      fact: Fact;
      /** True when a goal in this snapshot names this fact as proof. */
      provesGoal: boolean;
    })
  | (TaskGraphNodeBase & {
      kind: "goal";
      goal: Goal;
      /** The goal's recorded proof fact id, or `null` while unproven. */
      proofFactId: string | null;
    })
  | (TaskGraphNodeBase & { kind: "intent"; intent: Intent })
  | (TaskGraphNodeBase & {
      kind: "fact";
      fact: Fact;
      /** True when a goal in this snapshot names this fact as proof. */
      provesGoal: boolean;
    })
  | (TaskGraphNodeBase & { kind: "result"; result: ResultRecord });

export type TaskGraphEdge = {
  /** Deterministic id: `<relation>:<from>-><to>`. */
  id: string;
  from: string;
  to: string;
  relation: TaskGraphRelation;
};

export type TaskGraphModel = {
  nodes: TaskGraphNode[];
  edges: TaskGraphEdge[];
};

/** Fixed column order: origin → goals → intents → facts → results. */
export type TaskGraphColumn = 0 | 1 | 2 | 3 | 4;

export const TASK_GRAPH_COLUMN_COUNT = 5;

export const TASK_GRAPH_KIND_LABELS: Readonly<
  Record<TaskGraphNodeKind, string>
> = {
  origin: "Origin",
  goal: "Goal",
  intent: "Intent",
  fact: "Fact",
  result: "Result",
};

/** Left-to-right column for a node kind. */
export function taskGraphColumn(kind: TaskGraphNodeKind): TaskGraphColumn {
  switch (kind) {
    case "origin":
      return 0;
    case "goal":
      return 1;
    case "intent":
      return 2;
    case "fact":
      return 3;
    case "result":
      return 4;
  }
}

// ---------------------------------------------------------------------------
// Derivation
// ---------------------------------------------------------------------------

function nonEmpty(value: string, fallback: string): string {
  const trimmed = value.trim();
  return trimmed === "" ? fallback : trimmed;
}

function provenanceString(fact: Fact, key: string): string | null {
  const value = fact.provenance[key];
  return typeof value === "string" && value !== "" ? value : null;
}

/**
 * Builds nodes and edges from one task-work snapshot.
 *
 * Node sets are deduplicated by namespaced id (first row wins) and edge sets
 * by `(from, to, relation)`, so repeated/hostile rows can never draw the same
 * connection twice. The origin is the single root; no `task` node exists.
 */
export function buildTaskGraph(work: TaskWork): TaskGraphModel {
  const task = work.task;
  const goals = work.goals ?? [];
  const intents = work.intents ?? [];
  const facts = work.facts ?? [];
  const results = work.results ?? [];

  const goalById = new Map(goals.map((goal) => [goal.id, goal]));
  const intentById = new Map(intents.map((intent) => [intent.id, intent]));
  const factById = new Map(facts.map((fact) => [fact.id, fact]));

  // One origin, even if a hostile snapshot repeats/aliases origin facts.
  const originFact =
    facts.find((fact) => fact.id === task.origin_fact_id) ??
    facts.find((fact) => fact.state === "origin") ??
    null;
  const originFactId = originFact?.id ?? null;

  // A goal's proof_fact_id is a marker on both ends; it never becomes an edge.
  const proofFactIds = new Set(
    goals
      .map((goal) => goal.proof_fact_id)
      .filter((id): id is string => id !== null),
  );

  const nodes: TaskGraphNode[] = [];
  const nodeIds = new Set<string>();
  const addNode = (node: TaskGraphNode): void => {
    if (nodeIds.has(node.id)) {
      return;
    }
    nodeIds.add(node.id);
    nodes.push(node);
  };

  // 1. Origin root.
  if (originFact !== null) {
    addNode({
      id: `fact:${originFact.id}`,
      entityId: originFact.id,
      kind: "origin",
      label: nonEmpty(originFact.claim, "Origin fact"),
      state: originFact.state,
      createdAt: originFact.created_at,
      fact: originFact,
      provesGoal: proofFactIds.has(originFact.id),
    });
  }

  // 2. Goals.
  for (const goal of goals) {
    addNode({
      id: `goal:${goal.id}`,
      entityId: goal.id,
      kind: "goal",
      label: nonEmpty(goal.description, "Untitled goal"),
      state: goal.state,
      createdAt: goal.created_at,
      goal,
      proofFactId: goal.proof_fact_id,
    });
  }

  // 3. Intents.
  for (const intent of intents) {
    addNode({
      id: `intent:${intent.id}`,
      entityId: intent.id,
      kind: "intent",
      label: nonEmpty(intent.description, "Untitled intent"),
      state: intent.state,
      createdAt: intent.created_at,
      intent,
    });
  }

  // 4. Non-origin facts. Extra origin-state facts stay visible as facts so a
  // hostile snapshot cannot silently drop work; only one node is the root.
  for (const fact of facts) {
    if (fact.id === originFactId) {
      continue;
    }
    addNode({
      id: `fact:${fact.id}`,
      entityId: fact.id,
      kind: "fact",
      label: nonEmpty(fact.claim, "Untitled fact"),
      state: fact.state,
      createdAt: fact.created_at,
      fact,
      provesGoal: proofFactIds.has(fact.id),
    });
  }

  // 5. Results.
  for (const result of results) {
    addNode({
      id: `result:${result.id}`,
      entityId: result.id,
      kind: "result",
      label: nonEmpty(result.claim, "Untitled result"),
      state: result.acceptance,
      createdAt: result.created_at,
      result,
    });
  }

  // --- Edges ---------------------------------------------------------------
  const edges: TaskGraphEdge[] = [];
  const edgeKeys = new Set<string>();
  const addEdge = (
    from: string,
    to: string,
    relation: TaskGraphRelation,
  ): void => {
    if (from === to) {
      return; // A self-loop carries no information in this graph.
    }
    const key = `${from}\u0000${to}\u0000${relation}`;
    if (edgeKeys.has(key)) {
      return;
    }
    edgeKeys.add(key);
    edges.push({ id: `${relation}:${from}->${to}`, from, to, relation });
  };

  const originNodeId = originFact === null ? null : `fact:${originFact.id}`;

  // origin → goal (has_goal)
  if (originNodeId !== null) {
    for (const goal of goals) {
      addEdge(originNodeId, `goal:${goal.id}`, "has_goal");
    }
  }

  // goal → intent (spawns), or origin → intent when the intent has no goal.
  for (const intent of intents) {
    const intentNodeId = `intent:${intent.id}`;
    if (intent.goal_id === null) {
      if (originNodeId !== null) {
        addEdge(originNodeId, intentNodeId, "spawns");
      }
    } else if (goalById.has(intent.goal_id)) {
      addEdge(`goal:${intent.goal_id}`, intentNodeId, "spawns");
    }
  }

  // intent → fact (yields) from provenance.intent_id, and
  // fact → fact (derived_from) from fact.origin_of.
  for (const fact of facts) {
    const factNodeId = `fact:${fact.id}`;
    const intentId = provenanceString(fact, "intent_id");
    if (intentId !== null && intentById.has(intentId)) {
      addEdge(`intent:${intentId}`, factNodeId, "yields");
    }
    if (
      fact.origin_of !== null &&
      fact.origin_of !== fact.id &&
      factById.has(fact.origin_of)
    ) {
      addEdge(factNodeId, `fact:${fact.origin_of}`, "derived_from");
    }
  }

  // intent → result (yields) from result.intent_id.
  for (const result of results) {
    if (intentById.has(result.intent_id)) {
      addEdge(`intent:${result.intent_id}`, `result:${result.id}`, "yields");
    }
  }

  return { nodes, edges };
}

// ---------------------------------------------------------------------------
// Task list pagination
// ---------------------------------------------------------------------------

/** Five task rows per page, matching the approved Graphs reference. */
export const TASK_PAGE_SIZE = 5;

export type TaskPage<T> = {
  items: T[];
  /** Clamped 1-based page number. */
  page: number;
  pageCount: number;
  total: number;
  /** 1-based inclusive range; both are 0 for an empty roster. */
  start: number;
  end: number;
};

/**
 * Slices one page from a task roster. Page numbers are clamped into range so
 * a delayed poll that shrinks the roster (or a stale page state) can never
 * produce an empty page while tasks still exist.
 */
export function paginateTasks<T>(
  items: readonly T[],
  page: number,
): TaskPage<T> {
  const total = items.length;
  const pageCount = Math.max(1, Math.ceil(total / TASK_PAGE_SIZE));
  const requested = Number.isFinite(page) ? Math.trunc(page) : 1;
  const current = Math.min(pageCount, Math.max(1, requested));
  const offset = (current - 1) * TASK_PAGE_SIZE;
  const pageItems = items.slice(offset, offset + TASK_PAGE_SIZE);
  return {
    items: pageItems,
    page: current,
    pageCount,
    total,
    start: pageItems.length === 0 ? 0 : offset + 1,
    end: offset + pageItems.length,
  };
}

/** Search runs over the whole roster, never just the current page. */
export function filterTasksByGoal(
  tasks: readonly Task[],
  search: string,
): Task[] {
  const query = search.trim().toLowerCase();
  if (query === "") {
    return [...tasks];
  }
  return tasks.filter((task) => task.goal.toLowerCase().includes(query));
}

/**
 * Keeps a valid selection, otherwise selects the first task on the page.
 * Used after search, pagination, and roster refreshes.
 */
export function selectVisibleTaskId(
  visibleTasks: readonly Task[],
  selectedId: string | null,
): string | null {
  if (
    selectedId !== null &&
    visibleTasks.some((task) => task.id === selectedId)
  ) {
    return selectedId;
  }
  return visibleTasks[0]?.id ?? null;
}

// ---------------------------------------------------------------------------
// Layout (five fixed columns, one horizontal band per goal branch)
// ---------------------------------------------------------------------------

/** Five 200px columns fit the full-width graph stage at the fitted scale. */
export const TASK_GRAPH_NODE_WIDTH = 200;
/** Fixed card height keeps every lane and the minimap dimensions stable. */
export const TASK_GRAPH_NODE_HEIGHT = 172;
/** Compact gap keeps all five columns inside the viewport at fit scale. */
export const TASK_GRAPH_COLUMN_GAP = 56;
export const TASK_GRAPH_ROW_GAP = 16;
export const TASK_GRAPH_ROW_BAND_GAP = 32;
export const TASK_GRAPH_LANE_GAP = 56;
export const TASK_GRAPH_PADDING = 20;

/**
 * Gaps stop growing here when `minHeight` would otherwise stretch a sparse
 * graph into a poster. Any remaining height becomes canvas padding instead.
 */
export const TASK_GRAPH_MAX_LANE_GAP = 160;
export const TASK_GRAPH_MAX_ROW_BAND_GAP = 96;

export type TaskGraphPoint = { x: number; y: number };

/**
 * Measured border-box heights keyed by namespaced node id. The layout falls
 * back to `TASK_GRAPH_NODE_HEIGHT` for missing entries.
 */
export type TaskGraphNodeHeights = ReadonlyMap<string, number>;

export type TaskGraphLaneKind = "goal" | "root" | "loose";

/** One horizontal band: a goal branch, the goal-less root lane, or leftovers. */
export type TaskGraphLane = {
  id: string;
  kind: TaskGraphLaneKind;
  top: number;
  height: number;
  center: number;
};

/** One intent row inside a lane. Results/facts align to this band. */
export type TaskGraphRowBand = {
  laneId: string;
  intentId: string | null;
  top: number;
  height: number;
  center: number;
};

export type TaskGraphLayout = {
  positions: Map<string, TaskGraphPoint>;
  lanes: TaskGraphLane[];
  rows: TaskGraphRowBand[];
  width: number;
  height: number;
  nodeWidth: number;
  nodeHeight: number;
  /** Border-box height used for every placed node (measured or fallback). */
  nodeHeights: ReadonlyMap<string, number>;
  /** X coordinate of each of the five fixed column centres. */
  columnCenters: readonly number[];
};

export type TaskGraphLayoutOptions = {
  /**
   * Target canvas height. Heights above the natural layout are distributed
   * across lane gaps, then row-band gaps, then canvas padding.
   */
  minHeight?: number;
};

type IntentNode = Extract<TaskGraphNode, { kind: "intent" }>;
type GoalNode = Extract<TaskGraphNode, { kind: "goal" }>;
type FactNode = Extract<TaskGraphNode, { kind: "fact" }>;
type ResultNode = Extract<TaskGraphNode, { kind: "result" }>;

type RowDraft = {
  intent: IntentNode | null;
  facts: FactNode[];
  results: ResultNode[];
  height: number;
};

type LaneDraft = {
  id: string;
  kind: TaskGraphLaneKind;
  goal: GoalNode | null;
  rows: RowDraft[];
};

function nodeHeightOf(
  node: TaskGraphNode,
  measuredHeights: TaskGraphNodeHeights | undefined,
): number {
  const value = measuredHeights?.get(node.id);
  if (value === undefined || !Number.isFinite(value) || value <= 0) {
    return TASK_GRAPH_NODE_HEIGHT;
  }
  return Math.max(1, value);
}

function stackHeight(
  stack: readonly TaskGraphNode[],
  measuredHeights: TaskGraphNodeHeights | undefined,
): number {
  if (stack.length === 0) {
    return 0;
  }
  return (
    stack.reduce(
      (sum, node) => sum + nodeHeightOf(node, measuredHeights),
      0,
    ) +
    (stack.length - 1) * TASK_GRAPH_ROW_GAP
  );
}

/**
 * Places nodes into five fixed columns and horizontal lanes.
 *
 * Goals define the lanes. Each goal sits at its lane centre; its intents stack
 * vertically as rows, and the facts/results produced by an intent stack around
 * that row's centre. Goal-less intents share the root lane (or the sole goal
 * lane), and unlinked facts or results collect in a final loose lane. The
 * origin is a single node centred across all lanes, so the whole derivation
 * reads left to right without a task node.
 *
 * `measuredHeights` lets the component feed back real DOM card heights. Every
 * row, stack, lane, and the canvas itself grows from those measurements; the
 * fallback height only applies until the first ResizeObserver pass.
 *
 * `options.minHeight` lets the component fill a taller stage. Extra space is
 * distributed deterministically: lane gaps first, then row-band gaps, then
 * canvas padding above and below. Gaps are capped so a sparse graph never
 * becomes a poster; the leftover always returns to padding. Lane and row
 * centres stay intact, and the canvas width never changes.
 */
export function layoutTaskGraph(
  nodes: readonly TaskGraphNode[],
  measuredHeights?: TaskGraphNodeHeights,
  options?: TaskGraphLayoutOptions,
): TaskGraphLayout {
  const heightOf = (node: TaskGraphNode): number =>
    nodeHeightOf(node, measuredHeights);
  const nodeHeights = new Map<string, number>();
  for (const node of nodes) {
    nodeHeights.set(node.id, heightOf(node));
  }

  const originNode =
    nodes.find((node) => node.kind === "origin") ?? null;
  const goalNodes = nodes.filter(
    (node): node is GoalNode => node.kind === "goal",
  );
  const intentNodes = nodes.filter(
    (node): node is IntentNode => node.kind === "intent",
  );
  const factNodes = nodes.filter(
    (node): node is FactNode => node.kind === "fact",
  );
  const resultNodes = nodes.filter(
    (node): node is ResultNode => node.kind === "result",
  );

  const intentById = new Map(
    intentNodes.map((intent) => [intent.entityId, intent]),
  );

  const factsByIntent = new Map<string, FactNode[]>();
  const looseFacts: FactNode[] = [];
  for (const fact of factNodes) {
    const intentId = provenanceString(fact.fact, "intent_id");
    const intent = intentId === null ? undefined : intentById.get(intentId);
    if (intent === undefined) {
      looseFacts.push(fact);
      continue;
    }
    const bucket = factsByIntent.get(intent.entityId) ?? [];
    bucket.push(fact);
    factsByIntent.set(intent.entityId, bucket);
  }

  const resultsByIntent = new Map<string, ResultNode[]>();
  const looseResults: ResultNode[] = [];
  for (const result of resultNodes) {
    const intent = intentById.get(result.result.intent_id);
    if (intent === undefined) {
      looseResults.push(result);
      continue;
    }
    const bucket = resultsByIntent.get(intent.entityId) ?? [];
    bucket.push(result);
    resultsByIntent.set(intent.entityId, bucket);
  }

  const laneByGoalId = new Map<string, LaneDraft>();
  const lanes: LaneDraft[] = goalNodes.map((goal) => {
    const lane: LaneDraft = {
      id: `goal:${goal.entityId}`,
      kind: "goal",
      goal,
      rows: [],
    };
    laneByGoalId.set(goal.entityId, lane);
    return lane;
  });

  let rootLane: LaneDraft | null = null;
  // One goal plus goal-less intents is the common DHI shape. Keep those
  // intents in the goal lane so origin, goal, intent, and fact form one line;
  // the real origin→intent edge remains unchanged.
  const soleGoalLane = goalNodes.length === 1 ? lanes[0] : undefined;
  for (const intent of intentNodes) {
    const goalLane =
      intent.intent.goal_id === null
        ? soleGoalLane
        : laneByGoalId.get(intent.intent.goal_id);
    const row: RowDraft = {
      intent,
      facts: factsByIntent.get(intent.entityId) ?? [],
      results: resultsByIntent.get(intent.entityId) ?? [],
      height: 0,
    };
    if (goalLane !== undefined) {
      goalLane.rows.push(row);
      continue;
    }
    if (rootLane === null) {
      rootLane = { id: "root", kind: "root", goal: null, rows: [] };
    }
    rootLane.rows.push(row);
  }
  if (rootLane !== null) {
    lanes.push(rootLane);
  }

  if (looseFacts.length > 0 || looseResults.length > 0) {
    lanes.push({
      id: "loose",
      kind: "loose",
      goal: null,
      rows: [
        {
          intent: null,
          facts: looseFacts,
          results: looseResults,
          height: 0,
        },
      ],
    });
  }

  for (const lane of lanes) {
    for (const row of lane.rows) {
      row.height = Math.max(
        row.intent === null ? 0 : heightOf(row.intent),
        stackHeight(row.facts, measuredHeights),
        stackHeight(row.results, measuredHeights),
      );
    }
  }

  const laneHeightOf = (lane: LaneDraft, rowBandGap: number): number => {
    const rowsHeight =
      lane.rows.length === 0
        ? 0
        : lane.rows.reduce((sum, row) => sum + row.height, 0) +
          (lane.rows.length - 1) * rowBandGap;
    return Math.max(
      rowsHeight,
      lane.goal === null ? 0 : heightOf(lane.goal),
      TASK_GRAPH_NODE_HEIGHT,
    );
  };
  const laneHeights = lanes.map((lane) =>
    laneHeightOf(lane, TASK_GRAPH_ROW_BAND_GAP),
  );
  const naturalLanesHeight =
    laneHeights.reduce((sum, height) => sum + height, 0) +
    Math.max(0, lanes.length - 1) * TASK_GRAPH_LANE_GAP;
  const naturalContentHeight = Math.max(
    TASK_GRAPH_NODE_HEIGHT,
    naturalLanesHeight,
    originNode === null ? 0 : heightOf(originNode),
  );
  const naturalHeight = naturalContentHeight + TASK_GRAPH_PADDING * 2;

  const requestedMinHeight = options?.minHeight;
  const targetHeight =
    requestedMinHeight !== undefined &&
    Number.isFinite(requestedMinHeight) &&
    requestedMinHeight > naturalHeight
      ? requestedMinHeight
      : naturalHeight;

  // Extra height follows one deterministic priority: lane gaps, then row
  // bands, then padding. Each gap has a ceiling, so a two-card graph stays a
  // graph instead of becoming two distant islands.
  const laneGapCount = Math.max(0, lanes.length - 1);
  const rowGapCount = lanes.reduce(
    (sum, lane) => sum + Math.max(0, lane.rows.length - 1),
    0,
  );
  let remainingExtra = targetHeight - naturalHeight;
  let laneGapExtra = 0;
  if (laneGapCount > 0) {
    const maxExtra = Math.max(
      0,
      TASK_GRAPH_MAX_LANE_GAP - TASK_GRAPH_LANE_GAP,
    );
    laneGapExtra = Math.min(remainingExtra / laneGapCount, maxExtra);
    remainingExtra -= laneGapExtra * laneGapCount;
  }
  let rowBandGapExtra = 0;
  if (rowGapCount > 0) {
    const maxExtra = Math.max(
      0,
      TASK_GRAPH_MAX_ROW_BAND_GAP - TASK_GRAPH_ROW_BAND_GAP,
    );
    rowBandGapExtra = Math.min(remainingExtra / rowGapCount, maxExtra);
  }
  const laneGap = TASK_GRAPH_LANE_GAP + laneGapExtra;
  const rowBandGap = TASK_GRAPH_ROW_BAND_GAP + rowBandGapExtra;

  const effectiveLaneHeights = lanes.map((lane) =>
    laneHeightOf(lane, rowBandGap),
  );
  const lanesHeight =
    effectiveLaneHeights.reduce((sum, height) => sum + height, 0) +
    laneGapCount * laneGap;
  const contentHeight = Math.max(
    naturalContentHeight,
    lanesHeight,
    originNode === null ? 0 : heightOf(originNode),
  );
  const height = Math.max(
    targetHeight,
    contentHeight + TASK_GRAPH_PADDING * 2,
  );
  // Any allocated gap that could not actually grow (or any capped remainder)
  // becomes symmetric padding, keeping the whole drawing vertically centred.
  const verticalPadding = (height - contentHeight) / 2;

  const columnX = (column: number): number =>
    TASK_GRAPH_PADDING +
    column * (TASK_GRAPH_NODE_WIDTH + TASK_GRAPH_COLUMN_GAP);
  const columnCenters = Array.from(
    { length: TASK_GRAPH_COLUMN_COUNT },
    (_, column) => columnX(column) + TASK_GRAPH_NODE_WIDTH / 2,
  );
  const width =
    TASK_GRAPH_COLUMN_COUNT * TASK_GRAPH_NODE_WIDTH +
    (TASK_GRAPH_COLUMN_COUNT - 1) * TASK_GRAPH_COLUMN_GAP +
    TASK_GRAPH_PADDING * 2;

  const positions = new Map<string, TaskGraphPoint>();
  const lanesOut: TaskGraphLane[] = [];
  const rowsOut: TaskGraphRowBand[] = [];

  const laneTopStart =
    verticalPadding + (contentHeight - lanesHeight) / 2;
  let laneTop = laneTopStart;

  lanes.forEach((lane, laneIndex) => {
    const laneHeight =
      effectiveLaneHeights[laneIndex] ?? TASK_GRAPH_NODE_HEIGHT;
    const laneCenter = laneTop + laneHeight / 2;
    lanesOut.push({
      id: lane.id,
      kind: lane.kind,
      top: laneTop,
      height: laneHeight,
      center: laneCenter,
    });

    if (lane.goal !== null) {
      positions.set(lane.goal.id, {
        x: columnX(1),
        y: laneCenter - heightOf(lane.goal) / 2,
      });
    }

    const rowsHeight =
      lane.rows.length === 0
        ? 0
        : lane.rows.reduce((sum, row) => sum + row.height, 0) +
          (lane.rows.length - 1) * rowBandGap;
    let rowTop = laneTop + (laneHeight - rowsHeight) / 2;
    for (const row of lane.rows) {
      const rowCenter = rowTop + row.height / 2;
      rowsOut.push({
        laneId: lane.id,
        intentId: row.intent?.id ?? null,
        top: rowTop,
        height: row.height,
        center: rowCenter,
      });

      if (row.intent !== null) {
        const intentHeight = heightOf(row.intent);
        positions.set(row.intent.id, {
          x: columnX(2),
          y: rowCenter - intentHeight / 2,
        });
      }

      const factsHeight = stackHeight(row.facts, measuredHeights);
      let factY = rowCenter - factsHeight / 2;
      for (const fact of row.facts) {
        positions.set(fact.id, { x: columnX(3), y: factY });
        factY += heightOf(fact) + TASK_GRAPH_ROW_GAP;
      }

      const resultsHeight = stackHeight(row.results, measuredHeights);
      let resultY = rowCenter - resultsHeight / 2;
      for (const result of row.results) {
        positions.set(result.id, { x: columnX(4), y: resultY });
        resultY += heightOf(result) + TASK_GRAPH_ROW_GAP;
      }

      rowTop += row.height + rowBandGap;
    }

    laneTop += laneHeight + laneGap;
  });

  if (originNode !== null) {
    positions.set(originNode.id, {
      x: columnX(0),
      y: height / 2 - heightOf(originNode) / 2,
    });
  }

  return {
    positions,
    lanes: lanesOut,
    rows: rowsOut,
    width,
    height,
    nodeWidth: TASK_GRAPH_NODE_WIDTH,
    nodeHeight: TASK_GRAPH_NODE_HEIGHT,
    nodeHeights,
    columnCenters,
  };
}
