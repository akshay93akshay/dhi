import assert from "node:assert/strict";
import { test } from "node:test";
import { ApiError } from "./api.ts";
import {
  PIN_UNSET,
  PROJECT_FOLDER_UNSET,
  SWARM_WORKERS_OPTIONS,
  TOPOLOGY_OPTIONS,
  agentsMdDefaultOn,
  agentsMdStateLabel,
  agentsMdStateVariant,
  buildCreateTaskInput,
  defaultProjectFolderPath,
  describeApiError,
  parsePin,
  parseSwarmWorkers,
  parseTopology,
  provenanceDetailText,
  renderProvenance,
  serviceState,
  serviceStatusText,
  shortenId,
  swarmWorkersLabel,
  taskIdFromPathname,
  topologyLabel,
  validateGoal,
} from "./ui.ts";
import type { ProjectFolder } from "./types.ts";

// ---------------------------------------------------------------------------
// shortenId
// ---------------------------------------------------------------------------

test("shortenId keeps the first uuid hex group", () => {
  assert.equal(shortenId("00000000-0000-4000-8000-000000000001"), "00000000");
  assert.equal(shortenId("abcd"), "abcd");
  assert.equal(shortenId("12345678"), "12345678");
  assert.equal(shortenId("123456789"), "12345678");
  assert.equal(shortenId(""), "");
});

// ---------------------------------------------------------------------------
// taskIdFromPathname
// ---------------------------------------------------------------------------

const TASK_ID = "00000000-0000-4000-8000-000000000001";

test("taskIdFromPathname reads detail, work, and graph routes", () => {
  assert.equal(taskIdFromPathname(`/tasks/${TASK_ID}`), TASK_ID);
  assert.equal(taskIdFromPathname(`/tasks/${TASK_ID}/work`), TASK_ID);
  assert.equal(taskIdFromPathname(`/tasks/${TASK_ID}/graph`), TASK_ID);
});

test("taskIdFromPathname ignores trailing slashes", () => {
  assert.equal(taskIdFromPathname(`/tasks/${TASK_ID}/`), TASK_ID);
  assert.equal(taskIdFromPathname(`/tasks/${TASK_ID}/work/`), TASK_ID);
  assert.equal(taskIdFromPathname(`/tasks/${TASK_ID}/graph/`), TASK_ID);
});

test("taskIdFromPathname ignores the preview placeholder", () => {
  assert.equal(taskIdFromPathname("/tasks/preview"), null);
  assert.equal(taskIdFromPathname("/tasks/preview/"), null);
  assert.equal(taskIdFromPathname("/tasks/preview/work/"), null);
  assert.equal(taskIdFromPathname("/tasks/preview/graph"), null);
});

test("taskIdFromPathname returns null outside task routes", () => {
  assert.equal(taskIdFromPathname("/"), null);
  assert.equal(taskIdFromPathname("/tasks"), null);
  assert.equal(taskIdFromPathname(`/tasks/${TASK_ID}/other`), null);
  assert.equal(taskIdFromPathname("/api/health"), null);
  assert.equal(taskIdFromPathname(""), null);
});

test("taskIdFromPathname decodes the id segment", () => {
  assert.equal(taskIdFromPathname("/tasks/task%20one"), "task one");
  assert.equal(taskIdFromPathname("/tasks/%70review"), null);
});

// ---------------------------------------------------------------------------
// validateGoal / parsePin / buildCreateTaskInput
// ---------------------------------------------------------------------------

test("validateGoal rejects empty and whitespace-only goals", () => {
  assert.equal(validateGoal(""), "Goal is required.");
  assert.equal(validateGoal("   \n\t "), "Goal is required.");
  assert.equal(validateGoal("mate in two"), null);
  assert.equal(validateGoal(" padded "), null);
});

test("parsePin maps select values onto ids or null", () => {
  assert.equal(parsePin(PIN_UNSET), null);
  assert.equal(parsePin(""), null);
  assert.equal(parsePin("7"), 7);
  assert.equal(parsePin(" 3 "), 3);
  assert.equal(parsePin("0"), 0);
  assert.equal(parsePin("abc"), null);
  assert.equal(parsePin("-2"), null);
  assert.equal(parsePin("2.5"), null);
});

const folderWithAgents: ProjectFolder = {
  path: "/srv/projects/build",
  name: "Build",
  agents_md_present: true,
};

const folderWithoutAgents: ProjectFolder = {
  path: "/srv/projects/other",
  name: "Other",
  agents_md_present: false,
};

test("defaultProjectFolderPath preselects exactly one folder", () => {
  assert.equal(defaultProjectFolderPath([]), PROJECT_FOLDER_UNSET);
  assert.equal(
    defaultProjectFolderPath([folderWithAgents]),
    folderWithAgents.path,
  );
  // Several folders require an explicit choice, so legacy stays the default.
  assert.equal(
    defaultProjectFolderPath([folderWithAgents, folderWithoutAgents]),
    PROJECT_FOLDER_UNSET,
  );
});

