import { useEffect, useState } from "react";
import { IconChevronLeft as ChevronLeft, IconChevronRight as ChevronRight } from "@tabler/icons-react";

import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

/** 大列表的分页尺寸：整页清单统一每页 5 条 */
export const PAGE_SIZE = 5;

/**
 * 客户端分页：清单整个在前端手里（技能、工具、权限表……都是整份拉回来的），
 * 切页就是切数组切片。resetKey 变了（换了个清单）回到第 1 页；
 * 清单变短导致页码越界时夹回有效区间。
 */
export function usePaged<T>(items: T[], pageSize: number = PAGE_SIZE, resetKey?: string) {
  const [page, setPage] = useState(0);
  const pages = Math.max(1, Math.ceil(items.length / pageSize));
  const safePage = Math.min(page, pages - 1);

  useEffect(() => {
    if (page !== safePage) setPage(safePage);
  }, [page, safePage]);
  useEffect(() => {
    if (resetKey !== undefined) setPage(0);
    // eslint-disable-next-line react-hooks/exhaustive-deps -- resetKey 变化即回首页
  }, [resetKey]);

  return {
    page: safePage,
    pages,
    total: items.length,
    slice: items.slice(safePage * pageSize, safePage * pageSize + pageSize),
    setPage,
  };
}

/** 数字页码窗口：不超过 7 页全亮；再多时首尾恒在、当前 ±1 在，间隔补省略号 */
function pageWindow(page: number, pages: number): number[] {
  if (pages <= 7) return Array.from({ length: pages }, (_, index) => index);
  const wanted = new Set<number>([0, pages - 1, page - 1, page, page + 1]);
  return [...wanted]
    .filter((value) => value >= 0 && value < pages)
    .sort((a, b) => a - b);
}

/**
 * 大列表的分页条：上一页 / 数字页码 / 下一页 + 跳到指定页。
 * 只有一页时不渲染——三五条的东西挂分页条是噪音。
 */
export function PaginationBar({
  page,
  pages,
  total,
  disabled = false,
  onPage,
}: {
  page: number;
  pages: number;
  total: number;
  disabled?: boolean;
  onPage: (page: number) => void;
}) {
  const [jump, setJump] = useState("");
  if (pages <= 1) return null;

  const numbers = pageWindow(page, pages);
  const commitJump = () => {
    const value = Number(jump);
    if (Number.isInteger(value) && value >= 1 && value <= pages) {
      onPage(value - 1);
    }
    setJump("");
  };

  return (
    <div className="mt-3 flex flex-wrap items-center justify-end gap-x-3 gap-y-1.5 text-xs text-muted-foreground">
      <span>
        共 {total} 条 · 第 {page + 1} / {pages} 页
      </span>
      <div className="flex items-center gap-1">
        <Button
          variant="ghost"
          size="icon"
          className="size-6"
          aria-label="上一页"
          disabled={disabled || page === 0}
          onClick={() => onPage(page - 1)}
        >
          <ChevronLeft className="size-3.5" />
        </Button>
        {numbers.map((number, index) => (
          <span key={number} className="flex items-center gap-1">
            {index > 0 && number - numbers[index - 1] > 1 ? (
              <span aria-hidden className="px-0.5">
                …
              </span>
            ) : null}
            <button
              type="button"
              aria-label={`第 ${number + 1} 页`}
              aria-current={number === page ? "true" : undefined}
              disabled={disabled}
              onClick={() => onPage(number)}
              className={cn(
                "min-w-6 rounded-md px-1.5 py-0.5 text-center text-xs outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
                number === page
                  ? "bg-brand/15 font-medium text-brand-text"
                  : "text-muted-foreground hover:bg-accent hover:text-foreground",
              )}
            >
              {number + 1}
            </button>
          </span>
        ))}
        <Button
          variant="ghost"
          size="icon"
          className="size-6"
          aria-label="下一页"
          disabled={disabled || page >= pages - 1}
          onClick={() => onPage(page + 1)}
        >
          <ChevronRight className="size-3.5" />
        </Button>
      </div>
      <div className="flex items-center gap-1">
        <span>跳至</span>
        <input
          type="text"
          inputMode="numeric"
          value={jump}
          aria-label="跳到的页码"
          placeholder={String(page + 1)}
          disabled={disabled}
          onChange={(event) => setJump(event.target.value.replace(/\D/g, ""))}
          onKeyDown={(event) => {
            if (event.key === "Enter") {
              event.preventDefault();
              commitJump();
            }
          }}
          className="h-7 w-12 rounded-lg border border-input bg-background px-1.5 text-center text-xs text-foreground outline-none transition-colors focus-visible:border-brand/50 focus-visible:ring-2 focus-visible:ring-ring/35"
        />
        <span>页</span>
        <Button variant="subtle" size="sm" className="h-7 px-2 text-xs" disabled={disabled || !jump} onClick={commitJump}>
          跳转
        </Button>
      </div>
    </div>
  );
}
