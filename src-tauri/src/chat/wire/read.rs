use super::super::{ChatEvent, RoundFailure, RoundOutcome, Usage};
use super::payload::{
    anthropic_payload, chat_payload, gemini_payload, responses_payload, ANTHROPIC_VERSION,
};
use crate::chat::{stopped, ToolCallBuffer, STOP_MARK};
use crate::config::AppConfig;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::time::{Duration, Instant};

/// Gemini 的流式状态与 chat/anthropic 形状相同，直接复用 [`ChatState`]。
/// Gemini 的 functionCall 没有 id：按 parts 顺序合成 `call_{index}`，
/// 回放时工具结果按同一把 id 反查函数名（见 [`gemini_system_and_contents`]）
pub(crate) fn apply_gemini_event(
    state: &mut ChatState,
    chunk: &Value,
    emit: &mut dyn FnMut(ChatEvent),
) {
    if let Some(message) = chunk["error"]["message"].as_str() {
        state.failure = Some(format!("服务商报错：{message}"));
        return;
    }
    if let Some(next) = Usage::from_gemini(&chunk["usageMetadata"]) {
        // usageMetadata 是累计读数：每个 chunk 覆盖前值就是"最后一次"的口径
        state.usage = Some(next);
    }
    let candidate = &chunk["candidates"][0];
    if let Some(parts) = candidate["content"]["parts"].as_array() {
        for part in parts {
            if part["thought"].as_bool() == Some(true) {
                if let Some(piece) = part["text"].as_str() {
                    state.reasoning.push_str(piece);
                    emit(ChatEvent::Reasoning {
                        text: piece.to_string(),
                    });
                }
                continue;
            }
            if let Some(piece) = part["text"].as_str() {
                state.text.push_str(piece);
                emit(ChatEvent::Delta {
                    text: piece.to_string(),
                });
            }
            if let Some(call) = part["functionCall"].as_object() {
                let index = state.calls.len();
                let slot = state.calls.entry(index).or_default();
                if slot.id.is_empty() {
                    slot.id = format!("call_{index}");
                }
                if slot.name.is_empty() {
                    slot.name = call["name"].as_str().unwrap_or_default().to_string();
                }
                if call["args"].is_object() {
                    slot.arguments =
                        serde_json::to_string(&call["args"]).unwrap_or_else(|_| "{}".to_string());
                }
            }
        }
    }
    if let Some("MAX_TOKENS") = candidate["finishReason"].as_str() {
        state.truncated = true
    }
}

/// Gemini 的 round：服务商 `?alt=sse`、鉴权 `x-goog-api-key`、正文 generateContent。
/// 频内缓存没有显式开关，cache_key 在这条线不落地
pub(crate) fn read_gemini_round(
    config: &AppConfig,
    key: &str,
    thread: &[Value],
    declared: &[Value],
    stop: &std::sync::atomic::AtomicBool,
    emit: &mut dyn FnMut(ChatEvent),
) -> Result<RoundOutcome, RoundFailure> {
    let mut state = ChatState::default();

    let headers = vec![("x-goog-api-key", key.to_string())];
    let payload = gemini_payload(config, thread, declared);
    state.sent_chars = crate::session::layers::chars_of(&payload);
    // 模型对账：payload 里实际写出的 model 字段。gemini 的 model 在 URL 上，
    // body 里没有——回落 config.model，那本来就是这一发的实发值
    state.sent_model = payload
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| config.model.clone());
    emit(ChatEvent::Probe {
        key: "payload".into(),
        detail: format!("JSON · {:.1} KB", payload.to_string().len() as f64 / 1024.0),
        tone: None,
        hint: None,
    });
    let stream = read_events(
        &config.gemini_endpoint(),
        config,
        &headers,
        &payload,
        &config.credential_service,
        stop,
        &mut |item| match item {
            StreamItem::Chunk(chunk) => {
                // 模型对账：每帧先过嗅探器（认出即停），再进线协议状态机
                state.sniffer.feed_chunk(chunk);
                apply_gemini_event(&mut state, chunk, emit);
                Ok(())
            }
            StreamItem::Notice(text) => {
                emit(ChatEvent::Notice { text });
                Ok(())
            }
            StreamItem::Probe {
                key,
                detail,
                tone,
                hint,
            } => {
                emit(ChatEvent::Probe {
                    key,
                    detail,
                    tone,
                    hint,
                });
                Ok(())
            }
        },
    );

    let partial = partial_of(&state);
    if let Err(message) = stream {
        return Err(RoundFailure { message, partial });
    }
    if let Some(reason) = state.failure.clone() {
        return Err(RoundFailure {
            message: reason,
            partial,
        });
    }
    finish_round(
        std::mem::take(&mut state.text),
        std::mem::take(&mut state.reasoning),
        state.reasoning_signature.take(),
        Vec::new(),
        state.usage.take(),
        std::mem::take(&mut state.calls),
        state.truncated,
        state.sent_chars,
        // 模型对账三件套随行：收尾处一并写进 RoundOutcome
        std::mem::take(&mut state.sent_model),
        std::mem::take(&mut state.sniffer),
    )
    .map_err(|message| RoundFailure { message, partial })
}

