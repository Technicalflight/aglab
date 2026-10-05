import { useEffect, useState } from "react";
import { IconChevronLeft as ChevronLeft } from "@tabler/icons-react";

import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { FilePreview } from "@/components/file-preview";
import { fetchEditPreview, revertEdit } from "@/lib/chat-transport";
import { formatCount } from "@/lib/format";
import { useChatStore } from "@/store/chat-store";
import type { EditPreview, FileEdit } from "@/types/chat";

/** 台账里的时间是秒，直接用不了 */
function timeOf(seconds: number) {
  return new Date(seconds * 1000).toLocaleTimeString("zh-CN", {
    hour: "2-digit",
    minute: "2-digit",
  });
}

function Counts({ edit, className = "" }: { edit: FileEdit; className?: string }) {
  return (
    <span className={`font-mono text-xs ${className}`}>
      {/* "约"只管一次，写在符号前面；两边各挂一个会变成"+约 4,200 -约 3,100"那种读不通的东西 */}
      {edit.approximate ? <span className="text-muted-foreground">约 </span> : null}
      <span className="text-diff-added">+{formatCount(edit.additions)}</span>{" "}
      <span className="text-diff-removed">-{formatCount(edit.deletions)}</span>
    </span>
  );
}

/** 一个文件的预览。展开才取数：一个话题改过上百个文件时不该全量拉 */
function FileEditView({
  edit,
  onBack,
  onRevert,
}: {
  edit: FileEdit;
  onBack: () => void;
  onRevert: () => void;
}) {
  const activeId = useChatStore((s) => s.activeId);
  const [preview, setPreview] = useState<EditPreview | null>(null);
  const [error, setError] = useState<string | null>(null);

  // lastAt 进依赖：回滚会追加一条记录、只变这个字段，不带它就还看着回滚前的内容
  useEffect(() => {
    let active = true;
    setPreview(null);
    setError(null);
    fetchEditPreview(activeId, edit.absPath)
      .then((value) => active && setPreview(value))
      .catch((cause) => active && setError(cause instanceof Error ? cause.message : String(cause)));
    return () => {
      active = false;
    };
  }, [activeId, edit.absPath, edit.lastAt]);

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex items-center gap-1.5 pb-2">
        <button
          type="button"
          onClick={onBack}
          aria-label="返回文件清单"
          className="flex size-6 shrink-0 items-center justify-center rounded-lg text-muted-foreground outline-none transition-colors hover:bg-accent hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45"
        >
          <ChevronLeft className="size-3.5" />
        </button>
        <div className="min-w-0 flex-1">
          <p className="truncate font-mono text-xs text-foreground" title={edit.absPath}>
            {edit.path}
          </p>
          <p className="mt-0.5 flex items-baseline gap-2">
            <Counts edit={edit} />
            <span className="text-2xs text-muted-foreground/70">
              {edit.writes > 1 ? `写了 ${edit.writes} 次 · ` : ""}
              {timeOf(edit.lastAt)}
            </span>
          </p>
        </div>
        <Button variant="ghost" size="sm" disabled={!edit.rollbackable} onClick={onRevert}>
          回滚
        </Button>
      </div>

      {!edit.rollbackable && edit.reason ? (
        <p className="pb-2 text-2xs leading-4 text-muted-foreground">{edit.reason}</p>
      ) : null}

      <div className="flex min-h-0 flex-1 flex-col">
        {error ? (
          <p className="text-xs leading-5 text-destructive">{error}</p>
        ) : !preview ? (
          <p className="text-xs text-muted-foreground">读取中…</p>
        ) : (
          <>
            <FilePreview preview={preview} path={edit.absPath} />
            {preview.clipped ? (
              <p className="mt-1 shrink-0 text-2xs leading-4 text-muted-foreground">
                正文过长，只显示前面 {formatCount(preview.content.length)} 字符（共{" "}
                {formatCount(preview.bytes)} 字节）。
              </p>
            ) : null}
          </>
        )}
      </div>
    </div>
  );
}

