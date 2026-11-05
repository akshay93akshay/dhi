import type { AskSideQuestionResponse, SideQuestionSession } from "./types";

/** Sorts sessions newest first without mutating the input. */
export function newestSideSessions(
  sessions: SideQuestionSession[],
): SideQuestionSession[] {
  return [...sessions].sort(
    (a, b) => Date.parse(b.created_at) - Date.parse(a.created_at),
  );
}

/** The newest open session id, or `undefined` when every session is closed. */
export function openSideSessionId(
  sessions: SideQuestionSession[],
): string | undefined {
  return newestSideSessions(sessions).find(
    (session) => session.status === "open",
  )?.id;
}

/**
 * Applies a successful ask to the local history.
 *
 * - Appending to an existing session adds the request at its end.
 * - A brand-new session is prepended. The server closes the task's previous
 *   open session in the same call, so the local copy mirrors that.
 */
export function appendSideQuestion(
  sessions: SideQuestionSession[],
  response: AskSideQuestionResponse,
): SideQuestionSession[] {
  const existing = sessions.some(
    (session) => session.id === response.session_id,
  );
  if (existing) {
    return sessions.map((session) =>
      session.id === response.session_id
        ? { ...session, requests: [...session.requests, response.request] }
        : session,
    );
  }
  const created: SideQuestionSession = {
    id: response.session_id,
    created_at: response.request.created_at,
    closed_at: null,
    status: "open",
    snapshot_summary: null,
    requests: [response.request],
  };
  return [
    created,
    ...sessions.map((session) =>
      session.status === "open"
        ? { ...session, status: "closed" as const }
        : session,
    ),
  ];
}
