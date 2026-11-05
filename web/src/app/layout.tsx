import type { Metadata } from "next";
import "./globals.css";
import { AppShell } from "@/components/app-shell";
import { PreferencesProvider } from "@/components/preferences-provider";
import {
  PREFERENCE_DEFAULTS,
  preferencesBootstrapScript,
} from "@/lib/preferences";

export const metadata: Metadata = {
  title: {
    default: "DHI — Dynamic Heuristic Intelligence",
    template: "%s · DHI",
  },
  description: "Dynamic Heuristic Intelligence — task, tool, and agent control room.",
  icons: { icon: "/brand/dhi-mark-512.png" },
};

export default function RootLayout({
  children,
}: {
  children: React.ReactNode;
}) {
  return (
    <html
      lang="en"
      data-theme={PREFERENCE_DEFAULTS.theme}
      data-layout={PREFERENCE_DEFAULTS.layout}
      data-navbar={PREFERENCE_DEFAULTS.navbar}
      data-sidebar={PREFERENCE_DEFAULTS.sidebar}
      data-collapse={PREFERENCE_DEFAULTS.collapse}
      data-density={PREFERENCE_DEFAULTS.density}
      suppressHydrationWarning
    >
      <head>
        <script
          // Runs before first paint: storage/legacy preferences are applied
          // before the browser paints, so there is no theme or layout flash.
          dangerouslySetInnerHTML={{ __html: preferencesBootstrapScript }}
        />
      </head>
      <body>
        <PreferencesProvider>
          <AppShell>{children}</AppShell>
        </PreferencesProvider>
      </body>
    </html>
  );
}
