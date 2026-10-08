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

/// agent 进程的自报家门：status 方法与诊断命令共用这一份读数
#[derive(Debug, Clone, PartialEq)]
pub struct AgentStatus {
    pub pid: u32,
    pub uptime_secs: u64,
    /// 本进程被授予的 fencing token（接管/重启时由 Main 侧 +1 重新授予）
    pub fence: u64,
    /// 这一进程活着的期间处理过的请求帧数（诊断用：坏帧率与吞吐的对账基数）
    pub served: u64,
}

/// 工作循环：逐行读信封、分发、逐行写回。EOF（Main 关了管道）即正常退场。
/// 任何一行解析失败都只是坏一帧，循环不断——护栏挂在编码/脏字节上
/// 等于整条 IPC 断掉（hooks 的同一条教训）
pub fn run_stdio_loop<R: std::io::Read, W: std::io::Write>(
    input: R,
    mut output: W,
    fence: u64,
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
        dispatch(envelope.id, &envelope.payload, fence, started, served, &mut output)?;
    }
    Ok(())
}

/// 单帧分发。req 才有回答；resp/ev/err 跑进 agent 的入口本身就是协议错。
/// 拿着 output 是因为流式方法（stream.demo）要连发多条 ev 再收 resp——
/// 单信封返回值装不下"一问多答"，M2 的 ChatEvent 透传同款
fn dispatch<W: Write>(
    id: u64,
    payload: &EnvelopePayload,
    fence: u64,
    started: Instant,
    served: u64,
    output: &mut W,
) -> Result<(), String> {
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
        methods::AGENT_STATUS => {
            let status = AgentStatus {
                pid: std::process::id(),
                uptime_secs: started.elapsed().as_secs(),
                fence,
                served,
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
        run_stdio_loop(input, &mut output, 1).expect("循环要干净退场");

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
        let result = run_stdio_loop(Cursor::new(""), Vec::new(), 1);
        assert!(result.is_ok(), "Main 关管道 = 正常退场");
    }

    #[test]
    fn fence_grants_flow_into_the_status_reading() {
        let input = Cursor::new(r#"{"v":1,"id":9,"kind":"req","method":"agent.status"}"#);
        let mut output: Vec<u8> = Vec::new();
        run_stdio_loop(input, &mut output, 41).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(
            text.contains(r#""fence":41"#),
            "接管时发的 fence 要原样出现在 status 里：{text}"
        );
        assert!(fence_admits(41, 41));
    }

    #[test]
    fn stream_demo_emits_events_in_order_then_a_terminating_resp() {
        let input = Cursor::new(
            r#"{"v":1,"id":7,"kind":"req","method":"stream.demo","params":{"count":3,"prefix":"tick"}}"#,
        );
        let mut output: Vec<u8> = Vec::new();
        run_stdio_loop(input, &mut output, 1).expect("循环干净退场");

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
