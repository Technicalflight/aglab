/**
 * 切走话题之后，正在跑的那一轮该继续跑完（回归测试）。
 *
 * 坏掉的样子（本次改动之前的实现）：`apply` 里一句 `if (activeId !== ownerId) return`
 * 把切走期间的增量整个丢掉，连带把 Done 也丢了 → 那一轮再也不收尾；
 * 而切走那一刻 `openConversation` 先把半截正文落了盘。于是切回来看到的是一条
 * 断在中间、不再动、也没有"正在生成"的回答。
 *
 * 这里的桩只钉到 `@tauri-apps/api/core` 那一层：`chat_send` **立刻返回**，
 * 增量由测试自己从那条 Channel 上喂下去——和真后端"开线程就返回、事件陆续到达"同形。
 * 那个"立刻返回"正是当年把 `finally` 里收口动作提前触发、从而让停止按钮失效的机制。
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useChatStore } from "@/store/chat-store";
import type { ChatEvent, ConversationRecord, Message } from "@/types/chat";

const h = vi.hoisted(() => ({
  calls: [] as Array<{ cmd: string; args: Record<string, unknown> }>,
  channels: [] as Array<{ onmessage?: (event: ChatEvent) => void }>,
  /** 这些命令一律拒绝：用来钉"后端报错时那句话去得了哪里" */
  fails: new Set<string>(),
}));

