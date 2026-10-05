import { useEffect, useState } from "react";

import { CapabilityToggle } from "@/components/ui/capability-toggle";
import { Button } from "@/components/ui/button";
import { Group, Row, SettingsHeader, inputClass } from "@/components/settings-ui";
import { PaginationBar, usePaged } from "@/components/pagination";
import { DeleteButton } from "@/components/rule-editor";
import {
  fetchSecretRules,
  openBackupDir,
  setDeleteToTrash,
  setSecretScan,
} from "@/lib/chat-transport";
import { useChatStore } from "@/store/chat-store";
import type { CustomSecretRule, SecretRulePatternEdit } from "@/types/chat";
import { FormColumn } from "@/components/ui/content-column";
import { SecretRuleDialog } from "@/components/secret-rule-dialog";

/**
 * 敏感检测规则库（design-security-center.md D6）：`secrets::RULES` 的界面清单。
 * 每条按 id 开关——关闭对"检测"与"打码"同时生效，不存在"还在检测但不再打码"
 * 的半开关状态。开关即存盘并落执行侧（`setSecretScan` 一次做完两件事）
 */
function SecretRulesLibrary() {
  const config = useChatStore((s) => s.config);
  const updateConfig = useChatStore((s) => s.updateConfig);
  const [builtins, setBuiltins] = useState<
    Array<{ id: string; label: string; kind: string; hintGated: boolean; pattern: string }> | null
  >(null);
  const [error, setError] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  // 弹窗状态：edit = 打开某条的详情；create = 新建一条自建规则
  const [dialog, setDialog] = useState<{ mode: "edit"; key: string } | { mode: "create" } | null>(null);

  useEffect(() => {
    fetchSecretRules()
      .then(setBuiltins)
      .catch((cause: unknown) => setError(cause instanceof Error ? cause.message : String(cause)));
  }, []);

  const disabled = config.disabledSecretRules;
  const customs = config.customSecretRules;
  const edits = config.secretRulePatternEdits;

  // 任何一类改动都走同一道闸：存盘 + 落执行侧一次做完
  const apply = (
    nextDisabled: string[],
    nextCustoms: CustomSecretRule[],
    nextEdits: SecretRulePatternEdit[],
  ) => {
    void setSecretScan(config.secretScanEnabled, nextDisabled, nextCustoms, nextEdits).then(() =>
      void updateConfig({
        disabledSecretRules: nextDisabled,
        customSecretRules: nextCustoms,
        secretRulePatternEdits: nextEdits,
      }),
    );
  };
  const toggle = (id: string) => {
    const next = disabled.includes(id) ? disabled.filter((off) => off !== id) : [...disabled, id];
    apply(next, customs, edits);
  };
  const restoreDefaults = () => apply([], [], []);
  const restorePattern = (id: string) => {
    const nextEdits = edits.filter((item) => item.id !== id);
    apply(disabled, customs, nextEdits);
    // 弹窗还开着：草稿由父组件的下一次渲染喂回（rule.pattern 变了）
  };
  function addCustom(label: string, pattern: string) {
    const id = `custom-${crypto.randomUUID().slice(0, 8)}`;
    apply(disabled, [...customs, { id, label, pattern }], edits);
    // 新规则插在自建段首，回第 1 页找不到它——跳到最后一页
    requestAnimationFrame(() => paged.setPage(Number.MAX_SAFE_INTEGER));
  }
  function saveEdited(id: string, patch: { label?: string; pattern: string }) {
    const row = rows.find((item) => item.key === id);
    if (!row) return;
    if (row.builtin) {
      if (!patch.pattern) return;
      apply(disabled, customs, [...edits.filter((item) => item.id !== id), { id, pattern: patch.pattern }]);
    } else {
      const label = patch.label?.trim();
      const pattern = patch.pattern.trim();
      if (!label || !pattern) return;
      apply(disabled, customs.map((rule) => (rule.id === id ? { ...rule, label, pattern } : rule)), edits);
    }
    setDialog(null);
  }
  function deleteCustom(id: string) {
    apply(
      disabled.filter((off) => off !== id),
      customs.filter((rule) => rule.id !== id),
      edits,
    );
    setDialog(null);
  }

  const lowered = query.trim().toLowerCase();
  const builtinMatches = (label: string, id: string) =>
    !lowered || label.toLowerCase().includes(lowered) || id.includes(lowered);
  // 合成展示清单：内置（标注"已改"）+ 自建；搜索同时过滤两类
  const rows: Array<{
    key: string;
    id: string;
    label: string;
    kind: string;
    builtin: boolean;
    editedPattern?: string;
    pattern: string;
    hintGated: boolean;
    enabled: boolean;
  }> = [
    ...(builtins ?? [])
      .filter((rule) => builtinMatches(rule.label, rule.id))
      .map((rule) => {
        const edit = edits.find((item) => item.id === rule.id);
        return {
          key: rule.id,
          id: rule.id,
          label: rule.label,
          kind: edit ? "凭据 · 已改" : rule.kind,
          builtin: true,
          editedPattern: edit?.pattern,
          pattern: edit?.pattern ?? rule.pattern,
          hintGated: rule.hintGated,
          enabled: !disabled.includes(rule.id),
        };
      }),
    ...customs
      .filter((rule) => builtinMatches(rule.label, rule.id))
      .map((rule) => ({
        key: rule.id,
        id: rule.id,
        label: rule.label,
        kind: "自定义",
        builtin: false,
        pattern: rule.pattern,
        hintGated: false,
        enabled: !disabled.includes(rule.id),
      })),
  ];
  // 搜索词变了回第 1 页——hooks 顺序不能让位给下面的报错返回，所以都排在它前面
  const paged = usePaged(rows, undefined, query);
  if (error) {
    return <p className="mt-2 text-xs text-destructive">规则库读不出来：{error}</p>;
  }

  // 弹窗吃**活的**行数据：开关在弹窗里拨动后，父组件重渲染会把新状态喂回来
  const dialogRule = dialog
    ? dialog.mode === "create"
      ? {
          key: "__new__",
          id: "",
          label: "",
          kind: "自定义",
          builtin: false,
          editedPattern: undefined,
          pattern: "",
          hintGated: false,
          enabled: true,
        }
      : (rows.find((row) => row.key === dialog.key) ?? null)
    : null;

  return (
    <div className="border-b border-border px-1 py-4 last:border-b-0">
      <div className="flex items-center justify-between gap-2">
        <div className="min-w-0">
          <p className="text-base font-medium text-foreground">检测规则</p>
          <p className="mt-0.5 text-xs leading-5 text-muted-foreground">
            关掉的规则对检测与打码同时失效。默认全部开启。
          </p>
        </div>
        {disabled.length > 0 ? (
          <Button variant="subtle" size="sm" onClick={restoreDefaults}>
            恢复全部开启
          </Button>
        ) : null}
      </div>
      <input
        type="search"
        value={query}
        placeholder="搜索规则名"
        aria-label="搜索检测规则"
        className={`${inputClass} mt-2 h-8 text-sm`}
        onChange={(event) => setQuery(event.target.value)}
      />
      {builtins === null ? (
        <p className="mt-2 text-xs text-muted-foreground">读规则库…</p>
      ) : (
        <>
          <div className="mt-3 flex items-center gap-2">
            <Button variant="subtle" size="sm" onClick={() => setDialog({ mode: "create" })}>
              添加自定义规则
            </Button>
            <p className="min-w-0 flex-1 text-2xs leading-4 text-muted-foreground">
              自建规则吃正则，名称会显示在打码占位与规则库里。
            </p>
          </div>

          <ul className="mt-2 space-y-1">
            {paged.slice.map((row) => (
              <li key={row.key} className="rounded-lg border border-border bg-background px-2.5 py-1.5">
                <div className="flex items-center gap-2">
                  <div className="min-w-0 flex-1">
                    <p className="truncate text-sm text-foreground">
                      {row.label}
                      {row.editedPattern ? (
                        <span className="ml-1.5 text-2xs text-amber-600 dark:text-amber-500">已改</span>
                      ) : null}
                    </p>
                    <p className="text-2xs text-muted-foreground">{row.kind}</p>
                  </div>
                  <button
                    type="button"
                    aria-label={`查看与编辑规则 ${row.label}`}
                    onClick={() => setDialog({ mode: "edit", key: row.key })}
                    className="shrink-0 rounded-lg px-1.5 py-0.5 text-xs text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
                  >
                    详情
                  </button>
                  <CapabilityToggle
                    label={`检测规则 ${row.label}`}
                    enabled={row.enabled}
                    onToggle={() => toggle(row.id)}
                  />
                  {!row.builtin ? (
                    <DeleteButton
                      label={`删除自定义规则 ${row.label}`}
                      onClick={() => deleteCustom(row.id)}
                    />
                  ) : null}
                </div>
                {/* 自建规则与改过正则的内置规则把正则亮出来——这是"它到底拦什么"的唯一凭据 */}
                {row.editedPattern || !row.builtin ? (
                  <p className="mt-1 truncate font-mono text-2xs text-muted-foreground">{row.pattern}</p>
                ) : null}
              </li>
            ))}
            {paged.slice.length === 0 ? (
              <li className="py-1 text-xs text-muted-foreground">没有匹配的规则。</li>
            ) : null}
          </ul>
        </>
      )}
      {builtins !== null && builtins.length + customs.length > 0 ? (
        <PaginationBar page={paged.page} pages={paged.pages} total={paged.total} onPage={paged.setPage} />
      ) : null}

      <SecretRuleDialog
        rule={dialogRule}
        onClose={() => setDialog(null)}
        onSave={(id, patch) => {
          if (dialog?.mode === "create") {
            addCustom(patch.label ?? "", patch.pattern);
          } else {
            saveEdited(id, patch);
          }
        }}
        onToggle={toggle}
        onDelete={deleteCustom}
        onRestorePattern={restorePattern}
      />
    </div>
  );
}

