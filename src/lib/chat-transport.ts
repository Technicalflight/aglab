import { Channel, invoke } from "@tauri-apps/api/core";
import type {
  AppConfig,
  Attachment,
  BuiltinSubagentView,
  BuiltinTool,
  CcswitchCandidate,
  ChatEvent,
  ChatMessage,
  ConversationMeta,
  ConversationRecord,
  ConversationStore,
  ContextBreakdown,
  DiffScope,
  EditPreview,
  FileDiff,
  FileEdit,
  MarketView,
  AiImportSource,
  AiImportOutcome,
  McpCandidate,
  McpImportResult,
  McpServerView,
  ModeState,
  ContractView,
  ModelPrice,
  GoalStatus,
  RunningCommand,
  WorkingMode,
  PluginsListing,
  ReviewInfo,
  RevertOutcome,
  SkillsListing,
  SkillCandidate,
  SkillImportResult,
  SessionCacheUsage,
  StorageInfo,
  StorageSwitch,
  TaskApproval,
  TaskRun,
  TaskView,
  ConversationPage,
  UsageReport,
  UsageRequestPage,
  CustomSecretRule,
  SecretRulePatternEdit,
} from "@/types/chat";
export type { ConversationUsage } from "@/types/chat";

/** pool_catalog 的一条：一个可加进池子的（档案 × 模型）来源。
 *  error 只描述"这个来源这次没拉到"——一家服务商挂了不挡别家的目录 */
export interface PoolCatalogEntry {
  profileId: string;
  name: string;
  baseUrl: string;
  apiFormat: string;
  models: string[];
  error: string | null;
}

/** pool_stats 的一条：一个池成员此刻的调度读数 */
export interface PoolMemberStat {
  profileId: string;
  model: string;
  /** 被调度过的回合总数 */
  total: number;
  /** 当前正在跑的回合数 */
  inflight: number;
  /** 连续失败次数。一次成功就清零 */
  failures: number;
  /** 剩余冷却毫秒。0 = 没在冷却 */
  coolingMs: number;
}

export const fetchPoolCatalog = () => invoke<PoolCatalogEntry[]>("pool_catalog");
export const fetchPoolStats = () => invoke<PoolMemberStat[]>("pool_stats");

/** proxy_pool_stats 的一条：一条代理此刻的调度读数（账本在 Rust proxy.rs） */
export interface ProxyStat {
  id: string;
  /** 上路过的次数，含换路里的每一次尝试 */
  total: number;
  /** 当前正在跑的请求数 */
  inflight: number;
  /** 连续「连不上」次数。一次通路成立清零——服务商回的状态码不算代理的错 */
  failures: number;
  /** 剩余冷却毫秒。0 = 没在冷却 */
  coolingMs: number;
  /** 累计：拿到过响应头（通路成立）*/
  reached: number;
  /** 累计：一个头都没拿到（这才是代理的事）*/
  unreachable: number;
  /** 累计：头之后流被掐（服务商/中转站的空闲超时，不进冷却）*/
  interrupted: number;
  /** 响应头耗时 EWMA 毫秒。null = 没量过或已过 10 分钟新鲜期 */
  headMs: number | null;
  /** 首个 SSE 事件耗时毫秒。同上 */
  ttftMs: number | null;
}

/**
 * 发一条消息。只送这一句新输入，不再送整份历史——
 * 后端的历史就是它自己的话题日志，前端再送一份就成了第二个真相源
 */
export function sendChat(input: {
  message: string;
  attachments: string[];
  conversationId: string;
  /** 先把分支末端移到这条条目之后再发。空 message + rewindTo 就是"重新生成" */
  rewindTo?: string | null;
  /** 这一条不带记忆注入。一次性，不落到配置里 */
  skipMemory?: boolean;
  /** 决策层替模型池挑好的成员（integrations.pickPoolMember 的结果）。
   *  缺席 = 池子自己兜底，Rust 侧不会因为决策层没答话就不干活 */
  poolPick?: { profileId: string; model: string } | null;
  onEvent: (event: ChatEvent) => void;
}): Promise<void> {
  const channel = new Channel<ChatEvent>();
  channel.onmessage = input.onEvent;

  return invoke("chat_send", {
    input: input.message,
    attachments: input.attachments,
    conversationId: input.conversationId,
    rewindTo: input.rewindTo ?? null,
    rewindToRoot: input.rewindTo === null,
    skipMemory: input.skipMemory ?? false,
    poolPick: input.poolPick ?? null,
    onEvent: channel,
  });
}

export const chatAbort = (conversationId: string) =>
  invoke<void>("chat_abort", { conversationId });

/** 中转站探针：向当前（或点名）服务商连接发探测请求，收集注入/目标验证信号 */
export interface ProbeReport {
  /** 唯一 id：详情弹窗与单条删除的钥匙。旧历史行没有这格（空串） */
  id: string;
  depth: string;
  claimed: string;
  baseUrl: string;
  model: string;
  signals: ProbeSignal[];
  score: number;
  verdict: string;
  finishedAt: string;
}

