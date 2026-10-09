import { describe, expect, it, vi } from "vitest";
import type { Mock } from "vitest";
import { LayaProvider } from "../providers/laya";
import { JevProvider, JEV_ENDPOINTS } from "../providers/jev";
import { NullFallbackProvider } from "../providers/fallback";
import { DecisionTimeoutError, DecisionUnavailableError } from "../errors";
import type { DecisionRequest } from "../types";

const request: DecisionRequest = {
  state: { ticket: "refund not received", days: 14 },
  questions: {
    urgency: { type: "score", instructions: "How urgent?", criteria: ["low", "mid", "high"] },
  },
};

function jsonResponse(payload: unknown, status = 200): Response {
  return new Response(JSON.stringify(payload), { status });
}

/** 显式签名的 fetch mock：mock.calls[0] 直接是 [url, init] 元组，不用到处 as */
type FetchHandler = (url: string, init: RequestInit) => Promise<Response>;
function mockFetch(handler: FetchHandler): Mock<FetchHandler> {
  return vi.fn(handler);
}

describe("LayaProvider（http 传输）", () => {
  /**
   * fixture 的形状取自 @receptron/laya 的 dist/types.d.ts（ScoreAnswer：
   * type/score/legend/probabilities/confidence/rl_agent），不是我们自己编的字段。
   */
  it("POST 到 sidecar 的 /systemOne，对象 state 原样进 body", async () => {
    const fetchImpl = mockFetch(async () =>
      jsonResponse({
        model: "laya-0.6b",
        answers: {
          urgency: {
            type: "score",
            score: 1.4,
            legend: { "0": "low", "1": "mid", "2": "high" },
            probabilities: { low: 0.1, mid: 0.7, high: 0.2 },
            confidence: 0.62,
            rl_agent: { act_probability: 0.7 },
          },
        },
        usage: { input_tokens: 128, output_tokens: 0 },
      }),
    );
    const provider = new LayaProvider({
      transport: "http",
      sidecarEndpoint: "http://127.0.0.1:8787/",
      timeoutMs: 1000,
      fetchImpl: fetchImpl as unknown as typeof fetch,
    });
    const result = await provider.decide(request);
    const [url, init] = fetchImpl.mock.calls[0];
    expect(url).toBe("http://127.0.0.1:8787/systemOne"); // 尾斜杠被归一掉
    expect(init.method).toBe("POST");
    const body = JSON.parse(init.body as string);
    expect(body.state).toEqual(request.state);
    expect(body.questions).toEqual(request.questions);
    expect(result.model).toBe("laya");
    expect(result.answers.urgency.score).toBe(1.4);
    // 用模型自带的那一把（0.62），不是分布峰值（0.7）——上游按选项数调过温度，那把尺才是出厂口径
    expect(result.answers.urgency.confidence).toBe(0.62);
    expect(result.answers.urgency.probabilities).toEqual({ low: 0.1, mid: 0.7, high: 0.2 });
  });

  it("置信度的取数顺序：模型给的 > 按原语自算 > 中性 0.5", async () => {
    const fetchImpl = mockFetch(async () =>
      jsonResponse({
        answers: {
          // 自带 confidence：即便 top-1 概率是 0.9415，也读 0.55
          dept: {
            type: "choice",
            choice: "billing",
            probabilities: { billing: 0.9415, support: 0.031 },
            confidence: 0.55,
            rl_agent: { act_probability: 0.94 },
          },
          // noul 上游确实不带 confidence：按布尔的确信度算，与极性无关
          churn: { type: "noul", noul: 0.02, rl_agent: { act_probability: 0.02 } },
          // score 没带 confidence：退回分布峰值，而不是 0.5
          risk: { type: "score", score: 1.2, probabilities: { a: 0.2, b: 0.8 } },
          // 什么都没带：中性值，路由器会当「不够确信」处理
          vague: { type: "score", score: 2 },
        },
      }),
    );
    const provider = new LayaProvider({
      transport: "http",
      sidecarEndpoint: "http://x",
      timeoutMs: 1000,
      fetchImpl: fetchImpl as unknown as typeof fetch,
    });
    const result = await provider.decide(request);
    expect(result.answers.dept.type).toBe("choice");
    expect(result.answers.dept.confidence).toBe(0.55);
    expect(result.answers.churn.confidence).toBe(0.98);
    expect(result.answers.risk.confidence).toBe(0.8);
    expect(result.answers.vague.confidence).toBe(0.5);
  });

  /**
   * 反向钉：`distribution: number[]` 是历史上我们**凭空发明**的字段，上游从没发过它。
   * 曾经 normalize 只认它，于是每个 score 都读成 0.5、含 score 的判定永远降级。
   * 这条针的作用是让"再照那个形状写"必须红。
   */
  it("不认虚构的 distribution 字段：上游不发它，读了就是又一次静默放宽", async () => {
    const fetchImpl = mockFetch(async () =>
      jsonResponse({
        answers: {
          urgency: { type: "score", score: 1.2, distribution: [0.2, 0.8] },
        },
      }),
    );
    const provider = new LayaProvider({
      transport: "http",
      sidecarEndpoint: "http://x",
      timeoutMs: 1000,
      fetchImpl: fetchImpl as unknown as typeof fetch,
    });
    const answer = (await provider.decide(request)).answers.urgency;
    expect(answer.confidence).toBe(0.5); // 不是 0.8：那个字段不存在于契约里
    expect(answer.probabilities).toBeUndefined();
  });

  it("载荷没有 type 时按字段形状认原语，畸形答案整批不可用", async () => {
    const fetchImpl = mockFetch(async () =>
      jsonResponse({
        answers: {
          dept: { choice: "billing", probabilities: { billing: 0.6, support: 0.4 } },
          churn: { noul: 0.3 },
          // 声明与载荷都带：信声明。两种原语都有的载荷只在这一条规则下才有唯一解
          both: { type: "score", score: 1.5, choice: "ignored" },
        },
      }),
    );
    const provider = new LayaProvider({
      transport: "http",
      sidecarEndpoint: "http://x",
      timeoutMs: 1000,
      fetchImpl: fetchImpl as unknown as typeof fetch,
    });
    const result = await provider.decide(request);
    expect(result.answers.dept.type).toBe("choice");
    expect(result.answers.dept.confidence).toBe(0.6); // 没有自带值时按选中项概率
    expect(result.answers.churn.type).toBe("noul");
    expect(result.answers.churn.confidence).toBe(0.7);
    expect(result.answers.both.type).toBe("score");

    const broken = new LayaProvider({
      transport: "http",
      sidecarEndpoint: "http://x",
      timeoutMs: 1000,
      fetchImpl: (async () =>
        jsonResponse({
          answers: { q: { note: "三种原语字段都没有" } },
        })) as unknown as typeof fetch,
    });
    await expect(broken.decide(request)).rejects.toBeInstanceOf(DecisionUnavailableError);
  });

  it("sidecar 503 → 不可用错误；请求超时 → 超时错误", async () => {
    const unavailable = new LayaProvider({
      transport: "http",
      sidecarEndpoint: "http://x",
      timeoutMs: 1000,
      fetchImpl: mockFetch(async () =>
        jsonResponse({ error: "cold" }, 503),
      ) as unknown as typeof fetch,
    });
    await expect(unavailable.decide(request)).rejects.toBeInstanceOf(DecisionUnavailableError);

    const timeoutError = new Error("aborted");
    timeoutError.name = "TimeoutError";
    const timedOut = new LayaProvider({
      transport: "http",
      sidecarEndpoint: "http://x",
      timeoutMs: 1000,
      fetchImpl: mockFetch(async () => {
        throw timeoutError;
      }) as unknown as typeof fetch,
    });
    await expect(timedOut.decide(request)).rejects.toBeInstanceOf(DecisionTimeoutError);
  });

  it("畸形答案整批拒绝：缺一半的批量决策比失败更难排查", async () => {
    const fetchImpl = mockFetch(async () => jsonResponse({ answers: { bad: { whatever: 1 } } }));
    const provider = new LayaProvider({
      transport: "http",
      sidecarEndpoint: "http://x",
      timeoutMs: 1000,
      fetchImpl: fetchImpl as unknown as typeof fetch,
    });
    await expect(provider.decide(request)).rejects.toBeInstanceOf(DecisionUnavailableError);
  });
});

