/**
 * System 1 决策层的类型面。三个来源在这里对齐：
 * - Laya 本地模型（@receptron/laya / python laya-mlx）的 systemOne 契约
 * - Jev 云端（TypeSafe `POST /v1/systemone`）的请求/响应形状
 * - 本项目路由器与审计需要的附加字段
 *
 * 原则：决策即函数调用——Provider 不生成文本，返回的就是结构化概率，
 * 代码分支直接用返回值，任何一层都不做字符串解析。
 */

/** 三种决策原语。choice 选一项、score 在有序量表上取期望、noul 给是/否的 P(true)。 */
export type QuestionType = "choice" | "score" | "noul";

export interface ChoiceQuestion {
  type: "choice";
  instructions: string;
  /** 选项 ID → 人读描述。选项 ID 就是代码分支要判断的那个值 */
  criteria: Record<string, string>;
}

export interface ScoreQuestion {
  type: "score";
  instructions: string;
  /** 有序量表，从最低档到最高档。score 是期望档位（可落在两档之间） */
  criteria: Array<string | number>;
}

export interface NoulQuestion {
  type: "noul";
  instructions: string;
}

export type Question = ChoiceQuestion | ScoreQuestion | NoulQuestion;

/**
 * 敏感级别。`confidential` 的 state 一辈子不出本机——路由器看到它就直接把
 * Jev 和 System 2（也是云端）从候选里划掉，这不是可用性权衡，是红线。
 */
export type Sensitivity = "public" | "private" | "confidential";

/** 决策漏斗的三层。fallback 是 System 2 LLM（Phase 3 接线），不生成概率时就别冒充。 */
export type ModelTier = "laya" | "jev" | "fallback";

export interface DecisionRequest {
  /** 状态文本或结构化对象。走 Jev 时对象会被序列化成字符串（它的 API 只收文本） */
  state: string | Record<string, unknown>;
  /** 一次请求带全部问题——每个问题在同一次前向里批量出答案，这是零输出 token 的关键 */
  questions: Record<string, Question>;
  /** 默认 auto（走三级漏斗）。显式指定 `laya` 失败时**不会**静默降级到云端——见路由器 */
  modelPreference?: "auto" | ModelTier;
  /** 默认 public。审计的脱敏强度跟着这一档走 */
  sensitivity?: Sensitivity;
  /**
   * 判定属于哪场对话。只用于观测（决策面板按话题过滤），不参与路由与脱敏的取舍；
   * 不带的判定（试一次、记忆分级这类全局治理）在面板里不落进任何一场对话的流
   */
  conversationId?: string;
  /** 单层超时。不填用 Provider 自己的默认值 */
  timeoutMs?: number;
}

export interface DecisionAnswer {
  questionName: string;
  type: QuestionType;
  // choice
  choice?: string;
  /**
   * 档位/选项 → 概率的表。上游就是这个形状（choice 键是选项 ID，score 键是档位标签），
   * 不是一条有序数组——所以"第几档"要按问题自己的 criteria 去对，别在这里猜顺序
   */
  probabilities?: Record<string, number>;
  // score
  score?: number;
  // noul
  noul?: number;
  /**
   * 置信度 0-1，语义是「这个答案有多值得直接采信」：
   * **优先用模型自带的 `confidence`**（choice 是 1 − 归一化熵，且上游按选项数分档调过
   * 温度 `temperature_by_options`，那把尺是出厂口径）。模型没给才自己算：
   * noul 取 max(P(true), 1-P(true))（上游确实不带这一格），choice 取选中项概率，
   * score 取分布峰值占比，什么都没有是 0.5。
   * 路由器的升级阈值比的是这一列的**最小值**（木桶效应）。
   */
  confidence: number;
  model: ModelTier;
  latencyMs: number;
}

export interface DecisionResponse {
  answers: Record<string, DecisionAnswer>;
  model: ModelTier;
  totalLatencyMs: number;
  cacheHit: boolean;
  /**
   * 本项目附加字段：结果低于升级阈值（或漏斗半路塌了）仍然返回了最好的可得答案。
   * 调用方看到 degraded=true 就该低头看一眼 confidence 再决定采不采信。
   */
  degraded?: boolean;
  /**
   * V2 按问溯源（§4）：每个问题最终采纳的答案来自哪一层。
   * 整批响应与 response.model 一致；混合批次（laya 为主、低置信子集送 Jev 补答）
   * 逐问标注——调用方与审计按问溯源，不再只有「整批来自哪层」一个粗粒度。
   * 缺省（老响应形状）= 只能信 model 字段
   */
  sources?: Record<string, ModelTier>;
}

/** 决策提供者。name 必须是三层之一——漏斗的顺序由敏感性与隐私决定，不由 priority 决定 */
export interface DecisionProvider {
  readonly name: ModelTier;
  readonly isAvailable: boolean;
  decide(request: DecisionRequest): Promise<DecisionResponse>;
  /** 启动预热：加载模型 / 探测服务商。实现应当幂等且永不抛出（失败 = 不可用，不是崩溃） */
  warmup?(): Promise<void>;
}

export interface DecisionRouter {
  decide(request: DecisionRequest): Promise<DecisionResponse>;
  /** 按 name 替换对应层的 Provider；priority 保留参数位——漏斗顺序是定死的 */
  registerProvider(provider: DecisionProvider, priority?: number): void;
  setFallback(provider: DecisionProvider): void;
}

/** 决策链路：这次 decide 实际问过哪几层（按问的顺序）。缓存命中时是空数组 */
export type DecisionModelChain = ModelTier[];

/**
 * 漏斗里每一层的下场。
 * - `excluded` 根本没进这一轮的候选（红线或偏好），与"进去问了但没答上"是两件事
 * - `not-reached` 在候选里，但前面已经收工或撞上升级链上限，没轮到它
 */
export type DecisionAttemptOutcome =
  | "answered"
  | "below-threshold"
  | "failed"
  | "unavailable"
  | "excluded"
  | "not-reached";

export interface DecisionAttempt {
  tier: ModelTier;
  outcome: DecisionAttemptOutcome;
  /** 这一层自己花掉的时间。没被问过的那一层是 0，不是"快得数不清" */
  latencyMs: number;
  /** answered / below-threshold 时这一层给出的最小置信（木桶那块板） */
  minConfidence?: number;
  /** 为什么没成、为什么没问。failed 是错误原文，excluded 是挡它的那条规矩 */
  reason?: string;
}

export interface DecisionTrace {
  id: string;
  timestamp: string;
  /** 审计入口已做脱敏：confidential 只剩长度+哈希，private 截断预览（见 audit.ts） */
  request: DecisionRequest;
  /** null = 所有层都失败，没有可返回的答案 */
  response: DecisionResponse | null;
  modelChain: DecisionModelChain;
  /**
   * 逐层过程：候选之内每一层的下场，加上被红线/偏好挡在门外的那些。
   * 只有 modelChain 的话，"laya 到底是不是一开始就没起"这个问题没有答案——
   * 而它恰恰是这块面板最常见的用途。缓存命中时是空数组（什么都没问）。
   */
  attempts: DecisionAttempt[];
  totalLatencyMs: number;
  cacheHit: boolean;
  /** 逐层的不可用/失败原因，`层:原因` 用 | 连接。没有错误就没有这个字段 */
  errors?: string;
}
