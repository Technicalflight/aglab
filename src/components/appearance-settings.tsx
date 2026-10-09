import type { ReactNode } from "react";

import { cn } from "@/lib/utils";
import { ACCENT_PRESETS, CHAT_FONT_SIZES, ZOOM_STEPS, type ThemeMode } from "@/lib/theme";
import { CapabilityToggle } from "@/components/ui/capability-toggle";
import { useChatStore } from "@/store/chat-store";
import { FormColumn } from "@/components/ui/content-column";

/**
 * 设置页的「外观」项：主题模式 + 强调色 + 字号/缩放/动效。
 * 预览卡按各自主题的真实变量画，选中的卡亮出品牌色描边。
 */

const MODES: Array<{ value: ThemeMode; label: string }> = [
  { value: "dark", label: "深色" },
  { value: "light", label: "浅色" },
  { value: "system", label: "跟随系统" },
];

/** 一块缩小到 1/4 的界面示意：侧栏 + 两段对话气泡 */
function ModePreview({ mode }: { mode: ThemeMode }) {
  // 每种模式用自己主题的真实色值画，预览不撒谎
  const palettes: Record<ThemeMode, { half: "dark" | "light" | "split" }> = {
    dark: { half: "dark" },
    light: { half: "light" },
    system: { half: "split" },
  };
  const { half } = palettes[mode];
  const darkColors = { bg: "#0a0a0c", panel: "#131417", bubble: "#24252b", bar: "#3a3b44" };
  const lightColors = { bg: "#ffffff", panel: "#f2f2f4", bubble: "#e4e4e8", bar: "#c9cad1" };

  const Half = ({ colors }: { colors: typeof darkColors }) => (
    <div className="flex h-full flex-1 gap-1 p-1.5" style={{ backgroundColor: colors.bg }}>
      <div className="w-1/4 rounded-sm" style={{ backgroundColor: colors.panel }} />
      <div className="flex flex-1 flex-col justify-end gap-1">
        <div className="h-1.5 w-3/4 rounded-sm" style={{ backgroundColor: colors.bubble }} />
        <div className="h-1.5 w-1/2 rounded-sm" style={{ backgroundColor: colors.bubble }} />
        <div
          className="ml-auto h-2.5 w-2/3 rounded-sm"
          style={{ backgroundColor: "var(--brand)" }}
        />
      </div>
    </div>
  );

  if (half === "split") {
    return (
      <div className="flex h-full overflow-hidden">
        <Half colors={darkColors} />
        <Half colors={lightColors} />
      </div>
    );
  }
  return (
    <div className="h-full">
      <Half colors={half === "dark" ? darkColors : lightColors} />
    </div>
  );
}

/** 一粒小档位胶囊：字号与缩放这类"选一档"的旋钮共用同一套读法 */
function Pill({
  active,
  onClick,
  label,
  children,
}: {
  active: boolean;
  onClick: () => void;
  label: string;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      aria-pressed={active}
      aria-label={label}
      onClick={onClick}
      className={cn(
        "flex h-9 items-center gap-2 rounded-lg border px-3.5 text-sm transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
        active
          ? "border-brand bg-brand/10 text-brand-text"
          : "border-border text-muted-foreground hover:border-brand/40 hover:text-foreground",
      )}
    >
      {children}
    </button>
  );
}

