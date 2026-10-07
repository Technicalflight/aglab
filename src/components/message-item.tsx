import { memo, useEffect, useState, type ReactNode } from "react";
import { save } from "@tauri-apps/plugin-dialog";
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { IconCheck as Check, IconChevronDown as ChevronDown, IconChevronLeft as ChevronLeft, IconChevronRight as ChevronRight, IconAlertCircle as CircleAlert, IconCopy as Copy, IconFileDiff as FileDiff, IconFileText as FileText, IconMusic as AudioKind, IconDots as MoreHorizontal, IconGitBranch as GitBranch, IconLoader2 as Loader, IconMovie as MovieKind, IconPhotoAi as PhotoKind, IconPencil as Pencil, IconRefresh as RefreshCw, IconSquare as Square, IconVolume2 as Volume2, IconBrain as Brain, IconTextCaption as TextKind } from "@tabler/icons-react";

import { Button } from "@/components/ui/button";
import { Dialog, DialogClose, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { Markdown } from "@/components/markdown";
import { AgentPills } from "@/components/workflow-feedback";
import { RunFlow } from "@/components/run-flow";
import { ToolCard } from "@/components/tool-card";
import { Menu, MenuContent, MenuItem, MenuTrigger } from "@/components/ui/menu";
import { formatCount } from "@/lib/format";
import { cn } from "@/lib/utils";
import { isCompactionDone } from "@/lib/turns";
import { COMPACTION_MSG_ID, useChatStore } from "@/store/chat-store";
import type { Message, MessageAttachment, RunStep } from "@/types/chat";

function timeOf(timestamp: number) {
  return new Date(timestamp).toLocaleTimeString("zh-CN", {
    hour: "2-digit",
    minute: "2-digit",
  });
}

/** 用户消息带的附件。图片渲染缩略图：优先内存态 data URL（发送当轮），
 *  读回的历史没有它，走 asset 协议从盘上的 path 读；协议没开或路径不在
 *  scope 里时 img 加载失败，回退成图标 chip——显示退化，不出错 */
/** 四类生成的加载卡：脉冲骨架 + 旋转图标 + 走动的耗时。
 *  耗时从 createdAt 起算——占位气泡的时间戳就是开生成的那一刻。
 *  视频画布的节点四类生成（文本/图片/视频/音频）都用它 */
export function MediaGeneratingCard({
  kind,
  startedAt,
}: {
  kind: "text" | "image" | "video" | "audio" | "transcribe" | "music";
  startedAt: number;
}) {
  const [elapsed, setElapsed] = useState(() =>
    Math.max(0, Math.floor((Date.now() - startedAt) / 1000)),
  );
  useEffect(() => {
    const timer = setInterval(
      () => setElapsed(Math.max(0, Math.floor((Date.now() - startedAt) / 1000))),
      1000,
    );
    return () => clearInterval(timer);
  }, [startedAt]);

  const KindIcon =
    kind === "image"
      ? PhotoKind
      : kind === "video"
        ? MovieKind
        : kind === "audio" || kind === "music"
          ? AudioKind
          : kind === "transcribe"
            ? Volume2
            : TextKind;
  const label =
    kind === "image"
      ? "正在生成图片"
      : kind === "video"
        ? "正在生成视频"
        : kind === "audio"
          ? "正在生成音频"
          : kind === "transcribe"
            ? "正在转写音频"
            : kind === "music"
              ? "正在生成音乐"
              : "正在生成文本";
  const duration =
    elapsed >= 60 ? `${Math.floor(elapsed / 60)} 分 ${elapsed % 60} 秒` : `${elapsed} 秒`;

  return (
    <div className="relative w-[260px] overflow-hidden rounded-lg border border-border bg-elevated">
      <div className="flex h-[180px] items-center justify-center bg-muted/40">
        <div className="flex flex-col items-center gap-2.5">
          <div className="relative flex size-12 items-center justify-center">
            <span className="absolute inset-0 animate-ping rounded-full bg-brand/15" />
            <span className="absolute inset-1 animate-pulse rounded-full bg-brand/10" />
            <Loader className="relative size-6 animate-spin text-brand" />
          </div>
          <p className="flex items-center gap-1.5 text-xs text-muted-foreground">
            <KindIcon className="size-3.5" />
            {label}
          </p>
        </div>
      </div>
      <div className="flex items-center justify-between px-2.5 py-1.5 text-2xs text-muted-foreground">
        <span>生成中，完成后自动展示</span>
        <span className="tabular-nums">{duration}</span>
      </div>
    </div>
  );
}

function MessageAttachments({
  attachments,
  align = "end",
}: {
  attachments: MessageAttachment[];
  /** 助手侧的产物靠左，用户侧的靠右 */
  align?: "start" | "end";
}) {
  // 双击任意一张 → 灯箱大图预览
  const [lightbox, setLightbox] = useState<number | null>(null);
  const imageEntries = attachments
    .map((item, index) => ({ item, index }))
    .filter(({ item }) => item.kind === "image");
  const otherEntries = attachments.filter((item) => item.kind !== "image");
  // 多张图横向成排：定宽缩略图 + 右向滚动 + 右缘渐影（提示还能往右看）；
  // 单张保持大图直出
  const strip = imageEntries.length > 1;

  return (
    <div
      className={cn(
        "flex min-w-0 max-w-full flex-col gap-1.5",
        align === "end" ? "items-end" : "items-start",
      )}
    >
      {strip ? (
        <div className="relative max-w-full">
          <div className="flex max-w-full gap-2 overflow-x-auto rounded-lg pb-1 [scrollbar-width:thin]">
            {imageEntries.map(({ item, index }) => (
              <ImageAttachment
                key={item.path}
                item={item}
                compact
                onDoubleClick={() => setLightbox(index)}
              />
            ))}
          </div>
          {/* 右缘渐影：纯装饰，不拦截滚动 */}
          <div
            aria-hidden
            className="pointer-events-none absolute inset-y-0 right-0 w-12 bg-gradient-to-l from-background to-transparent"
          />
        </div>
      ) : (
        imageEntries.map(({ item, index }) => (
          <ImageAttachment
            key={item.path}
            item={item}
            onDoubleClick={() => setLightbox(index)}
          />
        ))
      )}
      {otherEntries.map((item) =>
        item.kind === "audio" ? (
          // 音频生成的产物：就地一个播放器，不用再点开
          <audio
            key={item.path}
            controls
            preload="metadata"
            src={item.previewDataUrl ?? convertFileSrc(item.path)}
            className="w-[260px]"
          />
        ) : (
          <span
            key={item.path}
            className="flex max-w-[260px] items-center gap-1.5 rounded-lg border border-border bg-elevated px-2 py-1 text-xs text-muted-foreground"
            title={item.path}
          >
            <FileText className="size-3 shrink-0" />
            <span className="truncate text-foreground">{item.name}</span>
          </span>
        ),
      )}
      {lightbox !== null && attachments[lightbox] ? (
        <Lightbox src={attachments[lightbox].path} onClose={() => setLightbox(null)} />
      ) : null}
    </div>
  );
}

/** 图片预览弹窗：大图 + 右上关闭，Esc/点遮罩关闭。多张时双击各自打开 */
function Lightbox({ src, onClose }: { src: string; onClose: () => void }) {
  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="w-auto max-w-[92vw] p-3">
        <DialogTitle className="sr-only">图片预览</DialogTitle>
        <DialogClose />
        <img
          src={convertFileSrc(src)}
          alt="预览"
          className="max-h-[82vh] max-w-[86vw] rounded-lg object-contain"
        />
      </DialogContent>
    </Dialog>
  );
}

