import * as React from "react";
import { Tooltip as TooltipPrimitive } from "radix-ui";

import { cn } from "@/lib/utils";

/** 400ms 延迟：鼠标掠过时不会一路点亮提示，键盘聚焦则由 Radix 立即显示 */
function TooltipProvider({
  delayDuration = 400,
  ...props
}: React.ComponentProps<typeof TooltipPrimitive.Provider>) {
  return <TooltipPrimitive.Provider delayDuration={delayDuration} {...props} />;
}

function Tooltip({ ...props }: React.ComponentProps<typeof TooltipPrimitive.Root>) {
  return <TooltipPrimitive.Root data-slot="tooltip" {...props} />;
}

function TooltipTrigger({ ...props }: React.ComponentProps<typeof TooltipPrimitive.Trigger>) {
  return <TooltipPrimitive.Trigger data-slot="tooltip-trigger" {...props} />;
}

/**
 * 提示气泡。底色用 spotlight（比 elevated 更深/更深一层），
 * 这是 LobeHub 的 colorBgSpotlight 语义：tooltip 属于"最上层的一次性提示"，
 * 复用面板底色会让它和菜单、弹窗混成同一层。
 * 文字必须配 spotlight-foreground，不能取常规 foreground：
 * 浅色主题下 spotlight 刻意反色成深底，而那时 foreground 是近黑——
 * 深底配深字只有 1.24:1，就是"黑泡看不清字"那次回归的根因。
 */
function TooltipContent({
  className,
  sideOffset = 6,
  ...props
}: React.ComponentProps<typeof TooltipPrimitive.Content>) {
  return (
    <TooltipPrimitive.Portal>
      <TooltipPrimitive.Content
        data-slot="tooltip-content"
        sideOffset={sideOffset}
        className={cn(
          "z-modal w-fit max-w-[min(240px,80vw)] rounded-md bg-spotlight px-2.5 py-1.5 text-xs leading-5 text-spotlight-foreground shadow-md",
          "data-[state=delayed-open]:animate-picker-in data-[state=closed]:animate-picker-out",
          className,
        )}
        {...props}
      />
    </TooltipPrimitive.Portal>
  );
}

export { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger };
