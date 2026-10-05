/**
 * 工具行的两个小零件：文件类型的彩色小图标、行数差。ToolCard 与 RunFlow
 * 共用——两处各画一套的话，加一种扩展名就会有一处漏掉。
 */
import {
  IconAtom as Atom,
  IconBraces as Braces,
  IconFileCode as FileCode,
  IconFileText as FileText,
  IconLock as Lock,
  IconPalette as Palette,
  IconSettings as Settings,
  IconSettings2 as Settings2,
} from "@tabler/icons-react";

import { cn } from "@/lib/utils";

/** 扩展名 → 字形与颜色（Seti 风：rs 是橙色齿轮、tsx 是 React 蓝） */
const EXT_GLYPHS: Record<string, { Icon: typeof Settings; className: string }> = {
  rs: { Icon: Settings, className: "text-orange-500" },
  tsx: { Icon: Atom, className: "text-sky-500" },
  jsx: { Icon: Atom, className: "text-sky-500" },
  ts: { Icon: Braces, className: "text-blue-500" },
  mts: { Icon: Braces, className: "text-blue-500" },
  cts: { Icon: Braces, className: "text-blue-500" },
  js: { Icon: Braces, className: "text-yellow-600" },
  mjs: { Icon: Braces, className: "text-yellow-600" },
  cjs: { Icon: Braces, className: "text-yellow-600" },
  json: { Icon: Braces, className: "text-amber-500" },
  jsonc: { Icon: Braces, className: "text-amber-500" },
  py: { Icon: FileCode, className: "text-yellow-500" },
  md: { Icon: FileText, className: "text-slate-400" },
  mdx: { Icon: FileText, className: "text-slate-400" },
  toml: { Icon: Settings2, className: "text-stone-400" },
  yml: { Icon: Settings2, className: "text-stone-400" },
  yaml: { Icon: Settings2, className: "text-stone-400" },
  css: { Icon: Palette, className: "text-sky-400" },
  scss: { Icon: Palette, className: "text-sky-400" },
  lock: { Icon: Lock, className: "text-stone-400" },
};

export function FileGlyph({ ext, className }: { ext: string; className?: string }) {
  const glyph = EXT_GLYPHS[ext];
  const Icon = glyph?.Icon ?? FileCode;
  return (
    <Icon
      className={cn("size-3.5 shrink-0", glyph ? glyph.className : "text-muted-foreground", className)}
    />
  );
}

/** 行数差：+N 绿、−M 红。都为 0 时不画 */
export function DiffStat({ added, removed }: { added: number; removed: number }) {
  if (added === 0 && removed === 0) return null;
  return (
    <span className="shrink-0 font-mono text-xs tabular-nums">
      {added > 0 ? <span className="text-emerald-600">+{added}</span> : null}
      {added > 0 && removed > 0 ? <span className="text-muted-foreground/50"> </span> : null}
      {removed > 0 ? <span className="text-red-500">−{removed}</span> : null}
    </span>
  );
}
