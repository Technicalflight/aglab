import { useCallback, useEffect, useState } from "react";
import { IconPlus as Plus } from "@tabler/icons-react";

import { Button } from "@/components/ui/button";
import { SectionFrame } from "@/components/section-frame";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { useChatStore } from "@/store/chat-store";
import { tasksApprovalDecide, tasksPendingApprovals } from "@/lib/chat-transport";
import { POLICIES, agoText, isPolicy, joinKind, policyLabel, splitKind } from "@/lib/tasks";
import { PaginationBar, usePaged } from "@/components/pagination";
import { emptyNode, patchNode, tidyGraph, toggleDepends, withoutNode } from "@/lib/task-graph";
import type {
  MissedPolicy,
  ScheduledTask,
  TaskApproval,
  TaskGraph,
  TaskKind,
  TaskView,
} from "@/types/chat";
import { cn } from "@/lib/utils";

const inputClass =
  "h-9 w-full rounded-lg border border-input bg-background px-3 text-base text-foreground outline-none transition-colors focus-visible:border-brand/50 focus-visible:ring-2 focus-visible:ring-ring/35";

const KINDS: Array<{ value: TaskKind; label: string }> = [
  { value: "interval", label: "每隔一段时间" },
  { value: "daily", label: "每天固定时刻" },
  { value: "weekly", label: "每周固定时刻" },
  { value: "cron", label: "Cron 表达式" },
];

// 下标 = 后端 num_days_from_sunday 的口径（0=周日…6=周六）
const WEEKDAYS = ["周日", "周一", "周二", "周三", "周四", "周五", "周六"];

function pad(value: number) {
  return String(value).padStart(2, "0");
}

function clockText(minutes: number) {
  return `${pad(Math.floor(minutes / 60))}:${pad(minutes % 60)}`;
}

