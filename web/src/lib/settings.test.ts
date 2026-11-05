import assert from "node:assert/strict";
import { afterEach, test } from "node:test";
import { ApiError, deleteProfile, getSettings, listModels, updateSettings } from "./api.ts";
import type { AppSettingsView, ChainEntry, WebProvidersResponse } from "./types.ts";
import {
  BASELINE_SETTINGS,
  MODELS_PICKER_SAVE_FIRST_HINT,
  REASONING_EFFORT_VALUES,
  THINKING_TYPE_VALUES,
  UNSEEDED_SETTINGS_HINT,
  buildProfilePayload,
  buildRoleAssignmentPatch,
  buildRulePayload,
  buildSettingsPatch,
  canFetchModels,
  chainEntryChips,
  countTrippedEntries,
  describeApiKeyHint,
  describeWebProviderKey,
  buildWebProviderUpdate,
  countReadyWebProviders,
  explainProfileDeleteFailure,
  explainSettingsWriteFailure,
  isReasoningEffort,
  isThinkingType,
  isUnseededSettings,
  modelFetchErrorText,
  parseIntOrNull,
  reasoningEffortLabel,
  ruleActionTone,
  ruleMatchDescription,
  ruleReasonOrAuto,
  splitChain,
  thinkingTypeLabel,
  toModelOptions,
  validateProfileForm,
  validateReasoningEffort,
  validateRuleForm,
  validateThinkingType,
  validateWebProviderForm,
  webProviderDefaultBaseUrl,
  webProviderOrderText,
  webProviderSourceLabel,
  webProviderStatusLabel,
  webProviderTestNote,
  type ProfileFormValues,
  type RoleAssignmentFormValues,
  type RuleFormValues,
  type SettingsFormValues,
  type WebProviderFormValues,
} from "./settings.ts";

const originalFetch = globalThis.fetch;

afterEach(() => {
  globalThis.fetch = originalFetch;
});

/** Stubs `globalThis.fetch` for the api-client tests below. */
function stubFetch(respond: (url: string) => Response): { urls: string[] } {
  const recorder = { urls: [] as string[] };
  globalThis.fetch = (async (input: RequestInfo | URL) => {
    const url = String(input);
    recorder.urls.push(url);
    return respond(url);
  }) as typeof fetch;
  return recorder;
}

// ---------------------------------------------------------------------------
// Factories
// ---------------------------------------------------------------------------

function profileForm(overrides: Partial<ProfileFormValues> = {}): ProfileFormValues {
  return {
    name: "backup",
    format: "openai",
    baseUrl: "https://api.backup.ai/v1",
    model: "river-xl",
    apiKey: "",
    maxTokens: "0",
    contextWindowK: "128",
    thinkingType: "",
    reasoningEffort: "",
    streaming: true,
    priority: "100",
    poolExclude: false,
    ...overrides,
  };
}

function ruleForm(overrides: Partial<RuleFormValues> = {}): RuleFormValues {
  return {
    name: "block-rm",
    matchTarget: "tool_name",
    matchType: "exact",
    pattern: "rm",
    action: "deny",
    reason: "no deletions",
    priority: "700",
    enabled: true,
    timeoutSecs: "0",
    ...overrides,
  };
}

function settingsForm(overrides: Partial<SettingsFormValues> = {}): SettingsFormValues {
  return {
    approvalTimeout: "900",
    guardDefaultAction: "deny",
    ...overrides,
  };
}

function roleAssignmentForm(
  overrides: Partial<RoleAssignmentFormValues> = {},
): RoleAssignmentFormValues {
  return {
    plannerChoice: "",
    workerChoice: "",
    ...overrides,
  };
}

const SEEDED_VIEW: AppSettingsView = {
  planner_profile: { id: 1, name: "planner-default", model: "river-planner" },
  worker_profile: { id: 2, name: "worker-default", model: "river-worker" },
  llm_key_set: true,
  planner_api_key_hint: "…9999",
  worker_api_key_hint: "…9999",
  approval_timeout_secs: 900,
  guard_default_action: "deny",
};

// ---------------------------------------------------------------------------
// parseIntOrNull
// ---------------------------------------------------------------------------

test("parseIntOrNull accepts integers and rejects everything else", () => {
  assert.equal(parseIntOrNull(" 42 "), 42);
  assert.equal(parseIntOrNull("-7"), -7);
  assert.equal(parseIntOrNull("+3"), 3);
  assert.equal(parseIntOrNull(""), null);
  assert.equal(parseIntOrNull("1.5"), null);
  assert.equal(parseIntOrNull("1e3"), null);
  assert.equal(parseIntOrNull("abc"), null);
});

