"use client";

import { useEffect, useRef, useState } from "react";
import Link from "next/link";
import { useParams, usePathname } from "next/navigation";
import {
  AlertTriangle,
  ArrowLeft,
  CheckCircle2,
  GitBranch,
  ListTree,
  MessageSquareText,
  Radio,
} from "lucide-react";

import { ApiError, getTaskActivity, getTaskWork } from "@/lib/api";
import type { Activity, Fact, ResultRecord, TaskWork } from "@/lib/types";
import { FEED_GRAPH_REFRESH_DEBOUNCE_MS, type FeedSignal } from "@/lib/feed";
import {
  describeApiError,
  renderProvenance,
  shortenId,
  taskIdFromPathname,
  topologyLabel,
} from "@/lib/ui";
import { formatLocalTimestamp } from "@/lib/localTime";
import { crumbTrailSegments } from "@/lib/nav";
import {
  capacityPauseNotice,
  resolveTaskTopology,
  taskUsesSwarm,
  type CapacityPauseNotice,
} from "@/lib/workTeam";
import { Crumbs } from "@/app/_components/Crumbs";
import { LiveActivityFeed } from "@/app/_components/ActivityLog";
import { MarkdownMath } from "@/components/markdown-math";
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
import { CapacityCard } from "./CapacityCard";
import { SwarmMessagesCard } from "./SwarmMessagesCard";

/** Poll cadence for the work view; suspended while the SSE stream is live. */
const POLL_INTERVAL_MS = 5000;

/** Wide but readable content column; the app shell caps navigation separately. */
const CONTENT_CLASS = "mx-auto w-full max-w-[1160px] space-y-5";

/** Tabs in the work view; Swarm is only offered for swarm tasks. */
type WorkTab = "graph" | "activity" | "swarm";

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
  const [activityRows, setActivityRows] = useState<Activity[]>([]);
  const [activeTab, setActiveTab] = useState<WorkTab>("graph");

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
    setActivityRows([]);
    setActiveTab("graph");

    const load = async (): Promise<void> => {
      try {
        const [snapshot, rows] = await Promise.all([
          getTaskWork(id),
          // Activity failure must never hide the work graph; the notice and
          // topology fallback simply stay unavailable until the next load.
          getTaskActivity(id).catch(() => null),
        ]);
        if (!mounted) {
          return;
        }
        setWork(snapshot);
        if (rows !== null) {
          setActivityRows(rows);
        }
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
      <main className={CONTENT_CLASS}>
        <p className="empty">Waiting for the task id…</p>
      </main>
    );
  }

  if (notFound) {
    return (
      <main className={CONTENT_CLASS}>
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
              No task with id <code>{id}</code> exists, so there is no work
              graph to show.
            </p>
          </CardContent>
        </Card>
      </main>
    );
  }

  if (loading || work === null) {
    return (
      <main className={CONTENT_CLASS}>
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
  // `task.topology` is authoritative when present; older rows fall back to the
  // latest team_run. The Swarm tab follows the same signal.
  const effectiveTopology = resolveTaskTopology(
    work.task.topology,
    activityRows,
  );
  const topologyName = topologyLabel(effectiveTopology);
  const swarmAvailable = taskUsesSwarm(work.task.topology, activityRows);
  const capacityNotice = capacityPauseNotice(work.task, activityRows);
  const shownTab: WorkTab =
    activeTab === "swarm" && !swarmAvailable ? "graph" : activeTab;
  // The live feed only exists on the Activity tab; leaving it unmounts the
  // controller, which never emits a final connection signal. Clear the
  // suspension flag here so the work graph poll resumes.
  const selectTab = (tab: WorkTab): void => {
    if (tab !== "activity") {
      streamLiveRef.current = false;
      setStreamLive(false);
    }
    setActiveTab(tab);
  };

  return (
    <main className={CONTENT_CLASS}>
      <Crumbs trail={crumbTrailSegments(work.task.id, "Work")} />

      {pollError !== null ? (
        <div className="errorbox" role="alert">
          Auto-refresh failed: {pollError} — keeping the last snapshot and
          retrying.
        </div>
      ) : null}

      <PageHeader
        eyebrow="Work graph"
        icon={<GitBranch className="h-3.5 w-3.5" />}
        title="Work"
        description={
          <span className="flex flex-wrap items-center gap-2">
            <StateBadge state={work.task.state} />
            {topologyName !== null ? (
              <Badge variant="outline" title="Task topology">
                Topology: {topologyName}
              </Badge>
            ) : null}
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
                <GitBranch className="h-3.5 w-3.5" /> Visual graph
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

      <GoalBlock text={work.task.goal} />

      {capacityNotice !== null ? (
        <CapacityNoticeBar notice={capacityNotice} />
      ) : null}

      <CapacityCard
        task={work.task}
        onTaskUpdated={(updated) =>
          setWork((current) =>
            current === null ? current : { ...current, task: updated },
          )
        }
      />

      <WorkTabBar
        active={shownTab}
        onSelect={selectTab}
        swarmAvailable={swarmAvailable}
      />

      {shownTab === "graph" ? (
        <>
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
                        <span>
                          completed {formatLocalTimestamp(intent.completed_at)}
                        </span>
                      </>
                    ) : null}
                  </p>
                </div>
              ))
            )}
          </SectionCard>
        </>
      ) : null}

      {shownTab === "activity" ? (
        <LiveActivityFeed
          taskId={work.task.id}
          onSignal={(signal) => signalBridgeRef.current?.(signal)}
        />
      ) : null}

      {shownTab === "swarm" && swarmAvailable ? (
        <SwarmMessagesCard key={work.task.id} taskId={work.task.id} />
      ) : null}

      {shownTab === "graph" ? (
        <>
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
                      <MarkdownMath
                        text={result.claim}
                        className="min-w-0 flex-1 text-[13.5px] font-medium leading-5"
                      />
                      <CopyButton
                        text={result.claim}
                        label="result claim"
                        size="icon"
                      />
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
        </>
      ) : null}
    </main>
  );
}

