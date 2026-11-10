import assert from "node:assert/strict";
import { afterEach, test } from "node:test";

import {
  ProjectSnapshotError,
  WEB_PROJECT_MAX_FILE_BYTES,
  WEB_PROJECT_MAX_TOTAL_BYTES,
  clearProjectHandle,
  decodeBase64,
  decodeOutputContent,
  exceedsFileLimit,
  exceedsTotalLimit,
  formatBytes,
  loadProjectFolderHandle,
  loadProjectHandle,
  normalizeOutputPath,
  outputRelativePath,
  projectHandleKey,
  readAgentsMd,
  saveProjectHandle,
  shouldSkipDirectory,
  snapshotLimitError,
  syncTaskOutputs,
  uploadProjectFiles,
  walkProjectDirectory,
  writeAgentsMd,
  type ProjectDirectoryHandle,
  type ProjectFileHandle,
  type ProjectSnapshot,
} from "./webProject.ts";
import {
  buildCreateTaskInput,
  defaultProjectSelection,
  webProjectAgentsMdLabel,
  webProjectIdFromSelection,
  webProjectSelectValue,
} from "./ui.ts";
import {
  createWebProject,
  getTaskOutputs,
  getWebProject,
  listWebProjects,
  uploadWebProjectFile,
} from "./api.ts";
import type { ProjectFolder, TaskOutputFile, WebProject } from "./types.ts";

// ---------------------------------------------------------------------------
// Fake folder handles
// ---------------------------------------------------------------------------

function fakeFile(name: string, size: number): ProjectFileHandle {
  return {
    kind: "file",
    name,
    getFile: async () => ({ size }) as File,
  };
}

function fakeDir(
  name: string,
  entries: Array<ProjectFileHandle | ProjectDirectoryHandle>,
): ProjectDirectoryHandle {
  return {
    kind: "directory",
    name,
    async *values() {
      for (const entry of entries) {
        yield entry;
      }
    },
    getDirectoryHandle: async () => {
      throw new Error("not implemented");
    },
    getFileHandle: async () => {
      throw new Error("not implemented");
    },
  };
}

function memoryRoot(): {
  root: ProjectDirectoryHandle;
  written: Map<string, Uint8Array>;
} {
  const written = new Map<string, Uint8Array>();
  const makeDir = (prefix: string): ProjectDirectoryHandle => ({
    kind: "directory",
    name: prefix,
    async *values() {
      // No entries; tests only write through this handle.
    },
    getDirectoryHandle: async (name) =>
      makeDir(prefix === "" ? name : `${prefix}/${name}`),
    getFileHandle: async (name) => {
      let pending: Uint8Array | null = null;
      return {
        kind: "file",
        name,
        getFile: async () => new File([], name),
        createWritable: async () => ({
          write: async (bytes) => {
            pending = bytes;
          },
          close: async () => {
            written.set(
              prefix === "" ? name : `${prefix}/${name}`,
              pending ?? new Uint8Array(0),
            );
          },
        }),
      };
    },
  });
  return { root: makeDir(""), written };
}

// ---------------------------------------------------------------------------
// Per-project handle storage (in-memory IndexedDB double)
// ---------------------------------------------------------------------------

type FakeRequest<T> = {
  result: T | undefined;
  error: Error | null;
  onsuccess: (() => void) | null;
  onerror: (() => void) | null;
  onupgradeneeded: (() => void) | null;
};

function fakeRequest<T>(): FakeRequest<T> {
  return {
    result: undefined,
    error: null,
    onsuccess: null,
    onerror: null,
    onupgradeneeded: null,
  };
}

class FakeObjectStore {
  readonly data = new Map<string, unknown>();
  private readonly database: FakeDatabase;

  constructor(database: FakeDatabase) {
    this.database = database;
  }

  put(value: unknown, key: string): FakeRequest<unknown> {
    this.data.set(String(key), value);
    return this.finish(fakeRequest<unknown>(), key);
  }

