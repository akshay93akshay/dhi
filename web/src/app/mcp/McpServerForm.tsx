"use client";

import { useState } from "react";

import { ApiError, addMcpServer, updateMcpServer } from "@/lib/api";
import {
  buildMcpServerInput,
  splitStdioTarget,
  type McpServerFormValues,
} from "@/lib/toolAdmin";
import type {
  McpReloadReport,
  McpServerStatus,
  McpServerSummary,
  McpTransportKind,
} from "@/lib/types";
import { describeApiError } from "@/lib/ui";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Textarea } from "@/components/ui/textarea";

export type McpServerFormTarget =
  | { mode: "add" }
  | {
      mode: "edit";
      name: string;
      summary?: McpServerSummary;
      status?: McpServerStatus;
    };

function toTransport(value: string | undefined): McpTransportKind {
  return value === "stdio" || value === "sse" ? value : "http";
}

function initialValues(
  target: McpServerFormTarget,
): McpServerFormValues {
  if (target.mode === "add") {
    return {
      name: "",
      transport: "stdio",
      command: "",
      args: "",
      env: "",
      url: "",
      headers: "",
      timeoutSecs: "",
      insecure: false,
      enabled: true,
    };
  }
  const transport = toTransport(
    target.summary?.transport ?? target.status?.transport,
  );
  const rawTarget = target.summary?.target ?? "";
  const stdio = splitStdioTarget(transport === "stdio" ? rawTarget : "");
  return {
    name: target.name,
    transport,
    command: stdio.command,
    args: stdio.args.join("\n"),
    env: "",
    url: transport === "stdio" ? "" : rawTarget,
    headers: "",
    timeoutSecs: String(
      target.summary?.timeout_secs ?? target.status?.call_timeout_secs ?? "",
    ),
    insecure: false,
    enabled:
      target.summary?.enabled ?? target.status?.enabled ?? true,
  };
}

const TRANSPORT_LABELS: Record<McpTransportKind, string> = {
  stdio: "stdio (child process)",
  http: "http (streamable HTTP)",
  sse: "sse (legacy)",
};

/**
 * Add/edit dialog for one MCP server. The API persists then reloads; a `502`
 * means the entry was saved but the target did not connect, so the message
 * says that explicitly and the page can refresh the list afterward.
 */
