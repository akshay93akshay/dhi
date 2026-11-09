import assert from "node:assert/strict";
import { afterEach, test } from "node:test";
import {
  ApiError,
  addGoal,
  askSideQuestion,
  createRule,
  createTask,
  decideApproval,
  deleteProfile,
  deleteRule,
  deleteTask,
  deleteTasks,
  deleteWebProviderKey,
  getHealth,
  getPoolStatus,
  getSettings,
  getSwarmMessages,
  getTask,
  getTaskActivity,
  getTaskUsage,
  getTaskWork,
  getWebProviders,
  listApprovals,
  listInstructions,
  listModels,
  listProfiles,
  listProjectFolders,
  listRules,
  listSideQuestions,
  listTasks,
  reloadWebProviders,
  resetPool,
  saveProfile,
  saveWebProvider,
  sendInstruction,
  stopTask,
  stopTasks,
  testWebProvider,
  toggleRule,
  updateRule,
  updateSettings,
  updateTaskCapacity,
} from "./api.ts";
import {
  TASK_STREAM_RESUME_DELAY_MS,
  connectTaskStream,
  parseActivityEvent,
  parseEventId,
  parseFrame,
  parseHelloData,
} from "./sse.ts";
import type { Activity, SideQuestionSession, Task } from "./types.ts";

const originalFetch = globalThis.fetch;

afterEach(() => {
  globalThis.fetch = originalFetch;
});

/** Stubs `globalThis.fetch`, recording every call for assertions. */
function stubFetch(
  respond: (url: string, init?: RequestInit) => Response | Promise<Response>,
): { urls: string[]; calls: Array<{ url: string; init?: RequestInit }> } {
  const recorder = {
    urls: [] as string[],
    calls: [] as Array<{ url: string; init?: RequestInit }>,
  };
  globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    recorder.urls.push(url);
    recorder.calls.push({ url, init });
    return respond(url, init);
  }) as typeof fetch;
  return recorder;
}

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

const sampleTask: Task = {
  id: "00000000-0000-4000-8000-000000000001",
  goal: "mate in two",
  state: "running",
  exploration_id: "00000000-0000-4000-8000-000000000002",
  origin_fact_id: "00000000-0000-4000-8000-000000000003",
  pinned_planner_profile_id: null,
  pinned_worker_profile_id: null,
  capacity: {
    max_rounds: 80,
    max_model_calls: null,
    max_wall_secs: null,
    max_parallel_workers: 4,
  },
  topology: "manager_worker",
  swarm_workers: null,
  created_at: "2026-10-30T00:00:00Z",
  updated_at: "2026-10-30T00:00:00Z",
};

const sampleActivity: Activity = {
  id: 1,
  task_id: sampleTask.id,
  kind: "task_created",
  message: "Task created: mate in two",
  payload: {},
  created_at: "2026-10-30T00:00:00Z",
};

const sampleGoal = {
  id: "00000000-0000-4000-8000-000000000009",
  task_id: sampleTask.id,
  description: "Find a second mating line",
  state: "open" as const,
  proof_fact_id: null,
  created_at: "2026-10-30T00:10:00Z",
};

const sampleSideRequest = {
  id: "00000000-0000-4000-8000-000000000008",
  session_id: "00000000-0000-4000-8000-000000000007",
  question: "Why did this finish?",
  answer: "advisory: the snapshot shows a proof.",
  status: "ok" as const,
  error: null,
  model: "river-worker",
  tokens_in: 512,
  tokens_out: 84,
  latency_ms: 900,
  created_at: "2026-10-30T00:10:01Z",
  completed_at: "2026-10-30T00:10:01.900Z",
};

const sampleSideSession: SideQuestionSession = {
  id: "00000000-0000-4000-8000-000000000007",
  created_at: "2026-10-30T00:10:00Z",
  closed_at: null,
  status: "open",
  snapshot_summary: {
    goal: "mate in two",
    state: "done",
    goals: 1,
    facts: 3,
    activity: 12,
  },
  requests: [sampleSideRequest],
};

// ---------------------------------------------------------------------------
// Envelope unwrapping + routing
// ---------------------------------------------------------------------------

test("listTasks unwraps the { tasks } envelope from GET /api/tasks", async () => {
  const recorder = stubFetch(() => jsonResponse({ tasks: [sampleTask] }));

  const tasks = await listTasks();
  assert.equal(recorder.urls[0], "/api/tasks");
  assert.deepEqual(tasks, [sampleTask]);
});

test("listTasks forwards the limit query parameter", async () => {
  const recorder = stubFetch(() => jsonResponse({ tasks: [] }));

  await listTasks({ limit: 5 });
  await listTasks({ limit: 1000 });

  assert.deepEqual(recorder.urls, [
    "/api/tasks?limit=5",
    "/api/tasks?limit=1000",
  ]);
});

test("listProjectFolders unwraps the { folders } envelope", async () => {
  const folder = {
    path: "/srv/projects/build",
    name: "Build",
    agents_md_present: true,
  };
  const recorder = stubFetch(() => jsonResponse({ folders: [folder] }));

  const folders = await listProjectFolders();

  assert.equal(recorder.urls[0], "/api/project-folders");
  assert.equal(recorder.calls[0]?.init?.method, undefined);
  assert.deepEqual(folders, [folder]);
});

test("listProjectFolders keeps an empty listing", async () => {
  const recorder = stubFetch(() => jsonResponse({ folders: [] }));

  const folders = await listProjectFolders();

  assert.equal(recorder.urls[0], "/api/project-folders");
  assert.deepEqual(folders, []);
});

test("getHealth reads the bare health object", async () => {
  const recorder = stubFetch(() =>
    jsonResponse({ status: "ok", service: "dhi", version: "0.1.0" }),
  );

  const health = await getHealth();
  assert.equal(recorder.urls[0], "/api/health");
  assert.equal(recorder.calls[0]?.init?.method, undefined);
  assert.deepEqual(health, { status: "ok", service: "dhi", version: "0.1.0" });
});

