/**
 * Pure mapper from the task-work graph model to React Flow nodes and edges.
 *
 * `taskGraph.ts` owns the graph semantics (which nodes/edges exist) and the
 * deterministic five-column layout. This module adds only the presentation
 * fields React Flow needs: the node type per kind, the resolved full text,
 * relation labels, arrowheads, and selection flags. It has no React or DOM
 * dependency, so `taskGraph.test.ts` can exercise it under `node --test`.
 */

import type { Edge, MarkerType, Node, Position } from "@xyflow/react";

import {
  TASK_GRAPH_COLUMN_GAP,
  TASK_GRAPH_KIND_LABELS,
  TASK_GRAPH_PADDING,
  type TaskGraphLayout,
  type TaskGraphModel,
  type TaskGraphNode,
  type TaskGraphNodeKind,
  type TaskGraphRelation,
} from "./taskGraph.ts";
import type { TaskWork } from "./types.ts";

/** Neutral wires keep the five node colours and the proves mark dominant. */
export const TASK_GRAPH_FLOW_EDGE_COLOR = "var(--muted-foreground)";
export const TASK_GRAPH_FLOW_EDGE_LABEL_COLOR = "var(--muted-foreground)";
export const TASK_GRAPH_FLOW_EDGE_LABEL_BG = "var(--surface-subtle)";

/** Relation labels are the lowercase wire names with spaces. */
export const TASK_GRAPH_RELATION_LABELS: Readonly<
  Record<TaskGraphRelation, string>
> = {
  has_goal: "has goal",
  spawns: "spawns",
  yields: "yields",
  derived_from: "derived from",
};

/** Data attached to every React Flow node. Plain JSON so it stays testable. */
export type TaskGraphFlowNodeData = {
  kind: TaskGraphNodeKind;
  kindLabel: string;
  /** Full, untruncated text (fact claims are resolved from `work.facts`). */
  label: string;
  state: string;
  /** Raw work-record id, without the graph's kind namespace. */
  entityId: string;
  /** Namespaced graph id (`fact:<uuid>`, `goal:<uuid>`, …). */
  nodeId: string;
  /** True when a goal names this fact as proof, or a goal has proof. */
  proves: boolean;
  selected: boolean;
  [key: string]: unknown;
};

export type TaskGraphFlowNode = Node<TaskGraphFlowNodeData, TaskGraphNodeKind>;

/** Backward `derived_from` edges use the explicit top-corridor path. */
export const TASK_GRAPH_BACKWARD_EDGE_TYPE = "taskGraphBackward";

export type TaskGraphFlowEdgeData = {
  relation: TaskGraphRelation;
  relationLabel: string;
  /** Absolute flow-space path for custom edges. */
  path?: string;
  labelX?: number;
  labelY?: number;
  [key: string]: unknown;
};

export type TaskGraphFlowEdge = Edge<
  TaskGraphFlowEdgeData,
  "smoothstep" | typeof TASK_GRAPH_BACKWARD_EDGE_TYPE
>;

export type TaskGraphFlowModel = {
  nodes: TaskGraphFlowNode[];
  edges: TaskGraphFlowEdge[];
  /** The selected node, or null when the id is absent/stale. */
  selected: TaskGraphFlowNode | null;
};

/** Mirrors the proves marker the cards and legend show. */
export function taskGraphNodeProves(node: TaskGraphNode): boolean {
  if (node.kind === "goal") {
    return node.proofFactId !== null;
  }
  if (node.kind === "fact" || node.kind === "origin") {
    return node.provesGoal;
  }
  return false;
}

/**
 * Resolves the full inspector text for one node.
 *
 * Fact cards can show a shortened claim, but the inspector must always show
 * the source record. Prefer `work.facts` for facts/origins so a shortened
 * label or an older snapshot still resolves to the authoritative claim.
 */
