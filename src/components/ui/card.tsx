import type * as React from "react";

import { cn } from "@/lib/utils";

/**
 * 卡片 / 面板。承载"一组相关内容的边界"，用 surface + 12px 圆角 + 细边。
 *
 * 刻意不给默认阴影：桌面端大量卡片并排时，阴影会互相打架，
 * 靠"底色差一档 + 一根细边"分层更干净（对齐 LobeHub 的做法）。
 * 需要抬起来的时候（浮层、粘性头）才用 interactive 变体给 shadow-md。
 */
function Card({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="card"
      className={cn(
        "flex flex-col rounded-lg border border-border bg-surface text-foreground",
        className,
      )}
      {...props}
    />
  );
}

/** 可点开的卡片：加 hover 底色与按下反馈，语义仍是 div（由调用方补 role） */
function CardInteractive({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="card-interactive"
      className={cn(
        "flex cursor-pointer flex-col rounded-lg border border-border bg-surface text-foreground outline-none transition-[background-color,border-color,transform] duration-[var(--dur-fast)]",
        "hover:border-brand/30 hover:bg-surface-secondary active:scale-[0.995]",
        "focus-visible:ring-2 focus-visible:ring-ring/45",
        className,
      )}
      {...props}
    />
  );
}

function CardHeader({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="card-header"
      className={cn("flex flex-col gap-1 px-4 pt-3.5 pb-2.5", className)}
      {...props}
    />
  );
}

function CardTitle({ className, ...props }: React.ComponentProps<"h3">) {
  return (
    <h3
      data-slot="card-title"
      className={cn("text-base font-semibold tracking-tight", className)}
      {...props}
    />
  );
}

function CardDescription({ className, ...props }: React.ComponentProps<"p">) {
  return (
    <p
      data-slot="card-description"
      className={cn("text-xs leading-5 text-foreground-tertiary", className)}
      {...props}
    />
  );
}

function CardContent({ className, ...props }: React.ComponentProps<"div">) {
  return <div data-slot="card-content" className={cn("px-4 pb-4", className)} {...props} />;
}

function CardFooter({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="card-footer"
      className={cn("flex items-center gap-2 border-t border-border-subtle px-4 py-2.5", className)}
      {...props}
    />
  );
}

export { Card, CardContent, CardDescription, CardFooter, CardHeader, CardInteractive, CardTitle };
