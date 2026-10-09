import * as React from "react";
import { Slot } from "radix-ui";
import { IconLoader2 as Loader2 } from "@tabler/icons-react";

import { cn } from "@/lib/utils";

/**
 * 按钮是全应用被按得最多的控件，所以状态做全：hover / focus-visible / active /
 * disabled / loading 一个不缺。
 *
 * 三条约定：
 * 1. active 必须有——按下瞬间的 1px 缩放是"真的被按下去了"的唯一证据，
 *    缺了它，触屏与鼠标用户都只能靠"松手后界面变了"来推断。
 * 2. loading 走 aria-busy 而不是 disabled 的语义——读屏软件仍会念出"按钮"，
 *    不会突然变成一个灰掉的未知元素。
 * 3. 焦点环用 ring 而非 outline：ring 走 box-shadow，不参与布局，
 *    不会像 outline 那样把相邻内容挤动造成重排。
 */
const BUTTON_BASE =
  "inline-flex shrink-0 items-center justify-center gap-2 whitespace-nowrap rounded-md text-base font-medium select-none outline-none transition-[color,background-color,border-color,box-shadow,transform] duration-[var(--dur-fast)] active:scale-[0.98] disabled:pointer-events-none disabled:opacity-45 [&_svg]:pointer-events-none [&_svg]:shrink-0 focus-visible:ring-2 focus-visible:ring-ring/55";

const BUTTON_VARIANTS = {
  default: "bg-primary text-primary-foreground hover:bg-brand-hover active:bg-brand-active",
  subtle: "border border-border bg-surface text-foreground hover:bg-accent active:bg-fill-3",
  ghost:
    "text-muted-foreground hover:bg-accent hover:text-foreground active:bg-fill-3 aria-pressed:bg-accent aria-pressed:text-foreground",
  outline: "border border-input bg-transparent text-foreground hover:bg-accent active:bg-fill-3",
  brand:
    "bg-brand text-brand-foreground hover:bg-brand-hover active:bg-brand-active disabled:bg-elevated disabled:text-muted-foreground disabled:opacity-100",
  /* 危险操作：仅用于"删除 / 丢弃 / 清空"这类不可逆动作。
     与 brand 同为实心、靠色相区分，降低误触。 */
  destructive:
    "bg-destructive text-destructive-foreground hover:brightness-110 active:brightness-95",
} as const;

const BUTTON_SIZES = {
  default: "h-9 px-4 has-[>svg]:px-3.5",
  sm: "h-8 px-3 text-sm has-[>svg]:px-2.5",
  lg: "h-10 px-5 has-[>svg]:px-4",
  icon: "size-9",
  "icon-sm": "size-8",
  /* 贴底排的图标条（消息操作、工具卡审批）：视觉更低矮，命中区仍 32px */
  "icon-xs": "size-7 rounded-sm",
} as const;

export type ButtonVariant = keyof typeof BUTTON_VARIANTS;
export type ButtonSize = keyof typeof BUTTON_SIZES;

function buttonVariants({
  variant = "default",
  size = "default",
  className,
}: {
  variant?: ButtonVariant;
  size?: ButtonSize;
  className?: string;
}) {
  return cn(BUTTON_BASE, BUTTON_VARIANTS[variant], BUTTON_SIZES[size], className);
}

function Button({
  className,
  variant,
  size,
  asChild = false,
  loading = false,
  children,
  disabled,
  ...props
}: React.ComponentProps<"button"> & {
  variant?: ButtonVariant;
  size?: ButtonSize;
  asChild?: boolean;
  /** 加载中：转圈 + 屏蔽点击。不复用 disabled 语义，读屏仍念作"按钮" */
  loading?: boolean;
}) {
  const Comp = asChild ? Slot.Root : "button";

  return (
    <Comp
      data-slot="button"
      aria-busy={loading || undefined}
      data-loading={loading || undefined}
      className={buttonVariants({ variant, size, className })}
      // asChild 时元素归调用方所有，不能塞 disabled 进去，由调用方自己处理
      disabled={asChild ? undefined : disabled || loading}
      {...props}
    >
      {loading ? (
        <>
          <Loader2 className="size-3.5 animate-spin-slow" aria-hidden />
          {children}
        </>
      ) : (
        children
      )}
    </Comp>
  );
}

export { Button, buttonVariants };
