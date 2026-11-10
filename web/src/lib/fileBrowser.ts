/**
 * Pure helpers for the server-side host file browser (`GET /api/fs`) and the
 * create-task project selection. No React, no fetch: the dialog and the form
 * both consume these rules, so the tests can drive them directly.
 */

import type { FileBrowserEntry } from "./types.ts";

/** The two browsing modes: folder picker and `AGENTS.md` picker. */
export type FileBrowserMode = "folder" | "file";

/** The only file name the file picker lets the user select. */
export const AGENTS_MD_FILE_NAME = "AGENTS.md";

/** Folder mode lists directories only. */
export function visibleFileBrowserEntries(
  entries: readonly FileBrowserEntry[],
  mode: FileBrowserMode,
): FileBrowserEntry[] {
  return mode === "folder"
    ? entries.filter((entry) => entry.kind === "directory")
    : [...entries];
}

/** True for a file entry named exactly `AGENTS.md`. */
export function isAgentsMdEntry(entry: FileBrowserEntry): boolean {
  return entry.kind === "file" && entry.name === AGENTS_MD_FILE_NAME;
}

/** Whether one file entry can be selected in the picker's current mode. */
export function isSelectableFileBrowserEntry(
  entry: FileBrowserEntry,
  mode: FileBrowserMode,
): boolean {
  return mode === "file" && isAgentsMdEntry(entry);
}

/** Create-form project selection: both values are absolute host paths. */
export type ProjectSelection = {
  projectFolder: string;
  /** Blank means no AGENTS.md was selected; the payload sends `null`. */
  agentsMdPath: string;
};

export const EMPTY_PROJECT_SELECTION: ProjectSelection = {
  projectFolder: "",
  agentsMdPath: "",
};

/**
 * Applies a folder-picker result. The AGENTS.md selection is independent of
 * the folder: the backend accepts any AGENTS.md under its browse root, inside
 * or outside the selected project folder, so it is preserved here.
 */
export function withProjectFolder(
  selection: ProjectSelection,
  folder: string,
): ProjectSelection {
  return {
    projectFolder: folder.trim(),
    agentsMdPath: selection.agentsMdPath.trim(),
  };
}

/** Applies an `AGENTS.md` picker result. */
export function withAgentsMdPath(
  selection: ProjectSelection,
  path: string,
): ProjectSelection {
  return { ...selection, agentsMdPath: path.trim() };
}

/** Clears the optional `AGENTS.md` selection. */
export function clearAgentsMdPath(
  selection: ProjectSelection,
): ProjectSelection {
  return { ...selection, agentsMdPath: "" };
}

/** Compact byte size for file-browser rows. */
export function formatFileSize(bytes: number | undefined): string {
  if (bytes === undefined || !Number.isFinite(bytes) || bytes <= 0) {
    return "";
  }
  if (bytes < 1024) {
    return `${bytes} B`;
  }
  const kb = bytes / 1024;
  if (kb < 1024) {
    return `${trimDecimal(kb)} KB`;
  }
  const mb = kb / 1024;
  if (mb < 1024) {
    return `${trimDecimal(mb)} MB`;
  }
  return `${trimDecimal(mb / 1024)} GB`;
}

function trimDecimal(value: number): string {
  return Number.isInteger(value) ? String(value) : value.toFixed(1);
}
