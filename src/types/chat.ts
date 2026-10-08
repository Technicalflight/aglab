export type MessageRole = "user" | "assistant";

/** 这一轮里做过的一步。顺序就是它真实发生的顺序：思考一段、一个工具、又思考一段…
 *
 *  只给界面看，不落盘（与 `streaming` 同命）——所以应用重启后那一轮退回按
 *  `reasoning` + `toolCalls` 拼出来的形状，而不是假装还知道谁先谁后 */
export type RunStep =
  /** 一段思考。`from` 是它在 `message.reasoning` 里的起始字符位：正文只存那一份，
   *  这里不抄第二份，否则两段思考各存一遍就会有一份开始说谎。
   *  `contentChars` 是它开始时正文已流出的码元数（流式内联的切片点） */
  | { kind: "thinking"; id: string; at: number; from: number; contentChars?: number }
  /** 一次工具调用。摘要与状态都读 `message.toolCalls` 里同 id 的那一条 */
  | { kind: "tool"; id: string; callId: string; contentChars?: number };

export interface Message {
  id: string;
  role: MessageRole;
  content: string;
  createdAt: number;
  streaming?: boolean;
  reasoning?: string;
  reasoningStreaming?: boolean;
  toolCalls?: ToolCall[];
  error?: string;
  /** 上下文压缩生成的摘要消息。压缩后它替代被压缩掉的旧消息，衔接后续对话 */
  summary?: boolean;
  /** 客户端自己产生的回执（记忆命令的结果）。它没进过模型，也不会再进模型，
   *  所以它不该有"重新生成"，也不该被算进标题和提取素材里 */
  note?: boolean;
  /** 这一条在后端话题日志里对应的条目 id（按登记顺序）。
   *  重新生成与编辑重发要指名"把分支末端移到哪条之后"，那个名字只有后端知道 */
  entryIds?: string[];
  /** 分支树上的父节点（哪一条气泡之后长出来的）。
   *  **由后端铸造、前端抄录**：日志里的父指针在追加时定，这里只是把它抄进界面状态。
   *  缺省/空 = 这一支的根。旧存档整份都没有它，那时插入序就是那条链 */
  parentId?: string | null;
  /** 视频画布节点：这条消息属于哪个节点（只有视频会话写）。缺省 = 老消息，
   *  前端把它归到第一个节点名下 */
  nodeId?: string;
  /** 这一轮的执行流程（界面专属，不落盘）。空/缺 = 这一轮还没开始动手，或它是从盘上读回来的 */
  steps?: RunStep[];
  /** 从发出到收尾，这一轮在界面上坐了多久（毫秒）。界面专属，不落盘 */
  durationMs?: number;
  /** 后端自己接的那一轮是第几轮（目标续跑）。有值就在气泡头上标「目标 · 第 N 轮」。
   *  它由广播读数派生、只活在界面上：日志里那一行是发给模型的，不该冒充一句用户发言 */
  goalRound?: number;
  /** 这条用户消息带的附件（发送时挂上，气泡里渲染）。落盘前剥掉 previewDataUrl——
   *  那是几百 KB 的 data URL，进存档就是膨胀；重开话题走 asset 协议从 path 读 */
  attachments?: MessageAttachment[];
  /** 答出这一条的模型名，由后端的 done 读数带来（`ChatEvent.model`）。
   *  缺 = 这一格加进来之前的历史，当时没人记过用它的是谁——那一格宁可空着，
   *  也不拿现在的配置顶上去冒充当时的读数 */
  model?: string;
  /** 生成会话的占位标记（仅前端，重载即逝）：加载卡按它说话。
   *  视频画布的节点四类生成（文本/图片/视频/音频）都用它 */
  media?: "text" | "image" | "video" | "audio" | "transcribe" | "music";
}

export interface MessageAttachment {
  name: string;
  /** audio = 音频生成的产物（消息气泡上用 <audio> 播放） */
  kind: "text" | "image" | "video" | "audio";
  path: string;
  bytes: number;
  /** 内存态缩略图（data URL）。只在当前话题的内存里活着 */
  previewDataUrl?: string;
}

/** 发给服务商的历史消息。toolCalls/toolCallId 用于话题恢复时重放工具历史 */
export interface ChatMessage {
  role: MessageRole | "tool";
  content: string;
  toolCalls?: Array<{ id: string; name: string; arguments: string }>;
  toolCallId?: string;
}

export type ToolStatus = "pending" | "running" | "done" | "denied" | "failed";
export type ToolRisk = "safe" | "elevated" | "high";

export interface ToolCall {
  id: string;
  name: string;
  status: ToolStatus;
  risk: ToolRisk;
  input: string;
  output?: string;
  /** 原始参数 JSON 文本。落盘后用于话题恢复时的历史重放 */
  arguments?: string;
  /**
   * 这一发声明时，本轮正文已经流出了多少个 UTF-16 码元（后端在派发前盖章）。
   * 有它，工具行就能插回原文流（文字→工具→文字的流式内联）；
   * null / 缺字段 = 加这一格之前落的历史，按堆叠布局显示
   */
  contentChars?: number | null;
  /**
   * 这一发**该问而没问**：凭什么过的闸，后端给的一句人话（"按本话题规则放行"…）。
   * null / 缺字段 = 要么权限表本来就放行，要么刚刚弹过窗。文案不在前端抄一份
   */
  passReason?: string | null;
}

export interface Usage {
  inputTokens: number;
  outputTokens: number;
  /** 命中服务商缓存的输入部分。null = 服务商没回这个字段，不等于命中 0 */
  cachedTokens: number | null;
  durationMs: number;
  /** 这一发实际生效的上下文窗口（后端随 Done 带回）。历史台账里没有这一格 →
   *  undefined，用量面板退回顶层配置的窗口读数 */
  contextTokens?: number;
  /** 这一发实发的模型名。换了模型之后，旧台账的窗口读数不再代表"下一发"，
   *  用量面板要让它让位给按新模型解析的静态值 */
  model?: string;
}

/** 落盘用的一条话题。streaming/reasoningStreaming 不在其中，恢复时不会留下"正在生成" */
/** 会话的能力档：对话走普通管线；生图/视频是"为生成而开"的会话——
 *  占位词、模型提示与侧栏标识随之切换，实际生成取决于所选模型的能力 */
export type ConversationKind = "chat" | "image" | "video" | "music";

/** 生图会话的生成参数（随请求原样发给上游） */
export interface ImageGenSettings {
  size: string;
  quality: string;
  count: number;
}

/** 视频会话画布的生成参数（对照即梦"16:9 · 720P · 5S"）。
 *  随生成请求原样发给上游（New API 视频文档口径），支不支持由上游/模型决定 */
export interface VideoGenSettings {
  /** 生成模式："omni" 全能参考（图片作参照）｜"frames" 首尾帧｜"edit" 视频编辑 */
  mode: "omni" | "frames" | "edit";
  ratio: string;
  resolution: string;
  duration: number;
}

export interface ConversationRecord {
  id: string;
  projectId: string;
  title: string;
  createdAt: number;
  updatedAt: number;
  /** 用户置顶。落盘必带：漏了它 Rust 会按 default 落成 false，置顶就被悄悄洗掉 */
  pinned: boolean;
  /** 能力档。落盘必带：漏了它 Rust 会按 default 落成 "chat"，生图会话被洗成对话 */
  kind: ConversationKind;
  /** 视频画布的节点登记（只视频会话有）。旧存档没有这一格 → undefined，
   *  画布从消息行的 nodeId 还能认领回节点 */
  videoNodes?: VideoNode[];
  /** 节点连接（有向 from→to）。下游生成时自动引用上游的最新产物 */
  videoEdges?: Array<{ from: string; to: string }>;
  messages: Message[];
  usage?: Usage;
}

/** 节点/生成会话里一发请求的产物类型。视频画布的四类页签按它分流；
 *  transcribe = 音频页签的"语音转写"子模式（音频进，文字出） */
export type MediaType =
  | "text"
  | "image"
  | "video"
  | "audio"
  | "transcribe"
  | "music";

/** 视频会话画布上的一个节点：登记（id/名/建时），内容长在消息的 nodeId 上 */
export interface VideoNode {
  id: string;
  label: string;
  createdAt: number;
}

export interface ConversationMeta {
  id: string;
  title: string;
  projectId: string;
  updatedAt: number;
  messageCount: number;
  preview: string;
  pinned: boolean;
  kind: ConversationKind;
}

/** 分级表里的一档。`scoped` 是"在声明的范围内自动放行，越界退化成 ask" */
export type PermissionLevel = "deny" | "ask" | "scoped" | "allow";

/**
 * 一条话题现在怎么干活。它与权限档位是**两个独立的轴**：档位问"这一下要不要有人点头"，
 * 模式问"这一支准不准动手"。规划模式那条红线与档位无关，切成完全访问也开不动它
 */
export type WorkingMode = "chat" | "plan" | "goal";

/**
 * 目标现在处于哪一格。六值，`paused` 是其中一格而不是旗子——
 * 从前 `outcome` 配一个 `paused` 布尔，两格要互相解释才说得清"这一支怎么样了"，
 * 于是界面上长出"待命"这种哪儿都不是的第五个词。三件失败也各不相关：
 * `blocked` 的出路是改目标、`budget_limited` 是调上限、`usage_limited` 是换档案
 */
export type GoalStatus =
  | "active"
  | "paused"
  | "blocked"
  | "usage_limited"
  | "budget_limited"
  | "complete";

/**
 * 一条判据在界面上的投影。`risk` 由后端查 `tools::classify`（那张"动不动东西"的表
 * 是唯一出处）；`safe` 的 check 收尾时会被运行时复跑，其余只接上报——
 * 这个差别要在人写下的那一刻看得见（design-goal-mode.md §5.3 的契约自检）
 */
