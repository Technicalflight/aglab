import { useEffect, useState, type ReactNode } from "react";
import { IconRobot as Bot } from "@tabler/icons-react";
import { ClaudeCode, GeminiCLI, OpenAI, OpenCode, Qwen } from "@lobehub/icons";

import { aiImportFrom, aiImportScan } from "@/lib/chat-transport";
import { useChatStore } from "@/store/chat-store";
import type { AiImportSource } from "@/types/chat";
import { cn } from "@/lib/utils";
import { FormColumn } from "@/components/ui/content-column";

/** 每个来源用**各自产品的真图标**（lobehub 的品牌标，含 Claude Code / Gemini CLI / OpenCode 的专属款），
 *  衬底色取各自品牌：Anthropic 奶油底配陶橙日芒、OpenAI/Codex 黑底白结、
 *  Gemini 白底渐变星芒、Qwen 白底紫标、OpenCode 黑底白标。认不出的来源回落机器人灰牌 */
const SOURCE_ICONS: Record<string, { icon: ReactNode; tint: string }> = {
  claude: { icon: <ClaudeCode.Color size={22} />, tint: "bg-[#F0EEE6]" },
  codex: { icon: <OpenAI size={22} className="text-white" />, tint: "bg-neutral-900" },
  gemini: { icon: <GeminiCLI.Color size={22} />, tint: "bg-white" },
  qwen: { icon: <Qwen.Color size={22} />, tint: "bg-white" },
  opencode: { icon: <OpenCode size={22} className="text-white" />, tint: "bg-neutral-900" },
};

const FALLBACK_ICON = { icon: <Bot size={22} className="text-white" />, tint: "bg-neutral-600" };

/**
 * 设置页的「其他 AI 应用」项：把 Claude Code / Codex 的项目和聊天导进 aglab。
 * 源文件只读；重复导入按话题 id 跳过，不会堆出两份。
 */
export function ImportSettings() {
  const refreshHistory = useChatStore((s) => s.refreshHistory);
  const bootstrap = useChatStore((s) => s.bootstrap);

  const [sources, setSources] = useState<AiImportSource[] | null>(null);
  const [scanError, setScanError] = useState<string | null>(null);
  const [busyKind, setBusyKind] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    aiImportScan()
      .then((value) => {
        setSources(value);
        setScanError(null);
      })
      .catch((cause) => setScanError(cause instanceof Error ? cause.message : String(cause)));
  }, []);

  async function runImport(kind: string) {
    setBusyKind(kind);
    setError(null);
    setNote(null);
    try {
      const outcome = await aiImportFrom(kind);
      setNote(outcome.note);
      // config（项目列表）和话题列表都可能变了，全部拉新
      await bootstrap();
      await refreshHistory();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setBusyKind(null);
    }
  }

  return (
    <FormColumn>
      <h1 className="text-2xl font-semibold tracking-tight text-foreground">其他 AI 应用</h1>
      <p className="mt-1 text-sm leading-6 text-muted-foreground">
        把其他 AI 应用的项目和聊天记录导入 aglab。源文件只读不写，
        导入后的话题按原时间归位，已导入过的自动跳过。
      </p>

      <div className="mt-8">
        <h2 className="text-lg font-semibold tracking-tight text-foreground">导入来源</h2>
        <p className="mt-1 text-sm leading-6 text-muted-foreground">
          检测到本机安装过的 AI 应用及其话题数据。工具调用与执行结果不在导入范围——
          离开原来的运行环境它们没有意义，导入的是对话正文。
        </p>

        {scanError ? <p className="mt-3 text-sm text-destructive">检测失败：{scanError}</p> : null}

        <div className="mt-4 space-y-2">
          {(sources ?? []).map((source) => {
            const style = SOURCE_ICONS[source.kind] ?? FALLBACK_ICON;
            const importing = busyKind === source.kind;
            return (
              <div
                key={source.kind}
                className="flex items-center gap-3 rounded-lg border border-border bg-surface px-3 py-3"
              >
                <span
                  className={cn(
                    "flex size-9 shrink-0 items-center justify-center rounded-lg",
                    style.tint,
                  )}
                >
                  {style.icon}
                </span>
                <div className="min-w-0 flex-1">
                  <p className="text-base font-medium text-foreground">{source.name}</p>
                  <p
                    className={cn(
                      "mt-0.5 text-xs",
                      source.available ? "text-muted-foreground" : "text-muted-foreground/70",
                    )}
                  >
                    {source.detail}
                  </p>
                </div>
                <button
                  type="button"
                  disabled={!source.available || busyKind !== null}
                  onClick={() => void runImport(source.kind)}
                  className={cn(
                    "shrink-0 rounded-lg px-3.5 py-1.5 text-sm font-medium transition-colors",
                    source.available && busyKind === null
                      ? "bg-brand text-brand-foreground hover:bg-brand/90"
                      : "cursor-not-allowed bg-muted text-muted-foreground",
                  )}
                >
                  {importing ? "导入中…" : "导入"}
                </button>
              </div>
            );
          })}

          {!sources && !scanError ? (
            <p className="text-sm text-muted-foreground">正在检测本机可导入的应用…</p>
          ) : null}
        </div>

        {note ? (
          <p className="mt-4 rounded-lg border border-brand/30 bg-brand/10 px-3 py-2.5 text-sm leading-5 text-brand-text">
            {note}
          </p>
        ) : null}
        {error ? (
          <p className="mt-4 rounded-lg border border-destructive/30 bg-destructive/10 px-3 py-2.5 text-sm leading-5 text-destructive">
            {error}
          </p>
        ) : null}
      </div>
    </FormColumn>
  );
}