test("agentsMdDefaultOn follows the folder's presence flag", () => {
  assert.equal(agentsMdDefaultOn(null), false);
  assert.equal(agentsMdDefaultOn(undefined), false);
  assert.equal(agentsMdDefaultOn(folderWithAgents), true);
  assert.equal(agentsMdDefaultOn(folderWithoutAgents), false);
});

test("agentsMdStateLabel and variant map every state", () => {
  assert.equal(agentsMdStateLabel("loaded"), "AGENTS.md loaded");
  assert.equal(agentsMdStateLabel("truncated"), "AGENTS.md truncated");
  assert.equal(agentsMdStateLabel("not_found"), "No AGENTS.md");
  assert.equal(agentsMdStateLabel("disabled"), "AGENTS.md off");
  // Legacy tasks / older servers omit the field entirely.
  assert.equal(agentsMdStateLabel(null), null);
  assert.equal(agentsMdStateLabel(undefined), null);

  assert.equal(agentsMdStateVariant("loaded"), "success");
  assert.equal(agentsMdStateVariant("truncated"), "warning");
  assert.equal(agentsMdStateVariant("not_found"), "muted");
  assert.equal(agentsMdStateVariant("disabled"), "muted");
  assert.equal(agentsMdStateVariant(undefined), "muted");
});

test("buildCreateTaskInput always sends topology and swarm_workers, defaulting to null", () => {
  const result = buildCreateTaskInput({
    goal: "  mate in two  ",
    plannerPin: "",
    workerPin: "",
  });
  assert.ok(result.ok);
  // Topology and the swarm width are explicit (`null` = planner decides);
  // absent pins stay absent.
  assert.deepEqual(result.input, {
    goal: "mate in two",
    topology: null,
    swarm_workers: null,
  });
  assert.deepEqual(Object.keys(result.input).sort(), [
    "goal",
    "swarm_workers",
    "topology",
  ]);

  const blank = buildCreateTaskInput({
    goal: "g",
    plannerPin: "",
    workerPin: "",
    topology: "",
  });
  assert.ok(blank.ok);
  assert.deepEqual(blank.input, {
    goal: "g",
    topology: null,
    swarm_workers: null,
  });
});

test("buildCreateTaskInput carries a selected topology enum", () => {
  for (const topology of ["single", "manager_worker", "swarm"] as const) {
    const result = buildCreateTaskInput({
      goal: "g",
      plannerPin: "",
      workerPin: "",
      topology,
    });
    assert.ok(result.ok);
    assert.deepEqual(result.input, {
      goal: "g",
      topology,
      swarm_workers: null,
    });
  }
  // Unknown/blank values degrade to "planner decides" instead of failing.
  const unknown = buildCreateTaskInput({
    goal: "g",
    plannerPin: "",
    workerPin: "",
    topology: "mystery",
  });
  assert.ok(unknown.ok);
  assert.equal(unknown.input.topology, null);
  assert.equal(unknown.input.swarm_workers, null);
});

test("buildCreateTaskInput sends swarm_workers only for a swarm topology", () => {
  for (const workers of ["2", "3", "4", "5", "6", "7", "8"]) {
    const result = buildCreateTaskInput({
      goal: "g",
      plannerPin: "",
      workerPin: "",
      topology: "swarm",
      swarmWorkers: workers,
    });
    assert.ok(result.ok);
    assert.equal(result.input.swarm_workers, Number(workers));
  }

  // Auto/blank stays null even for a swarm.
  const auto = buildCreateTaskInput({
    goal: "g",
    plannerPin: "",
    workerPin: "",
    topology: "swarm",
    swarmWorkers: "",
  });
  assert.ok(auto.ok);
  assert.equal(auto.input.swarm_workers, null);

  // A width selected before switching topology away is not sent.
  for (const topology of ["", "single", "manager_worker"]) {
    const result = buildCreateTaskInput({
      goal: "g",
      plannerPin: "",
      workerPin: "",
      topology,
      swarmWorkers: "6",
    });
    assert.ok(result.ok);
    assert.equal(result.input.swarm_workers, null);
  }
});

test("parseTopology and topologyLabel map the select vocabulary", () => {
  assert.equal(parseTopology(""), null);
  assert.equal(parseTopology(undefined), null);
  assert.equal(parseTopology(" SINGLE "), "single");
  assert.equal(parseTopology("manager_worker"), "manager_worker");
  assert.equal(parseTopology("swarm"), "swarm");
  assert.equal(parseTopology("other"), null);
  assert.deepEqual(
    TOPOLOGY_OPTIONS.map((option) => option.label),
    ["Let planner decide", "Single agent", "Multi agent", "Swarm"],
  );
  assert.equal(topologyLabel(null), null);
  assert.equal(topologyLabel("single"), "Single agent");
  assert.equal(topologyLabel("manager_worker"), "Multi agent");
  assert.equal(topologyLabel("swarm"), "Swarm");
});

