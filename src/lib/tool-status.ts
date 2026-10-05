/**
 * 工具调用那一行的图标与文字。流程条与工具卡共用这一份：
 * 两处各写一张表的话，加一种状态就会有一处漏掉，而漏掉的那处显示的是**上一个**状态的样子。
 *
 * 它住在 lib 里而不是组件文件里，因为组件文件混导出会让 Vite 判定 Fast Refresh 不兼容。
 */
import type { ComponentType } from "react";
import {
  IconAppWindow as AppWindow,
  IconRobot as Bot,
  IconBraces as Braces,
  IconBook as Book,
  IconEye as Eye,
  IconFileCode as FileCode,
  IconFilePlus as FilePlus,
  IconFlag as Flag,
  IconFolderOpen as FolderOpen,
  IconGlobe as Globe,
  IconHistory as History,
  IconLibrary as Library,
  IconListCheck as ListCheck,
  IconMessageCircleQuestion as MessageCircleQuestion,
  IconHandClick as MousePointerClick,
  IconPackage as Package,
  IconPencil as Pencil,
  IconPlug as Plug,
  IconSearch as Search,
  IconServer as Server,
  IconTerminal as Terminal,
  IconUsers as Users,
  IconTool as Wrench,
  IconExternalLink as ExternalLink,
  IconCircleCheck as CircleCheck,
  IconClock as Clock,
  IconAlertTriangle as TriangleAlert,
  IconFileX as FileX,
  IconX as X,
} from "@tabler/icons-react";

import type { ToolCall } from "@/types/chat";

export const STATUS_ICON = {
  pending: Clock,
  running: Terminal,
  done: CircleCheck,
  denied: X,
  failed: TriangleAlert,
} as const;

export const STATUS_TEXT: Record<ToolCall["status"], string> = {
  pending: "等待批准",
  running: "执行中",
  done: "已完成",
  denied: "已拒绝",
  failed: "失败",
};

/**
 * 工具名 → 行首的动作图标与动词（对齐参考设计：编辑/读取/终端/待办…）。
 * 图标永远是中性灰——状态由行尾那格状态文字说，一个图标不说两件事。
 */
export interface ToolLook {
  Icon: ComponentType<{ className?: string }>;
  verb: string;
}

const TOOL_LOOKS: Record<string, ToolLook> = {
  run_command: { Icon: Terminal, verb: "终端" },
  command_output: { Icon: Terminal, verb: "读输出" },
  command_stop: { Icon: Terminal, verb: "停终端" },
  read_file: { Icon: Search, verb: "读取" },
  write_file: { Icon: FilePlus, verb: "写入" },
  edit_file: { Icon: Pencil, verb: "编辑" },
  delete_file: { Icon: FileX, verb: "删除" },
  list_files: { Icon: FolderOpen, verb: "浏览" },
  search_text: { Icon: Search, verb: "搜索" },
  open_path: { Icon: ExternalLink, verb: "打开" },
  web_fetch: { Icon: Globe, verb: "网页" },
  web_search: { Icon: Globe, verb: "搜索" },
  update_plan: { Icon: ListCheck, verb: "待办" },
  ask_user: { Icon: MessageCircleQuestion, verb: "提问" },
  spawn_subagent: { Icon: Bot, verb: "子助理" },
  agent_control: { Icon: Users, verb: "子助理控制" },
  browser: { Icon: AppWindow, verb: "浏览器" },
  ssh_run: { Icon: Server, verb: "SSH" },
  lsp_query: { Icon: Braces, verb: "LSP" },
  run_program: { Icon: FileCode, verb: "脚本" },
  present_files: { Icon: Package, verb: "交付" },
  knowledge_search: { Icon: Library, verb: "资料库" },
  obs_recall: { Icon: History, verb: "观察" },
  goal_report: { Icon: Flag, verb: "目标" },
  load_skill: { Icon: Book, verb: "技能" },
  list_windows: { Icon: AppWindow, verb: "列窗口" },
  inspect_window: { Icon: Eye, verb: "读窗口" },
  computer_act: { Icon: MousePointerClick, verb: "桌面" },
};

