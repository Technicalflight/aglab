//! Main 侧的 agent-host 监督者（M1 骨架，见蓝图 §A1/§A3）。
//!
//! 职责：
//! * 拉起 agent 子进程（同一 exe + `--agent-worker`），握住它的 stdin；
//! * 读线程逐行收回程信封，按 id 配对到等待者；
//! * 请求超时/子进程死亡 → 标记 orphan → 下次请求时**自动重启并 fence+1**；
//! * 租约登记：`conversationId → fence`。agent 的任何写回都要带当前 fence，
//!   被顶掉的旧进程（stale run）的字节在这里被拒收。
//!
//! M1 刻意从简的地方：单飞请求（一次一个 in-flight，pending 表留给 M2 的
//! 并发流）；心跳用"下一次请求即体检"实现——真实的多发并发改造成 M2 的事。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::Value;
use tauri::{AppHandle, Manager};

use crate::agent_protocol::{fence_admits, Envelope, EnvelopePayload};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

enum Inbound {
    Envelope(Envelope),
    /// 读线程收到 EOF = 子进程管道断了（死了或被杀）
    Eof,
}

/// 一条请求的结局
pub enum Outcome {
    Ok(Value),
    Err(String),
}

pub struct Supervisor {
    state: Option<HostState>,
    spawn: Box<dyn FnMut(u64) -> std::io::Result<Child> + Send>,
    /// 话题 → 当前租约的 fence
    leases: HashMap<String, u64>,
    /// 全局唯一的发号器：租约接管与 host 重启共用一个单调序列，
    /// 任何拿旧号说话的人（stale host / stale run）都过不了闸
    next_fence: u64,
    total_restarts: u64,
    /// 回程通道挂在监督者身上而不是 HostState：request 的事件路由要在
    /// 借着 state 写不进去的同时读通道——拆开字段借用才借得动
    inbound: Option<Receiver<Inbound>>,
    /// ev 信封的出口（M2：ChatEvent 透传给 UI 的那一跳）。
    /// None = 事件就地丢弃——没有观众的流不该堵住回程
    event_sink: Option<EventSink>,
    /// Main 的 app_data / app_config 两个目录：每次拉起 worker 都经 CLI 传下去
    data_dir: Option<std::path::PathBuf>,
    config_dir: Option<std::path::PathBuf>,
}

/// ev 信封的消费者。M2 里它把 ChatEvent 转成 UI 事件；M1.5 只喂 stream.demo
pub type EventSink = Box<dyn FnMut(&str, &Value) + Send>;

impl Supervisor {
    /// 当前 fence（没有活着的 host 就是 0 = 从未授出）
    #[allow(dead_code)] // M2 接线 turn.* 时消费；语义由 takeover/admit_write 单测钉住
    pub fn fence(&self) -> u64 {
        self.state.as_ref().map(|state| state.fence).unwrap_or(0)
    }

    #[allow(dead_code)] // 同上：重启次数进诊断读数是 M2 的事
    pub fn restarts(&self) -> u64 {
        self.total_restarts
    }

    /// 接管一个话题：发新 fence 并登记。之后该话题上的一切写回必须带这个 fence
    #[allow(dead_code)] // M2 的 turn.start 带话题接管；语义由单测钉住
    pub fn takeover(&mut self, conversation_id: &str) -> u64 {
        let fence = self.issue_fence();
        self.leases.insert(conversation_id.to_string(), fence);
        fence
    }

    /// stale run 防护的闸门：agent 写回的 fence 必须等于当前租约
    #[allow(dead_code)] // M2 的写回信封过这道闸；语义由单测钉住
    pub fn admit_write(&self, conversation_id: &str, presented: u64) -> bool {
        match self.leases.get(conversation_id) {
            Some(current) => fence_admits(*current, presented),
            None => false, // 没租约的话题不接受任何写回
        }
    }
}

struct HostState {
    child: Child,
    stdin: ChildStdin,
    fence: u64,
    next_id: u64,
    served: u64,
    started_at: Instant,
    /// orphan 判定标记：超时/EOF/写失败置位，下一次请求先重启再发
    dead: bool,
}

