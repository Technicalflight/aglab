import * as React from "react";
import { Tabs as TabsPrimitive } from "radix-ui";

import { cn } from "@/lib/utils";

function Tabs({ className, ...props }: React.ComponentProps<typeof TabsPrimitive.Root>) {
  return (
    <TabsPrimitive.Root
      data-slot="tabs"
      className={cn("flex flex-col gap-2", className)}
      {...props}
    />
  );
}

/**
 * 标签栏底板。可横向滚动——标签多到装不下时（模型名、知识库分类），
 * 整条栏平移，而不是把标签挤成两行或裁掉。
 */
function TabsList({ className, ...props }: React.ComponentProps<typeof TabsPrimitive.List>) {
  return (
    <TabsPrimitive.List
      data-slot="tabs-list"
      className={cn(
        // p-1.5 与触发器的 py-1 配对：激活药丸到轨道边框留 6px（+1px 边框）呼吸位。
        // 曾用 p-1——药丸几乎顶满轨道，浅色主题下白药丸像要戳穿边框。
        "inline-flex max-w-full items-center gap-1 overflow-x-auto rounded-md border border-border bg-sidebar p-1.5",
        className,
      )}
      {...props}
    />
  );
}

/** 单个标签。disabled 态补上：原先标签不可禁用，灰字灰底反而更清楚 */
function TabsTrigger({ className, ...props }: React.ComponentProps<typeof TabsPrimitive.Trigger>) {
  return (
    <TabsPrimitive.Trigger
      data-slot="tabs-trigger"
      className={cn(
        // flex-1：右栏把 TabsList 拉成 w-full，触发器随之均分整条轨道
        // （分段控件的样子）——曾改成 shrink-0 导致标签左挤、轨道右侧留死白。
        // 压不下时 overflow-x-auto 的轨道仍可横向滚动，min-w-0 是收缩前提。
        // py-1 与轨道的 p-1.5 配对出 6px 呼吸位，见 TabsList 注释。
        "min-w-0 flex-1 rounded-sm px-2.5 py-1 text-base font-medium whitespace-nowrap text-foreground-tertiary outline-none transition-[color,background-color] duration-[var(--dur-fast)] hover:text-foreground data-[state=active]:bg-surface data-[state=active]:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45 disabled:pointer-events-none disabled:opacity-45",
        className,
      )}
      {...props}
    />
  );
}

function TabsContent({ className, ...props }: React.ComponentProps<typeof TabsPrimitive.Content>) {
  return (
    <TabsPrimitive.Content
      data-slot="tabs-content"
      className={cn("min-w-0 flex-1 outline-none", className)}
      {...props}
    />
  );
}

export { Tabs, TabsContent, TabsList, TabsTrigger };
