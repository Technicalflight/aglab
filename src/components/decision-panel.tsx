import { useEffect, useMemo, useState, useSyncExternalStore } from "react";
import { open as pickFolder } from "@tauri-apps/plugin-dialog";
import { IconPlayerPlay as Play, IconEraser as Eraser, IconSquare as Square } from "@tabler/icons-react";

import { Button } from "@/components/ui/button";
import {
  confidenceBuckets,
  confidenceBucketIndex,
  decisionMonitor,
  getMetrics,
  getMinConfidence,
  groupBySignature,
  latencyBuckets,
  minConfidenceOf,
  routingByTaskType,
  signatureLabel,
  summarizeTraces,
  summarizeV2Metrics,
  traceSignature,
  tracesForConversation,
} from "@/lib/decision";
import type { DecisionAttempt, DecisionTrace, Question } from "@/lib/decision";
import { useChatStore } from "@/store/chat-store";
import { useDecisionStore } from "@/store/decision-store";
import { cn } from "@/lib/utils";

function formatMs(value: number): string {
  if (value >= 1000) return `${(value / 1000).toFixed(value >= 10_000 ? 1 : 2)}s`;
  return `${Math.round(value)}ms`;
}

function formatRatio(value: number): string {
  return value.toFixed(2);
}

function formatTime(iso: string): string {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return iso;
  const pad = (value: number) => String(value).padStart(2, "0");
  return `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`;
}

function StatCard({
  label,
  value,
  hint,
  tone = "plain",
}: {
  label: string;
  value: string;
  hint?: string;
  tone?: "plain" | "good" | "warn" | "bad";
}) {
  return (
    <div className="rounded-lg border border-border bg-surface px-3 py-2.5">
      <p className="text-xs text-muted-foreground">{label}</p>
      <p
        className={cn(
          "mt-1 truncate text-lg font-medium",
          tone === "good" && "text-foreground",
          tone === "warn" && "text-warning",
          tone === "bad" && "text-destructive",
          tone === "plain" && "text-foreground",
        )}
      >
        {value}
      </p>
      {hint ? <p className="mt-0.5 truncate text-xs text-muted-foreground">{hint}</p> : null}
    </div>
  );
}

/** 纯 CSS 横条：分桶数据不值得为它引一个图表库 */
function Bars({
  title,
  buckets,
  note,
  highlight,
}: {
  title: string;
  buckets: Array<{ label: string; count: number }>;
  note?: string;
  /** 高亮第几格（阈值落点）。它是刻度，不是"这格有问题"的意思 */
  highlight?: number;
}) {
  const max = Math.max(...buckets.map((bucket) => bucket.count), 1);
  const total = buckets.reduce((acc, bucket) => acc + bucket.count, 0);
  return (
    <div className="mt-5">
      <div className="flex items-baseline justify-between gap-2">
        <h2 className="text-base font-semibold tracking-tight text-foreground">{title}</h2>
        <span className="text-xs text-muted-foreground">
          {total === 0 ? "还没有数据" : (note ?? `${total} 次`)}
        </span>
      </div>
      <div className="mt-2 space-y-1 rounded-lg border border-border bg-surface px-3 py-2.5">
        {buckets.map((bucket, index) => (
          <div key={bucket.label} className="flex items-center gap-2">
            <span
              className={cn(
                "w-[74px] shrink-0 text-right text-xs tabular-nums",
                index === highlight ? "text-foreground" : "text-muted-foreground",
              )}
            >
              {bucket.label}
              {index === highlight ? " ←" : ""}
            </span>
            <div className="h-2.5 min-w-0 flex-1 rounded-sm bg-elevated">
              <div
                className="h-full rounded-sm bg-foreground/25 transition-[width]"
                style={{ width: `${max > 0 ? (bucket.count / max) * 100 : 0}%` }}
              />
            </div>
            <span className="w-8 shrink-0 text-right text-xs text-muted-foreground tabular-nums">
              {bucket.count}
            </span>
          </div>
        ))}
      </div>
    </div>
  );
}

function sensitivityTone(level: string): string {
  if (level === "confidential") return "text-destructive";
  if (level === "private") return "text-warning";
  return "text-muted-foreground";
}

const ATTEMPT_LABELS: Record<DecisionAttempt["outcome"], string> = {
  answered: "已答",
  "below-threshold": "未达阈值",
  failed: "失败",
  unavailable: "不可用",
  excluded: "没进候选",
  "not-reached": "没轮到",
};

