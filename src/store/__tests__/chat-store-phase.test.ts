/**
 * 回合阶段埋点（O16-3）。
 *
 * 卡住的时候用户看到的只有「Done 没来」——而压缩、审批、工具、读流这四件事的等待
 * 时间差着量级：读流是常态、压缩要发一整份历史、审批在等人、工具可能跑好几分钟。
 * 分不清，就分不清该继续等还是该按停止。
 *
 * 钉四件事：
 * 1. 事件把阶段推到对应那一段；
 * 2. 收尾的事件把它清掉，而且**只清自己那一段**——迟到的事件不许把后一段的读数抹掉；
 * 3. 审批与工具分开记：等用户点 vs 工具自己在跑，看门狗那句提示才分得清该催谁；
 * 4. 没在跑的时候是 null。
 */
import { beforeEach, describe, expect, it, vi } from "vitest";
import { useChatStore } from "@/store/chat-store";
import type { ChatEvent } from "@/types/chat";

const h = vi.hoisted(() => ({
  channels: [] as Array<{ onmessage?: (event: ChatEvent) => void }>,
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
    switch (cmd) {
      case "chat_send":
        h.channels.push(args.onEvent as { onmessage?: (event: ChatEvent) => void });
        return Promise.resolve(undefined);
      case "config_get":
        return Promise.resolve({
          ui: {
            sidebarCollapsed: false,
            panelCollapsed: false,
            panelTab: "preview",
            section: "chats",
          },
          profiles: [],
          showReasoning: true,
        });
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
const feed = (event: ChatEvent) => h.channels[0]?.onmessage?.(event);
const phase = () => store().turnPhase;

const toolEvent = (status: "pending" | "running" | "done"): ChatEvent => ({
  type: "tool",
  id: "t1",
  name: "read_file",
  status,
  risk: "safe",
  input: "a.ts",
  output: status === "done" ? "ok" : undefined,
  passReason: null,
  contentChars: null,
});

let caseNo = 0;

beforeEach(() => {
  caseNo += 1;
  h.channels.length = 0;
  useChatStore.setState({
    activeId: `conv-phase-${caseNo}`,
    projectId: "proj-1",
    title: "甲",
    messages: [],
    usage: undefined,
    attachments: [],
    pending: false,
    turnPhase: null,
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

describe("回合阶段：卡住时要说得出卡在哪一段", () => {
  it("事件把阶段推到对应那一段，收尾的事件再把它清掉", async () => {
    await store().send("开工");
    expect(phase()).toBe("stream");

    // 模型出字：还在读流那一段
    feed({ type: "delta", text: "看一下" });
    expect(phase()).toBe("stream");

    // 工具挂着等人批准：这是"该催你点"的那一段
    feed(toolEvent("pending"));
    expect(phase()).toBe("approval");

    // 批准了、工具自己在跑：这是"该再等等"的那一段
    feed(toolEvent("running"));
    expect(phase()).toBe("tool");

    feed(toolEvent("done"));
    expect(phase()).toBeNull();

    // 压缩要发一整份历史，与读流不是一回事
    feed({ type: "compaction", phase: "start" });
    expect(phase()).toBe("compacting");
    feed({ type: "compaction", phase: "done", summary: "摘要" });
    expect(phase()).toBeNull();

    // 后端在等人回答：没有超时，界面得说得出这一点
    feed({ type: "ask", id: "q1", question: "选哪个？", options: [] });
    expect(phase()).toBe("waiting");
  });

  it("收尾只清自己那一段：迟到的事件不许把后一段的读数抹掉", async () => {
    await store().send("开工");

    // 压缩收尾的事件晚到了，而工具已经开始跑
    feed(toolEvent("running"));
    expect(phase()).toBe("tool");
    feed({ type: "compaction", phase: "done", summary: "摘要" });
    expect(phase(), "压缩的收尾不该把工具那一段抹掉").toBe("tool");
  });
});
