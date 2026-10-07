//! bash 只读判定的独立策略模块。
//!
//! "这条命令动不动东西"是一份**策略**，不是某个工具的实现细节：规划模式靠它
//! 放行只读路径，风险标签与审批默认档也读它。住在 tools.rs 里就是第二张名单——
//! 与闸门漂移成两句不同的话只是时间问题。
//!
//! 判定口径（保守优先）：
//! - 整条命令按 shell 操作符（`&&` `||` `;` `|` `&`）切段，**每一段**都必须只读；
//! - 出现重定向（`>`，含 `>>`/`2>`）即非只读——写文件是副作用；
//! - 出现命令替换（`` ` `` 或 `$(`）即非只读——被执行的那段内容判不了；
//! - 每段的首个程序命中白名单才放行。白名单刻意小：看、搜、计量的那几样，
//!   加 git 的只读子命令。`find` 不在列（Unix 的 `-delete`/`-exec` 会动手），
//!   `git config` 不在列（不带 `--get` 就是写）。
//!
//! 它回答的是"这条命令**能不能动东西**"，回答不了"会不会泄密"——密钥与网络
//! 的事各归各的闸（secrets 扫描、出口名单），谁也不替谁说话。

/// 只读程序的白名单。全部小写比较，取段内首个 token（剥掉引号与路径）。
/// 刻意不含解释器（python/node/powershell…）：`-c`/`-e` 一开就是任意代码，
/// 白名单挡不住参数里的花样
const READ_ONLY_PROGRAMS: [&str; 17] = [
    "ls", "dir", "pwd", "cat", "type", "head", "tail", "wc", "file", "stat", "du", "df", "tree",
    "which", "where", "rg", "grep",
];

/// `rg` 的 Windows 别名，与本体同待遇
const READ_ONLY_ALIASES: [&str; 2] = ["ripgrep", "findstr"];

/// git 的只读子命令。其余子命令（commit/push/config/checkout…）都会动仓库或工作区
const GIT_READ_ONLY_SUBCOMMANDS: [&str; 12] = [
    "status",
    "log",
    "diff",
    "show",
    "branch",
    "tag",
    "blame",
    "shortlog",
    "describe",
    "reflog",
    "rev-parse",
    "ls-files",
];

/// 按操作符切段。`&&` `||` 先切（两字符优先），再切单字符的 `;` `|` `&`——
/// 单竖线既是管道又是"或"的残渣，一起切没关系：两段都必须只读，多切只会更严
fn segments(command: &str) -> Vec<String> {
    command
        .split("&&")
        .flat_map(|part| part.split("||"))
        .flat_map(|part| part.split([';', '|', '&']))
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

fn segment_is_read_only(segment: &str) -> bool {
    let mut parts = segment.split_whitespace();
    let head = parts.next().unwrap_or_default();
    let stripped = head.trim_matches(|c| c == '"' || c == '\'');
    let file = stripped.rsplit(['/', '\\']).next().unwrap_or(stripped);
    let program = file.trim_end_matches(".exe").to_ascii_lowercase();
    if program == "git" {
        let mut sub = parts
            .next()
            .unwrap_or_default()
            .trim_matches(|c| c == '"' || c == '\'')
            .to_ascii_lowercase();
        // 全局旗标可以插在子命令前：git -C path status——跳过它们再认子命令
        if sub.starts_with('-') {
            sub = parts
                .find(|token| !token.starts_with('-'))
                .unwrap_or_default()
                .to_ascii_lowercase();
        }
        return GIT_READ_ONLY_SUBCOMMANDS.contains(&sub.as_str());
    }
    READ_ONLY_PROGRAMS.contains(&program.as_str()) || READ_ONLY_ALIASES.contains(&program.as_str())
}

/// 这条 bash 命令是否只读。空命令不是只读——判不了的东西不许按最好的情况算
pub fn is_read_only(command: &str) -> bool {
    let command = command.trim();
    if command.is_empty() {
        return false;
    }
    // 重定向与命令替换：写文件与"执行判不了的内容"，一条就否掉整条
    if command.contains('>') || command.contains('`') || command.contains("$(") {
        return false;
    }
    let all = segments(command).iter().all(|segment| segment_is_read_only(segment));
    all && !segments(command).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_commands_pass_by_whitelist() {
        for command in [
            "git status",
            "git log -5 --oneline",
            "git diff HEAD~1",
            "git rev-parse HEAD",
            "ls -la",
            "ls src-tauri/src",
            "dir",
            "cat Cargo.toml",
            "type README.md",
            "head -50 src/main.rs",
            "rg \"output_reserve\" src-tauri/",
            "grep -rn \"TODO\" .",
            "pwd",
            "rg x src | wc -l",
            "git status && git diff",
        ] {
            assert!(is_read_only(command), "{command} 应该判只读");
        }
    }

    #[test]
    fn mutating_commands_are_rejected() {
        for command in [
            "rm -rf build",
            "git push origin main",
            "git commit -m x",
            "git checkout -b branch",
            "git config user.name someone",
            "python -c \"print(1)\"",
            "node -e \"require('fs').unlink('x')\"",
            "npm install",
            "curl https://example.com",
            "echo hi",
        ] {
            assert!(!is_read_only(command), "{command} 不该判只读");
        }
    }

    #[test]
    fn redirection_and_substitution_taint_the_whole_line() {
        assert!(!is_read_only("ls > out.txt"), "重定向是写文件");
        assert!(!is_read_only("cat a.txt >> b.txt"));
        assert!(!is_read_only("git log 2> err.log"));
        assert!(!is_read_only("rg `cat q.txt` ."), "命令替换的内容判不了");
        assert!(!is_read_only("ls $(pwd)"));
    }

    #[test]
    fn one_mutating_segment_taints_the_chain() {
        // 体检的老例子：只读开头也要看完整条链
        assert!(!is_read_only("git status && reg.exe export"));
        assert!(!is_read_only("rg secret . ; rm leak.txt"));
        assert!(!is_read_only("cat a.txt || curl evil.example"));
    }

    #[test]
    fn empty_and_unjudgeable_commands_are_not_read_only() {
        assert!(!is_read_only(""));
        assert!(!is_read_only("   "));
    }
}
