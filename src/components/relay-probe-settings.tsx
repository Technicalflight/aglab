import { useCallback, useEffect, useState } from "react";
import {
  IconAlertTriangle as AlertTriangle,
  IconCheck as Check,
  IconChevronDown as ChevronDown,
  IconRadar as Radar,
} from "@tabler/icons-react";

import { Button } from "@/components/ui/button";
import { SettingsHeader } from "@/components/settings-ui";
import { fetchProbeHistory, runProbe, type ProbeReport, type ProbeSignal } from "@/lib/chat-transport";
import { useChatStore } from "@/store/chat-store";
import { cn } from "@/lib/utils";

type Claimed = "openai" | "anthropic" | "gemini";
type Depth = "quick" | "deep";

const CLAIMED_LABEL: Record<Claimed, string> = {
  openai: "OpenAI（官方直连特征）",
  anthropic: "Anthropic（官方直连特征）",
  gemini: "Gemini（官方直连特征）",
};

const SIGNAL_META: Record<string, { label: string; group: "inject" | "target" }> = {
  canary_echo: { label: "金丝雀回显", group: "inject" },
  token_anomaly: { label: "Token 用量对比", group: "inject" },
  schema_integrity: { label: "响应结构完整性", group: "inject" },
  prompt_leak: { label: "提示词泄漏", group: "inject" },
  instruction_override: { label: "指令覆写", group: "inject" },
  header_fingerprint: { label: "响应头指纹", group: "target" },
  model_self_id: { label: "模型自述身份", group: "target" },
};

function severityIcon(severity: ProbeSignal["severity"]) {
  if (severity === "pass") {
    return <Check className="size-3.5 text-emerald-400" />;
  }
  return <AlertTriangle className={cn("size-3.5", severity === "fail" ? "text-destructive" : "text-amber-400")} />;
}

function verdictTone(verdict: string) {
  if (verdict === "可信") return "border-emerald-500/50 bg-emerald-500/10 text-emerald-300";
  if (verdict === "存疑") return "border-amber-500/50 bg-amber-500/10 text-amber-300";
  return "border-destructive/50 bg-destructive/10 text-destructive";
}

function SignalRow({ signal }: { signal: ProbeSignal }) {
  const [open, setOpen] = useState(false);
  const meta = SIGNAL_META[signal.key] ?? { label: signal.key, group: "inject" as const };
  return (
    <div className="rounded-lg border border-border bg-surface/60">
      <button
        type="button"
        onClick={() => setOpen(!open)}
        className="flex w-full items-center gap-2.5 px-3 py-2.5 text-left"
      >
        {severityIcon(signal.severity)}
        <span className="min-w-0 flex-1 truncate text-sm text-foreground">{meta.label}</span>
        <span className="shrink-0 font-mono text-2xs text-muted-foreground">
          置信 {Math.round(signal.confidence * 100)}%
        </span>
        <ChevronDown className={cn("size-3.5 shrink-0 text-muted-foreground transition-transform", open && "rotate-180")} />
      </button>
      {open ? (
        <p className="whitespace-pre-wrap border-t border-border px-3 py-2 text-xs leading-5 text-muted-foreground">
          {signal.evidence}
        </p>
      ) : null}
    </div>
  );
}

function ReportView({ report }: { report: ProbeReport }) {
  const inject = report.signals.filter((s) => (SIGNAL_META[s.key]?.group ?? "inject") === "inject");
  const target = report.signals.filter((s) => SIGNAL_META[s.key]?.group === "target");
  return (
    <div className="space-y-4">
      {/* 综合评估横幅：诚实措辞——异常信号是线索不是判决 */}
      <div className={cn("flex items-center gap-4 rounded-xl border px-4 py-3", verdictTone(report.verdict))}>
        <div className="flex size-14 shrink-0 flex-col items-center justify-center rounded-lg border border-current/30 bg-background/60">
          <span className="text-lg font-bold leading-none tabular-nums">{report.score}</span>
          <span className="text-2xs opacity-80">置信</span>
        </div>
        <div className="min-w-0">
          <p className="text-sm font-semibold">综合评估：{report.verdict}</p>
          <p className="mt-0.5 text-xs leading-5 opacity-90">
            检测到{" "}
            {report.signals.filter((s) => s.severity !== "pass").length}{" "}
            项异常信号（{report.signals.length} 项中）。探针只能收集证据，建议结合多项信号进一步核实，不构成结论。
          </p>
        </div>
      </div>

      <div className="grid grid-cols-1 gap-3 lg:grid-cols-2">
        <div className="space-y-2">
          <p className="flex items-center gap-1.5 text-xs font-semibold text-muted-foreground">
            <span>🛡️</span> 注入检测
          </p>
          {inject.map((signal) => (
            <SignalRow key={signal.key} signal={signal} />
          ))}
        </div>
        <div className="space-y-2">
          <p className="flex items-center gap-1.5 text-xs font-semibold text-muted-foreground">
            <span>🎯</span> 目标验证
          </p>
          {target.map((signal) => (
            <SignalRow key={signal.key} signal={signal} />
          ))}
        </div>
      </div>

      <p className="text-2xs leading-4 text-muted-foreground">
        探测目标 {report.baseUrl} · 模型 {report.model} · 声称 {CLAIMED_LABEL[report.claimed as Claimed] ?? report.claimed} ·{" "}
        {report.depth === "deep" ? "深度探测（4 次请求）" : "快速探测（1 次请求）"}
      </p>
    </div>
  );
}

