/**
 * Verbatim Compaction 编排（design-decision-layer-optimization.md §5.2）。
 *
 * 与参考原型 fast-jev-compaction 的关键偏离（§2.1）：
 * - D1：Promise.allSettled + 失败批重试一次；仍失败/缺答的调用判定 keep（reason: unanswered），
 *   绝不整次作废——删减不可逆，不确定即保守。
 * - D2：Jev 全保不否决剪枝——判据是「剪完的历史是否补上预算缺口」（budget-fit 闸），
 *   缩减率只是告警指标。
 * - D7：state 恒为请求体第一字段且跨批逐字节一致（装配一次、多批共用）。
 * - Cache Guard 是响应驱动的顺序闸：前一批的 usage 决定下一批发不发——
 *   并发执行会让它形同虚设，所以批次是**顺序**的（经济性保险丝优先于延迟优化）。
 * - 轮级保守默认（§3）：全链不可用 → identity + degraded，绝不摘要、绝不硬失败。
 */
import { CACHE_GUARD_CEILING, COMPACT_BATCH_RETRIES, MAX_REQUEST_TOKENS } from "../constants";
import type { ChatMessage } from "@/types/chat";
import { assembleState } from "./assembly";
import { applyVerdicts, buildQuestions, findCallPairs, groupDuplicates, readVerdicts, totalChars } from "./judge";
import { incrementMetric } from "../metrics";
import type { CallVerdict, CompactionAsker, CompactionOptions, CompactionResult, VerdictRecord } from "./types";
import type { DuplicateGroup } from "./judge";

/* ---- Sticky replacement 映射（§5.2：决策缓存按 sessionId 复用判定）---- */

interface StickyEntry {
  /** callId → 判定。判过的调用不再问裁判 */
  verdicts: Map<string, { verdict: CallVerdict; reason: VerdictRecord["reason"] }>;
  requestsSinceRewrite: number;
  /** 上次 rewrite 时的消息条数（增长 +40% 的分母） */
  baseMessageCount: number;
}

const stickyStore = new Map<string, StickyEntry>();

/** 测试与配置关闭 Sticky 时清场 */
export function clearStickyStore(): void {
  stickyStore.clear();
}

function rewriteSticky(entry: StickyEntry, liveCallIds: Set<string>, currentMessageCount: number): void {
  // rewrite = 清理已不在历史里的判定 + 重置计数（映射膨胀/漂移的控制点）
  for (const callId of [...entry.verdicts.keys()]) {
    if (!liveCallIds.has(callId)) entry.verdicts.delete(callId);
  }
  entry.requestsSinceRewrite = 0;
  entry.baseMessageCount = currentMessageCount;
  incrementMetric("compaction.rewrite");
}

/* ---- 批次切分 ---- */

/**
 * 每批装多少组：预算 = MAX_REQUEST_TOKENS − state 的估算；
 * 每组两问（kr_/kc_），连指令文本按 ~1200 token/组 从宽预留。
 * state 本身超预算时至少装 1 组（state 是裁判理解问题的前提，不能裁它）。
 */
function batchSizeFor(stateTokens: number, groupCount: number): number {
  const budget = MAX_REQUEST_TOKENS - stateTokens;
  if (budget <= 0) return 1;
  return Math.min(groupCount, Math.max(1, Math.floor(budget / 1200)));
}

