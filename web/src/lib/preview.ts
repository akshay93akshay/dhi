/**
 * Compact, plain-text previews for dense surfaces such as task lists,
 * approvals, dashboard excerpts, and React Flow node labels.
 *
 * Full surfaces render through `MarkdownMath`; previews use this helper so raw
 * Markdown markers and TeX syntax never leak into a one-line label. Common TeX
 * commands are reduced to their visible glyphs, and unknown control sequences
 * are dropped. The helper is pure, dependency-free, and never throws on
 * malformed input.
 */

/** A fence-only line (with optional info string); the fenced content stays. */
const FENCE_LINE = /^[ \t]*(?:`{3,}|~{3,})[^`]*$/;

/**
 * One line-leading heading, blockquote, or list marker. Runs repeatedly so
 * nested forms such as `- > quote` are stripped down to their text.
 */
const LINE_PREFIX =
  /^[ \t]*(?:#{1,6}(?:[ \t]+|$)|(?:>[ \t]?)+|(?:[-*+]|\d{1,9}[.)])[ \t]+)/gm;

/** Paired single-asterisk emphasis, e.g. `*italic*` but not `2 * 3`. */
const ASTERISK_EMPHASIS = /\*([^\s*](?:[^*]*[^\s*])?)\*/g;

/** Paired single-underscore emphasis, e.g. `_italic_` but not `snake_case`. */
const UNDERSCORE_EMPHASIS = /(^|[^\w])_(?!\s)([^_]*?\S)_(?=$|[^\w])/g;

/** Markdown links and images, reduced to their label/alt text. */
const MARKDOWN_LINK = /!?\[([^\]]*)\]\((?:[^()]|\([^()]*\))*\)/g;

/**
 * Wrapper commands that only style their braced argument; the preview keeps
 * the argument itself, so `\mathbb{P}` reads as `P`.
 */
const TEX_WRAPPER = /\\(?:mathbb|mathcal|mathrm|text)\s*\{([^{}]*)\}/g;

/** A brace-delimited TeX group read from `start`, with its closing offset. */
function readTexGroup(
  input: string,
  start: number,
): { body: string; end: number } | null {
  let open = start;
  while (open < input.length && /\s/.test(input[open]!)) {
    open += 1;
  }
  if (input[open] !== "{") {
    return null;
  }
  let depth = 0;
  for (let index = open; index < input.length; index += 1) {
    const char = input[index];
    if (char === "{") {
      depth += 1;
    } else if (char === "}") {
      depth -= 1;
      if (depth === 0) {
        return { body: input.slice(open + 1, index), end: index + 1 };
      }
    }
  }
  return null;
}

/**
 * Replaces `\frac{a}{b}` with `a/b`, including fractions whose arguments
 * contain braces. Nested fractions reduce recursively; malformed input is
 * left for the generic cleanup below.
 */
function convertFractions(input: string, depth = 0): string {
  if (depth > 8 || !input.includes("\\frac")) {
    return input;
  }
  let output = "";
  let index = 0;
  while (index < input.length) {
    const command = input.indexOf("\\frac", index);
    if (command === -1) {
      output += input.slice(index);
      break;
    }
    output += input.slice(index, command);
    const numerator = readTexGroup(input, command + "\\frac".length);
    if (numerator === null) {
      output += "\\frac";
      index = command + "\\frac".length;
      continue;
    }
    const denominator = readTexGroup(input, numerator.end);
    if (denominator === null) {
      output += input.slice(command, numerator.end);
      index = numerator.end;
      continue;
    }
    output += `${convertFractions(numerator.body, depth + 1)}/${convertFractions(
      denominator.body,
      depth + 1,
    )}`;
    index = denominator.end;
  }
  return output;
}

