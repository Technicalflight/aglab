/**
 * 决策审计的读数层：把一串 DecisionTrace 折成面板能直接画的东西。
 *
 * 为什么单独一个文件而不是组件里现算：这些数字是"决策层到底有没有在干活"的凭据，
 * 得能被钉死在测试里（本项目没有 React 的测试设施，但有 vitest——逻辑剥到这里来）。
 *
 * 口径上的一条通则：**失败的决策（response=null）不参与耗时与置信度分布**。
 * 它没有答案，把它的 5 秒超时混进置信度直方图里就是编数据；它单独占一格 `failed`。
 */
import type { DecisionTrace, ModelTier, Sensitivity } from "./types";

const TIERS: readonly ModelTier[] = ["laya", "jev", "fallback"];
/** 会离开本机的两层。fallback 是 System 2 LLM，同样是云端——它冒充"本地"是红线级的谎 */
const CLOUD_TIERS: readonly ModelTier[] = ["jev", "fallback"];

/** 一次批量决策里最不确信的那个答案。没有答案就没有这个数 */
export function minConfidenceOf(trace: DecisionTrace): number | null {
  const answers = trace.response ? Object.values(trace.response.answers) : [];
  if (answers.length === 0) return null;
  return Math.min(...answers.map((answer) => answer.confidence));
}

/** 最近秩法：p50/p95 要的是"这一档以下有多少比例"，不是插值出来的小数 */
export function percentile(values: readonly number[], p: number): number {
  if (values.length === 0) return 0;
  const sorted = [...values].sort((a, b) => a - b);
  const rank = Math.min(sorted.length - 1, Math.max(0, Math.ceil(p * sorted.length) - 1));
  return sorted[rank];
}

export interface DecisionSummary {
  /** 进过审计的次数（含一次都没问成功的） */
  decisions: number;
  answered: number;
  /** response=null：所有层都没给出答案。sidecar 没起最常落在这里 */
  failed: number;
  cacheHits: number;
  /** 命中数 / 总次数。总次数为 0 时是 0，不是 NaN */
  cacheHitRate: number;
  /** 有答案但没达到阈值就交还的（调用方该看一眼置信度） */
  degraded: number;
  /** 累计被回答的问题个数：批量决策的宽度，一次三个问题算三个 */
  questions: number;
  avgLatencyMs: number;
  p50LatencyMs: number;
  p95LatencyMs: number;
  /** 平均一次决策带几个问题。失败的那些不参与（它们没有问题可言） */
  questionsPerDecision: number;
  byTier: Record<ModelTier, number>;
  bySensitivity: Record<Sensitivity, number>;
  /**
   * 红线越界次数：sensitivity 是 private/confidential，答案却来自云端层（jev/fallback）。
   * 正确配置下永远是 0——它出现在这里就是因为有人把 sensitiveForceLocal 关了，
   * 那时候它是这块面板上最该看见的一个数
   */
  redlineBreaches: number;
  /** 链路构成，按次数降序。chain 是人读的一行，如 "laya→jev"；空链路是缓存命中 */
  chains: Array<{ chain: string; count: number }>;
}

export function summarizeTraces(traces: readonly DecisionTrace[]): DecisionSummary {
  const byTier = { laya: 0, jev: 0, fallback: 0 } as Record<ModelTier, number>;
  const bySensitivity = { public: 0, private: 0, confidential: 0 } as Record<Sensitivity, number>;
  const chainCounts = new Map<string, number>();
  const latencies: number[] = [];
  let answered = 0;
  let cacheHits = 0;
  let degraded = 0;
  let questions = 0;
  let redlineBreaches = 0;

  for (const trace of traces) {
    const sensitivity = trace.request.sensitivity ?? "public";
    bySensitivity[sensitivity] += 1;
    if (trace.cacheHit) cacheHits += 1;
    const chainText = trace.modelChain.join("→") || "缓存";
    chainCounts.set(chainText, (chainCounts.get(chainText) ?? 0) + 1);
    if (!trace.response) continue;
    if (sensitivity !== "public" && CLOUD_TIERS.includes(trace.response.model))
      redlineBreaches += 1;
    answered += 1;
    if (trace.response.degraded) degraded += 1;
    byTier[trace.response.model] += 1;
    questions += Object.keys(trace.response.answers).length;
    latencies.push(trace.totalLatencyMs);
  }

  const total = traces.length;
  const sum = latencies.reduce((acc, value) => acc + value, 0);
  return {
    decisions: total,
    answered,
    failed: total - answered,
    cacheHits,
    cacheHitRate: total > 0 ? cacheHits / total : 0,
    degraded,
    questions,
    avgLatencyMs: latencies.length > 0 ? sum / latencies.length : 0,
    p50LatencyMs: percentile(latencies, 0.5),
    p95LatencyMs: percentile(latencies, 0.95),
    questionsPerDecision: answered > 0 ? questions / answered : 0,
    byTier,
    bySensitivity,
    redlineBreaches,
    chains: [...chainCounts.entries()]
      .map(([chain, count]) => ({ chain, count }))
      .sort((a, b) => b.count - a.count || a.chain.localeCompare(b.chain)),
  };
}

