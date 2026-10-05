/**
 * 决策层配置。默认值即安全值：Jev 关着、apiKey 为空、敏感强制本地开着。
 * localStorage 里的历史配置可能缺字段、多字段、类型被手改坏——
 * 合并函数逐字段做类型校验，坏的退回默认值，绝不把半份配置带进运行时。
 */
import { JEV_ENDPOINTS, type JevVia } from "./providers/jev";

export interface LayaConfig {
  enabled: boolean;
  /** auto：配了 sidecarEndpoint 走 http，否则探测 embedded（@receptron/laya 只在 Node 宿主可用） */
  transport: "auto" | "http" | "embedded";
  /** 本地推理 sidecar 的服务商。Windows 参考 scripts/laya-sidecar/ */
  sidecarEndpoint: string;
  /**
   * sidecar 那份脚本所在的目录（绝对路径）。只有"一键启动"用它：原生要照着它去找
   * index.mjs。留空 = 没告诉过 aglab，按钮就该问你要，而不是猜一个装机器上的路径
   */
  sidecarDir: string;
  /** @receptron/laya 的 checkpoint 子目录（多语言版） */
  subfolder: string;
  warmupOnStart: boolean;
  timeoutMs: number;
}

/** 决策池的一条：一个可出站的 Jev 服务商。apiKey 留空 = 落到全局那把（显式密钥或 keyring） */
export interface JevEndpointConfig {
  name: string;
  baseUrl: string;
  apiKey: string;
}

export interface JevConfig {
  /** 默认关：走原生通道前先想清楚要把哪些决策送出去（见 routing.sensitiveForceLocal） */
  enabled: boolean;
  /**
   * 全局兜底密钥：条目没带自己的密钥时就用它（keyring 模式下留空——密钥住系统凭据库
   * （decision_jev_key_set），原生侧请求时自取，不过 IPC。哪里都没有 = Jev 不可用
   */
  apiKey: string;
  /**
   * 链上各内置厂商自己的密钥（§5.4 Provider 链的配套用户资产）。
   * 某跳的取用顺序：apiKeys[via] → 全局 apiKey → keyring。缺密钥的那一跳运行时跳过
   */
  apiKeys: Record<string, string>;
  /** 主 via：链上排第一的内置厂商；池子（custom）出现前的单服务商时代遗留字段 */
  via: JevVia;
  /**
   * 降级链（§5.4 修 B7）：按序尝试的 via 列表（内置厂商名或 "custom"）。
   * 402/429/5xx/超时/网络错误降级到下一跳，其余 4xx fail-fast。循环在 TS 侧，
   * 每跳仍走 decision_jev_system_one 原生通道，不开新的出站通道。
   * 白名单之外的成员配置往返时保留（前向兼容），运行时跳过；所以类型是 string[]
   * 而不是闭合的 JevVia——未来厂商上线时老配置里的名字自动生效，不被抹掉。
   * 重复项去重、全空回默认。默认只含两家有真实 systemone 服务商的厂商
   * ——链上更多跳由用户按自己的凭据与自建网关（custom）补
   */
  chain: string[];
  /**
   * 遗留字段：决策池出现之前的单服务商。池子为空时它迁移成池里唯一一条，之后不再被读取
   * ——留在这儿只为老配置往返不丢字段，设置页改的是 endpoints
   */
  baseUrl: string;
  /**
   * 决策池：via=custom 时的出站服务商列表。请求粘住上次成功的那条（服务商亲和），
   * 失败或超时按顺序换下一条，全挂才把这一层报成不可用。内置两家不进池（就一家）
   */
  endpoints: JevEndpointConfig[];
  /**
   * rust：经原生 ureq 通道（decision_jev_system_one）——不看 CORS，但过 egress 出口名单。
   * 这是打包后的默认；direct 是 WebView 直连，留给 Node 宿主与联调，云端没放行 CORS 时必挂
   */
  transport: "direct" | "rust";
  /** 密钥走 keyring（前端只拿句柄）。开了它 apiKey 字段作废 */
  useKeyring: boolean;
  timeoutMs: number;
}

