//! 话题条目：日志里存的东西，以及它在类型上不允许存的东西。
//!
//! 三条决定形状的规则都在这里，其余模块只是消费它们：
//!
//! 1. **身份三件套由存储层铸造**（`id`/`parent_id`/`seq`/`timestamp`）。调用方只能交出一个
//!    [`EntryPayload`]，所以在 Rust 里**根本命名不出**"自己填了 seq 的条目"——这比参照实现里
//!    `NewEntry = Omit<Entry, "seq" | "timestamp">` 更硬，因为那边这些字段仍然存在、只是类型上
//!    被去掉了，而这边它们不在输入类型上。
//! 2. **未落定的 assistant 回答不是 [`Message`]**。在途正文的类型是 [`PendingAssistant`]，它进
//!    日志的唯一路径是 [`PendingAssistant::settle`]。参照实现要用运行时检查在每个写门上再兜一次
//!    （因为 TS 里能手写字面量），这里不需要：[`StopReason`] 没有 `Pending` 这一档。
//! 3. **压缩与分支摘要不是消息**，是顶层条目（[`EntryPayload::Compaction`] 等），所以
//!    [`Message`] 装不出它们——想绕过投影直接塞一条"摘要"进历史，在这个类型上写不出来。

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// 落定的那一档。注意：**没有** `Pending` 变体，这就是规则 2 的落点。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// 正常说完
    Stop,
    /// 撞到输出上限，话没说完
    Length,
    /// 以工具调用收尾
    ToolUse,
    /// 用户按了停止
    Aborted,
    /// 流出错收尾
    Error,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// 一次工具调用。`arguments` 存原始 JSON 文本而不是结构：服务商要的就是文本，解析失败时
/// 也要能逐字节重放出模型上一轮见过的那串字。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
    /// 界面专用：这一发声明时，本轮正文已经流出了多少个 **UTF-16 码元**——
    /// 前端按它把正文切片、把工具行插回原文流。wire 重建只取 id/name/arguments
    /// （下面 function 块的白名单），这一格到不了模型
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_chars: Option<u32>,
}

/// 一条已经落定的 assistant 回答
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SettledAssistant {
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    pub stop: StopReason,
    /// 思维链与错误文案是**界面字段**：`to_wire` 不带它们，服务商也从没收到过（旧协议里就
    /// 没有这两个键）。留在日志里是为了迁移老话题时不丢用户看过的东西（真数据里 98 条消息
    /// 有 40 条带思维链、6 条带错误文案）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 思考回放凭据（Anthropic 线）：服务商随思考块发回的签名。`store:false` 式的无状态
    /// 重放里，开思考的模型要求把 thinking 块按原样带回去——签名对不上就 400。
    /// 与 `reasoning` 文本配对出现；没有签名的思考（老话题）不回放
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_signature: Option<String>,
    /// 思考回放凭据（Responses 线）：服务商发回的 reasoning 输出项原样 JSON 数组。
    /// OpenAI 按 id 把 rs_xxx 与 fc_xxx 配对，缺了就 400；Azure 会在终态里才给
    /// encrypted_content，由解析层回填后再落库
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_items_json: Option<String>,
}

/// 在途的半截回答：只能读、只能继续喂，**进不了日志**
#[derive(Clone, Debug, Default)]
pub struct PendingAssistant {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
}

impl PendingAssistant {
    /// 落定的唯一出口。停止与出错也走这里——用户已经在界面上读过的那半截必须留在日志里，
    /// 否则下一轮模型会以为那句话从没说过
    pub fn settle(self, stop: StopReason) -> SettledAssistant {
        SettledAssistant {
            content: self.content,
            tool_calls: self.tool_calls,
            stop,
            reasoning: None,
            error: None,
            thinking_signature: None,
            reasoning_items_json: None,
        }
    }
}

/// 一条用户消息带的一张图。**日志里只存引用**：base64 进 jsonl 就是几百 KB 的膨胀，
/// 而那份字节每一发请求都能从盘上重新读出来。字节数留在这里是必需的——图片不计入
/// 正文字符，用量与压缩那两本账要靠它才知道这一行有多重
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageRef {
    pub path: String,
    pub mime: String,
    pub bytes: u64,
}

