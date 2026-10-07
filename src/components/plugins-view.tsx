import { useCallback, useEffect, useState, type ReactNode } from "react";
import { IconChevronLeft as ChevronLeft, IconFolderOpen as FolderOpen, IconRefresh as RefreshCw, IconShieldX as ShieldAlert } from "@tabler/icons-react";
import { revealItemInDir } from "@tauri-apps/plugin-opener";

import { Button } from "@/components/ui/button";
import { CapabilityToggle } from "@/components/ui/capability-toggle";
import { SectionFrame } from "@/components/section-frame";
import type { BuiltinView, HookView, PluginView } from "@/types/chat";
import { hookState } from "@/types/chat";
import { useChatStore } from "@/store/chat-store";
import { PaginationBar, usePaged } from "@/components/pagination";
import { cn } from "@/lib/utils";
import { ListSkeleton } from "@/components/ui/loading-skeleton";
import { pluginMarketList, pluginMarketInstall } from "@/lib/chat-transport";
import type { MarketEntry, MarketView, WorkspaceHooksView } from "@/types/chat";

function message(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}

/** 工作区钩子：当前工作目录的 hooks.json。信任按目录锚定，逐条确认后才会执行 */
function WorkspaceHooksSection({
  view,
  onAct,
  onChanged,
}: {
  view: WorkspaceHooksView;
  onAct: (run: () => Promise<unknown>) => void;
  onChanged: () => Promise<void>;
}) {
  const waiting = view.hooks.filter((hook) => hook.supported && (!hook.trusted || !hook.current));
  return (
    <div className="mt-6 border-t border-border pt-5">
      <h2 className="flex items-baseline gap-2 text-base font-semibold tracking-tight text-foreground">
        工作区钩子
        {waiting.length > 0 ? (
          <span className="text-xs font-normal text-destructive">{waiting.length} 条待确认</span>
        ) : null}
      </h2>
      <p className="mt-1 text-xs leading-5 text-muted-foreground">
        定义来自当前工作目录（<span className="break-all font-mono">{view.root}</span>
        ）。内容是仓库带来的第三方脚本：信任按这个目录锚定——同名的两个项目互不顶替，
        文件一改指纹就失配，撤回确认即时生效。
      </p>
      {view.notes.length > 0 ? (
        <ul className="mt-2 space-y-1">
          {view.notes.map((note) => (
            <li key={note} className="text-2xs leading-4 text-muted-foreground/80">
              {note}
            </li>
          ))}
        </ul>
      ) : null}
      {view.hooks.length === 0 ? (
        <p className="mt-2 text-xs text-muted-foreground">这个工作目录里没有 hooks.json。</p>
      ) : (
        <ul className="mt-2 space-y-2">
          {view.hooks.map((hook) => (
            <HookRow
              key={hook.id}
              hook={{ ...hook, file: hook.file || view.root }}
              onAct={(run) => onAct(async () => {
                await run();
                await onChanged();
              })}
            />
          ))}
        </ul>
      )}
    </div>
  );
}

