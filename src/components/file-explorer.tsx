import { useCallback, useEffect, useRef, useState } from "react";
import {
  IconChevronRight as ChevronRight,
  IconFile as FileIcon,
  IconFileText as FileText,
  IconFolder as Folder,
  IconFolderOpen as FolderOpen,
  IconLoader2 as Loader,
  IconPhoto as ImageKind,
  IconRefresh as Refresh,
} from "@tabler/icons-react";

import { dirList } from "@/lib/chat-transport";
import { effectiveProject } from "@/lib/workspace";
import { useChatStore } from "@/store/chat-store";
import type { DirEntryView } from "@/types/chat";
import { cn } from "@/lib/utils";

/**
 * 资源管理器（分区 3）：**当前这条会话的工作目录**的目录树，点一个文件就送到
 * 右侧工具栏的「预览」页里看。
 *
 * 根跟着会话走：话题自己的归属优先，没绑才回落到应用级默认（见 `lib/workspace.ts`）。
 * 切话题就换根、就重读——别的会话里模型刚写过的文件，切回来应该立刻看得见。
 *
 * 一次只拉一层：大仓库整棵读进来要好几秒，而人真正看的永远只有展开的那几格。
 * 换了工作目录就整棵丢掉重来（展开态一并清）；同一个工作目录下换话题只把已拉过
 * 的那几层重拉一遍，展开态保住——正在翻目录时切一下会话不至于把树收回去。
 *
 * 路径判据在后端（必须落在某个已绑定的工作目录内），这一层只管展示与展开。
 */

