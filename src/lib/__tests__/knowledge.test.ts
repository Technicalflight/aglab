import { describe, expect, it } from "vitest";

import type { KbSummary } from "../knowledge";
import { filterKbs, formatChars, relativeTime } from "../knowledge";

function kb(partial: Partial<KbSummary>): KbSummary {
  return {
    id: "kb1",
    name: "钓鱼笔记",
    description: "钓点与鱼情",
    projectId: "",
    docCount: 0,
    chars: 0,
    createdAt: 0,
    updatedAt: 0,
    ...partial,
  };
}

describe("filterKbs", () => {
  const items = [
    kb({ id: "a", name: "钓鱼笔记", description: "水库", projectId: "proj1" }),
    kb({ id: "b", name: "项目约定", description: "", projectId: "proj1" }),
    kb({ id: "c", name: "随手记", description: "未绑定", projectId: "" }),
  ];

  it("filters by workspace including the unbound bucket", () => {
    expect(filterKbs(items, "", "all").map((item) => item.id)).toEqual(["a", "b", "c"]);
    expect(filterKbs(items, "", "proj1").map((item) => item.id)).toEqual(["a", "b"]);
    expect(filterKbs(items, "", "none").map((item) => item.id)).toEqual(["c"]);
    expect(filterKbs(items, "", "proj2")).toEqual([]);
  });

  it("matches the keyword against name and description, case-insensitively", () => {
    expect(filterKbs(items, "钓鱼", "all").map((item) => item.id)).toEqual(["a"]);
    expect(filterKbs(items, "水库", "all").map((item) => item.id)).toEqual(["a"]);
    // 项目约定没有描述：名字兜底
    expect(filterKbs(items, "约定", "proj1").map((item) => item.id)).toEqual(["b"]);
    expect(filterKbs(items, "ABC", "all")).toEqual([]);
  });

  it("applies workspace before the keyword", () => {
    expect(filterKbs(items, "钓鱼", "none")).toEqual([]);
  });
});

describe("formatChars", () => {
  it("keeps small counts literal and folds big ones into 万", () => {
    expect(formatChars(0)).toBe("0 字");
    expect(formatChars(9999)).toBe("9999 字");
    expect(formatChars(10_000)).toBe("1.0 万字");
    expect(formatChars(123_456)).toBe("12.3 万字");
  });
});

describe("relativeTime", () => {
  it("matches the sidebar wording buckets", () => {
    const now = Date.now();
    expect(relativeTime(0)).toBe("—");
    expect(relativeTime(now)).toBe("刚刚");
    expect(relativeTime(now - 5 * 60_000)).toBe("5 分钟前");
    expect(relativeTime(now - 3 * 60 * 60_000)).toBe("3 小时前");
    // 超过一天落到月日：只看形状
    expect(relativeTime(now - 2 * 24 * 60 * 60_000)).toMatch(/^\d{1,2}月\d{1,2}日$/);
  });
});