/** 官方市场：官网发布的插件清单，安装走 CDN zip + sha256 指纹校验 */
function MarketSection({
  market,
  busy,
  note,
  onInstall,
}: {
  market: MarketView | null;
  busy: string | null;
  note: string | null;
  onInstall: (entry: MarketEntry) => void;
}) {
  if (market === null) return null;
  return (
    <div className="mt-6 border-t border-border pt-5">
      <h2 className="text-base font-semibold tracking-tight text-foreground">官方市场</h2>
      <p className="mt-1 text-xs leading-5 text-muted-foreground">
        来自 aglab 官网发布的插件清单。安装 = 下载 zip →
        先校验 <span className="font-mono">sha256</span> 指纹 →
        再解压进插件目录；指纹对不上整包拒绝。
      </p>
      {market.entries.length === 0 ? (
        <p className="mt-2 text-xs text-muted-foreground">清单暂时是空的。</p>
      ) : (
        <ul className="mt-2 space-y-2">
          {market.entries.map((entry) => {
            const installed = market.installedIds.includes(entry.id);
            return (
              <li
                key={entry.id}
                className="flex items-start gap-3 rounded-lg border border-border bg-surface px-3 py-3"
              >
                <div className="min-w-0 flex-1">
                  <p className="flex items-baseline gap-2 text-sm">
                    <span className="font-medium text-foreground">{entry.name}</span>
                    {entry.version ? (
                      <span className="font-mono text-xs text-muted-foreground">v{entry.version}</span>
                    ) : null}
                    {installed ? (
                      <span className="text-xs text-brand-text">已安装</span>
                    ) : null}
                  </p>
                  {entry.description ? (
                    <p className="mt-1 text-xs leading-5 text-muted-foreground">{entry.description}</p>
                  ) : null}
                </div>
                <Button
                  variant="subtle"
                  size="sm"
                  disabled={busy !== null || installed}
                  onClick={() => onInstall(entry)}
                >
                  {busy === entry.id ? "安装中…" : installed ? "已安装" : "安装"}
                </Button>
              </li>
            );
          })}
        </ul>
      )}
      {note ? <p className="mt-2 text-xs text-brand-text">{note}</p> : null}
    </div>
  );
}

/** 分区标题下面那行说明。三段式排版沿用技能/工具分区 */
function Lead({ children }: { children: ReactNode }) {
  return <p className="text-sm leading-6 text-muted-foreground">{children}</p>;
}

function GroupTitle({ children, note }: { children: ReactNode; note?: string }) {
  return (
    <div className="mt-6 flex items-baseline gap-2 border-t border-border pt-5">
      <h2 className="text-base font-semibold tracking-tight text-foreground">{children}</h2>
      {note ? <span className="text-xs text-muted-foreground">{note}</span> : null}
    </div>
  );
}

function HookRow({
  hook,
  onAct,
}: {
  hook: HookView;
  onAct: (run: () => Promise<unknown>) => void;
}) {
  const trustHook = useChatStore((s) => s.trustHook);
  const toggleHook = useChatStore((s) => s.toggleHook);
  const state = hookState(hook);
  const needsAttention = hook.supported && (!hook.trusted || !hook.current);

  return (
    <li className="rounded-lg border border-border bg-surface px-3 py-3">
      <div className="flex items-start gap-3">
        <div className="min-w-0 flex-1">
          <p className="flex flex-wrap items-baseline gap-x-2 gap-y-1">
            <span className="font-mono text-sm text-foreground">{hook.event}</span>
            {hook.matcher ? (
              <span className="font-mono text-xs text-muted-foreground">
                只筛 <span className="text-foreground">{hook.matcher}</span>
              </span>
            ) : (
              <span className="text-xs text-muted-foreground">不限工具</span>
            )}
            <span className="text-xs text-muted-foreground">{hook.timeout}s 超时</span>
            <span
              className={cn(
                "shrink-0 text-xs",
                hook.runs
                  ? "text-brand-text"
                  : needsAttention
                    ? "text-destructive"
                    : "text-muted-foreground",
              )}
            >
              {state.label}
            </span>
          </p>

          {hook.statusMessage ? (
            <p className="mt-1 text-xs leading-5 text-muted-foreground">{hook.statusMessage}</p>
          ) : null}

          {/* 命令原文要摊开给人看：确认的前提就是看得见要跑什么 */}
          <p className="mt-2 rounded-lg border border-border bg-background px-2.5 py-2 font-mono text-xs leading-5 break-all whitespace-pre-wrap text-foreground">
            {hook.command}
          </p>

          <p className="mt-2 text-xs leading-5 text-muted-foreground">{state.hint}</p>
        </div>

        <div className="flex shrink-0 flex-col items-end gap-2">
          {needsAttention ? (
            <Button
              variant={hook.current ? "brand" : "subtle"}
              size="sm"
              onClick={() => void onAct(() => trustHook(hook.id, hook.hash, true))}
            >
              {hook.trusted ? "重新确认" : "确认这段脚本"}
            </Button>
          ) : hook.trusted && hook.supported ? (
            <Button
              variant="ghost"
              size="sm"
              onClick={() => void onAct(() => trustHook(hook.id, hook.hash, false))}
            >
              撤回确认
            </Button>
          ) : null}

          <CapabilityToggle
            label={hook.event}
            enabled={hook.enabled}
            onToggle={() => void onAct(() => toggleHook(hook.id, !hook.enabled))}
          />

          <Button
            variant="ghost"
            size="sm"
            className="h-7 text-xs"
            aria-label="在文件夹里找到这个钩子文件"
            onClick={() => void revealItemInDir(hook.file).catch(() => undefined)}
          >
            找文件
          </Button>
        </div>
      </div>
    </li>
  );
}

