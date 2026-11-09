import assert from "node:assert/strict";
import { test } from "node:test";

import {
  instructionStatus,
  taskAcceptsInstructions,
  validateInstructionText,
} from "./instructions.ts";
import type { TaskState } from "./types.ts";

test("instructionStatus reports queued until delivered_at is set", () => {
  assert.equal(instructionStatus({ delivered_at: null }), "queued");
  assert.equal(
    instructionStatus({ delivered_at: "2026-11-09T10:00:00Z" }),
    "delivered",
  );
});

test("validateInstructionText rejects blank drafts and trims acceptable text", () => {
  for (const raw of ["", "   ", "\n\t "]) {
    assert.equal(validateInstructionText(raw), "Instruction text is required.");
  }
  assert.equal(validateInstructionText("focus on the rook line"), null);
  assert.equal(validateInstructionText("  padded  "), null);
});

test("taskAcceptsInstructions disables only terminal tasks", () => {
  const terminal: TaskState[] = ["done", "failed", "timeout", "cancelled"];
  const active: TaskState[] = ["created", "running", "paused"];
  for (const state of terminal) {
    assert.equal(taskAcceptsInstructions(state), false, state);
  }
  for (const state of active) {
    assert.equal(taskAcceptsInstructions(state), true, state);
  }
});