function attemptTone(outcome: DecisionAttempt["outcome"]): string {
  if (outcome === "failed" || outcome === "unavailable") return "text-destructive";
  if (outcome === "below-threshold") return "text-warning";
  if (outcome === "answered") return "text-foreground";
  return "text-muted-foreground";
}

/** 漏斗里的一层这一趟的下场 */
function AttemptRow({ attempt }: { attempt: DecisionAttempt }) {
  return (
    <div className="flex flex-wrap items-baseline gap-x-2 text-xs">
      <span className="font-mono text-foreground">{attempt.tier}</span>
      <span className={attemptTone(attempt.outcome)}>{ATTEMPT_LABELS[attempt.outcome]}</span>
      <span className="text-muted-foreground tabular-nums">
        {attempt.latencyMs > 0 ? `${attempt.latencyMs}ms` : "—"}
      </span>
      {attempt.minConfidence !== undefined ? (
        <span className="text-muted-foreground tabular-nums">
          最小置信 {formatRatio(attempt.minConfidence)}
        </span>
      ) : null}
      {attempt.reason ? (
        <span className="min-w-0 flex-1 break-words text-muted-foreground">{attempt.reason}</span>
      ) : null}
    </div>
  );
}

/** 这一次判定问出去的问题：问题文本与档位是代码写死的，不是用户正文，脱敏后仍可看 */
function QuestionRow({ name, question }: { name: string; question: Question }) {
  const options =
    question.type === "noul"
      ? []
      : Array.isArray(question.criteria)
        ? question.criteria.map(String)
        : Object.keys(question.criteria);
  return (
    <div className="text-xs">
      <div className="flex flex-wrap items-baseline gap-x-2">
        <span className="font-mono text-foreground">{name}</span>
        <span className="text-muted-foreground">{question.type}</span>
      </div>
      <p className="break-words text-muted-foreground">{question.instructions}</p>
      {options.length > 0 ? (
        <p className="break-words text-2xs text-muted-foreground/80">档位：{options.join(" / ")}</p>
      ) : null}
    </div>
  );
}

/** 一条决策的展开体：逐层过程 → 问了什么 → 逐答案的原始读数，不替模型圆场 */
function TraceDetail({ trace }: { trace: DecisionTrace }) {
  const answers = trace.response ? Object.values(trace.response.answers) : [];
  // 环形缓冲里可能有这次改造之前落下的读数（开发期热更新后尤其如此）：那一格没有过程
  const attempts = trace.attempts ?? [];
  return (
    <div className="space-y-2.5 px-3 pb-2.5">
      <div className="space-y-1">
        <p className="text-2xs tracking-[0.08em] text-foreground-tertiary uppercase">逐层过程</p>
        {attempts.length === 0 ? (
          <p className="text-xs text-muted-foreground">
            {trace.cacheHit ? "缓存命中：这一次一层都没问。" : "这条没有逐层记录。"}
          </p>
        ) : (
          attempts.map((attempt) => <AttemptRow key={attempt.tier} attempt={attempt} />)
        )}
      </div>

      <div className="space-y-1.5">
        <p className="text-2xs tracking-[0.08em] text-foreground-tertiary uppercase">问了什么</p>
        {Object.entries(trace.request.questions).map(([name, question]) => (
          <QuestionRow key={name} name={name} question={question} />
        ))}
      </div>

      {!trace.response ? (
        <p className="text-xs leading-5 text-destructive">
          没有任何一层给出答案。{trace.errors ? `原因：${trace.errors}` : ""}
        </p>
      ) : answers.length === 0 ? (
        <p className="text-xs text-muted-foreground">响应里一个答案都没有。</p>
      ) : (
        <div className="space-y-1">
          <p className="text-2xs tracking-[0.08em] text-foreground-tertiary uppercase">逐答案</p>
          {answers.map((answer) => (
            <div key={answer.questionName} className="flex flex-wrap items-baseline gap-x-2 text-xs">
              <span className="font-mono text-foreground">{answer.questionName}</span>
              <span className="text-muted-foreground">{answer.type}</span>
              <span className="text-foreground tabular-nums">
                {answer.type === "choice"
                  ? (answer.choice ?? "—")
                  : answer.type === "score"
                    ? formatRatio(answer.score ?? 0)
                    : formatRatio(answer.noul ?? 0)}
              </span>
              <span className="text-muted-foreground tabular-nums">
                置信 {formatRatio(answer.confidence)} · {answer.model} · {answer.latencyMs}ms
              </span>
              {answer.probabilities ? (
                <span className="text-muted-foreground tabular-nums">
                  {[...Object.entries(answer.probabilities)]
                    .sort((a, b) => b[1] - a[1])
                    .slice(0, 3)
                    .map(([key, value]) => `${key} ${formatRatio(value)}`)
                    .join(" / ")}
                </span>
              ) : null}
            </div>
          ))}
        </div>
      )}

      <p className="text-xs text-muted-foreground">
        state：
        <span className="font-mono">{String(trace.request.state).slice(0, 160) || "（空）"}</span>
      </p>
    </div>
  );
}