const LATENCY_EDGES_MS = [10, 25, 50, 100, 250, 500, 1000, 2000] as const;

export interface Bucket {
  label: string;
  count: number;
}

function bucketLabel(edges: readonly number[], index: number, unit: string): string {
  if (index === 0) return `<${edges[0]}${unit}`;
  if (index === edges.length) return `≥${edges[edges.length - 1]}${unit}`;
  return `${edges[index - 1]}–${edges[index]}${unit}`;
}

/** 值落进哪一格：恰好等于边界值算上一档（"≥25"而不是"<25"） */
function bucketIndex(value: number, edges: readonly number[]): number {
  for (let i = 0; i < edges.length; i++) {
    if (value < edges[i]) return i;
  }
  return edges.length;
}

function histogram(values: readonly number[], edges: readonly number[], unit: string): Bucket[] {
  const buckets: Bucket[] = Array.from({ length: edges.length + 1 }, (_, index) => ({
    label: bucketLabel(edges, index, unit),
    count: 0,
  }));
  for (const value of values) buckets[bucketIndex(value, edges)].count += 1;
  return buckets;
}

/** 端到端耗时分桶（只含有答案的那些） */
export function latencyBuckets(traces: readonly DecisionTrace[]): Bucket[] {
  return histogram(
    traces.filter((trace) => trace.response).map((trace) => trace.totalLatencyMs),
    LATENCY_EDGES_MS,
    "ms",
  );
}

/** 置信度的十分位边界。面板要标出升级阈值落在哪一格，用的就是这把尺 */
const CONFIDENCE_EDGES = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9] as const;

/** 某个阈值落在置信度的第几格（面板拿它高亮那一根条） */
export function confidenceBucketIndex(value: number): number {
  return bucketIndex(value, CONFIDENCE_EDGES);
}

/** 逐次决策的最小置信度分桶：升级阈值该定在哪一档，看这张图而不是拍 */
export function confidenceBuckets(traces: readonly DecisionTrace[]): Bucket[] {
  const values: number[] = [];
  for (const trace of traces) {
    const value = minConfidenceOf(trace);
    if (value !== null) values.push(value);
  }
  return histogram(values, CONFIDENCE_EDGES, "");
}

/**
 * 一次请求的问题名集合。它就是把决策层用在了哪件事上——嵌入函数各问各的问题，
 * 名字不重叠，所以签名足够当分组键。排序是为了 {a,b} 与 {b,a} 归成同一格。
 */
export function traceSignature(trace: DecisionTrace): string {
  return Object.keys(trace.request.questions).sort().join(",");
}

/**
 * 已知嵌入点的中文名。键是排序后的问题名串（与 traceSignature 同口径）。
 * 对不上号的照实在显示问题名——"未知"不是读数，画出来才查得出是谁在调。
 */
const SIGNATURE_LABELS: Record<string, string> = {
  "complexity,needs_premium,task_type": "模型路由",
  worth_remembering: "提取门控",
  "contains_pii,contains_secret,risk_level": "敏感检测",
  "best_agent,can_parallel,priority": "助理分配",
};

/** 上下文相关性一批几个候选就几个问题，签名不固定，按形状认 */
function isContextRelevance(signature: string): boolean {
  return /^cand_\d+(,cand_\d+)*$/.test(signature);
}

export function signatureLabel(signature: string): string {
  const known = SIGNATURE_LABELS[signature];
  if (known) return known;
  if (isContextRelevance(signature)) return "上下文相关性";
  return signature ? signature : "（没有问题）";
}

export interface SignatureStat {
  signature: string;
  label: string;
  count: number;
  failed: number;
  cacheHits: number;
  degraded: number;
  /** 有答案的那些的平均最小置信度；一个答案都没有是 null */
  avgMinConfidence: number | null;
  p95LatencyMs: number;
  /** 这一格实际由哪些层给出过答案（观测用：说好的"钉本地"有没有真钉住） */
  tiers: ModelTier[];
  /** 审计里出现过的最高敏感级别——决定这一格的内容有没有出过本机 */
  maxSensitivity: Sensitivity;
}

export function groupBySignature(traces: readonly DecisionTrace[]): SignatureStat[] {
  const groups = new Map<string, DecisionTrace[]>();
  for (const trace of traces) {
    const key = traceSignature(trace);
    const bucket = groups.get(key);
    if (bucket) bucket.push(trace);
    else groups.set(key, [trace]);
  }
  const rank: Record<Sensitivity, number> = { public: 0, private: 1, confidential: 2 };
  return [...groups.entries()]
    .map(([signature, items]) => {
      const summary = summarizeTraces(items);
      const values: number[] = [];
      for (const trace of items) {
        const value = minConfidenceOf(trace);
        if (value !== null) values.push(value);
      }
      let maxSensitivity: Sensitivity = "public";
      for (const trace of items) {
        const level = trace.request.sensitivity ?? "public";
        if (rank[level] > rank[maxSensitivity]) maxSensitivity = level;
      }
      return {
        signature,
        label: signatureLabel(signature),
        count: items.length,
        failed: summary.failed,
        cacheHits: summary.cacheHits,
        degraded: summary.degraded,
        avgMinConfidence:
          values.length > 0 ? values.reduce((a, b) => a + b, 0) / values.length : null,
        p95LatencyMs: summary.p95LatencyMs,
        tiers: TIERS.filter((tier) => summary.byTier[tier] > 0),
        maxSensitivity,
      };
    })
    .sort((a, b) => b.count - a.count || a.signature.localeCompare(b.signature));
}