export interface RoutingConfig {
  /** 全部答案置信度的最小值达到它才算「 System 1 拍板了」，否则逐级升级 */
  autoUpgradeThreshold: number;
  /** 漏斗最多走几层（laya→jev→fallback 恰好是 3） */
  maxUpgradeChain: number;
  /** 敏感请求钉死在本地：false 是给测试留的口子，生产永远 true */
  sensitiveForceLocal: boolean;
}

export interface DecisionCacheConfig {
  enabled: boolean;
  ttlMs: number;
  maxEntries: number;
}

export interface DecisionAuditConfig {
  enabled: boolean;
  maxEntries: number;
  /** private 级 state 在审计里保留的预览字符数；confidential 不受它影响（一律哈希化） */
  redactPrivatePreviewChars: number;
}

/**
 * Phase 3 嵌入点的独立开关。全部默认关：嵌入改变的是"现有流程多一步本地判定"，
 * 默认值必须是不改变现状的那一档——每个开关由用到它的接缝自己读。
 */
export interface IntegrationsConfig {
  /**
   * 记忆提取门控：每轮收尾先在本地判"这轮值不值得提取"（private → 强制本地，内容不出机器）。
   * 判不值得就省掉一次云端提取调用。fail-open：开关关、sidecar 没起、判定失败——照旧提取
   */
  memoryGate: boolean;
  /** worth_remembering 的 P(true) 低于它就跳过提取。0.5 = 模型自己都拿不准就不麻烦云端 */
  memoryGateThreshold: number;
  /** 记忆入库后的 sensitivity 自动分级（confidential 决策，强制本地）。只降不升 */
  sensitivityScan: boolean;
  /**
   * 逐消息模型路由：任务类型/复杂度进决策审计，**不改**模型选择——模型是档案级的
   * 产品决策，自动换档要等 Phase 4 面板把账摊出来再说
   */
  modelRouting: boolean;
  /**
   * Agent 分配（已接线）：Rust 编排侧给补做节点派工时经决策桥问到这里
   * （Rust 调用点 `orchestrator.rs` 的 `assign_followup_profiles`，派发表 `bridge.ts`）。
   * fail-open：开关关、判定失败——Rust 侧保持默认的 worker 档案
   */
  taskAssignment: boolean;
  /**
   * 上下文相关性打分（已接线）：记忆注入排序前经决策桥问到这里
   * （Rust 调用点 `inject.rs` 的 `decision_scores`）。fail-open：一条都没答齐就照原序
   */
  contextRelevance: boolean;
}

/**
 * Verbatim 压缩（§5.2）。阈值与判定全在 constants.ts，这里只有开关与用户资产。
 */
export interface CompactionConfig {
  enabled: boolean;
  /** 按 sessionId 复用 replacement 映射（Cache Guard 的 rewrite 触发条件在代码常量里） */
  sticky: boolean;
}

/**
 * 分阶段输出审查（§5.3）。Stage A/C/D 的编排、阈值、代码覆盖全部在代码。
 */
export interface StagedReviewConfig {
  enabled: boolean;
  /** 流式输出首块 ≥ REVIEW_FIRST_CHUNK_TOKENS 时先审再继续放 */
  streamingPreCheck: boolean;
}

/**
 * 检索决策层（§5.6）。Jev 只选不生成；候选生成、聚合、TTL 全在代码。
 */
export interface RetrievalConfig {
  enabled: boolean;
}

export interface DecisionLayerConfig {
  /** 总开关。关掉时路由器直接抛不可用，调用方退回各自原来的 prompt-and-parse 路径 */
  enabled: boolean;
  laya: LayaConfig;
  jev: JevConfig;
  routing: RoutingConfig;
  cache: DecisionCacheConfig;
  audit: DecisionAuditConfig;
  integrations: IntegrationsConfig;
  /** V2 新增能力的嵌入点开关（§7 修 B5：只留开关，阈值进常量） */
  compaction: CompactionConfig;
  stagedReview: StagedReviewConfig;
  retrieval: RetrievalConfig;
}

