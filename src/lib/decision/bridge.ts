/**
 * 决策桥的前端那一半（§7.3 / §7.5 的生产接线）。
 *
 * Rust 侧的编排派工与记忆注入排序跑在后台线程上，它们拿不准时会 emit
 * `decision://ask` 过来；这里把问题派给决策层的嵌入函数，再把答案 invoke 回
 * `decision_bridge_answer`。三条合同与 integrations.ts 同源：
 * 1. 每一问都有回话——决策层说 no 也回 null，Rust 侧不至于干等超时；
 * 2. 任何异常都折叠成 null 应答，绝不把桥的故障砸到调用方脸上；
 * 3. 监听装一次就够（App 挂载时），重复调用是幂等的。
 */
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

import { createIntegrations } from "./integrations";
import { getDecisionSystem } from "./index";
import type { DecisionIntegrations } from "./integrations";

export const DECISION_BRIDGE_EVENT = "decision://ask";

export interface DecisionBridgeAsk {
  id: string;
  method: string;
  payload: unknown;
}

/** 纯派发（测试注入点）：一个方法名对应决策层的一个嵌入函数。
 *  只回可序列化的结论本体——response 对象住在审计里，不过桥 */
export async function handleBridgeAsk(
  integrations: DecisionIntegrations,
  method: string,
  payload: unknown,
): Promise<unknown> {
  const source = (payload ?? {}) as Record<string, unknown>;
  if (method === "assignAgent") {
    const verdict = await integrations.assignAgent(
      source.task as { goal: string; type?: string },
      (source.agents ?? []) as ReadonlyArray<{ role: string; description: string }>,
    );
    return verdict
      ? { agent: verdict.agent, priority: verdict.priority, canParallel: verdict.canParallel }
      : null;
  }
  if (method === "scoreContextRelevance") {
    const verdict = await integrations.scoreContextRelevance(
      source.query as string,
      (source.candidates ?? []) as ReadonlyArray<{ id: number; text: string }>,
    );
    return verdict ? { scores: verdict.scores } : null;
  }
  // 认不出的方法名也是一句 null：桥的两端版本错位时，Rust 侧照旧走原路
  return null;
}

/** 装一次听一辈子。返回停听函数（App 卸载用不上它，但测试与热重载用得上） */
export function installDecisionBridge(): () => void {
  let stopped = false;
  let unlisten: UnlistenFn | null = null;
  const promise = listen<DecisionBridgeAsk>(DECISION_BRIDGE_EVENT, (event) => {
    const { id, method, payload } = event.payload;
    const integrations = createIntegrations(getDecisionSystem());
    handleBridgeAsk(integrations, method, payload)
      .then((answer) => invoke("decision_bridge_answer", { id, answer }))
      .catch(() =>
        // 决策层炸了也要回话：让 Rust 侧立刻拿到 null，而不是陪它等超时
        invoke("decision_bridge_answer", { id, answer: null }).catch(() => undefined),
      );
  });
  void promise
    .then((fn) => {
      if (stopped) {
        fn();
      } else {
        unlisten = fn;
      }
    })
    .catch(() => undefined);
  return () => {
    stopped = true;
    unlisten?.();
  };
}
