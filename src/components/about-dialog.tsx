import { useEffect, useState, type ReactNode } from "react";
import { getVersion } from "@tauri-apps/api/app";
import { invoke } from "@tauri-apps/api/core";
import { openUrl } from "@tauri-apps/plugin-opener";

import appIcon from "../../src-tauri/icons/128x128.png";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { Switch } from "@/components/ui/switch";
import { useChatStore } from "@/store/chat-store";
import { cn } from "@/lib/utils";

// 反馈与发版信息都挂在 GitHub 上。仓库还没公开前"检查更新"会拿到 404，
// 界面按"没查到发版"原样说——不假装有新版本，也不静默装没事
// GITHUB_PROFILE 同时被标题栏「帮助 → 问题反馈」复用，改地址只动这里
export const GITHUB_PROFILE = "https://github.com/Technicalflight";
const RELEASES_PAGE = "https://github.com/Technicalflight/aglab/releases";
const RELEASES_API = "https://api.github.com/repos/Technicalflight/aglab/releases/latest";

/** latest 是否比 current 新。容忍前缀 v 与缺段（缺的当 0 补） */
function isNewerVersion(latest: string, current: string): boolean {
  const parts = (value: string) =>
    value
      .trim()
      .replace(/^v/i, "")
      .split(".")
      .map((n) => Number.parseInt(n, 10) || 0);
  const next = parts(latest);
  const now = parts(current);
  for (let i = 0; i < 3; i += 1) {
    if ((next[i] ?? 0) !== (now[i] ?? 0)) return (next[i] ?? 0) > (now[i] ?? 0);
  }
  return false;
}

type UpdateState =
  | { kind: "idle" }
  | { kind: "checking" }
  | { kind: "latest"; message: string }
  | { kind: "newer"; message: string }
  | { kind: "error"; message: string };

/** 一行操作卡：标题 + 说明在左，动作在右（行样式对齐设置页） */
function Row({
  title,
  description,
  action,
}: {
  title: string;
  description: string;
  action: ReactNode;
}) {
  return (
    <div className="flex items-center justify-between gap-3 rounded-lg border border-border bg-surface px-3 py-3">
      <div className="min-w-0">
        <p className="text-sm font-medium text-foreground">{title}</p>
        <p className="mt-1 text-xs leading-5 text-muted-foreground">{description}</p>
      </div>
      {action}
    </div>
  );
}

