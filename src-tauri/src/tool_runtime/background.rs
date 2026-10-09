//! 后台命令登记表（design-command-execution-fixes.md §3）：开发服务器、watcher
//! 这类**本来就不该结束**的进程的去处。run_command 的 `background=true` 把进程
//! 放进来，`command_output` 增量读输出，`command_stop` 杀整棵树。
//!
//! 句柄只活在本次进程内：aglab 退出时这些孩子随进程树一起走，不需要跨重启的账。
//! 输出是环形缓冲——超限丢头留尾，读的人用总字节数对齐自己的光标。

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::json;

use super::constrain::{self, kill_tree};

/// 单个句柄的输出缓冲上限。超限丢头留尾——读的人靠 `dropped` 对齐光标
const LOG_CAP: usize = 128 * 1024;
/// 同时挂着的句柄上限。满了先收最早结束的，没有结束的就收最老的（杀树）
const MAX_HANDLES: usize = 32;

struct LogBuf {
    bytes: Vec<u8>,
    /// 因超限被丢掉的头部长度（总字节数口径）
    dropped: u64,
    total: u64,
}

impl LogBuf {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            dropped: 0,
            total: 0,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
        self.total += chunk.len() as u64;
        if self.bytes.len() > LOG_CAP {
            let cut = self.bytes.len() - LOG_CAP;
            self.bytes.drain(..cut);
            self.dropped += cut as u64;
        }
    }

    /// 自 `cursor`（总字节数口径）之后的新内容。落掉的头部按已消费计
    fn take_after(&mut self, cursor: &mut u64) -> Vec<u8> {
        let start = ((*cursor).max(self.dropped) - self.dropped) as usize;
        let start = start.min(self.bytes.len());
        let chunk = self.bytes[start..].to_vec();
        *cursor = self.total;
        chunk
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Status {
    Running,
    Finished { code: Option<i32> },
}

struct Handle {
    pid: u32,
    command: String,
    /// 谁启动的：启动它的那条话题 id。话题外的调用（测试、无上下文处）是空串——
    /// 面板按话题数"后台指令"时，没有主人的句柄不认领给任何一条
    owner: String,
    log: Arc<Mutex<LogBuf>>,
    cursor: u64,
    /// 收尸线程写的共享槽：output / stop / 驱逐都读它
    status: Arc<Mutex<Status>>,
    /// 收容壳。句柄随本条目生死：stop / 驱逐 / 退出清理都在 Drop 里被内核兜底收树，
    /// kill_tree 只是"马上就要用回端口"时打的提前量
    #[allow(dead_code)]
    job: crate::tool_runtime::job::Guard,
}

fn snapshot(status: &Arc<Mutex<Status>>) -> Status {
    status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn is_running(status: &Arc<Mutex<Status>>) -> bool {
    matches!(snapshot(status), Status::Running)
}

/// 登记表本体。`tauri::State` 管着它，测试直接 new
#[derive(Default)]
struct Inner {
    map: HashMap<u32, Handle>,
    next_id: u32,
}

/// 登记表：一把锁管句柄表与发号器
#[derive(Default)]
pub struct Registry {
    inner: Mutex<Inner>,
}

impl Registry {
    /// 拉起一条后台命令并登记。孩子立即返回，不套 60 秒——
    /// 它进这里就是为了长跑。环境清洗与前台命令同一份（constrained）。
    /// `owner` 是启动它的话题 id：面板按话题清点"后台指令"靠这一格。
    /// `sandbox_on` 是 run_command 解析过逐调用档位后的结果，后台不再读全局开关
    pub fn spawn(
        &mut self,
        command: &str,
        shell: &str,
        cwd: &std::path::Path,
        owner: &str,
        sandbox_on: bool,
    ) -> Result<u32, String> {
        self.evict_if_full()?;

        // 可执行程序只从字面量里来；命令文本作为独立参数段传给 shell，
        // 与前台 run_command 同一形状
        #[cfg(windows)]
        let mut cmd = match shell {
            "powershell" | "pwsh" => {
                let mut cmd = crate::childproc::hide(Command::new(shell));
                cmd.args(["-NoProfile", "-NonInteractive", "-Command", command]);
                cmd
            }
            "git-bash" => {
                let bash = crate::tools::git_bash_path()?;
                let mut cmd = crate::childproc::hide(Command::new(bash));
                cmd.args(["-c", command]);
                cmd
            }
            _ => {
                let mut cmd = crate::childproc::hide(Command::new("cmd"));
                cmd.args(["/C", command]);
                cmd
            }
        };
        #[cfg(not(windows))]
        let mut cmd = {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", command]);
            cmd
        };
        constrain::constrained(&mut cmd);

        // 沙箱：可写根就位（标签+授权，拿能力 SID）、专用临时目录 + 挂起拉起，
        // 收容之后换受限令牌、再恢复。长跑的后台进程更不能脱缰；失败按 fail-closed 拒绝
        let sandbox_cap_sids = if sandbox_on {
            match crate::tool_runtime::sandbox::prepare_command_roots(cwd) {
                Ok(sids) => Some(sids),
                Err(problem) => {
                    return Err(format!(
                        "沙箱可写根没就位，已拒绝执行（fail-closed）：{problem}"
                    ));
                }
            }
        } else {
            None
        };
        if sandbox_on {
            let tmp = crate::tool_runtime::sandbox::sandbox_tmp();
            cmd.env("TMP", &tmp).env("TEMP", &tmp);
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(
                    crate::tool_runtime::sandbox::CREATE_SUSPENDED
                        | crate::childproc::no_window_bit(),
                );
            }
        }

        let mut child = cmd
            .current_dir(cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("启动命令失败: {error}"))?;
        let pid = child.id();

        // 收容 fail-closed（拍板：约束建立失败 = 拒绝执行）。后台的长跑更不能脱缰：
        // 收不进去就杀掉孩子、把失败原样报回给模型
        let job = match crate::tool_runtime::job::Guard::contain(&child) {
            Ok(job) => job,
            Err(problem) => {
                constrain::reap_tree(&mut child);
                return Err(format!(
                    "执行收容约束建立失败，已拒绝执行（fail-closed）：{problem}"
                ));
            }
        };
        // 沙箱换受限令牌并恢复。失败同一条拍板
        if sandbox_on {
            let sids = sandbox_cap_sids.unwrap_or_default();
            if let Err(problem) = crate::tool_runtime::sandbox::activate(&child, &sids) {
                constrain::reap_tree(&mut child);
                return Err(format!(
                    "沙箱建立失败，已拒绝执行（fail-closed）：{problem}"
                ));
            }
        }

        let log = Arc::new(Mutex::new(LogBuf::new()));
        let status: Arc<Mutex<Status>> = Arc::new(Mutex::new(Status::Running));
        drain_into(child.stdout.take(), Arc::clone(&log));
        drain_into(child.stderr.take(), Arc::clone(&log));
        // 收尸线程：等退出码，别留僵尸
        let status_slot = Arc::clone(&status);
        std::thread::spawn(move || {
            let code = child.wait().ok().and_then(|exit| exit.code());
            *status_slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Status::Finished { code };
        });

        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.next_id += 1;
        let id = inner.next_id;
        inner.map.insert(
            id,
            Handle {
                pid,
                command: command.to_string(),
                owner: owner.to_string(),
                log,
                cursor: 0,
                status,
                job,
            },
        );
        Ok(id)
    }

    fn evict_if_full(&mut self) -> Result<(), String> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if inner.map.len() < MAX_HANDLES {
            return Ok(());
        }
        // 优先收已经结束的里最早那个；全是活人就收最老的
        let victim = inner
            .map
            .iter()
            .filter(|(_, handle)| !is_running(&handle.status))
            .map(|(id, _)| *id)
            .min()
            .or_else(|| inner.map.keys().min().copied());
        let Some(victim) = victim else {
            return Ok(());
        };
        if let Some(handle) = inner.map.remove(&victim) {
            if is_running(&handle.status) {
                handle.job.terminate();
                kill_tree(handle.pid);
            }
        }
        Ok(())
    }

    /// 增量读：只回上次之后的新输出，附状态。结束时给退出码
    pub fn output(&mut self, id: u32) -> Result<serde_json::Value, String> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let handle = inner.map.get_mut(&id).ok_or_else(|| {
            format!("没有 #{id} 这个后台命令句柄：可能从未启动，或重启后已作废。")
        })?;
        let mut log = handle
            .log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let chunk = log.take_after(&mut handle.cursor);
        drop(log);
        let (status_line, running) = match snapshot(&handle.status) {
            Status::Running => ("运行中".to_string(), true),
            Status::Finished { code: Some(code) } => (format!("已结束，退出码 {code}"), false),
            Status::Finished { code: None } => ("已结束".to_string(), false),
        };
        let mut text = constrain::decode_output(&chunk);
        if text.is_empty() {
            text = "（暂无新输出）".into();
        }
        Ok(json!({
            "id": id,
            "pid": handle.pid,
            "command": handle.command,
            "status": status_line,
            "running": running,
            "output": text,
        }))
    }

    /// 杀整棵树并移除句柄。重复停同一句柄按"没有这个句柄"处理——停两次无害
    pub fn stop(&mut self, id: u32) -> Result<String, String> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(handle) = inner.map.remove(&id) else {
            return Err(format!(
                "没有 #{id} 这个后台命令句柄：可能已经停过，或重启后已作废。"
            ));
        };
        if is_running(&handle.status) {
            // 收容壳先终止（整棵树），kill_tree 补刀保证确定性收场
            handle.job.terminate();
            kill_tree(handle.pid);
            Ok(format!(
                "已终止后台命令 #{id}（{}）及其整棵进程树。",
                handle.command
            ))
        } else {
            Ok(format!(
                "后台命令 #{id}（{}）本来就已经结束了，句柄已清。",
                handle.command
            ))
        }
    }

    /// 还在跑的那些句柄，带主人。面板那张"后台"小卡片按话题清点指令数，
    /// 全局读一遍、前端按 owner 分桶——一次读数两种口径（本话题 / 整机）都够用
    pub fn running(&self) -> Vec<RunningCommand> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut out: Vec<RunningCommand> = inner
            .map
            .iter()
            .filter(|(_, handle)| is_running(&handle.status))
            .map(|(id, handle)| RunningCommand {
                id: *id,
                command: handle.command.clone(),
                owner: handle.owner.clone(),
            })
            .collect();
        out.sort_by_key(|row| row.id);
        out
    }
}