  get(key: string): FakeRequest<unknown> {
    return this.finish(fakeRequest<unknown>(), this.data.get(String(key)));
  }

  delete(key: string): FakeRequest<undefined> {
    this.data.delete(String(key));
    return this.finish(fakeRequest<undefined>(), undefined);
  }

  private finish<T>(request: FakeRequest<T>, result: T): FakeRequest<T> {
    queueMicrotask(() => {
      request.result = result;
      request.onsuccess?.();
      this.database.currentTransaction?.complete();
    });
    return request;
  }
}

class FakeTransaction {
  oncomplete: (() => void) | null = null;
  onerror: (() => void) | null = null;
  onabort: (() => void) | null = null;
  error: Error | null = null;
  private readonly store: FakeObjectStore;

  constructor(store: FakeObjectStore) {
    this.store = store;
  }

  objectStore(): FakeObjectStore {
    return this.store;
  }

  complete(): void {
    this.oncomplete?.();
  }
}

class FakeDatabase {
  readonly stores = new Map<string, FakeObjectStore>();
  readonly objectStoreNames: { contains(name: string): boolean };
  readonly version: number;
  currentTransaction: FakeTransaction | null = null;

  constructor(version: number) {
    this.version = version;
    this.objectStoreNames = {
      contains: (name: string) => this.stores.has(name),
    };
  }

  createObjectStore(name: string): FakeObjectStore {
    const store = new FakeObjectStore(this);
    this.stores.set(name, store);
    return store;
  }

  transaction(name: string): FakeTransaction {
    const store = this.stores.get(name);
    if (store === undefined) {
      throw new Error(`Missing object store ${name}`);
    }
    const transaction = new FakeTransaction(store);
    this.currentTransaction = transaction;
    return transaction;
  }

  close(): void {
    // Nothing to release in the double.
  }
}

class FakeIndexedDb {
  private readonly databases = new Map<string, FakeDatabase>();

  open(name: string, version = 1): FakeRequest<FakeDatabase> {
    const request = fakeRequest<FakeDatabase>();
    queueMicrotask(() => {
      let database = this.databases.get(name);
      if (database === undefined || database.version < version) {
        database = new FakeDatabase(version);
        this.databases.set(name, database);
        request.result = database;
        request.onupgradeneeded?.();
      } else {
        request.result = database;
      }
      request.onsuccess?.();
    });
    return request;
  }
}

function installFakeIndexedDb(): () => void {
  const target = globalThis as unknown as { indexedDB?: IDBFactory };
  const original = target.indexedDB;
  target.indexedDB = new FakeIndexedDb() as unknown as IDBFactory;
  return () => {
    if (original === undefined) {
      delete target.indexedDB;
    } else {
      target.indexedDB = original;
    }
  };
}

function storedHandle(name: string): ProjectDirectoryHandle {
  return {
    kind: "directory",
    name,
    async *values() {},
    getDirectoryHandle: async () => {
      throw new Error("not used");
    },
    getFileHandle: async () => {
      throw new Error("not used");
    },
  };
}

test("folder handles are stored separately for each web project", async () => {
  const restore = installFakeIndexedDb();
  try {
    const first = storedHandle("first-project");
    const second = storedHandle("second-project");

    assert.equal(projectHandleKey("project-1"), "project-folder:project-1");
    assert.notEqual(
      projectHandleKey("project-1"),
      projectHandleKey("project-2"),
    );

    await saveProjectHandle("project-1", first);
    await saveProjectHandle("project-2", second);
    assert.equal(await loadProjectHandle("project-1"), first);
    assert.equal(await loadProjectHandle("project-2"), second);

    await clearProjectHandle("project-1");
    assert.equal(await loadProjectHandle("project-1"), null);
    assert.equal(await loadProjectHandle("project-2"), second);
  } finally {
    restore();
  }
});

