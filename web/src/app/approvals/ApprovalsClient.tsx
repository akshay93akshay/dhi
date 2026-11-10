"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import Link from "next/link";
import {
  ApiError,
  decideApproval,
  listApprovals,
  listTasks,
} from "@/lib/api";
import type { ApprovalItem, ApprovalDecision } from "@/lib/types";
import {
  APPROVAL_ARRIVAL_HIGHLIGHT_MS,
  APPROVAL_REFRESH_DEBOUNCE_MS,
  buildApprovalQueue,
  buildDecisionRequest,
  buildScaleDecisionRequest,
  formatApprovalAge,
  renderToolInput,
  scaleApprovalSummary,
  type ApprovalQueueRow,
} from "@/lib/approvals";
import { decideScaleApproval, listScaleApprovals } from "@/lib/team";
import {
  scaleApprovalStatusLabel,
  scaleApprovalStatusVariant,
  type ScaleApproval,
} from "@/lib/teamTypes";
import { plainPreview } from "@/lib/preview";
import { describeApiError, shortenId } from "@/lib/ui";
import { formatLocalTimestamp } from "@/lib/localTime";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
} from "@/components/ui/card";
import { PageHeader } from "@/components/ui/page-header";
import { StateBadge } from "@/components/ui/state-badge";
import {
  Check,
  Inbox,
  RefreshCw,
  ShieldCheck,
  Users,
  Wrench,
  X,
} from "lucide-react";

/** Poll cadence for the queue (5 s, visible tabs only). */
const POLL_INTERVAL_MS = 5000;

/** One persisted race report: replaced by the next race, dismissed by hand. */
type RaceNotice = { key: string; text: string };

/**
 * Approvals queue. Every guarded tool call that answered `Ask` parks
 * mid-execution inside `GuardedExecutor` until a human picks Allow or Deny
 * here — deciding wakes the waiting worker automatically, so there is
 * deliberately no third "resume" action.
 *
 * Swarm scale requests share the same queue: `pending` `swarm_scale` rows
 * raise the teammate cap above 8 when allowed and keep it at 8 when denied.
 */
