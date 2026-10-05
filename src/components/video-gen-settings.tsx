import { IconMovie as Movie } from "@tabler/icons-react";

import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { useChatStore } from "@/store/chat-store";
import { cn } from "@/lib/utils";

/** 视频会话画布的生成参数面板（对照即梦"16:9 · 720P · 5S"）：
 *  比例 / 分辨率 / 时长，触发器把当前组合亮在底栏上。
 *  参数随生成请求**原样**发给上游（New API 视频文档口径），
 *  支不支持由上游/模型决定，面板里如实说明 */
const RATIOS = ["16:9", "9:16", "1:1", "4:3", "21:9"] as const;
const RESOLUTIONS = ["480P", "720P", "1080P"] as const;
const DURATIONS = [5, 10] as const;

export function VideoGenSettingsPopover() {
  const videoGen = useChatStore((s) => s.config.videoGen);
  const updateConfig = useChatStore((s) => s.updateConfig);

  const patch = (patch: Partial<typeof videoGen>) =>
    void updateConfig({ videoGen: { ...videoGen, ...patch } });

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
        aria-label="视频生成设置"
        title="视频生成设置：比例 / 分辨率 / 时长"
        className="flex items-center gap-1 rounded-lg border border-border bg-surface px-2 py-1 text-xs text-muted-foreground outline-none transition-colors hover:border-brand/40 hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45 data-[state=open]:border-brand/40 data-[state=open]:text-foreground"
      >
        <Movie className="size-3 shrink-0" />
        <span className="tabular-nums">
          {videoGen.ratio} · {videoGen.resolution} · {videoGen.duration}S
        </span>
      </PopoverTrigger>
      <PopoverContent align="end" className="w-[300px] space-y-3">
        <div>
          <p className="text-xs font-medium text-foreground">视频生成设置</p>
          <p className="mt-1 text-2xs leading-4 text-muted-foreground">
            参数随请求原样发给上游：支不支持由上游/模型决定，不支持时通常被忽略或按其默认值处理。以实际上游能力为准。
          </p>
        </div>

        <div>
          <p className="mb-1 text-2xs font-medium text-muted-foreground">比例</p>
          <div className="flex flex-wrap gap-1.5">
            {RATIOS.map((item) => (
              <button
                key={item}
                type="button"
                aria-pressed={videoGen.ratio === item}
                onClick={() => patch({ ratio: item })}
                className={chip(videoGen.ratio === item)}
              >
                {item}
              </button>
            ))}
          </div>
        </div>

        <div>
          <p className="mb-1 text-2xs font-medium text-muted-foreground">分辨率</p>
          <div className="flex gap-1.5">
            {RESOLUTIONS.map((item) => (
              <button
                key={item}
                type="button"
                aria-pressed={videoGen.resolution === item}
                onClick={() => patch({ resolution: item })}
                className={chip(videoGen.resolution === item)}
              >
                {item}
              </button>
            ))}
          </div>
          <p className="mb-1 mt-2.5 text-2xs font-medium text-muted-foreground">时长</p>
          <div className="flex gap-1.5">
            {DURATIONS.map((item) => (
              <button
                key={item}
                type="button"
                aria-pressed={videoGen.duration === item}
                onClick={() => patch({ duration: item })}
                className={chip(videoGen.duration === item)}
              >
                {item}S
              </button>
            ))}
          </div>
        </div>
      </PopoverContent>
    </Popover>
  );
}
