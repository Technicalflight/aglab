/** slash 命令与 @-提及的纯函数层：解析草稿、展开模板。
 *  放在组件外是因为这些规则要被测试钉住——composer 是 UI，不是规则的住处 */

/** 草稿开头的 slash 命令：名字只认字母数字、连字符与下划线，
 *  后面跟的都算参数。不匹配（比如 /usr/bin 这种路径）返回 null，按普通文本发送 */
export interface SlashDraft {
  name: string;
  args: string;
}

export function parseSlashDraft(draft: string): SlashDraft | null {
  if (!draft.startsWith("/")) return null;
  const matched = draft.match(/^\/([A-Za-z0-9_-]*)(?:[ \t]+(.*))?$/s);
  if (!matched) return null;
  return { name: matched[1] ?? "", args: (matched[2] ?? "").trim() };
}

/** 菜单该不该开着：敲的是命令名（还没有空格进参数区）。
 *  一旦空格了就是在写参数，菜单让位 */
export function slashMenuOpen(draft: string): boolean {
  return /^\/[A-Za-z0-9_-]*$/.test(draft);
}

/** $ARGUMENTS 与 $1..$9 的展开。没给参数时占位符清成空串，
 *  再把行尾留下的空格收掉——"要求：$ARGUMENTS"不能把字面占位符问进日志 */
export function expandSlashTemplate(template: string, args: string): string {
  const trimmed = args.trim();
  let out = template;
  if (trimmed) {
    out = out.replace(/\$ARGUMENTS/g, trimmed);
    const positional = trimmed.split(/\s+/);
    for (let index = 0; index < Math.min(9, positional.length); index += 1) {
      out = out.replaceAll(`$${index + 1}`, positional[index]);
    }
  } else {
    out = out.replace(/\$ARGUMENTS/g, "");
    out = out.replace(/\$[1-9]/g, "");
  }
  return out.replace(/[ \t]+$/gm, "").trim();
}

/** 光标前的 @ 记号：从光标往回扫到最近的 @，且 @ 前面必须是行首或空白
 *  （user@mail 这种邮箱不触发）。query 里出现空白即不构成提及 */
export interface MentionDraft {
  /** @ 在草稿里的下标 */
  start: number;
  query: string;
}

export function parseMention(draft: string, caret: number): MentionDraft | null {
  if (caret <= 0 || caret > draft.length) return null;
  let index = caret - 1;
  while (index >= 0) {
    const char = draft[index];
    if (char === "@") {
      if (index > 0 && !/\s/.test(draft[index - 1])) return null; // 邮箱形状，不碰
      return { start: index, query: draft.slice(index + 1, caret) };
    }
    if (/\s/.test(char)) return null;
    index -= 1;
  }
  return null;
}
