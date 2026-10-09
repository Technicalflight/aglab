/**
 * 多 Provider 降级链（design-decision-layer-optimization.md §5.4，修 B7）。
 *
 * 裁定的语义（§3 不变量）：
 * - 402 / 429 / 5xx → 记日志，降级到下一跳
 * - 超时 / 网络错误（无状态码的不可用）→ 同样降级——草图漏掉的两类，B7 补上
 * - 其余 4xx（401/403/404…）→ **fail-fast**：链上换一家也救不了凭据/路径错误，
 *   聚合已尝试列表再抛，调用方按各自路径的保守默认兜底
 * - 每跳的超时在 JevProvider 内部生效（构造时给 DECISION_TIMEOUT_MS），链层不重复包
 *
 * 循环在 TS 侧，每跳指定不同 via，仍走 decision_jev_system_one 原生通道——
 * **不开新的出站通道**，出口名单照过。custom 是合法成员：用户自建的
 * systemone 兼容网关作为链上的一跳（endpoints 池的亲和逻辑在 JevProvider 内部）。
 */
import {
  BREAKER_COOLDOWN_MS,
  BREAKER_FAILURE_THRESHOLD,
  BREAKER_HALF_OPEN_PROBES,
  PROVIDER_FALLBACK_STATUSES,
} from "../constants";
import { DecisionError, DecisionTimeoutError, DecisionUnavailableError } from "../errors";
import { incrementMetric } from "../metrics";
import type { DecisionProvider, DecisionRequest, DecisionResponse } from "../types";
import { JEV_ENDPOINTS, JevProvider } from "./jev";

export interface JevChainHop {
  /** 跳的名字（人读的审计与聚合错误用）：内置厂商名或 "custom" */
  via: string;
  provider: DecisionProvider;
}

/**
 * 单跳断路器的状态（深度优化）：连续失败 N 次 → 冷却期直接跳过该跳，
 * 不再每发都付一次完整超时；冷却结束后放行探测量（半开态）——
 * 成功即闭合，失败重新计数。每跳独立，一跳熔断不牵连链上其他跳。
 */
interface BreakerState {
  failures: number;
  /** 冷却窗口的起点（null = 闭合态） */
  openedAt: number | null;
  /** 半开态已放行的探测数 */
  probes: number;
}

function breakerAllows(breaker: BreakerState, now: number): { allowed: boolean; cooling: boolean } {
  if (breaker.openedAt === null) return { allowed: true, cooling: false };
  if (now - breaker.openedAt >= BREAKER_COOLDOWN_MS) {
    // 冷却结束 → 半开：限量放行探测
    if (breaker.probes < BREAKER_HALF_OPEN_PROBES) {
      breaker.probes += 1;
      return { allowed: true, cooling: false };
    }
    return { allowed: false, cooling: true };
  }
  return { allowed: false, cooling: true };
}

function breakerOnSuccess(breaker: BreakerState): void {
  breaker.failures = 0;
  breaker.openedAt = null;
  breaker.probes = 0;
}

function breakerOnFailure(breaker: BreakerState, now: number): void {
  breaker.failures += 1;
  breaker.probes = 0; // 半开探测失败 → 重新进入冷却
  if (breaker.failures >= BREAKER_FAILURE_THRESHOLD) {
    breaker.openedAt = now;
  }
}

/**
 * 该不该降级到下一跳。返回 false = fail-fast。
 * 判定次序：超时类先看（它没有状态码）；不可用类再分有无状态码；
 * 意外错误形态（非 DecisionError）保守降级——链上多试一家总比整条链炸掉好，
 * 意外形态本身已经进了聚合错误列表，不会静默。
 */
export function shouldFallbackToNext(error: unknown): boolean {
  if (error instanceof DecisionTimeoutError) return true;
  if (error instanceof DecisionUnavailableError) {
    const { status } = error;
    if (status === undefined) return true; // 网络/协议层错误，无状态码
    if (status >= 500) return true;
    return (PROVIDER_FALLBACK_STATUSES as readonly number[]).includes(status);
  }
  return true;
}

/** 白名单之外的链成员（配置往返故意保留的前向兼容项）在构造时就被丢弃 */
function hopProviderFor(via: string, options: JevChainBuildOptions): JevProvider | null {
  const common = {
    apiKey: options.apiKeys[via] ?? options.apiKey,
    transport: options.transport,
    useKeyring: options.useKeyring,
    timeoutMs: options.timeoutMs,
  };
  if (via === "custom") {
    return new JevProvider({
      ...common,
      via: "custom",
      baseUrl: options.baseUrl,
      endpoints: options.endpoints,
    });
  }
  if (via in JEV_ENDPOINTS) {
    return new JevProvider({ ...common, via: via as keyof typeof JEV_ENDPOINTS });
  }
  return null;
}

