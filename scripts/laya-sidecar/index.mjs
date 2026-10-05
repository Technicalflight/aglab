/**
 * aglab System 1 决策层 · Laya 本地推理 sidecar
 *
 * 为什么存在：@receptron/laya 依赖 onnxruntime-node（Node 20+），跑不进 Tauri 的
 * WebView。决策层（src/lib/decision）通过 HTTP 调这个进程拿到本地推理能力，
 * 模型与 1.7GB 权重都留在本机，敏感数据不出门。
 *
 * 启动（首次会从 HuggingFace 下载权重，缓存到 ~/.cache/receptron-laya）：
 *   cd scripts/laya-sidecar
 *   npm i @receptron/laya
 *   npm start            # 默认 127.0.0.1:8787
 *   PORT=9000 npm start  # 换端口时同步改 config 里 laya.sidecarEndpoint
 *
 * 契约（与 decision 层 LayaProvider 对齐，也兼容 python laya-mlx 同形服务）：
 *   GET  /health      → 200 { ok, loaded, loading }
 *   POST /systemOne   → body { state: string|object, questions }
 *                     → 200 { answers: { [name]: {choice|score|noul, ...} } }
 *   OPTIONS *         → 预检：204（源在白名单里）/ 403
 *
 * CORS：调用方是 Tauri 的 WebView，它就是一个浏览器——POST 带 content-type:
 * application/json 属于非简单请求，没有这份头连预检都过不去，Laya 在 app 里
 * 就永远是「layer unavailable」。白名单见 ALLOWED_ORIGINS。
 */
import http from "node:http";
import process from "node:process";

const PORT = Number(process.env.PORT ?? 8787);

/**
 * 允许读响应的浏览器上下文。只听 127.0.0.1 不等于谁都能读：本机任意网页都能朝这个
 * 端口 POST 一段文本再把判定读走，白名单挡的就是这一类。不带 Origin 的请求
 * （curl、Node、未来的 laya-mlx 客户端）照常放行——要连它们一起挡得加共享令牌，
 * 那是另一件事，别和 CORS 混为一谈。
 */
const ALLOWED_ORIGINS = new Set([
  "tauri://localhost", // 打包后的 macOS/Linux 源
  "http://tauri.localhost", // 打包后的 Windows 源
  "http://localhost:1420", // tauri dev（vite 的端口，见 vite.config.ts strictPort）
  "http://127.0.0.1:1420",
]);
for (const extra of (process.env.LAYA_ALLOWED_ORIGINS ?? "").split(",")) {
  const origin = extra.trim();
  if (origin) ALLOWED_ORIGINS.add(origin);
}

/** 这个源能不能读响应。空 Origin（非浏览器）= 放行 */
function originAllowed(origin) {
  return !origin || ALLOWED_ORIGINS.has(origin);
}

/** Vary: Origin 不是礼节：漏了它，中间缓存可能把给 a 源的响应发给 b 源 */
function corsHeaders(origin) {
  return origin && ALLOWED_ORIGINS.has(origin)
    ? { "access-control-allow-origin": origin, vary: "Origin" }
    : { vary: "Origin" };
}

function json(res, status, body, origin) {
  res.writeHead(status, { "content-type": "application/json", ...corsHeaders(origin) });
  res.end(body === undefined ? "" : JSON.stringify(body));
}

/** 懒加载 + 单例：权重 1.7GB，进程里只许存在一份；首个请求承担冷启动 */
let layaInstance = null;
let loading = null;

async function getLaya() {
  if (layaInstance) return layaInstance;
  loading ??= (async () => {
    const { Laya } = await import("@receptron/laya");
    layaInstance = await Laya.load({
      subfolder: process.env.LAYA_SUBFOLDER ?? "multilingual",
      executionProviders: ["cpu"],
    });
    return layaInstance;
  })();
  try {
    return await loading;
  } catch (error) {
    loading = null; // 允许下次重试（例如权重没下完就断了电）
    throw error;
  }
}

function readBody(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    req.on("data", (chunk) => chunks.push(chunk));
    req.on("end", () => resolve(Buffer.concat(chunks).toString("utf8")));
    req.on("error", reject);
  });
}

const server = http.createServer(async (req, res) => {
  const origin = req.headers.origin ?? "";
  if (req.method === "OPTIONS") {
    // 预检只问"待会儿那一发你答不答应"，答应的内容全在头里，正文留空
    if (originAllowed(origin)) {
      res.writeHead(204, {
        ...corsHeaders(origin),
        "access-control-allow-methods": "GET, POST, OPTIONS",
        "access-control-allow-headers": "content-type",
        "access-control-max-age": "600",
      });
      res.end();
    } else {
      json(res, 403, { error: `源不在白名单里：${origin}` });
    }
    return;
  }
  if (!originAllowed(origin)) {
    json(res, 403, { error: `源不在白名单里：${origin}` });
    return;
  }
  if (req.method === "GET" && req.url === "/health") {
    // loaded 与 loading 分开报：决策面板要区分「没热过」和「热着」
    json(res, 200, { ok: true, loaded: !!layaInstance, loading: !!loading }, origin);
    return;
  }
  if (req.method === "POST" && req.url === "/systemOne") {
    try {
      const body = JSON.parse((await readBody(req)) || "{}");
      if (body.state === undefined || typeof body.questions !== "object") {
        json(res, 400, { error: "需要 state 与 questions 两个字段" }, origin);
        return;
      }
      const laya = await getLaya();
      const result = await laya.systemOne(body.state, body.questions);
      json(res, 200, result, origin);
    } catch (error) {
      json(res, 503, { error: String(error?.message ?? error) }, origin);
    }
    return;
  }
  json(res, 404, { error: "只有 /health 与 /systemOne" }, origin);
});

server.listen(PORT, "127.0.0.1", () => {
  // PORT=0 时内核挑端口，报实际听到的那个而不是配置值——测试靠这一行拿到端点，
  // 报错了就是让人对一个没人听的端口发起决策
  const bound = server.address();
  const port = typeof bound === "object" && bound ? bound.port : PORT;
  console.log(`[laya-sidecar] listening on http://127.0.0.1:${port} (weights load on first request)`);
});

// 优雅退出：把 1.7GB 的会话还回去
for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, async () => {
    server.close();
    try {
      await layaInstance?.close?.();
    } finally {
      process.exit(0);
    }
  });
}
