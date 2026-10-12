import type * as React from "react";
import { Popover as PopoverPrimitive } from "radix-ui";

import { cn } from "@/lib/utils";

function Popover(props: React.ComponentProps<typeof PopoverPrimitive.Root>) {
  return <PopoverPrimitive.Root data-slot="popover" {...props} />;
}

function PopoverTrigger({
  className,
  ...props
}: React.ComponentProps<typeof PopoverPrimitive.Trigger>) {
  return (
    <PopoverPrimitive.Trigger
      data-slot="popover-trigger"
      className={cn("outline-none", className)}
      {...props}
    />
  );
}

/**
 * 锚点（不接管点击）。悬停即开、点击只是“确保开着”的那类浮层用它：
 * 触发器的点击会 toggle——悬停已经开着时再点一下反而会关上。
 */
function PopoverAnchor(props: React.ComponentProps<typeof PopoverPrimitive.Anchor>) {
  return <PopoverPrimitive.Anchor data-slot="popover-anchor" {...props} />;
}

/**
 * 浮层。宽度从固定 w-72 改成 min(288px, 92vw)——模型选择器、模式选择器
 * 都走这里，窄窗口下固定宽会直接顶出视口。
 * 补退出动画：原来只有进入，浮层消失是瞬时的，扫视时像闪烁。
 */
function PopoverContent({
  className,
  align = "start",
  side = "top",
  sideOffset = 8,
  ...props
}: React.ComponentProps<typeof PopoverPrimitive.Content>) {
  return (
    <PopoverPrimitive.Portal>
      <PopoverPrimitive.Content
        data-slot="popover-content"
        align={align}
        side={side}
        sideOffset={sideOffset}
        className={cn(
          "z-modal w-[min(288px,92vw)] origin-bottom rounded-lg border border-border bg-elevated p-3 shadow-md outline-none",
          "data-[state=open]:animate-picker-in data-[state=closed]:animate-picker-out",
          className,
        )}
        {...props}
      />
    </PopoverPrimitive.Portal>
  );
}

export { Popover, PopoverAnchor, PopoverContent, PopoverTrigger };
