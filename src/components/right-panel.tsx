import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

import { Button } from "@/components/ui/button";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { DecisionPanel } from "@/components/decision-panel";
import { PreviewPanel } from "@/components/preview-panel";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { useChatStore } from "@/store/chat-store";
import { type PanelTab } from "@/types/chat";
import { cn } from "@/lib/utils";

/** 右栏标签清单：加新标签只需要在这里登记一行，放不下的自动收进「…」。
 *  原来的「详情」「工具」「上下文」「记忆」「思考」「检查点」诸格已删——
 *  最关注的几项搬去了输入框上方，其余的去处：工具调用在消息流、记忆在设置页、
 *  思考过程在消息卡片里、检查点摘要在话题顶部 */
const PANEL_TABS: Array<{ value: PanelTab; label: string }> = [
  { value: "decision", label: "决策" },
  { value: "preview", label: "预览" },
  { value: "terminal", label: "终端" },
  { value: "browser", label: "浏览器" },
];

/** 测量估算：每个标签约 76px（中文 2-3 字 + 内边距），「…」按钮预留 40px */
const TAB_WIDTH_ESTIMATE = 76;
const MORE_WIDTH_ESTIMATE = 40;

/**
 * 右侧详情栏。className 由 App 传入：窄屏时整块改成 fixed 浮层盖在内容上，
 * 宽屏时保持 flex 常驻。两种形态都从这里出，App 只管传类名。
 */
export function RightPanel({ className }: { className?: string }) {
  const panelTab = useChatStore((s) => s.panelTab);
  const edits = useChatStore((s) => s.edits);
  const setPanelTab = useChatStore((s) => s.setPanelTab);

  // 溢出收纳：按容器实测宽度估算能放下几个标签，放不下的收进「…」。
  // 用估算而非逐个测量 DOM，tab 数量少时足够准且不会抖动
  const listRef = useRef<HTMLDivElement | null>(null);
  const [listWidth, setListWidth] = useState(0);
  const [moreOpen, setMoreOpen] = useState(false);

  useEffect(() => {
    const element = listRef.current;
    if (!element) return;
    const observer = new ResizeObserver((entries) => {
      for (const entry of entries) setListWidth(entry.contentRect.width);
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  const visibleCount = useMemo(() => {
    if (listWidth <= 0) return PANEL_TABS.length;
    const available = listWidth - 8;
    let withMore = 0;
    for (let i = 0; i < PANEL_TABS.length; i++) {
      if (
        available - (i + 1 < PANEL_TABS.length ? MORE_WIDTH_ESTIMATE : 0) <
        (i + 1) * TAB_WIDTH_ESTIMATE
      ) {
        break;
      }
      withMore = i + 1;
    }
    const plain = Math.floor(available / TAB_WIDTH_ESTIMATE);
    if (PANEL_TABS.length <= plain) return PANEL_TABS.length;
    return Math.max(withMore, 1);
  }, [listWidth]);

  const visibleTabs = PANEL_TABS.slice(0, visibleCount);
  const hiddenTabs = PANEL_TABS.slice(visibleCount);
  const hiddenActive = hiddenTabs.some((tab) => tab.value === panelTab);

  // w-96 而不是 w-80：收纳按 76px/标签估算，320px 下第四格（预览）刚好挤不进去。
  // min-h-0 补上：作为 flex 列容器，缺它里面的 Tabs 无法收缩，矮窗口下会顶破父级。
  return (
    <aside
      className={cn(
        "flex w-96 min-h-0 shrink-0 flex-col border-l border-border bg-sidebar",
        className,
      )}
    >
      <Tabs
        value={panelTab}
        onValueChange={(value) => setPanelTab(value as PanelTab)}
        className="min-h-0 flex-1"
      >
        <div className="px-3 pt-3" ref={listRef}>
          <TabsList className="flex w-full">
            {visibleTabs.map((tab) => (
              <TabsTrigger key={tab.value} value={tab.value} className="min-w-0">
                {tab.label}
                {/* 预览 tab 不点进去看不见，没数字就等于一项没人会发现的功能 */}
                {tab.value === "preview" && edits.length > 0 ? (
                  <span className="ml-1 tabular-nums text-xs text-muted-foreground">
                    {edits.length}
                  </span>
                ) : null}
              </TabsTrigger>
            ))}
            {hiddenTabs.length > 0 ? (
              <Popover open={moreOpen} onOpenChange={setMoreOpen}>
                <PopoverTrigger asChild>
                  <button
                    type="button"
                    aria-label="更多标签"
                    onMouseEnter={() => setMoreOpen(true)}
                    className={cn(
                      "flex h-8 shrink-0 items-center justify-center rounded-lg px-2 text-base font-medium transition-colors hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45 focus-visible:outline-none",
                      hiddenActive ? "bg-surface text-foreground" : "text-muted-foreground",
                    )}
                  >
                    …
                  </button>
                </PopoverTrigger>
                <PopoverContent align="end" side="bottom" className="w-auto p-1">
                  <ul>
                    {hiddenTabs.map((tab) => (
                      <li key={tab.value}>
                        <button
                          type="button"
                          onClick={() => {
                            setPanelTab(tab.value as PanelTab);
                            setMoreOpen(false);
                          }}
                          className={cn(
                            "flex w-full items-center justify-between gap-2 rounded-lg px-2.5 py-1.5 text-left text-base outline-none transition-colors hover:bg-accent focus-visible:outline-none",
                            panelTab === tab.value ? "text-foreground" : "text-muted-foreground",
                          )}
                        >
                          {tab.label}
                          {panelTab === tab.value ? (
                            <span className="size-1.5 rounded-full bg-brand" />
                          ) : null}
                        </button>
                      </li>
                    ))}
                  </ul>
                </PopoverContent>
              </Popover>
            ) : null}
          </TabsList>
        </div>

        <TabsContent
          value="preview"
          className="details-scroll flex min-h-0 flex-1 flex-col overflow-hidden px-4 py-4"
        >
          <PreviewPanel />
        </TabsContent>

        <TabsContent value="decision" className="min-h-0 flex-1 overflow-y-auto px-4 py-4">
          <DecisionPanel />
        </TabsContent>

        <TabsContent
          value="terminal"
          className="details-scroll flex min-h-0 flex-1 flex-col overflow-y-auto px-4 py-4"
        >
          <TerminalTab />
        </TabsContent>

        <TabsContent
          value="browser"
          className="details-scroll flex min-h-0 flex-1 flex-col overflow-y-auto px-4 py-4"
        >
          <BrowserTab />
        </TabsContent>
      </Tabs>
    </aside>
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
