import assert from "node:assert/strict";
import test from "node:test";

import { copyText } from "./clipboard.ts";

test("copyText returns false when no browser clipboard exists", async () => {
  assert.equal(await copyText("hello"), false);
});
