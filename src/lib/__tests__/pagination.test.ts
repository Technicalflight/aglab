/**
 * 分页数学的边界钉。挑的都是"看起来对、错了静默"的形状：
 * 末页删空停在空白页、越界页号返回空数组、整除时多出一页。
 */
import { describe, expect, it } from "vitest";
import { clampPage, pageCount, pageSlice } from "../pagination";

const SIZE = 10;
const ids = (count: number) => Array.from({ length: count }, (_, index) => `i${index}`);

describe("pageCount", () => {
  it("空列表也算一页，整除不多出一页", () => {
    expect(pageCount(0, SIZE)).toBe(1);
    expect(pageCount(SIZE, SIZE)).toBe(1);
    expect(pageCount(SIZE + 1, SIZE)).toBe(2);
    expect(pageCount(25, SIZE)).toBe(3);
    expect(pageCount(192, 20)).toBe(10);
  });

  it("脏的尺寸不能把它算成 0 页或 NaN", () => {
    expect(pageCount(5, 0)).toBe(5);
    expect(pageCount(-3, SIZE)).toBe(1);
  });
});

describe("clampPage", () => {
  it("越界的页号钳回还在的那一页", () => {
    expect(clampPage(9, 12, SIZE)).toBe(1);
    expect(clampPage(-3, 12, SIZE)).toBe(0);
    expect(clampPage(0, 0, SIZE)).toBe(0);
  });

  it("末页被删空时，停着不动的页号自动落到新的末页", () => {
    // 站在第 3 页（索引 2，共 25 条），删到只剩 20 条 → 只剩 2 页
    expect(clampPage(2, 25, SIZE)).toBe(2);
    expect(clampPage(2, 20, SIZE)).toBe(1);
  });
});

describe("pageSlice", () => {
  it("按页切：第一页从头十条、第二页从第十一条起", () => {
    const items = ids(23);
    expect(pageSlice(items, 0, SIZE)).toEqual(items.slice(0, 10));
    expect(pageSlice(items, 1, SIZE)).toEqual(items.slice(10, 20));
    expect(pageSlice(items, 2, SIZE)).toEqual(items.slice(20, 23));
  });

  it("页号越界不返回空数组：那正是末页删空后用户看到的空白页", () => {
    const items = ids(12);
    expect(pageSlice(items, 7, SIZE)).toEqual(items.slice(10, 12));
    expect(pageSlice([], 3, SIZE)).toEqual([]);
  });
});
