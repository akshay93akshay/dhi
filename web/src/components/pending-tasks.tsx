"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import Link from "next/link";
import { ArrowRight } from "lucide-react";

import { listTasks, stopTasks } from "@/lib/api";
import type { StopTasksResponse, Task, TaskState } from "@/lib/types";
import { plainPreview } from "@/lib/preview";
import { describeApiError, shortenId } from "@/lib/ui";
import { formatLocalTimestamp } from "@/lib/localTime";
import { Button } from "@/components/ui/button";
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

const PENDING_POLL_MS = 15_000;
const MAX_PENDING_ROWS = 5;
const STOP_REFRESH_DELAYS_MS = [1_000, 3_000] as const;

const PENDING_TASK_STATES: ReadonlySet<TaskState> = new Set([
  "created",
  "running",
  "paused",
]);

/** Compact age for a task row; the full local timestamp stays in the title. */
function ageLabel(iso: string, nowMs: number): string {
  const createdAt = Date.parse(iso);
  if (Number.isNaN(createdAt)) {
    return "—";
  }
  const seconds = Math.max(0, Math.floor((nowMs - createdAt) / 1_000));
  if (seconds < 60) {
    return "just now";
  }
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) {
    return `${minutes}m ago`;
  }
  const hours = Math.floor(minutes / 60);
  if (hours < 24) {
    return `${hours}h ago`;
  }
  const days = Math.floor(hours / 24);
  if (days < 7) {
    return `${days}d ago`;
  }
  if (days < 30) {
    return `${Math.floor(days / 7)}w ago`;
  }
  if (days < 365) {
    return `${Math.floor(days / 30)}mo ago`;
  }
  return `${Math.floor(days / 365)}y ago`;
}

/**
 * Overview pending-task strip: the five newest `created | running | paused`
 * tasks with a stop-only bulk action. Delete lives on the task pages; records
 * stay when work stops.
 */