test("sync handle lookup never falls back to another project's folder", async () => {
  const restore = installFakeIndexedDb();
  try {
    const first = storedHandle("first-project");
    await saveProjectHandle("project-1", first);

    const own = await loadProjectFolderHandle("project-1");
    assert.equal(own.handle, first);
    assert.equal(own.needsFolderChoice, false);

    // The sync card asks for project-2's handle only; project-1's handle
    // must never be returned as a fallback.
    const other = await loadProjectFolderHandle("project-2");
    assert.equal(other.handle, null);
    assert.equal(other.needsFolderChoice, true);
  } finally {
    restore();
  }
});

test("a missing project handle asks the user to choose its folder", async () => {
  const restore = installFakeIndexedDb();
  try {
    const missing = await loadProjectFolderHandle("project-without-handle");
    assert.equal(missing.handle, null);
    assert.equal(missing.needsFolderChoice, true);
  } finally {
    restore();
  }
});

// ---------------------------------------------------------------------------
// Walk rules and limits
// ---------------------------------------------------------------------------

test("shouldSkipDirectory matches every excluded build directory", () => {
  for (const name of [
    ".git",
    ".dhi",
    "node_modules",
    "target",
    ".next",
    "dist",
    "build",
    "out",
    ".cache",
    "NODE_MODULES",
    " .Git ",
    " .DHI ",
  ]) {
    assert.equal(shouldSkipDirectory(name), true, name);
  }
  for (const name of ["src", "docs", "AGENTS.md", ".github", "builder"]) {
    assert.equal(shouldSkipDirectory(name), false, name);
  }
});

test("walkProjectDirectory skips excluded folders and walks the rest", async () => {
  const root = fakeDir("build", [
    fakeDir(".git", [fakeFile("HEAD", 10)]),
    fakeDir(".dhi", [
      fakeDir("old-task", [fakeFile("result.txt", 10)]),
    ]),
    fakeDir("node_modules", [fakeDir("pkg", [fakeFile("index.js", 10)])]),
    fakeDir("target", [fakeFile("debug.bin", 10)]),
    fakeDir(".next", [fakeFile("cache", 10)]),
    fakeDir("dist", [fakeFile("app.js", 10)]),
    fakeDir("build", [fakeFile("bundle.js", 10)]),
    fakeDir("out", [fakeFile("export.html", 10)]),
    fakeDir(".cache", [fakeFile("x", 10)]),
    fakeFile("AGENTS.md", 12),
    fakeDir("src", [
      fakeFile("a.ts", 100),
      fakeDir("nested", [fakeFile("c.md", 7)]),
      fakeDir(".dhi", [fakeFile("nested-result.txt", 10)]),
    ]),
  ]);

  const snapshot = await walkProjectDirectory(root);
  assert.deepEqual(
    snapshot.files.map((entry) => entry.path).sort(),
    ["AGENTS.md", "src/a.ts", "src/nested/c.md"],
  );
  assert.deepEqual(snapshot.skippedDirectories.sort(), [
    ".cache",
    ".dhi",
    ".git",
    ".next",
    "build",
    "dist",
    "node_modules",
    "out",
    "src/.dhi",
    "target",
  ]);
  assert.equal(snapshot.totalBytes, 119);
  assert.deepEqual(snapshot.droppedFiles, []);
});

test("walkProjectDirectory drops files over the per-file limit", async () => {
  const root = fakeDir("build", [
    fakeFile("small.txt", 10),
    fakeFile("big.bin", WEB_PROJECT_MAX_FILE_BYTES + 1),
  ]);
  const snapshot = await walkProjectDirectory(root);

  assert.deepEqual(snapshot.files.map((entry) => entry.path), ["small.txt"]);
  assert.equal(snapshot.totalBytes, 10);
  assert.deepEqual(snapshot.droppedFiles, [
    {
      path: "big.bin",
      size: WEB_PROJECT_MAX_FILE_BYTES + 1,
      reason: "file_limit",
    },
  ]);
});