function PluginDetail({
  plugin,
  onBack,
  onAct,
}: {
  plugin: PluginView;
  onBack: () => void;
  onAct: (run: () => Promise<unknown>) => void;
}) {
  const toggleSkill = useChatStore((s) => s.toggleSkill);
  const togglePlugin = useChatStore((s) => s.togglePlugin);
  const waiting = plugin.hooks.filter((hook) => hook.supported && (!hook.trusted || !hook.current));
  const running = plugin.hooks.filter((hook) => hook.runs).length;
  const ignored = [
    plugin.commands > 0 ? `命令 ${plugin.commands} 个` : null,
    plugin.agents > 0 ? `代理 ${plugin.agents} 个` : null,
  ].filter(Boolean);

  return (
    <div>
      <button
        type="button"
        onClick={onBack}
        className="-ml-1.5 flex items-center gap-1 rounded-lg px-1.5 py-1 text-xs text-muted-foreground transition-colors hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/45"
      >
        <ChevronLeft className="size-3.5" />
        插件列表
      </button>

      <div className="mt-2 flex items-start gap-3">
        <div className="min-w-0 flex-1">
          <h2 className="flex items-baseline gap-2 text-lg font-semibold tracking-tight text-foreground">
            <span className="min-w-0 truncate">{plugin.name}</span>
            {plugin.version ? (
              <span className="shrink-0 font-mono text-xs font-normal text-muted-foreground">
                v{plugin.version}
              </span>
            ) : null}
          </h2>
          {plugin.description ? (
            <p className="mt-1.5 text-sm leading-6 text-muted-foreground">
              {plugin.description}
            </p>
          ) : null}
        </div>
        <CapabilityToggle
          label={plugin.name}
          enabled={plugin.enabled}
          onToggle={() => void onAct(() => togglePlugin(plugin.id, !plugin.enabled))}
        />
      </div>

      {waiting.length > 0 ? (
        <p className="mt-3 flex items-start gap-2 rounded-lg border border-border bg-surface px-3 py-2.5 text-xs leading-5 text-muted-foreground">
          <ShieldAlert className="mt-0.5 size-3.5 shrink-0 text-destructive" />
          <span>
            这个插件带了 <span className="text-foreground">{waiting.length} 条</span>
            要你亲自看过才肯跑的脚本。钩子拿到的是本进程的全部权限，能读你的文件、联网、删东西；
            没确认过的不会被执行，确认过后脚本内容改一个字，那份确认也会自动作废。
          </span>
        </p>
      ) : null}

      <GroupTitle note={`${plugin.skills.length} 个`}>技能</GroupTitle>
      <Lead>
        技能只是一份份操作清单，模型看着名字自己决定要不要取用，取到了才读正文。
        关掉哪条，它就不会再出现在清单里。
      </Lead>
      {plugin.skills.length === 0 ? (
        <p className="mt-3 text-sm text-muted-foreground">这个插件没带技能。</p>
      ) : (
        <ul className="mt-3 space-y-2">
          {plugin.skills.map((skill) => (
            <li
              key={skill.id}
              className="flex items-start gap-3 rounded-lg border border-border bg-surface px-3 py-2.5"
            >
              <div className="min-w-0 flex-1">
                <p className="flex items-baseline gap-2 text-base">
                  <span className="font-medium text-foreground">{skill.name}</span>
                  <span className="text-xs text-muted-foreground">{skill.chars} 字</span>
                </p>
                <p className="mt-1 text-xs leading-5 text-muted-foreground">
                  {skill.description}
                </p>
              </div>
              <CapabilityToggle
                label={skill.name}
                enabled={skill.enabled}
                onToggle={() => void onAct(() => toggleSkill(skill.id, !skill.enabled))}
              />
            </li>
          ))}
        </ul>
      )}

      <GroupTitle note={running > 0 ? `${running} 条会执行` : "没有会执行的"}>钩子</GroupTitle>
      <Lead>
        钩子是插件自带的脚本，在回合的固定节点上跑：提交前补上下文、执行工具前拦一道、
        执行后把结果反馈给模型、收尾前要求继续。工作目录是当前项目根目录，没选项目时是插件自己的目录，
        另外会收到 <span className="font-mono text-foreground">AGLAB_PLUGIN_DIR</span> 这个环境变量。
        标准输入是事件 JSON，标准输出走 UTF-8。
      </Lead>
      {plugin.hooks.length === 0 ? (
        <p className="mt-3 text-sm text-muted-foreground">
          这个插件没有 hooks.json。要自己挂一条，就在插件目录里放
          <span className="font-mono text-foreground"> hooks/hooks.json</span>，写清事件、匹配组和命令。
        </p>
      ) : (
        <ul className="mt-3 space-y-2">
          {plugin.hooks.map((hook) => (
            <HookRow key={hook.id} hook={hook} onAct={onAct} />
          ))}
        </ul>
      )}

      {plugin.hookNotes.length > 0 ? (
        <ul className="mt-3 space-y-1.5">
          {plugin.hookNotes.map((note) => (
            <li
              key={note}
              className="flex items-start gap-2 text-xs leading-5 text-muted-foreground"
            >
              <ShieldAlert className="mt-0.5 size-3 shrink-0" />
              <span>{note}</span>
            </li>
          ))}
        </ul>
      ) : null}

      <GroupTitle note={`${plugin.mcpServers.length} 台`}>MCP 服务</GroupTitle>
      {plugin.mcpServers.length === 0 ? (
        <p className="mt-3 text-sm text-muted-foreground">这个插件没声明 MCP 服务。</p>
      ) : (
        <>
          <Lead>
            插件带来的服务器只能整停——停插件就没了。要看它连上了没有、逐个工具开关，去
            「设置 › MCP 服务」里操作。
          </Lead>
          <ul className="mt-3 space-y-1.5">
            {plugin.mcpServers.map((name) => (
              <li key={name} className="font-mono text-xs text-foreground">
                {name}
              </li>
            ))}
          </ul>
        </>
      )}

      <GroupTitle>信息</GroupTitle>
      <dl className="mt-3 grid grid-cols-[auto_1fr] gap-x-5 gap-y-2 text-xs">
        <dt className="text-muted-foreground">开发者</dt>
        <dd className="text-foreground">{plugin.author || "未署名"}</dd>
        <dt className="text-muted-foreground">类别</dt>
        <dd className="text-foreground">{plugin.category || "未标注"}</dd>
        <dt className="text-muted-foreground">版本</dt>
        <dd className="text-foreground">{plugin.version || "未标注"}</dd>
        <dt className="text-muted-foreground">目录</dt>
        <dd className="min-w-0 break-all font-mono text-xs text-foreground">{plugin.path}</dd>
      </dl>

      {ignored.length > 0 ? (
        <p className="mt-4 text-xs leading-5 text-muted-foreground">
          这个插件还带了 {ignored.join("、")}。aglab 不消费这两类，它们不会被加载，
          也不会出现在模型能看见的任何地方。
        </p>
      ) : null}
    </div>
  );
}

