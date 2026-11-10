import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { afterEach, test } from "node:test";

import { ApiError, listFileBrowser } from "./api.ts";
import {
  AGENTS_MD_FILE_NAME,
  EMPTY_PROJECT_SELECTION,
  clearAgentsMdPath,
  formatFileSize,
  isAgentsMdEntry,
  isSelectableFileBrowserEntry,
  visibleFileBrowserEntries,
  withAgentsMdPath,
  withProjectFolder,
} from "./fileBrowser.ts";
import type { FileBrowserEntry, FileBrowserResponse } from "./types.ts";

const originalFetch = globalThis.fetch;

afterEach(() => {
  globalThis.fetch = originalFetch;
});

function stubFetch(body: unknown, status = 200): string[] {
  const urls: string[] = [];
  globalThis.fetch = (async (input: RequestInfo | URL) => {
    urls.push(String(input));
    return new Response(JSON.stringify(body), {
      status,
      headers: { "content-type": "application/json" },
    });
  }) as typeof fetch;
  return urls;
}

const sampleListing: FileBrowserResponse = {
  root: "/srv",
  path: "/srv/build",
  parent: "/srv",
  entries: [
    { name: "src", path: "/srv/build/src", kind: "directory" },
    {
      name: "AGENTS.md",
      path: "/srv/build/AGENTS.md",
      kind: "file",
      size: 128,
      modified_at: "2026-11-10T00:00:00Z",
    },
    {
      name: "README.md",
      path: "/srv/build/README.md",
      kind: "file",
      size: 64,
    },
  ],
};

// ---------------------------------------------------------------------------
// GET /api/fs client
// ---------------------------------------------------------------------------

test("listFileBrowser starts at the root when path is omitted or blank", async () => {
  const urls = stubFetch(sampleListing);

  assert.deepEqual(await listFileBrowser(), sampleListing);
  await listFileBrowser("");
  await listFileBrowser("   ");

  assert.deepEqual(urls, ["/api/fs", "/api/fs", "/api/fs"]);
});

test("listFileBrowser URL-encodes an absolute directory path", async () => {
  const urls = stubFetch(sampleListing);

  const listing = await listFileBrowser("/srv/my project/AGENTS.md");

  assert.deepEqual(listing, sampleListing);
  assert.equal(
    urls[0],
    "/api/fs?path=%2Fsrv%2Fmy%20project%2FAGENTS.md",
  );
});

test("listFileBrowser surfaces a disabled browser as a 404 ApiError", async () => {
  stubFetch({ error: "file browser disabled" }, 404);

  await assert.rejects(
    () => listFileBrowser(),
    (error: unknown) =>
      error instanceof ApiError && error.status === 404 && error.isNotFound(),
  );
});

// ---------------------------------------------------------------------------
// Entry rules
// ---------------------------------------------------------------------------

const directoryEntry = sampleListing.entries[0];
const agentsEntry = sampleListing.entries[1];
const readmeEntry = sampleListing.entries[2];

test("folder mode shows directories only; file mode shows files too", () => {
  assert.deepEqual(
    visibleFileBrowserEntries(sampleListing.entries, "folder"),
    [directoryEntry],
  );
  assert.deepEqual(
    visibleFileBrowserEntries(sampleListing.entries, "file"),
    sampleListing.entries,
  );
});

test("only AGENTS.md files are selectable in file mode", () => {
  assert.equal(isAgentsMdEntry(agentsEntry), true);
  assert.equal(isAgentsMdEntry(readmeEntry), false);
  assert.equal(isAgentsMdEntry(directoryEntry), false);

  assert.equal(isSelectableFileBrowserEntry(directoryEntry, "folder"), false);
  assert.equal(isSelectableFileBrowserEntry(agentsEntry, "folder"), false);
  assert.equal(isSelectableFileBrowserEntry(readmeEntry, "file"), false);
  assert.equal(isSelectableFileBrowserEntry(agentsEntry, "file"), true);
  assert.equal(AGENTS_MD_FILE_NAME, "AGENTS.md");
});

