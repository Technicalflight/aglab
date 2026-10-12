import {
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  useSyncExternalStore,
  type ReactNode,
} from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  IconCompass as Compass,
  IconDots as Dots,
  IconFolderOpen as FolderOpen,
  IconPlus as Plus,
  IconTerminal2 as SquareTerminal,
  IconWorld as Globe,
  IconX as X,
} from "@tabler/icons-react";

import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { DecisionPanel } from "@/components/decision-panel";
import { PreviewPanel } from "@/components/preview-panel";
import { decisionMonitor, summarizeTraces, tracesForConversation } from "@/lib/decision";
import { useChatStore } from "@/store/chat-store";
import { useDecisionStore } from "@/store/decision-store";
import { type PanelTab } from "@/types/chat";
import { cn } from "@/lib/utils";

/**
 * useSyncExternalStore 要求 subscribe 是稳定引用，而且它会把函数单独取出去调用——
 * 直接传 decisionMonitor.subscribe 会把 this 丢掉（内部要用 this.listeners）。
 */
const subscribeTraces = (listener: () => void) => decisionMonitor.subscribe(listener);
const getTraces = () => decisionMonitor.snapshot;

function formatMs(value: number): string {
  if (value >= 1000) return `${(value / 1000).toFixed(value >= 10_000 ? 1 : 2)}s`;
  return `${Math.round(value)}ms`;
}

function clockOf(iso: string): string {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return "";
  const pad = (value: number) => String(value).padStart(2, "0");
  return `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`;
}

/**
 * 分区 7 上半张卡：决策层请求链路，滚动显示。
 *
 * 这一张替代了原来的「决策」页签——工具栏留给三件真工具（预览/终端/浏览器），
 * 而"这一层到底有没有在干活"是**状态**不是**工具**：做成一张常驻的卡，
 * 抬眼就看得见，不用先切过去。要展开看全量读数点「详情」。
 *
 * 它和下面的工具栏各自成卡，不共用一张卡的两段：状态与工具是两件事，
 * 靠一条分隔线切开只会读成"同一个面板被切了一刀"。
 *
 * 监控在这里 attach 而不是在详情弹窗里：这张卡是常驻的，挂在弹窗上就等于
 * "不点开详情就永远没有读数"，那条链路看起来像死的。
 */
