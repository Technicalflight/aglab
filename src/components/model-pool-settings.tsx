import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { IconRefresh as RefreshCw, IconSearch as Search } from "@tabler/icons-react";

import { getDecisionSystem } from "@/lib/decision";
import { CapabilityToggle } from "@/components/ui/capability-toggle";
import {
  fetchPoolCatalog,
  fetchPoolStats,
  type PoolCatalogEntry,
  type PoolMemberStat,
} from "@/lib/chat-transport";
import { catalogFingerprint, readCatalogCache, writeCatalogCache } from "@/lib/model-catalog";
import { useChatStore } from "@/store/chat-store";
import { capabilitiesOf } from "@/lib/model-capabilities";
import { ModelIcon } from "@/components/model-icon";
import { cn } from "@/lib/utils";
import type { ModelPool, PoolMember, PoolMode, PoolStrategy } from "@/types/chat";
import { FormColumn } from "@/components/ui/content-column";

const MODES: Array<{ value: PoolMode; label: string; desc: string }> = [
  {
    value: "off",
    label: "关闭",
    desc: "池子只在设置页躺着，请求照旧走顶层的模型",
  },
  {
    value: "auto",
    label: "智能调度",
    desc: "同一话题粘住一个成员保住服务商缓存，不同话题之间按策略分流",
  },
  {
    value: "pinned",
    label: "手动指定",
    desc: "固定用你选的那个成员，不再自动轮换",
  },
  {
    value: "decision",
    label: "决策模型",
    desc: "让一个模型看请求内容替你挑；挑不动退回调度器",
  },
];

const STRATEGIES: Array<{ value: PoolStrategy; label: string; desc: string }> = [
  {
    value: "failover",
    label: "优先级转移",
    desc: "永远先用列表里第一个健康的成员：主成员连挂进冷却自动滑到下一个，恢复后新话题回到主位",
  },
  {
    value: "round_robin",
    label: "加权轮询",
    desc: "按权重均匀轮换（平滑加权算法）：权重 5/1/1 也不会连发五次",
  },
  {
    value: "least_used",
    label: "最少并发",
    desc: "谁手里正在跑的回合最少就给谁，不看权重",
  },
  {
    value: "random",
    label: "加权随机",
    desc: "按权重随机抽一家",
  },
];

/** 成员在池里的唯一定位。pinned 与成员表都用它对齐 */
function sameMember(
  a: { profileId: string; model: string },
  b: { profileId: string; model: string },
) {
  return a.profileId === b.profileId && a.model === b.model;
}

/** 目录的指纹与本地缓存住在 `src/lib/model-catalog.ts`：
 *  模型池页与子助理页共用同一份取数与缓存，两处各养一份就是两个真相 */