test("createTask posts the goal/pins body to POST /api/tasks", async () => {
  const recorder = stubFetch((_url, init) => {
    assert.equal(init?.method, "POST");
    return jsonResponse(sampleTask, 201);
  });

  const task = await createTask({
    goal: "mate in two",
    planner_profile_id: 7,
    worker_profile_id: null,
  });

  assert.equal(recorder.urls[0], "/api/tasks");
  assert.equal(
    recorder.calls[0]?.init?.body,
    JSON.stringify({
      goal: "mate in two",
      planner_profile_id: 7,
      worker_profile_id: null,
    }),
  );
  assert.deepEqual(task, sampleTask);
});

test("createTask omits absent pin fields (server defaults apply)", async () => {
  const recorder = stubFetch(() => jsonResponse(sampleTask, 201));

  await createTask({ goal: "solo" });

  assert.equal(recorder.calls[0]?.init?.body, JSON.stringify({ goal: "solo" }));
});

test("createTask forwards topology and swarm_workers, including explicit nulls", async () => {
  const recorder = stubFetch(() => jsonResponse(sampleTask, 201));

  await createTask({ goal: "swarm task", topology: "swarm", swarm_workers: 4 });
  await createTask({
    goal: "planner task",
    topology: null,
    swarm_workers: null,
  });

  assert.deepEqual(
    recorder.calls.map((call) => JSON.parse(String(call.init?.body))),
    [
      { goal: "swarm task", topology: "swarm", swarm_workers: 4 },
      { goal: "planner task", topology: null, swarm_workers: null },
    ],
  );
});

test("createTask forwards project_folder and load_agents_md", async () => {
  const recorder = stubFetch(() => jsonResponse(sampleTask, 201));

  await createTask({
    goal: "project task",
    project_folder: "/srv/projects/build",
    load_agents_md: true,
  });

  assert.deepEqual(JSON.parse(String(recorder.calls[0]?.init?.body)), {
    goal: "project task",
    project_folder: "/srv/projects/build",
    load_agents_md: true,
  });
});

test("getTask reads a bare task by id", async () => {
  const recorder = stubFetch(() => jsonResponse(sampleTask));

  const task = await getTask(sampleTask.id);
  assert.equal(recorder.urls[0], `/api/tasks/${sampleTask.id}`);
  assert.deepEqual(task, sampleTask);
});

test("addGoal posts the description and returns {task, goal}", async () => {
  const recorder = stubFetch(() =>
    jsonResponse({
      task: { ...sampleTask, state: "running" },
      goal: sampleGoal,
    }),
  );

  const result = await addGoal(sampleTask.id, "Find a second mating line");

  assert.equal(recorder.urls[0], `/api/tasks/${sampleTask.id}/goals`);
  assert.equal(recorder.calls[0]?.init?.method, "POST");
  assert.equal(
    recorder.calls[0]?.init?.body,
    JSON.stringify({ description: "Find a second mating line" }),
  );
  assert.equal(result.task.state, "running");
  assert.deepEqual(result.goal, sampleGoal);
});

test("addGoal maps 409 to the conflict helper and keeps the server message", async () => {
  stubFetch(() =>
    jsonResponse(
      {
        error:
          "task 00000000-0000-4000-8000-000000000001 is running; only a done, failed, or timeout task can be reopened",
      },
      409,
    ),
  );

  const error = await addGoal(sampleTask.id, "too early").then(
    () => {
      throw new Error("addGoal must reject a 409");
    },
    (thrown: unknown) => thrown,
  );

  assert.ok(error instanceof ApiError);
  assert.equal(error.status, 409);
  assert.equal(error.isConflict(), true);
  assert.match(error.message, /is running/);
});

test("stopTask POSTs to the task stop route and reads the 202 body", async () => {
  const recorder = stubFetch(() =>
    jsonResponse({ task_id: sampleTask.id, status: "stopping" }, 202),
  );

  const response = await stopTask(sampleTask.id);

  assert.equal(recorder.urls[0], `/api/tasks/${sampleTask.id}/stop`);
  assert.equal(recorder.calls[0]?.init?.method, "POST");
  assert.deepEqual(response, {
    task_id: sampleTask.id,
    status: "stopping",
  });
});

test("stopTasks POSTs { ids } and returns stopping/blocked outcomes", async () => {
  const blocked = {
    id: "00000000-0000-4000-8000-000000000004",
    state: "done",
    error: "task is already terminal",
  };
  const recorder = stubFetch((_url, init) => {
    assert.equal(init?.method, "POST");
    assert.equal(
      init?.body,
      JSON.stringify({ ids: [sampleTask.id, blocked.id] }),
    );
    return jsonResponse({ stopping: [sampleTask.id], blocked: [blocked] });
  });

  const response = await stopTasks([sampleTask.id, blocked.id]);

  assert.equal(recorder.urls[0], "/api/tasks/stop");
  assert.deepEqual(response, {
    stopping: [sampleTask.id],
    blocked: [blocked],
  });
});

test("deleteTask sends DELETE and accepts a 204 without parsing JSON", async () => {
  const recorder = stubFetch(() => new Response(null, { status: 204 }));

  const result = await deleteTask(sampleTask.id);

  assert.equal(recorder.urls[0], `/api/tasks/${sampleTask.id}`);
  assert.equal(recorder.calls[0]?.init?.method, "DELETE");
  assert.equal(result, undefined);
});

test("deleteTasks POSTs { ids } and returns deleted/blocked outcomes", async () => {
  const blocked = {
    id: "00000000-0000-4000-8000-000000000005",
    state: "running",
    error: "task is still active",
  };
  const recorder = stubFetch((_url, init) => {
    assert.equal(init?.method, "POST");
    assert.equal(
      init?.body,
      JSON.stringify({ ids: [sampleTask.id, blocked.id] }),
    );
    return jsonResponse({ deleted: [sampleTask.id], blocked: [blocked] });
  });

  const response = await deleteTasks([sampleTask.id, blocked.id]);

  assert.equal(recorder.urls[0], "/api/tasks/delete");
  assert.deepEqual(response, {
    deleted: [sampleTask.id],
    blocked: [blocked],
  });
});

