import { cn } from "@/lib/utils";

/** 能力开关：工具、技能、扩展三处共用同一套读法（已开启 / 已关闭） */
export function CapabilityToggle({
  label,
  enabled,
  onToggle,
}: {
  label: string;
  enabled: boolean;
  onToggle: () => void;
}) {
  return (
    <button
      type="button"
      aria-pressed={enabled}
      aria-label={`${enabled ? "关闭" : "开启"}${label}`}
      onClick={onToggle}
      className={cn(
        "flex h-8 shrink-0 items-center gap-1.5 rounded-lg border px-2.5 text-xs transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
        enabled
          ? "border-brand/45 bg-brand/10 text-brand-text"
          : "border-border text-muted-foreground hover:text-foreground",
      )}
    >
      <span
        className={cn("size-1.5 rounded-full", enabled ? "bg-brand" : "bg-muted-foreground/70")}
      />
      {enabled ? "已开启" : "已关闭"}
    </button>
  );
}