// ---------------------------------------------------------------------------
// Pieces
// ---------------------------------------------------------------------------

/** The task goal as quiet body text, never the oversized page title. */
function GoalBlock(props: { text: string }) {
  return (
    <section aria-labelledby="task-goal-label">
      <h2
        id="task-goal-label"
        className="text-[11px] font-semibold uppercase tracking-[0.16em] text-muted-foreground"
      >
        Goal
      </h2>
      <MarkdownMath
        text={props.text}
        className="mt-1.5 max-w-[80ch] text-[15.5px] leading-[1.7] text-foreground"
      />
    </section>
  );
}

/** Warning bar above the capacity editor while a limit keeps the task paused. */
function CapacityNoticeBar(props: { notice: CapacityPauseNotice }) {
  return (
    <div
      className="flex items-start gap-2.5 rounded-lg border border-warning/40 bg-warning/10 px-3.5 py-2.5 text-[13px] leading-5 text-warning"
      role="status"
    >
      <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" />
      <div className="min-w-0">
        <p className="font-medium">{props.notice.text}</p>
        <p className="mt-0.5 text-[11.5px] text-warning/85">
          Extend the exhausted limit below to resume the task.
        </p>
      </div>
    </div>
  );
}

/** Work-view tab strip: Graph, Activity, and (swarm tasks only) Swarm. */
function WorkTabBar(props: {
  active: WorkTab;
  onSelect: (tab: WorkTab) => void;
  swarmAvailable: boolean;
}) {
  const tabs: Array<{ id: WorkTab; label: string; icon: React.ReactNode }> = [
    {
      id: "graph",
      label: "Graph",
      icon: <GitBranch className="h-3.5 w-3.5" />,
    },
    {
      id: "activity",
      label: "Activity",
      icon: <Radio className="h-3.5 w-3.5" />,
    },
  ];
  if (props.swarmAvailable) {
    tabs.push({
      id: "swarm",
      label: "Swarm",
      icon: <MessageSquareText className="h-3.5 w-3.5" />,
    });
  }
  return (
    <div
      aria-label="Work view tabs"
      className="flex flex-wrap items-center gap-1 border-b border-border pb-2"
      role="tablist"
    >
      {tabs.map((tab) => {
        const selected = tab.id === props.active;
        return (
          <button
            key={tab.id}
            aria-selected={selected}
            className={`inline-flex h-8 items-center gap-1.5 rounded-full border px-3 text-[12.5px] font-medium transition-colors ${
              selected
                ? "border-accent-foreground/30 bg-accent text-accent-foreground"
                : "border-transparent text-muted-foreground hover:bg-surface-subtle hover:text-foreground"
            }`}
            onClick={() => props.onSelect(tab.id)}
            role="tab"
            type="button"
          >
            {tab.icon}
            {tab.label}
          </button>
        );
      })}
    </div>
  );
}

/**
 * One fact: claim, state badge, and its provenance rendered according to the
 * detected convention (origin `{goal}` vs worker
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
        <div className="flex min-w-0 flex-1 items-start gap-1">
          <MarkdownMath
            text={fact.claim}
            className="min-w-0 flex-1 text-[13.5px] font-medium leading-5"
          />
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
            <ProvenanceLine
              key={index}
              line={line}
              last={index === provenance.lines.length - 1}
            />
          ))}
        </p>
      )}
    </div>
  );
}

/**
 * One provenance fragment. Model-authored values (goal, detail, evidence)
 * render Markdown + KaTeX; ids and confidence stay literal.
 */
function ProvenanceLine(props: { line: string; last: boolean }) {
  const separator = props.line.indexOf(": ");
  const label = separator === -1 ? null : props.line.slice(0, separator);
  const value =
    separator === -1 ? props.line : props.line.slice(separator + 2);
  const rich = label === "Goal" || label === "Detail" || label === "Evidence";
  return (
    <span>
      {label !== null ? `${label}: ` : null}
      {rich ? (
        <MarkdownMath text={value} variant="inline" />
      ) : (
        value
      )}
      {props.last ? null : " · "}
    </span>
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
            <dt className="font-semibold uppercase tracking-wide text-muted-foreground">
              {key}
            </dt>
            <dd className="mt-0.5 break-words font-mono">
              {typeof value === "string"
                ? value
                : JSON.stringify(value, null, 2)}
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
