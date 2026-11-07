"use client";

import {
  type FormEvent,
  useCallback,
  useEffect,
  useRef,
  useState,
} from "react";
import Link from "next/link";
import { useParams, usePathname } from "next/navigation";
import {
  ApiError,
  addGoal,
  getTask,
  getTaskActivity,
  getTaskUsage,
} from "@/lib/api";
import type { Activity, AgentRun, Goal, Task } from "@/lib/types";
import {
  canAddGoal,
  validateGoalDescription,
  ADD_GOAL_MAX_CHARS,
} from "@/lib/addGoal";
import {
  describeApiError,
  shortenId,
  taskIdFromPathname,
  type ServiceState,
} from "@/lib/ui";
import { formatLocalTimestamp } from "@/lib/localTime";
import {
  DhiBrandStatus,
  type BrandActivity,
} from "@/components/dhi-brand-status";
import { RecentActivityFeed } from "@/app/_components/ActivityLog";
import { SideQuestions } from "./SideQuestions";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { CopyButton } from "@/components/ui/copy-button";
import { Skeleton } from "@/components/ui/skeleton";
import { StateBadge } from "@/components/ui/state-badge";
import { Textarea } from "@/components/ui/textarea";
import {
  ArrowLeft,
  ArrowRight,
  Check,
  ChevronRight,
  Pin,
  RefreshCw,
} from "lucide-react";

/** Snapshot of one load: the task, its completion-call runs, and its activity. */
type DetailData = {
  task: Task;
  runs: AgentRun[];
  runsError: string | null;
  activity: Activity[] | null;
  activityError: string | null;
};

const COLUMN_CLASS = "mx-auto w-full max-w-[760px] px-1 pb-16";
const TITLE_CLASS =
  "whitespace-pre-wrap break-words text-[28px] font-medium leading-[1.2] tracking-[-0.025em] text-foreground sm:text-[30px]";

const DISCLOSURE_SUMMARY =
  "flex cursor-pointer select-none list-none items-center gap-1.5 text-[13px] text-muted-foreground transition-colors hover:text-foreground [&::-webkit-details-marker]:hidden";
const DISCLOSURE_CHEVRON =
  "h-3.5 w-3.5 shrink-0 transition-transform duration-150 group-open:rotate-90";

/** Centered brand block; the logo itself carries the top of the reading view. */
function BrandMasthead(props: {
  state: ServiceState;
  activity: BrandActivity;
}) {
  return (
    <div className="flex justify-center pt-2">
      <DhiBrandStatus
        state={props.state}
        detail
        interactive
        activity={props.activity}
      />
    </div>
  );
}

/**
 * Task detail: a single-task reading view. The original goal leads;
 * activity, facts, and completion calls are collapsed reference material.
 * Refreshed manually; the live view is the work page.
 */
export function TaskDetailClient() {
  return <TaskDetail />;
}

