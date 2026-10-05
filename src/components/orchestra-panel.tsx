import { useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";

import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { useChatStore } from "@/store/chat-store";
import {
  RUN_EVENT_NAME,
  SHAPE_LABELS,
  STATUS_LABELS,
  event_label,
  orchestraBoard,
  orchestraCancel,
  orchestraEditEdge,
  orchestraSetEdgeKind,
  orchestraLedger,
  orchestraPause,
  orchestraPlanBrief,
  orchestraPlans,
  orchestraResume,
  orchestraRerunNode,
  orchestraStart,
  orchestraStatus,
  PRIORITY_HINTS,
  PRIORITY_LABELS,
  type EmittedRunEvent,
  type PlanBrief,
  type PlanPriority,
  type PlanShape,
  type PlanView,
  type NodeView,
  type TraceRow,
} from "@/lib/orchestra";

function text_of(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}

const SHAPES = Object.keys(SHAPE_LABELS) as PlanShape[];
const PRIORITIES = Object.keys(PRIORITY_LABELS) as PlanPriority[];

/** 话题 id 是 `plan-<计划 id>-<节点>-a<尝试>`，把它还原成节点名。
 *  只在展示时用——判定不靠它，判定的事实住在账本里 */
/** 形状检查那两个列表的写法：逗号或换行分隔，空白条目一条不留。
 *  剪空白这件事后端也做了一遍——两边都得做，因为"一条空 must_contain"会把每一格都判死 */
const list_of = (text: string) =>
  text
    .split(/[,，\n]/)
    .map((item) => item.trim())
    .filter(Boolean);

function node_of(conversationId: string, planId: string): string {
  const prefix = `plan-${planId}-`;
  if (!conversationId.startsWith(prefix)) return "";
  return conversationId.slice(prefix.length).replace(/-a\d+$/, "");
}

/** 集合项那一栏的写法：一行一个。预览与开跑必须读同一个解析，否则面板说的那几张格
 *  和真跑起来的那张图可以不是同一张 */
const items_of = (text: string) => text.split("\n").map((line) => line.trim()).filter(Boolean);

/** 一格多大。写死而不是量出来：量到的那份在窗口没跑起来时会全是 0 */
const CELL = { width: 88, height: 24, gapX: 28, gapY: 12 };

/** 节点在图上的位置：列 = depth，列内按 id 定序（刷新前后同一个节点不会跳位），整列垂直居中。
 *  全是算术——不量 DOM，那样拼出来的布局一旦量不到就是错位的图，而错位没人报 */
function dag_positions(nodes: NodeView[]): Map<string, { x: number; y: number }> {
  const columns = new Map<number, NodeView[]>();
  for (const node of [...nodes].sort((a, b) => a.depth - b.depth || a.id.localeCompare(b.id))) {
    const column = columns.get(node.depth) ?? [];
    column.push(node);
    columns.set(node.depth, column);
  }
  const step = CELL.height + CELL.gapY;
  const tallest = Math.max(1, ...[...columns.values()].map((column) => column.length));
  const at = new Map<string, { x: number; y: number }>();
  for (const [depth, column] of columns) {
    const offset = ((tallest - column.length) * step) / 2;
    column.forEach((node, index) => {
      at.set(node.id, { x: depth * (CELL.width + CELL.gapX), y: offset + index * step });
    });
  }
  return at;
}

/** DAG 的形状：它只答"谁等谁"。状态、成本、输出、重跑仍在下面那份列表里，
 *  两张视图读的是同一个 `view.nodes`——图不存自己那份节点状态 */
function DagShape({ nodes, critical }: { nodes: NodeView[]; critical: string[] }) {
  const at = dag_positions(nodes);
  const spots = [...at.values()];
  if (spots.length === 0) return null;
  const width = Math.max(...spots.map((spot) => spot.x)) + CELL.width + 2;
  const height = Math.max(...spots.map((spot) => spot.y)) + CELL.height + 2;

  const lines: { x1: number; y1: number; x2: number; y2: number; onPath: boolean }[] = [];
  for (const node of nodes) {
    const to = at.get(node.id);
    if (!to) continue;
    for (const dependency of node.dependsOn) {
      const from = at.get(dependency);
      if (!from) continue;
      lines.push({
        x1: from.x + CELL.width,
        y1: from.y + CELL.height / 2,
        x2: to.x,
        y2: to.y + CELL.height / 2,
        // 两端都在关键路径上才算"拖慢整个计划的那一条"：单端在不算
        onPath: critical.includes(node.id) && critical.includes(dependency),
      });
    }
  }

  return (
    <div className="mt-2 overflow-x-auto">
      <svg
        width={width}
        height={height}
        viewBox={`0 0 ${width} ${height}`}
        role="img"
        aria-label="任务图"
      >
        {lines.map((line, index) => (
          <path
            key={`${line.x1}-${line.y1}-${line.x2}-${line.y2}-${index}`}
            d={`M ${line.x1} ${line.y1} C ${line.x1 + 16} ${line.y1}, ${line.x2 - 16} ${line.y2}, ${line.x2} ${line.y2}`}
            className={line.onPath ? "text-brand-text" : "text-border"}
            fill="none"
            stroke="currentColor"
            strokeWidth={line.onPath ? 1.6 : 1}
          />
        ))}
        {nodes.map((node) => {
          const spot = at.get(node.id);
          if (!spot) return null;
          return (
            <g
              key={node.id}
              className={critical.includes(node.id) ? "text-brand-text" : "text-muted-foreground"}
            >
              <title>{`${node.id} · ${STATUS_LABELS[node.status]}`}</title>
              <rect
                x={spot.x}
                y={spot.y}
                width={CELL.width}
                height={CELL.height}
                rx={6}
                fill="none"
                stroke="currentColor"
              />
              <text x={spot.x + 6} y={spot.y + 16} fill="currentColor" fontSize={10}>
                {node.id.length > 12 ? `${node.id.slice(0, 11)}…` : node.id}
              </text>
            </g>
          );
        })}
      </svg>
    </div>
  );
}

/** 改一条依赖边（design-multi-agent.md §5.13）。它是那张图的**写侧**：图能看不能改的话，
 *  "用户可干预"就只完成了一半。三条限制都在这儿说明：要先暂停、只能动还没开始的节点、成环会被拒 */
function EdgeEditor({
  planId,
  nodes,
  paused,
  busy,
  onAct,
}: {
  planId: string;
  nodes: NodeView[];
  paused: boolean;
  busy: boolean;
  onAct: (action: () => Promise<unknown>) => Promise<void>;
}) {
  const [waiting, setWaiting] = useState("");
  const [held, setHeld] = useState("");
  // 循环那一档的轮数上限。1..=8：后端那条命令也认这个界，两边不是各写一次规则，
  // 而是这里不收的数根本发不出去
  const [rounds, setRounds] = useState(2);
  const ids = nodes.map((node) => node.id).sort();
  const waiter = waiting || ids[0] || "";
  const upstream = held || ids.find((id) => id !== waiter) || waiter;
  const pick = (value: string, onChange: (next: string) => void, label: string) => (
    <Select value={value} onValueChange={onChange}>
      <SelectTrigger
        aria-label={label}
        className="h-6 w-auto max-w-[11rem] shrink-0 gap-1 rounded px-1.5 font-mono text-xs"
      >
        <SelectValue placeholder="没有节点" />
      </SelectTrigger>
      <SelectContent>
        {ids.map((id) => (
          <SelectItem key={id} value={id} className="font-mono text-xs">
            {id}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );

  return (
    <div className="mt-2 flex flex-wrap items-center gap-2 text-xs">
      <span className="text-muted-foreground">改依赖：</span>
      {pick(waiter, setWaiting, "谁等")}
      <span className="text-muted-foreground">等</span>
      {pick(upstream, setHeld, "等谁")}
      <Button
        size="sm"
        variant="subtle"
        disabled={!paused || busy}
        onClick={() => void onAct(() => orchestraEditEdge(planId, upstream, waiter, true))}
      >
        加上
      </Button>
      <Button
        size="sm"
        variant="subtle"
        disabled={!paused || busy}
        onClick={() => void onAct(() => orchestraEditEdge(planId, upstream, waiter, false))}
      >
        去掉
      </Button>
      <span className="text-muted-foreground">
        {paused ? "只能动还没开始的节点；会成环的改法直接被拒" : "要先暂停这份计划才改得动"}
      </span>

      {/* 放行方式那一行（design-multi-agent.md §5.28）。这一排按钮补的是"四种边里两种没有写侧"：
          判据早就在读某一格的校验结论，而生产代码从来不往那个键写东西。
          这里没有"键名"可填是刻意的——能选出来的边，读的键必须是有人在写的那个 */}
      <span className="mt-1.5 flex w-full flex-wrap items-center gap-2 border-t border-border pt-2">
        <span className="text-muted-foreground">放行方式（{waiter}）</span>
        <Button
          size="sm"
          variant="subtle"
          disabled={!paused || busy}
          onClick={() =>
            void onAct(() => orchestraSetEdgeKind(planId, waiter, { kind: "finishToStart" }))
          }
        >
          跑完就放行
        </Button>
        <Button
          size="sm"
          variant="subtle"
          disabled={!paused || busy || upstream === waiter}
          onClick={() =>
            void onAct(() =>
              orchestraSetEdgeKind(planId, waiter, { kind: "waitForVerdict", node: upstream, pass: true }),
            )
          }
        >
          等 {upstream} 过了校验
        </Button>
        <Button
          size="sm"
          variant="subtle"
          disabled={!paused || busy || upstream === waiter}
          onClick={() =>
            void onAct(() =>
              orchestraSetEdgeKind(planId, waiter, { kind: "waitForVerdict", node: upstream, pass: false }),
            )
          }
        >
          等它没过
        </Button>
        <Button
          size="sm"
          variant="subtle"
          disabled={!paused || busy}
          onClick={() =>
            void onAct(() =>
              orchestraSetEdgeKind(planId, waiter, { kind: "iterateUntilPass", maxIters: rounds }),
            )
          }
        >
          自己跑到过校验
        </Button>
        <input
          type="number"
          min={1}
          max={8}
          value={rounds}
          aria-label="循环轮数上限"
          className="h-6 w-12 rounded border border-border bg-background px-1 text-xs outline-none focus-visible:border-brand/60 focus-visible:ring-2 focus-visible:ring-ring/55"
          onChange={(event) =>
            setRounds(Math.max(1, Math.min(8, Math.round(Number(event.target.value) || 1))))
          }
        />
        <span className="text-muted-foreground">
          这几种读的是账本里已经记下的结论，重启之后还认；此刻黑板上有哪些键看下面那块
        </span>
      </span>
    </div>
  );
}

/** 1e-8 美元 → 看得懂的数。台账与预算都存整数，只在要给人看的那一刻换算一次 */
function usdOf(e8: number) {
  return `$${(e8 / 1e8).toFixed(4)}`;
}

export function OrchestraPanel() {
  const [goal, setGoal] = useState("");
  const [shape, setShape] = useState<PlanShape>("fanout");
  const [branches, setBranches] = useState(3);
  const [maxParallel, setMaxParallel] = useState(1);
  // 每一格自己的花费上限（美元）。0 = 不设：那是今天所有既有计划的形状
  const [nodeCapUsd, setNodeCapUsd] = useState(0);
  // 形状检查的三项。全空 = 没说 = 今天的读法（非空即可）
  const [minChars, setMinChars] = useState(0);
  const [mustContain, setMustContain] = useState("");
  const [forbid, setForbid] = useState("");
  const [priority, setPriority] = useState<PlanPriority>("normal");
  const [items, setItems] = useState("");
  // 装配这张图的规模，由后端从真构造器算。null = 还没问到，那这一句一个字也不说
  const [brief, setBrief] = useState<PlanBrief | null>(null);
  const [planId, setPlanId] = useState("");
  const [view, setView] = useState<PlanView | null>(null);
  const [board, setBoard] = useState<string[]>([]);
  const [trace, setTrace] = useState<TraceRow[]>([]);
  const [live, setLive] = useState<Record<string, string>>({});
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const polling = useRef<number | null>(null);

  /** 切走标签页再回来，planId 会随组件卸载一起没了：正在跑的计划不该因此变成看不见的东西 */
  useEffect(() => {
    if (planId) return;
    void orchestraPlans()
      .then((ids) => {
        if (ids.length > 0) setPlanId(ids[ids.length - 1]);
      })
      .catch(() => {
        // 后端还没起过任何计划：这里没什么可恢复的，也不该拿一次失败吓用户
      });
  }, [planId]);

  /** 后台 run 的事件与聊天里同一种形状，只是多包了一层归属：按节点 id 收增量 */
  useEffect(() => {
    if (!planId) return;
    let cancelled = false;
    const stop = listen<EmittedRunEvent>(RUN_EVENT_NAME, (message) => {
      const { conversationId, event } = message.payload;
      if (cancelled) return;
      const node = node_of(conversationId, planId);
      if (!node) return;
      if (event.type === "delta") {
        setLive((held) => ({ ...held, [node]: ((held[node] ?? "") + event.text).slice(-600) }));
      } else if (event.type === "error") {
        setLive((held) => ({ ...held, [node]: `服务商报错：${event.message}` }));
      }
    });
    return () => {
      cancelled = true;
      void stop.then((unlisten) => unlisten());
    };
  }, [planId]);

  // 状态是派生值：只在跑着的时候轮询，落定了就停手
  useEffect(() => {
    if (!planId) return;
    let cancelled = false;
    const refresh = async () => {
      try {
        const next = await orchestraStatus(planId);
        if (cancelled) return;
        setView(next);
        setBoard(await orchestraBoard(planId));
        setTrace(await orchestraLedger(planId));
        if (next.finished && polling.current !== null) {
          window.clearInterval(polling.current);
          polling.current = null;
        }
      } catch (cause) {
        if (!cancelled) setError(text_of(cause));
      }
    };
    void refresh();
    polling.current = window.setInterval(() => void refresh(), 1500);
    return () => {
      cancelled = true;
      if (polling.current !== null) window.clearInterval(polling.current);
    };
  }, [planId]);

  /** 开跑之前问一句"这张图多大"。这三个数由后端那座真构造器给：面板以前按轮数自己
   *  算过请求数，那等于把辩论的形状在另一端抄了一份——抄的那一刻起，构造器改形就只会
   *  让这句话在用户决定要不要花钱的时刻说一句假话 */
  useEffect(() => {
    let cancelled = false;
    orchestraPlanBrief({ shape, branches, items: items_of(items) })
      .then((next) => {
        if (!cancelled) setBrief(next);
      })
      .catch(() => {
        // 问不到就一个字也不显示。这里宁缺勿假
        if (!cancelled) setBrief(null);
      });
    return () => {
      cancelled = true;
    };
  }, [shape, branches, items]);

  const act = async (action: () => Promise<unknown>) => {
    setBusy(true);
    setError("");
    try {
      await action();
    } catch (cause) {
      setError(text_of(cause));
    } finally {
      setBusy(false);
    }
  };

  const running = view ? !view.finished : false;

  return (
    <div className="space-y-4">
      <div className="rounded-lg border border-border bg-surface p-3">
        <p className="text-xs leading-5 text-muted-foreground">
          一份目标拆成几个节点并行跑。每个节点都有自己的上下文、工具白名单与预算，
          默认一次只跑一支——并行是按真金白银计的，不是免费的。
        </p>
        <textarea
          value={goal}
          onChange={(event) => setGoal(event.target.value)}
          placeholder="要它们一起办的那件事"
          className="mt-2 min-h-16 w-full resize-y rounded-lg border border-border bg-background p-2 text-sm leading-5 outline-none focus-visible:border-brand/60 focus-visible:ring-2 focus-visible:ring-ring/55"
        />
        {shape === "mapReduce" ? (
          <textarea
            value={items}
            onChange={(event) => setItems(event.target.value)}
            placeholder="要逐个处理的那批东西，一行一个"
            className="mt-2 min-h-12 w-full resize-y rounded-lg border border-border bg-background p-2 text-xs leading-5 outline-none focus-visible:border-brand/60 focus-visible:ring-2 focus-visible:ring-ring/55"
          />
        ) : null}
        <div className="mt-2 flex flex-wrap items-center gap-2">
          {SHAPES.map((option) => (
            <Button
              key={option}
              size="sm"
              variant={shape === option ? "brand" : "subtle"}
              onClick={() => setShape(option)}
            >
              {SHAPE_LABELS[option]}
            </Button>
          ))}
        </div>
        <div className="mt-2 flex flex-wrap items-center gap-3 text-xs text-muted-foreground">
          <label className="flex items-center gap-1">
            {/* 同一个格子在辩论形状里说的是轮数：标签跟着形状走，不然"分支=3"在辩论里没人知道是三轮 */}
            {shape === "debate" ? "轮数" : "分支"}
            <input
              type="number"
              min={shape === "debate" ? 1 : 3}
              max={shape === "debate" ? 4 : 8}
              value={branches}
              onChange={(event) => {
                const raw = Number(event.target.value) || (shape === "debate" ? 1 : 3);
                // 范围在这儿收一次：旁边那句"等于几次请求"要报的就是实际会跑的数
                setBranches(shape === "debate" ? Math.min(Math.max(raw, 1), 4) : Math.min(Math.max(raw, 3), 8));
              }}
              className="w-14 rounded border border-border bg-background px-1 py-0.5 text-xs"
            />
          </label>
          {brief ? (
            <span title="按装配时那张图算的：「一次跑成」是每格都一发过，「用满重试」是每格都把尝试次数用完。跑起来之后改边、人工重跑、层级补的第二批都不在这两个数里">
              {shape === "debate" ? `${branches} 轮交替：` : "这张图"} {brief.nodes} 格 ·{" "}
              {brief.minRequests}~{brief.maxRequests} 发请求
            </span>
          ) : null}
          <label className="flex items-center gap-1">
            并发上限
            <input
              type="number"
              min={1}
              max={8}
              value={maxParallel}
              onChange={(event) => setMaxParallel(Number(event.target.value) || 1)}
              className="w-14 rounded border border-border bg-background px-1 py-0.5 text-xs"
            />
          </label>
          {/* 每一格自己的花费上限。它管的是"不许有一格把整份计划的钱吃光"，
              与并发上限一样，是用户显式说过的话才生效（0 = 不设） */}
          <label className="flex items-center gap-1">
            每格上限$
            <input
              type="number"
              min={0}
              step={0.05}
              value={nodeCapUsd}
              title="单节点花费上限（美元），0 = 不设。整份计划的预算另说"
              onChange={(event) => setNodeCapUsd(Math.max(0, Number(event.target.value) || 0))}
              className="w-16 rounded border border-border bg-background px-1 py-0.5 text-xs"
            />
          </label>
          {/* 形状检查：这三格以前在真实链路上永远是空的（`run_node` 写死"非空即可"），
              所以"结论里不许贴 token"那种要求根本没处说 */}
          <label className="flex items-center gap-1">
            每格至少
            <input
              type="number"
              min={0}
              value={minChars}
              title="产出少于此字数即判不合格；0 = 不要求字数"
              onChange={(event) => setMinChars(Math.max(0, Number(event.target.value) || 0))}
              className="w-14 rounded border border-border bg-background px-1 py-0.5 text-xs"
            />
            字
          </label>
          <input
            type="text"
            value={mustContain}
            placeholder="须含，逗号分隔"
              aria-label="必含字串"
            title="每一格结论里必须出现的字串（逗号或换行分隔）。留空 = 不要求"
            onChange={(event) => setMustContain(event.target.value)}
            className="w-28 rounded border border-border bg-background px-1 py-0.5 text-xs"
          />
          <input
            type="text"
            value={forbid}
            placeholder="禁含，逗号分隔"
              aria-label="禁含字串"
            title="结论里出现即判不合格（比如令牌的固定前缀）。报错只回显前三位，不把秘密抄回上下文"
            onChange={(event) => setForbid(event.target.value)}
            className="w-28 rounded border border-border bg-background px-1 py-0.5 text-xs"
          />
          <div className="flex items-center gap-1">
            优先级
            {PRIORITIES.map((option) => (
              <Button
                key={option}
                size="sm"
                variant={priority === option ? "brand" : "subtle"}
                title={PRIORITY_HINTS[option]}
                onClick={() => setPriority(option)}
              >
                {PRIORITY_LABELS[option]}
              </Button>
            ))}
          </div>
          <Button
            size="sm"
            variant="brand"
            disabled={busy || !goal.trim() || running}
            onClick={() =>
              void act(async () =>
                setPlanId(
                  await orchestraStart({
                    goal,
                    shape,
                    branches,
                    maxParallel,
                    priority,
                    // 界面收美元、账与闸都按微元走（1e-6 美元）。换算只在这一处
                    nodeCostMicros: Math.round(nodeCapUsd * 1_000_000),
                    check: { minChars, mustContain: list_of(mustContain), forbid: list_of(forbid) },
                    items: items_of(items),
                  }),
                )
              )
            }
          >
            开跑
          </Button>
          {view ? (
            <>
              <Button
                size="sm"
                variant="subtle"
                disabled={busy || !running}
                onClick={() => void act(() => (view.paused ? orchestraResume(view.planId) : orchestraPause(view.planId)))}
              >
                {view.paused ? "继续" : "暂停"}
              </Button>
              <Button
                size="sm"
                variant="subtle"
                disabled={busy || !running}
                onClick={() => void act(() => orchestraCancel(view.planId))}
              >
                取消
              </Button>
            </>
          ) : null}
        </div>
        {error ? <p className="mt-2 text-xs leading-5 text-destructive">{error}</p> : null}
      </div>

      {view && view.waiting.length > 0 ? (
        <div className="rounded-lg border border-primary/40 bg-surface p-3">
          <p className="text-xs leading-5">
            {view.waiting.length} 个节点在等你批准操作。后台运行不会替你点头。
            {/* 说清去哪儿点头：待批队列住在「任务」页那一块，这一格只能告诉你有人在等 */}
            <span className="text-muted-foreground"> 在「任务」页的待批队列里逐条点头；</span>
            <span className="text-muted-foreground">
              批过之后它记的是"以后同一份参数不再问"，这一发要接着跑请重跑那个节点。
            </span>
          </p>
          <ul className="mt-1 space-y-0.5 text-xs text-muted-foreground">
            {view.waiting.map(([node, why]) => (
              <li key={node}>
                {node} · {why}
              </li>
            ))}
          </ul>
        </div>
      ) : null}

      {view ? (
        <div className="rounded-lg border border-border bg-surface p-3">
          <p className="text-xs text-muted-foreground">
            在跑 {view.inFlight} / 上限 {view.maxParallel} ·{" "}
            {/* 全局那一格是"这台机器上几路"。账本里出现"等全局并发位"时要有个地方能看出是谁占着；
                0 是设置里写出来的"不设上限"，不是空池子——写成一串不可能的数字只会让人当坏掉了 */}
            全局 {view.quotaUsed}
            {view.quotaTotal === 0 ? " / 不限" : ` / ${view.quotaTotal}`}
            {view.quotaTotal === 0
              ? ""
              : `（${PRIORITY_LABELS[view.priority]}最多 ${view.quotaShare} 格）`}
            ·{" "}
            已用 {view.spentTokens} tokens ·{" "}
            {Math.round(view.spentDurationMs / 1000)}s ·{" "}
            {/* 钱这一格以前没有：预算里那条「花费」上限于是永远顶不住，
                而面板上只报 token——一个不换算成钱的用量数字，说不清这一份计划值不值 */}
            {usdOf(view.spentCostE8)}
            {/* 池子被失败砍过才报这一格：贴着天花板时它只是个重复的数 */}
            {view.workerCap < view.workerCeiling
              ? ` · 池子缩到 ${view.workerCap}/${view.workerCeiling}`
              : ""}
            {view.blockedBy ? ` · 预算里的「${view.blockedBy}」用完了` : ""}
            {view.conflicts ? ` · 冲突双留 ${view.conflicts} 次` : ""}
          </p>
          {/* 图与列表读的都是派生值：图答"谁等谁"，列表答"这一步怎么样了"。
              改边用的是图下面那一格——它和图读的是同一份 view.nodes，不另存一份形状 */}
          <DagShape nodes={view.nodes} critical={view.criticalPath} />
          <EdgeEditor
            planId={view.planId}
            nodes={view.nodes}
            paused={view.paused}
            busy={busy}
            onAct={act}
          />
          <ul className="mt-2 space-y-2">
            {[...view.nodes]
              .sort((a, b) => a.depth - b.depth || a.id.localeCompare(b.id))
              .map((node) => {
                const critical = view.criticalPath.includes(node.id);
                const session = node.conversationId;
                return (
                  <li key={node.id} className="border-l-2 border-border pl-2">
                    <div className="flex items-center gap-2">
                      <span className="truncate font-mono text-xs">{node.id}</span>
                      {/* 规划器追加的那一步要认得出来：它是失败之后多花的一笔钱 */}
                      {node.id.endsWith("#retry") ? (
                        <span className="shrink-0 text-xs text-brand-text">重做</span>
                      ) : null}
                      <span className="text-xs text-muted-foreground">{node.profile}</span>
                      <span className="ml-auto text-xs">{STATUS_LABELS[node.status]}</span>
                      {/* 将就收下的那一份不能与干净的那一份都叫"完成"：理由在 title 里，
                          也在 Trace 那一屏的那行 detail 里（同一个来源，不另抄一份） */}
                      {node.degraded ? (
                        <span className="shrink-0 text-xs text-brand-text" title={node.degraded}>
                          降级
                        </span>
                      ) : null}
                      <Button
                        size="sm"
                        variant="subtle"
                        disabled={busy}
                        onClick={() => void act(() => orchestraRerunNode(view.planId, node.id))}
                      >
                        重跑
                      </Button>
                    </div>
                    <div className="mt-1 flex items-center gap-1">
                      {/* 紧凑横条：深度是关键路径的位置，不是进度百分比 */}
                      {Array.from({ length: node.depth + 1 }).map((_, index) => (
                        <span
                          key={index}
                          className={
                            critical
                              ? "h-1 w-6 rounded-sm bg-primary"
                              : "h-1 w-6 rounded-sm bg-border"
                          }
                        />
                      ))}
                      {node.boardVersion > 0 ? (
                        <span className="text-2xs text-muted-foreground">
                          黑板 v{node.boardVersion}
                        </span>
                      ) : null}
                      {/* 验收第 5 条的那三格：输出在上面，成本与耗时在这里。
                          "没价表"必须说得出——把算不出钱写成 $0 是替服务商撒了个谎 */}
                      {node.tokens > 0 ? (
                        <span className="text-2xs text-muted-foreground">
                          {node.tokens} tokens · {Math.round(node.durationMs / 1000)}s ·{" "}
                          {node.priced ? usdOf(node.costE8) : "没价表"}
                        </span>
                      ) : null}
                      {/* 这一步动了什么文件、有没有快照。"回滚"这个词只能说到这儿：
                          真要回是变更请求页那一格，它按文件现在的字节判 */}
                      {node.filesTouched > 0 ? (
                        <span className="text-2xs text-muted-foreground">
                          动过 {node.filesTouched} 个文件 ·{" "}
                          {node.snapshotted ? "有快照可回" : "没留快照"}
                        </span>
                      ) : null}
                      {/* 上面那句的出口。回滚住在变更请求页那一侧而它只认话题 id，
                          所以这一发自己的话题得点得到，不能只写在句子里 */}
                      {session && node.filesTouched > 0 ? (
                        <button
                          type="button"
                          className="text-2xs text-muted-foreground underline decoration-dotted underline-offset-2 hover:text-foreground"
                          onClick={() => void useChatStore.getState().openConversation(session)}
                        >
                          {node.snapshotted ? "去回滚" : "看改动"}
                        </button>
                      ) : null}
                    </div>
                    {node.gate ? (
                      <p className="mt-1 text-2xs text-muted-foreground">
                        放行方式 · <span className="text-foreground">{node.gate}</span>
                      </p>
                    ) : null}
                    {live[node.id] ? (
                      <p className="mt-1 line-clamp-3 whitespace-pre-wrap text-xs leading-4 text-muted-foreground">
                        {live[node.id]}
                      </p>
                    ) : null}
                  </li>
                );
              })}
          </ul>
        </div>
      ) : null}

      {board.length > 0 ? (
        <div className="rounded-lg border border-border bg-surface p-3">
          <p className="text-xs text-muted-foreground">黑板（各节点的结论）</p>
          <ul className="mt-1 space-y-1">
            {board.map((line) => (
              <li key={line} className="truncate font-mono text-xs leading-5">
                {line}
              </li>
            ))}
          </ul>
        </div>
      ) : null}

      {trace.length > 0 ? (
        <div className="rounded-lg border border-border bg-surface p-3">
          <p className="text-xs text-muted-foreground">
            Trace（{trace.length} 条 · 最新在上）
          </p>
          <ul className="mt-1 max-h-44 space-y-1 overflow-y-auto">
            {[...trace]
              .reverse()
              .map((row, index) => (
                <li key={`${row.tsMs}-${row.node}-${row.event}-${index}`} className="text-xs leading-5">
                  <div className="flex items-baseline gap-2">
                    <span className="shrink-0 font-mono text-muted-foreground">
                      {new Date(row.tsMs).toLocaleTimeString()}
                    </span>
                    <span className="shrink-0">{event_label(row.event)}</span>
                    <span className="truncate font-mono">{row.node}</span>
                    {row.tokens ? (
                      <span className="ml-auto shrink-0 text-muted-foreground">
                        {row.tokens} tokens
                      </span>
                    ) : null}
                  </div>
                  {row.detail ? (
                    <p title={row.detail} className="truncate pl-[62px] text-muted-foreground">
                      {row.detail}
                    </p>
                  ) : null}
                </li>
              ))}
          </ul>
        </div>
      ) : null}

      {view?.merged ? (
        <div className="rounded-lg border border-border bg-surface p-3">
          <p className="text-xs text-muted-foreground">汇合结论</p>
          <p className="mt-1 whitespace-pre-wrap text-sm leading-6">{view.merged}</p>
        </div>
      ) : null}
    </div>
  );
}
