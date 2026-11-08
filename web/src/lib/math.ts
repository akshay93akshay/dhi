/**
 * Normalize the LaTeX delimiters a model tends to write (`\(...\)` and
 * `\[...\]`) into the dollar forms remark-math understands.
 *
 * This is a deliberately small, pure scanner. It copies code spans, fenced
 * code blocks, and existing `$...$` / `$$...$$` math through byte-for-byte so
 * a delimiter that happens to sit inside them is never rewritten.
 */

type Fence = {
  marker: string;
  length: number;
  contentStart: number;
};

/** True when the character at `index` is preceded by an odd `\` run. */
function isEscaped(text: string, index: number): boolean {
  let backslashes = 0;
  for (let cursor = index - 1; cursor >= 0 && text[cursor] === "\\"; cursor--) {
    backslashes += 1;
  }
  return backslashes % 2 === 1;
}

/**
 * Detect a fenced-code opener. CommonMark allows up to three leading spaces
 * and either backtick or tilde fences. The scanner calls this with `start` on
 * the first marker character after any copied indentation.
 */
function fenceAt(text: string, start: number): Fence | null {
  const marker = text[start];
  if (marker !== "`" && marker !== "~") {
    return null;
  }

  let lineStart = start;
  while (lineStart > 0 && text[lineStart - 1] !== "\n") {
    lineStart -= 1;
  }
  if (!/^ {0,3}$/.test(text.slice(lineStart, start))) {
    return null;
  }

  let runEnd = start;
  while (text[runEnd] === marker) {
    runEnd += 1;
  }
  if (runEnd - start < 3) {
    return null;
  }

  return {
    marker,
    length: runEnd - start,
    contentStart: runEnd,
  };
}

function isClosingFence(line: string, marker: string, length: number): boolean {
  let cursor = 0;
  while (cursor < 3 && line[cursor] === " ") {
    cursor += 1;
  }

  let runEnd = cursor;
  while (line[runEnd] === marker) {
    runEnd += 1;
  }

  return (
    runEnd - cursor >= length && /^[ \t\r]*$/.test(line.slice(runEnd))
  );
}

/** Return the exclusive end of a fenced block, or the end of the text. */
function fencedBlockEnd(text: string, fence: Fence): number {
  const openingLineEnd = text.indexOf("\n", fence.contentStart);
  if (openingLineEnd === -1) {
    return text.length;
  }

  let lineStart = openingLineEnd + 1;
  while (lineStart <= text.length) {
    const newline = text.indexOf("\n", lineStart);
    const lineEnd = newline === -1 ? text.length : newline;
    const line = text.slice(lineStart, lineEnd);
    if (isClosingFence(line, fence.marker, fence.length)) {
      return newline === -1 ? text.length : newline + 1;
    }
    if (newline === -1) {
      return text.length;
    }
    lineStart = newline + 1;
  }

  return text.length;
}

/**
 * Return the exclusive end of an inline code span, or the end of the opening
 * backtick run when there is no matching run.
 */
function codeSpanEnd(text: string, start: number): number {
  let openerEnd = start;
  while (text[openerEnd] === "`") {
    openerEnd += 1;
  }
  const width = openerEnd - start;

  let cursor = openerEnd;
  while (cursor < text.length) {
    if (text[cursor] !== "`") {
      cursor += 1;
      continue;
    }

    let runEnd = cursor;
    while (text[runEnd] === "`") {
      runEnd += 1;
    }
    if (runEnd - cursor === width) {
      return runEnd;
    }
    cursor = runEnd;
  }

  return openerEnd;
}

/**
 * Find the closing `$` for inline math. remark-math lets inline math span soft
 * line endings but not blank lines, so the search stops at a paragraph break.
 */
function inlineMathEnd(text: string, start: number): number {
  const remainder = text.slice(start);
  const paragraphBreak = remainder.search(/\n[ \t]*\r?\n/);
  const limit = paragraphBreak === -1 ? text.length : start + paragraphBreak;

  for (let cursor = start; cursor < limit; cursor++) {
    if (text[cursor] === "$" && !isEscaped(text, cursor)) {
      return cursor;
    }
  }

  return -1;
}

function findLatexCloser(text: string, start: number, closer: string): number {
  for (let cursor = start; cursor < text.length - 1; cursor++) {
    if (
      text[cursor] === "\\" &&
      text[cursor + 1] === closer &&
      !isEscaped(text, cursor)
    ) {
      return cursor;
    }
  }
  return -1;
}

/**
 * Rewrite `\(...\)` to `$...$` and `\[...\]` to `$$...$$`, leaving fenced
 * code, inline code, and already-dollar-delimited math untouched.
 */
export function normalizeMathDelimiters(text: string): string {
  let output = "";
  let cursor = 0;

  while (cursor < text.length) {
    const character = text[cursor];

    if (character === "`") {
      const fence = fenceAt(text, cursor);
      if (fence !== null) {
        const end = fencedBlockEnd(text, fence);
        output += text.slice(cursor, end);
        cursor = end;
        continue;
      }

      const end = codeSpanEnd(text, cursor);
      output += text.slice(cursor, end);
      cursor = end;
      continue;
    }

    if (character === "~") {
      const fence = fenceAt(text, cursor);
      if (fence !== null) {
        const end = fencedBlockEnd(text, fence);
        output += text.slice(cursor, end);
        cursor = end;
        continue;
      }

      output += character;
      cursor += 1;
      continue;
    }

    if (character === "$" && !isEscaped(text, cursor)) {
      if (text[cursor + 1] === "$") {
        const close = text.indexOf("$$", cursor + 2);
        if (close !== -1) {
          output += text.slice(cursor, close + 2);
          cursor = close + 2;
          continue;
        }

        output += "$$";
        cursor += 2;
        continue;
      }

      const close = inlineMathEnd(text, cursor + 1);
      if (close !== -1) {
        output += text.slice(cursor, close + 1);
        cursor = close + 1;
        continue;
      }

      output += character;
      cursor += 1;
      continue;
    }

    if (character === "\\" && !isEscaped(text, cursor)) {
      const opener = text[cursor + 1];
      if (opener === "(" || opener === "[") {
        const closer = opener === "(" ? ")" : "]";
        const close = findLatexCloser(text, cursor + 2, closer);
        if (close !== -1) {
          const body = text.slice(cursor + 2, close);
          output += opener === "(" ? `$${body}$` : `$$${body}$$`;
          cursor = close + 2;
          continue;
        }
      }
    }

    output += character;
    cursor += 1;
  }

  return output;
}
