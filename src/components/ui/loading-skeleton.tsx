import { Skeleton, SkeletonRow, SkeletonText } from "@/components/ui/skeleton";

import { cn } from "@/lib/utils";

/**
 * 列表页的加载骨架。
 *
 * 为什么必须和空状态分开：插件/技能这类"读目录"的页，冷启动时数据还没回来，
 * 列表长度是 0——和"这个目录真的什么都没有"在数据上完全一样。
 * 合成一个分支处理，用户就会在加载途中被告知"还没有插件"，
 * 然后内容又凭空冒出来。像坏了，也像系统在骗人。
 *
 * 形状与真实行同构（左侧 32px 方块 + 两行文字），数据到达时行高对得上，
 * 页面不会"弹一下"。
 */
function ListSkeleton({
  rows = 4,
  className,
  label = "正在加载",
}: {
  rows?: number;
  className?: string;
  label?: string;
}) {
  return (
    <div role="status" aria-label={label} className={cn("space-y-1", className)}>
      {Array.from({ length: rows }, (_, i) => (
        <SkeletonRow key={i} />
      ))}
      <span className="sr-only">{label}</span>
    </div>
  );
}

/**
 * 详情/表单页的加载骨架：一行标题 + 几行说明 + 一块内容区。
 * 用于设置类页面冷启动（配置尚未落进 store 的那几百毫秒）。
 */
function PanelSkeleton({ className, label = "正在加载" }: { className?: string; label?: string }) {
  return (
    <div role="status" aria-label={label} className={cn("space-y-3", className)}>
      <SkeletonText width="22%" className="h-3.5" />
      <SkeletonText width="58%" className="h-2.5" />
      <div className="space-y-2 pt-2">
        {Array.from({ length: 3 }, (_, i) => (
          <Skeleton key={i} className="h-16 w-full rounded-lg" />
        ))}
      </div>
      <span className="sr-only">{label}</span>
    </div>
  );
}

/** 表格骨架：表头行 + 若干数据行，列数与真实表一致 */
function TableSkeleton({
  rows = 5,
  columns = 4,
  className,
  label = "正在加载",
}: {
  rows?: number;
  columns?: number;
  className?: string;
  label?: string;
}) {
  return (
    <div role="status" aria-label={label} className={cn("space-y-2", className)}>
      <div className="flex gap-3 border-b border-border-subtle pb-2">
        {Array.from({ length: columns }, (_, i) => (
          <Skeleton key={i} className="h-2.5 flex-1" />
        ))}
      </div>
      {Array.from({ length: rows }, (_, i) => (
        <div key={i} className="flex gap-3">
          {Array.from({ length: columns }, (_, c) => (
            <Skeleton
              key={c}
              className="h-3 flex-1"
              // 首列略宽，模拟"名称列比数值列长"的真实表格
              style={c === 0 ? undefined : { opacity: 0.6 }}
            />
          ))}
        </div>
      ))}
      <span className="sr-only">{label}</span>
    </div>
  );
}

export { ListSkeleton, PanelSkeleton, TableSkeleton };
