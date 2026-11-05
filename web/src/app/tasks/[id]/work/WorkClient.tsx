"use client";

import { useEffect, useRef, useState } from "react";
import Link from "next/link";
import { useParams, usePathname } from "next/navigation";
import {
  ArrowLeft,
  CheckCircle2,
  GitBranch,
  ListTree,
  Radio,
} from "lucide-react";

import { ApiError, getTaskWork } from "@/lib/api";
import type { Fact, ResultRecord, TaskWork } from "@/lib/types";
import { FEED_GRAPH_REFRESH_DEBOUNCE_MS, type FeedSignal } from "@/lib/feed";
import {
  describeApiError,
  renderProvenance,
  shortenId,
  taskIdFromPathname,
} from "@/lib/ui";
import { formatLocalTimestamp } from "@/lib/localTime";
import { crumbTrailSegments } from "@/lib/nav";
import { Crumbs } from "@/app/_components/Crumbs";
import { LiveActivityFeed } from "@/app/_components/ActivityLog";
import { CopyButton } from "@/components/ui/copy-button";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { PageHeader } from "@/components/ui/page-header";
import { Skeleton } from "@/components/ui/skeleton";
import { StateBadge } from "@/components/ui/state-badge";

/** Poll cadence for the work view; suspended while the SSE stream is live. */
const POLL_INTERVAL_MS = 5000;

/**
 * Work view: goals, intents, facts, and results of one task, plus the live
 * activity log. The 5 s work-graph poll suspends while the activity stream
 * is delivering rows.
 */
export function TaskWorkClient() {
  return <TaskWorkView />;
}

