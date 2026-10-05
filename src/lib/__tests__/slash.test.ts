import { describe, expect, it } from "vitest";

import { expandSlashTemplate, parseMention, parseSlashDraft, slashMenuOpen } from "../slash";

describe("parseSlashDraft", () => {
  it("parses name and args from a draft", () => {
    expect(parseSlashDraft("/init")).toEqual({ name: "init", args: "" });
    expect(parseSlashDraft("/fix-issue 1042 优先")).toEqual({ name: "fix-issue", args: "1042 优先" });
    expect(parseSlashDraft("/deploy_staging now")).toEqual({ name: "deploy_staging", args: "now" });
  });

  it("keeps unknown shapes as plain text", () => {
    // 路径不是命令：不许把 /usr/bin 吞成命令
    expect(parseSlashDraft("/usr/bin/python3")).toBeNull();
    expect(parseSlashDraft("普通消息")).toBeNull();
    expect(parseSlashDraft("/")).toEqual({ name: "", args: "" }); // 光一个斜杠是正在敲命令名
  });

  it("args keep internal spacing but lose edge whitespace", () => {
    expect(parseSlashDraft("/cmd   a  b ")).toEqual({ name: "cmd", args: "a  b" });
  });
});

describe("slashMenuOpen", () => {
  it("is open while typing the bare command name and closed once args begin", () => {
    expect(slashMenuOpen("/")).toBe(true);
    expect(slashMenuOpen("/ini")).toBe(true);
    expect(slashMenuOpen("/init ")).toBe(false);
    expect(slashMenuOpen("/init 补充")).toBe(false);
    expect(slashMenuOpen("hello /cmd")).toBe(false);
  });
});

describe("expandSlashTemplate", () => {
  it("substitutes $ARGUMENTS and positional words", () => {
    expect(expandSlashTemplate("修复 $ARGUMENTS，回归 $1", "登录超时")).toBe(
      "修复 登录超时，回归 登录超时",
    );
    expect(expandSlashTemplate("$2 与 $1", "甲 乙")).toBe("乙 与 甲");
  });

  it("strips placeholders when no args are given", () => {
    expect(expandSlashTemplate("修复 $ARGUMENTS", "")).toBe("修复");
    // 行中的双空格不收：剥除是字面替换，只有行尾的空格才收掉
    expect(expandSlashTemplate("第 $1 条\n第 $2 条", "")).toBe("第  条\n第  条");
    expect(expandSlashTemplate("要求：$ARGUMENTS\n", "")).toBe("要求：");
    expect(expandSlashTemplate("原样", "")).toBe("原样");
  });

  it("trims the expansion so a bare command sends clean prose", () => {
    expect(expandSlashTemplate("  请修复 $ARGUMENTS  \n", "  x  ")).toBe("请修复 x");
  });
});

describe("parseMention", () => {
  it("finds the @token ending at the caret", () => {
    expect(parseMention("看下 @src/ma", 10)).toEqual({ start: 3, query: "src/ma" });
    expect(parseMention("@", 1)).toEqual({ start: 0, query: "" });
    expect(parseMention("@src", 4)).toEqual({ start: 0, query: "src" });
  });

  it("ignores email shapes and mid-word @", () => {
    expect(parseMention("联系 user@mail", 14)).toBeNull();
    expect(parseMention("a@b", 3)).toBeNull();
  });

  it("query ends at whitespace", () => {
    expect(parseMention("@src 已看完", 4)).toEqual({ start: 0, query: "src" });
    expect(parseMention("@src 已看完", 8)).toBeNull();
  });

  it("rejects carets outside the draft", () => {
    expect(parseMention("abc", 0)).toBeNull();
    expect(parseMention("abc", 9)).toBeNull();
  });
});