impl Supervisor {
    /// 生产入口：把当前 exe 用 `--agent-worker` 拉成 agent 进程。
    /// spawn 带 stdin 管道在个别宿主环境会撞 os error 231（管道实例耗尽的长相，
    /// hooks 同款）：前 3 次按瞬时抖动重试，全败才交错误——调用方按 orphan 处理
    pub fn spawn(data_dir: Option<std::path::PathBuf>, config_dir: Option<std::path::PathBuf>) -> Self {
        let dir_for_worker = data_dir.clone();
        let config_for_worker = config_dir.clone();
        Self::with_spawner(Box::new(move |fence| {
            let mut last: Option<std::io::Error> = None;
            for attempt in 1..=3u32 {
                let mut command = Command::new(std::env::current_exe().map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::NotFound, format!("定位自身失败：{e}"))
                })?);
                command
                    .arg("--agent-worker")
                    .arg("--agent-fence")
                    .arg(fence.to_string());
                if let Some(dir) = &dir_for_worker {
                    command.arg("--agent-data-dir").arg(dir);
                }
                if let Some(dir) = &config_for_worker {
                    command.arg("--agent-config-dir").arg(dir);
                }
                command
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null());
                match command.spawn() {
                    Ok(child) => return Ok(child),
                    Err(error) => {
                        last = Some(error);
                        std::thread::sleep(Duration::from_millis(60 * u64::from(attempt)));
                    }
                }
            }
            Err(last.unwrap_or_else(|| std::io::Error::other("spawn 三次全败且没有留下原因")))
        }))
    }

    /// 测试/变体入口：换一个拉起方式（集成测试用 CARGO_BIN_EXE 直接打真子进程）
    pub fn with_spawner(spawn: Box<dyn FnMut(u64) -> std::io::Result<Child> + Send>) -> Self {
        Self {
            state: None,
            spawn,
            leases: HashMap::new(),
            next_fence: 0,
            total_restarts: 0,
            inbound: None,
            event_sink: None,
            data_dir: None,
            config_dir: None,
        }
    }

    /// 登记 ev 信封的出口。整个监督者生命周期共用一个出口；
    /// M2 里 LocalHost 会在接管话题时把 ChatEvent 转发进 UI 的事件泵
    pub fn set_event_sink(&mut self, sink: EventSink) {
        self.event_sink = Some(sink);
    }

    /// 发一个新 fence。监督者级单调序列——租约与 host 重启共用同一个源
    fn issue_fence(&mut self) -> u64 {
        self.next_fence += 1;
        self.next_fence
    }

    fn respawn(&mut self) -> Result<(), String> {
        // 旧尸收殓：管道与读线程随 HostState 一起丢弃（mpsc 断开 = 读线程自然退场）
        if let Some(mut state) = self.state.take() {
            let _ = state.child.kill();
            let _ = state.child.wait();
        }
        let fence = self.issue_fence();
        let mut child = (self.spawn)(fence).map_err(|e| format!("拉起 agent 进程失败：{e}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "agent 进程没接上 stdin".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "agent 进程没接上 stdout".to_string())?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(line) => match Envelope::from_line(&line) {
                        Ok(envelope) => {
                            if tx.send(Inbound::Envelope(envelope)).is_err() {
                                return; // 监督者已丢弃本连接
                            }
                        }
                        Err(_) => continue, // 坏帧计数在 agent 侧的 status 里
                    },
                    Err(_) => break,
                }
            }
            let _ = tx.send(Inbound::Eof);
        });
        self.state = Some(HostState {
            child,
            stdin,
            fence,
            next_id: 1,
            served: 0,
            started_at: Instant::now(),
            dead: false,
        });
        self.inbound = Some(rx);
        self.total_restarts += 1;
        Ok(())
    }

    /// 一条请求：写信封、等配对回程。超时/EOF/写失败 → orphan 判定；
    /// 下一次请求会自动重启（fence+1）。单飞：M1/M1.5 一次只等一条请求。
    /// 途中的 ev 信封交给出站（event_sink）后继续等——**ev 不终结请求，
    /// resp/err 才终结**：M2 的 ChatEvent 流就坐在这一条规矩上
    pub fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        // 先体检：上一发留下的 orphan（EOF/超时）在这里变成重启
        let needs_respawn = match &mut self.state {
            None => true,
            Some(state) => state.dead,
        };
        if needs_respawn {
            self.respawn()?;
        }
        let (id, deadline) = {
            let state = self.state.as_mut().expect("刚确保活着");
            let id = state.next_id;
            state.next_id += 1;
            state.served += 1;
            let envelope = Envelope::req(id, method, params);
            state
                .stdin
                .write_all(envelope.to_line().as_bytes())
                .and_then(|_| state.stdin.write_all(b"\n"))
                .and_then(|_| state.stdin.flush())
                .map_err(|e| {
                    // 写不进去 = 管道断了：清尸，下次请求重启
                    state.dead = true;
                    format!("信封发不出去（agent 进程可能已死）：{e}")
                })?;
            (id, Instant::now() + REQUEST_TIMEOUT)
        };

        // 直接拆字段借：rx（共享）与 event_sink（可变）与 state（可变）互不相干，
        // 借用检查按字段认账——包成方法反而互相顶住
        let Supervisor { state, inbound, event_sink, .. } = self;
        let state = state.as_mut().expect("respawn 刚补上 host");
        let rx = inbound.as_ref().ok_or("没有活动的 agent 连接")?;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let inbound = match rx.recv_timeout(remaining.max(Duration::from_millis(10))) {
                Ok(inbound) => inbound,
                Err(RecvTimeoutError::Timeout) => {
                    if Instant::now() >= deadline {
                        state.dead = true; // 超时判 orphan；下次请求重启
                        return Err(format!(
                            "请求 {method} 超过 {}s 没有回程，已判孤儿。",
                            REQUEST_TIMEOUT.as_secs()
                        ));
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    state.dead = true;
                    return Err("读线程退场：agent 连接已断。".into());
                }
            };
            match inbound {
                Inbound::Eof => {
                    state.dead = true;
                    return Err("agent 进程的管道断了（进程退出）。".into());
                }
                Inbound::Envelope(envelope) => {
                    // 流内事件：交给出站后继续等终答。它不属于"别人的回程"
                    if let EnvelopePayload::Ev { event, data } = &envelope.payload {
                        if let Some(sink) = event_sink {
                            sink(event, data);
                        }
                        continue;
                    }
                    if envelope.id != id {
                        continue; // 别人的回程：单飞下不该出现，保险跳过
                    }
                    return match envelope.payload {
                        EnvelopePayload::Resp { result } => Ok(result),
                        EnvelopePayload::Err { error } => {
                            Err(format!("{}: {}", error.code, error.message))
                        }
                        EnvelopePayload::Req { .. } => {
                            Err("请求的回程不是 resp/err 形状。".into())
                        }
                        EnvelopePayload::Ev { .. } => unreachable!("ev 已在上面路由"),
                    };
                }
            }
        }
    }

    /// 流式检查：发一条 stream.demo，收集全部 ev，回 (事件序列, 终答)。
    /// 诊断命令与集成测试共用——顺序乱了就是 ev 通道坏了
    pub fn stream_check(&mut self, count: u64, prefix: &str) -> Result<(Vec<(String, Value)>, Value), String> {
        let collected = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(String, Value)>::new()));
        let keep = std::sync::Arc::clone(&collected);
        self.set_event_sink(Box::new(move |event, data| {
            if let Ok(mut bucket) = keep.lock() {
                bucket.push((event.to_string(), data.clone()));
            }
        }));
        let result = self.request(
            crate::agent_protocol::methods::STREAM_DEMO,
            serde_json::json!({ "count": count, "prefix": prefix }),
        );
        self.event_sink = None;
        let owned = std::sync::Arc::try_unwrap(collected)
            .map_err(|_| "事件收集器仍被占用".to_string())?;
        let events = owned
            .into_inner()
            .map_err(|poisoned| format!("事件收集锁中毒：{poisoned}"))?;
        Ok((events, result?))
    }
}

