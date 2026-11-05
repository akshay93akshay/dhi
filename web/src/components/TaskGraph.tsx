"use client";

import { useEffect, useId, useMemo, useRef, useState } from "react";
import {
  BadgeCheck,
  Check,
  FileText,
  GitBranch,
  Sparkles,
  Target,
  type LucideIcon,
} from "lucide-react";

import {
  TASK_GRAPH_COLUMN_GAP,
  TASK_GRAPH_KIND_LABELS,
  TASK_GRAPH_PADDING,
  TASK_GRAPH_ROW_GAP,
  buildTaskGraph,
  layoutTaskGraph,
  taskGraphColumn,
  type TaskGraphLayout,
  type TaskGraphNode,
  type TaskGraphNodeHeights,
  type TaskGraphNodeKind,
  type TaskGraphRelation,
} from "@/lib/taskGraph";
import type { TaskWork } from "@/lib/types";
import { Badge } from "@/components/ui/badge";
import { StateBadge, stateVariant } from "@/components/ui/state-badge";

const KIND_ORDER: readonly TaskGraphNodeKind[] = [
  "origin",
  "goal",
  "intent",
  "fact",
  "result",
];

const KIND_ICONS: Readonly<Record<TaskGraphNodeKind, LucideIcon>> = {
  origin: Sparkles,
  goal: Target,
  intent: GitBranch,
  fact: FileText,
  result: BadgeCheck,
};

/** DHI chart tokens keep the five kinds distinct in both themes. */
const KIND_COLORS: Readonly<Record<TaskGraphNodeKind, string>> = {
  origin: "var(--chart-4)",
  goal: "var(--chart-2)",
  intent: "var(--chart-3)",
  fact: "var(--success)",
  result: "var(--chart-5)",
};

const RELATIONS: readonly TaskGraphRelation[] = [
  "has_goal",
  "spawns",
  "yields",
  "derived_from",
];

/** Neutral wires keep the five node colours and the proves mark dominant. */
const EDGE_COLOR = "var(--muted-foreground)";

/** Fill the panel, but never magnify a small graph into a blurry poster. */
const MAX_FIT_SCALE = 1.15;
const FIT_PADDING = 12;

const EDGE_LABEL_FONT_SIZE = 10;
/** Keep every relation label inside the column gap, clear of both cards. */
const EDGE_LABEL_MAX_WIDTH = TASK_GRAPH_COLUMN_GAP - 8;

function naturalEdgeLabelWidth(label: string): number {
  return label.length * EDGE_LABEL_FONT_SIZE * 0.52;
}

function edgeLabelWidth(label: string): number {
  return Math.min(naturalEdgeLabelWidth(label), EDGE_LABEL_MAX_WIDTH);
}

function stateColor(state: string): string {
  switch (stateVariant(state)) {
    case "success":
      return "var(--success)";
    case "warning":
      return "var(--warning)";
    case "destructive":
      return "var(--destructive)";
    case "info":
      return "var(--info)";
    case "accent":
      return "var(--accent-foreground)";
    default:
      return "var(--muted-foreground)";
  }
}

function nodeProves(node: TaskGraphNode): boolean {
  if (node.kind === "goal") {
    return node.proofFactId !== null;
  }
  if (node.kind === "fact" || node.kind === "origin") {
    return node.provesGoal;
  }
  return false;
}

/**
 * Reusable task-work graph.
 *
 * Five fixed columns read left to right: origin, goal, intent, fact, result.
 * HTML node cards sit above one SVG edge layer; a single automatic transform
 * fits the fixed canvas into the visible stage. There is no pan/zoom, no
 * inspector, and no click selection — nodes are presentational.
 */
