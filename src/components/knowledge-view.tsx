import { useCallback, useEffect, useMemo, useState } from "react";
import { open as openFileDialog } from "@tauri-apps/plugin-dialog";
import {
  IconBooks as Books,
  IconChevronLeft as ChevronLeft,
  IconFileImport as FileImport,
  IconLayoutGrid as LayoutGrid,
  IconList as List,
  IconPencil as Pencil,
  IconPlus as Plus,
  IconRefresh as RefreshCw,
  IconSearch as SearchIcon,
  IconTrash as Trash,
} from "@tabler/icons-react";

import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { useChatStore } from "@/store/chat-store";
import { cn } from "@/lib/utils";
import { ContentColumn } from "@/components/ui/content-column";
import { ListSkeleton, PanelSkeleton } from "@/components/ui/loading-skeleton";
import {
  type KbDetail,
  type KbHit,
  type KbSummary,
  type KbWorkspaceFilter,
  filterKbs,
  formatChars,
  kbCreate,
  kbDelete,
  kbDocAdd,
  kbDocDelete,
  kbDocGet,
  kbDocUpdate,
  kbGet,
  kbImportFiles,
  kbImportWiki,
  kbList,
  kbSearch,
  kbUpdate,
  relativeTime,
} from "@/lib/knowledge";

const inputClass =
  "h-9 w-full rounded-lg border border-input bg-background px-3 text-base text-foreground outline-none transition-colors focus-visible:border-brand/50 focus-visible:ring-2 focus-visible:ring-ring/35";

function message(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}

/** 头部那对 tab：资料库与 Repo Wiki 导入。 */
function HeaderTab({
  active,
  onClick,
  children,
}: {
  active: boolean;
  onClick: () => void;
  children: string;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-current={active ? "page" : undefined}
      className={cn(
        "rounded-lg px-1.5 py-1 text-base outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
        active ? "font-medium text-foreground" : "text-muted-foreground hover:text-foreground",
      )}
    >
      {children}
    </button>
  );
}

