/**
 * Pure helpers for the settings control room (profiles CRUD, model picker,
 * pool status, guard rules, role assignment, general settings).
 *
 * Framework-free (types + `ApiError` only, mirroring `ui.ts`/`approvals.ts`)
 * so `settings.test.ts` drives every path under `node --test`.
 *
 * Wire truth (`crates/dhi-server/src/lib.rs` and
 * `crates/dhi-store/src/mgmt.rs`):
 * - `LlmProfileOut` serves exactly: id, name, format, base_url, model,
 *   api_key_hint, max_tokens, context_window_k, thinking_type,
 *   reasoning_effort, streaming, priority, pool_exclude, created_at,
 *   updated_at — `api_key` is write-only and the hint is `…last4` from
 *   `key_hint` (EMPTY when the stored key is shorter than four characters —
 *   so an empty hint does NOT prove the key is unset; it may be a tiny key).
 * - `POST /api/llm/profiles`: `Store::save_profile` treats an absent, `null`,
 *   `0`, or negative `id` as CREATE and only a positive id as an edit. An
 *   absent/empty/`null` `api_key` on edit keeps the stored key
 *   (`SaveProfileRequest` and `Store::save_profile`). Negative `max_tokens`
 *   CLAMPS to 0 in `Store::save_profile` before validation — it is never a
 *   400 despite the validator's own unreachable arm.
 * - `PUT /api/settings` is a partial merge where explicit `null` behaves
 *   exactly like omission: `SettingPatch` folds `None`/null to the stored
 *   value (serde turns a JSON `null` into `None`). There is
 *   therefore NO wire way to clear a role default — "unchanged" is expressed
 *   by omitting the field, which both settings patch builders do.
 * - On a literally zero-profile database `PUT /api/settings` answers `404
 *   "app settings are not initialized"` from `Store::update_settings` even
 *   for timeout-only patches; `GET` always answers 200 with the unseeded
 *   view. Creating the first profile bootstraps the singleton in
 *   `Store::save_profile`.
 * - Guard rules evaluate in priority DESC / id ASC order, FIRST MATCH WINS;
 *   every mutation hot-rebuilds the compiled policy (no restart). An empty
 *   rule `reason` becomes the server-generated "matched rule <name>" in
 *   `crates/dhi-guard/src/snapshot.rs`.
 * - The pool routes (`/api/llm/pool/status`, `/api/llm/pool/reset`) read the
 *   in-memory snapshot only and never touch `require_mgmt`, so unlike the
 *   other management routes they cannot answer the unwired-store 500 — they
 *   always 200 (`pool_status_route`, `pool_reset_route`). The UI still
 *   handles failures defensively.
 */

import { ApiError } from "./api.ts";
import type {
  AppSettingsView,
  ChainEntry,
  GuardAction,
  GuardRuleInput,
  LlmProfile,
  MatchTarget,
  MatchType,
  PoolStatus,
  ProfileFormat,
  ReasoningEffort,
  SaveProfileInput,
  SettingsPatch,
  ThinkingType,
  WebProviderId,
  WebProviderTestResult,
  WebProviderUpdate,
  WebProviderView,
  WebProvidersResponse,
} from "./types.ts";

// ---------------------------------------------------------------------------
// Tunables + shared copy
// ---------------------------------------------------------------------------

/** Pool auto-refresh cadence — transient state, refreshed gently (10 s). */
export const POOL_POLL_INTERVAL_MS = 10000;

/**
 * The stand-in baseline for a settings view the server has not answered yet
 * (component still loading). Field values mirror the server's `unseeded()`
 * view (`AppSettingsView::unseeded`) so the patch builder behaves identically
 * before and after the first load.
 */
export const BASELINE_SETTINGS: AppSettingsView = {
  planner_profile: null,
  worker_profile: null,
  llm_key_set: false,
  planner_api_key_hint: "",
  worker_api_key_hint: "",
  approval_timeout_secs: 900,
  guard_default_action: "deny",
};

/** Friendly copy for the zero-profile `PUT /api/settings` 404 (see above). */
export const UNSEEDED_SETTINGS_HINT =
  "Settings are not initialized — create a profile first (saving the first " +
  "profile bootstraps the settings row).";

/** Shown under the model field when the picker is unavailable in create mode. */
export const MODELS_PICKER_SAVE_FIRST_HINT =
  "Save the profile first to fetch models — fetching needs the stored key.";

/** Shown next to the fetch button in edit mode (registered 5E limitation). */
export const MODEL_FETCH_HINT =
  "Lists the upstream model ids via this profile's stored key (openai-format profiles only).";

// ---------------------------------------------------------------------------
// Small shared parse utility
// ---------------------------------------------------------------------------