export interface CriterionView {
  id: string;
  text: string;
  kind: "check" | "judgment";
  command: string | null;
  risk: "safe" | "elevated" | "high";
  /** open=还没证据；runtime=复验过；reported=仅上报；failed=最新的证据是失败 */
  evidence: "open" | "runtime" | "reported" | "failed";
}

/** 完成契约的界面投影。判据从 0 条到 N 条，弹框、目标带与完成门读的都是这一份 */
export interface ContractView {
  criteria: CriterionView[];
  constraints: string[];
}

/**
 * 后端从话题日志里算出来的作业模式读数。前端只读它、不存第二份——
 * 模式的事实只有日志那一条，切话题与重新生成都会带回那一支当时的模式
 */
export interface ModeState {
  mode: WorkingMode;
  /**
   * 这一支身上挂着的目标。它与 `mode` 是两件事：目标属于话题，`mode` 只是当下怎么交互，
   * 所以 `mode === "chat"` 时这一格可以照旧有值、照旧在推进。null = 没有目标
   */
  objective: string | null;
  /** 目标模式自己续跑过几轮。它是读数不是配额——目标没有轮次上限；用户自己发的消息不占这一格 */
  turnsUsed: number;
  /** 花费上限，单位 1e-8 美元（与台账同一把尺）。0 = 不设。这是唯一的自动刹车 */
  maxCostUsdE8: number;
  /** 起目标之后实花的那一笔，同样从台账读。null = 账读不出来，界面上是 `—` 而不是 $0 */
  spentUsdE8: number | null;
  /** 目标现在处于哪一格。六值见 [`GoalStatus`]——暂停就是它自己的一格，不再是别格上的旗子 */
  status: GoalStatus;
  /** 收尾那一句：完成时交付了什么，停住时卡在哪 */
  note: string | null;
  /**
   * 目标点名执行的服务商档案 id（`profiles` 里那张卡片的 id）。
   * null = 跟随当前配置。目标与档案是引用关系：档案改了连接，目标跟着新连接走
   */
  profile: string | null;
  /**
   * 目标的身份。立约时铸的、之后不改；旧行没有身份时后端读侧补铸 `legacy-<id>`。
   * 界面拿它给分叉出的同一支目标分组（design-goal-mode.md §5.5）
   */
  goalId: string | null;
  /** 完成契约的投影。null = 旧式目标（无判据），完成门对它退化、界面标「无判据」 */
  contract: ContractView | null;
  /**
   * 规划模式且模型已经把话说完了。那条「批准并执行」只看这一格——
   * 它由后端从日志派生（最新一条用户发言之后有没有一条以文本收尾的回答），
   * 前端不猜"这像不像一份方案"
   */
  planReady: boolean;
}

/**
 * 目标面板里的一条。它是投影不是真相：读数的原文在话题日志与台账里，
 * 这里由 store 从现场（LiveRun）、读数事件与「继续」开出的广播里攒出来。
 *
 * 目标挂在**话题**上、与交互档是两件事：`mode` 是对话档时这一条照样在推进，
 * 所以这里没有"挂起"那一格——切档不清零，也就没有第二个落点要认
 */
export interface GoalPanelEntry {
  conversationId: string;
  title: string;
  objective: string;
  /** 这条话题当下的交互档。目标可以挂在任何一档上 */
  mode: WorkingMode;
  /** 目标模式自己续跑过几轮。读数不是配额 */
  turnsUsed: number;
  /** 花费上限，1e-8 美元。0 = 不设 */
  maxCostUsdE8: number;
  /** 起目标之后的实花，同样从台账读。null = 账读不出来（`—`），不是 0 */
  spentUsdE8: number | null;
  status: GoalStatus;
  /** 点名的服务商档案 id。null = 跟随当前配置 */
  profile: string | null;
  /** 目标的身份。角落卡按它把分叉出的同一支合成一组 */
  goalId: string | null;
  /** 收尾那一句：完成时交付了什么，受阻时卡在哪 */
  note: string | null;
  /** 这一轮的服务商还在出字（含「继续」开出的那一轮） */
  pending: boolean;
  /** 最近输出的尾巴。推进中它跟手，收尾后是最后一截 */
  tail: string;
}

/** 一条还在跑的后台命令。「后台」小卡片按 owner（启动它的话题）分桶清点 */
export interface RunningCommand {
  id: number;
  command: string;
  owner: string;
}

/** 模式选择器那三档。`description` 是说给人听的，与后端那段说给模型的正文各管一头 */
export const MODE_LEVELS: Array<{
  value: WorkingMode;
  label: string;
  description: string;
}> = [
  {
    value: "chat",
    label: "对话",
    description: "照权限档位走：该问的问，该放的放。挂着的目标在后台照常推进。",
  },
  {
    value: "plan",
    label: "规划",
    description: "只读研究，不改文件、不跑命令；交一份方案，等你批准。挂着的目标这期间暂停。",
  },
  {
    value: "goal",
    label: "目标",
    description: "给定一个目标，一轮一轮自己推进到完成，随时可停。",
  },
];

/**
 * 一条覆盖项。`key` 是 capability 键的前缀（`file.write.projectRoot`、`exec`、`tool.read_file`…），
 * 而且只能往严了改：全局档是天花板，项目那一层也撤不掉全局划的红线。
 * 写错的键不会命中任何判定，所以后端在存之前就拒（`is_known_key`）
 */
export interface PermissionOverride {
  key: string;
  level: PermissionLevel;
}

/** 文件安全规则的一条（design-security-center.md D2）：路径前缀 × 读/写/删三档动作。
 * 判定顺序是项目表在前、全局表在后，首条命中即停；未命中的路径落回现行权限档 */
export type FileRuleAction = "deny" | "ask" | "allow";

/** 用户自建的敏感检测规则（id 由前端生成，custom- 前缀） */
export interface CustomSecretRule {
  id: string;
  label: string;
  pattern: string;
}

/** 对内置检测规则正则的改写：只许改正则，不许改名 */
export interface SecretRulePatternEdit {
  id: string;
  pattern: string;
}
export interface FileRule {
  /** 路径前缀（目录语义，覆盖子树），可以用 %USERPROFILE% 这类环境变量 */
  pattern: string;
  read: FileRuleAction;
  write: FileRuleAction;
  delete: FileRuleAction;
}

/** 权限表上的一行：这一刻生效的档位，以及这个数是哪一层给的 */
export interface PermissionRow {
  key: string;
  level: PermissionLevel;
  source: "档位" | "全局" | "项目";
}

/** 命令安全规则的一条（design-security-center.md D4）：命令行前缀 + 命中后的动作。
 * 拒绝不在表上——拒绝的语义由黑名单承担；段间取最严 */
export interface CommandRule {
  prefix: string;
  action: FileRuleAction;
}

export interface Project {
  id: string;
  name: string;
  path: string;
  /** 这个项目额外拦哪几行。只能比全局更严 */
  permissionOverrides: PermissionOverride[];
  /** 这个项目的文件安全规则：判定时排在全局表**之前**（首条命中即停） */
  fileRules: FileRule[];
  /** 这个项目的命令前缀规则：同样排在全局表之前。黑名单是机器级的，不进项目 */
  commandRules: CommandRule[];
}

export interface Attachment {
  id: string;
  name: string;
  path: string;
  chars: number;
  truncated: boolean;
  /** 缺省 = 文本附件（读内容进上下文）。image = 粘贴的截图/参考图；
   *  video = 视频编辑模式的素材视频；audio = 音频生成的产物 */
  kind?: "text" | "image" | "video" | "audio";
  /** 图片 chip 的缩略图（data URL，只在内存里活着——发送后附件即清空，不落盘不进历史） */
  previewDataUrl?: string;
  /** 文本附件的正文（≤128KB）：视频会话的剧本靠它拼进生成提示词——生成管线没有
   *  对话历史，这是剧本进"上下文"的唯一通道。图片恒为空串 */
  text?: string;
}

export interface UiState {
  sidebarCollapsed: boolean;
  panelCollapsed: boolean;
  panelTab: PanelTab;
  section: SidebarSection;
  diffLayout: DiffLayout;
}

/** 侧边栏分区。工具/技能/用量收进了「设置」页，决策读数住右栏「决策」标签——侧栏只留工作分区 */
export type SidebarSection =
  | "chats" | "review" | "tasks" | "plugins" | "knowledge" | "settings";

export const SECTION_IDS: readonly SidebarSection[] = [
  "chats",
  "review",
  "tasks",
  "plugins",
  "knowledge",
  "settings",
];

/** 分区来自持久化配置，坏值要退回话题而不是让界面空白 */
export function isSection(value: string): value is SidebarSection {
  return (SECTION_IDS as readonly string[]).includes(value);
}

/** 对话记录落在本地的哪种介质上。配置本身不受影响，始终写在 config.json */
export type ConversationStore = "json" | "sqlite";
export type WireFormat = "chat" | "responses" | "anthropic" | "gemini";

export interface StorageInfo {
  backend: ConversationStore;
  jsonDir: string;
  jsonCount: number;
  sqliteFile: string;
  sqliteCount: number;
}

export interface StorageSwitch {
  config: AppConfig;
  info: StorageInfo;
  /** 从旧存储拷过来的话题条数 */
  moved: number;
}

/** 一套连接里**某个模型自己**的那一份读数。档案级那几格是默认，命中这张表的行盖上去。
 *  为什么要有它：一张档案可以挂好几个模型（池成员就是"档案 id + 模型名"），而窗口、
 *  最大输出、思考档是模型的属性不是服务商的 */
