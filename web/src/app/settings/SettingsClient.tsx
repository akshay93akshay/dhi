"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import {
  Globe2,
  KeyRound,
  RefreshCw,
  Settings2,
  SlidersHorizontal,
  Wrench,
} from "lucide-react";

import { getSettings, getWebProviders, listProfiles } from "@/lib/api";
import type {
  AppSettingsView,
  LlmProfile,
  WebProviderView,
  WebProvidersResponse,
} from "@/lib/types";
import { describeApiError } from "@/lib/ui";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { PageHeader } from "@/components/ui/page-header";
import { BehaviorSettingsSection } from "./BehaviorSettingsSection";
import { GeneralOverviewSection } from "./GeneralOverviewSection";
import { PoolSection } from "./PoolSection";
import { ProfilesSection } from "./ProfilesSection";
import { RoleAssignmentSection } from "./RoleAssignmentSection";
import { RulesSection } from "./RulesSection";
import { WebProvidersSection } from "./WebProvidersSection";

const GROUPS = [
  { href: "#general", label: "General", icon: SlidersHorizontal },
  { href: "#llm", label: "LLM providers", icon: KeyRound },
  { href: "#providers", label: "Web search providers", icon: Globe2 },
  { href: "#advanced", label: "Advanced", icon: Wrench },
] as const;

/**
 * Settings control room (v2) — a grouped, single-page configuration surface:
 *
 * 1. **General** — a read-only overview of the active roles and providers.
 * 2. **LLM providers** — compact role routing, then the profile cards/editor.
 * 3. **Web search providers** — write-only keys and per-provider controls.
 * 4. **Advanced** — collapsed by default: guard rules, behavior, failover.
 *
 * The old five-section doc/link bar is intentionally gone; every historical
 * anchor (`#roles`, `#profiles`, `#rules`, `#pool`) still resolves, and the
 * Advanced nav item expands the hidden group before scrolling to it.
 */
