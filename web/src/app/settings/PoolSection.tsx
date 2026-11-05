"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { getPoolStatus, resetPool } from "@/lib/api";
import type { ChainEntry, PoolStatus } from "@/lib/types";
import {
  POOL_POLL_INTERVAL_MS,
  chainEntryChips,
  countTrippedEntries,
} from "@/lib/settings";
import { describeApiError } from "@/lib/ui";

/**
 * Failover pool section: the two role chains in resolution order with
 * per-entry breaker chips, per-tripped-entry and reset-all affordances, and
 * a ~10 s visibility-gated auto-refresh.
 *
 * The state shown here is TRANSIENT and in-memory per process: trip marks
 * are advisory counters over the durable profile config, and a restart
 * forgets them — the card says so. Unlike the other
 * management routes, `pool_status_route` and `pool_reset_route` never touch
 * `require_mgmt`, so they always answer 200 from the snapshot; failures
 * here can only be network-level, handled with a plain error state.
 */
export function PoolSection() {
  const [status, setStatus] = useState<PoolStatus | null>(null);
  const [initialError, setInitialError] = useState<string | null>(null);
  const [refreshError, setRefreshError] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [resetting, setResetting] = useState<ReadonlySet<number>>(new Set());
  const [resetAllBusy, setResetAllBusy] = useState(false);

  const loadSeqRef = useRef(0);
  const mountedRef = useRef(true);
  const loadedOnceRef = useRef(false);

  const load = useCallback(async (): Promise<void> => {
    const seq = ++loadSeqRef.current;
    try {
      const next = await getPoolStatus();
      if (!mountedRef.current || seq !== loadSeqRef.current) {
        return;
      }
      loadedOnceRef.current = true;
      setStatus(next);
      setRefreshError(null);
      setInitialError(null);
    } catch (err) {
      if (!mountedRef.current || seq !== loadSeqRef.current) {
        return;
      }
      const message = describeApiError(err);
      // Keep the last-known chains on a failed refresh — transient data
      // going stale beats flashing an empty card.
      setRefreshError(message);
      if (!loadedOnceRef.current) {
        setInitialError(message);
      }
    }
  }, []);

  useEffect(() => {
    mountedRef.current = true;
    void load();

    const timer = setInterval(() => {
      if (
        typeof document !== "undefined" &&
        document.visibilityState !== "visible"
      ) {
        return; // Hidden tabs pause polling (5B/5C/5D stance).
      }
      void load();
    }, POOL_POLL_INTERVAL_MS);

    const onVisibility = (): void => {
      if (document.visibilityState === "visible") {
        void load();
      }
    };
    const onFocus = (): void => {
      void load();
    };
    document.addEventListener("visibilitychange", onVisibility);
    window.addEventListener("focus", onFocus);

    return () => {
      mountedRef.current = false;
      clearInterval(timer);
      document.removeEventListener("visibilitychange", onVisibility);
      window.removeEventListener("focus", onFocus);
    };
  }, [load]);

  /** Applies a reset response (which IS the fresh status) to the view. */
  async function reset(id: number | undefined, label: string): Promise<void> {
    const key = id ?? 0;
    setActionError(null);
    if (id === undefined) {
      setResetAllBusy(true);
    } else {
      setResetting((current) => new Set(current).add(id));
    }
    try {
      const fresh = await resetPool(id);
      if (mountedRef.current) {
        setStatus(fresh);
      }
    } catch (err) {
      setActionError(`${label} failed: ${describeApiError(err)}`);
    } finally {
      if (id === undefined) {
        setResetAllBusy(false);
      } else {
        setResetting((current) => {
          const next = new Set(current);
          next.delete(id);
          return next;
        });
      }
    }
  }

  const trippedTotal = countTrippedEntries(status);

  return (
    <section className="card" id="pool" aria-labelledby="pool-heading">
      <div className="row spread">
        <h2 id="pool-heading" style={{ margin: 0 }}>
          Failover pool
        </h2>
        <div className="row">
          <span className="subtle">auto-refreshes ~10 s (visible tabs)</span>
          <button
            type="button"
            className="button small"
            onClick={() => void reset(undefined, "Reset all")}
            disabled={resetAllBusy}
            title='POST /api/llm/pool/reset with {"id": 0} — clears every breaker'
          >
            {resetAllBusy ? "Resetting…" : "Reset all"}
            {trippedTotal > 0 ? ` (${trippedTotal} tripped)` : ""}
          </button>
        </div>
      </div>

      {actionError !== null ? (
        <div className="errorbox" role="alert">
          {actionError}
        </div>
      ) : null}

      {/* Failed refresh AFTER a successful load: banner + keep stale chains. */}
      {initialError === null && refreshError !== null ? (
        <div className="errorbox" role="alert">
          Refresh failed: {refreshError}{" "}
          <button
            type="button"
            className="button"
            onClick={() => void load()}
          >
            Retry
          </button>
        </div>
      ) : null}

      {/* First-load failure: plain error state, never a crash. Also covers
          the (per-handler shape unreachable) mgmt-unwired 500 envelope. */}
      {initialError !== null ? (
        <div className="errorbox" role="alert">
          Could not load pool status: {initialError}{" "}
          <button
            type="button"
            className="button"
            onClick={() => void load()}
          >
            Retry
          </button>
        </div>
      ) : null}

      {status === null && initialError === null ? (
        <p className="empty">Loading pool status…</p>
      ) : null}

      {status !== null ? (
        <div className="pool-cols">
          <ChainList
            role="Planner"
            chain={status.planner?.chain ?? []}
            resetting={resetting}
            onReset={(id) => void reset(id, `Reset #${id}`)}
            resetAllBusy={resetAllBusy}
          />
          <ChainList
            role="Worker"
            chain={status.worker?.chain ?? []}
            resetting={resetting}
            onReset={(id) => void reset(id, `Reset #${id}`)}
            resetAllBusy={resetAllBusy}
          />
        </div>
      ) : null}
    </section>
  );
}

// ---------------------------------------------------------------------------
// One role chain
// ---------------------------------------------------------------------------

function ChainList(props: {
  role: string;
  chain: ChainEntry[];
  resetting: ReadonlySet<number>;
  resetAllBusy: boolean;
  onReset: (id: number) => void;
}) {
  const { chain, role } = props;
  return (
    <div className="pool-col">
      <span className="section-title">{role} chain (resolution order)</span>
      {chain.length === 0 ? (
        <p className="empty">empty — no eligible profiles</p>
      ) : (
        <ol className="chain">
          {chain.map((entry, index) => (
            <li key={`${entry.id}-${index}`}>
              <div className="row">
                <strong>{entry.name}</strong>
                <span className="mono">{entry.model}</span>
                <span className="chip">#{entry.id}</span>
                {chainEntryChips(entry).map((chip) => (
                  <span
                    key={chip.text}
                    className="chip"
                    data-tone={chip.tone}
                  >
                    {chip.text}
                  </span>
                ))}
                {entry.tripped ? (
                  <button
                    type="button"
                    className="button small"
                    onClick={() => props.onReset(entry.id)}
                    disabled={props.resetting.has(entry.id) || props.resetAllBusy}
                  >
                    {props.resetting.has(entry.id) ? "Resetting…" : "Reset"}
                  </button>
                ) : null}
              </div>
            </li>
          ))}
        </ol>
      )}
    </div>
  );
}
