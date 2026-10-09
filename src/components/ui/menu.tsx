import type * as React from "react";
import { DropdownMenu as MenuPrimitive } from "radix-ui";

import { cn } from "@/lib/utils";

function Menu(props: React.ComponentProps<typeof MenuPrimitive.Root>) {
  return <MenuPrimitive.Root data-slot="menu" {...props} />;
}

function MenuTrigger(props: React.ComponentProps<typeof MenuPrimitive.Trigger>) {
  return <MenuPrimitive.Trigger data-slot="menu-trigger" {...props} />;
}

function MenuContent({
  className,
  align = "start",
  ...props
}: React.ComponentProps<typeof MenuPrimitive.Content>) {
  return (
    <MenuPrimitive.Portal>
      <MenuPrimitive.Content
        data-slot="menu-content"
        align={align}
        side="top"
        sideOffset={8}
        className={cn(
          "z-modal w-[min(288px,92vw)] rounded-lg border border-border bg-elevated p-1.5 shadow-md outline-none",
          "data-[state=open]:animate-picker-in data-[state=closed]:animate-picker-out",
          className,
        )}
        {...props}
      />
    </MenuPrimitive.Portal>
  );
}

function MenuLabel({ className, ...props }: React.ComponentProps<typeof MenuPrimitive.Label>) {
  return (
    <MenuPrimitive.Label
      className={cn(
        "px-2 pt-2 pb-1 text-2xs font-medium tracking-wide text-foreground-tertiary",
        className,
      )}
      {...props}
    />
  );
}

function MenuSeparator() {
  return <MenuPrimitive.Separator className="mx-2 my-1.5 h-px bg-border-subtle" />;
}

/**
 * 菜单项。焦点由 data-[highlighted] 表达（Radix 键盘导航会移动它），
 * 底色 + 文字提亮双重表达，不只靠颜色——色觉障碍下也分得清选中项。
 */
function MenuItem({ className, ...props }: React.ComponentProps<typeof MenuPrimitive.Item>) {
  return (
    <MenuPrimitive.Item
      className={cn(
        "flex cursor-pointer items-center gap-2.5 rounded-md px-2 py-2 text-base text-foreground outline-none select-none transition-colors duration-[var(--dur-fast)] data-[highlighted]:bg-accent data-[highlighted]:text-foreground data-[disabled]:pointer-events-none data-[disabled]:opacity-45",
        className,
      )}
      {...props}
    />
  );
}

/** 危险菜单项：与普通项同形但用 destructive 色，且排在分隔线之后 */
function MenuDangerItem({ className, ...props }: React.ComponentProps<typeof MenuPrimitive.Item>) {
  return (
    <MenuPrimitive.Item
      className={cn(
        "flex cursor-pointer items-center gap-2.5 rounded-md px-2 py-2 text-base text-destructive outline-none select-none transition-colors duration-[var(--dur-fast)] data-[highlighted]:bg-destructive-soft data-[disabled]:pointer-events-none data-[disabled]:opacity-45",
        className,
      )}
      {...props}
    />
  );
}

export { Menu, MenuContent, MenuDangerItem, MenuItem, MenuLabel, MenuSeparator, MenuTrigger };