export function McpServerFormDialog({
  target,
  onClose,
  onSaved,
}: {
  target: McpServerFormTarget;
  onClose: () => void;
  onSaved: (message: string, report: McpReloadReport) => void;
}) {
  const [form, setForm] = useState<McpServerFormValues>(() =>
    initialValues(target),
  );
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  const isEdit = target.mode === "edit";
  const existingHeaderNames =
    target.mode === "edit" && target.summary?.headers !== undefined
      ? Object.keys(target.summary.headers).sort()
      : [];

  function update<K extends keyof McpServerFormValues>(
    key: K,
    value: McpServerFormValues[K],
  ): void {
    setForm((current) => ({ ...current, [key]: value }));
  }

  async function handleSubmit(
    event: React.FormEvent<HTMLFormElement>,
  ): Promise<void> {
    event.preventDefault();
    const built = buildMcpServerInput(form, target.mode);
    if (!built.ok) {
      setError(built.error);
      return;
    }
    setSaving(true);
    setError(null);
    try {
      if (target.mode === "add") {
        const response = await addMcpServer(built.input);
        onSaved(`Added MCP server ${response.server.name}.`, response.reload);
      } else {
        const response = await updateMcpServer(target.name, built.input);
        onSaved(`Saved MCP server ${response.server.name}.`, response.reload);
      }
    } catch (cause) {
      const message = describeApiError(cause);
      if (cause instanceof ApiError && cause.status === 502) {
        setError(
          `${message} The config was saved before the connection attempt failed; close this dialog and reload the list to confirm the stored state.`,
        );
      } else {
        setError(message);
      }
    } finally {
      setSaving(false);
    }
  }

  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open && !saving) {
          onClose();
        }
      }}
    >
      <DialogContent className="max-w-2xl">
        <DialogHeader>
          <DialogTitle>
            {isEdit ? `Edit ${target.name}` : "Add MCP server"}
          </DialogTitle>
          <DialogDescription>
            {isEdit
              ? "The write replaces the whole stored entry. The list summary masks header values and never exposes env vars, insecure, visible_to, tool_visibility, or deferred_threshold, so re-enter what must be kept (or manage those fields in ops/mcp.json)."
              : "The entry is written to the MCP config file first, then reloaded live. Secret header values are never echoed back by the API."}
          </DialogDescription>
        </DialogHeader>

        <form className="space-y-4" onSubmit={(event) => void handleSubmit(event)}>
          {isEdit && target.summary === undefined ? (
            <div className="rounded-md border border-warning/30 bg-warning/8 px-3 py-2 type-body text-warning">
              The masked config summary for this server is missing from the
              list response. Re-enter its transport, target, and timeout.
            </div>
          ) : null}
          <div className="grid gap-4 sm:grid-cols-2">
            <div className="space-y-2">
              <Label htmlFor="mcp-name">Name</Label>
              <Input
                id="mcp-name"
                value={form.name}
                onChange={(event) => update("name", event.target.value)}
                disabled={isEdit}
                placeholder="github"
                autoComplete="off"
              />
              {isEdit ? (
                <p className="type-meta text-muted-foreground">
                  Rename by deleting and adding a new server.
                </p>
              ) : null}
            </div>

            <div className="space-y-2">
              <Label htmlFor="mcp-transport">Transport</Label>
              <Select
                value={form.transport}
                onValueChange={(value) =>
                  update("transport", value as McpTransportKind)
                }
              >
                <SelectTrigger id="mcp-transport">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="stdio">
                    {TRANSPORT_LABELS.stdio}
                  </SelectItem>
                  <SelectItem value="http">{TRANSPORT_LABELS.http}</SelectItem>
                  <SelectItem value="sse">{TRANSPORT_LABELS.sse}</SelectItem>
                </SelectContent>
              </Select>
            </div>
          </div>

          {form.transport === "stdio" ? (
            <div className="space-y-4">
              <div className="space-y-2">
                <Label htmlFor="mcp-command">Command</Label>
                <Input
                  id="mcp-command"
                  value={form.command}
                  onChange={(event) => update("command", event.target.value)}
                  placeholder="npx"
                  autoComplete="off"
                />
              </div>
              <div className="space-y-2">
                <Label htmlFor="mcp-args">Arguments (one per line)</Label>
                <Textarea
                  id="mcp-args"
                  value={form.args}
                  onChange={(event) => update("args", event.target.value)}
                  placeholder={"-y\n@modelcontextprotocol/server-github"}
                  className="font-mono"
                />
                <p className="type-meta text-muted-foreground">
                  One argument per line preserves spaces inside an argument.
                  {isEdit
                    ? " The list summary joins the stored command line with spaces, so re-enter complex arguments here."
                    : ""}
                </p>
              </div>
              <div className="space-y-2">
                <Label htmlFor="mcp-env">Environment (KEY=VALUE per line)</Label>
                <Textarea
                  id="mcp-env"
                  value={form.env}
                  onChange={(event) => update("env", event.target.value)}
                  placeholder="GITHUB_TOKEN=…"
                  className="font-mono"
                />
              </div>
            </div>
          ) : (
            <div className="space-y-4">
              <div className="space-y-2">
                <Label htmlFor="mcp-url">URL</Label>
                <Input
                  id="mcp-url"
                  value={form.url}
                  onChange={(event) => update("url", event.target.value)}
                  placeholder="https://mcp.example.com/mcp"
                  autoComplete="off"
                />
              </div>
              <div className="space-y-2">
                <Label htmlFor="mcp-headers">
                  Headers (KEY=VALUE per line)
                </Label>
                <Textarea
                  id="mcp-headers"
                  value={form.headers}
                  onChange={(event) => update("headers", event.target.value)}
                  placeholder="Authorization=Bearer …"
                  className="font-mono"
                />
                <p className="type-meta text-muted-foreground">
                  Header values are write-only; the server returns only masked
                  summaries and this page never renders stored values.
                </p>
                {existingHeaderNames.length > 0 ? (
                  <p className="type-meta text-muted-foreground">
                    Currently configured header names:{" "}
                    <span className="font-mono">
                      {existingHeaderNames.join(", ")}
                    </span>
                    . Re-enter the ones that must be kept.
                  </p>
                ) : null}
              </div>
              <label className="flex cursor-pointer items-center gap-2 type-body">
                <Checkbox
                  checked={form.insecure}
                  onCheckedChange={(checked) =>
                    update("insecure", checked === true)
                  }
                />
                Allow plain HTTP / skip TLS verification (insecure)
              </label>
            </div>
          )}

          <div className="grid gap-4 sm:grid-cols-2">
            <div className="space-y-2">
              <Label htmlFor="mcp-timeout">Call timeout (seconds)</Label>
              <Input
                id="mcp-timeout"
                type="number"
                min={1}
                step={1}
                value={form.timeoutSecs}
                onChange={(event) =>
                  update("timeoutSecs", event.target.value)
                }
                placeholder="60"
              />
              <p className="type-meta text-muted-foreground">
                Blank uses the server default (60 s).
              </p>
            </div>
            <label className="flex cursor-pointer items-center gap-2 self-start pt-6 type-body">
              <Checkbox
                checked={form.enabled}
                onCheckedChange={(checked) =>
                  update("enabled", checked === true)
                }
              />
              Enabled
            </label>
          </div>

          {error !== null ? (
            <div
              role="alert"
              className="rounded-md border border-destructive/30 bg-destructive/8 px-3 py-2 type-body text-destructive"
            >
              {error}
            </div>
          ) : null}

          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              onClick={onClose}
              disabled={saving}
            >
              Cancel
            </Button>
            <Button type="submit" disabled={saving}>
              {saving ? "Saving…" : isEdit ? "Save server" : "Add server"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
