"use client";

import { useEffect, useState } from "react";
import Link from "next/link";
import { useParams, usePathname } from "next/navigation";
import { ArrowLeft, CircleAlert, GitBranch } from "lucide-react";

import { ApiError, getTaskWork } from "@/lib/api";
import type { TaskWork } from "@/lib/types";
import { describeApiError, shortenId, taskIdFromPathname } from "@/lib/ui";
import { formatLocalTimestamp } from "@/lib/localTime";
import type { CrumbSegment } from "@/lib/nav";
import { Crumbs } from "@/app/_components/Crumbs";
import { TaskGraph } from "@/components/TaskGraph";
import { Button } from "@/components/ui/button";
import { PageHeader } from "@/components/ui/page-header";
import { Skeleton } from "@/components/ui/skeleton";
import { StateBadge } from "@/components/ui/state-badge";

/** Poll cadence for the task-scoped graph; suspended while the tab is hidden. */
const POLL_INTERVAL_MS = 5000;

/**
 * Placeholder height for the loading and error states. The live graph owns its
 * canvas height so the inspector can sit below it without squeezing the
 * drawing; the whole page scrolls once a node is selected.
 */
const GRAPH_PANEL_HEIGHT =
  "min-h-[560px] lg:h-[calc(100svh-260px)] lg:min-h-[600px] lg:max-h-[900px]";

/**
 * Four-stop trail. `crumbTrailSegments` supports only one leaf, so the Work
 * stop (a real link) and the current Graph stop are assembled here.
 */
function graphCrumbs(taskId: string): CrumbSegment[] {
  return [
    { label: "Tasks", href: "/tasks" },
    { label: `#${shortenId(taskId)}`, href: `/tasks/${taskId}` },
    { label: "Work", href: `/tasks/${taskId}/work` },
    { label: "Graph" },
  ];
}

/** Task-scoped graph view: one task's work graph with a click-to-inspect panel. */
export function TaskGraphClient() {
  return <TaskGraphView />;
}

