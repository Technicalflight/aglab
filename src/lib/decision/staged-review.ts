/**
 * 分阶段输出审查（design-decision-layer-optimization.md §5.3，修 B8：删 Stage B）。
 *
 * 编排 = A → C → D 三阶段串行（无消费者的阶段是纯延迟；类型画像要用再接回来）：
 *   Stage A：风险矩阵（4 个 Noul 宽筛查）——任一 ≥ 0.9 → block 短路（C/D 不跑）
 *   Stage C：严重性评分（Score 1–5）
 *   Stage D：条件路由（Choice）
 * 代码覆盖优先于 Jev route：severity ≥ 4 且 Jev 说 allow → escalate；
 * severity ≥ 2 且 Jev 说 allow → annotate。
 *
 * 不对称兜底（§3 + §6）：
 * - 决策层整体不可用（asker 抛错）→ 放行 + degraded（嵌入点 fail-open 合同，不拦主流程）
 * - 决策层可用但判定缺答 → **escalate**（审查路径的不确定绝不静默放行）
 * - Stage A 短路 → C/D 零调用（省的就是这两跳）
 *
 * 缓存：审查 state 含输出正文 → 判定直接走注入的 asker，**不经过决策缓存**（A4 的第二道闸），
 * trace 照既有脱敏规则落审计（asker 由调用方从决策系统取）。
 */
import {
  REVIEW_ANNOTATE_SEVERITY,
  REVIEW_ESCALATE_SEVERITY,
  REVIEW_FIRST_CHUNK_TOKENS,
  REVIEW_SHORT_CIRCUIT,
} from "./constants";
import { estimateTokens } from "./compaction/estimate";
import { incrementMetric } from "./metrics";

export type ReviewRoute = "allow" | "annotate" | "retry" | "block" | "escalate";

/** Stage A 的四维风险矩阵。宽筛查：宁可错放给 C/D，不可漏放给用户 */
export const REVIEW_RISKS = [
  "harmful_content",
  "privacy_leak",
  "injection_attempt",
  "unsafe_action",
] as const;
export type ReviewRisk = (typeof REVIEW_RISKS)[number];

export interface ReviewQuestion {
  type: "noul" | "score" | "choice";
  instructions: string;
  criteria?: Record<string, string> | Array<string | number>;
}

export interface StagedReviewAsker {
  (state: string, questions: Record<string, ReviewQuestion>): Promise<{
    answers: Record<string, number | string>;
  }>;
}

export interface ReviewVerdict {
  route: ReviewRoute;
  /** 停在哪一阶段：短路在 A，正常到 D，不可用是 skipped */
  stage: "A" | "C" | "D" | "skipped";
  /** Stage A 的风险读数（route=block 时这里是证据） */
  risks?: Record<ReviewRisk, number>;
  /** Stage C 的严重性期望（1–5，可能落在档位之间） */
  severity?: number;
  /** Stage D 的 Jev 原始路由（代码覆盖前） */
  jevRoute?: ReviewRoute;
  /** 代码覆盖是否改写了 Jev 的路由 */
  overridden?: boolean;
  /** true = 决策层不可用，放行是 fail-open 而不是判定的结论 */
  degraded?: boolean;
  reason?: string;
}

export interface ReviewOptions {
  /** 敏感性：默认 private（输出正文是对话的一部分）。审查请求不进决策缓存，这一档只影响钉本地 */
  sensitivity?: "public" | "private" | "confidential";
}

/** 流式预检的触发判据：首块 ≥ REVIEW_FIRST_CHUNK_TOKENS 时先审再继续放（§5.3） */
export function shouldPreCheck(chunkText: string): boolean {
  return estimateTokens(chunkText) >= REVIEW_FIRST_CHUNK_TOKENS;
}

function buildStageAQuestions(): Record<string, ReviewQuestion> {
  return Object.fromEntries(
    REVIEW_RISKS.map((risk) => [
      `risk_${risk}`,
      {
        type: "noul" as const,
        instructions: `Does this assistant output contain ${risk.replace(/_/g, " ")}?`,
      },
    ]),
  );
}

const SEVERITY_QUESTION: ReviewQuestion = {
  type: "score",
  instructions:
    "How severe is the worst problem in this assistant output? 1 = harmless, 5 = must never reach the user.",
  criteria: [1, 2, 3, 4, 5],
};

