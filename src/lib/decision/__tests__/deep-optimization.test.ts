/**
 * 深度优化三项的行为钉（设计文档之外的增量）：
 * - in-flight 去重：并发同键请求共享一次漏斗执行，Provider 只被问一次
 * - Jev 断路器：连续失败 N 次 → 冷却跳过（不再付完整超时），成功即闭合
 * - 指标计数器：+1 并可读；无 localStorage 的宿主退化为内存不炸
 */
import { describe, expect, it, vi } from "vitest";
import { DecisionRouterImpl } from "../router";
import { DEFAULT_DECISION_CONFIG, mergeDecisionConfig } from "../config";
import { DecisionCache } from "../cache";
import { JevChainProvider } from "../providers/jev-chain";
import { DecisionUnavailableError } from "../errors";
import { getMetrics, incrementMetric, metricsWithPrefix, resetMetrics } from "../metrics";
import type { DecisionProvider, DecisionRequest, DecisionResponse } from "../types";

const REQUEST: DecisionRequest = {
  state: "shared state",
  questions: { q: { type: "noul", instructions: "?" } },
};

function slowProvider(delayMs: number, confidence = 0.95): DecisionProvider & { decideMock: ReturnType<typeof vi.fn> } {
  const decideMock = vi.fn(
    async (): Promise<DecisionResponse> => {
      await new Promise((resolve) => setTimeout(resolve, delayMs));
      return {
        answers: { q: { questionName: "q", type: "noul", noul: confidence, confidence, model: "laya", latencyMs: 1 } },
        model: "laya",
        totalLatencyMs: delayMs,
        cacheHit: false,
      };
    },
  );
  return { name: "laya", isAvailable: true, decide: decideMock, decideMock } as unknown as DecisionProvider & { decideMock: ReturnType<typeof vi.fn> };
}

describe("in-flight 去重", () => {
  it("并发同键请求只触发一次 Provider 调用，都拿到同一答案", async () => {
    resetMetrics();
    const laya = slowProvider(30);
    const router = new DecisionRouterImpl({
      laya,
      jev: null,
      fallback: { name: "fallback", isAvailable: false, decide: vi.fn() },
      cache: new DecisionCache(60_000, 100),
      audit: null,
      config: mergeDecisionConfig(DEFAULT_DECISION_CONFIG),
    });
    const [a, b, c] = await Promise.all([
      router.decide(REQUEST),
      router.decide(REQUEST),
      router.decide(REQUEST),
    ]);
    expect(laya.decideMock).toHaveBeenCalledTimes(1);
    expect(a.answers.q.confidence).toBe(0.95);
    expect(b.cacheHit).toBe(true);
    expect(c.cacheHit).toBe(true);
    expect(getMetrics()["router.coalesced"]).toBe(2);
  });

  it("不同键互不干扰，各自执行", async () => {
    const laya = slowProvider(10);
    const router = new DecisionRouterImpl({
      laya,
      jev: null,
      fallback: { name: "fallback", isAvailable: false, decide: vi.fn() },
      cache: new DecisionCache(60_000, 100),
      audit: null,
      config: mergeDecisionConfig(DEFAULT_DECISION_CONFIG),
    });
    await Promise.all([
      router.decide(REQUEST),
      router.decide({ ...REQUEST, state: "different" }),
    ]);
    expect(laya.decideMock).toHaveBeenCalledTimes(2);
  });
});

describe("Jev 断路器", () => {
  function failingChain(fail: () => Promise<DecisionResponse>): JevChainProvider {
    const alwaysFail: DecisionProvider = {
      name: "jev",
      isAvailable: true,
      decide: vi.fn(fail),
    };
    return new JevChainProvider([{ via: "hop", provider: alwaysFail }]);
  }

  it("连续失败达阈值 → 冷却期跳过该跳（Provider 不再被调用）", async () => {
    const provider = failingChain(async () => {
      throw new DecisionUnavailableError("timeout again");
    });
    // 前 3 次：真的打 provider（失败）
    for (let i = 0; i < 3; i++) {
      await expect(provider.decide(REQUEST)).rejects.toThrow();
    }
    const decide = (provider as unknown as { hops: Array<{ provider: DecisionProvider & { decide: ReturnType<typeof vi.fn> } }> }).hops[0].provider.decide;
    expect(decide).toHaveBeenCalledTimes(3);
    // 第 4 次：断路器开着，直接略过（调用数不变，错误消息带冷却标记）
    await expect(provider.decide(REQUEST)).rejects.toThrow(/熔断冷却中/);
    expect(decide).toHaveBeenCalledTimes(3);
  });

  it("冷却结束后半开放行；成功即闭合", async () => {
    vi.useFakeTimers();
    try {
      let attempts = 0;
      let healthy = false;
      const provider = failingChain(async () => {
        attempts += 1;
        if (!healthy) throw new DecisionUnavailableError("down");
        return {
          answers: { q: { questionName: "q", type: "noul", noul: 0.9, confidence: 0.9, model: "jev", latencyMs: 1 } },
          model: "jev",
          totalLatencyMs: 1,
          cacheHit: false,
        };
      });
      for (let i = 0; i < 3; i++) await expect(provider.decide(REQUEST)).rejects.toThrow();
      // 冷却期内跳过
      vi.advanceTimersByTime(10_000);
      await expect(provider.decide(REQUEST)).rejects.toThrow(/熔断冷却中/);
      expect(attempts).toBe(3);
      // 冷却结束：半开放行一次
      vi.advanceTimersByTime(21_000);
      healthy = true;
      const result = await provider.decide(REQUEST);
      expect(result.answers.q.confidence).toBe(0.9);
      expect(attempts).toBe(4);
      // 闭合后再打真 provider
      await provider.decide(REQUEST);
      expect(attempts).toBe(5);
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("指标计数器", () => {
  it("累加、按前缀读、重置", () => {
    resetMetrics();
    incrementMetric("compaction.applied");
    incrementMetric("compaction.applied");
    incrementMetric("compaction.rewrite", 0.5);
    expect(getMetrics()["compaction.applied"]).toBe(2);
    const prefixed = metricsWithPrefix("compaction.");
    expect(prefixed["compaction.applied"]).toBe(2);
    expect(prefixed["compaction.rewrite"]).toBe(0.5);
    expect(Object.keys(metricsWithPrefix("retrieval."))).toHaveLength(0);
    resetMetrics();
    expect(getMetrics()["compaction.applied"]).toBeUndefined();
  });
});
