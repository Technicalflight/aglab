import { useEffect, useState, type ReactNode } from "react";
import { getVersion } from "@tauri-apps/api/app";
import { Channel, invoke } from "@tauri-apps/api/core";
import { openUrl } from "@tauri-apps/plugin-opener";

import appIcon from "../../src-tauri/icons/128x128.png";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { Switch } from "@/components/ui/switch";
import { useChatStore } from "@/store/chat-store";
import { cn } from "@/lib/utils";

// 反馈与发版信息都挂在 GitHub 上。版本比较在后端做（清单语义归一处），
// 检查与下载走后端 + 应用代理池——前端直连 api.github.com 会被用户侧 hosts 挡掉（真机踩过）
// GITHUB_PROFILE 同时被标题栏「帮助 → 问题反馈」复用，改地址只动这里
export const GITHUB_PROFILE = "https://github.com/Technicalflight";
const CHANGELOG_PAGE = "https://technicalflight.github.io/aglab-site/changelog.html";

interface UpdateStatus {
  currentVersion: string;
  latestVersion: string | null;
  notes: string | null;
  hasUpdate: boolean;
}

/** 后端 update_install 的进度事件（serde tag=kind） */
type UpdateProgress =
  | { kind: "downloading"; received: number; total: number | null }
  | { kind: "installing" }
  | { kind: "done" };

type UpdateState =
  | { kind: "idle" }
  | { kind: "checking" }
  | { kind: "latest"; message: string }
  | { kind: "newer"; version: string; notes: string | null }
  | { kind: "downloading"; received: number; total: number | null }
  | { kind: "installing" }
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
    void invoke<UpdateStatus>("update_check").then(
      (status) => {
        if (status.hasUpdate && status.latestVersion) {
          setUpdate({ kind: "newer", version: status.latestVersion, notes: status.notes });
        } else {
          setUpdate({ kind: "latest", message: `当前已是最新（${status.currentVersion}）。` });
        }
      },
      (cause) => {
        const detail = cause instanceof Error ? cause.message : String(cause);
        setUpdate({
          kind: "error",
          message: `检查失败：${detail}。如果你的网络屏蔽了 GitHub，请在 设置 → 代理 配置代理后重试。`,
        });
      },
    );
  };

  // 下载并安装：进度从后端 Channel 流过来。Windows 安装器跑完会自动重启应用——
  // 进程退出就是成功，界面上"即将自动重启"是用户最后看到的一句话
  const installNow = () => {
    const channel = new Channel<UpdateProgress>();
    channel.onmessage = (progress) => {
      if (progress.kind === "downloading") {
        setUpdate({ kind: "downloading", received: progress.received, total: progress.total });
      } else if (progress.kind === "installing" || progress.kind === "done") {
        setUpdate({ kind: "installing" });
      }
    };
    setUpdate({ kind: "downloading", received: 0, total: null });
    void invoke("update_install", { onEvent: channel }).then(
      () => setUpdate({ kind: "installing" }),
      (cause) => {
        const detail = cause instanceof Error ? cause.message : String(cause);
        setUpdate({ kind: "error", message: `更新失败：${detail}` });
      },
    );
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
                description="有新版本会弹窗提醒；也可以在这里手动检查与一键更新。"
                action={
                  <div className="flex shrink-0 items-center gap-2">
                    <Button variant="subtle" size="sm" onClick={() => openLink(CHANGELOG_PAGE)}>
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
              {update.kind === "newer" ? (
                <div className="mt-2 rounded-lg border border-brand/30 bg-brand/8 px-3 py-2.5">
                  <p className="text-xs leading-5 text-foreground">
                    发现新版本{" "}
                    <span className="font-semibold text-brand-text">v{update.version}</span>
                    {update.notes ? (
                      <span className="mt-1 block whitespace-pre-wrap leading-5 text-muted-foreground">
                        {update.notes.length > 400 ? `${update.notes.slice(0, 400)}…` : update.notes}
                      </span>
                    ) : null}
                  </p>
                  <div className="mt-2">
                    <Button size="sm" onClick={installNow}>
                      立即更新
                    </Button>
                  </div>
                </div>
              ) : null}
              {update.kind === "downloading" ? (
                <div className="mt-2 px-1">
                  <div className="h-1.5 w-full overflow-hidden rounded-full bg-surface">
                    <div
                      className="h-full rounded-full bg-brand transition-[width]"
                      style={{
                        width: update.total
                          ? `${Math.min(100, Math.round((update.received / update.total) * 100))}%`
                          : "40%",
                      }}
                    />
                  </div>
                  <p className="mt-1.5 text-xs text-muted-foreground">
                    正在下载更新
                    {update.total
                      ? `（${Math.round(update.received / 1024)} / ${Math.round(update.total / 1024)} KB）`
                      : ""}
                    …
                  </p>
                </div>
              ) : null}
              {update.kind === "installing" ? (
                <p className="mt-2 px-1 text-xs leading-5 text-muted-foreground">
                  安装完成，应用即将自动重启…
                </p>
              ) : null}
              {update.kind === "latest" || update.kind === "error" ? (
                <p
                  className={cn(
                    "mt-2 px-1 text-xs leading-5",
                    update.kind === "error" ? "text-destructive" : "text-muted-foreground",
                  )}
                >
                  {update.message}
                </p>
              ) : null}
            </div>
            <Row
              title="自动检查更新"
              description="每 24 小时联网检查一次，发现新版本弹窗提醒。关掉后仍可在这里手动检查。"
              action={
                <Switch
                  checked={config.autoUpdateCheck}
                  onCheckedChange={(checked) => void updateConfig({ autoUpdateCheck: checked })}
                  aria-label="自动检查更新"
                />
              }
            />
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
            有新版本会弹窗提醒，一键更新后自动重启。更新包带签名校验。
          </p>
          <Button variant="ghost" size="sm" onClick={() => onOpenChange(false)}>
            关闭
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}
