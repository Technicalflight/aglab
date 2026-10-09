import { useState } from "react";

import { Button } from "@/components/ui/button";
import { PaginationBar, usePaged } from "@/components/pagination";
import { FormColumn } from "@/components/ui/content-column";
import { Group, Row, SettingsHeader } from "@/components/settings-ui";
import { ActionSelect, DeleteButton, RuleHeader, ShadowHint } from "@/components/rule-editor";
import { networkRuleCovers, shadowMap } from "@/lib/security-rules";
import { useChatStore } from "@/store/chat-store";
import type { FileRuleAction } from "@/types/chat";

interface NetworkRule {
  pattern: string;
  action: FileRuleAction;
}

/**
 * 设置页的「网络安全」（design-security-center.md D5）：域名规则 + HTTP 明文分档。
 * 判定在 Rust 侧（`egress.rs`，同一份 host 解析）；规则之外，最外圈的出口名单
 * 与私网拒绝照旧在执行侧——规则放行的一次出站仍要过那两道物理闸。
 */
export function NetworkRulesSettings() {
  const config = useChatStore((s) => s.config);
  const updateConfig = useChatStore((s) => s.updateConfig);
  const rules = config.networkRules;
  const [draftPattern, setDraftPattern] = useState("");
  const [draftAction, setDraftAction] = useState<FileRuleAction>("ask");

  const shadowed = shadowMap(rules, networkRuleCovers);
  const saveRules = (next: NetworkRule[]) => void updateConfig({ networkRules: next });
  // 分页行带着全表下标走：页内下标会让改/删打中别的行
  const paged = usePaged(rules.map((rule, index) => ({ rule, index })));

  function addRule() {
    const pattern = draftPattern.trim();
    if (!pattern) return;
    saveRules([{ pattern, action: draftAction }, ...rules]);
    setDraftPattern("");
    paged.setPage(0);
  }

  return (
    <FormColumn>
      <SettingsHeader
        title="网络安全"
        description="按域名配置放行、询问或拒绝；HTTP 明文单独分档。规则之外，出口名单与内网地址两道闸照旧在执行侧——规则放行的一次出站仍要过那两道物理闸。"
      />
      <Group title="HTTP 请求拦截">
        <Row
          title="HTTP 远程链接"
          description="明文传输不加密，内容在网络上可能被窃听或篡改。默认「询问」——问一次不算贵。"
        >
          <ActionSelect
            ariaLabel="HTTP 远程链接动作"
            className="w-full"
            value={config.netHttpRemote}
            onChange={(next) => void updateConfig({ netHttpRemote: next })}
          />
        </Row>
        <Row
          title="HTTP 本地链接"
          description="回环地址（127.0.0.1、localhost）不离开本机。默认「放行」——本机端口调用天天有，问就是路障。"
        >
          <ActionSelect
            ariaLabel="HTTP 本地链接动作"
            className="w-full"
            value={config.netHttpLocal}
            onChange={(next) => void updateConfig({ netHttpLocal: next })}
          />
        </Row>
        <p className="mt-2 px-1 text-xs leading-5 text-muted-foreground">
          保存后立即用于后续操作，已经开始的操作不受影响。https 不吃这一档——它没有明文的问题。
        </p>
      </Group>

      <Group title="网络安全规则">
        <RuleHeader
          description="配置域名和命中后的访问动作。条目按域后缀匹配（example.com 覆盖 api.example.com，不覆盖 notexample.com），也可以粘整条 URL。"
          onReset={() => saveRules([])}
          resetLabel="清空规则"
          canReset={rules.length > 0}
        />
        <div className="space-y-3">
          <div className="flex items-center gap-2">
            <input
              type="text"
              value={draftPattern}
              spellCheck={false}
              placeholder="例如：api.example.com"
              aria-label="新网络规则的域名"
              className="min-w-0 flex-1 rounded-lg border border-border bg-surface px-3 py-2 font-mono text-sm outline-none transition-colors placeholder:text-muted-foreground/60 focus-visible:ring-2 focus-visible:ring-ring/45"
              onChange={(event) => setDraftPattern(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter" && draftPattern.trim()) {
                  event.preventDefault();
                  addRule();
                }
              }}
            />
            <ActionSelect
              ariaLabel="新网络规则的动作"
              value={draftAction}
              onChange={setDraftAction}
            />
            <Button variant="brand" size="sm" disabled={!draftPattern.trim()} onClick={addRule}>
              添加
            </Button>
          </div>
          <p className="mt-1.5 text-xs text-muted-foreground">
            规则按从上到下顺序匹配，命中第一条后停止；未命中保持现有安全策略。规则之外，出口名单与内网地址两道闸照旧在。
          </p>

          {rules.length === 0 ? (
            <p className="mt-4 text-sm text-muted-foreground">暂无规则。</p>
          ) : (
            <ul className="mt-4 space-y-2">
              {paged.slice.map(({ rule, index }) => (
                <li
                  key={`${rule.pattern}-${index}`}
                  className="rounded-lg border border-border bg-surface px-3 py-2"
                >
                  <div className="flex items-center gap-2">
                    <input
                      type="text"
                      value={rule.pattern}
                      spellCheck={false}
                      aria-label={`网络规则第 ${index + 1} 条的域名`}
                      className="min-w-0 flex-1 rounded-lg border border-border bg-background px-2 py-1.5 font-mono text-sm outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45"
                      onChange={(event) =>
                        saveRules(
                          rules.map((item, at) =>
                            at === index ? { ...item, pattern: event.target.value } : item,
                          ),
                        )
                      }
                    />
                    <ActionSelect
                      ariaLabel={`网络规则第 ${index + 1} 条的动作`}
                      value={rule.action}
                      onChange={(next) =>
                        saveRules(
                          rules.map((item, at) =>
                            at === index ? { ...item, action: next } : item,
                          ),
                        )
                      }
                    />
                    <DeleteButton
                      label={`删除网络规则第 ${index + 1} 条`}
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