/**
 * Parses an integer from an `<input>` string. Accepts an optional leading
 * `+`/`-`, digits only, and forbids decimals/exponent/garbage. Returns
 * `null` for anything non-integral or non-finite — callers decide whether
 * that is an error or an omission.
 */
export function parseIntOrNull(raw: string): number | null {
  const trimmed = raw.trim();
  if (!/^[+-]?\d+$/.test(trimmed)) {
    return null;
  }
  const value = Number(trimmed);
  return Number.isSafeInteger(value) ? value : null;
}

// ---------------------------------------------------------------------------
// Key-hint display (hint only — a key NEVER reaches the UI)
// ---------------------------------------------------------------------------

/**
 * Display text for a profile's `api_key_hint`. Non-empty hints pass through
 * verbatim (`…last4`). An EMPTY hint is ambiguous by wire design — it means
 * either "no key stored" or "key shorter than four characters" (`key_hint`)
 * — so the copy says exactly that instead of guessing. Hostile shapes
 * degrade to the same explainer; this never throws.
 */
export function describeApiKeyHint(hint: unknown): string {
  if (typeof hint === "string" && hint.trim() !== "") {
    return hint;
  }
  return "(no hint — key unset or shorter than 4 chars)";
}

// ---------------------------------------------------------------------------
// Profile form → request builder
// ---------------------------------------------------------------------------

/** Every accepted `thinking_type` wire value, in display order. */
export const THINKING_TYPE_VALUES: readonly ThinkingType[] = [
  "",
  "disabled",
  "enabled",
];

/** Every accepted `reasoning_effort` wire value, in display order. */
export const REASONING_EFFORT_VALUES: readonly ReasoningEffort[] = [
  "",
  "low",
  "medium",
  "high",
  "xhigh",
  "max",
];

/** True when `value` is a `thinking_type` the API accepts. */
export function isThinkingType(value: unknown): value is ThinkingType {
  return (
    typeof value === "string" &&
    (THINKING_TYPE_VALUES as readonly string[]).includes(value)
  );
}

/** True when `value` is a `reasoning_effort` the API accepts. */
export function isReasoningEffort(value: unknown): value is ReasoningEffort {
  return (
    typeof value === "string" &&
    (REASONING_EFFORT_VALUES as readonly string[]).includes(value)
  );
}

/**
 * Validation message for a `thinking_type`, or `null` when it is one of the
 * documented wire values. Mirrors the server's accepted enum.
 */
export function validateThinkingType(value: unknown): string | null {
  return isThinkingType(value)
    ? null
    : "Thinking must be Not sent, Disabled, or Enabled.";
}

/**
 * Validation message for a `reasoning_effort`, or `null` when it is one of
 * the documented wire values. Mirrors the server's accepted enum.
 */
export function validateReasoningEffort(value: unknown): string | null {
  return isReasoningEffort(value)
    ? null
    : "Reasoning effort must be Not sent, Low, Medium, High, XHigh, or Max.";
}

/**
 * Display label for a profile's thinking control. `""` is the explicit
 * "field omitted" state, not a provider value of its own.
 */
export function thinkingTypeLabel(value: unknown): string {
  if (value === "") {
    return "Not sent";
  }
  if (value === "disabled") {
    return "Disabled";
  }
  if (value === "enabled") {
    return "Enabled";
  }
  return typeof value === "string" && value.trim() !== ""
    ? value
    : "Not sent";
}

/** Display label for a profile's reasoning-effort control. */
export function reasoningEffortLabel(value: unknown): string {
  if (value === "") {
    return "Not sent";
  }
  if (value === "low") {
    return "Low";
  }
  if (value === "medium") {
    return "Medium";
  }
  if (value === "high") {
    return "High";
  }
  if (value === "xhigh") {
    return "XHigh";
  }
  if (value === "max") {
    return "Max";
  }
  return typeof value === "string" && value.trim() !== ""
    ? value
    : "Not sent";
}

/** Raw profile form state (HTML inputs hand us strings). */
export type ProfileFormValues = {
  name: string;
  format: ProfileFormat;
  baseUrl: string;
  model: string;
  /** Password field; blank on edit means "keep the stored key". */
  apiKey: string;
  maxTokens: string;
  contextWindowK: string;
  /** Independent provider controls; `""` omits the field from the request. */
  thinkingType: ThinkingType;
  reasoningEffort: ReasoningEffort;
  streaming: boolean;
  priority: string;
  poolExclude: boolean;
};

