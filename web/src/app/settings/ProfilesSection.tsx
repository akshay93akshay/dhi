"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import { ApiError, deleteProfile, listModels, saveProfile } from "@/lib/api";
import type { AppSettingsView, LlmProfile, ProfileFormat } from "@/lib/types";
import {
  MODELS_PICKER_SAVE_FIRST_HINT,
  MODEL_FETCH_HINT,
  REASONING_EFFORT_VALUES,
  THINKING_TYPE_VALUES,
  buildProfilePayload,
  canFetchModels,
  describeApiKeyHint,
  editingIdOf,
  explainProfileDeleteFailure,
  isReasoningEffort,
  isThinkingType,
  reasoningEffortLabel,
  thinkingTypeLabel,
  toModelOptions,
  type ProfileFormValues,
} from "@/lib/settings";
import { describeApiError } from "@/lib/ui";

/**
 * Profiles section: cards with format/model/hint/capability
 * chips plus role-default markers, a create/edit form (key hygiene: the
 * password field blanks on edit = "keep the stored key" and is cleared
 * after a save), a saved-profiles-only model picker, and confirm-armed
 * deletion with 409-aware messaging. Role switching lives in the dedicated
 * RoleAssignmentSection at the top of Settings.
 *
 * Key hygiene guarantees: no read ever carries an api key (hint only), the
 * password input starts empty on edit, a blank submit OMITS the key from
 * the request body (keeping the stored one), and the field is discarded
 * with the form on save.
 */
