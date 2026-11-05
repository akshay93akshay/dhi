import type { LucideIcon } from "lucide-react";
import {
  Boxes,
  LayoutDashboard,
  ListTodo,
  ServerCog,
  Settings2,
  ShieldCheck,
  Sparkles,
  Wrench,
} from "lucide-react";

export type NavItem = {
  label: string;
  href: string;
  icon: LucideIcon;
  /** Short context shown by the command palette. */
  hint: string;
  /** Match tasks-detail routes even though the nav href is a dashboard hash. */
  taskRoute?: boolean;
};

export type NavGroup = {
  label: string;
  items: NavItem[];
};

export const NAV_GROUPS: ReadonlyArray<NavGroup> = [
  {
    label: "Workspace",
    items: [
      {
        label: "Overview",
        href: "/",
        icon: LayoutDashboard,
        hint: "Start a new task",
      },
      {
        label: "Tasks",
        href: "/tasks",
        icon: ListTodo,
        hint: "Browse the task ledger",
        taskRoute: true,
      },
      {
        label: "Approvals",
        href: "/approvals",
        icon: ShieldCheck,
        hint: "Guarded tool calls waiting on a human",
      },
    ],
  },
  {
    label: "Build",
    items: [
      {
        label: "Tools",
        href: "/tools",
        icon: Wrench,
        hint: "Catalog, enablement, and visibility",
      },
      {
        label: "MCP",
        href: "/mcp",
        icon: ServerCog,
        hint: "Live MCP servers and reload state",
      },
      {
        label: "Skills",
        href: "/skills",
        icon: Sparkles,
        hint: "File-based skills and load_skill",
      },
      {
        label: "Custom tools",
        href: "/custom-tools",
        icon: Boxes,
        hint: "File-defined custom tool catalog",
      },
    ],
  },
  {
    label: "System",
    items: [
      {
        label: "Settings",
        href: "/settings",
        icon: Settings2,
        hint: "Providers, failover pool, guard rules",
      },
    ],
  },
];

export const FLAT_NAV_ITEMS: ReadonlyArray<NavItem> = NAV_GROUPS.flatMap(
  (group) => group.items,
);

/** True when the pathname belongs to a given nav item. */
export function isNavItemActive(item: NavItem, pathname: string): boolean {
  if (item.taskRoute) {
    return pathname === "/tasks" || pathname.startsWith("/tasks/");
  }
  if (item.href === "/") {
    return pathname === "/";
  }
  return pathname === item.href || pathname.startsWith(`${item.href}/`);
}

/** Top-bar context for the current route. Trailing slashes (static export)
 * are normalized so `/approvals/` resolves exactly like `/approvals`. */
export function routeContext(pathname: string): {
  section: string;
  title: string;
} {
  const normalized =
    pathname !== "/" && pathname.endsWith("/")
      ? pathname.slice(0, -1)
      : pathname;

  if (normalized === "/") {
    return { section: "Workspace", title: "Overview" };
  }
  if (normalized.startsWith("/tasks/")) {
    const tail = normalized.split("/").filter(Boolean);
    if (tail[2] === "work") {
      return { section: "Workspace", title: "Work view" };
    }
    if (tail[2] === "graph") {
      return { section: "Workspace", title: "Task graph" };
    }
    return { section: "Workspace", title: "Task detail" };
  }
  for (const group of NAV_GROUPS) {
    const item = group.items.find((entry) => entry.href === normalized);
    if (item !== undefined) {
      return { section: group.label, title: item.label };
    }
  }
  return { section: "DHI", title: "Workspace" };
}
