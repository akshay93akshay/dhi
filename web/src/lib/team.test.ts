import assert from "node:assert/strict";
import { afterEach, test } from "node:test";

import {
  TEAM_WORKSPACE_POLL_MS,
  loadTeamWorkspace,
  listScaleApprovals,
  decideScaleApproval,
  shouldPollTeamWorkspace,
} from "./team.ts";
import {
  DEFAULT_SWARM_TAB,
  SWARM_TABS,
  TEAM_BOARD_COLUMNS,
  fragmentReviewLabel,
  groupTeamTasks,
  mailboxRows,
  missionKindLabel,
  missionKindVariant,
  normalizeMissionKind,
  normalizeOutcomeStatus,
  normalizeReviewStatus,
  normalizeScaleApprovalStatus,
  normalizeTeamTaskStatus,
  outcomeFinalizeState,
  outcomeSectionCoverage,
  outcomeStatusLabel,
  parseMissionLog,
  parseScaleApproval,
  parseScaleApprovals,
  parseTeamMessages,
  parseTeamOutcomes,
  parseTeamSummary,
  parseTeamRuns,
  parseTeamTasks,
  teamAuthorRole,
  teamAuthorRoleLabel,
  teamHeaderLabel,
  teamRunCounts,
  teamTaskBlockedBy,
  type ScaleApproval,
} from "./teamTypes.ts";

const originalFetch = globalThis.fetch;

afterEach(() => {
  globalThis.fetch = originalFetch;
});

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

// ---------------------------------------------------------------------------
// Tabs: board is default, graph stays available but secondary
// ---------------------------------------------------------------------------

test("default Swarm tab is the task board", () => {
  assert.equal(DEFAULT_SWARM_TAB, "board");
  assert.equal(SWARM_TABS[0]?.id, "board");
  assert.equal(SWARM_TABS[0]?.label, "Task board");
});

test("Swarm tabs keep contract order and graph remains available last", () => {
  assert.deepEqual(
    SWARM_TABS.map((tab) => tab.id),
    ["board", "mailbox", "mission", "outcomes", "runs", "graph"],
  );
  const graph = SWARM_TABS.find((tab) => tab.id === "graph");
  assert.ok(graph);
  assert.equal(graph.label, "Graph");
  assert.notEqual(DEFAULT_SWARM_TAB, "graph");
});

// ---------------------------------------------------------------------------
// Task board grouping
// ---------------------------------------------------------------------------

test("board groups tasks by the four canonical statuses", () => {
  const tasks = parseTeamTasks({
    tasks: [
      { id: "t1", title: "Scout", status: "pending", assignee: "w1" },
      { id: "t2", title: "Draft", status: "in-progress", assignee: "w2" },
      { id: "t3", title: "Legal", status: "blocked", blocked_by: ["t2"] },
      { id: "t4", title: "Ship", status: "done", dependencies: ["t1", "t3"] },
      { id: "t5", title: "Review", status: "in review" },
    ],
  });
  assert.equal(tasks.length, 5);
  // Unknown/non-board statuses fall back to Pending but keep the wire label.
  assert.equal(tasks[4]?.status, "pending");
  assert.equal(tasks[4]?.raw_status, "in review");

  const groups = groupTeamTasks(tasks);
  assert.deepEqual(
    groups.pending.map((task) => task.id),
    ["t1", "t5"],
  );
  assert.deepEqual(
    groups.in_progress.map((task) => task.id),
    ["t2"],
  );
  assert.deepEqual(
    groups.blocked.map((task) => task.id),
    ["t3"],
  );
  assert.deepEqual(
    groups.completed.map((task) => task.id),
    ["t4"],
  );
  assert.deepEqual(
    TEAM_BOARD_COLUMNS.map((column) => column.label),
    ["Pending", "In progress", "Blocked", "Completed"],
  );
  assert.equal(tasks[3]?.dependencies.length, 2);
  assert.deepEqual(tasks[2]?.blocked_by, ["t2"]);
  // Core rows only carry depends_on, so unmet dependencies read as blockers.
  assert.deepEqual(teamTaskBlockedBy(tasks[3]!, tasks), []);
  assert.deepEqual(teamTaskBlockedBy(tasks[0]!, tasks), []);
  const pendingWithDependency = parseTeamTasks([
    { id: "a", title: "Done", status: "completed" },
    { id: "b", title: "Waiting", status: "pending", depends_on: ["a", "c"] },
  ]);
  assert.deepEqual(
    teamTaskBlockedBy(
      pendingWithDependency[1]!,
      pendingWithDependency,
    ),
    ["c"],
  );
});