export interface ModelSpec {
  /** 模型名，原样匹配（与 proxyByModel / cacheTtlByModel 同一口径） */
  model: string;
  /** 上下文窗口。0 = 这一格没填，用档案级默认 */
  contextTokens: number;
  /** 最大输出。0 = 没填，用档案级默认 */
  maxTokens: number;
  /** 思考档。null = 没填，用档案级默认；"" = 明确不向服务商发送该字段 */
  reasoningEffort: string | null;
  /** 这个模型可选的思考档。空 = 全部档位都可选 */
  effortLevels: string[];
  supportsImages: boolean;
  /** 收不收视频本体（视频理解）。没勾就只在正文里写路径与大小 */
  supportsVideo?: boolean;
  /** 收不收音频本体（音频理解）。没勾就只在正文里写路径与大小 */
  supportsAudio?: boolean;
  /** AI 起的回合能不能被调度到这一行。false = 只有界面聊天与明确点名会用它 */
  delegatable: boolean;
  /** 能力档（chat/image/video）。不填按模型名启发式识别（seedream→生图、kling→视频…） */
  capabilities?: string[];
}

/** 一条服务商档案：一套可整体切换的连接配置。密钥凭据对跟着档案走（密钥本体只进凭据管理器） */
export interface EndpointProfile {
  id: string;
  name: string;
  baseUrl: string;
  model: string;
  apiFormat: string;
  reasoningEffort: string;
  temperature: number;
  maxTokens: number;
  contextTokens: number;
  autoCompact: boolean;
  /** 这套连接里各模型自己的读数。档案级那几格是它的默认 */
  models: ModelSpec[];
  promptCacheKey: boolean | null;
  cacheTtlSeconds: number | null;
  cacheTtlByModel: Record<string, number>;
  credentialService: string;
  credentialUser: string;
  /** 代理绑定："" 继承全局 / "direct" 直连 / "pool" 代理池 / 代理 id（连接域字段，随切换抄写） */
  proxy: string;
  /** 服务商内按模型覆盖代理，键是模型名（值域同 proxy） */
  proxyByModel: Record<string, string>;
}

/** 一条代理。url 形如 http://host:port、http://user:pass@host:port 或 socks5://… */
export interface ProxyEntry {
  id: string;
  name: string;
  url: string;
  enabled: boolean;
  /** 轮询与随机两档按它分配（1..=100，钳位在 Rust 读侧那一处）。自适应与最少使用不看它 */
  weight: number;
}

/** 代理池：绑定值为 "pool" 的请求在启用的代理之间按策略均衡（解析与账本在 Rust proxy.rs） */
export interface ProxyPool {
  /** round_robin（平滑加权轮询）| random（加权随机）| least_used（当前并发最少）| adaptive（并发×响应头耗时） */
  strategy: string;
  proxies: ProxyEntry[];
}

export interface AppConfig {
  baseUrl: string;
  model: string;
  /** 线协议：chat=/chat/completions，responses=/responses，anthropic=/v1/messages。中转站常常只开一条 */
  apiFormat: WireFormat;
  /** 空串 = 默认（不向服务商发送 reasoning_effort） */
  reasoningEffort: string;
  temperature: number;
  maxTokens: number;
  /** 模型上下文窗口（tokens），只用于界面估算上下文用量百分比 */
  contextTokens: number;
  /** 项目约定文件（AGENTS.md / CLAUDE.md）最多注入多少字符。超出只留开头。0 = 不设上限 */
  projectRulesMaxChars: number;
  /** 一条工具结果最多留多少字符，超出留头 3/4 与尾 1/8。0 = 不设上限 */
  toolResultMaxChars: number;
  /** 本轮检索出的记忆段超过这么多字符就整段不发（截一半等于伪造记忆）。0 = 不设上限 */
  memorySectionMaxChars: number;
  /** 发送前上下文超过窗口 90% 时自动压缩成摘要再继续 */
  autoCompact: boolean;
  /** "当前连接"这张看不见的档案里各模型自己的读数。档案上的同名表整体覆盖它 */
  models: ModelSpec[];
  /** 缓存保温：服务商缓存到期前用一次 max_tokens=1 的重放续上。默认关 */
  cacheWarming: boolean;
  /** 重复循环护栏：模型复读退化时流式检测并自动截断，省下循环后半段的 token。默认开 */
  repetitionGuard: boolean;
  /** 自动检查更新：每 24 小时联网查一次，发现新版本弹窗提醒。默认开 */
  autoUpdateCheck: boolean;
  /** 上次自动检查的时刻（epoch 毫秒），0 = 从没查过 */
  lastUpdateCheckAt: number;
  /** 是否在消息里显示模型的思考过程 */
  showReasoning: boolean;
  /**
   * 服务商回 429（限流）时无限重试：指数退避（2ⁿ 秒，封顶 60 秒），直到成功或用户按停止。
   * 默认关——"无限"意味着这一发可能永远不结束，用户没点头之前不这么做
   */
  unlimitedRetry429: boolean;
  /**
   * 重启后自动继续挂着的目标。默认关——那一格管的是"要不要继续花钱"，归人决定，
   * 不归一次程序重启替他重按播放键。关着时启动把 active 落成 paused 并说一声
   */
  goalResumeOnLaunch: boolean;
  /** 单回合工具调用轮数上限。0 = 不设上限；非 0 到顶停止 */
  maxToolRounds: number;
  /** 跨 plan 的全局并发上限：这台机器上所有编排计划加起来最多同时跑几路。0 = 不设上限 */
  totalParallel: number;
  /** 审计日志的保留天数。过期的整天分片整片搬进 `audit/archive/`，不删除 */
  auditKeepDays: number;
  /** 缓存身份能力的显式覆盖。null = 交给内置能力表（表里没有的服务商一律当作不支持） */
  promptCacheKey: boolean | null;
  /** 缓存存活期（秒）的覆盖。0/未知 = 保温不跑：不知道期限就不该花真钱赌命中 */
  cacheTtlSeconds: number | null;
  /** 逐模型的缓存存活期（秒），键是模型名。最具体的证据，压过全局覆盖与内置表 */
  cacheTtlByModel: Record<string, number>;
  /** 已保存的服务商档案（设置页的卡片）。切换 = 档案字段抄进顶层 */
  profiles: EndpointProfile[];
  /** 当前生效的档案 id。空 = 尚未关联任何档案，顶层字段独立生效 */
  activeProfileId: string;
  /** 当前连接的代理绑定："" 继承全局（全局空 = 直连）/ "direct" / "pool" / 代理 id */
  proxy: string;
  /** 当前连接的按模型代理覆盖，键是模型名 */
  proxyByModel: Record<string, string>;
  /** 代理池本体与全局默认绑定；三级解析与均衡在 Rust proxy.rs */
  proxyPool: ProxyPool;
  /** 全局默认代理绑定，值域同上（"" = 直连）。覆盖模型/决策层/webhook/MCP/命令/渲染层 */
  /** 「设置 → 命令 Shell」：AI 跑命令（run_command）用的 shell。空 = 默认 cmd */
  commandShell: string;
  proxyDefault: string;
  /** 不走代理的主机后缀（example.com 覆盖 api.example.com）。本机回环恒豁免，不依赖这里 */
  proxyBypass: string[];
  /** 主题模式："dark" | "light" | "system" */
  themeMode: "dark" | "light" | "system";
  /** 强调色（#RRGGBB）。空串 = 主题默认的品牌紫 */
  accentColor: string;
  credentialService: string;
  credentialUser: string;
  projects: Project[];
  activeProjectId: string;
  permission: PermissionTier;
  /** 全局覆盖项。项目那一份叠在它上面，且只能更严 */
  permissionOverrides: PermissionOverride[];
  /** 文件安全规则表（全局那份）。项目表在前、全局表在后，首条命中即停 */
  fileRules: FileRule[];
  /** 命令黑名单（design-security-center.md D4）：只收程序名（reg.exe，.exe 可省）。机器级 */
  commandBlocklist: string[];
  /** 命令前缀规则（全局那份）。项目表在前、全局表在后，段间取最严 */
  commandRules: CommandRule[];
  /** 网络安全规则（design-security-center.md D5）：域后缀 → 动作，机器级。未命中落回现行判定 */
  networkRules: { pattern: string; action: FileRuleAction }[];
  /** HTTP 明文分档：远程目标。默认 ask */
  netHttpRemote: FileRuleAction;
  /** HTTP 明文分档：回环目标。默认 allow */
  netHttpLocal: FileRuleAction;
  /** 删除保护（design-security-center.md D1）：delete_file 默认移入回收站；关掉 = 按系统删除 */
  deleteToTrash: boolean;
  /** 批量删除审批阈值：一次 delete_file 的路径数达到它强制走审批（档位与覆盖项都压不住）。0 = 不设 */
  deleteApprovalThreshold: number;
  /** 敏感保护（design-security-center.md D6）：工具结果进话题流之前就地打码 */
  secretScanEnabled: boolean;
  /** 被关闭的敏感检测规则（secrets::RULES 的 id）。关闭对检测与打码同时生效 */
  disabledSecretRules: string[];
  /** 用户自建的敏感检测规则（design-security-center.md D6）：名称 + 正则，不吃提示词闸 */
  customSecretRules: CustomSecretRule[];
  /** 对内置检测规则正则的改写：只许改正则。改过的规则不再吃提示词闸 */
  secretRulePatternEdits: SecretRulePatternEdit[];
  /** 自定义 MCP 总开关（design-security-center.md D7）：一键停用户自配的全部 MCP 服务器 */
  userMcpEnabled: boolean;
  /** 自动备份（design-security-center.md D3）：写/删之前存可恢复副本。失败不挡原操作，只落审计 */
  backupEnabled: boolean;
  /** 回合跑在 Agent 子进程（M3 收官分流开关）。默认关：重活类工具还没搬，子进程里诚实拒绝 */
  agentWorkerTurns: boolean;
  /** 备份总量上限（MB），按最老先删的 LRU 清。0 = 不设上限 */
  backupTotalMb: number;
  /** 网络出口的目标域名单：一行一个域，按域后缀匹配。空 = 不收紧（MCP 是本地子进程，管不到它） */
  netEgressAllow: string[];
  /** 本机监听那一格：默认关。开着要重启应用才听得到（它是启动时读的一次决定） */
  webhookInEnabled: boolean;
  /** 只绑 127.0.0.1 的端口。0 不被允许——让系统随机选等于让自己去找 */
  webhookInPort: number;
  /** 已确认看过「完全访问」的风险说明。为真时切入该档不再弹确认，设置页可恢复 */
  fullAccessAcknowledged: boolean;
  conversationStore: ConversationStore;
  /** 被关闭的工具 id；空=全开 */
  disabledTools: string[];
  /** 被关闭的插件目录名。插件是容器，关掉它它带的技能和 MCP 服务一起消失 */
  disabledPlugins: string[];
  /** 插件 userConfig 的当前值：插件 id → (键 → 字符串值)。声明住在插件 manifest */
  pluginUserConfig: Record<string, Record<string, string>>;
  /** 被关闭的内置扩展 id（出厂名册住在后端 builtins）。关 = 它带的技能整批消失 */
  disabledBuiltins: string[];
  /** 被关闭的技能，键是 "来源/技能目录名" */
  disabledSkills: string[];
  /** 用户逐条确认过内容的钩子：id 定位是哪一条，hash 是确认当时那份定义的指纹 */
  trustedHooks: TrustedHook[];
  /** 「以后都允许」的持久放行（审批指纹规则）。启动时回灌审批中心，判定面与话题内规则同一条 */
  allowRules: Array<{ key: string; label: string }>;
  /** 单独关掉的钩子 id */
  disabledHooks: string[];
  /** 被关闭的扩展工具暴露名，例如 mcp__mcp-1__ping */
  disabledMcpTools: string[];
  mcpServers: McpServer[];
  /** 联网搜索（web_search 工具）：provider 空 = 没配，工具整条不声明给模型 */
  /** 各能力档记住的模型（design：能力会话）。切会话档时自动把对应模型换上来。
   *  键除 chat/image/video 外还有 "audio"（视频画布的音频页签用的 TTS 模型行） */
  kindModels: Partial<Record<string, string>>;
  /** 生图会话的生成参数：随生成请求原样发给上游，支不支持由上游决定 */
  imageGen: ImageGenSettings;
  /** 视频会话画布的生成参数（比例/分辨率/时长）。随生成请求原样发给上游 */
  videoGen: VideoGenSettings;
  webSearch: {
    provider: string;
    apiKey: string;
    maxResults: number;
    /** SearXNG 实例地址（provider = "searxng" 时必填）。用户配置的可信端点 */
    searxngUrl: string;
  };
  tasks: ScheduledTask[];
  /** 开机自启的意图。OS 里到底注册了没有由 `autostart_state` 说，这一格只是下次启动的依据 */
  autostart: boolean;
  /** 聊天正文字号档位。只管读消息那段文字，标题与代码由 CSS 按倍数派生 */
  chatFontSize: ChatFontSize;
  /** 界面缩放（webview 原生 zoom）。1 = 100%，改动即时生效、启动时恢复 */
  uiZoom: number;
  /** 减少动效：压掉界面过渡与动画，只留最终状态 */
  reduceMotion: boolean;
  /** 开发者模式：把关「关于」里的开发者工具入口（WebView2 控制台） */
  devMode: boolean;
  /** 点关闭时：ask=每次问 / tray=直接收进托盘 / quit=直接退出 */
  closeAction: CloseAction;
  /** 窗口置顶 */
  alwaysOnTop: boolean;
  /** 系统通知（审批等待/任务收尾/目标停下）。默认开；窗口在前台时不打扰 */
  notifications: boolean;
  /** 全局快捷键（Ctrl+Shift+G 唤起窗口）。默认关：全局热键占系统按键，要用户点头 */
  globalShortcutEnabled: boolean;
  /**
   * 命令沙箱：三层（收容壳 + WRITE_RESTRICTED 受限令牌 + 低完整性）——写只限
   * 绑定工作目录、专用临时目录与额外可写根。默认关：启用会给项目文件打完整性标签
   */
  sandboxEnabled: boolean;
  /** 自动审查：审批升级交给审查模型替人拍板。不改沙箱边界。默认关 */
  autoReview: boolean;
  /** 自动审查的专属服务商档案 id（profiles 里的 id）。空 = 跟着当前连接走 */
  autoReviewProfileId: string;
  /** 自动审查的专属模型名。空 = 跟着（审查档案或当前的）默认模型走 */
  autoReviewModel: string;
  /** 沙箱的额外可写根（对齐 Codex writable_roots）：启用时逐个打 Low 标签 */
  sandboxWritableRoots: string[];
  /** SSH 主机花名册，每行 `名字=user@host:端口`（端口可省）。凭据走系统 ssh 自己的钥匙链 */
  sshHosts: string[];
  /** LSP 服务器逐扩展覆盖，每行 `ext=启动命令`（如 rs=D:\tools\rust-analyzer.exe） */
  lspServers: string[];
  /** 内置浏览器控制总开关（后端 browser.rs）。关 = browser 工具不声明 */
  browserControlEnabled: boolean;
  /** 内置浏览器忽略 HTTPS 证书校验。只进启动参数，改完要重启内置浏览器 */
  browserIgnoreCertErrors: boolean;
  /** 模型池。路由决定在后端 pool.rs：每一发请求按池的 mode/strategy 挑成员 */
  modelPool: ModelPool;
  /** 模型路由表（后端 route.rs）。空表 = 不路由；规则按数组顺序匹配，第一条命中即停 */
  modelRoutes: ModelRoute[];
  /** 上游模型白名单：中转站统一回这些名字时不算"被换人"（模型对账判定用） */
  upstreamModelWhitelist: string[];
  /** 自定义子助理目录。编排派工与聊天 spawn 工具都从这里取人；空表 = 编排通道保持原样（聊天还有出厂名册可派） */
  subagents: SubagentDef[];
  /** 内置子助理的覆盖项（键 = 出厂名）。定义住后端代码，这里只存偏离：
   *  服务商/模型留空 = 继承默认；未知名字的条目后端安静无视 */
  subagentOverrides: SubagentOverride[];
  /** 资料库语义检索的 embedding 档。baseUrl/model 留空 = 未启用（纯关键词检索） */
  embedding: EmbeddingConfig;
  /** Umi-OCR 引擎档（资料库导入 PDF/图片用）。baseUrl 留空 = 默认本机 127.0.0.1:1224 */
  ocr: { baseUrl: string };
  ui: UiState;
}

