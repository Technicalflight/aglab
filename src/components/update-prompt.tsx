import { useEffect, useRef, useState } from "react";
import { Channel, invoke } from "@tauri-apps/api/core";

import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { useChatStore } from "@/store/chat-store";

interface UpdateStatus {
  currentVersion: string;
  latestVersion: string | null;
  notes: string | null;
  hasUpdate: boolean;
}

type UpdateProgress =
  | { kind: "downloading"; received: number; total: number | null }
  | { kind: "installing" }
  | { kind: "done" };

/** 自动检查的节奏：24 小时一次。轮询器每半小时醒一次看有没有到点 */
const CHECK_INTERVAL_MS = 24 * 60 * 60 * 1000;
const TICK_MS = 30 * 60 * 1000;

/**
 * 自动更新提醒：到点（24 小时）静默查一次，发现新版本弹窗推给用户。
 * 与「关于」弹窗里的手动检查同走一条后端通道（update_check / update_install）。
 * 自动检查失败一律静默——弹窗是提醒，不该变成打扰；下一次到点再试
 */
export function UpdatePrompt() {
  const configLoaded = useChatStore((s) => s.configLoaded);
  const autoUpdateCheck = useChatStore((s) => s.config.autoUpdateCheck);
  const [info, setInfo] = useState<{ version: string; notes: string | null } | null>(null);
  const [progress, setProgress] = useState<
    { received: number; total: number | null } | "installing" | null
  >(null);
  const busy = useRef(false);

  useEffect(() => {
    if (!configLoaded || !autoUpdateCheck) return;
    let cancelled = false;

    const run = async () => {
      if (busy.current) return;
      busy.current = true;
      try {
        const status = await invoke<UpdateStatus>("update_check");
        if (!cancelled && status.hasUpdate && status.latestVersion) {
          setInfo({ version: status.latestVersion, notes: status.notes });
        }
      } catch {
        // 自动检查失败不出声：弹错了比不弹更烦，下一次到点自然重试
      } finally {
        // 无论成败都记时刻：查不到不该变成每半小时打一次后端
        void useChatStore.getState().updateConfig({ lastUpdateCheckAt: Date.now() });
        busy.current = false;
      }
    };

    const due = () =>
      Date.now() - (useChatStore.getState().config.lastUpdateCheckAt || 0) >= CHECK_INTERVAL_MS;

    if (due()) void run();
    const timer = window.setInterval(() => {
      if (due()) void run();
    }, TICK_MS);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [configLoaded, autoUpdateCheck]);

  const installNow = () => {
    const channel = new Channel<UpdateProgress>();
    channel.onmessage = (progress) => {
      if (progress.kind === "downloading") {
        setProgress({ received: progress.received, total: progress.total });
      } else {
        setProgress("installing");
      }
    };
    setProgress({ received: 0, total: null });
    void invoke("update_install", { onEvent: channel }).then(
      () => setProgress("installing"),
      () => setProgress(null),
    );
  };

  if (!info) return null;

  return (
    <Dialog open onOpenChange={(next) => !next && setInfo(null)}>
      <DialogContent className="w-[460px] max-w-[92vw]">
        <DialogTitle>发现新版本 v{info.version}</DialogTitle>
        {progress === "installing" ? (
          <p className="mt-2 text-sm leading-6 text-muted-foreground">
            安装完成，应用即将自动重启…
          </p>
        ) : progress ? (
          <div className="mt-3">
            <div className="h-1.5 w-full overflow-hidden rounded-full bg-surface">
              <div
                className="h-full rounded-full bg-brand transition-[width]"
                style={{
                  width: progress.total
                    ? `${Math.min(100, Math.round((progress.received / progress.total) * 100))}%`
                    : "40%",
                }}
              />
            </div>
            <p className="mt-1.5 text-xs text-muted-foreground">
              正在下载更新
              {progress.total
                ? `（${Math.round(progress.received / 1024)} / ${Math.round(progress.total / 1024)} KB）`
                : ""}
              …
            </p>
          </div>
        ) : (
          <>
            {info.notes ? (
              <p className="mt-2 max-h-40 overflow-y-auto whitespace-pre-wrap text-sm leading-6 text-muted-foreground">
                {info.notes.length > 600 ? `${info.notes.slice(0, 600)}…` : info.notes}
              </p>
            ) : null}
            <div className="mt-4 flex items-center justify-end gap-2">
              <Button variant="subtle" size="sm" onClick={() => setInfo(null)}>
                稍后再说
              </Button>
              <Button size="sm" onClick={installNow}>
                立即更新
              </Button>
            </div>
            <p className="mt-2 text-right text-2xs leading-4 text-muted-foreground">
              安装完成后应用会自动重启
            </p>
          </>
        )}
      </DialogContent>
    </Dialog>
  );
}
