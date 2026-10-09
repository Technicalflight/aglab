//! 进程收容（Windows Job Object）：模型与插件拉起的子进程，整棵树与一个句柄同生共死。
//!
//! 拍板记录（2026-10-02，用户）：**约束建立失败 = 拒绝执行**（fail-closed），
//! 不存在"约束没套上就明跑"的降级路。调用方拿到 `Err` 必须杀掉孩子并返回失败。
//!
//! 诚实边界：这是**收容（containment），不是沙箱（sandbox）**。它保证的是
//! "跑出去的树一定能一次收干净"——句柄关闭或终止时全树即死（含我们已经返回了
//! 结果却还在后台挂着的孙子进程）、子进程无法悄悄脱离、剪贴板与桌面 USER 句柄隔离；
//! 它**不**承诺文件与网络的权限隔离。真沙箱（受限令牌 + ACL 白名单）是独立的一期，
//! UI 与文案不许把这条说成"沙箱"——一个名字大于内容的开关，正是
//! design-tool-runtime.md 里 J6 说的那种"往轻了说的确认框"。
//!
//! 已知的窄缝：spawn 与 Assign 之间有一个毫秒级的窗口，孩子在被收容前已经活着。
//! 要关死它需要 CREATE_SUSPENDED + 手工恢复主线程，std 的 Command 给不了；
//! 窗口里的孩子还来不及做任何有副作用的事，记录在案，不假装它不存在。

use std::process::Child;

/// 一条命令的收容壳。句柄活着 = 树活着；Drop 即关句柄，`KILL_ON_JOB_CLOSE`
/// 会把整棵树（shell 已经退了也包含它留下的所有子孙）一并带走。
/// 内核句柄跨线程有效，登记表要 `Send`，这里如实声明
pub struct Guard {
    #[cfg(windows)]
    handle: windows::Win32::Foundation::HANDLE,
}

// 内核对象句柄不绑定线程；HANDLE 是裸指针别名，Rust 编译器不知道这一点
#[cfg(windows)]
unsafe impl Send for Guard {}

impl Guard {
    /// 收容一个已经 spawn 的孩子。任何一步失败都返回 `Err`——**调用方必须按
    /// 拍板处理：杀掉孩子、拒绝执行**，而不是当作没这回事继续跑
    #[cfg(windows)]
    pub fn contain(child: &Child) -> Result<Guard, String> {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_UILIMIT, JOB_OBJECT_UILIMIT_HANDLES,
            JOB_OBJECT_UILIMIT_READCLIPBOARD, JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
        };

        unsafe {
            let job =
                CreateJobObjectW(None, None).map_err(|e| format!("创建 Job Object 失败：{e}"))?;

            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            // 不设 BREAKAWAY_OK / SILENT_BREAKAWAY_OK：子进程不许悄悄脱离收容
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
            .map_err(|e| {
                let _ = windows::Win32::Foundation::CloseHandle(job);
                format!("设置收容约束失败：{e}")
            })?;

            // 桌面隔离单独设（它走自己的信息类）：不读不写剪贴板，拿不到收容树
            // 之外的 USER 句柄。构建/测试命令从不碰这些；碰了就该被挡下来
            let ui_restrictions: JOB_OBJECT_UILIMIT = JOB_OBJECT_UILIMIT_HANDLES
                | JOB_OBJECT_UILIMIT_READCLIPBOARD
                | JOB_OBJECT_UILIMIT_WRITECLIPBOARD;
            SetInformationJobObject(
                job,
                windows::Win32::System::JobObjects::JobObjectBasicUIRestrictions,
                &ui_restrictions as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOB_OBJECT_UILIMIT>() as u32,
            )
            .map_err(|e| {
                let _ = windows::Win32::Foundation::CloseHandle(job);
                format!("设置桌面隔离失败：{e}")
            })?;

            AssignProcessToJobObject(job, HANDLE(child.as_raw_handle())).map_err(|e| {
                let _ = windows::Win32::Foundation::CloseHandle(job);
                format!("把子进程收进 Job 失败：{e}")
            })?;

            Ok(Guard { handle: job })
        }
    }

    /// 立即终止整棵树。Drop 也会收，但"停掉之后马上要用回端口/文件"的调用方
    /// 需要的是确定性的这一下（与 kill_tree 同一条理由）
    #[cfg(windows)]
    pub fn terminate(&self) {
        use windows::Win32::System::JobObjects::TerminateJobObject;
        unsafe {
            let _ = TerminateJobObject(self.handle, 1);
        }
    }

    /// 这棵树里现在有几个进程（含子孙）。诊断与测试的对账用
    #[cfg(windows)]
    #[allow(dead_code)]
    pub fn process_count(&self) -> Result<u32, String> {
        use windows::Win32::System::JobObjects::{
            JobObjectBasicAccountingInformation, QueryInformationJobObject,
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        };
        unsafe {
            let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
            QueryInformationJobObject(
                Some(self.handle),
                JobObjectBasicAccountingInformation,
                &mut info as *mut _ as *mut std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                None,
            )
            .map_err(|e| format!("读取收容读数失败：{e}"))?;
            Ok(info.TotalProcesses)
        }
    }
}