export function RelayProbeSettings() {
  const config = useChatStore((s) => s.config);
  const [claimed, setClaimed] = useState<Claimed>("openai");
  const [depth, setDepth] = useState<Depth>("quick");
  const [profileId, setProfileId] = useState<string>("");
  const [running, setRunning] = useState(false);
  const [report, setReport] = useState<ProbeReport | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [history, setHistory] = useState<ProbeReport[]>([]);
  const [historyOpen, setHistoryOpen] = useState(false);

  const loadHistory = useCallback(() => {
    void (async () => {
      try {
        setHistory(await fetchProbeHistory());
      } catch {
        setHistory([]);
      }
    })();
  }, []);

  useEffect(() => {
    loadHistory();
  }, [loadHistory]);

  async function run() {
    setRunning(true);
    setError(null);
    try {
      const next = await runProbe({ claimed, depth, profileId: profileId || undefined });
      setReport(next);
      loadHistory();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setRunning(false);
    }
  }

  return (
    <div>
      <SettingsHeader
        title="中转站探针"
        description="向当前服务商发送少量探测请求，收集证据帮你判断两件事：请求是否被注入额外内容、声称的目标是否与实际响应特征相符。所有结论都标明置信度，探针不改变你的正常对话。"
        action={<Radar className="size-5 text-muted-foreground" />}
      />

      <div className="mt-6 space-y-4">
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
          <label className="block">
            <span className="mb-1.5 block text-xs text-muted-foreground">声称的目标（中转宣称接的是谁）</span>
            <select
              value={claimed}
              onChange={(event) => setClaimed(event.target.value as Claimed)}
              className="h-9 w-full rounded-lg border border-input bg-background px-3 text-sm text-foreground outline-none focus-visible:border-brand/50"
            >
              {(Object.keys(CLAIMED_LABEL) as Claimed[]).map((key) => (
                <option key={key} value={key}>
                  {CLAIMED_LABEL[key]}
                </option>
              ))}
            </select>
          </label>
          <label className="block">
            <span className="mb-1.5 block text-xs text-muted-foreground">对照的服务商档案（可选）</span>
            <select
              value={profileId}
              onChange={(event) => setProfileId(event.target.value)}
              className="h-9 w-full rounded-lg border border-input bg-background px-3 text-sm text-foreground outline-none focus-visible:border-brand/50"
            >
              <option value="">当前连接</option>
              {config.profiles.map((profile) => (
                <option key={profile.id} value={profile.id}>
                  {profile.name}
                </option>
              ))}
            </select>
          </label>
        </div>

        <div className="flex flex-wrap items-center gap-2">
          <Button onClick={() => void run()} loading={running}>
            {running ? "探测中…" : null}
            {depth === "quick" ? "快速探测（1 次请求，秒出）" : "深度探测（4 次请求，耗少量 token）"}
          </Button>
          <Button
            variant="subtle"
            size="sm"
            onClick={() => setDepth(depth === "quick" ? "deep" : "quick")}
            disabled={running}
          >
            {depth === "quick" ? "切到深度探测" : "切到快速探测"}
          </Button>
          <p className="text-2xs leading-4 text-muted-foreground">
            快速 = 头指纹 + 金丝雀 + token 对比 + 结构校验；深度追加提示词泄漏、指令覆写与模型自述
          </p>
        </div>

        {error ? (
          <p className="rounded-lg border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs leading-5 text-destructive">
            {error}
          </p>
        ) : null}

        {report ? <ReportView report={report} /> : null}

        {history.length > 0 ? (
          <div className="rounded-xl border border-border">
            <button
              type="button"
              onClick={() => setHistoryOpen(!historyOpen)}
              className="flex w-full items-center justify-between px-3 py-2.5 text-left text-xs font-semibold text-foreground"
            >
              历史对比（{history.length} 次）
              <ChevronDown className={cn("size-3.5 transition-transform", historyOpen && "rotate-180")} />
            </button>
            {historyOpen ? (
              <ul className="divide-y divide-border border-t border-border">
                {history.map((item, index) => (
                  <li key={`${item.finishedAt}-${index}`} className="flex items-center gap-3 px-3 py-2 text-xs">
                    <span className="font-mono text-2xs text-muted-foreground">{item.finishedAt.slice(0, 19).replace("T", " ")}</span>
                    <span className="text-muted-foreground">{item.model}</span>
                    <span className="ml-auto font-semibold tabular-nums">{item.score}</span>
                    <span className="w-10 text-right text-muted-foreground">{item.verdict}</span>
                  </li>
                ))}
              </ul>
            ) : null}
          </div>
        ) : null}
      </div>
    </div>
  );
}
