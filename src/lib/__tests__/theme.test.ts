import { describe, expect, it } from "vitest";

import { ACCENT_PRESETS, DEFAULT_ACCENT_COLOR, readableForeground } from "@/lib/theme";

describe("readableForeground", () => {
  it("深蓝（出厂默认）压白字", () => {
    expect(readableForeground("#2563eb")).toBe("#ffffff");
  });

  it("亮橙压黑字", () => {
    expect(readableForeground("#f59e0b")).toBe("#0a0a0c");
  });

  it("纯白压黑字、纯黑压白字（两端不翻车）", () => {
    expect(readableForeground("#ffffff")).toBe("#0a0a0c");
    expect(readableForeground("#000000")).toBe("#ffffff");
  });

  it("中灰压黑字（亮度分界之上）", () => {
    expect(readableForeground("#808080")).toBe("#0a0a0c");
  });

  it("支持 #RGB 简写与大小写", () => {
    expect(readableForeground("#00f")).toBe("#ffffff");
    expect(readableForeground("#2563EB")).toBe("#ffffff");
  });

  it("前后空白可容忍", () => {
    expect(readableForeground("  #2563eb  ")).toBe("#ffffff");
  });

  it("解析失败返回 null（调用方回退主题默认，不硬配）", () => {
    expect(readableForeground("blue")).toBeNull();
    expect(readableForeground("#12345")).toBeNull();
    expect(readableForeground("")).toBeNull();
  });
});

describe("出厂默认不漂移", () => {
  it("出厂蓝就是 #3451e0", () => {
    expect(DEFAULT_ACCENT_COLOR).toBe("#3451e0");
  });

  it("蓝预设与出厂默认同源（改预设值时两边一起动）", () => {
    const blue = ACCENT_PRESETS.find((preset) => preset.label === "蓝");
    expect(blue?.value).toBe(DEFAULT_ACCENT_COLOR);
  });

  it("「跟随主题」槽位仍是空串排第一", () => {
    const [followTheme] = ACCENT_PRESETS;
    expect(followTheme?.value).toBe("");
    expect(followTheme?.label).toBe("跟随主题");
  });
});
