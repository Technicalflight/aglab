import { describe, expect, it, vi } from "vitest";
import type { Mock } from "vitest";
import { DEFAULT_DECISION_CONFIG, mergeDecisionConfig } from "../config";
import type { DecisionLayerConfig } from "../config";
import { DecisionCache } from "../cache";
import { DecisionAudit } from "../audit";
import { DecisionRouterImpl } from "../router";
import { DecisionTimeoutError, DecisionUnavailableError } from "../errors";
import type { DecisionAnswer, DecisionProvider, DecisionRequest, ModelTier } from "../types";

/** 带调用记录的假 Provider：置信度、可用性、失败方式都能钉死 */
type FakeProvider = DecisionProvider & { decideMock: Mock };

function fakeProvider(
  name: ModelTier,
  opts: { available?: boolean; confidence?: number; fail?: Error } = {},
): FakeProvider {
  const confidence = opts.confidence ?? 0.95;
  const answers: Record<string, DecisionAnswer> = {
    q1: {
      questionName: "q1",
      type: "noul",
      noul: confidence,
      confidence,
      model: name,
      latencyMs: 1,
    },
  };
  const decideMock: Mock = vi.fn(async () => ({
    answers,
    model: name,
    totalLatencyMs: 2,
    cacheHit: false,
  }));
  if (opts.fail) {
    decideMock.mockRejectedValue(opts.fail);
  }
  return {
    name,
    get isAvailable() {
      return opts.available ?? true;
    },
    decide: decideMock as unknown as DecisionProvider["decide"],
    decideMock,
  };
}

interface RouterOverrides {
  laya?: FakeProvider;
  jev?: FakeProvider;
  cache?: DecisionCache | null;
  config?: Partial<DecisionLayerConfig>;
}

function makeRouter(overrides: RouterOverrides = {}) {
  const laya = overrides.laya ?? fakeProvider("laya");
  const jev = overrides.jev ?? fakeProvider("jev");
  const fallback = { name: "fallback" as const, isAvailable: false, decide: vi.fn() };
  const audit = new DecisionAudit(200);
  const config = mergeDecisionConfig({ ...DEFAULT_DECISION_CONFIG, ...overrides.config });
  const router = new DecisionRouterImpl({
    laya,
    jev,
    fallback,
    cache: overrides.cache !== undefined ? overrides.cache : new DecisionCache(60_000, 100),
    audit,
    config,
  });
  return { router, laya, jev, audit };
}

const publicRequest: DecisionRequest = {
  state: "user message",
  questions: { q1: { type: "noul", instructions: "Is this worth it?" } },
};

describe("DecisionRouter 三级漏斗", () => {
  it("Laya 高置信：直接采信，不惊动 Jev", async () => {
    const { router, laya, jev } = makeRouter();
    const result = await router.decide(publicRequest);
    expect(result.model).toBe("laya");
    expect(result.degraded).toBe(false);
    expect(laya.decideMock).toHaveBeenCalledTimes(1);
    expect(jev.decideMock).not.toHaveBeenCalled();
  });

  it("Laya 低置信升级 Jev：链路两条，结果不打 degraded（V2 按问升级：主体层是 laya，sources 溯源）", async () => {
    const { router, audit } = makeRouter({
      laya: fakeProvider("laya", { confidence: 0.4 }),
      jev: fakeProvider("jev", { confidence: 0.92 }),
    });
    const result = await router.decide(publicRequest);
    // V2 语义：低置信子集送 Jev 补答后合并，model 保留主体层，逐问来源看 sources
    expect(result.model).toBe("laya");
    expect(result.sources?.q1).toBe("jev");
    expect(result.degraded).toBe(false);
    const trace = audit.recent(1)[0];
    expect(trace.modelChain).toEqual(["laya", "jev"]);
  });

  it("全链低置信且 fallback 未接线：交还最好的结果并标 degraded", async () => {
    const { router } = makeRouter({
      laya: fakeProvider("laya", { confidence: 0.6 }),
      jev: fakeProvider("jev", { confidence: 0.55 }),
    });
    const result = await router.decide(publicRequest);
    expect(result.degraded).toBe(true);
    expect(result.model).toBe("laya"); // 0.6 > 0.55，留最好的
    expect(result.cacheHit).toBe(false);
  });

  it("Laya 抛错降级 Jev：错误进审计，主流程拿到 Jev 答案", async () => {
    const { router, jev, audit } = makeRouter({
      laya: fakeProvider("laya", { fail: new DecisionTimeoutError("Laya 请求超时") }),
      jev: fakeProvider("jev", { confidence: 0.9 }),
    });
    const result = await router.decide(publicRequest);
    expect(result.model).toBe("jev");
    expect(audit.recent(1)[0].errors).toContain("laya");
    expect(jev.decideMock).toHaveBeenCalledTimes(1);
  });

  it("缓存命中：Provider 只被问一次，第二条 trace 是空链路", async () => {
    const { router, laya, audit } = makeRouter();
    await router.decide(publicRequest);
    const second = await router.decide(publicRequest);
    expect(second.cacheHit).toBe(true);
    expect(laya.decideMock).toHaveBeenCalledTimes(1);
    expect(audit.recent(1)[0].modelChain).toEqual([]);
    expect(audit.size).toBe(2);
  });

  it("决策层总开关关闭：明确报不可用", async () => {
    const { router } = makeRouter({ config: { enabled: false } });
    await expect(router.decide(publicRequest)).rejects.toBeInstanceOf(DecisionUnavailableError);
  });
});

