/**
 * 端到端集成测试：起一个**真的** HTTP sidecar 桩（node:http，不是 mock fetch），
 * 把 LayaProvider 的 http 传输、路由器漏斗、缓存、审计整条链跑通。
 * Jev 用注入的 fetch/invoke 桩——云端不出现在测试里，但升级链要真实地穿过它。
 */
import http from "node:http";
import type { AddressInfo } from "node:net";
import { describe, expect, it, vi } from "vitest";
import { DecisionRouterImpl } from "../router";
import { DecisionCache } from "../cache";
import { DecisionAudit } from "../audit";
import { LayaProvider } from "../providers/laya";
import { JevProvider } from "../providers/jev";
import { NullFallbackProvider } from "../providers/fallback";
import { DEFAULT_DECISION_CONFIG, mergeDecisionConfig } from "../config";
import type { DecisionRequest } from "../types";

interface SidecarHandle {
  endpoint: string;
  hits(): number;
  close(): Promise<void>;
}

/** 假 sidecar：/health 报已加载；/systemOne 计数并按脚本返回答案。
 * 故意**不发 CORS 头**也不测它：Node 的 fetch 不执行 CORS，在这儿加头测不出任何东西。
 * 那份规矩由 sidecar.test.ts 对着真参考实现（scripts/laya-sidecar/index.mjs）钉。 */
async function startSidecar(answers: Record<string, unknown>): Promise<SidecarHandle> {
  let hits = 0;
  const server = http.createServer((req, res) => {
    if (req.method === "GET" && req.url === "/health") {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify({ ok: true, loaded: true, loading: false }));
      return;
    }
    const chunks: Buffer[] = [];
    req.on("data", (chunk) => chunks.push(chunk));
    req.on("end", () => {
      hits += 1;
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify({ answers }));
    });
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address() as AddressInfo;
  return {
    endpoint: `http://127.0.0.1:${address.port}`,
    hits: () => hits,
    close: () => new Promise<void>((resolve) => server.close(() => resolve())),
  };
}

function buildRouter(laya: LayaProvider, jev: JevProvider) {
  const audit = new DecisionAudit(200);
  const router = new DecisionRouterImpl({
    laya,
    jev,
    fallback: new NullFallbackProvider(),
    cache: new DecisionCache(60_000, 100),
    audit,
    config: mergeDecisionConfig(DEFAULT_DECISION_CONFIG),
  });
  return { router, audit };
}

const request: DecisionRequest = {
  state: "user message about a refund",
  questions: { worth_it: { type: "noul", instructions: "Is this worth doing?" } },
};