/// Anthropic 的流式状态与 chat 格式形状相同（正文/思考/用量/工具调用/截断），
/// 直接复用 [`ChatState`]：两套状态机各说各话迟早漂成两份真相
pub(crate) fn apply_anthropic_event(
    state: &mut ChatState,
    chunk: &Value,
    emit: &mut dyn FnMut(ChatEvent),
) {
    let kind = chunk["type"].as_str().unwrap_or_default();
    match kind {
        // 用量在开头就到：流被中途掐断时，input 与缓存命中也已经有数了
        "message_start" => {
            if let Some(next) = Usage::from_anthropic(&chunk["message"]["usage"]) {
                state.usage = Some(next);
            }
        }
        "content_block_start" => {
            let block = &chunk["content_block"];
            if block["type"] == "tool_use" {
                let index = chunk["index"].as_u64().unwrap_or(0) as usize;
                let slot = state.calls.entry(index).or_default();
                if slot.id.is_empty() {
                    if let Some(id) = block["id"].as_str() {
                        slot.id = id.to_string();
                    }
                }
                if slot.name.is_empty() {
                    if let Some(name) = block["name"].as_str() {
                        slot.name = name.to_string();
                    }
                }
            }
        }
        "content_block_delta" => {
            let delta = &chunk["delta"];
            match delta["type"].as_str().unwrap_or_default() {
                "text_delta" => {
                    if let Some(piece) = delta["text"].as_str() {
                        if !piece.is_empty() {
                            state.text.push_str(piece);
                            emit(ChatEvent::Delta { text: piece.into() });
                        }
                    }
                }
                "thinking_delta" => {
                    if let Some(piece) = delta["thinking"].as_str() {
                        if !piece.is_empty() {
                            state.reasoning.push_str(piece);
                            emit(ChatEvent::Reasoning { text: piece.into() });
                        }
                    }
                }
                // 思考块的回放凭据：分块可能来多次，原样拼接。
                // 没有签名的思考下一轮带不回去（Anthropic 校验签名）
                "signature_delta" => {
                    if let Some(piece) = delta["signature"].as_str() {
                        let slot = state.reasoning_signature.get_or_insert_with(String::new);
                        slot.push_str(piece);
                    }
                }
                "input_json_delta" => {
                    let index = chunk["index"].as_u64().unwrap_or(0) as usize;
                    let slot = state.calls.entry(index).or_default();
                    if let Some(piece) = delta["partial_json"].as_str() {
                        slot.arguments.push_str(piece);
                    }
                }
                _ => {}
            }
        }
        "message_delta" => {
            match chunk["delta"]["stop_reason"].as_str() {
                Some("max_tokens") => {
                    state.truncated = true;
                    emit(ChatEvent::Notice {
                        text: "输出长度达到上限，这一轮被截断了。".into(),
                    });
                }
                Some("refusal") => emit(ChatEvent::Notice {
                    text: "内容被服务商的安全策略拦下，这一轮不完整。".into(),
                }),
                _ => {}
            }
            // output_tokens 在这里涨到全量；缓存字段只在 message_start 出现过，
            // 这里不覆盖已拿到的值（防代理把字段置空）
            if let Some(slot) = state.usage.as_mut() {
                if let Some(output) = chunk["usage"]["output_tokens"].as_u64() {
                    slot.output_tokens = output as u32;
                }
            }
        }
        "error" => {
            let reason = chunk["error"]["message"]
                .as_str()
                .or_else(|| chunk["message"].as_str());
            state.failure = Some(match reason {
                Some(text) => format!("服务商报错：{text}"),
                None => "服务商报了一个没有说明的错误。".into(),
            });
        }
        _ => {}
    }
}

pub(crate) fn read_anthropic_round(
    config: &AppConfig,
    key: &str,
    thread: &[Value],
    declared: &[Value],
    stop: &std::sync::atomic::AtomicBool,
    emit: &mut dyn FnMut(ChatEvent),
) -> Result<RoundOutcome, RoundFailure> {
    let mut state = ChatState::default();

    let payload = anthropic_payload(config, thread, declared);
    state.sent_chars = crate::session::layers::chars_of(&payload);
    // 模型对账：payload 里实际写出的 model 字段。gemini 的 model 在 URL 上，
    // body 里没有——回落 config.model，那本来就是这一发的实发值
    state.sent_model = payload
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| config.model.clone());
    emit(ChatEvent::Probe {
        key: "payload".into(),
        detail: format!("JSON · {:.1} KB", payload.to_string().len() as f64 / 1024.0),
        tone: None,
        hint: None,
    });
    // OAuth 订阅令牌（sk-ant-oat01-…）走 Bearer + beta 头：Claude 官方的 OAuth
    // 语义只认这一种鉴权，拿 x-api-key 发它会直接 401
    let anthropic_auth: Vec<(&str, String)> = if key.starts_with("sk-ant-oat01") {
        vec![
            ("authorization", format!("Bearer {key}")),
            ("anthropic-beta", "oauth-2025-04-20".to_string()),
        ]
    } else {
        vec![
            ("x-api-key", key.to_string()),
            ("anthropic-version", ANTHROPIC_VERSION.to_string()),
        ]
    };
    let stream = read_events(
        &config.anthropic_endpoint(),
        config,
        &anthropic_auth,
        &payload,
        &config.credential_service,
        stop,
        &mut |item| match item {
            StreamItem::Chunk(chunk) => {
                // 模型对账：每帧先过嗅探器（认出即停），再进线协议状态机
                state.sniffer.feed_chunk(chunk);
                apply_anthropic_event(&mut state, chunk, emit);
                Ok(())
            }
            StreamItem::Notice(text) => {
                emit(ChatEvent::Notice { text });
                Ok(())
            }
            StreamItem::Probe {
                key,
                detail,
                tone,
                hint,
            } => {
                emit(ChatEvent::Probe {
                    key,
                    detail,
                    tone,
                    hint,
                });
                Ok(())
            }
        },
    );

    // 停止或读挂时 `state` 里已经有内容了：把它跟着错误一起交回去，
    // 否则那半截只活在界面上、日志里没有（F15 的原始形状）
    let partial = partial_of(&state);
    if let Err(message) = stream {
        return Err(RoundFailure { message, partial });
    }
    if let Some(reason) = state.failure.clone() {
        return Err(RoundFailure {
            message: reason,
            partial,
        });
    }
    finish_round(
        std::mem::take(&mut state.text),
        std::mem::take(&mut state.reasoning),
        state.reasoning_signature.take(),
        Vec::new(),
        state.usage.take(),
        std::mem::take(&mut state.calls),
        state.truncated,
        state.sent_chars,
        // 模型对账三件套随行：收尾处一并写进 RoundOutcome
        std::mem::take(&mut state.sent_model),
        std::mem::take(&mut state.sniffer),
    )
    .map_err(|message| RoundFailure { message, partial })
}

// 超时装配搬去了 net.rs：出站 HTTP 的公共件不该住在某个业务模块里，
// 否则别的请求方（OCR/探针/目录）都得反过来依赖聊天引擎
use crate::net::{whole_stream_timeout, with_timeouts};

