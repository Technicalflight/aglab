//! 模型一致性对账：用户要的模型、本地映射后实发的模型、上游自报回家的模型，
//! 三方对不上要说得出是谁改的——本地映射（路由表/点名/池子）是预期内，
//! 上游擅自换人是需要告警的。
//!
//! 三层链路：
//! 1. **请求侧**：`read_*_round` 拼好 payload 后抄下 `sent_model`（gemini 的
//!    model 走 URL 不进 body，回落 config.model——那本来就是实发值）；
//! 2. **响应侧**：[`ModelSniffer`] 增量嗅探流式/非流式里的自报模型名；
//! 3. **判定**：[`classify`] 分四格落账 `model_traces`，界面的链路条同步亮色。
//!
//! 嗅探是增量状态机：`read_events` 那一侧本来就是逐行缓冲（BufReader::lines，
//! 跨 chunk 断裂由行缓冲兜住），这里每收到一个完整 JSON chunk 才喂一口，
//! 且一旦认出模型名就不再解析后续帧——性能与正确性同一处保证。

use rusqlite::params;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::config::AppConfig;

/// 对不上的责任归属。serde 走 snake_case，与台账里的字符串同形。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MismatchKind {
    /// 一致（含日期后缀变体与白名单命中——都是预期内的"算一致"）
    None,
    /// 客户端本地映射导致（路由表/点名/池子），预期内
    LocalMapping,
    /// 上游替换/降级，需要告警
    UpstreamReplaced,
    /// 上游没报模型名或报了认不出来
    Unknown,
}

impl MismatchKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            MismatchKind::None => "none",
            MismatchKind::LocalMapping => "local_mapping",
            MismatchKind::UpstreamReplaced => "upstream_replaced",
            MismatchKind::Unknown => "unknown",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "none" => Some(MismatchKind::None),
            "local_mapping" => Some(MismatchKind::LocalMapping),
            "upstream_replaced" => Some(MismatchKind::UpstreamReplaced),
            "unknown" => Some(MismatchKind::Unknown),
            _ => None,
        }
    }
}

/// 一发请求的模型三方账。`message_id` 在回合内还铸不出来（assistant 行在
/// 收尾才落账），留空；前端拿 Done 的 entryIds 对号。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ModelTrace {
    pub id: String,
    pub conversation_id: String,
    #[serde(default)]
    pub message_id: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub endpoint: String,
    pub requested_model: String,
    pub mapped_model: String,
    pub sent_model: String,
    pub response_model: Option<String>,
    pub mismatch_kind: MismatchKind,
    pub variant_of: Option<String>,
    #[serde(default)]
    pub raw_response_model_path: Option<String>,
    pub created_at: i64,
}

/// 判定结果：四格归属 + 命中日期变体时的原值（如 sent=gpt-4o、上游答
/// gpt-4o-2024-08-06，kind=None 且 variant_of=Some("gpt-4o-2024-08-06")）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub kind: MismatchKind,
    pub variant_of: Option<String>,
}

/// 响应侧模型嗅探器：增量状态机，同时认得四条线的形状。
/// 一旦认出就停（后续帧直接略过），并把"从哪个字段抓到的"记下来。
#[derive(Default)]
pub struct ModelSniffer {
    detected: Option<String>,
    source_path: Option<String>,
    /// SSE 的 `event:` 行（Anthropic/Responses 线的帧类型也写在 JSON 的 type
    /// 里，这里是兜底：type 缺席时用事件名标注来源路径）
    last_event: Option<String>,
}

