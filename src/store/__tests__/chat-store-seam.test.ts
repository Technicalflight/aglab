/**
 * 轮间缝（流式内联的切片点对账）。
 *
 * 后端给每颗工具盖的位置章 `contentChars` 按"轮与轮之间隔着一条空行缝（2 个码元）"
 * 计账；前端实时攒的正文必须补出同一条缝，`InlineFlow` 按`章`切片时才切在轮边界上。
 * 缝少了，差的那 2 个码元正好把下一轮开头的词切成两半——真机踩过：
 * "……确认原因。Gi" + 工具卡 + "tHub 抓不到"，GitHub 被腰斩。
 *
 * 钉五件事：
 * 1. 普通对话（一条 mode 事件都没有）的工具轮也补缝：举旗靠工具首见，
 *    因为 mode 读数只有挂目标的轮次后端才发（chat.rs close_turn 的 view 条件）；
 * 2. 同一颗工具的 running→done 只举一次旗，同轮并发的多颗工具也只补一条缝；
 * 3. 纯工具轮（正文还没开口就先动手）不补引导缝；
 * 4. 目标轮的 mode 读数举旗照旧生效（原本唯一的一路，不能被改坏）；
 * 5. 跟随轮开新气泡时，上一条气泡攒下的旗不许带过来（新气泡开头不许多一条缝）。
 */
import { beforeEach, describe, expect, it, vi } from "vitest";
import { useChatStore } from "@/store/chat-store";
import type { ChatEvent, ConversationRecord, ModeState } from "@/types/chat";

const h = vi.hoisted(() => ({
  channels: [] as Array<{ onmessage?: (event: ChatEvent) => void }>,
  fails: new Set<string>(),
}));

vi.stubGlobal("window", {
  setInterval: () => 0,
  clearInterval: () => undefined,
});

vi.mock("@tauri-apps/api/core", () => ({
  Channel: class {
    onmessage?: (event: ChatEvent) => void;
  },
  invoke: (cmd: string, args: Record<string, unknown> = {}) => {
    if (h.fails.has(cmd)) return Promise.reject(new Error(`命令被拒：${cmd}`));
    switch (cmd) {
      case "chat_send":
        h.channels.push(args.onEvent as { onmessage?: (event: ChatEvent) => void });
        return Promise.resolve(undefined);
      case "config_get":
        return Promise.resolve({
          ui: { sidebarCollapsed: false, panelCollapsed: false, panelTab: "decision", section: "chats" },
          profiles: [],
          showReasoning: true,
        });
      case "history_list":
        return Promise.resolve([]);
      case "history_load":
        return Promise.resolve({
          id: args.id,
          projectId: "proj-存档",
          title: `存档 ${args.id}`,
          createdAt: 1,
          updatedAt: 1,
          messages: [],
        });
      case "history_save": {
        const record = args.conversation as ConversationRecord;
        return Promise.resolve({
          id: record.id,
          title: record.title,
          projectId: record.projectId,
          updatedAt: 2,
          messageCount: record.messages.length,
          preview: "",
        });
      }
      default:
        return Promise.resolve(undefined);
    }
  },
}));

vi.mock("@tauri-apps/plugin-dialog", () => ({ save: vi.fn(), open: vi.fn() }));

vi.mock("@tauri-apps/api/event", () => ({
  listen: () => Promise.resolve(() => undefined),
}));

vi.mock("@/lib/memory", () => ({
  consumeMemorySkipNextTurn: () => false,
  memoryExtract: vi.fn(async () => null),
  runMemoryCommand: vi.fn(async () => ({ handled: false })),
}));

vi.mock("@/lib/decision/integrations", () => ({
  gateMemoryExtraction: vi.fn(async () => null),
  pickPoolMember: vi.fn(async () => null),
  routeModel: vi.fn(async () => null),
}));

const store = () => useChatStore.getState();

const feed = (event: ChatEvent, n = 0) => h.channels[n]?.onmessage?.(event);

const done = (entryIds: string[]): ChatEvent => ({
  type: "done",
  inputTokens: 10,
  outputTokens: 5,
  durationMs: 100,
  cachedTokens: null,
  entryIds,
  model: "测试模型",
});

const goal = (): ModeState => ({
  mode: "goal",
  objective: "把三处对账补齐",
  turnsUsed: 1,
  maxCostUsdE8: 0,
  spentUsdE8: 0,
  status: "active",
  note: null,
  profile: null,
  goalId: "goal-1",
  contract: null,
  planReady: false,
});

const tool = (id: string, contentChars: number | null, status: "running" | "done" = "running"): ChatEvent => ({
  type: "tool",
  id,
  name: "web_fetch",
  status,
  risk: "safe",
  input: "https://example.com",
  output: status === "done" ? "ok" : undefined,
  passReason: null,
  contentChars,
});

