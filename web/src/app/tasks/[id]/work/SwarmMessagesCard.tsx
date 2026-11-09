"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { ChevronDown, MessageSquareText, RefreshCw } from "lucide-react";

import { getSwarmMessages } from "@/lib/api";
import { formatLocalClock, formatLocalTimestamp } from "@/lib/localTime";
import { describeApiError } from "@/lib/ui";
import {
  groupSwarmRuns,
  parseSwarmMessages,
  swarmKindLabel,
  swarmMessageCounts,
  type SwarmMessageView,
  type SwarmRoundGroup,
  type SwarmRunGroup,
  type SwarmSenderGroup,
} from "@/lib/workTeam";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { CopyButton } from "@/components/ui/copy-button";
import { MarkdownMath } from "@/components/markdown-math";

/** Badge tone per persisted swarm kind; unknown kinds stay neutral. */
function kindVariant(kind: string): "success" | "info" | "warning" | "muted" {
  switch (kind.toLowerCase()) {
    case "finding":
      return "success";
    case "question":
      return "info";
    case "challenge":
      return "warning";
    default:
      return "muted";
  }
}

function KindCounts(props: { counts: Record<string, number> }) {
  const entries = Object.entries(props.counts).sort(([a], [b]) =>
    a < b ? -1 : a > b ? 1 : 0,
  );
  return (
    <>
      {entries.map(([kind, count]) => (
        <Badge key={kind} variant={kindVariant(kind)} className="font-normal">
          {count} {swarmKindLabel(kind).toLowerCase()}
        </Badge>
      ))}
    </>
  );
}

function recipientText(peerCount: number): string {
  return `broadcast to ${peerCount} peer${peerCount === 1 ? "" : "s"}`;
}

/**
 * One message card: type badge, sender, recipients, short summary, and an
 * expandable body. Repeated findings collapse to a `repeat` marker instead of
 * duplicating their text; only malformed payloads show plain JSON.
 */
function SwarmMessageRow(props: { message: SwarmMessageView; peerCount: number }) {
  const { message, peerCount } = props;
  const repeat = message.repeatOf !== null;
  return (
    <li>
      <details className="group rounded-md border border-border bg-card">
        <summary className="flex cursor-pointer list-none flex-wrap items-center gap-x-2 gap-y-1 px-2.5 py-1.5 [&::-webkit-details-marker]:hidden">
          <Badge className="font-normal" variant={kindVariant(message.kind)}>
            {swarmKindLabel(message.kind)}
          </Badge>
          {repeat ? (
            <Badge className="font-normal" variant="muted">
              repeat
            </Badge>
          ) : null}
          <span className="text-[11px] text-muted-foreground">
            from{" "}
            <span className="mono font-medium text-foreground">
              {message.from ?? "unknown"}
            </span>
          </span>
          <span className="text-[10.5px] text-muted-foreground">
            {recipientText(peerCount)}
          </span>
          {repeat ? (
            <span className="min-w-0 flex-1 truncate text-[11.5px] text-muted-foreground">
              repeat of #{message.repeatOf}
            </span>
          ) : message.malformed ? (
            <span className="min-w-0 flex-1 truncate text-[11.5px] text-muted-foreground">
              Unknown message payload
            </span>
          ) : (
            <MarkdownMath
              className="min-w-0 flex-1 truncate text-[11.5px] text-muted-foreground"
              text={message.summary}
              variant="inline"
            />
          )}
          <span
            className="text-[10.5px] text-muted-foreground"
            title={
              message.createdAt === ""
                ? undefined
                : formatLocalTimestamp(message.createdAt)
            }
          >
            {message.createdAt === ""
              ? "—"
              : formatLocalClock(message.createdAt)}
          </span>
          <ChevronDown className="h-3.5 w-3.5 shrink-0 text-muted-foreground transition-transform group-open:rotate-180" />
        </summary>
        <div className="border-t border-border px-3 py-2">
          {repeat ? (
            <p className="text-[12px] text-muted-foreground">
              Same finding text as message #{message.repeatOf}; not repeated
              here.
            </p>
          ) : message.malformed ? (
            <pre className="pre mt-0 max-h-80 overflow-auto">
              {message.fullText}
            </pre>
          ) : (
            <MarkdownMath
              className="mt-0 max-h-80 overflow-auto text-[13px] leading-5 [&_p+p]:mt-2 [&_ul]:my-1.5 [&_ol]:my-1.5 [&_li]:my-0.5 [&_pre]:my-1.5 [&_blockquote]:my-1.5 [&_.katex-display]:my-1.5"
              text={message.fullText}
              variant="block"
            />
          )}
          <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-[10.5px] text-muted-foreground">
            <span>
              intent{" "}
              <span className="mono" title={message.intentId || undefined}>
                {message.intentShort === "" ? "—" : message.intentShort}
              </span>
            </span>
            {message.subtaskId !== null ? (
              <span>
                subtask <span className="mono">{message.subtaskId}</span>
              </span>
            ) : null}
            <CopyButton
              className="ml-auto"
              text={message.fullText}
              label="swarm message"
              size="icon"
            />
          </div>
        </div>
      </details>
    </li>
  );
}

