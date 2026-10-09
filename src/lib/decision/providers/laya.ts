/**
 * Laya 本地 Provider（System 1 的本地层）。
 *
 * 双传输、运行时探测：
 * - `http`：POST {sidecarEndpoint}/systemOne，把推理放进一个本地常驻进程。
 *   这是 WebView 里的**主路径**——@receptron/laya 依赖 onnxruntime-node，
 *   浏览器上下文里根本加载不起来，模型必须活在自己的 Node/Python 进程里
 *   （Windows 用 scripts/laya-sidecar/，macOS 可换 laya-mlx 的同契约服务）。
 * - `embedded`：动态 import @receptron/laya，只在 Node 宿主（测试/未来 CLI）成功；
 *   用变量specifier配合 @vite-ignore，构建期不解析、不进依赖图，WebView 里
 *   import 失败就安静地降 isAvailable，不影响构建也不断主流程。
 *
 * `auto`：配了 sidecarEndpoint 就认 http（本地端口探测便宜且确定），否则看 embedded。
 */
import type { DecisionProvider, DecisionRequest, DecisionResponse } from "../types";
import { DecisionUnavailableError } from "../errors";
import { mapFetchError, normalizeProviderAnswers } from "./normalize";

export interface LayaProviderOptions {
  transport: "auto" | "http" | "embedded";
  sidecarEndpoint?: string;
  subfolder?: string;
  timeoutMs: number;
  /** 测试注入点。生产用全局 fetch（WebView2 / Node 都有） */
  fetchImpl?: typeof fetch;
}

/** /health 的形状。scripts/laya-sidecar 的 index.mjs 是第一份实现，两边的字段对齐 */
export interface LayaHealth {
  /** 进程活着且端口在听 */
  ok: boolean;
  /** ONNX 权重已加载进内存——热身后第一次决策不再付冷启动 */
  loaded: boolean;
  /** 正在加载/下载权重（首次请求触发的 1.7GB 下载就走这个态） */
  loading: boolean;
}

interface RawLayaResult {
  answers?: unknown;
}

/** @receptron/laya 的最小类型面。包不在依赖里，这里只认我们用到的那几个字段 */
interface EmbeddedLayaModule {
  Laya: {
    load(options: { subfolder?: string; executionProviders?: string[] }): Promise<{
      systemOne(state: unknown, questions: unknown): Promise<RawLayaResult>;
      close?(): Promise<void>;
    }>;
  };
}

export class LayaProvider implements DecisionProvider {
  readonly name = "laya" as const;

  private readonly fetchImpl: typeof fetch;
  private readonly timeoutMs: number;
  private embeddedModule: EmbeddedLayaModule["Laya"] | null = null;
  private embeddedInstance: Awaited<ReturnType<EmbeddedLayaModule["Laya"]["load"]>> | null = null;
  private embeddedProbe: Promise<boolean> | null = null;

  constructor(private readonly options: LayaProviderOptions) {
    this.fetchImpl = options.fetchImpl ?? ((...args) => fetch(...args));
    this.timeoutMs = options.timeoutMs;
  }

  get hasHttpEndpoint(): boolean {
    return !!this.options.sidecarEndpoint;
  }

  get isAvailable(): boolean {
    switch (this.options.transport) {
      case "http":
        return this.hasHttpEndpoint;
      case "embedded":
        return false; // 同步口径下不给承诺——embedded 可用性要等探测落定，走 decide 前先 warmup
      case "auto":
      default:
        return this.hasHttpEndpoint; // auto 时没有服务商就只剩 embedded，但那是异步事实，让 decide 的失败兜底
    }
  }

  /**
   * 同步 isAvailable 的保守口径说明：http 有服务商就算可用（每次 decide 自带超时与失败处理，
   * sidecar 没起来的代价是一次快速连接失败，审计里看得见）；embedded 必须探测过才算数。
   * warmup 幂等且永不抛——失败等于「这层不可用」，不是崩溃。http 传输的预热 =
   * 打一次 /health：TCP 连接热了，sidecar 的 loaded/loading 状态也顺手带回来
   * （要不要据此弹「模型还在加载」的提示，是 Phase 4 决策面板的事）。
   */
  async warmup(): Promise<void> {
    if (this.options.transport === "http") {
      await this.health();
      return;
    }
    await this.probeEmbedded();
  }