function TaskDetail() {
  const pathname = usePathname();
  const params = useParams<{ id: string }>();
  const id =
    taskIdFromPathname(pathname) ??
    (typeof params?.id === "string" ? params.id : "");

  const [data, setData] = useState<DetailData | null>(null);
  const [pageError, setPageError] = useState<string | null>(null);
  const [notFound, setNotFound] = useState(false);
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);
  const [goalDescription, setGoalDescription] = useState("");
  const [goalPending, setGoalPending] = useState(false);
  const [goalError, setGoalError] = useState<string | null>(null);
  const [goalNotice, setGoalNotice] = useState<string | null>(null);
  const [addedGoal, setAddedGoal] = useState<Goal | null>(null);
  const [activity, setActivity] = useState<BrandActivity>("idle");
  const activityTimer = useRef<number | null>(null);

  const clearActivityTimer = useCallback((): void => {
    if (activityTimer.current !== null) {
      window.clearTimeout(activityTimer.current);
      activityTimer.current = null;
    }
  }, []);

  const flashActivity = useCallback(
    (next: BrandActivity, durationMs: number): void => {
      clearActivityTimer();
      setActivity(next);
      activityTimer.current = window.setTimeout(() => {
        activityTimer.current = null;
        setActivity("idle");
      }, durationMs);
    },
    [clearActivityTimer],
  );

  useEffect(() => clearActivityTimer, [clearActivityTimer]);

  const load = useCallback(async () => {
    if (id === "") {
      return;
    }
    setPageError(null);
    setNotFound(false);
    setRefreshing(true);
    clearActivityTimer();
    setActivity("submit");
    try {
      // The task drives the page; usage/activity failures degrade to notes
      // so an errored side route can't hide the task itself.
      const [taskResult, runsResult, activityResult] = await Promise.allSettled(
        [getTask(id), getTaskUsage(id), getTaskActivity(id)],
      );
      if (taskResult.status === "rejected") {
        const err = taskResult.reason;
        if (err instanceof ApiError && err.isNotFound()) {
          setNotFound(true);
          setData(null);
          return;
        }
        setPageError(describeApiError(err));
        return;
      }
      setData({
        task: taskResult.value,
        runs: runsResult.status === "fulfilled" ? runsResult.value : [],
        runsError:
          runsResult.status === "fulfilled"
            ? null
            : describeApiError(runsResult.reason),
        activity:
          activityResult.status === "fulfilled" ? activityResult.value : null,
        activityError:
          activityResult.status === "fulfilled"
            ? null
            : describeApiError(activityResult.reason),
      });
    } finally {
      setLoading(false);
      setRefreshing(false);
      setActivity("idle");
    }
  }, [clearActivityTimer, id]);

  useEffect(() => {
    void load();
  }, [load]);

  if (id === "") {
    return (
      <main className={COLUMN_CLASS}>
        <p className="empty">Waiting for the task id…</p>
      </main>
    );
  }

  if (notFound) {
    return (
      <main className={COLUMN_CLASS}>
        <BrandMasthead state="error" activity="idle" />
        <div className="mt-10">
          <h1 className={TITLE_CLASS}>Task not found</h1>
          <p className="mt-3 max-w-[52ch] text-sm leading-6 text-muted-foreground">
            The id may have been created against a different database or
            removed.
          </p>
          <p className="mt-2 text-sm text-muted-foreground">
            No task with id <code className="font-mono">{id}</code> exists.
          </p>
          <Button asChild variant="outline" className="mt-6">
            <Link href="/">
              <ArrowLeft className="h-4 w-4" /> Back to overview
            </Link>
          </Button>
        </div>
      </main>
    );
  }

  if (pageError !== null && data === null) {
    return (
      <main className={COLUMN_CLASS}>
        <BrandMasthead state="error" activity="idle" />
        <div className="mt-10">
          <h1 className={TITLE_CLASS}>Could not load task</h1>
          <p
            className="mt-3 border-l-2 border-destructive pl-3 text-sm leading-6 text-destructive"
            role="alert"
          >
            {pageError}
          </p>
          <div className="mt-6 flex flex-wrap items-center gap-2">
            <Button
              type="button"
              onClick={() => void load()}
              disabled={refreshing}
            >
              {refreshing ? "Retrying…" : "Retry"}
            </Button>
            <Button asChild variant="outline">
              <Link href="/">
                <ArrowLeft className="h-4 w-4" /> Overview
              </Link>
            </Button>
          </div>
        </div>
      </main>
    );
  }

  if (loading || data === null) {
    return (
      <main className={COLUMN_CLASS}>
        <BrandMasthead state="loading" activity="submit" />
        <div className="mt-10 space-y-4">
          <Skeleton className="h-10 w-4/5" />
          <Skeleton className="h-4 w-2/3" />
          <Skeleton className="h-32 w-full rounded-[14px]" />
        </div>
      </main>
    );
  }

  const { task, runs, runsError, activity: activityRows, activityError } = data;

  const visualState =
    task.state === "done"
      ? "healthy"
      : task.state === "failed" || task.state === "timeout"
        ? "error"
        : "loading";

  const submitGoal = async (
    event: FormEvent<HTMLFormElement>,
  ): Promise<void> => {
    event.preventDefault();
    if (goalPending) {
      return;
    }
    const validation = validateGoalDescription(goalDescription);
    if (validation !== null) {
      setGoalError(validation);
      flashActivity("error", 2600);
      return;
    }

    setGoalPending(true);
    setGoalError(null);
    setGoalNotice(null);
    clearActivityTimer();
    setActivity("submit");
    try {
      const response = await addGoal(task.id, goalDescription.trim());
      setAddedGoal(response.goal);
      setGoalDescription("");
      // The task just became `running`; refresh the whole detail snapshot so
      // the badge, timestamps, and activity reflect the reopen.
      await load();
      flashActivity("success", 1600);
    } catch (err) {
      if (err instanceof ApiError && err.isConflict()) {
        // The task is no longer terminal (another human or the engine moved
        // it). Show the server message and refresh; the panel disappears.
        setGoalNotice(`Could not add goal: ${describeApiError(err)}`);
        await load();
      } else {
        // `400` and every other failure stay inline, with the text kept.
        setGoalError(describeApiError(err));
      }
      flashActivity("error", 2600);
    } finally {
      setGoalPending(false);
    }
  };

  return (
    <main className={COLUMN_CLASS}>
      <BrandMasthead state={visualState} activity={activity} />

      <header className="mt-10">
        <h1 className={TITLE_CLASS}>{task.goal}</h1>
        <div className="mt-3 flex flex-wrap items-center gap-x-2 gap-y-1.5 text-[13px] text-muted-foreground">
          <span className="font-mono text-[12px]">
            Task #{shortenId(task.id)}
          </span>
          <span aria-hidden="true">·</span>
          <StateBadge state={task.state} />
          <span aria-hidden="true">·</span>
          <span>updated {formatLocalTimestamp(task.updated_at)}</span>
        </div>
        <div className="mt-5 flex flex-wrap items-center gap-2">
          <Button asChild size="sm" className="rounded-full">
            <Link href={`/tasks/${task.id}/work`}>
              Open work view <ArrowRight className="h-3.5 w-3.5" />
            </Link>
          </Button>
          <Button
            type="button"
            size="sm"
            variant="ghost"
            className="rounded-full text-muted-foreground"
            onClick={() => void load()}
            disabled={refreshing}
          >
            <RefreshCw
              className={`h-3.5 w-3.5 ${refreshing ? "animate-spin" : ""}`}
            />
            {refreshing ? "Refreshing…" : "Refresh"}
          </Button>
        </div>
      </header>

      {pageError !== null ? (
        <p
          className="mt-6 border-l-2 border-destructive pl-3 text-[13px] leading-6 text-destructive"
          role="alert"
        >
          Refresh failed: {pageError}
        </p>
      ) : null}

      {goalNotice !== null ? (
        <p className="mt-6 text-[13px] leading-6 text-muted-foreground">
          {goalNotice}
        </p>
      ) : null}

      {addedGoal !== null ? (
        <p className="mt-6 flex items-start gap-2 text-[13px] leading-6 text-success">
          <Check className="mt-1 h-3.5 w-3.5 shrink-0" />
          <span>
            Added goal:{" "}
            <strong className="font-medium">{addedGoal.description}</strong>
          </span>
        </p>
      ) : null}

      <ThinkingTranscript runs={runs} />

      <details className="group mt-7">
        <summary className={DISCLOSURE_SUMMARY}>
          Recent activity
          <ChevronRight className={DISCLOSURE_CHEVRON} />
        </summary>
        <div className="mt-4 [&_[data-slot=card]]:rounded-none [&_[data-slot=card]]:border-0 [&_[data-slot=card]]:bg-transparent [&_[data-slot=card]]:shadow-none [&_[data-slot=card-header]]:hidden [&_[data-slot=card-content]]:p-0">
          <p className="mb-3 text-[11px] text-muted-foreground">
            Latest events — newest last.
          </p>
          <RecentActivityFeed
            taskId={task.id}
            rows={activityRows}
            error={activityError}
          />
        </div>
      </details>

      {canAddGoal(task.state) ? (
        <section aria-labelledby="add-goal-heading" className="mt-10">
          <h2
            id="add-goal-heading"
            className="text-[15px] font-semibold tracking-tight text-foreground"
          >
            Add a goal
          </h2>
          <p className="mt-1 text-[13px] leading-6 text-muted-foreground">
            This reopens the task and is logged as a human action.
          </p>

          {goalError !== null ? (
            <p
              className="mt-3 border-l-2 border-destructive pl-3 text-[13px] leading-5 text-destructive"
              role="alert"
            >
              {goalError}
            </p>
          ) : null}

          <form onSubmit={submitGoal} className="mt-4">
            <label htmlFor="add-goal" className="sr-only">
              New goal
            </label>
            <Textarea
              id="add-goal"
              value={goalDescription}
              onChange={(event) => {
                clearActivityTimer();
                setGoalDescription(event.target.value);
                setActivity("typing");
              }}
              onFocus={() => {
                clearActivityTimer();
                setActivity("focus-goal");
              }}
              onBlur={() => {
                if (activity === "focus-goal" || activity === "typing") {
                  setActivity("idle");
                }
              }}
              maxLength={ADD_GOAL_MAX_CHARS}
              rows={3}
              placeholder="Describe the new goal…"
              disabled={goalPending}
            />
            <div className="mt-3 flex flex-wrap items-center gap-3">
              <Button type="submit" size="sm" disabled={goalPending}>
                {goalPending ? "Adding…" : "Add goal and reopen"}
              </Button>
              <span className="text-[11px] tabular-nums text-muted-foreground">
                {goalDescription.trim().length}/{ADD_GOAL_MAX_CHARS}
              </span>
            </div>
          </form>
        </section>
      ) : null}

      <details className="group mt-6">
        <summary className={DISCLOSURE_SUMMARY}>
          Task facts
          <ChevronRight className={DISCLOSURE_CHEVRON} />
        </summary>
        <dl className="mt-4 grid grid-cols-[minmax(96px,140px)_minmax(0,1fr)] gap-x-6 gap-y-2.5">
          <dt className="text-xs leading-5 text-muted-foreground">Task id</dt>
          <dd className="m-0 flex min-w-0 items-center gap-1">
            <span className="truncate font-mono text-[12px]" title={task.id}>
              {task.id}
            </span>
            <CopyButton text={task.id} label="task id" size="icon" />
          </dd>

          <dt className="text-xs leading-5 text-muted-foreground">
            Exploration
          </dt>
          <dd className="m-0 min-w-0">
            <span
              className="truncate font-mono text-[12px]"
              title={task.exploration_id}
            >
              {shortenId(task.exploration_id)}
            </span>
          </dd>

          <dt className="text-xs leading-5 text-muted-foreground">
            Origin fact
          </dt>
          <dd className="m-0 min-w-0">
            <span
              className="truncate font-mono text-[12px]"
              title={task.origin_fact_id}
            >
              {shortenId(task.origin_fact_id)}
            </span>
          </dd>

          <dt className="text-xs leading-5 text-muted-foreground">
            Planner pin
          </dt>
          <dd className="m-0">
            {task.pinned_planner_profile_id !== null ? (
              <Badge variant="warning">
                <Pin className="h-3 w-3" /> #{task.pinned_planner_profile_id}
              </Badge>
            ) : (
              <span className="text-xs text-muted-foreground">
                pool default
              </span>
            )}
          </dd>

          <dt className="text-xs leading-5 text-muted-foreground">
            Worker pin
          </dt>
          <dd className="m-0">
            {task.pinned_worker_profile_id !== null ? (
              <Badge variant="warning">
                <Pin className="h-3 w-3" /> #{task.pinned_worker_profile_id}
              </Badge>
            ) : (
              <span className="text-xs text-muted-foreground">
                pool default
              </span>
            )}
          </dd>

          <dt className="text-xs leading-5 text-muted-foreground">Created</dt>
          <dd className="m-0 text-xs leading-5 text-muted-foreground">
            {formatLocalTimestamp(task.created_at)}
          </dd>

          <dt className="text-xs leading-5 text-muted-foreground">Updated</dt>
          <dd className="m-0 text-xs leading-5 text-muted-foreground">
            {formatLocalTimestamp(task.updated_at)}
          </dd>
        </dl>
      </details>

      <details className="group mt-6">
        <summary className={DISCLOSURE_SUMMARY}>
          Completion calls
          {runs.length > 0 ? (
            <span className="text-muted-foreground/70">· {runs.length}</span>
          ) : null}
          <ChevronRight className={DISCLOSURE_CHEVRON} />
        </summary>
        <div className="mt-4">
          <UsageCard runs={runs} runsError={runsError} />
        </div>
      </details>

      <div className="mt-12">
        <SideQuestions taskId={task.id} />
      </div>
    </main>
  );
}

