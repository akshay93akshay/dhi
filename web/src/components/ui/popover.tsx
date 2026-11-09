"use client";

import * as React from "react";
import { Popover as PopoverPrimitive } from "radix-ui";

import { cn } from "@/lib/utils";

const Popover = PopoverPrimitive.Root;
const PopoverTrigger = PopoverPrimitive.Trigger;
const PopoverAnchor = PopoverPrimitive.Anchor;

function PopoverContent({
  className,
  align = "center",
  sideOffset = 8,
  ...props
}: React.ComponentProps<typeof PopoverPrimitive.Content>) {
  return (
    <PopoverPrimitive.Portal>
      <PopoverPrimitive.Content
        data-slot="popover-content"
        align={align}
        sideOffset={sideOffset}
        className={cn(
          "z-50 flex w-72 flex-col gap-2.5 rounded-xl border border-border bg-popover p-2.5 type-body text-popover-foreground shadow-lg outline-none",
          "data-[state=open]:animate-[popover-in_140ms_ease-out]",
          className,
        )}
        {...props}
      />
    </PopoverPrimitive.Portal>
  );
}

function PopoverHeader({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="popover-header"
      className={cn("flex flex-col gap-0.5 type-body", className)}
      {...props}
    />
  );
}

function PopoverTitle({ className, ...props }: React.ComponentProps<"h2">) {
  return (
    <h2
      data-slot="popover-title"
      className={cn("font-semibold", className)}
      {...props}
    />
  );
}

function PopoverDescription({
  className,
  ...props
}: React.ComponentProps<"p">) {
  return (
    <p
      data-slot="popover-description"
      className={cn("text-muted-foreground", className)}
      {...props}
    />
  );
}

export {
  Popover,
  PopoverAnchor,
  PopoverContent,
  PopoverDescription,
  PopoverHeader,
  PopoverTitle,
  PopoverTrigger,
};
