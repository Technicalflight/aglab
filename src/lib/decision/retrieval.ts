/**
 * 检索决策层（design-decision-layer-optimization.md §5.6，修 B6）。
 *
 * Jev 全程不生成文本（禁止事项）：
 *   用户消息 → **代码层**生成 2–4 个候选查询（整句规范化 / 去对话缀名词短语 / 最近术语）
 *   → Jev：needs_search（Noul）＋ pick_query（Choice，只选不生成）
 *   → needs_search < 0.5 或走兜底 → **不搜**（§3：不为了搜而搜）
 *   → 多源并行检索（每源 15s / 整体 30s，单源失败不牵连）
 *   → Jev 相关性评分（Score，批内一问一结果）
 *   → 代码层聚合排序（相关性 > 引擎一致 > 原始排名）→ append-only 交还调用方
 *
 * 检索结果的分级 TTL 缓存按「来源声明的内容类型」分档（news 短 / reference 长）；
 * 缓存只挡**检索**（贵的那段），Jev 的判定每次照做——查询是决策的产物，不是决策本身。
 */
import {
  RETRIEVAL_DEFAULT_TTL_MS,
  RETRIEVAL_ENGINE_TIMEOUT_MS,
  RETRIEVAL_MAX_CANDIDATES,
  RETRIEVAL_NEEDS_SEARCH_THRESHOLD,
  RETRIEVAL_OVERALL_TIMEOUT_MS,
  RETRIEVAL_TTL_MS,
} from "./constants";
import { fnv1a } from "./cache";
import { incrementMetric } from "./metrics";

/* ---- 代码层候选查询生成（B6：候选查询 Choice 的「候选」半边）---- */

/** 对话缀词：去掉后剩下的名词性短语更适合当检索词。轻量列表，宁多留不误删 */
const CONVERSATION_PREFIXES = [
  "帮我", "麻烦", "请", "请问", "我想知道", "我想了解", "想知道", "查一下", "查查", "搜一下", "搜索一下", "搜搜",
  "tell me", "what is", "what are", "who is", "please", "can you", "could you", "search for", "look up", "find out",
];

/** 从一条用户消息生成候选查询：整句规范化 / 去缀短语 / 引号或术语 */
export function generateCandidateQueries(userMessage: string): string[] {
  const trimmed = userMessage.trim().replace(/\s+/g, " ");
  if (trimmed.length === 0) return [];
  const candidates: string[] = [];
  // 1) 整句规范化
  const normalized = trimmed.length > 200 ? trimmed.slice(0, 200) : trimmed;
  candidates.push(normalized);
  // 2) 去对话缀的名词短语
  let stripped = normalized;
  for (const prefix of CONVERSATION_PREFIXES) {
    const re = new RegExp(`^${prefix}[，,\\s:：]+`, "i");
    if (re.test(stripped)) {
      stripped = stripped.replace(re, "").trim();
      break;
    }
  }
  // 句尾语气清理
  stripped = stripped.replace(/[？?！!。.，,]+$/g, "").trim();
  if (stripped.length > 0 && stripped !== normalized) candidates.push(stripped);
  // 3) 引号内容或高信息密度词串
  const quoted = trimmed.match(/[「“"'『]([^」”"'』]{2,60})[」”"'』]/);
  if (quoted?.[1]) {
    candidates.push(quoted[1].trim());
  } else {
    const terms = trimmed.match(/[\u4e00-\u9FFF]{2,12}|[A-Za-z][A-Za-z0-9.+-]{2,}/g) ?? [];
    const tail = terms.slice(-3).join(" ");
    if (tail.length > 0 && tail !== stripped) candidates.push(tail);
  }
  // 去重 + 数量钳制（下限守不住就守——候选少于 2 时 pick_query 的选项太窄，宁可让整句独扛）
  const deduped = [...new Set(candidates.map((c) => c.trim()).filter((c) => c.length > 0))];
  return deduped.slice(0, RETRIEVAL_MAX_CANDIDATES);
}

/* ---- 检索源与结果 ---- */

export interface SearchHit {
  title: string;
  url: string;
  snippet: string;
  /** 引擎名（聚合排序的「引擎一致」维度用） */
  engine: string;
  /** 来源声明的内容类型（TTL 分级键） */
  kind?: "news" | "general" | "reference";
}

