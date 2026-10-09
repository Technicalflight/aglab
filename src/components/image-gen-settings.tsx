import { IconPalette as Palette } from "@tabler/icons-react";

import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { useChatStore } from "@/store/chat-store";
import { cn } from "@/lib/utils";

/** 分辨率档位：基准边长（会按比例换算并规整到 16 倍数，最大 3840） */
const SIZE_TIERS = [
  { label: "1K", base: 1024 },
  { label: "2K", base: 2048 },
  { label: "4K", base: 3840 },
] as const;

const RATIOS = ["1:1", "3:2", "2:3", "16:9", "9:16", "4:3", "3:4", "21:9"] as const;

const QUALITY_LABEL: Record<string, string> = {
  auto: "自动",
  high: "高",
  medium: "中",
  low: "低",
};

/** 档位 + 比例 → "宽x高"：按面积归一，宽高规整到 16 的倍数。
 *  上限压在**长边**上、另一边按比例缩——逐维度独立截断会把宽比例压变形
 *  （4K + 16:9 曾算成 3840x2880 = 4:3，比例选项看起来"点不动"） */
function sizeFor(base: number, ratio: string): string {
  const [rw, rh] = ratio.split(":").map(Number);
  if (!rw || !rh) return `${base}x${base}`;
  const to16 = (value: number) => Math.min(3840, Math.max(16, Math.round(value / 16) * 16));
  const wide = base * Math.sqrt(rw / rh);
  const high = base * Math.sqrt(rh / rw);
  const scale = Math.min(1, 3840 / Math.max(wide, high));
  return `${to16(wide * scale)}x${to16(high * scale)}`;
}

/**
 * 生图设置弹窗（参照生图产品的设置面板）：比例/分辨率、质量、数量。
 * 参数随生成请求**原样**发给上游——支不支持由上游/模型决定，不支持时
 * 通常被忽略或按其默认值处理，面板里如实说明这一点。
 * 视频会话不挂这个按钮：视频端点的参数形状各家差异太大，等接入时再配
 */
export function ImageGenSettingsPopover({
  variant = "palette",
}: {
  /** palette = 调色板图标（生图会话）；summary = "16:9 · 2K · N 张" 文字 chip
   *  （视频画布的图片页签，对照即梦把参数亮在底栏上） */
  variant?: "palette" | "summary";
}) {
  const imageGen = useChatStore((s) => s.config.imageGen);
  const updateConfig = useChatStore((s) => s.updateConfig);

  const tier = SIZE_TIERS.find((item) => imageGen.size.includes(`${item.base}`))?.base ?? 1024;
  const ratio =
    RATIOS.find((item) => {
      const [rw, rh] = item.split(":").map(Number);
      const [w, h] = imageGen.size.split("x").map(Number);
      return w && h && Math.abs(w / h - rw / rh) < 0.02;
    }) ?? "1:1";
  const preview = sizeFor(tier, ratio);

  const patch = (patch: Partial<typeof imageGen>) =>
    void updateConfig({ imageGen: { ...imageGen, ...patch } });

  const chip = (on: boolean) =>
    cn(
      "rounded-lg border px-2.5 py-1 text-xs outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
      on
        ? "border-brand/45 bg-brand/10 text-brand-text"
        : "border-border text-muted-foreground hover:bg-accent hover:text-foreground",
    );

  return (
    <Popover>
      <PopoverTrigger
        type="button"
        aria-label="生图设置"
        title="生图设置：比例 / 分辨率 / 质量 / 数量"
        className={cn(
          "outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
          variant === "summary"
            ? "flex items-center gap-1 rounded-lg border border-border bg-surface px-2 py-1 text-xs text-muted-foreground hover:border-brand/40 hover:text-foreground data-[state=open]:border-brand/40 data-[state=open]:text-foreground"
            : "flex size-8 items-center justify-center rounded-lg text-muted-foreground hover:bg-accent hover:text-foreground data-[state=open]:bg-accent data-[state=open]:text-foreground",
        )}
      >
        {variant === "summary" ? (
          <span className="tabular-nums">
            {ratio} · {SIZE_TIERS.find((item) => item.base === tier)?.label ?? "1K"} ·{" "}
            {imageGen.count} 张
          </span>
        ) : (
          <Palette className="size-4" />
        )}
      </PopoverTrigger>
      <PopoverContent align="end" className="w-[300px] space-y-3">
        <div>
          <p className="text-xs font-medium text-foreground">生图设置</p>
          <p className="mt-1 text-2xs leading-4 text-muted-foreground">
            这里的参数会随请求原样发给上游，但不保证一定生效：上游/模型是否支持该参数由上游决定，不支持时通常被忽略或按其默认值处理。以实际上游能力为准。
          </p>
        </div>

        <div>
          <p className="mb-1 text-2xs font-medium text-muted-foreground">分辨率档位</p>
          <div className="flex gap-1.5">
            {SIZE_TIERS.map((item) => (
              <button
                key={item.label}
                type="button"
                aria-pressed={tier === item.base}
                onClick={() => patch({ size: sizeFor(item.base, ratio) })}
                className={chip(tier === item.base)}
              >
                {item.label}
              </button>
            ))}
          </div>
          <p className="mb-1 mt-2.5 text-2xs font-medium text-muted-foreground">比例</p>
          <div className="flex flex-wrap gap-1.5">
            {RATIOS.map((item) => (
              <button
                key={item}
                type="button"
                aria-pressed={ratio === item}
                onClick={() => patch({ size: sizeFor(tier, item) })}
                className={chip(ratio === item)}
              >
                {item}
              </button>
            ))}
          </div>
          <p className="mt-1.5 text-2xs text-muted-foreground">预览尺寸 {preview}</p>
        </div>

        <div className="grid grid-cols-2 gap-2">
          <div>
            <p className="mb-1 text-2xs font-medium text-muted-foreground">质量</p>
            <Select value={imageGen.quality} onValueChange={(next) => patch({ quality: next })}>
              <SelectTrigger className="h-8 text-sm">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {Object.entries(QUALITY_LABEL).map(([value, label]) => (
                  <SelectItem key={value} value={value} className="text-sm">
                    {label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
          <div>
            <p className="mb-1 text-2xs font-medium text-muted-foreground">数量（1–10）</p>
            <input
              type="number"
              min={1}
              max={10}
              aria-label="生成数量"
              className="h-8 w-full rounded-lg border border-input bg-background px-2 text-sm tabular-nums text-foreground outline-none focus-visible:ring-2 focus-visible:ring-ring/45"
              value={imageGen.count}
              onChange={(event) =>
                patch({
                  count: Math.min(10, Math.max(1, Math.round(Number(event.target.value) || 1))),
                })
              }
            />
          </div>
        </div>
      </PopoverContent>
    </Popover>
  );
}
