"use client";

import {
  type FormEvent,
  useCallback,
  useEffect,
  useRef,
  useState,
} from "react";
import { Bot, MessageSquareText, Send, Sparkles } from "lucide-react";

import { ApiError, askSideQuestion, listSideQuestions } from "@/lib/api";
import type { SideQuestionSession } from "@/lib/types";
import {
  appendSideQuestion,
  newestSideSessions,
  openSideSessionId,
} from "@/lib/sideQuestions";
import { describeApiError } from "@/lib/ui";
import { formatLocalTimestamp } from "@/lib/localTime";
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
import { Skeleton } from "@/components/ui/skeleton";
import { StateBadge } from "@/components/ui/state-badge";
import { Textarea } from "@/components/ui/textarea";

/** Matches the server's post-trim limit. */
const MAX_QUESTION_CHARS = 4000;

/**
 * Task-detail "Ask about this task" panel.
 *
 * Read-only by construction: the server answers from a snapshot frozen when
 * the session opened, with no tools and no graph writes. The panel keeps one
 * active conversation (`session_id` on follow-ups); a `409` refreshes the
 * history and the next question opens/reuses whatever is open now.
 */
export function SideQuestions({ taskId }: { taskId: string }) {
  const [sessions, setSessions] = useState<SideQuestionSession[] | null>(null);
  const [historyError, setHistoryError] = useState<string | null>(null);
  const [question, setQuestion] = useState("");
  const [pending, setPending] = useState(false);
  const [askError, setAskError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [activeSessionId, setActiveSessionId] = useState<string | undefined>(
    undefined,
  );
  /** Sequence token so a slow load cannot overwrite a newer one. */
  const loadSeq = useRef(0);

  const load = useCallback(async (): Promise<SideQuestionSession[] | null> => {
    const seq = ++loadSeq.current;
    try {
      const list = newestSideSessions(await listSideQuestions(taskId));
      if (seq === loadSeq.current) {
        setSessions(list);
        setHistoryError(null);
      }
      return list;
    } catch (err) {
      if (seq === loadSeq.current) {
        setHistoryError(describeApiError(err));
      }
      return null;
    }
  }, [taskId]);

  useEffect(() => {
    let cancelled = false;
    setSessions(null);
    setHistoryError(null);
    setAskError(null);
    setNotice(null);
    setActiveSessionId(undefined);
    void load().then((list) => {
      if (!cancelled && list !== null) {
        setActiveSessionId(openSideSessionId(list));
      }
    });
    return () => {
      cancelled = true;
    };
  }, [load]);

  const submit = async (event: FormEvent<HTMLFormElement>): Promise<void> => {
    event.preventDefault();
    const trimmed = question.trim();
    if (pending || trimmed === "") {
      return;
    }

    setPending(true);
    setAskError(null);
    setNotice(null);
    try {
      const response = await askSideQuestion(taskId, trimmed, activeSessionId);
      setSessions((current) => appendSideQuestion(current ?? [], response));
      setActiveSessionId(response.session_id);
      setQuestion("");
    } catch (err) {
      if (err instanceof ApiError && err.isConflict()) {
        // The session closed (another ask opened a new one). Refresh the
        // history, keep the typed question, and let the next ask continue.
        const list = await load();
        setActiveSessionId(list === null ? undefined : openSideSessionId(list));
        setNotice(
          "This conversation changed; history refreshed. Your question is still here.",
        );
      } else {
        // Includes 502: the failed attempt was persisted server-side, but
        // the question stays in the box so it can be retried.
        setAskError(describeApiError(err));
      }
    } finally {
      setPending(false);
    }
  };

  return (
    <Card id="side-questions">
      <CardHeader className="border-b border-border pb-4">
        <div className="flex flex-wrap items-start justify-between gap-2">
          <div className="flex items-start gap-2.5">
            <span className="flex h-8 w-8 items-center justify-center rounded-lg bg-accent text-accent-foreground">
              <MessageSquareText className="h-4 w-4" />
            </span>
            <div>
              <CardTitle className="text-base">Ask about this task</CardTitle>
              <CardDescription className="mt-0.5">
                Frozen snapshot · advisory only · cannot change the task.
              </CardDescription>
            </div>
          </div>
          <Badge variant="accent">
            <Sparkles className="h-3 w-3" /> advisory
          </Badge>
        </div>
      </CardHeader>
      <CardContent className="pt-4">
        {historyError !== null ? (
          <div className="errorbox mt-0" role="alert">
            <div className="flex flex-wrap items-center gap-3">
              <span className="min-w-0 flex-1">Could not load questions: {historyError}</span>
              <Button
                type="button"
                size="sm"
                variant="outline"
                className="h-7 border-destructive/30 text-destructive hover:bg-destructive/10 hover:text-destructive"
                onClick={() =>
                  void load().then((list) => {
                    if (list !== null) {
                      setActiveSessionId(openSideSessionId(list));
                    }
                  })
                }
              >
                Retry
              </Button>
            </div>
          </div>
        ) : null}

        {sessions === null && historyError === null ? (
          <div className="space-y-2">
            <Skeleton className="h-16 w-full" />
            <Skeleton className="h-16 w-full" />
          </div>
        ) : null}

        {sessions !== null && sessions.length === 0 ? (
          <div className="rounded-xl border border-dashed border-border bg-surface-subtle px-5 py-8 text-center">
            <Bot className="mx-auto h-5 w-5 text-muted-foreground" />
            <p className="mt-2 text-sm font-medium text-foreground">No questions yet</p>
            <p className="mt-1 text-xs text-muted-foreground">
              Ask what happened, why it finished, or what the evidence was.
            </p>
          </div>
        ) : null}

        {sessions !== null && sessions.length > 0 ? (
          <div className="space-y-5">
            {sessions.map((session) => (
              <section className="border-b border-border pb-5 last:border-0 last:pb-0" key={session.id}>
                <div className="mb-2 flex flex-wrap items-center justify-between gap-2">
                  <span className="flex items-center gap-2 text-[10.5px] font-semibold uppercase tracking-[0.12em] text-muted-foreground">
                    <Bot className="h-3 w-3" /> Session · {formatLocalTimestamp(session.created_at)}
                  </span>
                  <StateBadge state={session.status} />
                </div>
                {session.requests.length === 0 ? (
                  <p className="empty">No questions yet</p>
                ) : null}
                {session.requests.map((request) => (
                  <div className="mt-3 space-y-1.5" key={request.id}>
                    <div className="flex items-start justify-between gap-2">
                      <div className="flex items-start gap-2 text-[13.5px] font-medium leading-5 text-foreground">
                        <span className="mt-1 h-1.5 w-1.5 shrink-0 rounded-full bg-muted-foreground" />
                        <span className="whitespace-pre-wrap break-words">{request.question}</span>
                      </div>
                      <CopyButton
                        className="shrink-0"
                        text={request.question}
                        label="question"
                        size="icon"
                      />
                    </div>
                    {request.status === "error" ? (
                      <div className="side-answer side-answer-error">
                        Could not answer: {request.error ?? "the answerer failed"}
                      </div>
                    ) : (
                      <>
                        <div className="side-answer">{request.answer ?? "…"}</div>
                        {request.answer !== null ? (
                          <div className="flex justify-end">
                            <CopyButton text={request.answer} label="answer" size="icon" />
                          </div>
                        ) : null}
                      </>
                    )}
                    <div className="flex flex-wrap items-center gap-2 text-[11px] text-muted-foreground">
                      <span>{formatLocalTimestamp(request.created_at)}</span>
                      {request.model !== null ? <span className="font-mono">{request.model}</span> : null}
                    </div>
                  </div>
                ))}
              </section>
            ))}
          </div>
        ) : null}

        {notice !== null ? (
          <div className="notice" role="status">
            {notice}
          </div>
        ) : null}

        {askError !== null ? (
          <div className="errorbox" role="alert">
            {askError}
          </div>
        ) : null}

        <form className="mt-4 space-y-2" onSubmit={(event) => void submit(event)}>
          <label className="text-xs font-semibold text-foreground" htmlFor="side-question">
            Question
          </label>
          <Textarea
            id="side-question"
            value={question}
            maxLength={MAX_QUESTION_CHARS}
            rows={3}
            disabled={pending}
            placeholder="Ask what happened, why it finished, or what the evidence was…"
            onChange={(event) => setQuestion(event.target.value)}
          />
          <div className="flex flex-wrap items-center justify-between gap-2">
            <span className="text-[11px] text-muted-foreground">
              {question.length}/{MAX_QUESTION_CHARS}
            </span>
            <Button
              type="submit"
              size="sm"
              disabled={pending || question.trim() === ""}
            >
              <Send className="h-3.5 w-3.5" />
              {pending ? "Asking…" : "Ask"}
            </Button>
          </div>
          <p className="text-[11px] leading-4 text-muted-foreground">
            Answers are advisory text. They are never recorded as facts and
            cannot change goals, intents, or the work graph.
          </p>
        </form>
      </CardContent>
    </Card>
  );
}
