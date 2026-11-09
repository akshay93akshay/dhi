"use client";

import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type MouseEvent,
  type ReactNode,
} from "react";
import { MessageSquareText, RefreshCw } from "lucide-react";
import {
  Background,
  Controls,
  MarkerType,
  MiniMap,
  Position,
  ReactFlow,
  type Edge,
  type Node,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";

import { getSwarmMessages } from "@/lib/api";
import { formatLocalClock, formatLocalTimestamp } from "@/lib/localTime";
import { describeApiError } from "@/lib/ui";
import { cn } from "@/lib/utils";
import {
  groupSwarmRuns,
  parseSwarmMessages,
  swarmKindLabel,
  swarmMessageCounts,
  swarmRoundLabel,
  type SwarmMessageView,
} from "@/lib/workTeam";
import {
  buildSwarmGraph,
  swarmEdgeColor,
  type SwarmGraphModel,
} from "@/lib/swarmGraph";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { CopyButton } from "@/components/ui/copy-button";
import { MarkdownMath } from "@/components/markdown-math";
import { useColorMode } from "@/components/use-color-mode";

/** Badge tone per persisted swarm kind; unknown kinds stay neutral. */
function kindVariant(kind: string): "success" | "info" | "warning" | "muted" {
  switch (kind.toLowerCase()) {
    case "finding":
      return "success";
    case "question":
      return "info";
    case "challenge":
      return "warning";
    default:
      return "muted";
  }
}

function KindCounts(props: { counts: Record<string, number> }) {
  const entries = Object.entries(props.counts).sort(([a], [b]) =>
    a < b ? -1 : a > b ? 1 : 0,
  );
  return (
    <>
      {entries.map(([kind, count]) => (
        <Badge key={kind} variant={kindVariant(kind)} className="font-normal">
          {count} {swarmKindLabel(kind).toLowerCase()}
        </Badge>
      ))}
    </>
  );
}

function recipientText(peerCount: number): string {
  return `broadcast to ${peerCount} peer${peerCount === 1 ? "" : "s"}`;
}

/** Small pill used by the run and round selectors. */
function SelectorButton(props: {
  active: boolean;
  children: ReactNode;
  onClick: () => void;
}) {
  return (
    <Button
      aria-pressed={props.active}
      className={cn(
        "h-8 rounded-full px-2.5 type-label",
        props.active
          ? "border-accent-foreground/30 bg-accent text-accent-foreground hover:bg-accent"
          : "text-muted-foreground",
      )}
      onClick={props.onClick}
      size="sm"
      type="button"
      variant={props.active ? "secondary" : "outline"}
    >
      {props.children}
    </Button>
  );
}

/** Data attached to every React Flow worker node. */
type WorkerNodeData = {
  label: ReactNode;
  sender: string;
  count: number;
  selected: boolean;
  recipient: boolean;
};

/**
 * Fixed node box. The height fits the sender chip, the message count, and the
 * optional sender/recipient line (with padding and border); the height is
 * declared on the node as well as its style so the MiniMap can draw it.
 */
const SWARM_NODE_WIDTH = 176;
const SWARM_NODE_HEIGHT = 88;

/** Default React Flow nodes with worker labels and selected/recipient tone. */
function buildFlowNodes(model: SwarmGraphModel): Node<WorkerNodeData>[] {
  return model.nodes.map((node) => {
    const { sender, count, selected, recipient } = node.data;
    const highlighted = selected || recipient;
    const status = selected ? "sender" : recipient ? "recipient" : null;
    return {
      id: node.id,
      position: node.position,
      width: SWARM_NODE_WIDTH,
      height: SWARM_NODE_HEIGHT,
      data: {
        sender,
        count,
        selected,
        recipient,
        label: (
          <div className="flex h-full w-[150px] flex-col items-center justify-center gap-1">
            <span className="mono max-w-full truncate font-semibold leading-tight">
              {sender}
            </span>
            <span className="type-label text-muted-foreground">
              {count} message{count === 1 ? "" : "s"}
            </span>
            {status !== null ? (
              <span
                className="type-label font-semibold uppercase tracking-[0.08em]"
                style={{ color: model.color }}
              >
                {status}
              </span>
            ) : null}
          </div>
        ),
      },
      style: {
        width: SWARM_NODE_WIDTH,
        height: SWARM_NODE_HEIGHT,
        cursor: "pointer",
        padding: "8px 10px",
        borderRadius: 12,
        border: `1.5px solid ${highlighted ? model.color : "var(--border)"}`,
        background: selected
          ? `color-mix(in srgb, ${model.color} 16%, var(--card))`
          : "var(--card)",
        color: "var(--card-foreground)",
        boxShadow: selected
          ? `0 0 0 3px color-mix(in srgb, ${model.color} 25%, transparent)`
          : undefined,
        opacity: model.selected !== null && !highlighted ? 0.72 : 1,
        transition: "opacity 120ms ease, border-color 120ms ease",
      },
      sourcePosition: Position.Right,
      targetPosition: Position.Left,
      draggable: false,
    };
  });
}

/** Broadcast edges: only the selected message, from sender to every peer. */
function buildFlowEdges(model: SwarmGraphModel): Edge[] {
  return model.edges.map((edge) => ({
    id: edge.id,
    source: edge.source,
    target: edge.target,
    type: "smoothstep",
    style: { stroke: edge.color, strokeWidth: 2 },
    markerEnd: {
      type: MarkerType.ArrowClosed,
      color: edge.color,
      width: 18,
      height: 18,
    },
  }));
}

/**
 * One selected message: type, sender, recipients, full body, and metadata.
 * Ids and timestamps stay plain text; only the kind uses a badge.
 */
function SwarmMessageDetail(props: { model: SwarmGraphModel }) {
  const { model } = props;
  const message = model.selected;
  if (message === null) {
    return null;
  }
  const from = message.from ?? "unknown";
  return (
    <div className="rounded-lg border border-border bg-surface-subtle p-4">
      <div className="flex flex-wrap items-center gap-x-2 gap-y-1.5">
        <Badge className="font-normal" variant={kindVariant(message.kind)}>
          {swarmKindLabel(message.kind)}
        </Badge>
        <span className="mono font-semibold">{from}</span>
        <span className="type-meta text-muted-foreground">
          {recipientText(model.recipients.length)}
        </span>
        {message.repeatOf !== null ? (
          <Badge className="font-normal" variant="muted">
            repeat
          </Badge>
        ) : null}
        <CopyButton
          className="ml-auto"
          label="Copy"
          size="sm"
          text={message.fullText}
        />
      </div>
      <MarkdownMath
        className="mt-2 max-h-96 overflow-auto type-goal [&_p+p]:mt-2 [&_ul]:my-1.5 [&_ol]:my-1.5 [&_li]:my-0.5 [&_pre]:my-1.5 [&_blockquote]:my-1.5 [&_.katex-display]:my-1.5"
        text={message.fullText}
        variant="block"
      />
      <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 type-meta text-muted-foreground">
        <span>
          id <span className="mono text-foreground">#{message.id}</span>
        </span>
        <span>
          intent{" "}
          <span className="mono" title={message.intentId || undefined}>
            {message.intentShort === "" ? "—" : message.intentShort}
          </span>
        </span>
        {message.subtaskId !== null ? (
          <span>
            subtask <span className="mono">{message.subtaskId}</span>
          </span>
        ) : null}
        {message.repeatOf !== null ? (
          <span>
            repeat of <span className="mono">#{message.repeatOf}</span>
          </span>
        ) : null}
        <span
          title={
            message.createdAt === ""
              ? undefined
              : formatLocalTimestamp(message.createdAt)
          }
        >
          {message.createdAt === ""
            ? "—"
            : formatLocalClock(message.createdAt)}
        </span>
      </div>
    </div>
  );
}

/**
 * Audit view of a task's DHI+ swarm broadcasts.
 *
 * Runs are grouped by `intent_id`; within a run the round selector scopes the
 * message chips. Selecting a chip (or clicking a worker, which picks that
 * worker's first message in the round) draws only that one broadcast in the
 * graph, from its sender to every other worker. There is no polling: fetch on
 * mount, refresh manually.
 */
export function SwarmMessagesCard(props: { taskId: string }) {
  const { taskId } = props;
  const [messages, setMessages] = useState<SwarmMessageView[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [runIndex, setRunIndex] = useState(0);
  const [roundIndex, setRoundIndex] = useState(0);
  const [selectedMessageId, setSelectedMessageId] = useState<number | null>(
    null,
  );
  const colorMode = useColorMode();

  const mounted = useRef(true);
  const seq = useRef(0);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  const load = useCallback(async (): Promise<void> => {
    const token = ++seq.current;
    setLoading(true);
    setError(null);
    try {
      const records = await getSwarmMessages(taskId);
      if (!mounted.current || token !== seq.current) {
        return;
      }
      setMessages(parseSwarmMessages(records));
    } catch (err) {
      if (!mounted.current || token !== seq.current) {
        return;
      }
      setError(describeApiError(err));
    } finally {
      if (mounted.current && token === seq.current) {
        setLoading(false);
      }
    }
  }, [taskId]);

  // Reset local state when the route moves to another task; the parent also
  // keys this card, but the guard keeps it correct on its own.
  useEffect(() => {
    seq.current += 1;
    setMessages(null);
    setLoading(false);
    setError(null);
    setRunIndex(0);
    setRoundIndex(0);
    setSelectedMessageId(null);
  }, [taskId]);

  useEffect(() => {
    void load();
  }, [load]);

  const runs = useMemo(
    () => (messages === null ? [] : groupSwarmRuns(messages)),
    [messages],
  );
  const counts = useMemo(
    () => swarmMessageCounts(messages ?? []),
    [messages],
  );

  const activeRunIndex =
    runs.length === 0 ? 0 : Math.min(runIndex, runs.length - 1);
  const selectedRun = runs[activeRunIndex] ?? null;
  const activeRoundIndex =
    selectedRun === null
      ? 0
      : Math.min(roundIndex, Math.max(0, selectedRun.rounds.length - 1));
  const selectedRound = selectedRun?.rounds[activeRoundIndex] ?? null;

  const model = useMemo(
    () =>
      selectedRun === null || selectedRound === null
        ? null
        : buildSwarmGraph(selectedRun, selectedRound, selectedMessageId),
    [selectedRun, selectedRound, selectedMessageId],
  );
  const flowNodes = useMemo(
    () => (model === null ? [] : buildFlowNodes(model)),
    [model],
  );
  const flowEdges = useMemo(
    () => (model === null ? [] : buildFlowEdges(model)),
    [model],
  );

  const handleNodeClick = useCallback(
    (_event: MouseEvent, node: Node) => {
      if (selectedRound === null) {
        return;
      }
      const sender = (node.data as WorkerNodeData).sender;
      const first = selectedRound.senders.find(
        (group) => group.sender === sender,
      )?.messages[0];
      if (first !== undefined) {
        setSelectedMessageId(first.id);
      }
    },
    [selectedRound],
  );

  return (
    <Card>
      <CardHeader className="flex-row flex-wrap items-start justify-between gap-3 border-b border-border pb-4">
        <div className="flex items-start gap-2.5">
          <span className="flex h-8 w-8 items-center justify-center rounded-lg bg-accent text-accent-foreground">
            <MessageSquareText className="h-4 w-4" />
          </span>
          <div>
            <CardTitle>Swarm messages</CardTitle>
            <CardDescription className="mt-0.5">
              Who broadcast what to whom, per run and round.
            </CardDescription>
          </div>
        </div>
        <div className="flex items-center gap-2">
          {messages !== null ? (
            <Badge variant="outline" className="font-mono font-normal">
              {counts.total}
            </Badge>
          ) : null}
          <Button
            className="h-8 rounded-full"
            disabled={loading}
            onClick={() => void load()}
            size="sm"
            type="button"
            variant="outline"
          >
            <RefreshCw
              className={`h-3.5 w-3.5 ${loading ? "animate-spin" : ""}`}
            />
            {loading ? "Loading…" : messages !== null ? "Refresh" : "Load"}
          </Button>
        </div>
      </CardHeader>
      <CardContent className="pt-4">
        {error !== null ? (
          <div className="errorbox mt-0" role="alert">
            Could not load swarm messages: {error}
          </div>
        ) : null}

        {loading && messages === null ? (
          <p className="empty">Loading swarm messages…</p>
        ) : null}

        {messages !== null && messages.length === 0 ? (
          <div className="rounded-lg border border-dashed border-border bg-surface-subtle px-4 py-7 text-center type-meta text-muted-foreground">
            No swarm messages recorded for this task.
          </div>
        ) : null}

        {messages !== null && messages.length > 0 ? (
          <>
            <div className="mb-3 flex flex-wrap items-center gap-1.5">
              <Badge variant="outline" className="font-normal">
                {counts.total} message{counts.total === 1 ? "" : "s"}
              </Badge>
              <Badge variant="outline" className="font-normal">
                {counts.workers} sender{counts.workers === 1 ? "" : "s"}
              </Badge>
              <Badge variant="outline" className="font-normal">
                {counts.rounds} round{counts.rounds === 1 ? "" : "s"}
              </Badge>
              <KindCounts counts={counts.byKind} />
            </div>

            {selectedRun !== null ? (
              <div className="space-y-2.5">
                {runs.length > 1 ? (
                  <div className="flex flex-wrap items-center gap-1.5">
                    <span className="type-label font-semibold uppercase tracking-[0.08em] text-muted-foreground">
                      Run
                    </span>
                    {runs.map((run, index) => (
                      <SelectorButton
                        key={run.runId || `run-${index}`}
                        active={index === activeRunIndex}
                        onClick={() => {
                          setRunIndex(index);
                          setRoundIndex(0);
                          setSelectedMessageId(null);
                        }}
                      >
                        {run.label}
                      </SelectorButton>
                    ))}
                  </div>
                ) : null}

                {selectedRun.rounds.length > 1 ? (
                  <div className="flex flex-wrap items-center gap-1.5">
                    <span className="type-label font-semibold uppercase tracking-[0.08em] text-muted-foreground">
                      Round
                    </span>
                    {selectedRun.rounds.map((round, index) => (
                      <SelectorButton
                        key={`round-${round.round ?? "unknown"}`}
                        active={index === activeRoundIndex}
                        onClick={() => {
                          setRoundIndex(index);
                          setSelectedMessageId(null);
                        }}
                      >
                        {swarmRoundLabel(round.round)}
                      </SelectorButton>
                    ))}
                  </div>
                ) : null}

                {model !== null ? (
                  <>
                    <div
                      aria-label="Messages in the selected round"
                      className="flex flex-wrap gap-1.5"
                      role="group"
                    >
                      {model.chips.map((chip) => (
                        <button
                          key={chip.id}
                          aria-pressed={chip.selected}
                          className={cn(
                            "inline-flex items-center gap-1.5 rounded-full border px-2.5 py-1.5 type-label transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
                            chip.selected
                              ? "border-accent-foreground/40 bg-accent text-accent-foreground"
                              : "border-border bg-card text-foreground hover:bg-surface-subtle",
                          )}
                          onClick={() => setSelectedMessageId(chip.id)}
                          title={chip.label}
                          type="button"
                        >
                          <span
                            aria-hidden="true"
                            className="h-1.5 w-1.5 shrink-0 rounded-full"
                            style={{
                              backgroundColor: swarmEdgeColor(chip.message.kind),
                            }}
                          />
                          <span className="mono font-medium">
                            {chip.message.from ?? "unknown"}
                          </span>
                          <span aria-hidden="true" className="text-muted-foreground">
                            ·
                          </span>
                          <span>{swarmKindLabel(chip.message.kind)}</span>
                          {chip.repeat ? (
                            <Badge
                              className="px-1.5 py-0 type-badge font-normal"
                              variant="muted"
                            >
                              repeat
                            </Badge>
                          ) : null}
                        </button>
                      ))}
                    </div>

                    <div
                      aria-label={`${model.roundLabel} message graph`}
                      className="h-[420px] w-full overflow-hidden rounded-lg border border-border bg-surface-subtle"
                      role="group"
                    >
                      <ReactFlow
                        key={selectedRun.runId || `run-${activeRunIndex}`}
                        className="dhi-flow h-full w-full"
                        colorMode={colorMode}
                        edges={flowEdges}
                        elementsSelectable={false}
                        fitView
                        fitViewOptions={{ padding: 0.25 }}
                        maxZoom={1.5}
                        minZoom={0.35}
                        nodes={flowNodes}
                        nodesConnectable={false}
                        nodesDraggable={false}
                        onNodeClick={handleNodeClick}
                      >
                        <Background color="var(--border)" gap={20} size={1} />
                        <Controls showInteractive={false} />
                        <MiniMap
                          nodeColor={(node) => {
                            const data = node.data as WorkerNodeData;
                            if (data.selected) {
                              return model.color;
                            }
                            return data.recipient ? "#94a3b8" : "var(--border)";
                          }}
                          nodeStrokeWidth={2}
                          pannable
                          zoomable
                        />
                      </ReactFlow>
                    </div>

                    <SwarmMessageDetail model={model} />
                  </>
                ) : null}
              </div>
            ) : null}
          </>
        ) : null}
      </CardContent>
    </Card>
  );
}