test("getTaskWork returns the { task, goals, intents, facts, results } shape", async () => {
  const goal = {
    id: "g1",
    task_id: sampleTask.id,
    description: "mate in two",
    state: "open" as const,
    proof_fact_id: null,
    created_at: "2026-10-30T00:00:00Z",
  };
  const fact = {
    id: sampleTask.origin_fact_id,
    task_id: sampleTask.id,
    claim: "Origin: mate in two",
    state: "origin" as const,
    provenance: { goal: "mate in two" },
    origin_of: null,
    created_at: "2026-10-30T00:00:00Z",
  };
  const result = {
    id: "r1",
    task_id: sampleTask.id,
    intent_id: "i1",
    claim: "found it",
    acceptance: "accepted" as const,
    evidence: { moves: 2 },
    created_at: "2026-10-30T00:00:00Z",
  };
  const recorder = stubFetch(() =>
    jsonResponse({
      task: sampleTask,
      goals: [goal],
      intents: [],
      facts: [fact],
      results: [result],
    }),
  );

  const work = await getTaskWork(sampleTask.id);
  assert.equal(recorder.urls[0], `/api/tasks/${sampleTask.id}/work`);
  assert.deepEqual(work.task, sampleTask);
  assert.deepEqual(work.goals, [goal]);
  assert.deepEqual(work.intents, []);
  assert.deepEqual(work.facts, [fact]);
  assert.deepEqual(work.results, [result]);
  // The handler serves no `digests` collection.
  assert.equal("digests" in work, false);
});

test("updateTaskCapacity POSTs the patch and returns the updated task", async () => {
  const updated: Task = {
    ...sampleTask,
    capacity: {
      max_rounds: 100,
      max_model_calls: 120,
      max_wall_secs: null,
      max_parallel_workers: 4,
    },
  };
  const recorder = stubFetch((_url, init) => {
    assert.equal(init?.method, "POST");
    assert.equal(
      init?.body,
      JSON.stringify({ max_rounds: 100, max_model_calls: 120 }),
    );
    return jsonResponse(updated);
  });

  const task = await updateTaskCapacity(sampleTask.id, {
    max_rounds: 100,
    max_model_calls: 120,
  });

  assert.equal(recorder.urls[0], `/api/tasks/${sampleTask.id}/capacity`);
  assert.deepEqual(task, updated);
});

test("updateTaskCapacity exposes the terminal-task 409 as a conflict", async () => {
  stubFetch(() =>
    jsonResponse(
      {
        error: `task ${sampleTask.id} is done; its capacity cannot be extended`,
      },
      409,
    ),
  );

  const error = await updateTaskCapacity(sampleTask.id, {
    max_rounds: 90,
  }).then(
    () => {
      throw new Error("updateTaskCapacity must reject a 409");
    },
    (thrown: unknown) => thrown,
  );

  assert.ok(error instanceof ApiError);
  assert.equal(error.status, 409);
  assert.equal(error.isConflict(), true);
  assert.match(error.message, /capacity cannot be extended/);
});

test("getSwarmMessages unwraps { messages } and forwards the limit", async () => {
  const record = {
    id: 7,
    task_id: sampleTask.id,
    intent_id: "11111111-2222-4333-8444-555555555555",
    kind: "finding",
    payload: { type: "finding", from: "worker-a", summary: "found it" },
    created_at: "2026-11-07T10:00:00Z",
  };
  const recorder = stubFetch(() => jsonResponse({ messages: [record] }));

  const defaulted = await getSwarmMessages(sampleTask.id);
  const explicit = await getSwarmMessages(sampleTask.id, 50);

  assert.deepEqual(recorder.urls, [
    `/api/tasks/${sampleTask.id}/swarm-messages?limit=200`,
    `/api/tasks/${sampleTask.id}/swarm-messages?limit=50`,
  ]);
  assert.equal(recorder.calls[0]?.init?.method, undefined);
  assert.deepEqual(defaulted, [record]);
  assert.deepEqual(explicit, [record]);
});

const sampleInstruction = {
  id: "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
  task_id: sampleTask.id,
  text: "focus on the rook line",
  created_at: "2026-11-09T10:00:00Z",
  delivered_at: null as string | null,
};

test("listInstructions unwraps the { instructions } envelope", async () => {
  const recorder = stubFetch(() =>
    jsonResponse({ instructions: [sampleInstruction] }),
  );

  const instructions = await listInstructions(sampleTask.id);
  assert.equal(recorder.urls[0], `/api/tasks/${sampleTask.id}/instructions`);
  assert.equal(recorder.calls[0]?.init?.method, undefined);
  assert.deepEqual(instructions, [sampleInstruction]);
});

test("sendInstruction posts { text } and returns the created row", async () => {
  const delivered = {
    ...sampleInstruction,
    delivered_at: "2026-11-09T10:00:05Z",
  };
  const recorder = stubFetch((_url, init) => {
    assert.equal(init?.method, "POST");
    assert.equal(
      init?.body,
      JSON.stringify({ text: "focus on the rook line" }),
    );
    return jsonResponse(delivered, 201);
  });

  const instruction = await sendInstruction(
    sampleTask.id,
    "focus on the rook line",
  );
  assert.equal(recorder.urls[0], `/api/tasks/${sampleTask.id}/instructions`);
  assert.deepEqual(instruction, delivered);
});

test("sendInstruction maps the terminal-task 409 to a conflict", async () => {
  stubFetch(() =>
    jsonResponse(
      {
        error: `task ${sampleTask.id} is done; instructions cannot be added to a terminal task`,
      },
      409,
    ),
  );

  const error = await sendInstruction(sampleTask.id, "too late").then(
    () => null,
    (thrown: unknown) => thrown,
  );
  assert.ok(error instanceof ApiError);
  assert.equal(error.status, 409);
  assert.equal(error.isConflict(), true);
  assert.match(error.message, /instructions cannot be added/);
});

test("getTaskActivity unwraps the { activity } envelope", async () => {
  const recorder = stubFetch(() =>
    jsonResponse({ activity: [sampleActivity] }),
  );

  const activity = await getTaskActivity(sampleTask.id);
  assert.equal(recorder.urls[0], `/api/tasks/${sampleTask.id}/activity`);
  assert.deepEqual(activity, [sampleActivity]);
});

