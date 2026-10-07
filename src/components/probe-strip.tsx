import { Fragment } from "react";
import {
  IconBolt as Bolt,
  IconBraces as Braces,
  IconCheck as Check,
  IconMessage2 as Message2,
  IconRoute as Route,
  IconShieldCheck as ShieldCheck,
} from "@tabler/icons-react";

import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";

export interface ProbeStage {
  key: string;
  detail: string;
  /** 格子的读色：ok 一致 / info 中性 / warn 要告警。缺省 = 老探针帧，照常配色 */
  tone?: "ok" | "info" | "warn";
  /** 悬停展开的完整读数（模型对账格：请求→实发→上游 三元组） */
  hint?: string;
}

/** 阶段键 → 图标与文案。后端只发 key+detail，视觉归前端 */
const STAGE_META: Record<string, { label: string; icon: typeof Bolt }> = {
  input: { label: "用户输入", icon: Message2 },
  payload: { label: "载荷序列化", icon: Braces },
  egress: { label: "出站链路", icon: Route },
  ttft: { label: "服务商响应", icon: Bolt },
  model: { label: "模型", icon: ShieldCheck },
  usage: { label: "完成", icon: Check },
};

/** tone → 格圈的配色。缺省（老探针）沿用"活跃天蓝、完成翠绿"的老配色 */
const TONE_RING: Record<string, string> = {
  ok: "border-emerald-500/60 bg-surface text-emerald-400",
  info: "border-sky-400/60 bg-surface text-sky-300",
  warn: "border-amber-500/70 bg-surface text-amber-400",
};

const STAGE_W = 72;
const CONNECTOR_W = 18;

/**
 * 请求链路的胶囊动画条：一排阶段图标 + 连接线 + 一颗游动的蓝色光点。
 * 活跃格光点停驻、图标脉冲；完成格绿环收尾。只读 `journey`（store），
 * 不做任何请求。尺寸按 h-12 顶栏设计（总高约 40px）
 */
export function ProbeStrip({ stages, live = true }: { stages: ProbeStage[]; live?: boolean }) {
  if (stages.length === 0) return null;
  const active = stages.length - 1;

  return (
    <div className="relative flex items-start" style={{ minWidth: stages.length * STAGE_W }}>
      {/* 游动光点：停在最新阶段的图标上，随阶段推进滑动；回合收尾后淡出 */}
      <span
        aria-hidden
        className="pointer-events-none absolute top-[6px] size-[8px] rounded-full bg-sky-400 transition-[left,opacity] duration-500 ease-out"
        style={{
          left: active * (STAGE_W + CONNECTOR_W) + STAGE_W / 2 - 4,
          opacity: live ? 1 : 0,
          boxShadow: "0 0 10px 2px rgba(56, 189, 248, .65)",
        }}
      />
      {stages.map((stage, index) => {
        const meta = STAGE_META[stage.key] ?? { label: stage.key, icon: Check };
        const Icon = meta.icon;
        const isActive = live && index === active;
        // 有 tone 的格子按 tone 说话（warn 的黄圈就是要让人一眼看见）；
        // 没有 tone 的格子沿用老配色——活跃天蓝、完成翠绿
        const ring =
          stage.tone != null
            ? TONE_RING[stage.tone] ?? TONE_RING.ok
            : isActive
              ? "border-sky-400/70 bg-surface text-sky-300"
              : "border-emerald-500/60 bg-surface text-emerald-400";
        const badge = (
          <div className="flex shrink-0 flex-col items-center" style={{ width: STAGE_W }}>
            <span
              className={`flex size-[18px] items-center justify-center rounded-full border ${ring} ${
                isActive ? "animate-pulse" : ""
              }`}
            >
              <Icon className="size-[11px]" />
            </span>
            <span className="mt-1 whitespace-nowrap text-[9px] font-medium leading-[10px] text-foreground">
              {meta.label}
            </span>
            <span className="max-w-full truncate whitespace-nowrap text-[8px] leading-[10px] text-muted-foreground">
              {stage.detail}
            </span>
          </div>
        );
        return (
          <Fragment key={stage.key}>
            {index > 0 ? (
              <span className="mt-[9px] h-px shrink-0 bg-border" style={{ width: CONNECTOR_W }} />
            ) : null}
            {stage.hint ? (
              <Tooltip>
                <TooltipTrigger asChild>
                  <div className="cursor-default">{badge}</div>
                </TooltipTrigger>
                <TooltipContent side="bottom" className="max-w-72">
                  <p className="font-mono text-xs leading-5">{stage.hint}</p>
                </TooltipContent>
              </Tooltip>
            ) : (
              badge
            )}
          </Fragment>
        );
      })}
    </div>
  );
}