function TraceRow({ trace, threshold }: { trace: DecisionTrace; threshold: number }) {
  const [open, setOpen] = useState(false);
  const minConfidence = minConfidenceOf(trace);
  const weakest = minConfidence !== null && minConfidence < threshold;
  const chain = trace.modelChain.length > 0 ? trace.modelChain.join("→") : "缓存";
  const sensitivity = trace.request.sensitivity ?? "public";

  return (
    <div className="border-b border-border/60 last:border-b-0">
      <button
        type="button"
        onClick={() => setOpen((previous) => !previous)}
        aria-expanded={open}
        className="flex w-full flex-wrap items-baseline gap-x-2.5 gap-y-1 px-3 py-2 text-left outline-none focus-visible:ring-2 focus-visible:ring-ring/45"
      >
        <span className="text-xs text-muted-foreground tabular-nums">{formatTime(trace.timestamp)}</span>
        <span className="text-xs text-foreground">{signatureLabel(traceSignature(trace))}</span>
        <span className={cn("text-xs", sensitivityTone(sensitivity))}>{sensitivity}</span>
        <span className="text-xs text-muted-foreground">{chain}</span>
        <span className="text-xs text-muted-foreground tabular-nums">{formatMs(trace.totalLatencyMs)}</span>
        {minConfidence === null ? (
          <span className="text-xs text-destructive tabular-nums">没答案</span>
        ) : (
          <span
            className={cn(
              "text-xs tabular-nums",
              weakest ? "text-warning" : "text-muted-foreground",
            )}
          >
            {formatRatio(minConfidence)}
          </span>
        )}
        {trace.cacheHit ? <span className="text-2xs text-muted-foreground">命中</span> : null}
        {trace.response?.degraded ? (
          <span className="text-2xs text-warning">降级</span>
        ) : null}
      </button>
      {open ? <TraceDetail trace={trace} /> : null}
    </div>
  );
}

/**
 * useSyncExternalStore 要求 subscribe 是稳定引用，而且它会把函数单独取出去调用——
 * 直接传 decisionMonitor.subscribe 会把 this 丢掉（内部要用 this.listeners）。
 */
const subscribeTraces = (listener: () => void) => decisionMonitor.subscribe(listener);
const getTraces = () => decisionMonitor.snapshot;

/**
 * 右栏「决策」标签：System 1 这一层到底有没有在干活、代价多少、被判成了什么。
 * 开关不住这里（在 设置 → 决策层），这里只出读数。
 * 读数跟着当前对话走：只统计 request.conversationId 等于活动话题的判定，
 * 订阅是实时的——判定一落审计这里就重算。
 * 按右栏 384px 宽设计：统计卡两列、宽表格横向滚动——别把整屏页的栅格原样搬回来。
 */