/**
 * Mirrors the server's documented profile `400`s (`LlmProfileUpsert::validate`
 * and `SaveProfileRequest`): name and model must not be blank, format is the
 * two-value enum, numerics must be whole numbers. Negative `max_tokens` is
 * refused HERE as a courtesy even though the server clamps it to 0 in
 * `Store::save_profile` — the UI saying so is clearer than silent clamping,
 * and every other bad value really is a server 400. Returns an error
 * message, or `null` when the form is usable.
 */
export function validateProfileForm(form: ProfileFormValues): string | null {
  if (form.name.trim() === "") {
    return "Name is required.";
  }
  if (form.format !== "openai" && form.format !== "anthropic") {
    return "Format must be openai or anthropic.";
  }
  const thinkingError = validateThinkingType(form.thinkingType);
  if (thinkingError !== null) {
    return thinkingError;
  }
  const effortError = validateReasoningEffort(form.reasoningEffort);
  if (effortError !== null) {
    return effortError;
  }
  if (form.model.trim() === "") {
    return "Model is required.";
  }
  const maxTokens = parseIntOrNull(form.maxTokens);
  if (form.maxTokens.trim() !== "" && maxTokens === null) {
    return "Max tokens must be a whole number.";
  }
  if (maxTokens !== null && maxTokens < 0) {
    return "Max tokens must be 0 or more (0 = no cap).";
  }
  const contextK = parseIntOrNull(form.contextWindowK);
  if (form.contextWindowK.trim() !== "" && contextK === null) {
    return "Context window must be a whole number.";
  }
  if (contextK !== null && contextK < 0) {
    return "Context window must be 0 or more.";
  }
  const priority = parseIntOrNull(form.priority);
  if (form.priority.trim() !== "" && priority === null) {
    return "Priority must be a whole number.";
  }
  return null;
}

/** Result of {@link buildProfilePayload}. */
export type ProfilePayloadResult =
  | { ok: true; input: SaveProfileInput }
  | { ok: false; error: string };

/**
 * Pure form → `POST /api/llm/profiles` body.
 *
 * Id semantics (ARTEX parity, `Store::save_profile`): a NON-POSITIVE or
 * absent id means CREATE — an editing id of `0` is normalized to omission,
 * which is the `api.ts` `saveProfile` create convention
 * ("id 0 ≡ absent ≡ create"). Only a positive id is sent for edits.
 *
 * Key hygiene: a blank/whitespace key is OMITTED from the body — on edit
 * the server then keeps the stored key and hint (`Store::save_profile`); on
 * create it stores no key. A typed key is trimmed. Either way the plaintext
 * key is used once for the request and never cached in component state
 * afterwards.
 *
 * Thinking controls are ALWAYS included, even as `""`, so editing can clear
 * a stored value; `""` means "do not send the provider field".
 */
export function buildProfilePayload(
  form: ProfileFormValues,
  editingId: number | null,
): ProfilePayloadResult {
  const validationError = validateProfileForm(form);
  if (validationError !== null) {
    return { ok: false, error: validationError };
  }
  const input: SaveProfileInput = {
    name: form.name.trim(),
    format: form.format,
    base_url: form.baseUrl.trim(),
    model: form.model.trim(),
    thinking_type: form.thinkingType,
    reasoning_effort: form.reasoningEffort,
    streaming: form.streaming,
    pool_exclude: form.poolExclude,
  };
  if (editingId !== null && editingId > 0) {
    input.id = editingId;
  }
  const key = form.apiKey.trim();
  if (key !== "") {
    input.api_key = key;
  }
  const maxTokens = parseIntOrNull(form.maxTokens);
  if (maxTokens !== null) {
    input.max_tokens = maxTokens;
  }
  const contextK = parseIntOrNull(form.contextWindowK);
  if (contextK !== null) {
    input.context_window_k = contextK;
  }
  const priority = parseIntOrNull(form.priority);
  if (priority !== null) {
    input.priority = priority;
  }
  return { ok: true, input };
}

/** Editing id carried by a profile row, or `null` when it is unusable. */
export function editingIdOf(profile: LlmProfile | null): number | null {
  return profile !== null && Number.isFinite(profile.id) && profile.id > 0
    ? profile.id
    : null;
}

// ---------------------------------------------------------------------------
// Model picker
// ---------------------------------------------------------------------------

/**
 * Shapes a `GET /api/llm/models` response into picker options: strings only,
 * trimmed, empties dropped, duplicates collapsed preserving upstream order,
 * capped at 200 entries (DOM protection). Anything hostile degrades to an
 * empty list — never throws.
 */
export function toModelOptions(models: unknown): string[] {
  if (!Array.isArray(models)) {
    return [];
  }
  const seen = new Set<string>();
  const options: string[] = [];
  for (const raw of models) {
    if (typeof raw !== "string") {
      continue;
    }
    const value = raw.trim();
    if (value === "" || seen.has(value)) {
      continue;
    }
    seen.add(value);
    options.push(value);
    if (options.length >= 200) {
      break;
    }
  }
  return options;
}