// ---------------------------------------------------------------------------
// Key-hint display
// ---------------------------------------------------------------------------

test("describeApiKeyHint passes the hint through and explains an empty one", () => {
  assert.equal(describeApiKeyHint("…9999"), "…9999");
  const empty = describeApiKeyHint("");
  assert.match(empty, /no hint/);
  assert.match(empty, /shorter than 4 chars/);
  // Hostile shapes degrade, never throw.
  assert.equal(describeApiKeyHint(undefined), describeApiKeyHint(""));
  assert.equal(describeApiKeyHint(42), describeApiKeyHint(""));
});

// ---------------------------------------------------------------------------
// Profile form: validation mirrors the server's 400 rules
// ---------------------------------------------------------------------------

test("validateProfileForm blocks an empty name (server 400)", () => {
  assert.match(validateProfileForm(profileForm({ name: "   " }))!, /Name is required/);
});

test("validateProfileForm blocks an empty model (server 400)", () => {
  assert.match(validateProfileForm(profileForm({ model: "" }))!, /Model is required/);
});

test("validateProfileForm blocks a bad format enum (server 400)", () => {
  const bad = profileForm({ format: "vertex" as ProfileFormValues["format"] });
  assert.match(validateProfileForm(bad)!, /openai or anthropic/);
});

test("validateProfileForm blocks negative max_tokens with the clamp explained", () => {
  assert.match(validateProfileForm(profileForm({ maxTokens: "-5" }))!, /0 or more/);
});

test("validateProfileForm blocks non-whole numerics", () => {
  assert.match(validateProfileForm(profileForm({ contextWindowK: "12.5" }))!, /whole number/);
  assert.match(validateProfileForm(profileForm({ priority: "many" }))!, /whole number/);
});

test("validateProfileForm accepts a fully valid form", () => {
  assert.equal(validateProfileForm(profileForm()), null);
  assert.equal(validateProfileForm(profileForm({ maxTokens: "", priority: "" })), null);
});

// ---------------------------------------------------------------------------
// Thinking controls
// ---------------------------------------------------------------------------

test("thinking and reasoning labels cover every wire value exactly", () => {
  assert.deepEqual(THINKING_TYPE_VALUES.map(thinkingTypeLabel), [
    "Not sent",
    "Disabled",
    "Enabled",
  ]);
  assert.deepEqual(REASONING_EFFORT_VALUES.map(reasoningEffortLabel), [
    "Not sent",
    "Low",
    "Medium",
    "High",
    "XHigh",
    "Max",
  ]);
  // Hostile/missing values degrade without throwing.
  assert.equal(thinkingTypeLabel(undefined), "Not sent");
  assert.equal(reasoningEffortLabel(null), "Not sent");
  assert.equal(reasoningEffortLabel("turbo"), "turbo");
});

test("thinking value validators accept only the documented enums", () => {
  for (const value of THINKING_TYPE_VALUES) {
    assert.equal(isThinkingType(value), true);
    assert.equal(validateThinkingType(value), null);
  }
  for (const value of REASONING_EFFORT_VALUES) {
    assert.equal(isReasoningEffort(value), true);
    assert.equal(validateReasoningEffort(value), null);
  }
  assert.equal(isThinkingType("auto"), false);
  assert.equal(isThinkingType(null), false);
  assert.match(validateThinkingType("auto")!, /Thinking must be/);
  assert.equal(isReasoningEffort("ultra"), false);
  assert.equal(isReasoningEffort(5), false);
  assert.match(validateReasoningEffort("ultra")!, /Reasoning effort must be/);
});

test("validateProfileForm rejects out-of-contract thinking controls", () => {
  const badThinking = profileForm({
    thinkingType: "auto" as ProfileFormValues["thinkingType"],
  });
  assert.match(validateProfileForm(badThinking)!, /Thinking must be/);
  const badEffort = profileForm({
    reasoningEffort: "ultra" as ProfileFormValues["reasoningEffort"],
  });
  assert.match(validateProfileForm(badEffort)!, /Reasoning effort must be/);
});

