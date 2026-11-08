"use client";

import { useEffect, useRef, useState } from "react";
import Link from "next/link";
import {
  FEED_FALLBACK_ERROR_STREAK,
  FEED_LIVE_COUNT,
  FEED_POLL_INTERVAL_MS,
  FEED_RECENT_COUNT,
  FEED_STREAM_RETRY_MS,
  activityActor,
  activityKindView,
  activityToolIo,
  capTimeline,
  createLiveFeedController,
  formatFeedTime,
  hasVisiblePayload,
  prettyJson,
  previewFeedText,
  toolIoFullText,
  type FeedConnection,
  type FeedSignal,
  type FeedToolIo,
  type LiveFeedSnapshot,
} from "@/lib/feed";
import type { Activity } from "@/lib/types";
import { describeApiError } from "@/lib/ui";
import { formatLocalTimestamp } from "@/lib/localTime";
import { latestTeamRun, type TeamRun } from "@/lib/teamRun";
import { CopyButton } from "@/components/ui/copy-button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { StateBadge } from "@/components/ui/state-badge";

/**
 * The activity timeline and its two riders.
 *
 * - `ActivityTimeline` — the shared presentational piece: a vertical event
 *   log, NEWEST AT BOTTOM (the header says so), each entry showing the
 *   system-local time (short clock, full stamp on hover), the kind as a
 *   badge, the actor when derivable, the message, and either one combined
 *   tool input/output cell for `tool_allowed` rows or the generic payload
 *   collapsed behind a one-click `<details>`.
 * - `LiveActivityFeed` — the work view's live log: history seed + SSE
 *   stream + connection indicator + graceful polling fallback, showing the
 *   newest ten events.
 * - `RecentActivityFeed` — the task detail page's compact list: last ten
 *   rows, no stream, linking to the work view (keeps detail fast).
 */

// ---------------------------------------------------------------------------
// The live hook (thin glue over the headless controller in lib/feed.ts)
// ---------------------------------------------------------------------------

const EMPTY_SNAPSHOT: LiveFeedSnapshot = {
  rows: [],
  cursor: 0,
  connection: "connecting",
  historyError: null,
  pollError: null,
};

function useLiveActivityFeed(
  taskId: string,
  onSignal?: (signal: FeedSignal) => void,
): LiveFeedSnapshot {
  const [snapshot, setSnapshot] = useState<LiveFeedSnapshot>(EMPTY_SNAPSHOT);
  // Latest-signal indirection so parent callbacks may be unstable without
  // restarting the controller (the effect below is keyed on taskId only).
  const signalRef = useRef(onSignal);
  useEffect(() => {
    signalRef.current = onSignal;
  });

  useEffect(() => {
    if (taskId === "") {
      return;
    }
    setSnapshot(EMPTY_SNAPSHOT);
    const controller = createLiveFeedController(taskId, {
      describeError: describeApiError,
      onSignal: (signal) => signalRef.current?.(signal),
    });
    const unsubscribe = controller.subscribe(setSnapshot);
    controller.start();

    const onVisibility = (): void => {
      controller.visibility(document.visibilityState === "visible");
    };
    document.addEventListener("visibilitychange", onVisibility);

    return () => {
      document.removeEventListener("visibilitychange", onVisibility);
      controller.dispose();
      unsubscribe();
    };
  }, [taskId]);

  return snapshot;
}

// ---------------------------------------------------------------------------
// Presentational pieces
// ---------------------------------------------------------------------------

const CONNECTION_LABELS: Record<FeedConnection, string> = {
  connecting: "connecting…",
  live: "live",
  reconnecting: "reconnecting…",
  polling: "polling",
};

function connectionDescription(
  connection: FeedConnection,
  cursor: number,
): string {
  switch (connection) {
    case "live":
      return `Connected to the activity stream. Resume cursor #${cursor}: after a reconnect only rows with a higher id are delivered — older ones are already on screen.`;
    case "connecting":
      return "Opening the activity stream…";
    case "reconnecting":
      return "The stream hiccupped; the client is resuming from the last seen row (amber until hello returns).";
    case "polling":
      return `The stream did not recover after ${FEED_FALLBACK_ERROR_STREAK} consecutive errors — polling the activity route every ${FEED_POLL_INTERVAL_MS / 1000} s instead and retrying the stream every ${Math.round(FEED_STREAM_RETRY_MS / 1000)} s.`;
  }
}

function ConnectionChip(props: { connection: FeedConnection; cursor: number }) {
  const { connection, cursor } = props;
  const dotClass =
    connection === "live"
      ? "dot live"
      : connection === "reconnecting"
        ? "dot warn"
        : "dot";
  return (
    <span
      className="status subtle"
      title={connectionDescription(connection, cursor)}
    >
      <span className={dotClass} aria-hidden="true" />
      {CONNECTION_LABELS[connection]}
      <span
        className="mono feed-cursor"
        title={`Highest activity id seen (#${cursor}); doubled as the stream resume cursor.`}
      >
        #{cursor}
      </span>
    </span>
  );
}

