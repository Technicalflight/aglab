import { describe, expect, it } from "vitest";
import type {
  DecisionAnswer,
  DecisionAttempt,
  DecisionTrace,
  ModelTier,
  Question,
  Sensitivity,
} from "../types";
import {
  confidenceBuckets,
  confidenceBucketIndex,
  groupBySignature,
  latencyBuckets,
  minConfidenceOf,
  percentile,
  routingByTaskType,
  signatureLabel,
  summarizeTraces,
  traceSignature,
  tracesForConversation,
} from "../stats";

function answer(
  questionName: string,
  over: Partial<DecisionAnswer> & { confidence: number },
): DecisionAnswer {
  return {
    questionName,
    type: "noul",
    noul: over.confidence,
    model: "laya",
    latencyMs: 3,
    ...over,
  };
}

interface TraceOptions {
  questions?: Record<string, Question>;
  /** null = 这次一个答案都没拿到（全层失败的 trace） */
  answers?: Record<string, DecisionAnswer> | null;
  model?: ModelTier;
  chain?: ModelTier[];
  latency?: number;
  cacheHit?: boolean;
  degraded?: boolean;
  sensitivity?: Sensitivity;
  conversationId?: string;
  attempts?: DecisionAttempt[];
  id?: string;
}

let seq = 0;

function trace(options: TraceOptions = {}): DecisionTrace {
  const answers =
    options.answers === undefined ? { q1: answer("q1", { confidence: 0.9 }) } : options.answers;
  const response = answers
    ? {
        answers,
        model: options.model ?? "laya",
        totalLatencyMs: options.latency ?? 12,
        cacheHit: options.cacheHit ?? false,
        ...(options.degraded === undefined ? {} : { degraded: options.degraded }),
      }
    : null;
  seq += 1;
  return {
    id: options.id ?? `t${seq}`,
    timestamp: "2026-09-27T00:00:00.000Z",
    request: {
      state: "state",
      questions: options.questions ?? { q1: { type: "noul", instructions: "?" } },
      ...(options.sensitivity === undefined ? {} : { sensitivity: options.sensitivity }),
      ...(options.conversationId === undefined ? {} : { conversationId: options.conversationId }),
    },
    response,
    modelChain: options.chain ?? (options.cacheHit ? [] : ["laya"]),
    attempts: options.attempts ?? [],
    totalLatencyMs: options.latency ?? 12,
    cacheHit: options.cacheHit ?? false,
  };
}

/** routeModel 那一组问题：观测表与签名分组都靠它 */
function routingTrace(taskType: string, complexity: number, premium: number, confidence = 0.9) {
  return trace({
    questions: {
      task_type: { type: "choice", instructions: "?", criteria: { [taskType]: "" } },
      needs_premium: { type: "noul", instructions: "?" },
      complexity: { type: "score", instructions: "?", criteria: [0, 1, 2, 3, 4] },
    },
    answers: {
      task_type: answer("task_type", { type: "choice", choice: taskType, confidence }),
      needs_premium: answer("needs_premium", { noul: premium, confidence }),
      complexity: answer("complexity", { type: "score", score: complexity, confidence }),
    },
  });
}

describe("percentile", () => {
  it("空集合是 0，不是 NaN", () => {
    expect(percentile([], 0.5)).toBe(0);
  });

  it("最近秩：p50 落在第 5 个、p95 落在最后一格", () => {
    const ten = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
    expect(percentile(ten, 0.5)).toBe(5);
    expect(percentile(ten, 0.95)).toBe(10);
    expect(percentile(ten, 0.01)).toBe(1);
  });

  it("输入乱序不影响读数（它自己排序）", () => {
    expect(percentile([9, 1, 5, 3, 7], 0.5)).toBe(5);
  });
});