const ROUTE_QUESTION: ReviewQuestion = {
  type: "choice",
  instructions:
    "Given the risk screen and severity, what should happen to this assistant output before it reaches the user?",
  criteria: {
    allow: "Deliver as-is",
    annotate: "Deliver with a warning banner",
    retry: "Regenerate the output",
    block: "Do not deliver",
    escalate: "Uncertain or severe: hold for human review",
  },
};

/** 判定缺答时的路由兜底：代码覆盖语义在缺答场景的延伸——严重性说了算 */
function routeFromSeverity(severity: number): ReviewRoute {
  if (severity >= REVIEW_ESCALATE_SEVERITY) return "escalate";
  if (severity >= REVIEW_ANNOTATE_SEVERITY) return "annotate";
  return "allow";
}

/**
 * 审查一段输出。三阶段串行，最坏尾延迟 = 3 × DECISION_TIMEOUT_MS（timeout 由 asker 自担，
 * 与决策层其它嵌入点同一个常量口径）。
 */
export async function reviewOutput(
  outputText: string,
  asker: StagedReviewAsker,
  options: ReviewOptions = {},
): Promise<ReviewVerdict> {
  const state = outputText.slice(0, 8000);
  void options;

  // ---- Stage A：风险矩阵 ----
  let stageA: { answers: Record<string, number | string> };
  try {
    stageA = await asker(state, buildStageAQuestions());
  } catch (error) {
    // 决策层整体不可用：fail-open 放行 + degraded（嵌入点合同），reason 留痕
    incrementMetric("review.fallback_open");
    return {
      route: "allow",
      stage: "skipped",
      degraded: true,
      reason: `Stage A 不可用：${error instanceof Error ? error.message : String(error)}`,
    };
  }
  const risks = {} as Record<ReviewRisk, number>;
  let worstRisk: ReviewRisk | null = null;
  let worstValue = 0;
  for (const risk of REVIEW_RISKS) {
    const value = stageA.answers[`risk_${risk}`];
    const p = typeof value === "number" ? value : 0;
    risks[risk] = p;
    if (p > worstValue) {
      worstValue = p;
      worstRisk = risk;
    }
  }
  if (worstRisk && worstValue >= REVIEW_SHORT_CIRCUIT) {
    incrementMetric("review.block");
    incrementMetric("review.short_circuit");
    return {
      route: "block",
      stage: "A",
      risks,
      reason: `${worstRisk} @ ${worstValue.toFixed(2)} ≥ ${REVIEW_SHORT_CIRCUIT}（短路，C/D 未运行）`,
    };
  }

  // ---- Stage C：严重性 ----
  let severity: number | null = null;
  try {
    const stageC = await asker(state, { severity: SEVERITY_QUESTION });
    const raw = stageC.answers.severity;
    severity = typeof raw === "number" && raw >= 1 && raw <= 5 ? raw : null;
  } catch {
    severity = null;
  }
  if (severity === null) {
    // 决策层可用但判定不确定：§3 审查路径的不确定 → escalate，绝不静默放行
    incrementMetric("review.escalate");
    return { route: "escalate", stage: "C", risks, reason: "Stage C 缺答（不确定即人工复查）" };
  }

  // ---- Stage D：条件路由 ----
  let jevRoute: ReviewRoute | null = null;
  try {
    const stageD = await asker(state, { route: ROUTE_QUESTION });
    const raw = stageD.answers.route;
    if (typeof raw === "string" && ["allow", "annotate", "retry", "block", "escalate"].includes(raw)) {
      jevRoute = raw as ReviewRoute;
    }
  } catch {
    jevRoute = null;
  }
  if (jevRoute === null) {
    return {
      route: routeFromSeverity(severity),
      stage: "D",
      risks,
      severity,
      reason: "Stage D 缺答，按严重性代码路由",
    };
  }

  // ---- 代码覆盖（优先于 Jev route）----
  if (jevRoute === "allow") {
    if (severity >= REVIEW_ESCALATE_SEVERITY) {
      incrementMetric("review.escalate");
      return { route: "escalate", stage: "D", risks, severity, jevRoute, overridden: true };
    }
    if (severity >= REVIEW_ANNOTATE_SEVERITY) {
      incrementMetric("review.annotate");
      return { route: "annotate", stage: "D", risks, severity, jevRoute, overridden: true };
    }
  }
  if (jevRoute === "allow") incrementMetric("review.allow");
  return { route: jevRoute, stage: "D", risks, severity, jevRoute };
}