test("buildProfilePayload always sends both thinking controls independently", () => {
  const off = buildProfilePayload(profileForm(), null);
  assert.ok(off.ok);
  if (off.ok) {
    // Empty is the explicit "not sent" state; clearing must reach the server.
    assert.equal(off.input.thinking_type, "");
    assert.equal(off.input.reasoning_effort, "");
  }

  const mixed = buildProfilePayload(
    profileForm({ thinkingType: "enabled", reasoningEffort: "xhigh" }),
    7,
  );
  assert.ok(mixed.ok);
  if (mixed.ok) {
    assert.equal(mixed.input.thinking_type, "enabled");
    assert.equal(mixed.input.reasoning_effort, "xhigh");
  }

  const thinkingOnly = buildProfilePayload(
    profileForm({ thinkingType: "disabled", reasoningEffort: "" }),
    7,
  );
  assert.ok(thinkingOnly.ok);
  if (thinkingOnly.ok) {
    // The controls are independent: thinking never drags effort along.
    assert.equal(thinkingOnly.input.thinking_type, "disabled");
    assert.equal(thinkingOnly.input.reasoning_effort, "");
  }
});

// ---------------------------------------------------------------------------
// Profile payload: id-0-as-create, key hygiene
// ---------------------------------------------------------------------------

test("buildProfilePayload omits the id when creating (id 0 normalizes to create)", () => {
  const created = buildProfilePayload(profileForm(), null);
  assert.ok(created.ok);
  if (created.ok) {
    assert.equal("id" in created.input, false);
  }
  // `"id": 0` is create per ARTEX (`Store::save_profile` accepts only
  // id > 0): the payload
  // carries NO id, which is the api.ts saveProfile create convention.
  const zeroId = buildProfilePayload(profileForm(), 0);
  assert.ok(zeroId.ok);
  if (zeroId.ok) {
    assert.equal("id" in zeroId.input, false);
  }
});

test("buildProfilePayload sends a positive id on edit", () => {
  const edited = buildProfilePayload(profileForm(), 7);
  assert.ok(edited.ok);
  if (edited.ok) {
    assert.equal(edited.input.id, 7);
  }
});

test("buildProfilePayload omits a blank api key and trims a typed one", () => {
  const blank = buildProfilePayload(profileForm({ apiKey: "   " }), 7);
  assert.ok(blank.ok);
  if (blank.ok) {
    // Blank on edit = keep the stored key: the field is ABSENT, never an
    // empty string, and never echoed anywhere else in the payload.
    assert.equal("api_key" in blank.input, false);
  }
  const typed = buildProfilePayload(profileForm({ apiKey: "  sk-live-1  " }), 7);
  assert.ok(typed.ok);
  if (typed.ok) {
    assert.equal(typed.input.api_key, "sk-live-1");
  }
});

test("buildProfilePayload trims fields and includes numerics + booleans", () => {
  const built = buildProfilePayload(
    profileForm({ name: " x ", baseUrl: " https://a.i ", model: " m " }),
    null,
  );
  assert.ok(built.ok);
  if (built.ok) {
    assert.equal(built.input.name, "x");
    assert.equal(built.input.base_url, "https://a.i");
    assert.equal(built.input.model, "m");
    assert.deepEqual(
      [
        built.input.max_tokens,
        built.input.context_window_k,
        built.input.priority,
        built.input.streaming,
        built.input.pool_exclude,
      ],
      [0, 128, 100, true, false],
    );
  }
});

test("buildProfilePayload surfaces the validation error instead of a payload", () => {
  const bad = buildProfilePayload(profileForm({ model: "" }), null);
  assert.equal(bad.ok, false);
});

test("models picker is edit-time only (unsaved profiles have no stored key)", () => {
  assert.equal(canFetchModels(null), false);
  assert.equal(canFetchModels(0), false);
  assert.equal(canFetchModels(3), true);
  assert.match(MODELS_PICKER_SAVE_FIRST_HINT, /Save the profile first/);
});

// ---------------------------------------------------------------------------
// Model options shaping
// ---------------------------------------------------------------------------

test("toModelOptions trims, dedupes, keeps upstream order, and caps at 200", () => {
  assert.deepEqual(toModelOptions([" b", "a", "b", "a ", "", 5, null]), ["b", "a"]);
  const big = Array.from({ length: 250 }, (_, i) => `m${i}`);
  assert.equal(toModelOptions(big).length, 200);
  assert.equal(toModelOptions("not-an-array").length, 0);
});

// ---------------------------------------------------------------------------
// Pool chain helpers
// ---------------------------------------------------------------------------

