/**
 * 常量钉测试（design-decision-layer-optimization.md §7）：
 * 每个值用**字面量**断言而不是引用常量本身——把测试写成
 * `expect(X).toBe(X)` 是自欺，这里钉的是「值是多少」这一事实。
 * 改 constants.ts 任何一行，这里必红；想改值，先改文档再改两处。
 */
import { describe, expect, it } from "vitest";
import * as C from "../constants";

describe("funnel v2 constants", () => {
  it("upgrade thresholds are pinned", () => {
    expect(C.ESCALATE_MIN_CONFIDENCE).toBe(0.85);
    expect(C.PER_QUESTION_UPGRADE_BATCH).toBe(8);
  });
});

describe("compaction constants", () => {
  it("dual keep thresholds lean conservative (D4)", () => {
    expect(C.KEEP_RESULT_THRESHOLD).toBe(0.6);
    expect(C.KEEP_CALL_THRESHOLD).toBe(0.4);
    // 不变量的方向钉：结果门槛必须高于调用门槛——丢结果可逆、丢调用不可逆
    expect(C.KEEP_RESULT_THRESHOLD).toBeGreaterThan(C.KEEP_CALL_THRESHOLD);
  });
  it("batch failure semantics (D1)", () => {
    expect(C.COMPACT_BATCH_RETRIES).toBe(1);
  });
  it("goal injection windows", () => {
    expect(C.GOAL_RECENT_USER_MESSAGES).toBe(3);
    expect(C.GOAL_MESSAGE_MAX_CHARS).toBe(500);
  });
  it("state assembly budgets and CJK calibration (D3)", () => {
    expect(C.INPUT_TRUNCATE_LEVELS).toEqual([200, 60]);
    expect(C.ABRIDGE_HEAD_CHARS).toBe(400);
    expect(C.ABRIDGE_TAIL_CHARS).toBe(150);
    expect(C.PRESERVE_RECENT_MESSAGES).toBe(6);
    expect(C.MAX_STATE_TOKENS).toBe(25_000);
    expect(C.MAX_REQUEST_TOKENS).toBe(30_000);
    // 中文优先的应用：CJK 计价必须 ≥ 1 字 1 token，且比原型英文标定（0.9）高
    expect(C.CJK_TOKENS_PER_CHAR).toBeGreaterThanOrEqual(1);
    expect(C.CJK_TOKENS_PER_CHAR).toBeGreaterThan(0.9);
    expect(C.TRUNCATE_HEAD_CHARS).toBe(300);
  });
  it("sticky / cache guard rewrite triggers", () => {
    expect(C.CACHE_GUARD_CEILING).toBe(0.8);
    expect(C.REWRITE_GROWTH).toBe(0.4);
    expect(C.MIN_REQUESTS_BETWEEN_REWRITES).toBe(15);
    expect(C.MAX_REQUESTS_BETWEEN_REWRITES).toBe(40);
  });
});

describe("staged review constants", () => {
  it("short-circuit and code-override severities are pinned", () => {
    expect(C.REVIEW_SHORT_CIRCUIT).toBe(0.9);
    expect(C.REVIEW_ESCALATE_SEVERITY).toBe(4);
    expect(C.REVIEW_ANNOTATE_SEVERITY).toBe(2);
    expect(C.REVIEW_FIRST_CHUNK_TOKENS).toBe(400);
  });
  it("stage timeout", () => {
    expect(C.DECISION_TIMEOUT_MS).toBe(5_000);
  });
});

describe("retrieval constants", () => {
  it("candidate query window", () => {
    expect(C.RETRIEVAL_MIN_CANDIDATES).toBe(2);
    expect(C.RETRIEVAL_MAX_CANDIDATES).toBe(4);
  });
  it("needs-search conservative default", () => {
    expect(C.RETRIEVAL_NEEDS_SEARCH_THRESHOLD).toBe(0.5);
  });
  it("timeouts", () => {
    expect(C.RETRIEVAL_ENGINE_TIMEOUT_MS).toBe(15_000);
    expect(C.RETRIEVAL_OVERALL_TIMEOUT_MS).toBe(30_000);
  });
  it("graded ttl table", () => {
    expect(C.RETRIEVAL_TTL_MS.news).toBe(600_000);
    expect(C.RETRIEVAL_TTL_MS.general).toBe(3_600_000);
    expect(C.RETRIEVAL_TTL_MS.reference).toBe(86_400_000);
    expect(C.RETRIEVAL_DEFAULT_TTL_MS).toBe(3_600_000);
  });
});

describe("provider chain constants", () => {
  it("fallback statuses are exactly 402 and 429 (B7)", () => {
    expect(C.PROVIDER_FALLBACK_STATUSES).toEqual([402, 429]);
  });
});

describe("breaker constants (deep-optimization increment)", () => {
  it("cooldown window is pinned", () => {
    expect(C.BREAKER_FAILURE_THRESHOLD).toBe(3);
    expect(C.BREAKER_COOLDOWN_MS).toBe(30_000);
    expect(C.BREAKER_HALF_OPEN_PROBES).toBe(1);
  });
});