/** 出厂扩展区（design-builtin-extensions.md）。定义住在程序里随应用自带，
 *  关掉一个 = 它带的技能整批从清单与取用里消失——所以扩展停用时，
 *  技能行只留一句实话，不再摆出能拨的开关 */
function BuiltinSection({
  builtins,
  onToggleBuiltin,
  onToggleSkill,
  onAct,
}: {
  builtins: BuiltinView[];
  onToggleBuiltin: (id: string, enabled: boolean) => Promise<void>;
  onToggleSkill: (id: string, enabled: boolean) => Promise<void>;
  onAct: (run: () => Promise<unknown>) => void;
}) {
  const pagedBuiltins = usePaged(builtins);
  if (builtins.length === 0) return null;
  const active = builtins.filter((extension) => extension.enabled).length;

  return (
    <div className="mt-6 border-t border-border pt-5">
      <div className="flex items-baseline gap-2">
        <h2 className="text-base font-semibold tracking-tight text-foreground">出厂扩展</h2>
        <span className="text-xs text-muted-foreground">
          {active} / {builtins.length} 个已启用
        </span>
      </div>
      <p className="mt-1 text-sm leading-6 text-muted-foreground">
        随应用自带的技能包，升级即更新；单条可单独关，关掉后技能页与模型都看不到。
      </p>
      <ul className="mt-3 space-y-2">
        {pagedBuiltins.slice.map((extension) => (
          <li
            key={extension.id}
            className={cn(
              "rounded-lg border border-border bg-surface px-3 py-3",
              !extension.enabled && "opacity-60",
            )}
          >
            <div className="flex items-start gap-3">
              <div className="min-w-0 flex-1">
                <p className="flex items-baseline gap-2 text-base">
                  <span className="font-medium text-foreground">{extension.name}</span>
                  <span className="shrink-0 rounded-md bg-brand/10 px-1.5 py-0.5 text-2xs text-brand-text">
                    出厂
                  </span>
                </p>
                <p className="mt-1 text-xs leading-5 text-muted-foreground">
                  {extension.description}
                </p>
              </div>
              <CapabilityToggle
                label={extension.name}
                enabled={extension.enabled}
                onToggle={() => void onAct(() => onToggleBuiltin(extension.id, !extension.enabled))}
              />
            </div>
            {extension.enabled ? (
              <ul className="mt-2.5 space-y-1.5 border-t border-border pt-2.5">
                {extension.skills.map((skill) => (
                  <li key={skill.id} className="flex items-center gap-3">
                    <div className="min-w-0 flex-1">
                      <p className="truncate text-sm text-foreground">{skill.name}</p>
                      <p className="truncate text-xs text-muted-foreground">
                        {skill.description}
                      </p>
                    </div>
                    <span className="shrink-0 text-xs text-muted-foreground">
                      {skill.chars} 字
                    </span>
                    <CapabilityToggle
                      label={skill.name}
                      enabled={skill.enabled}
                      onToggle={() => void onAct(() => onToggleSkill(skill.id, !skill.enabled))}
                    />
                  </li>
                ))}
              </ul>
            ) : (
              <p className="mt-2 border-t border-border pt-2.5 text-xs text-muted-foreground">
                {extension.skills.length} 条技能随扩展一起停用中。
              </p>
            )}
          </li>
        ))}
      </ul>
      {/* 分页条属于整份出厂扩展清单，一条就够：曾误放进上面的卡片循环，
          每张启用的卡各挂一条共享同一页码的分页条（截图里"每卡一条 2/2 页"就是这么来的） */}
      <PaginationBar
        page={pagedBuiltins.page}
        pages={pagedBuiltins.pages}
        total={pagedBuiltins.total}
        onPage={pagedBuiltins.setPage}
      />
    </div>
  );
}