export function AppearanceSettings() {
  const config = useChatStore((s) => s.config);
  const updateConfig = useChatStore((s) => s.updateConfig);

  return (
    <FormColumn>
      <h1 className="text-2xl font-semibold tracking-tight text-foreground">外观</h1>
      <p className="mt-1 text-sm leading-6 text-muted-foreground">
        界面的深浅、强调色、字号与缩放。选择即时生效并写入配置，重启后保持。
      </p>

      <div className="mt-8">
        <h2 className="text-lg font-semibold tracking-tight text-foreground">主题</h2>
        <div className="mt-3 grid grid-cols-3 gap-3">
          {MODES.map((mode) => {
            const active = config.themeMode === mode.value;
            return (
              <button
                key={mode.value}
                type="button"
                aria-pressed={active}
                onClick={() => void updateConfig({ themeMode: mode.value })}
                className={cn(
                  "group rounded-xl border p-1.5 text-left outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
                  active
                    ? "border-brand ring-1 ring-brand/50"
                    : "border-border hover:border-brand/40",
                )}
              >
                <div className="h-24 overflow-hidden rounded-lg border border-border/60">
                  <ModePreview mode={mode.value} />
                </div>
                <p
                  className={cn(
                    "px-1 pt-1.5 pb-0.5 text-center text-sm",
                    active ? "font-medium text-foreground" : "text-muted-foreground",
                  )}
                >
                  {mode.label}
                </p>
              </button>
            );
          })}
        </div>
        {config.themeMode === "system" ? (
          <p className="mt-2 text-xs leading-5 text-muted-foreground">
            跟随系统：Windows 换深浅色时 aglab 会跟着切，不用重启。
          </p>
        ) : null}
      </div>

      <div className="mt-8">
        <h2 className="text-lg font-semibold tracking-tight text-foreground">强调色</h2>
        <p className="mt-1 text-sm leading-6 text-muted-foreground">
          按钮、选中态、光标这一类" 点睛 "的颜色。悬停与浅淡底色由它自动派生，不用逐个调。
        </p>
        <div className="mt-3 flex flex-wrap items-center gap-2.5">
          {ACCENT_PRESETS.map((preset) => {
            const active = config.accentColor === preset.value;
            return (
              <button
                key={preset.label}
                type="button"
                aria-label={`强调色 ${preset.label}`}
                title={preset.label}
                aria-pressed={active}
                onClick={() => void updateConfig({ accentColor: preset.value })}
                className={cn(
                  "size-8 rounded-full border-2 transition-transform focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/45",
                  active ? "scale-110 border-foreground" : "border-transparent hover:scale-105",
                )}
                style={{ background: preset.sample }}
              />
            );
          })}

          <label
            className={cn(
              "relative size-8 cursor-pointer overflow-hidden rounded-full border-2 transition-transform",
              config.accentColor &&
                !ACCENT_PRESETS.some((preset) => preset.value === config.accentColor)
                ? "scale-110 border-foreground"
                : "border-transparent hover:scale-105",
            )}
            style={{
              background:
                "conic-gradient(#ef4444, #f59e0b, #10b981, #06b6d4, #3b82f6, #ec4899, #ef4444)",
            }}
            title="自定义颜色"
          >
            <input
              type="color"
              value={config.accentColor || "#ffffff"}
              className="absolute inset-0 cursor-pointer opacity-0"
              onChange={(event) => void updateConfig({ accentColor: event.target.value })}
            />
          </label>

          {config.accentColor ? (
            <button
              type="button"
              onClick={() => void updateConfig({ accentColor: "" })}
              className="ml-1 rounded-lg px-2 py-1 text-xs text-muted-foreground transition-colors hover:bg-accent hover:text-foreground focus-visible:outline-none"
            >
              恢复默认
            </button>
          ) : null}
        </div>
      </div>

      <div className="mt-8">
        <h2 className="text-lg font-semibold tracking-tight text-foreground">聊天字号</h2>
        <p className="mt-1 text-sm leading-6 text-muted-foreground">
          只管消息正文这一段。标题、行内代码按倍数跟着走，界面其余部分不动——
          想整体放大用下面的「界面缩放」。
        </p>
        <div className="mt-3 flex flex-wrap gap-2">
          {CHAT_FONT_SIZES.map((size) => {
            const active = config.chatFontSize === size.value;
            return (
              <Pill
                key={size.value}
                active={active}
                label={`聊天字号 ${size.label}`}
                onClick={() => void updateConfig({ chatFontSize: size.value })}
              >
                <span style={{ fontSize: size.px }} className="font-medium">
                  Aa
                </span>
                <span>{size.label}</span>
              </Pill>
            );
          })}
        </div>
      </div>

      <div className="mt-8">
        <h2 className="text-lg font-semibold tracking-tight text-foreground">界面缩放</h2>
        <p className="mt-1 text-sm leading-6 text-muted-foreground">
          整个窗口一起缩（webview 原生缩放，文字不糊）。即时生效，重启后保持。
        </p>
        <div className="mt-3 flex flex-wrap gap-2">
          {ZOOM_STEPS.map((step) => {
            const active = Math.abs(config.uiZoom - step.value) < 0.001;
            return (
              <Pill
                key={step.label}
                active={active}
                label={`界面缩放 ${step.label}`}
                onClick={() => void updateConfig({ uiZoom: step.value })}
              >
                {step.label}
              </Pill>
            );
          })}
        </div>
      </div>

      <div className="mt-8">
        <h2 className="text-lg font-semibold tracking-tight text-foreground">动效</h2>
        <div className="mt-3 flex items-center justify-between gap-3 rounded-lg border border-border bg-surface px-3 py-3">
          <div className="min-w-0">
            <p className="text-sm font-medium text-foreground">减少动效</p>
            <p className="mt-1 text-xs leading-5 text-muted-foreground">
              压掉界面过渡与动画，只留最终状态。窗口切换、弹层、气泡入场都会立刻完成。
            </p>
          </div>
          <CapabilityToggle
            label="减少动效"
            enabled={config.reduceMotion}
            onToggle={() => void updateConfig({ reduceMotion: !config.reduceMotion })}
          />
        </div>
      </div>
    </FormColumn>
  );
}
