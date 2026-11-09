/**
 * Pure model for the swarm message graph.
 *
 * React Flow needs plain nodes, edges, and positions; keeping that here (and
 * out of the component) lets `swarmGraph.test.ts` assert the broadcast shape,
 * the selected/recipient highlighting, and both layouts without a DOM.
 */

import {
  swarmKindLabel,
  swarmRoundLabel,
  type SwarmMessageView,
} from "./workTeam.ts";
import type { SwarmRoundGroup, SwarmRunGroup } from "./workTeam.ts";

/** Circle for six workers or fewer; a centered grid above that. */
export type SwarmGraphLayout = "circle" | "grid";

export const SWARM_CIRCLE_LIMIT = 6;
export const SWARM_DEFAULT_EDGE_COLOR = "#64748b";

/** Broadcast edge color by persisted message kind. */
export const SWARM_EDGE_COLORS: Readonly<Record<string, string>> = {
  finding: "#16a34a",
  question: "#2563eb",
  challenge: "#d97706",
};

/** Color for a message kind; unknown kinds stay neutral. */
export function swarmEdgeColor(kind: string): string {
  return SWARM_EDGE_COLORS[kind.trim().toLowerCase()] ?? SWARM_DEFAULT_EDGE_COLOR;
}

/** Stable React Flow id for a worker; sender names may contain punctuation. */
export function swarmNodeId(sender: string): string {
  return `worker:${sender}`;
}

export type SwarmGraphNodeData = {
  sender: string;
  /** Messages sent by this worker across the whole selected run. */
  count: number;
  /** True for the selected message's sender. */
  selected: boolean;
  /** True for every other worker when a message is selected. */
  recipient: boolean;
};

export type SwarmGraphNode = {
  id: string;
  position: { x: number; y: number };
  data: SwarmGraphNodeData;
};

export type SwarmGraphEdge = {
  id: string;
  source: string;
  target: string;
  kind: string;
  color: string;
};

export type SwarmGraphChip = {
  id: number;
  message: SwarmMessageView;
  /** `sender · Kind`, ready for the chip button. */
  label: string;
  repeat: boolean;
  selected: boolean;
};

export type SwarmGraphModel = {
  nodes: SwarmGraphNode[];
  edges: SwarmGraphEdge[];
  chips: SwarmGraphChip[];
  /** Shared round label; matches the run's selector (`Round 1 (seed)`, …). */
  roundLabel: string;
  selected: SwarmMessageView | null;
  /** Sender of the selected message, or null when the round is empty. */
  sender: string | null;
  /** Every other worker in the run, in the run's sender order. */
  recipients: string[];
  peerCount: number;
  layout: SwarmGraphLayout;
  /** Edge/selection color for the selected message (neutral when none). */
  color: string;
};

const CIRCLE_RADIUS = 190;
const GRID_COLUMN_GAP = 200;
const GRID_ROW_GAP = 110;

/**
 * Lays workers out in a circle (readable for small swarms) or a centered grid
 * (so a large run still fits `fitView` without overlapping nodes).
 */
function workerLayout(senderCount: number): {
  layout: SwarmGraphLayout;
  positions: Array<{ x: number; y: number }>;
} {
  if (senderCount <= SWARM_CIRCLE_LIMIT) {
    const positions = Array.from({ length: senderCount }, (_, index) => {
      const angle = -Math.PI / 2 + (2 * Math.PI * index) / senderCount;
      return {
        x: Math.round(CIRCLE_RADIUS * Math.cos(angle)),
        y: Math.round(CIRCLE_RADIUS * Math.sin(angle)),
      };
    });
    return { layout: "circle", positions };
  }

  const columns = Math.ceil(Math.sqrt(senderCount));
  const rows = Math.ceil(senderCount / columns);
  const positions = Array.from({ length: senderCount }, (_, index) => {
    const column = index % columns;
    const row = Math.floor(index / columns);
    return {
      x: Math.round((column - (columns - 1) / 2) * GRID_COLUMN_GAP),
      y: Math.round((row - (rows - 1) / 2) * GRID_ROW_GAP),
    };
  });
  return { layout: "grid", positions };
}

/** Per-sender message counts across the whole run. */
function senderCounts(run: SwarmRunGroup): Map<string, number> {
  const counts = new Map<string, number>();
  for (const round of run.rounds) {
    for (const group of round.senders) {
      counts.set(
        group.sender,
        (counts.get(group.sender) ?? 0) + group.messages.length,
      );
    }
  }
  return counts;
}

/**
 * Builds the graph for one run, one round, and one selected message.
 *
 * - every distinct sender in the run becomes a node;
 * - chips are the selected round's messages in id order;
 * - a missing/invalid selected id falls back to the round's first message;
 * - the only edges are the selected message's broadcast from its sender to
 *   every other worker in the run, colored by kind;
 * - `selected`/`recipient` ride on every node so the UI can highlight them.
 */
export function buildSwarmGraph(
  run: SwarmRunGroup,
  round: SwarmRoundGroup,
  selectedMessageId?: number | null,
): SwarmGraphModel {
  const roundMessages = round.senders
    .flatMap((group) => group.messages)
    .sort((a, b) => a.id - b.id);

  const selected =
    (selectedMessageId === null || selectedMessageId === undefined
      ? undefined
      : roundMessages.find((message) => message.id === selectedMessageId)) ??
    roundMessages[0] ??
    null;

  const sender = selected === null ? null : (selected.from ?? "unknown");
  const recipients =
    sender === null ? [] : run.senders.filter((worker) => worker !== sender);
  const color =
    selected === null ? SWARM_DEFAULT_EDGE_COLOR : swarmEdgeColor(selected.kind);

  const counts = senderCounts(run);
  const { layout, positions } = workerLayout(run.senders.length);

  const nodes: SwarmGraphNode[] = run.senders.map((worker, index) => ({
    id: swarmNodeId(worker),
    position: positions[index] ?? { x: 0, y: 0 },
    data: {
      sender: worker,
      count: counts.get(worker) ?? 0,
      selected: sender !== null && worker === sender,
      recipient: sender !== null && worker !== sender,
    },
  }));

  const edges: SwarmGraphEdge[] =
    selected === null || sender === null
      ? []
      : recipients.map((recipient) => ({
          id: `message-${selected.id}-to-${swarmNodeId(recipient)}`,
          source: swarmNodeId(sender),
          target: swarmNodeId(recipient),
          kind: selected.kind,
          color,
        }));

  const chips: SwarmGraphChip[] = roundMessages.map((message) => ({
    id: message.id,
    message,
    label: `${message.from ?? "unknown"} · ${swarmKindLabel(message.kind)}`,
    repeat: message.repeatOf !== null,
    selected: selected !== null && message.id === selected.id,
  }));

  return {
    nodes,
    edges,
    chips,
    roundLabel: round.label || swarmRoundLabel(round.round),
    selected,
    sender,
    recipients,
    peerCount: Math.max(0, run.senders.length - 1),
    layout,
    color,
  };
}
