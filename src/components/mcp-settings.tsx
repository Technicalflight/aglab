import { useEffect, useState } from "react";
import { IconPlus as Plus, IconRefresh as RefreshCw, IconTrash as Trash } from "@tabler/icons-react";

import { Button } from "@/components/ui/button";
import { CapabilityToggle } from "@/components/ui/capability-toggle";
import { SectionFrame } from "@/components/section-frame";
import type { McpServer } from "@/types/chat";
import { hostOfUrl } from "@/lib/links";
import { registrySearch, type RegistryEntry, mcpOauthLogin, mcpOauthStatus, mcpOauthLogout, type McpOAuthStatus as OAuthStatusView } from "@/lib/chat-transport";
import { useChatStore } from "@/store/chat-store";
import { PaginationBar, usePaged } from "@/components/pagination";
import { cn } from "@/lib/utils";

const inputClass =
  "h-9 w-full rounded-lg border border-input bg-background px-3 text-base text-foreground outline-none transition-colors focus-visible:border-brand/50 focus-visible:ring-2 focus-visible:ring-ring/35";

function message(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}

function emptyServer(): McpServer {
  return { id: "", name: "", command: "", args: [], env: {}, enabled: true, transport: "stdio" };
}

/**
 * MCP 服务的独立管理页（设置 › MCP 服务）。
 * 从插件页整块搬来：MCP 是连接外部服务的能力配置，不是扩展本体——
 * 插件带来的服务在这里看状态与逐工具开关，独立配置的在这里增删改。
 */
/** OAuth 行：登录/登出 + 过期时刻。登录要等浏览器授权（最长 5 分钟），
 *  完成后自动重连这条服务器——新 Bearer 在下一次连接时才注入 */
function OAuthRow({ id, name }: { id: string; name: string }) {
  const [status, setStatus] = useState<OAuthStatusView | null>(null);
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<string | null>(null);
  const refreshMcp = useChatStore((s) => s.refreshMcp);
  const pushToast = useChatStore((s) => s.pushToast);

  useEffect(() => {
    mcpOauthStatus(id)
      .then(setStatus)
      .catch(() => setStatus({ loggedIn: false, expiresAtMs: null }));
  }, [id]);

  const expires = status?.expiresAtMs
    ? new Date(status.expiresAtMs).toLocaleString("zh-CN")
    : null;

  return (
    <div className="mt-1.5 flex items-center gap-2 rounded-lg border border-border bg-background px-2.5 py-1.5">
      <span className="min-w-0 flex-1 truncate text-xs text-muted-foreground">
        OAuth：
        {status === null
          ? "读取中…"
          : status.loggedIn
            ? `已登录${expires ? ` · 至 ${expires}` : ""}`
            : "未登录"}
      </span>
      <Button
        variant="subtle"
        size="sm"
        disabled={busy}
        onClick={() => {
          setBusy(true);
          setNote(null);
          mcpOauthLogin(id)
            .then((outcome) => {
              pushToast({
                tone: "info",
                title: `「${outcome.server}」OAuth 登录成功`,
                detail: outcome.scopes.length > 0 ? `scope：${outcome.scopes.join(" ")}` : undefined,
              });
              return mcpOauthStatus(id).then((next) => {
                setStatus(next);
                void refreshMcp();
              });
            })
            .catch((cause: unknown) =>
              setNote(cause instanceof Error ? cause.message : String(cause)),
            )
            .finally(() => setBusy(false));
        }}
      >
        OAuth 登录
      </Button>
      {status?.loggedIn ? (
        <Button
          variant="ghost"
          size="sm"
          disabled={busy}
          onClick={() => {
            setBusy(true);
            mcpOauthLogout(id)
              .then((removed) => {
                setStatus({ loggedIn: false, expiresAtMs: null });
                if (removed) void refreshMcp();
              })
              .finally(() => setBusy(false));
          }}
        >
          登出
        </Button>
      ) : null}
      {note ? (
        <span className="max-w-[280px] shrink truncate text-xs text-destructive" title={note}>
          {note}
        </span>
      ) : null}
      <span className="sr-only">{name} 的 OAuth 状态</span>
    </div>
  );
}

