import { fetchConfig, persistPatch } from "@/lib/chat-transport";
import { DEFAULT_ACCENT_COLOR } from "@/lib/theme";
import type { AppConfig } from "@/types/chat";

/**
 * 一次性外观迁移：出厂默认从「暗色 + mono」翻到「浅色 + 蓝」（2026-10）。
 * 已保存过配置的老机器会停在旧外观上，靠这里一次性搬到新出厂态；
 * 搬完写标记，此后用户手动切深色/换强调色永不回改。
 *
 * 标记放 localStorage 而不是 config.json：配置结构由 Rust 侧定死，
 * 为一个纯前端的一次性动作加字段不值当（decision/config.ts 同款取舍）。
 *
 * 2026-11：出厂蓝从 #2563eb 换成 #3451e0（对齐新设计基准），
 * 标记升一代让迁移重跑一次，把停在旧出厂蓝上的机器一并搬到新蓝。
 */

const MIGRATION_FLAG = "aglab-appearance-migrated";
const MIGRATION_FLAG_VALUE = "light-blue-2026-11";

type Appearance = Pick<AppConfig, "themeMode" | "accentColor">;

/** 没跑过这次迁移（或标记没写上）就算待迁移 */
export function appearanceMigrationPending(flag: string | null): boolean {
  return flag !== MIGRATION_FLAG_VALUE;
}

/** 要搬成什么样。已在新出厂态 → 不用动（null）；其余任何已保存值 → 强制搬
 *  （迁移语义是"搬一次到新出厂态"，不是"只搬旧默认"）。appearance 缺失
 *  （后端还没答话的占位）也不动——那一刻没有"用户的选择"可保 */
export function appearanceMigrationPatch(appearance: Appearance | undefined): Appearance | null {
  if (!appearance) return null;
  if (appearance.themeMode === "light" && appearance.accentColor === DEFAULT_ACCENT_COLOR) {
    return null;
  }
  return { themeMode: "light", accentColor: DEFAULT_ACCENT_COLOR };
}

/** 启动早期跑一次（App 挂载时、bootstrap 拉配置之前——顺序反了迁移结果会被
 *  盘上旧值盖回 store）。失败静默：外观是化妆不是救命，没写标记下轮启动重试 */
export async function migrateAppearance(): Promise<void> {
  try {
    if (!appearanceMigrationPending(localStorage.getItem(MIGRATION_FLAG))) return;
    const patch = appearanceMigrationPatch(await fetchConfig());
    if (patch) await persistPatch(patch);
    localStorage.setItem(MIGRATION_FLAG, MIGRATION_FLAG_VALUE);
  } catch {
    // 静默重试语义，见上
  }
}
