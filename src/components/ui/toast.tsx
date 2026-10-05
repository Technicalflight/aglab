import type * as React from "react";
import { Toast as ToastPrimitive } from "radix-ui";

import { cn } from "@/lib/utils";

export const ToastProvider = ToastPrimitive.Provider;
export const ToastViewport = ToastPrimitive.Viewport;
export const ToastRoot = ToastPrimitive.Root;
export const ToastTitle = ToastPrimitive.Title;
export const ToastDescription = ToastPrimitive.Description;
export const ToastClose = ToastPrimitive.Close;

/**
 * 贴附在右上角的告警条。刻意不复用品牌色：点缀色只服务选中态与发送键，
 * 告警靠 destructive 和明度分层，混在一起就分不清"出了问题"和"这是当前项"
 */
export function ToastCard({
  tone = "error",
  title,
  detail,
  className,
  ...props
}: React.ComponentProps<typeof ToastRoot> & {
  tone?: "error" | "info";
  title: string;
  detail?: string;
}) {
  return (
    <ToastRoot
      data-slot="toast"
      className={cn(
        "pointer-events-auto flex w-full items-start gap-2.5 rounded-xl border bg-elevated px-3 py-2.5",
        "animate-message-in shadow-lg outline-none",
        tone === "error" ? "border-destructive/40" : "border-border",
        className,
      )}
      {...props}
    >
      <div className="min-w-0 flex-1">
        <ToastTitle
          className={cn(
            "text-sm font-medium",
            tone === "error" ? "text-destructive" : "text-foreground",
          )}
        >
          {title}
        </ToastTitle>
        {detail ? (
          <ToastDescription className="mt-1 line-clamp-4 text-xs leading-5 break-words text-muted-foreground">
            {detail}
          </ToastDescription>
        ) : null}
      </div>
      <ToastClose
        aria-label="关闭提示"
        className="inline-flex size-6 shrink-0 items-center justify-center rounded-sm text-muted-foreground outline-none transition-colors hover:bg-accent hover:text-foreground active:bg-fill-3 focus-visible:ring-2 focus-visible:ring-ring/45"
      >
        ×
      </ToastClose>
    </ToastRoot>
  );
}
