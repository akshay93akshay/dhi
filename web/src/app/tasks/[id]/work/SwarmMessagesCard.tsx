"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { ChevronDown, MessageSquareText, RefreshCw } from "lucide-react";

import { getSwarmMessages } from "@/lib/api";
import { formatLocalClock, formatLocalTimestamp } from "@/lib/localTime";
import { describeApiError } from "@/lib/ui";
import {
  groupSwarmMessages,
  parseSwarmMessages,
  swarmKindLabel,
  swarmMessageCounts,
  type SwarmMessageView,
  type SwarmRoundGroup,
  type SwarmWorkerGroup,
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

function SwarmMessageRow(props: { message: SwarmMessageView }) {
  const { message } = props;
  return (
    <li>
      <details className="group rounded-md border border-border bg-card">
        <summary className="flex cursor-pointer list-none flex-wrap items-center gap-x-2 gap-y-1 px-2.5 py-1.5 [&::-webkit-details-marker]:hidden">
          <Badge className="font-normal" variant={kindVariant(message.kind)}>
            {swarmKindLabel(message.kind)}
          </Badge>
          <span className="text-[11px] text-muted-foreground">
            from{" "}
            <span className="mono font-medium text-foreground">
              {message.from ?? "unknown"}
            </span>
          </span>
          <span className="min-w-0 flex-1 truncate text-[11.5px] text-muted-foreground">
            {message.summary}
          </span>
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
          <pre className="pre mt-0 max-h-80 overflow-auto">
            {message.fullText}
          </pre>
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

function SwarmRoundSection(props: { group: SwarmRoundGroup }) {
  const { group } = props;
  return (
    <div className="mt-2">
      <div className="flex flex-wrap items-center gap-1.5 px-0.5">
        <span className="text-[10.5px] font-semibold uppercase tracking-[0.08em] text-muted-foreground">
          {group.round === null ? "Round ?" : `Round ${group.round}`}
        </span>
        <Badge variant="outline" className="font-mono font-normal">
          {group.counts.total}
        </Badge>
        <KindCounts counts={group.counts.byKind} />
      </div>
      <ul className="mt-1.5 space-y-1.5">
        {group.messages.map((message) => (
          <SwarmMessageRow key={message.id} message={message} />
        ))}
      </ul>
    </div>
  );
}

function SwarmWorkerSection(props: { group: SwarmWorkerGroup }) {
  const { group } = props;
  return (
    <section className="rounded-lg border border-border bg-surface-subtle px-2.5 py-2.5">
      <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
        <span className="mono text-[12px] font-semibold">{group.worker}</span>
        <Badge variant="outline" className="font-normal">
          {group.counts.total} message{group.counts.total === 1 ? "" : "s"}
        </Badge>
        <span className="text-[10.5px] text-muted-foreground">
          {group.rounds.length} round{group.rounds.length === 1 ? "" : "s"}
        </span>
        <KindCounts counts={group.counts.byKind} />
      </div>
      {group.rounds.map((round) => (
        <SwarmRoundSection
          key={`${group.worker}:${round.round ?? "unknown"}`}
          group={round}
        />
      ))}
    </section>
  );
}

/**
 * Grouped audit view of a task's DHI+ swarm broadcasts.
 *
 * Messages are grouped by worker and then by swarm round; each row shows the
 * type, sender, and a short summary, and expands to the untruncated text.
 * There is no polling: the tab fetch runs on mount and refresh is manual.
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

  const groups = messages === null ? [] : groupSwarmMessages(messages);
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
              Typed peer broadcasts grouped by worker and round.
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
                {counts.workers} worker{counts.workers === 1 ? "" : "s"}
              </Badge>
              <Badge variant="outline" className="font-normal">
                {counts.rounds} round{counts.rounds === 1 ? "" : "s"}
              </Badge>
              <KindCounts counts={counts.byKind} />
            </div>
            <div className="space-y-2.5">
              {groups.map((group) => (
                <SwarmWorkerSection key={group.worker} group={group} />
              ))}
            </div>
          </>
        ) : null}
      </CardContent>
    </Card>
  );
}
