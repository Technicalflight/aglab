import { useEffect, useRef, useState } from "react";
import {
  IconChevronDown as ChevronDown,
  IconPlayerPause as PlayerPause,
  IconPlayerPlay as PlayerPlay,
  IconTarget as Target,
  IconX as X,
} from "@tabler/icons-react";

import { Button } from "@/components/ui/button";
import {
  GOAL_RESUMABLE,
  GOAL_STATUS_WORD,
  criterionEvidence,
  goalNoteIsWarning,
  goalNoteLead,
} from "@/lib/goal-status";
import { formatUsd, spentLabel } from "@/lib/format";
import { useChatStore } from "@/store/chat-store";
import { cn } from "@/lib/utils";
import type { ModeState } from "@/types/chat";

/** 规划档是只读红线：目标在那里被强制按住，这一行要说得出是被谁按的 */
function wordFor(mode: ModeState): { text: string; tone: string } {
  const base = GOAL_STATUS_WORD[mode.status];
  return mode.mode === "plan" && mode.status === "paused"
    ? { text: "已暂停（规划档）", tone: base.tone }
    : { text: base.word, tone: base.tone };
}

/** 花费那一格后面跟着的"还剩多少"。不设上限时必须把这句话说完：
 *  目标没有轮次上限，它是屏幕上唯一提示"没人拦得住"的东西 */
function budgetPart(mode: ModeState): string {
  const spent = spentLabel(mode.spentUsdE8);
  if (mode.maxCostUsdE8 <= 0) return `已花 ${spent} · 无上限`;
  return `已花 ${spent} / 上限 ${formatUsd(mode.maxCostUsdE8 / 1e8)}`;
}

/**
 * 目标带：这一支话题正挂着的目标，长在输入框上方，不是左下角那张浮层卡。
 *
 * 它管当前这一支；`GoalDock` 从此只管**别的**话题——同一个数在屏幕上印两遍，
 * 就是等着漂。它也不在这上面加任何决定：读数全从 `ModeView` 来，动作全是已有的命令。
 * 展开体是判据清单：窗口约三条、内部滚动，执行推进一格就平滑滚到下一条
 * 还没闭合的判据——"它凭什么说自己干完了"和"现在做到哪一条了"都长在这条带上
 */