test("status aliases normalize onto the four board columns", () => {
  assert.equal(normalizeTeamTaskStatus("running"), "in_progress");
  assert.equal(normalizeTeamTaskStatus("waiting"), "blocked");
  assert.equal(normalizeTeamTaskStatus("complete"), "completed");
});

test("team header counts teammates and labels the extra agent coordinator", () => {
  assert.equal(teamHeaderLabel(4), "4 workers + coordinator");
  assert.equal(teamHeaderLabel(1), "1 worker + coordinator");
  assert.equal(teamHeaderLabel(0), "0 workers + coordinator");
  const summary = parseTeamSummary({
    team: {
      team_id: "team-1",
      task_id: "task-1",
      lead: "lead",
      members: [
        { id: "m1", agent: "lead", role: "lead" },
        { id: "m2", agent: "w1", role: "teammate" },
        { id: "m3", agent: "w2", role: "teammate" },
      ],
    },
  });
  assert.ok(summary);
  assert.equal(summary.worker_count, 2);
  assert.equal(summary.lead, "lead");
  assert.equal(teamHeaderLabel(summary.worker_count), "2 workers + coordinator");
});

test("author roles label the coordinator and teammates from the roster", () => {
  const summary = parseTeamSummary({
    team: {
      team_id: "team-1",
      lead: "coord",
      members: [
        { id: "m1", agent: "coord", role: "lead" },
        { id: "m2", agent: "w1", role: "teammate" },
      ],
    },
  });
  assert.ok(summary);
  assert.equal(teamAuthorRole("coord", summary), "coordinator");
  assert.equal(teamAuthorRole("m1", summary), "coordinator");
  assert.equal(teamAuthorRole("w1", summary), "teammate");
  assert.equal(teamAuthorRole("m2", summary), "teammate");
  assert.equal(teamAuthorRole("outsider", summary), null);
  assert.equal(teamAuthorRole(null, summary), null);
  assert.equal(teamAuthorRole("coord", null), null);
  assert.equal(teamAuthorRoleLabel("coordinator"), "Coordinator");
  assert.equal(teamAuthorRoleLabel("teammate"), "Teammate");
  assert.equal(teamAuthorRoleLabel(null), null);
});

test("workspace polling pauses whenever the document is not visible", () => {
  assert.equal(TEAM_WORKSPACE_POLL_MS, 5000);
  assert.equal(shouldPollTeamWorkspace("visible"), true);
  assert.equal(shouldPollTeamWorkspace("hidden"), false);
  assert.equal(shouldPollTeamWorkspace("prerender"), false);
  assert.equal(shouldPollTeamWorkspace(undefined), false);
});

// ---------------------------------------------------------------------------
// Mailbox Inbox/Sent filter
// ---------------------------------------------------------------------------

test("mailbox rows separate Inbox from Sent", () => {
  const inbox = parseTeamMessages(
    {
      messages: [
        {
          id: "m1",
          sender: "alice",
          recipient: "lead",
          subject: "Found a source",
          body: "The primary source is here.",
          task_id: "t1",
          created_at: "2026-11-10T09:00:00Z",
          read_at: null,
        },
        {
          id: "m2",
          from_agent: "bob",
          to_agents: ["lead", "alice"],
          subject: "Question",
          text: "Which line?",
          is_read: true,
        },
      ],
    },
    "inbox",
  );
  const sent = parseTeamMessages(
    {
      sent: [
        {
          id: "m3",
          sender: "lead",
          recipient: "alice",
          subject: "Re: Found a source",
          body: "Thanks.",
        },
      ],
    },
    "sent",
  );
  const all = [...inbox, ...sent];

  assert.deepEqual(
    mailboxRows(all, "inbox").map((message) => message.id),
    ["m1", "m2"],
  );
  assert.deepEqual(
    mailboxRows(all, "sent").map((message) => message.id),
    ["m3"],
  );
  assert.equal(inbox[0]?.read, false);
  assert.equal(inbox[1]?.read, true);
  assert.equal(inbox[1]?.recipient, "lead, alice");
  assert.equal(inbox[0]?.task_id, "t1");
  assert.equal(sent[0]?.box, "sent");
});

