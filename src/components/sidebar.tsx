import { useMemo, useState } from "react";
import { IconBooks as Books, IconChevronDown as ChevronDown, IconFolderOpen as FolderOpen, IconGitPullRequest as GitPullRequest, IconPhotoAi as ImageKind, IconMovie as VideoKind, IconPlus as Plus, IconPuzzle as Puzzle, IconSettings as Settings, IconPencil as SquarePen, IconPinned as Pin, IconPinnedFilled as PinFilled, IconClock as Timer, IconTrash as Trash2 , IconMusic as MusicKind} from "@tabler/icons-react";

import { Button } from "@/components/ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { useChatStore } from "@/store/chat-store";
import { useIsNarrow } from "@/lib/use-media-query";
import type { ConversationKind, SidebarSection } from "@/types/chat";
import { cn } from "@/lib/utils";

/** 能力会话在侧栏行首的小标：对话不标（大多数），生图/视频/音乐一眼可辨 */
const KIND_BADGES: Partial<Record<ConversationKind, { Icon: typeof ImageKind; label: string }>> = {
  image: { Icon: ImageKind, label: "生图会话" },
  video: { Icon: VideoKind, label: "视频会话" },
  music: { Icon: MusicKind, label: "音乐会话" },
};

/** 标题行内编辑：确认前是输入框，Esc 还原、回车或失焦提交。空串不提交 */
function TitleEditor({ initial, onCommit }: { initial: string; onCommit: (title: string) => void }) {
  const [draft, setDraft] = useState(initial);
  const commit = () => {
    if (draft.trim() && draft.trim() !== initial) onCommit(draft);
    else onCommit(initial); // 还原：交给父级收起编辑框
  };
  return (
    <input
      value={draft}
      autoFocus
      aria-label="话题标题"
      className="mr-1.5 min-w-0 flex-1 rounded-md border border-brand/50 bg-background px-2 py-1 text-base text-foreground outline-none focus-visible:ring-2 focus-visible:ring-ring/55"
      onChange={(event) => setDraft(event.target.value)}
      onClick={(event) => event.stopPropagation()}
      onKeyDown={(event) => {
        if (event.nativeEvent.isComposing) return;
        if (event.key === "Enter") commit();
        if (event.key === "Escape") onCommit(initial);
      }}
      onBlur={commit}
    />
  );
}

// 工具、技能、用量收进了设置页，决策读数住右栏「决策」标签：侧栏只留工作分区
const NAV: Array<{
  value: Exclude<SidebarSection, "chats" | "settings">;
  label: string;
  icon: typeof GitPullRequest;
}> = [
  { value: "review", label: "变更请求", icon: GitPullRequest },
  { value: "tasks", label: "定时任务", icon: Timer },
  { value: "knowledge", label: "资料库", icon: Books },
  { value: "plugins", label: "插件", icon: Puzzle },
];