/// 网关话题亲和（pi 同款）：OpenRouter 这类按请求负载均衡的网关，
/// 同一话题粘到同一上游才谈得上命中它那层的 prompt 缓存。
/// 只对认识的网关发，避免把非标头塞给不相干的中转站
pub(crate) fn affinity_headers(
    base_url: &str,
    cache_key: Option<&str>,
) -> Vec<(&'static str, String)> {
    let Some(identity) = cache_key else {
        return Vec::new();
    };
    if !base_url.to_ascii_lowercase().contains("openrouter.ai") {
        return Vec::new();
    }
    vec![(
        "x-session-id",
        crate::provider::capability::cache_key(identity),
    )]
}

/// 表上那一行 `net.provider` 的执行者：这一发要不要发到推理服务商去。
///
/// 默认档是 `Allow`（`grants` 里那句"今天真实发生的事"），所以这一句落地时一条也不挡；
/// 它新增的是"用户可以把它划成红线"——那一档之下这台应用一次模型请求都不发，连连接都不建立。
/// **Ask 在这一格绑不住**：`read_events` 是传输层，没有"先弹窗再连"的位置；
/// 那一种收紧要的是审批那一套（谁批、批什么指纹），不在这儿假装
pub(crate) fn provider_gate(config: &AppConfig) -> Result<(), String> {
    use crate::policy::{Capability, Level, NetScope};
    let cap = Capability::Net {
        scope: NetScope::Provider,
    };
    match config.active_policy().resolve(&cap) {
        Level::Deny => Err(format!(
            "权限表把 {} 划成了红线：这一发不发出去。要放开去设置 → 权限表改那一行。",
            cap.key()
        )),
        _ => Ok(()),
    }
}

/// 一次出口失败的归因包：`message` 给人看，`outcome` 给代理的账本看。
/// 分在错误种类**还知道**的那一层（发出请求处 / 读流处），出了这一层就只剩字符串了
pub(crate) struct EgressFail {
    pub(crate) message: String,
    pub(crate) outcome: crate::proxy::Outcome,
}

impl EgressFail {
    /// 与代理无关的失败：名单拦截、红线、我们自己的 URL/凭据毛病、前端拒收
    fn neutral(message: String) -> Self {
        Self {
            message,
            outcome: crate::proxy::Outcome::Neutral,
        }
    }
}

/// [`read_events`] 交回调用方的两种东西。合成一个回调而不是两个，是因为调用方那头只有
/// 一份 `emit` 的可变借用：分成"帧回调 + 通知回调"两个闭包，编译器会（正确地）拒绝——
/// "两个闭包同时独占 `*emit`"。把两者塞进同一个 `FnMut` 才是那一份借用的唯一用法
pub(crate) enum StreamItem<'a> {
    /// 服务商吐回来的一帧。回调报错就原样往上抛，这一发算失败
    Chunk(&'a Value),
    /// 传输层自己要说给用户听的一句进度话（429 重试）。它不进日志，也不参与成败：
    /// 无限等待不该是无声的，但一句"正在等"既不是模型说的话，也不该把这一发算成失败
    Notice(String),
    /// 请求链路的阶段探针（载荷序列化/出站链路/首字节…）。不进日志、不参与成败：
    /// 它是界面顶部那条链路动画的数据源，随数据帧走同一条管道省一层回调
    Probe {
        key: String,
        detail: String,
        tone: Option<String>,
        hint: Option<String>,
    },
}

/// SSE 传输层：把 `data:` 行解成 JSON 交给回调，回调报错就原样往上抛。
/// 三条线协议共用，差别只在事件语义与鉴权头（OpenAI 系 Bearer，Anthropic 系 x-api-key）。
/// 每行之间检查停止开关：用户按停止后，最多再读一行就会带着 STOP_MARK 返回。
///
/// 它是这台机器上模型请求**唯一**的 POST 出口，所以出口域名名单（§16）就坐在这儿：
/// 三条线协议共用它，新增第四条协议不会天然漏掉这道闸。
/// 紧挨着它的还有第二道：表上那一行 `net.provider`（[`provider_gate`]）
///
/// 这一层包着两件事：把每一次真实往返的成败回流给**模型池**的账本（`pool::note_*`，
/// 按这一发最后的结局记——一个回合里可能有很多次请求，把回合级失败赖到成员头上
/// 会冤枉好模型；也正因这里是全机唯一的模型 POST 出口，池子的账与线上发生的成败
/// 天然一致），以及把代理的账交给 [`read_events_routed`] 逐条路去记（`proxy::Leg`）
pub(crate) fn read_events(
    url: &str,
    config: &AppConfig,
    headers: &[(&str, String)],
    payload: &Value,
    credential_service: &str,
    stop: &std::sync::atomic::AtomicBool,
    on_item: &mut dyn FnMut(StreamItem) -> Result<(), String>,
) -> Result<(), String> {
    // 429 无限重试（可选开关）：限流说的是"稍后再来"，不是"此路不通"。
    // 开着时按指数退避一直试到成功为止，用户按停止随时可退；
    // 关着时行为与从前一字不差。每次重试经 `Notice` 说一句——
    // 无限等待不该是无声的，用户得知道它卡在哪儿、第几次
    let mut attempt = 0u32;
    loop {
        // 这一层的回调只认帧：通知是传输层自己的话，不经代理的逐条路
        let outcome = read_events_routed(
            url,
            config,
            headers,
            payload,
            credential_service,
            stop,
            on_item,
        );
        match outcome {
            Ok(()) => {
                // 模型池的账看**这一发最后的结局**：换路成功、重试成功的那一发都记成功。
                // 所以限流重试期间一次 `note_failure` 都不记——记了就是把"这一发成了"
                // 说成"这一发败过"，池子会照着那句假话去躲一个其实能用的成员
                crate::pool::note_success();
                return Ok(());
            }
            Err(message) if message == STOP_MARK => return Err(message),
            Err(message) => {
                // 限流的判据与 [`describe_status`] 的 429 那一句同源：
                // 那一层只留得下字符串，这里按字面认领——改文案两边一起改
                // （`the_retry_gate_reads_the_same_429_wording_the_user_sees` 钉这一句）
                let retrying = config.unlimited_retry_429
                    && message.contains(RETRYABLE_STATUS_WORD)
                    && !stopped(stop);
                if !retrying {
                    crate::pool::note_failure();
                    return Err(message);
                }
                attempt += 1;
                let delay = retry_429_delay(attempt);
                let _ = on_item(StreamItem::Notice(format!(
                    "服务商限流（429），{} 秒后第 {attempt} 次重试…（按停止可退出）",
                    delay / 1000
                )));
                if wait_cancellable(stop, delay) {
                    // 用户按了停止：这一发没成，但停下来的原因不是服务商病了，
                    // 与 `STOP_MARK` 那条同族——不往池子身上记一笔失败
                    return Err(STOP_MARK.to_string());
                }
            }
        }
    }
}