export function KnowledgeView() {
  const projects = useChatStore((s) => s.config.projects);
  const activeProjectId = useChatStore((s) => s.config.activeProjectId);
  const pushToast = useChatStore((s) => s.pushToast);

  const [items, setItems] = useState<KbSummary[]>([]);
  const [loaded, setLoaded] = useState(false);
  const [tab, setTab] = useState<"kb" | "wiki">("kb");
  const [keyword, setKeyword] = useState("");
  const [workspace, setWorkspace] = useState<KbWorkspaceFilter>("all");
  const [layout, setLayout] = useState<"grid" | "list">("grid");

  // 创建对话框
  const [creating, setCreating] = useState(false);
  const [draftName, setDraftName] = useState("");
  const [draftDesc, setDraftDesc] = useState("");
  const [draftWorkspace, setDraftWorkspace] = useState<string>("none");
  const [busy, setBusy] = useState(false);

  // 库详情
  const [detail, setDetail] = useState<KbDetail | null>(null);
  const [detailLoading, setDetailLoading] = useState(false);
  const [docKeyword, setDocKeyword] = useState("");
  const [hits, setHits] = useState<KbHit[] | null>(null);
  const [confirmingKb, setConfirmingKb] = useState<string | null>(null);
  const [confirmingDoc, setConfirmingDoc] = useState<string | null>(null);
  const [editingKb, setEditingKb] = useState(false);

  // 文档查看/编辑共用的一个对话框：docId 为空 = 新建
  const [docEditing, setDocEditing] = useState<{ docId: string | null; title: string; content: string } | null>(null);

  // Repo Wiki 导入：目标库 + 仓库地址
  const [wikiKbId, setWikiKbId] = useState("");
  const [wikiRepo, setWikiRepo] = useState("");
  const [wikiBusy, setWikiBusy] = useState(false);

  const refresh = useCallback(async () => {
    try {
      setItems(await kbList());
      setLoaded(true);
    } catch (error) {
      pushToast({ tone: "error", title: "资料库列表没读到", detail: message(error) });
    }
  }, [pushToast]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const loadDetail = useCallback(
    async (id: string) => {
      setDetailLoading(true);
      try {
        setDetail(await kbGet(id));
      } catch (error) {
        pushToast({ tone: "error", title: "资料库详情没读到", detail: message(error) });
        setDetail(null);
      } finally {
        setDetailLoading(false);
      }
    },
    [pushToast],
  );

  const openDetail = useCallback(
    (id: string) => {
      setDocKeyword("");
      setHits(null);
      setConfirmingDoc(null);
      void loadDetail(id);
    },
    [loadDetail],
  );

  // 库内检索：250ms 防抖。kb_search 是跨库的，这里按库 id 收窄成"库内"
  useEffect(() => {
    if (!detail) return;
    const query = docKeyword.trim();
    if (!query) {
      setHits(null);
      return;
    }
    const timer = window.setTimeout(async () => {
      try {
        const all = await kbSearch(query, undefined, 50);
        setHits(all.filter((hit) => hit.kbId === detail.id));
      } catch (error) {
        pushToast({ tone: "error", title: "检索失败", detail: message(error) });
      }
    }, 250);
    return () => window.clearTimeout(timer);
  }, [docKeyword, detail, pushToast]);

  const visible = useMemo(() => filterKbs(items, keyword, workspace), [items, keyword, workspace]);

  function openCreateDialog() {
    setDraftWorkspace(activeProjectId && projects.some((p) => p.id === activeProjectId) ? activeProjectId : "none");
    setDraftName("");
    setDraftDesc("");
    setCreating(true);
  }

  async function submitCreate() {
    const name = draftName.trim();
    if (!name || busy) return;
    setBusy(true);
    try {
      const created = await kbCreate(name, draftDesc.trim(), draftWorkspace === "none" ? "" : draftWorkspace);
      setCreating(false);
      await refresh();
      openDetail(created.id);
    } catch (error) {
      pushToast({ tone: "error", title: "创建失败", detail: message(error) });
    } finally {
      setBusy(false);
    }
  }

  async function submitRename() {
    if (!detail || busy) return;
    const name = draftName.trim();
    if (!name) return;
    setBusy(true);
    try {
      await kbUpdate(detail.id, name, draftDesc.trim());
      setEditingKb(false);
      setDetail(await kbGet(detail.id));
      await refresh();
    } catch (error) {
      pushToast({ tone: "error", title: "保存失败", detail: message(error) });
    } finally {
      setBusy(false);
    }
  }

  async function removeKb(id: string) {
    try {
      await kbDelete(id);
      setConfirmingKb(null);
      if (detail?.id === id) setDetail(null);
      await refresh();
    } catch (error) {
      pushToast({ tone: "error", title: "删除失败", detail: message(error) });
    }
  }

  async function removeDoc(docId: string) {
    if (!detail) return;
    try {
      await kbDocDelete(detail.id, docId);
      setConfirmingDoc(null);
      setDetail(await kbGet(detail.id));
      await refresh();
    } catch (error) {
      pushToast({ tone: "error", title: "删除失败", detail: message(error) });
    }
  }

  async function openDoc(docId: string) {
    if (!detail) return;
    try {
      const doc = await kbDocGet(detail.id, docId);
      setDocEditing({ docId: doc.id, title: doc.title, content: doc.content });
    } catch (error) {
      pushToast({ tone: "error", title: "文档没读到", detail: message(error) });
    }
  }

  async function saveDoc() {
    if (!detail || !docEditing || busy) return;
    setBusy(true);
    try {
      if (docEditing.docId) {
        await kbDocUpdate(detail.id, docEditing.docId, docEditing.title, docEditing.content);
      } else {
        await kbDocAdd(detail.id, docEditing.title, docEditing.content);
      }
      setDocEditing(null);
      setDetail(await kbGet(detail.id));
      await refresh();
    } catch (error) {
      pushToast({ tone: "error", title: "保存失败", detail: message(error) });
    } finally {
      setBusy(false);
    }
  }

  async function importFiles() {
    if (!detail) return;
    try {
      const picked = await openFileDialog({ multiple: true, title: "选择要导入的文本文件" });
      if (!picked) return;
      const paths = Array.isArray(picked) ? picked : [picked];
      if (paths.length === 0) return;
      const outcome = await kbImportFiles(detail.id, paths);
      pushToast({
        tone: outcome.added > 0 ? "info" : "error",
        title: `导入完成：新增 ${outcome.added} 篇，跳过 ${outcome.skipped} 个`,
        detail: outcome.skippedNames.length > 0 ? outcome.skippedNames.join("；") : undefined,
      });
      setDetail(await kbGet(detail.id));
      await refresh();
    } catch (error) {
      pushToast({ tone: "error", title: "导入失败", detail: message(error) });
    }
  }

  async function importWiki() {
    const kbId = wikiKbId;
    const repo = wikiRepo.trim();
    if (!kbId || !repo || wikiBusy) return;
    setWikiBusy(true);
    try {
      const outcome = await kbImportWiki(kbId, repo);
      pushToast({
        tone: "info",
        title: `Wiki 导入完成：新增 ${outcome.added} 篇，跳过 ${outcome.skipped} 篇`,
        detail: outcome.skippedNames.length > 0 ? outcome.skippedNames.join("；") : undefined,
      });
      setWikiRepo("");
      await refresh();
      openDetail(kbId);
    } catch (error) {
      pushToast({ tone: "error", title: "Wiki 导入失败", detail: message(error) });
    } finally {
      setWikiBusy(false);
    }
  }

  // 列表到位后给 Wiki 导入一个默认目标库：省一次必点
  useEffect(() => {
    if (!wikiKbId && items.length > 0) setWikiKbId(items[0].id);
  }, [items, wikiKbId]);

  // ---- 列表页 ----

  const emptyAll = loaded && items.length === 0;
  const emptyFiltered = loaded && items.length > 0 && visible.length === 0;

  const listBody = (
    <>
      <div className="mt-12">
        <h1 className="text-2xl font-semibold tracking-tight text-foreground">构建 AI-Native 资料库</h1>
        <p className="mt-2 text-base text-muted-foreground">AI 智能体随时调用</p>
      </div>

      <div className="mt-9 flex items-center justify-between gap-3">
        <Select value={workspace} onValueChange={(value) => setWorkspace(value as KbWorkspaceFilter)}>
          <SelectTrigger className="h-8 w-40 text-sm">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="all">全部工作目录</SelectItem>
            <SelectItem value="none">未绑定工作目录</SelectItem>
            {projects.map((project) => (
              <SelectItem key={project.id} value={project.id}>
                {project.name}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>

        <div className="flex items-center rounded-lg border border-border bg-surface p-1">
          {(
            [
              ["grid", LayoutGrid, "网格视图"],
              ["list", List, "列表视图"],
            ] as const
          ).map(([value, Icon, label]) => (
            <button
              key={value}
              type="button"
              aria-label={label}
              aria-pressed={layout === value}
              onClick={() => setLayout(value)}
              className={cn(
                "flex size-7 items-center justify-center rounded-md outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
                layout === value ? "bg-background text-foreground" : "text-muted-foreground hover:text-foreground",
              )}
            >
              <Icon className="size-4" />
            </button>
          ))}
        </div>
      </div>

      {!loaded ? (
        <ListSkeleton rows={4} className="mt-4" label="正在加载资料库列表" />
      ) : null}

      {emptyAll ? (
        <div className="mt-8 rounded-xl border border-dashed border-border bg-surface/40 px-10 py-24 text-center">
          <Books className="mx-auto size-10 text-muted-foreground/50" />
          <p className="mt-7 text-lg font-medium text-foreground">还没有资料库</p>
          <p className="mx-auto mt-3 max-w-sm text-base leading-6 text-muted-foreground">
            创建资料库，集中整理文件和 Repo Wiki 内容，供 AI 检索和使用
          </p>
          <Button variant="brand" className="mt-10" onClick={openCreateDialog}>
            <Plus className="size-4" />
            创建资料库
          </Button>
        </div>
      ) : emptyFiltered ? (
        <div className="mt-8 rounded-xl border border-dashed border-border px-10 py-20 text-center">
          <p className="text-base text-muted-foreground">没有匹配的资料库。换个关键词或工作目录试试。</p>
        </div>
      ) : layout === "grid" ? (
        <div className="mt-8 grid grid-cols-2 gap-4">
          {visible.map((item) => (
            <div
              key={item.id}
              role="button"
              tabIndex={0}
              onClick={() => openDetail(item.id)}
              onKeyDown={(event) => event.key === "Enter" && openDetail(item.id)}
              className="group relative cursor-pointer rounded-lg border border-border bg-surface p-5 outline-none transition-colors hover:border-brand/40 focus-visible:ring-2 focus-visible:ring-ring/45"
            >
              {confirmingKb === item.id ? (
                <div className="flex items-center gap-2">
                  <span className="min-w-0 flex-1 truncate text-sm text-muted-foreground">删除「{item.name}」？</span>
                  <Button variant="ghost" size="sm" className="h-7 text-xs" onClick={(event) => { event.stopPropagation(); setConfirmingKb(null); }}>
                    取消
                  </Button>
                  <Button variant="ghost" size="sm" className="h-7 text-xs text-destructive hover:bg-destructive/15" onClick={(event) => { event.stopPropagation(); void removeKb(item.id); }}>
                    删除
                  </Button>
                </div>
              ) : (
                <>
                  <button
                    type="button"
                    aria-label={`删除资料库 ${item.name}`}
                    onClick={(event) => { event.stopPropagation(); setConfirmingKb(item.id); }}
                    className="absolute top-5 right-5 hidden size-6 items-center justify-center rounded-lg text-muted-foreground transition-colors group-hover:flex hover:bg-elevated hover:text-foreground"
                  >
                    <Trash className="size-3.5" />
                  </button>
                  <p className="line-clamp-1 pr-6 text-base font-medium text-foreground">{item.name}</p>
                  <p className="mt-2.5 line-clamp-2 min-h-[40px] text-sm leading-5 text-muted-foreground">
                    {item.description || "没有描述"}
                  </p>
                  <p className="mt-4 text-xs text-muted-foreground">
                    {item.docCount} 篇 · {formatChars(item.chars)} · {relativeTime(item.updatedAt)}
                  </p>
                </>
              )}
            </div>
          ))}
          {/* 添加卡片：创建入口永远在列表末尾，建了多少个库都找得到 */}
          <button
            type="button"
            onClick={openCreateDialog}
            className="flex min-h-[132px] cursor-pointer flex-col items-center justify-center gap-2.5 rounded-lg border border-dashed border-border text-muted-foreground outline-none transition-colors hover:border-brand/50 hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45"
          >
            <Plus className="size-5" />
            <span className="text-sm">创建资料库</span>
          </button>
        </div>
      ) : (
        <ul className="mt-8 space-y-2.5">
          {visible.map((item) => (
            <li key={item.id}>
              <div
                role="button"
                tabIndex={0}
                onClick={() => openDetail(item.id)}
                onKeyDown={(event) => event.key === "Enter" && openDetail(item.id)}
                className="group flex cursor-pointer items-center gap-3 rounded-lg border border-border bg-surface px-4 py-3.5 outline-none transition-colors hover:border-brand/40 focus-visible:ring-2 focus-visible:ring-ring/45"
              >
                <Books className="size-4 shrink-0 text-muted-foreground" />
                <div className="min-w-0 flex-1">
                  <p className="truncate text-base font-medium text-foreground">{item.name}</p>
                  <p className="truncate text-xs text-muted-foreground">
                    {item.description || "没有描述"}
                  </p>
                </div>
                <span className="shrink-0 text-xs text-muted-foreground">
                  {item.docCount} 篇 · {formatChars(item.chars)} · {relativeTime(item.updatedAt)}
                </span>
                {confirmingKb === item.id ? (
                  <span className="flex shrink-0 items-center gap-1">
                    <Button variant="ghost" size="sm" className="h-7 text-xs" onClick={(event) => { event.stopPropagation(); setConfirmingKb(null); }}>
                      取消
                    </Button>
                    <Button variant="ghost" size="sm" className="h-7 text-xs text-destructive hover:bg-destructive/15" onClick={(event) => { event.stopPropagation(); void removeKb(item.id); }}>
                      删除
                    </Button>
                  </span>
                ) : (
                  <button
                    type="button"
                    aria-label={`删除资料库 ${item.name}`}
                    onClick={(event) => { event.stopPropagation(); setConfirmingKb(item.id); }}
                    className="hidden size-6 shrink-0 items-center justify-center rounded-lg text-muted-foreground transition-colors group-hover:flex hover:bg-elevated hover:text-foreground"
                  >
                    <Trash className="size-3.5" />
                  </button>
                )}
              </div>
            </li>
          ))}
          <li>
            <button
              type="button"
              onClick={openCreateDialog}
              className="flex w-full cursor-pointer items-center justify-center gap-2 rounded-lg border border-dashed border-border px-4 py-3.5 text-muted-foreground outline-none transition-colors hover:border-brand/50 hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45"
            >
              <Plus className="size-4" />
              <span className="text-sm">新建资料库</span>
            </button>
          </li>
        </ul>
      )}
    </>
  );

  // ---- 详情页 ----

  const docsOrHits = detail ? (
    hits !== null ? (
      hits.length === 0 ? (
        <p className="mt-8 rounded-lg border border-border px-6 py-12 text-center text-sm text-muted-foreground">
          库内没有匹配「{docKeyword.trim()}」的段落。
        </p>
      ) : (
        <ul className="mt-6 space-y-2.5">
          {hits.map((hit) => (
            <li key={hit.docId} className="rounded-lg border border-border bg-surface px-4 py-3.5">
              <button type="button" className="w-full rounded-md text-left outline-none focus-visible:ring-2 focus-visible:ring-ring/55" onClick={() => void openDoc(hit.docId)}>
                <p className="text-base font-medium text-foreground">{hit.docTitle}</p>
                <p className="mt-1 text-sm leading-5 text-muted-foreground">{hit.snippet}</p>
                <p className="mt-1 text-xs text-muted-foreground/70">相关度 {hit.score.toFixed(1)} · 更新 {relativeTime(hit.updatedAt)}</p>
              </button>
            </li>
          ))}
        </ul>
      )
    ) : detail.docs.length === 0 ? (
      <div className="mt-8 rounded-xl border border-dashed border-border px-10 py-24 text-center">
        <p className="text-lg font-medium text-foreground">还没有文档</p>
        <p className="mx-auto mt-3 max-w-sm text-base leading-6 text-muted-foreground">
          添加文档或导入文件后，AI 检索才找得到这里的内容
        </p>
        <div className="mt-10 flex justify-center gap-3">
          <Button variant="brand" onClick={() => setDocEditing({ docId: null, title: "", content: "" })}>
            <Plus className="size-4" />
            添加文档
          </Button>
          <Button variant="subtle" onClick={() => void importFiles()}>
            <FileImport className="size-4" />
            导入文件
          </Button>
        </div>
      </div>
    ) : (
      <ul className="mt-6 space-y-2.5">
        {detail.docs.map((doc) => (
          <li
            key={doc.id}
            className="group flex items-center gap-3 rounded-lg border border-border bg-surface px-4 py-3.5 transition-colors hover:border-brand/40"
          >
            <button type="button" className="min-w-0 flex-1 rounded-md text-left outline-none focus-visible:ring-2 focus-visible:ring-ring/55" onClick={() => void openDoc(doc.id)}>
              <p className="truncate text-base font-medium text-foreground">{doc.title}</p>
              <p className="truncate text-xs text-muted-foreground">
                {formatChars(doc.chars)} · {relativeTime(doc.updatedAt)} · {doc.source}
              </p>
            </button>
            {confirmingDoc === doc.id ? (
              <span className="flex shrink-0 items-center gap-1">
                <Button variant="ghost" size="sm" className="h-7 text-xs" onClick={() => setConfirmingDoc(null)}>
                  取消
                </Button>
                <Button variant="ghost" size="sm" className="h-7 text-xs text-destructive hover:bg-destructive/15" onClick={() => void removeDoc(doc.id)}>
                  删除
                </Button>
              </span>
            ) : (
              <button
                type="button"
                aria-label={`删除文档 ${doc.title}`}
                onClick={() => setConfirmingDoc(doc.id)}
                className="hidden size-6 shrink-0 items-center justify-center rounded-lg text-muted-foreground transition-colors group-hover:flex hover:bg-elevated hover:text-foreground"
              >
                <Trash className="size-3.5" />
              </button>
            )}
          </li>
        ))}
      </ul>
    )
  ) : null;

  return (
    // min-h-0：与 chat-area 同理，缺了它列表一长就会把整区撑出窗口
    <section className="flex min-h-0 min-w-0 flex-1 flex-col bg-background">
      <header className="flex h-12 shrink-0 items-center gap-4 border-b border-border px-6">
        {detail ? (
          <button
            type="button"
            onClick={() => setDetail(null)}
            className="flex items-center gap-1 rounded-lg px-1.5 py-1 text-sm text-muted-foreground outline-none transition-colors hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45"
          >
            <ChevronLeft className="size-3.5" />
            资料库
          </button>
        ) : (
          <nav className="flex items-center gap-3">
            <HeaderTab active={tab === "kb"} onClick={() => setTab("kb")}>
              资料库
            </HeaderTab>
            <HeaderTab active={tab === "wiki"} onClick={() => setTab("wiki")}>
              Repo Wiki
            </HeaderTab>
          </nav>
        )}
        <span className="flex-1" />
        {detail ? (
          <div className="flex items-center gap-2">
            <Button variant="subtle" size="sm" onClick={() => setDocEditing({ docId: null, title: "", content: "" })}>
              <Plus className="size-4" />
              添加文档
            </Button>
            <Button variant="subtle" size="sm" onClick={() => void importFiles()}>
              <FileImport className="size-4" />
              导入文件
            </Button>
          </div>
        ) : (
          <div className="flex items-center gap-3">
            <Button
              variant="ghost"
              size="icon-sm"
              aria-label="刷新资料库列表"
              onClick={() => void refresh()}
            >
              <RefreshCw className="size-4" />
            </Button>
            <div className="relative">
              <SearchIcon className="pointer-events-none absolute top-1/2 left-3 size-4 -translate-y-1/2 text-muted-foreground" />
              <input aria-label="搜索资料库"
                value={keyword}
                onChange={(event) => setKeyword(event.target.value)}
                placeholder="搜索资料库"
                className={cn(inputClass, "h-9 w-60 pl-9 text-sm")}
              />
            </div>
          </div>
        )}
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto">
        <ContentColumn>
          {!loaded ? (
            <PanelSkeleton className="mt-10" label="正在读资料库" />
          ) : detailLoading ? (
            <PanelSkeleton className="mt-10" label="正在打开资料库" />
          ) : detail ? (
            <>
              <div className="flex items-start gap-3">
                <div className="min-w-0 flex-1">
                  <h1 className="text-2xl font-semibold tracking-tight text-foreground">{detail.name}</h1>
                  {detail.description ? (
                    <p className="mt-1.5 text-sm leading-5 text-muted-foreground">{detail.description}</p>
                  ) : null}
                  <p className="mt-3 text-xs text-muted-foreground">
                    {detail.docCount} 篇 · {formatChars(detail.chars)} · 创建于 {relativeTime(detail.createdAt)}
                  </p>
                </div>
                <div className="flex shrink-0 items-center gap-1">
                  <Button
                    variant="ghost"
                    size="sm"
                    onClick={() => {
                      setDraftName(detail.name);
                      setDraftDesc(detail.description);
                      setEditingKb(true);
                    }}
                  >
                    <Pencil className="size-3.5" />
                    编辑
                  </Button>
                  {confirmingKb === detail.id ? (
                    <>
                      <Button variant="ghost" size="sm" onClick={() => setConfirmingKb(null)}>
                        取消
                      </Button>
                      <Button variant="ghost" size="sm" className="text-destructive hover:bg-destructive/15" onClick={() => void removeKb(detail.id)}>
                        删除资料库
                      </Button>
                    </>
                  ) : (
                    <Button variant="ghost" size="sm" onClick={() => setConfirmingKb(detail.id)}>
                      <Trash className="size-3.5" />
                      删除
                    </Button>
                  )}
                </div>
              </div>

              <div className="relative mt-9">
                <SearchIcon className="pointer-events-none absolute top-1/2 left-3 size-4 -translate-y-1/2 text-muted-foreground" />
                <input aria-label="搜索文档"
                  value={docKeyword}
                  onChange={(event) => setDocKeyword(event.target.value)}
                  placeholder="在本库内检索，回车看命中段落"
                  className={cn(inputClass, "h-10 pl-9")}
                />
              </div>
              {docsOrHits}
            </>
          ) : tab === "wiki" ? (
            <div className="mt-12">
              <h1 className="text-2xl font-semibold tracking-tight text-foreground">Repo Wiki</h1>
              <p className="mt-2 max-w-2xl text-base leading-6 text-muted-foreground">
                把 GitHub 仓库的 Wiki 整本拉进资料库：浅克隆 <span className="font-mono">&lt;repo&gt;.wiki.git</span>
                ，每页 Markdown 存成一篇文档（Home 页排最前），随后自动排入语义索引。
              </p>

              {items.length === 0 ? (
                <div className="mt-9 rounded-xl border border-dashed border-border px-10 py-24 text-center">
                  <p className="text-lg font-medium text-foreground">还没有资料库</p>
                  <p className="mx-auto mt-3 max-w-sm text-base leading-6 text-muted-foreground">
                    先回「资料库」标签创建一个库，再来导入 Wiki——页面总要有个落点。
                  </p>
                </div>
              ) : (
                <div className="mt-9 max-w-2xl rounded-xl border border-border bg-surface px-8 py-7">
                  {/* label 包 Select 会把空白处的点击转发给触发按钮（浏览器把 label
                      的激活行为落在第一个控件上）——下拉框看着自己拉开了，用 div */}
                  <div className="block">
                    <span className="text-sm text-muted-foreground">目标资料库</span>
                    <Select value={wikiKbId} onValueChange={setWikiKbId}>
                      <SelectTrigger className="mt-1.5 w-72">
                        <SelectValue placeholder="选一个资料库" />
                      </SelectTrigger>
                      <SelectContent>
                        {items.map((kb) => (
                          <SelectItem key={kb.id} value={kb.id}>
                            {kb.name}
                          </SelectItem>
                        ))}
                      </SelectContent>
                    </Select>
                  </div>
                  <label className="mt-4 block">
                    <span className="text-sm text-muted-foreground">仓库</span>
                    <input
                      value={wikiRepo}
                      onChange={(event) => setWikiRepo(event.target.value)}
                      placeholder="owner/repo 或完整的 GitHub URL"
                      spellCheck={false}
                      aria-label="Wiki 仓库"
                      className={cn(inputClass, "mt-1.5 max-w-md font-mono")}
                      onKeyDown={(event) => {
                        if (event.key === "Enter" && !wikiBusy) void importWiki();
                      }}
                    />
                  </label>
                  <div className="mt-5 flex items-center gap-3">
                    <Button disabled={wikiBusy || !wikiKbId || !wikiRepo.trim()} onClick={() => void importWiki()}>
                      {wikiBusy ? <RefreshCw className="size-4 animate-spin" /> : <FileImport className="size-4" />}
                      {wikiBusy ? "克隆导入中…" : "导入 Wiki"}
                    </Button>
                    <span className="text-xs leading-5 text-muted-foreground">
                      私有仓库走本机 git 已有凭据；同名页面已存在或超 2MB 时跳过。
                    </span>
                  </div>
                </div>
              )}
            </div>
          ) : (
            listBody
          )}
        </ContentColumn>
      </div>

      {/* 创建 / 重命名 */}
      <Dialog
        open={creating || editingKb}
        onOpenChange={(next) => {
          if (!next) {
            setCreating(false);
            setEditingKb(false);
          }
        }}
      >
        <DialogContent className="w-[440px]">
          <DialogTitle>{editingKb ? "编辑资料库" : "创建资料库"}</DialogTitle>
          <div className="mt-4 space-y-4">
            <label className="block">
              <span className="text-sm text-muted-foreground">名称</span>
              <input
                value={draftName}
                onChange={(event) => setDraftName(event.target.value)}
                placeholder="例如：钓鱼笔记"
                autoFocus
                className={cn(inputClass, "mt-1.5")}
              />
            </label>
            <label className="block">
              <span className="text-sm text-muted-foreground">描述（可留空）</span>
              <textarea
                value={draftDesc}
                onChange={(event) => setDraftDesc(event.target.value)}
                placeholder="这个库存什么、给 AI 什么时候用"
                rows={3}
                className={cn(inputClass, "mt-1.5 h-auto resize-none py-2")}
              />
            </label>
            {!editingKb ? (
              <div className="block">
                <span className="text-sm text-muted-foreground">所属工作目录</span>
                <Select value={draftWorkspace} onValueChange={setDraftWorkspace}>
                  <SelectTrigger className="mt-1.5 w-full">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="none">不绑定工作目录</SelectItem>
                    {projects.map((project) => (
                      <SelectItem key={project.id} value={project.id}>
                        {project.name}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </div>
            ) : null}
          </div>
          <div className="mt-5 flex justify-end gap-2">
            <Button
              variant="subtle"
              size="sm"
              onClick={() => {
                setCreating(false);
                setEditingKb(false);
              }}
            >
              取消
            </Button>
            <Button variant="brand" size="sm" disabled={busy || !draftName.trim()} onClick={() => void (editingKb ? submitRename() : submitCreate())}>
              {editingKb ? "保存" : "创建"}
            </Button>
          </div>
        </DialogContent>
      </Dialog>

      {/* 文档查看 / 编辑 / 新建 */}
      <Dialog
        open={docEditing !== null}
        onOpenChange={(next) => {
          if (!next) setDocEditing(null);
        }}
      >
        <DialogContent className="w-[640px]">
          <DialogTitle>{docEditing?.docId ? "编辑文档" : "添加文档"}</DialogTitle>
          {docEditing ? (
            <>
              <div className="mt-4 space-y-3">
                <input aria-label="文档标题"
                  value={docEditing.title}
                  onChange={(event) => setDocEditing({ ...docEditing, title: event.target.value })}
                  placeholder="标题"
                  autoFocus
                  className={inputClass}
                />
                <textarea
                  value={docEditing.content}
                  onChange={(event) => setDocEditing({ ...docEditing, content: event.target.value })}
                  placeholder="正文。粘贴或书写要给 AI 检索的内容"
                  rows={14}
                  className={cn(inputClass, "h-auto resize-y py-2 font-mono text-sm leading-5")}
                />
              </div>
              <div className="mt-4 flex justify-between">
                <span className="text-xs text-muted-foreground">{formatChars(docEditing.content.length)}</span>
                <span className="flex gap-2">
                  <Button variant="subtle" size="sm" onClick={() => setDocEditing(null)}>
                    取消
                  </Button>
                  <Button
                    variant="brand"
                    size="sm"
                    disabled={busy || !docEditing.title.trim() || !docEditing.content.trim()}
                    onClick={() => void saveDoc()}
                  >
                    保存
                  </Button>
                </span>
              </div>
            </>
          ) : null}
        </DialogContent>
      </Dialog>
    </section>
  );
}