function inText(ms: number) {
  if (ms <= 0) return "没有下一次";
  const left = ms - Date.now();
  if (left <= 0) return "就在这一轮";
  const minutes = Math.round(left / 60_000);
  if (minutes < 60) return `${minutes} 分钟后`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours} 小时后`;
  return `${Math.floor(hours / 24)} 天后`;
}

function atText(ms: number) {
  const date = new Date(ms);
  return `${date.getMonth() + 1}月${date.getDate()}日 ${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

function scheduleText(task: ScheduledTask) {
  const { base } = splitKind(task.kind);
  // 认不出的 kind 在后端**没有触发器**（连到期都不算），所以这里不能替它编一个频率出来
  if (base === null) return "频率认不出来";
  if (base === "daily") return `每天 ${clockText(task.atMinute)}`;
  if (base === "weekly") return `每${WEEKDAYS[task.atWeekday] ?? "？"} ${clockText(task.atMinute)}`;
  // cron 的表达本身就是频率，原样亮出来比翻译成人话诚实（翻译错了更糟）
  if (base === "cron") return `Cron ${task.cronExpr.trim() || "（空）"}`;
  return `每 ${task.everyMinutes} 分钟`;
}

const ON_FAILURE: Array<{ value: TaskGraph["onFailure"]; label: string }> = [
  { value: "block_run", label: "一处失败就停下" },
  { value: "skip_branch", label: "只放弃那条分支" },
];

function emptyDraft(): ScheduledTask {
  return {
    id: "",
    name: "",
    prompt: "",
    kind: "interval",
    everyMinutes: 60,
    atMinute: 9 * 60,
    atWeekday: 1,
    cronExpr: "",
    enabled: true,
    createdAt: 0,
    webhookUrl: "",
    webhookToken: "",
    // 新建的是"一句 prompt 跑一发"：空图。格子要在下面那块里加
    graph: { nodes: [], onFailure: "block_run" },
  };
}

export function TasksView() {
  const tasks = useChatStore((s) => s.tasks);
  const pagedTasks = usePaged(tasks);
  const tasksError = useChatStore((s) => s.tasksError);
  const refreshTasks = useChatStore((s) => s.refreshTasks);
  const saveTask = useChatStore((s) => s.saveTask);
  const removeTask = useChatStore((s) => s.removeTask);
  const runTaskNow = useChatStore((s) => s.runTaskNow);

  const [draft, setDraft] = useState<ScheduledTask | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [running, setRunning] = useState<string | null>(null);
  // 删除任务不可逆：第一下只把"要删的是哪一条"说出来，第二下才动手
  const [confirmDelete, setConfirmDelete] = useState<string | null>(null);
  // 这一格只报"此刻有几步停在等人点头"。已经表过态的那些是常设先例，住在
  // 设置 → Agent → 定时任务运行时，撤它不是这一页的事
  const [approvals, setApprovals] = useState<TaskApproval[]>([]);
  const [approvalError, setApprovalError] = useState<string | null>(null);
  const [deciding, setDeciding] = useState<string | null>(null);

  const draftSchedule = draft ? splitKind(draft.kind) : null;

  const refreshApprovals = useCallback(async () => {
    try {
      setApprovals(await tasksPendingApprovals());
      setApprovalError(null);
    } catch (cause) {
      setApprovalError(cause instanceof Error ? cause.message : String(cause));
    }
  }, []);

  const decide = useCallback(
    async (id: string, approved: boolean) => {
      setDeciding(id);
      setApprovalError(null);
      try {
        await tasksApprovalDecide(id, approved);
        await refreshApprovals();
        // 那一发的票已经投了，任务行上"停在待审批"那句就该跟着退场
        await refreshTasks();
      } catch (cause) {
        setApprovalError(cause instanceof Error ? cause.message : String(cause));
      } finally {
        setDeciding(null);
      }
    },
    [refreshApprovals, refreshTasks],
  );

  // 确认那一格不能一直挂着：手一抖点了删除又走开，回来不该发现任务没了
  useEffect(() => {
    if (!confirmDelete) return;
    const timer = setTimeout(() => setConfirmDelete(null), 3000);
    return () => clearTimeout(timer);
  }, [confirmDelete]);

  // 下次触发的倒计时与"有没有新东西挂在待审批上"都要自己走起来，否则停在这页就不动了
  useEffect(() => {
    const tick = () => {
      void refreshTasks();
      void refreshApprovals();
    };
    tick();
    const timer = setInterval(tick, 30_000);
    return () => clearInterval(timer);
  }, [refreshTasks, refreshApprovals]);

  const submit = useCallback(async () => {
    if (!draft) return;
    const name = draft.name.trim();
    const prompt = draft.prompt.trim();
    if (!name) {
      setError("先给任务起个名字。");
      return;
    }
    if (!prompt) {
      setError(
        draft.graph.nodes.length > 0
          ? "有格子的时候，这一句是这条任务在干什么的说明——写一句。"
          : "提示词是空的，到点没有可发的内容。",
      );
      return;
    }
    if (draftSchedule?.base === "interval" && draft.everyMinutes < 1) {
      setError("间隔至少 1 分钟。");
      return;
    }

    setError(null);
    try {
      await saveTask({
        ...draft,
        id: draft.id || `task-${Date.now().toString(36)}`,
        createdAt: draft.createdAt || Date.now(),
        name,
        prompt,
        // 图的判据住在后端那一道：被拒就是下面这一行显示出来，不在这儿重述规则
        graph: tidyGraph(draft.graph),
        // 去掉首尾空白再存：请求行是按空白切段的，带个尾巴的空格就永远敲不开，
        // 而那一刻界面上看着"配了令牌"
        webhookToken: draft.webhookToken.trim(),
      });
      setDraft(null);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    }
  }, [draft, draftSchedule, saveTask]);

  const active = tasks.filter((task) => task.enabled).length;

  return (
    <SectionFrame
      title="定时任务"
      note={tasks.length ? `${active} / ${tasks.length} 项在跑` : "到点自动发一条提示词"}
      actions={
        <Button
          variant="subtle"
          size="sm"
          disabled={draft !== null}
          onClick={() => {
            setError(null);
            setDraft(emptyDraft());
          }}
        >
          <Plus className="size-3.5" />
          <span>新建任务</span>
        </Button>
      }
    >
      <p className="text-sm leading-6 text-muted-foreground">
        1. 点「新建任务」：写一句提示词，选触发频率（每隔 / 每天 / 每周 / Cron）与错过怎么补；
        2. 到点后自动新建一条话题去执行，要人点头的步骤会停在下面等你；
        3. 运行记录与放行过的先例在 <span className="text-foreground">设置 → 助理 → 定时任务</span>。
      </p>

      {approvals.length > 0 ? (
        <div className="mt-4 rounded-lg border border-brand/40 bg-surface p-3">
          <p className="text-sm font-medium text-foreground">
            {approvals.length} 步在等人点头
          </p>
          <p className="mt-1 text-xs leading-5 text-muted-foreground">
            这些动作都还没被执行，不处理就一直停着。批准也不会当场续跑：它记的是
            <span className="text-foreground">以后每次运行碰到同一份参数都放行</span>
            ，改参数就是另一发。这一句"放行"站得住，直到你在
            <span className="text-foreground">设置 → Agent → 定时任务运行时</span>里把它撤回。
          </p>
          <ul className="mt-2 space-y-2">
            {approvals.map((item) => (
              <li
                key={item.id}
                className="rounded-lg border border-border bg-background px-3 py-2.5"
              >
                <p className="text-sm break-words text-foreground">{item.target}</p>
                <p className="mt-1 text-xs text-muted-foreground">
                  {item.capability} · {agoText(item.requestedAt)} · 运行 {item.runId}
                </p>
                <p className="mt-1 text-xs leading-5 break-words text-muted-foreground">
                  {item.reason}
                </p>
                <div className="mt-2 flex items-center gap-2">
                  <Button
                    variant="brand"
                    size="sm"
                    disabled={deciding === item.id}
                    onClick={() => void decide(item.id, true)}
                  >
                    批准
                  </Button>
                  <Button
                    variant="ghost"
                    size="sm"
                    className="text-destructive hover:bg-destructive/15 hover:text-destructive"
                    disabled={deciding === item.id}
                    onClick={() => void decide(item.id, false)}
                  >
                    拒绝
                  </Button>
                </div>
              </li>
            ))}
          </ul>
        </div>
      ) : null}

      {approvalError ? <p className="mt-2 text-xs text-destructive">{approvalError}</p> : null}

      {draft ? (
        <div className="mt-4 rounded-lg border border-border bg-surface p-3">
          <div className="grid gap-3 sm:grid-cols-2">
            <label className="block">
              <span className="mb-1.5 block text-xs text-muted-foreground">名称</span>
              <input
                type="text"
                value={draft.name}
                placeholder="例如：每日改动摘要"
                className={inputClass}
                onChange={(event) => setDraft({ ...draft, name: event.target.value })}
              />
            </label>
            <div className="block">
              <span className="mb-1.5 block text-xs text-muted-foreground">频率</span>
              <div className="flex gap-2">
                <Select
                  value={draftSchedule?.base ?? "interval"}
                  onValueChange={(value) =>
                    setDraft({
                      ...draft,
                      kind: joinKind(value as TaskKind, draftSchedule?.missed ?? "run_latest"),
                    })
                  }
                >
                  <SelectTrigger className="flex-1">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {KINDS.map((kind) => (
                      <SelectItem key={kind.value} value={kind.value}>
                        {kind.label}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
                {draftSchedule?.base === "interval" ? (
                  <span className="flex items-center gap-1.5">
                    <input aria-label="间隔分钟数"
                      type="number"
                      min={1}
                      max={1440}
                      value={draft.everyMinutes}
                      className={cn(inputClass, "w-20")}
                      onChange={(event) =>
                        setDraft({ ...draft, everyMinutes: Number(event.target.value) || 0 })
                      }
                    />
                    <span className="text-xs text-muted-foreground">分钟</span>
                  </span>
                ) : null}
                {draftSchedule?.base === "weekly" ? (
                  <Select
                    value={String(draft.atWeekday)}
                    onValueChange={(value) => setDraft({ ...draft, atWeekday: Number(value) })}
                  >
                    <SelectTrigger className="w-24 shrink-0">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      {WEEKDAYS.map((label, index) => (
                        <SelectItem key={index} value={String(index)}>
                          {label}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                ) : null}
                {draftSchedule?.base === "daily" || draftSchedule?.base === "weekly" ? (
                  <input aria-label="执行时刻"
                    type="time"
                    value={clockText(draft.atMinute)}
                    className={cn(inputClass, "w-28")}
                    onChange={(event) => {
                      const [hour, minute] = event.target.value.split(":").map(Number);
                      setDraft({ ...draft, atMinute: (hour || 0) * 60 + (minute || 0) });
                    }}
                  />
                ) : null}
                {draftSchedule?.base === "cron" ? (
                  <input
                    type="text"
                    value={draft.cronExpr}
                    aria-label="cron 表达式"
                    spellCheck={false}
                    placeholder="分 时 日 月 周，如 0 9 * * 1-5"
                    className={cn(inputClass, "w-56 shrink-0 font-mono text-sm")}
                    onChange={(event) => setDraft({ ...draft, cronExpr: event.target.value })}
                  />
                ) : null}
              </div>
            </div>
            <div className="block">
              <span className="mb-1.5 block text-xs text-muted-foreground">错过的点</span>
              <Select
                value={draftSchedule?.missed ?? "run_latest"}
                onValueChange={(value) =>
                  setDraft({
                    ...draft,
                    kind: joinKind(draftSchedule?.base ?? "interval", value as MissedPolicy),
                  })
                }
              >
                <SelectTrigger className="w-full">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {POLICIES.map((policy) => (
                    <SelectItem key={policy.value} value={policy.value}>
                      {policy.label}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
          </div>

          {draftSchedule?.base === null ? (
            <p className="mt-2 text-xs leading-5 text-brand-text">
              这条任务的频率那一格后端认不出来（
              <span className="break-all text-foreground">kind={draft.kind || "（空）"}</span>
              ），它现在不会有下一次。上面选好一种再存就会把它改成选的那种；不存就仍按"没有触发器"放着。
            </p>
          ) : null}

          <label className="mt-3 block">
            <span className="mb-1.5 block text-xs text-muted-foreground">到点发给模型的提示词</span>
            <textarea
              rows={3}
              value={draft.prompt}
              placeholder="例如：用三句话总结今天值得记下的结论"
              className="w-full resize-y rounded-lg border border-input bg-background px-3 py-2 text-base leading-6 text-foreground outline-none transition-colors focus-visible:border-brand/50 focus-visible:ring-2 focus-visible:ring-ring/35"
              onChange={(event) => setDraft({ ...draft, prompt: event.target.value })}
            />
          </label>

          {/* 图的形状此前只住在 config.json 里手写。这块界面不重述判据：
              存的时候后端那一道（`AppConfig::check_task_graphs`）说了算，被拒就把那一句显示出来 */}
          <div className="mt-3 rounded-lg border border-border px-3 py-2.5">
            <div className="flex items-baseline justify-between gap-3">
              <span className="text-xs text-muted-foreground">
                多步任务的格子
                {draft.graph.nodes.length > 0 ? ` · ${draft.graph.nodes.length} 格` : ""}
              </span>
              <span className="flex items-center gap-2">
                {draft.graph.nodes.length > 0 ? (
                  <Select
                    value={draft.graph.onFailure}
                    onValueChange={(value) =>
                      setDraft({
                        ...draft,
                        graph: { ...draft.graph, onFailure: value as TaskGraph["onFailure"] },
                      })
                    }
                  >
                    <SelectTrigger className="h-7 w-40 text-xs">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      {ON_FAILURE.map((item) => (
                        <SelectItem key={item.value} value={item.value}>
                          {item.label}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                ) : null}
                <Button
                  variant="ghost"
                  size="sm"
                  onClick={() =>
                    setDraft({
                      ...draft,
                      graph: {
                        ...draft.graph,
                        nodes: [
                          ...draft.graph.nodes,
                          emptyNode(draft.graph.nodes.map((node) => node.id)),
                        ],
                      },
                    })
                  }
                >
                  加一格
                </Button>
              </span>
            </div>
            <p className="mt-1 text-xs leading-5 text-muted-foreground">
              不加格子 = 到点把上面那句提示词发一发。加了之后发出去的是各格自己的那句话，
              上面那句只当这条任务在干什么的说明；每一格都会拿到它前置那几格跑完的结论。
              同一层的先后按下面这个次序，谁在前面谁先跑。
            </p>
            {draft.graph.nodes.map((node, index) => {
              const others = draft.graph.nodes.filter((_, at) => at !== index);
              return (
                <div
                  key={index}
                  className="mt-2 rounded-lg border border-border bg-background px-3 py-2.5"
                >
                  <div className="flex items-center gap-2">
                    <span className="shrink-0 text-xs text-muted-foreground">{index + 1}</span>
                    <input aria-label="节点名称"
                      type="text"
                      value={node.id}
                      placeholder="这一格叫什么"
                      spellCheck={false}
                      className={cn(inputClass, "h-7 w-40")}
                      onChange={(event) =>
                        setDraft({
                          ...draft,
                          graph: patchNode(draft.graph, index, { id: event.target.value }),
                        })
                      }
                    />
                    {node.subagent ? (
                      <span className="shrink-0 rounded-md border border-border px-1.5 py-0.5 text-2xs text-muted-foreground">
                        子助理 · 规格在 config.json 里改
                      </span>
                    ) : null}
                    <Button
                      variant="ghost"
                      size="sm"
                      className="ml-auto shrink-0 text-destructive hover:bg-destructive/15 hover:text-destructive"
                      onClick={() => setDraft({ ...draft, graph: withoutNode(draft.graph, index) })}
                    >
                      删掉这格
                    </Button>
                  </div>

                  <textarea
                    rows={2}
                    value={node.prompt}
                    placeholder="这一格要发出去的那句话"
                    className="mt-2 w-full resize-y rounded-lg border border-input bg-background px-3 py-2 text-base leading-6 text-foreground outline-none transition-colors focus-visible:border-brand/50 focus-visible:ring-2 focus-visible:ring-ring/35"
                    onChange={(event) =>
                      setDraft({
                        ...draft,
                        graph: patchNode(draft.graph, index, { prompt: event.target.value }),
                      })
                    }
                  />

                  <div className="mt-1.5 flex flex-wrap items-center gap-1.5">
                    <span className="shrink-0 text-xs text-muted-foreground">等谁的产出</span>
                    {others.length === 0 ? (
                      <span className="text-xs text-muted-foreground">
                        加第二格之后才有可等的对象
                      </span>
                    ) : (
                      others.map((other, at) => (
                        <button
                          key={at}
                          type="button"
                          onClick={() =>
                            setDraft({
                              ...draft,
                              graph: toggleDepends(draft.graph, index, other.id),
                            })
                          }
                          className={cn(
                            "rounded-md border px-1.5 py-0.5 text-2xs transition-colors",
                            node.dependsOn.includes(other.id)
                              ? "border-brand/45 bg-brand/10 text-brand-text"
                              : "border-border text-muted-foreground hover:text-foreground",
                          )}
                        >
                          {other.id || "（没有名字）"}
                        </button>
                      ))
                    )}
                  </div>

                  <label className="mt-1.5 flex items-center gap-2">
                    <span className="shrink-0 text-xs text-muted-foreground">
                      只许用这几个工具
                    </span>
                    <input
                      type="text"
                      value={node.allowedTools.join(",")}
                      placeholder="留空 = 沿用任务自己的白名单"
                      spellCheck={false}
                      className={cn(inputClass, "h-7 flex-1")}
                      onChange={(event) =>
                        setDraft({
                          ...draft,
                          graph: patchNode(draft.graph, index, {
                            // 只按逗号切、不剪空白也不丢空项：边敲边规范化会把用户刚打的那个逗号
                            // 吞掉（`baseUrl` 结尾那个斜杠是同一个坑）。收尾交给 tidyGraph
                            allowedTools: event.target.value.split(","),
                          }),
                        })
                      }
                    />
                  </label>
                </div>
              );
            })}
          </div>

          <label className="mt-3 block">
            <span className="mb-1.5 block text-xs text-muted-foreground">跑完通知谁（可选）</span>
            <input
              type="text"
              value={draft.webhookUrl}
              placeholder="https://…，或 http://127.0.0.1:8080/hook。留空就不发"
              spellCheck={false}
              className={inputClass}
              onChange={(event) => setDraft({ ...draft, webhookUrl: event.target.value })}
            />
            <span className="mt-1 block text-xs leading-5 text-muted-foreground">
              每发都带 HMAC-SHA256 签名（请求头 <span className="text-foreground">x-aglab-signature</span>），
              幂等键是这一发的 run id。签名密钥取系统凭据里的
              <span className="text-foreground"> aglab-webhook</span>——没有它就整条不发，
              也不会发一发没签名的。只投 https，本机回环允许 http。
            </span>
          </label>

          <label className="mt-3 block">
            <span className="mb-1.5 block text-xs text-muted-foreground">
              谁能敲它一下就跑（可选）
            </span>
            <input
              type="text"
              value={draft.webhookToken}
              placeholder="留空 = 这条只能由本机自己起"
              spellCheck={false}
              className={inputClass}
              onChange={(event) => setDraft({ ...draft, webhookToken: event.target.value })}
            />
            <span className="mt-1 block text-xs leading-5 text-muted-foreground">
              填了令牌，本机的另一个进程就能用「设置 → Agent → 定时任务运行时」里那条 URL
              点着这一条任务。
              令牌不对只回一句 404，不会说明是哪一步错的；它也不进审计与日志——那是凭据。
              开关没开或没重启过一次的话，谁敲都不理。
            </span>
          </label>

          <div className="mt-3 flex items-center gap-2">
            <Button variant="brand" size="sm" onClick={() => void submit()}>
              {draft.id ? "保存修改" : "建好任务"}
            </Button>
            <Button variant="ghost" size="sm" onClick={() => setDraft(null)}>
              取消
            </Button>
            <span className="text-xs text-muted-foreground">
              下次：
              {draftSchedule?.base === "daily"
                ? `每天 ${clockText(draft.atMinute)}`
                : draftSchedule?.base === "weekly"
                  ? `每${WEEKDAYS[draft.atWeekday] ?? "？"} ${clockText(draft.atMinute)}`
                  : draftSchedule?.base === "cron"
                    ? `Cron ${draft.cronExpr.trim() || "（空）"}`
                    : `每 ${draft.everyMinutes || 0} 分钟`}
            </span>
          </div>
        </div>
      ) : null}

      {error ? <p className="mt-3 text-xs text-destructive">{error}</p> : null}
      {!error && tasksError ? <p className="mt-3 text-xs text-destructive">{tasksError}</p> : null}

      {tasks.length === 0 && !draft ? (
        <p className="mt-4 text-sm text-muted-foreground">还没有任务。</p>
      ) : (
        <>
        <ul className="mt-4 space-y-2">
          {pagedTasks.slice.map((task: TaskView) => (
            <li
              key={task.id}
              className={cn(
                "rounded-lg border border-border bg-surface px-3 py-3",
                !task.enabled && "opacity-60",
              )}
            >
              <div className="flex items-baseline gap-2">
                <span className="min-w-0 flex-1 truncate text-base font-medium text-foreground">
                  {task.name}
                </span>
                <span className="shrink-0 text-xs text-muted-foreground">
                  {scheduleText(task)}
                  {/* 只在这三种之一时才提它：频率认不出的那条任务后端给的是空串，
                      那句"错过只补最近一次"是它没答应过的事（停用不在此列，它仍报自己的策略） */}
                  {isPolicy(task.missedPolicy) && task.missedPolicy !== "run_latest"
                    ? ` · ${policyLabel(task.missedPolicy)}`
                    : ""}
                  {task.graph.nodes.length > 0 ? ` · ${task.graph.nodes.length} 格` : ""}
                  {/* 只报"有没有配"，不报令牌本身：这一份是要贴到屏幕上的 */}
                  {task.webhookToken ? " · 可被本机敲" : ""}
                </span>
              </div>

              <p className="mt-1 line-clamp-2 text-xs leading-5 text-muted-foreground">
                {task.prompt}
              </p>

              <p className="mt-2 flex flex-wrap items-center gap-x-4 gap-y-1 text-xs">
                <span className="text-muted-foreground">
                  下次 <span className={task.enabled ? "text-brand-text" : "text-muted-foreground"}>
                    {task.enabled ? inText(task.nextRunAt) : "已停用"}
                  </span>
                </span>
                <span className="text-muted-foreground">
                  上次{" "}
                  <span className="text-foreground">
                    {task.lastRunAt ? atText(task.lastRunAt) : "没跑过"}
                  </span>
                  {task.lastStatus === "error" ? (
                    <span className="ml-1 text-destructive">失败</span>
                  ) : task.lastStatus === "waiting" ? (
                    <span className="ml-1 text-brand-text">在等人点头</span>
                  ) : task.lastStatus === "ok" ? (
                    <span className="ml-1 text-brand-text">跑完了</span>
                  ) : task.lastStatus === "skipped" ? (
                    <span className="ml-1 text-muted-foreground">追账作废</span>
                  ) : null}
                </span>
              </p>

              {task.lastError ? (
                <p className="mt-1 text-xs leading-5 break-words text-destructive">
                  {task.lastError}
                </p>
              ) : null}

              {task.deferred ? (
                <>
                <p className="mt-1 text-xs leading-5 break-words text-brand-text">
                  这一发还没起跑 · {task.deferred}
                </p>
                </>
              ) : null}

              <div className="mt-3 flex flex-wrap items-center gap-2 border-t border-border pt-2.5">
                <Button
                  variant="subtle"
                  size="sm"
                  disabled={running === task.id}
                  onClick={() => {
                    setRunning(task.id);
                    void runTaskNow(task.id).catch((cause) => {
                      setError(cause instanceof Error ? cause.message : String(cause));
                      setRunning(null);
                    });
                    setTimeout(() => setRunning(null), 3000);
                  }}
                >
                  {running === task.id ? "已在跑" : "立即运行"}
                </Button>
                <Button
                  variant="ghost"
                  size="sm"
                  onClick={() =>
                    void saveTask({
                      id: task.id,
                      name: task.name,
                      prompt: task.prompt,
                      kind: task.kind,
                      everyMinutes: task.everyMinutes,
                      atMinute: task.atMinute,
                      atWeekday: task.atWeekday,
                      cronExpr: task.cronExpr,
                      enabled: !task.enabled,
                      createdAt: task.createdAt,
                      webhookUrl: task.webhookUrl,
                      webhookToken: task.webhookToken,
                      // 图要原样带回去：漏一个字段，"暂停一下"就会把一条多步任务压回单发
                      graph: task.graph,
                    }).catch((cause) =>
                      setError(cause instanceof Error ? cause.message : String(cause)),
                    )
                  }
                >
                  {task.enabled ? "暂停" : "恢复"}
                </Button>
                <Button
                  variant="ghost"
                  size="sm"
                  onClick={() => {
                    setError(null);
                    setDraft({
                      id: task.id,
                      name: task.name,
                      prompt: task.prompt,
                      kind: task.kind,
                      everyMinutes: task.everyMinutes,
                      atMinute: task.atMinute,
                      atWeekday: task.atWeekday,
                      cronExpr: task.cronExpr,
                      enabled: task.enabled,
                      createdAt: task.createdAt,
                      webhookUrl: task.webhookUrl,
                      webhookToken: task.webhookToken,
                      graph: task.graph,
                    });
                  }}
                >
                  编辑
                </Button>
                <Button
                  variant="outline"
                  size="sm"
                  className={cn(
                    "ml-auto shrink-0 text-destructive",
                    confirmDelete === task.id
                      ? "border-destructive/60 bg-destructive/15"
                      : "hover:border-destructive/50 hover:bg-destructive/10 hover:text-destructive",
                  )}
                  onClick={() => {
                    if (confirmDelete !== task.id) {
                      setConfirmDelete(task.id);
                      return;
                    }
                    setConfirmDelete(null);
                    void removeTask(task.id).catch((cause) =>
                      setError(cause instanceof Error ? cause.message : String(cause)),
                    );
                  }}
                >
                  {confirmDelete === task.id ? "再点一次确认删除" : "删除"}
                </Button>
              </div>
            </li>
          ))}
        </ul>
          <PaginationBar page={pagedTasks.page} pages={pagedTasks.pages} total={pagedTasks.total} onPage={pagedTasks.setPage} />
      </>
      )}
    </SectionFrame>
  );
}
