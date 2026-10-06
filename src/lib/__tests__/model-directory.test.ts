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
    invoke.mockImplementation(() => new Promise((resolve) => setTimeout(() => resolve(DIRECTORY), 5)));
    const [a, b] = await Promise.all([loadModelDirectory(), loadModelDirectory()]);
    expect(a).toEqual(DIRECTORY);
    expect(b).toEqual(DIRECTORY);
    expect(invoke).toHaveBeenCalledTimes(1);
  });
});