export interface ProbeSignal {
  key: string;
  severity: "pass" | "warn" | "fail";
  confidence: number;
  evidence: string;
}

export const runProbe = (input: { claimed: string; depth: string; profileId?: string; model?: string }) =>
  invoke<ProbeReport>("probe_run", {
    claimed: input.claimed,
    depth: input.depth,
    profileId: input.profileId ?? null,
    model: input.model ?? null,
  });

export const fetchProbeHistory = () => invoke<ProbeReport[]>("probe_history");

export const deleteProbeHistory = (id: string) =>
  invoke<void>("probe_history_delete", { id });

/** 往正在运行的回合里插话。后端排队，下一轮请求前进入上下文 */
export const chatSteer = (conversationId: string, text: string) =>
  invoke<void>("chat_steer", { conversationId, text });

/** 往跟随队列排一句话：这一轮收尾后自动作为新输入开下一轮。返回排队后的长度 */
export const chatFollowUp = (conversationId: string, text: string) =>
  invoke<number>("chat_follow_up", { conversationId, text });

/** 从某条用户消息分叉出新话题：到那条为止的分支整体抄过去，返回新话题 id */
export const conversationFork = (conversationId: string, entryId: string) =>
  invoke<string>("conversation_fork", { conversationId, entryId });

/** 话题标题自动生成（首轮完成后调用一次，失败静默） */
export const generateTitle = (messages: ChatMessage[]) =>
  invoke<string>("generate_title", { messages });

/** 手动压缩：后端在自己的话题日志上压，这里只拿摘要正文回显 */
export const compactHistory = (conversationId: string) =>
  invoke<string>("compact_history", { conversationId });

/** 当前话题的缓存命中聚合（input 与其中命中的 cached） */
export const sessionCacheUsage = (conversationId: string) =>
  invoke<SessionCacheUsage>("usage_session_cache", { conversationId });

/** 按话题聚合的一页（page 从 0 起；page_size 后端夹在 1–100）。与汇总同一个时间窗 */
export const usageConversations = (days: number, page: number, pageSize: number) =>
  invoke<ConversationPage>("usage_conversations", { days, page, pageSize });

export const fetchContextBreakdown = () =>
  invoke<ContextBreakdown>("context_breakdown");

export const fetchConfig = () => invoke<AppConfig>("config_get");

/** config.json 的完整路径，设置页「打开配置文件」用 */
export const fetchConfigPath = () => invoke<string>("config_file_path");

/// 只提交要改的顶层字段。整份覆盖写会让另一页的过期快照把无关字段抹回旧值。
export const persistPatch = (patch: Partial<AppConfig>) =>
  invoke<AppConfig>("config_patch", { patch });

/** 拉模型列表。全部参数可缺省＝按当前生效配置拉；档案弹窗传草稿的 * 服务商/凭据目标乃至刚敲的密钥——草稿没落盘，不传就只会对着错误的服务商拉 */
export const fetchModels = (params?: {
  baseUrl?: string;
  apiFormat?: string;
  credentialService?: string;
  credentialUser?: string;
  secret?: string;
}) => invoke<string[]>("list_models", params ?? {});

export const probeCredential = () => invoke<boolean>("credential_probe");

export const writeCredential = (secret: string) => invoke<void>("credential_set", { secret });

export const decideTool = (id: string, approved: boolean) =>
  invoke<void>("tool_decision", { id, approved });

/** ask_user 的回答：按后端发来那条提问的 id 投一次，答案原文进工具结果 */
export const answerQuestion = (id: string, answer: string) =>
  invoke<boolean>("ask_user_respond", { id, answer });

/** 批准这一次，并把同一份动作在本话题内记住。
 *  记的是哪一份由后端按这条待批的 id 查——前端传不出它没见过的键 */
export const allowToolSession = (id: string) => invoke<boolean>("tool_allow_session", { id });

/** 「以后都允许」：批准这一次，并把那份动作写进配置，重启后同一份动作不再问。
 *  撤销走工具页那条列表（它现在管着话题内与持久两层） */
export const allowToolAlways = (id: string) => invoke<boolean>("tool_allow_always", { id });

/** 现在生效的放行规则（话题内点过的 + 「以后都允许」落过盘的，启动时已并成一张表） */
export interface RememberedRule {
  key: string;
  label: string;
}
export const fetchToolRules = () => invoke<RememberedRule[]>("tool_rules");
/** 撤销一条。持久的那条会连配置一起摘 */
export const forgetToolRule = (key: string) => invoke<boolean>("tool_rule_forget", { key });
/** 撤销全部（含持久的）。返回清掉了几条 */
export const clearToolRules = () => invoke<number>("tool_rules_clear");

/** 全局快捷键（Ctrl+Shift+G 唤起窗口）的开关。注册失败会报错且不落盘 */
export const setGlobalShortcut = (enabled: boolean) =>
  invoke<void>("global_shortcut_set", { enabled });