export function taskGraphNodeText(
  node: TaskGraphNode,
  work: TaskWork | null | undefined,
): string {
  if ((node.kind === "fact" || node.kind === "origin") && work !== null) {
    const fact = work?.facts?.find((entry) => entry.id === node.entityId);
    const claim = fact?.claim.trim();
    if (claim !== undefined && claim !== "") {
      return claim;
    }
  }
  return node.label;
}

/**
 * Picks the pair of handles whose route keeps the edge (and its label) out of
 * the cards. Ordinary forward edges use the right/left pair. Two shapes need
 * another corridor:
 *
 * - origin → goal-less intent crosses the goal column, so it leaves the
 *   origin bottom and runs below the goal row before entering the intent left;
 * - fact → fact (`derived_from`, same column) loops through the fact/result gap
 *   using the right side of both cards.
 *
 * Fact → origin `derived_from` uses `taskGraphBackwardEdgePath` instead, so its
 * handles are irrelevant; it keeps the ordinary pair.
 *
 * Positions are unchanged; only the attachment side changes.
 */
export function taskGraphFlowHandles(
  source: TaskGraphNode,
  target: TaskGraphNode,
  relation: TaskGraphRelation,
): { sourceHandle: string; targetHandle: string } {
  if (
    relation === "derived_from" &&
    source.kind === "fact" &&
    target.kind === "fact"
  ) {
    return { sourceHandle: "out-right", targetHandle: "in-right" };
  }
  if (relation === "spawns" && source.kind === "origin") {
    return { sourceHandle: "out-bottom", targetHandle: "in-left" };
  }
  return { sourceHandle: "out-right", targetHandle: "in-left" };
}

/** Finds a mapped node by namespaced id; a stale id resolves to null. */
export function findTaskGraphFlowNode(
  nodes: readonly TaskGraphFlowNode[],
  nodeId: string | null | undefined,
): TaskGraphFlowNode | null {
  if (nodeId === null || nodeId === undefined) {
    return null;
  }
  return nodes.find((node) => node.id === nodeId) ?? null;
}

const BACKWARD_EDGE_CORNER = 6;

/**
 * Routes a backward `derived_from` edge (fact → origin) through the empty top
 * corridor. A standard smoothstep edge would cross the goal/intent columns at
 * the origin's height, hiding the wire and its label behind those cards. The
 * geometry mirrors the previous SVG renderer: right edge of the fact, up
 * through the fact/result gap, across above every lane, down into the origin.
 */
export function taskGraphBackwardEdgePath(
  source: TaskGraphNode,
  target: TaskGraphNode,
  layout: TaskGraphLayout,
): { path: string; labelX: number; labelY: number } | null {
  const from = layout.positions.get(source.id);
  const to = layout.positions.get(target.id);
  if (from === undefined || to === undefined) {
    return null;
  }
  const sourceHeight =
    layout.nodeHeights.get(source.id) ?? layout.nodeHeight;
  const sx = from.x + layout.nodeWidth;
  const sy = from.y + sourceHeight / 2;
  const gapX = sx + TASK_GRAPH_COLUMN_GAP / 2 - 3;
  const topCorridor = TASK_GRAPH_PADDING / 2;
  const targetX = to.x + layout.nodeWidth - 14;
  const targetY = to.y;
  const corner = BACKWARD_EDGE_CORNER;
  return {
    path: [
      `M ${sx} ${sy}`,
      `L ${gapX - corner} ${sy}`,
      `Q ${gapX} ${sy} ${gapX} ${sy - corner}`,
      `L ${gapX} ${topCorridor + corner}`,
      `Q ${gapX} ${topCorridor} ${gapX - corner} ${topCorridor}`,
      `L ${targetX} ${topCorridor}`,
      `L ${targetX} ${targetY}`,
    ].join(" "),
    labelX: gapX + 4,
    labelY: (topCorridor + sy) / 2,
  };
}

