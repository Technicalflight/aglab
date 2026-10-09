/**
 * 决策审计。原则「决策可观测」和「敏感数据不出本地」在这里交汇：
 * 审计如果原样落 state，决策面板就变成了敏感数据的第二份拷贝——
 * 所以脱敏做在写入端而不是读取端：confidential 只剩长度+哈希，private 截断预览。
 */
import type { DecisionRequest, DecisionTrace } from "./types";
import { fnv1a, stableStringify } from "./cache";

export class DecisionAudit {
  private records: DecisionTrace[] = [];
  private readonly listeners = new Set<(trace: DecisionTrace) => void>();

  constructor(private readonly maxEntries: number) {}

  record(trace: DecisionTrace): void {
    this.records.push(trace);
    if (this.records.length > this.maxEntries) {
      this.records.splice(0, this.records.length - this.maxEntries);
    }
    for (const listener of this.listeners) {
      try {
        listener(trace);
      } catch {
        // 订阅者（Phase 4 的决策面板）坏了不能拖垮决策路径本身
      }
    }
  }

  /** 最近 n 条，新的在前。Phase 4 决策面板直接吃这个 */
  recent(n: number): DecisionTrace[] {
    return this.records.slice(-n).reverse();
  }

  /** 返回退订函数。UI 组件卸载时调用，别留悬空监听 */
  subscribe(listener: (trace: DecisionTrace) => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  clear(): void {
    this.records = [];
  }

  get size(): number {
    return this.records.length;
  }
}

/**
 * 按 sensitivity 脱敏请求。public 原样；private 只留预览（对象先归一序列化再截断）；
 * confidential 连预览都不留——长度加哈希足够把两条 trace 关联起来做回放，
 * 但不足以还原内容。
 */
export function redactRequest(
  request: DecisionRequest,
  privatePreviewChars: number,
): DecisionRequest {
  const sensitivity = request.sensitivity ?? "public";
  if (sensitivity === "confidential") {
    const raw = typeof request.state === "string" ? request.state : stableStringify(request.state);
    return { ...request, state: `<confidential: ${raw.length} chars, hash=${fnv1a(raw)}>` };
  }
  if (sensitivity === "private") {
    const raw = typeof request.state === "string" ? request.state : stableStringify(request.state);
    const preview =
      raw.length > privatePreviewChars ? `${raw.slice(0, privatePreviewChars)}…` : raw;
    return { ...request, state: preview };
  }
  return request;
}