/** 命令沙箱的开关（三层：收容壳 + WRITE_RESTRICTED 令牌 + 低完整性）。
 *  启用前先把可写根就位（标签 + 授权），失败报错且不落盘 */
export const setAutoReview = (enabled: boolean) =>
  invoke<void>("auto_review_set", { enabled });

export const setSandbox = (enabled: boolean) =>
  invoke<void>("sandbox_set", { enabled });

// ---- 安全中心（design-security-center.md D1-D9）----

/** 生图/视频会话的生成调用（独立 REST 管线）：返回落盘的产物信息 */
export const mediaGenerate = (
  kind: string,
  prompt: string,
  options: {
    size?: string;
    quality?: string;
    count?: number;
    mode?: string;
    resolution?: string;
    duration?: number;
    ratio?: string;
    lyrics?: string;
    instrumental?: boolean;
  },
  referenceImages: string[] = [],
  model?: string,
  /** 视频编辑模式的素材视频（本地路径，Rust 侧转 base64 data URL 进 content） */
  videoReference?: string,
  /** 模型所属的服务商档案：media 请求按它路由连接域（各类型模型可住不同站点） */
  profileId?: string,
) =>
  invoke<{
    images?: Array<{ path: string; name: string; bytes: number; index: number }>;
    path?: string;
    name?: string;
    bytes?: number;
    /** 文本生成的产物：一次进一句出，落在节点的文本气泡里 */
    text?: string;
  }>("media_generate", {
    kind,
    prompt,
    options,
    referenceImages,
    model,
    videoReference,
    profileId,
  });

/** 删除保护：存盘 + 落执行侧开关一次做完（delete_file 的回收站/硬删） */
export const setDeleteToTrash = (toTrash: boolean) =>
  invoke<void>("delete_to_trash_set", { toTrash });

/** 敏感保护：工具结果进话题流前就地打码（含关闭清单、自建规则与内置正则改写） */
export const setSecretScan = (
  enabled: boolean,
  disabledRules: string[],
  customRules: CustomSecretRule[],
  patternEdits: SecretRulePatternEdit[],
) =>
  invoke<void>("secret_scan_set", {
    enabled,
    disabledRules,
    customRules,
    patternEdits,
  });

/** 打开备份目录（不存在就先建） */
export const openBackupDir = () => invoke<string>("backup_open_dir");

/** 导出审计日志：把 from..=to（含）日期段的活动分片拼成一份 JSONL */
export const auditExport = (from: string, to: string) =>
  invoke<string>("audit_export", { from, to });

/** 清空审计活动分片：先归档再清，返回归档文件路径（清空不销毁） */
export const auditClear = () => invoke<string[]>("audit_clear");

/** 敏感检测规则库（id/名称/类别），设置页按它渲染清单 */
export const fetchSecretRules = () =>
  invoke<Array<{ id: string; label: string; kind: string; hintGated: boolean; pattern: string }>>(
    "secret_rules_list",
  );

/** 沙箱额外可写根（writable_roots）：逐个验证并标注，失败整体报错不落盘 */
export const setSandboxRoots = (roots: string[]) =>
  invoke<void>("sandbox_set_roots", { roots });

// ---- MCP OAuth（http 型远程服务器）----

export interface McpOAuthOutcome {
  server: string;
  scopes: string[];
  clientId: string;
}

export interface McpOAuthStatus {
  loggedIn: boolean;
  expiresAtMs: number | null;
}

/** 浏览器 PKCE 登录（发现 → 动态注册 → 授权码 → 令牌落 keyring）。等待期间别关窗口 */
export const mcpOauthLogin = (id: string) =>
  invoke<McpOAuthOutcome>("mcp_oauth_login", { id });

export const mcpOauthStatus = (id: string) =>
  invoke<McpOAuthStatus>("mcp_oauth_status", { id });

export const mcpOauthLogout = (id: string) => invoke<boolean>("mcp_oauth_logout", { id });

// ---- 话题全文搜索 ----

export interface SearchHit {
  conversationId: string;
  title: string;
  messageId: string;
  role: string;
  snippet: string;
}

/** 全文搜索（LIKE 子串、大小写不敏感、中文友好）。≥2 字符才值得发 */
export const sessionSearch = (query: string, limit?: number) =>
  invoke<SearchHit[]>("session_search", { query, limit: limit ?? 60 });

export interface SearchIndexStatusView {
  conversations: number;
  stale: number;
}

export const sessionSearchStatus = () => invoke<SearchIndexStatusView>("session_search_status", {});

/** 全量重建索引（换后端/索引落后时用）。返回重建的话题数 */
export const sessionSearchRebuild = () => invoke<number>("session_search_rebuild", {});

export const readAttachment = (path: string) =>
  invoke<Omit<Attachment, "id"> & { text: string }>("read_attachment", { path });

/** asset 协议按需放行：scope 已收窄到固定目录，用户自选路径的附件/预览
 *  在渲染前经这里逐个交给后端 allow（失败静默——顶多图挂不了，不该炸会话） */
export const assetAllow = (paths: string[]) =>
  invoke<void>("asset_allow", { paths });

