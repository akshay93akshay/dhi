import assert from "node:assert/strict";
import { test } from "node:test";

import { normalizeMathDelimiters } from "./math.ts";

test("normalizeMathDelimiters rewrites inline LaTeX delimiters", () => {
  assert.equal(normalizeMathDelimiters("\\(x^2\\)"), "$x^2$");
  assert.equal(
    normalizeMathDelimiters("The value is \\(\\frac{a}{b}\\)."),
    "The value is $\\frac{a}{b}$.",
  );
});

test("normalizeMathDelimiters rewrites display LaTeX delimiters", () => {
  assert.equal(normalizeMathDelimiters("\\[x^2\\]"), "$$x^2$$");
  assert.equal(
    normalizeMathDelimiters("Before\n\\[\n\\sum_{i=1}^n i\n\\]\nAfter"),
    "Before\n$$\n\\sum_{i=1}^n i\n$$\nAfter",
  );
});

test("normalizeMathDelimiters leaves existing dollar math unchanged", () => {
  assert.equal(normalizeMathDelimiters("$x$"), "$x$");
  assert.equal(normalizeMathDelimiters("$$x$$"), "$$x$$");
  assert.equal(
    normalizeMathDelimiters("Inline $x$ and display $$y$$ stay put."),
    "Inline $x$ and display $$y$$ stay put.",
  );
});

test("normalizeMathDelimiters does not rewrite delimiters inside dollar math", () => {
  assert.equal(
    normalizeMathDelimiters("$a \\(b\\) c$"),
    "$a \\(b\\) c$",
  );
  assert.equal(
    normalizeMathDelimiters("$$\na \\[b\\]\n$$"),
    "$$\na \\[b\\]\n$$",
  );
  assert.equal(
    normalizeMathDelimiters("$a\n\\(b\\)\nc$"),
    "$a\n\\(b\\)\nc$",
  );
});

test("normalizeMathDelimiters still converts after an unpaired dollar", () => {
  assert.equal(
    normalizeMathDelimiters("Cost $5, then \\(x\\)."),
    "Cost $5, then $x$.",
  );
});

test("normalizeMathDelimiters leaves inline code unchanged", () => {
  assert.equal(
    normalizeMathDelimiters("Use `\\(x\\)` as the syntax."),
    "Use `\\(x\\)` as the syntax.",
  );
  assert.equal(
    normalizeMathDelimiters("Use ``\\(x\\) and `\\[y\\]` `` here."),
    "Use ``\\(x\\) and `\\[y\\]` `` here.",
  );
});

test("normalizeMathDelimiters leaves fenced code blocks unchanged", () => {
  const source = [
    "```text",
    "\\(x^2\\)",
    "\\[y^2\\]",
    "```",
    "Outside \\(z^2\\).",
  ].join("\n");
  const expected = [
    "```text",
    "\\(x^2\\)",
    "\\[y^2\\]",
    "```",
    "Outside $z^2$.",
  ].join("\n");

  assert.equal(normalizeMathDelimiters(source), expected);
});

test("normalizeMathDelimiters handles indented and tilde fences", () => {
  const source = [
    "  ```",
    "  \\(x\\)",
    "  ```",
    "~~~",
    "\\(y\\)",
    "~~~",
    "\\(z\\)",
  ].join("\n");
  const expected = [
    "  ```",
    "  \\(x\\)",
    "  ```",
    "~~~",
    "\\(y\\)",
    "~~~",
    "$z$",
  ].join("\n");

  assert.equal(normalizeMathDelimiters(source), expected);
});

test("normalizeMathDelimiters rewrites multiple pairs on one line", () => {
  assert.equal(
    normalizeMathDelimiters("\\(a\\) + \\(b\\) = \\[c\\]"),
    "$a$ + $b$ = $$c$$",
  );
});

test("normalizeMathDelimiters rewrites multiple pairs across lines", () => {
  assert.equal(
    normalizeMathDelimiters("First \\(a\\)\nSecond \\(b\\)\nThird \\[c\\]"),
    "First $a$\nSecond $b$\nThird $$c$$",
  );
});

test("normalizeMathDelimiters leaves text without math unchanged", () => {
  const source = "# Heading\n\nPlain text with a backslash \\ and a [link](https://example.com).\n";
  assert.equal(normalizeMathDelimiters(source), source);
});

test("normalizeMathDelimiters leaves malformed pairs unchanged", () => {
  assert.equal(normalizeMathDelimiters("An open \\( stays."), "An open \\( stays.");
  assert.equal(normalizeMathDelimiters("An open \\[ stays."), "An open \\[ stays.");
});
