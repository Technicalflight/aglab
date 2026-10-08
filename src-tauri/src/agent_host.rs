//! agent-host 的工作循环（M1 骨架）：一个独立的子进程，stdin/stdout 上跑
//! JSON Lines 信封协议。M1 的方法面只有 ping/echo/agent.status——回合循环、
//! 审批、钩子、MCP hub 按蓝图 M2/M3 逐步搬进来。
//!
//! 入口：同一个可执行文件带 `--agent-worker` 参数启动（见 lib.rs run() 的
//! 分流）。工作循环对 IO 泛型——内存缓冲可直接喂测试，不必真起进程。

use std::io::{BufRead, BufReader, Write};
use std::time::Instant;

use serde_json::json;

use crate::agent_protocol::{methods, Envelope, EnvelopePayload};

/// agent 进程的运行上下文：CLI 传进来的两样东西。
/// fence 是接管时授予的身份；data_dir 是 Main 的 app_data 目录——
/// M2 起 worker 侧的一切定位（config/会话/台账）都从它派生
#[derive(Debug, Clone)]
pub struct WorkerContext {
    pub fence: u64,
    pub data_dir: Option<std::path::PathBuf>,
    pub config_dir: Option<std::path::PathBuf>,
}

/// worker 进程的长活运行时（M3 地基）：hub 四件套在 worker 进程里自建——
/// 它们全是纯 Arc<Mutex> 结构，不需要 tauri::App。审批/插话/保温/连接池
/// 的生命周期与 worker 进程同寿，跨回合持久
pub struct WorkerRuntime {
    pub approvals: crate::approvals::ApprovalHub,
    pub steering: crate::chat::SteeringHub,
    pub warm: crate::warm::Hub,
    pub mcp: crate::mcp::Hub,
}

impl WorkerRuntime {
    fn build() -> Self {
        Self {
            approvals: crate::approvals::ApprovalHub::default(),
            steering: crate::chat::SteeringHub::default(),
            warm: crate::warm::Hub::default(),
            mcp: crate::mcp::Hub::default(),
        }
    }
}

/// agent 进程的自报家门：status 方法与诊断命令共用这一份读数
#[derive(Debug, Clone, PartialEq)]
pub struct AgentStatus {
    pub pid: u32,
    pub uptime_secs: u64,
    /// 本进程被授予的 fencing token（接管/重启时由 Main 侧 +1 重新授予）
    pub fence: u64,
    /// 这一进程活着的期间处理过的请求帧数（诊断用：坏帧率与吞吐的对账基数）
    pub served: u64,
    /// Main 传来的数据目录在不在（config.read 的前提）
    pub has_data_dir: bool,
}

/// 工作循环：逐行读信封、分发、逐行写回。EOF（Main 关了管道）即正常退场。
/// 任何一行解析失败都只是坏一帧，循环不断——护栏挂在编码/脏字节上
/// 等于整条 IPC 断掉（hooks 的同一条教训）
pub fn run_stdio_loop<R: std::io::Read, W: std::io::Write>(
    input: R,
    mut output: W,
    context: WorkerContext,
) -> Result<(), String> {
    let started = Instant::now();
    let reader = BufReader::new(input);
    let mut served: u64 = 0;
    for line in reader.lines() {
        let line = line.map_err(|e| format!("读 stdin 失败：{e}"))?;
        let envelope = match Envelope::from_line(&line) {
            Ok(envelope) => envelope,
            Err(_reason) => {
                // 坏帧没有可信的 id，回不了定向错误信封：计数后丢弃。
                // 坏帧率对账靠 agent.status 的 served 读数
                continue;
            }
        };
        // served 只数真正的请求帧：回程形状跑错了入口、坏 JSON 都不算
        if matches!(envelope.payload, EnvelopePayload::Req { .. }) {
            served += 1;
        }
        let runtime = WorkerRuntime::build();
        dispatch(envelope.id, &envelope.payload, &context, &runtime, started, served, &mut output)?;
    }
    Ok(())
}

