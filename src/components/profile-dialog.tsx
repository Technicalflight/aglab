import { useEffect, useRef, useState, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  IconChevronDown as ChevronDown,
  IconChevronUp as ChevronUp,
  IconInfoCircle as InfoCircle,
  IconPlus as Plus,
  IconRefresh as RefreshCw,
  IconSearch as Search,
  IconX as X,
} from "@tabler/icons-react";

import {
  SERVICE_PRESETS,
  knownModelInfo,
  servicePresetFor,
  specPair,
} from "@/lib/model-catalog-known";

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
import { formatTokensCompact } from "@/lib/format";
import {
  acceptsImagesByDefault,
  classifyModelCapabilities,
  ModelCapability,
} from "@/lib/model-capabilities";
import {
  oauthDevicePoll,
  oauthDeviceStart,
  oauthLogin,
  oauthProviders,
  type OAuthLoginOutcome,
  type OAuthProviderInfo,
} from "@/lib/chat-transport";
import { useChatStore } from "@/store/chat-store";
import type { AppConfig, EndpointProfile, ModelSpec, WireFormat } from "@/types/chat";
import { EFFORT_LEVELS } from "@/types/chat";
import { ModelIcon } from "@/components/model-icon";
import { cn } from "@/lib/utils";

const inputClass =
  "h-9 w-full rounded-lg border border-input bg-background px-3 text-base text-foreground outline-none transition-colors focus-visible:border-brand/50 focus-visible:ring-2 focus-visible:ring-ring/35";

const smallInputClass =
  "h-7 w-full rounded-lg border border-input bg-background px-2 text-sm text-foreground outline-none transition-colors focus-visible:border-brand/50";

/** Radix 的 Select 不收空 value，所以每个"没选/继承"的档位都要一个非空哨兵 */
const PROXY_INHERIT = "__inherit__";
const PROXY_FOLLOW_PROFILE = "__follow__";
const EFFORT_INHERIT = "__default__";
const CACHE_KEY_INHERIT = "__inherit__";
/** 默认模型下拉在"一个模型都没勾"时没有可选项，用哨兵占位 */
const MODEL_NONE = "__none__";
/** 服务预设下拉的"自定义服务商"：Base URL 对不上任何一家官方地址就是它 */
const CUSTOM_ENDPOINT = "__custom__";

/** 弹窗的打开状态：编辑某张卡片，或新建一套 */
export type ProfileDialogTarget =
  | { mode: "edit"; profile: EndpointProfile }
  | { mode: "create" }
  | null;

/** 新建弹窗的空白草稿：连接域全部留空——之前预填当前生效配置，baseUrl 一继承，
 *  模型列表就自动拉成了上一家的（用户当它是"残留缓存"）；思考档/温度这类**偏好**
 *  继承全局默认。凭据对继承顶层共享槽：保存时后端会把与顶层同槽的新档案铸成专属槽
 *  （见 upsert_new_profile 的 2026-10-01 修复） */
function blankDraft(config: AppConfig): EndpointProfile {
  return {
    id: "",
    name: "",
    baseUrl: "",
    model: "",
    apiFormat: "chat",
    reasoningEffort: config.reasoningEffort,
    temperature: config.temperature,
    maxTokens: 0,
    contextTokens: 0,
    autoCompact: true,
    models: [],
    promptCacheKey: null,
    cacheTtlSeconds: null,
    cacheTtlByModel: {},
    credentialService: config.credentialService,
    credentialUser: config.credentialUser,
    proxy: "",
    proxyByModel: {},
  };
}

function Field({ label, children, hint }: { label: string; children: ReactNode; hint?: ReactNode }) {
  return (
    // 不能用 <label> 包字段：里面有 Select，Chromium 会把悬停/点击转发给第一个
    // 表单控件——点说明文字会把下拉弹开
    <div className="block">
      <span className="mb-1.5 block text-xs text-muted-foreground">{label}</span>
      {children}
      {hint ? (
        <span className="mt-1.5 block text-xs leading-5 text-muted-foreground">{hint}</span>
      ) : null}
    </div>
  );
}

/** 面板头部的小搜索框：放大镜进框内，跟参考设计一个长相 */
function SearchInput({
  value,
  onChange,
  placeholder,
  ariaLabel,
  className,
}: {
  value: string;
  onChange: (value: string) => void;
  placeholder: string;
  ariaLabel: string;
  className?: string;
}) {
  return (
    <div className={cn("relative", className)}>
      <Search className="pointer-events-none absolute left-2.5 top-1/2 size-3.5 -translate-y-1/2 text-muted-foreground" />
      <input
        type="search"
        value={value}
        placeholder={placeholder}
        aria-label={ariaLabel}
        className="h-7 w-full rounded-lg border border-input bg-background pl-7 pr-2 text-sm text-foreground outline-none transition-colors placeholder:text-muted-foreground/60 focus-visible:border-brand/50"
        onChange={(event) => onChange(event.target.value)}
      />
    </div>
  );
}

/** 已勾模型这一行的读数：只念模型卡里填的事实，没填就是空，不拿档案默认冒充这一行的证据 */
function specReading(spec: ModelSpec): string {
  return specPair(spec.contextTokens, spec.maxTokens);
}

/** 未勾模型的读数来自内置目录：它是预填建议，不是这一行的事实 */
function catalogReading(model: string): string {
  const info = knownModelInfo(model);
  return info ? specPair(info.contextTokens, info.maxTokens) : "";
}

/** 左栏模型的类别（名字识别，与模型选择器同一套事实源）：
 *  视频/生图/音频各自成组，其余（推理/视觉/兜底）都是拿文字说话的对话组 */
type ModelCategory = "chat" | "image" | "video" | "audio";

const MODEL_CATEGORIES: Array<{ key: ModelCategory; label: string }> = [
  { key: "chat", label: "对话" },
  { key: "image", label: "生图" },
  { key: "video", label: "视频" },
  { key: "audio", label: "音频" },
];

function categoryOf(model: string): ModelCategory {
  const caps = classifyModelCapabilities(model);
  if (caps.includes(ModelCapability.VideoGeneration)) return "video";
  if (caps.includes(ModelCapability.ImageGeneration)) return "image";
  if (caps.includes(ModelCapability.AudioGeneration)) return "audio";
  return "chat";
}

const KIND_CHIP = "rounded-full border px-2.5 py-0.5 text-xs leading-4 transition-colors";
const KIND_CHIP_ON = "border-brand/50 bg-brand/10 text-brand-text";
const KIND_CHIP_OFF = "border-border text-muted-foreground hover:bg-accent hover:text-foreground";

