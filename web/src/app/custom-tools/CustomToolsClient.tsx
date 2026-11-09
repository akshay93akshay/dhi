"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { getToolCatalog, setToolEnabled, setToolVisibility } from "@/lib/api";
import { availableRoles, mergeToolStates } from "@/lib/toolAdmin";
import type {
  ToolCatalogEntry,
  ToolCatalogResponse,
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
import { OpsPage } from "@/components/OpsPage";
import { StatusBanner } from "@/components/StatusBanner";
import { ToolCatalogTable } from "@/components/ToolCatalogTable";

export function CustomToolsClient() {
  const [catalog, setCatalog] = useState<ToolCatalogResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

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

  const allTools = catalog?.tools ?? [];
  const tools = useMemo(
    () => allTools.filter((tool) => tool.source === "custom"),
    [allTools],
  );
  const roles = useMemo(() => availableRoles(allTools), [allTools]);

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
    rolesToSave: string[],
  ): Promise<void> {
    const states = await setToolVisibility(tool.key, rolesToSave);
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
      current="custom-tools"
      title="Custom tools"
      description="File-defined custom tools and persisted enable/visibility control."
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
        <StatusBanner tone="info" title="Definitions are file-based">
          Custom tools are loaded at server startup from{" "}
          <code>ops/custom-tools.json</code> (override with{" "}
          <code>DHI_CUSTOM_TOOLS_CONFIG</code>). This page does not create or
          edit definitions; add the file entry and restart the server so the
          catalog snapshot includes it.
        </StatusBanner>

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
            title={
              catalog === null
                ? "Could not load the tool catalog"
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
            {catalog !== null
              ? " Showing the last successfully loaded catalog."
              : null}
          </StatusBanner>
        ) : null}

        {controlError !== null ? (
          <StatusBanner tone="error" title="Tool control store unavailable">
            {controlError}. Enable/visibility changes are disabled; reads still
            work.
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
              <CardTitle>Custom tool catalog</CardTitle>
              <CardDescription>
                {tools.length} {tools.length === 1 ? "entry" : "entries"} with
                source <span className="font-mono">custom</span>. An empty role
                set means every recognized role.
              </CardDescription>
            </CardHeader>
            <CardContent className="p-0">
              <ToolCatalogTable
                tools={tools}
                availableRoles={roles}
                editable
                controlsDisabled={controlsDisabled}
                emptyMessage="No custom tools are registered. Add a definition to ops/custom-tools.json and restart the server."
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
