import { useEffect, useState, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { openUrl } from "@tauri-apps/plugin-opener";
import {
  IconCopy as Copy,
  IconMinus as Minus,
  IconLayoutSidebar as PanelLeft,
  IconLayoutSidebarRight as PanelRight,
  IconSearch as Search,
  IconSquare as Square,
  IconX as X,
} from "@tabler/icons-react";

import { AboutDialog, GITHUB_PROFILE } from "@/components/about-dialog";
import { Menu, MenuContent, MenuItem, MenuTrigger } from "@/components/ui/menu";
import { useChatStore } from "@/store/chat-store";
import { cn } from "@/lib/utils";

export function TitleBar({ onOpenSearch }: { onOpenSearch: () => void }) {
  const [maximized, setMaximized] = useState(false);
  const [aboutOpen, setAboutOpen] = useState(false);
  const sidebarCollapsed = useChatStore((s) => s.sidebarCollapsed);
  const panelCollapsed = useChatStore((s) => s.panelCollapsed);
  const toggleSidebar = useChatStore((s) => s.toggleSidebar);
  const togglePanel = useChatStore((s) => s.togglePanel);
  const pushToast = useChatStore((s) => s.pushToast);

  // 最大化状态只从窗口事件读：原生拖拽区双击已经会切换窗口，前端再记一份就会不同步
  useEffect(() => {
    let win: ReturnType<typeof getCurrentWindow> | null = null;
    try {
      win = getCurrentWindow();
    } catch {
      // 浏览器里预览界面时没有 Tauri 宿主，窗口集成整体跳过
      return;
    }

    let active = true;
    const current = win;
    const sync = () => {
      void current.isMaximized().then((value) => {
        if (active) setMaximized(value);
      });
    };

    sync();
    const unlisten = current.onResized(sync);

    return () => {
      active = false;
      void unlisten.then((fn) => fn());
    };
  }, []);

  const toggleMaximize = () => {
    try {
      void getCurrentWindow().toggleMaximize();
    } catch {
      /* 同上 */
    }
  };

  return (
    <header
      data-tauri-drag-region
      className="flex h-10 shrink-0 items-center gap-1 border-b border-border bg-sidebar pr-0 pl-2 select-none"
    >
      <button
        type="button"
        aria-label={sidebarCollapsed ? "展开侧边栏" : "折叠侧边栏"}
        onClick={toggleSidebar}
        className="flex size-8 items-center justify-center rounded-lg text-muted-foreground transition-colors hover:bg-accent hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45 focus-visible:outline-none"
      >
        <PanelLeft className="size-4" />
      </button>

      <div data-tauri-drag-region className="flex items-baseline gap-2 px-2">
        <span data-tauri-drag-region className="text-base font-semibold tracking-tight">
          aglab
        </span>
        <span data-tauri-drag-region className="text-xs text-muted-foreground">
          工作台
        </span>
      </div>

      {/* Tauri 只认事件目标元素自身的属性，中间这块占位区必须自己带上 */}
      <div data-tauri-drag-region className="flex-1" />

      <Menu>
        <MenuTrigger asChild>
          <button
            type="button"
            className="flex h-8 items-center rounded-lg px-2 text-base text-muted-foreground transition-colors hover:bg-accent hover:text-foreground focus-visible:outline-none"
          >
            帮助
          </button>
        </MenuTrigger>
        {/* 靠右缘后菜单改右对齐展开，避免溢出屏幕。「关于」有平铺按钮了，
            菜单里放帮助性质的动作——反馈入口与 About 里的同一份 GitHub 地址 */}
        <MenuContent side="bottom" align="end" className="w-40">
          <MenuItem
            onSelect={() =>
              void openUrl(GITHUB_PROFILE).catch((cause) =>
                pushToast({
                  tone: "error",
                  title: "打不开问题反馈页",
                  detail: cause instanceof Error ? cause.message : String(cause),
                }),
              )
            }
          >
            <span>问题反馈</span>
          </MenuItem>
        </MenuContent>
      </Menu>

      {/* 平铺的「关于」入口：About 对话框的唯一标题栏入口，帮助菜单里放的是反馈 */}
      <button
        type="button"
        onClick={() => setAboutOpen(true)}
        className="flex h-8 items-center rounded-lg px-2 text-base text-muted-foreground transition-colors hover:bg-accent hover:text-foreground focus-visible:outline-none"
      >
        关于
      </button>

      <button
        type="button"
        aria-label="搜索话题"
        onClick={onOpenSearch}
        className="flex size-8 items-center justify-center rounded-lg text-muted-foreground transition-colors hover:bg-accent hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45 focus-visible:outline-none"
      >
        <Search className="size-4" />
      </button>

      <button
        type="button"
        aria-label={panelCollapsed ? "展开详情面板" : "折叠详情面板"}
        onClick={togglePanel}
        className={cn(
          "flex size-8 items-center justify-center rounded-lg transition-colors focus-visible:ring-2 focus-visible:ring-ring/45 focus-visible:outline-none",
          panelCollapsed
            ? "text-muted-foreground hover:bg-accent hover:text-foreground"
            : "bg-accent text-foreground",
        )}
      >
        <PanelRight className="size-4" />
      </button>

      <div className="ml-1 flex h-full items-stretch">
        <WindowButton label="最小化" onClick={() => void invoke("window_minimize")}>
          <Minus className="size-3.5" />
        </WindowButton>
        <WindowButton label={maximized ? "还原窗口" : "最大化"} onClick={toggleMaximize}>
          {maximized ? <Copy className="size-3.5" /> : <Square className="size-3" />}
        </WindowButton>
        <WindowButton label="关闭" danger onClick={() => void invoke("window_close")}>
          <X className="size-3.5" />
        </WindowButton>
      </div>

      <AboutDialog open={aboutOpen} onOpenChange={setAboutOpen} />
    </header>
  );
}

function WindowButton({
  label,
  danger,
  onClick,
  children,
}: {
  label: string;
  danger?: boolean;
  onClick: () => void;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      aria-label={label}
      onClick={onClick}
      className={cn(
        "flex h-full w-11 items-center justify-center text-muted-foreground transition-colors hover:bg-accent hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45 focus-visible:outline-none",
        danger && "hover:bg-destructive/18 hover:text-destructive",
      )}
    >
      {children}
    </button>
  );
}
