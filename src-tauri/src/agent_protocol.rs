//! Agent 子进程协议（M1 骨架，见 docs/design/agent-subprocess-and-sync.md §A2）。
//!
//! 传输：JSON Lines——stdin/stdout 上每行一个信封，UTF-8，无跨行帧。
//! 信封是唯一通道形状，双侧各持一份 schema：Rust serde（编译期）+
//! 前端 zod（M2 接线时加，运行时校验）。校验失败永不 panic——回错误信封。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 协议版本。字段只增不改：未知字段保留（serde 默认行为），未知方法回错
pub const PROTOCOL_VERSION: u64 = 1;

/// 信封载荷。`kind` 标签 + 各臂自带的字段——req 是请求，resp/err 是对请求的
/// 终答，ev 是流内事件（M2 起事件面直接复用 ChatEvent）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum EnvelopePayload {
    #[serde(rename_all = "camelCase")]
    Req { method: String, #[serde(default)] params: Value },
    #[serde(rename_all = "camelCase")]
    Resp { result: Value },
    #[serde(rename_all = "camelCase")]
    Ev { event: String, #[serde(default)] data: Value },
    #[serde(rename_all = "camelCase")]
    Err { error: AgentError },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentError {
    pub code: String,
    pub message: String,
}

impl AgentError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self { code: code.to_string(), message: message.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub v: u64,
    pub id: u64,
    #[serde(flatten)]
    pub payload: EnvelopePayload,
}

impl Envelope {
    pub fn req(id: u64, method: &str, params: Value) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id,
            payload: EnvelopePayload::Req { method: method.to_string(), params },
        }
    }

    pub fn resp(id: u64, result: Value) -> Self {
        Self { v: PROTOCOL_VERSION, id, payload: EnvelopePayload::Resp { result } }
    }

    pub fn err(id: u64, code: &str, message: impl Into<String>) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id,
            payload: EnvelopePayload::Err { error: AgentError::new(code, message) },
        }
    }

    /// 序列化成一行（不含换行符）。信封层不做大小假设：M2 的快照类大载荷
    /// 走共享内存文件，信封里只带引用——这里不做任何截断
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| {
            // 信封本身序列化失败 = 编程错误，降级成最小错误帧而不是哑掉
            format!(
                r#"{{"v":{PROTOCOL_VERSION},"id":{},"kind":"err","error":{{"code":"internal","message":"envelope serialize failed"}}}}"#,
                self.id
            )
        })
    }

    /// 从一行解析。认得出的信封返回 Ok；坏 JSON 返回 Err——调用方决定
    /// 丢弃还是报坏帧率（协议层不知道 id，回不了错误信封）
    pub fn from_line(line: &str) -> Result<Self, String> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Err("空行".into());
        }
        serde_json::from_str(trimmed).map_err(|e| format!("信封不是合法 JSON：{e}"))
    }
}

/// fencing 检查（§A3）：写回必须带着当前租约的 fence。
/// 小于 = stale run 的字节，一律拒收；等于 = 放行；大于不可能出现
/// （fence 由 Main 单调发号），出现说明有人越过了发号器，同样拒收。
/// M2 起 agent 的写回信封才带上 fence——现在由监督者的租约闸与单测钉语义
#[allow(dead_code)]
pub fn fence_admits(current: u64, presented: u64) -> bool {
    presented == current
}

/// M1 的方法面：六种里的前三种 + 流式演示（ev 通道的地基）。
/// turn.* 要等 M2 把回合循环搬进 host——届时事件面直接复用 ChatEvent
pub mod methods {
    pub const PING: &str = "ping";
    pub const ECHO: &str = "echo";
    pub const AGENT_STATUS: &str = "agent.status";
    /// 流式演示：发 count 条 `ev` 再收一条 resp 终答。它钉的是 ev 通道的
    /// 物理形状——顺序保序、ev 不终结请求、resp 才终结——M2 的 ChatEvent
    /// 透传吃的正是这套形状
    pub const STREAM_DEMO: &str = "stream.demo";
    /// 读一份配置读数（M2 第一切片）：worker 用 Main 传来的数据目录
    /// 加载真实 config.json，只回模型名——证明子进程看得见用户配置，
    /// 也证明目录传递链（Main → CLI → worker → load_from_dir）是通的。
    /// 刻意只回 model 一个字段：整份配置里有密钥，诊断面不带密钥出门
    pub const CONFIG_READ: &str = "config.read";
    /// 会话日志只读探针（M2 切片 2）：按 Main 传来的目录打开一条话题日志，
    /// 回当前分支的条目数。证明 sessions 定位链通了——turn.start 的读写
    /// 就坐在同一个 open_session_in 上
    pub const SESSION_PEEK: &str = "session.peek";
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn envelope_roundtrips_through_a_line() {
        let envelope = Envelope::req(7, "turn.start", json!({"conversationId":"c1"}));
        let line = envelope.to_line();
        assert!(!line.contains('\n'), "信封是单行帧");
        let parsed = Envelope::from_line(&line).expect("要解析回来");
        assert_eq!(parsed, envelope);
        match parsed.payload {
            EnvelopePayload::Req { method, params } => {
                assert_eq!(method, "turn.start");
                assert_eq!(params["conversationId"], "c1");
            }
            other => panic!("req 变成了 {other:?}"),
        }
    }

    #[test]
    fn kind_tag_and_camel_case_fields_survive_serde() {
        let line = envelope_resp_line_fixture();
        let parsed = Envelope::from_line(&line).expect("要解析回来");
        assert_eq!(parsed.v, PROTOCOL_VERSION);
        assert_eq!(parsed.id, 3);
        match parsed.payload {
            EnvelopePayload::Resp { result } => assert_eq!(result["pong"], true),
            other => panic!("resp 变成了 {other:?}"),
        }
    }

    fn envelope_resp_line_fixture() -> String {
        r#"{"v":1,"id":3,"kind":"resp","result":{"pong":true}}"#.into()
    }

    #[test]
    fn bad_lines_are_errors_not_panics() {
        assert!(Envelope::from_line("").is_err());
        assert!(Envelope::from_line("这不是 JSON").is_err());
        assert!(Envelope::from_line(r#"{"v":1,"id":1}"#).is_err(), "缺 kind 标签");
    }

    #[test]
    fn fence_admits_only_the_current_token() {
        assert!(fence_admits(5, 5));
        assert!(!fence_admits(5, 4), "旧 fence = stale run 的字节，拒收");
        assert!(!fence_admits(5, 6), "超过当前发号器的更不可能放");
    }
}
