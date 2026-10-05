/**
 * Verbatim Compaction 的类型面（design-decision-layer-optimization.md §5.2）。
 *
 * 铁律先行：**给裁判的可以有损，输出必须 verbatim**——压缩只做 delete / truncate，
 * 保留内容的字节一致；未动过的消息以原对象返回（引用相等）。
 * 消息契约是应用自己的 ChatMessage（@/types/chat）：assistant 带 toolCalls，
 * 工具结果是 role:"tool" 的消息、用 toolCallId 关联调用。
 */
import type { ChatMessage } from "@/types/chat";

/** 一次判定的三种下场。第四种（unanswered）不是下场，是 keep 的理由 */
export type CallVerdict = "keep" | "drop_result" | "drop_call";

/** 配对好的一次候选：assistant 消息里的调用 + 它的 tool 结果消息 */
export interface ToolCallPair {
  /** toolCalls[].id，也是 tool 消息的 toolCallId */
  callId: string;
  tool: string;
  input: string;
  /** 结果消息在 messages 数组里的下标（重写时按它定位） */
  resultIndex: number;
  resultContent: string;
  isError: boolean;
  /** 调用消息（assistant）在 messages 数组里的下标 */
  callIndex: number;
}

/** 单个调用（或合并组）的判定来源 */
export interface VerdictRecord {
  verdict: CallVerdict;
  /** unanswered = 批次失败/缺答的保守判定（D1），单独计数供监控读数 */
  reason?: "unanswered" | "judged" | "pinned" | "sticky";
}

export interface CompactionStats {
  originalChars: number;
  compactedChars: number;
  /** compacted / original；identity 时是 1 */
  reductionRatio: number;
  /** 六级降档命中的档位名（full / inputs200 / inputs60 / abridged / collapsed / callsCompacted / leftOut / overflow） */
  stateStage: string;
  batches: number;
  /** D1 的缺答保守判定个数：持续 > 0 说明裁判响应质量有问题 */
  unansweredKeeps: number;
  /** D6 的重复调用合并组数（每组一次判定替代 N 次） */
  mergedCallGroups: number;
  estimatedStateTokens: number;
  /** budget-fit 闸：剪完的历史是否补上触发压缩时的预算缺口 */
  budgetFit: boolean;
}

export interface CompactionResult {
  /** 重写后的历史。未动的消息是**原对象引用**（身份保持契约） */
  messages: ChatMessage[];
  /** true = 一个字节都没动（不值得 / 全链不可用 / budget 不 fit） */
  identity: boolean;
  /** true = 决策全链不可用导致跳过（§3 轮级保守默认：绝不摘要、绝不硬失败） */
  degraded?: boolean;
  /** Cache Guard 的跳过原因（§5.2：读不到 usage = 无法验证缓存经济性 = 不删） */
  skippedByCacheGuard?: "no_usage" | "low_hit";
  stats: CompactionStats;
}

/** 裁判的传输注入点（JevAsker）。压缩不直接依赖路由器：调用方把「state+questions → 答案」
 *  的通道递进来（生产是决策层的 Jev 链 / 敏感时的 Laya，测试是脚本桩） */
export interface CompactionAsker {
  (state: string, questions: Record<string, Question4Compaction>): Promise<CompactionAskResult>;
}

export interface Question4Compaction {
  type: "noul";
  instructions: string;
}

export interface CompactionAskResult {
  /** questionName → P(true)。缺答的调用按 D1 判 keep */
  answers: Record<string, number>;
  /** provider usage（归一化）。Cache Guard 吃它：读不到 → 后续批次不再发 */
  usage?: { promptTokens: number; cachedTokens: number };
}

export interface CompactionOptions {
  /** Sticky 复用 replacement 映射（默认开，config.compaction.sticky 控制） */
  sticky?: boolean;
  /** 显式 goal 文本；缺省自动取最近 GOAL_RECENT_USER_MESSAGES 条用户消息 */
  goal?: string;
  /** budget-fit 闸的预算线（字符）：触发压缩时的历史预算。缺省不启用闸 */
  budgetChars?: number;
  /** state 装配前的脱敏注入点（D8）。confidential 的红线在路由层，这里是内容级 */
  redact?: (entry: { kind: "input" | "result" | "text"; content: string }) => string;
  /** Cache Guard 开关。默认开（读不到 usage 的 provider 会让压缩恒跳过——保守拍板） */
  cacheGuard?: boolean;
  /** 敏感性：private/confidential 时调用方必须把裁判钉在本地（Laya） */
  sensitivity?: "public" | "private" | "confidential";
}