describe("决策层端到端（真实 HTTP sidecar）", () => {
  it("Laya 高置信走完全程：缓存吃掉第二次调用，审计两条 trace", async () => {
    const sidecar = await startSidecar({ worth_it: { noul: 0.96 } });
    try {
      const laya = new LayaProvider({
        transport: "http",
        sidecarEndpoint: sidecar.endpoint,
        timeoutMs: 2000,
      }); // 故意不注入 fetchImpl——走真 fetch
      const jevFetch = vi.fn(async () => new Response("{}", { status: 200 }));
      const { router, audit } = buildRouter(
        laya,
        new JevProvider({
          apiKey: "sk",
          transport: "direct",
          timeoutMs: 1000,
          fetchImpl: jevFetch as unknown as typeof fetch,
        }),
      );

      const first = await router.decide(request);
      expect(first.model).toBe("laya");
      expect(first.degraded).toBe(false);
      expect(first.cacheHit).toBe(false);
      expect(sidecar.hits()).toBe(1);

      const second = await router.decide(request);
      expect(second.cacheHit).toBe(true);
      expect(sidecar.hits()).toBe(1); // 第二次没打到 sidecar
      expect(jevFetch).not.toHaveBeenCalled();

      expect(audit.size).toBe(2);
      expect(audit.recent(1)[0].modelChain).toEqual([]); // 缓存命中是空链路
      expect(audit.recent(2)[1].modelChain).toEqual(["laya"]);
    } finally {
      await sidecar.close();
    }
  });

  it("升级链穿透真实 HTTP：sidecar 低置信 → Jev 兜上", async () => {
    const sidecar = await startSidecar({ worth_it: { noul: 0.3 } });
    try {
      const laya = new LayaProvider({
        transport: "http",
        sidecarEndpoint: sidecar.endpoint,
        timeoutMs: 2000,
      });
      const jevFetch = vi.fn(
        async () =>
          new Response(JSON.stringify({ answers: { worth_it: { noul: 0.91 } } }), { status: 200 }),
      );
      const { router, audit } = buildRouter(
        laya,
        new JevProvider({
          apiKey: "sk",
          transport: "direct",
          timeoutMs: 1000,
          fetchImpl: jevFetch as unknown as typeof fetch,
        }),
      );

      const result = await router.decide(request);
      // V2 按问升级：合并响应主体层是 laya，采信的 Jev 补答在 sources 里溯源
      expect(result.model).toBe("laya");
      expect(result.sources?.worth_it).toBe("jev");
      expect(result.degraded).toBe(false);
      expect(sidecar.hits()).toBe(1);
      expect(jevFetch).toHaveBeenCalledTimes(1);
      expect(audit.recent(1)[0].modelChain).toEqual(["laya", "jev"]);
    } finally {
      await sidecar.close();
    }
  });

  it("confidential：请求只打到本机 sidecar，云端桩一次都不被碰，审计只剩哈希", async () => {
    const sidecar = await startSidecar({ worth_it: { noul: 0.4 } });
    try {
      const laya = new LayaProvider({
        transport: "http",
        sidecarEndpoint: sidecar.endpoint,
        timeoutMs: 2000,
      });
      const jevInvoke = vi.fn(async (_command: string, _args: Record<string, unknown>) => ({
        answers: { worth_it: { noul: 0.99 } },
      }));
      const { router, audit } = buildRouter(
        laya,
        new JevProvider({
          apiKey: "sk",
          transport: "rust",
          timeoutMs: 1000,
          invokeImpl: jevInvoke,
        }),
      );

      const result = await router.decide({
        ...request,
        state: "secret-token-abc",
        sensitivity: "confidential",
      });
      expect(result.model).toBe("laya");
      expect(result.degraded).toBe(true); // 本地低置信也认，红线优先于精度
      expect(sidecar.hits()).toBe(1); // sidecar 在本机，允许打
      expect(jevInvoke).not.toHaveBeenCalled(); // 云端通道一次都不许碰
      const confState = String(audit.recent(1)[0].request.state);
      expect(confState).not.toContain("secret-token-abc");
      expect(confState).toMatch(/^<confidential: \d+ chars, hash=[0-9a-f]{8}>$/);
    } finally {
      await sidecar.close();
    }
  });

  /**
   * 端到端的回归针：score 答案的置信度来自模型自带的那一格。
   * 曾经 normalize 只认一个上游从没发过的 `distribution` 数组，取不到就报 0.5——
   * 于是**任何含 score 问题的判定永远达不到阈值**：每次都升级、每次都 degraded，
   * 而单元测试因为 fixture 用的是同一个虚构字段而一路绿。这条走真 HTTP 与真漏斗。
   */
  it("score 判定带着自己的 confidence：够确信就不升级、不降级", async () => {
    const sidecar = await startSidecar({
      complexity: {
        type: "score",
        score: 3.1,
        probabilities: {
          trivial: 0.02,
          simple: 0.05,
          moderate: 0.1,
          complex: 0.6,
          very_complex: 0.23,
        },
        confidence: 0.91,
        rl_agent: { act_probability: 0.9 },
      },
    });
    try {
      const laya = new LayaProvider({
        transport: "http",
        sidecarEndpoint: sidecar.endpoint,
        timeoutMs: 2000,
      });
      const jevInvoke = vi.fn(async (_command: string, _args: Record<string, unknown>) => ({
        answers: { complexity: { type: "score", score: 3.4, confidence: 0.95 } },
      }));
      const { router, audit } = buildRouter(
        laya,
        new JevProvider({
          apiKey: "sk",
          transport: "rust",
          timeoutMs: 1000,
          invokeImpl: jevInvoke,
        }),
      );
      const result = await router.decide({
        state: "refactor the session store",
        questions: {
          complexity: {
            type: "score",
            instructions: "How complex?",
            criteria: ["trivial", "simple", "moderate", "complex", "very_complex"],
          },
        },
      });
      expect(result.answers.complexity.confidence).toBe(0.91);
      expect(result.degraded).toBe(false);
      expect(result.model).toBe("laya");
      expect(sidecar.hits()).toBe(1); // 没有为了"再问一次云端"多花一发
      expect(jevInvoke).not.toHaveBeenCalled();
      expect(audit.recent(1)[0].modelChain).toEqual(["laya"]);
    } finally {
      await sidecar.close();
    }
  });

  it("health() 读到 sidecar 的 loaded 态；rust 传输的 Jev 在漏斗里正常补位", async () => {
    const sidecar = await startSidecar({ worth_it: { noul: 0.5 } });
    try {
      const laya = new LayaProvider({
        transport: "http",
        sidecarEndpoint: sidecar.endpoint,
        timeoutMs: 2000,
      });
      expect(await laya.health()).toEqual({ ok: true, loaded: true, loading: false });
      await laya.warmup(); // 预热不该抛

      const jevInvoke = vi.fn(async (_command: string, _args: Record<string, unknown>) => ({
        answers: { worth_it: { noul: 0.88 } },
      }));
      const { router } = buildRouter(
        laya,
        new JevProvider({
          apiKey: "sk",
          transport: "rust",
          timeoutMs: 1000,
          invokeImpl: jevInvoke,
        }),
      );
      const result = await router.decide(request);
      expect(result.model).toBe("laya"); // V2：升级补答后合并，主体层保留
      expect(result.sources?.worth_it).toBe("jev"); // 0.5 → 升级；0.88 ≥ 0.85 → 采信
      expect(jevInvoke).toHaveBeenCalledTimes(1);
    } finally {
      await sidecar.close();
    }
  });
});
