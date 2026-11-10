/**
 * Typed Swarm team state plus defensive wire parsers.
 *
 * The read-only team routes are still settling, so every parser accepts the
 * canonical shape and the common aliases without throwing. Keep this module
 * React-free: `team.test.ts` drives every branch under `node --test`.
 *
 * Canonical envelopes (all `GET /api/tasks/{id}/team…`):
 * - `/team`            `{ team: TeamSummary }`
 * - `/team/tasks`      `{ tasks: TeamTask[] }`
 * - `/team/mailbox`    `{ messages: TeamMessage[] }`
 * - `/team/messages`   `{ messages: TeamMessage[] }` (both directions)
 * - `/team/mission`    `{ mission: MissionEntry[] }`
 * - `/team/runs`       `{ runs: TeamRun[] }`
 * - `/team/outcomes`   `{ outcomes: TeamOutcome[] }`
 * - `/api/scale-approvals` `{ approvals: ScaleApproval[] }`
 */

import type { JsonObject } from "./types.ts";

// ---------------------------------------------------------------------------
// Shared defensive helpers
// ---------------------------------------------------------------------------

export function isRecord(value: unknown): value is JsonObject {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** First non-empty string among the given keys (or `fallback`). */
function textField(
  row: JsonObject,
  keys: readonly string[],
  fallback = "",
): string {
  for (const key of keys) {
    const value = row[key];
    if (typeof value === "string" && value.trim() !== "") {
      return value;
    }
  }
  return fallback;
}

/** Stable string form of a string/number id; anything else is `""`. */
function identifier(value: unknown): string {
  if (typeof value === "string") {
    return value.trim();
  }
  if (typeof value === "number" && Number.isFinite(value)) {
    return String(value);
  }
  return "";
}

/** First usable id among several field names. */
function idField(row: JsonObject, keys: readonly string[]): string {
  for (const key of keys) {
    const id = identifier(row[key]);
    if (id !== "") {
      return id;
    }
  }
  return "";
}

/** Optional RFC 3339 timestamp. */
function timeField(row: JsonObject, keys: readonly string[]): string | null {
  for (const key of keys) {
    const value = row[key];
    if (typeof value === "string" && value.trim() !== "") {
      return value;
    }
  }
  return null;
}

function numberField(row: JsonObject, keys: readonly string[]): number | null {
  for (const key of keys) {
    const value = row[key];
    if (typeof value === "number" && Number.isFinite(value)) {
      return value;
    }
    if (typeof value === "string" && value.trim() !== "") {
      const parsed = Number(value);
      if (Number.isFinite(parsed)) {
        return parsed;
      }
    }
  }
  return null;
}

function booleanField(
  row: JsonObject,
  keys: readonly string[],
): boolean | null {
  for (const key of keys) {
    const value = row[key];
    if (typeof value === "boolean") {
      return value;
    }
  }
  return null;
}

/** Agent identity from a string or an object like `{ key }` / `{ name }`. */
function agentName(value: unknown): string {
  if (typeof value === "string") {
    return value.trim();
  }
  if (isRecord(value)) {
    return textField(value, [
      "agent_id",
      "agent_key",
      "agent",
      "name",
      "key",
      "worker",
    ]);
  }
  return "";
}

/** Ids/titles from an array, comma string, or single object. */
function stringList(value: unknown): string[] {
  if (Array.isArray(value)) {
    const out: string[] = [];
    for (const item of value) {
      const text = agentName(item) || identifier(item);
      if (text !== "") {
        out.push(text);
      }
    }
    return out;
  }
  if (typeof value === "string") {
    const trimmed = value.trim();
    if (trimmed === "") {
      return [];
    }
    return trimmed
      .split(",")
      .map((part) => part.trim())
      .filter((part) => part !== "");
  }
  const single = agentName(value) || identifier(value);
  return single === "" ? [] : [single];
}

/** Envelope list from one of several keys, or a bare array. */
function listFrom(
  raw: unknown,
  keys: readonly string[],
): readonly unknown[] {
  if (Array.isArray(raw)) {
    return raw;
  }
  if (!isRecord(raw)) {
    return [];
  }
  for (const key of keys) {
    const value = raw[key];
    if (Array.isArray(value)) {
      return value;
    }
  }
  return [];
}

// ---------------------------------------------------------------------------
// Tabs
// ---------------------------------------------------------------------------

export type SwarmTabId =
  | "board"
  | "mailbox"
  | "mission"
  | "outcomes"
  | "runs"
  | "graph";

export type SwarmTab = {
  id: SwarmTabId;
  label: string;
  /** One-line tab description used by the workspace panel. */
  description: string;
};

/** Tabs in contract order. The task board is deliberately first/default. */
export const SWARM_TABS: readonly SwarmTab[] = [
  {
    id: "board",
    label: "Task board",
    description: "Shared work items, owners, dependencies, and blockers.",
  },
  {
    id: "mailbox",
    label: "Mailbox",
    description: "Direct and broadcast team messages, Inbox and Sent.",
  },
  {
    id: "mission",
    label: "Mission log",
    description: "Progress, handoffs, blockers, decisions, and failures.",
  },
  {
    id: "outcomes",
    label: "Outcomes",
    description: "Required sections, fragments, review, and finalization.",
  },
  {
    id: "runs",
    label: "Runs",
    description: "Agent execution, progress, and result summaries.",
  },
  {
    id: "graph",
    label: "Graph",
    description: "Secondary view: who messaged whom, direct and broadcast.",
  },
] as const;

export const DEFAULT_SWARM_TAB: SwarmTabId = "board";

export function isSwarmTabId(value: unknown): value is SwarmTabId {
  return SWARM_TABS.some((tab) => tab.id === value);
}

// ---------------------------------------------------------------------------
// Team summary / header
// ---------------------------------------------------------------------------

export type TeamMemberRole = "lead" | "teammate";

export type TeamMember = {
  id: string;
  agent: string;
  role: TeamMemberRole;
  status: string;
  joined_at: string | null;
};

export type TeamSummary = {
  team_id: string;
  task_id: string;
  status: string;
  lead: string | null;
  members: TeamMember[];
  /** Teammates only; the lead is extra and never counted. */
  worker_count: number;
};

function normalizeMemberRole(value: unknown): TeamMemberRole {
  const role = typeof value === "string" ? value.trim().toLowerCase() : "";
  return role === "lead" || role === "manager" ? "lead" : "teammate";
}

export function parseTeamMember(
  raw: unknown,
  index = 0,
): TeamMember | null {
  if (!isRecord(raw)) {
    return null;
  }
  const agent = agentName(raw) || `worker-${index + 1}`;
  return {
    id: idField(raw, ["id", "member_id", "agent_id"]) || `member-${index}`,
    agent,
    role: normalizeMemberRole(raw.role),
    status: textField(raw, ["status", "state"], "active"),
    joined_at: timeField(raw, ["joined_at", "created_at", "started_at"]),
  };
}

/** Parses `GET /team` (`{ team: {...} }`, flat, or `{ team, members }`). */
export function parseTeamSummary(raw: unknown): TeamSummary | null {
  if (!isRecord(raw)) {
    return null;
  }
  const team = isRecord(raw.team) ? raw.team : raw;
  const memberSource = Array.isArray(team.members)
    ? team.members
    : Array.isArray(raw.members)
      ? raw.members
      : [];
  const members = memberSource
    .map((member, index) => parseTeamMember(member, index))
    .filter((member): member is TeamMember => member !== null);

  const reportedWorkers =
    numberField(team, ["worker_count", "workers", "teammate_count"]) ??
    numberField(raw, ["worker_count", "workers", "teammate_count"]);
  const teammateCount = members.filter(
    (member) => member.role === "teammate",
  ).length;
  const workerCount =
    reportedWorkers !== null && reportedWorkers >= 0
      ? Math.floor(reportedWorkers)
      : teammateCount;

  const lead =
    agentName(team.lead) ||
    agentName(team.lead_agent) ||
    agentName(raw.lead) ||
    agentName(raw.lead_agent) ||
    members.find((member) => member.role === "lead")?.agent ||
    null;

  const hasTeamState =
    team.team_id !== undefined ||
    team.id !== undefined ||
    team.status !== undefined ||
    members.length > 0 ||
    reportedWorkers !== null ||
    Object.keys(team).some((key) => key !== "team");
  if (!hasTeamState) {
    return null;
  }

  return {
    team_id: idField(team, ["team_id", "id"]),
    task_id: idField(team, ["task_id", "id"]),
    status: textField(team, ["status", "state"], "active"),
    lead,
    members,
    worker_count: workerCount,
  };
}

/** Header string: `4 workers + coordinator` (the coordinator is extra). */
export function teamHeaderLabel(
  workerCount: number | null | undefined,
): string {
  const count =
    workerCount === null || workerCount === undefined
      ? 0
      : Math.max(0, Math.floor(workerCount));
  return `${count} worker${count === 1 ? "" : "s"} + coordinator`;
}

/** Turns a member roster into the worker count (coordinator excluded). */
export function teammateCount(members: readonly TeamMember[]): number {
  return members.filter((member) => member.role === "teammate").length;
}

// ---------------------------------------------------------------------------
// Author roles
// ---------------------------------------------------------------------------

/**
 * User-facing role of a task/message/mission/outcome author. The wire still
 * calls the coordinator `lead`; only the display label changes.
 */
export type TeamAuthorRole = "coordinator" | "teammate";

function sameAgent(
  left: string,
  right: string | null | undefined,
): boolean {
  if (right === null || right === undefined) {
    return false;
  }
  return left === right || left.toLowerCase() === right.toLowerCase();
}

/**
 * Resolves an author's role from the team summary. Matching accepts either a
 * member's row id or its agent name, so callers can pass the id the wire
 * provides. Unknown authors stay unlabelled rather than guessed.
 */
export function teamAuthorRole(
  agentId: string | null | undefined,
  summary: TeamSummary | null | undefined,
): TeamAuthorRole | null {
  const id = typeof agentId === "string" ? agentId.trim() : "";
  if (id === "" || summary === null || summary === undefined) {
    return null;
  }
  if (sameAgent(id, summary.lead)) {
    return "coordinator";
  }
  const member = summary.members.find(
    (candidate) =>
      sameAgent(id, candidate.id) || sameAgent(id, candidate.agent),
  );
  if (member === undefined) {
    return null;
  }
  return member.role === "lead" ? "coordinator" : "teammate";
}

/** Display label for an author role; `null` hides the role chip. */
export function teamAuthorRoleLabel(
  role: TeamAuthorRole | null,
): string | null {
  if (role === null) {
    return null;
  }
  return role === "coordinator" ? "Coordinator" : "Teammate";
}

// ---------------------------------------------------------------------------
// Task board
// ---------------------------------------------------------------------------

export type TeamTaskStatus =
  | "pending"
  | "in_progress"
  | "blocked"
  | "completed";

export const TEAM_BOARD_COLUMNS = [
  { status: "pending", label: "Pending" },
  { status: "in_progress", label: "In progress" },
  { status: "blocked", label: "Blocked" },
  { status: "completed", label: "Completed" },
] as const;

export type TeamTask = {
  id: string;
  title: string;
  description: string;
  status: TeamTaskStatus;
  /** Original wire status, retained for unknown states. */
  raw_status: string;
  assignee: string | null;
  /** Agent id/name that created the task, when the server records it. */
  created_by: string | null;
  dependencies: string[];
  blocked_by: string[];
  /** Free-text reason when the server records a blocked task. */
  blocked_reason: string;
  summary: string;
  created_at: string | null;
  updated_at: string | null;
};

/** Maps the common status spellings onto the four board columns. */
export function normalizeTeamTaskStatus(value: unknown): TeamTaskStatus {
  const status =
    typeof value === "string"
      ? value.trim().toLowerCase().replace(/[\s-]+/g, "_")
      : "";
  switch (status) {
    case "pending":
    case "open":
    case "queued":
    case "backlog":
    case "new":
    case "ready":
      return "pending";
    case "in_progress":
    case "inprogress":
    case "running":
    case "active":
    case "claimed":
    case "doing":
    case "working":
      return "in_progress";
    case "blocked":
    case "waiting":
    case "stalled":
    case "failed":
    case "error":
      return "blocked";
    case "completed":
    case "complete":
    case "done":
    case "finished":
    case "succeeded":
    case "closed":
      return "completed";
    default:
      return "pending";
  }
}

export function teamTaskStatusLabel(status: TeamTaskStatus): string {
  switch (status) {
    case "pending":
      return "Pending";
    case "in_progress":
      return "In progress";
    case "blocked":
      return "Blocked";
    case "completed":
      return "Completed";
  }
}

export function parseTeamTask(raw: unknown, index = 0): TeamTask | null {
  if (!isRecord(raw)) {
    return null;
  }
  const rawStatus = textField(raw, ["status", "state"], "pending");
  const description = textField(raw, [
    "description",
    "detail",
    "body",
    "instructions",
  ]);
  const title =
    textField(raw, ["title", "subject", "name", "summary"]) ||
    firstLine(description) ||
    `Task ${index + 1}`;
  const assignee =
    agentName(raw.assignee) ||
    agentName(raw.owner) ||
    agentName(raw.agent) ||
    agentName(raw.claimed_by) ||
    null;
  return {
    id: idField(raw, ["id", "team_task_id", "task_id"]) || `task-${index}`,
    title,
    description,
    status: normalizeTeamTaskStatus(rawStatus),
    raw_status: rawStatus,
    assignee,
    created_by:
      agentName(raw.created_by) ||
      agentName(raw.created_by_agent) ||
      agentName(raw.author) ||
      agentName(raw.creator) ||
      null,
    dependencies: stringList(raw.dependencies ?? raw.depends_on ?? raw.depends),
    blocked_by: stringList(
      raw.blocked_by ?? raw.blockers ?? raw.blocked_on,
    ),
    blocked_reason: textField(raw, ["blocked_reason", "block_reason"]),
    summary: textField(raw, [
      "summary",
      "result_summary",
      "result",
      "outcome",
    ]),
    created_at: timeField(raw, ["created_at", "created"]),
    updated_at: timeField(raw, ["updated_at", "updated", "completed_at"]),
  };
}

export type TeamTaskGroups = Record<TeamTaskStatus, TeamTask[]>;

/** Groups tasks into exactly the four board columns, preserving order. */
export function groupTeamTasks(
  tasks: readonly TeamTask[],
): TeamTaskGroups {
  const groups: TeamTaskGroups = {
    pending: [],
    in_progress: [],
    blocked: [],
    completed: [],
  };
  for (const task of tasks) {
    groups[task.status].push(task);
  }
  return groups;
}

export function parseTeamTasks(raw: unknown): TeamTask[] {
  return listFrom(raw, ["tasks", "team_tasks", "items"])
    .map((task, index) => parseTeamTask(task, index))
    .filter((task): task is TeamTask => task !== null);
}

/**
 * Blocked-by ids for one card. An explicit `blocked_by` wins; otherwise any
 * dependency that is not `completed` (or is missing) is a blocker, matching
 * the core task board's `unmet_dependencies`.
 */
export function teamTaskBlockedBy(
  task: TeamTask,
  tasks: readonly TeamTask[],
): string[] {
  if (task.blocked_by.length > 0) {
    return task.blocked_by;
  }
  if (task.status === "completed") {
    return [];
  }
  return task.dependencies.filter((dependency) => {
    const target = tasks.find((candidate) => candidate.id === dependency);
    return target === undefined || target.status !== "completed";
  });
}

function firstLine(text: string): string {
  const line = text.split("\n")[0]?.trim() ?? "";
  return line.length > 100 ? `${line.slice(0, 99)}…` : line;
}

// ---------------------------------------------------------------------------
// Mailbox
// ---------------------------------------------------------------------------

export type MailboxBox = "inbox" | "sent";

export type TeamMessage = {
  id: string;
  box: MailboxBox;
  sender: string;
  recipient: string;
  subject: string;
  body: string;
  task_id: string | null;
  task_title: string | null;
  read: boolean;
  created_at: string | null;
};

export function parseTeamMessage(
  raw: unknown,
  box: MailboxBox,
  index = 0,
): TeamMessage | null {
  if (!isRecord(raw)) {
    return null;
  }
  const body = textField(raw, [
    "body",
    "text",
    "content",
    "message",
    "summary",
  ]);
  const subject =
    textField(raw, ["subject", "title", "topic"]) ||
    firstLine(body) ||
    "(no subject)";
  const recipients = stringList(
    raw.recipients ??
      raw.to ??
      raw.recipient ??
      raw.to_agents ??
      raw.to_agent_id,
  );
  const recipient =
    recipients.length > 0
      ? recipients.join(", ")
      : raw.to_agent_id === null ||
          booleanField(raw, ["broadcast", "is_broadcast"]) === true
        ? "all"
        : "—";
  const readAt = timeField(raw, ["read_at", "seen_at"]);
  const hasReadAt = raw.read_at !== undefined || raw.seen_at !== undefined;
  const read =
    booleanField(raw, ["read", "is_read"]) ??
    (booleanField(raw, ["unread", "is_unread"]) === true
      ? false
      : hasReadAt
        ? readAt !== null
        : true);
  return {
    id: idField(raw, ["id", "message_id"]) || `message-${index}`,
    box,
    sender:
      agentName(raw.from) ||
      agentName(raw.sender) ||
      agentName(raw.from_agent_id) ||
      agentName(raw.from_agent) ||
      agentName(raw.author) ||
      "unknown",
    recipient,
    subject,
    body,
    task_id:
      idField(raw, ["task_id", "team_task_id", "about_task_id"]) || null,
    task_title: textField(raw, ["task_title", "task", "about"]) || null,
    read,
    created_at: timeField(raw, [
      "created_at",
      "sent_at",
      "time",
      "timestamp",
    ]),
  };
}

export function parseTeamMessages(
  raw: unknown,
  box: MailboxBox,
): TeamMessage[] {
  const guessedBox =
    box === "inbox"
      ? ["messages", "inbox", "mailbox", "items"]
      : ["messages", "sent", "mailbox", "items"];
  return listFrom(raw, guessedBox)
    .map((message, index) => parseTeamMessage(message, box, index))
    .filter((message): message is TeamMessage => message !== null);
}

/** Inbox/Sent filter used by the mailbox sub-tabs. */
export function mailboxRows(
  messages: readonly TeamMessage[],
  box: MailboxBox,
): TeamMessage[] {
  return messages.filter((message) => message.box === box);
}

export function unreadCount(messages: readonly TeamMessage[]): number {
  return messages.filter((message) => !message.read).length;
}

// ---------------------------------------------------------------------------
// Mission log
// ---------------------------------------------------------------------------

export type MissionKind =
  | "progress"
  | "handoff"
  | "blocked"
  | "decision"
  | "done"
  | "error"
  | "unknown";

export type MissionEntry = {
  id: string;
  kind: MissionKind;
  raw_kind: string;
  summary: string;
  evidence: string | null;
  next_action: string | null;
  actor: string | null;
  created_at: string | null;
};

export const MISSION_KINDS: readonly MissionKind[] = [
  "progress",
  "handoff",
  "blocked",
  "decision",
  "done",
  "error",
] as const;

export function normalizeMissionKind(value: unknown): MissionKind {
  const kind =
    typeof value === "string"
      ? value.trim().toLowerCase().replace(/[\s-]+/g, "_")
      : "";
  switch (kind) {
    case "progress":
    case "update":
    case "status":
    case "note":
      return "progress";
    case "handoff":
    case "hand_off":
    case "handover":
    case "hand_over":
    case "transfer":
      return "handoff";
    case "blocked":
    case "blocker":
    case "waiting":
      return "blocked";
    case "decision":
    case "decided":
      return "decision";
    case "done":
    case "complete":
    case "completed":
    case "finished":
      return "done";
    case "error":
    case "failed":
    case "failure":
      return "error";
    default:
      return "unknown";
  }
}

export function missionKindLabel(kind: MissionKind | string): string {
  switch (normalizeMissionKind(kind)) {
    case "progress":
      return "Progress";
    case "handoff":
      return "Handoff";
    case "blocked":
      return "Blocked";
    case "decision":
      return "Decision";
    case "done":
      return "Done";
    case "error":
      return "Error";
    case "unknown":
      return typeof kind === "string" && kind.trim() !== ""
        ? kind.trim()
        : "Note";
  }
}

export function missionKindVariant(
  kind: MissionKind | string,
): "info" | "accent" | "warning" | "success" | "destructive" | "muted" {
  switch (normalizeMissionKind(kind)) {
    case "progress":
      return "muted";
    case "handoff":
      return "info";
    case "blocked":
      return "warning";
    case "decision":
      return "accent";
    case "done":
      return "success";
    case "error":
      return "destructive";
    case "unknown":
      return "muted";
  }
}

export function parseMissionEntry(
  raw: unknown,
  index = 0,
): MissionEntry | null {
  if (!isRecord(raw)) {
    return null;
  }
  const rawKind = textField(raw, ["kind", "type", "event"], "progress");
  return {
    id: idField(raw, ["id", "entry_id", "log_id"]) || `mission-${index}`,
    kind: normalizeMissionKind(rawKind),
    raw_kind: rawKind,
    summary:
      textField(raw, ["summary", "message", "title", "text", "detail"]) ||
      "(no summary)",
    evidence:
      textField(raw, ["evidence", "evidence_text", "details"]) || null,
    next_action:
      textField(raw, ["next_action", "next", "next_step", "follow_up"]) ||
      null,
    actor:
      agentName(raw.actor) ||
      agentName(raw.agent_id) ||
      agentName(raw.agent) ||
      agentName(raw.agent_key) ||
      null,
    created_at: timeField(raw, ["created_at", "time", "timestamp"]),
  };
}

export function parseMissionLog(raw: unknown): MissionEntry[] {
  return listFrom(raw, ["mission", "mission_log", "entries", "log", "items"])
    .map((entry, index) => parseMissionEntry(entry, index))
    .filter((entry): entry is MissionEntry => entry !== null);
}

// ---------------------------------------------------------------------------
// Outcomes
// ---------------------------------------------------------------------------

export type OutcomeStatus = "draft" | "in_review" | "finalized";

export type FragmentReviewStatus =
  | "pending"
  | "approved"
  | "rejected"
  | "changes_requested";

export type OutcomeFragment = {
  id: string;
  outcome_id: string | null;
  section: string | null;
  content: string;
  source_agent: string | null;
  review_status: FragmentReviewStatus;
  created_at: string | null;
};

export type TeamOutcome = {
  id: string;
  title: string;
  status: OutcomeStatus;
  raw_status: string;
  required_sections: string[];
  fragments: OutcomeFragment[];
  /** Agent id/name that created the outcome, when the server records it. */
  created_by: string | null;
  finalized_at: string | null;
  created_at: string | null;
};

export function normalizeOutcomeStatus(value: unknown): OutcomeStatus {
  const status =
    typeof value === "string"
      ? value.trim().toLowerCase().replace(/[\s-]+/g, "_")
      : "";
  switch (status) {
    case "in_review":
    case "inreview":
    case "review":
    case "reviewing":
    case "under_review":
      return "in_review";
    case "finalized":
    case "finalised":
    case "final":
    case "done":
    case "complete":
      return "finalized";
    case "draft":
    default:
      return "draft";
  }
}

export function outcomeStatusLabel(status: OutcomeStatus | string): string {
  switch (normalizeOutcomeStatus(status)) {
    case "draft":
      return "Draft";
    case "in_review":
      return "In review";
    case "finalized":
      return "Finalized";
  }
}

export function normalizeReviewStatus(value: unknown): FragmentReviewStatus {
  const status =
    typeof value === "string"
      ? value.trim().toLowerCase().replace(/[\s-]+/g, "_")
      : "";
  switch (status) {
    case "approved":
    case "accepted":
    case "ok":
    case "reviewed":
      return "approved";
    case "rejected":
    case "denied":
      return "rejected";
    case "changes_requested":
    case "change_requested":
    case "needs_changes":
    case "revision":
      return "changes_requested";
    case "pending":
    case "unreviewed":
    case "open":
    case "draft":
    default:
      return "pending";
  }
}

export function fragmentReviewLabel(
  status: FragmentReviewStatus | string,
): string {
  switch (normalizeReviewStatus(status)) {
    case "pending":
      return "Pending review";
    case "approved":
      return "Reviewed";
    case "rejected":
      return "Rejected";
    case "changes_requested":
      return "Changes requested";
  }
}

export function reviewStatusVariant(
  status: FragmentReviewStatus | string,
): "info" | "success" | "destructive" | "warning" {
  switch (normalizeReviewStatus(status)) {
    case "approved":
      return "success";
    case "rejected":
      return "destructive";
    case "changes_requested":
      return "warning";
    case "pending":
      return "info";
  }
}

function requiredSections(value: unknown): string[] {
  if (!Array.isArray(value)) {
    return stringList(value);
  }
  const sections: string[] = [];
  for (const item of value) {
    if (typeof item === "string" && item.trim() !== "") {
      sections.push(item.trim());
      continue;
    }
    if (isRecord(item)) {
      const label = textField(item, [
        "title",
        "name",
        "section",
        "label",
        "key",
      ]);
      if (label !== "") {
        sections.push(label);
      }
    }
  }
  return sections;
}

export function parseOutcomeFragment(
  raw: unknown,
  index = 0,
): OutcomeFragment | null {
  if (!isRecord(raw)) {
    return null;
  }
  return {
    id: idField(raw, ["id", "fragment_id"]) || `fragment-${index}`,
    outcome_id:
      idField(raw, ["outcome_id", "team_outcome_id"]) || null,
    section:
      textField(raw, [
        "section",
        "section_key",
        "required_section",
        "name",
      ]) || null,
    content: textField(raw, ["content", "text", "body", "fragment"]),
    source_agent:
      agentName(raw.source_agent_id) ||
      agentName(raw.source_agent) ||
      agentName(raw.agent) ||
      agentName(raw.source) ||
      null,
    review_status: normalizeReviewStatus(
      raw.review_status ?? raw.review ?? raw.status,
    ),
    created_at: timeField(raw, ["created_at", "submitted_at"]),
  };
}

export function parseTeamOutcome(
  raw: unknown,
  index = 0,
): TeamOutcome | null {
  if (!isRecord(raw)) {
    return null;
  }
  const rawStatus = textField(raw, ["status", "state"], "draft");
  const id = idField(raw, ["id", "outcome_id"]) || `outcome-${index}`;
  const fragmentSource = Array.isArray(raw.fragments)
    ? raw.fragments
    : Array.isArray(raw.outcome_fragments)
      ? raw.outcome_fragments
      : [];
  const fragments = fragmentSource
    .map((fragment, fragmentIndex) =>
      parseOutcomeFragment(fragment, fragmentIndex),
    )
    .filter((fragment): fragment is OutcomeFragment => fragment !== null)
    .map((fragment) =>
      fragment.outcome_id === null
        ? { ...fragment, outcome_id: id }
        : fragment,
    );
  return {
    id,
    title:
      textField(raw, ["title", "name", "subject"]) ||
      `Outcome ${index + 1}`,
    status: normalizeOutcomeStatus(rawStatus),
    raw_status: rawStatus,
    required_sections: requiredSections(
      raw.required_sections ?? raw.sections,
    ),
    fragments,
    created_by:
      agentName(raw.created_by) ||
      agentName(raw.created_by_agent) ||
      agentName(raw.author) ||
      agentName(raw.creator) ||
      null,
    finalized_at: timeField(raw, ["finalized_at", "completed_at"]),
    created_at: timeField(raw, ["created_at", "created"]),
  };
}

export function parseTeamOutcomes(raw: unknown): TeamOutcome[] {
  const outcomes = listFrom(raw, [
    "outcomes",
    "team_outcomes",
    "items",
  ])
    .map((outcome, index) => parseTeamOutcome(outcome, index))
    .filter((outcome): outcome is TeamOutcome => outcome !== null);

  // Some responses serve outcomes and fragments as sibling arrays (core's
  // `TeamSnapshot` does); attach those to their outcome here.
  const topLevelFragments = listFrom(raw, [
    "fragments",
    "outcome_fragments",
  ])
    .map((fragment, index) => parseOutcomeFragment(fragment, index))
    .filter((fragment): fragment is OutcomeFragment => fragment !== null);
  if (topLevelFragments.length > 0) {
    for (const outcome of outcomes) {
      const ids = new Set(outcome.fragments.map((fragment) => fragment.id));
      for (const fragment of topLevelFragments) {
        if (
          fragment.outcome_id === outcome.id &&
          !ids.has(fragment.id)
        ) {
          outcome.fragments.push(fragment);
          ids.add(fragment.id);
        }
      }
    }
  }
  return outcomes;
}

export type OutcomeSectionCoverage = {
  total: number;
  covered: number;
  missing: string[];
  awaiting: string[];
};

function sectionKey(value: string): string {
  return value.trim().toLowerCase().replace(/\s+/g, " ");
}

/** Which required sections have fragments, and which still await approval. */
export function outcomeSectionCoverage(
  outcome: TeamOutcome,
): OutcomeSectionCoverage {
  const approved = new Set<string>();
  const present = new Set<string>();
  for (const fragment of outcome.fragments) {
    if (fragment.section === null) {
      continue;
    }
    const key = sectionKey(fragment.section);
    present.add(key);
    if (fragment.review_status === "approved") {
      approved.add(key);
    }
  }
  const missing: string[] = [];
  const awaiting: string[] = [];
  for (const section of outcome.required_sections) {
    const key = sectionKey(section);
    if (!present.has(key)) {
      missing.push(section);
    } else if (!approved.has(key)) {
      awaiting.push(section);
    }
  }
  return {
    total: outcome.required_sections.length,
    covered: outcome.required_sections.length - missing.length,
    missing,
    awaiting,
  };
}

export type OutcomeFinalizeState = {
  ready: boolean;
  label: string;
  variant: "success" | "warning" | "info" | "muted";
};

/** Human finalize state shown under each outcome. */
export function outcomeFinalizeState(
  outcome: TeamOutcome,
): OutcomeFinalizeState {
  if (outcome.status === "finalized") {
    return { ready: true, label: "Finalized", variant: "success" };
  }
  const coverage = outcomeSectionCoverage(outcome);
  if (coverage.total === 0) {
    // Core still requires at least one reviewed fragment when the outcome
    // declares no explicit sections.
    if (
      outcome.fragments.some(
        (fragment) => fragment.review_status === "approved",
      )
    ) {
      return { ready: true, label: "Ready to finalize", variant: "success" };
    }
    return {
      ready: false,
      label:
        outcome.fragments.length === 0
          ? "No required sections"
          : "Awaiting review",
      variant: outcome.fragments.length === 0 ? "muted" : "warning",
    };
  }
  if (coverage.missing.length > 0) {
    const count = coverage.missing.length;
    return {
      ready: false,
      label: `${count} required section${count === 1 ? "" : "s"} missing`,
      variant: "info",
    };
  }
  if (coverage.awaiting.length > 0) {
    return { ready: false, label: "Awaiting review", variant: "warning" };
  }
  return { ready: true, label: "Ready to finalize", variant: "success" };
}

// ---------------------------------------------------------------------------
// Runs
// ---------------------------------------------------------------------------

export type TeamRun = {
  id: string;
  agent: string;
  task_id: string | null;
  task_title: string | null;
  status: string;
  progress: number | null;
  /** Latest human progress note (core's `last_progress_message`). */
  progress_message: string;
  /** Current activity text (core's `current_activity`). */
  activity: string;
  result_summary: string;
  error: string | null;
  created_at: string | null;
  updated_at: string | null;
};

const ACTIVE_RUN_STATES: ReadonlySet<string> = new Set([
  "active",
  "running",
  "in_progress",
  "working",
  "claimed",
  "started",
]);

const QUEUED_RUN_STATES: ReadonlySet<string> = new Set([
  "queued",
  "pending",
  "waiting",
  "scheduled",
  "ready",
]);

export function normalizeRunStatus(value: unknown): string {
  return typeof value === "string"
    ? value.trim().toLowerCase().replace(/[\s-]+/g, "_")
    : "";
}

export function runStatusLabel(status: string): string {
  const normalized = normalizeRunStatus(status);
  if (normalized === "") {
    return "Unknown";
  }
  return normalized
    .split("_")
    .map((part, index) =>
      index === 0 ? part.charAt(0).toUpperCase() + part.slice(1) : part,
    )
    .join(" ");
}

export function runStatusVariant(
  status: string,
): "success" | "warning" | "destructive" | "info" | "muted" {
  const normalized = normalizeRunStatus(status);
  if (ACTIVE_RUN_STATES.has(normalized)) {
    return "info";
  }
  if (QUEUED_RUN_STATES.has(normalized)) {
    return "muted";
  }
  switch (normalized) {
    case "completed":
    case "complete":
    case "done":
    case "succeeded":
    case "ok":
      return "success";
    case "failed":
    case "error":
    case "cancelled":
    case "canceled":
      return "destructive";
    case "blocked":
    case "stalled":
      return "warning";
    default:
      return "muted";
  }
}

export function parseTeamRun(raw: unknown, index = 0): TeamRun | null {
  if (!isRecord(raw)) {
    return null;
  }
  return {
    id: idField(raw, ["id", "run_id"]) || `run-${index}`,
    agent:
      agentName(raw.agent_id) ||
      agentName(raw.agent) ||
      agentName(raw.worker) ||
      agentName(raw.agent_key) ||
      "unknown",
    task_id:
      idField(raw, ["task_id", "team_task_id", "about_task_id"]) || null,
    task_title: textField(raw, ["task_title", "task_subject"]) || null,
    status: textField(raw, ["status", "state"], "unknown"),
    progress: numberField(raw, ["progress", "progress_percent", "percent"]),
    progress_message: textField(raw, [
      "last_progress_message",
      "progress_message",
      "message",
    ]),
    activity: textField(raw, ["current_activity", "activity"]),
    result_summary: textField(raw, [
      "result_summary",
      "result",
      "summary",
      "outcome",
    ]),
    error: textField(raw, ["error"]) || null,
    created_at: timeField(raw, ["created_at", "started_at"]),
    updated_at: timeField(raw, ["updated_at", "finished_at", "completed_at"]),
  };
}

export function parseTeamRuns(raw: unknown): TeamRun[] {
  return listFrom(raw, ["runs", "team_runs", "items"])
    .map((run, index) => parseTeamRun(run, index))
    .filter((run): run is TeamRun => run !== null);
}

export type TeamRunCounts = {
  total: number;
  active: number;
  queued: number;
  completed: number;
  failed: number;
};

export function teamRunCounts(runs: readonly TeamRun[]): TeamRunCounts {
  let active = 0;
  let queued = 0;
  let completed = 0;
  let failed = 0;
  for (const run of runs) {
    const status = normalizeRunStatus(run.status);
    if (ACTIVE_RUN_STATES.has(status)) {
      active += 1;
    } else if (QUEUED_RUN_STATES.has(status)) {
      queued += 1;
    } else if (
      status === "completed" ||
      status === "complete" ||
      status === "done" ||
      status === "succeeded" ||
      status === "ok"
    ) {
      completed += 1;
    } else if (
      status === "failed" ||
      status === "error" ||
      status === "cancelled" ||
      status === "canceled"
    ) {
      failed += 1;
    }
  }
  return { total: runs.length, active, queued, completed, failed };
}

/** Clamped 0–100 progress, or `null` when the run has no number. */
export function runProgressPercent(run: TeamRun): number | null {
  if (run.progress === null || !Number.isFinite(run.progress)) {
    return null;
  }
  return Math.max(0, Math.min(100, Math.round(run.progress)));
}

// ---------------------------------------------------------------------------
// Scale approvals
// ---------------------------------------------------------------------------

export type ScaleApprovalStatus =
  | "pending"
  | "granted"
  | "denied"
  | "expired"
  | "unknown";

export type ScaleApproval = {
  id: string;
  task_id: string;
  team_id: string | null;
  requested_workers: number;
  /** Current teammate cap before this request (the default is 8). */
  current_cap: number;
  reason: string;
  status: ScaleApprovalStatus;
  created_at: string | null;
  expires_at: string | null;
  decided_at: string | null;
};

export const SCALE_APPROVAL_DEFAULT_CAP = 8;

/** Canonicalizes the allow/deny/expire spellings the server may emit. */
export function normalizeScaleApprovalStatus(
  value: unknown,
): ScaleApprovalStatus {
  const status =
    typeof value === "string"
      ? value.trim().toLowerCase().replace(/[\s-]+/g, "_")
      : "";
  switch (status) {
    case "pending":
      return "pending";
    case "allow":
    case "allowed":
    case "grant":
    case "granted":
    case "approve":
    case "approved":
      return "granted";
    case "deny":
    case "denied":
    case "reject":
    case "rejected":
      return "denied";
    case "expire":
    case "expired":
      return "expired";
    default:
      return "unknown";
  }
}

export function scaleApprovalStatusLabel(
  status: ScaleApprovalStatus | string,
): string {
  switch (normalizeScaleApprovalStatus(status)) {
    case "pending":
      return "Pending";
    case "granted":
      return "Allowed";
    case "denied":
      return "Denied";
    case "expired":
      return "Expired";
    case "unknown":
      return "Unknown";
  }
}

export function scaleApprovalStatusVariant(
  status: ScaleApprovalStatus | string,
): "info" | "success" | "destructive" | "muted" {
  switch (normalizeScaleApprovalStatus(status)) {
    case "pending":
      return "info";
    case "granted":
      return "success";
    case "denied":
      return "destructive";
    case "expired":
      return "muted";
    case "unknown":
      return "muted";
  }
}

export function isScaleApprovalPending(
  approval: Pick<ScaleApproval, "status">,
): boolean {
  return approval.status === "pending";
}

export function parseScaleApproval(raw: unknown): ScaleApproval | null {
  if (!isRecord(raw)) {
    return null;
  }
  const id = idField(raw, ["id", "approval_id"]);
  if (id === "") {
    return null;
  }
  const requested =
    numberField(raw, [
      "requested_workers",
      "requested",
      "requested_n",
      "workers",
    ]) ?? 0;
  return {
    id,
    task_id: idField(raw, ["task_id"]),
    team_id: idField(raw, ["team_id"]) || null,
    requested_workers: Math.max(0, Math.floor(requested)),
    current_cap:
      Math.max(
        0,
        Math.floor(
          numberField(raw, ["current_cap", "cap", "max_workers"]) ??
            SCALE_APPROVAL_DEFAULT_CAP,
        ),
      ),
    reason: textField(raw, ["reason", "justification"], ""),
    status: normalizeScaleApprovalStatus(raw.status),
    created_at: timeField(raw, ["created_at", "created"]),
    expires_at: timeField(raw, ["expires_at", "expiry", "expires"]),
    decided_at: timeField(raw, ["decided_at", "decision_at"]),
  };
}

export function parseScaleApprovals(raw: unknown): ScaleApproval[] {
  return listFrom(raw, ["approvals", "scale_approvals", "items"])
    .map((approval) => parseScaleApproval(approval))
    .filter((approval): approval is ScaleApproval => approval !== null);
}

// ---------------------------------------------------------------------------
// Task lookup helpers shared by the mailbox/run cards
// ---------------------------------------------------------------------------

/** Best-effort task label: stored title, then other stored ids. */
export function teamTaskLabel(
  taskId: string | null,
  tasks: readonly TeamTask[],
): string | null {
  if (taskId === null || taskId === "") {
    return null;
  }
  return tasks.find((task) => task.id === taskId)?.title ?? taskId;
}
