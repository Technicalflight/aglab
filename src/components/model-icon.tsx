import type { ComponentType } from "react";
import {
  Azure,
  Baichuan,
  Bedrock,
  Claude,
  Cohere,
  DeepSeek,
  Doubao,
  Fireworks,
  Gemma,
  Gemini,
  Grok,
  Kimi,
  Groq,
  Hunyuan,
  Meta,
  Minimax,
  Mistral,
  Moonshot,
  Ollama,
  OpenAI,
  OpenRouter,
  Perplexity,
  Qwen,
  SiliconCloud,
  Spark,
  Stability,
  Stepfun,
  Together,
  Wenxin,
  Yi,
  Zhipu,
} from "@lobehub/icons";

import { cn } from "@/lib/utils";

type IconComponent = ComponentType<{ size?: number | string; className?: string }>;

/**
 * 模型/服务商 → lobe-icons 品牌图标（https://github.com/lobehub/lobe-icons）。
 * 匹配按**顺序取第一条命中**：专有名在前、宽泛词在后（`gpt-4o` 里的 `o`
 * 不该单独成词，`\bo\d\b` 的边界就是为它写的）。没命中给首字母灰牌，
 * 不硬凑一个错误的牌子。
 */
const MODEL_RULES: Array<[RegExp, IconComponent]> = [
  [/deepseek/, DeepSeek],
  [/claude|anthropic/, Claude],
  [/gemini/, Gemini],
  [/\bgemma\b/, Gemma],
  [/qwen|qwq|qvq|dashscope/, Qwen],
  [/kimi/, Kimi],
  [/moonshot/, Moonshot],
  [/glm|zhipu|chatglm|cogview|cogvideo/, Zhipu],
  [/mistral|mixtral|ministral|codestral|pixtral/, Mistral],
  [/llama/, Meta],
  [/grok|\bxai\b/, Grok],
  [/minimax|abab/, Minimax],
  [/ernie|wenxin/, Wenxin],
  [/hunyuan/, Hunyuan],
  [/\bspark\b|xfyun|iflytek/, Spark],
  [/doubao|skylark/, Doubao],
  [/cohere|command-(r|a)/, Cohere],
  [/perplexity|\bsonar\b/, Perplexity],
  [/\byi[-/]|01-?ai|lingyi/, Yi],
  [/baichuan/, Baichuan],
  [/stepfun|step-/, Stepfun],
  [/stable-diffusion|sdxl|stability/, Stability],
  [/ollama/, Ollama],
  [/groq/, Groq],
  [/openrouter/, OpenRouter],
  [/together/, Together],
  [/fireworks/, Fireworks],
  [/siliconflow|siliconcloud/, SiliconCloud],
  [/bedrock/, Bedrock],
  [/azure/, Azure],
  // OpenAI 放最后兜底：gpt/o1/o3/o4/chatgpt/davinci/codex 都是它家
  [/gpt|openai|chatgpt|davinci|\bo[1345]\b|\bcodex\b/, OpenAI],
];

/** 服务商档案 → 图标：按 Base URL 与线协议猜。猜不出回落到按模型名猜 */
const PROVIDER_RULES: Array<[RegExp, IconComponent]> = [
  [/anthropic\.com|\banthropic\b/, Claude],
  [/generativelanguage\.googleapis\.com|aiplatform\.googleapis\.com/, Gemini],
  [/api\.deepseek\.com|deepseek/, DeepSeek],
  [/dashscope\.aliyuncs/, Qwen],
  [/api\.moonshot|moonshot\.cn|moonshot\.ai/, Moonshot],
  [/bigmodel\.cn|\bzhipu\b/, Zhipu],
  [/api\.mistral\.ai|mistral/, Mistral],
  [/api\.x\.ai|\bxai\b|grok/, Grok],
  [/api\.groq\.com|groq/, Groq],
  [/openrouter\.ai|openrouter/, OpenRouter],
  [/api\.together|together\.xyz/, Together],
  [/api\.fireworks\.ai|fireworks/, Fireworks],
  [/api\.siliconflow|siliconcloud/, SiliconCloud],
  [/api\.cohere|cohere/, Cohere],
  [/api\.perplexity|perplexity/, Perplexity],
  [/api\.minimax|minimax/, Minimax],
  [/api\.baichuan|baichuan/, Baichuan],
  [/api\.stepfun|stepfun/, Stepfun],
  [/localhost:11434|:11434|ollama/, Ollama],
  [/openai\.azure\.com|\bazure\b/, Azure],
  [/bedrock/, Bedrock],
  [/api\.lingyiwanwu|\byi\b/, Yi],
  [/hunyuan|tencent/, Hunyuan],
  [/xfyun|iflytek|spark/, Spark],
  [/doubao|volces|bytedance/, Doubao],
  [/wenxin|baidu/, Wenxin],
  [/api\.openai\.com|openai/, OpenAI],
];

/** 模型名 → 图标组件。null = 没认出来（调用方给灰牌兜底） */
export function modelIconFor(model: string): IconComponent | null {
  const name = model.toLowerCase();
  for (const [pattern, Icon] of MODEL_RULES) {
    if (pattern.test(name)) return Icon;
  }
  return null;
}

/** 服务商 → 图标组件。Base URL 优先，线协议与模型名接在后面 */
export function providerIconFor(baseUrl: string, apiFormat: string, model: string): IconComponent | null {
  const haystack = `${baseUrl} ${apiFormat}`.toLowerCase();
  for (const [pattern, Icon] of PROVIDER_RULES) {
    if (pattern.test(haystack)) return Icon;
  }
  return modelIconFor(model);
}

/** 没认出牌子时的兜底：首字母灰牌，不冒充任何一家 */
function LetterFallback({ label, size, className }: { label: string; size: number; className?: string }) {
  return (
    <span
      aria-hidden
      style={{ width: size, height: size, fontSize: Math.max(9, Math.round(size * 0.55)) }}
      className={cn(
        "flex shrink-0 select-none items-center justify-center rounded-xs bg-muted font-semibold leading-none text-muted-foreground",
        className,
      )}
    >
      {label.slice(0, 1).toUpperCase() || "?"}
    </span>
  );
}

/** 模型的品牌图标。认不出牌子给首字母灰牌 */
export function ModelIcon({
  model,
  size = 14,
  className,
}: {
  model: string;
  size?: number;
  className?: string;
}) {
  const Icon = modelIconFor(model);
  if (!Icon) return <LetterFallback label={model} size={size} className={className} />;
  return <Icon size={size} className={cn("shrink-0", className)} />;
}

/** 服务商档案的品牌图标。按 Base URL 与线协议认；认不出按它家默认模型再认 */
export function ProviderIcon({
  baseUrl,
  apiFormat,
  model,
  size = 14,
  className,
}: {
  baseUrl: string;
  apiFormat: string;
  model: string;
  size?: number;
  className?: string;
}) {
  const Icon = providerIconFor(baseUrl, apiFormat, model);
  if (!Icon) return <LetterFallback label={model || baseUrl} size={size} className={className} />;
  return <Icon size={size} className={cn("shrink-0", className)} />;
}

