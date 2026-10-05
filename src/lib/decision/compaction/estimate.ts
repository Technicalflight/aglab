/**
 * CJK 感知的 token 估算（D3 修偏）。
 *
 * 原型 fast-jev-compaction 的标定是英文语料的：词按 6 字母 1 token、符号 0.9、
 * 自称 never undercount——但真实分词器对中文是 1–1.5 token/字，按它的公式
 * 中文「一个字一个 token 都不到」，预算全部虚高，state 装配会系统性装过头。
 * 这里的修正：**CJK 连续段单独按 CJK_TOKENS_PER_CHAR（≥1.2）计**，其余沿用原标定
 * （宁高勿低的方向不变）。R1 用真请求 usage 回测校准两个系数。
 */
import { CJK_TOKENS_PER_CHAR } from "../constants";

/** CJK 统一表意文字 + 假名 + 谚文（覆盖中日韩三种用户输入） */
const CJK_RE = /[\u3000-\u9FFF\uF900-\uFAFF\u3040-\u30FF\uAC00-\uD7AF]/;

export function isCjkChar(ch: string): boolean {
  return CJK_RE.test(ch);
}

/**
 * 估算一段文本的 token 数。拆成「CJK 连续段 / 其余」两种流分别计价：
 * - CJK 段：字数 × CJK_TOKENS_PER_CHAR
 * - 其余：连续的「字母数字词」按 6 字母 1 token，数字串减半，其他符号 0.9/个
 */
export function estimateTokens(text: string): number {
  let tokens = 0;
  let word = 0;
  let digits = 0;
  let cjk = 0;
  const flush = () => {
    if (word > 0) tokens += Math.ceil(word / 6);
    if (digits > 0) tokens += Math.ceil(digits / 12);
    if (cjk > 0) tokens += Math.ceil(cjk * CJK_TOKENS_PER_CHAR);
    word = 0;
    digits = 0;
    cjk = 0;
  };
  for (const ch of text) {
    if (isCjkChar(ch)) {
      cjk += 1;
      continue;
    }
    if (/[a-zA-Z_]/.test(ch)) {
      if (digits > 0) flush();
      word += 1;
      continue;
    }
    if (/[0-9]/.test(ch)) {
      if (word > 0) flush();
      digits += 1;
      continue;
    }
    flush();
    tokens += 0.9;
  }
  flush();
  return Math.ceil(tokens);
}