const ENTRY_A = { id: 1, name: "a", model: "ma", tripped: false, consecutive_failures: 0 };
const ENTRY_B = { id: 2, name: "b", model: "mb", tripped: true, consecutive_failures: 3 };
const ENTRY_C = { id: 3, name: "c", model: "mc", tripped: false, consecutive_failures: 1 };

test("chainEntryChips maps tripped/failure/healthy tones", () => {
  assert.deepEqual(chainEntryChips(ENTRY_A), [{ text: "healthy", tone: "ok" }]);
  assert.deepEqual(chainEntryChips(ENTRY_C), [
    { text: "1 consecutive failure", tone: "warn" },
  ]);
  const tripped = chainEntryChips(ENTRY_B);
  assert.equal(tripped[0]?.text, "tripped");
  assert.equal(tripped[0]?.tone, "danger");
  // Counter still shown while tripped.
  assert.equal(tripped[1]?.text, "3 consecutive failures");
});

test("chainEntryChips is hostile-shape-safe", () => {
  assert.deepEqual(chainEntryChips(null), [{ text: "healthy", tone: "ok" }]);
  assert.deepEqual(chainEntryChips({ tripped: "yes", consecutive_failures: NaN }), [
    { text: "healthy", tone: "ok" },
  ]);
});

test("splitChain partitions preserving order and ignores junk", () => {
  const split = splitChain([
    ENTRY_A,
    ENTRY_B,
    ENTRY_C,
    null,
    "junk",
  ] as unknown as ChainEntry[]);
  assert.deepEqual(split.healthy.map((entry) => entry.id), [1, 3]);
  assert.deepEqual(split.tripped.map((entry) => entry.id), [2]);
  assert.deepEqual(splitChain(null), { healthy: [], tripped: [] });
});

test("countTrippedEntries totals both role chains", () => {
  const status = {
    planner: { chain: [ENTRY_A, ENTRY_B] },
    worker: { chain: [ENTRY_C] },
  };
  assert.equal(countTrippedEntries(status), 1);
  assert.equal(countTrippedEntries({ planner: { chain: [] }, worker: { chain: [] } }), 0);
  assert.equal(countTrippedEntries(null), 0);
});

// ---------------------------------------------------------------------------
// Rule form: validation + payload
// ---------------------------------------------------------------------------

test("validateRuleForm catches a bad regex BEFORE the server 400", () => {
  const bad = ruleForm({ matchType: "regex", pattern: "[unclosed" });
  const error = validateRuleForm(bad);
  assert.match(error!, /Invalid regex pattern/);
});

test("validateRuleForm skips the regex check for exact matching", () => {
  // "[" is a poor regex but a perfectly fine exact pattern.
  assert.equal(validateRuleForm(ruleForm({ matchType: "exact", pattern: "[" })), null);
});

test("validateRuleForm blocks empty name/pattern and bad enums", () => {
  assert.match(validateRuleForm(ruleForm({ name: "" }))!, /Name is required/);
  assert.match(validateRuleForm(ruleForm({ pattern: "  " }))!, /Pattern is required/);
  assert.match(
    validateRuleForm(ruleForm({ action: "maybe" as RuleFormValues["action"] }))!,
    /allow.*deny.*ask|Action/,
  );
  assert.match(
    validateRuleForm(ruleForm({ matchTarget: "url" as RuleFormValues["matchTarget"] }))!,
    /tool_name|Match target/,
  );
});

test("validateRuleForm blocks a negative timeout (server 400)", () => {
  assert.match(validateRuleForm(ruleForm({ timeoutSecs: "-1" }))!, /0 or more/);
});

test("buildRulePayload trims fields and keeps an empty reason (server auto-texts)", () => {
  const built = buildRulePayload(ruleForm({ reason: "  ", pattern: " rm " }));
  assert.ok(built.ok);
  if (built.ok) {
    assert.equal(built.input.reason, "");
    assert.equal(built.input.pattern, "rm");
    assert.equal(built.input.timeout_secs, 0);
    assert.equal(built.input.priority, 700);
  }
  assert.equal(buildRulePayload(ruleForm({ name: "" })).ok, false);
});

test("rule display helpers: tone, match description, auto reason", () => {
  assert.equal(ruleActionTone("deny"), "deny");
  assert.equal(ruleActionTone("allow"), "allow");
  assert.equal(ruleActionTone("ask"), "ask");
  assert.equal(ruleActionTone("zap"), "unknown");
  assert.match(
    ruleMatchDescription("tool_input", "exact"),
    /case-insensitive/,
  );
  assert.equal(ruleMatchDescription("tool_name", "exact"), "tool name equals the pattern");
  assert.equal(ruleReasonOrAuto("block-rm", ""), "matched rule block-rm");
  assert.equal(ruleReasonOrAuto("block-rm", "custom"), "custom");
});

