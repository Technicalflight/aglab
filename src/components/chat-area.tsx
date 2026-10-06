import { useEffect } from "react";
import { invoke } from "@tauri-apps/api/core";
import { IconFolderOpen as FolderOpen } from "@tabler/icons-react";

import { Composer } from "@/components/composer";
import { MessageList } from "@/components/message-list";
import { kindModelPlaceholder, modelDisplayLabel, useKindModelLine } from "@/components/model-picker";
import { ProbeStrip } from "@/components/probe-strip";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { VideoWorkspace } from "@/components/video-workspace";
import { MusicWorkspace } from "@/components/music-workspace";
import { ASSISTANT_NAME, useChatStore } from "@/store/chat-store";

export function ChatArea() {
  const pending = useChatStore((s) => s.pending);
  const config = useChatStore((s) => s.config);
  const projects = useChatStore((s) => s.config.projects);
  // 会话档：视频会话的工作区是"上画布 + 下节点对话"，对话/生图仍是消息流
  const kind = useChatStore((s) => s.kind);
  // 请求链路的阶段序列：顶栏模型名旁的那条胶囊动画（turn 进行中有数据）
  const journey = useChatStore((s) => s.journey);
  // 取的是**话题自己的归属**（s.projectId），与后端 tools 真正落盘的根目录同一个
  // 来源：后端回合从话题台账里读归属（chat.rs run_turn），输入框旁的选择器显示的
  // 也是它——按钮、选择器、工具三处一句话。此前读 config.activeProjectId，
  // 切过激活项目后按钮开的是另一个目录、工具却落在话题的项目里
  const conversationProjectId = useChatStore((s) => s.projectId);
  const pushToast = useChatStore((s) => s.pushToast);
  const activeId = useChatStore((s) => s.activeId);
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

  return (
    // min-h-0 是纵向 flex 链的承重节点：缺了它 section 的 min-height:auto 会
    // 等于内容自然高，消息一长整列撑破父级——输入框被推出窗口、消息区失去
    // 内部滚动、GoalDock（absolute bottom-4）被钉到视口外。横向的同构问题
    // 是 App.tsx 里的 min-w-0，两处要一起看。
    <section className="flex min-h-0 min-w-0 flex-1 flex-col bg-background">
      <header className="flex h-12 shrink-0 items-center justify-between gap-4 border-b border-border px-6">
        <div className="flex min-w-0 items-center gap-4">
          <div className="flex min-w-0 items-baseline gap-2">
            <h1 className="truncate text-base font-medium text-foreground">{ASSISTANT_NAME}</h1>
            {kindKey == null ? (
              config.model || config.modelPool.mode !== "off" ? (
                <p className="truncate font-mono text-xs text-muted-foreground">
                  {modelDisplayLabel(config)}
                </p>
              ) : null
            ) : (
              <p className="truncate font-mono text-xs text-muted-foreground">
                {kindModel || kindModelPlaceholder(kindKey)}
              </p>
            )}
          </div>
          {/* 请求链路的胶囊动画条：回合进行中光点逐格游动，完成后静置为徽章。
              挂在模型名旁边——这是全局读数，与具体哪条消息无关 */}
          {journey.length > 0 ? (
            <ProbeStrip stages={journey} live={!journey.some((stage) => stage.key === "usage")} />
          ) : null}
        </div>
        <div className="flex shrink-0 items-center gap-1">
          {pending ? <span className="text-xs text-muted-foreground">生成中</span> : null}
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
          对话/生图两档原样走消息流 + Composer */}
      {kind === "video" ? (
        <VideoWorkspace />
      ) : kind === "music" ? (
        <MusicWorkspace />
      ) : (
        <>
          <MessageList />
          <Composer />
        </>
      )}
    </section>
  );
}