test("parseSwarmWorkers accepts Auto and exactly 2 through 8", () => {
  assert.equal(parseSwarmWorkers(""), null);
  assert.equal(parseSwarmWorkers(undefined), null);
  assert.equal(parseSwarmWorkers("auto"), null);
  assert.equal(parseSwarmWorkers(" 2 "), 2);
  assert.equal(parseSwarmWorkers("4"), 4);
  assert.equal(parseSwarmWorkers("8"), 8);
  // Below/above the server range, fractions, negative, and text are Auto.
  for (const raw of ["0", "1", "9", "10", "-2", "2.5", "four", "4x"]) {
    assert.equal(parseSwarmWorkers(raw), null, raw);
  }
  assert.deepEqual(
    SWARM_WORKERS_OPTIONS.map((option) => option.label),
    [
      "Auto (planner decides)",
      "2",
      "3",
      "4",
      "5",
      "6",
      "7",
      "8",
    ],
  );
  assert.equal(swarmWorkersLabel(null), null);
  assert.equal(swarmWorkersLabel(undefined), null);
  assert.equal(swarmWorkersLabel(4), "4");
});

test("buildCreateTaskInput carries numeric pins into the canonical fields", () => {
  const result = buildCreateTaskInput({
    goal: "g",
    plannerPin: "7",
    workerPin: "9",
  });
  assert.ok(result.ok);
  assert.deepEqual(result.input, {
    goal: "g",
    topology: null,
    swarm_workers: null,
    planner_profile_id: 7,
    worker_profile_id: 9,
  });
});

test("buildCreateTaskInput sends project_folder and load_agents_md together", () => {
  const withAgents = buildCreateTaskInput({
    goal: "g",
    plannerPin: "",
    workerPin: "",
    projectFolder: " /srv/projects/build ",
    loadAgentsMd: true,
  });
  assert.ok(withAgents.ok);
  assert.deepEqual(withAgents.input, {
    goal: "g",
    topology: null,
    swarm_workers: null,
    project_folder: "/srv/projects/build",
    load_agents_md: true,
  });

  const withoutAgents = buildCreateTaskInput({
    goal: "g",
    plannerPin: "",
    workerPin: "",
    projectFolder: "/srv/projects/build",
    loadAgentsMd: false,
  });
  assert.ok(withoutAgents.ok);
  assert.equal(
    withoutAgents.input.project_folder,
    "/srv/projects/build",
  );
  // Explicit false is sent, not omitted, once a folder is active.
  assert.equal(withoutAgents.input.load_agents_md, false);
});

test("buildCreateTaskInput omits project fields in legacy workspace mode", () => {
  for (const projectFolder of [undefined, "", "   "]) {
    for (const loadAgentsMd of [undefined, true, false]) {
      const result = buildCreateTaskInput({
        goal: "g",
        plannerPin: "",
        workerPin: "",
        projectFolder,
        loadAgentsMd,
      });
      assert.ok(result.ok);
      assert.equal("project_folder" in result.input, false);
      assert.equal("load_agents_md" in result.input, false);
    }
  }
});

test("buildCreateTaskInput refuses an empty goal without submitting", () => {
  for (const goal of ["", "   "]) {
    const result = buildCreateTaskInput({
      goal,
      plannerPin: "1",
      workerPin: "",
    });
    assert.deepEqual(result, { ok: false, error: "Goal is required." });
  }
});

// ---------------------------------------------------------------------------
// renderProvenance
// ---------------------------------------------------------------------------

test("renderProvenance recognizes the origin convention { goal }", () => {
  const view = renderProvenance({ goal: "mate in two" });
  assert.deepEqual(view, {
    kind: "origin",
    lines: ["Goal: mate in two"],
  });
});

test("renderProvenance recognizes the worker convention with all four keys", () => {
  const view = renderProvenance({
    intent_id: "i1",
    detail: "checked Qh5 line",
    evidence: "stockfish eval +1.2",
    confidence: "observed",
  });
  assert.deepEqual(view, {
    kind: "worker",
    lines: [
      "Intent: i1",
      "Detail: checked Qh5 line",
      "Evidence: stockfish eval +1.2",
      "Confidence: observed",
    ],
  });
});

test("renderProvenance tolerates worker rows with an empty detail", () => {
  const view = renderProvenance({
    intent_id: "i2",
    detail: "",
    evidence: "board position after 1.Qh5",
    confidence: "inferred",
  });
  assert.ok(view.kind === "worker");
  assert.deepEqual(view.lines, [
    "Intent: i2",
    "Detail: (empty)",
    "Evidence: board position after 1.Qh5",
    "Confidence: inferred",
  ]);
});