/** 一眼可辨的图标。认不出的扩展名一律普通文件，不为了好看去猜 */
const IMAGE_EXT = new Set(["png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "svg", "avif"]);
const TEXT_EXT = new Set([
  "md",
  "txt",
  "json",
  "jsonl",
  "yaml",
  "yml",
  "toml",
  "csv",
  "tsv",
  "log",
  "xml",
  "ini",
  "env",
]);

function iconFor(name: string) {
  const ext = name.includes(".") ? (name.split(".").pop() ?? "").toLowerCase() : "";
  if (IMAGE_EXT.has(ext)) return ImageKind;
  if (TEXT_EXT.has(ext)) return FileText;
  return FileIcon;
}

function formatBytes(bytes: number) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

/** 递归的一层。抽成独立组件而不是内联闭包：内联定义每次渲染都是新类型，
 *  React 会把整棵子树卸载重建，展开态下的滚动位置与动画全丢 */
function TreeLevel({
  rel,
  depth,
  children,
  expanded,
  busy,
  activeAbs,
  onToggle,
  onPick,
}: {
  rel: string;
  depth: number;
  children: Record<string, DirEntryView[]>;
  expanded: ReadonlySet<string>;
  busy: ReadonlySet<string>;
  activeAbs: string | null;
  onToggle: (rel: string) => void;
  onPick: (abs: string) => void;
}) {
  const items = children[rel];
  if (!items) return null;
  if (items.length === 0) {
    return (
      <p
        className="px-2 py-1 text-2xs text-muted-foreground/70"
        style={{ paddingLeft: depth * 12 + 22 }}
      >
        空目录
      </p>
    );
  }

  return (
    <ul role="group">
      {items.map((item) => {
        const indent = { paddingLeft: depth * 12 + 6 };
        if (item.isDir) {
          const open = expanded.has(item.rel);
          const loading = busy.has(item.rel) && !children[item.rel];
          return (
            <li key={item.abs}>
              <button
                type="button"
                aria-expanded={open}
                draggable
                onDragStart={(event) => {
                  // 页内拖拽：目录没有可读正文，落进输入框的是路径文本（composer 按
                  // 自定义 type 区分文件与目录）；点击仍是展开/折叠
                  event.dataTransfer.effectAllowed = "copy";
                  event.dataTransfer.setData("application/x-aglab-dir", item.abs);
                }}
                onClick={() => onToggle(item.rel)}
                style={indent}
                className="flex w-full min-w-0 items-center gap-1.5 rounded-md py-1 pr-2 text-left outline-none transition-colors hover:bg-accent focus-visible:ring-2 focus-visible:ring-ring/45"
              >
                <ChevronRight
                  className={cn(
                    "size-3 shrink-0 text-muted-foreground transition-transform duration-[var(--dur-fast)]",
                    open && "rotate-90",
                  )}
                />
                {open ? (
                  <FolderOpen className="size-3.5 shrink-0 text-muted-foreground" />
                ) : (
                  <Folder className="size-3.5 shrink-0 text-muted-foreground" />
                )}
                <span className="truncate text-base text-foreground">{item.name}</span>
                {loading ? (
                  <Loader className="ml-auto size-3 shrink-0 animate-spin text-muted-foreground" />
                ) : null}
              </button>
              {open ? (
                <TreeLevel
                  rel={item.rel}
                  depth={depth + 1}
                  children={children}
                  expanded={expanded}
                  busy={busy}
                  activeAbs={activeAbs}
                  onToggle={onToggle}
                  onPick={onPick}
                />
              ) : null}
            </li>
          );
        }

        const Icon = iconFor(item.name);
        const active = item.abs === activeAbs;
        return (
          <li key={item.abs}>
            <button
              type="button"
              draggable
              onDragStart={(event) => {
                // 页内拖拽：拖进输入框即挂为附件（绝对路径由 composer 的投放区接）
                event.dataTransfer.effectAllowed = "copy";
                event.dataTransfer.setData("application/x-aglab-file", item.abs);
              }}
              onClick={() => onPick(item.abs)}
              title={`${item.rel} · ${formatBytes(item.bytes)}`}
              style={indent}
              className={cn(
                "flex w-full min-w-0 items-center gap-1.5 rounded-md py-1 pr-2 text-left outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
                // 选中态：白色药丸从灰底里浮出来（设计基准），行尾一颗蓝点收尾
                active ? "bg-elevated shadow-sm" : "hover:bg-accent",
              )}
            >
              {/* 目录那份的箭头占位，让同一层的文件名左缘对齐 */}
              <span className="size-3 shrink-0" />
              <Icon
                className={cn(
                  "size-3.5 shrink-0",
                  active ? "text-brand-text" : "text-foreground-tertiary",
                )}
              />
              <span className="min-w-0 flex-1 truncate text-base text-foreground">{item.name}</span>
              {active ? (
                <span aria-hidden className="size-1.5 shrink-0 rounded-full bg-brand-text" />
              ) : null}
            </button>
          </li>
        );
      })}
    </ul>
  );
}

export function FileExplorer() {
  const config = useChatStore((s) => s.config);
  // 话题自己的归属优先：资源管理器必须跟着"我正在看的这条会话"走，
  // 而不是跟着应用级默认项目走——后者是"新建会话默认绑哪儿"
  const conversationProjectId = useChatStore((s) => s.projectId);
  const activeId = useChatStore((s) => s.activeId);
  const openPreview = useChatStore((s) => s.openPreview);
  const previewTarget = useChatStore((s) => s.previewTarget);
  const project = effectiveProject(config, conversationProjectId);
  const projectId = project?.id ?? "";

  const [children, setChildren] = useState<Record<string, DirEntryView[]>>({});
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(new Set());
  const [busy, setBusy] = useState<ReadonlySet<string>>(new Set());
  const [error, setError] = useState<string | null>(null);
  /** 手动刷新用：换个值就把已展开的那几层重拉一遍 */
  const [nonce, setNonce] = useState(0);

  /** 已经拉过的层。换话题时按它重拉——用 ref 读，免得塞进依赖引发循环 */
  const loadedRef = useRef<Set<string>>(new Set());
  /** 上一次的根：用来分辨"换了工作目录"（整棵重来）与"同目录下换话题"（只重拉） */
  const lastRootRef = useRef("");

  const load = useCallback(async (root: string, rel: string) => {
    loadedRef.current.add(rel);
    setBusy((prev) => new Set(prev).add(rel));
    try {
      const items = await dirList(root, rel);
      setChildren((prev) => ({ ...prev, [rel]: items }));
      setError(null);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setBusy((prev) => {
        const next = new Set(prev);
        next.delete(rel);
        return next;
      });
    }
  }, []);

  // 换工作目录 / 换话题 / 手动刷新，三种情况走两条路：
  //   · 换了工作目录 → 整棵推倒重来。展开态一并清掉：上一份目录里的路径对新目录
  //     没有意义，留着只会让人以为新目录里真有那些子目录
  //   · 同目录下换话题（或点刷新）→ 已拉过的那几层重拉一遍，展开态保住。
  //     别的会话里模型刚写过的文件，切回来应该立刻看得见，而不是等用户手动点刷新
  useEffect(() => {
    if (!projectId) {
      loadedRef.current = new Set();
      lastRootRef.current = "";
      setChildren({});
      setExpanded(new Set());
      setBusy(new Set());
      setError(null);
      return;
    }
    if (lastRootRef.current !== projectId) {
      lastRootRef.current = projectId;
      loadedRef.current = new Set([""]);
      setChildren({});
      setExpanded(new Set());
      setBusy(new Set());
      setError(null);
      void load(projectId, "");
      return;
    }
    for (const rel of loadedRef.current) void load(projectId, rel);
  }, [activeId, projectId, nonce, load]);

  const onToggle = useCallback(
    (rel: string) => {
      const willExpand = !expanded.has(rel);
      setExpanded((prev) => {
        const next = new Set(prev);
        if (next.has(rel)) next.delete(rel);
        else next.add(rel);
        return next;
      });
      // 已经拉到过就不重复问后端：折叠再展开是纯界面动作。
      // 这里读的是 state 而不是在 updater 里发请求——updater 必须是纯函数，
      // StrictMode 下会被调用两次，副作用会跟着发两遍
      if (willExpand && !children[rel]) void load(projectId, rel);
    },
    [expanded, children, projectId, load],
  );

  const rootItems = children[""];

  return (
    <section className="flex min-h-0 min-w-0 flex-1 flex-col overflow-hidden">
      <header className="flex h-10 shrink-0 items-center gap-2 pl-4 pr-2" title={project?.path}>
        <h2 className="min-w-0 flex-1 truncate text-sm font-semibold text-foreground">
          资源管理器
        </h2>
        <button
          type="button"
          aria-label="刷新目录"
          title="重新读取当前工作目录"
          disabled={!project}
          onClick={() => setNonce((value) => value + 1)}
          className="flex size-6 shrink-0 items-center justify-center rounded-md text-foreground-tertiary outline-none transition-colors hover:bg-accent hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45 disabled:pointer-events-none disabled:opacity-40"
        >
          <Refresh className="size-3.5" />
        </button>
      </header>

      <div className="details-scroll min-h-0 flex-1 overflow-y-auto px-1 py-1.5">
        {!project ? (
          <p className="px-2 py-1.5 text-xs leading-5 text-muted-foreground">
            还没有绑定工作目录。在输入框上方选一个目录，这里会列出它的文件；
            点文件即在右侧「预览」里打开。
          </p>
        ) : error ? (
          <div className="px-2 py-1.5">
            <p className="text-xs leading-5 text-destructive">{error}</p>
            <button
              type="button"
              onClick={() => setNonce((value) => value + 1)}
              className="mt-1.5 rounded-md px-1.5 py-0.5 text-xs text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
            >
              重试
            </button>
          </div>
        ) : !rootItems ? (
          <p className="px-2 py-1.5 text-xs text-muted-foreground">读取中…</p>
        ) : (
          <TreeLevel
            rel=""
            depth={0}
            children={children}
            expanded={expanded}
            busy={busy}
            activeAbs={previewTarget}
            onToggle={onToggle}
            onPick={openPreview}
          />
        )}
      </div>
    </section>
  );
}
