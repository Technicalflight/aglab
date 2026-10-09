import type { AppConfig } from "@/types/chat";

/** 这一发的图片到底发不发得出去。判据住在 Rust 的 `AppConfig::takes_images()`，
 *  这里只做界面要的那一句人话：
 *  - `sent` 当前这一发确定用的模型开了「图像」多模态能力
 *  - `skipped` 没开、或表里根本没这一行 → 图只以路径进正文，模型看不见图里内容
 *  - `unknown` 模型池在自动调度，这一发用哪个模型还没定，客户端不能替它承诺 */
export function imageOutlook(config: AppConfig): "sent" | "skipped" | "unknown" {
  const pool = config.modelPool;
  // 手动指定是确定的：那一行就是答案。只有 auto/decision 才谈"还没定"
  const model = pool.mode === "pinned" && pool.pinned ? pool.pinned.model : config.model;
  if (pool.mode !== "off" && pool.mode !== "pinned") return "unknown";
  // 固定成员的规格读**它自己档案**的模型表：请求时 overlay 套的就是那份
  // （300K 窗口同款教训——顶层 config.models 是激活档案的，别的档案的行不在里面）
  const table =
    pool.mode === "pinned" && pool.pinned?.profileId
      ? (config.profiles?.find((profile) => profile.id === pool.pinned?.profileId)?.models ??
        config.models)
      : config.models;
  const spec = table.find((row) => row.model === model);
  return spec?.supportsImages ? "sent" : "skipped";
}
