//! Main 侧的 agent-host 监督者（M1 骨架，见蓝图 §A1/§A3）。
//!
//! 职责：
//! * 拉起 agent 子进程（同一 exe + `--agent-worker`），握住它的 stdin；
//! * 读线程逐行收回程信封，按 id 配对到等待者（**pending 表**——A6 第④步起
//!   真正并发：turn.start 的长回合等在它自己的槽位上，tool.decide /
//!   steer.push / turn.stop 在回合进行中照样进出，互不持锁）；
//! * 请求超时/子进程死亡 → 标记 orphan → 下次请求时**自动重启并 fence+1**；
//! * 租约登记：`conversationId → fence`。agent 的任何写回都要带当前 fence，
//!   被顶掉的旧进程（stale run）的字节在这里被拒收。
//!
//! 并发模型（A6 定稿）：`Supervisor` 是 `&self` 方法面——共享状态住在
//! `Arc<HostShared>` 里。请求只占自己的 pending 槽，不占监督者本身；
//! 重启（respawn）在 spawn 锁内重查 dead 旗，双请求竞争也不会拉起两个进程。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
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
#[allow(dead_code)] // M4 LocalHost 路由表用形状占位；语义由 request_opts 的返回钉住
pub enum Outcome {
    Ok(Value),
    Err(String),
}

/// Host 侧的共享状态：读线程与所有请求共用。字段各自带锁，
/// 请求之间不互斥——这正是审批闭环不死锁的前提
struct HostShared {
    has_child: AtomicBool,
    /// orphan 判定标记：超时/EOF/写失败置位，下一次请求先重启再发
    dead: AtomicBool,
    /// 拉起代数：每次 respawn +1。旧代读线程的 EOF 不得误杀新 host——
    /// 等待者只在自己那一代被判死
    generation: AtomicU64,
    /// 本代拉起失败的代号（0 = 本代还没失败过）。失败后同一代的后续请求
    /// 直接拿缓存的错误走，不重复撞 spawn 三连——下一代（新请求重试）才再试
    failed_generation: AtomicU64,
    child: Mutex<Option<Child>>,
    stdin: Mutex<Option<ChildStdin>>,
    fence: AtomicU64,
    next_id: AtomicU64,
    served: AtomicU64,
    started_at: Mutex<Instant>,
    /// 请求 id → (所属代, 等回程的槽)。读线程按 id 派发；槽消失 = 等待者已走
    pending: Mutex<HashMap<u64, (u64, Sender<Inbound>)>>,
    /// ev 信封的出口（ChatEvent 透传给 UI 的那一跳）。
    /// None = 事件就地丢弃——没有观众的流不该堵住回程
    event_sink: Mutex<Option<EventSink>>,
}

pub struct Supervisor {
    shared: Arc<HostShared>,
    spawn: Mutex<Box<dyn FnMut(u64) -> std::io::Result<Child> + Send>>,
    /// 话题 → 当前租约的 fence
    leases: Mutex<HashMap<String, u64>>,
    /// 全局唯一的发号器：租约接管与 host 重启共用一个单调序列，
    /// 任何拿旧号说话的人（stale host / stale run）都过不了闸
    next_fence: AtomicU64,
    total_restarts: AtomicU64,
    /// Main 的 app_data / app_config 两个目录：每次拉起 worker 都经 CLI 传下去
    pub data_dir: Option<std::path::PathBuf>,
    pub config_dir: Option<std::path::PathBuf>,
}

/// ev 信封的消费者。它把 ChatEvent 转成 UI 事件；诊断面喂 stream.demo
pub type EventSink = Box<dyn FnMut(&str, &Value) + Send>;

impl Supervisor {
    /// 当前 fence（没有活着的 host 就是 0 = 从未授出）
    #[allow(dead_code)] // M2 接线 turn.* 时消费；语义由 takeover/admit_write 单测钉住
    pub fn fence(&self) -> u64 {
        if self.shared.has_child.load(Ordering::Relaxed) {
            self.shared.fence.load(Ordering::Relaxed)
        } else {
            0
        }
    }

