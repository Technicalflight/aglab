import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";

import {
  IconRobot as Bot,
  IconPackage as Package,
  IconPencil as Pencil,
  IconPlus as Plus,
  IconTrash as Trash2,
} from "@tabler/icons-react";

import {
  builtinSubagentsList,
  builtinToolsList,
  type PoolCatalogEntry,
} from "@/lib/chat-transport";
import { catalogFingerprint, loadCatalog, readCatalogCache } from "@/lib/model-catalog";
import {
  getBuiltinRosterCache,
  getCatalogMemo,
  getRosterFetchedOverridesKey,
  setBuiltinRosterCache,
  setCatalogMemo,
} from "@/lib/subagent-cache";
import {
  INHERIT_MODEL,
  modelKeyLabel,
  modelKeyOf,
  modelOptions,
  parseModelKey,
  sanitizeOverrides,
  validateSubagent,
} from "@/lib/subagents";
import { PaginationBar, usePaged } from "@/components/pagination";
import { ModelIcon } from "@/components/model-icon";
import { cn } from "@/lib/utils";

import { Button } from "@/components/ui/button";
import { CapabilityToggle } from "@/components/ui/capability-toggle";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import type { BuiltinTool, BuiltinSubagentView, SubagentDef, SubagentOverride } from "@/types/chat";
import { useChatStore } from "@/store/chat-store";
import { FormColumn } from "@/components/ui/content-column";

const inputClass =
  "h-9 w-full rounded-lg border border-input bg-background px-3 text-base text-foreground outline-none transition-colors focus-visible:border-brand/50 focus-visible:ring-2 focus-visible:ring-ring/35";

const textareaClass =
  "w-full rounded-lg border border-input bg-background px-3 py-2 text-base leading-6 text-foreground outline-none transition-colors focus-visible:border-brand/50 focus-visible:ring-2 focus-visible:ring-ring/35";

function Field({
  label,
  children,
  hint,
}: {
  label: string;
  children: ReactNode;
  hint?: ReactNode;
}) {
  return (
    // 不能用 <label> 包字段：里面含 Select（如「模型」），Chromium 会把
    // 悬停/点击转发给第一个表单控件——点说明文字会把下拉弹开
    <div className="block">
      <span className="mb-1.5 block text-xs text-muted-foreground">{label}</span>
      {children}
      {hint ? (
        <span className="mt-1.5 block text-xs leading-5 text-muted-foreground">{hint}</span>
      ) : null}
    </div>
  );
}

function newDraft(): SubagentDef {
  return {
    name: "",
    description: "",
    systemPrompt: "",
    tools: [],
    endpointProfileId: "",
    model: "",
    orchestrationAssignable: true,
    chatSpawnable: false,
  };
}

/** 弹窗的打开状态：编辑某一份（按名字定位，名字在弹窗里可以改），或新建一份 */
type DialogTarget = { mode: "edit"; originalName: string } | { mode: "create" } | null;

/**
 * 设置页的「子助理」：出厂内置名册 + 用户自定义目录。
 * 两条消费通道共用——决策层派工（编排补做节点问该派谁，只认自定义）与
 * 主模型按需调用（聊天里的 spawn_subagent，内置 + 自定义都在名单上）。
 * 权限不在这里选：由工具白名单推导，并进全局权限表时只收紧不放松。
 */
export function SubagentSettings() {
  return (
    <FormColumn>
      <h1 className="text-2xl font-semibold tracking-tight text-foreground">子助理</h1>
      <p className="mt-1 text-sm leading-6 text-muted-foreground">
        1. 用出厂名册，或「新建」一个子助理：写清楚它是干什么的、给哪些工具； 2.
        勾「编排可派」「主模型可调」，决定编排与聊天分别能不能派它； 3.
        权限由工具白名单决定、只会比全局档更严；专属服务商与模型在派到那一刻生效。
      </p>
      <div className="mt-8">
        <SubagentCards />
      </div>
    </FormColumn>
  );
}

