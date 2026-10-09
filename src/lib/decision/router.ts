/**
 * 三级决策漏斗：Laya（本地）→ Jev（云端）→ Fallback（System 2）。
 *
 * 几条写死的规矩（对应 design-decision-layer.md 的红线）：
 * 1. 敏感请求（private/confidential）钉死在本地——候选列表里只有 laya，
 *    Jev 和 fallback（也是云端）连被尝试的机会都没有。这层硬约束失败就抛错，
 *    让调用方退回规则策略，绝不「降级到云端」。
 * 2. modelPreference="laya" 同理：显式选本地是隐私/成本意图，失败静默换 Jev
 *    就是违背调用方明说的话。而 preference="jev" 是质量意图，降级到 Laya
 *    只损失精度不越界，所以允许降级并标 degraded。
 * 3. 低于阈值走到漏斗尽头时，交还前几层里最好的结果并标 degraded——
 *    不阻塞、不编造，置信度是什么就是什么，采信权在调用方。
 * 4. 每次 decide 恰好一条 trace 进审计（脱敏在 audit 写入端做）。全层失败也是失败，
 *    也要留痕：那条 trace 的 response 是 null，它正是"sidecar 到底起没起"唯一的读数处。
 *    只有总开关关着时不记——那一刻决策层根本不存在，谈不上一次决策。
 */
import type {
  DecisionAttempt,
  DecisionProvider,
  DecisionRequest,
  DecisionResponse,
  DecisionRouter,
  DecisionTrace,
  ModelTier,
} from "./types";
import { DecisionError, DecisionUnavailableError } from "./errors";
import { DecisionCache, fnv1a, stableStringify } from "./cache";
import { DecisionAudit, redactRequest } from "./audit";
import { PER_QUESTION_UPGRADE_BATCH } from "./constants";
import { incrementMetric } from "./metrics";
import type { DecisionLayerConfig } from "./config";

export interface DecisionRouterOptions {
  laya: DecisionProvider;
  jev: DecisionProvider | null;
  fallback: DecisionProvider;
  cache: DecisionCache | null;
  audit: DecisionAudit | null;
  config: DecisionLayerConfig;
}

/** 取一次批量决策里最不确信的那个答案——木桶效应，升级判断看短板 */
export function getMinConfidence(response: DecisionResponse): number {
  const values = Object.values(response.answers).map((a) => a.confidence);
  if (values.length === 0) return 0;
  return Math.min(...values);
}

export class DecisionRouterImpl implements DecisionRouter {
  private readonly providers = new Map<ModelTier, DecisionProvider>();
  /**
   * 进行中的同键请求（深度优化）：决策缓存只挡**已完成的**——同一毫秒里三个调用方
   * 问同一个问题时，缓存救不了它们，三份全价判定照付。in-flight 表让并发同键请求
   * 共享同一次 Provider 调用。键是缓存键同款（键序归一哈希）；请求 settle 后即删
   */
  private readonly inFlight = new Map<string, Promise<DecisionResponse>>();

  constructor(private readonly options: DecisionRouterOptions) {
    this.providers.set("laya", options.laya);
    if (options.jev) this.providers.set("jev", options.jev);
    this.providers.set("fallback", options.fallback);
  }

  /** 与 DecisionCache 同口径的键（缓存缺席时也能算——去重不依赖缓存开着） */
  private flightKey(request: DecisionRequest): string {
    const canonical = `${stableStringify(request.state)}\u0000${stableStringify(request.questions)}`;
    return `dlf:${request.sensitivity ?? "public"}:${fnv1a(canonical)}-${canonical.length}`;
  }

  registerProvider(provider: DecisionProvider, _priority?: number): void {
    // priority 是接口兼容保留位：漏斗顺序由敏感性与偏好决定，不由它决定
    this.providers.set(provider.name, provider);
  }

  setFallback(provider: DecisionProvider): void {
    this.providers.set("fallback", provider);
  }