// ---------------------------------------------------------------------------
// Thinking transcript
// ---------------------------------------------------------------------------

/**
 * Provider-returned thinking text, grouped by completion call. The section is
 * absent until at least one run carries non-empty thinking; it stays separate
 * from the Completion calls table so thinking never masquerades as response
 * content. Rendering is plain text inside a scroll-capped `<pre>` — no
 * markdown, no graph, collapsed by default.
 */
function ThinkingTranscript(props: { runs: AgentRun[] }) {
  const entries = props.runs.filter(
    (run) => (run.thinking ?? "").trim() !== "",
  );
  if (entries.length === 0) {
    return null;
  }
  return (
    <details className="group mt-7">
      <summary className={DISCLOSURE_SUMMARY}>
        {`Thinking · ${entries.length} ${entries.length === 1 ? "call" : "calls"}`}
        <ChevronRight className={DISCLOSURE_CHEVRON} />
      </summary>
      <div className="mt-4 border-y border-border/70">
        {entries.map((run) => (
          <article
            key={run.id}
            className="border-b border-border/70 py-4 last:border-0"
          >
            <div className="flex flex-wrap items-center gap-x-2 gap-y-1 text-[11px] text-muted-foreground">
              <span className="font-medium text-foreground">{run.role}</span>
              <span aria-hidden="true">·</span>
              <span className="font-mono">{run.model}</span>
              <span aria-hidden="true">·</span>
              <span>{formatLocalTimestamp(run.created_at)}</span>
              <span aria-hidden="true">·</span>
              <span className="tabular-nums">
                {run.reasoning_tokens} reasoning tokens
              </span>
            </div>
            <pre className="mt-3 max-h-[420px] overflow-auto whitespace-pre-wrap break-words rounded-[10px] bg-surface-subtle px-4 py-3 font-mono text-[12px] leading-6 text-foreground [overflow-wrap:anywhere]">{run.thinking}</pre>
          </article>
        ))}
      </div>
    </details>
  );
}

