/**
 * 决策面板的订阅源。
 *
 * 为什么不干脆在组件里 useEffect 订：设置页每改一次配置就重装一次决策系统，
 * 新系统是新的审计实例，组件重挂时历史会被清空——而"改完阈值/开关再看链路有没有变"
 * 恰恰是这块面板的主要用法。所以历史住在这个模块级单例里，跨系统活下来：
 * 挂上新系统时先按 id 补一次种子，再收增量。
 */
import type { DecisionTrace } from "./types";
import type { DecisionSystem } from "./index";

export class DecisionMonitor {
  private items: DecisionTrace[] = [];
  /** useSyncExternalStore 要求同一份数据返回同一个引用，所以快照单独存一份 */
  private cached: DecisionTrace[] = [];
  private readonly listeners = new Set<() => void>();
  private detach: (() => void) | null = null;
  private attachedTo: DecisionSystem | null = null;

  constructor(private readonly cap = 300) {}

  /** 新的在前 */
  get snapshot(): readonly DecisionTrace[] {
    return this.cached;
  }

  /**
   * 挂到一个决策系统上：先补历史（审计实例里的 recent），再接增量。
   * 返回退订函数——组件卸载时调它，别留悬空监听。重复挂同一个系统是空操作。
   */
  attach(system: DecisionSystem): () => void {
    if (this.attachedTo === system) return () => undefined;
    this.detach?.();
    this.attachedTo = system;
    // recent() 是新的在前，而 record() 按"来一条压一条"排；补种子时要反着喂，
    // 否则整段历史被倒过来，面板上最新的决策停在最下面
    for (const trace of (system.audit?.recent(this.cap) ?? []).slice().reverse()) this.record(trace);
    const unsubscribe = system.audit?.subscribe((trace) => this.record(trace));
    this.detach = () => {
      unsubscribe?.();
      this.detach = null;
      this.attachedTo = null;
    };
    return this.detach;
  }

  subscribe(listener: () => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  /**
   * 清空。连着审计实例一起清——只清视图的话，下一次重新挂载时 recent() 又把同一批
   * 历史种回来，看起来就是"清空按钮没起作用"
   */
  clear(): void {
    this.items = [];
    this.cached = [];
    this.attachedTo?.audit?.clear();
    this.notify();
  }

  private record(trace: DecisionTrace): void {
    // 种子与增量可能撞上同一条（重挂同一个系统、或 record 前 audit 已经推过）
    if (this.items.some((item) => item.id === trace.id)) return;
    this.items.unshift(trace);
    if (this.items.length > this.cap) this.items.length = this.cap;
    this.cached = [...this.items];
    this.notify();
  }

  private notify(): void {
    for (const listener of this.listeners) {
      try {
        listener();
      } catch {
        // 订阅者是界面，界面坏了不该把决策路径拖下水（audit.ts 同一条规矩）
      }
    }
  }
}

/** 面板共用的一份历史。审计本身是进程内的环形缓冲，这份是它的跨重建投影 */
export const decisionMonitor = new DecisionMonitor();
