import { useCallback, useEffect, useState } from "react";

import { CapabilityToggle } from "@/components/ui/capability-toggle";
import { Button } from "@/components/ui/button";
import {
  taskRunResume,
  tasksApprovalForget,
  tasksApprovalHistory,
  tasksRunList,
  tasksRunsPurge,
} from "@/lib/chat-transport";
import { agoText } from "@/lib/tasks";
import { useChatStore } from "@/store/chat-store";
import type { TaskApproval, TaskRun, TaskRunStatus } from "@/types/chat";
import { cn } from "@/lib/utils";
import { FormColumn } from "@/components/ui/content-column";

/** 账本上那五个结论要说得出人话：`running` 与 `waiting_approval` 是"没跑完"的两种，不是一回事，
 *  而 `skipped` 连"跑"都不是——它是追账被上限砍掉的那一行 */
const RUN_LABEL: Record<TaskRunStatus, string> = {
  running: "在跑",
  waiting_approval: "等人点头",
  succeeded: "跑成",
  failed: "失败",
  skipped: "追账作废",
};

const RUN_TONE: Record<TaskRunStatus, string> = {
  running: "border-border text-muted-foreground",
  waiting_approval: "border-brand/45 bg-brand/10 text-brand-text",
  succeeded: "border-border text-muted-foreground",
  failed: "border-destructive/45 text-destructive",
  skipped: "border-border text-muted-foreground",
};

/** 这一发是谁起的。三种就要三句话：第三种是"别人家的进程"，把它并入"调度器起的"
 *  等于把外部触发说成自己的账。Record 按联合类型取键，以后加第四种会编译不过 */
const STARTED_BY: Record<TaskRun["startedBy"], string> = {
  scheduler: "调度器起的",
  user: "手动起的",
  webhook: "本机进程敲的",
};

/**
 * 定时任务的运行时。任务定义住在侧栏「定时任务」那页，这里放的是它跑起来才有的三件事：
 * 谁能敲它一下、哪些动作以后不再问、每一发跑成什么样。
 */
