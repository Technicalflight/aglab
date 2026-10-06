import { Suspense, lazy, useEffect, useState } from "react";
import { IconAppWindow as AppWindow, IconArrowLeft as ArrowLeft, IconArrowsLeftRight as ArrowLeftRight, IconRobot as Bot, IconBrain as Brain, IconBrush as Brush, IconCoin as CircleDollarSign, IconDatabase as Database, IconGauge as Gauge, IconGlobe as Globe, IconStack2 as Layers, IconPlug as Plug, IconRoute as Route, IconFileDescription as ScrollText, IconShieldHalf as ShieldHalf, IconServer as Server, IconAdjustmentsHorizontal as SlidersHorizontal, IconStopwatch as Stopwatch, IconTerminal2 as Terminal2, IconUsers as Users, IconWorld as World, IconCode as Code, IconTool as Wrench, IconFileText as FileLock, IconGavel as Gavel, IconShieldLock as ShieldLock, IconHistory as History, IconRadar as Radar, IconShieldCheck as ShieldCheck } from "@tabler/icons-react";

import { CcswitchImportBlock } from "@/components/ccswitch-import-block";
import { useChatStore } from "@/store/chat-store";
import { useIsNarrow } from "@/lib/use-media-query";
import { warmSubagentCaches } from "@/lib/subagent-cache";
import { cn } from "@/lib/utils";
import { FormColumn } from "@/components/ui/content-column";

// 二十多个设置页全部懒加载：设置壳（分组导航 + 数据预热）常驻，每页的包等到
// 点进去才拉——服务商档案 1300 行、记忆页 2000 行，全量塞进首屏曾是 index 包
// 1.3 MB 的主要成分。预热函数住在 lib 小模块里，壳引用它不会带上页面本体。
const AppearanceSettings = lazy(() =>
  import("@/components/appearance-settings").then((m) => ({ default: m.AppearanceSettings })),
);
const AppSettings = lazy(() =>
  import("@/components/app-settings").then((m) => ({ default: m.AppSettings })),
);
const SubagentSettings = lazy(() =>
  import("@/components/subagent-settings").then((m) => ({ default: m.SubagentSettings })),
);
const BrowserSettings = lazy(() =>
  import("@/components/browser-settings").then((m) => ({ default: m.BrowserSettings })),
);
const BehaviorSettings = lazy(() =>
  import("@/components/behavior-settings").then((m) => ({ default: m.BehaviorSettings })),
);
const DecisionSettings = lazy(() =>
  import("@/components/decision-settings").then((m) => ({ default: m.DecisionSettings })),
);
const ImportSettings = lazy(() =>
  import("@/components/import-settings").then((m) => ({ default: m.ImportSettings })),
);
const RelayProbeSettings = lazy(() =>
  import("@/components/relay-probe-settings").then((m) => ({ default: m.RelayProbeSettings })),
);
const LspSettings = lazy(() =>
  import("@/components/lsp-settings").then((m) => ({ default: m.LspSettings })),
);
const McpSettings = lazy(() =>
  import("@/components/mcp-settings").then((m) => ({ default: m.McpSettings })),
);
const MemorySettings = lazy(() =>
  import("@/components/memory-settings").then((m) => ({ default: m.MemorySettings })),
);
const ModelPoolSettings = lazy(() =>
  import("@/components/model-pool-settings").then((m) => ({ default: m.ModelPoolSettings })),
);
const ModelRouteSettings = lazy(() =>
  import("@/components/model-route-settings").then((m) => ({ default: m.ModelRouteSettings })),
);
const ProxySettings = lazy(() =>
  import("@/components/proxy-settings").then((m) => ({ default: m.ProxySettings })),
);
const ProfileSettings = lazy(() =>
  import("@/components/profile-settings").then((m) => ({ default: m.ProfileSettings })),
);
const SecuritySettings = lazy(() =>
  import("@/components/security-settings").then((m) => ({ default: m.SecuritySettings })),
);
const FileRulesSettings = lazy(() =>
  import("@/components/file-rules-settings").then((m) => ({ default: m.FileRulesSettings })),
);
const CommandRulesSettings = lazy(() =>
  import("@/components/command-rules-settings").then((m) => ({ default: m.CommandRulesSettings })),
);
const NetworkRulesSettings = lazy(() =>
  import("@/components/network-rules-settings").then((m) => ({ default: m.NetworkRulesSettings })),
);
const AuditSettings = lazy(() =>
  import("@/components/audit-settings").then((m) => ({ default: m.AuditSettings })),
);
const DataSecuritySettings = lazy(() =>
  import("@/components/data-security-settings").then((m) => ({ default: m.DataSecuritySettings })),
);
const SkillsView = lazy(() =>
  import("@/components/skills-view").then((m) => ({ default: m.SkillsView })),
);
const SshSettings = lazy(() =>
  import("@/components/ssh-settings").then((m) => ({ default: m.SshSettings })),
);
const StorageSettings = lazy(() =>
  import("@/components/storage-settings").then((m) => ({ default: m.StorageSettings })),
);
const TaskRuntimeSettings = lazy(() =>
  import("@/components/task-runtime-settings").then((m) => ({ default: m.TaskRuntimeSettings })),
);
const ToolsView = lazy(() =>
  import("@/components/tools-view").then((m) => ({ default: m.ToolsView })),
);
const UsageView = lazy(() =>
  import("@/components/usage-view").then((m) => ({ default: m.UsageView })),
);
const WebSearchSettings = lazy(() =>
  import("@/components/web-search-settings").then((m) => ({ default: m.WebSearchSettings })),
);

