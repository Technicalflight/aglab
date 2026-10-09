//! 全球模型能力规格目录：把公开的模型元数据集归一成
//! "模型 id → 能力/模态/窗口" 的扁平索引，供档案弹窗勾模型时自动预填与展示。
//!
//! 数据源按用户给的优先级逐个尝试（第一个成功解析的胜出）；
//! 三家 schema 同族（provider → models → {modalities, features|顶层旗标, limit}），
//! 差异只在旗标嵌不嵌 `features` 里——归一化两条都认。同一模型 id 被多家
//! 网关收录时合并：能力与模态取并集（规格是模型本体属性，网关漏标不该丢能力），
//! 窗口取最大。识别失败静默落空：档案弹窗退回内置目录与名字正则，行为不变。

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

/// 数据源（按序尝试）。全是固定 https 公共端点，发请求前逐条校验
const SOURCES: [&str; 5] = [
    "https://gh-proxy.org/https://raw.githubusercontent.com/The-Best-Codes/ai-model-directory/refs/heads/main/data/all.json",
    "https://models5.com/api.json",
    "https://models.dev/api.json",
    "https://v4.gh-proxy.org/https://raw.githubusercontent.com/The-Best-Codes/ai-model-directory/refs/heads/main/data/all.json",
    "https://models5.com/models.json",
];

/// 一个模型的归一规格。能力值与前端 ModelCapability 枚举字面量一致
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DirectorySpec {
    pub capabilities: Vec<String>,
    pub input_modalities: Vec<String>,
    pub output_modalities: Vec<String>,
    /// 0 = 数据源没给
    pub context_tokens: u64,
    /// 0 = 数据源没给
    pub max_tokens: u64,
}

/// 一次成功拉取：来源 + 合并后的索引
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryFetch {
    pub source: String,
    pub total: usize,
    pub models: BTreeMap<String, DirectorySpec>,
}

fn flag(model: &Value, key: &str) -> bool {
    model
        .get("features")
        .and_then(|features| features.get(key))
        .and_then(Value::as_bool)
        .or_else(|| model.get(key).and_then(Value::as_bool))
        .unwrap_or(false)
}

fn side_modalities(model: &Value, side: &str) -> Vec<String> {
    model
        .get("modalities")
        .and_then(|modalities| modalities.get(side))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|s| s.to_ascii_lowercase())
                .collect()
        })
        .unwrap_or_default()
}