/**
 * Whether the models picker can be offered for a profile. The picker is an
 * EDIT-time affordance only: it calls `GET /api/llm/models?profile_id=N`
 * which reads the STORED key (`list_models_route`) — a brand-new unsaved
 * profile has no id/key yet. Registered limitation.
 */
export function canFetchModels(editingId: number | null): boolean {
  return editingId !== null && editingId > 0;
}

// ---------------------------------------------------------------------------
// Pool chains (transient, in-memory breaker state)
// ---------------------------------------------------------------------------

/** Visual tone of a chain entry's failure state. */
export type ChainTone = "ok" | "warn" | "danger";

/** One renderable chip on a chain entry. */
export type ChainChip = { text: string; tone: ChainTone };

/**
 * Chips describing one chain entry's breaker state: a red `tripped` badge
 * when tripped (plus the failure counter when above zero — the badge
 * explains the skip, the counter says how far past the threshold the
 * profile slid), an amber `${n} consecutive failures` counter when merely
 * accumulating failures, and a calm `healthy` chip otherwise. Hostile
 * shapes (junk entries, NaN counters) degrade gracefully.
 */
export function chainEntryChips(entry: unknown): ChainChip[] {
  if (entry === null || typeof entry !== "object") {
    return [{ text: "healthy", tone: "ok" }];
  }
  const record = entry as { tripped?: unknown; consecutive_failures?: unknown };
  const tripped = record.tripped === true;
  const failures =
    typeof record.consecutive_failures === "number" &&
    Number.isFinite(record.consecutive_failures)
      ? Math.max(0, Math.floor(record.consecutive_failures))
      : 0;
  if (tripped) {
    // The counter rides along even when tripped: the red badge explains the
    // skip, the counter says how far past the threshold the profile slid.
    const chips: ChainChip[] = [{ text: "tripped", tone: "danger" }];
    if (failures > 0) {
      chips.push({
        text:
          failures === 1
            ? "1 consecutive failure"
            : `${failures} consecutive failures`,
        tone: "warn",
      });
    }
    return chips;
  }
  if (failures > 0) {
    return [
      {
        text:
          failures === 1
            ? "1 consecutive failure"
            : `${failures} consecutive failures`,
        tone: "warn",
      },
    ];
  }
  return [{ text: "healthy", tone: "ok" }];
}

/** Partitions a chain into healthy and tripped entries (order preserved). */
export function splitChain(
  chain: readonly ChainEntry[] | null | undefined,
): { healthy: ChainEntry[]; tripped: ChainEntry[] } {
  const healthy: ChainEntry[] = [];
  const tripped: ChainEntry[] = [];
  if (!Array.isArray(chain)) {
    return { healthy, tripped };
  }
  for (const entry of chain) {
    if (entry !== null && typeof entry === "object" && entry.tripped === true) {
      tripped.push(entry);
    } else if (entry !== null && typeof entry === "object") {
      healthy.push(entry);
    }
  }
  return { healthy, tripped };
}

/**
 * Counts tripped entries across BOTH role chains — drives the "reset all"
 * affordance's badge. Hostile shapes count as zero.
 */
export function countTrippedEntries(status: PoolStatus | null): number {
  if (status === null || typeof status !== "object") {
    return 0;
  }
  const record = status as { planner?: unknown; worker?: unknown };
  let total = 0;
  for (const role of [record.planner, record.worker]) {
    const chain =
      role !== null && typeof role === "object"
        ? (role as { chain?: unknown }).chain
        : null;
    if (Array.isArray(chain)) {
      total += splitChain(chain as ChainEntry[]).tripped.length;
    }
  }
  return total;
}

/** Transient-state caption under the pool card. */
// ---------------------------------------------------------------------------
// Web search providers (Settings v2)
// ---------------------------------------------------------------------------

/** Fixed router order. The UI never reorders or exposes priority. */
export const WEB_PROVIDER_IDS: readonly WebProviderId[] = [
  "tinyfish",
  "parallel",
  "firecrawl",
];

/** Display names used when a provider DTO is not loaded yet. */
export const WEB_PROVIDER_LABELS: Record<WebProviderId, string> = {
  tinyfish: "TinyFish",
  parallel: "Parallel",
  firecrawl: "Firecrawl",
};

/** Accepted timeout range from the frozen settings contract. */
export const WEB_PROVIDER_TIMEOUT_MIN_SECS = 1;
export const WEB_PROVIDER_TIMEOUT_MAX_SECS = 3600;

/**
 * Built-in base URL placeholders (the router's own defaults). TinyFish has
 * separate search/fetch endpoints and no user-editable base URL.
 */
