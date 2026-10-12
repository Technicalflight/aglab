import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  IconDatabaseImport as DatabaseImport,
  IconHistory as History,
  IconNetwork as Network,
  IconSearch as Search,
  IconShieldCheck as ShieldCheck,
} from "@tabler/icons-react";

import { cn } from "@/lib/utils";

/**
 * 助手库分区（分区 2 左轨进入）。把本机已安装的外部 AI 编程助手的配置、
 * 会话与记忆只读导入本客户端，统一管理、检索与（分期）续聊。
 *
 * 设计稿 docs/agent-hub-design.md；当前是第 2 步落地后的骨架：
 * 库已可创建（下方状态卡是真实打库链路），发现/导入/检索按阶段点亮。
 * 原则：适配器驱动、本地优先、增量同步、只读优先、用户授权——
 * 授权闸与扫描清单在阶段 A 第一批代码落地，在此之前不发起任何文件扫描。
 */

interface HubStatus {
  dbPath: string;
  tablesReady: string[];
  tablesMissing: string[];
  ftsReady: boolean;
}

const PHASES: Array<{
  tag: string;
  title: string;
  desc: string;
  Icon: typeof Search;
  state: "active" | "planned";
}> = [
  {
    tag: "阶段 A",
    title: "只读导入 + 检索",
    desc: "自动发现已安装的助手（勾选确认后才扫描导入），会话/记忆统一入库，支持全文搜索与历史引用。",
    Icon: Search,
    state: "active",
  },
  {
    tag: "阶段 B",
    title: "本地网关",
    desc: "监听 127.0.0.1 的本地网关，把对话请求转换为目标助手可用的调用方式，支持流式与工具调用。",
    Icon: Network,
    state: "planned",
  },
  {
    tag: "阶段 C",
    title: "续聊",
    desc: "支持恢复原会话的助手直接以子进程续聊；不支持的自动建新会话并附历史摘要。",
    Icon: History,
    state: "planned",
  },
];

export function AgentsView() {
  const [status, setStatus] = useState<HubStatus | null>(null);
  const [error, setError] = useState<string | null>(null);

  // 真实打库链路：第一次进分区就建库建表（幂等），状态卡显示就绪度
  useEffect(() => {
    invoke<HubStatus>("agent_hub_status")
      .then(setStatus)
      .catch((cause) => setError(cause instanceof Error ? cause.message : String(cause)));
  }, []);

  return (
    <section className="flex min-h-0 flex-1 flex-col items-center overflow-y-auto px-6 py-8">
      <div className="w-full max-w-2xl">
        <header className="flex items-center gap-2.5">
          <span className="flex size-9 items-center justify-center rounded-xl bg-muted">
            <DatabaseImport className="size-5 text-foreground-secondary" />
          </span>
          <div>
            <h1 className="text-lg font-semibold text-foreground">助手库</h1>
            <p className="text-xs leading-4 text-muted-foreground">
              把本机其他 AI 编程助手的会话与记忆请进来——统一管理、搜索、引用，全程本地处理
            </p>
          </div>
        </header>

        {/* 库状态：真实打库链路的读数（schema 幂等，每次进入自动就绪） */}
        <div className="mt-5 rounded-xl border border-border bg-surface p-4">
          <p className="flex items-center gap-1.5 text-sm font-medium text-foreground">
            <ShieldCheck className="size-4 text-success" />
            本地库
            {status ? (
              <span className="ml-auto text-2xs tabular-nums text-muted-foreground">
                {status.tablesReady.length}/9 表就绪 · FTS {status.ftsReady ? "✓" : "…"}
              </span>
            ) : null}
          </p>
          <p className="mt-1.5 break-all text-2xs leading-4 text-muted-foreground">
            {error
              ? `打库失败：${error}`
              : (status?.dbPath ?? "首次进入时创建 agent-hub.db（WAL 模式，含全文索引）")}
          </p>
        </div>

        {/* 阶段路线：设计稿已定，分批点亮 */}
        <div className="mt-4 space-y-2.5">
          {PHASES.map(({ tag, title, desc, Icon, state }) => (
            <div
              key={tag}
              className={cn(
                "flex items-start gap-3 rounded-xl border px-4 py-3",
                state === "active" ? "border-border bg-surface" : "border-border/60 bg-transparent",
              )}
            >
              <span
                className={cn(
                  "mt-0.5 flex size-8 shrink-0 items-center justify-center rounded-lg",
                  state === "active"
                    ? "bg-brand/10 text-brand-text"
                    : "bg-muted text-muted-foreground",
                )}
              >
                <Icon className="size-4" />
              </span>
              <div className="min-w-0 flex-1">
                <p className="flex items-center gap-2 text-sm font-medium text-foreground">
                  {title}
                  <span
                    className={cn(
                      "rounded-md px-1.5 py-0.5 text-2xs leading-4",
                      state === "active"
                        ? "bg-brand/10 text-brand-text"
                        : "bg-muted text-muted-foreground",
                    )}
                  >
                    {tag}
                  </span>
                  {state === "planned" ? (
                    <span className="text-2xs text-muted-foreground">待确认后实施</span>
                  ) : null}
                </p>
                <p className="mt-0.5 text-xs leading-5 text-muted-foreground">{desc}</p>
              </div>
            </div>
          ))}
        </div>

        {/* 隐私底线：进功能先说清，授权闸在阶段 A 第一批代码落地 */}
        <div className="mt-4 rounded-xl border border-border/60 bg-transparent p-4">
          <p className="text-xs font-medium text-foreground">隐私底线</p>
          <ul className="mt-1.5 space-y-1 text-2xs leading-4 text-muted-foreground">
            <li>· 全部数据只在本机处理，不上传云端；原始文件只读，绝不改写。</li>
            <li>· 扫描范围由你逐项授权，敏感路径（密钥、凭证类）默认排除。</li>
            <li>· 识别清单数据驱动，代码里不写死任何具体产品。</li>
          </ul>
        </div>
      </div>
    </section>
  );
}
