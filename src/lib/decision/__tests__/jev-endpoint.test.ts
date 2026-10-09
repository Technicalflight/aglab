/**
 * 自定义服务商那条规则的前端这一半。
 *
 * `JEV_URL_CASES` 是两边唯一的共同口径：Rust 侧 `decision.rs::jev_endpoint_problem`
 * 用 `include_str!` 读同一张表逐行比对（那条测试叫
 * `custom_endpoint_cases_match_the_frontend_table`）。加一条用例只改这张表，
 * 两边一起绿或一起红——各写一份规则迟早会漂，漂了也没人看得见。
 */
import { describe, expect, it } from "vitest";
import { JEV_URL_CASES, jevEndpointProblem } from "../providers/jev";
import { DEFAULT_DECISION_CONFIG, mergeDecisionConfig } from "../config";

describe("Jev 自定义服务商的形式规则", () => {
  it("表本身两边都有货（全是合法或全是不合法的表，钉不住任何一边）", () => {
    const ok = JEV_URL_CASES.filter((entry) => entry.ok).length;
    expect(ok).toBeGreaterThanOrEqual(5);
    expect(JEV_URL_CASES.length - ok).toBeGreaterThanOrEqual(8);
  });

  it("表上每一条都按规则判过", () => {
    for (const { url, ok } of JEV_URL_CASES) {
      expect(
        jevEndpointProblem(url) === null,
        `${JSON.stringify(url)} 应判「${ok ? "合法" : "不合法"}」`,
      ).toBe(ok);
    }
  });

  it("规则只判形式：元数据地址形式上合法，拦它的是出口名单", () => {
    expect(jevEndpointProblem("https://169.254.169.254/latest/meta-data")).toBeNull();
  });
});

describe("自定义服务商过配置合并", () => {
  it("via 认 custom，baseUrl 原样留着——合并这一层不替用户改写那一格", () => {
    const merged = mergeDecisionConfig({
      jev: { via: "custom", baseUrl: "http://localhost:8787/v1/systemone" },
    });
    expect(merged.jev.via).toBe("custom");
    expect(merged.jev.baseUrl).toBe("http://localhost:8787/v1/systemone");

    // 坏地址也不在这里悄悄换成别的：它由 provider 的可用性消化，
    // 而"为什么被跳过"写在设置页那一格的说明上
    const broken = mergeDecisionConfig({
      jev: { via: "custom", baseUrl: "http://api.example.com/v1" },
    });
    expect(broken.jev.baseUrl).toBe("http://api.example.com/v1");
    expect(jevEndpointProblem(broken.jev.baseUrl)).not.toBeNull();
  });

  it("认不出的厂商名与类型不对的地址都退回默认，不照抄", () => {
    expect(mergeDecisionConfig({ jev: { via: "https://evil.example.com" } }).jev.via).toBe(
      DEFAULT_DECISION_CONFIG.jev.via,
    );
    expect(mergeDecisionConfig({ jev: { baseUrl: 42 } }).jev.baseUrl).toBe("");
  });

  it("决策池：老配置没有 endpoints，单独那格 baseUrl 迁成池里唯一一条", () => {
    const migrated = mergeDecisionConfig({
      jev: { via: "custom", baseUrl: "http://localhost:8787/v1/systemone" },
    });
    expect(migrated.jev.endpoints).toEqual([
      { name: "服务商 1", baseUrl: "http://localhost:8787/v1/systemone", apiKey: "" },
    ]);
    // 老配置连 baseUrl 都没有：池子空着，不可用性由 provider 那层说
    expect(mergeDecisionConfig({ jev: {} }).jev.endpoints).toEqual([]);
  });

  it("决策池：新配置逐条合并，字段各自退默认，整条空白丢弃", () => {
    const merged = mergeDecisionConfig({
      jev: {
        via: "custom",
        endpoints: [
          { name: "甲", baseUrl: "https://a.example.com/v1/systemone", apiKey: "sk-a" },
          "junk",
          { name: "", baseUrl: "", apiKey: "" },
          { baseUrl: "https://b.example.com/v1/systemone" },
        ],
      },
    });
    expect(merged.jev.endpoints).toEqual([
      { name: "甲", baseUrl: "https://a.example.com/v1/systemone", apiKey: "sk-a" },
      { name: "", baseUrl: "https://b.example.com/v1/systemone", apiKey: "" },
    ]);
    // endpoints 不是数组（手改坏）就当没有：退回迁移路径
    expect(
      mergeDecisionConfig({ jev: { baseUrl: "https://solo.example.com/v1" } }).jev.endpoints,
    ).toEqual([{ name: "服务商 1", baseUrl: "https://solo.example.com/v1", apiKey: "" }]);
  });
});
