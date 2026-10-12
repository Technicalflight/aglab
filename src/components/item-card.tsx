import type { ReactNode } from "react";

import { cn } from "@/lib/utils";

/**
 * 条目卡片（插件 / 技能 / MCP 三页共用的市场卡形态，参照插件市场的设计基准）：
 *
 *   ┌──────────────────────────────┐
 *   │ [瓦片] 标题 徽标      [动作] │
 *   │        描述（按 clamp 截断） │
 *   │        正文区（预览/子清单） │
 *   │        元信息行（小字一行）  │
 *   └──────────────────────────────┘
 *
 * 只管形状不管数据：三页各自的开关、按钮、子清单通过 action / meta / children
 * 塞进来，逻辑一条不动。定高用法由调用方给 className（如 h-44 overflow-hidden）
 * 配合 clamp 截断摘要——高度一致了，网格两列也就没有行洞。`muted` 给停用态；
 * `onClick` 给可点进详情的卡（悬停描边染品牌色作为"能点"的暗示）。
 */
export function ItemCard({
  icon,
  title,
  badges,
  description,
  children,
  meta,
  action,
  clamp,
  muted,
  onClick,
  className,
}: {
  /** 图标瓦片内容（一个 Icon）。不传就用标题首字占位 */
  icon?: ReactNode;
  title: ReactNode;
  /** 标题旁的小徽标（出厂 / 内置 / 个人 / 来源…） */
  badges?: ReactNode;
  description?: ReactNode;
  /** 描述的截断行数：定高卡片的摘要不需要全部显示，超出省略（完整内容进详情） */
  clamp?: 2 | 3 | 4;
  /** 描述下方的正文区：正文预览、工具子清单、命令块这类"卡中内容" */
  children?: ReactNode;
  /** 底部元信息行：小字条目（来源、字数、版本…）。定高卡里钉在卡底 */
  meta?: ReactNode;
  /** 右上角动作位：开关、连接/断开这类常驻操作 */
  action?: ReactNode;
  muted?: boolean;
  onClick?: () => void;
  className?: string;
}) {
  const firstChar =
    typeof title === "string" || typeof title === "number"
      ? String(title).trim().charAt(0).toUpperCase()
      : "";

  return (
    <div
      onClick={onClick}
      className={cn(
        "flex flex-col rounded-xl border border-border bg-surface p-3.5 transition-colors",
        onClick && "cursor-pointer hover:border-brand/40",
        muted && "opacity-60",
        className,
      )}
    >
      <div className="flex items-start gap-3">
        {icon ? (
          <div className="flex size-10 shrink-0 items-center justify-center rounded-lg bg-elevated text-foreground-secondary [&_svg]:size-5">
            {icon}
          </div>
        ) : firstChar ? (
          <div className="flex size-10 shrink-0 items-center justify-center rounded-lg bg-elevated text-sm font-semibold text-foreground-secondary">
            {firstChar}
          </div>
        ) : null}

        <div className="min-w-0 flex-1">
          <div className="flex items-start gap-2">
            <p className="flex min-w-0 flex-wrap items-center gap-x-1.5 gap-y-0.5 text-sm leading-5">
              <span className="min-w-0 truncate font-medium text-foreground">{title}</span>
              {badges}
            </p>
            {action ? <div className="ml-auto flex shrink-0 items-center">{action}</div> : null}
          </div>
          {description ? (
            <div
              className={cn(
                "mt-1 text-xs leading-5 text-muted-foreground",
                clamp === 2 && "line-clamp-2",
                clamp === 3 && "line-clamp-3",
                clamp === 4 && "line-clamp-4",
              )}
            >
              {description}
            </div>
          ) : null}
        </div>
      </div>

      {children ? <div className="mt-2.5 min-w-0">{children}</div> : null}

      {meta ? (
        <div className="mt-auto flex flex-wrap items-center gap-x-3 gap-y-1 pt-2.5 text-2xs leading-4 text-muted-foreground [&_svg]:size-3">
          {meta}
        </div>
      ) : null}
    </div>
  );
}

/** 元信息行里的一个小条目（可带图标）。保证三页的底排密度一致 */
export function ItemMeta({
  icon,
  children,
  className,
}: {
  icon?: ReactNode;
  children: ReactNode;
  className?: string;
}) {
  return (
    <span className={cn("flex min-w-0 items-center gap-1", className)}>
      {icon}
      <span className="min-w-0 truncate">{children}</span>
    </span>
  );
}