/// 认限流用的那个字面串。[`describe_status`] 把状态码翻成人话时就写死了这一串，
/// 重试闸门只留得下字符串，于是按字面认领它——两处共用这一个常量，改文案时
/// 不会一边改了另一边还在等旧的那句
pub(crate) const RETRYABLE_STATUS_WORD: &str = "HTTP 429";

/// 429 的退避间隔：第 n 次等 2ⁿ 秒，封顶 60 秒。"无限重试"不是"无间隔轰炸"——
/// 那只会把限流踩得更死
pub(crate) fn retry_429_delay(attempt: u32) -> u64 {
    2u64.saturating_pow(attempt.min(6)).min(60) * 1000
}

/// 可中断的等待：每 100ms 看一眼停止旗。返回 true = 用户按了停止
pub(crate) fn wait_cancellable(stop: &std::sync::atomic::AtomicBool, ms: u64) -> bool {
    let mut waited = 0u64;
    while waited < ms {
        if stopped(stop) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
        waited += 100;
    }
    stopped(stop)
}

/// 代理层的那一半：按换路计划一发一发试。池绑定最多 [`proxy::MAX_ATTEMPTS`] 条
/// （第一条按策略挑，其余是替补），点名与直连只有一条
pub(crate) fn read_events_routed(
    url: &str,
    config: &AppConfig,
    headers: &[(&str, String)],
    payload: &Value,
    credential_service: &str,
    stop: &std::sync::atomic::AtomicBool,
    on_item: &mut dyn FnMut(StreamItem) -> Result<(), String>,
) -> Result<(), String> {
    // 代理在名单**之后**解析：出口名单问的是"能发到哪一家"（目标域），
    // 代理是这一发走哪条路——顺序反了就等于让代理替出口名单背书
    let mut plan = crate::proxy::plan(config, url)?;
    let mut tried: Vec<String> = Vec::new();
    let mut last = String::from("代理池一条路都没试出去。");
    while let Some(mut leg) = plan.next() {
        let via = leg.proxy_url().unwrap_or("直连").to_string();
        let fail = match read_events_inner(
            url,
            config,
            headers,
            payload,
            credential_service,
            stop,
            on_item,
            &mut leg,
        ) {
            Ok(()) => {
                leg.finish(crate::proxy::Outcome::Reached);
                return Ok(());
            }
            Err(fail) => fail,
        };
        let outcome = fail.outcome;
        let message = fail.message;
        leg.finish(outcome);
        // 只有"一个头都没拿到"才换下一条。这个归因本身就意味着还没往界面吐过
        // 任何一个字节——换路不会把同一回合的正文重播一遍。停止、名单拦截、
        // 服务商的状态码、掐流都各有别的下场，换代理治不了它们
        if outcome != crate::proxy::Outcome::Unreachable {
            return Err(message);
        }
        last = message;
        tried.push(format!("{via}：{last}"));
    }
    if tried.len() <= 1 {
        // 只试了一条（点名、直连，或池里就一条）：错文案照旧，别多包一层
        return Err(last);
    }
    Err(format!(
        "代理池的 {} 条路都连不上：{}。检查这些代理是否还在跑，或去设置 → 代理 换绑定。",
        tried.len(),
        tried.join("；")
    ))
}

pub(crate) fn read_events_inner(
    url: &str,
    config: &AppConfig,
    headers: &[(&str, String)],
    payload: &Value,
    credential_service: &str,
    stop: &std::sync::atomic::AtomicBool,
    on_item: &mut dyn FnMut(StreamItem) -> Result<(), String>,
    leg: &mut crate::proxy::Leg,
) -> Result<(), EgressFail> {
    // 名单先问，网络后动：被拦下的这一发连连接都不该建立（§16）
    crate::egress::guard(&config.net_egress_allow, url).map_err(EgressFail::neutral)?;
    provider_gate(config).map_err(EgressFail::neutral)?;
    // Agent 由这一步的代理地址构造（None = 显式直连，系统代理环境变量不再掺和）
    let agent = crate::proxy::agent_for(leg.proxy_url()).map_err(EgressFail::neutral)?;
    let mut request = with_timeouts(agent.post(url), whole_stream_timeout())
        .header("accept", "text/event-stream");
    for (name, value) in headers {
        request = request.header(*name, value.as_str());
    }
    let started = Instant::now();
    let mut response = match request.send_json(payload.clone()) {
        Ok(response) => response,
        Err(error) => {
            let message = match &error {
                ureq::Error::StatusCode(code) => {
                    // 状态码也是拿到的头：一条慢代理哪怕回了 429，也该留下"它慢"这个样本
                    leg.note_head(started.elapsed());
                    describe_status(*code, credential_service, url)
                }
                other => format!("请求失败：{other}"),
            };
            return Err(EgressFail {
                outcome: crate::proxy::outcome_of(&error),
                message,
            });
        }
    };
    leg.note_head(started.elapsed());
    // 出站链路阶段：连接已建立（头已到手）。目标主机 + 直连/代理
    let via = leg.proxy_url().map(|_| "经代理").unwrap_or("直连");
    let host = tauri::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();
    let _ = on_item(StreamItem::Probe {
        key: "egress".into(),
        detail: format!("{host} · {via}"),
        tone: None,
        hint: None,
    });

    let reader = BufReader::new(response.body_mut().as_reader());
    let mut first_event = true;
    for line in reader.lines() {
        if stopped(stop) {
            return Err(EgressFail::neutral(STOP_MARK.into()));
        }
        let line = line.map_err(|e| {
            let detail = e.to_string();
            // 思考模型在思考阶段长时间不吐字节时，中转站的空闲超时会把连接掐断。
            // 头已经拿到了，所以这不是"这条代理发不出去"——掐流不进冷却，只进读数
            EgressFail {
                outcome: crate::proxy::Outcome::Interrupted,
                message: if detail.contains("disconnect")
                    || detail.contains("reset")
                    || detail.contains("timed out")
                {
                    format!(
                        "读取流中断（{detail}）：常见于服务商或中转站对长时间无数据的连接超时。"
                    )
                } else {
                    format!("读取流中断：{detail}")
                },
            }
        })?;
        let Some(data) = line.trim().strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data == "[DONE]" {
            break;
        }

        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        if let Err(error) = on_item(StreamItem::Chunk(&chunk)) {
            // 前端拒收这一帧：与代理无关，不该冤枉它
            return Err(EgressFail::neutral(error));
        }
        if first_event {
            leg.note_ttft(started.elapsed());
            first_event = false;
            // 首字节阶段：TTFT 是链路质量最硬的那一格读数
            let _ = on_item(StreamItem::Probe {
                key: "ttft".into(),
                detail: format!("首字节 · {} ms", started.elapsed().as_millis()),
                tone: None,
                hint: None,
            });
        }
    }
    Ok(())
}

