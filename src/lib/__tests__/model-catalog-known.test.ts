/** 内置模型目录：规格只做预填，表里没有的模型不编数 */
import { describe, expect, it } from "vitest";

import { knownModelInfo, SERVICE_PRESETS, servicePresetFor, specPair } from "../model-catalog-known";

describe("knownModelInfo", () => {
  it("精确 ID 给展示名与规格", () => {
    expect(knownModelInfo("gpt-5.3-codex")).toEqual({
      label: "GPT-5.3 Codex",
      contextTokens: 400_000,
      maxTokens: 128_000,
    });
    expect(knownModelInfo("gpt-5.6")).toEqual({
      label: "GPT-5.6",
      contextTokens: 1_050_000,
      maxTokens: 128_000,
    });
  });

  it("变体 ID 走前缀规则兜底", () => {
    const info = knownModelInfo("gpt-5.9-preview-20260301");
    expect(info?.contextTokens).toBe(1_050_000);
    expect(info?.maxTokens).toBe(128_000);
    expect(knownModelInfo("claude-sonnet-4-6")?.contextTokens).toBe(200_000);
    expect(knownModelInfo("claude-sonnet-4-6")?.maxTokens).toBe(64_000);
  });

  it("大小写不敏感：手填大写也能对上目录", () => {
    expect(knownModelInfo("GPT-5.6-Luna")?.label).toBe("GPT-5.6 Luna");
  });

  it("表里没录的模型返回 null，不编规格", () => {
    expect(knownModelInfo("grok-4.7")).toBeNull();
    expect(knownModelInfo("my-private-model")).toBeNull();
    expect(knownModelInfo("  ")).toBeNull();
  });

  it("精确表压过前缀表：grok-4 本尊用精确值，grok-4-fast 走前缀", () => {
    expect(knownModelInfo("grok-4")?.contextTokens).toBe(256_000);
    expect(knownModelInfo("grok-4-fast")?.contextTokens).toBe(2_000_000);
  });
});

describe("servicePresetFor", () => {
  it("按 Base URL 反推预设，忽略尾部斜杠", () => {
    expect(servicePresetFor("https://api.openai.com/v1/")?.id).toBe("openai");
    expect(servicePresetFor("  https://api.anthropic.com  ")?.id).toBe("anthropic");
  });

  it("不是任何一家官方地址就是自定义服务商", () => {
    expect(servicePresetFor("https://x-api.cfd/v1")).toBeNull();
    expect(servicePresetFor("")).toBeNull();
  });

  it("预设之间的 baseUrl 互不重复（反推依赖这一条）", () => {
    const urls = SERVICE_PRESETS.map((preset) => preset.baseUrl);
    expect(new Set(urls).size).toBe(urls.length);
  });
});

describe("specPair", () => {
  it("目录写法：1.05M 显示成 1.1M，整数千位不拖小数", () => {
    expect(specPair(1_050_000, 128_000)).toBe("1.1M · 128K");
    expect(specPair(400_000, 128_000)).toBe("400K · 128K");
    expect(specPair(1_000_000, 32_000)).toBe("1M · 32K");
  });

  it("没填的一侧画 —，两侧都没填是空串", () => {
    expect(specPair(400_000, 0)).toBe("400K · —");
    expect(specPair(0, 128_000)).toBe("— · 128K");
    expect(specPair(0, 0)).toBe("");
  });
});
