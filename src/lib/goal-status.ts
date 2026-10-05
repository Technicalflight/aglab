import type { CriterionView, GoalStatus } from "@/types/chat";

/**
 * `design-goal-mode.md` §5.2 那张表的唯一出处：六格 → 中文与颜色 token。目标带与角落卡都读这一份——
 * 两处各写一份迟早漂成两句不同的话，而漂移的那一句是用户先看见的。
 *
 * 三条约束是刻意的：
 * - **只有 `brand` / `muted-foreground` / `destructive` 三个既有 token**，不为新状态
 *   造第四种颜色（深色单主题与单点缀色那条禁则）。
 * - `usage_limited` 用 muted 不用 destructive：那不是这一支做错了什么，是账号或服务商
 *   的闸，颜色不该指责用户。
 * - `blocked` 叫「停住了」而不是"受阻"：它没死，是在等人动一下（Codex 用的就是 stalled）。
 *
 * `pending`（这一轮的服务商还在出字）与这六格**正交**，不许合成第七个名字：
 * 它由状态点要不要脉冲来表达。从前那个"待命"就是两格互相解释时生出来的第五个词
 */
export const GOAL_STATUS_WORD: Record<GoalStatus, { word: string; tone: string }> = {
  active: { word: "推进中", tone: "text-brand" },
  paused: { word: "已暂停", tone: "text-brand" },
  blocked: { word: "停住了", tone: "text-destructive" },
  budget_limited: { word: "预算花完", tone: "text-destructive" },
  usage_limited: { word: "额度到顶", tone: "text-muted-foreground" },
  complete: { word: "已完成", tone: "text-muted-foreground" },
};

/** 按得动「继续」的四格。`complete` 不在里面：收尾是事实，要往下走得重定一个目标 */
export const GOAL_RESUMABLE: GoalStatus[] = [
  "paused",
  "blocked",
  "usage_limited",
  "budget_limited",
];

/** 那句收尾的引导词。三格停法各有各的说法，全挤成"卡在"会把钱到顶说成模型无能 */
export function goalNoteLead(status: GoalStatus): string {
  switch (status) {
    case "complete":
      return "结论：";
    case "budget_limited":
      return "上限：";
    case "usage_limited":
      return "额度：";
    default:
      return "停在：";
  }
}

/** 停住的那三格才用警示色；`complete` 与 `paused` 不是错 */
export function goalNoteIsWarning(status: GoalStatus): boolean {
  return status === "blocked" || status === "budget_limited" || status === "usage_limited";
}

/** 一条判据的闭合状态词与颜色。`仅上报`（运行时没复验过的那几条）必须长在行上，
 *  不许只放 tooltip——那是完成门分档复验在人这头的对账面（design-goal-mode.md §5.4）。
 *  目标带与左下角的判据小卡共用这一份，两处各写一份迟早漂成两套记号 */
export function criterionEvidence(criterion: CriterionView): {
  word: string;
  className: string;
  mark: string;
} {
  switch (criterion.evidence) {
    case "runtime":
      return { word: "复验通过", className: "text-brand", mark: "■" };
    case "reported":
      return { word: "仅上报", className: "text-muted-foreground", mark: "◆" };
    case "failed":
      return { word: "失败", className: "text-destructive", mark: "✗" };
    default:
      return { word: "待验", className: "text-muted-foreground", mark: "□" };
  }
}
