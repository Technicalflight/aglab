import { useEffect, useState } from "react";
import { IconTrash as Trash } from "@tabler/icons-react";

import { Button } from "@/components/ui/button";
import { CapabilityToggle } from "@/components/ui/capability-toggle";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { inputClass } from "@/components/settings-ui";
import { cn } from "@/lib/utils";

/** 弹窗里一条规则要编辑/展示的全部信息（由规则库页组装传入） */
export interface SecretRuleDialogData {
  key: string;
  id: string;
  label: string;
  kind: string;
  builtin: boolean;
  /** 内置规则被改写过的正则；没有 = 从未改过 */
  editedPattern?: string;
  /** 当前生效的正则（改写优先） */
  pattern: string;
  hintGated: boolean;
  enabled: boolean;
}

/**
 * 「规则详情」弹窗（design-security-center.md D6）。
 *
 * 内置规则：名称是"这条是什么"的契约，只读；可改正则（改过正则的规则不再吃
 * 关键词预筛闸，弹窗里的检测能力标签会如实换掉）。自建规则：名称与正则都可改，
 * 还可删除。规则状态（开启/停用）用行内同一颗开关——两处改的是同一份配置。
 * 保存 = `onSave` 回调（存盘并落执行侧一次做完），弹窗只管收集草稿。
 */
export function SecretRuleDialog({
  rule,
  onClose,
  onSave,
  onToggle,
  onDelete,
  onRestorePattern,
}: {
  rule: SecretRuleDialogData | null;
  onClose: () => void;
  onSave: (id: string, patch: { label?: string; pattern: string }) => void;
  onToggle: (id: string) => void;
  onDelete: (id: string) => void;
  onRestorePattern: (id: string) => void;
}) {
  const [draftLabel, setDraftLabel] = useState("");
  const [draftPattern, setDraftPattern] = useState("");

  // 打开那一刻抓一次草稿；rule 未变时不重置——编辑中途的重渲染不能吞掉正在敲的字。
  // 「恢复默认正则」会让 rule.pattern 变化，草稿跟着回到恢复后的值
  const openKey = rule?.key ?? "";
  const livePattern = rule?.pattern ?? "";
  const liveLabel = rule?.label ?? "";
  useEffect(() => {
    if (rule) {
      setDraftLabel(rule.label);
      setDraftPattern(rule.editedPattern ?? rule.pattern);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps -- 换对象或生效正则变化时才抓草稿
  }, [openKey, livePattern, liveLabel]);

  if (!rule) return null;
  const isCustom = !rule.builtin;
  const patternDirty = draftPattern.trim() !== (rule.editedPattern ?? rule.pattern);
  const labelDirty = isCustom && draftLabel.trim() !== rule.label;
  const canSave = draftPattern.trim().length > 0 && (!isCustom || draftLabel.trim().length > 0);

  return (
    <Dialog open onOpenChange={(next) => !next && onClose()}>
      <DialogContent className="w-[min(560px,92vw)]">
        <DialogTitle>规则详情</DialogTitle>

        <div className="space-y-4">
          <div>
            <p className="mb-1 text-sm font-medium text-foreground">规则名称</p>
            <input
              type="text"
              value={isCustom ? draftLabel : rule.label}
              disabled={!isCustom}
              spellCheck={false}
              aria-label="规则名称"
              className={cn(inputClass, !isCustom && "opacity-60")}
              onChange={(event) => setDraftLabel(event.target.value)}
            />
            {!isCustom ? (
              <p className="mt-1 text-2xs leading-4 text-muted-foreground">
                内置规则的名称不改——它是"这条规则拦什么"的契约，要换语义就改正则。
              </p>
            ) : null}
          </div>

          <div>
            <p className="mb-1 text-sm font-medium text-foreground">规则来源</p>
            <span className="inline-block rounded border border-border bg-surface px-1.5 py-0.5 text-2xs text-muted-foreground">
              {isCustom ? "自定义规则" : "内置规则"}
            </span>
          </div>

          <div>
            <p className="mb-1 text-sm font-medium text-foreground">检测能力</p>
            <div className="flex flex-wrap gap-1.5">
              <span className="rounded bg-surface px-1.5 py-0.5 text-2xs text-muted-foreground">
                正则匹配检测
              </span>
              {rule.hintGated && !patternDirty ? (
                <span className="rounded bg-surface px-1.5 py-0.5 text-2xs text-muted-foreground">
                  关键词预筛
                </span>
              ) : null}
            </div>
            {rule.hintGated && patternDirty ? (
              <p className="mt-1 text-2xs leading-4 text-muted-foreground">
                正则改过之后不再吃关键词预筛——你写的 pattern 按字面生效。
              </p>
            ) : null}
          </div>

          <div>
            <p className="mb-1 text-sm font-medium text-foreground">规则状态</p>
            <div className="flex items-center justify-between gap-2 rounded-lg border border-border bg-surface px-3 py-2">
              <span className="text-sm text-muted-foreground">
                {rule.enabled ? "开启：参与检测与打码" : "停用：既不检测也不打码"}
              </span>
              <CapabilityToggle
                label={`规则状态 ${rule.label}`}
                enabled={rule.enabled}
                onToggle={() => onToggle(rule.id)}
              />
            </div>
          </div>

          <div>
            <p className="mb-1 text-sm font-medium text-foreground">正则</p>
            {/* resize 必须关掉：手一拖就把底部按钮挤出弹窗——高度交给 rows 与弹窗自己的滚动 */}
            <textarea
              rows={4}
              value={draftPattern}
              spellCheck={false}
              aria-label="规则正则"
              className="w-full resize-none rounded-lg border border-input bg-background px-3 py-2 font-mono text-sm text-foreground outline-none transition-colors focus-visible:border-brand/50 focus-visible:ring-2 focus-visible:ring-ring/35"
              onChange={(event) => setDraftPattern(event.target.value)}
            />
            <p className="mt-1 text-2xs leading-4 text-muted-foreground">
              正则按 Rust regex 语法（不支持环视）。保存后立即用于后续的工具结果打码与检测。
            </p>
          </div>

          <div className="flex items-center justify-between gap-2">
            {isCustom ? (
              <Button variant="ghost" size="sm" onClick={() => onDelete(rule.id)}>
                <Trash className="size-3.5" />
                删除规则
              </Button>
            ) : rule.editedPattern ? (
              <Button variant="ghost" size="sm" onClick={() => onRestorePattern(rule.id)}>
                恢复默认正则
              </Button>
            ) : (
              <span />
            )}
            <div className="flex gap-2">
              <Button
                variant="ghost"
                size="sm"
                onClick={() => {
                  setDraftPattern(rule.editedPattern ?? rule.pattern);
                  setDraftLabel(rule.label);
                }}
                disabled={!patternDirty && !labelDirty}
              >
                撤销修改
              </Button>
              <Button
                variant="brand"
                size="sm"
                disabled={!canSave || (!patternDirty && !labelDirty)}
                onClick={() =>
                  onSave(
                    rule.id,
                    isCustom
                      ? { label: draftLabel.trim(), pattern: draftPattern.trim() }
                      : { pattern: draftPattern.trim() },
                  )
                }
              >
                保存
              </Button>
            </div>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}