  /** sidecar 进程的两态：loaded（权重在内存里）与 loading（首次请求正在触发下载/加载）。
   * 决策面板用它区分「没起 sidecar」和「起了但模型还在热身」——运维上是两件事 */
  async health(): Promise<LayaHealth> {
    const endpoint = this.options.sidecarEndpoint;
    if (!endpoint) return { ok: false, loaded: false, loading: false };
    try {
      const resp = await this.fetchImpl(`${endpoint.replace(/\/+$/, "")}/health`, {
        method: "GET",
        signal: AbortSignal.timeout(Math.min(this.timeoutMs, 2000)),
      });
      if (!resp.ok) return { ok: false, loaded: false, loading: false };
      const data = (await resp.json()) as { ok?: unknown; loaded?: unknown; loading?: unknown };
      return {
        ok: data.ok === true,
        loaded: data.loaded === true,
        loading: data.loading === true,
      };
    } catch {
      // 探测失败是健康检查的正常结果，不是错误：进程没起、端口没听，都落在这里
      return { ok: false, loaded: false, loading: false };
    }
  }

  async decide(request: DecisionRequest): Promise<DecisionResponse> {
    const useHttp =
      this.options.transport === "http" ||
      (this.options.transport === "auto" && this.hasHttpEndpoint);
    if (useHttp) return this.decideViaHttp(request);
    return this.decideEmbedded(request);
  }

  private async decideViaHttp(request: DecisionRequest): Promise<DecisionResponse> {
    const endpoint = this.options.sidecarEndpoint;
    if (!endpoint) throw new DecisionUnavailableError("Laya sidecar 服务商未配置");
    const started = Date.now();
    try {
      const resp = await this.fetchImpl(`${endpoint.replace(/\/+$/, "")}/systemOne`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ state: request.state, questions: request.questions }),
        // AbortSignal.timeout 比 setTimeout+abort 组合省一次手工清理；WebView2/Node 20+ 都支持
        signal: AbortSignal.timeout(request.timeoutMs ?? this.timeoutMs),
      });
      if (!resp.ok) {
        throw new DecisionUnavailableError(`Laya sidecar HTTP ${resp.status}`);
      }
      const data = (await resp.json()) as RawLayaResult;
      return {
        answers: normalizeProviderAnswers(data.answers, "laya", started),
        model: "laya",
        totalLatencyMs: Date.now() - started,
        cacheHit: false,
      };
    } catch (error) {
      throw mapFetchError(error, "Laya");
    }
  }

  private async decideEmbedded(request: DecisionRequest): Promise<DecisionResponse> {
    const instance = await this.loadEmbedded();
    const started = Date.now();
    try {
      const result = await instance.systemOne(request.state, request.questions);
      return {
        answers: normalizeProviderAnswers(result.answers, "laya", started),
        model: "laya",
        totalLatencyMs: Date.now() - started,
        cacheHit: false,
      };
    } catch (error) {
      throw mapFetchError(error, "Laya");
    }
  }

  /** 探测 embedded 模块，结果按进程缓存：失败一次就别每次调用都付一次 import 的代价 */
  private probeEmbedded(): Promise<boolean> {
    this.embeddedProbe ??= (async () => {
      try {
        // 变量 specifier + @vite-ignore：Vite 构建期跳过解析，包不在依赖里也不会炸构建
        const specifier = "@receptron/laya";
        const mod = (await import(/* @vite-ignore */ specifier)) as Partial<EmbeddedLayaModule>;
        if (!mod?.Laya) return false;
        this.embeddedModule = mod.Laya;
        return true;
      } catch {
        return false;
      }
    })();
    return this.embeddedProbe;
  }

  private async loadEmbedded(): Promise<NonNullable<LayaProvider["embeddedInstance"]>> {
    const available = await this.probeEmbedded();
    if (!available || !this.embeddedModule) {
      throw new DecisionUnavailableError(
        "@receptron/laya 在当前宿主不可用（WebView 没有 Node 运行时，请改用 sidecar 传输）",
      );
    }
    this.embeddedInstance ??= await this.embeddedModule.load({
      subfolder: this.options.subfolder ?? "multilingual",
      executionProviders: ["cpu"],
    });
    return this.embeddedInstance;
  }
}
