/**
 * 内置模型目录与服务预设：弹窗里的"展示名 / 规格 / 一键填地址"都从这来。
 *
 * 目录只做**预填与展示**，不锁死任何一格——用户在模型卡里填的数永远压过这里。
 * 表里没录的模型就老实显示空白，不编规格；要补条目就往表里加一行。
 * （对齐参考设计：左列每行右侧的 "400K · 128K" 与模型 ID 下的展示名。）
 */

export interface KnownModelInfo {
  /** 展示名，如 "GPT-5.3 Codex"。没有正式名字的条目省略，界面只显示 ID */
  label?: string;
  contextTokens: number;
  maxTokens: number;
}

interface CatalogRule {
  /** 精确 ID（小写比较） */
  id?: string;
  /** 前缀匹配（小写比较）。精确表先查，前缀表按数组顺序先到先得 */
  prefix?: string;
  info: KnownModelInfo;
}

/**
 * 精确表：正式发布的模型 ID 一条一录。gpt-5.x 系取自参考设计的目录读数
 * （列表显示 1.1M 是 1_050_000 的缩写）。
 */
const EXACT_RULES: Record<string, KnownModelInfo> = {
  "gpt-5.3-codex": { label: "GPT-5.3 Codex", contextTokens: 400_000, maxTokens: 128_000 },
  "gpt-5.3-codex-spark": {
    label: "GPT-5.3 Codex Spark",
    contextTokens: 128_000,
    maxTokens: 32_000,
  },
  "gpt-5.4": { label: "GPT-5.4", contextTokens: 1_050_000, maxTokens: 128_000 },
  "gpt-5.4-mini": { label: "GPT-5.4 mini", contextTokens: 400_000, maxTokens: 128_000 },
  "gpt-5.4-nano": { label: "GPT-5.4 nano", contextTokens: 400_000, maxTokens: 128_000 },
  "gpt-5.4-pro": { label: "GPT-5.4 Pro", contextTokens: 1_050_000, maxTokens: 128_000 },
  "gpt-5.5": { label: "GPT-5.5", contextTokens: 1_050_000, maxTokens: 128_000 },
  "gpt-5.5-pro": { label: "GPT-5.5 Pro", contextTokens: 1_050_000, maxTokens: 128_000 },
  "gpt-5.6": { label: "GPT-5.6", contextTokens: 1_050_000, maxTokens: 128_000 },
  "gpt-5.6-luna": { label: "GPT-5.6 Luna", contextTokens: 1_050_000, maxTokens: 128_000 },
  "gpt-5.6-sol": { label: "GPT-5.6 Sol", contextTokens: 1_050_000, maxTokens: 128_000 },
  "gpt-5.6-terra": { label: "GPT-5.6 Terra", contextTokens: 1_050_000, maxTokens: 128_000 },
  "gpt-6-astra": { label: "GPT-6 Astra", contextTokens: 1_050_000, maxTokens: 128_000 },
  "grok-4": { label: "Grok 4", contextTokens: 256_000, maxTokens: 32_000 },
  "deepseek-chat": { label: "DeepSeek Chat", contextTokens: 128_000, maxTokens: 8_000 },
  "deepseek-reasoner": { label: "DeepSeek Reasoner", contextTokens: 128_000, maxTokens: 64_000 },
};

/** 前缀表：家族变体（带日期后缀、-latest 之类）兜底。顺序就是优先级 */
const PREFIX_RULES: CatalogRule[] = [
  { prefix: "gpt-5.3-codex", info: { contextTokens: 400_000, maxTokens: 128_000 } },
  { prefix: "gpt-5.4-mini", info: { contextTokens: 400_000, maxTokens: 128_000 } },
  { prefix: "gpt-5.4-nano", info: { contextTokens: 400_000, maxTokens: 128_000 } },
  { prefix: "gpt-5.4-pro", info: { contextTokens: 1_050_000, maxTokens: 128_000 } },
  { prefix: "gpt-5", info: { contextTokens: 1_050_000, maxTokens: 128_000 } },
  { prefix: "gpt-4.1", info: { contextTokens: 1_000_000, maxTokens: 32_000 } },
  { prefix: "gpt-4o", info: { contextTokens: 128_000, maxTokens: 16_000 } },
  { prefix: "o3", info: { contextTokens: 200_000, maxTokens: 100_000 } },
  { prefix: "o4-mini", info: { contextTokens: 200_000, maxTokens: 100_000 } },
  { prefix: "claude-opus", info: { contextTokens: 200_000, maxTokens: 32_000 } },
  { prefix: "claude-sonnet", info: { contextTokens: 200_000, maxTokens: 64_000 } },
  { prefix: "claude-haiku", info: { contextTokens: 200_000, maxTokens: 64_000 } },
  { prefix: "claude", info: { contextTokens: 200_000, maxTokens: 32_000 } },
  { prefix: "gemini", info: { contextTokens: 1_000_000, maxTokens: 64_000 } },
  { prefix: "deepseek", info: { contextTokens: 128_000, maxTokens: 8_000 } },
  { prefix: "grok-4-fast", info: { contextTokens: 2_000_000, maxTokens: 32_000 } },
  { prefix: "kimi", info: { contextTokens: 256_000, maxTokens: 8_000 } },
  { prefix: "glm", info: { contextTokens: 200_000, maxTokens: 128_000 } },
];

