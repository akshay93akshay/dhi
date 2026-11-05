/**
 * DHI shell preferences.
 *
 * One small, framework-free module owns the preference vocabulary, the
 * localStorage migration, and the pre-paint attribute application. The React
 * provider (`components/preferences-provider.tsx`) is only a thin subscription
 * over these helpers, so the boot script and the live UI can never disagree.
 *
 * Stored shape (JSON, one key: `dhi-preferences`):
 *   {
 *     theme, layout, navbar, sidebar, collapse, density, sidebarCollapsed
 *   }
 *
 * `navbar` is retained as a single-value ("sticky") storage field so existing
 * records stay readable; the top bar is always sticky and has no control.
 * `sidebarCollapsed` is the runtime state of the existing collapse button; the
 * five option fields are what the Preferences popover edits. Keeping it in the
 * same record lets the old `dhi-sidebar-collapsed` boolean migrate losslessly
 * and lets Restore Defaults reset absolutely everything.
 */

// ---------------------------------------------------------------------------
// Keys and vocabulary
// ---------------------------------------------------------------------------

export const PREFERENCES_STORAGE_KEY = "dhi-preferences";
export const LEGACY_THEME_STORAGE_KEY = "dhi-theme";
export const LEGACY_SIDEBAR_STORAGE_KEY = "dhi-sidebar-collapsed";

export const THEME_OPTIONS = [
  { value: "light", label: "Light" },
  { value: "dark", label: "Dark" },
  { value: "system", label: "System" },
] as const;

export const LAYOUT_OPTIONS = [
  { value: "centered", label: "Centered" },
  { value: "full-width", label: "Full width" },
] as const;

export const NAVBAR_OPTIONS = [
  { value: "sticky", label: "Sticky" },
] as const;

export const SIDEBAR_OPTIONS = [
  { value: "inset", label: "Inset" },
  { value: "sidebar", label: "Sidebar" },
  { value: "floating", label: "Floating" },
] as const;

export const COLLAPSE_OPTIONS = [
  { value: "icon", label: "Icon" },
  { value: "offcanvas", label: "Offcanvas" },
] as const;

export const DENSITY_OPTIONS = [
  { value: "comfortable", label: "Comfortable" },
  { value: "compact", label: "Compact" },
] as const;

export type ThemePreference = (typeof THEME_OPTIONS)[number]["value"];
export type LayoutPreference = (typeof LAYOUT_OPTIONS)[number]["value"];
export type NavbarPreference = (typeof NAVBAR_OPTIONS)[number]["value"];
export type SidebarPreference = (typeof SIDEBAR_OPTIONS)[number]["value"];
export type CollapsePreference = (typeof COLLAPSE_OPTIONS)[number]["value"];
export type DensityPreference = (typeof DENSITY_OPTIONS)[number]["value"];

export type Preferences = {
  theme: ThemePreference;
  layout: LayoutPreference;
  navbar: NavbarPreference;
  sidebar: SidebarPreference;
  collapse: CollapsePreference;
  density: DensityPreference;
  /** Runtime state of the shell's collapse toggle (not an option in the panel). */
  sidebarCollapsed: boolean;
};

export type PreferenceKey = keyof Preferences;

export const PREFERENCE_DEFAULTS: Preferences = {
  theme: "light",
  layout: "full-width",
  navbar: "sticky",
  sidebar: "sidebar",
  collapse: "icon",
  density: "comfortable",
  sidebarCollapsed: false,
};

export const THEME_VALUES = THEME_OPTIONS.map((option) => option.value);
export const LAYOUT_VALUES = LAYOUT_OPTIONS.map((option) => option.value);
export const NAVBAR_VALUES = NAVBAR_OPTIONS.map((option) => option.value);
export const SIDEBAR_VALUES = SIDEBAR_OPTIONS.map((option) => option.value);
export const COLLAPSE_VALUES = COLLAPSE_OPTIONS.map((option) => option.value);
export const DENSITY_VALUES = DENSITY_OPTIONS.map((option) => option.value);

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

/** The three localStorage methods this module needs; easy to fake in tests. */
export type PreferenceStorage = {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
};

export type PreferencesReadResult = {
  preferences: Preferences;
  /** True when at least one value came from a pre-`dhi-preferences` key. */
  migrated: boolean;
  /** True when a valid/new-schema record was present under the new key. */
  stored: boolean;
};

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function includes<T extends string>(
  values: readonly T[],
  value: unknown,
): value is T {
  return typeof value === "string" && (values as readonly string[]).includes(value);
}

/**
 * Coerces an unknown JSON value into the subset of preferences that are valid.
 * Unknown keys, wrong types, and unknown option values are dropped rather than
 * trusted, so a corrupt/hand-edited record falls back field by field.
 */
