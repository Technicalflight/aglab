/**
 * 降级链的行为钉（design-decision-layer-optimization.md §5.4 / §3 不变量）：
 * - 402/429/5xx/超时/网络 → 降级下一跳；4xx → fail-fast
 * - 全链失败 → 聚合错误（带已尝试列表）
 * - 服务商亲和每跳独立；keyring 喂到每一跳
 */
import { describe, expect, it, vi } from "vitest";
import {
  JevChainProvider,
  buildJevChainHops,
  shouldFallbackToNext,
  type JevChainBuildOptions,
} from "../providers/jev-chain";
import { JevProvider } from "../providers/jev";
import { DecisionTimeoutError, DecisionUnavailableError } from "../errors";
import type { DecisionProvider, DecisionRequest, DecisionResponse } from "../types";

function fakeProvider(
  _name: string,
  impl: (request: DecisionRequest) => Promise<DecisionResponse>,
  available = true,
): DecisionProvider {
  return { name: "jev", isAvailable: available, decide: impl } as unknown as DecisionProvider;
}

const REQUEST: DecisionRequest = {
  state: "s",
  questions: { q: { type: "noul", instructions: "?" } },
};

function answer(): DecisionResponse {
  return {
    answers: {
      q: {
        questionName: "q",
        type: "noul",
        noul: 0.9,
        confidence: 0.9,
        model: "jev",
        latencyMs: 1,
      },
    },
    model: "jev",
    totalLatencyMs: 1,
    cacheHit: false,
  };
}

describe("shouldFallbackToNext（B7 的降级判定矩阵）", () => {
  it("402 / 429 / 5xx → 降级", () => {
    expect(shouldFallbackToNext(new DecisionUnavailableError("x", { status: 402 }))).toBe(true);
    expect(shouldFallbackToNext(new DecisionUnavailableError("x", { status: 429 }))).toBe(true);
    expect(shouldFallbackToNext(new DecisionUnavailableError("x", { status: 500 }))).toBe(true);
    expect(shouldFallbackToNext(new DecisionUnavailableError("x", { status: 599 }))).toBe(true);
  });
  it("超时 / 无状态码的网络错误 → 降级（草图漏掉的两类）", () => {
    expect(shouldFallbackToNext(new DecisionTimeoutError("t"))).toBe(true);
    expect(shouldFallbackToNext(new DecisionUnavailableError("socket hung up"))).toBe(true);
  });
  it("其余 4xx → fail-fast（401/403/404）", () => {
    expect(
      shouldFallbackToNext(new DecisionUnavailableError("Jev HTTP 401", { status: 401 })),
    ).toBe(false);
    expect(
      shouldFallbackToNext(new DecisionUnavailableError("Jev HTTP 403", { status: 403 })),
    ).toBe(false);
    expect(
      shouldFallbackToNext(new DecisionUnavailableError("Jev HTTP 404", { status: 404 })),
    ).toBe(false);
  });
  it("意外错误形态保守降级", () => {
    expect(shouldFallbackToNext(new TypeError("boom"))).toBe(true);
  });
});

describe("JevChainProvider", () => {
  const baseOptions: JevChainBuildOptions = {
    apiKey: "",
    apiKeys: { typesafe: "sk-ts", openrouter: "sk-or" },
    baseUrl: "",
    endpoints: [],
    transport: "direct",
    useKeyring: false,
    timeoutMs: 5_000,
  };

  it("按配置顺序建跳，白名单外成员丢弃，custom 合法", () => {
    const hops = buildJevChainHops(["custom", "typesafe", "vercel", "custom"], baseOptions);
    expect(hops.map((h) => h.via)).toEqual(["custom", "typesafe"]);
  });

  it("402 降级到下一跳并返回其答案", async () => {
    const first = new JevProvider({
      apiKey: "k",
      via: "typesafe",
      transport: "direct",
      timeoutMs: 100,
      fetchImpl: async () => {
        throw new DecisionUnavailableError("Jev HTTP 402", { status: 402 });
      },
    });
    const chain = new JevChainProvider([
      { via: "typesafe", provider: first },
      { via: "or", provider: fakeProvider("or", async () => answer()) },
    ]);
    const result = await chain.decide(REQUEST);
    expect(result.answers.q.confidence).toBe(0.9);
  });

  it("4xx fail-fast：不试下一跳，聚合错误带已尝试列表", async () => {
    const next = vi.fn(async () => answer());
    const first = new JevProvider({
      apiKey: "k",
      via: "typesafe",
      transport: "direct",
      timeoutMs: 100,
      fetchImpl: async () => {
        throw new DecisionUnavailableError("Jev HTTP 401", { status: 401 });
      },
    });
    const chain = new JevChainProvider([
      { via: "typesafe", provider: first },
      { via: "or", provider: fakeProvider("or", next) },
    ]);
    await expect(chain.decide(REQUEST)).rejects.toThrow(/已尝试：typesafe/);
    expect(next).not.toHaveBeenCalled();
  });

  it("超时降级；全链失败抛聚合错误（每跳失败都在列）", async () => {
    const timingOut = new JevProvider({
      apiKey: "k",
      via: "typesafe",
      transport: "direct",
      timeoutMs: 20,
      fetchImpl: (_url, init) =>
        new Promise((_resolve, reject) => {
          const signal = (init as { signal?: AbortSignal }).signal;
          signal?.addEventListener("abort", () =>
            reject(new DOMException("aborted", "TimeoutError")),
          );
        }),
    });
    const chain = new JevChainProvider([
      { via: "typesafe", provider: timingOut },
      {
        via: "or",
        provider: fakeProvider("or", async () => {
          throw new DecisionUnavailableError("network down");
        }),
      },
    ]);
    await expect(chain.decide(REQUEST)).rejects.toThrow(/typesafe.*or/s);
  });

  it("没有跳有凭据 → 不可用，而不是逐跳撞墙", async () => {
    const hops = buildJevChainHops(["typesafe"], { ...baseOptions, apiKeys: {} });
    const chain = new JevChainProvider(hops);
    expect(chain.isAvailable).toBe(false);
    await expect(chain.decide(REQUEST)).rejects.toThrow(DecisionUnavailableError);
  });

  it("keyring 状态喂到每一跳", () => {
    const hops = buildJevChainHops(["typesafe", "openrouter"], baseOptions);
    const chain = new JevChainProvider(hops);
    expect(chain.isAvailable).toBe(true); // apiKeys 已给了钥匙——反向验证：清空后喂 keyring
    const bare = new JevChainProvider(
      buildJevChainHops(["typesafe"], { ...baseOptions, apiKeys: {}, useKeyring: true }),
    );
    expect(bare.isAvailable).toBe(false);
    bare.setKeyringAvailable(true);
    expect(bare.isAvailable).toBe(true);
  });
});
