"use client";

import { useState } from "react";

import { describeApiError } from "@/lib/ui";
import {
  addSpecialistRole,
  formatToolRoles,
  sortRoles,
  toggleRoleSelection,
  toolDisplayLabel,
} from "@/lib/toolAdmin";
import type { ToolCatalogEntry } from "@/lib/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";

export type ToolCatalogTableProps = {
  tools: ToolCatalogEntry[];
  /** Base roles plus every specialist role in the catalog. */
  availableRoles: string[];
  /** Enables the enable/disable and visibility controls. */
  editable?: boolean;
  /** True when the control store is unavailable (reads still work). */
  controlsDisabled?: boolean;
  emptyMessage: string;
  onToggle?: (tool: ToolCatalogEntry, enabled: boolean) => Promise<void>;
  onSetVisibility?: (
    tool: ToolCatalogEntry,
    roles: string[],
  ) => Promise<void>;
  /** Receives toggle failures so the page can show one persistent banner. */
  onError?: (message: string) => void;
};

/**
 * Shared catalog table for `/tools`, `/skills`, and `/custom-tools`.
 *
 * Read-only mode renders the same columns without controls. Editable mode
 * adds an enable/disable button and a visibility dialog; all writes flow
 * through the page callbacks so the page remains the single owner of the
 * catalog state and error banner.
 */
