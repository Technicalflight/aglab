/**
 * Provider 返回值归一化。Laya（sidecar/embedded）与 Jev 的 answers 形状是同一契约
 * （choice/score/noul 三件套），差异只在传输层——归一化只补 metadata
 * （questionName、model、latency），不改写概率本身。
 *
 * 契约的权威来源是 `@receptron/laya@0.1.2` 的 dist/types.d.ts（Jev 的 system_one 形状，
 * Laya 照抄），不是我们发明的字段名。那条历史教训：本文件曾按一个**上游根本没有的**
 * `distribution: number[]` 去算 score 的置信度，取不到就回落 0.5——于是任何含 score
 * 问题的判定永远达不到升级阈值，而测试 fixture 用的是同一个虚构字段，三个阶段全绿。
 */
import type { DecisionAnswer, ModelTier, QuestionType } from "../types";
import { DecisionTimeoutError, DecisionUnavailableError } from "../errors";

const KNOWN_TYPES: readonly QuestionType[] = ["choice", "score", "noul"];

/** 从答案自带字段反推问题类型。上游三种都带 `type`，那是第一依据；缺了才按载荷猜 */
function inferType(raw: Record<string, unknown>): QuestionType {
  if (typeof raw.type === "string" && (KNOWN_TYPES as readonly string[]).includes(raw.type)) {
    return raw.type as QuestionType;
  }
  if (typeof raw.choice === "string") return "choice";
  if (typeof raw.noul === "number") return "noul";
  if (typeof raw.score === "number") return "score";
  throw new DecisionUnavailableError("答案缺少 choice/score/noul 任何一种已知字段");
}

function clamp(value: number): number {
  return Math.min(1, Math.max(0, value));
}

/** 上游的 probabilities 是 `档位标签 → 概率` 的表（choice 的键是选项 ID，score 的是档位） */
function probabilityTable(raw: Record<string, unknown>): Record<string, number> | null {
  const table = raw.probabilities;
  if (typeof table !== "object" || table === null) return null;
  const values = Object.values(table as Record<string, unknown>);
  if (!values.every((value) => typeof value === "number" && Number.isFinite(value))) return null;
  return table as Record<string, number>;
}

/**
 * 置信度：优先用**模型自己给的那一个**。choice 的是 1 − 归一化熵，且上游按选项数
 * 分档调过温度（`laya_config.json` 的 `temperature_by_options`）——20 个选项的 0.85
 * 与 2 个选项的 0.85 本来就不同尺度，那把尺是它的出厂口径，我们不该另造一把。
 *
 * 只有模型没给的时候才自己算，且按原语的保守方向算：
 * noul 没有 confidence 字段（上游事实），取 max(p, 1-p)——布尔的确信度与极性无关；
 * choice 退回选中项的概率；score 退回分布峰值占比；连分布都没有就是 0.5，
 * 路由器会把它当「不够确信」处理，这正是我们想要的方向。
 */
function confidenceOf(type: QuestionType, raw: Record<string, unknown>): number {
  const given = raw.confidence;
  if (typeof given === "number" && Number.isFinite(given)) return clamp(given);
  if (type === "noul") {
    const p = typeof raw.noul === "number" && Number.isFinite(raw.noul) ? raw.noul : 0.5;
    return clamp(Math.max(p, 1 - p));
  }
  const table = probabilityTable(raw);
  if (!table) return 0.5;
  if (type === "choice") {
    const picked = table[String(raw.choice)];
    return typeof picked === "number" && Number.isFinite(picked) ? clamp(picked) : 0.5;
  }
  const values = Object.values(table);
  return values.length > 0 ? clamp(Math.max(...values)) : 0.5;
}

/**
 * 归一化一批 answers。任何一条答案畸形（缺字段/不是对象）都整批抛不可用：
 * 决策是批量承诺，缺一半答案的「成功」比失败更难排查。
 */
export function normalizeProviderAnswers(
  raw: unknown,
  model: ModelTier,
  startedAt: number,
): Record<string, DecisionAnswer> {
  if (typeof raw !== "object" || raw === null) {
    throw new DecisionUnavailableError(`${model} 返回的 answers 不是对象`);
  }
  const answers: Record<string, DecisionAnswer> = {};
  for (const [questionName, value] of Object.entries(raw as Record<string, unknown>)) {
    if (typeof value !== "object" || value === null) {
      throw new DecisionUnavailableError(`${model} 对问题 ${questionName} 返回了非对象答案`);
    }
    const record = value as Record<string, unknown>;
    const type = inferType(record);
    const table = probabilityTable(record) ?? undefined;
    answers[questionName] = {
      questionName,
      type,
      ...(type === "choice" ? { choice: record.choice as string } : {}),
      ...(type === "score" ? { score: record.score as number } : {}),
      ...(type === "noul" ? { noul: record.noul as number } : {}),
      ...(table ? { probabilities: table } : {}),
      confidence: confidenceOf(type, record),
      model,
      latencyMs: Math.max(0, Date.now() - startedAt),
    };
  }
  return answers;
}

/** fetch 的失败分流：超时是 TimeoutError，其余（断连、非 2xx、协议坏）都算不可用 */
export function mapFetchError(error: unknown, providerName: string): Error {
  if (error instanceof DecisionUnavailableError || error instanceof DecisionTimeoutError) {
    return error;
  }
  const name = (error as { name?: string } | null)?.name;
  if (name === "TimeoutError" || name === "AbortError") {
    return new DecisionTimeoutError(`${providerName} 请求超时`);
  }
  const message = error instanceof Error ? error.message : String(error);
  return new DecisionUnavailableError(`${providerName} 请求失败：${message}`);
}

/**
 * 从 provider 的错误原文里捞 HTTP 状态码。原生通道（decision_jev_system_one）只回
 * 字符串（"Jev HTTP 401"），而降级链的 fail-fast 判定要数字——解析在这里做一次，
 * 挂上 DecisionUnavailableError.status，链层不再对字符串做正则。
 */
export function statusFromMessage(message: string): number | undefined {
  const match = /HTTP (\d{3})/.exec(message);
  const status = match ? Number(match[1]) : NaN;
  return Number.isInteger(status) && status >= 100 && status <= 599 ? status : undefined;
}
