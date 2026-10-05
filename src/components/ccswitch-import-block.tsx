import { useEffect, useState } from "react";
import { IconChevronDown as ChevronDown, IconRefresh as RefreshCw } from "@tabler/icons-react";

import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { useChatStore } from "@/store/chat-store";
import type { CcswitchCandidate } from "@/types/chat";
import { cn } from "@/lib/utils";

/**
 * 设置面板里的「从 cc-switch 迁移」折叠区块。
 *
 * 数据只从 cc-switch 的库读：密钥在导入那一刻直写 Windows 凭据管理器，
 * 界面从头到尾只出现"带不带密钥"这个布尔。导入的语义是覆盖当前连接设置——
 * 弹层里必须说清楚这一点，确认了才动手。
 */
export function CcswitchImportBlock({
  defaultExpanded = false,
  title = "从 cc-switch 迁移",
}: {
  defaultExpanded?: boolean;
  title?: string;
}) {
  const candidates = useChatStore((s) => s.ccswitchCandidates);
  const loading = useChatStore((s) => s.ccswitchLoading);
  const error = useChatStore((s) => s.ccswitchError);
  const importingId = useChatStore((s) => s.ccswitchImportingId);
  const note = useChatStore((s) => s.ccswitchNote);
  const refresh = useChatStore((s) => s.refreshCcswitchCandidates);
  const importProvider = useChatStore((s) => s.importProvider);

  const [expanded, setExpanded] = useState(defaultExpanded);
  const [pending, setPending] = useState<CcswitchCandidate | null>(null);

  function toggle() {
    const next = !expanded;
    setExpanded(next);
    // 第一次展开才拉候选：收着的时候不打扰那个可能不存在的库
    if (next && candidates.length === 0 && !error) void refresh();
  }

  // 默认展开时（导入页）直接拉一次候选
  useEffect(() => {
    if (defaultExpanded && candidates.length === 0 && !error) void refresh();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return (
    <div className="mt-6 border-t border-border pt-5">
      <button
        type="button"
        onClick={toggle}
        className="flex w-full items-center gap-2 rounded-lg text-left focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/45"
      >
        <ChevronDown
          className={cn(
            "size-3.5 shrink-0 text-muted-foreground transition-transform",
            expanded && "rotate-180",
          )}
        />
        <h2 className="text-md font-semibold tracking-tight">{title}</h2>
        {candidates.length > 0 ? (
          <span className="text-xs text-muted-foreground">{candidates.length} 条候选</span>
        ) : null}
      </button>

      {expanded ? (
        <div className="mt-3">
          <p className="text-xs leading-5 text-muted-foreground">
            这份清单读自 cc-switch 自己的数据库，整个过程只读不改。导入会
            <span className="text-foreground">覆盖</span>
            「服务商档案」页里的连接设置（Base URL、模型、线协议）；密钥直接写进 Windows 凭据管理器，界面上只显示带不带。
            价表在「用量」页导，MCP 服务器在「工具」页导，技能在「技能」页导。
          </p>

          <div className="mt-3 flex items-center gap-2">
            <Button
              variant="ghost"
              size="sm"
              aria-label="重新读取 cc-switch 候选"
              disabled={loading}
              onClick={() => void refresh()}
            >
              <RefreshCw className={cn("size-3.5", loading && "animate-pulse")} />
              <span>重新读取</span>
            </Button>
          </div>

          {error ? (
            <p className="mt-3 text-sm leading-6 text-destructive">{error}</p>
          ) : null}

          {note ? <p className="mt-3 text-xs leading-5 text-brand-text">{note}</p> : null}

          {!error && !loading && candidates.length === 0 ? (
            <p className="mt-3 text-sm leading-6 text-muted-foreground">
              cc-switch 里没有能导过来的供应商（内置官方条目没有服务商地址）。
            </p>
          ) : null}

          <ul className="mt-3 space-y-2">
            {candidates.map((candidate) => (
              <li key={candidate.sourceId} className="rounded-lg border border-border bg-surface px-3 py-2.5">
                <div className="flex items-start gap-3">
                  <div className="min-w-0 flex-1">
                    <p className="flex flex-wrap items-baseline gap-x-2 gap-y-1">
                      <span className="font-medium text-foreground">{candidate.name}</span>
                      <span className="rounded border border-border px-1.5 py-px font-mono text-2xs text-muted-foreground">
                        {candidate.appType}
                      </span>
                      {candidate.isCurrent ? (
                        <span className="rounded bg-brand/15 px-1.5 py-px text-2xs text-brand-text">
                          cc-switch 当前使用
                        </span>
                      ) : null}
                      <span
                        className={cn(
                          "text-xs",
                          candidate.hasKey ? "text-muted-foreground" : "text-destructive",
                        )}
                      >
                        {candidate.hasKey ? "带密钥" : "无密钥"}
                      </span>
                    </p>
                    <p className="mt-1.5 break-all font-mono text-xs leading-5 text-foreground">
                      {candidate.endpoint}
                    </p>
                    <p className="mt-1 text-xs text-muted-foreground">
                      模型 {candidate.model || "（未填）"}
                      {candidate.reasoningEffort ? ` · 推理力度 ${candidate.reasoningEffort}` : ""}
                      {" · "}
                      {candidate.apiFormat === "responses" ? "/responses" : "/chat/completions"}
                    </p>
                    {candidate.note ? (
                      <p className="mt-1.5 rounded-lg border border-warning-border bg-warning-soft px-2.5 py-2 text-xs leading-5 text-warning">
                        {candidate.note}
                      </p>
                    ) : null}
                  </div>
                  <Button
                    variant="subtle"
                    size="sm"
                    className="shrink-0"
                    disabled={importingId !== null}
                    onClick={() => setPending(candidate)}
                  >
                    导入
                  </Button>
                </div>
              </li>
            ))}
          </ul>
        </div>
      ) : null}

      <Dialog
        open={pending !== null}
        onOpenChange={(open) => {
          if (!open) setPending(null);
        }}
      >
        <DialogContent className="max-w-md">
          <DialogTitle>覆盖导入「{pending?.name}」？</DialogTitle>
          {pending ? (
            <div className="mt-3">
              <p className="text-sm leading-6 text-muted-foreground">
                导入后连接设置会变成下面这样，原来的配置被覆盖。没有"撤销"。
              </p>
              <dl className="mt-3 space-y-2 rounded-lg border border-border bg-surface px-3 py-2.5 text-sm">
                <div>
                  <dt className="text-xs text-muted-foreground">请求地址</dt>
                  <dd className="mt-0.5 break-all font-mono text-xs text-foreground">
                    {pending.endpoint}
                  </dd>
                </div>
                <div>
                  <dt className="text-xs text-muted-foreground">模型</dt>
                  <dd className="mt-0.5 text-foreground">
                    {pending.model || "（这条没记模型名，保持你现在的选择）"}
                  </dd>
                </div>
                <div>
                  <dt className="text-xs text-muted-foreground">线协议</dt>
                  <dd className="mt-0.5 text-foreground">
                    {pending.apiFormat === "responses" ? "/responses" : "/chat/completions"}
                  </dd>
                </div>
                <div>
                  <dt className="text-xs text-muted-foreground">密钥</dt>
                  <dd className="mt-0.5 text-foreground">
                    {pending.hasKey
                      ? "会写入 Windows 凭据管理器，不在界面上显示"
                      : "这条没带密钥，导入后仍用你已保存的密钥"}
                  </dd>
                </div>
              </dl>
              {pending.note ? (
                <p className="mt-2.5 text-xs leading-5 text-warning">{pending.note}</p>
              ) : null}
              <div className="mt-4 flex justify-end gap-2">
                <Button variant="ghost" size="sm" onClick={() => setPending(null)}>
                  先不了
                </Button>
                <Button
                  variant="brand"
                  size="sm"
                  disabled={importingId !== null}
                  onClick={() => {
                    const sourceId = pending.sourceId;
                    setPending(null);
                    void importProvider(sourceId);
                  }}
                >
                  覆盖导入
                </Button>
              </div>
            </div>
          ) : null}
        </DialogContent>
      </Dialog>
    </div>
  );
}