/** slash 命令菜单的一份：内置动作 + 个人/项目/插件 commands 目录里的自定义命令。
 *  模板展开发生在前端发送那一刻（$ARGUMENTS / $1..$9），语义由 lib/slash 的测试钉住 */
export interface SlashCommand {
  name: string;
  title: string;
  argumentHint: string | null;
  /** builtin / user / project / 插件 id */
  source: string;
  /** 内置动作 id；null = 自定义命令 */
  action: string | null;
  /** 发送时展开的提示词（内置 /init 与自定义正文） */
  prompt: string | null;
}
export const slashCommandsList = () => invoke<SlashCommand[]>("slash_commands_list");

/** @-提及的文件候选（按名字在活动工作目录里找） */
export interface FileSuggest {
  /** 项目相对路径（正斜杠），也是插进草稿的 @ 记号 */
  rel: string;
  abs: string;
  isDir: boolean;
}
export const filesSuggest = (query: string) =>
  invoke<FileSuggest[]>("files_suggest", { query });

/** 设置页「内置子助理」名册：出厂定义合并覆盖后的完整视图（含停用的） */
export const builtinSubagentsList = () =>
  invoke<BuiltinSubagentView[]>("builtin_subagents_list");

/** 粘贴的剪贴板位图落进临时目录（Win+Shift+S 的截图没有文件路径，这里给它一个）。
 *  返回的 path 之后就是一张普通的路径附件；width/height 只对 PNG 有（IHDR 固定偏移可读） */
export interface SavedClipboardImage {
  path: string;
  name: string;
  mime: string;
  bytes: number;
  width: number | null;
  height: number | null;
}
export const saveClipboardImage = (dataBase64: string, mime: string) =>
  invoke<SavedClipboardImage>("save_clipboard_image", { dataBase64, mime });

/** 抓取网页可读正文：出站过出口名单，正文落盘成 txt 后按普通附件走链路 */
export interface FetchedLinkText {
  path: string;
  name: string;
  url: string;
  title: string | null;
  chars: number;
  truncated: boolean;
}
export const fetchUrlText = (url: string) => invoke<FetchedLinkText>("fetch_url_text", { url });

export const addProject = (name: string, path: string) =>
  invoke<AppConfig>("project_add", { name, path });

export const selectProject = (id: string) => invoke<AppConfig>("project_select", { id });

export const removeProject = (id: string) => invoke<AppConfig>("project_remove", { id });

export const historyList = () => invoke<ConversationMeta[]>("history_list");

export const historyLoad = (id: string) => invoke<ConversationRecord>("history_load", { id });

export const historySave = (conversation: ConversationRecord) =>
  invoke<ConversationMeta>("history_save", { conversation });

export const historyRemove = (id: string) => invoke<void>("history_remove", { id });

export const fetchStorageInfo = () => invoke<StorageInfo>("storage_info");

export const requestStorageSwitch = (backend: ConversationStore) =>
  invoke<StorageSwitch>("storage_switch", { backend });

export const builtinToolsList = () => invoke<BuiltinTool[]>("builtin_tools_list");

/** 经某条代理试连活动服务商：任何 HTTP 状态码都算通（401 也是路通了）。这一次真尝试进账本 */
export const proxyTest = (proxyId: string) => invoke<string>("proxy_test", { proxyId });

/** 代理池此刻的调度读数：谁在跑、谁在冷却、每条多快 */
export const fetchProxyPoolStats = () => invoke<ProxyStat[]>("proxy_pool_stats");

/** proxy_import 的一行：解析出来的地址与名字（reason 非空 = 这一行被挡下，没让它进池） */
export interface ProxyImportRow {
  url: string;
  name: string;
  /** null = 合格。命令只答"这些行会怎么样"，落盘仍走 config_patch 那一个写入口 */
  reason: string | null;
}

/** 批量粘贴导入的解析与去重（形状尺与 Rust parse_proxy_url 同一把） */
export const proxyImport = (text: string) => invoke<ProxyImportRow[]>("proxy_import", { text });

/** proxy_pool_test_all 的一条：经这条代理真探一次的结局（结果进账本，测通即解冷却） */
export interface ProxyTestOutcome {
  id: string;
  name: string;
  ok: boolean;
  ms: number;
  note: string;
}

/** 把池里每条启用的代理各测一次（批内并发） */
export const proxyPoolTestAll = () => invoke<ProxyTestOutcome[]>("proxy_pool_test_all");

/** 话题日志里的一条节点（只读投影，界面上没有任何东西能写回它） */
export interface ConversationNode {
  id: string;
  parentId: string | null;
  seq: number;
  /** message / compaction / model_change / usage… */
  kind: string;
  /** 只有承载消息的行有角色 */
  role: string | null;
  /** 消息行的正文预览（前 200 字符）。重试的兜底路径用它把问题对回日志行 */
  preview: string | null;
  at: number;
  /** 在不在当前分支上 */
  onPath: boolean;
}

