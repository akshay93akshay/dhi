"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import {
  ArrowUp,
  Check,
  FileText,
  FolderOpen,
  Home,
} from "lucide-react";

import { ApiError, listFileBrowser } from "@/lib/api";
import {
  formatFileSize,
  isSelectableFileBrowserEntry,
  visibleFileBrowserEntries,
  type FileBrowserMode,
} from "@/lib/fileBrowser";
import type { FileBrowserResponse } from "@/lib/types";
import { describeApiError } from "@/lib/ui";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";

/**
 * Server-side host file browser backed by `GET /api/fs`.
 *
 * Folder mode lists directories only and selects the current directory;
 * file mode also lists files but only `AGENTS.md` is selectable. Directories
 * are entered with a double-click (or Enter) in both modes.
 */
export function FileBrowserDialog(props: {
  open: boolean;
  mode: FileBrowserMode;
  initialPath: string;
  onOpenChange: (open: boolean) => void;
  onSelect: (path: string) => void;
  onUnavailable: () => void;
}) {
  const { open, mode, initialPath, onOpenChange, onSelect, onUnavailable } =
    props;
  const [listing, setListing] = useState<FileBrowserResponse | null>(null);
  const [loading, setLoading] = useState(false);
  const [unavailable, setUnavailable] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [selectedPath, setSelectedPath] = useState<string | null>(null);
  const requestRef = useRef(0);

  const load = useCallback(
    async (target: string): Promise<void> => {
      const requestId = requestRef.current + 1;
      requestRef.current = requestId;
      setLoading(true);
      setError(null);
      setUnavailable(false);
      setSelectedPath(null);
      try {
        const value = await listFileBrowser(target);
        if (requestRef.current !== requestId) {
          return;
        }
        setListing(value);
      } catch (cause) {
        if (requestRef.current !== requestId) {
          return;
        }
        setListing(null);
        if (cause instanceof ApiError && cause.isNotFound()) {
          setUnavailable(true);
          onUnavailable();
        } else {
          setError(describeApiError(cause));
        }
      } finally {
        if (requestRef.current === requestId) {
          setLoading(false);
        }
      }
    },
    [onUnavailable],
  );

  useEffect(() => {
    if (!open) {
      requestRef.current += 1;
      return;
    }
    setListing(null);
    setError(null);
    setUnavailable(false);
    setSelectedPath(null);
    void load(initialPath);
  }, [open, initialPath, load]);

  const entries =
    listing === null ? [] : visibleFileBrowserEntries(listing.entries, mode);
  const isFolderMode = mode === "folder";
  const title = isFolderMode ? "Choose project folder" : "Choose AGENTS.md";
  const description = isFolderMode
    ? "Browse folders on the DHI+ host. The task works directly in the selected folder."
    : "Browse the DHI+ host and select one AGENTS.md file.";
  const selectDisabled =
    loading || unavailable || listing === null || (!isFolderMode && selectedPath === null);

  const selectCurrent = (): void => {
    if (isFolderMode) {
      if (listing !== null) {
        onSelect(listing.path);
        onOpenChange(false);
      }
      return;
    }
    if (selectedPath !== null) {
      onSelect(selectedPath);
      onOpenChange(false);
    }
  };

  const enterDirectory = (path: string): void => {
    if (!loading) {
      void load(path);
    }
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-2xl">
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
          <DialogDescription>{description}</DialogDescription>
        </DialogHeader>

        <div className="space-y-3">
          <div className="flex flex-wrap items-center gap-2">
            <Button
              type="button"
              size="sm"
              variant="outline"
              onClick={() => void load("")}
              disabled={loading}
            >
              <Home className="h-3.5 w-3.5" />
              Root
            </Button>
            <Button
              type="button"
              size="sm"
              variant="outline"
              onClick={() => {
                if (listing?.parent != null) {
                  void load(listing.parent);
                }
              }}
              disabled={loading || listing?.parent == null}
            >
              <ArrowUp className="h-3.5 w-3.5" />
              Up
            </Button>
            <span className="min-w-0 flex-1 break-all type-mono text-muted-foreground">
              {listing?.path ?? "…"}
            </span>
          </div>

          {unavailable ? (
            <div
              className="rounded-md border border-warning/30 bg-warning/8 px-3 py-2 type-meta text-warning"
              role="alert"
            >
              <p className="font-medium">File browser unavailable</p>
              <p className="mt-1">
                This DHI+ server does not expose the host file browser (404).
                The rest of the form still works.
              </p>
            </div>
          ) : null}

          {error !== null ? (
            <p
              className="rounded-md border border-destructive/30 bg-destructive/8 px-3 py-2 type-meta text-destructive"
              role="alert"
            >
              {error}
            </p>
          ) : null}

          {loading ? (
            <p className="type-meta text-muted-foreground">Loading…</p>
          ) : null}

          {!loading && !unavailable && error === null && listing !== null ? (
            entries.length === 0 ? (
              <p className="rounded-md border border-border bg-surface-subtle px-3 py-5 text-center type-meta text-muted-foreground">
                {isFolderMode
                  ? "This folder has no subfolders."
                  : "This folder is empty."}
              </p>
            ) : (
              <ul className="max-h-[46vh] space-y-1 overflow-y-auto rounded-md border border-border p-2">
                {entries.map((entry) => {
                  if (entry.kind === "directory") {
                    return (
                      <li key={entry.path}>
                        <button
                          type="button"
                          className="flex w-full items-center gap-2 rounded-md px-2.5 py-2 text-left type-body text-foreground transition-colors hover:bg-surface-subtle focus:outline-none focus-visible:ring-2 focus-visible:ring-ring"
                          title="Double-click to open"
                          onDoubleClick={() => enterDirectory(entry.path)}
                          onKeyDown={(event) => {
                            if (event.key === "Enter" || event.key === " ") {
                              event.preventDefault();
                              enterDirectory(entry.path);
                            }
                          }}
                        >
                          <FolderOpen className="h-4 w-4 shrink-0 text-muted-foreground" />
                          <span className="min-w-0 flex-1 break-all">
                            {entry.name}
                          </span>
                          <span className="type-meta text-muted-foreground">
                            folder
                          </span>
                        </button>
                      </li>
                    );
                  }

                  const selectable = isSelectableFileBrowserEntry(
                    entry,
                    mode,
                  );
                  const selected = selectedPath === entry.path;
                  return (
                    <li key={entry.path}>
                      {selectable ? (
                        <button
                          type="button"
                          aria-pressed={selected}
                          className={`flex w-full items-center gap-2 rounded-md px-2.5 py-2 text-left type-body transition-colors focus:outline-none focus-visible:ring-2 focus-visible:ring-ring ${
                            selected
                              ? "bg-accent text-accent-foreground"
                              : "text-foreground hover:bg-surface-subtle"
                          }`}
                          onClick={() => setSelectedPath(entry.path)}
                          onDoubleClick={() => {
                            setSelectedPath(entry.path);
                            onSelect(entry.path);
                            onOpenChange(false);
                          }}
                        >
                          {selected ? (
                            <Check className="h-4 w-4 shrink-0" />
                          ) : (
                            <FileText className="h-4 w-4 shrink-0 text-muted-foreground" />
                          )}
                          <span className="min-w-0 flex-1 break-all">
                            {entry.name}
                          </span>
                          {formatFileSize(entry.size) !== "" ? (
                            <span className="type-meta text-muted-foreground">
                              {formatFileSize(entry.size)}
                            </span>
                          ) : null}
                        </button>
                      ) : (
                        <div className="flex items-center gap-2 px-2.5 py-2 type-body text-muted-foreground/70">
                          <FileText className="h-4 w-4 shrink-0" />
                          <span className="min-w-0 flex-1 break-all">
                            {entry.name}
                          </span>
                          {formatFileSize(entry.size) !== "" ? (
                            <span className="type-meta">
                              {formatFileSize(entry.size)}
                            </span>
                          ) : null}
                        </div>
                      )}
                    </li>
                  );
                })}
              </ul>
            )
          ) : null}

          {!loading &&
          !unavailable &&
          error === null &&
          listing !== null &&
          !isFolderMode &&
          selectedPath === null ? (
            <p className="type-meta text-muted-foreground">
              Only an AGENTS.md file can be selected. Double-click a folder to
              enter it.
            </p>
          ) : null}
        </div>

        <DialogFooter>
          <DialogClose asChild>
            <Button type="button" variant="outline">
              Cancel
            </Button>
          </DialogClose>
          <Button
            type="button"
            onClick={selectCurrent}
            disabled={selectDisabled}
          >
            {isFolderMode ? "Select this folder" : "Select AGENTS.md"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
