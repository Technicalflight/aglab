import type { MissedPolicy, TaskKind } from "@/types/chat";

/**
 * 错过策略写在 `kind` 的后缀上（`"interval|skip"`），这样 config 不必新增键。
 * 代价是 `kind` 不再等于界面上那两个选项——所以拆与拼只允许在这一个地方做，
 * 免得第二处忘了后缀、把用户选过的策略当成 interval 覆盖掉
 */
export const POLICIES: Array<{ value: MissedPolicy; label: string }> = [
  { value: "run_latest", label: "错过只补最近一次" },
  { value: "catch_up_once", label: "错过几次补几次" },
  { value: "skip", label: "错过的一律作废" },
];

const DEFAULT_POLICY: MissedPolicy = "run_latest";

export function splitKind(kind: string): { base: TaskKind | null; missed: MissedPolicy } {
  const index = kind.indexOf("|");
  const head = index < 0 ? kind : kind.slice(0, index);
  const suffix = index < 0 ? "" : kind.slice(index + 1);
  const known = POLICIES.find((policy) => policy.value === suffix);
  return {
    // 与后端 `trigger::of` 同一口径：head 只认这四个词，认不出就是**没有触发器**，
    // 不是"那就当它是 interval"。当成 interval 的代价写在界面上：那一行会说"每 60 分钟"，
    // 而后端那边连到期都不算——`disabled_and_unknown_kinds_have_no_next_run` 钉的就是那一侧
    base: head === "interval" || head === "daily" || head === "weekly" || head === "cron"
      ? (head as TaskKind)
      : null,
    // 认不出的策略后缀退回默认档，与 `mode_from_legacy` 同一条规矩：打错一个字母
    // 不该让任务变成另一种东西
    missed: known ? known.value : DEFAULT_POLICY,
  };
}

/** 这一格是不是三种策略之一。`TaskView.missedPolicy` 只在**频率本身不是触发器**时给空串
 *  （停用不在此列：`trigger::of` 不看 `enabled`），把它直接交给 `policyLabel` 会读出
 *  "错过只补最近一次"——那是一句后端没有答应过的话 */
export function isPolicy(value: string): value is MissedPolicy {
  return POLICIES.some((policy) => policy.value === value);
}

export function joinKind(base: TaskKind, missed: MissedPolicy) {
  return missed === DEFAULT_POLICY ? base : `${base}|${missed}`;
}

export function policyLabel(missed: MissedPolicy) {
  return POLICIES.find((policy) => policy.value === missed)?.label ?? "错过只补最近一次";
}

/** "多久以前"要说人话而不是给时间戳。任务页那一格（在等人点头）与设置页那两本账
 *  （点过头的、运行记录）报的是同一种读数，所以只写在这一个地方 */
export function agoText(ms: number) {
  const left = Date.now() - ms;
  if (left < 60_000) return "刚刚";
  const minutes = Math.round(left / 60_000);
  if (minutes < 60) return `${minutes} 分钟前`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours} 小时前`;
  return `${Math.floor(hours / 24)} 天前`;
}
