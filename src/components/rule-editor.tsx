import { useState } from "react";
import { IconTrash as Trash } from "@tabler/icons-react";

import { Button } from "@/components/ui/button";
import { CapabilityToggle } from "@/components/ui/capability-toggle";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import type { FileRuleAction } from "@/types/chat";
import { ACTION_LABEL } from "@/lib/security-rules";
import { cn } from "@/lib/utils";

/**
 * 三张安全规则表共用的小件（design-security-center.md D2/D4/D5 的设置页）。
 * 只管长什么样的重复：保存一律走 updateConfig，校验在后端存盘那一道。
 */

/** 动作下拉（拒绝｜询问｜放行）。三张表的动作语义同源，连文案一起共用 */
export function ActionSelect({
  value,
  onChange,
  ariaLabel,
  allowDeny = true,
  className,
}: {
  value: FileRuleAction;
  onChange: (next: FileRuleAction) => void;
  ariaLabel: string;
  /** 命令前缀规则没有「拒绝」——拒绝的语义由黑名单承担 */
  allowDeny?: boolean;
  className?: string;
}) {
  const options: FileRuleAction[] = allowDeny ? ["deny", "ask", "allow"] : ["ask", "allow"];
  return (
    <Select value={value} onValueChange={(next) => onChange(next as FileRuleAction)}>
      <SelectTrigger
        aria-label={ariaLabel}
        className={cn("h-8 w-[96px] shrink-0 text-sm", className)}
      >
        <SelectValue />
      </SelectTrigger>
      <SelectContent>
        {options.map((option) => (
          <SelectItem key={option} value={option} className="text-sm">
            {ACTION_LABEL[option]}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}

/** 行尾的删除按钮 */
export function DeleteButton({ onClick, label }: { onClick: () => void; label: string }) {
  return (
    <button
      type="button"
      aria-label={label}
      onClick={onClick}
      className="flex size-8 shrink-0 items-center justify-center rounded-lg text-muted-foreground outline-none transition-colors hover:bg-accent hover:text-destructive focus-visible:ring-2 focus-visible:ring-ring/45"
    >
      <Trash className="size-3.5" />
    </button>
  );
}

/** 遮蔽提示：首条命中即停的固有后果——排在后面的这条永远不会生效。
 *  装饰要让看得见（is_known_key 教训的 UI 版），但它是提示不是错误 */
export function ShadowHint({ by }: { by: number }) {
  return (
    <p className="mt-1 text-2xs leading-4 text-amber-600 dark:text-amber-500">
      被第 {by} 条遮蔽：更靠前的规则先命中，这条不会生效。
    </p>
  );
}

/** 卡片顶部的生效说明 + 重置按钮：标题由 Group 的 h2 承担，卡片内不再重复 */
export function RuleHeader({
  description,
  onReset,
  resetLabel = "重置为默认",
  canReset,
}: {
  description: string;
  onReset: () => void;
  resetLabel?: string;
  canReset: boolean;
}) {
  const [confirming, setConfirming] = useState(false);
  return (
    <div className="flex items-start justify-between gap-4">
      <div className="min-w-0">
        <p className="text-xs leading-5 text-muted-foreground">
          规则按从上到下顺序匹配，命中第一条后停止；未命中的目标保持现有安全策略。
          新增的规则加在最上面；「放行」是你显式授权，与审批里的「以后都允许」同责。
        </p>
        <p className="mt-1 text-xs leading-5 text-muted-foreground">{description}</p>
      </div>
      {canReset ? (
        confirming ? (
          <div className="flex shrink-0 items-center gap-2">
            <Button
              variant="destructive"
              size="sm"
              onClick={() => {
                onReset();
                setConfirming(false);
              }}
            >
              确认重置
            </Button>
            <Button variant="ghost" size="sm" onClick={() => setConfirming(false)}>
              取消
            </Button>
          </div>
        ) : (
          <Button
            variant="subtle"
            size="sm"
            className="shrink-0"
            onClick={() => setConfirming(true)}
          >
            {resetLabel}
          </Button>
        )
      ) : null}
    </div>
  );
}

/** 开关行（复用 CapabilityToggle 的形状，带右对齐状态字） */
export function ToggleCell({
  label,
  enabled,
  onToggle,
}: {
  label: string;
  enabled: boolean;
  onToggle: () => void;
}) {
  return (
    <div className="flex items-center justify-end gap-2">
      <span className="text-xs text-muted-foreground">{enabled ? "已开启" : "已关闭"}</span>
      <CapabilityToggle label={label} enabled={enabled} onToggle={onToggle} />
    </div>
  );
}
