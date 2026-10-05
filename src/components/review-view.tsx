import { useCallback, useEffect, useState } from "react";
import { IconRefresh as RefreshCw } from "@tabler/icons-react";

import { DiffBody } from "@/components/diff-view";
import { Button } from "@/components/ui/button";
import { SectionFrame } from "@/components/section-frame";
import { draftReview, fetchFileDiff, fetchReview, saveReview } from "@/lib/chat-transport";
import {
  getDraftCache,
  getReviewCache,
  setDraftCache,
  setReviewCache,
} from "@/lib/review-cache";
import { useChatStore } from "@/store/chat-store";
import type { DiffLayout, DiffScope, FileDiff, ReviewFile, ReviewInfo } from "@/types/chat";

const STATE_LABELS: Record<string, string> = {
  A: "新增",
  M: "修改",
  D: "删除",
  R: "重命名",
  C: "复制",
  T: "类型变化",
  U: "冲突",
  "?": "未跟踪",
  B: "二进制",
};

const SCOPE_LABELS: Record<DiffScope, string> = {
  working: "未提交",
  committed: "相对基线",
};

function message(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}

/** 一个文件默认看哪一侧：还没提交的那批才是此刻要写进说明里的东西 */
function defaultScope(file: ReviewFile): DiffScope {
  return file.working ? "working" : "committed";
}

function diffKey(path: string, scope: DiffScope) {
  return `${path}|${scope}`;
}

/**
 * 一个文件的差异面板。取数与缓存都在父组件，这里只管三种显示态
 */
function FileDiffPanel({
  layout,
  diff,
  error,
  onRetry,
}: {
  layout: DiffLayout;
  diff: FileDiff | undefined;
  error: string | undefined;
  onRetry: () => void;
}) {
  if (error) {
    return (
      <button
        type="button"
        onClick={onRetry}
        className="w-full px-3 py-2 text-left text-xs leading-5 text-destructive outline-none hover:bg-accent focus-visible:ring-2 focus-visible:ring-ring/45"
      >
        {error}（点此重试）
      </button>
    );
  }
  if (!diff) {
    return <p className="px-3 py-2 text-xs text-muted-foreground">读取差异…</p>;
  }
  return <DiffBody diff={diff} layout={layout} />;
}

