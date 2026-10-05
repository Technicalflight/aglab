import { invoke } from "@tauri-apps/api/core";
import type { ChatMessage } from "../types/chat";
import { detectSensitivity } from "./decision/integrations";

export type MemoryScope = "global" | "project" | "session" | "temp";
export type MemoryStatus = "candidate" | "active" | "archived" | "deleted";
/**
 * 一条记忆的外发分级。判据只有一条：这段内容允许被送到推理服务商吗，以什么名义。
 * `public` 照常；`private` 只在你正在问的那一轮里当上下文，不进后台自动外发的材料；
 * `secret` 哪儿都不去——它仍然能被检索到、能在这里看，但模型永远收不到。
 */
export type MemorySensitivity = "public" | "private" | "secret";

export interface MemoryRecord {
  id: string;
  kind: string;
  scope: MemoryScope;
  projectId: string | null;
  status: MemoryStatus;
  importance: number;
  confidence: number;
  stability: "stable" | "volatile";
  source: "user" | "assistant" | "inferred" | "import";
  sensitivity: MemorySensitivity;
  createdAt: string;
  updatedAt: string;
  lastUsedAt: string | null;
  ttlDays: number | null;
  tags: string[];
  supersedes: string[];
  content: string;
  extra: [string, string][];
  /** 事情发生的时间与最近一次真的被用上的时间。Rust 那边是 `skip_serializing_if = none`，
   *  所以这里必须是可选：写成必填就是"字段齐了没人读"的镜像——界面上会显示 `undefined` */
  occurredAt?: string;
  reinforcedAt?: string;
  /** 从哪次对话、哪几条消息里长出来的。只带标识，不带正文 */
  origin?: MemoryOrigin;
}

/** 一条记忆的出处。`entries` 是那场话题里的消息行 id */
export interface MemoryOrigin {
  conversationId: string;
  entries: string[];
  extractedAt: string;
}

export interface MemoryView {
  record: MemoryRecord;
  path: string;
  injections: number;
}

export interface MemoryHit {
  id: string;
  path: string;
  kind: string;
  scope: string;
  projectId: string | null;
  status: string;
  /** 索引行带着它，注入侧才知道这一条能不能出门（读不懂的值按最严的那一档算） */
  sensitivity: MemorySensitivity;
  importance: number;
  confidence: number;
  updatedAt: string;
  content: string;
  injections: number;
  score: number;
  why: string;
}

/** 主动回忆的一条提示：实体图上相关、这一轮却没注入的记忆。只是提示，一个字都不进正文 */
export interface MemoryRecallHint {
  recordId: string;
  content: string;
  score: number;
  /** 第几跳、经由哪个实体，例如「实体命中（第 1 跳）：张三」 */
  reason: string;
  /** true = 这一轮模型已经看到了，不用再提示 */
  injected: boolean;
}

/** 时间线上的一格。`at` 是事情发生的时间，不是这条被写下的时间 */
export interface MemoryTimelineRow {
  at: string;
  recordId: string;
  kind: string;
  entity: string | null;
  content: string;
}

export interface AddArgs {
  content: string;
  kind?: string;
  scope?: string;
  importance?: number;
  tags?: string[];
}

export const memoryAdd = (args: AddArgs) => invoke<MemoryView>("memory_add", { args });

/**
 * 入库后的外发档自动分级（决策层 integrations.sensitivityScan，强制本地判定）。
 * 只降不升：判成 private/secret 就当场改档——密钥进了记忆却还能外发给服务商，
 * 比"这条记忆暂时用不上"严重得多；判成 public 维持后端默认，什么都不做。
 * 判定不了（开关关、sidecar 没起）返回 null，调用方当无事发生——这不是拦截功能，
 * 是给"用户忘了划档"补一道本地的保险
 */
export async function autoTagMemorySensitivity(view: MemoryView): Promise<MemorySensitivity | null> {
  if (view.record.sensitivity !== "public") return null;
  try {
    const verdict = await detectSensitivity(view.record.content);
    if (!verdict || verdict.suggested === "public") {
      return null;
    }
    await memoryEdit(view.record.id, { sensitivity: verdict.suggested });
    return verdict.suggested;
  } catch {
    return null;
  }
}