/** 检索源适配器：调用方注入（本地引擎/云端 API 各自实现），检索层不关心协议 */
export interface SearchEngine {
  name: string;
  search(query: string, timeoutMs: number): Promise<SearchHit[]>;
}

export interface RankedHit extends SearchHit {
  /** Jev 相关性（量表 0–5 的期望值） */
  relevance: number;
  /** 同一 URL 被几个引擎返回（引擎一致） */
  engineAgreement: number;
}

export interface RetrievalDecision {
  performed: boolean;
  /** 最终采用的查询（performed=true 时在） */
  query?: string;
  /** 候选清单与 Jev 的选择（审计/回放用） */
  candidates?: string[];
  picked?: string;
  needsSearch?: number;
  hits?: RankedHit[];
  reason?: string;
  cached?: boolean;
}

export interface RetrievalAsker {
  (state: string, questions: Record<string, RetrievalQuestion>): Promise<{
    answers: Record<string, number | string>;
  }>;
}

export interface RetrievalQuestion {
  type: "noul" | "score" | "choice";
  instructions: string;
  criteria?: Record<string, string> | Array<string | number>;
}

export interface RetrievalOptions {
  /** 敏感性：检索查询从对话正文里来，默认 private（钉本地判定；检索引擎本身由调用方选） */
  sensitivity?: "public" | "private" | "confidential";
  /** 测试注入时钟 */
  now?: () => number;
}

/* ---- 分级 TTL 缓存 ---- */

interface CacheEntry {
  hits: SearchHit[];
  expiresAt: number;
}
const retrievalCache = new Map<string, CacheEntry>();

export function clearRetrievalCache(): void {
  retrievalCache.clear();
}

export function retrievalCacheSize(): number {
  return retrievalCache.size;
}

function ttlForKind(kind: SearchHit["kind"]): number {
  if (kind && kind in RETRIEVAL_TTL_MS) return RETRIEVAL_TTL_MS[kind];
  return RETRIEVAL_DEFAULT_TTL_MS;
}

function cacheKey(query: string, engines: ReadonlyArray<SearchEngine>): string {
  return `rt:${fnv1a(query)}:${engines.map((e) => e.name).sort().join(",")}`;
}

/* ---- 多源并行检索：单源失败不牵连 ---- */

async function searchAllEngines(
  query: string,
  engines: ReadonlyArray<SearchEngine>,
): Promise<SearchHit[]> {
  const deadline = Date.now() + RETRIEVAL_OVERALL_TIMEOUT_MS;
  const tasks = engines.map(async (engine) => {
    const perEngine = Math.min(RETRIEVAL_ENGINE_TIMEOUT_MS, Math.max(1000, deadline - Date.now()));
    const hits = await engine.search(query, perEngine);
    return hits.map((hit) => ({ ...hit, engine: hit.engine || engine.name }));
  });
  // allSettled：单源失败不牵连其余（草图 §6.3 语义）
  const settled = await Promise.allSettled(tasks);
  const hits: SearchHit[] = [];
  for (const result of settled) {
    if (result.status === "fulfilled") hits.push(...result.value);
  }
  return hits;
}

/* ---- 聚合排序：相关性 > 引擎一致 > 原始排名 ---- */

function aggregate(hits: SearchHit[], relevance: Map<string, number>): RankedHit[] {
  const byUrl = new Map<string, RankedHit>();
  hits.forEach((hit, index) => {
    const key = hit.url || `#${index}`;
    const existing = byUrl.get(key);
    if (existing) {
      existing.engineAgreement += 1;
      return;
    }
    byUrl.set(key, {
      ...hit,
      relevance: relevance.get(key) ?? 0,
      engineAgreement: 1,
      // 原始排名：先到者优先（隐含在插入序里）
    });
  });
  return [...byUrl.values()].sort(
    (a, b) => b.relevance - a.relevance || b.engineAgreement - a.engineAgreement || 0,
  );
}

