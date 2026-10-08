import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

import { CapabilityToggle } from "@/components/ui/capability-toggle";
import { AgentDiagnosticsCard } from "@/components/agent-diagnostics";
import { setGlobalShortcut } from "@/lib/chat-transport";
import { useChatStore } from "@/store/chat-store";
import { cn } from "@/lib/utils";
import type { CloseAction } from "@/types/chat";
import { FormColumn } from "@/components/ui/content-column";

/** 开机自启。读的是 OS 里那条注册而不是配置里写着什么：
 *  "配置写着开"与"开机真的会起"是两件事，这一格只许说后者 */
function AutostartRow() {
  const [enabled, setEnabled] = useState<boolean | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let active = true;
    invoke<boolean>("autostart_state")
      .then((value) => {
        if (active) setEnabled(value);
      })
      .catch((cause) => {
        if (active) setError(cause instanceof Error ? cause.message : String(cause));
      });
    return () => {
      active = false;
    };
  }, []);

  return (
    <div className="mt-3 flex items-center justify-between gap-3 rounded-lg border border-border bg-surface px-3 py-3">
      <div className="min-w-0">
        <p className="text-sm font-medium text-foreground">开机自启</p>
        <p className="mt-1 text-xs leading-5 text-muted-foreground">
          开：登录 Windows 后 aglab 自己起来，窗口照常是开着的。关：把那条注册撤掉。
          开关读的是系统里此刻注册了没有，不是配置里写着什么。
        </p>
        {error ? <p className="mt-1 text-xs text-destructive">{error}</p> : null}
      </div>
      <CapabilityToggle
        label="开机自启"
        enabled={enabled === true}
        onToggle={() => {
          setError(null);
          invoke<boolean>("autostart_set", { enabled: enabled !== true })
            .then(setEnabled)
            .catch((cause) => setError(cause instanceof Error ? cause.message : String(cause)));
        }}
      />
    </div>
  );
}

/** 点关闭时做什么。判定在 Rust 侧的 CloseRequested 里，这里只是把意图写进配置 */
const CLOSE_ACTIONS: Array<{ value: CloseAction; label: string; desc: string }> = [
  {
    value: "ask",
    label: "每次问",
    desc: "弹三选框：最小化到托盘 / 关闭 / 取消。默认这一档",
  },
  {
    value: "tray",
    label: "收进托盘",
    desc: "窗口藏起来，不再问。正在跑的回合与定时任务接着跑，点托盘图标回来",
  },
  {
    value: "quit",
    label: "直接退出",
    desc: "整个进程结束，不再问。没跑完的回合按账本留着，下次能续",
  },
];

/**
 * 设置页的「应用」项：aglab 自己在 Windows 上怎么待着，与连到哪个模型无关。
 */
export function AppSettings() {
  const config = useChatStore((s) => s.config);
  const updateConfig = useChatStore((s) => s.updateConfig);
  // 注册失败的原因只在这一格说：全局热键被占是常态，报错不能挡住整页
  const [shortcutError, setShortcutError] = useState<string | null>(null);

  return (
    <FormColumn>
      <h1 className="text-2xl font-semibold tracking-tight text-foreground">应用</h1>
      <p className="mt-1 text-sm leading-6 text-muted-foreground">
        这个程序在这台电脑上的行为。不涉及服务商、模型和密钥。
      </p>

      <AutostartRow />

      <div className="mt-8">
        <h2 className="text-lg font-semibold tracking-tight text-foreground">Agent 子进程</h2>
        <AgentDiagnosticsCard />
      </div>

      <div className="mt-8">
        <h2 className="text-lg font-semibold tracking-tight text-foreground">关闭行为</h2>
        <p className="mt-1 text-sm leading-6 text-muted-foreground">
          点标题栏的关闭按钮（或 Alt+F4）时做什么。选了「收进托盘」或「直接退出」就不再弹三选框。
        </p>
        <div className="mt-3 grid grid-cols-3 gap-3">
          {CLOSE_ACTIONS.map((action) => {
            const active = config.closeAction === action.value;
            return (
              <button
                key={action.value}
                type="button"
                aria-pressed={active}
                onClick={() => void updateConfig({ closeAction: action.value })}
                className={cn(
                  "rounded-xl border p-3 text-left outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
                  active
                    ? "border-brand ring-1 ring-brand/50"
                    : "border-border hover:border-brand/40",
                )}
              >
                <p
                  className={cn(
                    "text-sm",
                    active ? "font-medium text-foreground" : "text-muted-foreground",
                  )}
                >
                  {action.label}
                </p>
                <p className="mt-1 text-xs leading-5 text-muted-foreground/85">
                  {action.desc}
                </p>
              </button>
            );
          })}
        </div>
        <p className="mt-2 text-xs leading-5 text-muted-foreground">
          收进托盘只在托盘可用时生效：托盘没建起来时会退回「每次问」，不会把窗口藏进没有入口的地方。
        </p>
      </div>

      <div className="mt-8">
        <h2 className="text-lg font-semibold tracking-tight text-foreground">窗口</h2>
        <div className="mt-3 flex items-center justify-between gap-3 rounded-lg border border-border bg-surface px-3 py-3">
          <div className="min-w-0">
            <p className="text-sm font-medium text-foreground">窗口置顶</p>
            <p className="mt-1 text-xs leading-5 text-muted-foreground">
              把 aglab 钉在最前，不被别的窗口盖住。改动即时生效，重启后保持。
            </p>
          </div>
          <CapabilityToggle
            label="窗口置顶"
            enabled={config.alwaysOnTop}
            onToggle={() => void updateConfig({ alwaysOnTop: !config.alwaysOnTop })}
          />
        </div>
        <div className="mt-3 flex items-center justify-between gap-3 rounded-lg border border-border bg-surface px-3 py-3">
          <div className="min-w-0">
            <p className="text-sm font-medium text-foreground">系统通知</p>
            <p className="mt-1 text-xs leading-5 text-muted-foreground">
              Windows 通知：有操作等你批准、定时任务跑完（或停在审批）、目标停下时弹一条。
              收进托盘时这是唯一的动静。窗口在前台时不打扰。
            </p>
          </div>
          <CapabilityToggle
            label="系统通知"
            enabled={config.notifications}
            onToggle={() => void updateConfig({ notifications: !config.notifications })}
          />
        </div>
        <div className="mt-3 flex items-center justify-between gap-3 rounded-lg border border-border bg-surface px-3 py-3">
          <div className="min-w-0">
            <p className="text-sm font-medium text-foreground">全局快捷键唤起</p>
            <p className="mt-1 text-xs leading-5 text-muted-foreground">
              任何应用在前台时按 <span className="font-mono">Ctrl+Shift+G</span> 把 aglab
              喊回来。默认关：全局热键会占用系统按键。别的程序先占了会注册失败，开关不会亮。
            </p>
          </div>
          <CapabilityToggle
            label="全局快捷键唤起"
            enabled={config.globalShortcutEnabled}
            onToggle={() => {
              setShortcutError(null);
              setGlobalShortcut(!config.globalShortcutEnabled)
                .then(() => void updateConfig({ globalShortcutEnabled: !config.globalShortcutEnabled }))
                .catch((cause) =>
                  setShortcutError(cause instanceof Error ? cause.message : String(cause)),
                );
            }}
          />
        </div>
        {shortcutError ? (
          <p className="mt-2 text-xs text-destructive">{shortcutError}</p>
        ) : null}
      </div>
    </FormColumn>
  );
}