function SubagentCards() {
  const config = useChatStore((s) => s.config);
  const updateConfig = useChatStore((s) => s.updateConfig);

  const subagents = config.subagents ?? [];
  const overrides = config.subagentOverrides ?? [];
  const [dialog, setDialog] = useState<DialogTarget>(null);
  const [draft, setDraft] = useState<SubagentDef>(newDraft);
  const [problems, setProblems] = useState<string[]>([]);
  const [query, setQuery] = useState("");
  // 两步确认删除：第一下只进入待确认态，3 秒内再点一下才真删
  const [confirmDelete, setConfirmDelete] = useState<string | null>(null);

  // 出厂名册：**整个会话只拉一次**（模块级缓存）。进页面先画缓存，后台静默对账——
  // 以前每次挂载都重拉并闪"读取中"，而开关写完配置本来就要靠重拉来合并视图，
  // 现在那次重拉挪到了 writeOverride 里当场做，进页面不再承担刷新职责
  const [builtins, setBuiltins] = useState<BuiltinSubagentView[] | null>(getBuiltinRosterCache());
  // 覆盖的指纹：名册视图 = 出厂定义 × 覆盖，覆盖没变名册就不会变。
  // 进页面时缓存还在且指纹相同 → 一次 IPC 都不发；开关写完覆盖 → 指纹变 → 这里自动重拉
  const overridesKey = useMemo(() => JSON.stringify(overrides), [overrides]);
  useEffect(() => {
    if (getBuiltinRosterCache() && getRosterFetchedOverridesKey() === overridesKey) return;
    let alive = true;
    builtinSubagentsList()
      .then((list) => {
        setBuiltinRosterCache(list, overridesKey);
        if (alive) setBuiltins(list);
      })
      .catch(() => {
        // 首拉就失败且手里没有缓存：按空表画（与旧行为一致）；有缓存就守着缓存
        if (alive && getBuiltinRosterCache() === null) setBuiltins([]);
      });
    return () => {
      alive = false;
    };
  }, [overridesKey]);

  // 模型目录：指纹一致时连 localStorage 都不再摸（模块级 memo 直接给数组），
  // 进页面零请求零解析；指纹变了才真正轮服务商。以前每开一次下拉就把
  // 所有服务商轮一遍，目录又大又慢——现在进页面就备好且只备一次
  const endpointFingerprint = useMemo(() => catalogFingerprint(config), [config]);
  const [catalog, setCatalog] = useState<PoolCatalogEntry[] | null>(() => {
    const memo = getCatalogMemo();
    return memo && memo.fingerprint === endpointFingerprint ? memo.entries : null;
  });
  const [catalogError, setCatalogError] = useState<string | null>(null);
  const fetchSeq = useRef(0);
  useEffect(() => {
    const memo = getCatalogMemo();
    if (memo && memo.fingerprint === endpointFingerprint) {
      if (catalog !== memo.entries) setCatalog(memo.entries);
      setCatalogError(null);
      return;
    }
    let alive = true;
    const seq = ++fetchSeq.current;
    loadCatalog(config)
      .then((entries) => {
        if (!alive || seq !== fetchSeq.current) return;
        setCatalogMemo(endpointFingerprint, entries);
        setCatalog(entries);
        setCatalogError(null);
      })
      .catch((cause: unknown) => {
        if (!alive || seq !== fetchSeq.current) return;
        // 整体拉取失败（provider 没配好之类）时退回上一份缓存，下拉不至于空白
        const cached = readCatalogCache();
        if (cached) setCatalog(cached.entries);
        setCatalogError(cause instanceof Error ? cause.message : String(cause));
      });
    return () => {
      alive = false;
    };
    // config 进依赖会让无关配置更新触发重拉；服务商指纹才是目录的输入
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [endpointFingerprint]);

  useEffect(() => {
    if (!confirmDelete) return;
    const timer = setTimeout(() => setConfirmDelete(null), 3000);
    return () => clearTimeout(timer);
  }, [confirmDelete]);

  function openDialog(target: Exclude<DialogTarget, null>) {
    setProblems([]);
    if (target.mode === "edit") {
      const held = subagents.find((item) => item.name === target.originalName);
      if (!held) return;
      setDraft({ ...held, tools: [...held.tools] });
    } else {
      setDraft(newDraft());
    }
    setDialog(target);
  }

  function saveDraft() {
    if (!dialog) return;
    const trimmed = { ...draft, name: draft.name.trim() };
    const error = validateSubagent(trimmed, subagents, config.profiles);
    if (error.length > 0) {
      setProblems(error);
      return;
    }
    const next =
      dialog.mode === "edit"
        ? subagents.map((item) => (item.name === dialog.originalName ? trimmed : item))
        : [...subagents, trimmed];
    updateConfig({ subagents: next });
    setDialog(null);
  }

  function removeDef(name: string) {
    updateConfig({ subagents: subagents.filter((item) => item.name !== name) });
    setConfirmDelete(null);
  }

  /** 写一条内置覆盖：patch 为 null = 这条覆盖整个撤掉（回到"继承默认且启用"）。
   *  落盘前清洗一道：没命中出厂名册的条目不写（口径与后端 merged_catalog 一致） */
  function writeOverride(name: string, patch: Partial<SubagentOverride> | null) {
    const next = patch
      ? overrides.some((item) => item.name === name)
        ? overrides.map((item) => (item.name === name ? { ...item, ...patch } : item))
        : [...overrides, { name, endpointProfileId: "", model: "", disabled: false, ...patch }]
      : overrides.filter((item) => item.name !== name);
    updateConfig({ subagentOverrides: sanitizeOverrides(next) });
    // 卡片视图的重拉不用在这里发：覆盖一变 overridesKey 就变，上面那条 effect
    // 会自己重拉合并——开关状态当场对上
  }

  /** 内置卡片的模型下拉：选回"继承默认"且没停用 = 覆盖条目整个撤掉，配置里不留空壳 */
  function builtinModelKey(name: string, key: string) {
    const picked = parseModelKey(key);
    const held = overrides.find((item) => item.name === name);
    const backToInherit = !picked.endpointProfileId && !picked.model;
    if (backToInherit && !held?.disabled) {
      writeOverride(name, null);
    } else {
      writeOverride(name, picked);
    }
  }

  /** 内置卡片的停用开关：回到启用且没有任何连接覆盖时，条目同样整个撤掉 */
  function builtinToggleDisabled(view: BuiltinSubagentView) {
    const held = overrides.find((item) => item.name === view.name);
    const nextDisabled = !view.disabled;
    if (!nextDisabled && !held?.endpointProfileId && !held?.model) {
      writeOverride(view.name, null);
    } else {
      writeOverride(view.name, { disabled: nextDisabled });
    }
  }

  const filtered = query.trim()
    ? subagents.filter((item) =>
        `${item.name} ${item.description}`.toLowerCase().includes(query.trim().toLowerCase()),
      )
    : subagents;
  const pagedCustom = usePaged(filtered);
  const filteredBuiltins = query.trim()
    ? (builtins ?? []).filter((item) =>
        `${item.name} ${item.description}`.toLowerCase().includes(query.trim().toLowerCase()),
      )
    : builtins;

  return (
    <div>
      <div className="flex items-center gap-3">
        <span className="shrink-0 text-sm text-muted-foreground">已安装 {subagents.length} 项</span>
        <input
          type="text"
          value={query}
          placeholder="搜索子助理…"
          spellCheck={false}
          className={cn(inputClass, "ml-auto max-w-[280px]")}
          onChange={(event) => setQuery(event.target.value)}
        />
        <Button size="sm" onClick={() => openDialog({ mode: "create" })}>
          <Plus className="size-3.5" />
          新建
        </Button>
      </div>

      {subagents.length === 0 ? (
        <div className="mt-4 rounded-xl border border-dashed border-border px-6 py-12 text-center">
          <p className="text-base text-foreground">没有自定义子助理</p>
          <p className="mt-1.5 text-sm leading-5 text-muted-foreground">
            出厂名册在下方，开箱即可被聊天派单；要建自己的，填写名称、描述、工具和系统提示词，保存后这里就会多一张卡片。
          </p>
          <Button
            size="sm"
            variant="subtle"
            className="mt-4"
            onClick={() => openDialog({ mode: "create" })}
          >
            <Plus className="size-3.5" />
            新建
          </Button>
        </div>
      ) : filtered.length === 0 ? (
        <p className="mt-4 rounded-xl border border-dashed border-border px-6 py-10 text-center text-sm text-muted-foreground">
          没有匹配「{query.trim()}」的子助理。
        </p>
      ) : (
        <>
          <div className="mt-4 space-y-2.5">
            {pagedCustom.slice.map((item) => (
              <SubagentCard
                key={item.name}
                item={item}
                profiles={config.profiles}
                catalog={catalog}
                catalogError={catalogError}
                confirmingDelete={confirmDelete === item.name}
                onEdit={() => openDialog({ mode: "edit", originalName: item.name })}
                onDelete={() => removeDef(item.name)}
                onConfirmDelete={() => setConfirmDelete(item.name)}
                onModelKey={(key) => {
                  const picked = parseModelKey(key);
                  updateConfig({
                    subagents: subagents.map((held) =>
                      held.name === item.name ? { ...held, ...picked } : held,
                    ),
                  });
                }}
              />
            ))}
          </div>
          <PaginationBar
            page={pagedCustom.page}
            pages={pagedCustom.pages}
            total={pagedCustom.total}
            onPage={pagedCustom.setPage}
          />
        </>
      )}

      <BuiltinSection
        views={filteredBuiltins}
        loading={builtins === null}
        profiles={config.profiles}
        catalog={catalog}
        catalogError={catalogError}
        onModelKey={builtinModelKey}
        onToggleDisabled={builtinToggleDisabled}
      />

      <SubagentDialog
        open={dialog !== null}
        mode={dialog?.mode ?? "create"}
        draft={draft}
        problems={problems}
        profiles={config.profiles}
        catalog={catalog}
        catalogError={catalogError}
        onDraft={(patch) => setDraft((current) => ({ ...current, ...patch }))}
        onClose={() => setDialog(null)}
        onSave={saveDraft}
      />
    </div>
  );
}

function SubagentCard({
  item,
  profiles,
  catalog,
  catalogError,
  confirmingDelete,
  onEdit,
  onDelete,
  onConfirmDelete,
  onModelKey,
}: {
  item: SubagentDef;
  profiles: { id: string; name: string }[];
  catalog: PoolCatalogEntry[] | null;
  catalogError: string | null;
  confirmingDelete: boolean;
  onEdit: () => void;
  onDelete: () => void;
  onConfirmDelete: () => void;
  onModelKey: (key: string) => void;
}) {
  // 下拉条目懒加载：展开那一刻才挂目录（关闭时 Radix 也会 portal 渲染内容，见 modelKeyLabel）
  const [open, setOpen] = useState(false);

  return (
    <div className="flex items-center gap-3 rounded-xl border border-border bg-elevated px-4 py-3">
      <div className="flex size-9 shrink-0 items-center justify-center rounded-lg bg-brand/10 text-brand-text">
        <Bot className="size-4.5" />
      </div>
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          <span className="truncate text-base font-medium text-foreground">{item.name}</span>
          <span className="shrink-0 rounded-full bg-muted px-2 py-0.5 text-2xs text-muted-foreground">
            {item.tools.length} 个工具
          </span>
          {item.orchestrationAssignable ? (
            <span className="shrink-0 rounded-full bg-brand/10 px-2 py-0.5 text-2xs text-brand-text">
              编排可派
            </span>
          ) : null}
          {item.chatSpawnable ? (
            <span className="shrink-0 rounded-full bg-brand/10 px-2 py-0.5 text-2xs text-brand-text">
              主模型可调
            </span>
          ) : null}
        </div>
        <p className="mt-0.5 line-clamp-2 text-xs leading-5 text-muted-foreground">
          {item.description || item.systemPrompt || "（没有描述）"}
        </p>
      </div>
      <div className="flex shrink-0 items-center gap-2">
        <Select
          open={open}
          onOpenChange={setOpen}
          value={modelKeyOf(item)}
          onValueChange={onModelKey}
        >
          <SelectTrigger className="w-[190px] text-sm">
            <SelectValue>{modelKeyLabel(item, profiles)}</SelectValue>
          </SelectTrigger>
          <SelectContent>
            <SelectItem value={INHERIT_MODEL}>继承默认</SelectItem>
            {open ? <CatalogItems catalog={catalog} error={catalogError} /> : null}
          </SelectContent>
        </Select>
        <Button variant="subtle" size="icon" aria-label={`编辑 ${item.name}`} onClick={onEdit}>
          <Pencil className="size-3.5" />
        </Button>
        {confirmingDelete ? (
          <Button
            variant="subtle"
            size="icon"
            aria-label={`再点一次确认删除 ${item.name}`}
            className="text-destructive"
            onClick={onDelete}
          >
            <Trash2 className="size-3.5" />
          </Button>
        ) : (
          <Button
            variant="subtle"
            size="icon"
            aria-label={`删除 ${item.name}`}
            onClick={onConfirmDelete}
          >
            <Trash2 className="size-3.5" />
          </Button>
        )}
      </div>
    </div>
  );
}

/** 出厂名册分组（对照参考图的「内置子助理」组）：只读卡片，没有编辑与删除——
 *  定义住后端代码，这里能动的只有模型覆盖与停用。要不同行为就自建一份不同名的 */
function BuiltinSection({
  views,
  loading,
  profiles,
  catalog,
  catalogError,
  onModelKey,
  onToggleDisabled,
}: {
  views: BuiltinSubagentView[] | null;
  loading: boolean;
  profiles: { id: string; name: string }[];
  catalog: PoolCatalogEntry[] | null;
  catalogError: string | null;
  onModelKey: (name: string, key: string) => void;
  onToggleDisabled: (view: BuiltinSubagentView) => void;
}) {
  const pagedViews = usePaged(views ?? []);

  return (
    <div className="mt-8">
      <div className="flex items-baseline gap-2">
        <h2 className="text-base font-medium text-foreground">内置子助理</h2>
        <span className="text-xs text-muted-foreground">
          {loading ? "读取中…" : `${views?.length ?? 0} 项`}
        </span>
      </div>
      <div className="mt-3 space-y-2.5">
        {pagedViews.slice.map((view) => (
          <BuiltinCard
            key={view.name}
            view={view}
            profiles={profiles}
            catalog={catalog}
            catalogError={catalogError}
            onModelKey={onModelKey}
            onToggleDisabled={onToggleDisabled}
          />
        ))}
        {!loading && (views ?? []).length === 0 ? (
          <p className="rounded-xl border border-dashed border-border px-6 py-8 text-center text-sm text-muted-foreground">
            出厂名册是空的（或没读到）。
          </p>
        ) : null}
      </div>
      <PaginationBar
        page={pagedViews.page}
        pages={pagedViews.pages}
        total={pagedViews.total}
        onPage={pagedViews.setPage}
      />
    </div>
  );
}

function BuiltinCard({
  view,
  profiles,
  catalog,
  catalogError,
  onModelKey,
  onToggleDisabled,
}: {
  view: BuiltinSubagentView;
  profiles: { id: string; name: string }[];
  catalog: PoolCatalogEntry[] | null;
  catalogError: string | null;
  onModelKey: (name: string, key: string) => void;
  onToggleDisabled: (view: BuiltinSubagentView) => void;
}) {
  // 下拉条目懒加载：展开那一刻才挂目录（关闭时 Radix 也会 portal 渲染内容，见 modelKeyLabel）
  const [open, setOpen] = useState(false);

  return (
    <div
      className={cn(
        "flex items-center gap-3 rounded-xl border border-border bg-elevated px-4 py-3 transition-opacity",
        view.disabled && "opacity-55",
      )}
    >
      <div className="flex size-9 shrink-0 items-center justify-center rounded-lg bg-muted text-muted-foreground">
        <Package className="size-4.5" />
      </div>
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          <span className="truncate text-base font-medium text-foreground">{view.name}</span>
          <span className="shrink-0 rounded-full bg-muted px-2 py-0.5 text-2xs text-muted-foreground">
            {view.tools.length} 个工具
          </span>
          {view.disabled ? (
            <span className="shrink-0 rounded-full bg-muted px-2 py-0.5 text-2xs text-muted-foreground">
              已停用
            </span>
          ) : (
            <span className="shrink-0 rounded-full bg-brand/10 px-2 py-0.5 text-2xs text-brand-text">
              主模型可调
            </span>
          )}
        </div>
        <p className="mt-0.5 line-clamp-2 text-xs leading-5 text-muted-foreground">
          {view.description}
        </p>
      </div>
      <div className="flex shrink-0 items-center gap-2">
        <Select
          open={open}
          onOpenChange={setOpen}
          value={modelKeyOf(view)}
          onValueChange={(key) => onModelKey(view.name, key)}
        >
          <SelectTrigger className="w-[190px] text-sm" disabled={view.disabled}>
            <SelectValue>{modelKeyLabel(view, profiles)}</SelectValue>
          </SelectTrigger>
          <SelectContent>
            <SelectItem value={INHERIT_MODEL}>继承默认</SelectItem>
            {open ? <CatalogItems catalog={catalog} error={catalogError} /> : null}
          </SelectContent>
        </Select>
        <Button
          variant="subtle"
          size="sm"
          className="w-[52px] px-0 text-sm"
          aria-label={view.disabled ? `启用 ${view.name}` : `停用 ${view.name}`}
          onClick={() => onToggleDisabled(view)}
        >
          {view.disabled ? "启用" : "停用"}
        </Button>
      </div>
    </div>
  );
}

/** 目录的纯渲染：页面级已取好的那份直接画出来，自己不发任何请求。
 *  目录为 null = 还在拉（或缓存还没有），显示一句状态而不是空白 */
function CatalogItems({
  catalog,
  error,
}: {
  catalog: PoolCatalogEntry[] | null;
  error: string | null;
}) {
  if (!catalog) {
    return (
      <p className="px-2.5 py-2 text-xs text-muted-foreground">
        {error ? `模型目录没拉到：${error}` : "正在拉取模型目录…"}
      </p>
    );
  }
  const options = modelOptions(catalog);
  if (options.length === 0) {
    return <p className="px-2.5 py-2 text-xs text-muted-foreground">各服务商都没有拉到模型。</p>;
  }
  return (
    <>
      {options.map((option) => (
        <SelectItem key={option.key} value={option.key} className="text-sm">
          <span className="flex items-center gap-2">
            <ModelIcon model={option.model} size={13} />
            {option.label}
          </span>
        </SelectItem>
      ))}
    </>
  );
}

function SubagentDialog({
  open,
  mode,
  draft,
  problems,
  profiles,
  catalog,
  catalogError,
  onDraft,
  onClose,
  onSave,
}: {
  open: boolean;
  mode: "edit" | "create";
  draft: SubagentDef;
  problems: string[];
  profiles: { id: string; name: string }[];
  catalog: PoolCatalogEntry[] | null;
  catalogError: string | null;
  onDraft: (patch: Partial<SubagentDef>) => void;
  onClose: () => void;
  onSave: () => void;
}) {
  // 工具清单进弹窗才拉一次：REGISTRY 是常量，缓存进组件状态就够
  const [tools, setTools] = useState<BuiltinTool[] | null>(null);
  // 模型下拉的条目也懒挂：弹窗开着不等于下拉开着
  const [modelOpen, setModelOpen] = useState(false);
  useEffect(() => {
    if (!open || tools) return;
    let alive = true;
    builtinToolsList()
      .then((list) => {
        if (alive) setTools(list);
      })
      .catch(() => {
        if (alive) setTools([]);
      });
    return () => {
      alive = false;
    };
  }, [open, tools]);

  function toggleTool(id: string) {
    onDraft({
      tools: draft.tools.includes(id)
        ? draft.tools.filter((held) => held !== id)
        : [...draft.tools, id],
    });
  }

  return (
    <Dialog open={open} onOpenChange={(next) => (next ? undefined : onClose())}>
      <DialogContent className="max-h-[85vh] max-w-[560px] overflow-y-auto">
        <DialogTitle>{mode === "edit" ? `编辑「${draft.name}」` : "新建子助理"}</DialogTitle>
        <div className="space-y-4">
          <Field label="名称">
            <input
              type="text"
              value={draft.name}
              placeholder="如：审查员"
              spellCheck={false}
              className={inputClass}
              onChange={(event) => onDraft({ name: event.target.value })}
            />
          </Field>
          <Field
            label="描述（什么时候该派它）"
            hint="决策层派工的判据、spawn 工具 schema 里的说明，读的都是这一句。"
          >
            <textarea
              value={draft.description}
              rows={2}
              placeholder="如：对照要求复核已有结论，只指出对不上号的地方。"
              className={textareaClass}
              onChange={(event) => onDraft({ description: event.target.value })}
            />
          </Field>
          <Field label="系统提示" hint="写进那次 run 的第一句：你是谁、这一支只负责什么。">
            <textarea
              value={draft.systemPrompt}
              rows={4}
              placeholder="如：你是只读复核员。对照要求逐条检查，把对不上的地方指出来。"
              className={textareaClass}
              onChange={(event) => onDraft({ systemPrompt: event.target.value })}
            />
          </Field>
          <Field
            label={`工具白名单（已选 ${draft.tools.length} 个）`}
            hint="空 = 纯推理，一个工具都不给。工具页里关掉的工具，子助理同样要不到。"
          >
            <div className="flex flex-wrap gap-1.5">
              {(tools ?? []).map((tool) => {
                const picked = draft.tools.includes(tool.id);
                return (
                  <button
                    key={tool.id}
                    type="button"
                    title={`${tool.title}：${tool.blurb}`}
                    onClick={() => toggleTool(tool.id)}
                    className={cn(
                      "rounded-full border px-2.5 py-1 text-xs transition-colors",
                      picked
                        ? "border-brand/40 bg-brand/10 text-brand-text"
                        : "border-border text-muted-foreground hover:bg-accent",
                    )}
                  >
                    {tool.title}
                  </button>
                );
              })}
              {tools !== null && tools.length === 0 ? (
                <span className="text-xs text-muted-foreground">工具清单没拉到。</span>
              ) : null}
            </div>
          </Field>
          <Field label="模型" hint="继承默认 = 跟当前连接走；指定后这一发整份连接域照那张档案走。">
            <Select
              open={modelOpen}
              onOpenChange={setModelOpen}
              value={modelKeyOf(draft)}
              onValueChange={(key) => onDraft(parseModelKey(key))}
            >
              <SelectTrigger className="w-full text-base">
                <SelectValue>{modelKeyLabel(draft, profiles)}</SelectValue>
              </SelectTrigger>
              <SelectContent>
                <SelectItem value={INHERIT_MODEL}>继承默认</SelectItem>
                {modelOpen ? <CatalogItems catalog={catalog} error={catalogError} /> : null}
              </SelectContent>
            </Select>
          </Field>
          <div className="space-y-2.5 rounded-lg border border-border px-3.5 py-3">
            <CapabilityRow
              label="编排可派"
              hint="进决策层派工的花名册：编排补做节点时可以被派成它。"
              enabled={draft.orchestrationAssignable}
              onToggle={() => onDraft({ orchestrationAssignable: !draft.orchestrationAssignable })}
            />
            <CapabilityRow
              label="主模型可调"
              hint="进聊天 spawn_subagent 的名单：主模型可以在对话里按需派它。"
              enabled={draft.chatSpawnable}
              onToggle={() => onDraft({ chatSpawnable: !draft.chatSpawnable })}
            />
          </div>
          {problems.length > 0 ? (
            <ul className="space-y-1 rounded-lg border border-destructive/40 bg-destructive/5 px-3.5 py-2.5 text-sm leading-5 text-destructive">
              {problems.map((problem) => (
                <li key={problem}>{problem}</li>
              ))}
            </ul>
          ) : null}
          <div className="flex justify-end gap-2">
            <Button variant="subtle" size="sm" onClick={onClose}>
              取消
            </Button>
            <Button size="sm" onClick={onSave}>
              保存
            </Button>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}

function CapabilityRow({
  label,
  hint,
  enabled,
  onToggle,
}: {
  label: string;
  hint: string;
  enabled: boolean;
  onToggle: () => void;
}) {
  return (
    <div className="flex items-center justify-between gap-3">
      <div className="min-w-0">
        <p className="text-sm text-foreground">{label}</p>
        <p className="text-xs leading-4 text-muted-foreground">{hint}</p>
      </div>
      <CapabilityToggle label={label} enabled={enabled} onToggle={onToggle} />
    </div>
  );
}
