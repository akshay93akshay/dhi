"use client";

import {
  useCallback,
  useEffect,
  useMemo,
  useState,
  type MouseEvent as ReactMouseEvent,
  type ReactNode,
} from "react";
import {
  BadgeCheck,
  Check,
  FileText,
  GitBranch,
  MousePointerClick,
  Sparkles,
  Target,
  X,
  type LucideIcon,
} from "lucide-react";
import {
  Background,
  BaseEdge,
  Controls,
  Handle,
  MiniMap,
  Position,
  ReactFlow,
  type EdgeProps,
  type EdgeTypes,
  type Node,
  type NodeProps,
  type NodeTypes,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";

import {
  TASK_GRAPH_KIND_LABELS,
  buildTaskGraph,
  layoutTaskGraph,
  type TaskGraphNodeKind,
} from "@/lib/taskGraph";
import {
  TASK_GRAPH_BACKWARD_EDGE_TYPE,
  buildTaskGraphFlow,
  type TaskGraphFlowEdge,
  type TaskGraphFlowNode,
  type TaskGraphFlowNodeData,
} from "@/lib/taskGraphFlow";
import type { TaskWork } from "@/lib/types";
import { plainPreview } from "@/lib/preview";
import { cn } from "@/lib/utils";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { CopyButton } from "@/components/ui/copy-button";
import { MarkdownMath } from "@/components/markdown-math";
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

/**
 * One card shape for all five kinds. React Flow registers this component under
 * every kind name, so the node `type` still identifies the work record.
 *
 * The label is clamped to the fixed card height; the inspector below the
 * graph always shows the complete text.
 */
function TaskGraphNodeCard({ data, selected }: NodeProps<TaskGraphFlowNode>) {
  const node = data;
  const Icon = KIND_ICONS[node.kind];
  const color = KIND_COLORS[node.kind];
  const active = selected || node.selected;
  // Cards show a compact plain-text preview; the inspector renders the full
  // Markdown/LaTeX source.
  const preview = plainPreview(node.label);

  return (
    <>
      <Handle
        id="in-left"
        type="target"
        position={Position.Left}
        isConnectable={false}
        className="!h-1.5 !w-1.5 !border-0 !bg-transparent"
        style={{ opacity: 0, pointerEvents: "none" }}
      />
      <Handle
        id="in-right"
        type="target"
        position={Position.Right}
        isConnectable={false}
        className="!h-1.5 !w-1.5 !border-0 !bg-transparent"
        style={{ opacity: 0, pointerEvents: "none" }}
      />
      <Handle
        id="in-bottom"
        type="target"
        position={Position.Bottom}
        isConnectable={false}
        className="!h-1.5 !w-1.5 !border-0 !bg-transparent"
        style={{ opacity: 0, pointerEvents: "none" }}
      />
      <div
        className={cn(
          "flex h-full w-full flex-col overflow-hidden rounded-xl border bg-card px-3.5 py-3 shadow-sm transition-shadow",
          active
            ? "border-accent-foreground/60 ring-2 ring-accent-foreground/25"
            : "border-border",
        )}
        style={{ borderLeft: `3px solid ${color}` }}
      >
        <span className="flex min-w-0 items-center gap-1.5">
          <Icon
            className="h-3.5 w-3.5 shrink-0"
            style={{ color }}
            aria-hidden="true"
          />
          <span
            className="min-w-0 flex-1 text-[12px] font-bold uppercase tracking-[0.13em]"
            style={{ color }}
          >
            {node.kindLabel}
          </span>
          {node.proves ? (
            <span
              className="flex h-4 w-4 shrink-0 items-center justify-center rounded-full bg-success text-success-foreground shadow-sm"
              aria-label="proves"
            >
              <Check className="h-2.5 w-2.5" strokeWidth={3} aria-hidden="true" />
            </span>
          ) : null}
        </span>
        <span
          className="mt-1.5 line-clamp-3 min-h-0 flex-1 break-words text-[15px] font-medium leading-[1.55] text-foreground [overflow-wrap:anywhere]"
          title={preview}
        >
          {preview}
        </span>
        <span className="mt-2 flex min-w-0 items-start gap-1.5 text-[12px] leading-[1.4] text-muted-foreground">
          <span
            className="mt-1 h-1.5 w-1.5 shrink-0 rounded-full"
            style={{ background: stateColor(node.state) }}
            aria-hidden="true"
          />
          <span className="min-w-0 truncate">{node.state}</span>
        </span>
      </div>
      <Handle
        id="out-right"
        type="source"
        position={Position.Right}
        isConnectable={false}
        className="!h-1.5 !w-1.5 !border-0 !bg-transparent"
        style={{ opacity: 0, pointerEvents: "none" }}
      />
      <Handle
        id="out-bottom"
        type="source"
        position={Position.Bottom}
        isConnectable={false}
        className="!h-1.5 !w-1.5 !border-0 !bg-transparent"
        style={{ opacity: 0, pointerEvents: "none" }}
      />
    </>
  );
}

const NODE_TYPES: NodeTypes = {
  origin: TaskGraphNodeCard,
  goal: TaskGraphNodeCard,
  intent: TaskGraphNodeCard,
  fact: TaskGraphNodeCard,
  result: TaskGraphNodeCard,
};

/** Renders the precomputed top-corridor path for backward derived_from edges. */
function TaskGraphBackwardEdge(props: EdgeProps<TaskGraphFlowEdge>) {
  const data = props.data;
  if (data?.path === undefined) {
    return null;
  }
  return (
    <BaseEdge
      id={props.id}
      path={data.path}
      label={props.label}
      labelX={data.labelX}
      labelY={data.labelY}
      labelStyle={props.labelStyle}
      labelShowBg={props.labelShowBg}
      labelBgStyle={props.labelBgStyle}
      labelBgPadding={props.labelBgPadding}
      labelBgBorderRadius={props.labelBgBorderRadius}
      markerEnd={props.markerEnd}
      style={props.style}
      interactionWidth={props.interactionWidth}
    />
  );
}

const EDGE_TYPES: EdgeTypes = {
  [TASK_GRAPH_BACKWARD_EDGE_TYPE]: TaskGraphBackwardEdge,
};

function InspectorSection(props: { title: string; children: ReactNode }) {
  return (
    <section>
      <h3 className="text-[12px] font-semibold uppercase tracking-[0.12em] text-muted-foreground">
        {props.title}
      </h3>
      {props.children}
    </section>
  );
}

/** Full-text inspector for one clicked node. */
function TaskGraphInspector(props: {
  node: TaskGraphFlowNode;
  work: TaskWork;
  onClose: () => void;
}) {
  const { node, work, onClose } = props;
  const data = node.data;
  const Icon = KIND_ICONS[data.kind];
  const color = KIND_COLORS[data.kind];

  // Fact/origin cards deliberately clamp their text. Resolve the source claim
  // from the latest work snapshot so the inspector never shows the truncation.
  const fact =
    data.kind === "fact" || data.kind === "origin"
      ? work.facts.find((entry) => entry.id === data.entityId)
      : undefined;
  const fullLabel =
    fact !== undefined && fact.claim.trim() !== ""
      ? fact.claim.trim()
      : data.label;
  const heading =
    data.kind === "fact" || data.kind === "origin" ? "Claim" : "Full label";

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex items-start gap-2.5 border-b border-border px-4 py-3">
        <span
          className="mt-0.5 flex h-8 w-8 shrink-0 items-center justify-center rounded-lg border border-border"
          style={{
            background: `color-mix(in srgb, ${color} 12%, transparent)`,
          }}
        >
          <Icon className="h-3.5 w-3.5" style={{ color }} aria-hidden="true" />
        </span>
        <div className="min-w-0 flex-1">
          <p
            className="text-[12px] font-bold uppercase tracking-[0.13em]"
            style={{ color }}
          >
            {TASK_GRAPH_KIND_LABELS[data.kind]}
          </p>
          <p
            className="mono truncate text-[12.5px] text-muted-foreground"
            title={data.nodeId}
          >
            {data.entityId}
          </p>
        </div>
        <Button
          aria-label="Close node inspector"
          className="h-7 w-7 shrink-0"
          onClick={onClose}
          size="icon"
          type="button"
          variant="ghost"
        >
          <X className="h-3.5 w-3.5" />
        </Button>
      </div>

      <div className="min-h-0 flex-1 space-y-4 overflow-y-auto px-4 py-4">
        <div className="flex flex-wrap items-center gap-2">
          <StateBadge state={data.state} />
          {data.proves ? (
            <Badge variant="success" className="gap-1 font-normal">
              <Check className="h-2.5 w-2.5" strokeWidth={3} aria-hidden="true" />
              proves
            </Badge>
          ) : null}
        </div>

        <InspectorSection title={heading}>
          <MarkdownMath
            className="mt-1 text-[15px] leading-[1.55] text-foreground [&_p+p]:mt-2 [&_ul]:my-1 [&_ol]:my-1 [&_li]:my-0.5 [&_.katex-display]:my-1"
            text={fullLabel}
          />
          <CopyButton
            className="-ml-2 mt-1 h-7 text-[12px]"
            label="Copy label"
            text={fullLabel}
          />
        </InspectorSection>

        <InspectorSection title="ID">
          <div className="mt-1 flex items-start gap-1 rounded-lg border border-border bg-surface-subtle px-3 py-2.5">
            <code className="mono min-w-0 flex-1 break-all text-[12.5px] leading-[1.5] text-foreground">
              {data.entityId}
            </code>
            <CopyButton
              className="h-6"
              label="Copy ID"
              text={data.entityId}
            />
          </div>
          <p className="mt-1 break-all text-[12px] leading-[1.45] text-muted-foreground">
            node <span className="mono">{data.nodeId}</span>
          </p>
        </InspectorSection>
      </div>
    </div>
  );
}