test("renderProvenance treats partial worker shapes as worker, skipping gaps", () => {
  assert.deepEqual(renderProvenance({ intent_id: "i3" }), {
    kind: "worker",
    lines: ["Intent: i3"],
  });
  assert.deepEqual(
    renderProvenance({ evidence: "eval +0.4", confidence: "observed" }),
    {
      kind: "worker",
      lines: ["Evidence: eval +0.4", "Confidence: observed"],
    },
  );
});

test("renderProvenance prefers the origin convention when both shapes appear", () => {
  const view = renderProvenance({
    goal: "g",
    intent_id: "i4",
    evidence: "e",
  });
  assert.equal(view.kind, "origin");
  assert.deepEqual(view.lines, ["Goal: g"]);
});

test("renderProvenance falls back to raw JSON for unknown shapes", () => {
  const empty = renderProvenance({});
  assert.deepEqual(empty, { kind: "raw", lines: ["{}"] });

  const exotic = renderProvenance({
    trace: { nested: [1, 2] },
    notes: "kept verbatim",
    count: 2,
    flag: null,
  });
  assert.equal(exotic.kind, "raw");
  assert.equal(exotic.lines.length, 1);
  assert.equal(
    exotic.lines[0],
    JSON.stringify(
      {
        trace: { nested: [1, 2] },
        notes: "kept verbatim",
        count: 2,
        flag: null,
      },
      null,
      2,
    ),
  );
});

test("renderProvenance never crashes on hostile value types", () => {
  // Goal arriving as the wrong type, arrays as evidence.
  const oddTypes = renderProvenance({
    goal: 42,
  } as never);
  assert.deepEqual(oddTypes, { kind: "origin", lines: ["Goal: 42"] });

  const nested = renderProvenance({
    intent_id: "i5",
    evidence: { moves: ["e2e4", "e7e5"], depth: 3 },
  });
  assert.ok(nested.kind === "worker");
  assert.equal(
    nested.lines[1],
    `Evidence: ${JSON.stringify({ moves: ["e2e4", "e7e5"], depth: 3 })}`,
  );

  // A null confidence stringifies as JSON null, not the word undefined.
  const nulls = renderProvenance({ intent_id: "i6", confidence: null });
  assert.ok(nulls.kind === "worker");
  assert.equal(nulls.lines[1], "Confidence: null");
});

test("provenanceDetailText joins only Goal/Detail/Evidence content", () => {
  assert.equal(
    provenanceDetailText(renderProvenance({ goal: "mate in two" })),
    "Goal: mate in two",
  );
  assert.equal(
    provenanceDetailText(
      renderProvenance({
        intent_id: "i1",
        detail: "checked Qh5 line",
        evidence: "stockfish eval +1.2",
        confidence: "observed",
      }),
    ),
    "Detail: checked Qh5 line\nEvidence: stockfish eval +1.2",
  );
  // An id-only worker row has no copyable detail; raw JSON stays plain.
  assert.equal(provenanceDetailText(renderProvenance({ intent_id: "i2" })), null);
  assert.equal(
    provenanceDetailText(renderProvenance({ trace: { nested: true } })),
    null,
  );
});

// ---------------------------------------------------------------------------
// describeApiError
// ---------------------------------------------------------------------------

test("describeApiError words each failure class distinctly", () => {
  assert.match(
    describeApiError(new ApiError(0, "socket hung up")),
    /^Network error:/,
  );
  assert.match(
    describeApiError(new ApiError(400, "goal must not be empty")),
    /^Invalid request:/,
  );
  assert.match(
    describeApiError(new ApiError(404, "task not found: x")),
    /^Not found:/,
  );
  assert.equal(describeApiError(new ApiError(500, "boom")), "boom (HTTP 500)");
  assert.equal(describeApiError(new Error("plain")), "plain");
  assert.equal(describeApiError("just a string"), "just a string");
});

// ---------------------------------------------------------------------------
// serviceState
// ---------------------------------------------------------------------------

test("serviceState maps health and errors to the three brand states", () => {
  const healthy = { status: "ok", service: "dhi", version: "0.1.0" };
  const degraded = { status: "degraded", service: "dhi", version: "0.1.0" };
  assert.equal(serviceState(null, null), "loading");
  assert.equal(serviceState(healthy, null), "healthy");
  assert.equal(serviceState(degraded, null), "error");
  assert.equal(serviceState(null, "network down"), "error");
  assert.equal(serviceStatusText("healthy"), "Status: Healthy");
  assert.equal(serviceStatusText("error"), "Status: Error");
  assert.equal(serviceStatusText("loading"), "Status: Loading");
});
