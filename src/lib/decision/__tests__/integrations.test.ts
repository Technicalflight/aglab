import { describe, expect, it, vi } from "vitest";
import { createIntegrations } from "../integrations";
import { DEFAULT_DECISION_CONFIG, mergeDecisionConfig } from "../config";
import type { IntegrationsConfig } from "../config";
import type { DecisionRequest, DecisionResponse, DecisionRouter } from "../types";
import type { DecisionSystem } from "../index";

type AnswerSeed = { choice?: string; noul?: number; score?: number; probabilities?: Record<string, number> };

/** 按问题名手工拼一批规范化答案（形状与 normalizeProviderAnswers 的产物一致） */
function answersOf(seeds: Record<string, AnswerSeed>, model: DecisionResponse["model"] = "laya"): Record<string, DecisionAnswerForTest> {
  return Object.fromEntries(
    Object.entries(seeds).map(([name, seed]) => [
      name,
      {
        questionName: name,
        type: seed.choice !== undefined ? "choice" : seed.noul !== undefined ? "noul" : "score",
        ...(seed.choice !== undefined ? { choice: seed.choice, probabilities: seed.probabilities } : {}),
        ...(seed.noul !== undefined ? { noul: seed.noul } : {}),
        ...(seed.score !== undefined ? { score: seed.score } : {}),
        confidence: seed.noul !== undefined ? Math.max(seed.noul, 1 - seed.noul) : (seed.probabilities?.[seed.choice ?? ""] ?? 0.6),
        model,
        latencyMs: 1,
      },
    ]),
  );
}
type DecisionAnswerForTest = DecisionResponse["answers"][string];

/** 造一个带脚本化路由器的假系统：记录每次 decide 收到的请求，返回脚本给定的答案 */
function systemWith(
  integrations: Partial<IntegrationsConfig>,
  handler: (request: DecisionRequest) => DecisionResponse | Promise<DecisionResponse> = () => {
    throw new Error("not scripted");
  },
): DecisionSystem & { decideCalls: DecisionRequest[] } {
  const decideCalls: DecisionRequest[] = [];
  const router = {
    decide: vi.fn(async (request: DecisionRequest) => {
      decideCalls.push(request);
      return handler(request);
    }),
  } as unknown as DecisionRouter;
  return {
    config: mergeDecisionConfig({
      ...DEFAULT_DECISION_CONFIG,
      integrations: { ...DEFAULT_DECISION_CONFIG.integrations, ...integrations },
    }),
    cache: null,
    audit: null,
    router,
    decideCalls,
  } as DecisionSystem & { decideCalls: DecisionRequest[] };
}

const ALL_ON: Partial<IntegrationsConfig> = {
  modelRouting: true,
  memoryGate: true,
  sensitivityScan: true,
  taskAssignment: true,
};

describe("integrations.routeModel（§7.1 模型路由，观测面）", () => {
  it("解析出任务类型/是否要大模型/复杂度", async () => {
    const system = systemWith(ALL_ON, () => ({
      answers: answersOf({
        task_type: { choice: "code_generation", probabilities: { code_generation: 0.93 } },
        needs_premium: { noul: 0.82 },
        complexity: { score: 2.4 },
      }),
      model: "laya",
      totalLatencyMs: 3,
      cacheHit: false,
    }));
    const result = await createIntegrations(system).routeModel("帮我修这个报错");
    expect(result?.taskType).toBe("code_generation");
    expect(result?.needsPremium).toBe(true);
    expect(result?.complexity).toBeCloseTo(2.4);
  });

  it("开关关着：一个决定都不问", async () => {
    const system = systemWith({ modelRouting: false });
    expect(await createIntegrations(system).routeModel("hi")).toBeNull();
    expect(system.decideCalls).toHaveLength(0);
  });

  it("路由器不可用：null（fail-open），不往外抛", async () => {
    const system = systemWith(ALL_ON, () => {
      throw new Error("sidecar down");
    });
    await expect(createIntegrations(system).routeModel("hi")).resolves.toBeNull();
  });
});

describe("integrations.gateMemoryExtraction（§7.2 提取门控）", () => {
  const turns = [{ role: "user", content: "我住在杭州，邮编 310000" }];

  it("判定钉在本地（private）：对话正文不为省调用多出一条出网的路", async () => {
    const system = systemWith(ALL_ON, () => ({
      answers: answersOf({ worth_remembering: { noul: 0.2 } }),
      model: "laya",
      totalLatencyMs: 2,
      cacheHit: false,
    }));
    await createIntegrations(system).gateMemoryExtraction(turns);
    expect(system.decideCalls[0].sensitivity).toBe("private");
  });

  it("低于门限 → skip；高于门限 → 不跳", async () => {
    const low = systemWith(ALL_ON, () => ({
      answers: answersOf({ worth_remembering: { noul: 0.2 } }),
      model: "laya",
      totalLatencyMs: 1,
      cacheHit: false,
    }));
    expect((await createIntegrations(low).gateMemoryExtraction(turns))?.skip).toBe(true);

    const high = systemWith(ALL_ON, () => ({
      answers: answersOf({ worth_remembering: { noul: 0.9 } }),
      model: "laya",
      totalLatencyMs: 1,
      cacheHit: false,
    }));
    expect((await createIntegrations(high).gateMemoryExtraction(turns))?.skip).toBe(false);
  });

  it("答案缺 worth 这一格：fail-open 不跳过", async () => {
    const system = systemWith(ALL_ON, () => ({
      answers: answersOf({}),
      model: "laya",
      totalLatencyMs: 1,
      cacheHit: false,
    }));
    const verdict = await createIntegrations(system).gateMemoryExtraction(turns);
    expect(verdict?.skip).toBe(false);
    expect(verdict?.worth).toBeNull();
  });
});