export function ApprovalsClient() {
  // --- Queue state ----------------------------------------------------------
  const [toolApprovals, setToolApprovals] = useState<ApprovalItem[] | null>(
    null,
  );
  const [scaleApprovals, setScaleApprovals] = useState<ScaleApproval[]>([]);
  const [scaleWarning, setScaleWarning] = useState<string | null>(null);
  const [refreshError, setRefreshError] = useState<string | null>(null);
  const [initialError, setInitialError] = useState<string | null>(null);
  const [refreshing, setRefreshing] = useState(false);

  // --- Decisions ------------------------------------------------------------
  const [inflight, setInflight] = useState<ReadonlySet<string>>(new Set());
  const [denyArmedKey, setDenyArmedKey] = useState<string | null>(null);
  /** Distinct surface for refused requests (400) — not a race, not a crash. */
  const [requestError, setRequestError] = useState<string | null>(null);
  const [raceNotice, setRaceNotice] = useState<RaceNotice | null>(null);

  // --- Arrival highlighting --------------------------------------------------
  const [freshKeys, setFreshKeys] = useState<ReadonlySet<string>>(new Set());
  const knownKeysRef = useRef<Set<string> | null>(null);
  const highlightTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  // --- Goal excerpts (one-shot, cheap: existing listTasks plumbing) ----------
  const [taskGoals, setTaskGoals] = useState<Map<string, string>>(new Map());

  // Sequence token so slow responses can never resurrect stale rows over a
  // newer decision-triggered refetch.
  const loadSeqRef = useRef(0);
  const mountedRef = useRef(true);
  const refreshTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  /** True once a load ever succeeded — splits first-load vs refresh errors. */
  const loadedOnceRef = useRef(false);
  /** Latest fresh keys, readable from `load` without joining its deps. */
  const freshKeysRef = useRef<ReadonlySet<string>>(freshKeys);
  freshKeysRef.current = freshKeys;

  const load = useCallback(async (): Promise<void> => {
    const seq = ++loadSeqRef.current;
    setRefreshing(true);
    try {
      // Scale approvals are additive. A missing/failed scale route must not
      // blank the tool queue; a failed tool route still owns the error state.
      const [toolResult, scaleResult] = await Promise.allSettled([
        listApprovals(),
        listScaleApprovals(),
      ]);
      if (!mountedRef.current || seq !== loadSeqRef.current) {
        return;
      }
      if (toolResult.status === "rejected") {
        throw toolResult.reason;
      }
      const tools = toolResult.value;
      const scales =
        scaleResult.status === "fulfilled" ? scaleResult.value : [];
      setScaleWarning(
        scaleResult.status === "rejected"
          ? describeApiError(scaleResult.reason)
          : null,
      );

      // Arrival detection: compare against the previously seen queue keys.
      // The very first load seeds the baseline (nothing flashes on entry).
      const pendingRows = buildApprovalQueue(tools, scales);
      const known = knownKeysRef.current;
      if (known !== null) {
        const arrived = pendingRows.filter((row) => !known.has(row.key));
        if (arrived.length > 0) {
          const stamped = new Set(freshKeysRef.current);
          for (const row of arrived) {
            stamped.add(row.key);
          }
          setFreshKeys(stamped);
          if (highlightTimerRef.current !== null) {
            clearTimeout(highlightTimerRef.current);
          }
          highlightTimerRef.current = setTimeout(() => {
            highlightTimerRef.current = null;
            if (mountedRef.current) {
              setFreshKeys(new Set());
            }
          }, APPROVAL_ARRIVAL_HIGHLIGHT_MS);
        }
      }
      knownKeysRef.current = new Set(pendingRows.map((row) => row.key));
      loadedOnceRef.current = true;
      setToolApprovals(tools);
      setScaleApprovals(scales);
      setRefreshError(null);
      setInitialError(null);
    } catch (err) {
      if (!mountedRef.current || seq !== loadSeqRef.current) {
        return;
      }
      // A failed refresh keeps the previously loaded rows (below) and only
      // degrades the banner — a network blip must never look like an empty
      // queue. A failed FIRST load promotes to the full error state.
      const message = describeApiError(err);
      setRefreshError(message);
      if (!loadedOnceRef.current) {
        setInitialError(message);
      }
    } finally {
      if (mountedRef.current && seq === loadSeqRef.current) {
        setRefreshing(false);
      }
    }
  }, []);

  useEffect(() => {
    mountedRef.current = true;
    void load();
    // Goal excerpts ride one-shot `listTasks` — no new fetch plumbing.
    listTasks()
      .then((tasks) => {
        if (mountedRef.current) {
          setTaskGoals(new Map(tasks.map((task) => [task.id, task.goal])));
        }
      })
      .catch(() => {
        // Excerpts degrade to "task <short-id>"; never an error on its own.
      });

    const timer = setInterval(() => {
      if (
        typeof document !== "undefined" &&
        document.visibilityState !== "visible"
      ) {
        return; // Hidden tabs pause polling.
      }
      void load();
    }, POLL_INTERVAL_MS);

    const onVisibility = (): void => {
      if (document.visibilityState === "visible") {
        void load(); // Immediate refresh when the tab returns.
      }
    };
    const onFocus = (): void => {
      void load(); // Window focus is the other "user is looking" signal.
    };
    document.addEventListener("visibilitychange", onVisibility);
    window.addEventListener("focus", onFocus);

    return () => {
      mountedRef.current = false;
      clearInterval(timer);
      if (refreshTimerRef.current !== null) {
        clearTimeout(refreshTimerRef.current);
        refreshTimerRef.current = null;
      }
      if (highlightTimerRef.current !== null) {
        clearTimeout(highlightTimerRef.current);
        highlightTimerRef.current = null;
      }
      document.removeEventListener("visibilitychange", onVisibility);
      window.removeEventListener("focus", onFocus);
    };
  }, [load]);

  /** Coalesced follow-up refresh: bursts of decisions fetch once. */
  const scheduleRefresh = useCallback((): void => {
    if (refreshTimerRef.current !== null) {
      clearTimeout(refreshTimerRef.current);
    }
    refreshTimerRef.current = setTimeout(() => {
      refreshTimerRef.current = null;
      if (mountedRef.current) {
        void load();
      }
    }, APPROVAL_REFRESH_DEBOUNCE_MS);
  }, [load]);

  /**
   * Drops one row from the local queue immediately (optimistic removal) —
   * used on success and on races; the refetch then settles the rest.
   */
  const dropRow = useCallback(
    (row: ApprovalQueueRow): void => {
      if (row.kind === "tool") {
        setToolApprovals((current) =>
          current === null
            ? null
            : current.filter((item) => item.id !== row.item.id),
        );
      } else {
        setScaleApprovals((current) =>
          current.filter((item) => item.id !== row.item.id),
        );
      }
      if (denyArmedKey === row.key) {
        setDenyArmedKey(null);
      }
    },
    [denyArmedKey],
  );

  const decide = useCallback(
    async (
      row: ApprovalQueueRow,
      decision: ApprovalDecision,
    ): Promise<void> => {
      // Validate at the boundary (typed buttons make this infallible, but
      // the builders own both wire shapes).
      const request =
        row.kind === "tool"
          ? buildDecisionRequest(row.item.id, decision)
          : buildScaleDecisionRequest(row.item.id, decision);
      if (!request.ok) {
        setRequestError(request.error);
        return;
      }

      setRequestError(null);
      setInflight((current) => new Set(current).add(row.key));
      if (decision === "deny") {
        setDenyArmedKey(null); // Confirmation consumed.
      }
      const label = row.kind === "tool" ? "Approval" : "Scale approval";
      try {
        if (row.kind === "tool") {
          await decideApproval(row.item.id, decision);
        } else {
          await decideScaleApproval(row.item.id, decision);
        }
        // Success: the parked tool call proceeds (or fails with "denied")
        // on the engine side; the worker wakes itself — nothing to resume.
        dropRow(row);
        scheduleRefresh();
      } catch (err) {
        if (!(err instanceof ApiError)) {
          setRefreshError(describeApiError(err));
          return;
        }
        if (err.isConflict()) {
          // Someone/something got there first (another operator, a concurrent
          // decision, or the expiry timer). Calm notice + refetch — NOT an
          // error state.
          setRaceNotice({
            key: row.key,
            text: `${label} #${shortenId(row.item.id)} was already decided or expired — list refreshed.`,
          });
          dropRow(row);
          void load();
          return;
        }
        if (err.isNotFound()) {
          setRaceNotice({
            key: row.key,
            text: `${label} #${shortenId(row.item.id)} no longer exists — list refreshed.`,
          });
          dropRow(row);
          void load();
          return;
        }
        if (err.isValidation()) {
          // Genuine invalid input surfaces distinctly (should be unreachable
          // from the fixed two buttons, but never silently swallowed).
          setRequestError(describeApiError(err));
          return;
        }
        // Network/server trouble: keep the row and the stale list, surface
        // the message.
        setRefreshError(describeApiError(err));
      } finally {
        setInflight((current) => {
          const next = new Set(current);
          next.delete(row.key);
          return next;
        });
      }
    },
    [dropRow, load, scheduleRefresh],
  );

  const pending = useMemo(
    () => buildApprovalQueue(toolApprovals ?? [], scaleApprovals),
    [toolApprovals, scaleApprovals],
  );
  const toolPending = pending.filter((row) => row.kind === "tool").length;
  const scalePending = pending.length - toolPending;
  const busy = refreshing || toolApprovals === null;

  return (
    <main className="space-y-5">
      <PageHeader
        eyebrow="Human in the loop"
        icon={<ShieldCheck className="h-3.5 w-3.5" />}
        title="Approvals"
        description="Guarded tool calls and Swarm scale requests parked waiting on a human. A decision wakes the waiting task automatically — no resume step."
        actions={
          <>
            <Badge
              variant={pending.length > 0 ? "warning" : "muted"}
              className="h-9 gap-1.5 rounded-full px-3"
            >
              <span
                className={`h-1.5 w-1.5 rounded-full ${
                  pending.length > 0 ? "bg-warning" : "bg-muted-foreground"
                }`}
              />
              {pending.length} pending
            </Badge>
            <Button
              type="button"
              variant="outline"
              className="h-9 rounded-full"
              onClick={() => void load()}
              disabled={refreshing}
            >
              <RefreshCw
                className={`h-3.5 w-3.5 ${refreshing ? "animate-spin" : ""}`}
              />
              Refresh
            </Button>
          </>
        }
      />

      {raceNotice !== null ? (
        <div className="notice" role="status">
          <span className="flex items-start gap-2">
            <span className="mt-0.5 h-1.5 w-1.5 shrink-0 rounded-full bg-info" />
            {raceNotice.text}
          </span>
          <button
            type="button"
            className="notice-dismiss"
            onClick={() => setRaceNotice(null)}
            aria-label="Dismiss notice"
          >
            ×
          </button>
        </div>
      ) : null}

      {requestError !== null ? (
        <div className="errorbox" role="alert">
          Could not apply the decision: {requestError}
        </div>
      ) : null}

      {scaleWarning !== null ? (
        <div className="notice" role="status">
          <span className="flex items-start gap-2">
            <span className="mt-0.5 h-1.5 w-1.5 shrink-0 rounded-full bg-warning" />
            Scale approvals are unavailable: {scaleWarning}. Tool approvals
            still refresh normally.
          </span>
        </div>
      ) : null}

      {/* Refresh trouble BELOW a successful first load: keep stale rows,
          show the message, offer retry. */}
      {initialError === null && refreshError !== null ? (
        <div className="errorbox" role="alert">
          <div className="flex flex-wrap items-center gap-3">
            <span className="min-w-0 flex-1">Refresh failed: {refreshError}</span>
            <Button
              type="button"
              variant="outline"
              size="sm"
              className="h-7 border-destructive/30 text-destructive hover:bg-destructive/10 hover:text-destructive"
              onClick={() => void load()}
              disabled={refreshing}
            >
              Retry
            </Button>
          </div>
        </div>
      ) : null}

      {/* First-load failure: no stale rows to keep — full error state. */}
      {initialError !== null ? (
        <Card>
          <CardContent className="pt-5">
            <div className="errorbox mt-0" role="alert">
              Could not load the approval queue: {initialError}
            </div>
            <div className="actions">
              <Button type="button" onClick={() => void load()} disabled={refreshing}>
                {refreshing ? "Retrying…" : "Retry"}
              </Button>
              <Button asChild variant="outline">
                <Link href="/">← Overview</Link>
              </Button>
            </div>
          </CardContent>
        </Card>
      ) : null}

      {toolApprovals === null && initialError === null ? (
        <Card>
          <CardContent className="py-12 text-center">
            <RefreshCw className="mx-auto h-5 w-5 animate-spin text-muted-foreground" />
            <p className="mt-3 type-body text-muted-foreground">Loading approvals…</p>
          </CardContent>
        </Card>
      ) : null}

      {toolApprovals !== null && initialError === null ? (
        <>
          {pending.length === 0 ? (
            <Card>
              <CardContent className="flex flex-col items-center justify-center py-16 text-center">
                <span className="flex h-12 w-12 items-center justify-center rounded-2xl bg-success/10 text-success">
                  <Inbox className="h-6 w-6" />
                </span>
                <h2 className="mt-4 type-section font-semibold">Nothing waiting on you</h2>
                <p className="mt-1 max-w-md type-body text-muted-foreground">
                  Guarded tool calls and Swarm scale requests that need a
                  decision will appear here and refresh every{" "}
                  {POLL_INTERVAL_MS / 1000}s while this tab is visible.
                </p>
              </CardContent>
            </Card>
          ) : (
            <>
              <div className="flex flex-wrap items-center gap-1.5">
                <Badge variant="warning" className="font-normal">
                  <Wrench className="h-3 w-3" /> {toolPending} tool
                </Badge>
                <Badge variant="info" className="font-normal">
                  <Users className="h-3 w-3" /> {scalePending} scale
                </Badge>
              </div>
              <div className="grid gap-3">
                {pending.map((row) =>
                  row.kind === "tool" ? (
                    <ApprovalCard
                      key={row.key}
                      item={row.item}
                      goal={taskGoals.get(row.item.task_id) ?? null}
                      busy={inflight.has(row.key)}
                      denying={denyArmedKey === row.key}
                      fresh={freshKeys.has(row.key)}
                      onDecide={(decision) => void decide(row, decision)}
                      onArmDeny={() => setDenyArmedKey(row.key)}
                      onCancelDeny={() =>
                        setDenyArmedKey((current) =>
                          current === row.key ? null : current,
                        )
                      }
                    />
                  ) : (
                    <ScaleApprovalCard
                      key={row.key}
                      item={row.item}
                      goal={taskGoals.get(row.item.task_id) ?? null}
                      busy={inflight.has(row.key)}
                      denying={denyArmedKey === row.key}
                      fresh={freshKeys.has(row.key)}
                      onDecide={(decision) => void decide(row, decision)}
                      onArmDeny={() => setDenyArmedKey(row.key)}
                      onCancelDeny={() =>
                        setDenyArmedKey((current) =>
                          current === row.key ? null : current,
                        )
                      }
                    />
                  ),
                )}
              </div>
            </>
          )}

          {!busy ? (
            <p className="flex items-center gap-2 type-meta text-muted-foreground">
              <RefreshCw className="h-3 w-3" />
              Auto-refreshes every {POLL_INTERVAL_MS / 1000}s while this tab is visible.
            </p>
          ) : null}
        </>
      ) : null}
    </main>
  );
}