function ImageAttachment({
  item,
  onDoubleClick,
  compact,
}: {
  item: MessageAttachment;
  onDoubleClick?: () => void;
  /** 横向排里的定宽缩略图形态 */
  compact?: boolean;
}) {
  const [failed, setFailed] = useState(false);
  if (failed) {
    return (
      <span
        className="flex max-w-[260px] items-center gap-1.5 rounded-lg border border-border bg-elevated px-2 py-1 text-xs text-muted-foreground"
        title={item.path}
      >
        <FileText className="size-3 shrink-0" />
        <span className="truncate text-foreground">{item.name}</span>
        <span className="shrink-0 tabular-nums">{Math.max(1, Math.round(item.bytes / 1024))} KB</span>
      </span>
    );
  }
  return (
    <img
      src={item.previewDataUrl ?? convertFileSrc(item.path)}
      alt={item.name}
      title={`${item.name}（双击预览大图，原图在 ${item.path}）`}
      onError={() => setFailed(true)}
      onDoubleClick={onDoubleClick}
      className={cn(
        "cursor-zoom-in rounded-lg border border-border",
        compact ? "h-36 w-36 shrink-0 object-cover" : "max-h-48 max-w-[280px] object-contain",
      )}
    />
  );
}

/**
 * 单条消息。
 *
 * 包 memo 是这份列表能撑住长会话的关键：流式生成时每收到一个 token，
 * store 里的 messages 数组换新引用，message-list 重渲染，
 * 若不加 memo 则**全部历史消息跟着重渲染一遍**——第 200 条时每 token 要跑
 * 200 次 Markdown 解析与工具卡渲染。会话越长越卡，是这里最贵的一笔开销。
 *
 * memo 生效的前提是 props 浅比较相等：message 来自 byId.get(id)，
 * 同一对象引用在消息不变时保持稳定，所以未变的历史消息会被正确跳过。
 */
