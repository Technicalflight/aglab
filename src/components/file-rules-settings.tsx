import { useState } from "react";

import { Button } from "@/components/ui/button";
import { PaginationBar, usePaged } from "@/components/pagination";
import { FormColumn } from "@/components/ui/content-column";
import { Group, SettingsHeader } from "@/components/settings-ui";
import { ActionSelect, DeleteButton, RuleHeader, ShadowHint } from "@/components/rule-editor";
import { FILE_RULE_PRESETS, fileRuleCovers, shadowMap } from "@/lib/security-rules";
import { useChatStore } from "@/store/chat-store";
import type { FileRule } from "@/types/chat";

type RuleActions = Pick<FileRule, "read" | "write" | "delete">;

const DEFAULT_ACTIONS: RuleActions = { read: "ask", write: "ask", delete: "deny" };

const COLUMNS: Array<{ key: "read" | "write" | "delete"; label: string }> = [
  { key: "read", label: "读取" },
  { key: "write", label: "写入" },
  { key: "delete", label: "删除" },
];

/**
 * 设置页的「文件安全」：按路径前缀分别配置读取、写入、删除三个动作。
 * 判定在 Rust 侧（`file_rules.rs`），这里只增删改查配置——保存即生效
 * （每回合现读配置装配权限表），存不进去时后端校验会整体回退并弹 toast
 */
export function FileRulesSettings() {
  const rules = useChatStore((s) => s.config.fileRules);
  const updateConfig = useChatStore((s) => s.updateConfig);
  const [draftPattern, setDraftPattern] = useState("");
  const [draftActions, setDraftActions] = useState<RuleActions>(DEFAULT_ACTIONS);

  const shadowed = shadowMap(rules, fileRuleCovers);
  // 分页行带着全表序号走：改/删/遮蔽提示认的都是全表下标，不是页内下标
  const indexed = rules.map((rule, index) => ({ rule, index }));
  const paged = usePaged(indexed);

  const save = (next: FileRule[]) => void updateConfig({ fileRules: next });

  function addRule() {
    const pattern = draftPattern.trim();
    if (!pattern) return;
    // 新规则加在最上面：首条命中即停的语义下，后加的规则优先——
    // 用户的最新意图不该被早先的宽规则悄悄吃掉
    save([{ pattern, ...draftActions }, ...rules]);
    setDraftPattern("");
    setDraftActions(DEFAULT_ACTIONS);
    paged.setPage(0);
  }

  function patchAt(index: number, patch: Partial<FileRule>) {
    save(rules.map((rule, at) => (at === index ? { ...rule, ...patch } : rule)));
  }

  const dirty =
    rules.length !== FILE_RULE_PRESETS.length ||
    rules.some((rule, at) => {
      const preset = FILE_RULE_PRESETS[at];
      return (
        !preset ||
        preset.pattern !== rule.pattern ||
        preset.read !== rule.read ||
        preset.write !== rule.write ||
        preset.delete !== rule.delete
      );
    });

  return (
    <FormColumn>
      <SettingsHeader
        title="文件安全"
        description="按路径分别配置读取、写入和删除权限；未命中规则时保持现有安全策略。规则可以放行——那是你的显式授权，不是模型的权限。"
      />
      <Group title="安全规则">
        <RuleHeader
          description="配置文件路径和读取、写入、删除动作。路径是目录前缀（覆盖整个子树），可以用 %USERPROFILE% 这类环境变量；不做通配符。"
          onReset={() => save([...FILE_RULE_PRESETS])}
          canReset={dirty}
        />

        <div className="space-y-3">
          {/* 添加一行：路径 + 三档动作，各档默认「询问」 */}
          <div className="flex items-center gap-2">
            <input
              type="text"
              value={draftPattern}
              spellCheck={false}
              placeholder="例如：%USERPROFILE%\.cache\myapp\"
              aria-label="新规则的路径前缀"
              className="min-w-0 flex-1 rounded-lg border border-border bg-surface px-3 py-2 font-mono text-sm outline-none transition-colors placeholder:text-muted-foreground/60 focus-visible:ring-2 focus-visible:ring-ring/45"
              onChange={(event) => setDraftPattern(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter" && draftPattern.trim()) {
                  event.preventDefault();
                  addRule();
                }
              }}
            />
            {COLUMNS.map((column) => (
              <ActionSelect
                key={column.key}
                ariaLabel={`新规则的${column.label}动作`}
                value={draftActions[column.key]}
                onChange={(next) =>
                  setDraftActions((previous) => ({ ...previous, [column.key]: next }))
                }
              />
            ))}
            <Button variant="brand" size="sm" disabled={!draftPattern.trim()} onClick={addRule}>
              添加
            </Button>
          </div>
          <p className="text-xs text-muted-foreground">
            规则按从上到下顺序匹配，命中第一条后停止；新规则默认添加到顶部。
          </p>

          {rules.length === 0 ? (
            <p className="mt-4 text-sm text-muted-foreground">
              暂无规则，未命中时保持现有安全策略（权限档位与可写目录）。
            </p>
          ) : (
            <ul className="mt-4 space-y-2">
              {paged.slice.map(({ rule, index }) => (
                <li
                  key={`${rule.pattern}-${index}`}
                  className="rounded-lg border border-border bg-surface px-3 py-3"
                >
                  <div className="flex flex-wrap items-center gap-2">
                    <input
                      type="text"
                      value={rule.pattern}
                      spellCheck={false}
                      aria-label={`第 ${index + 1} 条规则的路径前缀`}
                      className="min-w-0 flex-1 rounded-lg border border-border bg-background px-2 py-1.5 font-mono text-sm outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45"
                      onChange={(event) => patchAt(index, { pattern: event.target.value })}
                    />
                    {COLUMNS.map((column) => (
                      <ActionSelect
                        key={column.key}
                        ariaLabel={`第 ${index + 1} 条规则的${column.label}动作`}
                        value={rule[column.key]}
                        onChange={(next) => patchAt(index, { [column.key]: next })}
                      />
                    ))}
                    <DeleteButton
                      label={`删除第 ${index + 1} 条规则`}
                      onClick={() => save(rules.filter((_, at) => at !== index))}
                    />
                  </div>
                  {shadowed.get(index) ? <ShadowHint by={shadowed.get(index)!} /> : null}
                </li>
              ))}
            </ul>
          )}
          {rules.length > 0 ? (
            <PaginationBar
              page={paged.page}
              pages={paged.pages}
              total={paged.total}
              onPage={paged.setPage}
            />
          ) : null}
        </div>
      </Group>
    </FormColumn>
  );
}
