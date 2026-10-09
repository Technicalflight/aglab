import { useCallback, useEffect, useRef, useState } from "react";

import { HoverCard, HoverCardContent, HoverCardTrigger } from "@/components/ui/hover-card";
import { excerpt, type Turn } from "@/lib/turns";
import type { FileEdit } from "@/types/chat";

function timeOf(timestamp: number) {
  return new Date(timestamp).toLocaleString("zh-CN", {
    month: "numeric",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

interface Mark {
  id: string;
  ratio: number;
}

/**
 * 对话区右缘的时间线刻度条：一轮一个刻度，位置对应它在那一长串内容里的真实位置，
 * 悬停出该轮的纯文本摘要，点击滚过去并闪一下。
 *
 * 刻度按内容位置摆，不是等距排——等距的话它就退化成一个目录列表，回答不了
 * "我这轮大概在整场对话的哪一段"。
 *
 * 这个文件只导出组件：轮次分组与摘要函数住在 lib/turns.ts。混着导出会让
 * Vite 判定 Fast Refresh 不兼容（"groupTurns export is incompatible"），
 * 改一行样式就得整页重载，还会把模块图搞花到组件报错
 */
export function ConversationTimeline({
  turns,
  edits,
  forks,
  scroller,
  onJump,
}: {
  turns: Turn[];
  edits: FileEdit[];
  /** 轮 id → 那一处有几支答案。有兄弟的轮在 tick 上点一颗点，不另开一块界面 */
  forks: Map<string, number>;
  /** 滚动容器本身，不是 ref：ref 快照会让 effect 在容器被换掉后继续监听旧节点 */
  scroller: HTMLDivElement | null;
  onJump: (turnId: string) => void;
}) {
  // 位置只用来判"当前在哪一轮"，不参与排布（排布是等距居中），
  // 所以存 ref 就够——渲染不需要跟着它重画
  const marksRef = useRef<Mark[]>([]);
  const [railHeight, setRailHeight] = useState(0);
  const [active, setActive] = useState<string | null>(null);
  const lastMeasure = useRef(0);

  const turnsRef = useRef(turns);
  turnsRef.current = turns;
  // 量位置的时机跟"轮集合变没变"挂钩，不跟 turns 数组引用挂钩：
  // 流式期间每个 flush 都换新数组，跟着重跑就是对每一轮做强制布局
  const turnSignature = turns.map((turn) => turn.id).join("\u0000");

  const measure = useCallback(() => {
    if (!scroller) return;
    const total = scroller.scrollHeight || 1;
    const origin = scroller.getBoundingClientRect().top - scroller.scrollTop;
    const next = turnsRef.current.map((turn) => {
      const el = scroller.querySelector<HTMLElement>(`[data-turn-id="${turn.id}"]`);
      if (!el) return { id: turn.id, ratio: 0 };
      const top = el.getBoundingClientRect().top - origin;
      return { id: turn.id, ratio: Math.min(1, Math.max(0, top / total)) };
    });
    marksRef.current = next;
    setRailHeight(scroller.clientHeight);
    lastMeasure.current = Date.now();
  }, [scroller]);

  // 轮集合真正变化（新消息/新轮）时才重测；高度微增长交给滚动节流里的补测
  useEffect(() => {
    measure();
  }, [measure, turnSignature]);

  useEffect(() => {
    if (!scroller) return;
    const observer = new ResizeObserver(measure);
    observer.observe(scroller);
    return () => observer.disconnect();
  }, [measure, scroller]);

  useEffect(() => {
    if (!scroller) return;
    let frame = 0;

    const locate = () => {
      const current = marksRef.current;
      if (current.length === 0) return;
      const at = scroller.scrollTop + 24;
      const total = scroller.scrollHeight || 1;
      let picked = current[0].id;
      for (const mark of current) {
        if (mark.ratio * total <= at) picked = mark.id;
      }
      setActive(picked);
    };

    const onScroll = () => {
      if (frame) return;
      frame = requestAnimationFrame(() => {
        frame = 0;
        // 先量再判：流式回答在长，内容高度一直变，顺序反了就等于拿旧位置判新滚动
        if (Date.now() - lastMeasure.current > 400) measure();
        locate();
      });
    };

    locate();
    scroller.addEventListener("scroll", onScroll, { passive: true });
    return () => {
      scroller.removeEventListener("scroll", onScroll);
      if (frame) cancelAnimationFrame(frame);
    };
  }, [measure, scroller]);

  if (turns.length === 0) return null;

  // 整组居中紧凑排，不铺满轨道：每条占一个 pitch 高的格子，
  // 轮数多了就把 pitch 收小，保证再多也塞得下且始终居中
  const pitch = railHeight ? Math.max(9, Math.min(20, (railHeight * 0.8) / turns.length)) : 14;

  return (
    <div className="pointer-events-none absolute inset-y-0 right-1.5 flex w-7 flex-col items-center justify-center">
      {turns.map((turn, index) => {
        const files = edits.filter((edit) =>
          edit.callIds.some((id) => turn.callIds.includes(id)),
        ).length;
        const isActive = active === turn.id;
        const branches = forks.get(turn.id) ?? 1;

        return (
          <HoverCard key={turn.id} openDelay={60} closeDelay={140}>
            <HoverCardTrigger asChild>
              <button
                type="button"
                onClick={() => onJump(turn.id)}
                aria-label={`跳到第 ${index + 1} 轮${branches > 1 ? `（这一处有 ${branches} 支）` : ""}`}
                style={{ height: pitch }}
                className="group pointer-events-auto flex w-full shrink-0 items-center justify-center rounded-sm outline-none focus-visible:ring-2 focus-visible:ring-ring/55"
              >
                {/* 横条。格子尺寸不变，只有条本身在悬停时变长变粗——
                    否则整列会跟着抖 */}
                <span
                  className={`h-[3px] rounded-full transition-[width,background-color] ${
                    isActive ? "w-6 bg-brand" : "w-3.5 bg-muted-foreground/30"
                  } group-hover:w-7 group-hover:bg-muted-foreground/70`}
                />
                {/* 分叉点：一颗点，不改变格子尺寸。有兄弟这件事得在轨道上就看得见，
                    不然用户要点开每一轮才知道自己还有另一支可看 */}
                {branches > 1 ? (
                  <span className="ml-0.5 size-1 shrink-0 rounded-full bg-brand/75" aria-hidden />
                ) : null}
              </button>
            </HoverCardTrigger>

            <HoverCardContent side="left" align="center" className="p-3">
              <p className="text-2xs leading-4 text-muted-foreground">
                {turn.kind === "compaction" ? "上下文压缩" : `第 ${index + 1} 轮`}
                {" · "}
                {timeOf(turn.at)}
                {files > 0 ? ` · 改了 ${files} 个文件` : ""}
                {turn.tools > 0 ? ` · ${turn.tools} 个工具调用` : ""}
                {turn.failed ? " · 本轮失败" : ""}
                {branches > 1 ? ` · 这一处有 ${branches} 支` : ""}
              </p>
              <p className="mt-1.5 line-clamp-2 text-sm leading-5 text-foreground">
                {turn.kind === "compaction"
                  ? "更早的对话在这里被压缩成摘要"
                  : excerpt(turn.question, 120) || "（没有文字，只发了附件）"}
              </p>
              {turn.answer ? (
                <p className="mt-1.5 line-clamp-4 text-xs leading-5 text-muted-foreground">
                  {excerpt(turn.answer, 220)}
                </p>
              ) : null}
            </HoverCardContent>
          </HoverCard>
        );
      })}
    </div>
  );
}