/** Defensive display text: history rows may carry unexpected field types. */
function displayText(value: unknown): string {
  return typeof value === "string" ? value : String(value ?? "");
}

function ToolValueBlock(props: { label: string; text: string }) {
  const { label, text } = props;
  const { preview, truncated } = previewFeedText(text);
  const lower = label.toLowerCase();
  return (
    <div className="min-w-0">
      <div className="flex items-center justify-between gap-2">
        <span className="text-[10px] font-semibold uppercase tracking-[0.08em] text-muted-foreground">
          {label}
        </span>
        <CopyButton text={text} label={lower} size="icon" />
      </div>
      <pre className="pre mt-1 max-h-64 overflow-auto whitespace-pre-wrap break-words">
        {preview}
      </pre>
      {truncated ? (
        <details className="feed-details">
          <summary className="subtle">Show full {lower}</summary>
          <pre className="pre max-h-80">{text}</pre>
        </details>
      ) : null}
    </div>
  );
}

/**
 * One tool call's input and output, combined in a single cell. Older
 * `tool_allowed` rows have no `input`; they render the output alone.
 */
function ToolIoCell(props: { io: FeedToolIo }) {
  const { io } = props;
  return (
    <div className="mt-2 rounded-lg border border-border bg-surface-subtle px-3 py-2">
      <div className="flex items-center justify-between gap-2">
        <span className="text-[10px] font-semibold uppercase tracking-[0.08em] text-muted-foreground">
          Tool call
        </span>
        <CopyButton
          text={toolIoFullText(io)}
          label="tool input and output"
          size="icon"
        />
      </div>
      <div className="mt-1.5 space-y-2">
        {io.input !== null ? (
          <ToolValueBlock label="Input" text={io.input} />
        ) : null}
        {io.output !== null ? (
          <ToolValueBlock label="Output" text={io.output} />
        ) : null}
      </div>
    </div>
  );
}

function ActivityEntry(props: { activity: Activity }) {
  const { activity } = props;
  const kind = activityKindView(activity.kind);
  const actor = activityActor(activity.kind);
  const message = displayText(activity.message);
  const toolIo = activityToolIo(activity);
  const payload =
    toolIo === null && hasVisiblePayload(activity.payload)
      ? prettyJson(activity.payload)
      : null;
  return (
    <li className="feed-item">
      <div className="feed-meta">
        <span
          className="time"
          title={formatLocalTimestamp(displayText(activity.created_at))}
        >
          {formatFeedTime(displayText(activity.created_at))}
        </span>
        <span className="badge" data-state={kind.tone ?? undefined}>
          {kind.label}
        </span>
        {actor !== null ? <span className="feed-actor">{actor}</span> : null}
      </div>
      <div className="row spread">
        <span className="feed-msg">{message}</span>
        <CopyButton text={message} label="message" size="icon" />
      </div>
      {toolIo !== null ? <ToolIoCell io={toolIo} /> : null}
      {payload !== null ? (
        <details className="feed-details">
          <summary className="subtle">payload</summary>
          <pre className="pre">{payload}</pre>
          <CopyButton
            text={payload}
            label="payload"
            size="icon"
            className="mt-1"
          />
        </details>
      ) : null}
    </li>
  );
}

/**
 * The list itself: the newest window only, with no hidden-count line. One
 * activity is one row; tool input and output never split into separate rows.
 */
function ActivityList(props: { rows: readonly Activity[]; max: number }) {
  const { kept } = capTimeline(props.rows, props.max);
  if (props.rows.length === 0) {
    return <p className="empty">No activity recorded yet.</p>;
  }
  return (
    <ul className="feed">
      {kept.map((activity) => (
        <ActivityEntry key={activity.id} activity={activity} />
      ))}
    </ul>
  );
}

// ---------------------------------------------------------------------------
// Live log (work view)
// ---------------------------------------------------------------------------

/**
 * Compact DHI+ summary of the latest `team_run` activity: topology, worker
 * count, outcome totals, and the per-worker keys/roles/statuses/subtasks.
 * Rendered only when the feed contains a valid team run — no API call.
 */
