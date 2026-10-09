import { useEffect, useMemo, useState } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import {
  IconPhoto as ImagePlus,
  IconSparkles as Sparkles,
  IconVideo as Video,
  IconX as X,
} from "@tabler/icons-react";

import { Button } from "@/components/ui/button";
import { ModelPicker } from "@/components/model-picker";
import { MediaGeneratingCard } from "@/components/message-item";
import { enhancePrompt } from "@/lib/chat-transport";
import { useModelCapabilities } from "@/lib/model-capabilities";
import { useChatStore } from "@/store/chat-store";
import { cn } from "@/lib/utils";

/** 视频会话的三种生成模式（放弃画布后的定稿）：
 *  文本转视频=纯提示词；图像转视频=首帧参照（第一张首帧，可再加一张尾帧）；
 *  视频转视频=素材视频改写。模式写进 videoGen.mode，管线按它装配参照素材的角色 */
type VideoMode = "t2v" | "i2v" | "v2v";

const MODE_TABS: Array<[VideoMode, string]> = [
  ["t2v", "文本转视频"],
  ["i2v", "图像转视频"],
  ["v2v", "视频转视频"],
];

const MODE_TO_WIRE: Record<VideoMode, "omni" | "frames" | "edit"> = {
  t2v: "omni",
  i2v: "frames",
  v2v: "edit",
};

const RATIOS = ["Auto", "16:9", "9:16"] as const;
const DURATIONS = [5, 8, 10] as const;
const RESOLUTIONS = ["480P", "720P", "1080P"] as const;

function formatTime(seconds: number) {
  return `${String(Math.floor(seconds / 60)).padStart(2, "0")}:${String(
    Math.floor(seconds % 60),
  ).padStart(2, "0")}`;
}