/** 资料库语义检索的 embedding 档（OpenAI 兼容 /embeddings 端点；密钥沿用主密钥） */
export interface EmbeddingConfig {
  /** 端点基址（如 https://relay.example.com/v1）。空 = 未启用语义检索 */
  baseUrl: string;
  /** embedding 模型名 */
  model: string;
  /** 向量维度。0 = 首次嵌入时自动探测 */
  dimensions: number;
  /** rerank 精排模型名（如 bge-reranker-v2-m3）。空 = 不精排；与 embedding 同端点同钥匙 */
  rerankModel: string;
}

/** 一个自定义子助理的定义（设置页「子助理」）。
 *  权限不进定义——由工具白名单推导并只收紧不放松；两条消费路径共这份目录：
 *  编排派工（决策桥花名册，orchestrationAssignable）与聊天派单（spawn_subagent，chatSpawnable） */
export interface SubagentDef {
  /** 稳定名：编排节点的 profile 字符串、spawn_subagent 的 name 参数。与内置角色撞名的定义在消费点被无视 */
  name: string;
  /** 什么时候该派它：决策层花名册的判据、spawn 工具 schema 里的描述都是这一句 */
  description: string;
  /** 写进那次 run 的第一句：你是谁、这一支只负责什么 */
  systemPrompt: string;
  /** 工具白名单（内置注册表里的 id）。空 = 纯推理 */
  tools: string[];
  /** 专属服务商档案 id（profiles 里的 id）。空 = 跟着当前连接走 */
  endpointProfileId: string;
  /** 专属模型名。空 = 跟着（服务商档案或当前的）默认模型走 */
  model: string;
  /** 决策层派工可不可以派它 */
  orchestrationAssignable: boolean;
  /** 主聊天模型可不可以按需调用它 */
  chatSpawnable: boolean;
}

/** 一个内置子助理的用户覆盖（设置页「内置子助理」）。定义本身住后端代码，
 *  配置只存偏离默认的那几格——升级能加新角色、改描述，不被旧拷贝钉死 */
export interface SubagentOverride {
  /** 必须命中出厂名册的名字；不命中的整条被后端安静无视（升级挪走了角色，旧覆盖不弹红条） */
  name: string;
  /** 专属服务商档案 id（profiles 里的 id）。空 = 继承默认 */
  endpointProfileId: string;
  /** 专属模型名。空 = 继承默认 */
  model: string;
  /** 停用：从可派名单与 spawn enum 里摘掉，设置页卡片置灰 */
  disabled: boolean;
}

/** 内置子助理的设置页视图：出厂定义套上覆盖后的完整样子（停用的也在，
 *  卡片要置灰展示而不是消失，停用态由 disabled 说明） */
export interface BuiltinSubagentView {
  name: string;
  description: string;
  systemPrompt: string;
  /** 工具白名单（内置注册表里的 id）。空 = 纯推理 */
  tools: string[];
  endpointProfileId: string;
  model: string;
  /** 恒为 false：出厂名册只服务聊天派单，不进编排花名册 */
  orchestrationAssignable: boolean;
  /** 恒为 true：内置的全部可被主模型派出（停用走 disabled） */
  chatSpawnable: boolean;
  disabled: boolean;
}

/** 模型池。mode: off=关（请求走顶层）/ auto=调度器每一发挑 / pinned=手动指定 /
 *  decision=决策层挑，挑不动退回调度器 */
