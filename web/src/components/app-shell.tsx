"use client";

import { useEffect, useState } from "react";
import Link from "next/link";
import { usePathname } from "next/navigation";
import {
  PanelLeftClose,
  PanelLeftOpen,
  Plus,
  Menu,
  X,
} from "lucide-react";

import { getHealth } from "@/lib/api";
import type { Health } from "@/lib/types";
import { serviceState, type ServiceState } from "@/lib/ui";
import {
  DhiBrandStatus,
  DhiServiceStatus,
} from "@/components/dhi-brand-status";
import { Button } from "@/components/ui/button";
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetTitle,
} from "@/components/ui/sheet";
import { CommandPalette } from "@/components/command-palette";
import { PreferencesPanel } from "@/components/preferences-panel";
import { usePreferences } from "@/components/preferences-provider";
import {
  NAV_GROUPS,
  isNavItemActive,
  routeContext,
  type NavItem,
} from "@/components/nav-config";
import type { CollapsePreference } from "@/lib/preferences";
import { cn } from "@/lib/utils";

const HEALTH_POLL_MS = 30_000;

/** One health poll shared by every brand-status instance in the shell. */
function useServiceState(): ServiceState {
  const [health, setHealth] = useState<Health | null>(null);
  const [failed, setFailed] = useState(false);

  useEffect(() => {
    let alive = true;

    const load = (): void => {
      getHealth()
        .then((value) => {
          if (alive) {
            setHealth(value);
            setFailed(false);
          }
        })
        .catch(() => {
          if (alive) {
            setFailed(true);
          }
        });
    };

    void load();
    const timer = window.setInterval(() => {
      if (document.visibilityState === "visible") {
        load();
      }
    }, HEALTH_POLL_MS);

    const onVisible = (): void => {
      if (document.visibilityState === "visible") {
        load();
      }
    };
    const onFocus = (): void => load();

    document.addEventListener("visibilitychange", onVisible);
    window.addEventListener("focus", onFocus);
    return () => {
      alive = false;
      window.clearInterval(timer);
      document.removeEventListener("visibilitychange", onVisible);
      window.removeEventListener("focus", onFocus);
    };
  }, []);

  return serviceState(health, failed ? "unreachable" : null);
}

function NavLinks({
  collapsed = false,
  onNavigate,
}: {
  collapsed?: boolean;
  onNavigate?: () => void;
}) {
  const pathname = usePathname() ?? "/";

  return (
    <nav aria-label="Primary" className="space-y-5">
      {NAV_GROUPS.map((group, groupIndex) => (
        <div key={group.label}>
          {!collapsed ? (
            <div
              className="mb-1.5 px-2.5 text-[10px] font-semibold uppercase tracking-[0.16em] text-sidebar-muted/80"
              data-sidebar-copy
            >
              {group.label}
            </div>
          ) : groupIndex > 0 ? (
            <div className="mx-2 mb-2 h-px bg-sidebar-border" />
          ) : null}
          <ul className="space-y-0.5">
            {group.items.map((item) => {
              const active = isNavItemActive(item, pathname);
              return (
                <li key={item.href}>
                  <NavLink
                    item={item}
                    active={active}
                    collapsed={collapsed}
                    onNavigate={onNavigate}
                  />
                </li>
              );
            })}
          </ul>
        </div>
      ))}
    </nav>
  );
}

function NavLink({
  item,
  active,
  collapsed,
  onNavigate,
}: {
  item: NavItem;
  active: boolean;
  collapsed: boolean;
  onNavigate?: () => void;
}) {
  const Icon = item.icon;
  return (
    <Link
      href={item.href}
      onClick={onNavigate}
      aria-current={active ? "page" : undefined}
      title={collapsed ? item.label : undefined}
      className={cn(
        "group relative flex items-center gap-2.5 rounded-lg py-2 text-[13px] font-medium transition-colors",
        collapsed ? "justify-center px-2" : "px-2.5",
        active
          ? "bg-sidebar-accent text-sidebar-accent-foreground"
          : "text-sidebar-muted hover:bg-sidebar-accent/60 hover:text-sidebar-foreground",
      )}
    >
      {active ? (
        <span
          className="absolute left-0 top-1/2 h-4 w-0.5 -translate-y-1/2 rounded-r-full bg-accent-foreground"
          aria-hidden="true"
        />
      ) : null}
      <Icon
        className={cn(
          "h-4 w-4 shrink-0",
          active ? "text-accent-foreground" : "text-sidebar-muted group-hover:text-sidebar-foreground",
        )}
        strokeWidth={active ? 2.2 : 1.8}
      />
      {!collapsed ? <span className="truncate" data-sidebar-copy>{item.label}</span> : null}
    </Link>
  );
}

function Sidebar({
  collapsed,
  collapse,
  service,
}: {
  collapsed: boolean;
  collapse: CollapsePreference;
  service: ServiceState;
}) {
  const offcanvasCollapsed = collapsed && collapse === "offcanvas";

  return (
    <aside
      data-collapsed={collapsed ? "true" : "false"}
      className={cn(
        "app-sidebar sticky top-0 h-svh shrink-0 flex-col border-r border-sidebar-border bg-sidebar text-sidebar-foreground transition-[width] duration-200",
        offcanvasCollapsed ? "hidden" : "hidden lg:flex",
        collapsed ? "w-[70px]" : "w-[248px]",
      )}
    >
      <div className="app-sidebar-header flex h-16 shrink-0 items-center justify-center border-b border-sidebar-border px-2">
        <DhiBrandStatus state={service} compact />
      </div>
      <div className={cn("flex-1 overflow-y-auto py-4", collapsed ? "px-2" : "px-3")}>
        <NavLinks collapsed={collapsed} />
      </div>
      <div
        className={cn(
          "app-sidebar-footer shrink-0 border-t border-sidebar-border",
          collapsed ? "px-2 py-3" : "px-4 py-3",
        )}
      >
        <DhiServiceStatus state={service} collapsed={collapsed} />
      </div>
    </aside>
  );
}