export function SettingsClient() {
  const [settings, setSettings] = useState<AppSettingsView | null>(null);
  const [profiles, setProfiles] = useState<LlmProfile[] | null>(null);
  const [providers, setProviders] = useState<WebProvidersResponse | null>(null);
  const [providersError, setProvidersError] = useState<string | null>(null);

  const [initialError, setInitialError] = useState<string | null>(null);
  const [refreshError, setRefreshError] = useState<string | null>(null);
  const [refreshing, setRefreshing] = useState(false);

  const [advancedOpen, setAdvancedOpen] = useState(false);
  const [editRequest, setEditRequest] = useState<{
    profileId: number;
    nonce: number;
  } | null>(null);
  const editNonceRef = useRef(0);

  // Sequence token so a slow shared reload can never overwrite the result
  // of a newer one (reloads only start from user actions, but the guard is
  // cheap insurance against double clicks).
  const loadSeqRef = useRef(0);
  const mountedRef = useRef(true);
  const loadedOnceRef = useRef(false);

  const loadShared = useCallback(async (): Promise<void> => {
    const seq = ++loadSeqRef.current;
    setRefreshing(true);
    const results = await Promise.allSettled([
      getSettings(),
      listProfiles(),
      getWebProviders(),
    ]);
    if (!mountedRef.current || seq !== loadSeqRef.current) {
      return;
    }
    let failed: string | null = null;
    if (results[0].status === "fulfilled") {
      setSettings(results[0].value);
    } else {
      failed = describeApiError(results[0].reason);
    }
    if (results[1].status === "fulfilled") {
      setProfiles(results[1].value);
    } else {
      // Profiles failing does not invalidate the settings view — keep stale
      // rows and surface one combined banner.
      const message = describeApiError(results[1].reason);
      failed = failed === null ? message : `${failed} · profiles: ${message}`;
    }
    // Provider failure is isolated: the rest of Settings still renders, and
    // the section owns its own one-line error + retry.
    if (results[2].status === "fulfilled") {
      setProviders(results[2].value);
      setProvidersError(null);
    } else {
      // Keep stale provider rows on a failed refresh.
      setProvidersError(describeApiError(results[2].reason));
    }
    if (!loadedOnceRef.current && failed !== null) {
      // First load failed: no stale rows to keep — full error state.
      loadedOnceRef.current = true;
      setInitialError(failed);
      setRefreshing(false);
      return;
    }
    loadedOnceRef.current = true;
    // A failed refresh keeps whatever is on screen and degrades the banner.
    // A successful retry after the first-load error returns to the page.
    if (failed === null) {
      setInitialError(null);
    }
    setRefreshError(failed);
    setRefreshing(false);
  }, []);

  useEffect(() => {
    mountedRef.current = true;
    void loadShared();
    return () => {
      mountedRef.current = false;
    };
  }, [loadShared]);

  // Deep links to the advanced sections must expand the group
  // before the browser can reveal the target. Listening for `hashchange`
  // also covers an in-app link to `#rules`/`#pool` while already here.
  useEffect(() => {
    const advancedHashes = new Set([
      "#advanced",
      "#rules",
      "#pool",
      "#behavior",
    ]);
    let timer: number | undefined;
    const reveal = (): void => {
      const hash = window.location.hash;
      if (!advancedHashes.has(hash)) {
        return;
      }
      setAdvancedOpen(true);
      if (timer !== undefined) {
        window.clearTimeout(timer);
      }
      timer = window.setTimeout(() => {
        document
          .getElementById(hash.slice(1))
          ?.scrollIntoView({ behavior: "smooth", block: "start" });
      }, 0);
    };
    reveal();
    window.addEventListener("hashchange", reveal);
    return () => {
      window.removeEventListener("hashchange", reveal);
      if (timer !== undefined) {
        window.clearTimeout(timer);
      }
    };
  }, []);

  function handleProviderUpdated(next: WebProviderView): void {
    setProvidersError(null);
    setProviders((current) => {
      if (current === null) {
        return current;
      }
      return {
        ...current,
        providers: current.providers.map((provider) =>
          provider.provider === next.provider ? next : provider,
        ),
      };
    });
  }

  function requestEditKey(profileId: number): void {
    editNonceRef.current += 1;
    setEditRequest({ profileId, nonce: editNonceRef.current });
  }

  const retryVisible =
    initialError === null && refreshError !== null ? (
      <div className="errorbox mt-0" role="alert">
        <div className="flex flex-wrap items-center gap-3">
          <span className="min-w-0 flex-1">Refresh failed: {refreshError}</span>
          <Button
            type="button"
            size="sm"
            variant="outline"
            className="h-7 border-destructive/30 text-destructive hover:bg-destructive/10 hover:text-destructive"
            onClick={() => void loadShared()}
            disabled={refreshing}
          >
            Retry
          </Button>
        </div>
      </div>
    ) : null;

  if (initialError !== null) {
    return (
      <main className="space-y-5">
        <PageHeader
          eyebrow="System"
          icon={<Settings2 className="h-3.5 w-3.5" />}
          title="Settings"
        />
        <Card>
          <CardContent className="pt-5">
            <div className="errorbox mt-0" role="alert">
              Could not load settings: {initialError}
            </div>
            <div className="actions">
              <Button
                type="button"
                onClick={() => void loadShared()}
                disabled={refreshing}
              >
                {refreshing ? "Retrying…" : "Retry"}
              </Button>
            </div>
          </CardContent>
        </Card>
      </main>
    );
  }

  return (
    <main className="space-y-5">
      <PageHeader
        eyebrow="System"
        icon={<Settings2 className="h-3.5 w-3.5" />}
        title="Settings"
        actions={
          <>
            <Badge
              variant={
                settings === null
                  ? "muted"
                  : settings.llm_key_set
                    ? "success"
                    : "warning"
              }
              className="h-9 gap-1.5 rounded-full px-3"
            >
              <KeyRound className="h-3 w-3" />
              {settings?.llm_key_set ? "keys set" : "keys incomplete"}
            </Badge>
            <Button
              type="button"
              variant="outline"
              className="h-9 rounded-full"
              onClick={() => void loadShared()}
              disabled={refreshing}
            >
              <RefreshCw
                className={`h-3.5 w-3.5 ${refreshing ? "animate-spin" : ""}`}
              />
              {refreshing ? "Refreshing…" : "Reload"}
            </Button>
          </>
        }
      />

      <div className="grid gap-5 lg:grid-cols-[220px_minmax(0,1fr)] lg:items-start">
        <aside className="min-w-0 lg:sticky lg:top-24">
          <nav
            aria-label="Settings sections"
            className="flex min-w-0 items-center gap-1 overflow-x-auto rounded-xl border border-border bg-card p-1.5 shadow-sm lg:flex-col lg:items-stretch lg:overflow-visible"
          >
            {GROUPS.map((group) => {
              const Icon = group.icon;
              const isAdvanced = group.href === "#advanced";
              return (
                <a
                  key={group.href}
                  href={group.href}
                  onClick={
                    isAdvanced
                      ? () => {
                          setAdvancedOpen(true);
                        }
                      : undefined
                  }
                  className="inline-flex shrink-0 items-center gap-2 rounded-lg px-3 py-2 type-label font-medium text-muted-foreground transition-colors hover:bg-secondary hover:text-foreground lg:w-full"
                >
                  <Icon className="h-3.5 w-3.5" />
                  {group.label}
                </a>
              );
            })}
          </nav>
        </aside>

        <div className="min-w-0 space-y-5">
          {retryVisible}

          <GeneralOverviewSection
            settings={settings}
            providers={providers}
            providersError={providersError}
          />

          <section
            id="llm"
            className="scroll-mt-24 space-y-4"
            aria-labelledby="llm-heading"
          >
            <div className="px-1">
              <h2 id="llm-heading">LLM providers</h2>
              <p className="subtle" style={{ margin: 0 }}>
                Role routing and the connection profiles behind it.
              </p>
            </div>
            <RoleAssignmentSection
              profiles={profiles}
              settings={settings}
              onSaved={(view) => {
                // The PUT response IS the merged view; role edits reshape
                // the pool chains server-side, and PoolSection polls that.
                setSettings(view);
              }}
              onEditKey={requestEditKey}
            />
            <ProfilesSection
              profiles={profiles}
              settings={settings}
              onChanged={() => void loadShared()}
              editRequest={editRequest}
            />
          </section>

          <WebProvidersSection
            providers={providers}
            error={providersError}
            onProviderUpdated={handleProviderUpdated}
            onReloaded={(response) => {
              setProviders(response);
              setProvidersError(null);
            }}
            onRetry={() => void loadShared()}
          />

          <section
            id="advanced"
            className="scroll-mt-24 space-y-4"
            aria-labelledby="advanced-heading"
          >
            <div className="card">
              <div className="row spread">
                <div>
                  <h2 id="advanced-heading" style={{ margin: 0 }}>
                    Advanced
                  </h2>
                  <p className="subtle" style={{ margin: "4px 0 0" }}>
                    Guard rules, approval behavior, and the failover pool.
                  </p>
                </div>
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  onClick={() => setAdvancedOpen((current) => !current)}
                  aria-expanded={advancedOpen}
                  aria-controls="advanced-content"
                >
                  {advancedOpen ? "Hide advanced settings" : "Show advanced settings"}
                </Button>
              </div>
            </div>

            {advancedOpen ? (
              <div id="advanced-content" className="space-y-5">
                <RulesSection />
                <BehaviorSettingsSection
                  settings={settings}
                  onSaved={(view) => {
                    setSettings(view);
                  }}
                />
                <PoolSection />
              </div>
            ) : null}
          </section>
        </div>
      </div>
    </main>
  );
}