/// 一张图最多带多少字节。base64 后是 4/3，再大的图先把整场对话的前缀挤掉才轮到它值钱
pub const IMAGE_MAX_BYTES: u64 = 4 * 1024 * 1024;
/// 一条消息最多带几张图。超出来的不是"压一压还能塞"，是直接不发并说清为什么
pub const IMAGE_MAX_COUNT: usize = 8;
/// 音频的本体上限：OpenAI 的 input_audio 25MB、Gemini inline 整请求 20MB——取严的那个
pub const AUDIO_MAX_BYTES: u64 = 20 * 1024 * 1024;
pub const AUDIO_MAX_COUNT: usize = 4;
/// 视频的本体上限与生成管线的素材同款（30MB）：再大就该走文件路径而不是走上下文
pub const VIDEO_MAX_BYTES: u64 = 30 * 1024 * 1024;
pub const VIDEO_MAX_COUNT: usize = 2;

/// 音频/视频附件与 ImageRef 同形（路径 + mime + 字节数）。别名只为读起来不撒谎
pub type MediaRef = ImageRef;

/// 真的发出去时的那一串字节
pub struct MediaPayload {
    pub mime: String,
    pub base64: String,
}

/// 一段媒体在这一发的下场。`Skipped` 的文案要进正文——模型得知道自己被给了一个它看不见的东西，
/// 否则它会照着"用户发了张图/发了段音频"那句话把内容编出来
pub enum MediaOutcome {
    Sent(MediaPayload),
    Skipped(String),
}

/// 一类媒体的长相与门槛：文案里怎么称呼它、计数用哪个量词、单条多大、一条消息几条
pub struct MediaLimits {
    /// 附件在文案里的称呼（"这张图"/"这段音频"/"这段视频"）
    pub label: &'static str,
    /// 计数用的量词名词（"张图"/"段音频"/"段视频"）——"最多带 8 张图"读得通，
    /// "最多带 8 段这张图"读不通，所以两个词都要有
    pub noun: &'static str,
    /// 档案里对应能力开关的叫法（"收图片"/"收音频"/"收视频"）
    pub toggle: &'static str,
    pub max_bytes: u64,
    pub max_count: usize,
}

pub const IMAGE_LIMITS: MediaLimits = MediaLimits {
    label: "这张图",
    noun: "张图",
    toggle: "收图片",
    max_bytes: IMAGE_MAX_BYTES,
    max_count: IMAGE_MAX_COUNT,
};
pub const AUDIO_LIMITS: MediaLimits = MediaLimits {
    label: "这段音频",
    noun: "段音频",
    toggle: "收音频",
    max_bytes: AUDIO_MAX_BYTES,
    max_count: AUDIO_MAX_COUNT,
};
pub const VIDEO_LIMITS: MediaLimits = MediaLimits {
    label: "这段视频",
    noun: "段视频",
    toggle: "收视频",
    max_bytes: VIDEO_MAX_BYTES,
    max_count: VIDEO_MAX_COUNT,
};

/// 这一段媒体能不能发、发的是哪一串。四家的外壳不同，但"能不能发"与"字节从哪来"是
/// 同一件事，所以判定和解码只做一次，在这里。方言级的缺口（anthropic 不收音视频）
/// 在投影里挡，不在这里——那句"为什么没发"两家措辞不同
pub fn media_outcome(
    path: &str,
    mime: &str,
    bytes: u64,
    model_accepts: bool,
    slot: usize,
    limits: &MediaLimits,
) -> MediaOutcome {
    if !model_accepts {
        return MediaOutcome::Skipped(format!(
            "这个模型不收{}（服务商档案里没勾「{}」），本体没发出去。\
             文件在 {path}；要处理它请用 run_command（复制/压缩/转格式），要看内容请让用户描述。",
            limits.label, limits.toggle
        ));
    }
    if slot >= limits.max_count {
        return MediaOutcome::Skipped(format!(
            "一条消息最多带 {} {}，这是第 {} {}，没发出去。",
            limits.max_count,
            limits.noun,
            slot + 1,
            limits.noun
        ));
    }
    if bytes > limits.max_bytes {
        return MediaOutcome::Skipped(format!(
            "{} {:.1} MB，超过 {} MB 的上限，没发出去。",
            limits.label,
            bytes as f64 / 1_048_576.0,
            limits.max_bytes / 1_048_576
        ));
    }
    let Ok(raw) = std::fs::read(path) else {
        return MediaOutcome::Skipped(format!(
            "{}已经不在了（{path}），没发出去。",
            limits.label
        ));
    };
    // 盘上的真实大小说了算：日志里那个 bytes 是当时记的，文件被人换过就以现状为准
    if raw.len() as u64 > limits.max_bytes {
        return MediaOutcome::Skipped(format!("{}比上限还大，没发出去。", limits.label));
    }
    use base64::Engine as _;
    MediaOutcome::Sent(MediaPayload {
        mime: mime.to_string(),
        base64: base64::engine::general_purpose::STANDARD.encode(&raw),
    })
}