test("getTaskUsage unwraps the { runs } envelope", async () => {
  const run = {
    id: "run-1",
    run_id: "run-1",
    call_index: 0,
    task_id: sampleTask.id,
    intent_id: null,
    role: "planner" as const,
    model: "river-model",
    profile: "openai/river-model/max_tokens=4096/stream=true",
    prompt: "...",
    prompt_hash: "planner@...",
    variables: {},
    response: "...",
    thinking: "Compare the pinned pieces before moving.",
    tokens_in: 435,
    tokens_out: 86,
    cache_read_tokens: 0,
    cache_write_tokens: 0,
    reasoning_tokens: 0,
    cost: 0,
    status: "ok" as const,
    error: null,
    latency_ms: 120,
    checkpoint_id: null,
    created_at: "2026-10-30T00:00:00Z",
    updated_at: "2026-10-30T00:00:00Z",
  };
  const recorder = stubFetch(() => jsonResponse({ runs: [run] }));

  const runs = await getTaskUsage(sampleTask.id);
  assert.equal(recorder.urls[0], `/api/tasks/${sampleTask.id}/usage`);
  assert.deepEqual(runs, [run]);
});

test("listSideQuestions unwraps the { sessions } envelope", async () => {
  const recorder = stubFetch(() =>
    jsonResponse({ sessions: [sampleSideSession] }),
  );

  const sessions = await listSideQuestions(sampleTask.id);
  assert.equal(recorder.urls[0], `/api/tasks/${sampleTask.id}/side-questions`);
  assert.deepEqual(sessions, [sampleSideSession]);
});

test("askSideQuestion posts the question and omits an absent session_id", async () => {
  const recorder = stubFetch((_url, init) => {
    assert.equal(init?.method, "POST");
    assert.equal(
      init?.body,
      JSON.stringify({ question: "Why did this finish?" }),
    );
    return jsonResponse({
      session_id: sampleSideSession.id,
      request: sampleSideRequest,
      answer: sampleSideRequest.answer,
    });
  });

  const response = await askSideQuestion(sampleTask.id, "Why did this finish?");
  assert.equal(recorder.urls[0], `/api/tasks/${sampleTask.id}/side-questions`);
  assert.equal(response.session_id, sampleSideSession.id);
  assert.deepEqual(response.request, sampleSideRequest);
  assert.equal(response.answer, sampleSideRequest.answer);
});

test("askSideQuestion appends with session_id when one is active", async () => {
  const recorder = stubFetch((_url, init) => {
    assert.equal(
      init?.body,
      JSON.stringify({
        question: "And the evidence?",
        session_id: sampleSideSession.id,
      }),
    );
    return jsonResponse({
      session_id: sampleSideSession.id,
      request: sampleSideRequest,
      answer: sampleSideRequest.answer,
    });
  });

  await askSideQuestion(
    sampleTask.id,
    "And the evidence?",
    sampleSideSession.id,
  );
  assert.equal(recorder.urls[0], `/api/tasks/${sampleTask.id}/side-questions`);
});

test("side-question 400 and 409 responses keep their status helpers", async () => {
  stubFetch(() => jsonResponse({ error: "question must not be empty" }, 400));
  const validation = await askSideQuestion(sampleTask.id, " ").then(
    () => null,
    (err: unknown) => err,
  );
  assert.ok(validation instanceof ApiError);
  assert.equal(validation.status, 400);
  assert.equal(validation.isValidation(), true);

  stubFetch(() =>
    jsonResponse({ error: "side-question session s1 is closed" }, 409),
  );
  const conflict = await askSideQuestion(sampleTask.id, "again", "s1").then(
    () => null,
    (err: unknown) => err,
  );
  assert.ok(conflict instanceof ApiError);
  assert.equal(conflict.status, 409);
  assert.equal(conflict.isConflict(), true);
});

test("side-question 502 carries the answer failure message", async () => {
  stubFetch(() =>
    jsonResponse({ error: "side-question answer failed: provider down" }, 502),
  );
  const error = await askSideQuestion(sampleTask.id, "Why?").then(
    () => null,
    (err: unknown) => err,
  );
  assert.ok(error instanceof ApiError);
  assert.equal(error.status, 502);
  assert.equal(error.message, "side-question answer failed: provider down");
});

test("listApprovals unwraps the { approvals } envelope with flattened tool fields", async () => {
  const approval = {
    id: "a1",
    task_id: sampleTask.id,
    tool_call_id: "c1",
    guard_decision_id: "d1",
    status: "pending" as const,
    reason: "needs a person",
    expires_at: "2026-10-30T00:15:00Z",
    decided_at: null,
    created_at: "2026-10-30T00:00:00Z",
    tool_name: "finishTurn",
    input: { move: "Qh5" },
  };
  const recorder = stubFetch(() => jsonResponse({ approvals: [approval] }));

  const approvals = await listApprovals();
  assert.equal(recorder.urls[0], "/api/approvals");
  assert.deepEqual(approvals, [approval]);
});

test("decideApproval posts { decision } and returns { approval, task }", async () => {
  const recorder = stubFetch((_url, init) => {
    assert.equal(init?.method, "POST");
    assert.equal(init?.body, JSON.stringify({ decision: "deny" }));
    return jsonResponse({
      approval: {
        id: "a1",
        task_id: sampleTask.id,
        tool_call_id: "c1",
        guard_decision_id: "d1",
        status: "denied",
        reason: "needs a person",
        expires_at: "2026-10-30T00:15:00Z",
        decided_at: "2026-10-30T00:01:00Z",
        created_at: "2026-10-30T00:00:00Z",
      },
      task: sampleTask,
    });
  });

  const decision = await decideApproval("a1", "deny");
  assert.equal(recorder.urls[0], "/api/approvals/a1/decision");
  assert.equal(decision.approval.status, "denied");
  assert.deepEqual(decision.task, sampleTask);
});

// ---------------------------------------------------------------------------
// LLM profiles / models / pool
// ---------------------------------------------------------------------------

const sampleProfile = {
  id: 1,
  name: "planner-default",
  format: "openai" as const,
  base_url: "https://api.river.ai/v1",
  model: "river-planner",
  api_key_hint: "…9999",
  max_tokens: 0,
  context_window_k: 128,
  thinking_type: "enabled" as const,
  reasoning_effort: "xhigh" as const,
  streaming: true,
  priority: 0,
  pool_exclude: false,
  created_at: "2026-10-30T00:00:00+00:00",
  updated_at: "2026-10-30T00:00:00+00:00",
};