export function webProviderDefaultBaseUrl(
  provider: WebProviderId,
): string | null {
  if (provider === "parallel") {
    return "https://api.parallel.ai";
  }
  if (provider === "firecrawl") {
    return "https://api.firecrawl.dev";
  }
  return null;
}

/** Raw provider-card form state (HTML inputs hand us strings). */
export type WebProviderFormValues = {
  /** Password field; blank always means "keep the stored key". */
  apiKey: string;
  /** Only rendered for Parallel and Firecrawl; "" clears the override. */
  baseUrl: string;
  timeoutSecs: string;
  enabled: boolean;
};

/**
 * Mirrors the frozen provider contract: timeout is a whole number in
 * 1..3600. A blank key is valid (keep the stored key), and an empty base URL
 * is valid (restore the built-in default). Server truth still wins for any
 * provider-side validation.
 */
export function validateWebProviderForm(
  form: WebProviderFormValues,
): string | null {
  if (form.timeoutSecs.trim() === "") {
    return "Timeout is required.";
  }
  const timeout = parseIntOrNull(form.timeoutSecs);
  if (timeout === null) {
    return "Timeout must be a whole number of seconds.";
  }
  if (
    timeout < WEB_PROVIDER_TIMEOUT_MIN_SECS ||
    timeout > WEB_PROVIDER_TIMEOUT_MAX_SECS
  ) {
    return "Timeout must be between 1 and 3600 seconds.";
  }
  return null;
}

/** Result of {@link buildWebProviderUpdate}. */
export type WebProviderUpdateResult =
  | { ok: true; input: WebProviderUpdate }
  | { ok: false; error: string };

/**
 * Pure provider-card form → partial `PUT /api/web/providers/{provider}` body.
 *
 * Key hygiene: a blank/whitespace key is OMITTED, so the stored key is kept;
 * a typed key is trimmed and sent once. `base_url` is always sent for
 * Parallel/Firecrawl so an emptied field actively clears the override
 * (omitting it would keep it); TinyFish has no base URL field at all.
 */
export function buildWebProviderUpdate(
  provider: WebProviderId,
  form: WebProviderFormValues,
): WebProviderUpdateResult {
  const validationError = validateWebProviderForm(form);
  if (validationError !== null) {
    return { ok: false, error: validationError };
  }
  const timeout = parseIntOrNull(form.timeoutSecs);
  if (timeout === null) {
    // validateWebProviderForm already rejected this; keep the narrowing
    // explicit so the payload can never carry a non-number.
    return { ok: false, error: "Timeout must be a whole number of seconds." };
  }
  const input: WebProviderUpdate = {
    timeout_secs: timeout,
    enabled: form.enabled,
  };
  const key = form.apiKey.trim();
  if (key !== "") {
    input.api_key = key;
  }
  if (provider !== "tinyfish") {
    input.base_url = form.baseUrl.trim();
  }
  return { ok: true, input };
}

/** Badge copy for a provider `status`. Hostile values stay visible. */
export function webProviderStatusLabel(status: unknown): string {
  if (status === "ready") {
    return "Ready";
  }
  if (status === "missing_key") {
    return "Missing key";
  }
  if (status === "disabled") {
    return "Disabled";
  }
  return String(status ?? "Unknown");
}

/** Source copy for a provider `source` (database/environment/config/none). */
export function webProviderSourceLabel(source: unknown): string {
  if (source === "database") {
    return "Database";
  }
  if (source === "environment") {
    return "Environment";
  }
  if (source === "config_file") {
    return "Config file";
  }
  if (source === "none") {
    return "None";
  }
  return String(source ?? "Unknown");
}

/**
 * Key display for a provider card: the server's mask when configured, or
 * `No key` when not. Configured without a hint is explicitly "No hint"
 * rather than claiming a key is absent.
 */
export function describeWebProviderKey(
  provider: Pick<WebProviderView, "configured" | "key_hint"> | null | undefined,
): string {
  if (provider === null || provider === undefined || !provider.configured) {
    return "No key";
  }
  return typeof provider.key_hint === "string" && provider.key_hint.trim() !== ""
    ? provider.key_hint
    : "No hint";
}

function providerName(id: unknown): string {
  return typeof id === "string" &&
    (WEB_PROVIDER_IDS as readonly string[]).includes(id)
    ? WEB_PROVIDER_LABELS[id as WebProviderId]
    : String(id ?? "?");
}

/**
 * Read-only router order caption, built only from the response arrays:
 * `Search/read: TinyFish → Parallel → Firecrawl · Research: Parallel`.
 */