/// 一发健康检查：ping + status。诊断命令与集成测试共用
pub fn probe(supervisor: &mut Supervisor) -> Result<Value, String> {
    let pong = supervisor.request(methods_ping(), Value::Null)?;
    let status = supervisor.request(methods_status(), Value::Null)?;
    Ok(serde_json::json!({
        "pong": pong["pong"],
        "pid": status["pid"],
        "uptimeSecs": status["uptimeSecs"],
        "served": status["served"],
        "fence": supervisor.fence(),
        "restarts": supervisor.restarts(),
    }))
}

fn methods_ping() -> &'static str {
    crate::agent_protocol::methods::PING
}

fn methods_status() -> &'static str {
    crate::agent_protocol::methods::AGENT_STATUS
}

// ---------------------------------------------------------------------------
// 诊断面：设置/诊断页可以拿它当场验证"子进程还活着、协议通不通"。
// 全局唯一一份监督者：M2 起它升级成 LocalHost 的内核
// ---------------------------------------------------------------------------

static SUPERVISOR: std::sync::OnceLock<std::sync::Mutex<Supervisor>> = std::sync::OnceLock::new();

fn global(app: &AppHandle) -> &'static std::sync::Mutex<Supervisor> {
    SUPERVISOR.get_or_init(|| {
        let mut supervisor = Supervisor::spawn(
            app.path().app_data_dir().ok(),
            app.path().app_config_dir().ok(),
        );
        supervisor.data_dir = app.path().app_data_dir().ok();
        supervisor.config_dir = app.path().app_config_dir().ok();
        std::sync::Mutex::new(supervisor)
    })
}

