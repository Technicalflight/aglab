/**
 * 整页勾选的边界钉。分页那份在 `pagination.test.ts`。
 */
import { describe, expect, it } from "vitest";
import { allSelected, mergeSelection } from "../proxy-list";

const ids = (count: number) => Array.from({ length: count }, (_, index) => `px-${index}`);

describe("mergeSelection", () => {
  it("整页并进去重，取消整页只去掉那一页的", () => {
    const page = ids(3);
    expect(mergeSelection(["px-0", "keep"], page, true)).toEqual(["px-0", "keep", "px-1", "px-2"]);
    expect(mergeSelection(["px-0", "px-1", "keep"], page, false)).toEqual(["keep"]);
  });

  it("不动调用方那份数组", () => {
    const selected = ["a"];
    mergeSelection(selected, ["b"], true);
    expect(selected).toEqual(["a"]);
  });
});

describe("allSelected", () => {
  it("整页都在选择里才算全选，空页不算", () => {
    expect(allSelected(["a", "b"], ["a", "b"])).toBe(true);
    expect(allSelected(["a"], ["a", "b"])).toBe(false);
    expect(allSelected(["a", "b"], [])).toBe(false);
  });
});
