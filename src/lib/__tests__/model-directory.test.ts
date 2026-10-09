/** 全球模型规格目录：查找语义与加载链。invoke 打桩，localStorage 在 node 环境静默不可用 */
import { beforeEach, describe, expect, it, vi } from "vitest";

const invoke = vi.hoisted(() => vi.fn());
vi.mock("@tauri-apps/api/core", () => ({ invoke }));

import {
  directoryIfLoaded,
  loadModelDirectory,
  lookupModelDirectorySpec,
  primeModelDirectoryForTests,
  type ModelDirectory,
} from "../model-directory";

const DIRECTORY: ModelDirectory = {
  source: "https://models.dev/api.json",
  total: 3,
  models: {
    "gpt-4o": {
      capabilities: ["chat", "vision", "function_call"],
      inputModalities: ["text", "image"],
      outputModalities: ["text"],
      contextTokens: 128_000,
      maxTokens: 16_384,
    },
    "anthropic/claude-haiku-4-5": {
      capabilities: ["chat", "reasoning", "function_call"],
      inputModalities: ["text", "image"],
      outputModalities: ["text"],
      contextTokens: 200_000,
      maxTokens: 64_000,
    },
    "tencent/hy3": {
      capabilities: ["chat", "reasoning"],
      inputModalities: ["text"],
      outputModalities: ["text"],
      contextTokens: 256_000,
      maxTokens: 8_192,
    },
  },
};

beforeEach(() => {
  primeModelDirectoryForTests(null);
  invoke.mockReset();
});

describe("lookupModelDirectorySpec", () => {
  it("目录没加载时返回 null，不抛错", () => {
    expect(directoryIfLoaded()).toBeNull();
    expect(lookupModelDirectorySpec("gpt-4o")).toBeNull();
  });

  it("精确小写命中", () => {
    primeModelDirectoryForTests(DIRECTORY);
    expect(lookupModelDirectorySpec("GPT-4O")?.contextTokens).toBe(128_000);
  });

  it("用户 id 不带命名空间时按目录键的后缀匹配", () => {
    primeModelDirectoryForTests(DIRECTORY);
    expect(lookupModelDirectorySpec("claude-haiku-4-5")?.contextTokens).toBe(200_000);
    expect(lookupModelDirectorySpec("hy3")?.maxTokens).toBe(8_192);
  });

  it("用户 id 带命名空间而目录不带时剥掉再查", () => {
    primeModelDirectoryForTests(DIRECTORY);
    expect(lookupModelDirectorySpec("openai/gpt-4o")?.contextTokens).toBe(128_000);
  });

  it("查不到返回 null", () => {
    primeModelDirectoryForTests(DIRECTORY);
    expect(lookupModelDirectorySpec("mystery-model")).toBeNull();
    expect(lookupModelDirectorySpec("  ")).toBeNull();
  });

  it("模糊：修饰尾缀（preview/日期）不挡匹配，版本对上就行", () => {
    primeModelDirectoryForTests({
      source: "test",
      total: 4,
      models: {
        ...DIRECTORY.models,
        "minimax-m3": {
          capabilities: ["chat", "vision", "function_call"],
          inputModalities: ["text", "image", "audio", "video"],
          outputModalities: ["text", "audio", "image", "video"],
          contextTokens: 1_000_000,
          maxTokens: 0,
        },
        "gemini-2.5-pro": {
          capabilities: ["chat", "vision"],
          inputModalities: ["text", "image"],
          outputModalities: ["text"],
          contextTokens: 1_048_576,
          maxTokens: 65_536,
        },
        "claude-sonnet-4.5": {
          capabilities: ["chat", "vision"],
          inputModalities: ["text", "image"],
          outputModalities: ["text"],
          contextTokens: 200_000,
          maxTokens: 64_000,
        },
      },
    });
    // preview 尾缀
    expect(lookupModelDirectorySpec("minimax-m3-preview")?.contextTokens).toBe(1_000_000);
    // 完整日期尾缀
    expect(lookupModelDirectorySpec("gpt-4o-2024-11-20")?.contextTokens).toBe(128_000);
    expect(lookupModelDirectorySpec("gemini-2.5-pro-preview-06-05")?.contextTokens).toBe(1_048_576);
    // MMDD 式尾缀（deepseek-r1-0528 风格）
    expect(lookupModelDirectorySpec("gpt-4o-0528")?.maxTokens).toBe(16_384);
    // 4-5 与 4.5 是同一个版本
    expect(lookupModelDirectorySpec("claude-sonnet-4-5")?.contextTokens).toBe(200_000);
    // 大小写与命名空间混着来也不挡
    expect(lookupModelDirectorySpec("MiniMax/M3-Preview")?.contextTokens).toBe(1_000_000);
  });

  it("模糊：版本不同不硬凑，前缀相近也不串", () => {
    primeModelDirectoryForTests(DIRECTORY);
    expect(lookupModelDirectorySpec("gpt-4o-mini")).toBeNull();
    expect(lookupModelDirectorySpec("claude-haiku-4-6")).toBeNull();
    expect(lookupModelDirectorySpec("minimax-m2")).toBeNull();
  });
});

describe("loadModelDirectory", () => {
  it("成功加载后进 memo，二次调用不再打后端", async () => {
    invoke.mockResolvedValue(DIRECTORY);
    expect(await loadModelDirectory()).toEqual(DIRECTORY);
    expect(await loadModelDirectory()).toEqual(DIRECTORY);
    expect(invoke).toHaveBeenCalledTimes(1);
    expect(invoke).toHaveBeenCalledWith("model_directory");
  });

  it("加载失败返回 null 且不污染 memo", async () => {
    invoke.mockRejectedValue("网络炸了");
    expect(await loadModelDirectory()).toBeNull();
    expect(directoryIfLoaded()).toBeNull();
  });

  it("并发调用共享同一次请求", async () => {
    invoke.mockImplementation(
      () => new Promise((resolve) => setTimeout(() => resolve(DIRECTORY), 5)),
    );
    const [a, b] = await Promise.all([loadModelDirectory(), loadModelDirectory()]);
    expect(a).toEqual(DIRECTORY);
    expect(b).toEqual(DIRECTORY);
    expect(invoke).toHaveBeenCalledTimes(1);
  });
});
