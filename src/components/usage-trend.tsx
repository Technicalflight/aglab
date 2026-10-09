import type { DayUsage } from "@/types/chat";
import { formatCount, formatUsd } from "@/lib/format";

/**
 * 每日费用柱状图。纯 CSS 实现：数据形状只有 日期/请求数/费用 三列，
 * 引一个图表库比这几行 flex 重得多。
 *
 * 全 0 费用时按请求数兜底画柱，再全 0 就平铺占位——一根光秃秃的横线
 * 至少说明"这里是有东西的，只是没花钱"。
 */
export function UsageTrend({ daily }: { daily: DayUsage[] }) {
  // days=0 时 daily 可能拉出几百根，超出部分压掉，注明截断
  const TRUNCATE_AT = 90;
  const truncated = daily.length > TRUNCATE_AT;
  const shown = truncated ? daily.slice(-TRUNCATE_AT) : daily;

  const maxCost = Math.max(...shown.map((day) => day.costUsd), 0);
  const maxRequests = Math.max(...shown.map((day) => day.requests), 0);
  const byCost = maxCost > 0;

  function heightOf(day: DayUsage): number {
    const ratio = byCost ? day.costUsd / maxCost : maxRequests > 0 ? day.requests / maxRequests : 0;
    return Math.max(ratio * 100, 0);
  }

  return (
    <div>
      <div className="flex items-baseline justify-between gap-2">
        <h2 className="text-base font-semibold tracking-tight text-foreground">每日费用</h2>
        <span className="text-xs text-muted-foreground">
          {truncated
            ? `只画最近 ${TRUNCATE_AT} 天`
            : byCost
              ? "柱高按费用"
              : "没有费用记录，柱高按请求数"}
        </span>
      </div>
      <div className="mt-2 flex h-24 items-end gap-[2px] rounded-lg border border-border bg-surface px-2 py-2">
        {shown.map((day) => {
          const [, month, date] = day.date.split("-");
          const label = `${Number(month)}/${Number(date)}`;
          return (
            <div
              key={day.date}
              className="min-w-[4px] flex-1 rounded-sm bg-brand/70 transition-colors hover:bg-brand"
              style={{ height: `${Math.max(heightOf(day), 4)}%` }}
              title={`${label} · ${formatUsd(day.costUsd)} · ${formatCount(day.requests)} 次请求`}
            />
          );
        })}
      </div>
    </div>
  );
}
