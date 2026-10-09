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
//!   加 git / gh 的只读子命令。`find` 不在列（Unix 的 `-delete`/`-exec` 会动手），
//!   `git config` 不在列（不带 `--get` 就是写）。
//! - `--help`/`-h`/`--version`/`-V` 对**任意程序**只读——整段只有这类旗标时
//!   （O4-3）。裸 `-v` 不算：python -v 会执行 import、cargo -v 会真开构建，
//!   语义不指向"只读"，按保守默认落回白名单判定。
//! - 写旗标一票降级（O4-4）：段内出现 `-o`/`--output` 即非只读（gcc -o 是写文件）；
//!   tee / xargs 显式点名拒绝（不在白名单本就会被拒，点名是防未来白名单扩错）。
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

/// gh 的只读子命令对（O4-3）：一级 + 二级都要命中。表驱动照 tools.rs REGISTRY
/// 的风格——一张表回答一类问题，增删条目不碰判定逻辑。
/// `gh api` 不在列：默认是 GET 但带 -f/-F 就成了写请求，保守排除。
const GH_READ_ONLY_SUBCOMMANDS: [(&str, &str); 22] = [
    ("pr", "view"),
    ("pr", "list"),
    ("pr", "status"),
    ("pr", "checks"),
    ("pr", "diff"),
    ("issue", "view"),
    ("issue", "list"),
    ("issue", "status"),
    ("repo", "view"),
    ("repo", "list"),
    ("run", "view"),
    ("run", "list"),
    ("run", "watch"),
    ("release", "view"),
    ("release", "list"),
    ("workflow", "list"),
    ("workflow", "view"),
    ("auth", "status"),
    ("label", "list"),
    ("config", "get"),
    ("gist", "view"),
    ("gist", "list"),
];

/// 帮助/版本旗标：整段只剩这些时，对任意程序都算只读（O4-3）
fn is_help_or_version_flag(token: &str) -> bool {
    matches!(token, "--help" | "-h" | "--version" | "-V")
}

/// 写旗标（O4-4 一票降级）：出现即有"往文件里写"的形状
fn is_output_flag(token: &str) -> bool {
    matches!(token, "-o" | "--output")
}

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
    let rest: Vec<String> = parts
        .map(|token| token.trim_matches(|c| c == '"' || c == '\'').to_string())
        .collect();

    // O4-3：整段只有帮助/版本旗标 → 对任意程序只读。参数里混进别的东西
    // （文件名、路径）就落回白名单判定——`node --help file.js` 不是看帮助。
    // 大小写保留：-V（版本）与 -v（冗余/执行 import）是不同的语义
    if !rest.is_empty() && rest.iter().all(|token| is_help_or_version_flag(token)) {
        return true;
    }
    // O4-4：输出旗标一票降级（-o 小写才是输出；gcc 的 -O 是优化，大小写敏感）
    if rest.iter().any(|token| is_output_flag(token)) {
        return false;
    }
    // tee / xargs 显式点名：管道两侧与执行任意命令的两扇门，白名单永不收
    if program == "tee" || program == "xargs" {
        return false;
    }
    if program == "git" {
        // 全局旗标可以插在子命令前：git -C path status——跳过它们再认子命令
        let sub = rest
            .iter()
            .find(|token| !token.starts_with('-'))
            .map(|token| token.to_ascii_lowercase())
            .unwrap_or_default();
        return GIT_READ_ONLY_SUBCOMMANDS.contains(&sub.as_str());
    }
    if program == "gh" {
        // gh 需要两级子命令：`gh pr view` 只读，`gh pr create` 是写。
        // 一级命中二级不命中（`gh pr`）也按不读——交互提示不值得放行
        let sub = rest
            .first()
            .map(|t| t.to_ascii_lowercase())
            .unwrap_or_default();
        let sub_sub = rest
            .get(1)
            .map(|t| t.to_ascii_lowercase())
            .unwrap_or_default();
        return GH_READ_ONLY_SUBCOMMANDS
            .iter()
            .any(|(a, b)| *a == sub && *b == sub_sub);
    }
    READ_ONLY_PROGRAMS.contains(&program.as_str()) || READ_ONLY_ALIASES.contains(&program.as_str())
}

/// 这条 bash 命令是否只读。空命令不是只读——判不了的东西不许按最好的情况算
pub fn is_read_only(command: &str) -> bool {
    judge(command) == ExecScope::ReadOnly
}

