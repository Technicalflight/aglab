//! 出站 wire 层（优化路线 O1-2 从 chat.rs 拆出的第二大批）。
//!
//! 一个请求从线程数组到服务商字节要过三道：
//! 1. [`content`]：日志里的中性投影 → 某家方言认得的消息块（图片/音频/视频外壳）；
//! 2. [`payload`]：四家各自的 payload 组装（chat / responses / anthropic / gemini）；
//! 3. [`read`]：四家各自的流式 reader 与状态机，外加 SSE 传输（read_events 系）。
//!
//! [`request_round`] 是唯一的方言分派器：调用方只管递线程，选哪家由配置决定。
//! 这一层不碰 `AppHandle`、不碰 `Send` 状态——进去是数据与凭据，出来是回合结果。

mod content;
mod payload;
mod read;

use super::{AppConfig, ChatEvent, RoundFailure, RoundOutcome};
use serde_json::Value;

#[cfg(test)]
pub(crate) use content::{project_content, ImageDialect, ModalInputs};
pub(crate) use payload::ANTHROPIC_VERSION;
#[cfg(test)]
pub(crate) use payload::{
    anthropic_payload, chat_payload, gemini_payload, responses_input, responses_payload,
    responses_tools,
};
pub(crate) use read::provider_gate;
#[cfg(test)]
pub(crate) use read::{
    affinity_headers, apply_anthropic_event, apply_chat_event, apply_gemini_event,
    apply_responses_event, partial_of, read_events, retry_429_delay, ChatState, ResponsesState,
    StreamItem,
};
pub(crate) use read::{
    describe_status, is_pool_swappable_error, read_anthropic_round, read_chat_round,
    read_gemini_round, read_responses_round, DeltaCoalescer, EgressFail, ENHANCE_SYSTEM,
    RETRYABLE_STATUS_WORD,
};

pub(crate) fn request_round(
    config: &AppConfig,
    key: &str,
    thread: &[Value],
    declared: &[Value],
    cache_key: Option<&str>,
    stop: &std::sync::atomic::AtomicBool,
    emit: &mut dyn FnMut(ChatEvent),
) -> Result<RoundOutcome, RoundFailure> {
    if config.uses_anthropic() {
        read_anthropic_round(config, key, thread, declared, stop, emit)
    } else if config.uses_responses() {
        read_responses_round(config, key, thread, declared, cache_key, stop, emit)
    } else if config.uses_gemini() {
        read_gemini_round(config, key, thread, declared, stop, emit)
    } else {
        read_chat_round(config, key, thread, declared, cache_key, stop, emit)
    }
}
