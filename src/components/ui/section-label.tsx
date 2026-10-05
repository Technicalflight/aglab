import { cn } from "@/lib/utils";

/**
 * 分区小标签。全应用有 20+ 处"全大写 + 加宽字距 + 三级灰"的分组标题，
 * 以前每处手写一遍 `text-xs font-medium tracking-[0.08em] text-muted-foreground uppercase`，
 * 改字距就得全文搜索 20 次。收成一个组件后，字距与颜色从此只有一处定义。
 *
 * size="sm" 用于卡片/面板内的分组（12px），默认档用于设置页与侧栏（11px）。
 */
function SectionLabel({
  className,
  size = "default",
  ...props
}: React.ComponentProps<"p"> & { size?: "default" | "sm" }) {
  return (
    <p
      data-slot="section-label"
      className={cn(
        "font-medium tracking-[0.08em] text-foreground-tertiary uppercase",
        size === "sm" ? "text-xs" : "text-2xs",
        className,
      )}
      {...props}
    />
  );
}

export { SectionLabel };