export function TaskGraph({ work }: { work: TaskWork | null }) {
  const model = useMemo(
    () => (work === null ? { nodes: [], edges: [] } : buildTaskGraph(work)),
    [work],
  );
  const [measuredHeights, setMeasuredHeights] =
    useState<TaskGraphNodeHeights>(() => new Map<string, number>());
  const measuredHeightsRef = useRef<TaskGraphNodeHeights>(measuredHeights);
  const nodeElements = useRef(new Map<string, HTMLElement>());
  const stageRef = useRef<HTMLDivElement | null>(null);
  const [stageSize, setStageSize] = useState<{
    width: number;
    height: number;
  } | null>(null);
  const stageSizeRef = useRef<{ width: number; height: number } | null>(null);
  const [fit, setFit] = useState<{
    k: number;
    tx: number;
    ty: number;
    layoutWidth: number;
    layoutHeight: number;
  } | null>(null);

  // The natural layout only needs measured card heights. Stage height is a
  // separate input so it can stretch the canvas vertically without ever
  // changing its width or the card boxes.
  const baseLayout = useMemo(
    () => layoutTaskGraph(model.nodes, measuredHeights),
    [model.nodes, measuredHeights],
  );
  const minHeight = useMemo(() => {
    if (stageSize === null) {
      return undefined;
    }
    const availableWidth = Math.max(1, stageSize.width - FIT_PADDING * 2);
    const availableHeight = Math.max(1, stageSize.height - FIT_PADDING * 2);
    const widthScale = availableWidth / Math.max(1, baseLayout.width);
    // Short graphs grow their canvas until the width-fit scale also fills the
    // stage height. The cap matches the fit transform above.
    const fillScale = Math.min(widthScale, MAX_FIT_SCALE);
    return availableHeight / Math.max(0.0001, fillScale);
  }, [stageSize, baseLayout.width]);
  const layout = useMemo(
    () =>
      layoutTaskGraph(
        model.nodes,
        measuredHeights,
        minHeight === undefined ? undefined : { minHeight },
      ),
    [model.nodes, measuredHeights, minHeight],
  );
  const nodeById = useMemo(
    () => new Map(model.nodes.map((node) => [node.id, node])),
    [model.nodes],
  );

  const taskId = work?.task.id ?? "";
  const empty =
    work !== null &&
    work.goals.length === 0 &&
    work.intents.length === 0 &&
    work.facts.length === 0 &&
    work.results.length === 0;

  // Stage dimensions drive `minHeight`; commit only whole-pixel changes so a
  // fractional ResizeObserver report cannot spin the layout.
  useEffect(() => {
    const element = stageRef.current;
    if (element === null) {
      return;
    }
    const update = (): void => {
      const width = element.clientWidth;
      const height = element.clientHeight;
      if (width <= 0 || height <= 0) {
        return;
      }
      const previous = stageSizeRef.current;
      if (
        previous !== null &&
        Math.abs(previous.width - width) < 1 &&
        Math.abs(previous.height - height) < 1
      ) {
        return;
      }
      const next = { width, height };
      stageSizeRef.current = next;
      setStageSize(next);
    };
    update();
    if (typeof ResizeObserver === "undefined") {
      return;
    }
    const observer = new ResizeObserver(update);
    observer.observe(element);
    return () => observer.disconnect();
  }, [empty, taskId]);

  // One ref per node feeds one ResizeObserver. Heights only commit when they
  // move by a full pixel, so a layout reflow cannot ping-pong with the observer.
  useEffect(() => {
    if (typeof ResizeObserver === "undefined") {
      return;
    }
    const observer = new ResizeObserver((entries) => {
      let next: Map<string, number> | null = null;
      for (const entry of entries) {
        const element = entry.target as HTMLElement;
        const id = element.dataset.taskGraphNode;
        if (id === undefined) {
          continue;
        }
        const borderBox = entry.borderBoxSize[0];
        const height = Math.max(
          1,
          Math.ceil(borderBox?.blockSize ?? element.offsetHeight),
        );
        const previous = measuredHeightsRef.current.get(id);
        if (previous !== undefined && Math.abs(previous - height) < 1) {
          continue;
        }
        if (next === null) {
          next = new Map(measuredHeightsRef.current);
        }
        next.set(id, height);
      }
      if (next !== null) {
        measuredHeightsRef.current = next;
        setMeasuredHeights(next);
      }
    });
    for (const element of nodeElements.current.values()) {
      observer.observe(element);
    }
    return () => observer.disconnect();
  }, [model.nodes]);

  // One automatic fit: always fit the canvas width, then centre it. A canvas
  // shorter than the stage is vertically centred; a taller one starts at the
  // top and scrolls inside the stage.
  // ResizeObserver covers panel resizes; layout dimension changes cover data.
  useEffect(() => {
    const element = stageRef.current;
    if (element === null) {
      return;
    }
    const update = (): void => {
      const width = element.clientWidth;
      const height = element.clientHeight;
      if (width <= 0 || height <= 0) {
        return;
      }
      const availableWidth = Math.max(1, width - FIT_PADDING * 2);
      const k = Math.max(
        0.0001,
        Math.min(
          availableWidth / Math.max(1, layout.width),
          MAX_FIT_SCALE,
        ),
      );
      setFit({
        k,
        tx: (width - layout.width * k) / 2,
        ty: Math.max(
          FIT_PADDING,
          (height - layout.height * k) / 2,
        ),
        layoutWidth: layout.width,
        layoutHeight: layout.height,
      });
    };
    update();
    if (typeof ResizeObserver === "undefined") {
      return;
    }
    const observer = new ResizeObserver(update);
    observer.observe(element);
    return () => observer.disconnect();
  }, [empty, layout.height, layout.width, taskId]);

  const markerPrefix = useId().replaceAll(":", "");
  // A fit from an older layout is never applied to a newer, larger canvas.
  const activeFit =
    fit !== null &&
    fit.layoutWidth === layout.width &&
    fit.layoutHeight === layout.height
      ? fit
      : null;
  // The spacer is the only in-flow child, so the auto scrollbar range is
  // exactly the scaled canvas plus the same top/bottom padding the fit uses.
  const scrollHeight =
    layout.height * (activeFit?.k ?? 0) + FIT_PADDING * 2;

  if (work === null) {
    return null;
  }

  if (empty) {
    return (
      <div className="flex h-full min-h-[360px] flex-col items-center justify-center rounded-xl border border-dashed border-border bg-surface-subtle px-6 py-12 text-center">
        <span className="flex h-11 w-11 items-center justify-center rounded-full border border-border bg-card text-muted-foreground">
          <GitBranch className="h-5 w-5" />
        </span>
        <h3 className="mt-3 text-sm font-semibold text-foreground">
          No work nodes yet
        </h3>
        <p className="mt-1 max-w-md text-xs leading-5 text-muted-foreground">
          This task has no goals, intents, facts, or results. Its graph will
          appear here as the planner and workers record work.
        </p>
        <StateBadge state={work.task.state} className="mt-3" />
      </div>
    );
  }

  return (
    <div className="flex h-full min-h-[360px] flex-col overflow-hidden rounded-xl border border-border bg-card">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1.5 border-b border-border px-3 py-2">
        <div
          className="flex flex-wrap items-center gap-x-3 gap-y-1"
          aria-label="Legend"
        >
          {KIND_ORDER.map((kind) => (
            <span
              key={kind}
              className="inline-flex items-center gap-1 text-[11px] font-medium text-muted-foreground"
            >
              <span
                className="h-1.5 w-1.5 rounded-full"
                style={{ background: KIND_COLORS[kind] }}
                aria-hidden="true"
              />
              {TASK_GRAPH_KIND_LABELS[kind]}
            </span>
          ))}
          <span className="inline-flex items-center gap-1 text-[11px] font-medium text-muted-foreground">
            <span className="flex h-3 w-3 items-center justify-center rounded-full bg-success text-success-foreground">
              <Check className="h-2 w-2" strokeWidth={3} aria-hidden="true" />
            </span>
            proves
          </span>
        </div>
        <Badge
          variant="outline"
          className="ml-auto font-mono font-normal tabular-nums"
        >
          {model.nodes.length} nodes · {model.edges.length} edges
        </Badge>
      </div>

      <div
        ref={stageRef}
        className="relative min-h-0 flex-1 overflow-y-auto overflow-x-hidden"
        role="img"
        aria-label={`Task work graph for ${work.task.goal}`}
        style={{
          scrollbarGutter: "stable",
          backgroundColor: "var(--surface-subtle)",
          backgroundImage:
            "radial-gradient(color-mix(in srgb, var(--muted-foreground) 15%, transparent) 0.7px, transparent 0.7px)",
          backgroundPosition: "10px 10px",
          backgroundSize: "22px 22px",
        }}
      >
        <div
          aria-hidden="true"
          className="pointer-events-none"
          style={{ height: scrollHeight }}
        />

        <div
          className="absolute left-0 top-0 origin-top-left"
          style={{
            width: layout.width,
            height: layout.height,
            opacity: activeFit === null ? 0 : 1,
            transform:
              activeFit === null
                ? undefined
                : `translate(${activeFit.tx}px, ${activeFit.ty}px) scale(${activeFit.k})`,
          }}
        >
          {layout.columnCenters.map((x) => (
            <span
              key={x}
              className="pointer-events-none absolute inset-y-0 border-l border-dashed border-border/70"
              style={{ left: x }}
              aria-hidden="true"
            />
          ))}

          <svg
            width={layout.width}
            height={layout.height}
            className="pointer-events-none absolute inset-0"
            aria-hidden="true"
          >
            <defs>
              {RELATIONS.map((relation) => (
                <marker
                  key={relation}
                  id={`${markerPrefix}-${relation}`}
                  markerWidth="8"
                  markerHeight="8"
                  refX="7"
                  refY="4"
                  orient="auto"
                >
                  <path
                    d="M0,0 L8,4 L0,8 z"
                    style={{ fill: EDGE_COLOR }}
                  />
                </marker>
              ))}
            </defs>

            {model.edges.map((edge) => {
              const from = layout.positions.get(edge.from);
              const to = layout.positions.get(edge.to);
              const fromNode = nodeById.get(edge.from);
              const toNode = nodeById.get(edge.to);
              if (
                from === undefined ||
                to === undefined ||
                fromNode === undefined ||
                toNode === undefined
              ) {
                return null;
              }
              const geometry = edgeGeometry(
                from,
                to,
                fromNode,
                toNode,
                layout,
              );
              const label = edge.relation.replaceAll("_", " ");
              const labelPoint = edgeLabelPoint(geometry, label, layout);
              const naturalLabelWidth = naturalEdgeLabelWidth(label);
              return (
                <g key={edge.id}>
                  <path
                    d={geometry.path}
                    fill="none"
                    stroke={EDGE_COLOR}
                    strokeWidth={1.5}
                    strokeOpacity={0.55}
                    markerEnd={`url(#${markerPrefix}-${edge.relation})`}
                  >
                    <title>
                      {edge.relation}: {fromNode.label} → {toNode.label}
                    </title>
                  </path>
                  <text
                    x={labelPoint.x}
                    y={labelPoint.y}
                    textAnchor="middle"
                    textLength={
                      naturalLabelWidth > EDGE_LABEL_MAX_WIDTH
                        ? EDGE_LABEL_MAX_WIDTH
                        : undefined
                    }
                    lengthAdjust={
                      naturalLabelWidth > EDGE_LABEL_MAX_WIDTH
                        ? "spacingAndGlyphs"
                        : undefined
                    }
                    style={{
                      fill: "var(--muted-foreground)",
                      fontSize: EDGE_LABEL_FONT_SIZE,
                      fontWeight: 600,
                      paintOrder: "stroke",
                      stroke: "var(--surface-subtle)",
                      strokeWidth: 3,
                      strokeLinejoin: "round",
                    }}
                  >
                    {label}
                  </text>
                </g>
              );
            })}
          </svg>

          {model.nodes.map((node) => {
            const point = layout.positions.get(node.id);
            if (point === undefined) {
              return null;
            }
            const Icon = KIND_ICONS[node.kind];
            const color = KIND_COLORS[node.kind];
            const proves = nodeProves(node);
            return (
              <article
                key={node.id}
                ref={(element) => {
                  if (element === null) {
                    nodeElements.current.delete(node.id);
                  } else {
                    nodeElements.current.set(node.id, element);
                  }
                }}
                data-task-graph-node={node.id}
                className="absolute flex flex-col rounded-xl border border-border bg-card px-3 py-2.5 shadow-sm"
                style={{
                  left: point.x,
                  top: point.y,
                  width: layout.nodeWidth,
                  borderLeft: `3px solid ${color}`,
                }}
              >
                <span className="flex min-w-0 items-center gap-1.5">
                  <Icon
                    className="h-3 w-3 shrink-0"
                    style={{ color }}
                    aria-hidden="true"
                  />
                  <span
                    className="min-w-0 flex-1 text-[11px] font-bold uppercase tracking-[0.13em]"
                    style={{ color }}
                  >
                    {TASK_GRAPH_KIND_LABELS[node.kind]}
                  </span>
                  {proves ? (
                    <span
                      className="flex h-3.5 w-3.5 shrink-0 items-center justify-center rounded-full bg-success text-success-foreground shadow-sm"
                      aria-label="proves"
                    >
                      <Check
                        className="h-2 w-2"
                        strokeWidth={3}
                        aria-hidden="true"
                      />
                    </span>
                  ) : null}
                </span>
                <span className="mt-1.5 block break-words text-[16px] font-medium leading-[22px] text-foreground [overflow-wrap:anywhere]">
                  {node.label}
                </span>
                <span className="mt-2 flex min-w-0 items-start gap-1.5 text-[11px] leading-[16px] text-muted-foreground">
                  <span
                    className="mt-1 h-1.5 w-1.5 shrink-0 rounded-full"
                    style={{ background: stateColor(node.state) }}
                    aria-hidden="true"
                  />
                  <span className="min-w-0 break-words [overflow-wrap:anywhere]">
                    {node.state}
                  </span>
                </span>
              </article>
            );
          })}
        </div>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Edge geometry
// ---------------------------------------------------------------------------

type Point = { x: number; y: number };
type EdgeGeometry = { path: string; labelX: number; labelY: number };

function cubicPoint(
  p0: Point,
  p1: Point,
  p2: Point,
  p3: Point,
  t: number,
): Point {
  const mt = 1 - t;
  const a = mt * mt * mt;
  const b = 3 * mt * mt * t;
  const c = 3 * mt * t * t;
  const d = t * t * t;
  return {
    x: a * p0.x + b * p1.x + c * p2.x + d * p3.x,
    y: a * p0.y + b * p1.y + c * p2.y + d * p3.y,
  };
}

const CORNER = 6;

/** Border-box height of one node in this layout (measured or fallback). */
function nodeHeightIn(
  node: TaskGraphNode,
  layout: TaskGraphLayout,
): number {
  return layout.nodeHeights.get(node.id) ?? layout.nodeHeight;
}

/** Keeps edge labels inside the canvas even at the outermost corridors. */
function edgeLabelPoint(
  geometry: EdgeGeometry,
  label: string,
  layout: TaskGraphLayout,
): Point {
  const halfWidth = edgeLabelWidth(label) / 2;
  const minX = Math.min(layout.width / 2, halfWidth + 6);
  const maxX = Math.max(minX, layout.width - halfWidth - 6);
  return {
    x: Math.min(maxX, Math.max(minX, geometry.labelX)),
    y: Math.min(layout.height - 6, Math.max(12, geometry.labelY)),
  };
}

/**
 * Routes each relation through empty space:
 * - forward edges use the gap between adjacent columns;
 * - origin → goal-less intents goes around the left/bottom margin so it never
 *   crosses a goal card;
 * - intent → result dips under the row's fact stack before rising into the
 *   result column;
 * - fact → fact bows into the intent/fact gap, while backward fact → origin
 *   uses the fact/result gap and the top corridor.
 *
 * Every endpoint uses the node's own measured height, so taller wrapped cards
 * keep their wires on the edge centres instead of assuming a fixed card size.
 */
function edgeGeometry(
  from: Point,
  to: Point,
  fromNode: TaskGraphNode,
  toNode: TaskGraphNode,
  layout: TaskGraphLayout,
): EdgeGeometry {
  const nodeWidth = layout.nodeWidth;
  const fromHeight = nodeHeightIn(fromNode, layout);
  const toHeight = nodeHeightIn(toNode, layout);
  const fromColumn = taskGraphColumn(fromNode.kind);
  const toColumn = taskGraphColumn(toNode.kind);
  const halfGap = TASK_GRAPH_COLUMN_GAP / 2;
  const bottomCorridor = layout.height - TASK_GRAPH_PADDING / 2;

  // Same column (fact → fact derived_from): bow through the left gap so the
  // edge never enters a card.
  if (fromColumn === toColumn) {
    const channelX = from.x - halfGap;
    const sx = from.x;
    const sy = from.y + fromHeight / 2;
    const ex = to.x;
    const ey = to.y + toHeight / 2;
    const mid = cubicPoint(
      { x: sx, y: sy },
      { x: channelX, y: sy },
      { x: channelX, y: ey },
      { x: ex, y: ey },
      0.5,
    );
    return {
      path: `M ${sx} ${sy} C ${channelX} ${sy}, ${channelX} ${ey}, ${ex} ${ey}`,
      labelX: channelX,
      labelY: mid.y - 4,
    };
  }

  // Backward cross-column (fact → origin derived_from): leave through the
  // fact/result gap, run above every lane, then descend into the origin's top.
  if (toColumn < fromColumn) {
    const gapX = from.x + nodeWidth + halfGap - 3;
    const targetX = to.x + nodeWidth - 14;
    const topCorridor = TASK_GRAPH_PADDING / 2;
    const sx = from.x + nodeWidth;
    const sy = from.y + fromHeight / 2;
    const ey = to.y;
    const path = [
      `M ${sx} ${sy}`,
      `L ${gapX - CORNER} ${sy}`,
      `Q ${gapX} ${sy} ${gapX} ${sy - CORNER}`,
      `L ${gapX} ${topCorridor + CORNER}`,
      `Q ${gapX} ${topCorridor} ${gapX - CORNER} ${topCorridor}`,
      `L ${targetX} ${topCorridor}`,
      `L ${targetX} ${ey}`,
    ].join(" ");
    return {
      path,
      labelX: gapX + 4,
      labelY: (topCorridor + sy) / 2,
    };
  }

  // origin → goal-less intent: when goal lanes exist, go around the outside
  // (left margin, bottom corridor, then up the empty goal/intent gap) so it
  // cannot pass through a goal card in an intervening lane. A graph with only
  // a root lane can use the ordinary direct path through the empty goal column.
  if (
    fromColumn === 0 &&
    toColumn === 2 &&
    layout.lanes.some((lane) => lane.kind === "goal")
  ) {
    const leftMargin = TASK_GRAPH_PADDING / 2;
    const channelX = to.x - halfGap;
    const sx = from.x;
    const sy = from.y + fromHeight / 2;
    const ty = to.y + toHeight / 2;
    const path = [
      `M ${sx} ${sy}`,
      `L ${leftMargin + CORNER} ${sy}`,
      `Q ${leftMargin} ${sy} ${leftMargin} ${sy + CORNER}`,
      `L ${leftMargin} ${bottomCorridor - CORNER}`,
      `Q ${leftMargin} ${bottomCorridor} ${leftMargin + CORNER} ${bottomCorridor}`,
      `L ${channelX - CORNER} ${bottomCorridor}`,
      `Q ${channelX} ${bottomCorridor} ${channelX} ${bottomCorridor - CORNER}`,
      `L ${channelX} ${ty + CORNER}`,
      `Q ${channelX} ${ty} ${channelX + CORNER} ${ty}`,
      `L ${to.x} ${ty}`,
    ].join(" ");
    return {
      path,
      labelX: channelX,
      labelY: (bottomCorridor + ty) / 2,
    };
  }

  // intent → result: dip under the row's fact stack, then rise through the
  // fact/result gap to the result's left edge. Results stay aligned with their
  // intent row without the wire cutting through a fact card.
  if (fromColumn === 2 && toColumn === 4) {
    const row = layout.rows.find((entry) => entry.intentId === fromNode.id);
    const intentBottom = from.y + fromHeight;
    const rowBottom =
      row === undefined ? intentBottom : row.top + row.height;
    const corridorY =
      Math.max(rowBottom, intentBottom) + TASK_GRAPH_ROW_GAP / 2;
    const channelX = to.x - halfGap + 3;
    const sx = from.x + nodeWidth * 0.6;
    const sy = intentBottom;
    const ey = to.y + toHeight / 2;
    const path = [
      `M ${sx} ${sy}`,
      `L ${sx} ${corridorY - CORNER}`,
      `Q ${sx} ${corridorY} ${sx + CORNER} ${corridorY}`,
      `L ${channelX - CORNER} ${corridorY}`,
      `Q ${channelX} ${corridorY} ${channelX} ${corridorY - CORNER}`,
      `L ${channelX} ${ey + CORNER}`,
      `Q ${channelX} ${ey} ${channelX + CORNER} ${ey}`,
      `L ${to.x} ${ey}`,
    ].join(" ");
    return {
      path,
      labelX: channelX,
      labelY: corridorY + 5,
    };
  }

  // Standard forward edge: right edge → left edge through the column gap.
  const sx = from.x + nodeWidth;
  const sy = from.y + fromHeight / 2;
  const ex = to.x;
  const ey = to.y + toHeight / 2;
  const bend = Math.min(80, Math.max(8, Math.abs(ex - sx) * 0.45));
  const p1 = { x: sx + bend, y: sy };
  const p2 = { x: ex - bend, y: ey };
  const mid = cubicPoint({ x: sx, y: sy }, p1, p2, { x: ex, y: ey }, 0.5);
  return {
    path: `M ${sx} ${sy} C ${p1.x} ${p1.y}, ${p2.x} ${p2.y}, ${ex} ${ey}`,
    labelX: mid.x,
    labelY: mid.y - 5,
  };
}
