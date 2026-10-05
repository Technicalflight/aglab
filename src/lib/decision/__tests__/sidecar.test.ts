/**
 * 真起参考实现那份 sidecar（scripts/laya-sidecar/index.mjs），验它对浏览器上下文的 CORS 口径。
 *
 * 为什么这条不许换成自己的桩：Node 的 fetch 根本不执行 CORS，桩写得再像也测不出缺失——
 * "Laya 的 http 传输是 WebView 主路径"那句话就是这么一路绿着错过去的。被测的必须是
 * 交付给用户去跑的那一份文件，包括它到底在哪个端口上听。
 */
import { spawn, type ChildProcess } from "node:child_process";
import http from "node:http";
import { fileURLToPath } from "node:url";

import { afterAll, beforeAll, describe, expect, it } from "vitest";

const SIDECAR = fileURLToPath(new URL("../../../../scripts/laya-sidecar/index.mjs", import.meta.url));

interface RawReply {
  status: number;
  headers: http.IncomingHttpHeaders;
}

/** 裸 HTTP：Origin 要能被浏览器那样发出去，也要能干脆不发。fetch 在这一点上不如它直白 */
function raw(port: number, method: string, path: string, headers: Record<string, string>): Promise<RawReply> {
  return new Promise((resolve, reject) => {
    const req = http.request({ host: "127.0.0.1", port, method, path, headers }, (res) => {
      res.resume(); // 响应体不读也要抽干，否则连接挂着
      res.on("end", () => resolve({ status: res.statusCode ?? 0, headers: res.headers }));
    });
    req.on("error", reject);
    req.end();
  });
}

/** 起一个 sidecar 进程，等它把实际端口打进 stdout（PORT=0 时端口是内核挑的） */
function launch(env: Record<string, string> = {}): Promise<{ port: number; stop: () => void }> {
  return new Promise((resolve, reject) => {
    const child: ChildProcess = spawn(process.execPath, [SIDECAR], {
      env: { ...process.env, PORT: "0", ...env },
      stdio: ["ignore", "pipe", "pipe"],
    });
    let settled = false;
    const timer = setTimeout(() => {
      if (settled) return;
      settled = true;
      child.kill();
      reject(new Error("sidecar 三秒内没报出监听地址"));
    }, 5000);
    child.stdout?.setEncoding("utf8");
    child.stdout?.on("data", (chunk: string) => {
      const found = chunk.match(/listening on http:\/\/127\.0\.0\.1:(\d+)/);
      if (!found || settled) return;
      settled = true;
      clearTimeout(timer);
      const port = Number(found[1]);
      resolve({
        port,
        stop: () => child.kill(),
      });
    });
    child.on("exit", (code) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      reject(new Error(`sidecar 提前退出了，退出码 ${code}`));
    });
  });
}

describe("参考实现 sidecar 的 CORS", () => {
  let sidecar: { port: number; stop: () => void };

  beforeAll(async () => {
    sidecar = await launch();
  });

  afterAll(() => sidecar?.stop());

  it("两个本机源各自回显，不给通配", async () => {
    for (const origin of ["http://tauri.localhost", "http://localhost:1420", "tauri://localhost"]) {
      const reply = await raw(sidecar.port, "GET", "/health", { origin });
      expect(reply.status, origin).toBe(200);
      // 回显而不是 *：名单外的源要拿不到，* 就等于对任何网页敞开读走判定
      expect(reply.headers["access-control-allow-origin"], origin).toBe(origin);
      expect(reply.headers.vary, origin).toContain("Origin");
    }
  });

  it("陌生源拿不到许可头（浏览器随即把响应拦下）", async () => {
    const reply = await raw(sidecar.port, "GET", "/health", { origin: "https://evil.example" });
    expect(reply.status).toBe(403);
    expect(reply.headers["access-control-allow-origin"]).toBeUndefined();
  });

  it("预检放行 POST 与 content-type", async () => {
    const reply = await raw(sidecar.port, "OPTIONS", "/systemOne", {
      origin: "http://tauri.localhost",
      "access-control-request-method": "POST",
      "access-control-request-headers": "content-type",
    });
    // POST 带 json content-type 属于非简单请求：这一格少了，WebView 连一发都发不出去
    expect(reply.status).toBe(204);
    expect(reply.headers["access-control-allow-origin"]).toBe("http://tauri.localhost");
    expect(String(reply.headers["access-control-allow-methods"])).toContain("POST");
    expect(String(reply.headers["access-control-allow-headers"])).toContain("content-type");
  });

  it("不带 Origin 的客户端（curl / Node）仍然能读", async () => {
    const reply = await raw(sidecar.port, "GET", "/health", {});
    expect(reply.status).toBe(200);
    // 没有源可比对，就不该有 ACAO——它此刻不是被 CORS 保护的请求
    expect(reply.headers["access-control-allow-origin"]).toBeUndefined();
  });
});

describe("sidecar 的名单可以按环境扩", () => {
  it("LAYA_ALLOWED_ORIGINS 追加的源能读响应", async () => {
    const extra = await launch({ LAYA_ALLOWED_ORIGINS: "https://lab.example, https://other.example" });
    try {
      const allowed = await raw(extra.port, "GET", "/health", { origin: "https://lab.example" });
      expect(allowed.status).toBe(200);
      expect(allowed.headers["access-control-allow-origin"]).toBe("https://lab.example");
      // 逗号分隔的第二条同样算数，不是只认第一个
      const also = await raw(extra.port, "GET", "/health", { origin: "https://other.example" });
      expect(also.headers["access-control-allow-origin"]).toBe("https://other.example");
      // 内置那几个不能被追加动作顶掉
      const builtin = await raw(extra.port, "GET", "/health", { origin: "http://tauri.localhost" });
      expect(builtin.status).toBe(200);
    } finally {
      extra.stop();
    }
  });
});
