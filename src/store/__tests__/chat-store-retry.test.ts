/**
 * 重试（重新生成）在**失败回合**上的行为钉。
 *
 * 用户实测（v0.1.4/v0.1.5）：服务商 400/500 连续失败后点重试，每重试一次
 * 屏上就多一条 "hi"、日志里多一行同样的问题。根因链：
 * 1. 失败回合的问题行**已经落了日志**（回合在追加之后才死），但 done 事件没来，
 *    乐观气泡没领到 entryIds → 重试走了"当新输入重发"的兜底 → 日志再叠一行；
 * 2. 失败回合收尾时 pending 没复位 → 重试按钮静默失效。
 *
 * 修复后的契约：兜底路径先问后端要**树**，从 tip 沿父链回溯到同内容的用户行
 * （最早的那条——失败重试叠出来的层一并愈合），指着它 rewind 重问；
 * 日志里没有这句（回合死在追加之前）才当新输入发。
 */
import { beforeEach, describe, expect, it, vi } from "vitest";
import { useChatStore } from "@/store/chat-store";
import type { ChatEvent } from "@/types/chat";

const h = vi.hoisted(() => ({
  calls: [] as Array<{ cmd: string; args: Record<string, unknown> }>,
  channels: [] as Array<{ onmessage?: (event: ChatEvent) => void }>,
  archive: null as Record<string, unknown> | null,
  tree: null as Record<string, unknown> | null,
}));

vi.stubGlobal("window", {
  setInterval: () => 1,
  clearInterval: () => undefined,
});

vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => undefined) }));

vi.mock("@tauri-apps/api/core", () => ({
  Channel: class {
    onmessage?: (event: ChatEvent) => void;
  },
  invoke: (cmd: string, args: Record<string, unknown> = {}) => {
    h.calls.push({ cmd, args });
    switch (cmd) {
      case "chat_send":
        h.channels.push(args.onEvent as { onmessage?: (event: ChatEvent) => void });
        return Promise.resolve(undefined);
      case "history_load":
        return Promise.resolve(h.archive);
      case "conversation_tree":
        return Promise.resolve(h.tree ?? { tip: null, nodes: [] });
      case "history_save":
        return Promise.resolve({
          id: (args.conversation as { id: string }).id,
          title: "",
          projectId: "",
          updatedAt: 2,
          messageCount: (args.conversation as { messages: unknown[] }).messages.length,
          preview: "",
        });
      case "edits_for_session":
        return Promise.resolve([]);
      default:
        return Promise.resolve({});
    }
  },
}));

vi.mock("@/lib/memory", () => ({
  consumeMemorySkipNextTurn: () => false,
  memoryExtract: vi.fn(async () => null),
  runMemoryCommand: vi.fn(async () => ({ handled: false })),
}));
vi.mock("@/lib/decision/integrations", () => ({
  gateMemoryExtraction: vi.fn(async () => null),
  routeModel: vi.fn(async () => null),
}));

const store = () => useChatStore.getState();
const feed = (event: ChatEvent, n = h.channels.length - 1) => h.channels[n]?.onmessage?.(event);
const error = (message: string): ChatEvent => ({ type: "error", message });
const done = (entryIds: string[]): ChatEvent => ({
  type: "done",
  inputTokens: 10,
  outputTokens: 5,
  durationMs: 100,
  cachedTokens: null,
  entryIds,
  model: "测试模型",
});
const userRows = () => store().messages.filter((m) => m.role === "user");
const waitChannel = async (n: number) => {
  await vi.waitFor(() => expect(h.channels.length).toBe(n));
};
const settled = async () => {
  await vi.waitFor(() => expect(store().pending).toBe(false));
  await Promise.resolve();
  await Promise.resolve();
};
const lastChatSend = () =>
  h.calls.filter((c) => c.cmd === "chat_send").at(-1)?.args as Record<string, unknown>;

let caseNo = 0;

beforeEach(() => {
  caseNo += 1;
  h.calls.length = 0;
  h.channels.length = 0;
  h.archive = null;
  h.tree = null;
  useChatStore.setState({
    activeId: `conv-retry${caseNo}`,
    projectId: "",
    title: "新话题",
    kind: "chat",
    messages: [],
    offPath: [],
    attachments: [],
    usage: undefined,
    pending: false,
    conversations: [],
    toasts: [],
    section: "chats",
  });
});

