import { useEffect } from "react";

import { Button } from "@/components/ui/button";
import { formatCount, formatTokens, formatUsd } from "@/lib/format";
import { useChatStore } from "@/store/chat-store";
import { cn } from "@/lib/utils";

/** 每页行数与后端 clamp 区间约定一致（store 固定传 20） */
const PAGE = 20;

function formatWhen(ts: number): string {
  const date = new Date(ts);
  const pad = (value: number) => String(value).padStart(2, "0");
  return `${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

/** 「最近请求」表：跟汇总同一个时间窗，20 行一页往下翻。 */
export function UsageRequestTable() {
  const rows = useChatStore((s) => s.usageRows);
  const total = useChatStore((s) => s.usageRowsTotal);
  const offset = useChatStore((s) => s.usageRowsOffset);
  const loading = useChatStore((s) => s.usageRowsLoading);
  const error = useChatStore((s) => s.usageRowsError);
  const loadRows = useChatStore((s) => s.loadUsageRows);
  const setOffset = useChatStore((s) => s.setUsageRowsOffset);

  useEffect(() => {
    void loadRows();
  }, [loadRows]);

  const canPrev = offset > 0 && !loading;
  const canNext = offset + PAGE < total && !loading;

  return (
    <div>
      <div className="flex items-baseline justify-between gap-2 border-t border-border pt-5">
        <h2 className="text-base font-semibold tracking-tight text-foreground">最近请求</h2>
        <span className="text-xs text-muted-foreground">
          {error ? "" : `共 ${formatCount(total)} 条`}
        </span>
      </div>
      <p className="mt-1 text-sm leading-6 text-muted-foreground">
        台账里每一次模型请求一行，失败的也留着—— 想知道"这个服务商今天挂了几次"，答案在这里。
      </p>

      {error ? <p className="mt-2 text-xs text-destructive">读取失败：{error}</p> : null}
      {loading ? <p className="mt-2 text-xs text-muted-foreground">读取中…</p> : null}

      {!error && !loading && rows.length === 0 ? (
        <p className="mt-3 text-sm text-muted-foreground">这个时间窗里没有请求。</p>
      ) : null}

      {rows.length > 0 ? (
        <>
          <div className="mt-3 overflow-x-auto rounded-lg border border-border bg-surface">
            <table className="w-full min-w-[640px] text-sm">
              <thead>
                <tr className="border-b border-border text-left text-xs text-muted-foreground">
                  <th className="px-3 py-2 font-normal">时间</th>
                  <th className="px-3 py-2 font-normal">模型</th>
                  <th className="px-3 py-2 text-right font-normal">输入</th>
                  <th className="px-3 py-2 text-right font-normal">输出</th>
                  <th className="px-3 py-2 text-right font-normal">缓存</th>
                  <th className="px-3 py-2 text-right font-normal">费用</th>
                  <th className="px-3 py-2 text-right font-normal">延迟</th>
                  <th className="px-3 py-2 text-right font-normal">首字</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((row) => (
                  <tr
                    key={row.id}
                    className={cn(
                      "border-b border-border/60 last:border-b-0",
                      !row.ok && "bg-destructive/5",
                    )}
                  >
                    <td className="whitespace-nowrap px-3 py-2 text-muted-foreground">
                      {formatWhen(row.ts)}
                    </td>
                    <td className="max-w-[160px] truncate px-3 py-2" title={row.model}>
                      {row.model}
                    </td>
                    <td className="px-3 py-2 text-right">{formatTokens(row.inputTokens)}</td>
                    <td className="px-3 py-2 text-right">{formatTokens(row.outputTokens)}</td>
                    <td
                      className="px-3 py-2 text-right"
                      title={row.cacheReported ? undefined : "服务商没回缓存字段，不是命中 0"}
                    >
                      {row.cacheReported ? formatTokens(row.cachedTokens) : "—"}
                    </td>
                    <td className="px-3 py-2 text-right">
                      {row.priced ? formatUsd(row.costUsd) : "—"}
                    </td>
                    <td className="px-3 py-2 text-right text-muted-foreground">
                      {row.latencyMs > 0 ? `${(row.latencyMs / 1000).toFixed(1)}s` : "—"}
                    </td>
                    <td className="px-3 py-2 text-right text-muted-foreground">
                      {row.firstTokenMs !== null && row.firstTokenMs > 0
                        ? `${row.firstTokenMs}ms`
                        : "—"}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          {rows.some((row) => !row.ok) ? (
            <ul className="mt-2 space-y-1">
              {rows
                .filter((row) => !row.ok)
                .map((row) => (
                  <li
                    key={`err-${row.id}`}
                    className="truncate text-xs text-destructive"
                    title={row.error}
                  >
                    {formatWhen(row.ts)} · {row.model} · {row.error}
                  </li>
                ))}
            </ul>
          ) : null}

          <div className="mt-3 flex items-center justify-end gap-2">
            <span className="text-xs text-muted-foreground">
              第 {Math.floor(offset / PAGE) + 1} / {Math.max(Math.ceil(total / PAGE), 1)} 页
            </span>
            <Button
              variant="subtle"
              size="sm"
              disabled={!canPrev}
              onClick={() => void setOffset(Math.max(offset - PAGE, 0))}
            >
              上一页
            </Button>
            <Button
              variant="subtle"
              size="sm"
              disabled={!canNext}
              onClick={() => void setOffset(offset + PAGE)}
            >
              下一页
            </Button>
          </div>
        </>
      ) : null}
    </div>
  );
}
