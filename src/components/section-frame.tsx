import type { ReactNode } from "react";
import { ContentColumn } from "@/components/ui/content-column";

/** 话题之外的分区共用这套栏头 + 居中内容宽度，保持和聊天列一致的骨架 */
export function SectionFrame({
  title,
  note,
  actions,
  children,
}: {
  title: string;
  note?: ReactNode;
  actions?: ReactNode;
  children: ReactNode;
}) {
  return (
    // min-h-0：与 chat-area 同理，缺了它内容一长就会把整区撑出窗口
    <section className="flex min-h-0 min-w-0 flex-1 flex-col bg-background">
      <header className="flex h-12 shrink-0 items-center gap-3 border-b border-border px-6">
        <h1 className="shrink-0 text-base font-medium text-foreground">{title}</h1>
        {note ? <p className="min-w-0 flex-1 truncate text-xs text-muted-foreground">{note}</p> : <span className="flex-1" />}
        {actions}
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto">
        <ContentColumn>{children}</ContentColumn>
      </div>
    </section>
  );
}