/** 话题树：整棵树 + 当前分支末端。前端那份 parentId 是从这里抄的，不是自己编的 */
export interface ConversationTree {
  tip: string | null;
  nodes: ConversationNode[];
}

export const fetchConversationTree = (conversationId: string) =>
  invoke<ConversationTree>("conversation_tree", { conversationId });

/** 只移动分支末端，不发任何请求。entryId 为 null = 退到根之前 */
export const conversationNavigate = (conversationId: string, entryId: string | null) =>
  invoke<string | null>("conversation_navigate", { conversationId, entryId });

/** 读这一支现在的作业模式。读数由后端从话题日志算出来，前端不存第二份 */
export const fetchModeState = (conversationId: string) =>
  invoke<ModeState>("session_mode_get", { conversationId });

/**
 * 作业模式那三条会改日志的命令（切档 / 暂停 / 结束目标）共用的读数包。
 * `deferred` = 回合还在跑：请求刚立进登记表，日志里那一行要等这一轮收尾才落。
 * 界面据此说一句"这一轮收尾后生效"——"点了没反应"与"点了、正在等"必须长得不一样
 */
export interface ModeOutcome {
  view: ModeState;
  deferred: boolean;
}

/**
 * 切作业模式，返回切完之后的读数——界面信这一次返回，不自己拼一份状态出来。
 * `maxCostUsd` 用美元字符串（与价表同一口径，钱不用 float 过 IPC）：留空就是不设上限。
 * 目标没有轮次上限，这一格因此是它唯一的自动刹车。
 * `profile` 是目标点名执行的服务商档案 id：目标期间（含用户在对话档插话的那些轮）整份
 * 连接域照那张档案走，池子与路由都不抢。null/缺省 = 跟随当前配置
 */
/**
 * 切交互档（对话 / 规划）。目标不在这条命令上——一格命令改一件事，
 * 定目标走 `sessionGoalSet`（design-goal-mode.md §3.2）
 */
export const setModeState = (input: { conversationId: string; mode: WorkingMode }) =>
  invoke<ModeOutcome>("session_mode_set", {
    conversationId: input.conversationId,
    mode: input.mode,
  });

/** 弹框里一条判据的形状。id 由后端补铸，人在弹框里写的是内容不是名字 */
export type CriterionInput = {
  text: string;
  kind: "check" | "judgment";
  command?: string;
};

/**
 * 定目标：立一份完成契约并立刻开第一轮。已有一支在推进的目标且写的不是同一句时
 * 要 `force`（弹框先确认）；回合在跑时后端寄存、轮次边界落行
 */
export const sessionGoalSet = (input: {
  conversationId: string;
  objective: string;
  criteria: CriterionInput[];
  constraints: string[];
  maxCostUsd?: string | null;
  profile?: string | null;
  force: boolean;
}) =>
  invoke<ModeOutcome>("session_goal_set", {
    conversationId: input.conversationId,
    objective: input.objective,
    criteria: input.criteria,
    constraints: input.constraints,
    maxCostUsd: input.maxCostUsd ?? null,
    profile: input.profile ?? null,
    force: input.force,
  });

/** 编辑目标：同一支换文字，账全留。判据文本改过的那些条，证据自动作废 */
export const sessionGoalEdit = (input: {
  conversationId: string;
  objective?: string | null;
  criteria?: CriterionInput[] | null;
  constraints?: string[] | null;
}) =>
  invoke<ModeOutcome>("session_goal_edit", {
    conversationId: input.conversationId,
    objective: input.objective ?? null,
    criteria: input.criteria ?? null,
    constraints: input.constraints ?? null,
  });

/** 契约草案：让模型补全判据与约束。产出是草案，采纳才立目标 */
export const goalCriteriaDraft = (source: string) =>
  invoke<ContractView>("goal_criteria_draft", { source });

/** 一条判据命令的风险档（tools::classify 是唯一出处）。弹框的契约自检读它 */
export const commandRisk = (conversationId: string, command: string) =>
  invoke<"safe" | "elevated" | "high">("command_risk", { conversationId, command });
/**
 * 暂停 / 恢复一个目标（`paused = true` 暂停）。回合中暂停由收尾判据落行，
 * 空闲时直接写行；恢复只翻旗子，目标、轮数、已花的钱原样跟着走
 */
export const goalPauseState = (conversationId: string, paused: boolean) =>
  invoke<ModeOutcome>("session_goal_pause", { conversationId, paused });

/** 恢复一个目标并立刻接一轮：续跑两行由后端落，事件走 chat-event 广播 */
export const goalResumeRun = (conversationId: string) =>
  invoke<ModeState>("session_goal_resume", { conversationId });

/** 整机还在跑的后台命令（带主人）。卡片按话题分桶清点 */
export const backgroundCommandsList = () =>
  invoke<RunningCommand[]>("background_commands_list");

/** 结束目标：整份清掉，当前交互档保持不变（目标档下回对话档）。回合中按则寄存，
 *  由那一轮收尾时落行——"结束目标"不该要人先按一次停止再按一次结束 */
