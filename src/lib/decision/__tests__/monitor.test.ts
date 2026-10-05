import { describe, expect, it, vi } from "vitest";
import { assembleDecisionSystem } from "../index";
import { DEFAULT_DECISION_CONFIG, mergeDecisionConfig } from "../config";
import { DecisionMonitor } from "../monitor";
import type { DecisionSystem } from "../index";
import type { DecisionTrace } from "../types";

/** 真装配一份系统（不预热、不碰云端），测的就是面板实际会挂上的那种对象 */
function makeSystem(maxEntries = 200): DecisionSystem {
  return assembleDecisionSystem(
    mergeDecisionConfig({
      ...DEFAULT_DECISION_CONFIG,
      laya: { ...DEFAULT_DECISION_CONFIG.laya, warmupOnStart: false },
      audit: { ...DEFAULT_DECISION_CONFIG.audit, maxEntries },
    }),
  );
}

let seq = 0;
function trace(): DecisionTrace {
  seq += 1;
  return {
    id: `m${seq}`,
    timestamp: "2026-09-27T00:00:00.000Z",
    request: { state: "s", questions: { q1: { type: "noul", instructions: "?" } } },
    response: null,
    modelChain: [],
    // 这一格要的是"缓存命中/什么都没问"那种形状：没问过任何一层，就没有逐层过程
    attempts: [],
    totalLatencyMs: 1,
    cacheHit: false,
  };
}

describe("DecisionMonitor", () => {
  it("挂上时先把审计里的历史补进来，新的在前", () => {
    const system = makeSystem();
    const first = trace();
    const second = trace();
    system.audit!.record(first);
    system.audit!.record(second);

    const monitor = new DecisionMonitor(50);
    monitor.attach(system);
    expect(monitor.snapshot.map((item) => item.id)).toEqual([second.id, first.id]);
  });

  it("增量实时进来，订阅者被通知", () => {
    const system = makeSystem();
    const monitor = new DecisionMonitor(50);
    const notified = vi.fn();
    monitor.attach(system);
    monitor.subscribe(notified);

    system.audit!.record(trace());
    expect(monitor.snapshot).toHaveLength(1);
    expect(notified).toHaveBeenCalledTimes(1);
  });

  it("超出上限时挤掉最老的，不是新的", () => {
    const system = makeSystem();
    const monitor = new DecisionMonitor(3);
    monitor.attach(system);
    const ids: string[] = [];
    for (let i = 0; i < 5; i++) {
      const item = trace();
      ids.push(item.id);
      system.audit!.record(item);
    }
    expect(monitor.snapshot.map((item) => item.id)).toEqual([ids[4], ids[3], ids[2]]);
  });

  it("设置页改配置重装系统：历史留着，也不重复种进来", () => {
    const first = makeSystem();
    const monitor = new DecisionMonitor(50);
    monitor.attach(first);
    const shared = trace();
    first.audit!.record(shared);
    expect(monitor.snapshot).toHaveLength(1);

    // 新系统是新的审计实例（空的），旧订阅随之作废
    const second = makeSystem();
    monitor.attach(second);
    expect(monitor.snapshot.map((item) => item.id)).toEqual([shared.id]);

    second.audit!.record(trace());
    expect(monitor.snapshot).toHaveLength(2);

    // 再挂一次同一个系统：不该把已经收过的两条又种一遍
    monitor.attach(second);
    expect(monitor.snapshot).toHaveLength(2);
  });

  it("退订之后不再收增量", () => {
    const system = makeSystem();
    const monitor = new DecisionMonitor(50);
    const detach = monitor.attach(system);
    detach();
    system.audit!.record(trace());
    expect(monitor.snapshot).toHaveLength(0);
    // 退订之后重新挂同一个系统：补种子会把它自己那条收回来，但不会重复
    monitor.attach(system);
    expect(monitor.snapshot).toHaveLength(1);
  });

  it("清空连着审计一起清（否则重挂时 recent() 又把同一批种回来）", () => {
    const system = makeSystem();
    const monitor = new DecisionMonitor(50);
    monitor.attach(system);
    system.audit!.record(trace());
    expect(system.audit!.size).toBe(1);

    monitor.clear();
    expect(monitor.snapshot).toHaveLength(0);
    expect(system.audit!.size).toBe(0);
  });

  it("快照引用在没有新数据时保持稳定（useSyncExternalStore 的前提）", () => {
    const system = makeSystem();
    const monitor = new DecisionMonitor(50);
    monitor.attach(system);
    const before = monitor.snapshot;
    monitor.subscribe(() => undefined);
    expect(monitor.snapshot).toBe(before);

    system.audit!.record(trace());
    expect(monitor.snapshot).not.toBe(before);
  });

  it("订阅者自己抛错，不影响别的订阅者收数", () => {
    const system = makeSystem();
    const monitor = new DecisionMonitor(50);
    monitor.attach(system);
    monitor.subscribe(() => {
      throw new Error("面板炸了");
    });
    const also = vi.fn();
    monitor.subscribe(also);
    expect(() => system.audit!.record(trace())).not.toThrow();
    expect(also).toHaveBeenCalledTimes(1);
  });
});
