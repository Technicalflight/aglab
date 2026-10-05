/**
 * 流程条那几条判据。它们决定用户看见的"这一轮在做什么"，所以每一条都要能被真跑，
 * 而不是只在浏览器里瞄一眼。
 */
import { describe, expect, it } from "vitest";
import {
  flowTitle,
  formatElapsed,
  hasAwaitingApproval,
  isFlowOpen,
  stepSummary,
  thinkingPreview,
} from "@/lib/run-flow";
import type { Message, ToolCall } from "@/types/chat";

const base: Message = { id: "m1", role: "assistant", content: "", createdAt: 1_000 };

const call = (patch: Partial<ToolCall>): ToolCall => ({
  id: "c1",
  name: "write_file",
  status: "done",
  risk: "safe",
  input: "",
  ...patch,
});

describe("每一步那一行写什么", () => {
  it("JSON 输入里挑得出目标，路径只留最后一段", () => {
    expect(stepSummary(call({ input: '{"path":"C:/repo/src-tauri/src/chat.rs","content":"…"}' }))).toBe(
      "chat.rs",
    );
    expect(stepSummary(call({ name: "run_command", input: '{"command":"cargo test --lib"}' }))).toBe(
      "cargo test --lib",
    );
  });

  it("不是 JSON 的输入按第一行读，长行会被截", () => {
    expect(stepSummary(call({ input: "第一行\n第二行" }))).toBe("第一行");
    expect(stepSummary(call({ input: "" }))).toBe("");
    const long = "x".repeat(200);
    expect(stepSummary(call({ input: long })).length).toBeLessThanOrEqual(64);
    expect(stepSummary(call({ input: "{不是合法 JSON" }))).toBe("{不是合法 JSON");
  });

  it("思考那行从自己的起点往后读，正文只有一份", () => {
    const first = "第一段想法\n继续\n";
    const message: Message = {
      ...base,
      reasoning: `${first}第二段想法`,
      steps: [
        { kind: "thinking", id: "s1", at: 0, from: 0 },
        { kind: "tool", id: "s2", callId: "c1" },
        { kind: "thinking", id: "s3", at: 0, from: first.length },
      ],
    };
    expect(thinkingPreview(message, message.steps![0], false)).toBe("第一段想法");
    expect(thinkingPreview(message, message.steps![2], false)).toBe("第二段想法");
  });

  it("正在出的那一段报最新一句：第一行在长思考里从头到尾不变", () => {
    const message: Message = {
      ...base,
      streaming: true,
      reasoningStreaming: true,
      reasoning: "先看一眼现在的实现\n再看第二处\n这一句是刚出来的",
      steps: [{ kind: "thinking", id: "s1", at: 0, from: 0 }],
    };
    expect(thinkingPreview(message, message.steps![0], true)).toBe("这一句是刚出来的");
    // 同一段想完之后回到开头那句，回看时才有用
    expect(thinkingPreview(message, message.steps![0], false)).toBe("先看一眼现在的实现");
  });

  it("空正文不报错", () => {
    const message: Message = { ...base, steps: [{ kind: "thinking", id: "s1", at: 0, from: 0 }] };
    expect(thinkingPreview(message, message.steps![0], true)).toBe("");
  });
});

describe("标题与耗时", () => {
  it("秒表读数说人话", () => {
    expect(formatElapsed(8_000)).toBe("8 秒");
    expect(formatElapsed(92_000)).toBe("1 分 32 秒");
    expect(formatElapsed(60_000)).toBe("1 分");
    expect(formatElapsed(3_720_000)).toBe("1 时 2 分");
    expect(formatElapsed(-5)).toBe("0 秒");
  });

  it("跑着报秒表，跑完报步数与总耗时", () => {
    expect(flowTitle({ ...base, streaming: true, steps: [] }, 92_000)).toBe("正在执行中 · 1 分 32 秒");
    expect(
      flowTitle({ ...base, steps: [{ kind: "tool", id: "s", callId: "c1" }], durationMs: 5_000 }, 0),
    ).toBe("执行了 1 步 · 5 秒");
    expect(
      flowTitle({ ...base, steps: new Array(6).fill({ kind: "tool", id: "s", callId: "c1" }), durationMs: 92_000 }, 0),
    ).toBe("执行了 6 步 · 1 分 32 秒");
  });
});

describe("展开与否", () => {
  const withPending: Message = {
    ...base,
    steps: [{ kind: "tool", id: "s", callId: "c1" }],
    toolCalls: [call({ status: "pending" })],
  };

  it("有待批的那一发时压都压不下去", () => {
    expect(hasAwaitingApproval(withPending)).toBe(true);
    // manual=false 是用户点了收起：待批卡必须赢过它
    expect(isFlowOpen(withPending, false)).toBe(true);
  });

  it("没在等批准时：跑着展开、跑完收起，手动的决定优先", () => {
    const running: Message = { ...base, streaming: true, steps: [{ kind: "tool", id: "s", callId: "c1" }] };
    const finished: Message = { ...running, streaming: false };
    expect(isFlowOpen(running, null)).toBe(true);
    expect(isFlowOpen(finished, null)).toBe(false);
    expect(isFlowOpen(finished, true)).toBe(true);
    expect(isFlowOpen(running, false)).toBe(false);
  });
});
