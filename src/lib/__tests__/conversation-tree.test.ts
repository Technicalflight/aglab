/**
 * 话题树投影的边界钉。挑的都是"截图上看不出来"的形状：断链、成环、
 * 旧归档没有父子关系、根节点互为兄弟。
 */
import { describe, expect, it } from "vitest";
import {
  branchPositions,
  branchTail,
  forkPoints,
  nextSiblingId,
  partition,
  pathOf,
  siblingsOf,
} from "../conversation-tree";

type Node = { id: string; parentId?: string | null };

const linear: Node[] = [{ id: "a" }, { id: "b", parentId: "a" }, { id: "c", parentId: "b" }];

describe("pathOf", () => {
  it("沿父链走到根再反过来", () => {
    expect(pathOf(linear, "c").map((node) => node.id)).toEqual(["a", "b", "c"]);
    expect(pathOf(linear, "b").map((node) => node.id)).toEqual(["a", "b"]);
  });

  it("两条分支各走各的，不互相掺进来", () => {
    const nodes: Node[] = [...linear, { id: "d", parentId: "b" }];
    expect(pathOf(nodes, "c").map((node) => node.id)).toEqual(["a", "b", "c"]);
    expect(pathOf(nodes, "d").map((node) => node.id)).toEqual(["a", "b", "d"]);
  });

  it("tip 为空或旧归档没有 parentId：退回插入序，不是一片空白", () => {
    expect(pathOf(linear, null)).toEqual(linear);
    const legacy: Node[] = [{ id: "a" }, { id: "b" }, { id: "c" }];
    expect(pathOf(legacy, "c").map((node) => node.id)).toEqual(["a", "b", "c"]);
  });

  it("tip 指着不存在的节点、或父链成了环：同样退回插入序", () => {
    expect(pathOf(linear, "ghost").map((node) => node.id)).toEqual(["a", "b", "c"]);
    const loop: Node[] = [
      { id: "x", parentId: "y" },
      { id: "y", parentId: "x" },
    ];
    expect(pathOf(loop, "x").map((node) => node.id)).toEqual(["x", "y"]);
  });
});

describe("siblingsOf", () => {
  it("同一父下的都算，含自己，按插入序", () => {
    const nodes: Node[] = [{ id: "a" }, { id: "b", parentId: "a" }, { id: "c", parentId: "a" }];
    expect(siblingsOf(nodes, "b").map((node) => node.id)).toEqual(["b", "c"]);
    expect(siblingsOf(nodes, "c").map((node) => node.id)).toEqual(["b", "c"]);
  });

  it("显式 null 才是根：两条根互为兄弟", () => {
    const nodes: Node[] = [
      { id: "r1", parentId: null },
      { id: "r2", parentId: null },
      { id: "k", parentId: "r1" },
    ];
    expect(siblingsOf(nodes, "r1").map((node) => node.id)).toEqual(["r1", "r2"]);
  });

  /** 缺省（undefined）与 null 是两件事：前者是"没说"，按插入序接在前一条之后。
   *  过渡气泡（压缩中的占位、记忆回执）与插话/跟随那几句都不记得盖章，
   *  靠这条规则它们天然在链上，而不是各自变成一条根把路径截断 */
  it("没写 parentId 的按插入序接在前一条之后，不会半路断链", () => {
    const nodes: Node[] = [
      { id: "q1", parentId: null },
      { id: "a1", parentId: "q1" },
      { id: "note" }, // 只是接在尾巴上的过渡气泡
    ];
    expect(siblingsOf(nodes, "note").map((node) => node.id)).toEqual(["note"]);
    expect(pathOf(nodes, "note").map((node) => node.id)).toEqual(["q1", "a1", "note"]);
    expect(branchTail(nodes, "q1")).toBe("note");
  });

  it("找不到的 id 给空数组", () => {
    expect(siblingsOf(linear, "ghost")).toEqual([]);
  });
});

describe("nextSiblingId", () => {
  it("两端不绕圈：没有上一条就说没有", () => {
    const nodes: Node[] = [{ id: "a" }, { id: "b", parentId: "a" }, { id: "c", parentId: "a" }];
    expect(nextSiblingId(nodes, "b", 1)).toBe("c");
    expect(nextSiblingId(nodes, "c", -1)).toBe("b");
    expect(nextSiblingId(nodes, "b", -1)).toBeNull();
    expect(nextSiblingId(nodes, "c", 1)).toBeNull();
    expect(nextSiblingId(nodes, "a", 1)).toBeNull();
  });
});

describe("branchPositions / forkPoints", () => {
  const nodes: Node[] = [
    { id: "a" },
    { id: "b1", parentId: "a" },
    { id: "b2", parentId: "a" },
    { id: "c", parentId: "b2" },
  ];

  it("走 b2 那条时：a 只有一支（它自己是根），b2 是第二支", () => {
    const positions = branchPositions(nodes, "c");
    expect(positions.get("b2")).toEqual({ index: 2, total: 2 });
    expect(positions.has("a")).toBe(false);
    expect(positions.has("c")).toBe(false);
    expect(forkPoints(nodes, "c")).toEqual(new Set(["b2"]));
  });

  it("走 b1 那条时：b1 是第一支，b2 那一支整个下来", () => {
    expect(branchPositions(nodes, "b1").get("b1")).toEqual({ index: 1, total: 2 });
    expect(forkPoints(nodes, "b1")).toEqual(new Set(["b1"]));
  });
});

describe("branchTail", () => {
  const nodes: Node[] = [
    { id: "a" },
    { id: "b1", parentId: "a" },
    { id: "b2", parentId: "a" },
    { id: "c", parentId: "b2" },
  ];

  it("落到那一支最深的一处，不是停在被点的那一条", () => {
    expect(branchTail(nodes, "b2")).toBe("c");
    expect(branchTail(nodes, "b1")).toBe("b1");
    // 根也要落到最后那条：a 之下最深的孩子是 b2，再往下是 c
    expect(branchTail(nodes, "a")).toBe("c");
  });

  it("成环的父链不会把这里转出去", () => {
    const loop: Node[] = [
      { id: "x", parentId: "y" },
      { id: "y", parentId: "x" },
    ];
    expect(branchTail(loop, "x")).toBe("y");
  });

  it("不存在的 id 给 null", () => {
    expect(branchTail(nodes, "ghost")).toBeNull();
  });
});

describe("partition", () => {
  const nodes: Node[] = [
    { id: "a" },
    { id: "b1", parentId: "a" },
    { id: "b2", parentId: "a" },
    { id: "c", parentId: "b2" },
  ];

  it("看得见的是那一条路径，其余整段留在 offPath 里", () => {
    const cut = partition(nodes, "c");
    expect(cut.thread.map((node) => node.id)).toEqual(["a", "b2", "c"]);
    expect(cut.offPath.map((node) => node.id)).toEqual(["b1"]);
  });

  it("切到另一支时，前一条路径整段下来——一个节点都不丢", () => {
    const cut = partition(nodes, "b1");
    expect(cut.thread.map((node) => node.id)).toEqual(["a", "b1"]);
    expect(cut.offPath.map((node) => node.id)).toEqual(["b2", "c"]);
    expect(cut.thread.length + cut.offPath.length).toBe(nodes.length);
  });
});
