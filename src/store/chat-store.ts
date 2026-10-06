import { create } from "zustand";
import { listen } from "@tauri-apps/api/event";
import { save } from "@tauri-apps/plugin-dialog";

import {
  addProject as requestAddProject,
  chatAbort,
  chatSteer,
  chatFollowUp,
  goalsOverview,
  conversationFork,
  compactHistory,
  decideTool,
  answerQuestion as answerQuestionIpc,
  generateTitle,
  builtinToolsList,
  ccswitchCandidates,
  ccswitchImportMcp,
  ccswitchImportProvider,
  ccswitchImportPricing,
  ccswitchImportSkills,
  ccswitchMcpCandidates,
  ccswitchSkillCandidates,
  fetchConfig,
  fetchSessionEdits,
  fetchUsageReport,
  goalDiscardState,
  goalPauseState,
  goalResumeRun,
  pricingRemove,
  pricingUpsert,
  skillsList,
  fetchModels,
  fetchStorageInfo,
  historyList,
  historyLoad,
  historyRemove,
  historySave,
  mcpConnect,
  mcpList,
  mcpStop,
  mcpRefresh,
  persistPatch,
  pluginsList,
  pricingList,
  probeCredential,
  readAttachment,
  removeProject as requestRemoveProject,
  requestStorageSwitch,
  selectProject as requestSelectProject,
  sendChat,
  taskRunNow,
  tasksList,
  usageExportCsv,
  usageRecent,
  conversationNavigate,
  fetchConversationTree,
  fetchModeState,
  setModeState,
  sessionGoalSet,
  sessionGoalEdit,
  type CriterionInput,
  type ModeOutcome,
  mediaGenerate,
  assetAllow,
} from "@/lib/chat-transport";
import {
  capabilityForKind,
  capabilitiesOf,
} from "@/lib/model-capabilities";
import { branchTail, partition } from "@/lib/conversation-tree";
import { consumeMemorySkipNextTurn, memoryExtract, runMemoryCommand } from "@/lib/memory";
import { gateMemoryExtraction, pickPoolMember, routeModel } from "@/lib/decision/integrations";
import type {
  AppConfig,
  AppToast,
  Attachment,
  ChatEvent,
  ConversationMeta,
  ConversationRecord,
  ConversationStore,
  FileEdit,
  BuiltinTool,
  CcswitchCandidate,
  GoalPanelEntry,
  GoalStatus,
  McpCandidate,
  McpServer,
  McpServerView,
  Message,
  ModeState,
  ModelPrice,
  PanelTab,
  PendingQuestion,
  PlanStep,
  PluginView,
  BuiltinView,
  ScheduledTask,
  SidebarSection,
  Skill,
  SkillCandidate,
  StorageInfo,
  TaskView,
  ToolCall,
  Usage,
  WorkingMode,
  UsageReport,
  UsageRequestRow,
  ConversationKind,
  MediaType,
} from "@/types/chat";
import { isPanelTab, isSection, storageOption, pinnedIsGhost, switchPinnedMember } from "@/types/chat";
import {
  type GitBranches,
  type WorktreeInfo,
  worktreeAttach,
  worktreeBranches,
  worktreeDetach,
  worktreeStatus,
} from "@/lib/worktree";

export const ASSISTANT_NAME = "aglab 助手";

/** 用量时间窗。0 不是零天，是"全部"——后端 days<=0 同一个语义 */
export type UsageDays = 1 | 7 | 30 | 0;

/** 「最近请求」每页行数，后端 clamp 1..=200，这里固定 20 */
const USAGE_ROWS_PAGE = 20;

/** 压缩状态消息的固定 id：start 插入、done 替换，靠它定位 */
export const COMPACTION_MSG_ID = "compaction-status";

const FALLBACK_CONFIG: AppConfig = {
  baseUrl: "",
  model: "",
  apiFormat: "chat",
  reasoningEffort: "medium",
  temperature: 0.7,
  maxTokens: 2048,
  contextTokens: 128_000,
  // 与 Rust 侧 `WebSearchConfig::default()` 同款：没配 = web_search 不声明
  kindModels: {},
  imageGen: { size: "1024x1024", quality: "auto", count: 1 },
  videoGen: { mode: "omni", ratio: "16:9", resolution: "720P", duration: 5 },
  webSearch: { provider: "", apiKey: "", maxResults: 8, searxngUrl: "" },
  notifications: true,
  globalShortcutEnabled: false,
  sandboxEnabled: false,
  autoReview: false,
  autoReviewProfileId: "",
  autoReviewModel: "",
  sandboxWritableRoots: [],
  sshHosts: [],
  lspServers: [],
  allowRules: [],
  // 与 Rust 侧 `AppConfig::default()` 同一个数：这三个是"长度闸"的默认值，
  // 后端还没答话时这里只是占位，写歪了第一眼看到的数字就是假的
  projectRulesMaxChars: 8_000,
  toolResultMaxChars: 16_000,
  memorySectionMaxChars: 0,
  autoCompact: true,
  models: [],
  cacheWarming: false,
  repetitionGuard: true,
  autoUpdateCheck: true,
  lastUpdateCheckAt: 0,
  showReasoning: true,
  // 与 Rust 侧 `AppConfig::default()` 同一个数：429 无限重试默认关——
  // "无限"意味着这一发可能永远不结束，用户没点头之前不这么做
  unlimitedRetry429: false,
  // 与 Rust 侧 `AppConfig::default()` 同一个数：重启后自动继续目标默认关——
  // 要不要继续花钱由人决定，不让一次启动替他重按播放键
  goalResumeOnLaunch: false,
  maxToolRounds: 6,
  // 与 Rust 侧 `orchestrator::DEFAULT_TOTAL_PARALLEL` 同一个数：这份默认值只在后端
  // 还没答话时占位，两边不一致的话第一眼看到的数字就是假的
  totalParallel: 6,
  // 与 Rust 侧 `AppConfig::default()` 同一个数（`audit::RETENTION_DAYS`）。
  // 这份默认值只在后端还没答话时占位，两边不一致第一眼看到的数字就是假的
  auditKeepDays: 90,
  promptCacheKey: null,
  cacheTtlSeconds: null,
  cacheTtlByModel: {},
  profiles: [],
  activeProfileId: "",
  themeMode: "dark",
  accentColor: "",
  // 与 Rust 侧 `AppConfig::default()` 同一套：外观新四项的占位默认值
  chatFontSize: "medium",
  uiZoom: 1,
  reduceMotion: false,
  devMode: false,
  closeAction: "ask",
  alwaysOnTop: false,
  // 内置浏览器默认关：它让模型能驱动一个真浏览器，用户点头之前不动
  browserControlEnabled: false,
  browserIgnoreCertErrors: false,
  // 池子默认整体关闭：请求照旧走顶层模型，成员表空着等用户去挑
  modelPool: {
    mode: "off",
    strategy: "round_robin",
    members: [],
    pinned: null,
  },
  // 路由表默认空 = 不路由：每一发照旧走设置直连
  modelRoutes: [],
  // 代理默认全空：直连，系统代理环境变量也不读（行为确定）
  proxy: "",
  proxyByModel: {},
  proxyPool: { strategy: "round_robin", proxies: [] },
  proxyDefault: "",
  proxyBypass: [],
  // 命令 Shell 默认空：run_command 回 cmd（模型显式指定的 shell 仍优先）
  commandShell: "",
  // 子助理目录默认空：编排通道（决策层花名册）保持原样；聊天的可派名单由
  // 后端的出厂名册兜底，永远不空
  subagents: [],
  // 内置子助理的覆盖默认空：全部继承默认、全部启用
  subagentOverrides: [],
  // 语义检索默认关：没配 embedding 端点就是纯关键词检索；rerank 同理
  embedding: { baseUrl: "", model: "", dimensions: 0, rerankModel: "" },
  // OCR 引擎档：空地址 = 默认本机 Umi-OCR 服务
  ocr: { baseUrl: "" },
  credentialService: "aglab/api-key",
  credentialUser: "default",
  projects: [],
  activeProjectId: "",
  permission: "ask",
  permissionOverrides: [],
  // 与 Rust 侧 `AppConfig::default()` 同一个数：空 = 不收紧（§16）
  netEgressAllow: [],
  // 本机监听：与 Rust 默认一致——关，端口 8786（design-task-engine.md §15）
  webhookInEnabled: false,
  webhookInPort: 8786,
  fullAccessAcknowledged: false,
  conversationStore: "json",
  disabledTools: [],
  disabledPlugins: [],
  fileRules: [],
  commandBlocklist: [],
  commandRules: [],
  networkRules: [],
  netHttpRemote: "ask",
  netHttpLocal: "allow",
  deleteToTrash: true,
  deleteApprovalThreshold: 50,
  secretScanEnabled: true,
  disabledSecretRules: [],
  customSecretRules: [],
  secretRulePatternEdits: [],
  userMcpEnabled: true,
  backupEnabled: true,
  backupTotalMb: 3000,
  disabledBuiltins: [],
  disabledSkills: [],
  trustedHooks: [],
  disabledHooks: [],
  disabledMcpTools: [],
  mcpServers: [],
  tasks: [],
  autostart: false,
  ui: {
    sidebarCollapsed: false,
    // 与 Rust UiState::default 同步：右侧面板默认收起
    panelCollapsed: true,
    panelTab: "decision",
    section: "chats",
    diffLayout: "unified",
  },
};

function newId(prefix: string) {
  return `${prefix}_${crypto.randomUUID().slice(0, 8)}`;
}

/** 一轮生成自己的现场。这一轮的正文与状态归它所有，界面上那一份只是它显示在屏上时的
 *  投影（`patchRunOf`）：切走话题不再"丢尾巴"，也不再落半截盘 */
interface LiveRun {
  conversationId: string;
  projectId: string;
  title: string;
  /** 能力档随 run 走：切走再切回来，界面上这份要与 run 的一致 */
  kind: ConversationKind;
  messages: Message[];
  /** 不在当前分支上的那些气泡。切走的那一支一行都不丢——丢了就又变成
   *  "界面抹掉、日志留着"的两份真相 */
  offPath: Message[];
  usage?: Usage;
  /** 服务商还在出字 */
  pending: boolean;
  /** 这一轮真的收尾了（跟随轮跑完或出错）。之后不再接受事件，等落盘完交还现场 */
  settled: boolean;
  /** 最近一次落盘。结束时要等它，否则切回来读到的还是存档里上一截 */
  save: Promise<void>;
  followUpCount: number;
  /** 排队的跟随气泡 id（FIFO）。后端每条跟随各回一个 Done，按同序对号 */
  followUpBubbleIds: string[];
  /** 这一支现在的作业模式读数。跑着的那一轮自己带一份，切走话题时不该被别的话题盖掉 */
  mode: ModeState | null;
  /** 后端已经说过"还有一轮要自己接"（目标模式自动续跑）。下一个事件到了开新气泡，
   *  而手上这个 Done 不许把这一轮落定——落定了后面那轮的字节就整批丢掉 */
  modeArmed: boolean;
}

/** 生成工作区草稿（视频与音乐共用一张表）：没发送的输入随会话记着，
 *  切走再回来不蒸发。字段全部可选——缺了就按工作区的默认值画。
 *  只住内存不入盘：存档的真相是消息与产物，草稿是工作台的临时状态 */
export interface MediaDraft {
  prompt?: string;
  mode?: "t2v" | "i2v" | "v2v";
  description?: string;
  style?: string;
  lyrics?: string;
  instrumental?: boolean;
}

interface ChatState {
  config: AppConfig;
  configLoaded: boolean;
  models: string[];
  modelsError: string | null;
  loadingModels: boolean;
  hasKey: boolean;

  activeId: string;
  projectId: string;
  title: string;
  /** 会话的能力档（对话/生图/视频）。开档即定，中途不改——切能力就是开新会话 */
  kind: ConversationKind;
  /** 当前分支：从根到末端那一条，按顺序。界面渲染只看它 */
  messages: Message[];
  /** 不在当前分支上的气泡（兄弟分支）。落盘与切分支都要带着它，一个都不丢 */
  offPath: Message[];
  attachments: Attachment[];
  usage?: Usage;
  pending: boolean;
  /** 生图/视频的生成跑着（独立管线，不占对话轮的 pending）。发送钮按它禁用 */
  mediaBusy: boolean;
  /** 生成工作区的草稿，按会话 id 各存一份：切换会话不是草稿的清理时机，
   *  切回来要原样接上。键随 deleteConversation 一起删 */
  mediaDrafts: Record<string, MediaDraft>;
  /** 插队/排队没落进在跑回合时，把那句话放回输入框的交接槽（null = 没有要还的） */
  draftRestore: string | null;
  /** 视频画布当前的生成类型（四类页签）：模型选择器按它过滤候选、
   *  选中后把模型行写进 kindModels 对应的键 */
  videoGenerationType: MediaType;
  /** 画布上正在对话的节点。发送时没有可用节点就自动建第一个 */
  /** 跟随队列长度：排了 N 条，回合收尾后就会自动接着跑 N 轮 */
  followUpCount: number;
  /** 当前话题的作业模式读数（null = 还没读到）。真相在话题日志里，这里只是投影 */
  mode: ModeState | null;
  modeError: string | null;
  modeBusy: boolean;
  /** 此刻还在跑的话题（切走的那些一样在跑，见 `LiveRun`）。侧栏按它标"生成中"，
   *  停止/插话/排队则按 `activeId` 找自己那一条——并行跑着几轮时不能杀错回合 */
  runningIds: string[];
  deciding: string[];
  /** 各话题最新一份计划（update_plan）。卡片只画当前正在看的这条；话题级的活状态，
   *  不进存档——重开之后模型的历史里有它，要用时它会重发 */
  plans: Record<string, { explanation: string | null; steps: PlanStep[] }>;
  /** 挂起中的模型提问（ask_user），按话题分桶。后端在等，没有超时；按 done/停止清 */
  pendingQuestions: Record<string, PendingQuestion>;
  /** 请求链路的阶段序列（input/payload/egress/ttft/usage）：头行那条链路动画的数据源。
   *  只属于当前活跃会话的现场回合，切走/重开即清 */
  journey: Array<{ key: string; detail: string }>;
  /** 目标面板的投影：这个应用里见过的目标——在推进的、暂停的、这一程收尾的。
   *  真相在各自的话题日志与台账里，这里只是看得见的那一份 */
  goalRuns: GoalPanelEntry[];
  conversations: ConversationMeta[];
  storage: StorageInfo | null;
  storageBusy: boolean;
  storageNote: string | null;
  storageError: string | null;

  sidebarCollapsed: boolean;
  panelCollapsed: boolean;
  panelTab: PanelTab;
  section: SidebarSection;
  /** 话题挂着的 Worktree，键是话题 id。真相源在后端注册表，这份只是投影 */
  worktrees: Record<string, WorktreeInfo>;
  /** 当前工作目录的分支清单（工作目录不是 git 仓库时 isRepo=false） */
  gitBranches: GitBranches | null;
  builtinTools: BuiltinTool[];
  toolsError: string | null;
  skills: Skill[];
  skillsDir: string;
  plugins: PluginView[];
  builtins: BuiltinView[];
  pluginsDir: string;
  pluginsError: string | null;
  mcpServers: McpServerView[];
  mcpError: string | null;
  skillsError: string | null;
  tasks: TaskView[];
  tasksError: string | null;
  projectDialogOpen: boolean;
  /**
   * 目标弹框的开合与草案材料。它不住在 ModePicker 的局部状态里，因为有两个入口
   * 在组件外面：模式选择器的「定一个目标」，和规划档那条「按这份方案立目标」
   * （§5.6 的接力——材料是方案全文，打开后自动走一次草案）
   */
  goalDialogOpen: boolean;
  goalDialogSource: string | null;
  setGoalDialog: (open: boolean, source?: string | null) => void;

  usageDays: UsageDays;
  usageReport: UsageReport | null;
  usageLoading: boolean;
  usageError: string | null;
  prices: ModelPrice[];
  pricesError: string | null;
  /** 价表增删改、一键导入进行中，按钮防抖用 */
  pricesBusy: boolean;
  /** 价表一键导入的结果文案 */
  pricesNote: string | null;

  ccswitchCandidates: CcswitchCandidate[];
  ccswitchLoading: boolean;
  ccswitchError: string | null;
  /** 正在导入的 sourceId，按钮防抖 */
  ccswitchImportingId: string | null;
  /** 最近一次供应商导入的结果文案（成功/失败） */
  ccswitchNote: string | null;

  mcpCandidates: McpCandidate[];
  mcpCandidatesLoading: boolean;
  mcpCandidatesError: string | null;
  /** "已导入 X 个，跳过 Y 个…" */
  mcpImportNote: string | null;

  skillCandidates: SkillCandidate[];
  skillCandidatesLoading: boolean;
  skillCandidatesError: string | null;
  skillImportNote: string | null;

  usageRows: UsageRequestRow[];
  usageRowsTotal: number;
  /** 当前页起始下标，页大小固定 20 */
  usageRowsOffset: number;
  usageRowsLoading: boolean;
  usageRowsError: string | null;

  /** 本次话题里 aglab 改过的文件，按文件聚合。只统计内置 write_file，
   *  MCP 工具写的文件不在其中——面板要把这条边界说清楚，不能假装是全量 */
  edits: FileEdit[];
  /** 台账读不出来时的说法。空着和读不到是两件事，不能混 */
  editsError: string | null;
  /** 预览面板里当前展开的那个文件的绝对路径。对话流中的汇总卡靠它一键定位 */
  previewTarget: string | null;
  /** 右上角告警队列。请求失败不再写进模型正文，改贴在这里 */
  toasts: AppToast[];