describe("summarizeTraces", () => {
  it("一条都没有时全部是 0，命中率也是 0", () => {
    const summary = summarizeTraces([]);
    expect(summary.decisions).toBe(0);
    expect(summary.cacheHitRate).toBe(0);
    expect(summary.p95LatencyMs).toBe(0);
    expect(summary.questionsPerDecision).toBe(0);
    expect(summary.chains).toEqual([]);
  });

  it("命中、降级、失败三格各算各的", () => {
    const summary = summarizeTraces([
      trace({}), // 直接给出
      trace({ cacheHit: true }),
      trace({ degraded: true, answers: { q1: answer("q1", { confidence: 0.4 }) } }),
      trace({ answers: null, chain: ["laya"] }),
    ]);
    expect(summary.decisions).toBe(4);
    expect(summary.cacheHits).toBe(1);
    expect(summary.cacheHitRate).toBeCloseTo(0.25);
    expect(summary.degraded).toBe(1);
    expect(summary.failed).toBe(1);
    expect(summary.answered).toBe(3);
    expect(summary.questions).toBe(3); // 失败那次没有问题可言
  });

  it("耗时只统计拿到答案的那些", () => {
    const summary = summarizeTraces([
      trace({ latency: 20 }),
      trace({ latency: 40 }),
      trace({ answers: null, latency: 5000 }), // 超时失败：不该把均值拖到天上
    ]);
    expect(summary.avgLatencyMs).toBeCloseTo(30);
    expect(summary.p95LatencyMs).toBe(40);
  });

  it("链路标签：多层用箭头连，缓存命中单独一格", () => {
    const summary = summarizeTraces([
      trace({ chain: ["laya", "jev"], model: "jev" }),
      trace({ chain: ["laya", "jev"], model: "jev" }),
      trace({ cacheHit: true, chain: [] }),
    ]);
    expect(summary.chains).toEqual([
      { chain: "laya→jev", count: 2 },
      { chain: "缓存", count: 1 },
    ]);
    expect(summary.byTier.jev).toBe(2);
    // 命中的那次也算在给出答案的那层上：缓存里那份本来就是 laya 算的。
    // "其中几次没重跑"由 cacheHits 那一格说，不靠从 byTier 里挖掉
    expect(summary.byTier.laya).toBe(1);
  });

  it("sensitivity 缺省算 public", () => {
    const summary = summarizeTraces([
      trace({}),
      trace({ sensitivity: "confidential" }),
      trace({ sensitivity: "private" }),
    ]);
    expect(summary.bySensitivity).toEqual({ public: 1, private: 1, confidential: 1 });
  });

  /** 红线越界是"关掉了 sensitiveForceLocal"才会出现的形状：它必须是读得出来的数 */
  it("敏感请求被云端答了，就是一次越界", () => {
    const clean = summarizeTraces([
      trace({ sensitivity: "confidential" }), // model 默认 laya
      trace({ sensitivity: "private", model: "laya" }),
      trace({}), // public 走云端不算越界
      trace({ model: "jev" }),
    ]);
    expect(clean.redlineBreaches).toBe(0);

    const breached = summarizeTraces([
      trace({ sensitivity: "confidential", model: "jev" }),
      trace({ sensitivity: "private", model: "fallback" }),
      trace({ sensitivity: "public", model: "jev" }),
    ]);
    expect(breached.redlineBreaches).toBe(2);
  });

  it("失败的敏感请求不算越界（没答案就没有内容出网）", () => {
    expect(
      summarizeTraces([trace({ answers: null, sensitivity: "confidential" })]).redlineBreaches,
    ).toBe(0);
  });

  it("questionsPerDecision 是批量宽度，失败的那些不进分母", () => {
    const summary = summarizeTraces([
      routingTrace("code_generation", 3, 0.8), // 三个问题
      trace({ answers: null }),
    ]);
    expect(summary.questionsPerDecision).toBeCloseTo(3);
  });
});

describe("minConfidenceOf", () => {
  it("木桶效应：取这一批里最不确信的那个", () => {
    const value = minConfidenceOf(
      trace({
        answers: {
          a: answer("a", { confidence: 0.95 }),
          b: answer("b", { confidence: 0.42 }),
          c: answer("c", { confidence: 0.7 }),
        },
      }),
    );
    expect(value).toBe(0.42);
  });

  it("没有答案就没有这个数（不是 0）", () => {
    expect(minConfidenceOf(trace({ answers: null }))).toBeNull();
    expect(minConfidenceOf(trace({ answers: {} }))).toBeNull();
  });
});

describe("分桶", () => {
  it("边界值归上一档", () => {
    const buckets = latencyBuckets([
      trace({ latency: 25 }), // 正好 25 → 落在 25–50
      trace({ latency: 24.9 }),
    ]);
    expect(buckets.find((bucket) => bucket.label === "<10ms")?.count).toBe(0);
    expect(buckets.find((bucket) => bucket.label === "10–25ms")?.count).toBe(1);
    expect(buckets.find((bucket) => bucket.label === "25–50ms")?.count).toBe(1);
  });

  it("耗时分布不看失败的那些", () => {
    const buckets = latencyBuckets([
      trace({ latency: 5 }),
      trace({ answers: null, latency: 9000 }),
    ]);
    expect(buckets.reduce((acc, bucket) => acc + bucket.count, 0)).toBe(1);
  });

  it("置信度十格，失败与空答案都不进来", () => {
    const buckets = confidenceBuckets([
      trace({ answers: { q1: answer("q1", { confidence: 0.85 }) } }),
      trace({ answers: { q1: answer("q1", { confidence: 0.1 }) } }),
      trace({ answers: null }),
      trace({ answers: {} }),
    ]);
    expect(buckets).toHaveLength(10);
    expect(buckets.reduce((acc, bucket) => acc + bucket.count, 0)).toBe(2);
    expect(buckets.find((bucket) => bucket.label === "0.8–0.9")?.count).toBe(1);
    // 0.1 恰好不在 <0.1 那格——归 0.1–0.2
    expect(buckets.find((bucket) => bucket.label === "<0.1")?.count).toBe(0);
    expect(buckets.find((bucket) => bucket.label === "0.1–0.2")?.count).toBe(1);
  });

  it("阈值落在哪一格与分桶用的是同一把尺", () => {
    // 面板高亮的那一格必须就是直方图里装着这个阈值的那一格，否则刻度在骗人
    expect(confidenceBucketIndex(0.85)).toBe(8);
    expect(
      confidenceBuckets([trace({ answers: { q1: answer("q1", { confidence: 0.85 }) } })])[8].label,
    ).toBe("0.8–0.9");
    expect(confidenceBucketIndex(0.05)).toBe(0);
    expect(confidenceBucketIndex(0.95)).toBe(9);
    expect(confidenceBucketIndex(0)).toBe(0);
  });
});

