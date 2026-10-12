import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { save as saveDialog } from "@tauri-apps/plugin-dialog";
import { IconDownload as Download, IconFolderOpen as FolderOpen } from "@tabler/icons-react";

import { Composer } from "@/components/composer";
import { MessageList } from "@/components/message-list";
import {
  kindModelPlaceholder,
  modelDisplayLabel,
  useKindModelLine,
} from "@/components/model-picker";
import { RequestChainCard } from "@/components/request-chain-card";
import { useIsNarrow } from "@/lib/use-media-query";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { Menu, MenuContent, MenuItem, MenuTrigger } from "@/components/ui/menu";
import { cn } from "@/lib/utils";
import { VideoWorkspace } from "@/components/video-workspace";
import { MusicWorkspace } from "@/components/music-workspace";
import { ASSISTANT_NAME, TURN_PHASE_LABEL, useChatStore } from "@/store/chat-store";
import { CompileFeedbackLine } from "@/components/workflow-feedback";
import { exportConversation } from "@/lib/knowledge";

const EXPORT_FORMATS = [
  { format: "markdown", label: "Markdown（.md）", hint: "人读格式，标题 + 逐条用户/助手正文" },
  { format: "json", label: "JSON（.json）", hint: "整段结构，含标题与时间戳" },
  {
    format: "jsonl",
    label: "微调数据 JSONL（.jsonl）",
    hint: "Unsloth/ShareGpt 格式，一问一答一行",
  },
  {
    format: "snapshot",
    label: "分享快照（.html）",
    hint: "自包含只读网页，双击即看；只含对话正文，无脚本无外部资源",
  },
] as const;

