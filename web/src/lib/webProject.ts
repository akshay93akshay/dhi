/**
 * Browser-folder integration for web projects.
 *
 * The snapshot is uploaded through the ordinary JSON API, but the folder
 * handle never leaves this browser: it is kept in IndexedDB so a reload can
 * ask for permission again, and result sync writes straight back through the
 * File System Access API.
 *
 * Pure helpers (walk rules, limits, path handling, base64 decode) are
 * deliberately free of React and of module-load-time `window`/`indexedDB`
 * access, so `webProject.test.ts` can drive them under `node --test`.
 */

import {
  createWebProject,
  getTaskOutputs,
  uploadWebProjectFile,
} from "./api.ts";
import type { TaskOutputFile, WebProject } from "./types.ts";

// ---------------------------------------------------------------------------
// Limits, skip rules, and user-facing constants
// ---------------------------------------------------------------------------

/**
 * Snapshot directories that never belong in a web project. `.dhi` is DHI's
 * own per-task output folder, so repeat uploads never re-ingest prior results.
 */
export const WEB_PROJECT_SKIP_DIRECTORIES = [
  ".git",
  ".dhi",
  "node_modules",
  "target",
  ".next",
  "dist",
  "build",
  "out",
  ".cache",
] as const;

const SKIP_DIRECTORY_SET = new Set<string>(WEB_PROJECT_SKIP_DIRECTORIES);

/** Per-file upload limit (8 MB). */
export const WEB_PROJECT_MAX_FILE_BYTES = 8 * 1024 * 1024;

/** Whole-snapshot upload limit (256 MB). */
export const WEB_PROJECT_MAX_TOTAL_BYTES = 256 * 1024 * 1024;

/** Human-readable limits shown next to the upload controls. */
export const WEB_PROJECT_LIMITS_TEXT = "8 MB per file · 256 MB per snapshot";

/** Shown wherever the File System Access API is missing. */
export const FILE_SYSTEM_ACCESS_FALLBACK_TEXT =
  "Folder uploads and result sync need Chrome or Edge (File System Access API). Docker project folders still work.";

/** Stable IndexedDB coordinates for saved directory handles. */
export const PROJECT_HANDLE_DB_NAME = "dhi-web-project";
export const PROJECT_HANDLE_STORE_NAME = "handles";
export const PROJECT_HANDLE_KEY_PREFIX = "project-folder";

/**
 * IndexedDB key for one project's folder handle. Every web project owns its
 * own key so sync can never silently write through another project's folder.
 */
export function projectHandleKey(projectId: string): string {
  const id = projectId.trim();
  if (id === "") {
    throw new Error("A web project id is required to store a folder handle.");
  }
  return `${PROJECT_HANDLE_KEY_PREFIX}:${id}`;
}

/** True when the current directory name is excluded from snapshots. */
export function shouldSkipDirectory(name: string): boolean {
  return SKIP_DIRECTORY_SET.has(name.trim().toLowerCase());
}

