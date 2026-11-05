import { Badge } from "@/components/ui/badge";

/**
 * One shared state vocabulary across tasks, goals, intents, facts, results,
 * agent runs, approvals, and sessions. Unknown states stay neutral.
 */
export function stateVariant(
  state: string,
):
  | "success"
  | "warning"
  | "destructive"
  | "info"
  | "accent"
  | "muted" {
  switch (state) {
    case "created":
    case "open":
    case "origin":
    case "pending":
      return "info";
    case "running":
      return "accent";
    case "done":
    case "met":
    case "accepted":
    case "confirmed":
    case "ok":
    case "granted":
    case "allow":
      return "success";
    case "paused":
    case "blocked":
    case "ask":
      return "warning";
    case "failed":
    case "error":
    case "rejected":
    case "denied":
    case "dismissed":
      return "destructive";
    case "timeout":
    case "timed_out":
      return "warning";
    case "cancelled":
      return "muted";
    default:
      return "muted";
  }
}

export function StateBadge({
  state,
  className,
}: {
  state: string;
  className?: string;
}) {
  return (
    <Badge variant={stateVariant(state)} className={className}>
      {state}
    </Badge>
  );
}
