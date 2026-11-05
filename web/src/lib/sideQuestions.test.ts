import assert from "node:assert/strict";
import { test } from "node:test";
import {
  appendSideQuestion,
  newestSideSessions,
  openSideSessionId,
} from "./sideQuestions.ts";
import type {
  AskSideQuestionResponse,
  SideQuestionRequest,
  SideQuestionSession,
} from "./types.ts";

function request(
  id: string,
  sessionId: string,
  createdAt: string,
): SideQuestionRequest {
  return {
    id,
    session_id: sessionId,
    question: `question ${id}`,
    answer: `answer ${id}`,
    status: "ok",
    error: null,
    model: "river-worker",
    tokens_in: 1,
    tokens_out: 2,
    latency_ms: 3,
    created_at: createdAt,
    completed_at: createdAt,
  };
}

function session(
  id: string,
  createdAt: string,
  status: "open" | "closed",
  requests: SideQuestionRequest[],
): SideQuestionSession {
  return {
    id,
    created_at: createdAt,
    closed_at: null,
    status,
    snapshot_summary: null,
    requests,
  };
}

test("newestSideSessions sorts newest first without mutating the input", () => {
  const older = session("s1", "2026-10-30T00:00:00Z", "closed", []);
  const newer = session("s2", "2026-10-30T01:00:00Z", "open", []);
  const input = [older, newer];

  assert.deepEqual(newestSideSessions(input), [newer, older]);
  assert.deepEqual(input, [older, newer]);
});

test("openSideSessionId picks the newest open session", () => {
  const closed = session("s1", "2026-10-30T02:00:00Z", "closed", []);
  const openOlder = session("s2", "2026-10-30T01:00:00Z", "open", []);
  const openNewer = session("s3", "2026-10-30T03:00:00Z", "open", []);

  assert.equal(
    openSideSessionId([openOlder, closed, openNewer]),
    openNewer.id,
  );
  assert.equal(
    openSideSessionId([
      session("s4", "2026-10-30T00:00:00Z", "closed", []),
    ]),
    undefined,
  );
});

test("appendSideQuestion appends to the active session", () => {
  const first = request("q1", "s1", "2026-10-30T00:00:01Z");
  const second = request("q2", "s1", "2026-10-30T00:00:02Z");
  const existing = session("s1", "2026-10-30T00:00:00Z", "open", [first]);
  const response: AskSideQuestionResponse = {
    session_id: "s1",
    request: second,
    answer: "answer q2",
  };

  const updated = appendSideQuestion([existing], response);
  assert.equal(updated.length, 1);
  assert.deepEqual(updated[0].requests, [first, second]);
  assert.equal(updated[0].status, "open");
});

test("appendSideQuestion prepends a new session and closes the previous open one", () => {
  const old = session("s1", "2026-10-30T00:00:00Z", "open", [
    request("q1", "s1", "2026-10-30T00:00:01Z"),
  ]);
  const newRequest = request("q2", "s2", "2026-10-30T01:00:01Z");
  const response: AskSideQuestionResponse = {
    session_id: "s2",
    request: newRequest,
    answer: "answer q2",
  };

  const updated = appendSideQuestion([old], response);
  assert.equal(updated.length, 2);
  assert.equal(updated[0].id, "s2");
  assert.equal(updated[0].status, "open");
  assert.deepEqual(updated[0].requests, [newRequest]);
  assert.equal(updated[1].status, "closed");
});