export type PoolMode = "off" | "auto" | "pinned" | "decision";

/** 调度策略。round_robin=平滑加权轮询 / least_used=当前并发最少 / random=加权随机 /
 *  failover=优先级转移（永远用列表里第一个没进冷却的成员，恢复后回主位） */
export type PoolStrategy = "round_robin" | "least_used" | "random" | "failover";

/** 池成员的定位键：哪张档案 + 哪个模型。profileId 空串 = "当前连接"——
 *  顶层配置本身就是一张看不见的档案，不建档案也能把现在这套连接加进池子 */
export interface PoolKey {
  profileId: string;
  model: string;
}

/** 模型池的一个成员。成员存档案引用不存快照：档案页改了地址，池子跟着走 */
export interface PoolMember {
  profileId: string;
  model: string;
  /** 权重。轮询与随机按它加权；"最少并发"不看它——那条策略的权重就是实时并发 */
  weight: number;
  enabled: boolean;
}

/** 模型池。调度读数（轮询位置/并发/冷却）只住后端内存，这里只有"池里有哪些人"。
 *  decision 模式的决策者是 System 1 决策层（Laya 本地 / Jev 云端）——它的开关、
 *  密钥与 Sidecar 在决策层自己的配置里管，池子只负责把选好的成员认下来 */
export interface ModelPool {
  mode: PoolMode;
  strategy: PoolStrategy;
  members: PoolMember[];
  /** pinned 模式生效的那一个成员 */
  pinned: PoolKey | null;
  /** 缓存感知首挑：新话题第一次挑人时优先落到「最近刚成功过、缓存还热」的成员，
   *  让共享 system+工具声明 前缀的请求在时间上聚到同一个成员。只影响新话题首挑，
   *  话题粘住后由亲和账接管 */
  cacheAwarePick?: boolean;
}

/**
 * 手动指定模式下把 pinned 换成另一个模型。pinned 是（档案 × 模型）**成对**的成员定位，
 * Rust 按对校验成员表——只换模型名留着旧档案 id，会写出"档案 A × 模型 B"的幽灵对，
 * 每发必报"手动指定的池成员已经不在池里了"（真机踩过：视频会话固定 (sda×mx-h3)，
 * 切回对话档换成 grok-4.7，pinned 变 (sda×grok-4.7)）。
 * 规则：优先留在原档案（同服务商，密钥与地址都不动）；原档案没有这个模型，
 * 就取第一个启用中的同名成员；池里没人带这个模型 → 返回 null，pinned 原样不动
 * （池子继续供原成员，别为了换模型写坏池子）。
 */
export function switchPinnedMember(
  pool: ModelPool,
  model: string,
): ModelPool | null {
  if (pool.mode !== "pinned" || !pool.pinned) {
    return null;
  }
  const sameModel = pool.members.filter(
    (member) => member.enabled && member.model === model,
  );
  if (sameModel.length === 0) return null;
  const target =
    sameModel.find((member) => member.profileId === pool.pinned?.profileId) ??
    sameModel[0];
  // 已是这个组合就不写（含 pinned 本来就有效的情形）
  if (target.profileId === pool.pinned.profileId && target.model === pool.pinned.model) {
    return null;
  }
  return { ...pool, pinned: { profileId: target.profileId, model: target.model } };
}

/** pinned 现在是不是成员表里不存在的幽灵对（历史版本只换模型名不换档案写出来的） */
export function pinnedIsGhost(pool: ModelPool): boolean {
  return Boolean(
    pool.mode === "pinned" &&
      pool.pinned &&
      !pool.members.some(
        (member) =>
          member.enabled &&
          member.profileId === pool.pinned?.profileId &&
          member.model === pool.pinned?.model,
      ),
  );
}

/** 上下文窗口没配（0）时的兜底，与 config.rs 的 DEFAULT_CONTEXT_TOKENS 同一个数 */
export const DEFAULT_CONTEXT_TOKENS = 128_000;

/**
 * 解析"下一发真正会用的上下文窗口"，与后端请求时的结算链同一条
 * （pool.rs overlay → apply_profile_connection → apply_model_spec）：
 * 手动指定的池成员 = 那份档案的模型行（spec.contextTokens > 0 优先）→ 档案级窗口 → 默认；
 * 固定在「当前连接」上时模型行从激活连接的表里找、档案级就是顶层读数；
 * 池自动/未开池时选谁要等请求才定，先按顶层读数，发过一发后由 Done 带回的真实窗口接管。
 * 用量面板的分母用它：让发消息前后显示同一个数（真机反馈：128K→300K 发一条才跳变）
 */
export function effectiveContextWindow(config: AppConfig, modelOverride?: string): number {
  const pool = config.modelPool;
  if (pool.mode === "pinned" && pool.pinned) {
    const { profileId, model } = pool.pinned;
    const profile = profileId
      ? config.profiles.find((item) => item.id === profileId)
      : undefined;
    const spec = (profile ? profile.models : config.models).find(
      (item) => item.model === model,
    );
    if (spec && spec.contextTokens > 0) return spec.contextTokens;
    const profileLevel = profile ? profile.contextTokens : config.contextTokens;
    if (profileLevel > 0) return profileLevel;
    return DEFAULT_CONTEXT_TOKENS;
  }
  // 非钉死（含未开池）：用"下一发真正用的模型"（调用方给档位模型，缺省全局选中）
  // 查勾选表行——切模型时读数即时跟手，不等下一发请求带回真实窗口。
  // （路由表改道、池自动挑人的场合静态猜不中，仍由 Done 带回的真实值接管）
  const model = modelOverride ?? config.model;
  const spec = config.models.find((item) => item.model === model);
  if (spec && spec.contextTokens > 0) return spec.contextTokens;
  return config.contextTokens > 0 ? config.contextTokens : DEFAULT_CONTEXT_TOKENS;
}

/** 模型路由表的一条规则（design-model-routing.md）。生效档位：点名 > 模型池 >
 *  路由表 > 设置直连——只在「池子没接管、也没点名」的那一档查表 */
export interface ModelRoute {
  id: string;
  /** 匹配的模型名：精确匹配，或 `*` 结尾的前缀匹配；单独一个 `*` 接住一切。逐字节比较，大小写敏感 */
  pattern: string;
  /** 命中后改去的服务商档案 id。空 = 不改服务商；指向已删档案的规则按不命中处理 */
  endpointProfileId: string;
  /** 命中后改成的模型名。空 = 不改名（保持请求原名，档案默认模型不趁乱塞进来） */
  model: string;
  enabled: boolean;
}

/** 聊天正文字号档位。px 基准住在 lib/theme.ts 的 CHAT_FONT_SIZES */
export type ChatFontSize = "small" | "medium" | "large" | "xlarge";

/** 点关闭时的行为。tray 只在托盘真建起来时生效，否则退回 ask */
export type CloseAction = "ask" | "tray" | "quit";

/** 一个外部 MCP 服务器。扩展带来的是别人的工具，不是我们的 */
export interface McpServer {
  id: string;
  name: string;
  /** 传输方式：stdio（默认，老配置没有这一格）| http（streamable HTTP） */
  transport?: "stdio" | "http";
  command: string;
  args: string[];
  /** 启动时注入的环境变量；cc-switch 的 MCP 库普遍靠它传路径和凭据 */
  env: Record<string, string>;
  /** http 型的服务地址（streamable HTTP 服务商）。stdio 型不读这一格 */
  url?: string;
  /** http 型随每个请求带上的请求头（Authorization 这类凭据住这里） */
  headers?: Record<string, string>;
  /** http 型走 OAuth 登录（MCP 授权规范）：传输侧自动注入 Bearer，手写 Authorization 头优先 */
  oauth?: boolean;
  enabled: boolean;
}

export interface McpToolView {
  /** 交给模型的名字：mcp__<服务器>__<工具> */
  exposed: string;
  name: string;
  description: string;
  enabled: boolean;
}

export interface McpServerView extends McpServer {
  /** "独立配置" 或插件名 */
  source: string;
  connected: boolean;
  tools: McpToolView[];
  /** 握手声明过的两格能力。没声明与"声明了但是空的"是两件事，所以这里不报条目数 */
  canResources: boolean;
  canPrompts: boolean;
}

/** 一条生命周期钩子在界面上的样子。执行门槛四条，缺一条就不会跑 */
export interface HookView {
  /** 位置标识：插件目录::哪份文件::事件::序号 */
  id: string;
  event: string;
  /** 正则，只筛工具名；null 表示不限 */
  matcher: string | null;
  command: string;
  /** 秒。超时按"钩子坏了"报出来，不会当成放行 */
  timeout: number;
  /** 发射后不管：输出与退出码被丢弃，不占回合时间。拦截类事件上写 async 等于自己拆护栏 */
  async: boolean;
  statusMessage: string;
  file: string;
  /** 定义指纹。确认信任时记下的就是它 */
  hash: string;
  /** 这个事件在本客户端有落点吗 */
  supported: boolean;
  /** 记下的指纹还和当前定义一致吗 */
  current: boolean;
  trusted: boolean;
  enabled: boolean;
  /** supported && trusted && current && enabled */
  runs: boolean;
}

/** 用户确认过内容的钩子。脚本改一个字节，这份确认就作废 */
export interface TrustedHook {
  id: string;
  hash: string;
  /** 插件钩子没有这一格；工作区钩子锚定它所属的工作目录路径 */
  root?: string | null;
}