// ---------------------------------------------------------------------------
// One pending tool approval card
// ---------------------------------------------------------------------------

function ApprovalCard(props: {
  item: ApprovalItem;
  /** Task goal text when the one-shot lookup knows it; `null` → fallback. */
  goal: string | null;
  /** True while THIS row's decision call is in flight (disables both buttons). */
  busy: boolean;
  /** True when the deny confirmation step is armed for this row. */
  denying: boolean;
  /** True during the brief new-arrival highlight window. */
  fresh: boolean;
  onDecide: (decision: ApprovalDecision) => void;
  onArmDeny: () => void;
  onCancelDeny: () => void;
}) {
  const { item, goal, busy, denying, fresh } = props;
  const inputView = renderToolInput(item.input);

  return (
    <Card className={`approval-card ${fresh ? "arriving" : ""}`}>
      <CardContent className="pt-5">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div className="flex flex-wrap items-center gap-2">
            <span className="mono">{shortenId(item.id)}</span>
            <StateBadge state={item.status} />
            <Badge variant="warning">
              <Wrench className="h-3 w-3" /> tool approval
            </Badge>
          </div>
          <span className="time">waiting {formatApprovalAge(item.created_at)}</span>
        </div>

        <p className="mt-4 type-body text-foreground">
          A tool wants to run:{" "}
          {item.tool_name !== null ? (
            <strong className="font-semibold">{item.tool_name}</strong>
          ) : (
            <strong className="font-semibold text-muted-foreground">unknown tool</strong>
          )}
        </p>

        <p className="mt-1.5 type-meta text-muted-foreground">
          Asked because: {item.reason === "" ? "(no reason recorded)" : item.reason}
        </p>

        <p className="mt-2 type-table">
          Task:{" "}
          <Link
            href={`/tasks/${item.task_id}`}
            className="font-medium hover:underline"
          >
            {excerpt(goal, item.task_id)}
          </Link>
        </p>

        <div className="mt-4">
          <span className="section-title">Input</span>
          {inputView.kind === "missing" ? (
            <p className="type-meta text-muted-foreground">
              <em>{inputView.note}</em>
            </p>
          ) : (
            <pre className="pre mt-0">{inputView.lines.join("\n")}</pre>
          )}
        </div>

        <div className="mt-4 flex flex-wrap items-center gap-2 border-t border-border pt-4">
          <Button
            type="button"
            size="sm"
            className="h-8 gap-1.5 bg-success text-success-foreground hover:bg-success/90"
            onClick={() => props.onDecide("allow")}
            disabled={busy}
          >
            <Check className="h-3.5 w-3.5" /> Allow
          </Button>
          {denying ? (
            <>
              <Button
                type="button"
                size="sm"
                variant="destructive"
                className="h-8 gap-1.5"
                onClick={() => props.onDecide("deny")}
                disabled={busy}
              >
                <X className="h-3.5 w-3.5" /> Confirm deny
              </Button>
              <Button
                type="button"
                size="sm"
                variant="outline"
                className="h-8"
                onClick={props.onCancelDeny}
                disabled={busy}
              >
                Cancel
              </Button>
              <span className="type-meta text-muted-foreground">
                The parked tool call will fail with “denied”.
              </span>
            </>
          ) : (
            <Button
              type="button"
              size="sm"
              variant="outline"
              className="h-8 border-destructive/30 text-destructive hover:bg-destructive/10 hover:text-destructive"
              onClick={props.onArmDeny}
              disabled={busy}
            >
              <X className="h-3.5 w-3.5" /> Deny
            </Button>
          )}
          <span className="ml-auto type-meta text-muted-foreground">
            expires {formatLocalTimestamp(item.expires_at)}
          </span>
        </div>
      </CardContent>
    </Card>
  );
}

