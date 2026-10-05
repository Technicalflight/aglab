import type { DiffHunk, DiffLine, FileDiff } from "@/types/chat";

const GUTTER = "w-11 shrink-0 select-none text-right tabular-nums";

function kindClasses(kind: DiffLine["kind"]) {
  if (kind === "added") return "bg-diff-added/12 border-l-2 border-l-diff-added";
  if (kind === "removed") return "bg-diff-removed/12 border-l-2 border-l-diff-removed";
  return "border-l-2 border-l-transparent";
}

function kindText(kind: DiffLine["kind"]) {
  if (kind === "added") return "text-diff-added";
  if (kind === "removed") return "text-diff-removed";
  return "text-muted-foreground";
}

function Text({ line }: { line: DiffLine }) {
  return (
    <span className="min-w-0 flex-1 whitespace-pre-wrap break-words text-foreground">
      {line.text === "" ? " " : line.text}
    </span>
  );
}

function HunkHeader({ header }: { header: string }) {
  return (
    <div className="border-y border-border bg-muted/60 px-3 py-1 font-mono text-xs text-muted-foreground">
      {header}
    </div>
  );
}

function UnifiedHunk({ hunk }: { hunk: DiffHunk }) {
  return (
    <div>
      <HunkHeader header={hunk.header} />
      {hunk.lines.map((line, index) => (
        <div
          key={index}
          className={`flex items-baseline gap-2 py-px pl-1 pr-3 font-mono text-sm leading-6 ${kindClasses(line.kind)}`}
        >
          <span className={`${GUTTER} text-xs text-muted-foreground/70`}>
            {line.oldNo ?? ""}
          </span>
          <span className={`${GUTTER} text-xs text-muted-foreground/70`}>
            {line.newNo ?? ""}
          </span>
          <span className={`w-2 shrink-0 select-none ${kindText(line.kind)}`}>
            {line.kind === "added" ? "+" : line.kind === "removed" ? "-" : " "}
          </span>
          <Text line={line} />
        </div>
      ))}
    </div>
  );
}

/**
 * 双栏要把成片的删除行和成片的新增行配回同一水平线。
 * git 在一个块内总是先列完删除再列新增，所以按"连续段"配对就是对的
 */
function splitRows(hunk: DiffHunk) {
  const rows: { left: DiffLine | null; right: DiffLine | null }[] = [];
  const lines = hunk.lines;
  let cursor = 0;

  while (cursor < lines.length) {
    const line = lines[cursor];
    if (line.kind === "context") {
      rows.push({ left: line, right: line });
      cursor += 1;
      continue;
    }

    const removed: DiffLine[] = [];
    const added: DiffLine[] = [];
    while (cursor < lines.length && lines[cursor].kind === "removed") {
      removed.push(lines[cursor]);
      cursor += 1;
    }
    while (cursor < lines.length && lines[cursor].kind === "added") {
      added.push(lines[cursor]);
      cursor += 1;
    }
    const span = Math.max(removed.length, added.length);
    for (let offset = 0; offset < span; offset += 1) {
      rows.push({ left: removed[offset] ?? null, right: added[offset] ?? null });
    }
  }

  return rows;
}

function SplitCell({ line, side }: { line: DiffLine | null; side: "old" | "new" }) {
  if (!line) {
    // 补空的那半边：给一层极淡的底，让"这里没有行"和"这里是空行"看得出区别
    return <div className="min-w-0 flex-1 border-l border-border bg-muted/40" />;
  }
  return (
    <div
      className={`flex min-w-0 flex-1 items-baseline gap-2 py-px pl-1 pr-2 font-mono text-sm leading-6 ${kindClasses(line.kind)}`}
    >
      <span className={`${GUTTER} text-xs text-muted-foreground/70`}>
        {side === "old" ? (line.oldNo ?? "") : (line.newNo ?? "")}
      </span>
      <Text line={line} />
    </div>
  );
}

function SplitHunk({ hunk }: { hunk: DiffHunk }) {
  return (
    <div>
      <HunkHeader header={hunk.header} />
      {splitRows(hunk).map((row, index) => (
        <div key={index} className="flex">
          <SplitCell line={row.left} side="old" />
          <SplitCell line={row.right} side="new" />
        </div>
      ))}
    </div>
  );
}

/** 单个文件的差异。行号与配对都在 Rust 侧算清，这里只负责排 */
export function DiffBody({ diff, layout }: { diff: FileDiff; layout: "unified" | "split" }) {
  if (diff.binary) {
    return (
      <p className="px-3 py-2 text-xs leading-5 text-muted-foreground">
        二进制文件，没有文本差异可显示。
      </p>
    );
  }
  if (diff.hunks.length === 0) {
    return (
      <p className="px-3 py-2 text-xs leading-5 text-muted-foreground">
        这一侧没有文本改动——纯重命名或只有模式等元数据变化时就是这样。
      </p>
    );
  }

  return (
    <div className="overflow-hidden border-t border-border">
      {diff.oldPath ? (
        <p className="px-3 py-1 font-mono text-xs break-all text-muted-foreground">
          {diff.oldPath} → {diff.path}
        </p>
      ) : null}
      {diff.hunks.map((hunk, index) =>
        layout === "split" ? (
          <SplitHunk key={index} hunk={hunk} />
        ) : (
          <UnifiedHunk key={index} hunk={hunk} />
        ),
      )}
      {diff.truncated ? (
        <p className="border-t border-border bg-muted/40 px-3 py-1.5 text-xs leading-5 text-muted-foreground">
          差异过大，后面截断了。上面这些不是全部。
        </p>
      ) : null}
    </div>
  );
}