    #[allow(dead_code)] // 同上：重启次数进诊断读数是 M2 的事
    pub fn restarts(&self) -> u64 {
        self.total_restarts.load(Ordering::Relaxed)
    }

    /// 接管一个话题：发新 fence 并登记。之后该话题上的一切写回必须带这个 fence
    #[allow(dead_code)] // M2 的 turn.start 带话题接管；语义由单测钉住
    pub fn takeover(&self, conversation_id: &str) -> u64 {
        let fence = self.issue_fence();
        self.leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(conversation_id.to_string(), fence);
        fence
    }

    /// stale run 防护的闸门：agent 写回的 fence 必须等于当前租约
    #[allow(dead_code)] // M2 的写回信封过这道闸；语义由单测钉住
    pub fn admit_write(&self, conversation_id: &str, presented: u64) -> bool {
        match self
            .leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(conversation_id)
        {
            Some(current) => fence_admits(*current, presented),
            None => false, // 没租约的话题不接受任何写回
        }
    }

    fn issue_fence(&self) -> u64 {
        self.next_fence.fetch_add(1, Ordering::Relaxed) + 1
    }
}

impl Supervisor {
    /// 生产入口：把当前 exe 用 `--agent-worker` 拉成 agent 进程。
    /// spawn 带 stdin 管道在个别宿主环境会撞 os error 231（管道实例耗尽的长相，
    /// hooks 同款）：前 3 次按瞬时抖动重试，全败才交错误——调用方按 orphan 处理
    pub fn spawn(
        data_dir: Option<std::path::PathBuf>,
        config_dir: Option<std::path::PathBuf>,
    ) -> Self {
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
            shared: Arc::new(HostShared {
                has_child: AtomicBool::new(false),
                dead: AtomicBool::new(false),
                generation: AtomicU64::new(0),
                failed_generation: AtomicU64::new(0),
                child: Mutex::new(None),
                stdin: Mutex::new(None),
                fence: AtomicU64::new(0),
                next_id: AtomicU64::new(0),
                served: AtomicU64::new(0),
                started_at: Mutex::new(Instant::now()),
                pending: Mutex::new(HashMap::new()),
                event_sink: Mutex::new(None),
            }),
            spawn: Mutex::new(spawn),
            leases: Mutex::new(HashMap::new()),
            next_fence: AtomicU64::new(0),
            total_restarts: AtomicU64::new(0),
            data_dir: None,
            config_dir: None,
        }
    }

    /// 登记 ev 信封的出口。整个监督者生命周期共用一个出口；
    /// M4 里 LocalHost 会在接管话题时把 ChatEvent 转发进 UI 的事件泵
    /// （M3 的回合转发走 request_opts 的每请求闭包，不占这个全局口）
    #[allow(dead_code)]
    pub fn set_event_sink(&self, sink: EventSink) {
        *self
            .shared
            .event_sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sink);
    }

    /// 旧尸收殓 + 拉起新 host。spawn 锁内**重查** dead 旗：
    /// 两个并发请求同时发现 orphan，只有头一个真的拉进程
    fn respawn(&self) -> Result<(), String> {
        let mut spawn = self
            .spawn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.shared.has_child.load(Ordering::Relaxed)
            && !self.shared.dead.load(Ordering::Relaxed)
        {
            return Ok(()); // 别人刚拉起：直接用
        }
        // 本代已试过且失败：不重复撞 spawn（同一代 orphan 只准一次尝试），
        // 把失败交给调用方；重试入口是下一代——新请求会先 +1 再进来
        let gen_now = self.shared.generation.load(Ordering::Relaxed);
        if gen_now != 0 && self.shared.failed_generation.load(Ordering::Relaxed) == gen_now {
            return Err("agent 进程拉不起（本代已试过，等下一次请求重试）。".into());
        }
        // 旧尸收殓：管道与读线程随死旗一起退场（mpsc 断开 = 读线程自然退场）
        if let Some(mut child) = self
            .shared
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
        *self
            .shared
            .stdin
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        let fence = self.issue_fence();
        let generation = self.shared.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let mut child = (spawn)(fence).map_err(|e| {
            self.shared
                .failed_generation
                .store(generation, Ordering::Relaxed);
            format!("拉起 agent 进程失败：{e}")
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "agent 进程没接上 stdin".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "agent 进程没接上 stdout".to_string())?;
        let shared = Arc::clone(&self.shared);
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let line = match line {
                    Ok(line) => line,
                    Err(_) => break,
                };
                let envelope = match Envelope::from_line(&line) {
                    Ok(envelope) => envelope,
                    Err(_) => continue, // 坏帧计数在 agent 侧的 status 里
                };
                // 流内事件：先交全局出口，再投给在等的请求槽（两处各取所需）
                if let EnvelopePayload::Ev { event, data } = &envelope.payload {
                    if let Some(sink) = shared
                        .event_sink
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .as_mut()
                    {
                        sink(event, data);
                    }
                }
                let waiting = shared
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&envelope.id)
                    .map(|(_, tx)| tx.clone());
                if let Some(tx) = waiting {
                    if tx.send(Inbound::Envelope(envelope)).is_err() {
                        // 等待者已走：迟到帧丢弃，读线程继续
                    }
                }
            }
            // EOF（本代进程退场）：只唤醒**本代**的等待者，各自按代数决定是否判死。
            // 旧代读线程不得碰新代的槽，也不得碰 dead 旗——那时新 host 可能已经活着
            let mut pending = shared
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mine: Vec<u64> = pending
                .iter()
                .filter(|(_, (slot_gen, _))| *slot_gen == generation)
                .map(|(id, _)| *id)
                .collect();
            for id in mine {
                if let Some((_, tx)) = pending.remove(&id) {
                    let _ = tx.send(Inbound::Eof);
                }
            }
        });
        self.shared.fence.store(fence, Ordering::Relaxed);
        self.shared.has_child.store(true, Ordering::Relaxed);
        self.shared.dead.store(false, Ordering::Relaxed);
        *self
            .shared
            .stdin
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(stdin);
        *self
            .shared
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(child);
        *self
            .shared
            .started_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Instant::now();
        self.total_restarts.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn ensure_alive(&self) -> Result<(), String> {
        if self.shared.has_child.load(Ordering::Relaxed)
            && !self.shared.dead.load(Ordering::Relaxed)
        {
            return Ok(());
        }
        // 本代已试过且失败：不重复撞 spawn，直接把失败交给调用方。
        // 重试的入口是"下一代"——下一次请求判定孤儿时 generation 已 +1
        if self.shared.failed_generation.load(Ordering::Relaxed)
            == self.shared.generation.load(Ordering::Relaxed)
            && self.shared.generation.load(Ordering::Relaxed) != 0
        {
            return Err("agent 进程拉不起（本代已试过，等下一次请求重试）。".into());
        }
        self.respawn()
    }

    /// 一条请求：写信封、等配对回程。超时/EOF/写失败 → orphan 判定；
    /// 下一次请求会自动重启（fence+1）。途中的 ev 信封交给全局出口后继续等——
    /// **ev 不终结请求，resp/err 才终结**：M2 的 ChatEvent 流就坐在这一条规矩上
    pub fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        self.request_opts(method, params, Some(REQUEST_TIMEOUT), &mut |_, _| {})
    }

    /// 带选项的请求：长回合（turn.start）用 `None` 超时——回合的长短由停止键
    /// 与审批超时管，监督者只盯着管道断线（EOF/断连即刻失败）。
    /// `on_event` 收途中的每一帧 ev（请求 id 归属已由 pending 表保证）
    pub fn request_opts(
        &self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
        on_event: &mut dyn FnMut(&str, &Value),
    ) -> Result<Value, String> {
        self.ensure_alive()?;
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        self.shared.served.fetch_add(1, Ordering::Relaxed);
        let generation = self.shared.generation.load(Ordering::Relaxed);
        let (tx, rx): (Sender<Inbound>, Receiver<Inbound>) = mpsc::channel();
        self.shared
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, (generation, tx));
        let write_result = {
            let mut guard = self
                .shared
                .stdin
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match guard.as_mut() {
                Some(stdin) => stdin
                    .write_all(Envelope::req(id, method, params).to_line().as_bytes())
                    .and_then(|_| stdin.write_all(b"\n"))
                    .and_then(|_| stdin.flush())
                    .map_err(|e| e.to_string()),
                None => Err("agent 进程没有可写的 stdin".to_string()),
            }
        };
        if let Err(problem) = write_result {
            // 写不进去 = 管道断了：清尸，下次请求重启
            self.shared.dead.store(true, Ordering::Relaxed);
            self.shared
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&id);
            return Err(format!("信封发不出去（agent 进程可能已死）：{problem}"));
        }
        let deadline = timeout.map(|budget| Instant::now() + budget);
        let outcome = loop {
            // 长回合（timeout=None）：慢轮询等回程；EOF/断连随时打断
            let wait: Result<Inbound, RecvTimeoutError> = match deadline {
                Some(at) => {
                    let remaining = at.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        Err(RecvTimeoutError::Timeout)
                    } else {
                        rx.recv_timeout(remaining)
                    }
                }
                None => match rx.recv_timeout(Duration::from_millis(250)) {
                    Ok(inbound) => Ok(inbound),
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(error) => Err(error),
                },
            };
            match wait {
                Err(RecvTimeoutError::Timeout) => {
                    self.shared.dead.store(true, Ordering::Relaxed);
                    self.shared
                        .pending
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&id);
                    let budget = timeout
                        .map(|t| format!("{}s", t.as_secs()))
                        .unwrap_or_else(|| "∞".into());
                    break Err(format!("请求 {method} 超过 {budget} 没有回程，已判孤儿。"));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    self.shared.dead.store(true, Ordering::Relaxed);
                    break Err("读线程退场：agent 连接已断。".into());
                }
                Ok(Inbound::Eof) => {
                    // 只有本代的 EOF 才判死：旧代读线程唤醒的等待者面前，
                    // 新 host 可能已经活着（respawn 已把 dead 清回 false）
                    if self.shared.generation.load(Ordering::Relaxed) == generation {
                        self.shared.dead.store(true, Ordering::Relaxed);
                    }
                    break Err("agent 进程的管道断了（进程退出）。".into());
                }
                Ok(Inbound::Envelope(envelope)) => {
                    if let EnvelopePayload::Ev { event, data } = &envelope.payload {
                        on_event(event, data);
                        continue;
                    }
                    break match envelope.payload {
                        EnvelopePayload::Resp { result } => Ok(result),
                        EnvelopePayload::Err { error } => {
                            Err(format!("{}: {}", error.code, error.message))
                        }
                        EnvelopePayload::Req { .. } => Err("请求的回程不是 resp/err 形状。".into()),
                        EnvelopePayload::Ev { .. } => unreachable!("ev 已在上面路由"),
                    };
                }
            }
        };
        self.shared
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        outcome
    }

    /// 流式检查：发一条 stream.demo，收集全部 ev，回 (事件序列, 终答)。
    /// 诊断命令与集成测试共用——顺序乱了就是 ev 通道坏了
    pub fn stream_check(
        &self,
        count: u64,
        prefix: &str,
    ) -> Result<(Vec<(String, Value)>, Value), String> {
        let collected = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
        let keep = Arc::clone(&collected);
        let result = self.request_opts(
            crate::agent_protocol::methods::STREAM_DEMO,
            serde_json::json!({ "count": count, "prefix": prefix }),
            Some(REQUEST_TIMEOUT),
            &mut |event, data| {
                if let Ok(mut bucket) = keep.lock() {
                    bucket.push((event.to_string(), data.clone()));
                }
            },
        )?;
        let owned = Arc::try_unwrap(collected).map_err(|_| "事件收集器仍被占用".to_string())?;
        let events = owned
            .into_inner()
            .map_err(|poisoned| format!("事件收集锁中毒：{poisoned}"))?;
        Ok((events, result))
    }
}