export function webProviderOrderText(
  response: WebProvidersResponse | null | undefined,
): string {
  if (response === null || response === undefined) {
    return "";
  }
  const search = Array.isArray(response.search_read_order)
    ? response.search_read_order.map(providerName)
    : [];
  const research = Array.isArray(response.research_order)
    ? response.research_order.map(providerName)
    : [];
  return `Search/read: ${
    search.length > 0 ? search.join(" → ") : "—"
  } · Research: ${research.length > 0 ? research.join(" → ") : "—"}`;
}

/** Counts provider DTOs whose router status is `ready` (General overview). */
export function countReadyWebProviders(
  response: WebProvidersResponse | null | undefined,
): number {
  if (response === null || response === undefined || !Array.isArray(response.providers)) {
    return 0;
  }
  return response.providers.filter((provider) => provider.status === "ready")
    .length;
}

/** Success copy for a live provider probe: `Connected in 123 ms`. */
export function webProviderTestNote(
  result: Pick<WebProviderTestResult, "latency_ms"> | null | undefined,
): string {
  const raw = result?.latency_ms;
  const latency =
    typeof raw === "number" && Number.isFinite(raw)
      ? Math.max(0, Math.round(raw))
      : 0;
  return `Connected in ${latency} ms`;
}

// ---------------------------------------------------------------------------
// Guard rules
// ---------------------------------------------------------------------------

/** Raw guard-rule form state (HTML inputs hand us strings). */
export type RuleFormValues = {
  name: string;
  matchTarget: MatchTarget;
  matchType: MatchType;
  pattern: string;
  action: GuardAction;
  reason: string;
  priority: string;
  enabled: boolean;
  timeoutSecs: string;
};

/**
 * Validates the regex field BEFORE the server can 400: `match_type=regex`
 * compiles the pattern through `new RegExp` in a try/catch and reports the
 * engine error inline. CAVEAT (documented, not hidden): the server compiles
 * with the Rust `regex` crate (`TryFrom<RuleRequest> for GuardRuleInput`),
 * whose dialect differs from JavaScript's — a pattern both accept passes
 * either way, a pattern only JS
 * rejects saves a round-trip, and a pattern only RUST rejects still surfaces
 * the server's 400 verbatim. This check is advisory pre-flight; the server
 * remains the authority. Returns an error message or `null`.
 */
export function validateRuleForm(form: RuleFormValues): string | null {
  if (form.name.trim() === "") {
    return "Name is required.";
  }
  if (form.matchTarget !== "tool_name" && form.matchTarget !== "tool_input") {
    return 'Match target must be "tool_name" or "tool_input".';
  }
  if (form.matchType !== "exact" && form.matchType !== "regex") {
    return 'Match type must be "exact" or "regex".';
  }
  if (form.action !== "allow" && form.action !== "deny" && form.action !== "ask") {
    return 'Action must be "allow", "deny", or "ask".';
  }
  if (form.pattern.trim() === "") {
    return "Pattern is required.";
  }
  if (form.matchType === "regex") {
    try {
      void new RegExp(form.pattern);
    } catch (cause) {
      const message =
        cause instanceof Error ? cause.message : String(cause);
      return `Invalid regex pattern: ${message}`;
    }
  }
  const priority = parseIntOrNull(form.priority);
  if (form.priority.trim() !== "" && priority === null) {
    return "Priority must be a whole number.";
  }
  const timeout = parseIntOrNull(form.timeoutSecs);
  if (form.timeoutSecs.trim() !== "" && timeout === null) {
    return "Timeout must be a whole number of seconds.";
  }
  if (timeout !== null && timeout < 0) {
    return "Timeout must be 0 or more (0 = use the app default).";
  }
  return null;
}

/** Result of {@link buildRulePayload}. */
export type RulePayloadResult =
  | { ok: true; input: GuardRuleInput }
  | { ok: false; error: string };

/** Pure form → guard-rule create/replace body (full replace semantics). */
export function buildRulePayload(form: RuleFormValues): RulePayloadResult {
  const validationError = validateRuleForm(form);
  if (validationError !== null) {
    return { ok: false, error: validationError };
  }
  const input: GuardRuleInput = {
    name: form.name.trim(),
    match_target: form.matchTarget,
    match_type: form.matchType,
    pattern: form.pattern.trim(),
    action: form.action,
    reason: form.reason.trim(),
    enabled: form.enabled,
  };
  const priority = parseIntOrNull(form.priority);
  if (priority !== null) {
    input.priority = priority;
  }
  const timeout = parseIntOrNull(form.timeoutSecs);
  if (timeout !== null) {
    input.timeout_secs = timeout;
  }
  return { ok: true, input };
}

/** Badge `data-state` for a rule's action chip (deny shares the red family). */
export function ruleActionTone(action: unknown): GuardAction | "unknown" {
  return action === "allow" || action === "deny" || action === "ask"
    ? action
    : "unknown";
}