// ---------------------------------------------------------------------------
// Settings partial patch: only-changed-fields semantics
// ---------------------------------------------------------------------------

test("buildSettingsPatch emits nothing when the draft matches the view", () => {
  const result = buildSettingsPatch(SEEDED_VIEW, settingsForm());
  assert.ok(result.ok);
  if (result.ok) {
    assert.deepEqual(result.patch, {});
    assert.deepEqual(result.changed, []);
  }
});

test("buildSettingsPatch includes ONLY changed general fields", () => {
  const timeoutOnly = buildSettingsPatch(
    SEEDED_VIEW,
    settingsForm({ approvalTimeout: "600" }),
  );
  assert.ok(timeoutOnly.ok);
  if (timeoutOnly.ok) {
    assert.deepEqual(timeoutOnly.patch, { approval_timeout_secs: 600 });
    assert.deepEqual(timeoutOnly.changed, ["approval_timeout_secs"]);
  }

  const actionOnly = buildSettingsPatch(
    SEEDED_VIEW,
    settingsForm({ guardDefaultAction: "ask" }),
  );
  assert.ok(actionOnly.ok);
  if (actionOnly.ok) {
    assert.deepEqual(actionOnly.patch, { guard_default_action: "ask" });
    assert.deepEqual(actionOnly.changed, ["guard_default_action"]);
  }
});

test("buildSettingsPatch never carries role selectors — they live in RoleAssignmentSection", () => {
  const result = buildSettingsPatch(
    SEEDED_VIEW,
    settingsForm({ approvalTimeout: "300", guardDefaultAction: "allow" }),
  );
  assert.ok(result.ok);
  if (result.ok) {
    assert.equal("planner_profile_id" in result.patch, false);
    assert.equal("worker_profile_id" in result.patch, false);
    for (const value of Object.values(result.patch)) {
      assert.notEqual(value === null || value === undefined, true);
    }
  }
});

test("buildSettingsPatch treats an unseeded view like the server's unseeded defaults", () => {
  assert.equal(isUnseededSettings(BASELINE_SETTINGS), true);
  const unseededServerShape = {
    planner_profile: null,
    worker_profile: null,
    llm_key_set: false,
    planner_api_key_hint: "",
    worker_api_key_hint: "",
    approval_timeout_secs: 900,
    guard_default_action: "deny",
  } as AppSettingsView;
  // Passthrough of the documented unseeded GET shape (always 200).
  assert.equal(isUnseededSettings(unseededServerShape), true);
  const draft = settingsForm({ approvalTimeout: "600" });
  const againstNull = buildSettingsPatch(null, draft);
  const againstUnseeded = buildSettingsPatch(unseededServerShape, draft);
  assert.deepEqual(
    againstNull.ok && againstUnseeded.ok ? [againstNull.patch, againstUnseeded.patch] : [],
    [{ approval_timeout_secs: 600 }, { approval_timeout_secs: 600 }],
  );
});

test("isUnseededSettings answers false for seeded and hostile shapes", () => {
  assert.equal(isUnseededSettings(SEEDED_VIEW), false);
  assert.equal(isUnseededSettings(null), false);
  // A half-seeded shape (one role only) counts as seeded.
  assert.equal(
    isUnseededSettings({ ...BASELINE_SETTINGS, planner_profile: { id: 1, name: "p", model: "m" } }),
    false,
  );
});

test("buildSettingsPatch validates timeout >= 1 and the action enum (mirrors server 400s)", () => {
  const low = buildSettingsPatch(
    SEEDED_VIEW,
    settingsForm({ approvalTimeout: "0" }),
  );
  assert.equal(low.ok, false);
  if (!low.ok) {
    assert.match(low.error, /at least 1/);
  }
  const badAction = buildSettingsPatch(SEEDED_VIEW, {
    ...settingsForm(),
    guardDefaultAction: "maybe" as SettingsFormValues["guardDefaultAction"],
  });
  assert.equal(badAction.ok, false);
  if (!badAction.ok) {
    assert.match(badAction.error, /allow.*deny.*ask|Guard default/);
  }
});

// ---------------------------------------------------------------------------
// Role assignment partial patch: one request for both roles when needed
// ---------------------------------------------------------------------------

