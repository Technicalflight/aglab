import { useEffect, useRef, useState, type ReactNode } from "react";

import { IconGlobe as Globe, IconPlus as Plus, IconTrash as Trash2 } from "@tabler/icons-react";

import {
  proxyTest,
  proxyImport,
  proxyPoolTestAll,
  fetchProxyPoolStats,
  type ProxyStat,
} from "@/lib/chat-transport";
import { PROXY_PAGE_SIZE, allSelected, mergeSelection } from "@/lib/proxy-list";
import { clampPage, pageCount, pageSlice } from "@/lib/pagination";
import { cn } from "@/lib/utils";

import { Button } from "@/components/ui/button";
import { CapabilityToggle } from "@/components/ui/capability-toggle";
import { Pager } from "@/components/ui/pager";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import type { AppConfig, ProxyEntry } from "@/types/chat";
import { useChatStore } from "@/store/chat-store";
import { FormColumn } from "@/components/ui/content-column";

const inputClass =
  "h-9 w-full rounded-lg border border-input bg-background px-3 text-base text-foreground outline-none transition-colors focus-visible:border-brand/50 focus-visible:ring-2 focus-visible:ring-ring/35";

/** 均衡策略四档。绑定 "pool" 的请求在启用的代理之间按它挑 */
const STRATEGIES: Array<{ value: string; label: string; desc: string }> = [
  {
    value: "round_robin",
    label: "轮询",
    desc: "平滑加权轮询，按权重一条接一条均匀轮换（nginx 同款算法）",
  },
  {
    value: "random",
    label: "随机",
    desc: "按权重随机挑一条，权重越大越常被挑中",
  },
  {
    value: "least_used",
    label: "最少使用",
    desc: "挑当前并发最少的那条，长回合场景更平均",
  },
  {
    value: "adaptive",
    label: "自适应",
    desc: "并发乘以响应头耗时，谁快谁闲走谁；还没量过的代理先量一次",
  },
];

/** 毫秒说人话：一千以上换成秒，一位小数 */
function ms(ms: number) {
  return ms >= 1000 ? `${(ms / 1000).toFixed(1)}s` : `${Math.round(ms)}ms`;
}

/** 冷却倒计时只关心"还要等多久"：向上取整到秒 */
function secs(ms: number) {
  return `${Math.ceil(ms / 1000)}s`;
}

/** 全局绑定的下拉值。""（直连）与 direct 在全局层同义，列表里只显示一个"直连" */
const GLOBAL_INHERIT = "__direct__";

function Field({ label, children, hint }: { label: string; children: ReactNode; hint?: ReactNode }) {
  // 不能用 <label> 包字段：它里面是按钮组和 Select，Chromium 会把悬停/点击
  // 转发给 label 里第一个表单控件——悬停说明文字，第一格「轮询」就亮 hover
  return (
    <div className="block">
      <span className="mb-1.5 block text-xs text-muted-foreground">{label}</span>
      {children}
      {hint ? <span className="mt-1.5 block text-xs leading-5 text-muted-foreground">{hint}</span> : null}
    </div>
  );
}

/** 绑定下拉的公共部分：直连 / 代理池 / 各代理。全局层多一个"继承"语义（=直连），调用方自己拼 */
function BindingOptions({ config }: { config: AppConfig }) {
  return (
    <>
      <SelectItem value="direct">直连</SelectItem>
      <SelectItem value="pool">代理池（按策略均衡）</SelectItem>
      {config.proxyPool.proxies.map((entry) => (
        <SelectItem key={entry.id} value={entry.id} className="text-sm">
          {entry.name}
          {entry.enabled ? "" : "（已停用）"}
        </SelectItem>
      ))}
    </>
  );
}

/**
 * 设置页的「代理」项：代理池（增删启停测）+ 全局默认绑定 + 绕过名单 + 均衡策略。
 * 服务商级与按模型的绑定在「服务商档案」的编辑弹窗里。
 */