export const goalDiscardState = (conversationId: string) =>
  invoke<ModeOutcome>("session_goal_discard", { conversationId });

/**
 * 启动投影里的一条（D8）。它是**投影不是真相**：原文在各条话题的日志里，
 * 这一份随时可删可重建
 */
export type GoalSummary = {
  conversationId: string;
  title: string;
  /** 那条话题当下的交互档 */
  mode: WorkingMode;
  objective: string;
  /** 收尾或停住那一句：面板要读得出为什么停 */
  note: string | null;
  status: GoalStatus;
  turnsUsed: number;
  maxCostUsdE8: number;
  /** null = 台账读不出来，界面上是 `—` */
  spentUsdE8: number | null;
  profile: string | null;
  /** 目标的身份：角落卡把分叉出的同一支合成一组，认的就是它 */
  goalId: string | null;
  /** 契约的界面投影：读数形状与目标带是同一份 */
  contract: ContractView | null;
  /** 这一条是被这次启动从推进中改成已暂停的：界面拿它决定要不要说那一句 */
  parkedByRestart: boolean;
};

/**
 * 扫一遍各条话题，把还挂着账的目标列出来。
 * 默认顺手把推进中的落成已暂停并落一行——重启不该替人重按播放键（设置里可开回自动继续）
 */
export const goalsOverview = () => invoke<GoalSummary[]>("goals_overview");

export const skillsList = () => invoke<SkillsListing>("skills_list");

export const pluginsList = () => invoke<PluginsListing>("plugins_list");

export const pluginMarketList = () => invoke<MarketView>("plugin_market_list");

export const pluginMarketInstall = (id: string, downloadUrl: string, sha256: string) =>
  invoke<string>("plugin_market_install", { id, downloadUrl, sha256 });

export const browserClearCache = () => invoke<string>("browser_clear_cache");

export const browserClearAll = () => invoke<string>("browser_clear_all");

export const mcpList = () => invoke<McpServerView[]>("mcp_list");

// ---- 官方订阅登录（OAuth）：浏览器授权 → 本地回调 → 令牌写 keyring ----

export interface OAuthProviderInfo {
  id: string;
  label: string;
  hint: string;
  baseUrl: string;
  apiFormat: string;
  /** 常用模型提示（逗号分隔），给模型表空着的档案垫提示 */
  modelsHint: string;
}

export interface OAuthLoginOutcome {
  baseUrl: string;
  apiFormat: string;
  modelsHint: string;
  credentialService: string;
  credentialUser: string;
  /** 非空时界面要转述的提示：点名的槽位已有密钥，本次令牌改落了专属槽位 */
  notice?: string;
}

export const oauthProviders = () => invoke<OAuthProviderInfo[]>("oauth_providers");

export const oauthLogin = (
  providerId: string,
  credentialService?: string,
  credentialUser?: string,
) =>
  invoke<OAuthLoginOutcome>("oauth_login", {
    providerId,
    credentialService: credentialService || null,
    credentialUser: credentialUser || null,
  });

/** 设备码流程第一步的产出：userCode 亮给用户抄，deviceCode 留给轮询命令 */
export interface OAuthDeviceStart {
  userCode: string;
  verificationUri: string;
  deviceCode: string;
  interval: number;
}

export const oauthDeviceStart = (providerId: string) =>
  invoke<OAuthDeviceStart>("oauth_device_start", { providerId });

/** 长轮询：后端自己循环到授权完成/拒绝/超时（最长 15 分钟），前端 await 一次即可 */
export const oauthDevicePoll = (providerId: string, deviceCode: string, interval?: number) =>
  invoke<OAuthLoginOutcome>("oauth_device_poll", {
    providerId,
    deviceCode,
    interval: interval ?? null,
  });

/** 增强提示词：把输入框草稿交给当前生效的模型改写，返回改写后的文本本身 */
export const enhancePrompt = (text: string) => invoke<string>("enhance_prompt", { text });

// ---- MCP 市场（官方注册表 registry.modelcontextprotocol.io）----

export interface RegistryCredential {
  name: string;
  description?: string;
  isRequired?: boolean;
  /** 目录标记这格该放凭据；值不会从市场带过来，要用户自己填 */
  isSecret?: boolean;
}

export interface RegistryRemote {
  transportType: string;
  url: string;
  headers: RegistryCredential[];
}

export interface RegistryPackage {
  registryType: string;
  identifier: string;
  runtimeHint?: string;
  /** runtimeArguments 里的 positional 值，排在包名前面 */
  args: string[];
  env: RegistryCredential[];
}

export interface RegistryEntry {
  name: string;
  title?: string;
  description: string;
  version: string;
  repository?: string | null;
  remotes: RegistryRemote[];
  package: RegistryPackage | null;
}

export interface RegistryPage {
  entries: RegistryEntry[];
  nextCursor: string | null;
}

/** 市场搜索：Rust 侧代理请求（WebView 直连有 CORS），分页用 nextCursor 续拉 */
export const registrySearch = (search?: string, cursor?: string | null) =>
  invoke<RegistryPage>("registry_search", {
    search: search || null,
    cursor: cursor ?? null,
    limit: 20,
  });

