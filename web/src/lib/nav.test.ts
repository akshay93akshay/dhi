import assert from "node:assert/strict";
import { test } from "node:test";

import { crumbTrailSegments } from "./nav.ts";

// ---------------------------------------------------------------------------
// Nav — trail segments
// ---------------------------------------------------------------------------

test("crumbTrailSegments builds Tasks / #id / leaf", () => {
  const segments = crumbTrailSegments(
    "00000000-0000-4000-8000-000000000001",
    "Work",
  );
  assert.deepEqual(segments, [
    { label: "Tasks", href: "/tasks" },
    {
      label: "#00000000",
      href: "/tasks/00000000-0000-4000-8000-000000000001",
    },
    { label: "Work" },
  ]);
});

test("crumbTrailSegments degrades gracefully", () => {
  // Blank id → just Tasks + leaf.
  assert.deepEqual(crumbTrailSegments("", "Detail"), [
    { label: "Tasks", href: "/tasks" },
    { label: "Detail" },
  ]);
  // Blank leaf → no dangling separator segment.
  assert.deepEqual(crumbTrailSegments("task-9", "   "), [
    { label: "Tasks", href: "/tasks" },
    { label: "#task-9", href: "/tasks/task-9" },
  ]);
});