/// 流式合帧（pi 的 stream-coalescer 同思路）：逐 token 的增量事件先在本地攒帧，
/// 同帧合并为一次 IPC、一次界面渲染。日志不受影响——条目记的是状态机里的
/// 完整文本，这里只决定"往界面送多少次"。
/// 窗口刻意取小（100ms / 512 字符）：快流少跑几百次 IPC，慢流里末尾那半截
/// 最多多停一拍，任何下一个事件到达时立即冲出来
pub(crate) struct DeltaCoalescer {
    pub(crate) text: String,
    pub(crate) reasoning: String,
    pub(crate) since: Option<Instant>,
}

impl DeltaCoalescer {
    pub(crate) fn new() -> Self {
        Self {
            text: String::new(),
            reasoning: String::new(),
            since: None,
        }
    }

    pub(crate) fn push(&mut self, event: ChatEvent, emit: &mut dyn FnMut(ChatEvent)) {
        match event {
            ChatEvent::Delta { text } => {
                if self.since.is_none() {
                    self.since = Some(Instant::now());
                }
                self.text.push_str(&text);
                self.flush_if_due(emit);
            }
            ChatEvent::Reasoning { text } => {
                if self.since.is_none() {
                    self.since = Some(Instant::now());
                }
                self.reasoning.push_str(&text);
                self.flush_if_due(emit);
            }
            // 非增量事件是节点（工具卡、截断提示、用量……）：先冲帧再放行，
            // 保证界面上的事件顺序与服务商给出的顺序一致
            other => {
                self.flush(emit);
                emit(other);
            }
        }
    }

    fn flush_if_due(&mut self, emit: &mut dyn FnMut(ChatEvent)) {
        const MAX_CHARS: usize = 512;
        const MAX_AGE_MS: u128 = 100;
        let chars = self.text.chars().count() + self.reasoning.chars().count();
        let aged = self
            .since
            .is_some_and(|since| since.elapsed().as_millis() >= MAX_AGE_MS);
        if chars >= MAX_CHARS || aged {
            self.flush(emit);
        }
    }