// ---- 技能商店（SkillHub，skillhub.cn）----
// SKILL.md 本体不在其公开接口里，市场只做浏览与跳转：安装走详情页或上游仓库

export interface SkillhubEntry {
  name: string;
  slug: string;
  /** `@作者/slug` 的规范名 */
  handle: string;
  pageUrl: string;
  description: string;
  version: string;
  category: string;
  downloads: number;
  stars: number;
  verified: boolean;
  upstreamUrl?: string | null;
}

export interface SkillhubPage {
  entries: SkillhubEntry[];
  total: number;
  page: number;
}

export type SkillhubSortBy = "score" | "downloads" | "trending" | "updated";

export const skillhubSearch = (
  keyword?: string,
  sortBy: SkillhubSortBy = "score",
  page = 1,
) =>
  invoke<SkillhubPage>("skillhub_search", {
    keyword: keyword || null,
    sortBy,
    page,
  });

/** 一键安装：Rust 下载 zip（302 跟到对象存储）→ 解压进个人技能目录。
 *  返回的 dir 是安装位置，files/bytes 用来拼成功提示 */
export interface SkillhubInstallReport {
  dir: string;
  files: number;
  bytes: number;
}

export const skillhubInstall = (slug: string) =>
  invoke<SkillhubInstallReport>("skillhub_install", { slug });

// ---- 自定义 GitHub 仓库的技能识别与安装 ----

export interface GithubCandidate {
  /** 仓库内相对目录；根候选是空串 */
  path: string;
  name: string;
  description: string;
}

export interface GithubProbe {
  owner: string;
  repo: string;
  branch: string;
  candidates: GithubCandidate[];
}

/** 识别：树里找所有带 SKILL.md 的目录，frontmatter 里读名字与描述 */
export const githubSkillProbe = (url: string) =>
  invoke<GithubProbe>("github_skill_probe", { url });

/** 安装选中的候选目录（SKILL.md + 它的附属文件）进个人技能目录 */
export const githubSkillInstall = (params: {
  url: string;
  path: string;
  branch: string;
  name: string;
}) => invoke<SkillhubInstallReport>("github_skill_install", params);

export const mcpConnect = (id: string) => invoke<void>("mcp_connect", { id });

export const mcpStop = (id: string) => invoke<void>("mcp_stop", { id });
/** 刷新某台服务器的工具清单。它只能靠**重开这条连接**做到（清单住在握手那一份里），
 *  正在跑的调用会被打断，所以 `confirm` 必须显式给 true——后端那道闸不认缺省值 */
export const mcpRefresh = (id: string, confirm: boolean) =>
  invoke<void>("mcp_refresh", { id, confirm });

export const fetchReview = () => invoke<ReviewInfo>("review_info");

/** 单个文件的差异。scope 只有两个合法值，越界的 path 由后端拒 */
export const fetchFileDiff = (path: string, scope: DiffScope) =>
  invoke<FileDiff>("review_file_diff", { path, scope });

export const draftReview = () => invoke<string>("review_draft");

export const saveReview = (markdown: string) => invoke<string>("review_save", { markdown });

// ---- 编辑台账：aglab 内置 write_file 改过哪些文件 ----

/** 本次话题按文件聚合的编辑。只统计内置写入，MCP 工具写的文件不在其中 */
export const fetchSessionEdits = (conversationId: string) =>
  invoke<FileEdit[]>("edits_for_session", { conversationId });

/** 面板内预览正文。路径必须是本次话题编辑清单里的绝对路径 */
export const fetchEditPreview = (conversationId: string, path: string) =>
  invoke<EditPreview>("edit_preview", { conversationId, path });

/** 恢复到本次话题第一次被写之前的样子。漂移时后端会拒 */
export const revertEdit = (conversationId: string, path: string) =>
  invoke<RevertOutcome>("edit_revert", { conversationId, path });

/** 回滚的三档范围：file = 单文件；turn = 这几笔工具调用动过的所有文件；conversation = 整条话题 */
export type RewindScope =
  | { kind: "file"; path: string }
  | { kind: "turn"; callIds: string[] }
  | { kind: "conversation" };

export interface RewindOutcome {
  reverted: string[];
  skipped: { path: string; reason: string }[];
}

export const rewindEdits = (conversationId: string, scope: RewindScope) =>
  invoke<RewindOutcome>("edit_rewind", { conversationId, scope });

export const tasksList = () => invoke<TaskView[]>("tasks_list");

export const taskRunNow = (id: string) => invoke<void>("tasks_run", { id });

/** 账本里的运行记录，含每一格。`taskId` 给了就只列那个任务的 */
export const tasksRunList = (taskId?: string) =>
  invoke<TaskRun[]>("tasks_runs_list", { taskId });

/** 从检查点续跑：只跑账本没记成"跑成了"的那些格，已完成的一格不会再花一次钱 */
export const taskRunResume = (runId: string) => invoke<void>("tasks_run_resume", { runId });