/// 单帧分发。req 才有回答；resp/ev/err 跑进 agent 的入口本身就是协议错。
/// 拿着 output 是因为流式方法（stream.demo）要连发多条 ev 再收 resp——
/// 单信封返回值装不下"一问多答"，M2 的 ChatEvent 透传同款
fn dispatch<W: Write>(
    id: u64,
    payload: &EnvelopePayload,
    context: &WorkerContext,
    runtime: &WorkerRuntime,
    started: Instant,
    served: u64,
    output: &mut W,
) -> Result<(), String> {
    let fence = context.fence;
    let EnvelopePayload::Req { method, params } = payload else {
        return write_line(
            output,
            &Envelope::err(
                id,
                "not_a_request",
                "agent 入口只收 req 信封；resp/ev/err 是回程的形状。",
            ),
        );
    };
    match method.as_str() {
        methods::PING => write_line(output, &Envelope::resp(id, json!({"pong": true, "v": 1}))),
        methods::ECHO => write_line(output, &Envelope::resp(id, params.clone())),
        methods::STREAM_DEMO => {
            let count = params["count"].as_u64().unwrap_or(3).min(10);
            let prefix = params["prefix"].as_str().unwrap_or("tick").to_string();
            for index in 0..count {
                let envelope = Envelope {
                    v: 1,
                    id,
                    payload: EnvelopePayload::Ev {
                        event: prefix.clone(),
                        data: json!({ "i": index }),
                    },
                };
                write_line(output, &envelope)?;
            }
            write_line(output, &Envelope::resp(id, json!({ "delivered": count })))
        }
        methods::CONFIG_READ => {
            // M2 第一切片的验收方法：目录链通了，这里回的就是用户真实模型名。
            // 刻意只回 model：整份配置里有密钥，诊断面不带密钥出门
            let Some(config_dir) = &context.config_dir else {
                return write_line(
                    output,
                    &Envelope::err(id, "no_data_dir", "Main 没传来配置目录，worker 无法定位配置。"),
                );
            };
            let config = crate::config::load_from_dir(config_dir);
            write_line(output, &Envelope::resp(id, json!({ "model": config.model })))
        }
        methods::STORAGE_PROBE => {
            // M2 切片 3 的验收方法：usage.db 的目录链通了（打开 + SCHEMA + 数行），
            // 审计目录的存在性顺手报出——record 本来就是路径参数制
            let Some(config_dir) = &context.config_dir else {
                return write_line(
                    output,
                    &Envelope::err(id, "no_config_dir", "Main 没传来配置目录，worker 无法打开用量台账。"),
                );
            };
            let requests = crate::usage::open_in(config_dir)
                .and_then(|conn| {
                    conn.query_row("SELECT COUNT(*) FROM requests", rusqlite::params![], |row| row.get::<_, i64>(0))
                        .map_err(|e| e.to_string())
                })
                .unwrap_or(-1);
            let audit_dir = context
                .data_dir
                .as_ref()
                .map(|dir| dir.join("audit").is_dir())
                .unwrap_or(false);
            write_line(
                output,
                &Envelope::resp(id, json!({ "usageRequests": requests, "auditDirPresent": audit_dir })),
            )
        }
        methods::PLUGINS_COUNT => {
            // M2 切片 4 的验收方法：插件名册（config 过滤后）与可运行钩子
            // 都能从传入目录派生——M3 的钩子发射坐在同一条链上
            let Some(config_dir) = &context.config_dir else {
                return write_line(
                    output,
                    &Envelope::err(id, "no_config_dir", "Main 没传来配置目录。"),
                );
            };
            let Some(data_dir) = &context.data_dir else {
                return write_line(
                    output,
                    &Envelope::err(id, "no_data_dir", "Main 没传来数据目录。"),
                );
            };
            let config = crate::config::load_from_dir(config_dir);
            let plugins = crate::plugins::enabled_in(&config, data_dir);
            let hooks = crate::hooks::runnable_in(&config, data_dir);
            write_line(
                output,
                &Envelope::resp(
                    id,
                    json!({ "plugins": plugins.len(), "runnableHooks": hooks.len() }),
                ),
            )
        }
        methods::HUBS_CHECK => {
            // M3 地基的验收方法：hub 四件套在 worker 里活着、MCP 连接计数可读。
            // turn.start 的组装坐在这些实例上——不再需要 tauri::State
            write_line(
                output,
                &Envelope::resp(
                    id,
                    json!({
                        "approvals": true,
                        "steering": true,
                        "warm": true,
                        "mcpConnected": runtime.mcp.connected().len(),
                    }),
                ),
            )
        }
        methods::TOOL_DECIDE => {
            // M3 审批闭环的回程腿：决定送到 worker 的 ApprovalHub。
            // requestId 就是 turn.start 途中 approval_request 事件带的那个
            let request_id = params["requestId"].as_str().unwrap_or_default();
            if request_id.is_empty() {
                return write_line(output, &Envelope::err(id, "bad_params", "params.requestId 缺了。"));
            }
            let approved = params["approved"].as_bool().unwrap_or(false);
            runtime.approvals.resolve(request_id, approved);
            write_line(output, &Envelope::resp(id, json!({ "resolved": request_id })))
        }
        methods::STEER_PUSH => {
            // 插话进队：返回队列长度（诊断可见），插话内容在下一轮请求前拼进上下文
            let conversation_id = params["conversationId"].as_str().unwrap_or_default();
            let text = params["text"].as_str().unwrap_or_default();
            if conversation_id.is_empty() || text.is_empty() {
                return write_line(output, &Envelope::err(id, "bad_params", "params.conversationId 与 text 都不能为空。"));
            }
            match runtime.steering.push(conversation_id, text) {
                Ok(queued) => write_line(output, &Envelope::resp(id, json!({ "queued": queued }))),
                Err(message) => write_line(output, &Envelope::err(id, "steer_failed", message)),
            }
        }
        methods::TURN_ONCE => {
            // M3 主体第一刀：worker 里跑一轮**真实模型请求**。
            // 依赖全部就位：config（load_from_dir）/ 密钥（api_key→keyring，同进程同用户）/
            // 请求核心（request_round 本就不碰 AppHandle）。事件面第一次真跑：
            // ChatEvent serde 后逐条 ev("chat") 透传，终答 resp {text}
            let Some(config_dir) = &context.config_dir else {
                return write_line(
                    output,
                    &Envelope::err(id, "no_config_dir", "Main 没传来配置目录。"),
                );
            };
            let prompt = params["prompt"].as_str().unwrap_or_default();
            if prompt.trim().is_empty() {
                return write_line(output, &Envelope::err(id, "bad_params", "params.prompt 缺了。"));
            }
            let config = crate::config::load_from_dir(config_dir);
            if config.base_url.trim().is_empty() {
                return write_line(output, &Envelope::err(id, "no_provider", "配置里没有服务商地址。"));
            }
            let key = match crate::config::api_key(&config) {
                Ok(key) => key,
                Err(error) => {
                    return write_line(
                        output,
                        &Envelope::err(id, "no_credentials", format!("密钥解析失败：{error}")),
                    )
                }
            };
            let thread = json!([{ "role": "user", "content": prompt }])
                .as_array()
                .cloned()
                .unwrap_or_default();
            let stop = std::sync::atomic::AtomicBool::new(false);
            // ChatEvent → ev 透传：流内事件按序出站，ev 不终结请求
            let mut sink = |event: crate::chat::ChatEvent| {
                let envelope = Envelope {
                    v: 1,
                    id,
                    payload: EnvelopePayload::Ev {
                        event: "chat".into(),
                        data: serde_json::to_value(event).unwrap_or_default(),
                    },
                };
                let _ = write_line(output, &envelope);
            };
            match crate::chat::request_round(
                &config,
                &key,
                &thread,
                &[],
                None,
                &stop,
                &mut sink,
            ) {
                Ok(outcome) => write_line(output, &Envelope::resp(id, json!({ "text": outcome.text() }))),
                Err(failure) => write_line(
                    output,
                    &Envelope::err(id, "turn_failed", failure.message()),
                ),
            }
        }
        methods::SESSION_PEEK => {
            // M2 切片 2 的验收方法：sessions 定位链通了，这里回当前分支条目数。
            let Some(config_dir) = &context.config_dir else {
                return write_line(
                    output,
                    &Envelope::err(id, "no_config_dir", "Main 没传来配置目录，worker 无法定位会话。"),
                );
            };
            let Some(data_dir) = &context.data_dir else {
                return write_line(
                    output,
                    &Envelope::err(id, "no_data_dir", "Main 没传来数据目录，worker 无法定位台账。"),
                );
            };
            let conversation_id = params["conversationId"].as_str().unwrap_or_default();
            if conversation_id.trim().is_empty() {
                return write_line(
                    output,
                    &Envelope::err(id, "bad_params", "params.conversationId 缺了。"),
                );
            }
            match crate::chat::open_session_in(config_dir, data_dir, conversation_id) {
                Ok(session) => {
                    // 当前分支的条目投影（沿父链到根）；空日志/新话题 = 0
                    let entries = session.log.path().map(|entries| entries.len()).unwrap_or(0);
                    write_line(output, &Envelope::resp(id, json!({ "entries": entries })))
                }
                Err(message) => write_line(output, &Envelope::err(id, "session_open_failed", message)),
            }
        }
        methods::AGENT_STATUS => {
            let status = AgentStatus {
                pid: std::process::id(),
                uptime_secs: started.elapsed().as_secs(),
                fence,
                served,
                has_data_dir: context.data_dir.is_some(),
            };
            write_line(
                output,
                &Envelope::resp(
                    id,
                    json!({
                        "pid": status.pid,
                        "uptimeSecs": status.uptime_secs,
                        "fence": status.fence,
                        "served": status.served,
                        "hasDataDir": status.has_data_dir,
                    }),
                ),
            )
        }
        other => write_line(
            output,
            &Envelope::err(
                id,
                "unknown_method",
                format!("方法「{other}」在协议 v1 里不存在。"),
            ),
        ),
    }
}

