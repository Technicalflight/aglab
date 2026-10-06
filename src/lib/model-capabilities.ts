/**
 * 模型能力识别的唯一收口（design：能力会话 / 整合识别模型能力的设计）。
 *
 * 三层结构，精髓是"声明优先、回退兜底、纯函数收口"：
 *
 * 1. **声明**：`ModelIdentity.capabilities` 是开发者/注册表声明的意图，
 *    非空即信任——回退推理的结果**永不写回**，不污染原始数据；
 * 2. **回退**：capabilities 为空时，按"视频 → 生图 → 推理"的优先级对
 *    `id + ' ' + name` 的**合并字符串**跑正则（合并而不是 ID/名称分别匹配，
 *    避免"ID 命中生图、名称命中视频"的冲突），视觉是附加能力与上面并存，
 *    一类都没中就兜底对话——模型不能在 UI 里"消失"；
 * 3. **收口**：所有判断走 `resolveCapabilities`，UI 只消费 is* 谓词，
 *    不直接读 capabilities、不关心判断过程。
 *
 * 能力枚举的成员名按设计文档，**值与已存配置兼容**（档案里标过的
 * `image`/`video`/`chat` 短值就是这里的枚举值，存量数据零迁移）。
 */

import { useMemo } from "react";
import type { ConversationKind } from "@/types/chat";

export enum ModelCapability {
  Chat = "chat",
  Reasoning = "reasoning",
  ImageGeneration = "image",
  VideoGeneration = "video",
  AudioGeneration = "audio",
  ImageRecognition = "vision",
  VideoRecognition = "video_recognition",
  FunctionCall = "function_call",
  Embedding = "embedding",
}

/** 能力识别的事实源：一个模型的身份（id + 用户可见名）与其声明的能力/输入模态 */
export interface ModelIdentity {
  /** 模型名（与端点侧的原样匹配键）。id 与 name 在本应用里同源 */
  id: string;
  /** 用户可见名称，可能包含 "Flux" "Sora" 等关键词 */
  name: string;
  /** 声明的能力。空/未覆盖 = 走回退推理 */
  capabilities?: string[];
  /** 输入模态（部分服务商的元数据给到这一级，视觉判断要多看一眼它） */
  inputModalities?: Array<"text" | "image" | "video" | "audio">;
}

export const CAPABILITY_LABEL: Partial<Record<ModelCapability, string>> = {
  [ModelCapability.Chat]: "对话",
  [ModelCapability.ImageGeneration]: "生图",
  [ModelCapability.VideoGeneration]: "视频",
  [ModelCapability.AudioGeneration]: "音频",
  [ModelCapability.ImageRecognition]: "视觉",
  [ModelCapability.Reasoning]: "推理",
  [ModelCapability.FunctionCall]: "工具",
  [ModelCapability.VideoRecognition]: "视频理解",
  [ModelCapability.Embedding]: "向量",
};

// ---- 回退正则（大小写不敏感；作用于合并后的标识串） ----

const IMAGE_GEN_PATTERNS = [
  /flux/i,
  /diffusion/i,
  /dall-?e/i,
  /stable-diffusion/i,
  /sdxl/i,
  /midjourney/i,
  /imagen/i,
  /qwen-image/i,
  /seedream/i,
  /seededit/i,
  /cogview/i,
  /kolors/i,
  /irag/i,
  // 名字里带 image 的家族（gpt-image-2.5、grok-2-image…）与常见独立生图品牌
  /image/i,
  /banana/i,
  /hidream/i,
  /ideogram/i,
  /recraft/i,
];

const VIDEO_GEN_PATTERNS = [
  /sora/i,
  // 词边界必须加："thinkingmachines/inkling" 的子串恰是 "kling"——整族文本模型
  // 被误判成可灵视频（真机翻车：档案弹窗视频组全是被撞进来的 inkling）
  /\bkling/i,
  /可灵/i,
  /runway/i,
  /pika/i,
  /veo/i,
  /wan[-_. ]?(2|video)/i,
  /hunyuan[-_. ]?video/i,
  /seedance/i,
  /cogvideo/i,
  /vidu/i,
];

const AUDIO_GEN_PATTERNS = [
  /tts/i,
  /tts-\d/i,
  /speech/i,
  /mini-audio/i,
  /audio/i,
  /voice/i,
  /cosyvoice/i,
  /fish-?speech/i,
  // 音乐生成家族：MiniMax music 系、Mureka、Suno 网关
  /music/i,
  /mureka/i,
  /suno/i,
];

const REASONING_PATTERNS = [
  /o1-preview/i,
  /o1-mini/i,
  /\bo3\b/i,
  /deepseek-reasoner/i,
  /\br1\b/i,
  /reasoner/i,
  // 尾边界：厂商前缀 "thinkingmachines" 不算推理，模型名后缀 "-thinking" 才算
  /\bthinking\b/i,
];

const VISION_PATTERNS = [
  /vision/i,
  /multimodal/i,
  /llava/i,
  /gpt-4o/i,
  /claude-3/i,
  /claude-4/i,
  /gemini/i,
  /qwen.*vl/i,
];

/** 回退检测的唯一入口：id 与 name 拼成合并串，一遍正则按优先级收集 */
export function getModelIdentifier(model: ModelIdentity): string {
  return `${model.id} ${model.name}`.toLowerCase();
}

