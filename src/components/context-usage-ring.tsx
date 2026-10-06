import { useEffect, useState } from "react";

import { fetchContextBreakdown, sessionCacheUsage } from "@/lib/chat-transport";
import { formatTokensCompact } from "@/lib/format";
import { cn } from "@/lib/utils";
import { effectiveContextWindow } from "@/types/chat";

import { Button } from "@/components/ui/button";
import { HoverCard, HoverCardContent, HoverCardTrigger } from "@/components/ui/hover-card";
import type { ContextBreakdown, SessionCacheUsage } from "@/types/chat";
import { useChatStore } from "@/store/chat-store";

/** K 缩写：241.9K / 1000.0K，和分段条上的比例一个口径 */
function formatK(value: number): string {
  return `${(value / 1000).toFixed(1)}K`;
}

const SEGMENT_STYLES = [
  { bar: "bg-blue-500", dot: "bg-blue-500" },
  { bar: "bg-emerald-500", dot: "bg-emerald-500" },
  { bar: "bg-amber-500", dot: "bg-amber-500" },
  { bar: "bg-violet-500", dot: "bg-violet-500" },
  { bar: "bg-pink-500", dot: "bg-pink-500" },
] as const;

/**
 * 圆环的颜色档：70% 往琥珀走（该留意了），90% 往红走（自动压缩的闸就在那儿）。
 * 90 这条线不是画的——后端按窗口 90% 触发自动压缩，环红的那一刻就是它要动手的时候
 */
function ringTone(percent: number): string {
  if (percent >= 90) return "stroke-destructive";
  if (percent >= 70) return "stroke-amber-500";
  return "stroke-brand";
}

function UsageRing({ percent }: { percent: number }) {
  const size = 16;
  const stroke = 2;
  const radius = (size - stroke) / 2;
  const circumference = 2 * Math.PI * radius;
  const clamped = Math.min(Math.max(percent, 0), 100);

  return (
    <svg
      width={size}
      height={size}
      viewBox={`0 0 ${size} ${size}`}
      // 进度从顶部顺时针走：SVG 的圆默认从 3 点钟起笔，转回去四分之一圈
      className="-rotate-90"
      aria-hidden
    >
      <circle
        cx={size / 2}
        cy={size / 2}
        r={radius}
        fill="none"
        strokeWidth={stroke}
        className="stroke-muted"
      />
      {clamped > 0 ? (
        <circle
          cx={size / 2}
          cy={size / 2}
          r={radius}
          fill="none"
          strokeWidth={stroke}
          strokeLinecap="round"
          className={cn("transition-[stroke-dashoffset] duration-500", ringTone(clamped))}
          strokeDasharray={circumference}
          strokeDashoffset={circumference * (1 - clamped / 100)}
        />
      ) : null}
    </svg>
  );
}

/**
 * 输入框右下角的上下文用量：圆环是当前的占用比，悬停展开右栏里原来那一整块——
 * 分段条、各段占比、缓存命中与「立即压缩上下文」都在卡里。
 * 数字口径与右栏旧格完全一致（搬过来的，不是重算的一份）：主数字优先用服务商真实值，
 * 没有台账才退回按字符的估算。
 */
