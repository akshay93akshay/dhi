"use client";

import { useEffect, useState, type FormEvent } from "react";
import { Gauge } from "lucide-react";

import { ApiError, updateTaskCapacity } from "@/lib/api";
import type { Task } from "@/lib/types";
import { describeApiError } from "@/lib/ui";
import {
  buildCapacityPatch,
  CAPACITY_FIELDS,
  capacityInputs,
  capacityLimitText,
  capacitySummary,
  type CapacityInputs,
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
import { Input } from "@/components/ui/input";

/**
 * Compact DHI+ capacity ledger editor for the work view.
 *
 * The four inputs seed from the task's current limits; blank means "engine
 * default" on create, and on submit blank/unchanged fields are omitted so the
 * server keeps its stored value. The server returns the updated task, which
 * replaces the local copy immediately.
 */
export function CapacityCard(props: {
  task: Task;
  onTaskUpdated: (task: Task) => void;
}) {
  const { task, onTaskUpdated } = props;
  const [draft, setDraft] = useState<CapacityInputs>(() =>
    capacityInputs(task.capacity),
  );
  const [pending, setPending] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  // Polling hands us a fresh task object every few seconds. Reseed only when
  // the ledger itself changed, so typing is never interrupted by a poll.
  const capacityKey = JSON.stringify(task.capacity ?? null);
  useEffect(() => {
    setDraft(capacityInputs(task.capacity));
  }, [task.id, capacityKey]);

  const submit = async (event: FormEvent<HTMLFormElement>): Promise<void> => {
    event.preventDefault();
    if (pending) {
      return;
    }
    setNotice(null);
    setError(null);

    const result = buildCapacityPatch(draft, task.capacity);
    if (!result.ok) {
      setError(result.error);
      return;
    }

    setPending(true);
    try {
      const updated = await updateTaskCapacity(task.id, result.patch);
      onTaskUpdated(updated);
      setDraft(capacityInputs(updated.capacity));
      setNotice("Capacity updated");
    } catch (err) {
      if (err instanceof ApiError && err.isConflict()) {
        setError("Task is terminal; capacity cannot be extended");
      } else {
        setError(describeApiError(err));
      }
    } finally {
      setPending(false);
    }
  };

  return (
    <Card>
      <CardHeader className="border-b border-border pb-3">
        <div className="flex flex-wrap items-start justify-between gap-2">
          <div className="flex items-start gap-2.5">
            <span className="flex h-8 w-8 items-center justify-center rounded-lg bg-accent text-accent-foreground">
              <Gauge className="h-4 w-4" />
            </span>
            <div>
              <CardTitle className="text-base">Capacity</CardTitle>
              <CardDescription className="mt-0.5">
                Current: {capacitySummary(task.capacity)}
              </CardDescription>
            </div>
          </div>
          <Badge variant="outline" className="font-normal">
            DHI<sup>+</sup>
          </Badge>
        </div>
      </CardHeader>
      <CardContent className="pt-4">
        <form className="space-y-3" onSubmit={(event) => void submit(event)}>
          <div className="grid grid-cols-2 gap-2.5 lg:grid-cols-4">
            {CAPACITY_FIELDS.map((field) => (
              <label className="space-y-1.5" key={field.key}>
                <span className="block text-[10.5px] font-semibold uppercase tracking-[0.08em] text-muted-foreground">
                  {field.label}
                </span>
                <Input
                  aria-label={field.label}
                  className="h-8 font-mono text-[12.5px]"
                  disabled={pending}
                  inputMode="numeric"
                  min={1}
                  placeholder={capacityLimitText(task.capacity?.[field.key])}
                  step={1}
                  type="number"
                  value={draft[field.key]}
                  onChange={(event) =>
                    setDraft((current) => ({
                      ...current,
                      [field.key]: event.target.value,
                    }))
                  }
                />
              </label>
            ))}
          </div>
          <div className="flex flex-wrap items-center gap-x-3 gap-y-1.5">
            <Button
              className="h-8 rounded-full"
              disabled={pending}
              size="sm"
              type="submit"
            >
              {pending ? "Extending…" : "Extend capacity"}
            </Button>
            <p className="text-[11px] text-muted-foreground">
              Blank fields keep their stored limit.
            </p>
            {notice !== null ? (
              <span className="text-[11.5px] font-medium text-success" role="status">
                {notice}
              </span>
            ) : null}
            {error !== null ? (
              <span className="text-[11.5px] font-medium text-destructive" role="alert">
                {error}
              </span>
            ) : null}
          </div>
        </form>
      </CardContent>
    </Card>
  );
}