export function PendingTasks() {
  const [tasks, setTasks] = useState<Task[] | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [nowMs, setNowMs] = useState(() => Date.now());
  const [selected, setSelected] = useState<ReadonlySet<string>>(
    () => new Set<string>(),
  );
  const [stopping, setStopping] = useState(false);
  const [stopError, setStopError] = useState<string | null>(null);
  const [blocked, setBlocked] = useState<StopTasksResponse["blocked"]>([]);
  const [confirmOpen, setConfirmOpen] = useState(false);

  const mounted = useRef(true);
  const seq = useRef(0);
  const refreshTimers = useRef<number[]>([]);

  const load = useCallback(async (): Promise<void> => {
    const current = ++seq.current;
    try {
      const next = await listTasks();
      if (mounted.current && current === seq.current) {
        setTasks(next);
        setLoadError(null);
        setNowMs(Date.now());
      }
    } catch (cause) {
      if (mounted.current && current === seq.current) {
        setLoadError(describeApiError(cause));
      }
    }
  }, []);

  useEffect(() => {
    mounted.current = true;
    void load();

    const poll = window.setInterval(() => {
      if (document.visibilityState === "visible") {
        void load();
      }
    }, PENDING_POLL_MS);

    const onVisible = (): void => {
      if (document.visibilityState === "visible") {
        void load();
      }
    };
    document.addEventListener("visibilitychange", onVisible);

    return () => {
      mounted.current = false;
      window.clearInterval(poll);
      document.removeEventListener("visibilitychange", onVisible);
      for (const timer of refreshTimers.current) {
        window.clearTimeout(timer);
      }
      refreshTimers.current = [];
    };
  }, [load]);

  const pending = useMemo(
    () =>
      (tasks ?? []).filter((task) => PENDING_TASK_STATES.has(task.state)),
    [tasks],
  );
  const visible = pending.slice(0, MAX_PENDING_ROWS);
  const selectedPending = pending.filter((task) => selected.has(task.id));
  const selectedCount = selectedPending.length;
  const allSelected =
    pending.length > 0 && pending.every((task) => selected.has(task.id));
  const someSelected = pending.some((task) => selected.has(task.id));
  const visibleIds = new Set(visible.map((task) => task.id));
  const hiddenBlocked = blocked.filter((item) => !visibleIds.has(item.id));

  // Drop selections for tasks that stopped, finished, or otherwise left the
  // pending set while the section was open.
  useEffect(() => {
    if (tasks === null) {
      return;
    }
    const pendingIds = new Set(
      tasks
        .filter((task) => PENDING_TASK_STATES.has(task.state))
        .map((task) => task.id),
    );
    setSelected((current) => {
      let changed = false;
      const next = new Set<string>();
      for (const id of current) {
        if (pendingIds.has(id)) {
          next.add(id);
        } else {
          changed = true;
        }
      }
      return changed ? next : current;
    });
  }, [tasks]);

  useEffect(() => {
    if (confirmOpen && selectedCount === 0) {
      setConfirmOpen(false);
    }
  }, [confirmOpen, selectedCount]);

  const toggleSelected = (id: string, checked: boolean): void => {
    setSelected((current) => {
      const next = new Set(current);
      if (checked) {
        next.add(id);
      } else {
        next.delete(id);
      }
      return next;
    });
  };

  const toggleAll = (): void => {
    setSelected(
      allSelected ? new Set<string>() : new Set(pending.map((task) => task.id)),
    );
  };

  const scheduleStopRefresh = (): void => {
    for (const timer of refreshTimers.current) {
      window.clearTimeout(timer);
    }
    refreshTimers.current = STOP_REFRESH_DELAYS_MS.map((delay) =>
      window.setTimeout(() => {
        void load();
      }, delay),
    );
    refreshTimers.current.push(
      window.setTimeout(() => {
        if (mounted.current) {
          setStopping(false);
        }
      }, STOP_REFRESH_DELAYS_MS[STOP_REFRESH_DELAYS_MS.length - 1]),
    );
  };

  const requestStop = async (): Promise<void> => {
    if (stopping || selectedCount === 0) {
      return;
    }
    const ids = selectedPending.map((task) => task.id);
    setConfirmOpen(false);
    setStopping(true);
    setStopError(null);
    setBlocked([]);
    try {
      const result = await stopTasks(ids);
      if (!mounted.current) {
        return;
      }
      setBlocked(result.blocked);
      setSelected((current) => {
        const next = new Set(current);
        for (const id of result.stopping) {
          next.delete(id);
        }
        return next;
      });
      scheduleStopRefresh();
    } catch (cause) {
      if (mounted.current) {
        setStopError(describeApiError(cause));
        setStopping(false);
      }
    }
  };

  return (
    <section
      aria-labelledby="pending-tasks-heading"
      className="w-full border-t border-border pt-5 text-left"
    >
      <div className="flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1">
        <div className="flex items-baseline gap-2">
          <h2
            id="pending-tasks-heading"
            className="type-section font-semibold tracking-[-0.01em] text-foreground"
          >
            Pending tasks
          </h2>
          <span className="type-meta tabular-nums text-muted-foreground">
            {tasks === null ? "—" : pending.length}
          </span>
        </div>
        <Link
          href="/tasks"
          className="inline-flex items-center gap-1 type-meta font-medium text-muted-foreground transition-colors hover:text-foreground"
        >
          View all tasks <span aria-hidden="true">→</span>
        </Link>
      </div>

      {tasks === null ? (
        loadError === null ? (
          <p className="mt-3 type-meta text-muted-foreground">
            Loading pending tasks…
          </p>
        ) : (
          <div className="mt-3 flex flex-wrap items-center gap-3">
            <p className="break-words type-meta text-destructive">
              Could not load pending tasks: {loadError}
            </p>
            <Button
              type="button"
              variant="outline"
              size="sm"
              className="h-7 rounded-full px-2.5"
              onClick={() => void load()}
            >
              Retry
            </Button>
          </div>
        )
      ) : pending.length === 0 ? (
        <>
          {loadError !== null ? (
            <RefreshWarning
              message={loadError}
              onRetry={() => void load()}
            />
          ) : null}
          <p className="mt-3 type-meta text-muted-foreground">
            No pending tasks. Start one above.
          </p>
        </>
      ) : (
        <>
          <div className="mt-3 flex flex-wrap items-center gap-x-4 gap-y-2">
            <label className="inline-flex cursor-pointer items-center gap-2 type-table font-medium text-foreground">
              <Checkbox
                checked={
                  allSelected ? true : someSelected ? "indeterminate" : false
                }
                disabled={stopping}
                aria-label="Select all pending tasks"
                onCheckedChange={toggleAll}
              />
              Select all
            </label>
            <Button
              type="button"
              variant="outline"
              size="sm"
              className="h-8 rounded-full border-destructive/30 px-3 text-destructive hover:bg-destructive/8 hover:text-destructive"
              disabled={selectedCount === 0 || stopping}
              onClick={() => setConfirmOpen(true)}
            >
              {stopping ? "Stopping…" : "Stop selected"}
            </Button>
            <p className="w-full type-meta text-muted-foreground sm:w-auto">
              Select tasks to stop them. The task records stay.
            </p>
          </div>

          {stopError !== null ? (
            <p
              className="mt-2 break-words type-meta text-destructive"
              role="alert"
            >
              Could not stop tasks: {stopError}
            </p>
          ) : null}

          {loadError !== null ? (
            <RefreshWarning
              message={loadError}
              onRetry={() => void load()}
            />
          ) : null}

          <ul className="mt-2 divide-y divide-border/80 border-t border-border/80">
            {visible.map((task) => {
              const rowBlocked = blocked.find((item) => item.id === task.id);
              const goalPreview = plainPreview(task.goal);
              return (
                <li key={task.id} className="py-3">
                  <div className="flex items-start gap-2.5">
                    <Checkbox
                      className="mt-1"
                      checked={selected.has(task.id)}
                      disabled={stopping}
                      aria-label={`Select task ${shortenId(task.id)}: ${goalPreview}`}
                      onCheckedChange={(checked) =>
                        toggleSelected(task.id, checked === true)
                      }
                    />
                    <div className="flex min-w-0 flex-1 flex-wrap items-center gap-x-2.5 gap-y-1">
                      <StateBadge state={task.state} className="shrink-0" />
                      <Link
                        href={`/tasks/${task.id}`}
                        title={goalPreview}
                        className="order-last w-full truncate type-table font-medium text-foreground hover:underline sm:order-none sm:w-auto sm:flex-1"
                      >
                        {goalPreview}
                      </Link>
                      <span
                        className="ml-auto shrink-0 type-meta tabular-nums text-muted-foreground sm:ml-0"
                        title={`Created ${formatLocalTimestamp(task.created_at)}`}
                      >
                        {ageLabel(task.created_at, nowMs)}
                      </span>
                      <Link
                        href={`/tasks/${task.id}`}
                        aria-label={`Open task ${shortenId(task.id)}`}
                        className="inline-flex shrink-0 items-center gap-1 type-label font-medium text-muted-foreground transition-colors hover:text-foreground"
                      >
                        Open <ArrowRight className="h-3.5 w-3.5" />
                      </Link>
                    </div>
                  </div>
                  {rowBlocked !== undefined ? (
                    <p className="ml-6 mt-1 break-words type-meta text-destructive">
                      {rowBlocked.state}: {rowBlocked.error}
                    </p>
                  ) : null}
                </li>
              );
            })}
          </ul>

          {hiddenBlocked.length > 0 ? (
            <div className="mt-2 space-y-1" role="alert">
              {hiddenBlocked.map((item) => (
                <p
                  key={item.id}
                  className="break-words type-meta text-destructive"
                >
                  Task {shortenId(item.id)} ({item.state}): {item.error}
                </p>
              ))}
            </div>
          ) : null}
        </>
      )}

      <Dialog open={confirmOpen} onOpenChange={setConfirmOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>
              Stop {selectedCount} task{selectedCount === 1 ? "" : "s"}?
            </DialogTitle>
            <DialogDescription>
              The running work will stop. The task records stay.
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              onClick={() => setConfirmOpen(false)}
            >
              Cancel
            </Button>
            <Button
              type="button"
              variant="destructive"
              onClick={() => void requestStop()}
            >
              Stop task{selectedCount === 1 ? "" : "s"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </section>
  );
}

function RefreshWarning(props: {
  message: string;
  onRetry: () => void;
}) {
  return (
    <div className="mt-2 flex flex-wrap items-center gap-x-2 gap-y-1 type-meta text-warning">
      <span className="break-words">
        Could not refresh pending tasks: {props.message}
      </span>
      <button
        type="button"
        className="font-medium underline underline-offset-2 hover:text-foreground"
        onClick={props.onRetry}
      >
        Retry
      </button>
    </div>
  );
}