export function GoalStrip() {
  const mode = useChatStore((s) => s.mode);
  const pending = useChatStore((s) => s.pending);
  const modeBusy = useChatStore((s) => s.modeBusy);
  const activeId = useChatStore((s) => s.activeId);
  const goalPause = useChatStore((s) => s.goalPause);
  const goalResume = useChatStore((s) => s.goalResume);
  const goalDiscard = useChatStore((s) => s.goalDiscard);
  const setGoalDialog = useChatStore((s) => s.setGoalDialog);

  const [expanded, setExpanded] = useState(false);
  // 跟随执行的滚动。hooks 全在早退之前：目标有没有与钩子无关
  const listRef = useRef<HTMLUListElement | null>(null);
  const criteria = mode?.contract?.criteria ?? [];
  const evidenceSignature = criteria.map((criterion) => criterion.evidence).join("|");
  const firstOpenIndex = criteria.findIndex((criterion) => criterion.evidence === "open");
  useEffect(() => {
    if (!expanded) return;
    const list = listRef.current;
    if (!list || firstOpenIndex < 0) return;
    const target = list.children[firstOpenIndex] as HTMLElement | undefined;
    // nearest：目标已在窗口里就一动不动——人自己滚的时候不打架
    target?.scrollIntoView({ behavior: "smooth", block: "nearest" });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [expanded, evidenceSignature]);

  if (!mode?.objective) return null;

  const status = wordFor(mode);
  const running = mode.status === "active";
  const closedCount = criteria.filter(
    (criterion) => criterion.evidence === "runtime" || criterion.evidence === "reported",
  ).length;
  const failedCount = criteria.filter((criterion) => criterion.evidence === "failed").length;

  return (
    <div className="mx-2.5 mt-2.5 rounded-xl border border-brand/30 bg-brand/5 px-3 py-2">
      <div className="flex items-center gap-2">
        <span
          className={cn(
            "size-1.5 shrink-0 rounded-full",
            running && pending ? "animate-pulse bg-brand" : running ? "bg-brand/60" : "bg-border",
          )}
        />
        <Target className="size-3.5 shrink-0 text-brand-text" />
        <span
          className="min-w-0 flex-1 truncate text-sm text-foreground"
          title={mode.objective}
        >
          {mode.objective}
        </span>
        {criteria.length > 0 ? (
          <button
            type="button"
            title="展开判据清单：它凭什么说自己干完了，现在做到哪一条"
            className="flex shrink-0 cursor-pointer items-center gap-1 rounded-md px-1 py-0.5 text-xs tabular-nums text-muted-foreground outline-none transition-colors hover:bg-accent hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45"
            onClick={() => setExpanded((held) => !held)}
          >
            {closedCount}/{criteria.length} 判据
            {failedCount > 0 ? (
              <span className="text-destructive">· {failedCount} 失败</span>
            ) : null}
            <ChevronDown
              className={cn("size-3 transition-transform", expanded && "rotate-180")}
            />
          </button>
        ) : (
          // 旧式目标没有契约：完成门对它退化（§4.3），这一格要说得出
          <span className="shrink-0 text-xs text-muted-foreground">无判据</span>
        )}
        <span className={cn("shrink-0 text-xs", status.tone)}>{status.text}</span>
        <span
          className="shrink-0 text-xs tabular-nums text-muted-foreground"
          title={
            mode.spentUsdE8 === null
              ? "台账读不出来。设了上限时那一头会直接报错停下，所以这里宁可显示不知道"
              : undefined
          }
        >
          {mode.turnsUsed} 轮 · {budgetPart(mode)}
        </span>
        <span className="flex shrink-0 items-center gap-1">
          {running ? (
            <Button
              size="sm"
              variant="subtle"
              className="h-6 px-2 text-xs"
              disabled={modeBusy}
              title="跑完手头这一轮就不再自己接下一轮。目标、轮数、已花的钱都留着"
              onClick={() => void goalPause(activeId, true)}
            >
              <PlayerPause className="size-3" />
              暂停
            </Button>
          ) : null}
          {GOAL_RESUMABLE.includes(mode.status) ? (
            <Button
              size="sm"
              variant="brand"
              className="h-6 px-2 text-xs"
              disabled={modeBusy}
              title="接着往下推，并立刻开一轮。账不从零起"
              onClick={() => void goalResume(activeId)}
            >
              <PlayerPlay className="size-3" />
              继续
            </Button>
          ) : null}
          <Button
            size="sm"
            variant="ghost"
            className="h-6 px-2 text-xs"
            disabled={modeBusy}
            title="改目标文字与判据：同一支换文字，账全留。判据改过的那些条，证据作废重验"
            onClick={() => setGoalDialog(true)}
          >
            编辑
          </Button>
          {mode.status !== "complete" ? (
            <Button
              size="sm"
              variant="ghost"
              className="h-6 px-2 text-xs hover:text-destructive"
              disabled={modeBusy}
              title="整份清掉这个目标（目标、轮数、点名的档案都不保留），交互档不变"
              onClick={() => void goalDiscard(activeId)}
            >
              <X className="size-3" />
              结束
            </Button>
          ) : null}
        </span>
      </div>
      {expanded && criteria.length > 0 ? (
        // 窗口约三条（带命令的行两行一条），其余内部滚——八条判据全幅铺开
        // 能把整个对话区顶走。执行推进一格就平滑滚到下一条待验的（见上方 effect）
        <ul
          ref={listRef}
          className="mt-1.5 max-h-32 space-y-0.5 overflow-y-auto border-t border-brand/20 pt-1.5"
        >
          {criteria.map((criterion) => {
            const state = criterionEvidence(criterion);
            return (
              <li
                key={criterion.id}
                className="flex items-baseline gap-1.5 text-xs leading-5"
              >
                <span className={cn("shrink-0 tabular-nums", state.className)}>
                  {state.mark}
                </span>
                <span
                  className="min-w-0 flex-1 text-foreground"
                  title={criterion.command ?? undefined}
                >
                  {criterion.text}
                  {criterion.command ? (
                    <span className="ml-1 font-mono text-2xs text-muted-foreground">
                      {criterion.command}
                    </span>
                  ) : null}
                </span>
                <span className={cn("shrink-0", state.className)}>{state.word}</span>
              </li>
            );
          })}
        </ul>
      ) : null}
      {mode.note ? (
        <p
          className={cn(
            "mt-1 text-xs leading-5",
            goalNoteIsWarning(mode.status) ? "text-destructive" : "text-muted-foreground",
          )}
        >
          {goalNoteLead(mode.status)}
          {mode.note}
        </p>
      ) : null}
    </div>
  );
}
