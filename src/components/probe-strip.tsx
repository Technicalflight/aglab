import { Fragment } from "react";
import {
  IconBolt as Bolt,
  IconBraces as Braces,
  IconCheck as Check,
  IconMessage2 as Message2,
  IconRoute as Route,
} from "@tabler/icons-react";

export interface ProbeStage {
  key: string;
  detail: string;
}

/** 阶段键 → 图标与文案。后端只发 key+detail，视觉归前端 */
const STAGE_META: Record<string, { label: string; icon: typeof Bolt }> = {
  input: { label: "用户输入", icon: Message2 },
  payload: { label: "载荷序列化", icon: Braces },
  egress: { label: "出站链路", icon: Route },
  ttft: { label: "服务商响应", icon: Bolt },
  usage: { label: "完成", icon: Check },
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
        return (
          <Fragment key={stage.key}>
            {index > 0 ? (
              <span className="mt-[9px] h-px shrink-0 bg-border" style={{ width: CONNECTOR_W }} />
            ) : null}
            <div className="flex shrink-0 flex-col items-center" style={{ width: STAGE_W }}>
              <span
                className={
                  isActive
                    ? "flex size-[18px] animate-pulse items-center justify-center rounded-full border border-sky-400/70 bg-surface text-sky-300"
                    : "flex size-[18px] items-center justify-center rounded-full border border-emerald-500/60 bg-surface text-emerald-400"
                }
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
          </Fragment>
        );
      })}
    </div>
  );
}