/// 一条还在跑的后台命令。照 ModeView 那条纪律：只 Serialize 不 Deserialize——
/// 读数不该被界面掰回去
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunningCommand {
    pub id: u32,
    pub command: String,
    /// 启动它的话题 id。空串 = 话题外的调用（正常使用中不该出现）
    pub owner: String,
}

/// 面板"后台"小卡片的读数：整机还在跑的后台命令，带主人。
/// 话题维度与全局维度由前端从同一份里各取所需
#[tauri::command]
pub fn background_commands_list() -> Result<Vec<RunningCommand>, String> {
    Ok(state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .running())
}

/// 全局登记表。工具执行层（tools.rs）够不着 AppHandle 的 State，
/// 与池子的全局 hub 同一款：进程级的账本用进程级的状态
pub fn state() -> &'static Mutex<Registry> {
    static STATE: std::sync::OnceLock<Mutex<Registry>> = std::sync::OnceLock::new();
    STATE.get_or_init(|| Mutex::new(Registry::default()))
}

/// 应用退出时的收尾：句柄只活在本次进程内，孩子们也不该留下来——
/// 逐个杀树（lib.rs 的 RunEvent::Exit 调用，与内置浏览器同一处）
pub fn shutdown_all() {
    let Ok(registry) = state().lock() else { return };
    let mut inner = registry
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let ids: Vec<u32> = inner.map.keys().copied().collect();
    for id in ids {
        if let Some(handle) = inner.map.remove(&id) {
            if is_running(&handle.status) {
                kill_tree(handle.pid);
            }
        }
    }
}