// ---------------------------------------------------------------------------
// Mission log kinds
// ---------------------------------------------------------------------------

test("mission log parses all six kind badges and aliases", () => {
  const entries = parseMissionLog({
    mission: [
      { id: 1, kind: "progress", summary: "Scouted sources" },
      { id: 2, kind: "handoff", summary: "Passing to writer" },
      { id: 3, kind: "blocked", summary: "Missing access" },
      { id: 4, kind: "decision", summary: "Use plan B" },
      { id: 5, kind: "done", summary: "Delivered" },
      { id: 6, kind: "error", summary: "Tool failed" },
      { id: 7, kind: "hand_off", summary: "Alias handoff" },
      { id: 8, kind: "failed", summary: "Alias error" },
    ],
  });
  assert.deepEqual(
    entries.map((entry) => entry.kind),
    [
      "progress",
      "handoff",
      "blocked",
      "decision",
      "done",
      "error",
      "handoff",
      "error",
    ],
  );
  assert.deepEqual(
    entries.slice(0, 6).map((entry) => missionKindLabel(entry.kind)),
    ["Progress", "Handoff", "Blocked", "Decision", "Done", "Error"],
  );
  assert.equal(normalizeMissionKind("handover"), "handoff");
  assert.equal(normalizeMissionKind("weird"), "unknown");
  assert.equal(missionKindLabel("weird"), "weird");
  assert.equal(missionKindVariant("done"), "success");
  assert.equal(missionKindVariant("error"), "destructive");
});

test("mission log keeps evidence and next action", () => {
  const [entry] = parseMissionLog([
    {
      id: "e1",
      kind: "decision",
      summary: "Ship the draft",
      evidence: "All sections approved",
      next_action: "Finalize outcome",
      actor: "lead",
      created_at: "2026-11-10T10:00:00Z",
    },
  ]);
  assert.ok(entry);
  assert.equal(entry.evidence, "All sections approved");
  assert.equal(entry.next_action, "Finalize outcome");
  assert.equal(entry.actor, "lead");
});

// ---------------------------------------------------------------------------
// Outcomes: statuses, sections, fragment review, finalize state
// ---------------------------------------------------------------------------

test("outcome status and fragment review labels are humanized", () => {
  assert.equal(normalizeOutcomeStatus("in-review"), "in_review");
  assert.equal(normalizeOutcomeStatus("FINALIZED"), "finalized");
  assert.equal(outcomeStatusLabel("draft"), "Draft");
  assert.equal(outcomeStatusLabel("in_review"), "In review");
  assert.equal(outcomeStatusLabel("finalized"), "Finalized");

  assert.equal(normalizeReviewStatus("changes-requested"), "changes_requested");
  assert.equal(fragmentReviewLabel("pending"), "Pending review");
  assert.equal(fragmentReviewLabel("approved"), "Reviewed");
  assert.equal(fragmentReviewLabel("rejected"), "Rejected");
  assert.equal(
    fragmentReviewLabel("changes_requested"),
    "Changes requested",
  );
});