export function AboutDialog({
  open,
  onOpenChange,
}: {
  open: boolean;
  onOpenChange: (next: boolean) => void;
}) {
  const [version, setVersion] = useState<string | null>(null);
  const [update, setUpdate] = useState<UpdateState>({ kind: "idle" });
  const [actionError, setActionError] = useState<string | null>(null);
  const config = useChatStore((s) => s.config);
  const updateConfig = useChatStore((s) => s.updateConfig);

  // 只在第一次打开时读，之后复用；读不到就明说读不到，不拿占位串冒充版本
  useEffect(() => {
    if (!open || version !== null) return;
    try {
      void getVersion().then(
        (value) => setVersion(value),
        () => setVersion("读取失败"),
      );
    } catch {
      setVersion("读取失败");
    }
  }, [open, version]);

  const checkUpdate = () => {
    setUpdate({ kind: "checking" });
    void fetch(RELEASES_API, { headers: { Accept: "application/vnd.github+json" } })
      .then(async (res) => {
        if (!res.ok) {
          setUpdate({
            kind: "error",
            message: `没查到发版（HTTP ${res.status}）：仓库还没有公开的发版。`,
          });
          return;
        }
        const data = (await res.json()) as { tag_name?: string };
        const tag = (data.tag_name ?? "").trim();
        if (!tag || !version) {
          setUpdate({ kind: "error", message: "检查失败：读不到版本号，稍后再试。" });
          return;
        }
        if (isNewerVersion(tag, version)) {
          setUpdate({
            kind: "newer",
            message: `发现新版本 ${tag.replace(/^v/i, "")}，更新内容见「发版日志」。`,
          });
        } else {
          setUpdate({ kind: "latest", message: `当前已是最新（${version}）。` });
        }
      })
      .catch((cause: unknown) => {
        const detail = cause instanceof Error ? cause.message : String(cause);
        setUpdate({
          kind: "error",
          message: `检查失败：${detail}。多半是网络或代理没放行 api.github.com。`,
        });
      });
  };

  // 打开失败要有一句话交代，不能点了没反应
  const openLogDir = () => {
    void invoke("open_log_dir").then(
      () => setActionError(null),
      (cause) => setActionError(`打开日志目录失败：${String(cause)}`),
    );
  };
  const openConsole = () => {
    void invoke("open_devtools").then(
      () => setActionError(null),
      (cause) => setActionError(`打开控制台失败：${String(cause)}`),
    );
  };
  const openLink = (url: string) => {
    void openUrl(url).then(
      () => setActionError(null),
      (cause) => setActionError(`打开链接失败：${String(cause)}`),
    );
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="w-[560px] max-w-[92vw]">
        <div className="flex items-center gap-3">
          <img src={appIcon} alt="aglab 图标" className="h-11 w-11 rounded-lg" />
          <div>
            <DialogTitle>关于 aglab</DialogTitle>
            <p className="mt-0.5 text-xs text-muted-foreground">版本 {version ?? "读取中"}</p>
          </div>
        </div>
        <p className="mt-3 text-sm leading-6 text-muted-foreground">
          本地优先的 AI Agent 工作台。对话、改代码、跑任务都在你自己的机器上完成，
          模型服务商由你配置，数据不出本机。
        </p>

        <div className="mt-5 max-h-[62vh] space-y-5 overflow-y-auto pr-1">
          <section className="space-y-2">
            <Row
              title="日志"
              description="工具调用的审计记录（JSONL），排查问题时会用到。"
              action={
                <Button variant="subtle" size="sm" onClick={openLogDir}>
                  打开日志
                </Button>
              }
            />
            <Row
              title="问题反馈"
              description="在 GitHub 上提 issue，或看看已知问题。"
              action={
                <Button variant="subtle" size="sm" onClick={() => openLink(GITHUB_PROFILE)}>
                  打开 GitHub
                </Button>
              }
            />
            <div>
              <Row
                title="软件更新"
                description="发新版时在这里说；更新方式目前是手动检查。"
                action={
                  <div className="flex shrink-0 items-center gap-2">
                    <Button variant="subtle" size="sm" onClick={() => openLink(RELEASES_PAGE)}>
                      发版日志
                    </Button>
                    <Button
                      variant="subtle"
                      size="sm"
                      loading={update.kind === "checking"}
                      onClick={checkUpdate}
                    >
                      检查更新
                    </Button>
                  </div>
                }
              />
              {update.kind !== "idle" && update.kind !== "checking" ? (
                <p
                  className={cn(
                    "mt-2 px-1 text-xs leading-5",
                    update.kind === "newer"
                      ? "text-brand-text"
                      : update.kind === "error"
                        ? "text-destructive"
                        : "text-muted-foreground",
                  )}
                >
                  {update.message}
                </p>
              ) : null}
            </div>
          </section>

          {/* 开发者设施只在这道开关后面出现：入口存在 ≠ 默认可见 */}
          <section className="space-y-2 border-t border-border pt-4">
            <h3 className="text-xs font-semibold tracking-tight">开发者</h3>
            <Row
              title="开发者模式"
              description="打开后显示开发者工具入口。日常使用不需要它。"
              action={
                <Switch
                  checked={config.devMode}
                  onCheckedChange={(checked) => void updateConfig({ devMode: checked })}
                  aria-label="开发者模式"
                />
              }
            />
            <Row
              title="开发者工具"
              description="打开 WebView2 控制台，看网络请求与控制台输出。"
              action={
                <Button variant="subtle" size="sm" disabled={!config.devMode} onClick={openConsole}>
                  打开控制台
                </Button>
              }
            />
          </section>

          {actionError ? (
            <p className="px-1 text-xs leading-5 text-destructive">{actionError}</p>
          ) : null}
        </div>

        <div className="mt-5 flex items-end justify-between gap-4">
          <p className="text-2xs leading-5 text-muted-foreground">
            更新方式目前为手动检查，后续接入自动更新。
          </p>
          <Button variant="ghost" size="sm" onClick={() => onOpenChange(false)}>
            关闭
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}