/// 一条消息里"给模型看的文字"。数组形（带图的行）与字符串形（今天绝大多数行）都认：
/// 只认 `as_str()` 的那些调用点遇到数组会把整行读成空串，于是压缩与摘要会**当这条消息不存在**
pub fn content_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|part| match part.get("type").and_then(Value::as_str) {
                Some("text") => part
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                // 摘要模型同样看不见这些媒体，但要知道这里有过东西，
                // 否则它会以为用户什么也没发
                Some("image") => "［一张图片］".to_string(),
                Some("audio") => "［一段音频］".to_string(),
                Some("video") => "［一段视频］".to_string(),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// 这一行发出去要占多少字符。图片按 base64 后的量算（4/3 再加外壳），不按路径长度算——
/// 路径只有几十字节，按它记账等于告诉压缩器"这一行很轻"，于是压得太晚
pub fn content_chars(message: &Value) -> usize {
    match message.get("content") {
        Some(Value::String(text)) => text.chars().count(),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|part| match part.get("type").and_then(Value::as_str) {
                Some("text") => part
                    .get("text")
                    .and_then(Value::as_str)
                    .map(|text| text.chars().count())
                    .unwrap_or_default(),
                Some("image") => {
                    let bytes = part.get("bytes").and_then(Value::as_u64).unwrap_or_default();
                    (bytes as usize).div_ceil(3) * 4
                }
                // 音视频按同一口径记账：base64 后 4/3。按路径长度算等于告诉压缩器
                // "这一行很轻"，于是压得太晚
                Some("audio") | Some("video") => {
                    let bytes = part.get("bytes").and_then(Value::as_u64).unwrap_or_default();
                    (bytes as usize).div_ceil(3) * 4
                }
                _ => 0,
            })
            .sum(),
        _ => 0,
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: String,
        /// 这一行带的图。空 = 纯文本行，出站形状与加这一格之前逐字相同
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageRef>,
        /// 这一行带的音频/视频（多模态识别）。同样只记"用户给了什么"，能不能发由
        /// 出站投影按当次模型的能力决定
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        audios: Vec<MediaRef>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        videos: Vec<MediaRef>,
    },
    Assistant(SettledAssistant),
    Tool {
        tool_call_id: String,
        content: String,
    },
}

impl Message {
    pub fn role(&self) -> Role {
        match self {
            Self::System { .. } => Role::System,
            Self::User { .. } => Role::User,
            Self::Assistant(..) => Role::Assistant,
            Self::Tool { .. } => Role::Tool,
        }
    }

    /// 换正文，保留角色与配对信息——`context_edit` 的替换半边就是这个动作
    pub fn with_content(&self, content: &str) -> Self {
        match self {
            Self::System { .. } => Self::System {
                content: content.to_string(),
            },
            Self::User {
                images,
                audios,
                videos,
                ..
            } => Self::User {
                content: content.to_string(),
                // 换正文不能把附件丢了：`context_edit` 替换的只是文字，媒体还在那一发里
                images: images.clone(),
                audios: audios.clone(),
                videos: videos.clone(),
            },
            Self::Assistant(settled) => Self::Assistant(SettledAssistant {
                content: content.to_string(),
                ..settled.clone()
            }),
            Self::Tool { tool_call_id, .. } => Self::Tool {
                tool_call_id: tool_call_id.clone(),
                content: content.to_string(),
            },
        }
    }