/**
 * 服务商档案弹窗：一套连接 + 它名下各模型的读数。
 *
 * 形状照的是"左列勾模型、右列每模型一张卡"——因为窗口、最大输出、思考档、收不收图
 * 是**模型**的属性，不是服务商的。一张档案挂 grok 与 deepseek 时，只有 per-model 才说得清
 * 每一发该按哪个窗口压缩。档案级那几格是**默认**，勾进模型表就被那一行盖掉。
 */
export function ProfileDialog({
  target,
  onClose,
}: {
  target: ProfileDialogTarget;
  onClose: () => void;
}) {
  const config = useChatStore((s) => s.config);
  const refreshConnection = useChatStore((s) => s.refreshConnection);

  const editing = target?.mode === "edit" ? target.profile : null;
  const [name, setName] = useState(editing?.name ?? "");
  const [draft, setDraft] = useState<EndpointProfile>(() => {
    if (!editing) return blankDraft(config);
    const cloned = structuredClone(editing);
    // 存量补默认：生图/视频模型的「图像」开关按现行默认点亮（收图是这类模型的
    // 本职），只在这一格还是 false 时翻——已经显式开过的不动
    cloned.models = cloned.models.map((spec) => ({
      ...spec,
      supportsImages:
        spec.supportsImages || acceptsImagesByDefault({ id: spec.model, name: spec.model, capabilities: spec.capabilities }),
    }));
    return cloned;
  });
  const [secret, setSecret] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  // 左列的过滤与右列的"高级"展开都是界面状态，不进档案
  const [leftQuery, setLeftQuery] = useState("");
  // 左栏的类别筛选：all=按类别分组全展示；选了某一类就只看那一组
  const [leftKind, setLeftKind] = useState<ModelCategory | "all">("all");
  const [rightQuery, setRightQuery] = useState("");
  const [customModel, setCustomModel] = useState("");
  const [openAdvanced, setOpenAdvanced] = useState<Record<string, boolean>>({});
  const [profileAdvanced, setProfileAdvanced] = useState(false);

  // 草稿自己的模型列表：弹窗里的 baseUrl/密钥还没落盘，必须按草稿参数去拉
  // （带刚敲的密钥），不能借用后端当前配置
  const [draftModels, setDraftModels] = useState<string[]>([]);
  const [modelsLoading, setModelsLoading] = useState(false);
  const [modelsError, setModelsError] = useState<string | null>(null);
  const fetchSeq = useRef(0);

  async function fetchDraftModels() {
    const base = draft.baseUrl.trim();
    if (!base) {
      setDraftModels([]);
      setModelsError(null);
      return;
    }
    const seq = ++fetchSeq.current;
    setModelsLoading(true);
    setModelsError(null);
    try {
      const models = await invoke<string[]>("list_models", {
        baseUrl: base,
        apiFormat: draft.apiFormat,
        credentialService: draft.credentialService || undefined,
        credentialUser: draft.credentialUser || undefined,
        secret: secret.trim() || undefined,
      });
      if (fetchSeq.current === seq) setDraftModels(models);
    } catch (error) {
      if (fetchSeq.current === seq) {
        setDraftModels([]);
        setModelsError(error instanceof Error ? error.message : String(error));
      }
    } finally {
      if (fetchSeq.current === seq) setModelsLoading(false);
    }
  }

  useEffect(() => {
    if (target === null) return;
    const timer = setTimeout(() => void fetchDraftModels(), 700);
    return () => clearTimeout(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [
    target,
    draft.baseUrl,
    draft.apiFormat,
    draft.credentialService,
    draft.credentialUser,
    secret,
  ]);

  // ---- 官方订阅登录：浏览器授权 → 令牌写 keyring → 自动填 Base URL 与线协议 ----
  const [oauthList, setOauthList] = useState<OAuthProviderInfo[]>([]);
  const [oauthBusy, setOauthBusy] = useState<string | null>(null);
  const [oauthNote, setOauthNote] = useState<string | null>(null);
  const oauthLoaded = useRef(false);

  useEffect(() => {
    if (target && !oauthLoaded.current) {
      oauthLoaded.current = true;
      void oauthProviders()
        .then((list) => setOauthList(list))
        .catch(() => setOauthList([]));
    }
    if (!target) oauthLoaded.current = false;
  }, [target]);

  // 两种登录流（浏览器授权 / 设备码）共用的收尾：目标写进草稿，提示说清落位
  function applyLoginOutcome(outcome: OAuthLoginOutcome) {
    setDraft((previous) => ({
      ...previous,
      baseUrl: outcome.baseUrl,
      apiFormat: outcome.apiFormat as EndpointProfile["apiFormat"],
      // 凭据目标跟着登录结果走：令牌就写在这个 keyring 条目里，
      // 保存档案时目标一并落盘，读取才会命中
      credentialService: outcome.credentialService,
      credentialUser: outcome.credentialUser,
    }));
    const models = outcome.modelsHint
      ? `到「模型表」勾上常用模型（如 ${outcome.modelsHint.split(",").slice(0, 2).join("、")}）。`
      : "到「模型表」勾上要用的模型，或点拉取模型列表。";
    setOauthNote(
      `${outcome.notice ? outcome.notice : ""}登录成功：密钥已写入凭据目标 ${outcome.credentialUser}.${outcome.credentialService}。${models}改完记得点「保存」。`,
    );
  }

  // Copilot 没有公开的浏览器授权回调服务商，走 GitHub 设备码流程：后端要码并自动
  // 打开 github.com/login/device，用户抄码确认，后端轮询换令牌（最长 15 分钟）
  async function startCopilotDeviceFlow() {
    const started = await oauthDeviceStart("github-copilot");
    navigator.clipboard?.writeText(started.userCode)?.catch(() => {});
    setOauthNote(
      `GitHub 设备码 ${started.userCode}（已自动复制）。浏览器已打开 github.com/login/device——粘贴该码并确认授权，完成后这里会自动继续。`,
    );
    const outcome = await oauthDevicePoll("github-copilot", started.deviceCode, started.interval);
    applyLoginOutcome(outcome);
  }

  async function startOAuth(provider: OAuthProviderInfo) {
    setOauthBusy(provider.id);
    setOauthNote(null);
    try {
      if (provider.id === "github-copilot") {
        await startCopilotDeviceFlow();
        return;
      }
      // OAuth 令牌永远落提供方的专属凭据槽位（aglab.oauth.{id}/oauth），不传档案
      // 现有的凭据目标——那格（如 aglab/api-key.default）是全局共享的，多半存着
      // 手动 API key，顶掉它会让共用这个槽位的所有服务商一起 401（2026-10-01 事故）
      const outcome = await oauthLogin(provider.id);
      applyLoginOutcome(outcome);
    } catch (cause) {
      setOauthNote(`登录没成：${cause instanceof Error ? cause.message : String(cause)}`);
    } finally {
      setOauthBusy(null);
    }
  }

  function set<K extends keyof EndpointProfile>(key: K, value: EndpointProfile[K]) {
    setDraft((previous) => ({ ...previous, [key]: value }));
  }

  function patchSpec(model: string, patch: Partial<ModelSpec>) {
    setDraft((previous) => ({
      ...previous,
      models: previous.models.map((spec) => (spec.model === model ? { ...spec, ...patch } : spec)),
    }));
  }

  /** 勾进来 = 建一行空读数（全 0/None 表示"这行没填，用档案默认"）；取消勾 = 连它
   *  自己的代理与 TTL 覆盖一起删掉，不留看不见的孤儿键 */
  function toggleModel(model: string, checked: boolean) {
    const trimmed = model.trim();
    if (!trimmed) return;
    setDraft((previous) => {
      if (!checked) {
        const proxyByModel = { ...previous.proxyByModel };
        const cacheTtlByModel = { ...previous.cacheTtlByModel };
        delete proxyByModel[trimmed];
        delete cacheTtlByModel[trimmed];
        return {
          ...previous,
          models: previous.models.filter((spec) => spec.model !== trimmed),
          proxyByModel,
          cacheTtlByModel,
        };
      }
      if (previous.models.some((spec) => spec.model === trimmed)) return previous;
      // 目录里有规格的模型勾进来就先按目录预填：数值在模型卡里可见可改，
      // 用户改过的数永远压过目录。表里没录的还是全 0 = 用档案默认，不编数
      const known = knownModelInfo(trimmed);
      return {
        ...previous,
        models: [
          ...previous.models,
          {
            model: trimmed,
            contextTokens: known?.contextTokens ?? 0,
            maxTokens: known?.maxTokens ?? 0,
            reasoningEffort: null,
            effortLevels: [],
            // 生图/视频模型默认收图：图生图参照吃的是图片本体
            supportsImages: acceptsImagesByDefault({ id: trimmed, name: trimmed }),
            delegatable: true,
          },
        ],
      };
    });
  }

  function saveModelName(from: string, to: string) {
    const trimmed = to.trim();
    if (!trimmed) return;
    setDraft((previous) => ({
      ...previous,
      models: previous.models.map((spec) => (spec.model === from ? { ...spec, model: trimmed } : spec)),
      // 手改模型名时把两张按模型表一起搬过去：留下旧键就是一行没人读的孤儿覆盖
      proxyByModel: Object.fromEntries(
        Object.entries(previous.proxyByModel).map(([model, binding]) =>
          model === from ? [trimmed, binding] : [model, binding],
        ),
      ),
      cacheTtlByModel: Object.fromEntries(
        Object.entries(previous.cacheTtlByModel).map(([model, seconds]) =>
          model === from ? [trimmed, seconds] : [model, seconds],
        ),
      ),
      model: previous.model === from ? trimmed : previous.model,
    }));
    setOpenAdvanced((current) => {
      if (!current[from]) return current;
      const next = { ...current, [trimmed]: true };
      delete next[from];
      return next;
    });
  }

  function setProxyFor(model: string, value: string) {
    setDraft((previous) => {
      const proxyByModel = { ...previous.proxyByModel };
      if (value === PROXY_FOLLOW_PROFILE) delete proxyByModel[model];
      else proxyByModel[model] = value;
      return { ...previous, proxyByModel };
    });
  }

  function setTtlFor(model: string, raw: string) {
    setDraft((previous) => {
      const cacheTtlByModel = { ...previous.cacheTtlByModel };
      if (raw.trim() === "") delete cacheTtlByModel[model];
      else cacheTtlByModel[model] = Math.max(0, Math.round(Number(raw)) || 0);
      return { ...previous, cacheTtlByModel };
    });
  }

  async function save() {
    if (!name.trim()) {
      setError("档案名称不能为空。");
      return;
    }
    if (!draft.baseUrl.trim()) {
      setError("Base URL 不能为空。");
      return;
    }
    if (!draft.model.trim()) {
      setError("默认模型不能为空：勾一个模型当默认，或在「这套连接的默认」里手填。");
      return;
    }
    setSaving(true);
    setError(null);
    try {
      const payload: EndpointProfile = {
        ...draft,
        name: name.trim(),
        model: draft.model.trim(),
        // 空模型名的行丢弃：那一行没有键，留着就是一格谁也读不到的读数
        models: draft.models
          .filter((spec) => spec.model.trim() !== "")
          .map((spec) => ({ ...spec, model: spec.model.trim() })),
      };
      if (editing) {
        await invoke("profile_update", { draft: payload, secret: secret || null });
      } else {
        await invoke("profile_create", {
          name: name.trim(),
          draft: payload,
          secret: secret || null,
        });
      }
      await refreshConnection();
      onClose();
    } catch (error) {
      setError(error instanceof Error ? error.message : String(error));
    } finally {
      setSaving(false);
    }
  }

  const rowsByName = new Map(draft.models.map((spec) => [spec.model, spec]));
  const listed = Array.from(new Set([...draftModels, ...draft.models.map((spec) => spec.model)]));
  const query = leftQuery.trim().toLowerCase();
  const matched = query
    ? listed.filter((model) => model.toLowerCase().includes(query))
    : listed;
  // 类别分组（计数按当前搜索算，chips 才有"这一类里还剩哪些"的意义）；
  // 空组直接不出现——端点不发音频模型就不该有一个空的"音频"组
  const categoryGroups = MODEL_CATEGORIES.map(({ key, label }) => ({
    key,
    label,
    models: matched.filter((model) => categoryOf(model) === key),
  })).filter((group) => group.models.length > 0);
  const visibleGroups =
    leftKind === "all"
      ? categoryGroups
      : categoryGroups.filter((group) => group.key === leftKind);
  const rightFilter = rightQuery.trim().toLowerCase();
  const visibleSpecs = rightFilter
    ? draft.models.filter((spec) => spec.model.toLowerCase().includes(rightFilter))
    : draft.models;

  return (
    <Dialog open={target !== null} onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="flex max-h-[88vh] w-[1120px] flex-col gap-0 p-0">
        {/* 动作在顶上：这页比屏幕高，保存不该先滚一趟到底 */}
        <div className="flex shrink-0 items-start justify-between gap-4 border-b border-border px-6 py-4">
          <div className="min-w-0">
            <DialogTitle className="text-lg font-semibold tracking-tight">
              {editing ? "编辑档案" : "新建配置"}
            </DialogTitle>
            <p className="mt-1 text-xs leading-5 text-muted-foreground">
              {editing
                ? config.activeProfileId === editing.id
                  ? "这张卡片正在使用中：保存后连接立即更新（含密钥凭据目标）。"
                  : "保存只更新这张卡片，不影响当前正在使用的连接。"
                : "以当前生效的连接为底稿，保存后立即启用这套配置。"}
            </p>
          </div>
          <div className="flex shrink-0 items-center gap-2">
            <Button variant="subtle" size="sm" onClick={onClose}>
              取消
            </Button>
            <Button size="sm" disabled={saving} onClick={() => void save()}>
              {saving ? "保存中…" : "保存"}
            </Button>
          </div>
        </div>

        <div className="min-h-0 flex-1 overflow-y-auto px-6 py-5">
          {error ? <p className="mb-3 text-xs leading-5 text-destructive">{error}</p> : null}

          <Field
            label="服务预设"
            hint="选官方服务自动填 Base URL 与线协议（地址可再手改）；中转站、自建网关选自定义服务商。"
          >
            <Select
              value={servicePresetFor(draft.baseUrl)?.id ?? CUSTOM_ENDPOINT}
              onValueChange={(value) => {
                if (value === CUSTOM_ENDPOINT) return;
                const preset = SERVICE_PRESETS.find((entry) => entry.id === value);
                if (!preset) return;
                setDraft((previous) => ({
                  ...previous,
                  baseUrl: preset.baseUrl,
                  apiFormat: preset.apiFormat,
                }));
              }}
            >
              <SelectTrigger>
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value={CUSTOM_ENDPOINT}>自定义服务商</SelectItem>
                {SERVICE_PRESETS.map((preset) => (
                  <SelectItem key={preset.id} value={preset.id} className="text-base">
                    {preset.label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </Field>

          {oauthList.length > 0 ? (
            <div className="mt-3 rounded-lg border border-border bg-background px-3 py-2.5">
              <p className="text-xs leading-5 text-muted-foreground">
                用订阅登录（浏览器授权，令牌自动写入凭据目标，Base URL 与线协议一并填好）：
              </p>
              <div className="mt-2 flex flex-wrap gap-1.5">
                {oauthList.map((provider) => (
                  <Button
                    key={provider.id}
                    variant="subtle"
                    size="sm"
                    disabled={oauthBusy !== null}
                    title={provider.hint}
                    onClick={() => void startOAuth(provider)}
                  >
                    {oauthBusy === provider.id ? "等待授权…" : provider.label}
                  </Button>
                ))}
              </div>
              {oauthNote ? (
                <p className="mt-1.5 text-xs leading-5 text-muted-foreground">{oauthNote}</p>
              ) : null}
            </div>
          ) : null}

          <div className="mt-3 grid grid-cols-[1fr_1.5fr] gap-3">
            <Field label="档案名称">
              <input
                type="text"
                value={name}
                placeholder="如「DeepSeek 常用」「Claude 官方」"
                spellCheck={false}
                className={inputClass}
                onChange={(event) => setName(event.target.value)}
              />
            </Field>
            <Field label="Base URL">
              <input
                type="text"
                value={draft.baseUrl}
                placeholder="https://api.example.com/v1"
                spellCheck={false}
                className={inputClass}
                onChange={(event) => set("baseUrl", event.target.value)}
              />
            </Field>
          </div>

          <div className="mt-3 grid grid-cols-[1.5fr_1fr] gap-3">
            <Field
              label={`API 密钥（凭据目标 ${draft.credentialUser}.${draft.credentialService}）`}
              hint="密钥直接进 Windows 凭据管理器；留空则不修改该档案已保存的密钥。不同档案用不同凭据目标，互不覆盖。"
            >
              <input
                type="password"
                value={secret}
                placeholder="粘贴密钥后随档案一起保存"
                className={inputClass}
                onChange={(event) => setSecret(event.target.value)}
              />
            </Field>
            <Field label="线协议" hint="中转站常常只开其中一条，所以这是配置不是猜测。">
              <Select
                value={draft.apiFormat}
                onValueChange={(value) => set("apiFormat", value as WireFormat)}
              >
                <SelectTrigger>
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="chat">Chat Completions · /chat/completions</SelectItem>
                  <SelectItem value="responses">Responses · /responses</SelectItem>
                  <SelectItem value="anthropic">Anthropic Messages · /v1/messages</SelectItem>
                  <SelectItem value="gemini">Gemini · :streamGenerateContent</SelectItem>
                </SelectContent>
              </Select>
            </Field>
          </div>

          {/* 两栏等高：左列的头部与右列的头部要在同一条线上，各自内部滚，
              不然一边是框里的标题、一边是框外的标题，看着就是没对齐 */}
          <div className="mt-5 grid h-[52vh] min-h-[340px] grid-cols-[1fr_1.2fr] gap-4">
            {/* 左列：这一套连接名下有哪些模型。勾进来才进右侧那张表。
                未勾行的读数是内置目录的预填建议（灰一号），勾了才念这一行的事实 */}
            <section className="flex min-h-0 flex-col overflow-hidden rounded-xl border border-border bg-surface">
              <header className="flex shrink-0 items-center justify-between gap-2 border-b border-border px-3.5 py-2.5">
                <p className="text-xs font-semibold tracking-[0.08em] text-foreground-tertiary uppercase">
                  该服务商的模型
                </p>
                <div className="flex items-center gap-1.5">
                  <SearchInput
                    value={leftQuery}
                    onChange={setLeftQuery}
                    placeholder="搜模型 ID"
                    ariaLabel="搜索该服务商公布的模型"
                    className="w-36"
                  />
                  <Button
                    variant="subtle"
                    size="icon"
                    aria-label="拉取模型列表"
                    disabled={modelsLoading}
                    onClick={() => void fetchDraftModels()}
                  >
                    <RefreshCw className={cn("size-3.5", modelsLoading && "animate-spin")} />
                  </Button>
                </div>
              </header>

              {listed.length > 0 ? (
                <div className="flex shrink-0 flex-wrap items-center gap-1 border-b border-border px-3.5 py-2">
                  <button
                    type="button"
                    aria-pressed={leftKind === "all"}
                    className={cn(KIND_CHIP, leftKind === "all" ? KIND_CHIP_ON : KIND_CHIP_OFF)}
                    onClick={() => setLeftKind("all")}
                  >
                    全部 {matched.length}
                  </button>
                  {categoryGroups.map((group) => (
                    <button
                      key={group.key}
                      type="button"
                      aria-pressed={leftKind === group.key}
                      className={cn(
                        KIND_CHIP,
                        leftKind === group.key ? KIND_CHIP_ON : KIND_CHIP_OFF,
                      )}
                      onClick={() => setLeftKind(group.key)}
                    >
                      {group.label} {group.models.length}
                    </button>
                  ))}
                </div>
              ) : null}

              <div className="min-h-0 flex-1 overflow-y-auto">
                {modelsError ? (
                  <p className="px-3.5 py-2 text-xs leading-5 text-destructive">{modelsError}</p>
                ) : draftModels.length === 0 && draft.models.length === 0 ? (
                  <p className="px-3.5 py-2.5 text-xs leading-5 text-muted-foreground">
                    填好 Base URL 与密钥会自动拉取列表；服务商没发列表就在右栏底部手填。
                  </p>
                ) : null}
                {listed.length > 0 && visibleGroups.length === 0 ? (
                  <p className="px-3.5 py-2.5 text-xs leading-5 text-muted-foreground">
                    没有匹配的模型。
                  </p>
                ) : null}
                {visibleGroups.map((group) => (
                  <section key={group.key}>
                    {/* 组头吸顶：几百个模型滚起来时始终知道自己在哪一类里 */}
                    <header className="sticky top-0 z-10 flex items-center justify-between border-b border-border/60 bg-surface px-3.5 py-1">
                      <span className="text-xs font-medium text-foreground-tertiary">
                        {group.label}
                      </span>
                      <span className="text-xs tabular-nums text-muted-foreground/70">
                        {group.models.length}
                      </span>
                    </header>
                    <ul>
                      {group.models.map((model) => {
                        const spec = rowsByName.get(model);
                        const checked = spec !== undefined;
                        const info = knownModelInfo(model);
                        return (
                          <li
                            key={model}
                            className={cn(
                              "flex items-start gap-2.5 border-b border-border/50 px-3.5 py-2 transition-colors last:border-b-0",
                              checked ? "bg-brand/[0.06]" : "hover:bg-accent/50",
                            )}
                          >
                            <input
                              type="checkbox"
                              checked={checked}
                              aria-label={`${checked ? "移出" : "加入"} ${model}`}
                              className="mt-0.5 size-4 shrink-0 accent-brand"
                              onChange={(event) => toggleModel(model, event.target.checked)}
                            />
                            <div className="flex min-w-0 flex-1 items-start gap-2">
                              <span className="mt-0.5 shrink-0"><ModelIcon model={model} size={13} /></span>
                              <div className="min-w-0 flex-1">
                              <p className="truncate font-mono text-sm leading-5 text-foreground">
                                {model}
                              </p>
                              {info?.label ? (
                                <p className="truncate text-xs leading-4 text-muted-foreground">
                                  {info.label}
                                </p>
                              ) : null}
                              </div>
                            </div>
                            <span
                              className={cn(
                                "shrink-0 pt-0.5 text-xs tabular-nums",
                                checked ? "text-muted-foreground" : "text-muted-foreground/65",
                              )}
                            >
                              {checked ? specReading(spec) : catalogReading(model)}
                            </span>
                          </li>
                        );
                      })}
                    </ul>
                  </section>
                ))}
              </div>
            </section>

            {/* 右列：每个模型一张卡。窗口/输出/思考档/附件/可派工都是这一行的事实 */}
            <section className="flex min-h-0 flex-col overflow-hidden rounded-xl border border-border bg-surface">
              <header className="flex shrink-0 items-center justify-between gap-2 border-b border-border px-3.5 py-2.5">
                <p className="text-xs font-semibold tracking-[0.08em] text-foreground-tertiary uppercase">
                  模型设置 · {draft.models.length}
                </p>
                <SearchInput
                  value={rightQuery}
                  onChange={setRightQuery}
                  placeholder="搜已添加模型"
                  ariaLabel="搜索已添加的模型"
                  className="w-36"
                />
              </header>

              <div className="min-h-0 flex-1 space-y-3 overflow-y-auto p-3">
                {draft.models.length === 0 ? (
                  <p className="rounded-lg border border-dashed border-border px-3 py-6 text-center text-xs leading-5 text-muted-foreground">
                    还没勾模型。左边勾一个进来，才谈得上"这一发用多大的窗口、哪一档思考"。
                  </p>
                ) : visibleSpecs.length === 0 ? (
                  <p className="text-xs text-muted-foreground">没有匹配的已添加模型。</p>
                ) : null}

                {visibleSpecs.map((spec) => (
                  <ModelCard
                    key={spec.model}
                    spec={spec}
                    profile={draft}
                    advancedOpen={openAdvanced[spec.model] ?? false}
                    onToggleAdvanced={() =>
                      setOpenAdvanced((current) => ({ ...current, [spec.model]: !current[spec.model] }))
                    }
                    onPatch={(patch) => patchSpec(spec.model, patch)}
                    onRename={saveModelName}
                    onRemove={() => toggleModel(spec.model, false)}
                    onProxy={(value) => setProxyFor(spec.model, value)}
                    onTtl={(raw) => setTtlFor(spec.model, raw)}
                    proxies={config.proxyPool.proxies}
                    isDefault={draft.model === spec.model}
                  />
                ))}
              </div>

              {/* 自定义模型住右栏底部：目录没公布的 ID 从这里直接进设置 */}
              <footer className="shrink-0 border-t border-border px-3.5 py-3">
                <p className="text-xs font-semibold tracking-[0.08em] text-foreground-tertiary uppercase">
                  自定义模型
                </p>
                <div className="mt-2 flex items-center gap-2">
                  <input
                    type="text"
                    value={customModel}
                    placeholder="输入模型 ID，如 my-model-v2"
                    aria-label="手填模型 ID"
                    spellCheck={false}
                    className={cn(smallInputClass, "font-mono")}
                    onChange={(event) => setCustomModel(event.target.value)}
                    onKeyDown={(event) => {
                      if (event.key !== "Enter") return;
                      event.preventDefault();
                      toggleModel(customModel, true);
                      setCustomModel("");
                    }}
                  />
                  <Button
                    variant="subtle"
                    size="sm"
                    className="shrink-0"
                    disabled={customModel.trim() === ""}
                    onClick={() => {
                      toggleModel(customModel, true);
                      setCustomModel("");
                    }}
                  >
                    <Plus className="size-3.5" />
                    <span>添加</span>
                  </Button>
                </div>
                <p className="mt-1.5 text-xs leading-5 text-muted-foreground">
                  服务商没公布、内置目录也没录的 ID 从这里进。
                </p>
              </footer>
            </section>
          </div>

          {/* 档案级：这一套连接的默认值与被勾模型压过的那几格 */}
          <section className="mt-5 overflow-hidden rounded-xl border border-border bg-surface">
            <button
              type="button"
              onClick={() => setProfileAdvanced((current) => !current)}
              className="flex w-full items-center justify-between gap-3 px-3 py-2.5 text-left outline-none focus-visible:ring-2 focus-visible:ring-ring/45"
            >
              <p className="text-sm font-medium text-foreground">这套连接的默认</p>
              <span className="flex items-center gap-1.5 text-xs text-muted-foreground">
                {profileAdvanced ? "收起" : "展开"}
                {profileAdvanced ? (
                  <ChevronUp className="size-3.5" />
                ) : (
                  <ChevronDown className="size-3.5" />
                )}
              </span>
            </button>
            {!profileAdvanced ? (
              <p className="px-3 pb-2.5 text-xs leading-5 text-muted-foreground">
                默认模型 <span className="font-mono text-foreground">{draft.model || "（空）"}</span> ·
                窗口 {formatTokensCompact(draft.contextTokens)} · 思考档{" "}
                {EFFORT_LEVELS.find((level) => level.value === draft.reasoningEffort)?.label ??
                  (draft.reasoningEffort === "" ? "不发送" : draft.reasoningEffort)} · 代理绑定{" "}
                {draft.proxy === "" ? "继承全局" : draft.proxy === "direct" ? "直连" : draft.proxy === "pool" ? "代理池" : "指定代理"}
                。被勾进来的模型有自己的行时以那一行为准。
              </p>
            ) : null}

            {profileAdvanced ? (
              <div className="space-y-3.5 border-t border-border px-3 py-3">
                <div className="grid grid-cols-2 gap-3">
                  <Field
                    label="默认模型"
                    hint={
                      draft.models.length > 0
                        ? "没被任何一行盖住的请求（含手填的模型名）都按这套连接的默认走。"
                        : "一个模型都没勾时，这里是手填的输入框。"
                    }
                  >
                    {draft.models.length > 0 ? (
                      <Select
                        value={
                          rowsByName.has(draft.model) ? draft.model : MODEL_NONE
                        }
                        onValueChange={(value) => {
                          if (value === MODEL_NONE) return;
                          set("model", value);
                        }}
                      >
                        <SelectTrigger>
                          <SelectValue />
                        </SelectTrigger>
                        <SelectContent>
                          {rowsByName.has(draft.model) ? null : (
                            <SelectItem value={MODEL_NONE}>
                              {draft.model || "（没填）"}——不在已勾模型里
                            </SelectItem>
                          )}
                          {draft.models.map((spec) => (
                            <SelectItem key={spec.model} value={spec.model} className="font-mono text-sm">
                              {spec.model}
                            </SelectItem>
                          ))}
                        </SelectContent>
                      </Select>
                    ) : (
                      <input
                        type="text"
                        value={draft.model}
                        placeholder="模型 ID"
                        spellCheck={false}
                        className={cn(inputClass, "font-mono")}
                        onChange={(event) => set("model", event.target.value)}
                      />
                    )}
                  </Field>

                  <Field
                    label="代理"
                    hint="继承全局 = 用设置 → 代理 里的全局绑定；写 direct 表示这个服务商明确直连。"
                  >
                    <Select
                      value={draft.proxy === "" ? PROXY_INHERIT : draft.proxy}
                      onValueChange={(value) => set("proxy", value === PROXY_INHERIT ? "" : value)}
                    >
                      <SelectTrigger>
                        <SelectValue />
                      </SelectTrigger>
                      <SelectContent>
                        <SelectItem value={PROXY_INHERIT}>继承全局</SelectItem>
                        <SelectItem value="direct">直连</SelectItem>
                        <SelectItem value="pool">代理池（按策略均衡）</SelectItem>
                        {config.proxyPool.proxies.map((entry) => (
                          <SelectItem key={entry.id} value={entry.id} className="text-sm">
                            {entry.name}
                            {entry.enabled ? "" : "（已停用）"}
                          </SelectItem>
                        ))}
                      </SelectContent>
                    </Select>
                  </Field>
                </div>

                <div className="grid grid-cols-2 gap-3">
                  <Field label="默认思考程度" hint="空 = 不向服务商发送 reasoning 字段。">
                    <Select
                      value={draft.reasoningEffort === "" ? EFFORT_INHERIT : draft.reasoningEffort}
                      onValueChange={(value) =>
                        set("reasoningEffort", value === EFFORT_INHERIT ? "" : value)
                      }
                    >
                      <SelectTrigger>
                        <SelectValue />
                      </SelectTrigger>
                      <SelectContent>
                        <SelectItem value={EFFORT_INHERIT}>默认（不发送）</SelectItem>
                        {EFFORT_LEVELS.map((level) => (
                          <SelectItem key={level.value} value={level.value} className="text-sm">
                            {level.label}
                          </SelectItem>
                        ))}
                      </SelectContent>
                    </Select>
                  </Field>
                  <Field label="缓存身份（prompt_cache_key）" hint="「跟随内置表」按服务商判断（OpenAI 开、Claude/DeepSeek 关）；明确开启或关闭压过内置表。">
                    <Select
                      value={
                        draft.promptCacheKey === null
                          ? CACHE_KEY_INHERIT
                          : draft.promptCacheKey
                            ? "on"
                            : "off"
                      }
                      onValueChange={(value) =>
                        set("promptCacheKey", value === CACHE_KEY_INHERIT ? null : value === "on")
                      }
                    >
                      <SelectTrigger>
                        <SelectValue />
                      </SelectTrigger>
                      <SelectContent>
                        <SelectItem value={CACHE_KEY_INHERIT}>跟随内置表</SelectItem>
                        <SelectItem value="on">明确开启</SelectItem>
                        <SelectItem value="off">明确关闭</SelectItem>
                      </SelectContent>
                    </Select>
                  </Field>
                </div>

                <div className="grid grid-cols-[1fr_1fr_1.6fr] gap-3">
                  <Field label="默认上下文窗口" hint="0 = 没填。被勾模型的行压过它。">
                    <input
                      type="number"
                      min={1000}
                      step={1000}
                      value={draft.contextTokens}
                      className={inputClass}
                      onChange={(event) => set("contextTokens", Number(event.target.value) || 0)}
                    />
                  </Field>
                  <Field label="默认最大输出" hint="0 = 没填。">
                    <input
                      type="number"
                      min={1}
                      max={128000}
                      step={256}
                      value={draft.maxTokens}
                      className={inputClass}
                      onChange={(event) => set("maxTokens", Number(event.target.value) || 0)}
                    />
                  </Field>
                  <Field label="默认缓存存活期（秒）" hint="空 = 跟随内置表；0 = 未知寿命，缓存保温不会跑。">
                    <input
                      type="number"
                      min={0}
                      step={30}
                      value={draft.cacheTtlSeconds ?? ""}
                      placeholder="跟随内置表"
                      className={cn(inputClass, "placeholder:text-muted-foreground/50")}
                      onChange={(event) => {
                        const raw = event.target.value;
                        set(
                          "cacheTtlSeconds",
                          raw.trim() === "" ? null : Math.max(0, Math.round(Number(raw)) || 0),
                        );
                      }}
                    />
                  </Field>
                </div>

                <div className="flex items-center justify-between gap-3 rounded-lg border border-border bg-background px-3 py-3">
                  <p className="min-w-0 text-xs leading-5 text-muted-foreground">
                    发送前估算上下文超过窗口的 90% 时，自动把更早的对话压缩成摘要再继续。
                  </p>
                  <CapabilityToggle
                    label="自动压缩上下文"
                    enabled={draft.autoCompact}
                    onToggle={() => set("autoCompact", !draft.autoCompact)}
                  />
                </div>

                <div className="grid grid-cols-2 gap-3">
                  <Field label="凭据服务（高级）">
                    <input
                      type="text"
                      value={draft.credentialService}
                      spellCheck={false}
                      className={inputClass}
                      onChange={(event) => set("credentialService", event.target.value)}
                    />
                  </Field>
                  <Field
                    label="凭据用户（高级）"
                    hint="全部档案默认共用同一凭据目标：一把 key 伺候多个服务商会互相顶掉。多服务商请给每张档案填不同的凭据用户（如 default、hyb、openrouter），再各存各的密钥。"
                  >
                    <input
                      type="text"
                      value={draft.credentialUser}
                      spellCheck={false}
                      className={inputClass}
                      onChange={(event) => set("credentialUser", event.target.value)}
                    />
                  </Field>
                </div>
              </div>
            ) : null}
          </section>
        </div>
      </DialogContent>
    </Dialog>
  );
}

/** 一个模型一张卡。卡片里的读数压过档案默认——命不中这一行的请求才用默认那一份 */
function ModelCard({
  spec,
  profile,
  advancedOpen,
  proxies,
  isDefault,
  onToggleAdvanced,
  onPatch,
  onRename,
  onRemove,
  onProxy,
  onTtl,
}: {
  spec: ModelSpec;
  profile: EndpointProfile;
  advancedOpen: boolean;
  proxies: AppConfig["proxyPool"]["proxies"];
  isDefault: boolean;
  onToggleAdvanced: () => void;
  onPatch: (patch: Partial<ModelSpec>) => void;
  onRename: (from: string, to: string) => void;
  onRemove: () => void;
  onProxy: (value: string) => void;
  onTtl: (raw: string) => void;
}) {
  // 空数组 = 全部档位可选。所以"全亮"是默认态，不是用户挑过六次
  const allowed = spec.effortLevels.length === 0 ? EFFORT_LEVELS.map((level) => level.value) : spec.effortLevels;

  function toggleLevel(value: string) {
    const next = allowed.includes(value)
      ? allowed.filter((level) => level !== value)
      : [...allowed, value];
    // 全不选等于没限制：退回"全部可选"那个默认态，别存一份长得不一样的全表
    onPatch({
      effortLevels: next.length === EFFORT_LEVELS.length ? [] : next,
      // 默认档落在被取消的档位上就清回继承：留着一行发不出去的档是假配置
      reasoningEffort:
        spec.reasoningEffort && !next.includes(spec.reasoningEffort) ? null : spec.reasoningEffort,
    });
  }

  return (
    <article className="overflow-hidden rounded-xl border border-border bg-background">
      <header className="flex items-center gap-2 border-b border-border px-3 py-2.5">
        <input
          type="text"
          value={spec.model}
          aria-label="模型 ID"
          spellCheck={false}
          className="min-w-0 flex-1 truncate rounded border border-transparent bg-transparent px-1 py-0.5 font-mono text-base text-foreground outline-none transition-colors hover:border-border focus-visible:border-brand/50"
          onChange={(event) => onPatch({ model: event.target.value })}
          onBlur={(event) => onRename(spec.model, event.target.value)}
        />
        {/* 读数紧跟模型名：它和左列那一栏是同一个数，位置也该对得上 */}
        <span className="shrink-0 text-xs tabular-nums text-muted-foreground">
          {specReading(spec)}
        </span>
        {isDefault ? (
          <span className="shrink-0 rounded-md border border-brand/45 bg-brand/10 px-1.5 py-0.5 text-2xs text-brand-text">
            这套连接的默认
          </span>
        ) : null}
        <button
          type="button"
          onClick={onToggleAdvanced}
          className="shrink-0 rounded-md border border-border px-1.5 py-0.5 text-2xs text-muted-foreground outline-none transition-colors hover:bg-accent hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45"
        >
          高级
        </button>
        <button
          type="button"
          aria-label={`把 ${spec.model} 从这套连接里移除`}
          onClick={onRemove}
          className="flex size-6 shrink-0 items-center justify-center rounded-md text-muted-foreground outline-none transition-colors hover:bg-accent hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45"
        >
          <X className="size-3.5" />
        </button>
      </header>

      <div className="space-y-3 px-3 py-3">
        <div className="grid grid-cols-2 gap-3">
          <Field label="上下文窗口" hint="0 = 用档案默认。">
            <input
              type="number"
              min={0}
              step={1000}
              value={spec.contextTokens}
              className={smallInputClass}
              onChange={(event) => onPatch({ contextTokens: Number(event.target.value) || 0 })}
            />
          </Field>
          <Field label="最大输出" hint="0 = 用档案默认。">
            <input
              type="number"
              min={0}
              max={128000}
              step={256}
              value={spec.maxTokens}
              className={smallInputClass}
              onChange={(event) => onPatch({ maxTokens: Number(event.target.value) || 0 })}
            />
          </Field>
        </div>

        <div>
          <p className="mb-1.5 text-xs text-muted-foreground">
            思考档 · 不选任何一档 = 全部可选
          </p>
          <div className="flex flex-wrap gap-1.5">
            {EFFORT_LEVELS.map((level) => {
              const on = allowed.includes(level.value);
              return (
                <button
                  key={level.value}
                  type="button"
                  aria-pressed={on}
                  onClick={() => toggleLevel(level.value)}
                  className={cn(
                    "rounded-lg border px-2 py-1 text-xs outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
                    on
                      ? "border-brand/45 bg-brand/10 text-brand-text"
                      : "border-border text-muted-foreground hover:bg-accent hover:text-foreground",
                  )}
                >
                  {level.label}
                </button>
              );
            })}
          </div>
        </div>

        <Field label="默认思考档" hint="继承档案 = 这一行不表态。">
          <Select
            value={
              spec.reasoningEffort === null
                ? EFFORT_INHERIT
                : spec.reasoningEffort === ""
                  ? "__off__"
                  : spec.reasoningEffort
            }
            onValueChange={(value) =>
              onPatch({
                reasoningEffort:
                  value === EFFORT_INHERIT ? null : value === "__off__" ? "" : value,
              })
            }
          >
            <SelectTrigger className="h-7 text-sm">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value={EFFORT_INHERIT}>继承档案</SelectItem>
              <SelectItem value="__off__">不发送该字段</SelectItem>
              {EFFORT_LEVELS.filter((level) => allowed.includes(level.value)).map((level) => (
                <SelectItem key={level.value} value={level.value} className="text-sm">
                  {level.label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </Field>

        <div>
          <p className="mb-1.5 text-xs text-muted-foreground">多模态能力</p>
          <div className="flex flex-wrap gap-1.5">
            {/* 三类都是真的：附件本体发不发由这里的开关与四家方言外壳共同决定 */}
            {(
              [
                ["图像", "supportsImages"],
                ["视频", "supportsVideo"],
                ["音频", "supportsAudio"],
              ] as Array<["图像" | "视频" | "音频", "supportsImages" | "supportsVideo" | "supportsAudio"]>
            ).map(([label, field]) => {
              const on = Boolean(spec[field]);
              return (
                <button
                  key={field}
                  type="button"
                  aria-pressed={on}
                  onClick={() => onPatch({ [field]: !on })}
                  className={cn(
                    "rounded-lg border px-2.5 py-1 text-xs outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
                    on
                      ? "border-brand/45 bg-brand/10 text-brand-text"
                      : "border-border text-muted-foreground hover:bg-accent hover:text-foreground",
                  )}
                >
                  {label}
                </button>
              );
            })}
          </div>
          <p className="mt-1 text-xs leading-5 text-muted-foreground">
            勾「图像」才把图片本体发给这个模型，勾「视频/音频」才把音视频本体随行发去识别；没勾就只在正文里写路径与大小，模型看不到/听不到内容。
            生图/视频模型默认开启「图像」（图生图参照吃的是图片本体），对话模型按需勾选；音视频按模型说明书勾，端点不支持时会被拒。
          </p>
        </div>

        <div>
          <div className="mb-1.5 flex items-center justify-between gap-2">
            <p className="text-xs text-muted-foreground">生成能力</p>
            {/* 存量陷阱的出口：识别名单升级前存下的错误声明（比如把 gpt-image 存成了对话）
                会一直被"声明优先"信任——一键清空，回到按名字识别 */}
            {spec.capabilities?.length ? (
              <button
                type="button"
                onClick={() =>
                  onPatch({
                    capabilities: [],
                    // 清声明回启发式，「图像」也跟着按启发式预置：声明可能已经过时
                    supportsImages: acceptsImagesByDefault({ id: spec.model, name: spec.model }),
                  })
                }
                className="rounded px-1.5 py-0.5 text-2xs text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
              >
                按名字重新识别
              </button>
            ) : null}
          </div>
          <div className="flex flex-wrap gap-1.5">
            {(["chat", "image", "video", "audio"] as const).map((cap) => {
              // 未标注时按模型名启发式识别作为底稿（seedream→生图、kling/sora→视频），
              // 用户一勾选就固化成显式配置
              const current = spec.capabilities?.length
                ? spec.capabilities
                : classifyModelCapabilities(spec.model);
              const on = current.includes(cap);
              const next = on
                ? current.filter((item) => item !== cap)
                : [...current, cap];
              const label =
                cap === "chat"
                  ? "对话"
                  : cap === "image"
                    ? "生图"
                    : cap === "video"
                      ? "视频"
                      : "音频";
              return (
                <button
                  key={cap}
                  type="button"
                  aria-pressed={on}
                  onClick={() => {
                    const patch: Partial<typeof spec> = { capabilities: next };
                    // 勾上生图/视频的同时把「图像」点亮：这类模型收图是本职
                    // （图生图参照吃图片本体）。取消勾选不动「图像」，留给用户自己关
                    if (!on && cap !== "chat" && !spec.supportsImages) {
                      patch.supportsImages = true;
                    }
                    onPatch(patch);
                  }}
                  className={cn(
                    "rounded-lg border px-2.5 py-1 text-xs outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
                    on
                      ? "border-brand/45 bg-brand/10 text-brand-text"
                      : "border-border text-muted-foreground hover:bg-accent hover:text-foreground",
                  )}
                >
                  {label}
                </button>
              );
            })}
          </div>
          <p className="mt-1 text-xs leading-5 text-muted-foreground">
            生图/视频会话的模型选择器只列勾了对应能力的模型。没标注的按模型名自动识别
            （seedream/cogview → 生图，kling/sora/cogvideo → 视频，tts/speech → 音频）。
          </p>
        </div>

        <div>
          <label className="flex items-center gap-1.5 text-sm text-foreground">
            <input
              type="checkbox"
              checked={spec.delegatable}
              className="size-4 shrink-0 accent-brand"
              onChange={(event) => onPatch({ delegatable: event.target.checked })}
            />
            可被 AI 派工
            <InfoCircle
              className="size-3.5 text-muted-foreground"
              aria-label="关掉后，子助理/编排/定时任务不会被自动调度到这一行；你自己的聊天与明确点名不受影响。"
            />
          </label>
          <p className="mt-1 text-xs leading-5 text-muted-foreground">
            关掉后，子助理/编排/定时任务不会被自动调度到这一行；你自己的聊天与明确点名不受影响。
          </p>
        </div>

        {advancedOpen ? (
          <div className="grid grid-cols-2 gap-3 border-t border-border pt-3">
            <Field label="这一发的代理" hint="跟随档案 = 用上面那格服务商绑定。">
              <Select
                value={
                  spec.model in profile.proxyByModel
                    ? profile.proxyByModel[spec.model] === ""
                      ? "direct"
                      : profile.proxyByModel[spec.model]
                    : PROXY_FOLLOW_PROFILE
                }
                onValueChange={onProxy}
              >
                <SelectTrigger className="h-7 text-sm">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value={PROXY_FOLLOW_PROFILE}>跟随档案</SelectItem>
                  <SelectItem value="direct">直连</SelectItem>
                  <SelectItem value="pool">代理池（按策略均衡）</SelectItem>
                  {proxies.map((entry) => (
                    <SelectItem key={entry.id} value={entry.id} className="text-sm">
                      {entry.name}
                      {entry.enabled ? "" : "（已停用）"}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </Field>
            <Field label="这一发的缓存存活期（秒）" hint="空 = 跟随档案与内置表。最具体的一条证据。">
              <input
                type="number"
                min={0}
                step={30}
                value={spec.model in profile.cacheTtlByModel ? profile.cacheTtlByModel[spec.model] : ""}
                placeholder="跟随档案"
                className={cn(smallInputClass, "placeholder:text-muted-foreground/50")}
                onChange={(event) => onTtl(event.target.value)}
              />
            </Field>
          </div>
        ) : null}
      </div>
    </article>
  );
}