export interface RoutingByTaskType {
  taskType: string;
  count: number;
  /** 平均复杂度期望值，量表 trivial..very_complex = 0..4 */
  avgComplexity: number;
  /** 判成"需要大模型"的比例 */
  premiumRate: number;
  /** 这一类任务里，判定达到升级阈值的比例（观测：漏斗有没有真的往上走） */
  aboveThreshold: number;
}

/** 最高一档的 task_type 分布，用于 routeModel 的观测表：模型有没有被换掉是产品决策，这里只摊账 */
export function routingByTaskType(
  traces: readonly DecisionTrace[],
  threshold: number,
): RoutingByTaskType[] {
  interface Row {
    count: number;
    complexity: number;
    premium: number;
    above: number;
  }
  const rows = new Map<string, Row>();
  for (const trace of traces) {
    const response = trace.response;
    if (!response) continue;
    const taskType = response.answers.task_type?.choice;
    if (typeof taskType !== "string") continue; // 没答上任务类型的那些不进这张表
    const row = rows.get(taskType) ?? { count: 0, complexity: 0, premium: 0, above: 0 };
    row.count += 1;
    row.complexity += response.answers.complexity?.score ?? 0;
    if ((response.answers.needs_premium?.noul ?? 0) >= 0.5) row.premium += 1;
    if ((minConfidenceOf(trace) ?? 0) >= threshold) row.above += 1;
    rows.set(taskType, row);
  }
  return [...rows.entries()]
    .map(([taskType, row]) => ({
      taskType,
      count: row.count,
      avgComplexity: row.count > 0 ? row.complexity / row.count : 0,
      premiumRate: row.count > 0 ? row.premium / row.count : 0,
      aboveThreshold: row.count > 0 ? row.above / row.count : 0,
    }))
    .sort((a, b) => b.count - a.count || a.taskType.localeCompare(b.taskType));
}

/**
 * 只留属于这场对话的判定。没带 conversationId 的那些（试一次、记忆分级这类全局治理）
 * 不落进任何一场对话——把它们混进当前话题的流里，等于在念一场没发生过的对话的读数。
 * 没有打开的话题（null）时是空集，不是"全部"。
 */
export function tracesForConversation(
  traces: readonly DecisionTrace[],
  conversationId: string | null,
): DecisionTrace[] {
  if (!conversationId) return [];
  return traces.filter((trace) => trace.request.conversationId === conversationId);
}

/* ---- V2 能力读数（§9：压缩 / 审查 / 检索 / 漏斗升级）----
 * 这些能力不走 router.decide（压缩/审查/检索有自己的判定通道），审计流里没有它们的
 * trace——读数来自 metrics 计数器（跨重启累计），而不是审计快照。
 * 只做聚合整形，计数来源见 metrics.ts。 */

export interface V2MetricsSummary {
  compaction: {
    applied: number;
    identity: number;
    unansweredKeep: number;
    cacheGuardSkip: number;
    rewrite: number;
  };
  review: {
    block: number;
    escalate: number;
    annotate: number;
    allow: number;
    fallbackOpen: number;
  };
  retrieval: {
    performed: number;
    skipped: number;
    cacheHit: number;
  };
  funnel: {
    upgraded: number;
    conservativeDefault: number;
    coalesced: number;
  };
}

function num(metrics: Record<string, number>, key: string): number {
  return metrics[key] ?? 0;
}

export function summarizeV2Metrics(metrics: Record<string, number>): V2MetricsSummary {
  return {
    compaction: {
      applied: num(metrics, "compaction.applied"),
      identity: num(metrics, "compaction.identity"),
      unansweredKeep: num(metrics, "compaction.unanswered_keep"),
      cacheGuardSkip:
        num(metrics, "compaction.cache_guard_no_usage") +
        num(metrics, "compaction.cache_guard_low_hit"),
      rewrite: num(metrics, "compaction.rewrite"),
    },
    review: {
      block: num(metrics, "review.block"),
      escalate: num(metrics, "review.escalate"),
      annotate: num(metrics, "review.annotate"),
      allow: num(metrics, "review.allow"),
      fallbackOpen: num(metrics, "review.fallback_open"),
    },
    retrieval: {
      performed: num(metrics, "retrieval.performed"),
      skipped: num(metrics, "retrieval.skipped"),
      cacheHit: num(metrics, "retrieval.cache_hit"),
    },
    funnel: {
      upgraded: num(metrics, "funnel.upgraded"),
      conservativeDefault: num(metrics, "funnel.conservative_default"),
      coalesced: num(metrics, "router.coalesced"),
    },
  };
}
