"use client";

import { Globe2, Sparkles, UserRound } from "lucide-react";

import type { AppSettingsView, RoleSummary, WebProvidersResponse } from "@/lib/types";
import { countReadyWebProviders } from "@/lib/settings";

/**
 * General (Settings v2): a short, read-only overview. Controls intentionally
 * live elsewhere — behavior under Advanced, routing under LLM providers, and
 * provider keys under Web search providers.
 */
export function GeneralOverviewSection(props: {
  settings: AppSettingsView | null;
  providers: WebProvidersResponse | null;
  providersError: string | null;
}) {
  const { settings, providers, providersError } = props;

  const ready = providers === null ? null : countReadyWebProviders(providers);
  const total = providers === null ? null : providers.providers.length;

  return (
    <section className="card" id="general" aria-labelledby="general-heading">
      <div className="row spread">
        <h2 id="general-heading" style={{ margin: 0 }}>
          General
        </h2>
        <span className="chip" data-tone="ok">
          Live configuration
        </span>
      </div>
      <div className="mt-4 grid gap-3 sm:grid-cols-3">
        <RoleOverview
          label="Planner"
          icon={<Sparkles className="h-3.5 w-3.5" />}
          profile={settings?.planner_profile ?? null}
          loading={settings === null}
        />
        <RoleOverview
          label="Worker"
          icon={<UserRound className="h-3.5 w-3.5" />}
          profile={settings?.worker_profile ?? null}
          loading={settings === null}
        />
        <div className="rounded-lg border border-border bg-surface-subtle px-3 py-2.5">
          <div className="flex items-center gap-2">
            <span className="flex h-6 w-6 items-center justify-center rounded-md bg-secondary text-muted-foreground">
              <Globe2 className="h-3.5 w-3.5" />
            </span>
            <span className="text-[10.5px] font-semibold uppercase tracking-[0.12em] text-muted-foreground">
              Web providers
            </span>
          </div>
          <div className="mt-2 text-[13px] font-medium">
            {providersError !== null
              ? "Unavailable"
              : ready === null || total === null
                ? "Loading…"
                : `${ready} of ${total} ready`}
          </div>
          <div className="mt-0.5 text-[11.5px] text-muted-foreground">
            Search, read, and research routing.
          </div>
        </div>
      </div>
    </section>
  );
}

function RoleOverview(props: {
  label: string;
  icon: React.ReactNode;
  profile: RoleSummary | null;
  loading: boolean;
}) {
  const { label, icon, profile, loading } = props;
  return (
    <div className="rounded-lg border border-border bg-surface-subtle px-3 py-2.5">
      <div className="flex items-center gap-2">
        <span className="flex h-6 w-6 items-center justify-center rounded-md bg-secondary text-muted-foreground">
          {icon}
        </span>
        <span className="text-[10.5px] font-semibold uppercase tracking-[0.12em] text-muted-foreground">
          {label}
        </span>
      </div>
      <div className="mt-2 truncate text-[13px] font-medium">
        {loading ? "Loading…" : profile?.name ?? "Not set"}
      </div>
      <div className="mt-0.5 truncate font-mono text-[11px] text-muted-foreground">
        {loading ? "—" : profile?.model ?? "No profile selected"}
      </div>
    </div>
  );
}