/** Compact byte size for progress and limit text. */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) {
    return "0 B";
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

/** Whether one file is over the per-file upload limit. */
export function exceedsFileLimit(
  size: number,
  limit = WEB_PROJECT_MAX_FILE_BYTES,
): boolean {
  return size > limit;
}

/** Whether the snapshot total is over the whole-upload limit. */
export function exceedsTotalLimit(
  totalBytes: number,
  limit = WEB_PROJECT_MAX_TOTAL_BYTES,
): boolean {
  return totalBytes > limit;
}

// ---------------------------------------------------------------------------
// Minimal structural File System Access types
// ---------------------------------------------------------------------------

export type ProjectPermissionState = "granted" | "denied" | "prompt";

export type ProjectWritable = {
  write(data: Uint8Array): Promise<void>;
  close(): Promise<void>;
  abort?(): Promise<void>;
};

export type ProjectFileHandle = {
  kind: "file";
  name: string;
  getFile(): Promise<File>;
  createWritable?(): Promise<ProjectWritable>;
};

export type ProjectDirectoryHandle = {
  kind: "directory";
  name: string;
  values(): AsyncIterable<ProjectFileHandle | ProjectDirectoryHandle>;
  getDirectoryHandle(
    name: string,
    options?: { create?: boolean },
  ): Promise<ProjectDirectoryHandle>;
  getFileHandle(
    name: string,
    options?: { create?: boolean },
  ): Promise<ProjectFileHandle>;
  queryPermission?(descriptor?: {
    mode?: "read" | "readwrite";
  }): Promise<ProjectPermissionState>;
  requestPermission?(descriptor?: {
    mode?: "read" | "readwrite";
  }): Promise<ProjectPermissionState>;
};

type DirectoryPickerWindow = Window & {
  showDirectoryPicker?: (options?: {
    id?: string;
    mode?: "read" | "readwrite";
  }) => Promise<unknown>;
};

/** True when `window.showDirectoryPicker` is available. */
export function isFileSystemAccessSupported(): boolean {
  if (typeof window === "undefined") {
    return false;
  }
  return (
    typeof (window as DirectoryPickerWindow).showDirectoryPicker === "function"
  );
}

// ---------------------------------------------------------------------------
// IndexedDB handle persistence
// ---------------------------------------------------------------------------

function openHandleDatabase(): Promise<IDBDatabase> {
  if (typeof indexedDB === "undefined") {
    return Promise.reject(
      new Error("IndexedDB is unavailable, so the folder cannot be remembered."),
    );
  }
  return new Promise((resolve, reject) => {
    const request = indexedDB.open(PROJECT_HANDLE_DB_NAME, 1);
    request.onupgradeneeded = () => {
      if (!request.result.objectStoreNames.contains(PROJECT_HANDLE_STORE_NAME)) {
        request.result.createObjectStore(PROJECT_HANDLE_STORE_NAME);
      }
    };
    request.onsuccess = () => resolve(request.result);
    request.onerror = () =>
      reject(request.error ?? new Error("Could not open IndexedDB."));
  });
}

/** Stores one project's directory handle under its per-project key. */
export async function saveProjectHandle(
  projectId: string,
  handle: ProjectDirectoryHandle,
): Promise<void> {
  const key = projectHandleKey(projectId);
  const database = await openHandleDatabase();
  try {
    await new Promise<void>((resolve, reject) => {
      const transaction = database.transaction(
        PROJECT_HANDLE_STORE_NAME,
        "readwrite",
      );
      transaction.objectStore(PROJECT_HANDLE_STORE_NAME).put(handle, key);
      transaction.oncomplete = () => resolve();
      transaction.onerror = () =>
        reject(transaction.error ?? new Error("Could not save the folder."));
      transaction.onabort = () =>
        reject(transaction.error ?? new Error("Could not save the folder."));
    });
  } finally {
    database.close();
  }
}

/**
 * Loads the directory handle saved for exactly this project. Returns `null`
 * when this project has no handle; it never falls back to another project's
 * folder.
 */
export async function loadProjectHandle(
  projectId: string,
): Promise<ProjectDirectoryHandle | null> {
  const id = projectId.trim();
  if (id === "") {
    return null;
  }
  const key = projectHandleKey(id);
  const database = await openHandleDatabase();
  try {
    const value = await new Promise<unknown>((resolve, reject) => {
      const transaction = database.transaction(
        PROJECT_HANDLE_STORE_NAME,
        "readonly",
      );
      const request = transaction
        .objectStore(PROJECT_HANDLE_STORE_NAME)
        .get(key);
      request.onsuccess = () => resolve(request.result);
      request.onerror = () =>
        reject(request.error ?? new Error("Could not read the folder."));
    });
    if (
      typeof value === "object" &&
      value !== null &&
      (value as { kind?: unknown }).kind === "directory"
    ) {
      return value as ProjectDirectoryHandle;
    }
    return null;
  } finally {
    database.close();
  }
}

/** Forgets one project's saved handle. */
export async function clearProjectHandle(projectId: string): Promise<void> {
  const key = projectHandleKey(projectId);
  const database = await openHandleDatabase();
  try {
    await new Promise<void>((resolve, reject) => {
      const transaction = database.transaction(
        PROJECT_HANDLE_STORE_NAME,
        "readwrite",
      );
      transaction
        .objectStore(PROJECT_HANDLE_STORE_NAME)
        .delete(key);
      transaction.oncomplete = () => resolve();
      transaction.onerror = () =>
        reject(transaction.error ?? new Error("Could not clear the folder."));
    });
  } finally {
    database.close();
  }
}

/** Folder-handle lookup used by the task sync card. */
export async function loadProjectFolderHandle(
  projectId: string,
): Promise<{
  handle: ProjectDirectoryHandle | null;
  needsFolderChoice: boolean;
}> {
  const handle = await loadProjectHandle(projectId);
  return { handle, needsFolderChoice: handle === null };
}

// ---------------------------------------------------------------------------
// Folder selection and permissions
// ---------------------------------------------------------------------------

/**
 * Opens the browser folder picker in read/write mode. Must be called from a
 * user gesture. When `projectId` is given the handle is saved under that
 * project immediately (the task sync path); the start page passes no id and
 * saves after the web project is created. Returns `null` on cancel.
 */
export async function pickProjectFolder(
  projectId?: string,
): Promise<ProjectDirectoryHandle | null> {
  if (!isFileSystemAccessSupported()) {
    throw new Error(FILE_SYSTEM_ACCESS_FALLBACK_TEXT);
  }
  const picker = window as DirectoryPickerWindow;
  try {
    const handle = await picker.showDirectoryPicker?.({
      id: "dhi-project",
      mode: "readwrite",
    });
    if (handle === undefined) {
      return null;
    }
    const directory = handle as ProjectDirectoryHandle;
    if (projectId !== undefined && projectId.trim() !== "") {
      await saveProjectHandle(projectId, directory);
    }
    return directory;
  } catch (error) {
    // Cancelling the picker is not a failure.
    if (error instanceof DOMException && error.name === "AbortError") {
      return null;
    }
    throw error;
  }
}

function normalizePermission(value: unknown): ProjectPermissionState {
  return value === "granted" || value === "denied" || value === "prompt"
    ? value
    : "prompt";
}

/** Reads the current read/write permission without prompting. */
export async function projectHandlePermission(
  handle: ProjectDirectoryHandle,
  mode: "read" | "readwrite" = "readwrite",
): Promise<ProjectPermissionState> {
  if (typeof handle.queryPermission !== "function") {
    // Test doubles and browsers without the permission extension: the handle
    // itself already implies the grant.
    return "granted";
  }
  try {
    return normalizePermission(await handle.queryPermission({ mode }));
  } catch {
    return "prompt";
  }
}

/** Requests read/write permission. Call from a user gesture. */
export async function requestProjectPermission(
  handle: ProjectDirectoryHandle,
  mode: "read" | "readwrite" = "readwrite",
): Promise<ProjectPermissionState> {
  if (typeof handle.requestPermission !== "function") {
    return "granted";
  }
  try {
    return normalizePermission(await handle.requestPermission({ mode }));
  } catch {
    return "denied";
  }
}

/** Queries, then requests when needed. Must run from a user gesture to prompt. */
export async function ensureProjectPermission(
  handle: ProjectDirectoryHandle,
  mode: "read" | "readwrite" = "readwrite",
): Promise<boolean> {
  if ((await projectHandlePermission(handle, mode)) === "granted") {
    return true;
  }
  return (await requestProjectPermission(handle, mode)) === "granted";
}

// ---------------------------------------------------------------------------
// Snapshot walking
// ---------------------------------------------------------------------------

export type ProjectSnapshotFile = {
  /** Folder-relative path, `/` separated, e.g. `src/app/page.tsx`. */
  path: string;
  file: File;
  size: number;
};

export type DroppedSnapshotFile = {
  path: string;
  size: number;
  reason: "file_limit";
};

export type ProjectSnapshot = {
  files: ProjectSnapshotFile[];
  totalBytes: number;
  skippedDirectories: string[];
  droppedFiles: DroppedSnapshotFile[];
};

export type WalkProjectOptions = {
  maxFileBytes?: number;
  maxTotalBytes?: number;
  signal?: AbortSignal;
};

/**
 * Walks a selected folder recursively, newest contents included. Skipped
 * directories are recorded (not thrown); files over the per-file limit are
 * dropped into `droppedFiles` so the UI can report them. The total limit is
 * validated by {@link snapshotLimitError} before any upload starts.
 */
export async function walkProjectDirectory(
  root: ProjectDirectoryHandle,
  options: WalkProjectOptions = {},
): Promise<ProjectSnapshot> {
  const maxFileBytes = options.maxFileBytes ?? WEB_PROJECT_MAX_FILE_BYTES;
  const files: ProjectSnapshotFile[] = [];
  const skippedDirectories: string[] = [];
  const droppedFiles: DroppedSnapshotFile[] = [];
  let totalBytes = 0;

  async function visit(
    directory: ProjectDirectoryHandle,
    prefix: string,
  ): Promise<void> {
    const entries: Array<ProjectFileHandle | ProjectDirectoryHandle> = [];
    for await (const entry of directory.values()) {
      entries.push(entry);
    }
    entries.sort((left, right) => left.name.localeCompare(right.name));

    for (const entry of entries) {
      throwIfAborted(options.signal);
      const path = prefix === "" ? entry.name : `${prefix}/${entry.name}`;
      if (entry.kind === "directory") {
        if (shouldSkipDirectory(entry.name)) {
          skippedDirectories.push(path);
          continue;
        }
        await visit(entry, path);
        continue;
      }
      const file = await entry.getFile();
      if (exceedsFileLimit(file.size, maxFileBytes)) {
        droppedFiles.push({
          path,
          size: file.size,
          reason: "file_limit",
        });
        continue;
      }
      files.push({ path, file, size: file.size });
      totalBytes += file.size;
    }
  }

  await visit(root, "");
  return { files, totalBytes, skippedDirectories, droppedFiles };
}

/** A clear one-line failure for an over-limit snapshot, or `null`. */
export function snapshotLimitError(
  snapshot: Pick<ProjectSnapshot, "files" | "totalBytes">,
  limits: { maxFileBytes?: number; maxTotalBytes?: number } = {},
): string | null {
  const maxFileBytes = limits.maxFileBytes ?? WEB_PROJECT_MAX_FILE_BYTES;
  const maxTotalBytes = limits.maxTotalBytes ?? WEB_PROJECT_MAX_TOTAL_BYTES;

  for (const entry of snapshot.files) {
    if (exceedsFileLimit(entry.size, maxFileBytes)) {
      return `“${entry.path}” is ${formatBytes(entry.size)}; the per-file limit is ${formatBytes(maxFileBytes)}.`;
    }
  }
  if (exceedsTotalLimit(snapshot.totalBytes, maxTotalBytes)) {
    return `The snapshot is ${formatBytes(snapshot.totalBytes)}; the total limit is ${formatBytes(maxTotalBytes)}.`;
  }
  return null;
}

/** Thrown by uploads when the snapshot violates a size limit. */
export class ProjectSnapshotError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ProjectSnapshotError";
  }
}

