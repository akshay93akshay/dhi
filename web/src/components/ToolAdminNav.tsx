import Link from "next/link";

import { cn } from "@/lib/utils";

export type ToolAdminPage = "tools" | "mcp" | "skills" | "custom-tools";

const LINKS: ReadonlyArray<{ page: ToolAdminPage; href: string; label: string }> =
  [
    { page: "tools", href: "/tools", label: "Tools" },
    { page: "mcp", href: "/mcp", label: "MCP servers" },
    { page: "skills", href: "/skills", label: "Skills" },
    { page: "custom-tools", href: "/custom-tools", label: "Custom tools" },
  ];

/**
 * Secondary navigation for the tool administration surfaces. The primary
 * sidebar covers the same destinations; this row is a local switcher for the
 * four related build surfaces.
 */
export function ToolAdminNav({ current }: { current: ToolAdminPage }) {
  return (
    <nav
      aria-label="Tool administration"
      className="flex flex-wrap items-center gap-1 rounded-xl border border-border bg-card p-1.5 shadow-sm"
    >
      <span className="mr-1 hidden px-2 type-label font-semibold uppercase tracking-[0.14em] text-muted-foreground sm:inline">
        Tool admin
      </span>
      {LINKS.map((link) =>
        link.page === current ? (
          <span
            key={link.page}
            aria-current="page"
            className="rounded-lg bg-secondary px-3 py-1.5 type-label font-semibold text-secondary-foreground shadow-sm"
          >
            {link.label}
          </span>
        ) : (
          <Link
            key={link.page}
            href={link.href}
            className={cn(
              "rounded-lg px-3 py-1.5 type-label font-medium text-muted-foreground transition-colors hover:bg-secondary hover:text-foreground",
            )}
          >
            {link.label}
          </Link>
        ),
      )}
    </nav>
  );
}