function SwarmSenderSection(props: {
  group: SwarmSenderGroup;
  peerCount: number;
}) {
  const { group, peerCount } = props;
  return (
    <div className="mt-2">
      <div className="flex flex-wrap items-center gap-1.5 px-0.5">
        <span className="mono text-[11.5px] font-semibold">{group.sender}</span>
        <Badge variant="outline" className="font-mono font-normal">
          {group.counts.total}
        </Badge>
        <KindCounts counts={group.counts.byKind} />
      </div>
      <ul className="mt-1 space-y-1.5">
        {group.messages.map((message) => (
          <SwarmMessageRow
            key={message.id}
            message={message}
            peerCount={peerCount}
          />
        ))}
      </ul>
    </div>
  );
}

function SwarmRoundSection(props: {
  group: SwarmRoundGroup;
  peerCount: number;
}) {
  const { group, peerCount } = props;
  return (
    <div className="mt-2.5">
      <div className="flex flex-wrap items-center gap-1.5 px-0.5">
        <span className="text-[10.5px] font-semibold uppercase tracking-[0.08em] text-muted-foreground">
          {group.round === null ? "Round ?" : `Round ${group.round}`}
        </span>
        <Badge variant="outline" className="font-mono font-normal">
          {group.counts.total}
        </Badge>
        <KindCounts counts={group.counts.byKind} />
      </div>
      {group.senders.map((sender) => (
        <SwarmSenderSection
          key={`${sender.sender}:${group.round ?? "unknown"}`}
          group={sender}
          peerCount={peerCount}
        />
      ))}
    </div>
  );
}

function SwarmRunSection(props: { group: SwarmRunGroup }) {
  const { group } = props;
  return (
    <section className="rounded-lg border border-border bg-surface-subtle px-2.5 py-2.5">
      <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
        <span className="text-[12px] font-semibold">{group.label}</span>
        {group.runShort !== "" ? (
          <span className="mono text-[10.5px] text-muted-foreground">
            #{group.runShort}
          </span>
        ) : null}
        <Badge variant="outline" className="font-normal">
          {group.counts.total} message{group.counts.total === 1 ? "" : "s"}
        </Badge>
        <span className="text-[10.5px] text-muted-foreground">
          {group.senders.length} sender{group.senders.length === 1 ? "" : "s"}{" "}
          · {group.rounds.length} round{group.rounds.length === 1 ? "" : "s"}
        </span>
        <KindCounts counts={group.counts.byKind} />
      </div>
      {group.rounds.map((round) => (
        <SwarmRoundSection
          key={`${group.runId}:${round.round ?? "unknown"}`}
          group={round}
          peerCount={group.peerCount}
        />
      ))}
    </section>
  );
}