fn write_line<W: Write>(output: &mut W, envelope: &Envelope) -> Result<(), String> {
    output
        .write_all(envelope.to_line().as_bytes())
        .and_then(|_| output.write_all(b"\n"))
        .and_then(|_| output.flush())
        .map_err(|e| format!("写 stdout 失败：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_protocol::fence_admits;
    use std::io::Cursor;

    /// 内存往返：喂几行进去，收的回程行逐条对上
    #[test]
    fn the_worker_loop_answers_ping_echo_status_and_rejects_unknown_methods() {
        let input = Cursor::new(
            concat!(
                r#"{"v":1,"id":1,"kind":"req","method":"ping","params":{}}"#, "\n",
                r#"{"v":1,"id":2,"kind":"req","method":"echo","params":{"hi":"你 好"}}"#, "\n",
                "\n", // 空行：无害噪声
                "这不是 JSON\n", // 坏帧：跳过不断循环
                r#"{"v":1,"id":3,"kind":"req","method":"agent.status","params":{}}"#, "\n",
                r#"{"v":1,"id":4,"kind":"req","method":"turn.start","params":{}}"#, "\n",
                r#"{"v":1,"id":5,"kind":"resp","result":{}}"#, "\n", // 回程形状进请求入口 = 协议错
            ),
        );
        let mut output: Vec<u8> = Vec::new();
        run_stdio_loop(input, &mut output, WorkerContext { fence: 1, data_dir: None, config_dir: None }).expect("循环要干净退场");

        let text = String::from_utf8(output).expect("回程是 UTF-8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 5, "ping/echo/status/未知方法/非请求 各一帧回");

        let reply = |index: usize| Envelope::from_line(lines[index]).expect("回程行要合法");
        match reply(0).payload {
            EnvelopePayload::Resp { result } => assert_eq!(result["pong"], true),
            other => panic!("ping 的回答变成了 {other:?}"),
        }
        match reply(1).payload {
            EnvelopePayload::Resp { result } => assert_eq!(result["hi"], "你 好"),
            other => panic!("echo 的回答变成了 {other:?}"),
        }
        match reply(2).payload {
            EnvelopePayload::Resp { result } => {
                assert_eq!(result["fence"], 1, "status 报出本进程的 fence");
                assert_eq!(result["served"], 3, "served 只数请求帧，且回答时含当帧");
            }
            other => panic!("status 的回答变成了 {other:?}"),
        }
        match reply(3).payload {
            EnvelopePayload::Err { error } => {
                assert_eq!(error.code, "unknown_method", "M1 还没搬 turn.*，点了就是明确说不存在");
            }
            other => panic!("未知方法的回答变成了 {other:?}"),
        }
        match reply(4).payload {
            EnvelopePayload::Err { error } => assert_eq!(error.code, "not_a_request"),
            other => panic!("回程形状进请求入口的回答变成了 {other:?}"),
        }
    }

    #[test]
    fn eof_is_a_clean_exit_not_an_error() {
        let result = run_stdio_loop(Cursor::new(""), Vec::new(), WorkerContext { fence: 1, data_dir: None, config_dir: None });
        assert!(result.is_ok(), "Main 关管道 = 正常退场");
    }

    #[test]
    fn fence_grants_flow_into_the_status_reading() {
        let input = Cursor::new(r#"{"v":1,"id":9,"kind":"req","method":"agent.status"}"#);
        let mut output: Vec<u8> = Vec::new();
        run_stdio_loop(input, &mut output, WorkerContext { fence: 41, data_dir: None, config_dir: None }).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(
            text.contains(r#""fence":41"#),
            "接管时发的 fence 要原样出现在 status 里：{text}"
        );
        assert!(fence_admits(41, 41));
    }



    #[test]
    fn storage_probe_counts_usage_rows_from_the_passed_config_dir() {
        use crate::test_support::temp_dir;
        let base = temp_dir("agent-storage-probe");
        std::fs::create_dir_all(&base).unwrap();

        // 空库：SCHEMA 建好后 requests = 0
        let input = Cursor::new(r#"{"v":1,"id":1,"kind":"req","method":"storage.probe"}"#);
        let mut output: Vec<u8> = Vec::new();
        run_stdio_loop(
            input,
            &mut output,
            WorkerContext { fence: 1, data_dir: Some(base.clone()), config_dir: Some(base.clone()) },
        )
        .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains(r#""usageRequests":0"#), "空库零行：{text}");

        // 主进程侧写一笔（模拟 usage::record 已发生），worker 再读 = 1
        let conn = crate::usage::open_in(&base).unwrap();
        conn.execute(
            "INSERT INTO requests (ts, model) VALUES (?1, ?2)",
            rusqlite::params![crate::session::now_millis(), "m"],
        )
        .unwrap_or_else(|e| panic!("测试插行只填必填列：{e}"));
        drop(conn);

        let input = Cursor::new(r#"{"v":1,"id":2,"kind":"req","method":"storage.probe"}"#);
        let mut output: Vec<u8> = Vec::new();
        run_stdio_loop(
            input,
            &mut output,
            WorkerContext { fence: 1, data_dir: Some(base.clone()), config_dir: Some(base.clone()) },
        )
        .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(
            text.contains(r#""usageRequests":1"#),
            "worker 读到的是主进程写的同一份库：{text}"
        );
        crate::test_support::remove_tree(&base);
    }



    #[test]
    fn tool_decide_and_steer_push_speak_the_protocol_shapes() {
        let input = Cursor::new(
            concat!(
                r#"{"v":1,"id":1,"kind":"req","method":"tool.decide","params":{"requestId":"req-1","approved":true}}"#, "\n",
                r#"{"v":1,"id":2,"kind":"req","method":"steer.push","params":{"conversationId":"ghost","text":"插话"}}"#, "\n",
            ),
        );
        let mut output: Vec<u8> = Vec::new();
        run_stdio_loop(
            input,
            &mut output,
            WorkerContext { fence: 1, data_dir: None, config_dir: None },
        )
        .expect("循环干净退场");
        let text = String::from_utf8(output).expect("回程是 UTF-8");
        let lines: Vec<Envelope> = text.lines().map(Envelope::from_line).collect::<Result<_, _>>().expect("每行合法信封");

        // tool.decide 对未知 requestId 也是合法回执（resolve 是幂等投递）
        match &lines[0].payload {
            EnvelopePayload::Resp { result } => {
                assert_eq!(result["resolved"], "req-1", "tool.decide 回程：{text}");
            }
            other => panic!("tool.decide 的回答变成了 {other:?}"),
        }
        // steer.push 对不存在的回合诚实报错（SteeringHub 的"条目不存在"语义）：
        // 插话的降级发送是前端的事，协议层不许假装排上了
        match &lines[1].payload {
            EnvelopePayload::Err { error } => {
                assert_eq!(error.code, "steer_failed", "steer.push 回程：{text}");
            }
            other => panic!("steer.push 的回答变成了 {other:?}"),
        }
    }


    #[test]
    fn turn_once_validates_before_touching_the_network() {
        let base = crate::test_support::temp_dir("agent-turn-once");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("config.json"), r#"{"model":"甲","base_url":""}"#).unwrap();
        let context = WorkerContext { fence: 1, data_dir: Some(base.clone()), config_dir: Some(base.clone()) };

        // 空 prompt：参数闸先挡
        let input = Cursor::new(r#"{"v":1,"id":1,"kind":"req","method":"turn.once","params":{"prompt":"  "}}"#);
        let mut output: Vec<u8> = Vec::new();
        run_stdio_loop(input, &mut output, context.clone()).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains(r#""code":"bad_params""#), "{text}");

        // 没配服务商地址：不碰网络，明确报 no_provider
        let input = Cursor::new(r#"{"v":1,"id":2,"kind":"req","method":"turn.once","params":{"prompt":"你好"}}"#);
        let mut output: Vec<u8> = Vec::new();
        run_stdio_loop(input, &mut output, context).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains(r#""code":"no_provider""#), "{text}");
        crate::test_support::remove_tree(&base);
    }

    #[test]
    fn hubs_check_proves_the_runtime_lives_in_the_worker() {
        let input = Cursor::new(r#"{"v":1,"id":1,"kind":"req","method":"hubs.check"}"#);
        let mut output: Vec<u8> = Vec::new();
        run_stdio_loop(
            input,
            &mut output,
            WorkerContext { fence: 1, data_dir: None, config_dir: None },
        )
        .unwrap();
        let text = String::from_utf8(output).unwrap();
        let envelope = Envelope::from_line(text.trim()).unwrap();
        match envelope.payload {
            EnvelopePayload::Resp { result } => {
                assert_eq!(result["approvals"], true, "审批 hub 在 worker 里自建成功");
                assert_eq!(result["steering"], true);
                assert_eq!(result["warm"], true);
                assert!(result["mcpConnected"].is_u64(), "MCP 连接计数可读");
            }
            other => panic!("hubs.check 的回答变成了 {other:?}"),
        }
    }

    #[test]
    fn config_read_answers_the_real_model_and_demands_a_data_dir() {
        use crate::test_support::temp_dir;
        // 没传目录：明确报错，不猜
        let input = Cursor::new(r#"{"v":1,"id":1,"kind":"req","method":"config.read"}"#);
        let mut output: Vec<u8> = Vec::new();
        run_stdio_loop(input, &mut output, WorkerContext { fence: 1, data_dir: None, config_dir: None }).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains(r#""code":"no_data_dir""#), "{text}");

        // 传了目录：读到的就是那份 config.json 里的 model
        let base = temp_dir("agent-config-read");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("config.json"), r#"{"model":"配置甲"}"#).unwrap();
        let input = Cursor::new(r#"{"v":1,"id":2,"kind":"req","method":"config.read"}"#);
        let mut output: Vec<u8> = Vec::new();
        run_stdio_loop(
            input,
            &mut output,
            WorkerContext { fence: 1, data_dir: Some(base.clone()), config_dir: Some(base.clone()) },
        )
        .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(
            text.contains(r#""model":"配置甲""#),
            "worker 读到的是用户真实配置：{text}"
        );
        // 诊断面不带密钥出门：config.read 的回程只有 model 一个字段
        let envelope = Envelope::from_line(text.trim()).unwrap();
        match envelope.payload {
            EnvelopePayload::Resp { result } => assert_eq!(
                result.as_object().map(|map| map.len()),
                Some(1),
                "回程只许有 model 一个字段"
            ),
            other => panic!("config.read 的回答变成了 {other:?}"),
        }
        crate::test_support::remove_tree(&base);
    }

    #[test]
    fn stream_demo_emits_events_in_order_then_a_terminating_resp() {        let input = Cursor::new(
            r#"{"v":1,"id":7,"kind":"req","method":"stream.demo","params":{"count":3,"prefix":"tick"}}"#,
        );
        let mut output: Vec<u8> = Vec::new();
        run_stdio_loop(input, &mut output, WorkerContext { fence: 1, data_dir: None, config_dir: None }).expect("循环干净退场");

        let text = String::from_utf8(output).expect("回程是 UTF-8");
        let lines: Vec<Envelope> = text.lines().map(Envelope::from_line).collect::<Result<_, _>>().expect("每行都是合法信封");
        assert_eq!(lines.len(), 4, "3 条 ev + 1 条终答");
        for (index, envelope) in lines.iter().take(3).enumerate() {
            assert_eq!(envelope.id, 7, "ev 与请求同 id：等待方靠它归组");
            match &envelope.payload {
                EnvelopePayload::Ev { event, data } => {
                    assert_eq!(event, "tick");
                    assert_eq!(data["i"], index as u64, "事件保序");
                }
                other => panic!("第 {index} 帧该是 ev，结果是 {other:?}"),
            }
        }
        match &lines[3].payload {
            EnvelopePayload::Resp { result } => {
                assert_eq!(result["delivered"], 3, "resp 才是终结帧");
            }
            other => panic!("最后一帧该是 resp，结果是 {other:?}"),
        }
    }
}