export function VideoWorkspace() {
  const messages = useChatStore((s) => s.messages);
  const config = useChatStore((s) => s.config);
  const updateConfig = useChatStore((s) => s.updateConfig);
  const attachments = useChatStore((s) => s.attachments);
  const attachPaths = useChatStore((s) => s.attachPaths);
  const detachAttachment = useChatStore((s) => s.detachAttachment);
  const sendMedia = useChatStore((s) => s.sendMedia);
  const mediaBusy = useChatStore((s) => s.mediaBusy);
  const stopMedia = useChatStore((s) => s.stopMedia);

  // 草稿住 store（按会话各存一份）：切去别的会话再回来，提示词与模式还在原处。
  // 缺省值就地兜底（draft?.mode ?? "t2v"），不存在"草稿必须初始化"这回事
  const draft = useChatStore((s) => s.mediaDrafts[s.activeId]);
  const setMediaDraft = useChatStore((s) => s.setMediaDraft);
  const mode: VideoMode = draft?.mode ?? "t2v";
  const prompt = draft?.prompt ?? "";
  const [enhancing, setEnhancing] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // 模式写进 videoGen.mode：发送时管线按它装配参照素材的角色
  function switchMode(next: VideoMode) {
    setMediaDraft({ mode: next });
    void updateConfig({
      videoGen: { ...config.videoGen, mode: MODE_TO_WIRE[next] },
    });
  }

  // 档位记忆里挂着的模型得真有视频能力才算数：老配置残留/档案没标注的模型
  // 发出去会被端点拒（真机 400 "Model ... does not exist" 就是这么来的）。
  // 在设置区说穿，别等端点报错才让人对着 400 猜
  const videoModel = config.kindModels?.video || "";
  const videoModelCaps = useModelCapabilities(
    videoModel ? { id: videoModel, name: videoModel } : null,
  );
  const videoModelHint = videoModel !== "" && !videoModelCaps.isVideoGeneration;

  const setVideoGen = (patch: Partial<typeof config.videoGen>) =>
    void updateConfig({ videoGen: { ...config.videoGen, ...patch } });

  async function addMaterial(kind: "image" | "video") {
    setError(null);
    try {
      const picked = await open({
        multiple: kind === "image",
        title: kind === "image" ? "选择参照图片" : "选择素材视频",
      });
      if (picked === null) return;
      // 图像转视频最多两张（首帧+可选尾帧），视频转视频一段
      const chosen: string[] = Array.isArray(picked) ? picked : [picked];
      await attachPaths(kind === "image" ? chosen.slice(0, 2) : chosen.slice(0, 1));
    } catch {
      setError("当前环境没有系统文件选择器。");
    }
  }

  async function enhance() {
    const text = prompt.trim();
    if (!text || enhancing) return;
    setEnhancing(true);
    try {
      setMediaDraft({ prompt: await enhancePrompt(text) });
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setEnhancing(false);
    }
  }

  function generate() {
    const text = prompt.trim();
    if (!text || mediaBusy) return;
    setError(null);
    void sendMedia(text, "video").catch((cause) =>
      setError(cause instanceof Error ? cause.message : String(cause)),
    );
  }

  // 本会话的视频产物（老→新），提示词取它前面最近一句用户输入
  const results = useMemo(() => {
    const out: Array<{ path: string; name: string; prompt: string }> = [];
    let lastPrompt = "";
    for (const message of messages) {
      if (message.role === "user" && message.content.trim()) {
        lastPrompt = message.content.trim();
      }
      if (message.role === "assistant") {
        for (const item of message.attachments ?? []) {
          if (item.kind === "video") {
            out.push({ path: item.path, name: item.name, prompt: lastPrompt });
          }
        }
      }
    }
    return out;
  }, [messages]);
  // 预览选中：默认最新一支；新一轮生成自动跟到最新
  const [selectedIndex, setSelectedIndex] = useState(0);
  const safeIndex = results.length > 0 ? Math.min(selectedIndex, results.length - 1) : 0;
  const current = results[safeIndex];
  const generating = messages.some((message) => message.streaming && message.media === "video");
  const lastMessageId = messages[messages.length - 1]?.id;
  useEffect(() => {
    setSelectedIndex(results.length - 1);
  }, [results.length, lastMessageId]);

  const imageRefs = attachments.filter((item) => item.kind === "image");
  const videoRef = attachments.find((item) => item.kind === "video");
  const chip = (on: boolean) =>
    cn(
      "flex-1 rounded-lg border px-2 py-1.5 text-xs outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
      on
        ? "border-brand/45 bg-brand/10 text-brand-text"
        : "border-border text-muted-foreground hover:bg-accent hover:text-foreground",
    );

  return (
    <div className="flex min-h-0 min-w-0 flex-1">
      {/* 左：创作面板（模式/参照/提示词/设置/生成） */}
      <aside className="flex w-[380px] shrink-0 flex-col overflow-y-auto border-r border-border bg-surface">
        <div className="flex items-center gap-1 border-b border-border px-4 py-2">
          {MODE_TABS.map(([value, label]) => (
            <button
              key={value}
              type="button"
              aria-pressed={mode === value}
              onClick={() => switchMode(value)}
              className={cn(
                "rounded-md px-2 py-1 text-xs outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
                mode === value
                  ? "bg-brand/10 font-medium text-brand-text"
                  : "text-muted-foreground hover:bg-accent hover:text-foreground",
              )}
            >
              {label}
            </button>
          ))}
        </div>

        <div className="flex min-h-0 flex-1 flex-col gap-4 px-4 py-4">
          {/* 参照素材：图像转视频=首尾帧，视频转视频=素材视频 */}
          {mode !== "t2v" ? (
            <div>
              <p className="mb-1.5 text-xs font-medium text-muted-foreground">
                {mode === "i2v" ? "首帧（可再加一张作尾帧）" : "素材视频"}
              </p>
              <div className="flex flex-wrap items-center gap-2">
                {mode === "i2v" ? (
                  imageRefs.map((item) => (
                    <span
                      key={item.id}
                      className="relative size-16 overflow-hidden rounded-lg border border-border"
                    >
                      <img
                        src={item.previewDataUrl ?? convertFileSrc(item.path)}
                        alt={item.name}
                        className="size-full object-cover"
                      />
                      <button
                        type="button"
                        aria-label={`移除 ${item.name}`}
                        onClick={() => detachAttachment(item.id)}
                        className="absolute right-0.5 top-0.5 flex size-4 items-center justify-center rounded-full bg-background/80 text-foreground"
                      >
                        <X className="size-2.5" />
                      </button>
                    </span>
                  ))
                ) : videoRef ? (
                  <span className="flex items-center gap-1.5 rounded-lg border border-border bg-elevated px-2 py-1 text-xs text-muted-foreground">
                    <Video className="size-3 shrink-0" />
                    <span className="max-w-[180px] truncate text-foreground">{videoRef.name}</span>
                    <button
                      type="button"
                      aria-label={`移除 ${videoRef.name}`}
                      onClick={() => detachAttachment(videoRef.id)}
                      className="text-muted-foreground hover:text-foreground"
                    >
                      <X className="size-3" />
                    </button>
                  </span>
                ) : null}
                <button
                  type="button"
                  onClick={() => void addMaterial(mode === "i2v" ? "image" : "video")}
                  className={cn(
                    "flex items-center justify-center rounded-lg border border-dashed border-border text-muted-foreground transition-colors hover:border-brand/40 hover:text-brand-text",
                    mode === "i2v" ? "size-16" : "h-10 gap-1.5 px-3 text-xs",
                  )}
                >
                  {mode === "i2v" ? (
                    <ImagePlus className="size-4" />
                  ) : (
                    <>
                      <Video className="size-4" />
                      选择视频
                    </>
                  )}
                </button>
              </div>
              {mode === "i2v" ? (
                <p className="mt-1 text-2xs leading-4 text-muted-foreground">
                  第一张作首帧驱动画面，可再加一张作尾帧过渡。
                </p>
              ) : null}
            </div>
          ) : null}

          {/* 提示词 */}
          <div>
            <div className="mb-1.5 flex items-center justify-between">
              <p className="text-xs font-medium text-muted-foreground">提示词</p>
              <button
                type="button"
                aria-label="AI 提示词"
                title="用当前对话模型把这句话扩写成更专业的视频提示词"
                disabled={!prompt.trim() || enhancing}
                onClick={() => void enhance()}
                className="flex items-center gap-1 rounded px-1.5 py-0.5 text-2xs text-muted-foreground outline-none transition-colors hover:bg-accent hover:text-foreground disabled:pointer-events-none disabled:opacity-40"
              >
                <Sparkles className={cn("size-3", enhancing && "animate-pulse")} />
                AI 提示词
              </button>
            </div>
            <textarea
              value={prompt}
              rows={5}
              aria-label="视频提示词"
              placeholder={
                mode === "t2v"
                  ? "描述你想要的视频：主体、镜头运动、氛围……"
                  : mode === "i2v"
                    ? "描述首帧之后发生什么——动作、运镜、节奏"
                    : "描述要怎么改写这段素材视频"
              }
              className="w-full resize-none rounded-lg border border-input bg-background px-3 py-2 text-sm leading-6 text-foreground outline-none transition-colors placeholder:text-muted-foreground/60 focus-visible:border-brand/50 focus-visible:ring-2 focus-visible:ring-ring/35"
              onChange={(event) => setMediaDraft({ prompt: event.target.value })}
            />
          </div>

          {/* 设置 */}
          <div className="space-y-3">
            <p className="text-xs font-medium text-muted-foreground">设置</p>
            <div>
              <p className="mb-1 text-2xs text-muted-foreground">模型</p>
              <ModelPicker />
              {videoModelHint ? (
                <p className="mt-1 text-2xs leading-4 text-amber-600 dark:text-amber-400">
                  当前模型未标注视频生成能力——发出去多半被端点 400
                  拒收，请换模型或在服务商档案里标注。
                </p>
              ) : null}
            </div>
            <div>
              <p className="mb-1 text-2xs text-muted-foreground">比例</p>
              <div className="flex gap-1.5">
                {RATIOS.map((item) => (
                  <button
                    key={item}
                    type="button"
                    aria-pressed={config.videoGen.ratio === item}
                    onClick={() => setVideoGen({ ratio: item })}
                    className={chip(config.videoGen.ratio === item)}
                  >
                    {item}
                  </button>
                ))}
              </div>
            </div>
            <div>
              <p className="mb-1 text-2xs text-muted-foreground">时长</p>
              <div className="flex gap-1.5">
                {DURATIONS.map((item) => (
                  <button
                    key={item}
                    type="button"
                    aria-pressed={config.videoGen.duration === item}
                    onClick={() => setVideoGen({ duration: item })}
                    className={chip(config.videoGen.duration === item)}
                  >
                    {item}s
                  </button>
                ))}
              </div>
            </div>
            <div>
              <p className="mb-1 text-2xs text-muted-foreground">分辨率</p>
              <div className="flex gap-1.5">
                {RESOLUTIONS.map((item) => (
                  <button
                    key={item}
                    type="button"
                    aria-pressed={config.videoGen.resolution === item}
                    onClick={() => setVideoGen({ resolution: item })}
                    className={chip(config.videoGen.resolution === item)}
                  >
                    {item}
                  </button>
                ))}
              </div>
            </div>
          </div>

          {error ? <p className="text-xs leading-5 text-destructive">{error}</p> : null}
        </div>

        <div className="mt-auto border-t border-border p-4">
          {mediaBusy ? (
            <Button variant="subtle" className="w-full" onClick={stopMedia}>
              生成中，点击停止…
            </Button>
          ) : (
            <Button variant="brand" className="w-full" disabled={!prompt.trim()} onClick={generate}>
              生成视频
            </Button>
          )}
        </div>
      </aside>

      {/* 右：结果舞台 + 历史画廊 */}
      <div className="flex min-w-0 flex-1 flex-col bg-[#0b0b0d]">
        <div className="flex min-h-0 flex-1 flex-col items-center justify-center gap-3 p-6">
          {generating ? (
            <MediaGeneratingCard
              kind="video"
              startedAt={
                messages.find((message) => message.streaming && message.media === "video")
                  ?.createdAt ?? Date.now()
              }
            />
          ) : current ? (
            <>
              <video
                key={current.path}
                src={convertFileSrc(current.path)}
                controls
                preload="metadata"
                className="max-h-full max-w-full rounded-lg bg-black object-contain shadow-2xl"
              />
              {current.prompt ? (
                <p className="max-w-[80%] text-center text-xs leading-5 text-muted-foreground/80">
                  <span className="font-medium text-muted-foreground">Prompt：</span>
                  {current.prompt}
                </p>
              ) : null}
            </>
          ) : (
            <div className="flex flex-col items-center gap-2 text-center">
              <Video className="size-9 text-muted-foreground/30" strokeWidth={1.25} />
              <p className="text-sm text-muted-foreground">
                在左侧描述灵感并生成——视频会出现在这里。
              </p>
            </div>
          )}
        </div>

        {results.length > 0 ? (
          <div className="shrink-0 border-t border-border/60 p-3">
            <div className="flex gap-2 overflow-x-auto">
              {results
                .map((item, index) => ({ ...item, index }))
                .reverse()
                .map((item) => (
                  <button
                    key={item.path}
                    type="button"
                    onClick={() => setSelectedIndex(item.index)}
                    className={cn(
                      "relative h-16 w-28 shrink-0 overflow-hidden rounded-lg border bg-black outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
                      item.index === safeIndex
                        ? "border-brand/60"
                        : "border-transparent hover:border-brand/40",
                    )}
                  >
                    <video
                      src={convertFileSrc(item.path)}
                      preload="metadata"
                      muted
                      className="size-full object-cover"
                    />
                    {item.prompt ? (
                      <span className="absolute inset-x-0 bottom-0 truncate bg-background/70 px-1 text-left text-2xs text-foreground/80">
                        {item.prompt}
                      </span>
                    ) : null}
                  </button>
                ))}
            </div>
          </div>
        ) : null}
      </div>
    </div>
  );
}

/** 舞台的时长显示占位（自绘控制条暂缓：原生 controls 已够用，接口留着） */
void formatTime;
