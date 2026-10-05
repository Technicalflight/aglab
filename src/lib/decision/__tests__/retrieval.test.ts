/**
 * 检索决策层的行为钉（design-decision-layer-optimization.md §5.6 + §10 禁止事项）：
 * - pick_query 是 Choice：prompt 里没有「生成查询」的指令（Jev 只选不生成）
 * - needs_search < 0.5 / 判定缺答 / pick 无效 → 不搜
 * - 单源失败不牵连；聚合排序 相关性 > 引擎一致 > 原始排名
 * - 分级 TTL 缓存：news 短 reference 长；缓存只挡检索段
 */
import { describe, expect, it, vi } from "vitest";
import {
  clearRetrievalCache,
  decideRetrieval,
  generateCandidateQueries,
  type RetrievalAsker,
  type SearchEngine,
  type SearchHit,
} from "../retrieval";

function hit(url: string, engine = "web", kind: SearchHit["kind"] = "general"): SearchHit {
  return { title: `t-${url}`, url, snippet: `snippet ${url}`, engine, kind };
}

function engine(name: string, results: SearchHit[] | Error): SearchEngine {
  return {
    name,
    search: vi.fn(async () => {
      if (results instanceof Error) throw results;
      return results;
    }),
  };
}

function askerWith(needsSearch: number, pick: string, relevance?: Record<string, number>): RetrievalAsker {
  return async (_state, questions) => {
    const answers: Record<string, number | string> = {};
    if ("needs_search" in questions) answers.needs_search = needsSearch;
    if ("pick_query" in questions) answers.pick_query = pick;
    for (const name of Object.keys(questions)) {
      if (name.startsWith("rel_")) {
        const index = Number(name.slice(4));
        answers[name] = relevance?.[String(index)] ?? 0;
      }
    }
    return { answers };
  };
}

describe("候选查询生成（代码层，B6）", () => {
  it("生成 2–4 个候选：整句 / 去缀 / 术语", () => {
    const candidates = generateCandidateQueries("帮我查一下 React Server Components 的原理");
    expect(candidates.length).toBeGreaterThanOrEqual(2);
    expect(candidates.length).toBeLessThanOrEqual(4);
    expect(candidates[0]).toContain("React");
    // 去缀后的短语不含「帮我查一下」
    if (candidates.length > 1) expect(candidates[1]).not.toContain("帮我");
  });
  it("引号内容优先成为候选", () => {
    const candidates = generateCandidateQueries('帮我看看「西部世界」第二季的评价');
    expect(candidates.some((c) => c === "西部世界")).toBe(true);
  });
  it("空消息零候选", () => {
    expect(generateCandidateQueries("   ")).toEqual([]);
  });
});

describe("检索判定", () => {
  const engines = [engine("web", [hit("https://a/1"), hit("https://a/2")]), engine("news", [hit("https://a/1", "news", "news")])];

  it("needs_search < 0.5 → 不搜（引擎零调用）", async () => {
    clearRetrievalCache();
    const web = engines[0];
    const decision = await decideRetrieval("你好呀", engines, askerWith(0.2, "q0"));
    expect(decision.performed).toBe(false);
    expect(web.search).not.toHaveBeenCalled();
  });

  it("pick_query 花名册之外 → 不搜，绝不自己造查询", async () => {
    clearRetrievalCache();
    const decision = await decideRetrieval("查一下 react", engines, askerWith(0.9, "q9"));
    expect(decision.performed).toBe(false);
    expect(decision.reason).toContain("无效");
  });

  it("判定不可用（asker 抛错）→ 不搜", async () => {
    clearRetrievalCache();
    const asker: RetrievalAsker = async () => {
      throw new Error("down");
    };
    const decision = await decideRetrieval("查一下 rust", engines, asker);
    expect(decision.performed).toBe(false);
  });

  it("正常检索：多源并行、单源失败不牵连、评分聚合排序", async () => {
    clearRetrievalCache();
    const good1 = engine("web", [hit("https://x/1"), hit("https://x/2")]);
    const broken = engine("broken", new Error("timeout"));
    const good2 = engine("news", [hit("https://x/1", "news", "news")]);
    // rel_0 = 5（https://x/1）最高；rel_1 = 1
    const decision = await decideRetrieval("查询内容 abc", [good1, broken, good2], askerWith(0.9, "q0", { 0: 5, 1: 1 }));
    expect(decision.performed).toBe(true);
    expect(broken.search).toHaveBeenCalled(); // 试过但失败被吞
    expect(decision.hits![0].url).toBe("https://x/1");
    expect(decision.hits![0].engineAgreement).toBe(2); // 两家都返回
    expect(decision.hits![1].url).toBe("https://x/2");
  });

  it("TTL 缓存命中：第二次检索不再调用引擎", async () => {
    clearRetrievalCache();
    const web = engine("web", [hit("https://y/1")]);
    const first = await decideRetrieval("缓存测试 query", [web], askerWith(0.9, "q0"));
    expect(first.cached).toBeUndefined();
    const second = await decideRetrieval("缓存测试 query", [web], askerWith(0.9, "q0"));
    expect(second.cached).toBe(true);
    expect(web.search).toHaveBeenCalledTimes(1);
  });

  it("分级 TTL：news 类型用短 TTL（时钟注入验证）", async () => {
    clearRetrievalCache();
    let clock = 1_000_000;
    const now = () => clock;
    const web = engine("news", [hit("https://z/1", "news", "news")]); // news TTL = 10min
    await decideRetrieval("news ttl query", [web], askerWith(0.9, "q0"), { now });
    clock += 9 * 60_000; // 9 分钟后：仍命中
    const fresh = await decideRetrieval("news ttl query", [web], askerWith(0.9, "q0"), { now });
    expect(fresh.cached).toBe(true);
    clock += 2 * 60_000; // 再过 2 分钟（共 11 分钟）：过期
    const stale = await decideRetrieval("news ttl query", [web], askerWith(0.9, "q0"), { now });
    expect(stale.cached).toBeUndefined();
    expect(web.search).toHaveBeenCalledTimes(2);
  });
});
