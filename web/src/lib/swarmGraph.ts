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

// ---------------------------------------------------------------------------
// Team messages (`GET /api/tasks/{id}/team/messages`)
// ---------------------------------------------------------------------------

/** One team message prepared for the graph. */
export type TeamGraphMessage = {
  /** Stable React key/selection id (the wire id, stringified). */
  id: string;
  from: string;
  /** Direct recipient; `null` means a team broadcast. */
  to: string | null;
  subject: string;
  body: string;
  sentAt: string;
  broadcast: boolean;
};

export type TeamGraphChip = {
  id: string;
  message: TeamGraphMessage;
  /** `sender → recipient · subject`, ready for the chip button. */
  label: string;
  selected: boolean;
};

/**
 * Same node/edge shape as the legacy swarm graph so the existing React Flow
 * layout and card rendering are reused unchanged.
 */
export type TeamGraphModel = {
  nodes: SwarmGraphNode[];
  edges: SwarmGraphEdge[];
  chips: TeamGraphChip[];
  roundLabel: string;
  selected: TeamGraphMessage | null;
  sender: string | null;
  /** Direct target, or every other participant for a broadcast. */
  recipients: string[];
  peerCount: number;
  layout: SwarmGraphLayout;
  color: string;
  broadcast: boolean;
};

const TEAM_GRAPH_LABEL = "Team messages";

function nonEmptyText(value: unknown): string | null {
  return typeof value === "string" && value.trim() !== "" ? value.trim() : null;
}

function firstLineText(text: string): string | null {
  const line = text.split("\n")[0]?.trim() ?? "";
  if (line === "") {
    return null;
  }
  return line.length > 100 ? `${line.slice(0, 99)}…` : line;
}

/**
 * Maps one wire row from `/team/messages` onto the graph shape. Accepts the
 * canonical `from_agent_id` / `to_agent_id` fields plus the common aliases so
 * a slightly older response still renders.
 */
export function parseTeamGraphMessage(
  raw: unknown,
): TeamGraphMessage | null {
  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) {
    return null;
  }
  const row = raw as Record<string, unknown>;
  const id =
    typeof row.id === "number" && Number.isSafeInteger(row.id) && row.id >= 0
      ? String(row.id)
      : typeof row.id === "string" && row.id.trim() !== ""
        ? row.id.trim()
        : "";
  if (id === "") {
    return null;
  }

  const from =
    nonEmptyText(row.from_agent_id) ??
    nonEmptyText(row.sender) ??
    nonEmptyText(row.from) ??
    "unknown";
  const rawTo =
    nonEmptyText(row.to_agent_id) ??
    nonEmptyText(row.recipient) ??
    nonEmptyText(row.to);
  const to =
    rawTo === null || rawTo.toLowerCase() === "all" ? null : rawTo;
  const body =
    nonEmptyText(row.body) ??
    nonEmptyText(row.text) ??
    nonEmptyText(row.message) ??
    "";
  const subject =
    nonEmptyText(row.subject) ??
    nonEmptyText(row.title) ??
    firstLineText(body) ??
    "(no subject)";
  const sentAt =
    nonEmptyText(row.sent_at) ??
    nonEmptyText(row.created_at) ??
    nonEmptyText(row.time) ??
    "";

  return {
    id,
    from,
    to,
    subject,
    body,
    sentAt,
    broadcast: to === null,
  };
}

/** Parses every usable team message, preserving server order. */
export function mapTeamMessages(
  messages: readonly unknown[],
): TeamGraphMessage[] {
  const views: TeamGraphMessage[] = [];
  for (const message of messages) {
    const view = parseTeamGraphMessage(message);
    if (view !== null) {
      views.push(view);
    }
  }
  return views;
}

function compareTeamAgents(a: string, b: string): number {
  if (a === "unknown") {
    return b === "unknown" ? 0 : 1;
  }
  if (b === "unknown") {
    return -1;
  }
  return a < b ? -1 : a > b ? 1 : 0;
}

/**
 * Builds the graph for all team messages and one selected message.
 *
 * - every sender and every direct recipient becomes a node;
 * - a direct message draws one edge from `from_agent_id` to `to_agent_id`;
 * - a broadcast (`to_agent_id` null) fans out from the sender to every other
 *   participant;
 * - chips list every message so selecting one redraws the edges;
 * - node positions reuse the legacy circle/grid layout.
 */
export function buildTeamMessageGraph(
  messages: readonly unknown[],
  selectedMessageId?: string | number | null,
): TeamGraphModel {
  const views = mapTeamMessages(messages);
  const participants = [
    ...new Set(
      views.flatMap((message) =>
        message.to === null ? [message.from] : [message.from, message.to],
      ),
    ),
  ].sort(compareTeamAgents);

  const counts = new Map<string, number>();
  for (const message of views) {
    counts.set(message.from, (counts.get(message.from) ?? 0) + 1);
  }

  const selected =
    (selectedMessageId === null || selectedMessageId === undefined
      ? undefined
      : views.find(
          (message) => message.id === String(selectedMessageId),
        )) ??
    views[0] ??
    null;

  const sender = selected === null ? null : selected.from;
  let recipients: string[] = [];
  if (selected !== null) {
    if (selected.broadcast) {
      recipients = participants.filter((worker) => worker !== sender);
    } else if (selected.to !== null && selected.to !== sender) {
      recipients = [selected.to];
    }
  }
  const recipientSet = new Set(recipients);
  const color = SWARM_DEFAULT_EDGE_COLOR;
  const { layout, positions } = workerLayout(participants.length);

  const nodes: SwarmGraphNode[] = participants.map((worker, index) => ({
    id: swarmNodeId(worker),
    position: positions[index] ?? { x: 0, y: 0 },
    data: {
      sender: worker,
      count: counts.get(worker) ?? 0,
      selected: sender !== null && worker === sender,
      recipient: recipientSet.has(worker),
    },
  }));

  const edges: SwarmGraphEdge[] =
    selected === null || sender === null
      ? []
      : recipients.map((recipient) => ({
          id: `team-message-${selected.id}-to-${swarmNodeId(recipient)}`,
          source: swarmNodeId(sender),
          target: swarmNodeId(recipient),
          kind: selected.broadcast ? "broadcast" : "direct",
          color,
        }));

  const chips: TeamGraphChip[] = views.map((message) => ({
    id: message.id,
    message,
    label: `${message.from} → ${message.to ?? "all"} · ${message.subject}`,
    selected: selected !== null && message.id === selected.id,
  }));

  return {
    nodes,
    edges,
    chips,
    roundLabel: TEAM_GRAPH_LABEL,
    selected,
    sender,
    recipients,
    peerCount: Math.max(0, participants.length - 1),
    layout,
    color,
    broadcast: selected !== null && selected.broadcast,
  };
}