export const memoryList = () => invoke<MemoryView[]>("memory_list");
export const memorySearch = (query: string) => invoke<MemoryHit[]>("memory_search", { query });
/** 顺实体图带出来的提示（只读）。开关关着时后端就返回空数组，前端不必再判一次 */
export const memoryRecallHints = (query: string, conversationId: string) =>
  invoke<MemoryRecallHint[]>("memory_recall_hints", { query, conversationId });
/** 事件时间线：说过「什么时候发生」的那些记录，按业务时间倒序 */
export const memoryTimeline = (limit?: number) => invoke<MemoryTimelineRow[]>("memory_timeline", { limit });
export const memoryForget = (query: string) => invoke<string>("memory_forget", { query });
export const memoryRebuild = () => invoke<number>("memory_rebuild");
export const memoryStats = () => invoke<MemoryStats>("memory_stats");
export const memoryConfigSet = (patch: Record<string, unknown>) =>
  invoke<MemoryConfig>("memory_config_set", { patch });

/** 改一条记录。候选转正、丢弃、就地改正文都走这一条，真相源仍是 Markdown */
export interface MemoryPatch {
  content?: string;
  importance?: number;
  confidence?: number;
  tags?: string[];
  status?: MemoryStatus;
  /** 这里就是"把这条划进不出去的那一档"的入口。只有 `.md` 里能改 = 界面上没有这条规则 */
  sensitivity?: MemorySensitivity;
}

export const memoryEdit = (id: string, patch: MemoryPatch) =>
  invoke<MemoryView>("memory_edit", { id, patch });
/** 导出一段可移交给另一台机器的 JSON 文本。不碰网络。
 *  给了口令就返回 age 装甲文本（`-----BEGIN AGE ENCRYPTED FILE-----` 开头），
 *  留空 = 不加密，字节与加加密这一格之前完全一致 */
export const memoryExport = (passphrase?: string) =>
  invoke<string>("memory_export", { passphrase: passphrase || null });
/** 导入：文本来自设置页的粘贴框。同 id 已存在就跳过，不覆盖用户手改过的记录。
 *  备份是加密的就必须给口令——解不开就是解不开，不会退回读明文 */
export const memoryImport = (text: string, passphrase?: string) =>
  invoke<number>("memory_import", { text, passphrase: passphrase || null });
/** 导入另一台机器搬过来的文件。读文件这件事只能在后端做，前端没有文件系统权限 */
export const memoryImportFile = (path: string, passphrase?: string) =>
  invoke<number>("memory_import_file", { path, passphrase: passphrase || null });
/** 一键清空：删掉记录，Markdown 归档而不是蒸发（可重建的前提是别把真相源弄丢） */
export const memoryWipe = () => invoke<number>("memory_wipe");

/**
 * "这一条消息不带记忆"的一次性旗标。
 * 它故意不写进 config：配置是长期状态，而这个开关的意义恰好是"只这一次"。
 * 发送时取走并清零，所以忘记清它的唯一后果是下一条不带记忆，不是永远不带
 */
let skipNextTurn = false;

export const setMemorySkipNextTurn = (skip: boolean) => {
  skipNextTurn = skip;
};

export const consumeMemorySkipNextTurn = () => {
  const value = skipNextTurn;
  skipNextTurn = false;
  return value;
};

/** 上一轮真正交给模型的那批记忆，以及每条为什么被选中 */
export interface InjectedItem {
  id: string;
  path: string;
  line: string;
  score: number;
  why: string;
  updatedAt: string;
}

export interface Injection {
  body: string;
  items: InjectedItem[];
  standingTokens: number;
  retrieveTokens: number;
  at: string;
  query: string;
}

export const memoryWhy = (conversationId: string) =>
  invoke<Injection | null>("memory_why", { conversationId });

export interface ExtractSummary {
  stored: number;
  merged: number;
  candidates: number;
  dropped: number;
}

/** 一轮结束后交给提取器的消息。后端自己看配置开关，所以这里可以无条件调用。
 *  `conversationId` 是出处那一条：日志行的 id 由后端自己按话题读，客户端不知道也不该猜 */
