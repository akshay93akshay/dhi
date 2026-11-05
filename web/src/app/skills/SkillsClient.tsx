"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { getToolCatalog } from "@/lib/api";
import { availableRoles, formatToolRoles } from "@/lib/toolAdmin";
import type {
  ToolCatalogEntry,
  ToolCatalogResponse,
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
import { ToolCatalogTable } from "@/components/ToolCatalogTable";

function LoadSkillState({ tool }: { tool: ToolCatalogEntry }) {
  return (
    <dl className="grid gap-2 text-sm sm:grid-cols-[8rem_1fr]">
      <dt className="text-muted-foreground">Enabled</dt>
      <dd>
        <Badge variant={tool.enabled ? "success" : "destructive"}>
          {tool.enabled ? "enabled" : "disabled"}
        </Badge>
      </dd>
      <dt className="text-muted-foreground">Roles</dt>
      <dd className="text-muted-foreground">
        {formatToolRoles(tool.default_roles)}
      </dd>
      <dt className="text-muted-foreground">Deferred</dt>
      <dd>
        {tool.deferred ? (
          <Badge variant="warning">deferred</Badge>
        ) : (
          <span className="text-muted-foreground">no</span>
        )}
      </dd>
      <dt className="text-muted-foreground">Source</dt>
      <dd>
        <Badge variant="secondary">{tool.source}</Badge>
      </dd>
      {tool.description !== "" ? (
        <>
          <dt className="text-muted-foreground">Description</dt>
          <dd className="text-muted-foreground">{tool.description}</dd>
        </>
      ) : null}
    </dl>
  );
}

export function SkillsClient() {
  const [catalog, setCatalog] = useState<ToolCatalogResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);

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
  const skillsTools = useMemo(
    () => allTools.filter((tool) => tool.source === "skills"),
    [allTools],
  );
  const roles = useMemo(() => availableRoles(allTools), [allTools]);
  const loadSkill = useMemo(
    () => allTools.find((tool) => tool.key === "load_skill") ?? null,
    [allTools],
  );

  const controlError = catalog?.control_error ?? null;

  return (
    <OpsPage
      current="skills"
      title="Skills"
      description="Read-only view of the skills tool source."
      actions={
        <Button
          type="button"
          variant="outline"
          onClick={() => void load()}
          disabled={refreshing}
        >
          {refreshing ? "Refreshing…" : "Refresh catalog"}
        </Button>
      }
    >
      <div className="space-y-4">
        <StatusBanner tone="info" title="Skills are file-based">
          Skills live under <code>DHI_SKILLS_DIR</code> (default{" "}
          <code>skills/</code>), one directory per skill with a{" "}
          <code>SKILL.md</code> file and optional <code>name</code>,{" "}
          <code>description</code>, and <code>visible_to</code> frontmatter. Add
          or edit files on the host and restart the server; this page only
          reads the catalog.
        </StatusBanner>

        {loading ? (
          <Card>
            <CardContent className="py-6 text-sm text-muted-foreground">
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
          <StatusBanner tone="warning" title="Control state could not be read">
            {controlError}. Enabled and role values below are the catalog
            defaults; persisted control overrides are not merged in this view.
          </StatusBanner>
        ) : null}

        {catalog !== null && catalog.errors.length > 0 ? (
          <StatusBanner tone="warning" title="Catalog source errors">
            <ul className="list-disc space-y-1 pl-5 text-current">
              {catalog.errors.map((sourceError, index) => (
                <li key={`${sourceError.source}-${index}`}>
                  <span className="font-mono text-xs">{sourceError.source}</span>:{" "}
                  {sourceError.error}
                </li>
              ))}
            </ul>
          </StatusBanner>
        ) : null}

        {catalog !== null ? (
          <Card>
            <CardHeader>
              <CardTitle>load_skill</CardTitle>
              <CardDescription>Lists or loads visible skills.</CardDescription>
            </CardHeader>
            <CardContent>
              {loadSkill !== null ? (
                <LoadSkillState tool={loadSkill} />
              ) : (
                <StatusBanner tone="warning" title="load_skill is not registered">
                  The current catalog has no <code>load_skill</code> entry, so
                  skills cannot be listed or loaded through the tool catalog.
                </StatusBanner>
              )}
            </CardContent>
          </Card>
        ) : null}

        {catalog !== null ? (
          <Card>
            <CardHeader>
              <CardTitle>Skills source</CardTitle>
              <CardDescription>
                {skillsTools.length}{" "}
                {skillsTools.length === 1 ? "entry" : "entries"} with source{" "}
                <span className="font-mono">skills</span>.
              </CardDescription>
            </CardHeader>
            <CardContent className="p-0">
              <ToolCatalogTable
                tools={skillsTools}
                availableRoles={roles}
                editable={false}
                emptyMessage={'No entries with source "skills" are present in the catalog. Skills may still exist as files, but the load_skill tool is not registered.'}
              />
            </CardContent>
          </Card>
        ) : null}
      </div>
    </OpsPage>
  );
}
