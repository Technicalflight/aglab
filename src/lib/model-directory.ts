import { invoke } from "@tauri-apps/api/core";

/**
 * 全球模型能力规格目录的前端侧：Rust 按五源优先级拉取归一好的索引，
 * 这里负责缓存与查找。查到的规格住 profile-dialog 做两件事——
 * 勾模型时预填能力/窗口（存进 ModelSpec，随声明优先的老路径全 app 生效）、
 * 左列行上亮能力小徽章。识别不到就静默落空，内置目录与名字正则照旧兜底。
 */

/** 一个模型的规格。capabilities 值与 ModelCapability 枚举字面量一致 */
export interface ModelDirectorySpec {
  capabilities: string[];
  inputModalities: string[];
  outputModalities: string[];
  contextTokens: number;
  maxTokens: number;
}

/** 一次成功加载：来源与索引（键 = 小写模型 id） */
export interface ModelDirectory {
  source: string;
  total: number;
  models: Record<string, ModelDirectorySpec>;
}

const CACHE_KEY = "aglab.model-directory.v1";
const TTL_MS = 24 * 60 * 60 * 1000;

let memo: ModelDirectory | null = null;
let loading: Promise<ModelDirectory | null> | null = null;

function readCache(): ModelDirectory | null {
  try {
    const raw = localStorage.getItem(CACHE_KEY);
    if (!raw) return null;
    const parsed = JSON.parse(raw) as { fetchedAt: number; directory: ModelDirectory };
    if (!parsed?.directory?.models || Date.now() - parsed.fetchedAt > TTL_MS) return null;
    return parsed.directory;
  } catch {
    return null;
  }
}

function writeCache(directory: ModelDirectory) {
  try {
    localStorage.setItem(CACHE_KEY, JSON.stringify({ fetchedAt: Date.now(), directory }));
  } catch {
    // localStorage 满/不可用就只留内存份，别为缓存把功能弄挂
  }
}

/** 加载目录（内存 → localStorage 24h → 后端五源）。失败返回 null，不抛——
 *  规格识别是锦上添花，不该在弹窗里报错打扰 */
export function loadModelDirectory(force = false): Promise<ModelDirectory | null> {
  if (!force) {
    if (memo) return Promise.resolve(memo);
    const cached = readCache();
    if (cached) {
      memo = cached;
      return Promise.resolve(memo);
    }
  }
  loading ??= invoke<ModelDirectory>("model_directory")
    .then((directory) => {
      memo = directory;
      writeCache(directory);
      return directory;
    })
    .catch(() => null)
    .finally(() => {
      loading = null;
    });
  return loading;
}

/** 已加载就同步拿到（弹窗渲染路径用），没加载 = null */
export function directoryIfLoaded(): ModelDirectory | null {
  return memo;
}

// ---- 模糊匹配：端点给的名字带各种尾巴（-preview、日期、命名空间、4-5/4.5），
// 名字 + 版本对上就算同一个模型。归一结果缓存成 core 索引，每行渲染的查找是 O(1) ----

/** 无信息量的修饰尾缀：去掉它们剩下的才是名字与版本 */
const NOISE_TOKENS = new Set([
  "preview",
  "latest",
  "stable",
  "experimental",
  "exp",
  "snapshot",
  "free",
]);

function tokenize(id: string): string[] {
  return id
    .toLowerCase()
    .split(/[^a-z0-9.]+/)
    .filter(Boolean);
}

/** 日期样的纯数字 token：20241120 / 2024 / 0528（MMDD）。版本号不在此列（4.5、r1 都不是纯数字） */
function isDateLike(token: string): boolean {
  return (
    /^\d{8}$/.test(token) ||
    /^(19|20)\d{2}$/.test(token) ||
    /^(0[1-9]|1[0-2])(0[1-9]|[12]\d|3[01])$/.test(token)
  );
}

