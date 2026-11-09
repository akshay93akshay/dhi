"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { getToolCatalog, setToolEnabled, setToolVisibility } from "@/lib/api";
import {
  TOOL_SOURCES,
  availableRoles,
  filterCatalogTools,
  mergeToolStates,
  sortRoles,
  type EnabledFilter,
  type ToolFilterState,
} from "@/lib/toolAdmin";
import type {
  ToolCatalogEntry,
  ToolCatalogResponse,
  ToolSource,
} from "@/lib/types";
import { describeApiError } from "@/lib/ui";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Label } from "@/components/ui/label";
import { OpsPage } from "@/components/OpsPage";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { StatusBanner } from "@/components/StatusBanner";
import { ToolCatalogTable } from "@/components/ToolCatalogTable";

export function ToolsClient() {
  const [catalog, setCatalog] = useState<ToolCatalogResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [filters, setFilters] = useState<ToolFilterState>({
    source: "all",
    enabled: "all",
    role: "all",
  });

  const mounted = useRef(true);
  const seq = useRef(0);

  const load = useCallback(async (): Promise<void> => {
    const current = ++seq.current;
    setRefreshing(true);
    try {
      const next = await getToolCatalog();
      if (mounted.current && current === seq.current) {
        setCatalog(next);
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

  const tools = catalog?.tools ?? [];
  const roleOptions = useMemo(() => {
    const roles = availableRoles(tools);
    if (filters.role !== "all" && !roles.includes(filters.role)) {
      return sortRoles([...roles, filters.role]);
    }
    return roles;
  }, [tools, filters.role]);
  const filtered = useMemo(
    () => filterCatalogTools(tools, filters),
    [tools, filters],
  );

  async function handleToggle(
    tool: ToolCatalogEntry,
    enabled: boolean,
  ): Promise<void> {
    const states = await setToolEnabled(tool.key, enabled);
    setCatalog((previous) =>
      previous === null
        ? previous
        : { ...previous, tools: mergeToolStates(previous.tools, states) },
    );
    setActionError(null);
    setNotice(`${enabled ? "Enabled" : "Disabled"} ${tool.key}.`);
  }

  async function handleSetVisibility(
    tool: ToolCatalogEntry,
    roles: string[],
  ): Promise<void> {
    const states = await setToolVisibility(tool.key, roles);
    setCatalog((previous) =>
      previous === null
        ? previous
        : { ...previous, tools: mergeToolStates(previous.tools, states) },
    );
    setActionError(null);
    setNotice(`Updated visibility for ${tool.key}.`);
  }

  const controlError = catalog?.control_error ?? null;
  const controlsDisabled = controlError !== null;

  return (
    <OpsPage
      current="tools"
      title="Tools"
      description="Read the tool catalog. Enable or hide tools; changes apply to the next engine call."
      actions={
        <Button
          type="button"
          variant="outline"
          onClick={() => {
            setActionError(null);
            setNotice(null);
            void load();
          }}
          disabled={refreshing}
        >
          {refreshing ? "Refreshing…" : "Refresh catalog"}
        </Button>
      }
    >
      <div className="space-y-4">
        {loading ? (
          <Card>
            <CardContent className="py-6 type-body text-muted-foreground">
              Loading tool catalog…
            </CardContent>
          </Card>
        ) : null}

        {loadError !== null ? (
          <StatusBanner
            tone="error"
            title={catalog === null ? "Could not load the tool catalog" : "Refresh failed"}
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
            {catalog !== null ? " Showing the last successfully loaded catalog." : null}
          </StatusBanner>
        ) : null}

        {controlError !== null ? (
          <StatusBanner tone="error" title="Tool control store unavailable">
            {controlError}. The catalog is still readable, but enable/visibility
            changes are disabled. GET /api/tools/control and POST
            /api/tools/control answer 503 in this state.
          </StatusBanner>
        ) : null}

        {catalog !== null && catalog.errors.length > 0 ? (
          <StatusBanner tone="warning" title="Catalog source errors">
            <ul className="list-disc space-y-1 pl-5 text-current">
              {catalog.errors.map((sourceError, index) => (
                <li key={`${sourceError.source}-${index}`}>
                  <span className="type-mono">{sourceError.source}</span>:{" "}
                  {sourceError.error}
                </li>
              ))}
            </ul>
          </StatusBanner>
        ) : null}

        {actionError !== null ? (
          <StatusBanner
            tone="error"
            title="Control action failed"
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
          <StatusBanner tone="success">{notice}</StatusBanner>
        ) : null}

        {catalog !== null ? (
          <Card>
            <CardHeader>
              <CardTitle>Filters</CardTitle>
              <CardDescription>
                Showing {filtered.length} of {tools.length} catalog entries.
              </CardDescription>
            </CardHeader>
            <CardContent>
              <div className="grid gap-4 sm:grid-cols-3">
                <div className="space-y-2">
                  <Label htmlFor="tool-source-filter">Source</Label>
                  <Select
                    value={filters.source}
                    onValueChange={(value) =>
                      setFilters((current) => ({
                        ...current,
                        source: value as ToolSource | "all",
                      }))
                    }
                  >
                    <SelectTrigger id="tool-source-filter">
                      <SelectValue placeholder="All sources" />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectItem value="all">All sources</SelectItem>
                      {TOOL_SOURCES.map((source) => (
                        <SelectItem key={source} value={source}>
                          {source}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                </div>

                <div className="space-y-2">
                  <Label htmlFor="tool-enabled-filter">Enabled</Label>
                  <Select
                    value={filters.enabled}
                    onValueChange={(value) =>
                      setFilters((current) => ({
                        ...current,
                        enabled: value as EnabledFilter,
                      }))
                    }
                  >
                    <SelectTrigger id="tool-enabled-filter">
                      <SelectValue placeholder="Any state" />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectItem value="all">Any state</SelectItem>
                      <SelectItem value="enabled">Enabled only</SelectItem>
                      <SelectItem value="disabled">Disabled only</SelectItem>
                    </SelectContent>
                  </Select>
                </div>

                <div className="space-y-2">
                  <Label htmlFor="tool-role-filter">Role</Label>
                  <Select
                    value={filters.role}
                    onValueChange={(value) =>
                      setFilters((current) => ({ ...current, role: value }))
                    }
                  >
                    <SelectTrigger id="tool-role-filter">
                      <SelectValue placeholder="All roles" />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectItem value="all">All roles</SelectItem>
                      {roleOptions.map((role) => (
                        <SelectItem key={role} value={role}>
                          {role}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                </div>
              </div>
            </CardContent>
          </Card>
        ) : null}

        {catalog !== null ? (
          <Card>
            <CardHeader>
              <CardTitle>Catalog</CardTitle>
            </CardHeader>
            <CardContent className="p-0">
              <ToolCatalogTable
                tools={filtered}
                availableRoles={roleOptions}
                editable
                controlsDisabled={controlsDisabled}
                emptyMessage="No catalog entries match these filters."
                onToggle={handleToggle}
                onSetVisibility={handleSetVisibility}
                onError={(message) => {
                  setActionError(message);
                  setNotice(null);
                }}
              />
            </CardContent>
          </Card>
        ) : null}
      </div>
    </OpsPage>
  );
}