function TaskWorkView() {
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
  const [streamLive, setStreamLive] = useState(false);

  // Mirrors for closures that must read/write cross-cutting state without
  // re-subscribing the effect below (keyed on [id]).
  const streamLiveRef = useRef(false);
  const graphReloadRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  // The live feed hands signals here; the effect installs the real handler.
  const signalBridgeRef = useRef<((signal: FeedSignal) => void) | null>(null);

  useEffect(() => {
    if (id === "") {
      return;
    }
    let mounted = true;
    let haveSnapshot = false;
    streamLiveRef.current = false;
    setStreamLive(false);

    const load = async (): Promise<void> => {
      try {
        const snapshot = await getTaskWork(id);
        if (!mounted) {
          return;
        }
        setWork(snapshot);
        setPollError(null);
        setPageError(null);
        setNotFound(false);
        setUpdatedAt(new Date());
        haveSnapshot = true;
      } catch (err) {
        if (!mounted) {
          return;
        }
        if (err instanceof ApiError && err.isNotFound()) {
          setNotFound(true);
          setWork(null);
          return;
        }
        const message = describeApiError(err);
        // A poll failure keeps the last good snapshot and shows a quiet
        // notice; only a first-ever load failure becomes a page error.
        if (haveSnapshot) {
          setPollError(message);
        } else {
          setPageError(message);
        }
      } finally {
        if (mounted) {
          setLoading(false);
        }
      }
    };

    // Live activity rows imply graph changes (claims, proofs, denials…).
    // Debounced refresh replaces the blind 5 s poll while the stream lives.
    const signalHandler = (signal: FeedSignal): void => {
      if (!mounted) {
        return;
      }
      if (signal.kind === "connection") {
        const live = signal.connection === "live";
        streamLiveRef.current = live;
        setStreamLive(live);
        return;
      }
      if (graphReloadRef.current !== null) {
        clearTimeout(graphReloadRef.current);
      }
      graphReloadRef.current = setTimeout(() => {
        graphReloadRef.current = null;
        if (mounted) {
          void load();
        }
      }, FEED_GRAPH_REFRESH_DEBOUNCE_MS);
    };
    signalBridgeRef.current = signalHandler;

    const timer = setInterval(() => {
      // Poll only while the page is actually visible — and not at all while
      // the SSE stream drives freshness (graceful-degradation trade: the
      // feed's fallback restores this poll whenever the stream degrades).
      if (
        typeof document !== "undefined" &&
        document.visibilityState !== "visible"
      ) {
        return;
      }
      if (streamLiveRef.current) {
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
      if (graphReloadRef.current !== null) {
        clearTimeout(graphReloadRef.current);
        graphReloadRef.current = null;
      }
      signalBridgeRef.current = null;
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [id]);

  if (id === "") {
    return (
      <main className="space-y-5">
        <p className="empty">Waiting for the task id…</p>
      </main>
    );
  }

  if (notFound) {
    return (
      <main className="space-y-5">
        <PageHeader
          eyebrow="Work view"
          title="Task not found"
          description="No work graph exists for this id."
          actions={
            <Button asChild variant="outline">
              <Link href="/">
                <ArrowLeft className="h-4 w-4" /> Back to overview
              </Link>
            </Button>
          }
        />
        <Card>
          <CardContent className="pt-5">
            <p className="text-sm text-muted-foreground">
              No task with id <code>{id}</code> exists, so there is no work graph to show.
            </p>
          </CardContent>
        </Card>
      </main>
    );
  }

  if (loading || work === null) {
    return (
      <main className="space-y-5">
        <Crumbs trail={crumbTrailSegments(id, "Work")} />
        {pageError !== null ? (
          <div className="errorbox" role="alert">
            Still connecting… last failure: {pageError}
          </div>
        ) : null}
        <Skeleton className="h-24 w-full max-w-2xl" />
        <div className="grid gap-4 lg:grid-cols-2">
          <Skeleton className="h-64 w-full" />
          <Skeleton className="h-64 w-full" />
        </div>
      </main>
    );
  }

  // Plain derivation (no hook — this sits after the early returns): which
  // fact proves which goal, for the fact-list highlight.
  const proofByFactId = new Map<string, string>();
  for (const goal of work.goals) {
    if (goal.proof_fact_id !== null) {
      proofByFactId.set(goal.proof_fact_id, goal.description);
    }
  }

  return (
    <main className="space-y-5">
      <Crumbs trail={crumbTrailSegments(work.task.id, "Work")} />

      {pollError !== null ? (
        <div className="errorbox" role="alert">
          Auto-refresh failed: {pollError} — keeping the last snapshot and retrying.
        </div>
      ) : null}

      <PageHeader
        eyebrow="Work graph"
        icon={<GitBranch className="h-3.5 w-3.5" />}
        title={work.task.goal}
        description={
          <span className="flex flex-wrap items-center gap-2">
            <StateBadge state={work.task.state} />
            <span className="mono" title={work.task.id}>
              #{shortenId(work.task.id)}
            </span>
            <Badge variant={streamLive ? "success" : "outline"}>
              <Radio className="h-3 w-3" />
              {streamLive
                ? "live via activity stream"
                : `auto-refresh ${POLL_INTERVAL_MS / 1000}s`}
            </Badge>
            {updatedAt !== null ? (
              <span className="text-xs text-muted-foreground">
                updated {formatLocalTimestamp(updatedAt.toISOString())}
              </span>
            ) : null}
          </span>
        }
        actions={
          <>
            <Button asChild className="h-9 rounded-full">
              <Link href={`/tasks/${work.task.id}/graph`}>
                <GitBranch className="h-3.5 w-3.5" /> Graph
              </Link>
            </Button>
            <Button asChild variant="outline" className="h-9 rounded-full">
              <Link href={`/tasks/${work.task.id}`}>
                <ArrowLeft className="h-3.5 w-3.5" /> Task detail
              </Link>
            </Button>
            <Button asChild variant="outline" className="h-9 rounded-full">
              <Link href="/">
                <ListTree className="h-3.5 w-3.5" /> Overview
              </Link>
            </Button>
          </>
        }
      />

      <SectionCard
        title="Goals"
        count={work.goals.length}
        description="Outcomes the planner is trying to prove."
      >
        {work.goals.length === 0 ? (
          <EmptyState message="No goals yet." />
        ) : (
          work.goals.map((goal) => (
            <div
              className="rounded-lg border border-border bg-surface-subtle px-3.5 py-3"
              key={goal.id}
            >
              <div className="flex items-start justify-between gap-3">
                <strong className="text-[13.5px] font-medium leading-5">
                  {goal.description}
                </strong>
                <StateBadge state={goal.state} />
              </div>
              {goal.proof_fact_id !== null ? (
                <p className="mt-2 flex items-center gap-1.5 text-[11px] text-success">
                  <CheckCircle2 className="h-3 w-3" />
                  Proved by fact{" "}
                  <span className="mono" title={goal.proof_fact_id}>
                    {shortenId(goal.proof_fact_id)}
                  </span>
                  (highlighted below)
                </p>
              ) : null}
            </div>
          ))
        )}
      </SectionCard>

      <SectionCard
        title="Intents"
        count={work.intents.length}
        description="Planner proposals; workers claim one and stop."
      >
        {work.intents.length === 0 ? (
          <EmptyState message="No intents yet — the planner has not planned." />
        ) : (
          work.intents.map((intent) => (
            <div
              className="rounded-lg border border-border bg-surface-subtle px-3.5 py-3"
              key={intent.id}
            >
              <div className="flex items-start justify-between gap-3">
                <strong className="text-[13.5px] font-medium leading-5">
                  {intent.description}
                </strong>
                <StateBadge state={intent.state} />
              </div>
              <p className="mt-1.5 flex flex-wrap items-center gap-1.5 text-[11px] text-muted-foreground">
                <span className="mono" title={intent.id}>
                  {shortenId(intent.id)}
                </span>
                <span>·</span>
                <span>owner {intent.owner ?? "unclaimed"}</span>
                <span>·</span>
                <span>priority {intent.priority}</span>
                {intent.completed_at !== null ? (
                  <>
                    <span>·</span>
                    <span>completed {formatLocalTimestamp(intent.completed_at)}</span>
                  </>
                ) : null}
              </p>
            </div>
          ))
        )}
      </SectionCard>

      <LiveActivityFeed
        taskId={work.task.id}
        onSignal={(signal) => signalBridgeRef.current?.(signal)}
      />

      <SectionCard
        title="Facts"
        count={work.facts.length}
        description="Evidence the workers persisted; proven facts are highlighted."
      >
        {work.facts.length === 0 ? (
          <EmptyState message="No facts recorded yet." />
        ) : (
          work.facts.map((fact) => (
            <FactLine
              key={fact.id}
              fact={fact}
              proofOf={proofByFactId.get(fact.id)}
            />
          ))
        )}
      </SectionCard>

      {work.results.length > 0 ? (
        <SectionCard
          title="Results"
          count={work.results.length}
          description="Acceptance decisions with their supporting evidence."
        >
          {work.results.map((result) => (
            <div
              className="rounded-lg border border-border bg-surface-subtle px-3.5 py-3"
              key={result.id}
            >
              <div className="flex items-start justify-between gap-3">
                <div className="flex min-w-0 items-start gap-1">
                  <strong className="text-[13.5px] font-medium leading-5">{result.claim}</strong>
                  <CopyButton text={result.claim} label="result claim" size="icon" />
                </div>
                <StateBadge state={result.acceptance} />
              </div>
              <p className="mt-1.5 flex flex-wrap items-center gap-1.5 text-[11px] text-muted-foreground">
                <span>intent</span>
                <span className="mono" title={result.intent_id}>
                  {shortenId(result.intent_id)}
                </span>
                <span>·</span>
                <span>{formatLocalTimestamp(result.created_at)}</span>
              </p>
              <EvidenceBlock evidence={result.evidence} />
            </div>
          ))}
        </SectionCard>
      ) : null}
    </main>
  );
}

// ---------------------------------------------------------------------------
// Pieces
// ---------------------------------------------------------------------------

/**
 * One fact: claim, state badge, and its provenance rendered according to the
 * detected convention (origin `{goal,bound}` vs worker
 * `{intent_id,detail,evidence,confidence}` vs raw JSON).
 */
function FactLine(props: { fact: Fact; proofOf?: string }) {
  const { fact, proofOf } = props;
  const provenance = renderProvenance(fact.provenance);

  return (
    <div
      className={`rounded-lg border border-border px-3.5 py-3 ${
        proofOf !== undefined
          ? "border-accent-foreground/35 bg-accent/40"
          : "bg-surface-subtle"
      }`}
    >
      <div className="flex items-start justify-between gap-3">
        <div className="flex min-w-0 items-start gap-1">
          <strong className="text-[13.5px] font-medium leading-5">{fact.claim}</strong>
          <CopyButton text={fact.claim} label="fact claim" size="icon" />
        </div>
        <StateBadge state={fact.state} />
      </div>
      {proofOf !== undefined ? (
        <p className="mt-1.5 flex items-center gap-1 text-[11px] text-accent-foreground">
          <CheckCircle2 className="h-3 w-3" /> proves goal “{proofOf}”
        </p>
      ) : null}
      {fact.origin_of !== null ? (
        <p className="mt-1 text-[11px] text-muted-foreground">
          refines{" "}
          <span className="mono" title={fact.origin_of}>
            {shortenId(fact.origin_of)}
          </span>
        </p>
      ) : null}
      {provenance.kind === "raw" ? (
        <pre className="pre">{provenance.lines.join("\n")}</pre>
      ) : (
        <p className="mt-1.5 flex flex-wrap gap-x-2 gap-y-1 text-[11px] leading-4 text-muted-foreground">
          {provenance.lines.map((line, index) => (
            <span key={index}>
              {line}
              {index < provenance.lines.length - 1 ? " · " : ""}
            </span>
          ))}
        </p>
      )}
    </div>
  );
}

function EvidenceBlock(props: { evidence: ResultRecord["evidence"] }) {
  const entries = Object.entries(props.evidence);
  if (entries.length === 0) {
    return null;
  }
  return (
    <details className="mt-2 rounded-md border border-border bg-card px-3 py-2">
      <summary className="cursor-pointer text-[11px] font-medium text-muted-foreground">
        Evidence
      </summary>
      <dl className="mt-2 space-y-2 text-[11px]">
        {entries.map(([key, value]) => (
          <div key={key}>
            <dt className="font-semibold uppercase tracking-wide text-muted-foreground">{key}</dt>
            <dd className="mt-0.5 break-words font-mono">
              {typeof value === "string" ? value : JSON.stringify(value, null, 2)}
            </dd>
          </div>
        ))}
      </dl>
    </details>
  );
}

function SectionCard({
  title,
  count,
  description,
  children,
}: {
  title: string;
  count?: number;
  description?: string;
  children: React.ReactNode;
}) {
  return (
    <Card>
      <CardHeader className="border-b border-border pb-3">
        <div className="flex items-center justify-between gap-2">
          <CardTitle className="text-base">{title}</CardTitle>
          {count !== undefined ? (
            <Badge variant="outline" className="font-mono font-normal">
              {count}
            </Badge>
          ) : null}
        </div>
        {description !== undefined ? (
          <CardDescription>{description}</CardDescription>
        ) : null}
      </CardHeader>
      <CardContent className="space-y-2.5 pt-4">{children}</CardContent>
    </Card>
  );
}

function EmptyState({ message }: { message: string }) {
  return (
    <div className="rounded-lg border border-dashed border-border bg-surface-subtle px-4 py-6 text-center text-xs text-muted-foreground">
      {message}
    </div>
  );
}