/**
 * 名字+版本的核心串：小写、去修饰尾缀、去尾部日期、拼掉分隔符（含命名空间斜杠）。
 * "gpt-4o-2024-11-20"→"gpt4o"、"gemini-2.5-pro-preview-06-05"→"gemini25pro"、
 * "claude-sonnet-4-5"→"claudesonnet45"（与目录 "claude-sonnet-4.5" 同串）。
 * 版本差异不会被抹掉："minimax-m3" 与 "minimax-m2" 核心不同。
 * keepNamespace 时厂商名也进核心（"minimax/m3-preview"→"minimaxm3"，
 * 对得上目录里厂商嵌名的 "minimax-m3"）
 */
function coreOf(id: string, keepNamespace = false): string {
  const bare = !keepNamespace && id.includes("/") ? id.slice(id.lastIndexOf("/") + 1) : id;
  const raw = tokenize(bare);
  const kept = raw.filter((token) => token !== "v" && !NOISE_TOKENS.has(token));
  const droppedNoise = kept.length < raw.length;
  // 尾部纯数字串是日期就整串去掉（gpt-4o-2024-11-20）；带了 preview 之类修饰的，
  // 跟在后面的短数字几乎必是日期（gemini-2.5-pro-preview-06-05），长度≥2 也去掉。
  // 单个非日期数字是版本（claude-4），保留
  let end = kept.length;
  if (end > 0 && /^\d+$/.test(kept[end - 1])) {
    let start = end;
    while (start > 0 && /^\d+$/.test(kept[start - 1])) start -= 1;
    const runLength = end - start;
    if (isDateLike(kept[start]) || (droppedNoise && runLength >= 2)) end = start;
  }
  // 分隔符差异一并抹掉：claude-sonnet-4-5 与 claude-sonnet-4.5 是同一个版本
  return kept.slice(0, end).join("").replace(/\./g, "");
}

interface CoreIndexEntry {
  spec: ModelDirectorySpec;
  /** 原始键长度：同一核心有多个条目时（"gpt-4o" 与 "openai/gpt-4o"），短的是正主 */
  keyLength: number;
}

let coreIndex: { source: ModelDirectory; map: Map<string, CoreIndexEntry> } | null = null;

function buildCoreIndex(directory: ModelDirectory): Map<string, CoreIndexEntry> {
  if (coreIndex?.source === directory) return coreIndex.map;
  const map = new Map<string, CoreIndexEntry>();
  for (const [key, spec] of Object.entries(directory.models)) {
    const core = coreOf(key);
    if (!core) continue;
    const prev = map.get(core);
    if (!prev || key.length < prev.keyLength) {
      map.set(core, { spec, keyLength: key.length });
    }
  }
  coreIndex = { source: directory, map };
  return map;
}

/**
 * 查规格：精确小写 → 斜杠两侧互为后缀 → 模糊核心（名字+版本，容忍
 * -preview/日期/分隔符/命名空间差异）。都查不到返回 null
 */
export function lookupModelDirectorySpec(model: string): ModelDirectorySpec | null {
  const directory = memo;
  if (!directory) return null;
  const id = model.trim().toLowerCase();
  if (!id) return null;
  const direct = directory.models[id];
  if (direct) return direct;
  const slash = id.indexOf("/");
  if (slash >= 0) {
    const hit = directory.models[id.slice(slash + 1)];
    if (hit) return hit;
  }
  // 目录键带命名空间而用户 id 不带：扫一遍后缀（3938 条的 Map 扫描一次可忽略）
  for (const [key, spec] of Object.entries(directory.models)) {
    if (key.endsWith(`/${id}`)) return spec;
  }
  // 模糊核心：裸名与全名（含厂商段）两种归一都试——
  // "minimax/m3-preview" 靠全名核心对上 "minimax-m3"，
  // "openai/gpt-4o-2024-11-20" 靠裸名核心对上 "gpt-4o"
  const map = buildCoreIndex(directory);
  return map.get(coreOf(id, true))?.spec ?? map.get(coreOf(id))?.spec ?? null;
}

/** 测试注入口：把索引塞进 memo（生产路径只走 loadModelDirectory） */
export function primeModelDirectoryForTests(directory: ModelDirectory | null) {
  memo = directory;
  coreIndex = null;
}
