/**
 * System 2 兜底层。Phase 1 没有接线真实的 LLM fallback（那是 Phase 3 的活——
 * 得接上 chat-transport 的推理通道才算数），所以给一个明确「不可用」的空实现。
 *
 * 语义约定：fallback 层**永远不伪造答案**。漏斗走到这里还没结果时，
 * 路由器返回前两层里最好的那个并标 degraded，或者整条抛不可用——
 * 低置信度的诚实答案好过编造的高置信度。
 */
import type { DecisionProvider, DecisionRequest, DecisionResponse } from "../types";
import { DecisionUnavailableError } from "../errors";

export class NullFallbackProvider implements DecisionProvider {
  readonly name = "fallback" as const;
  readonly isAvailable = false;

  async decide(_request: DecisionRequest): Promise<DecisionResponse> {
    throw new DecisionUnavailableError("System 2 fallback 未接线（Phase 3）");
  }
}
