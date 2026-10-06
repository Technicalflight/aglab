import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open, save } from "@tauri-apps/plugin-dialog";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { IconArrowUp as ArrowUp, IconCheck as Check, IconChevronDown as ChevronDown, IconCornerDownLeft as CornerDownLeft, IconFileText as FileText, IconFolder as FolderIcon, IconFolderPlus as FolderPlus, IconGitBranch as GitBranch, IconPhoto as ImageIcon, IconLink as Link2, IconMovie as Movie, IconPaperclip as Paperclip, IconPlus as Plus, IconSparkles as Sparkles, IconSquare as Square, IconX as X } from "@tabler/icons-react";

import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import { Menu, MenuContent, MenuItem, MenuLabel, MenuSeparator, MenuTrigger } from "@/components/ui/menu";
import { ContextUsageRing } from "@/components/context-usage-ring";
import { ModelPicker } from "@/components/model-picker";
import { ImageGenSettingsPopover } from "@/components/image-gen-settings";
import { VideoGenSettingsPopover } from "@/components/video-gen-settings";
import { useModelCapabilities } from "@/lib/model-capabilities";
import { ModePicker, PlanApprovalBar } from "@/components/mode-picker";
import { GoalStrip } from "@/components/goal-strip";
import { PlanCard } from "@/components/plan-card";
import { AskCard } from "@/components/ask-card";
import { PermissionPicker } from "@/components/permission-picker";
import { ProjectPicker } from "@/components/project-picker";
import { enhancePrompt, fetchUrlText, saveClipboardImage, slashCommandsList, filesSuggest, type SlashCommand, type FileSuggest } from "@/lib/chat-transport";
import { parseSlashDraft, slashMenuOpen, expandSlashTemplate, parseMention } from "@/lib/slash";
import { imageOutlook } from "@/lib/vision";
import { extractUrls, hostOfUrl } from "@/lib/links";
import { formatTokensCompact } from "@/lib/format";
import { cn } from "@/lib/utils";
import { useChatStore } from "@/store/chat-store";
import type { MediaType } from "@/types/chat";
import { ContentColumn } from "@/components/ui/content-column";

/** FileReader 的 Promise 壳。data URL 一鱼两吃：纯 base64 给 Rust 落盘，
 *  整串给 chip 当缩略图（只在内存里活着，发送后附件清空） */
function readAsDataUrl(file: Blob): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(String(reader.result));
    reader.onerror = () => reject(reader.error ?? new Error("读不到剪贴板里的图片"));
    reader.readAsDataURL(file);
  });
}

/** 视频会话的模板骨架（参照即梦的"短剧漫画/"前缀设计）：
 *  点按把结构化骨架填进输入框，用户照着【】逐格填——
 *  生成模型吃的是完整描述，骨架替用户把镜头/时长/风格这些维度想全 */
const VIDEO_TEMPLATES: Array<{ label: string; scaffold: string }> = [
  {
    label: "产品展示",
    scaffold:
      "【产品】…\n【镜头】特写开场 → 环绕展示 → 定格收尾\n【时长】5 秒\n【风格】明亮商业感，干净背景",
  },
  {
    label: "城市漫游",
    scaffold:
      "【场景】…\n【镜头】跟拍视角，缓推\n【时长】8 秒\n【风格】电影感调色，黄昏暖光",
  },
  {
    label: "剧情短片",
    scaffold:
      "【剧本】一句话梗概…\n【分镜】1) … 2) … 3) …\n【时长】10 秒\n【风格】叙事感，浅景深",
  },
];

/** Worktree 开关 + 分支选择。绑定后这一场话题的文件工具全落在独立工作树上，
 *  原工作目录一个字节不动；摘树时分支与提交保留，脏树要点两次（第二次强制）。 */
function WorktreeControl() {
  const activeId = useChatStore((s) => s.activeId);
  const activeProjectId = useChatStore((s) => s.config.activeProjectId);
  const gitBranches = useChatStore((s) => s.gitBranches);
  const bound = useChatStore((s) => s.worktrees[activeId] ?? null);
  const attachWorktree = useChatStore((s) => s.attachWorktree);
  const detachWorktree = useChatStore((s) => s.detachWorktree);
  const refreshWorktreeBranches = useChatStore((s) => s.refreshWorktreeBranches);
  const pushToast = useChatStore((s) => s.pushToast);
  const [picked, setPicked] = useState("");
  const forceRef = useRef(false);

  // 话题切换或工作目录切换后分支清单都要重取：清单跟"这场话题生效的仓库"走
  // （话题绑定项目优先，散对话回落激活项目）；绑定态由 refreshWorktree 在切话题时读
  useEffect(() => {
    void refreshWorktreeBranches();
  }, [activeId, activeProjectId, refreshWorktreeBranches]);

  // 没绑 git 工作目录（或压根没绑工作目录）时这两个控件没有意义：整组隐藏，别摆死按钮
  if (!activeId || !gitBranches?.isRepo) return null;

  // unborn 分支：选择器上显示的名字只是 HEAD 的意向（init 后没提交过），
  // 如实标注"未提交"，不然勾上 Worktree 只会吃到一句莫名的报错。
  // 后缀刻意短：这条工具条上每个字都在挤同一行
  const branchLabel = bound
    ? bound.branch
    : gitBranches.unborn
      ? `${gitBranches.current || "HEAD"}（未提交）`
      : picked || gitBranches.current || "选分支";

  async function toggle() {
    if (!activeId) return;
    if (bound) {
      const error = await detachWorktree(activeId, forceRef.current);
      if (error) {
        // 第一次点通常是因为树上有未提交的改动：把"再点一次 = 强制"告知到位
        forceRef.current = true;
        pushToast({ tone: "error", title: "Worktree 没有摘掉", detail: error });
      } else {
        forceRef.current = false;
      }
      return;
    }
    const error = await attachWorktree(activeId, picked || undefined);
    if (error) pushToast({ tone: "error", title: "Worktree 没有挂上", detail: error });
  }

  return (
    <div className="flex items-center gap-0.5">
      {/* 打开菜单的瞬间重取一次分支读数：label 可能已经过期（比如 AI 刚提交完），
          但菜单里的选项必须是真的——过期账本不能当操作依据 */}
      <Menu onOpenChange={(open) => open && void refreshWorktreeBranches()}>
        <MenuTrigger
          type="button"
          aria-label="选择 Worktree 基于的分支"
          disabled={!!bound}
          title={
            bound
              ? `分支已由 Worktree 定为「${bound.branch}」（基于「${bound.baseBranch}」）`
              : "选择 Worktree 基于哪个分支"
          }
          className="flex h-8 w-max max-w-40 shrink-0 items-center gap-1 rounded-lg px-2 text-muted-foreground outline-none transition-colors hover:bg-accent hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45 disabled:pointer-events-none"
        >
          <GitBranch className="size-3.5 shrink-0" />
          <span className="min-w-0 truncate text-sm">{branchLabel}</span>
          {bound ? null : <ChevronDown className="size-3 shrink-0" />}
        </MenuTrigger>
        <MenuContent className="max-h-64 overflow-y-auto">
          <MenuLabel>基于哪个分支</MenuLabel>
          {gitBranches.branches.length === 0 ? (
            <MenuItem disabled>
              {gitBranches.unborn
                ? "仓库还没有任何提交——先提交一次，再来开 Worktree"
                : "这个仓库还没有分支"}
            </MenuItem>
          ) : (
            gitBranches.branches.map((name) => (
              <MenuItem key={name} onSelect={() => setPicked(name)}>
                {name === gitBranches.current ? `${name}（当前）` : name}
              </MenuItem>
            ))
          )}
        </MenuContent>
      </Menu>

      <button
        type="button"
        aria-pressed={!!bound}
        onClick={() => void toggle()}
        title={
          bound
            ? `本话题运行在独立工作树上：\n${bound.dir}\n分支 ${bound.branch}（未提交改动 ${bound.changedFiles} 个文件）。点击摘除工作树，分支与提交都保留。`
            : "启用后，aglab 会基于所选分支为本次话题创建独立工作目录，避免直接修改原工作目录中的文件。"
        }
        className={cn(
          "flex h-8 shrink-0 items-center gap-1.5 whitespace-nowrap rounded-lg px-2 outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
          bound ? "text-foreground" : "text-muted-foreground hover:bg-accent hover:text-foreground",
        )}
      >
        <span
          className={cn(
            "flex size-3.5 shrink-0 items-center justify-center rounded border transition-colors",
            bound ? "border-brand bg-brand text-brand-foreground" : "border-muted-foreground/60",
          )}
        >
          {bound ? <Check className="size-2.5" /> : null}
        </span>
        <span className="text-sm">Worktree</span>
      </button>
    </div>
  );
}