export function ReviewView() {
  // 初值直接吃缓存：进页即画。只有手里毫无缓存（启动后没预热完就点进来）才真的走读取态
  const [info, setInfo] = useState<ReviewInfo | null>(getReviewCache());
  const [loading, setLoading] = useState(getReviewCache() === null);
  const [error, setError] = useState<string | null>(null);
  const [draft, setDraft] = useState(() => getDraftCache() ?? "");
  const [drafting, setDrafting] = useState(false);
  const [savedTo, setSavedTo] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

  // 展开状态就是"这个文件此刻看哪一侧"，所以一个 map 同时表达了开合与范围
  const [expanded, setExpanded] = useState<Record<string, DiffScope>>({});
  const [diffs, setDiffs] = useState<Record<string, FileDiff>>({});
  const [diffErrors, setDiffErrors] = useState<Record<string, string>>({});
  const [pending, setPending] = useState<Record<string, boolean>>({});

  const layout = useChatStore((s) => s.config.ui.diffLayout);
  const setUi = useChatStore((s) => s.setUi);

  const reload = useCallback(async () => {
    setLoading(true);
    setError(null);
    // 重新读取之后，之前展开看到的差异就已经是旧的了——留着它比不显示更坏
    setExpanded({});
    setDiffs({});
    setDiffErrors({});
    try {
      const fresh = await fetchReview();
      setReviewCache(fresh);
      setInfo(fresh);
    } catch (cause) {
      setInfo(null);
      setError(message(cause));
    } finally {
      setLoading(false);
    }
  }, []);

  // 挂载策略：毫无缓存（预热没赶上）走 reload 的正路（读取态 + 报错）；
  // 有缓存就进页即画，IPC 挪到后台静默对账——不闪读取态、不动已展开的差异。
  // reload 无依赖、身份稳定，本效果每次挂载只跑一次
  useEffect(() => {
    if (getReviewCache() === null) {
      void reload();
      return;
    }
    let alive = true;
    fetchReview()
      .then((fresh) => {
        setReviewCache(fresh);
        if (alive) {
          setInfo(fresh);
          setError(null);
        }
      })
      .catch(() => undefined); // 静默守着缓存，报错留给「重新读取」
    return () => {
      alive = false;
    };
  }, [reload]);

  // 草稿同步进会话级缓存：切页/切分区不丢稿
  useEffect(() => {
    setDraftCache(draft);
  }, [draft]);

  const wanted = Object.entries(expanded).map(([path, scope]) => diffKey(path, scope));
  const signature = wanted.join("\n");

  useEffect(() => {
    for (const key of signature ? signature.split("\n") : []) {
      // 路径里可能有 '|'，从最后一个分隔符切
      const sep = key.lastIndexOf("|");
      const path = key.slice(0, sep);
      const scope = key.slice(sep + 1) as DiffScope;
      if (diffs[key] || pending[key] || diffErrors[key]) continue;
      setPending((prev) => ({ ...prev, [key]: true }));
      fetchFileDiff(path, scope)
        .then((value) => setDiffs((prev) => ({ ...prev, [key]: value })))
        .catch((cause) => setDiffErrors((prev) => ({ ...prev, [key]: message(cause) })))
        .finally(() => setPending((prev) => ({ ...prev, [key]: false })));
    }
  }, [signature, diffs, pending, diffErrors]);

  async function onDraft() {
    setDrafting(true);
    setError(null);
    setSavedTo(null);
    try {
      setDraft((await draftReview()).trim());
    } catch (cause) {
      setError(message(cause));
    } finally {
      setDrafting(false);
    }
  }

  async function onCopy() {
    try {
      await navigator.clipboard.writeText(draft);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    } catch (cause) {
      setError(`复制失败，下方文本可以直接选中：${message(cause)}`);
    }
  }

  async function onSave() {
    setError(null);
    try {
      setSavedTo(await saveReview(draft));
    } catch (cause) {
      setError(message(cause));
    }
  }

  const workingCount = info?.files.filter((file) => file.working).length ?? 0;
  const committedCount = info?.files.filter((file) => file.committed).length ?? 0;

  return (
    <SectionFrame
      title="变更请求"
      note={info ? `${info.branch} · ${info.head}` : "读取工作目录的 git 状态"}
      actions={
        <Button
          variant="ghost"
          size="sm"
          aria-label="重新读取 git 状态"
          disabled={loading}
          onClick={() => void reload()}
        >
          <RefreshCw className={loading ? "size-3.5 animate-spin" : "size-3.5"} />
          <span>重新读取</span>
        </Button>
      }
    >
      {!info && !loading ? (
        <p className="text-sm leading-6 text-muted-foreground">
          {error ?? "还没有可读取的仓库。绑定一个 git 工作目录后，这里会列出分支、提交和改动文件。"}
        </p>
      ) : null}

      {info ? (
        <>
          <div className="flex flex-wrap items-baseline gap-x-5 gap-y-1 text-xs text-muted-foreground">
            <span>
              基线 <span className="font-mono text-foreground">{info.base || "无可比对象"}</span>
            </span>
            <span>
              领先 <span className="font-mono text-foreground">{info.ahead}</span> · 落后{" "}
              <span className="font-mono text-foreground">{info.behind}</span>
            </span>
            <span>
              未提交 <span className="font-mono text-foreground">{workingCount}</span> · 相对基线{" "}
              <span className="font-mono text-foreground">{committedCount}</span>
            </span>
            <span className="break-all font-mono">{info.root}</span>
          </div>

          <h2 className="mt-5 text-xs font-medium tracking-[0.08em] text-foreground-tertiary uppercase">
            提交
          </h2>
          {info.commits.length === 0 ? (
            <p className="mt-1.5 text-sm text-muted-foreground">基线之上没有提交。</p>
          ) : (
            <ul className="mt-2 space-y-1">
              {info.commits.map((commit) => (
                <li key={commit.sha} className="flex items-baseline gap-2 text-sm">
                  <span className="shrink-0 font-mono text-xs text-muted-foreground">
                    {commit.sha}
                  </span>
                  <span className="min-w-0 truncate text-foreground">{commit.subject}</span>
                </li>
              ))}
            </ul>
          )}

          <div className="mt-5 flex items-baseline justify-between gap-3">
            <h2 className="text-xs font-medium tracking-[0.08em] text-foreground-tertiary uppercase">
              改动文件
            </h2>
            <div className="flex items-center gap-1 text-xs">
              {(["unified", "split"] as const).map((value) => (
                <button
                  key={value}
                  type="button"
                  onClick={() => setUi({ diffLayout: value })}
                  aria-pressed={layout === value}
                  className={`rounded-lg px-2 py-1 outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45 ${
                    layout === value
                      ? "bg-surface text-foreground"
                      : "text-muted-foreground hover:text-foreground"
                  }`}
                >
                  {value === "unified" ? "单栏" : "双栏"}
                </button>
              ))}
            </div>
          </div>

          {info.files.length === 0 ? (
            <p className="mt-1.5 text-sm text-muted-foreground">
              {info.base ? "与基线没有差异，工作目录也是干净的。" : "没有可比较的基线，也没有改动。"}
            </p>
          ) : (
            <ul className="mt-2 divide-y divide-border overflow-hidden rounded-lg border border-border bg-surface">
              {info.files.map((file) => {
                const open = file.path in expanded;
                const scope = expanded[file.path] ?? defaultScope(file);
                const side = (scope === "working" ? file.working : file.committed) ?? null;
                const both = Boolean(file.working && file.committed);
                const key = diffKey(file.path, scope);

                return (
                  <li key={file.path}>
                    <button
                      type="button"
                      aria-expanded={open}
                      onClick={() =>
                        setExpanded((prev) => {
                          const next = { ...prev };
                          if (file.path in next) delete next[file.path];
                          else next[file.path] = defaultScope(file);
                          return next;
                        })
                      }
                      className="flex w-full items-baseline gap-2 px-3 py-2 text-left outline-none transition-colors hover:bg-accent focus-visible:ring-2 focus-visible:ring-ring/45"
                    >
                      <span className="w-14 shrink-0 text-xs text-muted-foreground">
                        {STATE_LABELS[side?.state ?? ""] ?? side?.state ?? "—"}
                      </span>
                      <span className="min-w-0 flex-1 truncate font-mono text-foreground">
                        {file.oldPath ? (
                          <span className="text-muted-foreground">{file.oldPath} → </span>
                        ) : null}
                        {file.path}
                      </span>
                      {both ? (
                        <span className="shrink-0 rounded-lg border border-border px-1.5 text-2xs text-muted-foreground">
                          {SCOPE_LABELS[scope]}
                        </span>
                      ) : null}
                      <span className="shrink-0 font-mono text-xs text-muted-foreground">
                        +{side?.additions ?? 0} -{side?.deletions ?? 0}
                      </span>
                      <span className="w-3 shrink-0 text-2xs text-muted-foreground">
                        {open ? "▾" : "▸"}
                      </span>
                    </button>

                    {open ? (
                      <div className="border-t border-border bg-background">
                        {both ? (
                          <div className="flex items-center gap-1 px-3 py-1.5 text-xs">
                            {(["working", "committed"] as const).map((value) => (
                              <button
                                key={value}
                                type="button"
                                onClick={() =>
                                  setExpanded((prev) => ({ ...prev, [file.path]: value }))
                                }
                                aria-pressed={scope === value}
                                className={`rounded-lg px-2 py-0.5 outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45 ${
                                  scope === value
                                    ? "bg-surface text-foreground"
                                    : "text-muted-foreground hover:text-foreground"
                                }`}
                              >
                                {SCOPE_LABELS[value]}
                              </button>
                            ))}
                          </div>
                        ) : null}
                        <FileDiffPanel
                          layout={layout}
                          diff={diffs[key]}
                          error={diffErrors[key]}
                          onRetry={() =>
                            setDiffErrors((prev) => {
                              const next = { ...prev };
                              delete next[key];
                              return next;
                            })
                          }
                        />
                      </div>
                    ) : null}
                  </li>
                );
              })}
            </ul>
          )}

          {info.untrackedOmitted > 0 ? (
            <p className="mt-2 text-xs leading-5 text-muted-foreground">
              未跟踪文件过多，另有 {info.untrackedOmitted} 个没有列出。
            </p>
          ) : null}

          <div className="mt-5 flex flex-wrap items-center gap-2 border-t border-border pt-4">
            <Button variant="brand" size="sm" disabled={drafting} onClick={() => void onDraft()}>
              {drafting ? "起草中" : "起草说明"}
            </Button>
            <Button variant="subtle" size="sm" disabled={!draft || copied} onClick={() => void onCopy()}>
              {copied ? "已复制" : "复制"}
            </Button>
            <Button
              variant="ghost"
              size="sm"
              disabled={!draft}
              onClick={() => void onSave()}
            >
              写入 .aglab/change-request.md
            </Button>
            <span className="text-xs text-muted-foreground">
              {info.patchTruncated
                ? `差异较大，只把前面 ${info.patchLines} 行交给模型。`
                : `${info.patchLines} 行差异交给模型。`}
            </span>
          </div>

          {savedTo ? (
            <p className="mt-2 text-xs break-all text-brand-text">已写入 {savedTo}</p>
          ) : null}
          {error ? <p className="mt-2 text-xs leading-5 text-destructive">{error}</p> : null}

          {draft ? (
            <pre className="mt-3 rounded-lg border border-border bg-elevated p-3 text-sm leading-6 whitespace-pre-wrap text-foreground">
              {draft}
            </pre>
          ) : null}
        </>
      ) : null}
    </SectionFrame>
  );
}
