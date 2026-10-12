import type { PanelTab } from "@/types/chat";

/**
 * 命令中心（O7-1）的数据层。组件（search-dialog.tsx）只消费这三个纯函数——
 * 过滤与合并的规则住在这里，才能在不挂组件的测试里钉住行为。
 *
 * 动作（run）由调用方注入：这份清单与左轨（sidebar-rail.tsx）同一套动作源，
 * 但它不 import store——纯数据进纯函数，动作在组件里组装。
 */

export type CommandItem = {
  key: string;
  label: string;
  hint: string;
  run: () => void;
};

export type TopicRow = {
  id: string;
  title: string;
  preview: string;
  project: string;
};

export type PaletteRow =
  | { kind: "command"; command: CommandItem }
  | {
      kind: "topic";
      id: string;
      title: string;
      project: string;
      /** 前 9 条话题带 Ctrl+1..9 直达徽标（命令不占号） */
      badge?: string;
    };

export const PANEL_COMMANDS: ReadonlyArray<{
  tab: PanelTab;
  label: string;
}> = [
  { tab: "preview", label: "预览" },
  { tab: "terminal", label: "终端" },
  { tab: "browser", label: "浏览器" },
];

export const SECTION_COMMANDS: ReadonlyArray<{
  section: Exclude<import("@/types/chat").SidebarSection, "chats">;
  label: string;
}> = [
  { section: "review", label: "变更请求" },
  { section: "tasks", label: "定时任务" },
  { section: "knowledge", label: "资料库" },
  { section: "agents", label: "助手库" },
  { section: "plugins", label: "插件" },
  { section: "settings", label: "设置" },
];

export function filterCommands<T extends CommandItem>(commands: T[], keyword: string): T[] {
  const lower = keyword.trim().toLowerCase();
  if (!lower) return commands;
  return commands.filter(
    (item) => item.label.toLowerCase().includes(lower) || item.hint.toLowerCase().includes(lower),
  );
}

export function filterTopics(topics: TopicRow[], keyword: string): TopicRow[] {
  const lower = keyword.trim().toLowerCase();
  if (!lower) return topics;
  return topics.filter(
    (item) =>
      item.title.toLowerCase().includes(lower) ||
      item.preview.toLowerCase().includes(lower) ||
      item.project.toLowerCase().includes(lower),
  );
}

/** 命令与话题排进同一条可导航的列表：光标与 Enter 只认这一条序列 */
export function mergeRows(commands: CommandItem[], topics: TopicRow[]): PaletteRow[] {
  return [
    ...commands.map((command): PaletteRow => ({ kind: "command", command })),
    ...topics.map(
      (item, index): PaletteRow => ({
        kind: "topic",
        id: item.id,
        title: item.title,
        project: item.project,
        badge: index < 9 ? `Ctrl+${index + 1}` : undefined,
      }),
    ),
  ];
}