// 嗅探器的喂入面在 test 构建里随 run_turn 一起被门控收窄，非 test 下由 read_*_round 消费
#[allow(dead_code)]
impl ModelSniffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// 非流式：喂完整 body。OpenAI/Anthropic 是 `body.model`，Gemini 是
    /// `body.modelVersion`（老形状 `body.model` 也认）
    pub fn feed_json(&mut self, body: &[u8]) {
        if self.detected.is_some() {
            return;
        }
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
            return;
        };
        self.feed_value(&value, "");
    }

    /// 流式：喂每个 SSE 帧的 data 载荷（JSON 文本；`[DONE]` 与空帧是无害噪声）
    pub fn feed_sse_data(&mut self, data: &str) {
        if self.detected.is_some() {
            return;
        }
        let trimmed = data.trim();
        if trimmed.is_empty() || trimmed == "[DONE]" {
            return;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            return;
        };
        self.feed_chunk(&value);
    }

    /// 流式：喂 `event:` 行（可选；JSON 里的 type 字段优先于它）
    pub fn feed_sse_event(&mut self, event: &str) {
        self.last_event = Some(event.trim().to_string());
    }

    /// 流式：喂已解析的一帧。四条线的认法按优先级：
    /// 1. Anthropic `message_start` → `message.model`（路径 `message_start.message.model`）
    /// 2. Responses `response.created/completed` → `response.model`（路径带帧类型）
    /// 3. Gemini `modelVersion`（路径 `modelVersion`）
    /// 4. OpenAI chat 兜底 → 顶层 `model`（路径 `model`，首帧即真身）
    pub fn feed_chunk(&mut self, value: &serde_json::Value) {
        if self.detected.is_some() {
            return;
        }
        // 帧类型：JSON 的 type 字段说了算，缺席退回 event: 行
        let frame = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .or_else(|| self.last_event.clone());

        // 1) Anthropic：message_start 的 message.model
        if frame.as_deref() == Some("message_start") {
            if let Some(model) = value
                .pointer("/message/model")
                .and_then(serde_json::Value::as_str)
            {
                self.set(model, "message_start.message.model");
                return;
            }
        }
        // 2) Responses：response.created / response.completed 的 response.model
        if let Some(frame_name) = frame.as_deref() {
            if frame_name.starts_with("response.") {
                if let Some(model) = value
                    .pointer("/response/model")
                    .and_then(serde_json::Value::as_str)
                {
                    let path = format!("{frame_name}.response.model");
                    self.set(model, &path);
                    return;
                }
            }
        }
        // 3) Gemini：modelVersion
        if let Some(model) = value
            .get("modelVersion")
            .and_then(serde_json::Value::as_str)
        {
            self.set(model, "modelVersion");
            return;
        }
        // 4) OpenAI 系兜底：顶层 model
        if let Some(model) = value.get("model").and_then(serde_json::Value::as_str) {
            self.set(model, "model");
        }
    }

    /// 非流式与流式共用的落格：非流式的路径加个 body 前缀，调试时一眼分清
    fn feed_value(&mut self, value: &serde_json::Value, scope: &str) {
        for (path, key) in [
            ("body.modelVersion", "modelVersion"),
            ("body.model", "model"),
        ] {
            let pointer = format!("/{}{}", scope, key);
            if let Some(model) = value.pointer(&pointer).and_then(serde_json::Value::as_str) {
                self.set(model, path);
                return;
            }
        }
    }

    fn set(&mut self, model: &str, path: &str) {
        self.detected = Some(model.to_string());
        self.source_path = Some(path.to_string());
    }

    /// (model, 来源路径)。模型没报/没解析出来都是 (None, None)
    pub fn result(&self) -> (Option<String>, Option<String>) {
        (self.detected.clone(), self.source_path.clone())
    }
}

/// 大小写不敏感的等值；日期后缀变体（sent + "-2024-08-06" 这类）也算一致，
/// 返回 Some(变体原值)。变体的判法保守：余段只能由 2/4 位数字段与 `-` 组成。
fn variant_of(response: &str, base: &str) -> Option<String> {
    if response.eq_ignore_ascii_case(base) {
        return Some(response.to_string()); // 精确等值也算"变体命中"（变体原值=自己）
    }
    let (lower_response, lower_base) = (response.to_ascii_lowercase(), base.to_ascii_lowercase());
    let remainder = lower_response.strip_prefix(&lower_base)?;
    let remainder = remainder.strip_prefix('-')?;
    if remainder.is_empty() {
        return None;
    }
    let date_like = remainder.split('-').all(|part| {
        !part.is_empty() && part.len() <= 4 && part.chars().all(|c| c.is_ascii_digit())
    });
    date_like.then(|| response.to_string())
}

