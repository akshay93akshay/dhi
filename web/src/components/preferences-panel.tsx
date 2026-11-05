"use client";

import { RotateCcw, SlidersHorizontal } from "lucide-react";

import { usePreferences } from "@/components/preferences-provider";
import { Button } from "@/components/ui/button";
import {
  Popover,
  PopoverContent,
  PopoverDescription,
  PopoverHeader,
  PopoverTitle,
  PopoverTrigger,
} from "@/components/ui/popover";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import {
  COLLAPSE_OPTIONS,
  DENSITY_OPTIONS,
  LAYOUT_OPTIONS,
  SIDEBAR_OPTIONS,
  THEME_OPTIONS,
  type CollapsePreference,
  type DensityPreference,
  type LayoutPreference,
  type SidebarPreference,
  type ThemePreference,
} from "@/lib/preferences";

type Option = { value: string; label: string };

function PreferenceSection({
  label,
  options,
  value,
  onChange,
}: {
  label: string;
  options: ReadonlyArray<Option>;
  value: string;
  onChange: (value: string) => void;
}) {
  return (
    <div className="space-y-1.5">
      <div className="text-[11px] font-semibold text-foreground">{label}</div>
      <ToggleGroup
        type="single"
        value={value}
        onValueChange={(next) => {
          if (next !== "") {
            onChange(next);
          }
        }}
        aria-label={label}
      >
        {options.map((option) => (
          <ToggleGroupItem
            key={option.value}
            value={option.value}
            aria-label={`${label}: ${option.label}`}
          >
            {option.label}
          </ToggleGroupItem>
        ))}
      </ToggleGroup>
    </div>
  );
}

/**
 * ARTEX-style preferences popover. Every control writes through the shared
 * provider, so this panel and the top-bar Theme control are never out of sync.
 */
export function PreferencesPanel() {
  const { preferences, setPreference, resetPreferences } = usePreferences();

  return (
    <Popover>
      <PopoverTrigger asChild>
        <Button
          type="button"
          variant="outline"
          size="icon"
          className="h-9 w-9 rounded-full border-border bg-card text-muted-foreground shadow-none hover:text-foreground"
          aria-label="Preferences"
          title="Preferences"
        >
          <SlidersHorizontal className="h-4 w-4" />
        </Button>
      </PopoverTrigger>
      <PopoverContent
        align="end"
        className="max-h-[min(82vh,640px)] w-[min(92vw,340px)] overflow-y-auto p-4"
      >
        <PopoverHeader className="mb-1 space-y-1">
          <PopoverTitle>Preferences</PopoverTitle>
          <PopoverDescription className="text-xs leading-5">
            Customize this browser&apos;s DHI layout.
          </PopoverDescription>
        </PopoverHeader>

        <div className="space-y-3.5">
          <PreferenceSection
            label="Theme"
            options={THEME_OPTIONS}
            value={preferences.theme}
            onChange={(value) =>
              setPreference("theme", value as ThemePreference)
            }
          />
          <PreferenceSection
            label="Page layout"
            options={LAYOUT_OPTIONS}
            value={preferences.layout}
            onChange={(value) =>
              setPreference("layout", value as LayoutPreference)
            }
          />
          <PreferenceSection
            label="Sidebar style"
            options={SIDEBAR_OPTIONS}
            value={preferences.sidebar}
            onChange={(value) =>
              setPreference("sidebar", value as SidebarPreference)
            }
          />
          <PreferenceSection
            label="Sidebar collapse"
            options={COLLAPSE_OPTIONS}
            value={preferences.collapse}
            onChange={(value) =>
              setPreference("collapse", value as CollapsePreference)
            }
          />
          <PreferenceSection
            label="Density"
            options={DENSITY_OPTIONS}
            value={preferences.density}
            onChange={(value) =>
              setPreference("density", value as DensityPreference)
            }
          />
        </div>

        <div className="mt-4 border-t border-border pt-3">
          <Button
            type="button"
            variant="outline"
            size="sm"
            className="w-full"
            onClick={resetPreferences}
          >
            <RotateCcw className="h-3.5 w-3.5" />
            Restore defaults
          </Button>
          <p className="mt-2 text-center text-[10.5px] text-muted-foreground">
            Saved in this browser
          </p>
        </div>
      </PopoverContent>
    </Popover>
  );
}
