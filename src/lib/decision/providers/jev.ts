/**
 * Jev 云端 Provider（System 1 的云上层）。
 * 按 TypeSafe 规格实现：POST /v1/systemone，body = { state, model, questions }，
 * 输出免费、输入按百万 token 计价——所以 state 能序列化就序列化成字符串，
 * questions 一次批量带全，别把一次决策拆成多次往返。
 *
 * 两条传输：
 * - rust（打包默认）：invoke → 原生 ureq（src-tauri/src/decision.rs）。WebView 里
 *   的 fetch 受 CORS 约束，云端没义务放行 tauri://localhost；原生通道不看 CORS，
 *   但每发必过 egress 出口名单——闸在原生侧，前端绕不开。
 * - direct：WebView/Node 直连。留给 Node 宿主与联调；Rust 通道缺席时也是兜底，
 *   失败由路由器降级消化。
 */
import type { DecisionProvider, DecisionRequest, DecisionResponse } from "../types";
import { DecisionTimeoutError, DecisionUnavailableError } from "../errors";
import { mapFetchError, normalizeProviderAnswers, statusFromMessage } from "./normalize";
import { platformInvoke, type TauriInvoke } from "./invoke";

export interface JevEndpointInput {
  name?: string;
  baseUrl?: string;
  apiKey?: string;
}

export interface JevProviderOptions {
  apiKey: string;
  via?: JevVia;
  /** 默认 rust：CORS 是打包应用里的常态障碍，原生通道才是正路 */
  transport?: "direct" | "rust";
  /**
   * 遗留单服务商。决策池（endpoints）为空时它迁移成池里唯一一条；池子配了它就不再参与
   */
  baseUrl?: string;
  /**
   * 决策池：via=custom 时的出站服务商列表。请求粘住上次成功的那条（服务商亲和），
   * 失败或超时按顺序换下一条，全挂才把这一层报成不可用
   */
  endpoints?: JevEndpointInput[];
  timeoutMs: number;
  /** 测试注入点（direct 用） */
  fetchImpl?: typeof fetch;
  /** 测试注入点（rust 用）；生产是 @tauri-apps/api/core 的 invoke */
  invokeImpl?: TauriInvoke;
  /**
   * 开了它，apiKey 字段留空、密钥从 Rust 侧 keyring 读取（decision_jev_system_one 内部回退）。
   * 密钥从此不过 IPC、不进 WebView 内存——「前端只拿句柄」的落地形状
   */
  useKeyring?: boolean;
}

export const JEV_ENDPOINTS = {
  typesafe: "https://api.typesafe.ai/v1/systemone",
  openrouter: "https://openrouter.ai/api/alpha/decisions",
} as const;

export type JevVia = keyof typeof JEV_ENDPOINTS | "custom";

/** 环回：127.0.0.0/8 整段都是本机，`localhost` 与 `::1` 是它的两个名字。
 *  只用来放行 http 联调，外发明文一律拒 */
function isLoopbackHost(host: string): boolean {
  return host === "localhost" || host === "::1" || host.startsWith("127.");
}

/**
 * 自定义服务商的形式规则。原生侧 `decision.rs::jev_endpoint_problem` 是同一条，
 * 两边由那张 `JEV_URL_CASES` 表钉着（Rust 会读这个文件逐行比对）——改一边不同步另一边就红。
 *
 * 它管的是"这个地址读不读得出来、是不是明文、有没有把凭据写在里面"。
 * 它**不**管"这台主机该不该收这一发"：那是出口域名名单的活，名单为空 = 不收紧。
 */
export function jevEndpointProblem(rawUrl: string): string | null {
  const url = rawUrl.trim();
  if (!url) return "要填完整的服务商 URL";
  if (/\s/.test(url)) return "URL 里不许有空格";
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    return "读不出这个 URL";
  }
  if (parsed.protocol !== "https:" && parsed.protocol !== "http:") return "只允许 http 或 https";
  const host = parsed.hostname.replace(/^\[+|[\]]+$/g, "").toLowerCase();
  if (!host) return "缺主机名";
  if (parsed.protocol === "http:" && !isLoopbackHost(host))
    return "http 只允许本机（127.x / localhost / ::1），其余要 https";
  if (parsed.username || parsed.password) return "URL 里不许带凭据（user:token@）";
  if (parsed.pathname === "" || parsed.pathname === "/")
    return "要把服务商路径写全，例如 /v1/systemone";
  return null;
}

