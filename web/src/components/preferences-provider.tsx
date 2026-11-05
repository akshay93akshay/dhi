"use client";

import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";

import {
  PREFERENCES_STORAGE_KEY,
  PREFERENCE_DEFAULTS,
  applyPreferences,
  clearPreferences,
  migratePreferences,
  persistPreferences,
  readPreferencesState,
  systemPrefersDark,
  type Preferences,
  type PreferenceStorage,
} from "@/lib/preferences";

export type PreferencesContextValue = {
  preferences: Preferences;
  /** True after the provider has adopted the browser's stored values. */
  hydrated: boolean;
  setPreference: <K extends keyof Preferences>(
    key: K,
    value: Preferences[K],
  ) => void;
  resetPreferences: () => void;
};

const PreferencesContext = createContext<PreferencesContextValue | null>(null);

function browserStorage(): PreferenceStorage | null {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

/**
 * Owns the single source of truth for theme, layout, sidebar, collapse, and
 * density. The pre-paint script in `layout.tsx` applies stored values before
 * the first frame; this provider then adopts the same record after hydration so
 * React state and the document attributes stay in lockstep.
 *
 * The first render intentionally uses the server defaults to avoid hydration
 * mismatches. Layout-critical appearance is carried by the <html> attributes
 * set by the boot script, so adopting storage in an effect does not flash.
 */
export function PreferencesProvider({
  children,
}: {
  children: React.ReactNode;
}) {
  const [preferences, setPreferences] = useState<Preferences>(
    PREFERENCE_DEFAULTS,
  );
  const [hydrated, setHydrated] = useState(false);
  const preferencesRef = useRef(preferences);

  const apply = useCallback(
    (next: Preferences, persist: boolean): void => {
      preferencesRef.current = next;
      setPreferences(next);
      applyPreferences(next, document.documentElement, systemPrefersDark());

      if (!persist) {
        return;
      }
      const storage = browserStorage();
      if (storage === null) {
        return;
      }
      try {
        persistPreferences(storage, next);
      } catch {
        // The current document still reflects the choice.
      }
    },
    [],
  );

  useEffect(() => {
    const storage = browserStorage();
    const loaded =
      storage !== null ? migratePreferences(storage) : PREFERENCE_DEFAULTS;
    preferencesRef.current = loaded;
    setPreferences(loaded);
    setHydrated(true);
    applyPreferences(loaded, document.documentElement, systemPrefersDark());

    const media =
      typeof window.matchMedia === "function"
        ? window.matchMedia("(prefers-color-scheme: dark)")
        : null;
    const onSystemChange = (): void => {
      if (preferencesRef.current.theme === "system") {
        applyPreferences(
          preferencesRef.current,
          document.documentElement,
          systemPrefersDark(),
        );
      }
    };
    const onStorage = (event: StorageEvent): void => {
      if (event.key !== null && event.key !== PREFERENCES_STORAGE_KEY) {
        return;
      }
      const currentStorage = browserStorage();
      if (currentStorage === null) {
        return;
      }
      const next = readPreferencesState(currentStorage).preferences;
      preferencesRef.current = next;
      setPreferences(next);
      applyPreferences(next, document.documentElement, systemPrefersDark());
    };

    media?.addEventListener("change", onSystemChange);
    window.addEventListener("storage", onStorage);
    return () => {
      media?.removeEventListener("change", onSystemChange);
      window.removeEventListener("storage", onStorage);
    };
  }, []);

  const setPreference = useCallback(
    function setPreference<K extends keyof Preferences>(
      key: K,
      value: Preferences[K],
    ): void {
      apply({ ...preferencesRef.current, [key]: value }, true);
    },
    [apply],
  );

  const resetPreferences = useCallback((): void => {
    const storage = browserStorage();
    if (storage !== null) {
      clearPreferences(storage);
    }
    apply({ ...PREFERENCE_DEFAULTS }, false);
  }, [apply]);

  const value = useMemo<PreferencesContextValue>(
    () => ({ preferences, hydrated, setPreference, resetPreferences }),
    [preferences, hydrated, setPreference, resetPreferences],
  );

  return (
    <PreferencesContext.Provider value={value}>
      {children}
    </PreferencesContext.Provider>
  );
}

export function usePreferences(): PreferencesContextValue {
  const value = useContext(PreferencesContext);
  if (value === null) {
    throw new Error("usePreferences must be used inside PreferencesProvider");
  }
  return value;
}
