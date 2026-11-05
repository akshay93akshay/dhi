import assert from "node:assert/strict";
import { test } from "node:test";
import vm from "node:vm";

import {
  LEGACY_SIDEBAR_STORAGE_KEY,
  LEGACY_THEME_STORAGE_KEY,
  PREFERENCES_STORAGE_KEY,
  PREFERENCE_DEFAULTS,
  applyPreferences,
  clearPreferences,
  migratePreferences,
  normalizePreferences,
  parsePreferences,
  persistPreferences,
  preferencesBootstrapScript,
  readPreferencesState,
  resolveThemePreference,
  serializePreferences,
  type PreferenceRoot,
  type PreferenceStorage,
  type Preferences,
} from "./preferences.ts";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

class MemoryStorage implements PreferenceStorage {
  readonly values = new Map<string, string>();

  constructor(initial: Record<string, string> = {}) {
    for (const [key, value] of Object.entries(initial)) {
      this.values.set(key, value);
    }
  }

  getItem(key: string): string | null {
    return this.values.get(key) ?? null;
  }

  setItem(key: string, value: string): void {
    this.values.set(key, value);
  }

  removeItem(key: string): void {
    this.values.delete(key);
  }
}

function fakeRoot(): PreferenceRoot & { classes: Set<string> } {
  const classes = new Set<string>();
  return {
    dataset: {},
    classList: {
      toggle(token: string, force?: boolean): void {
        if (force === undefined) {
          if (classes.has(token)) {
            classes.delete(token);
          } else {
            classes.add(token);
          }
        } else if (force) {
          classes.add(token);
        } else {
          classes.delete(token);
        }
      },
    },
    style: { colorScheme: "" },
    classes,
  };
}

function storedPreferences(
  overrides: Partial<Preferences> = {},
): Preferences {
  return { ...PREFERENCE_DEFAULTS, ...overrides };
}

type BootstrapElement = {
  dataset: Record<string, string | undefined>;
  style: { colorScheme: string };
  classes: Set<string>;
  classList: {
    toggle(token: string, force?: boolean): void;
  };
  setAttribute(name: string, value: string): void;
};

function bootstrapHarness(initial: Record<string, string>) {
  const storage = new MemoryStorage(initial);
  const classes = new Set<string>();
  const element: BootstrapElement = {
    dataset: {},
    style: { colorScheme: "" },
    classes,
    classList: {
      toggle(token: string, force?: boolean): void {
        if (force === undefined) {
          if (classes.has(token)) {
            classes.delete(token);
          } else {
            classes.add(token);
          }
        } else if (force) {
          classes.add(token);
        } else {
          classes.delete(token);
        }
      },
    },
    setAttribute(name: string, value: string): void {
      const key = name
        .replace(/^data-/, "")
        .replace(/-([a-z])/g, (_match, letter: string) => letter.toUpperCase());
      element.dataset[key] = value;
    },
  };

  const window = {
    localStorage: storage,
    matchMedia: (query: string) => ({
      matches: query.includes("dark"),
    }),
  };
  const document = { documentElement: element };
  return { storage, element, window, document };
}

function runBootstrap(initial: Record<string, string>): BootstrapElement {
  const harness = bootstrapHarness(initial);
  vm.runInNewContext(preferencesBootstrapScript, {
    window: harness.window,
    document: harness.document,
  });
  return harness.element;
}

// ---------------------------------------------------------------------------
// Vocabulary / defaults
// ---------------------------------------------------------------------------

test("defaults preserve the pre-preferences DHI appearance", () => {
  assert.deepEqual(PREFERENCE_DEFAULTS, {
    theme: "light",
    layout: "full-width",
    navbar: "sticky",
    sidebar: "sidebar",
    collapse: "icon",
    density: "comfortable",
    sidebarCollapsed: false,
  });
});

test("resolveThemePreference follows system only for the system choice", () => {
  assert.equal(resolveThemePreference("light", true), "light");
  assert.equal(resolveThemePreference("dark", false), "dark");
  assert.equal(resolveThemePreference("system", true), "dark");
  assert.equal(resolveThemePreference("system", false), "light");
});

// ---------------------------------------------------------------------------
// Parsing / validation
// ---------------------------------------------------------------------------

