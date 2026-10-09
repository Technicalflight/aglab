import { useState } from "react";
import { IconShieldQuestion as ShieldQuestion } from "@tabler/icons-react";

import { DiffStat, FileGlyph } from "@/components/tool-bits";
import { Button } from "@/components/ui/button";
import { allowToolSession, allowToolAlways } from "@/lib/chat-transport";
import {
  STATUS_TEXT,
  detailOf,
  diffStatOf,
  fileTargetOf,
  planProgressOf,
  toolLook,
} from "@/lib/tool-status";
import { RISK_LABELS, type ToolCall } from "@/types/chat";
import { useChatStore } from "@/store/chat-store";
import { cn } from "@/lib/utils";

/**
 * 工具调用的紧凑行（对齐参考设计）：动作图标 + 动词 + 目标（文件类型彩色图标 +
 * 文件名 + 目录）+ 行数差 / 进度 + 等宽细节 + 状态。点击行展开完整参数与输出；
 * 等待批准的调用例外：审批按钮必须一眼可见，不能收进行里。
 */
export function ToolCard({ call }: { call: ToolCall }) {
  const decide = useChatStore((s) => s.decide);
  const deciding = useChatStore((s) => s.deciding.includes(call.id));
  const [expanded, setExpanded] = useState(false);
  const isPending = call.status === "pending";

  const look = toolLook(call.name);
  const KindIcon = look.Icon;
  const target = fileTargetOf(call);
  const diff = diffStatOf(call);
  const progress = planProgressOf(call);
  const detail = detailOf(call);

  return (
    <div className="my-0.5">
      <button
        type="button"
        aria-expanded={expanded}
        onClick={() => setExpanded((current) => !current)}
        className={cn(
          "flex w-full items-center gap-2 rounded-md px-1.5 py-1 text-left text-sm outline-none transition-colors",
          "hover:bg-accent/50 focus-visible:ring-2 focus-visible:ring-ring/45",
        )}
      >
        <KindIcon className="size-3.5 shrink-0 text-muted-foreground" />
        <span className="shrink-0 font-medium text-foreground">{look.verb}</span>

        {target ? (
          <span className="flex min-w-0 items-center gap-1.5">
            <FileGlyph ext={target.ext} />
            <span className="truncate font-medium text-foreground">{target.name}</span>
            <span className="shrink-0 truncate font-mono text-xs text-muted-foreground/70">
              {target.dir}
            </span>
          </span>
        ) : null}

        {diff ? <DiffStat added={diff.added} removed={diff.removed} /> : null}
        {progress ? (
          <span className="shrink-0 rounded bg-muted px-1.5 text-2xs tabular-nums text-muted-foreground">
            {progress.done} / {progress.total}
          </span>
        ) : null}

        {detail ? (
          <span className="min-w-0 flex-1 truncate font-mono text-xs text-muted-foreground">
            {detail}
          </span>
        ) : (
          <span className="min-w-0 flex-1" />
        )}

        {call.risk === "high" ? (
          <span className="shrink-0 text-2xs text-destructive">{RISK_LABELS[call.risk]}</span>
        ) : null}
        {call.passReason ? (
          <span
            className="shrink-0 text-2xs text-muted-foreground"
            title={
              call.passReason.includes("自动审查")
                ? `${call.passReason}：审查模型替你拍了板，这一次没有弹窗。要改，去「设置 → 行为」关掉自动审查`
                : `${call.passReason}：这一次没有再弹窗问你。要收回它，去右栏「工具」页底部那条放行列表`
            }
          >
            · {call.passReason}
          </span>
        ) : null}
        <span
          className={cn(
            "shrink-0 text-xs",
            call.status === "failed" || call.status === "denied"
              ? "text-destructive"
              : "text-muted-foreground/70",
          )}
        >
          {STATUS_TEXT[call.status]}
        </span>
      </button>

      {expanded ? (
        <div className="ml-5 border-l border-border pl-2.5">
          <p className="whitespace-pre-wrap break-words font-mono text-xs leading-4 text-muted-foreground">
            {call.input}
          </p>
          {call.output ? (
            <p className="mt-1.5 whitespace-pre-wrap break-words text-xs leading-5 text-muted-foreground">
              {call.output}
            </p>
          ) : null}
        </div>
      ) : null}

      {isPending ? (
        <div className="ml-5 mt-1 flex items-center gap-2">
          <ShieldQuestion className="size-3.5 shrink-0 text-muted-foreground" />
          <span className="min-w-0 flex-1 truncate text-xs text-muted-foreground">
            模型请求执行上面的操作
          </span>
          <Button
            variant="subtle"
            size="sm"
            disabled={deciding}
            onClick={() => void decide(call.id, false)}
          >
            拒绝
          </Button>
          <Button
            variant="subtle"
            size="sm"
            disabled={deciding}
            onClick={() => void allowToolSession(call.id)}
          >
            本话题内允许
          </Button>
          <Button
            variant="subtle"
            size="sm"
            disabled={deciding}
            title="批准这一次，并把同一份动作写进配置：重启之后也不再问。工具页里可逐条撤销"
            onClick={() => void allowToolAlways(call.id)}
          >
            以后都允许
          </Button>
          <Button
            variant="brand"
            size="sm"
            disabled={deciding}
            onClick={() => void decide(call.id, true)}
          >
            批准
          </Button>
        </div>
      ) : null}
    </div>
  );
}
