import { Suspense, lazy, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

import { ChatArea } from "@/components/chat-area";
import { CloseAskDialog } from "@/components/close-ask-dialog";
import { RightPanel } from "@/components/right-panel";
import { SearchDialog } from "@/components/search-dialog";
import { UpdatePrompt } from "@/components/update-prompt";
import { Sidebar } from "@/components/sidebar";
import { ToastLayer } from "@/components/toast-layer";
import { TitleBar } from "@/components/title-bar";
import { TooltipProvider } from "@/components/ui/tooltip";
import { useIsNarrow } from "@/lib/use-media-query";
import { useTheme } from "@/lib/theme";
import { installDecisionBridge } from "@/lib/decision/bridge";
import { warmReviewCache } from "@/lib/review-cache";
import type { MemoryConfig, MemoryStats } from "@/lib/memory";
import { useChatStore } from "@/store/chat-store";

// 非聊天分区整体懒加载：聊天是首屏，其余分区等第一次点进侧栏再各自拉包。
// 与保活容器分工——保活管"进过就常驻、切走不卸载"，懒加载管"没进过不付成本"。
// 预热函数都住在 lib 小模块里，App 壳引用它们不会把页面本体拖回首屏包
const ReviewView = lazy(() =>
  import("@/components/review-view").then((m) => ({ default: m.ReviewView })),
);
const TasksView = lazy(() =>
  import("@/components/tasks-view").then((m) => ({ default: m.TasksView })),
);
const KnowledgeView = lazy(() =>
  import("@/components/knowledge-view").then((m) => ({ default: m.KnowledgeView })),
);
const PluginsView = lazy(() =>
  import("@/components/plugins-view").then((m) => ({ default: m.PluginsView })),
);
const SettingsView = lazy(() =>
  import("@/components/settings-view").then((m) => ({ default: m.SettingsView })),
);

/** 分区包在后台拉取时的占位：挂在保活容器里，main 的 flex 子元素 */
function SectionFallback() {
  return (
    <div className="flex flex-1 items-center justify-center" aria-busy="true">
      <p className="text-sm text-muted-foreground">加载中…</p>
    </div>
  );
}

export default function App() {
  const [searchOpen, setSearchOpen] = useState(false);
  const panelCollapsed = useChatStore((s) => s.panelCollapsed);
  const setUi = useChatStore((s) => s.setUi);
  const section = useChatStore((s) => s.section);
  const config = useChatStore((s) => s.config);
  const bootstrap = useChatStore((s) => s.bootstrap);
  const refreshTasks = useChatStore((s) => s.refreshTasks);
  const refreshMcp = useChatStore((s) => s.refreshMcp);
  const refreshHistory = useChatStore((s) => s.refreshHistory);
  const narrow = useIsNarrow();

  // 主题模式与强调色随配置走，跟随系统时监听系统外观变化
  useTheme(config);

  // 窗口拖到窄屏时自动收起右栏：它是三个区域里最不吃压缩的一个，
  // 硬留着只会把主内容挤没。拉回宽屏后不自动展开——那会打断正在看的内容。
  useEffect(() => {
    if (narrow) setUi({ panelCollapsed: true });
  }, [narrow, setUi]);

  useEffect(() => {
    void bootstrap();
  }, [bootstrap]);

  // 变更请求页预热：起来 2 秒后后台拉一次 git 清单（错开启动窗口，不抢资源），
  // 第一次点进侧栏「变更请求」也是进页即画——对齐设置壳预热子助理数据的做法
  useEffect(() => {
    const timer = setTimeout(() => warmReviewCache(), 2000);
    return () => clearTimeout(timer);
  }, []);

  // 分区保活：到访过的分区常驻挂载，切走时只藏不卸。数据缓存只解决了"等数"，
  // 大树（聊天区几百条消息、变更请求的长清单）的反复装卸仍发生在主线程上，
  // 那才是快速切换卡顿的大头。没去过的分区不挂载——不为没点过的页面付首挂成本
  const [visited, setVisited] = useState<Record<string, boolean>>(() => ({ [section]: true }));
  useEffect(() => {
    setVisited((prev) => (prev[section] ? prev : { ...prev, [section]: true }));
  }, [section]);

  // 记忆的空闲自动蒸馏：开着时每半小时看一眼要不要蒸馏，需要就自动跑一次，
  // 至多一天一次——蒸馏要花一次服务商请求，所以默认关、开了也省着花。
  // 服务商请求与聊天同路但各走各的连接，spawn_blocking 不冻界面；写盘有写锁排队
  useEffect(() => {
    const tick = async () => {
      try {
        const memoryConfig = await invoke<MemoryConfig>("memory_config_get");
        if (!memoryConfig.enabled || !memoryConfig.autoDistill) return;
        const key = "aglab-last-auto-distill";
        const last = Number(window.localStorage.getItem(key) ?? 0);
        if (Date.now() - last < 24 * 60 * 60 * 1000) return;
        const stats = await invoke<MemoryStats>("memory_stats");
        if (!stats.needsDistill) return;
        window.localStorage.setItem(key, String(Date.now()));
        await invoke("memory_distill");
      } catch {
        // 自动蒸馏是帮忙不是义务：任何一步失败都安静等下一轮
      }
    };
    const first = window.setTimeout(() => void tick(), 90 * 1000);
    const timer = window.setInterval(() => void tick(), 30 * 60 * 1000);
    return () => {
      window.clearTimeout(first);
      window.clearInterval(timer);
    };
  }, []);

  // 全局快捷键：Ctrl+K 搜索话题、Ctrl+N 新聊天。和系统级快捷键一样，生成中照常可用
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (!(event.ctrlKey || event.metaKey) || event.shiftKey || event.altKey) return;
      const key = event.key.toLowerCase();
      if (key === "k") {
        event.preventDefault();
        setSearchOpen(true);
      } else if (key === "n") {
        event.preventDefault();
        useChatStore.getState().startConversation();
      } else if (key === "o") {
        event.preventDefault();
        useChatStore.getState().setProjectDialogOpen(true);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);

  // 决策桥：编排派工与记忆注入排序在 Rust 后台拿不准时，经这里问决策层（§7.3/§7.5）。
  // 装一次听一辈子；监听缺席只会让 Rust 侧等超时后 fail-open，不炸任何一路
  useEffect(() => installDecisionBridge(), []);

  // 定时任务跑完、扩展连上都发生在后台线程，靠事件把界面拉新
  useEffect(() => {
    const unlisteners: Array<() => void> = [];
    let cancelled = false;

    void Promise.all([
      listen("task-ran", () => {
        void refreshTasks();
        void refreshHistory();
      }),
      listen("mcp-status", () => {
        void refreshMcp();
      }),
    ]).then((stops) => {
      if (cancelled) stops.forEach((stop) => stop());
      else unlisteners.push(...stops);
    });

    return () => {
      cancelled = true;
      unlisteners.forEach((stop) => stop());
    };
  }, [refreshTasks, refreshHistory, refreshMcp]);

  return (
    <TooltipProvider>
      <div className="flex h-full flex-col overflow-hidden bg-background text-foreground">
        <TitleBar onOpenSearch={() => setSearchOpen(true)} />
        {/*
          min-w-0 是这条横向 flex 链的承重节点：少了它，容器的 min-width:auto
          会等于两侧固定栏之和（240 + 384 = 624px），窗口拖窄到 624px 以下时
          主内容被压成负宽，再被外层 overflow-hidden 静默裁掉右缘。
        */}
        <div className="flex min-h-0 min-w-0 flex-1">
          {/* 设置页全屏显示：它自带返回应用和分组导航，主侧栏在这里没有意义 */}
          {section === "settings" ? null : <Sidebar />}
          {/* 主内容是唯一可压缩的一侧：min-w-0 让它能被压到任意窄而不撑破父级 */}
          <main className="flex min-h-0 min-w-0 flex-1 flex-col">
            {/*
              保活容器：可见时 display:contents——盒子自身从布局里消失，分区根节点
              直接当 main 的 flex 子元素，既有的 min-h-0/flex-1 承重链原样保留；
              隐藏时 display:none——不参与布局不重绘，React 树连同滚动位置、
              展开与草稿等状态全部原地保住，切回来零重建成本。
            */}
            <div style={{ display: section === "chats" ? "contents" : "none" }}>
              {visited.chats ? <ChatArea /> : null}
            </div>
            {/* 懒分区共用一个边界：任一分区首次进点在拉包时显示占位，已加载的分区不受影响 */}
            <Suspense fallback={<SectionFallback />}>
              <div style={{ display: section === "review" ? "contents" : "none" }}>
                {visited.review ? <ReviewView /> : null}
              </div>
              <div style={{ display: section === "tasks" ? "contents" : "none" }}>
                {visited.tasks ? <TasksView /> : null}
              </div>
              <div style={{ display: section === "knowledge" ? "contents" : "none" }}>
                {visited.knowledge ? <KnowledgeView /> : null}
              </div>
              <div style={{ display: section === "plugins" ? "contents" : "none" }}>
                {visited.plugins ? <PluginsView /> : null}
              </div>
              <div style={{ display: section === "settings" ? "contents" : "none" }}>
                {visited.settings ? <SettingsView /> : null}
              </div>
            </Suspense>
          </main>
          {/*
            窄屏时右栏改成浮层盖在内容上：常驻的 384px 在 768px 窗口里会吃掉
            一半宽度，而桌面端此刻往往正是用户对照着看的时候。
            遮罩点击即收起，符合"浮层"的普遍预期。
          */}
          {panelCollapsed || section === "settings" ? null : narrow ? (
            <>
              <button
                type="button"
                aria-label="收起详情面板"
                onClick={() => setUi({ panelCollapsed: true })}
                className="fixed inset-0 z-overlay bg-black/40 backdrop-blur-[1px] lg:hidden"
              />
              <RightPanel className="fixed inset-y-0 right-0 z-modal shadow-lg lg:static lg:z-auto lg:shadow-none" />
            </>
          ) : (
            <RightPanel />
          )}
        </div>
        <SearchDialog open={searchOpen} onOpenChange={setSearchOpen} />
        <CloseAskDialog />
        <UpdatePrompt />
        <ToastLayer />
      </div>
    </TooltipProvider>
  );
}