export const MessageItem = memo(function MessageItem({
  message,
  author,
  isLast,
  branch,
}: {
  message: Message;
  author: string;
  isLast: boolean;
  /** 同一处有几支可选（含自己，按插入序）。少于两条时不显示切换器 */
  branch?: string[];
}) {
  const pending = useChatStore((s) => s.pending);
  const editAndResend = useChatStore((s) => s.editAndResend);
  const switchBranch = useChatStore((s) => s.switchBranch);
  // 生成会话（生图/视频）整档不挂对话轮操作排——失败气泡这类没有产物标记的
  // 也得盖住，所以判据是会话档位本身，不是单条消息带没带图
  const isMediaSession = (useChatStore((s) => s.kind) ?? "chat") !== "chat";
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState("");
  const [copied, setCopied] = useState(false);
  const [forkNote, setForkNote] = useState<string | null>(null);

  // 到这里为止另开一条话题：源话题一个字节不动（后端要求锚点是用户消息，
  // 所以这个按钮只长在问题气泡上——从回答中间分叉会留下永远欠着结果的工具轮）
  async function forkHere() {
    const state = useChatStore.getState();
    const entryId = message.entryIds?.[0];
    if (!entryId) return;
    try {
      const newId = await invoke<string>("conversation_fork", {
        conversationId: state.activeId,
        entryId,
      });
      await state.refreshHistory();
      await state.openConversation(newId);
    } catch (error) {
      setForkNote(error instanceof Error ? error.message : String(error));
      window.setTimeout(() => setForkNote(null), 4000);
    }
  }

  const branchSwitcher = (
    <BranchSwitcher ids={branch} current={message.id} onPick={(id) => void switchBranch(id)} />
  );

  if (message.role === "user") {
    // 编辑态：原文填入输入框，保存后丢弃这条消息之后的所有内容并重发
    if (editing) {
      return (
        <div className="flex animate-message-in flex-col items-end gap-1.5">
          <textarea
            autoFocus
            rows={Math.min(Math.max(draft.split("\n").length, 2), 10)}
            value={draft}
            className="w-[85%] resize-none rounded-lg border border-brand/50 bg-background px-3 py-2 text-[length:var(--chat-font-size)] leading-[1.7] text-foreground outline-none focus-visible:ring-2 focus-visible:ring-ring/35"
            onChange={(event) => setDraft(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter" && (event.ctrlKey || event.metaKey)) {
                event.preventDefault();
                if (draft.trim()) {
                  setEditing(false);
                  void editAndResend(message.id, draft);
                }
              }
              if (event.key === "Escape") {
                event.preventDefault();
                setEditing(false);
              }
            }}
          />
          <div className="flex items-center gap-2">
            <span className="text-2xs text-muted-foreground">
              保存后从这里另起一支重发；原来那一支不删，切换器能翻回去看
            </span>
            <Button size="sm" variant="ghost" onClick={() => setEditing(false)}>
              取消
            </Button>
            <Button
              size="sm"
              variant="brand"
              disabled={!draft.trim() || pending}
              onClick={() => {
                setEditing(false);
                void editAndResend(message.id, draft);
              }}
            >
              保存并重发
            </Button>
          </div>
        </div>
      );
    }

    async function copyUser() {
      try {
        await navigator.clipboard.writeText(message.content);
        setCopied(true);
        setTimeout(() => setCopied(false), 1500);
      } catch {
        setCopied(false);
      }
    }

    return (
      <div className="flex animate-message-in flex-col items-end gap-1">
        <div className="group flex max-w-[85%] flex-col items-end gap-1">
          {message.attachments && message.attachments.length > 0 ? (
            <MessageAttachments attachments={message.attachments} />
          ) : null}
          <div className="max-w-full rounded-lg bg-surface px-4 py-3 text-[length:var(--chat-font-size)] leading-[1.7] whitespace-pre-wrap">
            {message.content}
          </div>
          {/* 悬停气泡时露出操作按钮：时间常显，复制/编辑浮现 */}
          <div className="flex items-center gap-0.5 text-2xs tabular-nums text-muted-foreground opacity-0 transition-opacity group-hover:opacity-100">
            <span>{timeOf(message.createdAt)}</span>
            <button
              type="button"
              aria-label="复制消息"
              title="复制"
              onClick={() => void copyUser()}
              className="flex size-6 items-center justify-center rounded-lg transition-colors hover:bg-accent hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/45"
            >
              {copied ? <Check className="size-3" /> : <Copy className="size-3" />}
            </button>
            {/* 生成会话的用户行只留复制：编辑重发/分叉/版本切换都是对话轮机制 */}
            {!isMediaSession ? (
              <>
            <button
              type="button"
              aria-label="编辑并重发"
              title="编辑并重发"
              disabled={pending}
              onClick={() => {
                setDraft(message.content);
                setEditing(true);
              }}
              className="flex size-6 items-center justify-center rounded-lg transition-colors hover:bg-accent hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/45 disabled:cursor-not-allowed disabled:opacity-40"
            >
              <Pencil className="size-3" />
            </button>
            <button
              type="button"
              aria-label="到这里为止另开一条话题"
              title="到这里为止另开一条话题"
              disabled={pending || !message.entryIds?.length}
              onClick={() => void forkHere()}
              className="flex size-6 items-center justify-center rounded-lg transition-colors hover:bg-accent hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/45 disabled:cursor-not-allowed disabled:opacity-40"
            >
              <GitBranch className="size-3" />
            </button>
            {branchSwitcher}
              </>
            ) : null}
          </div>
          {forkNote ? (
            <p className="max-w-[85%] text-right text-2xs leading-4 text-destructive">{forkNote}</p>
          ) : null}
        </div>
      </div>
    );
  }

  // 压缩中的状态行：灰色小字居中，不占一个气泡的版面
  if (message.id === COMPACTION_MSG_ID || message.content.startsWith("【上下文压缩中】")) {
    return <CompactionLiveDivider />;
  }
  // 压缩完成的摘要行：一条居中的灰色分割线，点开能看到它压出来的摘要——
  // 没人想每次回看话题都被一大段摘要挡住
  if (isCompactionDone(message)) {
    return <CompactionDivider message={message} />;
  }

  // 流式内联：带切片点的调用（新版后端盖的章）按声明位置插回原文流——
  // 文字→工具→文字按真实发生顺序走，而不是全部堆在消息顶上。
  // 老历史没有这个章（contentChars 全空），照旧走流程条/堆叠布局
  const interleaved = (message.toolCalls ?? []).some(
    (call) => typeof call.contentChars === "number",
  );
  // 步骤全带章：思考段也内联（顶部的思考面板就不必再摆一份）
  const stepsStamped =
    (message.steps ?? []).length > 0 &&
    (message.steps ?? []).every((step) => typeof step.contentChars === "number");

  return (
    <div className="flex animate-message-in flex-col gap-1.5">
      <p className="text-xs text-muted-foreground">
        {author}
        <span className="ml-2 opacity-70">{timeOf(message.createdAt)}</span>
        {/* 后端自己接的那一轮要说得出是第几轮：屏上看不出"这是它接着往下跑的第 3 轮"，
            人读到的就是"模型一口气说了很多话"。轮数由广播读数派生，只活在界面上 */}
        {message.goalRound ? (
          <span className="ml-2 rounded bg-brand/12 px-1.5 text-2xs tabular-nums text-brand-text">
            目标 · 第 {message.goalRound} 轮
          </span>
        ) : null}
      </p>

      {/* 生图/视频的生成占位：加载卡（脉冲骨架 + 走动耗时）替代一切文字状态 */}
      {message.media && message.streaming ? (
        <MediaGeneratingCard kind={message.media} startedAt={message.createdAt} />
      ) : interleaved ? (
        <>
          {!stepsStamped && (message.reasoning || message.reasoningStreaming) ? (
            <ReasoningPanel message={message} />
          ) : null}
          <InlineFlow message={message} />
        </>
      ) : message.steps && message.steps.length > 0 ? (
        // 有流程可报的时候，思考与工具都从这一条读：两处各摆一份就会有一份是旧的
        <RunFlow message={message} />
      ) : (
        <>
          {message.reasoning || message.reasoningStreaming ? (
            <ReasoningPanel message={message} />
          ) : null}

          {message.toolCalls && message.toolCalls.length > 0 ? (
            <div className="my-1 space-y-0.5">
              {message.toolCalls.map((call) => (
                <ToolCard key={call.id} call={call} />
              ))}
            </div>
          ) : null}
        </>
      )}

      {!interleaved && message.content ? <Markdown content={message.content} /> : null}

      {/* 助手气泡也带附件：生图/视频管线的产物就挂在这格上（图片走 asset 协议渲染） */}
      {message.attachments && message.attachments.length > 0 ? (
        <MessageAttachments attachments={message.attachments} align="start" />
      ) : null}

      {!message.streaming ? <EditSummaryCard message={message} /> : null}

      {message.streaming && !message.media ? (
        <span className="flex items-center gap-2 text-xs text-muted-foreground">
          {/* 流程条的头部已经在报"正在执行中 · Ns"，这里就不必再重复一句"正在生成" */}
          {!message.content &&
            !message.reasoningStreaming &&
            (message.steps?.length ? null : "正在生成")}
          <span className="h-[1.1em] w-0.5 animate-caret-pulse bg-brand" />
        </span>
      ) : null}

      {message.error ? (
        // 完整原因在右上角的告警条里；这一行留着是为了让"这轮没答完"这件事
        // 长在记录本身上，重载之后也还在
        <p
          className="flex items-center gap-1.5 text-sm text-destructive"
          title={message.error}
        >
          <CircleAlert className="size-3.5" />
          本轮请求失败 · 悬停看原因
        </p>
      ) : null}

      {message.note ? (
        // 这行不是装饰：一条客户端回执坐在模型的名字下面，
        // 不写清楚是谁说的，用户就会以为模型自己说它记住了
        <p className="text-2xs text-muted-foreground/60">客户端回执 · 这句话不来自模型</p>
      ) : !message.streaming &&
        !isMediaSession &&
        !(message.media || message.attachments?.some((a) => a.kind === "image" || a.kind === "video")) ? (
        // 生成会话的操作排整档隐藏（复制/朗读/重生成/导出/版本切换都是对话轮机制）：
        // 按会话档位判——失败气泡没有产物标记，靠消息内容判会漏
        <MessageActions message={message} isLast={isLast} branch={branch} />
      ) : null}
    </div>
  );
});