#[cfg(windows)]
impl Drop for Guard {
    fn drop(&mut self) {
        unsafe {
            // KILL_ON_JOB_CLOSE：最后一个句柄关闭的瞬间，整棵树被内核收走
            let _ = windows::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

/// 非 Windows 的占位：本客户端是 Windows-only（Credential Manager / WebView2 /
/// chcp 全按此设计）。别的平台连不上这些 API，也不假装收容过——恒为空壳，
/// 由调用方的平台门挡着，不会走到这里
#[cfg(not(windows))]
pub struct Guard;

#[cfg(not(windows))]
impl Guard {
    pub fn contain(_child: &Child) -> Result<Guard, String> {
        Ok(Guard)
    }

    pub fn terminate(&self) {}

    pub fn process_count(&self) -> Result<u32, String> {
        Ok(0)
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    fn sleep_child() -> Child {
        Command::new("cmd")
            .args(["/C", "ping -n 30 127.0.0.1 > nul"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("拉起测试子进程")
    }

    #[test]
    fn containment_reports_the_process_and_terminate_kills_the_tree() {
        let mut child = sleep_child();
        let guard = Guard::contain(&child).expect("收容要成功");
        assert_eq!(guard.process_count().unwrap(), 1, "刚收容时树里只有它自己");

        guard.terminate();
        let started = std::time::Instant::now();
        let exited = loop {
            if child.try_wait().expect("等子进程").is_some() {
                break true;
            }
            if started.elapsed() > std::time::Duration::from_secs(5) {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        assert!(exited, "terminate 之后孩子要死透");
    }

    /// 拍板的那一半靠它兜底：句柄一关（哪怕调用方忘了 terminate、哪怕本进程崩了），
    /// 内核把整棵树收走。孩子没被 wait 也必须变成"已退出"
    #[test]
    fn dropping_the_guard_takes_the_tree_down() {
        let mut child = sleep_child();
        let guard = Guard::contain(&child).expect("收容要成功");
        drop(guard);

        let started = std::time::Instant::now();
        let exited = loop {
            if child.try_wait().expect("等子进程").is_some() {
                break true;
            }
            if started.elapsed() > std::time::Duration::from_secs(5) {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        assert!(exited, "Drop（关句柄）必须把整棵树带走");
    }

    /// 孙子也逃不掉：shell 又 spawn 出去的那一层同在树里
    #[test]
    fn grandchildren_are_inside_the_job_too() {
        // shell 自己再生一层：cmd → cmd → ping
        let mut child = Command::new("cmd")
            .args(["/C", "cmd /C ping -n 30 127.0.0.1 > nul"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("拉起测试子进程");
        let guard = Guard::contain(&child).expect("收容要成功");

        // 等孙子真的被生出来
        let mut seen_two = false;
        for _ in 0..100 {
            if guard.process_count().unwrap() >= 2 {
                seen_two = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(seen_two, "孙进程要被算进同一棵树");

        guard.terminate();
        let started = std::time::Instant::now();
        let exited = loop {
            if child.try_wait().expect("等子进程").is_some() {
                break true;
            }
            if started.elapsed() > std::time::Duration::from_secs(5) {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        assert!(exited, "terminate 连孙子一起收");
    }
}
