import { Folder } from "lucide-react";

import type { Task } from "@/lib/types";
import { agentsMdStateLabel, agentsMdStateVariant } from "@/lib/ui";
import { Badge } from "@/components/ui/badge";
import { cn } from "@/lib/utils";

/**
 * Compact project context for one task: project folder path, per-task folder,
 * and the AGENTS.md state. Legacy tasks (no `project_folder`) degrade to a
 * single quiet label instead of empty/broken rows.
 */
export function ProjectContext({
  task,
  className,
}: {
  task: Task;
  className?: string;
}) {
  const webProjectId = task.web_project_id ?? null;
  if (webProjectId !== null) {
    const webProjectName =
      task.web_project_name ?? `Web project ${webProjectId.slice(0, 8)}`;
    const label = agentsMdStateLabel(task.agents_md_state);
    return (
      <div
        className={cn(
          "flex flex-wrap items-center gap-x-2 gap-y-1 type-meta text-muted-foreground",
          className,
        )}
      >
        <Folder className="h-3.5 w-3.5 shrink-0" aria-hidden="true" />
        <span className="font-medium text-foreground">Web project</span>
        <span className="min-w-0 break-all type-mono" title={webProjectId}>
          {webProjectName}
        </span>
        {label !== null ? (
          <>
            <span aria-hidden="true">·</span>
            <Badge variant={agentsMdStateVariant(task.agents_md_state)}>
              {label}
            </Badge>
          </>
        ) : null}
      </div>
    );
  }

  const projectFolder = task.project_folder ?? null;
  if (projectFolder === null) {
    return (
      <div
        className={cn(
          "flex flex-wrap items-center gap-x-2 gap-y-1 type-meta text-muted-foreground",
          className,
        )}
      >
        <span>Legacy workspace</span>
      </div>
    );
  }

  const label = agentsMdStateLabel(task.agents_md_state);
  return (
    <div
      className={cn(
        "flex flex-wrap items-center gap-x-2 gap-y-1 type-meta text-muted-foreground",
        className,
      )}
    >
      <Folder className="h-3.5 w-3.5 shrink-0" aria-hidden="true" />
      <span className="font-medium text-foreground">Project</span>
      <span className="min-w-0 break-all type-mono" title={projectFolder}>
        {projectFolder}
      </span>
      <span aria-hidden="true">·</span>
      <span className="font-medium text-foreground">Task folder</span>
      <span
        className="min-w-0 break-all type-mono"
        title={task.task_folder ?? undefined}
      >
        {task.task_folder ?? "—"}
      </span>
      {label !== null ? (
        <>
          <span aria-hidden="true">·</span>
          <Badge variant={agentsMdStateVariant(task.agents_md_state)}>
            {label}
          </Badge>
        </>
      ) : null}
    </div>
  );
}
