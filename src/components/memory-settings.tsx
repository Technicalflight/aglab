import { useCallback, useEffect, useId, useRef, useState, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import { IconRefresh as RefreshCw, IconAlertTriangle as TriangleAlert } from "@tabler/icons-react";

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
import { Textarea } from "@/components/ui/textarea";
import { openPath } from "@tauri-apps/plugin-opener";
import {
  autoTagMemorySensitivity,
  memoryAdd,
  memoryConfigSet,
  memoryConflicts,
  memoryConflictResolve,
  memoryDistill,
  memoryDistillPreview,
  memoryEdit,
  memoryExport,
  memoryExportAgentsMd,
  memoryForget,
  memoryImport,
  memoryList,
  memoryRebuild,
  memoryReflect,
  memorySearch,
  memorySource,
  memoryStats,
  memoryTimeline,
  memoryWipe,
  scopeLabel,
  type AgentsExport,
  type ConflictChoice,
  type ConflictPair,
  type DistillPreview,
  type DistillSummary,
  type ExtractSummary,
  type MemoryConfig,
  type MemoryHit,
  type MemoryPatch,
  type MemorySource,
  type MemoryScope,
  type MemorySensitivity,
  type MemoryStats,
  type MemoryTimelineRow,
  type MemoryView,
} from "@/lib/memory";
import { PaginationBar, usePaged } from "@/components/pagination";
import { cn } from "@/lib/utils";
import { FormColumn } from "@/components/ui/content-column";

const inputClass =
  "h-9 w-full rounded-lg border border-input bg-background px-3 text-base text-foreground outline-none transition-colors focus-visible:border-brand/50 focus-visible:ring-2 focus-visible:ring-ring/35";

/** age 装甲文本的开头。界面上"这份是不是加密的"只看它——与后端 `envelope::is_armored` 同一判据 */
const AGE_ARMOR_PREFIX = "-----BEGIN AGE ENCRYPTED FILE-----";

/** 与后端 MemoryKind::ALL 对齐的下拉项。加 kind 时两处要一起动 */
const KIND_OPTIONS: Array<{ value: string; label: string }> = [
  { value: "preference", label: "偏好" },
  { value: "fact", label: "事实" },
  { value: "decision", label: "决定" },
  { value: "event", label: "事件" },
  { value: "rule", label: "规则" },
  { value: "profile", label: "画像" },
  { value: "relationship", label: "关系" },
];

/** 只有这六项是后端真的会照做的开关。cloudSync 单独画：它是"必须是关的"那条底线 */
type SwitchKey =
  | "enabled"
  | "autoInject"
  | "autoExtract"
  | "proactiveRecall"
  | "reflectEnabled"
  | "autoDistill";

type NumberKey =
  | "alwaysBudgetTokens"
  | "retrieveBudgetTokens"
  | "searchLimit"
  | "globalLimitChars"
  | "projectLimitChars"
  | "candidateTtlDays"
  | "dailyKeepDays"
  | "distillAfterDays"
  | "decayHalfLifeDays";

/**
 * 记忆库里一行的统一形状。memory_list 回 MemoryView、memory_search 回 MemoryHit，
 * 字段名不同但给人看的是同一批东西——先归一，模板里就不必到处判来源。
 */
interface Entry {
  id: string;
  content: string;
  scope: string;
  kind: string;
  path: string;
  updatedAt: string;
  injections: number;
  importance: number;
  status: string;
  /** 外发分级。它不落这一行的话，界面上就永远看不到自己标过什么 */
  sensitivity: MemorySensitivity;
  /** 检索结果才带相关性分，列表浏览时是 null */
  score: number | null;
}

const SWITCH_FIELDS: Array<{ key: SwitchKey; label: string; description: string }> = [
  {
    key: "enabled",
    label: "启用记忆",
    description: "关掉后不检索、不注入、不提取。已存的记录一条都不删，再打开就恢复。",
  },
  {
    key: "autoInject",
    label: "每轮按相关性注入",
    description: "关掉后只给常驻段（画像 / 人格 / 硬规则），不再按本轮提法挑记忆。",
  },
  {
    key: "autoExtract",
    label: "自动提取候选",
    description: "每轮结束后交给提取器一次，过不了置信门槛的停在下面的候选区。",
  },
  {
    key: "proactiveRecall",
    label: "主动回忆提示",
    description:
      "在右栏补一句「还想起几条相关的、这一轮没给模型」。它只是提示：发出去的内容一个字都不变，那条线归上面的注入开关管。",
  },
  {
    key: "reflectEnabled",
    label: "反思（我学到了什么）",
    description:
      "开着才允许下面那个「反思一次」发请求；关掉时一次服务商都不花钱。产物只会停在候选区等你点头，不会自己变成事实。",
  },
  {
    key: "autoDistill",
    label: "空闲自动蒸馏",
    description:
      "开着时每半小时看一眼要不要蒸馏，需要就自动跑一次，至多一天一次（会花一次服务商请求）。日志原文先归档、合并结果进长期记忆，都可逆。",
  },
];

const NUMBER_FIELDS: Array<{
  key: NumberKey;
  label: string;
  description: string;
  min: number;
  max: number;
  step: number;
}> = [
  {
    key: "alwaysBudgetTokens",
    label: "常驻段预算（tokens）",
    description: "画像、人格、硬规则每轮都占的额度，超了会被裁。",
    min: 0,
    max: 8000,
    step: 50,
  },
  {
    key: "retrieveBudgetTokens",
    label: "检索段预算（tokens）",
    description: "每轮按相关性挑进来的记忆最多占多少额度。",
    min: 0,
    max: 8000,
    step: 50,
  },
  {
    key: "searchLimit",
    label: "检索返回条数上限",
    description: "一次检索最多交给你和模型几条。",
    min: 1,
    max: 50,
    step: 1,
  },
  {
    key: "globalLimitChars",
    label: "全局 MEMORY.md 字符上限",
    description: "长期记忆的容量线，超了就该蒸馏。",
    min: 200,
    max: 100000,
    step: 100,
  },
  {
    key: "projectLimitChars",
    label: "项目 MEMORY.md 字符上限",
    description: "单个项目的记忆容量线。",
    min: 200,
    max: 100000,
    step: 100,
  },
  {
    key: "candidateTtlDays",
    label: "候选区保留天数",
    description:
      "自动提取的候选过了这么多天、且置信度与重要性都还在转正线之下，就自动归档（正文不动，可逆）。填 0 关掉这条。",
    min: 0,
    max: 365,
    step: 1,
  },
  {
    key: "dailyKeepDays",
    label: "每日日志保留天数",
    description: "超过这个天数的日志原文归档，不再进检索。",
    min: 1,
    max: 365,
    step: 1,
  },
  {
    key: "distillAfterDays",
    label: "多少天后送去蒸馏",
    description: "到线的记录交给蒸馏合并成一条长期记忆。",
    min: 1,
    max: 365,
    step: 1,
  },
  {
    key: "decayHalfLifeDays",
    label: "多少天不用，新鲜度对折",
    description: "只改排序里「新鲜」那一项，不改写任何一条记忆的正文。",
    min: 1,
    max: 3650,
    step: 1,
  },
];

const SCOPE_FILTERS: Array<{ value: MemoryScope | "all"; label: string }> = [
  { value: "all", label: "全部" },
  { value: "global", label: "全局" },
  { value: "project", label: "项目" },
  { value: "session", label: "话题" },
  { value: "temp", label: "临时" },
];

const STATUS_LABEL: Record<string, string> = {
  candidate: "候选",
  active: "在用",
  archived: "归档",
  deleted: "已删",
};

/** 三档各拦的是什么。文案要说得出一件具体的事，不然"private"这种词在界面上就是装饰 */
const SENSITIVITY_LABEL: Record<MemorySensitivity, string> = {
  public: "照常外发",
  private: "不进后台材料",
  secret: "永不外发",
};

function text_of(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}

/** 后端还没回这个字段时当不存在：拿 0 顶上去看起来就像「一条都没有」 */
function as_number(value: number | undefined | null): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

/** overBudget 是超线的文件名列表而不是布尔：说清是哪一份超了，人才敢决定要不要蒸馏 */
function as_name_list(value: string[] | undefined | null): string[] {
  return Array.isArray(value) ? value.filter((item) => typeof item === "string" && item !== "") : [];
}

function scope_text(scope: string): string {
  return scopeLabel[scope as MemoryScope] ?? scope;
}

function stamp_of(value: string): string {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value;
  return date.toLocaleString("zh-CN", {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function entry_of_view(view: MemoryView): Entry {
  return {
    id: view.record.id,
    content: view.record.content,
    scope: view.record.scope,
    kind: view.record.kind,
    path: view.path,
    updatedAt: view.record.updatedAt,
    injections: view.injections,
    importance: view.record.importance,
    status: view.record.status,
    sensitivity: view.record.sensitivity,
    score: null,
  };
}

function entry_of_hit(hit: MemoryHit): Entry {
  return {
    id: hit.id,
    content: hit.content,
    sensitivity: hit.sensitivity,
    scope: hit.scope,
    kind: hit.kind,
    path: hit.path,
    updatedAt: hit.updatedAt,
    injections: hit.injections,
    importance: hit.importance,
    status: hit.status,
    score: hit.score,
  };
}

function Group({
  title,
  description,
  note,
  error,
  footer,
  children,
}: {
  title: string;
  description?: string;
  note?: ReactNode;
  error?: string | null;
  footer?: ReactNode;
  children: ReactNode;
}) {
  return (
    <div className="mt-8 first:mt-0">
      <div className="flex items-baseline justify-between gap-3">
        <h2 className="text-lg font-semibold tracking-tight text-foreground">{title}</h2>
        {note ? <span className="shrink-0 text-xs text-muted-foreground">{note}</span> : null}
      </div>
      {description ? (
        <p className="mt-1 text-sm leading-6 text-muted-foreground">{description}</p>
      ) : null}
      <div className="mt-3 rounded-lg border border-border bg-surface px-3">{children}</div>
      {error ? <p className="mt-2 text-xs leading-5 text-destructive">{error}</p> : null}
      {footer ? (
        <p className="mt-2 text-xs leading-5 text-muted-foreground">{footer}</p>
      ) : null}
    </div>
  );
}

function Row({
  title,
  description,
  children,
}: {
  title: string;
  description?: string;
  children: ReactNode;
}) {
  return (
    <div className="flex items-center justify-between gap-6 border-b border-border px-1 py-4 last:border-b-0">
      <div className="min-w-0">
        <p className="text-base font-medium text-foreground">{title}</p>
        {description ? (
          <p className="mt-0.5 text-xs leading-5 text-muted-foreground">{description}</p>
        ) : null}
      </div>
      <div className="w-[150px] shrink-0">{children}</div>
    </div>
  );
}

/**
 * 数字项只在 blur / 回车时落盘：逐字符发请求的话，敲 3000 会打出三次配置写入，
 * 提交失败还要退回后端认的那个值——留着"3000"会让人以为改成功了。
 */
function NumberField({
  value,
  min,
  max,
  step,
  onCommit,
  label,
}: {
  value: number;
  min: number;
  max: number;
  step: number;
  onCommit: (value: number) => Promise<boolean>;
  /** 读屏名称：必填。这些控件复用多处，不写清楚用户不知道在改什么 */
  label: string;
}) {
  const [draft, setDraft] = useState(String(value));
  const [editing, setEditing] = useState(false);

  useEffect(() => {
    if (!editing) setDraft(String(value));
  }, [value, editing]);

  async function commit() {
    if (!editing) return;
    setEditing(false);
    const parsed = Math.round(Number(draft));
    if (!Number.isFinite(parsed)) {
      setDraft(String(value));
      return;
    }
    const next = Math.min(Math.max(parsed, min), max);
    if (next === value) {
      setDraft(String(value));
      return;
    }
    setDraft(String((await onCommit(next)) ? next : value));
  }

  return (
    <input
      type="number"
      aria-label={label}
      value={draft}
      min={min}
      max={max}
      step={step}
      className={inputClass}
      onFocus={() => setEditing(true)}
      onChange={(event) => setDraft(event.target.value)}
      onBlur={() => void commit()}
      onKeyDown={(event) => {
        if (event.key === "Enter") {
          event.preventDefault();
          void commit();
        }
      }}
    />
  );
}

/** 一行记忆 + 就地改正文 / 忘记。忘记要两下：删掉的就是真相源里的那一段 */
function EntryRow({
  entry,
  root,
  onChanged,
  onForgotten,
}: {
  entry: Entry;
  /** 记忆根目录的绝对路径：路径行是相对的，拼上它才能在系统文件管理器里定位 */
  root: string | null;
  onChanged: () => void;
  onForgotten: (message: string | null) => void;
}) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(entry.content);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [confirmingForget, setConfirmingForget] = useState(false);

  useEffect(() => {
    if (!confirmingForget) return;
    const timer = setTimeout(() => setConfirmingForget(false), 3000);
    return () => clearTimeout(timer);
  }, [confirmingForget]);

  async function save() {
    const content = draft.trim();
    if (!content) {
      setError("正文清空了就不叫编辑，那是一条没用的记录。要用忘记。");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await memoryEdit(entry.id, { content });
      setEditing(false);
      onChanged();
    } catch (cause) {
      setError(text_of(cause));
    } finally {
      setBusy(false);
    }
  }

  async function forget() {
    if (!confirmingForget) {
      setConfirmingForget(true);
      return;
    }
    setConfirmingForget(false);
    setBusy(true);
    setError(null);
    onForgotten(null);
    try {
      const message = await memoryForget(entry.id);
      onForgotten(message);
      onChanged();
    } catch (cause) {
      setError(text_of(cause));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="border-b border-border px-1 py-3 last:border-b-0">
      <div className="flex items-start justify-between gap-3">
        <p className="min-w-0 flex-1 whitespace-pre-wrap text-sm leading-6 text-foreground">
          {entry.content}
        </p>
        <div className="flex shrink-0 gap-1">
          {/* 这一格既是显示也是入口：分级只在 `.md` 里能改，就等于界面上没有这条规则 */}
          <Select
            value={entry.sensitivity}
            disabled={busy}
            onValueChange={(value) => {
              const level = value as MemorySensitivity;
              setBusy(true);
              setError(null);
              void memoryEdit(entry.id, { sensitivity: level })
                .then(onChanged)
                .catch((cause) => setError(text_of(cause)))
                .finally(() => setBusy(false));
            }}
          >
            <SelectTrigger
              aria-label="外发分级"
              title="这条记忆允不允许被送到模型：照常外发 / 不进后台自动外发的材料 / 永不外发"
              className="h-7 w-auto shrink-0 gap-1.5 rounded-md px-2 text-xs"
            >
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {(Object.keys(SENSITIVITY_LABEL) as MemorySensitivity[]).map((level) => (
                <SelectItem key={level} value={level} className="text-xs">
                  {SENSITIVITY_LABEL[level]}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <Button
            variant="subtle"
            size="sm"
            disabled={busy}
            onClick={() => {
              setEditing((previous) => !previous);
              setError(null);
            }}
          >
            {editing ? "收起" : "编辑"}
          </Button>
          <Button
            variant="subtle"
            size="sm"
            disabled={busy}
            className={confirmingForget ? "text-destructive" : undefined}
            onClick={() => void forget()}
          >
            {confirmingForget ? "再点一次确认" : "忘记"}
          </Button>
        </div>
      </div>

      <p className="mt-1.5 flex flex-wrap items-baseline gap-x-2 gap-y-0.5 text-xs text-muted-foreground">
        <span>{scope_text(entry.scope)}</span>
        <span className="font-mono">{entry.kind}</span>
        {entry.status !== "active" ? (
          <span>{STATUS_LABEL[entry.status] ?? entry.status}</span>
        ) : null}
        <span>重要 {entry.importance}</span>
        <span>用过 {entry.injections} 次</span>
        {entry.score !== null ? <span>评分 {entry.score.toFixed(2)}</span> : null}
        <span>{stamp_of(entry.updatedAt)}</span>
      </p>
      {/* 路径既是说明也是入口：一键在文件管理器里定位，手改 Markdown 就不必先找目录 */}
      <p className="mt-0.5 flex items-center gap-2 break-all font-mono text-xs text-muted-foreground">
        <span className="min-w-0 flex-1">{entry.path}</span>
        {root ? (
          <Button
            variant="subtle"
            size="sm"
            className="h-6 shrink-0 px-2 text-xs"
            onClick={() => {
              // 打开所在目录而不是文件本身：.md 的默认打开程序未必是编辑器
              const full = `${root.replace(/\\/g, "/")}/${entry.path}`;
              const dir = full.slice(0, full.lastIndexOf("/"));
              void openPath(dir || full).catch(() => undefined);
            }}
          >
            打开所在目录
          </Button>
        ) : null}
      </p>

      {editing ? (
        <div className="mt-2">
          <Textarea
            value={draft}
            rows={4}
            spellCheck={false}
            onChange={(event) => setDraft(event.target.value)}
          />
          <div className="mt-1.5 flex justify-end gap-2">
            <Button
              variant="subtle"
              size="sm"
              onClick={() => {
                setDraft(entry.content);
                setEditing(false);
                setError(null);
              }}
            >
              取消
            </Button>
            <Button size="sm" disabled={busy} onClick={() => void save()}>
              {busy ? "保存中…" : "保存"}
            </Button>
          </div>
        </div>
      ) : null}

      {error ? <p className="mt-1.5 text-xs leading-5 text-destructive">{error}</p> : null}
    </div>
  );
}

/** 候选行：确认（转正）、编辑后确认、丢弃。三条都改 Markdown，索引跟着走 */
function CandidateRow({
  view,
  onChanged,
  onForgotten,
}: {
  view: MemoryView;
  onChanged: () => void;
  onForgotten: (message: string | null) => void;
}) {
  const record = view.record;
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(record.content);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function accept(patch: MemoryPatch) {
    setBusy(true);
    setError(null);
    try {
      await memoryEdit(record.id, patch);
      setEditing(false);
      onChanged();
    } catch (cause) {
      setError(text_of(cause));
    } finally {
      setBusy(false);
    }
  }

  async function discard() {
    setBusy(true);
    setError(null);
    onForgotten(null);
    try {
      onForgotten(await memoryForget(record.id));
      onChanged();
    } catch (cause) {
      setError(text_of(cause));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="border-b border-border px-3 py-3 last:border-b-0">
      <p className="whitespace-pre-wrap text-sm leading-6 text-foreground">{record.content}</p>
      <p className="mt-1.5 flex flex-wrap items-baseline gap-x-2 gap-y-0.5 text-xs text-muted-foreground">
        <span>置信 {record.confidence.toFixed(2)}</span>
        <span>重要 {record.importance}</span>
        <span>{scope_text(record.scope)}</span>
        <span>{stamp_of(record.updatedAt)}</span>
      </p>
      <p className="mt-0.5 select-text break-all font-mono text-xs text-muted-foreground">
        {view.path}
      </p>

      {editing ? (
        <Textarea
          className="mt-2"
          value={draft}
          rows={4}
          spellCheck={false}
          onChange={(event) => setDraft(event.target.value)}
        />
      ) : null}

      <div className="mt-2 flex flex-wrap items-center gap-2">
        <Button size="sm" disabled={busy} onClick={() => void accept({ status: "active" })}>
          确认
        </Button>
        {editing ? (
          <Button
            size="sm"
            disabled={busy}
            onClick={() => {
              const content = draft.trim();
              if (!content) {
                setError("正文清空了就没法确认。要它消失请用丢弃。");
                return;
              }
              void accept({ content, status: "active" });
            }}
          >
            {busy ? "提交中…" : "编辑后确认"}
          </Button>
        ) : (
          <Button
            variant="subtle"
            size="sm"
            disabled={busy}
            onClick={() => {
              setDraft(record.content);
              setEditing(true);
              setError(null);
            }}
          >
            编辑后确认
          </Button>
        )}
        <Button variant="subtle" size="sm" disabled={busy} onClick={() => void discard()}>
          丢弃
        </Button>
      </div>

      {error ? <p className="mt-1.5 text-xs leading-5 text-destructive">{error}</p> : null}
    </div>
  );
}

/**
 * 设置页的「记忆」项：本地自记忆系统的总控台。
 * 每条 invoke 都在自己的小节里报错——后端有几台命令还在别的线上补，
 * 一个按钮点下去变成空白页或静默成功，都比一句实话糟得多。
 */
/** 冲突的四种裁法。四个按钮都要在：只给"留新的/留旧的"就是替用户决定"两条不能都活"，
 *  而 §8 那条关闭项明写的是"冲突双留之后必须有个地方说得清" */
const CHOICES: Array<{ value: ConflictChoice; label: string; hint: string }> = [
  { value: "newerWins", label: "留新的", hint: "旧的归档；正文仍留在 Markdown 里" },
  { value: "olderWins", label: "留旧的", hint: "新的归档" },
  { value: "keepBoth", label: "两条都留", hint: "清掉冲突标记，不再打扰" },
  { value: "archiveBoth", label: "都归档", hint: "两条都不再进检索" },
];

/** 一条记忆的来历。点一下才去问：它要读索引，不该在每张卡片渲染时敲一次数据库 */
function SourceLine({ id }: { id: string }) {
  const [source, setSource] = useState<MemorySource | null>(null);
  const [busy, setBusy] = useState(false);
  return (
    <span className="text-xs leading-5 text-muted-foreground">
      <button
        type="button"
        disabled={busy}
        className="outline-none hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45"
        onClick={() => {
          setBusy(true);
          void memorySource(id)
            .then(setSource)
            .catch(() => setSource(null))
            .finally(() => setBusy(false));
        }}
      >
        {source ? "出处" : "看出处"}
      </button>
      {source ? (
        <>
          {" · 来自话题 "}
          <span className="font-mono">{source.origin?.conversationId ?? "没记"}</span>
          {source.origin?.entries.length
            ? ` 的 ${source.origin.entries.length} 条消息`
            : "（提取时没记具体几条）"}
          {" · 被注入 "}
          {source.injections} 次
          {source.lastInjectedAt ? `（最近 ${stamp_of(source.lastInjectedAt)}）` : ""}
          {source.occurredAt ? ` · 事情发生在 ${stamp_of(source.occurredAt)}` : ""}
        </>
      ) : null}
    </span>
  );
}

function ConflictCard({ pair, onJudged }: { pair: ConflictPair; onJudged: () => void }) {
  const [busy, setBusy] = useState<ConflictChoice | null>(null);
  const [error, setError] = useState<string | null>(null);
  const recommended = CHOICES.find((choice) => choice.value === pair.recommendation)?.label ?? "";
  return (
    <li className="border-t border-border px-1 py-3 first:border-t-0">
      <p className="text-xs leading-5 text-muted-foreground">{pair.why}</p>
      <ul className="mt-2 space-y-2">
        {[
          { view: pair.a, role: "站着的那条" },
          { view: pair.b, role: "后来主张的那条" },
        ].map(({ view, role }) => (
          <li key={view.record.id}>
            <p className="text-xs text-muted-foreground">
              {role} · 更新 {stamp_of(view.record.updatedAt)} · 注入 {view.injections} 次
            </p>
            <p className="whitespace-pre-wrap text-sm leading-6 text-foreground">
              {view.record.content}
            </p>
            <SourceLine id={view.record.id} />
          </li>
        ))}
      </ul>
      <div className="mt-2 flex flex-wrap items-center gap-2">
        {CHOICES.map((choice) => (
          <Button
            key={choice.value}
            size="sm"
            variant={choice.value === pair.recommendation ? "brand" : "ghost"}
            disabled={busy !== null}
            title={choice.hint}
            onClick={() => {
              setBusy(choice.value);
              setError(null);
              void memoryConflictResolve(pair.a.record.id, pair.b.record.id, choice.value)
                .then(onJudged)
                .catch((cause) => setError(text_of(cause)))
                .finally(() => setBusy(null));
            }}
          >
            {busy === choice.value ? "在记…" : choice.label}
          </Button>
        ))}
        <span className="text-xs text-muted-foreground">它建议：{recommended}</span>
      </div>
      {error ? <p className="mt-1 text-xs text-destructive">{error}</p> : null}
    </li>
  );
}

export function MemorySettings() {
  const [config, setConfig] = useState<MemoryConfig | null>(null);
  const [configError, setConfigError] = useState<string | null>(null);
  // 冲突不是"记录列表的一个筛选结果"：它是后端按相似度算出来的一对，
  // 裁一次就落盘，所以读它要单独读、裁完要单独重读（不能拿本地数组乐观删一行）
  const [conflicts, setConflicts] = useState<ConflictPair[] | null>(null);
  const [conflictError, setConflictError] = useState<string | null>(null);
  const refreshConflicts = useCallback(async () => {
    try {
      setConflicts(await memoryConflicts());
      setConflictError(null);
    } catch (cause) {
      setConflictError(text_of(cause));
    }
  }, []);
  useEffect(() => {
    void refreshConflicts();
  }, [refreshConflicts]);
  const [stats, setStats] = useState<MemoryStats | null>(null);
  const [statsError, setStatsError] = useState<string | null>(null);

  const [views, setViews] = useState<MemoryView[] | null>(null);
  const [hits, setHits] = useState<MemoryHit[] | null>(null);
  const [query, setQuery] = useState("");
  const [scope, setScope] = useState<MemoryScope | "all">("all");
  const [searchToken, setSearchToken] = useState(0);
  /** 时间线那一块的重读旗标：它是投影，跟着记录走，不跟着检索词走 */
  const [graphToken, setGraphToken] = useState(0);
  const [listError, setListError] = useState<string | null>(null);
  const [listNote, setListNote] = useState<string | null>(null);

  const [switchError, setSwitchError] = useState<string | null>(null);
  const [budgetError, setBudgetError] = useState<string | null>(null);
  const [candidateNote, setCandidateNote] = useState<string | null>(null);

  const [adding, setAdding] = useState(false);
  const [newContent, setNewContent] = useState("");
  const [newKind, setNewKind] = useState("fact");
  const [newScope, setNewScope] = useState("global");
  const [newImportance, setNewImportance] = useState(3);
  const [batchBusy, setBatchBusy] = useState(false);
  const [addNote, setAddNote] = useState<string | null>(null);
  const [addError, setAddError] = useState<string | null>(null);

  const reloadConfig = useCallback(async () => {
    // 读配置没有包装函数（lib/memory.ts 只给了 set），而改完的回包就是整份配置，
    // 所以只在进页时直接拉一次，之后靠 set 的返回值喂
    try {
      setConfig(await invoke<MemoryConfig>("memory_config_get"));
      setConfigError(null);
    } catch (cause) {
      setConfig(null);
      setConfigError(text_of(cause));
    }
  }, []);

  const reloadStats = useCallback(async () => {
    try {
      setStats(await memoryStats());
      setStatsError(null);
    } catch (cause) {
      setStats(null);
      setStatsError(text_of(cause));
    }
  }, []);

  const reloadList = useCallback(async () => {
    try {
      setViews(await memoryList());
      setListError(null);
    } catch (cause) {
      setViews(null);
      setListError(text_of(cause));
    }
  }, []);

  // 三份数据各自 try/catch：统计读不到不该把记忆库也一起变空白
  const reload_all = useCallback(async () => {
    await Promise.all([reloadConfig(), reloadStats(), reloadList()]);
  }, [reloadConfig, reloadStats, reloadList]);

  useEffect(() => {
    void reload_all();
  }, [reload_all]);

  // 搜索单独一条请求，防抖 350ms：作用域筛选全在本地做，不再发第二遍
  useEffect(() => {
    const term = query.trim();
    if (!term) {
      setHits(null);
      return;
    }
    let active = true;
    const timer = setTimeout(() => {
      memorySearch(term)
        .then((value) => {
          if (!active) return;
          setHits(value);
          setListError(null);
        })
        .catch((cause) => {
          if (!active) return;
          setHits(null);
          setListError(text_of(cause));
        });
    }, 350);
    return () => {
      active = false;
      clearTimeout(timer);
    };
  }, [query, searchToken]);

  const after_mutation = useCallback(() => {
    void reloadStats();
    void reloadList();
    setGraphToken((token) => token + 1);
    if (query.trim()) setSearchToken((token) => token + 1);
  }, [reloadStats, reloadList, query]);

  const patch_config = useCallback(
    async (patch: Record<string, unknown>, fail: (message: string | null) => void) => {
      try {
        setConfig(await memoryConfigSet(patch));
        fail(null);
        return true;
      } catch (cause) {
        fail(text_of(cause));
        return false;
      }
    },
    [],
  );

  const searching = query.trim().length > 0;
  const entries = (searching ? (hits ?? []).map(entry_of_hit) : (views ?? []).map(entry_of_view))
    .filter((entry) => scope === "all" || entry.scope === scope);
  // "还没读到"和"读到了但空"是两件事：后者能说"没有匹配的记忆"，前者不能
  const loaded = searching ? hits !== null : views !== null;
  const candidates = (views ?? []).filter((view) => view.record.status === "candidate");
  const over_files = as_name_list(stats?.overBudget);

  async function remember() {
    const content = newContent.trim();
    if (!content) return;
    setAdding(true);
    setAddError(null);
    setAddNote(null);
    try {
      const view = await memoryAdd({ content, kind: newKind, scope: newScope, importance: newImportance });
      setNewContent("");
      // 决策层嵌入（sensitivityScan）：外发档自动分级，只降不升。列表里那一列会显示最终档位
      const tagged = await autoTagMemorySensitivity(view);
      setAddNote(
        `已记住（${view.record.id}·${scope_text(view.record.scope)}）` +
          (tagged ? `·外发档自动降为「${SENSITIVITY_LABEL[tagged]}」` : ""),
      );
      after_mutation();
    } catch (cause) {
      // 敏感内容后端会拒绝并给一句中文原因，那句就是要给人看的，不能吞
      setAddError(text_of(cause));
    } finally {
      setAdding(false);
    }
  }

  /** 候选批量确认 / 丢弃：逐条走同一条命令，中途失败就停下并报出已完成数 */
  async function batch_candidates(action: "confirm" | "discard") {
    if (!views) return;
    setBatchBusy(true);
    setCandidateNote(null);
    let done = 0;
    try {
      for (const view of candidates) {
        if (action === "confirm") {
          await memoryEdit(view.record.id, { status: "active" });
        } else {
          await memoryForget(view.record.id);
        }
        done += 1;
      }
      setCandidateNote(`已${action === "confirm" ? "确认" : "丢弃"} ${done} 条候选。`);
    } catch (cause) {
      setCandidateNote(`批量操作中断（已完成 ${done} 条）：${text_of(cause)}`);
    } finally {
      setBatchBusy(false);
      after_mutation();
    }
  }

  return (
    <FormColumn>
      <div className="flex items-start justify-between gap-4">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight text-foreground">记忆</h1>
          <p className="mt-1 text-sm leading-6 text-muted-foreground">
            1. 记忆就是本机的 Markdown 文件，直接手改也行，改完点「重建索引」对上；
            2. 对话里自动攒下的记忆在这里审：保留、删改都在这一页，全部只动本机。
          </p>
        </div>
        <Button
          variant="subtle"
          size="sm"
          className="shrink-0"
          onClick={() => {
            void reload_all();
            setGraphToken((token) => token + 1);
          }}
        >
          <RefreshCw className="size-3.5" />
          重新读取
        </Button>
      </div>

      <Group
        title="概览"
        error={statsError}
        footer={
          typeof stats?.root === "string" ? (
            <>
              记忆根目录：<span className="select-text font-mono">{stats.root}</span>
            </>
          ) : null
        }
      >
        <div className="flex flex-wrap gap-x-6 gap-y-2 px-1 py-3">
          {(
            [
              ["总记录", as_number(stats?.total)],
              ["在用", as_number(stats?.active)],
              ["候选", as_number(stats?.candidates)],
              ["归档", as_number(stats?.archived)],
              ["到期待蒸馏", as_number(stats?.expiredNow)],
            ] as Array<[string, number | null]>
          )
            .filter(([, value]) => value !== null)
            .map(([label, value]) => (
              <div key={label} className="min-w-[72px]">
                <p className="text-xs text-muted-foreground">{label}</p>
                <p className="mt-0.5 text-xl font-semibold tabular-nums text-foreground">
                  {value}
                </p>
              </div>
            ))}
          {stats === null && !statsError ? (
            <p className="text-sm text-muted-foreground">正在读取统计…</p>
          ) : null}
        </div>

        {over_files.length > 0 || stats?.needsDistill === true ? (
          <p className="border-t border-border px-1 py-3 text-sm leading-5 text-destructive">
            {over_files.length > 0
              ? `这几份记忆已经超了限额：${over_files.join("、")}。常驻段会被裁，建议现在跑一次蒸馏（在下面的「危险操作」）。`
              : "长期记忆到了该蒸馏的时候：建议现在跑一次蒸馏（在下面的「危险操作」）。"}
          </p>
        ) : null}
      </Group>

      <Group
        title="开关"
        error={switchError ?? (config === null ? configError : null)}
        footer="记忆在这台电脑上仍是明文 Markdown：加密只保护导出物那一份，所以别往记忆里记密钥。"
      >
        {config
          ? SWITCH_FIELDS.map((field) => (
              <Row key={field.key} title={field.label} description={field.description}>
                <div className="flex justify-end">
                  <CapabilityToggle
                    label={field.label}
                    enabled={config[field.key]}
                    onToggle={() =>
                      void patch_config({ [field.key]: !config[field.key] }, setSwitchError)
                    }
                  />
                </div>
              </Row>
            ))
          : null}

        <Row
          title="云同步"
          description="后端会拒绝开启，这是产品底线不是没做完：记忆里混着本机路径和顺手记下的凭据线索，它不该离开这台电脑。"
        >
          <div className="flex justify-end">
            <span
              title="后端会拒绝开启"
              className="flex h-8 items-center gap-1.5 rounded-lg border border-border px-2.5 text-xs text-muted-foreground"
            >
              <span className="size-1.5 rounded-full bg-muted-foreground/70" />
              不开放
            </span>
          </div>
        </Row>
      </Group>

      <Group
        title="预算与治理"
        description="每一项敲完回车或移开焦点就写进配置，立即对下一轮生效。评分权重还没做进界面，只在后端配置里。"
        error={budgetError ?? (config === null ? configError : null)}
      >
        {config
          ? NUMBER_FIELDS.map((field) => (
              <Row key={field.key} title={field.label} description={field.description}>
                <NumberField
                  label={field.label}
                  value={config[field.key]}
                  min={field.min}
                  max={field.max}
                  step={field.step}
                  onCommit={(value) => patch_config({ [field.key]: value }, setBudgetError)}
                />
              </Row>
            ))
          : null}
      </Group>

      <Group
        title="待确认候选"
        description="自动提取过不了置信门槛的停在这里。确认才进常驻检索池，丢弃是把它从 Markdown 里摘掉。"
        note={views ? `${candidates.length} 条` : undefined}
        error={views === null ? listError : null}
      >
        {views === null ? (
          <p className="px-1 py-3 text-xs leading-5 text-muted-foreground">
            还没读到记录，说不上有没有候选。
          </p>
        ) : candidates.length === 0 ? (
          <p className="px-1 py-3 text-xs leading-5 text-muted-foreground">
            没有待确认的候选。
          </p>
        ) : (
          <>
            <div className="flex items-center gap-2 p-3">
              <Button size="sm" disabled={batchBusy} onClick={() => void batch_candidates("confirm")}>
                {batchBusy ? "处理中…" : "全部确认"}
              </Button>
              <Button
                variant="subtle"
                size="sm"
                disabled={batchBusy}
                onClick={() => void batch_candidates("discard")}
              >
                全部丢弃
              </Button>
              <p className="min-w-0 flex-1 truncate text-xs text-muted-foreground">
                批量动作逐条落盘，中途失败会停在已完成的那条。
              </p>
            </div>
            {candidates.map((view) => (
              <CandidateRow
                key={view.record.id}
                view={view}
                onChanged={after_mutation}
                onForgotten={setCandidateNote}
              />
            ))}
          </>
        )}
        {candidateNote ? (
          <p className="border-t border-border px-3 py-3 text-xs leading-5 text-muted-foreground">
            {candidateNote}
          </p>
        ) : null}
      </Group>

      <Group
        title="互相打架的记忆"
        description="两条说法不能同时成立时停在这里。没裁的那对仍然会进检索与注入——所以这一格不是装饰，放着不管等于让模型同时相信两句话。裁一次就落盘：败者归档、胜者带上取代边，两条正文都留在 Markdown 文件里。"
        note={conflicts ? `${conflicts.length} 对` : undefined}
        error={conflictError}
      >
        {conflicts === null ? (
          <p className="px-1 py-3 text-xs leading-5 text-muted-foreground">
            还没读到冲突对，说不上有没有。
          </p>
        ) : conflicts.length === 0 ? (
          <p className="px-1 py-3 text-xs leading-5 text-muted-foreground">
            没有互相打架的记忆。
          </p>
        ) : (
          <ul>
            {conflicts.map((pair) => (
              <ConflictCard
                key={`${pair.a.record.id}|${pair.b.record.id}`}
                pair={pair}
                // 裁完以盘上那份为准重读：本地删一行就是"以为裁了、其实没落上"的旧形状
                onJudged={() => void refreshConflicts()}
              />
            ))}
          </ul>
        )}
      </Group>

      <Group
        title="记忆库"
        description="留空搜索框是浏览全部记录，填了就是按相关性检索。作用域筛选在本地做，不会再发一遍请求。"
        note={loaded ? `${entries.length} 条` : undefined}
        error={listError}
      >
        <div className="space-y-2 px-1 pt-3">
          <input
            type="text"
            value={query}
            placeholder="搜索记忆内容，留空则浏览全部"
            aria-label="搜索记忆"
            spellCheck={false}
            className={inputClass}
            onChange={(event) => setQuery(event.target.value)}
          />
          <div className="flex flex-wrap gap-1.5">
            {SCOPE_FILTERS.map((item) => {
              const active = scope === item.value;
              return (
                <button
                  key={item.value}
                  type="button"
                  aria-pressed={active}
                  onClick={() => setScope(item.value)}
                  className={cn(
                    "rounded-lg border px-2.5 py-1 text-xs transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/45",
                    active
                      ? "border-brand/45 bg-brand/10 text-brand-text"
                      : "border-border text-muted-foreground hover:text-foreground",
                  )}
                >
                  {item.label}
                </button>
              );
            })}
          </div>
        </div>

        <div className="mt-2">
          {!loaded ? (
            <p className="px-1 py-3 text-xs leading-5 text-muted-foreground">
              {searching ? "正在检索…" : "还没读到记录。"}
            </p>
          ) : entries.length === 0 ? (
            <p className="px-1 py-3 text-xs leading-5 text-muted-foreground">
              {searching
                ? "没有匹配的记忆。"
                : scope === "all"
                  ? "记忆里还是空的，用下面的输入框记一条。"
                  : "这个作用域下还没有记录。"}
            </p>
          ) : (
            entries.map((entry) => (
              <EntryRow
                key={entry.id}
                entry={entry}
                root={stats?.root ?? null}
                onChanged={after_mutation}
                onForgotten={setListNote}
              />
            ))
          )}
        </div>

        {listNote ? (
          <p className="border-t border-border px-1 py-3 text-xs leading-5 text-muted-foreground">
            {listNote}
          </p>
        ) : null}

        <div className="border-t border-border px-1 py-3">
          <div className="mb-1.5 flex flex-wrap gap-1.5">
            <Select value={newKind} disabled={adding} onValueChange={setNewKind}>
              <SelectTrigger
                aria-label="类型"
                className="h-8 w-auto shrink-0 gap-1.5 rounded-lg px-2.5 text-sm"
              >
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {KIND_OPTIONS.map((option) => (
                  <SelectItem key={option.value} value={option.value} className="text-sm">
                    {option.label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <Select value={newScope} disabled={adding} onValueChange={setNewScope}>
              <SelectTrigger
                aria-label="作用域"
                className="h-8 w-auto shrink-0 gap-1.5 rounded-lg px-2.5 text-sm"
              >
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="global" className="text-sm">
                  全局
                </SelectItem>
                <SelectItem value="project" className="text-sm">
                  当前项目
                </SelectItem>
              </SelectContent>
            </Select>
            <Select
              value={String(newImportance)}
              disabled={adding}
              onValueChange={(value) => setNewImportance(Number(value))}
            >
              <SelectTrigger
                aria-label="重要性"
                className="h-8 w-auto shrink-0 gap-1.5 rounded-lg px-2.5 text-sm"
              >
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {[1, 2, 3, 4, 5].map((level) => (
                  <SelectItem key={level} value={String(level)} className="text-sm">
                    重要 {level}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
          <Textarea
            value={newContent}
            rows={3}
            spellCheck={false}
            placeholder="记一条：一句话说清一个事实，别塞一整段聊天记录"
            onChange={(event) => setNewContent(event.target.value)}
          />
          <div className="mt-1.5 flex items-start justify-between gap-3">
            <p
              className={cn(
                "min-w-0 flex-1 text-xs leading-5",
                addError ? "text-destructive" : "text-muted-foreground",
              )}
            >
              {/* 拒绝原因整句要说全：截断了就等于把后端那句实话切掉一半 */}
              {addError ?? addNote ?? "带密码、密钥、证件号、手机号的那类内容后端会直接拒绝写入。"}
            </p>
            <Button
              size="sm"
              className="shrink-0"
              disabled={adding || !newContent.trim()}
              onClick={() => void remember()}
            >
              {adding ? "记录中…" : "记住"}
            </Button>
          </div>
        </div>
      </Group>

      <TimelineSection refresh={graphToken} />
      <TransferSection onChanged={after_mutation} />
      <AgentsExportSection />
      <DangerSection onChanged={after_mutation} />
    </FormColumn>
  );
}

/**
 * 事件时间线：对"说了什么时候发生"的那几条记录的一次投影，按业务时间倒序。
 * 它不是存储，一条都不写 —— 删了索引重建，这份名单该一个字不差地回来
 */
function TimelineSection({ refresh }: { refresh: number }) {
  const [rows, setRows] = useState<MemoryTimelineRow[] | null>(null);
  const pagedRows = usePaged(rows ?? []);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let active = true;
    memoryTimeline()
      .then((value) => {
        if (!active) return;
        setRows(value);
        setError(null);
      })
      .catch((cause) => {
        if (!active) return;
        setRows(null);
        setError(text_of(cause));
      });
    return () => {
      active = false;
    };
  }, [refresh]);

  return (
    <Group
      title="事件时间线"
      description="按「事情什么时候发生」排，不是按「这条什么时候被记下」——去年发生的事今年才记，它还在去年那一格。没说过时间的记录不进这条线。"
      note={rows ? `${rows.length} 条` : undefined}
      error={error}
    >
      {rows === null ? (
        <p className="px-1 py-3 text-xs leading-5 text-muted-foreground">
          还没读到时间线，说不上有没有事件。
        </p>
      ) : rows.length === 0 ? (
        <p className="px-1 py-3 text-xs leading-5 text-muted-foreground">
          还没有一条记忆说过它是什么时候发生的。
        </p>
      ) : (
        <>
        <ul className="divide-y divide-border">
          {pagedRows.slice.map((row) => (
            <li key={row.recordId} className="flex items-baseline gap-3 px-1 py-3">
              <span className="w-[86px] shrink-0 text-xs tabular-nums text-muted-foreground">
                {row.at}
              </span>
              <div className="min-w-0">
                <p className="whitespace-pre-wrap text-sm leading-5 text-foreground">
                  {row.content}
                </p>
                <p className="mt-0.5 text-2xs text-muted-foreground">
                  {row.kind}
                  {row.entity ? ` · ${row.entity}` : ""}
                </p>
              </div>
            </li>
          ))}
        </ul>
          <PaginationBar page={pagedRows.page} pages={pagedRows.pages} total={pagedRows.total} onPage={pagedRows.setPage} />
      </>
      )}
    </Group>
  );
}

/** 导出文件名用本地日期：跨机器搬记忆时这份文件的日期比时间戳更好认 */
function local_date_stamp(): string {
  const now = new Date();
  const pad = (value: number) => String(value).padStart(2, "0");
  return `${now.getFullYear()}-${pad(now.getMonth() + 1)}-${pad(now.getDate())}`;
}

/**
 * 导入 / 导出。这是记忆跨机器的唯一一条路，两个方向都不碰网络：
 * 导出的文本落进剪贴板或本地文件，导入的文本来自粘贴或本地文件读取。
 */
function TransferSection({ onChanged }: { onChanged: () => void }) {
  const [exported, setExported] = useState<string | null>(null);
  const [exportError, setExportError] = useState<string | null>(null);
  const [exportBusy, setExportBusy] = useState(false);
  const [exportPass, setExportPass] = useState("");
  const [clipboardError, setClipboardError] = useState<string | null>(null);

  const [pasted, setPasted] = useState("");
  const [importPass, setImportPass] = useState("");
  const [importError, setImportError] = useState<string | null>(null);
  const [importNote, setImportNote] = useState<string | null>(null);
  const [importBusy, setImportBusy] = useState(false);
  const fileRef = useRef<HTMLInputElement | null>(null);

  /** 加没加密由产物自己的头说，不由"我刚才填没填口令"说：两者不一致时信前者 */
  const armored = exported !== null && exported.trimStart().startsWith(AGE_ARMOR_PREFIX);

  async function run_export() {
    setExportBusy(true);
    setExportError(null);
    setClipboardError(null);
    try {
      setExported(await memoryExport(exportPass.trim() || undefined));
    } catch (cause) {
      setExported(null);
      setExportError(text_of(cause));
    } finally {
      setExportBusy(false);
    }
  }

  async function copy_to_clipboard() {
    if (exported === null) return;
    setClipboardError(null);
    try {
      await navigator.clipboard.writeText(exported);
    } catch (cause) {
      setClipboardError(text_of(cause));
    }
  }

  function download_as_file() {
    if (exported === null) return;
    const url = URL.createObjectURL(
      new Blob([exported], { type: armored ? "text/plain" : "application/json" }),
    );
    const anchor = document.createElement("a");
    anchor.href = url;
    anchor.download = `aglab-memory-${local_date_stamp()}.${armored ? "age" : "json"}`;
    anchor.click();
    URL.revokeObjectURL(url);
  }

  async function read_picked_file(file: File) {
    setImportError(null);
    try {
      const text = await file.text();
      setPasted(text);
      setImportNote(
        text.trimStart().startsWith(AGE_ARMOR_PREFIX)
          ? `已读入 ${file.name}：这是一份加密备份，把口令填进上面那格再点导入。`
          : `已读入 ${file.name}，看过下面这段文本再点导入。`,
      );
    } catch (cause) {
      setImportError(text_of(cause));
    }
  }

  async function run_import() {
    const text = pasted.trim();
    if (!text) return;
    setImportBusy(true);
    setImportError(null);
    setImportNote(null);
    try {
      const count = await memoryImport(text, importPass.trim() || undefined);
      setImportNote(
        `导入完成：新增 ${count} 条。同 id 已存在的会跳过——你手改过的那份不会被覆盖。`,
      );
      setPasted("");
      setImportPass("");
      onChanged();
    } catch (cause) {
      setImportError(text_of(cause));
    } finally {
      setImportBusy(false);
    }
  }

  return (
    <Group
      title="导入 / 导出"
      description="导出是一份 JSON 文本，填了口令则变成 age 加密的装甲文本；两种都能拷进剪贴板或存成文件，导入两种都认（加密那份要口令）。这一段全程不联网。"
      error={exportError && importError ? `${exportError}；${importError}` : exportError ?? importError}
    >
      <div className="px-1 py-3">
        <div className="flex items-center justify-between gap-3">
          <p className="min-w-0 text-base font-medium text-foreground">导出</p>
          <div className="flex shrink-0 gap-2">
            <Button
              variant="subtle"
              size="sm"
              disabled={exportBusy}
              onClick={() => void run_export()}
            >
              {exportBusy ? "导出中…" : "生成文本"}
            </Button>
            <Button
              variant="subtle"
              size="sm"
              disabled={exported === null}
              onClick={() => void copy_to_clipboard()}
            >
              复制到剪贴板
            </Button>
            <Button
              variant="subtle"
              size="sm"
              disabled={exported === null}
              onClick={download_as_file}
            >
              {armored ? "下载加密备份" : "下载 JSON"}
            </Button>
          </div>
        </div>

        <div className="mt-2 flex flex-wrap items-center gap-2">
          <input
            type="password"
            value={exportPass}
            aria-label="导出加密口令"
            placeholder="口令（留空 = 不加密）"
            autoComplete="new-password"
            spellCheck={false}
            className="h-8 w-56 min-w-0 rounded-lg border border-input bg-background px-2 text-sm text-foreground outline-none transition-colors focus-visible:border-brand/50"
            onChange={(event) => setExportPass(event.target.value)}
          />
          <span className="min-w-0 flex-1 text-xs leading-5 text-muted-foreground">
            填了口令才加密：产物变成 age 装甲文本，没有口令谁也读不开，且解不开就是解不开、
            不会退回明文。口令只在这一次导出里存在于内存，不写进配置也不落盘。
          </span>
        </div>

        {exported !== null ? (
          <Textarea
            className="mt-2 font-mono text-xs"
            value={exported}
            rows={5}
            readOnly
            spellCheck={false}
            onFocus={(event) => event.currentTarget.select()}
          />
        ) : null}

        {clipboardError ? (
          <p className="mt-1.5 text-xs leading-5 text-destructive">复制失败：{clipboardError}</p>
        ) : null}
      </div>

      <div className="border-t border-border px-1 py-3">
        <div className="flex items-center justify-between gap-3">
          <p className="min-w-0 text-base font-medium text-foreground">导入</p>
          <div className="flex shrink-0 items-center gap-2">
            <input aria-label="导入记忆文件"
              ref={fileRef}
              type="file"
              accept=".json,.age,.txt,application/json,text/plain"
              className="hidden"
              onChange={(event) => {
                const file = event.target.files?.[0];
                // 清空 value：连着读同一个文件时 change 才不会不触发
                event.target.value = "";
                if (file) void read_picked_file(file);
              }}
            />
            <input
              type="password"
              value={importPass}
              aria-label="导入解密口令"
              placeholder="口令（加密备份才要）"
              autoComplete="off"
              spellCheck={false}
              className="h-8 w-44 min-w-0 rounded-lg border border-input bg-background px-2 text-sm text-foreground outline-none transition-colors focus-visible:border-brand/50"
              onChange={(event) => setImportPass(event.target.value)}
            />
            <Button
              variant="subtle"
              size="sm"
              onClick={() => fileRef.current?.click()}
            >
              选文件
            </Button>
            <Button
              size="sm"
              disabled={importBusy || !pasted.trim()}
              onClick={() => void run_import()}
            >
              {importBusy ? "导入中…" : "导入这段文本"}
            </Button>
          </div>
        </div>

        <Textarea
          className="mt-2 font-mono text-xs"
          value={pasted}
          rows={5}
          spellCheck={false}
          placeholder="把导出的 JSON 或加密文本粘在这里，或者用「选文件」读进来；加密的那份要把口令填上面那格"
          onChange={(event) => setPasted(event.target.value)}
        />

        {importNote ? (
          <p className="mt-1.5 text-xs leading-5 text-muted-foreground">{importNote}</p>
        ) : null}
      </div>
    </Group>
  );
}

/** 记忆"毕业"成规则：把在用条目渲染成 AGENTS.md 的一节，审完自己粘进仓库的约定文件 */
function AgentsExportSection() {
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<AgentsExport | null>(null);
  const [copied, setCopied] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function render() {
    setBusy(true);
    setError(null);
    setCopied(false);
    try {
      setResult(await memoryExportAgentsMd());
    } catch (cause) {
      setError(text_of(cause));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Group
      title="毕业成规则"
      description="把在用的记忆渲染成 AGENTS.md 的一节。稳定规则应该住进仓库随 git 走，记忆只做回忆层——审完这段文字，粘进仓库根的 AGENTS.md 就算毕业。"
      error={error}
    >
      <div className="flex items-center justify-between gap-3 px-1 py-3">
        <p className="min-w-0 text-xs leading-5 text-muted-foreground">
          只导出 active 且公开的条目；private 与 secret 永远不出门。粘贴前请把口语句改写成正式约定。
        </p>
        <Button variant="subtle" size="sm" className="shrink-0" disabled={busy} onClick={() => void render()}>
          {busy ? "生成中…" : "生成片段"}
        </Button>
      </div>
      {result ? (
        <div className="space-y-2 px-1 pb-3">
          <Textarea readOnly rows={Math.min(10, 3 + result.count)} value={result.markdown} spellCheck={false} />
          <div className="flex items-center gap-2">
            <Button
              size="sm"
              onClick={() => {
                void navigator.clipboard.writeText(result.markdown).then(() => setCopied(true));
              }}
            >
              {copied ? "已复制" : "复制"}
            </Button>
            <p className="text-xs text-muted-foreground">共 {result.count} 条</p>
          </div>
        </div>
      ) : null}
    </Group>
  );
}

const WIPE_CONSEQUENCES = [
  "索引里的记录全部清掉，检索当场变空——正在进行的对话下一轮就没有记忆可注入了。",
  "MEMORY.md 里的正文会被摘走，按归档处理而不是原地蒸发：真相源还在，但不再是「活的」。",
  "已经发生过的那些轮次不会回滚，模型照着旧记忆说过的话留在话题记录里。",
];

/** 蒸馏 / 反思 / 重建索引 / 一键清空。这几个动作都直接动磁盘，所以错误只能落在这一节里 */
function DangerSection({ onChanged }: { onChanged: () => void }) {
  const descId = useId();
  const [distilling, setDistilling] = useState(false);
  const [distilled, setDistilled] = useState<DistillSummary | null>(null);
  const [preview, setPreview] = useState<DistillPreview | null>(null);
  const [previewOpen, setPreviewOpen] = useState(false);
  const [previewBusy, setPreviewBusy] = useState(false);
  const [reflecting, setReflecting] = useState(false);
  const [reflected, setReflected] = useState<ExtractSummary | null>(null);
  const [rebuilding, setRebuilding] = useState(false);
  const [rebuilt, setRebuilt] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);

  const [wipeOpen, setWipeOpen] = useState(false);
  const [wipeAcknowledged, setWipeAcknowledged] = useState(false);
  const [wiping, setWiping] = useState(false);
  const [wiped, setWiped] = useState<number | null>(null);

  async function distill() {
    setDistilling(true);
    setError(null);
    setDistilled(null);
    try {
      setDistilled(await memoryDistill());
      onChanged();
      setPreviewOpen(false);
    } catch (cause) {
      setError(text_of(cause));
    } finally {
      setDistilling(false);
    }
  }

  /** 先看清单再花钱：预览是纯读，把"将喂哪些日志"摊开让人确认，然后才真的蒸馏 */
  async function open_preview() {
    setPreviewBusy(true);
    setError(null);
    try {
      setPreview(await memoryDistillPreview());
      setPreviewOpen(true);
    } catch (cause) {
      setError(text_of(cause));
    } finally {
      setPreviewBusy(false);
    }
  }

  async function reflect() {
    setReflecting(true);
    setError(null);
    setReflected(null);
    try {
      setReflected(await memoryReflect());
      onChanged();
    } catch (cause) {
      setError(text_of(cause));
    } finally {
      setReflecting(false);
    }
  }

  async function rebuild() {
    setRebuilding(true);
    setError(null);
    setRebuilt(null);
    try {
      setRebuilt(await memoryRebuild());
      onChanged();
    } catch (cause) {
      setError(text_of(cause));
    } finally {
      setRebuilding(false);
    }
  }

  async function wipe() {
    setWiping(true);
    setError(null);
    try {
      setWiped(await memoryWipe());
      setDistilled(null);
      setRebuilt(null);
      setWipeOpen(false);
      setWipeAcknowledged(false);
      onChanged();
    } catch (cause) {
      setError(text_of(cause));
    } finally {
      setWiping(false);
    }
  }

  return (
    <Group
      title="危险操作"
      description="这几个动作都会改磁盘上的 Markdown。蒸馏与反思各要跑一次模型，几秒是常态，界面不会冻住。"
      error={error}
    >
      <div className="flex items-center justify-between gap-3 px-1 py-3">
        <div className="min-w-0">
          <p className="text-base font-medium text-foreground">蒸馏</p>
          <p className="mt-0.5 text-xs leading-5 text-muted-foreground">
            把过期的每日日志与超限额的长期记忆交给模型合并成几条更短的。归档不是删除。
          </p>
        </div>
        <div className="flex shrink-0 flex-col items-end gap-1">
          <Button variant="subtle" size="sm" disabled={distilling || previewBusy} onClick={() => void open_preview()}>
            {previewBusy ? "在算…" : distilling ? "蒸馏中…" : "开始蒸馏"}
          </Button>
          {distilled ? (
            <p className="text-xs leading-5 text-muted-foreground">
              新增 {distilled.distilled} 条 · 归档记录 {distilled.archivedRecords} 条 · 收走日志{" "}
              {distilled.archivedLogs} 份{distilled.overbudget ? "（还超着限额）" : ""}
            </p>
          ) : null}
        </div>
      </div>

      <Dialog open={previewOpen} onOpenChange={setPreviewOpen}>
        <DialogContent className="max-w-[520px]">
          <DialogTitle>蒸馏预览</DialogTitle>
          <div className="space-y-2 text-sm leading-6 text-foreground">
            <p className="text-muted-foreground">
              将把 {preview?.logs.length ?? 0} 份日志（共 {preview?.materialChars ?? 0} 字符）交给模型
              合并成长期记忆；完成后这些日志原文归档。归档可逆，合并结果进候选区或长期记忆。
            </p>
            {preview && preview.logs.length === 0 ? (
              <p className="text-xs text-muted-foreground">还没有到线的日志，现在跑只会把候选区判定一遍。</p>
            ) : (
              <ul className="max-h-48 overflow-auto rounded-lg border border-border p-2 font-mono text-xs">
                {(preview?.logs ?? []).map((log) => (
                  <li key={log.name} className="flex justify-between gap-3">
                    <span>{log.name}</span>
                    <span className="text-muted-foreground">{log.chars} 字符</span>
                  </li>
                ))}
              </ul>
            )}
          </div>
          <div className="flex justify-end gap-2">
            <Button variant="subtle" size="sm" disabled={distilling} onClick={() => setPreviewOpen(false)}>
              取消
            </Button>
            <Button size="sm" disabled={distilling} onClick={() => void distill()}>
              {distilling ? "蒸馏中…" : "确认蒸馏"}
            </Button>
          </div>
        </DialogContent>
      </Dialog>

      <div className="flex items-center justify-between gap-3 border-t border-border px-1 py-3">
        <div className="min-w-0">
          <p className="text-base font-medium text-foreground">反思一次</p>
          <p className="mt-0.5 text-xs leading-5 text-muted-foreground">
            问模型一次「最近的日志里有什么是长期记忆还没说过的」。产出一律停在上面的候选区等你点头，
            不会自己变成事实。开关没开时这一按什么都不发。
          </p>
        </div>
        <div className="flex shrink-0 flex-col items-end gap-1">
          <Button variant="subtle" size="sm" disabled={reflecting} onClick={() => void reflect()}>
            {reflecting ? "反思中…" : "反思一次"}
          </Button>
          {reflected ? (
            <p className="text-xs leading-5 text-muted-foreground">
              新增 {reflected.stored} 条候选 · 合并 {reflected.merged} 条 · 丢掉 {reflected.dropped} 条
            </p>
          ) : null}
        </div>
      </div>

      <div className="flex items-center justify-between gap-3 border-t border-border px-1 py-3">
        <div className="min-w-0">
          <p className="text-base font-medium text-foreground">重建索引</p>
          <p className="mt-0.5 text-xs leading-5 text-muted-foreground">
            手改过 MEMORY.md、或者索引和文件对不上时用。以 Markdown 为准重灌一遍。
          </p>
        </div>
        <div className="flex shrink-0 flex-col items-end gap-1">
          <Button variant="subtle" size="sm" disabled={rebuilding} onClick={() => void rebuild()}>
            {rebuilding ? "重建中…" : "重建索引"}
          </Button>
          {rebuilt !== null ? (
            <p className="text-xs leading-5 text-muted-foreground">{rebuilt} 条记录已入库</p>
          ) : null}
        </div>
      </div>

      <div className="flex items-center justify-between gap-3 border-t border-border px-1 py-3">
        <div className="min-w-0">
          <p className="text-base font-medium text-destructive">一键清空</p>
          <p className="mt-0.5 text-xs leading-5 text-muted-foreground">
            清掉所有记忆记录。要勾一次确认再点一次按钮，且不提供撤销。
          </p>
        </div>
        <div className="flex shrink-0 flex-col items-end gap-1">
          <Button
            variant="subtle"
            size="sm"
            className="text-destructive"
            onClick={() => {
              setError(null);
              setWipeOpen(true);
            }}
          >
            清空全部记忆
          </Button>
          {wiped !== null ? (
            <p className="text-xs leading-5 text-muted-foreground">{wiped} 条记录已摘走</p>
          ) : null}
        </div>
      </div>

      <Dialog
        open={wipeOpen}
        onOpenChange={(open) => {
          if (!open) {
            setWipeAcknowledged(false);
            setWipeOpen(false);
          }
        }}
      >
        <DialogContent aria-describedby={descId} className="w-[460px]">
          <DialogTitle className="flex items-center gap-2 text-destructive">
            <TriangleAlert className="size-4" />
            清空全部记忆？
          </DialogTitle>
          <p id={descId} className="mt-1 text-xs leading-5 text-muted-foreground">
            下面这些是点下去之后真的会发生的事，不是套话。
          </p>

          <ul className="mt-3.5 space-y-1">
            {WIPE_CONSEQUENCES.map((item) => (
              <li key={item} className="flex gap-2 text-sm leading-5 text-foreground/90">
                <span className="mt-1.5 size-1 shrink-0 rounded-full bg-muted-foreground/60" />
                {item}
              </li>
            ))}
          </ul>

          <label className="mt-4 flex cursor-pointer items-start gap-2 text-sm leading-5 text-foreground">
            <input
              type="checkbox"
              checked={wipeAcknowledged}
              onChange={(event) => setWipeAcknowledged(event.target.checked)}
              className="mt-0.5 size-3.5 shrink-0 accent-brand"
            />
            我确认要清掉这台电脑上 aglab 的全部记忆
          </label>

          {error ? (
            <p className="mt-3 rounded-lg border border-destructive/30 bg-destructive/10 px-3 py-2.5 text-sm leading-5 text-destructive">
              {error}
            </p>
          ) : null}

          <div className="mt-5 flex justify-end gap-2">
            <Button
              variant="subtle"
              onClick={() => {
                setWipeAcknowledged(false);
                setWipeOpen(false);
              }}
            >
              取消
            </Button>
            <Button
              disabled={!wipeAcknowledged || wiping}
              className="bg-destructive text-destructive-foreground hover:bg-destructive/90"
              onClick={() => void wipe()}
            >
              {wiping ? "清空中…" : "清空全部记忆"}
            </Button>
          </div>
        </DialogContent>
      </Dialog>
    </Group>
  );
}
