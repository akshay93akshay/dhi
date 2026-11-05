"use client";

import { useEffect, useState } from "react";
import { updateSettings } from "@/lib/api";
import type { AppSettingsView, GuardAction } from "@/lib/types";
import {
  UNSEEDED_SETTINGS_HINT,
  buildSettingsPatch,
  explainSettingsWriteFailure,
  isUnseededSettings,
  type SettingsFormValues,
} from "@/lib/settings";

/**
 * Advanced behavior card (Settings v2): the approval ask timeout and the
 * guard default action — saved as ONE partial `PUT /api/settings` carrying
 * ONLY the changed fields ({@link buildSettingsPatch}; "null ≡ omitted" per
 * the merge contract). Role defaults live in the dedicated role-routing
 * section, so this form never duplicates those selectors.
 *
 * Unseeded handling: `GET /api/settings` always 200s, but on a literally
 * zero-profile database the PUT answers `404 "app settings are not
 * initialized"` (a registered edge). Rather than let the operator hit a
 * scary error, the form DISABLES itself with the friendly
 * "create a profile first" hint as soon as the view reports null roles.
 */
export function BehaviorSettingsSection(props: {
  settings: AppSettingsView | null;
  /** Receives the PUT response (the merged view) to refresh the page. */
  onSaved: (view: AppSettingsView) => void;
}) {
  const { settings, onSaved } = props;

  const [draft, setDraft] = useState<SettingsFormValues>(() =>
    fromView(settings),
  );
  const [dirty, setDirty] = useState(false);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [savedNote, setSavedNote] = useState<string | null>(null);
  const [uninitialized, setUninitialized] = useState(false);

  // Baseline resync: on view change — but never over a half-edited form.
  // The save path clears `dirty` before the merged view lands, so the form
  // snaps to the committed state, not to the operator's keystrokes.
  useEffect(() => {
    if (!dirty) {
      setDraft(fromView(settings));
    }
  }, [settings, dirty]);

  const unseeded = settings !== null && isUnseededSettings(settings);
  const loading = settings === null;

  function update(partial: Partial<SettingsFormValues>): void {
    setDirty(true);
    setSavedNote(null);
    setSaveError(null);
    setDraft((current) => ({ ...current, ...partial }));
  }

  async function handleSave(): Promise<void> {
    setSaveError(null);
    setSavedNote(null);
    const built = buildSettingsPatch(settings, draft);
    if (!built.ok) {
      setSaveError(built.error);
      return;
    }
    if (built.changed.length === 0) {
      setSavedNote("No changes to save.");
      return;
    }
    setSaving(true);
    try {
      // Partial PUT of changed fields only; the response IS the merged view.
      const merged = await updateSettings(built.patch);
      setDirty(false); // Let the incoming view resync the draft.
      setSavedNote(
        `Saved — ${built.changed.length} field${
          built.changed.length === 1 ? "" : "s"
        } updated (${built.changed.join(", ")}).`,
      );
      onSaved(merged);
    } catch (err) {
      const mapped = explainSettingsWriteFailure(err);
      if (mapped.kind === "uninitialized") {
        // The registered zero-profile PUT 404: a calm hint, not an error.
        setUninitialized(true);
        setSaveError(null);
      } else {
        setSaveError(mapped.text);
      }
    } finally {
      setSaving(false);
    }
  }

  const formDisabled = loading || unseeded || saving;

  return (
    <section className="card" id="behavior" aria-labelledby="behavior-heading">
      <h2 id="behavior-heading" style={{ marginTop: 0 }}>
        Behavior
      </h2>
      <p className="subtle">
        How long approvals wait and what the guard does when no rule matches.
      </p>

      {loading ? <p className="empty">Loading settings…</p> : null}

      {unseeded ? (
        <div className="notice" role="status">
          {UNSEEDED_SETTINGS_HINT}
        </div>
      ) : null}

      {uninitialized && !unseeded ? (
        <div className="notice" role="status">
          {UNSEEDED_SETTINGS_HINT}
        </div>
      ) : null}

      <fieldset className="settings-fieldset" disabled={formDisabled}>
        <legend className="sr-only">Behavior settings</legend>

        <div className="form-grid">
          <div className="field">
            <label className="label" htmlFor="settings-timeout">
              Approval timeout (seconds)
            </label>
            <input
              id="settings-timeout"
              type="number"
              min={1}
              step={1}
              value={draft.approvalTimeout}
              onChange={(event) =>
                update({ approvalTimeout: event.target.value })
              }
            />
            <p className="hint">
              How long an Ask (human approval) waits before it expires and
              the tool call fails as denied.
            </p>
          </div>

          <div className="field">
            <label className="label" htmlFor="settings-action">
              Guard default action
            </label>
            <select
              id="settings-action"
              className="select-inline"
              value={draft.guardDefaultAction}
              onChange={(event) =>
                update({
                  guardDefaultAction: event.target.value as GuardAction,
                })
              }
            >
              <option value="allow">allow — run unmatched calls</option>
              <option value="deny">deny — block unmatched calls</option>
              <option value="ask">ask — queue a human approval</option>
            </select>
            <p className="hint">
              What happens when NO rule matches a tool call.
            </p>
          </div>
        </div>

        {savedNote !== null ? (
          <div className="successbox" role="status">
            {savedNote}
          </div>
        ) : null}

        {saveError !== null ? (
          <div className="errorbox" role="alert">
            {saveError}
          </div>
        ) : null}

        <div className="actions">
          <button
            type="button"
            className="button primary"
            onClick={() => void handleSave()}
            disabled={formDisabled}
          >
            {saving ? "Saving…" : "Save settings"}
          </button>
        </div>
      </fieldset>
    </section>
  );
}

// ---------------------------------------------------------------------------
// Draft <-> view mapping
// ---------------------------------------------------------------------------

function fromView(settings: AppSettingsView | null): SettingsFormValues {
  return {
    approvalTimeout: String(settings?.approval_timeout_secs ?? 900),
    guardDefaultAction: settings?.guard_default_action ?? "deny",
  };
}