/**
 * 本轮 aglab 改过哪些文件。数字来自编辑台账，按工具调用 id 对回这条消息。
 * 只有 aglab 内置的写入会被记上——模型经 MCP 改的文件不在这张卡上，
 * 那条边界写在右栏「预览」底部
 */
function EditSummaryCard({ message }: { message: Message }) {
  const edits = useChatStore((s) => s.edits);
  const openPreview = useChatStore((s) => s.openPreview);
  const [expanded, setExpanded] = useState(false);

  const callIds = new Set((message.toolCalls ?? []).map((call) => call.id));
  const mine = edits.filter((edit) => edit.callIds.some((id) => callIds.has(id)));
  if (mine.length === 0) return null;

  const additions = mine.reduce((sum, edit) => sum + edit.additions, 0);
  const deletions = mine.reduce((sum, edit) => sum + edit.deletions, 0);
  const approximate = mine.some((edit) => edit.approximate);
  const shown = expanded ? mine : mine.slice(0, 3);

  return (
    <div className="mt-2 overflow-hidden rounded-lg border border-border bg-surface">
      <div className="flex items-center gap-2.5 px-3 py-2.5">
        <span className="flex size-7 shrink-0 items-center justify-center rounded-lg bg-elevated">
          <FileDiff className="size-3.5 text-muted-foreground" />
        </span>
        <div className="min-w-0 flex-1">
          <p className="text-sm font-medium text-foreground">已编辑 {mine.length} 个文件</p>
          <p className="mt-0.5 font-mono text-xs">
            {approximate ? <span className="text-muted-foreground">约 </span> : null}
            <span className="text-diff-added">+{formatCount(additions)}</span>{" "}
            <span className="text-diff-removed">-{formatCount(deletions)}</span>
          </p>
        </div>
        <Button variant="subtle" size="sm" onClick={() => openPreview(mine[0].absPath)}>
          审阅
        </Button>
      </div>

      <ul className="divide-y divide-border border-t border-border">
        {shown.map((edit) => (
          <li key={edit.absPath}>
            <button
              type="button"
              onClick={() => openPreview(edit.absPath)}
              className="flex w-full items-baseline gap-3 px-3 py-2 text-left outline-none transition-colors hover:bg-accent focus-visible:ring-2 focus-visible:ring-ring/45"
            >
              <span className="min-w-0 flex-1 truncate font-mono text-xs text-foreground">
                {edit.path}
              </span>
              <span className="shrink-0 font-mono text-xs">
                <span className="text-diff-added">+{formatCount(edit.additions)}</span>{" "}
                <span className="text-diff-removed">-{formatCount(edit.deletions)}</span>
              </span>
            </button>
          </li>
        ))}
      </ul>

      {mine.length > 3 ? (
        <button
          type="button"
          onClick={() => setExpanded((prev) => !prev)}
          className="flex w-full items-center gap-1 border-t border-border px-3 py-2 text-left text-xs text-muted-foreground outline-none transition-colors hover:bg-accent hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45"
        >
          {expanded ? "收起" : `再显示 ${mine.length - 3} 个文件`}
          <ChevronDown className={`size-3 ${expanded ? "rotate-180" : ""}`} />
        </button>
      ) : null}
    </div>
  );
}