test("outcome sections and finalize state track fragment approval", () => {
  const [draft, review, ready, finalized] = parseTeamOutcomes({
    outcomes: [
      {
        id: "o1",
        title: "Draft",
        status: "draft",
        required_sections: ["Summary", "Evidence"],
        fragments: [
          {
            id: "f1",
            section: "Summary",
            content: "A summary",
            source_agent: "alice",
            review_status: "approved",
          },
        ],
      },
      {
        id: "o2",
        title: "Review",
        status: "in_review",
        required_sections: ["Summary"],
        fragments: [
          {
            id: "f2",
            section: "Summary",
            content: "Text",
            source_agent: "bob",
            review_status: "pending",
          },
        ],
      },
      {
        id: "o3",
        title: "Ready",
        status: "in_review",
        required_sections: ["Summary", "Evidence"],
        fragments: [
          { id: "f3", section: "summary", review_status: "approved" },
          { id: "f4", section: "Evidence", review_status: "accepted" },
        ],
      },
      {
        id: "o4",
        title: "Final",
        status: "finalized",
        required_sections: ["Summary"],
        fragments: [
          { id: "f5", section: "Summary", review_status: "approved" },
        ],
        finalized_at: "2026-11-10T11:00:00Z",
      },
    ],
  });

  assert.ok(draft && review && ready && finalized);
  assert.equal(draft.fragments[0]?.source_agent, "alice");
  assert.equal(draft.fragments[0]?.review_status, "approved");
  assert.deepEqual(outcomeSectionCoverage(draft).missing, ["Evidence"]);
  assert.equal(outcomeFinalizeState(draft).label, "1 required section missing");

  assert.equal(outcomeSectionCoverage(review).awaiting.length, 1);
  assert.equal(outcomeFinalizeState(review).label, "Awaiting review");

  assert.equal(outcomeSectionCoverage(ready).missing.length, 0);
  assert.equal(outcomeFinalizeState(ready).ready, true);
  assert.equal(outcomeFinalizeState(ready).label, "Ready to finalize");

  assert.equal(outcomeFinalizeState(finalized).label, "Finalized");
  assert.equal(outcomeFinalizeState(finalized).variant, "success");

  const [unsectioned] = parseTeamOutcomes([
    {
      id: "o5",
      title: "Unsectioned",
      status: "in_review",
      required_sections: [],
      fragments: [
        { id: "f6", section: "Summary", review_status: "draft" },
      ],
    },
  ]);
  assert.ok(unsectioned);
  assert.equal(outcomeFinalizeState(unsectioned).label, "Awaiting review");
});

// ---------------------------------------------------------------------------
// Runs
// ---------------------------------------------------------------------------

test("run counts split active and queued work", () => {
  const counts = teamRunCounts([
    { id: "r1", agent: "w1", task_id: null, task_title: null, status: "running", progress: 20, progress_message: "", activity: "", result_summary: "", error: null, created_at: null, updated_at: null },
    { id: "r2", agent: "w2", task_id: null, task_title: null, status: "queued", progress: 0, progress_message: "", activity: "", result_summary: "", error: null, created_at: null, updated_at: null },
    { id: "r3", agent: "w3", task_id: null, task_title: null, status: "completed", progress: 100, progress_message: "", activity: "", result_summary: "", error: null, created_at: null, updated_at: null },
    { id: "r4", agent: "w4", task_id: null, task_title: null, status: "failed", progress: 50, progress_message: "", activity: "", result_summary: "", error: null, created_at: null, updated_at: null },
  ]);
  assert.deepEqual(counts, {
    total: 4,
    active: 1,
    queued: 1,
    completed: 1,
    failed: 1,
  });
});

// ---------------------------------------------------------------------------
// Scale approvals: status handling
// ---------------------------------------------------------------------------

test("scale approval statuses accept allow/deny spellings", () => {
  assert.equal(normalizeScaleApprovalStatus("pending"), "pending");
  assert.equal(normalizeScaleApprovalStatus("allowed"), "granted");
  assert.equal(normalizeScaleApprovalStatus("approved"), "granted");
  assert.equal(normalizeScaleApprovalStatus("deny"), "denied");
  assert.equal(normalizeScaleApprovalStatus("expired"), "expired");
  assert.equal(normalizeScaleApprovalStatus("??? "), "unknown");

  const [approval] = parseScaleApprovals({
    approvals: [
      {
        id: "s1",
        task_id: "t1",
        team_id: "team-1",
        requested_workers: 24,
        current_cap: 8,
        reason: "Need more researchers",
        status: "allowed",
        created_at: "2026-11-10T09:00:00Z",
        expires_at: "2026-11-10T09:15:00Z",
      },
    ],
  });
  assert.ok(approval);
  assert.equal(approval.status, "granted");
  assert.equal(approval.requested_workers, 24);
  assert.equal(approval.current_cap, 8);
});

