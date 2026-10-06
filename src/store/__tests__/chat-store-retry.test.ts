/**
 * 重试（重新生成）在**失败回合**上的行为钉。
 *
 * 用户实测（v0.1.4）：服务商 400/500 连续失败后点重试，每重试一次屏上就多一条
 * "hi"——失败回合留下的 [用户气泡 + 空回答气泡] 对没有被收拢。另外失败回合之后
 * pending 卡死会让重试按钮静默失效（连 chat_send 都不发）。
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
const settled = async () => {
  await vi.waitFor(() => expect(store().pending).toBe(false));
  await Promise.resolve();
  await Promise.resolve();
};

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
  it("服务商连续 400/500 时，重试不累积重复的用户气泡", async () => {
    // 第一发：后端报 400（error 事件），回合带着失败回答收尾
    const sending = store().send("hi");
    feed(error("服务商返回 HTTP 400。"));
    await sending;
    await settled();
    expect(userRows().length).toBe(1);

    // 连续两次重试：同样失败。每次重试把上一对挪出当前分支，
    // 屏上保持「一句问题 + 一次失败标记」
    await store().regenerate();
    feed(error("服务商上游错误（HTTP 500），稍后重试。"));
    await settled();
    expect(h.channels.length).toBe(2);
    expect(userRows().length).toBe(1);

    await store().regenerate();
    feed(error("服务商上游错误（HTTP 500），稍后重试。"));
    await settled();
    expect(h.channels.length).toBe(3);
    expect(userRows().length).toBe(1);
  });

  it("失败后的重试最终成功：屏上只有一句问题与一份回答", async () => {
    const sending = store().send("hi");
    feed(error("尚未配置推理服务商地址，请在设置里填写 base URL。"));
    await sending;
    await settled();

    await store().regenerate();
    feed(error("服务商返回 HTTP 400。"));
    await settled();
    expect(h.channels.length).toBe(2);

    // 第三次成功
    await store().regenerate();
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
