import * as React from "react";
import { Boxes, ServerCog, Sparkles, Wrench } from "lucide-react";

import { PageHeader } from "@/components/ui/page-header";
import {
  ToolAdminNav,
  type ToolAdminPage,
} from "@/components/ToolAdminNav";

/**
 * Common chrome for the four tool-administration pages: the shared page
 * header, the secondary tool-admin switcher, then the page content. Primary
 * navigation lives in the app shell, so there is no duplicate top nav.
 */
export function OpsPage({
  current,
  title,
  description,
  actions,
  children,
}: {
  current: ToolAdminPage;
  title: string;
  description: string;
  actions?: React.ReactNode;
  children: React.ReactNode;
}) {
  const Icon =
    current === "tools"
      ? Wrench
      : current === "mcp"
        ? ServerCog
        : current === "skills"
          ? Sparkles
          : Boxes;

  return (
    <main className="space-y-5">
      <PageHeader
        eyebrow="Build & operate"
        icon={<Icon className="h-3.5 w-3.5" />}
        title={title}
        description={description}
        actions={actions}
      />
      <ToolAdminNav current={current} />
      {children}
    </main>
  );
}