/**
 * 设置页的「数据安全」项（design-security-center.md D8）：删除保护、自动备份、
 * 敏感保护与它的检测规则库。这一页收的是"出了事能不能反悔"与"凭据不出门"——
 * 拦截面（哪些动作要过闸）在另外四页，账在「审计中心」
 */
export function DataSecuritySettings() {
  const config = useChatStore((s) => s.config);
  const updateConfig = useChatStore((s) => s.updateConfig);

  return (
    <FormColumn>
      <SettingsHeader
        title="数据安全"
        description={
          "删除有备份与回收站两层保险，改动有副本可恢复，凭据进不了上下文。这里的开关只管「反悔的保险有多厚」；" +
          "拦截面在文件、命令、网络三页，发生过的都在「审计中心」。"
        }
      />

      <Group title="数据安全">
        <Row
          title="删除保护"
          description="「删除文件」默认移入回收站（能反悔）；关掉后按系统删除。每笔删除动手前都先存一份改前备份——回收站被组策略停用时（IFileOperation 会静默变成永久删除），备份就是那份能反悔的保险。批量删除另有单独的审批阈值，权限档位（含「完全访问」）都压不住它"
        >
          <div className="flex items-center justify-end gap-2">
            <span className="text-xs text-muted-foreground">
              {config.deleteToTrash ? "回收站" : "系统删除"}
            </span>
            <CapabilityToggle
              label="删除保护"
              enabled={config.deleteToTrash}
              onToggle={() =>
                void setDeleteToTrash(!config.deleteToTrash).then(() =>
                  void updateConfig({ deleteToTrash: !config.deleteToTrash }),
                )
              }
            />
          </div>
        </Row>
        <Row
          title="批量删除审批阈值"
          description="一次删除达到这个文件数就要审批。0 = 不设阈值（不推荐：一次删几百个的请求不该静默执行）"
        >
          <input
            type="number"
            min={0}
            aria-label="批量删除审批阈值"
            className={`${inputClass} w-24 text-sm`}
            value={config.deleteApprovalThreshold}
            onChange={(event) =>
              void updateConfig({ deleteApprovalThreshold: Math.max(0, Number(event.target.value) || 0) })
            }
          />
        </Row>
        <Row
          title="自动备份"
          description="写/删之前把当前文件存一份可恢复的副本（每改必存、内容没变就跳过）。备份是尽力而为：失败不挡原操作，只落审计"
        >
          <div className="flex items-center justify-end gap-2">
            <span className="text-xs text-muted-foreground">
              {config.backupEnabled ? "已开启" : "已关闭"}
            </span>
            <CapabilityToggle
              label="自动备份"
              enabled={config.backupEnabled}
              onToggle={() => void updateConfig({ backupEnabled: !config.backupEnabled })}
            />
          </div>
        </Row>
        {config.backupEnabled ? (
          <Row wide title="备份总上限" description="超限按最老先删的 LRU 清；0 = 不设上限">
            <div className="flex items-center justify-end gap-2">
              <input
                type="number"
                min={0}
                aria-label="备份总上限 MB"
                className={`${inputClass} w-24 text-sm`}
                value={config.backupTotalMb}
                onChange={(event) =>
                  void updateConfig({ backupTotalMb: Math.max(0, Number(event.target.value) || 0) })
                }
              />
              <span className="text-xs text-muted-foreground">MB</span>
              <Button
                variant="ghost"
                size="sm"
                onClick={() => void openBackupDir().catch(() => undefined)}
              >
                打开备份目录
              </Button>
            </div>
          </Row>
        ) : null}
        <Row
          title="敏感保护"
          description="工具结果进入对话之前就地打码：读盘读出来的凭据（密钥、token、私钥）不原样进上下文。只管工具结果——你自己打的字不打码（往输入框里贴密钥通常是故意的）"
        >
          <div className="flex items-center justify-end gap-2">
            <span className="text-xs text-muted-foreground">
              {config.secretScanEnabled ? "已开启" : "已关闭"}
            </span>
            <CapabilityToggle
              label="敏感保护"
              enabled={config.secretScanEnabled}
              onToggle={() =>
                void setSecretScan(
                  !config.secretScanEnabled,
                  config.disabledSecretRules,
                  config.customSecretRules,
                  config.secretRulePatternEdits,
                ).then(() => void updateConfig({ secretScanEnabled: !config.secretScanEnabled }))
              }
            />
          </div>
        </Row>
        {config.secretScanEnabled ? <SecretRulesLibrary /> : null}
      </Group>
    </FormColumn>
  );
}
