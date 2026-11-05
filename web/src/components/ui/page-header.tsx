import * as React from "react";

import { cn } from "@/lib/utils";

/**
 * Consistent inner-page header: eyebrow + title + one-line context, with an
 * optional action cluster. The app shell owns the top bar; this sits inside
 * the content column so pages read like documents, not nested navigations.
 */
export function PageHeader({
  eyebrow,
  title,
  description,
  icon,
  actions,
  className,
  children,
}: {
  eyebrow?: string;
  title: React.ReactNode;
  description?: React.ReactNode;
  icon?: React.ReactNode;
  actions?: React.ReactNode;
  className?: string;
  children?: React.ReactNode;
}) {
  return (
    <header
      className={cn(
        "flex flex-col gap-4 lg:flex-row lg:items-end lg:justify-between",
        className,
      )}
    >
      <div className="min-w-0 space-y-2">
        {eyebrow !== undefined ? (
          <div className="flex items-center gap-2 text-[11px] font-semibold uppercase tracking-[0.16em] text-muted-foreground">
            {icon !== undefined ? (
              <span className="text-accent-foreground">{icon}</span>
            ) : null}
            {eyebrow}
          </div>
        ) : null}
        <h1 className="text-[26px] font-semibold leading-tight tracking-[-0.03em] sm:text-[30px]">
          {title}
        </h1>
        {description !== undefined ? (
          <p className="max-w-2xl text-sm leading-6 text-muted-foreground">
            {description}
          </p>
        ) : null}
        {children}
      </div>
      {actions !== undefined ? (
        <div className="flex flex-wrap items-center gap-2 lg:justify-end">
          {actions}
        </div>
      ) : null}
    </header>
  );
}
