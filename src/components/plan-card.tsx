import { useState } from "react";
import {
  IconCircleCheck as CircleCheck,
  IconCircleDot as CircleDot,
  IconCircle as Circle,
  IconChevronDown as ChevronDown,
  IconChevronUp as ChevronUp,
} from "@tabler/icons-react";

import { useChatStore } from "@/store/chat-store";
import { cn } from "@/lib/utils";

/** 计划卡：update_plan 的整份清单，长在输入框上方（与目标带同一列）。
 *  后端是全量替换语义，这里不合并、不排序、不持久——模型给什么画什么 */
export function PlanCard() {
  const activeId = useChatStore((s) => s.activeId);
  const plan = useChatStore((s) => (activeId ? (s.plans[activeId] ?? null) : null));
  const [folded, setFolded] = useState(false);

  if (!plan || plan.steps.length === 0) return null;

  const done = plan.steps.filter((step) => step.status === "completed").length;
  const current = plan.steps.find((step) => step.status === "in_progress");

  return (
    <div className="mb-1.5 rounded-lg border border-border bg-elevated px-3 py-2">
      <button
        type="button"
        onClick={() => setFolded((value) => !value)}
        className="flex w-full items-center gap-2 rounded-sm text-left outline-none transition-colors hover:bg-accent/60 focus-visible:ring-2 focus-visible:ring-ring/55"
      >
        <span className="text-sm font-medium text-foreground">
          计划 · {done}/{plan.steps.length}
        </span>
        {current ? (
          <span className="min-w-0 flex-1 truncate text-xs text-muted-foreground">
            {current.title}
          </span>
        ) : (
          <span className="min-w-0 flex-1 truncate text-xs text-muted-foreground">
            {done === plan.steps.length ? "全部完成" : "还没开始"}
          </span>
        )}
        {folded ? (
          <ChevronDown className="size-3.5 shrink-0 text-muted-foreground" />
        ) : (
          <ChevronUp className="size-3.5 shrink-0 text-muted-foreground" />
        )}
      </button>

      {!folded ? (
        <div className="mt-1.5 space-y-1">
          {plan.explanation ? (
            <p className="text-xs leading-5 text-muted-foreground">{plan.explanation}</p>
          ) : null}
          {plan.steps.map((step, index) => (
            <div key={index} className="flex items-start gap-1.5 text-sm leading-5">
              {step.status === "completed" ? (
                <CircleCheck className="mt-0.5 size-3.5 shrink-0 text-brand-text" />
              ) : step.status === "in_progress" ? (
                <CircleDot className="mt-0.5 size-3.5 shrink-0 animate-pulse text-foreground" />
              ) : (
                <Circle className="mt-0.5 size-3.5 shrink-0 text-muted-foreground/50" />
              )}
              <span
                className={cn(
                  "min-w-0",
                  step.status === "completed" && "text-muted-foreground line-through",
                  step.status === "pending" && "text-muted-foreground",
                )}
              >
                {step.title}
              </span>
            </div>
          ))}
        </div>
      ) : null}
    </div>
  );
}
