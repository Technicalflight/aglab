import { useEffect, useMemo, useRef, useState } from "react";
import {
  IconChevronDown as ChevronDown,
  IconFolderPlus as FolderPlus,
  IconPhotoAi as ImageKind,
  IconMovie as VideoKind,
  IconMusic as MusicKind,
  IconPencil as SquarePen,
  IconPinned as Pin,
  IconPinnedFilled as PinFilled,
  IconPlus as Plus,
  IconTrash as Trash2,
} from "@tabler/icons-react";

import { Popover, PopoverAnchor, PopoverContent } from "@/components/ui/popover";
import { useChatStore } from "@/store/chat-store";
import type { ConversationKind } from "@/types/chat";
import { cn } from "@/lib/utils";

/**
 * 会话 / 项目（分区 11）：左起第二列的下半张卡。
 *
 * 话题按项目分组：项目的话题跟在项目下方；没有绑定项目（或项目已删）的进「最近」。
 * 组内置顶排最前，其余照新旧——跨组的相对顺序不动。
 *
 * 它与上面那张「资源管理器」是同一条竖直链上的两张独立卡片：一个管"在哪干活"
 * 的目录，一个管"干过什么"的会话，职责不同、生命周期也不同（换目录时
 * 资源管理器整棵重来，会话列表一动不动）。
 */

/** 能力会话在侧栏行首的小标：对话不标（大多数），生图/视频/音乐一眼可辨 */
const KIND_BADGES: Partial<Record<ConversationKind, { Icon: typeof ImageKind; label: string }>> = {
  image: { Icon: ImageKind, label: "生图会话" },
  video: { Icon: VideoKind, label: "视频会话" },
  music: { Icon: MusicKind, label: "音乐会话" },
};

/** 标题行内编辑：确认前是输入框，Esc 还原、回车或失焦提交。空串不提交 */
function TitleEditor({
  initial,
  onCommit,
}: {
  initial: string;
  onCommit: (title: string) => void;
}) {
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

function relativeTime(timestamp: number) {
  const minutes = Math.floor((Date.now() - timestamp) / 60_000);
  if (minutes < 1) return "刚刚";
  if (minutes < 60) return `${minutes} 分钟前`;
  // 今天与昨天按日历日说（设计基准：11:20 / 昨天），更早落到日期
  const date = new Date(timestamp);
  const now = new Date();
  const sameDay =
    date.getFullYear() === now.getFullYear() &&
    date.getMonth() === now.getMonth() &&
    date.getDate() === now.getDate();
  if (sameDay) {
    return `${String(date.getHours()).padStart(2, "0")}:${String(date.getMinutes()).padStart(2, "0")}`;
  }
  const yesterday = new Date(now);
  yesterday.setDate(now.getDate() - 1);
  if (
    date.getFullYear() === yesterday.getFullYear() &&
    date.getMonth() === yesterday.getMonth() &&
    date.getDate() === yesterday.getDate()
  ) {
    return "昨天";
  }
  return `${date.getMonth() + 1}月${date.getDate()}日`;
}

/**
 * 「会话 / 项目」卡头部的加号：悬停即展开两个动作，点击也只是确保展开（不 toggle）。
 *
 * 旧按钮直接开项目管理对话框——但这个加号顶在「会话 / 项目」两个字上，
 * 最常见的意图其实是“开一条新会话”，点进去却是目录管理，意图与落点错位。
 * 两个动作并排摆出来，各自带一句说明，比猜用户想干嘛诚实。
 *
 * 悬停开、移开收：进出浮层的空隙用两个短定时器衔接（触发器与浮层各自
 * 接管悬停），拖快了不闪，慢下来才展开。
 */
function NewSessionOrProjectButton() {
  const startConversation = useChatStore((s) => s.startConversation);
  const setProjectDialogOpen = useChatStore((s) => s.setProjectDialogOpen);
  const [open, setOpen] = useState(false);
  const closeTimer = useRef<number | null>(null);

  const cancelClose = () => {
    if (closeTimer.current !== null) {
      window.clearTimeout(closeTimer.current);
      closeTimer.current = null;
    }
  };
  const openSoon = () => {
    cancelClose();
    closeTimer.current = window.setTimeout(() => setOpen(true), 120);
  };
  const closeSoon = () => {
    cancelClose();
    closeTimer.current = window.setTimeout(() => setOpen(false), 150);
  };
  useEffect(
    () => () => {
      if (closeTimer.current !== null) window.clearTimeout(closeTimer.current);
    },
    [],
  );

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverAnchor asChild>
        <button
          type="button"
          aria-label="新建会话或新建项目"
          aria-expanded={open}
          aria-haspopup="menu"
          onClick={() => {
            cancelClose();
            setOpen(true);
          }}
          onMouseEnter={openSoon}
          onMouseLeave={closeSoon}
          className={cn(
            "flex size-6 shrink-0 items-center justify-center rounded-md outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
            open
              ? "bg-accent text-foreground"
              : "text-foreground-tertiary hover:bg-accent hover:text-foreground",
          )}
        >
          <Plus className="size-3.5" />
        </button>
      </PopoverAnchor>
      <PopoverContent
        side="bottom"
        align="end"
        sideOffset={6}
        onOpenAutoFocus={(event) => event.preventDefault()}
        onMouseEnter={cancelClose}
        onMouseLeave={closeSoon}
        className="w-44 p-1"
      >
        <button
          type="button"
          onClick={() => {
            setOpen(false);
            startConversation();
          }}
          className="flex w-full items-start gap-2 rounded-md px-2 py-1.5 text-left outline-none transition-colors hover:bg-accent focus-visible:bg-accent"
        >
          <SquarePen className="mt-0.5 size-3.5 shrink-0 text-foreground-tertiary" />
          <span className="min-w-0">
            <span className="block text-sm leading-5 text-foreground">新建会话</span>
            <span className="block text-2xs leading-4 text-muted-foreground">开一条新话题</span>
          </span>
        </button>
        <button
          type="button"
          onClick={() => {
            setOpen(false);
            setProjectDialogOpen(true);
          }}
          className="flex w-full items-start gap-2 rounded-md px-2 py-1.5 text-left outline-none transition-colors hover:bg-accent focus-visible:bg-accent"
        >
          <FolderPlus className="mt-0.5 size-3.5 shrink-0 text-foreground-tertiary" />
          <span className="min-w-0">
            <span className="block text-sm leading-5 text-foreground">新建项目</span>
            <span className="block text-2xs leading-4 text-muted-foreground">绑定一个工作目录</span>
          </span>
        </button>
      </PopoverContent>
    </Popover>
  );
}