test("listProfiles unwraps the { profiles } envelope", async () => {
  const recorder = stubFetch(() => jsonResponse({ profiles: [sampleProfile] }));

  const profiles = await listProfiles();
  assert.equal(recorder.urls[0], "/api/llm/profiles");
  assert.deepEqual(profiles, [sampleProfile]);
});

test("saveProfile posts create (no id) and edit (id present) bodies", async () => {
  const recorder = stubFetch(() => jsonResponse({ id: 9 }));

  const created = await saveProfile({
    name: "backup",
    format: "openai",
    base_url: "https://api.backup.ai/v1",
    model: "river-xl",
    api_key: "sk-new",
    max_tokens: 0,
    context_window_k: 128,
    streaming: true,
    priority: 100,
    pool_exclude: false,
  });
  assert.equal(recorder.urls[0], "/api/llm/profiles");
  assert.equal(recorder.calls[0]?.init?.method, "POST");
  assert.deepEqual(created, { id: 9 });

  const edited = await saveProfile({
    id: 9,
    name: "backup",
    format: "anthropic",
    model: "claude",
  });
  assert.equal(
    recorder.calls[1]?.init?.body,
    JSON.stringify({
      id: 9,
      name: "backup",
      format: "anthropic",
      model: "claude",
    }),
  );
  assert.deepEqual(edited, { id: 9 });
});

test("saveProfile forwards both thinking controls, including their empty state", async () => {
  const recorder = stubFetch(() => jsonResponse({ id: 4 }));

  await saveProfile({
    name: "thinking-on",
    format: "openai",
    model: "river-xl",
    thinking_type: "enabled",
    reasoning_effort: "xhigh",
  });
  await saveProfile({
    name: "thinking-off",
    format: "openai",
    model: "river-xl",
    thinking_type: "",
    reasoning_effort: "",
  });

  assert.deepEqual(JSON.parse(String(recorder.calls[0]?.init?.body)), {
    name: "thinking-on",
    format: "openai",
    model: "river-xl",
    thinking_type: "enabled",
    reasoning_effort: "xhigh",
  });
  // Cleared controls are sent as empty strings, not omitted.
  assert.deepEqual(JSON.parse(String(recorder.calls[1]?.init?.body)), {
    name: "thinking-off",
    format: "openai",
    model: "river-xl",
    thinking_type: "",
    reasoning_effort: "",
  });
});

test("deleteProfile sends DELETE and unwraps { deleted }", async () => {
  const recorder = stubFetch(() => jsonResponse({ deleted: 3 }));

  const deleted = await deleteProfile(3);
  assert.equal(recorder.urls[0], "/api/llm/profiles/3");
  assert.equal(recorder.calls[0]?.init?.method, "DELETE");
  assert.deepEqual(deleted, { deleted: 3 });
});

test("listModels defaults to the planner profile and honors profile_id", async () => {
  const recorder = stubFetch(() => jsonResponse({ models: ["id-1", "id-2"] }));

  assert.deepEqual(await listModels(), { models: ["id-1", "id-2"] });
  await listModels(7);
  assert.deepEqual(recorder.urls, [
    "/api/llm/models",
    "/api/llm/models?profile_id=7",
  ]);
});

test("getPoolStatus returns { planner, worker } chains with breaker state", async () => {
  const status = {
    planner: {
      chain: [
        {
          id: 1,
          name: "planner-default",
          model: "river-planner",
          tripped: false,
          consecutive_failures: 0,
        },
      ],
    },
    worker: { chain: [] },
  };
  const recorder = stubFetch(() => jsonResponse(status));

  const pool = await getPoolStatus();
  assert.equal(recorder.urls[0], "/api/llm/pool/status");
  assert.deepEqual(pool, status);
});

test("resetPool posts { id } (0 = everything) and returns the pool status", async () => {
  const status = { planner: { chain: [] }, worker: { chain: [] } };
  const recorder = stubFetch(() => jsonResponse(status));

  await resetPool();
  await resetPool(9);

  assert.equal(recorder.urls[0], "/api/llm/pool/reset");
  assert.equal(recorder.calls[0]?.init?.body, JSON.stringify({ id: 0 }));
  assert.equal(recorder.calls[1]?.init?.body, JSON.stringify({ id: 9 }));
});

// ---------------------------------------------------------------------------
// Guard rules
// ---------------------------------------------------------------------------

const sampleRule = {
  id: 1,
  name: "allow-createGame",
  match_target: "tool_name" as const,
  match_type: "exact" as const,
  pattern: "createGame",
  action: "allow" as const,
  reason: "start a new chess game",
  priority: 0,
  enabled: true,
  timeout_secs: 0,
  created_at: "2026-10-30T00:00:00+00:00",
  updated_at: "2026-10-30T00:00:00+00:00",
};

test("listRules unwraps the { rules } envelope", async () => {
  const recorder = stubFetch(() => jsonResponse({ rules: [sampleRule] }));

  const rules = await listRules();
  assert.equal(recorder.urls[0], "/api/intercept/rules");
  assert.deepEqual(rules, [sampleRule]);
});

test("createRule posts the body and returns the BARE row", async () => {
  const recorder = stubFetch(() => jsonResponse(sampleRule));

  const rule = await createRule({
    name: "allow-createGame",
    match_target: "tool_name",
    match_type: "exact",
    pattern: "createGame",
    action: "allow",
    reason: "start a new chess game",
    priority: 0,
    enabled: true,
    timeout_secs: 0,
  });

  assert.equal(recorder.urls[0], "/api/intercept/rules");
  assert.equal(recorder.calls[0]?.init?.method, "POST");
  assert.deepEqual(rule, sampleRule);
});

test("updateRule PUTs a full replace and returns the bare updated row", async () => {
  const { id: _dropped, ...body } = sampleRule;
  void _dropped;
  const updated = { ...sampleRule, action: "deny" as const };
  const recorder = stubFetch((_url, init) => {
    assert.equal(init?.method, "PUT");
    assert.equal(init?.body, JSON.stringify({ ...body, action: "deny" }));
    return jsonResponse(updated);
  });

  const rule = await updateRule(1, { ...body, action: "deny" });

  assert.equal(recorder.urls[0], "/api/intercept/rules/1");
  assert.deepEqual(rule, updated);
});

