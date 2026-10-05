import { useState } from "react";

import { Button } from "@/components/ui/button";
import { PaginationBar, usePaged } from "@/components/pagination";
import { FormColumn } from "@/components/ui/content-column";
import { Group, SettingsHeader } from "@/components/settings-ui";
import { ActionSelect, DeleteButton, RuleHeader, ShadowHint } from "@/components/rule-editor";
import { COMMAND_BLOCKLIST_PRESETS, commandRuleCovers, shadowMap } from "@/lib/security-rules";
import { useChatStore } from "@/store/chat-store";
import type { CommandRule, FileRuleAction } from "@/types/chat";

/**
 * 设置页的「命令安全」（design-security-center.md D4）：程序黑名单 + 命令前缀规则。
 * 黑名单是机器级的（wsl.exe 在哪个项目里都不该由模型跑），只有全局一份；
 * 判定在 Rust 侧（`command_rules.rs`），命令行按 `&& ; |` 与换行拆段后逐段过闸。
 */
export function CommandRulesSettings() {
  const blocklist = useChatStore((s) => s.config.commandBlocklist);
  const rules = useChatStore((s) => s.config.commandRules);
  const updateConfig = useChatStore((s) => s.updateConfig);
  const [draftProgram, setDraftProgram] = useState("");
  const [draftPrefix, setDraftPrefix] = useState("");
  const [draftAction, setDraftAction] = useState<FileRuleAction>("ask");

  const shadowed = shadowMap(rules, commandRuleCovers);
  const blocklistDirty =
    blocklist.length !== COMMAND_BLOCKLIST_PRESETS.length ||
    blocklist.some((name, at) => name !== COMMAND_BLOCKLIST_PRESETS[at]);
  // 分页行带着全表下标走：页内下标会让"改第 2 行"打到别的行
  const pagedBlocklist = usePaged(blocklist.map((name, index) => ({ name, index })));
  const pagedRules = usePaged(rules.map((rule, index) => ({ rule, index })));

  function addProgram() {
    const name = draftProgram.trim();
    if (!name) return;
    saveBlocklist([...blocklist, name]);
    setDraftProgram("");
    pagedBlocklist.setPage(0);
  }

  function addRule() {
    const prefix = draftPrefix.trim();
    if (!prefix) return;
    saveRules([{ prefix, action: draftAction }, ...rules]);
    setDraftPrefix("");
    pagedRules.setPage(0);
  }

  const saveBlocklist = (next: string[]) => void updateConfig({ commandBlocklist: next });
  const saveRules = (next: CommandRule[]) => void updateConfig({ commandRules: next });

  return (
    <FormColumn>
      <SettingsHeader
        title="命令安全"
        description="配置禁止运行的高风险程序（黑名单），与按前缀放行或询问的命令规则。拆段后的子命令同样会检查；解析不了的复合命令退回审批，绝不因规则存在而变松。"
      />
      <Group title="命令安全">
        <RuleHeader
          description="配置禁止运行的高风险程序名单，命中即拦，拆段后的子命令同样会检查。仅填写程序名称，.exe 后缀可省略。"
          onReset={() => saveBlocklist([...COMMAND_BLOCKLIST_PRESETS])}
          canReset={blocklistDirty}
        />
        <div className="space-y-3">
        <div className="flex items-center gap-2">
          <input
            type="text"
            value={draftProgram}
            spellCheck={false}
            placeholder="例如：reg.exe"
            aria-label="新黑名单程序名"
            className="min-w-0 flex-1 rounded-lg border border-border bg-surface px-3 py-2 font-mono text-sm outline-none transition-colors placeholder:text-muted-foreground/60 focus-visible:ring-2 focus-visible:ring-ring/45"
            onChange={(event) => setDraftProgram(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter" && draftProgram.trim()) {
                event.preventDefault();
                addProgram();
              }
            }}
          />
          <Button variant="brand" size="sm" disabled={!draftProgram.trim()} onClick={addProgram}>
            添加
          </Button>
        </div>

        {blocklist.length === 0 ? (
          <p className="mt-4 text-sm text-muted-foreground">黑名单是空的——没有任何程序被这台机器上的模型禁止。</p>
        ) : (
          <ul className="mt-4 space-y-1">
            {pagedBlocklist.slice.map(({ name, index }) => (
              <li
                key={`${name}-${index}`}
                className="flex items-center gap-2 rounded-lg border border-border bg-surface px-3 py-2"
              >
                <input
                  type="text"
                  value={name}
                  spellCheck={false}
                  aria-label={`黑名单第 ${index + 1} 项`}
                  className="min-w-0 flex-1 bg-transparent font-mono text-sm outline-none"
                  onChange={(event) =>
                    saveBlocklist(blocklist.map((item, at) => (at === index ? event.target.value : item)))
                  }
                />
                <DeleteButton
                  label={`删除黑名单第 ${index + 1} 项`}
                  onClick={() => saveBlocklist(blocklist.filter((_, at) => at !== index))}
                />
              </li>
            ))}
          </ul>
        )}
        {blocklist.length > 0 ? (
          <PaginationBar
            page={pagedBlocklist.page}
            pages={pagedBlocklist.pages}
            total={pagedBlocklist.total}
            onPage={pagedBlocklist.setPage}
          />
        ) : null}
      </div>
      </Group>

      <Group title="命令规则">
        <RuleHeader
          description="配置命令前缀和命中后的处理动作；命中询问规则后需确认，命中放行规则后自动执行。规则按优先级从上到下匹配，一次命令拆出的多段各查各的、取最严。"
          onReset={() => saveRules([])}
          resetLabel="清空规则"
          canReset={rules.length > 0}
        />
        <div className="space-y-3">
        <div className="flex items-center gap-2">
          <input
            type="text"
            value={draftPrefix}
            spellCheck={false}
            placeholder="例如：git push"
            aria-label="新命令规则的前缀"
            className="min-w-0 flex-1 rounded-lg border border-border bg-surface px-3 py-2 font-mono text-sm outline-none transition-colors placeholder:text-muted-foreground/60 focus-visible:ring-2 focus-visible:ring-ring/45"
            onChange={(event) => setDraftPrefix(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter" && draftPrefix.trim()) {
                event.preventDefault();
                addRule();
              }
            }}
          />
          <ActionSelect
            ariaLabel="新命令规则的动作"
            value={draftAction}
            allowDeny={false}
            onChange={setDraftAction}
          />
          <Button variant="brand" size="sm" disabled={!draftPrefix.trim()} onClick={addRule}>
            添加
          </Button>
        </div>

        {rules.length === 0 ? (
          <p className="mt-4 text-sm text-muted-foreground">暂无规则，未命中时保持现有安全策略（命令按类别过审批闸）。</p>
        ) : (
          <ul className="mt-4 space-y-2">
            {pagedRules.slice.map(({ rule, index }) => (
              <li key={`${rule.prefix}-${index}`} className="rounded-lg border border-border bg-surface px-3 py-2">
                <div className="flex items-center gap-2">
                  <input
                    type="text"
                    value={rule.prefix}
                    spellCheck={false}
                    aria-label={`命令规则第 ${index + 1} 条的前缀`}
                    className="min-w-0 flex-1 rounded-lg border border-border bg-background px-2 py-1.5 font-mono text-sm outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45"
                    onChange={(event) =>
                      saveRules(rules.map((item, at) => (at === index ? { ...item, prefix: event.target.value } : item)))
                    }
                  />
                  <ActionSelect
                    ariaLabel={`命令规则第 ${index + 1} 条的动作`}
                    value={rule.action}
                    allowDeny={false}
                    onChange={(next) =>
                      saveRules(rules.map((item, at) => (at === index ? { ...item, action: next } : item)))
                    }
                  />
                  <DeleteButton
                    label={`删除命令规则第 ${index + 1} 条`}
                    onClick={() => saveRules(rules.filter((_, at) => at !== index))}
                  />
                </div>
                {shadowed.get(index) ? <ShadowHint by={shadowed.get(index)!} /> : null}
              </li>
            ))}
          </ul>
        )}
        {rules.length > 0 ? (
          <PaginationBar
            page={pagedRules.page}
            pages={pagedRules.pages}
            total={pagedRules.total}
            onPage={pagedRules.setPage}
          />
        ) : null}
      </div>
      </Group>
    </FormColumn>
  );
}
