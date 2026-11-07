"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { MessageSquareText, RefreshCw } from "lucide-react";

import { getSwarmMessages } from "@/lib/api";
import { formatLocalClock, formatLocalTimestamp } from "@/lib/localTime";
import { describeApiError } from "@/lib/ui";
import {
  parseSwarmMessages,
  swarmKindLabel,
  type SwarmMessageView,
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

/** Badge tone per persisted swarm kind; unknown kinds stay neutral. */
function kindVariant(
  kind: string,
): "success" | "info" | "warning" | "muted" {
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

/**
 * Compact audit log of a task's DHI+ swarm broadcasts.
 *
 * It fetches once automatically when the work view has already seen a swarm
 * `team_run` (`autoLoad`), otherwise behind the explicit button. There is no
 * polling: refresh is manual and in-flight responses are sequence-guarded.
 */
export function SwarmMessagesCard(props: {
  taskId: string;
  autoLoad: boolean;
}) {
  const { taskId, autoLoad } = props;
  const [messages, setMessages] = useState<SwarmMessageView[] | null>(null);
  const [loaded, setLoaded] = useState(false);
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
      setLoaded(true);
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

  // Reset local state when the route moves to another task (the parent also
  // keys this card, but the guard keeps it correct on its own).
  useEffect(() => {
    seq.current += 1;
    setMessages(null);
    setLoaded(false);
    setLoading(false);
    setError(null);
  }, [taskId]);

  useEffect(() => {
    if (autoLoad) {
      void load();
    }
  }, [autoLoad, load]);

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
              Typed peer broadcasts from DHI+ swarm runs.
            </CardDescription>
          </div>
        </div>
        <div className="flex items-center gap-2">
          {loaded && messages !== null ? (
            <Badge variant="outline" className="font-mono font-normal">
              {messages.length}
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
            {loading
              ? "Loading…"
              : loaded || error !== null
                ? "Refresh"
                : "Load swarm messages"}
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

        {!loading && loaded && messages !== null && messages.length === 0 ? (
          <div className="rounded-lg border border-dashed border-border bg-surface-subtle px-4 py-6 text-center text-xs text-muted-foreground">
            No swarm messages
          </div>
        ) : null}

        {messages !== null && messages.length > 0 ? (
          <ul className="space-y-1.5">
            {messages.map((message) => (
              <li
                className="rounded-lg border border-border bg-surface-subtle px-3 py-2"
                key={message.id}
              >
                <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
                  <Badge
                    className="font-normal"
                    variant={kindVariant(message.kind)}
                  >
                    {swarmKindLabel(message.kind)}
                  </Badge>
                  {message.from !== null ? (
                    <span className="text-[11px] text-muted-foreground">
                      from{" "}
                      <span className="mono font-medium text-foreground">
                        {message.from}
                      </span>
                    </span>
                  ) : null}
                  <span
                    className="mono text-[10.5px] text-muted-foreground"
                    title={message.intentId === "" ? undefined : message.intentId}
                  >
                    intent {message.intentShort === "" ? "—" : message.intentShort}
                  </span>
                  <span
                    className="ml-auto text-[10.5px] text-muted-foreground"
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
                </div>
                <p className="mt-1 break-words text-[12px] leading-5">
                  {message.summary}
                </p>
              </li>
            ))}
          </ul>
        ) : null}
      </CardContent>
    </Card>
  );
}