/**
 * Maps every graph node/edge onto the fixed positions from `layoutTaskGraph`.
 *
 * Positions are never recomputed or shuffled: `layout` is the single source
 * of x/y geometry, so React Flow pan/zoom/resize cannot move a card. Node
 * dimensions come from the same layout (fallback height until measured, which
 * this graph does not do — cards clamp their text instead).
 */
export function buildTaskGraphFlow(
  model: TaskGraphModel,
  layout: TaskGraphLayout,
  selectedNodeId?: string | null,
  work?: TaskWork | null,
): TaskGraphFlowModel {
  const nodes: TaskGraphFlowNode[] = model.nodes.map((source) => {
    const label = taskGraphNodeText(source, work);
    return {
      id: source.id,
      type: source.kind,
      position: layout.positions.get(source.id) ?? { x: 0, y: 0 },
      data: {
        kind: source.kind,
        kindLabel: TASK_GRAPH_KIND_LABELS[source.kind],
        label,
        state: source.state,
        entityId: source.entityId,
        nodeId: source.id,
        proves: taskGraphNodeProves(source),
        selected: selectedNodeId === source.id,
      },
      draggable: false,
      selectable: true,
      selected: selectedNodeId === source.id,
      sourcePosition: "right" as Position,
      targetPosition: "left" as Position,
      // Declared dimensions as well as inline style: the MiniMap reads
      // `width`/`height` (or measured size) from the user node, so a
      // style-only size renders an empty minimap with controlled nodes.
      width: layout.nodeWidth,
      height: layout.nodeHeights.get(source.id) ?? layout.nodeHeight,
      style: {
        width: layout.nodeWidth,
        height: layout.nodeHeights.get(source.id) ?? layout.nodeHeight,
      },
      ariaLabel: `${TASK_GRAPH_KIND_LABELS[source.kind]}: ${label}`,
    };
  });

  const nodeById = new Map(model.nodes.map((node) => [node.id, node]));
  const edges: TaskGraphFlowEdge[] = model.edges.map((source) => {
    const relationLabel = TASK_GRAPH_RELATION_LABELS[source.relation];
    const sourceNode = nodeById.get(source.from);
    const targetNode = nodeById.get(source.to);
    const handles =
      sourceNode !== undefined && targetNode !== undefined
        ? taskGraphFlowHandles(sourceNode, targetNode, source.relation)
        : { sourceHandle: "out-right", targetHandle: "in-left" };
    const backward =
      source.relation === "derived_from" &&
      sourceNode !== undefined &&
      targetNode !== undefined &&
      sourceNode.kind === "fact" &&
      targetNode.kind === "origin"
        ? taskGraphBackwardEdgePath(sourceNode, targetNode, layout)
        : null;
    return {
      id: source.id,
      source: source.from,
      target: source.to,
      sourceHandle: handles.sourceHandle,
      targetHandle: handles.targetHandle,
      type:
        backward === null
          ? "smoothstep"
          : TASK_GRAPH_BACKWARD_EDGE_TYPE,
      label: relationLabel,
      data: {
        relation: source.relation,
        relationLabel,
        ...(backward ?? {}),
      },
      markerEnd: {
        type: "arrowclosed" as MarkerType,
        color: TASK_GRAPH_FLOW_EDGE_COLOR,
        width: 16,
        height: 16,
      },
      style: {
        stroke: TASK_GRAPH_FLOW_EDGE_COLOR,
        strokeWidth: 1.5,
        strokeOpacity: 0.6,
      },
      labelStyle: {
        fill: TASK_GRAPH_FLOW_EDGE_LABEL_COLOR,
        fontSize: 10,
        fontWeight: 600,
      },
      labelBgStyle: {
        fill: TASK_GRAPH_FLOW_EDGE_LABEL_BG,
        fillOpacity: 0.92,
      },
      labelBgPadding: [4, 2],
      labelBgBorderRadius: 4,
      pathOptions: { offset: 20, borderRadius: 8 },
    };
  });

  return {
    nodes,
    edges,
    selected: findTaskGraphFlowNode(nodes, selectedNodeId),
  };
}