// ---------------------------------------------------------------------------
// Upload
// ---------------------------------------------------------------------------

export type SnapshotUploadProgress = {
  uploadedFiles: number;
  totalFiles: number;
  uploadedBytes: number;
  totalBytes: number;
  currentPath: string | null;
};

export type UploadProjectFilesOptions = {
  signal?: AbortSignal;
  onProgress?: (progress: SnapshotUploadProgress) => void;
};

export type UploadProjectFilesResult = {
  uploaded: number;
  totalBytes: number;
};

function throwIfAborted(signal?: AbortSignal): void {
  if (signal?.aborted === true) {
    throw abortError();
  }
}

function abortError(): Error {
  if (typeof DOMException === "function") {
    return new DOMException("Upload cancelled.", "AbortError");
  }
  const error = new Error("Upload cancelled.");
  error.name = "AbortError";
  return error;
}

/**
 * Uploads every file of a snapshot, one at a time so progress stays honest
 * and an abort stops at a file boundary. `projectId` must already exist.
 */
export async function uploadProjectFiles(
  projectId: string,
  snapshot: ProjectSnapshot,
  options: UploadProjectFilesOptions = {},
): Promise<UploadProjectFilesResult> {
  const limitError = snapshotLimitError(snapshot);
  if (limitError !== null) {
    throw new ProjectSnapshotError(limitError);
  }

  const totalFiles = snapshot.files.length;
  let uploadedFiles = 0;
  let uploadedBytes = 0;
  const report = (currentPath: string | null): void => {
    options.onProgress?.({
      uploadedFiles,
      totalFiles,
      uploadedBytes,
      totalBytes: snapshot.totalBytes,
      currentPath,
    });
  };

  report(null);
  for (const entry of snapshot.files) {
    throwIfAborted(options.signal);
    report(entry.path);
    const bytes = new Uint8Array(await entry.file.arrayBuffer());
    throwIfAborted(options.signal);
    try {
      await uploadWebProjectFile(
        projectId,
        entry.path,
        bytes,
        options.signal,
      );
    } catch (error) {
      // `request` wraps fetch failures in ApiError, including an abort;
      // surface the cancellation as AbortError for the UI's cancel path.
      if (options.signal?.aborted === true) {
        throw abortError();
      }
      throw error;
    }
    uploadedFiles += 1;
    uploadedBytes += entry.size;
    report(null);
  }
  return { uploaded: uploadedFiles, totalBytes: uploadedBytes };
}

