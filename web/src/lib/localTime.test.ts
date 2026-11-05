import assert from "node:assert/strict";
import test from "node:test";

import {
  formatLocalClock,
  formatLocalDate,
  formatLocalTimestamp,
} from "./localTime.ts";

test("local time helpers use the system timezone, not UTC", () => {
  const iso = "2026-10-30T00:15:30.123456789Z";
  const date = new Date(iso);
  const pad = (n: number): string => String(n).padStart(2, "0");

  assert.equal(
    formatLocalClock(iso),
    `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`,
  );
  assert.equal(
    formatLocalDate(iso),
    `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}`,
  );
  assert.match(formatLocalTimestamp(iso), /^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2} \S+$/);
  assert.ok(!formatLocalTimestamp(iso).endsWith("UTC") || date.getTimezoneOffset() === 0);
});

test("local time helpers pass unparseable input through", () => {
  assert.equal(formatLocalTimestamp("not-a-date"), "not-a-date");
  assert.equal(formatLocalClock(""), "");
  assert.equal(formatLocalDate("not-a-date"), "not-a-date");
});
