/**
 * Typed, read-only fetch helpers for the Swarm workspace.
 *
 * The team routes are additive: an older server (or a task with no persisted
 * team state) answers `404`. Every helper folds that one status into an empty
 * value so the workspace renders its empty states instead of an error. Other
 * failures keep their `ApiError` and surface in the card.
 */

import { ApiError } from "./api.ts";
import type { ApprovalDecision } from "./types.ts";
import {
  parseMissionLog,
  parseScaleApproval,
  parseScaleApprovals,
  parseTeamMessages,
  parseTeamOutcomes,
  parseTeamRuns,
  parseTeamSummary,
  parseTeamTasks,
  type MailboxBox,
  type MissionEntry,
  type ScaleApproval,
  type TeamMessage,
  type TeamOutcome,
  type TeamRun,
  type TeamSummary,
  type TeamTask,
} from "./teamTypes.ts";

// ---------------------------------------------------------------------------
// Request plumbing
// ---------------------------------------------------------------------------

/** Wire paths, kept relative exactly like `api.ts`. */
export function teamPath(taskId: string): string {
  return `/tasks/${encodeURIComponent(taskId)}/team`;
}

export function teamTasksPath(taskId: string): string {
  return `${teamPath(taskId)}/tasks`;
}

export function teamMailboxPath(taskId: string, box: MailboxBox): string {
  return `${teamPath(taskId)}/mailbox?box=${box}`;
}

export function teamMissionPath(taskId: string): string {
  return `${teamPath(taskId)}/mission`;
}

export function teamRunsPath(taskId: string): string {
  return `${teamPath(taskId)}/runs`;
}

export function teamOutcomesPath(taskId: string): string {
  return `${teamPath(taskId)}/outcomes`;
}

export const SCALE_APPROVALS_PATH = "/scale-approvals";

export function scaleApprovalDecisionPath(id: string): string {
  return `/scale-approvals/${encodeURIComponent(id.trim())}/decision`;
}

async function errorMessage(response: Response): Promise<string> {
  let text = "";
  try {
    text = await response.text();
  } catch {
    // Fall through to the status line.
  }
  if (text !== "") {
    try {
      const parsed = JSON.parse(text) as { error?: unknown };
      if (typeof parsed?.error === "string" && parsed.error.trim() !== "") {
        return parsed.error;
      }
    } catch {
      // Not JSON; use the raw body below.
    }
    const trimmed = text.trim();
    if (trimmed !== "") {
      return trimmed.length > 300 ? trimmed.slice(0, 300) : trimmed;
    }
  }
  return `HTTP ${response.status}`;
}

async function requestJson(path: string, init?: RequestInit): Promise<unknown> {
  let response: Response;
  try {
    response = await fetch(`/api${path}`, {
      ...init,
      headers: {
        "content-type": "application/json",
        ...init?.headers,
      },
    });
  } catch (cause) {
    const message = cause instanceof Error ? cause.message : String(cause);
    throw new ApiError(0, `network error calling /api${path}: ${message}`);
  }

  if (!response.ok) {
    throw new ApiError(response.status, await errorMessage(response));
  }
  if (response.status === 204 || response.status === 205) {
    return null;
  }
  try {
    return (await response.json()) as unknown;
  } catch {
    throw new ApiError(0, `malformed JSON response from /api${path}`);
  }
}

/**
 * Reads one additive route. `404`/`405` mean "this server or task has no team
 * state", which is an empty view, not an error.
 */
async function readOptional(path: string): Promise<unknown | null> {
  try {
    return await requestJson(path);
  } catch (err) {
    if (
      err instanceof ApiError &&
      (err.status === 404 || err.status === 405)
    ) {
      return null;
    }
    throw err;
  }
}

// ---------------------------------------------------------------------------
// Team routes
// ---------------------------------------------------------------------------

export async function getTeamSummary(
  taskId: string,
): Promise<TeamSummary | null> {
  const raw = await readOptional(teamPath(taskId));
  return parseTeamSummary(raw);
}

export async function listTeamTasks(taskId: string): Promise<TeamTask[]> {
  const raw = await readOptional(teamTasksPath(taskId));
  return parseTeamTasks(raw);
}