/** 主入口。签名按 A2 裁定：sessionId 显式传入（Sticky 以它为键） */
export async function compact(
  sessionId: string,
  messages: ChatMessage[],
  asker: CompactionAsker,
  options: CompactionOptions = {},
): Promise<CompactionResult> {
  const originalChars = totalChars(messages);
  const stats = {
    originalChars,
    compactedChars: originalChars,
    reductionRatio: 1,
    stateStage: "full",
    batches: 0,
    unansweredKeeps: 0,
    mergedCallGroups: 0,
    estimatedStateTokens: 0,
    budgetFit: false,
  };
  const identity = (extra: Partial<CompactionResult> = {}): CompactionResult => ({
    messages,
    identity: true,
    stats: { ...stats, compactedChars: totalChars(messages), reductionRatio: 1 },
    ...extra,
  });

  // 1) 候选识别 + 重复合并（D6）
  const pairs = findCallPairs(messages);
  if (pairs.length === 0) return identity();
  const groups = groupDuplicates(pairs);
  stats.mergedCallGroups = groups.filter((group) => group.pairs.length > 1).length;

  // 2) Sticky：判过的调用直接复用判定，只对没判过的组发问
  const useSticky = options.sticky !== false;
  const sticky = useSticky
    ? stickyStore.get(sessionId) ?? { verdicts: new Map(), requestsSinceRewrite: 0, baseMessageCount: messages.length }
    : null;
  if (useSticky && !stickyStore.has(sessionId)) stickyStore.set(sessionId, sticky!);

  const pending: DuplicateGroup[] = [];
  if (sticky) {
    for (const group of groups) {
      const allJudged = group.pairs.every((pair) => sticky.verdicts.has(pair.callId));
      if (allJudged) continue;
      pending.push(group);
    }
  } else {
    pending.push(...groups);
  }

  // 3) 全部判定都有（Sticky 全命中）→ 零请求，直接应用
  let verdictsByGroup = new Map<string, VerdictRecord>();
  let skippedByCacheGuard: "no_usage" | "low_hit" | null = null;
  if (pending.length === 0) {
    for (const group of groups) {
      const verdict = sticky!.verdicts.get(group.pairs[0].callId)!;
      verdictsByGroup.set(group.key, { verdict: verdict.verdict, reason: "sticky" });
    }
  } else {
    // 4) state 装配（六级降档；D7：装配一次、跨批逐字节一致）
    const assembled = assembleState({ messages, redact: options.redact });
    stats.stateStage = assembled.stage;
    stats.estimatedStateTokens = assembled.estimatedTokens;
    if (assembled.stage === "overflow") {
      // 全档装不下：本轮跳过压缩（原型在此抛错交宿主摘要，本实现绝不——降级即不压）
      return identity({ degraded: true });
    }
    const perBatch = batchSizeFor(assembled.estimatedTokens, pending.length);
    const batches: DuplicateGroup[][] = [];
    for (let index = 0; index < pending.length; index += perBatch) {
      batches.push(pending.slice(index, index + perBatch));
    }

    // 5) 顺序执行批次（Cache Guard 响应驱动），失败批重试一次（D1）
    const cacheGuard = options.cacheGuard !== false;
    for (const batch of batches) {
      if (skippedByCacheGuard) break;
      stats.batches += 1;
      const batchQuestions: Record<string, { type: "noul"; instructions: string }> = {};
      for (const group of batch) {
        const all = buildQuestions([group]);
        Object.assign(batchQuestions, all);
      }
      let answers: Record<string, number> | null = null;
      let usage: { promptTokens: number; cachedTokens: number } | undefined;
      let lastError: unknown = null;
      for (let attempt = 0; attempt <= COMPACT_BATCH_RETRIES; attempt++) {
        try {
          const result = await asker(assembled.state, batchQuestions);
          answers = result.answers;
          usage = result.usage;
          lastError = null;
          break;
        } catch (error) {
          lastError = error;
        }
      }
      if (lastError !== null || answers === null) {
        // D1：批次失败 → 该批全部判定 keep（unanswered），绝不 throw 整次作废
        for (const group of batch) verdictsByGroup.set(group.key, { verdict: "keep", reason: "unanswered" });
        stats.unansweredKeeps += batch.length;
        continue;
      }
      // Cache Guard：读不到 usage = 无法验证缓存经济性 → 剩余批次不发（本批已确定的成果保留）
      if (cacheGuard) {
        if (!usage || typeof usage.promptTokens !== "number" || usage.promptTokens <= 0) {
          skippedByCacheGuard = "no_usage";
        } else if (usage.cachedTokens / usage.promptTokens < CACHE_GUARD_CEILING) {
          skippedByCacheGuard = "low_hit";
        }
      }
      const batchVerdicts = readVerdicts(batch, answers);
      for (const [key, verdict] of batchVerdicts) {
        verdictsByGroup.set(key, verdict);
        if (verdict.reason === "unanswered") stats.unansweredKeeps += 1;
        if (sticky && verdict.reason !== "unanswered") {
          const group = batch.find((g) => g.key === key);
          if (group) {
            for (const pair of group.pairs) sticky.verdicts.set(pair.callId, { verdict: verdict.verdict, reason: verdict.reason });
          }
        }
      }
    }
    if (skippedByCacheGuard) {
      // 被 guard 截断的未判定组：全部 keep（unanswered 计数）。已完成的批次成果保留——
      // 它们是确定性的判定，作废才是浪费（保守 ≠ 浪费）
      for (const group of pending) {
        if (!verdictsByGroup.has(group.key)) {
          verdictsByGroup.set(group.key, { verdict: "keep", reason: "unanswered" });
          stats.unansweredKeeps += 1;
        }
      }
    }
  }

  // Sticky 已判的组补进判定表（部分命中路径：pending 只是没判过的那部分）
  if (sticky) {
    for (const group of groups) {
      if (verdictsByGroup.has(group.key)) continue;
      const cached = sticky.verdicts.get(group.pairs[0].callId);
      if (cached) verdictsByGroup.set(group.key, { verdict: cached.verdict, reason: "sticky" });
    }
  }

  // 6) 判定写回历史（身份保持：未动的原对象返回）
  const applied = applyVerdicts(messages, groups, verdictsByGroup);
  const compactedMessages = applied.messages;

  // 7) budget-fit 闸（D2）：剪完的历史要补上触发压缩时的预算缺口，否则整轮不采纳。
  //    没传 budgetChars 的调用方没有预算语义，闸不启用（缩减率不做门槛）
  let budgetFit = true;
  const compactedChars = totalChars(compactedMessages);
  if (options.budgetChars !== undefined && originalChars > options.budgetChars) {
    budgetFit = compactedChars <= options.budgetChars;
  }

  // Sticky 的 rewrite 簿记（增长 +40% 且 ≥15 发，或 40 发封顶）
  if (sticky) {
    sticky.requestsSinceRewrite += 1;
    const grown = messages.length > sticky.baseMessageCount * 1.4;
    if (
      (grown && sticky.requestsSinceRewrite >= 15) ||
      sticky.requestsSinceRewrite >= 40
    ) {
      const liveCallIds = new Set<string>();
      for (const group of groups) for (const pair of group.pairs) liveCallIds.add(pair.callId);
      rewriteSticky(sticky, liveCallIds, messages.length);
    }
  }

  if (!budgetFit) return identity();

  // §9 指标：applied / 兜底 / unanswered / cache-guard / rewrite 各自计数，跨重启累计
  if (applied.touched > 0) incrementMetric("compaction.applied");
  else incrementMetric("compaction.identity");
  if (stats.unansweredKeeps > 0) incrementMetric("compaction.unanswered_keep", stats.unansweredKeeps);
  if (skippedByCacheGuard) incrementMetric(`compaction.cache_guard_${skippedByCacheGuard}`);

  return {
    messages: compactedMessages,
    identity: applied.touched === 0,
    skippedByCacheGuard: skippedByCacheGuard ?? undefined,
    stats: {
      ...stats,
      compactedChars,
      reductionRatio: originalChars > 0 ? compactedChars / originalChars : 1,
      budgetFit,
    },
  };
}

export type { DuplicateGroup };
