import { IconRoute as Route } from "@tabler/icons-react";

import { ProbeStrip } from "@/components/probe-strip";
import { useChatStore } from "@/store/chat-store";
import { cn } from "@/lib/utils";

/**
 * 模型请求链路监控（对话区上方那条画布级头部条）。
 *
 * 对齐设计稿的卡片语言：监控条不再是带边框的卡片，而是画布上的一条头部——
 * 左侧「标题 + 服务商」，右侧阶段页签（文字 + 下划线）与状态灯。它下面的
 * 对话卡才是内容，监控条只是给这条链路一个常驻的读数位置。
 *
 * 常驻而不是"有请求才出现"：会消失的条会让下面的对话区整块跳一下，
 * 而这一跳恰好发生在用户刚按下回车、正盯着链路看的那一刻。没请求时
 * 给一句说明，占位是诚实的。
 *
 * 只读 `journey`（store），不发任何请求。
 */

/** 服务商只露主机名：一行里放不下整个 URL，完整地址悬停可见 */
function hostOf(url: string): string {
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
}

export function RequestChainCard() {
  const journey = useChatStore((s) => s.journey);
  const config = useChatStore((s) => s.config);

  // 收尾的判据与 store 一致：链路里出现了 usage 这一格，这一轮就走完了
  const live = !journey.some((stage) => stage.key === "usage");
  const status =
    journey.length === 0
      ? { text: "待命", tone: "idle" as const }
      : live
        ? { text: "执行中", tone: "live" as const }
        : { text: "已完成", tone: "done" as const };

  return (
    <section className="shrink-0 px-1">
      <div className="flex h-10 items-center gap-3">
        <Route className="size-3.5 shrink-0 text-foreground-tertiary" aria-hidden />
        <h2 className="shrink-0 text-sm font-semibold text-foreground">模型请求链路监控</h2>
        <p className="min-w-0 flex-1 truncate text-xs text-foreground-tertiary">
          {config.baseUrl ? hostOf(config.baseUrl) : "未配置服务商"}
        </p>

        {/* 阶段页签长在标题行右侧：监控是这条链路的"读数"，与标题同一行才读得像一处 */}
        {journey.length > 0 ? (
          <div className="details-scroll min-w-0 overflow-x-auto">
            <ProbeStrip stages={journey} live={live} />
          </div>
        ) : null}

        <span
          className={cn(
            "flex shrink-0 items-center gap-1.5 text-xs",
            status.tone === "live" && "text-foreground",
            status.tone === "done" && "text-foreground-secondary",
            status.tone === "idle" && "text-foreground-tertiary",
          )}
        >
          <span
            aria-hidden="true"
            className={cn(
              "size-1.5 rounded-full",
              status.tone === "live" && "animate-pulse bg-brand",
              status.tone === "done" && "bg-success",
              status.tone === "idle" && "bg-border",
            )}
          />
          {status.text}
        </span>
      </div>

      {journey.length === 0 ? (
        <p className="pb-1.5 text-xs leading-5 text-foreground-tertiary">
          还没有请求。发出第一条消息后，这里会逐格显示它走到了哪儿。
        </p>
      ) : null}
    </section>
  );
}