export function ChatArea() {
  const pending = useChatStore((s) => s.pending);
  const turnPhase = useChatStore((s) => s.turnPhase);
  const config = useChatStore((s) => s.config);
  const projects = useChatStore((s) => s.config.projects);
  // 会话档：视频会话的工作区是"上画布 + 下节点对话"，对话/生图仍是消息流
  const kind = useChatStore((s) => s.kind);
  // 两侧边栏都收起时，输入框并进对话卡（见下方 embedded）
  const sidebarCollapsed = useChatStore((s) => s.sidebarCollapsed);
  const panelCollapsed = useChatStore((s) => s.panelCollapsed);
  const narrow = useIsNarrow();
  // 取的是**话题自己的归属**（s.projectId），与后端 tools 真正落盘的根目录同一个
  // 来源：后端回合从话题台账里读归属（chat.rs run_turn），输入框旁的选择器显示的
  // 也是它——按钮、选择器、工具三处一句话。此前读 config.activeProjectId，
  // 切过激活项目后按钮开的是另一个目录、工具却落在话题的项目里
  const conversationProjectId = useChatStore((s) => s.projectId);
  const pushToast = useChatStore((s) => s.pushToast);
  const activeId = useChatStore((s) => s.activeId);
  const messages = useChatStore((s) => s.messages);
  const messageCount = useChatStore((s) => s.messages.length);
  const refreshEdits = useChatStore((s) => s.refreshEdits);
  // 头部模型行与选择器触发器同一份读数：能力档（生图/视频/音乐）显示该档的
  // 模型，空着就占位——不能沿用全局/池显示，那会在音乐会话里挂着上个模式
  // 留下的模型名（真机踩过：veo 残留在音乐档标题旁）。对话档没有档位行，
  // 保持池感知的全局显示
  const { kindKey, model: kindModel } = useKindModelLine();

  // 一轮结束会落一条新消息，切话题会整体换掉消息数组——这两个信号足够回答
  // "编辑台账什么时候可能变了"，不必在每条流式事件上各刷一次
  useEffect(() => {
    void refreshEdits();
  }, [activeId, messageCount, refreshEdits]);

  const project = projects.find((item) => item.id === conversationProjectId);
  const [exportOpen, setExportOpen] = useState(false);

  async function exportAs(format: string) {
    setExportOpen(false);
    try {
      const target = await saveDialog({
        title: "导出对话",
        defaultPath: `aglab-对话-${new Date().toISOString().slice(0, 10)}.${format === "markdown" ? "md" : format}`,
        filters: [
          format === "markdown"
            ? { name: "Markdown", extensions: ["md"] }
            : format === "json"
              ? { name: "JSON", extensions: ["json"] }
              : format === "snapshot"
                ? { name: "HTML", extensions: ["html"] }
                : { name: "JSONL", extensions: ["jsonl"] },
        ],
      });
      if (!target) return;
      const saved = await exportConversation(activeId, format, target);
      pushToast({ tone: "info", title: "已导出", detail: saved });
    } catch (error) {
      pushToast({
        tone: "error",
        title: "导出失败",
        detail: error instanceof Error ? error.message : String(error),
      });
    }
  }

  async function openWorkspace() {
    if (!project) return;
    try {
      // 传话题 id 不传路径：归属由后端从台账与配置里解析（与工具根目录同一条链）
      await invoke("workspace_open", { conversationId: activeId });
    } catch (error) {
      pushToast({
        tone: "error",
        title: "打不开工作目录",
        detail: error instanceof Error ? error.message : String(error),
      });
    }
  }

  // 两侧边栏都收起时，输入框并进对话卡内部。
  // 左边那侧的"收起"有两种来路：用户点了折叠开关，或者窗口窄到 App 直接不挂它
  // （见 App 的 showExplorerColumn）——两种都算"左侧没了"。右侧就是面板的折叠开关。
  // 判据用"实际可见性"而不是单看折叠开关：窄屏下侧栏本来就不在，用户眼里
  // 左右都是空的，那时输入框还单独占一块就说不过去了。
  //
  // 这同一组信号也是沉浸式会话的开关（App 那边同步用）：两侧边栏都收起后，
  // 整个窗口让给会话——链路监控不挂、对话卡去框铺满主区。
  const embedded = (sidebarCollapsed || narrow) && panelCollapsed;

  return (
    // min-h-0 是纵向 flex 链的承重节点：缺了它 section 的 min-height:auto 会
    // 等于内容自然高，消息一长整列撑破父级——输入框被推出窗口、消息区失去
    // 内部滚动、GoalDock（absolute bottom-4）被钉到视口外。横向的同构问题
    // 是 App.tsx 里的 min-w-0，两处要一起看。
    //
    // 这一列是"消息卡 + 输入框"两件上下分开的东西（gap 就是它们之间的缝）：
    // 栏头、消息流、编译反馈同住上面那张卡，输入框单独一份。拆开是为了让
    // 输入框的高度只由它自己的内容决定——塞进消息卡里，长消息会把输入框
    // 顶出视口，而底部栏最不能缺的就是"永远留在屏上"。
    <section className="flex min-h-0 min-w-0 flex-1 flex-col gap-2 sm:gap-3">
      {/* 请求链路监控独立成卡，长在对话区上方：它读的是"这一发走到哪儿了"，
          与"这是谁、用的哪个模型"不是一回事，挤在同一行只会互相压。
          沉浸式（两侧边栏都收起）时不挂：整个窗口让给会话本身，
          想看链路就把任意一侧边栏展开回来 */}
      {embedded ? null : <RequestChainCard />}

      {/* 沉浸式：对话区铺满主区——与标题栏、状态栏、左轨齐平，不再是浮起的一张卡，
          relative 是输入浮层的定位锚点；
          常规形态：圆角卡片浮在画布上（设计基准） */}
      <div
        className={cn(
          "relative flex min-h-0 flex-1 flex-col overflow-hidden bg-surface",
          embedded ? "" : "rounded-lg border border-border shadow-md",
        )}
      >
        <header className="flex h-12 shrink-0 items-center justify-between gap-4 border-b border-border px-4 sm:px-6">
          {/* 栏头只剩身份与动作。链路条搬去了上面那张卡，这里回到能呼吸的密度 */}
          <div className="flex min-w-0 items-baseline gap-2">
            <h1 className="truncate text-base font-semibold text-foreground">{ASSISTANT_NAME}</h1>
            {kindKey == null ? (
              config.model || config.modelPool.mode !== "off" ? (
                <p className="truncate text-xs text-muted-foreground">
                  {modelDisplayLabel(config)}
                </p>
              ) : null
            ) : (
              <p className="truncate text-xs text-muted-foreground">
                {kindModel || kindModelPlaceholder(kindKey)}
              </p>
            )}
          </div>
          <div className="flex shrink-0 items-center gap-1">
            {/* 生成中要说得再细一点（O16-3）：光一个"生成中"分不清该等还是该停，
                而压缩、审批、工具、读流的等待时间差着量级 */}
            {pending ? (
              <span className="shrink-0 text-xs text-muted-foreground">
                生成中{turnPhase ? ` · ${TURN_PHASE_LABEL[turnPhase]}` : ""}
              </span>
            ) : null}
            {/* 导出：当前话题当前分支的消息行落成 Markdown/JSON/JSONL */}
            <Menu open={exportOpen} onOpenChange={setExportOpen}>
              <Tooltip>
                <TooltipTrigger asChild>
                  <MenuTrigger asChild>
                    <button
                      type="button"
                      aria-label="导出对话"
                      className="flex size-8 items-center justify-center rounded-lg text-muted-foreground outline-none transition-colors hover:bg-accent hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45"
                    >
                      <Download className="size-4" />
                    </button>
                  </MenuTrigger>
                </TooltipTrigger>
                <TooltipContent side="bottom">导出对话</TooltipContent>
              </Tooltip>
              <MenuContent align="end" sideOffset={6}>
                {EXPORT_FORMATS.map(({ format, label, hint }) => (
                  <MenuItem key={format} onSelect={() => void exportAs(format)}>
                    <div className="min-w-0">
                      <p className="text-sm text-foreground">{label}</p>
                      <p className="mt-0.5 text-xs leading-4 text-muted-foreground">{hint}</p>
                    </div>
                  </MenuItem>
                ))}
              </MenuContent>
            </Menu>
            {/* 用 aria-disabled 而不是 disabled：真 disabled 的按钮收不到指针事件，
              提示气泡就不会出现，用户只看到一个点不动的图标 */}
            <Tooltip>
              <TooltipTrigger asChild>
                <button
                  type="button"
                  aria-disabled={!project}
                  onClick={() => void openWorkspace()}
                  className={`flex size-8 items-center justify-center rounded-lg outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45 ${
                    project
                      ? "text-muted-foreground hover:bg-accent hover:text-foreground"
                      : "text-muted-foreground/40"
                  }`}
                >
                  <FolderOpen className="size-4" />
                </button>
              </TooltipTrigger>
              <TooltipContent side="bottom">
                {project
                  ? `用文件资源管理器打开「${project.name}」`
                  : "还没有绑定工作目录，先在输入框上方选一个目录"}
              </TooltipContent>
            </Tooltip>
          </div>
        </header>

        {/* 视频/音乐会话是专用工作区（自带生成入口，不挂底部 Composer）；
            对话/生图两档原样走消息流。两侧边栏都收起时 Composer 落进这张卡里
            （栏头 / 消息 / 输入同一张卡的上下三段），否则落到卡外单独一块 */}
        {kind === "video" ? (
          <VideoWorkspace />
        ) : kind === "music" ? (
          <MusicWorkspace />
        ) : (
          <>
            <MessageList />
            {embedded ? (
              // 沉浸式悬浮输入层：输入卡浮在消息流底部，内容从它下面滚过。
              // · 宽度与正文列同轨（同一份 max-w 与内边距）：卡片左右缘对齐
              //   消息文字的左右缘，不再横贯整窗；
              // · pointer-events-none 外层 + auto 内层：浮层四周的空隙不拦截
              //   对消息的点击与滚轮；
              // · 渐隐底幕保持全宽：消息滚到卡后先淡出，不与文字硬叠；
              // · 编译反馈行跟着浮层走，不与浮卡抢底部位置。
              // 层内锚 bottom：textarea 长高、提问/计划/目标卡出现时都向上生长
              <div className="pointer-events-none absolute inset-x-0 bottom-0 z-20">
                <div
                  aria-hidden
                  className="pointer-events-none absolute inset-x-0 -top-10 bottom-0 bg-gradient-to-t from-surface via-surface/95 to-transparent"
                />
                <div className="pointer-events-auto relative mx-auto w-full max-w-[var(--content-max)] px-4 pb-4 pt-1 sm:px-6 lg:px-8">
                  <CompileFeedbackLine messages={messages} />
                  <Composer embedded />
                </div>
              </div>
            ) : (
              <CompileFeedbackLine messages={messages} />
            )}
          </>
        )}
      </div>

      {kind === "video" || kind === "music" || embedded ? null : <Composer />}
    </section>
  );
}
