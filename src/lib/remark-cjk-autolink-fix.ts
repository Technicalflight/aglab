/**
 * GFM 自动链接的 CJK 截断修复。
 *
 * micromark-extension-gfm-autolink-literal（≤2.1.0，现最新）认字符只认到
 * "非空白"为止：`4399（www.4399.com）是国内最大的…，2004 年上线` 里 `）`、
 * 汉字、全角逗号全被当成 URL 的一部分，一路吞到下一个空格——链接高亮横跨
 * 半句话（真机踩过：4399 / 7k7k 两条）。上游没有修复版可升，这里在 mdast
 * 上做后处理：自动链接文本里出现第一个非 ASCII 字符即视为 URL 的真实终点，
 * 切断后其余部分还原成普通文本。
 *
 * 只动"文本就是 URL"的自动链接（含 www 形式 url 带 http:// 前缀、mailto），
 * 显式写的 `[中文](链接)` 一个字都不碰——判据是 `url.endsWith(文本)`，
 * 而显式链接的文本与 href 各说各的，天然不进这条分支。
 */
import type { Link, Parent, Text } from "mdast";

/** 切完之后 URL 尾上不允许挂的 ASCII 标点（挂了就还回后续文本） */
const TRAILING_ASCII_PUNCT = /[.,;:!?]+$/;

/** 自动链接的判据：href 以链接文本收尾（纯文本 URL，前缀是 http(s):// 或 mailto:） */
function isAutoliteral(link: Link, text: string): boolean {
  return link.url.endsWith(text);
}

/** 第一个非 ASCII（> DEL）字符的位置；没有返回 -1 */
function firstNonAscii(value: string): number {
  for (let i = 0; i < value.length; i += 1) {
    if (value.charCodeAt(i) > 0x7e) return i;
  }
  return -1;
}

/** 一条 link：要切就切，返回替代它的节点数组（0 或 1 处切口，自动链接只会有一段纯文本） */
function splitLink(link: Link): Array<Link | Text> {
  const children = link.children ?? [];
  const child = children[0];
  // 自动链接的孩子是一段纯文本；带强调/代码等结构的显式链接原样放行
  if (children.length !== 1 || !child || child.type !== "text") return [link];

  const value = child.value;
  const cut = firstNonAscii(value);
  if (cut === -1 || !isAutoliteral(link, value)) return [link];

  let linkText = value.slice(0, cut);
  let rest = value.slice(cut);
  const trailing = TRAILING_ASCII_PUNCT.exec(linkText);
  if (trailing) {
    rest = trailing[0] + rest;
    linkText = linkText.slice(0, -trailing[0].length);
  }
  // 裁完得还像个地址（有 host 点或邮箱 @），否则宁可不切，别把显式链接切碎
  if (!linkText || !/[.@]/.test(linkText)) return [link];

  const prefix = link.url.slice(0, link.url.length - value.length);
  link.url = prefix + linkText;
  child.value = linkText;
  return [link, { type: "text", value: rest } as Text];
}

/** 深度遍历：只替换父级的直接 link 孩子（mdast 里链接不可嵌套，无需深入 link 内部） */
export function fixCjkAutolinks<T extends Parent>(tree: T): T {
  const walk = (node: Parent): void => {
    if (!node.children) return;
    const next: Array<Pick<Link, "type" | "url" | "children"> | Text | typeof node.children[number]> = [];
    for (const child of node.children) {
      if (child.type === "link") {
        next.push(...(splitLink(child as Link) as Array<typeof child>));
      } else {
        walk(child as Parent);
        next.push(child);
      }
    }
    node.children = next as typeof node.children;
  };
  walk(tree as unknown as Parent);
  return tree;
}

/** remark 插件壳：排在 remarkGfm 之后，切它产出的自动链接 */
export default function remarkCjkAutolinkFix() {
  return (tree: Parent) => {
    fixCjkAutolinks(tree);
  };
}