  async decide(request: DecisionRequest): Promise<DecisionResponse> {
    if (!this.options.config.enabled) {
      throw new DecisionUnavailableError("决策层已被关闭（decisionLayer.enabled=false）");
    }
    const started = Date.now();

    // 缓存在漏斗前面：命中时链路是空数组、cacheHit=true，审计照记（回放要能对上）。
    // 命中就是什么都没问——attempts 跟着是空集，不编出一层"答得飞快"的假过程
    const cached = this.options.cache?.get<DecisionResponse>(request);
    if (cached) {
      const hit = { ...cached, cacheHit: true };
      this.emitTrace(started, request, hit, [], [], true, []);
      return hit;
    }

    // in-flight 去重（深度优化）：并发同键请求共享同一次漏斗执行。
    // 复用者与缓存命中同语义（什么都没问、cacheHit=true、attempts 空集）——
    // 效果上它就是一次即时缓存；区分它们对调用方没有行动价值
    const flightKey = this.flightKey(request);
    const pending = this.inFlight.get(flightKey);
    if (pending) {
      incrementMetric("router.coalesced");
      const shared = await pending;
      const shared_ = { ...shared, cacheHit: true };
      this.emitTrace(started, request, shared_, [], [], true, []);
      return shared_;
    }
    const execution = this.executeDecide(request, started);
    this.inFlight.set(flightKey, execution);
    try {
      return await execution;
    } finally {
      // 成败都删：失败的下一次请求要能重试，成功后缓存接手
      this.inFlight.delete(flightKey);
    }
  }

  /** 漏斗主体（decide 的执行半身）。in-flight 表保证同一时刻同一键只有一份在跑 */
  private async executeDecide(
    request: DecisionRequest,
    started: number,
  ): Promise<DecisionResponse> {
    const maxSteps = Math.max(1, Math.round(this.options.config.routing.maxUpgradeChain));
    const { tiers, hardLocal, excluded } = this.planTiers(request, maxSteps);
    const asked = new Map<ModelTier, DecisionAttempt>();
    const attempts = () => this.attemptsFor(tiers, asked, excluded);

    const chain: ModelTier[] = [];
    const errors: string[] = [];
    let best: DecisionResponse | null = null;

    for (const tier of tiers) {
      const layerStarted = Date.now();
      const provider = this.providers.get(tier);
      if (!provider || !provider.isAvailable) {
        errors.push(`${tier}:unavailable`);
        asked.set(tier, {
          tier,
          outcome: "unavailable",
          latencyMs: 0,
          reason: provider ? "已注册，但它自己报不可用" : "这一层没有注册 Provider",
        });
        continue;
      }
      chain.push(tier);
      try {
        const response = await provider.decide(request);
        const minConfidence = getMinConfidence(response);
        const isFirstAnswer = !best;
        if (!best || minConfidence > getMinConfidence(best)) {
          best = response;
        }
        if (minConfidence >= this.options.config.routing.autoUpgradeThreshold) {
          asked.set(tier, {
            tier,
            outcome: "answered",
            latencyMs: Date.now() - layerStarted,
            minConfidence,
          });
          return this.finish(started, request, response, chain, errors, attempts());
        }
        // 漏斗 V2（§4 修 A3）：laya 首答低于阈值时不再整批作废——只有低置信子问题
        // 送 Jev 补答（≤8 问单包，>8 问整批重验），其余保留。升级成功就收工，
        // 走不到 fallback；升级失败/不可用才继续原漏斗。敏感钉本地（hardLocal）、
        // jev 已在前面答过（preference=jev）、或 jev 被升级链上限砍掉时都不升级
        if (tier === "laya" && !hardLocal && isFirstAnswer && tiers.includes("jev")) {
          const merged = await this.upgradePerQuestion(request, response, asked, chain, errors);
          if (merged) return this.finish(started, request, merged, chain, errors, attempts());
        }
        asked.set(tier, {
          tier,
          outcome: "below-threshold",
          latencyMs: Date.now() - layerStarted,
          minConfidence,
          reason: `最小置信 ${minConfidence.toFixed(2)} < 阈值 ${this.options.config.routing.autoUpgradeThreshold.toFixed(2)}`,
        });
      } catch (error) {
        const message = error instanceof Error ? `${error.name}: ${error.message}` : String(error);
        errors.push(`${tier}:${message}`);
        asked.set(tier, {
          tier,
          outcome: "failed",
          latencyMs: Date.now() - layerStarted,
          reason: message,
        });
        if (hardLocal) {
          // 本地钉死且本地失败：与其把敏感数据送出去，不如把这次决策交还失败。
          // 交还之前先落一条 response=null 的 trace——"这条红线拦下了什么"必须看得见，
          // 否则面板只会显示一次都没来过
          this.emitTrace(started, request, null, chain, errors, false, attempts());
          const wrapped =
            error instanceof DecisionError
              ? error
              : new DecisionUnavailableError(`本地决策失败且该请求被钉在本地：${message}`);
          throw wrapped;
        }
      }
    }

    if (best) {
      // degraded 语义：没达到阈值就到头了。答案是真的，只是没人替它拍胸脯
      return this.finish(started, request, best, chain, errors, attempts());
    }
    const summary = errors.length > 0 ? errors.join(" | ") : "无已注册 Provider";
    this.emitTrace(started, request, null, chain, errors, false, attempts());
    throw new DecisionUnavailableError(`所有决策层都不可用：${summary}`);
  }

