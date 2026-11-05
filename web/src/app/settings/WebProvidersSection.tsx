"use client";

import { useEffect, useState } from "react";
import { ListOrdered, RefreshCw } from "lucide-react";

import {
  deleteWebProviderKey,
  reloadWebProviders,
  saveWebProvider,
  testWebProvider,
} from "@/lib/api";
import {
  WEB_PROVIDER_IDS,
  buildWebProviderUpdate,
  describeWebProviderKey,
  webProviderDefaultBaseUrl,
  webProviderOrderText,
  webProviderSourceLabel,
  webProviderStatusLabel,
  webProviderTestNote,
  type WebProviderFormValues,
} from "@/lib/settings";
import type {
  WebProviderId,
  WebProviderView,
  WebProvidersResponse,
} from "@/lib/types";
import { describeApiError } from "@/lib/ui";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";

/**
 * Web search providers (Settings v2): one card per router provider in the
 * fixed TinyFish → Parallel → Firecrawl order. Keys are write-only — the UI
 * only ever sees `key_hint` — and a blank key field keeps the stored key.
 *
 * The router reloads server-side after every save/remove; the DTO returned by
 * that request updates just the affected card. `Reload router` re-reads the
 * whole view when an operator wants a fresh order/status snapshot.
 */
export function WebProvidersSection(props: {
  providers: WebProvidersResponse | null;
  error: string | null;
  onProviderUpdated: (provider: WebProviderView) => void;
  onReloaded: (response: WebProvidersResponse) => void;
  onRetry: () => void;
}) {
  const { providers, error, onProviderUpdated, onReloaded, onRetry } = props;

  const [reloading, setReloading] = useState(false);
  const [reloadNote, setReloadNote] = useState<string | null>(null);
  const [reloadError, setReloadError] = useState<string | null>(null);

  async function handleReload(): Promise<void> {
    setReloading(true);
    setReloadNote(null);
    setReloadError(null);
    try {
      const response = await reloadWebProviders();
      onReloaded(response);
      setReloadNote("Router reloaded from the current provider configuration.");
    } catch (err) {
      setReloadError(describeApiError(err));
    } finally {
      setReloading(false);
    }
  }

  const ordered = providers === null ? [] : orderProviders(providers.providers);

  return (
    <section className="card" id="providers" aria-labelledby="providers-heading">
      <div className="row spread">
        <h2 id="providers-heading" style={{ margin: 0 }}>
          Web search providers
        </h2>
        <Button
          type="button"
          variant="outline"
          size="sm"
          onClick={() => void handleReload()}
          disabled={reloading}
        >
          <RefreshCw
            className={`h-3.5 w-3.5 ${reloading ? "animate-spin" : ""}`}
          />
          {reloading ? "Reloading…" : "Reload router"}
        </Button>
      </div>
      {providers !== null ? (
        <p className="hint flex items-start gap-1.5">
          <ListOrdered className="mt-0.5 h-3.5 w-3.5 shrink-0" />
          <span>{webProviderOrderText(providers)}</span>
        </p>
      ) : null}

      {error !== null ? (
        <div className="errorbox" role="alert">
          <div className="flex flex-wrap items-center gap-3">
            <span className="min-w-0 flex-1">
              Could not load web providers: {error}
            </span>
            <Button
              type="button"
              size="sm"
              variant="outline"
              className="h-7 border-destructive/30 text-destructive hover:bg-destructive/10 hover:text-destructive"
              onClick={onRetry}
            >
              Retry
            </Button>
          </div>
        </div>
      ) : null}

      {reloadNote !== null ? (
        <div className="successbox" role="status">
          {reloadNote}
        </div>
      ) : null}

      {reloadError !== null ? (
        <div className="errorbox" role="alert">
          Reload failed: {reloadError}
        </div>
      ) : null}

      {providers === null && error === null ? (
        <p className="empty">Loading web providers…</p>
      ) : null}

      {ordered.map((provider) => (
        <ProviderCard
          key={provider.provider}
          provider={provider}
          onUpdated={onProviderUpdated}
        />
      ))}
    </section>
  );
}

