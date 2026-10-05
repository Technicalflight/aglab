import { describe, expect, it } from "vitest";

import {
  type AppConfig,
  DEFAULT_CONTEXT_TOKENS,
  effectiveContextWindow,
} from "@/types/chat";

/** 只造解析链读的那几格，其余字段与解析无关（宽松入参：完整 AppConfig 的必填字段与本测试无关） */
function config(partial: Record<string, unknown>): AppConfig {
  return {
    contextTokens: 0,
    models: [],
    profiles: [],
    modelPool: {
      mode: "off",
      strategy: "failover",
      members: [],
      pinned: null,
    },
    ...partial,
  } as unknown as AppConfig;
}

describe("effectiveContextWindow", () => {
  it("手动指定池成员时，读成员档案里那一行模型规格的窗口", () => {
    const value = effectiveContextWindow(
      config({
        contextTokens: 0,
        profiles: [
          {
            id: "prof-a",
            name: "a",
            contextTokens: 0,
            models: [{ model: "glm-5.3-flash", contextTokens: 300_000 }],
          },
        ],
        modelPool: {
          mode: "pinned",
          strategy: "failover",
          members: [],
          pinned: { profileId: "prof-a", model: "glm-5.3-flash" },
        },
      }),
    );
    expect(value).toBe(300_000);
  });

  it("模型行没配窗口就退档案级，再退默认", () => {
    const base = {
      profiles: [
        {
          id: "prof-a",
          name: "a",
          contextTokens: 200_000,
          models: [{ model: "glm-5.3-flash", contextTokens: 0 }],
        },
      ],
      modelPool: {
        mode: "pinned" as const,
        strategy: "failover",
        members: [],
        pinned: { profileId: "prof-a", model: "glm-5.3-flash" },
      },
    };
    expect(effectiveContextWindow(config(base))).toBe(200_000);

    const noProfileLevel = {
      ...base,
      profiles: [{ ...base.profiles[0], contextTokens: 0 }],
    };
    expect(effectiveContextWindow(config(noProfileLevel))).toBe(
      DEFAULT_CONTEXT_TOKENS,
    );
  });

  it("固定在当前连接上时，模型行从激活连接的表里找", () => {
    const value = effectiveContextWindow(
      config({
        contextTokens: 0,
        models: [{ model: "glm-5.3-flash", contextTokens: 64_000 }],
        modelPool: {
          mode: "pinned",
          strategy: "failover",
          members: [],
          pinned: { profileId: "", model: "glm-5.3-flash" },
        },
      }),
    );
    expect(value).toBe(64_000);
  });

  it("池未开/自动时按顶层读数，没配则用默认窗口", () => {
    expect(effectiveContextWindow(config({ contextTokens: 256_000 }))).toBe(
      256_000,
    );
    expect(
      effectiveContextWindow(config({ contextTokens: 0 })),
      "顶层没配 → 与后端 DEFAULT_CONTEXT_TOKENS 同一个数",
    ).toBe(DEFAULT_CONTEXT_TOKENS);
  });
});
