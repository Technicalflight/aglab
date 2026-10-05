/**
 * 判定构造与应用（design-decision-layer-optimization.md §5.2 + §2.1 D4/D5/D6）。
 *
 * 双阈值的方向（D4）：keepResult 0.6 偏高（偏向截断）、keepCall 0.4 偏低（偏向保记录）
 * ——丢结果可逆（工具可重跑），丢调用抹掉「试过什么」的记录，两头都朝保守移。
 * D5：keep_result 的指令写明错误状态——错误原文是最常需要 verbatim 保留的东西。
 * D6：同 (tool, 规范化 input) 且结果字符量级相近的调用并成一问，判定应用全组。
 */
import {
  KEEP_CALL_THRESHOLD,
  KEEP_RESULT_THRESHOLD,
  PRESERVE_RECENT_MESSAGES,
  TRUNCATE_HEAD_CHARS,
} from "../constants";
import { fnv1a } from "../cache";
import type { ChatMessage } from "@/types/chat";
import type { Question4Compaction, ToolCallPair, VerdictRecord } from "./types";
import { type StateStage } from "./assembly";

/** 配对好的候选：有结果、且不在钉扎区（首条 + 最近 N 条内）的调用对 */
export function findCallPairs(messages: ChatMessage[]): ToolCallPair[] {
  const recentStart = Math.max(1, messages.length - PRESERVE_RECENT_MESSAGES);
  const resultsByCallId = new Map<string, { index: number; message: ChatMessage }>();
  for (let index = 0; index < messages.length; index++) {
    const message = messages[index];
    if (message.role === "tool" && typeof message.toolCallId === "string" && message.toolCallId !== "") {
      resultsByCallId.set(message.toolCallId, { index, message });
    }
  }
  const pairs: ToolCallPair[] = [];
  for (let index = 0; index < messages.length; index++) {
    const message = messages[index];
    if (message.role !== "assistant" || !message.toolCalls) continue;
    // 钉扎：首条消息与最近区内的调用不做候选（「试过什么」的最近语境不可抹）
    const pinned = index === 0 || index >= recentStart;
    for (const call of message.toolCalls) {
      const result = resultsByCallId.get(call.id);
      if (!result || pinned) continue;
      const isError =
        /error|failed|失败/i.test(result.message.content.slice(0, 80)) || /error|failed|失败/i.test(call.arguments ?? "");
      pairs.push({
        callId: call.id,
        tool: call.name,
        input: call.arguments ?? "",
        resultIndex: result.index,
        resultContent: result.message.content,
        isError,
        callIndex: index,
      });
    }
  }
  return pairs;
}

/** 输入规范化：能 parse 的 JSON 键序归一后比对；不能 parse 的退回 trim 原文 */
function normalizeInput(input: string): string {
  try {
    const parsed: unknown = JSON.parse(input);
    if (typeof parsed === "object" && parsed !== null) {
      return stableStringifyForCompare(parsed);
    }
    return String(parsed).trim();
  } catch {
    return input.trim();
  }
}

function stableStringifyForCompare(value: unknown): string {
  if (value === null || typeof value !== "object") return JSON.stringify(value) ?? "null";
  if (Array.isArray(value)) return `[${value.map(stableStringifyForCompare).join(",")}]`;
  const entries = Object.entries(value as Record<string, unknown>)
    .filter(([, v]) => v !== undefined)
    .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
  return `{${entries.map(([k, v]) => `${JSON.stringify(k)}:${stableStringifyForCompare(v)}`).join(",")}}`;
}

/** 字符量级相近：同数量级（对数差 < 1）就算，防止「同输入但一次失败一次成功」误并到失真 */
function sameMagnitude(a: number, b: number): boolean {
  if (a === 0 && b === 0) return true;
  if (a === 0 || b === 0) return false;
  return Math.abs(Math.log10(a) - Math.log10(b)) < 1;
}

/** D6 的重复调用组：并组的前提是工具相同、输入规范化后一致、结果量级相近 */
export interface DuplicateGroup {
  key: string;
  pairs: ToolCallPair[];
  /** 组内最多的一次错误状态（并组指令里写明「此调用发生了 N 次」） */
  isError: boolean;
}

export function groupDuplicates(pairs: ToolCallPair[]): DuplicateGroup[] {
  const groups = new Map<string, ToolCallPair[]>();
  for (const pair of pairs) {
    const key = `${pair.tool}::${normalizeInput(pair.input)}`;
    const bucket = groups.get(key);
    if (bucket) bucket.push(pair);
    else groups.set(key, [pair]);
  }
  // 量级校验放在同 key 组内：不同量级的结果拆回单元素组（判定不并，删除仍可并）
  const out: DuplicateGroup[] = [];
  for (const [key, members] of groups) {
    const buckets: ToolCallPair[][] = [];
    for (const pair of members) {
      const bucket = buckets.find((b) => sameMagnitude(b[0].resultContent.length, pair.resultContent.length));
      if (bucket) bucket.push(pair);
      else buckets.push([pair]);
    }
    for (const bucket of buckets) {
      out.push({
        key: `${key}#${fnv1a(bucket.map((p) => p.callId).join("|")).slice(0, 8)}`,
        pairs: bucket,
        isError: bucket.some((p) => p.isError),
      });
    }
  }
  return out;
}

