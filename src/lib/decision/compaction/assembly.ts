/**
 * state 装配：六级降档（design-decision-layer-optimization.md §5.2）。
 *
 * 「给裁判的可以有损，输出必须 verbatim」在装配层的含义：state 里塞的是
 * 摘要化的历史（结果只给短注），裁判据此判断哪些调用还需要 verbatim 保留——
 * 它不需要读结果全文才能做这个判断（调用的输入、错误状态、字符量级足够）。
 * 逐级启用直到装进 MAX_STATE_TOKENS；全档仍装不下 → overflow（本轮跳过压缩）。
 *
 * 老消息 = 不是首条、也不在最近 PRESERVE_RECENT_MESSAGES 条里的消息。
 * 钉扎区（首条 + 最近区）的有损永远晚于老消息（「钉扎的最后动」）。
 */
import {
  ABRIDGE_HEAD_CHARS,
  ABRIDGE_TAIL_CHARS,
  GOAL_MESSAGE_MAX_CHARS,
  GOAL_RECENT_USER_MESSAGES,
  INPUT_TRUNCATE_LEVELS,
  MAX_STATE_TOKENS,
  PRESERVE_RECENT_MESSAGES,
} from "../constants";
import type { ChatMessage } from "@/types/chat";
import { estimateTokens } from "./estimate";

export const STATE_STAGES = [
  "full",
  "inputs200",
  "inputs60",
  "abridged",
  "collapsed",
  "callsCompacted",
  "leftOut",
] as const;
export type StateStage = (typeof STATE_STAGES)[number];

/** 装配的输入：消息 + 自动 goal（调用方也可显式传） */
export interface AssemblyInput {
  messages: ChatMessage[];
  goal?: string;
  redact?: (entry: { kind: "input" | "result" | "text"; content: string }) => string;
}

export interface AssemblyOutput {
  state: string;
  /** 命中的档位；overflow = 全档都装不下（编排层据此跳过本轮压缩） */
  stage: StateStage | "overflow";
  estimatedTokens: number;
}

function clip(content: string, head: number, tail: number): string {
  if (content.length <= head + tail) return content;
  return `${content.slice(0, head)}[…${content.length - head - tail} chars omitted…]${content.slice(-tail)}`;
}

/** 自动 goal：最近几条用户消息，各截一段——锚定「后续还需要什么」 */
export function autoGoal(messages: ChatMessage[]): string {
  const userContents = messages
    .filter((m) => m.role === "user" && m.content.trim() !== "")
    .map((m) => m.content);
  const recent = userContents.slice(-GOAL_RECENT_USER_MESSAGES);
  if (recent.length === 0) return "";
  return recent.map((content) => content.slice(0, GOAL_MESSAGE_MAX_CHARS)).join("\n---\n");
}

interface RenderOptions {
  inputLimit: number | null;
  oldTextMode: "full" | "abridge" | "collapse" | "drop";
  recentTextMode: "full" | "abridge";
  oldCallsMode: "full" | "compact";
}

/**
 * 渲染一份 state 文本。行格式：`#<下标> <role>: <正文>`；
 * assistant 的每次调用一行 `tool_call <id> <tool>(<input>)`，结果行 `result <id>: <短注>`。
 * 输入/正文经 redact 注入点（D8）后才落进 state。
 */
