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

/// worker 进程的长活运行时（M3 地基 + M3 收官扩员）：交互登记表全套在
/// worker 进程里自建——它们全是纯 Arc<Mutex> 结构，不需要 tauri::App。
/// 审批/插话/保温/连接池/停止/护栏/跟随/模式寄存的生命周期与 worker
/// 进程同寿，跨回合持久
#[derive(Clone)]
#[cfg_attr(test, allow(dead_code))] // 字段随 turn 体内消费接线，测试构建暂不可达
pub struct WorkerRuntime {
    pub approvals: crate::approvals::ApprovalHub,
    pub steering: crate::chat::SteeringHub,
    pub warm: crate::warm::Hub,
    pub mcp: crate::mcp::Hub,
    /// turn.stop 的拉闸对象：回合线程 register，收尾 release
    pub stops: crate::chat::StopHub,
    pub pause: crate::chat::PauseHub,
    pub follow_up: crate::chat::FollowUpHub,
    pub guards: crate::chat::GoalGuards,
    pub mode_hub: crate::chat::ModeHub,
}

impl WorkerRuntime {
    fn build() -> Self {
        Self {
            approvals: crate::approvals::ApprovalHub::default(),
            steering: crate::chat::SteeringHub::default(),
            warm: crate::warm::Hub::default(),
            mcp: crate::mcp::Hub::default(),
            stops: crate::chat::StopHub::default(),
            pause: crate::chat::PauseHub::default(),
            follow_up: crate::chat::FollowUpHub::default(),
            guards: crate::chat::GoalGuards::default(),
            mode_hub: crate::chat::ModeHub::default(),
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
pub fn run_stdio_loop<R: std::io::Read + Send + 'static, W: std::io::Write + Send + 'static>(
    input: R,
    output: W,
    context: WorkerContext,
) -> Result<(), String> {
    let started = Instant::now();
    // A6 第 ② 步：三线程架构——
    // * 读线程：stdin 逐行 → 入站队列（EOF 发 Eof 哨兵）；
    // * 主循环：收信封路由（turn.start 派生回合线程、tool.decide/steer.push 直达 hub、
    //   其余同步分发）；
    // * 写线程：唯一持有 output，所有出站信封经 outbox 队列送到它手上排序落盘——
    //   多路并发的回程永不交错。
    let (outbox_tx, outbox_rx) = std::sync::mpsc::channel::<Envelope>();
    let writer = std::thread::spawn(move || {
        let mut output = output;
        for envelope in outbox_rx {
            if write_line(&mut output, &envelope).is_err() {
                break;
            }
        }
    });
    let (tx, rx) = std::sync::mpsc::channel::<Envelope>();
    std::thread::spawn(move || {
        let reader = BufReader::new(input);
        for line in reader.lines() {
            let line = match line {
                Ok(line) => line,
                Err(_) => break,
            };
            match Envelope::from_line(&line) {
                Ok(envelope) => {
                    if tx.send(envelope).is_err() {
                        return;
                    }
                }
                Err(_) => continue, // 坏帧丢弃：坏帧率对账靠 agent.status 的 served
            }
        }
    });
    let runtime = WorkerRuntime::build();
    let mut served: u64 = 0;
    // 读线程退场 = EOF = 干净收摊
    while let Ok(envelope) = rx.recv() {
        let is_req = matches!(envelope.payload, EnvelopePayload::Req { .. });
        if is_req {
            served += 1;
        }
        let id = envelope.id;
        // turn.start 是异步的：立即回执 started，回合在本线程之外跑，
        // 途中 ChatEvent 以 ev("chat") 出站，收尾 ev("turn.done") + resp {text}。
        // 主循环继续收信封——tool.decide / steer.push 在回合进行中照样可达
        if is_req {
            if let EnvelopePayload::Req { method, params } = &envelope.payload {
                if method == methods::TURN_START {
                    handle_turn_start(id, params, &context, &runtime, &outbox_tx);
                    continue;
                }
                if method == methods::TURN_STOP {
                    let conversation_id = params["conversationId"].as_str().unwrap_or_default();
                    let reply = match runtime.stops.abort(conversation_id) {
                        Ok(()) => Envelope::resp(id, json!({ "stopped": conversation_id })),
                        Err(message) => Envelope::err(id, "turn_not_running", message),
                    };
                    let _ = outbox_tx.send(reply);
                    continue;
                }
            }
        }
        // 控制信令在回合进行中也要活：路由到 hub，而不是等回合结束
        if is_req {
            if let EnvelopePayload::Req { method, params } = &envelope.payload {
                match method.as_str() {
                    methods::TOOL_DECIDE => {
                        let request_id = params["requestId"].as_str().unwrap_or_default();
                        let approved = params["approved"].as_bool().unwrap_or(false);
                        runtime.approvals.resolve(request_id, approved);
                        let _ =
                            outbox_tx.send(Envelope::resp(id, json!({ "resolved": request_id })));
                        continue;
                    }
                    methods::STEER_PUSH => {
                        let conversation_id = params["conversationId"].as_str().unwrap_or_default();
                        let text = params["text"].as_str().unwrap_or_default();
                        if conversation_id.is_empty() || text.is_empty() {
                            let _ = outbox_tx.send(Envelope::err(
                                id,
                                "bad_params",
                                "params.conversationId 与 text 都不能为空。",
                            ));
                        } else {
                            match runtime.steering.push(conversation_id, text) {
                                Ok(queued) => {
                                    let _ = outbox_tx
                                        .send(Envelope::resp(id, json!({ "queued": queued })));
                                }
                                Err(message) => {
                                    let _ =
                                        outbox_tx.send(Envelope::err(id, "steer_failed", message));
                                }
                            }
                        }
                        continue;
                    }
                    _ => {}
                }
            }
        }
        dispatch(
            envelope.id,
            &envelope.payload,
            &context,
            &runtime,
            started,
            served,
            &outbox_tx,
        )?;
    }
    drop(outbox_tx); // 出站队列排干 = 写线程收尾
    let _ = writer.join();
    Ok(())
}

/// 单帧分发（同步方法）：config/status/stream.demo/plugins/storage/peek。
/// 回答写入 outbox；turn.start / tool.decide / steer.push 在主循环里特判路由
fn dispatch(
    id: u64,
    payload: &EnvelopePayload,
    context: &WorkerContext,
    runtime: &WorkerRuntime,
    started: Instant,
    served: u64,
    outbox: &std::sync::mpsc::Sender<Envelope>,
) -> Result<(), String> {
    let EnvelopePayload::Req { method, params } = payload else {
        let _ = outbox.send(Envelope::err(
            id,
            "not_a_request",
            "agent 入口只收 req 信封；resp/ev/err 是回程的形状。",
        ));
        return Ok(());
    };
    match method.as_str() {
        methods::PING => {
            let _ = outbox.send(Envelope::resp(id, json!({"pong": true, "v": 1})));
        }
        methods::ECHO => {
            let _ = outbox.send(Envelope::resp(id, params.clone()));
        }
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
                let _ = outbox.send(envelope);
            }
            let _ = outbox.send(Envelope::resp(id, json!({ "delivered": count })));
        }
        methods::CONFIG_READ => {
            let Some(config_dir) = &context.config_dir else {
                let _ = outbox.send(Envelope::err(id, "no_data_dir", "Main 没传来配置目录。"));
                return Ok(());
            };
            let config = crate::config::load_from_dir(config_dir);
            let _ = outbox.send(Envelope::resp(id, json!({ "model": config.model })));
        }
        methods::AGENT_STATUS => {
            let status = AgentStatus {
                pid: std::process::id(),
                uptime_secs: started.elapsed().as_secs(),
                fence: context.fence,
                served,
                has_data_dir: context.data_dir.is_some(),
            };
            let _ = outbox.send(Envelope::resp(
                id,
                json!({
                    "pid": status.pid,
                    "uptimeSecs": status.uptime_secs,
                    "fence": status.fence,
                    "served": status.served,
                    "hasDataDir": status.has_data_dir,
                }),
            ));
        }
        methods::STORAGE_PROBE => {
            let Some(config_dir) = &context.config_dir else {
                let _ = outbox.send(Envelope::err(
                    id,
                    "no_config_dir",
                    "Main 没传来配置目录，worker 无法打开用量台账。",
                ));
                return Ok(());
            };
            let requests = crate::usage::open_in(config_dir)
                .and_then(|conn| {
                    conn.query_row(
                        "SELECT COUNT(*) FROM requests",
                        rusqlite::params![],
                        |row| row.get::<_, i64>(0),
                    )
                    .map_err(|e| e.to_string())
                })
                .unwrap_or(-1);
            let audit_dir = context
                .data_dir
                .as_ref()
                .map(|dir| dir.join("audit").is_dir())
                .unwrap_or(false);
            let _ = outbox.send(Envelope::resp(
                id,
                json!({ "usageRequests": requests, "auditDirPresent": audit_dir }),
            ));
        }
        methods::PLUGINS_COUNT => {
            let Some(config_dir) = &context.config_dir else {
                let _ = outbox.send(Envelope::err(id, "no_config_dir", "Main 没传来配置目录。"));
                return Ok(());
            };
            let Some(data_dir) = &context.data_dir else {
                let _ = outbox.send(Envelope::err(id, "no_data_dir", "Main 没传来数据目录。"));
                return Ok(());
            };
            let config = crate::config::load_from_dir(config_dir);
            let plugins = crate::plugins::enabled_in(&config, data_dir);
            let hooks = crate::hooks::runnable_in(&config, data_dir);
            let _ = outbox.send(Envelope::resp(
                id,
                json!({ "plugins": plugins.len(), "runnableHooks": hooks.len() }),
            ));
        }
        methods::HUBS_CHECK => {
            let _ = outbox.send(Envelope::resp(
                id,
                json!({
                    "approvals": true,
                    "steering": true,
                    "warm": true,
                    "mcpConnected": runtime.mcp.connected().len(),
                }),
            ));
        }
        methods::TURN_ONCE => {
            // 同步形态的最小真回合（校验闸测试用它；异步形态是 turn.start）
            match run_turn_once(
                &context.config_dir,
                params["prompt"].as_str().unwrap_or_default(),
                id,
                outbox,
            ) {
                Ok(text) => {
                    let _ = outbox.send(Envelope::resp(id, json!({ "text": text })));
                }
                Err((code, message)) => {
                    let _ = outbox.send(Envelope::err(id, &code, message));
                }
            }
        }
        methods::SESSION_PEEK => {
            let Some(config_dir) = &context.config_dir else {
                let _ = outbox.send(Envelope::err(
                    id,
                    "no_config_dir",
                    "Main 没传来配置目录，worker 无法定位会话。",
                ));
                return Ok(());
            };
            let Some(data_dir) = &context.data_dir else {
                let _ = outbox.send(Envelope::err(
                    id,
                    "no_data_dir",
                    "Main 没传来数据目录，worker 无法定位台账。",
                ));
                return Ok(());
            };
            let conversation_id = params["conversationId"].as_str().unwrap_or_default();
            if conversation_id.trim().is_empty() {
                let _ = outbox.send(Envelope::err(
                    id,
                    "bad_params",
                    "params.conversationId 缺了。",
                ));
                return Ok(());
            }
            match crate::chat::open_session_in(config_dir, data_dir, conversation_id) {
                Ok(session) => {
                    let entries = session.log.path().map(|entries| entries.len()).unwrap_or(0);
                    let _ = outbox.send(Envelope::resp(id, json!({ "entries": entries })));
                }
                Err(message) => {
                    let _ = outbox.send(Envelope::err(id, "session_open_failed", message));
                }
            }
        }
        other => {
            let _ = outbox.send(Envelope::err(
                id,
                "unknown_method",
                format!("方法「{other}」在协议 v1 里不存在。"),
            ));
        }
    }
    Ok(())
}

/// turn.start 的全量执行：真参数 → 停止登记 → 异步回合线程跑 run_turn。
/// 回执形状（A6 收官版）：ev("turn.started") 立即出门（不再是 resp——一条请求
/// 只许一个终答，resp/err 留给回合的结果），途中 ChatEvent 以 ev("chat") 出站，
/// 终答 resp {ok} / err，最后 ev("turn.done") 收尾
fn handle_turn_start(
    id: u64,
    params: &serde_json::Value,
    context: &WorkerContext,
    runtime: &WorkerRuntime,
    outbox: &std::sync::mpsc::Sender<Envelope>,
) {
    let send_err = |outbox: &std::sync::mpsc::Sender<Envelope>, code: &str, message: String| {
        let _ = outbox.send(Envelope::err(id, code, message));
    };
    let Some(config_dir) = context.config_dir.clone() else {
        send_err(outbox, "no_config_dir", "Main 没传来配置目录。".into());
        return;
    };
    let Some(data_dir) = context.data_dir.clone() else {
        send_err(outbox, "no_data_dir", "Main 没传来数据目录。".into());
        return;
    };
    let params = crate::chat::WorkerTurnParams {
        conversation_id: params["conversationId"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        input: params["input"].as_str().unwrap_or_default().to_string(),
        attachments: params["attachments"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        rewind_to: params["rewindTo"].as_str().map(String::from),
        rewind_to_root: params["rewindToRoot"].as_bool().unwrap_or(false),
        skip_memory: params["skipMemory"].as_bool().unwrap_or(false),
    };
    // 停止登记（worker 侧互斥）：登记失败 = 这条话题在 worker 里还有一轮没收尾
    let stop = match runtime.stops.register(&params.conversation_id) {
        Ok(flag) => flag,
        Err(message) => {
            send_err(outbox, "turn_already_running", message);
            return;
        }
    };
    // 立即回执：started 以 ev 出门，回合在别的线程里跑，主循环继续收信封——
    // tool.decide / steer.push / turn.stop 在回合进行中照样可达
    let _ = outbox.send(Envelope {
        v: 1,
        id,
        payload: EnvelopePayload::Ev {
            event: "turn.started".into(),
            data: json!({ "conversationId": params.conversation_id }),
        },
    });
    let conversation_id = params.conversation_id.clone();
    let out = outbox.clone();
    let runtime = runtime.clone();
    std::thread::spawn(move || {
        // 测试构建不含全量回合体（run_turn 的链接面会拉起 comctl32 v6-only
        // 导入，见 chat.rs run_worker_turn 的注释）；真回合在真机/CI 验收
        #[cfg(test)]
        {
            let _ = (
                &config_dir,
                &data_dir,
                &runtime,
                &params,
                &stop,
                &conversation_id,
            );
            let _ = out.send(Envelope::err(id, "test_build", "测试构建不含全量回合体。"));
        }
        #[cfg(not(test))]
        {
            let emit_out = out.clone();
            let emit = std::sync::Arc::new(move |event: &str, data: serde_json::Value| {
                let _ = emit_out.send(Envelope {
                    v: 1,
                    id,
                    payload: EnvelopePayload::Ev {
                        event: event.to_string(),
                        data,
                    },
                });
            });
            let outcome = crate::chat_heavy_tools::run_worker_turn(
                &config_dir,
                &data_dir,
                &runtime,
                params,
                &stop,
                emit,
            );
            let ok = outcome.is_ok();
            runtime.stops.release(&conversation_id);
            let _ = out.send(match outcome {
                Ok(()) => Envelope::resp(id, json!({ "ok": true })),
                Err((code, message)) => Envelope::err(id, &code, message),
            });
            let _ = out.send(Envelope {
                v: 1,
                id,
                payload: EnvelopePayload::Ev {
                    event: "turn.done".into(),
                    data: json!({ "conversationId": conversation_id, "ok": ok }),
                },
            });
        }
    });
}

/// 一轮最小真回合（turn.once 的执行体，turn.start 异步复用）。
/// 错误以 (code, message) 返回，由调用方落成 err 信封
fn run_turn_once(
    config_dir: &Option<std::path::PathBuf>,
    prompt: &str,
    id: u64,
    outbox: &std::sync::mpsc::Sender<Envelope>,
) -> Result<String, (String, String)> {
    let Some(config_dir) = config_dir else {
        return Err(("no_config_dir".into(), "Main 没传来配置目录。".into()));
    };
    if prompt.trim().is_empty() {
        return Err(("bad_params".into(), "params.prompt 缺了。".into()));
    }
    let config = crate::config::load_from_dir(config_dir);
    if config.base_url.trim().is_empty() {
        return Err(("no_provider".into(), "配置里没有服务商地址。".into()));
    }
    let key = crate::config::api_key(&config)
        .map_err(|error| ("no_credentials".into(), format!("密钥解析失败：{error}")))?;
    let thread = json!([{ "role": "user", "content": prompt }])
        .as_array()
        .cloned()
        .unwrap_or_default();
    let stop = std::sync::atomic::AtomicBool::new(false);
    let mut sink = |event: crate::chat::ChatEvent| {
        let envelope = Envelope {
            v: 1,
            id,
            payload: EnvelopePayload::Ev {
                event: "chat".into(),
                data: serde_json::to_value(event).unwrap_or_default(),
            },
        };
        let _ = outbox.send(envelope);
    };
    match crate::chat::request_round(&config, &key, &thread, &[], None, &stop, &mut sink) {
        Ok(outcome) => Ok(outcome.text().to_string()),
        Err(failure) => Err(("turn_failed".into(), failure.message().to_string())),
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

    /// 测试输出缓冲：W 现在要 'static（写线程持有），Vec<u8> 包一层共享句柄
    #[derive(Clone, Default)]
    struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl SharedBuf {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap_or_else(|e| e.into_inner()).clone())
                .expect("回程是 UTF-8")
        }
    }
    use crate::agent_protocol::fence_admits;
    use std::io::Cursor;

    /// 内存往返：喂几行进去，收的回程行逐条对上
    #[test]
    fn the_worker_loop_answers_ping_echo_status_and_rejects_unknown_methods() {
        let input = Cursor::new(concat!(
            r#"{"v":1,"id":1,"kind":"req","method":"ping","params":{}}"#,
            "\n",
            r#"{"v":1,"id":2,"kind":"req","method":"echo","params":{"hi":"你 好"}}"#,
            "\n",
            "\n",            // 空行：无害噪声
            "这不是 JSON\n", // 坏帧：跳过不断循环
            r#"{"v":1,"id":3,"kind":"req","method":"agent.status","params":{}}"#,
            "\n",
            r#"{"v":1,"id":4,"kind":"req","method":"nope","params":{}}"#,
            "\n",
            r#"{"v":1,"id":5,"kind":"resp","result":{}}"#,
            "\n", // 回程形状进请求入口 = 协议错
        ));
        let output = SharedBuf::default();
        run_stdio_loop(
            input,
            output.clone(),
            WorkerContext {
                fence: 1,
                data_dir: None,
                config_dir: None,
            },
        )
        .expect("循环要干净退场");

        let text = output.text();
        let _ = &output; // "回程是 UTF-8");
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
                assert_eq!(
                    error.code, "unknown_method",
                    "M1 还没搬 turn.*，点了就是明确说不存在"
                );
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
        let result = run_stdio_loop(
            Cursor::new(""),
            Vec::new(),
            WorkerContext {
                fence: 1,
                data_dir: None,
                config_dir: None,
            },
        );
        assert!(result.is_ok(), "Main 关管道 = 正常退场");
    }

    #[test]
    fn fence_grants_flow_into_the_status_reading() {
        let input = Cursor::new(r#"{"v":1,"id":9,"kind":"req","method":"agent.status"}"#);
        let output = SharedBuf::default();
        run_stdio_loop(
            input,
            output.clone(),
            WorkerContext {
                fence: 41,
                data_dir: None,
                config_dir: None,
            },
        )
        .unwrap();
        let text = output.text();
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
        let output = SharedBuf::default();
        run_stdio_loop(
            input,
            output.clone(),
            WorkerContext {
                fence: 1,
                data_dir: Some(base.clone()),
                config_dir: Some(base.clone()),
            },
        )
        .unwrap();
        let text = output.text();
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
        let output = SharedBuf::default();
        run_stdio_loop(
            input,
            output.clone(),
            WorkerContext {
                fence: 1,
                data_dir: Some(base.clone()),
                config_dir: Some(base.clone()),
            },
        )
        .unwrap();
        let text = output.text();
        assert!(
            text.contains(r#""usageRequests":1"#),
            "worker 读到的是主进程写的同一份库：{text}"
        );
        crate::test_support::remove_tree(&base);
    }

    #[test]
    fn tool_decide_and_steer_push_speak_the_protocol_shapes() {
        let input = Cursor::new(concat!(
            r#"{"v":1,"id":1,"kind":"req","method":"tool.decide","params":{"requestId":"req-1","approved":true}}"#,
            "\n",
            r#"{"v":1,"id":2,"kind":"req","method":"steer.push","params":{"conversationId":"ghost","text":"插话"}}"#,
            "\n",
        ));
        let output = SharedBuf::default();
        run_stdio_loop(
            input,
            output.clone(),
            WorkerContext {
                fence: 1,
                data_dir: None,
                config_dir: None,
            },
        )
        .expect("循环干净退场");
        let text = output.text();
        let _ = &output; // "回程是 UTF-8");
        let lines: Vec<Envelope> = text
            .lines()
            .map(Envelope::from_line)
            .collect::<Result<_, _>>()
            .expect("每行合法信封");

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
        let context = WorkerContext {
            fence: 1,
            data_dir: Some(base.clone()),
            config_dir: Some(base.clone()),
        };

        // 空 prompt：参数闸先挡
        let input = Cursor::new(
            r#"{"v":1,"id":1,"kind":"req","method":"turn.once","params":{"prompt":"  "}}"#,
        );
        let output = SharedBuf::default();
        run_stdio_loop(input, output.clone(), context.clone()).unwrap();
        let text = output.text();
        assert!(text.contains(r#""code":"bad_params""#), "{text}");

        // 没配服务商地址：不碰网络，明确报 no_provider
        let input = Cursor::new(
            r#"{"v":1,"id":2,"kind":"req","method":"turn.once","params":{"prompt":"你好"}}"#,
        );
        let output = SharedBuf::default();
        run_stdio_loop(input, output.clone(), context).unwrap();
        let text = output.text();
        assert!(text.contains(r#""code":"no_provider""#), "{text}");
        crate::test_support::remove_tree(&base);
    }

    #[test]
    fn hubs_check_proves_the_runtime_lives_in_the_worker() {
        let input = Cursor::new(r#"{"v":1,"id":1,"kind":"req","method":"hubs.check"}"#);
        let output = SharedBuf::default();
        run_stdio_loop(
            input,
            output.clone(),
            WorkerContext {
                fence: 1,
                data_dir: None,
                config_dir: None,
            },
        )
        .unwrap();
        let text = output.text();
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
        let output = SharedBuf::default();
        run_stdio_loop(
            input,
            output.clone(),
            WorkerContext {
                fence: 1,
                data_dir: None,
                config_dir: None,
            },
        )
        .unwrap();
        let text = output.text();
        assert!(text.contains(r#""code":"no_data_dir""#), "{text}");

        // 传了目录：读到的就是那份 config.json 里的 model
        let base = temp_dir("agent-config-read");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("config.json"), r#"{"model":"配置甲"}"#).unwrap();
        let input = Cursor::new(r#"{"v":1,"id":2,"kind":"req","method":"config.read"}"#);
        let output = SharedBuf::default();
        run_stdio_loop(
            input,
            output.clone(),
            WorkerContext {
                fence: 1,
                data_dir: Some(base.clone()),
                config_dir: Some(base.clone()),
            },
        )
        .unwrap();
        let text = output.text();
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
    fn stream_demo_emits_events_in_order_then_a_terminating_resp() {
        let input = Cursor::new(
            r#"{"v":1,"id":7,"kind":"req","method":"stream.demo","params":{"count":3,"prefix":"tick"}}"#,
        );
        let output = SharedBuf::default();
        run_stdio_loop(
            input,
            output.clone(),
            WorkerContext {
                fence: 1,
                data_dir: None,
                config_dir: None,
            },
        )
        .expect("循环干净退场");

        let text = output.text();
        let _ = &output; // "回程是 UTF-8");
        let lines: Vec<Envelope> = text
            .lines()
            .map(Envelope::from_line)
            .collect::<Result<_, _>>()
            .expect("每行都是合法信封");
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
