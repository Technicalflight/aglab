import { useState } from "react";

import { Button } from "@/components/ui/button";
import { SettingsHeader, inputClass } from "@/components/settings-ui";
import { useChatStore } from "@/store/chat-store";
import { FormColumn } from "@/components/ui/content-column";

/**
 * 设置页的「SSH 主机」项：ssh_run 工具的花名册。
 * 从运行行为页拆出来——它配置的是一个执行通道的准入名单。
 */
export function SshSettings() {
  const hosts = useChatStore((s) => s.config.sshHosts);
  const updateConfig = useChatStore((s) => s.updateConfig);
  const [draft, setDraft] = useState("");

  const commit = (next: string[]) => void updateConfig({ sshHosts: next });
  const line = draft.trim();

  return (
    <FormColumn>
      <SettingsHeader
        title="SSH 主机"
        description="1. 添加一行 名字=user@地址:端口；2. 对话里让模型用「SSH 执行」点名这台主机跑命令；3. 认证用系统 ssh 的密钥或 agent。"
      />

      <div className="mt-6 rounded-lg border border-border bg-surface px-3 py-4">
        <p className="text-base font-medium text-foreground">主机清单</p>
        <p className="mt-0.5 text-xs leading-5 text-muted-foreground">
          每行 <span className="font-mono">名字=user@host:端口</span>。凭据不在这里—— 走系统 ssh
          自己的钥匙链（密钥/agent），口令认证不做。
        </p>
        <div className="mt-2.5 max-w-[460px] space-y-1.5">
          {hosts.map((host) => (
            <div
              key={host}
              className="flex items-center gap-1.5 rounded-lg border border-border bg-background py-1 pr-1 pl-2"
            >
              <span className="min-w-0 flex-1 truncate font-mono text-xs" title={host}>
                {host}
              </span>
              <button
                type="button"
                aria-label={`移除 ${host}`}
                onClick={() => commit(hosts.filter((item) => item !== host))}
                className="shrink-0 rounded-lg px-1.5 py-0.5 text-xs text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
              >
                移除
              </button>
            </div>
          ))}
          <div className="flex items-center gap-1.5">
            <input
              aria-label="SSH 主机别名"
              type="text"
              spellCheck={false}
              value={draft}
              placeholder="生产机=deploy@10.0.0.8:22"
              className={`${inputClass} min-w-0 flex-1 font-mono text-sm`}
              onChange={(event) => setDraft(event.target.value)}
              onKeyDown={(event) => {
                if (event.key !== "Enter" || !line) return;
                event.preventDefault();
                if (line.includes("=") && line.includes("@")) {
                  commit([...hosts, line]);
                  setDraft("");
                }
              }}
            />
            <Button
              variant="subtle"
              size="sm"
              disabled={!(line.includes("=") && line.includes("@"))}
              onClick={() => {
                commit([...hosts, line]);
                setDraft("");
              }}
            >
              添加
            </Button>
          </div>
          <span className="block text-xs leading-5 text-muted-foreground">
            端口可省（默认 22）。认证走系统 ssh 的密钥或 agent——连不上时先在终端里
            <code className="mx-1 font-mono">ssh user@host</code>确认能登。
          </span>
        </div>
      </div>
    </FormColumn>
  );
}
