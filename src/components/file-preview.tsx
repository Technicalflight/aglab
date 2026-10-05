import { useEffect, useMemo, useState } from "react";

import { Markdown } from "@/components/markdown";
import { highlightCode, resolveLanguage, type HighlightToken } from "@/lib/highlight";
import { formatCount } from "@/lib/format";
import type { EditPreview } from "@/types/chat";

function extensionOf(path: string) {
  const name = path.split(/[\\/]/).pop() ?? "";
  const dot = name.lastIndexOf(".");
  return dot <= 0 ? "" : name.slice(dot + 1).toLowerCase();
}

/**
 * 沙箱里再补一道 CSP。iframe 的 sandbox 已经挡掉了脚本和同源访问，但没挡网络——
 * 不加这一道，预览一个 HTML 就会替它向外部发请求（图片、字体、像素追踪都算）
 */
function withCsp(source: string) {
  const meta =
    "<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'; img-src data: blob:; font-src data:\">";
  // 必须插在 head 里。塞在最前面会把文档推 quirks mode，布局跟着就变了
  if (/<head[^>]*>/i.test(source)) return source.replace(/<head[^>]*>/i, (head) => head + meta);
  for (const opener of [/<html[^>]*>/i, /<!doctype[^>]*>/i]) {
    if (opener.test(source)) {
      return source.replace(opener, (tag) => `${tag}<head>${meta}</head>`);
    }
  }
  return `${meta}${source}`;
}

function HtmlPreview({ source, path }: { source: string; path: string }) {
  const doc = useMemo(() => withCsp(source), [source]);
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <iframe
        title={`预览 ${path}`}
        srcDoc={doc}
        // 一个能力都不给：不跑脚本、不给同源、不开窗、不提交表单
        sandbox=""
        referrerPolicy="no-referrer"
        className="min-h-0 w-full flex-1 rounded-lg border border-border bg-web-canvas"
      />
      <p className="mt-1 shrink-0 text-2xs leading-4 text-muted-foreground">
        静态渲染：脚本被禁，外链的样式表与图片也不加载（相对路径在沙箱里解析不出来）。
      </p>
    </div>
  );
}

function ImagePreview({ preview, path }: { preview: EditPreview; path: string }) {
  return (
    <div className="flex min-h-0 flex-1 items-center justify-center overflow-auto rounded-lg border border-border bg-elevated">
      <img
        src={`data:${preview.mime};base64,${preview.data}`}
        alt={`预览 ${path}`}
        className="max-h-full max-w-full object-contain"
      />
    </div>
  );
}

/** svg 是文本，编成 data URL 交给 <img>：这样 svg 里的脚本不会执行 */
function SvgPreview({ source, path }: { source: string; path: string }) {
  return (
    <div className="flex min-h-0 flex-1 items-center justify-center overflow-auto rounded-lg border border-border bg-web-canvas">
      <img
        src={`data:image/svg+xml;charset=utf-8,${encodeURIComponent(source)}`}
        alt={`预览 ${path}`}
        className="max-h-full max-w-full object-contain"
      />
    </div>
  );
}

/** 编辑器样式：行号栏在横向滚动时钉住，语法色沿用代码块那套低饱和功能色 */
const HIGHLIGHT_LIMIT = 60_000;
const RENDER_LIMIT = 2_000;

function TextPreview({ code, language }: { code: string; language: string }) {
  const [lines, setLines] = useState<HighlightToken[][] | null>(null);
  const tooBigToHighlight = code.length > HIGHLIGHT_LIMIT;

  useEffect(() => {
    // 高亮一份几百 KB 的正文会把主线程占住好几秒。超阈值就只排版不上色，
    // 至少界面还转得动
    if (tooBigToHighlight) {
      setLines(null);
      return;
    }
    let active = true;
    highlightCode(code, resolveLanguage(language))
      .then((result) => active && setLines(result))
      .catch(() => active && setLines(null));
    return () => {
      active = false;
    };
  }, [code, language, tooBigToHighlight]);

  const allRows: HighlightToken[][] =
    lines ?? code.split("\n").map((line) => [{ content: line }]);
  const rows = allRows.slice(0, RENDER_LIMIT);

  return (
    <div className="flex min-h-0 flex-1 flex-col overflow-hidden rounded-lg border border-border bg-elevated">
      <div className="min-h-0 flex-1 overflow-auto">
        <pre className="font-mono text-xs leading-5">
          {rows.map((row, index) => (
            <div key={index} className="flex">
              <span className="sticky left-0 w-10 shrink-0 border-r border-border bg-elevated pr-1.5 text-right tabular-nums text-muted-foreground/55 select-none">
                {index + 1}
              </span>
              <code className="min-w-0 flex-1 py-px pl-2 pr-3 whitespace-pre text-foreground">
                {row.length === 0
                  ? " "
                  : row.map((token, tokenIndex) => (
                      <span
                        key={tokenIndex}
                        style={token.color ? { color: token.color } : undefined}
                      >
                        {token.content}
                      </span>
                    ))}
              </code>
            </div>
          ))}
        </pre>
      </div>
      {allRows.length > rows.length ? (
        <p className="shrink-0 border-t border-border px-3 py-1.5 text-2xs leading-4 text-muted-foreground">
          还有 {formatCount(allRows.length - rows.length)} 行没有显示。
          {tooBigToHighlight ? "正文过大，这次也没做语法着色。" : ""}
        </p>
      ) : tooBigToHighlight ? (
        <p className="shrink-0 border-t border-border px-3 py-1.5 text-2xs leading-4 text-muted-foreground">
          正文过大，没做语法着色。
        </p>
      ) : null}
    </div>
  );
}

/** 按扩展名把文件交给对应的渲染器。认不出来的当纯文本，不猜 */
export function FilePreview({ preview, path }: { preview: EditPreview; path: string }) {
  const ext = extensionOf(path);

  if (preview.kind === "binary") {
    return <p className="text-xs leading-5 text-muted-foreground">{preview.note}</p>;
  }

  if (preview.kind === "image") {
    return <ImagePreview preview={preview} path={path} />;
  }

  if (ext === "html" || ext === "htm") {
    return <HtmlPreview source={preview.content} path={path} />;
  }
  if (ext === "svg") {
    return <SvgPreview source={preview.content} path={path} />;
  }
  if (ext === "md" || ext === "markdown") {
    return (
      <div className="min-h-0 flex-1 overflow-y-auto rounded-lg border border-border bg-elevated px-3 py-3">
        <Markdown content={preview.content} />
      </div>
    );
  }

  return <TextPreview code={preview.content} language={ext} />;
}