test("per-file and total limits match the documented thresholds", () => {
  assert.equal(exceedsFileLimit(WEB_PROJECT_MAX_FILE_BYTES), false);
  assert.equal(exceedsFileLimit(WEB_PROJECT_MAX_FILE_BYTES + 1), true);
  assert.equal(exceedsTotalLimit(WEB_PROJECT_MAX_TOTAL_BYTES), false);
  assert.equal(exceedsTotalLimit(WEB_PROJECT_MAX_TOTAL_BYTES + 1), true);
  assert.equal(formatBytes(WEB_PROJECT_MAX_FILE_BYTES), "8 MB");
  assert.equal(formatBytes(WEB_PROJECT_MAX_TOTAL_BYTES), "256 MB");
});

test("snapshotLimitError explains over-limit snapshots", () => {
  const snapshot: ProjectSnapshot = {
    files: [
      {
        path: "a.txt",
        file: new File([], "a.txt"),
        size: WEB_PROJECT_MAX_FILE_BYTES + 1,
      },
    ],
    totalBytes: WEB_PROJECT_MAX_FILE_BYTES + 1,
    skippedDirectories: [],
    droppedFiles: [],
  };
  assert.match(snapshotLimitError(snapshot) ?? "", /per-file limit/);

  const total: ProjectSnapshot = {
    files: [
      {
        path: "a.txt",
        file: new File([], "a.txt"),
        size: 10,
      },
    ],
    totalBytes: WEB_PROJECT_MAX_TOTAL_BYTES + 1,
    skippedDirectories: [],
    droppedFiles: [],
  };
  assert.match(snapshotLimitError(total) ?? "", /total limit/);

  assert.equal(
    snapshotLimitError({ files: [], totalBytes: 0 }),
    null,
  );
});