    pub(crate) fn flush(&mut self, emit: &mut dyn FnMut(ChatEvent)) {
        if !self.text.is_empty() {
            let text = std::mem::take(&mut self.text);
            emit(ChatEvent::Delta { text });
        }
        if !self.reasoning.is_empty() {
            let text = std::mem::take(&mut self.reasoning);
            emit(ChatEvent::Reasoning { text });
        }
        self.since = None;
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn finish_round(
    text: String,
    reasoning: String,
    reasoning_signature: Option<String>,
    reasoning_items: Vec<Value>,
    usage: Option<Usage>,
    tool_calls: BTreeMap<usize, ToolCallBuffer>,
    truncated: bool,
    sent_chars: usize,
    // 模型对账三件套：实发的 model 字段 + 上游自报的名字与来源路径
    sent_model: String,
    sniffer: crate::model_trace::ModelSniffer,
) -> Result<RoundOutcome, String> {
    if text.is_empty() && tool_calls.is_empty() && usage.is_none() {
        return Err("服务商没有返回任何内容，请检查模型名与服务商地址。".into());
    }
    // reasoning 项序列化成 JSON 字符串随条目落库：条目结构要 Eq，
    // Value 进不了 Eq，序列化后的字符串既保序又保字段
    let reasoning_items_json = (!reasoning_items.is_empty())
        .then(|| serde_json::to_string(&reasoning_items).ok())
        .flatten();
    let (response_model, response_model_path) = sniffer.result();
    Ok(RoundOutcome {
        text,
        reasoning: (!reasoning.is_empty()).then_some(reasoning),
        reasoning_signature,
        reasoning_items_json,
        tool_calls: tool_calls.into_values().collect(),
        usage,
        sent_chars,
        truncated,
        sent_model,
        response_model,
        response_model_path,
    })
}

#[derive(Default)]
pub(crate) struct ChatState {
    pub(crate) text: String,
    pub(crate) reasoning: String,
    pub(crate) usage: Option<Usage>,
    pub(crate) failure: Option<String>,
    pub(crate) calls: BTreeMap<usize, ToolCallBuffer>,
    /// 这一发放上 wire 的请求体字符数。它由 `read_*_round` 在拼好 payload 之后写进来，
    /// 不另算一遍：估算与实发分家就是 Inspector 那份字节账说谎的开始
    pub(crate) sent_chars: usize,
    /// finish_reason == "length"：输出被 token 上限切断
    pub(crate) truncated: bool,
    /// 模型对账（`crate::model_trace`）：payload 里实际写出的 model 字段
    /// （gemini 的 model 在 URL 上，回落 config.model——那本来就是实发值）
    pub(crate) sent_model: String,
    /// 上游自报的模型名：流式/非流式共用一个增量嗅探器，认出即停
    pub(crate) sniffer: crate::model_trace::ModelSniffer,
    /// Anthropic 随思考块发回的签名（signature_delta）。chat 线永远用不到它
    pub(crate) reasoning_signature: Option<String>,
}

/// chat 格式的一个 SSE 块。单独拆出来是为了能喂录制好的事件序列做断言。
pub(crate) fn apply_chat_event(
    state: &mut ChatState,
    chunk: &Value,
    emit: &mut dyn FnMut(ChatEvent),
) {
    if let Some(message) = chunk["error"]["message"].as_str() {
        state.failure = Some(format!("服务商报错：{message}"));
        return;
    }

    let delta = &chunk["choices"][0]["delta"];

    if let Some(piece) = delta["reasoning_content"].as_str() {
        if !piece.is_empty() {
            state.reasoning.push_str(piece);
            emit(ChatEvent::Reasoning { text: piece.into() });
        }
    }

    if let Some(piece) = delta["content"].as_str() {
        if !piece.is_empty() {
            state.text.push_str(piece);
            emit(ChatEvent::Delta { text: piece.into() });
        }
    }

    // chat 格式没有"这一轮不完整"的终态事件，截断只能从 finish_reason 看出来
    match chunk["choices"][0]["finish_reason"].as_str() {
        Some("length") => {
            state.truncated = true;
            emit(ChatEvent::Notice {
                text: "输出长度达到上限，这一轮被截断了。".into(),
            });
        }
        Some("content_filter") => emit(ChatEvent::Notice {
            text: "内容被服务商的安全策略拦下，这一轮不完整。".into(),
        }),
        _ => {}
    }

    if let Some(calls) = delta["tool_calls"].as_array() {
        for call in calls {
            let index = call["index"].as_u64().unwrap_or(0) as usize;
            let slot = state.calls.entry(index).or_default();
            // chat 格式的 id 和 name 可能被拆进多个增量，所以是累加而不是覆盖
            if let Some(id) = call["id"].as_str() {
                slot.id.push_str(id);
            }
            if let Some(name) = call["function"]["name"].as_str() {
                slot.name.push_str(name);
            }
            if let Some(args) = call["function"]["arguments"].as_str() {
                slot.arguments.push_str(args);
            }
        }
    }

    // 服务商通常在最后一个块里才带 usage，中间那些空对象不能把已经拿到的清掉
    if let Some(next) = Usage::from_chat(&chunk["usage"]) {
        state.usage = Some(next);
    }
}

pub(crate) fn read_chat_round(
    config: &AppConfig,
    key: &str,
    thread: &[Value],
    declared: &[Value],
    cache_key: Option<&str>,
    stop: &std::sync::atomic::AtomicBool,
    emit: &mut dyn FnMut(ChatEvent),
) -> Result<RoundOutcome, RoundFailure> {
    let mut state = ChatState::default();

    let mut headers = vec![("authorization", format!("Bearer {key}"))];
    headers.extend(affinity_headers(&config.base_url, cache_key));
    let payload = chat_payload(config, thread, declared, cache_key);
    state.sent_chars = crate::session::layers::chars_of(&payload);
    // 模型对账：payload 里实际写出的 model 字段。gemini 的 model 在 URL 上，
    // body 里没有——回落 config.model，那本来就是这一发的实发值
    state.sent_model = payload
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| config.model.clone());
    emit(ChatEvent::Probe {
        key: "payload".into(),
        detail: format!("JSON · {:.1} KB", payload.to_string().len() as f64 / 1024.0),
        tone: None,
        hint: None,
    });
    let stream = read_events(
        &config.chat_endpoint(),
        config,
        &headers,
        &payload,
        &config.credential_service,
        stop,
        &mut |item| match item {
            StreamItem::Chunk(chunk) => {
                // 模型对账：每帧先过嗅探器（认出即停），再进线协议状态机
                state.sniffer.feed_chunk(chunk);
                apply_chat_event(&mut state, chunk, emit);
                Ok(())
            }
            StreamItem::Notice(text) => {
                emit(ChatEvent::Notice { text });
                Ok(())
            }
            StreamItem::Probe {
                key,
                detail,
                tone,
                hint,
            } => {
                emit(ChatEvent::Probe {
                    key,
                    detail,
                    tone,
                    hint,
                });
                Ok(())
            }
        },
    );

    // 停止或读挂时 `state` 里已经有内容了：把它跟着错误一起交回去，
    // 否则那半截只活在界面上、日志里没有（F15 的原始形状）
    let partial = partial_of(&state);
    if let Err(message) = stream {
        return Err(RoundFailure { message, partial });
    }
    if let Some(reason) = state.failure.clone() {
        return Err(RoundFailure {
            message: reason,
            partial,
        });
    }
    finish_round(
        std::mem::take(&mut state.text),
        std::mem::take(&mut state.reasoning),
        state.reasoning_signature.take(),
        Vec::new(),
        state.usage.take(),
        std::mem::take(&mut state.calls),
        state.truncated,
        state.sent_chars,
        // 模型对账三件套随行：收尾处一并写进 RoundOutcome
        std::mem::take(&mut state.sent_model),
        std::mem::take(&mut state.sniffer),
    )
    .map_err(|message| RoundFailure { message, partial })
}

/// 从流式状态里取一份"到目前为止已经发出去的东西"。未完成的工具调用**不进这里**：
/// 参数可能只到一半，落库就等于伪造成它跑完了（F9）
pub(crate) fn partial_of(state: &ChatState) -> RoundOutcome {
    let (response_model, response_model_path) = state.sniffer.result();
    RoundOutcome {
        text: state.text.clone(),
        reasoning: (!state.reasoning.is_empty()).then(|| state.reasoning.clone()),
        reasoning_signature: state.reasoning_signature.clone(),
        reasoning_items_json: None,
        tool_calls: Vec::new(),
        usage: state.usage.clone(),
        sent_chars: state.sent_chars,
        truncated: state.truncated,
        sent_model: state.sent_model.clone(),
        response_model,
        response_model_path,
    }
}

#[derive(Default)]
pub(crate) struct ResponsesState {
    pub(crate) text: String,
    pub(crate) reasoning: String,
    pub(crate) usage: Option<Usage>,
    pub(crate) failure: Option<String>,
    /// 一个响应里可以有多个函数调用，事件靠 output_index 认人
    pub(crate) calls: BTreeMap<usize, ToolCallBuffer>,
    /// 同 [`ChatState::sent_chars`]：拼好 payload 那一刻记下的实发字节数
    pub(crate) sent_chars: usize,
    /// response.incomplete（max_output_tokens）：输出被 token 上限切断
    pub(crate) truncated: bool,
    /// 服务商发回的 reasoning 输出项原样保留：store:false 的多轮回放里，
    /// OpenAI 按 id 把 rs_xxx 与 fc_xxx 配对，缺了就 400
    pub(crate) reasoning_items: Vec<Value>,
    /// 模型对账：与 [`ChatState`] 同款的两格（payload 实发 + 上游自报嗅探）
    pub(crate) sent_model: String,
    pub(crate) sniffer: crate::model_trace::ModelSniffer,
}

/// responses 的事件名取自官方 SDK 的 ResponseStreamEvent 联合（63 个），
/// 这里只处理文本、思考、函数调用、终态四类，其余（音频/图像/代码解释器/web 搜索…）忽略。
pub(crate) fn apply_responses_event(
    state: &mut ResponsesState,
    chunk: &Value,
    emit: &mut dyn FnMut(ChatEvent),
) {
    let kind = chunk["type"].as_str().unwrap_or_default();

    match kind {
        "response.output_text.delta" => {
            if let Some(piece) = chunk["delta"].as_str() {
                if !piece.is_empty() {
                    state.text.push_str(piece);
                    emit(ChatEvent::Delta { text: piece.into() });
                }
            }
        }
        "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
            if let Some(piece) = chunk["delta"].as_str() {
                if !piece.is_empty() {
                    state.reasoning.push_str(piece);
                    emit(ChatEvent::Reasoning { text: piece.into() });
                }
            }
        }
        "response.output_item.added" | "response.output_item.done" => {
            // reasoning 项原样留档（回放时逐字透传）：只在 done 时收一次，
            // added 的槽位字段是流式暂存，不该进历史
            if kind == "response.output_item.done"
                && chunk["item"]["type"].as_str() == Some("reasoning")
            {
                state.reasoning_items.push(chunk["item"].clone());
            }
            if chunk["item"]["type"].as_str() == Some("function_call") {
                let index = chunk["output_index"].as_u64().unwrap_or(0) as usize;
                let slot = state.calls.entry(index).or_default();
                if slot.id.is_empty() {
                    if let Some(id) = chunk["item"]["call_id"].as_str() {
                        slot.id = id.to_string();
                    }
                }
                if slot.name.is_empty() {
                    if let Some(name) = chunk["item"]["name"].as_str() {
                        slot.name = name.to_string();
                    }
                }
                // done 带的是拼好的完整参数，以它为准，覆盖增量拼出来的那份
                if kind.ends_with(".done") {
                    if let Some(args) = chunk["item"]["arguments"].as_str() {
                        slot.arguments = args.to_string();
                    }
                }
            }
        }
        "response.function_call_arguments.delta" => {
            let index = chunk["output_index"].as_u64().unwrap_or(0) as usize;
            let slot = state.calls.entry(index).or_default();
            if let Some(piece) = chunk["delta"].as_str() {
                slot.arguments.push_str(piece);
            }
        }
        "response.function_call_arguments.done" => {
            let index = chunk["output_index"].as_u64().unwrap_or(0) as usize;
            let slot = state.calls.entry(index).or_default();
            if let Some(args) = chunk["arguments"].as_str() {
                slot.arguments = args.to_string();
            }
        }
        // 真机实测：服务商会在 incomplete 上照样回 usage，只认 completed 就把这一轮的用量整个丢了
        "response.completed" | "response.incomplete" => {
            // Azure 在 output_item.done 里省略 encrypted_content，只在终态的
            // response.output 里给全（pi 同款回填）：按 id 补进留档的项里，
            // 否则下一轮的 function_call 配不上它的 reasoning 项
            for item in chunk["response"]["output"].as_array().into_iter().flatten() {
                if item["type"].as_str() != Some("reasoning") {
                    continue;
                }
                let (Some(id), Some(encrypted)) =
                    (item["id"].as_str(), item["encrypted_content"].as_str())
                else {
                    continue;
                };
                for stored in &mut state.reasoning_items {
                    if stored["id"].as_str() == Some(id)
                        && stored.get("encrypted_content").is_none()
                    {
                        stored["encrypted_content"] = json!(encrypted);
                    }
                }
            }
            if let Some(next) = Usage::from_responses(&chunk["response"]["usage"]) {
                state.usage = Some(next);
            }
            if kind == "response.incomplete" {
                let reason = match chunk["response"]["incomplete_details"]["reason"].as_str() {
                    Some("max_output_tokens") => {
                        state.truncated = true;
                        "输出长度达到上限".to_string()
                    }
                    Some("content_filter") => "内容被安全策略拦下".to_string(),
                    Some(other) => other.to_string(),
                    None => String::new(),
                };
                let suffix = match &state.usage {
                    Some(usage) => format!("，已生成 {} 个输出 token", usage.output_tokens),
                    None => String::new(),
                };
                emit(ChatEvent::Notice {
                    text: if reason.is_empty() {
                        format!("这一轮被服务商提前截断{suffix}。")
                    } else {
                        format!("这一轮被服务商提前截断（{reason}{suffix}）。")
                    },
                });
            }
        }
        "response.failed" => {
            let reason = chunk["response"]["error"]["message"].as_str();
            state.failure = Some(match reason {
                Some(text) => format!("服务商生成失败：{text}"),
                None => "服务商报告生成失败，但没有给出原因。".into(),
            });
        }
        // 事件名是 "error"，不是 "response.error"——照 SDK 的字面量来
        "error" => {
            let reason = chunk["message"].as_str();
            state.failure = Some(match reason {
                Some(text) => format!("服务商报错：{text}"),
                None => "服务商报了一个没有说明的错误。".into(),
            });
        }
        _ => {}
    }
}

