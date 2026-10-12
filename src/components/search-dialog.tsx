import { useEffect, useMemo, useRef, useState } from "react";
import {
  IconBooks as Books,
  IconClock as Timer,
  IconDatabaseImport as DatabaseImport,
  IconFolderPlus as FolderPlus,
  IconGitPullRequest as GitPullRequest,
  IconMessage as MessageSquare,
  IconMoon as Moon,
  IconMovie as VideoKind,
  IconMusic as MusicKind,
  IconPhotoAi as ImageKind,
  IconPencil as SquarePen,
  IconPuzzle as Puzzle,
  IconSearch as SearchIcon,
  IconSettings as Settings,
  IconSun as Sun,
  IconTerminal2 as Terminal,
  IconBrowser as Browser,
  IconPhoto as Preview,
} from "@tabler/icons-react";

import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { sessionSearch, type SearchHit as SearchHitView } from "@/lib/chat-transport";
import {
  filterCommands,
  filterTopics,
  mergeRows,
  PANEL_COMMANDS,
  SECTION_COMMANDS,
  type CommandItem,
} from "@/lib/command-palette";
import { useChatStore } from "@/store/chat-store";
import { useMediaQuery } from "@/lib/use-media-query";
import type { ConversationKind, SidebarSection } from "@/types/chat";
import { cn } from "@/lib/utils";

/**
 * 命令中心（Ctrl+K 唤起，O7-1）。
 * 顶部搜索、命令区（切会话 / 切模式 / 分区跳转 / 工具面板 / 外观）在上、
 * 话题列表（前 9 条带 Ctrl+N 徽标，键盘直达）在下，输入 2 字以上再搜全文。
 * 一个输入框、一份 ↑↓ 光标、一个 Enter——命令与话题排进同一条可导航的列表。
 * 过滤与合并的规则住在 lib/command-palette.ts，这里只组装动作与渲染。
 */

type CommandIcon = typeof SquarePen;

type WithIcon = CommandItem & { Icon: CommandIcon };