/** 收尾之后的回复气泡（批器在 finish 里冲过，正文已是全量） */
const reply = () => store().messages.filter((message) => message.role === "assistant").at(-1);

/** 后端口径的章：so_far + 缝 2 + 本轮正文。缝只在两侧都有正文时存在 */
const seam = 2;
const stamp = (soFar: number, roundText: string) => soFar + seam + roundText.length;

let caseNo = 0;
let A = "conv-0";

beforeEach(() => {
  caseNo += 1;
  A = `conv-seam-${caseNo}`;
  h.channels.length = 0;
  h.fails.clear();
  useChatStore.setState({
    activeId: A,
    projectId: "proj-1",
    title: "甲",
    messages: [],
    usage: undefined,
    attachments: [],
    pending: false,
    followUpCount: 0,
    mode: null,
    modeError: null,
    modeBusy: false,
    runningIds: [],
    goalRuns: [],
    conversations: [],
    toasts: [],
    section: "chats",
  });
});

describe("轮间缝：实时正文要补出后端计账的那条空行缝", () => {
  it("普通对话的工具轮也补缝：章切在轮边界上，下一轮开头的词不再被切成两半", async () => {
    await store().send("帮我看看那个网站");
    // 三轮的真实结构（"Gi|tHub" 那次的形状）：正文→工具→正文→工具→正文，
    // 后端发的章按带缝口径计账，一条 mode 事件都没有
    const round1 = "我来抓取它的首页看看。";
    const round2 = "抓取失败了，原因很有意思。";
    const round3 = "GitHub 抓不到，但原因查清楚了。";
    const stamp1 = round1.length; // 首轮之前没有缝
    const stamp2 = stamp(round1.length, round2);

    feed({ type: "delta", text: round1 });
    feed(tool("t1", stamp1));
    feed({ type: "delta", text: round2 });
    feed(tool("t2", stamp2));
    feed({ type: "delta", text: round3 });
    feed(done(["e1", "e2", "e3"]));

    const message = reply();
    // 实时正文与后端的计账口径一致：轮与轮之间有那条缝
    expect(message?.content).toBe(`${round1}\n\n${round2}\n\n${round3}`);
    const stamps = (message?.steps ?? [])
      .filter((step) => step.kind === "tool")
      .map((step) => step.contentChars);
    expect(stamps).toEqual([stamp1, stamp2]);
    // 关键一刀：章切在轮正文的末尾（下一轮的缝归下一轮的前缀），词不被腰斩
    expect(message?.content.slice(0, stamp2)).toBe(`${round1}\n\n${round2}`);
    expect(message?.content.slice(stamp2)).toBe(`\n\n${round3}`);
  });

  it("同一颗工具的 running→done 只举一次旗，同轮并发的多颗也只补一条缝", async () => {
    await store().send("跑一件事");
    feed({ type: "delta", text: "第一轮正文" });
    feed(tool("t1", 5));
    feed(tool("t1", 5, "done"));
    feed(tool("t2", 5));
    feed({ type: "delta", text: "第二轮正文" });
    feed(done(["e1"]));

    expect(reply()?.content).toBe("第一轮正文\n\n第二轮正文");
  });

  it("纯工具轮（正文未开口先动手）不补引导缝", async () => {
    await store().send("先动手");
    feed(tool("t1", 0));
    feed({ type: "delta", text: "先动手后说话" });
    feed(done(["e1"]));

    expect(reply()?.content).toBe("先动手后说话");
  });

  it("目标轮的 mode 读数举旗照旧：缝照补，章照切在边界上", async () => {
    await store().send("推进目标");
    const round1 = "第一轮做了一半";
    const round2 = "第二轮的话";
    const round3 = "第三轮";
    const stamp2 = stamp(round1.length, round2);

    feed({ type: "delta", text: round1 });
    feed({ type: "mode", continuing: true, state: goal() });
    feed({ type: "delta", text: round2 });
    feed(tool("t1", stamp2));
    feed({ type: "delta", text: round3 });
    feed(done(["g1"]));

    expect(reply()?.content).toBe(`${round1}\n\n${round2}\n\n${round3}`);
    expect(reply()?.content.slice(stamp2)).toBe(`\n\n${round3}`);
  });

  it("跟随轮开新气泡不带旧缝：新气泡的第一个字前面是干净的", async () => {
    await store().send("第一问");
    feed({ type: "delta", text: "第一条回复" });
    feed({ type: "mode", continuing: true, state: goal() });
    feed(done(["e1"]));

    // 续跑那一轮开新气泡：上一条气泡攒下的缝旗不许带过来
    feed({ type: "delta", text: "第二条回复" });
    feed(done(["e2"]));

    const assistants = store().messages.filter((message) => message.role === "assistant");
    expect(assistants).toHaveLength(2);
    expect(assistants[0].content).toBe("第一条回复");
    expect(assistants[1].content).toBe("第二条回复");
  });
});