test("buildRoleAssignmentPatch emits both roles when both changed (same profile allowed)", () => {
  const result = buildRoleAssignmentPatch(
    SEEDED_VIEW,
    roleAssignmentForm({ plannerChoice: "3", workerChoice: "3" }),
  );
  assert.ok(result.ok);
  if (result.ok) {
    assert.deepEqual(result.patch, {
      planner_profile_id: 3,
      worker_profile_id: 3,
    });
    assert.deepEqual(result.changed, [
      "planner_profile_id",
      "worker_profile_id",
    ]);
  }
});

test("buildRoleAssignmentPatch emits both roles when each picks a different profile", () => {
  const result = buildRoleAssignmentPatch(
    SEEDED_VIEW,
    roleAssignmentForm({ plannerChoice: "3", workerChoice: "4" }),
  );
  assert.ok(result.ok);
  if (result.ok) {
    assert.deepEqual(result.patch, {
      planner_profile_id: 3,
      worker_profile_id: 4,
    });
  }
});

test("buildRoleAssignmentPatch sends ONLY the role that changed", () => {
  const plannerOnly = buildRoleAssignmentPatch(
    SEEDED_VIEW,
    roleAssignmentForm({ plannerChoice: "3", workerChoice: "2" }),
  );
  assert.ok(plannerOnly.ok);
  if (plannerOnly.ok) {
    assert.deepEqual(plannerOnly.patch, { planner_profile_id: 3 });
    assert.deepEqual(plannerOnly.changed, ["planner_profile_id"]);
  }

  const workerOnly = buildRoleAssignmentPatch(
    SEEDED_VIEW,
    roleAssignmentForm({ plannerChoice: "1", workerChoice: "4" }),
  );
  assert.ok(workerOnly.ok);
  if (workerOnly.ok) {
    assert.deepEqual(workerOnly.patch, { worker_profile_id: 4 });
    assert.deepEqual(workerOnly.changed, ["worker_profile_id"]);
  }
});

test("buildRoleAssignmentPatch emits nothing when both choices match the view", () => {
  const result = buildRoleAssignmentPatch(
    SEEDED_VIEW,
    roleAssignmentForm({ plannerChoice: "1", workerChoice: "2" }),
  );
  assert.ok(result.ok);
  if (result.ok) {
    assert.deepEqual(result.patch, {});
    assert.deepEqual(result.changed, []);
  }
});

test("buildRoleAssignmentPatch treats empty selects as omission and an unseeded view as null roles", () => {
  const unchanged = buildRoleAssignmentPatch(SEEDED_VIEW, roleAssignmentForm());
  assert.ok(unchanged.ok);
  if (unchanged.ok) {
    assert.deepEqual(unchanged.patch, {});
  }

  const seeded = buildRoleAssignmentPatch(
    BASELINE_SETTINGS,
    roleAssignmentForm({ plannerChoice: "1", workerChoice: "2" }),
  );
  assert.ok(seeded.ok);
  if (seeded.ok) {
    assert.deepEqual(seeded.patch, {
      planner_profile_id: 1,
      worker_profile_id: 2,
    });
  }
  for (const value of Object.values(seeded.ok ? seeded.patch : {})) {
    assert.notEqual(value === null || value === undefined, true);
  }
});

test("buildRoleAssignmentPatch rejects malformed profile selections", () => {
  const badPlanner = buildRoleAssignmentPatch(
    SEEDED_VIEW,
    roleAssignmentForm({ plannerChoice: "three" }),
  );
  assert.equal(badPlanner.ok, false);
  if (!badPlanner.ok) {
    assert.match(badPlanner.error, /Planner/);
  }
  const badWorker = buildRoleAssignmentPatch(
    SEEDED_VIEW,
    roleAssignmentForm({ workerChoice: "1.5" }),
  );
  assert.equal(badWorker.ok, false);
  if (!badWorker.ok) {
    assert.match(badWorker.error, /Worker/);
  }
});

// ---------------------------------------------------------------------------
// Failure mapping (API stubs: server truth renders verbatim)
// ---------------------------------------------------------------------------

test("stubbed profile DELETE 409 maps to the unbind-first explanation", async () => {
  stubFetch(() =>
    new Response(
      JSON.stringify({
        error: "profile is the active default for a role; activate another profile first",
      }),
      { status: 409 },
    ),
  );
  let caught: unknown = null;
  try {
    await deleteProfile(1);
  } catch (err) {
    caught = err;
  }
  assert.ok(caught instanceof ApiError && caught.isConflict());
  assert.match(explainProfileDeleteFailure(caught), /^Cannot delete — profile is the active default/);
});

