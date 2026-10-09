import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

import { CcswitchMcpImport } from "@/components/ccswitch-mcp-import";
import { Button } from "@/components/ui/button";
import { CapabilityToggle } from "@/components/ui/capability-toggle";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { FormColumn } from "@/components/ui/content-column";
import { SettingsHeader } from "@/components/settings-ui";
import {
  clearToolRules,
  fetchToolRules,
  forgetToolRule,
  type RememberedRule,
} from "@/lib/chat-transport";
import {
  RISK_LABELS,
  type PermissionLevel,
  type PermissionOverride,
  type PermissionRow,
  type Project,
} from "@/types/chat";
import { useChatStore } from "@/store/chat-store";
import { PaginationBar, usePaged } from "@/components/pagination";
import { cn } from "@/lib/utils";

const LEVEL_LABEL: Record<PermissionLevel, string> = {
  deny: "拒绝",
  ask: "要确认",
  scoped: "范围内放行",
  allow: "放行",
};

function text_of(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}

/**
 * 一列覆盖项。全局与项目各用一份实例，差别只在它写回哪一格。
 *
 * 键的候选来自权限表本身，所以界面上写不出一个"存得进去但永远不命中"的键——
 * 那种键坏了是静默的：屏幕上写着"我拦了 git"，实际一行都没拦
 */