describe("LayaProvider（embedded 探测）", () => {
  it("宿主装不上 @receptron/laya 时：isAvailable false，decide 报不可用", async () => {
    const provider = new LayaProvider({ transport: "embedded", timeoutMs: 1000 });
    expect(provider.isAvailable).toBe(false);
    await provider.warmup(); // 探测失败不抛——不可用是常态不是崩溃
    await expect(provider.decide(request)).rejects.toBeInstanceOf(DecisionUnavailableError);
  });
});

describe("JevProvider", () => {
  it("按 TypeSafe 规格发请求：Bearer、jev-latest、对象 state 序列化成字符串", async () => {
    const fetchImpl = mockFetch(async () => jsonResponse({ answers: { urgency: { score: 1.4 } } }));
    const provider = new JevProvider({
      apiKey: "sk-test",
      transport: "direct",
      timeoutMs: 1000,
      fetchImpl: fetchImpl as unknown as typeof fetch,
    });
    const result = await provider.decide(request);
    const [url, init] = fetchImpl.mock.calls[0];
    expect(url).toBe(JEV_ENDPOINTS.typesafe);
    expect((init.headers as Record<string, string>).Authorization).toBe("Bearer sk-test");
    const body = JSON.parse(init.body as string);
    expect(body.model).toBe("jev-latest");
    expect(body.state).toBe(JSON.stringify(request.state)); // Jev 的 state 只收文本
    expect(result.model).toBe("jev");
  });

  it("via=openrouter 换服务商；401 映射为不可用", async () => {
    const fetchImpl = mockFetch(async () => jsonResponse({}, 401));
    const provider = new JevProvider({
      apiKey: "sk-test",
      via: "openrouter",
      transport: "direct",
      timeoutMs: 1000,
      fetchImpl: fetchImpl as unknown as typeof fetch,
    });
    await expect(provider.decide(request)).rejects.toThrow("Jev HTTP 401");
    expect(fetchImpl.mock.calls[0][0]).toBe(JEV_ENDPOINTS.openrouter);
  });

  it("没有 apiKey 就不可用，也不会发出请求", async () => {
    const fetchImpl = mockFetch(async () => jsonResponse({}));
    const provider = new JevProvider({
      apiKey: "",
      transport: "direct",
      timeoutMs: 1000,
      fetchImpl: fetchImpl as unknown as typeof fetch,
    });
    expect(provider.isAvailable).toBe(false);
    await expect(provider.decide(request)).rejects.toBeInstanceOf(DecisionUnavailableError);
    expect(fetchImpl).not.toHaveBeenCalled();
  });

  it("warmup 发最小 noul ping", async () => {
    const fetchImpl = mockFetch(async () => jsonResponse({ answers: { alive: { noul: 0.9 } } }));
    const provider = new JevProvider({
      apiKey: "sk-test",
      transport: "direct",
      timeoutMs: 1000,
      fetchImpl: fetchImpl as unknown as typeof fetch,
    });
    await provider.warmup();
    const [, init] = fetchImpl.mock.calls[0];
    const body = JSON.parse(init.body as string);
    expect(body.questions.alive.type).toBe("noul");
  });
});