/** 被测代码要的只是"有一个能停下来的定时器"。测试里不必真走时，记账即可——
 *  于是"看门狗到底有没有被停掉"也成了一个能读的数 */
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
    if (h.fails.has(cmd)) return Promise.reject(new Error(`命令被拒：${cmd}`));
    switch (cmd) {
      case "chat_send":
        h.channels.push(args.onEvent as { onmessage?: (event: ChatEvent) => void });
        return Promise.resolve(undefined);
      case "history_load":
        return Promise.resolve({
          id: args.id,
          projectId: "proj-存档",
          title: `存档 ${args.id}`,
          createdAt: 1,
          updatedAt: 1,
          messages: [],
          usage: undefined,
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
      case "chat_follow_up":
        return Promise.resolve(1);
      case "edits_for_session":
        return Promise.resolve([]);
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
const slept = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

/** 第 n 发送出去的那一轮的事件通道 */
const channel = (n = 0) => h.channels[n];
const feed = (event: ChatEvent, n = 0) => channel(n)?.onmessage?.(event);
const savedOf = (id: string) =>
  h.calls
    .filter((call) => call.cmd === "history_save")
    .map((call) => call.args.conversation as ConversationRecord)
    .filter((record) => record.id === id);
const aborted = () =>
  h.calls.filter((call) => call.cmd === "chat_abort").map((call) => String(call.args.conversationId));

const done = (entryIds: string[]): ChatEvent => ({
  type: "done",
  inputTokens: 10,
  outputTokens: 5,
  durationMs: 100,
  cachedTokens: null,
  entryIds,
  model: "测试模型",
});

/** store 是模块级单例，`runs` 那张表不随 setState 清空：每条用例用自己的一对话题 id，
 *  免得上一条没收干净的那一轮把这一条的 send 挡在门外 */
let caseNo = 0;
let A = "conv-0";
let B = "conv-0B";

beforeEach(() => {
  caseNo += 1;
  A = `conv-A${caseNo}`;
  B = `conv-B${caseNo}`;
  h.calls.length = 0;
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
    runningIds: [],
    conversations: [],
    toasts: [],
    section: "chats",
  });
});

afterEach(() => {
  timers.clear();
});

describe("新话题进侧栏名单的时机", () => {
  it("发出第一句就进名单，预落盘是壳：不带消息行（它归后端日志写）", async () => {
    await store().send("甲的第一句");
    // 以前这一格要等模型整轮答完才出现：中间那一整分钟列表里什么都没有
    expect(store().conversations.map((item) => item.id)).toEqual([A]);
    const saved = savedOf(A);
    expect(saved).toHaveLength(1);
    expect(saved[0].title).toBe("甲的第一句");
    // 壳里一条消息都没有：这份记录会在 chat_send 第一次开话题时被**整份迁移成
    // 日志**，乐观的用户气泡若在，迁移进日志后 chat_send 再追加一遍——
    // 同一句话在日志里两行，屏上"发一条显示两条"。消息行是后端的
    expect(saved[0].messages).toEqual([]);
    // 界面上那条空气泡照旧在（正在生成），只是它不进存档
    expect(store().messages.at(-1)?.streaming).toBe(true);
  });

  it("对照：同一条话题再发一轮，名单里不多出一个影子", async () => {
    await store().send("甲的第二句");
    feed({ type: "delta", text: "答一句" });
    feed(done(["x1", "x2"]));
    await store().send("甲的第三句");
    expect(store().conversations.filter((item) => item.id === A)).toHaveLength(1);
  });
});

describe("这一轮做了什么的流程", () => {
  const tool = (
    id: string,
    status: "running" | "done",
    name = "write_file",
    input = '{"path":"C:/repo/src/chat.rs"}',
  ): ChatEvent => ({
    type: "tool",
    id,
    name,
    status,
    risk: "safe",
    input,
    arguments: "{}",
    passReason: null,
  });

  it("按真实顺序记步：思考→工具→又思考，同一次调用只占一格", async () => {
    await store().send("甲的第十三问");
    feed({ type: "reasoning", text: "先看一眼文件" });
    feed(tool("c1", "running"));
    feed(tool("c1", "done"));
    feed({ type: "reasoning", text: "改完了，写回答" });
    feed({ type: "delta", text: "答案" });
    feed(done(["z1", "z2"]));

    const reply = store().messages.at(-1) as Message;
    expect(reply.steps?.map((step) => step.kind)).toEqual(["thinking", "tool", "thinking"]);
    // 第二段思考的起点必须是第一段末尾：偏了就会在流程里重复显示同一句
    expect(reply.steps?.[2]).toMatchObject({ kind: "thinking", from: "先看一眼文件".length });
    // 一次调用来了两个状态只占一格，摘要与状态由 toolCalls 那一份读
    expect(reply.steps?.filter((step) => step.kind === "tool")).toHaveLength(1);
    expect(reply.durationMs).toBeTypeOf("number");
  });

  it("关掉思考显示时流程里也不记思考步，正文也不在背后攒", async () => {
    useChatStore.setState({ config: { ...store().config, showReasoning: false } });
    await store().send("甲的第十四问");
    feed({ type: "reasoning", text: "看不见的一段" });
    feed(tool("c2", "running"));
    feed(done(["w1", "w2"]));

    const hidden = store().messages.at(-1) as Message;
    expect(hidden.steps?.map((step) => step.kind)).toEqual(["tool"]);
    expect(hidden.reasoning).toBeUndefined();
  });

  it("每个事件都换掉最后一条消息对象（滚动跟随靠的就是这个身份变化）", async () => {
    await store().send("甲的第十五问");
    feed({ type: "delta", text: "一段正文" });
    await slept(90);
    const afterText = store().messages.at(-1);
    expect(afterText).toBeDefined();

    // 正文一个字没长，只来了一个工具状态：仍然必须是新的那一个对象，
    // 否则"跟着最新内容滚"就会退化成"只有出字时才滚"
    feed(tool("c9", "running"));
    const afterTool = store().messages.at(-1);
    expect(afterTool).not.toBe(afterText);
    expect(afterTool?.content).toBe("一段正文");

    feed(tool("c9", "done"));
    expect(store().messages.at(-1)).not.toBe(afterTool);
    // 同一个 id 再来一次状态变化也换对象（流程里那一格不该重复长出来）
    expect(store().messages.at(-1)?.steps?.filter((step) => step.kind === "tool")).toHaveLength(1);
  });
});

describe("切走话题之后，正在跑的那一轮", () => {
  it("增量不丢：切回来接到的是完整的这一轮", async () => {
    await store().send("甲的第一问");
    await store().openConversation(B);

    feed({ type: "delta", text: "前半段" });
    feed({ type: "delta", text: "后半段" });
    feed(done(["e1", "e2"]));

    await store().openConversation(A);
    const reply = store().messages.filter((message) => message.role === "assistant").at(-1) as Message;
    expect(reply.content).toBe("前半段后半段");
    expect(reply.streaming).toBe(false);
    // 条目 id 也得贴回来：重新生成与编辑重发靠它指名"移到哪条之后"
    expect(reply.entryIds).toEqual(["e1", "e2"]);
    // 实发模型同样贴在这条消息上：那行归属读数不能拿"现在的配置"猜——池子换过人就不一样了
    expect(reply.model).toBe("测试模型");
  });

  it("切回来还在跑：pending 与流式标志如实恢复", async () => {
    await store().send("甲的第二问");
    await store().openConversation(B);
    feed({ type: "delta", text: "还在出字" });
    await slept(90); // 增量按 60ms 合并

    await store().openConversation(A);
    expect(store().pending).toBe(true);
    const reply = store().messages.at(-1) as Message;
    expect(reply.content).toBe("还在出字");
    expect(reply.streaming).toBe(true);

    feed(done(["e3", "e4"]));
    expect(store().pending).toBe(false);
  });

  it("切走那一刻不落半截盘，收尾才落完整的那一份", async () => {
    await store().send("甲的第三问");
    feed({ type: "delta", text: "前半段" });
    await slept(90); // 让合并缓冲先落地：界面此刻真的带着半截正文

    await store().openConversation(B);
    // 反向钉：这一轮还没跑完，存档上不许出现带着这半截正文的任何一份
    const before = savedOf(A);
    expect(before.length).toBeGreaterThan(0); // 发第一句时进的那一行（只有问题）
    expect(
      before.every((record) => !record.messages.some((message) => message.content.includes("前半段"))),
      "切走时把半截正文写进了存档",
    ).toBe(true);

    feed({ type: "delta", text: "后半段" });
    feed(done(["e5", "e6"]));
    expect(savedOf(A).at(-1)?.messages.at(-1)?.content).toBe("前半段后半段");
  });

  it("对照：没在跑的话题切走照常落盘（那条 0 不是因为存不进去）", async () => {
    // 不走 send：内容没经过任何一轮，落盘指纹对不上，切走必须写一次
    useChatStore.setState({
      messages: [{ id: "msg-hand", role: "user", content: "甲手贴的一条", createdAt: 1 }],
    });
    await store().openConversation(B);
    const saved = savedOf(A);
    expect(saved).toHaveLength(1);
    expect(saved[0].messages.at(-1)?.content).toBe("甲手贴的一条");
  });

  it("不污染正在看的那条话题", async () => {
    await store().send("甲的第五问");
    await store().openConversation(B);
    feed({ type: "delta", text: "不该出现在乙的正文里" });
    feed({ type: "reasoning", text: "也不该出现在乙的思考里" });
    await slept(90);

    expect(store().activeId).toBe(B);
    expect(store().pending).toBe(false);
    expect(store().messages.map((message) => message.content).join("")).not.toContain("不该出现");
    expect(store().runningIds).toEqual([A]);
  });

  it("停止按话题定位：chat_send 立刻返回之后仍然停得下，且点下去必有一句话", async () => {
    await store().send("甲的第六问");
    // chat_send 已经返回（真后端就是立即返回），停止按钮不能因此变成空转
    expect(aborted()).toEqual([]);
    // 回合还活着时看门狗必须在走——它以前被那个 finally 提前掐掉，90 秒提示于是永不响
    expect(timers.size).toBe(1);
    await store().stopGeneration();
    expect(aborted()).toEqual([A]);
    // 闸拉起来不等于立刻停：这一句必须先到，否则"点了没反应"没有任何证据可查
    expect(store().toasts.at(-1)?.title).toBe("已请求停止");
    // 后端停止时会补一发 Done，界面按正常收尾走
    feed(done(["e-stop"]));
    expect(store().runningIds).toEqual([]);
    expect(timers.size).toBe(0);
  });

  it("对照：眼前这条没在跑时，停止不发 abort，但要说出为什么没动", async () => {
    await store().openConversation(B);
    await store().stopGeneration();
    expect(aborted()).toEqual([]);
    expect(store().toasts.at(-1)?.title).toBe("这一条话题现在没在跑");
  });

  it("后端拒了停止：那句话进告警条，不是只进 console", async () => {
    await store().send("甲的第十二问");
    h.fails.add("chat_abort");
    await store().stopGeneration();
    const toast = store().toasts.at(-1);
    expect(toast?.title).toBe("停止没生效");
    expect(toast?.detail).toContain("chat_abort");
  });

  it("插话与排队只认自己那条话题：切走了不给别的话题排队", async () => {
    await store().send("甲的第七问");
    await store().openConversation(B);
    await store().followUp("这句本该排队到甲");
    expect(h.calls.some((call) => call.cmd === "chat_follow_up")).toBe(false);
    expect(store().followUpCount).toBe(0);
    expect(store().messages).toHaveLength(0);

    await store().openConversation(A);
    await store().followUp("这句排到甲");
    expect(h.calls.some((call) => call.cmd === "chat_follow_up")).toBe(true);
    expect(store().followUpCount).toBe(1);
    expect(store().messages.at(-1)?.content).toBe("这句排到甲");
  });

  it("并行两轮各自收尾，另一条不受影响", async () => {
    await store().send("甲的第八问");
    await store().openConversation(B);
    await store().send("乙的第一问");
    expect(store().runningIds).toEqual([A, B]);

    // 两条通道各喂一遍：甲先收，乙还在出字
    feed({ type: "delta", text: "甲的正文" }, 0);
    feed({ type: "delta", text: "乙的正文" }, 1);
    feed(done(["e9", "e10"]), 0);
    expect(store().runningIds).toEqual([B]);
    // 甲收尾不该把界面上乙的流式标志一起收掉
    expect(store().pending).toBe(true);
    expect((store().messages.at(-1) as Message).streaming).toBe(true);

    feed(done(["e11", "e12"]), 1);
    expect(store().runningIds).toEqual([]);
    expect(store().messages.at(-1)?.content).toBe("乙的正文");
    // 乙落的是乙的那一份，标题与正文都不串到甲
    expect(savedOf(B).at(-1)?.messages.at(-1)?.content).toBe("乙的正文");
    expect(savedOf(A).at(-1)?.messages.at(-1)?.content).toBe("甲的正文");
  });

  it("跟随轮照旧接得上，且幕间不再开出第二轮", async () => {
    await store().send("甲的第十问");
    await store().followUp("甲的排一句");
    feed({ type: "delta", text: "甲的第一轮正文" });
    feed(done(["e15", "e16"]));
    // 幕间：这一轮收尾了，但队列里还有一句要接话——现场必须还占着这条话题
    expect(store().pending).toBe(false);
    expect(store().runningIds).toEqual([A]);
    const before = store().messages.length;

    await store().send("幕间又发的一句");
    expect(h.channels).toHaveLength(1); // 没有第二轮被发出去
    expect(store().messages).toHaveLength(before); // 那句也不该贴进正文

    feed({ type: "delta", text: "甲的第二轮正文" });
    feed(done(["e17", "e18"]));
    expect(store().messages).toHaveLength(before + 1); // 只有跟随轮那一条新气泡
    expect(store().messages.at(-1)?.content).toBe("甲的第二轮正文");
    expect(store().runningIds).toEqual([]);
    // 第二条用户行是从队列里出来的，它带着后端登记回来的条目 id：
    // 认不出它，就等于界面不知道这句话在日志里的位置
    expect(store().messages.find((message) => message.content === "甲的排一句")?.entryIds).toEqual([
      "e17",
    ]);
  });

  it("删掉正在跑的话题：中止它，之后不再把存档写回来", async () => {
    await store().send("甲的第九问");
    await store().openConversation(B);
    await store().deleteConversation(A);
    expect(aborted()).toEqual([A]);
    expect(store().runningIds).toEqual([]);
    expect(store().activeId).not.toBe(A);

    h.calls.length = 0;
    // 服务商迟到的尾巴（停止前已经上路的事件）不该把已删的话题复活
    feed({ type: "delta", text: "迟到的正文" });
    feed(done(["e13", "e14"]));
    expect(savedOf(A)).toHaveLength(0);
  });
});

describe("决策嵌入的话题归属", () => {
  it("路由判定与提取门控都带着这场对话的 id——决策面板按话题过滤，这是它的写侧来源", async () => {
    const integrations = await import("@/lib/decision/integrations");
    await store().send("归属第一句");
    feed(done(["y1"]), 0);
    await slept(0);
    await store().send("归属第二句");
    feed(done(["y2"]), 1);
    await slept(0);
    await store().send("归属第三句");
    feed(done(["y3"]), 2);
    await slept(0);
    // 每条消息一次路由判定，三次都指向同一场对话
    expect(vi.mocked(integrations.routeModel).mock.calls.map((call) => call[2])).toEqual([A, A, A]);
    // 提取门控每 3 轮跑一次，跑的时候也要带上话题 id
    const gateCalls = vi.mocked(integrations.gateMemoryExtraction).mock.calls;
    expect(gateCalls.length).toBeGreaterThanOrEqual(1);
    expect(gateCalls.at(-1)?.[1]).toBe(A);
  });
});