export function SessionsCard() {
  const conversations = useChatStore((s) => s.conversations);
  const activeId = useChatStore((s) => s.activeId);
  // 正在看的这条话题，归属以 store 里的活读数为准（选择器/状态栏读的也是它）。
  // 列表条目可能落后一拍：重启恢复读的是盘上旧值、落盘去重可能跳过纯归属的变更——
  // 不对齐的话，人明明把这条话题选进了某个项目，侧栏却还把它挂在「最近」
  const liveProjectId = useChatStore((s) => s.projectId);
  const runningIds = useChatStore((s) => s.runningIds);
  const section = useChatStore((s) => s.section);
  const startConversation = useChatStore((s) => s.startConversation);
  const openConversation = useChatStore((s) => s.openConversation);
  const deleteConversation = useChatStore((s) => s.deleteConversation);
  const renameConversation = useChatStore((s) => s.renameConversation);
  const togglePin = useChatStore((s) => s.togglePin);
  const config = useChatStore((s) => s.config);
  const chooseProject = useChatStore((s) => s.chooseProject);
  const dropProject = useChatStore((s) => s.dropProject);

  const [confirming, setConfirming] = useState<string | null>(null);
  /** 行内删除确认中的项目 id（null = 没在确认）。与话题删除同一套两步交互 */
  const [confirmingProject, setConfirmingProject] = useState<string | null>(null);
  /** 行内改名中的话题 id（null = 没在改名） */
  const [renaming, setRenaming] = useState<string | null>(null);
  /** 折叠起来的项目分组 */
  const [foldedGroups, setFoldedGroups] = useState<ReadonlySet<string>>(new Set());

  const byPinned = (
    a: { pinned: boolean; updatedAt: number },
    b: { pinned: boolean; updatedAt: number },
  ) => (b.pinned ? 1 : 0) - (a.pinned ? 1 : 0) || b.updatedAt - a.updatedAt;

  // 分组数据源：活跃话题的归属用活读数覆盖，其余照列表条目
  const groupedConversations = useMemo(
    () =>
      conversations.map((conversation) =>
        conversation.id === activeId ? { ...conversation, projectId: liveProjectId } : conversation,
      ),
    [conversations, activeId, liveProjectId],
  );

  const projectGroups = useMemo(
    () =>
      config.projects.map((item) => ({
        id: item.id,
        name: item.name,
        items: groupedConversations
          .filter((conversation) => conversation.projectId === item.id)
          .sort(byPinned),
      })),
    [config.projects, groupedConversations],
  );
  const loose = useMemo(
    () =>
      groupedConversations
        .filter(
          (conversation) =>
            !config.projects.some((project) => project.id === conversation.projectId),
        )
        .sort(byPinned),
    [config.projects, groupedConversations],
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
            "group relative flex items-center rounded-md transition-colors",
            // 选中态：白色药丸从灰底里浮出来（与资源管理器同一套选中语言）
            active ? "bg-elevated shadow-sm" : "hover:bg-accent",
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
                title={`${item.title} · ${relativeTime(item.updatedAt)} · ${item.messageCount} 条`}
                className="flex min-w-0 flex-1 items-center gap-2 rounded-md py-2 pl-3.5 pr-2 text-left outline-none focus-visible:ring-2 focus-visible:ring-ring/45"
              >
                <span
                  className={cn(
                    "min-w-0 flex-1 truncate text-sm",
                    active ? "text-foreground" : "text-foreground-secondary",
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
                {/* 切走了那一轮也还在服务商上跑：不给个读数，"回去找它"就只能靠记 */}
                {runningIds.includes(item.id) ? (
                  <span
                    role="img"
                    aria-label="生成中"
                    title="这一轮还在跑"
                    className="mr-1.5 size-1.5 shrink-0 animate-pulse rounded-full bg-brand"
                  />
                ) : null}
                {/* 单行布局（设计基准）：时间靠右一行，不再另起副行 */}
                <span className="shrink-0 text-xs tabular-nums text-foreground-quaternary">
                  {relativeTime(item.updatedAt)}
                </span>
              </button>

              <button
                type="button"
                aria-label={item.pinned ? `取消置顶 ${item.title}` : `置顶 ${item.title}`}
                title={item.pinned ? "取消置顶" : "置顶：在这个分组里排最前"}
                onClick={() => void togglePin(item.id)}
                className={cn(
                  "mr-1 hidden size-6 shrink-0 items-center justify-center rounded-lg transition-colors group-hover:flex hover:bg-fill-2 hover:text-foreground",
                  item.pinned ? "text-brand-text" : "text-muted-foreground",
                )}
              >
                {item.pinned ? <PinFilled className="size-3.5" /> : <Pin className="size-3.5" />}
              </button>

              <button
                type="button"
                aria-label={`重命名话题 ${item.title}`}
                onClick={() => setRenaming(item.id)}
                className="mr-1 hidden size-6 shrink-0 items-center justify-center rounded-lg text-muted-foreground transition-colors group-hover:flex hover:bg-fill-2 hover:text-foreground"
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
                    className="rounded-lg px-1.5 py-1 text-muted-foreground transition-colors hover:bg-fill-2"
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
                  className="mr-1.5 hidden size-6 shrink-0 items-center justify-center rounded-lg text-muted-foreground transition-colors group-hover:flex hover:bg-fill-2 hover:text-foreground"
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

  return (
    <section className="flex min-h-0 min-w-0 flex-1 flex-col overflow-hidden border-t border-border">
      <header className="flex h-10 shrink-0 items-center gap-2 pl-4 pr-2">
        <h2 className="min-w-0 flex-1 truncate text-sm font-semibold text-foreground">
          会话 / 项目
        </h2>
        <NewSessionOrProjectButton />
      </header>

      <nav
        aria-label="话题列表"
        className="details-scroll min-h-0 flex-1 overflow-y-auto px-2 py-1.5"
      >
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
                // mb-2：组与组之间也是同样的道理——上一组最后一条话题选中、
                // 下一组组头又是当前项目时，两块底色会贴死（见下面 ul 的 mt-2）
                <div key={group.id} className="mb-2">
                  <div
                    className={cn(
                      "group/heading flex items-center gap-1 rounded-lg px-2 py-1.5 transition-colors",
                      // 激活项目组头只垫一层 bg-accent：它压在白色卡片上，
                      // 再深就会读成"另一个卡片"；区分当前组靠字色加深
                      // + 「新对话」按钮已经足够
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
                          className="rounded-lg px-1.5 py-0.5 text-2xs text-muted-foreground transition-colors hover:bg-fill-2"
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
                          className="hidden size-5 shrink-0 items-center justify-center rounded-lg text-muted-foreground transition-colors group-hover/heading:flex hover:bg-fill-2 hover:text-foreground focus-visible:flex"
                        >
                          <Trash2 className="size-3" />
                        </button>
                      </>
                    )}
                  </div>
                  {!folded ? (
                    group.items.length > 0 ? (
                      // mt-2 是组头与它的话题之间那道缝：组头选中时垫的是 bg-accent，
                      // 话题选中时垫的是 bg-brand/10，两块浅色方块上下贴死会连成一个
                      // 色块，"我在哪个项目"和"我在看哪条话题"就分不出来了。
                      // 比行距（space-y-1，4px）大一档，层级也跟着清楚
                      <ul className="mt-2 space-y-1 pl-2">
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
    </section>
  );
}