function MobileSheet({
  open,
  onOpenChange,
  service,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  service: ServiceState;
}) {
  return (
    <Sheet open={open} onOpenChange={onOpenChange}>
      <SheetContent side="left" className="p-0">
        <SheetTitle className="sr-only">Navigation</SheetTitle>
        <SheetDescription className="sr-only">
          DHI workspace navigation
        </SheetDescription>
        <div className="flex h-16 shrink-0 items-center justify-center border-b border-sidebar-border px-4">
          <DhiBrandStatus state={service} compact />
        </div>
        <div className="flex-1 overflow-y-auto px-3 py-4">
          <NavLinks onNavigate={() => onOpenChange(false)} />
        </div>
        <div className="flex shrink-0 items-center justify-between gap-3 border-t border-sidebar-border p-3">
          <DhiServiceStatus state={service} />
          <Button
            type="button"
            variant="ghost"
            size="sm"
            onClick={() => onOpenChange(false)}
            className="text-sidebar-muted hover:bg-sidebar-accent hover:text-sidebar-foreground"
          >
            <X className="h-3.5 w-3.5" /> Close
          </Button>
        </div>
      </SheetContent>
    </Sheet>
  );
}

/**
 * Persistent dashboard shell: grouped sidebar, slim contextual top bar,
 * command palette, and the Preferences popover (theme included). Layout options
 * are carried as attributes on <html> (see `globals.css`); the sidebar collapse
 * state is shared through the preferences provider.
 */
export function AppShell({ children }: { children: React.ReactNode }) {
  const pathname = usePathname() ?? "/";
  const context = routeContext(pathname);
  const { preferences, setPreference } = usePreferences();
  const collapsed = preferences.sidebarCollapsed;
  const [mobileOpen, setMobileOpen] = useState(false);
  const service = useServiceState();

  useEffect(() => {
    setMobileOpen(false);
  }, [pathname]);

  const toggleCollapsed = (): void => {
    setPreference("sidebarCollapsed", !collapsed);
  };

  const offcanvasCollapsed = collapsed && preferences.collapse === "offcanvas";

  return (
    <div className="flex min-h-svh bg-background text-foreground">
      <Sidebar
        collapsed={collapsed}
        collapse={preferences.collapse}
        service={service}
      />
      <MobileSheet
        open={mobileOpen}
        onOpenChange={setMobileOpen}
        service={service}
      />

      <div className="flex min-w-0 flex-1 flex-col">
        <header className="app-header sticky top-0 z-40 flex h-14 shrink-0 items-center gap-2 border-b border-border bg-background px-3 sm:px-5">
          <Button
            type="button"
            variant="ghost"
            size="icon"
            className={cn(
              "h-8 w-8 text-muted-foreground lg:hidden",
              offcanvasCollapsed && "inline-flex lg:inline-flex",
            )}
            onClick={() => setMobileOpen(true)}
            aria-label="Open navigation"
          >
            <Menu className="h-4 w-4" />
          </Button>
          <Button
            type="button"
            variant="ghost"
            size="icon"
            className="hidden h-8 w-8 text-muted-foreground lg:inline-flex"
            onClick={toggleCollapsed}
            aria-label={collapsed ? "Expand sidebar" : "Collapse sidebar"}
            title={collapsed ? "Expand sidebar" : "Collapse sidebar"}
          >
            {collapsed ? (
              <PanelLeftOpen className="h-4 w-4" />
            ) : (
              <PanelLeftClose className="h-4 w-4" />
            )}
          </Button>

          <div className="flex min-w-0 items-center gap-2">
            <span className="sm:hidden">
              <DhiBrandStatus state={service} compact badge />
            </span>
            <span className="hidden text-[11px] font-medium uppercase tracking-[0.12em] text-muted-foreground sm:inline">
              {context.section}
            </span>
            <span className="hidden text-muted-foreground/40 sm:inline">/</span>
            <span className="truncate text-[13px] font-semibold text-foreground">
              {context.title}
            </span>
          </div>

          <div className="ml-auto flex items-center gap-1.5 sm:gap-2">
            <Button
              asChild
              size="sm"
              className="hidden h-9 rounded-full px-3.5 sm:inline-flex"
            >
              <Link href="/#new-task">
                <Plus className="h-3.5 w-3.5" />
                New task
              </Link>
            </Button>
            <Button
              asChild
              size="icon"
              className="h-9 w-9 rounded-full sm:hidden"
              aria-label="New task"
              title="New task"
            >
              <Link href="/#new-task">
                <Plus className="h-4 w-4" />
              </Link>
            </Button>
            <CommandPalette />
            <PreferencesPanel />
          </div>
        </header>

        <div className="min-w-0 flex-1 px-4 py-6 sm:px-6 lg:px-8 lg:py-8">
          <div className="app-content mx-auto w-full max-w-[1440px]">
            {children}
          </div>
        </div>
      </div>
    </div>
  );
}
