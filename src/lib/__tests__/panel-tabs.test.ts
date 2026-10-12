/**
 * 右栏工具标签状态机的钉（src/lib/panel-tabs.ts）。
 *
 * 两条入口语义：手动 openTab=每次开新、满员拒绝；外部导航 focusTool=聚焦同类
 * 已有、没有才开。多开落地后最容易破的是 focusTool 的「已停在同工具上保持
 * 现状」——手动新开一枚同类后 setPanelTab 会触发外部导航路径，没有这道短路
 * 新实例会被瞬间顶回第一枚（焦点跳走，用户以为没开出来）。
 */
import { describe, expect, it } from "vitest";
import { closeTab, emptyTabs, focusTool, MAX_OPEN_TABS, openTab } from "@/lib/panel-tabs";

const state = (
  open: Array<{ id: string; tool: "terminal" | "browser" | "preview" }>,
  activeId: string | null,
) => ({
  open,
  activeId,
  picking: false,
});

describe("openTab：手动开新，每次一枚新实例", () => {
  it("同类也可再开：open 追加、焦点指向新实例", () => {
    const prev = state([{ id: "a", tool: "terminal" }], "a");
    const next = openTab(prev, "terminal", "b");
    expect(next.ok).toBe(true);
    expect(next.state.open).toEqual([
      { id: "a", tool: "terminal" },
      { id: "b", tool: "terminal" },
    ]);
    expect(next.state.activeId).toBe("b");
  });

  it("满员拒绝：状态原样返回，交给调用方提示", () => {
    const full = state(
      Array.from({ length: MAX_OPEN_TABS }, (_, index) => ({
        id: `t${index}`,
        tool: "terminal" as const,
      })),
      "t0",
    );
    const next = openTab(full, "browser", "extra");
    expect(next.ok).toBe(false);
    expect(next.state).toBe(full);
  });

  it("未满时不受上限影响", () => {
    const near = state(
      Array.from({ length: MAX_OPEN_TABS - 1 }, (_, i) => ({
        id: `t${i}`,
        tool: "preview" as const,
      })),
      "t0",
    );
    expect(openTab(near, "terminal", "extra").ok).toBe(true);
  });
});

describe("closeTab：焦点回退", () => {
  it("关非激活标签：焦点不动", () => {
    const prev = state(
      [
        { id: "a", tool: "terminal" },
        { id: "b", tool: "browser" },
      ],
      "b",
    );
    const next = closeTab(prev, "a");
    expect(next.open.map((item) => item.id)).toEqual(["b"]);
    expect(next.activeId).toBe("b");
  });

  it("关激活标签（居中）：焦点落到删除位置上的新占据者（右邻）", () => {
    const prev = state(
      [
        { id: "a", tool: "terminal" },
        { id: "b", tool: "browser" },
        { id: "c", tool: "preview" },
      ],
      "b",
    );
    const next = closeTab(prev, "b");
    expect(next.open.map((item) => item.id)).toEqual(["a", "c"]);
    // min(at, 末位) 语义：b 在位置 1，删掉后位置 1 是 c——焦点跟位置不跟左邻
    expect(next.activeId).toBe("c");
  });

  it("关激活标签（末位）：焦点落到前一枚", () => {
    const prev = state(
      [
        { id: "a", tool: "terminal" },
        { id: "b", tool: "browser" },
      ],
      "b",
    );
    expect(closeTab(prev, "b").activeId).toBe("a");
  });

  it("关到空：回开始页", () => {
    const prev = state([{ id: "a", tool: "terminal" }], "a");
    const next = closeTab(prev, "a");
    expect(next.open).toEqual([]);
    expect(next.activeId).toBeNull();
  });
});

describe("focusTool：外部导航", () => {
  it("无同类：开新一枚（不受上限约束的路径由组件保证，这里只管状态）", () => {
    const prev = state([{ id: "a", tool: "terminal" }], "a");
    const next = focusTool(prev, "browser", "b");
    expect(next.open).toEqual([
      { id: "a", tool: "terminal" },
      { id: "b", tool: "browser" },
    ]);
    expect(next.activeId).toBe("b");
  });

  it("有同类：聚焦最早那枚，不开新", () => {
    const prev = state(
      [
        { id: "t1", tool: "terminal" },
        { id: "b", tool: "browser" },
        { id: "t2", tool: "terminal" },
      ],
      "b",
    );
    const next = focusTool(prev, "terminal", "t3");
    expect(next.open).toHaveLength(3);
    expect(next.activeId).toBe("t1");
  });

  it("已停在同工具的某枚上：保持现状——手动新开的同类不被顶回第一枚", () => {
    const prev = state(
      [
        { id: "t1", tool: "terminal" },
        { id: "t2", tool: "terminal" },
      ],
      "t2",
    );
    const next = focusTool(prev, "terminal", "t3");
    expect(next.open).toHaveLength(2);
    expect(next.activeId).toBe("t2");
  });

  it("从开始页进来（无激活）：照常开新", () => {
    const next = focusTool(emptyTabs(), "preview", "p1");
    expect(next.open).toEqual([{ id: "p1", tool: "preview" }]);
    expect(next.activeId).toBe("p1");
  });
});