/**
 * Audit view of a task's DHI+ swarm broadcasts, laid out as run → round →
 * sender. A task with one swarm run shows one run; multiple runs are
 * numbered in message order. There is no polling: the tab fetch runs on
 * mount and refresh is manual.
 */
export function SwarmMessagesCard(props: { taskId: string }) {
  const { taskId } = props;
  const [messages, setMessages] = useState<SwarmMessageView[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const mounted = useRef(true);
  const seq = useRef(0);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  const load = useCallback(async (): Promise<void> => {
    const token = ++seq.current;
    setLoading(true);
    setError(null);
    try {
      const records = await getSwarmMessages(taskId);
      if (!mounted.current || token !== seq.current) {
        return;
      }
      setMessages(parseSwarmMessages(records));
    } catch (err) {
      if (!mounted.current || token !== seq.current) {
        return;
      }
      setError(describeApiError(err));
    } finally {
      if (mounted.current && token === seq.current) {
        setLoading(false);
      }
    }
  }, [taskId]);

  // Reset local state when the route moves to another task; the parent also
  // keys this card, but the guard keeps it correct on its own.
  useEffect(() => {
    seq.current += 1;
    setMessages(null);
    setLoading(false);
    setError(null);
  }, [taskId]);

  useEffect(() => {
    void load();
  }, [load]);

  const runs = messages === null ? [] : groupSwarmRuns(messages);
  const counts = swarmMessageCounts(messages ?? []);

  return (
    <Card>
      <CardHeader className="flex-row flex-wrap items-start justify-between gap-3 border-b border-border pb-4">
        <div className="flex items-start gap-2.5">
          <span className="flex h-8 w-8 items-center justify-center rounded-lg bg-accent text-accent-foreground">
            <MessageSquareText className="h-4 w-4" />
          </span>
          <div>
            <CardTitle className="text-base">Swarm messages</CardTitle>
            <CardDescription className="mt-0.5">
              Typed peer broadcasts grouped by run, round, and sender.
            </CardDescription>
          </div>
        </div>
        <div className="flex items-center gap-2">
          {messages !== null ? (
            <Badge variant="outline" className="font-mono font-normal">
              {counts.total}
            </Badge>
          ) : null}
          <Button
            className="h-8 rounded-full"
            disabled={loading}
            onClick={() => void load()}
            size="sm"
            type="button"
            variant="outline"
          >
            <RefreshCw
              className={`h-3.5 w-3.5 ${loading ? "animate-spin" : ""}`}
            />
            {loading ? "Loading…" : messages !== null ? "Refresh" : "Load"}
          </Button>
        </div>
      </CardHeader>
      <CardContent className="pt-4">
        {error !== null ? (
          <div className="errorbox mt-0" role="alert">
            Could not load swarm messages: {error}
          </div>
        ) : null}

        {loading && messages === null ? (
          <p className="empty">Loading swarm messages…</p>
        ) : null}

        {messages !== null && messages.length === 0 ? (
          <div className="rounded-lg border border-dashed border-border bg-surface-subtle px-4 py-6 text-center text-xs text-muted-foreground">
            No swarm messages recorded for this task.
          </div>
        ) : null}

        {messages !== null && messages.length > 0 ? (
          <>
            <div className="mb-3 flex flex-wrap items-center gap-1.5">
              <Badge variant="outline" className="font-normal">
                {counts.total} message{counts.total === 1 ? "" : "s"}
              </Badge>
              <Badge variant="outline" className="font-normal">
                {counts.workers} sender{counts.workers === 1 ? "" : "s"}
              </Badge>
              <Badge variant="outline" className="font-normal">
                {counts.rounds} round{counts.rounds === 1 ? "" : "s"}
              </Badge>
              <KindCounts counts={counts.byKind} />
            </div>
            <div className="space-y-2.5">
              {runs.map((run) => (
                <SwarmRunSection key={run.runId || "unknown-run"} group={run} />
              ))}
            </div>
          </>
        ) : null}
      </CardContent>
    </Card>
  );
}