/// 四格判定。顺序承重：
/// 1. 上游没报 → Unknown（不是一致——"没报"与"一致"是两件事）；
/// 2. 与 sent 一致（含变体）→ None；
/// 3. 与 mapped 一致（含变体）→ None（sent 与 mapped 理应相同，防御性放行）；
/// 4. 白名单命中 → None（中转站统一回一个名字是已知约定）；
/// 5. sent != mapped → LocalMapping（响应又与谁都不一致时先怪本地映射，防御）；
/// 6. 其余（response != mapped）→ UpstreamReplaced。
pub fn classify(
    _requested: &str,
    mapped: &str,
    sent: &str,
    response: Option<&str>,
    whitelist: &[String],
) -> Verdict {
    let Some(response) = response else {
        return Verdict {
            kind: MismatchKind::Unknown,
            variant_of: None,
        };
    };
    if let Some(variant) = variant_of(response, sent) {
        let exact = response.eq_ignore_ascii_case(sent);
        return Verdict {
            kind: MismatchKind::None,
            variant_of: (!exact).then_some(variant),
        };
    }
    if let Some(variant) = variant_of(response, mapped) {
        let exact = response.eq_ignore_ascii_case(mapped);
        return Verdict {
            kind: MismatchKind::None,
            variant_of: (!exact).then_some(variant),
        };
    }
    let whitelisted = whitelist.iter().any(|entry| {
        response.eq_ignore_ascii_case(entry.trim()) || variant_of(response, entry.trim()).is_some()
    });
    if whitelisted {
        return Verdict {
            kind: MismatchKind::None,
            variant_of: None,
        };
    }
    if !sent.eq_ignore_ascii_case(mapped) {
        return Verdict {
            kind: MismatchKind::LocalMapping,
            variant_of: None,
        };
    }
    Verdict {
        kind: MismatchKind::UpstreamReplaced,
        variant_of: None,
    }
}

// ---------------------------------------------------------------------------
// 存储：跟随 usage.db（同库不同表），建表与索引随打开即就绪
// ---------------------------------------------------------------------------

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS model_traces (
    id TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL,
    message_id TEXT NOT NULL DEFAULT '',
    provider TEXT NOT NULL DEFAULT '',
    endpoint TEXT NOT NULL DEFAULT '',
    requested_model TEXT NOT NULL,
    mapped_model TEXT NOT NULL,
    sent_model TEXT NOT NULL,
    response_model TEXT,
    mismatch_kind TEXT NOT NULL,
    variant_of TEXT,
    raw_response_model_path TEXT,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_model_traces_conversation ON model_traces(conversation_id);
CREATE INDEX IF NOT EXISTS idx_model_traces_kind ON model_traces(mismatch_kind);
CREATE INDEX IF NOT EXISTS idx_model_traces_created ON model_traces(created_at);
";

fn open(app: &AppHandle) -> Result<rusqlite::Connection, String> {
    // 与 usage 台账同一份库文件：对账数据归"账"不归"话题"，独立小表不开第二个库
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("定位配置目录失败：{e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建配置目录失败：{e}"))?;
    let conn = rusqlite::Connection::open(dir.join("usage.db"))
        .map_err(|e| format!("打开用量台账失败：{e}"))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    conn.execute_batch("PRAGMA journal_mode = WAL;")
        .map_err(|e| e.to_string())?;
    conn.execute_batch(SCHEMA)
        .map_err(|e| format!("建模型对账表失败：{e}"))?;
    Ok(conn)
}

fn now_id() -> String {
    // 进程内唯一即可（id 只用来对号，不跨库引用）：毫秒时间戳 + 原子计数
    use std::sync::atomic::{AtomicU64, Ordering};
    static NONCE: AtomicU64 = AtomicU64::new(0);
    let nonce = NONCE.fetch_add(1, Ordering::Relaxed);
    format!("trace_{:x}_{nonce}", crate::session::now_millis())
}

#[allow(dead_code)] // Main 侧封装：worker 直用 _in 变体；M5 chat.rs 拆空时统一清算
/// 一发记一笔。写库失败静默：对账是副产品，绝不能打断对话。
pub fn record(
    app: &AppHandle,
    conversation_id: &str,
    provider: &str,
    endpoint: &str,
    requested: &str,
    mapped: &str,
    sent: &str,
    response_model: Option<String>,
    kind: MismatchKind,
    variant_of: Option<String>,
    raw_path: Option<String>,
) {
    match app.path().app_config_dir().map_err(|e| e.to_string()) {
        Ok(config_dir) => record_in(
            &config_dir,
            conversation_id,
            provider,
            endpoint,
            requested,
            mapped,
            sent,
            response_model,
            kind,
            variant_of,
            raw_path,
        ),
        Err(e) => eprintln!("对账表路径没解析出来，这一笔没记上：{e}"),
    }
}

