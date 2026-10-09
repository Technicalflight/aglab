/**
 * 漏斗 V2 的行为钉（design-decision-layer-optimization.md §4 修 A3）：
 * 升级单位从「整批」改为「问题」——只有低置信子问题送 Jev，其余保留；
 * sources 逐问溯源；敏感钉本地不升级；升级失败不阻塞原漏斗。
 */
import { describe, expect, it, vi } from "vitest";
import type { Mock } from "vitest";
import { DEFAULT_DECISION_CONFIG, mergeDecisionConfig } from "../config";
import { DecisionCache } from "../cache";
import { DecisionAudit } from "../audit";
import { DecisionRouterImpl } from "../router";
import { DecisionUnavailableError } from "../errors";
import type {
  DecisionAnswer,
  DecisionProvider,
  DecisionRequest,
  DecisionResponse,
  ModelTier,
} from "../types";

/** 逐问置信度的假 Provider：answers[name].confidence = confidences[name] ?? fallback */
function scriptedProvider(
  name: ModelTier,
  confidences: Record<string, number>,
  opts: { available?: boolean; fail?: Error } = {},
): DecisionProvider & { decideMock: Mock } {
  const decideMock: Mock = vi.fn(async (request: DecisionRequest) => {
    if (opts.fail) throw opts.fail;
    const answers: Record<string, DecisionAnswer> = {};
    for (const questionName of Object.keys(request.questions)) {
      const confidence = confidences[questionName] ?? 0.5;
      answers[questionName] = {
        questionName,
        type: "noul",
        noul: confidence,
        confidence,
        model: name,
        latencyMs: 1,
      };
    }
    const response: DecisionResponse = { answers, model: name, totalLatencyMs: 2, cacheHit: false };
    return response;
  });
  return {
    name,
    get isAvailable() {
      return opts.available ?? true;
    },
    decide: decideMock as unknown as DecisionProvider["decide"],
    decideMock,
  };
}

function questionsOf(count: number): DecisionRequest["questions"] {
  return Object.fromEntries(
    Array.from({ length: count }, (_, i) => [
      `q${i + 1}`,
      { type: "noul" as const, instructions: "?" },
    ]),
  );
}

function makeRouter(
  laya: DecisionProvider,
  jev: DecisionProvider,
  configOverrides: Record<string, unknown> = {},
) {
  const audit = new DecisionAudit(200);
  const config = mergeDecisionConfig({ ...DEFAULT_DECISION_CONFIG, ...configOverrides });
  const router = new DecisionRouterImpl({
    laya,
    jev,
    fallback: { name: "fallback", isAvailable: false, decide: vi.fn() },
    cache: new DecisionCache(60_000, 100),
    audit,
    config,
  });
  return { router, audit };
}

