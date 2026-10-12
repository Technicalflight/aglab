/**
 * 生成档进场的右栏自动收缩钉。
 *
 * 视频/音乐工作区自带生成入口、要整幅横向空间，右栏（预览/终端/浏览器）
 * 挤在那儿碍事：选中「视频模式/音乐模式」开新档时顺手收掉。单向语义——
 * 切回对话/生图档不自动展开（展开权留在面板按钮上，与窄屏自动收同款）。
 *
 * 桩与 media 那份同形：只钉到 @tauri-apps/api/core 的 invoke。
 */
import { beforeEach, describe, expect, it, vi } from "vitest";
import { useChatStore } from "@/store/chat-store";

const h = vi.hoisted(() => ({
  calls: [] as Array<{ cmd: string; args: Record<string, unknown> }>,
}));

vi.stubGlobal("window", {
  setInterval: (_fn: () => void, _ms: number) => 1,
  clearInterval: () => undefined,
});

// startFresh 会 wireGoalEvents：不 mock 的话真 listen 会在测试环境里摸不到
// __TAURI_INTERNALS__ 直接炸
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => () => undefined),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args: Record<string, unknown> = {}) => {
    h.calls.push({ cmd, args });
    switch (cmd) {
      case "config_get":
        // bootstrap 灌进 store 的最小完整形状（syncModelForKind 还要读它）
        return Promise.resolve({
          ui: {},
          model: "",
          kindModels: {},
          models: [],
          profiles: [],
          activeProfileId: "",
          modelPool: { members: [], pinned: null },
        });
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

let caseNo = 0;

beforeEach(() => {
  caseNo += 1;
  h.calls.length = 0;
  useChatStore.setState({
    activeId: `conv-panel-${caseNo}`,
    kind: "chat",
    messages: [],
    offPath: [],
    attachments: [],
    pending: false,
    mediaBusy: false,
    conversations: [],
    toasts: [],
    section: "chats",
    // 摆成"用户本来展开着右栏"：默认值是收起，那样断言没有区分度
    panelCollapsed: false,
  });
});

describe("生成档进场的右栏自动收缩", () => {
  it("视频模式：展开着的右栏被顺手收起", () => {
    store().startConversation("video");
    expect(store().panelCollapsed).toBe(true);
  });

  it("音乐模式：同款收起", () => {
    store().startConversation("music");
    expect(store().panelCollapsed).toBe(true);
  });

  it("对话模式不抢用户的展开权：保持展开", () => {
    store().startConversation("chat");
    expect(store().panelCollapsed).toBe(false);
  });

  it("生图档不收（消息流形态右栏不碍事）；无参（Ctrl+N）同款不动", () => {
    store().startConversation("image");
    expect(store().panelCollapsed).toBe(false);
    store().startConversation();
    expect(store().panelCollapsed).toBe(false);
  });

  it("收起走 setUi：config.ui 镜像同步，落盘交给 updateConfig 统一走", () => {
    store().startConversation("video");
    expect(store().config.ui.panelCollapsed).toBe(true);
  });
});