function TaskGraphView() {
  const pathname = usePathname();
  const params = useParams<{ id: string }>();
  const id =
    taskIdFromPathname(pathname) ??
    (typeof params?.id === "string" ? params.id : "");

  const [work, setWork] = useState<TaskWork | null>(null);
  const [pageError, setPageError] = useState<string | null>(null);
  const [pollError, setPollError] = useState<string | null>(null);
  const [notFound, setNotFound] = useState(false);
  const [loading, setLoading] = useState(true);
  const [updatedAt, setUpdatedAt] = useState<Date | null>(null);
  const [retryKey, setRetryKey] = useState(0);

  useEffect(() => {
    if (id === "") {
      return;
    }
    let mounted = true;
    let haveSnapshot = false;
    let requestSeq = 0;

    // A new id (or an explicit retry) starts from a clean loading state so a
    // stale graph never flashes under the new route.
    setWork(null);
    setPageError(null);
    setPollError(null);
    setNotFound(false);
    setUpdatedAt(null);
    setLoading(true);

    const load = async (): Promise<void> => {
      const request = ++requestSeq;
      try {
        const snapshot = await getTaskWork(id);
        if (!mounted || request !== requestSeq) {
          return;
        }
        setWork(snapshot);
        setPageError(null);
        setPollError(null);
        setNotFound(false);
        setUpdatedAt(new Date());
        haveSnapshot = true;
      } catch (err) {
        if (!mounted || request !== requestSeq) {
          return;
        }
        if (err instanceof ApiError && err.isNotFound()) {
          setNotFound(true);
          setWork(null);
          setPageError(null);
          setPollError(null);
          return;
        }
        const message = describeApiError(err);
        // Poll failures keep the last good graph and surface a quiet notice;
        // only a first-load failure becomes the page error.
        if (haveSnapshot) {
          setPollError(message);
        } else {
          setPageError(message);
        }
      } finally {
        if (mounted && request === requestSeq) {
          setLoading(false);
        }
      }
    };

    const timer = setInterval(() => {
      if (
        typeof document !== "undefined" &&
        document.visibilityState !== "visible"
      ) {
        return;
      }
      void load();
    }, POLL_INTERVAL_MS);

    // Refetch immediately when the tab becomes visible again.
    const onVisibility = (): void => {
      if (document.visibilityState === "visible") {
        void load();
      }
    };
    document.addEventListener("visibilitychange", onVisibility);

    void load();

    return () => {
      mounted = false;
      clearInterval(timer);
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [id, retryKey]);

  if (id === "") {
    return (
      <main className="space-y-4">
        <p className="empty">Waiting for the task id…</p>
      </main>
    );
  }

  if (notFound) {
    return (
      <main className="space-y-4">
        <Crumbs trail={graphCrumbs(id)} />
        <PageHeader
          eyebrow="Work graph"
          icon={<GitBranch className="h-3.5 w-3.5" />}
          title="Task not found"
          description={
            <>
              No task with id{" "}
              <span className="mono" title={id}>
                #{shortenId(id)}
              </span>{" "}
              exists, so there is no work graph to show.
            </>
          }
          actions={
            <>
              <Button asChild variant="outline" className="h-9 rounded-full">
                <Link href={`/tasks/${id}/work`}>
                  <ArrowLeft className="h-3.5 w-3.5" /> Back to work
                </Link>
              </Button>
              <Button asChild variant="outline" className="h-9 rounded-full">
                <Link href="/tasks">All tasks</Link>
              </Button>
            </>
          }
        />
        <div className="flex min-h-[240px] flex-col items-center justify-center rounded-xl border border-dashed border-border bg-surface-subtle px-6 py-10 text-center">
          <CircleAlert className="h-6 w-6 text-muted-foreground" />
          <h2 className="type-section mt-3 font-semibold">No work graph for this task</h2>
          <p className="type-meta mt-1 max-w-md text-muted-foreground">
            The id may belong to a different database, or the task may have been
            removed.
          </p>
        </div>
      </main>
    );
  }

  if (loading) {
    return (
      <main className="space-y-4">
        <Crumbs trail={graphCrumbs(id)} />
        <div className="space-y-2">
          <Skeleton className="h-3.5 w-24" />
          <Skeleton className="h-7 w-44" />
          <Skeleton className="h-5 w-64" />
        </div>
        <Skeleton className={`${GRAPH_PANEL_HEIGHT} w-full rounded-xl`} />
      </main>
    );
  }

  if (work === null) {
    return (
      <main className="space-y-4">
        <Crumbs trail={graphCrumbs(id)} />
        <PageHeader
          eyebrow="Work graph"
          icon={<GitBranch className="h-3.5 w-3.5" />}
          title="Task graph"
          description={
            <span className="mono" title={id}>
              #{shortenId(id)}
            </span>
          }
          actions={
            <>
              <Button asChild variant="outline" className="h-9 rounded-full">
                <Link href={`/tasks/${id}/work`}>
                  <ArrowLeft className="h-3.5 w-3.5" /> Back to work
                </Link>
              </Button>
              <Button asChild variant="outline" className="h-9 rounded-full">
                <Link href="/tasks">All tasks</Link>
              </Button>
            </>
          }
        />
        <div
          className={`${GRAPH_PANEL_HEIGHT} flex flex-col items-center justify-center rounded-xl border border-border bg-card px-6 py-10 text-center`}
        >
          <CircleAlert className="h-6 w-6 text-destructive" />
          <h2 className="type-section mt-3 font-semibold">Could not load task graph</h2>
          <p
            className="type-meta mt-1 max-w-lg break-words text-muted-foreground"
            role="alert"
          >
            {pageError ?? "The work graph request failed."}
          </p>
          <div className="mt-4 flex flex-wrap items-center justify-center gap-2">
            <Button
              type="button"
              className="h-9 rounded-full"
              onClick={() => setRetryKey((current) => current + 1)}
            >
              Retry
            </Button>
            <Button asChild variant="outline" className="h-9 rounded-full">
              <Link href={`/tasks/${id}/work`}>
                <ArrowLeft className="h-3.5 w-3.5" /> Back to work
              </Link>
            </Button>
          </div>
        </div>
      </main>
    );
  }

  return (
    <main className="space-y-4">
      <Crumbs trail={graphCrumbs(work.task.id)} />

      <PageHeader
        eyebrow="Work graph"
        icon={<GitBranch className="h-3.5 w-3.5" />}
        title="Task graph"
        description={
          <span className="flex flex-wrap items-center gap-2">
            <StateBadge state={work.task.state} />
            <span className="mono" title={work.task.id}>
              #{shortenId(work.task.id)}
            </span>
            {updatedAt !== null ? (
              <span className="type-meta text-muted-foreground">
                updated {formatLocalTimestamp(updatedAt.toISOString())}
              </span>
            ) : null}
          </span>
        }
        actions={
          <>
            <Button asChild variant="outline" className="h-9 rounded-full">
              <Link href={`/tasks/${work.task.id}/work`}>
                <ArrowLeft className="h-3.5 w-3.5" /> Back to work
              </Link>
            </Button>
            <Button asChild variant="outline" className="h-9 rounded-full">
              <Link href={`/tasks/${work.task.id}`}>Task detail</Link>
            </Button>
          </>
        }
      />

      {pollError !== null ? (
        <p
          className="type-meta text-warning"
          role="status"
        >
          Auto-refresh failed: {pollError} — showing the last snapshot and
          retrying every {POLL_INTERVAL_MS / 1000} s.
        </p>
      ) : null}

      <TaskGraph key={work.task.id} work={work} />
    </main>
  );
}
