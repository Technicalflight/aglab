/**
 * 分阶段输出审查的行为钉（design-decision-layer-optimization.md §5.3）：
 * - Stage A ≥ 0.9 短路 block，C/D 零调用（反向钉）
 * - 代码覆盖：severity ≥ 4 且 Jev=allow → escalate；≥ 2 且 allow → annotate
 * - 不确定 → escalate（C/D 缺答）；决策层整体不可用 → 放行 + degraded（fail-open）
 */
import { describe, expect, it, vi } from "vitest";
import {
  reviewOutput,
  shouldPreCheck,
  type ReviewQuestion,
  type StagedReviewAsker,
} from "../staged-review";
import { REVIEW_ANNOTATE_SEVERITY, REVIEW_ESCALATE_SEVERITY } from "../constants";

type Scripted = {
  noul?: Record<string, number>;
  score?: Record<string, number>;
  choice?: Record<string, string>;
};

function scriptedAsker(script: Scripted) {
  const asker = vi.fn(async (_state: string, questions: Record<string, ReviewQuestion>) => {
    const answers: Record<string, number | string> = {};
    for (const [name, question] of Object.entries(questions)) {
      if (question.type === "noul") {
        const value = script.noul?.[name];
        if (typeof value === "number") answers[name] = value;
      } else if (question.type === "score") {
        const value = script.score?.[name];
        if (typeof value === "number") answers[name] = value;
      } else {
        const value = script.choice?.[name];
        if (typeof value === "string") answers[name] = value;
      }
    }
    return { answers };
  }) as unknown as StagedReviewAsker & { mock: { calls: unknown[][] } };
  return asker;
}

describe("分阶段输出审查", () => {
  it("Stage A 短路：任一风险 ≥ 0.9 → block，C/D 零调用", async () => {
    const asker = scriptedAsker({ noul: { risk_injection_attempt: 0.95 } });
    const verdict = await reviewOutput("output text", asker);
    expect(verdict.route).toBe("block");
    expect(verdict.stage).toBe("A");
    expect(asker.mock.calls).toHaveLength(1); // C/D 没跑
    expect(verdict.reason).toContain("短路");
  });

  it("低风险走完三阶段：severity 1 + Jev allow → 放行，无覆盖", async () => {
    const asker = scriptedAsker({
      noul: { risk_harmful_content: 0.05, risk_privacy_leak: 0.05, risk_injection_attempt: 0.05, risk_unsafe_action: 0.05 },
      score: { severity: 1 },
      choice: { route: "allow" },
    });
    const verdict = await reviewOutput("plain answer", asker);
    expect(verdict.route).toBe("allow");
    expect(verdict.stage).toBe("D");
    expect(verdict.severity).toBe(1);
    expect(verdict.overridden).toBeUndefined();
    expect(asker.mock.calls).toHaveLength(3);
  });

  it("代码覆盖：severity ≥ 4 且 Jev=allow → escalate（正对照）", async () => {
    const asker = scriptedAsker({
      noul: {},
      score: { severity: REVIEW_ESCALATE_SEVERITY },
      choice: { route: "allow" },
    });
    const verdict = await reviewOutput("output", asker);
    expect(verdict.route).toBe("escalate");
    expect(verdict.overridden).toBe(true);
    expect(verdict.jevRoute).toBe("allow");
  });

  it("代码覆盖：severity ≥ 2 且 Jev=allow → annotate", async () => {
    const asker = scriptedAsker({
      noul: {},
      score: { severity: REVIEW_ANNOTATE_SEVERITY },
      choice: { route: "allow" },
    });
    const verdict = await reviewOutput("output", asker);
    expect(verdict.route).toBe("annotate");
    expect(verdict.overridden).toBe(true);
  });

  it("Jev 说不 allow 时代码覆盖不动它（retry 照采）", async () => {
    const asker = scriptedAsker({ noul: {}, score: { severity: 2 }, choice: { route: "retry" } });
    const verdict = await reviewOutput("output", asker);
    expect(verdict.route).toBe("retry");
    expect(verdict.overridden).toBeUndefined();
  });

  it("Stage C 缺答 → escalate（§3：审查的不确定绝不静默放行）", async () => {
    const asker = scriptedAsker({ noul: {} }); // severity 缺
    const verdict = await reviewOutput("output", asker);
    expect(verdict.route).toBe("escalate");
    expect(verdict.stage).toBe("C");
  });

  it("Stage D 缺答 → 按严重性代码路由（severity 3 → annotate）", async () => {
    const asker = scriptedAsker({ noul: {}, score: { severity: 3 } });
    const verdict = await reviewOutput("output", asker);
    expect(verdict.route).toBe("annotate");
    expect(verdict.stage).toBe("D");
  });

  it("决策层整体不可用（asker 抛错）→ 放行 + degraded（嵌入点 fail-open 合同）", async () => {
    const asker = vi.fn(async () => {
      throw new Error("sidecar down");
    }) as unknown as StagedReviewAsker;
    const verdict = await reviewOutput("output", asker);
    expect(verdict.route).toBe("allow");
    expect(verdict.degraded).toBe(true);
    expect(verdict.stage).toBe("skipped");
  });

  it("流式预检判据：≥400 tokens 触发", () => {
    expect(shouldPreCheck("word ".repeat(500))).toBe(true);
    expect(shouldPreCheck("短回答")).toBe(false);
  });
});