  /** 候选层顺序，以及被规矩挡在门外的那些层和它们的理由 */
  private planTiers(
    request: DecisionRequest,
    maxSteps: number,
  ): {
    tiers: ModelTier[];
    hardLocal: boolean;
    excluded: Array<{ tier: ModelTier; reason: string }>;
  } {
    const sensitivity = request.sensitivity ?? "public";
    const forceLocal =
      this.options.config.routing.sensitiveForceLocal &&
      (sensitivity === "private" || sensitivity === "confidential");
    const preference = request.modelPreference ?? "auto";
    const excluded: Array<{ tier: ModelTier; reason: string }> = [];
    let ordered: ModelTier[];
    let hardLocal = false;
    if (forceLocal) {
      ordered = ["laya"];
      hardLocal = true;
      excluded.push({ tier: "jev", reason: `敏感请求（${sensitivity}）钉在本地，不出这台电脑` });
      excluded.push({ tier: "fallback", reason: "System 2 也在云端，同一条红线" });
    } else if (preference === "laya") {
      ordered = ["laya"];
      hardLocal = true;
      excluded.push({ tier: "jev", reason: "调用方指定本地" });
      excluded.push({ tier: "fallback", reason: "调用方指定本地" });
    } else if (preference === "jev") {
      ordered = ["jev", "laya"];
      excluded.push({ tier: "fallback", reason: "这一轮的候选里没有它（偏好 jev）" });
    } else {
      ordered = ["laya", "jev", "fallback"];
    }
    const tiers = ordered.slice(0, maxSteps);
    for (const dropped of ordered.slice(maxSteps)) {
      excluded.push({ tier: dropped, reason: `升级链上限 ${maxSteps} 层` });
    }
    return { tiers, hardLocal, excluded };
  }

  /**
   * 摊成给审计看的那一串：候选里每一层的下场（没轮到的补一格 not-reached），
   * 后面接被规矩挡掉的那些。顺序就是漏斗问下去的顺序。
   */
  private attemptsFor(
    tiers: ModelTier[],
    asked: Map<ModelTier, DecisionAttempt>,
    excluded: Array<{ tier: ModelTier; reason: string }>,
  ): DecisionAttempt[] {
    return [
      ...tiers.map(
        (tier) => asked.get(tier) ?? { tier, outcome: "not-reached" as const, latencyMs: 0 },
      ),
      ...excluded.map((entry) => ({
        tier: entry.tier,
        outcome: "excluded" as const,
        latencyMs: 0,
        reason: entry.reason,
      })),
    ];
  }