test("scale approval parsing defaults the cap to 8 and rejects missing ids", () => {
  const parsed = parseScaleApproval({
    id: "s2",
    task_id: "t1",
    requested_workers: "16",
    status: "pending",
  });
  assert.ok(parsed);
  assert.equal(parsed.current_cap, 8);
  assert.equal(parsed.requested_workers, 16);
  assert.equal(parsed.status, "pending");
  assert.equal(parseScaleApproval({ requested_workers: 9 }), null);
});

// ---------------------------------------------------------------------------
// Fetch helpers: 404 means an empty state, not an error
// ---------------------------------------------------------------------------

test("old servers without team routes produce an empty workspace", async () => {
  globalThis.fetch = (async () => jsonResponse({ error: "not found" }, 404)) as typeof fetch;
  const workspace = await loadTeamWorkspace("task-1");
  assert.equal(workspace.summary, null);
  assert.deepEqual(workspace.tasks, []);
  assert.deepEqual(workspace.inbox, []);
  assert.deepEqual(workspace.sent, []);
  assert.deepEqual(workspace.mission, []);
  assert.deepEqual(workspace.outcomes, []);
  assert.deepEqual(workspace.runs, []);
});

test("missing scale approval route reads as an empty queue", async () => {
  globalThis.fetch = (async () => jsonResponse({ error: "not found" }, 404)) as typeof fetch;
  assert.deepEqual(await listScaleApprovals(), []);
});

test("scale decisions post the allow/deny body to the decision route", async () => {
  const calls: Array<{ url: string; init?: RequestInit }> = [];
  globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    calls.push({ url: String(input), init });
    return jsonResponse({
      approval: {
        id: "s1",
        task_id: "task-1",
        requested_workers: 24,
        current_cap: 8,
        status: "denied",
      },
    });
  }) as typeof fetch;

  const decided = await decideScaleApproval("s1", "deny");
  assert.ok(decided);
  assert.equal(decided.status, "denied");
  assert.equal(calls[0]?.url, "/api/scale-approvals/s1/decision");
  assert.equal(calls[0]?.init?.method, "POST");
  assert.deepEqual(JSON.parse(String(calls[0]?.init?.body)), {
    decision: "deny",
  });
});

