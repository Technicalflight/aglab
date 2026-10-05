import { useEffect, useState } from "react";
import { IconDownload as Download } from "@tabler/icons-react";

import { Button } from "@/components/ui/button";
import { PricingBlock } from "@/components/pricing-block";
import { SectionFrame } from "@/components/section-frame";
import { UsageRequestTable } from "@/components/usage-request-table";
import { UsageTrend } from "@/components/usage-trend";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { usageConversations, type ConversationUsage } from "@/lib/chat-transport";
import { formatCount, formatTokens, formatTokensCompact, formatUsd } from "@/lib/format";
import { useChatStore, type UsageDays } from "@/store/chat-store";
import { PaginationBar } from "@/components/pagination";
import { ModelIcon } from "@/components/model-icon";
import { cn } from "@/lib/utils";
import { TableSkeleton } from "@/components/ui/loading-skeleton";

/** 时间窗。days 语义与后端一致：0 = 全部（不是零天） */
const WINDOWS: Array<{ days: UsageDays; label: string; note: string }> = [
  { days: 1, label: "今天", note: "今天" },
  { days: 7, label: "最近 7 天", note: "近 7 天" },
  { days: 30, label: "最近 30 天", note: "近 30 天" },
  { days: 0, label: "全部", note: "全部" },
];

/** 按话题页的行数：与后端 clamp 上限（100）和请求表的每页习惯对齐 */
const CONVERSATION_PAGE_SIZE = 5;

/** 「按话题」表：钱烧在哪一场对话。自拉分页（后端 GROUP BY + LIMIT/OFFSET），
 *  时间窗跟汇总同一格——切窗就回第一页。标题查话题列表，查不到露 id 头几位 */
