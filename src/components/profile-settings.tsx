import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { IconPencil as Pencil, IconPlus as Plus, IconTrash as Trash2 } from "@tabler/icons-react";

import { Button } from "@/components/ui/button";
import { ProfileDialog, type ProfileDialogTarget } from "@/components/profile-dialog";
import type { EndpointProfile } from "@/types/chat";
import { PaginationBar, usePaged } from "@/components/pagination";
import { ProviderIcon } from "@/components/model-icon";
import { useChatStore } from "@/store/chat-store";
import { FormColumn } from "@/components/ui/content-column";

const FORMAT_LABEL: Record<string, string> = {
  chat: "Chat · /chat/completions",
  responses: "Responses · /responses",
  anthropic: "Anthropic · /v1/messages",
};

/** base_url 只露主机名，卡片上放不下整个 URL */
function host_of(url: string): string {
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
}

/**
 * 设置页的「服务商档案」项：每张卡片是一套可整体切换的连接配置（含密钥凭据目标）；
 * 编辑走弹窗，弹窗里保存哪个卡片就写哪个卡片——正在使用的卡片保存后立即生效。
 */
export function ProfileSettings() {
  return (
    <FormColumn>
      <h1 className="text-2xl font-semibold tracking-tight text-foreground">服务商档案</h1>
      <p className="mt-1 text-sm leading-6 text-muted-foreground">
        1. 「新建档案」：填服务地址、密钥、模型；
        2. 点「切换」整套换过去（密钥一起换，只进 Windows 凭据管理器），不用删了重配。
      </p>
      <div className="mt-8">
        <ProfileCards />
      </div>
    </FormColumn>
  );
}

/** 弹窗的打开状态与表单本体在 profile-dialog.tsx：那张表比这一页大，分文件放 */
type DialogTarget = ProfileDialogTarget;

function ProfileCards() {
  const config = useChatStore((s) => s.config);
  // 每页 6 对齐 2 列网格（2×3）：6 张档案一页铺满、翻页条自动隐藏——
  // 默认的 5 会把第 6 张孤零零切到第二页，还顶着全套翻页控件
  const pagedProfiles = usePaged(config.profiles, 6);
  const refreshConnection = useChatStore((s) => s.refreshConnection);

  const [dialog, setDialog] = useState<DialogTarget>(null);
  const [profileError, setProfileError] = useState<string | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  // 两步确认删除：第一下只进入待确认态，3 秒内再点一下才真删
  const [confirmDeleteId, setConfirmDeleteId] = useState<string | null>(null);

  useEffect(() => {
    if (!confirmDeleteId) return;
    const timer = setTimeout(() => setConfirmDeleteId(null), 3000);
    return () => clearTimeout(timer);
  }, [confirmDeleteId]);

  function error_text(error: unknown): string {
    return error instanceof Error ? error.message : String(error);
  }

  async function switchTo(id: string) {
    setBusyId(id);
    setProfileError(null);
    try {
      await invoke("profile_switch", { id });
      await refreshConnection();
    } catch (error) {
      setProfileError(error_text(error));
    } finally {
      setBusyId(null);
    }
  }

  async function remove(id: string) {
    if (confirmDeleteId !== id) {
      setConfirmDeleteId(id);
      return;
    }
    setBusyId(id);
    setProfileError(null);
    try {
      await invoke("profile_delete", { id });
      setConfirmDeleteId(null);
      await refreshConnection();
    } catch (error) {
      setProfileError(error_text(error));
    } finally {
      setBusyId(null);
    }
  }

  return (
    <div className="mt-4">
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        {pagedProfiles.slice.map((profile) => (
          <ProfileCard
            key={profile.id}
            profile={profile}
            active={profile.id === config.activeProfileId}
            busy={busyId === profile.id}
            confirmingDelete={confirmDeleteId === profile.id}
            onEdit={() => setDialog({ mode: "edit", profile })}
            onSwitch={() => void switchTo(profile.id)}
            onDelete={() => void remove(profile.id)}
          />
        ))}

        {/* 新建：预填当前生效的连接字段，保存后立即启用。
            只在最后一页渲染——每页都摆一张的话，翻页会看到两个一模一样的「新建」 */}
        {pagedProfiles.page === pagedProfiles.pages - 1 ? (
          <button
            type="button"
            onClick={() => setDialog({ mode: "create" })}
            className="flex min-h-[120px] flex-col items-center justify-center gap-1.5 rounded-lg border border-dashed border-border bg-surface text-muted-foreground transition-colors hover:border-brand/50 hover:text-brand-text"
          >
            <Plus className="size-4" />
            <span className="text-sm">新建配置</span>
            <span className="px-4 text-center text-2xs leading-4">
              以当前生效的连接为底稿，保存后立即启用
            </span>
          </button>
        ) : null}
      </div>
      <PaginationBar page={pagedProfiles.page} pages={pagedProfiles.pages} total={pagedProfiles.total} onPage={pagedProfiles.setPage} />

      {profileError ? (
        <p className="mt-3 text-xs leading-5 text-destructive">{profileError}</p>
      ) : null}

      <ProfileDialog
        key={dialog?.mode === "edit" ? dialog.profile.id : dialog?.mode ?? "closed"}
        target={dialog}
        onClose={() => setDialog(null)}
      />
    </div>
  );
}