export function ToolCatalogTable({
  tools,
  availableRoles,
  editable = false,
  controlsDisabled = false,
  emptyMessage,
  onToggle,
  onSetVisibility,
  onError,
}: ToolCatalogTableProps) {
  const [busyKey, setBusyKey] = useState<string | null>(null);
  const [visibilityTool, setVisibilityTool] =
    useState<ToolCatalogEntry | null>(null);
  const [selectedRoles, setSelectedRoles] = useState<string[]>([]);
  const [specialist, setSpecialist] = useState("");
  const [dialogError, setDialogError] = useState<string | null>(null);
  const [savingVisibility, setSavingVisibility] = useState(false);

  const canToggle = editable && !controlsDisabled && onToggle !== undefined;
  const canEditVisibility =
    editable && !controlsDisabled && onSetVisibility !== undefined;

  async function handleToggle(tool: ToolCatalogEntry): Promise<void> {
    if (!canToggle || onToggle === undefined) {
      return;
    }
    setBusyKey(tool.key);
    try {
      await onToggle(tool, !tool.enabled);
    } catch (error) {
      onError?.(
        `Could not ${tool.enabled ? "disable" : "enable"} ${tool.key}: ${describeApiError(error)}`,
      );
    } finally {
      setBusyKey(null);
    }
  }

  function openVisibility(tool: ToolCatalogEntry): void {
    setVisibilityTool(tool);
    setSelectedRoles(sortRoles(tool.default_roles));
    setSpecialist("");
    setDialogError(null);
  }

  function closeVisibility(): void {
    if (savingVisibility) {
      return;
    }
    setVisibilityTool(null);
    setDialogError(null);
  }

  function addSpecialist(): void {
    const result = addSpecialistRole(selectedRoles, specialist);
    if (!result.ok) {
      setDialogError(result.error);
      return;
    }
    setSelectedRoles(result.roles);
    setSpecialist("");
    setDialogError(null);
  }

  async function saveVisibility(): Promise<void> {
    if (visibilityTool === null || !canEditVisibility) {
      return;
    }
    if (onSetVisibility === undefined) {
      return;
    }
    setSavingVisibility(true);
    setDialogError(null);
    try {
      await onSetVisibility(visibilityTool, selectedRoles);
      setVisibilityTool(null);
    } catch (error) {
      setDialogError(
        `Could not update visibility for ${visibilityTool.key}: ${describeApiError(error)}`,
      );
    } finally {
      setSavingVisibility(false);
    }
  }

  const roleChoices = sortRoles([...availableRoles, ...selectedRoles]);

  return (
    <>
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead>Tool</TableHead>
            <TableHead>Source</TableHead>
            <TableHead>Enabled</TableHead>
            <TableHead>Roles</TableHead>
            <TableHead>Deferred</TableHead>
            {editable ? <TableHead>Actions</TableHead> : null}
          </TableRow>
        </TableHeader>
        <TableBody>
          {tools.length === 0 ? (
            <TableRow>
              <TableCell
                colSpan={editable ? 6 : 5}
                className="py-6 type-body text-muted-foreground"
              >
                {emptyMessage}
              </TableCell>
            </TableRow>
          ) : (
            tools.map((tool) => (
              <TableRow key={tool.key}>
                <TableCell className="max-w-[18rem]">
                  <div className="type-table font-medium">
                    {toolDisplayLabel(tool.key, tool.source)}
                  </div>
                  <div className="mt-0.5 type-mono break-all text-muted-foreground">
                    {tool.key}
                  </div>
                </TableCell>
                <TableCell>
                  <Badge variant="secondary">{tool.source}</Badge>
                </TableCell>
                <TableCell>
                  <div className="flex flex-wrap items-center gap-2">
                    <Badge variant={tool.enabled ? "success" : "destructive"}>
                      {tool.enabled ? "enabled" : "disabled"}
                    </Badge>
                    {editable ? (
                      <Button
                        type="button"
                        size="sm"
                        variant="outline"
                        disabled={!canToggle || busyKey === tool.key}
                        onClick={() => void handleToggle(tool)}
                        title={
                          controlsDisabled
                            ? "The tool control store is unavailable."
                            : undefined
                        }
                      >
                        {busyKey === tool.key
                          ? "Working…"
                          : tool.enabled
                            ? "Disable"
                            : "Enable"}
                      </Button>
                    ) : null}
                  </div>
                </TableCell>
                <TableCell className="max-w-[18rem]">
                  <div className="type-meta text-muted-foreground">
                    {formatToolRoles(tool.default_roles)}
                  </div>
                </TableCell>
                <TableCell>
                  {tool.deferred ? (
                    <Badge variant="warning">deferred</Badge>
                  ) : (
                    <span className="type-meta text-muted-foreground">—</span>
                  )}
                </TableCell>
                {editable ? (
                  <TableCell>
                    <Button
                      type="button"
                      size="sm"
                      variant="outline"
                      disabled={!canEditVisibility}
                      onClick={() => openVisibility(tool)}
                      title={
                        controlsDisabled
                          ? "The tool control store is unavailable."
                          : undefined
                      }
                    >
                      Visibility
                    </Button>
                  </TableCell>
                ) : null}
              </TableRow>
            ))
          )}
        </TableBody>
      </Table>

      <Dialog
        open={visibilityTool !== null}
        onOpenChange={(open) => {
          if (!open) {
            closeVisibility();
          }
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Visibility for {visibilityTool?.key ?? ""}</DialogTitle>
            <DialogDescription>
              Choose the roles that can see this tool. Leaving every role
              unchecked clears the explicit set, which the catalog treats as
              visible to every recognized role. To hide a tool entirely,
              disable it instead.
            </DialogDescription>
          </DialogHeader>

          <div className="space-y-4">
            <div className="grid gap-2 sm:grid-cols-2">
              {roleChoices.map((role) => (
                <label
                  key={role}
                  className="flex min-h-10 cursor-pointer items-center gap-2 rounded-md border border-border px-3 py-2 type-table"
                >
                  <Checkbox
                    checked={selectedRoles.includes(role)}
                    onCheckedChange={(checked) =>
                      setSelectedRoles((current) =>
                        toggleRoleSelection(current, role, checked === true),
                      )
                    }
                  />
                  <span className="type-mono">{role}</span>
                </label>
              ))}
            </div>

            <div className="space-y-2">
              <Label htmlFor="specialist-role">Add specialist role</Label>
              <div className="flex gap-2">
                <Input
                  id="specialist-role"
                  value={specialist}
                  placeholder="researcher"
                  onChange={(event) => setSpecialist(event.target.value)}
                  onKeyDown={(event) => {
                    if (event.key === "Enter") {
                      event.preventDefault();
                      addSpecialist();
                    }
                  }}
                />
                <Button
                  type="button"
                  variant="outline"
                  className="h-10"
                  onClick={addSpecialist}
                >
                  Add
                </Button>
              </div>
              <p className="type-meta text-muted-foreground">
                Stored as <code>specialist:&lt;name&gt;</code>. Recognized
                specialists only; no role inheritance is applied.
              </p>
            </div>

            {dialogError !== null ? (
              <div
                role="alert"
                className="rounded-md border border-destructive/30 bg-destructive/8 px-3 py-2 type-body text-destructive"
              >
                {dialogError}
              </div>
            ) : null}
          </div>

          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              onClick={closeVisibility}
              disabled={savingVisibility}
            >
              Cancel
            </Button>
            <Button
              type="button"
              onClick={() => void saveVisibility()}
              disabled={savingVisibility}
            >
              {savingVisibility ? "Saving…" : "Save visibility"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}
