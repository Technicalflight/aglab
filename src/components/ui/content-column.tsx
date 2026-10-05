import { cn } from "@/lib/utils";

/**
 * 内容列容器。**行长上限是阅读体验的硬约束**，不是审美偏好：
 * 正文列超过约 75 个字符后眼睛回到行首容易串行。所以对话、设置、
 * 各类设置页共用同一条 760px 上限（--content-max 令牌），居中居中。
 *
 * 窄屏下留白必须收窄：px-8（32px）在 375px 屏上两侧就吃掉 64px，
 * 只剩 311px 给内容。这里用 sm: 断点分档，窄屏 16px、宽屏 32px。
 */
function ContentColumn({
  className,
  /** py：设置页有标题栏要留白，对话列表不需要 */
  padded = true,
  children,
  ...props
}: React.ComponentProps<"div"> & { padded?: boolean }) {
  return (
    <div
      data-slot="content-column"
      className={cn(
        "mx-auto w-full max-w-[var(--content-max)] px-4 sm:px-6 lg:px-8",
        padded && "py-6 sm:py-8",
        className,
      )}
      {...props}
    >
      {children}
    </div>
  );
}

/**
 * 设置分组的落点。ContentColumn 的窄一档：设置项本身已经是卡片，
 * 再套一层 760px 会让卡片在大屏上显得漂浮，640px 更贴手。
 */
function FormColumn({ className, children, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="form-column"
      className={cn(
        "mx-auto w-full max-w-[var(--content-max)] px-4 py-5 sm:px-6 sm:py-6",
        className,
      )}
      {...props}
    >
      {children}
    </div>
  );
}

export { ContentColumn, FormColumn };