export function normalizePreferences(value: unknown): Partial<Preferences> {
  if (!isRecord(value)) {
    return {};
  }

  const normalized: Partial<Preferences> = {};
  if (includes(THEME_VALUES, value.theme)) {
    normalized.theme = value.theme;
  }
  if (includes(LAYOUT_VALUES, value.layout)) {
    normalized.layout = value.layout;
  }
  if (value.navbar === "sticky" || value.navbar === "scroll") {
    // The top bar is always sticky; older records stored the retired "scroll"
    // choice, which is normalized away during load.
    normalized.navbar = "sticky";
  }
  if (includes(SIDEBAR_VALUES, value.sidebar)) {
    normalized.sidebar = value.sidebar;
  }
  if (includes(COLLAPSE_VALUES, value.collapse)) {
    normalized.collapse = value.collapse;
  }
  if (includes(DENSITY_VALUES, value.density)) {
    normalized.density = value.density;
  }
  if (typeof value.sidebarCollapsed === "boolean") {
    normalized.sidebarCollapsed = value.sidebarCollapsed;
  }
  return normalized;
}

/** Parse a raw JSON string without ever throwing. */
export function parsePreferences(raw: string | null): Partial<Preferences> {
  if (raw === null || raw === "") {
    return {};
  }
  try {
    return normalizePreferences(JSON.parse(raw));
  } catch {
    return {};
  }
}

/** Stable JSON shape written to localStorage. */
export function serializePreferences(preferences: Preferences): string {
  return JSON.stringify({
    theme: preferences.theme,
    layout: preferences.layout,
    navbar: preferences.navbar,
    sidebar: preferences.sidebar,
    collapse: preferences.collapse,
    density: preferences.density,
    sidebarCollapsed: preferences.sidebarCollapsed,
  });
}

function readLegacyTheme(storage: PreferenceStorage): ThemePreference | null {
  try {
    const value = storage.getItem(LEGACY_THEME_STORAGE_KEY);
    return includes(THEME_VALUES, value) ? value : null;
  } catch {
    return null;
  }
}

function readLegacySidebarCollapsed(
  storage: PreferenceStorage,
): boolean | null {
  try {
    const value = storage.getItem(LEGACY_SIDEBAR_STORAGE_KEY);
    if (value === "true") {
      return true;
    }
    if (value === "false") {
      return false;
    }
    return null;
  } catch {
    return null;
  }
}

/**
 * Read the current preferences, folding in the two legacy keys when the new
 * record is absent (or when a particular field was never stored).
 */
export function readPreferencesState(
  storage: PreferenceStorage,
): PreferencesReadResult {
  let raw: string | null = null;
  try {
    raw = storage.getItem(PREFERENCES_STORAGE_KEY);
  } catch {
    raw = null;
  }

  const storedPreferences = parsePreferences(raw);
  const stored = raw !== null && Object.keys(storedPreferences).length > 0;

  const legacyTheme =
    storedPreferences.theme === undefined ? readLegacyTheme(storage) : null;
  const legacyCollapsed =
    storedPreferences.sidebarCollapsed === undefined
      ? readLegacySidebarCollapsed(storage)
      : null;

  const migrated = legacyTheme !== null || legacyCollapsed !== null;
  const preferences: Preferences = {
    ...PREFERENCE_DEFAULTS,
    ...storedPreferences,
    ...(legacyTheme !== null ? { theme: legacyTheme } : {}),
    ...(legacyCollapsed !== null ? { sidebarCollapsed: legacyCollapsed } : {}),
  };

  return { preferences, migrated, stored };
}

/** Persist the complete record. Callers may catch blocked-storage errors. */
export function persistPreferences(
  storage: PreferenceStorage,
  preferences: Preferences,
): void {
  storage.setItem(PREFERENCES_STORAGE_KEY, serializePreferences(preferences));
}

/**
 * Read + one-time migration. When a legacy value is found, the merged record
 * is written under `dhi-preferences` and the two old keys are removed.
 */
export function migratePreferences(
  storage: PreferenceStorage,
): Preferences {
  const result = readPreferencesState(storage);
  if (!result.migrated) {
    return result.preferences;
  }

  try {
    persistPreferences(storage, result.preferences);
    storage.removeItem(LEGACY_THEME_STORAGE_KEY);
    storage.removeItem(LEGACY_SIDEBAR_STORAGE_KEY);
  } catch {
    // Private mode / blocked storage: keep the in-memory values for this tab.
  }
  return result.preferences;
}

/** Restore Defaults: remove the key entirely, including any stale legacy keys. */
export function clearPreferences(storage: PreferenceStorage): void {
  try {
    storage.removeItem(PREFERENCES_STORAGE_KEY);
    storage.removeItem(LEGACY_THEME_STORAGE_KEY);
    storage.removeItem(LEGACY_SIDEBAR_STORAGE_KEY);
  } catch {
    // Nothing to clear (or storage is unavailable).
  }
}

