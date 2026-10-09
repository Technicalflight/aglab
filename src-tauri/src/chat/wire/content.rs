use crate::config::AppConfig;
use serde_json::{json, Value};

/// 图片块的三家外壳。中性形只存在于日志投影里，出站前必须落到某一家认得的形状
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImageDialect {
    Chat,
    Responses,
    Anthropic,
    Gemini,
}

pub(crate) fn text_block(dialect: ImageDialect, text: &str) -> Value {
    match dialect {
        // responses 的输入块叫 input_text，另两家叫 text
        ImageDialect::Responses => json!({ "type": "input_text", "text": text }),
        _ => json!({ "type": "text", "text": text }),
    }
}

pub(crate) fn image_block(
    dialect: ImageDialect,
    payload: &crate::session::entry::MediaPayload,
) -> Value {
    let data_url = format!("data:{};base64,{}", payload.mime, payload.base64);
    match dialect {
        ImageDialect::Chat => json!({ "type": "image_url", "image_url": { "url": data_url } }),
        ImageDialect::Responses => json!({ "type": "input_image", "image_url": data_url }),
        ImageDialect::Anthropic => json!({
            "type": "image",
            "source": { "type": "base64", "media_type": payload.mime, "data": payload.base64 },
        }),
        ImageDialect::Gemini => json!({
            "inline_data": { "mime_type": payload.mime, "data": payload.base64 },
        }),
    }
}

/// 音频外壳。chat/responses 同用 OpenAI 的 `input_audio`（要三字母格式：audio/mpeg
/// 要写成 mp3，其余按子类型透传，端点认不认由它说）；Gemini 走 inline_data。
/// Anthropic 不收音频——投影层在 outcome 之前就把它挡成正文说明，到不了这里
pub(crate) fn audio_block(
    dialect: ImageDialect,
    payload: &crate::session::entry::MediaPayload,
) -> Value {
    let subtype = payload.mime.rsplit('/').next().unwrap_or("mp3");
    let format = if subtype == "mpeg" { "mp3" } else { subtype };
    match dialect {
        ImageDialect::Chat | ImageDialect::Responses => json!({
            "type": "input_audio",
            "input_audio": { "data": payload.base64, "format": format },
        }),
        ImageDialect::Gemini => json!({
            "inline_data": { "mime_type": payload.mime, "data": payload.base64 },
        }),
        // 防御臂：投影已挡，真走到这里就退回一句正文，别发非法外壳
        ImageDialect::Anthropic => {
            text_block(dialect, "（音频没发出去）Anthropic 线不收音频输入。")
        }
    }
}

/// 视频外壳。chat 线没有标准外壳，Qwen/GLM/OpenRouter 系通行 `video_url` 的 data URL
/// （生成管线 edit 模式同款）；responses 线没有标准外壳，按同款发，认不认由端点说；
/// Gemini 走 inline_data。Anthropic 不收视频——投影层挡，同 audio
pub(crate) fn video_block(
    dialect: ImageDialect,
    payload: &crate::session::entry::MediaPayload,
) -> Value {
    let data_url = format!("data:{};base64,{}", payload.mime, payload.base64);
    match dialect {
        ImageDialect::Chat | ImageDialect::Responses => json!({
            "type": "video_url",
            "video_url": { "url": data_url },
        }),
        ImageDialect::Gemini => json!({
            "inline_data": { "mime_type": payload.mime, "data": payload.base64 },
        }),
        ImageDialect::Anthropic => {
            text_block(dialect, "（视频没发出去）Anthropic 线不收视频输入。")
        }
    }
}

/// 这一发按模型能力放行哪些媒体本体。是否收是模型表那一行的属性；方言级缺口
/// （anthropic 不收音视频）在投影里就地说明，不劳 outcome 再管一遍
#[derive(Clone, Copy)]
pub(crate) struct ModalInputs {
    pub(crate) images: bool,
    pub(crate) audios: bool,
    pub(crate) videos: bool,
}

/// 按当前实发模型（池/路由换人后的那一个）的模型表行读三类收件能力。
/// 表里没这一行就是没收过这个证据——按不发处理，赌服务商会 400 不如先守住
pub(crate) fn modal_inputs_of(config: &AppConfig) -> ModalInputs {
    ModalInputs {
        images: config.takes_images(),
        audios: config.takes_audio(),
        videos: config.takes_video(),
    }
}

/// 一行的 content → 某一家的出站形状。**没有媒体的行原样返回**，所以纯文本请求的字节
/// 与加这一格之前逐字相同（前缀缓存按字节匹配，动一下就整段作废）。
/// 被挡下的媒体不是静默丢掉：它换成一句写在正文里的话——模型不知道自己被给了一个看不见
/// 的东西时，会照着"用户发了张图/发了段音频"那句话把内容编出来
pub(crate) fn project_content(
    message: &Value,
    dialect: ImageDialect,
    inputs: ModalInputs,
) -> Value {
    let parts = match message.get("content").and_then(Value::as_array) {
        Some(parts)
            if parts
                .iter()
                .any(|part| matches!(part["type"].as_str(), Some("image" | "audio" | "video"))) =>
        {
            parts.clone()
        }
        _ => return message.clone(),
    };
    let mut out: Vec<Value> = Vec::with_capacity(parts.len());
    let mut slot = 0usize;
    for part in &parts {
        match part["type"].as_str() {
            Some("text") => out.push(text_block(
                dialect,
                part["text"].as_str().unwrap_or_default(),
            )),
            Some(kind @ ("image" | "audio" | "video")) => {
                let (limits, model_accepts) = match kind {
                    "image" => (&crate::session::entry::IMAGE_LIMITS, inputs.images),
                    "audio" => (&crate::session::entry::AUDIO_LIMITS, inputs.audios),
                    _ => (&crate::session::entry::VIDEO_LIMITS, inputs.videos),
                };
                // 方言级缺口先行：anthropic 两类都不收，直接按被挡处理，
                // 措辞与 outcome 的"档案没勾"那款区分开
                let dialect_gap = match (kind, dialect) {
                    ("audio", ImageDialect::Anthropic) => Some("Anthropic 线不收音频输入。"),
                    ("video", ImageDialect::Anthropic) => Some("Anthropic 线不收视频输入。"),
                    _ => None,
                };
                let outcome = match dialect_gap {
                    Some(why) => crate::session::entry::MediaOutcome::Skipped(why.to_string()),
                    None => crate::session::entry::media_outcome(
                        part["path"].as_str().unwrap_or_default(),
                        part["mime"].as_str().unwrap_or_default(),
                        part["bytes"].as_u64().unwrap_or_default(),
                        model_accepts,
                        slot,
                        limits,
                    ),
                };
                slot += 1;
                match outcome {
                    crate::session::entry::MediaOutcome::Sent(payload) => match kind {
                        "image" => out.push(image_block(dialect, &payload)),
                        "audio" => out.push(audio_block(dialect, &payload)),
                        _ => out.push(video_block(dialect, &payload)),
                    },
                    crate::session::entry::MediaOutcome::Skipped(why) => {
                        let kind_label = match kind {
                            "image" => "图片",
                            "audio" => "音频",
                            _ => "视频",
                        };
                        out.push(text_block(
                            dialect,
                            &format!("（{kind_label}没发出去）{why}"),
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    let mut row = message.clone();
    row["content"] = Value::Array(out);
    row
}
