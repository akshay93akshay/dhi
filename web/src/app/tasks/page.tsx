"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import {
  CircleAlert,
  Loader2,
  Pin,
  Plus,
  RefreshCw,
  Square,
  Trash2,
} from "lucide-react";

import { deleteTask, deleteTasks, listTasks, stopTask } from "@/lib/api";
import type { Task } from "@/lib/types";
import { plainPreview } from "@/lib/preview";
import { describeApiError, shortenId } from "@/lib/ui";
import { formatLocalTimestamp } from "@/lib/localTime";
import { MarkdownMath } from "@/components/markdown-math";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { StateBadge } from "@/components/ui/state-badge";

const ACTIVE_TASK_STATES: ReadonlySet<string> = new Set([
  "created",
  "running",
  "paused",
]);

const TERMINAL_TASK_STATES: ReadonlySet<string> = new Set([
  "done",
  "failed",
  "timeout",
  "cancelled",
]);

function isActiveTask(task: Task): boolean {
  return ACTIVE_TASK_STATES.has(task.state);
}

function isTerminalTask(task: Task): boolean {
  return TERMINAL_TASK_STATES.has(task.state);
}

type BlockedTask = { id: string; state: string; error: string };

function blockedRowsMessage(
  verb: "delete" | "stop",
  blocked: BlockedTask[],
): string {
  const rows = blocked
    .map((row) => `#${shortenId(row.id)} (${row.state}): ${row.error}`)
    .join("; ");
  return `Could not ${verb} ${blocked.length} task${
    blocked.length === 1 ? "" : "s"
  } — ${rows}`;
}

