/**
 * Verbatim Compaction 的行为钉（design-decision-layer-optimization.md §5.2 + §3 不变量）。
 * 四条硬钉：压缩只删不改（保留内容字节一致）；未动消息引用相等；
 * 缺答/失败的调用一律 keep（反向钉：缺答后无任何 drop）；全链不可用 → identity。
 */
import { describe, expect, it } from "vitest";
import type { ChatMessage } from "@/types/chat";
import { compact, clearStickyStore } from "../compaction";
import { assembleState, autoGoal } from "../compaction/assembly";
import { estimateTokens } from "../compaction/estimate";
import { findCallPairs, groupDuplicates, buildQuestions, readVerdicts, applyVerdicts } from "../compaction/judge";
import type { CompactionAsker } from "../compaction/types";
import { KEEP_CALL_THRESHOLD, TRUNCATE_HEAD_CHARS } from "../constants";

function assistantWithCalls(calls: Array<{ id: string; name: string; input: string }>, content = ""): ChatMessage {
  return {
    role: "assistant",
    content,
    toolCalls: calls.map((c) => ({ id: c.id, name: c.name, arguments: c.input })),
  };
}

function toolResult(id: string, content: string): ChatMessage {
  return { role: "tool", content, toolCallId: id };
}

/** 脚本裁判：按问题名前缀给 P(true)。缺省全 1（全保） */
function scriptedAsker(kr: Record<string, number> = {}, kc: Record<string, number> = {}): CompactionAsker {
  return async (_state, questions) => {
    const answers: Record<string, number> = {};
    for (const name of Object.keys(questions)) {
      if (name.startsWith("kr_")) answers[name] = kr[name] ?? 1;
      else if (name.startsWith("kc_")) answers[name] = kc[name] ?? 1;
      else answers[name] = 1;
    }
    return { answers, usage: { promptTokens: 1000, cachedTokens: 900 } };
  };
}

const SESSION = "session-test";

describe("token 估算（D3：CJK 不低估）", () => {
  it("同样字数，中文的 token 估算高于英文标定公式的结果", () => {
    const cjk = "这是一段三十个字左右的中文文本用来测试分词估算是否足够保守不低估预算" + "字".repeat(20);
    const en = "a".repeat(cjk.length);
    // 英文按 6 字母 1 token；中文按 ≥1.2/字——中文必须更高
    expect(estimateTokens(cjk)).toBeGreaterThan(estimateTokens(en));
    expect(estimateTokens(cjk)).toBeGreaterThanOrEqual(Math.ceil(cjk.length * 1.2));
  });
});

describe("候选配对与钉扎", () => {
  it("首条与最近区内的调用不做候选；无结果的调用不是候选", () => {
    const messages: ChatMessage[] = [
      assistantWithCalls([{ id: "t0", name: "Read", input: "{}" }]), // 首条：钉扎
      toolResult("t0", "pinned content"),
      { role: "user", content: "继续" },
      assistantWithCalls([{ id: "t1", name: "Read", input: "{}" }]),
      toolResult("t1", "old result"),
      { role: "user", content: "1" },
      { role: "assistant", content: "2" },
      { role: "user", content: "3" },
      { role: "assistant", content: "4" },
      { role: "user", content: "5" },
      { role: "assistant", content: "6" },
      { role: "user", content: "7" },
      assistantWithCalls([{ id: "t2", name: "Read", input: "{}" }]), // 最近区：钉扎
      toolResult("t2", "recent result"),
    ];
    const pairs = findCallPairs(messages);
    expect(pairs.map((p) => p.callId)).toEqual(["t1"]);
  });
});

describe("六级 state 装配", () => {
  it("小历史命中 full 档；结果在 state 里只留短注", () => {
    const messages: ChatMessage[] = [
      { role: "user", content: "帮我读文件" },
      assistantWithCalls([{ id: "t1", name: "Read", input: '{"path":"a.txt"}' }]),
      toolResult("t1", "x".repeat(5000)),
    ];
    const out = assembleState({ messages });
    expect(out.stage).toBe("full");
    expect(out.state).toContain("ok");
    expect(out.state).toContain("5000 chars (omitted)");
    expect(out.state).not.toContain("xxxxx"); // 结果全文不进 state
    expect(out.state).toContain('"path":"a.txt"'); // 输入原样
  });

  it("超长历史逐级降档；老正文在 collapsed 档折叠、recent 保持最久", () => {
    const messages: ChatMessage[] = [
      { role: "user", content: "开头语境".repeat(2000) },
    ];
    for (let i = 0; i < 60; i++) {
      messages.push({ role: "user", content: `旧消息 ${i} `.padEnd(400, "字") });
      messages.push(assistantWithCalls([{ id: `t${i}`, name: "Read", input: JSON.stringify({ path: `file${i}.txt` }) }]));
      messages.push(toolResult(`t${i}`, ("结果".repeat(300)) + String(i)));
    }
    const out = assembleState({ messages });
    // 大历史不可能全量装下：必须降档，且不 overflow
    expect(out.stage).not.toBe("full");
    expect(out.stage).not.toBe("overflow");
  });

  it("goal 自动注入最近用户消息（最多 3 条）", () => {
    const messages: ChatMessage[] = [
      { role: "user", content: "第零条" },
      { role: "assistant", content: "好" },
      { role: "user", content: "第一条目标" },
      { role: "assistant", content: "好" },
      { role: "user", content: "第二条目标" },
      { role: "assistant", content: "好" },
      { role: "user", content: "第三条目标" },
    ];
    const goal = autoGoal(messages);
    expect(goal).toContain("第三条目标");
    expect(goal).toContain("第二条目标");
    expect(goal).toContain("第一条目标");
    expect(goal).not.toContain("第零条"); // 4 条用户消息只取最近 3 条
  });
});

