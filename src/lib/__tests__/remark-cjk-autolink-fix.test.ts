/**
 * GFM 自动链接的 CJK 截断修复（remark-cjk-autolink-fix）。
 *
 * micromark 的字面自动链接吞全角字符：`4399（www.4399.com）是国内…，2004 年上线`
 * 会被连成一条 href 横跨半句话的链接（真机踩过：4399 / 7k7k 两条回答全中）。
 * 钉六件事：
 * 1. `www.` 形式吞中文——切回真实 URL，后半句还原成普通文本；
 * 2. `https://` 形式吞句号与后续中文——同上；
 * 3. 切口上的 ASCII 尾标点（`www.x.com.）`）还回文本侧；
 * 4. 显式 `[中文](链接)` 的链接文本不挨刀（判据 url.endsWith(文本) 不成立）；
 * 5. 显式 `[纯 ASCII 链接文本](…)` 全 ASCII 不切，原样；
 * 6. 邮箱自动链接（mailto）同样吃这条修复。
 */
import { describe, expect, it } from "vitest";
import { fromMarkdown } from "mdast-util-from-markdown";
import { gfmFromMarkdown } from "mdast-util-gfm";
import { gfm } from "micromark-extension-gfm";
import type { Link, Text } from "mdast";

import { fixCjkAutolinks } from "@/lib/remark-cjk-autolink-fix";

/** 解析 + 修复后，摘出段落里的顺序片段（link 摘 href 与文本） */
function pieces(
  source: string,
): Array<{ kind: "link"; href: string; text: string } | { kind: "text"; text: string }> {
  const tree = fixCjkAutolinks(
    fromMarkdown(source, { extensions: [gfm()], mdastExtensions: [gfmFromMarkdown()] }),
  );
  const paragraph = tree.children[0];
  if (paragraph.type !== "paragraph") throw new Error(`expected paragraph, got ${paragraph.type}`);
  return (paragraph.children ?? []).map((node) => {
    if (node.type === "link") {
      const link = node as Link;
      return { kind: "link" as const, href: link.url, text: (link.children?.[0] as Text).value };
    }
    return { kind: "text" as const, text: (node as Text).value };
  });
}

describe("GFM 自动链接的 CJK 截断", () => {
  it("4399（www.4399.com）是国内最大的……：链接只留域名，中文全部还原成文本", () => {
    const got = pieces(
      "4399（www.4399.com）是国内最大的小游戏门户网站之一，主打「免费在线玩小游戏」，2004 年上线",
    );
    expect(got[0]).toEqual({ kind: "text", text: "4399（" });
    expect(got[1]).toEqual({ kind: "link", href: "http://www.4399.com", text: "www.4399.com" });
    // 被吞的中文在链接处切断；空格之后 micromark 本来就是另一段文本节点
    expect(got[2]).toEqual({
      kind: "text",
      text: "）是国内最大的小游戏门户网站之一，主打「免费在线玩小游戏」，2004",
    });
    expect(got[3]).toEqual({ kind: "text", text: " 年上线" });
  });

  it("7k7k（www.7k7k.com）和 4399：同一条缝的另一种切法", () => {
    const got = pieces("7k7k（www.7k7k.com）和 4399 是同类网站——国内老牌的小游戏门户网站");
    expect(got[1]).toEqual({ kind: "link", href: "http://www.7k7k.com", text: "www.7k7k.com" });
    expect(got[2]).toEqual({ kind: "text", text: "）和" });
    expect(got[3]).toEqual({ kind: "text", text: " 4399 是同类网站——国内老牌的小游戏门户网站" });
  });

  it("https:// 链接吞句号：切在句号前，href 不带尾巴", () => {
    const got = pieces("网址是 https://example.com/a。然后下一句");
    expect(got[1]).toEqual({
      kind: "link",
      href: "https://example.com/a",
      text: "https://example.com/a",
    });
    expect(got[2]).toEqual({ kind: "text", text: "。然后下一句" });
  });

  it("切口上的 ASCII 尾标点还回文本侧（www.x.com.）好）", () => {
    const got = pieces("地址 www.example.com.）后面还有话");
    expect(got[1]).toEqual({
      kind: "link",
      href: "http://www.example.com",
      text: "www.example.com",
    });
    expect(got[2]).toEqual({ kind: "text", text: ".）后面还有话" });
  });

  it("显式 [中文](链接) 不挨刀", () => {
    const source = "[中文文档](https://example.com/文档)请查收";
    expect(pieces(source)).toEqual([
      { kind: "link", href: "https://example.com/文档", text: "中文文档" },
      { kind: "text", text: "请查收" },
    ]);
  });

  it("显式纯 ASCII 链接文本（文本恰好是 href 尾巴）原样保留", () => {
    const source = "[www.x.com](https://x.com/www.x.com) 正文";
    expect(pieces(source)).toEqual([
      { kind: "link", href: "https://x.com/www.x.com", text: "www.x.com" },
      { kind: "text", text: " 正文" },
    ]);
  });

  it("邮箱自动链接同样吃修复", () => {
    const got = pieces("发到 a@b.com）这个邮箱");
    expect(got[1]).toEqual({ kind: "link", href: "mailto:a@b.com", text: "a@b.com" });
    expect(got[2]).toEqual({ kind: "text", text: "）这个邮箱" });
  });
});
