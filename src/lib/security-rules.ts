/**
 * 三张安全规则表共用的前端判据（design-security-center.md D2/D4/D5）。
 *
 * 这里只做**展示层**要用的两件事：遮蔽提示（"被第 N 条遮蔽"）与归一化。
 * 判定的唯一真相在 Rust 侧（`file_rules.rs` / `command_rules.rs` / `egress.rs`）——
 * 这份镜子只回答"界面上该提醒哪几条"，不参与任何放行/拦截。
 * 两边口径一旦漂移，代价只是提示不准，不会放行任何东西——所以这里宁可少提示。
 */

import type { CommandRule, FileRule, FileRuleAction } from "@/types/chat";

/** 文件安全规则的出厂预设（design-security-center.md D2）：凭据目录，读=问、写=拒、删=拒。
 *  `%USERPROFILE%` 由后端在匹配时按环境展开——前端只存字面量 */
export const FILE_RULE_PRESETS: FileRule[] = [
  { pattern: "%USERPROFILE%\\.ssh\\", read: "ask", write: "deny", delete: "deny" },
  { pattern: "%USERPROFILE%\\.aws\\", read: "ask", write: "deny", delete: "deny" },
  { pattern: "%USERPROFILE%\\.gcp\\", read: "ask", write: "deny", delete: "deny" },
  { pattern: "%USERPROFILE%\\.gnupg\\", read: "ask", write: "deny", delete: "deny" },
  { pattern: "%USERPROFILE%\\.gpg\\", read: "ask", write: "deny", delete: "deny" },
];

/** 命令黑名单的出厂预设（D4）：动系统而不是动项目的那批程序。机器级，只有全局一份 */
export const COMMAND_BLOCKLIST_PRESETS: string[] = [
  "wsl.exe",
  "wslconfig.exe",
  "wmic.exe",
  "sc.exe",
  "reg.exe",
  "schtasks.exe",
];

export const ACTION_LABEL: Record<FileRuleAction, string> = {
  deny: "拒绝",
  ask: "询问",
  allow: "放行",
};

/** 文件规则条目的展示归一：小写、分隔符归一、剥 `\\?\`、去尾分隔符。
 *  %VAR% 不展开（浏览器拿不到环境变量）——同变量的预设之间遮蔽判断照样成立；
 *  跨变量的误报/漏报由"宁可少提示"的纪律兜着 */
export function normalizeFilePattern(pattern: string): string {
  return pattern
    .trim()
    .replace(/^\\\\\?\\/, "")
    .toLowerCase()
    .replaceAll("/", "\\")
    .replace(/\\+$/, "");
}

/** 命令前缀的展示归一：小写 + 空白折叠（与后端 normalize_prefix 同一口径） */
export function normalizeCommandPrefix(prefix: string): string {
  return prefix.trim().toLowerCase().split(/\s+/).filter(Boolean).join(" ");
}

/** 域名的展示归一：剥 scheme 与路径/端口，小写（与后端 host_of 同口径的窄版） */
export function normalizeDomain(pattern: string): string {
  let text = pattern.trim().toLowerCase();
  const schemeAt = text.indexOf("://");
  if (schemeAt >= 0) text = text.slice(schemeAt + 3);
  const cut = text.search(/[/?#]/);
  if (cut >= 0) text = text.slice(0, cut);
  return text.replace(/:\d+$/, "");
}

/** 更早的一条是否**完全遮住**更晚的一条：凡是晚条能命中的目标，早条一定先命中。
 *  首条命中即停的语义下，这样的晚条整条是装饰。
 *  边界与后端同一条纪律：两边都补上尾分隔符再比前缀，`.ssh\` 因此吞不进 `.sshx\` */
export function fileRuleCovers(earlier: FileRule, later: FileRule): boolean {
  const a = `${normalizeFilePattern(earlier.pattern)}\\`;
  const b = `${normalizeFilePattern(later.pattern)}\\`;
  if (a === "\\" || b === "\\") return false;
  return b.startsWith(a);
}

/** 命令前缀的遮蔽：晚条的前缀以早条的前缀开头（晚条能匹配的段，早条都先匹配） */
export function commandRuleCovers(earlier: CommandRule, later: CommandRule): boolean {
  const a = normalizeCommandPrefix(earlier.prefix);
  const b = normalizeCommandPrefix(later.prefix);
  if (!a || !b) return false;
  return b.startsWith(a);
}

/** 域名规则的遮蔽：晚条的域落在早条的后缀域之内（晚条能命中的主机，早条都先命中） */
export function networkRuleCovers(earlier: { pattern: string }, later: { pattern: string }): boolean {
  const a = normalizeDomain(earlier.pattern);
  const b = normalizeDomain(later.pattern);
  if (!a || !b) return false;
  return b === a || b.endsWith(`.${a}`);
}

/**
 * 遮蔽提示：对每一条规则找出**最近的一条遮住它的更早规则**（1 起的序号）。
 * 没有被遮的条目不在结果里。首条命中即停的固有后果是"下面的规则可能永远够不着"——
 * 让装饰看得见，是 `is_known_key` 那条教训的 UI 版
 */
export function shadowMap<T>(rules: T[], covers: (earlier: T, later: T) => boolean): Map<number, number> {
  const result = new Map<number, number>();
  for (let later = 1; later < rules.length; later += 1) {
    for (let earlier = later - 1; earlier >= 0; earlier -= 1) {
      if (covers(rules[earlier], rules[later])) {
        result.set(later, earlier + 1);
        break;
      }
    }
  }
  return result;
}
