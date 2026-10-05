import type { TaskGraph, TaskNode } from "../types/chat";

/** 任务图编辑器的纯函数。它们住在这里而不是界面文件里，是为了**能被真的跑一次**：
 *  这个项目没有 JS 测试框架（前端验证 = tsc + Rust + 手点），而 `import type` 会被
 *  类型剥离擦掉，所以 `node --experimental-strip-types` 能直接载这个文件把下面几条判据
 *  跑在**发出去的那一份代码**上，不是跑在一份抄件上 */

/** 新格子占一个不撞车的 id：撞了就是后端那句「节点 id 重复了」，与其让用户撞了再改，
 *  不如一开始就给个没人用的 */
export function emptyNode(taken: string[]): TaskNode {
  let index = 1;
  while (taken.includes(`step-${index}`)) index += 1;
  return { id: `step-${index}`, prompt: "", dependsOn: [], allowedTools: [], subagent: null };
}

/** 改一格只能在那一份格子上覆盖：整格替换会把 `subagent` 一起抹掉，
 *  而它是只在 config.json 里写的那半份定义——抹掉了没有任何地方会说出来 */
export function patchNode(graph: TaskGraph, at: number, changes: Partial<TaskNode>): TaskGraph {
  return {
    ...graph,
    nodes: graph.nodes.map((node, index) => (index === at ? { ...node, ...changes } : node)),
  };
}

/** 删掉一格时，把别人对它的引用一起摘掉：留一根断边只会变成"这条任务存不下去了"，
 *  而用户刚做的事只是删了一格 */
export function withoutNode(graph: TaskGraph, at: number): TaskGraph {
  const gone = graph.nodes[at]?.id ?? "";
  return {
    ...graph,
    nodes: graph.nodes
      .filter((_, index) => index !== at)
      .map((node) => ({ ...node, dependsOn: node.dependsOn.filter((item) => item !== gone) })),
  };
}

export function toggleDepends(graph: TaskGraph, at: number, id: string): TaskGraph {
  const node = graph.nodes[at];
  if (!node) return graph;
  return patchNode(graph, at, {
    dependsOn: node.dependsOn.includes(id)
      ? node.dependsOn.filter((item) => item !== id)
      : [...node.dependsOn, id],
  });
}

/** 存之前把两边都收一遍：id 与 dependsOn 必须按同一套规则剪空白，
 *  否则" a" 与 "a" 在后端那道判据眼里是两格，那条边就成了界面自己造出来的断边。
 *
 *  工具名单这一格额外**按逗号再切一遍**：界面上那个输入框本来就是逗号分隔，
 *  但一张手写于 `config.json` 的图能把 `"read_file, write_file"` 当成**一个**工具名存进来——
 *  那一格于是永远匹配不到任何工具，表现为"这一格什么工具都用不了"，而界面上看着两个都在。
 *  工具 id 里不会含逗号，所以切它只会修好，不会改坏任何合法值 */
export function tidyGraph(graph: TaskGraph): TaskGraph {
  return {
    ...graph,
    nodes: graph.nodes.map((node) => ({
      ...node,
      id: node.id.trim(),
      dependsOn: node.dependsOn.map((item) => item.trim()).filter(Boolean),
      allowedTools: node.allowedTools
        .flatMap((item) => item.split(","))
        .map((item) => item.trim())
        .filter(Boolean),
    })),
  };
}
