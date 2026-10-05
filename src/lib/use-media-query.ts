import { useEffect, useState } from "react";

/**
 * 视口断点订阅。Tauri 窗口可拖拽变窄，桌面端也得像样地收窄布局，
 * 所以这里监听窗口宽度而不是假定"永远是宽屏"。
 *
 * 断点与 index.css 的 --breakpoint-* 一致：sm 640 / md 768 / lg 1024 / xl 1280。
 * 返回布尔而不是查询字符串，调用处直接写三元更好读。
 */
export function useMediaQuery(query: string): boolean {
  const [matches, setMatches] = useState(() =>
    typeof window === "undefined" ? false : window.matchMedia(query).matches,
  );

  useEffect(() => {
    const media = window.matchMedia(query);
    const listener = (event: MediaQueryListEvent) => setMatches(event.matches);
    // 挂上前先对一次：首次渲染到 effect 之间窗口可能被拖过，状态会停在旧值
    setMatches(media.matches);
    media.addEventListener("change", listener);
    return () => media.removeEventListener("change", listener);
  }, [query]);

  return matches;
}

/** 窄于 lg（1024）：侧栏与右栏不能同时常驻，改成浮层 */
export function useIsNarrow(): boolean {
  return useMediaQuery("(max-width: 1023px)");
}

/** 窄于 md（768）：连单列内容都要收紧留白 */
export function useIsCompact(): boolean {
  return useMediaQuery("(max-width: 767px)");
}

/** 尊重系统"减少动态效果"。与 index.css 的媒体查询同源，两处一起生效 */
export function usePrefersReducedMotion(): boolean {
  return useMediaQuery("(prefers-reduced-motion: reduce)");
}