test("uploadProjectFiles refuses an over-limit snapshot before any request", async () => {
  const originalFetch = globalThis.fetch;
  let calls = 0;
  globalThis.fetch = (async () => {
    calls += 1;
    return new Response("{}");
  }) as typeof fetch;
  try {
    const snapshot: ProjectSnapshot = {
      files: [],
      totalBytes: WEB_PROJECT_MAX_TOTAL_BYTES + 1,
      skippedDirectories: [],
      droppedFiles: [],
    };
    await assert.rejects(
      () => uploadProjectFiles("project-1", snapshot),
      (error: unknown) => error instanceof ProjectSnapshotError,
    );
    assert.equal(calls, 0);
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test("uploadProjectFiles stops with AbortError when the signal is cancelled", async () => {
  const snapshot: ProjectSnapshot = {
    files: [{ path: "a.txt", file: new File(["a"], "a.txt"), size: 1 }],
    totalBytes: 1,
    skippedDirectories: [],
    droppedFiles: [],
  };
  const controller = new AbortController();
  controller.abort();
  await assert.rejects(
    () => uploadProjectFiles("project-1", snapshot, {
      signal: controller.signal,
    }),
    (error: unknown) =>
      error instanceof Error && error.name === "AbortError",
  );
});

test("uploadProjectFiles reports progress after each stored file", async () => {
  const originalFetch = globalThis.fetch;
  const calls: string[] = [];
  globalThis.fetch = (async (input: RequestInfo | URL) => {
    calls.push(String(input));
    return new Response(JSON.stringify({ path: "x", size: 1 }), {
      status: 200,
      headers: { "content-type": "application/json" },
    });
  }) as typeof fetch;
  try {
    const snapshot: ProjectSnapshot = {
      files: [
        { path: "a.txt", file: new File(["a"], "a.txt"), size: 1 },
        { path: "b.txt", file: new File(["bb"], "b.txt"), size: 2 },
      ],
      totalBytes: 3,
      skippedDirectories: [],
      droppedFiles: [],
    };
    const progress: number[] = [];
    const result = await uploadProjectFiles("p1", snapshot, {
      onProgress: (value) => progress.push(value.uploadedFiles),
    });
    assert.deepEqual(result, { uploaded: 2, totalBytes: 3 });
    assert.deepEqual(progress, [0, 0, 1, 1, 2]);
    assert.equal(calls.length, 2);
  } finally {
    globalThis.fetch = originalFetch;
  }
});

// ---------------------------------------------------------------------------
// Create-form selector and payload
// ---------------------------------------------------------------------------

const folder: ProjectFolder = {
  path: "/srv/projects/build",
  name: "Build",
  agents_md_present: true,
};

const webProject: WebProject = {
  id: "00000000-0000-4000-8000-000000000099",
  name: "Browser build",
  agents_md_present: true,
};

test("defaultProjectSelection preselects exactly one project option", () => {
  assert.equal(defaultProjectSelection([], []), "");
  assert.equal(defaultProjectSelection([folder], []), folder.path);
  assert.equal(
    defaultProjectSelection([], [webProject]),
    webProjectSelectValue(webProject.id),
  );
  // Several options require an explicit choice.
  assert.equal(defaultProjectSelection([folder], [webProject]), "");
  assert.equal(
    defaultProjectSelection([folder, { ...folder, path: "/other" }], []),
    "",
  );
  assert.equal(
    defaultProjectSelection([], [webProject, { ...webProject, id: "p2" }]),
    "",
  );
});

test("web project selector values round-trip", () => {
  const value = webProjectSelectValue(webProject.id);
  assert.equal(value, `web:${webProject.id}`);
  assert.equal(webProjectIdFromSelection(value), webProject.id);
  assert.equal(webProjectIdFromSelection(` ${value} `), webProject.id);
  assert.equal(webProjectIdFromSelection(folder.path), null);
  assert.equal(webProjectIdFromSelection("web:"), null);
  assert.equal(webProjectIdFromSelection(""), null);
  assert.equal(webProjectIdFromSelection(null), null);
});

test("buildCreateTaskInput sends web_project_id and omits project_folder", () => {
  const enabled = buildCreateTaskInput({
    goal: "g",
    plannerPin: "",
    workerPin: "",
    webProjectId: webProject.id,
    projectFolder: "/ignored/folder",
    loadAgentsMd: true,
  });
  assert.ok(enabled.ok);
  assert.deepEqual(enabled.input, {
    goal: "g",
    topology: null,
    swarm_workers: null,
    web_project_id: webProject.id,
    load_agents_md: true,
  });
  assert.equal("project_folder" in enabled.input, false);

  const disabled = buildCreateTaskInput({
    goal: "g",
    plannerPin: "",
    workerPin: "",
    webProjectId: webProject.id,
    loadAgentsMd: false,
  });
  assert.ok(disabled.ok);
  assert.equal(disabled.input.web_project_id, webProject.id);
  // Explicit false is sent, matching the Docker-folder behavior.
  assert.equal(disabled.input.load_agents_md, false);
});

test("webProjectAgentsMdLabel names both presence states", () => {
  assert.equal(webProjectAgentsMdLabel(true), "AGENTS.md present");
  assert.equal(webProjectAgentsMdLabel(false), "No AGENTS.md");
  assert.equal(webProjectAgentsMdLabel(null), "No AGENTS.md");
  assert.equal(webProjectAgentsMdLabel(undefined), "No AGENTS.md");
});

// ---------------------------------------------------------------------------
// AGENTS.md read/write
// ---------------------------------------------------------------------------

test("readAgentsMd reads the file and treats NotFoundError as absent", async () => {
  const present: ProjectDirectoryHandle = {
    kind: "directory",
    name: "build",
    async *values() {},
    getDirectoryHandle: async () => {
      throw new Error("not implemented");
    },
    getFileHandle: async () => ({
      kind: "file",
      name: "AGENTS.md",
      getFile: async () => new File(["# Rules"], "AGENTS.md"),
    }),
  };
  assert.equal(await readAgentsMd(present), "# Rules");

  const absent: ProjectDirectoryHandle = {
    ...present,
    getFileHandle: async () => {
      throw new DOMException("missing", "NotFoundError");
    },
  };
  assert.equal(await readAgentsMd(absent), null);
});

test("writeAgentsMd stores the encoded file bytes", async () => {
  const { root, written } = memoryRoot();
  const size = await writeAgentsMd(root, "# Hello\n");
  assert.equal(size, 8);
  const bytes = written.get("AGENTS.md");
  assert.ok(bytes !== undefined);
  assert.equal(new TextDecoder().decode(bytes), "# Hello\n");
});

// ---------------------------------------------------------------------------
// Output paths, base64, and result sync
// ---------------------------------------------------------------------------

const TASK_ID = "00000000-0000-4000-8000-000000000001";

test("normalizeOutputPath rejects absolute and traversal paths", () => {
  assert.equal(normalizeOutputPath(".dhi/t1/report.md"), ".dhi/t1/report.md");
  assert.equal(normalizeOutputPath("src\\app\\\\page.tsx"), "src/app/page.tsx");
  assert.equal(normalizeOutputPath("./src/./a.ts"), "src/a.ts");
  assert.equal(normalizeOutputPath("../escape.txt"), null);
  assert.equal(normalizeOutputPath("src/../../escape.txt"), null);
  assert.equal(normalizeOutputPath("/etc/passwd"), null);
  assert.equal(normalizeOutputPath("C:/Windows/x"), null);
  assert.equal(normalizeOutputPath(""), null);
  assert.equal(normalizeOutputPath("   "), null);
});

test("outputRelativePath preserves .dhi and nests bare paths by task", () => {
  assert.equal(
    outputRelativePath(TASK_ID, ".dhi/t1/report.md"),
    ".dhi/t1/report.md",
  );
  assert.equal(
    outputRelativePath(TASK_ID, "src/result.txt"),
    `.dhi/${TASK_ID}/src/result.txt`,
  );
  assert.equal(outputRelativePath(TASK_ID, "../escape.txt"), null);
  assert.equal(outputRelativePath(TASK_ID, "/abs.txt"), null);
  assert.equal(outputRelativePath("bad/id", "x.txt"), null);
});

test("decodeBase64 and decodeOutputContent handle both encodings", () => {
  assert.equal(
    new TextDecoder().decode(decodeBase64("aGVsbG8=")),
    "hello",
  );
  assert.equal(decodeBase64("").length, 0);
  // URL-safe aliases and whitespace are tolerated.
  assert.equal(
    new TextDecoder().decode(decodeBase64("aGVs\nbG8")),
    "hello",
  );
  assert.deepEqual(
    Array.from(decodeBase64("AAECAw==")),
    [0, 1, 2, 3],
  );

  const utf8: TaskOutputFile = {
    path: ".dhi/t1/a.txt",
    content: "héllo",
    encoding: "utf8",
  };
  assert.equal(
    new TextDecoder().decode(decodeOutputContent(utf8)),
    "héllo",
  );
  const base64: TaskOutputFile = {
    path: ".dhi/t1/a.bin",
    content: "AAECAw==",
    encoding: "base64",
  };
  assert.deepEqual(Array.from(decodeOutputContent(base64)), [0, 1, 2, 3]);
});

test("syncTaskOutputs writes, skips unsafe paths, and counts errors", async () => {
  const { root, written } = memoryRoot();
  const summary = await syncTaskOutputs(root, TASK_ID, [
    {
      path: ".dhi/t1/report.md",
      content: "# done",
      encoding: "utf8",
    },
    {
      path: "notes.txt",
      content: "aGVsbG8=",
      encoding: "base64",
    },
    {
      path: "../escape.txt",
      content: "nope",
      encoding: "utf8",
    },
    {
      path: ".dhi/t1/bad.bin",
      content: "!!!not-base64!!!",
      encoding: "base64",
    },
  ]);

  assert.equal(summary.written, 2);
  assert.equal(summary.skipped, 1);
  assert.equal(summary.errors, 1);
  assert.deepEqual(summary.writtenPaths, [
    ".dhi/t1/report.md",
    `.dhi/${TASK_ID}/notes.txt`,
  ]);
  assert.deepEqual(summary.skippedPaths, ["../escape.txt"]);
  assert.match(summary.errorMessages[0] ?? "", /base64|invalid/i);
  assert.equal(
    new TextDecoder().decode(written.get(".dhi/t1/report.md")),
    "# done",
  );
  assert.equal(
    new TextDecoder().decode(written.get(`.dhi/${TASK_ID}/notes.txt`)),
    "hello",
  );
});

// ---------------------------------------------------------------------------
// API helpers unwrap their envelopes
// ---------------------------------------------------------------------------

const originalFetch = globalThis.fetch;

afterEach(() => {
  globalThis.fetch = originalFetch;
});

function stubFetch(
  respond: (url: string, init?: RequestInit) => Response,
): Array<{ url: string; init?: RequestInit }> {
  const calls: Array<{ url: string; init?: RequestInit }> = [];
  globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    calls.push({ url, init });
    return respond(url, init);
  }) as typeof fetch;
  return calls;
}

function jsonResponse(body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status: 200,
    headers: { "content-type": "application/json" },
  });
}