/** Task ledger: the task table that used to sit under the Overview charts. */
export default function TasksPage() {
  const router = useRouter();
  const [tasks, setTasks] = useState<Task[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [stoppingIds, setStoppingIds] = useState<Set<string>>(
    () => new Set(),
  );
  const [selected, setSelected] = useState<Set<string>>(() => new Set());
  const [deleteOpen, setDeleteOpen] = useState(false);
  const [deleteTarget, setDeleteTarget] = useState<Task | null>(null);
  const [deleting, setDeleting] = useState(false);
  const [stopTarget, setStopTarget] = useState<Task | null>(null);
  const mounted = useRef(true);
  const seq = useRef(0);
  const stopTimers = useRef<number[]>([]);

  const load = useCallback(async (): Promise<void> => {
    const current = ++seq.current;
    setRefreshing(true);
    try {
      const next = await listTasks();
      if (mounted.current && current === seq.current) {
        setTasks(next);
        setSelected((currentSelection) => {
          if (currentSelection.size === 0) {
            return currentSelection;
          }
          const selectable = new Set(
            next.filter(isTerminalTask).map((task) => task.id),
          );
          const kept = new Set<string>();
          let changed = false;
          for (const id of currentSelection) {
            if (selectable.has(id)) {
              kept.add(id);
            } else {
              changed = true;
            }
          }
          return changed ? kept : currentSelection;
        });
        setError(null);
      }
    } catch (cause) {
      if (mounted.current && current === seq.current) {
        setError(describeApiError(cause));
      }
    } finally {
      if (mounted.current && current === seq.current) {
        setRefreshing(false);
      }
    }
  }, []);

  useEffect(() => {
    mounted.current = true;
    void load();
    return () => {
      mounted.current = false;
      for (const timer of stopTimers.current) {
        window.clearTimeout(timer);
      }
      stopTimers.current = [];
    };
  }, [load]);

  const terminalTasks = tasks === null ? [] : tasks.filter(isTerminalTask);
  const selectedIds = terminalTasks
    .filter((task) => selected.has(task.id))
    .map((task) => task.id);
  const selectedCount = selectedIds.length;
  const allTerminalSelected =
    terminalTasks.length > 0 && selectedCount === terminalTasks.length;
  const someTerminalSelected = selectedCount > 0 && !allTerminalSelected;
  const deleteCount = deleteTarget !== null ? 1 : selectedCount;
  const rowDeleteBusy = deleteTarget !== null && busyId === deleteTarget.id;
  const stopBusy =
    stopTarget !== null && stoppingIds.has(stopTarget.id);

  function toggleSelected(id: string, checked: boolean): void {
    setSelected((current) => {
      const next = new Set(current);
      if (checked) {
        next.add(id);
      } else {
        next.delete(id);
      }
      return next;
    });
  }

  function selectAllTerminal(checked: boolean): void {
    setSelected(
      checked ? new Set(terminalTasks.map((task) => task.id)) : new Set(),
    );
  }

  const handleDeleteOne = useCallback(
    async (id: string): Promise<void> => {
      setBusyId(id);
      setActionError(null);
      try {
        await deleteTask(id);
        if (!mounted.current) {
          return;
        }
        setDeleteOpen(false);
        setDeleteTarget(null);
        setSelected((current) => {
          const next = new Set(current);
          next.delete(id);
          return next;
        });
        await load();
      } catch (cause) {
        if (mounted.current) {
          setDeleteOpen(false);
          setDeleteTarget(null);
          setActionError(
            `Could not delete task #${shortenId(id)}: ${describeApiError(cause)}`,
          );
        }
      } finally {
        if (mounted.current) {
          setBusyId(null);
        }
      }
    },
    [load],
  );

  const handleDeleteSelected = useCallback(async (): Promise<void> => {
    const ids = selectedIds;
    if (ids.length === 0) {
      return;
    }
    setDeleting(true);
    setActionError(null);
    try {
      const result = await deleteTasks(ids);
      if (!mounted.current) {
        return;
      }
      setDeleteOpen(false);
      setDeleteTarget(null);
      setSelected((current) => {
        const next = new Set(current);
        for (const id of result.deleted) {
          next.delete(id);
        }
        return next;
      });
      await load();
      if (mounted.current && result.blocked.length > 0) {
        setActionError(blockedRowsMessage("delete", result.blocked));
      }
    } catch (cause) {
      if (mounted.current) {
        setDeleteOpen(false);
        setDeleteTarget(null);
        setActionError(
          `Could not delete the selected tasks: ${describeApiError(cause)}`,
        );
      }
    } finally {
      if (mounted.current) {
        setDeleting(false);
      }
    }
  }, [load, selectedIds]);

  const handleStop = useCallback(async (): Promise<void> => {
    if (stopTarget === null) {
      return;
    }
    const id = stopTarget.id;
    setActionError(null);
    setStoppingIds((current) => {
      const next = new Set(current);
      next.add(id);
      return next;
    });
    try {
      await stopTask(id);
      if (!mounted.current) {
        return;
      }
      setStopTarget(null);
      const firstRefresh = window.setTimeout(() => {
        void load();
      }, 1000);
      const secondRefresh = window.setTimeout(() => {
        void load().finally(() => {
          if (mounted.current) {
            setStoppingIds((current) => {
              const next = new Set(current);
              next.delete(id);
              return next;
            });
          }
        });
      }, 3000);
      stopTimers.current.push(firstRefresh, secondRefresh);
    } catch (cause) {
      if (mounted.current) {
        setStoppingIds((current) => {
          const next = new Set(current);
          next.delete(id);
          return next;
        });
        setStopTarget(null);
        setActionError(
          `Could not stop task #${shortenId(id)}: ${describeApiError(cause)}`,
        );
      }
    }
  }, [load, stopTarget]);

  return (
    <main className="space-y-5">
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h1 className="type-page font-semibold tracking-[-0.02em]">Tasks</h1>
          <p className="mt-1 type-meta text-muted-foreground">
            {tasks === null
              ? "Loading the task ledger…"
              : `${tasks.length} task${tasks.length === 1 ? "" : "s"} · open one for detail or work view`}
          </p>
        </div>
        <div className="flex items-center gap-2">
          <Button
            type="button"
            variant="outline"
            size="sm"
            className="h-8 rounded-full"
            onClick={() => void load()}
            disabled={refreshing}
          >
            <RefreshCw
              className={`h-3.5 w-3.5 ${refreshing ? "animate-spin" : ""}`}
            />
            Refresh
          </Button>
          <Button asChild size="sm" className="h-8 rounded-full">
            <Link href="/#new-task">
              <Plus className="h-3.5 w-3.5" />
              New task
            </Link>
          </Button>
        </div>
      </div>

      <Card className="min-w-0" id="tasks">
        <CardHeader className="flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
          <div className="flex items-center gap-2">
            <CardTitle>Task ledger</CardTitle>
            <Badge variant="outline" className="font-normal">
              {tasks === null ? "—" : `${tasks.length} total`}
            </Badge>
          </div>
          <div className="flex flex-wrap items-center gap-2">
            <label className="flex cursor-pointer select-none items-center gap-2 rounded-full px-1 type-table text-muted-foreground">
              <Checkbox
                aria-label="Select all terminal tasks"
                checked={
                  allTerminalSelected
                    ? true
                    : someTerminalSelected
                      ? "indeterminate"
                      : false
                }
                disabled={terminalTasks.length === 0 || deleting}
                onCheckedChange={(checked) =>
                  selectAllTerminal(checked === true)
                }
              />
              Select all
            </label>
            <span className="type-meta text-muted-foreground">
              {selectedCount} selected
            </span>
            <Button
              type="button"
              variant="destructive"
              size="sm"
              className="h-8 rounded-full"
              onClick={() => {
                setActionError(null);
                setDeleteTarget(null);
                setDeleteOpen(true);
              }}
              disabled={selectedCount === 0 || deleting}
            >
              Delete selected ({selectedCount})
            </Button>
          </div>
        </CardHeader>
        <CardContent className="p-0">
          {actionError !== null ? (
            <div
              role="alert"
              className="mx-5 mb-4 mt-4 rounded-lg border border-destructive/30 bg-destructive/8 px-3 py-2 type-meta text-destructive"
            >
              {actionError}
            </div>
          ) : null}

          {error !== null && tasks !== null ? (
            <div className="mx-5 mb-4 rounded-lg border border-destructive/30 bg-destructive/8 px-3 py-2 type-meta text-destructive">
              Could not refresh tasks: {error} — showing the last loaded rows.
            </div>
          ) : null}

          {tasks === null ? (
            <div className="px-5 pb-5">
              {error !== null ? (
                <div className="flex flex-col items-center justify-center gap-3 rounded-xl border border-destructive/25 bg-destructive/5 px-5 py-8 text-center">
                  <CircleAlert className="h-5 w-5 text-destructive" />
                  <p className="max-w-md type-meta text-destructive">
                    Could not load tasks: {error}
                  </p>
                  <Button
                    type="button"
                    variant="outline"
                    size="sm"
                    onClick={() => void load()}
                  >
                    Retry
                  </Button>
                </div>
              ) : (
                <p className="py-6 type-body text-muted-foreground">
                  Loading tasks…
                </p>
              )}
            </div>
          ) : tasks.length === 0 ? (
            <div className="px-5 pb-6 pt-2">
              <div className="rounded-xl border border-dashed border-border bg-surface-subtle px-5 py-8 text-center">
                <p className="type-table font-medium text-foreground">
                  No tasks yet
                </p>
                <p className="mx-auto mt-1 max-w-sm type-meta text-muted-foreground">
                  Create the first task from the Overview start page.
                </p>
              </div>
            </div>
          ) : (
            <div className="overflow-x-auto">
              <table className="w-full min-w-[900px] table-fixed type-table">
                <thead>
                  <tr className="border-y border-border bg-surface-subtle text-left type-label uppercase tracking-[0.09em] text-muted-foreground">
                    <th className="w-[52px] px-5 py-3 font-semibold">
                      <span className="sr-only">Select</span>
                    </th>
                    <th className="w-[100px] whitespace-nowrap px-3 py-3 font-semibold">
                      Task
                    </th>
                    <th className="px-3 py-3 font-semibold">Goal</th>
                    <th className="w-[104px] whitespace-nowrap px-3 py-3 font-semibold">
                      State
                    </th>
                    <th className="w-[104px] whitespace-nowrap px-3 py-3 font-semibold">
                      Pins
                    </th>
                    <th className="w-[150px] whitespace-nowrap px-3 py-3 text-right font-semibold">
                      Created
                    </th>
                    <th className="w-[124px] whitespace-nowrap px-5 py-3 text-right font-semibold">
                      Actions
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {tasks.map((task) => {
                    const terminal = isTerminalTask(task);
                    const active = isActiveTask(task);
                    const stopping = stoppingIds.has(task.id);
                    const rowBusy = busyId === task.id;
                    return (
                      <tr
                        key={task.id}
                        className="cursor-pointer border-b border-border/80 transition-colors last:border-0 hover:bg-muted/70"
                        onClick={() => router.push(`/tasks/${task.id}`)}
                      >
                        <td
                          className="whitespace-nowrap px-5 py-3"
                          onClick={(event) => event.stopPropagation()}
                        >
                          <Checkbox
                            aria-label={
                              terminal
                                ? `Select task ${shortenId(task.id)}`
                                : `Task ${shortenId(task.id)} is not terminal`
                            }
                            checked={terminal && selected.has(task.id)}
                            disabled={!terminal || deleting}
                            title={
                              terminal
                                ? "Select this task for deletion"
                                : "Only terminal tasks can be deleted"
                            }
                            onCheckedChange={(checked) =>
                              toggleSelected(task.id, checked === true)
                            }
                          />
                        </td>
                        <td className="whitespace-nowrap px-3 py-3">
                          <Link
                            href={`/tasks/${task.id}`}
                            onClick={(event) => event.stopPropagation()}
                            className="rounded type-mono text-muted-foreground hover:text-foreground"
                            title={task.id}
                          >
                            #{shortenId(task.id)}
                          </Link>
                        </td>
                        <td className="px-3 py-3">
                          <Link
                            href={`/tasks/${task.id}`}
                            onClick={(event) => event.stopPropagation()}
                            className="line-clamp-2 max-w-xl break-words type-table font-medium text-foreground hover:underline"
                          >
                            {plainPreview(task.goal)}
                          </Link>
                        </td>
                        <td className="whitespace-nowrap px-3 py-3">
                          <StateBadge state={task.state} />
                        </td>
                        <td className="whitespace-nowrap px-3 py-3">
                          {task.pinned_planner_profile_id === null &&
                          task.pinned_worker_profile_id === null ? (
                            <span className="type-meta text-muted-foreground">
                              —
                            </span>
                          ) : (
                            <span className="flex items-center gap-2 type-label text-warning">
                              {task.pinned_planner_profile_id !== null ? (
                                <span
                                  className="inline-flex items-center gap-1"
                                  title={`Planner profile #${task.pinned_planner_profile_id}`}
                                >
                                  <Pin className="h-3.5 w-3.5" /> planner
                                </span>
                              ) : null}
                              {task.pinned_worker_profile_id !== null ? (
                                <span
                                  className="inline-flex items-center gap-1"
                                  title={`Worker profile #${task.pinned_worker_profile_id}`}
                                >
                                  <Pin className="h-3.5 w-3.5" /> worker
                                </span>
                              ) : null}
                            </span>
                          )}
                        </td>
                        <td className="px-3 py-3 text-right type-meta text-muted-foreground">
                          <span className="block break-words">
                            {formatLocalTimestamp(task.created_at)}
                          </span>
                        </td>
                        <td
                          className="whitespace-nowrap px-5 py-3 text-right"
                          onClick={(event) => event.stopPropagation()}
                        >
                          {stopping ? (
                            <Button
                              type="button"
                              variant="outline"
                              size="sm"
                              className="h-8 gap-1.5 px-2.5 type-label"
                              disabled
                            >
                              <Loader2 className="h-3.5 w-3.5 animate-spin" />
                              Stopping…
                            </Button>
                          ) : active ? (
                            <Button
                              type="button"
                              variant="outline"
                              size="sm"
                              className="h-8 gap-1.5 px-2.5 type-label"
                              onClick={() => setStopTarget(task)}
                              disabled={deleting}
                            >
                              <Square className="h-3.5 w-3.5 fill-current" />
                              Stop
                            </Button>
                          ) : terminal ? (
                            <Button
                              type="button"
                              variant="ghost"
                              size="sm"
                              className="h-8 gap-1.5 px-2.5 type-label text-muted-foreground hover:bg-destructive/10 hover:text-destructive"
                              onClick={() => {
                                setActionError(null);
                                setDeleteTarget(task);
                                setDeleteOpen(true);
                              }}
                              disabled={rowBusy || deleting}
                              title={`Delete task #${shortenId(task.id)}`}
                            >
                              <Trash2 className="h-3.5 w-3.5" />
                              Delete
                            </Button>
                          ) : null}
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          )}
        </CardContent>
      </Card>

      <Dialog
        open={deleteOpen}
        onOpenChange={(open) => {
          if (!open && !deleting && !rowDeleteBusy) {
            setDeleteOpen(false);
            setDeleteTarget(null);
          }
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>
              Delete {deleteCount} task{deleteCount === 1 ? "" : "s"}?
            </DialogTitle>
            <DialogDescription>
              The task records and activity will be removed. This cannot be
              undone.
            </DialogDescription>
          </DialogHeader>
          <p className="rounded-md border border-border bg-surface-subtle px-3 py-2 type-meta text-muted-foreground">
            Running and paused tasks are not selected.
          </p>
          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              onClick={() => {
                setDeleteOpen(false);
                setDeleteTarget(null);
              }}
              disabled={deleting || rowDeleteBusy}
            >
              Cancel
            </Button>
            <Button
              type="button"
              variant="destructive"
              onClick={() => {
                if (deleteTarget !== null) {
                  void handleDeleteOne(deleteTarget.id);
                } else {
                  void handleDeleteSelected();
                }
              }}
              disabled={deleting || rowDeleteBusy || deleteCount === 0}
            >
              {deleting || rowDeleteBusy ? "Deleting…" : "Delete"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <Dialog
        open={stopTarget !== null}
        onOpenChange={(open) => {
          if (!open && !stopBusy) {
            setStopTarget(null);
          }
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Stop this task?</DialogTitle>
            <DialogDescription>
              The running work will stop. The task record stays.
            </DialogDescription>
          </DialogHeader>
          {stopTarget !== null ? (
            <div className="rounded-md border border-border bg-surface-subtle px-3 py-2 type-body text-muted-foreground">
              <span className="type-mono">#{shortenId(stopTarget.id)}</span>
              <span className="mx-1.5 text-border">·</span>
              <MarkdownMath text={stopTarget.goal} variant="inline" />
            </div>
          ) : null}
          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              onClick={() => setStopTarget(null)}
              disabled={stopBusy}
            >
              Cancel
            </Button>
            <Button
              type="button"
              variant="destructive"
              onClick={() => void handleStop()}
              disabled={stopBusy}
            >
              {stopBusy ? (
                <>
                  <Loader2 className="h-4 w-4 animate-spin" />
                  Stopping…
                </>
              ) : (
                "Stop task"
              )}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </main>
  );
}
