"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  deleteMcpServer,
  getMcpServers,
  refreshMcpServer,
  reloadMcpServers,
} from "@/lib/api";
import { describeMcpReload } from "@/lib/toolAdmin";
import type {
  McpReloadReport,
  McpServerStatus,
  McpServerSummary,
  McpServersResponse,
} from "@/lib/types";
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
import { OpsPage } from "@/components/OpsPage";
import { StatusBanner } from "@/components/StatusBanner";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import {
  McpServerFormDialog,
  type McpServerFormTarget,
} from "./McpServerForm";

type ServerRow = {
  name: string;
  summary?: McpServerSummary;
  status?: McpServerStatus;
};

export function McpClient() {
  const [data, setData] = useState<McpServersResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [lastReload, setLastReload] = useState<McpReloadReport | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [deleteArmed, setDeleteArmed] = useState<string | null>(null);
  const [form, setForm] = useState<McpServerFormTarget | null>(null);

  const mounted = useRef(true);
  const seq = useRef(0);

  const load = useCallback(async (): Promise<void> => {
    const current = ++seq.current;
    setRefreshing(true);
    try {
      const next = await getMcpServers();
      if (mounted.current && current === seq.current) {
        setData(next);
        setLoadError(null);
      }
    } catch (error) {
      if (mounted.current && current === seq.current) {
        setLoadError(describeApiError(error));
      }
    } finally {
      if (mounted.current && current === seq.current) {
        setRefreshing(false);
        setLoading(false);
      }
    }
  }, []);

  useEffect(() => {
    mounted.current = true;
    void load();
    return () => {
      mounted.current = false;
    };
  }, [load]);

  const rows = useMemo<ServerRow[]>(() => {
    const byName = new Map<string, ServerRow>();
    for (const status of data?.servers ?? []) {
      byName.set(status.name, { name: status.name, status });
    }
    for (const summary of data?.config ?? []) {
      const existing = byName.get(summary.name);
      if (existing === undefined) {
        byName.set(summary.name, { name: summary.name, summary });
      } else {
        existing.summary = summary;
      }
    }
    return [...byName.values()].sort((left, right) =>
      left.name < right.name ? -1 : left.name > right.name ? 1 : 0,
    );
  }, [data]);

  const controlsDisabled = busy !== null;

  async function handleReload(): Promise<void> {
    setBusy("reload");
    setActionError(null);
    setNotice(null);
    try {
      const report = await reloadMcpServers();
      setLastReload(report);
      setNotice(
        `Reloaded the MCP config. ${describeMcpReload(report)}.`,
      );
      await load();
    } catch (error) {
      setActionError(`Could not reload the MCP config: ${describeApiError(error)}`);
    } finally {
      setBusy(null);
    }
  }

  async function handleRefresh(name: string): Promise<void> {
    setBusy(`refresh:${name}`);
    setActionError(null);
    setNotice(null);
    try {
      const result = await refreshMcpServer(name);
      setNotice(
        `Refreshed ${name}: ${result.tools.length} discovered ${
          result.tools.length === 1 ? "tool" : "tools"
        }.`,
      );
      await load();
    } catch (error) {
      setActionError(`Could not refresh ${name}: ${describeApiError(error)}`);
    } finally {
      setBusy(null);
    }
  }

  async function handleDelete(name: string): Promise<void> {
    setBusy(`delete:${name}`);
    setActionError(null);
    setNotice(null);
    try {
      const response = await deleteMcpServer(name);
      setDeleteArmed(null);
      setLastReload(response.reload);
      setNotice(`Deleted MCP server ${response.deleted}.`);
      await load();
    } catch (error) {
      setActionError(`Could not delete ${name}: ${describeApiError(error)}`);
    } finally {
      setBusy(null);
    }
  }

  return (
    <OpsPage
      current="mcp"
      title="MCP servers"
      description="Live MCP configuration and health."
      actions={
        <>
          <Button
            type="button"
            onClick={() => {
              setActionError(null);
              setNotice(null);
              setForm({ mode: "add" });
            }}
            disabled={controlsDisabled}
          >
            Add server
          </Button>
          <Button
            type="button"
            variant="outline"
            onClick={() => void handleReload()}
            disabled={controlsDisabled}
          >
            {busy === "reload" ? "Reloading…" : "Reload config"}
          </Button>
        </>
      }
    >
      <div className="space-y-4">
        <StatusBanner tone="info" title="Config file is the source of truth">
          Servers come from <code>ops/mcp.json</code> (override with{" "}
          <code>DHI_MCP_CONFIG</code>). Header values are masked by the server
          and this page renders header names only. Editing a server replaces
          the whole entry; headers, env vars, and DHI-specific extras not
          shown in the list summary must be re-entered or managed in the file.
        </StatusBanner>

        {loading ? (
          <Card>
            <CardContent className="py-6 type-body text-muted-foreground">
              Loading MCP servers…
            </CardContent>
          </Card>
        ) : null}

        {loadError !== null ? (
          <StatusBanner
            tone="error"
            title={
              data === null
                ? "Could not load MCP servers"
                : "Refresh failed"
            }
            action={
              <Button
                type="button"
                size="sm"
                variant="outline"
                onClick={() => void load()}
                disabled={refreshing}
              >
                Retry
              </Button>
            }
          >
            {loadError}
            {data !== null
              ? " Showing the last successfully loaded server list."
              : null}
          </StatusBanner>
        ) : null}

        {actionError !== null ? (
          <StatusBanner
            tone="error"
            title="MCP action failed"
            action={
              <Button
                type="button"
                size="sm"
                variant="outline"
                onClick={() => setActionError(null)}
              >
                Dismiss
              </Button>
            }
          >
            {actionError}
          </StatusBanner>
        ) : null}

        {notice !== null ? (
          <StatusBanner
            tone="success"
            action={
              <Button
                type="button"
                size="sm"
                variant="outline"
                onClick={() => setNotice(null)}
              >
                Dismiss
              </Button>
            }
          >
            {notice}
          </StatusBanner>
        ) : null}

        {lastReload !== null ? (
          <Card>
            <CardHeader>
              <div className="flex flex-wrap items-start justify-between gap-2">
                <div>
                  <CardTitle>Last reload</CardTitle>
                  <CardDescription>{describeMcpReload(lastReload)}</CardDescription>
                </div>
                <Button
                  type="button"
                  size="sm"
                  variant="ghost"
                  onClick={() => setLastReload(null)}
                >
                  Dismiss
                </Button>
              </div>
            </CardHeader>
            <CardContent className="space-y-2 type-body">
              {!lastReload.runtime_attached ? (
                <div className="text-warning">
                  The MCP runtime is not attached; the config was applied but
                  no new servers could connect.
                </div>
              ) : null}
              <div className="flex flex-wrap gap-4 type-meta text-muted-foreground">
                <span>Connected: {lastReload.connected.length}</span>
                <span>Reconnected: {lastReload.reconnected.length}</span>
                <span>Disconnected: {lastReload.disconnected.length}</span>
                <span>Kept: {lastReload.kept.length}</span>
                <span>Failed: {lastReload.failed.length}</span>
              </div>
              {lastReload.failed.length > 0 ? (
                <ul className="list-disc space-y-1 pl-5 text-destructive">
                  {lastReload.failed.map((failure) => (
                    <li key={failure.name}>
                      <span className="type-mono">{failure.name}</span>:{" "}
                      {failure.error}
                    </li>
                  ))}
                </ul>
              ) : null}
            </CardContent>
          </Card>
        ) : null}

        {data !== null ? (
          <Card>
            <CardHeader>
              <CardTitle>Configured servers</CardTitle>
              <CardDescription>
                {rows.length} {rows.length === 1 ? "server" : "servers"}.
              </CardDescription>
            </CardHeader>
            <CardContent className="p-0">
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>Server</TableHead>
                    <TableHead>Transport</TableHead>
                    <TableHead>Enabled</TableHead>
                    <TableHead>Connected</TableHead>
                    <TableHead>Tools</TableHead>
                    <TableHead>Target</TableHead>
                    <TableHead>Headers</TableHead>
                    <TableHead>Last error</TableHead>
                    <TableHead>Actions</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {rows.length === 0 ? (
                    <TableRow>
                      <TableCell
                        colSpan={9}
                        className="py-6 type-table text-muted-foreground"
                      >
                        No MCP servers are configured. Add one here, or edit
                        ops/mcp.json and reload.
                      </TableCell>
                    </TableRow>
                  ) : (
                    rows.map((row) => {
                      const enabled =
                        row.summary?.enabled ?? row.status?.enabled ?? false;
                      const connected = row.status?.connected ?? false;
                      const target = row.summary?.target;
                      const transport =
                        row.summary?.transport ?? row.status?.transport;
                      const timeout =
                        row.summary?.timeout_secs ??
                        row.status?.call_timeout_secs;
                      const headerNames =
                        row.summary?.headers === undefined
                          ? []
                          : Object.keys(row.summary.headers).sort();
                      const tools = row.status?.tools ?? [];
                      const isRefreshing = busy === `refresh:${row.name}`;
                      const isDeleting = busy === `delete:${row.name}`;

                      return (
                        <TableRow key={row.name}>
                          <TableCell className="type-mono font-semibold">
                            {row.name}
                          </TableCell>
                          <TableCell className="whitespace-nowrap type-table">
                            {transport ?? "—"}
                            {timeout !== undefined ? (
                              <span className="text-muted-foreground">
                                {" "}
                                · {timeout}s
                              </span>
                            ) : null}
                          </TableCell>
                          <TableCell>
                            <Badge variant={enabled ? "success" : "secondary"}>
                              {enabled ? "enabled" : "disabled"}
                            </Badge>
                          </TableCell>
                          <TableCell>
                            {row.status === undefined ? (
                              <Badge variant="warning">unknown</Badge>
                            ) : connected ? (
                              <Badge variant="success">connected</Badge>
                            ) : (
                              <Badge variant="destructive">disconnected</Badge>
                            )}
                          </TableCell>
                          <TableCell
                            className="type-table"
                            title={tools.map((tool) => tool.name).join(", ")}
                          >
                            {row.status === undefined ? "—" : tools.length}
                          </TableCell>
                          <TableCell className="max-w-[20rem]">
                            <span className="type-mono break-all">
                              {target ?? "—"}
                            </span>
                          </TableCell>
                          <TableCell className="max-w-[14rem] type-table text-muted-foreground">
                            {headerNames.length === 0
                              ? "—"
                              : headerNames.join(", ")}
                          </TableCell>
                          <TableCell className="max-w-[18rem] break-words type-table text-destructive">
                            {row.status?.last_error ?? "—"}
                          </TableCell>
                          <TableCell>
                            <div className="flex flex-wrap gap-2">
                              <Button
                                type="button"
                                size="sm"
                                variant="outline"
                                onClick={() => void handleRefresh(row.name)}
                                disabled={controlsDisabled || !connected}
                                title={
                                  connected
                                    ? undefined
                                    : "Refresh requires a connected server."
                                }
                              >
                                {isRefreshing ? "Refreshing…" : "Refresh"}
                              </Button>
                              <Button
                                type="button"
                                size="sm"
                                variant="outline"
                                onClick={() => {
                                  setActionError(null);
                                  setNotice(null);
                                  setForm({
                                    mode: "edit",
                                    name: row.name,
                                    summary: row.summary,
                                    status: row.status,
                                  });
                                }}
                                disabled={controlsDisabled}
                              >
                                Edit
                              </Button>
                              {deleteArmed === row.name ? (
                                <>
                                  <Button
                                    type="button"
                                    size="sm"
                                    variant="destructive"
                                    onClick={() => void handleDelete(row.name)}
                                    disabled={controlsDisabled}
                                  >
                                    {isDeleting ? "Deleting…" : "Confirm delete"}
                                  </Button>
                                  <Button
                                    type="button"
                                    size="sm"
                                    variant="ghost"
                                    onClick={() => setDeleteArmed(null)}
                                    disabled={controlsDisabled}
                                  >
                                    Cancel
                                  </Button>
                                </>
                              ) : (
                                <Button
                                  type="button"
                                  size="sm"
                                  variant="outline"
                                  onClick={() => {
                                    setActionError(null);
                                    setNotice(null);
                                    setDeleteArmed(row.name);
                                  }}
                                  disabled={controlsDisabled}
                                >
                                  Delete
                                </Button>
                              )}
                            </div>
                          </TableCell>
                        </TableRow>
                      );
                    })
                  )}
                </TableBody>
              </Table>
            </CardContent>
          </Card>
        ) : null}
      </div>

      {form !== null ? (
        <McpServerFormDialog
          key={form.mode === "add" ? "add" : `edit:${form.name}`}
          target={form}
          onClose={() => setForm(null)}
          onSaved={(message, report) => {
            setForm(null);
            setActionError(null);
            setNotice(message);
            setLastReload(report);
            void load();
          }}
        />
      ) : null}
    </OpsPage>
  );
}