export const DEFAULT_DECISION_CONFIG: DecisionLayerConfig = {
  enabled: true,
  laya: {
    enabled: true,
    transport: "auto",
    sidecarEndpoint: "http://127.0.0.1:8787",
    sidecarDir: "",
    subfolder: "multilingual",
    warmupOnStart: true,
    timeoutMs: 5000,
  },
  jev: {
    enabled: false,
    apiKey: "",
    apiKeys: {},
    via: "typesafe",
    chain: ["typesafe", "openrouter"],
    baseUrl: "",
    endpoints: [],
    transport: "rust",
    useKeyring: false,
    timeoutMs: 5000,
  },
  routing: {
    autoUpgradeThreshold: 0.85,
    maxUpgradeChain: 3,
    sensitiveForceLocal: true,
  },
  cache: {
    enabled: true,
    ttlMs: 60_000,
    maxEntries: 1000,
  },
  audit: {
    enabled: true,
    maxEntries: 200,
    redactPrivatePreviewChars: 200,
  },
  integrations: {
    memoryGate: false,
    memoryGateThreshold: 0.5,
    sensitivityScan: false,
    modelRouting: false,
    taskAssignment: false,
    contextRelevance: false,
  },
  compaction: { enabled: true, sticky: true },
  stagedReview: { enabled: true, streamingPreCheck: true },
  retrieval: { enabled: true },
};

export const DECISION_CONFIG_STORAGE_KEY = "aglab.decisionLayer.config";

/** 最小存储接口：真身是 localStorage，测试注入 Map 桩，谁也不依赖谁 */
export interface ConfigStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}

function oneOf<T extends string>(raw: unknown, allowed: readonly T[], fallback: T): T {
  return typeof raw === "string" && (allowed as readonly string[]).includes(raw) ? (raw as T) : fallback;
}

/** 认得的厂商。名字从 `JEV_ENDPOINTS` 那一份推出来，这里不另抄一遍厂商名——
 *  抄一份就意味着加一家要记得改两处 */
const JEV_VIAS = [...Object.keys(JEV_ENDPOINTS), "custom"] as JevVia[];

function num(raw: unknown, fallback: number, min: number, max: number): number {
  return typeof raw === "number" && Number.isFinite(raw) && raw >= min && raw <= max ? raw : fallback;
}

function bool(raw: unknown, fallback: boolean): boolean {
  return typeof raw === "boolean" ? raw : fallback;
}

function str(raw: unknown, fallback: string): string {
  return typeof raw === "string" ? raw : fallback;
}

/** 决策池逐条合并：字段各自退默认，整条空白（没地址也没名字）的直接丢 */
function jevEndpointList(raw: unknown): JevEndpointConfig[] {
  if (!Array.isArray(raw)) return [];
  const out: JevEndpointConfig[] = [];
  for (const item of raw) {
    if (typeof item !== "object" || item === null) continue;
    const entry = item as Record<string, unknown>;
    const baseUrl = str(entry.baseUrl, "");
    const name = str(entry.name, "");
    if (baseUrl.trim() === "" && name.trim() === "") continue;
    out.push({ name, baseUrl, apiKey: str(entry.apiKey, "") });
  }
  return out;
}

