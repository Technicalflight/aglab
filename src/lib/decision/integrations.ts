/**
 * Phase 3 嵌入函数：把 System 1 判定塞进既有流程的四个口子。
 *
 * 共同的合同（写死，别逐个发明）：
 * 1. **独立开关**——每个函数读 config.integrations 里自己那一格，关着就返回 null。
 * 2. **fail-open**——决策层不可用（总开关关、sidecar 没起、超时、答案畸形）一律 null，
 *    调用方走原来的路。嵌入只省调用/添信息，绝不拦截既有功能。
 * 3. **隐私定向**——state 里带对话正文/记忆正文的判定一律 `private`/`confidential`
 *    钉本地（路由器红线），内容不因为嵌入多出一条出网的路。
 * 4. 每次判定自然进审计（路由器埋的），Phase 4 决策面板直接吃。
 */
import type { IntegrationsConfig } from "./config";
import type { DecisionResponse } from "./types";
import { getDecisionSystem } from "./index";
import type { DecisionSystem } from "./index";

export interface ModelRoutingDecision {
  /** choice 的取值：simple_qa / code_generation / complex_reasoning / creative_writing */
  taskType: string;
  needsPremium: boolean;
  /** score 期望值，量表 trivial..very_complex（0..4） */
  complexity: number;
  response: DecisionResponse;
}

export interface MemoryGateVerdict {
  /** true = 本轮不值得提取，调用方跳过 memoryExtract；false/null = 照旧 */
  skip: boolean;
  /** P(worth remembering)。判定缺这一格时是 null（视为不跳过） */
  worth: number | null;
  response: DecisionResponse;
}

export interface SensitivityVerdict {
  containsSecret: boolean;
  containsPii: boolean;
  risk: "safe" | "low" | "high";
  /** 映射到记忆的外发三档：high→secret（哪儿都不去）、low→private、safe→public（不动） */
  suggested: "public" | "private" | "secret";
  response: DecisionResponse;
}

export interface AgentAssignment {
  agent: string;
  /** score 期望值，量表 1..5 */
  priority: number;
  canParallel: boolean;
  response: DecisionResponse;
}

export interface ContextRelevanceVerdict {
  /** 与传入 candidates 顺序对齐的相关性分（量表 0..5，可能落在档位之间） */
  scores: number[];
  response: DecisionResponse;
}

export interface PoolMemberPick {
  profileId: string;
  model: string;
  response: DecisionResponse;
}

export interface DecisionIntegrations {
  routeModel(
    message: string,
    contextSummary?: string,
    conversationId?: string,
  ): Promise<ModelRoutingDecision | null>;
  gateMemoryExtraction(
    recent: ReadonlyArray<{ role: string; content: string }>,
    conversationId?: string,
  ): Promise<MemoryGateVerdict | null>;
  detectSensitivity(content: string, conversationId?: string): Promise<SensitivityVerdict | null>;
  assignAgent(
    task: { goal: string; type?: string },
    agents: ReadonlyArray<{ role: string; description: string }>,
  ): Promise<AgentAssignment | null>;
  scoreContextRelevance(
    query: string,
    candidates: ReadonlyArray<{ id: number; text: string }>,
  ): Promise<ContextRelevanceVerdict | null>;
  pickPoolMember(
    prompt: string,
    members: ReadonlyArray<{ profileId: string; model: string; label: string }>,
    conversationId?: string,
  ): Promise<PoolMemberPick | null>;
}

/** 决策一次，失败返回 null——四个嵌入函数共用的 fail-open 底座 */
async function decideOnce(
  system: DecisionSystem,
  request: Parameters<DecisionSystem["router"]["decide"]>[0],
) {
  try {
    return await system.router.decide(request);
  } catch {
    // 决策层关着 / sidecar 没起 / 超时：嵌入的前提是"多一层本地判定"，
    // 这层不在就退回原路径，调用方永远不该为嵌入层写 catch
    return null;
  }
}

function switches(system: DecisionSystem): IntegrationsConfig {
  return system.config.integrations;
}

