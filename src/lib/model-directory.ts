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

/**
 * 查规格：精确小写匹配 → 斜杠两侧互为后缀（端点给 "claude-haiku-4-5"、
 * 目录收 "anthropic/claude-haiku-4-5"，或反过来）。都查不到返回 null
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
    const bare = id.slice(slash + 1);
    const hit = directory.models[bare];
    if (hit) return hit;
  }
  // 目录键带命名空间而用户 id 不带：扫一遍后缀（3938 条的 Map 扫描一次可忽略）
  for (const [key, spec] of Object.entries(directory.models)) {
    if (key.endsWith(`/${id}`)) return spec;
  }
  return null;
}

/** 测试注入口：把索引塞进 memo（生产路径只走 loadModelDirectory） */
export function primeModelDirectoryForTests(directory: ModelDirectory | null) {
  memo = directory;
}