describe("签名与分组", () => {
  it("问题键序不影响签名", () => {
    const a = traceSignature(
      trace({
        questions: {
          x: { type: "noul", instructions: "?" },
          y: { type: "noul", instructions: "?" },
        },
      }),
    );
    const b = traceSignature(
      trace({
        questions: {
          y: { type: "noul", instructions: "?" },
          x: { type: "noul", instructions: "?" },
        },
      }),
    );
    expect(a).toBe(b);
  });

  it("四个嵌入点认得出来，候选打分的形状也认得，剩下的照实显示", () => {
    expect(signatureLabel("complexity,needs_premium,task_type")).toBe("模型路由");
    expect(signatureLabel("worth_remembering")).toBe("提取门控");
    expect(signatureLabel("contains_pii,contains_secret,risk_level")).toBe("敏感检测");
    expect(signatureLabel("best_agent,can_parallel,priority")).toBe("助理分配");
    expect(signatureLabel("cand_7,cand_9")).toBe("上下文相关性");
    expect(signatureLabel("my_custom_question")).toBe("my_custom_question");
    expect(signatureLabel("")).toBe("（没有问题）");
  });

  it("按签名分组：次数降序、敏感级别取该组最高一档、层按实际出现的", () => {
    const groups = groupBySignature([
      routingTrace("code_generation", 3, 0.8),
      routingTrace("simple_qa", 1, 0.1, 0.5),
      trace({
        questions: { worth_remembering: { type: "noul", instructions: "?" } },
        sensitivity: "private",
        model: "jev",
        chain: ["laya", "jev"],
      }),
      trace({
        answers: null,
        questions: { worth_remembering: { type: "noul", instructions: "?" } },
      }),
    ]);
    expect(groups.map((group) => group.label)).toEqual(["模型路由", "提取门控"]);
    const routing = groups[0];
    expect(routing.count).toBe(2);
    expect(routing.degraded).toBe(0);
    expect(routing.tiers).toEqual(["laya"]);
    expect(routing.maxSensitivity).toBe("public");
    const gate = groups[1];
    expect(gate.failed).toBe(1);
    expect(gate.maxSensitivity).toBe("private");
    expect(gate.tiers).toEqual(["jev"]);
    expect(gate.avgMinConfidence).toBeCloseTo(0.9); // 失败那次没有置信度，不进平均
  });
});

describe("模型路由观测", () => {
  it("按任务类型摊账：次数、复杂度均值、需要大模型的比例", () => {
    const rows = routingByTaskType(
      [
        routingTrace("code_generation", 4, 0.9),
        routingTrace("code_generation", 2, 0.4),
        routingTrace("simple_qa", 0, 0.05),
      ],
      0.85,
    );
    expect(rows.map((row) => row.taskType)).toEqual(["code_generation", "simple_qa"]);
    const code = rows[0];
    expect(code.count).toBe(2);
    expect(code.avgComplexity).toBeCloseTo(3);
    expect(code.premiumRate).toBeCloseTo(0.5);
    expect(code.aboveThreshold).toBeCloseTo(1);
  });

  it("没有 task_type 的判定与失败的判定都不进这张表", () => {
    expect(routingByTaskType([trace({}), trace({ answers: null })], 0.85)).toEqual([]);
  });
});

const request_of = (trace: DecisionTrace) => trace.request as { conversationId?: string };

describe("tracesForConversation：决策面板按话题过滤", () => {
  it("每场对话只看见自己的判定（正对照：两场的条数各自对得上，不是因为集合并为空）", () => {
    const traces = [
      trace({ conversationId: "conv-A" }),
      trace({ conversationId: "conv-B" }),
      trace({ conversationId: "conv-A" }),
    ];
    expect(tracesForConversation(traces, "conv-A")).toHaveLength(2);
    expect(tracesForConversation(traces, "conv-B")).toHaveLength(1);
  });

  it("反向钉：没带 conversationId 的判定不落进任何一场对话——试一次与记忆分级不该被念成本场对话的判定", () => {
    const unscoped = trace();
    expect(request_of(unscoped).conversationId).toBeUndefined();
    expect(tracesForConversation([unscoped], "conv-A")).toEqual([]);
    expect(tracesForConversation([unscoped], "conv-B")).toEqual([]);
  });

  it("没有打开的话题时是空集，不是「全部」——否则切到新对话会看见上一场的读数", () => {
    const traces = [trace({ conversationId: "conv-A" }), trace()];
    expect(tracesForConversation(traces, null)).toEqual([]);
  });
});