  bootstrap: () => Promise<void>;
  /** 轻量连接刷新：只取配置、探密钥、按（可能换了的）服务商重拉模型。
   *  档案切换专用——全量 bootstrap 会把历史/工具/插件/MCP 全家桶重刷一遍，
   *  切换时那些根本没变，用户只会感到卡 */
  refreshConnection: () => Promise<void>;
  /** 返回 false 表示后端拒了这个补丁（配置已回退，且右上角已经报过一句） */
  updateConfig: (patch: Partial<AppConfig>) => Promise<boolean>;
  setUi: (patch: Partial<AppConfig["ui"]>) => void;
  loadModels: () => Promise<void>;
  attachPaths: (paths: string[]) => Promise<void>;
  detachAttachment: (id: string) => void;
  attachImage: (image: { path: string; name: string; mime: string; bytes: number; previewDataUrl: string }) => void;
  decide: (id: string, approved: boolean) => Promise<void>;
  /** 回答一条挂起中的 ask_user 提问。答案原文进工具结果；后端说没这条了就提示 */
  answerQuestion: (id: string, answer: string) => Promise<void>;
  setVideoGenerationType: (type: MediaType) => void;
  /** 音乐会话的子模式（简单/自定义=音频模型；生成歌词=对话模型写词）。
   *  模型选择器按它过滤候选与写入 */
  musicSubMode: "simple" | "custom" | "write";
  setMusicSubMode: (mode: "simple" | "custom" | "write") => void;
  createProject: (name: string, path: string) => Promise<string | null>;
  chooseProject: (
    id: string,
    opts?: { rebindCurrent?: boolean },
  ) => Promise<void>;
  dropProject: (id: string) => Promise<void>;
  startConversation: (kind?: ConversationKind) => void;
  /** 生图/视频会话的发送：走独立的生成 REST 管线（images/videos generations），
   *  不进对话轮。产物作为带附件的助手气泡落进话题并存档 */
  sendMedia: (
    prompt: string,
    generation?: MediaType,
    material?: { lyrics?: string; instrumental?: boolean },
  ) => Promise<void>;
  /** 写当前会话的工作区草稿：输入每敲一笔就写一次——切换不是可预知的时机，
   *  草稿必须随时都在 store 里 */
  setMediaDraft: (patch: Partial<MediaDraft>) => void;
  stopMedia: () => void;
  openConversation: (id: string) => Promise<void>;
  deleteConversation: (id: string) => Promise<void>;
  /** 手动改话题标题。自动起的标题只是便利，用户的话才是定论 */
  renameConversation: (id: string, title: string) => Promise<void>;
  /** 置顶/取消置顶。置顶的话题在分组里排最前，落盘活过重启 */
  togglePin: (id: string) => Promise<void>;
  refreshHistory: () => Promise<void>;
  /** 重取编辑台账。一轮结束或切话题时刷，右栏预览与对话流汇总卡共用它 */
  refreshEdits: () => Promise<void>;
  /** 重读这一支的作业模式读数。真相在话题日志里，切话题与每轮收尾都要再问一次 */
  loadMode: (conversationId?: string) => Promise<void>;
  /**
   * 切作业模式（对话 / 规划）。返回错误文案，null = 成功。那一轮还在跑时不拒也不静默：
   * 请求寄存进后端的登记表，由那一轮收尾时落行，界面上说一句"这一轮收尾后生效"。
   * 目标不在这里——一格命令改一件事，定目标走 `goalSet`
   */
  setMode: (input: { mode: WorkingMode }) => Promise<string | null>;
  /**
   * 定目标：立一份完成契约并立刻开第一轮。返回错误文案，null = 成功。
   * 已有一支在推进的目标且写的不是同一句时要 `force`（弹框先确认）
   */
  goalSet: (input: {
    objective: string;
    criteria: CriterionInput[];
    constraints: string[];
    maxCostUsd?: string | null;
    profile?: string | null;
    force: boolean;
  }) => Promise<string | null>;
  /** 编辑目标：同一支换文字，账全留。返回错误文案，null = 成功 */
  goalEdit: (input: {
    objective?: string | null;
    criteria?: CriterionInput[] | null;
    constraints?: string[] | null;
  }) => Promise<string | null>;
  /** 暂停 / 恢复一个目标（目标面板与模式选择器共用）。返回错误文案，null = 成功 */
  goalPause: (conversationId: string, paused: boolean) => Promise<string | null>;
  /** 恢复一个目标并立刻接一轮（后端落续跑两行、空输入开轮）。返回错误文案，null = 成功 */
  goalResume: (conversationId: string) => Promise<string | null>;
  /** 结束目标：整份清掉，当前交互档保持。返回错误文案，null = 成功 */
  goalDiscard: (conversationId: string) => Promise<string | null>;
  /** 按话题点名停掉正在跑的那一轮（目标面板的「停止」）。没有在跑的回合就静默 */
  stopRun: (conversationId: string) => Promise<void>;
  /** 跳到右栏「预览」并展开某个文件。传 null 只收起，不切 tab */
  openPreview: (absPath: string | null) => void;
  pushToast: (toast: Omit<AppToast, "id">) => void;
  dismissToast: (id: string) => void;
  refreshStorage: () => Promise<void>;
  switchStorage: (backend: ConversationStore) => Promise<void>;
  setProjectDialogOpen: (open: boolean) => void;
  toggleSidebar: () => void;
  togglePanel: () => void;
  setPanelTab: (tab: PanelTab) => void;
  setSection: (section: SidebarSection) => void;
  /** 勾选 Worktree：基于所选分支开独立工作树。返回错误文案，null = 成功 */
  attachWorktree: (conversationId: string, baseBranch?: string) => Promise<string | null>;
  /** 摘掉工作树。树脏时后端拒绝，确认不要了传 force=true（改动不可恢复） */
  detachWorktree: (conversationId: string, force?: boolean) => Promise<string | null>;
  /** 读话题的绑定状态：打开话题/重启后同步勾选态 */
  refreshWorktree: (conversationId: string) => Promise<void>;
  /** 重取分支清单：绑定工作目录、切工作目录、开新话题后调 */
  refreshWorktreeBranches: () => Promise<void>;
  refreshTools: () => Promise<void>;
  toggleTool: (id: string, enabled: boolean) => Promise<void>;
  refreshSkills: () => Promise<void>;
  refreshPlugins: () => Promise<void>;
  togglePlugin: (id: string, enabled: boolean) => Promise<void>;
  /** 出厂扩展的整扩开关。技能级开关与普通技能共用 toggleSkill */
  toggleBuiltin: (id: string, enabled: boolean) => Promise<void>;
  toggleSkill: (id: string, enabled: boolean) => Promise<void>;
  /** 逐条确认钩子内容。记的是定义指纹，脚本改一个字节就得重新确认 */
  trustHook: (id: string, hash: string, trusted: boolean) => Promise<void>;
  toggleHook: (id: string, enabled: boolean) => Promise<void>;
  refreshMcp: () => Promise<void>;
  saveMcpServer: (server: McpServer) => Promise<void>;
  removeMcpServer: (id: string) => Promise<void>;
  toggleMcpServer: (id: string, enabled: boolean) => Promise<void>;
  toggleMcpTool: (exposed: string, enabled: boolean) => Promise<void>;
  connectMcp: (id: string) => Promise<void>;
  stopMcp: (id: string) => Promise<void>;
  /** 重开某台服务器以回读它的工具清单。会打断它正在跑的调用，所以只有界面上那一次确认会调它 */
  refreshMcpTools: (id: string) => Promise<void>;
  refreshTasks: () => Promise<void>;
  saveTask: (task: ScheduledTask) => Promise<void>;
  removeTask: (id: string) => Promise<void>;
  runTaskNow: (id: string) => Promise<void>;
  setUsageDays: (days: UsageDays) => Promise<void>;
  refreshUsage: () => Promise<void>;
  loadPrices: () => Promise<void>;
  /** 返回错误文案让弹层就地显示；null = 成功 */
  upsertPrice: (price: ModelPrice) => Promise<string | null>;
  removePrice: (modelId: string) => Promise<void>;
  /** cc-switch 价表一键导入，结果写 pricesNote */
  importPricing: () => Promise<void>;
  refreshCcswitchCandidates: () => Promise<void>;
  /** 成功 true。成功后依次刷 config → models → hasKey */
  importProvider: (sourceId: string) => Promise<boolean>;
  refreshMcpCandidates: () => Promise<void>;
  importMcpServers: (ids: string[]) => Promise<void>;
  refreshSkillCandidates: () => Promise<void>;
  importSkills: (ids: string[]) => Promise<void>;
  loadUsageRows: () => Promise<void>;
  setUsageRowsOffset: (offset: number) => Promise<void>;
  /** 拼串→save 对话框→落盘；返回错误文案或 null（取消不算错） */
  exportUsageCsv: () => Promise<string | null>;
  stopGeneration: () => Promise<void>;
  /** 生成中插话：乐观入列 + 后端排队。失败撤回乐观消息 */
  /** 取走还话槽里的正文（composer 把它接回草稿后调用） */
  clearDraftRestore: () => void;
  steer: (text: string) => Promise<void>;
  /** 排队到跟随队列：这一轮收尾后自动作为新输入开下一轮（pi 的 followUpMode） */
  followUp: (text: string) => Promise<void>;
  /** 从某条用户消息分叉出新话题：到那条为止的分支整体抄过去，分叉完直接打开 */
  forkFrom: (conversationId: string, entryId: string) => Promise<void>;
  /** 手动压缩：把当前话题的对话历史换成一份摘要 */
  compactConversation: () => Promise<void>;
  /** 重新生成最后一条回复：丢弃尾部助手消息，把触发它的用户消息重新发送 */
  regenerate: () => Promise<void>;
  /** 编辑某条用户消息并重发：该消息之后的内容全部丢弃，用新内容重新开始 */
  editAndResend: (messageId: string, content: string) => Promise<void>;
  /** 换到同一处的另一条分支：移动后端分支末端并重投影。不发请求、不删任何东西 */
  switchBranch: (messageId: string) => Promise<void>;
  send: (prompt: string, rewindTo?: string | null) => Promise<void>;
}