function RevertDialog({
  edit,
  onClose,
  onDone,
}: {
  edit: FileEdit | null;
  onClose: () => void;
  onDone: (note: string) => void;
}) {
  const activeId = useChatStore((s) => s.activeId);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  return (
    <Dialog
      open={edit !== null}
      onOpenChange={(open) => {
        if (!open) {
          setError(null);
          onClose();
        }
      }}
    >
      <DialogContent className="w-[440px]">
        <DialogTitle>回滚 {edit?.path ?? ""}？</DialogTitle>
        <p className="mt-1 text-sm leading-6 text-muted-foreground">
          文件会恢复到 aglab 第一次动它之前的样子，中间 aglab 写的内容全部丢掉。
          {edit && edit.writes > 1 ? `这个文件本次被写了 ${edit.writes} 次。` : ""}
        </p>
        {edit?.drifted ? (
          <p className="mt-3 rounded-lg border border-destructive/30 bg-destructive/10 px-3 py-2 text-sm leading-5 text-destructive">
            aglab 之后这个文件又被改过，回滚会连带覆盖那部分改动。
          </p>
        ) : null}
        {error ? <p className="mt-3 text-xs leading-5 text-destructive">{error}</p> : null}
        <div className="mt-5 flex justify-end gap-2">
          <Button variant="subtle" onClick={onClose}>
            取消
          </Button>
          <Button
            disabled={busy || !edit}
            className="bg-destructive text-destructive-foreground hover:bg-destructive/90"
            onClick={() => {
              if (!edit) return;
              setBusy(true);
              setError(null);
              revertEdit(activeId, edit.absPath)
                .then((outcome) => {
                  onClose();
                  onDone(`已回滚 ${outcome.path}`);
                })
                .catch((cause) =>
                  setError(cause instanceof Error ? cause.message : String(cause)),
                )
                .finally(() => setBusy(false));
            }}
          >
            {busy ? "回滚中…" : "回滚这个文件"}
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}

/**
 * 右栏「预览」：aglab 本次话题改过的文件。清单点开后按类型渲染——
 * HTML 出网页、Markdown 排版、图片显示图、其余文本走行号 + 语法色
 */
export function PreviewPanel() {
  const edits = useChatStore((s) => s.edits);
  const editsError = useChatStore((s) => s.editsError);
  const refreshEdits = useChatStore((s) => s.refreshEdits);
  const target = useChatStore((s) => s.previewTarget);
  const openPreview = useChatStore((s) => s.openPreview);
  const [pendingRevert, setPendingRevert] = useState<FileEdit | null>(null);
  const [note, setNote] = useState<string | null>(null);

  const open = target ? edits.find((edit) => edit.absPath === target) : undefined;

  const dialog = (
    <RevertDialog
      edit={pendingRevert}
      onClose={() => setPendingRevert(null)}
      onDone={(text) => {
        setNote(text);
        // 回滚完回到清单：预览视图会重新取那份刚被换掉的文件，
        // 留在原处只会让人以为回滚没生效
        openPreview(null);
        void refreshEdits();
        setTimeout(() => setNote(null), 4000);
      }}
    />
  );

  if (editsError) {
    return (
      <>
        <div className="space-y-2">
          <p className="text-xs leading-5 text-destructive">
            读取编辑记录失败：{editsError}
          </p>
          <Button variant="subtle" size="sm" onClick={() => void refreshEdits()}>
            重试
          </Button>
        </div>
        {dialog}
      </>
    );
  }

  if (open) {
    return (
      <>
        <FileEditView
          edit={open}
          onBack={() => openPreview(null)}
          onRevert={() => setPendingRevert(open)}
        />
        {dialog}
      </>
    );
  }

  if (edits.length === 0) {
    return (
      <>
        <p className="text-xs leading-5 text-muted-foreground">
          aglab 还没有在本次话题里写过文件。
        </p>
        {dialog}
      </>
    );
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col overflow-y-auto">
      <p className="text-xs leading-5 text-muted-foreground">
        本次话题改过 {edits.length} 个文件。数字是每次写入相对上一次的累计，
        要看相对基线的净改动去「变更请求」页。
      </p>

      <ul className="mt-2 divide-y divide-border overflow-hidden rounded-lg border border-border bg-surface">
        {edits.map((edit) => (
          <li key={edit.absPath}>
            <button
              type="button"
              onClick={() => openPreview(edit.absPath)}
              className="flex w-full items-baseline gap-2 px-2.5 py-2 text-left outline-none transition-colors hover:bg-accent focus-visible:ring-2 focus-visible:ring-ring/45"
            >
              <span className="min-w-0 flex-1 truncate font-mono text-xs text-foreground">
                {edit.path}
              </span>
              <Counts edit={edit} className="shrink-0" />
              <span className="shrink-0 text-2xs text-muted-foreground/70">
                {timeOf(edit.lastAt)}
              </span>
            </button>
          </li>
        ))}
      </ul>

      <p className="mt-2 text-2xs leading-4 text-muted-foreground/80">
        只统计 aglab 内置的写入工具。模型经 MCP 连接器改的文件不在这里，
        它没走 aglab 的写入口。
      </p>
      {note ? <p className="mt-1 text-xs text-brand-text">{note}</p> : null}

      {dialog}
    </div>
  );
}