/**
 * Human-readable match semantics for a rule row, e.g.
 * `tool named exactly` / `tool input matching /…/`. Hostile enums degrade to
 * the raw values joined.
 */
export function ruleMatchDescription(
  matchTarget: unknown,
  matchType: unknown,
): string {
  const target =
    matchTarget === "tool_name" || matchTarget === "tool_input"
      ? matchTarget
      : String(matchTarget ?? "?");
  const kind =
    matchType === "exact" || matchType === "regex" ? matchType : String(matchType ?? "?");
  if (target === "tool_name" && kind === "exact") {
    return "tool name equals the pattern";
  }
  if (target === "tool_name" && kind === "regex") {
    return "tool name matches the regex";
  }
  if (target === "tool_input" && kind === "exact") {
    return "serialized input contains the pattern (case-insensitive)";
  }
  if (target === "tool_input" && kind === "regex") {
    return "serialized input matches the regex";
  }
  return `${target} · ${kind}`;
}

/**
 * Effective ask-reason display: the stored reason, or — when the rule was
 * saved with an empty reason — the exact auto-text the guard snapshot
 * generates at evaluation time (`matched rule <name>`,
 * `CompiledPolicy` in `dhi-guard/src/snapshot.rs`).
 */
export function ruleReasonOrAuto(name: unknown, reason: unknown): string {
  if (typeof reason === "string" && reason.trim() !== "") {
    return reason;
  }
  const ruleName = typeof name === "string" && name.trim() !== "" ? name : "?";
  return `matched rule ${ruleName}`;
}

// ---------------------------------------------------------------------------
// Role assignment partial patch (only-changed-fields contract)
// ---------------------------------------------------------------------------

/** Raw role-assignment form state (both values are `<select>` ids). */
export type RoleAssignmentFormValues = {
  /** "" = no selection; unchanged/unknown roles are omitted, never null. */
  plannerChoice: string;
  workerChoice: string;
};

/** Result of {@link buildRoleAssignmentPatch}. */
export type RoleAssignmentPatchResult =
  | { ok: true; patch: SettingsPatch; changed: string[] }
  | { ok: false; error: string };

/**
 * Builds the `PUT /api/settings` body for the dedicated role-assignment
 * section. Both role choices are compared against the current view and only
 * genuinely changed ids are emitted, so selecting the same profile for both
 * roles sends BOTH fields in one request (same profile is valid), while
 * changing one role sends only that field.
 *
 * The server folds an explicit JSON `null` into "keep the stored value"
 * (`SettingPatch`), so a cleared/empty select is expressed by omission and
 * this builder NEVER emits null. There is no wire way to unset a role
 * default, so "" always means "leave this role alone".
 *
 * Profile ids are only checked for integer shape here; the server remains
 * the authority on whether an id exists (a stale selection surfaces its
 * `404` verbatim through {@link explainSettingsWriteFailure}).
 */
export function buildRoleAssignmentPatch(
  view: AppSettingsView | null,
  draft: RoleAssignmentFormValues,
): RoleAssignmentPatchResult {
  const baseline = view ?? BASELINE_SETTINGS;
  const patch: SettingsPatch = {};
  const changed: string[] = [];

  const plannerId = parseIntOrNull(draft.plannerChoice);
  if (draft.plannerChoice.trim() !== "" && plannerId === null) {
    return { ok: false, error: "Planner profile selection is invalid." };
  }
  if (plannerId !== null && plannerId !== baseline.planner_profile?.id) {
    patch.planner_profile_id = plannerId;
    changed.push("planner_profile_id");
  }

  const workerId = parseIntOrNull(draft.workerChoice);
  if (draft.workerChoice.trim() !== "" && workerId === null) {
    return { ok: false, error: "Worker profile selection is invalid." };
  }
  if (workerId !== null && workerId !== baseline.worker_profile?.id) {
    patch.worker_profile_id = workerId;
    changed.push("worker_profile_id");
  }

  return { ok: true, patch, changed };
}

// ---------------------------------------------------------------------------
// General settings partial patch (only-changed-fields contract)
// ---------------------------------------------------------------------------

/** Raw general-settings form state (HTML inputs hand us strings). */
export type SettingsFormValues = {
  approvalTimeout: string;
  guardDefaultAction: GuardAction;
};

/** Result of {@link buildSettingsPatch}. */
export type SettingsPatchResult =
  | { ok: true; patch: SettingsPatch; changed: string[] }
  | { ok: false; error: string };