export const useChatStore = create<ChatState>((set, get) => {
  // 落过盘的话题内容指纹。只是"点开看看"时内容没变就不必再写一次——
  // 写了就会把 updated_at 顶新，侧栏按 updated_at 排序，列表于是自己上下跳
  const persisted = new Map<string, string>();
  /** 落过壳的会话：媒体会话的第一笔草稿才落壳，一个会话顶多落一次（防异步缝隙里连写） */
  const shellSaved = new Set<string>();
  /** openConversation 的时序闸：每次调用自增，只有最新一次允许写状态——
   *  慢加载期间用户又点了别条话题，过期回复在这里被丢弃（连点两条的串台就出在这） */
  let openConversationSeq = 0;

  /** send 的双发闸：占用判定与 `runs.set` 占上之间隔着记忆命令、修档这些 await，
   *  第一发悬在半空时第二发的判定看到的还是空的——两发都过闸，后到的
   *  `runs.set` 把先到的现场整个顶掉，先到那轮从此对事件与停止都失联。
   *  JS 单线程，判定+占下同一帧内完成就是原子的；按话题各占各的 */
  const sendGuard = new Set<string>();

  // 用量报表的请求序号：切时间窗时旧请求可能还在路上，
  // 回来晚了就丢掉，免得"近 7 天"的数据盖掉刚切的"全部"
  let usageRequestSeq = 0;

  // 请求明细分页的同款竞态保护
  let usageRowsSeq = 0;

  function fingerprintOf(record: ConversationRecord): string {
    // updatedAt 每次都是新的，不能进指纹，否则永远判成"变过"。
    // 以前是对整份 record 做 JSON.stringify——长会话每轮收尾要多付一整份序列化。
    // 指纹只需要"变没变"：对正文做一遍 djb2（不分配中间字符串），灵敏度等同全量比对
    let hash = 5381;
    for (const message of record.messages) {
      const content = message.content ?? "";
      for (let i = 0; i < content.length; i++) {
        hash = ((hash << 5) + hash + content.charCodeAt(i)) | 0;
      }
      hash = (hash + (message.attachments?.length ?? 0) + (message.steps?.length ?? 0)) | 0;
    }
    return `${record.messages.length}|${hash}|${record.usage?.durationMs ?? 0}`;
  }

  // 每个话题上次落盘的元信息。排序语义是"新增了消息才把话题顶上去"——
  // 仅仅切走看一眼（工具状态等非消息变化）不该把话题顶到列表最上面
  const lastSavedInfo = new Map<string, { updatedAt: number; messageCount: number }>();

  /** 在跑的每一轮，按话题归主。切走话题不等于中止这一轮：它的现场留在这里继续收增量，
   *  切回来直接接管。所以"这一轮的正文"自始至终只有一份真相 */
  const runs = new Map<string, LiveRun>();

  function setRunning(conversationId: string, running: boolean) {
    set((s) => {
      if (s.runningIds.includes(conversationId) === running) return {};
      return {
        runningIds: running
          ? [...s.runningIds, conversationId]
          : s.runningIds.filter((id) => id !== conversationId),
      };
    });
  }

  /** 生图/视频的停止等待标记，按话题归主：切走的那一发也要能被自己的话题认领，
   *  全局布尔在 per-run 身份下会张冠李戴。单飞（mediaBusy 互斥）不变。
   *  停止 = 不再等待与回填；端点上那次生成照常完成（已付费） */
  const mediaStopFlags = new Set<string>();

  const runFields = (run: LiveRun) => ({
    activeId: run.conversationId,
    projectId: run.projectId,
    title: run.title,
    kind: run.kind,
    messages: run.messages,
    offPath: run.offPath,
    usage: run.usage,
  });

  type RunPatch = Partial<Omit<LiveRun, "conversationId" | "settled" | "save">>;

  /** 把这一轮的现场投影到界面。它不是你正在看的那条话题时一个字段都不动——
   *  界面上那份属于别的话题，或者属于一条已经跑完的存档 */
  function syncRun(run: LiveRun) {
    if (get().activeId !== run.conversationId) return;
    set({
      title: run.title,
      messages: run.messages,
      usage: run.usage,
      pending: run.pending,
      followUpCount: run.followUpCount,
      mode: run.mode ?? null,
    });
  }

  /** 改某一轮的现场，再投影。run 已经交还给存档（迟到的事件）就整个不生效 */
  function patchRunOf(run: LiveRun, patch: RunPatch | ((run: LiveRun) => RunPatch)) {
    if (runs.get(run.conversationId) !== run) return;
    Object.assign(run, typeof patch === "function" ? patch(run) : patch);
    syncRun(run);
    refreshGoalPanels();
  }

  // ── 目标面板 ─────────────────────────────────────────────────────────────
  // 面板是一份投影：真相在各自的话题日志与台账里。三个来源拼成 goalRuns——
  // 正在跑的现场（`runs`）、眼前这条话题的读数（`mode`）、以及面板自己记过的
  // 目标（`goalModes`：「继续」开出的广播轮、暂停着的、这一程收尾的）。
  // 目标挂在话题上，与那条话题当下是对话/规划/目标哪一档无关。

  /** 面板尾巴的长度。只留末端：目标一轮的话往往比整屏长，头顶那截谁也不看 */
  const GOAL_TAIL_CHARS = 700;

  /** 面板记过的目标。`tracked` = 这个目标是这一程里活动过的（事件/按钮/恢复开轮），
   *  只是从存档路过的收尾目标不进面板——面板管"现在怎么样"，不管考古 */
  const goalModes = new Map<string, { state: ModeState; tracked: boolean }>();
  /** 「继续」开出的广播轮的输出尾巴（chat-event 按 conversationId 归主） */
  const goalTails = new Map<string, string>();
  /** 回合还在跑时按下的暂停：那一行要等收尾才落，界面先自己标着 */
  const goalOptimisticPaused = new Set<string>();
  /** 「继续」开出的那一轮还在跑（后端不说"还有下一轮"就不摘） */
  const goalKicked = new Set<string>();
  /** 后端自己开的那一轮（定目标即开工 / 「继续」）没有本地现场，事件从广播里来。
   *  这一格记"正看着这一支时，那一轮接出来的活气泡是哪一条"。它不落盘：收尾后
   *  `refreshActiveThread` 会用盘上那一份换掉它 */
  const broadcastBubbles = new Map<string, string>();
  /** 那一轮是第几轮。开工时按读数记一次，此后只由 `Mode{continuing}` 往前推——
   *  轮次只有这一个出处，界面上不许再算第二遍 */
  const broadcastRounds = new Map<string, number>();
  let goalEventsWired = false;
  let goalPanelsSignature = "";

  function goalTailOf(messages: Message[]): string {
    for (let index = messages.length - 1; index >= 0; index -= 1) {
      const message = messages[index];
      if (message.role === "assistant" && message.content.trim().length > 0) {
        return message.content.slice(-GOAL_TAIL_CHARS);
      }
    }
    return "";
  }

  /** 拿到这一支的活气泡 id，没有就开一条空的。正文、思考、工具三路都从这儿认领——
   *  三处各写一遍"开气泡"就会出现三条气泡 */
  function ensureBroadcastBubble(id: string): string {
    const held = broadcastBubbles.get(id);
    if (held) return held;
    const message: Message = {
      id: newId("msg"),
      role: "assistant",
      content: "",
      createdAt: Date.now(),
      streaming: true,
      toolCalls: [],
      goalRound: broadcastRounds.get(id),
    };
    broadcastBubbles.set(id, message.id);
    set((state) => ({ messages: [...state.messages, message] }));
    return message.id;
  }

  /** 把广播轮的正文增量接进活气泡。只在"正看着这一支、而且它没有本地现场"时叫：
   *  人自己发的那句话开出的循环由 `apply` 分幕，两处都接就会出现双份正文 */
  function appendBroadcastDelta(id: string, text: string) {
    const held = ensureBroadcastBubble(id);
    // 正文开流，思考那格的"正在想"就收了——同一时刻只有一种流在出字
    set((state) => ({
      messages: state.messages.map((message) =>
        message.id === held
          ? { ...message, content: message.content + text, reasoningStreaming: false }
          : message,
      ),
    }));
  }

  /** 把广播轮的思考增量接进活气泡。气泡还没有正文时才标"正在想"——
   *  正文与思考在同一格气泡里的先后由事件顺序决定，这里不猜 */
  function appendBroadcastReasoning(id: string, text: string) {
    const held = ensureBroadcastBubble(id);
    set((state) => ({
      messages: state.messages.map((message) =>
        message.id === held
          ? {
              ...message,
              reasoning: (message.reasoning ?? "") + text,
              reasoningStreaming: message.content.length === 0,
            }
          : message,
      ),
    }));
  }

  /** 把广播轮的工具事件接进活气泡：同 id 更新状态（running → done/failed/denied），
   *  没见过的 id 追加一张卡。工具卡与正文共用一条气泡，与 Channel 路同形状 */
  function patchBroadcastTool(id: string, call: ToolCall) {
    const held = ensureBroadcastBubble(id);
    set((state) => ({
      messages: state.messages.map((message) => {
        if (message.id !== held) return message;
        const calls = message.toolCalls ?? [];
        const index = calls.findIndex((item) => item.id === call.id);
        return {
          ...message,
          toolCalls:
            index < 0
              ? [...calls, call]
              : calls.map((item, i) => (i === index ? { ...item, ...call } : item)),
        };
      }),
    }));
  }

  /** 那一轮收口：活气泡落定，盘上那份由 `refreshActiveThread` 换进来。
   *  不清这一格的话下一轮的字节会接着往这条已收尾的气泡后面写 */
  function settleBroadcastBubble(id: string) {
    const held = broadcastBubbles.get(id);
    if (!held) return;
    broadcastBubbles.delete(id);
    set((state) => ({
      messages: state.messages.map((message) =>
        message.id === held
          ? { ...message, streaming: false, reasoningStreaming: false }
          : message,
      ),
    }));
  }

  /** 面板与目标带上那一格状态。**乐观的"暂停中"只覆盖 `active` 这一格**：人在回合
   *  跑着时按了暂停，那一行要等收尾才落，此刻读数还写着 active。已经停住或收尾的
   *  不许被覆盖——那会把"预算花完"显示成"已暂停"，而这两格的出路完全不同
   *  （一个是去调上限，一个是按继续） */
  function statusForPanel(mode: ModeState, optimisticPaused: boolean): GoalStatus {
    return optimisticPaused && mode.status === "active" ? "paused" : mode.status;
  }

  /**
   * 启动扫描（D8）：把这一程挂着的目标请进面板。
   *
   * 没有这一步，重启之后"别处还挂着一支会自己花钱的目标"这件事在屏幕上是不存在的——
   * 面板此前只认这一程被事件或按钮碰过的话题。后端顺手把推进中的落成已暂停，
   * 那一格由人按「继续」才翻回来（设置里可以开回自动继续，默认关）
   */
  async function loadGoalOverview() {
    let rows;
    try {
      rows = await goalsOverview();
    } catch {
      // 扫不到就当没有：面板少几条，比启动卡在一句报错上强
      return;
    }
    if (!Array.isArray(rows)) return;
    for (const row of rows) {
      noteGoalMode(
        row.conversationId,
        {
          mode: row.mode,
          objective: row.objective,
          turnsUsed: row.turnsUsed,
          maxCostUsdE8: row.maxCostUsdE8,
          spentUsdE8: row.spentUsdE8,
          status: row.status,
          note: row.note,
          profile: row.profile,
          goalId: row.goalId,
          contract: row.contract,
          planReady: false,
        },
        true,
      );
    }
    const parked = rows.filter((row) => row.parkedByRestart);
    if (parked.length > 0) {
      get().pushToast({
        tone: "info",
        title:
          parked.length === 1
            ? "一支目标还挂着，已按暂停"
            : `${parked.length} 支目标还挂着，已按暂停`,
        detail: "重启不替你重按播放键。在目标带上按「继续」才往下跑。",
      });
    }
  }

  /** 记一条读数。`tracked` 只是"要不要因此把它请进面板"：已经在面板里的不会掉出去 */
  function noteGoalMode(id: string, state: ModeState, tracked: boolean) {
    // 认的是"这一支身上挂着目标"，不是"当下处于目标档"：目标属于话题，
    // 对话/规划只是当下怎么交互——切档既不清零也不挂起
    if (!state.objective) {
      goalModes.delete(id);
      goalOptimisticPaused.delete(id);
      goalKicked.delete(id);
      refreshGoalPanels();
      return;
    }
    const previous = goalModes.get(id);
    goalModes.set(id, { state, tracked: tracked || (previous?.tracked ?? false) });
    // 读数带着"已暂停"来了：乐观那一格可以收了
    // 读数已经不是"在推进"了：那一行落下去了，乐观那一格可以收
    if (state.status !== "active") goalOptimisticPaused.delete(id);
    refreshGoalPanels();
  }

  /**
   * 切档 / 定目标 / 编辑目标三条命令共用的收尾。后端回的是同一个 `ModeOutcome`，
   * 界面的后手也该是同一套：补现场读数、认领"定目标即开工"那一轮、面板记账、
   * 排队时说一句——三处各抄一遍，迟早漂成三种形状
   */
  function settleModeOutcome(outcome: ModeOutcome, ownerId: string): string | null {
    const state = outcome.view;
    const run = runs.get(ownerId);
    if (run) patchRunOf(run, { mode: state });
    if (get().activeId === ownerId) set({ mode: state, modeBusy: false });
    else set({ modeBusy: false });
    // 定目标即开工：落下的是"一支在推进的目标"，后端已经顺手开了第一轮（与「继续」
    // 同一条路）。这一格必须跟着举起来——它是"续跑轮没有界面现场"那条路由的凭据，
    // 不举的话人再发一句话就会开出第二个写者，同写一份日志
    if (!outcome.deferred && state.mode === "goal" && state.status === "active") {
      goalTails.delete(ownerId);
      goalKicked.add(ownerId);
      broadcastRounds.set(ownerId, state.turnsUsed);
    }
    // 用户亲手发的命令：面板最后记账——重建时读的才是落完的那一份
    noteGoalMode(ownerId, state, true);
    // 那一轮还在跑：这一行等收尾才落，读数此刻还是旧档位。说一句就走——
    // "点了没反应"与"点了、正在等这一轮收口"在屏幕上必须长得不一样
    if (outcome.deferred) {
      get().pushToast({
        tone: "info",
        title: "排在这一轮后面",
        detail: "这一轮还在跑。它收尾时这一次改动就落下去，目标不受影响。",
      });
    }
    return null;
  }

  /** 重建 goalRuns。带签名比对：没有变化就不 set，别让每个增量都摇一遍面板 */
  function refreshGoalPanels() {
    const state = get();
    const titleOf = (id: string) =>
      state.conversations.find((item) => item.id === id)?.title ?? "话题";
    /** 一条读数折成卡上的一条。目标就住在读数的主格里——它不随交互档搬家，
     *  所以这里也没有第二份落点要认 */
    const entryOf = (
      id: string,
      title: string,
      mode: ModeState,
      isPending: boolean,
      tail: string,
    ): GoalPanelEntry => {
      return {
        conversationId: id,
        title,
        mode: mode.mode,
        objective: mode.objective ?? "",
        turnsUsed: mode.turnsUsed,
        maxCostUsdE8: mode.maxCostUsdE8,
        spentUsdE8: mode.spentUsdE8,
        status: statusForPanel(mode, goalOptimisticPaused.has(id)),
        profile: mode.profile,
        goalId: mode.goalId,
        note: mode.note,
        pending: isPending,
        tail,
      };
    };
    /** 这份读数算不算一条目标：身上挂着目标就算，在哪一档都一样 */
    const isGoal = (mode: ModeState) => mode.objective !== null;
    const entries = new Map<string, GoalPanelEntry>();

    // 1) 正在跑的现场（含被切走的那条）：读数与尾巴都取自 run，它是最新的那份
    for (const [id, run] of runs) {
      const mode = run.mode;
      if (!mode || !isGoal(mode)) continue;
      entries.set(id, entryOf(id, run.title || titleOf(id), mode, run.pending, goalTailOf(run.messages)));
    }
    // 2) 眼前这条话题：目标刚定下还没发第一句的形态只有这里有读数。
    //    「继续」开出的广播轮不走 Channel，state.pending 看不见它——按 kicked 旗并进来
    const activeMode = state.mode;
    if (activeMode && isGoal(activeMode) && !entries.has(state.activeId)) {
      entries.set(
        state.activeId,
        entryOf(
          state.activeId,
          state.title,
          activeMode,
          state.pending || goalKicked.has(state.activeId),
          goalTails.get(state.activeId) ?? goalTailOf(state.messages),
        ),
      );
    }
    // 3) 面板记过的目标：暂停着的、恢复开出的广播轮、这一程收尾的
    for (const [id, cached] of goalModes) {
      if (entries.has(id) || !isGoal(cached.state)) continue;
      // 只是从存档路过、这一程没活动过的收尾目标不进面板——面板管"现在怎么样"
      if (cached.state.status !== "active" && !cached.tracked) continue;
      entries.set(
        id,
        entryOf(id, titleOf(id), cached.state, goalKicked.has(id), goalTails.get(id) ?? ""),
      );
    }

    const ordered = [...entries.values()];
    const signature = JSON.stringify(ordered);
    if (signature === goalPanelsSignature) return;
    goalPanelsSignature = signature;
    set({ goalRuns: ordered });
  }

  /** 「继续」那一轮从后端广播（chat-event）来，没有 Channel 收着：这里按话题 id
   *  认领目标话题的增量，喂给面板的尾巴与读数。任务与编排的后台运行有各自的面板，
   *  不进这份账 */
  function wireGoalEvents() {
    if (goalEventsWired) return;
    goalEventsWired = true;
    void listen<{ conversationId: string; event: ChatEvent }>("chat-event", ({ payload }) => {
      const { conversationId, event } = payload;
      if (!goalModes.has(conversationId)) return;
      // 思考增量：正看着这一支就接进活气泡（"正在想"那格面板要认）；
      // 关了思考显示就连流式缓冲都不进——与 Channel 路同一句规矩。
      // 没在看的那一支不攒思考：尾巴只留产出，思考积起来谁也不看
      if (event.type === "reasoning") {
        if (!get().config.showReasoning) return;
        if (conversationId === get().activeId && !runs.has(conversationId)) {
          appendBroadcastReasoning(conversationId, event.text);
        }
        return;
      }
      // 工具事件：正看着就上/更新工具卡（文件读、命令跑、写文件，全在这条气泡里
      // 看得见过程）；没在看的那一支把"跑完了什么"记一截尾巴——角落卡是它唯一的出口
      if (event.type === "tool") {
        if (conversationId === get().activeId && !runs.has(conversationId)) {
          patchBroadcastTool(conversationId, {
            id: event.id,
            name: event.name,
            status: event.status,
            risk: event.risk,
            input: event.input,
            output: event.output,
            arguments: event.arguments,
            passReason: event.passReason,
          });
        } else if (event.status === "done") {
          goalTails.set(
            conversationId,
            `${goalTails.get(conversationId) ?? ""}\n[${event.name}] 完成`.slice(
              -GOAL_TAIL_CHARS,
            ),
          );
          refreshGoalPanels();
        }
        return;
      }
      if (event.type === "plan") {
        // 后台轮也会报计划：记下最新一份，卡片只认当前正在看的这条
        set((s) => ({
          plans: { ...s.plans, [conversationId]: { explanation: event.explanation, steps: event.steps } },
        }));
        return;
      }
      if (event.type === "ask") {
        // 后台轮的提问也挂进桶里：用户切回这条话题时卡片才接得上后端正在等的那一发
        set((s) => ({
          pendingQuestions: {
            ...s.pendingQuestions,
            [conversationId]: { id: event.id, question: event.question, options: event.options },
          },
        }));
        return;
      }
      if (event.type === "delta") {
        // 正看着这一支、而它又是后端自己开的轮：字节接成话题里的活气泡——这才是
        // "在当前话题里执行显示"。没在看的那一支才留一截尾巴，角落卡是它唯一的出口
        if (conversationId === get().activeId && !runs.has(conversationId)) {
          appendBroadcastDelta(conversationId, event.text);
        } else {
          goalTails.set(
            conversationId,
            `${goalTails.get(conversationId) ?? ""}${event.text}`.slice(-GOAL_TAIL_CHARS),
          );
        }
        refreshGoalPanels();
        return;
      }
      if (event.type === "mode") {
        // 每一发 Mode 都是一次轮次边界（后端在收尾时发它，紧跟着 Done）：活气泡到此
        // 落定，下一轮的字节另开一条。不在这儿收口的话第二轮的正文会接在第一条后面，
        // 屏幕上就是一条越写越长的气泡，看不出它其实是两轮
        settleBroadcastBubble(conversationId);
        // 后端说"还有下一轮"就别摘"推进中"——轮与轮之间那半秒不该闪
        if (event.continuing) {
          goalKicked.add(conversationId);
          // 收尾这行报的是刚跑完那一轮的账，所以往下接的那一轮是它 +1
          broadcastRounds.set(conversationId, event.state.turnsUsed + 1);
        } else {
          goalKicked.delete(conversationId);
          broadcastRounds.delete(conversationId);
        }
        // 眼前就是这条话题时，界面那份读数同步跟上。不跟的话面板第二优先级
        // 会拿屏上的旧读数盖回去：目标明明报完了，卡片却停在"推进中"，
        // 要等下一轮 mode 事件才翻面——正是那次"完成后不自动显示完成"的根源
        if (conversationId === get().activeId) set({ mode: event.state });
        noteGoalMode(conversationId, event.state, true);
        return;
      }
      if (event.type === "error") {
        goalKicked.delete(conversationId);
        goalOptimisticPaused.delete(conversationId);
        broadcastRounds.delete(conversationId);
        settleBroadcastBubble(conversationId);
        goalTails.set(
          conversationId,
          `${goalTails.get(conversationId) ?? ""}\n[服务商报错] ${event.message}`.slice(
            -GOAL_TAIL_CHARS,
          ),
        );
        refreshGoalPanels();
        return;
      }
      // 收尾/出错之后把屏上这份投影刷到存档的当下——只刷正在看的这条，
      // 不落盘（存档由后端收尾写，这里只读回屏上）
      if (
        (event.type === "done" || event.type === "compaction" || event.type === "notice") &&
        conversationId === get().activeId &&
        !runs.has(conversationId)
      ) {
        // 活气泡先落定，再让盘上那一份换进来。顺序反了不会双份（refresh 整替换），
        // 但会闪一下"生成中"的尾巴在已经落盘的正文上
        settleBroadcastBubble(conversationId);
        void refreshActiveThread(conversationId);
      }
    });
  }

  /** 把某条话题的存档重新投影到屏上（不落盘、不换话题）。「继续」开出的那一轮
   *  不经过 Channel，正在看这条话题的人要等这一次刷才能看到新话 */
  async function refreshActiveThread(id: string) {
    if (get().activeId !== id || runs.has(id)) return;
    try {
      const restored = await historyLoad(id);
      const nodes = restored.messages;
      let thread = nodes;
      let offPath: Message[] = [];
      try {
        const tree = await fetchConversationTree(id);
        const tip = tree.tip
          ? nodes.find((message) => message.entryIds?.includes(tree.tip!))?.id ?? null
          : null;
        ({ thread, offPath } = partition(nodes, tip ?? nodes[nodes.length - 1]?.id ?? null));
      } catch {
        // 读不到树：整份按插入序显示
      }
      if (get().activeId !== id || runs.has(id)) return;
      set({
        title: restored.title,
        projectId: restored.projectId,
        messages: thread,
        offPath,
        usage: restored.usage,
        pending: false,
        followUpCount: 0,
      });
      void get().loadMode(id);
    } catch {
      // 存档读不到就维持屏上那份：面板的读数与尾巴还开着，不差这一下
    }
  }

  /** 目标线程还在跑时的发送路由：这句话**不开并行回合**——「继续」/自动续跑开出的
   *  目标轮没有界面现场（pending 看不见它），此时再 chat_send 一条就是第二个写者
   *  各持一份副本同写一份日志，后收尾的整片盖掉先收尾的。走插话：人的话永远优先，
   *  它折进当前这一轮。要排到后面去是 Ctrl+回车那一条路，由人明说。
   *  气泡落现场（有 run 进 run，切走也不丢）；广播轮落屏上那份，收尾刷存档时对齐 */
  async function sendIntoGoalThread(text: string) {
    const conversationId = get().activeId;
    const parked = runs.get(conversationId);
    const attached = parked !== undefined && !parked.settled;
    const message: Message = {
      id: newId("msg"),
      role: "user",
      content: text,
      createdAt: Date.now(),
    };
    if (attached) {
      patchRunOf(parked, (r) => ({ messages: [...r.messages, message] }));
    } else {
      set((s) => ({ messages: [...s.messages, message] }));
    }
    const retract = () => {
      if (attached) {
        patchRunOf(parked, (r) => ({
          messages: r.messages.filter((item) => item.id !== message.id),
        }));
      } else {
        set((s) => ({ messages: s.messages.filter((item) => item.id !== message.id) }));
      }
      set({ draftRestore: text });
    };
    try {
      await chatSteer(conversationId, text);
    } catch (error) {
      // 入队失败撤回乐观气泡：不能让人以为话已经送进去了。话不丢——放回输入框
      retract();
      get().pushToast({
        tone: "info",
        title: "这句话没送进去",
        detail: `${error instanceof Error ? error.message : String(error)} 已放回输入框。`,
      });
    }
  }

  /** 每轮都问一遍模型"这次有什么值得记"，钱和延迟都不划算。
   *  设计要的是"话题结束或每 N 轮"，这里取每 3 次往返一次 */
  const EXTRACT_EVERY = 3;
  let extractTurns = 0;

  async function persistConversation(
    state: {
      activeId: string;
      projectId: string;
      title: string;
      messages: Message[];
      /** 不在当前分支上的那些气泡。存档连着它们一起写——只写看得见的那条，切走的分支
       *  就在盘上没了，而后端日志里还在（那正是这一轮要终结的双轨真相） */
      offPath: Message[];
      usage?: Usage;
      /** 能力档随存档走：漏了它后端按 default 落成 "chat"，生图会话被洗成对话 */
      kind: ConversationKind;
    },
    // 新话题开工前的那次预落盘是**壳**（只有标题，一条消息都没有）：
    // 它的任务是让侧栏立刻看得见，消息行归后端的日志写。护栏默认照旧——
    // 不许误写空话题，只有点名要壳的那一处过得去
    opts?: { allowShell?: boolean },
  ) {
    if (state.messages.length === 0 && !opts?.allowShell) return;

    const record: ConversationRecord = {
      id: state.activeId,
      projectId: state.projectId,
      title: state.title,
      // 能力档随存档走（conversation kind）
      kind: state.kind,
      createdAt: state.messages[0]?.createdAt ?? Date.now(),
      updatedAt: Date.now(),
      // 置顶住在话题列表的元信息里：落盘时带回去，不然下一次保存会把它洗掉
      pinned: get().conversations.find((item) => item.id === state.activeId)?.pinned ?? false,
      // 存档里剥掉缩略图：一张截图的 data URL 是几百 KB，进档就是膨胀；
      // 元数据保留（kind/path/bytes），重开话题走 asset 协议从 path 读图
      // （剧本正文不随气泡走：发送时已拼进生成提示词，附件格上只有元数据）
      messages: [...state.messages, ...state.offPath].map((message) =>
        message.attachments
          ? {
              ...message,
              attachments: message.attachments.map(
                ({ previewDataUrl: _preview, ...rest }) => rest,
              ),
            }
          : message,
      ),
      usage: state.usage,
    };

    // 排序时间戳：新增了消息才推进；否则沿用上次落盘的位置，切来切去列表顺序不动。
    // 上次信息缺失时（恢复回来的话题）从列表元信息回退，messageCount 对得上
    const previous =
      lastSavedInfo.get(record.id) ??
      (() => {
        const meta = get().conversations.find((item) => item.id === record.id);
        return meta
          ? { updatedAt: meta.updatedAt, messageCount: meta.messageCount }
          : undefined;
      })();
    const grew = !previous || record.messages.length > previous.messageCount;
    record.updatedAt = grew ? Date.now() : previous.updatedAt;

    const stamp = fingerprintOf(record);
    if (persisted.get(record.id) === stamp) {
      lastSavedInfo.set(record.id, {
        updatedAt: record.updatedAt,
        messageCount: record.messages.length,
      });
      return;
    }

    try {
      const meta = await historySave(record);
      persisted.set(record.id, stamp);
      lastSavedInfo.set(record.id, {
        updatedAt: record.updatedAt,
        messageCount: record.messages.length,
      });
      set((state) => ({
        conversations: [meta, ...state.conversations.filter((item) => item.id !== meta.id)]
          .sort((a, b) => b.updatedAt - a.updatedAt)
          .slice(0, 200),
      }));
    } catch (error) {
      console.error("话题落盘失败", error);
    }
  }

  /** 把界面上正在看的这条话题落盘。正在跑的那一轮不走这里（见 `endRun`），
   *  它的真相在 `runs` 里，半截正文不该进存档 */
  const persistCurrent = () => persistConversation(get());

  /** 对话轮是否还在飞：pending 只盖"当下这条正输入的"，runs 从登记到 settled
   *  之间（跟随幕间、目标轮间隙）切分支/重新生成会跟回合线程竞写同一份日志 */
  const roundInFlight = (conversationId: string) => {
    const run = runs.get(conversationId);
    return Boolean(run && !run.settled);
  };

  /** 切到某个能力档时把该档记住的模型换上来。三段行为：
   *  1. 该档有记忆且不同 → 换上它，同时把离开档的记忆补好（空槽且当前模型
   *     匹配离开档的能力时才记——别把生图模型记成对话档的常驻）；
   *  2. 该档没记忆 → 从档案模型表里找第一个能力匹配的换上（用户挑表的顺序
   *     就是优先级），并把当前模型种进离开档的空槽；
   *  3. 找不到能力匹配的模型（比如视频模型一个都没配）→ 保持现状，该档记忆
   *     也不污染。回退永不写回原始声明 */
  function syncModelForKind(nextKind: ConversationKind, previousKind?: ConversationKind) {
    const config = get().config;
    // 老会话的 kind 是空串：归一成 chat，否则档位记忆与徽标全部错位
    const target: ConversationKind = nextKind || "chat";
    const current = config.model;
    const wanted = config.kindModels?.[target];
    const prevKind = previousKind ?? target;
    const specs = config.profiles.flatMap((profile) => profile.models);
    const kindModels = { ...config.kindModels };
    // 本函数对档位记忆的**意图增量**：写回时合并到最新 config 上，而不是整包
    // 覆盖快照——快速连续切档时，慢一拍的整包补丁会把别档刚记住的模型洗掉
    const kindModelsDelta: Partial<Record<ConversationKind, string>> = {};
    const prevSlotEmpty = !kindModels[prevKind];
    const currentMatchesPrev = capabilitiesOf(current, specs).includes(
      capabilityForKind(prevKind),
    );
    if (prevSlotEmpty && prevKind !== target && currentMatchesPrev) {
      kindModels[prevKind] = current;
      kindModelsDelta[prevKind] = current;
    }
    // 自愈：历史版本写过"档案A×模型B"的幽灵 pinned（成员表里不存在，Rust 每发必拒，
    // 真机踩过）。切档读到就顺手修好——同名模型找真实归属，池里没人带这个模型就退回自动调度
    if (pinnedIsGhost(config.modelPool)) {
      const repaired = switchPinnedMember(config.modelPool, wanted || current);
      void get().updateConfig(
        repaired
          ? { modelPool: repaired }
          : {
              modelPool: {
                ...config.modelPool,
                mode: "auto" as const,
                pinned: null,
              },
            },
      );
    }
    if (wanted && wanted !== current) {
      // 池处于"手动指定"时，固定成员才是请求真正用的模型——切档必须连它一起换，
      // 否则 config.model 换了也被池盖住（真机踩过：三个会话全显示池里固定的那个）。
      // pinned 是（档案×模型）成对定位：只换模型名会写出幽灵对（真机踩过，见
      // switchPinnedMember 注释），必须落到成员表里真实存在的组合
      const nextPool = switchPinnedMember(config.modelPool, wanted);
      void get().updateConfig({
        model: wanted,
        kindModels: { ...get().config.kindModels, ...kindModelsDelta },
        ...(nextPool ? { modelPool: nextPool } : {}),
      });
      return;
    }
    if (wanted) return;
    // 该档没记忆时的三层回退：当前服务商的勾选表 → 池里启用的成员 → 其他档案的
    // 勾选表。媒体模型常住在别的档案（生图/音乐各归各站），只看当前服务商的话，
    // 切档时找不到候选，选择器就一直挂着上一个模式的模型（真机踩过：音乐会话
    // 显示着视频模型 veo）。跨档案的模型发送时按归属档案路由，选了就能用
    const capability = capabilityForKind(nextKind);
    const activeProfileId = config.activeProfileId ?? "";
    const activeCandidate = config.models
      .map((spec) => spec.model.trim())
      .find(
        (name) =>
          name !== "" &&
          name !== current &&
          capabilitiesOf(name, specs).includes(capability),
      );
    if (activeCandidate) {
      kindModels[nextKind] = activeCandidate;
      kindModelsDelta[nextKind] = activeCandidate;
      // 同上：pinned 成对换，不写幽灵组合
      const nextPool = switchPinnedMember(config.modelPool, activeCandidate);
      void get().updateConfig({
        model: activeCandidate,
        kindModels: { ...get().config.kindModels, ...kindModelsDelta },
        ...(nextPool ? { modelPool: nextPool } : {}),
      });
      return;
    }
    const memberCandidate = config.modelPool.members.find(
      (member) =>
        member.enabled &&
        member.model !== current &&
        capabilitiesOf(member.model, specs).includes(capability),
    );
    const otherCandidate = config.profiles
      .filter((profile) => profile.id !== activeProfileId)
      .flatMap((profile) =>
        profile.models.map((spec) => ({
          profileId: profile.id,
          model: spec.model.trim(),
        })),
      )
      .find(
        (entry) =>
          entry.model !== "" &&
          entry.model !== current &&
          capabilitiesOf(entry.model, specs).includes(capability),
      );
    const chosen = memberCandidate
      ? { profileId: memberCandidate.profileId, model: memberCandidate.model }
      : otherCandidate;
    if (!chosen) return;
    kindModels[nextKind] = chosen.model;
    kindModelsDelta[nextKind] = chosen.model;
    // 池手动指定时固定成员成对跟上（只换模型名会写幽灵组合）；池关/自动时
    // 固定不参与，配置 model 即显示与发送的真相
    const nextPool = switchPinnedMember(config.modelPool, chosen.model);
    void get().updateConfig({
      model: chosen.model,
      kindModels: { ...get().config.kindModels, ...kindModelsDelta },
      ...(nextPool ? { modelPool: nextPool } : {}),
    });
  }

  function startFresh(kind: ConversationKind = "chat") {
    set({
      activeId: newId("conv"),
      projectId: get().config.activeProjectId,
      title: "新话题",
      kind,
      messages: [],
      offPath: [],
      attachments: [],
      usage: undefined,
      pending: false,
      journey: [],
      mediaBusy: false,
      draftRestore: null,
      videoGenerationType: "video",
      followUpCount: 0,
      // 新话题没有模式那一行：读数为 null 就是"对话"，不从上一支继承目标
      mode: null,
      modeError: null,
      edits: [],
    });
    // 旧话题那条"眼前读数"形态的面板条目该退场了：它要么已在 runs/goalModes 里有据，
    // 要么就是一条从没跑起来的目标
    refreshGoalPanels();
  }

  return {
    config: FALLBACK_CONFIG,
    configLoaded: false,
    models: [],
    modelsError: null,
    loadingModels: false,
    hasKey: false,

    activeId: newId("conv"),
    projectId: "",
    title: "新话题",
    kind: "chat",
    messages: [],
    offPath: [],
    attachments: [],
    pending: false,
    mediaBusy: false,
    draftRestore: null,
    videoGenerationType: "video",
    mediaDrafts: {},
    musicSubMode: "simple",
    followUpCount: 0,
    mode: null,
    modeError: null,
    modeBusy: false,
    runningIds: [],
    deciding: [],
    plans: {},
    pendingQuestions: {},
  journey: [],
    goalRuns: [],
    conversations: [],
    storage: null,
    storageBusy: false,
    storageNote: null,
    edits: [],
    editsError: null,
    previewTarget: null,
    toasts: [],
    storageError: null,

    sidebarCollapsed: false,
    panelCollapsed: true,
    panelTab: "decision",
    section: "chats",
    worktrees: {},
    gitBranches: null,
    builtinTools: [],
    toolsError: null,
    skills: [],
    skillsDir: "",
    plugins: [],
    builtins: [],
    pluginsDir: "",
    pluginsError: null,
    mcpServers: [],
    mcpError: null,
    skillsError: null,
    tasks: [],
    tasksError: null,
    projectDialogOpen: false,
    goalDialogOpen: false,
    goalDialogSource: null,

    usageDays: 7,
    usageReport: null,
    usageLoading: false,
    usageError: null,
    prices: [],
    pricesError: null,
    pricesBusy: false,
    pricesNote: null,

    ccswitchCandidates: [],
    ccswitchLoading: false,
    ccswitchError: null,
    ccswitchImportingId: null,
    ccswitchNote: null,

    mcpCandidates: [],
    mcpCandidatesLoading: false,
    mcpCandidatesError: null,
    mcpImportNote: null,

    skillCandidates: [],
    skillCandidatesLoading: false,
    skillCandidatesError: null,
    skillImportNote: null,

    usageRows: [],
    usageRowsTotal: 0,
    usageRowsOffset: 0,
    usageRowsLoading: false,
    usageRowsError: null,

    bootstrap: async () => {
      const [config, hasKey] = await Promise.all([fetchConfig(), probeCredential()]);
      set({
        config,
        configLoaded: true,
        hasKey,
        sidebarCollapsed: config.ui.sidebarCollapsed,
        panelCollapsed: config.ui.panelCollapsed,
        panelTab: isPanelTab(config.ui.panelTab) ? config.ui.panelTab : "decision",
        section: isSection(config.ui.section) ? config.ui.section : "chats",
      });
      // 「继续」开出的目标轮从后端广播：面板的实时尾巴靠这一条线喂
      wireGoalEvents();
      // 启动扫描：别处挂着的目标要看得见（它顺手把推进中的落成已暂停）
      void loadGoalOverview();
      void get().refreshTools();
      void get().refreshSkills();
      void get().refreshPlugins();
      void get().refreshMcp();
      void get().refreshTasks();
      void get().refreshWorktreeBranches();

      try {
        let conversations = await historyList();
        // 空壳清场，只在启动这一口做（refreshHistory 不做，别误删正在写的）：
        // 0 条消息的存档里没有任何内容——媒体会话的草稿只住内存，重启后空壳连
        // "接着写"的可能都没有，留着只会把侧栏变成一排空"新话题"
        const stale = conversations.filter((item) => item.messageCount === 0);
        if (stale.length > 0) {
          // 清理是纯后台动作，不 await——它不该挡住用户看到第一屏对话
          void Promise.allSettled(stale.map((item) => historyRemove(item.id)));
          const staleIds = new Set(stale.map((item) => item.id));
          conversations = conversations.filter((item) => !staleIds.has(item.id));
        }
        set({ conversations });

        if (conversations.length === 0) {
          startFresh();
          return;
        }

        const restored = await historyLoad(conversations[0].id);
        set({
          activeId: restored.id,
          projectId: restored.projectId,
          title: restored.title,
          kind: restored.kind || "chat",
          messages: restored.messages,
          usage: restored.usage,
          attachments: [],
          pending: false,
        });
        syncModelForKind(restored.kind);
        // 重启回来的那条话题可能正带着一个目标：模式在日志里，不在存档里，得单独问一次
        void get().loadMode(restored.id);
      } catch (error) {
        console.error("读取历史话题失败", error);
        startFresh();
      }
    },

    updateConfig: async (patch) => {
      const previous = get().config;
      set({ config: { ...previous, ...patch } });
      // 只把改动的字段交给后端：整份回写等于让"谁手里快照最新"决定一切，
      // 另一页留着过期 config 再存一次就会把这边刚改的无关字段抹回旧值。
      // 成功时不回写持久化结果：受控输入若被异步返回值覆盖，会吞掉用户随后敲的那几个字符。
      // 但**失败必须回退**：不然界面会留着一份"说改了、盘上没有"的状态，
      // 而权限档 (`mode`) 就在这份状态里——显示成 full 而实际还是 ask，
      // 比一句报错难发现得多（所有设置页都是 `void updateConfig(...)`，没人接这个 rejection）
      try {
        await persistPatch(patch);
        return true;
      } catch (error) {
        set({ config: previous });
        get().pushToast({ tone: "error", title: "设置没保存", detail: String(error) });
        return false;
      }
    },

    refreshConnection: async () => {
      const [config, hasKey] = await Promise.all([fetchConfig(), probeCredential()]);
      set({ config, hasKey, configLoaded: true });
      await get().loadModels();
    },

    setUi: (patch) => {
      const ui = { ...get().config.ui, ...patch };
      // 顶层那三个字段是 `config.ui` 的镜像，两份要一起动，
      // 所以回退也得跟着回：只回 config 会留下"这格看着开了，
      // 下一次 setUi 从回退后的 config 又把它弹回来"的自相矛盾
      const mirror = (from: AppConfig["ui"]) => ({
        sidebarCollapsed: from.sidebarCollapsed,
        panelCollapsed: from.panelCollapsed,
        panelTab: from.panelTab,
      });
      set(mirror(ui));
      void get().updateConfig({ ui }).then((saved) => {
        if (!saved) set(mirror(get().config.ui));
      });
    },

    loadModels: async () => {
      if (get().loadingModels) return;
      set({ loadingModels: true, modelsError: null });
      try {
        set({ models: await fetchModels(), loadingModels: false });
      } catch (error) {
        set({
          models: [],
          loadingModels: false,
          modelsError: error instanceof Error ? error.message : String(error),
        });
      }
    },

    attachPaths: async (paths) => {
      const existing = new Set(get().attachments.map((item) => item.path));
      const fresh = paths.filter((path) => !existing.has(path));
      // 用户自选路径不在静态 asset scope 里：先放行再挂，重开会话的渲染才不至于挂图
      if (fresh.length > 0) await assetAllow(fresh).catch(() => undefined);

      const loaded: Attachment[] = [];
      for (const path of fresh) {
        try {
          // text 留在附件上：视频会话的剧本发送时要拼进生成提示词
          const meta = await readAttachment(path);
          loaded.push({ id: newId("att"), ...meta });
        } catch (error) {
          // 选择器旁边的 pickError 只有从菜单发起那一眼看得见；拖拽/粘贴路径
          // 走不到它——toast 三条路都盖住
          get().pushToast({
            tone: "error",
            title: "附件没挂上",
            detail: error instanceof Error ? error.message : String(error),
          });
        }
      }

      if (loaded.length > 0) {
        set((s) => ({ attachments: [...s.attachments, ...loaded] }));
      }
    },

    detachAttachment: (id) =>
      set((s) => ({ attachments: s.attachments.filter((item) => item.id !== id) })),

    /** 粘贴的截图：Rust 侧已落盘，这里挂一个图片 chip（缩略图只在内存里活着）。
     *  不走 attachPaths——那是按文本读的，图片读出来是一屏乱码 */
    attachImage: (image) =>
      set((s) => ({
        attachments: [
          ...s.attachments.filter((item) => item.path !== image.path),
          {
            id: newId("att"),
            name: image.name,
            path: image.path,
            chars: image.bytes,
            truncated: false,
            kind: "image",
            previewDataUrl: image.previewDataUrl,
          },
        ],
      })),

    decide: async (id, approved) => {
      set((s) => ({ deciding: [...s.deciding, id] }));
      try {
        await decideTool(id, approved);
      } finally {
        set((s) => ({ deciding: s.deciding.filter((value) => value !== id) }));
      }
    },

    answerQuestion: async (id, answer) => {
      const delivered = await answerQuestionIpc(id, answer);
      if (!delivered) {
        // 投晚了：那一发已经被停止或作废。卡片随 done 清理，这里只补一句人话
        get().pushToast({ tone: "info", title: "这条提问已经结束了，回答没有送达" });
      }
    },


    setVideoGenerationType: (type) => {
      set({ videoGenerationType: type });
    },

    setMusicSubMode: (mode) => set({ musicSubMode: mode }),

    setMediaDraft: (patch) => {
      set((s) => ({
        mediaDrafts: {
          ...s.mediaDrafts,
          [s.activeId]: { ...s.mediaDrafts[s.activeId], ...patch },
        },
      }));
      // 第一笔**敲出来的内容**出现的那一刻才落壳：只有提示词/描述/风格/歌词算数。
      // 页签（mode）与开关（instrumental）是界面状态——点一下"图像转视频"或切个
      // 纯音乐不该把会话钉进历史（真机踩过：切子模式连出一排空"新话题"）。
      // 壳一次会话顶多落一份（shellSaved 挡住异步缝隙里的连写）；重启回来的
      // 旧会话本来就在盘上与侧栏里，不用补
      const state = get();
      if (shellSaved.has(state.activeId)) return;
      const draft = state.mediaDrafts[state.activeId];
      const meaningful = [draft?.prompt, draft?.description, draft?.style, draft?.lyrics].some(
        (value) => typeof value === "string" && value.trim() !== "",
      );
      if (!meaningful) return;
      shellSaved.add(state.activeId);
      if (!state.conversations.some((item) => item.id === state.activeId)) {
        void persistConversation(state, { allowShell: true });
      }
    },

    createProject: async (name, path) => {
      try {
        set({ config: await requestAddProject(name, path) });
        return null;
      } catch (error) {
        return error instanceof Error ? error.message : String(error);
      }
    },

    chooseProject: async (id, opts) => {
      set({ config: await requestSelectProject(id) });
      // 选择器长在这支话题的输入框旁边：人读它当"**这条话题**绑的是哪"，
      // 所以选择同时改写当前话题的归属。只改应用级默认的话，先建话题再解绑
      // 会撞上这个：话题带着旧项目落盘，侧栏把它排在老项目下面，而选择器
      // 明明写着"选择工作目录"——同一件事两种说法（实测：日志真落在旧项目目录里）。
      // `rebindCurrent: false` 是侧栏项目切换器那一格：它只要默认值 + 新话题，
      // 老话题的归属不许被顺手拽走。
      // 未绑定的会话也同一语义（用户拍板：选择即移动比"开新话题"更顺手）——
      // 一版曾把未绑定+带历史的场景改成开新话题，用户要求恢复原样
      if (opts?.rebindCurrent !== false) {
        set({ projectId: id });
        // 侧栏立即归组：不等落盘（回合在跑时落盘会被收尾覆盖成旧归属）
        set((s) => ({
          conversations: s.conversations.map((c) =>
            c.id === s.activeId ? { ...c, projectId: id } : c,
          ),
        }));
        // 在飞的回合也要带上新归属：收尾那次全量落盘用的是 run.projectId，
        // 不补的话回合一结束就把刚选的工作区冲回"未绑定"（用户实测）。
        // 回合不在跑才立即落盘（半截正文不该进档）
        const liveRun = runs.get(get().activeId);
        if (liveRun && !liveRun.settled) patchRunOf(liveRun, { projectId: id });
        if (!get().pending && !runs.has(get().activeId)) await persistCurrent();
      }
    },

    dropProject: async (id) => {
      set({ config: await requestRemoveProject(id) });
    },

    startConversation: (kind) => {
      // 正在生成时也得回得来：这时候它是"回到这轮对话"，不是新建。
      // 早先直接 return，人切到工具/插件分区就再也点不回对话页了
      get().setSection("chats");
      if (get().pending) return;
      const previousKind = get().kind;
      startFresh(kind ?? "chat");
      syncModelForKind(kind ?? "chat", previousKind);
      // 开局**不**落盘：光点开媒体会话、切走、再点开，不该在历史里留下一排
      // 空"新话题"（真机踩过：一晚上 23 个 0 条壳）。值得留的内容出现的那一刻
      // 才落壳——见 setMediaDraft；生成产物照旧在 sendMedia 收尾落盘
    },

    sendMedia: async (prompt, generation, material) => {
      const trimmed = prompt.trim();
      // 转写不需要提示词：素材是附件里的音频。归属话题已有没收尾的生成现场
      // （切走又切回的那种，mediaBusy 在 startFresh/接管路上被重置过）也不收：
      // 第二发会把第一发的现场顶掉，产物跟着蒸发
      const occupant = runs.get(get().activeId);
      if (
        (!trimmed && generation !== "transcribe") ||
        get().mediaBusy ||
        (occupant !== undefined && !occupant.settled && (occupant.kind ?? "chat") !== "chat")
      )
        return;
      get().setSection("chats");
      const kind = get().kind;
      // per-run 身份从这一刻定：生成挂到归属话题的现场上，结果跟着话题走，
      // 不再依赖"完成那一刻你正好站在哪一条"——那是切走后产物静默丢失的根
      const ownerId = get().activeId;
      mediaStopFlags.delete(ownerId);
      // 这一发生成什么：视频画布的四类页签点名（文本/图片/视频/音频）；
      // 生图会话恒为图。类型与模型行各归各的（kindModels.text/image/video/audio）
      const type: MediaType =
        generation ?? (kind === "image" ? "image" : kind === "music" ? "music" : "video");
      const stamp = Date.now();
      const replyId = newId("msg");
      // 参考图/剧本随问题走：发送即从输入框清场（与对话发送同一拍）。
      // 生图会话的图片附件 = 图生图参考图；视频会话的文本附件 = 剧本
      const consumed = get().attachments;
      // 参照素材按生成模式路由：
      // 图片生成 → 图片附件是参考图；视频生成按模式——图像转视频（frames）吃
      // 图片附件（第一张首帧、第二张可选尾帧），视频转视频（edit）吃视频附件
      const videoMode = get().config.videoGen?.mode ?? "omni";
      const ownImages = consumed.filter((item) => item.kind === "image").map((item) => item.path);
      const referenceImages =
        type === "image"
          ? [...new Set(ownImages)]
          : type === "video" && videoMode !== "edit"
            ? [...new Set(ownImages)]
            : type === "transcribe"
              ? consumed.filter((item) => item.kind === "audio").map((item) => item.path)
              : [];
      const videoReference =
        type === "video" && videoMode === "edit"
          ? consumed.find((item) => item.kind === "video")?.path
          : undefined;
      // 模型行可能住在别的服务商：按模型名找它所属的档案（激活档案优先），
      // 发送时把连接域路由过去
      const resolveKey = type === "text" ? "chat" : type;
      const resolveModel =
        type === "transcribe"
          ? get().config.kindModels?.transcribe || "whisper-1"
          : get().config.kindModels?.[resolveKey] || get().config.model;
      const activeProfile = get().config.profiles.find(
        (profile) => profile.id === get().config.activeProfileId,
      );
      const profileId = activeProfile?.models.some((spec) => spec.model === resolveModel)
        ? activeProfile.id
        : get().config.profiles.find((profile) =>
            profile.models.some((spec) => spec.model === resolveModel),
          )?.id;
      // 生成管线没有对话历史，剧本进"上下文"的唯一通道就是提示词。
      // 只拼给视频生成：图片页签吃的是参考图
      const script =
        kind === "video" && type === "video"
          ? consumed
              .filter((item) => (item.kind ?? "text") === "text")
              .map((item) => item.text?.trim())
              .filter((text): text is string => Boolean(text))
              .join("\n\n")
          : "";
      // 用户气泡 + 生成占位（streaming + media 标记 → 气泡渲染生成加载卡）
      set((s) => ({
        messages: [
          ...s.messages,
          {
            id: newId("msg"),
            role: "user" as const,
            content: trimmed || (type === "transcribe" ? "转写这段音频" : ""),
            createdAt: stamp,
            // 缩略图留在气泡上（落盘时由 persistConversation 剥掉，重开走 asset 协议）
            attachments:
              consumed.length > 0
                ? consumed.map((item) => ({
                    name: item.name,
                    kind: (item.kind ?? "text") as "text" | "image" | "video",
                    path: item.path,
                    bytes: item.chars,
                    previewDataUrl: item.previewDataUrl,
                  }))
                : undefined,
          },
          {
            id: replyId,
            role: "assistant" as const,
            content: "",
            createdAt: stamp + 1,
            streaming: true,
            media: type,
            model:
              type === "transcribe"
                ? get().config.kindModels?.transcribe || "whisper-1"
                : get().config.kindModels?.[
                    type === "text" ? "chat" : type
                  ] || get().config.model,
          },
        ],
      }));
      set({ mediaBusy: true, attachments: [] });
      // 生成现场挂进 runs（与对话轮同一套机制）：切走话题它继续收尾，切回来
      // openConversation 直接接管这份现场；完成落盘后交还给存档
      const mediaRun: LiveRun = {
        conversationId: ownerId,
        projectId: get().projectId,
        title: get().title,
        kind: get().kind,
        messages: get().messages,
        offPath: get().offPath,
        usage: undefined,
        pending: false,
        settled: false,
        save: Promise.resolve(),
        followUpCount: 0,
        followUpBubbleIds: [],
        mode: null,
        modeArmed: false,
      };
      runs.set(ownerId, mediaRun);
      setRunning(ownerId, true);
      /** 产物只回填归属话题现场里的那一格：patchRunOf 自带身份校验，
       *  现场已经交还（或被新一发顶替）时整个不生效 */
      const patchMedia = (patch: (message: Message) => Message) =>
        patchRunOf(mediaRun, (r) => ({
          messages: r.messages.map((message) =>
            message.id === replyId ? patch(message) : message,
          ),
        }));
      try {
        // 这一发的参照素材与模型行都按页签类型走：sendMedia 曾把会话档 kind
        // 原样传给 media_generate——视频会话的图片页签实际在生成视频（静默失配）
        const media = await mediaGenerate(
          type,
          script ? `${trimmed}\n\n${script}` : trimmed,
          // 视频生成的参数形状是模式/比例/分辨率/时长，其余（生图）是尺寸/质量/数量
          type === "video"
            ? get().config.videoGen
            : type === "music"
              ? {
                  lyrics: material?.lyrics,
                  instrumental: material?.instrumental === true,
                }
              : get().config.imageGen,
          referenceImages,
          resolveModel,
          videoReference,
          profileId,
        );
        if (mediaStopFlags.has(ownerId)) {
          // 用户停止等待：产物已付钱但不回填界面（设计如此）。但占位不能停在
          // "生成中"——streaming 留真的话，落盘重开就是一张永远转的加载卡，
          // 工作区的 generating 判据（streaming && media）也会永远成立
          patchMedia((message) => ({ ...message, streaming: false, content: "已停止等待。" }));
          return;
        }
        const secs = Math.max(1, Math.round((Date.now() - stamp) / 1000));
        const duration = secs >= 60 ? `${Math.floor(secs / 60)} 分 ${secs % 60} 秒` : `${secs} 秒`;
        // 产物按类型回填：图片可能多张（n 参数），文本是正文，音频/视频是单文件
        const attachments: Array<{
          name: string;
          kind: "image" | "video" | "audio";
          path: string;
          bytes: number;
        }> =
          type === "image"
            ? (media.images ?? []).map((image) => ({
                name: image.name,
                kind: "image" as const,
                path: image.path,
                bytes: image.bytes,
              }))
            : type === "video"
              ? [
                  {
                    name: media.name ?? "generated.mp4",
                    kind: "video" as const,
                    path: media.path ?? "",
                    bytes: media.bytes ?? 0,
                  },
                ]
              : type === "audio" || type === "music"
                ? [
                    {
                      name: media.name ?? "generated.mp3",
                      kind: "audio" as const,
                      path: media.path ?? "",
                      bytes: media.bytes ?? 0,
                    },
                  ]
                : [];
        const countLabel = type === "image" ? `共 ${attachments.length} 张，` : "";
        patchMedia((message) => ({
          ...message,
          streaming: false,
          content:
            type === "text" || type === "transcribe"
              ? media.text ?? ""
              : `生成好了（${countLabel}耗时 ${duration}）。要改风格或构图，继续描述就行。`,
          attachments: attachments.length > 0 ? attachments : undefined,
        }));
      } catch (error) {
        const detail = error instanceof Error ? error.message : String(error);
        patchMedia((item) => ({ ...item, streaming: false, content: `生成失败：${detail}`, error: detail }));
        get().pushToast({ tone: "error", title: "生成失败", detail });
      } finally {
        mediaStopFlags.delete(ownerId);
        set({ mediaBusy: false });
        // 收口与对话轮的 endRun 同一形状：产物落进**归属话题**的存档——不是
        // persistCurrent（那写的是眼前这一屏，切走后完成的那发会把别的话题写坏）。
        // 落完盘把现场交还；身份对不上（新一发已顶替）就什么都不动
        if (runs.get(ownerId) === mediaRun) {
          mediaRun.settled = true;
          setRunning(ownerId, false);
          await persistConversation(runFields(mediaRun));
          if (runs.get(ownerId) === mediaRun) runs.delete(ownerId);
        }
      }
    },

    stopMedia: () => {
      mediaStopFlags.add(get().activeId);
      set({ mediaBusy: false });
    },

    openConversation: async (id) => {
      // 先换分区再谈"要不要重新加载"：在工具页点自己正在看的那条话题，
      // 早先是先 return 的，于是中间栏留在工具页，看着就像点不动
      get().setSection("chats");
      const seq = ++openConversationSeq;
      const state = get();
      if (id === state.activeId) return;
      const prevKind = state.kind;

      // 走的时候这一轮还在跑：不落盘。存档里那份是半截正文，写进去就等于把
      // "切走看一眼"变成"把答案截断"。完整的那一份由这一轮收尾时自己落
      if (!runs.has(state.activeId)) await persistCurrent();
      // Worktree 勾选态是话题的属性：切到谁就读谁的（重启后也一样，真相源在后端注册表）
      void get().refreshWorktree(id);

      const incoming = runs.get(id);
      if (incoming) {
        // 排队期间用户又点了别条：只有最新一次 open 说了算
        if (seq !== openConversationSeq) return;
        // 直接接管它的现场：正文接到当前进度，流式标志与排队计数照原样带回来。
        // 不读存档——那份是这一轮开始之前的样子
        set({
          activeId: incoming.conversationId,
          projectId: incoming.projectId,
          title: incoming.title,
          kind: incoming.kind || "chat",
          messages: incoming.messages,
          offPath: incoming.offPath,
          usage: incoming.usage,
          attachments: [],
          pending: incoming.pending,
          followUpCount: incoming.followUpCount,
          // 媒体现场带着"忙"回来：不然输入框不设防，第二发生成会顶掉第一发的现场
          mediaBusy: (incoming.kind ?? "chat") !== "chat" && !incoming.settled,
          // 现场自己带读数：那一轮的 `mode` 事件比再读一次日志新
          mode: incoming.mode ?? null,
        });
        syncModelForKind(incoming.kind, prevKind);
        return;
      }
      try {
        const restored = await historyLoad(id);
        // 慢加载期间用户又点了别条：过期回复在这里丢弃
        if (seq !== openConversationSeq) return;
        // 链路动画是活跃会话的现场：换话题即清
        set({ journey: [] });
        // 归档里现在是整棵树（切走的那些分支也在，一行不丢）。看得见的那条由**后端的
        // 分支末端**决定：用户上次站在哪一支，重开就还在哪一支
        const nodes = restored.messages;
        let thread = nodes;
        let offPath: Message[] = [];
        try {
          const tree = await fetchConversationTree(id);
          if (seq !== openConversationSeq) return;
          const tip = tree.tip
            ? nodes.find((message) => message.entryIds?.includes(tree.tip!))?.id ?? null
            : null;
          // 映不上（旧存档没有条目 id）就退回"最后插入的那条是末端"——那等于
          // 分支树之前用户看到的那一条，而不是把兄弟全摊开
          ({ thread, offPath } = partition(nodes, tip ?? nodes[nodes.length - 1]?.id ?? null));
        } catch {
          // 读不到树：整份按插入序显示。旧存档没有 parentId，partition 本来也走这条
        }
        set({
          activeId: restored.id,
          projectId: restored.projectId,
          title: restored.title,
          kind: restored.kind || "chat",
          // 视频画布的节点登记随档案回来；选中落在第一个（节点内对话从那看起）
          videoGenerationType: "video",
          messages: thread,
          offPath,
          usage: restored.usage,
          attachments: [],
          pending: false,
          followUpCount: 0,
          // 读数不在这份存档里：模式是话题日志上的一行，得问后端
          mode: null,
        });
        // 恢复回来的附件走 asset 协议渲染（previewDataUrl 落盘时已剥掉）：
        // 用户自选路径不在静态 scope 里，整批按需放行
        const allowPaths = [...thread, ...offPath].flatMap((message) =>
          (message.attachments ?? []).map((item) => item.path).filter(Boolean),
        );
        if (allowPaths.length > 0) void assetAllow(allowPaths).catch(() => undefined);
        syncModelForKind(restored.kind);
        void get().loadMode(id);
      } catch (error) {
        set({ modelsError: error instanceof Error ? error.message : String(error) });
      }
    },

    loadMode: async (conversationId) => {
      const ownerId = conversationId ?? get().activeId;
      try {
        const state = await fetchModeState(ownerId);
        // 现场带一份、界面带一份：切走的那条话题也有自己的读数，回来不该是空的
        const run = runs.get(ownerId);
        if (run) patchRunOf(run, { mode: state });
        // 读回来时人可能已经切走了：那一份属于别条话题，盖到当前界面上就是串了台
        if (get().activeId === ownerId) set({ mode: state, modeError: null });
        // 目标面板最后记账：它重建 goalRuns 时读的是界面与现场——两处都落好了再刷，
        // 否则面板拿到的是上一条的旧读数
        if (state.objective) {
          const wasTracked = goalModes.get(ownerId)?.tracked ?? false;
          // 挂着且还没收尾 = 这一程活动过；收尾的只在面板里留到有人再碰它
          noteGoalMode(ownerId, state, wasTracked || state.status === "active");
        } else {
          noteGoalMode(ownerId, state, false);
        }
      } catch (error) {
        // 读数读不到就清成"不知道"，而不是留着上一条话题的那一份——
        // 面板上挂着别条话题的模式，比空着更容易误导人
        if (get().activeId === ownerId) {
          set({
            mode: null,
            modeError: error instanceof Error ? error.message : String(error),
          });
        }
      }
    },

    setMode: async (input) => {
      const ownerId = get().activeId;
      set({ modeBusy: true, modeError: null });
      try {
        const outcome = await setModeState({ conversationId: ownerId, ...input });
        return settleModeOutcome(outcome, ownerId);
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        set({ modeError: message, modeBusy: false });
        return message;
      }
    },

    goalSet: async (input) => {
      const ownerId = get().activeId;
      set({ modeBusy: true, modeError: null });
      try {
        const outcome = await sessionGoalSet({ conversationId: ownerId, ...input });
        return settleModeOutcome(outcome, ownerId);
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        set({ modeError: message, modeBusy: false });
        return message;
      }
    },

    goalEdit: async (input) => {
      const ownerId = get().activeId;
      set({ modeBusy: true, modeError: null });
      try {
        const outcome = await sessionGoalEdit({ conversationId: ownerId, ...input });
        return settleModeOutcome(outcome, ownerId);
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        set({ modeError: message, modeBusy: false });
        return message;
      }
    },

    goalPause: async (conversationId, paused) => {
      try {
        const outcome = await goalPauseState(conversationId, paused);
        if (paused && outcome.deferred) {
          // 回合还在跑：那一行等收尾才落，界面先自己标"暂停中"
          goalOptimisticPaused.add(conversationId);
        } else {
          goalOptimisticPaused.delete(conversationId);
        }
        const run = runs.get(conversationId);
        if (run) patchRunOf(run, { mode: outcome.view });
        if (get().activeId === conversationId) set({ mode: outcome.view, modeError: null });
        // 面板最后记账（理由同 loadMode）：乐观旗与界面读数都落好了再刷
        noteGoalMode(conversationId, outcome.view, true);
        return null;
      } catch (error) {
        return error instanceof Error ? error.message : String(error);
      }
    },

    goalResume: async (conversationId) => {
      try {
        const view = await goalResumeRun(conversationId);
        goalOptimisticPaused.delete(conversationId);
        // 新开的这一轮从零记尾巴：上一段的输出已经收在话题里，混在一起反而读不清
        goalTails.delete(conversationId);
        goalKicked.add(conversationId);
        broadcastRounds.set(conversationId, view.turnsUsed);
        const run = runs.get(conversationId);
        if (run) patchRunOf(run, { mode: view });
        if (get().activeId === conversationId) set({ mode: view, modeError: null });
        noteGoalMode(conversationId, view, true);
        return null;
      } catch (error) {
        return error instanceof Error ? error.message : String(error);
      }
    },

    goalDiscard: async (conversationId) => {
      try {
        const outcome = await goalDiscardState(conversationId);
        const view = outcome.view;
        goalOptimisticPaused.delete(conversationId);
        goalTails.delete(conversationId);
        goalKicked.delete(conversationId);
        const run = runs.get(conversationId);
        if (run) patchRunOf(run, { mode: view });
        if (get().activeId === conversationId) set({ mode: view, modeError: null });
        // 记账放最后：没有目标的读数会把面板里那条摘掉
        noteGoalMode(conversationId, view, true);
        if (outcome.deferred) {
          get().pushToast({
            tone: "info",
            title: "结束目标排在这一轮后面",
            detail: "这一轮跑完就不再自己接下一轮，那一行由它收尾时落下。",
          });
        }
        return null;
      } catch (error) {
        return error instanceof Error ? error.message : String(error);
      }
    },

    stopRun: async (conversationId) => {
      // 与输入框上的停止是同一扇闸，只是按话题点名。没在跑的回合后端会拒：
      // 那正是"没什么可停"，静默收下
      await chatAbort(conversationId).catch(() => undefined);
    },

    deleteConversation: async (id) => {
      // 删掉正在跑的话题：先把服务商那一发中止，再把现场撤了。不中止的话它会继续在
      // 已删的话题上出字，收尾时把整份存档写回来——等于删不掉
      const run = runs.get(id);
      if (run) {
        run.settled = true;
        runs.delete(id);
        setRunning(id, false);
        void chatAbort(id).catch(() => undefined);
      }
      // 删除失败必须出声：侧栏还挂着它、盘上还有它，静默吞掉的话用户以为删掉了，
      // 重启它又回来了。清理一律排在删除成功之后，本地与盘上才不会各说各话
      try {
        await historyRemove(id);
      } catch (error) {
        get().pushToast({
          tone: "error",
          title: "话题没有删掉",
          detail: error instanceof Error ? error.message : String(error),
        });
        return;
      }
      persisted.delete(id);
      lastSavedInfo.delete(id);
      // 工作区草稿跟着话题走：话题没了，名下的草稿也没有存在的依据
      set((s) => {
        if (!(id in s.mediaDrafts)) return {};
        const mediaDrafts = { ...s.mediaDrafts };
        delete mediaDrafts[id];
        return { mediaDrafts };
      });
      // 面板的账跟着话题走：话题没了，面板里那一条也没有存在的依据
      goalModes.delete(id);
      goalTails.delete(id);
      goalOptimisticPaused.delete(id);
      goalKicked.delete(id);
      refreshGoalPanels();
      set((s) => ({ conversations: s.conversations.filter((item) => item.id !== id) }));
      if (get().activeId === id) startFresh();
    },

    renameConversation: async (id, title) => {
      const trimmed = title.trim();
      if (!trimmed) return;
      // 正看着的这条：标题在状态里就有一份，改完照常落盘（指纹会变，去重不会吞掉它）
      if (id === get().activeId) {
        set({ title: trimmed });
        persistCurrent();
        return;
      }
      // 切走的话题：整份记录读上来、只改标题、存回去
      try {
        const record = await historyLoad(id);
        record.title = trimmed;
        const meta = await historySave(record);
        set((s) => ({
          conversations: [meta, ...s.conversations.filter((item) => item.id !== meta.id)],
        }));
      } catch (error) {
        get().pushToast({
          tone: "error",
          title: "标题没有改成",
          detail: error instanceof Error ? error.message : String(error),
        });
      }
    },

    togglePin: async (id) => {
      // 当前正在看的这条：置顶就是本地状态的一部分，改完照常落盘
      if (id === get().activeId) {
        const current = get().conversations.find((item) => item.id === id)?.pinned ?? false;
        set((s) => ({
          conversations: s.conversations.map((item) =>
            item.id === id ? { ...item, pinned: !current } : item,
          ),
        }));
        persistCurrent();
        return;
      }
      // 切走的话题：整份记录读上来、只翻置顶位、存回去
      try {
        const record = await historyLoad(id);
        record.pinned = !record.pinned;
        const meta = await historySave(record);
        set((s) => ({
          conversations: [meta, ...s.conversations.filter((item) => item.id !== meta.id)],
        }));
      } catch (error) {
        get().pushToast({
          tone: "error",
          title: "置顶没有改成",
          detail: error instanceof Error ? error.message : String(error),
        });
      }
    },

    refreshHistory: async () => {
      // 裸 await 的拒绝没人接（task-ran 那路是 `void refreshHistory()`）：
      // 一次 IPC 抖动就把整条事件链打成 unhandled rejection。列表保持原样，
      // 下一个事件来了再试——侧栏空一屏比晚刷新一拍伤得多
      try {
        set({ conversations: await historyList() });
      } catch (error) {
        console.error("读取话题列表失败", error);
      }
    },

    refreshEdits: async () => {
      try {
        set({ edits: await fetchSessionEdits(get().activeId), editsError: null });
      } catch (error) {
        // 清空而不是留着旧的一份：面板上挂着已经不属于这个话题的文件，
        // 比面板空着更容易误导人
        set({
          edits: [],
          editsError: error instanceof Error ? error.message : String(error),
        });
      }
    },

    openPreview: (absPath) => {
      // 预览目标可以是盘上任意文件：静态 scope 盖不到，渲染前先放行
      if (absPath) void assetAllow([absPath]).catch(() => undefined);
      set({ previewTarget: absPath });
      if (absPath) get().setPanelTab("preview");
    },

    pushToast: (toast) => {
      // 只留最后 3 条：连续失败时堆一屏一样的红条，读不出比读出一条更多的信息
      set((state) => ({
        toasts: [...state.toasts, { ...toast, id: newId("toast") }].slice(-3),
      }));
    },

    dismissToast: (id) => {
      set((state) => ({ toasts: state.toasts.filter((item) => item.id !== id) }));
    },

    refreshStorage: async () => {
      try {
        set({ storage: await fetchStorageInfo(), storageError: null });
      } catch (error) {
        set({ storageError: error instanceof Error ? error.message : String(error) });
      }
    },

    switchStorage: async (backend) => {
      if (get().pending) {
        set({ storageError: "正在生成回复，等这轮结束后再切换。" });
        return;
      }
      const previous = get().config.conversationStore;
      if (previous === backend) return;

      set({ storageBusy: true, storageError: null, storageNote: null });
      // 当前话题先落到旧存储，切换时才会被一起带走
      await persistCurrent();
      try {
        const result = await requestStorageSwitch(backend);
        set({
          config: result.config,
          storage: result.info,
          storageBusy: false,
          storageNote:
            result.moved > 0
              ? `已把 ${result.moved} 条话题拷贝到 ${storageOption(backend).short} 一侧，原来那份保留未动。`
              : null,
        });
        await get().refreshHistory();
      } catch (error) {
        set({
          storageBusy: false,
          storageError: error instanceof Error ? error.message : String(error),
        });
        // 以后端为准：失败时开关到底拨过去没有，不能由前端猜
        set({ config: await fetchConfig() });
      }
    },

    refreshSkills: async () => {
      try {
        const listing = await skillsList();
        set({ skills: listing.skills, skillsDir: listing.dir, skillsError: null });
      } catch (error) {
        set({ skillsError: error instanceof Error ? error.message : String(error) });
      }
    },

    toggleSkill: async (id, enabled) => {
      const disabled = get().config.disabledSkills;
      const nextDisabled = enabled
        ? disabled.filter((item) => item !== id)
        : [...disabled, id];
      const flip = () =>
        set((state) => ({
          skills: state.skills.map((item) => (item.id === id ? { ...item, enabled } : item)),
        }));
      flip();

      try {
        await get().updateConfig({ disabledSkills: nextDisabled });
        // 内置技能的开关有两份投影：技能页的 skills、插件页出厂扩展卡里的技能行。
        // 只靠本地 flip，另一份纹丝不动——"点了开关没反应"就是这么来的
        //（对照 toggleBuiltin：它本来就刷新这两个清单）。命令只读本地目录，不碰网络
        await get().refreshSkills();
        await get().refreshPlugins();
      } catch (error) {
        // 没落盘就弹回去：界面显示"已启用"而提示词里没这条技能，是最难查的那种不一致
        set((state) => ({
          skills: state.skills.map((item) =>
            item.id === id ? { ...item, enabled: !enabled } : item,
          ),
          skillsError: `开关没保存：${error instanceof Error ? error.message : String(error)}`,
        }));
      }
    },

    // 钩子开关住在 config 里，和插件、技能同一套写法：改本地状态 → 落盘 → 失败就弹回
    toggleHook: async (id, enabled) => {
      const flip = (value: boolean) =>
        set((state) => ({
          plugins: state.plugins.map((plugin) => ({
            ...plugin,
            hooks: plugin.hooks.map((hook) => (hook.id === id ? { ...hook, enabled: value } : hook)),
          })),
        }));

      flip(enabled);
      const disabled = get().config.disabledHooks;
      try {
        await get().updateConfig({
          disabledHooks: enabled
            ? disabled.filter((item) => item !== id)
            : [...disabled, id],
        });
      } catch (error) {
        flip(!enabled);
        throw error;
      }
      // runs 是四个条件的合取，本地推不算准，让 Rust 重算一遍
      await get().refreshPlugins();
    },

    trustHook: async (id, hash, trusted) => {
      const kept = get().config.trustedHooks.filter((item) => item.id !== id);
      const previous = get().plugins;
      try {
        await get().updateConfig({
          trustedHooks: trusted ? [...kept, { id, hash }] : kept,
        });
      } catch (error) {
        set({ plugins: previous });
        throw error;
      }
      await get().refreshPlugins();
    },

    refreshMcp: async () => {
      try {
        set({ mcpServers: await mcpList(), mcpError: null });
      } catch (error) {
        set({ mcpError: error instanceof Error ? error.message : String(error) });
      }
    },

    // 服务器定义同样住在 config，运行状态由 Rust 的连接表给出
    refreshPlugins: async () => {
      try {
        const listing = await pluginsList();
        set({
          plugins: listing.plugins,
          builtins: listing.builtins,
          pluginsDir: listing.dir,
          pluginsError: null,
        });
      } catch (error) {
        set({ pluginsError: error instanceof Error ? error.message : String(error) });
      }
    },

    // 插件的启用状态是 aglab 自己的开关，所以同样住在 config
    togglePlugin: async (id, enabled) => {
      const disabled = get().config.disabledPlugins;
      await get().updateConfig({
        disabledPlugins: enabled
          ? disabled.filter((item) => item !== id)
          : [...disabled, id],
      });
      await get().refreshPlugins();
      await get().refreshSkills();
      await get().refreshMcp();
    },

    // 出厂扩展同一个道理：定义在代码里，配置只记「被关掉的 id」。
    // 刷新技能清单——关扩展 = 它的技能整批从清单与取用里消失
    toggleBuiltin: async (id, enabled) => {
      const disabled = get().config.disabledBuiltins;
      await get().updateConfig({
        disabledBuiltins: enabled
          ? disabled.filter((item) => item !== id)
          : [...disabled, id],
      });
      await get().refreshPlugins();
      await get().refreshSkills();
    },

    saveMcpServer: async (server) => {
      const definitions = get().config.mcpServers;
      const exists = definitions.some((item) => item.id === server.id);
      await get().updateConfig({
        mcpServers: exists
          ? definitions.map((item) => (item.id === server.id ? server : item))
          : [...definitions, server],
      });
      await get().refreshMcp();
    },

    removeMcpServer: async (id) => {
      await get().updateConfig({
        mcpServers: get().config.mcpServers.filter((item) => item.id !== id),
      });
      await get().refreshMcp();
    },

    toggleMcpServer: async (id, enabled) => {
      const next = get().config.mcpServers.map((item) =>
        item.id === id ? { ...item, enabled } : item,
      );
      await get().updateConfig({ mcpServers: next });
      if (!enabled) {
        // 关掉就把子进程收掉，别留一个没人管的进程挂在后台
        await mcpStop(id).catch((error) => console.error("停止扩展失败", error));
      }
      await get().refreshMcp();
    },

    toggleMcpTool: async (exposed, enabled) => {
      const disabled = get().config.disabledMcpTools;
      const nextDisabled = enabled
        ? disabled.filter((item) => item !== exposed)
        : [...disabled, exposed];
      set((state) => ({
        mcpServers: state.mcpServers.map((server) => ({
          ...server,
          tools: server.tools.map((tool) =>
            tool.exposed === exposed ? { ...tool, enabled } : tool,
          ),
        })),
      }));

      try {
        await get().updateConfig({ disabledMcpTools: nextDisabled });
      } catch (error) {
        set({ mcpError: error instanceof Error ? error.message : String(error) });
        await get().refreshMcp();
      }
    },

    connectMcp: async (id) => {
      await mcpConnect(id);
    },

    stopMcp: async (id) => {
      await mcpStop(id);
      await get().refreshMcp();
    },

    refreshMcpTools: async (id) => {
      // `confirm: true` 只代表"是界面上那一次点击走过来的"。真正的闸在后端：
      // 任何别的路径调这条命令都得自己带确认，否则一律拒
      await mcpRefresh(id, true);
    },

    refreshTasks: async () => {
      try {
        set({ tasks: await tasksList(), tasksError: null });
      } catch (error) {
        set({ tasksError: error instanceof Error ? error.message : String(error) });
      }
    },

    // 任务定义住在 config.tasks，所以增删改都走 updateConfig；运行状态由 Rust 单独维护
    saveTask: async (task) => {
      const definitions = get().config.tasks;
      const exists = definitions.some((item) => item.id === task.id);
      const next = exists
        ? definitions.map((item) => (item.id === task.id ? task : item))
        : [...definitions, task];
      await get().updateConfig({ tasks: next });
      await get().refreshTasks();
    },

    removeTask: async (id) => {
      await get().updateConfig({ tasks: get().config.tasks.filter((item) => item.id !== id) });
      await get().refreshTasks();
    },

    runTaskNow: async (id) => {
      await taskRunNow(id);
    },

    // ---- 用量台账。读操作失败只进 usageError，不向上抛 ----

    refreshUsage: async () => {
      const seq = ++usageRequestSeq;
      set({ usageLoading: true, usageError: null });
      try {
        const report = await fetchUsageReport(get().usageDays);
        if (seq !== usageRequestSeq) return;
        set({ usageReport: report, usageLoading: false });
      } catch (error) {
        if (seq !== usageRequestSeq) return;
        set({
          usageLoading: false,
          usageError: error instanceof Error ? error.message : String(error),
        });
      }
    },

    setUsageDays: async (days) => {
      if (get().usageDays === days) return;
      // 先把窗口拨过去再拉：报表和界面标题读的是同一个值
      set({ usageDays: days });
      await get().refreshUsage();
      // 明细跟同一个窗口走：窗口变了，翻页位置也没有意义了
      set({ usageRowsOffset: 0 });
      await get().loadUsageRows();
    },

    loadPrices: async () => {
      try {
        set({ prices: await pricingList(), pricesError: null });
      } catch (error) {
        set({ pricesError: error instanceof Error ? error.message : String(error) });
      }
    },

    upsertPrice: async (price) => {
      set({ pricesBusy: true, pricesError: null });
      try {
        set({ prices: await pricingUpsert(price) });
        return null;
      } catch (error) {
        return error instanceof Error ? error.message : String(error);
      } finally {
        set({ pricesBusy: false });
      }
    },

    removePrice: async (modelId) => {
      set({ pricesBusy: true, pricesError: null });
      try {
        set({ prices: await pricingRemove(modelId) });
      } catch (error) {
        set({ pricesError: error instanceof Error ? error.message : String(error) });
      } finally {
        set({ pricesBusy: false });
      }
    },

    // 价表更新后同窗口的未计价情况会变，顺手把报表也重拉一次
    importPricing: async () => {
      set({ pricesBusy: true, pricesNote: null });
      try {
        const count = await ccswitchImportPricing();
        set({ pricesNote: `已从 cc-switch 导入/更新 ${count} 条单价。` });
        await get().loadPrices();
        await get().refreshUsage();
      } catch (error) {
        set({
          pricesNote: `导入失败：${error instanceof Error ? error.message : String(error)}`,
        });
      } finally {
        set({ pricesBusy: false });
      }
    },

    // ---- cc-switch 迁移。读候选失败进 error；导入以后端返回为准，不乐观更新 ----

    refreshCcswitchCandidates: async () => {
      set({ ccswitchLoading: true, ccswitchError: null });
      try {
        set({ ccswitchCandidates: await ccswitchCandidates(), ccswitchLoading: false });
      } catch (error) {
        set({
          ccswitchLoading: false,
          ccswitchError: error instanceof Error ? error.message : String(error),
        });
      }
    },

    importProvider: async (sourceId) => {
      set({ ccswitchImportingId: sourceId, ccswitchNote: null });
      try {
        const config = await ccswitchImportProvider(sourceId);
        set({ config, ccswitchImportingId: null, ccswitchNote: "已导入，连接设置已更新。" });
        await get().loadModels();
        set({ hasKey: await probeCredential() });
        return true;
      } catch (error) {
        set({
          ccswitchImportingId: null,
          ccswitchNote: `导入失败：${error instanceof Error ? error.message : String(error)}`,
        });
        return false;
      }
    },

    refreshMcpCandidates: async () => {
      set({ mcpCandidatesLoading: true, mcpCandidatesError: null, mcpImportNote: null });
      try {
        set({ mcpCandidates: await ccswitchMcpCandidates(), mcpCandidatesLoading: false });
      } catch (error) {
        set({
          mcpCandidatesLoading: false,
          mcpCandidatesError: error instanceof Error ? error.message : String(error),
        });
      }
    },

    importMcpServers: async (ids) => {
      try {
        const result = await ccswitchImportMcp(ids);
        set({
          config: result.config,
          mcpImportNote: `已导入 ${result.added} 个，跳过 ${result.skipped} 个（已存在或类型不支持）。`,
        });
        await get().refreshMcp();
      } catch (error) {
        set({
          mcpImportNote: `导入失败：${error instanceof Error ? error.message : String(error)}`,
        });
      }
    },

    refreshSkillCandidates: async () => {
      set({ skillCandidatesLoading: true, skillCandidatesError: null, skillImportNote: null });
      try {
        set({ skillCandidates: await ccswitchSkillCandidates(), skillCandidatesLoading: false });
      } catch (error) {
        set({
          skillCandidatesLoading: false,
          skillCandidatesError: error instanceof Error ? error.message : String(error),
        });
      }
    },

    importSkills: async (ids) => {
      try {
        const result = await ccswitchImportSkills(ids);
        const skipped = result.skippedNames.length
          ? `；跳过：${result.skippedNames.join("、")}`
          : "";
        set({
          skillImportNote: `已导入 ${result.added} 个，跳过 ${result.skipped} 个${skipped}。`,
        });
        await get().refreshSkills();
      } catch (error) {
        set({
          skillImportNote: `导入失败：${error instanceof Error ? error.message : String(error)}`,
        });
      }
    },

    // ---- 请求明细（P1）。读操作失败只打日志，表格留在上一页数据上 ----

    loadUsageRows: async () => {
      const seq = ++usageRowsSeq;
      set({ usageRowsLoading: true, usageRowsError: null });
      try {
        const page = await usageRecent(get().usageDays, get().usageRowsOffset, USAGE_ROWS_PAGE);
        if (seq !== usageRowsSeq) return;
        set({ usageRows: page.rows, usageRowsTotal: page.total, usageRowsLoading: false });
      } catch (error) {
        if (seq !== usageRowsSeq) return;
        set({
          usageRowsLoading: false,
          usageRowsError: error instanceof Error ? error.message : String(error),
        });
      }
    },

    setUsageRowsOffset: async (offset) => {
      set({ usageRowsOffset: Math.max(0, offset) });
      await get().loadUsageRows();
    },

    // CSV 是给人拿去 Excel 看的：列名中文，金额写足 6 位小数保精度
    exportUsageCsv: async () => {
      const report = get().usageReport;
      if (!report) return "还没有可导出的报表，先等台账读取完成。";

      const path = await save({
        defaultPath: `aglab-usage-${new Date().toISOString().slice(0, 10)}.csv`,
        filters: [{ name: "CSV（逗号分隔）", extensions: ["csv"] }],
      });
      // 用户在保存对话框点了取消，不算错误
      if (!path) return null;

      // 值里出现逗号、引号或换行时按 CSV 规则整体加引号
      const cell = (value: string | number | boolean) => {
        const text = String(value);
        return /[",\n]/.test(text) ? `"${text.replace(/"/g, '""')}"` : text;
      };
      const money = (value: number) => value.toFixed(6);

      const lines: string[] = [];
      lines.push("汇总");
      lines.push(
        "请求数,失败数,输入tokens,输出tokens,缓存tokens,推理tokens,费用USD,未计价请求数",
      );
      lines.push(
        [
          report.totals.requests,
          report.totals.failed,
          report.totals.inputTokens,
          report.totals.outputTokens,
          report.totals.cachedTokens,
          report.totals.reasoningTokens,
          money(report.totals.costUsd),
          report.totals.unpricedRequests,
        ]
          .map(cell)
          .join(","),
      );
      lines.push("");
      lines.push("按模型");
      lines.push("模型,请求数,输入tokens,输出tokens,缓存tokens,费用USD,是否计价");
      for (const row of report.byModel) {
        lines.push(
          [
            row.model,
            row.requests,
            row.inputTokens,
            row.outputTokens,
            row.cachedTokens,
            money(row.costUsd),
            row.priced ? "是" : "否",
          ]
            .map(cell)
            .join(","),
        );
      }
      lines.push("");
      lines.push("每日");
      lines.push("日期,请求数,费用USD");
      for (const day of report.daily) {
        lines.push([day.date, day.requests, money(day.costUsd)].map(cell).join(","));
      }

      try {
        await usageExportCsv(path, `${lines.join("\n")}\n`);
        return null;
      } catch (error) {
        return error instanceof Error ? error.message : String(error);
      }
    },

    setProjectDialogOpen: (projectDialogOpen) => set({ projectDialogOpen }),
    setGoalDialog: (goalDialogOpen, source) =>
      set({ goalDialogOpen, goalDialogSource: source ?? null }),

    toggleSidebar: () => get().setUi({ sidebarCollapsed: !get().sidebarCollapsed }),

    togglePanel: () => get().setUi({ panelCollapsed: !get().panelCollapsed }),

    setPanelTab: (panelTab) => {
      get().setUi({ panelTab, panelCollapsed: false });
    },

    setSection: (section) => {
      if (get().section === section) return;
      set({ section });
      get().setUi({ section });
    },

    attachWorktree: async (conversationId, baseBranch) => {
      try {
        const info = await worktreeAttach(conversationId, baseBranch);
        set((state) => ({ worktrees: { ...state.worktrees, [conversationId]: info } }));
        return null;
      } catch (error) {
        return error instanceof Error ? error.message : String(error);
      }
    },

    detachWorktree: async (conversationId, force) => {
      try {
        await worktreeDetach(conversationId, force);
        set((state) => {
          const next = { ...state.worktrees };
          delete next[conversationId];
          return { worktrees: next };
        });
        return null;
      } catch (error) {
        return error instanceof Error ? error.message : String(error);
      }
    },

    refreshWorktree: async (conversationId) => {
      try {
        const info = await worktreeStatus(conversationId);
        set((state) => {
          const next = { ...state.worktrees };
          if (info) next[conversationId] = info;
          else delete next[conversationId];
          return { worktrees: next };
        });
      } catch {
        // 状态读不到就当没挂：勾选态以 worktrees 这份投影为准，别让界面挂死
      }
    },

    refreshWorktreeBranches: async () => {
      try {
        // 带上话题 id：清单跟"这场话题生效的仓库"走（绑定项目优先，散对话回落激活项目）
        set({ gitBranches: await worktreeBranches(get().activeId || "") });
      } catch {
        set({ gitBranches: null });
      }
    },

    refreshTools: async () => {
      try {
        set({ builtinTools: await builtinToolsList(), toolsError: null });
      } catch (error) {
        set({ toolsError: error instanceof Error ? error.message : String(error) });
      }
    },

    toggleTool: async (id, enabled) => {
      const previous = get().builtinTools;
      const next = previous.map((item) => (item.id === id ? { ...item, enabled } : item));
      set({ builtinTools: next, toolsError: null });

      try {
        await get().updateConfig({
          disabledTools: next.filter((item) => !item.enabled).map((item) => item.id),
        });
      } catch (error) {
        set({
          builtinTools: previous,
          toolsError: `开关没保存：${error instanceof Error ? error.message : String(error)}`,
        });
      }
    },

    // 重新生成：丢弃尾部的助手回复（保留压缩摘要，那不是"回复"），
    // 然后把触发它的用户消息原样重发一轮。没有可重生成的回复就静默返回
    regenerate: async () => {
      // 生成会话没有对话轮：重新生成会把请求打进对话管线（chat/completions），
      // 生图端点接不住。要换一张就继续描述再发
      if ((get().kind ?? "chat") !== "chat") {
        get().pushToast({ tone: "error", title: "生成会话不支持重新生成", detail: "继续描述你的需求，会作为新一轮生成发送。" });
        return;
      }
      const state = get();
      if (state.pending || roundInFlight(state.activeId)) return;
      const msgs = [...state.messages];
      let cut = msgs.length;
      while (cut > 0 && msgs[cut - 1].role === "assistant" && !msgs[cut - 1].summary) cut--;
      if (cut === msgs.length) return;
      if (cut === 0 || msgs[cut - 1].role !== "user") return;
      const question = msgs[cut - 1];
      const questionEntryId = question.entryIds?.[0];
      if (questionEntryId) {
        // 日志里那句问题还在：把分支末端移回它之后重问一次。界面上保留那句问题，
        // 只把回答**挪出当前分支**——旧做法是把它一起删掉再当新问题发一遍，于是日志里
        // 被问了两次、盘上那份又抹掉了旧答案（两份真相）。现在它还在 offPath 里，
        // 切换器一眼就能看到"这是第 2 支，还有第 1 支"
        set({
          messages: msgs.slice(0, cut),
          offPath: [...get().offPath, ...msgs.slice(cut)],
          usage: undefined,
        });
        lastSavedInfo.set(state.activeId, { updatedAt: Date.now(), messageCount: 0 });
        await get().send("", questionEntryId);
        return;
      }
      // 问句没有条目 id：多半是**失败回合**——问题行已经落了日志（回合在追加
      // 之后才死），但 done 事件没来、乐观气泡没领到 id。问后端要树，从 tip 沿
      // 父链往回走：同内容的最近用户行就是日志里那句问题，指着它重问，日志
      // 不再叠行；再往前还有同内容的用户行（失败重试曾经叠出来的），一并跳到
      // 最早那条，旧叠层一并愈合。走到回答行（target 已定）或根就停：回答行
      // 说明那句问题已经被答过，再往前是上一轮的事。找不到（回合死在追加之前，
      // 日志里没有这句）才退回旧行为当新输入发
      try {
        const tree = await fetchConversationTree(state.activeId);
        const byId = new Map(tree.nodes.map((node) => [node.id, node]));
        const preview = question.content.trim().slice(0, 200);
        let node = tree.tip ? byId.get(tree.tip) : undefined;
        let logQuestionId: string | null = null;
        let guard = 0;
        while (node && guard < 512) {
          guard += 1;
          if (node.kind === "message" && node.role === "user") {
            if (node.preview?.trim() === preview) logQuestionId = node.id;
            else break;
          } else if (node.kind === "message" && node.role === "assistant" && logQuestionId) {
            break;
          }
          node = node.parentId ? byId.get(node.parentId) : undefined;
        }
        if (logQuestionId) {
          set({
            messages: msgs.slice(0, cut),
            offPath: [...get().offPath, ...msgs.slice(cut)],
            usage: undefined,
          });
          lastSavedInfo.set(state.activeId, { updatedAt: Date.now(), messageCount: 0 });
          await get().send("", logQuestionId);
          return;
        }
      } catch {
        // 树读不到：退回旧行为
      }
      // 从旧存档载入的话题没有条目 id，只能退回旧行为（把这句话当新输入再发一次）
      const prompt = question.content.trim();
      if (!prompt) return;
      set({
        messages: msgs.slice(0, cut - 1),
        offPath: [...get().offPath, ...msgs.slice(cut - 1)],
        usage: undefined,
      });
      // 与编辑重发同款：重生成是一次新的交互，排序要顶上去
      lastSavedInfo.set(state.activeId, { updatedAt: Date.now(), messageCount: 0 });
      await get().send(prompt);
    },

    // 编辑重发：这条消息之后的对话全部丢弃（改了前提，后面的结论都不成立），
    // 用编辑后的内容重新开始。排序强制顶上去——这是一次新的交互
    editAndResend: async (messageId, content) => {
      if ((get().kind ?? "chat") !== "chat") {
        get().pushToast({ tone: "error", title: "生成会话不支持编辑重发", detail: "继续描述你的需求，会作为新一轮生成发送。" });
        return;
      }
      const state = get();
      if (state.pending || roundInFlight(state.activeId)) return;
      const index = state.messages.findIndex((message) => message.id === messageId);
      if (index === -1) return;
      const text = content.trim();
      if (!text) return;
      const previous = state.messages[index - 1];
      const previousEntryId = previous?.entryIds?.[previous.entryIds.length - 1];
      // 这条之后的对话**不是删掉**，是挪出当前分支：改了前提，后面的结论不成立了，
      // 但那一支说过什么还在树里——想比较"改前怎么答、改后怎么答"就切回去看
      set({
        messages: state.messages.slice(0, index),
        offPath: [...state.offPath, ...state.messages.slice(index)],
        usage: undefined,
      });
      // 重置落盘基准：截断后的消息数比历史少，不重置的话"新增消息"判定会失效
      lastSavedInfo.set(state.activeId, { updatedAt: Date.now(), messageCount: 0 });
      if (index === 0) {
        // 编辑第一条：日志退到根之前，整段历史都让位给这句新话
        await get().send(text, null);
      } else if (previousEntryId) {
        await get().send(text, previousEntryId);
      } else {
        // 旧存档的话题没有条目 id：只能像以前那样只切界面，后端日志与界面会分叉
        await get().send(text);
      }
    },

    // 换到同一处的另一条分支：重投影 + 把后端的分支末端也移过去。
    // 一把锁仍是"一话题一现场"——正在跑的那一轮不许被换掉底（它写的是这一支的尾巴）
    switchBranch: async (messageId) => {
      const state = get();
      if (state.pending || roundInFlight(state.activeId)) return;
      const nodes = [...state.messages, ...state.offPath];
      const tip = branchTail(nodes, messageId);
      if (!tip) return;
      const { thread, offPath } = partition(nodes, tip);
      set({ messages: thread, offPath });
      // 后端的末端要跟着走，否则界面停在 A 支、下一次追加挂在 B 支之后。
      // 目标那一支没有条目 id（分支树之前的旧存档）时无从移动——那种存档本来也没有
      // 兄弟分支可切，这里就只改视图，不发一次会写错位置的请求
      const entryId = thread[thread.length - 1]?.entryIds?.at(-1);
      if (entryId) {
        try {
          await conversationNavigate(state.activeId, entryId);
        } catch {
          // 移不动就是移不动：视图已经换了，下一发 send 会带着 rewindTo 再试一次
        }
      }
      void persistCurrent();
    },

    send: async (prompt, rewindTo) => {
      // 兜底分流：生成会话里任何打到 send 的路径都改走生成管线。
      // composer 的 submit 已在前面分岔，这里接住的是绕过它的那些调用方
      if ((get().kind ?? "chat") !== "chat" && !(rewindTo !== undefined && prompt.trim().length === 0)) {
        await get().sendMedia(prompt);
        return;
      }
      // 发送前自愈幽灵 pinned（历史版本只换模型名不换档案写出来的，Rust 按对校验
      // 成员表会每发必拒）：切档/开会话时也会修，这里接住"开着旧会话直接发"的那条路
      if (pinnedIsGhost(get().config.modelPool)) {
        const pool = get().config.modelPool;
        const repaired = switchPinnedMember(pool, pool.pinned?.model ?? "");
        await get().updateConfig(
          repaired
            ? { modelPool: repaired }
            : { modelPool: { ...pool, mode: "auto", pinned: null } },
        );
      }
      const trimmed = prompt.trim();
      const { attachments, pending } = get();
      // rewindTo 带着、正文为空 = 重新生成：后端会把分支末端移回那句问题，
      // 而那句问题已经在日志里了，再追加一遍就是问两次
      const redo = rewindTo !== undefined && trimmed.length === 0 && attachments.length === 0;
      // 空正文只有在"重新生成"这一条路上才合法（那句问题已经在日志里）；其余情况照旧拦下
      // 最后一条：一条话题上只压着一轮。跟随轮的幕间那一刻 `pending` 是 false，
      // 这时候再发一遍会把两轮的正文写进同一份现场
      // 已 settled 的那一轮只差一次落盘删除，不算压着话题：Done 后那半秒发的新消息
      // 会被旧守卫静默吞掉，而草稿早被 composer 清了——那是"发送没反应"的另一半
      const parked = runs.get(get().activeId);
      const occupied = pending || (parked !== undefined && !parked.settled);
      // 目标线程还在跑：带现场的看 modeArmed（目标轮的幕间 pending 是 false，但现场没落定）；
      // 「继续」/自动续跑开出的广播轮没有现场，看 goalKicked。此时这句话不开并行回合，
      // 此时这句话不开并行回合，转插话——人的话永远优先，而排队是 Ctrl+回车那条
      // 由人明说的路（从前这里按目标的"插话策略"分流，那一格已删）
      const goalThreadBusy =
        !redo &&
        trimmed.length > 0 &&
        (goalKicked.has(get().activeId) ||
          (parked !== undefined &&
            !parked.settled &&
            parked.modeArmed &&
            parked.mode?.objective != null));
      if ((!trimmed && attachments.length === 0 && !redo) || (occupied && !goalThreadBusy)) return;
      // 双发闸：占用判定与 `runs.set` 占上之间不许有 await 空窗——第一发悬在
      // 记忆命令/插话的 await 里时，第二发的判定看到的还是空的，两发都过闸，
      // 后到的 `runs.set` 把先到的现场整个顶掉，先到那轮从此对事件与停止都失联。
      // 判定+占下同帧完成（JS 单线程内原子），过了这段 await 区间闸就放行：
      // 出了 finally 到 runs.set 之间全是同步代码，别的 send 插不进来
      if (sendGuard.has(get().activeId)) return;
      const sendGuardId = get().activeId;
      sendGuard.add(sendGuardId);
      try {
        if (goalThreadBusy) {
          await sendIntoGoalThread(trimmed);
          return;
        }

        // 记忆命令是给客户端的指令，不是给模型的提示词：在这儿分岔，一个字节都不落到网络那端。
        // 放在构造气泡之前——它既不该占一轮"生成中"，也不该进后端话题日志
        if (!redo && trimmed.startsWith("/")) {
          const outcome = await runMemoryCommand(trimmed, get().activeId);
          if (outcome.handled) {
            const stamp = Date.now();
            set((s) => ({
              messages: [
                ...s.messages,
                { id: newId("msg"), role: "user" as const, content: trimmed, createdAt: stamp },
                {
                  id: newId("msg"),
                  role: "assistant" as const,
                  content: outcome.text,
                  createdAt: stamp + 1,
                  note: true,
                },
              ],
            }));
            void persistCurrent();
            return;
          }
        }
      } finally {
        sendGuard.delete(sendGuardId);
      }

      // 历史不再由这里拼：后端持有话题日志，那份日志就是发送源。
      // 这里的 `messages` 只负责界面（显示与本地存档），它决定不了模型看到什么。
      // 以前每轮都要从这份台账反推一遍"模型见过的那份消息"，于是形状、文案、
      // 顺序三处各失真一次（F1/F2/F13 都是这一个根因的症状）

      const now = Date.now();
      const replyId = newId("msg");
      // 这一轮流属于哪个话题。切走话题不会中止它：后面的增量继续写进这一轮自己的现场
      // （`patchRunOf`），只是那份现场当下不投影到界面上
      const ownerId = get().activeId;
      // 父子关系抄的是日志的形状：日志里下一条追加永远挂在当前末端之后，
      // 而界面这条路径的末端就是 `messages` 的最后一条。自己编一套就对不上了
      const tail = get().messages[get().messages.length - 1];
      const userMessage: Message = {
        id: newId("msg"),
        role: "user",
        content: trimmed,
        createdAt: now,
        parentId: tail?.id ?? null,
        // 附件元数据跟着气泡走（含内存态缩略图）——对话区要能看见自己发过图
        attachments:
          attachments.length > 0
            ? attachments.map((item) => ({
                name: item.name,
                kind: item.kind ?? ("text" as const),
                path: item.path,
                bytes: item.chars,
                previewDataUrl: item.previewDataUrl,
              }))
            : undefined,
      };
      const reply: Message = {
        id: replyId,
        role: "assistant",
        content: "",
        createdAt: now + 1,
        streaming: true,
        toolCalls: [],
        // 正常发送的父是刚贴上去的那句问题；重新生成不重贴问题，父就是那句原问题
        // ——于是旧答案自然成了同一父下的另一支，不用删它也看不见不了
        parentId: redo ? tail?.id ?? null : userMessage.id,
      };

      // 重新生成不往界面上再贴一句同样的问题；正常发送才贴
      const appended: Message[] = redo ? [] : [userMessage];
      const run: LiveRun = {
        conversationId: ownerId,
        projectId: get().projectId,
        title: get().messages.length === 0 ? (trimmed || attachments[0]?.name).slice(0, 24) : get().title,
        kind: get().kind,
        messages: [...get().messages, ...appended, reply],
        offPath: get().offPath,
        usage: undefined,
        pending: true,
        settled: false,
        save: Promise.resolve(),
        // 一次新的发送重开一轮跟随计数：排队的归属从这一轮重新算
        followUpCount: 0,
        followUpBubbleIds: [],
        // 模式读数带着当下那一份走；后面的每一轮收尾由后端的 `mode` 事件更新它
        mode: get().mode,
        modeArmed: false,
      };
      runs.set(ownerId, run);
      // 链路动画从这一发重新走
      set({ journey: [] });
      setRunning(ownerId, true);

      const patchRun = (patch: RunPatch | ((run: LiveRun) => RunPatch)) => patchRunOf(run, patch);
      set({ attachments: [] });
      syncRun(run);

      // 新话题发出第一句就进侧栏名单。等到模型答完才出现，中间那一整分钟列表里
      // 什么都没有，看着就像这句根本没发出去。
      // **这一发只落壳（标题），不落消息行**：新话题在磁盘上还没有日志文件，
      // 这份预落盘的记录会在 chat_send 第一次开话题时被整份迁移成日志——
      // 把乐观的用户气泡也迁移进去，chat_send 再追加一遍真正的用户行，
      // 同一句话在日志里就有了两行（实测相隔 13ms，一条带 msg_* 前端 id、
      // 一条带后端 id），屏上就是"发一条显示两条"。消息行归后端写，
      // 收尾那次全量保存（done 上的 persistConversation）补齐一切
      if (!get().conversations.some((item) => item.id === ownerId)) {
        run.save = persistConversation(
          {
            ...runFields(run),
            messages: run.messages.filter(
              (message) => message.id !== replyId && message.id !== userMessage.id,
            ),
          },
          { allowShell: true },
        );
      }

      // 跟随轮会接出第二条回复气泡：补丁函数始终打到"当前那一条"上，
      // startNextTurn 换 id 之后旧缓冲照样落进新气泡
      let currentReplyId = replyId;
      // 当前的乐观用户气泡：普通发送是发起那条；跟随轮是排队时插上的那条
      let currentUserBubbleId: string | null = redo ? null : userMessage.id;
      // 跟随轮的预备旗标：Done 里看到队列还有货就举起，下一个事件到达时开新气泡
      let followUpArmed = false;
      // 第几轮（0 = 本次发送本身）。后端每条跟随恰好多出一轮、多回一个 Done
      let turnIndex = 0;

      const patchReply = (updater: (message: Message) => Message) =>
        patchRun((r) => ({
          messages: r.messages.map((message) =>
            message.id === currentReplyId ? updater(message) : message,
          ),
        }));

      const patchUser = (updater: (message: Message) => Message) =>
        currentUserBubbleId === null
          ? undefined
          : patchRun((r) => ({
              messages: r.messages.map((message) =>
                message.id === currentUserBubbleId ? updater(message) : message,
              ),
            }));

      // 跟随轮开幕：上一条回复已经收尾，挂一条新的流式气泡接住这一轮
      const startNextTurn = () => {
        currentReplyId = newId("msg");
        contentStarted = false;
        patchRun((r) => ({
          messages: [
            ...r.messages,
            {
              id: currentReplyId,
              role: "assistant" as const,
              content: "",
              createdAt: Date.now(),
              streaming: true,
              toolCalls: [],
            },
          ],
          pending: true,
        }));
      };

      const patchTool = (call: ToolCall) =>
        patchReply((message) => {
          const others = (message.toolCalls ?? []).filter((item) => item.id !== call.id);
          return { ...message, toolCalls: [...others, call] };
        });

      /** 流程里给这次调用占一格。同一个 id 会来好几次（running→done），只占一格；
       *  摘要与状态由 ToolCard 自己从 toolCalls 里读，这里不另存一份 */
      const pushToolStep = (callId: string, contentChars?: number | null) =>
        patchReply((message) =>
          (message.steps ?? []).some((step) => step.kind === "tool" && step.callId === callId)
            ? message
            : {
                ...message,
                steps: [
                  ...(message.steps ?? []),
                  {
                    kind: "tool" as const,
                    id: newId("step"),
                    callId,
                    contentChars: contentChars ?? undefined,
                  },
                ],
              },
        );

      const batchers: Array<{ push: (text: string) => void; flush: () => void }> = [];

      // 增量按 60ms 合并，否则每个 token 都会让 Markdown 重解析一次
      function createBatcher(append: (text: string) => void) {
        let buffer = "";
        let timer: ReturnType<typeof setTimeout> | undefined;

        const flush = () => {
          if (timer) {
            clearTimeout(timer);
            timer = undefined;
          }
          if (!buffer) return;
          const text = buffer;
          buffer = "";
          append(text);
        };

        const batcher = {
          push(text: string) {
            buffer += text;
            if (!timer) timer = setTimeout(flush, 60);
          },
          flush,
        };
        batchers.push(batcher);
        return batcher;
      }

      const contentBatcher = createBatcher((text) =>
        patchReply((message) => ({ ...message, content: message.content + text })),
      );
      const reasoningBatcher = createBatcher((text) =>
        patchReply((message) => ({ ...message, reasoning: (message.reasoning ?? "") + text })),
      );

      // 一轮跑完。跟随队列里还有货时它还会再来一次（每一轮各落一次盘）
      const finish = (error?: string) => {
        for (const batcher of batchers) batcher.flush();
        patchReply((message) => ({
          ...message,
          streaming: false,
          reasoningStreaming: false,
          error,
          // 流程条收起之后要还能报"这一轮坐了多久"。取界面这一侧的墙钟：
          // 服务商自己报的那个数不含排队与工具时间，跟用户等的时间不是一回事
          durationMs: Date.now() - message.createdAt,
        }));
        // 失败原因贴到右上角，不再写进模型正文里；正文只留一行短标记。
        // 标记必须留着——半截答案冒充完整答案是最难查的骗人方式
        if (error) get().pushToast({ tone: "error", title: "本轮请求失败", detail: error });
        patchRun({ pending: false });
        // 本轮可能写了文件。消息条数在这一轮里根本没变过（气泡早就在了），
        // 所以不能靠它当信号——必须在这里刷，否则审阅卡和预览永远慢一整轮。
        // 只在看着它的时候刷：台账说的是你正在看的那条话题，刷成别那条比不刷更糟
        if (get().activeId === ownerId) void get().refreshEdits();
        // 分支读数同理：这一轮可能提交过、建过分支——Worktree 的基分支菜单
        // 不该捧着上一轮的旧账（ unborn 仓库提交完第一次，"未提交"就该消失）。
        // 一次 git branch 的成本，每轮收尾跑一次不算重
        void get().refreshWorktreeBranches();
        // chat_send 立即返回，落盘只能挂在 done/error 上，不能挂在 await 之后。
        // 落的是这一轮自己那份——切走了也照落，此刻界面上是别的话题
        run.save = persistConversation(runFields(run));
        // 自动提取挂在收尾这里、不 await：记忆是一轮的副产品，不该让用户等它。
        // 带 note 的是客户端回执，它没进过模型——把它当"用户的事实"喂回去就闭环成自说了
        extractTurns += 1;
        if (!error && extractTurns % EXTRACT_EVERY === 0) {
          const recent = run.messages
            .filter((message) => !message.note && message.content.trim().length > 0)
            .slice(-8)
            .map((message) => ({ role: message.role, content: message.content }));
          // 决策层嵌入（integrations.memoryGate）：先在本地判一轮"值不值得提取"。
          // 判定是 private 请求——钉在本地，对话正文不为省一次调用多出一条出网的路。
          // fail-open：开关关着、sidecar 没起、判定失败，verdict 是 null → 照旧提取
          void gateMemoryExtraction(recent, ownerId)
            .then((verdict) => (verdict?.skip ? null : memoryExtract(ownerId, recent)))
            .then((summary) => {
              if (summary && summary.candidates > 0) {
                get().pushToast({
                  tone: "info",
                  title: `有 ${summary.candidates} 条候选记忆待确认`,
                  detail: "在 设置 → 记忆 里逐条确认或丢弃。",
                });
              }
            })
            .catch((reason: unknown) => {
              // 提取失败不影响这一轮的回答，但也不该一点动静都没有：
              // 悄悄漏掉的记忆是最难查的——用户会以为它已经记住了
              get().pushToast({ tone: "info", title: "这轮的记忆提取没成功", detail: String(reason) });
            });
        }
      };

      /** 这一轮真的结束了（跟随队列跑空，或者失败了）：摘掉"在跑"、停掉看门狗，
       *  并等 finish 那一次落盘完成再把现场交还给存档。在那之前切回来看的仍是这一轮的
       *  进度，而不是存档里那一截 */
      const endRun = () => {
        if (run.settled || runs.get(ownerId) !== run) return;
        run.settled = true;
        run.pending = false;
        setRunning(ownerId, false);
        // 输入框的"发送中"跟着回合走：错误收尾没有后续事件来翻它，
        // 不在这里归零的话失败之后发送与重试会一直被守卫静默拦下
        patchRun({ pending: false, followUpCount: run.followUpCount });
        window.clearInterval(watchdog);
        // 这一支跑完了，身上挂着的目标（哪一档都算）交给面板记着，
        // 现场撤掉后面板才不会一直挂着一个"不在跑"的影子
        if (run.mode?.objective) noteGoalMode(ownerId, run.mode, true);
        void run.save.then(() => {
          if (runs.get(ownerId) === run) runs.delete(ownerId);
          refreshGoalPanels();
        });
      };

      let contentStarted = false;

      const apply = (event: ChatEvent) => {
        // 收尾之后服务商不该再说话；真说了（例如跟随轮的尾巴落在 Done 之后）就整个作废，
        // 免得往已经落盘的正文后面接字
        if (run.settled) return;

        // 跟随轮或目标自动续跑的第一个事件到了：上一条回复已收尾，开一条新气泡接住这一轮。
        // 后端在同一个通道里顺序跑完所有轮，这里是它们的分幕
        if (!run.pending && (followUpArmed || run.modeArmed)) {
          followUpArmed = false;
          patchRun({ modeArmed: false });
          startNextTurn();
        }

        switch (event.type) {
          case "reasoning": {
            // 思考过程是"看/不看"的界面偏好，关掉时连流式缓冲都不进——
            // 免得面板关了字还在攒，开一次开关涌出一大段旧思考
            if (!get().config.showReasoning) break;
            // 起点要读已经落地的正文，所以先把缓冲冲掉：晚一步就会把上一段的尾巴
            // 算进下一段的起点，流程里那一行于是重复显示同一段思考
            reasoningBatcher.flush();
            // 思考段的正文位置章同理：先冲正文缓冲，再读 content.length
            contentBatcher.flush();
            patchReply((message) => {
              const last = message.steps?.at(-1);
              // 上一格已经是思考了就是"还在想"，不另起一段；中间隔了别的事才算新的一段
              if (last?.kind === "thinking") return { ...message, reasoningStreaming: true };
              return {
                ...message,
                reasoningStreaming: true,
                steps: [
                  ...(message.steps ?? []),
                  {
                    kind: "thinking" as const,
                    id: newId("step"),
                    at: Date.now(),
                    from: (message.reasoning ?? "").length,
                    contentChars: message.content.length,
                  },
                ],
              };
            });
            reasoningBatcher.push(event.text);
            break;
          }
          case "delta": {
            if (!contentStarted) {
              contentStarted = true;
              patchReply((message) => ({ ...message, reasoningStreaming: false }));
            }
            contentBatcher.push(event.text);
            break;
          }
          case "compaction": {
            // 压缩状态直接长在对话流里：开始时插一条"压缩中"，
            // 完成后把它换成摘要消息，并把被压缩的旧消息截掉——
            // 本地话题与发出去的上下文保持一致，下一轮才不会重复压缩
            if (event.phase === "start") {
              patchRun((r) => ({
                messages: [
                  ...r.messages.filter((message) => message.id !== COMPACTION_MSG_ID),
                  {
                    id: COMPACTION_MSG_ID,
                    role: "assistant" as const,
                    content: "【上下文压缩中】正在把更早的对话压缩成摘要，完成后自动继续。",
                    createdAt: Date.now(),
                  },
                  ...r.messages.filter((message) => message.id === currentReplyId),
                ],
              }));
            } else {
              const summaryMessage: Message = {
                id: newId("msg"),
                role: "assistant",
                content: `【上下文压缩完成】更早的对话已压缩成摘要，任务上下文已衔接，继续处理中。\n\n${event.summary ?? ""}`,
                createdAt: Date.now(),
                summary: true,
              };
              const kept = Math.max(event.kept ?? 0, 0);
              patchRun((r) => {
                const index = r.messages.findIndex((message) => message.id === COMPACTION_MSG_ID);
                // 没找到 start 那条（理论上不该发生）就退化为只追加摘要
                if (index === -1) {
                  return { messages: [...r.messages, summaryMessage] };
                }
                const before = r.messages.slice(0, index);
                const after = r.messages.slice(index + 1);
                return { messages: [...before.slice(-kept), summaryMessage, ...after] };
              });
            }
            break;
          }
          case "mode": {
            // 后端每一轮跑完都会把这一支的作业模式读数报一次。`continuing` 是它给下一个
            // Done 的预告：true 就别让这一轮落定，还有一轮要自己接下去
            const previous = run.mode;
            // 目标面板同步记一份：这条话题切走了面板也认得它跑到哪儿了。
            // 认的是"身上还挂着目标"——对话档下推进的那些轮也得进这份账
            if (event.state.objective) noteGoalMode(ownerId, event.state, true);
            patchRun({ mode: event.state, modeArmed: event.continuing });
            // 由"还在推进"转成收尾或受阻，说一句就走。它不是"这一轮残缺"——那一轮答完了，
            // 是这一支不再自己往下跑；留在正文里等于替模型说了一句它没说的话
            const settledNow =
              previous?.status === "active" && event.state.status !== "active";
            if (settledNow && get().activeId === ownerId) {
              get().pushToast({
                tone: "info",
                title: event.state.status === "complete" ? "目标报完了" : "目标停下了",
                detail: event.state.note ?? "没附上那一句结论。",
              });
            }
            break;
          }
          case "notice": {
            // 半句话冒充完整答案是最难查的骗人方式，所以把它接在正文里说清楚
            contentBatcher.push(`\n\n> ${event.text}`);
            break;
          }
          case "retry": {
            // 不进正文：重试成功后接出来的是完整回答，正文里夹一条失败告警只会
            // 让人以为那段回答本身就是断的
            get().pushToast({
              tone: "info",
              title: `请求失败，${event.text}`,
              detail: event.reason,
            });
            break;
          }
          case "tool": {
            // 工具状态要立刻落地，审批卡晚一帧就会出现"按钮能点但状态还是旧的"
            patchTool({
              id: event.id,
              name: event.name,
              status: event.status,
              risk: event.risk,
              input: event.input,
              output: event.output,
              arguments: event.arguments,
              passReason: event.passReason,
              contentChars: event.contentChars ?? null,
            });
            pushToolStep(event.id, event.contentChars);
            // 写一个显示一个：长回合里等到整轮结束才刷新，前面写的文件在面板上
            // 一直是隐形的。失败与拒绝不刷——后端只在真写成功时才落账。
            // 切走了不刷：台账跟着的是你正在看的那条话题
            if (
              event.name === "write_file" &&
              event.status === "done" &&
              get().activeId === ownerId
            )
              void get().refreshEdits();
            break;
          }
          case "probe": {
            // 链路动画的阶段：同 key 原位更新（每个请求重放管线），新 key 追加
            set((s) => {
              const journey = [...s.journey];
              const at = journey.findIndex((stage) => stage.key === event.key);
              if (at === -1) journey.push({ key: event.key, detail: event.detail });
              else journey[at] = { key: event.key, detail: event.detail };
              return { journey };
            });
            break;
          }
          case "plan": {
            // 计划整份替换：后端每次都给全量，这里不合并、不排序
            set((s) => ({
              plans: { ...s.plans, [ownerId]: { explanation: event.explanation, steps: event.steps } },
            }));
            break;
          }
          case "ask": {
            // 提问挂进桶里，卡片只认当前正在看的这条；后端这一发在等人，没有超时
            set((s) => ({
              pendingQuestions: {
                ...s.pendingQuestions,
                [ownerId]: { id: event.id, question: event.question, options: event.options },
              },
            }));
            break;
          }
          case "done": {
            // 后端登记的条目 id 贴回这两条消息：重新生成与编辑重发要指名
            // "把分支末端移到哪条之后"，而那个名字只有后端知道。
            // 同一趟车上的是这一发实发的模型名：池子换过人，配置里那个名字不是它的真相
            patchReply((message) => ({ ...message, entryIds: event.entryIds, model: event.model }));
            // 挂起中的提问随这一轮收尾一起作废：回答了的走工具结果，没回答的是"用户没有回答"
            set((s) => {
              if (!(ownerId in s.pendingQuestions)) return s;
              const { [ownerId]: _cleared, ...rest } = s.pendingQuestions;
              return { pendingQuestions: rest };
            });
            // 用户行条目 id 贴回它对应的乐观气泡：本次发送贴发起那条，
            // 跟随轮贴排队时插上的那条（FIFO 与后端队列同序）
            if (turnIndex === 0) {
              if (!redo) patchUser((message) => ({ ...message, entryIds: [event.entryIds[0]] }));
            } else {
              const followUpBubbleId = run.followUpBubbleIds.shift();
              if (followUpBubbleId) {
                patchRun((r) => ({
                  messages: r.messages.map((message) =>
                    message.id === followUpBubbleId
                      ? { ...message, entryIds: [event.entryIds[0]] }
                      : message,
                  ),
                }));
              }
            }
            turnIndex += 1;
            patchRun({
              usage: {
                inputTokens: event.inputTokens,
                outputTokens: event.outputTokens,
                cachedTokens: event.cachedTokens,
                durationMs: event.durationMs,
                // 这一发实际生效的窗口：面板分母跟它走，与后端预算表同一格读数
                contextTokens: event.contextTokens,
              },
            });
            // 链路动画的收尾格：这一发的 token 账
            set((s) => ({
              journey: [
                ...s.journey.filter((stage) => stage.key !== "usage"),
                { key: "usage", detail: `${event.inputTokens} → ${event.outputTokens} tokens` },
              ],
            }));
            // 首轮完成后给话题起个像样的名字：异步、失败静默——标题只是便利，不该打扰对话
            if (run.title === "新话题") {
              // 客户端回执不参与起标题：它说的是本机发生了什么，不是用户在问什么
              const named = run.messages
                .filter((message) => !message.note && message.content.trim().length > 0)
                .slice(0, 5)
                .map((message) => ({ role: message.role, content: message.content }));
              if (trimmed) named.push({ role: "user" as const, content: trimmed });
              void generateTitle(named)
                .then((title) => {
                  // 现场还在（或者就在眼前）才认这个标题：已经交还给存档的一轮不该再回头写
                  if (!title || run.title !== "新话题" || runs.get(ownerId) !== run) return;
                  patchRun({ title });
                  run.save = persistConversation(runFields(run));
                })
                .catch(() => undefined);
            }
            // 跟随计数：每条排队的跟随恰好多出一轮、多回一个 Done。
            // 队列还有货就举起预备旗，后端的下一轮事件一到就开新气泡
            const remaining = run.followUpCount;
            if (remaining > 0) {
              patchRun({ followUpCount: remaining - 1 });
              followUpArmed = true;
            }
            finish();
            // 队列跑空、后端也没说"还有一轮要自己接"，才是真的结束。目标模式那一路
            // 现场得留着：一落定，续跑那一轮的字节就整批被丢掉
            if (remaining === 0 && !run.modeArmed) endRun();
            break;
          }
          case "error": {
            // 失败时后端已把跟随队列清空：计数归零，不再开幕
            followUpArmed = false;
            patchRun({ followUpCount: 0 });
            // 这一发没跑到收尾判据，立着的暂停旗被后端兜底清了：乐观那一格跟着撤
            goalOptimisticPaused.delete(ownerId);
            finish(event.message);
            endRun();
            break;
          }
        }
      };

      // 回合看门狗：服务商静默超过 90 秒就提示一次，提示完不打扰（用户可自行停止）
      const lastEventAt = { value: Date.now() };
      let watchdogFired = false;
      const watchdog = window.setInterval(() => {
        // 这一轮已经不在了（收尾落完盘、或者话题被删）：定时器不该继续活着
        if (run.settled || runs.get(ownerId) !== run) {
          window.clearInterval(watchdog);
          return;
        }
        if (!watchdogFired && Date.now() - lastEventAt.value > 90_000) {
          watchdogFired = true;
          // 挂起提示不进正文：它说的是"现在"，不是模型答了什么。
          // 塞进回答里，滚上去之后就再也找不到这条提醒过什么
          get().pushToast({
            tone: "info",
            title: "服务商已超过 90 秒没有返回",
            detail: "可以继续等待，或点输入框旁的停止按钮中断后重试。",
          });
        }
      }, 15_000);

      try {
        // 决策层嵌入（integrations.modelRouting，观测面）：任务类型/复杂度进决策审计，
        // 不改模型选择——模型是档案级的产品决策，自动换档等 Phase 4 面板把账摊出来再说
        void routeModel(userMessage.content, undefined, ownerId);
        // 决策层调度（池子 mode = "decision"）：发送前问一次 System 1（Laya 本地 /
        // Jev 云端，漏斗与敏感性归决策层自己的配置）这一发交给池里哪个成员。
        // fail-open：决策层关着/没答上/答非所选，poolPick 缺席，Rust 侧自动退回调度器；
        // 跟随轮沿用同一个结果，后端不再问第二次
        let poolPick: { profileId: string; model: string } | null = null;
        const pool = get().config.modelPool;
        if (pool.mode === "decision") {
          const members = pool.members
            .filter((member) => member.enabled && member.model.trim().length > 0)
            .map((member) => ({
              profileId: member.profileId,
              model: member.model,
              label: `${member.model}（${
                member.profileId === ""
                  ? "当前连接"
                  : get().config.profiles.find((profile) => profile.id === member.profileId)
                      ?.name ?? "已删除的档案"
              }）`,
            }));
          const picked = await pickPoolMember(userMessage.content, members, ownerId);
          if (picked) poolPick = { profileId: picked.profileId, model: picked.model };
        }
        await sendChat({
          // 占位串只属于"带附件但没写字"那种发送。重新生成是空正文空附件，
          // 带着占位串下去会被后端当成一句新问题落进日志（实盘测出：日志里多出一条「（仅附件）」）
          message: userMessage.content || (attachments.length > 0 ? "（仅附件）" : ""),
          attachments: attachments.map((item) => item.path),
          conversationId: ownerId,
          rewindTo,
          skipMemory: consumeMemorySkipNextTurn(),
          poolPick,
          onEvent: (event) => {
            lastEventAt.value = Date.now();
            apply(event);
          },
        });
      } catch (error) {
        finish(error instanceof Error ? error.message : String(error));
        endRun();
      }
      // 这里没有 finally：chat_send 开了线程就立刻返回，收尾挂在 done/error 上。
      // 早先那个 finally 会在流刚开头时就把这一轮当成已经结束
    },

    stopGeneration: async () => {
      // 停的是你正在看的那一条：并行跑着好几轮时，切走了就不该杀到别条话题的回合
      const run = runs.get(get().activeId);
      if (!run) {
        // 这颗按钮只在生成中出现，走到这里说明界面与现场对不上。静默返回等于让人
        // 以为自己点坏了——它得说一句
        get().pushToast({
          tone: "info",
          title: "这一条话题现在没在跑",
          detail: "没什么可以停。",
        });
        return;
      }
      // 停止的语义是"到此为止"：跟随队列由后端清空，计数同步归零
      patchRunOf(run, { followUpCount: 0 });
      try {
        await chatAbort(run.conversationId);
        // 闸拉起来不等于立刻停：SSE 要读完当前这一行才退出来，思考模型一段可以几十秒
        // 不出字。不先给这句话，点下去看着就是"毫无反应"
        get().pushToast({
          tone: "info",
          title: "已请求停止",
          detail: "这一发会在读完当前这一段后停下。",
        });
      } catch (error) {
        // 后端那句"这条话题现在没有正在跑的回合"就从这里出来，不许只进 console
        get().pushToast({ tone: "error", title: "停止没生效", detail: String(error) });
        console.error("停止生成失败", error);
      }
    },

    clearDraftRestore: () => set({ draftRestore: null }),

    // 插话与排队都只针对"你正在看的那一条话题"上还在跑的那一轮：
    // 乐观气泡必须进那一轮的现场，否则会掉进别的话题的正文里
    steer: async (text) => {
      const run = runs.get(get().activeId);
      if (!run) return;
      const message: Message = {
        id: newId("msg"),
        role: "user",
        content: text,
        createdAt: Date.now(),
      };
      patchRunOf(run, (r) => ({ messages: [...r.messages, message] }));
      try {
        await chatSteer(run.conversationId, text);
      } catch (error) {
        // 入队失败就把乐观消息撤回来：不能让用户以为模型看到了那句插话。
        // 话不丢——放回输入框：回合竞态的缝隙刚合上，再按一次发送即可
        patchRunOf(run, (r) => ({
          messages: r.messages.filter((item) => item.id !== message.id),
        }));
        set({ draftRestore: text });
        get().pushToast({
          tone: "info",
          title: "插话没插进去",
          detail: `${error instanceof Error ? error.message : String(error)}这句已放回输入框。`,
        });
      }
    },

    followUp: async (text) => {
      const run = runs.get(get().activeId);
      if (!run) return;
      const message: Message = {
        id: newId("msg"),
        role: "user",
        content: text,
        createdAt: Date.now(),
      };
      patchRunOf(run, (r) => ({ messages: [...r.messages, message] }));
      try {
        const count = await chatFollowUp(run.conversationId, text);
        // 气泡 id 进这一轮自己的 FIFO：后端每条跟随各回一个 Done，按同样的顺序对上号
        run.followUpBubbleIds.push(message.id);
        patchRunOf(run, { followUpCount: count });
      } catch (error) {
        // 入队失败就撤回乐观气泡：不能让用户以为排上了。话不丢——放回输入框
        patchRunOf(run, (r) => ({
          messages: r.messages.filter((item) => item.id !== message.id),
        }));
        set({ draftRestore: text });
        get().pushToast({
          tone: "info",
          title: "没排上队",
          detail: `${error instanceof Error ? error.message : String(error)}这句已放回输入框。`,
        });
      }
    },

    forkFrom: async (conversationId, entryId) => {
      const newConversationId = await conversationFork(conversationId, entryId);
      await get().refreshHistory();
      await get().openConversation(newConversationId);
    },

    // 手动压缩：只压对话正文（跳过既有摘要），压完整个话题就是一条摘要——
    // 这是用户主动"清空脑子重新开始"的开关，所以压得比自动压缩更彻底
    compactConversation: async () => {
      // 生成会话没有对话上下文：没有可压缩的日志，压缩调用只会打到生图端点上
      if ((get().kind ?? "chat") !== "chat") {
        get().pushToast({ tone: "error", title: "生成会话没有可压缩的上下文", detail: "每次生成相互独立，不存在需要压缩的对话历史。" });
        return;
      }
      // 压缩发生在后端的话题日志上（写一条 compaction 条目）。这里只把界面收成一条摘要行。
      // 旧做法是把界面里的数组送下去当压缩输入——那等于让界面决定模型的历史
      const summary = await compactHistory(get().activeId);
      set({
        messages: [
          {
            id: newId("msg"),
            role: "assistant",
            content: `【上下文压缩完成】对话历史已压缩成摘要，任务上下文已衔接。\n\n${summary}`,
            createdAt: Date.now(),
            summary: true,
          },
        ],
      });
      // 压缩结果落盘。排序语义与自动保存一致：内容大变但不是"新消息"，列表位置不动
      await persistCurrent();
    },
  };
});