export function ModelPoolSettings() {
  const config = useChatStore((s) => s.config);
  const updateConfig = useChatStore((s) => s.updateConfig);
  const pool = config.modelPool;

  const [catalog, setCatalog] = useState<PoolCatalogEntry[] | null>(null);
  const [catalogError, setCatalogError] = useState<string | null>(null);
  // 模型目录的搜索：按模型名大小写不敏感过滤；一张卡里没匹配到任何模型的整卡
  // 隐藏，匹配到的卡照常走分页（匹配数不多时自然单页）
  const [catalogQuery, setCatalogQuery] = useState("");
  const catalogQueryTrimmed = catalogQuery.trim().toLowerCase();
  const visibleCatalog = (catalog ?? [])
    .map((entry) => ({
      entry,
      models: catalogQueryTrimmed
        ? entry.models.filter((model) => model.toLowerCase().includes(catalogQueryTrimmed))
        : entry.models,
    }))
    .filter(({ models }) => catalogQueryTrimmed === "" || models.length > 0);
  const [loadingCatalog, setLoadingCatalog] = useState(false);
  // 服务商配置指纹：目录内容只由它决定。挂在 useMemo 上，无关配置更新不会让它变
  const endpointFingerprint = useMemo(() => catalogFingerprint(config), [config]);
  // 拉取序号：指纹连续变化时会同时在飞多份请求，只有最新一份能落笔
  const fetchSeq = useRef(0);
  const [stats, setStats] = useState<PoolMemberStat[]>([]);
  // 决策层的可用性读数（decision 模式的展示格）。isAvailable 是 Provider 的同步口径：
  // Laya 有服务商就算可用（每次 decide 自带超时与失败处理），Jev 看开关+密钥装配
  const [decisionAvailability, setDecisionAvailability] = useState({ laya: false, jev: false });

  useEffect(() => {
    const read = () => {
      try {
        const system = getDecisionSystem();
        setDecisionAvailability({
          laya: system.laya.isAvailable,
          jev: system.jev?.isAvailable ?? false,
        });
      } catch {
        // 决策层装配失败不是池子页面崩掉的充分理由：这一格显示"不可用"就是实话
      }
    };
    read();
    const timer = window.setInterval(read, 5000);
    return () => window.clearInterval(timer);
  }, []);

  const updatePool = useCallback(
    (patch: Partial<ModelPool>) => {
      void updateConfig({ modelPool: { ...config.modelPool, ...patch } });
    },
    [config.modelPool, updateConfig],
  );

  const refreshCatalog = useCallback(() => {
    const seq = ++fetchSeq.current;
    setLoadingCatalog(true);
    setCatalogError(null);
    fetchPoolCatalog()
      .then((entries) => {
        if (seq !== fetchSeq.current) return; // 已有更新的一份在路上，这份旧回答作废
        setCatalog(entries);
        setLoadingCatalog(false);
        // 拉到的目录连同当时的服务商指纹一起落本地，下次进页面直接显示
        writeCatalogCache({ fingerprint: endpointFingerprint, entries, savedAt: Date.now() });
      })
      .catch((cause) => {
        if (seq !== fetchSeq.current) return;
        setCatalogError(cause instanceof Error ? cause.message : String(cause));
        setLoadingCatalog(false);
        // 整体拉取失败（provider 没配好之类）时退回缓存里的上一份，页面不至于空白
        const cached = readCatalogCache();
        if (cached) setCatalog(cached.entries);
      });
  }, [endpointFingerprint]);

  // 目录的自动拉取只发生在两种时刻：第一次进来还没有可用缓存（指纹对不上），
  // 或者服务商配置变了（指纹变了）。其余时候进页面直接显示本地缓存，
  // 不把所有服务商重新轮一遍。手动「刷新模型目录」按钮永远可以强制重拉
  useEffect(() => {
    const cached = readCatalogCache();
    if (cached && cached.fingerprint === endpointFingerprint) {
      setCatalog(cached.entries);
      return;
    }
    refreshCatalog();
  }, [refreshCatalog, endpointFingerprint]);

  // 调度读数 3 秒一拍：它是"这一刻谁在忙"的读数，不是历史报表
  useEffect(() => {
    let active = true;
    const tick = () => {
      fetchPoolStats()
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

  const profileName = (profileId: string) => {
    if (profileId === "") return "当前连接";
    return config.profiles.find((profile) => profile.id === profileId)?.name ?? "已删除的档案";
  };

  const statOf = (member: { profileId: string; model: string }) =>
    stats.find((stat) => sameMember(stat, member));

  const addMember = (profileId: string, model: string) => {
    if (pool.members.some((member) => sameMember(member, { profileId, model }))) return;
    updatePool({
      members: [...pool.members, { profileId, model, weight: 1, enabled: true }],
    });
  };

  const patchMember = (index: number, patch: Partial<PoolMember>) => {
    updatePool({
      members: pool.members.map((member, i) => (i === index ? { ...member, ...patch } : member)),
    });
  };

  const removeMember = (index: number) => {
    const removed = pool.members[index];
    const members = pool.members.filter((_, i) => i !== index);
    // 指定的成员被移除时，pinned 一起撤：留着它就是一格没人能解释的状态
    const pinned = removed && pool.pinned && sameMember(pool.pinned, removed) ? null : pool.pinned;
    updatePool({ members, pinned });
  };

  const enabledMembers = pool.members.filter((member) => member.enabled);

  return (
    <FormColumn>
      <h1 className="text-2xl font-semibold tracking-tight text-foreground">模型池</h1>
      <p className="mt-1 text-sm leading-6 text-muted-foreground">
        1. 选模式与分流策略； 2. 添加成员：从服务商档案里挑模型、调权重； 3.
        保存后每一发请求按策略在成员间分。成员跟着档案现值走，改档案即改成员。
      </p>

      <div className="mt-8">
        <h2 className="text-lg font-semibold tracking-tight text-foreground">模式</h2>
        <div className="mt-3 grid grid-cols-4 gap-3">
          {MODES.map((mode) => {
            const active = pool.mode === mode.value;
            return (
              <button
                key={mode.value}
                type="button"
                aria-pressed={active}
                onClick={() => void updateConfig({ modelPool: { ...pool, mode: mode.value } })}
                className={cn(
                  "rounded-xl border p-3 text-left outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
                  active
                    ? "border-brand ring-1 ring-brand/50"
                    : "border-border hover:border-brand/40",
                )}
              >
                <p
                  className={cn(
                    "text-sm",
                    active ? "font-medium text-foreground" : "text-muted-foreground",
                  )}
                >
                  {mode.label}
                </p>
                <p className="mt-1 text-xs leading-5 text-muted-foreground/85">{mode.desc}</p>
              </button>
            );
          })}
        </div>
      </div>

      {pool.mode === "auto" ? (
        <div className="mt-8">
          <h2 className="text-lg font-semibold tracking-tight text-foreground">调度策略</h2>
          <p className="mt-1 text-sm leading-6 text-muted-foreground">
            策略决定的是新话题第一次发给谁；话题一旦开跑就粘住那个成员，
            粘的人进冷却或被移除才换人（服务商缓存按账号×模型分域，话题内换人等于把命中率交给运气）。
            连续失败 3 次的成员自动进冷却（30 秒起步、逐次翻倍），冷却结束自动归队。
          </p>
          <div className="mt-3 space-y-2">
            {STRATEGIES.map((strategy) => {
              const active = pool.strategy === strategy.value;
              return (
                <button
                  key={strategy.value}
                  type="button"
                  aria-pressed={active}
                  onClick={() =>
                    void updateConfig({ modelPool: { ...pool, strategy: strategy.value } })
                  }
                  className={cn(
                    "flex w-full items-start gap-3 rounded-lg border px-3 py-2.5 text-left outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
                    active
                      ? "border-brand ring-1 ring-brand/50"
                      : "border-border hover:border-brand/40",
                  )}
                >
                  <span
                    className={cn(
                      "mt-1 size-2 shrink-0 rounded-full",
                      active ? "bg-brand" : "bg-muted-foreground/40",
                    )}
                  />
                  <span className="min-w-0">
                    <span
                      className={cn(
                        "block text-sm",
                        active ? "font-medium text-foreground" : "text-muted-foreground",
                      )}
                    >
                      {strategy.label}
                    </span>
                    <span className="mt-0.5 block text-xs leading-5 text-muted-foreground/85">
                      {strategy.desc}
                    </span>
                  </span>
                </button>
              );
            })}
          </div>

          <div className="mt-4 flex items-center gap-3 rounded-lg border border-border bg-surface px-3 py-3">
            <p className="min-w-0 flex-1 text-xs leading-5 text-muted-foreground">
              缓存感知首挑：新话题第一次挑人时，优先落到最近刚成功过、缓存还热的成员上——
              新话题与刚完成的话题共享同一份系统提示词与工具声明，热成员身上那段前缀直接命中，
              省一次全价 prefill。只影响新话题的第一发，话题粘住后照旧由亲和账接管。
            </p>
            <CapabilityToggle
              label="缓存感知首挑"
              enabled={pool.cacheAwarePick ?? true}
              onToggle={() =>
                void updateConfig({
                  modelPool: { ...pool, cacheAwarePick: !(pool.cacheAwarePick ?? true) },
                })
              }
            />
          </div>
        </div>
      ) : null}

      {pool.mode === "pinned" ? (
        <div className="mt-8">
          <h2 className="text-lg font-semibold tracking-tight text-foreground">指定成员</h2>
          <p className="mt-1 text-sm leading-6 text-muted-foreground">
            从启用中的成员里挑一个固定下来。聊天输入框的模型选择器也能随时切换。
          </p>
          <div className="mt-3 flex flex-wrap gap-2">
            {enabledMembers.length === 0 ? (
              <p className="text-sm text-muted-foreground">
                池子里还没有启用的成员，先在下面把模型加进来。
              </p>
            ) : (
              enabledMembers.map((member) => {
                const active = pool.pinned ? sameMember(pool.pinned, member) : false;
                return (
                  <button
                    key={`${member.profileId}\u{0}${member.model}`}
                    type="button"
                    aria-pressed={active}
                    onClick={() =>
                      void updateConfig({
                        modelPool: {
                          ...pool,
                          pinned: { profileId: member.profileId, model: member.model },
                        },
                      })
                    }
                    className={cn(
                      "rounded-lg border px-3 py-1.5 text-sm transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
                      active
                        ? "border-brand bg-brand/10 text-brand-text"
                        : "border-border text-muted-foreground hover:border-brand/40 hover:text-foreground",
                    )}
                  >
                    <ModelIcon model={member.model} size={12} />
                    <span className="truncate">{member.model}</span>
                    <span className="shrink-0 text-muted-foreground/70">
                      {profileName(member.profileId)}
                    </span>
                  </button>
                );
              })
            )}
          </div>
        </div>
      ) : null}

      {pool.mode === "decision" ? (
        <div className="mt-8">
          <h2 className="text-lg font-semibold tracking-tight text-foreground">决策层调度</h2>
          <p className="mt-1 text-sm leading-6 text-muted-foreground">
            每发请求前，把候选清单和这段话交给 System 1 决策层——Laya 在本地出结构化判定， Jev
            走云端批量前向，漏斗、阈值与敏感性红线都由决策层自己的配置说了算。
            决策层没开、没答上或答非所选时，这一发由调度器兜底，绝不因为选不出模型而不干活；
            定时任务与编排等后台运行不经决策层，直接走调度器。
          </p>
          <div className="mt-3 flex items-center justify-between gap-3 rounded-lg border border-border bg-surface px-3 py-3">
            <div className="min-w-0">
              <p className="text-sm font-medium text-foreground">决策层此刻的可用性</p>
              <p className="mt-1 text-xs leading-5 text-muted-foreground">
                Laya 本地：{decisionAvailability.laya ? "可用" : "不可用（sidecar 没起或还在加载）"}
                {" · "}
                Jev 云端：{decisionAvailability.jev ? "可用" : "未启用（开关或密钥）"}
              </p>
            </div>
          </div>
          <p className="mt-2 text-xs leading-5 text-muted-foreground">
            决策层的开关、密钥与 Sidecar 在「设置 → 决策层」里配置；判定会照常进决策审计。
          </p>
        </div>
      ) : null}

      <div className="mt-8">
        <div className="flex items-center justify-between">
          <h2 className="text-lg font-semibold tracking-tight text-foreground">
            成员（{pool.members.length}）
          </h2>
          <button
            type="button"
            onClick={refreshCatalog}
            disabled={loadingCatalog}
            className="flex items-center gap-1.5 rounded-lg px-2 py-1 text-xs text-muted-foreground transition-colors hover:bg-accent hover:text-foreground disabled:opacity-45"
          >
            <RefreshCw className={cn("size-3", loadingCatalog && "animate-spin")} />
            刷新模型目录
          </button>
        </div>

        {pool.members.length > 0 ? (
          <div className="mt-3 rounded-lg border border-border bg-surface px-3">
            {pool.members.map((member, index) => {
              const stat = statOf(member);
              return (
                <div
                  key={`${member.profileId}\u{0}${member.model}`}
                  className="flex items-center gap-3 border-b border-border py-3 last:border-b-0"
                >
                  <ModelIcon model={member.model} size={16} />
                  <div className="min-w-0 flex-1">
                    <p className="truncate text-sm font-medium text-foreground">
                      {member.model}
                      <span className="ml-1.5 text-2xs font-normal text-muted-foreground">
                        {capabilitiesOf(
                          member.model,
                          config.profiles.flatMap((profile) => profile.models),
                        )
                          .map((cap) =>
                            cap === "image" ? "生图" : cap === "video" ? "视频" : "对话",
                          )
                          .join(" / ")}
                      </span>
                    </p>
                    <p className="mt-0.5 truncate text-xs text-muted-foreground">
                      {profileName(member.profileId)}
                      {stat
                        ? ` · 已调度 ${stat.total} · 并发 ${stat.inflight}${
                            stat.failures > 0 ? ` · 连续失败 ${stat.failures}` : ""
                          }${stat.coolingMs > 0 ? " · 冷却中" : ""}`
                        : ""}
                    </p>
                  </div>
                  <label className="flex shrink-0 items-center gap-1.5 text-xs text-muted-foreground">
                    权重
                    <input
                      type="number"
                      min={1}
                      max={100}
                      value={member.weight}
                      onChange={(event) => {
                        const next = Math.round(Number(event.target.value));
                        if (Number.isFinite(next)) {
                          patchMember(index, { weight: Math.min(Math.max(next, 1), 100) });
                        }
                      }}
                      className="h-8 w-16 rounded-lg border border-input bg-background px-2 text-sm text-foreground outline-none focus-visible:border-brand/50"
                    />
                  </label>
                  <CapabilityToggle
                    label={`启用 ${member.model}`}
                    enabled={member.enabled}
                    onToggle={() => patchMember(index, { enabled: !member.enabled })}
                  />
                  <button
                    type="button"
                    aria-label={`移除 ${member.model}`}
                    onClick={() => removeMember(index)}
                    className="shrink-0 rounded-lg px-2 py-1 text-xs text-muted-foreground transition-colors hover:bg-accent hover:text-destructive"
                  >
                    移除
                  </button>
                </div>
              );
            })}
          </div>
        ) : (
          <p className="mt-3 text-sm leading-6 text-muted-foreground">
            还没有成员。下面从拉到的模型目录里挑几个加进来。
          </p>
        )}
      </div>

      <div className="mt-8">
        <h2 className="text-lg font-semibold tracking-tight text-foreground">模型目录</h2>
        <p className="mt-1 text-sm leading-6 text-muted-foreground">
          每张服务商档案（连同当前连接）各拉一次模型列表，结果保存在本地：
          进页面直接显示上一次的结果，服务商配置变更或手动刷新才重新拉取。
        </p>
        {catalogError ? <p className="mt-3 text-sm text-destructive">{catalogError}</p> : null}
        <div className="relative mt-3">
          <Search className="pointer-events-none absolute left-2.5 top-1/2 size-3.5 -translate-y-1/2 text-muted-foreground" />
          <input
            aria-label="搜索模型目录"
            type="text"
            value={catalogQuery}
            onChange={(event) => setCatalogQuery(event.target.value)}
            placeholder="搜模型 ID，如 qwen"
            spellCheck={false}
            className="h-9 w-full max-w-xs rounded-lg border border-input bg-background pl-8 pr-3 text-base text-foreground outline-none transition-colors focus-visible:border-brand/50 focus-visible:ring-2 focus-visible:ring-ring/35"
          />
        </div>
        <div className="mt-3 space-y-3">
          {visibleCatalog.map(({ entry, models }) => (
            <CatalogCard
              key={entry.profileId || "\u{0}current"}
              entry={entry}
              models={models}
              pool={pool}
              onAdd={addMember}
            />
          ))}
          {catalog === null && !catalogError ? (
            <p className="text-sm text-muted-foreground">正在拉取各服务商的模型列表……</p>
          ) : null}
          {catalog !== null && catalogQueryTrimmed !== "" && visibleCatalog.length === 0 ? (
            <p className="text-sm text-muted-foreground">没有匹配的模型。</p>
          ) : null}
        </div>
      </div>
    </FormColumn>
  );
}

/** 模型目录单卡：一个服务商的模型列表按页展示——一个服务商几十个 chip 全量平铺，
 *  页面会被拉得非常长。页码是卡片自己的状态；重新拉取后列表变短由派生 clamp 兜底，
 *  不需要 effect 同步 */
function CatalogCard({
  entry,
  models,
  pool,
  onAdd,
}: {
  entry: PoolCatalogEntry;
  models: string[];
  pool: ModelPool;
  onAdd: (profileId: string, model: string) => void;
}) {
  const PAGE_SIZE = 24;
  const [page, setPage] = useState(0);
  const total = models.length;
  const maxPage = Math.max(0, Math.ceil(total / PAGE_SIZE) - 1);
  const current = Math.min(page, maxPage);
  const visible = models.slice(current * PAGE_SIZE, (current + 1) * PAGE_SIZE);

  return (
    <div className="rounded-lg border border-border bg-surface px-3 py-3">
      <div className="flex items-baseline justify-between gap-3">
        <p className="text-sm font-medium text-foreground">{entry.name}</p>
        <p className="truncate text-xs text-muted-foreground">{entry.baseUrl}</p>
      </div>
      {entry.error ? (
        <p className="mt-1.5 text-xs leading-5 text-destructive">{entry.error}</p>
      ) : total === 0 ? (
        <p className="mt-1.5 text-xs text-muted-foreground">这个服务商没有列出模型。</p>
      ) : (
        <>
          <div className="mt-2 flex flex-wrap gap-1.5">
            {visible.map((model) => {
              const inPool = pool.members.some((member) =>
                sameMember(member, { profileId: entry.profileId, model }),
              );
              return (
                <button
                  key={model}
                  type="button"
                  disabled={inPool}
                  onClick={() => onAdd(entry.profileId, model)}
                  title={inPool ? "已经在池里" : `把 ${model} 加入池子`}
                  className={cn(
                    "rounded-lg border px-2 py-1 font-mono text-xs transition-colors focus-visible:ring-2 focus-visible:ring-ring/45",
                    inPool
                      ? "cursor-default border-border/60 text-muted-foreground/50"
                      : "border-border text-foreground hover:border-brand/50 hover:text-brand-text",
                  )}
                >
                  {model}
                  {inPool ? " ·" : ""}
                </button>
              );
            })}
          </div>
          {maxPage > 0 ? (
            <div className="mt-2.5 flex items-center justify-end gap-2 text-xs text-muted-foreground">
              <span>
                第 {current + 1} / {maxPage + 1} 页 · 共 {total} 个
              </span>
              <button
                type="button"
                aria-label="上一页"
                disabled={current === 0}
                onClick={() => setPage(current - 1)}
                className="rounded-lg px-2 py-0.5 transition-colors hover:bg-accent hover:text-foreground disabled:cursor-default disabled:opacity-40"
              >
                上一页
              </button>
              <button
                type="button"
                aria-label="下一页"
                disabled={current >= maxPage}
                onClick={() => setPage(current + 1)}
                className="rounded-lg px-2 py-0.5 transition-colors hover:bg-accent hover:text-foreground disabled:cursor-default disabled:opacity-40"
              >
                下一页
              </button>
            </div>
          ) : null}
        </>
      )}
    </div>
  );
}
