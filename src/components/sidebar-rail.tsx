import type { ComponentProps } from "react";
import {
  IconBooks as Books,
  IconClock as Timer,
  IconDatabaseImport as DatabaseImport,
  IconGitPullRequest as GitPullRequest,
  IconMoon as Moon,
  IconMovie as VideoKind,
  IconMusic as MusicKind,
  IconPhotoAi as ImageKind,
  IconPencil as SquarePen,
  IconPuzzle as Puzzle,
  IconSettings as Settings,
  IconSun as Sun,
} from "@tabler/icons-react";

import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { useChatStore } from "@/store/chat-store";
import { useMediaQuery } from "@/lib/use-media-query";
import type { ConversationKind, SidebarSection } from "@/types/chat";
import { cn } from "@/lib/utils";

/**
 * 左轨（分区 2）：内嵌面板最左那条竖排控制条，四段自上而下——
 * 模式切换 / 分区导航 / 深浅色 / 设置。
 *
 * 它**永远在场**：折叠开关管的是它右边那一段（资源管理器 + 会话/项目），
 * 不是这条轨。理由是这四段回答的是"去哪"与"什么模式"，任何一屏都用得上，
 * 跟着一起收起来只会逼用户先展开再点一下。
 *
 * 对齐设计稿的卡片语言：轨自己不再是一张带边框的卡，而是灰面板（App 挂的
 * --sunken 底）上的一列图标格——选中格浮成白色药丸、图标染品牌色。
 *
 * 窄屏（<1024）由 App 直接不挂右侧那段，轨照旧——它不是可压缩的那一侧。
 */

/** 模式切换：四个能力档。点哪一档就开一条该档的新会话 */
const MODES: Array<{ kind: ConversationKind; label: string; Icon: typeof SquarePen }> = [
  { kind: "chat", label: "对话", Icon: SquarePen },
  { kind: "image", label: "生图", Icon: ImageKind },
  { kind: "video", label: "视频", Icon: VideoKind },
  { kind: "music", label: "音乐", Icon: MusicKind },
];

/** 分区导航：工作分区各占一格。设置单独放在轨底，不混进这一组 */
const NAV: Array<{
  value: Exclude<SidebarSection, "chats" | "settings">;
  label: string;
  Icon: typeof GitPullRequest;
}> = [
  { value: "review", label: "变更请求", Icon: GitPullRequest },
  { value: "tasks", label: "定时任务", Icon: Timer },
  { value: "knowledge", label: "资料库", Icon: Books },
  { value: "agents", label: "助手库", Icon: DatabaseImport },
  { value: "plugins", label: "插件", Icon: Puzzle },
];

/**
 * 轨上的一格。36px 图标 + 36px 命中区，选中态浮成白色药丸。
 *
 * 多出来的 props 与 ref 必须原样落到里面的 button 上：这一格是
 * `TooltipTrigger asChild` 的孩子，Radix 会把 tooltip 的 aria 与事件处理器
 * clone 进来——丢掉它们，气泡永远不弹、读屏也读不到提示。
 */
function RailCell({
  label,
  active,
  className,
  children,
  ...rest
}: ComponentProps<"button"> & { label: string; active?: boolean }) {
  return (
    <button
      type="button"
      aria-label={label}
      aria-current={active ? "true" : undefined}
      className={cn(
        "group relative flex size-9 shrink-0 items-center justify-center rounded-lg outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
        active
          ? "bg-elevated text-brand-text shadow-sm"
          : "text-foreground-secondary hover:bg-accent hover:text-foreground",
        className,
      )}
      {...rest}
    >
      {children}
    </button>
  );
}

export function SidebarRail() {
  const section = useChatStore((s) => s.section);
  const kind = useChatStore((s) => s.kind);
  const setSection = useChatStore((s) => s.setSection);
  const startConversation = useChatStore((s) => s.startConversation);
  const themeMode = useChatStore((s) => s.config.themeMode);
  const updateConfig = useChatStore((s) => s.updateConfig);

  // 系统偏好只在 themeMode === "system" 时参与判断；其余情况用户已经表态了，
  // 不该再被系统覆盖。这样按钮上的太阳/月亮与屏幕上真正显示的那套永远一致
  const prefersLight = useMediaQuery("(prefers-color-scheme: light)");
  const light = themeMode === "light" || (themeMode === "system" && prefersLight);

  return (
    <nav aria-label="导航轨" className="flex w-14 shrink-0 flex-col items-center gap-1 py-2">
      {/* 模式切换：四档合圈在一个描边容器里——它们是“开关”不是“入口”，
          整组圈起来才与下方的工作分区导航隔得开。选中格白圆从灰底里浮出来 */}
      <div
        role="group"
        aria-label="模式切换"
        className="flex w-fit shrink-0 flex-col items-center gap-1 rounded-xl border border-foreground/25 p-0.5"
      >
        {MODES.map(({ kind: value, label, Icon }) => (
          <Tooltip key={value}>
            <TooltipTrigger asChild>
              <RailCell
                label={`新建${label}会话`}
                active={kind === value}
                onClick={() => startConversation(value)}
              >
                <Icon className="size-4" />
              </RailCell>
            </TooltipTrigger>
            <TooltipContent side="right">{label}模式 · 开新会话</TooltipContent>
          </Tooltip>
        ))}
      </div>

      <nav aria-label="工作分区" className="mt-3 flex w-full shrink-0 flex-col items-center gap-1">
        {NAV.map(({ value, label, Icon }) => (
          <Tooltip key={value}>
            <TooltipTrigger asChild>
              <RailCell label={label} active={section === value} onClick={() => setSection(value)}>
                <Icon className="size-4" />
              </RailCell>
            </TooltipTrigger>
            <TooltipContent side="right">{label}</TooltipContent>
          </Tooltip>
        ))}
      </nav>

      <span className="flex-1" />

      <Tooltip>
        <TooltipTrigger asChild>
          <RailCell
            label={light ? "切换到深色" : "切换到浅色"}
            onClick={() => void updateConfig({ themeMode: light ? "dark" : "light" })}
          >
            {light ? <Moon className="size-4" /> : <Sun className="size-4" />}
          </RailCell>
        </TooltipTrigger>
        <TooltipContent side="right">
          {light ? "深浅色 · 现在浅色" : "深浅色 · 现在深色"}
        </TooltipContent>
      </Tooltip>

      <Tooltip>
        <TooltipTrigger asChild>
          <RailCell
            label="设置"
            active={section === "settings"}
            onClick={() => setSection("settings")}
          >
            <Settings className="size-4" />
          </RailCell>
        </TooltipTrigger>
        <TooltipContent side="right">设置</TooltipContent>
      </Tooltip>
    </nav>
  );
}