test("toggleRule posts { enabled } and unwraps { ok, enabled }", async () => {
  const recorder = stubFetch((_url, init) => {
    assert.equal(init?.body, JSON.stringify({ enabled: false }));
    return jsonResponse({ ok: true, enabled: false });
  });

  const toggled = await toggleRule(1, false);
  assert.equal(recorder.urls[0], "/api/intercept/rules/1/toggle");
  assert.deepEqual(toggled, { ok: true, enabled: false });
});

test("deleteRule sends DELETE and unwraps { deleted }", async () => {
  const recorder = stubFetch(() => jsonResponse({ deleted: 4 }));

  const deleted = await deleteRule(4);
  assert.equal(recorder.urls[0], "/api/intercept/rules/4");
  assert.equal(recorder.calls[0]?.init?.method, "DELETE");
  assert.deepEqual(deleted, { deleted: 4 });
});

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

test("getSettings returns the (possibly unseeded) settings view", async () => {
  const unseeded = {
    planner_profile: null,
    worker_profile: null,
    llm_key_set: false,
    planner_api_key_hint: "",
    worker_api_key_hint: "",
    approval_timeout_secs: 900,
    guard_default_action: "deny" as const,
  };
  const recorder = stubFetch(() => jsonResponse(unseeded));

  const settings = await getSettings();
  assert.equal(recorder.urls[0], "/api/settings");
  assert.deepEqual(settings, unseeded);
});

test("updateSettings PUTs a partial patch and returns the merged view", async () => {
  const merged = {
    planner_profile: { id: 2, name: "fast", model: "river-fast" },
    worker_profile: null,
    llm_key_set: true,
    planner_api_key_hint: "…7777",
    worker_api_key_hint: "",
    approval_timeout_secs: 600,
    guard_default_action: "ask" as const,
  };
  const recorder = stubFetch((_url, init) => {
    assert.equal(init?.method, "PUT");
    assert.equal(
      init?.body,
      JSON.stringify({ planner_profile_id: 2, approval_timeout_secs: 600 }),
    );
    return jsonResponse(merged);
  });

  const settings = await updateSettings({
    planner_profile_id: 2,
    approval_timeout_secs: 600,
  });
  assert.equal(recorder.urls[0], "/api/settings");
  assert.deepEqual(settings, merged);
});

// ---------------------------------------------------------------------------
// Web search providers (Settings v2)
// ---------------------------------------------------------------------------

const sampleTinyfish = {
  provider: "tinyfish" as const,
  display_name: "TinyFish",
  configured: true,
  key_hint: "…1a2b",
  source: "database" as const,
  base_url: null,
  timeout_secs: 60,
  enabled: true,
  status: "ready" as const,
};

const sampleProviders = {
  search_read_order: [
    "tinyfish" as const,
    "parallel" as const,
    "firecrawl" as const,
  ],
  research_order: ["parallel" as const],
  providers: [
    sampleTinyfish,
    {
      provider: "parallel" as const,
      display_name: "Parallel",
      configured: false,
      key_hint: null,
      source: "none" as const,
      base_url: null,
      timeout_secs: 60,
      enabled: true,
      status: "missing_key" as const,
    },
    {
      provider: "firecrawl" as const,
      display_name: "Firecrawl",
      configured: true,
      key_hint: "…9f0e",
      source: "environment" as const,
      base_url: "http://127.0.0.1:3002",
      timeout_secs: 90,
      enabled: false,
      status: "disabled" as const,
    },
  ],
};

test("getWebProviders reads the provider list and fixed router order", async () => {
  const recorder = stubFetch(() => jsonResponse(sampleProviders));

  const response = await getWebProviders();
  assert.equal(recorder.urls[0], "/api/web/providers");
  assert.equal(recorder.calls[0]?.init?.method, undefined);
  assert.deepEqual(response, sampleProviders);
});

test("saveWebProvider PUTs the partial body and returns one provider DTO", async () => {
  const updated = {
    ...sampleTinyfish,
    key_hint: "…c0de",
    source: "database" as const,
  };
  const recorder = stubFetch((_url, init) => {
    assert.equal(init?.method, "PUT");
    assert.equal(
      init?.body,
      JSON.stringify({
        api_key: "tf-new-key",
        timeout_secs: 45,
        enabled: false,
      }),
    );
    return jsonResponse(updated);
  });

  const provider = await saveWebProvider("tinyfish", {
    api_key: "tf-new-key",
    timeout_secs: 45,
    enabled: false,
  });
  assert.equal(recorder.urls[0], "/api/web/providers/tinyfish");
  assert.deepEqual(provider, updated);
});

test("saveWebProvider percent-encodes the provider path segment", async () => {
  const recorder = stubFetch(() => jsonResponse(sampleTinyfish));

  await saveWebProvider("tiny/fish" as Parameters<typeof saveWebProvider>[0], {
    timeout_secs: 60,
  });
  assert.equal(recorder.urls[0], "/api/web/providers/tiny%2Ffish");
});

test("deleteWebProviderKey sends DELETE to /key and returns the fresh DTO", async () => {
  const cleared = {
    ...sampleTinyfish,
    configured: false,
    key_hint: null,
    source: "none" as const,
    status: "missing_key" as const,
  };
  const recorder = stubFetch(() => jsonResponse(cleared));

  const provider = await deleteWebProviderKey("tinyfish");
  assert.equal(recorder.urls[0], "/api/web/providers/tinyfish/key");
  assert.equal(recorder.calls[0]?.init?.method, "DELETE");
  assert.deepEqual(provider, cleared);
});

test("testWebProvider POSTs to /test and returns the latency result", async () => {
  const result = {
    ok: true as const,
    provider: "tinyfish" as const,
    message: "TinyFish responded",
    latency_ms: 123,
  };
  const recorder = stubFetch(() => jsonResponse(result));

  const tested = await testWebProvider("tinyfish");
  assert.equal(recorder.urls[0], "/api/web/providers/tinyfish/test");
  assert.equal(recorder.calls[0]?.init?.method, "POST");
  assert.deepEqual(tested, result);
});

