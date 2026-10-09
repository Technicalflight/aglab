import { useMemo } from "react";
import {
  IconCircleCheck as CircleCheck,
  IconCircleX as CircleX,
  IconLoader2 as Loader2,
} from "@tabler/icons-react";

import type { Message } from "@/types/chat";

/**
 * dynamic-workflow 的读数件：从既有消息数据派生，不发请求、不建新管道。
 *
 * * **AgentPills**：消息头行的小药丸——这一轮派出过哪些子助理、跑成没有。
 *   数据就是消息自带的 steps + toolCalls（spawn_subagent 的调用卡），
 *   状态跟着工具卡走（running/done/failed/denied）。
 * * **CompileFeedbackLine**：贴在输入框上方的编译/测试反馈行——最近一条
 *   构建/测试类 run_command 的结果一行带过（过了绿、挂了红），不用翻消息。
 */

interface PillSpec {
  key: string;
  label: string;
  status: "running" | "done" | "failed" | "denied";
}

const PILL_TONE: Record<PillSpec["status"], { ring: string; icon: typeof CircleCheck }> = {
  running: { ring: "border-sky-400/60 bg-sky-400/10 text-sky-300", icon: Loader2 },
  done: { ring: "border-emerald-500/50 bg-emerald-500/10 text-emerald-400", icon: CircleCheck },
  failed: { ring: "border-red-500/60 bg-red-500/10 text-red-400", icon: CircleX },
  denied: { ring: "border-border bg-surface text-muted-foreground", icon: CircleX },
};

/** 这一轮消息里派过的子助理药丸。没有派过就不渲染 */
export function AgentPills({ message }: { message: Message }) {
  const pills = useMemo<PillSpec[]>(() => {
    const calls = message.toolCalls ?? [];
    const steps = message.steps ?? [];
    const stepIds = new Set(
      steps.filter((step) => step.kind === "tool").map((step) => step.callId),
    );
    return calls
      .filter((call) => call.name === "spawn_subagent" && stepIds.has(call.id))
      .map((call) => {
        let label = "子助理";
        try {
          const args = JSON.parse(call.arguments ?? "{}") as { name?: string };
          if (typeof args.name === "string" && args.name.trim()) label = args.name.trim();
        } catch {
          // 参数解析不了就给兜底名，卡片本体点开能看到全部
        }
        const status: PillSpec["status"] =
          call.status === "running" || call.status === "pending"
            ? "running"
            : call.status === "failed"
              ? "failed"
              : call.status === "denied"
                ? "denied"
                : "done";
        return { key: call.id, label, status };
      });
  }, [message.toolCalls, message.steps]);

  if (pills.length === 0) return null;
  return (
    <span className="flex items-center gap-1">
      {pills.map((pill) => {
        const tone = PILL_TONE[pill.status];
        const Icon = tone.icon;
        return (
          <span
            key={pill.key}
            title={`子助理「${pill.label}」${
              pill.status === "running"
                ? "正在运行"
                : pill.status === "failed"
                  ? "运行失败"
                  : pill.status === "denied"
                    ? "被拦下"
                    : "已完成"
            }`}
            className={`flex items-center gap-1 rounded-full border px-1.5 py-px text-2xs leading-4 ${tone.ring}`}
          >
            <Icon className={`size-2.5 ${pill.status === "running" ? "animate-spin" : ""}`} />
            {pill.label}
          </span>
        );
      })}
    </span>
  );
}

/** 构建与测试的识别口径：run_command 的命令行里带这些字样才算"编译反馈" */
const BUILD_PATTERNS = [
  /cargo\s+(build|test|check)/,
  /npm\s+run\s+(build|test|lint)/,
  /npx\s+(vitest|tsc)/,
  /pytest/,
  /go\s+(build|test)/,
];

interface Feedback {
  label: string;
  ok: boolean;
}

/** 从最近的消息里找最后一条构建/测试类 run_command 的结果 */
function latestFeedback(messages: Message[]): Feedback | null {
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    const message = messages[index];
    if (message.role !== "assistant" || message.note) continue;
    const calls = message.toolCalls ?? [];
    for (let callIndex = calls.length - 1; callIndex >= 0; callIndex -= 1) {
      const call = calls[callIndex];
      if (call.name !== "run_command") continue;
      const command = (call.arguments ?? "").toLowerCase();
      if (!BUILD_PATTERNS.some((pattern) => pattern.test(command))) continue;
      const output = call.output ?? "";
      const failed =
        call.status === "failed" || /error\[E\d{4}\]|FAILED|✗|\d+ (errors|失败)/i.test(output);
      // 命令行太长截断展示：反馈行只放得下"跑的什么"的一个影子
      let label = "构建/测试";
      try {
        const args = JSON.parse(call.arguments ?? "{}") as { command?: string };
        if (typeof args.command === "string" && args.command.trim()) {
          label = args.command.trim().split(/\s+/).slice(0, 4).join(" ");
        }
      } catch {
        // 同上：解析不了给兜底
      }
      return { label, ok: !failed && call.status === "done" };
    }
  }
  return null;
}

/** 输入框上方的编译反馈行：最近一条构建/测试的结果一行带过 */
export function CompileFeedbackLine({ messages }: { messages: Message[] }) {
  const feedback = useMemo(() => latestFeedback(messages), [messages]);
  if (!feedback) return null;
  return (
    <div
      className={`mx-6 mb-1 flex items-center gap-1.5 text-2xs leading-4 ${
        feedback.ok ? "text-emerald-500" : "text-red-400"
      }`}
      title="最近一条构建/测试命令的结果"
    >
      {feedback.ok ? (
        <CircleCheck className="size-3 shrink-0" />
      ) : (
        <CircleX className="size-3 shrink-0" />
      )}
      <span className="truncate font-mono">{feedback.label}</span>
      <span>{feedback.ok ? "通过" : "未通过"}</span>
    </div>
  );
}
