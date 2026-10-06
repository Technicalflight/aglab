//! 执行约束。设计里刻意不叫"沙箱"：在这个只有 Windows、没有 WSL 的客户端上，
//! 现在拿得到的是**环境变量筛选、强制 cwd、进程树终止、时长与输出预算**这四件事，
//! 叫沙箱就等于一个名字大于内容的开关（`deliverables/design-tool-runtime.md` §1.1-3）。
//! 真的 OS 隔离（受限令牌 / Job Object）在 P2，且要先拍板"约束失败时是拒绝还是降级"。

use std::path::{Path, PathBuf};

/// 命令最长跑多久。超时是终止整棵进程树，不是只掐住 `cmd` 那一层
pub const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// 命令输出预算。超过就截断并在结果里说明，不静默丢
pub const MAX_COMMAND_OUTPUT: usize = 32 * 1024;

/// 名字里带这些片段的变量不传给子进程。它们是这个客户端之外别的程序存的凭据：
/// 模型只要能跑 `set` / `env` / `printenv`，或者跑一个会把环境打印出来的构建脚本，
/// 就等于把用户钥匙串旁边的东西抄进了对话上下文——而上下文是要发给服务商的
const CREDENTIAL_HINTS: [&str; 7] = [
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "APIKEY",
    "API_KEY",
    "PRIVATE_KEY",
    "CREDENTIAL",
];

/// 永远保留的变量。它们撑起 shell、编译器与 git，去掉不是安全，是把自己弄瘫
const ALWAYS_KEEP: [&str; 12] = [
    "PATH",
    "HOME",
    "USERPROFILE",
    "SYSTEMROOT",
    "WINDIR",
    "COMSPEC",
    "PROMPT",
    "TEMP",
    "TMP",
    "LANG",
    "LC_ALL",
    "TZ",
];

fn is_credential_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    if ALWAYS_KEEP.contains(&upper.as_str()) {
        return false;
    }
    CREDENTIAL_HINTS.iter().any(|hint| upper.contains(hint))
}

/// 子进程不许继承的代理变量。aglab 的代理配置是唯一真相：系统环境里那几只
/// （用户为别的程序设的）不该再漏给子进程——该走什么代理由设置页说了算
const PROXY_VAR_NAMES: [&str; 4] = ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY"];

/// 子进程要带的环境。
///
/// 这是**按名字摘掉凭据类变量**，不是白名单。白名单听起来更严，但它会顺手打断
/// `git` / `cargo` / `npm` 这些天天在用的东西（它们各自依赖一串专有变量），
/// 而一个跑不通的约束不会让人更安全，只会让人想办法绕过去。
/// 想升级成白名单，得先把常用工具链的变量清单一起补上并测过
pub fn child_env() -> Vec<(String, String)> {
    let mut vars: Vec<(String, String)> = std::env::vars()
        .filter(|(name, _)| !is_credential_name(name))
        // 代理变量不继承：aglab 的代理设置是唯一真相（见 proxy.rs），系统环境里
        // 那几只是为别的程序设的，透传下去等于子进程各走各的代理
        .filter(|(name, _)| !PROXY_VAR_NAMES.contains(&name.to_ascii_uppercase().as_str()))
        .collect();
    // aglab 自己的代理绑定追加在后：MCP / 命令 / 钩子的出口流量跟着设置页走，
    // NO_PROXY 恒含本机回环（sidecar 不被卷进代理）
    vars.extend(crate::proxy::child_proxy_env());
    vars
}

/// 把一条命令的环境换成"摘掉凭据形状变量"的那一份。
///
/// 三处 spawn 都过这里：工具命令（模型点的那条）、**MCP 服务器**、**插件钩子**。
/// 后两处比模型那条更需要过一遍——MCP 服务器常常是 `npx` 上的第三方包，
/// 钩子是一整行 shell，两者都能把环境原样打出来，而打出来的东西会回到这个进程的
/// stdout / stderr 里，也就回到上下文。
///
/// 需要专有变量的服务器请写进它自己的 `env` 配置：那是用户显式给的，
/// 调用方在本函数之后再 `.env(...)`，因此不受这条筛选影响。
pub fn constrained(command: &mut std::process::Command) {
    command.env_clear().envs(child_env());
}