#[tauri::command]
pub fn agent_probe(app: AppHandle) -> Result<Value, String> {
    let mut supervisor = global(&app).lock().map_err(|e| format!("监督者锁坏了：{e}"))?;
    probe(&mut supervisor)
}

/// ev 通道的诊断：发 stream.demo，验证事件按序到达、终答对得上。
/// UI 的 M2 接线前，这一条就是"事件流通了没有"的判决书
#[tauri::command]
pub fn agent_stream_check(app: AppHandle, count: Option<u64>) -> Result<Value, String> {
    let count = count.unwrap_or(3).clamp(1, 10);
    let mut supervisor = global(&app).lock().map_err(|e| format!("监督者锁坏了：{e}"))?;
    let (events, result) = supervisor.stream_check(count, "tick")?;
    let delivered = result["delivered"].as_u64().unwrap_or(0);
    Ok(serde_json::json!({
        "requested": count,
        "delivered": delivered,
        "received": events.len(),
        "inOrder": events
            .iter()
            .enumerate()
            .all(|(index, (_, data))| data["i"].as_u64() == Some(index as u64)),
        "events": events.iter().map(|(_, data)| data.clone()).collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 不起真进程的监督者测试桩：Child 无法在单测里凭空造，
    /// 这里的用例只钉纯逻辑（租约/fence 闸），进程级行为归集成测试
    #[test]
    fn takeover_increments_fence_and_the_gate_rejects_stale_writes() {
        let mut supervisor = Supervisor::with_spawner(Box::new(|_| {
            Err(std::io::Error::new(std::io::ErrorKind::Other, "测试桩不起进程"))
        }));
        let first = supervisor.takeover("c1");
        assert_eq!(first, 1, "首接管 fence=1（无 host 时从 0 起号）");
        let second = supervisor.takeover("c1");
        assert_eq!(second, 2, "再接管 = 再 +1，旧进程从此说不上话");
        assert!(supervisor.admit_write("c1", 2));
        assert!(!supervisor.admit_write("c1", 1), "stale run 的字节拒收");
        assert!(!supervisor.admit_write("ghost", 2), "没租约的话题不接受写回");

        // 不同话题各自从当前顶格起号？不——fence 是**监督者级**单调号，
        // 话题只持有引用，这是"全局一个发号器"的防重放设计
        let other = supervisor.takeover("c2");
        assert!(other > second);
    }
}
