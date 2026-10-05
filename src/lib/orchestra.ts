import { invoke } from "@tauri-apps/api/core";

/** 编排面板读的这几种类型住在这一份文件里，与 `lib/memory.ts` 同一条先例 */

export type NodeStatus =
  | "pending"
  | "running"
  | "done"
  | "failed"
  | "skipped"
  | "waitingApproval"
  | "blocked"
  | "canceled"
  /** 循环边：这一轮跑完了，循环还开着。它不是落定状态，节点还会再被派一次 */
  | "iterated";

export interface NodeView {
  id: string;
  profile: string;
  dependsOn: string[];
  depth: number;
  status: NodeStatus;
  boardVersion: number;
  /** 这一格到现在为止的读数，全从账本行推（重跑与循环各算一行）。
   *  验收那句"每个 agent 的实时输出、成本、耗时"里，成本与耗时以前无处可取 */
  tokens: number;
  durationMs: number;
  /** 1e-8 美元 */
  costE8: number;
  /** 台账算得出钱吗。false = 这个模型没价表，不等于"免费" */
  priced: boolean;
  /** 这一格动过几个文件（多次尝试去重后合起来） */
  filesTouched: number;
  /** 那些改动里留着快照，变更请求页那边有的可回。
   *  真的能不能回是点下去那一刻按文件现在的字节判的，这一格不冒充那个判定 */
  snapshotted: boolean;
  /** 这一格最新那一发自己的话题。回滚住在变更请求页那一侧，而它只认话题 id——
   *  没有这一格，上面那句"有快照可回"就点不动。null = 这一发还没跑过（或账上没记） */
  conversationId: string | null;
  /** 这一格是**降级收下**的那一份：有产出但没过校验，值就是没过的理由。
   *  状态那一格只有"完成"，分不出干净与将就的两份，所以这一格单独带着。
   *  null = 最后那一发过了校验（或还没跑） */
  degraded: string | null;
  /** 这一格**凭什么被放行**的一句人话，从图上现读："跑完就放行"、
   *  "等「write#verdict」＝fail"、"反复跑直到「a#verdict」＝pass，最多 3 轮"。
   *  上面那排选择按下去要看得见，不然它就是一个没有读数的旋钮 */
  gate: string | null;
}

/** 跨 plan 抢并发位的那一档。"让路"和"抢在前面"都是用户显式说过的话 */
export type PlanPriority = "background" | "normal" | "foreground";

export const PRIORITY_LABELS: Record<PlanPriority, string> = {
  background: "后台",
  normal: "常规",
  foreground: "前台",
};

export const PRIORITY_HINTS: Record<PlanPriority, string> = {
  background: "别人在跑时让它等，最多占全局的一半",
  normal: "默认。按全局上限排，不特别让也不特别抢",
  foreground: "可以占满全局池，但抢不走已经在跑的那一手",
};

export interface PlanView {
  planId: string;
  goal: string;
  nodes: NodeView[];
  criticalPath: string[];
  spentTokens: number;
  spentDurationMs: number;
  /** 这份计划到现在花了多少，单位 1e-8 美元。钱不用 float 过 IPC：
   * 一分一分加的东西换成 float，界面上那个数就会与价表算出来的对不上 */
  spentCostE8: number;
  waiting: [string, string][];
  merged: string | null;
  blockedBy: string | null;
  paused: boolean;
  canceled: boolean;
  finished: boolean;
  inFlight: number;
  maxParallel: number;
  /** 现在占着几个 worker 租约。它与 inFlight 分开：并发位和线程不是一回事 */
  workers: number;
  /** 池子这一刻愿意开几路。被失败砍过它就小于 workerCeiling */
  workerCap: number;
  /** 池子的天花板（= maxParallel）。报"缩过没有"要有对照数 */
  workerCeiling: number;
  /** 黑板上被 CAS 拒过几次：冲突双留之后必须有个地方说得清 */
  conflicts: number;
  /** 这一份计划抢全局位时的那一档 */
  priority: PlanPriority;
  /** 全局池一共几格（设置里那个数）。它和 maxParallel 是两个问题：
   * 一个是"这台机器上同时几路"，一个是"这份计划里同时几路"。0 = 不设上限 */
  quotaTotal: number;
  /** 这一档最多能占几格。不限池时没有份额这回事（后端报 0，界面按"不限"读） */
  quotaShare: number;
  /** 此刻全局占了几格（所有档加起来） */
  quotaUsed: number;
}

export interface TraceRow {
  tsMs: number;
  planId: string;
  node: string;
  attempt: number;
  event: string;
  status?: NodeStatus;
  tokens?: number;
  /** 只有终结行带：这一发的墙钟与花费（1e-8 美元）。没价表时 costE8 缺席，
   *  那与"花了 0"是两件事 */
  durationMs?: number;
  costE8?: number;
  conversationId?: string;
  detail?: string;
}

/** 账本里的事件词。认不出的照原样显示——账本多出一个事件，不该让界面少看一行 */
export const EVENT_LABELS: Record<string, string> = {
  queued: "排队",
  planned: "装配这张图",
  started: "开跑",
  finished: "落定",
  failed: "失败",
  escalated: "转成待批",
  blocked: "被策略挡住",
  merged: "已汇合",
  replan: "追加步骤",
  bus: "总线回报",
  conflict: "结论被顶回来",
  iterated: "这一轮跑完，还要再来",
  quota: "等全局并发位",
  scaled: "扩缩",
  stolen: "窃取派发",
  canceled: "已取消",
  skipped: "没轮到",
};

