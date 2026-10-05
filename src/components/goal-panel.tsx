import { useEffect, useMemo, useState } from "react";
import {
  IconAiAgent as Bot,
  IconChevronDown as ChevronDown,
  IconPlayerPause as PlayerPause,
  IconPlayerPlay as PlayerPlay,
  IconSquare as Square,
  IconTarget as Target,
  IconTerminal2 as Terminal2,
  IconTool as Tool,
  IconX as X,
} from "@tabler/icons-react";

import { backgroundCommandsList } from "@/lib/chat-transport";
import { formatUsd, spentLabel } from "@/lib/format";
import {
  GOAL_RESUMABLE,
  GOAL_STATUS_WORD,
  goalNoteIsWarning,
  goalNoteLead,
} from "@/lib/goal-status";
import { Button } from "@/components/ui/button";
import { useChatStore } from "@/store/chat-store";
import { cn } from "@/lib/utils";
import { MODE_LEVELS, type GoalPanelEntry, type RunningCommand, type WorkingMode } from "@/types/chat";

/** 交互档的中文短名，用选择器那张表——两处各写一份迟早漂成两句 */
const tierLabel = (mode: WorkingMode) =>
  MODE_LEVELS.find((item) => item.value === mode)?.label ?? mode;

/** 一条目标的短状态字。查的是 `goal-status` 那张表——它与目标带共用一份，
 *  两处各写一份迟早漂成两句不同的话。
 *  这里**没有"待命"**：那个词是 `outcome` + `paused` 两格互相解释时生出来的第五个名字，
 *  六值之后 `pending`（还在出字）与状态是正交的，由状态点要不要脉冲来说 */
function statusWord(entry: GoalPanelEntry): { word: string; className: string } {
  const base = GOAL_STATUS_WORD[entry.status];
  return { word: base.word, className: base.tone };
}

function statusDot(entry: GoalPanelEntry): string {
  if (entry.pending && entry.status === "active") return "animate-pulse bg-brand";
  if (entry.status === "active") return "bg-brand/60";
  if (entry.status === "paused") return "bg-brand/50";
  return "bg-border";
}

/** 分叉继承的那一支：后端落的是 paused + 「分叉继承」那句 note（§5.5）。
 *  行上必须看得出"它不会自己往下跑"——徽标长在行上，不许只放 tooltip */
function isInherited(entry: GoalPanelEntry): boolean {
  return entry.status !== "complete" && (entry.note ?? "").includes("分叉继承");
}