    /// 发到服务商的那一串字节。工具调用一律是嵌套形 `{id,type,function{}}`：
    /// 实测扁平形会被 chat 服务商直接 400 拒掉，而这条路径是唯一的出站口。
    ///
    /// 思考回放凭据是**条件字段**：只有服务商真发回过签名/reasoning 项的回合才带——
    /// chat 线在 payload 组装时剥掉它们（DeepSeek 不收 reasoning），responses 与
    /// anthropic 线消费它们。没有凭据的行与历史字节逐字相同
    pub fn to_wire(&self) -> Value {
        match self {
            Self::System { content } => json!({ "role": "system", "content": content }),
            // 没带媒体就是今天那一行，逐字不变；带了才换成数组形。
            // 这里的 image/audio/video 部分是**中性形**（只有路径与字节），四家外壳在
            // 各自的 payload 里拼——判定与解码走 `media_outcome`，只此一处
            Self::User {
                content,
                images,
                audios,
                videos,
            } => {
                if images.is_empty() && audios.is_empty() && videos.is_empty() {
                    return json!({ "role": "user", "content": content });
                }
                let mut parts: Vec<Value> = Vec::new();
                if !content.trim().is_empty() {
                    parts.push(json!({ "type": "text", "text": content }));
                }
                parts.extend(images.iter().map(|image| {
                    json!({
                        "type": "image",
                        "path": image.path,
                        "mime": image.mime,
                        "bytes": image.bytes,
                    })
                }));
                parts.extend(audios.iter().map(|audio| {
                    json!({
                        "type": "audio",
                        "path": audio.path,
                        "mime": audio.mime,
                        "bytes": audio.bytes,
                    })
                }));
                parts.extend(videos.iter().map(|video| {
                    json!({
                        "type": "video",
                        "path": video.path,
                        "mime": video.mime,
                        "bytes": video.bytes,
                    })
                }));
                json!({ "role": "user", "content": parts })
            }
            Self::Assistant(settled) => {
                let mut row = if settled.tool_calls.is_empty() {
                    json!({ "role": "assistant", "content": settled.content })
                } else {
                    json!({
                        "role": "assistant",
                        "content": settled.content,
                        "tool_calls": settled.tool_calls.iter().map(|call| json!({
                            "id": call.id,
                            "type": "function",
                            "function": { "name": call.name, "arguments": call.arguments },
                        })).collect::<Vec<_>>(),
                    })
                };
                // 思考文本只有配上签名才有出站的资格：签名是服务商验证回放的凭据
                if let Some(signature) = &settled.thinking_signature {
                    if let Some(text) = &settled.reasoning {
                        row["thinking"] = json!(text);
                        row["thinking_signature"] = json!(signature);
                    }
                }
                if let Some(items) = &settled.reasoning_items_json {
                    if let Ok(parsed) = serde_json::from_str::<Value>(items) {
                        if parsed.as_array().is_some_and(|items| !items.is_empty()) {
                            row["reasoning_items"] = parsed;
                        }
                    }
                }
                row
            }
            Self::Tool {
                tool_call_id,
                content,
            } => json!({
                "role": "tool",
                "tool_call_id": tool_call_id,
                "content": content,
            }),
        }
    }

    /// 这条消息发出去的那串字节有多长。层的统计与"投影没带上它"的统计共用这一个口径：
    /// 两处各算一遍，Inspector 报的量就跟实发的对不上账
    pub fn wire_chars(&self) -> usize {
        serde_json::to_string(&self.to_wire())
            .map(|text| text.chars().count())
            .unwrap_or(0)
    }
}

