import { useState, type ReactNode } from "react";

import { browserClearAll, browserClearCache } from "@/lib/chat-transport";

import { Button } from "@/components/ui/button";
import { CapabilityToggle } from "@/components/ui/capability-toggle";
import { useChatStore } from "@/store/chat-store";
import { FormColumn } from "@/components/ui/content-column";

function Field({ title, hint, children }: { title: string; hint: ReactNode; children: ReactNode }) {
  return (
    <div className="flex items-center justify-between gap-4 rounded-xl border border-border bg-surface px-4 py-3.5">
      <div className="min-w-0">
        <p className="text-base font-medium text-foreground">{title}</p>
        <p className="mt-1 text-xs leading-5 text-muted-foreground">{hint}</p>
      </div>
      <div className="shrink-0">{children}</div>
    </div>
  );
}

function GroupTitle({ children }: { children: ReactNode }) {
  return (
    <p className="mt-6 text-xs font-medium tracking-[0.08em] text-foreground-tertiary uppercase">
      {children}
    </p>
  );
}

/**
 * 设置页的「浏览器控制」项（design-browser-control.md §5）：
 * 总开关 + 证书校验开关 + 两档数据清除。浏览器用本机 Chrome/Edge 拉起、
 * 独立配置目录，与日常浏览器不共享登录态。
 */
export function BrowserSettings() {
  const config = useChatStore((s) => s.config);
  const updateConfig = useChatStore((s) => s.updateConfig);
  const [result, setResult] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  // 清除全部不可撤销：第一下只进入待确认态，3 秒内再点一下才真删
  const [confirmWipe, setConfirmWipe] = useState(false);

  const run = async (run_: () => Promise<string>) => {
    setError(null);
    setResult(null);
    try {
      setResult(await run_());
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    }
  };

  return (
    <FormColumn>
      <h1 className="text-2xl font-semibold tracking-tight text-foreground">浏览器控制</h1>
      <p className="mt-1 text-sm leading-6 text-muted-foreground">
        1. 打开下方的「内置浏览器控制」开关； 2. 在对话里让模型打开网址、按快照里的编号点击与输入；
        3. 浏览器用本机 Chrome 或 Edge 拉起，窗口可见、随时可接管。
      </p>

      <div className="mt-6 space-y-3">
        <CapabilityToggle
          label="开启内置浏览器控制"
          enabled={config.browserControlEnabled}
          onToggle={() =>
            void updateConfig({ browserControlEnabled: !config.browserControlEnabled })
          }
        />
        <p className="text-xs leading-5 text-muted-foreground">
          开启后新发消息的话题就有这个工具；关掉后工具不再声明。已开着的浏览器窗口不受影响，
          关掉它下次按新值走。
        </p>
      </div>

      <GroupTitle>安全</GroupTitle>
      <div className="mt-2 space-y-3">
        <Field
          title="忽略证书校验"
          hint={
            <>
              开启后内置浏览器不再校验 HTTPS 证书，仅影响内置浏览器。
              <span className="text-foreground">修改后需重启生效</span>
              （关掉内置浏览器的窗口，下次工具调用会重新拉起）。
            </>
          }
        >
          <CapabilityToggle
            label="忽略证书校验"
            enabled={config.browserIgnoreCertErrors}
            onToggle={() =>
              void updateConfig({ browserIgnoreCertErrors: !config.browserIgnoreCertErrors })
            }
          />
        </Field>
      </div>

      <GroupTitle>浏览器数据</GroupTitle>
      <div className="mt-2 space-y-3">
        <Field
          title="清除内置浏览器缓存"
          hint="清除 HTTP 缓存、Cache Storage 和 Service Worker，保留 Cookie 和本地站点数据。"
        >
          <Button variant="subtle" size="sm" onClick={() => void run(browserClearCache)}>
            清除缓存
          </Button>
        </Field>
        <Field
          title="清除全部浏览器数据"
          hint={
            <span>
              删除内置浏览器中的 Cookie、站点数据和缓存。此操作不可撤销。 浏览器开着会先关掉。
            </span>
          }
        >
          {confirmWipe ? (
            <Button
              variant="subtle"
              size="sm"
              className="text-destructive"
              onClick={() => {
                setConfirmWipe(false);
                void run(browserClearAll);
              }}
            >
              再点一次确认
            </Button>
          ) : (
            <Button
              variant="subtle"
              size="sm"
              className="text-destructive"
              onClick={() => {
                setConfirmWipe(true);
                window.setTimeout(() => setConfirmWipe(false), 3000);
              }}
            >
              清除全部
            </Button>
          )}
        </Field>
      </div>

      {result ? <p className="mt-4 text-xs leading-5 text-muted-foreground">{result}</p> : null}
      {error ? <p className="mt-4 text-xs leading-5 text-destructive">{error}</p> : null}
    </FormColumn>
  );
}