/**
 * Creates the web project, then uploads the snapshot. The project is created
 * first so a partial upload still leaves a retryable project behind; callers
 * that want retry should use `createWebProject` + `uploadProjectFiles`
 * directly.
 */
export async function uploadProjectSnapshot(options: {
  name: string;
  snapshot: ProjectSnapshot;
  signal?: AbortSignal;
  onProgress?: (progress: SnapshotUploadProgress) => void;
}): Promise<{ project: WebProject } & UploadProjectFilesResult> {
  const project = await createWebProject(options.name);
  const result = await uploadProjectFiles(project.id, options.snapshot, {
    signal: options.signal,
    onProgress: options.onProgress,
  });
  return { project, ...result };
}

// ---------------------------------------------------------------------------
// AGENTS.md
// ---------------------------------------------------------------------------

function isNotFoundError(error: unknown): boolean {
  return (
    (error instanceof DOMException && error.name === "NotFoundError") ||
    (typeof error === "object" &&
      error !== null &&
      (error as { name?: unknown }).name === "NotFoundError")
  );
}

/** Reads `AGENTS.md` from the selected folder, or `null` when absent. */
export async function readAgentsMd(
  root: ProjectDirectoryHandle,
): Promise<string | null> {
  try {
    const handle = await root.getFileHandle("AGENTS.md");
    const file = await handle.getFile();
    return await file.text();
  } catch (error) {
    if (isNotFoundError(error)) {
      return null;
    }
    throw error;
  }
}

