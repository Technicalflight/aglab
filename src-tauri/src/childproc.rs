//! Windows：GUI 进程拉起控制台程序（git / icacls / cmd / 语言服务器 / MCP 服务器……），
//! 子进程找不到可继承的控制台时 conhost 会给自己开一个新窗——用户看到的就是
//! "闪了一下终端"。这个应用没有任何想让控制台露出来的场景：
//! 所有子进程一律挂 `CREATE_NO_WINDOW`（与已有的 CREATE_SUSPENDED 按位或共存）。

pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 在构造点包一层：`hide(Command::new(..)).args(..)..`。
pub fn hide(mut command: std::process::Command) -> std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}
