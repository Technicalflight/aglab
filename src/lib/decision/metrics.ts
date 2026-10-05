/**
 * 决策层指标计数器（design-decision-layer-optimization.md §9 新增三指标的落地）。
 *
 * 审计是内存环形缓冲，重启就丢——而 rewrite 频率、兜底触发率、unanswered keep
 * 这类指标的价值恰恰在**跨话题累计**（「Provider 最近是不是变差了」不是一次话题能回答的）。
 * 所以计数器持久化到 localStorage：写微秒级（几百字节 JSON），读随取。
 * 纯 Node 宿主（测试）没有 localStorage——计数退化为内存，测试照样能钉行为。
 *
 * 计数名是平的字符串键（`路径.事件`），面板按前缀分组；不加中间层抽象——
 * 这个模块的全部职责就是「+1 并记住」。
 */

const STORAGE_KEY = "aglab.decisionLayer.metrics";
const MAX_COUNTERS = 200;

let counters: Record<string, number> | null = null;

function load(): Record<string, number> {
  if (counters) return counters;
  counters = {};
  try {
    const raw = globalThis.localStorage?.getItem(STORAGE_KEY);
    if (raw) {
      const parsed: unknown = JSON.parse(raw);
      if (typeof parsed === "object" && parsed !== null) {
        for (const [key, value] of Object.entries(parsed as Record<string, unknown>)) {
          if (typeof value === "number" && Number.isFinite(value)) counters[key] = value;
        }
      }
    }
  } catch {
    // 存储坏/不可用：从零开始计，不炸调用方
  }
  return counters;
}

function persist(): void {
  try {
    globalThis.localStorage?.setItem(STORAGE_KEY, JSON.stringify(counters));
  } catch {
    // 写不进（隐私模式/配额）：计数继续活在内存里
  }
}

/** 累加一个计数。by 支持非整数（加权场景），但持久化值保持有限数 */
export function incrementMetric(name: string, by = 1): void {
  const current = load();
  current[name] = (current[name] ?? 0) + by;
  // 超上限时丢最老的键（计数器不是审计——丢一个冷键比无限膨胀好）
  const keys = Object.keys(current);
  if (keys.length > MAX_COUNTERS) {
    for (const key of keys.slice(0, keys.length - MAX_COUNTERS)) delete current[key];
  }
  persist();
}

/** 面板读数：一份浅拷贝（调用方改它不影响计数器） */
export function getMetrics(): Record<string, number> {
  return { ...load() };
}

/** 按前缀聚合：`compaction.*` 之类的前缀分组读数 */
export function metricsWithPrefix(prefix: string): Record<string, number> {
  const out: Record<string, number> = {};
  for (const [key, value] of Object.entries(load())) {
    if (key.startsWith(prefix)) out[key] = value;
  }
  return out;
}

/** 用户清空（设置页的「重置统计」动作） */
export function resetMetrics(): void {
  counters = {};
  persist();
}
