/**
 * 决策缓存。决策的输入是（state, questions）的纯函数，同一输入在同一模型上
 * 结果稳定——这类毫秒级判断被高频重复调用，缓存是漏斗前面最便宜的一层。
 *
 * 键的稳定性是这里的命门：调用方随手写的对象字面量键序不该决定缓存命中与否，
 * 所以哈希前先做键序归一（递归排序），再过 FNV-1a。
 * sensitivity 参与键：confidential 的请求永远不命中一条由云端算出来的缓存，
 * 哪怕 state 和问题一字不差——隐私边界不做「内容相同就网开一面」的例外。
 */

export interface DecisionCacheKeyParts {
  state: string | Record<string, unknown>;
  questions: Record<string, unknown>;
  sensitivity?: string;
}

export class DecisionCache {
  private readonly entries = new Map<string, { response: unknown; expiresAt: number }>();

  constructor(
    private readonly ttlMs: number,
    private readonly maxEntries: number,
  ) {}

  /** 稳定键：键序无关、undefined 值剔除、带输入长度（32 位 FNV 碰撞时长度是不花钱的第二道筛） */
  getKey(request: DecisionCacheKeyParts): string {
    const canonical = `${stableStringify(request.state)}\u0000${stableStringify(request.questions)}`;
    const sensitivity = request.sensitivity ?? "public";
    return `dl1:${sensitivity}:${fnv1a(canonical)}-${canonical.length}`;
  }

  get<T>(request: DecisionCacheKeyParts): T | null {
    const key = this.getKey(request);
    const entry = this.entries.get(key);
    if (!entry) return null;
    if (entry.expiresAt <= Date.now()) {
      this.entries.delete(key);
      return null;
    }
    // LRU 靠 Map 的插入序：读一次就搬到队尾，最老的永远在队头等着被赶
    this.entries.delete(key);
    this.entries.set(key, entry);
    return entry.response as T;
  }

  set(request: DecisionCacheKeyParts, response: unknown): void {
    const key = this.getKey(request);
    if (this.entries.has(key)) this.entries.delete(key);
    this.entries.set(key, { response, expiresAt: Date.now() + this.ttlMs });
    while (this.entries.size > this.maxEntries) {
      const oldest = this.entries.keys().next().value;
      if (oldest === undefined) break;
      this.entries.delete(oldest);
    }
  }

  clear(): void {
    this.entries.clear();
  }

  get size(): number {
    return this.entries.size;
  }
}

/**
 * 递归键序归一序列化。手写而不用 JSON.stringify(replacer) 的原因：
 * replacer 拿到的键序仍是原对象的插入序，归一不了。
 */
export function stableStringify(value: unknown): string {
  if (value === null || typeof value !== "object") {
    return JSON.stringify(value) ?? "undefined";
  }
  if (Array.isArray(value)) {
    return `[${value.map(stableStringify).join(",")}]`;
  }
  const entries = Object.entries(value as Record<string, unknown>)
    .filter(([, v]) => v !== undefined)
    .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
  return `{${entries.map(([k, v]) => `${JSON.stringify(k)}:${stableStringify(v)}`).join(",")}}`;
}

/** FNV-1a 32 位。缓存键不需要密码学强度，需要的是快和稳定 */
export function fnv1a(input: string): string {
  let hash = 0x811c9dc5;
  for (let i = 0; i < input.length; i++) {
    hash ^= input.charCodeAt(i);
    hash = Math.imul(hash, 0x01000193);
  }
  return (hash >>> 0).toString(16).padStart(8, "0");
}