export function ContextUsageRing() {
  const config = useChatStore((s) => s.config);
  const messages = useChatStore((s) => s.messages);
  const attachments = useChatStore((s) => s.attachments);
  const mcpServers = useChatStore((s) => s.mcpServers);
  const activeId = useChatStore((s) => s.activeId);
  const pending = useChatStore((s) => s.pending);
  const usage = useChatStore((s) => s.usage);
  // 会话的能力档：生成会话不占对话上下文，压缩/缓存/分段这些对话模型的概念全部不适用
  const kind = useChatStore((s) => s.kind);
  const isChatSession = kind === "chat";

  const [breakdown, setBreakdown] = useState<ContextBreakdown | null>(null);
  const [error, setError] = useState<string | null>(null);

  // breakdown 的四个分段只跟项目绑定与 MCP 连接状态有关——
  // 对话消息段是前端自算的。依赖里放 messages.length 的话每条消息都会白扫一遍目录
  useEffect(() => {
    let active = true;
    fetchContextBreakdown()
      .then((value) => {
        if (active) {
          setBreakdown(value);
          setError(null);
        }
      })
      .catch((cause) => {
        if (active) setError(cause instanceof Error ? cause.message : String(cause));
      });
    return () => {
      active = false;
    };
  }, [config.activeProjectId, mcpServers]);

  // 缓存命中率按"当前话题"聚合（全局会把别的话题的数字混进来）。
  // done 事件在台账落库之后才到，配合头部的 usage 依赖触发命中改判
  const [sessionCache, setSessionCache] = useState<SessionCacheUsage | null>(null);
  useEffect(() => {
    let active = true;
    sessionCacheUsage(activeId)
      .then((value) => {
        if (active) setSessionCache(value);
      })
      .catch(() => {
        if (active) setSessionCache(null);
      });
    return () => {
      active = false;
    };
  }, [activeId, messages.length, usage]);

  // 对话消息段：正文按字符数（中文口径），每条 +8 抵 role 与 JSON 结构开销；
  // 文本附件已拼进最后一条用户消息，用它的字符数补上。
  // 图片不是文字：它按 base64 出站，占的是 4/3 个字符——按文件字节算会低报三分之一，
  // 而这一格低报的代价是"看着还有余量，一发就爆窗口"
  const messageChars =
    messages.reduce((sum, message) => sum + message.content.length + 8, 0) +
    attachments.reduce(
      (sum, attachment) =>
        sum +
        (attachment.kind === "image"
          ? Math.ceil(attachment.chars / 3) * 4
          : attachment.chars),
      0,
    );

  const segments = [
    // 每场对话独立隔离：还没有消息时所有段都是 0——
    // 固定开销（系统提示词/工具/技能）属于"发送那一刻才成立"的预算，不该预支给新对话
    { label: "系统提示词", chars: messages.length > 0 ? breakdown?.systemChars ?? 0 : 0 },
    { label: "工具", chars: messages.length > 0 ? breakdown?.toolsChars ?? 0 : 0 },
    { label: "对话消息", chars: messageChars },
    { label: "连接器及 MCP", chars: messages.length > 0 ? breakdown?.mcpChars ?? 0 : 0 },
    { label: "技能", chars: messages.length > 0 ? breakdown?.skillsChars ?? 0 : 0 },
  ].map((segment, index) => ({ ...segment, ...SEGMENT_STYLES[index] }));

  const estimatedChars = segments.reduce((sum, segment) => sum + segment.chars, 0);
  // 这一格以前有两种单位：有台账时是 token 除以 token，没台账时是**字符**除以 token，
  // 而分段条永远按字符算。现在只有一把尺——Rust 那边量出来的那个下界（§15）。
  // 除以下界而不是上界：宁可显示得多一点，也不要报出一个"还剩一半"而服务商接不住
  const perToken = breakdown?.charsPerToken || 1;
  const estimatedUsed = estimatedChars / perToken;
  // 主数字优先用服务商真实值：最近一轮 prompt（输入）+ 输出——
  // 输出会成为下一轮的输入，所以"当前占用"= 两者之和，与「上下文占用」一行对得上；
  // 还没有台账记录时才退回估算
  const realUsed =
    sessionCache && sessionCache.requests > 0
      ? sessionCache.lastInputTokens + sessionCache.lastOutputTokens
      : null;
  const used = realUsed ?? estimatedUsed;
  // 分母：手动指定池成员时静态解析（与后端请求时的结算链同一条，发不发消息都一样）；
  // 其余模式先按"下一发真正用的模型"静态解析——对话档读档位行（与模型选择器同一格），
  // Done 带回的真实窗口只在它属于当前模型时接管（换了模型后旧台账是上一个模型的事实，
  // 压住新解析就会把卡片钉在旧容量上）；两者都缺再退顶层读数
  const chatModel = config.kindModels?.chat || config.model;
  const staticWindow = effectiveContextWindow(config, chatModel || undefined);
  const total =
    config.modelPool.mode === "pinned"
      ? staticWindow
      : usage?.contextTokens && usage.contextTokens > 0 && usage.model === chatModel
        ? usage.contextTokens
        : staticWindow;
  // 生成会话不占上下文窗口：圆环恒为 0，不是没算，是真的没有"占用"这回事
  const percent =
    isChatSession && total > 0 ? (used / total) * 100 : 0;

  return (
    <HoverCard openDelay={150} closeDelay={120}>
      <HoverCardTrigger asChild>
        <button
          type="button"
          aria-label={`上下文用量 ${percent.toFixed(1)}%`}
          className="flex size-7 items-center justify-center rounded-lg text-muted-foreground outline-none transition-colors hover:bg-accent hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45"
        >
          <UsageRing percent={percent} />
        </button>
      </HoverCardTrigger>
      <HoverCardContent side="top" align="end" className="w-[330px] p-4">
        <p className="text-xs font-medium tracking-[0.08em] text-foreground-tertiary uppercase">
          上下文用量
        </p>

        {/* 生成会话不占对话上下文：每次生成相互独立，压缩/缓存/分段都是对话模型的概念。
            这些都不渲染，只给一句说明——挂一个假的百分比比没有更误导 */}
        {!isChatSession ? (
          <p className="mt-2 text-xs leading-5 text-muted-foreground">
            生图/视频会话不占用对话上下文——每次生成相互独立，没有需要压缩或统计的历史。
          </p>
        ) : (
        <>
        <div className="mt-1.5 flex items-baseline gap-2">
          <span className="text-2xl font-semibold tabular-nums text-foreground">
            {percent.toFixed(1)}%
          </span>
          <span className="text-xs tabular-nums text-muted-foreground">
            已使用 {formatK(used)} / {formatK(total)}
          </span>
        </div>

        {/* 分段条：占多少画多少，零段不画；最后一根吃掉剩余宽度免得条尾留白 */}
        <div className="mt-2 flex h-1.5 gap-px overflow-hidden rounded-full bg-muted">
          {segments
            .filter((segment) => segment.chars > 0)
            .map((segment) => (
              <div
                key={segment.label}
                className={cn("h-full", segment.bar)}
                style={{
                  width: `${total > 0 ? Math.min(segment.chars / perToken / total, 1) * 100 : 0}%`,
                }}
                title={`${segment.label} · ${formatK(segment.chars)}`}
              />
            ))}
        </div>

        {error ? (
          <p className="mt-2 text-xs text-destructive">读取失败：{error}</p>
        ) : (
          <ul className="mt-2 space-y-1">
            {segments.map((segment) => (
              <li key={segment.label} className="flex items-center gap-2 text-xs">
                <span className={cn("size-1.5 shrink-0 rounded-full", segment.dot)} />
                <span className="flex-1 truncate text-muted-foreground">{segment.label}</span>
                <span className="tabular-nums text-foreground">
                  {total > 0 ? `${((segment.chars / total) * 100).toFixed(1)}%` : "—"}
                </span>
              </li>
            ))}
            {(() => {
              // 缓存命中率只统计"这场对话"，按话题 id 过滤台账。
              // 主显示是"最近一轮"的命中率（诊断缓存现在是否生效），
              // 累计值放小字做参照
              // 四态与缓存流程对齐：turn 开始乐观置"判定中"（本轮 usage 未回）→
              // done 改判（usage.cachedTokens 即最近一轮命中，null 是服务商没回这个字段）→
              // 切回旧话题用台账累计回退
              const dot = <span className="size-1.5 shrink-0 rounded-full bg-brand" />;
              const label = (text: string) => (
                <span className="flex-1 truncate text-muted-foreground">{text}</span>
              );
              if (pending && !usage) {
                return (
                  <li className="flex items-center gap-2 pt-1 text-xs">
                    {dot}
                    {label("缓存命中")}
                    <span className="tabular-nums text-brand-text">本轮判定中…</span>
                  </li>
                );
              }
              const inputTokens = sessionCache?.inputTokens ?? 0;
              const cachedTokens = sessionCache?.cachedTokens ?? 0;
              const requests = sessionCache?.requests ?? 0;
              if (requests <= 0) {
                return (
                  <li className="flex items-center gap-2 pt-1 text-xs">
                    {dot}
                    {label("缓存命中")}
                    <span className="tabular-nums text-brand-text">0.0%</span>
                  </li>
                );
              }
              // "未上报"必须是一个独立状态：把它画成 0%，会让人以为前缀被改坏了，
              // 而真相只是这家服务商不回缓存字段
              const reported = usage
                ? usage.cachedTokens !== null
                : (sessionCache?.lastCacheReported ?? true);
              if (!reported) {
                return (
                  <li className="flex items-center gap-2 pt-1 text-xs">
                    {dot}
                    {label("缓存命中")}
                    <span className="tabular-nums text-muted-foreground">服务商未上报</span>
                  </li>
                );
              }
              // done 改判优先：本轮的真实 cached/input 来自服务商 usage（比台账回查更即时）
              const thisInput = usage?.inputTokens ?? 0;
              const lastInput = thisInput > 0 ? thisInput : sessionCache?.lastInputTokens ?? 0;
              const lastCached =
                thisInput > 0 ? (usage?.cachedTokens ?? 0) : sessionCache?.lastCachedTokens ?? 0;
              const lastHit = lastInput > 0 ? (lastCached / lastInput) * 100 : 0;
              const totalHit = inputTokens > 0 ? (cachedTokens / inputTokens) * 100 : 0;
              // 命中率是百分比，它不告诉你"这一场亏了多少"。白付量才是能拿去决定
              // "要不要改用法"的那个数
              const wasted = sessionCache?.wastedTokens ?? 0;
              const wastedCost = sessionCache?.wastedCostUsd ?? 0;
              // 累计只含上报过的那些笔，说清分母，否则它看起来像全量命中率
              const reportedRequests = sessionCache?.reportedRequests ?? 0;
              const partial = reportedRequests > 0 && reportedRequests < requests;
              return (
                <li className="flex flex-col gap-0.5 pt-1 text-xs">
                  <div className="flex items-center gap-2">
                    {dot}
                    {label("缓存命中（最近一轮）")}
                    <span className="tabular-nums text-brand-text">
                      {lastInput > 0 ? `${lastHit.toFixed(1)}%` : "—"}
                    </span>
                  </div>
                  <div className="pl-3.5 text-2xs leading-4 text-muted-foreground/80">
                    {lastCached > 0
                      ? `${formatTokensCompact(lastCached)} / ${formatTokensCompact(lastInput)} tokens · 话题累计 ${totalHit.toFixed(1)}%${
                          partial ? `（仅 ${reportedRequests}/${requests} 笔上报）` : ""
                        }`
                      : "最近一轮未命中缓存——输入过短，或前缀在这一轮变了。"}
                    {wasted > 0
                      ? ` · 白付 ${formatTokensCompact(wasted)} tokens${
                          wastedCost > 0 ? `（约 $${wastedCost.toFixed(4)}）` : "（无价格表，未计价）"
                        }`
                      : ""}
                  </div>
                </li>
              );
            })()}
          </ul>
        )}
        </>
        )}

        {isChatSession ? <CompactButton /> : null}
      </HoverCardContent>
    </HoverCard>
  );
}

/** 手动压缩入口：觉得上下文太脏了就主动清一次，不用等 90% 自动触发 */
function CompactButton() {
  const pending = useChatStore((s) => s.pending);
  const compact = useChatStore((s) => s.compactConversation);

  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<string | null>(null);

  // 能不能压由后端判断（它读话题日志）。界面以前自己数一遍历史来决定按钮可用性，
  // 那是把第二份真相当依据——压不动时后端会回话，界面无需抢先猜
  const disabled = pending || busy;

  return (
    <div className="mt-3">
      <Button
        className="w-full"
        variant="subtle"
        size="sm"
        disabled={disabled}
        onClick={() => {
          setBusy(true);
          setNote(null);
          void compact()
            .then(() => setNote("已压缩。摘要见话题顶部，任务上下文已衔接。"))
            .catch((cause) =>
              setNote(cause instanceof Error ? cause.message : String(cause)),
            )
            .finally(() => setBusy(false));
        }}
      >
        {busy ? "压缩中…" : "立即压缩上下文"}
      </Button>
      {note ? (
        <p className="mt-1 text-right text-2xs leading-4 text-muted-foreground">{note}</p>
      ) : null}
    </div>
  );
}
