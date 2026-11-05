import assert from "node:assert/strict";
import { afterEach, test } from "node:test";

import {
  addMcpServer,
  deleteMcpServer,
  getMcpServers,
  getToolCatalog,
  getToolControl,
  refreshMcpServer,
  reloadMcpServers,
  setToolEnabled,
  setToolVisibility,
  updateMcpServer,
} from "./api.ts";
import {
  BASE_ROLES,
  TOOL_SOURCES,
  addSpecialistRole,
  availableRoles,
  buildMcpServerInput,
  describeMcpReload,
  filterCatalogTools,
  formatToolRoles,
  isRecognizedRole,
  isVisibleToRole,
  mergeToolStates,
  parseKeyValueLines,
  sortRoles,
  splitStdioTarget,
  toggleRoleSelection,
  toolDisplayLabel,
  type McpServerFormValues,
  type ToolFilterState,
} from "./toolAdmin.ts";
import type { McpReloadReport, ToolCatalogEntry } from "./types.ts";

const originalFetch = globalThis.fetch;

afterEach(() => {
  globalThis.fetch = originalFetch;
});

type RecordedCall = { url: string; init?: RequestInit };

function stubFetch(
  respond: (url: string, init?: RequestInit) => Response,
): { calls: RecordedCall[] } {
  const recorder: { calls: RecordedCall[] } = { calls: [] };
  globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
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

function tool(overrides: Partial<ToolCatalogEntry> = {}): ToolCatalogEntry {
  return {
    key: "web_search",
    source: "builtin",
    description: "",
    inputSchema: { type: "object", properties: {} },
    default_roles: [],
    enabled: true,
    deferred: false,
    metadata: {},
    ...overrides,
  };
}

function mcpForm(
  overrides: Partial<McpServerFormValues> = {},
): McpServerFormValues {
  return {
    name: "github",
    transport: "stdio",
    command: "npx",
    args: "-y\n@modelcontextprotocol/server-github",
    env: "",
    url: "",
    headers: "",
    timeoutSecs: "",
    insecure: false,
    enabled: true,
    ...overrides,
  };
}

const sampleReload: McpReloadReport = {
  kept: ["alpha"],
  connected: ["beta"],
  disconnected: [],
  reconnected: [],
  failed: [{ name: "dead", error: "connect refused" }],
  runtime_attached: true,
  configured: 3,
};

// ---------------------------------------------------------------------------
// Role and catalog helpers
// ---------------------------------------------------------------------------

test("isRecognizedRole accepts the four base roles and well-formed specialists", () => {
  for (const role of BASE_ROLES) {
    assert.equal(isRecognizedRole(role), true);
  }
  assert.equal(isRecognizedRole("specialist:chess"), true);
  assert.equal(isRecognizedRole("specialist:reviewer-2"), true);
  for (const role of ["", "Planner", "specialist:", "specialist:   ", "intern"]) {
    assert.equal(isRecognizedRole(role), false, role);
  }
});

test("isVisibleToRole treats empty roles as every recognized role", () => {
  const open = tool();
  const workerOnly = tool({
    key: "worker_only",
    default_roles: ["worker"],
  });
  assert.equal(isVisibleToRole(open, "planner"), true);
  assert.equal(isVisibleToRole(open, "specialist:chess"), true);
  assert.equal(isVisibleToRole(workerOnly, "worker"), true);
  assert.equal(isVisibleToRole(workerOnly, "planner"), false);
  assert.equal(isVisibleToRole(open, "intern"), false);
});

test("sortRoles is stable, deduplicated, and base-first", () => {
  assert.deepEqual(
    sortRoles([
      "specialist:z",
      "worker",
      "planner",
      "specialist:a",
      "planner",
      "intern",
    ]),
    ["planner", "worker", "specialist:a", "specialist:z", "intern"],
  );
  assert.deepEqual(sortRoles([]), []);
});

test("availableRoles always includes the base roles plus catalog specialists", () => {
  const tools = [
    tool(),
    tool({ key: "chess", default_roles: ["specialist:chess"] }),
    tool({ key: "z", default_roles: ["specialist:zeta"] }),
  ];
  assert.deepEqual(availableRoles(tools), [
    "planner",
    "worker",
    "reporter",
    "digest",
    "specialist:chess",
    "specialist:zeta",
  ]);
});

test("formatToolRoles spells out the empty all-roles case", () => {
  assert.equal(formatToolRoles([]), "All roles");
  assert.equal(formatToolRoles(["worker", "planner"]), "worker, planner");
});

test("filterCatalogTools applies source, enabled, and role filters together", () => {
  const tools = [
    tool(),
    tool({
      key: "mcp__github__search",
      source: "mcp",
      default_roles: ["worker"],
    }),
    tool({
      key: "load_skill",
      source: "skills",
      enabled: false,
      default_roles: ["planner"],
    }),
    tool({
      key: "custom_x",
      source: "custom",
      default_roles: ["specialist:chess"],
    }),
  ];
  const filters: ToolFilterState = {
    source: "all",
    enabled: "all",
    role: "all",
  };

  assert.deepEqual(
    filterCatalogTools(tools, { ...filters, source: "mcp" }).map((row) => row.key),
    ["mcp__github__search"],
  );
  assert.deepEqual(
    filterCatalogTools(tools, { ...filters, enabled: "disabled" }).map(
      (row) => row.key,
    ),
    ["load_skill"],
  );
  assert.deepEqual(
    filterCatalogTools(tools, { ...filters, role: "worker" }).map(
      (row) => row.key,
    ),
    ["web_search", "mcp__github__search"],
  );
  assert.deepEqual(
    filterCatalogTools(tools, { ...filters, role: "specialist:chess" }).map(
      (row) => row.key,
    ),
    ["web_search", "custom_x"],
  );
  assert.deepEqual(
    filterCatalogTools(tools, { ...filters, role: "intern" }).map(
      (row) => row.key,
    ),
    [],
  );
  assert.deepEqual(
    filterCatalogTools(tools, {
      source: "builtin",
      enabled: "enabled",
      role: "planner",
    }).map((row) => row.key),
    ["web_search"],
  );
});

test("toolDisplayLabel maps every built-in tool to its short label", () => {
  const expected: Record<string, string> = {
    read: "Read — read a file",
    write: "Write — write a file",
    edit: "Edit — replace exact text in one file",
    search: "Search — search files",
    list: "List — list files",
    shell: "Shell — run a command",
    shell_open: "Shell open — open a terminal",
    shell_send: "Shell send — send terminal input",
    shell_read: "Shell read — read terminal output",
    shell_close: "Shell close — close a terminal",
    shell_list: "Shell list — list terminals",
    web_search: "Web search — search the web",
    web_read: "Web read — read a page",
    web_parse: "Web parse — parse a document",
    web_research: "Web research — answer with sources",
  };
  for (const [key, label] of Object.entries(expected)) {
    assert.equal(toolDisplayLabel(key, "builtin"), label, key);
  }
});

test("toolDisplayLabel humanizes the last key segment and tags its source", () => {
  assert.equal(
    toolDisplayLabel("mcp__github__search_issues", "mcp"),
    "Search issues · MCP tool",
  );
  assert.equal(
    toolDisplayLabel("mcp:filesystem/read_file", "mcp"),
    "Read file · MCP tool",
  );
  assert.equal(
    toolDisplayLabel("skills__pdf_tools", "skills"),
    "Pdf tools · skill",
  );
  assert.equal(
    toolDisplayLabel("custom.web.scrape-page", "custom"),
    "Scrape page · custom tool",
  );
  assert.equal(
    toolDisplayLabel("deploy_helper", "builtin"),
    "Deploy helper · built-in tool",
  );
  assert.equal(toolDisplayLabel("fetch_url", "web"), "Fetch url · web tool");
  assert.equal(
    toolDisplayLabel("run_tests", "coding"),
    "Run tests · coding tool",
  );
  assert.equal(toolDisplayLabel("tmux", "pty"), "Tmux · terminal tool");
});

test("toolDisplayLabel handles keys with no usable segment", () => {
  assert.equal(toolDisplayLabel("", "mcp"), "Tool · MCP tool");
  assert.equal(toolDisplayLabel("__", "skills"), "Tool · skill");
});

test("mergeToolStates overrides only matching tools", () => {
  const tools = [
    tool(),
    tool({ key: "other", enabled: true, default_roles: [] }),
  ];
  const merged = mergeToolStates(tools, [
    { key: "web_search", enabled: false, roles: ["worker"] },
    { key: "missing", enabled: true, roles: [] },
  ]);
  assert.deepEqual(merged[0], {
    ...tools[0],
    enabled: false,
    default_roles: ["worker"],
  });
  assert.deepEqual(merged[1], tools[1]);
});

test("toggleRoleSelection adds and removes without duplicates", () => {
  assert.deepEqual(toggleRoleSelection(["worker"], "planner", true), [
    "planner",
    "worker",
  ]);
  assert.deepEqual(toggleRoleSelection(["planner", "worker"], "planner", false), [
    "worker",
  ]);
  assert.deepEqual(toggleRoleSelection(["planner"], "planner", true), [
    "planner",
  ]);
});

test("addSpecialistRole validates, prefixes, and deduplicates", () => {
  assert.deepEqual(addSpecialistRole(["worker"], "researcher"), {
    ok: true,
    roles: ["worker", "specialist:researcher"],
  });
  assert.deepEqual(addSpecialistRole([], "specialist:chess"), {
    ok: true,
    roles: ["specialist:chess"],
  });
  assert.deepEqual(addSpecialistRole(["specialist:chess"], "chess"), {
    ok: true,
    roles: ["specialist:chess"],
  });
  const empty = addSpecialistRole([], "   ");
  assert.equal(empty.ok, false);
  const control = addSpecialistRole([], "bad\nname");
  assert.equal(control.ok, false);
});

// ---------------------------------------------------------------------------
// Key/value and MCP form helpers
// ---------------------------------------------------------------------------

test("parseKeyValueLines accepts blanks, trims, and preserves equals in values", () => {
  const parsed = parseKeyValueLines(
    "Authorization=Bearer a=b\n\n  X-Trace = 1  \r\n",
    "Headers",
  );
  assert.deepEqual(parsed, {
    ok: true,
    values: { Authorization: "Bearer a=b", "X-Trace": "1" },
  });
});

test("parseKeyValueLines rejects malformed and duplicate lines", () => {
  assert.deepEqual(parseKeyValueLines("NoEquals", "Headers"), {
    ok: false,
    error: "Headers line 1: expected KEY=VALUE.",
  });
  assert.deepEqual(parseKeyValueLines("=value", "Environment"), {
    ok: false,
    error: "Environment line 1: expected KEY=VALUE.",
  });
  assert.deepEqual(parseKeyValueLines("A=1\nA=2", "Headers"), {
    ok: false,
    error: 'Headers: duplicate key "A".',
  });
});

test("splitStdioTarget is whitespace based and degrades safely", () => {
  assert.deepEqual(splitStdioTarget("npx -y server"), {
    command: "npx",
    args: ["-y", "server"],
  });
  assert.deepEqual(splitStdioTarget("  "), { command: "", args: [] });
});

test("buildMcpServerInput builds a stdio add body", () => {
  const result = buildMcpServerInput(
    mcpForm({
      args: "-y\n@modelcontextprotocol/server-github",
      env: "GITHUB_TOKEN=abc=def",
      timeoutSecs: "30",
    }),
    "add",
  );
  assert.deepEqual(result, {
    ok: true,
    input: {
      name: "github",
      transport: "stdio",
      command: "npx",
      args: ["-y", "@modelcontextprotocol/server-github"],
      env: { GITHUB_TOKEN: "abc=def" },
      timeout_secs: 30,
      enabled: true,
    },
  });
});

test("buildMcpServerInput builds an HTTP body and omits the name on edit", () => {
  const result = buildMcpServerInput(
    mcpForm({
      name: "ignored-on-edit",
      transport: "http",
      command: "",
      args: "",
      url: "https://mcp.example/mcp",
      headers: "Authorization=Bearer secret\nX-Trace=1",
      timeoutSecs: "",
      insecure: true,
      enabled: false,
    }),
    "edit",
  );
  assert.deepEqual(result, {
    ok: true,
    input: {
      transport: "http",
      url: "https://mcp.example/mcp",
      headers: { Authorization: "Bearer secret", "X-Trace": "1" },
      insecure: true,
      enabled: false,
    },
  });
});

test("buildMcpServerInput supports SSE and rejects invalid forms", () => {
  const sse = buildMcpServerInput(
    mcpForm({
      transport: "sse",
      command: "",
      args: "",
      url: "https://mcp.example/sse",
    }),
    "add",
  );
  assert.equal(sse.ok, true);

  const cases: Array<[Partial<McpServerFormValues>, string]> = [
    [{ name: "   " }, "Server name is required."],
    [{ command: "" }, "Command is required for a stdio server."],
    [
      { transport: "http", command: "", args: "", url: "ftp://x" },
      "URL must start with http:// or https://.",
    ],
    [
      { transport: "http", command: "", args: "", url: "" },
      "URL is required for an HTTP or SSE server.",
    ],
    [
      { timeoutSecs: "0" },
      "Timeout must be a positive whole number of seconds, or blank for the 60 s default.",
    ],
    [
      { transport: "http", command: "", args: "", url: "https://x", headers: "bad" },
      "Headers line 1: expected KEY=VALUE.",
    ],
  ];
  for (const [overrides, expected] of cases) {
    const result = buildMcpServerInput(mcpForm(overrides), "add");
    assert.equal(result.ok, false);
    if (!result.ok) {
      assert.equal(result.error, expected);
    }
  }
});

test("describeMcpReload summarizes every report bucket", () => {
  assert.equal(
    describeMcpReload(sampleReload),
    "3 configured · 1 connected · 1 kept · 1 failed",
  );
  assert.equal(
    describeMcpReload({
      kept: [],
      connected: [],
      disconnected: ["old"],
      reconnected: ["new"],
      failed: [],
      runtime_attached: false,
      configured: 2,
    }),
    "2 configured · 1 reconnected · 1 disconnected · runtime not attached",
  );
});

// ---------------------------------------------------------------------------
// API client calls
// ---------------------------------------------------------------------------

test("getToolCatalog reads the catalog envelope", async () => {
  const body = {
    tools: [tool()],
    errors: [{ source: "mcp", error: "boom" }],
    control_error: null,
  };
  const recorder = stubFetch(() => jsonResponse(body));
  assert.deepEqual(await getToolCatalog(), body);
  assert.equal(recorder.calls[0].url, "/api/tools/catalog");
  assert.equal(recorder.calls[0].init?.method, undefined);
});

test("getToolControl unwraps the tools array", async () => {
  const recorder = stubFetch(() =>
    jsonResponse({ tools: [{ key: "web_search", enabled: true, roles: [] }] }),
  );
  assert.deepEqual(await getToolControl(), [
    { key: "web_search", enabled: true, roles: [] },
  ]);
  assert.equal(recorder.calls[0].url, "/api/tools/control");
});

test("setToolEnabled and setToolVisibility post tagged actions", async () => {
  const recorder = stubFetch((_url, init) => {
    assert.equal(init?.method, "POST");
    return jsonResponse({
      tools: [{ key: "web_search", enabled: false, roles: ["worker"] }],
    });
  });

  const states = await setToolEnabled("web_search", false);
  assert.deepEqual(states, [
    { key: "web_search", enabled: false, roles: ["worker"] },
  ]);
  assert.deepEqual(JSON.parse(String(recorder.calls[0].init?.body)), {
    action: "set_enabled",
    key: "web_search",
    enabled: false,
  });

  await setToolVisibility("web_search", ["worker", "planner"]);
  assert.equal(recorder.calls[1].url, "/api/tools/control");
  assert.deepEqual(JSON.parse(String(recorder.calls[1].init?.body)), {
    action: "set_visibility",
    key: "web_search",
    roles: ["worker", "planner"],
  });
});

test("getMcpServers reads status and masked config without leaking values", async () => {
  const body = {
    servers: [
      {
        name: "beta",
        transport: "http",
        enabled: true,
        connected: false,
        tools: [],
        last_error: "connect refused",
        call_timeout_secs: 60,
      },
    ],
    config: [
      {
        name: "beta",
        transport: "http",
        enabled: true,
        target: "https://example.test/mcp",
        timeout_secs: 60,
        headers: { Authorization: "***" },
      },
    ],
  };
  const recorder = stubFetch(() => jsonResponse(body));
  assert.deepEqual(await getMcpServers(), body);
  assert.equal(recorder.calls[0].url, "/api/mcp/servers");
});

test("addMcpServer posts the entry body", async () => {
  const recorder = stubFetch(() =>
    jsonResponse({
      server: {
        name: "github",
        transport: "stdio",
        enabled: true,
        target: "npx -y server",
        timeout_secs: 60,
      },
      reload: sampleReload,
    }),
  );
  const response = await addMcpServer({
    name: "github",
    transport: "stdio",
    command: "npx",
    args: ["-y", "server"],
    enabled: true,
  });
  assert.equal(response.server.name, "github");
  assert.equal(recorder.calls[0].url, "/api/mcp/servers");
  assert.equal(recorder.calls[0].init?.method, "POST");
  assert.deepEqual(JSON.parse(String(recorder.calls[0].init?.body)), {
    name: "github",
    transport: "stdio",
    command: "npx",
    args: ["-y", "server"],
    enabled: true,
  });
});

test("updateMcpServer PATCHes an encoded name", async () => {
  const recorder = stubFetch(() =>
    jsonResponse({
      server: {
        name: "my server",
        transport: "stdio",
        enabled: true,
        target: "npx",
        timeout_secs: 60,
      },
      reload: sampleReload,
    }),
  );
  await updateMcpServer("my server", { transport: "stdio", command: "npx" });
  assert.equal(recorder.calls[0].url, "/api/mcp/servers/my%20server");
  assert.equal(recorder.calls[0].init?.method, "PATCH");
});

test("deleteMcpServer, refreshMcpServer, and reloadMcpServers use the right verbs", async () => {
  const recorder = stubFetch((url) => {
    if (url.endsWith("/refresh")) {
      return jsonResponse({ server: "beta", tools: [] });
    }
    if (url.endsWith("/reload")) {
      return jsonResponse({ reload: sampleReload });
    }
    return jsonResponse({ deleted: "beta", reload: sampleReload });
  });

  const deleted = await deleteMcpServer("beta");
  assert.equal(deleted.deleted, "beta");
  assert.equal(recorder.calls[0].url, "/api/mcp/servers/beta");
  assert.equal(recorder.calls[0].init?.method, "DELETE");

  const refreshed = await refreshMcpServer("beta");
  assert.deepEqual(refreshed, { server: "beta", tools: [] });
  assert.equal(recorder.calls[1].url, "/api/mcp/servers/beta/refresh");
  assert.equal(recorder.calls[1].init?.method, "POST");

  const reload = await reloadMcpServers();
  assert.deepEqual(reload, sampleReload);
  assert.equal(recorder.calls[2].url, "/api/mcp/reload");
  assert.equal(recorder.calls[2].init?.method, "POST");
});

test("the source constant covers every catalog source", () => {
  assert.deepEqual(TOOL_SOURCES, [
    "builtin",
    "web",
    "coding",
    "pty",
    "mcp",
    "skills",
    "custom",
  ]);
});