export function SearchDialog({
  open,
  onOpenChange,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const conversations = useChatStore((s) => s.conversations);
  const config = useChatStore((s) => s.config);
  const section = useChatStore((s) => s.section);
  const openConversation = useChatStore((s) => s.openConversation);
  const startConversation = useChatStore((s) => s.startConversation);
  const setSection = useChatStore((s) => s.setSection);
  const setUi = useChatStore((s) => s.setUi);
  const updateConfig = useChatStore((s) => s.updateConfig);
  const setProjectDialogOpen = useChatStore((s) => s.setProjectDialogOpen);

  // 系统偏好只在 themeMode === "system" 时参与判断——与左轨深浅色按钮同一口径
  const prefersLight = useMediaQuery("(prefers-color-scheme: light)");
  const light = config.themeMode === "light" || (config.themeMode === "system" && prefersLight);

  const [query, setQuery] = useState("");
  const [cursor, setCursor] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);

  // ---- 全文搜索：防抖 250ms 触发后端扫描，命中按消息展示、点击直达话题 ----
  const [fulltextHits, setFulltextHits] = useState<SearchHitView[] | null>(null);
  const [fulltextBusy, setFulltextBusy] = useState(false);
  const keyword = query.trim();
  useEffect(() => {
    if (!open || keyword.length < 2) {
      setFulltextHits(null);
      return;
    }
    const timer = setTimeout(() => {
      setFulltextBusy(true);
      sessionSearch(keyword, 30)
        .then((hits) => setFulltextHits(hits))
        .catch(() => setFulltextHits(null))
        .finally(() => setFulltextBusy(false));
    }, 250);
    return () => clearTimeout(timer);
  }, [open, keyword]);

  const projectsName = useMemo(() => {
    const map = new Map<string, string>();
    for (const project of config.projects) map.set(project.id, project.name);
    return map;
  }, [config.projects]);

  // ---- 命令清单：与左轨同一套动作源（模式四档 / 分区导航 / 工具面板 / 外观）----
  const commands = useMemo<WithIcon[]>(() => {
    const modes: Array<{ kind: ConversationKind; label: string; Icon: CommandIcon }> = [
      { kind: "image", label: "生图", Icon: ImageKind },
      { kind: "video", label: "视频", Icon: VideoKind },
      { kind: "music", label: "音乐", Icon: MusicKind },
    ];
    const navIcons: Record<SidebarSection, CommandIcon> = {
      chats: SquarePen,
      review: GitPullRequest,
      tasks: Timer,
      knowledge: Books,
      agents: DatabaseImport,
      plugins: Puzzle,
      settings: Settings,
    };
    const panelIcons: Record<string, CommandIcon> = {
      preview: Preview,
      terminal: Terminal,
      browser: Browser,
    };
    return [
      {
        key: "new-chat",
        label: "新聊天",
        hint: "对话模式 · 开新会话",
        Icon: SquarePen,
        run: () => startConversation(),
      },
      ...modes.map(({ kind: value, label, Icon }) => ({
        key: `mode-${value}`,
        label: `新建${label}会话`,
        hint: `${label}模式`,
        Icon,
        run: () => startConversation(value),
      })),
      ...SECTION_COMMANDS.map(({ section: value, label }) => ({
        key: `section-${value}`,
        label: `前往${label}`,
        hint: section === value ? "当前分区" : "分区",
        Icon: navIcons[value],
        run: () => setSection(value),
      })),
      ...PANEL_COMMANDS.map(({ tab, label }) => ({
        key: `panel-${tab}`,
        label: `打开${label}面板`,
        hint: "工具栏",
        Icon: panelIcons[tab],
        run: () => setUi({ panelCollapsed: false, panelTab: tab }),
      })),
      {
        key: "theme",
        label: light ? "切换到深色主题" : "切换到浅色主题",
        hint: "外观",
        Icon: light ? Moon : Sun,
        run: () => void updateConfig({ themeMode: light ? "dark" : "light" }),
      },
      {
        key: "add-project",
        label: "添加工作目录",
        hint: "项目",
        Icon: FolderPlus,
        run: () => setProjectDialogOpen(true),
      },
    ];
  }, [light, section, setProjectDialogOpen, setSection, setUi, startConversation, updateConfig]);

  const filteredCommands = useMemo(() => filterCommands(commands, keyword), [commands, keyword]);

  const filteredTopics = useMemo(
    () =>
      filterTopics(
        conversations.map((item) => ({
          id: item.id,
          title: item.title,
          preview: item.preview,
          project: projectsName.get(item.projectId) ?? "",
        })),
        keyword,
      ),
    [conversations, projectsName, keyword],
  );

  // 命令与话题排进同一条可导航的列表：光标、Enter 都只认这一条
  const rows = useMemo(
    () => mergeRows(filteredCommands, filteredTopics),
    [filteredCommands, filteredTopics],
  );

  // 每次打开重置：搜索词和光标回到起点，输入框自动聚焦
  useEffect(() => {
    if (open) {
      setQuery("");
      setCursor(0);
      setFulltextHits(null);
      setTimeout(() => inputRef.current?.focus(), 0);
    }
  }, [open]);

  // 关闭即执行：命令动作在弹窗收起之后跑，避免焦点还挂在即将卸载的浮层里
  const closeAnd = (action: () => void) => {
    onOpenChange(false);
    action();
  };

  const chooseTopic = (id: string) => closeAnd(() => void openConversation(id));

  // Ctrl+1..9 直开话题区里对应位置的话题（只统计过滤后的结果，命令不占号）
  useEffect(() => {
    if (!open) return;
    const handler = (event: KeyboardEvent) => {
      if (!event.ctrlKey || event.shiftKey || event.altKey) return;
      const index = Number(event.key) - 1;
      if (Number.isInteger(index) && index >= 0 && index < filteredTopics.length) {
        event.preventDefault();
        chooseTopic(filteredTopics[index].id);
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, filteredTopics]);

  function onKeyDown(event: React.KeyboardEvent) {
    if (event.key === "ArrowDown") {
      event.preventDefault();
      setCursor((current) => Math.min(current + 1, Math.max(rows.length - 1, 0)));
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      setCursor((current) => Math.max(current - 1, 0));
    } else if (event.key === "Enter") {
      event.preventDefault();
      const row = rows[cursor];
      if (row?.kind === "command") {
        closeAnd(row.command.run);
      } else if (row?.kind === "topic") {
        chooseTopic(row.id);
      } else if (fulltextHits !== null && fulltextHits.length > 0) {
        // 列表过滤不中时，Enter 落到全文命中的第一条：两条路都有直达出口
        chooseTopic(fulltextHits[0].conversationId);
      }
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        aria-describedby={undefined}
        className="top-[8%] w-[560px] translate-x-[-50%] translate-y-0 overflow-hidden p-0"
      >
        <DialogTitle className="sr-only">命令面板</DialogTitle>

        <input
          ref={inputRef}
          type="text"
          value={query}
          placeholder="搜索命令与聊天 · 输入 2 字以上搜全文…"
          aria-label="搜索命令与聊天"
          spellCheck={false}
          className="h-12 w-full border-b border-border bg-transparent px-4 text-md text-foreground outline-none placeholder:text-muted-foreground focus-visible:border-brand/60"
          onChange={(event) => {
            setQuery(event.target.value);
            setCursor(0);
          }}
          onKeyDown={onKeyDown}
        />

        <div className="max-h-[430px] overflow-y-auto px-2 pb-1 pt-2">
          {filteredCommands.length > 0 ? (
            <>
              <p className="px-2 pb-1 text-xs text-muted-foreground">命令</p>
              <ul>
                {filteredCommands.map((command, index) => {
                  const active = index === cursor;
                  return (
                    <li key={command.key}>
                      <button
                        type="button"
                        onMouseEnter={() => setCursor(index)}
                        onClick={() => closeAnd(command.run)}
                        className={cn(
                          "flex w-full items-center gap-2 rounded-md px-2.5 py-2 text-left outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/55",
                          active ? "bg-surface" : "hover:bg-accent",
                        )}
                      >
                        <command.Icon className="size-3.5 shrink-0 text-muted-foreground" />
                        <span className="min-w-0 flex-1 truncate text-base text-foreground">
                          {command.label}
                        </span>
                        <span className="shrink-0 text-xs text-muted-foreground">
                          {command.hint}
                        </span>
                      </button>
                    </li>
                  );
                })}
              </ul>
            </>
          ) : null}

          <p
            className={cn(
              "px-2 pb-1 text-xs text-muted-foreground",
              filteredCommands.length > 0 ? "mt-2 border-t border-border pt-2" : null,
            )}
          >
            聊天
          </p>
          {filteredTopics.length === 0 ? (
            <p className="px-2 py-3 text-sm text-muted-foreground">
              {conversations.length === 0 ? "还没有话题。" : "没有匹配的话题。"}
            </p>
          ) : (
            <ul>
              {filteredTopics.slice(0, 12).map((item, index) => {
                const cursorIndex = filteredCommands.length + index;
                const active = cursorIndex === cursor;
                return (
                  <li key={item.id}>
                    <button
                      type="button"
                      onMouseEnter={() => setCursor(cursorIndex)}
                      onClick={() => chooseTopic(item.id)}
                      className={cn(
                        "flex w-full items-center gap-2 rounded-md px-2.5 py-2 text-left outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/55",
                        active ? "bg-surface" : "hover:bg-accent",
                      )}
                    >
                      <MessageSquare className="size-3.5 shrink-0 text-muted-foreground" />
                      <span className="min-w-0 flex-1 truncate text-base text-foreground">
                        {item.title}
                      </span>
                      {item.project ? (
                        <span className="shrink-0 text-xs text-muted-foreground">
                          {item.project}
                        </span>
                      ) : null}
                      {index < 9 ? <Badge>Ctrl+{index + 1}</Badge> : null}
                    </button>
                  </li>
                );
              })}
            </ul>
          )}
        </div>

        {fulltextHits !== null && fulltextHits.length > 0 ? (
          <div className="max-h-[220px] overflow-y-auto border-t border-border px-2 pb-1 pt-2">
            <p className="flex items-center gap-1.5 px-2 pb-1 text-xs text-muted-foreground">
              <SearchIcon className="size-3" />
              全文命中 {fulltextBusy ? "（扫描中…）" : `· ${fulltextHits.length} 条`}
            </p>
            <ul>
              {fulltextHits.slice(0, 12).map((hit) => {
                const titles = useChatStore.getState().conversations;
                const title =
                  titles.find((item) => item.id === hit.conversationId)?.title ?? "话题";
                return (
                  <li key={`${hit.conversationId}/${hit.messageId}`}>
                    <button
                      type="button"
                      onClick={() => chooseTopic(hit.conversationId)}
                      className="flex w-full items-start gap-2 rounded-md px-2.5 py-1.5 text-left outline-none transition-colors hover:bg-accent focus-visible:ring-2 focus-visible:ring-ring/55"
                    >
                      <MessageSquare className="mt-0.5 size-3.5 shrink-0 text-muted-foreground" />
                      <span className="min-w-0 flex-1">
                        <span className="block truncate text-sm text-foreground">{title}</span>
                        <span className="block truncate text-xs text-muted-foreground">
                          {hit.snippet}
                        </span>
                      </span>
                      <span className="shrink-0 text-2xs tabular-nums text-muted-foreground">
                        {hit.role === "assistant" ? "答" : hit.role === "user" ? "问" : "工具"}
                      </span>
                    </button>
                  </li>
                );
              })}
            </ul>
          </div>
        ) : null}
      </DialogContent>
    </Dialog>
  );
}

function Badge({ children }: { children: React.ReactNode }) {
  return (
    <span className="shrink-0 rounded-md border border-border bg-background px-1.5 py-0.5 text-2xs tabular-nums text-muted-foreground">
      {children}
    </span>
  );
}