describe("隐私红线", () => {
  it("confidential 钉死本地：Laya 低置信也不许问 Jev", async () => {
    const { router, jev } = makeRouter({
      laya: fakeProvider("laya", { confidence: 0.3 }),
    });
    const result = await router.decide({ ...publicRequest, sensitivity: "confidential" });
    expect(result.model).toBe("laya");
    expect(result.degraded).toBe(true);
    expect(jev.decideMock).not.toHaveBeenCalled();
  });

  it("confidential 且 Laya 不可用：宁可不决策，也不送云端", async () => {
    const { router, jev } = makeRouter({
      laya: fakeProvider("laya", { available: false }),
    });
    await expect(
      router.decide({ ...publicRequest, sensitivity: "confidential" }),
    ).rejects.toBeInstanceOf(DecisionUnavailableError);
    expect(jev.decideMock).not.toHaveBeenCalled();
  });

  it("modelPreference=laya 失败同样拒绝静默降级", async () => {
    const { router, jev } = makeRouter({
      laya: fakeProvider("laya", { fail: new DecisionUnavailableError("sidecar 没起") }),
    });
    await expect(
      router.decide({ ...publicRequest, modelPreference: "laya" }),
    ).rejects.toBeInstanceOf(DecisionUnavailableError);
    expect(jev.decideMock).not.toHaveBeenCalled();
  });

  it("modelPreference=jev 失败允许降到 Laya（质量意图可降级，隐私意图不可）", async () => {
    const { router, laya } = makeRouter({
      jev: fakeProvider("jev", { fail: new DecisionUnavailableError("Jev HTTP 401") }),
    });
    const result = await router.decide({ ...publicRequest, modelPreference: "jev" });
    expect(result.model).toBe("laya");
    expect(laya.decideMock).toHaveBeenCalledTimes(1);
  });

  it("sensitivity 参与缓存键：confidential 不会命中 public 算过的缓存", async () => {
    const { router, laya } = makeRouter();
    await router.decide(publicRequest);
    await router.decide({ ...publicRequest, sensitivity: "confidential" });
    expect(laya.decideMock).toHaveBeenCalledTimes(2);
  });

  it("审计脱敏：confidential 只剩长度+哈希，private 截断预览，public 原样", async () => {
    const { router, audit } = makeRouter();
    const secret = "api_key=sk-very-secret-value-should-not-appear";
    const longSecret = secret.repeat(15); // 超过 200 字符预览上限
    await router.decide({ ...publicRequest, state: secret, sensitivity: "confidential" });
    await router.decide({ ...publicRequest, state: longSecret, sensitivity: "private" });
    await router.decide({ ...publicRequest, state: secret, sensitivity: "public" });
    // recent() 新的在前：public、private、confidential
    const [publicTrace, privateTrace, confidentialTrace] = audit.recent(3);
    expect(publicTrace.request.state).toBe(secret);
    expect(privateTrace.request.state).toBe(longSecret.slice(0, 200) + "…");
    const confState = String(confidentialTrace.request.state);
    expect(confState).not.toContain("sk-very-secret");
    expect(confState).toMatch(/^<confidential: \d+ chars, hash=[0-9a-f]{8}>$/);
  });

  it("调用点钉：请求带的 conversationId 必须原样活进审计 trace（含 confidential 脱敏后）——决策面板按它过滤话题，丢了它面板就静默变成空流", async () => {
    const { router, audit } = makeRouter();
    await router.decide({ ...publicRequest, conversationId: "conv-1", sensitivity: "public" });
    await router.decide({
      ...publicRequest,
      conversationId: "conv-2",
      sensitivity: "confidential",
    });
    const [confidentialTrace, publicTrace] = audit.recent(2);
    expect(publicTrace.request.conversationId).toBe("conv-1");
    expect(confidentialTrace.request.conversationId).toBe("conv-2");
  });
});

/**
 * 面板的"这一格为什么是空的"全靠这几条 trace 回答：一次失败的决策如果什么都不留，
 * 决策面板看到的就和"从没调用过"一模一样——而 sidecar 没起是这里最常见的那一种失败。
 */
