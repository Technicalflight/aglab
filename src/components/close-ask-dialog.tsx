import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";

/**
 * 关闭请求的三选框。请求由 Rust 拦下 `CloseRequested` 后发出来，所以标题栏的关闭按钮、
 * Alt+F4、任务栏右键关闭走的是同一条路——"关闭"在界面上只有一个语义入口，
 * 不会出现"按钮会问、键盘不问"的两套语义
 */
export function CloseAskDialog() {
  const [open, setOpen] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let active = true;
    let stop: (() => void) | null = null;
    void listen("window-close-asked", () => {
      if (active) {
        setError(null);
        setOpen(true);
      }
    }).then((fn) => {
      if (active) stop = fn;
      else fn();
    });
    return () => {
      active = false;
      stop?.();
    };
  }, []);

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        // 遮罩与 Esc 的关闭都等于"当作没点"：这一框没有第四种意图
        if (!next) setOpen(false);
      }}
    >
      <DialogContent>
        <DialogTitle>关闭 aglab？</DialogTitle>
        <p className="mt-2 text-sm leading-6 text-muted-foreground">
          最小化到托盘：窗口藏起来，正在跑的回合与定时任务接着跑，点托盘图标回来。
          关闭：整个进程退出，没跑完的回合按账本留着，下次能续。
        </p>
        {error ? <p className="mt-2 text-xs leading-5 text-destructive">{error}</p> : null}
        <div className="mt-4 flex justify-end gap-2">
          <Button variant="ghost" size="sm" onClick={() => setOpen(false)}>
            取消
          </Button>
          <Button
            variant="subtle"
            size="sm"
            onClick={() => {
              setError(null);
              // 只在真的藏成了才收掉这一框：藏不成就把原因留在原地，「关闭」还挨着它
              invoke("window_hide_to_tray")
                .then(() => setOpen(false))
                .catch((cause) => setError(cause instanceof Error ? cause.message : String(cause)));
            }}
          >
            最小化到托盘
          </Button>
          <Button
            size="sm"
            className="bg-destructive text-destructive-foreground hover:bg-destructive"
            onClick={() => void invoke("window_quit")}
          >
            关闭
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}
