"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useRouter } from "next/navigation";
import { ArrowRight, CornerDownLeft, Search, Wrench } from "lucide-react";

import { listTasks } from "@/lib/api";
import type { Task } from "@/lib/types";
import { plainPreview } from "@/lib/preview";
import { describeApiError, shortenId } from "@/lib/ui";
import { Badge } from "@/components/ui/badge";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { StateBadge } from "@/components/ui/state-badge";
import { FLAT_NAV_ITEMS } from "@/components/nav-config";

type Result =
  | {
      kind: "page";
      key: string;
      label: string;
      hint: string;
      href: string;
      icon: React.ReactNode;
    }
  | {
      kind: "task";
      key: string;
      label: string;
      hint: string;
      href: string;
      state: string;
    };

/**
 * Command palette: jumps to every shipped surface and searches the real task
 * list. Tasks are loaded lazily the first time the palette opens; failures
 * degrade to page navigation rather than blocking the dialog.
 */
export function CommandPalette() {
  const router = useRouter();
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [active, setActive] = useState(0);
  const [tasks, setTasks] = useState<Task[] | null>(null);
  const [taskError, setTaskError] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  const loadTasks = useCallback(async (): Promise<void> => {
    try {
      setTasks(await listTasks());
      setTaskError(null);
    } catch (error) {
      setTaskError(describeApiError(error));
    }
  }, []);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent): void => {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
        event.preventDefault();
        setOpen((current) => !current);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);

  useEffect(() => {
    if (!open) {
      setQuery("");
      setActive(0);
      return;
    }
    if (tasks === null) {
      void loadTasks();
    }
    const timer = window.setTimeout(() => inputRef.current?.focus(), 30);
    return () => window.clearTimeout(timer);
  }, [open, tasks, loadTasks]);

  const results = useMemo<Result[]>(() => {
    const needle = query.trim().toLowerCase();
    const pages: Result[] = FLAT_NAV_ITEMS.filter((item) => {
      if (needle === "") {
        return true;
      }
      return (
        item.label.toLowerCase().includes(needle) ||
        item.hint.toLowerCase().includes(needle)
      );
    }).map((item) => {
      const Icon = item.icon;
      return {
        kind: "page",
        key: `page:${item.href}`,
        label: item.label,
        hint: item.hint,
        href: item.href,
        icon: <Icon className="h-4 w-4" />,
      };
    });

    const taskResults: Result[] = (tasks ?? [])
      .filter((task) => {
        if (needle === "") {
          return true;
        }
        return (
          task.goal.toLowerCase().includes(needle) ||
          task.id.toLowerCase().includes(needle)
        );
      })
      .slice(0, 7)
      .map((task) => ({
        kind: "task",
        key: `task:${task.id}`,
        label: plainPreview(task.goal),
        hint: `#${shortenId(task.id)}`,
        href: `/tasks/${task.id}`,
        state: task.state,
      }));

    return [...pages, ...taskResults];
  }, [query, tasks]);

  useEffect(() => {
    setActive(0);
  }, [query]);

  useEffect(() => {
    if (active >= results.length) {
      setActive(Math.max(0, results.length - 1));
    }
  }, [results.length, active]);

  const go = (result: Result): void => {
    setOpen(false);
    router.push(result.href);
  };

  const onKeyDown = (event: React.KeyboardEvent<HTMLInputElement>): void => {
    if (event.key === "ArrowDown") {
      event.preventDefault();
      setActive((current) => (results.length === 0 ? 0 : (current + 1) % results.length));
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      setActive((current) =>
        results.length === 0 ? 0 : (current - 1 + results.length) % results.length,
      );
    } else if (event.key === "Enter") {
      event.preventDefault();
      const result = results[active];
      if (result !== undefined) {
        go(result);
      }
    }
  };

  return (
    <>
      <button
        type="button"
        onClick={() => setOpen(true)}
        className="group flex h-9 items-center gap-2 rounded-full border border-border bg-card px-3 type-label text-muted-foreground shadow-none transition-colors hover:border-input hover:text-foreground sm:min-w-[190px]"
        aria-label="Search pages and tasks"
      >
        <Search className="h-3.5 w-3.5" />
        <span className="hidden sm:inline">Search…</span>
        <kbd className="ml-auto hidden items-center gap-0.5 rounded border border-border bg-muted px-1.5 py-0.5 type-mono text-muted-foreground sm:inline-flex">
          ⌘K
        </kbd>
      </button>

      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent className="top-[12%] max-w-xl translate-y-0 gap-0 overflow-hidden p-0 [&>button]:hidden">
          <DialogTitle className="sr-only">Search DHI</DialogTitle>
          <DialogDescription className="sr-only">
            Jump to a page or search existing tasks.
          </DialogDescription>

          <div className="flex items-center gap-2 border-b border-border px-4">
            <Search className="h-4 w-4 shrink-0 text-muted-foreground" />
            <Input
              ref={inputRef}
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              onKeyDown={onKeyDown}
              placeholder="Search pages and tasks…"
              aria-label="Search pages and tasks"
              className="h-12 border-0 bg-transparent px-0 type-body shadow-none focus-visible:ring-0"
            />
            <kbd className="hidden rounded border border-border bg-muted px-1.5 py-0.5 type-mono text-muted-foreground sm:block">
              ESC
            </kbd>
          </div>

          <div className="max-h-[min(60vh,26rem)] overflow-y-auto p-2">
            {results.length === 0 ? (
              <div className="px-3 py-10 text-center type-body text-muted-foreground">
                Nothing matches “{query}”.
                {taskError !== null ? ` Tasks unavailable: ${taskError}` : null}
              </div>
            ) : (
              <ul className="space-y-0.5" role="listbox" aria-label="Results">
                {results.map((result, index) => (
                  <li key={result.key}>
                    <button
                      type="button"
                      role="option"
                      aria-selected={index === active}
                      onMouseEnter={() => setActive(index)}
                      onClick={() => go(result)}
                      className={`flex w-full items-center gap-3 rounded-lg px-3 py-2.5 text-left transition-colors ${
                        index === active
                          ? "bg-secondary text-secondary-foreground"
                          : "text-foreground hover:bg-secondary/60"
                      }`}
                    >
                      <span className="flex h-7 w-7 shrink-0 items-center justify-center rounded-md border border-border bg-card text-muted-foreground">
                        {result.kind === "page" ? (
                          result.icon
                        ) : (
                          <Wrench className="h-3.5 w-3.5" />
                        )}
                      </span>
                      <span className="min-w-0 flex-1">
                        <span className="block truncate type-table font-medium">
                          {result.label}
                        </span>
                        <span className="mt-0.5 block truncate type-meta text-muted-foreground">
                          {result.hint}
                        </span>
                      </span>
                      {result.kind === "task" ? (
                        <StateBadge state={result.state} />
                      ) : (
                        <ArrowRight className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
                      )}
                    </button>
                  </li>
                ))}
              </ul>
            )}
          </div>

          <div className="flex items-center justify-between gap-3 border-t border-border bg-surface-subtle px-4 py-2.5 type-label text-muted-foreground">
            <span className="flex items-center gap-1.5">
              <CornerDownLeft className="h-3 w-3" /> open
              <span className="mx-1 text-border">·</span>
              ↑↓ navigate
            </span>
            <Badge variant="outline" className="font-normal">
              {tasks === null ? "Loading tasks…" : `${tasks.length} tasks`}
            </Badge>
          </div>
        </DialogContent>
      </Dialog>
    </>
  );
}