// ---------------------------------------------------------------------------
// One provider card
// ---------------------------------------------------------------------------

type CardNote = { tone: "success" | "error"; text: string };
type CardBusy = "save" | "test" | "remove" | null;

function ProviderCard(props: {
  provider: WebProviderView;
  onUpdated: (provider: WebProviderView) => void;
}) {
  const { provider, onUpdated } = props;
  const [form, setForm] = useState<WebProviderFormValues>(() =>
    formFromProvider(provider),
  );
  const [fieldError, setFieldError] = useState<string | null>(null);
  const [note, setNote] = useState<CardNote | null>(null);
  const [busy, setBusy] = useState<CardBusy>(null);
  const [removeArmed, setRemoveArmed] = useState(false);

  // A fresh DTO (this card's save/remove or a full router reload) resets the
  // form; the password field always clears, preserving write-only hygiene.
  useEffect(() => {
    setForm(formFromProvider(provider));
    setFieldError(null);
    setRemoveArmed(false);
  }, [provider]);

  const editableBaseUrl = provider.provider !== "tinyfish";
  const defaultBaseUrl = webProviderDefaultBaseUrl(provider.provider);

  function update(partial: Partial<WebProviderFormValues>): void {
    setForm((current) => ({ ...current, ...partial }));
    setFieldError(null);
    setNote(null);
  }

  async function handleSave(event: React.FormEvent<HTMLFormElement>): Promise<void> {
    event.preventDefault();
    setNote(null);
    setFieldError(null);
    const built = buildWebProviderUpdate(provider.provider, form);
    if (!built.ok) {
      setFieldError(built.error);
      return;
    }
    setBusy("save");
    try {
      const saved = await saveWebProvider(provider.provider, built.input);
      setNote({
        tone: "success",
        text: `Saved ${saved.display_name}.`,
      });
      onUpdated(saved);
    } catch (err) {
      setNote({ tone: "error", text: describeApiError(err) });
    } finally {
      setBusy(null);
    }
  }

  async function handleTest(): Promise<void> {
    setNote(null);
    setBusy("test");
    try {
      const result = await testWebProvider(provider.provider);
      setNote({ tone: "success", text: webProviderTestNote(result) });
    } catch (err) {
      setNote({ tone: "error", text: describeApiError(err) });
    } finally {
      setBusy(null);
    }
  }

  async function handleRemove(): Promise<void> {
    setNote(null);
    setBusy("remove");
    try {
      const cleared = await deleteWebProviderKey(provider.provider);
      setNote({
        tone: "success",
        text: `Removed the stored key for ${cleared.display_name}.`,
      });
      setRemoveArmed(false);
      onUpdated(cleared);
    } catch (err) {
      setNote({ tone: "error", text: describeApiError(err) });
    } finally {
      setBusy(null);
    }
  }

  const statusVariant =
    provider.status === "ready"
      ? "success"
      : provider.status === "missing_key"
        ? "warning"
        : "muted";

  return (
    <div className="profile-card">
      <div className="row spread">
        <div className="row">
          <strong>{provider.display_name}</strong>
          <Badge variant={statusVariant}>
            {webProviderStatusLabel(provider.status)}
          </Badge>
          <span className="chip">
            {webProviderSourceLabel(provider.source)}
          </span>
        </div>
        <span className="mono">Key {describeWebProviderKey(provider)}</span>
      </div>

      <form onSubmit={(event) => void handleSave(event)}>
        <div className="form-grid">
          <div className="field">
            <label
              className="label"
              htmlFor={`web-provider-${provider.provider}-key`}
            >
              API key
            </label>
            <input
              id={`web-provider-${provider.provider}-key`}
              type="password"
              autoComplete="off"
              value={form.apiKey}
              onChange={(event) => update({ apiKey: event.target.value })}
              placeholder={
                provider.configured
                  ? "Leave blank to keep the stored key"
                  : "Enter a new key"
              }
            />
            <p className="hint">
              {provider.configured
                ? "Blank keeps the stored key; a new value replaces it."
                : "Stored write-only; reads only ever show the last four characters."}
            </p>
          </div>

          {editableBaseUrl ? (
            <div className="field">
              <label
                className="label"
                htmlFor={`web-provider-${provider.provider}-base-url`}
              >
                Base URL
              </label>
              <input
                id={`web-provider-${provider.provider}-base-url`}
                value={form.baseUrl}
                onChange={(event) => update({ baseUrl: event.target.value })}
                placeholder={defaultBaseUrl ?? ""}
              />
              <p className="hint">
                Empty clears the saved override; the environment or built-in
                default applies
                {defaultBaseUrl !== null ? ` (${defaultBaseUrl})` : ""}.
              </p>
            </div>
          ) : null}

          <div className="field">
            <label
              className="label"
              htmlFor={`web-provider-${provider.provider}-timeout`}
            >
              Timeout (seconds)
            </label>
            <input
              id={`web-provider-${provider.provider}-timeout`}
              type="number"
              min={1}
              max={3600}
              step={1}
              value={form.timeoutSecs}
              aria-invalid={fieldError !== null}
              onChange={(event) => update({ timeoutSecs: event.target.value })}
            />
            {fieldError !== null ? (
              <p className="hint text-destructive" role="alert">
                {fieldError}
              </p>
            ) : (
              <p className="hint">1–3600 seconds.</p>
            )}
          </div>
        </div>

        <div className="checkline">
          <input
            id={`web-provider-${provider.provider}-enabled`}
            type="checkbox"
            checked={form.enabled}
            onChange={(event) => update({ enabled: event.target.checked })}
          />
          <label htmlFor={`web-provider-${provider.provider}-enabled`}>
            Enabled for routing
          </label>
        </div>

        <div className="actions">
          <button
            type="submit"
            className="button primary"
            disabled={busy !== null}
          >
            {busy === "save" ? "Saving…" : "Save"}
          </button>
          <button
            type="button"
            className="button"
            onClick={() => void handleTest()}
            disabled={busy !== null || !provider.configured || !provider.enabled}
            title={
              !provider.configured
                ? "Configure a key before testing"
                : !provider.enabled
                  ? "Enable the provider before testing"
                  : undefined
            }
          >
            {busy === "test" ? "Testing…" : "Test connection"}
          </button>
          {removeArmed ? (
            <>
              <button
                type="button"
                className="button deny"
                onClick={() => void handleRemove()}
                disabled={busy !== null}
              >
                {busy === "remove" ? "Removing…" : "Confirm remove"}
              </button>
              <button
                type="button"
                className="button"
                onClick={() => setRemoveArmed(false)}
                disabled={busy !== null}
              >
                Cancel
              </button>
            </>
          ) : (
            <button
              type="button"
              className="button deny"
              onClick={() => {
                setNote(null);
                setRemoveArmed(true);
              }}
              disabled={busy !== null || !provider.configured}
            >
              Remove key
            </button>
          )}
        </div>

        {note !== null ? (
          note.tone === "success" ? (
            <div className="successbox" role="status">
              {note.text}
            </div>
          ) : (
            <div className="errorbox" role="alert">
              {note.text}
            </div>
          )
        ) : null}
      </form>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Form + ordering helpers
// ---------------------------------------------------------------------------

function formFromProvider(provider: WebProviderView): WebProviderFormValues {
  return {
    apiKey: "",
    baseUrl: provider.base_url ?? "",
    timeoutSecs: String(provider.timeout_secs),
    enabled: provider.enabled,
  };
}

/** Fixed router order, regardless of what the response array contains. */
function orderProviders(providers: WebProviderView[]): WebProviderView[] {
  const rank = (id: WebProviderId): number => WEB_PROVIDER_IDS.indexOf(id);
  return [...providers].sort((a, b) => rank(a.provider) - rank(b.provider));
}
