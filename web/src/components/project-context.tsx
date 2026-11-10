import { FileText, Folder } from "lucide-react";

import type { Task } from "@/lib/types";
import { agentsMdStateLabel, agentsMdStateVariant } from "@/lib/ui";
import { Badge } from "@/components/ui/badge";
import { cn } from "@/lib/utils";

/**
 * Compact project context for one task: the absolute host project folder and
 * the selected AGENTS.md path. Legacy tasks (no `project_folder`) degrade to a
 * quiet label instead of an empty row.
 */
export function ProjectContext({
  task,
  className,
}: {
  task: Task;
  className?: string;
}) {
  const projectFolder = task.project_folder ?? null;
  const agentsMdPath = task.agents_md_path ?? null;
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
      <span
        className="min-w-0 break-all type-mono"
        title={projectFolder ?? undefined}
      >
        {projectFolder ?? "Legacy workspace"}
      </span>
      <span aria-hidden="true">·</span>
      <FileText className="h-3.5 w-3.5 shrink-0" aria-hidden="true" />
      <span className="font-medium text-foreground">AGENTS.md</span>
      <span
        className="min-w-0 break-all type-mono"
        title={agentsMdPath ?? undefined}
      >
        {agentsMdPath ?? "Not selected"}
      </span>
      {task.task_folder !== null && task.task_folder !== undefined ? (
        <>
          <span aria-hidden="true">·</span>
          <span className="font-medium text-foreground">Task folder</span>
          <span
            className="min-w-0 break-all type-mono"
            title={task.task_folder}
          >
            {task.task_folder}
          </span>
        </>
      ) : null}
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
