/**
 * 生成会话（视频/音乐）的"切换存活"钉。
 *
 * 坏掉的样子（本次改动之前）：媒体会话开局不落盘——对话档是首次发送才落壳，
 * 媒体这条没人管，没生成过东西就切走，会话从侧栏到磁盘一起蒸发；工作区草稿
 * （提示词/歌词/描述）住组件 useState，切换即清零；「停止等待」留下的占位带着
 * streaming:true 落盘，重开就是一张永远转的加载卡。
 *
 * 桩与 branch/running-switch 那两份同形：只钉到 @tauri-apps/api/core 的 invoke。
 */
import { beforeEach, describe, expect, it, vi } from "vitest";
import { useChatStore } from "@/store/chat-store";
import type { ConversationRecord } from "@/types/chat";

const h = vi.hoisted(() => ({
  calls: [] as Array<{ cmd: string; args: Record<string, unknown> }>,
  /** history_load 的返回内容由用例自己摆 */
  archive: null as ConversationRecord | null,
  /** history_list 的返回：bootstrap 清空壳的用例用 */
  list: [] as Array<Record<string, unknown>>,
  /** media_generate 的回包 */
  mediaResult: { path: "C:\\gen\\a.mp3", name: "a.mp3", bytes: 1024 },
  /** 挂起 media_generate：停止等待的用例要控制回包时机 */
  holdMedia: false,
  held: [] as Array<(value: unknown) => void>,
}));

vi.stubGlobal("window", {
  setInterval: (_fn: () => void, _ms: number) => 1,
  clearInterval: () => undefined,
});