/// 一次请求的用量。`cached_tokens` 用 `Option` 是有意的：服务商不上报和上报了 0 是两件事，
/// 把前者读成后者会让不支持缓存的服务商显示成"命中 0%"，看着像前缀被改坏了
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UsageRecord {
    pub input_tokens: u32,
    pub output_tokens: u32,
    #[serde(default)]
    pub cached_tokens: Option<u32>,
    #[serde(default)]
    pub cache_write_tokens: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EntryPayload {
    Message {
        message: Message,
    },
    /// 压缩边界。保留的是**真条目**（`first_kept_entry_id` 起）而不是一段拍平的文本，
    /// 所以尾巴不需要重新序列化；`system_message` 是边界那一刻的 system 快照，
    /// 有了它，"压缩之后重建出当时那份前缀"才是可恢复的
    Compaction {
        summary: String,
        first_kept_entry_id: String,
        tokens_before: u32,
        #[serde(default)]
        usage: Option<UsageRecord>,
        #[serde(default)]
        system_message: Option<Message>,
    },
    /// 只在投影里生效的编辑。`replacement: None` 是撤回（那条消息从上下文里消失），
    /// 原始条目一条不改——日志是事实，编辑是后补的事实，不能互相覆盖
    ContextEdit {
        target_id: String,
        replacement: Option<String>,
    },
    /// 回溯到更早的点时，对被放弃那一段的摘要；或者按层压缩顶替掉历史里某一段的那份摘要。
    ///
    /// 两个 id 都在当前路径上、且 `from_id` 不晚于 `through_id` 时，它是一条**段边界**：
    /// 那一段（含两端）从投影里消失，摘要站在 `from_id` 原来的位置上——`from_id` 之前的字节
    /// 一个都不动，这正是它与 [`Self::Compaction`] 的分工（压缩从数组头部断开，这一段从中间换）。
    /// 两个 id 有一个落不到当前路径上（比如分叉带来的那一份），它就只是一行摘要，不顶替任何东西
    BranchSummary {
        from_id: Option<String>,
        #[serde(default)]
        through_id: Option<String>,
        summary: String,
        #[serde(default)]
        usage: Option<UsageRecord>,
    },
    /// 三方注入的内容（钩子补的上下文、notice）。`display` 只管界面上显不显示，
    /// 不影响模型是否看到——反过来说，想让模型看不到就得换一种条目，不能靠这个位
    CustomMessage {
        custom_type: String,
        content: String,
        display: bool,
    },
    ModelChange {
        provider: String,
        model_id: String,
    },
    Usage {
        kind: String,
        provider: String,
        model: String,
        usage: UsageRecord,
        #[serde(default)]
        note: Option<String>,
    },
    SessionInfo {
        name: Option<String>,
    },
    Custom {
        custom_type: String,
        #[serde(default)]
        data: Option<Value>,
    },
}

impl EntryPayload {
    /// 这条目能不能当 `context_edit` 的目标。只有承载正文的行可以被改写或撤回
    pub fn editable(&self) -> bool {
        match self {
            Self::Message { message } => !matches!(message, Message::System { .. }),
            Self::CustomMessage { .. } => true,
            Self::Compaction { .. }
            | Self::BranchSummary { .. }
            | Self::ContextEdit { .. }
            | Self::ModelChange { .. }
            | Self::Usage { .. }
            | Self::SessionInfo { .. }
            | Self::Custom { .. } => false,
        }
    }

    /// 这条目能不能被**撤回**（`replacement: None`）。边界行不承载可以改写的正文，但撤掉它
    /// 正是"撤销一次压缩"那件事：边界一撤，被它顶替掉的那些条目原样回来，历史一行都没删
    pub fn revocable(&self) -> bool {
        matches!(self, Self::Compaction { .. } | Self::BranchSummary { .. })
    }
}

/// 追加进日志的唯一输入。它**只有** payload——身份三件套不在这个类型上，见模块头规则 1
#[derive(Clone, Debug)]
pub struct NewEntry {
    pub payload: EntryPayload,
    /// 这一发的实发模型名。assistant 行随账带走它：投影恢复"这句是谁答的"读的是这里——
    /// 台账那一格（done 事件贴回）在中途重启/崩溃时会停在旧一拍，认领就落了空
    /// （真机踩过：重启后切换会话，脚注全成"—"）。None = 非 assistant 行/老日志
    pub model: Option<String>,
}

impl NewEntry {
    pub fn new(payload: EntryPayload) -> Self {
        Self { payload, model: None }
    }

    /// assistant 行用：把这一发真正用的模型名随条目落账
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Entry {
    pub id: String,
    pub parent_id: Option<String>,
    pub seq: u64,
    pub timestamp: i64,
    /// 这一发的实发模型名（assistant 行）。键名刻意避开 `model`——payload 的 Usage
    /// 变体自带一个 `model` 字段，flatten 之后外层同名键会把它的吃掉，反序列化必炸。
    /// 老日志没有这个键：serde default 落 None
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "sent_model"
    )]
    sent_model: Option<String>,
    #[serde(flatten)]
    payload: EntryPayload,
}