/** 界面上那句"它现在到底会不会跑"。判据只有一处，免得每一栏各说一套 */
export function hookState(hook: HookView): { label: string; hint: string } {
  if (!hook.supported)
    return {
      label: "不会触发",
      hint: `aglab 的回合里没有「${hook.event}」这个节点，这段脚本不会被执行`,
    };
  if (!hook.trusted)
    return { label: "未确认", hint: "这是插件作者写的脚本。看过下面这行命令、确认过内容才会执行" };
  if (!hook.current)
    return {
      label: "内容已改，确认作废",
      hint: "你确认过之后这段命令被改动过。请重新看过再确认，否则它不会跑",
    };
  if (!hook.enabled) return { label: "已停用", hint: "你单独关掉了这一条，插件其他部分照旧" };
  return { label: "会执行", hint: "命中筛选条件时这段命令会真的跑起来" };
}

/** 插件带来的一个技能。id 就是技能分区里那个开关的键 */
export interface PluginSkill {
  id: string;
  name: string;
  description: string;
  chars: number;
  enabled: boolean;
}

/** 一个插件目录：容器，可以同时带命令、代理、技能、hooks 和 MCP 服务器 */
export interface PluginView {
  id: string;
  name: string;
  description: string;
  version: string;
  author: string;
  category: string;
  path: string;
  enabled: boolean;
  skills: PluginSkill[];
  mcpServers: string[];
  hooks: HookView[];
  /** hooks.json 里被跳过的部分和原因，例如我们只跑 command 处理器 */
  hookNotes: string[];
  commands: number;
  agents: number;
}

/** 一项可开关的内置执行能力，由 Rust 侧的工具清单给出 */
export interface BuiltinTool {
  id: string;
  title: string;
  blurb: string;
  risk: ToolRisk;
  enabled: boolean;
}

/** 一个 markdown 技能包。只是提示词，不给执行权 */
export interface Skill {
  /** "来源/技能目录名" */
  id: string;
  name: string;
  description: string;
  /** SKILL.md 里声明的工具白名单，空表示不额外限制 */
  allowedTools: string[];
  chars: number;
  preview: string;
  path: string;
  /** "个人" 或插件名 */
  source: string;
  enabled: boolean;
}

export interface SkillsListing {
  dir: string;
  skills: Skill[];
}

export interface PluginsListing {
  dir: string;
  plugins: PluginView[];
  /** 出厂扩展（定义在后端 builtins 名册里，随应用自带） */
  builtins: BuiltinView[];
  /** 工作区钩子：当前工作目录里的 hooks.json（信任按目录锚定，撤销即时生效） */
  workspaceHooks: WorkspaceHooksView | null;
}

/** 当前工作目录里的钩子定义。信任记录按目录路径锚定，换项目互不顶替 */
export interface WorkspaceHooksView {
  root: string;
  hooks: HookView[];
  notes: string[];
}

/** 官方市场条目：下载地址与 sha256 指纹，安装前先验指纹再解压 */
export interface MarketEntry {
  id: string;
  name: string;
  description: string;
  version: string;
  author: string;
  downloadUrl: string;
  sha256: string;
}

export interface MarketView {
  entries: MarketEntry[];
  installedIds: string[];
  source: string;
}

/** 出厂扩展条目：整扩开关走 disabledBuiltins，技能级开关与普通技能共用 disabledSkills */
export interface BuiltinView {
  id: string;
  name: string;
  description: string;
  enabled: boolean;
  skills: PluginSkill[];
}

/** 与 OpenAI 兼容服务商的 reasoning_effort 枚举一一对应 */
export const EFFORT_LEVELS = [
  { value: "minimal", label: "最低" },
  { value: "low", label: "低" },
  { value: "medium", label: "中" },
  { value: "high", label: "高" },
  { value: "xhigh", label: "极高" },
  { value: "max", label: "拉满" },
] as const;

export const DEFAULT_EFFORT = "medium";

export function effortIndex(value: string) {
  const found = EFFORT_LEVELS.findIndex((level) => level.value === value);
  return found === -1 ? EFFORT_LEVELS.findIndex((level) => level.value === DEFAULT_EFFORT) : found;
}

export function effortLabel(value: string) {
  return EFFORT_LEVELS[effortIndex(value)].label;
}

export type PermissionTier = "ask" | "auto" | "full";

export const PERMISSION_LEVELS: Array<{
  value: PermissionTier;
  label: string;
  description: string;
}> = [
  {
    value: "ask",
    label: "逐项确认",
    description: "写入文件、执行命令或碰到项目目录之外时先问你",
  },
  {
    value: "auto",
    label: "自动放行",
    description: "项目内直接执行，越界或跑命令时才询问",
  },
  {
    value: "full",
    label: "完全访问",
    description: "不再询问，按本机账户权限读写任意路径并执行命令",
  },
];

export const RISK_LABELS: Record<ToolRisk, string> = {
  safe: "只读",
  elevated: "写入",
  high: "高风险",
};

/** 右上角贴附的告警条。只活在界面里，不落盘 */
export interface AppToast {
  id: string;
  tone: "error" | "info";
  title: string;
  /** 完整原因。正文里只留一行短标记，长文本放这里 */
  detail?: string;
}

/** 右侧详情面板的标签。tab 数量可扩展，放不下的由「…」收纳。
 *  原来的「详情」「工具」「上下文」「记忆」「思考」「检查点」诸格已删——
 *  最关注的几项搬到了输入框上方；工具调用在消息流、记忆在设置页、
 *  思考过程在消息卡片里、检查点摘要在话题顶部 */
export type PanelTab =
  | "decision"
  | "preview"
  | "terminal"
  | "browser";

export const PANEL_TAB_IDS: readonly PanelTab[] = [
  "decision",
  "preview",
  "terminal",
  "browser",
];

/** panelTab 落盘的是裸字符串（config.ui），换版删签后要退回第一格而不是选中一片空白。
 *  老配置里存着的 "details" 会在这里被自然拒掉 */
export function isPanelTab(value: string): value is PanelTab {
  return (PANEL_TAB_IDS as readonly string[]).includes(value);
}

/** aglab 内置 write_file 留下的编辑记录，按文件聚合 */
export interface FileEdit {
  path: string;
  absPath: string;
  /** 这个文件被写了几次 */
  writes: number;
  /** 每次相对上一次的行数累计，不是"相对话题开始" */
  additions: number;
  deletions: number;
  /** 改动过大退回粗算，数字前面要说"约" */
  approximate: boolean;
  lastAt: number;
  /** 对应对话流里哪几次工具调用，汇总卡靠它挂到本轮 */
  callIds: string[];
  rollbackable: boolean;
  /** 不能回滚的原因，可直接显示 */
  reason: string;
  /** aglab 写完之后文件又被别处改过 */
  drifted: boolean;
}

export interface EditPreview {
  path: string;
  /** 后端只回答"这份内容能不能当文本读"；用哪种渲染器由扩展名在前端决定 */
  kind: "text" | "image" | "binary";
  content: string;
  mime: string;
  data: string;
  bytes: number;
  clipped: boolean;
  note: string;
}

export interface RevertOutcome {
  path: string;
  restoredBytes: number;
  seq: number;
}

export interface ReviewCommit {
  sha: string;
  subject: string;
}

/** 一个文件在某一侧的改动形状 */
export interface ReviewSide {
  /** git 状态码：A/M/D/R/C/T/U，未跟踪是 "?"，二进制是 "B" */
  state: string;
  additions: number;
  deletions: number;
}

export interface ReviewFile {
  path: string;
  /** 重命名与复制的旧路径；其余状态为 null */
  oldPath: string | null;
  /** 相对基线已提交的那一侧 */
  committed: ReviewSide | null;
  /** 工作目录还没提交的那一侧（含未跟踪文件） */
  working: ReviewSide | null;
}

/** 变更请求页的差异布局。与 Rust 的 UiState.diff_layout 同名同义 */
export type DiffLayout = "unified" | "split";

export interface DiffLine {
  kind: "context" | "added" | "removed";
  oldNo: number | null;
  newNo: number | null;
  text: string;
}

export interface DiffHunk {
  /** git 原样的块头，含函数上下文提示 */
  header: string;
  lines: DiffLine[];
}

export interface FileDiff {
  path: string;
  oldPath: string | null;
  scope: DiffScope;
  hunks: DiffHunk[];
  binary: boolean;
  truncated: boolean;
  additions: number;
  deletions: number;
}

/** 差异范围：相对基线已提交 / 工作目录还没提交 */
export type DiffScope = "committed" | "working";

/** 工作目录 git 状态，由 Rust 侧只读命令给出 */
export interface ReviewInfo {
  root: string;
  branch: string;
  head: string;
  /** 比较基线，空串表示没有可比的分支或远端 */
  base: string;
  ahead: number;
  behind: number;
  commits: ReviewCommit[];
  files: ReviewFile[];
  /** 未跟踪文件超过上限时没列进来的个数 */
  untrackedOmitted: number;
  /** 交给模型的差异行数。正文只在服务端拼，不过 IPC */
  patchLines: number;
  patchTruncated: boolean;
}

export type TaskKind = "interval" | "daily" | "weekly" | "cron";

/** 停机期间错过的槽位怎么补。它写在 `kind` 的后缀上（`"interval|skip"`），
 * 拆与拼只允许在 `src/lib/tasks.ts` 里做 */
export type MissedPolicy = "run_latest" | "skip" | "catch_up_once";

/** 定时任务的定义，存在 config.json 里；运行结果由 Rust 单独维护 */
export interface ScheduledTask {
  id: string;
  name: string;
  prompt: string;
  /** `TaskKind`，或 `TaskKind|MissedPolicy`：频率与错过策略共用这一个字符串 */
  kind: string;
  /** kind=interval：每隔多少分钟 */
  everyMinutes: number;
  /** kind=daily：本地的第几分钟（0 = 00:00） */
  atMinute: number;
  /** kind=weekly：目标星期（0=周日…6=周六）。interval/daily 不读这一格 */
  atWeekday: number;
  /** kind=cron：标准 5 段表达式（分 时 日 月 周；也吃 6/7 段的秒/年）。
   *  空串或认不出来 = 没有触发器；存的时候后端会把坏表达式拒在门外 */
  cronExpr: string;
  enabled: boolean;
  createdAt: number;
  /** 多步任务的图。`nodes` 为空 = 一句 prompt 跑一发（老形状，一个字节都不用改） */
  graph: TaskGraph;
  /** 跑完之后 POST 到哪儿。空串 = 不发。签名密钥不在配置里，走系统凭据 */
  webhookUrl: string;
  /** 反方向：本机另一个进程拿这个令牌敲 `POST 127.0.0.1:<端口>/hook/<令牌>` 就跑这一条。
   * 空串 = 这条任务不可被外部触发。总开关 `webhookInEnabled` 在 AppConfig 上，默认关 */
  webhookToken: string;
}

