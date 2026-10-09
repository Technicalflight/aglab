import { describe, expect, it } from "vitest";

import { highlightCode, resolveLanguage } from "../highlight";

/**
 * 高亮是异步且懒加载的，类型检查看不出"loadLanguage 到底能不能把语言
 * 注册进去"——写错了也是类型合法、运行时才炸（首次渲染代码块退化成无色文本）。
 * 这组用例就是钉住那条懒加载链路真的能跑通。
 */
describe("highlightCode 懒加载语言包", () => {
  it("已知语言能真正高亮出颜色（证明 loadLanguage 生效）", async () => {
    const tokens = await highlightCode("const a = 1;", "typescript");
    const flat = tokens.flat();
    expect(flat.length).toBeGreaterThan(0);
    // 关键字与变量应当被染上颜色；全等一段纯文本说明语言包没注册上
    expect(flat.some((token) => token.color !== undefined)).toBe(true);
  });

  it("同一语言重复调用不报错（命中已加载缓存）", async () => {
    const first = await highlightCode("let x = 1", "javascript");
    const second = await highlightCode("let x = 1", "javascript");
    expect(
      second
        .flat()
        .map((t) => t.content)
        .join(""),
    ).toBe(
      first
        .flat()
        .map((t) => t.content)
        .join(""),
    );
  });

  it("不同语言可各自加载（增量注册而非只认第一个）", async () => {
    const [rust, python] = await Promise.all([
      highlightCode("fn main() {}", "rust"),
      highlightCode("x = 1", "python"),
    ]);
    expect(rust.flat().length).toBeGreaterThan(0);
    expect(python.flat().length).toBeGreaterThan(0);
  });

  it("text 不加载任何语言包也能出结果", async () => {
    const tokens = await highlightCode("纯文本一行", "text");
    expect(
      tokens
        .flat()
        .map((t) => t.content)
        .join(""),
    ).toBe("纯文本一行");
  });
});

describe("resolveLanguage 别名归一", () => {
  it("常见别名映射到已注册语言", () => {
    expect(resolveLanguage("ts")).toBe("typescript");
    expect(resolveLanguage("rs")).toBe("rust");
    expect(resolveLanguage("py")).toBe("python");
    expect(resolveLanguage("sh")).toBe("bash");
  });

  it("大小写与空白不影响判定", () => {
    expect(resolveLanguage("  TS  ")).toBe("typescript");
  });

  it("未知语言退化为 text，不抛错", () => {
    expect(resolveLanguage("brainfuck")).toBe("text");
    expect(resolveLanguage(undefined)).toBe("text");
  });
});