test("folder select updates the form and preserves an independent AGENTS.md", () => {
  const selected = withProjectFolder(
    { projectFolder: "", agentsMdPath: "" },
    " /srv/build ",
  );
  assert.deepEqual(selected, {
    projectFolder: "/srv/build",
    agentsMdPath: "",
  });

  const withAgents = withAgentsMdPath(selected, "/srv/build/AGENTS.md");
  assert.deepEqual(withAgents, {
    projectFolder: "/srv/build",
    agentsMdPath: "/srv/build/AGENTS.md",
  });

  // Re-selecting the same folder keeps the matching file.
  assert.deepEqual(withProjectFolder(withAgents, "/srv/build"), withAgents);

  // Moving to another folder preserves the independently selected file.
  assert.deepEqual(withProjectFolder(withAgents, "/srv/other"), {
    projectFolder: "/srv/other",
    agentsMdPath: "/srv/build/AGENTS.md",
  });
});

test("folder select preserves an AGENTS.md selected outside the folder", () => {
  const selected = withAgentsMdPath(
    { projectFolder: "/srv/build", agentsMdPath: "" },
    "/srv/shared/AGENTS.md",
  );
  assert.deepEqual(withProjectFolder(selected, "/srv/other"), {
    projectFolder: "/srv/other",
    agentsMdPath: "/srv/shared/AGENTS.md",
  });
});

test("AGENTS.md select and Clear update the form", () => {
  const selected = withAgentsMdPath(
    { projectFolder: "/srv/build", agentsMdPath: "" },
    " /srv/build/AGENTS.md ",
  );
  assert.equal(selected.agentsMdPath, "/srv/build/AGENTS.md");

  assert.deepEqual(clearAgentsMdPath(selected), {
    projectFolder: "/srv/build",
    agentsMdPath: "",
  });
  assert.deepEqual(EMPTY_PROJECT_SELECTION, {
    projectFolder: "",
    agentsMdPath: "",
  });
  assert.equal(formatFileSize(128), "128 B");
  assert.equal(formatFileSize(2048), "2 KB");
  assert.equal(formatFileSize(undefined), "");
});

// ---------------------------------------------------------------------------
// Page wiring (source contract; the UI has no DOM test harness)
// ---------------------------------------------------------------------------

function source(relative: string): string {
  return readFileSync(new URL(relative, import.meta.url), "utf8");
}

test("create form wires folder and AGENTS.md selection into both fields", () => {
  const page = source("../app/page.tsx");
  const dialog = source("../components/file-browser-dialog.tsx");

  assert.match(page, /Project folder/);
  assert.match(page, /withProjectFolder/);
  assert.match(page, /withAgentsMdPath/);
  assert.match(page, /clearAgentsMdPath/);
  assert.match(page, /projectSelection\.projectFolder/);
  assert.match(page, /projectSelection\.agentsMdPath/);
  assert.match(page, /<FileBrowserDialog/);

  assert.match(dialog, /Select this folder/);
  assert.match(dialog, /Select AGENTS\.md/);
  assert.match(dialog, /onDoubleClick/);
  assert.match(dialog, /visibleFileBrowserEntries/);
  assert.match(dialog, /File browser unavailable/);
});

test("start page has no Chrome/Edge or web-folder upload surface", () => {
  const page = source("../app/page.tsx");

  assert.doesNotMatch(page, /Chrome|Edge/);
  assert.doesNotMatch(page, /Web folder/);
  assert.doesNotMatch(page, /WebProjectSyncCard/);
  assert.doesNotMatch(page, /web_project_id/);
  assert.doesNotMatch(page, /load_agents_md/);
  assert.doesNotMatch(page, /uploadWebProjectFile|listWebProjects|createWebProject/);
});

test("task pages show the folder and AGENTS.md path without a sync card", () => {
  const context = source("../components/project-context.tsx");
  const detail = source("../app/tasks/[id]/DetailClient.tsx");
  const work = source("../app/tasks/[id]/work/WorkClient.tsx");

  assert.match(context, /task\.project_folder/);
  assert.match(context, /task\.agents_md_path/);
  assert.match(context, /AGENTS\.md/);

  for (const page of [detail, work]) {
    assert.doesNotMatch(page, /WebProjectSyncCard/);
    assert.doesNotMatch(page, /web-project-sync/);
    assert.match(page, /ProjectContext/);
  }
});