describe("JevProvider（决策池）", () => {
  const rustAnswers = {
    answers: {
      urgency: {
        type: "score",
        score: 1.2,
        probabilities: { low: 0.2, mid: 0.8 },
        confidence: 0.74,
      },
    },
  };
  const A = "https://a.example.com/v1/systemone";
  const B = "https://b.example.com/v1/systemone";

  function invokeRust(handler: (baseUrl: string) => Promise<unknown>) {
    return vi.fn(async (_command: string, args: Record<string, unknown>) =>
      handler((args.request as { baseUrl?: string }).baseUrl ?? ""),
    );
  }

  it("粘住的那条挂了换下一条：按顺序故障转移，各条带各条的钥匙", async () => {
    const invokeImpl = invokeRust(async (baseUrl) => {
      if (baseUrl === A) throw new Error("Jev HTTP 502");
      return rustAnswers;
    });
    const provider = new JevProvider({
      apiKey: "",
      via: "custom",
      transport: "rust",
      timeoutMs: 1000,
      invokeImpl,
      endpoints: [
        { name: "甲", baseUrl: A, apiKey: "sk-a" },
        { name: "乙", baseUrl: B, apiKey: "sk-b" },
      ],
    });
    // 全局密钥是空的，但条目自带钥匙也算有钥匙
    expect(provider.isAvailable).toBe(true);
    const result = await provider.decide(request);
    expect(result.model).toBe("jev");
    expect(invokeImpl).toHaveBeenCalledTimes(2);
    const second = invokeImpl.mock.calls[1][1].request as Record<string, unknown>;
    expect(second.baseUrl).toBe(B);
    expect(second.apiKey).toBe("sk-b");
  });

  it("服务商亲和：成功过的那条下一次排最前，不再撞坏的", async () => {
    let broken = true;
    const invokeImpl = invokeRust(async (baseUrl) => {
      if (baseUrl === A && broken) {
        broken = false;
        throw new Error("Jev HTTP 503");
      }
      return rustAnswers;
    });
    const provider = new JevProvider({
      apiKey: "sk-global",
      via: "custom",
      transport: "rust",
      timeoutMs: 1000,
      invokeImpl,
      endpoints: [
        { name: "甲", baseUrl: A },
        { name: "乙", baseUrl: B },
      ],
    });
    await provider.decide(request); // 甲挂 → 乙成功，亲和指向乙
    const before = invokeImpl.mock.calls.length;
    await provider.decide(request); // 直接乙，一发成功
    expect(invokeImpl.mock.calls.length - before).toBe(1);
    const sent = invokeImpl.mock.calls[before][1].request as Record<string, unknown>;
    expect(sent.baseUrl).toBe(B);
    // 条目没带钥匙时落到全局那把
    expect(sent.apiKey).toBe("sk-global");
  });

  it("全挂才报不可用，报错里点得出每条的名字与原因", async () => {
    const invokeImpl = invokeRust(async (baseUrl) => {
      throw new Error(baseUrl === A ? "Jev HTTP 502" : "Jev 请求超时");
    });
    const provider = new JevProvider({
      apiKey: "sk-global",
      via: "custom",
      transport: "rust",
      timeoutMs: 1000,
      invokeImpl,
      endpoints: [
        { name: "甲", baseUrl: A },
        { name: "乙", baseUrl: B },
      ],
    });
    await expect(provider.decide(request)).rejects.toThrow(/「甲」[\s\S]*「乙」/);
    await expect(provider.decide(request)).rejects.toThrow(/决策池 2 个服务商都没答上/);
  });

  it("形式不合法的条目被跳过：只要有一条能走，这一层就不缺席", async () => {
    const invokeImpl = invokeRust(async () => rustAnswers);
    const provider = new JevProvider({
      apiKey: "sk-global",
      via: "custom",
      transport: "rust",
      timeoutMs: 1000,
      invokeImpl,
      endpoints: [
        { name: "残", baseUrl: "https://bad.example.com" }, // 缺路径
        { name: "好", baseUrl: B },
      ],
    });
    expect(provider.isAvailable).toBe(true);
    await provider.decide(request);
    expect(invokeImpl).toHaveBeenCalledTimes(1);
    const sent = invokeImpl.mock.calls[0][1].request as Record<string, unknown>;
    expect(sent.baseUrl).toBe(B);
  });

  it("老配置没有 endpoints：单独那格 baseUrl 迁成池里唯一一条", async () => {
    const invokeImpl = invokeRust(async () => rustAnswers);
    const provider = new JevProvider({
      apiKey: "sk-global",
      via: "custom",
      baseUrl: "http://localhost:8787/v1/systemone",
      transport: "rust",
      timeoutMs: 1000,
      invokeImpl,
    });
    expect(provider.isAvailable).toBe(true);
    await provider.decide(request);
    expect(invokeImpl).toHaveBeenCalledTimes(1);
    const sent = invokeImpl.mock.calls[0][1].request as Record<string, unknown>;
    expect(sent.baseUrl).toBe("http://localhost:8787/v1/systemone");
  });

  it("direct 传输同样按池故障转移", async () => {
    const fetchImpl = mockFetch(async (url) => {
      if (url === A) return jsonResponse({}, 502);
      return jsonResponse(rustAnswers);
    });
    const provider = new JevProvider({
      apiKey: "sk-global",
      via: "custom",
      transport: "direct",
      timeoutMs: 1000,
      fetchImpl: fetchImpl as unknown as typeof fetch,
      endpoints: [
        { name: "甲", baseUrl: A },
        { name: "乙", baseUrl: B },
      ],
    });
    const result = await provider.decide(request);
    expect(result.model).toBe("jev");
    expect(fetchImpl.mock.calls[1][0]).toBe(B);
  });
});