/** Common control sequences mapped to the glyphs they print. */
const TEX_SYMBOLS: ReadonlyArray<readonly [RegExp, string]> = [
  [/\\not\s*\\cong\b/g, "≇"],
  [/\\cong\b/g, "≅"],
  [/\\leqslant\b/g, "≤"],
  [/\\leq\b/g, "≤"],
  [/\\le\b/g, "≤"],
  [/\\geqslant\b/g, "≥"],
  [/\\geq\b/g, "≥"],
  [/\\ge\b/g, "≥"],
  [/\\rightarrow\b/g, "→"],
  [/\\to\b/g, "→"],
  [/\\mapsto\b/g, "↦"],
  [/\\infty\b/g, "∞"],
  [/\\subset\b/g, "⊂"],
  [/\\times\b/g, "×"],
  [/\\cdot\b/g, "·"],
  [/\\sum\b/g, "Σ"],
  [/\\in\b/g, "∈"],
  [/\\alpha\b/g, "α"],
  [/\\beta\b/g, "β"],
  [/\\gamma\b/g, "γ"],
  [/\\delta\b/g, "δ"],
  [/\\epsilon\b/g, "ε"],
  [/\\varepsilon\b/g, "ε"],
  [/\\theta\b/g, "θ"],
  [/\\lambda\b/g, "λ"],
  [/\\mu\b/g, "μ"],
  [/\\nu\b/g, "ν"],
  [/\\pi\b/g, "π"],
  [/\\rho\b/g, "ρ"],
  [/\\sigma\b/g, "σ"],
  [/\\tau\b/g, "τ"],
  [/\\phi\b/g, "φ"],
  [/\\varphi\b/g, "φ"],
  [/\\chi\b/g, "χ"],
  [/\\psi\b/g, "ψ"],
  [/\\omega\b/g, "ω"],
  [/\\Gamma\b/g, "Γ"],
  [/\\Delta\b/g, "Δ"],
  [/\\Theta\b/g, "Θ"],
  [/\\Lambda\b/g, "Λ"],
  [/\\Sigma\b/g, "Σ"],
  [/\\Phi\b/g, "Φ"],
  [/\\Psi\b/g, "Ψ"],
  [/\\Omega\b/g, "Ω"],
];

/** Unicode superscripts for the simple exponents previews care about. */
function superscript(value: string): string | null {
  switch (value) {
    case "2":
      return "²";
    case "3":
      return "³";
    default:
      return null;
  }
}

/**
 * Reduce Markdown/TeX source to a readable single-line preview.
 *
 * The result keeps ordinary punctuation and the text inside code and math,
 * converting TeX commands and removing only the markup that would otherwise
 * read as noise in compact UI.
 */
export function plainPreview(text: string): string {
  if (!text) {
    return "";
  }

  let output = text.replace(/\r\n?/g, "\n");

  // Drop fence lines (``` and ~~~, plus any info string) but keep their body.
  output = output
    .split("\n")
    .filter((line) => !FENCE_LINE.test(line))
    .join("\n");

  // Inline code and any leftover backtick runs: keep the content.
  output = output.replace(/`+/g, "");

  // Links and images: keep the visible label.
  output = output.replace(MARKDOWN_LINK, "$1");

  // TeX delimiters: keep the math content itself.
  output = output.replace(/\\[()[\]]/g, "");
  output = output.replace(/\\?\${1,2}/g, "");

  // Wrapper commands such as `\mathbb{P}` keep only their argument. Repeat so
  // nested wrappers (`\mathbb{\mathcal{X}}`) reduce completely.
  let previous = "";
  while (previous !== output) {
    previous = output;
    output = output.replace(TEX_WRAPPER, "$1");
  }

  // Fractions become inline division, including braced/nested arguments.
  output = convertFractions(output);

  for (const [pattern, glyph] of TEX_SYMBOLS) {
    output = output.replace(pattern, glyph);
  }

  // Layout-only commands add spacing in TeX but no words in a preview.
  output = output.replace(/\\(?:left|right|displaystyle)\b\s*/g, "");

  // Simple superscripts become Unicode; everything else keeps a caret.
  output = output.replace(/\^\{([^{}]*)\}/g, (_match, body: string) => {
    const value = body.trim();
    return superscript(value) ?? `^${value}`;
  });
  // Subscript braces are always dropped: `x_{1}` -> `x_1`.
  output = output.replace(
    /_\{([^{}]*)\}/g,
    (_match, body: string) => `_${body.trim()}`,
  );

  // Line breaks and any unknown control sequence carry no preview text. The
  // braced argument of an unknown command stays, then grouping braces go.
  output = output.replace(/\\\\/g, " ");
  output = output.replace(/\\[a-zA-Z]+/g, " ");
  output = output.replace(/\\/g, " ");
  output = output.replace(/[{}]/g, "");

  // Leading heading, blockquote, and list markers, including nested forms.
  previous = "";
  while (previous !== output) {
    previous = output;
    output = output.replace(LINE_PREFIX, "");
  }

  // Emphasis markers. Bold runs go first, then paired single markers.
  output = output.replace(/\*\*/g, "").replace(/__/g, "");
  output = output.replace(ASTERISK_EMPHASIS, "$1");
  output = output.replace(UNDERSCORE_EMPHASIS, "$1$2");

  return output.replace(/\s+/g, " ").trim();
}
