/**
 * 作业模式在前端该长成什么样。
 *
 * 钉的八件事，都是"后端那边对、这边也会坏"的那一类：
 * 1. 后端说"还有一轮要自己接"时，Done 不许把这一轮落定——一落定，续跑那一轮的字节
 *    会被 `run.settled` 整批丢掉，界面看着像停了而线程还在跑；
 * 2. 续跑那一轮的第一个事件要开一条新气泡，不能把字接在上一轮后面；
 * 3. 一条 `mode` 事件都没有的普通轮次，行为与改动前一致（默认档零变化）；
 * 4. 读数按话题对号，切走的那条不该盖到当前界面上；
 * 5. 切模式失败要说得出原因，并且不许留下"界面切了、日志没切"的那一份；
 * 6. **目标挂在话题上**：读数写着 `mode: "chat"` 而身上有目标时，面板与选择器都得
 *    照认这条目标——切档不再把它挪进任何第二落点；
 * 7. 点名的服务商档案要按后端要的名字交下去，读数里那一格也要投影出来；
 * 8. 那一轮还在跑时切档/结束目标是"排在后面"而不是"被拒"，界面上得说一句。
 */
import { beforeEach, describe, expect, it, vi } from "vitest";
import { useChatStore } from "@/store/chat-store";
import type { ChatEvent, ConversationRecord, ModeState } from "@/types/chat";

