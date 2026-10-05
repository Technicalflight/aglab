/**
 * 对话分支树的状态层钉。
 *
 * 坏掉的样子（本次改动之前）：「重新生成」把旧答案从界面归档里 `slice` 掉，
 * 而后端话题日志把它留成了同一父下的另一支——于是"两份真相"：日志说这条话题有三个
 * 回答，盘上那份只认一个。而用户没有任何办法回到旧那一支去看它说过什么。
 *
 * 现在：`messages` 是**当前那条路径**（44 处消费者照旧成立），切走的分支整段进
 * `offPath`，父子关系由 `parentId` 记着（抄自后端日志，前端不自造）。
 * 这里的桩只钉到 `@tauri-apps/api/core` 那一层，与 running-switch 那份同形。
 */
import { beforeEach, describe, expect, it, vi } from "vitest";
import { useChatStore } from "@/store/chat-store";
import type { ChatEvent, ConversationRecord, Message } from "@/types/chat";
import type { ConversationTree } from "@/lib/chat-transport";

const h = vi.hoisted(() => ({
  calls: [] as Array<{ cmd: string; args: Record<string, unknown> }>,
  channels: [] as Array<{ onmessage?: (event: ChatEvent) => void }>,
  /** history_load / conversation_tree 的返回内容由每条用例自己摆 */
  archive: null as ConversationRecord | null,
  tree: null as ConversationTree | null,
  treeFails: false,
  /** project_select 的返回（选择工作目录后后端给的整份配置） */
  selectConfig: null as Record<string, unknown> | null,
}));

/** 被测代码要的只是"有一个能停下来的定时器"。测试里不必真走时，记账即可 */
const timers = new Map<number, () => void>();
let timerSeq = 0;

vi.stubGlobal("window", {
  setInterval: (fn: () => void, _ms: number) => {
    const id = ++timerSeq;
    timers.set(id, fn);
    return id;
  },
  clearInterval: (id: number) => {
    timers.delete(id);
  },
});

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
        if (h.treeFails) return Promise.reject(new Error("读不到树"));
        return Promise.resolve(h.tree ?? { tip: null, nodes: [] });
      case "conversation_navigate":
        return Promise.resolve(args.entryId ?? null);
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
      case "edits_for_session":
        return Promise.resolve([]);
      case "project_select":
        return Promise.resolve(h.selectConfig ?? {});
      default:
        return Promise.resolve({});
    }
  },
}));

vi.mock("@tauri-apps/plugin-dialog", () => ({ save: vi.fn(), open: vi.fn() }));
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
const feed = (event: ChatEvent, n = 0) => h.channels[n]?.onmessage?.(event);
const savedOf = (id: string) =>
  h.calls
    .filter((call) => call.cmd === "history_save")
    .map((call) => call.args.conversation as ConversationRecord)
    .filter((record) => record.id === id);
const done = (entryIds: string[]): ChatEvent => ({
  type: "done",
  inputTokens: 10,
  outputTokens: 5,
  durationMs: 100,
  cachedTokens: null,
  entryIds,
  model: "测试模型",
});

const msg = (over: Partial<Message>): Message => ({
  id: `msg-${over.content ?? over.role}`,
  role: "assistant",
  content: "答",
  createdAt: 10,
  ...over,
});

let caseNo = 0;
let A = "conv-0";

beforeEach(() => {
  caseNo += 1;
  A = `conv-branch${caseNo}`;
  h.calls.length = 0;
  h.channels.length = 0;
  h.archive = null;
  h.tree = null;
  h.treeFails = false;
  h.selectConfig = null;
  useChatStore.setState({
    activeId: A,
    projectId: "proj-1",
    title: "甲",
    messages: [],
    offPath: [],
    usage: undefined,
    attachments: [],
    pending: false,
    followUpCount: 0,
    runningIds: [],
    conversations: [],
    toasts: [],
    section: "chats",
  });
});

