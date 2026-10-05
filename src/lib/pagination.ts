/**
 * 列表分页的公共数学。
 *
 * 现在有两个消费者（代理池、价格表）都有一屏装不下的时候。"末页被删空要落在还在
 * 的那一页"、"页号越界不能返回空数组糊弄过去"这两件事只该有一个写法——各写一份，
 * 迟早有一份在某个边界上悄悄不一致。
 */

/** 共几页。空列表也算一页：分页器不该因为"没有东西"就报 0 页 */
export function pageCount(total: number, size: number): number {
  return Math.max(1, Math.ceil(Math.max(0, total) / Math.max(1, size)));
}

/** 页号钳在范围内 */
export function clampPage(page: number, total: number, size: number): number {
  return Math.min(Math.max(0, page), pageCount(total, size) - 1);
}

/** 这一页该显示的那些条。页号越界时按钳位后的页取 */
export function pageSlice<T>(items: T[], page: number, size: number): T[] {
  const start = clampPage(page, items.length, size) * size;
  return items.slice(start, start + size);
}
