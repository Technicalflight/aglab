//! 请求 wire 与台账条目的构造件（优化路线 O1-1：chat.rs 拆分第一批）。
//!
//! 这一层的共同点：**纯函数**——不拿 `AppHandle`、不碰 `Send` 状态，进去是数据，
//! 出来是数据。它们是"界面上那条"与"模型见过那条"同源纪律的执行点：
//! [`tool_result_pair`] 是工具结果唯一的出站口，[`settle_failed`] 是半截回合的
//! 落账形制，[`clamp_tool_result`] / [`pack_tool_result`] 是工具结果的长度闸。
//!
//! 守卫测试仍住在 `chat.rs` 的 wire_format_tests 里（它们同时看着流式管线），
//! 拆分只搬构造件本体。

use super::{ChatEvent, RoundOutcome, ToolCallBuffer, ToolStatus};
use crate::session::entry::{Message, SettledAssistant, StopReason};

/// 一次没跑完的请求该落成什么样。半截正文要落，**半截工具调用不落成调用**：
/// 参数可能正好断在 JSON 中间，落成带 tool_calls 的行就永远欠一个工具结果（F9/I7）
pub(super) fn settle_failed(
    partial: &RoundOutcome,
    stop: StopReason,
    error: Option<&str>,
) -> Option<Message> {
    if partial.text.is_empty() {
        return None;
    }
    Some(Message::Assistant(SettledAssistant {
        content: partial.text.clone(),
        tool_calls: Vec::new(),
        stop,
        reasoning: partial.reasoning.clone(),
        error: error.map(str::to_string),
        thinking_signature: partial.reasoning_signature.clone(),
        reasoning_items_json: partial.reasoning_items_json.clone(),
    }))
}

/// 工具调用的唯一出站形制。回合内与历史重放共用它，这样"同一件事只有一种字节表示"。
/// 把重放进来的 tool_calls 收成服务商认的嵌套形。
/// 前端台账是扁平 `{id,name,arguments}`（外加 status/risk/output 等界面字段），
/// 实测扁平形会被 chat 服务商直接 400 拒掉；已经是嵌套形则原样重构，因此幂等。
/// UI 字段一律不上 wire：它们进历史会把同一轮对话编成两种字节。
/// 工具结果唯一的出口：一次同时构造"给界面的事件"和"给模型的 tool 消息"，
/// 两者共用同一个 `text`。
///
/// 分头写文案是这里漂移过的唯一原因：界面拿到第二人称那句、台账把它存进
/// `toolCalls[].output`，下一轮按台账回放历史时，模型收到的就不是它上一轮见过的
/// 字节——前缀从那条消息处断掉，而且模型还看到一句它从没说过的话。
/// 所以宁可让工具卡片显示模型口径的句子，也不留两份真相
///
/// `pass_reason` 是这件事唯一的例外，也正是它的边界：它只上事件、不上 `Message`。
/// 界面字段一旦混进台账，下一轮回放时模型收到的就不是它上一轮见过的字节
pub(super) fn tool_result_pair(
    call: &ToolCallBuffer,
    status: ToolStatus,
    risk: &str,
    input: String,
    text: String,
    pass_reason: Option<String>,
) -> (ChatEvent, Message) {
    // 敏感保护（design-security-center.md D6）：工具结果进话题流的**唯一**出口在这里。
    // 读盘读出来的凭据在进入历史之前就地打码——打码发生在第一次发出之前，
    // 实发体与存档从头到尾是同一份（缓存前缀一致性不受影响）；界面事件与给模型的
    // tool 消息读同一份打码后的文本，同源纪律不破
    let text = crate::secrets::mask_for_thread(&text);
    let event = ChatEvent::Tool {
        id: call.id.clone(),
        name: call.name.clone(),
        status,
        risk: risk.into(),
        input,
        output: Some(text.clone()),
        arguments: Some(call.arguments.clone()),
        pass_reason,
        content_chars: Some(call.content_chars),
    };
    let message = Message::Tool {
        tool_call_id: call.id.clone(),
        content: text,
    };
    (event, message)
}

/// 工具结果的长度闸：超长只留头尾，中间标注省略。
/// 真正影响下一步决策的几乎总在开头（状态/报错）和结尾（汇总），中间大段原文不再每轮重放。
///
/// 头尾各留多少**不是**旋钮：由 `max` 派生（头 3/4、尾 1/8）。拆成三个数就会出现
/// `head + tail > max` 这种截完比不截还长的配置（§14）。`max = 0` = 不设上限
pub(super) fn clamp_tool_result(text: &str, max: usize) -> String {
    pack_tool_result(text, max, None)
}

/// 带 handle 的版本：省略段不再是死信息——原文归档在观察存档里，中间那段要用
/// 的时候调 obs_recall 按需取回（SoL-Pi 的 ObservationPack 精神：句柄 + 分页召回）。
/// handle = None 保持旧的纯截断文案：界面那一侧没有"调用工具"的能力
pub(super) fn pack_tool_result(text: &str, max: usize, handle: Option<&str>) -> String {
    let count = text.chars().count();
    if max == 0 || count <= max {
        return text.to_string();
    }
    let head_len: usize = max * 3 / 4;
    let tail_len: usize = max / 8;
    let head: String = text.chars().take(head_len).collect();
    let tail: String = text.chars().skip(count - tail_len).collect();
    let note = match handle {
        Some(id) => format!(
            "。原文已归档为观察 #{id}：调 obs_recall（handle=\"{id}\"，start=起始字符位，limit=字符数）可分页取回任意段落"
        ),
        None => "，原文过长已截断".to_string(),
    };
    format!(
        "{head}\n\n……（中间省略约 {} 字符{note}）……\n\n{tail}",
        count - head_len - tail_len
    )
}