// ---------------------------------------------------------------------------
// One pending scale approval card
// ---------------------------------------------------------------------------

function ScaleApprovalCard(props: {
  item: ScaleApproval;
  goal: string | null;
  busy: boolean;
  denying: boolean;
  fresh: boolean;
  onDecide: (decision: ApprovalDecision) => void;
  onArmDeny: () => void;
  onCancelDeny: () => void;
}) {
  const { item, goal, busy, denying, fresh } = props;
  const statusVariant = scaleApprovalStatusVariant(item.status);

  return (
    <Card className={`approval-card ${fresh ? "arriving" : ""}`}>
      <CardContent className="pt-5">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div className="flex flex-wrap items-center gap-2">
            <span className="mono">{shortenId(item.id)}</span>
            <Badge variant={statusVariant}>
              {scaleApprovalStatusLabel(item.status)}
            </Badge>
            <Badge variant="info">
              <Users className="h-3 w-3" /> scale approval
            </Badge>
          </div>
          <span className="time">
            waiting{" "}
            {item.created_at === null
              ? "?"
              : formatApprovalAge(item.created_at)}
          </span>
        </div>

        <p className="mt-4 type-body text-foreground">
          The Swarm lead wants{" "}
          <strong className="font-semibold">
            {item.requested_workers} worker
            {item.requested_workers === 1 ? "" : "s"}
          </strong>{" "}
          instead of the current cap of{" "}
          <strong className="font-semibold">
            {item.current_cap > 0 ? item.current_cap : 8}
          </strong>
          .
        </p>

        <p className="mt-1.5 type-meta text-muted-foreground">
          Asked because: {item.reason === "" ? "(no reason recorded)" : item.reason}
        </p>

        <p className="mt-2 type-table">
          Task:{" "}
          <Link
            href={`/tasks/${item.task_id}`}
            className="font-medium hover:underline"
          >
            {excerpt(goal, item.task_id)}
          </Link>
        </p>

        <div className="mt-3 flex flex-wrap items-center gap-1.5">
          <Badge variant="outline" className="font-normal">
            {scaleApprovalSummary(item)}
          </Badge>
          {item.team_id !== null ? (
            <span className="type-meta text-muted-foreground">
              team <span className="mono">{shortenId(item.team_id)}</span>
            </span>
          ) : null}
        </div>

        <div className="mt-4 flex flex-wrap items-center gap-2 border-t border-border pt-4">
          <Button
            type="button"
            size="sm"
            className="h-8 gap-1.5 bg-success text-success-foreground hover:bg-success/90"
            onClick={() => props.onDecide("allow")}
            disabled={busy}
          >
            <Check className="h-3.5 w-3.5" /> Allow
          </Button>
          {denying ? (
            <>
              <Button
                type="button"
                size="sm"
                variant="destructive"
                className="h-8 gap-1.5"
                onClick={() => props.onDecide("deny")}
                disabled={busy}
              >
                <X className="h-3.5 w-3.5" /> Confirm deny
              </Button>
              <Button
                type="button"
                size="sm"
                variant="outline"
                className="h-8"
                onClick={props.onCancelDeny}
                disabled={busy}
              >
                Cancel
              </Button>
              <span className="type-meta text-muted-foreground">
                The team cap stays at {item.current_cap > 0 ? item.current_cap : 8}.
              </span>
            </>
          ) : (
            <Button
              type="button"
              size="sm"
              variant="outline"
              className="h-8 border-destructive/30 text-destructive hover:bg-destructive/10 hover:text-destructive"
              onClick={props.onArmDeny}
              disabled={busy}
            >
              <X className="h-3.5 w-3.5" /> Deny
            </Button>
          )}
          <span className="ml-auto type-meta text-muted-foreground">
            {item.expires_at === null
              ? "no expiry"
              : `expires ${formatLocalTimestamp(item.expires_at)}`}
          </span>
        </div>
        <p className="mt-3 type-meta text-muted-foreground">
          Allow raises the teammate cap to {item.requested_workers} (hard max
          64). Deny keeps the cap at{" "}
          {item.current_cap > 0 ? item.current_cap : 8}.
        </p>
      </CardContent>
    </Card>
  );
}

/**
 * Goal excerpt with a deterministic fallback to the short task id. Goals are
 * Markdown/TeX source, so compact rows render the plain preview instead of
 * leaking markup into the one-line label.
 */
function excerpt(goal: string | null, taskId: string): string {
  const preview = goal === null ? "" : plainPreview(goal);
  if (preview === "") {
    return `task ${shortenId(taskId)}`;
  }
  return preview.length > 80 ? `${preview.slice(0, 80)}…` : preview;
}
