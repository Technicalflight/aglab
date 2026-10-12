// 首帧主题垫片：在样式表与 React 之前把深浅色摆对。浅色是出厂默认（index.html
// 的 :root 即浅色），只有上次会话用过深色时才需要挂 html.dark。
// 提示值由 applyTheme 每次应用主题时写入（aglab-theme-hint）。
// 必须是外链脚本：CSP 的 script-src 'self' 不放内联脚本。
try {
  if (localStorage.getItem("aglab-theme-hint") === "dark") {
    document.documentElement.classList.add("dark");
  }
} catch {
  // localStorage 不可用（隐私模式等）：按默认浅色走
}
