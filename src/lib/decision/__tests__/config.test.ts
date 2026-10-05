import { describe, expect, it } from "vitest";
import {
  DEFAULT_DECISION_CONFIG,
  DECISION_CONFIG_STORAGE_KEY,
  loadDecisionConfig,
  mergeDecisionConfig,
  saveDecisionConfig,
} from "../config";
import type { ConfigStorage } from "../config";

function mapStorage(): ConfigStorage & { map: Map<string, string> } {
  const map = new Map<string, string>();
  return {
    map,
    getItem: (key) => map.get(key) ?? null,
    setItem: (key, value) => void map.set(key, value),
  };
}

describe("决策层配置", () => {
  it("默认值即安全值：Jev 关、apiKey 空、传输走原生、敏感强制本地开", () => {
    const config = mergeDecisionConfig(undefined);
    expect(config).toEqual(DEFAULT_DECISION_CONFIG);
    expect(config.jev.enabled).toBe(false);
    expect(config.jev.apiKey).toBe("");
    expect(config.jev.transport).toBe("rust"); // CORS 是打包应用的常态，direct 只留给调试
    expect(config.routing.sensitiveForceLocal).toBe(true);
    expect(config.routing.autoUpgradeThreshold).toBe(0.85);
  });

  it("部分覆盖：只动给过的字段，其余保持默认", () => {
    const config = mergeDecisionConfig({ jev: { enabled: true }, routing: { autoUpgradeThreshold: 0.7 } });
    expect(config.jev.enabled).toBe(true);
    expect(config.jev.apiKey).toBe("");
    expect(config.routing.autoUpgradeThreshold).toBe(0.7);
    expect(config.laya.timeoutMs).toBe(DEFAULT_DECISION_CONFIG.laya.timeoutMs);
  });

  it("嵌入点开关默认全关：嵌入不得改变现有行为，除非用户亲手打开", () => {
    const config = mergeDecisionConfig(undefined);
    expect(config.integrations.memoryGate).toBe(false);
    expect(config.integrations.sensitivityScan).toBe(false);
    expect(config.integrations.modelRouting).toBe(false);
    expect(config.integrations.taskAssignment).toBe(false);
    expect(config.integrations.memoryGateThreshold).toBe(0.5);
    // 手改坏的开关值退回关——"读不懂"绝不能解释成"打开"
    const broken = mergeDecisionConfig({ integrations: { memoryGate: "yes", memoryGateThreshold: 9 } });
    expect(broken.integrations.memoryGate).toBe(false);
    expect(broken.integrations.memoryGateThreshold).toBe(0.5);
  });

  it("V2 嵌入点开关默认开（§7）：能力未接线时开了也无从运行，接线后即生效", () => {
    const config = mergeDecisionConfig(undefined);
    expect(config.compaction).toEqual({ enabled: true, sticky: true });
    expect(config.stagedReview).toEqual({ enabled: true, streamingPreCheck: true });
    expect(config.retrieval).toEqual({ enabled: true });
    // 坏值退回默认
    const broken = mergeDecisionConfig({ compaction: { enabled: "on" }, retrieval: { enabled: 1 } });
    expect(broken.compaction.enabled).toBe(true);
    expect(broken.retrieval.enabled).toBe(true);
  });

  it("降级链：去重保序、白名单外成员往返保留（前向兼容）、全空回默认", () => {
    expect(DEFAULT_DECISION_CONFIG.jev.chain).toEqual(["typesafe", "openrouter"]);
    const custom = mergeDecisionConfig({ jev: { chain: ["custom", "typesafe", "custom"] } });
    expect(custom.jev.chain).toEqual(["custom", "typesafe"]);
    // 白名单外（未来厂商）保留：现在跑不到，但不能在配置层把用户的手笔抹掉
    const forward = mergeDecisionConfig({ jev: { chain: ["typesafe", "vercel"] } });
    expect(forward.jev.chain).toEqual(["typesafe", "vercel"]);
    const emptied = mergeDecisionConfig({ jev: { chain: [42, null] } });
    expect(emptied.jev.chain).toEqual(DEFAULT_DECISION_CONFIG.jev.chain);
  });

  it("链上密钥：只认内置厂商名的键，坏键丢弃", () => {
    const config = mergeDecisionConfig({ jev: { apiKeys: { typesafe: "sk-a", vercel: "sk-v", junk: 42 } } });
    expect(config.jev.apiKeys).toEqual({ typesafe: "sk-a" });
  });

  it("手改坏的配置退回默认：类型不对、范围越界、未知键都拦住", () => {
    const config = mergeDecisionConfig({
      enabled: "yes", // 类型不对
      routing: { autoUpgradeThreshold: "0.9", maxUpgradeChain: 99, sensitiveForceLocal: null }, // 越界/类型不对
      jev: { transport: "carrier-pigeon" }, // 传输认不出 → 退回 rust
      unknownTopLevel: { foo: 1 }, // 未知键直接丢
    });
    expect(config.enabled).toBe(true);
    expect(config.routing.autoUpgradeThreshold).toBe(0.85);
    expect(config.routing.maxUpgradeChain).toBe(3); // 越界不钳制到上限，而是整个退回默认——宁要已知的安全值
    expect(config.routing.sensitiveForceLocal).toBe(true);
    expect(config.jev.transport).toBe("rust");
    expect((config as unknown as Record<string, unknown>).unknownTopLevel).toBeUndefined();
  });

  it("storage 往返：save 后 load 拿回同一份", () => {
    const storage = mapStorage();
    const config = mergeDecisionConfig({ cache: { ttlMs: 5000 } });
    saveDecisionConfig(config, storage);
    expect(storage.map.get(DECISION_CONFIG_STORAGE_KEY)).toBeDefined();
    expect(loadDecisionConfig(storage)).toEqual(config);
  });

  it("存储里的坏 JSON 不炸，落回默认值", () => {
    const storage = mapStorage();
    storage.map.set(DECISION_CONFIG_STORAGE_KEY, "{not json");
    expect(loadDecisionConfig(storage)).toEqual(DEFAULT_DECISION_CONFIG);
  });

  it("没有存储（纯 Node）也照常出默认值", () => {
    expect(loadDecisionConfig(undefined)).toEqual(DEFAULT_DECISION_CONFIG);
  });
});