export function toolLook(name: string): ToolLook {
  if (TOOL_LOOKS[name]) return TOOL_LOOKS[name];
  if (name.startsWith("mcp__")) {
    const parts = name.split("__");
    return { Icon: Plug, verb: parts.length >= 3 ? parts.slice(2).join("__") : name };
  }
  return { Icon: Wrench, verb: name };
}

/**
 * 兼容旧签名：行内动词。流程条与工具卡现在都用 [`toolLook`]，这一条留给
 * 别处还挂着的一两个调用点
 */
export function toolVerb(name: string): string {
  return toolLook(name).verb;
}

// ---- 参数的懒解析：arguments 是原始 JSON 文本，随用随拆 ----

function parsedArgs(call: ToolCall): Record<string, unknown> {
  if (!call.arguments) return {};
  try {
    const parsed = JSON.parse(call.arguments) as unknown;
    return parsed && typeof parsed === "object" && !Array.isArray(parsed)
      ? (parsed as Record<string, unknown>)
      : {};
  } catch {
    return {};
  }
}

function strArg(call: ToolCall, key: string): string {
  const value = parsedArgs(call)[key];
  return typeof value === "string" ? value : "";
}

function firstLine(text: string): string {
  return text.split(/\r?\n/).find((line) => line.trim().length > 0)?.trim() ?? "";
}

/** 文件类工具的目标：文件名与目录分开（名字在前台上色，目录灰一号） */
export interface FileTarget {
  name: string;
  dir: string;
  ext: string;
}

export function fileTargetOf(call: ToolCall): FileTarget | null {
  const raw = strArg(call, "path");
  if (!raw) return null;
  const norm = raw.replace(/\\/g, "/");
  const at = norm.lastIndexOf("/");
  const name = norm.slice(at + 1);
  if (!name) return null;
  const dot = name.lastIndexOf(".");
  return {
    name,
    dir: at >= 0 ? norm.slice(0, at + 1) : "",
    ext: dot >= 0 ? name.slice(dot + 1).toLowerCase() : "",
  };
}

/**
 * 行数差（编辑行上的 +N −M）。old/new 按行裁掉公共前后缀，剩下的就是这一刀的
 * 增删——对单刀编辑是精确值。write_file 没有旧文可比，只报新内容的行数
 */
export function diffStatOf(call: ToolCall): { added: number; removed: number } | null {
  if (call.name === "write_file") {
    const content = strArg(call, "content");
    if (!content) return null;
    return { added: content.split(/\r?\n/).length, removed: 0 };
  }
  if (call.name !== "edit_file") return null;
  const oldText = strArg(call, "old_string");
  const newText = strArg(call, "new_string");
  if (!oldText && !newText) return null;
  const oldLines = oldText.split(/\r?\n/);
  const newLines = newText.split(/\r?\n/);
  let head = 0;
  while (
    head < oldLines.length &&
    head < newLines.length &&
    oldLines[head] === newLines[head]
  ) {
    head += 1;
  }
  let tail = 0;
  while (
    tail < oldLines.length - head &&
    tail < newLines.length - head &&
    oldLines[oldLines.length - 1 - tail] === newLines[newLines.length - 1 - tail]
  ) {
    tail += 1;
  }
  return {
    removed: Math.max(0, oldLines.length - head - tail),
    added: Math.max(0, newLines.length - head - tail),
  };
}

/** 待办行上的进度（update_plan 是全量替换，数一遍就知道走到第几步） */
export function planProgressOf(call: ToolCall): { done: number; total: number } | null {
  if (call.name !== "update_plan") return null;
  const steps = parsedArgs(call)["steps"];
  if (!Array.isArray(steps)) return null;
  const done = steps.filter(
    (step) => (step as { status?: unknown })?.status === "completed",
  ).length;
  return { done, total: steps.length };
}

/** 行内的等宽细节。路径类工具的目标已经在专用格里，不重复念一遍 */
export function detailOf(call: ToolCall): string {
  if (fileTargetOf(call)) return "";
  if (call.name === "run_command") {
    const command = strArg(call, "command");
    return firstLine(command) || call.input;
  }
  if (call.name === "search_text") {
    const query = strArg(call, "query");
    return query ? `「${query}」` : call.input;
  }
  if (call.name === "update_plan") {
    return strArg(call, "explanation");
  }
  return call.input;
}