/// 命令必须在某个项目根里跑。没有根就**拒绝执行**，而不是退回进程当前的工作目录——
/// 后者是客户端自己的安装目录，跑在那里等于在一个用户看不见的地方动文件
pub fn require_cwd(root: Option<&Path>) -> Result<PathBuf, String> {
    let Some(root) = root else {
        return Err(
            "没有选定项目目录，命令不能执行。请在标题栏选一个项目，或在调用里给出 cwd。".into(),
        );
    };
    if !root.is_dir() {
        return Err(format!("项目目录不存在或不是目录：{}", root.display()));
    }
    Ok(root.to_path_buf())
}

/// 子进程输出解码：UTF-8 优先（chcp 65001 与一切现代 CLI），解不开再按
/// GBK 兜底（zh-CN Windows 控制台的默认码页 936）。从前台 run_command 到
/// 后台缓冲都走这一份——两处各编一个码，"同一个输出两种乱法"最难查
pub fn decode_output(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => {
            let (text, _, _) = encoding_rs::GBK.decode(bytes);
            text.into_owned()
        }
    }
}

/// 终止整棵树用的命令。`/T` 是这条函数存在的全部理由：
/// 只 kill `cmd` 那一层，它spawn 出去的编译器、node、测试进程会继续跑，
/// 而我们已经把"超时"报给用户了
#[cfg(windows)]
pub fn kill_command(pid: u32) -> (&'static str, Vec<String>) {
    ("taskkill", vec!["/PID".to_string(), pid.to_string(), "/T".to_string(), "/F".to_string()])
}

#[cfg(not(windows))]
pub fn kill_command(pid: u32) -> (&'static str, Vec<String>) {
    ("kill", vec!["-TERM".to_string(), pid.to_string()])
}

