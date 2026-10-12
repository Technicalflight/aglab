import { Fragment } from "react";

import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";

export interface ProbeStage {
  key: string;
  detail: string;
  /** 格子的读色：ok 一致 / info 中性 / warn 要告警。缺省 = 老探针帧，照常配色 */
  tone?: "ok" | "info" | "warn";
  /** 悬停展开的完整读数（模型对账格：请求→实发→上游 三元组） */
  hint?: string;
}

/** 阶段键 → 文案。后端只发 key+detail，视觉归前端 */
const STAGE_META: Record<string, { label: string }> = {
  input: { label: "用户输入" },
  payload: { label: "载荷序列化" },
  egress: { label: "出站链路" },
  ttft: { label: "服务商响应" },
  model: { label: "模型" },
  usage: { label: "完成" },
};

/**
 * 请求链路的页签条：一排「文字 + 下划线」的阶段格（对齐设计稿的卡片语言——
 * 不再是圆圈图标加连接线）。走过的格子深灰条压深字，正在走的那格品牌蓝；
 * 每格的 detail 与完整对账读数收进悬停提示，一行里只留阶段名。
 *
 * 只读 `stages`（store 里的 journey），不做任何请求。宽度按内容收缩：
 * 阶段多到装不下时由父级的 overflow-x-auto 横向滚，不把字挤变形。
 */
export function ProbeStrip({ stages, live = true }: { stages: ProbeStage[]; live?: boolean }) {
  if (stages.length === 0) return null;
  const active = stages.length - 1;

  return (
    <div className="flex shrink-0 items-end gap-4">
      {stages.map((stage, index) => {
        const meta = STAGE_META[stage.key] ?? { label: stage.key };
        const isActive = live && index === active;
        const warn = stage.tone === "warn";
        const tab = (
          <span className="flex cursor-default flex-col items-center gap-1">
            <span
              className={cn(
                "whitespace-nowrap text-xs leading-none",
                isActive ? "font-medium text-brand-text" : "text-foreground-secondary",
                warn && "text-warning",
              )}
            >
              {meta.label}
            </span>
            <span
              className={cn(
                "h-[2.5px] w-8 rounded-full",
                isActive ? "bg-brand" : warn ? "bg-warning/70" : "bg-muted-foreground/40",
              )}
            />
          </span>
        );
        const tip = (
          // 提示框是反色底（浅色主题下也是深色 spotlight）：文字必须继承容器的
          // text-spotlight-foreground，不能用主题语义色——近黑压深底只有 1.4:1
          <p className="text-xs leading-5">
            <span className="font-medium">{meta.label}</span>
            {stage.detail ? <span className="opacity-70"> · {stage.detail}</span> : null}
          </p>
        );
        return (
          <Fragment key={stage.key}>
            {stage.hint ? (
              <Tooltip>
                <TooltipTrigger asChild>
                  <span className="cursor-default">{tab}</span>
                </TooltipTrigger>
                <TooltipContent side="bottom" className="max-w-72">
                  {tip}
                  <p className="mt-1 font-mono text-2xs leading-4 opacity-70">{stage.hint}</p>
                </TooltipContent>
              </Tooltip>
            ) : stage.detail ? (
              <Tooltip>
                <TooltipTrigger asChild>
                  <span className="cursor-default">{tab}</span>
                </TooltipTrigger>
                <TooltipContent side="bottom">{tip}</TooltipContent>
              </Tooltip>
            ) : (
              tab
            )}
          </Fragment>
        );
      })}
    </div>
  );
}
