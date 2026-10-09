import { useEffect, useState } from "react";
import { IconChevronDown as ChevronDown, IconRefresh as RefreshCw } from "@tabler/icons-react";

import { Button } from "@/components/ui/button";
import { useChatStore } from "@/store/chat-store";
import { cn } from "@/lib/utils";

/**
 * 工具页顶部的「从 cc-switch 导入 MCP」卡片，默认收起。
 *
 * cc-switch 的 MCP 库普遍靠环境变量传凭据；导入会把 env 的值明文写进
 * config.json——所以勾选区把会动到密钥的变量名单独列出来，确认前必须看见。
 */
export function CcswitchMcpImport() {
  const mcpCandidates = useChatStore((s) => s.mcpCandidates);
  const loading = useChatStore((s) => s.mcpCandidatesLoading);
  const error = useChatStore((s) => s.mcpCandidatesError);
  const note = useChatStore((s) => s.mcpImportNote);
  const refresh = useChatStore((s) => s.refreshMcpCandidates);
  const importMcpServers = useChatStore((s) => s.importMcpServers);

  const [expanded, setExpanded] = useState(false);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [importing, setImporting] = useState(false);

  function toggle() {
    const next = !expanded;
    setExpanded(next);
    if (next && mcpCandidates.length === 0 && !error) void refresh();
  }

  // 候选每次重新拉取后恢复默认全选（只选可导入的）
  useEffect(() => {
    setSelected(new Set(mcpCandidates.map((candidate) => candidate.sourceId)));
  }, [mcpCandidates]);

  function flip(sourceId: string) {
    setSelected((previous) => {
      const next = new Set(previous);
      if (next.has(sourceId)) next.delete(sourceId);
      else next.add(sourceId);
      return next;
    });
  }

  async function submit() {
    setImporting(true);
    try {
      await importMcpServers([...selected]);
      setSelected(new Set());
    } finally {
      setImporting(false);
    }
  }

  const secretCandidates = mcpCandidates.filter((candidate) => candidate.secretEnvKeys.length > 0);

  return (
    <div className="rounded-lg border border-border bg-surface">
      <button
        type="button"
        onClick={toggle}
        className="flex w-full items-center gap-2 px-3 py-2.5 text-left focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/45"
      >
        <ChevronDown
          className={cn(
            "size-3.5 shrink-0 text-muted-foreground transition-transform",
            expanded && "rotate-180",
          )}
        />
        <span className="min-w-0 flex-1 truncate text-base font-medium text-foreground">
          从 cc-switch 导入 MCP
        </span>
        {mcpCandidates.length > 0 ? (
          <span className="text-xs text-muted-foreground">{mcpCandidates.length} 台候选</span>
        ) : null}
      </button>

      {expanded ? (
        <div className="border-t border-border px-3 py-3">
          <p className="text-xs leading-5 text-muted-foreground">
            清单读自 cc-switch 的 MCP 库，只列 stdio 型（http / sse 的导进来也起不来）。
            勾选后批量导入，导入的服务器默认启用，连接状态回下面的列表看。
          </p>

          <div className="mt-2.5 flex items-center gap-2">
            <Button
              variant="ghost"
              size="sm"
              aria-label="重新读取 cc-switch MCP 候选"
              disabled={loading}
              onClick={() => void refresh()}
            >
              <RefreshCw className={cn("size-3.5", loading && "animate-pulse")} />
              <span>重新读取</span>
            </Button>
            <Button
              variant="subtle"
              size="sm"
              disabled={loading || selected.size === 0 || importing}
              onClick={() => void submit()}
            >
              {importing ? "导入中…" : `导入所选 (${selected.size})`}
            </Button>
          </div>

          {error ? <p className="mt-3 text-sm text-destructive">{error}</p> : null}
          {note ? <p className="mt-3 text-xs leading-5 text-brand-text">{note}</p> : null}

          {!error && !loading && mcpCandidates.length === 0 ? (
            <p className="mt-3 text-sm text-muted-foreground">
              cc-switch 里没有能导过来的 MCP 服务器。
            </p>
          ) : null}

          <ul className="mt-3 space-y-1.5">
            {mcpCandidates.map((candidate) => {
              const checked = selected.has(candidate.sourceId);
              return (
                <li
                  key={candidate.sourceId}
                  className={cn(
                    "rounded-lg border px-2.5 py-2",
                    checked ? "border-brand/40 bg-background" : "border-border bg-background",
                  )}
                >
                  <div className="flex items-start gap-2.5">
                    <button
                      type="button"
                      role="checkbox"
                      aria-checked={checked}
                      aria-label={`选择 ${candidate.name}`}
                      onClick={() => flip(candidate.sourceId)}
                      className={cn(
                        "mt-0.5 size-3.5 shrink-0 rounded border transition-colors",
                        checked
                          ? "border-brand bg-brand text-2xs leading-none text-brand-foreground"
                          : "border-input bg-background",
                      )}
                    >
                      {checked ? "✓" : ""}
                    </button>
                    <div className="min-w-0 flex-1">
                      <p className="flex items-baseline gap-2">
                        <span className="text-sm font-medium text-foreground">
                          {candidate.name}
                        </span>
                        <span className="truncate font-mono text-xs text-muted-foreground">
                          {[candidate.command, ...candidate.args].join(" ")}
                        </span>
                      </p>
                      <p className="mt-0.5 text-xs text-muted-foreground">
                        {candidate.envKeys.length > 0
                          ? `环境变量 ${candidate.envKeys.length} 个 · 启用于 ${candidate.enabledFor.join("、") || "未启用"}`
                          : `无环境变量 · 启用于 ${candidate.enabledFor.join("、") || "未启用"}`}
                      </p>
                    </div>
                  </div>
                </li>
              );
            })}
          </ul>

          {secretCandidates.length > 0 && selected.size > 0 ? (
            <p className="mt-3 rounded-lg border border-warning-border bg-warning-soft px-3 py-2.5 text-xs leading-5 text-warning">
              注意：勾选中的条目带这些环境变量 （
              {[...new Set(secretCandidates.flatMap((candidate) => candidate.secretEnvKeys))].join(
                "、",
              )}
              ），导入会把它们的值<span className="text-foreground">明文写进 config.json</span>。
            </p>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}
