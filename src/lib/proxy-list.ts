/**
 * 代理列表这一格特有的两件事：一屏多少条，以及"整页勾选/取消"怎么合并。
 *
 * 分页的公共数学住在 `pagination.ts`（价格表也用那份），这里不放——放两份就会有一处
 * 在某个边界上悄悄和另一处不一致。
 */

/** 一屏多少条代理：一行两格（控件 + 实时读数）还带着 3 秒心跳，导入一百条时不能全渲染 */
export const PROXY_PAGE_SIZE = 10;

/** 全选 / 取消全选一整页：合并去重，且不动调用方那份数组 */
export function mergeSelection(selected: string[], ids: string[], on: boolean): string[] {
  const next = new Set(selected);
  for (const id of ids) {
    if (on) next.add(id);
    else next.delete(id);
  }
  return [...next];
}

/** 这一页是不是已经整页选中（表头那个勾选框的亮法）。空页不算"全选" */
export function allSelected(selected: string[], ids: string[]): boolean {
  return ids.length > 0 && ids.every((id) => selected.includes(id));
}
