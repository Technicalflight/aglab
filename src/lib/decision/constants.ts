/**
 * 决策层 V2 的代码常量（design-decision-layer-optimization.md §7）。
 *
 * 已拍板的口径：**红线不做旋钮**。这一轮新增能力（压缩 / 审查 / 检索 / Provider 链）
 * 的全部阈值住在这里，不进配置文件——配置只留各嵌入点的 enabled 开关与用户资产
 * （密钥、服务商、链顺序）。`constants.test.ts` 把每个值钉死成字面量断言：
 * 改这里的值测试就红，谁也别想悄悄调阈值。
 *
 * 例外（文档允许的运行期输入）：请求自带的 timeoutMs 仍优先于 DECISION_TIMEOUT_MS，
 * 那是调用方对单次决策的显式承诺，不是阈值。
 */

/* ---- 漏斗 V2（§4，修 A3）---- */
/** 采信门槛：全部答案的最小置信达到它才「System 1 拍板」，否则按问题粒度升级 */
export const ESCALATE_MIN_CONFIDENCE = 0.85;
/** 低置信子集送 Jev 升级的分批钉：≤8 问单包升级省 token，>8 问整批送省往返 */
export const PER_QUESTION_UPGRADE_BATCH = 8;

/* ---- Verbatim Compaction（§5.2 + §2.1）---- */
/** D4：结果全文保留的证据门槛（偏高 → 偏向截断） */
export const KEEP_RESULT_THRESHOLD = 0.6;
/** D4：调用记录的丢弃门槛（偏低 → 偏向保记录） */
export const KEEP_CALL_THRESHOLD = 0.4;
/** D1：失败批次重试一次；仍失败/缺答的调用判定 keep（reason: unanswered） */
export const COMPACT_BATCH_RETRIES = 1;
/** goal 注入：默认取最近 3 条用户消息锚定「还需不需要」 */
export const GOAL_RECENT_USER_MESSAGES = 3;
/** goal 注入：每条用户消息截断的字符数 */
export const GOAL_MESSAGE_MAX_CHARS = 500;
/** 六级降档：工具输入的逐级截断宽度（full 装不下时先 200，再 60） */
export const INPUT_TRUNCATE_LEVELS = [200, 60] as const;
/** texts abridged 档：老非钉消息正文掐头去尾的宽度（钉扎的最后动） */
export const ABRIDGE_HEAD_CHARS = 400;
export const ABRIDGE_TAIL_CHARS = 150;
/** 首条消息与最近 N 条内的调用钉死不动 */
export const PRESERVE_RECENT_MESSAGES = 6;
/** state 装配的预算（原型默认，R1 用 CJK 校准系数回测） */
export const MAX_STATE_TOKENS = 25_000;
/** 单请求（state + 问题）的预算；超了按问题分批 */
export const MAX_REQUEST_TOKENS = 30_000;
/** D3：CJK 连续段 ≥1.2 token/字（英文标定必然低估，R1 用真用量回测） */
export const CJK_TOKENS_PER_CHAR = 1.2;
/** 截断附注（drop_result）保留的结果头部长度 */
export const TRUNCATE_HEAD_CHARS = 300;

/* ---- Sticky / Cache Guard（§5.2）---- */
/** provider 侧 KV 缓存命中率的警戒线：低于它说明前缀在被悄悄改坏 */
export const CACHE_GUARD_CEILING = 0.8;
/** replacement 映射的 rewrite 触发：历史增长 +40% 且距上次 ≥15 发 */
export const REWRITE_GROWTH = 0.4;
export const MIN_REQUESTS_BETWEEN_REWRITES = 15;
/** 或 40 发封顶，二者先到先触发 */
export const MAX_REQUESTS_BETWEEN_REWRITES = 40;

/* ---- 分阶段输出审查（§5.3，修 B8）---- */
/** Stage A 风险矩阵的短路门槛：任一风险 noul ≥ 0.9 → block（C/D 不跑） */
export const REVIEW_SHORT_CIRCUIT = 0.9;
/** 代码覆盖：severity ≥ 4 且 Jev 说 allow → escalate（代码优先于 Jev route） */
export const REVIEW_ESCALATE_SEVERITY = 4;
/** 代码覆盖：severity ≥ 2 且 Jev 说 allow → annotate */
export const REVIEW_ANNOTATE_SEVERITY = 2;
/** 流式预检的触发点：首块 ≥ 400 tokens 时先审再继续放 */
export const REVIEW_FIRST_CHUNK_TOKENS = 400;
/** 每阶段的超时；三阶段串行，最坏尾延迟 = 3 × 它 */
export const DECISION_TIMEOUT_MS = 5_000;

/* ---- 检索决策层（§5.6，修 B6）---- */
/** 代码层候选查询的个数区间（整句规范化 / 去对话缀名词短语 / 最近术语） */
export const RETRIEVAL_MIN_CANDIDATES = 2;
export const RETRIEVAL_MAX_CANDIDATES = 4;
/** needs_search < 0.5 或走兜底 → 不搜（§3：不为了搜而搜） */
export const RETRIEVAL_NEEDS_SEARCH_THRESHOLD = 0.5;
/** 每源超时 / 整体超时；单源失败不牵连其余源 */
export const RETRIEVAL_ENGINE_TIMEOUT_MS = 15_000;
export const RETRIEVAL_OVERALL_TIMEOUT_MS = 30_000;
/**
 * 检索结果的分级 TTL（照草图 §6.5 的分级思想；草图原文不在库，这里按内容时效定）：
 * news 时效最短，reference/长文档最长。键是来源声明的内容类型。
 */
export const RETRIEVAL_TTL_MS: Readonly<Record<string, number>> = {
  news: 10 * 60_000,
  general: 60 * 60_000,
  reference: 24 * 60 * 60_000,
};
export const RETRIEVAL_DEFAULT_TTL_MS = 60 * 60_000;

/* ---- Provider 链（§5.4，修 B7）---- */
/**
 * 降级语义：402 / 429 / 5xx / 超时 / 网络错误 → continue 下一家；
 * 其余 4xx（含 401/403/404）→ fail-fast 抛聚合错误。列表只服务「该不该降级」的判定，
 * retryOn 配置已按 B7 裁定删除——代码 `>= 500` 权威，这里钉的是 402/429 两个特例。
 */
export const PROVIDER_FALLBACK_STATUSES = [402, 429] as const;

/* ---- 深度优化：Jev 断路器（文档之外的增量）---- */
/** 连续失败 N 跳进入冷却：期间直接跳过该跳，不再每发都付一次完整超时 */
export const BREAKER_FAILURE_THRESHOLD = 3;
export const BREAKER_COOLDOWN_MS = 30_000;
/** 冷却结束后放行的探测请求数（半开态） */
export const BREAKER_HALF_OPEN_PROBES = 1;