test("listWebProjects unwraps { projects }", async () => {
  const calls = stubFetch(() => jsonResponse({ projects: [webProject] }));
  assert.deepEqual(await listWebProjects(), [webProject]);
  assert.equal(calls[0]?.url, "/api/web-projects");
});

test("createWebProject posts { name } and unwraps { project }", async () => {
  const calls = stubFetch(() => jsonResponse({ project: webProject }));
  assert.deepEqual(await createWebProject("Browser build"), webProject);
  assert.equal(calls[0]?.url, "/api/web-projects");
  assert.equal(calls[0]?.init?.method, "POST");
  assert.deepEqual(JSON.parse(String(calls[0]?.init?.body)), {
    name: "Browser build",
  });
});

test("getWebProject unwraps { project }", async () => {
  const calls = stubFetch(() => jsonResponse({ project: webProject }));
  assert.deepEqual(await getWebProject(webProject.id), webProject);
  assert.equal(calls[0]?.url, `/api/web-projects/${webProject.id}`);
});

test("uploadWebProjectFile PUTs raw bytes with an encoded path", async () => {
  const calls = stubFetch(() =>
    jsonResponse({ path: "src/a b.ts", size: 3 }),
  );
  const bytes = new Uint8Array([1, 2, 3]);
  assert.deepEqual(
    await uploadWebProjectFile(webProject.id, "src/a b.ts", bytes),
    { path: "src/a b.ts", size: 3 },
  );
  assert.equal(
    calls[0]?.url,
    `/api/web-projects/${webProject.id}/files?path=src%2Fa%20b.ts`,
  );
  assert.equal(calls[0]?.init?.method, "PUT");
  assert.equal(
    calls[0]?.init?.headers &&
      (calls[0].init.headers as Record<string, string>)["content-type"],
    "application/octet-stream",
  );
  assert.deepEqual(
    Array.from(calls[0]?.init?.body as Uint8Array),
    [1, 2, 3],
  );
});

test("getTaskOutputs unwraps { files }", async () => {
  const files: TaskOutputFile[] = [
    { path: ".dhi/t1/report.md", content: "done", encoding: "utf8" },
  ];
  const calls = stubFetch(() => jsonResponse({ files }));
  assert.deepEqual(await getTaskOutputs(TASK_ID), files);
  assert.equal(calls[0]?.url, `/api/tasks/${TASK_ID}/outputs`);
});