fn limit_of(model: &Value, key: &str) -> u64 {
    model
        .get("limit")
        .and_then(|limit| limit.get(key))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// 能力枚举映射。模态之外再吃 reasoning/tool_call 两面旗；
/// 一样没中的兜底 chat——模型不能在 UI 里"消失"
fn caps_of(model: &Value, id_lower: &str, input: &[String], output: &[String]) -> Vec<String> {
    let mut caps: Vec<String> = Vec::new();
    if output.iter().any(|s| s == "image") {
        caps.push("image".into());
    }
    if output.iter().any(|s| s == "video") {
        caps.push("video".into());
    }
    if output.iter().any(|s| s == "audio") {
        caps.push("audio".into());
    }
    if flag(model, "reasoning") {
        caps.push("reasoning".into());
    }
    if flag(model, "tool_call") {
        caps.push("function_call".into());
    }
    if input.iter().any(|s| s == "image") {
        caps.push("vision".into());
    }
    if input.iter().any(|s| s == "video") {
        caps.push("video_recognition".into());
    }
    if id_lower.contains("embedding") {
        caps.push("embedding".into());
    }
    // 输出里有文本的都能对话——包括音频/图像/视频输出齐全的 omni 模型
    // （MiniMax-M3 这类：input/output 全模态，但主业是聊天）。只有 embedding
    // 除外，它不是对话候选。声明表缺了 chat，会话选择器的 chat 候选回退
    // 就找不到它了
    if output.iter().any(|s| s == "text") && !caps.contains(&"embedding".to_string()) {
        caps.push("chat".into());
    }
    if caps.is_empty() {
        caps.push("chat".into());
    }
    caps.sort();
    caps.dedup();
    caps
}

/// 单模型的归一累加器：spec 之外记生成类能力的"信了几家"。
/// 一家网关乱标（302ai 曾把 grok-4.7 标成 output 含 image、MiniMax-M3 标成
/// 全模态输出）不该给整个模型定性——finalize 时按多数信闸收口
struct Acc {
    spec: DirectorySpec,
    image_believers: usize,
    video_believers: usize,
    audio_believers: usize,
    total: usize,
}

fn merge_into(index: &mut BTreeMap<String, Acc>, id: &str, model: &Value) {
    let id_lower = id.to_ascii_lowercase();
    let input = side_modalities(model, "input");
    let output = side_modalities(model, "output");
    let entry = index.entry(id_lower.clone()).or_insert_with(|| Acc {
        spec: DirectorySpec {
            capabilities: Vec::new(),
            input_modalities: Vec::new(),
            output_modalities: Vec::new(),
            context_tokens: 0,
            max_tokens: 0,
        },
        image_believers: 0,
        video_believers: 0,
        audio_believers: 0,
        total: 0,
    });
    entry.total += 1;
    if output.iter().any(|side| side == "image") {
        entry.image_believers += 1;
    }
    if output.iter().any(|side| side == "video") {
        entry.video_believers += 1;
    }
    if output.iter().any(|side| side == "audio") {
        entry.audio_believers += 1;
    }
    for cap in caps_of(model, &id_lower, &input, &output) {
        if !entry.spec.capabilities.contains(&cap) {
            entry.spec.capabilities.push(cap);
        }
    }
    // 输入/输出两侧各自独立去重合并——text 这类两侧都有的模态不能互相挤掉
    for side in &input {
        if !entry.spec.input_modalities.contains(side) {
            entry.spec.input_modalities.push(side.clone());
        }
    }
    for side in &output {
        if !entry.spec.output_modalities.contains(side) {
            entry.spec.output_modalities.push(side.clone());
        }
    }
    entry.spec.context_tokens = entry.spec.context_tokens.max(limit_of(model, "context"));
    entry.spec.max_tokens = entry.spec.max_tokens.max(limit_of(model, "output"));
}

/// 收口：生成类能力（image/video/audio）至少两家声称、或声称者就是全部来源，
/// 才保留并写回能力表；视觉/工具/推理这些输入侧与旗标能力不受闸——
/// 标多了顶多多给开关，不改变模型的类别归属
fn finalize(index: BTreeMap<String, Acc>) -> BTreeMap<String, DirectorySpec> {
    index
        .into_iter()
        .map(|(id, mut acc)| {
            let keep = |believers: usize| believers >= 2 || believers == acc.total;
            for (cap, believers) in [
                ("image", acc.image_believers),
                ("video", acc.video_believers),
                ("audio", acc.audio_believers),
            ] {
                if !keep(believers) {
                    acc.spec.capabilities.retain(|existing| existing != cap);
                    acc.spec
                        .output_modalities
                        .retain(|existing| existing != cap);
                }
            }
            acc.spec.capabilities.sort();
            (id, acc.spec)
        })
        .collect()
}

fn parse_source(text: &str) -> Result<BTreeMap<String, DirectorySpec>, String> {
    let root: Value = serde_json::from_str(text).map_err(|e| format!("不是合法 JSON：{e}"))?;
    let providers = root.as_object().ok_or("顶层不是对象")?;
    let mut index: BTreeMap<String, Acc> = BTreeMap::new();
    for (key, provider) in providers {
        if let Some(models) = provider.get("models").and_then(Value::as_object) {
            for (id, model) in models {
                if model.is_object() {
                    merge_into(&mut index, id, model);
                }
            }
        } else if provider.get("modalities").is_some()
            || provider.get("attachment").is_some()
            || provider.get("features").is_some()
        {
            // models5.com/models.json 形状：顶层键就是模型 id，值直接是模型条目
            merge_into(&mut index, key, provider);
        }
    }
    if index.is_empty() {
        return Err("数据源里没有模型条目".into());
    }
    Ok(finalize(index))
}

fn fetch_directory() -> Result<DirectoryFetch, String> {
    let mut reasons: Vec<String> = Vec::new();
    for source in SOURCES {
        if !source.starts_with("https://") {
            reasons.push(format!("{source}：只允许 https"));
            continue;
        }
        let agent = crate::proxy::agent_for(None)?;
        let request =
            crate::net::with_timeouts(agent.get(source), std::time::Duration::from_secs(30));
        let response = match request.call() {
            Ok(response) => response,
            Err(error) => {
                reasons.push(format!("{source}：{error}"));
                continue;
            }
        };
        if response.status().as_u16() != 200 {
            reasons.push(format!("{source}：HTTP {}", response.status().as_u16()));
            continue;
        }
        let mut text = String::new();
        let mut reader = response.into_body().into_reader();
        if let Err(error) = std::io::Read::read_to_string(&mut reader, &mut text) {
            reasons.push(format!("{source}：{error}"));
            continue;
        }
        match parse_source(&text) {
            Ok(models) => {
                let total = models.len();
                return Ok(DirectoryFetch {
                    source: source.to_string(),
                    total,
                    models,
                });
            }
            Err(error) => reasons.push(format!("{source}：{error}")),
        }
    }
    Err(format!("全部数据源都没拉到：{}", reasons.join("；")))
}

/// 档案弹窗加载全球模型规格。前端带 24h localStorage 缓存，这条本身不另设缓存——
/// 顶多一天一次 8MB 下载，不值得为它维护失效逻辑
#[tauri::command]
pub async fn model_directory() -> Result<DirectoryFetch, String> {
    tauri::async_runtime::spawn_blocking(fetch_directory)
        .await
        .map_err(|e| format!("目录任务中断：{e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn flags_are_read_from_both_schemas() {
        // models.dev/models5 形状：旗标在顶层
        let flat = json!({"attachment": true, "reasoning": true, "tool_call": true, "modalities": {"input": ["text", "image"], "output": ["text"]}});
        assert!(flag(&flat, "reasoning"));
        assert!(flag(&flat, "tool_call"));
        // ai-model-directory 形状：旗标嵌在 features 里
        let nested =
            json!({"features": {"attachment": false, "reasoning": false, "tool_call": true}});
        assert!(flag(&nested, "tool_call"));
        assert!(!flag(&nested, "reasoning"));
        // 都没有 = false，而不是 panic
        assert!(!flag(&json!({}), "reasoning"));
    }

    #[test]
    fn modalities_and_limits_map_to_capabilities() {
        // 生成侧：output 决定生图/视频/音频
        let veo = json!({"modalities": {"input": ["text"], "output": ["video"]}, "limit": {"context": 480}});
        let caps = caps_of(
            &veo,
            "google/veo-3-fast",
            &["text".into()],
            &["video".into()],
        );
        assert!(caps.contains(&"video".to_string()));
        assert!(
            !caps.contains(&"chat".to_string()),
            "纯生成模型（无文本输出）不当对话候选"
        );
        // 理解侧：input 决定 vision / video_recognition
        let gi = json!({"modalities": {"input": ["text", "image", "audio"], "output": ["text"]}, "reasoning": true, "tool_call": true});
        let caps = caps_of(
            &gi,
            "gemini-3-pro",
            &["text".into(), "image".into(), "audio".into()],
            &["text".into()],
        );
        assert!(caps.contains(&"vision".to_string()));
        assert!(caps.contains(&"reasoning".to_string()));
        assert!(caps.contains(&"function_call".to_string()));
        assert!(!caps.contains(&"video_recognition".to_string()));
        // embedding 家族按 id 认，且不再兜底对话（它不是对话候选）
        let embed = json!({"modalities": {"input": ["text"], "output": ["text"]}});
        let embed_caps = caps_of(
            &embed,
            "text-embedding-3-small",
            &["text".into()],
            &["text".into()],
        );
        assert!(embed_caps.contains(&"embedding".to_string()));
        assert!(!embed_caps.contains(&"chat".to_string()));
        // 推理/工具模型与 omni 全模态模型（输出 text+audio+image+video）都是
        // 文本对话的正当候选：声明表里要有 chat
        let omni = json!({"modalities": {"input": ["text", "audio", "image", "video"], "output": ["audio", "image", "text", "video"]}, "reasoning": true, "tool_call": true});
        let omni_caps = caps_of(
            &omni,
            "minimax-m3",
            &[
                "text".into(),
                "audio".into(),
                "image".into(),
                "video".into(),
            ],
            &[
                "audio".into(),
                "image".into(),
                "text".into(),
                "video".into(),
            ],
        );
        assert!(omni_caps.contains(&"chat".to_string()));
        assert!(omni_caps.contains(&"video".to_string()));
        let reasoner = json!({"reasoning": true, "tool_call": true, "modalities": {"input": ["text"], "output": ["text"]}});
        assert!(
            caps_of(&reasoner, "deepseek-r2", &["text".into()], &["text".into()])
                .contains(&"chat".to_string())
        );
        // 全空兜底 chat
        let bare = json!({});
        assert_eq!(
            caps_of(&bare, "mystery", &[], &[]),
            vec!["chat".to_string()]
        );
        // 限长从 limit 里读
        assert_eq!(limit_of(&veo, "context"), 480);
        assert_eq!(limit_of(&veo, "output"), 0);
    }

    #[test]
    fn same_id_across_providers_merges_without_losing_capabilities() {
        let mut index = BTreeMap::new();
        // 网关 A 只标了文本；网关 B 标了图像输入与更大窗口——并集不能丢
        merge_into(
            &mut index,
            "some-model",
            &json!({"modalities": {"input": ["text"], "output": ["text"]}, "limit": {"context": 8192}}),
        );
        merge_into(
            &mut index,
            "some-model",
            &json!({"attachment": true, "modalities": {"input": ["text", "image"], "output": ["text"]}, "limit": {"context": 128000, "output": 4096}}),
        );
        let spec = finalize(index).remove("some-model").unwrap();
        assert!(spec.capabilities.contains(&"vision".to_string()));
        assert!(spec.input_modalities.contains(&"image".to_string()));
        assert_eq!(spec.context_tokens, 128_000);
        assert_eq!(spec.max_tokens, 4_096);
    }

    #[test]
    fn parser_walks_providers_and_rejects_empty_payloads() {
        let payload = json!({
            "deepinfra": {"id": "deepinfra", "models": {
                "tencent/Hy3": {"id": "tencent/Hy3", "reasoning": true, "tool_call": true, "modalities": {"input": ["text"], "output": ["text"]}, "limit": {"context": 256000, "output": 8192}}
            }},
            "broken": {"id": "broken"}
        });
        let index = parse_source(&payload.to_string()).unwrap();
        assert_eq!(index.len(), 1);
        let spec = index.get("tencent/hy3").unwrap();
        assert!(spec.capabilities.contains(&"reasoning".to_string()));
        assert_eq!(spec.context_tokens, 256_000);
        // 空目录与非法形状都算失败——上层换下一个源
        assert!(parse_source("{}").is_err());
        assert!(parse_source("[]").is_err());
    }

    #[test]
    fn flat_shape_treats_top_level_keys_as_models() {
        // models5.com/models.json：顶层键就是模型 id，值直接带模态字段
        let payload = json!({
            "bytedance-seed/seed-2.0-pro": {
                "id": "bytedance-seed/seed-2.0-pro",
                "attachment": true,
                "reasoning": true,
                "tool_call": true,
                "modalities": {"input": ["text", "image"], "output": ["text"]},
                "limit": {"context": 200000, "output": 32768}
            }
        });
        let index = parse_source(&payload.to_string()).unwrap();
        let spec = index.get("bytedance-seed/seed-2.0-pro").unwrap();
        assert!(spec.capabilities.contains(&"vision".to_string()));
        assert!(spec.capabilities.contains(&"function_call".to_string()));
        assert_eq!(spec.context_tokens, 200_000);
    }

    #[test]
    fn single_gateway_output_claim_does_not_poison_the_model() {
        let mut index = BTreeMap::new();
        // 302ai 乱标事故复刻：六家里一家把 grok-4.7 的 output 标成带 image——
        // 多数信闸要把这条生图能力闸掉，别家纯 text 的共识说话
        merge_into(
            &mut index,
            "grok-4.7",
            &json!({"modalities": {"input": ["text"], "output": ["text"]}}),
        );
        merge_into(
            &mut index,
            "grok-4.7",
            &json!({"modalities": {"input": ["text"], "output": ["text"]}}),
        );
        merge_into(
            &mut index,
            "grok-4.7",
            &json!({"modalities": {"input": ["text"], "output": ["text"]}}),
        );
        merge_into(
            &mut index,
            "grok-4.7",
            &json!({"modalities": {"input": ["text"], "output": ["text"]}}),
        );
        merge_into(
            &mut index,
            "grok-4.7",
            &json!({"modalities": {"input": ["text"], "output": ["text"]}}),
        );
        merge_into(
            &mut index,
            "grok-4.7",
            &json!({"modalities": {"input": ["image", "text"], "output": ["image", "text"]}}),
        );
        let spec = finalize(index).remove("grok-4.7").unwrap();
        assert!(
            !spec.capabilities.contains(&"image".to_string()),
            "{:?}",
            spec.capabilities
        );
        assert!(!spec.output_modalities.contains(&"image".to_string()));
        assert!(spec.capabilities.contains(&"chat".to_string()));
        assert!(
            spec.capabilities.contains(&"vision".to_string()),
            "输入侧不受闸"
        );

        // 反例：模型只被一家收录、那家声称生图（dall-e 在 poe）——声称者即全部，保留
        let mut single = BTreeMap::new();
        merge_into(
            &mut single,
            "dall-e-3",
            &json!({"modalities": {"input": ["text"], "output": ["image"]}}),
        );
        let spec = finalize(single).remove("dall-e-3").unwrap();
        assert!(spec.capabilities.contains(&"image".to_string()));
    }

    #[test]
    fn every_source_is_a_fixed_https_public_endpoint() {
        for source in SOURCES {
            assert!(source.starts_with("https://"), "{source}");
            let host = source
                .split("//")
                .nth(1)
                .unwrap()
                .split('/')
                .next()
                .unwrap();
            assert!(!host.starts_with("localhost"), "{source}");
            assert!(!host.starts_with("127."), "{source}");
            assert!(!host.starts_with("192.168."), "{source}");
            assert!(!host.starts_with("10."), "{source}");
        }
    }
}