export function ProxySettings() {
  return (
    <FormColumn>
      <h1 className="text-2xl font-semibold tracking-tight text-foreground">代理</h1>
      <p className="mt-1 text-sm leading-6 text-muted-foreground">
        1. 在这里配全局默认代理（直连 / 代理池 / 指定某台）；
        2. 要按服务商或按模型细分，去服务商档案里绑定；点名的代理停用或被删时请求会报错，不会静默直连；
        3. 模型流量即时生效；界面渲染层与 MCP / 命令在下次启动 / 下次 spawn 生效。本机回环与绕过名单恒直连。
      </p>
      <p className="mt-1 text-xs leading-5 text-muted-foreground">
        aglab 不读取系统代理环境变量（HTTP_PROXY 等）——配了就走这里的，没配就直连。
      </p>
      <div className="mt-8">
        <ProxyBody />
      </div>
    </FormColumn>
  );
}

function ProxyBody() {
  const config = useChatStore((s) => s.config);
  const updateConfig = useChatStore((s) => s.updateConfig);
  const [stats, setStats] = useState<ProxyStat[]>([]);
  // 批量三格：粘贴导入 / 一键测全部 / 勾选管理。都是"一次动一组"，不是新的数据形状
  const [importing, setImporting] = useState(false);
  const [importText, setImportText] = useState("");
  const [importReport, setImportReport] = useState<string | null>(null);
  const [testingAll, setTestingAll] = useState(false);
  const [testReport, setTestReport] = useState<string | null>(null);
  const [managing, setManaging] = useState(false);
  const [selected, setSelected] = useState<string[]>([]);
  const [confirmBulkDelete, setConfirmBulkDelete] = useState(false);
  // 分页：一行两格（控件 + 读数）还带 3 秒心跳，导入一百条时不能全渲染
  const [page, setPage] = useState(0);
  const allOnPage = useRef<HTMLInputElement>(null);

  // 批量删除同样是两步确认：第一下只进入待确认态，3 秒内再点一下才真删
  useEffect(() => {
    if (!confirmBulkDelete) return;
    const timer = window.setTimeout(() => setConfirmBulkDelete(false), 3000);
    return () => window.clearTimeout(timer);
  }, [confirmBulkDelete]);

  // 调度读数 3 秒一拍：它是"这一刻谁在跑、谁在冷却"的读数，不是历史报表
  useEffect(() => {
    let active = true;
    const tick = () => {
      fetchProxyPoolStats()
        .then((next) => {
          if (active) setStats(next);
        })
        .catch(() => undefined);
    };
    tick();
    const timer = window.setInterval(tick, 3000);
    return () => {
      active = false;
      window.clearInterval(timer);
    };
  }, []);

  // 可用 = 启用、地址填了、且此刻不在冷却里。账上没读数的按可用算：
  // 还没发过请求的代理不该显示成坏了
  const available = config.proxyPool.proxies.filter((entry) => {
    if (!entry.enabled || !entry.url.trim()) return false;
    const stat = stats.find((candidate) => candidate.id === entry.id);
    return !stat || stat.coolingMs === 0;
  }).length;

  const total = config.proxyPool.proxies.length;
  // 页号在渲染时钳位：末页被删空之后停在原地，而不是停在一页不存在的位置上
  const current = clampPage(page, total, PROXY_PAGE_SIZE);
  const visible = pageSlice(config.proxyPool.proxies, current, PROXY_PAGE_SIZE);
  const visibleIds = visible.map((entry) => entry.id);
  const pageAllOn = allSelected(selected, visibleIds);

  // 半选那一格只能命令式设（React 没有 indeterminate 属性）
  useEffect(() => {
    if (allOnPage.current) {
      allOnPage.current.indeterminate = !pageAllOn && visibleIds.some((id) => selected.includes(id));
    }
  }, [selected, pageAllOn, visibleIds]);

  function patchPool(patch: Partial<AppConfig["proxyPool"]>) {
    void updateConfig({ proxyPool: { ...config.proxyPool, ...patch } });
  }

  function addProxy() {
    const id = `px-${Date.now().toString(36)}`;
    patchPool({
      proxies: [
        ...config.proxyPool.proxies,
        { id, name: `代理 ${config.proxyPool.proxies.length + 1}`, url: "", enabled: true, weight: 1 },
      ],
    });
  }

  function patchProxy(id: string, patch: Partial<ProxyEntry>) {
    patchPool({
      proxies: config.proxyPool.proxies.map((entry) =>
        entry.id === id ? { ...entry, ...patch } : entry,
      ),
    });
  }

  /** 删掉一批代理。点名它们的绑定同步退回继承，避免留下一堆"永远解析失败"的死绑定 */
  function removeProxies(ids: string[]) {
    if (ids.length === 0) return;
    const gone = new Set(ids);
    patchPool({ proxies: config.proxyPool.proxies.filter((entry) => !gone.has(entry.id)) });
    const sweep = (binding: string) => (gone.has(binding) ? "" : binding);
    void updateConfig({
      proxyDefault: sweep(config.proxyDefault),
      proxy: sweep(config.proxy),
      profiles: config.profiles.map((profile) => ({
        ...profile,
        proxy: sweep(profile.proxy),
        proxyByModel: Object.fromEntries(
          Object.entries(profile.proxyByModel).map(([model, binding]) => [model, sweep(binding)]),
        ),
      })),
    });
    setSelected((previous) => previous.filter((id) => !gone.has(id)));
  }

  /** 批量启用/停用：一次改一组，走同一个 patchPool */
  function setEnabled(ids: string[], enabled: boolean) {
    const gone = new Set(ids);
    patchPool({
      proxies: config.proxyPool.proxies.map((entry) =>
        gone.has(entry.id) ? { ...entry, enabled } : entry,
      ),
    });
  }

  async function runImport() {
    const text = importText;
    if (!text.trim()) return;
    setImporting(false);
    setImportText("");
    try {
      const rows = await proxyImport(text);
      const accepted = rows.filter((row) => row.reason === null);
      if (accepted.length > 0) {
        const stamp = Date.now().toString(36);
        patchPool({
          proxies: [
            ...config.proxyPool.proxies,
            ...accepted.map((row, index) => ({
              id: `px-${stamp}-${index}`,
              name: row.name,
              url: row.url,
              enabled: true,
              weight: 1,
            })),
          ],
        });
      }
      // 新条目接在列表末尾：跳到最后那一页，让人看见刚落地的到底是哪些
      setPage(pageCount(config.proxyPool.proxies.length + accepted.length, PROXY_PAGE_SIZE) - 1);
      const rejected = rows.filter((row) => row.reason !== null);
      const tail = rejected.slice(0, 3).map((row) => `${row.url}（${row.reason}）`).join("；");
      setImportReport(
        rejected.length === 0
          ? `已加入 ${accepted.length} 条。`
          : `已加入 ${accepted.length} 条，挡下 ${rejected.length} 条：${tail}${rejected.length > 3 ? "…" : ""}`,
      );
    } catch (cause) {
      setImportReport(cause instanceof Error ? cause.message : String(cause));
    }
  }

  async function runTestAll() {
    setTestingAll(true);
    setTestReport(null);
    try {
      const outcomes = await proxyPoolTestAll();
      const failed = outcomes.filter((outcome) => !outcome.ok);
      const slowest = outcomes.reduce((max, outcome) => Math.max(max, outcome.ms), 0);
      setTestReport(
        failed.length === 0
          ? `测完 ${outcomes.length} 条，全部通路正常（最慢 ${slowest}ms）。`
          : `测完 ${outcomes.length} 条：通 ${outcomes.length - failed.length}、连不上 ${failed.length}（${failed
              .slice(0, 3)
              .map((outcome) => outcome.name)
              .join("、")}${failed.length > 3 ? "…" : ""}）。`,
      );
      // 探测结果直接改了账本，别等下一拍
      fetchProxyPoolStats()
        .then((next) => setStats(next))
        .catch(() => undefined);
    } catch (cause) {
      setTestReport(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setTestingAll(false);
    }
  }

  return (
    <div className="space-y-6">
      <section className="rounded-xl border border-border p-4">
        <p className="text-base font-medium text-foreground">代理列表</p>
        <p className="mt-0.5 text-xs leading-5 text-muted-foreground">
          http:// 与 socks5:// 两种协议；可带用户名密码（http://user:pass@host:port）。
          删除一条代理会把它身上的绑定退回继承。绑定「代理池」的一发最多自动换 3 条路——
          只在这条代理一个头都没拿到、还没往界面吐出任何字节时换。
          「全部测试」是真探测：测通的那条立刻解除冷却，测不通的照实记账。
        </p>
        <div className="mt-3 space-y-2">
          {visible.map((entry) => (
            <ProxyRow
              key={entry.id}
              entry={entry}
              stat={stats.find((stat) => stat.id === entry.id)}
              selectable={managing}
              selected={selected.includes(entry.id)}
              onSelect={(id, on) =>
                setSelected((previous) => (on ? [...previous, id] : previous.filter((item) => item !== id)))
              }
              onPatch={patchProxy}
              onRemove={() => removeProxies([entry.id])}
            />
          ))}
          {total === 0 ? (
            <p className="rounded-lg border border-dashed border-border px-3 py-6 text-center text-sm text-muted-foreground">
              还没有代理。加一条、或「批量添加」粘一串地址，再在下方选全局绑定或去服务商档案里按服务商/模型绑定。
            </p>
          ) : null}
        </div>

        <Pager page={page} total={total} size={PROXY_PAGE_SIZE} onPage={setPage} />

        {managing ? (
          <div className="mt-3 flex flex-wrap items-center gap-2 rounded-lg border border-border bg-accent/40 px-3 py-2">
            <input
              ref={allOnPage}
              type="checkbox"
              checked={pageAllOn}
              aria-label="全选本页"
              className="size-4 shrink-0 accent-brand"
              onChange={(event) =>
                setSelected((previous) => mergeSelection(previous, visibleIds, event.target.checked))
              }
            />
            <span className="text-xs text-muted-foreground">
              全选本页 · 已选 {selected.length} / {total} 条
            </span>
            <Button size="sm" variant="subtle" disabled={selected.length === 0} onClick={() => setEnabled(selected, true)}>
              启用
            </Button>
            <Button size="sm" variant="subtle" disabled={selected.length === 0} onClick={() => setEnabled(selected, false)}>
              停用
            </Button>
            {confirmBulkDelete ? (
              <Button
                size="sm"
                variant="subtle"
                className="text-destructive"
                onClick={() => {
                  removeProxies(selected);
                  setConfirmBulkDelete(false);
                }}
              >
                再点一次确认删除
              </Button>
            ) : (
              <Button
                size="sm"
                variant="subtle"
                className="text-destructive"
                disabled={selected.length === 0}
                onClick={() => setConfirmBulkDelete(true)}
              >
                删除
              </Button>
            )}
          </div>
        ) : null}

        {importing ? (
          <div className="mt-3 space-y-2">
            <textarea
              rows={5}
              spellCheck={false}
              value={importText}
              aria-label="批量导入地址"
              placeholder={"一行一条，例如：\nhttp://127.0.0.1:7890 本机\nsocks5://user:pass@1.2.3.4:1080#东京"}
              className="w-full rounded-lg border border-input bg-background px-3 py-2 font-mono text-sm leading-6 text-foreground outline-none transition-colors focus-visible:border-brand/50 focus-visible:ring-2 focus-visible:ring-ring/35"
              onChange={(event) => setImportText(event.target.value)}
            />
            <div className="flex flex-wrap items-center gap-2">
              <Button size="sm" disabled={importText.trim().length === 0} onClick={() => void runImport()}>
                导入
              </Button>
              <span className="text-xs text-muted-foreground">
                重复的与地址不对的会被挡下并说明原因；一次最多 100 条。
              </span>
            </div>
          </div>
        ) : null}

        {importReport ? (
          <p className="mt-2 text-xs leading-5 text-muted-foreground">{importReport}</p>
        ) : null}
        {testReport ? <p className="mt-2 text-xs leading-5 text-muted-foreground">{testReport}</p> : null}

        <div className="mt-3 flex flex-wrap items-center gap-2">
          <Button size="sm" variant="subtle" onClick={addProxy}>
            <Plus className="size-3.5" />
            添加代理
          </Button>
          <Button
            size="sm"
            variant="subtle"
            onClick={() => {
              setImporting((previous) => !previous);
              setImportReport(null);
            }}
          >
            {importing ? "收起批量导入" : "批量添加"}
          </Button>
          <Button
            size="sm"
            variant="subtle"
            disabled={testingAll || config.proxyPool.proxies.length === 0}
            onClick={() => void runTestAll()}
          >
            {testingAll ? "测试中…" : "全部测试"}
          </Button>
          <Button
            size="sm"
            variant="subtle"
            disabled={config.proxyPool.proxies.length === 0}
            onClick={() => {
              setManaging((previous) => !previous);
              setSelected([]);
              setConfirmBulkDelete(false);
            }}
          >
            {managing ? "退出批量管理" : "批量管理"}
          </Button>
        </div>
      </section>

      <section className="rounded-xl border border-border p-4">
        <p className="text-base font-medium text-foreground">全局绑定与均衡</p>
        <div className="mt-3 space-y-3.5">
          <Field
            label="全局默认绑定"
            hint="服务商没写绑定时用它；服务商写了自己的或按模型覆盖了，就压过这里。"
          >
            <Select
              value={config.proxyDefault || GLOBAL_INHERIT}
              onValueChange={(value) =>
                void updateConfig({ proxyDefault: value === GLOBAL_INHERIT ? "" : value })
              }
            >
              <SelectTrigger className="w-full text-base">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value={GLOBAL_INHERIT}>直连（默认）</SelectItem>
                <BindingOptions config={config} />
              </SelectContent>
            </Select>
          </Field>

          <Field
            label="均衡策略"
            hint={
              <>
                绑定「代理池」的请求在启用的代理之间按它挑。连续 3 次
                <span className="text-foreground">连不上</span>的代理自动冷却（30 秒起倍增，封顶 10 分钟）；
                服务商回的状态码与中途掐流都不算代理的错。此刻可用{" "}
                <span className="text-foreground">{available}</span> / {config.proxyPool.proxies.length} 条。
              </>
            }
          >
            <div className="grid grid-cols-2 gap-2">
              {STRATEGIES.map((strategy) => (
                <button
                  key={strategy.value}
                  type="button"
                  onClick={() => patchPool({ strategy: strategy.value })}
                  className={cn(
                    "rounded-lg border px-2.5 py-2 text-left transition-colors",
                    config.proxyPool.strategy === strategy.value
                      ? "border-brand/40 bg-brand/10"
                      : "border-border hover:bg-accent",
                  )}
                >
                  <span className="block text-sm font-medium text-foreground">{strategy.label}</span>
                  <span className="mt-0.5 block text-2xs leading-4 text-muted-foreground">
                    {strategy.desc}
                  </span>
                </button>
              ))}
            </div>
          </Field>

          <Field
            label="绕过名单"
            hint="这些域后缀的主机不走代理（example.com 覆盖 api.example.com）。逗号或换行分隔。localhost / 127.0.0.1 恒在名单内，不用写。"
          >
            <textarea
              rows={2}
              spellCheck={false}
              value={config.proxyBypass.join(", ")}
              placeholder="例如：internal.example.com, another.test"
              className="w-full rounded-lg border border-input bg-background px-3 py-2 text-base leading-6 text-foreground outline-none transition-colors focus-visible:border-brand/50 focus-visible:ring-2 focus-visible:ring-ring/35"
              onChange={(event) =>
                void updateConfig({
                  proxyBypass: event.target.value
                    .split(/[,，\n]/)
                    .map((item) => item.trim())
                    .filter((item) => item.length > 0),
                })
              }
            />
          </Field>
        </div>
      </section>
    </div>
  );
}

function ProxyRow({
  entry,
  stat,
  selectable,
  selected,
  onSelect,
  onPatch,
  onRemove,
}: {
  entry: ProxyEntry;
  /** 此刻的调度读数。没有 = 这条还没上路过路 */
  stat?: ProxyStat;
  /** 批量管理态：这一行改用勾选框参与选择，行内的测试/启停/删除让位给底部那一排 */
  selectable?: boolean;
  selected?: boolean;
  onSelect?: (id: string, on: boolean) => void;
  onPatch: (id: string, patch: Partial<ProxyEntry>) => void;
  onRemove: (id: string) => void;
}) {
  const [testing, setTesting] = useState(false);
  const [result, setResult] = useState<string | null>(null);
  const [confirmDelete, setConfirmDelete] = useState(false);

  // 两步确认删除：第一下只进入待确认态，3 秒内再点一下才真删
  useEffect(() => {
    if (!confirmDelete) return;
    const timer = setTimeout(() => setConfirmDelete(false), 3000);
    return () => clearTimeout(timer);
  }, [confirmDelete]);

  const urlError =
    entry.url.trim().length === 0
      ? null
      : /^[a-zA-Z][a-zA-Z0-9+.-]*:\/\/.+/.test(entry.url.trim())
        ? null
        : "要带协议前缀（http:// 或 socks5://）";

  async function test() {
    setTesting(true);
    setResult(null);
    try {
      setResult(await proxyTest(entry.id));
    } catch (cause) {
      setResult(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setTesting(false);
    }
  }

  return (
    <div className="space-y-1.5">
      <div className="flex flex-wrap items-center gap-2">
        {selectable ? (
          <input
            type="checkbox"
            checked={selected === true}
            aria-label={`选中 ${entry.name}`}
            className="size-4 shrink-0 accent-brand"
            onChange={(event) => onSelect?.(entry.id, event.target.checked)}
          />
        ) : (
          <div className="flex size-8 shrink-0 items-center justify-center rounded-lg bg-brand/10 text-brand-text">
            <Globe className="size-4" />
          </div>
        )}
        <input
          type="text"
          value={entry.name}
          aria-label="代理名称"
          spellCheck={false}
          className={cn(inputClass, "w-[130px] shrink-0")}
          onChange={(event) => onPatch(entry.id, { name: event.target.value })}
        />
        <input
          type="text"
          value={entry.url}
          aria-label="代理地址"
          spellCheck={false}
          placeholder="http://127.0.0.1:7890"
          className={cn(inputClass, "min-w-[190px] flex-1 font-mono text-sm", urlError && "border-destructive/60")}
          onChange={(event) => onPatch(entry.id, { url: event.target.value })}
        />
        <input
          type="number"
          min={1}
          max={100}
          value={entry.weight || 1}
          aria-label={`权重 ${entry.name}`}
          title="轮询与随机按它分配；自适应与最少使用不看权重"
          className={cn(inputClass, "w-[64px] shrink-0 text-center")}
          onChange={(event) =>
            onPatch(entry.id, { weight: Number(event.target.value) || 0 })
          }
        />
        {/* 批量管理态：行内的测试/启停/删除让位给底部那一排，行才不会挤成两行 */}
        {selectable ? null : (
          <>
            <Button variant="subtle" size="sm" disabled={testing || urlError !== null || !entry.url.trim()} onClick={() => void test()}>
              {testing ? "测试中…" : "测试"}
            </Button>
            <CapabilityToggle
              label={`启用 ${entry.name}`}
              enabled={entry.enabled}
              onToggle={() => onPatch(entry.id, { enabled: !entry.enabled })}
            />
            {confirmDelete ? (
              <Button
                variant="subtle"
                size="icon"
                aria-label={`再点一次确认删除 ${entry.name}`}
                className="text-destructive"
                onClick={() => onRemove(entry.id)}
              >
                <Trash2 className="size-3.5" />
              </Button>
            ) : (
              <Button variant="subtle" size="icon" aria-label={`删除 ${entry.name}`} onClick={() => setConfirmDelete(true)}>
                <Trash2 className="size-3.5" />
              </Button>
            )}
          </>
        )}
      </div>
      <p className="flex flex-wrap items-center gap-x-3 pl-10 text-xs leading-5 text-muted-foreground">
        {stat && stat.total > 0 ? (
          <>
            <span>上路 {stat.total}</span>
            {stat.inflight > 0 ? <span>在跑 {stat.inflight}</span> : null}
            <span>通 {stat.reached}</span>
            {stat.unreachable > 0 ? (
              <span className="text-destructive">连不上 {stat.unreachable}</span>
            ) : null}
            {stat.interrupted > 0 ? <span>掐流 {stat.interrupted}</span> : null}
            <span>{stat.headMs === null ? "头 未测" : `头 ${ms(stat.headMs)}`}</span>
            <span>{stat.ttftMs === null ? "首字 未测" : `首字 ${ms(stat.ttftMs)}`}</span>
            {stat.coolingMs > 0 ? (
              <span className="text-destructive">冷却中 剩 {secs(stat.coolingMs)}</span>
            ) : null}
          </>
        ) : (
          <span>还没有读数</span>
        )}
      </p>
      {urlError ? (
        <p className="pl-10 text-xs text-destructive">{urlError}</p>
      ) : result ? (
        <p className="pl-10 text-xs leading-5 text-muted-foreground">{result}</p>
      ) : null}
    </div>
  );
}