describe("JevProvider（rust 传输）", () => {
  it("invoke decision_jev_system_one，参数结构与 decision.rs 的 JevDecisionRequest 对齐", async () => {
    const invokeImpl = vi.fn(async (_command: string, _args: Record<string, unknown>) => ({
      // Jev 的 system_one 与 Laya 同形：分布是 probabilities 表，confidence 由模型给
      answers: {
        urgency: {
          type: "score",
          score: 1.2,
          probabilities: { low: 0.2, mid: 0.8 },
          confidence: 0.74,
        },
      },
    }));
    const provider = new JevProvider({
      apiKey: "sk-test",
      transport: "rust",
      timeoutMs: 1000,
      invokeImpl,
    });
    const result = await provider.decide(request);
    expect(invokeImpl).toHaveBeenCalledTimes(1);
    const [command, args] = invokeImpl.mock.calls[0];
    expect(command).toBe("decision_jev_system_one");
    const inner = args.request as Record<string, unknown>;
    expect(inner.apiKey).toBe("sk-test");
    expect(inner.state).toBe(JSON.stringify(request.state)); // 序列化仍在前端做，原生只转发
    expect(inner.via).toBe("typesafe");
    expect(inner.timeoutMs).toBe(1000);
    expect(result.model).toBe("jev");
    expect(result.answers.urgency.confidence).toBe(0.74);
  });

  it("via=custom 把地址交给原生再校；切回内置厂商时那一格不再决定去向", async () => {
    const invokeImpl = vi.fn(async (_command: string, _args: Record<string, unknown>) => ({
      answers: {
        urgency: {
          type: "score",
          score: 1.2,
          probabilities: { low: 0.2, mid: 0.8 },
          confidence: 0.74,
        },
      },
    }));
    const custom = new JevProvider({
      apiKey: "sk-test",
      via: "custom",
      baseUrl: "http://localhost:8787/v1/systemone",
      transport: "rust",
      timeoutMs: 1000,
      invokeImpl,
    });
    await custom.decide(request);
    const sent = (invokeImpl.mock.calls[0][1] as Record<string, unknown>).request as Record<
      string,
      unknown
    >;
    expect(sent.via).toBe("custom");
    expect(sent.baseUrl).toBe("http://localhost:8787/v1/systemone");

    // 对照：地址那格留着旧内容，切回 TypeSafe 后它不该还跟着出去
    const back = new JevProvider({
      apiKey: "sk-test",
      via: "typesafe",
      baseUrl: "http://localhost:8787/v1/systemone",
      transport: "rust",
      timeoutMs: 1000,
      invokeImpl,
    });
    await back.decide(request);
    const second = (invokeImpl.mock.calls[1][1] as Record<string, unknown>).request as Record<
      string,
      unknown
    >;
    expect(second.via).toBe("typesafe");
    expect(second.baseUrl).toBeUndefined();
  });

  it("自定义地址不合法：这一层不可用，理由说得出，且一发都不发", async () => {
    const invokeImpl = vi.fn(async () => ({ answers: {} }));
    const provider = new JevProvider({
      apiKey: "sk-test",
      via: "custom",
      baseUrl: "http://api.example.com/v1/systemone", // 明文出外网
      transport: "rust",
      timeoutMs: 1000,
      invokeImpl,
    });
    expect(provider.isAvailable).toBe(false);
    await expect(provider.decide(request)).rejects.toThrow("http 只允许本机");
    expect(invokeImpl).not.toHaveBeenCalled();
  });

  it("原生错误原样透传：『Jev HTTP 401』不再二次包装", async () => {
    const invokeImpl = vi.fn(async (_command: string, _args: Record<string, unknown>) => {
      throw new Error("Jev HTTP 401");
    });
    const provider = new JevProvider({
      apiKey: "sk-test",
      transport: "rust",
      timeoutMs: 1000,
      invokeImpl,
    });
    await expect(provider.decide(request)).rejects.toThrow("Jev HTTP 401");
  });

  it("出口名单拦截的原文同样透传成不可用", async () => {
    const invokeImpl = vi.fn(async (_command: string, _args: Record<string, unknown>) => {
      throw new Error("出口被拦下：api.typesafe.ai 不在网络出口的域名名单里。");
    });
    const provider = new JevProvider({
      apiKey: "sk-test",
      transport: "rust",
      timeoutMs: 1000,
      invokeImpl,
    });
    await expect(provider.decide(request)).rejects.toBeInstanceOf(DecisionUnavailableError);
  });
});

