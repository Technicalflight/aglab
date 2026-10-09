/**
 * 「这一轮正在做什么」那条流程的纯逻辑：标题、耗时、展开与否、每一步那一行写什么。
 *
 * 住在这里而不是组件文件里有两个理由：① 组件文件只导出组件，Vite 才认它是
 * Fast Refresh 兼容的（混导出会让改一行样式变成整页重载）；② 这几条判据是能被
 * 真跑的，别让它们藏在一段只有浏览器里才执行的 JSX 里。
 */
import type { Message, RunStep, ToolCall } from "@/types/chat";

/** 输入里挑一个最能说明"这一下动的是谁"的目标。顺序就是优先级：
 *  路径与命令排前面，因为它们才是用户要看的那一个词 */
const TARGET_KEYS = [
  "path",
  "file_path",
  "filePath",
  "command",
  "pattern",
  "query",
  "url",
  "server",
  "skill",
] as const;

const MAX_SUMMARY = 64;

function firstLine(text: string) {
  return (
    text
      .split(/\r?\n/)
      .find((line) => line.trim().length > 0)
      ?.trim() ?? ""
  );
}

/** 路径只留最后一段：`C:\...\src-tauri\src\chat.rs` 的前面那一长串在行里没有信息量 */
function tailOf(path: string) {
  const parts = path.split(/[/\\]/).filter((part) => part.length > 0);
  return parts.length > 0 ? (parts.at(-1) as string) : path;
}

function clip(text: string) {
  return text.length > MAX_SUMMARY ? `${text.slice(0, MAX_SUMMARY - 1)}…` : text;
}

/** 一次工具调用在那一行上显示什么。读的是模型给的原始输入，不另存一份真相 */
export function stepSummary(call: ToolCall): string {
  const raw = call.input.trim();
  if (!raw) return "";
  if (raw.startsWith("{") || raw.startsWith("[")) {
    try {
      const parsed = JSON.parse(raw) as unknown;
      if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
        const record = parsed as Record<string, unknown>;
        for (const key of TARGET_KEYS) {
          const value = record[key];
          if (typeof value === "string" && value.trim()) {
            const text = value.trim();
            return clip(key.includes("path") || key === "path" ? tailOf(text) : firstLine(text));
          }
        }
      }
    } catch {
      // 不是合法 JSON：退到按行读，别为了一个括号把这一行清空
    }
  }
  return clip(firstLine(raw));
}

/** 一段思考在那一行上显示什么：从它自己的起点往后读。正文只住在 `message.reasoning`
 *  那一份里，这里不抄第二份。
 *
 *  正在想的那一段报**最新**那句：长思考里第一行从头到尾不变，那一行看着就是卡住了，
 *  而"屏幕在动"正是这一格要回答的事。已经想完的那段回到第一行——回看时开头才有用 */
export function thinkingPreview(message: Message, step: RunStep, live: boolean): string {
  if (step.kind !== "thinking") return "";
  const lines = (message.reasoning ?? "")
    .slice(step.from)
    .split(/\r?\n/)
    .filter((line) => line.trim().length > 0);
  const picked = live ? lines.at(-1) : lines[0];
  return clip((picked ?? "").trim());
}

/** 秒表读数。92 秒报成「1 分 32 秒」比报成「92 秒」好读，但 8 秒不该被写成 0 分 8 秒 */
export function formatElapsed(ms: number) {
  const seconds = Math.max(0, Math.round(ms / 1000));
  if (seconds < 60) return `${seconds} 秒`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60)
    return seconds % 60 === 0 ? `${minutes} 分` : `${minutes} 分 ${seconds % 60} 秒`;
  return `${Math.floor(minutes / 60)} 时 ${minutes % 60} 分`;
}

/** 标题那一串。跑着的时候报"正在执行中 + 秒表"，跑完报"执行了 N 步 + 总耗时" */
export function flowTitle(message: Message, elapsedMs: number): string {
  const steps = message.steps?.length ?? 0;
  if (message.streaming) return `正在执行中 · ${formatElapsed(elapsedMs)}`;
  const spent = message.durationMs ?? elapsedMs;
  return steps === 1
    ? `执行了 1 步 · ${formatElapsed(spent)}`
    : `执行了 ${steps} 步 · ${formatElapsed(spent)}`;
}

/** 有没有哪一发还停在待批上。这一条压过用户手动收起：把待批卡收进折叠区，
 *  等于把"要不要让它做这件事"这个决定从用户手里拿走 */
export function hasAwaitingApproval(message: Message): boolean {
  return (message.toolCalls ?? []).some((call) => call.status === "pending");
}

/** 展开与否：待批永远展开 > 用户手动决定 > 跑着的时候展开、跑完收起 */
export function isFlowOpen(message: Message, manual: boolean | null): boolean {
  if (hasAwaitingApproval(message)) return true;
  return manual ?? Boolean(message.streaming);
}