/** 消息操作栏：复制 / 朗读 / 重新生成 / 更多（导出）。生成中的消息不显示 */
/** 分支切换器：同一处有另一支可看时才出现。它换的是"看哪一条路径"，
 *  不删任何东西——被换掉的那一支整段留在 offPath 里 */
function BranchSwitcher({
  ids,
  current,
  onPick,
}: {
  ids?: string[];
  current: string;
  onPick: (id: string) => void;
}) {
  if (!ids || ids.length < 2) return null;
  const at = ids.indexOf(current);
  if (at === -1) return null;
  const go = (delta: number) => {
    const target = ids[at + delta];
    if (target) onPick(target);
  };
  const button = (label: string, disabled: boolean, onClick: () => void, icon: ReactNode) => (
    <button
      type="button"
      aria-label={label}
      title={label}
      disabled={disabled}
      onClick={onClick}
      className="flex size-6 items-center justify-center rounded-lg transition-colors hover:bg-accent hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/45 disabled:cursor-not-allowed disabled:opacity-35"
    >
      {icon}
    </button>
  );
  return (
    <span className="flex items-center gap-0.5">
      {button("上一支", at === 0, () => go(-1), <ChevronLeft className="size-3" />)}
      <span className="text-2xs tabular-nums">
        {at + 1}/{ids.length}
      </span>
      {button("下一支", at === ids.length - 1, () => go(1), <ChevronRight className="size-3" />)}
    </span>
  );
}

