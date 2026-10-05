import { describe, expect, it } from "vitest";

import { formatUsd, spentLabel } from "@/lib/format";

describe("spentLabel：读不出来的账不是 0", () => {
  it("null 显示 —，不许塌成 $0", () => {
    expect(spentLabel(null)).toBe("—");
  });

  it("「没花钱」与「看不见花了多少」在屏幕上长得不一样", () => {
    expect(spentLabel(0)).toBe(formatUsd(0));
    expect(spentLabel(0)).not.toBe(spentLabel(null));
  });

  it("有值时按 1e-8 美元那把尺格式化", () => {
    expect(spentLabel(120_000_000)).toBe("$1.20");
    expect(spentLabel(5_800_000)).toBe("$0.058");
  });
});
