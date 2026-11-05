import assert from "node:assert/strict";
import { test } from "node:test";
import {
  ADD_GOAL_MAX_CHARS,
  canAddGoal,
  validateGoalDescription,
} from "./addGoal.ts";

test("canAddGoal is true only for terminal task states", () => {
  for (const state of ["done", "failed", "timeout"] as const) {
    assert.equal(canAddGoal(state), true, state);
  }
  for (const state of ["created", "running", "paused"] as const) {
    assert.equal(canAddGoal(state), false, state);
  }
});

test("validateGoalDescription rejects empty and whitespace-only text", () => {
  assert.equal(
    validateGoalDescription(""),
    "Goal description is required.",
  );
  assert.equal(
    validateGoalDescription("   \n\t "),
    "Goal description is required.",
  );
});

test("validateGoalDescription accepts the 500-character boundary", () => {
  assert.equal(validateGoalDescription("x".repeat(ADD_GOAL_MAX_CHARS)), null);
  assert.equal(
    validateGoalDescription(` ${"x".repeat(ADD_GOAL_MAX_CHARS)} `),
    null,
    "the limit applies after trimming",
  );
  assert.equal(
    validateGoalDescription("x".repeat(ADD_GOAL_MAX_CHARS + 1)),
    "Goal description must be at most 500 characters.",
  );
});