/** Writes `AGENTS.md` through the folder handle; returns the byte length. */
export async function writeAgentsMd(
  root: ProjectDirectoryHandle,
  text: string,
): Promise<number> {
  const handle = await root.getFileHandle("AGENTS.md", { create: true });
  if (typeof handle.createWritable !== "function") {
    throw new Error("This browser cannot write through the folder handle.");
  }
  const bytes = new TextEncoder().encode(text);
  const writable = await handle.createWritable();
  try {
    await writable.write(bytes);
    await writable.close();
  } catch (error) {
    try {
      await writable.abort?.();
    } catch {
      // The write already failed; the abort is best-effort cleanup.
    }
    throw error;
  }
  return bytes.byteLength;
}

// ---------------------------------------------------------------------------
// Result sync
// ---------------------------------------------------------------------------

/** Normalizes a server-supplied relative path, rejecting traversal. */
export function normalizeOutputPath(raw: string): string | null {
  if (typeof raw !== "string") {
    return null;
  }
  const text = raw.trim().replace(/\\/g, "/");
  if (text === "" || text.startsWith("/") || text.includes("\0")) {
    return null;
  }
  if (/^[A-Za-z]:/.test(text)) {
    return null;
  }
  const segments: string[] = [];
  for (const segment of text.split("/")) {
    if (segment === "" || segment === ".") {
      continue;
    }
    if (segment === "..") {
      return null;
    }
    segments.push(segment);
  }
  return segments.length === 0 ? null : segments.join("/");
}