export function TaskRuntimeSettings() {
  // 本机监听那一格是全局配置，不在任务定义里：开关与端口只有一份，任务只带自己的令牌
  const config = useChatStore((s) => s.config);
  const updateConfig = useChatStore((s) => s.updateConfig);
  // 数的是定义那一份：侧栏那个列表是运行投影，上面没有令牌——它是凭据，不该往投影里塞
  const withToken = config.tasks.filter((task) => task.webhookToken.trim() !== "").length;

  const [nodded, setNodded] = useState<TaskApproval[]>([]);
  const [revoking, setRevoking] = useState<string | null>(null);
  const [forgetError, setForgetError] = useState<string | null>(null);
  const [forgetNote, setForgetNote] = useState<string | null>(null);

  const [runs, setRuns] = useState<TaskRun[] | null>(null);
  const [runsError, setRunsError] = useState<string | null>(null);
  const [runsNote, setRunsNote] = useState<string | null>(null);
  const [resuming, setResuming] = useState<string | null>(null);
  const [purgeDays, setPurgeDays] = useState(30);
  // 一次清除抹掉的是 N 发的账，比删一条任务更宽，所以它同样要点两下
  const [purgeArmed, setPurgeArmed] = useState(false);
  const [purging, setPurging] = useState(false);

  const refreshNodded = useCallback(async () => {
    try {
      setNodded(await tasksApprovalHistory());
      setForgetError(null);
    } catch (cause) {
      setForgetError(cause instanceof Error ? cause.message : String(cause));
    }
  }, []);

  const refreshRuns = useCallback(async () => {
    try {
      setRuns(await tasksRunList());
      setRunsError(null);
    } catch (cause) {
      setRuns(null);
      setRunsError(cause instanceof Error ? cause.message : String(cause));
    }
  }, []);

  const revoke = useCallback(
    async (id: string) => {
      setRevoking(id);
      setForgetError(null);
      setForgetNote(null);
      try {
        // 后端回的是"实际抹掉了几条"：同一发的多条先例一次撤完，不报出来就没人知道撤了多少
        const gone = await tasksApprovalForget(id);
        setForgetNote(gone > 1 ? `已撤回并删掉 ${gone} 条同发先例。` : "已撤回并删除。");
      } catch (cause) {
        setForgetError(cause instanceof Error ? cause.message : String(cause));
      } finally {
        setRevoking(null);
        // 撤没撤成都把队列里真实的那一份读回来：这里说"没了"必须以盘上为准
        await refreshNodded();
      }
    },
    [refreshNodded],
  );

  async function resume(runId: string) {
    setResuming(runId);
    setRunsError(null);
    try {
      await taskRunResume(runId);
      // 续跑在后台线程上起，这里只能报"接上了"；跑到哪一格由随后的刷新读回来
      await refreshRuns();
    } catch (cause) {
      setRunsError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setResuming(null);
    }
  }

  useEffect(() => {
    if (!purgeArmed) return;
    const timer = setTimeout(() => setPurgeArmed(false), 3000);
    return () => clearTimeout(timer);
  }, [purgeArmed]);

  // 先例账和运行账都要自己走起来：有人在另一头批了、跑了，停在这页不该看不见
  useEffect(() => {
    const tick = () => {
      void refreshNodded();
      void refreshRuns();
    };
    tick();
    const timer = setInterval(tick, 30_000);
    return () => clearInterval(timer);
  }, [refreshNodded, refreshRuns]);

  return (
    <FormColumn>
      <h1 className="text-2xl font-semibold tracking-tight text-foreground">
        定时任务运行时
      </h1>
      <p className="mt-1 text-sm leading-6 text-muted-foreground">
        任务本身在侧栏「定时任务」页新建。这一页管跑起来之后的事：
        1. 要让本机其他进程也能触发，开「入口」并复制那条 URL；
        2. 每一发要人点头的动作先在这里批；
        3. 跑过的每一发在账里看结果。
      </p>

      {/* "别人能引起一次花钱的动作"的入口只有这一处，所以它单独一块：开关、端口、
          以及"配了令牌的任务有几条 / 此刻到底听不听"三件事并排说清。
          只报前一件就会让人以为填完令牌就能敲 */}
      <div className="mt-6 rounded-lg border border-border bg-surface p-3">
        <div className="flex items-baseline justify-between gap-3">
          <p className="text-sm font-medium text-foreground">本机监听</p>
          <CapabilityToggle
            label="本机监听"
            enabled={config.webhookInEnabled}
            onToggle={() => void updateConfig({ webhookInEnabled: !config.webhookInEnabled })}
          />
        </div>
        <p className="mt-1 text-xs leading-5 text-muted-foreground">
          开了之后，本机的另一个进程拿任务上填的令牌就能点着那一条：
          <span className="break-all text-foreground">
            POST http://127.0.0.1:{config.webhookInPort}/hook/&lt;令牌&gt;
          </span>
          走的是「立刻运行」同一条路，"同一任务不叠开发"照旧（正跑着就回 409）。
          只绑 127.0.0.1，没有对外的地址可填；
          <span className="text-foreground">改开关或端口要重启一次才生效</span>。
        </p>
        <div className="mt-2 flex flex-wrap items-center gap-2">
          <span className="text-xs text-muted-foreground">端口</span>
          <input aria-label="webhook 入站端口"
            type="number"
            min={1}
            max={65535}
            value={config.webhookInPort}
            onChange={(event) => {
              const value = Math.round(Number(event.target.value));
              // 空与 0 都不写回去：让系统选一个随机端口等于让自己去找它
              if (Number.isFinite(value) && value >= 1 && value <= 65535) {
                void updateConfig({ webhookInPort: value });
              }
            }}
            className="h-7 w-24 rounded-lg border border-input bg-background px-2 text-sm text-foreground outline-none transition-colors focus-visible:border-brand/50"
          />
          <span className="text-xs text-muted-foreground">
            {withToken > 0
              ? `${withToken} 条任务配了令牌${
                  config.webhookInEnabled ? "" : "，开关没开，此刻谁敲都不理"
                }`
              : "还没有任务配令牌：开了也没东西可点"}
          </span>
        </div>
      </div>

      <div className="mt-4 rounded-lg border border-border bg-surface p-3">
        <div className="flex items-baseline justify-between gap-3">
          <p className="text-sm font-medium text-foreground">点过头的 {nodded.length} 发</p>
        </div>
        <p className="mt-1 text-xs leading-5 text-muted-foreground">
          每一条都还在替它盖过的那一发说话：同一份参数以后不再问。撤回就是把那一步放回
          <span className="text-foreground">会问一次</span>；
          已经执行过的那个动作不由这里撤销，那走变更请求的回滚。
        </p>
        {nodded.length === 0 ? (
          <p className="mt-2 text-xs text-muted-foreground">
            还没有一条先例：任务跑到没盖过章的那一步，会照常停下来等人点头。
          </p>
        ) : (
          <ul className="mt-2 space-y-2">
            {nodded.map((item) => (
              <li
                key={item.id}
                className="rounded-lg border border-border bg-background px-3 py-2.5"
              >
                <p className="text-sm break-words text-foreground">{item.target}</p>
                <p className="mt-1 text-xs text-muted-foreground">
                  {item.status === "approved" ? "已批准" : "已拒绝"} · {item.capability} ·{" "}
                  {agoText(item.decidedAt ?? item.requestedAt)} · 运行 {item.runId}
                </p>
                <div className="mt-2">
                  <Button
                    variant="outline"
                    size="sm"
                    className="text-destructive hover:border-destructive/50 hover:bg-destructive/10 hover:text-destructive"
                    disabled={revoking === item.id}
                    onClick={() => void revoke(item.id)}
                  >
                    {revoking === item.id ? "删除中…" : "撤回并删除"}
                  </Button>
                </div>
              </li>
            ))}
          </ul>
        )}
        {forgetError ? <p className="mt-2 text-xs text-destructive">{forgetError}</p> : null}
        {forgetNote && !forgetError ? (
          <p className="mt-2 text-xs text-muted-foreground">{forgetNote}</p>
        ) : null}
      </div>

      <div className="mt-4 rounded-lg border border-border bg-surface p-3">
        <div className="flex items-baseline justify-between gap-3">
          <p className="text-sm font-medium text-foreground">运行记录</p>
          <span className="shrink-0 text-xs text-muted-foreground">
            {runs === null ? "还没读到" : `${runs.length} 发`}
          </span>
        </div>
        <p className="mt-1 text-xs leading-5 text-muted-foreground">
          账本只追加：每一发跑成什么样、各格花了多少都在里面。跑到半路的那一发可以
          <span className="text-foreground">从检查点续跑</span>
          ——已经跑成的格子不会再花一次钱，判据是账本而不是内存，所以重启之后答案一样。
          下面那格是这条规则唯一的例外，它抹得掉的只有"已有结论、且不是某个任务最新一发"的老账：
          最新那一发是调度器算欠账的锚点，抽掉它会补跑出一批真花钱的运行。
        </p>
        <div className="mt-2 flex flex-wrap items-center gap-2">
          <label className="flex items-center gap-1.5 text-xs text-muted-foreground">
            清掉
            <input
              type="number"
              min={1}
              max={3650}
              value={purgeDays}
              aria-label="清掉多少天前的已了结记录"
              className="h-6 w-14 rounded border border-border bg-background px-1 text-xs text-foreground outline-none focus-visible:border-brand/60 focus-visible:ring-2 focus-visible:ring-ring/55"
              onChange={(event) =>
                setPurgeDays(
                  Math.max(1, Math.min(3650, Math.round(Number(event.target.value) || 1))),
                )
              }
            />
            天前
          </label>
          <Button
            variant="outline"
            size="sm"
            className={cn(
              "text-destructive",
              purgeArmed
                ? "border-destructive/60 bg-destructive/15"
                : "hover:border-destructive/50 hover:bg-destructive/10 hover:text-destructive",
            )}
            disabled={purging || runs === null || runs.length === 0}
            onClick={() => {
              // 第一下只把按钮换成"再点一次"：这一格一次动的是 N 条账，不是一条
              if (!purgeArmed) {
                setPurgeArmed(true);
                setRunsError(null);
                setRunsNote(null);
                return;
              }
              setPurgeArmed(false);
              setPurging(true);
              void tasksRunsPurge(purgeDays)
                .then((gone) => {
                  setRunsNote(
                    gone > 0
                      ? `抹掉 ${gone} 发。没跑完的那些与各任务最新一发未动。`
                      : `没有比 ${purgeDays} 天更老、且抹得掉的发。`,
                  );
                  return refreshRuns();
                })
                .catch((cause) =>
                  setRunsError(cause instanceof Error ? cause.message : String(cause)),
                )
                .finally(() => setPurging(false));
            }}
          >
            {purging ? "清除中…" : purgeArmed ? "再点一次确认清除" : "清除旧记录"}
          </Button>
          {runsNote ? (
            <span className="min-w-0 text-xs text-muted-foreground">{runsNote}</span>
          ) : null}
        </div>
        {runsError ? <p className="mt-2 text-xs text-destructive">{runsError}</p> : null}
        {runs === null ? (
          <p className="mt-2 text-xs text-muted-foreground">还没读到账本，说不上跑过几发。</p>
        ) : runs.length === 0 ? (
          <p className="mt-2 text-xs text-muted-foreground">还没有一次运行记进账本。</p>
        ) : (
          <ul className="mt-2 space-y-2">
            {runs.map((run) => (
              <li key={run.runId} className="rounded-lg border border-border bg-background px-3 py-2.5">
                <p className="text-sm text-foreground">
                  <span className={cn("rounded-md border px-1.5 py-0.5 text-2xs", RUN_TONE[run.status])}>
                    {RUN_LABEL[run.status]}
                  </span>
                  <span className="ml-2">{agoText(run.startedAt)}</span>
                  <span className="ml-2 text-muted-foreground">
                    {STARTED_BY[run.startedBy]}
                  </span>
                </p>
                {run.nodes.length > 0 ? (
                  <div className="mt-1.5 flex flex-wrap gap-1.5">
                    {run.nodes.map((node) => (
                      <span
                        key={node.nodeId}
                        className={cn("rounded-md border px-1.5 py-0.5 text-2xs", RUN_TONE[node.status])}
                      >
                        {node.nodeId} {RUN_LABEL[node.status]}
                        {node.delegated ? " · 子助理" : ""}
                        {node.costUsd > 0 ? ` · $${node.costUsd.toFixed(4)}` : ""}
                      </span>
                    ))}
                  </div>
                ) : null}
                {run.error ? (
                  <p className="mt-1.5 text-xs leading-5 break-words text-muted-foreground">
                    {run.error}
                  </p>
                ) : null}
                <p className="mt-1 text-2xs text-muted-foreground">
                  这发成本{" "}
                  {run.costUsd > 0 ? `$${run.costUsd.toFixed(4)}` : run.cost ? "$0" : "还没记账"}
                  {run.cost && run.cost.unpricedRequests > 0
                    ? ` · ${run.cost.unpricedRequests} 次没定价`
                    : ""}
                </p>
                {run.delivery ? (
                  <p className="mt-1 text-2xs break-words text-muted-foreground">
                    {run.delivery.sent
                      ? `通知已投 HTTP ${run.delivery.status} · 试了 ${run.delivery.attempts} 次`
                      : "通知没投出去"}
                    {run.delivery.note ? ` · ${run.delivery.note}` : ""}
                  </p>
                ) : null}
                {run.unfinished ? (
                  <Button
                    variant="subtle"
                    size="sm"
                    className="mt-1.5"
                    disabled={resuming === run.runId}
                    onClick={() => void resume(run.runId)}
                  >
                    {resuming === run.runId ? "续跑中…" : "从检查点续跑"}
                  </Button>
                ) : null}
              </li>
            ))}
          </ul>
        )}
      </div>
    </FormColumn>
  );
}
