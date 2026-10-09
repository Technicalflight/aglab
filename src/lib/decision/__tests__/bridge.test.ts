/**
 * 决策桥的派发表：Rust 递过来的方法名 → 决策层的嵌入函数 → 可序列化的结论。
 * 派发函数是纯的（ integrations 由参数注入），不需要 Tauri 就能测。
 */
import { describe, expect, it, vi } from "vitest";
import { createIntegrations } from "../integrations";
import { DEFAULT_DECISION_CONFIG, mergeDecisionConfig } from "../config";
import type { IntegrationsConfig } from "../config";
import type { DecisionRequest, DecisionRouter } from "../types";
import type { DecisionSystem } from "../index";
import { DECISION_BRIDGE_EVENT, handleBridgeAsk } from "../bridge";

function systemWith(
  integrations: Partial<IntegrationsConfig>,
  answers: Record<string, unknown>,
): DecisionSystem {
  const router = {
    decide: vi.fn(async (_request: DecisionRequest) => ({
      answers,
      model: "laya",
      totalLatencyMs: 1,
      cacheHit: false,
    })),
  } as unknown as DecisionRouter;
  return {
    config: mergeDecisionConfig({
      ...DEFAULT_DECISION_CONFIG,
      integrations: { ...DEFAULT_DECISION_CONFIG.integrations, ...integrations },
    }),
    cache: null,
    audit: null,
    router,
  } as DecisionSystem;
}

const AGENTS = [
  { role: "reader", description: "只看不改" },
  { role: "worker", description: "动手执行" },
  { role: "verifier", description: "只读复核" },
];

describe("决策桥的派发表", () => {
  it("assignAgent：抽结论本体回给 Rust，response 不过桥", async () => {
    const system = systemWith(
      { taskAssignment: true },
      {
        best_agent: {
          questionName: "best_agent",
          type: "choice",
          choice: "reader",
          probabilities: { reader: 0.9 },
          confidence: 0.9,
          model: "laya",
          latencyMs: 1,
        },
        priority: {
          questionName: "priority",
          type: "score",
          score: 3,
          confidence: 0.7,
          model: "laya",
          latencyMs: 1,
        },
        can_parallel: {
          questionName: "can_parallel",
          type: "noul",
          noul: 0.8,
          confidence: 0.8,
          model: "laya",
          latencyMs: 1,
        },
      },
    );
    const answer = (await handleBridgeAsk(createIntegrations(system), "assignAgent", {
      task: { goal: "把结论核对一遍" },
      agents: AGENTS,
    })) as { agent: string; priority: number; canParallel: boolean };
    expect(answer).toEqual({ agent: "reader", priority: 3, canParallel: true });
  });

  it("开关关着就回 null：Rust 侧拿到的是明确的「决策层说 no」，不是等待", async () => {
    const system = systemWith({}, {});
    const answer = await handleBridgeAsk(createIntegrations(system), "assignAgent", {
      task: { goal: "任何任务" },
      agents: AGENTS,
    });
    expect(answer).toBeNull();
  });

  it("scoreContextRelevance：分数与候选顺序对齐地回", async () => {
    const score = (value: number) => ({
      questionName: "x",
      type: "score",
      score: value,
      confidence: 0.6,
      model: "laya",
      latencyMs: 1,
    });
    const system = systemWith(
      { contextRelevance: true },
      {
        cand_0: score(4.5),
        cand_1: score(1.5),
        cand_2: score(3),
      },
    );
    const answer = (await handleBridgeAsk(createIntegrations(system), "scoreContextRelevance", {
      query: "部署脚本怎么配",
      candidates: [
        { id: 0, text: "部署脚本在 scripts/" },
        { id: 1, text: "午餐吃什么" },
        { id: 2, text: "流水线三条" },
      ],
    })) as { scores: number[] };
    expect(answer.scores).toEqual([4.5, 1.5, 3]);
  });

  it("认不出的方法名回 null：桥的两端版本错位时 Rust 侧照旧走原路", async () => {
    const system = systemWith({ taskAssignment: true, contextRelevance: true }, {});
    expect(await handleBridgeAsk(createIntegrations(system), "teleport", {})).toBeNull();
    expect(DECISION_BRIDGE_EVENT).toBe("decision://ask");
  });
});