/** 深合并 + 逐字段类型校验。未知键直接丢——配置文件不是自由格式字段袋 */
export function mergeDecisionConfig(raw: unknown): DecisionLayerConfig {
  const source = (typeof raw === "object" && raw !== null ? raw : {}) as Record<string, unknown>;
  const laya = (source.laya ?? {}) as Record<string, unknown>;
  const jev = (source.jev ?? {}) as Record<string, unknown>;
  const routing = (source.routing ?? {}) as Record<string, unknown>;
  const cache = (source.cache ?? {}) as Record<string, unknown>;
  const audit = (source.audit ?? {}) as Record<string, unknown>;
  const integrations = (source.integrations ?? {}) as Record<string, unknown>;
  const compaction = (source.compaction ?? {}) as Record<string, unknown>;
  const stagedReview = (source.stagedReview ?? {}) as Record<string, unknown>;
  const retrieval = (source.retrieval ?? {}) as Record<string, unknown>;
  return {
    enabled: bool(source.enabled, DEFAULT_DECISION_CONFIG.enabled),
    laya: {
      enabled: bool(laya.enabled, DEFAULT_DECISION_CONFIG.laya.enabled),
      transport: oneOf(laya.transport, ["auto", "http", "embedded"] as const, DEFAULT_DECISION_CONFIG.laya.transport),
      sidecarEndpoint: str(laya.sidecarEndpoint, DEFAULT_DECISION_CONFIG.laya.sidecarEndpoint),
      sidecarDir: str(laya.sidecarDir, DEFAULT_DECISION_CONFIG.laya.sidecarDir),
      subfolder: str(laya.subfolder, DEFAULT_DECISION_CONFIG.laya.subfolder),
      warmupOnStart: bool(laya.warmupOnStart, DEFAULT_DECISION_CONFIG.laya.warmupOnStart),
      timeoutMs: num(laya.timeoutMs, DEFAULT_DECISION_CONFIG.laya.timeoutMs, 100, 120_000),
    },
    jev: {
      enabled: bool(jev.enabled, DEFAULT_DECISION_CONFIG.jev.enabled),
      apiKey: str(jev.apiKey, DEFAULT_DECISION_CONFIG.jev.apiKey),
      // 链上各家的密钥：只认内置厂商名，其余键丢弃
      apiKeys: (() => {
        const out: Record<string, string> = {};
        if (typeof jev.apiKeys === "object" && jev.apiKeys !== null) {
          for (const [key, value] of Object.entries(jev.apiKeys as Record<string, unknown>)) {
            if ((JEV_VIAS as readonly string[]).includes(key) && typeof value === "string") {
              out[key] = value;
            }
          }
        }
        return out;
      })(),
      via: oneOf(jev.via, JEV_VIAS, DEFAULT_DECISION_CONFIG.jev.via),
      // 降级链：非空字符串去重保序（白名单外的成员往返保留、运行时跳过）；全空回默认
      chain: (() => {
        const listed = Array.isArray(jev.chain) ? jev.chain : [];
        const valid = listed.filter((item): item is string => typeof item === "string" && item.trim() !== "");
        const deduped = [...new Set(valid)];
        return deduped.length > 0 ? deduped : [...DEFAULT_DECISION_CONFIG.jev.chain];
      })(),
      baseUrl: str(jev.baseUrl, DEFAULT_DECISION_CONFIG.jev.baseUrl),
      // 决策池：新配置直接逐条合并；老配置（池子为空、只有单独一格 baseUrl）
      // 迁移成池里唯一一条——那一格就是当年的全部
      endpoints: (() => {
        const pooled = jevEndpointList(jev.endpoints);
        if (pooled.length > 0) return pooled;
        const legacy = str(jev.baseUrl, DEFAULT_DECISION_CONFIG.jev.baseUrl);
        return legacy.trim() === ""
          ? []
          : [{ name: "服务商 1", baseUrl: legacy, apiKey: "" }];
      })(),
      transport: oneOf(jev.transport, ["direct", "rust"] as const, DEFAULT_DECISION_CONFIG.jev.transport),
      useKeyring: bool(jev.useKeyring, DEFAULT_DECISION_CONFIG.jev.useKeyring),
      timeoutMs: num(jev.timeoutMs, DEFAULT_DECISION_CONFIG.jev.timeoutMs, 100, 120_000),
    },
    routing: {
      autoUpgradeThreshold: num(routing.autoUpgradeThreshold, DEFAULT_DECISION_CONFIG.routing.autoUpgradeThreshold, 0, 1),
      maxUpgradeChain: Math.round(num(routing.maxUpgradeChain, DEFAULT_DECISION_CONFIG.routing.maxUpgradeChain, 1, 8)),
      sensitiveForceLocal: bool(routing.sensitiveForceLocal, DEFAULT_DECISION_CONFIG.routing.sensitiveForceLocal),
    },
    cache: {
      enabled: bool(cache.enabled, DEFAULT_DECISION_CONFIG.cache.enabled),
      ttlMs: num(cache.ttlMs, DEFAULT_DECISION_CONFIG.cache.ttlMs, 0, 3_600_000),
      maxEntries: Math.round(num(cache.maxEntries, DEFAULT_DECISION_CONFIG.cache.maxEntries, 1, 100_000)),
    },
    audit: {
      enabled: bool(audit.enabled, DEFAULT_DECISION_CONFIG.audit.enabled),
      maxEntries: Math.round(num(audit.maxEntries, DEFAULT_DECISION_CONFIG.audit.maxEntries, 1, 10_000)),
      redactPrivatePreviewChars: Math.round(
        num(audit.redactPrivatePreviewChars, DEFAULT_DECISION_CONFIG.audit.redactPrivatePreviewChars, 0, 10_000),
      ),
    },
    integrations: {
      memoryGate: bool(integrations.memoryGate, DEFAULT_DECISION_CONFIG.integrations.memoryGate),
      memoryGateThreshold: num(
        integrations.memoryGateThreshold,
        DEFAULT_DECISION_CONFIG.integrations.memoryGateThreshold,
        0,
        1,
      ),
      sensitivityScan: bool(
        integrations.sensitivityScan,
        DEFAULT_DECISION_CONFIG.integrations.sensitivityScan,
      ),
      modelRouting: bool(integrations.modelRouting, DEFAULT_DECISION_CONFIG.integrations.modelRouting),
      taskAssignment: bool(integrations.taskAssignment, DEFAULT_DECISION_CONFIG.integrations.taskAssignment),
      contextRelevance: bool(
        integrations.contextRelevance,
        DEFAULT_DECISION_CONFIG.integrations.contextRelevance,
      ),
    },
    compaction: {
      enabled: bool(compaction.enabled, DEFAULT_DECISION_CONFIG.compaction.enabled),
      sticky: bool(compaction.sticky, DEFAULT_DECISION_CONFIG.compaction.sticky),
    },
    stagedReview: {
      enabled: bool(stagedReview.enabled, DEFAULT_DECISION_CONFIG.stagedReview.enabled),
      streamingPreCheck: bool(
        stagedReview.streamingPreCheck,
        DEFAULT_DECISION_CONFIG.stagedReview.streamingPreCheck,
      ),
    },
    retrieval: {
      enabled: bool(retrieval.enabled, DEFAULT_DECISION_CONFIG.retrieval.enabled),
    },
  };
}

function defaultStorage(): ConfigStorage | null {
  const candidate = (globalThis as { localStorage?: ConfigStorage }).localStorage;
  return typeof candidate?.getItem === "function" ? candidate : null;
}

/** 读配置：存储里的 JSON 合并到默认值上。存储缺席（纯 Node 测试）或解析失败都落回默认值 */
export function loadDecisionConfig(storage?: ConfigStorage): DecisionLayerConfig {
  const store = storage ?? defaultStorage();
  if (!store) return structuredClone(DEFAULT_DECISION_CONFIG);
  try {
    const raw = store.getItem(DECISION_CONFIG_STORAGE_KEY);
    if (!raw) return structuredClone(DEFAULT_DECISION_CONFIG);
    return mergeDecisionConfig(JSON.parse(raw));
  } catch {
    return structuredClone(DEFAULT_DECISION_CONFIG);
  }
}

export function saveDecisionConfig(config: DecisionLayerConfig, storage?: ConfigStorage): void {
  const store = storage ?? defaultStorage();
  if (!store) return;
  store.setItem(DECISION_CONFIG_STORAGE_KEY, JSON.stringify(config));
}