/** 设置页包在后台拉取时的占位：画在右栏内容区里 */
function PageFallback() {
  return (
    <div className="flex h-full min-h-40 items-center justify-center" aria-busy="true">
      <p className="text-sm text-muted-foreground">加载中…</p>
    </div>
  );
}

type SettingsTab =
  | "appearance"
  | "app"
  | "profiles"
  | "modelPool"
  | "modelRoutes"
  | "proxy"
  | "usage"
  | "behavior"
  | "security"
  | "dataSecurity"
  | "fileRules"
  | "commandRules"
  | "networkRules"
  | "toolControl"
  | "auditCenter"
  | "decision"
  | "subagents"
  | "browser"
  | "taskRuntime"
  | "skills"
  | "mcp"
  | "webSearch"
  | "ssh"
  | "lsp"
  | "storage"
  | "memory"
  | "import"
  | "ccswitch"
  | "relayProbe";

const GROUPS: Array<{
  label: string;
  items: Array<{ value: SettingsTab; label: string; icon: typeof Server }>;
}> = [
  {
    label: "通用",
    items: [
      { value: "appearance", label: "外观", icon: Brush },
      { value: "app", label: "应用", icon: AppWindow },
    ],
  },
  {
    label: "模型",
    items: [
      { value: "profiles", label: "服务商档案", icon: Server },
      { value: "modelPool", label: "模型池", icon: Layers },
      { value: "modelRoutes", label: "模型路由", icon: Route },
      { value: "proxy", label: "代理", icon: Globe },
      { value: "usage", label: "用量", icon: CircleDollarSign },
    ],
  },
  {
    label: "助理",
    items: [
      { value: "behavior", label: "运行行为", icon: SlidersHorizontal },
      { value: "decision", label: "决策层", icon: Gauge },
      { value: "subagents", label: "子助理", icon: Users },
      { value: "browser", label: "浏览器控制", icon: Globe },
      { value: "taskRuntime", label: "定时任务", icon: Stopwatch },
    ],
  },
  {
    label: "安全",
    items: [
      { value: "security", label: "安全概览", icon: ShieldHalf },
      { value: "dataSecurity", label: "数据安全", icon: ShieldCheck },
      { value: "fileRules", label: "文件安全", icon: FileLock },
      { value: "commandRules", label: "命令安全", icon: Gavel },
      { value: "networkRules", label: "网络安全", icon: ShieldLock },
      { value: "relayProbe", label: "中转站探针", icon: Radar },
      { value: "toolControl", label: "工具管控", icon: Wrench },
      { value: "auditCenter", label: "审计中心", icon: History },
    ],
  },
  {
    label: "扩展",
    items: [
      { value: "skills", label: "技能", icon: ScrollText },
      { value: "mcp", label: "MCP 服务", icon: Plug },
      { value: "webSearch", label: "联网搜索", icon: World },
      { value: "ssh", label: "SSH 主机", icon: Terminal2 },
      { value: "lsp", label: "LSP 服务器", icon: Code },
    ],
  },
  {
    label: "数据",
    items: [
      { value: "storage", label: "本地存储", icon: Database },
      { value: "memory", label: "记忆", icon: Brain },
    ],
  },
  {
    label: "导入",
    items: [
      { value: "import", label: "其他 AI 应用", icon: Bot },
      { value: "ccswitch", label: "cc-switch 迁移", icon: ArrowLeftRight },
    ],
  },
];

