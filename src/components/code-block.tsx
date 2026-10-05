import { useEffect, useMemo, useState } from "react";
import { IconCheck as Check, IconCopy as Copy } from "@tabler/icons-react";

import { Button } from "@/components/ui/button";
import { highlightCode, resolveLanguage, type HighlightToken } from "@/lib/highlight";
import { cn } from "@/lib/utils";

interface CodeBlockProps {
  code: string;
  language?: string;
  className?: string;
}

export function CodeBlock({ code, language, className }: CodeBlockProps) {
  const lang = useMemo(() => resolveLanguage(language), [language]);
  const [lines, setLines] = useState<HighlightToken[][] | null>(null);
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    let active = true;
    // 流式写入时代码块每个增量都在变，等笔画停住再高亮
    const timer = setTimeout(() => {
      highlightCode(code, lang)
        .then((result) => {
          if (active) setLines(result);
        })
        .catch(() => {
          if (active) setLines(null);
        });
    }, 150);

    return () => {
      active = false;
      clearTimeout(timer);
    };
  }, [code, lang]);

  useEffect(() => {
    if (!copied) return;
    const timer = setTimeout(() => setCopied(false), 1600);
    return () => clearTimeout(timer);
  }, [copied]);

  const rendered: HighlightToken[][] =
    lines ?? code.split("\n").map((line) => [{ content: line }]);

  return (
    <div
      className={cn(
        "my-4 overflow-hidden rounded-lg border border-border bg-elevated",
        className,
      )}
    >
      <div className="flex h-9 items-center justify-between border-b border-border px-3">
        <span className="text-xs font-medium tracking-[0.08em] text-foreground-tertiary uppercase">
          {language ?? "text"}
        </span>
        <Button
          variant="ghost"
          size="icon-sm"
          aria-label={copied ? "已复制" : "复制代码"}
          onClick={() => {
            void navigator.clipboard.writeText(code);
            setCopied(true);
          }}
        >
          {copied ? <Check className="size-3.5 text-brand-text" /> : <Copy className="size-3.5" />}
        </Button>
      </div>
      <pre className="overflow-x-auto px-4 py-3.5 font-mono text-base leading-6">
        <code>
          {rendered.map((line, index) => (
            <div key={index} className="min-h-6 whitespace-pre">
              {line.length === 0 ? " " : null}
              {line.map((token, tokenIndex) => (
                <span key={tokenIndex} style={token.color ? { color: token.color } : undefined}>
                  {token.content}
                </span>
              ))}
            </div>
          ))}
        </code>
      </pre>
    </div>
  );
}