/** 两边共用的判定表。Rust 侧按行读它（`url: "…"` 配 `ok: true|false`），所以一条一行、别折行 */
export const JEV_URL_CASES: ReadonlyArray<{ url: string; ok: boolean }> = [
  { url: "https://api.example.com/v1/systemone", ok: true },
  { url: "  https://api.example.com/v1/systemone  ", ok: true },
  { url: "https://api.example.com:8443/v1/decisions", ok: true },
  { url: "HTTPS://API.Example.COM/v1", ok: true },
  { url: "https://api.example.com/v1#frag", ok: true },
  { url: "http://127.0.0.1:8787/v1/systemone", ok: true },
  { url: "http://localhost:8787/v1/systemone", ok: true },
  { url: "http://[::1]:8787/v1/systemone", ok: true },
  { url: "http://127.0.0.2/v1", ok: true },
  // 形式上合法：这条明写着"规则不管该不该发，只管发得出去长什么样"。
  // 拦它的是出口域名名单——名单为空时它不拦（那是用户自己选的档）
  { url: "https://169.254.169.254/latest/meta-data", ok: true },
  { url: "https://api.example.com", ok: false },
  { url: "https://api.example.com/", ok: false },
  { url: "https://api.example.com/?x=1", ok: false },
  { url: "http://api.example.com/v1/systemone", ok: false },
  { url: "http://10.0.0.5:8080/v1", ok: false },
  { url: "ftp://api.example.com/v1", ok: false },
  { url: "https://user:token@api.example.com/v1", ok: false },
  { url: "https://ex.com/a b", ok: false },
  { url: "not a url", ok: false },
  { url: "", ok: false },
  { url: "https:///v1", ok: false },
];

/** 别名跟最新版走；要复现实验时在配置层锁 jev-x.y.z（这里不硬编码版本号）。
 * 原生通道（decision.rs）持同一对服务商与这个别名——两边各有一份测试钉着 */
const JEV_MODEL_ID = "jev-latest";

const COMMAND = "decision_jev_system_one";

/** 决策池里的一条（构造期定形，运行期只读）。name 是报错与亲和说明里的称呼 */
interface PooledEndpoint {
  name: string;
  baseUrl: string;
  /** 条目自己的密钥。空 = 落到全局那把（显式 apiKey 或 keyring） */
  apiKey: string;
  /** 形式校验结果。null = 合法；不合法的条目留在池里但尝试时跳过 */
  problem: string | null;
}

export class JevProvider implements DecisionProvider {
  readonly name = "jev" as const;

  private readonly transport: "direct" | "rust";
  private readonly useKeyring: boolean;
  private readonly fetchImpl: typeof fetch;
  private readonly invokeImpl: TauriInvoke | null;
  private readonly pooled: PooledEndpoint[];
  /** 全员不合法时的第一条说法（"这一层为什么被跳过"）。有一条合法就是 null */
  private readonly endpointProblem: string | null;
  private readonly timeoutMs: number;
  /** 服务商亲和：上次成功的那个，下一次排最前。进程级记忆，重装系统就重来 */
  private preferred: PooledEndpoint | null = null;
  /** keyring 里有没有存 key。异步探测（见 setKeyringAvailable），同步口径只看这个缓存值 */
  private keyringAvailable = false;

  constructor(private readonly options: JevProviderOptions) {
    this.transport = options.transport ?? "rust";
    this.useKeyring = options.useKeyring ?? false;
    this.fetchImpl = options.fetchImpl ?? ((...args) => fetch(...args));
    // invokeImpl 只留给测试注入；默认路径在 decideViaRust 里写字面量命令名——
    // lib.rs 的命令面守卫按文本数前端 invoke 调用的字面量命令名，变量透传它看不见，
    // 注释里连形似调用的字样都别写（上一版注释里的一对引号就把守卫钓红了）
    this.invokeImpl = options.invokeImpl ?? null;
    this.timeoutMs = options.timeoutMs;
    this.pooled = JevProvider.buildPool(options);
    this.endpointProblem =
      this.pooled.length > 0 && this.pooled.every((entry) => entry.problem !== null)
        ? this.pooled[0].problem
        : null;
  }

