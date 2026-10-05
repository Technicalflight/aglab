import type * as React from "react";
import { Dialog as DialogPrimitive } from "radix-ui";
import { IconX as X } from "@tabler/icons-react";

import { cn } from "@/lib/utils";

function Dialog(props: React.ComponentProps<typeof DialogPrimitive.Root>) {
  return <DialogPrimitive.Root data-slot="dialog" {...props} />;
}

/**
 * 弹窗。三处刻意改动：
 *
 * 1. 默认宽度用 min(420px, 92vw) 而不是 w-[420px] + max-w-[90vw]：
 *    一句话同时表达"理想 420px"与"绝不超视口"，不必靠两条约束互相拉扯。
 *    调用方仍可用 className 覆写（w-[560px] 之类照样生效）。
 * 2. 补退出动画：只有进入没有退出的弹窗会"啪"地消失，读起来像出错。
 * 3. 补 max-h + 内部滚动：长表单弹窗在矮窗口下必须能滚，否则底部按钮点不到。
 */
function DialogContent({
  className,
  children,
  ...props
}: React.ComponentProps<typeof DialogPrimitive.Content>) {
  return (
    <DialogPrimitive.Portal>
      <DialogPrimitive.Overlay className="fixed inset-0 z-overlay bg-black/55 backdrop-blur-[2px] data-[state=open]:animate-overlay-in data-[state=closed]:animate-overlay-out" />
      <DialogPrimitive.Content
        data-slot="dialog-content"
        className={cn(
          "fixed top-1/2 left-1/2 z-modal grid max-h-[88vh] w-[min(420px,92vw)] -translate-x-1/2 -translate-y-1/2 gap-4 overflow-y-auto overscroll-contain rounded-xl border border-border bg-elevated p-5 shadow-lg outline-none",
          "data-[state=open]:animate-picker-in data-[state=closed]:animate-picker-out",
          className,
        )}
        {...props}
      >
        {children}
      </DialogPrimitive.Content>
    </DialogPrimitive.Portal>
  );
}

/** 标题区。留出右侧 pr-7 给关闭钮，否则长标题会撞上 × */
function DialogHeader({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="dialog-header"
      className={cn("flex flex-col gap-1 pr-7", className)}
      {...props}
    />
  );
}

function DialogTitle({ className, ...props }: React.ComponentProps<typeof DialogPrimitive.Title>) {
  return (
    <DialogPrimitive.Title
      className={cn("text-lg font-semibold tracking-tight", className)}
      {...props}
    />
  );
}

function DialogDescription({
  className,
  ...props
}: React.ComponentProps<typeof DialogPrimitive.Description>) {
  return (
    <DialogPrimitive.Description
      className={cn("text-xs leading-5 text-foreground-secondary", className)}
      {...props}
    />
  );
}

function DialogFooter({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="dialog-footer"
      className={cn("flex items-center justify-end gap-2 pt-1", className)}
      {...props}
    />
  );
}

/** 右上角关闭钮。绝对定位，不占内容宽度 */
function DialogClose({
  className,
  children,
  ...props
}: React.ComponentProps<typeof DialogPrimitive.Close>) {
  return (
    <DialogPrimitive.Close
      data-slot="dialog-close"
      aria-label="关闭"
      className={cn(
        "absolute top-3.5 right-3.5 inline-flex size-7 items-center justify-center rounded-sm text-foreground-tertiary outline-none transition-colors hover:bg-accent hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/55",
        className,
      )}
      {...props}
    >
      {children ?? <X className="size-3.5" />}
    </DialogPrimitive.Close>
  );
}

export {
  Dialog,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
};