export function McpSettings() {
  const config = useChatStore((s) => s.config);
  const updateConfig = useChatStore((s) => s.updateConfig);
  const servers = useChatStore((s) => s.mcpServers);
  const pagedServers = usePaged(servers);
  const saveMcpServer = useChatStore((s) => s.saveMcpServer);
  const removeMcpServer = useChatStore((s) => s.removeMcpServer);
  const toggleMcpTool = useChatStore((s) => s.toggleMcpTool);
  const connectMcp = useChatStore((s) => s.connectMcp);
  const stopMcp = useChatStore((s) => s.stopMcp);
  const refreshMcpTools = useChatStore((s) => s.refreshMcpTools);
  const refreshMcp = useChatStore((s) => s.refreshMcp);

  const [draft, setDraft] = useState<McpServer | null>(null);
  /** 参数逐行编辑：一行一个参数，比逗号分隔少一半转义的心智负担 */
  const [argsRows, setArgsRows] = useState<string[]>([""]);
  /** 环境变量键值对：后端会把它们原样注进子进程（cc-switch 导入同一条通道） */
  const [envRows, setEnvRows] = useState<Array<{ key: string; value: string }>>([]);
  /** http 型的请求头键值对：Authorization 这类凭据随每个请求上行 */
  const [headerRows, setHeaderRows] = useState<Array<{ key: string; value: string }>>([]);
  const [error, setError] = useState<string | null>(null);
  // 刷新工具清单 = 重开那条连接，会打断正在跑的调用，所以它要两下：第一下只把代价说清楚
  const [pendingRefresh, setPendingRefresh] = useState<string | null>(null);

  // ---- 市场（官方注册表）：浏览、搜索、挑一条预填进添加表单 ----
  const [marketOpen, setMarketOpen] = useState(false);
  const [marketQuery, setMarketQuery] = useState("");
  const [marketEntries, setMarketEntries] = useState<RegistryEntry[]>([]);
  const [marketCursor, setMarketCursor] = useState<string | null>(null);
  const [marketLoading, setMarketLoading] = useState(false);
  const [marketError, setMarketError] = useState<string | null>(null);

  async function loadMarket(reset: boolean) {
    setMarketLoading(true);
    setMarketError(null);
    try {
      // 续拉也带上同一个搜索词：cursor 是结果集里的锚点，配合 search 保持同一过滤
      const page = await registrySearch(reset ? marketQuery : marketQuery, reset ? null : marketCursor);
      setMarketEntries((previous) => (reset ? page.entries : [...previous, ...page.entries]));
      setMarketCursor(page.nextCursor);
      if (reset) setMarketQuery(marketQuery);
    } catch (cause) {
      setMarketError(message(cause));
    } finally {
      setMarketLoading(false);
    }
  }

  /** 市场挑中一条：只做**预填**，进既有的添加表单让用户核对后再保存。
   *  凭据（请求头/环境变量）只预填名字，值永远留给用户自己填 */
  function pickFromMarket(entry: RegistryEntry) {
    const remote = entry.remotes[0];
    const pkg = entry.package;
    const shortName = (entry.title ?? entry.name.split("/").pop() ?? entry.name).trim();
    if (remote) {
      setDraft({
        id: "",
        name: shortName,
        transport: "http",
        command: "",
        args: [],
        env: {},
        url: remote.url,
        headers: {},
        enabled: true,
      });
      setArgsRows([""]);
      setEnvRows([]);
      setHeaderRows(remote.headers.map((header) => ({ key: header.name, value: "" })));
    } else if (pkg) {
      const runtime =
        pkg.runtimeHint ?? (pkg.registryType === "pypi" ? "uvx" : pkg.registryType === "npm" ? "npx" : "");
      setDraft({
        id: "",
        name: shortName,
        transport: "stdio",
        command: runtime,
        args: [],
        env: {},
        url: "",
        headers: {},
        enabled: true,
      });
      setArgsRows([...pkg.args, pkg.identifier].filter(Boolean).length > 0 ? [...pkg.args, pkg.identifier] : [""]);
      setEnvRows(pkg.env.map((variable) => ({ key: variable.name, value: "" })));
      setHeaderRows([]);
    } else {
      setMarketError("这条目没有可用的连接方式（既没有 remote 也没有 package），跳过。");
      return;
    }
    setMarketError(null);
    setError(null);
    setMarketOpen(false);
  }

  const connected = servers.filter((server) => server.connected).length;

  async function act(run: () => Promise<unknown>) {
    setError(null);
    try {
      await run();
    } catch (cause) {
      setError(message(cause));
    }
  }

  async function submitServer() {
    if (!draft) return;
    const name = draft.name.trim();
    const isHttp = draft.transport === "http";
    const command = draft.command.trim();
    const url = (draft.url ?? "").trim();
    if (!name) {
      setError("名称要填。");
      return;
    }
    if (!isHttp && !command) {
      setError("名称和命令都要填。");
      return;
    }
    if (isHttp && !/^https?:\/\//.test(url)) {
      setError("HTTP 服务地址要以 http:// 或 https:// 开头。");
      return;
    }
    const args = argsRows.map((row) => row.trim()).filter(Boolean);
    const envKeys = envRows.map((row) => row.key.trim()).filter(Boolean);
    const headerKeys = headerRows.map((row) => row.key.trim()).filter(Boolean);
    if (new Set(envKeys).size !== envKeys.length) {
      setError("环境变量有重复的键名。");
      return;
    }
    if (new Set(headerKeys).size !== headerKeys.length) {
      setError("请求头有重复的键名。");
      return;
    }
    const env: Record<string, string> = {};
    for (const row of envRows) {
      const key = row.key.trim();
      if (key) env[key] = row.value;
    }
    const headers: Record<string, string> = {};
    for (const row of headerRows) {
      const key = row.key.trim();
      if (key) headers[key] = row.value;
    }

    await act(async () => {
      await saveMcpServer({
        ...draft,
        id: draft.id || `mcp-${Date.now().toString(36)}`,
        name,
        transport: isHttp ? "http" : "stdio",
        command: isHttp ? "" : command,
        args: isHttp ? [] : args,
        env: isHttp ? {} : env,
        url: isHttp ? url : "",
        headers,
        oauth: isHttp && (draft.oauth ?? false),
        enabled: draft.enabled,
      });
      setDraft(null);
      setArgsRows([""]);
      setEnvRows([]);
      setHeaderRows([]);
    });
  }

  return (
    <SectionFrame
      title="MCP 服务"
      note={`${connected} / ${servers.length} 台已连接`}
      actions={
        <div className="flex items-center gap-1">
          <Button
            variant={marketOpen ? "subtle" : "ghost"}
            size="sm"
            aria-pressed={marketOpen}
            onClick={() => {
              setMarketError(null);
              setMarketOpen((open) => {
                // 首次打开就拉一页：空结果集和"还没搜"是两种状态
                if (!open && marketEntries.length === 0) void loadMarket(true);
                return !open;
              });
            }}
          >
            <span>市场</span>
          </Button>
          <Button
            variant="subtle"
            size="sm"
            disabled={draft !== null}
            onClick={() => {
              setError(null);
              setDraft(emptyServer());
              setArgsRows([""]);
              setEnvRows([]);
              setHeaderRows([]);
            }}
          >
            <span>独立配一个</span>
          </Button>
          <Button
            variant="ghost"
            size="sm"
            aria-label="重新扫描 MCP 服务"
            onClick={() => void act(refreshMcp)}
          >
            <RefreshCw className="size-3.5" />
          </Button>
        </div>
      }
    >
      <p className="text-sm leading-6 text-muted-foreground">
        MCP 让模型连上外部服务（数据库、API、网页）。跑的是别人的程序，看不到它会做什么，所以
        <span className="text-foreground"> MCP 工具一律按高风险处理</span>
        ，逐项确认和自动放行两档下都会先问你。插件带来的服务只能停整个插件，要单独删就在这里删独立配置的。
      </p>

      {marketOpen ? (
        <div className="mt-4 rounded-lg border border-border bg-surface p-3">
          <div className="flex items-center justify-between gap-2">
            <p className="text-base font-medium text-foreground">MCP 市场</p>
            <Button
              variant="ghost"
              size="sm"
              onClick={() => setMarketOpen(false)}
            >
              收起
            </Button>
          </div>
          <p className="mt-0.5 text-xs leading-5 text-muted-foreground">
            来自 MCP 官方注册表。挑一条带进表单核对后再保存；请求头与环境变量的值要你自己填——目录只提供名字。
          </p>
          <div className="mt-2 flex items-center gap-1.5">
            <input
              type="search"
              value={marketQuery}
              placeholder="搜名字或用途，如 github、filesystem"
                aria-label="搜索 MCP 服务器"
              className={cn(inputClass, "h-8 text-sm")}
              onChange={(event) => setMarketQuery(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter") {
                  event.preventDefault();
                  void loadMarket(true);
                }
              }}
            />
            <Button
              variant="subtle"
              size="sm"
              className="shrink-0"
              disabled={marketLoading}
              onClick={() => void loadMarket(true)}
            >
              搜索
            </Button>
          </div>
          {marketError ? <p className="mt-2 text-xs text-destructive">{marketError}</p> : null}
          <ul className="mt-2 max-h-72 space-y-1.5 overflow-y-auto">
            {marketEntries.map((entry) => {
              const remote = entry.remotes[0];
              const pkg = entry.package;
              const badge = remote
                ? `HTTP · ${hostOfUrl(remote.url)}`
                : pkg
                  ? `${pkg.registryType} · ${pkg.identifier}`
                  : "无可连接形态";
              const pickable = Boolean(remote || pkg);
              return (
                <li
                  key={entry.name}
                  className="rounded-lg border border-border bg-background px-2.5 py-2"
                >
                  <div className="flex items-start gap-2">
                    <div className="min-w-0 flex-1">
                      <p className="truncate text-sm text-foreground">
                        {entry.title || entry.name}
                        <span className="ml-1.5 font-mono text-2xs text-muted-foreground/60">
                          {remote ? remote.transportType : pkg ? pkg.registryType : "—"}
                        </span>
                      </p>
                      <p className="mt-0.5 line-clamp-2 text-xs leading-5 text-muted-foreground">
                        {entry.description}
                      </p>
                      <p className="mt-1 truncate font-mono text-2xs text-muted-foreground/70">
                        {badge}
                      </p>
                    </div>
                    <Button
                      variant="subtle"
                      size="sm"
                      className="shrink-0"
                      disabled={!pickable}
                      onClick={() => pickFromMarket(entry)}
                    >
                      添加
                    </Button>
                  </div>
                </li>
              );
            })}
            {marketEntries.length === 0 && !marketLoading && !marketError ? (
              <li className="py-2 text-center text-xs text-muted-foreground">
                还没有条目。输入关键词搜一下。
              </li>
            ) : null}
          </ul>
          {marketLoading ? (
            <p className="mt-2 text-xs text-muted-foreground">正在拉取…</p>
          ) : marketCursor ? (
            <Button
              variant="ghost"
              size="sm"
              className="mt-2 w-full"
              onClick={() => void loadMarket(false)}
            >
              加载更多
            </Button>
          ) : null}
        </div>
      ) : null}

      {draft ? (
        <div className="mt-4 rounded-lg border border-border bg-surface p-3">
          <div>
            <span className="mb-1.5 block text-xs text-muted-foreground">连接方式</span>
            <div className="grid grid-cols-2 gap-2">
              {(
                [
                  { value: "stdio", label: "本地程序", hint: "起一个子进程，走标准输入输出" },
                  { value: "http", label: "HTTP 地址", hint: "调一个网络服务商（streamable HTTP）" },
                ] as const
              ).map((option) => (
                <button
                  key={option.value}
                  type="button"
                  title={option.hint}
                  aria-pressed={draft.transport === option.value}
                  onClick={() => setDraft({ ...draft, transport: option.value })}
                  className={cn(
                    "rounded-lg border px-3 py-2 text-left text-sm outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
                    draft.transport === option.value
                      ? "border-brand/50 bg-brand/10 text-foreground"
                      : "border-border text-muted-foreground hover:bg-accent hover:text-foreground",
                  )}
                >
                  {option.label}
                </button>
              ))}
            </div>
          </div>

          <div className="mt-3 grid gap-3 sm:grid-cols-2">
            <label className="block">
              <span className="mb-1.5 block text-xs text-muted-foreground">显示名</span>
              <input
                type="text"
                value={draft.name}
                placeholder="例如：Postgres"
                className={inputClass}
                onChange={(event) => setDraft({ ...draft, name: event.target.value })}
              />
            </label>
            {draft.transport === "http" ? (
              <label className="block">
                <span className="mb-1.5 block text-xs text-muted-foreground">
                  服务地址（streamable HTTP 服务商）
                </span>
                <input
                  type="text"
                  value={draft.url ?? ""}
                  placeholder="https://mcp.example.com/mcp"
                  spellCheck={false}
                  className={cn(inputClass, "font-mono text-sm")}
                  onChange={(event) => setDraft({ ...draft, url: event.target.value })}
                />
              </label>
            ) : (
              <label className="block">
                <span className="mb-1.5 block text-xs text-muted-foreground">启动命令</span>
                <input
                  type="text"
                  value={draft.command}
                  placeholder="npx"
                  spellCheck={false}
                  className={inputClass}
                  onChange={(event) => setDraft({ ...draft, command: event.target.value })}
                />
              </label>
            )}
          </div>

          {draft.transport === "http" ? (
            <label className="mt-3 flex items-start gap-2">
              <input
                type="checkbox"
                checked={draft.oauth ?? false}
                onChange={(event) => setDraft({ ...draft, oauth: event.target.checked })}
                className="mt-0.5 size-3.5 accent-[var(--brand)]"
              />
              <span className="min-w-0">
                <span className="block text-sm text-foreground">OAuth 登录</span>
                <span className="block text-xs leading-5 text-muted-foreground">
                  远程服务器要 OAuth 时勾上（Notion、Linear 这类）。连接时自动带 Bearer；
                  保存后在服务器卡片里点「OAuth 登录」完成浏览器授权。
                </span>
              </span>
            </label>
          ) : null}

          {draft.transport === "http" ? (
            <div className="mt-3">
              <span className="mb-1.5 block text-xs text-muted-foreground">
                请求头（随每个请求上行，放 Authorization 这类凭据）
              </span>
              {headerRows.map((row, index) => (
                <div key={index} className="mt-1.5 flex items-center gap-1.5">
                  <input
                    type="text"
                    value={row.key}
                    placeholder="头名，例如 Authorization"
                  aria-label="请求头名称"
                    spellCheck={false}
                    className={cn(inputClass, "font-mono text-sm")}
                    onChange={(event) =>
                      setHeaderRows((rows) => rows.map((item, at) => (at === index ? { ...item, key: event.target.value } : item)))
                    }
                  />
                  <input
                    type="text"
                    value={row.value}
                    placeholder="值，例如 Bearer sk-…"
                  aria-label="请求头值"
                    spellCheck={false}
                    className={cn(inputClass, "font-mono text-sm")}
                    onChange={(event) =>
                      setHeaderRows((rows) => rows.map((item, at) => (at === index ? { ...item, value: event.target.value } : item)))
                    }
                  />
                  <button
                    type="button"
                    aria-label="删除这个请求头"
                    onClick={() => setHeaderRows((rows) => rows.filter((_, at) => at !== index))}
                    className="flex size-8 shrink-0 items-center justify-center rounded-lg text-muted-foreground outline-none transition-colors hover:bg-accent hover:text-destructive focus-visible:ring-2 focus-visible:ring-ring/45"
                  >
                    <Trash className="size-3.5" />
                  </button>
                </div>
              ))}
              <button
                type="button"
                onClick={() => setHeaderRows((rows) => [...rows, { key: "", value: "" }])}
                className="mt-1.5 flex w-full items-center justify-center gap-1 rounded-lg border border-dashed border-border py-1.5 text-xs text-muted-foreground outline-none transition-colors hover:border-brand/50 hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45"
              >
                <Plus className="size-3" />
                添加请求头
              </button>
              <p className="mt-1.5 text-xs leading-5 text-muted-foreground">
                请求头按原样随每个请求上行；HTTP 型不走子进程，环境变量这一格用不上。
              </p>
            </div>
          ) : (
            <>
          <div className="mt-3">
            <span className="mb-1.5 block text-xs text-muted-foreground">
              参数（一行一个，按顺序原样传给命令）
            </span>
            {argsRows.map((row, index) => (
              <div key={index} className="mt-1.5 flex items-center gap-1.5">
                <input aria-label="启动参数"
                  type="text"
                  value={row}
                  placeholder={index === 0 ? "-y" : "@modelcontextprotocol/server-postgres"}
                  spellCheck={false}
                  className={cn(inputClass, "font-mono text-sm")}
                  onChange={(event) =>
                    setArgsRows((rows) => rows.map((item, at) => (at === index ? event.target.value : item)))
                  }
                />
                <button
                  type="button"
                  aria-label="删除这个参数"
                  onClick={() => setArgsRows((rows) => rows.filter((_, at) => at !== index))}
                  className="flex size-8 shrink-0 items-center justify-center rounded-lg text-muted-foreground outline-none transition-colors hover:bg-accent hover:text-destructive focus-visible:ring-2 focus-visible:ring-ring/45"
                >
                  <Trash className="size-3.5" />
                </button>
              </div>
            ))}
            <button
              type="button"
              onClick={() => setArgsRows((rows) => [...rows, ""])}
              className="mt-1.5 flex w-full items-center justify-center gap-1 rounded-lg border border-dashed border-border py-1.5 text-xs text-muted-foreground outline-none transition-colors hover:border-brand/50 hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45"
            >
              <Plus className="size-3" />
              添加参数
            </button>
          </div>

          <div className="mt-3">
            <span className="mb-1.5 block text-xs text-muted-foreground">
              环境变量（原样注入子进程，放 API 密钥这类启动配置）
            </span>
            {envRows.map((row, index) => (
              <div key={index} className="mt-1.5 flex items-center gap-1.5">
                <input
                  type="text"
                  value={row.key}
                  placeholder="键，例如 API_KEY"
                  aria-label="环境变量名"
                  spellCheck={false}
                  className={cn(inputClass, "font-mono text-sm")}
                  onChange={(event) =>
                    setEnvRows((rows) => rows.map((item, at) => (at === index ? { ...item, key: event.target.value } : item)))
                  }
                />
                <input aria-label="环境变量值"
                  type="text"
                  value={row.value}
                  placeholder="值"
                  spellCheck={false}
                  className={cn(inputClass, "font-mono text-sm")}
                  onChange={(event) =>
                    setEnvRows((rows) => rows.map((item, at) => (at === index ? { ...item, value: event.target.value } : item)))
                  }
                />
                <button
                  type="button"
                  aria-label="删除这个环境变量"
                  onClick={() => setEnvRows((rows) => rows.filter((_, at) => at !== index))}
                  className="flex size-8 shrink-0 items-center justify-center rounded-lg text-muted-foreground outline-none transition-colors hover:bg-accent hover:text-destructive focus-visible:ring-2 focus-visible:ring-ring/45"
                >
                  <Trash className="size-3.5" />
                </button>
              </div>
            ))}
            <button
              type="button"
              onClick={() => setEnvRows((rows) => [...rows, { key: "", value: "" }])}
              className="mt-1.5 flex w-full items-center justify-center gap-1 rounded-lg border border-dashed border-border py-1.5 text-xs text-muted-foreground outline-none transition-colors hover:border-brand/50 hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45"
            >
              <Plus className="size-3" />
              添加环境变量
            </button>
          </div>
            </>
          )}
          <div className="mt-3 flex items-center gap-2">
            <Button variant="brand" size="sm" onClick={() => void submitServer()}>
              {draft.id ? "保存修改" : "加进来"}
            </Button>
            <Button variant="ghost" size="sm" onClick={() => setDraft(null)}>
              取消
            </Button>
          </div>
        </div>
      ) : null}

      {/* 自定义 MCP 总开关（design-security-center.md D7）：一键停用户自配的全部服务器。
          出厂扩展与插件自带的不受它管——它们的开关在各自那页 */}
      <div className="mt-4 flex items-center gap-3 rounded-lg border border-border bg-surface px-3 py-3">
        <div className="min-w-0 flex-1">
          <p className="text-base font-medium text-foreground">自定义 MCP</p>
          <p className="mt-0.5 text-xs leading-5 text-muted-foreground">
            一键停用用户自配清单里的全部服务器；出厂扩展与插件自带的不受影响。
          </p>
        </div>
        <CapabilityToggle
          label="自定义 MCP 总开关"
          enabled={config.userMcpEnabled}
          onToggle={() => void updateConfig({ userMcpEnabled: !config.userMcpEnabled })}
        />
      </div>

      {servers.length === 0 && !draft ? (
        <p className="mt-4 text-sm text-muted-foreground">
          还没有 MCP 服务。也可以把它写进插件的 <span className="font-mono">.mcp.json</span>。
        </p>
      ) : (
        <>
        <ul className="mt-4 space-y-2">
          {pagedServers.slice.map((server) => (
            <li key={server.id} className="rounded-lg border border-border bg-surface px-3 py-3">
              <div className="flex items-baseline gap-2">
                <span className="min-w-0 flex-1 truncate text-base font-medium text-foreground">
                  {server.name}
                </span>
                <span className="shrink-0 text-xs text-muted-foreground">
                  来源 <span className="text-foreground">{server.source}</span>
                </span>
                <span
                  className={cn(
                    "shrink-0 text-xs",
                    server.connected ? "text-brand-text" : "text-muted-foreground",
                  )}
                >
                  {server.connected
                    ? `已连接 · ${server.tools.length} 个工具${
                        server.canResources ? " · 可读资源" : ""
                      }${server.canPrompts ? " · 有提示词" : ""}`
                    : "未连接"}
                </span>
              </div>
              <p className="mt-1 break-all font-mono text-xs text-muted-foreground">
                {server.transport === "http"
                  ? [server.url ?? "", Object.entries(server.headers ?? {}).map(([key]) => key).join("、")]
                      .filter(Boolean)
                      .join("  ·  ")
                  : [server.command, ...server.args].join(" ")}
              </p>

              {server.transport === "http" && server.oauth ? (
                <OAuthRow id={server.id} name={server.name} />
              ) : null}

              {server.connected && server.tools.length > 0 ? (
                <ul className="mt-2 divide-y divide-border overflow-hidden rounded-lg border border-border bg-background">
                  {server.tools.map((tool) => (
                    <li key={tool.exposed} className="flex items-start gap-3 px-2.5 py-2">
                      <div className="min-w-0 flex-1">
                        <p className="truncate font-mono text-xs text-foreground">
                          {tool.name}
                        </p>
                        {tool.description ? (
                          <p className="mt-0.5 line-clamp-2 text-xs leading-5 text-muted-foreground">
                            {tool.description}
                          </p>
                        ) : null}
                      </div>
                      <CapabilityToggle
                        label={tool.name}
                        enabled={tool.enabled}
                        onToggle={() => void act(() => toggleMcpTool(tool.exposed, !tool.enabled))}
                      />
                    </li>
                  ))}
                </ul>
              ) : null}

              <div className="mt-3 flex flex-wrap items-center gap-2 border-t border-border pt-2.5">
                {server.connected ? (
                  <Button variant="subtle" size="sm" onClick={() => void act(() => stopMcp(server.id))}>
                    断开
                  </Button>
                ) : (
                  <Button
                    variant="subtle"
                    size="sm"
                    disabled={!server.enabled}
                    onClick={() => void act(() => connectMcp(server.id))}
                  >
                    连接
                  </Button>
                )}
                {server.connected ? (
                  <Button
                    variant="ghost"
                    size="sm"
                    aria-label="重开这台服务以回读它的工具清单"
                    onClick={() =>
                      void act(async () => {
                        if (pendingRefresh !== server.id) {
                          setPendingRefresh(server.id);
                          return;
                        }
                        setPendingRefresh(null);
                        await refreshMcpTools(server.id);
                      })
                    }
                  >
                    {pendingRefresh === server.id ? "确认重开（会打断在跑的调用）" : "刷新工具"}
                  </Button>
                ) : null}
                {server.source === "独立配置" ? (
                  <>
                    <Button
                      variant="ghost"
                      size="sm"
                      onClick={() => {
                        setError(null);
                        setDraft({
                          id: server.id,
                          name: server.name,
                          transport: server.transport ?? "stdio",
                          command: server.command,
                          args: server.args,
                          env: server.env,
                          url: server.url ?? "",
                          headers: server.headers ?? {},
                          enabled: server.enabled,
                        });
                        setArgsRows(server.args.length > 0 ? [...server.args] : [""]);
                        setEnvRows(Object.entries(server.env).map(([key, value]) => ({ key, value })));
                        setHeaderRows(
                          Object.entries(server.headers ?? {}).map(([key, value]) => ({ key, value })),
                        );
                      }}
                    >
                      编辑
                    </Button>
                    <Button
                      variant="ghost"
                      size="sm"
                      className="ml-auto text-destructive hover:bg-destructive/15 hover:text-destructive"
                      onClick={() => void act(() => removeMcpServer(server.id))}
                    >
                      删除
                    </Button>
                  </>
                ) : (
                  <span className="text-xs text-muted-foreground">
                    由插件提供，要停用就停整个插件
                  </span>
                )}
              </div>
            </li>
          ))}
        </ul>
        <PaginationBar page={pagedServers.page} pages={pagedServers.pages} total={pagedServers.total} onPage={pagedServers.setPage} />
        </>
      )}

      {error ? <p className="mt-3 text-xs text-destructive">{error}</p> : null}
    </SectionFrame>
  );
}
