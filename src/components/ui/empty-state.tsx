import type * as React from "react";
import { IconLoader2 as Loader2 } from "@tabler/icons-react";

import { cn } from "@/lib/utils";

/**
 * 空状态。空列表不是"出错了"，也不是"页面没做完"——它是一个正常且
 * 短暂的终点。写法上给三件事：一句说清现状、一句说清下一步、给一个动作。
 *
 * 图标默认 aria-hidden：它纯装饰，读屏软件只需要听到标题与描述。
 */
function EmptyState({
  icon,
  title,
  description,
  action,
  className,
  compact = false,
}: {
  icon?: React.ReactNode;
  title: string;
  description?: React.ReactNode;
  /** 主动作，一般是 Button 或链接 */
  action?: React.ReactNode;
  className?: string;
  /** 紧凑档：用于侧栏、卡片内等空间有限处 */
  compact?: boolean;
}) {
  return (
    <div
      data-slot="empty-state"
      className={cn(
        "flex flex-col items-center justify-center text-center",
        compact ? "gap-2 px-4 py-8" : "gap-3 px-6 py-14",
        className,
      )}
    >
      {icon ? (
        <div
          className={cn(
            "flex items-center justify-center rounded-lg bg-fill-2 text-foreground-tertiary",
            compact ? "size-9" : "size-11",
          )}
          aria-hidden
        >
          {icon}
        </div>
      ) : null}
      <p
        className={cn(
          "font-medium text-foreground",
          compact ? "text-sm" : "text-lg",
        )}
      >
        {title}
      </p>
      {description ? (
        <p
          className={cn(
            "max-w-[46ch] text-foreground-secondary",
            compact ? "text-xs leading-5" : "text-base leading-6",
          )}
        >
          {description}
        </p>
      ) : null}
      {action ? <div className="mt-1">{action}</div> : null}
    </div>
  );
}

/** 加载指示器。三档尺寸对应"整页加载 / 区块加载 / 行内加载" */
function Spinner({
  className,
  size = "md",
  label,
}: {
  className?: string;
  size?: "sm" | "md" | "lg";
  /** 给读屏软件的说明，如"正在加载用量" */
  label?: string;
}) {
  return (
    <span role="status" className={cn("inline-flex", className)}>
      <Loader2
        aria-hidden
        className={cn(
          "animate-spin-slow text-foreground-tertiary",
          size === "sm" && "size-3.5",
          size === "md" && "size-4",
          size === "lg" && "size-6",
        )}
      />
      {/* 视觉上没有文字，但读屏要听到"正在加载"，否则只剩一片沉默 */}
      <span className="sr-only">{label ?? "正在加载"}</span>
    </span>
  );
}

export { EmptyState, Spinner };