impl Entry {
    /// 只有 `session` 模块内部能铸造带 seq 的条目
    pub(super) fn assemble(
        id: String,
        parent_id: Option<String>,
        seq: u64,
        timestamp: i64,
        model: Option<String>,
        payload: EntryPayload,
    ) -> Self {
        Self {
            id,
            parent_id,
            seq,
            timestamp,
            sent_model: model,
            payload,
        }
    }

    pub fn payload(&self) -> &EntryPayload {
        &self.payload
    }

    pub fn model(&self) -> Option<&str> {
        self.sent_model.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 没带图的 user 行：出站形状与加 `images` 之前**逐字相同**。服务商缓存按前缀字节
    /// 匹配，纯文本话题要是因此多一个键或换个形状，整场对话的命中率就没了
    #[test]
    fn a_text_only_user_row_is_untouched_by_the_image_field() {
        let row = Message::User {
            content: "就一句话".into(),
            images: Vec::new(),
            audios: Vec::new(),
            videos: Vec::new(),
        };
        assert_eq!(row.to_wire(), json!({ "role": "user", "content": "就一句话" }));
        assert_eq!(
            serde_json::to_string(&row).unwrap(),
            r#"{"role":"user","content":"就一句话"}"#,
            "空 images 不进日志：老文件的每一行都不该因为加了这个字段而改写"
        );
    }

    /// 带图的行只多一个 `images` 键；老日志里没这个键的行照旧读得回来
    #[test]
    fn an_image_row_round_trips_and_old_rows_still_load() {
        let row = Message::User {
            content: "看这张图".into(),
            images: vec![ImageRef {
                path: "C:/x/shot.png".into(),
                mime: "image/png".into(),
                bytes: 1234,
            }],
            audios: Vec::new(),
            videos: Vec::new(),
        };
        let text = serde_json::to_string(&row).unwrap();
        assert_eq!(serde_json::from_str::<Message>(&text).unwrap(), row);
        assert_eq!(
            serde_json::from_str::<Message>(r#"{"role":"user","content":"老行"}"#).unwrap(),
            Message::User {
                content: "老行".into(),
                images: Vec::new(),
                audios: Vec::new(),
                videos: Vec::new(),
            }
        );
    }

    /// 图片按 base64 后的量记账，不按路径长度：路径几十字节，按它算等于告诉压缩器
    /// "这一行很轻"，于是压得太晚直接爆窗口
    #[test]
    fn content_accounting_sees_through_the_array_shape() {
        let with_image = json!({
            "role": "user",
            "content": [
                { "type": "text", "text": "看这张图" },
                { "type": "image", "path": "C:/x/shot.png", "mime": "image/png", "bytes": 3000 },
            ],
        });
        assert_eq!(content_text(&with_image), "看这张图\n［一张图片］");
        // 4 个文字 + 3000 字节按 4/3 进位成 4000
        assert_eq!(content_chars(&with_image), 4 + 4000, "图片要按 base64 后的量入账");
        // 字符串形（绝大多数行）口径不变
        assert_eq!(content_chars(&json!({ "content": "abc" })), 3);
    }

    /// `context_edit` 换的是文字，图还在那一发里：丢了图就等于把用户给过的东西弄不见
    #[test]
    fn replacing_the_text_keeps_the_images() {
        let row = Message::User {
            content: "原文".into(),
            images: vec![ImageRef {
                path: "C:/x/shot.png".into(),
                mime: "image/png".into(),
                bytes: 10,
            }],
            audios: vec![MediaRef {
                path: "C:/x/clip.mp3".into(),
                mime: "audio/mpeg".into(),
                bytes: 20,
            }],
            videos: Vec::new(),
        };
        let edited = row.with_content("改过的");
        let Message::User {
            content,
            images,
            audios,
            ..
        } = &edited
        else {
            panic!("改完还是 user 行");
        };
        assert_eq!(content, "改过的");
        assert_eq!(images.len(), 1, "引用不能跟着正文一起被换掉");
        assert_eq!(audios.len(), 1, "音视频引用同图：换了正文还在");
    }
}
