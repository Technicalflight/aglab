/**
 * 话题树的算术：一份节点集合 + 一个末端 → 看得见的那条路径。
 *
 * 为什么单独立一个文件：这些判定既要给界面用（渲染哪几条、切换器显示第几支），
 * 也要能脱离 React 被逐条钉住。留在组件里就只能靠肉眼读截图，而"父链成环"、
 * "tip 指着一个不存在的节点"这类形状，恰好在截图上看不出来。
 *
 * 一条规矩：**前端不自造父子关系**。parentId 是从后端 `conversation_tree` 抄来的
 * （后端的父指针在追加时铸造），这里只做投影，不做编辑——两份真相就是这么来的。
 */

export type TreeLike = { id: string; parentId?: string | null };

/** 每个节点的**生效父**。
 *
 * 三种值是三件事，不能并成一种：
 * - 给了 id：就是那个父的儿子（分支就是这么长出来的）
 * - 给了 `null`：这一条真的是一条根（第一条消息、以及"编辑第一条"另起的新根）
 * - 整个字段缺省（`undefined`）：**没说**——旧存档、以及只是接在尾巴上的过渡气泡
 *   （压缩中的占位、记忆命令的回执、插话与跟随的那句）。缺省就按插入序认在前一条之后，
 *   于是这些点不用每一处都记得盖章，也不会把链走到一半断掉
 */
export function parentIds<T extends TreeLike>(nodes: T[]): Map<string, string | null> {
  const map = new Map<string, string | null>();
  nodes.forEach((node, index) => {
    // 判"有没有这个属性"，不能用 ??：`null` 是显式的根，而 `??` 会把它一起当成没给
    map.set(
      node.id,
      node.parentId === undefined ? (index === 0 ? null : nodes[index - 1].id) : node.parentId,
    );
  });
  return map;
}

/** 从末端沿父链走到根再反过来 = 当前分支。
 *
 * 四种情况一律退回"数组顺序就是那条链"，而不是渲染出一片空白：
 * tip 为空（还没选过分支）、整份集合没有 parentId（分支树之前的旧归档）、
 * tip 指着一个不在集合里的 id（归档比日志旧）、父链成环（文件被改坏）
 */
export function pathOf<T extends TreeLike>(nodes: T[], tip: string | null): T[] {
  if (!tip) return nodes;
  // 整份集合一个 parentId 都没写 = 分支树之前的旧归档：插入序就是那条链。
  // 这条判据必须放在集合层面——放在单个节点上就分不开"旧数据没记父"和
  // "末端真的就是一个根节点"，后者会被错读成"这条话题只剩一句话"
  if (!nodes.some((node) => node.parentId)) return nodes;
  const byId = new Map(nodes.map((node) => [node.id, node]));
  const parents = parentIds(nodes);
  const walked: T[] = [];
  const seen = new Set<string>();
  let cursor: string | null | undefined = tip;
  while (cursor) {
    const node = byId.get(cursor);
    if (!node || seen.has(node.id)) {
      // 断链或成环：宁可退回插入序，也不把用户的对话读成半截
      return nodes;
    }
    seen.add(node.id);
    walked.push(node);
    cursor = parents.get(node.id) ?? null;
  }
  walked.reverse();
  return walked;
}

/** 同一个生效父下面的全部节点（含自己），按插入序 */
export function siblingsOf<T extends TreeLike>(nodes: T[], id: string): T[] {
  const parents = parentIds(nodes);
  const parent = parents.get(id);
  if (parent === undefined) return [];
  return nodes.filter((node) => parents.get(node.id) === parent);
}

/** 换到相邻的一条兄弟，返回它的 id；没有可换的返回 null */
export function nextSiblingId<T extends TreeLike>(nodes: T[], id: string, delta: number): string | null {
  const group = siblingsOf(nodes, id);
  if (group.length < 2) return null;
  const at = group.findIndex((node) => node.id === id);
  const target = group[at + delta];
  return target ? target.id : null;
}

/** 当前路径上每个节点的"第几支 / 共几支"。不在路径上的节点不进这张表 */
export function branchPositions<T extends TreeLike>(
  nodes: T[],
  tip: string | null,
): Map<string, { index: number; total: number }> {
  const positions = new Map<string, { index: number; total: number }>();
  for (const node of pathOf(nodes, tip)) {
    const group = siblingsOf(nodes, node.id);
    if (group.length < 2) continue;
    positions.set(node.id, {
      index: group.findIndex((item) => item.id === node.id) + 1,
      total: group.length,
    });
  }
  return positions;
}

/** 分叉点：当前路径上那些有兄弟的节点。轨道角标只在这些位置画 */
export function forkPoints<T extends TreeLike>(nodes: T[], tip: string | null): Set<string> {
  return new Set(branchPositions(nodes, tip).keys());
}

/** 站到某一支的"最深处"：从它出发反复取最后插入的那个孩子。
 *
 * 切分支要落的不是那一条气泡本身，而是它下面那条完整的对话——停在中间等于把
 * 那一支的后半截藏起来，而用户点的就是"我要看这一支说到哪"
 */
export function branchTail<T extends TreeLike>(nodes: T[], id: string): string | null {
  if (!nodes.some((node) => node.id === id)) return null;
  const parents = parentIds(nodes);
  let cursor = id;
  // 每次取最后一个孩子；父链成环的存档在这里靠 seen 挡住，不会转出去
  const seen = new Set<string>([id]);
  for (;;) {
    const child = nodes.filter((node) => parents.get(node.id) === cursor).pop();
    if (!child || seen.has(child.id)) return cursor;
    seen.add(child.id);
    cursor = child.id;
  }
}

/** 按末端把节点集合切成"看得见的那条"与"其余分支"。
 *
 * 界面状态里两份都要留着——只留看得见的那条，就是今天"重新生成把旧答案从归档里
 * 抹掉、后端日志却还留着"的那份双轨真相
 */
export function partition<T extends TreeLike>(
  nodes: T[],
  tip: string | null,
): { thread: T[]; offPath: T[] } {
  const thread = pathOf(nodes, tip);
  const onThread = new Set(thread.map((node) => node.id));
  return { thread, offPath: nodes.filter((node) => !onThread.has(node.id)) };
}