fn drain_into<S: std::io::Read + Send + 'static>(stream: Option<S>, log: Arc<Mutex<LogBuf>>) {
    let Some(mut stream) = stream else { return };
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(read) => log
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(&buf[..read]),
                Err(_) => break,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> std::path::PathBuf {
        std::env::temp_dir()
    }

    /// 全链路：spawn 一条 echo → 增量读到输出与退出码 → stop 清句柄。
    /// 真拉 cmd 的测试（echo 秒级结束，不依赖任何外部工具，cmd 恒在）
    #[cfg(windows)]
    #[test]
    fn a_background_command_runs_reads_and_stops() {
        let mut registry = Registry::default();
        let id = registry
            .spawn("echo 后台你好", "cmd", &root(), "conv-测试", false)
            .expect("echo 必能起");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut saw_text = false;
        let mut saw_exit = false;
        while std::time::Instant::now() < deadline {
            let value = registry.output(id).expect("句柄在册");
            let output = value["output"].as_str().unwrap_or_default();
            if output.contains("后台你好") {
                saw_text = true;
            }
            if !value["running"].as_bool().unwrap_or(true) {
                saw_exit = true;
                assert_eq!(value["status"].as_str(), Some("已结束，退出码 0"));
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(saw_text, "增量读该读到 echo 的输出");
        assert!(saw_exit, "十秒内该等到退出码");

        let message = registry.stop(id).expect("句柄还在");
        assert!(
            message.contains("已经结束"),
            "结束后 stop 是收尾不是杀树：{message}"
        );
        assert!(registry.stop(id).is_err(), "停两次该按没有句柄处理");
    }

    /// 归属：句柄生下来就带着主人。running 按它在册报；stop 之后不再报。
    /// 真拉一条两秒的命令，起完立刻读、读完立刻停，不靠时序赌
    #[cfg(windows)]
    #[test]
    fn a_running_handle_knows_its_owner() {
        let mut registry = Registry::default();
        let id = registry
            .spawn("ping -n 3 127.0.0.1", "cmd", &root(), "conv-主人", false)
            .expect("ping 必能起");
        let running = registry.running();
        assert_eq!(running.len(), 1, "刚拉起该只有这一条在册");
        assert_eq!(running[0].owner, "conv-主人", "主人要跟句柄走");
        assert_eq!(running[0].id, id);
        registry.stop(id).expect("句柄在册");
        assert!(registry.running().is_empty(), "停掉之后不再报");
    }

    /// 环形缓冲：超限丢头留尾，光标用总字节数对齐
    #[test]
    fn the_log_buffer_drops_the_head_keeps_the_tail() {
        let mut log = LogBuf::new();
        log.push(&[b'a'; 200]);
        let mut cursor = 0u64;
        let _ = log.take_after(&mut cursor);
        log.push(&vec![b'b'; LOG_CAP + 1024]);
        let tail = log.take_after(&mut cursor);
        assert!(tail.len() <= LOG_CAP, "缓冲不超上限");
        assert!(tail.starts_with(b"b"), "丢的是头");
        // 过期的 cursor（落后于被丢的头部）不炸：按已消费计，从现存最老处接上
        let mut stale = 0u64;
        let chunk = log.take_after(&mut stale);
        assert_eq!(chunk.len(), log.bytes.len(), "过期光标从现存最老处接上");
    }
}