/**
 * Reusable task-work graph.
 *
 * Five fixed columns read left to right: origin, goal, intent, fact, result.
 * Positions come from `layoutTaskGraph`; React Flow adds pan, zoom, fit,
 * minimap, controls, and click selection without ever auto-shuffling cards.
 * Clicking a node opens the full-text inspector below the graph, so the
 * drawing always keeps the full content width.
 */
export function TaskGraph({ work }: { work: TaskWork | null }) {
  const model = useMemo(
    () => (work === null ? { nodes: [], edges: [] } : buildTaskGraph(work)),
    [work],
  );
  const layout = useMemo(
    () => layoutTaskGraph(model.nodes),
    [model.nodes],
  );
  const [selectedNodeId, setSelectedNodeId] = useState<string | null>(null);

  // Polling can remove the selected record; close the inspector instead of
  // showing a stale snapshot that no longer exists on the graph.
  useEffect(() => {
    setSelectedNodeId((current) => {
      if (current === null) {
        return current;
      }
      return model.nodes.some((node) => node.id === current) ? current : null;
    });
  }, [model.nodes]);

  const flow = useMemo(
    () => buildTaskGraphFlow(model, layout, selectedNodeId, work),
    [model, layout, selectedNodeId, work],
  );

  const handleNodeClick = useCallback(
    (_event: ReactMouseEvent, node: Node) => {
      setSelectedNodeId(node.id);
    },
    [],
  );
  const handlePaneClick = useCallback(() => {
    setSelectedNodeId(null);
  }, []);

  const empty =
    work !== null &&
    work.goals.length === 0 &&
    work.intents.length === 0 &&
    work.facts.length === 0 &&
    work.results.length === 0;

  if (work === null) {
    return null;
  }

  if (empty) {
    return (
      <div className="flex h-full min-h-[560px] flex-col items-center justify-center rounded-xl border border-dashed border-border bg-surface-subtle px-6 py-12 text-center">
        <span className="flex h-11 w-11 items-center justify-center rounded-full border border-border bg-card text-muted-foreground">
          <GitBranch className="h-5 w-5" />
        </span>
        <h3 className="mt-3 text-[18px] font-semibold text-foreground">
          No work nodes yet
        </h3>
        <p className="mt-1 max-w-md text-[13px] leading-5 text-muted-foreground">
          This task has no goals, intents, facts, or results. Its graph will
          appear here as the planner and workers record work.
        </p>
        <StateBadge state={work.task.state} className="mt-3" />
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-3">
      <div className="flex h-[560px] min-w-0 flex-col overflow-hidden rounded-xl border border-border bg-card sm:h-[620px] lg:h-[calc(100svh-260px)] lg:min-h-[600px] lg:max-h-[900px]">
        <div className="flex flex-wrap items-center gap-x-3 gap-y-1.5 border-b border-border px-3 py-2">
          <div
            className="flex flex-wrap items-center gap-x-3 gap-y-1"
            aria-label="Legend"
          >
            {KIND_ORDER.map((kind) => (
              <span
                key={kind}
                className="inline-flex items-center gap-1 text-[12px] font-medium text-muted-foreground"
              >
                <span
                  className="h-1.5 w-1.5 rounded-full"
                  style={{ background: KIND_COLORS[kind] }}
                  aria-hidden="true"
                />
                {TASK_GRAPH_KIND_LABELS[kind]}
              </span>
            ))}
            <span className="inline-flex items-center gap-1 text-[12px] font-medium text-muted-foreground">
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
          className="relative min-h-0 flex-1"
          role="region"
          aria-label={`Task work graph for ${work.task.goal}`}
        >
          <ReactFlow
            className="h-full w-full"
            edges={flow.edges}
            edgeTypes={EDGE_TYPES}
            edgesReconnectable={false}
            elementsSelectable
            fitView
            // The 0.9 floor keeps the 15px card text at 13.5px or larger.
            fitViewOptions={{ padding: 0.06, minZoom: 0.9, maxZoom: 1.1 }}
            maxZoom={1.1}
            minZoom={0.9}
            nodes={flow.nodes}
            nodesConnectable={false}
            nodesDraggable={false}
            nodeTypes={NODE_TYPES}
            onNodeClick={handleNodeClick}
            onPaneClick={handlePaneClick}
            proOptions={{ hideAttribution: false }}
          >
            <Background color="var(--border)" gap={22} size={1} />
            <Controls showInteractive={false} />
            <MiniMap
              nodeColor={(node) => {
                const data = node.data as TaskGraphFlowNodeData;
                return KIND_COLORS[data.kind] ?? "var(--border)";
              }}
              nodeStrokeWidth={2}
              pannable
              zoomable
            />
          </ReactFlow>
        </div>
      </div>

      <aside
        aria-label="Node inspector"
        className={cn(
          "w-full shrink-0 flex-col overflow-hidden rounded-xl border border-border bg-card",
          flow.selected === null ? "hidden" : "flex h-[340px]",
        )}
      >
        {flow.selected === null ? (
          <div className="flex h-full min-h-[160px] flex-col items-center justify-center px-5 py-6 text-center">
            <span className="flex h-10 w-10 items-center justify-center rounded-full border border-border bg-surface-subtle text-muted-foreground">
              <MousePointerClick className="h-4 w-4" />
            </span>
            <p className="mt-2 text-[15px] font-semibold leading-[1.55] text-foreground">
              Select a node
            </p>
            <p className="mt-1 max-w-[240px] text-[13px] leading-5 text-muted-foreground">
              Click any card to inspect its full label, state, and ids.
            </p>
          </div>
        ) : (
          <TaskGraphInspector
            node={flow.selected}
            work={work}
            onClose={() => setSelectedNodeId(null)}
          />
        )}
      </aside>
    </div>
  );
}
