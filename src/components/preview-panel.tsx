import { useEffect, useState } from "react";
import { IconChevronLeft as ChevronLeft } from "@tabler/icons-react";

import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { FilePreview } from "@/components/file-preview";
import {
  fetchEditPreview,
  filePreview,
  revertEdit,
  rewindEdits,
  rewindPlan,
  type RewindFailure,
  type RewindPlan,
} from "@/lib/chat-transport";
import { formatCount } from "@/lib/format";
import { useChatStore } from "@/store/chat-store";
import type { EditPreview, FileEdit } from "@/types/chat";

/**
 * 后端的结构化失败（[`RewindFailure`] 这类）不是 Error 实例，
 * `String(cause)` 出来是 "[object Object]"——那等于什么都没说
 */
function messageOf(cause: unknown) {
  if (cause instanceof Error) return cause.message;
  if (cause && typeof cause === "object" && "message" in cause) {
    return String((cause as { message: unknown }).message);
  }
  return String(cause);
}

/** 台账里的时间是秒，直接用不了 */
function timeOf(seconds: number) {
  return new Date(seconds * 1000).toLocaleTimeString("zh-CN", {
    hour: "2-digit",
    minute: "2-digit",
  });
}

/** 头部那行的时间读数（设计基准的「刚刚修改」）：先答"是不是刚改的"，
 *  再落到钟点与日期；裸钟点答不了第一个问题 */
