import { useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import {
  IconFolder as Folder,
  IconFolderPlus as FolderPlus,
  IconX as X,
} from "@tabler/icons-react";

import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { useChatStore } from "@/store/chat-store";

const inputClass =
  "h-9 w-full rounded-lg border border-input bg-background px-3 text-base text-foreground outline-none transition-colors focus-visible:border-brand/50 focus-visible:ring-2 focus-visible:ring-ring/35";

export function ProjectDialog({
  open: isOpen,
  onOpenChange,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const createProject = useChatStore((s) => s.createProject);
  const [name, setName] = useState("");
  const [path, setPath] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function pickFolder() {
    setError(null);
    try {
      const picked = await open({ directory: true, multiple: false, title: "选择源文件夹" });
      if (typeof picked !== "string") return;
      setPath(picked);
      if (!name.trim()) {
        setName(picked.split(/[\\/]/).filter(Boolean).pop() ?? "");
      }
    } catch {
      setError("当前环境没有系统文件夹选择器，请直接填写已有目录。");
    }
  }

  async function submit() {
    setBusy(true);
    const failure = await createProject(name, path);
    setBusy(false);
    if (failure) {
      setError(failure);
      return;
    }
    setName("");
    setPath("");
    onOpenChange(false);
  }

  return (
    <Dialog open={isOpen} onOpenChange={onOpenChange}>
      <DialogContent>
        <div className="flex items-start justify-between">
          <DialogTitle>新建工作目录</DialogTitle>
          <button
            type="button"
            aria-label="关闭"
            onClick={() => onOpenChange(false)}
            className="-mt-1 -mr-1 flex size-7 items-center justify-center rounded-lg text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
          >
            <X className="size-3.5" />
          </button>
        </div>
        <p className="mt-1 text-xs text-muted-foreground">
          工作目录决定模型能用文件工具读写哪里；没有工作目录时以你的用户主目录为基准，
          权限表照常把关（建议还是给每个项目绑一个目录，写入范围更小）。
        </p>

        <div className="mt-5 space-y-4">
          <label className="block">
            <span className="mb-1.5 block text-xs text-muted-foreground">名称</span>
            <div className="relative">
              <Folder className="pointer-events-none absolute top-1/2 left-3 size-3.5 -translate-y-1/2 text-muted-foreground" />
              <input
                type="text"
                value={name}
                placeholder="给这个工作目录起个名"
                className={`${inputClass} pl-9`}
                onChange={(event) => setName(event.target.value)}
              />
            </div>
          </label>

          <div>
            <span className="mb-1.5 block text-xs text-muted-foreground">源文件夹</span>
            <div className="flex min-h-[88px] flex-col items-center justify-center gap-2 rounded-lg border border-dashed border-input px-4 py-4">
              {path ? (
                <p className="max-w-full truncate font-mono text-sm text-foreground" title={path}>
                  {path}
                </p>
              ) : (
                <p className="text-sm text-muted-foreground">还没有选择文件夹</p>
              )}
              <Button variant="subtle" size="sm" onClick={() => void pickFolder()}>
                <FolderPlus className="size-3.5" />
                选择文件夹
              </Button>
            </div>
          </div>

          {error ? <p className="text-xs text-destructive">{error}</p> : null}
        </div>

        <div className="mt-6 flex justify-end gap-2">
          <Button variant="ghost" onClick={() => onOpenChange(false)}>
            取消
          </Button>
          <Button
            variant="brand"
            disabled={!name.trim() || !path || busy}
            onClick={() => void submit()}
          >
            创建工作目录
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}
