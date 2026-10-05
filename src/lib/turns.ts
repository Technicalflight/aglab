import type { Message } from "@/types/chat";

/** 一轮 = 一次提问 + 它之后接出来的所有回答。压缩摘要单独算一个锚点 */
export interface Turn {
  id: string;
  kind: "turn" | "compaction";
  question: string;
  answer: string;
  at: number;
  tools: number;
  callIds: string[];
  /** 属于这一轮的消息 id。列表按轮分组渲染，锚点和高亮都挂在这个容器上 */
  messageIds: string[];
  failed: boolean;
}

export function groupTurns(messages: Message[]): Turn[] {
  const turns: Turn[] = [];

  for (const message of messages) {
    const opens = message.role === "user" || message.summary || turns.length === 0;
    if (opens) {
      turns.push({
        id: message.id,
        kind: message.summary ? "compaction" : "turn",
        question: message.role === "user" ? message.content : "",
        answer: message.summary ? message.content : "",
        at: message.createdAt,
        tools: message.toolCalls?.length ?? 0,
        callIds: (message.toolCalls ?? []).map((call) => call.id),
        messageIds: [message.id],
        failed: Boolean(message.error),
      });
      continue;
    }

    const current = turns[turns.length - 1];
    current.messageIds.push(message.id);
    if (message.content.trim()) current.answer = message.content;
    current.tools += message.toolCalls?.length ?? 0;
    current.callIds.push(...(message.toolCalls ?? []).map((call) => call.id));
    current.failed ||= Boolean(message.error);
  }

  return turns;
}

/** 卡片只要"看得出这段在讲什么"，所以剥掉 markdown 记号当纯文本用 */
export function excerpt(markdown: string, limit: number) {
  const plain = markdown
    .replace(/```[\s\S]*?```/g, " ")
    .replace(/`([^`]*)`/g, "$1")
    .replace(/!\[[^\]]*\]\([^)]*\)/g, " ")
    .replace(/\[([^\]]*)\]\([^)]*\)/g, "$1")
    .replace(/^\s{0,3}#{1,6}\s+/gm, "")
    .replace(/^\s{0,3}>+\s?/gm, "")
    .replace(/^\s*[-*+]\s+/gm, "")
    .replace(/[*_~]{1,3}/g, "")
    .replace(/\s+/g, " ")
    .trim();

  return plain.length > limit ? `${plain.slice(0, limit)}…` : plain;
}