export const memoryExtract = (conversationId: string, messages: ChatMessage[]) =>
  invoke<ExtractSummary>("memory_extract", { conversationId, messages });

/** 蒸馏要跑一次 LLM，几秒是常态。命令在 Rust 侧是 async + spawn_blocking，不冻界面 */
export const memoryDistill = () => invoke<DistillSummary>("memory_distill");

/** 蒸馏前的那一页：会喂进服务商哪些日志、各多少字。纯读，不做任何搬运 */
export interface DistillPreviewLog {
  name: string;
  chars: number;
}

export interface DistillPreview {
  logs: DistillPreviewLog[];
  materialChars: number;
}

export const memoryDistillPreview = () => invoke<DistillPreview>("memory_distill_preview");

/** 把在用的记忆渲染成 AGENTS.md 的一节。审阅后自己粘进仓库的约定文件——
 *  稳定规则归仓库随 git 走，记忆只做回忆层，这一条是两层之间的桥 */
export interface AgentsExport {
  markdown: string;
  count: number;
}

export const memoryExportAgentsMd = () => invoke<AgentsExport>("memory_export_agents_md");
/** 反思一次。开关关着时后端一次服务商都不发，回来的计数就全是 0——不是报错 */
export const memoryReflect = () => invoke<ExtractSummary>("memory_reflect");

/** 冲突的四种裁法。键名与 Rust 的 `ConflictChoice` 对齐（camelCase） */
export type ConflictChoice = "newerWins" | "olderWins" | "keepBoth" | "archiveBoth";

/** 一对现在还有实际影响的冲突。`a` 是站着的那条（按 created_at），`b` 是后来主张的那条 */
export interface ConflictPair {
  a: MemoryView;
  b: MemoryView;
  similarity: number;
  floor: number;
  why: string;
  recommendation: ConflictChoice;
}

/** 谁像谁、像到什么程度，由后端算：它读的是索引里的向量与阈值，界面上不该再算一遍 */
export const memoryConflicts = () => invoke<ConflictPair[]>("memory_conflicts");

/** 人选完的一刻才落盘：败者归档、胜者带上取代边，两条正文都留在 Markdown 里 */
export const memoryConflictResolve = (a: string, b: string, choice: ConflictChoice) =>
  invoke<string>("memory_conflict_resolve", { a, b, choice });

/** 一条记忆的来历。这里没有正文字段，出处不该多落一份原文（§"只存标识"） */
export interface MemorySource {
  recordId: string;
  file: string;
  origin: MemoryOrigin | null;
  createdAt: string;
  occurredAt: string | null;
  reinforcedAt: string | null;
  injections: number;
  lastInjectedAt: string | null;
}

export const memorySource = (id: string) => invoke<MemorySource>("memory_source", { id });

export interface DistillSummary {
  archivedRecords: number;
  archivedLogs: number;
  distilled: number;
  overbudget: boolean;
}

export interface MemoryConfig {
  enabled: boolean;
  autoInject: boolean;
  autoExtract: boolean;
  alwaysBudgetTokens: number;
  retrieveBudgetTokens: number;
  searchLimit: number;
  globalLimitChars: number;
  projectLimitChars: number;
  dailyKeepDays: number;
  distillAfterDays: number;
  /** 多少天没被用上，新鲜度对折一次。只影响排序里"新鲜"那一项，不改写任何一条记忆的事实 */
  decayHalfLifeDays: number;
  /** 候选区自然衰减：模型提的、一直没人点头的候选，过了这么多天且双低就归档。0 = 关 */
  candidateTtlDays: number;
  /** 空闲自动蒸馏：开着时前端每半小时看一眼 needsDistill，命中就自动跑，至多一天一次 */
  autoDistill: boolean;
  autoAcceptConfidence: number;
  autoAcceptImportance: number;
  dedupeSimilarity: number;
  /** 主动回忆：只在记忆页提示"有这几条相关但本轮没用上"。它不是第二条注入通道，
   *  开与关发给服务商的字节是一样的 */
  proactiveRecall: boolean;
  /** 反思：开着才允许「反思一次」发请求。产物只进候选区 */
  reflectEnabled: boolean;
  cloudSync: boolean;
}

