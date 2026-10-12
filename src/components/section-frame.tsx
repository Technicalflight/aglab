import type { ReactNode } from "react";
import { ContentColumn } from "@/components/ui/content-column";
import { cn } from "@/lib/utils";

/**
 * 话题之外的分区共用这套栏头 + 居中内容宽度，保持和聊天列一致的骨架。
 *
 * 默认自带卡片外框（圆角 + 细边 + 阴影 + 圆角裁剪）——分区之间是画布上的
 * 一块块卡片，这是全应用统一的层次语言。被嵌进设置页那张大卡片里时要传
 * `framed={false}`：那种场合外框已经由设置内容区提供，再套一层就是卡片套卡片。
 */
export function SectionFrame({
  title,
  note,
  actions,
  framed = true,
  children,
}: {
  title: string;
  note?: ReactNode;
  actions?: ReactNode;
  framed?: boolean;
  children: ReactNode;
}) {
  return (
    // min-h-0：与 chat-area 同理，缺了它内容一长就会把整区撑出窗口
    <section
      className={cn(
        "flex min-h-0 min-w-0 flex-1 flex-col",
        framed && "overflow-hidden rounded-lg border border-border bg-surface shadow-md",
      )}
    >
      <header className="flex h-12 shrink-0 items-center gap-3 border-b border-border px-6">
        <h1 className="shrink-0 text-base font-medium text-foreground">{title}</h1>
        {note ? (
          <p className="min-w-0 flex-1 truncate text-xs text-muted-foreground">{note}</p>
        ) : (
          <span className="flex-1" />
        )}
        {actions}
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto">
        <ContentColumn>{children}</ContentColumn>
      </div>
    </section>
  );
}