function matchAgainstPatterns(identifier: string): ModelCapability[] {
  const caps: ModelCapability[] = [];
  // 优先级顺序：视频最特殊先判，其次生图、推理——命中即入列
  if (VIDEO_GEN_PATTERNS.some((pattern) => pattern.test(identifier))) {
    caps.push(ModelCapability.VideoGeneration);
  }
  if (IMAGE_GEN_PATTERNS.some((pattern) => pattern.test(identifier))) {
    caps.push(ModelCapability.ImageGeneration);
  }
  if (AUDIO_GEN_PATTERNS.some((pattern) => pattern.test(identifier))) {
    caps.push(ModelCapability.AudioGeneration);
  }
  if (REASONING_PATTERNS.some((pattern) => pattern.test(identifier))) {
    caps.push(ModelCapability.Reasoning);
  }
  // 生成/推理一类都没中才兜底对话：模型至少可用，不能在 UI 里消失。
  // 视觉是附加能力（会看图与会画图是两件事），与上面并存
  if (caps.length === 0) {
    caps.push(ModelCapability.Chat);
  }
  if (VISION_PATTERNS.some((pattern) => pattern.test(identifier))) {
    caps.push(ModelCapability.ImageRecognition);
  }
  return caps;
}

/** 回退推理：对合并标识串跑正则。结果不写回任何地方，纯计算 */
export function inferCapabilitiesFromIdentifier(model: ModelIdentity): ModelCapability[] {
  return matchAgainstPatterns(getModelIdentifier(model));
}

/** 唯一对外接口：声明非空直接信任，否则回退推理 */
export function resolveCapabilities(model: ModelIdentity): ModelCapability[] {
  if (model.capabilities && model.capabilities.length > 0) {
    return model.capabilities as ModelCapability[];
  }
  return inferCapabilitiesFromIdentifier(model);
}

// ---- 能力谓词（每个只查一个能力；纯函数，UI 与逻辑层都只吃这些） ----

/** 对话模型：声明了对话，或既不是生成图也不是生成视频的 */
export const isChatModel = (model: ModelIdentity): boolean => {
  const caps = resolveCapabilities(model);
  return (
    caps.includes(ModelCapability.Chat) ||
    (!caps.includes(ModelCapability.ImageGeneration) && !caps.includes(ModelCapability.VideoGeneration))
  );
};

export const isGenerateImageModel = (model: ModelIdentity): boolean =>
  resolveCapabilities(model).includes(ModelCapability.ImageGeneration);

export const isGenerateVideoModel = (model: ModelIdentity): boolean =>
  resolveCapabilities(model).includes(ModelCapability.VideoGeneration);

export const isGenerateAudioModel = (model: ModelIdentity): boolean =>
  resolveCapabilities(model).includes(ModelCapability.AudioGeneration);

/** 视觉理解（能看图）：声明的能力或输入模态里有图都算——
 *  不同服务商的元数据粒度不一样，两条路都要通 */
export const isVisionModel = (model: ModelIdentity): boolean => {
  const caps = resolveCapabilities(model);
  return (
    caps.includes(ModelCapability.ImageRecognition) ||
    model.inputModalities?.includes("image") === true
  );
};

export const isReasoningModel = (model: ModelIdentity): boolean =>
  resolveCapabilities(model).includes(ModelCapability.Reasoning);

/** 生图/视频模型默认收图：图生图参照、画面参照都吃图片本体，「图像」开关对这类模型
 *  预置为开（声明的能力或名字识别里带生成能力即算）。对话模型的收图能力仍是显式配置 */
export const acceptsImagesByDefault = (model: ModelIdentity): boolean => {
  const caps = resolveCapabilities(model);
  return caps.includes(ModelCapability.ImageGeneration) || caps.includes(ModelCapability.VideoGeneration);
};

/** UI 侧的 hook：按当前模型算一遍能力谓词。identity 引用变了才算，别的不重算 */
export function useModelCapabilities(model: ModelIdentity | null) {
  const id = model?.id ?? "";
  const name = model?.name ?? "";
  const capabilities = model?.capabilities;
  const inputModalities = model?.inputModalities;

  return useMemo(() => {
    const identity: ModelIdentity = { id, name, capabilities, inputModalities };
    const caps = resolveCapabilities(identity);
    return {
      capabilities: caps,
      isChatModel: caps.includes(ModelCapability.Chat),
      isImageGeneration: caps.includes(ModelCapability.ImageGeneration),
      isVideoGeneration: caps.includes(ModelCapability.VideoGeneration),
      isAudioGeneration: caps.includes(ModelCapability.AudioGeneration),
      isVision: caps.includes(ModelCapability.ImageRecognition) || inputModalities?.includes("image") === true,
      isReasoning: caps.includes(ModelCapability.Reasoning),
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps -- 依赖数组用展开值：capabilities 数组引用不稳定，按值比对
  }, [id, name, JSON.stringify(capabilities ?? []), JSON.stringify(inputModalities ?? [])]);
}

// ---- 与既有调用形态的兼容层（会话档 → 能力；名字直查） ----

/** 会话能力档 → 模型能力枚举。视频会话找视频模型，以此类推 */
export function capabilityForKind(kind: ConversationKind): ModelCapability {
  return kind === "image"
    ? ModelCapability.ImageGeneration
    : kind === "video"
      ? ModelCapability.VideoGeneration
      : kind === "music"
        ? ModelCapability.AudioGeneration
        : ModelCapability.Chat;
}

/** 旧形态：只有模型名时的快速判定（id 与 name 同名，合并串退化为名字本身） */
export function classifyModelCapabilities(name: string): ModelCapability[] {
  return inferCapabilitiesFromIdentifier({ id: name, name });
}

/** 旧形态：名字 + 档案模型表（显式标注优先，否则启发式） */
export function capabilitiesOf(
  name: string,
  specs?: Array<{ model: string; capabilities?: string[] }>,
): string[] {
  const spec = specs?.find((item) => item.model === name);
  return resolveCapabilities({
    id: name,
    name,
    capabilities: spec?.capabilities,
  }) as string[];
}