  /** 决策池装配：内置厂商是只有一个成员的池；via=custom 用 endpoints，
   *  老配置没有 endpoints 就从单独那格 baseUrl 迁一条进来 */
  private static buildPool(options: JevProviderOptions): PooledEndpoint[] {
    const via = options.via ?? "typesafe";
    if (via !== "custom") {
      return [{ name: via, baseUrl: JEV_ENDPOINTS[via], apiKey: "", problem: null }];
    }
    const listed = Array.isArray(options.endpoints)
      ? options.endpoints.filter(
          (entry): entry is JevEndpointInput & { baseUrl: string } =>
            typeof entry?.baseUrl === "string" && entry.baseUrl.trim() !== "",
        )
      : [];
    const source =
      listed.length > 0
        ? listed
        : typeof options.baseUrl === "string" && options.baseUrl.trim() !== ""
          ? [{ name: "服务商 1", baseUrl: options.baseUrl }]
          : [];
    return source.map((entry, index) => {
      const baseUrl = entry.baseUrl.trim();
      return {
        name: entry.name?.trim() || `服务商 ${index + 1}`,
        baseUrl,
        apiKey: typeof entry.apiKey === "string" ? entry.apiKey : "",
        problem: jevEndpointProblem(baseUrl),
      };
    });
  }

  /**
   * 有钥匙（全局显式、keyring、或某条服务商自带）且池里至少有一条形式合法。
   * 没钥匙就不可用；服务商填得不像话的那几条在尝试时跳过——只要还有一条能走，
   * 这一层就不该整层缺席。keyring 模式下显式 apiKey 为空，可用性取决于探测结果
   */
  get isAvailable(): boolean {
    const hasKey =
      this.options.apiKey.length > 0 ||
      (this.useKeyring && this.keyringAvailable) ||
      this.pooled.some((entry) => entry.apiKey.length > 0);
    return hasKey && this.pooled.some((entry) => entry.problem === null);
  }

  /** 装配层启动时把 keyring 的有无喂进来（只喂布尔，密钥本体从不过 IPC） */
  setKeyringAvailable(available: boolean): void {
    this.keyringAvailable = available;
  }

  async decide(request: DecisionRequest): Promise<DecisionResponse> {
    if (!this.isAvailable) {
      throw new DecisionUnavailableError(
        this.endpointProblem
          ? `Jev 服务商不合法：${this.endpointProblem}`
          : this.pooled.length === 0
            ? "决策池里还没有服务商"
            : "Jev apiKey 未配置",
      );
    }
    // 顺序：上次成功的排最前，其余按配置顺序补位。粘住健康的那条，
    // 不每发都从头撞一遍坏掉的——撞坏条目的代价是一次完整超时
    const valid = this.pooled.filter((entry) => entry.problem === null);
    const order =
      this.preferred && valid.includes(this.preferred)
        ? [this.preferred, ...valid.filter((entry) => entry !== this.preferred)]
        : valid;
    const failures: string[] = [];
    const causes: unknown[] = [];
    for (const entry of order) {
      try {
        const response =
          this.transport === "rust"
            ? await this.decideViaRust(entry, request)
            : await this.decideDirect(entry, request);
        this.preferred = entry;
        return response;
      } catch (error) {
        failures.push(`「${entry.name}」${error instanceof Error ? error.message : String(error)}`);
        causes.push(error);
      }
    }
    // 聚合错误要保住「该不该 fail-fast」的状态语义：全部失败都是 4xx（非 402/429）时
    // 带上那条状态码——降级链靠它终止整条链；只要有一条临时性失败（5xx/网络）就不带，
    // 给链上的下一跳一个机会。凭据/路径级的错误换服务商也救不了，这正是 fail-fast 的初衷
    const statuses = causes
      .filter((cause): cause is DecisionUnavailableError =>
        cause instanceof DecisionUnavailableError && typeof cause.status === "number",
      )
      .map((cause) => cause.status as number);
    const allClientError =
      statuses.length > 0 && statuses.every((status) => status < 500 && status !== 402 && status !== 429);
    throw new DecisionUnavailableError(
      `决策池 ${order.length} 个服务商都没答上：${failures.join("；")}`,
      { status: allClientError ? statuses[0] : undefined },
    );
  }

