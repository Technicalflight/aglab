import { useState } from "react";

import { Button } from "@/components/ui/button";
import { SettingsHeader, inputClass } from "@/components/settings-ui";
import { useChatStore } from "@/store/chat-store";
import { FormColumn } from "@/components/ui/content-column";

/**
 * 设置页的「LSP 服务器」项：lsp_query 工具的服务器命令表。
 * 从运行行为页拆出来——它配置的是语义查询的后端进程，不是推理行为。
 */
export function LspSettings() {
  const servers = useChatStore((s) => s.config.lspServers);
  const updateConfig = useChatStore((s) => s.updateConfig);
  const [draft, setDraft] = useState("");

  const commit = (next: string[]) => void updateConfig({ lspServers: next });
  const line = draft.trim();
  const valid = line.includes("=") && line.split("=")[1].trim() !== "";

  return (
    <FormColumn>
      <SettingsHeader
        title="LSP 服务器"
        description="1. 常见语言开箱即用（rs / ts / py / go / c，服务器从 PATH 找）；2. 别的语言或自定义安装在这里加一行 ext=启动命令；3. 对话里用「LSP 语义查询」查定义、引用、悬停与符号。"
      />

      <div className="mt-6 rounded-lg border border-border bg-surface px-3 py-4">
        <p className="text-base font-medium text-foreground">逐扩展覆盖</p>
        <p className="mt-0.5 text-xs leading-5 text-muted-foreground">
          每行 <span className="font-mono">ext=启动命令</span>。
        </p>
        <div className="mt-2.5 max-w-[460px] space-y-1.5">
          {servers.map((server) => (
            <div
              key={server}
              className="flex items-center gap-1.5 rounded-lg border border-border bg-background py-1 pr-1 pl-2"
            >
              <span className="min-w-0 flex-1 truncate font-mono text-xs" title={server}>
                {server}
              </span>
              <button
                type="button"
                aria-label={`移除 ${server}`}
                onClick={() => commit(servers.filter((item) => item !== server))}
                className="shrink-0 rounded-lg px-1.5 py-0.5 text-xs text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
              >
                移除
              </button>
            </div>
          ))}
          <div className="flex items-center gap-1.5">
            <input
              type="text"
              spellCheck={false}
              value={draft}
              placeholder="zig=zls"
                  aria-label="LSP 映射"
              className={`${inputClass} min-w-0 flex-1 font-mono text-sm`}
              onChange={(event) => setDraft(event.target.value)}
              onKeyDown={(event) => {
                if (event.key !== "Enter" || !valid) return;
                event.preventDefault();
                commit([...servers, line]);
                setDraft("");
              }}
            />
            <Button
              variant="subtle"
              size="sm"
              disabled={!valid}
              onClick={() => {
                commit([...servers, line]);
                setDraft("");
              }}
            >
              添加
            </Button>
          </div>
          <span className="block text-xs leading-5 text-muted-foreground">
            默认认识 rs、ts/tsx、js/jsx、py、go、c/cpp（服务器从 PATH 找）。别的扩展名或
            非 PATH 安装在这里补一行。
          </span>
        </div>
      </div>
    </FormColumn>
  );
}
