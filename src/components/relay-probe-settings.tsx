import { useCallback, useEffect, useState } from "react";
import {
  IconAlertTriangle as AlertTriangle,
  IconCheck as Check,
  IconChevronDown as ChevronDown,
  IconRadar as Radar,
  IconTrash as Trash2,
} from "@tabler/icons-react";

import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Group, SettingsHeader, inputClass } from "@/components/settings-ui";
import { FormColumn } from "@/components/ui/content-column";
import {
  deleteProbeHistory,
  fetchProbeHistory,
  runProbe,
  type ProbeReport,
  type ProbeSignal,
} from "@/lib/chat-transport";
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
  const pushToast = useChatStore((s) => s.pushToast);
  const [claimed, setClaimed] = useState<Claimed>("openai");
  const [depth, setDepth] = useState<Depth>("quick");
  const [profileId, setProfileId] = useState<string>("current");
  // 指定模型检测：空 = 用档案默认；datalist 提示已知模型名，手填任意名也行
  const [model, setModel] = useState("");
  const [running, setRunning] = useState(false);
  const [report, setReport] = useState<ProbeReport | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [history, setHistory] = useState<ProbeReport[]>([]);
  const [historyOpen, setHistoryOpen] = useState(false);
  const [modelListOpen, setModelListOpen] = useState(false);
  const [detail, setDetail] = useState<ProbeReport | null>(null);
  const [confirmingDelete, setConfirmingDelete] = useState<string | null>(null);
  // 历史分页：本地列表量小，分页在前端做；跳页输入容忍手输越界（钳回有效范围）
  const [page, setPage] = useState(1);
  const [jumpTo, setJumpTo] = useState("");
  const PAGE_SIZE = 6;

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

  const totalPages = Math.max(1, Math.ceil(history.length / PAGE_SIZE));
  const safePage = Math.min(page, totalPages);
  const pageRows = history.slice((safePage - 1) * PAGE_SIZE, safePage * PAGE_SIZE);
  const knownModels = [...new Set(config.models.map((spec) => spec.model))].filter(Boolean);
  // 建议列表：按输入过滤（大小写不敏感），已精确输入的不再提示
  const suggestions = knownModels
    .filter((name) => name.toLowerCase() !== model.trim().toLowerCase())
    .filter((name) => model.trim() === "" || name.toLowerCase().includes(model.trim().toLowerCase()))
    .slice(0, 8);

  async function run() {
    setRunning(true);
    setError(null);
    try {
      const next = await runProbe({
        claimed,
        depth,
        // "current" 哨兵 = 探当前连接；真档案 id 才下发给后端
        profileId: profileId === "current" ? undefined : profileId,
        model: model.trim() || undefined,
      });
      setReport(next);
      setPage(1);
      loadHistory();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setRunning(false);
    }
  }

  async function removeHistory(id: string) {
    try {
      await deleteProbeHistory(id);
      setHistory(await fetchProbeHistory());
    } catch (cause) {
      pushToast({ tone: "error", title: "删除失败", detail: String(cause) });
    }
  }

  return (
    <FormColumn>
      <SettingsHeader
        title="中转站探针"
        description="向当前服务商发送少量探测请求，收集证据帮你判断两件事：请求是否被注入额外内容、声称的目标是否与实际响应特征相符。所有结论都标明置信度，探针不改变你的正常对话。"
        action={<Radar className="size-5 text-muted-foreground" />}
      />

      <Group title="探测">
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
          <label className="block">
            <span className="mb-1.5 block text-xs text-muted-foreground">声称的目标（中转宣称接的是谁）</span>
            <Select value={claimed} onValueChange={(value) => setClaimed(value as Claimed)}>
              <SelectTrigger aria-label="声称的目标">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {(Object.keys(CLAIMED_LABEL) as Claimed[]).map((key) => (
                  <SelectItem key={key} value={key}>
                    {CLAIMED_LABEL[key]}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </label>
          <label className="block">
            <span className="mb-1.5 block text-xs text-muted-foreground">对照的服务商档案（可选）</span>
            <Select value={profileId} onValueChange={setProfileId}>
              <SelectTrigger aria-label="对照的服务商档案">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="current">当前连接</SelectItem>
                {config.profiles.map((profile) => (
                  <SelectItem key={profile.id} value={profile.id}>
                    {profile.name}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </label>
        </div>

        {/* 指定模型检测：留空用档案默认；填了就走点名覆盖（降级/换模鉴别）。
            自定义建议面板代替原生 datalist——原生下拉不受主题控制（真机踩过） */}
        <div className="relative mt-4">
          <label className="block">
            <span className="mb-1.5 block text-xs text-muted-foreground">
              探测模型（可选，留空用当前模型；可填中转声称的其他模型名验证降级）
            </span>
            <input
              value={model}
              placeholder={config.model || "模型名"}
              onChange={(event) => {
                setModel(event.target.value);
                setModelListOpen(true);
              }}
              onFocus={() => setModelListOpen(true)}
              onBlur={() => setModelListOpen(false)}
              onKeyDown={(event) => {
                if (event.key === "Escape") setModelListOpen(false);
              }}
              className={inputClass}
            />
          </label>
          {modelListOpen && suggestions.length > 0 ? (
            <ul
              className="absolute top-full right-0 left-0 z-30 mt-1 max-h-56 overflow-y-auto rounded-lg border border-border bg-background py-1 shadow-lg"
              role="listbox"
            >
              {suggestions.map((name) => (
                <li key={name}>
                  <button
                    type="button"
                    role="option"
                    aria-selected={name === model}
                    // mousedown 先于 input 的 blur：按下时先把值填上，blur 再关面板
                    onMouseDown={(event) => {
                      event.preventDefault();
                      setModel(name);
                      setModelListOpen(false);
                    }}
                    className="w-full px-3 py-1.5 text-left text-sm text-foreground transition-colors hover:bg-accent"
                  >
                    {name}
                  </button>
                </li>
              ))}
            </ul>
          ) : null}
        </div>

        <div className="mt-4 flex flex-wrap items-center gap-2">
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
      </Group>

      {error ? (
        <div className="mt-4">
          <p className="rounded-lg border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs leading-5 text-destructive">
            {error}
          </p>
        </div>
      ) : null}

      {report ? (
        <div className="mt-5">
          <ReportView report={report} />
        </div>
      ) : null}

      {history.length > 0 ? (
        <div className="mt-5">
          <Group title="历史对比">
            <div className="rounded-lg border border-border">
              <button
                type="button"
                onClick={() => setHistoryOpen(!historyOpen)}
                className="flex w-full items-center justify-between px-3 py-2.5 text-left text-xs font-semibold text-foreground"
              >
                历史对比（{history.length} 次）
                <ChevronDown className={cn("size-3.5 transition-transform", historyOpen && "rotate-180")} />
              </button>
              {historyOpen ? (
                <>
                  <ul className="divide-y divide-border border-t border-border">
                    {pageRows.map((item) => (
                      <li key={item.id || item.finishedAt} className="group/item flex items-center gap-3 px-3 py-2 text-xs">
                        <button
                          type="button"
                          onClick={() => setDetail(item)}
                          className="min-w-0 flex-1 truncate text-left transition-colors hover:text-foreground"
                          title="点击查看详情"
                        >
                          <span className="font-mono text-2xs text-muted-foreground">{item.finishedAt.slice(0, 19).replace("T", " ")}</span>
                          <span className="ml-3 text-muted-foreground">{item.model}</span>
                        </button>
                        <span className="ml-auto shrink-0 font-semibold tabular-nums">{item.score}</span>
                        <span className="w-10 shrink-0 text-right text-muted-foreground">{item.verdict}</span>
                        {item.id ? (
                          confirmingDelete === item.id ? (
                            <span className="flex shrink-0 items-center gap-1">
                              <button
                                type="button"
                                aria-label="确认删除这条记录"
                                className="rounded px-1.5 py-0.5 text-destructive transition-colors hover:bg-destructive/15"
                                onClick={() => {
                                  setConfirmingDelete(null);
                                  void removeHistory(item.id);
                                }}
                              >
                                删除
                              </button>
                              <button
                                type="button"
                                className="rounded px-1.5 py-0.5 text-muted-foreground transition-colors hover:bg-elevated"
                                onClick={() => setConfirmingDelete(null)}
                              >
                                取消
                              </button>
                            </span>
                          ) : (
                            <button
                              type="button"
                              aria-label={`删除 ${item.finishedAt} 的探测记录`}
                              onClick={() => setConfirmingDelete(item.id)}
                              className="hidden size-6 shrink-0 items-center justify-center rounded-lg text-muted-foreground transition-colors hover:bg-elevated hover:text-foreground group-hover/item:flex"
                            >
                              <Trash2 className="size-3.5" />
                            </button>
                          )
                        ) : null}
                      </li>
                    ))}
                  </ul>

                  {/* 分页：上一页/下一页 + 页码 + 跳页输入 */}
                  <div className="flex items-center justify-between gap-3 border-t border-border px-3 py-2 text-xs">
                    <span className="text-muted-foreground">
                      共 {history.length} 条 · 第 {safePage} / {totalPages} 页
                    </span>
                    <div className="flex items-center gap-1.5">
                      <Button variant="subtle" size="sm" disabled={safePage <= 1} onClick={() => setPage(safePage - 1)}>
                        上一页
                      </Button>
                      <span className="px-1 text-muted-foreground">…</span>
                      <Button
                        variant="subtle"
                        size="sm"
                        disabled={safePage >= totalPages}
                        onClick={() => setPage(safePage + 1)}
                      >
                        下一页
                      </Button>
                      <form
                        onSubmit={(event) => {
                          event.preventDefault();
                          const target = Number.parseInt(jumpTo, 10);
                          if (Number.isFinite(target)) setPage(Math.min(Math.max(target, 1), totalPages));
                          setJumpTo("");
                        }}
                      >
                        <input
                          value={jumpTo}
                          onChange={(event) => setJumpTo(event.target.value)}
                          placeholder="页码"
                          aria-label="跳转到指定页"
                          className="h-7 w-14 rounded-md border border-input bg-background px-2 text-center text-xs tabular-nums outline-none focus-visible:border-brand/50"
                        />
                      </form>
                      <Button variant="subtle" size="sm" onClick={() => setPage(totalPages)} disabled={safePage >= totalPages}>
                        末页
                      </Button>
                    </div>
                  </div>
                </>
              ) : null}
            </div>
          </Group>
        </div>
      ) : null}

      {/* 详情弹窗：整份报告在弹窗里完整展开 */}
      <Dialog open={detail !== null} onOpenChange={(next) => !next && setDetail(null)}>
        <DialogContent className="max-h-[86vh] w-[560px] max-w-[92vw] overflow-y-auto">
          <DialogTitle>探测详情{detail ? ` · ${detail.model}` : ""}</DialogTitle>
          {detail ? (
            <div className="space-y-3">
              <p className="text-xs text-muted-foreground">
                {detail.finishedAt.slice(0, 19).replace("T", " ")} · {detail.baseUrl} ·{" "}
                {detail.depth === "deep" ? "深度探测" : "快速探测"}
              </p>
              <ReportView report={detail} />
            </div>
          ) : null}
        </DialogContent>
      </Dialog>
    </FormColumn>
  );
}
