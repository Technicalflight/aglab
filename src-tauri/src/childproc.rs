//! Windows：GUI 进程拉起控制台程序（git / icacls / cmd / 语言服务器 / MCP 服务器……），
//! 子进程找不到可继承的控制台时 conhost 会给自己开一个新窗——用户看到的就是
//! "闪了一下终端"。这个应用没有任何想让控制台露出来的场景：
//! 所有子进程一律挂 `CREATE_NO_WINDOW`（与已有的 CREATE_SUSPENDED 按位或共存）。
//!
//! **但测试编译一律不挂**：CI 的 Windows runner 跑在服务会话里，cmd.exe 配
//! CREATE_NO_WINDOW 会初始化失败（STATUS_DLL_INIT_FAILED，Server 2025 镜像实测），
//! 而"窗口闪不闪"本来就不是测试对象。生产（GUI，无控制台）照挂防闪窗。

pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 父进程当前有没有 Windows 控制台。GUI 应用为 false；测试与 CI 的控制台会话为 true
#[cfg(windows)]
pub fn console_absent() -> bool {
    use windows::Win32::System::Console::GetConsoleWindow;
    // 这条 FFI 只读一个进程属性，无失败路径
    #[allow(unsafe_code)]
    unsafe {
        GetConsoleWindow().0.is_null()
    }
}

#[cfg(not(windows))]
pub fn console_absent() -> bool {
    false
}

/// CREATE_SUSPENDED 一类基础旗标的使用方在此之上按需叠加 NO_WINDOW 位
#[cfg(windows)]
pub fn no_window_bit() -> u32 {
    // 测试编译不挂：见模块级注释（runner 服务会话里 cmd.exe 会崩）
    if cfg!(test) {
        return 0;
    }
    if console_absent() {
        CREATE_NO_WINDOW
    } else {
        0
    }
}

#[cfg(not(windows))]
pub fn no_window_bit() -> u32 {
    0
}

/// 在构造点包一层：`hide(Command::new(..)).args(..)..`。
pub fn hide(mut command: std::process::Command) -> std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // 测试编译不挂：见模块级注释
        if !cfg!(test) && console_absent() {
            command.creation_flags(CREATE_NO_WINDOW);
        }
    }
    command
}