export function createIntegrations(system: DecisionSystem): DecisionIntegrations {
  return {
    /** §7.1 模型路由（观测面）：不改模型选择，判定进审计，Phase 4 面板消费 */
    async routeModel(message, contextSummary, conversationId) {
      if (!switches(system).modelRouting) return null;
      const response = await decideOnce(system, {
        state: contextSummary ? { message, contextSummary } : message,
        questions: {
          task_type: {
            type: "choice",
            instructions: "What type of task is this user message?",
            criteria: {
              simple_qa: "Simple factual question or small talk",
              code_generation: "Writing, editing or debugging code",
              complex_reasoning: "Multi-step reasoning, planning or analysis",
              creative_writing: "Creative or long-form writing",
            },
          },
          needs_premium: {
            type: "noul",
            instructions: "Does this task require a premium/large model to answer well?",
          },
          complexity: {
            type: "score",
            instructions: "How complex is this task?",
            criteria: ["trivial", "simple", "moderate", "complex", "very_complex"],
          },
        },
        sensitivity: "public", // 消息本体本来就要发给聊天服务商，这里不多送一处
        conversationId,
      });
      if (!response) return null;
      const taskType = response.answers.task_type?.choice;
      if (typeof taskType !== "string") return null;
      return {
        taskType,
        needsPremium: (response.answers.needs_premium?.noul ?? 0) >= 0.5,
        complexity: response.answers.complexity?.score ?? 0,
        response,
      };
    },

    /** §7.2 提取门控：private 钉本地——对话正文不为"省一次提取"多出一条出网的路 */
    async gateMemoryExtraction(recent, conversationId) {
      if (!switches(system).memoryGate) return null;
      const response = await decideOnce(system, {
        state: recent.map((m) => `${m.role}: ${m.content}`).join("\n"),
        questions: {
          worth_remembering: {
            type: "noul",
            instructions:
              "Does this conversation turn contain information worth remembering long-term?",
          },
        },
        sensitivity: "private",
        conversationId,
      });
      if (!response) return null;
      const worth = response.answers.worth_remembering?.noul ?? null;
      const threshold = system.config.integrations.memoryGateThreshold;
      return { skip: worth !== null && worth < threshold, worth, response };
    },

    /** §7.4 敏感检测：confidential 钉本地——被检测的内容恰恰是最不能出去的 */
    async detectSensitivity(content, conversationId) {
      if (!switches(system).sensitivityScan) return null;
      const response = await decideOnce(system, {
        state: content,
        questions: {
          contains_secret: {
            type: "noul",
            instructions: "Does this content contain passwords, API keys, tokens, or private keys?",
          },
          contains_pii: {
            type: "noul",
            instructions:
              "Does this content contain personal identifiable information (ID numbers, bank cards, phone numbers)?",
          },
          risk_level: {
            type: "choice",
            instructions: "What is the risk level of this content?",
            criteria: {
              safe: "No sensitive information",
              low: "Minor sensitivity, can be processed locally",
              high: "Contains secrets or PII, must stay local",
            },
          },
        },
        sensitivity: "confidential",
        conversationId,
      });
      if (!response) return null;
      const risk = response.answers.risk_level?.choice;
      if (risk !== "safe" && risk !== "low" && risk !== "high") return null;
      return {
        containsSecret: (response.answers.contains_secret?.noul ?? 0) >= 0.5,
        containsPii: (response.answers.contains_pii?.noul ?? 0) >= 0.5,
        risk,
        suggested: risk === "high" ? "secret" : risk === "low" ? "private" : "public",
        response,
      };
    },

    /** §7.3 Agent 分配（已接线）：Rust 编排侧给补做节点派工时经决策桥问到这里
     *  （`bridge.ts` 的派发表 → 本函数；Rust 调用点 `orchestrator.rs` 的
     *  `assign_followup_profiles`）。开关关着回 null，那边保持默认的 worker */
    async assignAgent(task, agents) {
      if (!switches(system).taskAssignment) return null;
      if (agents.length === 0) return null;
      const response = await decideOnce(system, {
        state: {
          taskGoal: task.goal,
          taskType: task.type ?? "",
          availableRoles: agents.map((a) => a.role),
        },
        questions: {
          best_agent: {
            type: "choice",
            instructions: "Which agent role is best suited for this task?",
            criteria: Object.fromEntries(agents.map((a) => [a.role, a.description])),
          },
          priority: {
            type: "score",
            instructions: "What priority should this task have?",
            criteria: [1, 2, 3, 4, 5],
          },
          can_parallel: {
            type: "noul",
            instructions: "Can this task be executed in parallel with other independent tasks?",
          },
        },
        sensitivity: "public",
      });
      if (!response) return null;
      const agent = response.answers.best_agent?.choice;
      if (typeof agent !== "string" || !agents.some((a) => a.role === agent)) {
        // 模型给出了花名册之外的角色：按畸形答案处理，宁可 null 也不指错人
        return null;
      }
      return {
        agent,
        priority: response.answers.priority?.score ?? 3,
        canParallel: (response.answers.can_parallel?.noul ?? 0) >= 0.5,
        response,
      };
    },

    /** §7.5 上下文相关性（已接线，Rust 调用点 `inject.rs` 的 `decision_scores`）：
     * 每个候选一个问题、同一次前向批量出分——
     * 这是"零输出 token"的用武之地，逐候选一次调用就把批量的便宜丢光了。
     * 候选正文只送每段前 200 字的预览：打分用不着全文，也没必要让它们进 state */
    async scoreContextRelevance(query, candidates) {
      if (!switches(system).contextRelevance) return null;
      if (candidates.length === 0) return null;
      const response = await decideOnce(system, {
        state: {
          query,
          candidates: candidates.map((c) => ({ id: c.id, preview: c.text.slice(0, 200) })),
        },
        questions: Object.fromEntries(
          candidates.map((c) => [
            `cand_${c.id}`,
            {
              type: "score" as const,
              instructions: "How relevant is this candidate to the query?",
              criteria: [0, 1, 2, 3, 4, 5],
            },
          ]),
        ),
        sensitivity: "private", // 候选正文可能带着还没出门的内容，钉本地
      });
      if (!response) return null;
      // 分数与传入顺序对齐。任何一个候选缺答案就整批放弃：
      // 缺一半的相关性分会让排序看起来"有依据"，比没有依据更难查
      const scores = candidates.map((c) => response.answers[`cand_${c.id}`]?.score);
      if (scores.some((s) => typeof s !== "number")) return null;
      return { scores: scores as number[], response };
    },
    /** 模型池的决策层调度（池子 mode = "decision" 时由 chat-store 在发送前调用；
     *  它的独立开关就是池子那个模式，不再另设一格）。state 是用户请求本身——
     *  它与消息本体一样马上要发给池里的某个聊天服务商，决策不多送一处，
     *  所以按 routeModel 同一条理由走 public；想把判定钉死在本地，
     *  在决策层配置里关掉 Jev 即可（路由器认它自己的配置） */
    async pickPoolMember(prompt, members, conversationId) {
      if (members.length === 0) return null;
      const response = await decideOnce(system, {
        state: prompt.slice(0, 4000),
        questions: {
          best_member: {
            type: "choice",
            instructions:
              "Which model in the pool is best suited for this user request? Consider task type, required capability, and cost-efficiency.",
            criteria: Object.fromEntries(
              members.map((member, index) => [`m${index}`, member.label]),
            ),
          },
        },
        sensitivity: "public",
        conversationId,
      });
      if (!response) return null;
      const choice = response.answers.best_member?.choice;
      if (typeof choice !== "string") return null;
      const index = Number(choice.slice(1));
      if (!Number.isInteger(index) || index < 0 || index >= members.length) {
        // 花名册之外的成员：按畸形答案处理，宁可 null 也不指错模型
        return null;
      }
      return {
        profileId: members[index].profileId,
        model: members[index].model,
        response,
      };
    },
  };
}

/* ---- 生产入口：绑定进程级单例。每个函数都是 fail-open，见文件头合同第 2 条 ---- */

export const routeModel: DecisionIntegrations["routeModel"] = (
  message,
  contextSummary,
  conversationId,
) => createIntegrations(getDecisionSystem()).routeModel(message, contextSummary, conversationId);

export const gateMemoryExtraction: DecisionIntegrations["gateMemoryExtraction"] = (
  recent,
  conversationId,
) => createIntegrations(getDecisionSystem()).gateMemoryExtraction(recent, conversationId);

export const detectSensitivity: DecisionIntegrations["detectSensitivity"] = (
  content,
  conversationId,
) => createIntegrations(getDecisionSystem()).detectSensitivity(content, conversationId);

export const assignAgent: DecisionIntegrations["assignAgent"] = (task, agents) =>
  createIntegrations(getDecisionSystem()).assignAgent(task, agents);

export const pickPoolMember: DecisionIntegrations["pickPoolMember"] = (
  prompt,
  members,
  conversationId,
) => createIntegrations(getDecisionSystem()).pickPoolMember(prompt, members, conversationId);