/// worker 进程的变体（M3 收官）：目录由 Main 经 CLI 传来
#[allow(clippy::too_many_arguments)]
pub fn record_in(
    config_dir: &std::path::Path,
    conversation_id: &str,
    provider: &str,
    endpoint: &str,
    requested: &str,
    mapped: &str,
    sent: &str,
    response_model: Option<String>,
    kind: MismatchKind,
    variant_of: Option<String>,
    raw_path: Option<String>,
) {
    let trace = ModelTrace {
        id: now_id(),
        conversation_id: conversation_id.to_string(),
        message_id: String::new(),
        provider: provider.to_string(),
        endpoint: endpoint.to_string(),
        requested_model: requested.to_string(),
        mapped_model: mapped.to_string(),
        sent_model: sent.to_string(),
        response_model,
        mismatch_kind: kind,
        variant_of,
        raw_response_model_path: raw_path,
        created_at: crate::session::now_millis(),
    };
    if let Ok(conn) = crate::usage::open_in(config_dir) {
        let _ = conn.execute(
            "INSERT INTO model_traces (
                id, conversation_id, message_id, provider, endpoint,
                requested_model, mapped_model, sent_model, response_model,
                mismatch_kind, variant_of, raw_response_model_path, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                trace.id,
                trace.conversation_id,
                trace.message_id,
                trace.provider,
                trace.endpoint,
                trace.requested_model,
                trace.mapped_model,
                trace.sent_model,
                trace.response_model,
                trace.mismatch_kind.as_str(),
                trace.variant_of,
                trace.raw_response_model_path,
                trace.created_at,
            ],
        );
    }
}

fn row_to_trace(row: &rusqlite::Row<'_>) -> rusqlite::Result<ModelTrace> {
    let kind: String = row.get("mismatch_kind")?;
    Ok(ModelTrace {
        id: row.get("id")?,
        conversation_id: row.get("conversation_id")?,
        message_id: row.get("message_id")?,
        provider: row.get("provider")?,
        endpoint: row.get("endpoint")?,
        requested_model: row.get("requested_model")?,
        mapped_model: row.get("mapped_model")?,
        sent_model: row.get("sent_model")?,
        response_model: row.get("response_model")?,
        mismatch_kind: MismatchKind::from_str(&kind).unwrap_or(MismatchKind::Unknown),
        variant_of: row.get("variant_of")?,
        raw_response_model_path: row.get("raw_response_model_path")?,
        created_at: row.get("created_at")?,
    })
}

/// 按话题/归属过滤，新写的在前。
pub fn list(
    app: &AppHandle,
    conversation_id: Option<&str>,
    kind: Option<&str>,
    limit: u32,
) -> Result<Vec<ModelTrace>, String> {
    let conn = open(app)?;
    let mut sql = String::from("SELECT * FROM model_traces");
    let mut clauses: Vec<&'static str> = Vec::new();
    if conversation_id.is_some() {
        clauses.push("conversation_id = ?1");
    }
    if kind.is_some() {
        clauses.push("mismatch_kind = ?2");
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ");
    sql.push_str(&limit.max(1).to_string());
    let mut statement = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let mut rows = statement
        .query(params![conversation_id.unwrap_or(""), kind.unwrap_or(""),])
        .map_err(|e| e.to_string())?;
    let mut traces = Vec::new();
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        traces.push(row_to_trace(row).map_err(|e| e.to_string())?);
    }
    Ok(traces)
}

/// 四格计数（顺序 none/local_mapping/upstream_replaced/unknown）。
pub fn count_by_kind(app: &AppHandle) -> Result<Vec<(String, i64)>, String> {
    let conn = open(app)?;
    let mut statement = conn
        .prepare("SELECT mismatch_kind, COUNT(*) FROM model_traces GROUP BY mismatch_kind")
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(|e| e.to_string())?;
    let mut counts = Vec::new();
    for row in rows {
        let (kind, count) = row.map_err(|e| e.to_string())?;
        counts.push((kind, count));
    }
    Ok(counts)
}

