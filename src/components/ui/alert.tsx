import type * as React from "react";
import { IconAlertTriangle as AlertTriangle, IconCircleCheck as CircleCheck, IconInfoCircle as InfoCircle } from "@tabler/icons-react";

import { cn } from "@/lib/utils";

/**
 * 状态提示条。四种语气各配图标——**颜色永远不是唯一的状态信号**，
 * 色觉障碍用户与灰度打印都靠图标分辨，这是 WCAG 1.4.1 的硬要求。
 */
const TONES = {
  info: {
    wrap: "border-info-border bg-info-soft",
    icon: "text-info",
    Icon: InfoCircle,
  },
  success: {
    wrap: "border-success-border bg-success-soft",
    icon: "text-success",
    Icon: CircleCheck,
  },
  warning: {
    wrap: "border-warning-border bg-warning-soft",
    icon: "text-warning",
    Icon: AlertTriangle,
  },
  error: {
    wrap: "border-destructive-border bg-destructive-soft",
    icon: "text-destructive",
    Icon: AlertTriangle,
  },
} as const;

export type AlertTone = keyof typeof TONES;

function Alert({
  tone = "info",
  title,
  children,
  className,
  icon,
  ...props
}: React.ComponentProps<"div"> & {
  tone?: AlertTone;
  title?: string;
  /** 自定义图标；给了就替掉语气默认图标 */
  icon?: React.ReactNode;
}) {
  const { wrap, icon: iconColor, Icon } = TONES[tone];
  return (
    <div
      data-slot="alert"
      // 提示条是"停下来看一眼"的东西，不该打断读屏的线性朗读
      role="group"
      aria-label={title}
      className={cn("flex gap-2.5 rounded-md border px-3 py-2.5", wrap, className)}
      {...props}
    >
      <span className={cn("mt-px shrink-0", iconColor)} aria-hidden>
        {icon ?? <Icon className="size-3.5" />}
      </span>
      <div className="min-w-0 flex-1">
        {title ? <p className="text-xs font-medium text-foreground">{title}</p> : null}
        {children ? (
          <div className={cn("text-xs leading-5 text-foreground-secondary", title && "mt-1")}>
            {children}
          </div>
        ) : null}
      </div>
    </div>
  );
}

export { Alert };
