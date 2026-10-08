import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";

import { cn } from "@/lib/utils";

/**
 * Agent 子进程体检卡（M1/M2 地基的诊断面）：设置页里当场验证
 * "agent-host 活着没有、协议通不通、ev 事件通道顺序对不对"。
 * 两发命令打的是**真的子进程**——通过的那一秒，蓝图 §A 的地基就是运行事实。
 */

interface ProbeResult {
  pong: boolean;
  pid: number;
  uptimeSecs: number;
  served: number;
  fence: number;
  restarts: number;
}

interface StreamResult {
  requested: number;
  delivered: number;
  received: number;
  inOrder: boolean;
  events: Array<{ i: number }>;
}

const PROBE_ERROR = "体检没跑成" as const;

export function AgentDiagnosticsCard() {
  const [probe, setProbe] = useState<ProbeResult | null>(null);
  const [stream, setStream] = useState<StreamResult | null>(null);
  const [busy, setBusy] = useState<false | "probe" | "stream">(false);
  const [error, setError] = useState<string | null>(null);

  async function run(kind: "probe" | "stream") {
    setBusy(kind);
    setError(null);
    try {
      if (kind === "probe") {
        setProbe(await invoke<ProbeResult>("agent_probe"));
      } else {
        setStream(await invoke<StreamResult>("agent_stream_check", { count: 3 }));
      }
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setBusy(false);
    }
  }

  const verdict = stream
    ? stream.inOrder && stream.received === stream.delivered
      ? "通过"
      : "乱序/丢帧"
    : null;

  return (
    <div className="mt-3 rounded-lg border border-border bg-surface px-3 py-3">
      <div className="flex items-center justify-between gap-3">
        <div className="min-w-0">
          <p className="text-sm font-medium text-foreground">Agent 子进程</p>
          <p className="mt-1 text-xs leading-5 text-muted-foreground">
            agent-host 是蓝图 §A 的独立业务进程：体检打 ping/状态，事件通道检查发
            stream.demo 验证 ev 信封按序到达。两条都过，说明 IPC 地基是运行事实。
          </p>
        </div>
        <div className="flex shrink-0 gap-2">
          <button
            type="button"
            disabled={busy !== false}
            onClick={() => void run("probe")}
            className={cn(
              "rounded-lg border border-border px-3 py-1.5 text-xs outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
              busy === "probe"
                ? "text-muted-foreground/50"
                : "text-foreground hover:border-brand/40",
            )}
          >
            {busy === "probe" ? "体检中…" : "体检"}
          </button>
          <button
            type="button"
            disabled={busy !== false}
            onClick={() => void run("stream")}
            className={cn(
              "rounded-lg border border-border px-3 py-1.5 text-xs outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
              busy === "stream"
                ? "text-muted-foreground/50"
                : "text-foreground hover:border-brand/40",
            )}
          >
            {busy === "stream" ? "检查中…" : "事件通道"}
          </button>
        </div>
      </div>

      {error ? (
        <p className="mt-2 text-xs text-destructive">
          {PROBE_ERROR}：{error}
        </p>
      ) : null}

      {probe ? (
        <div className="mt-2 grid grid-cols-3 gap-x-4 gap-y-1 font-mono text-xs text-muted-foreground">
          <span>pid · {probe.pid}</span>
          <span>存活 · {probe.uptimeSecs}s</span>
          <span>已答帧 · {probe.served}</span>
          <span>fence · {probe.fence}</span>
          <span>重启 · {probe.restarts}</span>
          <span className={probe.pong ? "text-emerald-500" : "text-red-400"}>
            ping · {probe.pong ? "通" : "不通"}
          </span>
        </div>
      ) : null}

      {stream ? (
        <div className="mt-2 font-mono text-xs">
          <span className={verdict === "通过" ? "text-emerald-500" : "text-red-400"}>
            ev 通道 · {verdict}
          </span>
          <span className="ml-3 text-muted-foreground">
            请求 {stream.requested} · 到达 {stream.received} · 终答 {stream.delivered}
          </span>
        </div>
      ) : null}
    </div>
  );
}
