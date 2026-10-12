import { type PanelTab } from "@/types/chat";

/**
 * 右栏工具标签的状态机（0dc41ca 落的多标签模型，从组件里抽出以便钉测）：
 * open 是打开顺序（**同类可重复**——多枚终端/多枚浏览器并存），activeId 是当前
 * 聚焦，picking = 正在挑新工具。两条入口语义刻意不同：
 * - 手动（开始页卡片 / 「+」挑工具面板）→ openTab：每次都开一枚新实例，满员拒绝；
 * - 外部导航（资源管理器点文件、决策面板等）→ focusTool：聚焦同类已有、没有才开，
 *   不受上限约束——「点了就得到」。
 */

export interface ToolTabInstance {
  id: string;
  tool: PanelTab;
}

export interface PanelTabsState {
  open: ToolTabInstance[];
  activeId: string | null;
  picking: boolean;
}

/** 标签数量上限：超出后手动开新标签会被拒（外部导航不受限，保证可达） */
export const MAX_OPEN_TABS = 10;

export function emptyTabs(): PanelTabsState {
  return { open: [], activeId: null, picking: false };
}

/** 手动开新：每次都开一枚新实例。满员时拒绝——状态原样返回，调用方负责提示 */
export function openTab(
  prev: PanelTabsState,
  tool: PanelTab,
  id: string,
): { state: PanelTabsState; ok: boolean } {
  if (prev.open.length >= MAX_OPEN_TABS) return { state: prev, ok: false };
  return {
    state: { open: [...prev.open, { id, tool }], activeId: id, picking: false },
    ok: true,
  };
}

/** 关闭：× 单独关一枚。关的是当前标签时焦点落回相邻一枚（位置不动，取 min(at, 末位)）；
 *  关到空了就回开始页 */
export function closeTab(prev: PanelTabsState, instanceId: string): PanelTabsState {
  const at = prev.open.findIndex((instance) => instance.id === instanceId);
  if (at === -1) return prev;
  const open = prev.open.filter((instance) => instance.id !== instanceId);
  const wasActive = prev.activeId === instanceId;
  const fallback = open[Math.min(at, open.length - 1)] ?? null;
  return {
    open,
    activeId: open.length === 0 ? null : wasActive ? fallback.id : prev.activeId,
    picking: false,
  };
}

/** 外部导航：聚焦同类已有标签（多枚同类取最早那枚）、没有才开新。
 *  已经停在同工具的某枚上时**保持现状不重定位**——手动新开的那枚紧跟在
 *  setPanelTab 后会走到这里，没有这道短路新实例会瞬间被顶回第一枚 */
export function focusTool(prev: PanelTabsState, tool: PanelTab, id: string): PanelTabsState {
  const current = prev.open.find((instance) => instance.id === prev.activeId);
  if (current?.tool === tool) return { ...prev, picking: false };
  const existing = prev.open.find((instance) => instance.tool === tool);
  if (existing) return { ...prev, activeId: existing.id, picking: false };
  return { open: [...prev.open, { id, tool }], activeId: id, picking: false };
}