function DecisionChainCard() {
  const system = useDecisionStore((s) => s.system);
  const health = useDecisionStore((s) => s.health);
  const activeId = useChatStore((s) => s.activeId);
  const traces = useSyncExternalStore(subscribeTraces, getTraces, getTraces);
  const [detailOpen, setDetailOpen] = useState(false);
  const stripRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => decisionMonitor.attach(system), [system]);

  const conversationTraces = useMemo(
    () => tracesForConversation(traces, activeId),
    [traces, activeId],
  );
  const summary = useMemo(() => summarizeTraces(conversationTraces), [conversationTraces]);
  // 只留最近 12 条：这一行是"刚才发生了什么"，不是台账
  const recent = useMemo(() => conversationTraces.slice(-12), [conversationTraces]);

  // 新判定进来就滑到最右。停在开头等于"滚动显示"没显示
  useEffect(() => {
    const element = stripRef.current;
    if (element) element.scrollLeft = element.scrollWidth;
  }, [recent.length]);

  const laya = !system.config.laya.enabled
    ? { text: "已关闭", tone: "plain" as const }
    : health === null
      ? { text: "未探测", tone: "plain" as const }
      : health.ok
        ? health.loaded
          ? { text: "已加载", tone: "good" as const }
          : { text: "在听", tone: "warn" as const }
        : { text: "没在听", tone: "bad" as const };

  return (
    // 画布级头部条（设计基准）：决策读数不再是带边框的卡片，而是画布上的
    // 一行头部——左蓝点右「详情」，下方工具卡才是内容。
    // 分区 7 上半段：决策层请求链路。它与下面的工具栏是两件不同的东西——
    // 一个回答"这一层有没有在干活"，一个装着预览/终端/浏览器三件工具
    <section className="shrink-0 px-1">
      <div className="flex h-9 items-center gap-2">
        <span
          aria-hidden="true"
          className={cn(
            "size-1.5 shrink-0 rounded-full",
            laya.tone === "good" && "bg-success",
            laya.tone === "warn" && "bg-warning",
            laya.tone === "bad" && "bg-destructive",
            laya.tone === "plain" && "bg-border",
          )}
        />
        <h2 className="shrink-0 text-sm font-semibold text-foreground">决策层请求链路</h2>
        <p
          className="min-w-0 flex-1 truncate text-xs text-foreground-secondary"
          title={
            summary.decisions > 0
              ? `判定 ${summary.decisions} 次 · 命中 ${Math.round(summary.cacheHitRate * 100)}% · ${
                  summary.answered > 0
                    ? `p50 ${formatMs(summary.p50LatencyMs)}`
                    : `失败 ${summary.failed}`
                }`
              : undefined
          }
        >
          {summary.decisions > 0
            ? `本场通过 ${summary.decisions} 次判定`
            : `Laya ${laya.text} · 本场对话还没有判定`}
        </p>
        <button
          type="button"
          onClick={() => setDetailOpen(true)}
          className="shrink-0 rounded-md px-1.5 py-0.5 text-xs text-foreground-secondary outline-none transition-colors hover:bg-accent hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45"
        >
          详情
        </button>
      </div>

      {/* 近期判定：有过判定才出现（空态的话头部那句已经说清了），
          挤在头部行下方不占卡片，滚动显示 */}
      {recent.length > 0 ? (
        <div
          ref={stripRef}
          className="details-scroll flex items-center gap-1.5 overflow-x-auto px-1 pb-2"
        >
          {recent.map((trace) => (
            <span
              key={trace.id}
              title={`${trace.modelChain.join(" → ") || "缓存"} · ${trace.totalLatencyMs}ms`}
              className={cn(
                "flex shrink-0 items-center gap-1 rounded-md border px-1.5 py-0.5 text-2xs tabular-nums",
                trace.response === null
                  ? "border-destructive/40 text-destructive"
                  : trace.response.degraded
                    ? "border-warning/40 text-warning"
                    : "border-border text-foreground-tertiary",
              )}
            >
              <span className="text-foreground-quaternary">{clockOf(trace.timestamp)}</span>
              <span className="text-foreground-secondary">
                {trace.modelChain.length > 0 ? trace.modelChain.join("→") : "缓存"}
              </span>
              <span>{formatMs(trace.totalLatencyMs)}</span>
              {trace.cacheHit ? <span className="text-foreground-quaternary">命中</span> : null}
            </span>
          ))}
        </div>
      ) : null}

      <Dialog open={detailOpen} onOpenChange={setDetailOpen}>
        <DialogContent className="w-[min(760px,92vw)]">
          <DialogTitle>决策层请求链路</DialogTitle>
          <div className="max-h-[64vh] overflow-y-auto">
            <DecisionPanel />
          </div>
        </DialogContent>
      </Dialog>
    </section>
  );
}

/**
 * 右侧那一列（分区 7）。className 由 App 传入：窄屏时整列改成 fixed 浮层盖在内容上，
 * 宽屏时保持 flex 常驻。两种形态都从这里出，App 只管传类名。
 *
 * 列里是一条画布级头部（决策层请求链路）加一张工具卡（预览/终端/浏览器）：
 * 状态读数不占卡片，工具卡独享剩余高度。宽度分档：xl 之上 384px（四列并排时
 * 最舒服的一档），之下收到 320px——窗口下限 1044 那一档里，主内容才是主角。
 */
export function RightPanel({ className }: { className?: string }) {
  return (
    <div className={cn("flex w-80 min-h-0 shrink-0 flex-col gap-1.5 sm:gap-2 xl:w-96", className)}>
      <DecisionChainCard />
      <ToolPanel />
    </div>
  );
}

/** 右栏三件工具的启动卡（参照设计基准的「开始」页形制）：图标 + 名称 + 一句说明 */
const TOOL_CARDS: Array<{ value: PanelTab; label: string; desc: string; icon: ReactNode }> = [
  {
    value: "preview",
    label: "预览",
    desc: "浏览会话里改动过的文件",
    icon: <FolderOpen className="size-5 text-amber-500" />,
  },
  {
    value: "terminal",
    label: "终端",
    desc: "在会话工作区运行命令",
    icon: <SquareTerminal className="size-5 text-sky-500" />,
  },
  {
    value: "browser",
    label: "浏览器",
    desc: "浏览网页",
    icon: <Globe className="size-5 text-sky-500" />,
  },
];