function OverrideList({
  items,
  keys,
  onSave,
  busy,
}: {
  items: PermissionOverride[];
  keys: string[];
  onSave: (next: PermissionOverride[]) => Promise<void>;
  busy: boolean;
}) {
  const [pickedKey, setPickedKey] = useState("");
  // 权限表是异步读回来的，所以"没挑过就用第一行"要每次渲染现算：写进 useState 会永远停在读回来之前
  const draftKey = pickedKey || keys[0] || "";
  const [draftLevel, setDraftLevel] = useState<PermissionLevel>("deny");
  const [error, setError] = useState<string | null>(null);

  async function run(next: PermissionOverride[]) {
    setError(null);
    try {
      await onSave(next);
    } catch (cause) {
      setError(text_of(cause));
    }
  }

  return (
    <div className="mt-1.5">
      {items.length === 0 ? (
        <p className="text-xs text-muted-foreground">还没有额外的行。</p>
      ) : (
        <ul className="space-y-1">
          {items.map((item) => (
            <li key={item.key} className="flex items-center gap-2 text-xs">
              <span className="min-w-0 flex-1 truncate font-mono text-foreground">{item.key}</span>
              <Select
                value={item.level}
                disabled={busy}
                onValueChange={(value) =>
                  void run(
                    items.map((held) =>
                      held.key === item.key ? { ...held, level: value as PermissionLevel } : held,
                    ),
                  )
                }
              >
                <SelectTrigger
                  aria-label={`${item.key} 的档位`}
                  className="h-7 w-auto shrink-0 gap-1.5 rounded-md px-2 text-xs"
                >
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {(Object.keys(LEVEL_LABEL) as PermissionLevel[]).map((level) => (
                    <SelectItem key={level} value={level} className="text-xs">
                      {LEVEL_LABEL[level]}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
              <button
                type="button"
                disabled={busy}
                className="rounded-md border border-input px-1.5 py-0.5 text-xs text-muted-foreground transition-colors hover:text-foreground"
                onClick={() => void run(items.filter((held) => held.key !== item.key))}
              >
                去掉
              </button>
            </li>
          ))}
        </ul>
      )}
      <div className="mt-1.5 flex items-center gap-2">
        <Select value={draftKey} disabled={busy || keys.length === 0} onValueChange={setPickedKey}>
          <SelectTrigger
            aria-label="要拦哪一行"
            className="h-7 min-w-0 flex-1 gap-1.5 rounded-md px-2 font-mono text-xs"
          >
            <SelectValue placeholder="权限表还没有行" />
          </SelectTrigger>
          <SelectContent>
            {keys.map((key) => (
              <SelectItem key={key} value={key} className="font-mono text-xs">
                {key}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <Select
          value={draftLevel}
          disabled={busy}
          onValueChange={(value) => setDraftLevel(value as PermissionLevel)}
        >
          <SelectTrigger
            aria-label="改成哪一档"
            className="h-7 w-auto shrink-0 gap-1.5 rounded-md px-2 text-xs"
          >
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {(Object.keys(LEVEL_LABEL) as PermissionLevel[]).map((level) => (
              <SelectItem key={level} value={level} className="text-xs">
                {LEVEL_LABEL[level]}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <button
          type="button"
          disabled={busy || !draftKey}
          className="h-7 rounded-md border border-input px-2 text-xs text-muted-foreground transition-colors hover:text-foreground"
          onClick={() =>
            void run([
              ...items.filter((held) => held.key !== draftKey),
              { key: draftKey, level: draftLevel },
            ])
          }
        >
          加一行
        </button>
      </div>
      {error ? <p className="mt-1 text-xs text-destructive">{error}</p> : null}
    </div>
  );
}

/** 生效中的权限表，以及两层收紧入口（全局 / 这个项目） */
function PermissionSection() {
  const config = useChatStore((state) => state.config);
  const updateConfig = useChatStore((state) => state.updateConfig);
  const [rows, setRows] = useState<PermissionRow[] | null>(null);
  const pagedRows = usePaged(rows ?? []);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const project = config.projects.find((item) => item.id === config.activeProjectId) ?? null;

  const refresh = useCallback(() => {
    invoke<PermissionRow[]>("permission_table")
      .then((value) => {
        setRows(value);
        setError(null);
      })
      .catch((cause) => setError(text_of(cause)));
  }, []);
  // 保存要过 config，config 一变就重读一次：表上那个数与刚加的那一行不会是两件事
  useEffect(refresh, [refresh, config]);

  const keys = (rows ?? []).map((row) => row.key);

  async function save(kind: "global" | "project", next: PermissionOverride[]) {
    setBusy(true);
    try {
      if (kind === "global") {
        await updateConfig({ permissionOverrides: next });
        return;
      }
      if (!project) return;
      const projects = config.projects.map(
        (item): Project => (item.id === project.id ? { ...item, permissionOverrides: next } : item),
      );
      await updateConfig({ projects });
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="mt-5">
      <p className="text-sm font-medium text-foreground">权限表</p>
      <p className="mt-1 text-xs leading-5 text-muted-foreground">
        这一刻每一项到底拦不拦。右边那格数字来自哪一层也写出来了——只给一个数的话，
        刚加的那一行生效没生效就只能靠猜。下面两格只能往里加更严的，撤不掉上面已经划的红线。
      </p>

      {error ? <p className="mt-2 text-xs text-destructive">权限表读不出来：{error}</p> : null}

      <ul className="mt-2 divide-y divide-border overflow-hidden rounded-lg border border-border bg-surface">
        {pagedRows.slice.map((row) => (
          <li key={row.key} className="flex items-baseline gap-2 px-3 py-1.5 text-xs">
            <span className="min-w-0 flex-1 truncate font-mono text-foreground">{row.key}</span>
            <span className="text-muted-foreground">{row.source}</span>
            <span
              className={cn(
                "shrink-0",
                row.level === "deny" ? "text-destructive" : "text-foreground",
              )}
            >
              {LEVEL_LABEL[row.level]}
            </span>
          </li>
        ))}
      </ul>
      <PaginationBar
        page={pagedRows.page}
        pages={pagedRows.pages}
        total={pagedRows.total}
        onPage={pagedRows.setPage}
      />

      <p className="mt-3 text-xs text-muted-foreground">全局额外收紧</p>
      <OverrideList
        items={config.permissionOverrides ?? []}
        keys={keys}
        busy={busy}
        onSave={(next) => save("global", next)}
      />

      <p className="mt-3 text-xs text-muted-foreground">
        {project ? `「${project.name}」额外收紧` : "这个项目：先绑定工作目录"}
      </p>
      {project ? (
        <OverrideList
          items={project.permissionOverrides ?? []}
          keys={keys}
          busy={busy}
          onSave={(next) => save("project", next)}
        />
      ) : null}

      <EgressList />
    </div>
  );
}

/** 只为把当前服务商那一家显示出来、并给一个"加进去"的按钮。判定不住在这儿——
 *  真正拦不拦是 Rust 那一份 `egress::permitted` 说的，这里复述一遍规则只为了少一个来回 */
function hostOf(url: string): string {
  const rest = url.split("://")[1] ?? url;
  const authority = rest.split(/[/?#]/)[0] ?? "";
  return (authority.split("@").pop() ?? "").trim().toLowerCase();
}

/** 网络出口的目标域名单（design-security-permission.md §16）。空 = 不收紧 */
function EgressList() {
  const config = useChatStore((s) => s.config);
  const updateConfig = useChatStore((s) => s.updateConfig);
  const allow = config.netEgressAllow;
  const provider = hostOf(config.baseUrl);
  const covered =
    allow.length === 0 ||
    allow.some((entry) => {
      const host = hostOf(entry);
      return host !== "" && (provider === host || provider.endsWith(`.${host}`));
    });

  return (
    <div className="mt-5">
      <p className="text-sm font-medium text-foreground">网络出口名单</p>
      <p className="mt-1 text-xs leading-5 text-muted-foreground">
        一行一个域名，按域后缀匹配：<span className="font-mono">example.com</span> 覆盖{" "}
        <span className="font-mono">api.example.com</span>，不覆盖{" "}
        <span className="font-mono">notexample.com</span>
        。管的是目标主机已知的那三处出口（模型请求、模型清单、 定时任务的通知）；MCP
        在这里是本地子进程，这份名单管不到它。
        <span className="font-medium text-foreground">留空 = 不收紧。</span>
        被名单拦下的动作不会挂成待批——一份可以被点通过的名单不是名单。
      </p>
      <textarea
        rows={3}
        spellCheck={false}
        value={allow.join("\n")}
        onChange={(event) =>
          void updateConfig({
            netEgressAllow: event.target.value.split("\n").map((line) => line.trim()),
          })
        }
        className="mt-2 w-full rounded-lg border border-input bg-background px-3 py-2 font-mono text-sm text-foreground outline-none transition-colors focus-visible:border-brand/50"
      />
      {allow.length > 0 && !covered ? (
        <div className="mt-2 flex items-center justify-between gap-3 rounded-lg border border-border bg-surface px-3 py-2">
          <p className="min-w-0 text-xs leading-5 text-destructive">
            当前服务商 <span className="font-mono">{provider || "（还没填）"}</span>{" "}
            不在名单里，模型请求会发不出去。
          </p>
          {provider ? (
            <Button
              variant="subtle"
              size="sm"
              onClick={() =>
                void updateConfig({
                  netEgressAllow: [...allow.filter((entry) => entry !== provider), provider],
                })
              }
            >
              加进去
            </Button>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}

/** 放行规则清单：话题内点过头的与「以后都允许」落过盘的，启动时在后端并成同一张表。
 *  这里是它们唯一的撤销口——列表里两类长得一样，"清空"就必须两类一起清 */
function AllowRulesSection() {
  const [rules, setRules] = useState<RememberedRule[] | null>(null);
  const pagedRules = usePaged(rules ?? []);
  const [busy, setBusy] = useState(false);

  const reload = useCallback(() => {
    fetchToolRules()
      .then(setRules)
      .catch(() => setRules(null));
  }, []);

  useEffect(() => {
    reload();
  }, [reload]);

  return (
    <div className="mt-6">
      <div className="flex items-center justify-between gap-3">
        <div>
          <h2 className="text-lg font-semibold tracking-tight text-foreground">放行规则</h2>
          <p className="mt-1 text-sm leading-6 text-muted-foreground">
            审批时点过头的动作在这里管理：撤销一条立即生效；「以后都允许」的那条会连配置一起摘掉。
          </p>
        </div>
        {rules && rules.length > 0 ? (
          <Button
            variant="subtle"
            size="sm"
            disabled={busy}
            onClick={() => {
              setBusy(true);
              clearToolRules()
                .then(reload)
                .finally(() => setBusy(false));
            }}
          >
            全部撤销
          </Button>
        ) : null}
      </div>
      {rules === null ? null : rules.length === 0 ? (
        <p className="mt-3 rounded-lg border border-border bg-surface px-3 py-2.5 text-xs leading-5 text-muted-foreground">
          还没有放行过的动作。审批框上点「本话题内允许」或「以后都允许」，这里就会多一行。
        </p>
      ) : (
        <>
          <ul className="mt-3 divide-y divide-border overflow-hidden rounded-lg border border-border bg-surface">
            {pagedRules.slice.map((rule) => (
              <li key={rule.key} className="flex items-center gap-3 px-3 py-2.5">
                <span className="min-w-0 flex-1 truncate text-sm text-foreground">
                  {rule.label}
                </span>
                <Button
                  variant="subtle"
                  size="sm"
                  disabled={busy}
                  onClick={() => {
                    setBusy(true);
                    forgetToolRule(rule.key)
                      .then(reload)
                      .finally(() => setBusy(false));
                  }}
                >
                  撤销
                </Button>
              </li>
            ))}
          </ul>
          <PaginationBar
            page={pagedRules.page}
            pages={pagedRules.pages}
            total={pagedRules.total}
            onPage={pagedRules.setPage}
          />
        </>
      )}
    </div>
  );
}

export function ToolsView() {
  const tools = useChatStore((s) => s.builtinTools);
  const toolsError = useChatStore((s) => s.toolsError);
  const toggleTool = useChatStore((s) => s.toggleTool);
  const config = useChatStore((s) => s.config);
  const pagedTools = usePaged(tools);

  const bound = config.projects.some((project) => project.id === config.activeProjectId);
  const enabledCount = tools.filter((tool) => tool.enabled).length;

  return (
    <FormColumn>
      <SettingsHeader
        title="工具管控"
        description={`${enabledCount} / ${tools.length} 项开启。工具是模型能自己调用的执行动作，也是它碰这台机器的文件与命令行的唯一入口：关掉的工具不再声明给它，看不见就不会去调；开着时仍按权限档位（当前：${
          config.permission === "ask"
            ? "逐项确认"
            : config.permission === "auto"
              ? "自动放行"
              : "完全访问"
        }）过闸。`}
      />
      <CcswitchMcpImport />

      {!bound ? (
        <p className="mt-3 rounded-lg border border-border bg-surface px-3 py-2.5 text-xs leading-5 text-muted-foreground">
          这些工具以生效的工作目录目录为基准：绑定了就是那个项目；现在还没绑定，
          <span className="text-foreground">以你的用户主目录为基准</span>
          （写入范围更大）。建议用输入框上方的「选择工作目录」给项目绑一个目录。
        </p>
      ) : null}

      <ul className="mt-4 divide-y divide-border overflow-hidden rounded-lg border border-border bg-surface">
        {pagedTools.slice.map((tool) => (
          <li key={tool.id} className="flex items-start gap-3 px-3 py-3">
            <div className="min-w-0 flex-1">
              <p className="flex items-baseline gap-2 text-base">
                <span className="font-medium text-foreground">{tool.title}</span>
                <span
                  className={cn(
                    "text-xs",
                    tool.risk === "high" ? "text-destructive" : "text-muted-foreground",
                  )}
                >
                  {RISK_LABELS[tool.risk]} · <span className="font-mono">{tool.id}</span>
                </span>
              </p>
              <p className="mt-1 text-xs leading-5 text-muted-foreground">{tool.blurb}</p>
            </div>
            <CapabilityToggle
              label={tool.title}
              enabled={tool.enabled}
              onToggle={() => void toggleTool(tool.id, !tool.enabled)}
            />
          </li>
        ))}
      </ul>
      <PaginationBar
        page={pagedTools.page}
        pages={pagedTools.pages}
        total={pagedTools.total}
        onPage={pagedTools.setPage}
      />

      <PermissionSection />

      <AllowRulesSection />

      <p className="mt-3 text-xs leading-5 text-muted-foreground">
        三格分工：工具给动作（模型自己调），技能给做法（重复的清单，按需取），插件给容器（一次装进技能和
        MCP 服务），MCP 给连接（数据库、API、网页这些外部服务）。
      </p>

      {toolsError ? <p className="mt-2 text-xs text-destructive">{toolsError}</p> : null}
    </FormColumn>
  );
}