/** 清掉 N 天前已经了结的运行记录，返回抹掉了几发。没跑完的那些与每个任务最新那一发
 *  不动——后者是调度器算欠账的锚点，抽掉它会补跑出一批真花钱的运行 */
export const tasksRunsPurge = (days: number) => invoke<number>("tasks_runs_purge", { days });

/** 挂在 durable 队列里的待审批：没人可问时，后台 run 停在这一发上，不超时也不放行 */
export const tasksPendingApprovals = () =>
  invoke<TaskApproval[]>("tasks_pending_approvals");

/** 对一条待批表台。决定只写进队列，执行永远在动作自己的那条路上 */
export const tasksApprovalDecide = (id: string, approved: boolean) =>
  invoke<TaskApproval>("tasks_approval_decide", { id, approved });

/** 以前表过态的那些：每一条都在替它盖过的那一发说话（同一份参数以后不再问），
 *  所以在它们被撤回之前，这一列必须看得见 */
export const tasksApprovalHistory = () => invoke<TaskApproval[]>("tasks_approval_history");

/** 撤回一次表态，返回撤掉了几条先例。撤的是"以后不用再问"，不是已经执行过的那个动作 */
export const tasksApprovalForget = (id: string) => invoke<number>("tasks_approval_forget", { id });

/** 按配置的天数归档旧审计分片，返回搬走了几片。只搬不删，0 是常态（没有到期的） */
export const auditRotate = () => invoke<number>("audit_rotate");

/** 审计那一行里的"谁干的"与"结果如何"。取值集合由 Rust 那两个枚举定，
 *  下面那两张标签表覆盖它们的每一个成员——少一个键，界面上剩下的就是一个英文原词，
 *  而"没翻译"在一整片中文里看着就像"这一类不会发生" */
export type AuditActor = "user" | "model" | "scheduler" | "reflection" | "import";
export type AuditOutcome = "ok" | "denied" | "failed" | "blocked";

export const AUDIT_ACTOR_LABELS: Record<AuditActor, string> = {
  user: "人点的",
  model: "模型",
  scheduler: "定时任务/编排",
  reflection: "反思",
  import: "导入",
};

export const AUDIT_OUTCOME_LABELS: Record<AuditOutcome, string> = {
  ok: "做了",
  denied: "被拒",
  failed: "失败了",
  blocked: "被策略挡住",
};

export interface AuditEntry {
  at: string;
  actor: AuditActor;
  action: string;
  target: string;
  outcome: AuditOutcome;
  detail?: string;
}

export interface AuditPage {
  date: string;
  entries: AuditEntry[];
  /** 读不懂的行数。跳过坏行是必须的（分片是并发追加的），静默跳过不是 */
  skipped: number;
  /** 这一页只是最新的那一截，更早的没送来 */
  truncated: boolean;
}

/** 读某一天的审计。日期留空 = 今天 */
export const auditView = (date?: string) =>
  invoke<AuditPage>("audit_view", { date: date?.trim() || null });

// ---- cc-switch 迁移（只读对方库存，密钥永远不跨 IPC） ----

export const ccswitchCandidates = () => invoke<CcswitchCandidate[]>("ccswitch_candidates");

export const ccswitchImportProvider = (sourceId: string) =>
  invoke<AppConfig>("ccswitch_import_provider", { sourceId });

export const ccswitchImportPricing = () => invoke<number>("ccswitch_import_pricing");

export const ccswitchMcpCandidates = () => invoke<McpCandidate[]>("ccswitch_mcp_candidates");

export const ccswitchImportMcp = (ids: string[]) =>
  invoke<McpImportResult>("ccswitch_import_mcp", { ids });

// ---- 用量台账与价格表 ----

export const fetchUsageReport = (days: number) => invoke<UsageReport>("usage_report", { days });

export const pricingList = () => invoke<ModelPrice[]>("pricing_list");

export const pricingUpsert = (price: ModelPrice) =>
  invoke<ModelPrice[]>("pricing_upsert", { price });

export const pricingRemove = (modelId: string) =>
  invoke<ModelPrice[]>("pricing_remove", { modelId });

// ---- P1：技能导入 / 请求明细 / CSV 导出 ----

export const ccswitchSkillCandidates = () =>
  invoke<SkillCandidate[]>("ccswitch_skill_candidates");

export const ccswitchImportSkills = (ids: string[]) =>
  invoke<SkillImportResult>("ccswitch_import_skills", { ids });

export const usageRecent = (days: number, offset: number, limit: number) =>
  invoke<UsageRequestPage>("usage_recent", { days, offset, limit });

export const usageExportCsv = (path: string, content: string) =>
  invoke<void>("usage_export_csv", { path, content });

// ---- 从其他 AI 应用导入项目与话题 ----

export const aiImportScan = () => invoke<AiImportSource[]>("import_scan");

export const aiImportFrom = (kind: string) =>
  invoke<AiImportOutcome>("import_from_app", { kind });
