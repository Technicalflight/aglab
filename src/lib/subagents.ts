/**
 * 子助理的纯规则层：名字的合法形状、保存前的校验、模型键的编解码。
 * 页面在这里问"能保存吗"；Rust 消费点对同几条再防一次——前端拦大概率，
 * 后端拦漏网，两边判的是同一张表（BUILTIN_ROLES）。
 */
import type { PoolCatalogEntry } from "@/lib/chat-transport";
import type { SubagentDef, SubagentOverride } from "@/types/chat";

/**
 * 保留名：编排的内置执行角色 + 出厂子助理名册（后端 `spawn::builtin_subagents`）。
 * 自定义的名字不许撞它们——消费点内置赢，撞名的定义会被无视（出厂名是保留名，
 * 与后端 merged_catalog 的同一张表）
 */
export const BUILTIN_ROLES = [
  "reader",
  "worker",
  "verifier",
  "supervisor",
  "planner",
  "integrator",
  // 出厂子助理（后端 spawn::builtin_subagents，2026-10 扩到九个）
  "general-purpose",
  "explore",
  "reviewer",
  "operator",
  "distill",
  "test-runner",
  "fixer",
  "ui-designer",
  "researcher",
] as const;

/**
 * 覆盖项落盘前的问题；null = 合法。名字必须命中出厂名册——后端对没命中的条目
 * 安静无视（升级挪走角色的降级路径），前端拦住能让"改了个寂寞"在保存前现形
 */
export function overrideProblem(name: string): string | null {
  if (!(BUILTIN_ROLES as readonly string[]).includes(name)) {
    return `「${name}」不在出厂名册里（可能已被升级移除），这条覆盖不会生效。`;
  }
  return null;
}

/** 覆盖项落盘前的清洗：只留命中出厂名册的条目，未知名字的安静丢掉（口径与后端一致） */
export function sanitizeOverrides(overrides: readonly SubagentOverride[]): SubagentOverride[] {
  return overrides.filter((item) => overrideProblem(item.name) === null);
}

/**
 * 名字的问题；null = 合法。名字要进编排节点的 profile 字符串与 spawn 工具的参数，
 * 空白与符号都是雷：首尾空白让账本对不上，中间空格让参数解析各说各话
 */
export function nameProblem(name: string): string | null {
  const trimmed = name.trim();
  if (!trimmed) return "名字不能为空。";
  if (trimmed !== name) return "名字首尾不能带空白。";
  if (!/^[\p{L}\p{N}_-]+$/u.test(trimmed)) {
    return "名字只能含文字、数字、下划线、连字符（不能有空格或符号）。";
  }
  if ((BUILTIN_ROLES as readonly string[]).includes(trimmed)) {
    return `「${trimmed}」是内置角色名，换一个。`;
  }
  return null;
}

/** 保存前的全部问题；空数组 = 可以存。重名按名字查，排除正在编辑的这份 */
export function validateSubagent(
  draft: SubagentDef,
  all: readonly SubagentDef[],
  profiles: readonly { id: string }[],
): string[] {
  const problems: string[] = [];
  const nameIssue = nameProblem(draft.name);
  if (nameIssue) problems.push(nameIssue);
  if (all.some((item) => item !== draft && item.name === draft.name.trim())) {
    problems.push(`已有叫「${draft.name.trim()}」的子助理。`);
  }
  if (
    draft.endpointProfileId &&
    !profiles.some((profile) => profile.id === draft.endpointProfileId)
  ) {
    problems.push("选中的服务商档案不存在（可能刚被删了），重新选一个。");
  }
  return problems;
}

/** 「继承默认」的哨兵。Radix 的 Select 不收空串 value，所以空串语义要有别的写法 */
export const INHERIT_MODEL = "__inherit__";

/** def 的两格 → 下拉的 value；两边都空 = 继承默认。分隔符用 \u{1}：模型名里不会出现 */
export function modelKeyOf(def: Pick<SubagentDef, "endpointProfileId" | "model">): string {
  if (!def.endpointProfileId && !def.model) return INHERIT_MODEL;
  return `${def.endpointProfileId}\u{1}${def.model}`;
}

/** 下拉的 value → def 的两格。旧形状（没有分隔符）按"只有模型名"读 */
export function parseModelKey(key: string): { endpointProfileId: string; model: string } {
  if (key === INHERIT_MODEL) return { endpointProfileId: "", model: "" };
  const at = key.indexOf("\u{1}");
  if (at < 0) return { endpointProfileId: "", model: key };
  return { endpointProfileId: key.slice(0, at), model: key.slice(at + 1) };
}

/**
 * 关闭的下拉**不再挂条目**：Radix 会把关闭的内容整个 portal 进游离节点照常渲染
 * （为了让 SelectValue 能读到条目文本），目录一大，每次挂载就是几百个 SelectItem。
 * 标签改由这里自己算，`<SelectValue>` 显式给——条目就可以完全懒加载到展开那一刻
 */
export function modelKeyLabel(
  picked: Pick<SubagentDef, "endpointProfileId" | "model">,
  profiles: readonly { id: string; name: string }[],
): string {
  if (!picked.endpointProfileId && !picked.model) return "继承默认";
  const name = picked.endpointProfileId
    ? (profiles.find((profile) => profile.id === picked.endpointProfileId)?.name ?? "已删除的档案")
    : "当前连接";
  return `${name} · ${picked.model}`;
}

export interface ModelOption {
  key: string;
  /** 「档案名 · 模型」；当前连接那张档案的 id 是空串，照实说"当前连接" */
  label: string;
  /** 裸模型名：下拉条目按它配品牌图标 */
  model: string;
}

/** 下拉的选项 = 目录里每个来源（含"当前连接"）× 它拉到的模型 */
export function modelOptions(catalog: readonly PoolCatalogEntry[]): ModelOption[] {
  return catalog.flatMap((entry) =>
    entry.models.map((model) => ({
      key: `${entry.profileId}\u{1}${model}`,
      label: entry.profileId ? `${entry.name} · ${model}` : `当前连接 · ${model}`,
      model,
    })),
  );
}