/** 任务图里的一格。空 `allowedTools` = 沿用任务自己的工具白名单，非空是收紧不是放宽 */
export interface TaskNode {
  id: string;
  prompt: string;
  dependsOn: string[];
  allowedTools: string[];
  /** 这一格交不交出去给一个子助理跑。只带能力面，做什么仍然写在上面的 `prompt` 里 */
  subagent: TaskSubagent | null;
}

/** 子助理那一发的规格。它刻意没有 prompt：两处都能写问句就没人说得清照哪一份干活 */
export interface TaskSubagent {
  tools: string[];
  /** 0 = 沿用设置里那个全局轮数上限 */
  maxRounds: number;
  /** 1e-8 美元口径，与用量台账一致；0 = 不设这条 */
  budgetUsdE8: number;
}

export interface TaskGraph {
  nodes: TaskNode[];
  /** block_run = 一处失败整张图停下；skip_branch = 只放弃那条分支，别的照跑 */
  onFailure: "block_run" | "skip_branch";
}

/** 账本里一次运行的一格。它与 `TaskNode` 不是一回事：这是发生过的事实，不是定义 */
export interface TaskRunNode {
  nodeId: string;
  status: TaskRunStatus;
  conversationId: string;
  startedAt: number;
  finishedAt: number | null;
  error: string | null;
  costUsd: number;
  /** 这一格的产出是一个子助理做的（它自己另有一发 run 与一个话题） */
  delegated: boolean;
}

export type TaskRunStatus =
  | "running"
  | "waiting_approval"
  | "succeeded"
  | "failed"
  /** 追账超出每轮上限，剩下那些格子作废。它不是一次运行：没有话题、没有成本 */
  | "skipped";

/** 一次运行的成本。`costUsdE8` 是账本里的整数口径（1e-8 美元），浮点只用于显示 */
export interface TaskRunCost {
  requests: number;
  inputTokens: number;
  outputTokens: number;
  cachedTokens: number;
  costUsdE8: number;
  unpricedRequests: number;
}

/** runs.jsonl 的一发：账本事实的投影。它不可编辑——要重来请起一发新的或用续跑 */
export interface TaskRun {
  runId: string;
  taskId: string;
  /** 这一发是谁起的。"webhook" = 本机另一个进程敲了那个端口，不是调度器到的点 */
  startedBy: "scheduler" | "user" | "webhook";
  status: TaskRunStatus;
  startedAt: number;
  finishedAt: number | null;
  conversationId: string;
  error: string | null;
  unfinished: boolean;
  cost: TaskRunCost | null;
  costUsd: number;
  nodes: TaskRunNode[];
  /** 通知投出去了没。null = 这个任务没配地址（不是失败） */
  delivery: TaskDelivery | null;
}

/** 一次 outbound 投递的结果。`note` 是"为什么没发/为什么没成"，那是要给人看的话 */
export interface TaskDelivery {
  sent: boolean;
  /** 对端状态码，没发出去是 0 */
  status: number;
  attempts: number;
  note: string;
}

export interface TaskView extends ScheduledTask {
  /** 0 = 停用或配置非法，没有下一次 */
  nextRunAt: number;
  lastRunAt: number;
  /** "" = 还在跑；"waiting" = 停在待审批上等人点头 */
  lastStatus: "" | "ok" | "error" | "waiting" | "skipped";
  lastError: string;
  lastConversationId: string;
  /** 后端按 `kind` 后缀算出来的当前策略，界面照它显示 */
  missedPolicy: MissedPolicy;
  /** 这一发被并发闸挡过、还没跑起来的原因。空 = 没被挡住 */
  deferred: string;
}

/**
 * 一条挂在 durable 队列上的动作：无人值守的后台 run 撞到"要点头"时停在这里，
 * 不超时放行也不超时拒绝。批准不会当场续跑——它记的是"下次再碰到同一份指纹时放行"。
 */
export interface TaskApproval {
  id: string;
  runId: string;
  taskId: string;
  conversationId: string;
  /** 权限表里拦下它的那一行 */
  capability: string;
  target: string;
  /** 用户点的就是这一份：换参数就是另一发，不会顺手被放行 */
  fingerprint: string;
  reason: string;
  requestedAt: number;
  status: "waiting" | "approved" | "denied";
  decidedAt: number | null;
}

/** 两种本地存储的取舍说明，设置页照这份渲染，避免文案和实现分开漂 */
export const STORAGE_OPTIONS: Array<{
  value: ConversationStore;
  label: string;
  short: string;
  pros: string;
  cons: string;
}> = [
  {
    value: "json",
    label: "每个话题一个 JSON 文件",
    short: "JSON",
    pros: "话题即文件：能用编辑器直接打开、逐条备份、丢进 git 看 diff；某一条写坏不牵连同目录的其它话题。",
    cons: "侧边栏列表要把每个文件读一遍并解析正文，攒到几百条后会明显变慢；没有查询能力，按工作目录或时间筛选只能在前端做。",
  },
  {
    value: "sqlite",
    label: "单个 SQLite 数据库",
    short: "SQLite",
    pros: "列表只查索引、不解析消息正文，几千条仍是一次查询；写入走事务，中途崩溃不会留下半条话题。",
    cons: "全部历史集中在一个文件，不能手工编辑，文件损坏影响面是整个历史；应用运行时旁边会有 -wal / -shm，热备份要连它们一起拷或先退出应用。",
  },
];

export function storageOption(value: ConversationStore) {
  return STORAGE_OPTIONS.find((option) => option.value === value) ?? STORAGE_OPTIONS[0];
}

export type ChatEvent =
  | { type: "delta"; text: string }
  | { type: "reasoning"; text: string }
  /** 请求链路的阶段探针：消息头行那条链路动画的数据源（key: input/payload/egress/ttft） */
  | {
      type: "probe";
      key: string;
      detail: string;
      /** 格子的读色：ok 一致 / info 中性 / warn 要告警（如模型对账的上游替换）。
       *  缺省 = 老探针帧，照常绿色收尾 */
      tone?: "ok" | "info" | "warn";
      /** 悬停展开的完整读数（如模型对账的 请求→实发→上游 三元组） */
      hint?: string;
    }
  | {
      type: "tool";
      id: string;
      name: string;
      status: ToolStatus;
      risk: ToolRisk;
      input: string;
      output?: string;
      /** 原始参数 JSON 文本，落盘供话题恢复重放 */
      arguments?: string;
      /** 这一发该问而没问的凭据，见 `ToolCall.passReason`。null = 没有跳过询问 */
      passReason: string | null;
      /** 声明时本轮正文已流出的 UTF-16 码元数（流式内联的切片点），见 `ToolCall.contentChars` */
      contentChars?: number | null;
    }
  | {
      type: "done";
      inputTokens: number;
      outputTokens: number;
      durationMs: number;
      /** 命中服务商缓存的输入：>0 即本轮缓存命中（setCacheHit）；null = 服务商没回这个字段 */
      cachedTokens: number | null;
      /** 本轮往话题日志里新登记的条目 id，按登记顺序 */
      entryIds: string[];
      /** 这一发实际发出去的模型名。池子会换人、路由表会改名，所以它不等于设置里那个
       *  顶层 model——"这句是谁答的"只有后端答得了，前端别拿配置猜 */
      model: string;
      /** 这一发实际生效的上下文窗口。分母跟着路由走（池成员/别的档案的模型行才是真相），
       *  用量面板拿它当分母；后端旧版本不发这一格 → undefined，面板退回顶层配置 */
      contextTokens?: number;
    }
  | {
      /** 上下文自动压缩：start 显示"压缩中"，done 用摘要替换被压缩的旧消息 */
      type: "compaction";
      phase: "start" | "done";
      summary?: string;
      /** 压缩后原样保留的最近消息条数 */
      kept?: number;
    }
  /**
   * 目标模式每一轮跑完之后的续跑读数。它排在 `done` **前面**：界面在 done 上就把这一轮
   * 落定了，之后再告诉它"还有一轮要接"，那一轮的字节会被整批丢掉
   */
  | { type: "mode"; continuing: boolean; state: ModeState }
  /** 一次文件快照已落账（write_file/delete_file 的动手前副本）。变更面板靠它即时点亮，
   *  不用等整轮结束；backup=false 表示文件太大没存副本，snapshotNote 里有人话原因 */
  | {
      type: "fileSnapshot";
      path: string;
      callId: string;
      additions: number;
      deletions: number;
      backup: boolean;
      snapshotNote: string;
    }
  /** 服务商没报错但这一轮不完整（输出被截断、被安全策略拦下） */
  | { type: "notice"; text: string }
  /** 服务商报错但这一轮会自己重来：走告警条，不进正文 */
  | { type: "retry"; text: string; reason: string }
  | { type: "error"; message: string }
  /** update_plan 的整份计划清单。后端每次都给全量，前端只画不判 */
  | { type: "plan"; explanation: string | null; steps: PlanStep[] }
  /** ask_user 的提问：后端这一发挂起等人，answerQuestion 投答案 */
  | {
      type: "ask";
      id: string;
      question: string;
      options: Array<{ label: string; description?: string | null }>;
    };