test("reloadWebProviders POSTs to /reload and returns the full view", async () => {
  const recorder = stubFetch(() => jsonResponse(sampleProviders));

  const response = await reloadWebProviders();
  assert.equal(recorder.urls[0], "/api/web/providers/reload");
  assert.equal(recorder.calls[0]?.init?.method, "POST");
  assert.deepEqual(response, sampleProviders);
});

// ---------------------------------------------------------------------------
// Error paths
// ---------------------------------------------------------------------------

test("non-2xx with an { error } envelope throws ApiError with the status and message", async () => {
  stubFetch(() => jsonResponse({ error: "task not found: abc" }, 404));

  const error = await getTask("abc").then(
    () => null,
    (err: unknown) => err,
  );
  assert.ok(error instanceof ApiError);
  assert.ok(error instanceof Error);
  assert.equal(error.status, 404);
  assert.equal(error.message, "task not found: abc");
  assert.equal(error.isNotFound(), true);
  assert.equal(error.isValidation(), false);
  assert.equal(error.isConflict(), false);
});

test("400/409 statuses map to the isValidation/isConflict helpers", async () => {
  stubFetch(() =>
    jsonResponse({ error: "limit must be between 1 and 1000" }, 400),
  );

  const validation = await listTasks({ limit: 0 }).then(
    () => null,
    (err: unknown) => err,
  );
  assert.ok(validation instanceof ApiError);
  assert.equal(validation.status, 400);
  assert.equal(validation.isValidation(), true);

  stubFetch(() =>
    jsonResponse({ error: "approval a1 is not pending: granted" }, 409),
  );
  const conflict = await decideApproval("a1", "allow").then(
    () => null,
    (err: unknown) => err,
  );
  assert.ok(conflict instanceof ApiError);
  assert.equal(conflict.status, 409);
  assert.equal(conflict.isConflict(), true);
});

test("a non-JSON error body travels as the message", async () => {
  stubFetch(
    () =>
      new Response("upstream exploded", {
        status: 502,
        headers: { "content-type": "text/plain" },
      }),
  );

  const error = await listModels(1).then(
    () => null,
    (err: unknown) => err,
  );
  assert.ok(error instanceof ApiError);
  assert.equal(error.status, 502);
  assert.equal(error.message, "upstream exploded");
});

test("an { error }-less JSON body falls back to the raw body text", async () => {
  stubFetch(() => jsonResponse({ oops: 1 }, 500));

  const error = await getHealth().then(
    () => null,
    (err: unknown) => err,
  );
  assert.ok(error instanceof ApiError);
  assert.equal(error.status, 500);
  assert.equal(error.message, '{"oops":1}');
});

test("an empty error body falls back to the HTTP status line", async () => {
  stubFetch(() => new Response("", { status: 500 }));

  const error = await getHealth().then(
    () => null,
    (err: unknown) => err,
  );
  assert.ok(error instanceof ApiError);
  assert.equal(error.status, 500);
  assert.equal(error.message, "HTTP 500");
});

test("a rejecting fetch becomes ApiError(0)", async () => {
  globalThis.fetch = (async () => {
    throw new TypeError("boom: network down");
  }) as typeof fetch;

  const error = await getHealth().then(
    () => null,
    (err: unknown) => err,
  );
  assert.ok(error instanceof ApiError);
  assert.equal(error.status, 0);
  assert.match(error.message, /network error calling \/api\/health/);
  assert.match(error.message, /boom: network down/);
});

test("malformed 2xx JSON becomes ApiError(0)", async () => {
  stubFetch(
    () =>
      new Response("this is not json", {
        status: 200,
        headers: { "content-type": "application/json" },
      }),
  );

  const error = await getHealth().then(
    () => null,
    (err: unknown) => err,
  );
  assert.ok(error instanceof ApiError);
  assert.equal(error.status, 0);
  assert.match(error.message, /malformed JSON response from \/api\/health/);
});

// ---------------------------------------------------------------------------
// SSE parsers
// ---------------------------------------------------------------------------

test("parseEventId accepts unsigned decimals only", () => {
  assert.equal(parseEventId("42"), 42);
  assert.equal(parseEventId(" 7 "), 7);
  assert.equal(parseEventId("0"), 0);
  assert.equal(parseEventId(null), null);
  assert.equal(parseEventId(undefined), null);
  assert.equal(parseEventId(""), null);
  assert.equal(parseEventId("abc"), null);
  assert.equal(parseEventId("12x"), null);
  assert.equal(parseEventId("-1"), null);
  assert.equal(parseEventId("2.5"), null);
});

test("parseFrame reads a hello frame", () => {
  const frame = parseFrame('event: hello\ndata: {"task_id":"t1"}\n\n');
  assert.deepEqual(frame, { kind: "hello", taskId: "t1" });
  assert.equal(parseHelloData('{"task_id":"t1"}'), "t1");
  assert.equal(parseHelloData("not json"), null);
  assert.equal(parseHelloData('{"task_id":42}'), null);
});

test("parseFrame reads an activity frame with its id cursor", () => {
  const row = {
    id: 2,
    task_id: "t1",
    kind: "intent_claimed",
    message: "claimed",
    payload: {},
    created_at: "2026-10-30T00:00:02Z",
  };
  const frame = parseFrame(
    `event: activity\nid: 2\ndata: ${JSON.stringify(row)}\n\n`,
  );
  assert.deepEqual(frame, { kind: "activity", activity: row });

  // The `id:` field wins over the JSON body's id.
  const conflict = parseFrame(
    `event: activity\nid: 9\ndata: ${JSON.stringify(row)}\n\n`,
  );
  assert.ok(conflict?.kind === "activity");
  assert.equal(conflict.activity.id, 9);
});

test("parseFrame treats keep-alive comments and blanks as no-ops", () => {
  assert.deepEqual(parseFrame(": keep-alive\n\n"), { kind: "ignored" });
  assert.deepEqual(parseFrame("\n\n"), { kind: "ignored" });
  assert.deepEqual(parseFrame(""), { kind: "ignored" });
});

