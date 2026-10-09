import { Button } from "@/components/ui/button";
import { clampPage, pageCount } from "@/lib/pagination";

/**
 * 列表分页器。代理池与价格表共用：同一句"第 X / Y 页 · 共 N 条"的说法只写一处，
 * 免得两个列表各长成一个样又各漂一点。
 *
 * 装得下就整个不出现——一屏能看全的列表不需要一个"1 / 1 页"的噪音。
 */
export function Pager({
  page,
  total,
  size,
  onPage,
}: {
  page: number;
  total: number;
  size: number;
  onPage: (page: number) => void;
}) {
  const pages = pageCount(total, size);
  if (total <= size) return null;
  // 渲染时钳位：末页被删空之后停在还在的那一页，而不是停在一页不存在的位置上
  const current = clampPage(page, total, size);
  return (
    <div className="mt-3 flex flex-wrap items-center gap-2">
      <Button
        size="sm"
        variant="subtle"
        disabled={current === 0}
        onClick={() => onPage(current - 1)}
      >
        上一页
      </Button>
      <span className="text-xs tabular-nums text-muted-foreground">
        第 {current + 1} / {pages} 页 · 共 {total} 条
      </span>
      <Button
        size="sm"
        variant="subtle"
        disabled={current >= pages - 1}
        onClick={() => onPage(current + 1)}
      >
        下一页
      </Button>
    </div>
  );
}