function MessageActions({
  message,
  isLast,
  branch,
}: {
  message: Message;
  isLast: boolean;
  branch?: string[];
}) {
  const regenerate = useChatStore((s) => s.regenerate);
  const [copied, setCopied] = useState(false);
  const [speaking, setSpeaking] = useState(false);
  const [exportNote, setExportNote] = useState<string | null>(null);
  const [menuOpen, setMenuOpen] = useState(false);

  useEffect(() => {
    window.speechSynthesis?.cancel();
    setSpeaking(false);
  }, [message.id]);

  async function copyContent() {
    try {
      await navigator.clipboard.writeText(message.content);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      setCopied(false);
    }
  }

  function toggleSpeak() {
    const synthesis = window.speechSynthesis;
    if (!synthesis) return;
    if (speaking) {
      synthesis.cancel();
      setSpeaking(false);
      return;
    }
    const utterance = new SpeechSynthesisUtterance(message.content);
    utterance.lang = "zh-CN";
    utterance.onend = () => setSpeaking(false);
    utterance.onerror = () => setSpeaking(false);
    synthesis.cancel();
    synthesis.speak(utterance);
    setSpeaking(true);
  }

  async function exportMarkdown() {
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
      setExportNote("已导出");
      setTimeout(() => setExportNote(null), 2000);
    } catch (error) {
      setExportNote(error instanceof Error ? error.message : String(error));
    }
  }

  return (
    <div className="mt-0.5 flex items-center gap-0.5 text-muted-foreground">
      <ActionButton
        label={copied ? "已复制" : "复制"}
        onClick={() => void copyContent()}
        disabled={!message.content}
      >
        {copied ? <Check className="size-3.5" /> : <Copy className="size-3.5" />}
      </ActionButton>

      <ActionButton label={speaking ? "停止朗读" : "朗读"} onClick={toggleSpeak}>
        {speaking ? <Square className="size-3" /> : <Volume2 className="size-3.5" />}
      </ActionButton>

      {isLast ? (
        <ActionButton label="重新生成" onClick={() => void regenerate()}>
          <RefreshCw className="size-3.5" />
        </ActionButton>
      ) : null}

      {/* 这一支之外还有别支时，切换器就坐在这里：换的是"看哪一条路径"，一行都不删 */}
      <BranchSwitcher ids={branch} current={message.id} onPick={(id) => void useChatStore.getState().switchBranch(id)} />

      <Menu open={menuOpen} onOpenChange={setMenuOpen}>
        <MenuTrigger asChild>
          <button
            type="button"
            aria-label="更多操作"
            className="flex size-7 items-center justify-center rounded-lg transition-colors hover:bg-accent hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/45"
          >
            <MoreHorizontal className="size-3.5" />
          </button>
        </MenuTrigger>
        <MenuContent align="start" className="w-48">
          <MenuItem onSelect={() => void exportMarkdown()}>
            <span>导出为 Markdown</span>
          </MenuItem>
        </MenuContent>
      </Menu>

      {exportNote ? (
        <span className="ml-1 text-2xs text-muted-foreground">{exportNote}</span>
      ) : (
        <span
          className="ml-1 text-xs tabular-nums text-muted-foreground/70"
          title={message.model ? undefined : "这条落盘时没人记下用它的是谁"}
        >
          {message.model ?? "—"} · {timeOf(message.createdAt)}
        </span>
      )}
      <AgentPills message={message} />

      {/* 交付出去的内容要看得出不是人写的。挤在同一行最右，不额外占一行 */}
      <span className="ml-auto pl-2 text-2xs text-muted-foreground/60">由 AI 生成</span>
    </div>
  );
}