/** memory_stats 返回的那一份。overBudget 是超限额的文件名列表，不是布尔：
 *  报"哪一份超了、超了多少"才谈得上让用户决定要不要蒸馏 */
export interface MemoryStats {
  total: number;
  active: number;
  candidates: number;
  archived: number;
  root: string;
  enabled: boolean;
  cloudSync: boolean;
  overBudget: string[];
  needsDistill: boolean;
  expiredNow: number;
  dailyKeepDays: number;
  distillAfterDays: number;
}

export const scopeLabel: Record<MemoryScope, string> = {
  global: "全局",
  project: "项目",
  session: "话题",
  temp: "临时",
};

/** 斜杠命令的执行结果：outcome 决定要不要把它说给模型听。 */
export type CommandOutcome =
  | { handled: false }
  | { handled: true; kind: "memory"; text: string; tone: "ok" | "warn" };

const SHOW_LIMIT = 8;

function formatHits(hits: MemoryHit[]): string {
  if (hits.length === 0) return "没有匹配的记忆。";
  return hits
    .slice(0, SHOW_LIMIT)
    .map((hit) => `- \`${hit.id}\` [${hit.scope}] ${hit.content.replace(/\s+/g, " ")}（${hit.score.toFixed(2)}）`)
    .join("\n")
    + (hits.length > SHOW_LIMIT ? `\n\n_还有 ${hits.length - SHOW_LIMIT} 条，用 /memory search 缩窄一下。_` : "");
}

function formatViews(views: MemoryView[]): string {
  const live = views.filter((view) => view.record.status !== "archived");
  if (live.length === 0) return "记忆里还是空的。用 `/remember 内容` 记一条。";
  const shown = live.slice(0, SHOW_LIMIT);
  return (
    shown
      .map(
        (view) =>
          `- \`${view.record.id}\` [${scopeLabel[view.record.scope]}] ${view.record.content.replace(/\s+/g, " ")}（用过 ${view.injections} 次）`,
      )
      .join("\n") + (live.length > shown.length ? `\n\n_共 ${live.length} 条，这里只显示前 ${shown.length} 条。_` : "")
  );
}

function formatWhy(shot: Injection): string {
  if (shot.items.length === 0) {
    return `这一轮没有按相关性挑出记忆${shot.query ? `（提法：${shot.query}）` : ""}。常驻段（画像 / 人格 / 硬规则）是每轮都给的，不在这个列表里。`;
  }
  const rows = shot.items.map(
    (item, index) =>
      `${index + 1}. ${item.line.replace(/\n+/g, " ")}\n   来源 \`${item.path}\` · 更新 ${item.updatedAt} · 评分 ${item.score.toFixed(2)}\n   为什么：${item.why}`,
  );
  return `上一轮注入的记忆（检索段 ${shot.retrieveTokens} tokens）：\n\n${rows.join("\n")}`;
}

/**
 * 吃掉 /remember、/forget、/memory 这一族命令。
 * 必须在发给模型之前拦下来：这些是给客户端的指令，不是给模型的提示词。
 */
