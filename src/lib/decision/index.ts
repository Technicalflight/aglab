/**
 * 决策层出口。应用代码只 import 这一个文件：
 *   import { getDecisionRouter } from "@/lib/decision";
 *   const decision = await getDecisionRouter().decide({ state, questions, sensitivity });
 *
 * 装配规则：Jev 只在 config.jev.enabled 且有 apiKey 时进漏斗；
 * Laya 的 warmupOnStart 在装配时异步点火（失败安静吞掉——sidecar 没起来
 * 不该拖垮启动，每次 decide 自带失败处理）。
 */
import { DecisionCache } from "./cache";
import { DecisionAudit } from "./audit";
import { loadDecisionConfig, saveDecisionConfig } from "./config";
import type { ConfigStorage, DecisionLayerConfig } from "./config";
import { LayaProvider } from "./providers/laya";
import { buildJevChainHops, JevChainProvider } from "./providers/jev-chain";
import { NullFallbackProvider } from "./providers/fallback";
import { platformInvoke } from "./providers/invoke";
import { DecisionRouterImpl } from "./router";
import type { DecisionRouter } from "./types";

export * from "./types";
export * from "./errors";
export { DecisionCache, fnv1a, stableStringify } from "./cache";
export { DecisionAudit, redactRequest } from "./audit";
export {
  DEFAULT_DECISION_CONFIG,
  DECISION_CONFIG_STORAGE_KEY,
  loadDecisionConfig,
  saveDecisionConfig,
  mergeDecisionConfig,
} from "./config";
export type {
  DecisionLayerConfig,
  LayaConfig,
  JevConfig,
  JevEndpointConfig,
  RoutingConfig,
  DecisionCacheConfig,
  DecisionAuditConfig,
  IntegrationsConfig,
  CompactionConfig,
  StagedReviewConfig,
  RetrievalConfig,
} from "./config";
export * from "./constants";
export * from "./metrics";
export { DecisionRouterImpl, getMinConfidence } from "./router";
export * from "./stats";
export { DecisionMonitor, decisionMonitor } from "./monitor";
export { portOfEndpoint, startSidecar, stopSidecar } from "./sidecar";
export { LayaProvider } from "./providers/laya";
export type { LayaHealth } from "./providers/laya";
export { JevProvider, JEV_ENDPOINTS, JEV_URL_CASES, jevEndpointProblem } from "./providers/jev";
export type { JevVia } from "./providers/jev";
export {
  JevChainProvider,
  buildJevChainHops,
  shouldFallbackToNext,
  type JevChainHop,
  type JevChainBuildOptions,
} from "./providers/jev-chain";
export type { TauriInvoke } from "./providers/invoke";
export { NullFallbackProvider } from "./providers/fallback";

export interface DecisionSystem {
  config: DecisionLayerConfig;
  cache: DecisionCache | null;
  audit: DecisionAudit | null;
  router: DecisionRouter;
  /** 面板要读它的 /health：区分「sidecar 没起」和「起了但模型还在热身」 */
  laya: LayaProvider;
  /**
   * null = 按配置就没启用（没开、或链上没有任何一跳有钥匙）。
   * V2 起是降级链（§5.4）：按 config.jev.chain 顺序逐跳尝试，4xx fail-fast
   */
  jev: JevChainProvider | null;
}

