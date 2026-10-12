import { useEffect, useState } from "react";
import {
  IconBrain as Brain,
  IconChevronDown as ChevronDown,
  IconChevronRight as ChevronRight,
} from "@tabler/icons-react";

import { DiffStat, FileGlyph } from "@/components/tool-bits";
import { ToolCard } from "@/components/tool-card";
import { flowTitle, isFlowOpen, thinkingPreview } from "@/lib/run-flow";
import {
  STATUS_TEXT,
  detailOf,
  diffStatOf,
  fileTargetOf,
  planProgressOf,
  toolLook,
} from "@/lib/tool-status";
import { cn } from "@/lib/utils";
import type { Message } from "@/types/chat";

/** 秒表：只在跑着的时候走时。跑完之后读收尾那一刻钉在消息上的那个数 */
function useElapsed(message: Message) {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!message.streaming) return;
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [message.streaming]);
  return message.streaming ? now - message.createdAt : (message.durationMs ?? 0);
}

/**
 * 这一轮正在做什么：一条竖向时间线（对齐设计稿的卡片语言）。
 *
 * 收起时只是一行「已处理 N 步，用时 X」的灰色小字；展开后每一步一个格：
 * 左列圆点 + 穿起的连线，中间动词与目标文件，右侧状态读数。圆点三态——
 * 走完的实心灰、正在走的品牌蓝、失败的报错红。
 *
 * 它替代的是"只有一个光标在闪"——光标只回答"还在出字"，回答不了"它在忙哪件事、
 * 已经做了几件、做了多久"。顺序按事件真实到达的顺序（思考一段、一个工具、又思考一段）。
 *
 * 待批准的那一发一定展开：把一张要你点头的卡收进折叠区，等于把这个决定从你手里拿走。
 */
