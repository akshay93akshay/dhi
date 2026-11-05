"use client";

import Link from "next/link";
import { ChevronRight, Home } from "lucide-react";

import type { CrumbSegment } from "@/lib/nav";

/**
 * In-page breadcrumb trail. The primary navigation lives in the app shell,
 * so this component deliberately renders ONLY the trail for inner pages —
 * never a second top nav.
 */
export function Crumbs({
  trail,
}: {
  trail?: CrumbSegment[];
} = {}) {
  if (trail === undefined || trail.length === 0) {
    return null;
  }

  return (
    <nav
      aria-label="Breadcrumb"
      className="mb-4 flex flex-wrap items-center gap-1.5 text-[12px] text-muted-foreground"
    >
      <Link
        href="/"
        className="inline-flex items-center gap-1 rounded px-1 py-0.5 transition-colors hover:bg-secondary hover:text-foreground"
        aria-label="Overview"
      >
        <Home className="h-3 w-3" />
      </Link>
      {trail.map((segment, index) => {
        const isLast = index === trail.length - 1;
        return (
          <span
            className="inline-flex items-center gap-1.5"
            key={`${segment.label}-${index}`}
          >
            <ChevronRight className="h-3 w-3 text-border" aria-hidden="true" />
            {isLast || segment.href === undefined ? (
              <span
                className={isLast ? "font-medium text-foreground" : undefined}
                aria-current={isLast ? "page" : undefined}
              >
                {segment.label}
              </span>
            ) : (
              <Link
                href={segment.href}
                className="rounded px-1 py-0.5 transition-colors hover:bg-secondary hover:text-foreground"
              >
                {segment.label}
              </Link>
            )}
          </span>
        );
      })}
    </nav>
  );
}