export function ProfilesSection(props: {
  profiles: LlmProfile[] | null;
  settings: AppSettingsView | null;
  onChanged: () => void;
  /** Controlled "open this profile's key editor" request from role routing. */
  editRequest: { profileId: number; nonce: number } | null;
}) {
  const { profiles, settings, onChanged, editRequest } = props;

  // --- Form -----------------------------------------------------------------
  const [formState, setFormState] = useState<
    | { kind: "closed" }
    | { kind: "create" }
    | { kind: "edit"; profile: LlmProfile }
  >({ kind: "closed" });
  const [form, setForm] = useState<ProfileFormValues>(() => emptyForm());
  const [formError, setFormError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  // --- Model picker (edit-time only; needs the stored key) -------------------
  const [modelOptions, setModelOptions] = useState<string[]>([]);
  const [modelsBusy, setModelsBusy] = useState(false);
  const [modelsNote, setModelsNote] = useState<string | null>(null);

  // --- Card actions ----------------------------------------------------------
  const [deleteArmedId, setDeleteArmedId] = useState<number | null>(null);
  const [deleteBusy, setDeleteBusy] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const [savedNote, setSavedNote] = useState<string | null>(null);

  const formRef = useRef<HTMLFormElement | null>(null);
  const handledEditRequestRef = useRef(0);
  const pendingEditScrollRef = useRef(false);

  const editingId =
    formState.kind === "edit" ? editingIdOf(formState.profile) : null;

  // Controlled request from Role routing's "Edit key": open the matching
  // profile editor (never lift the form). A request that arrives before the
  // profile list is available stays pending and retries on the next load.
  useEffect(() => {
    if (editRequest === null) {
      return;
    }
    if (handledEditRequestRef.current === editRequest.nonce) {
      return;
    }
    const profile = (profiles ?? []).find(
      (row) => row.id === editRequest.profileId,
    );
    if (profile === undefined) {
      return;
    }
    handledEditRequestRef.current = editRequest.nonce;
    pendingEditScrollRef.current = true;
    openEdit(profile);
  }, [editRequest, profiles]);

  // The form mounts one render after openEdit; scroll once it exists.
  useEffect(() => {
    if (pendingEditScrollRef.current && formState.kind === "edit") {
      pendingEditScrollRef.current = false;
      formRef.current?.scrollIntoView({ behavior: "smooth", block: "start" });
    }
  }, [formState]);

  /** Role-default ids from the settings view, for the card markers. */
  const plannerDefaultId = settings?.planner_profile?.id ?? null;
  const workerDefaultId = settings?.worker_profile?.id ?? null;

  const datalistId = useMemo(() => "profile-model-options", []);

  function openCreate(): void {
    setForm(emptyForm());
    setFormState({ kind: "create" });
    setFormError(null);
    setModelOptions([]);
    setModelsNote(null);
    setActionError(null);
    setSavedNote(null);
  }

  function openEdit(profile: LlmProfile): void {
    setForm(fromProfile(profile));
    setFormState({ kind: "edit", profile });
    setFormError(null);
    setModelOptions([]);
    setModelsNote(null);
    setActionError(null);
    setSavedNote(null);
  }

  function closeForm(): void {
    setFormState({ kind: "closed" });
    setModelOptions([]);
    setModelsNote(null);
  }

  async function handleSubmit(event: React.FormEvent<HTMLFormElement>): Promise<void> {
    event.preventDefault();
    setFormError(null);
    const built = buildProfilePayload(form, editingId);
    if (!built.ok) {
      setFormError(built.error);
      return;
    }
    setSaving(true);
    try {
      const saved = await saveProfile(built.input);
      const name = built.input.name;
      // Key hygiene: the form (holding the typed password) closes outright,
      // so the plaintext key never lingers in component state.
      closeForm();
      setSavedNote(
        formState.kind === "edit"
          ? `Saved profile #${saved.id} ${name}.`
          : `Created profile #${saved.id} ${name}.`,
      );
      onChanged();
    } catch (err) {
      setFormError(describeApiError(err));
    } finally {
      setSaving(false);
    }
  }

  async function fetchModels(): Promise<void> {
    if (editingId === null) {
      return;
    }
    setModelsBusy(true);
    setModelsNote(null);
    try {
      const response = await listModels(editingId);
      const options = toModelOptions(response.models);
      setModelOptions(options);
      setModelsNote(
        options.length === 0
          ? "Upstream returned no model ids — type the model manually."
          : `Loaded ${options.length} model${options.length === 1 ? "" : "s"} from upstream — pick one below.`,
      );
    } catch (err) {
      // The 502's redacted upstream snippet (or any 400/404) renders as-is.
      setModelsNote(
        err instanceof ApiError
          ? `Fetch failed: ${err.message}`
          : `Fetch failed: ${describeApiError(err)}`,
      );
    } finally {
      setModelsBusy(false);
    }
  }

  async function confirmDelete(id: number, name: string): Promise<void> {
    setDeleteBusy(true);
    setActionError(null);
    try {
      await deleteProfile(id);
      setDeleteArmedId(null);
      setSavedNote(`Deleted profile ${name}.`);
      onChanged();
    } catch (err) {
      if (err instanceof ApiError && err.isNotFound()) {
        // Already gone: drop it locally and refresh the list.
        setDeleteArmedId(null);
        setSavedNote(explainProfileDeleteFailure(err));
        onChanged();
      } else {
        // 409 (role default / task pin) keeps the card and shows the
        // server's unbind guidance verbatim.
        setActionError(explainProfileDeleteFailure(err));
      }
    } finally {
      setDeleteBusy(false);
    }
  }

  return (
    <section className="card" id="profiles" aria-labelledby="profiles-heading">
      <div className="row spread">
        <h2 id="profiles-heading" style={{ margin: 0 }}>
          LLM profiles
        </h2>
        <button
          type="button"
          className="button"
          onClick={openCreate}
          disabled={formState.kind !== "closed"}
        >
          + New profile
        </button>
      </div>
      {savedNote !== null ? (
        <div className="successbox" role="status">
          {savedNote}
        </div>
      ) : null}

      {actionError !== null ? (
        <div className="errorbox" role="alert">
          {actionError}
        </div>
      ) : null}

      {profiles === null ? <p className="empty">Loading profiles…</p> : null}

      {profiles !== null && profiles.length === 0 ? (
        <p className="empty">
          No profiles yet — create one to give the planner and worker a
          model. Saving the first profile also initializes general settings.
        </p>
      ) : null}

      {profiles !== null &&
        profiles.map((profile) => (
          <ProfileCard
            key={profile.id}
            profile={profile}
            plannerDefault={plannerDefaultId === profile.id}
            workerDefault={workerDefaultId === profile.id}
            deleteArmed={deleteArmedId === profile.id}
            deleteBusy={deleteBusy}
            formHidden={formState.kind === "edit" && formState.profile.id === profile.id}
            onEdit={() => openEdit(profile)}
            onDelete={() => void confirmDelete(profile.id, profile.name)}
            onArmDelete={() => {
              setActionError(null);
              setDeleteArmedId(profile.id);
            }}
            onCancelDelete={() => setDeleteArmedId(null)}
          />
        ))}

      {formState.kind !== "closed" ? (
        <form
          ref={formRef}
          onSubmit={(event) => void handleSubmit(event)}
          className="profile-form scroll-mt-24"
        >
          <h3 style={{ marginBottom: 0 }}>
            {formState.kind === "edit"
              ? `Edit profile #${formState.profile.id}`
              : "New profile"}
          </h3>

          <div className="form-grid">
            <div className="field">
              <label className="label" htmlFor="profile-name">
                Name
              </label>
              <input
                id="profile-name"
                value={form.name}
                onChange={(event) =>
                  setForm({ ...form, name: event.target.value })
                }
                placeholder="e.g. backup-provider"
              />
            </div>

            <div className="field">
              <label className="label" htmlFor="profile-format">
                Format
              </label>
              <select
                id="profile-format"
                className="select-inline"
                value={form.format}
                onChange={(event) =>
                  setForm({
                    ...form,
                    format: event.target.value as ProfileFormat,
                  })
                }
              >
                <option value="openai">openai</option>
                <option value="anthropic">anthropic</option>
              </select>
            </div>

            <div className="field">
              <label className="label" htmlFor="profile-base-url">
                Base URL
              </label>
              <input
                id="profile-base-url"
                value={form.baseUrl}
                onChange={(event) =>
                  setForm({ ...form, baseUrl: event.target.value })
                }
                placeholder="https://api.example.com/v1"
              />
            </div>

            <div className="field">
              <label className="label" htmlFor="profile-model">
                Model
              </label>
              <input
                id="profile-model"
                list={datalistId}
                value={form.model}
                onChange={(event) =>
                  setForm({ ...form, model: event.target.value })
                }
                placeholder="e.g. river-planner"
              />
              <datalist id={datalistId}>
                {modelOptions.map((option) => (
                  <option key={option} value={option} />
                ))}
              </datalist>
            </div>

            <div className="field">
              <label className="label" htmlFor="profile-api-key">
                API key
              </label>
              <input
                id="profile-api-key"
                type="password"
                autoComplete="off"
                value={form.apiKey}
                onChange={(event) =>
                  setForm({ ...form, apiKey: event.target.value })
                }
                placeholder={
                  formState.kind === "edit"
                    ? describeApiKeyHint(formState.profile.api_key_hint)
                    : "paste the provider key"
                }
              />
              <p className="hint">
                {formState.kind === "edit"
                  ? "Leave blank to keep the stored key."
                  : "Stored write-only; never displayed again."}
              </p>
            </div>

            <div className="field">
              <label className="label" htmlFor="profile-max-tokens">
                Max tokens
              </label>
              <input
                id="profile-max-tokens"
                type="number"
                step={1}
                value={form.maxTokens}
                onChange={(event) =>
                  setForm({ ...form, maxTokens: event.target.value })
                }
              />
              <p className="hint">0 = no cap.</p>
            </div>

            <div className="field">
              <label className="label" htmlFor="profile-context-k">
                Context window (k tokens)
              </label>
              <input
                id="profile-context-k"
                type="number"
                step={1}
                value={form.contextWindowK}
                onChange={(event) =>
                  setForm({ ...form, contextWindowK: event.target.value })
                }
              />
            </div>

            <div className="field">
              <label className="label" htmlFor="profile-thinking-type">
                Thinking
              </label>
              <select
                id="profile-thinking-type"
                className="select-inline"
                value={form.thinkingType}
                onChange={(event) =>
                  setForm({
                    ...form,
                    thinkingType:
                      event.target.value as ProfileFormValues["thinkingType"],
                  })
                }
              >
                {THINKING_TYPE_VALUES.map((value) => (
                  <option key={value} value={value}>
                    {thinkingTypeLabel(value)}
                  </option>
                ))}
              </select>
              <p className="hint">
                Independent of reasoning effort; Not sent omits the field.
              </p>
            </div>

            <div className="field">
              <label className="label" htmlFor="profile-reasoning-effort">
                Reasoning effort
              </label>
              <select
                id="profile-reasoning-effort"
                className="select-inline"
                value={form.reasoningEffort}
                onChange={(event) =>
                  setForm({
                    ...form,
                    reasoningEffort:
                      event.target.value as ProfileFormValues["reasoningEffort"],
                  })
                }
              >
                {REASONING_EFFORT_VALUES.map((value) => (
                  <option key={value} value={value}>
                    {reasoningEffortLabel(value)}
                  </option>
                ))}
              </select>
              <p className="hint">
                Independent of thinking; Not sent omits the field.
              </p>
            </div>

            <div className="field">
              <label className="label" htmlFor="profile-priority">
                Priority
              </label>
              <input
                id="profile-priority"
                type="number"
                step={1}
                value={form.priority}
                onChange={(event) =>
                  setForm({ ...form, priority: event.target.value })
                }
              />
              <p className="hint">Higher priorities come first in the pool.</p>
            </div>
          </div>

          <div className="checkline">
            <input
              id="profile-streaming"
              type="checkbox"
              checked={form.streaming}
              onChange={(event) =>
                setForm({ ...form, streaming: event.target.checked })
              }
            />
            <label htmlFor="profile-streaming">Streaming</label>
          </div>

          <div className="checkline">
            <input
              id="profile-pool-exclude"
              type="checkbox"
              checked={form.poolExclude}
              onChange={(event) =>
                setForm({ ...form, poolExclude: event.target.checked })
              }
            />
            <label htmlFor="profile-pool-exclude">
              Exclude from the failover pool (task pins can still use it)
            </label>
          </div>

          {formState.kind === "edit" ? (
            <div className="actions">
              <button
                type="button"
                className="button"
                onClick={() => void fetchModels()}
                disabled={modelsBusy || !canFetchModels(editingId)}
              >
                {modelsBusy ? "Fetching…" : "⟳ Fetch models"}
              </button>
              <span className="hint" style={{ marginTop: 0 }}>
                {MODEL_FETCH_HINT}
              </span>
            </div>
          ) : (
            <p className="hint">{MODELS_PICKER_SAVE_FIRST_HINT}</p>
          )}

          {modelsNote !== null ? <p className="hint">{modelsNote}</p> : null}

          {formError !== null ? (
            <div className="errorbox" role="alert">
              {formError}
            </div>
          ) : null}

          <div className="actions">
            <button
              type="submit"
              className="button primary"
              disabled={saving}
            >
              {saving
                ? "Saving…"
                : formState.kind === "edit"
                  ? "Save changes"
                  : "Create profile"}
            </button>
            <button
              type="button"
              className="button"
              onClick={closeForm}
              disabled={saving}
            >
              Cancel
            </button>
          </div>
        </form>
      ) : null}
    </section>
  );
}

// ---------------------------------------------------------------------------
// One profile card
// ---------------------------------------------------------------------------

function ProfileCard(props: {
  profile: LlmProfile;
  plannerDefault: boolean;
  workerDefault: boolean;
  deleteArmed: boolean;
  deleteBusy: boolean;
  /** True while the edit form for THIS profile is open (Edit disabled). */
  formHidden: boolean;
  onEdit: () => void;
  onDelete: () => void;
  onArmDelete: () => void;
  onCancelDelete: () => void;
}) {
  const { profile } = props;
  return (
    <div className="card profile-card">
      <div className="row spread">
        <div className="row">
          <strong>{profile.name}</strong>
          <span className="chip" data-tone="plain">
            {profile.format}
          </span>
          {props.plannerDefault ? (
            <span className="chip" data-tone="accent">
              planner default
            </span>
          ) : null}
          {props.workerDefault ? (
            <span className="chip" data-tone="accent">
              worker default
            </span>
          ) : null}
          {profile.pool_exclude ? (
            <span className="chip" data-tone="warn">
              pool-excluded
            </span>
          ) : null}
          {isThinkingType(profile.thinking_type) &&
          profile.thinking_type !== "" ? (
            <span className="chip" data-tone="plain">
              thinking: {thinkingTypeLabel(profile.thinking_type)}
            </span>
          ) : null}
          {isReasoningEffort(profile.reasoning_effort) &&
          profile.reasoning_effort !== "" ? (
            <span className="chip" data-tone="plain">
              effort: {reasoningEffortLabel(profile.reasoning_effort)}
            </span>
          ) : null}
        </div>
        <span className="mono">#{profile.id}</span>
      </div>

      <dl className="kv profile-kv">
        <dt>Model</dt>
        <dd className="mono">{profile.model}</dd>
        <dt>Base URL</dt>
        <dd className="mono">{profile.base_url === "" ? "—" : profile.base_url}</dd>
        <dt>API key</dt>
        <dd>
          <span className="mono">{describeApiKeyHint(profile.api_key_hint)}</span>
        </dd>
        <dt>Caps</dt>
        <dd>
          <span className="chip">
            max_tokens {profile.max_tokens === 0 ? "∅ (no cap)" : profile.max_tokens}
          </span>{" "}
          <span className="chip">ctx {profile.context_window_k}k</span>{" "}
          <span className="chip">
            {profile.streaming ? "streaming" : "no streaming"}
          </span>{" "}
          <span className="chip">priority {profile.priority}</span>
        </dd>
      </dl>

      <div className="actions">
        <button
          type="button"
          className="button small"
          onClick={props.onEdit}
          disabled={props.formHidden}
        >
          Edit
        </button>
        {props.deleteArmed ? (
          <>
            <button
              type="button"
              className="button small deny"
              onClick={props.onDelete}
              disabled={props.deleteBusy}
            >
              Confirm delete
            </button>
            <button
              type="button"
              className="button small"
              onClick={props.onCancelDelete}
              disabled={props.deleteBusy}
            >
              Cancel
            </button>
            <span className="hint" style={{ marginTop: 0 }}>
              Deleting a role default or a task-pinned profile is refused —
              unbind it first.
            </span>
          </>
        ) : (
          <button
            type="button"
            className="button small deny"
            onClick={props.onArmDelete}
          >
            Delete
          </button>
        )}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Form state factories
// ---------------------------------------------------------------------------

function emptyForm(): ProfileFormValues {
  return {
    name: "",
    format: "openai",
    baseUrl: "",
    model: "",
    apiKey: "",
    maxTokens: "0",
    contextWindowK: "0",
    thinkingType: "",
    reasoningEffort: "",
    streaming: true,
    priority: "0",
    poolExclude: false,
  };
}

/** Populates the edit form from a row — the password field stays EMPTY. */
function fromProfile(profile: LlmProfile): ProfileFormValues {
  return {
    name: profile.name,
    format: profile.format,
    baseUrl: profile.base_url,
    model: profile.model,
    apiKey: "",
    maxTokens: String(profile.max_tokens),
    contextWindowK: String(profile.context_window_k),
    thinkingType: isThinkingType(profile.thinking_type)
      ? profile.thinking_type
      : "",
    reasoningEffort: isReasoningEffort(profile.reasoning_effort)
      ? profile.reasoning_effort
      : "",
    streaming: profile.streaming,
    priority: String(profile.priority),
    poolExclude: profile.pool_exclude,
  };
}