// ---------------------------------------------------------------------------
// Usage (completion calls)
// ---------------------------------------------------------------------------

function UsageCard(props: { runs: AgentRun[]; runsError: string | null }) {
  const { runs, runsError } = props;

  return (
    <section aria-label="Completion calls">
      <p className="text-[11px] leading-5 text-muted-foreground">
        Persisted agent runs with tokens, cache, cost, and latency.
      </p>

      {runsError !== null ? (
        <p
          className="mt-3 border-l-2 border-destructive pl-3 text-xs text-destructive"
          role="alert"
        >
          Could not load usage: {runsError}
        </p>
      ) : null}

      {runsError === null && runs.length === 0 ? (
        <p className="mt-3 text-sm text-muted-foreground">
          No completion calls yet. Rows appear once the planner or a worker
          calls the LLM.
        </p>
      ) : null}

      {runs.length > 0 ? (
        <div className="mt-4 overflow-x-auto">
          <table className="w-full whitespace-nowrap text-sm">
            <thead>
              <tr className="border-y border-border text-left text-[10px] uppercase tracking-[0.08em] text-muted-foreground">
                <th className="px-4 py-2.5 font-semibold">Call</th>
                <th className="px-3 py-2.5 font-semibold">Role</th>
                <th className="px-3 py-2.5 font-semibold">Status</th>
                <th className="px-3 py-2.5 font-semibold">Model</th>
                <th className="px-3 py-2.5 text-right font-semibold">In</th>
                <th className="px-3 py-2.5 text-right font-semibold">Out</th>
                <th className="px-3 py-2.5 text-right font-semibold">
                  Cache r/w
                </th>
                <th className="px-3 py-2.5 text-right font-semibold">
                  Reasoning
                </th>
                <th className="px-3 py-2.5 text-right font-semibold">Cost</th>
                <th className="px-3 py-2.5 text-right font-semibold">
                  Latency
                </th>
                <th className="px-3 py-2.5 font-semibold">When</th>
                <th className="px-4 py-2.5 font-semibold">Error</th>
              </tr>
            </thead>
            <tbody>
              {runs.map((run) => (
                <tr
                  key={run.id}
                  className="border-b border-border/70 last:border-0"
                >
                  <td className="px-4 py-3">
                    <span className="font-mono text-xs">
                      {run.call_index}/{shortenId(run.run_id)}
                    </span>
                  </td>
                  <td className="px-3 py-3 text-xs">{run.role}</td>
                  <td className="px-3 py-3">
                    <StateBadge state={run.status} />
                  </td>
                  <td className="px-3 py-3 font-mono text-[11px]">
                    {run.model}
                  </td>
                  <td className="px-3 py-3 text-right font-mono text-[11px] tabular-nums">
                    {run.tokens_in}
                  </td>
                  <td className="px-3 py-3 text-right font-mono text-[11px] tabular-nums">
                    {run.tokens_out}
                  </td>
                  <td className="px-3 py-3 text-right font-mono text-[11px] tabular-nums">
                    {run.cache_read_tokens}/{run.cache_write_tokens}
                  </td>
                  <td className="px-3 py-3 text-right font-mono text-[11px] tabular-nums">
                    {run.reasoning_tokens}
                  </td>
                  <td className="px-3 py-3 text-right font-mono text-[11px] tabular-nums">
                    {run.cost.toFixed(4)}
                  </td>
                  <td className="px-3 py-3 text-right text-[11px] text-muted-foreground">
                    {run.latency_ms} ms
                  </td>
                  <td className="px-3 py-3 text-[11px] text-muted-foreground">
                    {formatLocalTimestamp(run.created_at)}
                  </td>
                  <td
                    className="max-w-[14rem] truncate px-4 py-3 text-[11px] text-muted-foreground"
                    title={run.error ?? undefined}
                  >
                    {run.error ?? ""}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ) : null}
    </section>
  );
}