pub(crate) fn read_responses_round(
    config: &AppConfig,
    key: &str,
    thread: &[Value],
    declared: &[Value],
    cache_key: Option<&str>,
    stop: &std::sync::atomic::AtomicBool,
    emit: &mut dyn FnMut(ChatEvent),
) -> Result<RoundOutcome, RoundFailure> {
    let mut state = ResponsesState::default();

    let mut headers = vec![("authorization", format!("Bearer {key}"))];
    headers.extend(affinity_headers(&config.base_url, cache_key));
    let payload = responses_payload(config, thread, declared, cache_key);
    state.sent_chars = crate::session::layers::chars_of(&payload);
    // 模型对账：payload 里实际写出的 model 字段。gemini 的 model 在 URL 上，
    // body 里没有——回落 config.model，那本来就是这一发的实发值
    state.sent_model = payload
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| config.model.clone());
    emit(ChatEvent::Probe {
        key: "payload".into(),
        detail: format!("JSON · {:.1} KB", payload.to_string().len() as f64 / 1024.0),
        tone: None,
        hint: None,
    });
    let stream = read_events(
        &config.responses_endpoint(),
        config,
        &headers,
        &payload,
        &config.credential_service,
        stop,
        &mut |item| match item {
            StreamItem::Chunk(chunk) => {
                // 模型对账：每帧先过嗅探器（认出即停），再进线协议状态机
                state.sniffer.feed_chunk(chunk);
                apply_responses_event(&mut state, chunk, emit);
                Ok(())
            }
            StreamItem::Notice(text) => {
                emit(ChatEvent::Notice { text });
                Ok(())
            }
            StreamItem::Probe {
                key,
                detail,
                tone,
                hint,
            } => {
                emit(ChatEvent::Probe {
                    key,
                    detail,
                    tone,
                    hint,
                });
                Ok(())
            }
        },
    );

    // 停止或读挂时 `state` 里已经有内容了：把它跟着错误一起交回去，
    // 否则那半截只活在界面上、日志里没有（F15 的原始形状）
    let partial = responses_partial(&state);
    if let Err(message) = stream {
        return Err(RoundFailure { message, partial });
    }
    if let Some(reason) = state.failure.clone() {
        return Err(RoundFailure {
            message: reason,
            partial,
        });
    }
    let reasoning_items = std::mem::take(&mut state.reasoning_items);
    finish_round(
        std::mem::take(&mut state.text),
        std::mem::take(&mut state.reasoning),
        None,
        reasoning_items,
        state.usage.take(),
        std::mem::take(&mut state.calls),
        state.truncated,
        state.sent_chars,
        // 模型对账三件套随行：收尾处一并写进 RoundOutcome
        std::mem::take(&mut state.sent_model),
        std::mem::take(&mut state.sniffer),
    )
    .map_err(|message| RoundFailure { message, partial })
}