/** 主入口：检索判定。全链不确定 → 不搜（§3），绝不为了搜而搜 */
export async function decideRetrieval(
  userMessage: string,
  engines: ReadonlyArray<SearchEngine>,
  asker: RetrievalAsker,
  options: RetrievalOptions = {},
): Promise<RetrievalDecision> {
  const now = options.now ?? Date.now;
  const candidates = generateCandidateQueries(userMessage);
  if (candidates.length === 0) return { performed: false, reason: "消息为空，无可查询内容" };

  // Jev 只选不生成：needs_search + pick_query 一次前向
  let answers: Record<string, number | string>;
  try {
    const result = await asker(userMessage.slice(0, 2000), {
      needs_search: {
        type: "noul",
        instructions: "Does answering this user message require searching external sources?",
      },
      pick_query: {
        type: "choice",
        instructions: "Which query best captures the user's information need? Pick one; do not invent a new one.",
        criteria: Object.fromEntries(candidates.map((candidate, index) => [`q${index}`, candidate])),
      },
    });
    answers = result.answers;
  } catch (error) {
    return { performed: false, candidates, reason: `判定不可用：${error instanceof Error ? error.message : String(error)}` };
  }

  const needsSearch = typeof answers.needs_search === "number" ? answers.needs_search : 0;
  if (needsSearch < RETRIEVAL_NEEDS_SEARCH_THRESHOLD) {
    incrementMetric("retrieval.skipped");
    return { performed: false, candidates, needsSearch, reason: "needs_search 低于阈值，不搜" };
  }
  const pickedIndex = typeof answers.pick_query === "string" ? Number(answers.pick_query.slice(1)) : NaN;
  const picked = Number.isInteger(pickedIndex) && pickedIndex >= 0 && pickedIndex < candidates.length
    ? candidates[pickedIndex]
    : undefined;
  if (!picked) {
    // pick_query 缺答或选了花名册外的：不确定 → 不搜（绝不自己造一个查询）
    return { performed: false, candidates, needsSearch, reason: "pick_query 缺答或无效，不搜" };
  }

  // 检索缓存（分级 TTL）：只挡检索段，判定段每次照做
  const key = cacheKey(picked, engines);
  const cached = retrievalCache.get(key);
  if (cached && cached.expiresAt > now()) {
    incrementMetric("retrieval.cache_hit");
    incrementMetric("retrieval.performed");
    return {
      performed: true,
      query: picked,
      candidates,
      picked,
      needsSearch,
      hits: cached.hits.map((hit) => ({ ...hit, relevance: 5, engineAgreement: 1 })),
      cached: true,
      reason: "命中检索缓存",
    };
  }

  if (engines.length === 0) {
    return { performed: false, candidates, picked, needsSearch, reason: "没有配置检索源" };
  }

  // 多源并行 → 相关性评分 → 聚合
  const hits = await searchAllEngines(picked, engines);
  if (hits.length === 0) {
    return { performed: true, query: picked, candidates, picked, needsSearch, hits: [], reason: "检索无结果" };
  }

  // 相关性评分：批内一问一结果（零输出 token 的批量形态）
  const relevance = new Map<string, number>();
  try {
    const questions: Record<string, RetrievalQuestion> = {};
    hits.forEach((_hit, index) => {
      questions[`rel_${index}`] = {
        type: "score",
        instructions: `How relevant is this result to the query "${picked}"?`,
        criteria: [0, 1, 2, 3, 4, 5],
      };
    });
    const scored = await asker(
      JSON.stringify({ query: picked, results: hits.map((h) => ({ title: h.title, snippet: h.snippet.slice(0, 200) })) }),
      questions,
    );
    hits.forEach((hit, index) => {
      const value = scored.answers[`rel_${index}`];
      const score = typeof value === "number" ? value : 0;
      const key = hit.url || `#${index}`;
      const existing = relevance.get(key);
      // 同一 URL 被多个引擎返回时会得分多次：取最大——相关性是 URL 的属性，不是返回顺序的
      if (existing === undefined || score > existing) relevance.set(key, score);
    });
  } catch {
    // 评分不可用 → 全部按 0 分（引擎一致与原始排名仍可用），不搜这次的重试——检索是只读操作
  }

  const ranked = aggregate(hits, relevance);
  incrementMetric("retrieval.performed");

  // 写缓存：TTL 按结果里声明的内容类型分档（取最短的——一条 news 混进 reference 结果，整批按 news 算）
  const ttl = Math.min(...ranked.map((hit) => ttlForKind(hit.kind)));
  retrievalCache.set(key, { hits: ranked, expiresAt: now() + ttl });

  return { performed: true, query: picked, candidates, picked, needsSearch, hits: ranked };
}
