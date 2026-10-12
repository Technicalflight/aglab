import { describe, expect, it } from "vitest";

import { DEFAULT_ACCENT_COLOR } from "@/lib/theme";
import { appearanceMigrationPatch, appearanceMigrationPending } from "@/lib/appearance-migration";

describe("appearanceMigrationPending", () => {
  it("没跑过（无标记）＝待迁移", () => {
    expect(appearanceMigrationPending(null)).toBe(true);
  });

  it("标记值对不上（旧标记/损坏值）＝待迁移", () => {
    expect(appearanceMigrationPending("some-old-value")).toBe(true);
  });

  it("标记值精确匹配＝已完成", () => {
    expect(appearanceMigrationPending("light-blue-2026-11")).toBe(false);
  });
});

describe("appearanceMigrationPatch", () => {
  it("旧出厂态（dark + mono）→ 搬到浅色蓝", () => {
    expect(appearanceMigrationPatch({ themeMode: "dark", accentColor: "" })).toEqual({
      themeMode: "light",
      accentColor: DEFAULT_ACCENT_COLOR,
    });
  });

  it("已在新出厂态 → 不动", () => {
    expect(
      appearanceMigrationPatch({ themeMode: "light", accentColor: DEFAULT_ACCENT_COLOR }),
    ).toBeNull();
  });

  it("任何别的已保存选择也强制搬——语义是「搬到新出厂态一次」，不是「只修旧默认」", () => {
    expect(appearanceMigrationPatch({ themeMode: "system", accentColor: "#10b981" })).toEqual({
      themeMode: "light",
      accentColor: DEFAULT_ACCENT_COLOR,
    });
  });

  it("appearance 缺失（后端没答话的占位）→ 不动", () => {
    expect(appearanceMigrationPatch(undefined)).toBeNull();
  });
});
