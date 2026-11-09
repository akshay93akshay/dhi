"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { MessageSquareText, Send } from "lucide-react";

import { listInstructions, sendInstruction } from "@/lib/api";
import { formatLocalTimestamp } from "@/lib/localTime";
import {
  instructionStatus,
  taskAcceptsInstructions,
  validateInstructionText,
} from "@/lib/instructions";
import type { Instruction, TaskState } from "@/lib/types";
import { describeApiError } from "@/lib/ui";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { MarkdownMath } from "@/components/markdown-math";

/**
 * Planner instruction panel for the work view.
 *
 * The form queues one instruction and then refreshes the whole history from
 * the API (POST returns one row; GET is the source of truth for delivery
 * state). Instructions are planner-only and never enter worker prompts.
 */
export function InstructionsCard(props: {
  taskId: string;
  taskState: TaskState;
}) {
  const { taskId, taskState } = props;
  const [instructions, setInstructions] = useState<Instruction[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [text, setText] = useState("");
  const [sendError, setSendError] = useState<string | null>(null);
  const [sending, setSending] = useState(false);

  const mounted = useRef(true);
  const seq = useRef(0);
  const taskIdRef = useRef(taskId);
  const accepts = taskAcceptsInstructions(taskState);

  useEffect(() => {
    taskIdRef.current = taskId;
  }, [taskId]);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  const load = useCallback(async (): Promise<void> => {
    const token = ++seq.current;
    setLoading(true);
    try {
      const rows = await listInstructions(taskId);
      if (!mounted.current || token !== seq.current) {
        return;
      }
      setInstructions(rows);
      setLoadError(null);
    } catch (err) {
      if (!mounted.current || token !== seq.current) {
        return;
      }
      setLoadError(describeApiError(err));
    } finally {
      if (mounted.current && token === seq.current) {
        setLoading(false);
      }
    }
  }, [taskId]);

  // Reset local state when the route moves to another task.
  useEffect(() => {
    seq.current += 1;
    setInstructions(null);
    setLoading(false);
    setLoadError(null);
    setText("");
    setSendError(null);
    setSending(false);
  }, [taskId]);

  useEffect(() => {
    void load();
  }, [load]);

  const submit = async (
    event: React.FormEvent<HTMLFormElement>,
  ): Promise<void> => {
    event.preventDefault();
    if (sending || !accepts) {
      return;
    }
    const validation = validateInstructionText(text);
    if (validation !== null) {
      setSendError(validation);
      return;
    }
    setSending(true);
    setSendError(null);
    const requestTaskId = taskId;
    try {
      await sendInstruction(requestTaskId, text.trim());
      if (!mounted.current || taskIdRef.current !== requestTaskId) {
        return;
      }
      setText("");
      // Refresh from the API so the new row and its delivery state appear.
      await load();
    } catch (err) {
      if (mounted.current && taskIdRef.current === requestTaskId) {
        setSendError(describeApiError(err));
      }
    } finally {
      if (mounted.current && taskIdRef.current === requestTaskId) {
        setSending(false);
      }
    }
  };

  return (
    <Card>
      <CardHeader className="flex-row flex-wrap items-start justify-between gap-3 border-b border-border pb-4">
        <div className="flex items-start gap-2.5">
          <span className="flex h-8 w-8 items-center justify-center rounded-lg bg-accent text-accent-foreground">
            <MessageSquareText className="h-4 w-4" />
          </span>
          <div>
            <CardTitle>Instructions</CardTitle>
            <CardDescription className="mt-0.5">
              Planner-only notes; workers never see them.
            </CardDescription>
          </div>
        </div>
        {instructions !== null ? (
          <Badge variant="outline" className="font-mono font-normal">
            {instructions.length}
          </Badge>
        ) : null}
      </CardHeader>
      <CardContent className="space-y-3 pt-4">
        <div>
          <h3 className="type-label font-semibold uppercase tracking-[0.12em] text-muted-foreground">
            Instruction history
          </h3>

          {loadError !== null ? (
            <div className="errorbox mt-2" role="alert">
              Could not load instructions: {loadError}
            </div>
          ) : null}

          {loading && instructions === null ? (
            <p className="empty mt-2">Loading instructions…</p>
          ) : null}

          {instructions !== null && instructions.length === 0 ? (
            <p className="mt-2 rounded-lg border border-dashed border-border bg-surface-subtle px-3.5 py-3 type-meta text-muted-foreground">
              No instructions yet.
            </p>
          ) : null}

          {instructions !== null && instructions.length > 0 ? (
            <ul className="mt-2 space-y-1.5">
              {instructions.map((instruction) => {
                const status = instructionStatus(instruction);
                return (
                  <li
                    className="rounded-md border border-border bg-surface-subtle px-4 py-3"
                    key={instruction.id}
                  >
                    <MarkdownMath
                      className="type-body text-foreground [&_p+p]:mt-1.5 [&_ul]:my-1 [&_ol]:my-1 [&_li]:my-0.5 [&_.katex-display]:my-1"
                      text={instruction.text}
                    />
                    <p className="mt-1.5 flex flex-wrap items-center gap-x-2 gap-y-1 type-meta text-muted-foreground">
                      <Badge
                        className="font-normal"
                        variant={status === "delivered" ? "success" : "outline"}
                      >
                        {status}
                      </Badge>
                      <span>{formatLocalTimestamp(instruction.created_at)}</span>
                      {instruction.delivered_at !== null ? (
                        <>
                          <span aria-hidden="true">·</span>
                          <span>
                            delivered{" "}
                            {formatLocalTimestamp(instruction.delivered_at)}
                          </span>
                        </>
                      ) : null}
                    </p>
                  </li>
                );
              })}
            </ul>
          ) : null}
        </div>

        <form className="border-t border-border pt-3" onSubmit={submit}>
          <label className="sr-only" htmlFor="instruction-text">
            Instruction
          </label>
          <div className="flex flex-wrap items-center gap-2">
            <Input
              id="instruction-text"
              className="h-10 min-w-0 flex-1 type-body"
              value={text}
              placeholder="Tell the planner what to do next…"
              onChange={(event) => {
                setText(event.target.value);
                if (sendError !== null) {
                  setSendError(null);
                }
              }}
              disabled={sending || !accepts}
            />
            <Button
              className="h-10 rounded-full"
              disabled={sending || !accepts}
              size="sm"
              type="submit"
            >
              <Send className="h-3.5 w-3.5" />
              {sending ? "Sending…" : "Send"}
            </Button>
          </div>
          {!accepts ? (
            <p className="mt-1.5 type-meta text-muted-foreground">
              This task is terminal, so instructions are disabled.
            </p>
          ) : sendError !== null ? (
            <p
              className="mt-1.5 type-meta text-destructive"
              role="alert"
            >
              {sendError}
            </p>
          ) : null}
        </form>

      </CardContent>
    </Card>
  );
}