  /**
   * 按问题粒度升级（§4 修 A3）。laya 整批里只有低置信子问题值得再问一次云端：
   * - 子集 ≤ PER_QUESTION_UPGRADE_BATCH 问 → 单包升级（只送子集，state 原样）
   * - 子集更大 → 整批送：state 反正要发一次，全量重验的输入增量可忽略，
   *   还顺带交叉验证了高置信答案，省掉子集拆装的簿记
   * 采信判据是**问级**的：Jev 对该问的置信 ≥ 阈值才采纳（sources=jev），
   * 否则保留 laya 原答案（§3 保守默认：不确定即保留）。Jev 整体失败 → 返回 null，
   * 调用方继续原漏斗。返回的响应带逐问 sources。
   */
  private async upgradePerQuestion(
    request: DecisionRequest,
    layaResponse: DecisionResponse,
    asked: Map<ModelTier, DecisionAttempt>,
    chain: ModelTier[],
    errors: string[],
  ): Promise<DecisionResponse | null> {
    const jev = this.options.jev;
    if (!jev || !jev.isAvailable) return null;
    const threshold = this.options.config.routing.autoUpgradeThreshold;
    const lowNames = Object.entries(layaResponse.answers)
      .filter(([, answer]) => answer.confidence < threshold)
      .map(([name]) => name);
    if (lowNames.length === 0) return null;
    const questions =
      lowNames.length > PER_QUESTION_UPGRADE_BATCH
        ? request.questions
        : Object.fromEntries(lowNames.map((name) => [name, request.questions[name]!]));
    const started = Date.now();
    chain.push("jev");
    let jevResponse: DecisionResponse;
    try {
      jevResponse = await jev.decide({ ...request, questions });
    } catch (error) {
      const message = error instanceof Error ? `${error.name}: ${error.message}` : String(error);
      errors.push(`jev:upgrade:${message}`);
      asked.set("jev", {
        tier: "jev",
        outcome: "failed",
        latencyMs: Date.now() - started,
        reason: message,
      });
      return null;
    }
    const sources = Object.fromEntries(
      Object.keys(layaResponse.answers).map((name) => [name, layaResponse.model]),
    ) as Record<string, ModelTier>;
    const answers = { ...layaResponse.answers };
    let upgraded = 0;
    for (const name of Object.keys(questions)) {
      const jevAnswer = jevResponse.answers[name];
      if (jevAnswer && jevAnswer.confidence >= threshold) {
        answers[name] = { ...jevAnswer };
        sources[name] = "jev";
        upgraded += 1;
      }
    }
    asked.set("jev", {
      tier: "jev",
      outcome: upgraded > 0 ? "answered" : "below-threshold",
      latencyMs: Date.now() - started,
      minConfidence: getMinConfidence(jevResponse),
      reason:
        upgraded > 0
          ? `按问升级采信 ${upgraded}/${lowNames.length} 问`
          : `升级重验后仍全部低于阈值（原 laya 答案保留）`,
    });
    if (upgraded > 0) incrementMetric("funnel.upgraded");
    const merged: DecisionResponse = {
      ...layaResponse,
      answers,
      sources,
      degraded: Object.values(answers).some((answer) => answer.confidence < threshold),
    };
    return merged;
  }

  private finish(
    started: number,
    request: DecisionRequest,
    response: DecisionResponse,
    chain: ModelTier[],
    errors: string[],
    attempts: DecisionAttempt[],
  ): DecisionResponse {
    const degraded = getMinConfidence(response) < this.options.config.routing.autoUpgradeThreshold;
    if (degraded) incrementMetric("funnel.conservative_default");
    // sources 全量补齐：混合批次带着自己的 sources 来；整批响应逐问标 response.model
    const sources =
      response.sources ??
      (Object.fromEntries(
        Object.keys(response.answers).map((name) => [name, response.model]),
      ) as Record<string, ModelTier>);
    const result: DecisionResponse = {
      ...response,
      sources,
      totalLatencyMs: Date.now() - started,
      cacheHit: false,
      degraded,
    };
    this.options.cache?.set(request, result);
    this.emitTrace(started, request, result, chain, errors, false, attempts);
    return result;
  }

  private emitTrace(
    started: number,
    request: DecisionRequest,
    response: DecisionResponse | null,
    chain: ModelTier[],
    errors: string[],
    cacheHit: boolean,
    attempts: DecisionAttempt[],
  ): void {
    if (!this.options.audit || !this.options.config.audit.enabled) return;
    const trace: DecisionTrace = {
      id: randomId(),
      timestamp: new Date().toISOString(),
      request: redactRequest(request, this.options.config.audit.redactPrivatePreviewChars),
      response,
      modelChain: [...chain],
      attempts,
      totalLatencyMs: Date.now() - started,
      cacheHit,
    };
    if (errors.length > 0) trace.errors = errors.join(" | ");
    this.options.audit.record(trace);
  }
}

/** crypto.randomUUID 在 WebView2 / Node 19+ 都有；测试环境退化用时间+随机 */
function randomId(): string {
  const cryptoRef = globalThis.crypto;
  if (typeof cryptoRef?.randomUUID === "function") return cryptoRef.randomUUID();
  return `t-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}