/** 从一份配置装配整套决策系统。测试传自定义 config + 注入的 Provider 即可复用 */
export function assembleDecisionSystem(config: DecisionLayerConfig = loadDecisionConfig()): DecisionSystem {
  const audit = config.audit.enabled
    ? new DecisionAudit(config.audit.maxEntries)
    : null;
  const cache = config.cache.enabled ? new DecisionCache(config.cache.ttlMs, config.cache.maxEntries) : null;

  const laya = new LayaProvider({
    transport: config.laya.transport,
    sidecarEndpoint: config.laya.sidecarEndpoint,
    subfolder: config.laya.subfolder,
    timeoutMs: config.laya.timeoutMs,
  });
  // 降级链装配（§5.4）：每跳一把钥匙（apiKeys[via] → 全局 → keyring）。
  // 任一跳有钥匙就装配——链的可用性在运行时逐跳判定。
  // **主 via 永远是链的第一跳**：用户显式选的出站口不能被 chain 配置绕过——
  // 否则 via=custom / 配了决策池的老用户升级后，链会拿他的钥匙去打默认链里的
  // typesafe 官方服务商，401 fail-fast，看起来就是「设置页通了、面板不通」
  const chainOrder = [config.jev.via, ...config.jev.chain.filter((via) => via !== config.jev.via)];
  const jev =
    config.jev.enabled &&
    (config.jev.apiKey || config.jev.useKeyring || config.jev.endpoints.some((e) => e.apiKey) || Object.keys(config.jev.apiKeys).length > 0)
      ? new JevChainProvider(
          buildJevChainHops(chainOrder, {
            apiKey: config.jev.apiKey,
            apiKeys: config.jev.apiKeys,
            baseUrl: config.jev.baseUrl,
            endpoints: config.jev.endpoints,
            transport: config.jev.transport,
            useKeyring: config.jev.useKeyring,
            timeoutMs: config.jev.timeoutMs,
          }),
        )
      : null;
  assembledJev = jev;
  const fallback = new NullFallbackProvider();
  const router = new DecisionRouterImpl({ laya, jev, fallback, cache, audit, config });

  if (config.enabled && config.laya.enabled && config.laya.warmupOnStart) {
    // 异步预热，火不管：启动路径不允许被 1.7GB 的模型加载挡住
    void laya.warmup?.().catch(() => undefined);
  }
  if (config.jev.enabled && config.jev.useKeyring) {
    void refreshJevKeyringState();
  }
  return { config, cache, audit, router, laya, jev };
}

let system: DecisionSystem | null = null;
/** 装配出来的 Jev 链。密钥管理助手要往每一跳身上喂「keyring 有没有条目」这一个比特 */
let assembledJev: JevChainProvider | null = null;

/** 进程级单例。Phase 3 的集成点（路由/记忆/任务/安全）都从这里拿同一个路由器。
 * storage 只留给测试与第一次构造：浏览器里它是 undefined，走真 localStorage */
export function getDecisionSystem(storage?: ConfigStorage): DecisionSystem {
  if (!system) system = assembleDecisionSystem(loadDecisionConfig(storage));
  return system;
}

export function getDecisionRouter(): DecisionRouter {
  return getDecisionSystem().router;
}

/**
 * 丢掉单例：下一次 getDecisionSystem() 按存储里的配置重装。
 * 重建会换掉审计与缓存实例——历史不会跟着丢，那是 monitor.ts 的职责。
 */
export function resetDecisionSystem(): void {
  system = null;
  assembledJev = null;
}

/**
 * 让改动立即生效：保存 → 重建 → 把新系统交回调用方。
 * 设置页每一格都走这里，不留"改了但要重启才认"的暗坑（缓存里旧结果的代价最多 60 秒）
 */
export function applyDecisionConfig(
  next: DecisionLayerConfig,
  storage?: ConfigStorage,
): DecisionSystem {
  saveDecisionConfig(next, storage);
  resetDecisionSystem();
  return getDecisionSystem(storage);
}

/* ---- Jev 密钥管理（keyring 在 Rust 侧，密钥本体从不过 IPC）----
 * 三条都碰原生命令；命令缺席（旧二进制/纯 Node 测试）时安静返回，
 * 可用性维持原状——这些是运维动作，不该把异常砸到调用方脸上 */

/** 启动/写删后探一次：keyring 里有没有可用的 Jev 密钥。返回 null = 探测不了 */
export async function refreshJevKeyringState(): Promise<boolean | null> {
  const jev = assembledJev;
  if (!jev) return null;
  try {
    const invoke = await platformInvoke();
    const state = (await invoke("decision_jev_key_state", {})) as boolean;
    jev.setKeyringAvailable(state === true);
    return state === true;
  } catch {
    return null;
  }
}

export async function setJevKeyringKey(secret: string): Promise<void> {
  const invoke = await platformInvoke();
  await invoke("decision_jev_key_set", { secret });
  await refreshJevKeyringState();
}

export async function deleteJevKeyringKey(): Promise<void> {
  const invoke = await platformInvoke();
  await invoke("decision_jev_key_delete", {});
  await refreshJevKeyringState();
}
