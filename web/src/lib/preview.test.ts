import assert from "node:assert/strict";
import { test } from "node:test";

import { plainPreview } from "./preview.ts";

test("plainPreview strips Markdown bold and links", () => {
  assert.equal(plainPreview("**bold** and __strong__"), "bold and strong");
  assert.equal(
    plainPreview("See [the docs](https://example.com) for details."),
    "See the docs for details.",
  );
  assert.equal(
    plainPreview("[Wikipedia](https://en.wikipedia.org/wiki/Foo_(bar)) works."),
    "Wikipedia works.",
  );
});

test("plainPreview strips inline LaTeX delimiters", () => {
  assert.equal(plainPreview("\\(x^2\\)"), "x^2");
  assert.equal(plainPreview("\\[x^2\\]"), "x^2");
  assert.equal(
    plainPreview("The values \\(a + b\\) and \\[c + d\\] are sums."),
    "The values a + b and c + d are sums.",
  );
});

test("plainPreview strips dollar math delimiters", () => {
  assert.equal(plainPreview("$x$"), "x");
  assert.equal(plainPreview("$$x$$"), "x");
  assert.equal(
    plainPreview("Inline $a + b$ and display $$c + d$$ math."),
    "Inline a + b and display c + d math.",
  );
});

test("plainPreview keeps fenced and inline code content", () => {
  const fenced = ["```ts", "const value = 1;", "```"].join("\n");
  assert.equal(plainPreview(fenced), "const value = 1;");
  assert.equal(plainPreview("Use `npm test` now."), "Use npm test now.");

  const tildeFenced = ["~~~", "left < right", "~~~"].join("\n");
  assert.equal(plainPreview(tildeFenced), "left < right");
});

test("plainPreview strips headings, quotes, and list markers", () => {
  assert.equal(plainPreview("# Heading"), "Heading");
  assert.equal(plainPreview("## Sub heading"), "Sub heading");
  assert.equal(plainPreview("> quoted text"), "quoted text");
  assert.equal(plainPreview(">> nested quote"), "nested quote");
  assert.equal(plainPreview("- first\n- second"), "first second");
  assert.equal(plainPreview("* star item"), "star item");
  assert.equal(plainPreview("1. ordered item"), "ordered item");
  assert.equal(plainPreview("- > combined marker"), "combined marker");
});

test("plainPreview strips single emphasis markers but keeps identifiers", () => {
  assert.equal(plainPreview("*italic* and _emphasis_"), "italic and emphasis");
  assert.equal(
    plainPreview("Keep snake_case_names intact"),
    "Keep snake_case_names intact",
  );
  assert.equal(plainPreview("Wait... 2 * 3 = 6, right?"), "Wait... 2 * 3 = 6, right?");
});

test("plainPreview renders the task goal example without raw TeX", () => {
  assert.equal(
    plainPreview("Let C := V(y^{2}z − x^{3})⊂\\mathbb{P}^{2}..."),
    "Let C := V(y²z − x³)⊂P²...",
  );
});

test("plainPreview converts common TeX wrappers and symbols", () => {
  assert.equal(
    plainPreview(
      "\\mathbb{P}^{2}, \\mathcal{O}(1), \\mathrm{d}x, \\text{for all}",
    ),
    "P², O(1), dx, for all",
  );
  assert.equal(
    plainPreview(
      "\\pi \\lambda \\mu \\nu \\alpha \\beta \\infty \\subset \\cong \\not\\cong \\le \\leq \\ge \\geq \\times \\cdot \\to \\rightarrow \\mapsto \\in \\sum",
    ),
    "π λ μ ν α β ∞ ⊂ ≅ ≇ ≤ ≤ ≥ ≥ × · → → ↦ ∈ Σ",
  );
});

test("plainPreview converts fractions and simple scripts", () => {
  assert.equal(plainPreview("\\frac{a}{b}"), "a/b");
  assert.equal(plainPreview("\\frac{x^{2}}{\\pi}"), "x²/π");
  assert.equal(plainPreview("x^{2} + y^{3}"), "x² + y³");
  assert.equal(plainPreview("x_{1} + y_{n}"), "x_1 + y_n");
  // Braces still go when the exponent is not a simple digit.
  assert.equal(plainPreview("x^{n}"), "x^n");
});

test("plainPreview drops TeX layout commands and unknown commands", () => {
  assert.equal(
    plainPreview("\\displaystyle \\left( x_{1}^{2} + y^{n} \\right)"),
    "( x_1² + y^n )",
  );
  assert.equal(plainPreview("\\unknown{value}"), "value");
  assert.equal(plainPreview("\\frac{a}{"), "a");
});

test("plainPreview handles mixed Markdown and math", () => {
  const source = [
    "## Summary",
    "> **Important:** use \\(x^2\\) and `code`.",
    "- See [docs](https://example.com).",
    "```text",
    "raw code",
    "```",
  ].join("\n");

  assert.equal(
    plainPreview(source),
    "Summary Important: use x^2 and code. See docs. raw code",
  );
});

test("plainPreview leaves plain text unchanged", () => {
  assert.equal(
    plainPreview("Hello, world! (still here)"),
    "Hello, world! (still here)",
  );
  assert.equal(plainPreview("Task complete."), "Task complete.");
});

test("plainPreview returns an empty string for empty input", () => {
  assert.equal(plainPreview(""), "");
});

test("plainPreview does not throw on malformed input", () => {
  assert.doesNotThrow(() => plainPreview("```\nunclosed [link]( and **"));
  assert.equal(plainPreview("Unclosed **bold"), "Unclosed bold");
});