describe("父子关系抄的是日志的形状", () => {
  it("正常发送：新问题挂在当前末端，回答挂在那句问题之后", async () => {
    await store().send("第一句");
    feed(done(["e1", "e2"]));
    const [question, answer] = store().messages;
    expect(question.parentId).toBeNull();
    expect(answer.parentId).toBe(question.id);
    expect(store().offPath).toEqual([]);
  });

  it("第二轮的问题挂在上一轮的回答之后（链是一路接上去的，不是各自为根）", async () => {
    await store().send("第一句");
    feed(done(["e1", "e2"]));
    await store().send("第二句");
    feed(done(["e3", "e4"]), 1);
    const [, a1, q2] = store().messages;
    expect(q2.parentId).toBe(a1.id);
    expect(store().messages.map((item) => item.role)).toEqual(["user", "assistant", "user", "assistant"]);
  });
});

describe("重新生成长出一支，不抹掉旧的", () => {
  /** 摆一条"问 → 答"的现场，答带条目 id */
  function seeded() {
    const question = msg({ id: "q1", role: "user", content: "怎么改", entryIds: ["e1"] });
    const answer = msg({ id: "a1", content: "旧答案", parentId: "q1", entryIds: ["e2"] });
    useChatStore.setState({ messages: [question, answer], offPath: [] });
    return { question, answer };
  }

  it("旧答案整段进 offPath，新回答同父地长在它旁边", () => {
    const { answer } = seeded();
    void store().regenerate();
    // send 的同步段已经贴上了新的流式回答：这里不喂事件，只断言同步段保证得了的事
    expect(store().offPath.map((item) => item.id)).toEqual([answer.id]);
    const [question, reply] = store().messages;
    expect(question.id).toBe("q1");
    expect(reply.streaming).toBe(true);
    expect(reply.parentId).toBe("q1");
    expect(reply.id).not.toBe(answer.id);
  });

  it("新回答与旧回答同父——它俩是兄弟，不是替换", async () => {
    seeded();
    await store().regenerate();
    // send 是 await 的，桩里那一发要有人把 Done 喂回去才收尾
    feed(done(["e3", "e4"]));
    const [question] = store().messages;
    const answer = store().messages[1];
    expect(answer.parentId).toBe(question.id);
    expect(store().offPath[0].parentId).toBe(question.id);
    expect(store().offPath).toHaveLength(1);
  });

  it("发出去的那一轮指名把分支末端移回原问题之后", async () => {
    seeded();
    await store().regenerate();
    const send = h.calls.find((call) => call.cmd === "chat_send");
    expect(send?.args.rewindTo).toBe("e1");
  });

  it("落盘的是整棵树：旧答案还在盘上，父子连得起来", async () => {
    seeded();
    await store().regenerate();
    feed(done(["e3", "e4"]));
    const records = savedOf(A);
    const last = records[records.length - 1];
    expect(last).toBeTruthy();
    const ids = last.messages.map((item) => item.id);
    expect(ids).toHaveLength(3);
    expect(ids).toContain("q1");
    expect(ids).toContain("a1"); // 这一条就是这条钉的全部意义：旧答案不再被从盘上抹掉
    // 一句问题 + 它的两个回答：父是 null / q1 / q1
    expect(last.messages.map((item) => item.parentId ?? null).sort()).toEqual([null, "q1", "q1"]);
  });
});

describe("编辑重发把改后那一支留在树上", () => {
  it("被改的那条之后的整段挪进 offPath，而不是删掉", () => {
    useChatStore.setState({
      messages: [
        msg({ id: "q1", role: "user", content: "原问题", entryIds: ["e1"] }),
        msg({ id: "a1", content: "旧答", parentId: "q1", entryIds: ["e2"] }),
        msg({ id: "q2", role: "user", content: "再问", parentId: "a1", entryIds: ["e3"] }),
        msg({ id: "a2", content: "再答", parentId: "q2", entryIds: ["e4"] }),
      ],
      offPath: [],
    });
    void store().editAndResend("q1", "改过的问法");
    expect(store().offPath.map((item) => item.id)).toEqual(["q1", "a1", "q2", "a2"]);
    // 改的是第一条 → 日志退到根之前，新贴的那句问题就是新的根
    const [question, reply] = store().messages;
    expect(question.content).toBe("改过的问法");
    expect(question.parentId).toBeNull();
    expect(reply.parentId).toBe(question.id);
  });
});