// ---------------------------------------------------------------------------
// 诊断面：设置/诊断页可以拿它当场验证"子进程还活着、协议通不通"。
// 全局唯一一份监督者：M2 起它升级成 LocalHost 的内核
// ---------------------------------------------------------------------------

static SUPERVISOR: std::sync::OnceLock<Supervisor> = std::sync::OnceLock::new();

pub(crate) fn global(app: &AppHandle) -> &'static Supervisor {
    SUPERVISOR.get_or_init(|| {
        let mut supervisor = Supervisor::spawn(
            app.path().app_data_dir().ok(),
            app.path().app_config_dir().ok(),
        );
        supervisor.data_dir = app.path().app_data_dir().ok();
        supervisor.config_dir = app.path().app_config_dir().ok();
        supervisor
    })
}

/// 一发健康检查：ping + status。诊断命令与集成测试共用
pub fn probe(supervisor: &Supervisor) -> Result<Value, String> {
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

#[tauri::command]
pub fn agent_probe(app: AppHandle) -> Result<Value, String> {
    probe(global(&app))
}

/// ev 通道的诊断：发 stream.demo，验证事件按序到达、终答对得上。
/// UI 的 M2 接线前，这一条就是"事件流通了没有"的判决书
#[tauri::command]
pub fn agent_stream_check(app: AppHandle, count: Option<u64>) -> Result<Value, String> {
    let count = count.unwrap_or(3).clamp(1, 10);
    let (events, result) = global(&app).stream_check(count, "tick")?;
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
        let supervisor =
            Supervisor::with_spawner(Box::new(|_| Err(std::io::Error::other("测试桩不起进程"))));
        let first = supervisor.takeover("c1");
        assert_eq!(first, 1, "首接管 fence=1（无 host 时从 0 起号）");
        let second = supervisor.takeover("c1");
        assert_eq!(second, 2, "再接管 = 再 +1，旧进程从此说不上话");
        assert!(supervisor.admit_write("c1", 2));
        assert!(!supervisor.admit_write("c1", 1), "stale run 的字节拒收");
        assert!(
            !supervisor.admit_write("ghost", 2),
            "没租约的话题不接受写回"
        );

        // 不同话题各自从当前顶格起号？不——fence 是**监督者级**单调号，
        // 话题只持有引用，这是"全局一个发号器"的防重放设计
        let other = supervisor.takeover("c2");
        assert!(other > second);
    }

    /// respawn 在 spawn 锁内重查死旗：第二个发现 orphan 的请求不得再拉进程
    #[test]
    fn respawn_never_spawns_twice_for_one_orphan() {
        use std::sync::atomic::AtomicUsize;
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let supervisor = Supervisor::with_spawner(Box::new(move |_| {
            counter.fetch_add(1, Ordering::Relaxed);
            Err(std::io::Error::other("拉不起"))
        }));
        // 两个并发请求都发现没 host：spawn 锁内重查后第二个直接复用判定结果
        let a = supervisor.respawn();
        let b = supervisor.respawn();
        assert!(a.is_err() && b.is_err(), "拉不起就是报错，不吞");
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "同一轮 orphan 只准尝试一次拉起"
        );
    }
}