/// 掐掉一棵树。等到那条外部命令自己回来为止：调用方（`mcp_stop`、钩子超时）之后
/// 可能立刻要用回同一个端口或同一份文件，返回太早等于骗它"已经收干净了"
#[cfg(windows)]
pub fn kill_tree(pid: u32) {
    let (program, args) = kill_command(pid);
    let _ = crate::childproc::hide(std::process::Command::new(program))
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(not(windows))]
pub fn kill_tree(pid: u32) {
    let _ = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// 收掉一个孩子连同它全部的子孙。
///
/// 先 `kill` + `wait`：不 `wait` 就留一个僵尸，而调用方拿不到退出状态；
/// 再按 PID 掐整棵树——只杀 `cmd` / `sh` 那一层，它 spawn 出去的 node、python
/// 会继续活着，而我们已经把"超时"或"已停止"报给用户了
pub fn reap_tree(child: &mut std::process::Child) {
    let pid = child.id();
    let _ = child.kill();
    let _ = child.wait();
    kill_tree(pid);
}

///  unmistakable 的自我毁灭形命令。这一条**不是一档权限能放开的**：
/// 即使全局档是 `full`，它也是 Deny 而不是弹窗——
/// 弹一个"要不要格式化系统盘"的确认框，本质上是把误点的责任留给用户
///
/// 判定只看规范化后的整词形状，不做 shell 解析：我们拦的是"一眼就该停"的那几条，
/// 不是要当一个能绕过一切写法的过滤器（真有那个决心的用户会直接在终端里做，
/// 而那也正是这条闸门该省下的对话）
pub fn is_catastrophic(command: &str) -> Option<&'static str> {
    let normalized = command
        .to_ascii_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    const HITS: [(&str, &str); 8] = [
        ("rm -rf /", "删除根目录"),
        ("rm -rf /*", "删除根目录"),
        ("mkfs", "格式化磁盘"),
        ("format c:", "格式化系统盘"),
        ("diskpart", "磁盘分区操作"),
        ("dd if=/dev/zero of=/dev", "把零写进块设备"),
        (":(){ :|:& };:", "fork 炸弹"),
        ("remove-item -recurse -force c:\\windows", "递归删除 Windows 目录"),
    ];
    HITS.iter().find(|(pattern, _)| normalized.contains(pattern)).map(|(_, why)| *why)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 环境这一条约束此前只有 `run_command` 一处过：MCP 服务器（常常是 `npx` 上的第三方包）
    /// 与插件钩子（一整行 shell）都把父进程的环境原样继承下去，而这两处的 stdout / stderr
    /// 一样会回到上下文。设计里"所有子进程同一个封装"那一句当时只在纸上。
    /// 这条钉三件事：三个 spawn 点都过它、MCP 的 `cmd.exe` 退路也过、清环境全库只有一份实现
    #[test]
    fn every_spawn_site_goes_through_the_one_environment_constraint() {
        // 针脚一律 concat!：这条测试自己就在被 include 的那批文件所在的仓库里
        let needle = concat!("constrain::constr", "ained(");
        let tools = include_str!("../tools.rs");
        let mcp = include_str!("../mcp.rs");
        let hooks = include_str!("../hooks.rs");
        assert_eq!(tools.matches(needle).count(), 2, "工具命令与 SSH 执行两条 spawn 都要过共用的约束");
        assert_eq!(
            mcp.matches(needle).count(),
            2,
            "MCP 有两次 spawn（正常启动与 cmd.exe 退路）——退路不是免检的理由"
        );
        assert_eq!(hooks.matches(needle).count(), 1, "插件钩子那条 spawn 没过共用的约束");

        let clear = concat!("env_", "clear()");
        let here = include_str!("constrain.rs").split("#[cfg(test)]").next().unwrap_or_default();
        assert_eq!(here.matches(clear).count(), 1, "清环境这件事只许有一处实现");
        for (name, source) in [("tools.rs", tools), ("mcp.rs", mcp), ("hooks.rs", hooks)] {
            assert_eq!(source.matches(clear).count(), 0, "{name} 里不该再写第二份清环境");
        }

        // 掐树同理：三份各写一遍的代价是哪一份没跟上没人知道
        let taskkill = concat!("task", "kill");
        assert_eq!(
            here.matches(taskkill).count(),
            1,
            "掐树的命令形状只许住在 `kill_command` 那一条 cfg 分支里"
        );
        for (name, source) in [("tools.rs", tools), ("mcp.rs", mcp), ("hooks.rs", hooks)] {
            assert_eq!(source.matches(taskkill).count(), 0, "{name} 里不该再拼第二份掐树命令");
        }
        assert_eq!(
            tools.matches(concat!("constrain::kill_", "tree(pid)")).count(),
            1,
            "工具命令的超时没走共用的收树"
        );
        assert_eq!(
            mcp.matches(concat!("constrain::reap_", "tree(")).count(),
            1,
            "停一台 MCP 服务器没走共用的收树：漏的是常驻的 node"
        );
        assert_eq!(
            hooks.matches(concat!("constrain::reap_", "tree(")).count(),
            3,
            "钩子有三条退路（收容建立失败、超时、等收尾出错），少一条就是留一树孤儿"
        );
    }

    /// `/T` 是 `kill_tree` 存在的全部理由：只掐 `cmd` / `sh` 那一层，它 spawn 出去的
    /// node、python 会继续活着，而我们已经把"超时"或"已停止"报给用户了。
    /// 这一格此前是 P0 四项执行约束里唯一没有任何测试的一项
    #[test]
    fn the_kill_asks_for_the_whole_tree_not_just_the_direct_child() {
        let (program, args) = kill_command(4242);
        let joined = args.join(" ");
        assert!(joined.contains("4242"), "要掐的是那一棵树，得带上 pid：{joined}");
        if cfg!(windows) {
            assert_eq!(program, "taskkill", "Windows 上只能借 taskkill：{program}");
            assert!(joined.contains("/T"), "少了 /T 就只杀掉父进程那一层：{joined}");
            assert!(joined.contains("/F"), "不强制的话它会等一个永远不会来的答复：{joined}");
        } else {
            assert_eq!(program, "kill", "非 Windows 走 kill：{program}");
        }
    }

    #[test]
    fn credential_shaped_names_never_reach_a_child_process() {
        for name in [
            "MY_API_KEY",
            "GH_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "DB_PASSWORD",
            "OPENAI_APIKEY",
            "SSH_PRIVATE_KEY",
            "GITHUB_CREDENTIALS",
            "lower_case_api_key",
        ] {
            assert!(is_credential_name(name), "{name} 看着就像凭据，却没被摘掉");
        }
    }

    #[test]
    fn the_variables_the_toolchain_needs_survive() {
        // 反过来也要测：把约束做成"顺手删掉一堆"会让 git 和构建当场瘫掉，
        // 而瘫掉的护栏只会被用户关掉
        for name in [
            "PATH",
            "HOME",
            "USERPROFILE",
            "SYSTEMROOT",
            "COMSPEC",
            "TEMP",
            "GOPATH",
            "JAVA_HOME",
            "CARGO_HOME",
            "NPM_CONFIG_PREFIX",
            "GIT_AUTHOR_NAME",
        ] {
            assert!(!is_credential_name(name), "{name} 被误伤了，命令跑不通");
        }
    }

    #[test]
    fn a_protected_name_is_kept_even_when_it_shouts_like_a_secret() {
        // PATH 自己不含凭据片段，但这条检查顺序是"先保护后识别"，别哪天反过来写
        assert!(!is_credential_name("PATH"));
        assert!(CREDENTIAL_HINTS.iter().any(|hint| "A_TOKEN".contains(hint)));
    }

    #[test]
    fn the_live_environment_actually_gets_filtered() {
        std::env::set_var("AGLAB_TEST_TOKEN", "smoking-gun");
        std::env::set_var("AGLAB_TEST_PLAIN", "kept");
        let env = child_env();
        assert!(
            !env.iter().any(|(name, value)| name == "AGLAB_TEST_TOKEN" && value == "smoking-gun"),
            "子进程环境里还留着刚设的 token"
        );
        assert!(env.iter().any(|(name, _)| name == "AGLAB_TEST_PLAIN"));
        std::env::remove_var("AGLAB_TEST_TOKEN");
        std::env::remove_var("AGLAB_TEST_PLAIN");
    }

    #[test]
    fn a_command_without_a_project_root_is_refused_instead_of_running_in_our_own_directory() {
        let error = require_cwd(None).expect_err("没有根必须拒");
        assert!(error.contains("项目目录"), "拒绝理由要说人话，模型才可能自己修：{error}");
        let dir = std::env::temp_dir();
        assert_eq!(require_cwd(Some(&dir)).ok().as_deref(), Some(dir.as_path()));
        let missing = dir.join("aglab-does-not-exist-here");
        assert!(require_cwd(Some(&missing)).is_err(), "不存在的路径不能当 cwd 交出去");
    }

    #[test]
    fn the_kill_targets_the_whole_tree_not_just_the_shell() {
        let (program, args) = kill_command(4321);
        assert!(
            args.iter().any(|flag| flag == "/T") || cfg!(not(windows)),
            "少了 /T 就只杀了 cmd 那一层，编译器还在后台跑"
        );
        assert!(args.iter().any(|flag| flag == "/F") || cfg!(not(windows)), "少了 /F 会被子进程的挽留对话框卡住");
        assert!(args.iter().any(|arg| arg == "4321"));
        assert!(!program.is_empty());
    }

    #[test]
    fn self_destructive_commands_are_denied_even_in_the_full_mode() {
        use serde_json::json;
        use crate::policy::{Decision, Mode, Policy};
        use crate::tool_runtime::{rule, Call};

        for (command, _) in [
            ("rm -rf /", "删除根目录"),
            ("sudo  rm   -rf /*", "删除根目录"),
            ("mkfs.ext4 /dev/sda1", "格式化磁盘"),
            ("format C:", "格式化系统盘"),
            (":(){ :|:& };:", "fork 炸弹"),
        ] {
            let args = json!({ "command": command });
            let root = std::env::temp_dir();
            let ruling = rule(
                &Policy::new(Mode::Full),
                &Call::new("run_command", &args, Some(&root), false),
                command,
                None,
            );
            assert!(
                matches!(ruling.decision, Decision::Deny { .. }),
                "full 档下 {command} 仍然要问都不问就拒：{ruling:?}"
            );
        }
        // 反向也要成立：日常命令不该被这条误伤
        for command in ["git status", "cargo test", "rm -rf ./target", "npm run build"] {
            let args = json!({ "command": command });
            let root = std::env::temp_dir();
            let ruling = rule(
                &Policy::new(Mode::Full),
                &Call::new("run_command", &args, Some(&root), false),
                command,
                None,
            );
            assert!(ruling.is_allow(), "{command} 是日常活，不该被自我毁灭那条误伤：{ruling:?}");
        }
    }

    #[test]
    fn the_time_and_output_budgets_are_stated_in_one_place() {
        // 这两个数是给 UI 文案和 tests 共用的：哪里再出现第二个 60 秒或 32K，就是第二真相
        assert_eq!(COMMAND_TIMEOUT.as_secs(), 60);
        assert_eq!(MAX_COMMAND_OUTPUT, 32 * 1024);
    }
}