export async function runMemoryCommand(input: string, conversationId: string): Promise<CommandOutcome> {
  const text = input.trim();
  if (!text.startsWith("/")) return { handled: false };
  const [verb, ...rest] = text.slice(1).split(/\s+/);
  const arg = rest.join(" ").trim();

  try {
    if (verb === "remember") {
      if (!arg) return { handled: true, kind: "memory", text: "要记的内容是空的。", tone: "warn" };
      const view = await memoryAdd({ content: arg });
      // 决策层嵌入（sensitivityScan）：外发档自动分级，只降不升；判不出就维持默认
      const tagged = await autoTagMemorySensitivity(view);
      return {
        handled: true,
        kind: "memory",
        text:
          `已记住（\`${view.record.id}\`，${scopeLabel[view.record.scope]}）：${arg}` +
          (tagged ? `\n外发档自动降为「${tagged}」——密钥或证件号进了记忆，就别让它再出门。` : ""),
        tone: "ok",
      };
    }
    if (verb === "forget") {
      if (!arg) return { handled: true, kind: "memory", text: "要忘记什么，总得给个说法。", tone: "warn" };
      const result = await memoryForget(arg);
      return { handled: true, kind: "memory", text: `已忘记。${result}`, tone: "ok" };
    }
    if (verb === "memory") {
      const [sub, ...tail] = arg.split(/\s+/);
      const query = tail.join(" ").trim();
      switch (sub) {
        case "list":
          return { handled: true, kind: "memory", text: formatViews(await memoryList()), tone: "ok" };
        case "search":
          if (!query) return { handled: true, kind: "memory", text: "要给个关键词。", tone: "warn" };
          return { handled: true, kind: "memory", text: formatHits(await memorySearch(query)), tone: "ok" };
        case "rebuild":
          return {
            handled: true,
            kind: "memory",
            text: `索引已按 Markdown 重建，${await memoryRebuild()} 条记录重新入库。`,
            tone: "ok",
          };
        case "why": {
          const shot = await memoryWhy(conversationId);
          return {
            handled: true,
            kind: "memory",
            text: shot ? formatWhy(shot) : "这个话题还没注入过记忆。",
            tone: "ok",
          };
        }
        case "off":
          await memoryConfigSet({ enabled: false });
          return {
            handled: true,
            kind: "memory",
            text: "记忆已关闭：不再检索、不再注入、不再自动提取。已存的记录一条都没删，`/memory on` 就恢复。",
            tone: "ok",
          };
        case "on":
          await memoryConfigSet({ enabled: true });
          return { handled: true, kind: "memory", text: "记忆已打开。", tone: "ok" };
        case "distill": {
          const summary = await memoryDistill();
          return {
            handled: true,
            kind: "memory",
            text: `蒸馏完成：新增长期记忆 ${summary.distilled} 条，归档过期记录 ${summary.archivedRecords} 条，收走日志 ${summary.archivedLogs} 份。${summary.overbudget ? "（长期记忆还超着限额，可以再跑一次。）" : ""}`,
            tone: "ok",
          };
        }
        case "edit": {
          const [target] = query.split(/\s+/);
          const next = query.slice(target.length).trim();
          if (!target || !next)
            return {
              handled: true,
              kind: "memory",
              text: "用法：`/memory edit <id> <新的正文>`",
              tone: "warn",
            };
          const view = await memoryEdit(target, { content: next });
          return {
            handled: true,
            kind: "memory",
            text: `已改写成：${view.record.content}`,
            tone: "ok",
          };
        }
        case "export": {
          const text = await memoryExport();
          // 导出的 JSON 不往聊天里贴：那是一大段机器读的东西，贴出来只会淹没对话
          try {
            await navigator.clipboard.writeText(text);
            return {
              handled: true,
              kind: "memory",
              text: `已导出到剪贴板：${text.length} 个字符。存成文件后在另一台机器用 /memory import <路径> 导入。`,
              tone: "ok",
            };
          } catch {
            return {
              handled: true,
              kind: "memory",
              text: `这台机器不让客户端读写字贴板（${text.length} 个字符已备好）。改到 设置 → 记忆 里导出成文件。`,
              tone: "warn",
            };
          }
        }
        case "import": {
          if (!query)
            return {
              handled: true,
              kind: "memory",
              text: "用法：`/memory import <导出文件的路径>`",
              tone: "warn",
            };
          return {
            handled: true,
            kind: "memory",
            text: `导入了 ${await memoryImportFile(query)} 条记忆。已存在的同 id 记录没有被覆盖。`,
            tone: "ok",
          };
        }
        default:
          return {
            handled: true,
            kind: "memory",
            text: "用法：`/memory list` · `search 关键词` · `why` · `edit <id> <正文>` · `rebuild` · `distill` · `export` · `import <路径>` · `off` / `on`",
            tone: "warn",
          };
      }
    }
  } catch (error) {
    return { handled: true, kind: "memory", text: String(error), tone: "warn" };
  }

  return { handled: false };
}
