import { memo, type ReactElement } from "react";
import ReactMarkdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";
import remarkMath from "remark-math";
import rehypeKatex from "rehype-katex";

import { CodeBlock } from "@/components/code-block";
import remarkCjkAutolinkFix from "@/lib/remark-cjk-autolink-fix";

let katexStyles: Promise<unknown> | null = null;
/**
 * KaTeX 的样式连带 59 个字体文件（约 1.17MB）不进首屏：正文出现数学记号才拉，
 * 一次会话只拉一次。覆写规则（前景色/块级滚动）串行跟在本体后面——动态样式
 * 按到达先后算层叠，晚到的赢，覆写必须排在本体后面才不会被洗掉
 */
function ensureKatexStyles() {
  katexStyles ??= import("katex/dist/katex.min.css").then(() =>
    import("./katex-overrides.css"),
  );
  return katexStyles;
}

function languageOf(className?: string): string | undefined {
  const match = /language-([\w-]+)/.exec(className ?? "");
  return match?.[1];
}

const components: Components = {
  p: ({ children }) => (
    <p className="my-0 mb-3 text-[length:var(--chat-font-size)] leading-[1.75] last:mb-0">
      {children}
    </p>
  ),
  h1: ({ children }) => (
    <h1 className="mt-5 mb-2 text-[length:calc(var(--chat-font-size)*1.21)] font-semibold tracking-tight first:mt-0">
      {children}
    </h1>
  ),
  h2: ({ children }) => (
    <h2 className="mt-5 mb-2 text-[length:calc(var(--chat-font-size)*1.07)] font-semibold tracking-tight first:mt-0">
      {children}
    </h2>
  ),
  h3: ({ children }) => (
    <h3 className="mt-4 mb-1.5 text-[length:var(--chat-font-size)] font-semibold first:mt-0">
      {children}
    </h3>
  ),
  ul: ({ children }) => (
    <ul className="mb-3 mt-1 list-disc space-y-1.5 pl-5 text-[length:var(--chat-font-size)] leading-[1.7] marker:text-muted-foreground last:mb-0">
      {children}
    </ul>
  ),
  ol: ({ children }) => (
    <ol className="mb-3 mt-1 list-decimal space-y-1.5 pl-5 text-[length:var(--chat-font-size)] leading-[1.7] marker:text-muted-foreground last:mb-0">
      {children}
    </ol>
  ),
  li: ({ children }) => <li className="pl-0.5">{children}</li>,
  blockquote: ({ children }) => (
    <blockquote className="my-3 border-l-2 border-brand/45 pl-4 text-muted-foreground italic">
      {children}
    </blockquote>
  ),
  hr: () => <hr className="my-5 border-border" />,
  a: ({ children, href }) => (
    <a
      href={href}
      target="_blank"
      rel="noreferrer noopener"
      className="text-brand-text underline decoration-brand/35 underline-offset-2 transition-colors hover:decoration-brand"
    >
      {children}
    </a>
  ),
  strong: ({ children }) => <strong className="font-semibold text-foreground">{children}</strong>,
  table: ({ children }) => (
    <div className="scroll-fade-x-l scroll-fade-x my-4 overflow-x-auto rounded-lg border border-border">
      <table className="w-full border-collapse text-base">{children}</table>
    </div>
  ),
  thead: ({ children }) => (
    <thead className="bg-surface text-left text-muted-foreground">{children}</thead>
  ),
  th: ({ children }) => (
    <th className="px-3 py-2 font-medium">{children}</th>
  ),
  td: ({ children }) => <td className="border-t border-border px-3 py-2 align-top">{children}</td>,
  code: ({ children }) => (
    <code className="font-mono text-[length:calc(var(--chat-font-size)*0.93)] text-foreground">
      {children}
    </code>
  ),
  pre: ({ children }) => {
    const child = children as ReactElement<{ className?: string; children?: unknown }>;
    const raw = child?.props?.children;
    const code = typeof raw === "string" ? raw.replace(/\n$/, "") : String(raw ?? "");

    return <CodeBlock code={code} language={languageOf(child?.props?.className)} />;
  },
};

/**
 * memo 是流式性能的承重墙：长回答会被切成多个已完成段落，每个切片挂一个
 * Markdown——不 memo 的话，流式期间每个 flush（60ms 一次）所有已定稿切片
 * 都要整段重跑 react-markdown + KaTeX 解析，O(段落²) 地烧主线程。
 * content 字符串引用不变的切片在这里直接短路。
 */
export const Markdown = memo(function Markdown({ content }: { content: string }) {
  // 数学记号一出现就把样式备好（幂等，整个会话只拉一次）；没出现的会话永远不付这份钱
  if (content.includes("$")) void ensureKatexStyles();
  return (
    <div className="text-[length:var(--chat-font-size)] break-words text-foreground/90 [overflow-wrap:anywhere]">
      <ReactMarkdown
        remarkPlugins={[remarkGfm, remarkCjkAutolinkFix, remarkMath]}
        rehypePlugins={[rehypeKatex]}
        components={components}
      >
        {content}
      </ReactMarkdown>
    </div>
  );
});