function ProfileCard({
  profile,
  active,
  busy,
  confirmingDelete,
  onEdit,
  onSwitch,
  onDelete,
}: {
  profile: EndpointProfile;
  active: boolean;
  busy: boolean;
  confirmingDelete: boolean;
  onEdit: () => void;
  onSwitch: () => void;
  onDelete: () => void;
}) {
  return (
    <div
      className={`rounded-lg border px-3 py-3 transition-colors ${
        active
          ? "border-brand/60 bg-brand/5"
          : "border-border bg-surface hover:border-brand/40"
      }`}
    >
      <div className="flex items-start justify-between gap-2">
        <div className="flex min-w-0 items-center gap-2">
          <span className="flex min-w-0 items-center gap-2">
            <ProviderIcon baseUrl={profile.baseUrl} apiFormat={profile.apiFormat} model={profile.model} size={15} />
            <span className="truncate text-base font-medium text-foreground">{profile.name}</span>
          </span>
          {active ? (
            <span className="shrink-0 rounded-full bg-brand/15 px-2 py-0.5 text-2xs font-medium text-brand-text">
              使用中
            </span>
          ) : null}
        </div>

        <div className="flex shrink-0 gap-1">
          <Button variant="subtle" size="icon" aria-label={`编辑 ${profile.name}`} onClick={onEdit}>
            <Pencil className="size-3.5" />
          </Button>
          <Button
            variant="subtle"
            size="icon"
            aria-label={confirmingDelete ? `再点一次确认删除 ${profile.name}` : `删除 ${profile.name}`}
            disabled={busy}
            className={confirmingDelete ? "text-destructive" : undefined}
            onClick={onDelete}
          >
            <Trash2 className="size-3.5" />
          </Button>
        </div>
      </div>

      <div className="mt-2 space-y-0.5">
        <p className="truncate font-mono text-sm text-foreground">
          {profile.model}
          {/* 一套连接可以挂好几个模型：只报默认那个会看不出这张档案还管着别的模型 */}
          {profile.models.length > 1 ? (
            <span className="ml-1.5 font-sans text-xs text-muted-foreground">
              等 {profile.models.length} 个模型
            </span>
          ) : null}
        </p>
        <p className="truncate text-xs text-muted-foreground" title={profile.baseUrl}>
          {host_of(profile.baseUrl)}
        </p>
      </div>

      <div className="mt-2.5 flex items-center justify-between gap-2">
        <span className="truncate rounded-full bg-muted-foreground/10 px-2 py-0.5 text-2xs text-muted-foreground">
          {FORMAT_LABEL[profile.apiFormat] ?? profile.apiFormat}
        </span>
        {active ? (
          <Button variant="subtle" size="sm" disabled>
            使用中
          </Button>
        ) : (
          <Button variant="subtle" size="sm" disabled={busy} onClick={onSwitch}>
            {busy ? "切换中…" : "切换"}
          </Button>
        )}
      </div>
    </div>
  );
}

/**
 * 档案编辑弹窗。保存哪个卡片就写哪个卡片：
 * - 编辑"使用中"的卡片 → 档案与顶层连接一起更新（保存后凭据目标可能变了，要重探密钥）；
 * - 编辑其他卡片 → 只更新那张卡片，当前连接不受影响；
 * - 新建 → 建档并立即启用。
 * 密钥随弹窗一起保存：留空则不动该档案已保存的密钥。
 */