// ---------------------------------------------------------------------------
// Applying preferences
// ---------------------------------------------------------------------------

/** Minimal document root surface needed by the applier; keeps tests DOM-free. */
export type PreferenceRoot = {
  dataset: Record<string, string | undefined>;
  classList: {
    toggle(token: string, force?: boolean): void;
  };
  style: {
    colorScheme: string;
  };
};

export function resolveThemePreference(
  theme: ThemePreference,
  systemPrefersDark: boolean,
): "light" | "dark" {
  if (theme === "system") {
    return systemPrefersDark ? "dark" : "light";
  }
  return theme;
}

export function systemPrefersDark(): boolean {
  return (
    typeof window !== "undefined" &&
    typeof window.matchMedia === "function" &&
    window.matchMedia("(prefers-color-scheme: dark)").matches
  );
}

/**
 * Apply every attribute on <html>. This is deliberately imperative: the same
 * function backs the React provider and (as generated script) the pre-paint
 * bootstrap.
 */
export function applyPreferences(
  preferences: Preferences,
  root: PreferenceRoot,
  prefersDark: boolean,
): void {
  const resolved = resolveThemePreference(preferences.theme, prefersDark);

  root.dataset.theme = preferences.theme;
  root.dataset.layout = preferences.layout;
  root.dataset.navbar = preferences.navbar;
  root.dataset.sidebar = preferences.sidebar;
  root.dataset.collapse = preferences.collapse;
  root.dataset.density = preferences.density;
  // Not one of the six options, but carrying the runtime collapse state on
  // <html> lets CSS hide an offcanvas rail before React hydrates.
  root.dataset.sidebarCollapsed = preferences.sidebarCollapsed
    ? "true"
    : "false";

  root.classList.toggle("dark", resolved === "dark");
  root.style.colorScheme = resolved;
}

/**
 * Inline pre-paint bootstrap. It is generated from the same constants as the
 * provider, reads the new key first, falls back to the legacy keys, validates
 * every field, and applies all six attributes before first paint.
 *
 * Kept as a string (not JSX) so `layout.tsx` can put it in <head> without
 * shipping React state or waiting for hydration.
 */
export const preferencesBootstrapScript = `(function(){try{
var r=document.documentElement;
var d=${JSON.stringify(PREFERENCE_DEFAULTS)};
var v=${JSON.stringify({
  theme: THEME_VALUES,
  layout: LAYOUT_VALUES,
  navbar: NAVBAR_VALUES,
  sidebar: SIDEBAR_VALUES,
  collapse: COLLAPSE_VALUES,
  density: DENSITY_VALUES,
})};
var p=null;
try{var raw=window.localStorage.getItem(${JSON.stringify(PREFERENCES_STORAGE_KEY)});if(raw){var x=JSON.parse(raw);if(x&&typeof x==="object"&&!Array.isArray(x)){p=x;}}}catch(e){}
function pick(k,a){var val=p&&typeof p[k]==="string"?p[k]:null;return val&&a.indexOf(val)!==-1?val:null;}
var theme=pick("theme",v.theme);
var layout=pick("layout",v.layout);
var navbar=pick("navbar",v.navbar);
var sidebar=pick("sidebar",v.sidebar);
var collapse=pick("collapse",v.collapse);
var density=pick("density",v.density);
var collapsed=p&&typeof p.sidebarCollapsed==="boolean"?p.sidebarCollapsed:null;
try{var lt=window.localStorage.getItem(${JSON.stringify(LEGACY_THEME_STORAGE_KEY)});if(!theme&&(lt==="light"||lt==="dark"||lt==="system")){theme=lt;}}catch(e){}
try{var lc=window.localStorage.getItem(${JSON.stringify(LEGACY_SIDEBAR_STORAGE_KEY)});if(collapsed===null&&(lc==="true"||lc==="false")){collapsed=lc==="true";}}catch(e){}
theme=theme||d.theme;layout=layout||d.layout;navbar=navbar||d.navbar;sidebar=sidebar||d.sidebar;collapse=collapse||d.collapse;density=density||d.density;if(collapsed===null){collapsed=d.sidebarCollapsed;}
var dark=theme==="dark"||(theme==="system"&&typeof window.matchMedia==="function"&&window.matchMedia("(prefers-color-scheme: dark)").matches);
r.classList.toggle("dark",!!dark);
r.style.colorScheme=dark?"dark":"light";
r.setAttribute("data-theme",theme);
r.setAttribute("data-layout",layout);
r.setAttribute("data-navbar",navbar);
r.setAttribute("data-sidebar",sidebar);
r.setAttribute("data-collapse",collapse);
r.setAttribute("data-density",density);
r.setAttribute("data-sidebar-collapsed",collapsed?"true":"false");
}catch(e){}})();`;
