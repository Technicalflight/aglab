import { describe, expect, it } from "vitest";

import {
  INHERIT_MODEL,
  modelKeyOf,
  modelOptions,
  nameProblem,
  overrideProblem,
  parseModelKey,
  sanitizeOverrides,
  validateSubagent,
} from "../subagents";
import type { SubagentDef, SubagentOverride } from "@/types/chat";

function def(overrides: Partial<SubagentDef> = {}): SubagentDef {
  return {
    name: "审查员",
    description: "对照要求复核结论",
    systemPrompt: "你是这一支的执行者。",
    tools: ["read_file"],
    endpointProfileId: "",
    model: "",
    orchestrationAssignable: true,
    chatSpawnable: false,
    ...overrides,
  };
}

describe("子助理的名字", () => {
  it("合法形状：文字数字下划线连字符，含 CJK；空白与符号都不行", () => {
    expect(nameProblem("审查员")).toBeNull();
    expect(nameProblem("fact-checker_2")).toBeNull();
    expect(nameProblem("")).toBe("名字不能为空。");
    expect(nameProblem(" 审查员")).toBe("名字首尾不能带空白。");
    expect(nameProblem("审 查员")).toContain("不能有空格");
    expect(nameProblem("审/查员")).toContain("不能有空格");
  });

  it("内置角色名被拦下：消费点内置赢，撞名的定义会被无视，所以干脆不让存", () => {
    for (const role of ["reader", "worker", "verifier", "supervisor", "planner", "integrator"]) {
      expect(nameProblem(role)).toContain("内置角色名");
    }
  });

  it("出厂子助理的名字同样是保留名：自定义不许撞（后端 merged_catalog 同一张表）", () => {
    for (const name of ["general-purpose", "explore", "reviewer", "operator", "distill"]) {
      expect(nameProblem(name)).toContain("内置角色名");
    }
  });
});

describe("内置覆盖项", () => {
  it("名字必须命中出厂名册：没命中的报出来", () => {
    expect(overrideProblem("explore")).toBeNull();
    expect(overrideProblem("已经下线的角色")).toContain("不在出厂名册里");
  });

  it("清洗：只留命中名册的条目，未知名字的安静丢掉（口径与后端一致）", () => {
    const overrides: SubagentOverride[] = [
      { name: "explore", endpointProfileId: "", model: "m1", disabled: false },
      { name: "已经下线的角色", endpointProfileId: "p", model: "", disabled: true },
    ];
    expect(sanitizeOverrides(overrides)).toEqual([
      { name: "explore", endpointProfileId: "", model: "m1", disabled: false },
    ]);
  });
});

describe("保存前的校验", () => {
  const existing = [def(), def({ name: "侦察兵" })];

  it("重名（排除自己）与不存在的服务商档案各报各的", () => {
    expect(validateSubagent(def({ name: "侦察兵" }), existing, [])).toEqual([
      "已有叫「侦察兵」的子助理。",
    ]);
    expect(validateSubagent(def({ name: "新来的" }), existing, [])).toEqual([]);
    expect(
      validateSubagent(def({ name: "新来的", endpointProfileId: "prof-gone" }), existing, [
        { id: "prof-1" },
      ]),
    ).toEqual(["选中的服务商档案不存在（可能刚被删了），重新选一个。"]);
  });

  it("编辑中的那份不算与自己重名", () => {
    const editing = existing[0];
    expect(validateSubagent(editing, existing, [])).toEqual([]);
  });
});

describe("模型键的编解码", () => {
  it("两格都空 = 继承默认；有服务商、只有模型、两者都有各回各的", () => {
    expect(modelKeyOf(def())).toBe(INHERIT_MODEL);
    expect(parseModelKey(INHERIT_MODEL)).toEqual({ endpointProfileId: "", model: "" });
    expect(parseModelKey(modelKeyOf(def({ model: "deepseek-chat" })))).toEqual({
      endpointProfileId: "",
      model: "deepseek-chat",
    });
    expect(parseModelKey(modelKeyOf(def({ endpointProfileId: "prof-1", model: "m" })))).toEqual({
      endpointProfileId: "prof-1",
      model: "m",
    });
  });

  it("下拉选项 = 目录里每个来源 × 它的模型；当前连接那张照实说", () => {
    const options = modelOptions([
      {
        profileId: "",
        name: "当前连接",
        baseUrl: "",
        apiFormat: "",
        models: ["a", "b"],
        error: null,
      },
      { profileId: "prof-1", name: "中转", baseUrl: "", apiFormat: "", models: ["c"], error: null },
    ]);
    expect(options.map((option) => option.label)).toEqual([
      "当前连接 · a",
      "当前连接 · b",
      "中转 · c",
    ]);
    // 选项的 key 与 def 的键同一条编码：选完直接写回两格
    expect(parseModelKey(options[2].key)).toEqual({ endpointProfileId: "prof-1", model: "c" });
  });
});
