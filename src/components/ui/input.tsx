import type * as React from "react";

import { cn } from "@/lib/utils";

/**
 * 文本输入。原生 input 之上补齐四件事：
 * hover / focus-visible / disabled / invalid 四态，以及前置图标的占位补偿。
 *
 * 无障碍要点：焦点环同时给 border 变色与 ring，颜色变了但形状不变，
 * 纯色觉障碍用户也能看出焦点在哪（不只靠"变蓝了"）。
 */
function Input({ className, type, ...props }: React.ComponentProps<"input">) {
  return (
    <input
      type={type}
      data-slot="input"
      className={cn(
        "flex h-9 w-full min-w-0 rounded-md border border-input bg-background px-3 text-base text-foreground outline-none transition-[color,background-color,border-color,box-shadow] duration-[var(--dur-fast)]",
        "placeholder:text-foreground-tertiary",
        "hover:border-brand/35",
        "focus-visible:border-brand/60 focus-visible:ring-2 focus-visible:ring-ring/35",
        "disabled:cursor-not-allowed disabled:bg-fill-4 disabled:text-foreground-quaternary",
        "aria-[invalid=true]:border-destructive/70 aria-[invalid=true]:ring-2 aria-[invalid=true]:ring-destructive/25",
        "file:mr-3 file:rounded-sm file:border-0 file:bg-fill-2 file:px-2 file:py-1 file:text-xs file:text-foreground",
        className,
      )}
      {...props}
    />
  );
}

/**
 * 表单标签。刻意不用 <label htmlFor> 强制配对——本项目的输入框绝大多数
 * 由组件包出可见标签，htmlFor 需要全局唯一 id，维护成本高于收益。
 * 这里统一渲染成 <label> 元素包住控件，天然建立隐式关联，
 * 读屏软件照样能念出标签文本。
 */
function Label({ className, ...props }: React.ComponentProps<"label">) {
  return (
    <label
      data-slot="label"
      className={cn(
        "text-xs font-medium text-foreground-secondary select-none peer-disabled:opacity-50",
        className,
      )}
      {...props}
    />
  );
}

/** 必填星号。与 Label 配对使用，视觉在左、朗读顺序也在左 */
function RequiredMark() {
  return (
    <span className="text-destructive" aria-hidden>
      *
    </span>
  );
}

export { Input, Label, RequiredMark };