function UsageConversationTable() {
  const usageDays = useChatStore((s) => s.usageDays);
  const conversations = useChatStore((s) => s.conversations);
  const [page, setPage] = useState(0);
  const [rows, setRows] = useState<ConversationUsage[]>([]);
  const [total, setTotal] = useState(0);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // 换时间窗回第一页：窗口变了，旧的页码多半已经越界
  useEffect(() => {
    setPage(0);
  }, [usageDays]);

  useEffect(() => {
    let active = true;
    setLoading(true);
    setError(null);
    usageConversations(usageDays, page, CONVERSATION_PAGE_SIZE)
      .then((result) => {
        if (!active) return;
        setRows(result.rows);
        setTotal(result.total);
        setLoading(false);
      })
      .catch((cause: unknown) => {
        if (!active) return;
        setError(cause instanceof Error ? cause.message : String(cause));
        setLoading(false);
      });
    return () => {
      active = false;
    };
  }, [usageDays, page]);

  const titleOf = (conversation: string) => {
    if (!conversation) return "（未归属）";
    return conversations.find((item) => item.id === conversation)?.title ?? `${conversation.slice(0, 8)}…`;
  };

  const totalPages = Math.max(Math.ceil(total / CONVERSATION_PAGE_SIZE), 1);

  return (
    <div className="mt-6">
      <div className="flex items-baseline justify-between gap-2">
        <h2 className="text-base font-semibold tracking-tight text-foreground">按话题</h2>
        {error ? null : <span className="text-xs text-muted-foreground">共 {formatCount(total)} 场</span>}
      </div>
      <p className="mt-1 text-sm leading-6 text-muted-foreground">
        每场对话花的钱，按费用从多到少翻页。配合目标带的成本上限，回答"这个月烧在哪"。
      </p>

      {error ? <p className="mt-2 text-xs text-destructive">读取失败：{error}</p> : null}
      {loading ? <p className="mt-2 text-xs text-muted-foreground">读取中…</p> : null}

      {!error && !loading && rows.length === 0 ? (
        <p className="mt-3 text-sm text-muted-foreground">这个时间窗里还没有请求。</p>
      ) : null}

      {rows.length > 0 ? (
        <>
          <div className="mt-3 overflow-hidden rounded-lg border border-border bg-surface">
            <table className="w-full text-sm">
              <thead>
                <tr className="border-b border-border text-left text-xs text-muted-foreground">
                  <th className="px-3 py-2 font-normal">话题</th>
                  <th className="px-3 py-2 text-right font-normal">请求数</th>
                  <th className="px-3 py-2 text-right font-normal">输入</th>
                  <th className="px-3 py-2 text-right font-normal">输出</th>
                  <th className="px-3 py-2 text-right font-normal">费用</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((row) => (
                  <tr
                    key={row.conversation || "(unattributed)"}
                    className="border-b border-border/60 last:border-b-0"
                  >
                    <td className="max-w-[280px] truncate px-3 py-2.5" title={row.conversation}>
                      {titleOf(row.conversation)}
                    </td>
                    <td className="px-3 py-2.5 text-right">{formatCount(row.requests)}</td>
                    <td className="px-3 py-2.5 text-right">{formatTokens(row.inputTokens)}</td>
                    <td className="px-3 py-2.5 text-right">{formatTokens(row.outputTokens)}</td>
                    <td className="px-3 py-2.5 text-right">{formatUsd(row.costUsd)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          <PaginationBar
            page={page}
            pages={totalPages}
            total={total}
            disabled={loading}
            onPage={setPage}
          />
        </>
      ) : null}
    </div>
  );
}

function StatCard({
  label,
  value,
  hint,
  title,
}: {
  label: string;
  value: string;
  hint?: string;
  title?: string;
}) {
  return (
    <div className="rounded-lg border border-border bg-surface px-3 py-2.5">
      <p className="text-xs text-muted-foreground">{label}</p>
      <p className="mt-1 truncate text-lg font-medium text-foreground" title={title ?? value}>
        {value}
      </p>
      {hint ? <p className="mt-0.5 text-xs text-muted-foreground">{hint}</p> : null}
    </div>
  );
}

export function UsageView() {
  const usageDays = useChatStore((s) => s.usageDays);
  const usageReport = useChatStore((s) => s.usageReport);
  const usageLoading = useChatStore((s) => s.usageLoading);
  const usageError = useChatStore((s) => s.usageError);
  const refreshUsage = useChatStore((s) => s.refreshUsage);
  const setUsageDays = useChatStore((s) => s.setUsageDays);
  const loadPrices = useChatStore((s) => s.loadPrices);
  const exportUsageCsv = useChatStore((s) => s.exportUsageCsv);
  const [exportNote, setExportNote] = useState<string | null>(null);

  useEffect(() => {
    void refreshUsage();
    void loadPrices();
  }, [refreshUsage, loadPrices]);

  const totals = usageReport?.totals;
  const requests = totals?.requests ?? 0;
  const windowNote = WINDOWS.find((item) => item.days === usageDays)?.note ?? "";
  const note = totals
    ? `${windowNote} · ${formatCount(requests)} 次请求${usageLoading ? " · 读取中…" : ""}`
    : usageLoading
      ? "正在读取台账…"
      : "…";

  // 后端已按费用降序给过，这里再排一次：视图不该指望后端永远记得这件事
  const byModel = [...(usageReport?.byModel ?? [])].sort((a, b) => b.costUsd - a.costUsd);

  return (
    <SectionFrame
      title="用量"
      note={note}
      actions={
        <div className="flex items-center gap-2">
          <Select
            value={String(usageDays)}
            onValueChange={(value) => void setUsageDays(Number(value) as UsageDays)}
          >
            <SelectTrigger className="h-8 w-[130px] text-sm">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {WINDOWS.map((item) => (
                <SelectItem key={item.days} value={String(item.days)}>
                  {item.label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <Button
            variant="ghost"
            size="sm"
            disabled={!usageReport || usageLoading}
            onClick={() =>
              void exportUsageCsv().then((error) => setExportNote(error))
            }
          >
            <Download className="size-3.5" />
            <span>导出 CSV</span>
          </Button>
        </div>
      }
    >
      <p className="text-sm leading-6 text-muted-foreground">
        每次模型请求都会在这里记一笔：tokens 按价格表折成美元，失败的请求也留痕。
        没匹配到价格表的请求按 $0 记账，下方会单独报出来，不会假装成"免费"。
      </p>

      {usageError ? (
        <p className="mt-4 text-sm text-destructive">读取台账失败：{usageError}。</p>
      ) : null}

      {exportNote ? (
        <p className="mt-4 text-xs text-destructive">导出失败：{exportNote}</p>
      ) : null}

      {!usageReport && !usageError ? (
        <TableSkeleton rows={5} columns={4} className="mt-4" label="正在读取用量台账" />
      ) : null}

      {usageReport && requests === 0 ? (
        <p className="mt-4 text-sm leading-6 text-muted-foreground">
          还没记过账——发出第一条消息后这里会有数字。
        </p>
      ) : null}

      {usageReport && requests > 0 && totals ? (
        <>
          <div className="mt-4 grid grid-cols-2 gap-2 sm:grid-cols-3">
            <StatCard label="总花费" value={formatUsd(totals.costUsd)} />
            <StatCard
              label="请求"
              value={`${formatCount(totals.requests)} 次`}
              hint={totals.failed > 0 ? `失败 ${formatCount(totals.failed)} 次` : "全部成功"}
            />
            <StatCard
              label="输入 tokens"
              value={formatTokensCompact(totals.inputTokens)}
              title={formatTokens(totals.inputTokens)}
            />
            <StatCard
              label="输出 tokens"
              value={formatTokensCompact(totals.outputTokens)}
              title={formatTokens(totals.outputTokens)}
            />
            <StatCard
              label="缓存 tokens"
              value={formatTokensCompact(totals.cachedTokens)}
              title={
                totals.unreportedCacheRequests > 0
                  ? `${formatTokens(totals.cachedTokens)}（另有 ${totals.unreportedCacheRequests} 笔服务商没回缓存字段，未计入）`
                  : formatTokens(totals.cachedTokens)
              }
            />
            <StatCard
              label="推理 tokens"
              value={formatTokensCompact(totals.reasoningTokens)}
              title={formatTokens(totals.reasoningTokens)}
            />
          </div>

          {totals.unpricedRequests > 0 ? (
            <p className="mt-3 rounded-lg border border-warning-border bg-warning-soft px-3 py-2.5 text-xs leading-5 text-warning">
              有 {formatCount(totals.unpricedRequests)}{" "}
              次请求没匹配到价格表，按 $0 记账——总花费比实际偏低。
              <button
                type="button"
                className="underline underline-offset-2 hover:text-warning"
                onClick={() =>
                  document
                    .getElementById("pricing")
                    ?.scrollIntoView({ behavior: "smooth", block: "start" })
                }
              >
                补一条单价
              </button>
              。
            </p>
          ) : null}

          <div className="mt-5">
            <UsageTrend daily={usageReport.daily} />
          </div>

          <div className="mt-4 overflow-hidden rounded-lg border border-border bg-surface">
            <table className="w-full text-sm">
              <thead>
                <tr className="border-b border-border text-left text-xs text-muted-foreground">
                  <th className="px-3 py-2 font-normal">模型</th>
                  <th className="px-3 py-2 text-right font-normal">请求数</th>
                  <th className="px-3 py-2 text-right font-normal">输入</th>
                  <th className="px-3 py-2 text-right font-normal">输出</th>
                  <th className="px-3 py-2 text-right font-normal">缓存</th>
                  <th className="px-3 py-2 text-right font-normal">费用</th>
                  <th className="px-3 py-2 text-right font-normal">计价</th>
                </tr>
              </thead>
              <tbody>
                {byModel.map((row) => (
                  <tr
                    key={row.model}
                    className={cn(
                      "border-b border-border/60 last:border-b-0",
                      !row.priced && "text-muted-foreground",
                    )}
                  >
                    <td className="max-w-[180px] truncate px-3 py-2.5" title={row.model}>
                      <span className="flex items-center gap-1.5">
                        <ModelIcon model={row.model} size={13} />
                        <span className="truncate">{row.model}</span>
                      </span>
                    </td>
                    <td className="px-3 py-2.5 text-right">{formatCount(row.requests)}</td>
                    <td className="px-3 py-2.5 text-right">{formatTokens(row.inputTokens)}</td>
                    <td className="px-3 py-2.5 text-right">{formatTokens(row.outputTokens)}</td>
                    <td className="px-3 py-2.5 text-right">{formatTokens(row.cachedTokens)}</td>
                    <td className="px-3 py-2.5 text-right">{formatUsd(row.costUsd)}</td>
                    <td className="px-3 py-2.5 text-right">
                      {row.priced ? "已计价" : <span className="text-warning">未计价</span>}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          {usageLoading ? (
            <p className="mt-2 text-xs text-muted-foreground">正在按新窗口读取台账…</p>
          ) : null}

          <UsageConversationTable />
        </>
      ) : null}

      <div id="pricing" className="mt-6 scroll-mt-4">
        <PricingBlock />
      </div>

      <div className="mt-6">
        <UsageRequestTable />
      </div>
    </SectionFrame>
  );
}