/**
 * 设置页：参考 Codex 桌面端，左侧分组导航 + 右侧内容。
 * 工具/技能/用量原本是独立分区，现在整视图原样搬进来——
 * 它们自带 SectionFrame 栏头，右栏不再重复画标题。
 */
export function SettingsView() {
  const setSection = useChatStore((s) => s.setSection);
  const [tab, setTab] = useState<SettingsTab>("profiles");

  // 进设置壳就预热子助理页的两份后台数据（名册 + 模型目录）：
  // 默认落在服务商档案页，等用户点到「子助理」时手里已有现成的，进页即画
  useEffect(() => {
    warmSubagentCaches(useChatStore.getState().config);
  }, []);

  // 窄屏（<1024）分组导航收成可横向滚动的标签条：240px 竖排在 768px 窗口里
  // 会把设置内容压到不足 500px，而设置项本身都是宽表单，更需要横向空间。
  const narrow = useIsNarrow();

  return (
    // min-h-0：窄屏（flex-col）时与 chat-area 同理，缺了它长表单会把设置页撑出窗口
    <div className="flex min-h-0 min-w-0 flex-1 flex-col lg:flex-row">
      <nav
        aria-label="设置分组"
        className={cn(
          "shrink-0 border-r border-border bg-sidebar",
          narrow
            ? "flex gap-1 overflow-x-auto border-b border-r-0 px-3 py-2"
            : "flex w-60 flex-col",
        )}
      >
        <button
          type="button"
          onClick={() => setSection("chats")}
          className={cn(
            "flex shrink-0 items-center gap-2 rounded-md px-2 py-1.5 text-left text-base text-muted-foreground transition-colors hover:bg-accent hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45",
            narrow ? "order-last" : "mx-3 mt-3",
          )}
        >
          <ArrowLeft className="size-3.5" />
          <span>返回应用</span>
        </button>

        <div
          className={cn(
            "flex-1 px-3 pb-3",
            narrow ? "mt-0 flex items-center gap-2 overflow-x-auto" : "mt-3 overflow-y-auto",
          )}
        >
          {GROUPS.map((group) => (
            <div key={group.label} className={cn(narrow ? "shrink-0" : "mb-3")}>
              <p
                className={cn(
                  "px-2 pb-1.5 text-xs font-medium tracking-[0.08em] text-foreground-tertiary uppercase",
                  narrow && "sr-only",
                )}
              >
                {group.label}
              </p>
              <ul className={cn(narrow ? "flex gap-1" : "space-y-0.5")}>
                {group.items.map((item) => {
                  const Icon = item.icon;
                  const active = tab === item.value;
                  return (
                    <li key={item.value}>
                      <button
                        type="button"
                        aria-current={active ? "true" : undefined}
                        onClick={() => setTab(item.value)}
                        className={cn(
                          "group relative flex items-center gap-2 rounded-md px-2.5 py-1.5 text-left text-base whitespace-nowrap outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
                          narrow ? "shrink-0" : "w-full",
                          active
                            ? "bg-surface text-foreground"
                            : "text-muted-foreground hover:bg-accent hover:text-foreground",
                        )}
                      >
                        {active ? (
                          <span
                            className={cn(
                              "absolute bg-brand",
                              narrow ? "inset-x-2.5 -bottom-0.5 h-0.5" : "top-1/2 left-0 h-5 w-0.5 -translate-y-1/2",
                            )}
                          />
                        ) : null}
                        <Icon className="size-4 shrink-0" />
                        <span className="truncate">{item.label}</span>
                      </button>
                    </li>
                  );
                })}
              </ul>
            </div>
          ))}
        </div>
      </nav>

      <div className="min-h-0 min-w-0 flex-1 overflow-y-auto">
        {/* 各页都是懒组件：切到哪页拉哪页的包，加载期右栏显示占位 */}
        <Suspense fallback={<PageFallback />}>
          {tab === "appearance" ? <AppearanceSettings /> : null}
          {tab === "app" ? <AppSettings /> : null}

          {tab === "profiles" ? <ProfileSettings /> : null}
          {tab === "modelPool" ? <ModelPoolSettings /> : null}
          {tab === "modelRoutes" ? <ModelRouteSettings /> : null}
          {tab === "proxy" ? <ProxySettings /> : null}
          {tab === "usage" ? <UsageView /> : null}

          {tab === "behavior" ? <BehaviorSettings /> : null}
          {tab === "security" ? <SecuritySettings /> : null}
          {tab === "dataSecurity" ? <DataSecuritySettings /> : null}
          {tab === "fileRules" ? <FileRulesSettings /> : null}
          {tab === "commandRules" ? <CommandRulesSettings /> : null}
          {tab === "networkRules" ? <NetworkRulesSettings /> : null}
          {tab === "relayProbe" ? <RelayProbeSettings /> : null}
          {tab === "auditCenter" ? <AuditSettings /> : null}
          {tab === "decision" ? <DecisionSettings /> : null}
          {tab === "subagents" ? <SubagentSettings /> : null}
          {tab === "browser" ? <BrowserSettings /> : null}
          {tab === "taskRuntime" ? <TaskRuntimeSettings /> : null}

          {tab === "toolControl" ? <ToolsView /> : null}
          {tab === "skills" ? <SkillsView /> : null}
          {tab === "mcp" ? <McpSettings /> : null}
          {tab === "webSearch" ? <WebSearchSettings /> : null}
          {tab === "ssh" ? <SshSettings /> : null}
          {tab === "lsp" ? <LspSettings /> : null}

          {tab === "storage" ? <StorageSettings /> : null}
          {tab === "memory" ? <MemorySettings /> : null}

          {tab === "import" ? <ImportSettings /> : null}
          {tab === "ccswitch" ? <CcswitchSettings /> : null}
        </Suspense>
      </div>
    </div>
  );
}

/** cc-switch 迁移没有独立组件文件：整页只有那个折叠区块，标题写在这里 */
function CcswitchSettings() {
  return (
    <FormColumn>
      <h1 className="text-2xl font-semibold tracking-tight text-foreground">cc-switch 迁移</h1>
      <p className="mt-1 text-sm leading-6 text-muted-foreground">
        从 cc-switch 把供应商、价表、MCP 服务器和技能搬进 aglab。数据只读，密钥直进凭据管理器。
      </p>
      <div className="mt-6">
        <CcswitchImportBlock />
      </div>
    </FormColumn>
  );
}
