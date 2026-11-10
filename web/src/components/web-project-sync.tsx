"use client";

import { useEffect, useRef, useState } from "react";
import {
  AlertTriangle,
  FolderOpen,
  RefreshCw,
  ShieldCheck,
  Square,
} from "lucide-react";

import { getTaskOutputs } from "@/lib/api";
import { formatLocalTimestamp } from "@/lib/localTime";
import type { Task } from "@/lib/types";
import {
  FILE_SYSTEM_ACCESS_FALLBACK_TEXT,
  ensureProjectPermission,
  errorMessage,
  isFileSystemAccessSupported,
  loadProjectFolderHandle,
  loadSyncState,
  pickProjectFolder,
  projectHandlePermission,
  saveSyncState,
  syncTaskOutputs,
  type OutputSyncProgress,
  type OutputSyncSummary,
  type ProjectDirectoryHandle,
  type ProjectPermissionState,
  type StoredSyncState,
} from "@/lib/webProject";
import { shortenId } from "@/lib/ui";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";

/**
 * Result sync for one web-project task. The component never navigates or
 * opens a new tab: bytes are written through the folder handle remembered for
 * this exact web project, with a leave-page warning while a sync is in flight.
 */
export function WebProjectSyncCard({
  task,
  className,
}: {
  task: Task;
  className?: string;
}) {
  const projectId = task.web_project_id ?? null;
  const [supported, setSupported] = useState(false);
  const [handle, setHandle] = useState<ProjectDirectoryHandle | null>(null);
  const [permission, setPermission] = useState<ProjectPermissionState | null>(
    null,
  );
  const [loadingHandle, setLoadingHandle] = useState(true);
  const [needsFolderChoice, setNeedsFolderChoice] = useState(false);
  const [storedSync, setStoredSync] = useState<StoredSyncState | null>(null);
  const [busy, setBusy] = useState(false);
  const [progress, setProgress] = useState<OutputSyncProgress | null>(null);
  const [summary, setSummary] = useState<OutputSyncSummary | null>(null);
  const [error, setError] = useState<string | null>(null);
  const abortRef = useRef<AbortController | null>(null);

  useEffect(() => {
    if (projectId === null) {
      return;
    }
    setSupported(isFileSystemAccessSupported());
    setStoredSync(loadSyncState(task.id));
    let alive = true;
    setLoadingHandle(true);
    // Never leave another project's handle visible while this project's
    // handle is being resolved.
    setHandle(null);
    setPermission(null);
    setNeedsFolderChoice(false);
    setError(null);
    setSummary(null);
    loadProjectFolderHandle(projectId)
      .then(async ({ handle: saved, needsFolderChoice: missing }) => {
        if (!alive) {
          return;
        }
        setNeedsFolderChoice(missing);
        if (saved === null) {
          return;
        }
        setHandle(saved);
        setPermission(await projectHandlePermission(saved));
      })
      .catch((cause: unknown) => {
        if (alive) {
          setError(errorMessage(cause));
        }
      })
      .finally(() => {
        if (alive) {
          setLoadingHandle(false);
        }
      });
    return () => {
      alive = false;
    };
  }, [projectId, task.id]);

  useEffect(() => {
    if (!busy) {
      return;
    }
    const warn = (event: BeforeUnloadEvent): void => {
      event.preventDefault();
      event.returnValue = "";
    };
    window.addEventListener("beforeunload", warn);
    return () => window.removeEventListener("beforeunload", warn);
  }, [busy]);

  if (projectId === null) {
    return null;
  }

  const projectName = task.web_project_name ?? `Web project ${shortenId(projectId)}`;

  const chooseFolder = async (): Promise<void> => {
    setError(null);
    try {
      const picked = await pickProjectFolder(projectId);
      if (picked === null) {
        return;
      }
      setHandle(picked);
      setNeedsFolderChoice(false);
      setPermission(await projectHandlePermission(picked));
    } catch (cause) {
      setError(errorMessage(cause));
    }
  };

  const grantAccess = async (): Promise<void> => {
    if (handle === null) {
      return;
    }
    setError(null);
    const granted = await ensureProjectPermission(handle);
    setPermission(granted ? "granted" : "denied");
    if (!granted) {
      setError("Folder access was not granted.");
    }
  };

  const startSync = async (): Promise<void> => {
    if (handle === null) {
      setError("Choose the project folder in this browser before syncing.");
      return;
    }
    const controller = new AbortController();
    abortRef.current = controller;
    setBusy(true);
    setError(null);
    setSummary(null);
    setProgress(null);
    try {
      if (!(await ensureProjectPermission(handle))) {
        setPermission("denied");
        setError("Folder access was not granted.");
        return;
      }
      setPermission("granted");
      const files = await getTaskOutputs(task.id);
      const result = await syncTaskOutputs(handle, task.id, files, {
        signal: controller.signal,
        onProgress: setProgress,
      });
      setSummary(result);
      const state: StoredSyncState = {
        written: result.written,
        skipped: result.skipped,
        errors: result.errors,
        syncedAt: new Date().toISOString(),
      };
      setStoredSync(state);
      saveSyncState(task.id, state);
    } catch (cause) {
      if (cause instanceof Error && cause.name === "AbortError") {
        setError("Sync cancelled.");
      } else {
        setError(errorMessage(cause));
      }
    } finally {
      abortRef.current = null;
      setBusy(false);
      setProgress(null);
    }
  };

  const permissionLabel =
    permission === "granted"
      ? "write access granted"
      : permission === "denied"
        ? "access denied"
        : "permission needed";

  return (
    <Card className={className}>
      <CardHeader className="border-b border-border pb-3">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <div className="flex min-w-0 items-center gap-2">
            <FolderOpen className="h-4 w-4 shrink-0 text-muted-foreground" />
            <CardTitle className="min-w-0 truncate">{projectName}</CardTitle>
          </div>
          <Badge variant="outline">Web project</Badge>
        </div>
        <CardDescription>
          Sync results back into the browser folder. Files keep their{" "}
          <code className="type-mono">.dhi/{shortenId(task.id)}/</code> path.
          Keep this tab open while syncing.
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-3 pt-4">
        <p className="type-meta text-muted-foreground">
          <span className="font-medium text-foreground">Sync state:</span>{" "}
          {busy
            ? progress === null
              ? "fetching results…"
              : `syncing ${progress.processed}/${progress.total}${
                  progress.currentPath === null
                    ? ""
                    : ` · ${progress.currentPath}`
                }`
            : storedSync === null
              ? "not synced in this browser yet"
              : `last synced ${formatLocalTimestamp(storedSync.syncedAt)} · ${
                  storedSync.written
                } synced · ${storedSync.skipped} skipped · ${
                  storedSync.errors
                } errors`}
        </p>

        {!supported ? (
          <p className="rounded-md border border-warning/30 bg-warning/8 px-3 py-2 type-meta text-warning">
            {FILE_SYSTEM_ACCESS_FALLBACK_TEXT}
          </p>
        ) : null}

        {supported ? (
          <>
            <p className="flex flex-wrap items-center gap-x-2 gap-y-1 type-meta text-muted-foreground">
              <FolderOpen className="h-3.5 w-3.5 shrink-0" />
              {loadingHandle ? (
                <span>Checking the remembered folder…</span>
              ) : needsFolderChoice || handle === null ? (
                <span>
                  No folder remembered for this project — choose it once in
                  this browser.
                </span>
              ) : (
                <>
                  <span className="font-medium text-foreground">
                    {handle.name}
                  </span>
                  <span aria-hidden="true">·</span>
                  <span>{permissionLabel}</span>
                </>
              )}
            </p>

            <div className="flex flex-wrap items-center gap-2">
              <Button
                type="button"
                size="sm"
                variant="outline"
                onClick={() => void chooseFolder()}
                disabled={busy}
              >
                <FolderOpen className="h-3.5 w-3.5" />
                {handle === null ? "Choose folder" : "Change folder"}
              </Button>
              {handle !== null && permission !== "granted" ? (
                <Button
                  type="button"
                  size="sm"
                  variant="outline"
                  onClick={() => void grantAccess()}
                  disabled={busy}
                >
                  <ShieldCheck className="h-3.5 w-3.5" />
                  Grant access
                </Button>
              ) : null}
              {busy ? (
                <Button
                  type="button"
                  size="sm"
                  variant="ghost"
                  onClick={() => abortRef.current?.abort()}
                >
                  <Square className="h-3.5 w-3.5" />
                  Cancel
                </Button>
              ) : (
                <Button
                  type="button"
                  size="sm"
                  onClick={() => void startSync()}
                  disabled={handle === null}
                >
                  <RefreshCw className="h-3.5 w-3.5" />
                  Sync results
                </Button>
              )}
            </div>
          </>
        ) : null}

        {summary !== null ? (
          <div className="rounded-md border border-border bg-surface-subtle px-3 py-2 type-meta">
            <p className="flex flex-wrap items-center gap-2">
              <Badge variant={summary.errors > 0 ? "warning" : "success"}>
                {summary.written} synced
              </Badge>
              <Badge variant="muted">{summary.skipped} skipped</Badge>
              <Badge variant={summary.errors > 0 ? "destructive" : "muted"}>
                {summary.errors} errors
              </Badge>
            </p>
            {summary.skippedPaths.length > 0 ? (
              <p className="mt-2 break-all text-muted-foreground">
                Skipped (unsafe path): {summary.skippedPaths.join(", ")}
              </p>
            ) : null}
            {summary.errorMessages.length > 0 ? (
              <ul className="mt-2 list-disc space-y-1 pl-4 text-destructive">
                {summary.errorMessages.map((message) => (
                  <li key={message} className="break-all">
                    {message}
                  </li>
                ))}
              </ul>
            ) : null}
          </div>
        ) : null}

        {error !== null ? (
          <p
            className="flex items-start gap-2 rounded-md border border-destructive/30 bg-destructive/8 px-3 py-2 type-meta text-destructive"
            role="alert"
          >
            <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
            <span>{error}</span>
          </p>
        ) : null}
      </CardContent>
    </Card>
  );
}
