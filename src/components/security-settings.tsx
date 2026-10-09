import { useState } from "react";

import { CapabilityToggle } from "@/components/ui/capability-toggle";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Group, Row, SettingsHeader, inputClass } from "@/components/settings-ui";
import { setSandbox, setSandboxRoots } from "@/lib/chat-transport";
import { useChatStore } from "@/store/chat-store";
import { FormColumn } from "@/components/ui/content-column";

/** 沙箱额外可写根的清单编辑：增删即提交（后端逐个验证标注，失败整体回退）。
 *  输入框里敲的是绝对路径——后端会拒相对路径，这里不再各写一份校验 */
function SandboxRootsEditor() {
  const roots = useChatStore((s) => s.config.sandboxWritableRoots);
  const updateConfig = useChatStore((s) => s.updateConfig);
  const pushToast = useChatStore((s) => s.pushToast);
  const [draft, setDraft] = useState("");

  const commit = (next: string[]) => {
    setSandboxRoots(next)
      .then(() => void updateConfig({ sandboxWritableRoots: next }))
      .catch((cause: unknown) =>
        pushToast({
          tone: "error",
          title: "可写目录没有改成",
          detail: cause instanceof Error ? cause.message : String(cause),
        }),
      );
  };

  return (
    <div className="mt-2.5 max-w-[460px] space-y-1.5">
      {roots.map((root) => (
        <div
          key={root}
          className="flex items-center gap-1.5 rounded-lg border border-border bg-background py-1 pr-1 pl-2"
        >
          <span className="min-w-0 flex-1 truncate font-mono text-xs" title={root}>
            {root}
          </span>
          <button
            type="button"
            aria-label={`移除 ${root}`}
            onClick={() => commit(roots.filter((item) => item !== root))}
            className="shrink-0 rounded-lg px-1.5 py-0.5 text-xs text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
          >
            移除
          </button>
        </div>
      ))}
      <div className="flex items-center gap-1.5">
        <input
          aria-label="额外可写目录"
          type="text"
          spellCheck={false}
          value={draft}
          placeholder="C:\data\outputs"
          className={`${inputClass} min-w-0 flex-1 font-mono text-sm`}
          onChange={(event) => setDraft(event.target.value)}
          onKeyDown={(event) => {
            if (event.key !== "Enter" || !draft.trim()) return;
            event.preventDefault();
            commit([...roots, draft.trim()]);
            setDraft("");
          }}
        />
        <Button
          variant="subtle"
          size="sm"
          disabled={!draft.trim()}
          onClick={() => {
            commit([...roots, draft.trim()]);
            setDraft("");
          }}
        >
          添加
        </Button>
      </div>
    </div>
  );
}

/**
 * 设置页的「安全概览」项：命令沙箱、自动审查、审计账本。
 * 这里收的是"放行之前与放行之后"——边界怎么围、升级谁来批、发生过什么。
 */
export function SecuritySettings() {
  const config = useChatStore((s) => s.config);
  const updateConfig = useChatStore((s) => s.updateConfig);
  // 沙箱标注失败的原因只在这一格说：多为 icacls 被拦或目录只读
  const [sandboxError, setSandboxError] = useState<string | null>(null);

  return (
    <FormColumn>
      <SettingsHeader
        title="安全概览"
        description="1. 开「命令沙箱」，需要额外写入的目录在下方一并添加；2. 常常没人盯着时开「自动审查」，给它配一套便宜快的审查连接。拦截面在「文件 / 命令 / 网络安全」三页，反悔的保险在「数据安全」，账本在「审计中心」。"
      />

      <Group title="命令沙箱">
        <Row
          title="命令沙箱"
          description="把模型的一切写入收进同一条边界（对齐 Codex）。开启后命令子进程带三层：收容壳（全树同生共死）、WRITE_RESTRICTED 受限令牌（写只认能力清单：绑定工作目录 + 专用临时目录 + 额外可写目录）、低完整性（盘外 no-write-up）。write_file/edit_file 走同一条边界（未绑定则拒绝）；读与网络不受限。启用会给项目文件打完整性标签并授权能力 SID（可逆）。模型也可以逐条命令点名进/出沙箱，那一下要过单独的确认"
        >
          <div className="flex items-center justify-end gap-2">
            <span className="text-xs text-muted-foreground">
              {config.sandboxEnabled ? "已开启" : "已关闭"}
            </span>
            <CapabilityToggle
              label="命令沙箱"
              enabled={config.sandboxEnabled}
              onToggle={() => {
                setSandboxError(null);
                setSandbox(!config.sandboxEnabled)
                  .then(() => void updateConfig({ sandboxEnabled: !config.sandboxEnabled }))
                  .catch((cause) =>
                    setSandboxError(cause instanceof Error ? cause.message : String(cause)),
                  );
              }}
            />
          </div>
        </Row>
        {config.sandboxEnabled ? (
          <div className="border-b border-border px-1 py-4 last:border-b-0">
            <p className="text-base font-medium text-foreground">额外可写目录</p>
            <p className="mt-0.5 text-xs leading-5 text-muted-foreground">
              沙箱边界之外额外允许写入的目录（对齐 Codex 的
              writable_roots）。逐个打低完整性标签，填错会整体报错不落盘。
            </p>
            <SandboxRootsEditor />
          </div>
        ) : null}
        {sandboxError ? (
          <p className="mt-1.5 text-xs leading-5 text-destructive">
            沙箱没有开起来：{sandboxError}
          </p>
        ) : null}
      </Group>

      <Group title="自动审查">
        <Row
          title="自动审查"
          description="审批升级请求交给审查模型替人拍板：工具名 + 入参 + 风险档位 → APPROVE 或 DENY。不改沙箱边界——边界内照旧自主执行。默认关。审查失败按拒绝处理"
        >
          <div className="flex items-center justify-end gap-2">
            <span className="text-xs text-muted-foreground">
              {config.autoReview ? "已开启" : "已关闭"}
            </span>
            <CapabilityToggle
              label="自动审查"
              enabled={config.autoReview}
              onToggle={() => void updateConfig({ autoReview: !config.autoReview })}
            />
          </div>
        </Row>

        {config.autoReview ? (
          <Row
            wide
            title="审查连接"
            description="审查那一发走哪套服务商与模型。留空 = 跟着当前档案；审查是「看一行入参回 APPROVE/DENY」的小活，点名一套便宜快的连接才养得起常开"
          >
            <div className="flex items-center justify-end gap-2">
              <Select
                value={config.autoReviewProfileId || "__current__"}
                onValueChange={(value) =>
                  void updateConfig({ autoReviewProfileId: value === "__current__" ? "" : value })
                }
              >
                <SelectTrigger className="w-[170px] shrink-0 text-sm">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="__current__">当前连接</SelectItem>
                  {config.profiles.map((profile) => (
                    <SelectItem key={profile.id} value={profile.id} className="text-sm">
                      {profile.name}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
              <input
                type="text"
                value={config.autoReviewModel}
                aria-label="审查模型名"
                spellCheck={false}
                placeholder="模型名留空 = 用档案默认"
                className={`${inputClass} w-[180px] shrink-0 font-mono text-sm`}
                onChange={(event) => void updateConfig({ autoReviewModel: event.target.value })}
              />
            </div>
          </Row>
        ) : null}
      </Group>
    </FormColumn>
  );
}