describe("失败回合上的重新生成", () => {
  it("服务商连续失败时，重试不累积重复的用户气泡", async () => {
    const sending = store().send("hi");
    await waitChannel(1);
    feed(error("服务商返回 HTTP 400。"));
    await sending;
    await settled();
    expect(userRows().length).toBe(1);

    // 连续两次重试：同样失败。每次重试把上一对挪出当前分支，
    // 屏上保持「一句问题 + 一次失败标记」
    await store().regenerate();
    await waitChannel(2);
    feed(error("服务商上游错误（HTTP 500），稍后重试。"));
    await settled();
    expect(userRows().length).toBe(1);

    await store().regenerate();
    await waitChannel(3);
    feed(error("服务商上游错误（HTTP 500），稍后重试。"));
    await settled();
    expect(userRows().length).toBe(1);
  });

  it("失败后的重试最终成功：屏上只有一句问题与一份回答", async () => {
    const sending = store().send("hi");
    await waitChannel(1);
    feed(error("尚未配置推理服务商地址，请在设置里填写 base URL。"));
    await sending;
    await settled();

    await store().regenerate();
    await waitChannel(2);
    feed(error("服务商返回 HTTP 400。"));
    await settled();

    // 第三次成功
    await store().regenerate();
    await waitChannel(3);
    feed({ type: "delta", text: "你好！我在。" } as ChatEvent);
    feed(done(["e1", "e2"]));
    await vi.waitFor(() =>
      expect(store().messages.some((m) => m.content.includes("你好"))).toBe(true),
    );
    await settled();

    const users = userRows();
    expect(users.length).toBe(1);
    expect(users[0].content).toBe("hi");
    const answers = store().messages.filter((m) => m.role === "assistant");
    expect(answers.length).toBe(1);
    expect(answers[0].content).toContain("你好");
  });
});

describe("重试的兜底：先问树，把问题对回日志里已落的那一行", () => {
  it("问句已在日志（失败回合追加之后才死）：rewind 重问，日志不叠行", async () => {
    const sending = store().send("hi");
    await waitChannel(1);
    feed(error("服务商返回 HTTP 400。"));
    await sending;
    await settled();
    expect(userRows().length).toBe(1);

    // 后端的树：日志里躺着那行问题（sections 之后），无回答
    h.tree = {
      tip: "log-q1",
      nodes: [
        {
          id: "log-sec",
          parentId: null,
          seq: 1,
          kind: "custom_message",
          role: null,
          preview: null,
          at: 1,
          onPath: true,
        },
        {
          id: "log-q1",
          parentId: "log-sec",
          seq: 2,
          kind: "message",
          role: "user",
          preview: "hi",
          at: 2,
          onPath: true,
        },
      ],
    } as never;

    await store().regenerate();
    await waitChannel(2);
    feed(error("服务商上游错误（HTTP 500），稍后重试。"));
    await settled();
    // 重发用的是空正文 + rewindTo（树里那行的 id）
    expect(lastChatSend().input).toBe("");
    expect(lastChatSend().rewindTo).toBe("log-q1");
    expect(userRows().length).toBe(1);

    // 再重试一次（同样的失败）：树里仍只有一行问题，不再叠
    await store().regenerate();
    await waitChannel(3);
    feed(error("服务商上游错误（HTTP 500），稍后重试。"));
    await settled();
    expect(lastChatSend().input).toBe("");
    expect(lastChatSend().rewindTo).toBe("log-q1");
    expect(userRows().length).toBe(1);
  });

  it("问题在日志里叠过层（旧版留下的疤）：回溯到最早那条，一并愈合", async () => {
    const sending = store().send("hi");
    await waitChannel(1);
    feed(error("服务商返回 HTTP 400。"));
    await sending;
    await settled();

    // 旧版叠出来的疤：日志里有两行同内容的用户行（线性挂在链上）
    h.tree = {
      tip: "log-q2",
      nodes: [
        {
          id: "log-sec",
          parentId: null,
          seq: 1,
          kind: "custom_message",
          role: null,
          preview: null,
          at: 1,
          onPath: true,
        },
        {
          id: "log-q1",
          parentId: "log-sec",
          seq: 2,
          kind: "message",
          role: "user",
          preview: "hi",
          at: 2,
          onPath: true,
        },
        {
          id: "log-decl",
          parentId: "log-q1",
          seq: 3,
          kind: "custom",
          role: null,
          preview: null,
          at: 3,
          onPath: true,
        },
        {
          id: "log-q2",
          parentId: "log-decl",
          seq: 4,
          kind: "message",
          role: "user",
          preview: "hi",
          at: 4,
          onPath: true,
        },
      ],
    } as never;

    await store().regenerate();
    await waitChannel(2);
    feed(error("服务商上游错误（HTTP 500），稍后重试。"));
    await settled();
    // 回溯到**最早**的同内容用户行：q2、q1 之间的旧叠层一并退出当前分支
    expect(lastChatSend().input).toBe("");
    expect(lastChatSend().rewindTo).toBe("log-q1");
    expect(userRows().length).toBe(1);
  });

  it("日志里没有这句问题（回合死在追加之前）才当新输入发", async () => {
    const sending = store().send("hi");
    await waitChannel(1);
    feed(error("服务商返回 HTTP 400。"));
    await sending;
    await settled();

    // 后端的树：只有段落行，没有任何用户行（回合死在追加之前）
    h.tree = {
      tip: "log-sec",
      nodes: [
        {
          id: "log-sec",
          parentId: null,
          seq: 1,
          kind: "custom_message",
          role: null,
          preview: null,
          at: 1,
          onPath: true,
        },
      ],
    } as never;

    await store().regenerate();
    await waitChannel(2);
    feed(error("服务商上游错误（HTTP 500），稍后重试。"));
    await settled();
    expect(lastChatSend().input).toBe("hi");
    expect(lastChatSend().rewindTo).toBe(null);
    expect(userRows().length).toBe(1);
  });
});
