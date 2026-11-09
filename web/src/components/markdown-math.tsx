import type { ComponentProps } from "react";
import type { KatexOptions } from "katex";
import ReactMarkdown from "react-markdown";
import rehypeKatex from "rehype-katex";
import remarkMath from "remark-math";

import "katex/dist/katex.min.css";

import { normalizeMathDelimiters } from "@/lib/math";
import { cn } from "@/lib/utils";

type MarkdownMathProps = {
  text: string;
  variant?: "block" | "inline";
  className?: string;
};

type MarkdownOptions = ComponentProps<typeof ReactMarkdown>;

/** Shared KaTeX settings: render errors in place and never trust raw input. */
const KATEX_OPTIONS: KatexOptions = {
  throwOnError: false,
  trust: false,
  strict: false,
  output: "htmlAndMathml",
};

const REMARK_PLUGINS: MarkdownOptions["remarkPlugins"] = [remarkMath];
const REHYPE_PLUGINS: MarkdownOptions["rehypePlugins"] = [
  [rehypeKatex, KATEX_OPTIONS],
];

/** Inline rendering must not introduce paragraph-level block spacing. */
const INLINE_COMPONENTS: MarkdownOptions["components"] = {
  p: "span",
};

const BLOCK_CLASS = [
  // `whitespace-normal` overrides legacy plaintext wrappers that set
  // `white-space: pre-wrap` (for example the side-answer surface).
  "markdown-math block min-w-0 break-words whitespace-normal type-body text-foreground",
  "[&_p]:my-0 [&_p+p]:mt-2",
  "[&_strong]:font-semibold",
  "[&_a]:font-medium [&_a]:text-accent-foreground [&_a]:underline [&_a]:underline-offset-2",
  "[&_ul]:my-2 [&_ul]:list-disc [&_ul]:pl-5",
  "[&_ol]:my-2 [&_ol]:list-decimal [&_ol]:pl-5",
  "[&_li]:my-0.5 [&_li>p]:my-0",
  "[&_blockquote]:my-2 [&_blockquote]:border-l-2 [&_blockquote]:border-border [&_blockquote]:pl-3 [&_blockquote]:text-muted-foreground",
  "[&_code]:rounded-sm [&_code]:bg-muted [&_code]:px-1 [&_code]:py-0.5 [&_code]:font-mono [&_code]:text-[length:max(13px,0.85em)]",
  "[&_pre]:my-2 [&_pre]:overflow-x-auto [&_pre]:rounded-lg [&_pre]:border [&_pre]:border-border [&_pre]:bg-muted [&_pre]:p-3 [&_pre]:text-[length:var(--type-mono)]",
  "[&_pre_code]:bg-transparent [&_pre_code]:p-0 [&_pre_code]:text-[1em]",
  "[&_h1]:mt-3 [&_h1]:text-[length:var(--type-section)] [&_h1]:font-semibold",
  "[&_h2]:mt-3 [&_h2]:text-[length:var(--type-body)] [&_h2]:font-semibold",
  "[&_h3]:mt-2 [&_h3]:text-[length:var(--type-body)] [&_h3]:font-semibold",
  "[&_h4]:mt-2 [&_h4]:text-[length:var(--type-body)] [&_h4]:font-medium",
  "[&_hr]:my-3 [&_hr]:border-border",
  "[&_table]:my-2 [&_table]:w-full [&_table]:border-collapse [&_table]:text-[length:var(--type-table)]",
  "[&_th]:border [&_th]:border-border [&_th]:bg-muted/60 [&_th]:px-2 [&_th]:py-1 [&_th]:text-left [&_th]:font-semibold",
  "[&_td]:border [&_td]:border-border [&_td]:px-2 [&_td]:py-1",
  "[&_.katex-display]:my-2 [&_.katex-display]:overflow-x-auto [&_.katex-display]:overflow-y-hidden [&_.katex-display]:py-0.5",
].join(" ");

/**
 * Render model-authored Markdown and LaTeX with locally bundled KaTeX.
 *
 * Models emit both dollar math and LaTeX-style delimiters, so the source is
 * normalized before react-markdown sees it. Raw HTML stays disabled: no
 * rehype-raw plugin is installed.
 */
export function MarkdownMath({
  text,
  variant = "block",
  className,
}: MarkdownMathProps) {
  const source = normalizeMathDelimiters(text);

  if (variant === "inline") {
    return (
      <span
        className={cn(
          "markdown-math inline max-w-full break-words [&_.katex]:text-inherit",
          className,
        )}
      >
        <ReactMarkdown
          remarkPlugins={REMARK_PLUGINS}
          rehypePlugins={REHYPE_PLUGINS}
          components={INLINE_COMPONENTS}
        >
          {source}
        </ReactMarkdown>
      </span>
    );
  }

  return (
    <div className={cn(BLOCK_CLASS, className)}>
      <ReactMarkdown
        remarkPlugins={REMARK_PLUGINS}
        rehypePlugins={REHYPE_PLUGINS}
      >
        {source}
      </ReactMarkdown>
    </div>
  );
}