describe("失败也要留痕（response=null）", () => {
  it("全层都不可用：抛错之前先落一条没有答案的 trace", async () => {
    const { router, audit } = makeRouter({
      laya: fakeProvider("laya", { available: false }),
      jev: fakeProvider("jev", { available: false }),
    });
    await expect(router.decide(publicRequest)).rejects.toBeInstanceOf(DecisionUnavailableError);
    expect(audit.size).toBe(1);
    const trace = audit.recent(1)[0];
    expect(trace.response).toBeNull();
    expect(trace.modelChain).toEqual([]); // 一层都没真问过
    expect(trace.errors).toContain("laya:unavailable");
    expect(trace.errors).toContain("jev:unavailable");
    expect(trace.cacheHit).toBe(false);
  });

  it("红线拦下的失败：trace 里出现过本地层，绝不出现云端层", async () => {
    const { router, audit } = makeRouter({
      laya: fakeProvider("laya", { fail: new DecisionUnavailableError("sidecar 没起") }),
    });
    await expect(
      router.decide({ ...publicRequest, sensitivity: "confidential" }),
    ).rejects.toBeInstanceOf(DecisionUnavailableError);
    const trace = audit.recent(1)[0];
    expect(trace.response).toBeNull();
    expect(trace.modelChain).toEqual(["laya"]);
    expect(trace.errors).toContain("sidecar 没起");
    expect(trace.errors).not.toContain("jev");
  });

  it("总开关关着时不记：那一刻决策层不存在，谈不上一次决策", async () => {
    const { router, audit } = makeRouter({ config: { enabled: false } });
    await expect(router.decide(publicRequest)).rejects.toBeInstanceOf(DecisionUnavailableError);
    expect(audit.size).toBe(0);
  });
});

/**
 * 逐层过程。只有 modelChain 的话，"laya 到底起没起、jev 为什么没被问"这两个问题
 * 在审计里根本没有答案——而它们正是决策面板最常见的用途。
 */
describe("attempts：漏斗里每一层的下场", () => {
  it("Laya 高置信收工：另外两层记为「没轮到」，而不是从读数里消失", async () => {
    const { router, audit } = makeRouter();
    await router.decide(publicRequest);
    expect(audit.recent(1)[0].attempts).toEqual([
      { tier: "laya", outcome: "answered", latencyMs: expect.any(Number), minConfidence: 0.95 },
      { tier: "jev", outcome: "not-reached", latencyMs: 0 },
      { tier: "fallback", outcome: "not-reached", latencyMs: 0 },
    ]);
  });

  it("Laya 不可用、Jev 答上：两条下场各说各的话，fallback 才是没轮到", async () => {
    const { router, audit } = makeRouter({
      laya: fakeProvider("laya", { available: false }),
      jev: fakeProvider("jev", { confidence: 0.9 }),
    });
    await router.decide(publicRequest);
    const [laya, jev, fallback] = audit.recent(1)[0].attempts;
    expect(laya.outcome).toBe("unavailable");
    expect(laya.reason).toContain("不可用");
    expect(jev.outcome).toBe("answered");
    expect(fallback.outcome).toBe("not-reached");
  });

  it("private 请求：云端两层是「没进候选」并写着挡它的那条规矩（反向钉：它们一次都没被问）", async () => {
    const { router, audit, jev } = makeRouter();
    await router.decide({ ...publicRequest, sensitivity: "private" });
    const trace = audit.recent(1)[0];
    expect(trace.modelChain).toEqual(["laya"]);
    expect(jev.decideMock).not.toHaveBeenCalled();
    const excluded = trace.attempts.filter((attempt) => attempt.outcome === "excluded");
    expect(excluded.map((attempt) => attempt.tier)).toEqual(["jev", "fallback"]);
    expect(excluded[0].reason).toContain("钉在本地");
    expect(excluded[1].reason).toContain("红线");
  });

  it("升级链上限 1：被上限砍掉的层不冒充「没轮到」", async () => {
    const { router, audit } = makeRouter({
      laya: fakeProvider("laya", { confidence: 0.4 }),
      config: {
        routing: { autoUpgradeThreshold: 0.85, maxUpgradeChain: 1, sensitiveForceLocal: true },
      },
    });
    await router.decide(publicRequest);
    const attempts = audit.recent(1)[0].attempts;
    expect(attempts.find((attempt) => attempt.tier === "laya")?.outcome).toBe("below-threshold");
    expect(attempts.find((attempt) => attempt.tier === "jev")?.reason).toContain("上限 1 层");
    expect(attempts.find((attempt) => attempt.tier === "jev")?.outcome).toBe("excluded");
  });

  it("缓存命中：attempts 是空集——一层都没问，就不该有逐层过程", async () => {
    const { router, audit } = makeRouter();
    await router.decide(publicRequest);
    await router.decide(publicRequest);
    const hit = audit.recent(1)[0];
    expect(hit.cacheHit).toBe(true);
    expect(hit.attempts).toEqual([]);
  });

  it("Laya 抛错后 Jev 答上：失败那一层留着错误原文", async () => {
    const { router, audit } = makeRouter({
      laya: fakeProvider("laya", { fail: new DecisionTimeoutError("laya 5000ms 超时") }),
      jev: fakeProvider("jev", { confidence: 0.9 }),
    });
    await router.decide(publicRequest);
    const laya = audit.recent(1)[0].attempts.find((attempt) => attempt.tier === "laya");
    expect(laya?.outcome).toBe("failed");
    expect(laya?.reason).toContain("laya 5000ms 超时");
  });
});