function TeamRunPanel(props: { run: TeamRun }) {
  const { run } = props;
  return (
    <div className="mb-4 rounded-lg border border-border bg-surface-subtle px-3.5 py-3">
      <div className="flex flex-wrap items-center justify-between gap-x-3 gap-y-2">
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-[12px] font-semibold uppercase tracking-wide text-muted-foreground">
            Team run
          </span>
          <Badge variant="outline" className="font-normal">
            DHI<sup>+</sup>
          </Badge>
          <Badge variant="accent" className="font-mono">
            {run.topology}
          </Badge>
          <span className="text-[11.5px] text-muted-foreground">
            {run.workerCount} worker{run.workerCount === 1 ? "" : "s"}
          </span>
        </div>
        <div className="flex flex-wrap items-center gap-1.5">
          <Badge variant="success" className="font-normal">
            {run.success} success
          </Badge>
          <Badge
            variant={run.failed > 0 ? "destructive" : "muted"}
            className="font-normal"
          >
            {run.failed} failed
          </Badge>
          <Badge
            variant={run.skipped > 0 ? "warning" : "muted"}
            className="font-normal"
          >
            {run.skipped} skipped
          </Badge>
        </div>
      </div>
      {run.workers.length > 0 ? (
        <ul className="mt-2.5 space-y-1.5">
          {run.workers.map((worker, index) => (
            <li
              key={`${worker.key}:${index}`}
              className="flex flex-wrap items-center gap-x-2 gap-y-1 rounded-md border border-border bg-card px-2.5 py-1.5 text-[11.5px]"
            >
              <span className="mono font-medium" title={worker.key}>
                {worker.key}
              </span>
              {worker.role !== null ? (
                <span className="text-muted-foreground">{worker.role}</span>
              ) : null}
              {worker.status !== null ? (
                <StateBadge state={worker.status} />
              ) : null}
              {worker.subtask !== null ? (
                <span className="text-muted-foreground">
                  subtask <span className="mono">{worker.subtask}</span>
                </span>
              ) : null}
            </li>
          ))}
        </ul>
      ) : (
        <p className="mt-2 text-[11px] text-muted-foreground">
          Worker details were not recorded for this run.
        </p>
      )}
    </div>
  );
}

export function LiveActivityFeed(props: {
  taskId: string;
  onSignal?: (signal: FeedSignal) => void;
}) {
  const snapshot = useLiveActivityFeed(props.taskId, props.onSignal);
  const teamRun = latestTeamRun(snapshot.rows);
  return (
    <Card>
      <CardHeader className="flex-row flex-wrap items-start justify-between gap-3 border-b border-border pb-4">
        <div>
          <CardTitle className="text-base">Activity</CardTitle>
          <CardDescription className="mt-0.5">
            Live event log — newest last, last ten shown.
          </CardDescription>
        </div>
        <Badge
          variant="outline"
          className="h-7 gap-1.5 bg-surface-subtle px-2.5 font-normal"
        >
          <ConnectionChip
            connection={snapshot.connection}
            cursor={snapshot.cursor}
          />
        </Badge>
      </CardHeader>
      <CardContent className="pt-4">
        {snapshot.historyError !== null ? (
          <div className="errorbox mt-0" role="alert">
            Could not load past activity: {snapshot.historyError} — the live
            stream below still appends as events happen.
          </div>
        ) : null}
        {snapshot.pollError !== null ? (
          <div className="errorbox mt-0" role="alert">
            Polling fallback failed: {snapshot.pollError} — keeping the last
            snapshot and retrying.
          </div>
        ) : null}

        {teamRun !== null ? <TeamRunPanel run={teamRun} /> : null}

        <ActivityList rows={snapshot.rows} max={FEED_LIVE_COUNT} />
      </CardContent>
    </Card>
  );
}

// ---------------------------------------------------------------------------
// Compact recent list (task detail page — no stream, no indicator)
// ---------------------------------------------------------------------------

export function RecentActivityFeed(props: {
  taskId: string;
  /** Full fetched history, or `null` while loading. */
  rows: readonly Activity[] | null;
  error: string | null;
}) {
  const rows = props.rows ?? [];
  return (
    <Card>
      <CardHeader className="flex-row flex-wrap items-start justify-between gap-3 border-b border-border pb-4">
        <div>
          <CardTitle className="text-base">Recent activity</CardTitle>
          <CardDescription className="mt-0.5">
            Latest events — newest last.
          </CardDescription>
        </div>
        {props.rows !== null && rows.length > 0 ? (
          <Badge variant="outline" className="font-normal">
            last {Math.min(FEED_RECENT_COUNT, rows.length)} of {rows.length}
          </Badge>
        ) : null}
      </CardHeader>
      <CardContent className="pt-4">
        {props.error !== null ? (
          <div className="errorbox mt-0" role="alert">
            Could not load activity: {props.error}
          </div>
        ) : null}
        {props.rows === null && props.error === null ? (
          <p className="empty">Loading activity…</p>
        ) : null}

        <ActivityList rows={rows} max={FEED_RECENT_COUNT} />

        <p className="mt-3 text-xs">
          <Link
            href={`/tasks/${props.taskId}/work`}
            className="inline-flex items-center gap-1 text-muted-foreground hover:text-foreground"
          >
            Full live activity log on the work view →
          </Link>
        </p>
      </CardContent>
    </Card>
  );
}
