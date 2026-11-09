"use client";

import * as React from "react";
import { ToggleGroup as ToggleGroupPrimitive } from "radix-ui";

import { cn } from "@/lib/utils";

function ToggleGroup({
  className,
  ...props
}: React.ComponentProps<typeof ToggleGroupPrimitive.Root>) {
  return (
    <ToggleGroupPrimitive.Root
      data-slot="toggle-group"
      className={cn(
        "flex w-full items-center gap-0.5 rounded-lg border border-border bg-muted p-0.5 text-muted-foreground",
        className,
      )}
      {...props}
    />
  );
}

function ToggleGroupItem({
  className,
  ...props
}: React.ComponentProps<typeof ToggleGroupPrimitive.Item>) {
  return (
    <ToggleGroupPrimitive.Item
      data-slot="toggle-group-item"
      className={cn(
        "inline-flex h-8 min-w-0 flex-1 items-center justify-center rounded-md px-2 type-label font-medium text-muted-foreground transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-1 disabled:pointer-events-none disabled:opacity-50",
        "data-[state=on]:bg-card data-[state=on]:text-foreground data-[state=on]:shadow-sm",
        className,
      )}
      {...props}
    />
  );
}

export { ToggleGroup, ToggleGroupItem };
