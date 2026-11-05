/**
 * Pure helpers for the add-goal panel on the task detail page.
 *
 * Adding a goal is the only path that reopens a terminal task, so the UI
 * offers it only for `done|failed|timeout|cancelled` and validates the
 * server's 500 character limit before submitting. No React or fetch here;
 * the component imports these and `addGoal.test.ts` exercises them directly.
 */

import type { TaskState } from "./types.ts";

/** Matches the server's post-trim limit (`ADD_GOAL_MAX_CHARS`). */
export const ADD_GOAL_MAX_CHARS = 500;

/** The only task states a human may add a goal to. */
export const TERMINAL_TASK_STATES: readonly TaskState[] = [
  "done",
  "failed",
  "timeout",
  "cancelled",
];

/** Whether the task-detail page should show the add-goal panel. */
export function canAddGoal(state: TaskState): boolean {
  return TERMINAL_TASK_STATES.includes(state);
}

/**
 * Validates the goal description the way the server does: trim first, then
 * require 1–500 characters. Returns an error message, or `null` when usable.
 */
export function validateGoalDescription(raw: string): string | null {
  const trimmed = raw.trim();
  if (trimmed === "") {
    return "Goal description is required.";
  }
  if (trimmed.length > ADD_GOAL_MAX_CHARS) {
    return `Goal description must be at most ${ADD_GOAL_MAX_CHARS} characters.`;
  }
  return null;
}