export async function getTeamMailbox(
  taskId: string,
  box: MailboxBox,
): Promise<TeamMessage[]> {
  const raw = await readOptional(teamMailboxPath(taskId, box));
  return parseTeamMessages(raw, box);
}

export async function getTeamMission(
  taskId: string,
): Promise<MissionEntry[]> {
  const raw = await readOptional(teamMissionPath(taskId));
  return parseMissionLog(raw);
}

export async function getTeamRuns(taskId: string): Promise<TeamRun[]> {
  const raw = await readOptional(teamRunsPath(taskId));
  return parseTeamRuns(raw);
}

export async function getTeamOutcomes(
  taskId: string,
): Promise<TeamOutcome[]> {
  const raw = await readOptional(teamOutcomesPath(taskId));
  return parseTeamOutcomes(raw);
}

// Short aliases for callers that mirror the route names.
export const getTeam = getTeamSummary;
export const getTeamTasks = listTeamTasks;
export const getTeamMissionLog = getTeamMission;
export const listTeamRuns = getTeamRuns;
export const listTeamOutcomes = getTeamOutcomes;

// ---------------------------------------------------------------------------
// Scale approvals
// ---------------------------------------------------------------------------

/** Lists the scale queue; old servers without the route read as empty. */
export async function listScaleApprovals(): Promise<ScaleApproval[]> {
  const raw = await readOptional(SCALE_APPROVALS_PATH);
  return parseScaleApprovals(raw);
}

/**
 * Decides one scale request. The route may answer the updated approval row,
 * an `{ approval }` envelope, or no body; the caller refetches regardless.
 */
export async function decideScaleApproval(
  id: string,
  decision: ApprovalDecision,
): Promise<ScaleApproval | null> {
  const raw = await requestJson(scaleApprovalDecisionPath(id), {
    method: "POST",
    body: JSON.stringify({ decision }),
  });
  if (raw === null) {
    return null;
  }
  if (
    typeof raw === "object" &&
    raw !== null &&
    !Array.isArray(raw) &&
    "approval" in raw
  ) {
    return parseScaleApproval((raw as { approval: unknown }).approval);
  }
  return parseScaleApproval(raw);
}

// ---------------------------------------------------------------------------
// Whole workspace
// ---------------------------------------------------------------------------

/** Live-refresh cadence for the Swarm workspace while the card is mounted. */
export const TEAM_WORKSPACE_POLL_MS = 5000;

/**
 * Polling gate shared by the workspace and graph refreshes. Only a visible
 * document is polled; hiding the tab pauses both loads until it is visible
 * again (the visibility listener then refreshes immediately).
 */
export function shouldPollTeamWorkspace(
  visibilityState: string | null | undefined,
): boolean {
  return visibilityState === "visible";
}

export type TeamWorkspace = {
  summary: TeamSummary | null;
  tasks: TeamTask[];
  inbox: TeamMessage[];
  sent: TeamMessage[];
  mission: MissionEntry[];
  outcomes: TeamOutcome[];
  runs: TeamRun[];
};

export function emptyTeamWorkspace(): TeamWorkspace {
  return {
    summary: null,
    tasks: [],
    inbox: [],
    sent: [],
    mission: [],
    outcomes: [],
    runs: [],
  };
}

/**
 * Reads every team route in parallel. The routes are independent reads, so a
 * missing route only empties its own tab.
 */
export async function loadTeamWorkspace(
  taskId: string,
): Promise<TeamWorkspace> {
  const [summary, tasks, inbox, sent, mission, outcomes, runs] =
    await Promise.all([
      getTeamSummary(taskId),
      listTeamTasks(taskId),
      getTeamMailbox(taskId, "inbox"),
      getTeamMailbox(taskId, "sent"),
      getTeamMission(taskId),
      getTeamOutcomes(taskId),
      getTeamRuns(taskId),
    ]);
  return { summary, tasks, inbox, sent, mission, outcomes, runs };
}

/** Alias used by some callers: read the whole workspace in one call. */
export const fetchTeamWorkspace = loadTeamWorkspace;