function relativeEditLabel(seconds: number) {
  const date = new Date(seconds * 1000);
  const now = new Date();
  const minutes = Math.floor((now.getTime() - date.getTime()) / 60_000);
  if (minutes < 1) return "刚刚修改";
  if (minutes < 60) return `${minutes} 分钟前修改`;
  const sameDay =
    date.getFullYear() === now.getFullYear() &&
    date.getMonth() === now.getMonth() &&
    date.getDate() === now.getDate();
  if (sameDay) return `${timeOf(seconds)} 修改`;
  const yesterday = new Date(now);
  yesterday.setDate(now.getDate() - 1);
  const isYesterday =
    date.getFullYear() === yesterday.getFullYear() &&
    date.getMonth() === yesterday.getMonth() &&
    date.getDate() === yesterday.getDate();
  if (isYesterday) return "昨天修改";
  return `${date.getMonth() + 1}月${date.getDate()}日修改`;
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
      {/* 头部第一行只说"在看哪个文件、什么时候动的"（设计基准：todo.html · 刚刚修改）；
          行数账与回滚沉到第二行，不与文件名挤一处 */}
      <div className="flex items-center gap-1.5 pb-1">
        <button
          type="button"
          onClick={onBack}
          aria-label="返回文件清单"
          className="flex size-6 shrink-0 items-center justify-center rounded-lg text-foreground-tertiary outline-none transition-colors hover:bg-accent hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45"
        >
          <ChevronLeft className="size-3.5" />
        </button>
        <p
          className="min-w-0 flex-1 truncate text-sm font-medium text-foreground"
          title={edit.absPath}
        >
          {edit.path}
        </p>
        <span className="shrink-0 text-xs text-foreground-tertiary">
          {relativeEditLabel(edit.lastAt)}
        </span>
      </div>
      <div className="flex items-center gap-2 pb-2 pl-7">
        <Counts edit={edit} />
        {edit.writes > 1 ? (
          <span className="text-2xs text-foreground-quaternary">写了 {edit.writes} 次</span>
        ) : null}
        <span className="flex-1" />
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

/**
 * 资源管理器点进来的普通文件。与上面的编辑视图分开，是因为它**不是 aglab 写的**：
 * 没有"改了多少行""能不能回滚"这些只有台账才答得上来的问题。
 * 数据走 file_preview（工作目录内任意文件），渲染器与编辑视图共用同一个 FilePreview。
 */
function ProjectFileView({ abs, onBack }: { abs: string; onBack: () => void }) {
  const [preview, setPreview] = useState<EditPreview | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let active = true;
    setPreview(null);
    setError(null);
    filePreview(abs)
      .then((value) => active && setPreview(value))
      .catch((cause) => active && setError(cause instanceof Error ? cause.message : String(cause)));
    return () => {
      active = false;
    };
  }, [abs]);

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex items-center gap-1.5 pb-2">
        <button
          type="button"
          onClick={onBack}
          aria-label="返回文件清单"
          className="flex size-6 shrink-0 items-center justify-center rounded-lg text-foreground-tertiary outline-none transition-colors hover:bg-accent hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45"
        >
          <ChevronLeft className="size-3.5" />
        </button>
        <p className="min-w-0 flex-1 truncate text-sm font-medium text-foreground" title={abs}>
          {abs}
        </p>
        <span className="shrink-0 text-xs text-foreground-tertiary">
          {preview ? `${formatCount(preview.bytes)} 字节 · 只读预览` : "读取中…"}
        </span>
      </div>

      <div className="flex min-h-0 flex-1 flex-col">
        {error ? (
          <p className="text-xs leading-5 text-destructive">{error}</p>
        ) : !preview ? (
          <p className="text-xs text-muted-foreground">读取中…</p>
        ) : (
          <>
            <FilePreview preview={preview} path={abs} />
            {preview.clipped ? (
              <p className="mt-1 shrink-0 text-2xs leading-4 text-muted-foreground">
                正文过长，只显示前面 {formatCount(preview.content.length)} 字符（共{" "}
                {formatCount(preview.bytes)} 字节）。
              </p>
            ) : null}
            {preview.note ? (
              <p className="mt-1 shrink-0 text-2xs leading-4 text-muted-foreground">
                {preview.note}
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
                .catch((cause) => setError(cause instanceof Error ? cause.message : String(cause)))
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
 * 整条话题的全量回滚（O15-2）：**事务性**——要么全回，要么一个都不动。
 *
 * 打开时先出计划（只读）：三分类摆在用户下手之前，而预览里的那句话**就是**
 * 回滚时的拒绝原因（后端同一个判据出的），不会预览一套、执行另一套
 */
function RewindAllDialog({
  open,
  onClose,
  onDone,
}: {
  open: boolean;
  onClose: () => void;
  onDone: (note: string) => void;
}) {
  const activeId = useChatStore((s) => s.activeId);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [plan, setPlan] = useState<RewindPlan | null>(null);

  useEffect(() => {
    if (!open) return;
    let alive = true;
    setPlan(null);
    rewindPlan(activeId, { kind: "conversation" })
      .then((value) => alive && setPlan(value))
      .catch(() => alive && setPlan(null));
    return () => {
      alive = false;
    };
  }, [open, activeId]);

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next) {
          setError(null);
          onClose();
        }
      }}
    >
      <DialogContent className="w-[440px]">
        <DialogTitle>回滚本次话题的全部文件？</DialogTitle>
        <p className="mt-1 text-sm leading-6 text-muted-foreground">
          话题里被 aglab 写过的每个文件都会恢复到 aglab 第一次动它之前的样子， 中间 aglab
          写的内容全部丢掉。这件事是**要么全回、要么一个都不动**：中途失败会把已经回滚的
          撤回原样，不留半回滚的中间态。
        </p>
        {plan ? (
          <ul className="mt-3 space-y-1 text-xs leading-5">
            <li className="text-muted-foreground">会回滚 {plan.safe.length} 个文件</li>
            {plan.unsafe.length > 0 ? (
              <li className="text-destructive">
                会拒绝 {plan.unsafe.length} 个：{plan.unsafe[0].reason}
              </li>
            ) : null}
            {plan.ignored.length > 0 ? (
              <li className="text-muted-foreground">
                无关 {plan.ignored.length} 个：{plan.ignored[0].reason}
              </li>
            ) : null}
          </ul>
        ) : null}
        {error ? <p className="mt-3 text-xs leading-5 text-destructive">{error}</p> : null}
        <div className="mt-5 flex justify-end gap-2">
          <Button variant="subtle" onClick={onClose}>
            取消
          </Button>
          <Button
            disabled={busy}
            className="bg-destructive text-destructive-foreground hover:bg-destructive/90"
            onClick={() => {
              setBusy(true);
              setError(null);
              rewindEdits(activeId, { kind: "conversation" })
                .then((outcome) => {
                  onClose();
                  const skipped =
                    outcome.skipped.length > 0
                      ? `；跳过 ${outcome.skipped.length} 个（${outcome.skipped[0].reason}）`
                      : "";
                  onDone(`已回滚 ${outcome.reverted.length} 个文件${skipped}`);
                })
                .catch((cause) => {
                  // 补偿也可能失败：那几个文件停在"已经回滚、但没能撤回"的中间态上，
                  // 必须点名——只说"回滚失败"等于把用户留在现场不管
                  const failure = cause as Partial<RewindFailure> | null;
                  const stuck = failure?.stuck ?? [];
                  setError(
                    stuck.length > 0
                      ? `${messageOf(cause)}。另外这几个没能撤回，停在中间态：${stuck
                          .map((item) => item.path)
                          .join("、")}`
                      : messageOf(cause),
                  );
                })
                .finally(() => setBusy(false));
            }}
          >
            {busy ? "回滚中…" : "全部回滚"}
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
  const [rewindAllOpen, setRewindAllOpen] = useState(false);
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
          <p className="text-xs leading-5 text-destructive">读取编辑记录失败：{editsError}</p>
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

  // 有目标但不在编辑台账里 = 从资源管理器点进来的普通文件。
  // 走只读预览那条路，不摆"回滚"这类只有台账才答得上来的动作
  if (target) {
    return <ProjectFileView abs={target} onBack={() => openPreview(null)} />;
  }

  if (edits.length === 0) {
    return (
      <>
        {/* 空态居中：内容区现在占据标签页全部剩余空间，顶在左上的提示
            既浪费又被误读为“排错了” */}
        <div className="flex h-full min-h-0 flex-1 items-center justify-center">
          <p className="text-center text-xs leading-5 text-muted-foreground">
            aglab 还没有在本次话题里写过文件。
          </p>
        </div>
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
        只统计 aglab 内置的写入工具。模型经 MCP 连接器改的文件不在这里， 它没走 aglab 的写入口。
      </p>
      {note ? <p className="mt-1 text-xs text-brand-text">{note}</p> : null}

      <div className="mt-3 flex justify-end">
        <Button
          variant="ghost"
          size="sm"
          className="text-destructive hover:bg-destructive/10"
          onClick={() => setRewindAllOpen(true)}
        >
          回滚本次话题的全部文件
        </Button>
      </div>

      <RewindAllDialog
        open={rewindAllOpen}
        onClose={() => setRewindAllOpen(false)}
        onDone={(text) => {
          setNote(text);
          openPreview(null);
          void refreshEdits();
          setTimeout(() => setNote(null), 6000);
        }}
      />

      {dialog}
    </div>
  );
}
