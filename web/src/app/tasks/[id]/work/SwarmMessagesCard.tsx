"use client";

import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type MouseEvent,
  type ReactNode,
} from "react";
import Link from "next/link";
import {
  ArrowRight,
  Ban,
  CheckCircle2,
  ChevronRight,
  CircleDot,
  FileCheck2,
  GitBranch,
  Inbox,
  ListTodo,
  Mail,
  MessageSquareText,
  Radio,
  RefreshCw,
  Send,
  Users,
  Waypoints,
} from "lucide-react";
import {
  Background,
  Controls,
  MarkerType,
  MiniMap,
  Position,
  ReactFlow,
  type Edge,
  type Node,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";

import { getTeamMessages } from "@/lib/api";
import { formatLocalClock, formatLocalTimestamp } from "@/lib/localTime";
import { describeApiError, shortenId } from "@/lib/ui";
import { cn } from "@/lib/utils";
import {
  buildTeamMessageGraph,
  type TeamGraphModel,
} from "@/lib/swarmGraph";
import {
  TEAM_WORKSPACE_POLL_MS,
  loadTeamWorkspace,
  shouldPollTeamWorkspace,
  type TeamWorkspace,
} from "@/lib/team";
import {
  DEFAULT_SWARM_TAB,
  SWARM_TABS,
  TEAM_BOARD_COLUMNS,
  fragmentReviewLabel,
  groupTeamTasks,
  mailboxRows,
  missionKindLabel,
  missionKindVariant,
  normalizeOutcomeStatus,
  outcomeFinalizeState,
  outcomeSectionCoverage,
  outcomeStatusLabel,
  reviewStatusVariant,
  runProgressPercent,
  runStatusLabel,
  runStatusVariant,
  teamAuthorRole,
  teamAuthorRoleLabel,
  teamHeaderLabel,
  teamRunCounts,
  teamTaskBlockedBy,
  teamTaskLabel,
  teamTaskStatusLabel,
  unreadCount,
  type MailboxBox,
  type MissionEntry,
  type OutcomeFragment,
  type SwarmTabId,
  type TeamAuthorRole,
  type TeamOutcome,
  type TeamRun,
  type TeamSummary,
  type TeamTask,
  type TeamTaskStatus,
} from "@/lib/teamTypes";
import type { TeamMessageRecord } from "@/lib/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { CopyButton } from "@/components/ui/copy-button";
import { MarkdownMath } from "@/components/markdown-math";
import { useColorMode } from "@/components/use-color-mode";

// ---------------------------------------------------------------------------
// Shared visual helpers
// ---------------------------------------------------------------------------

function taskStatusVariant(
  status: TeamTaskStatus,
): "info" | "accent" | "warning" | "success" {
  switch (status) {
    case "pending":
      return "info";
    case "in_progress":
      return "accent";
    case "blocked":
      return "warning";
    case "completed":
      return "success";
  }
}

function outcomeStatusVariant(
  status: TeamOutcome["status"],
): "muted" | "info" | "success" {
  switch (status) {
    case "draft":
      return "muted";
    case "in_review":
      return "info";
    case "finalized":
      return "success";
  }
}

function EmptyPanel(props: {
  icon: ReactNode;
  title: string;
  description: string;
}) {
  return (
    <div className="flex flex-col items-center justify-center rounded-lg border border-dashed border-border bg-surface-subtle px-5 py-10 text-center">
      <span className="flex h-10 w-10 items-center justify-center rounded-xl bg-muted text-muted-foreground">
        {props.icon}
      </span>
      <p className="mt-3 type-body font-semibold text-foreground">{props.title}</p>
      <p className="mt-1 max-w-md type-meta text-muted-foreground">
        {props.description}
      </p>
    </div>
  );
}

function InlineEmpty(props: { text: string }) {
  return (
    <p className="rounded-lg border border-dashed border-border bg-surface-subtle px-3 py-4 text-center type-meta text-muted-foreground">
      {props.text}
    </p>
  );
}

function CountBadge(props: { children: ReactNode }) {
  return (
    <Badge variant="outline" className="font-mono font-normal">
      {props.children}
    </Badge>
  );
}

/**
 * Compact author-role chip. The wire calls the coordinator `lead`; the UI
 * always says "Coordinator". Unknown authors render nothing.
 */
function RoleBadge(props: { role: TeamAuthorRole | null }) {
  const label = teamAuthorRoleLabel(props.role);
  if (label === null) {
    return null;
  }
  return (
    <Badge
      className="px-1.5 py-0 type-badge font-normal"
      title={`${label} author`}
      variant={props.role === "coordinator" ? "accent" : "outline"}
    >
      {label}
    </Badge>
  );
}

function SubtabButton(props: {
  active: boolean;
  children: ReactNode;
  onClick: () => void;
}) {
  return (
    <Button
      aria-pressed={props.active}
      className={cn(
        "h-8 rounded-full px-3 type-label",
        props.active
          ? "border-accent-foreground/30 bg-accent text-accent-foreground hover:bg-accent"
          : "text-muted-foreground",
      )}
      onClick={props.onClick}
      size="sm"
      type="button"
      variant={props.active ? "secondary" : "outline"}
    >
      {props.children}
    </Button>
  );
}

// ---------------------------------------------------------------------------
// Task board
// ---------------------------------------------------------------------------

function TaskBoard(props: {
  tasks: readonly TeamTask[];
  summary: TeamSummary | null;
  onOpenTask: (task: TeamTask) => void;
}) {
  const groups = groupTeamTasks(props.tasks);
  if (props.tasks.length === 0) {
    return (
      <EmptyPanel
        description="Teammates have not created or claimed any shared tasks yet."
        icon={<ListTodo className="h-5 w-5" />}
        title="No team tasks yet"
      />
    );
  }
  return (
    <div className="grid gap-3 md:grid-cols-2 xl:grid-cols-4">
      {TEAM_BOARD_COLUMNS.map((column) => {
        const tasks = groups[column.status];
        return (
          <section
            aria-label={`${column.label} tasks`}
            className="flex min-w-0 flex-col rounded-xl border border-border bg-surface-subtle p-3"
            key={column.status}
          >
            <div className="mb-2.5 flex items-center justify-between gap-2">
              <div className="flex items-center gap-1.5">
                <span
                  aria-hidden="true"
                  className={cn(
                    "h-2 w-2 rounded-full",
                    column.status === "pending" && "bg-info",
                    column.status === "in_progress" && "bg-accent-foreground",
                    column.status === "blocked" && "bg-warning",
                    column.status === "completed" && "bg-success",
                  )}
                />
                <h3 className="type-label font-semibold uppercase tracking-[0.08em] text-muted-foreground">
                  {column.label}
                </h3>
              </div>
              <CountBadge>{tasks.length}</CountBadge>
            </div>
            <div className="space-y-2">
              {tasks.length === 0 ? (
                <InlineEmpty text={`No ${column.label.toLowerCase()} tasks.`} />
              ) : (
                tasks.map((task) => (
                  <TaskCard
                    key={task.id}
                    onOpen={() => props.onOpenTask(task)}
                    summary={props.summary}
                    task={task}
                    tasks={props.tasks}
                  />
                ))
              )}
            </div>
          </section>
        );
      })}
    </div>
  );
}

function TaskCard(props: {
  task: TeamTask;
  tasks: readonly TeamTask[];
  summary: TeamSummary | null;
  onOpen: () => void;
}) {
  const { task } = props;
  const blockedBy = teamTaskBlockedBy(task, props.tasks);
  const dependencyLabels = task.dependencies.map(
    (dependency) =>
      teamTaskLabel(dependency, props.tasks) ?? shortenId(dependency),
  );
  const blockedLabels = blockedBy.map(
    (blocker) => teamTaskLabel(blocker, props.tasks) ?? shortenId(blocker),
  );
  return (
    <button
      aria-haspopup="dialog"
      className="w-full rounded-lg border border-border bg-card px-3 py-3 text-left shadow-sm transition-colors hover:border-accent-foreground/30 hover:bg-accent/30 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
      onClick={props.onOpen}
      type="button"
    >
      <div className="flex items-start justify-between gap-2">
        <span className="min-w-0 flex-1 type-table font-semibold leading-snug text-foreground">
          {task.title}
        </span>
        <Badge variant={taskStatusVariant(task.status)} className="shrink-0">
          {teamTaskStatusLabel(task.status)}
        </Badge>
      </div>
      <div className="mt-2 flex flex-wrap items-center gap-x-2 gap-y-1 type-meta text-muted-foreground">
        <span className="inline-flex min-w-0 items-center gap-1">
          <Users className="h-3 w-3 shrink-0" />
          <span className="truncate">{task.assignee ?? "unassigned"}</span>
        </span>
        {task.created_by !== null ? (
          <span
            className="inline-flex min-w-0 items-center gap-1"
            title={`Created by ${task.created_by}`}
          >
            <span className="truncate">
              by <span className="mono">{task.created_by}</span>
            </span>
            <RoleBadge role={teamAuthorRole(task.created_by, props.summary)} />
          </span>
        ) : null}
        {dependencyLabels.length > 0 ? (
          <span
            className="max-w-full truncate"
            title={dependencyLabels.join(", ")}
          >
            · deps {dependencyLabels.join(", ")}
          </span>
        ) : null}
        {blockedLabels.length > 0 ? (
          <span
            className="max-w-full truncate text-warning"
            title={blockedLabels.join(", ")}
          >
            · blocked by {blockedLabels.join(", ")}
          </span>
        ) : null}
      </div>
      {task.summary !== "" ? (
        <p className="mt-2 line-clamp-2 type-meta text-muted-foreground">
          {task.summary}
        </p>
      ) : null}
      {task.blocked_reason !== "" ? (
        <p className="mt-1.5 line-clamp-2 type-meta text-warning">
          Blocked: {task.blocked_reason}
        </p>
      ) : null}
    </button>
  );
}

function TaskDetailsDialog(props: {
  task: TeamTask | null;
  tasks: readonly TeamTask[];
  summary: TeamSummary | null;
  onClose: () => void;
}) {
  const { task } = props;
  const blockedBy =
    task === null ? [] : teamTaskBlockedBy(task, props.tasks);
  return (
    <Dialog
      open={task !== null}
      onOpenChange={(open) => {
        if (!open) {
          props.onClose();
        }
      }}
    >
      {task !== null ? (
        <DialogContent className="max-w-2xl">
          <DialogHeader>
            <div className="flex flex-wrap items-center gap-2">
              <Badge variant={taskStatusVariant(task.status)}>
                {teamTaskStatusLabel(task.status)}
              </Badge>
              <span className="type-meta text-muted-foreground">
                task <span className="mono">{shortenId(task.id)}</span>
              </span>
              {task.created_by !== null ? (
                <span className="inline-flex items-center gap-1 type-meta text-muted-foreground">
                  by <span className="mono">{task.created_by}</span>
                  <RoleBadge
                    role={teamAuthorRole(task.created_by, props.summary)}
                  />
                </span>
              ) : null}
            </div>
            <DialogTitle>{task.title}</DialogTitle>
            <DialogDescription>
              {task.assignee === null
                ? "Unassigned"
                : `Assigned to ${task.assignee}`}
              {task.updated_at === null
                ? ""
                : ` · updated ${formatLocalTimestamp(task.updated_at)}`}
            </DialogDescription>
          </DialogHeader>

          <div className="space-y-4">
            {task.description !== "" ? (
              <div>
                <span className="section-title">Description</span>
                <MarkdownMath
                  className="type-body [&_p+p]:mt-2 [&_ul]:my-1.5 [&_ol]:my-1.5 [&_li]:my-0.5"
                  text={task.description}
                />
              </div>
            ) : null}

            {task.summary !== "" ? (
              <div>
                <span className="section-title">Summary</span>
                <p className="type-body text-foreground">{task.summary}</p>
              </div>
            ) : null}

            <div className="grid gap-4 sm:grid-cols-2">
              <div>
                <span className="section-title">Dependencies</span>
                {task.dependencies.length === 0 ? (
                  <p className="type-meta text-muted-foreground">None</p>
                ) : (
                  <ul className="space-y-1.5">
                    {task.dependencies.map((dependency) => (
                      <li
                        className="flex items-start gap-1.5 type-meta text-muted-foreground"
                        key={dependency}
                      >
                        <ArrowRight className="mt-0.5 h-3 w-3 shrink-0" />
                        <span className="min-w-0 break-words">
                          {teamTaskLabel(dependency, props.tasks) ?? dependency}
                        </span>
                      </li>
                    ))}
                  </ul>
                )}
              </div>
              <div>
                <span className="section-title">Blocked by</span>
                {blockedBy.length === 0 &&
                task.blocked_reason === "" ? (
                  <p className="type-meta text-muted-foreground">Nothing</p>
                ) : (
                  <>
                    {task.blocked_reason !== "" ? (
                      <p className="type-meta text-warning">
                        {task.blocked_reason}
                      </p>
                    ) : null}
                    {blockedBy.length > 0 ? (
                      <ul className="space-y-1.5">
                        {blockedBy.map((blocker) => (
                          <li
                            className="flex items-start gap-1.5 type-meta text-warning"
                            key={blocker}
                          >
                            <Ban className="mt-0.5 h-3 w-3 shrink-0" />
                            <span className="min-w-0 break-words">
                              {teamTaskLabel(blocker, props.tasks) ?? blocker}
                            </span>
                          </li>
                        ))}
                      </ul>
                    ) : null}
                  </>
                )}
              </div>
            </div>

            {task.raw_status !== task.status ? (
              <p className="type-meta text-muted-foreground">
                Server status: <span className="mono">{task.raw_status}</span>
              </p>
            ) : null}
          </div>
        </DialogContent>
      ) : null}
    </Dialog>
  );
}

// ---------------------------------------------------------------------------
// Mailbox
// ---------------------------------------------------------------------------

function MailboxTab(props: {
  workspace: TeamWorkspace;
  summary: TeamSummary | null;
  box: MailboxBox;
  onBoxChange: (box: MailboxBox) => void;
  taskId: string;
}) {
  const allMessages = [...props.workspace.inbox, ...props.workspace.sent];
  const messages = mailboxRows(allMessages, props.box);
  const inboxUnread = unreadCount(props.workspace.inbox);
  const coordinator = props.summary?.lead ?? null;

  return (
    <div className="space-y-3">
      <div className="flex flex-wrap items-center gap-2">
        <div
          aria-label="Mailbox folder"
          className="flex items-center gap-1"
          role="group"
        >
          <SubtabButton
            active={props.box === "inbox"}
            onClick={() => props.onBoxChange("inbox")}
          >
            <Inbox className="h-3.5 w-3.5" />
            Inbox
            {inboxUnread > 0 ? (
              <Badge variant="warning" className="px-1.5 py-0 type-badge">
                {inboxUnread}
              </Badge>
            ) : null}
          </SubtabButton>
          <SubtabButton
            active={props.box === "sent"}
            onClick={() => props.onBoxChange("sent")}
          >
            <Send className="h-3.5 w-3.5" />
            Sent
          </SubtabButton>
        </div>
        <CountBadge>{messages.length}</CountBadge>
        <span className="ml-auto type-meta text-muted-foreground">
          Coordinator mailbox
          {coordinator === null ? null : (
            <>
              {" · "}
              <span className="mono text-foreground">{coordinator}</span>
            </>
          )}
        </span>
      </div>

      {messages.length === 0 ? (
        <EmptyPanel
          description={
            props.box === "inbox"
              ? "No direct or broadcast messages have reached this team yet."
              : "This team has not sent any direct or broadcast messages yet."
          }
          icon={props.box === "inbox" ? <Inbox className="h-5 w-5" /> : <Send className="h-5 w-5" />}
          title={props.box === "inbox" ? "Inbox is empty" : "No sent messages"}
        />
      ) : (
        <ul className="space-y-2">
          {messages.map((message) => (
            <li
              className={cn(
                "rounded-lg border bg-card px-4 py-3",
                message.read
                  ? "border-border"
                  : "border-accent-foreground/30 bg-accent/20",
              )}
              key={message.id}
            >
              <div className="flex flex-wrap items-start justify-between gap-2">
                <div className="flex min-w-0 items-start gap-2">
                  {!message.read ? (
                    <span
                      aria-label="Unread"
                      className="mt-1.5 h-2 w-2 shrink-0 rounded-full bg-accent-foreground"
                      title="Unread"
                    />
                  ) : null}
                  <div className="min-w-0">
                    <p className="type-table font-semibold text-foreground">
                      {message.subject}
                    </p>
                    <p className="mt-0.5 flex flex-wrap items-center gap-1 type-meta text-muted-foreground">
                      <span className="mono font-semibold text-foreground">
                        {message.sender}
                      </span>
                      <RoleBadge
                        role={teamAuthorRole(message.sender, props.summary)}
                      />
                      <ArrowRight className="h-3 w-3" />
                      <span>{message.recipient}</span>
                    </p>
                  </div>
                </div>
                <span
                  className="time shrink-0"
                  title={
                    message.created_at === null
                      ? undefined
                      : formatLocalTimestamp(message.created_at)
                  }
                >
                  {message.created_at === null
                    ? "—"
                    : formatLocalClock(message.created_at)}
                </span>
              </div>

              {message.body !== "" ? (
                <p className="mt-2 line-clamp-2 type-meta text-muted-foreground">
                  {message.body}
                </p>
              ) : null}

              <div className="mt-2 flex flex-wrap items-center gap-2">
                {message.task_id !== null ? (
                  <Link
                    className="inline-flex items-center gap-1 type-meta font-medium text-accent-foreground hover:underline"
                    href={`/tasks/${props.taskId}/work`}
                    title={message.task_id}
                  >
                    <ChevronRight className="h-3 w-3" />
                    {message.task_title ??
                      props.workspace.tasks.find(
                        (task) => task.id === message.task_id,
                      )?.title ??
                      `task ${shortenId(message.task_id)}`}
                  </Link>
                ) : (
                  <span className="type-meta text-muted-foreground">
                    no task link
                  </span>
                )}
                <span className="type-meta text-muted-foreground">
                  · {message.read ? "read" : "unread"}
                </span>
              </div>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Mission log
// ---------------------------------------------------------------------------

function MissionTab(props: {
  entries: readonly MissionEntry[];
  summary: TeamSummary | null;
}) {
  if (props.entries.length === 0) {
    return (
      <EmptyPanel
        description="Progress, handoffs, blockers, decisions, and failures will be recorded here."
        icon={<Radio className="h-5 w-5" />}
        title="Mission log is empty"
      />
    );
  }
  return (
    <ol className="relative space-y-3 before:absolute before:bottom-3 before:left-[13px] before:top-3 before:w-px before:bg-border">
      {props.entries.map((entry) => (
        <li className="relative pl-9" key={entry.id}>
          <span
            aria-hidden="true"
            className="absolute left-[7px] top-4 flex h-3 w-3 items-center justify-center rounded-full border-2 border-card bg-muted-foreground"
          />
          <div className="rounded-lg border border-border bg-card px-4 py-3">
            <div className="flex flex-wrap items-center gap-2">
              <Badge variant={missionKindVariant(entry.kind)}>
                {missionKindLabel(entry.kind)}
              </Badge>
              {entry.actor !== null ? (
                <span className="mono font-semibold">{entry.actor}</span>
              ) : null}
              <RoleBadge role={teamAuthorRole(entry.actor, props.summary)} />
              <span
                className="time ml-auto"
                title={
                  entry.created_at === null
                    ? undefined
                    : formatLocalTimestamp(entry.created_at)
                }
              >
                {entry.created_at === null
                  ? ""
                  : formatLocalTimestamp(entry.created_at)}
              </span>
            </div>
            <p className="mt-2 type-body text-foreground">{entry.summary}</p>
            {entry.evidence !== null ? (
              <div className="mt-2 rounded-md border border-border bg-surface-subtle px-3 py-2">
                <span className="section-title mb-1">Evidence</span>
                <p className="whitespace-pre-wrap break-words type-meta text-muted-foreground">
                  {entry.evidence}
                </p>
              </div>
            ) : null}
            {entry.next_action !== null ? (
              <p className="mt-2 flex items-start gap-1.5 type-meta text-foreground">
                <ChevronRight className="mt-0.5 h-3.5 w-3.5 shrink-0 text-accent-foreground" />
                <span>
                  <span className="font-semibold">Next:</span>{" "}
                  {entry.next_action}
                </span>
              </p>
            ) : null}
          </div>
        </li>
      ))}
    </ol>
  );
}

// ---------------------------------------------------------------------------
// Outcomes
// ---------------------------------------------------------------------------

function FragmentCard(props: {
  fragment: OutcomeFragment;
  summary: TeamSummary | null;
}) {
  const { fragment } = props;
  return (
    <div className="rounded-lg border border-border bg-surface-subtle px-3 py-3">
      <div className="flex flex-wrap items-center gap-2">
        <Badge variant={reviewStatusVariant(fragment.review_status)}>
          {fragmentReviewLabel(fragment.review_status)}
        </Badge>
        {fragment.section !== null ? (
          <Badge variant="outline" className="font-normal">
            {fragment.section}
          </Badge>
        ) : null}
        <span className="ml-auto inline-flex items-center gap-1 type-meta text-muted-foreground">
          {fragment.source_agent === null
            ? "unknown agent"
            : fragment.source_agent}
          <RoleBadge
            role={teamAuthorRole(fragment.source_agent, props.summary)}
          />
        </span>
      </div>
      {fragment.content !== "" ? (
        <MarkdownMath
          className="mt-2 type-body [&_p+p]:mt-1.5 [&_ul]:my-1 [&_ol]:my-1 [&_li]:my-0.5"
          text={fragment.content}
        />
      ) : (
        <p className="mt-2 type-meta text-muted-foreground">
          No fragment content recorded.
        </p>
      )}
    </div>
  );
}

function OutcomeCard(props: {
  outcome: TeamOutcome;
  summary: TeamSummary | null;
}) {
  const { outcome } = props;
  const coverage = outcomeSectionCoverage(outcome);
  const finalize = outcomeFinalizeState(outcome);
  const status = normalizeOutcomeStatus(outcome.status);

  return (
    <article className="rounded-xl border border-border bg-card p-4">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <h3 className="type-section font-semibold text-foreground">
            {outcome.title}
          </h3>
          <div className="mt-1.5 flex flex-wrap items-center gap-1.5">
            <Badge variant={outcomeStatusVariant(status)}>
              {outcomeStatusLabel(status)}
            </Badge>
            <Badge variant={finalize.variant}>{finalize.label}</Badge>
            <CountBadge>
              {coverage.covered}/{coverage.total} sections
            </CountBadge>
            {outcome.created_by !== null ? (
              <span className="inline-flex items-center gap-1 type-meta text-muted-foreground">
                by <span className="mono text-foreground">{outcome.created_by}</span>
                <RoleBadge
                  role={teamAuthorRole(outcome.created_by, props.summary)}
                />
              </span>
            ) : null}
          </div>
        </div>
        {outcome.finalized_at !== null ? (
          <span className="time">
            finalized {formatLocalTimestamp(outcome.finalized_at)}
          </span>
        ) : null}
      </div>

      <div className="mt-3">
        <span className="section-title">Required sections</span>
        {outcome.required_sections.length === 0 ? (
          <p className="type-meta text-muted-foreground">
            No required sections declared.
          </p>
        ) : (
          <div className="flex flex-wrap gap-1.5">
            {outcome.required_sections.map((section) => {
              const missing = coverage.missing.includes(section);
              const awaiting = coverage.awaiting.includes(section);
              return (
                <Badge
                  className="font-normal"
                  key={section}
                  variant={
                    missing ? "muted" : awaiting ? "warning" : "success"
                  }
                >
                  {missing ? (
                    <CircleDot className="h-3 w-3" />
                  ) : awaiting ? (
                    <Radio className="h-3 w-3" />
                  ) : (
                    <CheckCircle2 className="h-3 w-3" />
                  )}
                  {section}
                </Badge>
              );
            })}
          </div>
        )}
      </div>

      <div className="mt-3">
        <span className="section-title">
          Fragments{" "}
          <span className="font-normal normal-case tracking-normal">
            ({outcome.fragments.length})
          </span>
        </span>
        {outcome.fragments.length === 0 ? (
          <InlineEmpty text="No outcome fragments attached yet." />
        ) : (
          <div className="grid gap-2">
            {outcome.fragments.map((fragment) => (
              <FragmentCard
                fragment={fragment}
                key={fragment.id}
                summary={props.summary}
              />
            ))}
          </div>
        )}
      </div>

      <p className="mt-3 flex items-center gap-1.5 type-meta text-muted-foreground">
        <FileCheck2 className="h-3.5 w-3.5" />
        {finalize.ready
          ? finalize.label === "Finalized"
            ? "This outcome is complete and final."
            : "Every required section has an approved fragment."
          : `${coverage.missing.length} missing · ${coverage.awaiting.length} awaiting review`}
      </p>
    </article>
  );
}

function OutcomesTab(props: {
  outcomes: readonly TeamOutcome[];
  summary: TeamSummary | null;
}) {
  if (props.outcomes.length === 0) {
    return (
      <EmptyPanel
        description="The coordinator and teammates attach fragments for each required section; finalized outcomes appear here."
        icon={<FileCheck2 className="h-5 w-5" />}
        title="No outcomes yet"
      />
    );
  }
  return (
    <div className="grid gap-3">
      {props.outcomes.map((outcome) => (
        <OutcomeCard
          key={outcome.id}
          outcome={outcome}
          summary={props.summary}
        />
      ))}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Runs
// ---------------------------------------------------------------------------

function RunCard(props: {
  run: TeamRun;
  tasks: readonly TeamTask[];
  taskPageId: string;
}) {
  const { run } = props;
  const progress = runProgressPercent(run);
  return (
    <div className="rounded-lg border border-border bg-card px-4 py-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="flex flex-wrap items-center gap-2">
          <span className="mono font-semibold">{run.agent}</span>
          <Badge variant={runStatusVariant(run.status)}>
            {runStatusLabel(run.status)}
          </Badge>
        </div>
        <span className="time">
          {run.updated_at === null ? "not started" : formatLocalClock(run.updated_at)}
        </span>
      </div>
      <p className="mt-1.5 type-meta text-muted-foreground">
        Task:{" "}
        {run.task_id === null ? (
          "unassigned"
        ) : (
          <Link
            className="font-medium text-accent-foreground hover:underline"
            href={`/tasks/${props.taskPageId}/work`}
            title={run.task_id}
          >
            {run.task_title ??
              teamTaskLabel(run.task_id, props.tasks) ??
              `task ${shortenId(run.task_id)}`}
          </Link>
        )}
      </p>
      {progress !== null ? (
        <div className="mt-2 flex items-center gap-2">
          <div
            aria-label={`Progress ${progress}%`}
            aria-valuemax={100}
            aria-valuemin={0}
            aria-valuenow={progress}
            className="h-1.5 min-w-0 flex-1 overflow-hidden rounded-full bg-muted"
            role="progressbar"
          >
            <span
              className="block h-full rounded-full bg-accent-foreground transition-[width]"
              style={{ width: `${progress}%` }}
            />
          </div>
          <span className="type-meta tabular-nums text-muted-foreground">
            {progress}%
          </span>
        </div>
      ) : null}
      {run.activity !== "" || run.progress_message !== "" ? (
        <p className="mt-2 line-clamp-2 type-meta text-muted-foreground">
          <span className="font-semibold text-foreground">Progress:</span>{" "}
          {run.progress_message !== ""
            ? run.progress_message
            : run.activity}
        </p>
      ) : null}
      {run.result_summary !== "" ? (
        <p className="mt-2 line-clamp-2 type-meta text-muted-foreground">
          {run.result_summary}
        </p>
      ) : null}
      {run.error !== null ? (
        <p className="mt-2 line-clamp-2 type-meta text-destructive">
          {run.error}
        </p>
      ) : null}
    </div>
  );
}

function RunsTab(props: {
  runs: readonly TeamRun[];
  tasks: readonly TeamTask[];
  taskPageId: string;
}) {
  const counts = teamRunCounts(props.runs);
  if (props.runs.length === 0) {
    return (
      <EmptyPanel
        description="Sync and async teammate executions, their progress, and result summaries appear here."
        icon={<Waypoints className="h-5 w-5" />}
        title="No team runs yet"
      />
    );
  }
  return (
    <div className="space-y-3">
      <div className="flex flex-wrap items-center gap-1.5">
        <Badge variant="info">{counts.active} active</Badge>
        <Badge variant="muted">{counts.queued} queued</Badge>
        <Badge variant="success">{counts.completed} completed</Badge>
        {counts.failed > 0 ? (
          <Badge variant="destructive">{counts.failed} failed</Badge>
        ) : null}
        <CountBadge>{counts.total} total</CountBadge>
      </div>
      <div className="grid gap-2">
        {props.runs.map((run) => (
          <RunCard
            key={run.id}
            run={run}
            taskPageId={props.taskPageId}
            tasks={props.tasks}
          />
        ))}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Swarm workspace shell
// ---------------------------------------------------------------------------

/**
 * Audit view of a task's DHI+ swarm.
 *
 * The default tab is the shared task board; the team message graph is
 * deliberately secondary (last tab). Team state is read-only here — agents
 * mutate it through tools; only scale approvals are a human action.
 */
export function SwarmMessagesCard(props: { taskId: string }) {
  const { taskId } = props;
  const [workspace, setWorkspace] = useState<TeamWorkspace | null>(null);
  const [activeTab, setActiveTab] = useState<SwarmTabId>(DEFAULT_SWARM_TAB);
  const [mailbox, setMailbox] = useState<MailboxBox>("inbox");
  const [selectedTask, setSelectedTask] = useState<TeamTask | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const [graphMessages, setGraphMessages] = useState<
    TeamMessageRecord[] | null
  >(null);
  const [graphLoading, setGraphLoading] = useState(false);
  const [graphError, setGraphError] = useState<string | null>(null);
  const [graphLoaded, setGraphLoaded] = useState(false);

  const mounted = useRef(true);
  const workspaceSeq = useRef(0);
  const graphSeq = useRef(0);
  const activeTabRef = useRef(activeTab);
  const graphLoadedRef = useRef(graphLoaded);

  useEffect(() => {
    activeTabRef.current = activeTab;
  }, [activeTab]);

  useEffect(() => {
    graphLoadedRef.current = graphLoaded;
  }, [graphLoaded]);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  const loadWorkspace = useCallback(
    async (background = false): Promise<void> => {
      const token = ++workspaceSeq.current;
      if (!background) {
        setLoading(true);
        setError(null);
      }
      try {
        const next = await loadTeamWorkspace(taskId);
        if (!mounted.current || token !== workspaceSeq.current) {
          return;
        }
        setWorkspace(next);
        setError(null);
      } catch (err) {
        if (!mounted.current || token !== workspaceSeq.current || background) {
          return;
        }
        setError(describeApiError(err));
      } finally {
        // The newest request owns the spinner, even when it superseded a
        // foreground load; order is guaranteed by the sequence token.
        if (mounted.current && token === workspaceSeq.current) {
          setLoading(false);
        }
      }
    },
    [taskId],
  );

  const loadGraph = useCallback(
    async (background = false): Promise<void> => {
      const token = ++graphSeq.current;
      if (!background) {
        setGraphLoading(true);
        setGraphError(null);
      }
      try {
        const records = await getTeamMessages(taskId);
        if (!mounted.current || token !== graphSeq.current) {
          return;
        }
        setGraphMessages(records);
        setGraphError(null);
        setGraphLoaded(true);
      } catch (err) {
        if (!mounted.current || token !== graphSeq.current || background) {
          return;
        }
        setGraphError(describeApiError(err));
      } finally {
        if (mounted.current && token === graphSeq.current) {
          setGraphLoading(false);
        }
      }
    },
    [taskId],
  );

  // Reset local state when the route moves to another task; the parent also
  // keys this card, but the guard keeps it correct on its own.
  useEffect(() => {
    workspaceSeq.current += 1;
    graphSeq.current += 1;
    setWorkspace(null);
    setActiveTab(DEFAULT_SWARM_TAB);
    setMailbox("inbox");
    setSelectedTask(null);
    setLoading(false);
    setError(null);
    setGraphMessages(null);
    setGraphLoaded(false);
    setGraphLoading(false);
    setGraphError(null);
  }, [taskId]);

  useEffect(() => {
    void loadWorkspace();
  }, [loadWorkspace]);

  // The graph is secondary and lazy: only fetch when its tab is opened.
  useEffect(() => {
    if (
      activeTab === "graph" &&
      !graphLoaded &&
      !graphLoading &&
      graphError === null
    ) {
      void loadGraph();
    }
  }, [activeTab, graphError, graphLoaded, graphLoading, loadGraph]);

  // Live refresh: workspace every 5 s, plus the graph only while its tab is
  // open and has already loaded. Hidden documents pause both; becoming
  // visible again refreshes immediately.
  useEffect(() => {
    const refresh = (): void => {
      if (!shouldPollTeamWorkspace(document.visibilityState)) {
        return;
      }
      void loadWorkspace(true);
      if (activeTabRef.current === "graph" && graphLoadedRef.current) {
        void loadGraph(true);
      }
    };

    const timer = setInterval(refresh, TEAM_WORKSPACE_POLL_MS);
    const onVisibility = (): void => {
      if (shouldPollTeamWorkspace(document.visibilityState)) {
        refresh();
      }
    };
    document.addEventListener("visibilitychange", onVisibility);

    return () => {
      clearInterval(timer);
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [loadGraph, loadWorkspace]);

  const tasks = workspace?.tasks ?? [];
  const inferredWorkers = useMemo(
    () =>
      new Set(
        tasks
          .map((task) => task.assignee)
          .filter((assignee): assignee is string => assignee !== null),
      ).size,
    [tasks],
  );
  const workerCount =
    workspace?.summary?.worker_count ?? inferredWorkers;
  const runs = workspace?.runs ?? [];
  const runCounts = teamRunCounts(runs);
  const unread =
    workspace === null
      ? 0
      : unreadCount(workspace.inbox);

  const counts: Record<SwarmTabId, number | null> = {
    board: workspace === null ? null : workspace.tasks.length,
    mailbox: workspace === null ? null : unread,
    mission: workspace === null ? null : workspace.mission.length,
    outcomes: workspace === null ? null : workspace.outcomes.length,
    runs: workspace === null ? null : runCounts.active,
    graph: null,
  };

  const selectTab = (tab: SwarmTabId): void => {
    setActiveTab(tab);
  };

  return (
    <Card>
      <CardHeader className="flex-row flex-wrap items-start justify-between gap-3 border-b border-border pb-4">
        <div className="flex items-start gap-2.5">
          <span className="flex h-8 w-8 items-center justify-center rounded-lg bg-accent text-accent-foreground">
            <MessageSquareText className="h-4 w-4" />
          </span>
          <div>
            <CardTitle>Swarm workspace</CardTitle>
            <CardDescription className="mt-0.5">
              Shared task board, mailbox, mission log, outcomes, and runs.
            </CardDescription>
          </div>
        </div>
        <div className="flex flex-wrap items-center gap-1.5">
          {workspace !== null ? (
            <>
              <Badge variant="outline" className="font-normal">
                <Users className="h-3 w-3" />
                {teamHeaderLabel(workerCount)}
              </Badge>
              <Badge variant={runCounts.active > 0 ? "accent" : "muted"} className="font-normal">
                {runCounts.active} active run
                {runCounts.active === 1 ? "" : "s"}
              </Badge>
              <Badge variant={unread > 0 ? "warning" : "muted"} className="font-normal">
                <Mail className="h-3 w-3" />
                {unread} unread
              </Badge>
            </>
          ) : null}
          <Button
            className="h-8 rounded-full"
            disabled={loading}
            onClick={() => {
              void loadWorkspace();
              if (activeTab === "graph") {
                void loadGraph();
              }
            }}
            size="sm"
            type="button"
            variant="outline"
          >
            <RefreshCw
              className={`h-3.5 w-3.5 ${loading ? "animate-spin" : ""}`}
            />
            {loading ? "Loading…" : workspace !== null ? "Refresh" : "Load"}
          </Button>
        </div>
      </CardHeader>

      <CardContent className="pt-4">
        <div
          aria-label="Swarm workspace tabs"
          className="mb-4 flex flex-wrap items-center gap-1 border-b border-border pb-2"
          role="tablist"
        >
          {SWARM_TABS.map((tab) => {
            const selected = tab.id === activeTab;
            return (
              <button
                key={tab.id}
                aria-selected={selected}
                className={`inline-flex h-8 items-center gap-1.5 rounded-full border px-3 type-label font-medium transition-colors ${
                  selected
                    ? "border-accent-foreground/30 bg-accent text-accent-foreground"
                    : "border-transparent text-muted-foreground hover:bg-surface-subtle hover:text-foreground"
                }`}
                onClick={() => selectTab(tab.id)}
                role="tab"
                type="button"
              >
                {tab.id === "board" ? <ListTodo className="h-3.5 w-3.5" /> : null}
                {tab.id === "mailbox" ? <Mail className="h-3.5 w-3.5" /> : null}
                {tab.id === "mission" ? <Radio className="h-3.5 w-3.5" /> : null}
                {tab.id === "outcomes" ? <FileCheck2 className="h-3.5 w-3.5" /> : null}
                {tab.id === "runs" ? <Waypoints className="h-3.5 w-3.5" /> : null}
                {tab.id === "graph" ? <GitBranch className="h-3.5 w-3.5" /> : null}
                {tab.label}
                {counts[tab.id] !== null && counts[tab.id]! > 0 ? (
                  <span className="type-meta tabular-nums text-muted-foreground">
                    {counts[tab.id]}
                  </span>
                ) : null}
              </button>
            );
          })}
        </div>

        {error !== null ? (
          <div className="errorbox mt-0" role="alert">
            Could not load team state: {error}
            <div className="mt-2">
              <Button
                className="h-7"
                onClick={() => void loadWorkspace()}
                size="sm"
                type="button"
                variant="outline"
              >
                Retry
              </Button>
            </div>
          </div>
        ) : null}

        {loading && workspace === null && error === null ? (
          <p className="empty">Loading team state…</p>
        ) : null}

        {workspace === null && error === null && !loading ? (
          <EmptyPanel
            description="This task has no persisted Swarm state yet, or the server predates the team routes."
            icon={<Users className="h-5 w-5" />}
            title="No team state recorded"
          />
        ) : null}

        {workspace !== null && error === null ? (
          <>
            {activeTab === "board" ? (
              <TaskBoard
                onOpenTask={setSelectedTask}
                summary={workspace.summary}
                tasks={workspace.tasks}
              />
            ) : null}

            {activeTab === "mailbox" ? (
              <MailboxTab
                box={mailbox}
                onBoxChange={setMailbox}
                summary={workspace.summary}
                taskId={taskId}
                workspace={workspace}
              />
            ) : null}

            {activeTab === "mission" ? (
              <MissionTab
                entries={workspace.mission}
                summary={workspace.summary}
              />
            ) : null}

            {activeTab === "outcomes" ? (
              <OutcomesTab
                outcomes={workspace.outcomes}
                summary={workspace.summary}
              />
            ) : null}

            {activeTab === "runs" ? (
              <RunsTab
                runs={workspace.runs}
                taskPageId={taskId}
                tasks={workspace.tasks}
              />
            ) : null}

            {activeTab === "graph" ? (
              <TeamGraphPanel
                error={graphError}
                loading={graphLoading}
                messages={graphMessages}
                onLoad={() => void loadGraph()}
              />
            ) : null}
          </>
        ) : null}

        <TaskDetailsDialog
          onClose={() => setSelectedTask(null)}
          summary={workspace?.summary ?? null}
          task={selectedTask}
          tasks={workspace?.tasks ?? []}
        />
      </CardContent>
    </Card>
  );
}

// ---------------------------------------------------------------------------
// Graph tab (all team messages: direct and broadcast)
// ---------------------------------------------------------------------------

/** Data attached to every React Flow worker node. */
type WorkerNodeData = {
  label: ReactNode;
  sender: string;
  count: number;
  selected: boolean;
  recipient: boolean;
};

/**
 * Fixed node box. The height fits the sender chip, the message count, and the
 * optional sender/recipient line (with padding and border); the height is
 * declared on the node as well as its style so the MiniMap can draw it.
 */
const SWARM_NODE_WIDTH = 176;
const SWARM_NODE_HEIGHT = 88;

/** Default React Flow nodes with worker labels and selected/recipient tone. */
function buildFlowNodes(model: TeamGraphModel): Node<WorkerNodeData>[] {
  return model.nodes.map((node) => {
    const { sender, count, selected, recipient } = node.data;
    const highlighted = selected || recipient;
    const status = selected ? "sender" : recipient ? "recipient" : null;
    return {
      id: node.id,
      position: node.position,
      width: SWARM_NODE_WIDTH,
      height: SWARM_NODE_HEIGHT,
      data: {
        sender,
        count,
        selected,
        recipient,
        label: (
          <div className="flex h-full w-[150px] flex-col items-center justify-center gap-1">
            <span className="mono max-w-full truncate font-semibold leading-tight">
              {sender}
            </span>
            <span className="type-label text-muted-foreground">
              {count} message{count === 1 ? "" : "s"}
            </span>
            {status !== null ? (
              <span
                className="type-label font-semibold uppercase tracking-[0.08em]"
                style={{ color: model.color }}
              >
                {status}
              </span>
            ) : null}
          </div>
        ),
      },
      style: {
        width: SWARM_NODE_WIDTH,
        height: SWARM_NODE_HEIGHT,
        cursor: "pointer",
        padding: "8px 10px",
        borderRadius: 12,
        border: `1.5px solid ${highlighted ? model.color : "var(--border)"}`,
        background: selected
          ? `color-mix(in srgb, ${model.color} 16%, var(--card))`
          : "var(--card)",
        color: "var(--card-foreground)",
        boxShadow: selected
          ? `0 0 0 3px color-mix(in srgb, ${model.color} 25%, transparent)`
          : undefined,
        opacity: model.selected !== null && !highlighted ? 0.72 : 1,
        transition: "opacity 120ms ease, border-color 120ms ease",
      },
      sourcePosition: Position.Right,
      targetPosition: Position.Left,
      draggable: false,
    };
  });
}

/** One edge per direct recipient, or one per peer for a broadcast. */
function buildFlowEdges(model: TeamGraphModel): Edge[] {
  return model.edges.map((edge) => ({
    id: edge.id,
    source: edge.source,
    target: edge.target,
    type: "smoothstep",
    style: { stroke: edge.color, strokeWidth: 2 },
    markerEnd: {
      type: MarkerType.ArrowClosed,
      color: edge.color,
      width: 18,
      height: 18,
    },
  }));
}

/**
 * One selected message: direction, sender, recipients, full body, and
 * metadata. Kept compact; the graph is still a secondary audit view.
 */
function TeamMessageDetail(props: { model: TeamGraphModel }) {
  const { model } = props;
  const message = model.selected;
  if (message === null) {
    return null;
  }
  const copyText =
    message.body === ""
      ? message.subject
      : `${message.subject}\n\n${message.body}`;
  return (
    <div className="rounded-lg border border-border bg-surface-subtle p-4">
      <div className="flex flex-wrap items-center gap-x-2 gap-y-1.5">
        <Badge
          className="font-normal"
          variant={message.broadcast ? "info" : "outline"}
        >
          {message.broadcast ? "Broadcast" : "Direct"}
        </Badge>
        <span className="mono font-semibold">{message.from}</span>
        <ArrowRight className="h-3 w-3 text-muted-foreground" />
        <span className="mono">
          {message.broadcast ? "all teammates" : message.to}
        </span>
        <span className="type-meta text-muted-foreground">
          {message.broadcast
            ? `${model.recipients.length} recipient${model.recipients.length === 1 ? "" : "s"}`
            : "direct message"}
        </span>
        <CopyButton
          className="ml-auto"
          label="Copy"
          size="sm"
          text={copyText}
        />
      </div>
      <p className="mt-2 type-table font-semibold text-foreground">
        {message.subject}
      </p>
      {message.body !== "" ? (
        <MarkdownMath
          className="mt-1 max-h-96 overflow-auto type-goal [&_p+p]:mt-2 [&_ul]:my-1.5 [&_ol]:my-1.5 [&_li]:my-0.5 [&_pre]:my-1.5 [&_blockquote]:my-1.5 [&_.katex-display]:my-1.5"
          text={message.body}
          variant="block"
        />
      ) : (
        <p className="mt-1 type-meta text-muted-foreground">
          No message body recorded.
        </p>
      )}
      <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 type-meta text-muted-foreground">
        <span>
          id <span className="mono text-foreground">#{message.id}</span>
        </span>
        <span>
          from <span className="mono">{message.from}</span>
        </span>
        <span>
          to <span className="mono">{message.to ?? "all"}</span>
        </span>
        <span
          title={
            message.sentAt === ""
              ? undefined
              : formatLocalTimestamp(message.sentAt)
          }
        >
          {message.sentAt === "" ? "—" : formatLocalClock(message.sentAt)}
        </span>
      </div>
    </div>
  );
}

function TeamGraphPanel(props: {
  messages: TeamMessageRecord[] | null;
  loading: boolean;
  error: string | null;
  onLoad: () => void;
}) {
  const { messages, loading, error } = props;
  const [selectedMessageId, setSelectedMessageId] = useState<string | null>(
    null,
  );
  const colorMode = useColorMode();

  const model = useMemo(
    () =>
      messages === null
        ? null
        : buildTeamMessageGraph(messages, selectedMessageId),
    [messages, selectedMessageId],
  );
  const flowNodes = useMemo(
    () => (model === null ? [] : buildFlowNodes(model)),
    [model],
  );
  const flowEdges = useMemo(
    () => (model === null ? [] : buildFlowEdges(model)),
    [model],
  );

  const total = messages?.length ?? 0;
  const broadcastCount =
    messages?.filter((message) => message.to_agent_id === null).length ?? 0;
  const directCount = total - broadcastCount;
  const senderCount = useMemo(
    () => new Set((messages ?? []).map((message) => message.from_agent_id)).size,
    [messages],
  );

  const handleNodeClick = useCallback(
    (_event: MouseEvent, node: Node) => {
      if (model === null) {
        return;
      }
      const sender = (node.data as WorkerNodeData).sender;
      const first = model.chips.find(
        (chip) =>
          chip.message.from === sender || chip.message.to === sender,
      );
      if (first !== undefined) {
        setSelectedMessageId(first.message.id);
      }
    },
    [model],
  );

  return (
    <div className="space-y-3">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h3 className="type-section font-semibold text-foreground">
            Team message graph
          </h3>
          <p className="mt-0.5 type-meta text-muted-foreground">
            A direct message draws one edge; a broadcast fans out to every
            other teammate. Secondary to the shared task state.
          </p>
        </div>
        <div className="flex items-center gap-2">
          {messages !== null ? (
            <Badge variant="outline" className="font-mono font-normal">
              {total}
            </Badge>
          ) : null}
          <Button
            className="h-8 rounded-full"
            disabled={loading}
            onClick={props.onLoad}
            size="sm"
            type="button"
            variant="outline"
          >
            <RefreshCw
              className={`h-3.5 w-3.5 ${loading ? "animate-spin" : ""}`}
            />
            {loading ? "Loading…" : messages !== null ? "Refresh" : "Load"}
          </Button>
        </div>
      </div>

      {error !== null ? (
        <div className="errorbox mt-0" role="alert">
          Could not load team messages: {error}
        </div>
      ) : null}

      {loading && messages === null ? (
        <p className="empty">Loading team messages…</p>
      ) : null}

      {messages !== null && total === 0 ? (
        <div className="rounded-lg border border-dashed border-border bg-surface-subtle px-4 py-7 text-center type-meta text-muted-foreground">
          No team messages recorded for this task.
        </div>
      ) : null}

      {messages !== null && total > 0 ? (
        <>
          <div className="flex flex-wrap items-center gap-1.5">
            <Badge variant="outline" className="font-normal">
              {total} message{total === 1 ? "" : "s"}
            </Badge>
            <Badge variant="outline" className="font-normal">
              {senderCount} sender{senderCount === 1 ? "" : "s"}
            </Badge>
            <Badge variant="outline" className="font-normal">
              {directCount} direct
            </Badge>
            <Badge variant="outline" className="font-normal">
              {broadcastCount} broadcast{broadcastCount === 1 ? "" : "s"}
            </Badge>
          </div>

          {model !== null && model.chips.length > 0 ? (
            <>
              <div
                aria-label="Team messages in the graph"
                className="flex flex-wrap gap-1.5"
                role="group"
              >
                {model.chips.map((chip) => (
                  <button
                    key={chip.id}
                    aria-pressed={chip.selected}
                    className={cn(
                      "inline-flex max-w-full items-center gap-1.5 rounded-full border px-2.5 py-1.5 type-label transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
                      chip.selected
                        ? "border-accent-foreground/40 bg-accent text-accent-foreground"
                        : "border-border bg-card text-foreground hover:bg-surface-subtle",
                    )}
                    onClick={() => setSelectedMessageId(chip.message.id)}
                    title={chip.label}
                    type="button"
                  >
                    <span
                      aria-hidden="true"
                      className="h-1.5 w-1.5 shrink-0 rounded-full"
                      style={{ backgroundColor: model.color }}
                    />
                    <span className="mono font-medium">
                      {chip.message.from}
                    </span>
                    <ArrowRight className="h-3 w-3 shrink-0 text-muted-foreground" />
                    <span className="mono">
                      {chip.message.broadcast ? "all" : chip.message.to}
                    </span>
                    <span aria-hidden="true" className="text-muted-foreground">
                      ·
                    </span>
                    <span className="max-w-[16rem] truncate">
                      {chip.message.subject}
                    </span>
                    {chip.message.broadcast ? (
                      <Badge
                        className="px-1.5 py-0 type-badge font-normal"
                        variant="muted"
                      >
                        broadcast
                      </Badge>
                    ) : null}
                  </button>
                ))}
              </div>

              <div
                aria-label="Team message graph"
                className="h-[420px] w-full overflow-hidden rounded-lg border border-border bg-surface-subtle"
                role="group"
              >
                <ReactFlow
                  className="dhi-flow h-full w-full"
                  colorMode={colorMode}
                  edges={flowEdges}
                  elementsSelectable={false}
                  fitView
                  fitViewOptions={{ padding: 0.25 }}
                  maxZoom={1.5}
                  minZoom={0.35}
                  nodes={flowNodes}
                  nodesConnectable={false}
                  nodesDraggable={false}
                  onNodeClick={handleNodeClick}
                >
                  <Background color="var(--border)" gap={20} size={1} />
                  <Controls showInteractive={false} />
                  <MiniMap
                    nodeColor={(node) => {
                      const data = node.data as WorkerNodeData;
                      if (data.selected) {
                        return model.color;
                      }
                      return data.recipient ? "#94a3b8" : "var(--border)";
                    }}
                    nodeStrokeWidth={2}
                    pannable
                    zoomable
                  />
                </ReactFlow>
              </div>

              <TeamMessageDetail model={model} />
            </>
          ) : null}
        </>
      ) : null}
    </div>
  );
}