export const event_label = (event: string): string => EVENT_LABELS[event] ?? event;

export type PlanShape =
  | "fanout"
  | "pipeline"
  | "bestOf"
  | "debate"
  | "hierarchical"
  | "mapReduce";

export const SHAPE_LABELS: Record<PlanShape, string> = {
  fanout: "扇出汇聚",
  pipeline: "流水线",
  bestOf: "竞争 N 份",
  debate: "辩论",
  hierarchical: "层级派发",
  mapReduce: "逐项归并",
};

/** 一份计划跑起来的中文状态词。界面不许出现英文枚举 */
export const STATUS_LABELS: Record<NodeStatus, string> = {
  pending: "排队中",
  running: "在跑",
  done: "完成",
  failed: "失败",
  skipped: "没轮到",
  waitingApproval: "等你批准",
  blocked: "被策略挡住",
  canceled: "已取消",
  iterated: "这一轮跑完，还要再来",
};

/** 发起一次编排的请求。命名而不是内联，是为了 Rust 那侧的形状守卫能比到它——
 *  这个方向以前没人比：请求里打错一个键，serde 会安静地按默认值补，
 *  而"每格花费上限"的默认值是**不设**，也就是安静地多花钱 */
export interface StartCheck {
  minChars: number;
  mustContain: string[];
  forbid: string[];
}

export interface StartRequest {
  goal: string;
  shape: PlanShape;
  branches: number;
  maxParallel: number;
  /** mapReduce 那一批要逐个处理的集合项；其他形状忽略它 */
  items: string[];
  /** 跨 plan 抢并发位的那一档 */
  priority: PlanPriority;
  /** **每一格**自己的花费上限，单位是微元（1e-6 美元）。0 = 不设 */
  nodeCostMicros: number;
  /** 每一格产出的形状检查。三项全空 = 不设（今天的读法就是非空即可） */
  check: StartCheck;
}

export const orchestraStart = (request: StartRequest) =>
  invoke<string>("orchestra_start", { request });

/** 开跑之前那张图的规模。三个数全部由后端从**真构造器**算出来：面板以前自己写
 *  `分支 × 2 + 1`，那等于把辩论的形状在另一端抄了一份，构造器改形的那天这句话会说谎，
 *  而读的正是"要不要花这笔钱"的那个人 */
export interface BriefRequest {
  shape: PlanShape;
  branches: number;
  items: string[];
}

export interface PlanBrief {
  /** 装配出来几格 */
  nodes: number;
  /** 每格都一次跑成，是几发请求 */
  minRequests: number;
  /** 每格都把尝试次数用满，是几发请求。跑起来之后改边、人工重跑、层级补的第二批都不算在里面 */
  maxRequests: number;
}

export const orchestraPlanBrief = (request: BriefRequest) =>
  invoke<PlanBrief>("orchestra_plan_brief", { request });

export const orchestraStatus = (planId: string) =>
  invoke<PlanView>("orchestra_status", { planId });

/** 这个进程里还认得的计划 id。面板重挂载后靠它找回自己在盯的那份 */
export const orchestraPlans = () => invoke<string[]>("orchestra_plans");

export const orchestraPause = (planId: string) =>
  invoke<number>("orchestra_pause", { planId });

export const orchestraResume = (planId: string) => invoke<void>("orchestra_resume", { planId });

export const orchestraCancel = (planId: string) => invoke<void>("orchestra_cancel", { planId });

export const orchestraBoard = (planId: string) =>
  invoke<string[]>("orchestra_board", { planId });

export const orchestraRerunNode = (planId: string, nodeId: string) =>
  invoke<number>("orchestra_rerun_node", { planId, nodeId });

/** 手工改一条依赖边：`add` 为真是让 `to` 多等一个 `from`，为假是去掉它。
 *  要先暂停，且只能动还没开始的节点（design-multi-agent.md §5.13） */
export const orchestraEditEdge = (planId: string, from: string, to: string, add: boolean) =>
  invoke<void>("orchestra_edit_edge", { planId, from, to, add });

/** 界面上能选出来的边。它比后端那一种少一项（`mapReduce` 改的不是"等谁"而是"这张图有几格"），
 *  而且**这里没有"键"那一格可以填**：能选的边读的键必须是有代码在写的那个，
 *  让人手打一个键名等于允许用户配出一条永远不会成立的边（§5.28） */
export type OrchestraEdgeKind =
  | { kind: "finishToStart" }
  | { kind: "waitForVerdict"; node: string; pass: boolean }
  | { kind: "iterateUntilPass"; maxIters: number };

/** 换某一格的放行方式。判据（等谁的结论、循环几轮）住在后端那一份图里，
 *  这里只把选择交出去：被拒就照原样显示那一句 */
export const orchestraSetEdgeKind = (planId: string, node: string, kind: OrchestraEdgeKind) =>
  invoke<void>("orchestra_set_edge_kind", { planId, node, kind });

export const orchestraLedger = (planId: string) =>
  invoke<TraceRow[]>("orchestra_ledger", { planId });

/** 一次后台 run 推过来的事件。形状与聊天里的 ChatEvent 相同，只是多包了一层归属 */
export interface EmittedRunEvent {
  conversationId: string;
  event:
    | { type: "delta"; text: string }
    | { type: "reasoning"; text: string }
    | { type: "tool"; id: string; name: string; status: string; input: string; output?: string }
    | { type: "done"; inputTokens: number; outputTokens: number; durationMs: number }
    | { type: "notice"; text: string }
    | { type: "error"; message: string };
}

export const RUN_EVENT_NAME = "chat-event";