describe("切换分支", () => {
  function branched() {
    useChatStore.setState({
      messages: [
        msg({ id: "q1", role: "user", content: "问", entryIds: ["e1"] }),
        msg({ id: "b1", content: "第一支", parentId: "q1", entryIds: ["e2"] }),
      ],
      offPath: [msg({ id: "b2", content: "第二支", parentId: "q1", entryIds: ["e3"] })],
    });
  }

  it("重投影：换过去的那条成为当前路径，原来那条整段下来", async () => {
    branched();
    await store().switchBranch("b2");
    expect(store().messages.map((item) => item.id)).toEqual(["q1", "b2"]);
    expect(store().offPath.map((item) => item.id)).toEqual(["b1"]);
  });

  it("后端的分支末端跟着走，否则界面在 A 支而下一发挂在 B 支之后", async () => {
    branched();
    await store().switchBranch("b2");
    const navigate = h.calls.find((call) => call.cmd === "conversation_navigate");
    expect(navigate?.args.entryId).toBe("e3");
    expect(navigate?.args.conversationId).toBe(A);
  });

  it("那一支下面还有话时，落到它最深的一处，不是停在被点的那一条", async () => {
    useChatStore.setState({
      messages: [msg({ id: "q1", role: "user", content: "问", entryIds: ["e1"] })],
      offPath: [
        msg({ id: "b2", content: "第二支", parentId: "q1", entryIds: ["e3"] }),
        msg({ id: "c2", role: "user", content: "第二支里的追问", parentId: "b2", entryIds: ["e4"] }),
      ],
    });
    await store().switchBranch("b2");
    expect(store().messages.map((item) => item.id)).toEqual(["q1", "b2", "c2"]);
  });

  it("正在跑的那一轮不许被换掉底（一把锁仍是一话题一现场）", async () => {
    branched();
    useChatStore.setState({ pending: true });
    await store().switchBranch("b2");
    expect(store().messages.map((item) => item.id)).toEqual(["q1", "b1"]);
    expect(h.calls.some((call) => call.cmd === "conversation_navigate")).toBe(false);
  });
});

describe("重开话题时站在哪一支由后端决定", () => {
  const tree = (tip: string | null): ConversationTree => ({
    tip,
    nodes: [
      { id: "e1", parentId: null, seq: 1, kind: "message", role: "user", at: 1, onPath: true },
      { id: "e3", parentId: "e1", seq: 2, kind: "message", role: "assistant", at: 2, onPath: true },
    ],
  });
  const archive = (id: string): ConversationRecord => ({
    id,
    projectId: "proj-1",
    title: "甲",
    kind: "chat",
    createdAt: 1,
    updatedAt: 1,
    pinned: false,
    usage: undefined,
    messages: [
      msg({ id: "q1", role: "user", content: "问", entryIds: ["e1"] }),
      msg({ id: "b1", content: "第一支", parentId: "q1", entryIds: ["e2"] }),
      msg({ id: "b2", content: "第二支", parentId: "q1", entryIds: ["e3"] }),
    ],
  });

  it("tip 指向第二支的条目 → 看得见的是第二支那条路径", async () => {
    h.archive = archive("conv-open-x");
    h.tree = tree("e3");
    await store().openConversation("conv-open-x");
    expect(store().messages.map((item) => item.id)).toEqual(["q1", "b2"]);
    expect(store().offPath.map((item) => item.id)).toEqual(["b1"]);
  });

  it("过渡气泡从不盖章 parentId：重开时按插入序接回去，不把前面的对话截掉", async () => {
    // 现场是"问 → 答 → 插话"，最后那句是 steer 贴的，它没有 parentId
    h.archive = {
      id: "conv-open-pos",
      projectId: "proj-1",
      title: "甲",
      kind: "chat",
      createdAt: 1,
      updatedAt: 1,
      pinned: false,
      usage: undefined,
      messages: [
        msg({ id: "q1", role: "user", content: "问", parentId: null, entryIds: ["e1"] }),
        msg({ id: "a1", content: "答", parentId: "q1", entryIds: ["e2"] }),
        msg({ id: "st", role: "user", content: "插一句", entryIds: ["e9"] }),
      ],
    };
    h.tree = { tip: "e9", nodes: [] };
    await store().openConversation("conv-open-pos");
    expect(store().messages.map((item) => item.id)).toEqual(["q1", "a1", "st"]);
    expect(store().offPath).toEqual([]);
  });

  it("读不到树（旧后端、旧存档）就整份按插入序显示，offPath 留空", async () => {
    h.archive = archive("conv-open-y");
    h.treeFails = true;
    await store().openConversation("conv-open-y");
    expect(store().messages).toHaveLength(3);
    expect(store().offPath).toEqual([]);
  });

  it("树在但 tip 为空：退回最后插入那条是末端——不把兄弟全摊开", async () => {
    h.archive = archive("conv-open-z");
    h.tree = tree(null);
    await store().openConversation("conv-open-z");
    expect(store().messages.map((item) => item.id)).toEqual(["q1", "b2"]);
    expect(store().offPath.map((item) => item.id)).toEqual(["b1"]);
  });
});

