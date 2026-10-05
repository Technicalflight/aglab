import { useEffect } from "react";

import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { STORAGE_OPTIONS, type ConversationStore } from "@/types/chat";
import { useChatStore } from "@/store/chat-store";
import { FormColumn } from "@/components/ui/content-column";

/**
 * 设置页的「本地存储」项：话题记录落在哪种介质上。
 * 与连接设置分开——换服务商的人未必关心存储，管存储的人未必想看到密钥栏。
 */
export function StorageSettings() {
  const config = useChatStore((s) => s.config);
  const storage = useChatStore((s) => s.storage);
  const storageBusy = useChatStore((s) => s.storageBusy);
  const storageNote = useChatStore((s) => s.storageNote);
  const storageError = useChatStore((s) => s.storageError);
  const refreshStorage = useChatStore((s) => s.refreshStorage);
  const switchStorage = useChatStore((s) => s.switchStorage);

  useEffect(() => {
    void refreshStorage();
  }, [refreshStorage]);

  return (
    <FormColumn>
      <h1 className="text-2xl font-semibold tracking-tight text-foreground">本地存储</h1>

      <p className="mt-2 text-sm leading-6 text-muted-foreground">
        对话记录始终写在这台电脑上。切换方式时会把另一侧缺的话题拷过来，两边都不会被删除。
        服务商、参数和密钥不受影响：设置永远写在 <span className="font-mono">config.json</span>，
        密钥只在 Windows 凭据管理器。
      </p>

      <div className="mt-4">
        <Select
          value={config.conversationStore}
          onValueChange={(value) => void switchStorage(value as ConversationStore)}
          disabled={storageBusy}
        >
          <SelectTrigger>
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {STORAGE_OPTIONS.map((option) => (
              <SelectItem key={option.value} value={option.value}>
                {option.label}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>

      {storage ? (
        <dl className="mt-3 space-y-1.5 rounded-lg border border-border bg-background px-3 py-2.5">
          {STORAGE_OPTIONS.map((option) => {
            const active = storage.backend === option.value;
            const location = option.value === "json" ? storage.jsonDir : storage.sqliteFile;
            const count = option.value === "json" ? storage.jsonCount : storage.sqliteCount;
            return (
              <div key={option.value} className="flex items-baseline justify-between gap-3">
                <dt className={active ? "shrink-0 text-foreground" : "shrink-0"}>
                  {option.short}
                  {active ? <span className="ml-1 text-xs text-brand-text">当前</span> : null}
                </dt>
                <dd className="flex min-w-0 items-baseline gap-2 text-xs">
                  <span className="break-all font-mono text-muted-foreground">{location}</span>
                  <span className="shrink-0 text-muted-foreground">{count} 条</span>
                </dd>
              </div>
            );
          })}
        </dl>
      ) : null}

      <ul className="mt-3 space-y-2">
        {STORAGE_OPTIONS.map((option) => (
          <li key={option.value} className="rounded-lg border border-border bg-surface px-3 py-2.5">
            <p className="text-sm font-medium text-foreground">{option.label}</p>
            <p className="mt-1 text-xs leading-5 text-muted-foreground">
              <span className="text-foreground">好处 </span>
              {option.pros}
            </p>
            <p className="mt-1 text-xs leading-5 text-muted-foreground">
              <span className="text-foreground">代价 </span>
              {option.cons}
            </p>
          </li>
        ))}
      </ul>

      {storageNote ? (
        <p className="mt-2 text-xs leading-5 text-brand-text">{storageNote}</p>
      ) : storageError ? (
        <p className="mt-2 text-xs leading-5 text-destructive">{storageError}</p>
      ) : null}
    </FormColumn>
  );
}
