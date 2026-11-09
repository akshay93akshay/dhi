/**
 * Pure helpers for the planner-instruction panel on the work page.
 *
 * Instructions are planner-only notes: the server queues them, marks them
 * delivered when the planner reads them, and never adds them to worker
 * prompts. Keep this module React-free so `instructions.test.ts` can drive
 * the validation and status rules directly.
 */

import { TERMINAL_TASK_STATES } from "./addGoal.ts";
import type { Instruction, TaskState } from "./types.ts";

/** Delivery state shown next to one instruction in the history list. */
export type InstructionStatus = "queued" | "delivered";

/** `delivered` once the planner has read the instruction, otherwise queued. */
export function instructionStatus(
  instruction: Pick<Instruction, "delivered_at">,
): InstructionStatus {
  return instruction.delivered_at === null ? "queued" : "delivered";
}

/**
 * Validates the instruction draft the way the panel submits it: trim first,
 * then require non-empty text. Returns an error message, or `null` when
 * usable.
 */
export function validateInstructionText(raw: string): string | null {
  return raw.trim() === "" ? "Instruction text is required." : null;
}

/**
 * Whether the instruction form is enabled. Terminal tasks no longer have a
 * planner turn to consume the instruction, so the form is disabled.
 */
export function taskAcceptsInstructions(state: TaskState): boolean {
  return !TERMINAL_TASK_STATES.includes(state);
}