/** 计划卡上的一步。status 是 schema 三态原样：pending / in_progress / completed */
export interface PlanStep {
  title: string;
  status: "pending" | "in_progress" | "completed";
}

/** 挂起中的模型提问（ask_user）。id 是回答通道的钥匙 */
export interface PendingQuestion {
  id: string;
  question: string;
  options: Array<{ label: string; description?: string | null }>;
}

/** 一次模型请求折成多少钱：台账按 1e-8 美元存整数，这里只给展示用的浮点值 */
export interface UsageTotals {
  requests: number;
  failed: number;
  inputTokens: number;
  outputTokens: number;
  cachedTokens: number;
  reasoningTokens: number;
  costUsd: number;
  /** 没匹配到价格表的请求数：它们按 0 记账，界面必须说出来 */
  unpricedRequests: number;
  /** 服务商没回缓存字段的请求数。cachedTokens 只统计已上报的那些 */
  unreportedCacheRequests: number;
}

export interface ModelUsage {
  model: string;
  requests: number;
  inputTokens: number;
  outputTokens: number;
  cachedTokens: number;
  costUsd: number;
  priced: boolean;
}

export interface DayUsage {
  date: string;
  requests: number;
  costUsd: number;
}

/** 按话题聚合的一行。conversation 为空串 = 那一发没挂在具体话题上（起标题这类边角请求） */
/** 按话题聚合的一行。conversation 为空串 = 那一发没挂在具体话题上（起标题这类边角请求） */
export interface ConversationUsage {
  conversation: string;
  requests: number;
  inputTokens: number;
  outputTokens: number;
  costUsd: number;
}

/** 按话题聚合的一页（自拉分页） */
export interface ConversationPage {
  rows: ConversationUsage[];
  /** 这个时间窗里一共有几场话题：页数与"共 N 场"从它来 */
  total: number;
}

export interface UsageReport {
  totals: UsageTotals;
  byModel: ModelUsage[];
  daily: DayUsage[];
}

/** 每百万 token 的美元单价。字符串存：小数文本没有二进制浮点的表示误差 */
export interface ModelPrice {
  modelId: string;
  displayName: string;
  inputUsdPerM: string;
  outputUsdPerM: string;
  cacheReadUsdPerM: string;
  cacheCreationUsdPerM: string;
}

/** cc-switch 里的一个供应商候选。密钥永远不跨 IPC，只有 hasKey 这个布尔 */
export interface CcswitchCandidate {
  /** cc-switch providers 表的 id，导入时原样传回 */
  sourceId: string;
  /** "claude" | "codex"：cc-switch 存配置的两种壳子 */
  appType: string;
  name: string;
  baseUrl: string;
  model: string;
  reasoningEffort: string;
  /** "chat" | "responses" */
  apiFormat: string;
  hasKey: boolean;
  isCurrent: boolean;
  /** 派生的完整请求地址，导入前给用户看清楚 */
  endpoint: string;
  /** claude 壳子会有警示文案，原样展示；codex 为空串 */
  note: string;
}

/** cc-switch MCP 库里的一个 stdio 候选 */
export interface McpCandidate {
  sourceId: string;
  name: string;
  command: string;
  args: string[];
  /** 全部环境变量名（不含值） */
  envKeys: string[];
  /** 看着像密钥的变量名。导入会把它们的值明文写进 config.json */
  secretEnvKeys: string[];
  /** 在 cc-switch 里为哪些客户端启用：Claude / Codex / Gemini */
  enabledFor: string[];
}

/** 批量导入 MCP 的结果。config 是导入后的整份新配置 */
export interface McpImportResult {
  config: AppConfig;
  added: number;
  skipped: number;
}

/** 上下文各段的字符数。组装规则与后端发请求时完全一致 */
/** Inspector 的一层读数。键名照 Rust 序列化的实测列表写 */
export interface ContextLayerRow {
  layer: string;
  entries: number;
  rows: number;
  chars: number;
  sharePct: number;
  target: number;
  max: number;
  conceded: boolean;
}

/** 一个命名段现在是什么状态：段是差分行，所以"历史里几行"与"生效哪一行"分开报 */
export interface ContextSectionRow {
  name: string;
  layer: string;
  effectiveEntryId: string;
  writtenRows: number;
  chars: number;
  revoked: boolean;
  projected: boolean;
}

/** 被投影丢掉的东西：哪条条目、本来要占多少、为什么 */
export interface ContextDroppedRow {
  entryId: string;
  layer: string | null;
  chars: number;
  why: string;
}

export interface ContextBudgetRow {
  layer: string;
  chars: number;
  target: number;
  max: number;
}

/** 这一轮生效的那次改写。撤销只对着它做：被顶替掉的那些条目自己撤不掉自己 */
export interface ContextRewrite {
  entryId: string;
  /** "compaction" = 从头部断开，整个前缀重付一次；"span" = 只换中间那一段 */
  kind: string;
  replacedRows: number;
  replacedChars: number;
}

/** `context_inspect` 的那份报告。字段名与 Rust 侧一一对上，由契约测试钉住 */
export interface ContextInspector {
  conversationId: string;
  leafEntryId: string | null;
  sentDigest: string;
  sentRows: number;
  sentChars: number;
  declaredRows: number;
  declaredChars: number;
  layers: ContextLayerRow[];
  budget: ContextBudgetRow[];
  sections: ContextSectionRow[];
  dropped: ContextDroppedRow[];
  rewrite: ContextRewrite | null;
  /** 幂等读缓存的读数。它不在日志里，是进程内那张表现读的 */
  cache: { hits: number; misses: number; entries: number; retries: number } | null;
  limit: number;
  deficit: number;
  endpointPromptTokens: number | null;
  /** 本机实测的字符↔token 系数与偏差上界。样本不够就是 null，那时口径仍是估算 */
  calibration: {
    charsPerToken: number;
    maxDeviationPct: number;
    samples: number;
  } | null;
  /** 这一支从哪来。只有分叉出来的话题有值，其余是 null */
  parentSessionId: string | null;
  /** 内联自 layers::Estimate：它明写自己是字符口径，不是 token */
  estimate: { chars: number; kind: string };
  /** 欠的让步，从便宜到贵；什么也不欠时是 ["none"] 而不是空表 */
  ladder: string[];
  /** 阶梯上**没有自动执行者**的那几步：那句"本轮要让步"要同时说清哪一步其实不会自己发生 */
  unattended: string[];
  reason: { kind: string; limit?: number; cause?: unknown };
}

/** 一条"本话题内允许"的放行：`key` 是撤销句柄，`label` 是当初确认框上那句话 */
export interface RememberedRule {
  key: string;
  label: string;
}

export interface ContextBreakdown {
  /** 项目系统提示词 */
  systemChars: number;
  /** 技能清单 */
  skillsChars: number;
  /** 内置工具声明（含 load_skill 入口） */
  toolsChars: number;
  /** 连接器及 MCP 工具声明 */
  mcpChars: number;
  /** 把字符折回 token 的那把尺（实测下界；没量过就是 1） */
  charsPerToken: number;
}

/** 一个可导入的外部 AI 应用（Claude Code / Codex） */
export interface AiImportSource {
  /** "claude" | "codex"，导入时原样传回 */
  kind: string;
  name: string;
  /** 检测到可导入的数据 */
  available: boolean;
  /** "12 个项目 · 45 条话题" 或 "未检测到" */
  detail: string;
}

export interface AiImportOutcome {
  projects: number;
  imported: number;
  skipped: number;
  /** 汇总文案，直接展示 */
  note: string;
}

/** cc-switch 里已安装的一个本地技能 */
export interface SkillCandidate {
  sourceId: string;
  name: string;
  description: string;
  /** cc-switch 侧的目录名，同时也是 aglab 侧的目标目录名 */
  directory: string;
  /** ~/.cc-switch/skills/<directory>/SKILL.md 是否存在 */
  hasSkillMd: boolean;
  /** aglab 个人技能目录下是否已有同名目录（导入会跳过） */
  exists: boolean;
}

export interface SkillImportResult {
  added: number;
  skipped: number;
  /** 被跳过的目录名及原因，逐条展示给用户 */
  skippedNames: string[];
}

/** usage.db requests 表的一行（分页明细） */
export interface UsageRequestRow {
  id: number;
  ts: number;
  model: string;
  baseUrl: string;
  inputTokens: number;
  outputTokens: number;
  cachedTokens: number;
  /** false = 服务商没回缓存字段，此时 cachedTokens 不代表「命中 0」 */
  cacheReported: boolean;
  reasoningTokens: number;
  costUsd: number;
  priced: boolean;
  latencyMs: number;
  /** 首 token 延迟；失败的请求可能是 null */
  firstTokenMs: number | null;
  ok: boolean;
  error: string;
}

export interface UsageRequestPage {
  rows: UsageRequestRow[];
  /** 同窗口下的总行数，分页控件用 */
  total: number;
}

/** 单个话题的缓存命中聚合 */
export interface SessionCacheUsage {
  /** 累计值只统计服务商上报过的请求，所以它是下界 */
  inputTokens: number;
  cachedTokens: number;
  requests: number;
  /** 上报了缓存字段的请求数，小于 requests 时累计命中率不可比 */
  reportedRequests: number;
  /** 最近一轮的输入与命中——诊断"缓存现在有没有生效"看它 */
  lastInputTokens: number;
  /** 末笔未上报时为 0，必须配 lastCacheReported 一起读 */
  lastCachedTokens: number;
  lastCacheReported: boolean;
  /** 最近一轮的输出：它会成为下一轮的输入 */
  lastOutputTokens: number;
  /** 白付的 token：本该命中却没命中的那部分。百分比说不清的亏，用 token 数说得清 */
  wastedTokens: number;
  /** 白付量折算的美元。没有价格表的模型只计 token 不计价 */
  wastedCostUsd: number;
}