function relativeTime(timestamp: number) {
  const minutes = Math.floor((Date.now() - timestamp) / 60_000);
  if (minutes < 1) return "刚刚";
  if (minutes < 60) return `${minutes} 分钟前`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours} 小时前`;
  const date = new Date(timestamp);
  return `${date.getMonth() + 1}月${date.getDate()}日`;
}

export function Sidebar() {
  const collapsed = useChatStore((s) => s.sidebarCollapsed);
  const conversations = useChatStore((s) => s.conversations);
  const activeId = useChatStore((s) => s.activeId);
  const runningIds = useChatStore((s) => s.runningIds);
  const section = useChatStore((s) => s.section);
  const setSection = useChatStore((s) => s.setSection);
  const startConversation = useChatStore((s) => s.startConversation);
  const openConversation = useChatStore((s) => s.openConversation);
  const deleteConversation = useChatStore((s) => s.deleteConversation);
  const renameConversation = useChatStore((s) => s.renameConversation);
  const togglePin = useChatStore((s) => s.togglePin);
  const config = useChatStore((s) => s.config);
  const chooseProject = useChatStore((s) => s.chooseProject);
  const dropProject = useChatStore((s) => s.dropProject);
  const setProjectDialogOpen = useChatStore((s) => s.setProjectDialogOpen);
  const [confirming, setConfirming] = useState<string | null>(null);
  /** 行内删除确认中的项目 id（null = 没在确认）。与话题删除同一套两步交互 */
  const [confirmingProject, setConfirmingProject] = useState<string | null>(null);
  /** 行内改名中的话题 id（null = 没在改名） */
  const [renaming, setRenaming] = useState<string | null>(null);

  const project = config.projects.find((item) => item.id === config.activeProjectId);

  // 话题按项目分组：项目的话题跟在项目下方；没有绑定项目（或项目已删）的进「最近」。
  // 组内置顶排最前，其余照新旧——跨组的相对顺序不动
  const [foldedGroups, setFoldedGroups] = useState<ReadonlySet<string>>(new Set());
  const byPinned = (a: { pinned: boolean; updatedAt: number }, b: { pinned: boolean; updatedAt: number }) =>
    (b.pinned ? 1 : 0) - (a.pinned ? 1 : 0) || b.updatedAt - a.updatedAt;
  const projectGroups = useMemo(
    () =>
      config.projects.map((item) => ({
        id: item.id,
        name: item.name,
        items: conversations
          .filter((conversation) => conversation.projectId === item.id)
          .sort(byPinned),
      })),
    [config.projects, conversations],
  );
  const loose = useMemo(
    () =>
      conversations
        .filter(
          (conversation) => !config.projects.some((project) => project.id === conversation.projectId),
        )
        .sort(byPinned),
    [config.projects, conversations],
  );

  function toggleGroup(groupId: string) {
    setFoldedGroups((previous) => {
      const next = new Set(previous);
      if (next.has(groupId)) next.delete(groupId);
      else next.add(groupId);
      return next;
    });
  }

  /** 切到某个项目：切换绑定并开一个新对话——项目之间的对话互相隔离 */
  function switchToProject(groupId: string) {
    setFoldedGroups((previous) => {
      const next = new Set(previous);
      next.delete(groupId);
      return next;
    });
    if (groupId !== config.activeProjectId) {
      // 只切默认 + 开新话题；`rebindCurrent: false`——正在看的那条话题的归属
      // 不许被顺手拽到新项目下面，它属于哪是它自己的属性
      void chooseProject(groupId, { rebindCurrent: false }).then(() => startConversation());
    }
  }

  /** 单条话题行。分组渲染共用一份，删确认、改名、高亮与"生成中"的标都在这里 */
  const renderConversationRow = (item: (typeof conversations)[number]) => {
    const active = item.id === activeId && section === "chats";

    return (
      <li key={item.id}>
        <div
          className={cn(
            "group relative flex items-center rounded-lg transition-colors",
            active ? "bg-surface" : "hover:bg-accent",
          )}
        >
          {renaming === item.id ? (
            <TitleEditor
              initial={item.title}
              onCommit={(title) => {
                setRenaming(null);
                if (title !== item.title) void renameConversation(item.id, title);
              }}
            />
          ) : (
            <>
              <button
                type="button"
                onClick={() => void openConversation(item.id)}
                aria-current={active ? "true" : undefined}
                className="min-w-0 flex-1 rounded-lg py-2.5 pr-2 pl-3.5 text-left outline-none focus-visible:ring-2 focus-visible:ring-ring/45"
              >
                {active ? (
                  <span className="absolute top-1/2 left-0 h-5 w-0.5 -translate-y-1/2 bg-brand" />
                ) : null}
                <span
                  className={cn(
                    "block truncate text-base",
                    active ? "text-foreground" : "text-muted-foreground",
                  )}
                >
                  {item.pinned ? (
                    <PinFilled
                      className="mr-1 inline size-3 -translate-y-px text-brand-text"
                      aria-label="已置顶"
                    />
                  ) : null}
                  {(() => {
                    // 能力会话的行首小标：生图/视频/音乐一眼可辨；对话不标（大多数）
                    const badge = KIND_BADGES[item.kind];
                    return badge ? (
                      <badge.Icon
                        className="mr-1 inline size-3.5 -translate-y-px text-brand-text"
                        aria-label={badge.label}
                      />
                    ) : null;
                  })()}
                  {item.title}
                </span>
                <span className="mt-0.5 block truncate text-xs text-muted-foreground">
                  {relativeTime(item.updatedAt)} · {item.messageCount} 条
                </span>
              </button>

              {/* 切走了那一轮也还在服务商上跑：不给个读数，"回去找它"就只能靠记 */}
              {runningIds.includes(item.id) ? (
                <span
                  role="img"
                  aria-label="生成中"
                  title="这一轮还在跑"
                  className="mr-1.5 size-1.5 shrink-0 animate-pulse rounded-full bg-brand"
                />
              ) : null}

              <button
                type="button"
                aria-label={item.pinned ? `取消置顶 ${item.title}` : `置顶 ${item.title}`}
                title={item.pinned ? "取消置顶" : "置顶：在这个分组里排最前"}
                onClick={() => void togglePin(item.id)}
                className={cn(
                  "mr-1 hidden size-6 shrink-0 items-center justify-center rounded-lg transition-colors group-hover:flex hover:bg-elevated hover:text-foreground",
                  item.pinned ? "text-brand-text" : "text-muted-foreground",
                )}
              >
                {item.pinned ? <PinFilled className="size-3.5" /> : <Pin className="size-3.5" />}
              </button>

              <button
                type="button"
                aria-label={`重命名话题 ${item.title}`}
                onClick={() => setRenaming(item.id)}
                className="mr-1 hidden size-6 shrink-0 items-center justify-center rounded-lg text-muted-foreground transition-colors group-hover:flex hover:bg-elevated hover:text-foreground"
              >
                <SquarePen className="size-3.5" />
              </button>

              {confirming === item.id ? (
                <span className="flex shrink-0 items-center gap-1 pr-1.5 text-xs">
                  <button
                    type="button"
                    className="rounded-lg px-1.5 py-1 text-destructive transition-colors hover:bg-destructive/15"
                    onClick={() => {
                      setConfirming(null);
                      void deleteConversation(item.id);
                    }}
                  >
                    删除
                  </button>
                  <button
                    type="button"
                    className="rounded-lg px-1.5 py-1 text-muted-foreground transition-colors hover:bg-elevated"
                    onClick={() => setConfirming(null)}
                  >
                    取消
                  </button>
                </span>
              ) : (
                <button
                  type="button"
                  aria-label={`删除话题 ${item.title}`}
                  onClick={() => setConfirming(item.id)}
                  className="mr-1.5 hidden size-6 shrink-0 items-center justify-center rounded-lg text-muted-foreground transition-colors group-hover:flex hover:bg-elevated hover:text-foreground"
                >
                  <Trash2 className="size-3.5" />
                </button>
              )}
            </>
          )}
        </div>
      </li>
    );
  };

  // 窄屏（<1024）强制走图标档：侧栏 240px 在 768px 窗口里要吃掉三分之一宽度，
  // 而主内容才是主角。宽屏下仍听用户的折叠开关——不替用户改主意。
  const forceCompact = useIsNarrow();
  const rail = collapsed || forceCompact;

  const navRow = (item: (typeof NAV)[number], collapsed: boolean) => {
    const Icon = item.icon;
    const active = section === item.value;

    return (
      <button
        // key 长在这一格自己的根上：调用它的是 NAV.map 的两个分支，
        // 折叠态包着 Tooltip 自带 key，展开态是裸元素——少了它 React 每次重渲染都告警
        key={item.value}
        type="button"
        aria-current={active ? "true" : undefined}
        onClick={() => setSection(item.value)}
        className={cn(
          "group relative flex w-full items-center rounded-lg text-left outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
          collapsed ? "size-10 justify-center px-0" : "gap-2.5 px-3.5 py-2",
          active ? "bg-surface text-foreground" : "text-muted-foreground hover:bg-accent hover:text-foreground",
        )}
      >
        {active ? (
          <span className="absolute top-1/2 left-0 h-5 w-0.5 -translate-y-1/2 bg-brand" />
        ) : null}
        <Icon className="size-4 shrink-0" />
        {collapsed ? null : <span className="truncate text-base">{item.label}</span>}
      </button>
    );
  };

  return (
    <aside
      className={cn(
        "flex shrink-0 flex-col border-r border-border bg-sidebar transition-[width] duration-[var(--dur-slow)]",
        rail ? "w-14" : "w-60",
      )}
    >
      <div className={cn("space-y-1 p-3", rail && "flex flex-col items-center gap-1.5 p-2")}>
        {rail ? (
          <Tooltip>
            <TooltipTrigger asChild>
              <Button variant="subtle" size="icon" aria-label="新对话" onClick={() => startConversation()}>
                <SquarePen className="size-4" />
              </Button>
            </TooltipTrigger>
            <TooltipContent side="right">新对话</TooltipContent>
          </Tooltip>
        ) : (
          <>
            {/* 能力会话的入口：对话 | 生图 | 视频。生图/视频段开的是带能力档的会话，
                模型得自己切到支持生成的款——入口处不替用户猜模型 */}
            <div
              role="group"
              aria-label="新建能力会话"
              className="flex w-full overflow-hidden rounded-lg border border-border bg-surface"
            >
              <Button
                variant="ghost"
                className="h-9 min-w-0 flex-1 justify-center gap-1.5 rounded-none border-r border-border font-medium"
                aria-label="新对话（对话）"
                onClick={() => startConversation("chat")}
              >
                <SquarePen className="size-4" />
                <span>对话</span>
              </Button>
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button
                    variant="ghost"
                    size="icon"
                    className="size-9 shrink-0 rounded-none border-r border-border"
                    aria-label="新对话（生图）"
                    onClick={() => startConversation("image")}
                  >
                    <ImageKind className="size-4" />
                  </Button>
                </TooltipTrigger>
                <TooltipContent side="bottom">生图会话</TooltipContent>
              </Tooltip>
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button
                    variant="ghost"
                    size="icon"
                    className="size-9 shrink-0 rounded-none"
                    aria-label="新对话（视频）"
                    onClick={() => startConversation("video")}
                  >
                    <VideoKind className="size-4" />
                  </Button>
                </TooltipTrigger>
                <TooltipContent side="bottom">视频会话</TooltipContent>
              </Tooltip>
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button
                    variant="ghost"
                    size="icon"
                    className="size-9 shrink-0 rounded-none border-l border-border"
                    aria-label="新对话（音乐）"
                    onClick={() => startConversation("music")}
                  >
                    <MusicKind className="size-4" />
                  </Button>
                </TooltipTrigger>
                <TooltipContent side="bottom">音乐会话</TooltipContent>
              </Tooltip>
            </div>
          </>
        )}

        {NAV.map((item) =>
          rail ? (
            <Tooltip key={item.value}>
              <TooltipTrigger asChild>{navRow(item, true)}</TooltipTrigger>
              <TooltipContent side="right">{item.label}</TooltipContent>
            </Tooltip>
          ) : (
            navRow(item, false)
          ),
        )}
      </div>

      {/* 项目区：项目是"在哪干活"，和下面的对话列表分开——切项目就开新对话，互不混用 */}
      {rail ? (
        <div className="flex justify-center pt-1">
          <Tooltip>
            <TooltipTrigger asChild>
              <Button
                variant="ghost"
                size="icon"
                aria-label="切换项目"
                onClick={() => setProjectDialogOpen(true)}
              >
                <FolderOpen className="size-4" />
              </Button>
            </TooltipTrigger>
            <TooltipContent side="right">
              {project ? `项目：${project.name}` : "没有项目"}
            </TooltipContent>
          </Tooltip>
        </div>
      ) : (
        <div className="flex items-center justify-between px-3 pt-1">
          <p className="px-1.5 text-xs font-medium tracking-[0.08em] text-foreground-tertiary uppercase">
            项目
          </p>
          <button
            type="button"
            aria-label="管理工作目录"
            onClick={() => setProjectDialogOpen(true)}
            className="rounded-lg p-1 text-muted-foreground transition-colors hover:bg-accent hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/45"
          >
            <Plus className="size-3.5" />
          </button>
        </div>
      )}

      {/* 话题按项目分组：项目的话题跟在项目下方，「最近」只放没有绑定工作目录的对话 */}
      {!rail ? (
        <nav className="details-scroll min-h-0 flex-1 overflow-y-auto px-2 pb-2">
          {conversations.length === 0 ? (
            <p className="px-2 py-1.5 text-xs leading-5 text-muted-foreground">
              还没有历史话题。发出一条消息后，这里会开始记录。
            </p>
          ) : (
            <>
              {projectGroups.map((group) => {
                const folded = foldedGroups.has(group.id);
                const isCurrent = group.id === config.activeProjectId;
                return (
                  <div key={group.id} className="mb-1">
                    <div
                      className={cn(
                        "group/heading flex items-center gap-1 rounded-lg px-2 py-1.5 transition-colors",
                        // 激活项目组头用 bg-accent 而非 bg-surface：整行纯白压在 #f7f7f8
                        // 侧栏上读作"渲染坏了的卡片"；区分当前组靠字色加深 + 「新对话」按钮已足够
                        isCurrent ? "bg-accent" : "hover:bg-accent",
                      )}
                    >
                      <button
                        type="button"
                        aria-expanded={!folded}
                        onClick={() => toggleGroup(group.id)}
                        className="flex min-w-0 flex-1 items-center gap-1 text-left focus-visible:outline-none"
                      >
                        <ChevronDown
                          className={cn(
                            "size-3 shrink-0 text-muted-foreground transition-transform",
                            folded && "-rotate-90",
                          )}
                        />
                        <span
                          className={cn(
                            "truncate text-xs font-medium tracking-[0.08em] uppercase",
                            isCurrent ? "text-foreground" : "text-muted-foreground",
                          )}
                        >
                          {group.name}
                        </span>
                      </button>
                      {confirmingProject === group.id ? (
                        <span className="flex shrink-0 items-center gap-1 text-xs">
                          <button
                            type="button"
                            className="rounded-lg px-1.5 py-0.5 text-2xs text-destructive transition-colors hover:bg-destructive/15"
                            onClick={() => {
                              setConfirmingProject(null);
                              void dropProject(group.id);
                            }}
                          >
                            移除
                          </button>
                          <button
                            type="button"
                            className="rounded-lg px-1.5 py-0.5 text-2xs text-muted-foreground transition-colors hover:bg-elevated"
                            onClick={() => setConfirmingProject(null)}
                          >
                            取消
                          </button>
                        </span>
                      ) : (
                        <>
                          {isCurrent ? (
                            <button
                              type="button"
                              onClick={() => startConversation()}
                              aria-label="在此项目新建对话"
                              className="shrink-0 rounded-lg px-1.5 py-0.5 text-2xs text-muted-foreground transition-colors hover:bg-accent hover:text-foreground focus-visible:outline-none"
                            >
                              新对话
                            </button>
                          ) : (
                            <button
                              type="button"
                              onClick={() => switchToProject(group.id)}
                              className="shrink-0 rounded-lg px-1.5 py-0.5 text-2xs text-muted-foreground transition-colors hover:bg-accent hover:text-foreground focus-visible:outline-none"
                            >
                              切换
                            </button>
                          )}
                          <button
                            type="button"
                            aria-label={`移除项目 ${group.name}（只解除绑定，不删除磁盘文件）`}
                            title="移除项目（不删除磁盘文件）"
                            onClick={() => setConfirmingProject(group.id)}
                            className="hidden size-5 shrink-0 items-center justify-center rounded-lg text-muted-foreground transition-colors group-hover/heading:flex hover:bg-elevated hover:text-foreground focus-visible:flex"
                          >
                            <Trash2 className="size-3" />
                          </button>
                        </>
                      )}
                    </div>
                    {!folded ? (
                      group.items.length > 0 ? (
                        <ul className="space-y-1 pl-2">
                          {group.items.map(renderConversationRow)}
                        </ul>
                      ) : (
                        <p className="px-2.5 py-1 text-xs text-muted-foreground/70">
                          这个项目还没有话题。
                        </p>
                      )
                    ) : null}
                  </div>
                );
              })}
              {loose.length > 0 ? (
                <div className="mb-1">
                  <p className="flex items-center px-2 py-1.5 text-xs font-medium tracking-[0.08em] text-foreground-tertiary uppercase">
                    最近
                  </p>
                  <ul className="space-y-1">{loose.map(renderConversationRow)}</ul>
                </div>
              ) : null}
            </>
          )}
        </nav>
      ) : (
        <div className="flex-1" />
      )}

      <div className={cn("border-t border-border p-2", rail ? "p-2" : "p-3")}>
        <Button
          variant="ghost"
          className={cn(
            "w-full",
            rail ? "justify-center px-0" : "justify-start",
            section === "settings" && "bg-surface text-foreground",
          )}
          aria-label="设置"
          onClick={() => setSection("settings")}
        >
          <Settings className="size-4" />
          {rail ? null : <span>设置</span>}
        </Button>
      </div>
    </aside>
  );
}