/** 工具栏卡：多标签形态——开始页（启动卡居中、占满整页）⇄ 标签条（每件打开的
 *  工具一枚标签，同类可重复打开，× 单独关闭，+ 挑新工具；内容区 keep-alive）。
 *  min-h-0 让它在矮窗口里能收缩 */
/** 一枚打开的工具标签：id 是实例身份（同一件工具可以开多枚），tool 决定渲染哪个内容 */
interface ToolTabInstance {
  id: string;
  tool: PanelTab;
}

interface PanelTabsState {
  open: ToolTabInstance[];
  activeId: string | null;
  picking: boolean;
}

let tabInstanceSeq = 0;
function nextTabInstanceId() {
  tabInstanceSeq += 1;
  return `tool-tab-${tabInstanceSeq}`;
}

/** 标签数量上限：超出后手动开新标签会被拒（外部导航不受限，保证可达） */
const MAX_OPEN_TABS = 10;

function ToolPanel() {
  const panelTab = useChatStore((s) => s.panelTab);
  const edits = useChatStore((s) => s.edits);
  const setPanelTab = useChatStore((s) => s.setPanelTab);
  const pushToast = useChatStore((s) => s.pushToast);

  // 多标签状态：open 是打开顺序（同一件工具可开多枚），activeId 是当前聚焦，
  // picking = 正在挑新工具。store 的 panelTab 仍是外部导航（资源管理器点文件等）
  // 的入口：它一变就聚焦同类已有标签、没有才开新。挂载默认停在开始页；但外部
  // 已经点名（如先点了文件再展开右栏）就直接开预览
  const [tabs, setTabs] = useState<PanelTabsState>(() => {
    if (!useChatStore.getState().previewTarget) {
      return { open: [], activeId: null, picking: false };
    }
    const id = nextTabInstanceId();
    return { open: [{ id, tool: "preview" }], activeId: id, picking: false };
  });
  const lastPanelTab = useRef(panelTab);
  // 标签省略：容器装不下时显示「…」下拉（列出全部标签，可切换/关闭）
  const stripRef = useRef<HTMLElement | null>(null);
  const [overflowOpen, setOverflowOpen] = useState(false);
  const [overflowing, setOverflowing] = useState(false);

  // 每次渲染后量一次：开关标签、窗口缩放都会改变可容纳的枚数
  useLayoutEffect(() => {
    const element = stripRef.current;
    if (!element) return;
    setOverflowing(element.scrollWidth > element.clientWidth + 1);
  });

  // 外部导航同步：panelTab 变了（资源管理器、决策面板等）就聚焦同类已有标签、
  // 没有才开新。自己点的标签也会走 setPanelTab——幂等，无额外副作用
  useEffect(() => {
    if (lastPanelTab.current === panelTab) return;
    lastPanelTab.current = panelTab;
    setTabs((prev) => {
      const existing = prev.open.find((instance) => instance.tool === panelTab);
      if (existing) return { ...prev, activeId: existing.id, picking: false };
      const id = nextTabInstanceId();
      return { open: [...prev.open, { id, tool: panelTab }], activeId: id, picking: false };
    });
  }, [panelTab]);

  // 每次都开一枚新实例：同类重复是被明确要求的行为（多枚终端/多枚浏览器并存）。
  // 手动开新标签受上限约束：满员拒绝并提示，先关掉几个再试——
  // 外部导航（资源管理器点文件等）不受限，保证「点了就得到」
  const openTool = (tool: PanelTab) => {
    if (tabs.open.length >= MAX_OPEN_TABS) {
      pushToast({
        tone: "info",
        title: "标签已达上限",
        detail: `最多同时打开 ${MAX_OPEN_TABS} 个标签，先关掉几个再试。`,
      });
      return;
    }
    const id = nextTabInstanceId();
    setTabs((prev) => ({ open: [...prev.open, { id, tool }], activeId: id, picking: false }));
    setPanelTab(tool);
  };

  const closeTab = (instanceId: string) => {
    const open = tabs.open.filter((instance) => instance.id !== instanceId);
    const wasActive = tabs.activeId === instanceId;
    const at = tabs.open.findIndex((instance) => instance.id === instanceId);
    const fallback = open[Math.min(at, open.length - 1)] ?? null;
    setTabs({
      open,
      activeId: open.length === 0 ? null : wasActive ? fallback.id : tabs.activeId,
      picking: false,
    });
    if (wasActive && fallback) setPanelTab(fallback.tool);
  };

  const activate = (instanceId: string) => {
    setTabs((prev) => ({ ...prev, activeId: instanceId, picking: false }));
    const instance = tabs.open.find((item) => item.id === instanceId);
    if (instance) setPanelTab(instance.tool);
    // 从「…」菜单激活的标签可能滚出可视区：把它滚回来
    requestAnimationFrame(() => {
      stripRef.current
        ?.querySelector(`[data-tab-id="${instanceId}"]`)
        ?.scrollIntoView({ behavior: "smooth", inline: "nearest", block: "nearest" });
    });
  };

  // min-h-0 补上：作为 flex 列容器，缺它里面的内容区无法收缩，矮窗口下会顶破父级。
  // overflow-hidden 与圆角配套：卡片外的圆角必须真的裁掉内容
  return (
    <section className="flex min-h-0 flex-1 flex-col overflow-hidden rounded-lg border border-border bg-surface shadow-md">
      {tabs.open.length === 0 ? (
        <div className="flex min-h-0 flex-1 flex-col">
          <p className="flex items-center gap-1.5 px-3 pt-3 text-sm font-medium text-foreground">
            <Compass className="size-4 text-muted-foreground" />
            开始
          </p>
          <div className="flex min-h-0 flex-1 flex-col items-center justify-center px-6 pb-8">
            <div
              aria-hidden
              className="flex size-16 items-center justify-center rounded-full bg-muted"
            >
              <Compass className="size-8 text-muted-foreground/40" strokeWidth={1.5} />
            </div>
            <div className="mt-8 w-full max-w-xs space-y-2.5">
              {TOOL_CARDS.map((tool) => (
                <button
                  key={tool.value}
                  type="button"
                  onClick={() => openTool(tool.value)}
                  className="flex w-full items-center gap-3 rounded-xl border border-border bg-surface px-3 py-2.5 text-left outline-none transition-colors hover:border-brand/40 hover:bg-accent focus-visible:ring-2 focus-visible:ring-ring/45"
                >
                  {tool.icon}
                  <span className="min-w-0 flex-1">
                    <span className="flex items-center gap-1.5">
                      <span className="truncate text-sm font-medium text-foreground">
                        {tool.label}
                      </span>
                      {tool.value === "preview" && edits.length > 0 ? (
                        <span className="rounded-md bg-muted px-1.5 text-2xs leading-4 tabular-nums text-muted-foreground">
                          {edits.length}
                        </span>
                      ) : null}
                    </span>
                    <span className="mt-0.5 block truncate text-xs leading-4 text-muted-foreground">
                      {tool.desc}
                    </span>
                  </span>
                </button>
              ))}
            </div>
          </div>
        </div>
      ) : (
        <>
          {/* 标签条：每件打开的工具一枚标签（× 单独关闭）；+ 挑新工具。
              当前标签凸起并与下方内容连成一体（-mb-px + 无下边框） */}
          <nav
            ref={stripRef}
            aria-label="打开的工具"
            className="flex shrink-0 items-end gap-1 overflow-x-auto bg-sunken px-2 pt-1.5"
          >
            {tabs.open.map((instance) => {
              const tool = TOOL_CARDS.find((item) => item.value === instance.tool) ?? TOOL_CARDS[0];
              // 同一件工具开了多枚时标题带序号（终端 ·2），否则分不清哪枚是哪枚
              const sameTool = tabs.open.filter((item) => item.tool === instance.tool);
              const numbered = sameTool.length > 1;
              const ordinal = sameTool.findIndex((item) => item.id === instance.id) + 1;
              const active = instance.id === tabs.activeId && !tabs.picking;
              return (
                <span
                  key={instance.id}
                  data-tab-id={instance.id}
                  className={cn(
                    "flex min-w-0 shrink items-center gap-1.5 rounded-t-lg border border-b-0 px-2.5 py-1.5 text-xs font-medium",
                    active
                      ? "-mb-px border-border bg-surface text-foreground"
                      : "border-transparent text-muted-foreground",
                  )}
                >
                  <button
                    type="button"
                    onClick={() => activate(instance.id)}
                    title={tool.desc}
                    className="flex min-w-0 items-center gap-1.5 rounded outline-none focus-visible:ring-2 focus-visible:ring-ring/45"
                  >
                    <span className="shrink-0">{tool.icon}</span>
                    <span className="truncate">
                      {numbered ? `${tool.label} ·${ordinal}` : tool.label}
                    </span>
                  </button>
                  {instance.tool === "preview" && edits.length > 0 ? (
                    <span className="rounded-md bg-muted px-1 text-2xs leading-4 tabular-nums text-muted-foreground">
                      {edits.length}
                    </span>
                  ) : null}
                  <button
                    type="button"
                    aria-label={`关闭${tool.label}标签`}
                    onClick={() => closeTab(instance.id)}
                    className="flex size-4 items-center justify-center rounded text-muted-foreground outline-none transition-colors hover:bg-accent hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45"
                  >
                    <X className="size-3" />
                  </button>
                </span>
              );
            })}
            <button
              type="button"
              aria-label="挑一件工具打开"
              aria-pressed={tabs.picking}
              onClick={() => setTabs((prev) => ({ ...prev, picking: !prev.picking }))}
              className={cn(
                "mb-1 flex size-6 items-center justify-center rounded-md outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
                tabs.picking
                  ? "bg-accent text-foreground"
                  : "text-muted-foreground hover:bg-accent hover:text-foreground",
              )}
            >
              <Plus className="size-3.5" />
            </button>
            {overflowing ? (
              <Popover open={overflowOpen} onOpenChange={setOverflowOpen}>
                <PopoverTrigger asChild>
                  <button
                    type="button"
                    aria-label="更多标签"
                    aria-pressed={overflowOpen}
                    className={cn(
                      "mb-1 flex size-6 shrink-0 items-center justify-center rounded-md outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
                      overflowOpen
                        ? "bg-accent text-foreground"
                        : "text-muted-foreground hover:bg-accent hover:text-foreground",
                    )}
                  >
                    <Dots className="size-3.5" />
                  </button>
                </PopoverTrigger>
                <PopoverContent align="end" side="bottom" className="w-60 p-1">
                  <p className="px-2 pb-1 pt-1.5 text-2xs leading-4 text-muted-foreground">
                    打开的标签（点击切换）
                  </p>
                  <ul>
                    {tabs.open.map((instance) => {
                      const tool =
                        TOOL_CARDS.find((item) => item.value === instance.tool) ?? TOOL_CARDS[0];
                      const sameTool = tabs.open.filter((item) => item.tool === instance.tool);
                      const numbered = sameTool.length > 1;
                      const ordinal = sameTool.findIndex((item) => item.id === instance.id) + 1;
                      const active = instance.id === tabs.activeId;
                      return (
                        <li key={instance.id} className="flex items-center gap-1">
                          <button
                            type="button"
                            onClick={() => {
                              activate(instance.id);
                              setOverflowOpen(false);
                            }}
                            className={cn(
                              "flex min-w-0 flex-1 items-center gap-2 rounded-lg px-2 py-1.5 text-left outline-none transition-colors hover:bg-accent focus-visible:ring-2 focus-visible:ring-ring/45",
                              active ? "text-foreground" : "text-muted-foreground",
                            )}
                          >
                            {tool.icon}
                            <span className="min-w-0 flex-1 truncate text-xs">
                              {numbered ? `${tool.label} ·${ordinal}` : tool.label}
                            </span>
                            {active ? (
                              <span
                                aria-hidden
                                className="size-1.5 shrink-0 rounded-full bg-brand"
                              />
                            ) : null}
                          </button>
                          <button
                            type="button"
                            aria-label={`关闭${tool.label}标签`}
                            onClick={() => closeTab(instance.id)}
                            className="flex size-6 shrink-0 items-center justify-center rounded-md text-muted-foreground outline-none transition-colors hover:bg-accent hover:text-destructive focus-visible:ring-2 focus-visible:ring-ring/45"
                          >
                            <X className="size-3" />
                          </button>
                        </li>
                      );
                    })}
                  </ul>
                </PopoverContent>
              </Popover>
            ) : null}
          </nav>

          <div className="min-h-0 flex-1">
            {tabs.picking ? (
              <div className="flex h-full flex-col items-center justify-center px-6 pb-8">
                <div className="w-full max-w-xs space-y-2.5">
                  {TOOL_CARDS.map((tool) => {
                    const instances = tabs.open.filter((instance) => instance.tool === tool.value);
                    const count = instances.length;
                    // 满员时：未打开的工具禁用（开不了新标签）；已打开的改为「切换到它」
                    const blocked = tabs.open.length >= MAX_OPEN_TABS && count === 0;
                    const switchTo = count > 0 ? instances[0] : undefined;
                    return (
                      <button
                        key={tool.value}
                        type="button"
                        disabled={blocked}
                        onClick={() => {
                          if (switchTo) {
                            activate(switchTo.id);
                            return;
                          }
                          openTool(tool.value);
                        }}
                        title={
                          blocked
                            ? `标签已达上限（${MAX_OPEN_TABS} 个），先关掉几个`
                            : count > 0
                              ? "点击切换到已有标签，或再开一枚新标签"
                              : "每次都会开一枚新标签"
                        }
                        className={cn(
                          "flex w-full items-center gap-3 rounded-xl border px-3 py-2.5 text-left outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
                          blocked
                            ? "cursor-not-allowed border-border bg-surface opacity-60"
                            : count > 0
                              ? "border-border bg-background hover:border-brand/40 hover:bg-accent"
                              : "border-border bg-surface hover:border-brand/40 hover:bg-accent",
                        )}
                      >
                        {tool.icon}
                        <span className="min-w-0 flex-1">
                          <span className="block truncate text-sm font-medium text-foreground">
                            {tool.label}
                          </span>
                          <span className="mt-0.5 block truncate text-xs leading-4 text-muted-foreground">
                            {tool.desc}
                          </span>
                        </span>
                        {count > 0 ? (
                          <span className="shrink-0 text-2xs text-muted-foreground">
                            已打开 {count} 个
                          </span>
                        ) : blocked ? (
                          <span className="shrink-0 text-2xs text-muted-foreground">已满</span>
                        ) : null}
                      </button>
                    );
                  })}
                </div>
              </div>
            ) : null}

            {/* keep-alive：所有打开的实例保持挂载（非当前 display:none 隐藏），
                切换不丢终端历史、浏览器地址这类组件内状态 */}
            {tabs.open.map((instance) => {
              const visible = instance.id === tabs.activeId && !tabs.picking;
              return (
                <div key={instance.id} className={cn("h-full", visible ? "block" : "hidden")}>
                  {instance.tool === "preview" ? (
                    <div className="details-scroll flex h-full flex-col overflow-hidden px-4 py-3">
                      <PreviewPanel />
                    </div>
                  ) : instance.tool === "terminal" ? (
                    <div className="details-scroll flex h-full flex-col overflow-y-auto px-4 py-3">
                      <TerminalTab />
                    </div>
                  ) : (
                    <div className="details-scroll flex h-full flex-col overflow-y-auto px-4 py-3">
                      <BrowserTab />
                    </div>
                  )}
                </div>
              );
            })}
          </div>
        </>
      )}
    </section>
  );
}

