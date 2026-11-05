"use client";

import * as React from "react";
import { Check, Copy } from "lucide-react";

import { copyText } from "@/lib/clipboard";
import { cn } from "@/lib/utils";
import { Button } from "@/components/ui/button";

export function CopyButton({
  text,
  label,
  className,
  size = "sm",
}: {
  text: string;
  label?: string;
  className?: string;
  size?: "sm" | "icon";
}) {
  const [state, setState] = React.useState<"idle" | "copied" | "failed">("idle");
  const timeout = React.useRef<ReturnType<typeof setTimeout> | null>(null);

  React.useEffect(() => {
    return () => {
      if (timeout.current !== null) {
        clearTimeout(timeout.current);
      }
    };
  }, []);

  async function handleCopy(): Promise<void> {
    const copied = await copyText(text);
    setState(copied ? "copied" : "failed");
    if (timeout.current !== null) {
      clearTimeout(timeout.current);
    }
    timeout.current = setTimeout(() => setState("idle"), 1500);
  }

  const title =
    state === "copied"
      ? "Copied"
      : state === "failed"
        ? "Copy failed"
        : label !== undefined
          ? `Copy ${label}`
          : "Copy";

  return (
    <Button
      type="button"
      variant="ghost"
      size={size === "icon" ? "icon" : "sm"}
      className={cn("shrink-0 text-muted-foreground", className)}
      onClick={() => void handleCopy()}
      aria-label={title}
      title={title}
    >
      {state === "copied" ? (
        <Check className="h-3.5 w-3.5" />
      ) : (
        <Copy className="h-3.5 w-3.5" />
      )}
      {label !== undefined && size !== "icon" ? (
        <span>{state === "copied" ? "Copied" : label}</span>
      ) : null}
    </Button>
  );
}