export function Composer() {
  const [draft, setDraft] = useState("");
  const [enhancing, setEnhancing] = useState(false);
  const pushToast = useChatStore((s) => s.pushToast);
  const [pickError, setPickError] = useState<string | null>(null);
  const [pasteBusy, setPasteBusy] = useState(false);
  /** 发送时正在抓取的链接主机名（null = 没在抓）。抓取完成前发送按钮不亮 */
  const [fetchingHosts, setFetchingHosts] = useState<string[] | null>(null);
  /** 本组件生命周期里已抓过正文的链接：draft 里的 URL 抓完就不再重复提示/重抓 */
  const fetchedUrls = useRef(new Set<string>());
  const pending = useChatStore((s) => s.pending);
  const mediaBusy = useChatStore((s) => s.mediaBusy);
  const sendMedia = useChatStore((s) => s.sendMedia);
  const stopMedia = useChatStore((s) => s.stopMedia);
  // 会话的能力档：占位词与能力徽标随它切换
  const kind = useChatStore((s) => s.kind);
  const config = useChatStore((s) => s.config);
  const updateConfig = useChatStore((s) => s.updateConfig);
  // 四类页签的真相在 store：模型选择器按它过滤候选、选中后写进对应类型的模型行
  const generationType = useChatStore((s) => s.videoGenerationType);
  const setGenerationType = useChatStore((s) => s.setVideoGenerationType);
  // 音频页签的子模式：语音合成｜语音转写｜音乐生成（Mureka/Sunu 族）。会话级 UI，不落盘
  const [audioMode, setAudioMode] = useState<"tts" | "transcribe" | "music">("tts");
  // 音乐生成的歌词设置：自动生成｜自定义歌词｜纯乐器
  const [lyricsMode, setLyricsMode] = useState<"auto" | "custom" | "instrumental">("auto");
  const [lyrics, setLyrics] = useState("");
  // 当前模型的识别能力（声明优先、名字回退）：徽标据此提示模型能不能生成。
  // 视频会话跟着四类页签走——徽标说的是"这一发生成用的模型"行不行
  const activeModel = useChatStore((s) => {
    if (s.kind !== "video") return s.config.model;
    const key = s.videoGenerationType === "text" ? "chat" : s.videoGenerationType;
    return s.config.kindModels?.[key] || s.config.model;
  });
  const activeModelSpec = useChatStore((s) => {
    const name =
      s.kind !== "video"
        ? s.config.model
        : s.config.kindModels?.[s.videoGenerationType === "text" ? "chat" : s.videoGenerationType] ||
          s.config.model;
    return s.config.models.find((spec) => spec.model === name);
  });
  const attachments = useChatStore((s) => s.attachments);
  // 图发不发得出去要在发之前就说：等到模型回一句"我看不到图"再发现，那一问的钱已经花了
  const outlook = useChatStore((s) => imageOutlook(s.config));
  const attachPaths = useChatStore((s) => s.attachPaths);
  // 当前模型的识别能力（声明优先、名字回退）——生图/视频会话的徽标据此变色。
  // 视频会话的徽标跟随四类页签：文本页签看对话能力、音频页签看音频能力
  const activeModelCaps = useModelCapabilities({
    id: activeModel,
    name: activeModel,
    capabilities: activeModelSpec?.capabilities,
  });
  const activeModelMissing =
    kind === "image"
      ? !activeModelCaps.isImageGeneration
      : kind === "video"
        ? generationType === "text"
          ? !activeModelCaps.isChatModel
          : generationType === "image"
            ? !activeModelCaps.isImageGeneration
            : generationType === "audio"
              ? !activeModelCaps.isAudioGeneration
              : !activeModelCaps.isVideoGeneration
        : false;
  const attachImage = useChatStore((s) => s.attachImage);
  const detachAttachment = useChatStore((s) => s.detachAttachment);
  const setProjectDialogOpen = useChatStore((s) => s.setProjectDialogOpen);
  const send = useChatStore((s) => s.send);
  const stopGeneration = useChatStore((s) => s.stopGeneration);
  const steer = useChatStore((s) => s.steer);
  const followUp = useChatStore((s) => s.followUp);
  const followUpCount = useChatStore((s) => s.followUpCount);
  const draftRestore = useChatStore((s) => s.draftRestore);
  const clearDraftRestore = useChatStore((s) => s.clearDraftRestore);
  const startConversation = useChatStore((s) => s.startConversation);
  const compactConversation = useChatStore((s) => s.compactConversation);
  const setSection = useChatStore((s) => s.setSection);
  const textareaRef = useRef<HTMLTextAreaElement | null>(null);

  // ---- slash 命令 ----
  const [slashCommands, setSlashCommands] = useState<SlashCommand[]>([]);
  const [slashIndex, setSlashIndex] = useState(0);
  /** Esc 之后菜单让位，但展开规则还在：直接回车照样按已知命令展开 */
  const [slashDismissed, setSlashDismissed] = useState(false);
  const slashMatch = useMemo(() => parseSlashDraft(draft), [draft]);
  // 草稿进入 slash 形状（敲的是命令名，还没空格进参数区）。可见性与取数都认它
  const slashShape = !pending && slashMatch !== null && slashMenuOpen(draft);
  const filteredCommands = useMemo(() => {
    const query = slashMatch?.name.toLowerCase() ?? "";
    return slashCommands
      .filter((item) => item.name.toLowerCase().startsWith(query))
      .slice(0, 12);
  }, [slashCommands, slashMatch]);
  const slashMenuVisible = slashShape && !slashDismissed && filteredCommands.length > 0;

  // 草稿一进入 slash 形状就拉清单——**不能等菜单可见再拉**：可见性要拿清单算
  // 过滤结果，先拉清单还是先判可见互相等，第一次就永远弹不出来。
  // 每次重新进入 slash 形状都重拉：commands 目录是用户手改的，缓存的菜单会撒谎
  const slashFetched = useRef(false);
  useEffect(() => {
    if (!slashShape) {
      slashFetched.current = false;
      return;
    }
    if (slashFetched.current) return;
    slashFetched.current = true;
    slashCommandsList()
      .then(setSlashCommands)
      .catch((error) => {
        // 后端没这条命令（旧二进制 + 新前端）时菜单会静默缺席：留一行痕迹好排查
        console.warn("slash 命令清单没拉到", error);
        setSlashCommands([]);
      });
  }, [slashShape]);

  // 换了命令名前缀就回到第一项：过滤结果变了，旧的高亮没有意义
  useEffect(() => {
    setSlashIndex(0);
  }, [slashMatch?.name]);

  // ---- @-文件提及 ----
  const [mention, setMention] = useState<{ start: number; query: string } | null>(null);
  const [suggestions, setSuggestions] = useState<FileSuggest[]>([]);
  const [mentionIndex, setMentionIndex] = useState(0);

  useEffect(() => {
    if (!mention) return;
    const timer = setTimeout(() => {
      filesSuggest(mention.query)
        .then((items) => setSuggestions(items))
        .catch(() => setSuggestions([]));
    }, 150);
    return () => clearTimeout(timer);
  }, [mention]);

  useEffect(() => {
    setMentionIndex(0);
  }, [mention?.query]);

  // ---- 拖拽附件：整个窗口都是投放区，落下的路径走既有附件链路 ----
  const [dragOver, setDragOver] = useState(false);
  useEffect(() => {
    const unlisten = getCurrentWebview().onDragDropEvent((event) => {
      if (event.payload.type === "over") {
        setDragOver(true);
      } else if (event.payload.type === "leave") {
        setDragOver(false);
      } else if (event.payload.type === "drop") {
        setDragOver(false);
        if (event.payload.paths.length > 0) void attachPaths(event.payload.paths);
      }
    });
    return () => {
      void unlisten.then((dispose) => dispose());
    };
  }, [attachPaths]);

  const chooseSlash = (item: SlashCommand) => {
    setSlashDismissed(false);
    if (item.prompt) {
      // 带 prompt 的（内置 /init、自定义命令）：留在草稿里让用户补参数，回车时展开
      setDraft(`/${item.name} `);
      textareaRef.current?.focus();
      return;
    }
    if (item.action) {
      setDraft("");
      runBuiltinAction(item.action);
    }
  };

  const runBuiltinAction = (action: string) => {
    if (action === "new") startConversation();
    else if (action === "compact") void compactConversation();
    else if (action === "export") void exportConversationMarkdown();
    else if (action === "review") setSection("review");
    else if (action === "tasks") setSection("tasks");
    else if (action === "knowledge") setSection("knowledge");
    else if (action === "plugins") setSection("plugins");
    else if (action === "usage" || action === "settings") setSection("settings");
  };

  const chooseMention = (item: FileSuggest) => {
    if (!mention) return;
    setDraft((previous) => {
      const caret = textareaRef.current?.selectionStart ?? previous.length;
      const before = previous.slice(0, mention.start);
      const after = previous.slice(Math.max(caret, mention.start + 1 + mention.query.length));
      return `${before}@${item.rel} ${after}`;
    });
    setMention(null);
    setSuggestions([]);
    if (!item.isDir) void attachPaths([item.abs]);
    textareaRef.current?.focus();
  };

  /** /export 的执行体：与 message-item 的导出同一份格式。放这里是因为
   *  slash 菜单要触发它，而那条动作行在消息尾部——两边都不该搬着一份格式走 */
  async function exportConversationMarkdown() {
    const state = useChatStore.getState();
    const lines: string[] = [`# ${state.title}`, ""];
    for (const item of state.messages) {
      if (item.summary) {
        lines.push(`> ${item.content}`, "");
        continue;
      }
      const label = item.role === "user" ? "**用户**" : `**${item.model ?? "助手"}**`;
      lines.push(label, "", item.content, "");
    }
    const path = await save({
      defaultPath: `${state.title || "对话"}.md`,
      filters: [{ name: "Markdown", extensions: ["md"] }],
    });
    if (!path) return;
    try {
      await invoke("usage_export_csv", { path, content: lines.join("\n") });
      pushToast({ tone: "info", title: "话题已导出为 Markdown" });
    } catch (error) {
      pushToast({
        tone: "error",
        title: "导出失败",
        detail: error instanceof Error ? error.message : String(error),
      });
    }
  }

  // 插队/排队没落进在跑的回合时，后端报错、store 把那句话放进还话槽：
  // 这里接回草稿，让用户再按一次发送——话不能因为竞态就蒸发
  useEffect(() => {
    if (draftRestore === null) return;
    setDraft(draftRestore);
    clearDraftRestore();
  }, [draftRestore, clearDraftRestore]);

  const pendingLinks = useMemo(
    () => extractUrls(draft).filter((url) => !fetchedUrls.current.has(url)),
    [draft],
  );

  // 生成中输入不再被锁死：回车把这句话插队进正在跑的任务（pi 的 steering），
  // Ctrl+回车则排进跟随队列——这一轮收尾后自动作为新输入开下一轮。
  // 生图/视频的生成跑着时整条锁住：生成没有"插队"可言，等它完
  const canSend =
    (draft.trim().length > 0 || attachments.length > 0) &&
    !pasteBusy &&
    !fetchingHosts &&
    !mediaBusy;

  // 增强提示词：把输入框草稿交给当前生效的模型改写，结果整体替换草稿。
  // 增强中按钮转圈置灰；原文不保留，模型改写失败时报错且草稿原样不动
  async function enhanceDraft() {
    if (enhancing) return;
    const text = draft.trim();
    if (!text) return;
    setEnhancing(true);
    try {
      const enhanced = await enhancePrompt(text);
      setDraft(enhanced);
      pushToast({ tone: "info", title: "提示词已增强" });
    } catch (cause) {
      pushToast({
        tone: "error",
        title: "提示词增强失败",
        detail: cause instanceof Error ? cause.message : String(cause),
      });
    } finally {
      setEnhancing(false);
    }
  }

  function submitFollowUp() {
    const text = draft.trim();
    if (!text) return;
    setDraft("");
    void followUp(text).catch((error) =>
      setPickError(error instanceof Error ? error.message : String(error)),
    );
  }

  async function submit() {
    if (mediaBusy) return;
    if (pending) {
      const text = draft.trim();
      if (!text) return;
      setDraft("");
      void steer(text).catch((error) =>
        setPickError(error instanceof Error ? error.message : String(error)),
      );
      return;
    }
    if (!canSend) return;
    // 生图/视频会话：走独立的生成管线，不进对话轮——slash/链接抓取这些
    // 对话侧的前处理都不适用，提示词原样交给生成接口。
    // 视频画布的四类页签点名这一发生成什么；生图会话恒为图
    if (kind !== "chat") {
      const prompt = draft.trim();
      if (!prompt) return;
      setDraft("");
      void sendMedia(
        prompt,
        kind === "video"
          ? generationType === "audio"
            ? audioMode === "transcribe"
              ? "transcribe"
              : audioMode === "music"
                ? "music"
                : "audio"
            : generationType
          : "image",
        kind === "video" && generationType === "audio" && audioMode === "music"
          ? {
              lyrics: lyricsMode === "custom" ? lyrics : "",
              instrumental: lyricsMode === "instrumental",
            }
          : undefined,
      ).catch((error) =>
        setPickError(error instanceof Error ? error.message : String(error)),
      );
      return;
    }
    let prompt = draft;
    // slash 展开：已知命令（内置提示词 / 自定义模板）在发送那一刻展开，
    // 未知命令按普通文本放行——它可能是路径，不该被吞掉
    const parsed = parseSlashDraft(prompt);
    if (parsed) {
      // 没开过菜单就直敲命令回车的情况：这里现拉一份，不然 /init 会被当普通文本发出去
      let commands = slashCommands;
      if (commands.length === 0) {
        try {
          commands = await slashCommandsList();
          setSlashCommands(commands);
        } catch {
          commands = [];
        }
      }
      const item = commands.find((command) => command.name === parsed.name);
      if (item?.action && !item.prompt) {
        setDraft("");
        runBuiltinAction(item.action);
        return;
      }
      if (item?.prompt) {
        prompt = expandSlashTemplate(item.prompt, parsed.args);
        if (!prompt.trim()) {
          setPickError("命令展开后是空的：补上参数再发，或直接写正文。");
          return;
        }
      }
    }
    setDraft("");
    // 链接抓取（识别链接）：草稿里的 http(s) 链接在发送前抓正文、落盘成文本附件
    // ——正文跟着附件链路进上下文。抓不到不阻塞发送：链接原文还在 prompt 里
    const links = extractUrls(prompt).filter((url) => !fetchedUrls.current.has(url));
    if (links.length > 0) {
      setFetchingHosts(links.map(hostOfUrl));
      for (const url of links) {
        try {
          const fetched = await fetchUrlText(url);
          await attachPaths([fetched.path]);
          fetchedUrls.current.add(url);
        } catch (error) {
          const message = error instanceof Error ? error.message : String(error);
          setPickError(`链接 ${hostOfUrl(url)} 没抓到正文：${message}（已按普通文本发送）`);
        }
      }
      setFetchingHosts(null);
    }
    void send(prompt);
  }

  /** 粘贴截图：Win+Shift+S 之后直接 Ctrl+V，剪贴板里的位图落盘成附件。
   *  只拦图片粘贴——普通文本的粘贴走浏览器默认行为。
   *  坑：DataTransferItem 在 paste 事件**同步段**结束后就失效（type 变空串），
   *  所以 mime 和 file 都必须在第一个 await 之前捕获成局部值 */
  async function handlePaste(event: React.ClipboardEvent) {
    const imageItem = Array.from(event.clipboardData.items).find((item) =>
      item.type.startsWith("image/"),
    );
    if (!imageItem) return;
    const mime = imageItem.type;
    const file = imageItem.getAsFile();
    if (!file) return;
    event.preventDefault();
    setPasteBusy(true);
    setPickError(null);
    try {
      const dataUrl = await readAsDataUrl(file);
      const comma = dataUrl.indexOf(",");
      if (comma < 0) throw new Error("剪贴板图片数据格式不对");
      const saved = await saveClipboardImage(dataUrl.slice(comma + 1), mime);
      attachImage({
        path: saved.path,
        name: saved.name,
        mime: saved.mime,
        bytes: saved.bytes,
        previewDataUrl: dataUrl,
      });
    } catch (error) {
      setPickError(`截图没贴上：${error instanceof Error ? error.message : String(error)}`);
    } finally {
      setPasteBusy(false);
    }
  }

  async function pickFiles() {
    setPickError(null);
    try {
      const picked = await open({ multiple: true, title: "添加到本轮上下文" });
      if (picked === null) return;
      await attachPaths(Array.isArray(picked) ? picked : [picked]);
    } catch {
      setPickError("当前环境没有系统文件选择器。");
    }
  }

  return (
    <div className="shrink-0 px-6 pt-3 pb-5">
      {/* padded={false}：外层已经是 "shrink-0 px-6 pt-3 pb-5" 的贴底条，
          上下留白由它负责。这里若再叠 ContentColumn 默认的 py-6 sm:py-8，
          输入框会被顶到视口外——底部栏最不能缺的就是"永远留在屏上"。
          横向 px 也交给外层，内层只留居中与最大宽度。
          不画 border-t：参考设计里输入框是浮在背景上的，与消息区之间不拉线。 */}
      <ContentColumn padded={false} className="px-0">
        {/* 模型的提问（ask_user）与计划（update_plan）：都长在输入框上方这一列。
            提问卡在最上——它挂着后端一发等人的回合，视觉优先级最高 */}
        <AskCard />
        <PlanCard />

        {/* 目标带：这一支挂着目标就长在输入框上方。它可以和「批准并执行」同屏叠着
            （规划档与"挂着目标"两件事互不排斥），谁也不许为了省位置把对方藏掉 */}
        <GoalStrip />

        <div
          className={cn(
            "relative rounded-lg border border-input bg-surface transition-colors focus-within:border-brand/50",
            dragOver && "border-brand ring-2 ring-brand/30",
          )}
        >
          {/* slash / @ 两个浮层：压在输入框上方，不把布局顶下去 */}
          {slashMenuVisible ? (
            <div className="absolute bottom-[calc(100%-6px)] right-2 left-2 z-overlay max-h-72 overflow-y-auto rounded-lg border border-border bg-elevated py-1 shadow-md">
              {filteredCommands.map((item, index) => (
                <button
                  key={`${item.source}/${item.name}`}
                  type="button"
                  onMouseDown={(event) => {
                    event.preventDefault();
                    chooseSlash(item);
                  }}
                  onMouseEnter={() => setSlashIndex(index)}
                  className={cn(
                    "flex w-full items-baseline gap-2 rounded-sm px-3 py-1.5 text-left text-sm outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring/55",
                    index === slashIndex && "bg-accent",
                  )}
                >
                  <span className="shrink-0 font-mono text-foreground">/{item.name}</span>
                  <span className="min-w-0 flex-1 truncate text-muted-foreground">{item.title}</span>
                  {item.argumentHint ? (
                    <span className="shrink-0 text-muted-foreground/70">{item.argumentHint}</span>
                  ) : null}
                  <span className="shrink-0 text-2xs text-muted-foreground/60">
                    {item.source === "builtin"
                      ? "内置"
                      : item.source === "user"
                        ? "个人"
                        : item.source === "project"
                          ? "项目"
                          : item.source}
                  </span>
                </button>
              ))}
              <div className="flex items-center gap-1 border-t border-border px-3 py-1 text-2xs text-muted-foreground/60">
                <CornerDownLeft className="size-2.5" /> 选用 · Esc 关闭
              </div>
            </div>
          ) : null}

          {mention && suggestions.length > 0 ? (
            <div className="absolute bottom-[calc(100%-6px)] right-2 left-2 z-overlay max-h-64 overflow-y-auto rounded-lg border border-border bg-elevated py-1 shadow-md">
              {suggestions.map((item, index) => (
                <button
                  key={item.abs}
                  type="button"
                  onMouseDown={(event) => {
                    event.preventDefault();
                    chooseMention(item);
                  }}
                  onMouseEnter={() => setMentionIndex(index)}
                  className={cn(
                    "flex w-full items-center gap-2 rounded-sm px-3 py-1.5 text-left text-sm outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring/55",
                    index === mentionIndex && "bg-accent",
                  )}
                >
                  {item.isDir ? (
                    <FolderIcon className="size-3 shrink-0 text-muted-foreground" />
                  ) : (
                    <FileText className="size-3 shrink-0 text-muted-foreground" />
                  )}
                  <span className="min-w-0 flex-1 truncate font-mono text-foreground">{item.rel}</span>
                  {item.isDir ? (
                    <span className="shrink-0 text-2xs text-muted-foreground/60">目录 · 只插入路径</span>
                  ) : (
                    <span className="shrink-0 text-2xs text-muted-foreground/60">附正文</span>
                  )}
                </button>
              ))}
            </div>
          ) : null}

          <PlanApprovalBar />

          {attachments.length > 0 ? (
            <div className="flex flex-wrap gap-1.5 px-2.5 pt-2.5">
              {attachments.map((item) => (
                <span
                  key={item.id}
                  className="flex max-w-[280px] items-center gap-1.5 rounded-lg border border-border bg-elevated py-1 pr-1 pl-2 text-xs"
                >
                  {item.kind === "image" && item.previewDataUrl ? (
                    <img
                      src={item.previewDataUrl}
                      alt={item.name}
                      className="size-6 shrink-0 rounded border border-border object-cover"
                    />
                  ) : (
                    <FileText className="size-3 shrink-0 text-muted-foreground" />
                  )}
                  <span className="truncate">{item.name}</span>
                  <span className="shrink-0 text-muted-foreground tabular-nums">
                    {item.kind === "image"
                      ? `${Math.max(1, Math.round(item.chars / 1024))} KB`
                      : item.chars.toLocaleString("zh-CN")}
                  </span>
                  <button
                    type="button"
                    aria-label={`移除 ${item.name}`}
                    onClick={() => detachAttachment(item.id)}
                    className="flex size-4 shrink-0 items-center justify-center rounded-lg text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
                  >
                    <X className="size-3" />
                  </button>
                </span>
              ))}
            </div>
          ) : null}

          {/* 「图像」能力只管**对话管线**的附件发送：生图/视频会话的参考图走
              media_generate 的 images/edits（从盘上直读），与这道开关无关，
              提示在这里纯属误导（真机踩过） */}
          {kind === "chat" &&
          attachments.some((item) => item.kind === "image") &&
          outlook !== "sent" ? (
            <p className="px-2.5 pt-1.5 text-xs leading-5 text-muted-foreground">
              {outlook === "unknown"
                ? "模型池在自动调度：这一发由哪个模型答还没定，图发不发得出去看它的多模态能力。"
                : "这一发用的模型没开「图像」能力：图只会以路径写进正文，模型看不见图里的内容。要让它真看到，去服务商档案的模型卡片点亮「图像」。"}
            </p>
          ) : null}

          {pendingLinks.length > 0 ? (
            <div className="flex flex-wrap items-center gap-1.5 px-2.5 pt-2.5 text-xs text-muted-foreground">
              <Link2 className="size-3 shrink-0" />
              <span>发送时抓取正文：</span>
              {pendingLinks.map((url) => (
                <span
                  key={url}
                  className="max-w-[200px] truncate rounded-lg border border-border bg-elevated px-1.5 py-0.5 font-mono text-foreground"
                  title={url}
                >
                  {hostOfUrl(url)}
                </span>
              ))}
            </div>
          ) : null}

          {/* 视频画布的四类生成页签（参照即梦）：文本/图片/视频/音频各走各的
              管线与模型行（kindModels.text/image/video/audio），产物落进选中节点 */}
          {kind === "video" ? (
            <div className="flex items-center gap-1 border-b border-border px-3 pt-1.5 pb-0">
              {(
                [
                  ["text", "文本生成"],
                  ["image", "图片生成"],
                  ["video", "视频生成"],
                  ["audio", "音频生成"],
                ] as Array<[MediaType, string]>
              ).map(([value, label]) => (
                <button
                  key={value}
                  type="button"
                  aria-pressed={generationType === value}
                  onClick={() => setGenerationType(value)}
                  className={cn(
                    "rounded-t-md px-2.5 py-1.5 text-xs outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
                    generationType === value
                      ? "border-b-2 border-brand font-medium text-foreground"
                      : "border-b-2 border-transparent text-muted-foreground hover:text-foreground",
                  )}
                >
                  {label}
                </button>
              ))}
            </div>
          ) : null}

          {/* 能力会话的徽标：生图/视频开档即定，模型得自己切到支持生成的款。
              视频会话另有模板骨架 chips（参照即梦的"短剧漫画/"前缀）——
              点按把结构化骨架填进输入框，占位符就是让用户逐格填的 */}
          {kind !== "chat" ? (
            <div className="flex flex-wrap items-center gap-1.5 px-3 pt-2">
              <span
                className={cn(
                  "inline-flex items-center gap-1 rounded-md px-1.5 py-0.5 text-2xs font-medium",
                  activeModelMissing
                    ? "bg-amber-500/10 text-amber-600 dark:text-amber-400"
                    : kind === "image"
                      ? "bg-sky-500/10 text-sky-600 dark:text-sky-400"
                      : "bg-violet-500/10 text-violet-600 dark:text-violet-400",
                )}
              >
                {kind === "image" ? "生图会话" : "视频会话"}
                <span className="font-normal opacity-80">
                  {activeModelMissing
                    ? "· 当前模型未标注该生成能力——请切换模型或在服务商档案里标注"
                    : "· 模型已就绪"}
                </span>
              </span>
              {kind === "video" && generationType === "audio"
                ? (
                    [
                      ["tts", "语音合成"],
                      ["transcribe", "语音转写"],
                      ["music", "音乐生成"],
                    ] as Array<["tts" | "transcribe" | "music", string]>
                  ).map(([value, label]) => (
                    <button
                      key={value}
                      type="button"
                      aria-pressed={audioMode === value}
                      onClick={() => setAudioMode(value)}
                      className={cn(
                        "rounded-md border px-2 py-0.5 text-2xs outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
                        audioMode === value
                          ? "border-brand/45 bg-brand/10 text-brand-text"
                          : "border-border bg-surface text-muted-foreground hover:border-brand/40 hover:text-foreground",
                      )}
                    >
                      {label}
                    </button>
                  ))
                : null}
              {kind === "video" && generationType === "audio" && audioMode === "music"
                ? (
                    // 与子模式 chips 视觉区分：歌词组无描边、前缀「歌词」标签
                    <>
                      <span className="px-0.5 text-2xs text-muted-foreground/60">歌词</span>
                      {(
                        [
                          ["auto", "自动生成"],
                          ["custom", "自定义"],
                          ["instrumental", "纯乐器"],
                        ] as Array<["auto" | "custom" | "instrumental", string]>
                      ).map(([value, label]) => (
                        <button
                          key={value}
                          type="button"
                          aria-pressed={lyricsMode === value}
                          onClick={() => setLyricsMode(value)}
                          className={cn(
                            "rounded-md px-1.5 py-0.5 text-2xs outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
                            lyricsMode === value
                              ? "bg-brand/10 font-medium text-brand-text"
                              : "text-muted-foreground/80 hover:bg-accent hover:text-foreground",
                          )}
                        >
                          {label}
                        </button>
                      ))}
                    </>
                  )
                : null}
              {kind === "video" && generationType !== "audio"
                ? VIDEO_TEMPLATES.map((template) => (
                    <button
                      key={template.label}
                      type="button"
                      title="把这份结构化骨架填进输入框"
                      onClick={() =>
                        setDraft((previous) =>
                          previous.trim() ? `${previous}\n\n${template.scaffold}` : template.scaffold,
                        )
                      }
                      className="rounded-md border border-border bg-surface px-2 py-0.5 text-2xs text-muted-foreground outline-none transition-colors hover:border-brand/40 hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45"
                    >
                      {template.label}
                    </button>
                  ))
                : null}
            </div>
          ) : null}

          {/* 音乐生成的自定义歌词：只有选了「自定义歌词」才出现 */}
          {kind === "video" &&
          generationType === "audio" &&
          audioMode === "music" &&
          lyricsMode === "custom" ? (
            <Textarea
              value={lyrics}
              rows={3}
              aria-label="歌词"
              placeholder="输入歌词——按段换行，副歌可以重复"
              className="max-h-[140px] resize-none border-0 border-b border-border bg-transparent px-3.5 py-2 text-sm text-foreground focus-visible:ring-0"
              onChange={(event) => setLyrics(event.target.value)}
            />
          ) : null}

          <Textarea
            value={draft}
            rows={1}
            aria-label="消息输入"
            placeholder={
              pasteBusy
                ? "正在读取剪贴板里的截图……"
                : pending
                  ? "生成中——回车插队当前任务，Ctrl+回车排队下一轮"
                  : kind === "image"
                  ? "描述想生成的画面——主体、风格、构图，越具体越好…"
                  : kind === "video"
                    ? generationType === "text"
                      ? "让模型帮你把灵感写成分镜、文案或台词…"
                      : generationType === "audio"
                        ? audioMode === "transcribe"
                          ? "挂上一段音频（+ 选择音频），发送后转写成文字落进这个节点"
                          : audioMode === "music"
                            ? "输入你想要创作的音乐内容——风格、主题、情绪，歌词用上方设置填"
                            : "描述要配的旁白或台词——生成的音频落进这个节点"
                        : generationType === "image"
                          ? "为这个节点生成一张画面参考…"
                          : "输入你的灵感——一句话或一个点子，或点上方模板骨架逐格填"
                    : "随心输入 · / 命令 · @ 文件 · Ctrl+V 粘贴截图与链接"
            }
            className="max-h-[168px] min-h-11 flex-1 resize-none border-0 bg-transparent px-3.5 pt-3 pb-1 focus-visible:ring-0"
            onChange={(event) => {
              setDraft(event.target.value);
              setSlashDismissed(false);
              const caret = event.target.selectionStart ?? event.target.value.length;
              setMention(parseMention(event.target.value, caret));
            }}
            onSelect={(event) => {
              const target = event.currentTarget;
              setMention(parseMention(target.value, target.selectionStart ?? target.value.length));
            }}
            onPaste={(event) => void handlePaste(event)}
            onKeyDown={(event) => {
              // 输入法合成中的回车（确认候选词）是输入法的事：抢下来就会
              // 半截草稿当消息发出去，或者把用户要保留的拼音清掉
              if (event.nativeEvent.isComposing || event.keyCode === 229) return;
              // slash 菜单开着时方向键与回车归菜单，不归发送
              if (slashMenuVisible && filteredCommands.length > 0) {
                if (event.key === "ArrowDown") {
                  event.preventDefault();
                  setSlashIndex((index) => (index + 1) % filteredCommands.length);
                  return;
                }
                if (event.key === "ArrowUp") {
                  event.preventDefault();
                  setSlashIndex((index) => (index - 1 + filteredCommands.length) % filteredCommands.length);
                  return;
                }
                if (event.key === "Enter" || event.key === "Tab") {
                  event.preventDefault();
                  chooseSlash(filteredCommands[slashIndex]);
                  return;
                }
                if (event.key === "Escape") {
                  event.preventDefault();
                  setSlashDismissed(true);
                  return;
                }
              }
              // @-候选开着时同样归菜单
              if (mention && suggestions.length > 0) {
                if (event.key === "ArrowDown") {
                  event.preventDefault();
                  setMentionIndex((index) => (index + 1) % suggestions.length);
                  return;
                }
                if (event.key === "ArrowUp") {
                  event.preventDefault();
                  setMentionIndex((index) => (index - 1 + suggestions.length) % suggestions.length);
                  return;
                }
                if (event.key === "Enter" || event.key === "Tab") {
                  event.preventDefault();
                  chooseMention(suggestions[mentionIndex]);
                  return;
                }
                if (event.key === "Escape") {
                  event.preventDefault();
                  setMention(null);
                  return;
                }
              }
              if (event.key === "Enter") {
                event.preventDefault();
                // 生成中 Ctrl+回车 = 排进跟随队列，这一轮收尾后自动开跑
                if (pending && (event.ctrlKey || event.metaKey)) {
                  submitFollowUp();
                  return;
                }
                void submit();
              }
            }}
          />

          <div className="flex items-center justify-between gap-3 px-2.5 pb-2.5">
            <div className="flex min-w-0 items-center gap-1">
              <Menu>
                <MenuTrigger
                  type="button"
                  aria-label="添加"
                  className="flex size-8 items-center justify-center rounded-lg text-muted-foreground outline-none transition-colors hover:bg-accent hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45"
                >
                  <Plus className="size-4" />
                </MenuTrigger>

                <MenuContent>
                  {/* 各能力档的附件语义随档切换：视频=剧本（进上下文），
                      生图=参考图（图生图 images/edits 的参照，不进上下文），
                      对话=普通附件 */}
                  <MenuLabel>
                    {kind === "video" && generationType === "video"
                      ? config.videoGen?.mode === "edit"
                        ? "添加素材视频（编辑模式的改写对象）"
                        : config.videoGen?.mode === "frames"
                          ? "添加首尾帧（先选首帧，再选尾帧）"
                          : "添加参考图（视频的全能参照）"
                      : kind === "video" && generationType === "audio"
                        ? audioMode === "transcribe"
                          ? "添加音频（转写的对象，mp3/wav/m4a…）"
                          : "添加到本轮"
                        : kind === "video" && generationType === "image"
                          ? "添加参考图（图生图的参照）"
                          : kind === "video"
                            ? "上传到本轮（剧本随提示词发给视频模型）"
                            : kind === "image"
                              ? "添加参考图（图生图的参照）"
                              : "添加到本轮"}
                  </MenuLabel>
                  <MenuItem onSelect={() => void pickFiles()}>
                    <Paperclip className="size-3.5 text-muted-foreground" />
                    {kind === "video" && generationType === "video"
                      ? config.videoGen?.mode === "edit"
                        ? "选择视频"
                        : "选择参考图"
                      : kind === "video" && generationType === "audio"
                        ? audioMode === "transcribe"
                          ? "选择音频"
                          : "文件和文件夹"
                        : kind === "video" && generationType === "image"
                          ? "选择参考图"
                          : kind === "video"
                            ? "上传剧本"
                            : kind === "image"
                              ? "选择参考图"
                              : "文件和文件夹"}
                  </MenuItem>

                  <MenuSeparator />

                  <MenuLabel>工作目录</MenuLabel>
                  <MenuItem onSelect={() => setProjectDialogOpen(true)}>
                    <FolderPlus className="size-3.5 text-muted-foreground" />
                    新建工作目录
                  </MenuItem>
                </MenuContent>
              </Menu>

              {/* 视频画布的生成模式（即梦三模式）：参照素材的角色由它决定——
                  全能参考=图片作风格/主体参照；首尾帧=第一张首帧、第二张尾帧；
                  视频编辑=素材视频改写。模式随 videoGen 落盘 */}
              {kind === "video" && generationType === "video" ? (
                <Menu>
                  <MenuTrigger
                    type="button"
                    title="生成模式：决定参照素材怎么被视频模型使用"
                    className="flex items-center gap-1 rounded-lg border border-border bg-surface px-2 py-1 text-xs text-muted-foreground outline-none transition-colors hover:border-brand/40 hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45 data-[state=open]:border-brand/40 data-[state=open]:text-foreground"
                  >
                    <Movie className="size-3 shrink-0" />
                    {config.videoGen?.mode === "frames"
                      ? "首尾帧"
                      : config.videoGen?.mode === "edit"
                        ? "视频编辑"
                        : "全能参考"}
                    <ChevronDown className="size-3 shrink-0" />
                  </MenuTrigger>
                  <MenuContent>
                    {(
                      [
                        ["omni", "全能参考", "附上图片当主体/风格参照"],
                        ["frames", "首尾帧", "第一张作首帧，第二张作尾帧"],
                        ["edit", "视频编辑", "附上素材视频，按提示词改写"],
                      ] as Array<["omni" | "frames" | "edit", string, string]>
                    ).map(([value, label, hint]) => (
                      <MenuItem
                        key={value}
                        onSelect={() =>
                          void updateConfig({
                            videoGen: {
                              ...(config.videoGen ?? {
                                mode: "omni",
                                ratio: "16:9",
                                resolution: "720P",
                                duration: 5,
                              }),
                              mode: value,
                            },
                          })
                        }
                        className="justify-between gap-3"
                      >
                        <span>
                          <span className="block text-foreground">{label}</span>
                          <span className="block text-2xs text-muted-foreground">{hint}</span>
                        </span>
                        {(config.videoGen?.mode ?? "omni") === value ? (
                          <Check className="size-3.5 shrink-0 text-brand-text" />
                        ) : null}
                      </MenuItem>
                    ))}
                  </MenuContent>
                </Menu>
              ) : null}

              {/* 生图设置与 + 并排：比例/分辨率、质量、数量——随生成请求原样发给上游 */}
              {kind === "image" ? <ImageGenSettingsPopover /> : null}

              {/* Worktree（分支/工作树）与权限档、作业模式一样是**对话轮**的机制：
                  生图/视频会话不碰工作树，不显示 */}
              {kind === "chat" ? <WorktreeControl /> : null}

              {/* 权限档与作业模式是对话轮的机制（工具审批、目标/计划）——
                  生图/视频会话走生成管线，这些旋钮没有可作用的对象，不显示 */}
              {kind === "chat" ? (
                <>
                  <PermissionPicker />
                  <ModePicker />
                </>
              ) : null}

              {fetchingHosts ? (
                <span className="flex min-w-0 items-center gap-1 truncate text-xs text-muted-foreground">
                  <ImageIcon className="size-3 shrink-0 animate-pulse" />
                  正在抓取 {fetchingHosts.join("、")} 的正文…
                </span>
              ) : null}
              {pickError ? (
                <p className="truncate text-xs text-destructive" title={pickError}>
                  {pickError}
                </p>
              ) : null}
            </div>

            <div className="flex shrink-0 items-center gap-2">
              {/* 增强提示词走对话管线（把草稿发给当前模型改写）——生成会话里
                  选中的是生图/视频模型，没有对话可增强，不显示 */}
              {kind === "chat" ? (
                <button
                  type="button"
                  aria-label="增强提示词"
                  title="用当前模型增强输入框里的提示词"
                  disabled={!draft.trim() || enhancing}
                  onClick={() => void enhanceDraft()}
                  className="flex size-8 items-center justify-center rounded-lg text-muted-foreground outline-none transition-colors hover:bg-accent hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45 disabled:pointer-events-none disabled:opacity-40"
                >
                  <Sparkles className={cn("size-4", enhancing && "animate-pulse")} />
                </button>
              ) : null}
              {pending && followUpCount > 0 ? (
                <span className="text-2xs tabular-nums text-muted-foreground">
                  已排队 {followUpCount} 条
                </span>
              ) : null}
              <ContextUsageRing />
              {/* 视频画布的图片页签：比例/分辨率/张数亮在底栏上（对照即梦），
                  点开与生图会话同一套设置面板，参数共用 config.imageGen */}
              {kind === "video" && generationType === "image" ? (
                <ImageGenSettingsPopover variant="summary" />
              ) : null}
              {kind === "video" && generationType === "video" ? (
                <VideoGenSettingsPopover />
              ) : null}
              <ModelPicker />
              {pending || mediaBusy ? (
                <Button
                  size="icon-sm"
                  variant="brand"
                  aria-label={mediaBusy ? "停止等待生成" : "停止生成"}
                  className="rounded-full"
                  onClick={() => {
                    if (mediaBusy) {
                      stopMedia();
                      return;
                    }
                    void stopGeneration();
                  }}
                >
                  <Square className="size-3" />
                </Button>
              ) : (
                <Button
                  size="icon-sm"
                  variant="brand"
                  aria-label="发送"
                  disabled={!canSend}
                  className="rounded-full"
                  onClick={() => void submit()}
                >
                  <ArrowUp className="size-4" />
                </Button>
              )}
            </div>
          </div>
        </div>

        {/* 工作目录与详情摘要：挪到输入框下方作脚注行（参考设计：框外不放分割线，
            信息跟在框后面）。工作目录选择 + 服务商/密钥/消息/上轮/耗时同一行 */}
        <div className="mt-2 flex items-center justify-between gap-3">
          <ProjectPicker />
          <DetailStrip />
        </div>
      </ContentColumn>
    </div>
  );
}