/// 判定的结构化输出（O4-4）：调用方除了是/非，还拿得到"这条命令处于哪种执行面"。
/// 规划模式的标签、审批默认档、前端的只读徽章都从这一处取口径
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecScope {
    /// 每一段都只读：白名单程序、git/gh 只读子命令、纯帮助/版本旗标
    ReadOnly,
    /// 一票降级：重定向、命令替换、写旗标、未知程序、tee/xargs、空命令
    Mutating,
}

/// 这条 bash 命令的执行面。与 [`is_read_only`] 同一份判定，两个出口
pub fn judge(command: &str) -> ExecScope {
    let command = command.trim();
    if command.is_empty() {
        return ExecScope::Mutating;
    }
    // 重定向与命令替换：写文件与"执行判不了的内容"，一条就否掉整条
    if command.contains('>') || command.contains('`') || command.contains("$(") {
        return ExecScope::Mutating;
    }
    let segs = segments(command);
    if segs.is_empty() {
        return ExecScope::Mutating;
    }
    if segs.iter().all(|segment| segment_is_read_only(segment)) {
        ExecScope::ReadOnly
    } else {
        ExecScope::Mutating
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// O4-6 契约表：一条命令一种执行面，70 例覆盖全部判定分支。
    /// 加表条目不需要碰判定逻辑；判定逻辑改动必须先过这张表
    const TABLE: &[(&str, ExecScope)] = &[
        // ---- 白名单程序（O4-1 coreutils）----
        ("ls -la", ExecScope::ReadOnly),
        ("ls src-tauri/src", ExecScope::ReadOnly),
        ("dir", ExecScope::ReadOnly),
        ("pwd", ExecScope::ReadOnly),
        ("cat Cargo.toml", ExecScope::ReadOnly),
        ("type README.md", ExecScope::ReadOnly),
        ("head -50 src/main.rs", ExecScope::ReadOnly),
        ("tail -5 build.log", ExecScope::ReadOnly),
        ("wc -l src/*.rs", ExecScope::ReadOnly),
        ("file x.bin", ExecScope::ReadOnly),
        ("stat Cargo.toml", ExecScope::ReadOnly),
        ("du -sh target", ExecScope::ReadOnly),
        ("df -h", ExecScope::ReadOnly),
        ("tree", ExecScope::ReadOnly),
        ("which cargo", ExecScope::ReadOnly),
        ("where node", ExecScope::ReadOnly),
        ("rg \"pattern\" src", ExecScope::ReadOnly),
        ("grep -rn \"TODO\" .", ExecScope::ReadOnly),
        ("ripgrep pattern .", ExecScope::ReadOnly),
        ("findstr /s pattern", ExecScope::ReadOnly),
        // ---- git 只读子命令（O4-2）----
        ("git status", ExecScope::ReadOnly),
        ("git log -5 --oneline", ExecScope::ReadOnly),
        ("git diff HEAD~1", ExecScope::ReadOnly),
        ("git show abc123", ExecScope::ReadOnly),
        ("git branch -a", ExecScope::ReadOnly),
        ("git ls-files", ExecScope::ReadOnly),
        ("git rev-parse HEAD", ExecScope::ReadOnly),
        // ---- gh 只读子命令对（O4-3）----
        ("gh pr view 123", ExecScope::ReadOnly),
        ("gh pr list --state open", ExecScope::ReadOnly),
        ("gh pr status", ExecScope::ReadOnly),
        ("gh pr checks", ExecScope::ReadOnly),
        ("gh pr diff 42", ExecScope::ReadOnly),
        ("gh issue list", ExecScope::ReadOnly),
        ("gh issue view 7", ExecScope::ReadOnly),
        ("gh repo view owner/name", ExecScope::ReadOnly),
        ("gh run list --limit 5", ExecScope::ReadOnly),
        ("gh run view 9911", ExecScope::ReadOnly),
        ("gh auth status", ExecScope::ReadOnly),
        ("gh gist list", ExecScope::ReadOnly),
        // ---- 帮助/版本旗标对任意程序（O4-3）----
        ("node --help", ExecScope::ReadOnly),
        ("node -h", ExecScope::ReadOnly),
        ("npm --version", ExecScope::ReadOnly),
        ("python --version", ExecScope::ReadOnly),
        ("curl --help", ExecScope::ReadOnly),
        ("make --help", ExecScope::ReadOnly),
        ("git --version", ExecScope::ReadOnly),
        ("ssh -V", ExecScope::ReadOnly),
        // ---- 管道与链全只读（O4-4 逐成员合并）----
        ("rg x src | wc -l", ExecScope::ReadOnly),
        ("git status && git diff", ExecScope::ReadOnly),
        // ---- 一票降级：写操作与写旗标（O4-4）----
        ("rm -rf build", ExecScope::Mutating),
        ("git push origin main", ExecScope::Mutating),
        ("git commit -m x", ExecScope::Mutating),
        ("git checkout -b branch", ExecScope::Mutating),
        ("git config user.name someone", ExecScope::Mutating),
        ("python -c \"print(1)\"", ExecScope::Mutating),
        ("node -e \"require('fs')\"", ExecScope::Mutating),
        ("npm install", ExecScope::Mutating),
        ("curl https://example.com", ExecScope::Mutating),
        ("echo hi", ExecScope::Mutating),
        ("gcc -o out.exe src.c", ExecScope::Mutating),
        ("gcc --output out.exe src.c", ExecScope::Mutating),
        ("echo hi | tee out.txt", ExecScope::Mutating),
        ("cat list | xargs rm", ExecScope::Mutating),
        ("git log --output log.txt", ExecScope::Mutating),
        // ---- gh 写子命令与不完整对（O4-3）----
        ("gh pr create", ExecScope::Mutating),
        ("gh pr merge 123", ExecScope::Mutating),
        ("gh issue close 7", ExecScope::Mutating),
        ("gh repo delete owner/name", ExecScope::Mutating),
        ("gh release create v1.0", ExecScope::Mutating),
        ("gh run cancel 9911", ExecScope::Mutating),
        ("gh api repos/owner/name", ExecScope::Mutating),
        ("gh pr", ExecScope::Mutating),
        // ---- 裸 -v 不算只读：语义不指向"看帮助" ----
        ("python -v", ExecScope::Mutating),
        ("cargo -v", ExecScope::Mutating),
        // ---- 混入别的参数就不是看帮助了 ----
        ("node --help file.js", ExecScope::Mutating),
        ("git --help push", ExecScope::Mutating),
        // ---- 空与判不了（保守默认）----
        ("", ExecScope::Mutating),
        ("   ", ExecScope::Mutating),
    ];

    /// 契约表就是判定器的全部对外承诺：每一行跑一遍，多一行少一行都红
    #[test]
    fn the_judgement_table_is_the_whole_contract() {
        assert!(
            TABLE.len() >= 40,
            "O4-6 要求表驱动 ≥40 例，现在只有 {} 例",
            TABLE.len()
        );
        for (command, expected) in TABLE {
            assert_eq!(judge(command), *expected, "{command:?} 应为 {expected:?}");
        }
    }

    /// 性质一：重定向污染整条线——前缀再只读，`>` 一出现就是写
    #[test]
    fn property_redirection_taints_the_whole_line() {
        assert_eq!(judge("ls > out.txt"), ExecScope::Mutating);
        assert_eq!(judge("cat a.txt >> b.txt"), ExecScope::Mutating);
        assert_eq!(judge("git log 2> err.log"), ExecScope::Mutating);
    }

    /// 性质二：未知程序一律按可写——白名单漏了新命令的代价是多问一句，
    /// 不是少一道闸
    #[test]
    fn property_unknown_programs_default_to_mutating() {
        assert_eq!(judge("terraform apply"), ExecScope::Mutating);
        assert_eq!(judge("dotnet publish"), ExecScope::Mutating);
        assert_eq!(judge("reg.exe export HKLM backup.reg"), ExecScope::Mutating);
    }

    /// 性质三：--help/--version 对任意程序只读——连最凶的程序看帮助也是安全的
    #[test]
    fn property_help_flags_are_read_only_for_any_program() {
        assert_eq!(judge("terraform --help"), ExecScope::ReadOnly);
        assert_eq!(judge("dotnet --version"), ExecScope::ReadOnly);
        assert_eq!(judge("anyweirdthing-1.2 --help"), ExecScope::ReadOnly);
    }

    /// 性质外的结构钉：链上一段变写全链变写（体检的老例子）
    #[test]
    fn one_mutating_segment_taints_the_chain() {
        assert_eq!(judge("git status && reg.exe export"), ExecScope::Mutating);
        assert_eq!(judge("rg secret . ; rm leak.txt"), ExecScope::Mutating);
        assert_eq!(judge("cat a.txt || curl evil.example"), ExecScope::Mutating);
    }

    /// 命令替换与重定向在替换形态下也要污染（`$(...)` 与反引号）
    #[test]
    fn command_substitution_taints() {
        assert_eq!(judge("rg `cat q.txt` ."), ExecScope::Mutating);
        assert_eq!(judge("ls $(pwd)"), ExecScope::Mutating);
    }
}