/**
 * Destination of one task output under the selected folder. Outputs already
 * rooted at `.dhi/…` keep their relative path; bare paths are nested under
 * `.dhi/<task-id>/` so the per-task folder layout is preserved.
 */
export function outputRelativePath(
  taskId: string,
  rawPath: string,
): string | null {
  const normalized = normalizeOutputPath(rawPath);
  if (normalized === null) {
    return null;
  }
  if (normalized === ".dhi" || normalized.startsWith(".dhi/")) {
    return normalized;
  }
  const taskSegment = normalizeOutputPath(taskId);
  if (taskSegment === null || taskSegment.includes("/")) {
    return null;
  }
  return `.dhi/${taskSegment}/${normalized}`;
}

/** Decodes standard base64 (URL-safe aliases and whitespace tolerated). */
export function decodeBase64(content: string): Uint8Array {
  const cleaned = content
    .replace(/\s+/g, "")
    .replace(/-/g, "+")
    .replace(/_/g, "/");
  if (cleaned === "") {
    return new Uint8Array(0);
  }
  if (typeof atob !== "function") {
    throw new Error("Base64 decoding is unavailable in this browser.");
  }
  const binary = atob(cleaned);
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) {
    bytes[index] = binary.charCodeAt(index);
  }
  return bytes;
}

/** Turns one output file into bytes according to its declared encoding. */
export function decodeOutputContent(file: TaskOutputFile): Uint8Array {
  if (file.encoding === "base64") {
    return decodeBase64(file.content);
  }
  if (file.encoding === "utf8") {
    return new TextEncoder().encode(file.content);
  }
  throw new Error(`Unsupported output encoding “${String(file.encoding)}”.`);
}

/** Writes bytes under a relative path, creating every parent directory. */
export async function writeProjectFile(
  root: ProjectDirectoryHandle,
  relativePath: string,
  bytes: Uint8Array,
): Promise<string> {
  const normalized = normalizeOutputPath(relativePath);
  if (normalized === null) {
    throw new Error("Unsafe output path.");
  }
  const segments = normalized.split("/");
  const fileName = segments.pop();
  if (fileName === undefined) {
    throw new Error("Unsafe output path.");
  }
  let directory = root;
  for (const segment of segments) {
    directory = await directory.getDirectoryHandle(segment, { create: true });
  }
  const handle = await directory.getFileHandle(fileName, { create: true });
  if (typeof handle.createWritable !== "function") {
    throw new Error("This browser cannot write through the folder handle.");
  }
  const writable = await handle.createWritable();
  try {
    await writable.write(bytes);
    await writable.close();
  } catch (error) {
    try {
      await writable.abort?.();
    } catch {
      // Cleanup only.
    }
    throw error;
  }
  return normalized;
}

export type OutputSyncProgress = {
  processed: number;
  total: number;
  currentPath: string | null;
};