test("stubbed profile DELETE 404 asks for a list refresh", async () => {
  stubFetch(() =>
    new Response(JSON.stringify({ error: "llm profile 9" }), { status: 404 }),
  );
  let caught: unknown = null;
  try {
    await deleteProfile(9);
  } catch (err) {
    caught = err;
  }
  assert.ok(caught instanceof ApiError && caught.isNotFound());
  assert.match(explainProfileDeleteFailure(caught), /no longer exists/i);
});

test("stubbed GET /api/settings serves the unseeded 200 shape and it passes through", async () => {
  stubFetch(() =>
    new Response(
      JSON.stringify({
        planner_profile: null,
        worker_profile: null,
        llm_key_set: false,
        planner_api_key_hint: "",
        worker_api_key_hint: "",
        approval_timeout_secs: 900,
        guard_default_action: "deny",
      }),
      { status: 200 },
    ),
  );
  const view = await getSettings();
  assert.equal(isUnseededSettings(view), true);
  assert.equal(view.approval_timeout_secs, 900);
  assert.equal(view.guard_default_action, "deny");
  assert.equal(view.llm_key_set, false);
});

test("stubbed settings PUT 404 maps to the not-initialized hint, not a scary error", async () => {
  stubFetch(() =>
    new Response(
      JSON.stringify({ error: "app settings are not initialized" }),
      { status: 404 },
    ),
  );
  let caught: unknown = null;
  try {
    await updateSettings({ approval_timeout_secs: 600 });
  } catch (err) {
    caught = err;
  }
  const mapped = explainSettingsWriteFailure(caught);
  assert.equal(mapped.kind, "uninitialized");
  assert.equal(mapped.text, UNSEEDED_SETTINGS_HINT);
});

test("settings PUT 400s render the server message verbatim", () => {
  const mapped = explainSettingsWriteFailure(
    new ApiError(400, "approval_timeout_secs must be at least 1"),
  );
  assert.equal(mapped.kind, "message");
  assert.equal(mapped.text, "Invalid request: approval_timeout_secs must be at least 1");
});

test("stubbed models 502 carries the redacted snippet and displays it AS-IS", async () => {
  const snippet =
    'model list returned 502 Bad Gateway: {"error":{"message":"upstream boom key [redacted]","type":"gateway_error"}}';
  stubFetch(() => new Response(JSON.stringify({ error: snippet }), { status: 502 }));
  let caught: unknown = null;
  try {
    await listModels(1);
  } catch (err) {
    caught = err;
  }
  assert.ok(caught instanceof ApiError);
  assert.equal(caught.status, 502);
  // Display-only: the text survives byte-for-byte, [redacted] intact, no
  // client-side rewriting.
  assert.equal(modelFetchErrorText(caught), snippet);
  assert.match(modelFetchErrorText(caught), /\[redacted\]/);
});

// ---------------------------------------------------------------------------
// Web search provider card helpers (Settings v2)
// ---------------------------------------------------------------------------

function webProviderForm(
  overrides: Partial<WebProviderFormValues> = {},
): WebProviderFormValues {
  return {
    apiKey: "",
    baseUrl: "",
    timeoutSecs: "60",
    enabled: true,
    ...overrides,
  };
}

const WEB_PROVIDERS_VIEW: WebProvidersResponse = {
  search_read_order: ["tinyfish", "parallel", "firecrawl"],
  research_order: ["parallel"],
  providers: [
    {
      provider: "tinyfish",
      display_name: "TinyFish",
      configured: true,
      key_hint: "…1a2b",
      source: "database",
      base_url: null,
      timeout_secs: 60,
      enabled: true,
      status: "ready",
    },
    {
      provider: "parallel",
      display_name: "Parallel",
      configured: false,
      key_hint: null,
      source: "none",
      base_url: null,
      timeout_secs: 60,
      enabled: true,
      status: "missing_key",
    },
    {
      provider: "firecrawl",
      display_name: "Firecrawl",
      configured: true,
      key_hint: "…9f0e",
      source: "config_file",
      base_url: "http://127.0.0.1:3002",
      timeout_secs: 90,
      enabled: false,
      status: "disabled",
    },
  ],
};