describe("重复合并（D6）", () => {
  it("同工具同输入且结果量级相近 → 并组，判定一次应用全组", () => {
    const pairs = findCallPairs([
      { role: "user", content: "x" },
      assistantWithCalls([
        { id: "a", name: "Read", input: '{"path":"same.txt"}' },
        { id: "b", name: "Read", input: '{ "path": "same.txt" }' }, // 键序空白不同，规范化后相同
        { id: "c", name: "Read", input: '{"path":"other.txt"}' },
      ]),
      toolResult("a", "r".repeat(1000)),
      toolResult("b", "r".repeat(1100)),
      toolResult("c", "r".repeat(1000)),
      ...Array.from({ length: 8 }, (_, i) => ({ role: "user" as const, content: `pad ${i}` })),
    ]);
    const groups = groupDuplicates(pairs);
    const merged = groups.find((g) => g.pairs.length === 2);
    expect(merged).toBeDefined();
    expect(merged!.pairs.map((p) => p.callId).sort()).toEqual(["a", "b"]);
  });
});

describe("compact 编排", () => {
  const baseHistory = (): ChatMessage[] => [
    { role: "user", content: "帮我查三件事" },
    assistantWithCalls([
      { id: "t1", name: "WebSearch", input: '{"q":"query one"}' },
      { id: "t2", name: "Read", input: '{"path":"big.txt"}' },
    ]),
    toolResult("t1", "search result text ".repeat(50)),
    toolResult("t2", "file content ".repeat(100)),
    ...Array.from({ length: 10 }, (_, i) => ({ role: "user" as const, content: `后续消息 ${i}` })),
  ];

  it("双阈值：keepResult≥0.6 全保；keepCall≥0.4 截结果；都低 → 连调用一起删", async () => {
    clearStickyStore();
    const messages = baseHistory();
    // t1：结果不要、调用要（drop_result）；t2：全不要（drop_call）
    const asker: CompactionAsker = async (_state, questions) => {
      const answers: Record<string, number> = {};
      for (const name of Object.keys(questions)) {
        const isWebSearch = name.includes("WebSearch");
        if (name.startsWith("kr_")) answers[name] = isWebSearch ? 0.2 : 0.1;
        else if (name.startsWith("kc_")) answers[name] = isWebSearch ? KEEP_CALL_THRESHOLD + 0.1 : 0.1;
        else answers[name] = 1;
      }
      return { answers, usage: { promptTokens: 500, cachedTokens: 480 } };
    };
    const result = await compact(SESSION, messages, asker, { cacheGuard: true });
    expect(result.identity).toBe(false);
    const out = result.messages;
    // t1 调用还在、结果被截
    const t1Call = out.find((m) => m.role === "assistant")!.toolCalls!.find((c) => c.id === "t1");
    expect(t1Call).toBeDefined();
    const t1Result = out.find((m) => m.role === "tool" && m.toolCallId === "t1");
    expect(t1Result!.content).toContain("[truncated: original");
    expect(t1Result!.content.length).toBeLessThan(TRUNCATE_HEAD_CHARS + 120);
    // t2 连调用带结果一起消失；结果消息不落单
    const t2Call = out.find((m) => m.role === "assistant")!.toolCalls!.find((c) => c.id === "t2");
    expect(t2Call).toBeUndefined();
    expect(out.find((m) => m.role === "tool" && m.toolCallId === "t2")).toBeUndefined();
  });

  it("身份保持：keep 的消息是原对象引用", async () => {
    clearStickyStore();
    const messages = baseHistory();
    const result = await compact(SESSION, messages, scriptedAsker(), { cacheGuard: true });
    expect(result.identity).toBe(true); // 全 1 = 全保 = 一个字节没动
    expect(result.messages[0]).toBe(messages[0]);
    expect(result.messages).toHaveLength(messages.length);
  });

  it("裁判全保也不作废剪枝成果（D2 的反向钉：缩减率不是否决条件），budget-fit 只在有预算语义时生效", async () => {
    clearStickyStore();
    const messages = baseHistory();
    const result = await compact(SESSION, messages, scriptedAsker());
    // 全保 → touched=0 → identity，但没有 degraded、没有错误
    expect(result.identity).toBe(true);
    expect(result.degraded).toBeUndefined();
  });

  it("D1：批次失败 → 缺答调用判定 keep，绝不 throw、绝无 drop（反向钉）", async () => {
    clearStickyStore();
    const messages = baseHistory();
    const failing: CompactionAsker = async () => {
      throw new Error("链全断");
    };
    const result = await compact(SESSION, messages, failing);
    expect(result.identity).toBe(true);
    expect(result.degraded).toBeUndefined(); // 失败批不是全链不可用（每批都试了）
    expect(result.stats.unansweredKeeps).toBeGreaterThan(0);
    expect(result.messages).toHaveLength(messages.length);
  });

  it("全链不可用（asker 永久不可用形态）→ identity + 每次调用都有人接住，调用方无感", async () => {
    clearStickyStore();
    const messages = baseHistory();
    const dead: CompactionAsker = async () => {
      throw new Error("unavailable");
    };
    const result = await compact(SESSION, messages, dead, { cacheGuard: false });
    expect(result.identity).toBe(true);
    expect(result.messages.every((m, i) => m === messages[i])).toBe(true);
  });

  it("Cache Guard：读不到 usage → 首批之后停发，未判定组全 keep 并标记", async () => {
    clearStickyStore();
    const messages = baseHistory();
    let calls = 0;
    const noUsage: CompactionAsker = async (_state, questions) => {
      calls += 1;
      const answers: Record<string, number> = {};
      for (const name of Object.keys(questions)) answers[name] = 0.1; // 全判 drop_call
      return { answers }; // 无 usage
    };
    const result = await compact(SESSION, messages, noUsage);
    expect(calls).toBe(1); // 第一批之后 guard 拦下后续批次
    expect(result.skippedByCacheGuard).toBe("no_usage");
  });

  it("Sticky：第二次 compact 复用判定，零请求", async () => {
    clearStickyStore();
    const messages = baseHistory();
    let calls = 0;
    const counting: CompactionAsker = async (_state, questions) => {
      calls += 1;
      const answers: Record<string, number> = {};
      for (const name of Object.keys(questions)) {
        if (name.startsWith("kr_")) answers[name] = 0.1;
        else answers[name] = 0.9; // drop_result
      }
      return { answers, usage: { promptTokens: 500, cachedTokens: 490 } };
    };
    const first = await compact(SESSION, messages, counting);
    expect(calls).toBe(1);
    const again = await compact(SESSION, first.messages, counting);
    expect(calls).toBe(1); // 判定全部来自 Sticky，没再问裁判
    expect(again.stats.batches).toBe(0);
  });

  it("敏感历史同样走压缩但裁判由调用方钉本地：模块只透传 sensitivity 语义，不做第二套路由", async () => {
    clearStickyStore();
    const messages = baseHistory();
    const seenSensitivity: string[] = [];
    const asker: CompactionAsker = async () => {
      seenSensitivity.push("asked");
      return { answers: {}, usage: { promptTokens: 1, cachedTokens: 1 } }; // 缺答 → 全 keep
    };
    const result = await compact(SESSION, messages, asker, { sensitivity: "confidential" });
    expect(seenSensitivity).toHaveLength(1);
    expect(result.identity).toBe(true); // 缺答 → keep → identity
  });
});

describe("判定应用与不变量", () => {
  it("压缩只删不改：保留消息的字节与原历史一致", async () => {
    const messages: ChatMessage[] = [
      { role: "user", content: "上下文".repeat(100) },
      assistantWithCalls([{ id: "t1", name: "Bash", input: '{"cmd":"ls"}' }]),
      toolResult("t1", "output ".repeat(200)),
      ...Array.from({ length: 10 }, (_, i) => ({ role: "user" as const, content: `m${i}` })),
    ];
    const pairs = findCallPairs(messages);
    const groups = groupDuplicates(pairs);
    const questions = buildQuestions(groups);
    const answers: Record<string, number> = {};
    for (const name of Object.keys(questions)) answers[name] = 0.1; // 全 drop_call
    const verdicts = readVerdicts(groups, answers);
    const applied = applyVerdicts(messages, groups, verdicts);
    // 剩下的每条消息要么是原对象，要么是截断结果——不出现"改写"的第三种
    for (const message of applied.messages) {
      const original = messages.find((m) => m === message);
      if (original) continue;
      expect(message.content).toContain("[truncated: original");
    }
    expect(applied.messages.some((m) => m.role === "assistant" && m.toolCalls?.length)).toBe(false);
  });
});