describe("JevProvider（keyring 模式）", () => {
  it("显式 key 留空、探测喂进可用后才可用；请求里 apiKey 传空由 Rust 侧自取", async () => {
    const invokeImpl = vi.fn(async (command: string, _args: Record<string, unknown>) => {
      if (command === "decision_jev_key_state") return true;
      return { answers: { urgency: { score: 1.0 } } };
    });
    const provider = new JevProvider({
      apiKey: "",
      useKeyring: true,
      transport: "rust",
      timeoutMs: 1000,
      invokeImpl,
    });
    expect(provider.isAvailable).toBe(false); // 探测回来之前不可用：宁可跳过一层也不空发
    provider.setKeyringAvailable(true);
    expect(provider.isAvailable).toBe(true);
    await provider.decide(request);
    const [, args] = invokeImpl.mock.calls.at(-1) as unknown as [string, Record<string, unknown>];
    expect((args.request as Record<string, unknown>).apiKey).toBe("");
  });

  it("keyring 没条目：保持不可用，decide 直接报未配置", async () => {
    const invokeImpl = vi.fn(async (command: string, _args: Record<string, unknown>) => {
      if (command === "decision_jev_key_state") return false;
      throw new Error("不该发请求");
    });
    const provider = new JevProvider({
      apiKey: "",
      useKeyring: true,
      transport: "rust",
      timeoutMs: 1000,
      invokeImpl,
    });
    provider.setKeyringAvailable(false);
    await expect(provider.decide(request)).rejects.toBeInstanceOf(DecisionUnavailableError);
  });
});

describe("NullFallbackProvider", () => {
  it("Phase 1 明确不可用，绝不伪造答案", async () => {
    const provider = new NullFallbackProvider();
    expect(provider.isAvailable).toBe(false);
    await expect(provider.decide(request)).rejects.toBeInstanceOf(DecisionUnavailableError);
  });
});