export type OutputSyncSummary = {
  written: number;
  skipped: number;
  errors: number;
  writtenPaths: string[];
  skippedPaths: string[];
  errorMessages: string[];
};

export type SyncTaskOutputsOptions = {
  signal?: AbortSignal;
  onProgress?: (progress: OutputSyncProgress) => void;
};

/**
 * Writes every fetched output file under the selected folder. Unsafe paths
 * count as skipped, a bad encoding or a failed write counts as an error, and
 * one bad row never stops the rest of the sync.
 */
export async function syncTaskOutputs(
  root: ProjectDirectoryHandle,
  taskId: string,
  files: readonly TaskOutputFile[],
  options: SyncTaskOutputsOptions = {},
): Promise<OutputSyncSummary> {
  const summary: OutputSyncSummary = {
    written: 0,
    skipped: 0,
    errors: 0,
    writtenPaths: [],
    skippedPaths: [],
    errorMessages: [],
  };
  const total = files.length;
  const report = (currentPath: string | null): void => {
    options.onProgress?.({
      processed: summary.written + summary.skipped + summary.errors,
      total,
      currentPath,
    });
  };

  report(null);
  for (const file of files) {
    throwIfAborted(options.signal);
    report(file.path);
    const relativePath = outputRelativePath(taskId, file.path);
    if (relativePath === null) {
      summary.skipped += 1;
      summary.skippedPaths.push(file.path);
      continue;
    }
    let bytes: Uint8Array;
    try {
      bytes = decodeOutputContent(file);
    } catch (error) {
      summary.errors += 1;
      summary.errorMessages.push(
        `${file.path}: ${errorMessage(error)}`,
      );
      continue;
    }
    try {
      await writeProjectFile(root, relativePath, bytes);
      summary.written += 1;
      summary.writtenPaths.push(relativePath);
    } catch (error) {
      summary.errors += 1;
      summary.errorMessages.push(`${file.path}: ${errorMessage(error)}`);
    }
    report(null);
  }
  return summary;
}

/** Fetch outputs and write them, in one call. */
export async function syncTaskResults(
  root: ProjectDirectoryHandle,
  taskId: string,
  options: SyncTaskOutputsOptions = {},
): Promise<OutputSyncSummary> {
  const files = await getTaskOutputs(taskId);
  return syncTaskOutputs(root, taskId, files, options);
}

export function errorMessage(error: unknown): string {
  if (error instanceof Error) {
    return error.message;
  }
  return String(error);
}

// ---------------------------------------------------------------------------
// Last-sync state (local only; the server has no sync-status route)
// ---------------------------------------------------------------------------

export type StoredSyncState = {
  written: number;
  skipped: number;
  errors: number;
  syncedAt: string;
};

const SYNC_STATE_PREFIX = "dhi:web-project-sync:";

export function loadSyncState(taskId: string): StoredSyncState | null {
  if (typeof localStorage === "undefined") {
    return null;
  }
  try {
    const raw = localStorage.getItem(`${SYNC_STATE_PREFIX}${taskId}`);
    if (raw === null) {
      return null;
    }
    const parsed = JSON.parse(raw) as Partial<StoredSyncState>;
    if (
      typeof parsed.written !== "number" ||
      typeof parsed.skipped !== "number" ||
      typeof parsed.errors !== "number" ||
      typeof parsed.syncedAt !== "string"
    ) {
      return null;
    }
    return {
      written: parsed.written,
      skipped: parsed.skipped,
      errors: parsed.errors,
      syncedAt: parsed.syncedAt,
    };
  } catch {
    return null;
  }
}

export function saveSyncState(taskId: string, state: StoredSyncState): void {
  if (typeof localStorage === "undefined") {
    return;
  }
  try {
    localStorage.setItem(
      `${SYNC_STATE_PREFIX}${taskId}`,
      JSON.stringify(state),
    );
  } catch {
    // Storage can be disabled or full; the in-memory state still renders.
  }
}
