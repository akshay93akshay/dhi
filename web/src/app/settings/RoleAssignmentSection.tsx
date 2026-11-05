"use client";

import { useEffect, useState } from "react";
import { updateSettings } from "@/lib/api";
import type { AppSettingsView, LlmProfile } from "@/lib/types";
import {
  UNSEEDED_SETTINGS_HINT,
  buildRoleAssignmentPatch,
  describeApiKeyHint,
  explainSettingsWriteFailure,
  parseIntOrNull,
  type RoleAssignmentFormValues,
} from "@/lib/settings";

/**
 * Compact role routing (Settings v2): Planner and Worker share one card and
 * ONE apply button. Both selects may point at the same profile or at
 * different profiles; the pure {@link buildRoleAssignmentPatch} compares each
 * choice with the current view and emits only the changed fields, so changing
 * one role sends only that field and changing both sends a single merged
 * `PUT /api/settings`.
 *
 * `Edit key` hands the selected profile id to the parent, which asks
 * {@link ProfilesSection} to open that profile's editor below — the key form
 * itself is never lifted into this component.
 */
export function RoleAssignmentSection(props: {
  profiles: LlmProfile[] | null;
  settings: AppSettingsView | null;
  /** Receives the PUT response (the merged view) to refresh the page. */
  onSaved: (view: AppSettingsView) => void;
  /** Opens the matching profile editor in ProfilesSection. */
  onEditKey: (profileId: number) => void;
}) {
  const { profiles, settings, onSaved, onEditKey } = props;

  const [draft, setDraft] = useState<RoleAssignmentFormValues>(() =>
    fromView(settings),
  );
  const [dirty, setDirty] = useState(false);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [savedNote, setSavedNote] = useState<string | null>(null);
  const [uninitialized, setUninitialized] = useState(false);

  // Baseline resync: never overwrite a half-edited selection, but snap to
  // the merged view after a save (the save path clears `dirty` first).
  useEffect(() => {
    if (!dirty) {
      setDraft(fromView(settings));
    }
  }, [settings, dirty]);

  const loading = settings === null || profiles === null;
  const empty = profiles !== null && profiles.length === 0;

  const plannerId = settings?.planner_profile?.id ?? null;
  const workerId = settings?.worker_profile?.id ?? null;

  function update(partial: Partial<RoleAssignmentFormValues>): void {
    setDirty(true);
    setSavedNote(null);
    setSaveError(null);
    setUninitialized(false);
    setDraft((current) => ({ ...current, ...partial }));
  }

  async function handleApply(): Promise<void> {
    setSaveError(null);
    setSavedNote(null);
    setUninitialized(false);
    const built = buildRoleAssignmentPatch(settings, draft);
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
      // One partial PUT: both roles when both changed, one field otherwise.
      const merged = await updateSettings(built.patch);
      setDirty(false); // Let the merged view resync the selects.
      setSavedNote("Role routing saved.");
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

  const plannerSelection = selectionFor(profiles, draft.plannerChoice);
  const workerSelection = selectionFor(profiles, draft.workerChoice);

  return (
    <section className="card" id="roles" aria-labelledby="roles-heading">
      <div className="row spread">
        <h2 id="roles-heading" style={{ margin: 0 }}>
          Role routing
        </h2>
      </div>
      {loading && !empty ? (
        <p className="empty">Loading role routing…</p>
      ) : null}

      {empty ? (
        <div className="notice" role="status">
          <span>
            <strong>Create a profile first</strong> — role routing has nothing
            to assign yet.
          </span>
        </div>
      ) : null}

      {!loading && !empty ? (
        <fieldset className="settings-fieldset" disabled={saving}>
          <legend className="sr-only">Role routing</legend>

          <div className="mt-4 divide-y divide-border overflow-hidden rounded-xl border border-border">
            <RoleRow
              role="Planner"
              htmlId="roles-planner"
              choice={draft.plannerChoice}
              profiles={profiles}
              currentId={plannerId}
              currentHint={settings?.planner_api_key_hint ?? ""}
              selection={plannerSelection}
              onChoice={(plannerChoice) => update({ plannerChoice })}
              onEditKey={onEditKey}
            />
            <RoleRow
              role="Worker"
              htmlId="roles-worker"
              choice={draft.workerChoice}
              profiles={profiles}
              currentId={workerId}
              currentHint={settings?.worker_api_key_hint ?? ""}
              selection={workerSelection}
              onChoice={(workerChoice) => update({ workerChoice })}
              onEditKey={onEditKey}
            />
          </div>

          {uninitialized ? (
            <div className="notice" role="status">
              {UNSEEDED_SETTINGS_HINT}
            </div>
          ) : null}

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
              onClick={() => void handleApply()}
              disabled={saving}
            >
              {saving ? "Applying…" : "Apply"}
            </button>
          </div>
        </fieldset>
      ) : null}
    </section>
  );
}

// ---------------------------------------------------------------------------
// One compact role row
// ---------------------------------------------------------------------------

function RoleRow(props: {
  role: "Planner" | "Worker";
  htmlId: string;
  choice: string;
  profiles: LlmProfile[] | null;
  currentId: number | null;
  currentHint: string;
  selection: LlmProfile | null;
  onChoice: (choice: string) => void;
  onEditKey: (profileId: number) => void;
}) {
  const {
    role,
    htmlId,
    choice,
    profiles,
    currentId,
    currentHint,
    selection,
    onChoice,
    onEditKey,
  } = props;
  const selectedId = parseIntOrNull(choice);
  const keyHint =
    selection !== null
      ? describeApiKeyHint(selection.api_key_hint)
      : selectedId !== null && selectedId === currentId && currentId !== null
        ? describeApiKeyHint(currentHint)
        : "No key";

  return (
    <div className="grid gap-2 px-3 py-3 sm:grid-cols-[88px_minmax(0,1fr)_auto] sm:items-center sm:gap-3">
      <label className="label mb-0" htmlFor={htmlId}>
        {role}
      </label>
      <div className="min-w-0">
        <select
          id={htmlId}
          className="select-inline"
          value={choice}
          onChange={(event) => onChoice(event.target.value)}
        >
          {currentId === null ? (
            <option value="">— none (uninitialized) —</option>
          ) : null}
          {optionsFor(profiles, currentId).map((profile) => (
            <option key={profile.id} value={String(profile.id)}>
              {profileOptionLabel(profile)}
            </option>
          ))}
        </select>
        <p className="hint">
          Key <span className="mono">{keyHint}</span>
        </p>
      </div>
      <button
        type="button"
        className="button small"
        onClick={() => {
          if (selection !== null) {
            onEditKey(selection.id);
          }
        }}
        disabled={selection === null}
      >
        Edit key
      </button>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Draft <-> view mapping
// ---------------------------------------------------------------------------

function fromView(settings: AppSettingsView | null): RoleAssignmentFormValues {
  return {
    plannerChoice:
      settings?.planner_profile != null
        ? String(settings.planner_profile.id)
        : "",
    workerChoice:
      settings?.worker_profile != null
        ? String(settings.worker_profile.id)
        : "",
  };
}

// ---------------------------------------------------------------------------
// Selection rendering
// ---------------------------------------------------------------------------

/** Options: every profile, plus the current binding even if unlisted. */
function optionsFor(
  profiles: LlmProfile[] | null,
  currentId: number | null,
): LlmProfile[] {
  const rows = profiles ?? [];
  if (currentId !== null && !rows.some((profile) => profile.id === currentId)) {
    return [
      ...rows,
      {
        id: currentId,
        name: `profile #${currentId}`,
        format: "openai",
        base_url: "",
        model: "",
        api_key_hint: "",
        max_tokens: 0,
        context_window_k: 0,
        thinking_type: "",
        reasoning_effort: "",
        streaming: true,
        priority: 0,
        pool_exclude: false,
        created_at: "",
        updated_at: "",
      },
    ];
  }
  return rows;
}

/** Option copy carries the three facts the operator needs to choose. */
function profileOptionLabel(profile: LlmProfile): string {
  const model = profile.model.trim() === "" ? "(no model)" : profile.model;
  return `${profile.name} — ${model} — ${profile.format}`;
}

/** The profile DTO behind a select choice, or `null` when it is not loaded. */
function selectionFor(
  profiles: LlmProfile[] | null,
  choice: string,
): LlmProfile | null {
  const id = parseIntOrNull(choice);
  if (id === null) {
    return null;
  }
  return (profiles ?? []).find((profile) => profile.id === id) ?? null;
}
