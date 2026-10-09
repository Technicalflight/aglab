/**
 * "这一发的图发不发得出去"的判据。它决定输入框上面那句提示，
 * 说错了用户会以为图已经给模型看过了。
 */
import { describe, expect, it } from "vitest";
import type { AppConfig } from "../../types/chat";
import { imageOutlook } from "../vision";

/** 只造这个函数真读的三格。整份 AppConfig 有几十格，铺满它只会让用例说不清"哪一格改了结论" */
const config = (over: Partial<AppConfig>): AppConfig =>
  ({
    model: "grok-4.7",
    models: [],
    modelPool: { mode: "off", members: [], strategy: "round_robin", pinned: null },
    ...over,
  }) as AppConfig;

const pool = (over: Record<string, unknown>) => ({
  mode: "off",
  members: [],
  strategy: "round_robin",
  pinned: null,
  ...over,
});

describe("imageOutlook", () => {
  it("表里那一行勾了收图片才算发得出去", () => {
    expect(
      imageOutlook(config({ models: [{ model: "grok-4.7", supportsImages: true } as never] })),
    ).toBe("sent");
    expect(
      imageOutlook(config({ models: [{ model: "grok-4.7", supportsImages: false } as never] })),
    ).toBe("skipped");
  });

  it("表里没这一行是没填过证据，不是支持", () => {
    expect(imageOutlook(config({ models: [] }))).toBe("skipped");
    expect(
      imageOutlook(config({ models: [{ model: "别的", supportsImages: true } as never] })),
    ).toBe("skipped");
  });

  it("池子自动调度时不替它承诺：这一发用哪个模型还没定", () => {
    expect(
      imageOutlook(
        config({
          modelPool: pool({
            mode: "auto",
            members: [{ profileId: "", model: "grok-4.7", enabled: true }],
          }) as never,
        }),
      ),
    ).toBe("unknown");
  });

  it("手动指定是确定的，按被指定的那个模型判", () => {
    const models = [
      { model: "grok-4.7", supportsImages: false },
      { model: "vision-model", supportsImages: true },
    ] as never;
    expect(
      imageOutlook(
        config({
          models,
          modelPool: pool({
            mode: "pinned",
            pinned: { profileId: "p", model: "vision-model" },
          }) as never,
        }),
      ),
    ).toBe("sent");
    expect(
      imageOutlook(
        config({
          models,
          modelPool: pool({
            mode: "pinned",
            pinned: { profileId: "p", model: "grok-4.7" },
          }) as never,
        }),
      ),
    ).toBe("skipped");
  });

  it("固定成员的规格读它自己档案的模型表：激活档案的表里没有不算没有", () => {
    // 请求时 overlay 把成员档案的连接域与模型表整体抄过去，顶层 config.models
    // 是激活档案的那份——别的档案勾了「图像」，顶层看不见不等于发不出去
    expect(
      imageOutlook(
        config({
          models: [{ model: "vision-model", supportsImages: false } as never],
          profiles: [
            {
              id: "prof-a",
              models: [{ model: "vision-model", supportsImages: true }],
            },
          ] as never,
          modelPool: pool({
            mode: "pinned",
            pinned: { profileId: "prof-a", model: "vision-model" },
          }) as never,
        }),
      ),
    ).toBe("sent");
  });
});
