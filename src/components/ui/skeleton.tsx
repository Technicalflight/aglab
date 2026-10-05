import { cn } from "@/lib/utils";

/**
 * 骨架屏。加载态的第一原则：**形状先于文字**。
 *
 * 加载中时先摆出与真实内容同高的占位，页面不会在数据到达时"弹一下"，
 * 用户的视线也不用重新找位置。用 animate-pulse 整块呼吸会让人误以为
 * 内容在闪，所以这里走一条横向扫过的高光——读作"正在来"，而非"这里坏了"。
 */
function Skeleton({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="skeleton"
      aria-hidden
      className={cn("skeleton animate-skeleton rounded-md bg-skeleton-sheen", className)}
      {...props}
    />
  );
}

/** 一行文字骨架。width 用真实文案的大致长度给，避免占位和内容宽度对不上 */
function SkeletonText({
  className,
  width = "100%",
  ...props
}: React.ComponentProps<"div"> & { width?: string }) {
  return <Skeleton className={cn("h-3 rounded-sm", className)} style={{ width }} {...props} />;
}

/** 列表项骨架：头像位 + 两行文字位，和真实列表行同构 */
function SkeletonRow({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div className={cn("flex items-center gap-3 px-3 py-2.5", className)} {...props}>
      <Skeleton className="size-8 shrink-0 rounded-md" />
      <div className="flex min-w-0 flex-1 flex-col gap-1.5">
        <SkeletonText width="38%" />
        <SkeletonText width="62%" className="h-2.5" />
      </div>
    </div>
  );
}

export { Skeleton, SkeletonRow, SkeletonText };