describe("integrations.detectSensitivity（§7.4 敏感检测）", () => {
  it("判定钉在本地（confidential）", async () => {
    const system = systemWith(ALL_ON, () => ({
      answers: answersOf({ contains_secret: { noul: 0.97 }, contains_pii: { noul: 0.1 }, risk_level: { choice: "high" } }),
      model: "laya",
      totalLatencyMs: 2,
      cacheHit: false,
    }));
    await createIntegrations(system).detectSensitivity("api_key=sk-123");
    expect(system.decideCalls[0].sensitivity).toBe("confidential");
  });

  it("风险三档映射到记忆外发三档：high→secret、low→private、safe→public", async () => {
    const scripted: Array<[string, "public" | "private" | "secret"]> = [
      ["high", "secret"],
      ["low", "private"],
      ["safe", "public"],
    ];
    for (const [risk, suggested] of scripted) {
      const system = systemWith(ALL_ON, () => ({
        answers: answersOf({ contains_secret: { noul: 0.1 }, contains_pii: { noul: 0.1 }, risk_level: { choice: risk } }),
        model: "laya",
        totalLatencyMs: 1,
        cacheHit: false,
      }));
      expect((await createIntegrations(system).detectSensitivity("x"))?.suggested).toBe(suggested);
    }
  });

  it("风险档不在枚举内：按畸形答案处理返回 null", async () => {
    const system = systemWith(ALL_ON, () => ({
      answers: answersOf({ risk_level: { choice: "moderately_spicy" } }),
      model: "laya",
      totalLatencyMs: 1,
      cacheHit: false,
    }));
    expect(await createIntegrations(system).detectSensitivity("x")).toBeNull();
  });
});

describe("integrations.scoreContextRelevance（§7.5 上下文相关性，已接线）", () => {
  const candidates = [
    { id: 1, text: "rust ownership rules" },
    { id: 2, text: "今天的钓鱼日记" },
  ];

  it("每个候选一个问题、分数与传入顺序对齐", async () => {
    const system = systemWith({ ...ALL_ON, contextRelevance: true }, () => ({
      answers: answersOf({ cand_1: { score: 4.5 }, cand_2: { score: 0.2 } }),
      model: "laya",
      totalLatencyMs: 3,
      cacheHit: false,
    }));
    const result = await createIntegrations(system).scoreContextRelevance("ownership", candidates);
    expect(result?.scores).toEqual([4.5, 0.2]);
    // 批量前向：一次请求带全部问题，不是逐候选一次调用
    expect(Object.keys(system.decideCalls[0].questions)).toEqual(["cand_1", "cand_2"]);
  });

  it("任何一个候选缺答案：整批放弃，不出半份排序依据", async () => {
    const system = systemWith({ ...ALL_ON, contextRelevance: true }, () => ({
      answers: answersOf({ cand_1: { score: 4.5 } }),
      model: "laya",
      totalLatencyMs: 1,
      cacheHit: false,
    }));
    expect(await createIntegrations(system).scoreContextRelevance("q", candidates)).toBeNull();
  });

  it("空候选列表与开关关着都不问模型", async () => {
    const off = systemWith({ ...ALL_ON, contextRelevance: false });
    expect(await createIntegrations(off).scoreContextRelevance("q", candidates)).toBeNull();
    const empty = systemWith({ ...ALL_ON, contextRelevance: true });
    expect(await createIntegrations(empty).scoreContextRelevance("q", [])).toBeNull();
    expect(empty.decideCalls).toHaveLength(0);
  });
});

describe("integrations.assignAgent（§7.3 Agent 分配，已接线）", () => {
  const agents = [
    { role: "researcher", description: "digs through sources" },
    { role: "coder", description: "writes and runs code" },
  ];

  it("花名册内的角色照常分配", async () => {
    const system = systemWith(ALL_ON, () => ({
      answers: answersOf({
        best_agent: { choice: "coder", probabilities: { coder: 0.88, researcher: 0.1 } },
        priority: { score: 4.2 },
        can_parallel: { noul: 0.95 },
      }),
      model: "laya",
      totalLatencyMs: 3,
      cacheHit: false,
    }));
    const result = await createIntegrations(system).assignAgent({ goal: "fix the bug" }, agents);
    expect(result?.agent).toBe("coder");
    expect(result?.canParallel).toBe(true);
    // 花名册进了 criteria，选出来的值必须是其中之一
    expect(system.decideCalls[0].questions.best_agent.type).toBe("choice");
  });

  it("花名册之外的角色：宁可不指派也不指错人", async () => {
    const system = systemWith(ALL_ON, () => ({
      answers: answersOf({ best_agent: { choice: "ghost" } }),
      model: "laya",
      totalLatencyMs: 1,
      cacheHit: false,
    }));
    expect(await createIntegrations(system).assignAgent({ goal: "x" }, agents)).toBeNull();
  });

  it("空花名册直接 null，不问模型", async () => {
    const system = systemWith(ALL_ON);
    expect(await createIntegrations(system).assignAgent({ goal: "x" }, [])).toBeNull();
    expect(system.decideCalls).toHaveLength(0);
  });
});
