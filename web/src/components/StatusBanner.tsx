import * as React from "react";

import { cn } from "@/lib/utils";

type Tone = "error" | "warning" | "success" | "info";

const TONES: Record<Tone, string> = {
  error: "border-destructive/30 bg-destructive/8",
  warning: "border-warning/30 bg-warning/8",
  success: "border-success/30 bg-success/8",
  info: "border-info/30 bg-info/8",
};

const TITLE_TONES: Record<Tone, string> = {
  error: "text-destructive",
  warning: "text-warning",
  success: "text-success",
  info: "text-info",
};

/**
 * One inline status block used by the ops pages for load failures, control
 * failures, reload reports, and success acknowledgements. `role="alert"` is
 * reserved for actual errors so the status messages stay polite.
 */
export function StatusBanner({
  tone,
  title,
  children,
  action,
  className,
}: {
  tone: Tone;
  title?: string;
  children?: React.ReactNode;
  action?: React.ReactNode;
  className?: string;
}) {
  return (
    <div
      role={tone === "error" ? "alert" : "status"}
      className={cn(
        "flex flex-wrap items-start justify-between gap-3 rounded-xl border px-4 py-3 text-sm text-foreground",
        TONES[tone],
        className,
      )}
    >
      <div className="min-w-0 flex-1">
        {title !== undefined ? (
          <div className={cn("font-semibold", TITLE_TONES[tone])}>{title}</div>
        ) : null}
        {children !== undefined ? (
          <div
            className={cn(
              "text-[13px] leading-5 text-muted-foreground",
              title === undefined ? "" : "mt-1",
            )}
          >
            {children}
          </div>
        ) : null}
      </div>
      {action !== undefined ? <div className="shrink-0">{action}</div> : null}
    </div>
  );
}