/**
 * 查一个模型 ID 的目录信息。表里没有就返回 null——不编造规格。
 * 大小写不敏感：服务商返回的 ID 通常是小写，但手填不一定。
 */
export function knownModelInfo(modelId: string): KnownModelInfo | null {
  const key = modelId.trim().toLowerCase();
  if (!key) return null;
  const exact = EXACT_RULES[key];
  if (exact) return exact;
  const rule = PREFIX_RULES.find(
    (entry) => entry.prefix !== undefined && key.startsWith(entry.prefix),
  );
  return rule ? { ...rule.info } : null;
}

/** 一条服务预设：选中即把 Base URL 与线协议填进草稿（地址可再手改） */
export interface ServicePreset {
  id: string;
  label: string;
  baseUrl: string;
  apiFormat: "chat" | "responses" | "anthropic";
}

/**
 * 常用服务的官方入口。Anthropic 官方地址不带 /v1（后端按线协议自己补
 * /v1/messages）；其余 OpenAI 兼容服务都带 /v1。新增预设注意 baseUrl 全局唯一，
 * 弹窗靠它反推当前选中的是哪家。
 */
export const SERVICE_PRESETS: ServicePreset[] = [
  { id: "openai", label: "OpenAI", baseUrl: "https://api.openai.com/v1", apiFormat: "responses" },
  {
    id: "anthropic",
    label: "Anthropic",
    baseUrl: "https://api.anthropic.com",
    apiFormat: "anthropic",
  },
  { id: "deepseek", label: "DeepSeek", baseUrl: "https://api.deepseek.com/v1", apiFormat: "chat" },
  {
    id: "openrouter",
    label: "OpenRouter",
    baseUrl: "https://openrouter.ai/api/v1",
    apiFormat: "chat",
  },
  { id: "xai", label: "xAI (Grok)", baseUrl: "https://api.x.ai/v1", apiFormat: "chat" },
  { id: "groq", label: "Groq", baseUrl: "https://api.groq.com/openai/v1", apiFormat: "chat" },
  {
    id: "moonshot",
    label: "Moonshot AI (Kimi)",
    baseUrl: "https://api.moonshot.cn/v1",
    apiFormat: "chat",
  },
  {
    id: "zhipu",
    label: "智谱 GLM",
    baseUrl: "https://open.bigmodel.cn/api/paas/v4",
    apiFormat: "chat",
  },
  {
    id: "alibaba",
    label: "阿里云百炼",
    baseUrl: "https://dashscope.aliyuncs.com/compatible-mode/v1",
    apiFormat: "chat",
  },
];

/** 按 Base URL 反推是哪家预设（忽略尾部斜杠）。不匹配 = 自定义服务商 */
export function servicePresetFor(baseUrl: string): ServicePreset | null {
  const normalized = baseUrl.trim().replace(/\/+$/, "");
  return SERVICE_PRESETS.find((preset) => preset.baseUrl === normalized) ?? null;
}

function compactTokens(value: number): string {
  if (value >= 1_000_000) {
    const millions = value / 1_000_000;
    return `${Number.isInteger(millions) ? millions.toFixed(0) : millions.toFixed(1)}M`;
  }
  if (value >= 1_000) {
    const thousands = value / 1_000;
    return `${Number.isInteger(thousands) ? thousands.toFixed(0) : thousands.toFixed(1)}K`;
  }
  return String(value);
}

/**
 * 规格对的目录写法："1.1M · 128K"。没填的那一侧画 "—"，两侧都没填返回空串
 * （调用方决定要不要显示别的文案）。
 */
export function specPair(contextTokens: number, maxTokens: number): string {
  if (contextTokens <= 0 && maxTokens <= 0) return "";
  const context = contextTokens > 0 ? compactTokens(contextTokens) : "—";
  const output = maxTokens > 0 ? compactTokens(maxTokens) : "—";
  return `${context} · ${output}`;
}