/// 从流式状态里取一份"到目前为止已经发出去的东西"。未完成的工具调用**不进这里**：
/// 参数可能只到一半，落库就等于伪造成它跑完了（F9）
pub(crate) fn responses_partial(state: &ResponsesState) -> RoundOutcome {
    // 已经收完的 reasoning 项是完整凭据（含回填后的 encrypted_content），
    // 半截回答里它们照样跟着走
    let reasoning_items_json = (!state.reasoning_items.is_empty())
        .then(|| serde_json::to_string(&state.reasoning_items).ok())
        .flatten();
    let (response_model, response_model_path) = state.sniffer.result();
    RoundOutcome {
        text: state.text.clone(),
        reasoning: (!state.reasoning.is_empty()).then(|| state.reasoning.clone()),
        reasoning_signature: None,
        reasoning_items_json,
        tool_calls: Vec::new(),
        usage: state.usage.clone(),
        sent_chars: state.sent_chars,
        truncated: state.truncated,
        sent_model: state.sent_model.clone(),
        response_model,
        response_model_path,
    }
}

pub(crate) fn describe_status(code: u16, credential_service: &str, base_url: &str) -> String {
    match code {
        401 | 403 => format!(
            "服务商拒绝鉴权（HTTP {code}）：{base_url} 不接受凭据目标 {credential_service} 里存的密钥。\
             所有档案默认共用这一个槽位——你刚换的 key 若是别的站的（或已换过凭据目标），\
             这里读到的就不是本站的 key。到「设置 → 服务商档案」核对这张请求用的档案及其凭据目标。"
        ),
        404 => "服务商返回 HTTP 404：通常是 base URL 少了 /v1 这类路径前缀，或模型名不存在。".into(),
        // 这一句里那个字面串就是重试闸门认的那个：两边共用 `RETRYABLE_STATUS_WORD`，
        // 于是"改文案把重试改瞎了"这件事在编译期就不可能发生
        429 => format!("服务商限流（{RETRYABLE_STATUS_WORD}），稍后重试。"),
        c if c >= 500 => format!("服务商上游错误（HTTP {c}），稍后重试。"),
        c => format!("服务商返回 HTTP {c}。"),
    }
}

/// 这条失败要不要让池子换人重试。判的是"服务商病了/忙了"这一族——
/// 鉴权与路径错误（401/403/404）是成员自己的配置问题，换人只会掩盖它
pub(crate) fn is_pool_swappable_error(message: &str) -> bool {
    message.starts_with("服务商上游错误")
        || message.starts_with("服务商限流")
        || message.starts_with("请求失败：")
}

pub(crate) const ENHANCE_SYSTEM: &str =
    "你是提示词改写助手。把用户给出的草稿改写成一份给 AI 的清晰、具体、可执行的提示词：\
补全关键细节与约束、明确期望的输出形式，保留用户原本的意图与语言（中文草稿就输出中文）。\
只输出改写后的提示词本身：不要解释你做了什么、不要前后缀、不要用代码块包裹。";
