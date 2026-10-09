import { useMemo, useState } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import { IconMusic as Music, IconSparkles as Sparkles } from "@tabler/icons-react";

import { Button } from "@/components/ui/button";
import { ModelPicker } from "@/components/model-picker";
import { MediaGeneratingCard } from "@/components/message-item";
import { mediaGenerate } from "@/lib/chat-transport";
import { useModelCapabilities } from "@/lib/model-capabilities";
import { useChatStore } from "@/store/chat-store";
import { cn } from "@/lib/utils";

/** 音乐会话的工作区（Suno 式）：左创作面板（简单模式=描述成曲｜自定义模式=风格+歌词｜
 *  生成歌词=让对话模型先把歌词写出来）、模型行、生成按钮；右侧制作记录（本会话的
 *  成曲列表，就地播放）。只挂 kind = "music"；其余三档完全不走这里 */
type SubMode = "simple" | "custom" | "write";

const EMPTY_SONG_HINT = "在左侧描述风格与主题并生成——成曲会出现在这里。";

export function MusicWorkspace() {
  const messages = useChatStore((s) => s.messages);
  const config = useChatStore((s) => s.config);
  const sendMedia = useChatStore((s) => s.sendMedia);
  const mediaBusy = useChatStore((s) => s.mediaBusy);
  const stopMedia = useChatStore((s) => s.stopMedia);
  const pushToast = useChatStore((s) => s.pushToast);
  // 子模式的真相在 store：模型选择器按它过滤（生成歌词页签=对话模型，其余=音频模型）
  const subMode = useChatStore((s) => s.musicSubMode);
  const setSubMode = useChatStore((s) => s.setMusicSubMode);
  // 草稿住 store（按会话各存一份）：切去别的会话再回来，写了一半的歌词还在原处。
  // writing/error 是瞬时态，留本地；draft 可能还没有任何字段，逐个就地兜底
  const draft = useChatStore((s) => s.mediaDrafts[s.activeId]);
  const setMediaDraft = useChatStore((s) => s.setMediaDraft);
  const description = draft?.description ?? "";
  const style = draft?.style ?? "";
  const lyrics = draft?.lyrics ?? "";
  const instrumental = draft?.instrumental ?? false;
  const [writing, setWriting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const generating = messages.some((message) => message.streaming && message.media === "music");
  // 本会话的成曲（老→新），标题取它前面最近一句用户输入
  const songs = useMemo(() => {
    const out: Array<{ path: string; name: string; prompt: string }> = [];
    let lastPrompt = "";
    for (const message of messages) {
      if (message.role === "user" && message.content.trim()) {
        lastPrompt = message.content.trim();
      }
      if (message.role === "assistant") {
        for (const item of message.attachments ?? []) {
          if (item.kind === "audio") {
            out.push({ path: item.path, name: item.name, prompt: lastPrompt });
          }
        }
      }
    }
    return out;
  }, [messages]);

  // 音乐模型行走 kindModels.music（选择器选中即写）；写词用对话模型行
  const chatModel = config.kindModels?.chat || config.model;

  // 档位记忆里挂着的模型得真有对应能力才算数（写词=对话、其余=音频生成）：
  // 老配置残留/档案没标注的模型发出去会被端点拒，在模型行下说穿，别等 400
  const wantedModel = subMode === "write" ? chatModel : config.kindModels?.music || "";
  const wantedCaps = useModelCapabilities(
    wantedModel ? { id: wantedModel, name: wantedModel } : null,
  );
  const musicModelHint =
    wantedModel !== "" &&
    !(subMode === "write" ? wantedCaps.isChatModel : wantedCaps.isAudioGeneration);

  function generate() {
    if (mediaBusy) return;
    const text =
      subMode === "custom"
        ? style.trim() ||
          lyrics
            .split("\n")
            .find((line) => line.trim())
            ?.trim() ||
          ""
        : description.trim();
    if (!text) {
      setError(subMode === "custom" ? "先写点风格描述或歌词。" : "先描述一下歌曲的主题和风格。");
      return;
    }
    setError(null);
    void sendMedia(
      text,
      "music",
      subMode === "custom" ? { lyrics, instrumental } : { instrumental },
    ).catch((cause) => setError(cause instanceof Error ? cause.message : String(cause)));
  }

  // 生成歌词：一次性对话模型调用，产物直接填进自定义模式的歌词框
  async function writeLyrics() {
    const text = description.trim();
    if (!text || writing) return;
    setWriting(true);
    setError(null);
    try {
      // 写词走对话模型，而对话模型可能住在别的服务商：按模型名解析所属档案
      // （激活档案优先），把连接域路由过去——否则请求会打到当前连接上
      // （真机踩过：图片站的连接收到 chat/completions 直接 403）
      const activeProfile = config.profiles.find(
        (profile) => profile.id === config.activeProfileId,
      );
      const profileId = activeProfile?.models.some((spec) => spec.model === chatModel)
        ? activeProfile.id
        : config.profiles.find((profile) => profile.models.some((spec) => spec.model === chatModel))
            ?.id;
      const result = await mediaGenerate(
        "text",
        `为下面这首歌写一份完整歌词，按段换行，直接输出歌词正文：${text}`,
        {},
        [],
        chatModel,
        undefined,
        profileId,
      );
      if (result.text) {
        setMediaDraft({ lyrics: result.text });
        setSubMode("custom");
        pushToast({ tone: "info", title: "歌词已生成，可在自定义模式里修改" });
      }
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setWriting(false);
    }
  }

  return (
    <div className="flex min-h-0 min-w-0 flex-1">
      {/* 左：创作面板 */}
      <aside className="flex w-[400px] shrink-0 flex-col overflow-y-auto border-r border-border bg-surface">
        <div className="flex items-center gap-1 border-b border-border px-4 py-2">
          {(
            [
              ["simple", "简单模式"],
              ["custom", "自定义模式"],
              ["write", "生成歌词"],
            ] as Array<[SubMode, string]>
          ).map(([value, label]) => (
            <button
              key={value}
              type="button"
              aria-pressed={subMode === value}
              onClick={() => setSubMode(value)}
              className={cn(
                "flex-1 rounded-md px-2 py-1 text-xs outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
                subMode === value
                  ? "bg-brand/10 font-medium text-brand-text"
                  : "text-muted-foreground hover:bg-accent hover:text-foreground",
              )}
            >
              {label}
            </button>
          ))}
        </div>

        <div className="flex min-h-0 flex-1 flex-col gap-4 px-4 py-4">
          {/* 歌曲描述：三个子模式共用 */}
          <div>
            <div className="mb-1.5 flex items-center justify-between">
              <p className="text-xs font-medium text-muted-foreground">
                {subMode === "write" ? "想要什么样的歌词" : "歌曲描述"}
              </p>
              {subMode !== "write" ? (
                <label className="flex cursor-pointer items-center gap-1.5 text-2xs text-muted-foreground">
                  纯音乐
                  <input
                    type="checkbox"
                    checked={instrumental}
                    className="size-3.5 accent-brand"
                    onChange={(event) => setMediaDraft({ instrumental: event.target.checked })}
                  />
                </label>
              ) : null}
            </div>
            <textarea
              value={description}
              rows={5}
              aria-label="歌曲描述"
              placeholder="描述你想要的歌曲主题和风格，例如：一首简单的情歌"
              className="w-full resize-none rounded-lg border border-input bg-background px-3 py-2 text-sm leading-6 text-foreground outline-none transition-colors placeholder:text-muted-foreground/60 focus-visible:border-brand/50 focus-visible:ring-2 focus-visible:ring-ring/35"
              onChange={(event) => setMediaDraft({ description: event.target.value })}
            />
            {subMode === "write" ? (
              <Button
                variant="subtle"
                size="sm"
                className="mt-2 w-full"
                disabled={!description.trim() || writing}
                onClick={() => void writeLyrics()}
              >
                <Sparkles className={cn("size-3.5", writing && "animate-pulse")} />
                {writing ? "写词中…" : "让对话模型生成歌词"}
              </Button>
            ) : null}
          </div>

          {/* 自定义模式（或生成歌词填好后）的歌词框 */}
          {subMode === "custom" ? (
            <div>
              <p className="mb-1.5 text-xs font-medium text-muted-foreground">风格描述</p>
              <textarea
                value={style}
                rows={2}
                aria-label="风格描述"
                placeholder="曲风、节奏、情绪——例如：慢板 72 BPM 的钢琴独奏"
                className="mb-3 w-full resize-none rounded-lg border border-input bg-background px-3 py-2 text-sm leading-6 text-foreground outline-none transition-colors placeholder:text-muted-foreground/60 focus-visible:border-brand/50 focus-visible:ring-2 focus-visible:ring-ring/35"
                onChange={(event) => setMediaDraft({ style: event.target.value })}
              />
              <p className="mb-1.5 text-xs font-medium text-muted-foreground">歌词</p>
              <textarea
                value={lyrics}
                rows={8}
                aria-label="歌词"
                placeholder={"输入歌词——按段换行，副歌可以重复"}
                className="w-full resize-none rounded-lg border border-input bg-background px-3 py-2 text-sm leading-6 text-foreground outline-none transition-colors placeholder:text-muted-foreground/60 focus-visible:border-brand/50 focus-visible:ring-2 focus-visible:ring-ring/35"
                onChange={(event) => setMediaDraft({ lyrics: event.target.value })}
              />
            </div>
          ) : null}

          {/* 模型 */}
          <div>
            <p className="mb-1 text-2xs text-muted-foreground">模型</p>
            <ModelPicker />
            {musicModelHint ? (
              <p className="mt-1 text-2xs leading-4 text-amber-600 dark:text-amber-400">
                当前模型未标注{subMode === "write" ? "对话" : "音频"}能力——发出去多半被端点 400
                拒收，请换模型或在服务商档案里标注。
              </p>
            ) : null}
          </div>

          {error ? <p className="text-xs leading-5 text-destructive">{error}</p> : null}
        </div>

        <div className="mt-auto border-t border-border p-4">
          {mediaBusy ? (
            <Button variant="subtle" className="w-full" onClick={stopMedia}>
              生成中，点击停止…
            </Button>
          ) : (
            <Button variant="brand" className="w-full" onClick={generate}>
              <Music className="size-4" />
              生成歌曲
            </Button>
          )}
        </div>
      </aside>

      {/* 右：制作记录 */}
      <div className="flex min-w-0 flex-1 flex-col bg-[#0b0b0d]">
        <p className="shrink-0 px-5 pt-4 text-xs font-medium tracking-wide text-muted-foreground">
          制作记录
        </p>
        <div className="min-h-0 flex-1 overflow-y-auto p-4">
          {generating ? <MediaGeneratingCard kind="music" startedAt={Date.now() - 1000} /> : null}
          {songs.length === 0 && !generating ? (
            <div className="flex flex-col items-center gap-2 pt-16 text-center">
              <Music className="size-9 text-muted-foreground/30" strokeWidth={1.25} />
              <p className="max-w-[60%] text-sm leading-6 text-muted-foreground">
                {EMPTY_SONG_HINT}
              </p>
            </div>
          ) : (
            <div className="space-y-3">
              {[...songs].reverse().map((song) => (
                <div
                  key={song.path}
                  className="flex items-center gap-3 rounded-lg border border-border/60 bg-elevated/60 px-3 py-2.5"
                >
                  <span className="flex size-10 shrink-0 items-center justify-center rounded-lg bg-brand/10">
                    <Music className="size-4 text-brand" />
                  </span>
                  <div className="min-w-0 flex-1">
                    <p className="truncate text-sm text-foreground">{song.prompt}</p>
                    <p className="truncate text-2xs text-muted-foreground/70">{song.name}</p>
                  </div>
                  <audio
                    controls
                    preload="metadata"
                    src={convertFileSrc(song.path)}
                    className="h-8 w-48 shrink-0"
                  />
                </div>
              ))}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