test("validateWebProviderForm enforces the 1..3600 whole-second range", () => {
  assert.equal(validateWebProviderForm(webProviderForm()), null);
  assert.match(validateWebProviderForm(webProviderForm({ timeoutSecs: "" }))!, /required/);
  assert.match(validateWebProviderForm(webProviderForm({ timeoutSecs: "1.5" }))!, /whole number/);
  assert.match(validateWebProviderForm(webProviderForm({ timeoutSecs: "0" }))!, /between 1 and 3600/);
  assert.match(validateWebProviderForm(webProviderForm({ timeoutSecs: "3601" }))!, /between 1 and 3600/);
});

test("buildWebProviderUpdate omits a blank key and always sends base_url for Parallel/Firecrawl", () => {
  const blank = buildWebProviderUpdate(
    "parallel",
    webProviderForm({ apiKey: "   ", baseUrl: "  " }),
  );
  assert.ok(blank.ok);
  if (blank.ok) {
    // Blank = keep the stored key: the field is ABSENT, never "".
    assert.equal("api_key" in blank.input, false);
    // Empty base URL must be sent to actively clear an override.
    assert.equal(blank.input.base_url, "");
    assert.equal(blank.input.timeout_secs, 60);
    assert.equal(blank.input.enabled, true);
  }

  const typed = buildWebProviderUpdate(
    "firecrawl",
    webProviderForm({ apiKey: " fc-live ", baseUrl: " http://127.0.0.1:3002 " }),
  );
  assert.ok(typed.ok);
  if (typed.ok) {
    assert.equal(typed.input.api_key, "fc-live");
    assert.equal(typed.input.base_url, "http://127.0.0.1:3002");
  }
});

test("buildWebProviderUpdate omits base_url for TinyFish (it has no override)", () => {
  const built = buildWebProviderUpdate(
    "tinyfish",
    webProviderForm({ baseUrl: "https://ignored.example" }),
  );
  assert.ok(built.ok);
  if (built.ok) {
    assert.equal("base_url" in built.input, false);
  }
  const bad = buildWebProviderUpdate("tinyfish", webProviderForm({ timeoutSecs: "3601" }));
  assert.equal(bad.ok, false);
});

test("provider labels map every wire status/source and stay hostile-safe", () => {
  assert.equal(webProviderStatusLabel("ready"), "Ready");
  assert.equal(webProviderStatusLabel("missing_key"), "Missing key");
  assert.equal(webProviderStatusLabel("disabled"), "Disabled");
  assert.equal(webProviderSourceLabel("database"), "Database");
  assert.equal(webProviderSourceLabel("environment"), "Environment");
  assert.equal(webProviderSourceLabel("config_file"), "Config file");
  assert.equal(webProviderSourceLabel("none"), "None");
  // Unknown values stay visible instead of vanishing.
  assert.equal(webProviderStatusLabel("future"), "future");
  assert.equal(webProviderSourceLabel(undefined), "Unknown");
});

test("describeWebProviderKey shows the mask, No key, or No hint", () => {
  assert.equal(
    describeWebProviderKey({ configured: true, key_hint: "…1a2b" }),
    "…1a2b",
  );
  assert.equal(
    describeWebProviderKey({ configured: false, key_hint: null }),
    "No key",
  );
  assert.equal(
    describeWebProviderKey({ configured: true, key_hint: null }),
    "No hint",
  );
});

test("webProviderOrderText is built from the response arrays only", () => {
  assert.equal(
    webProviderOrderText(WEB_PROVIDERS_VIEW),
    "Search/read: TinyFish → Parallel → Firecrawl · Research: Parallel",
  );
  assert.equal(
    webProviderOrderText({
      search_read_order: [],
      research_order: [],
      providers: [],
    }),
    "Search/read: — · Research: —",
  );
  assert.equal(webProviderOrderText(null), "");
});

test("countReadyWebProviders counts ready DTOs; test note formats latency", () => {
  assert.equal(countReadyWebProviders(WEB_PROVIDERS_VIEW), 1);
  assert.equal(countReadyWebProviders(null), 0);
  assert.equal(webProviderTestNote({ latency_ms: 123 }), "Connected in 123 ms");
  assert.equal(
    webProviderTestNote({ latency_ms: Number.NaN }),
    "Connected in 0 ms",
  );
});

test("webProviderDefaultBaseUrl exposes only the editable provider defaults", () => {
  assert.equal(webProviderDefaultBaseUrl("parallel"), "https://api.parallel.ai");
  assert.equal(webProviderDefaultBaseUrl("firecrawl"), "https://api.firecrawl.dev");
  assert.equal(webProviderDefaultBaseUrl("tinyfish"), null);
});