/**
 * Builds the `PUT /api/settings` body carrying ONLY the general fields the
 * operator actually changed — the partial-merge contract. Role defaults live
 * in the dedicated {@link buildRoleAssignmentPatch} so the two selectors are
 * never duplicated.
 *
 * The baseline is the CURRENT VIEW for changed-field detection; a missing
 * view (still loading) falls back to {@link BASELINE_SETTINGS}, whose values
 * equal the server's own unseeded defaults, so a not-yet-loaded page and a
 * freshly-loaded unseeded page compute identical patches.
 *
 * Validation mirrors the server's 400s: `approval_timeout_secs` must be a
 * whole number >= 1 and `guard_default_action` must be one of the three enum
 * values; `Store::update_settings` enforces both.
 */
export function buildSettingsPatch(
  view: AppSettingsView | null,
  draft: SettingsFormValues,
): SettingsPatchResult {
  const baseline = view ?? BASELINE_SETTINGS;
  const patch: SettingsPatch = {};
  const changed: string[] = [];

  const action = draft.guardDefaultAction;
  if (
    action !== "allow" &&
    action !== "deny" &&
    action !== "ask"
  ) {
    return {
      ok: false,
      error: 'Guard default action must be "allow", "deny", or "ask".',
    };
  }
  if (action !== baseline.guard_default_action) {
    patch.guard_default_action = action;
    changed.push("guard_default_action");
  }

  const timeout = parseIntOrNull(draft.approvalTimeout);
  if (draft.approvalTimeout.trim() !== "" && timeout === null) {
    return { ok: false, error: "Approval timeout must be a whole number of seconds." };
  }
  if (timeout !== null) {
    if (timeout < 1) {
      return {
        ok: false,
        error: "Approval timeout must be at least 1 second.",
      };
    }
    if (timeout !== baseline.approval_timeout_secs) {
      patch.approval_timeout_secs = timeout;
      changed.push("approval_timeout_secs");
    }
  }

  return { ok: true, patch, changed };
}

/**
 * True when a settings view is the unseeded shape (both role pointers null).
 * Hostile shapes conservatively answer false (treated as seeded).
 */
export function isUnseededSettings(view: AppSettingsView | null): boolean {
  return (
    view !== null &&
    typeof view === "object" &&
    view.planner_profile === null &&
    view.worker_profile === null
  );
}

// ---------------------------------------------------------------------------
// Failure mapping (server truth renders verbatim; we only route it)
// ---------------------------------------------------------------------------

/**
 * Maps a failed settings write onto display copy. The known zero-profile
 * `404 "app settings are not initialized"` becomes the friendly
 * {@link UNSEEDED_SETTINGS_HINT} (a hint box, not a scary error); everything
 * else passes the server message through untouched — client validation never
 * substitutes for server truth.
 */
export function explainSettingsWriteFailure(err: unknown): {
  kind: "uninitialized" | "message";
  text: string;
} {
  if (
    err instanceof ApiError &&
    err.isNotFound() &&
    err.message.includes("not initialized")
  ) {
    return { kind: "uninitialized", text: UNSEEDED_SETTINGS_HINT };
  }
  if (err instanceof ApiError && err.isValidation()) {
    return { kind: "message", text: `Invalid request: ${err.message}` };
  }
  if (err instanceof ApiError) {
    return { kind: "message", text: `${err.message} (HTTP ${err.status})` };
  }
  if (err instanceof Error) {
    return { kind: "message", text: err.message };
  }
  return { kind: "message", text: String(err) };
}

/**
 * Display copy for a failed profile delete. The `409` messages name the
 * binding (role default or task pin, `Store::delete_profile`) and pass
 * through verbatim behind a short instructional lead; `404` asks for a list
 * refresh (the profile is already gone). Other failures pass through.
 */
export function explainProfileDeleteFailure(err: unknown): string {
  if (err instanceof ApiError && err.isConflict()) {
    return `Cannot delete — ${err.message}`;
  }
  if (err instanceof ApiError && err.isNotFound()) {
    return "Profile no longer exists — list refreshed.";
  }
  if (err instanceof ApiError && err.isValidation()) {
    return `Invalid request: ${err.message}`;
  }
  if (err instanceof ApiError) {
    return `${err.message} (HTTP ${err.status})`;
  }
  if (err instanceof Error) {
    return err.message;
  }
  return String(err);
}

/**
 * Display copy for a failed model fetch. The `502` body already carries a
 * REDACTED upstream snippet (the server scrubs the bearer key before
 * truncation, `redact`) and is displayed AS-IS — display-only, no
 * client-side rewriting that could re-leak or mangle it.
 */
export function modelFetchErrorText(err: unknown): string {
  if (err instanceof ApiError) {
    return err.message;
  }
  if (err instanceof Error) {
    return err.message;
  }
  return String(err);
}
