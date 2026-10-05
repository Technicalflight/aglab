/**
 * 装配与"改配置立即生效"。设置页每一格都走 applyDecisionConfig，
 * 这条测试钉的是它的合同：保存 → 换掉单例 → 下一次拿到的就是新配置装出来的系统。
 */
import { describe, expect, it } from "vitest";
import {
  DEFAULT_DECISION_CONFIG,
  applyDecisionConfig,
  assembleDecisionSystem,
  getDecisionSystem,
  mergeDecisionConfig,
  resetDecisionSystem,
} from "../index";
import type { ConfigStorage } from "../config";

function mapStorage(): ConfigStorage & { map: Map<string, string> } {
  const map = new Map<string, string>();
  return {
    map,
    getItem: (key) => map.get(key) ?? null,
    setItem: (key, value) => void map.set(key, value),
  };
}

describe("配置改了立即生效", () => {
  it("单例按新配置重装：Jev 从无到有，key 真的装进了 Provider", () => {
    const storage = mapStorage();
    const before = getDecisionSystem(storage);
    expect(before.jev).toBeNull(); // 默认 Jev 关着

    const after = applyDecisionConfig(
      mergeDecisionConfig({ jev: { enabled: true, apiKey: "sk-test" } }),
      storage,
    );
    expect(after).not.toBe(before);
    expect(after.config.jev.enabled).toBe(true);
    expect(after.jev?.isAvailable).toBe(true);
    // 再取一次是同一个实例：重建只发生在那一次改动上，不是每次读配置都重装
    expect(getDecisionSystem(storage)).toBe(after);
    // 存储里落下的是新那一份，不是默认值
    expect(JSON.parse(storage.map.get("aglab.decisionLayer.config") ?? "{}")).toMatchObject({
      jev: { enabled: true },
    });

    resetDecisionSystem();
  });

  it("关掉的字段真的关掉：laya 换成没有服务商的配置后，敏感请求就没法再决策", async () => {
    const system = assembleDecisionSystem(
      mergeDecisionConfig({ laya: { ...DEFAULT_DECISION_CONFIG.laya, sidecarEndpoint: "" } }),
    );
    expect(system.laya.isAvailable).toBe(false);
    await expect(
      system.router.decide({
        state: "secret",
        questions: { q: { type: "noul", instructions: "?" } },
        sensitivity: "confidential",
      }),
    ).rejects.toThrow(/本地|不可用/);
  });

  it("系统上带着面板要用的那两个实例（不暴露它们，面板就只能靠自己再造一套）", () => {
    const system = assembleDecisionSystem(mergeDecisionConfig(undefined));
    expect(typeof system.laya.health).toBe("function");
    expect(system.audit).not.toBeNull();
    expect(system.cache).not.toBeNull();
  });

  it("主 via 永远是链的第一跳：via=custom 的老用户不被默认链绕过（设置页通了、面板 401 的回归钉）", () => {
    // 老用户形态：主 via=custom + 决策池一条自建服务商，chain 还是默认（不含 custom）。
    // 装配出来的链首必须是 custom——否则链会拿他的钥匙去打 typesafe 官方服务商（401 fail-fast）
    const system = assembleDecisionSystem(
      mergeDecisionConfig({
        jev: {
          enabled: true,
          useKeyring: true,
          via: "custom",
          endpoints: [{ name: "自建", baseUrl: "https://sub.nekopeer.com/v1/systemone", apiKey: "" }],
        },
      }),
    );
    expect(system.jev).not.toBeNull();
    expect(system.jev!.hopNames).toEqual(["custom", "typesafe", "openrouter"]);
  });

  it("主 via 已在链中时不重复插入：via=typesafe 链序保持默认", () => {
    const system = assembleDecisionSystem(
      mergeDecisionConfig({ jev: { enabled: true, apiKey: "sk-test", via: "typesafe" } }),
    );
    expect(system.jev!.hopNames).toEqual(["typesafe", "openrouter"]);
  });
});
