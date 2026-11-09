"use client";

import { useEffect, useState } from "react";

/** React Flow's color mode vocabulary, resolved from the app theme. */
export type ColorMode = "light" | "dark";

/**
 * Resolves the live theme from the same `<html class="dark">` / `data-theme`
 * source the preferences provider writes and the pre-paint bootstrap applies.
 *
 * Observing the root element keeps React Flow in sync when the theme changes
 * without a reload: the preferences panel, the system-preference listener, and
 * cross-tab storage events all cycle through `applyPreferences`.
 */
export function useColorMode(): ColorMode {
  const [colorMode, setColorMode] = useState<ColorMode>("light");

  useEffect(() => {
    const root = document.documentElement;
    const read = (): void => {
      setColorMode(root.classList.contains("dark") ? "dark" : "light");
    };

    read();
    const observer = new MutationObserver(read);
    observer.observe(root, {
      attributes: true,
      attributeFilter: ["class", "data-theme"],
    });
    return () => observer.disconnect();
  }, []);

  return colorMode;
}