/** 终端标签：逐条执行命令并保留输出历史。不是交互式 PTY——跑命令看结果够用 */
function TerminalTab() {
  const config = useChatStore((s) => s.config);
  const [command, setCommand] = useState("");
  const [running, setRunning] = useState(false);
  const [entries, setEntries] = useState<Array<{ command: string; output: string }>>([]);
  const outputRef = useRef<HTMLDivElement | null>(null);

  const project = config.projects.find((item) => item.id === config.activeProjectId);
  const cwd = project?.path ?? "";

  useEffect(() => {
    outputRef.current?.scrollTo({ top: outputRef.current.scrollHeight });
  }, [entries]);

  async function run() {
    const text = command.trim();
    if (!text || running) return;
    setRunning(true);
    setCommand("");
    setEntries((previous) => [...previous, { command: text, output: "" }]);
    try {
      const result = await invoke<{ output: string }>("terminal_exec", {
        command: text,
        cwd: cwd || null,
      });
      setEntries((previous) =>
        previous.map((entry, index) =>
          index === previous.length - 1 ? { ...entry, output: result.output } : entry,
        ),
      );
    } catch (error) {
      setEntries((previous) =>
        previous.map((entry, index) =>
          index === previous.length - 1
            ? {
                ...entry,
                output: error instanceof Error ? error.message : String(error),
              }
            : entry,
        ),
      );
    } finally {
      setRunning(false);
    }
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <p className="text-xs leading-5 text-muted-foreground">
        命令在<span className="text-foreground">{cwd || "未绑定的工作目录"}</span>下执行， 60
        秒超时，输出超长会截断。
      </p>

      <div
        ref={outputRef}
        className="mt-2 min-h-0 flex-1 overflow-y-auto rounded-lg border border-border bg-background p-2.5 font-mono text-xs leading-5"
      >
        {entries.length === 0 ? (
          <p className="text-muted-foreground">还没有执行过命令。</p>
        ) : (
          entries.map((entry, index) => (
            <div key={index} className="mb-2 last:mb-0">
              <p className="text-brand-text">$ {entry.command}</p>
              <p className="whitespace-pre-wrap break-all text-foreground">
                {entry.output || (index === entries.length - 1 && running ? "…" : "（无输出）")}
              </p>
            </div>
          ))
        )}
      </div>

      <div className="mt-2 flex items-center gap-2">
        <input
          type="text"
          value={command}
          placeholder="输入命令，回车执行"
          aria-label="终端命令"
          spellCheck={false}
          disabled={running}
          className="h-9 min-w-0 flex-1 rounded-lg border border-input bg-background px-3 font-mono text-sm text-foreground outline-none transition-colors focus-visible:border-brand/50 focus-visible:ring-2 focus-visible:ring-ring/35 disabled:opacity-60"
          onChange={(event) => setCommand(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter") {
              event.preventDefault();
              void run();
            }
          }}
        />
        <Button
          size="sm"
          variant="subtle"
          disabled={running || !command.trim()}
          onClick={() => void run()}
        >
          {running ? "执行中…" : "执行"}
        </Button>
      </div>
    </div>
  );
}

