/**
 * 懒加载的 invoke。决策层会被纯 Node 环境（vitest）import——模块求值期
 * 不能背 @tauri-apps 的运行时包袱，真正要跨 IPC 时才把 invoke 拿进来。
 * specifier 是字面量：Vite 静态解析正常打包，只有求值时机被推迟。
 */

export type TauriInvoke = (command: string, args: Record<string, unknown>) => Promise<unknown>;

let cached: TauriInvoke | null = null;

/** 首次调用才动态 import，之后命中缓存 */
export async function platformInvoke(): Promise<TauriInvoke> {
  if (!cached) {
    cached = (await import("@tauri-apps/api/core")).invoke as TauriInvoke;
  }
  return cached;
}