/** 判定问题：每组两问。指令写明错误状态（D5）与发生次数（D6） */
export function buildQuestions(groups: DuplicateGroup[]): Record<string, Question4Compaction> {
  const questions: Record<string, Question4Compaction> = {};
  for (const group of groups) {
    const times = group.pairs.length > 1 ? ` This exact call happened ${group.pairs.length} times with the same input and similar result size.` : "";
    const status = group.isError ? "The result reports an ERROR." : "The result is a normal success.";
    questions[`kr_${group.key}`] = {
      type: "noul",
      instructions: `${status}${times} Does the conversation STILL NEED the full verbatim text of this tool result to continue correctly?`,
    };
    questions[`kc_${group.key}`] = {
      type: "noul",
      instructions: `${status}${times} Does the conversation still need the RECORD that this tool was called with its input (the result text itself can be truncated)?`,
    };
  }
  return questions;
}

/** 逐问拆出 P(true)。缺答返回 null（D1 的保守判定由编排层落 keep） */
export function readVerdicts(
  groups: DuplicateGroup[],
  answers: Record<string, number>,
): Map<string, VerdictRecord> {
  const verdicts = new Map<string, VerdictRecord>();
  for (const group of groups) {
    const keepResult = answers[`kr_${group.key}`];
    const keepCall = answers[`kc_${group.key}`];
    if (typeof keepResult !== "number" || typeof keepCall !== "number") {
      verdicts.set(group.key, { verdict: "keep", reason: "unanswered" });
      continue;
    }
    if (keepResult >= KEEP_RESULT_THRESHOLD) {
      verdicts.set(group.key, { verdict: "keep", reason: "judged" });
    } else if (keepCall >= KEEP_CALL_THRESHOLD) {
      verdicts.set(group.key, { verdict: "drop_result", reason: "judged" });
    } else {
      verdicts.set(group.key, { verdict: "drop_call", reason: "judged" });
    }
  }
  return verdicts;
}

/**
 * 把判定写回历史。身份保持契约：keep 的消息一律原对象；只有 drop_result 的
 * 结果消息和 drop_call 触及的 assistant 消息产生新对象；结果不落单——
 * drop_call 连调用带结果一起走（数组里直接移除结果消息）。
 */
export function applyVerdicts(
  messages: ChatMessage[],
  groups: DuplicateGroup[],
  verdicts: Map<string, VerdictRecord>,
): { messages: ChatMessage[]; touched: number } {
  const dropResults = new Map<number, number>(); // resultIndex → 截断后长度参照
  const dropCalls = new Map<number, Set<string>>(); // callIndex → 移除的 callIds
  const removedResults = new Set<number>();
  let touched = 0;
  for (const group of groups) {
    const verdict = verdicts.get(group.key);
    if (!verdict) continue;
    if (verdict.verdict === "keep") continue;
    if (verdict.verdict === "drop_result") {
      for (const pair of group.pairs) dropResults.set(pair.resultIndex, pair.resultContent.length);
    } else {
      for (const pair of group.pairs) {
        removedResults.add(pair.resultIndex);
        const set = dropCalls.get(pair.callIndex) ?? new Set<string>();
        set.add(pair.callId);
        dropCalls.set(pair.callIndex, set);
      }
    }
    touched += group.pairs.length;
  }
  const out: ChatMessage[] = [];
  for (let index = 0; index < messages.length; index++) {
    const message = messages[index];
    if (removedResults.has(index)) continue; // drop_call：结果连调用一起走
    const truncateTo = dropResults.get(index);
    if (truncateTo !== undefined && message.role === "tool") {
      out.push({
        ...message,
        content: `${message.content.slice(0, TRUNCATE_HEAD_CHARS)}[truncated: original ${truncateTo} chars, re-run the tool if needed]`,
      });
      continue;
    }
    const dropIds = dropCalls.get(index);
    if (dropIds && message.toolCalls) {
      const remaining = message.toolCalls.filter((call) => !dropIds.has(call.id));
      if (remaining.length === 0) continue; // 纯调用消息：全组消失后消息也没有存在意义
      out.push({ ...message, toolCalls: remaining });
      continue;
    }
    out.push(message); // 未动：原对象引用
  }
  return { messages: out, touched };
}

/** 剪枝后历史重算（budget-fit 闸用）：字符口径 */
export function totalChars(messages: ChatMessage[]): number {
  let sum = 0;
  for (const message of messages) {
    sum += message.content.length;
    if (message.toolCalls) for (const call of message.toolCalls) sum += (call.arguments ?? "").length;
  }
  return sum;
}

export type { StateStage };