describe("漏斗 V2：按问题粒度升级", () => {
  it("部分低置信：只把低置信子集送 Jev，高置信答案保留，sources 逐问溯源", async () => {
    const laya = scriptedProvider("laya", { q1: 0.95, q2: 0.5 });
    const jev = scriptedProvider("jev", { q2: 0.9 });
    const { router } = makeRouter(laya, jev);
    const result = await router.decide({
      state: "s",
      questions: questionsOf(2),
    });
    // jev 只被问 q2（单包升级）
    expect(jev.decideMock).toHaveBeenCalledTimes(1);
    expect(Object.keys(jev.decideMock.mock.calls[0][0].questions)).toEqual(["q2"]);
    // 合并：q1 保留 laya 答案，q2 采信 jev
    expect(result.answers.q1.model).toBe("laya");
    expect(result.answers.q2.model).toBe("jev");
    expect(result.sources).toEqual({ q1: "laya", q2: "jev" });
    expect(result.degraded).toBe(false);
  });

  it("子集超过 8 问 → 整批送 Jev 重验（批量更省）", async () => {
    const laya = scriptedProvider("laya", {});
    const jev = scriptedProvider("jev", {});
    const { router } = makeRouter(laya, jev);
    // 9 问全部 0.5：低于阈值
    await router.decide({ state: "s", questions: questionsOf(9) });
    expect(Object.keys(jev.decideMock.mock.calls[0][0].questions)).toHaveLength(9);
  });

  it("升级后仍低 → 保留 laya 原答案并标 degraded（§3 保守默认：不确定即保留）", async () => {
    const laya = scriptedProvider("laya", { q1: 0.5 });
    const jev = scriptedProvider("jev", { q1: 0.4 });
    const { router } = makeRouter(laya, jev);
    const result = await router.decide({ state: "s", questions: questionsOf(1) });
    expect(result.answers.q1.model).toBe("laya");
    expect(result.answers.q1.confidence).toBe(0.5);
    expect(result.degraded).toBe(true);
    expect(result.sources).toEqual({ q1: "laya" });
  });

  it("Jev 升级失败 → 原漏斗继续（fallback 不可用 → 交还 laya best，degraded）", async () => {
    const laya = scriptedProvider("laya", { q1: 0.5 });
    const jev = scriptedProvider(
      "jev",
      { q1: 0.9 },
      { fail: new DecisionUnavailableError("链全断") },
    );
    const { router } = makeRouter(laya, jev);
    const result = await router.decide({ state: "s", questions: questionsOf(1) });
    expect(result.model).toBe("laya");
    expect(result.degraded).toBe(true);
    expect(result.answers.q1.model).toBe("laya");
  });

  it("敏感请求钉本地：private 不升级，Jev 连被尝试的机会都没有", async () => {
    const laya = scriptedProvider("laya", { q1: 0.5 });
    const jev = scriptedProvider("jev", { q1: 0.9 });
    const { router } = makeRouter(laya, jev);
    const result = await router.decide({
      state: "s",
      questions: questionsOf(1),
      sensitivity: "private",
    });
    expect(jev.decideMock).not.toHaveBeenCalled();
    expect(result.degraded).toBe(true);
  });

  it("preference=jev 时 laya 低置信不再回头升级（jev 已经答过）", async () => {
    const laya = scriptedProvider("laya", { q1: 0.5 });
    const jev = scriptedProvider("jev", { q1: 0.5 });
    const { router } = makeRouter(laya, jev);
    const result = await router.decide({
      state: "s",
      questions: questionsOf(1),
      modelPreference: "jev",
    });
    // jev 一共被问 1 次（漏斗首层），laya 被问到后不再触发升级
    expect(jev.decideMock).toHaveBeenCalledTimes(1);
    expect(laya.decideMock).toHaveBeenCalledTimes(1);
    expect(result.degraded).toBe(true);
  });

  it("整批高置信路径 sources 也全量补齐（全 laya）", async () => {
    const laya = scriptedProvider("laya", { q1: 0.95, q2: 0.9 });
    const jev = scriptedProvider("jev", {});
    const { router } = makeRouter(laya, jev);
    const result = await router.decide({ state: "s", questions: questionsOf(2) });
    expect(jev.decideMock).not.toHaveBeenCalled();
    expect(result.sources).toEqual({ q1: "laya", q2: "laya" });
  });

  it("合并结果按原请求缓存：第二次调用直接命中，不再惊动任何 Provider", async () => {
    const laya = scriptedProvider("laya", { q1: 0.95, q2: 0.5 });
    const jev = scriptedProvider("jev", { q2: 0.9 });
    const { router } = makeRouter(laya, jev);
    const request: DecisionRequest = { state: "s", questions: questionsOf(2) };
    const first = await router.decide(request);
    const second = await router.decide(request);
    expect(second.cacheHit).toBe(true);
    expect(second.sources).toEqual(first.sources);
    expect(jev.decideMock).toHaveBeenCalledTimes(1);
    expect(laya.decideMock).toHaveBeenCalledTimes(1);
  });
});