test("normalizePreferences keeps valid fields and drops everything else", () => {
  assert.deepEqual(
    normalizePreferences({
      theme: "dark",
      layout: "centered",
      navbar: "scroll",
      sidebar: "floating",
      collapse: "offcanvas",
      density: "compact",
      sidebarCollapsed: true,
      extra: "ignored",
    }),
    {
      theme: "dark",
      layout: "centered",
      navbar: "sticky",
      sidebar: "floating",
      collapse: "offcanvas",
      density: "compact",
      sidebarCollapsed: true,
    },
  );

  assert.deepEqual(
    normalizePreferences({
      theme: "neon",
      layout: "wide",
      navbar: 3,
      sidebar: null,
      collapse: "sidebar",
      density: "cozy",
      sidebarCollapsed: "true",
    }),
    {},
  );
});

test("normalizePreferences retires the navbar scroll choice", () => {
  assert.equal(normalizePreferences({ navbar: "scroll" }).navbar, "sticky");
  assert.equal(normalizePreferences({ navbar: "sticky" }).navbar, "sticky");
  assert.deepEqual(normalizePreferences({ navbar: "sometimes" }), {});
});

test("normalizePreferences ignores non-object JSON values", () => {
  assert.deepEqual(normalizePreferences(null), {});
  assert.deepEqual(normalizePreferences([]), {});
  assert.deepEqual(normalizePreferences("dark"), {});
  assert.deepEqual(normalizePreferences(42), {});
});

test("parsePreferences tolerates missing, empty, and corrupt raw values", () => {
  assert.deepEqual(parsePreferences(null), {});
  assert.deepEqual(parsePreferences(""), {});
  assert.deepEqual(parsePreferences("{not json"), {});
  assert.deepEqual(parsePreferences('{"theme":"dark"}'), { theme: "dark" });
});

test("serializePreferences writes one stable record", () => {
  assert.equal(
    serializePreferences(
      storedPreferences({
        theme: "dark",
        layout: "centered",
        navbar: "sticky",
        sidebar: "floating",
        collapse: "offcanvas",
        density: "compact",
        sidebarCollapsed: true,
      }),
    ),
    '{"theme":"dark","layout":"centered","navbar":"sticky","sidebar":"floating","collapse":"offcanvas","density":"compact","sidebarCollapsed":true}',
  );
});

// ---------------------------------------------------------------------------
// Reading + migration
// ---------------------------------------------------------------------------

test("readPreferencesState uses defaults when storage is empty", () => {
  const result = readPreferencesState(new MemoryStorage());
  assert.deepEqual(result.preferences, PREFERENCE_DEFAULTS);
  assert.equal(result.migrated, false);
  assert.equal(result.stored, false);
});

test("readPreferencesState reads a complete new record", () => {
  const preferences = storedPreferences({
    theme: "dark",
    layout: "centered",
    navbar: "sticky",
    sidebar: "inset",
    collapse: "offcanvas",
    density: "compact",
    sidebarCollapsed: true,
  });
  const result = readPreferencesState(
    new MemoryStorage({ [PREFERENCES_STORAGE_KEY]: serializePreferences(preferences) }),
  );
  assert.deepEqual(result.preferences, preferences);
  assert.equal(result.migrated, false);
  assert.equal(result.stored, true);
});

test("readPreferencesState normalizes a stored navbar scroll choice", () => {
  const result = readPreferencesState(
    new MemoryStorage({
      [PREFERENCES_STORAGE_KEY]: JSON.stringify({
        theme: "dark",
        navbar: "scroll",
        sidebar: "floating",
      }),
    }),
  );
  assert.equal(result.preferences.navbar, "sticky");
  assert.equal(result.preferences.theme, "dark");
  assert.equal(result.preferences.sidebar, "floating");
  assert.equal(result.stored, true);
});

test("readPreferencesState migrates dhi-theme and dhi-sidebar-collapsed", () => {
  const result = readPreferencesState(
    new MemoryStorage({
      [LEGACY_THEME_STORAGE_KEY]: "dark",
      [LEGACY_SIDEBAR_STORAGE_KEY]: "true",
    }),
  );
  assert.equal(result.preferences.theme, "dark");
  assert.equal(result.preferences.sidebarCollapsed, true);
  assert.deepEqual(
    { ...result.preferences, theme: PREFERENCE_DEFAULTS.theme, sidebarCollapsed: false },
    PREFERENCE_DEFAULTS,
  );
  assert.equal(result.migrated, true);
  assert.equal(result.stored, false);
});

