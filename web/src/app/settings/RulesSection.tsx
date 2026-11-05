"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import {
  ApiError,
  createRule,
  deleteRule,
  listRules,
  toggleRule,
  updateRule,
} from "@/lib/api";
import type { GuardAction, GuardRule, MatchTarget, MatchType } from "@/lib/types";
import {
  buildRulePayload,
  ruleActionTone,
  ruleMatchDescription,
  ruleReasonOrAuto,
  type RuleFormValues,
} from "@/lib/settings";
import { describeApiError } from "@/lib/ui";
import { formatLocalTimestamp } from "@/lib/localTime";

/**
 * Guard rules section: the rule table in EVALUATION order
 * (priority DESC, id ASC — first enabled match wins), the enabled toggle,
 * create/edit forms with client-side regex pre-flight, and confirm-armed
 * deletes.
 *
 * The persistent teal note records that every mutation hot-rebuilds the
 * compiled guard snapshot (crates/dhi-server `refresh_guard`): changes apply
 * to the NEXT tool-call evaluation — no restart — and it stamps the last
 * acknowledged mutation.
 */
export function RulesSection() {
  // --- Table state ------------------------------------------------------------
  const [rules, setRules] = useState<GuardRule[] | null>(null);
  const [initialError, setInitialError] = useState<string | null>(null);
  const [refreshError, setRefreshError] = useState<string | null>(null);
  const [refreshing, setRefreshing] = useState(false);

  // --- Hot-rebuild ledger -------------------------------------------------------
  const [lastAppliedAt, setLastAppliedAt] = useState<string | null>(null);

  // --- Row actions ---------------------------------------------------------------
  const [toggling, setToggling] = useState<ReadonlySet<number>>(new Set());
  const [deleteArmedId, setDeleteArmedId] = useState<number | null>(null);
  const [rowActionError, setRowActionError] = useState<string | null>(null);

  // --- Form -----------------------------------------------------------------------
  const [formState, setFormState] = useState<
    { kind: "closed" } | { kind: "create" } | { kind: "edit"; rule: GuardRule }
  >({ kind: "closed" });
  const [form, setForm] = useState<RuleFormValues>(() => emptyRuleForm());
  const [formError, setFormError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  const loadSeqRef = useRef(0);
  const mountedRef = useRef(true);
  const loadedOnceRef = useRef(false);

  const load = useCallback(async (): Promise<void> => {
    const seq = ++loadSeqRef.current;
    setRefreshing(true);
    try {
      const rows = await listRules();
      if (!mountedRef.current || seq !== loadSeqRef.current) {
        return;
      }
      loadedOnceRef.current = true;
      setRules(rows);
      setRefreshError(null);
      setInitialError(null);
    } catch (err) {
      if (!mountedRef.current || seq !== loadSeqRef.current) {
        return;
      }
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
    return () => {
      mountedRef.current = false;
    };
  }, [load]);

  /** Any acknowledged mutation bumps the hot-rebuild stamp. */
  const noteApplied = useCallback((): void => {
    setLastAppliedAt(new Date().toISOString());
  }, []);

  function openCreate(): void {
    setForm(emptyRuleForm());
    setFormState({ kind: "create" });
    setFormError(null);
    setRowActionError(null);
  }

  function openEdit(rule: GuardRule): void {
    setForm(fromRule(rule));
    setFormState({ kind: "edit", rule });
    setFormError(null);
    setRowActionError(null);
  }

  function closeForm(): void {
    setFormState({ kind: "closed" });
  }

  async function handleSubmit(event: React.FormEvent<HTMLFormElement>): Promise<void> {
    event.preventDefault();
    setFormError(null);
    const built = buildRulePayload(form);
    if (!built.ok) {
      setFormError(built.error);
      return;
    }
    setSaving(true);
    try {
      if (formState.kind === "edit") {
        await updateRule(formState.rule.id, built.input);
      } else {
        await createRule(built.input);
      }
      noteApplied();
      closeForm();
      void load();
    } catch (err) {
      // Server 400s (enum/bad regex per the Rust dialect) render verbatim;
      // a stale edit target 404s and asks for a refresh below.
      if (err instanceof ApiError && err.isNotFound()) {
        setFormError("Rule no longer exists — the table will refresh.");
        closeForm();
        void load();
      } else {
        setFormError(describeApiError(err));
      }
    } finally {
      setSaving(false);
    }
  }

  async function toggle(rule: GuardRule): Promise<void> {
    setRowActionError(null);
    setToggling((current) => new Set(current).add(rule.id));
    try {
      const response = await toggleRule(rule.id, !rule.enabled);
      // Optimistic flip from the authoritative response, then reconcile.
      setRules((current) =>
        current === null
          ? current
          : current.map((row) =>
              row.id === rule.id ? { ...row, enabled: response.enabled } : row,
            ),
      );
      noteApplied();
      void load();
    } catch (err) {
      if (err instanceof ApiError && err.isNotFound()) {
        setRowActionError("Rule no longer exists — list refreshed.");
        void load();
      } else {
        setRowActionError(describeApiError(err));
      }
    } finally {
      setToggling((current) => {
        const next = new Set(current);
        next.delete(rule.id);
        return next;
      });
    }
  }

  async function confirmDelete(rule: GuardRule): Promise<void> {
    setRowActionError(null);
    try {
      await deleteRule(rule.id);
      setDeleteArmedId(null);
      noteApplied();
      void load();
    } catch (err) {
      if (err instanceof ApiError && err.isNotFound()) {
        setDeleteArmedId(null);
        setRowActionError("Rule no longer exists — list refreshed.");
        void load();
      } else {
        setRowActionError(describeApiError(err));
      }
    }
  }

  return (
    <section className="card" id="rules" aria-labelledby="rules-heading">
      <div className="row spread">
        <h2 id="rules-heading" style={{ margin: 0 }}>
          Guard rules
        </h2>
        <button
          type="button"
          className="button"
          onClick={openCreate}
          disabled={formState.kind !== "closed"}
        >
          + New rule
        </button>
      </div>

      <div className="apply-note">
        Rules are evaluated <strong>first match wins</strong> in priority
        DESC, then id order — the table below is that order.{" "}
        <strong>
          Changes apply immediately: every mutation hot-rebuilds the guard;
          no restart.
        </strong>
        {lastAppliedAt !== null ? (
          <span className="subtle">
            {" "}
            Last change applied {formatLocalTimestamp(lastAppliedAt)}.
          </span>
        ) : null}
      </div>

      {rowActionError !== null ? (
        <div className="errorbox" role="alert">
          {rowActionError}
        </div>
      ) : null}

      {initialError === null && refreshError !== null ? (
        <div className="errorbox" role="alert">
          Refresh failed: {refreshError}{" "}
          <button type="button" className="button" onClick={() => void load()}>
            Retry
          </button>
        </div>
      ) : null}

      {rules === null && initialError === null ? (
        <p className="empty">Loading rules…</p>
      ) : null}

      {initialError !== null ? (
        <div className="errorbox" role="alert">
          Could not load rules: {initialError}{" "}
          <button type="button" className="button" onClick={() => void load()}>
            {refreshing ? "Retrying…" : "Retry"}
          </button>
        </div>
      ) : null}

      {rules !== null && rules.length === 0 && initialError === null ? (
        <p className="empty">
          No rules — every tool call falls through to the guard default
          action (see General settings).
        </p>
      ) : null}

      {rules !== null && rules.length > 0 ? (
        <div className="tablewrap">
          <table className="grid">
            <thead>
              <tr>
                <th title="Higher runs first; ties use rule ID.">Priority</th>
                <th>Name</th>
                <th>Match</th>
                <th>Pattern</th>
                <th>Action</th>
                <th>Ask timeout</th>
                <th>Enabled</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {rules.map((rule) => (
                <tr key={rule.id}>
                  <td>
                    <span className="mono">{rule.priority}</span>
                  </td>
                  <td>
                    <strong>{rule.name}</strong>
                    <br />
                    <span className="subtle">
                      {ruleReasonOrAuto(rule.name, rule.reason)}
                    </span>
                  </td>
                  <td>
                    <span className="subtle">
                      {ruleMatchDescription(rule.match_target, rule.match_type)}
                    </span>
                    <br />
                    <span className="chip">{rule.match_target}</span>{" "}
                    <span className="chip">{rule.match_type}</span>
                  </td>
                  <td>
                    <code className="mono rule-pattern">{rule.pattern}</code>
                  </td>
                  <td>
                    <span className="badge" data-state={ruleActionTone(rule.action)}>
                      {rule.action}
                    </span>
                  </td>
                  <td>
                    <span className="subtle">
                      {rule.timeout_secs === 0
                        ? "app default"
                        : `${rule.timeout_secs}s`}
                    </span>
                  </td>
                  <td>
                    <span className="badge" data-state={rule.enabled ? "ok" : "neutral"}>
                      {rule.enabled ? "enabled" : "disabled"}
                    </span>
                  </td>
                  <td>
                    <div className="row">
                      <button
                        type="button"
                        className="button small"
                        onClick={() => void toggle(rule)}
                        disabled={toggling.has(rule.id)}
                      >
                        {toggling.has(rule.id)
                          ? "…"
                          : rule.enabled
                            ? "Disable"
                            : "Enable"}
                      </button>
                      <button
                        type="button"
                        className="button small"
                        onClick={() => openEdit(rule)}
                        disabled={formState.kind === "edit" && formState.rule.id === rule.id}
                      >
                        Edit
                      </button>
                      {deleteArmedId === rule.id ? (
                        <>
                          <button
                            type="button"
                            className="button small deny"
                            onClick={() => void confirmDelete(rule)}
                          >
                            Confirm
                          </button>
                          <button
                            type="button"
                            className="button small"
                            onClick={() => setDeleteArmedId(null)}
                          >
                            Cancel
                          </button>
                        </>
                      ) : (
                        <button
                          type="button"
                          className="button small deny"
                          onClick={() => setDeleteArmedId(rule.id)}
                        >
                          Delete
                        </button>
                      )}
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ) : null}

      {formState.kind !== "closed" ? (
        <form onSubmit={(event) => void handleSubmit(event)}>
          <h3 style={{ marginBottom: 0 }}>
            {formState.kind === "edit"
              ? `Edit rule #${formState.rule.id}`
              : "New rule"}
          </h3>

          <div className="form-grid">
            <div className="field">
              <label className="label" htmlFor="rule-name">
                Name
              </label>
              <input
                id="rule-name"
                value={form.name}
                onChange={(event) => setForm({ ...form, name: event.target.value })}
                placeholder="e.g. ask-dangerous-tools"
              />
            </div>

            <div className="field">
              <label className="label" htmlFor="rule-target">
                Match target
              </label>
              <select
                id="rule-target"
                className="select-inline"
                value={form.matchTarget}
                onChange={(event) =>
                  setForm({
                    ...form,
                    matchTarget: event.target.value as MatchTarget,
                  })
                }
              >
                <option value="tool_name">tool_name</option>
                <option value="tool_input">tool_input</option>
              </select>
            </div>

            <div className="field">
              <label className="label" htmlFor="rule-match-type">
                Match type
              </label>
              <select
                id="rule-match-type"
                className="select-inline"
                value={form.matchType}
                onChange={(event) =>
                  setForm({ ...form, matchType: event.target.value as MatchType })
                }
              >
                <option value="exact">exact</option>
                <option value="regex">regex</option>
              </select>
            </div>

            <div className="field">
              <label className="label" htmlFor="rule-action">
                Action
              </label>
              <select
                id="rule-action"
                className="select-inline"
                value={form.action}
                onChange={(event) =>
                  setForm({ ...form, action: event.target.value as GuardAction })
                }
              >
                <option value="allow">allow — run without asking</option>
                <option value="deny">deny — block the call</option>
                <option value="ask">ask — wait for a human</option>
              </select>
            </div>

            <div className="field">
              <label className="label" htmlFor="rule-priority">
                Priority
              </label>
              <input
                id="rule-priority"
                type="number"
                step={1}
                value={form.priority}
                onChange={(event) =>
                  setForm({ ...form, priority: event.target.value })
                }
              />
              <p className="hint">Higher evaluates first.</p>
            </div>

            <div className="field">
              <label className="label" htmlFor="rule-timeout">
                Ask timeout (secs)
              </label>
              <input
                id="rule-timeout"
                type="number"
                step={1}
                value={form.timeoutSecs}
                onChange={(event) =>
                  setForm({ ...form, timeoutSecs: event.target.value })
                }
              />
              <p className="hint">0 = use the app default.</p>
            </div>
          </div>

          <div className="field">
            <label className="label" htmlFor="rule-pattern">
              Pattern
            </label>
            <input
              id="rule-pattern"
              value={form.pattern}
              onChange={(event) => setForm({ ...form, pattern: event.target.value })}
              placeholder={form.matchType === "regex" ? "^(rm|del).*" : "createGame"}
            />
          </div>

          <div className="field">
            <label className="label" htmlFor="rule-reason">
              Reason
            </label>
            <textarea
              id="rule-reason"
              value={form.reason}
              onChange={(event) => setForm({ ...form, reason: event.target.value })}
            />
            <p className="hint">
              Shown on the approval when this rule asks. Left empty, the
              server records &quot;matched rule &lt;name&gt;&quot;.
            </p>
          </div>

          <div className="checkline">
            <input
              id="rule-enabled"
              type="checkbox"
              checked={form.enabled}
              onChange={(event) =>
                setForm({ ...form, enabled: event.target.checked })
              }
            />
            <label htmlFor="rule-enabled">Enabled (participates in evaluation)</label>
          </div>

          {formError !== null ? (
            <div className="errorbox" role="alert">
              {formError}
            </div>
          ) : null}

          <div className="actions">
            <button type="submit" className="button primary" disabled={saving}>
              {saving
                ? "Saving…"
                : formState.kind === "edit"
                  ? "Replace rule"
                  : "Create rule"}
            </button>
            <button type="button" className="button" onClick={closeForm}>
              Cancel
            </button>
          </div>
        </form>
      ) : null}
    </section>
  );
}

// ---------------------------------------------------------------------------
// Form state factories
// ---------------------------------------------------------------------------

function emptyRuleForm(): RuleFormValues {
  return {
    name: "",
    matchTarget: "tool_name",
    matchType: "exact",
    pattern: "",
    action: "deny",
    reason: "",
    priority: "0",
    enabled: true,
    timeoutSecs: "0",
  };
}

function fromRule(rule: GuardRule): RuleFormValues {
  return {
    name: rule.name,
    matchTarget: rule.match_target,
    matchType: rule.match_type,
    pattern: rule.pattern,
    action: rule.action,
    reason: rule.reason,
    priority: String(rule.priority),
    enabled: rule.enabled,
    timeoutSecs: String(rule.timeout_secs),
  };
}
