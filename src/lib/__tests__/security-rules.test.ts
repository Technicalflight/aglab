import { describe, expect, it } from "vitest";

import {
  commandRuleCovers,
  fileRuleCovers,
  networkRuleCovers,
  normalizeCommandPrefix,
  normalizeDomain,
  normalizeFilePattern,
  shadowMap,
} from "@/lib/security-rules";

describe("security-rules 展示层判据", () => {
  it("文件规则的遮蔽按目录前缀与边界判定", () => {
    const ssh = {
      pattern: "%USERPROFILE%\\.ssh\\",
      read: "ask",
      write: "deny",
      delete: "deny",
    } as const;
    const home = { pattern: "%USERPROFILE%\\", read: "ask", write: "ask", delete: "deny" } as const;
    const sshx = {
      pattern: "%USERPROFILE%\\.sshx\\",
      read: "ask",
      write: "ask",
      delete: "ask",
    } as const;
    expect(fileRuleCovers(home, ssh), "家目录覆盖 .ssh 子目录").toBe(true);
    expect(fileRuleCovers(ssh, home), ".ssh 不覆盖家目录").toBe(false);
    expect(
      fileRuleCovers(ssh, sshx),
      "差一个字符是另一家：.ssh 不遮 .sshx（后端同一条边界纪律的镜子）",
    ).toBe(false);
  });

  it("文件规则的归一抹掉大小写、分隔符方向与尾部反斜杠", () => {
    expect(normalizeFilePattern("C:/Users/Someone/Proj\\")).toBe(
      normalizeFilePattern("c:\\users\\someone\\proj"),
    );
    expect(normalizeFilePattern("\\\\?\\C:\\x\\y")).toBe("c:\\x\\y");
  });

  it("命令前缀的遮蔽要求前缀相接，空白折叠后比较", () => {
    const push = { prefix: "git push", action: "ask" } as const;
    const pushForce = { prefix: "git push --force", action: "allow" } as const;
    const status = { prefix: "git status", action: "allow" } as const;
    expect(commandRuleCovers(push, pushForce), "git push 遮住 git push --force").toBe(true);
    expect(commandRuleCovers(push, status), "git push 不遮 git status").toBe(false);
    expect(normalizeCommandPrefix("  GIT   push ")).toBe("git push");
  });

  it("域名的遮蔽按后缀域与相似名边界判定", () => {
    expect(networkRuleCovers({ pattern: "example.com" }, { pattern: "api.example.com" })).toBe(
      true,
    );
    expect(networkRuleCovers({ pattern: "example.com" }, { pattern: "notexample.com" })).toBe(
      false,
    );
    expect(
      networkRuleCovers({ pattern: "https://Example.com/path" }, { pattern: "EXAMPLE.com" }),
      "条目粘整条 URL 也行，归一后同域",
    ).toBe(true);
    expect(normalizeDomain("https://api.example.com:8443/x?y=1")).toBe("api.example.com");
  });

  it("遮蔽提示指向最近的遮蔽者，未被遮的条目不进结果", () => {
    const rules = [
      { pattern: "a.com" },
      { pattern: "b.com" },
      { pattern: "x.a.com" },
      { pattern: "y.x.a.com" },
    ];
    const shadowed = shadowMap(rules, networkRuleCovers);
    expect(shadowed.get(2), "第 3 条被第 1 条遮").toBe(1);
    expect(shadowed.get(3), "第 4 条被最近的第 3 条遮，而不是第 1 条").toBe(3);
    expect(shadowed.has(1), "第 2 条没人遮它").toBe(false);
    expect(shadowed.size).toBe(2);
  });
});