export function RunFlow({ message }: { message: Message }) {
  const [manual, setManual] = useState<boolean | null>(null);
  const [openCalls, setOpenCalls] = useState<ReadonlySet<string>>(new Set());
  const elapsed = useElapsed(message);
  const open = isFlowOpen(message, manual);
  const steps = message.steps ?? [];
  const byId = new Map((message.toolCalls ?? []).map((call) => [call.id, call]));
  const Chevron = open ? ChevronDown : ChevronRight;
  const lastStep = steps.at(-1);

  function toggleDetail(callId: string) {
    setOpenCalls((held) => {
      const next = new Set(held);
      if (next.has(callId)) next.delete(callId);
      else next.add(callId);
      return next;
    });
  }

  return (
    <div className="my-1">
      <button
        type="button"
        aria-expanded={open}
        onClick={() => setManual(!open)}
        className="flex w-full items-center gap-1.5 rounded-md px-1 py-1 text-left outline-none transition-colors hover:bg-accent/60 focus-visible:ring-2 focus-visible:ring-ring/45"
      >
        <Chevron className="size-3.5 shrink-0 text-foreground-tertiary" />
        <span
          className={cn(
            "min-w-0 flex-1 truncate text-sm tabular-nums",
            message.streaming ? "text-foreground-secondary" : "text-foreground-tertiary",
          )}
        >
          {flowTitle(message, elapsed)}
        </span>
        {message.streaming ? (
          <span className="h-[1.1em] w-0.5 shrink-0 animate-caret-pulse bg-brand" />
        ) : null}
      </button>

      {open ? (
        <ol className="mt-0.5 ml-1.5">
          {steps.map((step, index) => {
            const call = step.kind === "tool" ? byId.get(step.callId) : undefined;
            // 只有"最后一步且思考还在出字"才算正在想：答案开始流之后 reasoningStreaming 就灭了
            const live = step === lastStep && message.reasoningStreaming === true;
            const look = call ? toolLook(call.name) : null;
            const KindIcon = step.kind === "thinking" ? Brain : (look?.Icon ?? Brain);
            const label = step.kind === "thinking" ? "思考" : (look?.verb ?? call?.name ?? "工具");
            const target = call ? fileTargetOf(call) : null;
            const diff = call ? diffStatOf(call) : null;
            const progress = call ? planProgressOf(call) : null;
            const detail =
              step.kind === "thinking"
                ? thinkingPreview(message, step, live)
                : call
                  ? detailOf(call)
                  : "";
            const awaiting = call?.status === "pending";
            const failed = call?.status === "failed" || call?.status === "denied";
            const showCard = awaiting || (call ? openCalls.has(call.id) : false);
            const isLast = index === steps.length - 1;

            return (
              <li key={step.id} className="flex gap-2.5">
                {/* 左列：圆点 + 穿起各格的连线。连线只画到下一格，最后一格不画 */}
                <div className="flex w-2 shrink-0 flex-col items-center">
                  <span
                    aria-hidden
                    className={cn(
                      "mt-[7px] size-2 shrink-0 rounded-full",
                      failed
                        ? "bg-destructive"
                        : live
                          ? "bg-brand"
                          : awaiting
                            ? "border-[1.5px] border-border bg-elevated"
                            : "bg-muted-foreground/50",
                    )}
                  />
                  {!isLast ? <span className="mt-0.5 w-px flex-1 bg-border" /> : null}
                </div>

                <div className="min-w-0 flex-1">
                  <button
                    type="button"
                    disabled={!call}
                    onClick={() => call && toggleDetail(call.id)}
                    className={cn(
                      "flex w-full items-baseline gap-2 rounded-md py-[3px] text-left outline-none focus-visible:ring-2 focus-visible:ring-ring/45",
                      call ? "transition-colors hover:bg-accent/60" : "cursor-default",
                    )}
                  >
                    <KindIcon
                      className={cn(
                        "size-3.5 shrink-0 translate-y-[2px]",
                        failed ? "text-destructive" : "text-foreground-tertiary",
                      )}
                    />
                    <span
                      className={cn(
                        "shrink-0 text-sm leading-5",
                        step.kind === "tool" || live
                          ? "font-medium text-foreground"
                          : "text-foreground-secondary",
                      )}
                    >
                      {label}
                    </span>

                    {target ? (
                      <span className="flex min-w-0 items-baseline gap-1.5">
                        <FileGlyph ext={target.ext} />
                        <span className="truncate font-mono text-xs leading-5 text-foreground">
                          {target.name}
                        </span>
                        <span className="hidden shrink-0 truncate font-mono text-xs leading-5 text-foreground-quaternary sm:inline">
                          {target.dir}
                        </span>
                      </span>
                    ) : null}
                    {detail ? (
                      <span className="min-w-0 flex-1 truncate text-xs leading-5 text-foreground-tertiary">
                        {detail}
                      </span>
                    ) : (
                      <span className="min-w-0 flex-1" />
                    )}
                    {diff ? <DiffStat added={diff.added} removed={diff.removed} /> : null}
                    {progress ? (
                      <span className="shrink-0 rounded bg-muted px-1.5 text-2xs tabular-nums text-foreground-secondary">
                        {progress.done} / {progress.total}
                      </span>
                    ) : null}

                    <span
                      className={cn(
                        "ml-auto shrink-0 text-xs leading-5",
                        failed
                          ? "text-destructive"
                          : live
                            ? "text-brand-text"
                            : "text-foreground-quaternary",
                      )}
                    >
                      {call
                        ? STATUS_TEXT[call.status]
                        : step.kind === "thinking"
                          ? message.reasoningStreaming && step === lastStep
                            ? "思考中"
                            : ""
                          : ""}
                    </span>
                  </button>
                  {call && showCard ? (
                    <div className="pb-1 pl-5">
                      <ToolCard call={call} />
                    </div>
                  ) : null}
                </div>
              </li>
            );
          })}
        </ol>
      ) : null}
    </div>
  );
}