/** 目标卡里的一条。收起是一行（状态 + 目标 + 读数），点开有尾巴与按钮 */
function DockEntry({
  entry,
  expanded,
  onToggle,
}: {
  entry: GoalPanelEntry;
  expanded: boolean;
  onToggle: () => void;
}) {
  const goalPause = useChatStore((s) => s.goalPause);
  const goalResume = useChatStore((s) => s.goalResume);
  const goalDiscard = useChatStore((s) => s.goalDiscard);
  const stopRun = useChatStore((s) => s.stopRun);
  const openConversation = useChatStore((s) => s.openConversation);
  const profileName = useChatStore((s) =>
    entry.profile
      ? (s.config.profiles.find((card) => card.id === entry.profile)?.name ?? "已删除的档案")
      : null,
  );
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const status = statusWord(entry);
  const running = entry.status === "active";

  async function act(action: () => Promise<string | null>) {
    setBusy(true);
    setError(null);
    const message = await action();
    if (message) setError(message);
    setBusy(false);
  }

  return (
    <li>
      <button
        type="button"
        onClick={onToggle}
        title={expanded ? "收起" : entry.objective}
        className="flex w-full cursor-pointer items-center gap-1.5 rounded-lg px-1.5 py-1 text-left outline-none transition-colors hover:bg-accent/60 focus-visible:ring-2 focus-visible:ring-ring/45"
      >
        <span className={cn("size-1.5 shrink-0 rounded-full", statusDot(entry))} />
        <span className="min-w-0 flex-1 truncate text-xs leading-4 text-foreground">
          {entry.objective || "（没写下目标）"}
        </span>
        {isInherited(entry) ? (
          <span className="shrink-0 rounded-full bg-border/60 px-1.5 text-2xs text-muted-foreground">
            继承 · 未启动
          </span>
        ) : null}
        <span className={cn("shrink-0 text-2xs tabular-nums", status.className)}>
          {status.word}
        </span>
        <span className="shrink-0 text-2xs tabular-nums text-muted-foreground">
          {entry.turnsUsed}轮
        </span>
      </button>

      {expanded ? (
        <div className="px-2 pb-1.5">
          <p className="text-2xs tabular-nums text-muted-foreground">
            已跑 {entry.turnsUsed} 轮 · 已花 {spentLabel(entry.spentUsdE8)}
            {entry.maxCostUsdE8 > 0
              ? ` / 上限 ${formatUsd(entry.maxCostUsdE8 / 1e8)}`
              : " / 不设上限"}
          </p>
          {/* 点名了档案就说得出是谁在跑；顺便报出这条话题当下的交互档——
              目标在对话档下也在推进，这一行是那件事唯一的凭据 */}
          <p className="mt-1 text-2xs leading-4 text-muted-foreground">
            {entry.profile
              ? `由「${profileName ?? "…"}」执行 · `
              : "跟随当前配置 · "}
            当前{tierLabel(entry.mode)}档
          </p>
          {entry.note ? (
            <p
              className={cn(
                "mt-1 text-2xs leading-4",
                goalNoteIsWarning(entry.status) ? "text-destructive" : "text-muted-foreground",
              )}
            >
              {goalNoteLead(entry.status)}
              {entry.note}
            </p>
          ) : null}
          {entry.tail ? (
            <p className="mt-1 max-h-24 overflow-y-auto whitespace-pre-wrap text-2xs leading-4 text-muted-foreground">
              {entry.tail}
            </p>
          ) : null}
          {error ? <p className="mt-1 text-2xs text-destructive">{error}</p> : null}
          <div className="mt-1.5 flex flex-wrap items-center gap-1">
            {GOAL_RESUMABLE.includes(entry.status) ? (
              <Button
                size="sm"
                variant="brand"
                className="h-6 px-2 text-xs"
                disabled={busy}
                onClick={() => void act(() => goalResume(entry.conversationId))}
              >
                <PlayerPlay className="size-3" />
                继续
              </Button>
            ) : null}
            {running ? (
              <Button
                size="sm"
                variant="subtle"
                className="h-6 px-2 text-xs"
                disabled={busy}
                onClick={() => void act(() => goalPause(entry.conversationId, true))}
              >
                <PlayerPause className="size-3" />
                暂停
              </Button>
            ) : null}
            {entry.pending ? (
              <Button
                size="sm"
                variant="subtle"
                className="h-6 px-2 text-xs"
                disabled={busy}
                title="只停正在出字的这一轮。目标本身不受影响，它接着往下推——要停下请用暂停或结束"
                onClick={() => void stopRun(entry.conversationId)}
              >
                <Square className="size-3" />
                停止
              </Button>
            ) : null}
            {!entry.pending ? (
              <Button
                size="sm"
                variant="ghost"
                className="h-6 px-2 text-xs hover:text-destructive"
                disabled={busy}
                title="整份清掉这个目标（目标、轮数、点名的档案都不保留），交互档不变"
                onClick={() => void act(() => goalDiscard(entry.conversationId))}
              >
                <X className="size-3" />
                结束
              </Button>
            ) : null}
            <Button
              size="sm"
              variant="ghost"
              className="h-6 px-2 text-xs"
              onClick={() => void openConversation(entry.conversationId)}
            >
              打开话题
            </Button>
          </div>
        </div>
      ) : null}
    </li>
  );
}

/** 目标卡：后台那些自己一轮一轮往下推的目标。有目标在册时浮在对话区左下角 */
function GoalCard({ entries }: { entries: GoalPanelEntry[] }) {
  const loadMode = useChatStore((s) => s.loadMode);
  const [collapsed, setCollapsed] = useState(false);
  const [expandedId, setExpandedId] = useState<string | null>(null);

  const activeCount = entries.filter((entry) => entry.status === "active").length;
  const anyRunning = entries.some((entry) => entry.pending);

  // 同一 goal_id 的几支合成一组（design-goal-mode.md §5.5）：分叉出的话题抄走同一支
  // 目标，没有身份就只能靠 objective 文本相同来猜。没有身份的旧行各自成组
  const groups = useMemo(() => {
    const map = new Map<string, GoalPanelEntry[]>();
    for (const entry of entries) {
      const key = entry.goalId ?? `solo-${entry.conversationId}`;
      map.set(key, [...(map.get(key) ?? []), entry]);
    }
    return [...map.values()];
  }, [entries]);

  return (
    <div className="pointer-events-auto overflow-hidden rounded-xl border border-border bg-surface/95 shadow-sm backdrop-blur">
      <button
        type="button"
        onClick={() => setCollapsed((held) => !held)}
        className="flex w-full cursor-pointer items-center gap-1.5 px-2.5 py-1.5 text-left outline-none transition-colors hover:bg-accent/60 focus-visible:ring-2 focus-visible:ring-ring/45"
      >
        <Target className={cn("size-3.5 shrink-0 text-brand-text", anyRunning && "animate-pulse")} />
        <span className="shrink-0 text-xs font-medium text-foreground">目标</span>
        {activeCount > 0 ? (
          <span className="rounded-full bg-brand/15 px-1.5 text-2xs font-medium tabular-nums text-brand-text">
            {activeCount} 在推进
          </span>
        ) : null}
        <ChevronDown
          className={cn(
            "ml-auto size-3 shrink-0 text-muted-foreground transition-transform",
            !collapsed && "rotate-180",
          )}
        />
      </button>

      {!collapsed ? (
        // 打开就把每条的读数问一遍：钱与轮数只在轮次边界更新，停一会儿再展开不该是旧账
        <ul
          onMouseEnter={() => {
            for (const entry of entries) void loadMode(entry.conversationId);
          }}
          className="max-h-64 space-y-0.5 overflow-y-auto border-t border-border/70 p-1"
        >
          {groups.map((group) => (
            <div key={group[0].goalId ?? group[0].conversationId}>
              {group.length > 1 ? (
                <p className="px-1.5 pt-1 text-2xs leading-4 text-muted-foreground">
                  {group.length} 支话题挂着同一目标 · 只有 {group.filter((e) => e.status === "active").length} 支在跑
                </p>
              ) : null}
              {group.map((entry) => (
                <DockEntry
                  key={entry.conversationId}
                  entry={entry}
                  expanded={expandedId === entry.conversationId}
                  onToggle={() =>
                    setExpandedId((held) =>
                      held === entry.conversationId ? null : entry.conversationId,
                    )
                  }
                />
              ))}
            </div>
          ))}
        </ul>
      ) : null}
    </div>
  );
}