function ActionButton({
  label,
  onClick,
  disabled,
  children,
}: {
  label: string;
  onClick: () => void;
  disabled?: boolean;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      aria-label={label}
      title={label}
      disabled={disabled}
      onClick={onClick}
      className="flex size-7 items-center justify-center rounded-lg transition-colors hover:bg-accent hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/45 disabled:cursor-not-allowed disabled:opacity-40"
    >
      {children}
    </button>
  );
}

/**
 * 上下文压缩的两条状态线。都是灰色小字居中的分割样式——它的职责是把
 * "被压缩的旧上下文"与"其后的对话"在版面上分开，而不是占一条气泡：
 * - 压缩中：脉冲的"正在压缩上下文…"（开始时由 chat-store 插入，完成后换掉）
 * - 已压缩：一条可点开的分割线，摘要在折叠区里，标题行永远只有一行小字
 */
function CompactionLiveDivider() {
  return (
    <div className="animate-message-in flex items-center justify-center gap-3 py-1" role="status">
      <span className="h-px flex-1 bg-border" />
      <span className="animate-pulse text-2xs leading-4 text-muted-foreground">
        正在压缩上下文…
      </span>
      <span className="h-px flex-1 bg-border" />
    </div>
  );
}

function CompactionDivider({ message }: { message: Message }) {
  const [open, setOpen] = useState(false);
  // 摘要正文 = 前缀行之后的部分；老数据没有前缀就整段展示
  const prefixEnd = message.content.indexOf("\n\n");
  const body =
    message.content.startsWith("【上下文压缩完成】") && prefixEnd !== -1
      ? message.content.slice(prefixEnd + 2)
      : message.content;

  return (
    <div className="animate-message-in my-1 flex flex-col items-center gap-1" role="separator">
      <button
        type="button"
        onClick={() => setOpen(!open)}
        aria-expanded={open}
        title={open ? "收起摘要" : "展开摘要"}
        className="group flex w-full items-center gap-3 py-0.5 focus-visible:outline-none"
      >
        <span className="h-px flex-1 bg-border transition-colors group-hover:bg-muted-foreground/40" />
        <span className="flex items-center gap-1 whitespace-nowrap text-2xs leading-4 text-muted-foreground transition-colors group-hover:text-foreground/80">
          <ChevronRight className={`size-3 transition-transform ${open ? "rotate-90" : ""}`} />
          上下文已压缩
        </span>
        <span className="h-px flex-1 bg-border transition-colors group-hover:bg-muted-foreground/40" />
      </button>
      {open ? (
        <div className="w-full max-w-2xl rounded-lg border border-dashed border-border bg-surface/60 px-3.5 py-2.5">
          <Markdown content={body} />
        </div>
      ) : null}
    </div>
  );
}

/**
 * 流式内联的正文流：按每个调用的声明位置把正文切片，工具行插在原文流的
 * 真实位置上（文字→工具→文字）。同一次声明的一批调用共享同一个切片点，
 * 按到达次序排；切片点之后还在流的字归下一段
 */