describe("输入框旁选工作目录的归属语义", () => {
  /** project_select 回给前端的那份配置：默认项目已切过去，带齐后续动作要读的格 */
  const selectConfig = (activeProjectId: string) => ({
    activeProjectId,
    kindModels: {},
    profiles: [],
    models: [],
    modelPool: { mode: "off", strategy: "failover", members: [], pinned: null },
  });

  it("未绑定且带历史的话题：选择即移动（用户拍板的语义，别再改回开新话题）", async () => {
    await store().send("先聊几句");
    feed(done(["e1", "e2"]));
    useChatStore.setState({ projectId: "" });
    const oldId = store().activeId;

    h.selectConfig = selectConfig("proj-new");
    await store().chooseProject("proj-new");

    expect(store().projectId).toBe("proj-new");
    expect(store().activeId).toBe(oldId);
  });

  it("已绑定的话题：选择即移动（选择器本来的职责）", async () => {
    useChatStore.setState({ projectId: "proj-1" });
    const oldId = store().activeId;

    h.selectConfig = selectConfig("proj-2");
    await store().chooseProject("proj-2");

    expect(store().activeId).toBe(oldId);
    expect(store().projectId).toBe("proj-2");
  });

  it("侧栏切换器（rebindCurrent: false）：只切默认，老话题不被拽走", async () => {
    useChatStore.setState({ projectId: "proj-1" });
    h.selectConfig = selectConfig("proj-2");
    await store().chooseProject("proj-2", { rebindCurrent: false });

    expect(store().projectId).toBe("proj-1");
    expect(store().config.activeProjectId).toBe("proj-2");
  });

  it("「不绑定」那一项：当前话题解绑，落在不绑定分组", async () => {
    useChatStore.setState({ projectId: "proj-1" });

    h.selectConfig = selectConfig("");
    await store().chooseProject("");

    expect(store().projectId).toBe("");
  });
});

describe("生成会话的发送清场", () => {
  it("参考图随问题气泡走，输入框即清空（真机踩过：留在输入框且气泡上没有）", async () => {
    useChatStore.setState({
      kind: "image",
      projectId: "",
      attachments: [
        {
          id: "att-1",
          name: "ref.png",
          path: "C:/pics/ref.png",
          chars: 1280 * 1024,
          truncated: false,
          kind: "image",
          previewDataUrl: "data:image/png;base64,AAAA",
        },
      ],
    });

    await store().sendMedia("给我生成一张类似参考图的图片");

    const [question] = store().messages;
    expect(question.role).toBe("user");
    expect(question.attachments?.[0]).toMatchObject({
      name: "ref.png",
      kind: "image",
      path: "C:/pics/ref.png",
    });
    expect(store().attachments).toEqual([]);
    const mediaCall = h.calls.find((call) => call.cmd === "media_generate");
    expect(mediaCall?.args.referenceImages).toEqual(["C:/pics/ref.png"]);
  });

  it("视频会话的剧本拼进生成提示词（生成管线没有对话历史）", async () => {
    useChatStore.setState({
      kind: "video",
      projectId: "",
      attachments: [
        {
          id: "att-2",
          name: "script.txt",
          path: "C:/docs/script.txt",
          chars: 12,
          truncated: false,
          text: "第一场：城市漫游",
        },
      ],
    });

    await store().sendMedia("按剧本拍");

    const mediaCall = h.calls.find((call) => call.cmd === "media_generate");
    expect(String(mediaCall?.args.prompt)).toContain("第一场：城市漫游");
    const [question] = store().messages;
    expect(question.attachments?.[0]?.name).toBe("script.txt");
    expect(store().attachments).toEqual([]);
  });

});