/** 后台活动卡：这一条话题眼下还在跑的东西——后台指令、执行中的工具、子助理。 */
function ActivityCard({
  commands,
  tools,
  agents,
}: {
  commands: number;
  tools: number;
  agents: number;
}) {
  const items = [
    { icon: Terminal2, label: "指令", count: commands, title: "本话题启动、还在跑的后台命令（run_command background）" },
    { icon: Tool, label: "工具", count: tools, title: "正在执行中的工具调用" },
    { icon: Bot, label: "子助理", count: agents, title: "正在跑的子助理（spawn_subagent）" },
  ].filter((item) => item.count > 0);

  return (
    <div
      className="pointer-events-auto flex items-center gap-2 rounded-xl border border-border bg-surface/95 px-2.5 py-1.5 shadow-sm backdrop-blur"
      title="这一条话题后台还在跑的东西。指令按启动它的话题归属；工具与子助理是正在执行中的调用"
    >
      {items.map((item) => (
        <span key={item.label} title={item.title} className="flex items-center gap-1">
          <item.icon className="size-3 shrink-0 text-muted-foreground" />
          <span className="text-2xs text-muted-foreground">{item.label}</span>
          <span className="text-xs font-medium tabular-nums text-foreground">
            {item.count}
          </span>
        </span>
      ))}
    </div>
  );
}

/**
 * 对话区左下角的浮层小卡片：目标 + 后台活动。
 *
 * 容器 pointer-events-none——盖在消息流上也不挡滚动与点选；卡片自己再把手接回来。
 * 数据都是投影：目标来自 store 的 `goalRuns`，后台指令按 2.5s 问一遍登记表
 * （句柄是进程内存活，没有事件可听），工具与子助理数的是正在跑的那几格调用。
 */
export function GoalDock() {
  const goalRuns = useChatStore((s) => s.goalRuns);
  const activeId = useChatStore((s) => s.activeId);
  const pending = useChatStore((s) => s.pending);
  const messages = useChatStore((s) => s.messages);
  const [commands, setCommands] = useState<RunningCommand[]>([]);

  // 后台命令没有事件可听（进程在登记表里自己长跑）：开着就低频问一遍。
  // 读不到就维持上一份——这一格只影响显示，不值得为它报错
  useEffect(() => {
    let cancelled = false;
    const tick = async () => {
      try {
        const list = await backgroundCommandsList();
        if (!cancelled) setCommands(list);
      } catch {
        // 维持上一份
      }
    };
    void tick();
    const timer = window.setInterval(() => void tick(), 2500);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, []);

  // 工具与子助理：正在执行中的调用。只有活着的一轮里才有真的"执行中"——
  // 落盘的历史消息可能残留没关上的调用格，拿 pending 把它们挡在外面
  const runningCalls = useMemo(
    () =>
      pending
        ? messages.flatMap((message) => message.toolCalls ?? []).filter(
            (call) => call.status === "running" || call.status === "pending",
          )
        : [],
    [pending, messages],
  );
  const agents = runningCalls.filter((call) => call.name === "spawn_subagent").length;
  const tools = runningCalls.length - agents;
  const myCommands = commands.filter((row) => row.owner === activeId).length;

  // 收尾的目标也留在卡上（本轮程里活动过的那些），但推进中的排前面。
  // **当前这一支不在这里**：它长在输入框上方那条 `GoalStrip` 上，同一个数印两遍就是等着漂
  const ordered = useMemo(() => {
    const rank = (entry: GoalPanelEntry) => (entry.status === "active" ? 0 : 1);
    return goalRuns
      .filter((entry) => entry.conversationId !== activeId)
      .sort((a, b) => rank(a) - rank(b));
  }, [goalRuns, activeId]);

  const hasGoals = ordered.length > 0;
  const hasActivity = myCommands > 0 || runningCalls.length > 0;
  if (!hasGoals && !hasActivity) return null;

  return (
    <div className="pointer-events-none absolute bottom-4 left-4 z-20 flex max-w-[300px] flex-col gap-2">
      {hasGoals ? <GoalCard entries={ordered} /> : null}
      {hasActivity ? (
        <ActivityCard commands={myCommands} tools={tools} agents={agents} />
      ) : null}
    </div>
  );
}