// bootstrap 会 wireGoalEvents：不 mock 的话真 listen 会在测试环境里摸不到
// __TAURI_INTERNALS__ 直接炸
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => () => undefined),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args: Record<string, unknown> = {}) => {
    h.calls.push({ cmd, args });
    switch (cmd) {
      case "media_generate":
        if (h.holdMedia) {
          return new Promise((resolve) => {
            h.held.push(resolve);
          });
        }
        return Promise.resolve(h.mediaResult);
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
      case "history_load":
        return Promise.resolve(h.archive);
      case "history_list":
        return Promise.resolve(h.list);
      case "config_get":
        // bootstrap 会把这份 config 灌进 store，后续用例的 syncModelForKind
        // 还要读它：给个字段齐的最小形状，别让 config 变成残缺对象
        return Promise.resolve({
          ui: {},
          model: "",
          kindModels: {},
          models: [],
          profiles: [],
          activeProfileId: "",
          modelPool: { members: [], pinned: null },
        });
      case "conversation_tree":
        return Promise.resolve({ tip: null, nodes: [] });
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
const savedOf = (id: string) =>
  h.calls
    .filter((call) => call.cmd === "history_save")
    .map((call) => call.args.conversation as ConversationRecord)
    .filter((record) => record.id === id);

let caseNo = 0;

beforeEach(() => {
  caseNo += 1;
  h.calls.length = 0;
  h.archive = null;
  h.list = [];
  h.holdMedia = false;
  h.held.length = 0;
  useChatStore.setState({
    activeId: `conv-media-${caseNo}`,
    projectId: "proj-1",
    title: "甲",
    kind: "chat",
    messages: [],
    offPath: [],
    attachments: [],
    usage: undefined,
    pending: false,
    mediaBusy: false,
    mediaDrafts: {},
    conversations: [],
    toasts: [],
    section: "chats",
  });
});

describe("媒体会话的壳：有内容才落", () => {
  it("开局不落盘：光点开媒体会话、切走，历史里什么都不留", async () => {
    store().startConversation("music");
    // 让壳那一路的异步有 chance 跑（如果它不该跑，就什么都不该出现）
    await Promise.resolve();
    await Promise.resolve();
    expect(h.calls.some((call) => call.cmd === "history_save")).toBe(false);
  });

  it("第一笔草稿才落壳：kind=music 的 0 条壳，一次会话只落一次", async () => {
    store().startConversation("music");
    const convId = store().activeId;
    // 纯空串不算内容：不动侧栏也不动盘
    store().setMediaDraft({ lyrics: "" });
    await Promise.resolve();
    await Promise.resolve();
    expect(h.calls.some((call) => call.cmd === "history_save")).toBe(false);
    // 页签与开关是界面状态，更不算内容：切"图像转视频"、开纯音乐，
    // 都不该把会话钉进历史（真机踩过：切子模式连出一排空"新话题"）
    store().setMediaDraft({ mode: "i2v" });
    await Promise.resolve();
    await Promise.resolve();
    expect(h.calls.some((call) => call.cmd === "history_save")).toBe(false);
    store().setMediaDraft({ instrumental: true });
    await Promise.resolve();
    await Promise.resolve();
    expect(h.calls.some((call) => call.cmd === "history_save")).toBe(false);

    store().setMediaDraft({ lyrics: "半截歌词" });
    await vi.waitFor(() => expect(savedOf(convId).length).toBeGreaterThanOrEqual(1));
    const shell = savedOf(convId)[0];
    expect(shell.kind).toBe("music");
    expect(shell.messages).toEqual([]);
    await vi.waitFor(() =>
      expect(store().conversations.some((item) => item.id === convId)).toBe(true),
    );
    // 后续按键不再产生 IO：壳已经在，草稿是内存态
    const savesSoFar = savedOf(convId).length;
    store().setMediaDraft({ lyrics: "半截歌词第二行" });
    await Promise.resolve();
    await Promise.resolve();
    expect(savedOf(convId).length).toBe(savesSoFar);
  });

  it("对话档维持原样：startConversation('chat') 不落壳", async () => {
    store().startConversation("chat");
    await Promise.resolve();
    await Promise.resolve();
    expect(h.calls.some((call) => call.cmd === "history_save")).toBe(false);
  });

  it("启动清一次 0 消息空壳：盘上与侧栏都不再留，有内容的原样保留", async () => {
    h.list = [
      {
        id: "conv-empty-1",
        title: "新话题",
        projectId: "",
        updatedAt: 3,
        messageCount: 0,
        preview: "",
        kind: "video",
      },
      {
        id: "conv-kept",
        title: "有内容的",
        projectId: "",
        updatedAt: 2,
        messageCount: 2,
        preview: "答",
        kind: "music",
      },
    ];
    h.archive = {
      id: "conv-kept",
      projectId: "",
      title: "有内容的",
      createdAt: 1,
      updatedAt: 2,
      pinned: false,
      kind: "music",
      messages: [
        { id: "m1", role: "user", content: "问", createdAt: 1 },
        { id: "m2", role: "assistant", content: "答", createdAt: 2 },
      ],
    };
    await store().bootstrap();
    expect(
      h.calls.some((call) => call.cmd === "history_remove" && call.args.id === "conv-empty-1"),
    ).toBe(true);
    expect(store().conversations.map((item) => item.id)).toEqual(["conv-kept"]);
    expect(store().activeId).toBe("conv-kept");
  });
});

describe("工作区草稿按会话各存一份", () => {
  it("切走再切回来，写了一半的歌词还在原处", async () => {
    store().startConversation("music");
    const convA = store().activeId;
    store().setMediaDraft({ lyrics: "半截歌词" });
    // 切去一个新对话档：新会话名下没有草稿，A 的草稿也不许被清
    store().startConversation("chat");
    expect(store().mediaDrafts[store().activeId]).toBeUndefined();
    // 切回 A：kind 从存档回来，草稿原地接上
    h.archive = {
      id: convA,
      projectId: "proj-1",
      title: "新话题",
      createdAt: 1,
      updatedAt: 2,
      pinned: false,
      kind: "music",
      messages: [],
    };
    await store().openConversation(convA);
    expect(store().activeId).toBe(convA);
    expect(store().mediaDrafts[convA]?.lyrics).toBe("半截歌词");
  });

  it("删掉话题，名下的草稿跟着走", async () => {
    store().startConversation("music");
    const convA = store().activeId;
    store().setMediaDraft({ style: "民谣" });
    // 清草稿排在 await historyRemove 之后：必须等完，断言才不抢先
    await store().deleteConversation(convA);
    expect(store().mediaDrafts[convA]).toBeUndefined();
  });
});

describe("停止等待不落卡死的占位", () => {
  it("停止后占位收尾成明确文案，落盘的不再是 streaming:true 的假生成中", async () => {
    store().startConversation("music");
    const convA = store().activeId;
    h.holdMedia = true;
    const sending = store().sendMedia("写首歌", "music");
    await vi.waitFor(() =>
      expect(store().messages.some((m) => m.streaming && m.media === "music")).toBe(true),
    );
    store().stopMedia();
    // 上游其实出了产物（钱已付）：回包放行，走"停止等待"的收尾分支
    h.held.pop()?.(h.mediaResult);
    await sending;
    const placeholder = store().messages.find((m) => m.media === "music");
    expect(placeholder?.streaming).toBe(false);
    expect(placeholder?.content).toBe("已停止等待。");
    // 落盘的那份同样不能带 streaming：重开才不会渲染一张永远转的加载卡
    await vi.waitFor(() => expect(savedOf(convA).length).toBeGreaterThanOrEqual(1));
    const last = savedOf(convA)[savedOf(convA).length - 1];
    const savedPlaceholder = last.messages.find((m) => m.media === "music");
    expect(savedPlaceholder?.streaming).toBe(false);
  });
});

describe("生成中切走，产物跟着话题走", () => {
  it("完成那一刻人不在归属话题：产物仍落进归属话题的存档，不打进眼前的会话", async () => {
    store().startConversation("music");
    const convA = store().activeId;
    h.holdMedia = true;
    const sending = store().sendMedia("写首歌", "music");
    await vi.waitFor(() =>
      expect(store().messages.some((m) => m.streaming && m.media === "music")).toBe(true),
    );
    // 切去一条新的对话档（切换不中止生成，现场留在 runs 里）
    store().startConversation("chat");
    expect(store().activeId).not.toBe(convA);
    // 上游出了产物：此刻屏幕上是别的话题，回填只许进归属话题的现场
    h.held.pop()?.(h.mediaResult);
    await sending;
    // 眼前这条对话档没被泼上产物
    expect(store().messages.some((m) => m.media === "music")).toBe(false);
    // 归属话题的存档里是收尾完的产物，不是永远转的占位
    await vi.waitFor(() => expect(savedOf(convA).length).toBeGreaterThanOrEqual(1));
    const last = savedOf(convA)[savedOf(convA).length - 1];
    const savedPlaceholder = last.messages.find((m) => m.media === "music");
    expect(savedPlaceholder?.streaming).toBe(false);
    expect(savedPlaceholder?.attachments?.[0]?.path).toBe("C:\\gen\\a.mp3");

    // 切回来：从存档接上，看到的就是带产物的那条
    h.archive = last;
    await store().openConversation(convA);
    expect(store().activeId).toBe(convA);
    const restored = store().messages.find((m) => m.media === "music");
    expect(restored?.streaming).toBe(false);
    expect(restored?.attachments?.[0]?.path).toBe("C:\\gen\\a.mp3");
  });

  it("切回正在生成的会话：现场被接管（不读存档），mediaBusy 带回来挡住第二发", async () => {
    store().startConversation("music");
    const convA = store().activeId;
    h.holdMedia = true;
    const sending = store().sendMedia("写首歌", "music");
    await vi.waitFor(() =>
      expect(store().messages.some((m) => m.streaming && m.media === "music")).toBe(true),
    );
    store().startConversation("chat");
    // 切回 convA：openConversation 优先接管 runs 里的现场
    h.archive = null;
    await store().openConversation(convA);
    expect(store().activeId).toBe(convA);
    expect(store().messages.some((m) => m.streaming && m.media === "music")).toBe(true);
    expect(store().mediaBusy).toBe(true);
    // 第二发被拒：现场只能有一个主人
    await store().sendMedia("再写一首", "music");
    expect(h.calls.filter((call) => call.cmd === "media_generate").length).toBe(1);

    h.held.pop()?.(h.mediaResult);
    await sending;
    // 收尾之后 busy 落下，恢复正常发送
    await vi.waitFor(() => expect(store().mediaBusy).toBe(false));
  });
});