test("parseFrame tolerates malformed and unrecognized frames", () => {
  // Broken JSON in an activity frame: dropped (null), not thrown.
  assert.equal(parseFrame("event: activity\nid: 3\ndata: {broken\n\n"), null);
  // Valid JSON with an unusable activity shape: dropped.
  assert.equal(parseFrame('event: activity\ndata: {"nope":true}\n\n'), null);
  // Unknown named events with parsable data are ignored, not dropped.
  assert.deepEqual(parseFrame('event: future\ndata: {"x":1}\n\n'), {
    kind: "ignored",
  });
});

test("parseFrame joins multi-line data and survives CRLF", () => {
  const multiline = parseFrame(
    'event: hello\r\ndata: {"task_id":\r\ndata: "t2"}\r\n\r\n',
  );
  assert.deepEqual(multiline, { kind: "hello", taskId: "t2" });
});

test("parseActivityEvent falls back to the JSON id when no event id is present", () => {
  const activity = parseActivityEvent(
    JSON.stringify({ ...sampleActivity, id: 5 }),
    null,
  );
  assert.ok(activity);
  assert.equal(activity.id, 5);

  const missing = parseActivityEvent(
    JSON.stringify({
      task_id: "t1",
      kind: "k",
      message: "m",
      created_at: "x",
    }),
    null,
  );
  assert.equal(missing, null);
});

// ---------------------------------------------------------------------------
// connectTaskStream against a scripted EventSource
// ---------------------------------------------------------------------------

type Listener = (ev: { data?: string; lastEventId?: string }) => void;

class FakeEventSource {
  static instances: FakeEventSource[] = [];
  static reset(): void {
    FakeEventSource.instances = [];
  }
  static get last(): FakeEventSource | undefined {
    return FakeEventSource.instances[FakeEventSource.instances.length - 1];
  }

  static CLOSED = 2;
  static OPEN = 1;
  static CONNECTING = 0;

  url: string;
  readyState = FakeEventSource.CONNECTING;
  closed = false;
  onerror: (() => void) | null = null;
  private listeners = new Map<string, Listener[]>();

  constructor(url: string) {
    this.url = url;
    FakeEventSource.instances.push(this);
  }

  addEventListener(type: string, listener: Listener): void {
    const list = this.listeners.get(type) ?? [];
    list.push(listener);
    this.listeners.set(type, list);
  }

  close(): void {
    this.closed = true;
    this.readyState = FakeEventSource.CLOSED;
  }

  emit(type: string, data: string, lastEventId = ""): void {
    for (const listener of this.listeners.get(type) ?? []) {
      listener({ data, lastEventId });
    }
  }

  failAsClosed(): void {
    this.readyState = FakeEventSource.CLOSED;
    this.onerror?.();
  }
}

const delay = (ms: number) =>
  new Promise<void>((resolve) => setTimeout(resolve, ms));

function withFakeEventSource(): void {
  globalThis.EventSource = FakeEventSource as unknown as typeof EventSource;
}

function activityPayload(id: number): string {
  return JSON.stringify({ ...sampleActivity, id, kind: `step-${id}` });
}

test("connectTaskStream delivers hello and activity rows in order", async () => {
  withFakeEventSource();
  FakeEventSource.reset();
  const seenHello: string[] = [];
  const seen: number[] = [];
  const unsub = connectTaskStream(sampleTask.id, {
    onHello: (taskId) => seenHello.push(taskId),
    onActivity: (row) => seen.push(row.id),
  });

  const es = FakeEventSource.last;
  assert.ok(es);
  assert.equal(es.url, `/api/stream?task_id=${sampleTask.id}`);

  es.emit("hello", JSON.stringify({ task_id: sampleTask.id }));
  es.emit("activity", activityPayload(1), "1");
  es.emit("activity", activityPayload(2), "2");
  // Replays/stales are deduped by the cursor.
  es.emit("activity", activityPayload(2), "2");
  es.emit("activity", activityPayload(1), "1");

  assert.deepEqual(seenHello, [sampleTask.id]);
  assert.deepEqual(seen, [1, 2]);

  unsub();
  assert.equal(FakeEventSource.last?.closed, true);
});

test("connectTaskStream rebuilds the URL with after=<cursor> after a hard disconnect", async () => {
  withFakeEventSource();
  FakeEventSource.reset();
  const seen: number[] = [];
  const unsub = connectTaskStream(sampleTask.id, {
    after: 5,
    resumeDelayMs: 5,
    onActivity: (row) => seen.push(row.id),
  });

  const first = FakeEventSource.last;
  assert.ok(first);
  assert.equal(first.url, `/api/stream?task_id=${sampleTask.id}&after=5`);

  first.emit("activity", activityPayload(6), "6");
  first.failAsClosed(); // platform gave up -> explicit rebuild scheduled

  await delay(40);
  const second = FakeEventSource.last;
  assert.notEqual(second, first);
  assert.equal(second?.url, `/api/stream?task_id=${sampleTask.id}&after=6`);

  // A replay of id 6 from the rebuilt connection is deduped.
  second?.emit("activity", activityPayload(6), "6");
  second?.emit("activity", activityPayload(7), "7");
  assert.deepEqual(seen, [6, 7]);

  // After unsubscribe: connection closed and no further resumes.
  unsub();
  assert.equal(second?.closed, true);
  second?.failAsClosed();
  await delay(30);
  assert.equal(FakeEventSource.instances.length, 2);
});

test("connectTaskStream honors TASK_STREAM_RESUME_DELAY_MS as the default backoff", () => {
  assert.equal(TASK_STREAM_RESUME_DELAY_MS, 1000);
});

test("connectTaskStream keeps streaming past malformed rows and uses lastEventId", async () => {
  withFakeEventSource();
  FakeEventSource.reset();
  const seen: number[] = [];
  const unsub = connectTaskStream(sampleTask.id, {
    onActivity: (row) => seen.push(row.id),
  });

  const es = FakeEventSource.last;
  assert.ok(es);
  es.emit("activity", "definitely not json", "3"); // dropped
  es.emit("activity", activityPayload(4), "4");
  // No lastEventId given: the JSON body's id is used.
  es.emit("activity", activityPayload(5));
  assert.deepEqual(seen, [4, 5]);

  unsub();
});