test("parsers accept the core team row field names", () => {
  const summary = parseTeamSummary({
    team_id: "task-1",
    members: [
      {
        team_id: "task-1",
        agent_id: "lead",
        role: "lead",
        status: "active",
        description: "Lead",
      },
      {
        team_id: "task-1",
        agent_id: "w1",
        role: "teammate",
        status: "idle",
        description: "Writer",
      },
    ],
  });
  assert.ok(summary);
  assert.equal(summary.lead, "lead");
  assert.equal(summary.worker_count, 1);
  assert.equal(summary.members[1]?.agent, "w1");

  const [task] = parseTeamTasks([
    {
      id: "t1",
      team_id: "task-1",
      title: "Core task",
      description: "",
      status: "blocked",
      created_by: "lead",
      assignee: "w1",
      depends_on: ["t0"],
      summary: "",
      blocked_reason: "Waiting on access",
    },
  ]);
  assert.ok(task);
  assert.deepEqual(task.dependencies, ["t0"]);
  assert.equal(task.blocked_reason, "Waiting on access");
  assert.equal(task.created_by, "lead");

  const [message] = parseTeamMessages(
    {
      messages: [
        {
          id: 7,
          team_id: "task-1",
          from_agent_id: "w1",
          to_agent_id: null,
          subject: "Update",
          body: "Done",
          task_id: "t1",
          sent_at: "2026-11-10T09:00:00Z",
          read_at: null,
        },
      ],
    },
    "inbox",
  );
  assert.ok(message);
  assert.equal(message.sender, "w1");
  assert.equal(message.recipient, "all");
  assert.equal(message.read, false);

  const [mission] = parseMissionLog([
    {
      id: 1,
      team_id: "task-1",
      agent_id: "w1",
      task_id: "t1",
      kind: "handoff",
      summary: "Passing to review",
      evidence: "",
      next_action: "Review",
      created_at: "2026-11-10T09:00:00Z",
    },
  ]);
  assert.ok(mission);
  assert.equal(mission.actor, "w1");

  const [run] = parseTeamRuns([
    {
      id: "r1",
      team_id: "task-1",
      agent_id: "w1",
      task_id: "t1",
      status: "running",
      current_activity: "Reading sources",
      last_progress_message: "2 of 5 sources",
      result: null,
      error: null,
    },
  ]);
  assert.ok(run);
  assert.equal(run.agent, "w1");
  assert.equal(run.activity, "Reading sources");
  assert.equal(run.progress_message, "2 of 5 sources");

  const [outcome] = parseTeamOutcomes({
    outcomes: [
      {
        id: "o1",
        team_id: "task-1",
        title: "Core outcome",
        status: "in_review",
        required_sections: ["Summary"],
        created_by: "lead",
      },
    ],
    fragments: [
      {
        id: "f1",
        team_id: "task-1",
        outcome_id: "o1",
        section: "Summary",
        source_agent_id: "w1",
        content: "Text",
        status: "reviewed",
      },
    ],
  });
  assert.ok(outcome);
  assert.equal(outcome.fragments.length, 1);
  assert.equal(outcome.fragments[0]?.source_agent, "w1");
  assert.equal(outcome.fragments[0]?.review_status, "approved");
  assert.equal(outcome.created_by, "lead");
  assert.equal(outcomeFinalizeState(outcome).label, "Ready to finalize");
});

test("team fetch parses envelopes and scale approval queue", async () => {
  const urls: string[] = [];
  globalThis.fetch = (async (input: RequestInfo | URL) => {
    const url = String(input);
    urls.push(url);
    if (url.includes("/mailbox?box=inbox")) {
      return jsonResponse({
        messages: [{ id: "i1", sender: "a", recipient: "lead", body: "hi" }],
      });
    }
    if (url.includes("/mailbox?box=sent")) {
      return jsonResponse({ messages: [] });
    }
    if (url.endsWith("/team/tasks")) {
      return jsonResponse({
        tasks: [{ id: "t1", title: "One", status: "pending" }],
      });
    }
    if (url.endsWith("/team")) {
      return jsonResponse({
        team: { team_id: "team-1", worker_count: 3, lead: "lead" },
      });
    }
    if (url.endsWith("/team/mission")) {
      return jsonResponse({ mission: [{ id: 1, kind: "progress", summary: "ok" }] });
    }
    if (url.endsWith("/team/outcomes")) {
      return jsonResponse({ outcomes: [{ id: "o1", title: "Out", status: "draft" }] });
    }
    if (url.endsWith("/team/runs")) {
      return jsonResponse({ runs: [{ id: "r1", agent: "w1", status: "running" }] });
    }
    if (url.endsWith("/scale-approvals")) {
      return jsonResponse({
        approvals: [
          {
            id: "s1",
            task_id: "task-1",
            requested_workers: 12,
            status: "pending",
          } as ScaleApproval,
        ],
      });
    }
    return jsonResponse({ error: "unexpected" }, 500);
  }) as typeof fetch;

  const workspace = await loadTeamWorkspace("task-1");
  assert.equal(workspace.summary?.worker_count, 3);
  assert.equal(workspace.tasks[0]?.title, "One");
  assert.equal(workspace.inbox[0]?.box, "inbox");
  assert.equal(workspace.mission[0]?.kind, "progress");
  assert.equal(workspace.outcomes[0]?.status, "draft");
  assert.equal(workspace.runs[0]?.status, "running");
  assert.ok(urls.every((url) => url.startsWith("/api/")));

  const scale = await listScaleApprovals();
  assert.equal(scale[0]?.requested_workers, 12);
  assert.equal(scale[0]?.current_cap, 8);
});
