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
 * 这一轮正在做什么：一条可折叠的流程。
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
    <div className="my-1 overflow-hidden rounded-lg border border-border bg-surface">
      <button
        type="button"
        aria-expanded={open}
        onClick={() => setManual(!open)}
        className="flex w-full items-center gap-2 px-3 py-2 text-left outline-none focus-visible:ring-2 focus-visible:ring-ring/45"
      >
        <Chevron className="size-3.5 shrink-0 text-muted-foreground" />
        <span
          className={cn(
            "min-w-0 flex-1 truncate text-sm tabular-nums",
            message.streaming ? "text-foreground" : "text-muted-foreground",
          )}
        >
          {flowTitle(message, elapsed)}
        </span>
        {message.streaming ? (
          <span className="h-[1.1em] w-0.5 shrink-0 animate-caret-pulse bg-brand" />
        ) : null}
      </button>

      {open ? (
        <ol className="space-y-0.5 border-t border-border px-3 py-2">
          {steps.map((step) => {
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
            const showCard = awaiting || (call ? openCalls.has(call.id) : false);

            return (
              <li key={step.id}>
                <button
                  type="button"
                  disabled={!call}
                  onClick={() => call && toggleDetail(call.id)}
                  className={cn(
                    "flex w-full items-center gap-2 rounded-lg px-1 py-1 text-left outline-none focus-visible:ring-2 focus-visible:ring-ring/45",
                    call ? "transition-colors hover:bg-accent" : "cursor-default",
                  )}
                >
                  <KindIcon
                    className={cn(
                      "size-3.5 shrink-0",
                      step.kind === "thinking" && "text-muted-foreground",
                      call?.status === "failed" || call?.status === "denied"
                        ? "text-destructive"
                        : "text-muted-foreground",
                    )}
                  />
                  <span
                    className={cn(
                      "shrink-0 text-sm font-medium",
                      step.kind === "tool" ? "text-foreground" : "text-muted-foreground",
                    )}
                  >
                    {label}
                  </span>

                  {target ? (
                    <span className="flex min-w-0 items-center gap-1.5">
                      <FileGlyph ext={target.ext} />
                      <span className="truncate font-medium text-foreground">{target.name}</span>
                      <span className="shrink-0 truncate font-mono text-xs text-muted-foreground/70">
                        {target.dir}
                      </span>
                    </span>
                  ) : null}
                  {diff ? <DiffStat added={diff.added} removed={diff.removed} /> : null}
                  {progress ? (
                    <span className="shrink-0 rounded bg-muted px-1.5 text-2xs tabular-nums text-muted-foreground">
                      {progress.done} / {progress.total}
                    </span>
                  ) : null}
                  {detail ? (
                    <span className="min-w-0 flex-1 truncate font-mono text-xs text-muted-foreground">
                      {detail}
                    </span>
                  ) : null}

                  {call ? (
                    <span
                      className={cn(
                        "ml-auto shrink-0 text-xs",
                        call.status === "failed" || call.status === "denied"
                          ? "text-destructive"
                          : "text-muted-foreground/70",
                      )}
                    >
                      {STATUS_TEXT[call.status]}
                    </span>
                  ) : null}
                </button>
                {call && showCard ? (
                  <div className="pl-5">
                    <ToolCard call={call} />
                  </div>
                ) : null}
              </li>
            );
          })}
        </ol>
      ) : null}
    </div>
  );
}