  /** 空状态最小请求验证可用性。失败会带出具体原因（401/超时/网络），比裸 ping 有用 */
  async warmup(): Promise<void> {
    await this.decide({
      state: "ping",
      questions: { alive: { type: "noul", instructions: "Is this a ping message?" } },
      timeoutMs: 3000,
    });
  }

  /** 这一条的生效密钥：条目自带的优先，空则落到全局（显式密钥或 keyring） */
  private keyFor(entry: PooledEndpoint): string {
    return entry.apiKey.length > 0 ? entry.apiKey : this.options.apiKey;
  }

  private async decideDirect(entry: PooledEndpoint, request: DecisionRequest): Promise<DecisionResponse> {
    const started = Date.now();
    try {
      const resp = await this.fetchImpl(entry.baseUrl, {
        method: "POST",
        headers: {
          Authorization: `Bearer ${this.keyFor(entry)}`,
          "Content-Type": "application/json",
        },
        body: JSON.stringify(this.payloadFor(request)),
        signal: AbortSignal.timeout(request.timeoutMs ?? this.timeoutMs),
      });
      if (!resp.ok) {
        // 状态码结构化带上：降级链（jev-chain）靠它分「该换下一跳」还是「fail-fast」
        throw new DecisionUnavailableError(`Jev HTTP ${resp.status}`, { status: resp.status });
      }
      const data = (await resp.json()) as { answers?: unknown; model?: string };
      return {
        answers: normalizeProviderAnswers(data.answers, "jev", started),
        model: "jev",
        totalLatencyMs: Date.now() - started,
        cacheHit: false,
      };
    } catch (error) {
      throw mapFetchError(error, "Jev");
    }
  }

  private async decideViaRust(entry: PooledEndpoint, request: DecisionRequest): Promise<DecisionResponse> {
    const started = Date.now();
    const via = this.options.via ?? "typesafe";
    // 结构与 decision.rs 的 JevDecisionRequest 对齐（camelCase，request 包一层）。
    // baseUrl 只在自定义时送：那两家的地址由原生侧自己持有，前端递过去也没人认
    const payload = {
      request: {
        apiKey: this.keyFor(entry),
        state: typeof request.state === "string" ? request.state : JSON.stringify(request.state),
        questions: request.questions,
        via,
        baseUrl: via === "custom" ? entry.baseUrl : undefined,
        timeoutMs: request.timeoutMs ?? this.timeoutMs,
      },
    };
    try {
      // 默认路径把命令名写成字面量——lib.rs 的命令面守卫数的就是这个
      const raw = (await (this.invokeImpl
        ? this.invokeImpl(COMMAND, payload)
        : platformInvoke().then((invoke) => invoke("decision_jev_system_one", payload)))) as {
        answers?: unknown;
      };
      return {
        answers: normalizeProviderAnswers(raw.answers, "jev", started),
        model: "jev",
        totalLatencyMs: Date.now() - started,
        cacheHit: false,
      };
    } catch (error) {
      // 原生侧已给出人话错误（"Jev HTTP 401"/出口被拦的原文），直接沿用，不要再包一层；
      // 状态码从原文里捞出来结构化挂上——降级链的 fail-fast 判定要数字，不要正则重跑
      const name = (error as { name?: string } | null)?.name;
      if (name === "TimeoutError" || name === "AbortError") {
        throw new DecisionTimeoutError("Jev 请求超时");
      }
      const message = error instanceof Error ? error.message : String(error);
      throw new DecisionUnavailableError(message, { status: statusFromMessage(message) });
    }
  }

  /** Jev 的 state 只收文本：对象在这里序列化，questions 保持结构化 */
  private payloadFor(request: DecisionRequest): {
    state: string;
    model: string;
    questions: DecisionRequest["questions"];
  } {
    return {
      state: typeof request.state === "string" ? request.state : JSON.stringify(request.state),
      model: JEV_MODEL_ID,
      questions: request.questions,
    };
  }
}