// ---------------------------------------------------------------------------
// Tauri commands：面板/测试要查账走的门。全部 Result<T, String>
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn model_trace_insert(app: AppHandle, trace: ModelTrace) -> Result<(), String> {
    let conn = open(&app)?;
    conn.execute(
        "INSERT OR REPLACE INTO model_traces (
            id, conversation_id, message_id, provider, endpoint,
            requested_model, mapped_model, sent_model, response_model,
            mismatch_kind, variant_of, raw_response_model_path, created_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            trace.id,
            trace.conversation_id,
            trace.message_id,
            trace.provider,
            trace.endpoint,
            trace.requested_model,
            trace.mapped_model,
            trace.sent_model,
            trace.response_model,
            trace.mismatch_kind.as_str(),
            trace.variant_of,
            trace.raw_response_model_path,
            trace.created_at,
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn model_trace_list(
    app: AppHandle,
    conversation_id: Option<String>,
    kind: Option<String>,
    limit: Option<u32>,
) -> Result<Vec<ModelTrace>, String> {
    list(
        &app,
        conversation_id.as_deref(),
        kind.as_deref(),
        limit.unwrap_or(200),
    )
}

#[tauri::command]
pub fn model_trace_count_by_kind(app: AppHandle) -> Result<Vec<(String, i64)>, String> {
    count_by_kind(&app)
}

/// 这一发走的线协议名（对账的 endpoint 格）。
pub fn endpoint_label(config: &AppConfig) -> &'static str {
    if config.uses_anthropic() {
        "messages"
    } else if config.uses_responses() {
        "responses"
    } else if config.uses_gemini() {
        "gemini"
    } else {
        "chat_completions"
    }
}

/// 中转站白名单：这些名字上游统一返回时不算被换人（config.upstream_model_whitelist）。
pub fn whitelist_of(config: &AppConfig) -> &[String] {
    &config.upstream_model_whitelist
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---------------- ModelSniffer：四条线 + 噪声 + 提前停止 ----------------

    #[test]
    fn sniffs_openai_chat_first_chunk_model() {
        let mut sniffer = ModelSniffer::new();
        sniffer
            .feed_sse_data(r#"{"id":"1","model":"gpt-4o","choices":[{"delta":{"content":"hi"}}]}"#);
        // 后续帧不再解析：就算下一帧换了名字（现实中不会），也以首帧为准
        sniffer.feed_sse_data(r#"{"model":"someone-else","choices":[]}"#);
        assert_eq!(
            sniffer.result(),
            (Some("gpt-4o".into()), Some("model".into())),
            "OpenAI chat 首帧的 model 就是真身，且认出即停"
        );
    }

    #[test]
    fn sniffs_anthropic_message_start() {
        let mut sniffer = ModelSniffer::new();
        sniffer.feed_sse_event("message_start");
        sniffer.feed_sse_data(
            r#"{"type":"message_start","message":{"id":"m1","model":"claude-sonnet-4-5"}}"#,
        );
        assert_eq!(
            sniffer.result(),
            (
                Some("claude-sonnet-4-5".into()),
                Some("message_start.message.model".into())
            )
        );
    }

    #[test]
    fn sniffs_responses_created_and_completed() {
        let mut sniffer = ModelSniffer::new();
        sniffer.feed_sse_data(
            r#"{"type":"response.created","response":{"id":"r1","model":"gpt-5-mini"}}"#,
        );
        assert_eq!(
            sniffer.result(),
            (
                Some("gpt-5-mini".into()),
                Some("response.created.response.model".into())
            )
        );

        let mut sniffer = ModelSniffer::new();
        sniffer.feed_sse_event("response.completed");
        sniffer.feed_sse_data(r#"{"response":{"id":"r2","model":"gpt-5"}}"#);
        assert_eq!(
            sniffer.result(),
            (
                Some("gpt-5".into()),
                Some("response.completed.response.model".into())
            ),
            "type 字段缺席时退回 event: 行标注来源"
        );
    }

    #[test]
    fn sniffs_gemini_model_version() {
        let mut sniffer = ModelSniffer::new();
        sniffer.feed_sse_data(
            r#"{"candidates":[{"content":{"parts":[{"text":"好"}]}}],"modelVersion":"gemini-2.5-flash"}"#,
        );
        assert_eq!(
            sniffer.result(),
            (Some("gemini-2.5-flash".into()), Some("modelVersion".into()))
        );
    }

    #[test]
    fn ignores_done_and_noise_frames() {
        let mut sniffer = ModelSniffer::new();
        sniffer.feed_sse_data("[DONE]");
        sniffer.feed_sse_data("   ");
        sniffer.feed_sse_data("这不是 JSON");
        assert_eq!(sniffer.result(), (None, None), "噪声帧不产出读数");
    }

    #[test]
    fn non_stream_bodies_both_shapes() {
        let mut sniffer = ModelSniffer::new();
        sniffer.feed_json(
            br#"{"id":"c1","model":"deepseek-chat","choices":[{"message":{"role":"assistant"}}]}"#,
        );
        assert_eq!(
            sniffer.result(),
            (Some("deepseek-chat".into()), Some("body.model".into()))
        );

        let mut sniffer = ModelSniffer::new();
        sniffer.feed_json(br#"{"modelVersion":"gemini-2.5-pro","candidates":[]}"#);
        assert_eq!(
            sniffer.result(),
            (
                Some("gemini-2.5-pro".into()),
                Some("body.modelVersion".into())
            ),
            "Gemini 优先认 modelVersion"
        );
    }

    #[test]
    fn feed_chunk_accepts_value_and_stops_early() {
        let mut sniffer = ModelSniffer::new();
        sniffer.feed_chunk(&json!({"model": "glm-4.7"}));
        // 已认出之后哪怕喂进完全不同形状的帧也不再翻动
        sniffer.feed_chunk(&json!({"type":"message_start","message":{"model":"claude-x"}}));
        assert_eq!(
            sniffer.result(),
            (Some("glm-4.7".into()), Some("model".into()))
        );
    }

    #[test]
    fn message_start_without_model_falls_through_to_generic() {
        // 防御：形状不齐（message_start 里没 model）就退回顶层 model 兜底，别空手
        let mut sniffer = ModelSniffer::new();
        sniffer.feed_sse_data(r#"{"type":"message_start","message":{},"model":"claude-fallback"}"#);
        assert_eq!(
            sniffer.result(),
            (Some("claude-fallback".into()), Some("model".into()))
        );
    }

    // ---------------- classify：四格判定 ----------------

    #[test]
    fn no_response_model_is_unknown() {
        let verdict = classify("gpt-4o", "gpt-4o", "gpt-4o", None, &[]);
        assert_eq!(verdict.kind, MismatchKind::Unknown, "没报不算一致");
    }

    #[test]
    fn exact_and_date_variant_count_as_consistent() {
        let exact = classify("gpt-4o", "gpt-4o", "gpt-4o", Some("GPT-4o"), &[]);
        assert_eq!(exact.kind, MismatchKind::None, "大小写不敏感");
        assert_eq!(exact.variant_of, None);

        let variant = classify("gpt-4o", "gpt-4o", "gpt-4o", Some("gpt-4o-2024-08-06"), &[]);
        assert_eq!(variant.kind, MismatchKind::None, "日期后缀变体视为一致");
        assert_eq!(variant.variant_of.as_deref(), Some("gpt-4o-2024-08-06"));
    }

    #[test]
    fn upstream_replacement_is_caught() {
        let verdict = classify("glm-4.7", "glm-4.7", "glm-4.7", Some("gpt-3.5-turbo"), &[]);
        assert_eq!(verdict.kind, MismatchKind::UpstreamReplaced);
    }

    #[test]
    fn whitelist_hit_is_not_replacement() {
        let verdict = classify(
            "gpt-4o",
            "gpt-4o",
            "gpt-4o",
            Some("gpt-3.5-turbo"),
            &["gpt-3.5-turbo".to_string()],
        );
        assert_eq!(verdict.kind, MismatchKind::None, "中转站统一回名是已知约定");
    }

    #[test]
    fn local_mapping_blame_when_sent_differs_from_mapped() {
        // sent != mapped 只可能来自本地链路自己（路由/点名写错），响应也不认账时先怪自己
        let verdict = classify("a", "b", "c", Some("totally-other"), &[]);
        assert_eq!(verdict.kind, MismatchKind::LocalMapping);
    }

    #[test]
    fn variant_lookalike_is_not_consistent() {
        // 变体判法保守：余段不是纯数字段（latest/exp 之类）不算变体，照常替换处理
        let verdict = classify("gpt-4o", "gpt-4o", "gpt-4o", Some("gpt-4o-latest"), &[]);
        assert_eq!(verdict.kind, MismatchKind::UpstreamReplaced);
    }
}