function InlineFlow({ message }: { message: Message }) {
  const content = message.content;
  const reasoning = message.reasoning ?? "";
  const calls = message.toolCalls ?? [];
  const steps = message.steps ?? [];
  const blocks: ReactNode[] = [];

  // 步骤全带章（直播与重放都是）：按步骤顺序走，思考段切片内联
  const stepsReady =
    steps.length > 0 && steps.every((step) => typeof step.contentChars === "number");

  if (stepsReady) {
    let cursor = 0;
    steps.forEach((step, index) => {
      const at = Math.min(step.contentChars ?? 0, content.length);
      const text = content.slice(cursor, at);
      if (text.trim()) blocks.push(<Markdown key={`text-${step.id}`} content={text} />);
      cursor = Math.max(cursor, at);

      if (step.kind === "thinking") {
        // 思考段管到下一个思考段开头（或整段思考的末尾）——中间夹着的工具行
        // 不消耗 reasoning，只挪正文游标
        const nextThinking = steps.find(
          (later, laterIndex) => laterIndex > index && later.kind === "thinking",
        );
        const start = step.kind === "thinking" ? step.from : 0;
        const end =
          nextThinking && nextThinking.kind === "thinking"
            ? (nextThinking.from ?? reasoning.length)
            : reasoning.length;
        const slice = reasoning.slice(start, Math.max(end, start));
        const live = streamingThinking(message, step, steps);
        if (slice.trim() || live) {
          blocks.push(<ThinkingBlock key={step.id} text={slice} live={live} />);
        }
      } else {
        const call = calls.find((item) => item.id === step.callId);
        if (call) blocks.push(<ToolCard key={call.id} call={call} />);
      }
    });
    const tail = content.slice(cursor);
    if (tail.trim()) blocks.push(<Markdown key="text-tail" content={tail} />);
    return <div className="my-1 flex flex-col gap-1">{blocks}</div>;
  }

  // 调用有章、步骤没章（老事件）：只内联工具行，思考走顶部面板
  const stamped = [...calls]
    .filter((call) => typeof call.contentChars === "number")
    .sort((a, b) => (a.contentChars ?? 0) - (b.contentChars ?? 0));
  let cursor = 0;
  for (const call of stamped) {
    const at = Math.min(call.contentChars ?? 0, content.length);
    const slice = content.slice(cursor, at);
    if (slice.trim()) blocks.push(<Markdown key={`text-${call.id}`} content={slice} />);
    blocks.push(<ToolCard key={call.id} call={call} />);
    cursor = Math.max(cursor, at);
  }
  const tail = content.slice(cursor);
  if (tail.trim()) blocks.push(<Markdown key="text-tail" content={tail} />);
  return <div className="my-1 flex flex-col gap-1">{blocks}</div>;
}

/** 只有"最后一步且思考还在出字"才算正在想：答案开始流之后 reasoningStreaming 就灭了 */
function streamingThinking(message: Message, step: RunStep, steps: RunStep[]): boolean {
  return message.reasoningStreaming === true && step === steps.at(-1);
}

/** 内联的思考段：直播时展开跟着长，想完收成一行预览；点开随时回看 */
function ThinkingBlock({ text, live }: { text: string; live: boolean }) {
  const [manual, setManual] = useState<boolean | null>(null);
  const open = manual ?? live;
  const preview = text.split(/\r?\n/).find((line) => line.trim().length > 0)?.trim() ?? "";
  return (
    <div className="my-0.5">
      <button
        type="button"
        aria-expanded={open}
        onClick={() => setManual(!open)}
        className="flex w-full items-center gap-2 rounded-md px-1.5 py-1 text-left text-sm outline-none transition-colors hover:bg-accent/50 focus-visible:ring-2 focus-visible:ring-ring/45"
      >
        <Brain className="size-3.5 shrink-0 text-muted-foreground" />
        <span className="shrink-0 font-medium text-muted-foreground">思考</span>
        <span className="min-w-0 flex-1 truncate text-xs text-muted-foreground/70">
          {open ? "" : preview}
        </span>
        {live ? <span className="h-[1.1em] w-0.5 shrink-0 animate-caret-pulse bg-brand" /> : null}
      </button>
      {open ? (
        <p className="ml-5 whitespace-pre-wrap break-words border-l border-border pl-2.5 text-xs leading-5 text-muted-foreground">
          {text}
        </p>
      ) : null}
    </div>
  );
}

function ReasoningPanel({ message }: { message: Message }) {  // 流式期间自动展开，答案开始后收起；用户手动开合优先
  const [override, setOverride] = useState<boolean | null>(null);
  const open = override ?? Boolean(message.reasoningStreaming);
  const chevron = open ? <ChevronDown className="size-3.5" /> : <ChevronRight className="size-3.5" />;

  return (
    <div className="my-1">
      <button
        type="button"
        onClick={() => setOverride(!open)}
        className="flex items-center gap-1.5 text-xs text-muted-foreground transition-colors hover:text-foreground focus-visible:outline-none"
      >
        {chevron}
        思考过程
        {message.reasoningStreaming ? (
          <span className="h-[1em] w-0.5 animate-caret-pulse bg-brand" />
        ) : null}
      </button>

      {open ? (
        <div className="mt-2 max-h-64 overflow-y-auto border-l-2 border-border pl-3.5">
          <p className="text-sm leading-6 whitespace-pre-wrap break-words text-muted-foreground">
            {message.reasoning}
          </p>
        </div>
      ) : null}
    </div>
  );
}