function renderState(
  messages: ChatMessage[],
  options: RenderOptions,
  redact?: AssemblyInput["redact"],
): string {
  const recentStart = Math.max(1, messages.length - PRESERVE_RECENT_MESSAGES);
  const lines: string[] = [];
  for (let index = 0; index < messages.length; index++) {
    const message = messages[index];
    // tool 结果消息的正文**永不**进 state（full 档也不进）——它的存在与状态由
    // tool_call 渲染时附的短注行表达。「给裁判的可以有损」从这里开始
    if (message.role === "tool") continue;
    // 老消息 = 不是首条、且落在最近区**之前**的（recentStart 之后的是最近区）
    const isOld = index < recentStart && index !== 0;
    const textMode = isOld ? options.oldTextMode : options.recentTextMode;
    if (textMode === "drop" && (message.toolCalls?.length ?? 0) === 0) continue;

    const role = message.role;
    let body = message.content;
    if (textMode === "abridge") body = clip(body, ABRIDGE_HEAD_CHARS, ABRIDGE_TAIL_CHARS);
    if (textMode === "collapse") body = body.length > 0 ? `[…${body.length} chars omitted…]` : "";
    if (body.length > 0) {
      const text = textMode === "full" ? body : body;
      lines.push(`#${index} ${role}: ${redact ? redact({ kind: "text", content: text }) : text}`);
    }

    if (role === "assistant" && message.toolCalls) {
      for (const call of message.toolCalls) {
        if (options.oldCallsMode === "compact" && isOld) {
          const note = renderResultNoteShort(messages, call.id);
          lines.push(
            `#${index} tool_call ${call.id} ${call.name}(${clip(call.arguments ?? "", 40, 0)}) → ${note}`,
          );
          continue;
        }
        const input =
          options.inputLimit !== null
            ? clip(call.arguments ?? "", options.inputLimit, 0)
            : (call.arguments ?? "");
        lines.push(
          `#${index} tool_call ${call.id} ${call.name}(${redact ? redact({ kind: "input", content: input }) : input})`,
        );
        const resultMessage = messages.find((m) => m.role === "tool" && m.toolCallId === call.id);
        if (resultMessage) {
          const isError =
            resultMessage.content.length === 0 ||
            /error|failed|失败/i.test(resultMessage.content.slice(0, 80));
          lines.push(
            `result ${call.id}: ${isError ? "error" : "ok"}, ${resultMessage.content.length} chars (omitted)`,
          );
        }
      }
    }
  }
  return lines.join("\n");
}

function renderResultNoteShort(messages: ChatMessage[], callId: string): string {
  const resultMessage = messages.find((m) => m.role === "tool" && m.toolCallId === callId);
  if (!resultMessage) return "no result";
  const isError = /error|failed|失败/i.test(resultMessage.content.slice(0, 80));
  return `${isError ? "error" : "ok"} ${resultMessage.content.length}ch`;
}

/** 六级逐档装配：第一份装进 MAX_STATE_TOKENS 的 state 就是它 */
export function assembleState(input: AssemblyInput): AssemblyOutput {
  const goal = input.goal ?? autoGoal(input.messages);
  const goalBlock = goal.length > 0 ? `[goal]\n${goal}\n\n[history]\n` : "[history]\n";
  const levels: Array<{ stage: StateStage; options: RenderOptions }> = [
    {
      stage: "full",
      options: {
        inputLimit: null,
        oldTextMode: "full",
        recentTextMode: "full",
        oldCallsMode: "full",
      },
    },
    {
      stage: "inputs200",
      options: {
        inputLimit: INPUT_TRUNCATE_LEVELS[0],
        oldTextMode: "full",
        recentTextMode: "full",
        oldCallsMode: "full",
      },
    },
    {
      stage: "inputs60",
      options: {
        inputLimit: INPUT_TRUNCATE_LEVELS[1],
        oldTextMode: "full",
        recentTextMode: "full",
        oldCallsMode: "full",
      },
    },
    {
      stage: "abridged",
      options: {
        inputLimit: INPUT_TRUNCATE_LEVELS[1],
        oldTextMode: "abridge",
        recentTextMode: "full",
        oldCallsMode: "full",
      },
    },
    {
      stage: "collapsed",
      options: {
        inputLimit: INPUT_TRUNCATE_LEVELS[1],
        oldTextMode: "collapse",
        recentTextMode: "abridge",
        oldCallsMode: "full",
      },
    },
    {
      stage: "callsCompacted",
      options: {
        inputLimit: INPUT_TRUNCATE_LEVELS[1],
        oldTextMode: "collapse",
        recentTextMode: "abridge",
        oldCallsMode: "compact",
      },
    },
    {
      stage: "leftOut",
      options: {
        inputLimit: INPUT_TRUNCATE_LEVELS[1],
        oldTextMode: "drop",
        recentTextMode: "abridge",
        oldCallsMode: "compact",
      },
    },
  ];
  for (const level of levels) {
    const state = goalBlock + renderState(input.messages, level.options, input.redact);
    const estimatedTokens = estimateTokens(state);
    if (estimatedTokens <= MAX_STATE_TOKENS) {
      return { state, stage: level.stage, estimatedTokens };
    }
  }
  const overflow =
    goalBlock + renderState(input.messages, levels[levels.length - 1].options, input.redact);
  return { state: overflow, stage: "overflow", estimatedTokens: estimateTokens(overflow) };
}
