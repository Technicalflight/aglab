import { useEffect, useMemo, useRef, useState } from "react";
import {
  IconFolderPlus as FolderPlus,
  IconMessage as MessageSquare,
  IconPencil as SquarePen,
  IconSearch as SearchIcon,
} from "@tabler/icons-react";

import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { sessionSearch, type SearchHit as SearchHitView } from "@/lib/chat-transport";
import { useChatStore } from "@/store/chat-store";
import { cn } from "@/lib/utils";

/**
 * 话题搜索弹窗（Ctrl+K 唤起）。
 * 顶部搜索、中部话题列表（前 9 条带 Ctrl+N 徽标，键盘直达）、底部快捷操作。
 * 列表数据直接读 store 里的话题元信息，不额外请求。
 */
export function SearchDialog({
  open,
  onOpenChange,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const conversations = useChatStore((s) => s.conversations);
  const config = useChatStore((s) => s.config);
  const openConversation = useChatStore((s) => s.openConversation);
  const startConversation = useChatStore((s) => s.startConversation);
  const setProjectDialogOpen = useChatStore((s) => s.setProjectDialogOpen);

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

  const filtered = useMemo(() => {
    const keyword = query.trim().toLowerCase();
    const list = conversations.map((item) => ({
      id: item.id,
      title: item.title,
      preview: item.preview,
      project: projectsName.get(item.projectId) ?? "",
    }));
    if (!keyword) return list;
    return list.filter(
      (item) =>
        item.title.toLowerCase().includes(keyword) ||
        item.preview.toLowerCase().includes(keyword) ||
        item.project.toLowerCase().includes(keyword),
    );
  }, [conversations, projectsName, query]);

  // 每次打开重置：搜索词和光标回到起点，输入框自动聚焦
  useEffect(() => {
    if (open) {
      setQuery("");
      setCursor(0);
      setTimeout(() => inputRef.current?.focus(), 0);
    }
  }, [open]);

  function choose(id: string) {
    onOpenChange(false);
    void openConversation(id);
  }

  // Ctrl+1..9 直开列表里对应位置的话题（只统计过滤后的结果）
  useEffect(() => {
    if (!open) return;
    const handler = (event: KeyboardEvent) => {
      if (!event.ctrlKey || event.shiftKey || event.altKey) return;
      const index = Number(event.key) - 1;
      if (Number.isInteger(index) && index >= 0 && index < filtered.length) {
        event.preventDefault();
        choose(filtered[index].id);
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [open, filtered]);

  function onKeyDown(event: React.KeyboardEvent) {
    if (event.key === "ArrowDown") {
      event.preventDefault();
      setCursor((current) => Math.min(current + 1, Math.max(filtered.length - 1, 0)));
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      setCursor((current) => Math.max(current - 1, 0));
    } else if (event.key === "Enter") {
      event.preventDefault();
      const target = filtered[cursor];
      if (target) {
        choose(target.id);
      } else if (fulltextHits !== null && fulltextHits.length > 0) {
        // 列表过滤不中时，Enter 落到全文命中的第一条：两条路都有直达出口
        choose(fulltextHits[0].conversationId);
      }
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        aria-describedby={undefined}
        className="top-[10%] w-[560px] translate-x-[-50%] translate-y-0 overflow-hidden p-0"
      >
        <DialogTitle className="sr-only">搜索聊天</DialogTitle>

        <input
          ref={inputRef}
          type="text"
          value={query}
          placeholder="搜索聊天 · 输入 2 字以上搜全文…"
          aria-label="搜索聊天"
          spellCheck={false}
          className="h-12 w-full border-b border-border bg-transparent px-4 text-md text-foreground outline-none placeholder:text-muted-foreground focus-visible:border-brand/60"
          onChange={(event) => {
            setQuery(event.target.value);
            setCursor(0);
          }}
          onKeyDown={onKeyDown}
        />

        {fulltextHits !== null && fulltextHits.length > 0 ? (
          <div className="max-h-[220px] overflow-y-auto border-b border-border px-2 pb-1 pt-2">
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
                      onClick={() => choose(hit.conversationId)}
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

        <div className="max-h-[380px] overflow-y-auto px-2 pb-1 pt-2">
          <p className="px-2 pb-1 text-xs text-muted-foreground">聊天</p>
          {filtered.length === 0 ? (
            <p className="px-2 py-3 text-sm text-muted-foreground">
              {conversations.length === 0 ? "还没有话题。" : "没有匹配的话题。"}
            </p>
          ) : (
            <ul>
              {filtered.slice(0, 12).map((item, index) => {
                const active = index === cursor;
                return (
                  <li key={item.id}>
                    <button
                      type="button"
                      onMouseEnter={() => setCursor(index)}
                      onClick={() => choose(item.id)}
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

        <div className="border-t border-border px-2 py-2">
          <p className="px-2 pb-1 text-xs text-muted-foreground">快捷操作</p>
          <ul>
            <li>
              <button
                type="button"
                onClick={() => {
                  onOpenChange(false);
                  startConversation();
                }}
                className="flex w-full items-center gap-2 rounded-lg px-2.5 py-2 text-left text-base text-foreground outline-none transition-colors hover:bg-accent focus-visible:ring-2 focus-visible:ring-ring/45"
              >
                <SquarePen className="size-3.5 text-muted-foreground" />
                <span className="flex-1">新聊天</span>
                <Badge>Ctrl+N</Badge>
              </button>
            </li>
            <li>
              <button
                type="button"
                onClick={() => {
                  onOpenChange(false);
                  setProjectDialogOpen(true);
                }}
                className="flex w-full items-center gap-2 rounded-lg px-2.5 py-2 text-left text-base text-foreground outline-none transition-colors hover:bg-accent focus-visible:ring-2 focus-visible:ring-ring/45"
              >
                <FolderPlus className="size-3.5 text-muted-foreground" />
                <span className="flex-1">添加工作目录</span>
                <Badge>Ctrl+O</Badge>
              </button>
            </li>
          </ul>
        </div>
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
