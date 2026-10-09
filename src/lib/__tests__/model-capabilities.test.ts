import { describe, expect, it } from "vitest";

import {
  classifyModelCapabilities,
  inferCapabilitiesFromIdentifier,
  isChatModel,
  isGenerateImageModel,
  isGenerateVideoModel,
  isVisionModel,
  resolveCapabilities,
  ModelCapability,
} from "@/lib/model-capabilities";
import type { ModelIdentity } from "@/lib/model-capabilities";

const model = (parts: Partial<ModelIdentity>): ModelIdentity => ({
  id: "",
  name: "",
  ...parts,
});

describe("模型能力识别（声明优先、回退兜底、纯函数收口）", () => {
  it("标准模型 gpt-4o：识别为 对话 + 视觉（视觉是附加能力，与兜底对话并存）", () => {
    const caps = resolveCapabilities(model({ id: "gpt-4o", name: "GPT-4o" }));
    expect(caps).toContain(ModelCapability.Chat);
    expect(caps).toContain(ModelCapability.ImageRecognition);
  });

  it("生图模型 flux-1.1-pro：仅通过合并标识串命中，不附带兜底对话", () => {
    const caps = resolveCapabilities(model({ id: "flux-1.1-pro", name: "Flux" }));
    expect(caps).toEqual([ModelCapability.ImageGeneration]);
  });

  it("自定义 id 对不上但名称命中的模型：合并串让名称照样说话", () => {
    const caps = resolveCapabilities(model({ id: "volcengine-abc", name: "Sora 2" }));
    expect(caps).toEqual([ModelCapability.VideoGeneration]);
    expect(isGenerateVideoModel(model({ id: "volcengine-abc", name: "Sora 2" }))).toBe(true);
  });

  it("未知模型：兜底为对话，绝不返回空数组（空数组等于在 UI 里消失）", () => {
    const caps = resolveCapabilities(model({ id: "custom-model-x", name: "自定义模型" }));
    expect(caps).toEqual([ModelCapability.Chat]);
    expect(isChatModel(model({ id: "custom-model-x", name: "自定义模型" }))).toBe(true);
  });

  it("显式声明的 capabilities 被直接信任：跳过回退，哪怕名字长得像别的", () => {
    const caps = resolveCapabilities(
      model({ id: "flux-1.1-pro", name: "Flux", capabilities: [ModelCapability.Chat] }),
    );
    expect(caps).toEqual([ModelCapability.Chat]);
    expect(
      isGenerateImageModel(
        model({ id: "flux-1.1-pro", name: "Flux", capabilities: [ModelCapability.Chat] }),
      ),
      "声明说是对话模型，就不该再被名字里的 flux 拉去当生图模型",
    ).toBe(false);
  });

  it("回退推理不写回：连续两次调用结果一致，原始声明不被污染", () => {
    const raw = model({ id: "volcengine-abc", name: "Sora 2" });
    const first = inferCapabilitiesFromIdentifier(raw);
    const second = inferCapabilitiesFromIdentifier(raw);
    expect(first).toEqual(second);
    expect(raw.capabilities).toBeUndefined();
  });

  it("视觉判断兼顾输入模态：没有 vision 标注但元数据里能收图的也算", () => {
    const byModality = model({
      id: "m2",
      name: "某多模态模型",
      inputModalities: ["text", "image"],
    });
    expect(isVisionModel(byModality)).toBe(true);
  });

  it("aglab 名单：seedream → 生图，cogvideo/可灵 → 视频，深度推理 → 推理", () => {
    expect(classifyModelCapabilities("doubao-seedream-4.0")).toContain(
      ModelCapability.ImageGeneration,
    );
    expect(classifyModelCapabilities("cogvideox-2")).toContain(ModelCapability.VideoGeneration);
    expect(classifyModelCapabilities("可灵 2.0")).toContain(ModelCapability.VideoGeneration);
    expect(classifyModelCapabilities("deepseek-reasoner")).toContain(ModelCapability.Reasoning);
    // 真机翻过车的名字：gpt-image 家族整族都是生图，不能落回对话
    expect(classifyModelCapabilities("gpt-image-2.5")).toContain(ModelCapability.ImageGeneration);
    expect(classifyModelCapabilities("gpt-image-2.5-sunburst")).toContain(
      ModelCapability.ImageGeneration,
    );
    expect(classifyModelCapabilities("grok-2-image")).toContain(ModelCapability.ImageGeneration);
    expect(classifyModelCapabilities("nano-banana")).toContain(ModelCapability.ImageGeneration);
  });

  it("厂商名子串碰撞：inkling 不是可灵，thinkingmachines 不触发推理", () => {
    // "in·kling" 的子串恰是可灵的关键词——真机翻车：四个 inkling 全被撞进视频组
    expect(classifyModelCapabilities("thinkingmachines/inkling")).not.toContain(
      ModelCapability.VideoGeneration,
    );
    expect(classifyModelCapabilities("thinkingmachines/inkling:free")).toContain(
      ModelCapability.Chat,
    );
    // 真可灵不受词边界影响：独立成词、品牌连写都照认
    expect(classifyModelCapabilities("kling-v2")).toContain(ModelCapability.VideoGeneration);
    expect(classifyModelCapabilities("klingai-v3")).toContain(ModelCapability.VideoGeneration);
    // 同类碰撞：厂商前缀不算推理，名字后缀 -thinking 仍然是
    expect(classifyModelCapabilities("thinkingmachines/inkling")).not.toContain(
      ModelCapability.Reasoning,
    );
    expect(classifyModelCapabilities("glm-4.5-thinking")).toContain(ModelCapability.Reasoning);
  });
});