const h = vi.hoisted(() => ({
  calls: [] as Array<{ cmd: string; args: Record<string, unknown> }>,
  channels: [] as Array<{ onmessage?: (event: ChatEvent) => void }>,
  fails: new Set<string>(),
  /** 桩后面的那份日志：读与写都回它 */
  held: null as ModeState | null,
  /** 三条会改日志的命令的 deferred：回合还在跑、那一行要等收尾才落 */
  deferPause: false,
  deferSet: false,
  deferDiscard: false,
  /** 「继续」开出的广播轮走 chat-event：这里收着每个监听者，测试直接喂事件 */
  broadcast: [] as Array<(payload: unknown) => void>,
  /** 启动扫描（goals_overview）的回货：测试按用例改它 */
  overview: [] as unknown[],
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
    h.calls.push({ cmd, args });
    if (h.fails.has(cmd)) return Promise.reject(new Error(`命令被拒：${cmd}`));
    switch (cmd) {
      case "chat_send":
        h.channels.push(args.onEvent as { onmessage?: (event: ChatEvent) => void });
        return Promise.resolve(undefined);
      case "config_get":
        // bootstrap 要的最小形状：ui 四格 + profiles。少一格它就先摔在界线之前。
        // showReasoning 也在最小形状里：广播轮的思考增量认这一格，缺了会被读成"关"
        return Promise.resolve({
          ui: { sidebarCollapsed: false, panelCollapsed: false, panelTab: "decision", section: "chats" },
          profiles: [],
          showReasoning: true,
        });
      case "history_list":
        return Promise.resolve([]);
      case "session_mode_get":
      case "session_goal_resume":
        return Promise.resolve(h.held);
      case "goals_overview":
        return Promise.resolve(h.overview);
      case "chat_follow_up":
        // 后端回的是排队后的长度，前端拿它显示"已排队 N 条"
        return Promise.resolve(1);
      // 三条会改日志的命令回的是同一个读数包：`view` 是那一行落下之后的读数，
      // `deferred` 说那一行还没落（回合还在跑）。定目标与编辑目标回的是同一种包
      case "session_mode_set":
      case "session_goal_set":
      case "session_goal_edit":
        return Promise.resolve({ view: h.held, deferred: h.deferSet });
      case "command_risk":
        return Promise.resolve("safe");
      case "goal_criteria_draft":
        return Promise.resolve({ criteria: [], constraints: [] });
      case "session_goal_pause":
        return Promise.resolve({ view: h.held, deferred: h.deferPause });
      case "session_goal_discard":
        return Promise.resolve({ view: h.held, deferred: h.deferDiscard });
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
  listen: (_event: string, handler: (payload: unknown) => void) => {
    h.broadcast.push(handler);
    return Promise.resolve(() => undefined);
  },
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

/** 事件通道里第 n 发送出去的那一轮 */
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

const goal = (over: Partial<ModeState> = {}): ModeState => ({
  mode: "goal",
  objective: "把三处对账补齐",
  turnsUsed: 0,
  maxCostUsdE8: 0,
  spentUsdE8: 0,
  status: "active",
  note: null,

  profile: null,
  goalId: "goal-1",
  contract: null,
  planReady: false,
  ...over,
});

/** store 是模块级单例，`runs` 那张表不随 setState 清空：每条用例用自己的话题 id */
let caseNo = 0;
let A = "conv-0";

beforeEach(() => {
  caseNo += 1;
  A = `conv-mode-${caseNo}`;
  h.calls.length = 0;
  h.channels.length = 0;
  h.fails.clear();
  h.held = null;
  h.deferPause = false;
  h.deferSet = false;
  h.deferDiscard = false;
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

describe("目标模式的自动续跑", () => {
  it("后端说还有下一轮时，Done 不落定，那一轮开一条新气泡", async () => {
    await store().send("甲的第一句");
    feed({ type: "delta", text: "第一轮做了一半" });
    feed({ type: "mode", continuing: true, state: goal({ turnsUsed: 1 }) });
    feed(done(["e1", "e2"]));

    // 这一轮答完了但这一支没停：现场必须还在，否则后面的字节整批被丢
    expect(store().pending).toBe(false); // 上一轮的流收起了
    expect(store().runningIds).toContain(A); // 而这一发还没交还给存档
    expect(store().mode?.turnsUsed).toBe(1);

    feed({ type: "delta", text: "接着把剩下的做完" }, 0);
    const assistants = store().messages.filter((message) => message.role === "assistant");
    expect(assistants).toHaveLength(2);
    // 续跑那一轮不许把字接在第一轮后面——那是两次回答，不是一句长话
    expect(assistants[0].content).toBe("第一轮做了一半");

    feed({ type: "mode", continuing: false, state: goal({ turnsUsed: 2, status: "complete", note: "齐了" }) }, 0);
    feed(done(["e3", "e4"]), 0);
    expect(store().runningIds).not.toContain(A);
    expect(store().mode?.status).toBe("complete");
  });

  it("对照：一条 mode 事件都没有的普通轮次，Done 就是结束", async () => {
    await store().send("普通的一句");
    feed({ type: "delta", text: "答完了" });
    feed(done(["n1", "n2"]));
    expect(store().runningIds).not.toContain(A);
    expect(store().mode).toBeNull();
  });

  it("由推进中转为受阻要说一句就走，而不是把它写进模型正文", async () => {
    await store().send("甲的目标");
    feed({ type: "mode", continuing: true, state: goal({ turnsUsed: 1 }) });
    feed(done(["g1"]));
    feed({ type: "delta", text: "第二轮的话" });
    feed({
      type: "mode",
      continuing: false,
      state: goal({ turnsUsed: 2, status: "blocked", note: "轮次到顶" }),
    });
    feed(done(["g2"]));

    expect(store().toasts.map((toast) => toast.title)).toContain("目标停下了");
    expect(store().toasts.at(-1)?.detail).toBe("轮次到顶");
    // 正文里不该多出这句——它不是"这一轮残缺"，那一轮答完了
    expect(store().messages.at(-1)?.content).toBe("第二轮的话");
  });
});

describe("模式读数的来路", () => {
  it("loadMode 只投影当前话题的那一份", async () => {
    h.held = goal({ turnsUsed: 2 });
    await store().loadMode(A);
    expect(store().mode?.turnsUsed).toBe(2);
    expect(store().modeError).toBeNull();

    // 人已经切走了：属于别的话题的读数不该盖到当前界面上
    h.held = goal({ turnsUsed: 3 });
    await store().loadMode("conv-别条话题");
    expect(store().mode?.turnsUsed).toBe(2);
  });

  it("读不到就清成不知道，而不是留着上一条话题的那一份", async () => {
    h.held = goal();
    await store().loadMode(A);
    expect(store().mode).not.toBeNull();

    h.held = null;
    h.fails.add("session_mode_get");
    await store().loadMode(A);
    expect(store().mode).toBeNull();
    expect(store().modeError).toContain("命令被拒");
  });

  it("切模式成功时投影后端返回的读数，参数按后端要的名字交下去", async () => {
    h.held = { ...goal(), mode: "plan", objective: null };
    const error = await store().setMode({ mode: "plan" });
    expect(error).toBeNull();
    expect(store().mode?.mode).toBe("plan");

    const call = h.calls.find((item) => item.cmd === "session_mode_set");
    expect(call?.args).toMatchObject({
      conversationId: A,
      mode: "plan",
    });
    // 切档那条命令不再收目标：一格命令改一件事（§3.2）
    expect(call?.args).not.toHaveProperty("objective");
  });

  it("定目标失败要说得出原因，且界面不许自己先切过去", async () => {
    h.held = null;
    h.fails.add("session_goal_set");
    const error = await store().goalSet({
      objective: "随便一个目标",
      criteria: [{ text: "x", kind: "check", command: "y" }],
      constraints: [],
      force: false,
    });
    expect(error).toContain("命令被拒：session_goal_set");
    expect(store().mode).toBeNull();
    expect(store().modeError).toContain("命令被拒");
    expect(store().modeBusy).toBe(false);
  });
});

describe("目标的暂停与继续", () => {
  /** 面板里属于本条用例话题的那一条（store 的面板账跨用例累积，按话题对号取） */
  const panelEntry = () => store().goalRuns.find((entry) => entry.conversationId === A);

  it("空闲时暂停直接投影「已暂停」，继续把读数翻回来并标推进中", async () => {
    h.held = goal({ status: "paused" });
    const error = await store().goalPause(A, true);
    expect(error).toBeNull();
    expect(h.calls.find((item) => item.cmd === "session_goal_pause")?.args).toMatchObject({
      conversationId: A,
      // 命令那一头仍是"要不要暂停"这个布尔：六值是**读数**的形状，不是命令的形状
      paused: true,
    });
    expect(store().mode?.status).toBe("paused");
    expect(panelEntry()?.status).toBe("paused");

    h.held = goal({ status: "active", turnsUsed: 3 });
    const resumeError = await store().goalResume(A);
    expect(resumeError).toBeNull();
    expect(h.calls.find((item) => item.cmd === "session_goal_resume")?.args).toMatchObject({
      conversationId: A,
    });
    expect(store().mode?.turnsUsed).toBe(3);
    const entry = panelEntry();
    expect(entry?.status).toBe("active");
    // "继续"开出的那一轮在跑：面板要当场把它标成推进中
    expect(entry?.pending).toBe(true);
  });

  it("回合还在跑时暂停先乐观标注，收尾的读数来对账", async () => {
    await store().send("推进中的目标");
    h.deferPause = true;
    h.held = goal({ status: "active", turnsUsed: 1 });
    const error = await store().goalPause(A, true);
    expect(error).toBeNull();
    // 那一行还没落，但界面上那格先标着——不标的话用户按了暂停却看不出任何变化
    expect(panelEntry()?.status).toBe("paused");

    // 收尾判据落了行：读数带着 paused 来，乐观那一格就该收掉
    feed({ type: "mode", continuing: false, state: goal({ status: "paused", turnsUsed: 1 }) });
    feed(done(["p1"]));
    expect(store().mode?.status).toBe("paused");
    expect(panelEntry()?.status).toBe("paused");
    expect(panelEntry()?.pending).toBe(false);
  });

  it("乐观那一格只覆盖 active：钱到顶的目标不许被显示成「已暂停」", async () => {
    // 出路不同：budget_limited 要人去调上限，paused 要人按继续。
    // 覆盖上去就是把一句"该掏钱"显示成"接着跑"，而那正是唯一自动刹车咬下来的那一格
    await store().send("推进中的目标");
    h.deferPause = true;
    h.held = goal({ status: "budget_limited", turnsUsed: 4 });
    expect(await store().goalPause(A, true)).toBeNull();
    expect(panelEntry()?.status).toBe("budget_limited");
  });

  it("暂停被拒要说得出原因，读数维持原样", async () => {
    h.held = goal({ turnsUsed: 2 });
    await store().loadMode(A);
    h.fails.add("session_goal_pause");
    const error = await store().goalPause(A, true);
    expect(error).toContain("命令被拒：session_goal_pause");
    expect(store().mode?.status).toBe("active");
    expect(panelEntry()?.status).toBe("active");
  });

  it("切到对话档目标留在主格、面板照认；结束才摘掉", async () => {
    h.held = goal({ turnsUsed: 2 });
    await store().loadMode(A);
    expect(panelEntry()).toBeDefined();

    // 切到对话档：后端把目标整份留在读数的主格里，`mode` 那一格换成 chat。
    // 面板认的是"身上挂着目标"，所以这条不该因为换了档就变成暂停、更不该消失
    h.held = goal({ mode: "chat", turnsUsed: 2 });
    const switchError = await store().setMode({ mode: "chat" });
    expect(switchError).toBeNull();
    const kept = panelEntry();
    expect(kept).toBeDefined();
    expect(kept?.mode).toBe("chat");
    expect(kept?.objective).toBe("把三处对账补齐");
    expect(kept?.turnsUsed).toBe(2);
    // 这一条就是"切档不再挂起"在界面上的形状：没人按暂停，卡上就不许出现暂停
    expect(kept?.status).toBe("active");

    // 结束才是真正的销毁：读数没有目标了，面板才摘掉
    h.held = { ...goal(), mode: "chat", objective: null };
    const error = await store().goalDiscard(A);
    expect(error).toBeNull();
    expect(h.calls.find((item) => item.cmd === "session_goal_discard")?.args).toMatchObject({
      conversationId: A,
    });
    expect(panelEntry()).toBeUndefined();
  });

  it("对话档上暂停过的目标，卡上写着已暂停，按继续把读数翻回来", async () => {
    // 规划档替用户按的那次暂停会跟着切回对话档：paused 自己是一格，不是收尾
    h.held = goal({ mode: "chat", status: "paused", turnsUsed: 3 });
    await store().loadMode(A);
    const entry = panelEntry();
    expect(entry).toBeDefined();
    expect(entry?.status).toBe("paused");
    expect(entry?.mode).toBe("chat");

    // 继续推进：后端翻了旗子并立刻接一轮，读数整份换回来
    h.held = goal({ mode: "chat", turnsUsed: 4 });
    const error = await store().goalResume(A);
    expect(error).toBeNull();
    const resumed = panelEntry();
    expect(resumed?.status).toBe("active");
    expect(resumed?.pending).toBe(true);
    expect(resumed?.turnsUsed).toBe(4);
  });

  it("点名的服务商档案按后端要的名字交下去，读数里那一格也投影出来", async () => {
    h.held = goal({ profile: "profile-deep" });
    const error = await store().goalSet({
      objective: "把三处对账补齐",
      criteria: [{ text: "x", kind: "check", command: "y" }],
      constraints: [],
      profile: "profile-deep",
      force: false,
    });
    expect(error).toBeNull();
    expect(h.calls.find((item) => item.cmd === "session_goal_set")?.args).toMatchObject({
      conversationId: A,
      objective: "把三处对账补齐",
      profile: "profile-deep",
    });
    expect(store().mode?.profile).toBe("profile-deep");
    expect(panelEntry()?.profile).toBe("profile-deep");

    // 没点名也要把 null 交下去，而不是留一个 undefined 让后端看心情
    h.held = goal();
    await store().goalSet({
      objective: "另一件事",
      criteria: [{ text: "x", kind: "check", command: "y" }],
      constraints: [],
      force: false,
    });
    expect(h.calls.at(-1)?.args).toMatchObject({ profile: null });
    expect(store().mode?.profile).toBeNull();
  });

  it("那一轮还在跑时切档要说一句\"排在这轮后面\"，而不是静悄悄没换", async () => {
    await store().send("推进中的目标");
    // 后端收了请求、立在登记表里，返回的还是旧档位那一行 + deferred
    h.held = goal({ turnsUsed: 1 });
    h.deferSet = true;
    const error = await store().setMode({ mode: "plan" });
    expect(error).toBeNull();
    // 界面不假装已经切过去了：读数还是后端回的那一份
    expect(store().mode?.mode).toBe("goal");
    // 但这一格必须说得出"排着"——否则用户只会以为没点上，再按一次
    expect(store().toasts.map((toast) => toast.title)).toContain("排在这一轮后面");
  });

  it("那一轮还在跑时结束目标同样排在后面，读数不许提前摘掉那条", async () => {
    h.held = goal({ turnsUsed: 2 });
    await store().loadMode(A);
    expect(panelEntry()).toBeDefined();

    // deferred 时后端回的是**还没清掉目标**的那一份：面板此时不该摘
    h.deferDiscard = true;
    const error = await store().goalDiscard(A);
    expect(error).toBeNull();
    expect(panelEntry()).toBeDefined();
    expect(store().toasts.map((toast) => toast.title)).toContain("结束目标排在这一轮后面");

    // 收尾那一行落了：读数没有目标，这一条才从面板上消失
    h.held = { ...goal(), mode: "chat", objective: null };
    h.deferDiscard = false;
    await store().goalDiscard(A);
    expect(panelEntry()).toBeUndefined();
  });
});

describe("目标在跑时的发送路由：只有插话这一条（插话策略那一格已删）", () => {
  /** 面板里属于本条用例话题的那一条（store 的面板账跨用例累积，按话题对号取） */
  const panelEntry = () => store().goalRuns.find((entry) => entry.conversationId === A);

  it("「继续」开出的广播轮在跑时，再发的话转成插话而不是并行回合", async () => {
    h.held = goal({ turnsUsed: 2 });
    await store().loadMode(A);
    await store().goalResume(A);
    await store().send("这句该成为插话");

    expect(h.calls.find((item) => item.cmd === "chat_steer")?.args).toMatchObject({
      conversationId: A,
      text: "这句该成为插话",
    });
    // 并行的第二个回合线程不许开：它会同写一份日志，后收尾的整片盖掉先收尾的
    expect(h.calls.some((item) => item.cmd === "chat_send")).toBe(false);
    // 也不许自动转成排队。队列是 Ctrl+回车那条**由人明说**的路，不是目标替人决定的
    // （反向钉：从前独占档会把它悄悄塞进 follow_up，用户按的是回车，得到的是排队）
    expect(h.calls.some((item) => item.cmd === "chat_follow_up")).toBe(false);
    expect(panelEntry()?.turnsUsed).toBe(2);
  });

  it("广播的 mode 事件要同步眼前话题的读数：目标报完那一刻卡片就得翻面", async () => {
    // bootstrap 把广播监听接上（wireGoalEvents 只在这里挂）；它末尾 startFresh
    // 会另起新话题，把眼前话题拨回用例的这条
    await store().bootstrap();
    useChatStore.setState({ activeId: A });

    // 目标先在账上（guard：广播事件只认账上有的话题），眼前读数还是推进中
    h.held = goal({ turnsUsed: 3 });
    await store().loadMode(A);
    expect(store().mode?.status).toBe("active");

    // 目标报完的那一轮：mode{continuing:false, complete} 从广播来。
    // 不同步眼前读数的话，面板第二优先级会拿屏上这份旧读数把"完成"盖回"推进中"
    expect(h.broadcast.length).toBeGreaterThan(0);
    h.broadcast[0]({
      payload: {
        conversationId: A,
        event: {
          type: "mode",
          continuing: false,
          state: goal({ turnsUsed: 4, status: "complete", note: "齐了" }),
        },
      },
    });

    expect(store().mode?.status).toBe("complete");
    expect(store().mode?.turnsUsed).toBe(4);
    const entry = panelEntry();
    expect(entry?.status).toBe("complete");
    expect(entry?.pending).toBe(false);
  });
});

describe("定目标即开工，而且就在这一支话题里看得见（D16 / D17）", () => {
  /** 广播事件的入口（`wireGoalEvents` 由 bootstrap 挂上，harness 把 handler 收进数组） */
  const emit = (event: Record<string, unknown>, conversationId = A) =>
    h.broadcast[0]({ payload: { conversationId, event } });

  /** 把眼前话题立成"刚定下目标、后端已经开了第一轮"的形状 */
  async function armFirstRound(turnsUsed = 1) {
    await store().bootstrap();
    useChatStore.setState({ activeId: A });
    h.held = goal({ mode: "goal", turnsUsed });
    expect(
      await store().goalSet({
        objective: "把三处对账补齐",
        criteria: [{ text: "测试全绿", kind: "check", command: "npm test" }],
        constraints: [],
        force: false,
      }),
    ).toBeNull();
  }

  it("按下「开始推进」后不需要人再发任何东西，广播来的字节就接成这一支里的活气泡", async () => {
    await armFirstRound();
    const before = store().messages.length;

    emit({ type: "delta", text: "先看台账" });
    emit({ type: "delta", text: "那两处" });

    const added = store().messages.slice(before);
    // 两个字节接进**同一条**气泡：一帧一条会把屏幕刷成字模
    expect(added).toHaveLength(1);
    expect(added[0].content).toBe("先看台账那两处");
    expect(added[0].streaming).toBe(true);
    // 轮数标的是后端 armed 的那一轮。从前这一轮的字节只进角落那截尾巴，
    // 整轮跑完才"啪"地出现——用户问的"在当前话题执行显示"缺的就是这一段
    expect(added[0].goalRound).toBe(1);
  });

  it("目标报完时活气泡要落定，不许一直挂着「生成中」", async () => {
    await armFirstRound(2);
    emit({ type: "delta", text: "最后一截" });
    expect(store().messages.at(-1)?.streaming).toBe(true);

    emit({
      type: "mode",
      continuing: false,
      state: goal({ mode: "goal", turnsUsed: 2, status: "complete", note: "齐了" }),
    });
    expect(store().messages.at(-1)?.streaming).toBe(false);
  });

  it("下一轮的字节另开一条，并标成下一轮（continuing 报的是刚跑完那一轮的账）", async () => {
    await armFirstRound(1);
    emit({ type: "delta", text: "第一轮" });
    emit({ type: "mode", continuing: true, state: goal({ mode: "goal", turnsUsed: 1 }) });
    const before = store().messages.length;
    emit({ type: "delta", text: "第二轮" });

    const added = store().messages.slice(before);
    expect(added).toHaveLength(1);
    expect(added[0].content).toBe("第二轮");
    expect(added[0].goalRound).toBe(2);
  });

  it("没在看的那一支不接活气泡：字节仍进角落那截尾巴", async () => {
    await armFirstRound();
    useChatStore.setState({ activeId: "conv-别的话题" });
    const before = store().messages.length;

    emit({ type: "delta", text: "后台在跑" }, A);
    // 眼前这条话题一个字都不该多——它是另一支话题的目标
    expect(store().messages).toHaveLength(before);
  });

  it("广播轮的思考与工具过程也接进活气泡：同一条气泡里看得到它在读文件、跑命令", async () => {
    await armFirstRound();
    const before = store().messages.length;

    // 先想后做：思考增量起一条气泡，正文一开口"正在想"就收
    emit({ type: "reasoning", text: "先看有没有现成文件" });
    const bubble = store().messages.at(-1);
    expect(store().messages.length).toBe(before + 1);
    expect(bubble?.reasoning).toContain("先看有没有现成文件");
    expect(bubble?.reasoningStreaming).toBe(true);

    emit({ type: "delta", text: "我先看一下工作目录" });
    expect(store().messages.at(-1)?.reasoningStreaming).toBe(false);

    // 工具三态走同一条气泡同一张卡：running 起卡，done 补输出，不另开第二条
    emit({
      type: "tool",
      id: "t1",
      name: "list_files",
      status: "running",
      risk: "safe",
      input: "src",
      passReason: null,
    });
    emit({
      type: "tool",
      id: "t1",
      name: "list_files",
      status: "done",
      risk: "safe",
      input: "src",
      output: "index.html",
      passReason: null,
    });
    emit({
      type: "tool",
      id: "t2",
      name: "write_file",
      status: "running",
      risk: "elevated",
      input: "index.html",
      passReason: null,
    });

    const held = store().messages.at(-1);
    expect(held?.id).toBe(bubble?.id);
    expect(held?.toolCalls).toHaveLength(2);
    expect(held?.toolCalls?.[0]).toMatchObject({ id: "t1", status: "done", output: "index.html" });
    expect(held?.toolCalls?.[1]).toMatchObject({ id: "t2", status: "running" });

    // 收口：思考流与气泡一起落定
    emit({
      type: "mode",
      continuing: false,
      state: goal({ mode: "goal", turnsUsed: 1, status: "active" }),
    });
    expect(store().messages.at(-1)?.streaming).toBe(false);
    expect(store().messages.at(-1)?.reasoningStreaming).toBe(false);
  });
});

describe("启动扫描：挂着的目标要看得见（D8）", () => {
  const parkedRow = {
    conversationId: "conv-重启前就在跑",
    title: "可乐官网",
    mode: "chat",
    objective: "用 html 写一个可乐官网",
    note: null,
    status: "paused",
    turnsUsed: 4,
    maxCostUsdE8: 0,
    spentUsdE8: 120_000_000,
    profile: null,
    parkedByRestart: true,
  };
  const rowFor = (id: string) =>
    store().goalRuns.find((entry) => entry.conversationId === id);

  it("重启把推进中的目标按停了，这一条就要出现在面板上并说一声", async () => {
    h.overview = [parkedRow];
    await store().bootstrap();

    const entry = rowFor(parkedRow.conversationId);
    // 这一条是 D8 的全部理由：从前面板只认"这一程被事件或按钮碰过"的话题，
    // 于是重启后一支会自己花钱的目标在屏幕上不存在
    expect(entry?.objective).toBe("用 html 写一个可乐官网");
    expect(entry?.status).toBe("paused");
    expect(entry?.turnsUsed).toBe(4);
    expect(store().toasts.map((toast) => toast.title)).toContain("一支目标还挂着，已按暂停");
    h.overview = [];
  });

  it("设置里开了自动继续（没被按住）：照旧列出来，但不重复说那一句", async () => {
    h.overview = [{ ...parkedRow, parkedByRestart: false, status: "active" }];
    await store().bootstrap();

    expect(rowFor(parkedRow.conversationId)?.status).toBe("active");
    expect(store().toasts.map((toast) => toast.title)).not.toContain("一支目标还挂着，已按暂停");
    h.overview = [];
  });

  it("扫描失败不许卡住启动：面板少几条，比屏幕上一句报错强", async () => {
    h.overview = [];
    h.fails.add("goals_overview");
    await store().bootstrap();
    h.fails.delete("goals_overview");
    expect(store().configLoaded).toBe(true);
  });
});
