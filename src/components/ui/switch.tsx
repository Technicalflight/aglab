import { cn } from "@/lib/utils";

/**
 * 开关。role="switch" + aria-checked 是正确姿势——读屏软件会念"开/关"
 * 而不是"复选框，已勾选"，与视觉上的滑动开关对得上。
 *
 * 整个控件（含轨道）都在 <button> 里，命中区 32px 高，
 * 比 WCAG 2.2 建议的 24px 最小目标尺寸宽裕。
 */
function Switch({
  checked,
  onCheckedChange,
  disabled,
  className,
  ...props
}: Omit<React.ComponentProps<"button">, "onChange" | "value"> & {
  checked: boolean;
  onCheckedChange: (checked: boolean) => void;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      disabled={disabled}
      onClick={() => onCheckedChange(!checked)}
      className={cn(
        "group relative inline-flex h-5 w-9 shrink-0 items-center rounded-full outline-none transition-colors duration-[var(--dur-fast)]",
        "focus-visible:ring-2 focus-visible:ring-ring/55 focus-visible:ring-offset-2 focus-visible:ring-offset-background",
        "disabled:cursor-not-allowed disabled:opacity-45",
        checked ? "bg-brand" : "bg-fill-1",
        className,
      )}
      {...props}
    >
      <span
        aria-hidden
        className={cn(
          "pointer-events-none block size-4 rounded-full bg-white shadow-sm transition-transform duration-[var(--dur-fast)]",
          checked ? "translate-x-4.5" : "translate-x-0.5",
        )}
      />
    </button>
  );
}

export { Switch };
