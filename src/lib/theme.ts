import { useEffect } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { AppConfig, ChatFontSize } from "@/types/chat";

export type ThemeMode = AppConfig["themeMode"];

/** 预设强调色。空串表示跟随主题默认（深色=白、浅色=黑），排第一；
 *  sample 是完整 background 值——默认这颗用双色对半圆片表达"跟着主题走" */
export const ACCENT_PRESETS: Array<{ value: string; label: string; sample: string }> = [
  { value: "", label: "默认", sample: "linear-gradient(135deg, #ffffff 50%, #1a1c1f 50%)" },
  { value: "#3b82f6", label: "蓝", sample: "#3b82f6" },
  { value: "#10b981", label: "绿", sample: "#10b981" },
  { value: "#f59e0b", label: "橙", sample: "#f59e0b" },
  { value: "#ef4444", label: "红", sample: "#ef4444" },
  { value: "#06b6d4", label: "青", sample: "#06b6d4" },
  { value: "#ec4899", label: "粉", sample: "#ec4899" },
];

/** 聊天字号档位。px 是正文那一档的基准；标题与代码由 CSS 按倍数派生，不逐个配 */
export const CHAT_FONT_SIZES: Array<{ value: ChatFontSize; label: string; px: string }> = [
  { value: "small", label: "小", px: "12.5px" },
  { value: "medium", label: "标准", px: "14px" },
  { value: "large", label: "大", px: "15.5px" },
  { value: "xlarge", label: "特大", px: "17px" },
];

/** 界面缩放档位。走 webview 原生 zoom，文字与界面一起缩，CSS zoom 会糊所以不用 */
export const ZOOM_STEPS: Array<{ value: number; label: string }> = [
  { value: 0.9, label: "90%" },
  { value: 1, label: "100%" },
  { value: 1.1, label: "110%" },
  { value: 1.25, label: "125%" },
];

/** 把主题模式和强调色刷到文档根元素上。hover/subtle 由 CSS color-mix 自动派生 */
export function applyTheme(config: Pick<AppConfig, "themeMode" | "accentColor">) {
  const root = document.documentElement;
  const preferLight = window.matchMedia("(prefers-color-scheme: light)").matches;
  const light =
    config.themeMode === "light" || (config.themeMode === "system" && preferLight);
  root.classList.toggle("light", light);

  // 强调色直接覆盖 --brand；空串回到主题默认，hover/subtle 跟着一起变
  if (config.accentColor) {
    root.style.setProperty("--brand", config.accentColor);
  } else {
    root.style.removeProperty("--brand");
  }
}

/** 把字号档、减少动效、缩放、置顶刷到文档根与窗口上。与 applyTheme 一样是纯覆写，可重复调用 */
function applyUiPrefs(
  config: Pick<AppConfig, "chatFontSize" | "reduceMotion" | "uiZoom" | "alwaysOnTop">,
) {
  const root = document.documentElement;
  const size =
    CHAT_FONT_SIZES.find((item) => item.value === config.chatFontSize) ?? CHAT_FONT_SIZES[1];
  root.style.setProperty("--chat-font-size", size.px);
  root.classList.toggle("reduce-motion", config.reduceMotion);
  // 缩放与置顶在 Rust 侧落地（webview 原生能力），失败只当没这回事：
  // 它们是体验项，不该把一条报错顶到正在打字的用户面前
  void invoke("window_zoom", { scale: config.uiZoom }).catch(() => undefined);
  void invoke("window_set_always_on_top", { onTop: config.alwaysOnTop }).catch(() => undefined);
}

/** 主题随配置与系统外观联动，顺带应用字号/动效/缩放/置顶。挂在 App 根上一次即可 */
export function useTheme(
  config: Pick<
    AppConfig,
    "themeMode" | "accentColor" | "chatFontSize" | "reduceMotion" | "uiZoom" | "alwaysOnTop"
  >,
) {
  useEffect(() => {
    applyTheme(config);
    applyUiPrefs(config);
    if (config.themeMode !== "system") return;

    const media = window.matchMedia("(prefers-color-scheme: light)");
    const listener = () => applyTheme(config);
    media.addEventListener("change", listener);
    return () => media.removeEventListener("change", listener);
  }, [
    config.themeMode,
    config.accentColor,
    config.chatFontSize,
    config.reduceMotion,
    config.uiZoom,
    config.alwaysOnTop,
  ]);
}
