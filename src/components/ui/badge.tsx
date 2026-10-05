import { cn } from "@/lib/utils";

/**
 * 徽标 / 标签。用于状态、分类、计数这类"贴在内容旁的小标记"。
 *
 * 圆角用 xs 档（4px）而非控件圆角：徽标本身很小，大圆角会显得像按钮，
 * 让人以为可以点。
 */
function Badge({
  className,
  tone = "neutral",
  ...props
}: React.ComponentProps<"span"> & {
  tone?: "neutral" | "brand" | "success" | "warning" | "danger" | "info";
}) {
  return (
    <span
      data-slot="badge"
      className={cn(
        "inline-flex shrink-0 items-center gap-1 rounded-xs px-1.5 py-0.5 text-2xs font-medium whitespace-nowrap",
        tone === "neutral" && "bg-fill-2 text-foreground-secondary",
        tone === "brand" && "bg-brand-subtle text-brand-text",
        tone === "success" && "bg-success-soft text-success",
        tone === "warning" && "bg-warning-soft text-warning",
        tone === "danger" && "bg-destructive-soft text-destructive",
        tone === "info" && "bg-info-soft text-info",
        className,
      )}
      {...props}
    />
  );
}

/**
 * 键盘按键。等宽字体 + 细边 + 微凸起，读作"可以按这个键"而不是"一段文字"。
 */
function Kbd({ className, children, ...props }: React.ComponentProps<"kbd">) {
  return (
    <kbd
      data-slot="kbd"
      className={cn(
        "inline-flex h-5 min-w-5 items-center justify-center rounded-xs border border-border bg-fill-2 px-1 font-mono text-2xs text-foreground-secondary select-none",
        className,
      )}
      {...props}
    >
      {children}
    </kbd>
  );
}

/** 分隔线。用 border-subtle 而非 border：分隔线该比轮廓线更轻 */
function Separator({
  className,
  orientation = "horizontal",
  ...props
}: React.ComponentProps<"div"> & { orientation?: "horizontal" | "vertical" }) {
  return (
    <div
      data-slot="separator"
      role="separator"
      aria-orientation={orientation}
      className={cn(
        "shrink-0 bg-border-subtle",
        orientation === "horizontal" ? "h-px w-full" : "h-full w-px",
        className,
      )}
      {...props}
    />
  );
}

export { Badge, Kbd, Separator };