test("new values win over legacy keys, while missing fields still migrate", () => {
  const result = readPreferencesState(
    new MemoryStorage({
      [PREFERENCES_STORAGE_KEY]: JSON.stringify({
        theme: "light",
        collapse: "offcanvas",
        density: "compact",
      }),
      [LEGACY_THEME_STORAGE_KEY]: "dark",
      [LEGACY_SIDEBAR_STORAGE_KEY]: "true",
    }),
  );
  assert.equal(result.preferences.theme, "light");
  assert.equal(result.preferences.collapse, "offcanvas");
  assert.equal(result.preferences.density, "compact");
  assert.equal(result.preferences.sidebarCollapsed, true);
  assert.equal(result.preferences.layout, PREFERENCE_DEFAULTS.layout);
  assert.equal(result.migrated, true);
  assert.equal(result.stored, true);
});

test("a corrupt new record falls back to valid legacy keys", () => {
  const result = readPreferencesState(
    new MemoryStorage({
      [PREFERENCES_STORAGE_KEY]: "{oops",
      [LEGACY_THEME_STORAGE_KEY]: "dark",
    }),
  );
  assert.equal(result.preferences.theme, "dark");
  assert.equal(result.migrated, true);
  assert.equal(result.stored, false);
});

test("migratePreferences writes the new key and removes the legacy keys", () => {
  const storage = new MemoryStorage({
    [LEGACY_THEME_STORAGE_KEY]: "dark",
    [LEGACY_SIDEBAR_STORAGE_KEY]: "true",
  });
  const preferences = migratePreferences(storage);

  assert.equal(preferences.theme, "dark");
  assert.equal(preferences.sidebarCollapsed, true);
  assert.equal(storage.getItem(LEGACY_THEME_STORAGE_KEY), null);
  assert.equal(storage.getItem(LEGACY_SIDEBAR_STORAGE_KEY), null);
  assert.deepEqual(parsePreferences(storage.getItem(PREFERENCES_STORAGE_KEY)), {
    ...PREFERENCE_DEFAULTS,
    theme: "dark",
    sidebarCollapsed: true,
  });
});

test("persistPreferences round-trips every value", () => {
  const storage = new MemoryStorage();
  const preferences = storedPreferences({
    theme: "system",
    layout: "centered",
    navbar: "sticky",
    sidebar: "floating",
    collapse: "offcanvas",
    density: "compact",
    sidebarCollapsed: true,
  });
  persistPreferences(storage, preferences);
  assert.deepEqual(
    readPreferencesState(storage).preferences,
    preferences,
  );
});

test("clearPreferences removes the new key and any legacy leftovers", () => {
  const storage = new MemoryStorage({
    [PREFERENCES_STORAGE_KEY]: '{"theme":"dark"}',
    [LEGACY_THEME_STORAGE_KEY]: "dark",
    [LEGACY_SIDEBAR_STORAGE_KEY]: "true",
  });
  clearPreferences(storage);
  assert.equal(storage.getItem(PREFERENCES_STORAGE_KEY), null);
  assert.equal(storage.getItem(LEGACY_THEME_STORAGE_KEY), null);
  assert.equal(storage.getItem(LEGACY_SIDEBAR_STORAGE_KEY), null);
  assert.deepEqual(readPreferencesState(storage).preferences, PREFERENCE_DEFAULTS);
});

// ---------------------------------------------------------------------------
// Applying attributes
// ---------------------------------------------------------------------------

test("applyPreferences sets all six attributes plus the dark class", () => {
  const root = fakeRoot();
  applyPreferences(
    storedPreferences({
      theme: "dark",
      layout: "centered",
      navbar: "sticky",
      sidebar: "floating",
      collapse: "offcanvas",
      density: "compact",
      sidebarCollapsed: true,
    }),
    root,
    false,
  );

  assert.equal(root.dataset.theme, "dark");
  assert.equal(root.dataset.layout, "centered");
  assert.equal(root.dataset.navbar, "sticky");
  assert.equal(root.dataset.sidebar, "floating");
  assert.equal(root.dataset.collapse, "offcanvas");
  assert.equal(root.dataset.density, "compact");
  assert.equal(root.dataset.sidebarCollapsed, "true");
  assert.equal(root.classes.has("dark"), true);
  assert.equal(root.style.colorScheme, "dark");
});

