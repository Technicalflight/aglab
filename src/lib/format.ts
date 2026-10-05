/**
 * 全项目唯一的数字格式化入口。
 *
 * 规则写在设计文档 §7：组件里禁止手写 toFixed / toLocaleString——
 * 口径改一处就要全局生效，散在组件里每处各说一套就再也对不齐了。
 */

/** 去掉小数结尾的零："0.5000" → "0.5"，"0.0580" → "0.058" */
function trimZeros(text: string): string {
  return text.replace(/(\.\d*?)0+$/, "$1").replace(/\.$/, "");
}

/**
 * 金额（美元）：
 * 0 → "$0"；≥1 → 两位小数 + 千分位（$1,234.56）；
 * <1 → 最多 4 位小数去尾零（$0.058）；<0.0001 → "<$0.0001"。
 * 最便宜的模型一次也就几厘钱，四位小数以下的差异不值得占一栏。
 */
export function formatUsd(value: number): string {
  if (!Number.isFinite(value) || value === 0) return "$0";
  const abs = Math.abs(value);
  if (abs < 0.0001) return "<$0.0001";
  if (abs >= 1) {
    return `$${value.toLocaleString("en-US", {
      minimumFractionDigits: 2,
      maximumFractionDigits: 2,
    })}`;
  }
  return `$${trimZeros(value.toFixed(4))}`;
}

/**
 * 目标那一格的花费读数。`null` = **台账读不出来**，显示 `—` 而不是 `$0.00`：
 * "这一支没花钱"与"看不见它花了多少"在屏幕上必须长得不一样——判据那一头拿不到账
 * 会直接停下（花费上限是唯一的自动刹车），界面跟着说没花就是同一件事的两种打架说法。
 * 归属读数没值就留 `—`，这条规矩在别处也成立
 */
export function spentLabel(spentUsdE8: number | null): string {
  return spentUsdE8 === null ? "—" : formatUsd(spentUsdE8 / 1e8);
}

/** token 数，表格写法：永远全量千分位（1,231,200）。 */
export function formatTokens(value: number): string {
  if (!Number.isFinite(value)) return "0";
  return Math.round(value).toLocaleString("en-US");
}

/**
 * token 数，汇总卡写法：≥100 万缩成 "x.xxM"、≥10 万缩成 "x.xK"，
 * 完整数字由调用方放进 title 提示。卡片就一格宽，全量数字会挤爆。
 */
export function formatTokensCompact(value: number): string {
  const abs = Math.abs(value);
  if (abs >= 1_000_000) return `${(value / 1_000_000).toFixed(2)}M`;
  if (abs >= 100_000) return `${(value / 1_000).toFixed(1)}K`;
  return formatTokens(value);
}

/** 请求数这类计数：千分位，四位以上的数字才看得清量级。 */
export function formatCount(value: number): string {
  if (!Number.isFinite(value)) return "0";
  return Math.round(value).toLocaleString("en-US");
}
