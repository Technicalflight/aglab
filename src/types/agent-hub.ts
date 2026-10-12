/**
 * Agent Hub（助手库）统一模型与适配器契约——设计稿 docs/agent-hub-design.md 的
 * 第 2 步代码形态。原则：适配器驱动、本地优先、增量同步、只读优先、用户授权。
 *
 * 本文件的接口是**协议形状**：Tauri 命令的 payload 类型与前端渲染模型。
 * 真正的探测/解析实现住 Rust 侧（src-tauri/src/agent_hub/，trait AgentAdapter），
 * 两边以这组类型对齐——Rust 序列化后的 JSON 必须能被这组类型原样接住。
 *
 * 命名约定：所有标识符用中性代称（外部 Agent / 内置识别清单），
 * 具体识别目标由数据清单（registry）注入，不进代码。
 */

/** 适配器稳定代号（registry 注入的 slug），入库主键与跨会话引用都用它 */
export type AgentSlug = string;

/** 发现置信度：高（端态路径与配置同时命中）/ 中（部分命中）/ 低（仅特征弱命中） */
export type DetectConfidence = "high" | "medium" | "low";

/** 候选来源：内置识别清单 / 运行中的进程 / 用户手动添加 */
export type DetectSource = "builtin" | "process" | "manual";

/** 发现器输出的候选条目：用户在 UI 勾选确认后才入库（enabled=1），不静默导入 */
export interface AgentCandidate {
  slug: AgentSlug;
  displayName: string;
  installPath?: string;
  configPath?: string;
  sessionRoot?: string;
  memoryPaths: string[];
  confidence: DetectConfidence;
  /** 关键路径可读（存在且已获授权）才可导入 */
  readable: boolean;
  source: DetectSource;
  /** 命中证据（脱敏后的路径描述），展示给用户看为什么算它候选 */
  evidence: string[];
}

/** 探测回包：detect() 的统一形状 */
export interface DetectResult {
  slug: AgentSlug;
  displayName: string;
  installed: boolean;
  confidence: DetectConfidence;
  /** 证据（脱敏）：例如"配置目录存在且含 N 个会话文件" */
  evidence: string[];
  configPaths: string[];
  sessionRoots: string[];
  memoryPaths: string[];
}

/** 解析后的配置（只读视图；字段缺失是常态，全部可选） */
export interface AgentConfig {
  slug: AgentSlug;
  /** 原样保留的原始配置对象（脱敏后：密钥类字段只留"已配置/未配置"布尔） */
  raw: Record<string, unknown>;
  defaultModel?: string;
  /** 其余语义化字段按适配器补充；未知字段进 raw */
}

/** 消息内容块类型：文本 / 工具调用 / 工具结果 / 附件 / 思考过程 */
export type MessagePartType = "text" | "tool_call" | "tool_result" | "attachment" | "thinking";

/** 统一内容块：一段消息可由任意多个块按序组成（多模态与工具链都靠它表达） */
export interface UnifiedMessagePart {
  type: MessagePartType;
  /** type=text|thinking 的正文 */
  text?: string;
  toolName?: string;
  /** JSON 字符串：调用参数 / 结构化结果 */
  toolInput?: string;
  toolOutput?: string;
  mime?: string;
  /** 大附件落盘后的引用（data_dir/blobs/ 内相对路径），小内容直接进 text */
  blobPath?: string;
}

export type MessageRole = "user" | "assistant" | "system" | "tool";

/** 统一消息：流式过程中的增量块在导入侧折叠为最终形态 */
export interface UnifiedMessage {
  /** 原始消息 id；缺失时导入侧用 内容哈希+时间戳 兜底去重 */
  originalMessageId?: string;
  role: MessageRole;
  createdAt?: number;
  model?: string;
  parentMessageId?: string;
  parts: UnifiedMessagePart[];
}

/** 统一会话：JSONL / JSON / SQLite / Markdown / 目录树 等原始形态归一后的产物 */
export interface UnifiedSession {
  agentSlug: AgentSlug;
  originalSessionId: string;
  projectPath?: string;
  title?: string;
  startedAt?: number;
  updatedAt?: number;
  model?: string;
  tokenUsage?: number;
  /** 原始文件路径（只读引用）——"查看原始文件"入口 */
  rawPath?: string;
  messages: UnifiedMessage[];
}

export type MemoryKind = "instruction" | "memory" | "rules";

/** 统一记忆文档：CLAUDE.md/AGENTS.md 类项目说明、memory 目录条目、规则文件等 */
export interface MemoryDoc {
  agentSlug: AgentSlug;
  path: string;
  kind: MemoryKind;
  /** 项目根的相对路径；用户级记忆为 null */
  projectPath?: string;
  content: string;
  updatedAt?: number;
}

/**
 * 适配器契约（TS 形状）。实现住 Rust（trait AgentAdapter）；这里保留任务书的
 * 接口签名作为跨语言对齐基准——每个方法的参数与返回都必须能映射到本文件的类型。
 */
export interface AgentAdapter {
  id: AgentSlug;
  displayName: string;
  detect(): Promise<DetectResult>;
  getConfigPaths(): string[];
  parseConfig(path: string): Promise<AgentConfig>;
  getSessionRoots(): string[];
  parseSession(file: string): Promise<UnifiedSession>;
  getMemoryPaths(projectPath?: string): string[];
  parseMemory(file: string): Promise<MemoryDoc>;
  /** 阶段 C：支持恢复原会话的适配器返回子进程命令行；不支持的返回 null（建新会话附摘要） */
  getResumeCommand?(session: UnifiedSession): Promise<string[] | null>;
  /** 增量监听路径（Rust 侧 notify 实际监听这些根） */
  watchPaths(): string[];
}

/** 同步进度事件（Tauri event payload）：不因单个坏文件中断整体任务 */
export interface SyncProgressEvent {
  runId: string;
  /** 总文件数 / 已处理数 / 跳过数 / 失败数 */
  total: number;
  processed: number;
  skipped: number;
  failed: number;
  currentPath?: string;
  done: boolean;
}

/** 跳过与失败的原因记录（用户可查，但不刷屏） */
export interface SyncIssue {
  runId: string;
  path: string;
  reason: string;
  kind: "skipped" | "failed";
}