/** base_url 只露主机名：一行里放不下整个 URL，完整地址悬停可见 */
function hostOf(url: string): string {
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
}

/**
 * 输入框下方的详情摘要（原右栏「详情」格的精选，与工作目录选择同一行）。
 * 挑的标准是"别处看不见的"：模型与思考程度在底栏选择器上、权限在权限选择器上、
 * 工作目录在左边——这里放的是服务商、密钥状态、消息数和上一轮的账（tokens 与耗时）。
 */
function DetailStrip() {
  const config = useChatStore((s) => s.config);
  const hasKey = useChatStore((s) => s.hasKey);
  // 只用得到条数：订阅长度而不是数组——流式期间数组引用每 60ms 换一次，
  // 挂大数组等于给底栏加一笔白拿的重渲税
  const messageCount = useChatStore((s) => s.messages.length);
  const usage = useChatStore((s) => s.usage);

  const items: Array<{ label: string; value: string; warn?: boolean; title?: string }> = [
    {
      label: "服务商",
      value: hostOf(config.baseUrl) || "未配置",
      warn: !config.baseUrl,
      title: config.baseUrl || undefined,
    },
    { label: "密钥", value: hasKey ? "已就绪" : "未找到", warn: !hasKey },
    { label: "消息", value: String(messageCount) },
    {
      // 输入与输出分开报：input 才是"上下文"，output 是这一轮的生成量——
      // 相加的那个数会把两者混在一起，看起来就像和缓存命中对不上
      label: "上轮",
      value: usage
        ? `${formatTokensCompact(usage.inputTokens)} → ${formatTokensCompact(usage.outputTokens)}`
        : "—",
      title:
        !usage
          ? undefined
          : usage.cachedTokens === null
            ? "服务商没回缓存字段，命中量不可知（不是 0）"
            : `输入 ${usage.inputTokens}（其中命中缓存 ${usage.cachedTokens}）· 输出 ${usage.outputTokens} tokens`,
    },
    { label: "耗时", value: usage ? `${(usage.durationMs / 1000).toFixed(1)}s` : "—" },
  ];

  return (
    <div className="flex min-w-0 items-center text-xs">
      {items.map((item, index) => (
        <span
          key={item.label}
          className={cn("flex min-w-0 items-center gap-1", index > 0 && "ml-3 border-l border-border pl-3")}
        >
          <span className="shrink-0 text-muted-foreground">{item.label}</span>
          <span
            title={item.title}
            className={cn("truncate tabular-nums", item.warn ? "text-destructive" : "text-foreground")}
          >
            {item.value}
          </span>
        </span>
      ))}
    </div>
  );
}