export interface JevChainBuildOptions {
  apiKey: string;
  /** 链上各内置厂商自己的密钥（apiKeys[via] 优先于全局 apiKey） */
  apiKeys: Record<string, string>;
  baseUrl: string;
  endpoints: Array<{ name?: string; baseUrl?: string; apiKey?: string }>;
  transport: "direct" | "rust";
  useKeyring: boolean;
  timeoutMs: number;
}

/** 按配置顺序把链装配成一串跳。白名单外的成员在这里落地成「跳过」，不留后患 */
export function buildJevChainHops(
  chain: readonly string[],
  options: JevChainBuildOptions,
): JevChainHop[] {
  const hops: JevChainHop[] = [];
  const seen = new Set<string>();
  for (const via of chain) {
    if (seen.has(via)) continue;
    seen.add(via);
    const provider = hopProviderFor(via, options);
    if (provider) hops.push({ via, provider });
  }
  return hops;
}

export class JevChainProvider implements DecisionProvider {
  readonly name = "jev" as const;

  /** 每跳一个断路器（深度优化）：连续失败 3 次冷却 30s，半开放行 1 次探测 */
  private readonly breakers = new Map<string, BreakerState>();

  constructor(private readonly hops: JevChainHop[]) {}

  private breakerFor(via: string): BreakerState {
    let breaker = this.breakers.get(via);
    if (!breaker) {
      breaker = { failures: 0, openedAt: null, probes: 0 };
      this.breakers.set(via, breaker);
    }
    return breaker;
  }

  get isAvailable(): boolean {
    return this.hops.some((hop) => hop.provider.isAvailable);
  }

  /** 链的预热 = 探测第一跳（幂等且永不抛的合同由 JevProvider.warmup 兑现） */
  async warmup(): Promise<void> {
    const first = this.hops.find((hop) => hop.provider.isAvailable);
    await first?.provider.warmup?.();
  }

  /** keyring 探测结果要喂到每一跳（各跳的 isAvailable 都依赖这一个比特） */
  setKeyringAvailable(available: boolean): void {
    for (const hop of this.hops) {
      if (hop.provider instanceof JevProvider) hop.provider.setKeyringAvailable(available);
    }
  }

  get hopNames(): string[] {
    return this.hops.map((hop) => hop.via);
  }

  async decide(request: DecisionRequest): Promise<DecisionResponse> {
    if (!this.isAvailable) {
      throw new DecisionUnavailableError(
        this.hops.length === 0 ? "降级链上没有可用的跳" : "降级链上没有任何一跳有可用凭据",
      );
    }
    const attempted: string[] = [];
    const failures: string[] = [];
    const now = Date.now();
    for (const hop of this.hops) {
      if (!hop.provider.isAvailable) {
        failures.push(`${hop.via}: 无凭据或服务商不合法`);
        continue;
      }
      // 断路器：熔断中的跳直接略过（半开探测除外）——冷却期里不再每发付一次完整超时
      const breaker = this.breakerFor(hop.via);
      const gate = breakerAllows(breaker, now);
      if (!gate.allowed) {
        failures.push(`${hop.via}: 熔断冷却中（${BREAKER_COOLDOWN_MS / 1000}s）`);
        continue;
      }
      attempted.push(hop.via);
      try {
        const response = await hop.provider.decide(request);
        breakerOnSuccess(breaker);
        return response;
      } catch (error) {
        const message = error instanceof Error ? `${error.name}: ${error.message}` : String(error);
        failures.push(`${hop.via}: ${message}`);
        breakerOnFailure(breaker, Date.now());
        if (breaker.failures >= BREAKER_FAILURE_THRESHOLD && breaker.openedAt !== null) {
          incrementMetric("jev.breaker_opened");
        }
        if (!shouldFallbackToNext(error)) {
          // fail-fast：4xx 是凭据/路径级的错误，链上换一家救不了。聚合已尝试列表再抛
          throw new DecisionUnavailableError(
            `降级链在「${hop.via}」撞上不可降级的错误（${message}）；已尝试：${attempted.join(" → ")}`,
          );
        }
      }
    }
    incrementMetric("jev.chain_exhausted");
    throw new DecisionUnavailableError(
      `降级链 ${attempted.length} 跳都没答上（${this.hopNames.join(" → ")}）：${failures.join("；")}`,
    );
  }
}

/** 链的失败分类给测试用：不进运行时热路径 */
export function isDecisionError(error: unknown): error is DecisionError {
  return error instanceof DecisionError;
}