/** 浏览器标签：地址栏 + 内嵌网页。部分站点拒绝被内嵌时给出说明 */
function BrowserTab() {
  const [urlInput, setUrlInput] = useState("");
  const [activeUrl, setActiveUrl] = useState("");

  function normalize(raw: string): string {
    const text = raw.trim();
    if (!text) return "";
    if (/^https?:\/\//i.test(text)) return text;
    // 像域名的直接补 https，否则当搜索交给搜索引擎
    return /^[\w-]+(\.[\w-]+)+/.test(text)
      ? `https://${text}`
      : `https://www.bing.com/search?q=${encodeURIComponent(text)}`;
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex items-center gap-2">
        <input
          type="text"
          value={urlInput}
          placeholder="输入网址或搜索词"
          aria-label="网址或搜索词"
          spellCheck={false}
          className="h-9 min-w-0 flex-1 rounded-lg border border-input bg-background px-3 text-sm text-foreground outline-none transition-colors focus-visible:border-brand/50 focus-visible:ring-2 focus-visible:ring-ring/35"
          onChange={(event) => setUrlInput(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter") {
              event.preventDefault();
              setActiveUrl(normalize(urlInput));
            }
          }}
        />
        <Button
          size="sm"
          variant="subtle"
          disabled={!urlInput.trim()}
          onClick={() => setActiveUrl(normalize(urlInput))}
        >
          打开
        </Button>
      </div>

      {activeUrl ? (
        <>
          <iframe
            key={activeUrl}
            src={activeUrl}
            title="内置浏览器"
            className="mt-2 min-h-0 w-full flex-1 rounded-lg border border-border bg-web-canvas"
            sandbox="allow-scripts allow-same-origin allow-forms allow-popups"
          />
          <p className="mt-2 text-2xs leading-4 text-muted-foreground">
            部分站点（设置了 X-Frame-Options 的）拒绝被内嵌显示，会呈现空白——
            那类站点请直接在系统浏览器打开。
          </p>
        </>
      ) : (
        <div className="mt-2 flex min-h-0 flex-1 items-center justify-center rounded-lg border border-dashed border-border">
          <p className="text-sm text-muted-foreground">输入网址后回车，网页会显示在这里。</p>
        </div>
      )}
    </div>
  );
}
