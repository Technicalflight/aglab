/**
 * 决策层在界面上的那一份状态：当前装配的系统、sidecar 的健康与进程号、
 * 最近一次运维动作的失败原因。
 *
 * 为什么单独一个 store 而不是塞进 chat-store：决策层的配置不住在 config.json，
 * 它住 localStorage，生命周期也和 chat-store 无关（改一次配置就换一次实例）。
 * 混在一起只会让"这个字段该谁负责"变成猜谜。
 */
import { create } from "zustand";

import {
  applyDecisionConfig,
  getDecisionSystem,
  refreshJevKeyringState,
  resetDecisionSystem,
  startSidecar,
  stopSidecar,
} from "@/lib/decision";
import type { DecisionSystem, LayaHealth } from "@/lib/decision";
import type { DecisionLayerConfig } from "@/lib/decision/config";

export interface DecisionState {
  /** 当前运行的那套系统。配置一改它就是新实例——面板靠这个引用变化重新挂载订阅 */
  system: DecisionSystem;
  /** sidecar 的三态；null = 还没探过 */
  health: LayaHealth | null;
  probing: boolean;
  /** aglab 亲手拉起的那个进程的 pid。用户自己在终端里跑的那份没有 pid，也不归我们停 */
  pid: number | null;
  busy: boolean;
  /** 最近一次动作失败的原因。显示一次说清一次，不攒日志 */
  note: string | null;
  /** 探一次 /health。系统被换掉时在途结果作废，不让旧读数盖住新配置 */
  probe: () => Promise<void>;
  /** 改一格配置：写进存储、换掉系统实例。返回后 system 已经是新的 */
  patch: (mutate: (draft: DecisionLayerConfig) => void) => void;
  /** 从头再读一遍存储（用户在 devtools 里手改过配置时用得上） */
  reload: () => void;
  start: (dir: string) => Promise<void>;
  stop: () => Promise<void>;
  clearNote: () => void;
}

export const useDecisionStore = create<DecisionState>((set, get) => ({
  // 读单例而不是 applyDecisionConfig：建 store 不该顺手把一份配置写进存储——
  // 那样"用户到底改过没有"这个问题就再也答不了了
  system: getDecisionSystem(),
  health: null,
  probing: false,
  pid: null,
  busy: false,
  note: null,

  probe: async () => {
    const system = get().system;
    set({ probing: true });
    const health = await system.laya.health();
    if (get().system !== system) return; // 探的过程里换了配置：这一份读数属于旧系统
    set({ health, probing: false });
  },

  patch: (mutate) => {
    const draft = structuredClone(get().system.config);
    mutate(draft);
    set({ system: applyDecisionConfig(draft), note: null });
  },

  reload: () => {
    resetDecisionSystem();
    set({ system: getDecisionSystem(), note: null });
    void get().probe();
    void refreshJevKeyringState();
  },

  start: async (dir) => {
    const { system } = get();
    set({ busy: true, note: null });
    try {
      const pid = await startSidecar({
        dir,
        endpoint: system.config.laya.sidecarEndpoint,
        subfolder: system.config.laya.subfolder,
      });
      set({ pid });
      await get().probe();
      // 起了但没起来是这里最常见的形状：进程在、端口没听（或者权重还在路上）
      if (!get().health?.ok) {
        set({
          note: `已经拉起进程 ${pid}，但 ${system.config.laya.sidecarEndpoint} 还没有回应——首次运行要下 1.7GB 权重，看一眼它自己的终端`,
        });
      }
    } catch (error) {
      set({ note: error instanceof Error ? error.message : String(error) });
    } finally {
      set({ busy: false });
    }
  },

  stop: async () => {
    set({ busy: true, note: null });
    try {
      const reaped = await stopSidecar();
      set({
        pid: null,
        note: reaped
          ? null
          : "aglab 手上没有这个进程：那个是用户自己在终端里跑的，去它自己的窗口 Ctrl+C",
      });
      await get().probe();
    } catch (error) {
      set({ note: error instanceof Error ? error.message : String(error) });
    } finally {
      set({ busy: false });
    }
  },

  clearNote: () => set({ note: null }),
}));