export function PluginsView() {
  const plugins = useChatStore((s) => s.plugins);
  // 冷启动时配置还没回来，plugins 同样是空数组——不区分就会先说"还没有插件"再冒出内容
  const configLoaded = useChatStore((s) => s.configLoaded);
  const pagedPlugins = usePaged(plugins ?? []);
  const builtins = useChatStore((s) => s.builtins);
  const workspaceHooks = useChatStore((s) => s.workspaceHooks);
  const pluginsDir = useChatStore((s) => s.pluginsDir);
  const pluginsError = useChatStore((s) => s.pluginsError);
  const refreshPlugins = useChatStore((s) => s.refreshPlugins);
  const togglePlugin = useChatStore((s) => s.togglePlugin);
  const toggleBuiltin = useChatStore((s) => s.toggleBuiltin);

  const servers = useChatStore((s) => s.mcpServers);
  const toggleSkill = useChatStore((s) => s.toggleSkill);
  const refreshMcp = useChatStore((s) => s.refreshMcp);

  const [error, setError] = useState<string | null>(null);
  const [openedId, setOpenedId] = useState<string | null>(null);
  // 官方市场：进页拉一次，装完重拉（installedIds 跟着变）
  const [market, setMarket] = useState<MarketView | null>(null);
  const [marketBusy, setMarketBusy] = useState<string | null>(null);
  const [marketNote, setMarketNote] = useState<string | null>(null);
  const loadMarket = useCallback(() => {
    void pluginMarketList()
      .then(setMarket)
      .catch(() => setMarket(null));
  }, []);
  useEffect(() => {
    loadMarket();
  }, [loadMarket]);

  const opened = plugins.find((plugin) => plugin.id === openedId) ?? null;
  // 顶部计数把出厂扩展一并算进去：页面上两类都归这一页管，
  // 只报用户自装的会出现"插件 0/0 已启用"而下面分区明明 9/9 的假象
  const activePlugins = plugins.filter((plugin) => plugin.enabled).length;
  const activeBuiltins = builtins.filter((extension) => extension.enabled).length;
  const connected = servers.filter((server) => server.connected).length;
  const pendingHooks = plugins.reduce(
    (sum, plugin) =>
      sum + plugin.hooks.filter((hook) => hook.supported && (!hook.trusted || !hook.current)).length,
    0,
  );

  async function act(run: () => Promise<unknown>) {
    setError(null);
    try {
      await run();
    } catch (cause) {
      setError(message(cause));
    }
  }

  if (opened) {
    return (
      <SectionFrame
        title={opened.name}
        note="管理 › 插件" 
        actions={
          <Button
            variant="ghost"
            size="sm"
            aria-label="重新扫描插件"
            onClick={() => void act(refreshPlugins)}
          >
            <RefreshCw className="size-3.5" />
          </Button>
        }
      >
        <PluginDetail plugin={opened} onBack={() => setOpenedId(null)} onAct={act} />
        {error ? <p className="mt-3 text-xs text-destructive">{error}</p> : null}
      </SectionFrame>
    );
  }

  return (
    <SectionFrame
      title="插件"
      note={`${activePlugins + activeBuiltins} / ${plugins.length + builtins.length} 个已启用 · ${connected} / ${servers.length} 台 MCP 服务已连接${
        pendingHooks > 0 ? ` · ${pendingHooks} 条钩子待确认` : ""
      }`}
      actions={
        <div className="flex items-center gap-1">
          <Button
            variant="ghost"
            size="sm"
            aria-label="打开插件目录"
            disabled={!pluginsDir}
            onClick={() =>
              void revealItemInDir(pluginsDir).catch((cause) => setError(message(cause)))
            }
          >
            <FolderOpen className="size-3.5" />
            <span>打开目录</span>
          </Button>
          <Button
            variant="ghost"
            size="sm"
            aria-label="重新扫描插件和 MCP 服务"
            onClick={() =>
              void act(async () => {
                await refreshPlugins();
                await refreshMcp();
              })
            }
          >
            <RefreshCw className="size-3.5" />
          </Button>
        </div>
      }
    >
      <Lead>
        插件是<span className="text-foreground">容器</span>：一个目录里可以同时放技能、MCP
        服务器、钩子脚本，以及命令和代理。aglab 消费前三样——技能是清单、MCP
        是连出去的服务、钩子是要你逐条确认才跑的脚本；命令和代理会列出来但暂不加载。
      </Lead>

      <div className="mt-3 rounded-lg border border-border bg-background px-3 py-2.5">
        <p className="text-xs text-muted-foreground">把插件目录放进这里，回来点右上角刷新</p>
        <p className="mt-1 break-all font-mono text-xs text-foreground">{pluginsDir || "…"}</p>
      </div>

      <BuiltinSection builtins={builtins} onToggleBuiltin={toggleBuiltin} onToggleSkill={toggleSkill} onAct={act} />

      {workspaceHooks ? (
        <WorkspaceHooksSection
          view={workspaceHooks}
          onAct={act}
          onChanged={refreshPlugins}
        />
      ) : null}

      <MarketSection
        market={market}
        busy={marketBusy}
        note={marketNote}
        onInstall={(entry) => {
          setMarketBusy(entry.id);
          setMarketNote(null);
          void act(async () => {
            const saved = await pluginMarketInstall(entry.id, entry.downloadUrl, entry.sha256);
            setMarketNote(`${saved}。点右上角刷新即可见。`);
            setMarketBusy(null);
            loadMarket();
            await refreshPlugins();
          });
        }}
      />

      {!configLoaded ? (
        <ListSkeleton rows={3} className="mt-4" label="正在加载插件" />
      ) : plugins.length === 0 ? (
        <p className="mt-4 text-sm text-muted-foreground">
          还没有插件。只想加一个技能的话，直接放进技能目录就行，不必包一层插件。
        </p>
      ) : (
        <>
        <ul className="mt-4 space-y-2">
          {pagedPlugins.slice.map((plugin) => {
            const running = plugin.hooks.filter((hook) => hook.runs).length;
            const waiting = plugin.hooks.filter(
              (hook) => hook.supported && (!hook.trusted || !hook.current),
            ).length;

            return (
              <li key={plugin.id} className="rounded-lg border border-border bg-surface px-3 py-3">
                <div className="flex items-start gap-3">
                  <div className="min-w-0 flex-1">
                    <p className="flex items-baseline gap-2 text-base">
                      <button
                        type="button"
                        onClick={() => setOpenedId(plugin.id)}
                        className="max-w-full truncate rounded-lg text-left font-medium text-foreground transition-colors hover:text-brand-text focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/45"
                      >
                        {plugin.name}
                      </button>
                      {plugin.version ? (
                        <span className="font-mono text-xs text-muted-foreground">
                          v{plugin.version}
                        </span>
                      ) : null}
                    </p>
                    {plugin.description ? (
                      <p className="mt-1 text-xs leading-5 text-muted-foreground">
                        {plugin.description}
                      </p>
                    ) : null}
                    <p className="mt-1.5 text-xs text-muted-foreground">
                      {plugin.skills.length} 个技能 · {plugin.mcpServers.length} 台 MCP 服务 ·{" "}
                      {plugin.hooks.length} 条钩子
                      {running > 0 ? (
                        <span className="text-brand-text">（{running} 条会执行）</span>
                      ) : null}
                      {waiting > 0 ? (
                        <span className="text-destructive">（{waiting} 条待确认）</span>
                      ) : null}
                    </p>
                  </div>
                  <CapabilityToggle
                    label={plugin.name}
                    enabled={plugin.enabled}
                    onToggle={() => void act(() => togglePlugin(plugin.id, !plugin.enabled))}
                  />
                </div>
              </li>
            );
          })}
        </ul>
          <PaginationBar page={pagedPlugins.page} pages={pagedPlugins.pages} total={pagedPlugins.total} onPage={pagedPlugins.setPage} />
      </>
      )}

      {error ? <p className="mt-3 text-xs text-destructive">{error}</p> : null}
      {!error && pluginsError ? (
        <p className="mt-3 text-xs text-destructive">{pluginsError}</p>
      ) : null}
    </SectionFrame>
  );
}