export function DecisionPanel() {
  const system = useDecisionStore((s) => s.system);
  const health = useDecisionStore((s) => s.health);
  const probing = useDecisionStore((s) => s.probing);
  const pid = useDecisionStore((s) => s.pid);
  const busy = useDecisionStore((s) => s.busy);
  const note = useDecisionStore((s) => s.note);
  const probe = useDecisionStore((s) => s.probe);
  const patch = useDecisionStore((s) => s.patch);
  const start = useDecisionStore((s) => s.start);
  const stop = useDecisionStore((s) => s.stop);
  const clearNote = useDecisionStore((s) => s.clearNote);

  const traces = useSyncExternalStore(subscribeTraces, getTraces, getTraces);
  const activeId = useChatStore((s) => s.activeId);
  const [testing, setTesting] = useState<string | null>(null);

  // 面板挂着才收数：审计本身一直在记，这里只是把它订阅进来并留住跨配置重建的历史
  useEffect(() => decisionMonitor.attach(system), [system]);
  const [tick, setTick] = useState(0);
  useEffect(() => {
    void probe();
    // tick 联动探测节奏：V2 能力读数（metrics 计数器）跟着每 5s 重读一次
    const timer = window.setInterval(() => {
      void probe();
      setTick((value) => value + 1);
    }, 5000);
    return () => window.clearInterval(timer);
  }, [probe, system]);

  const config = system.config;
  const threshold = config.routing.autoUpgradeThreshold;
  // 读数跟着当前对话走：只统计这场对话自己的判定。
  // 没带话题 id 的判定（试一次、记忆分级这类全局治理）不落进任何一场对话的流
  const conversationTraces = useMemo(
    () => tracesForConversation(traces, activeId),
    [traces, activeId],
  );
  const summary = useMemo(() => summarizeTraces(conversationTraces), [conversationTraces]);
  const groups = useMemo(() => groupBySignature(conversationTraces), [conversationTraces]);
  const routing = useMemo(() => routingByTaskType(conversationTraces, threshold), [conversationTraces, threshold]);
  const latency = useMemo(() => latencyBuckets(conversationTraces), [conversationTraces]);
  const confidence = useMemo(() => confidenceBuckets(conversationTraces), [conversationTraces]);

  const layaState = !config.laya.enabled
    ? { text: "已关闭", tone: "plain" as const }
    : !config.laya.sidecarEndpoint
      ? { text: "没配服务商", tone: "bad" as const }
      : health === null
        ? { text: probing ? "探测中…" : "还没探", tone: "plain" as const }
        : health.ok
          ? health.loaded
            ? { text: "已加载", tone: "good" as const }
            : health.loading
              ? { text: "正在加载权重", tone: "warn" as const }
              : { text: "在听，没热过", tone: "warn" as const }
          : { text: "没在听", tone: "bad" as const };

  const jevState = !config.jev.enabled
    ? { text: "未启用", tone: "plain" as const }
    : !system.jev
      ? { text: "缺密钥", tone: "bad" as const }
      : system.jev.isAvailable
        ? { text: `可用 · ${config.jev.via}`, tone: "good" as const }
        : { text: "等密钥", tone: "warn" as const };

  async function pickDir() {
    try {
      const picked = await pickFolder({ directory: true, multiple: false, title: "选择 sidecar 目录" });
      if (typeof picked !== "string") return;
      patch((draft) => {
        draft.laya.sidecarDir = picked;
      });
    } catch {
      clearNote();
    }
  }

  /** 走一遍真漏斗。它是这块面板唯一的主动读数：别的都等别人来调 */
  async function tryOnce() {
    setTesting(null);
    try {
      const response = await system.router.decide({
        state: `aglab 自检 ${new Date().toISOString()}`,
        questions: {
          self_check: { type: "noul", instructions: "Is this a synthetic self-check message?" },
        },
        sensitivity: "public",
      });
      setTesting(
        `${response.model} · ${formatRatio(getMinConfidence(response))}${
          response.degraded ? " · 未达阈值" : ""
        } · ${formatMs(response.totalLatencyMs)}${response.cacheHit ? " · 命中缓存" : ""}`,
      );
    } catch (error) {
      setTesting(`失败：${error instanceof Error ? error.message : String(error)}`);
    }
  }

  return (
    <div className="min-h-0 flex-1">
      <div className="flex items-start justify-between gap-2">
        <p className="min-w-0 text-xs leading-5 text-muted-foreground">
          不生成文本，只出概率化判定。开关与阈值在 设置 → 决策层。
          {!config.enabled
            ? "总开关已关。"
            : summary.decisions === 0
              ? "本场对话还没有判定。"
              : ` ${summary.decisions} 次决策 · 命中 ${Math.round(summary.cacheHitRate * 100)}% · ${
                  // 全失败的时候报"p50 0ms"是把"没人答上"念成了"快得数不清"
                  summary.answered > 0 ? `p50 ${formatMs(summary.p50LatencyMs)}` : `失败 ${summary.failed}`
                }`}
        </p>
        <div className="flex shrink-0 gap-1">
          <Button variant="ghost" size="sm" onClick={() => void tryOnce()}>
            <Play className="size-3.5" />
            <span>试一次</span>
          </Button>
          <Button
            variant="ghost"
            size="sm"
            disabled={conversationTraces.length === 0}
            onClick={() => decisionMonitor.clear()}
          >
            <Eraser className="size-3.5" />
            <span>清空</span>
          </Button>
        </div>
      </div>

      <div className="mt-4 grid grid-cols-2 gap-2">
        <StatCard label="Laya 本地" value={layaState.text} tone={layaState.tone} hint={config.laya.sidecarEndpoint} />
        <StatCard
          label="Jev 云端"
          value={jevState.text}
          tone={jevState.tone}
          hint={config.jev.enabled ? `传输 ${config.jev.transport}` : "出站判定未启用"}
        />
        <StatCard
          label="缓存"
          value={system.cache ? `${system.cache.size} 条` : "已关闭"}
          hint={system.cache ? `命中率 ${Math.round(summary.cacheHitRate * 100)}%` : "每次都重问"}
        />
        <StatCard
          label="审计"
          value={system.audit ? `${summary.decisions} 条` : "已关闭"}
          hint={system.audit ? `失败 ${summary.failed} · 降级 ${summary.degraded}` : "没有读数可看"}
        />
      </div>

      {testing ? <p className="mt-3 text-xs text-muted-foreground">试一次：{testing}</p> : null}

      <div className="mt-5 rounded-lg border border-border bg-surface px-3 py-3">
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-base font-medium text-foreground">Laya sidecar</span>
          <span className="min-w-0 flex-1 truncate font-mono text-xs text-muted-foreground">
            {config.laya.sidecarDir || "还没选目录"}
          </span>
          <Button variant="subtle" size="sm" onClick={() => void pickDir()}>
            选目录
          </Button>
          {pid === null ? (
            <Button
              variant="subtle"
              size="sm"
              disabled={busy || !config.laya.sidecarDir}
              onClick={() => void start(config.laya.sidecarDir)}
            >
              <Play className="size-3.5" />
              <span>启动</span>
            </Button>
          ) : (
            <Button variant="subtle" size="sm" disabled={busy} onClick={() => void stop()}>
              <Square className="size-3.5" />
              <span>停止 {pid}</span>
            </Button>
          )}
        </div>
        <p className="mt-1.5 text-xs leading-5 text-muted-foreground">
          要 Node 20+。首次启动从 HuggingFace 拉 1.7GB 权重，之后一直住在内存里。
          {pid === null ? " 自己在终端里跑的那份，aglab 停不了。" : ""}
        </p>
        {note ? <p className="mt-1.5 text-xs leading-5 text-destructive">{note}</p> : null}
      </div>

      {config.enabled && summary.decisions === 0 ? (
        <p className="mt-5 text-sm leading-6 text-muted-foreground">
          这场对话还没有判定记录。每条消息的路由判定与每轮收尾的提取门控会实时记在这里。
        </p>
      ) : null}

      {summary.decisions > 0 ? (
        <>
          <div className="mt-5 grid grid-cols-2 gap-2">
            <StatCard
              label="达阈值"
              value={`${summary.answered - summary.degraded}/${summary.answered}`}
              hint={`阈值 ${formatRatio(threshold)}`}
            />
            <StatCard
              label="p50 / p95"
              value={
                summary.answered > 0
                  ? `${formatMs(summary.p50LatencyMs)} / ${formatMs(summary.p95LatencyMs)}`
                  : "—"
              }
              hint={summary.answered > 0 ? `均值 ${formatMs(summary.avgLatencyMs)}` : "一次有答案的都没有"}
            />
            <StatCard
              label="批量宽度"
              value={`${summary.questionsPerDecision.toFixed(1)} 问/次`}
              hint={`共 ${summary.questions} 个问题`}
            />
            <StatCard
              label="红线"
              value={summary.redlineBreaches === 0 ? "没越界" : `${summary.redlineBreaches} 次越界`}
              tone={summary.redlineBreaches === 0 ? "good" : "bad"}
              hint={`云端答过 ${summary.byTier.jev + summary.byTier.fallback} 次`}
            />
          </div>

          <Bars
            title="端到端耗时"
            buckets={latency}
            note={`${summary.answered} 次有答案的`}
          />
          <Bars
            title="最小置信度"
            buckets={confidence}
            highlight={confidenceBucketIndex(threshold)}
            note={`阈值 ${formatRatio(threshold)}`}
          />

          <div className="mt-5">
            <h2 className="text-base font-semibold tracking-tight text-foreground">链路构成</h2>
            <div className="mt-2 rounded-lg border border-border bg-surface">
              {summary.chains.map((chain) => (
                <div
                  key={chain.chain}
                  className="flex items-baseline justify-between gap-3 border-b border-border/60 px-3 py-2 text-xs last:border-b-0"
                >
                  <span className="font-mono text-foreground">{chain.chain}</span>
                  <span className="text-muted-foreground tabular-nums">{chain.count} 次</span>
                </div>
              ))}
            </div>
          </div>

          <div className="mt-5">
            <h2 className="text-base font-semibold tracking-tight text-foreground">按用途</h2>
            <p className="mt-1 text-xs text-muted-foreground">
              问题签名 = 谁在调用这一层。钉本地的用途不该出现 jev。
            </p>
            <div className="scroll-fade-x-l scroll-fade-x mt-2 overflow-x-auto rounded-lg border border-border bg-surface">
              <table className="w-full min-w-[380px] text-xs">
                <thead>
                  <tr className="border-b border-border text-left text-muted-foreground">
                    <th className="px-3 py-2 font-normal">用途</th>
                    <th className="px-3 py-2 text-right font-normal">次数</th>
                    <th className="px-3 py-2 text-right font-normal">命中</th>
                    <th className="px-3 py-2 text-right font-normal">均置信</th>
                    <th className="px-3 py-2 text-right font-normal">p95</th>
                    <th className="px-3 py-2 text-right font-normal">降级 / 失败</th>
                    <th className="px-3 py-2 text-right font-normal">出过答案的层</th>
                  </tr>
                </thead>
                <tbody>
                  {groups.map((group) => (
                    <tr key={group.signature} className="border-b border-border/60 last:border-b-0">
                      <td className="max-w-[160px] truncate px-3 py-2 text-foreground" title={group.signature}>
                        {group.label}
                        {group.maxSensitivity !== "public" ? (
                          <span className={cn("ml-1.5", sensitivityTone(group.maxSensitivity))}>
                            {group.maxSensitivity}
                          </span>
                        ) : null}
                      </td>
                      <td className="px-3 py-2 text-right tabular-nums">{group.count}</td>
                      <td className="px-3 py-2 text-right tabular-nums text-muted-foreground">{group.cacheHits}</td>
                      <td className="px-3 py-2 text-right tabular-nums">
                        {group.avgMinConfidence === null ? "—" : formatRatio(group.avgMinConfidence)}
                      </td>
                      <td className="px-3 py-2 text-right tabular-nums text-muted-foreground">
                        {formatMs(group.p95LatencyMs)}
                      </td>
                      <td className="px-3 py-2 text-right tabular-nums">
                        {group.degraded} / <span className={group.failed > 0 ? "text-destructive" : "text-muted-foreground"}>{group.failed}</span>
                      </td>
                      <td className="px-3 py-2 text-right font-mono text-muted-foreground">
                        {group.tiers.length > 0 ? group.tiers.join("→") : "—"}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </div>

          {routing.length > 0 ? (
            <div className="mt-5">
              <h2 className="text-base font-semibold tracking-tight text-foreground">模型路由观测</h2>
              <p className="mt-1 text-xs text-muted-foreground">
                只摊账，不改档：模型怎么选仍是档案级的决定。
              </p>
              <div className="scroll-fade-x-l scroll-fade-x mt-2 overflow-x-auto rounded-lg border border-border bg-surface">
                <table className="w-full min-w-[340px] text-xs">
                  <thead>
                    <tr className="border-b border-border text-left text-muted-foreground">
                      <th className="px-3 py-2 font-normal">任务类型</th>
                      <th className="px-3 py-2 text-right font-normal">次数</th>
                      <th className="px-3 py-2 text-right font-normal">复杂度</th>
                      <th className="px-3 py-2 text-right font-normal">要大模型</th>
                      <th className="px-3 py-2 text-right font-normal">达阈值</th>
                    </tr>
                  </thead>
                  <tbody>
                    {routing.map((row) => (
                      <tr key={row.taskType} className="border-b border-border/60 last:border-b-0">
                        <td className="px-3 py-2 text-foreground">{row.taskType}</td>
                        <td className="px-3 py-2 text-right tabular-nums">{row.count}</td>
                        <td className="px-3 py-2 text-right tabular-nums">{row.avgComplexity.toFixed(1)} / 4</td>
                        <td className="px-3 py-2 text-right tabular-nums">
                          {Math.round(row.premiumRate * 100)}%
                        </td>
                        <td className="px-3 py-2 text-right tabular-nums text-muted-foreground">
                          {Math.round(row.aboveThreshold * 100)}%
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            </div>
          ) : null}
        </>
      ) : null}

      <V2MetricsSection tick={tick} />

      <div className="mt-6">
        <div className="flex items-baseline justify-between gap-2">
          <h2 className="text-base font-semibold tracking-tight text-foreground">决策流</h2>
          <span className="text-xs text-muted-foreground">
            {config.audit.enabled
              ? `展开看逐答案 · confidential 只剩哈希（最近 ${conversationTraces.length} 条）`
              : "审计已关闭，这里不会有读数"}
          </span>
        </div>
        <div className="mt-2 rounded-lg border border-border bg-surface">
          {conversationTraces.length === 0 ? (
            <p className="px-3 py-3 text-xs text-muted-foreground">这场对话还没有判定记录。</p>
          ) : (
            conversationTraces.map((trace) => <TraceRow key={trace.id} trace={trace} threshold={threshold} />)
          )}
        </div>
      </div>
    </div>
  );
}

/** V2 能力读数（§9）：压缩 / 审查 / 检索 / 漏斗升级的跨重启累计计数。
 *  这些能力不走 router.decide，审计流里没有它们的 trace——读数来自 metrics 计数器 */
function V2MetricsSection({ tick }: { tick: number }) {
  // tick 只用来触发重读（探测定时器每 5s 翻动一次）；计数器本身是模块级同步状态
  const summary = useMemo(() => summarizeV2Metrics(getMetrics()), [tick]);
  const hasAny =
    summary.compaction.applied + summary.compaction.identity + summary.review.block + summary.review.allow +
      summary.retrieval.performed + summary.retrieval.skipped + summary.funnel.upgraded >
    0;
  if (!hasAny) return null;
  const rows: Array<{ label: string; value: string; hint?: string }> = [
    {
      label: "压缩",
      value: `${summary.compaction.applied} 次生效`,
      hint: [
        summary.compaction.identity > 0 ? `未动 ${summary.compaction.identity}` : null,
        summary.compaction.unansweredKeep > 0 ? `缺答保留 ${summary.compaction.unansweredKeep}` : null,
        summary.compaction.cacheGuardSkip > 0 ? `guard 拦截 ${summary.compaction.cacheGuardSkip}` : null,
        summary.compaction.rewrite > 0 ? `rewrite ${summary.compaction.rewrite}` : null,
      ]
        .filter(Boolean)
        .join(" · ") || undefined,
    },
    {
      label: "审查",
      value: `${summary.review.allow} 放行 / ${summary.review.block} 拦截`,
      hint: [
        summary.review.escalate > 0 ? `人工复查 ${summary.review.escalate}` : null,
        summary.review.annotate > 0 ? `标注 ${summary.review.annotate}` : null,
        summary.review.fallbackOpen > 0 ? `fail-open ${summary.review.fallbackOpen}` : null,
      ]
        .filter(Boolean)
        .join(" · ") || undefined,
    },
    {
      label: "检索",
      value: `${summary.retrieval.performed} 次搜索 / ${summary.retrieval.skipped} 次不搜`,
      hint: summary.retrieval.cacheHit > 0 ? `缓存命中 ${summary.retrieval.cacheHit}` : undefined,
    },
    {
      label: "漏斗",
      value: summary.funnel.upgraded > 0 ? `${summary.funnel.upgraded} 次按问升级` : "未触发升级",
      hint: [
        summary.funnel.conservativeDefault > 0 ? `保守兜底 ${summary.funnel.conservativeDefault}` : null,
        summary.funnel.coalesced > 0 ? `并发合并 ${summary.funnel.coalesced}` : null,
      ]
        .filter(Boolean)
        .join(" · ") || undefined,
    },
  ];
  return (
    <div className="mt-6">
      <div className="flex items-baseline justify-between gap-2">
        <h2 className="text-base font-semibold tracking-tight text-foreground">V2 能力读数</h2>
        <span className="text-xs text-muted-foreground">跨重启累计 · 压缩/审查/检索/按问升级</span>
      </div>
      <div className="mt-2 grid grid-cols-2 gap-2">
        {rows.map((row) => (
          <StatCard key={row.label} label={row.label} value={row.value} hint={row.hint} />
        ))}
      </div>
    </div>
  );
}