test("applyPreferences resolves the system theme for the class only", () => {
  const dark = fakeRoot();
  applyPreferences(storedPreferences({ theme: "system" }), dark, true);
  assert.equal(dark.dataset.theme, "system");
  assert.equal(dark.classes.has("dark"), true);
  assert.equal(dark.style.colorScheme, "dark");

  const light = fakeRoot();
  applyPreferences(storedPreferences({ theme: "system" }), light, false);
  assert.equal(light.dataset.theme, "system");
  assert.equal(light.classes.has("dark"), false);
  assert.equal(light.style.colorScheme, "light");
});

// ---------------------------------------------------------------------------
// Pre-paint bootstrap
// ---------------------------------------------------------------------------

test("bootstrap script names every attribute and storage key", () => {
  for (const fragment of [
    PREFERENCES_STORAGE_KEY,
    LEGACY_THEME_STORAGE_KEY,
    LEGACY_SIDEBAR_STORAGE_KEY,
    "data-theme",
    "data-layout",
    "data-navbar",
    "data-sidebar",
    "data-collapse",
    "data-density",
  ]) {
    assert.ok(
      preferencesBootstrapScript.includes(fragment),
      `missing ${fragment}`,
    );
  }
});

test("bootstrap applies a stored record before paint", () => {
  const element = runBootstrap({
    [PREFERENCES_STORAGE_KEY]: serializePreferences(
      storedPreferences({
        theme: "dark",
        layout: "centered",
        navbar: "sticky",
        sidebar: "inset",
        collapse: "offcanvas",
        density: "compact",
      }),
    ),
  });

  assert.equal(element.dataset.theme, "dark");
  assert.equal(element.dataset.layout, "centered");
  assert.equal(element.dataset.navbar, "sticky");
  assert.equal(element.dataset.sidebar, "inset");
  assert.equal(element.dataset.collapse, "offcanvas");
  assert.equal(element.dataset.density, "compact");
  assert.equal(element.dataset.sidebarCollapsed, "false");
  assert.equal(element.classes.has("dark"), true);
  assert.equal(element.style.colorScheme, "dark");
});

test("bootstrap normalizes a stored navbar scroll choice", () => {
  const element = runBootstrap({
    [PREFERENCES_STORAGE_KEY]: JSON.stringify({ navbar: "scroll" }),
  });
  assert.equal(element.dataset.navbar, "sticky");
});

test("bootstrap reads legacy keys when the new record is absent", () => {
  const element = runBootstrap({
    [LEGACY_THEME_STORAGE_KEY]: "dark",
    [LEGACY_SIDEBAR_STORAGE_KEY]: "true",
  });
  assert.equal(element.dataset.theme, "dark");
  assert.equal(element.dataset.layout, PREFERENCE_DEFAULTS.layout);
  assert.equal(element.dataset.sidebarCollapsed, "true");
  assert.equal(element.classes.has("dark"), true);
});

test("bootstrap rejects junk fields field by field", () => {
  const element = runBootstrap({
    [PREFERENCES_STORAGE_KEY]: JSON.stringify({
      theme: "neon",
      layout: "centered",
      navbar: "sometimes",
      density: "cozy",
    }),
  });
  assert.equal(element.dataset.theme, PREFERENCE_DEFAULTS.theme);
  assert.equal(element.dataset.layout, "centered");
  assert.equal(element.dataset.navbar, PREFERENCE_DEFAULTS.navbar);
  assert.equal(element.dataset.density, PREFERENCE_DEFAULTS.density);
});

test("bootstrap resolves system theme through matchMedia", () => {
  const element = runBootstrap({
    [PREFERENCES_STORAGE_KEY]: JSON.stringify({ theme: "system" }),
  });
  assert.equal(element.dataset.theme, "system");
  assert.equal(element.classes.has("dark"), true);
  assert.equal(element.style.colorScheme, "dark");
});
