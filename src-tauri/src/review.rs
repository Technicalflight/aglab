use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use serde::Serialize;
use serde_json::json;
use tauri::AppHandle;

use crate::chat;
use crate::config;

const MAX_PATCH_CHARS: usize = 24_000;
const MAX_COMMITS: usize = 40;
/// 单文件差异的字符上限。超了如实标注截断，不假装看到的就是全部
const MAX_FILE_DIFF_CHARS: usize = 120_000;
/// 未跟踪文件的个数上限：一个没被 .gitignore 收住的构建目录能吐出几万个路径
const MAX_UNTRACKED_FILES: usize = 300;
/// 判断未跟踪文件是不是文本时只看开头这些字节。edits 的预览走同一个判据，
/// 免得一处当文本、另一处当二进制
pub(crate) const SNIFF_BYTES: usize = 8_192;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitBrief {
    pub sha: String,
    pub subject: String,
}

/// 一个文件在某一侧（相对基线 / 工作目录）的改动形状
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSide {
    /// git 的状态码：A/M/D/R/C/T/U，未跟踪用 "?"，二进制用 "B"
    pub state: String,
    pub additions: u32,
    pub deletions: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChange {
    pub path: String,
    /// 重命名与复制的旧路径。以前只留新路径，diff 的 --- +++ 头就对不上
    pub old_path: Option<String>,
    /// 相对基线已提交的那一侧；没有则 None
    pub committed: Option<FileSide>,
    /// 工作目录未提交的那一侧（含未跟踪文件）；没有则 None
    pub working: Option<FileSide>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewInfo {
    pub root: String,
    pub branch: String,
    pub head: String,
    /// 比较基线，"@{u}" / "origin/main" 这类；空串表示仓库里没有可比的基线
    pub base: String,
    pub ahead: u32,
    pub behind: u32,
    pub commits: Vec<CommitBrief>,
    pub files: Vec<FileChange>,
    /// 未跟踪文件超过上限时没列进来的个数，界面要如实说出来
    pub untracked_omitted: u32,
    /// 交给模型的差异行数。正文只在服务端拼，不再每次刷新过一遍 IPC
    pub patch_lines: u32,
    pub patch_truncated: bool,
    /// 只喂提示词，不发给前端
    #[serde(skip)]
    pub patch: String,
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = crate::childproc::hide(Command::new("git"))
        // 关掉路径转义，否则中文文件名会变成八进制串
        .args(["-c", "core.quotepath=false"])
        .args(args)
        .current_dir(root)
        // 任何需要交互的 git 都直接失败，而不是把界面挂住
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .map_err(|e| format!("调用 git 失败：{e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("git {} 执行失败。", args.join(" "))
        } else {
            stderr
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string())
}

fn workspace(app: &AppHandle) -> Result<PathBuf, String> {
    let config = config::load(app);
    let project = config
        .active_project()
        .ok_or_else(|| "还没有绑定工作目录：先在输入框上方选一个目录。".to_string())?;

    let path = PathBuf::from(&project.path);
    if !path.is_dir() {
        return Err(format!("工作目录目录已经不在了：{}", project.path));
    }
    Ok(path)
}

/// 比较基线：先看当前分支的上游，再看远端默认分支，最后试常见的主分支名
fn resolve_base(root: &Path) -> String {
    for candidate in ["@{u}", "origin/HEAD", "origin/main", "origin/master"] {
        if git(root, &["rev-parse", "--verify", "--quiet", candidate]).is_ok() {
            return candidate.to_string();
        }
    }
    String::new()
}

fn parse_commits(raw: &str) -> Vec<CommitBrief> {
    raw.lines()
        .filter_map(|line| {
            let (sha, subject) = line.split_once('\u{1f}')?;
            Some(CommitBrief {
                sha: sha.to_string(),
                subject: subject.to_string(),
            })
        })
        .collect()
}

/// -z 输出的字段是 NUL 结尾的，按行切会把它们粘在一起
fn fields(raw: &str) -> Vec<String> {
    raw.split('\0')
        .filter(|item| !item.is_empty())
        .map(String::from)
        .collect()
}

/// 状态码后面跟几个路径：重命名和复制有两个（旧、新），其余一个
fn path_count(code: char) -> usize {
    matches!(code, 'R' | 'C') as usize + 1
}

/// 把一次 `git diff` 的 name-status 与 numstat 拼成文件清单。
///
/// 两份输出按同一顺序逐条对应，所以用 name-status 的状态码决定 numstat
/// 那条要消费几个路径字段——重命名时 numstat 不会自己说它是重命名
fn diff_changes(root: &Path, range: &str) -> Vec<FileChange> {
    let status_raw = match git(root, &["diff", "--name-status", "-z", "-M", range]) {
        Ok(value) => value,
        Err(_) => return Vec::new(),
    };
    let numstat_raw = git(root, &["diff", "--numstat", "-z", "-M", range]).unwrap_or_default();

    let statuses = fields(&status_raw);
    let numbers = fields(&numstat_raw);

    let mut changes = Vec::new();
    let mut cursor = 0usize;

    let mut tokens = statuses.iter();
    while let Some(status) = tokens.next() {
        let Some(code) = status.chars().next() else {
            continue;
        };
        let slots = path_count(code);
        let mut paths: Vec<&String> = Vec::new();
        for _ in 0..slots {
            match tokens.next() {
                Some(path) => paths.push(path),
                None => break,
            }
        }
        if paths.len() != slots {
            break;
        }
        // 重命名取新路径当展示路径，旧路径单独留着给 diff 头用
        let path = paths[slots - 1].to_string();
        let old_path = if slots == 2 {
            Some(paths[0].to_string())
        } else {
            None
        };

        let (additions, deletions) = match numbers.get(cursor) {
            Some(counts) => {
                let mut columns = counts.split('\t');
                // "-" 是二进制：git 不数它的行数，我们也不假装数得出来
                let parsed = |column: Option<&str>| column.and_then(|value| value.parse().ok());
                (
                    parsed(columns.next()).unwrap_or(0),
                    parsed(columns.next()).unwrap_or(0),
                )
            }
            None => (0, 0),
        };
        cursor += 1;

        changes.push(FileChange {
            path,
            old_path,
            committed: Some(FileSide {
                state: code.to_string(),
                additions,
                deletions,
            }),
            working: None,
        });
    }

    changes
}

/// 工作目录未提交的改动：已跟踪的走 `git diff HEAD`（含暂存区），未跟踪的单独补
fn working_changes(root: &Path) -> (Vec<FileChange>, u32) {
    let mut changes = diff_changes(root, "HEAD");
    for change in &mut changes {
        if let Some(side) = change.committed.take() {
            change.working = Some(side);
        }
    }

    let untracked = match git(root, &["ls-files", "--others", "--exclude-standard", "-z"]) {
        Ok(raw) => raw,
        Err(_) => return (changes, 0),
    };

    let untracked_list = fields(&untracked);
    let paths: Vec<&String> = untracked_list.iter().collect();
    let omitted = paths.len().saturating_sub(MAX_UNTRACKED_FILES) as u32;

    for path in paths.into_iter().take(MAX_UNTRACKED_FILES) {
        changes.push(FileChange {
            path: path.to_string(),
            old_path: None,
            committed: None,
            working: Some(untracked_side(root, path)),
        });
    }

    (changes, omitted)
}

/// 未跟踪文件没有 git 侧的行数可问，只能自己数。符号链接一律不跟——
/// 仓库里放一个指向 ~/.ssh 的链接，跟着读就把私钥显示在界面上了
fn untracked_side(root: &Path, rel: &str) -> FileSide {
    let binary = || FileSide {
        state: "B".into(),
        additions: 0,
        deletions: 0,
    };
    let unknown = || FileSide {
        state: "?".into(),
        additions: 0,
        deletions: 0,
    };

    let full = root.join(rel);
    match fs::symlink_metadata(&full) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => return binary(),
        Ok(_) => {}
        Err(_) => return unknown(),
    }

    let bytes = match fs::read(&full) {
        Ok(bytes) => bytes,
        Err(_) => return unknown(),
    };
    if bytes[..bytes.len().min(SNIFF_BYTES)].contains(&0) {
        return binary();
    }

    let lines = bytes.iter().filter(|byte| **byte == b'\n').count() as u32
        + u32::from(bytes.last().is_some_and(|byte| *byte != b'\n'));
    FileSide {
        state: "?".into(),
        additions: lines,
        deletions: 0,
    }
}

/// 两侧按路径合并成一个清单。同一个文件既可能已提交过、又还有未提交改动，
/// 两侧各自保留自己的数字——加起来会是一个谁都不代表的数
fn merge_sides(committed: Vec<FileChange>, working: Vec<FileChange>) -> Vec<FileChange> {
    let mut merged: BTreeMap<String, FileChange> = BTreeMap::new();

    for change in committed {
        merged.insert(
            change.path.clone(),
            FileChange {
                working: None,
                ..change
            },
        );
    }
    for change in working {
        merged
            .entry(change.path.clone())
            .and_modify(|entry| {
                entry.working = change.working.clone();
                if entry.old_path.is_none() {
                    entry.old_path = change.old_path.clone();
                }
            })
            .or_insert(change);
    }

    merged.into_values().collect()
}

fn collect(app: &AppHandle) -> Result<ReviewInfo, String> {
    collect_at(&workspace(app)?)
}

fn collect_at(root: &Path) -> Result<ReviewInfo, String> {
    let inside = git(root, &["rev-parse", "--is-inside-work-tree"])
        .map_err(|_| format!("{} 不是 git 仓库。", root.display()))?;
    if inside != "true" {
        return Err(format!("{} 不是 git 仓库。", root.display()));
    }

    let branch = git(root, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    let head = git(root, &["rev-parse", "--short", "HEAD"]).unwrap_or_default();
    let base = resolve_base(root);

    let (ahead, behind) = if base.is_empty() {
        (0, 0)
    } else {
        git(
            root,
            &[
                "rev-list",
                "--left-right",
                "--count",
                &format!("{base}...HEAD"),
            ],
        )
        .ok()
        .and_then(|line| {
            let mut columns = line.split_whitespace();
            let behind: u32 = columns.next()?.parse().ok()?;
            let ahead: u32 = columns.next()?.parse().ok()?;
            Some((ahead, behind))
        })
        .unwrap_or((0, 0))
    };

    // 有基线就只列基线之上的提交；没有基线时列最近的提交，界面上会说明没有可比对象
    let log_range = if base.is_empty() {
        "HEAD".to_string()
    } else {
        format!("{base}..HEAD")
    };
    let commits = git(
        root,
        &[
            "log",
            "--no-color",
            &format!("-n{MAX_COMMITS}"),
            // 用 %x1f 而不是往命令行里塞控制字符：Windows 传参时后者会被吃掉
            "--pretty=format:%h%x1f%s",
            &log_range,
        ],
    )
    .map(|raw| parse_commits(&raw))
    .unwrap_or_default();

    let diff_range = if base.is_empty() {
        "HEAD".to_string()
    } else {
        format!("{base}...HEAD")
    };
    let committed = diff_changes(root, &diff_range);
    let (working, untracked_omitted) = working_changes(root);
    let files = merge_sides(committed, working);

    let (patch, patch_truncated) = match git(root, &["diff", &diff_range]) {
        Ok(raw) if raw.chars().count() > MAX_PATCH_CHARS => {
            let head: String = raw.chars().take(MAX_PATCH_CHARS).collect();
            (head, true)
        }
        Ok(raw) => (raw, false),
        Err(_) => (String::new(), false),
    };
    let patch_lines = patch.lines().count() as u32;

    Ok(ReviewInfo {
        root: root.display().to_string(),
        branch,
        head,
        base,
        ahead,
        behind,
        commits,
        files,
        untracked_omitted,
        patch_lines,
        patch_truncated,
        patch,
    })
}

fn prompt_for(info: &ReviewInfo) -> String {
    let mut text = String::new();
    text.push_str(&format!(
        "仓库：{}\n当前分支：{}（HEAD {}）\n比较基线：{}\n领先 {} 个提交，落后 {} 个提交\n",
        info.root,
        info.branch,
        info.head,
        if info.base.is_empty() {
            "（没有上游或主分支可比）"
        } else {
            &info.base
        },
        info.ahead,
        info.behind
    ));

    text.push_str("\n提交：\n");
    if info.commits.is_empty() {
        text.push_str("（无）\n");
    }
    for commit in &info.commits {
        text.push_str(&format!("- {} {}\n", commit.sha, commit.subject));
    }

    text.push_str("\n改动文件（committed=相对基线已提交，working=还没提交）：\n");
    if info.files.is_empty() {
        text.push_str("（无）\n");
    }
    for file in &info.files {
        let describe = |name: &str, side: &FileSide| {
            format!(
                "{} {} +{} -{}",
                name, side.state, side.additions, side.deletions
            )
        };
        let mut parts = Vec::new();
        if let Some(side) = &file.committed {
            parts.push(describe("committed", side));
        }
        if let Some(side) = &file.working {
            parts.push(describe("working", side));
        }
        text.push_str(&format!("- {} | {}\n", file.path, parts.join(" | ")));
    }

    if info.untracked_omitted > 0 {
        text.push_str(&format!(
            "\n另有 {} 个未跟踪文件未列出。\n",
            info.untracked_omitted
        ));
    }

    if !info.patch.is_empty() {
        text.push_str("\n差异（");
        text.push_str(if info.patch_truncated {
            "已截断"
        } else {
            "完整"
        });
        text.push_str("）：\n");
        text.push_str(&info.patch);
        text.push('\n');
    }

    text
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffLine {
    /// context / added / removed
    pub kind: String,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffHunk {
    /// git 原样的块头，含函数上下文提示
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDiff {
    pub path: String,
    pub old_path: Option<String>,
    pub scope: String,
    pub hunks: Vec<DiffHunk>,
    pub binary: bool,
    pub truncated: bool,
    pub additions: u32,
    pub deletions: u32,
}

/// 前端能传进来的路径必须留在仓库内。这个应用别处没有路径约束，
/// 这条新命令不能顺手开一个读仓库外文件的口子
fn guard_relative(rel: &str) -> Result<&Path, String> {
    let path = Path::new(rel);
    if rel.is_empty() {
        return Err("路径为空。".into());
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => return Err(format!("路径不能越出仓库根：{rel}")),
        }
    }
    Ok(path)
}

/// 解析单文件 unified diff。行号在这里算清，前端两种布局都只消费结果
fn parse_file_diff(raw: &str, scope: &str, path: &str, old_path: Option<String>) -> FileDiff {
    let mut diff = FileDiff {
        path: path.to_string(),
        old_path,
        scope: scope.to_string(),
        hunks: Vec::new(),
        binary: false,
        truncated: false,
        additions: 0,
        deletions: 0,
    };

    let mut hunk: Option<DiffHunk> = None;
    let (mut old_no, mut new_no) = (0u32, 0u32);
    let mut seen_diff_header = false;

    for line in raw.lines() {
        if line.starts_with("diff --git ") {
            // 一次只问一个文件；出现第二个头说明范围串了，停在这里比硬解更诚实
            if seen_diff_header {
                break;
            }
            seen_diff_header = true;
            continue;
        }
        if line.starts_with("GIT binary patch") || line.starts_with("Binary files ") {
            diff.binary = true;
            break;
        }

        if let Some(header) = line.strip_prefix("@@ ") {
            let Some((old_start, new_start)) = parse_hunk_header(header) else {
                continue;
            };
            if let Some(done) = hunk.take() {
                diff.hunks.push(done);
            }
            old_no = old_start;
            new_no = new_start;
            hunk = Some(DiffHunk {
                header: line.to_string(),
                lines: Vec::new(),
            });
            continue;
        }

        // 第一个块头之前是文件级元数据。旧路径只从 rename/copy 行取——
        // "--- a/自身" 对普通修改也成立，拿它兜底会让每个文件都显示成重命名
        if hunk.is_none() {
            for prefix in ["rename from ", "copy from "] {
                if let Some(old) = line.strip_prefix(prefix) {
                    diff.old_path = Some(old.to_string());
                    break;
                }
            }
            continue;
        }

        let current = hunk.as_mut().expect("checked above");

        // git 对空的上下文行有时整行就是空串，split_at(1) 会当场 panic
        let (marker, text) = line.split_at_checked(1).unwrap_or((" ", ""));
        let entry = match marker {
            "+" => {
                diff.additions += 1;
                new_no += 1;
                DiffLine {
                    kind: "added".into(),
                    old_no: None,
                    new_no: Some(new_no - 1),
                    text: text.to_string(),
                }
            }
            "-" => {
                diff.deletions += 1;
                old_no += 1;
                DiffLine {
                    kind: "removed".into(),
                    old_no: Some(old_no - 1),
                    new_no: None,
                    text: text.to_string(),
                }
            }
            // 没有前缀的行是 "\ No newline at end of file" 这类注记：
            // 原样作为上下文行显示，不替 git 圆场
            _ => {
                old_no += 1;
                new_no += 1;
                DiffLine {
                    kind: "context".into(),
                    old_no: Some(old_no - 1),
                    new_no: Some(new_no - 1),
                    text: line.strip_prefix(' ').unwrap_or(line).to_string(),
                }
            }
        };
        current.lines.push(entry);
    }
    if let Some(done) = hunk {
        diff.hunks.push(done);
    }

    diff
}

/// "@@ -12,7 +12,9 @@ fn foo" → (12, 12)。旧新两段各是一个空白分隔的 token。
/// 计数这里用不上，行号是逐行推出来的，比信一个总数更稳
fn parse_hunk_header(header: &str) -> Option<(u32, u32)> {
    let mut ranges = header.split_whitespace();
    let old = ranges.next()?.strip_prefix('-')?;
    let new = ranges.next()?.strip_prefix('+')?;
    let start = |value: &str| value.split(',').next()?.parse::<u32>().ok();
    Some((start(old)?, start(new)?))
}

/// 未跟踪文件没有 git 侧的 diff，按 git 对新文件的写法补一份全新增的
fn synthetic_new_diff(path: &str, bytes: &[u8], truncated: bool) -> FileDiff {
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text.lines().collect();
    let kept = lines.len().min(4_000);

    FileDiff {
        path: path.to_string(),
        old_path: None,
        scope: "working".into(),
        hunks: if kept == 0 {
            Vec::new()
        } else {
            vec![DiffHunk {
                header: format!("@@ -0,0 +1,{kept} @@"),
                lines: (0..kept)
                    .map(|index| DiffLine {
                        kind: "added".into(),
                        old_no: None,
                        new_no: Some(index as u32 + 1),
                        text: lines[index].to_string(),
                    })
                    .collect(),
            }]
        },
        binary: bytes[..bytes.len().min(SNIFF_BYTES)].contains(&0),
        truncated: truncated || kept < lines.len(),
        additions: kept as u32,
        deletions: 0,
    }
}

fn file_diff_at(root: &Path, rel: &str, scope: &str) -> Result<FileDiff, String> {
    guard_relative(rel)?;

    let range = match scope {
        "committed" => {
            let base = resolve_base(root);
            if base.is_empty() {
                return Err("这个仓库没有可比的基线，看不了相对基线的差异。".into());
            }
            format!("{base}...HEAD")
        }
        "working" => "HEAD".to_string(),
        other => return Err(format!("不认识的差异范围：{other}")),
    };

    // 未跟踪文件不在任何 git 差异里，只能自己读内容补出来
    if scope == "working" {
        let tracked = git(root, &["ls-files", "--error-unmatch", "--", rel]).is_ok();
        if !tracked {
            let full = root.join(rel);
            let meta = fs::symlink_metadata(&full).map_err(|_| format!("读不到 {rel}"))?;
            if meta.file_type().is_symlink() || !meta.is_file() {
                return Ok(FileDiff {
                    path: rel.to_string(),
                    old_path: None,
                    scope: scope.to_string(),
                    hunks: Vec::new(),
                    binary: true,
                    truncated: false,
                    additions: 0,
                    deletions: 0,
                });
            }
            let bytes = fs::read(&full).map_err(|e| format!("读取 {rel} 失败：{e}"))?;
            let over = bytes.len() > MAX_FILE_DIFF_CHARS;
            let kept: Vec<u8> = bytes.iter().copied().take(MAX_FILE_DIFF_CHARS).collect();
            return Ok(synthetic_new_diff(rel, &kept, over));
        }
    }

    // 只把新路径交给 git 会让重命名退化成"整文件新增"——重命名检测需要两侧同框。
    // 旧路径自己算，不接受前端传：传了就能被喂一个假的
    let old_path = diff_changes(root, &range)
        .into_iter()
        .find(|change| change.path == rel)
        .and_then(|change| change.old_path);

    let mut args: Vec<&str> = vec!["diff", "--no-color", "-M", range.as_str(), "--", rel];
    if let Some(old) = &old_path {
        args.push(old);
    }

    let raw = git(root, &args)?;
    if raw.trim().is_empty() {
        return Err(format!("{rel} 在这一侧没有差异。"));
    }

    let truncated = raw.chars().count() > MAX_FILE_DIFF_CHARS;
    let clipped: String = raw.chars().take(MAX_FILE_DIFF_CHARS).collect();
    let mut diff = parse_file_diff(&clipped, scope, rel, old_path);
    diff.truncated |= truncated;
    Ok(diff)
}

#[tauri::command]
pub fn review_info(app: AppHandle) -> Result<ReviewInfo, String> {
    collect(&app)
}

/// 单个文件的差异。scope 只有两个合法值，别给前端留自由拼 range 的余地
#[tauri::command]
pub fn review_file_diff(app: AppHandle, path: String, scope: String) -> Result<FileDiff, String> {
    file_diff_at(&workspace(&app)?, &path, &scope)
}

/// 把仓库事实交给模型，换回一份变更说明草稿
#[tauri::command]
pub fn review_draft(app: AppHandle) -> Result<String, String> {
    let info = collect(&app)?;
    let config = config::load(&app);

    let messages = json!([
        {
            "role": "system",
            "content": "你在为一次代码改动写变更说明。只依据给到的信息，不要编造没看到的实现细节。用中文，格式固定为：\
                        第一行一个 60 字以内的标题；空一行；正文分四小段，依次是「这个改动做了什么」「为什么这么做」「怎么验证」「还没做的」。\
                        不要写客套话，不要用 markdown 一级标题符号。"
        },
        { "role": "user", "content": prompt_for(&info) },
    ]);

    chat::complete_once(&app, &config, messages, "review")
}

#[tauri::command]
pub fn review_save(app: AppHandle, markdown: String) -> Result<String, String> {
    let root = workspace(&app)?;
    if markdown.trim().is_empty() {
        return Err("草稿是空的。".into());
    }

    let dir = root.join(".aglab");
    fs::create_dir_all(&dir).map_err(|e| format!("创建 .aglab 目录失败：{e}"))?;
    let file = dir.join("change-request.md");
    fs::write(&file, markdown).map_err(|e| format!("写入失败：{e}"))?;

    Ok(file.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn git_ok(root: &Path, args: &[&str]) -> String {
        git(root, args).unwrap_or_else(|error| panic!("git {args:?} 失败：{error}"))
    }

    use crate::test_support::{remove_tree, temp_dir};

    fn temp_repo(label: &str) -> PathBuf {
        let root = temp_dir(&format!("review-{label}"));
        git_ok(&root, &["init", "-b", "main", "."]);
        git_ok(&root, &["config", "user.email", "aglab@example.invalid"]);
        git_ok(&root, &["config", "user.name", "aglab test"]);
        root
    }

    fn side(change: &FileChange, which: &str) -> Option<FileSide> {
        if which == "committed" {
            change.committed.clone()
        } else {
            change.working.clone()
        }
    }

    fn find<'a>(files: &'a [FileChange], path: &str) -> &'a FileChange {
        files
            .iter()
            .find(|item| item.path == path)
            .unwrap_or_else(|| panic!("清单里没有 {path}：{files:?}"))
    }

    #[test]
    fn reads_branch_commits_files_and_dirty_state() {
        let root = temp_repo("base");
        fs::write(root.join("a.txt"), "one\n").unwrap();
        git_ok(&root, &["add", "-A"]);
        git_ok(&root, &["commit", "-m", "初始化"]);
        // 伪造一个远端基线：origin/main 指向上面那个提交
        git_ok(&root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);

        git_ok(&root, &["checkout", "-b", "feature"]);
        fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
        fs::write(root.join("b.txt"), "new file\n").unwrap();
        git_ok(&root, &["add", "-A"]);
        git_ok(&root, &["commit", "-m", "加上 b 并补一行"]);

        let info = collect_at(&root).unwrap();
        assert_eq!(info.branch, "feature");
        assert_eq!(info.base, "origin/main", "应该认出伪造的远端基线");
        assert_eq!((info.ahead, info.behind), (1, 0));
        assert_eq!(info.commits.len(), 1);
        assert_eq!(info.commits[0].subject, "加上 b 并补一行");
        assert!(!info.commits[0].sha.is_empty());

        let paths: Vec<&str> = info.files.iter().map(|item| item.path.as_str()).collect();
        assert_eq!(paths, vec!["a.txt", "b.txt"]);
        let changed = find(&info.files, "a.txt");
        let committed = side(changed, "committed").expect("a.txt 有已提交侧");
        assert_eq!(
            (
                committed.state.as_str(),
                committed.additions,
                committed.deletions
            ),
            ("M", 1, 0)
        );
        assert!(changed.working.is_none(), "全提交完了就不该有未提交侧");

        let added = find(&info.files, "b.txt");
        let added_side = side(added, "committed").expect("b.txt 有已提交侧");
        assert_eq!(
            (
                added_side.state.as_str(),
                added_side.additions,
                added_side.deletions
            ),
            ("A", 1, 0)
        );

        assert_eq!(info.untracked_omitted, 0);
        assert!(info.patch.contains("+two"), "补丁里要能看到实际改动");
        assert!(!info.patch_truncated);
        assert!(info.patch_lines > 0);

        // 交给模型的提示词必须自带这些事实，否则它只能靠编
        let prompt = prompt_for(&info);
        assert!(prompt.contains("当前分支：feature"));
        assert!(prompt.contains("origin/main"));
        assert!(prompt.contains("加上 b 并补一行"));
        assert!(prompt.contains("committed M +1 -0"));
        assert!(prompt.contains("+two"));

        fs::write(root.join("c.txt"), "uncommitted\n").unwrap();
        let info = collect_at(&root).unwrap();
        let untracked = find(&info.files, "c.txt");
        let working = side(untracked, "working").expect("未跟踪文件要在未提交侧");
        assert_eq!((working.state.as_str(), working.additions), ("?", 1));

        remove_tree(&root);
    }

    #[test]
    fn keeps_both_sides_when_a_file_is_committed_and_edited_again() {
        let root = temp_repo("both");
        fs::write(root.join("a.txt"), "one\n").unwrap();
        git_ok(&root, &["add", "-A"]);
        git_ok(&root, &["commit", "-m", "初始化"]);
        git_ok(&root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);

        git_ok(&root, &["checkout", "-b", "feature"]);
        fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
        git_ok(&root, &["add", "-A"]);
        git_ok(&root, &["commit", "-m", "提交一行"]);
        fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();

        let info = collect_at(&root).unwrap();
        let a = find(&info.files, "a.txt");
        assert_eq!(side(a, "committed").unwrap().additions, 1);
        assert_eq!(side(a, "working").unwrap().additions, 1);
        assert!(
            prompt_for(&info).contains("| working M +1 -0"),
            "提示词要分得清哪一侧还没提交"
        );

        remove_tree(&root);
    }

    #[test]
    fn keeps_the_old_path_of_a_rename() {
        let root = temp_repo("rename");
        // 重命名检测按相似度算，默认阈值 50%。文件太小、改动太大时 git 会判成
        // A+D 而不是 R——那是 git 判断得对，别在代码里替它圆
        let body: String = (1..=20).map(|index| format!("line {index}\n")).collect();
        fs::write(root.join("old.txt"), &body).unwrap();
        git_ok(&root, &["add", "-A"]);
        git_ok(&root, &["commit", "-m", "初始化"]);
        git_ok(&root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);

        git_ok(&root, &["checkout", "-b", "feature"]);
        git_ok(&root, &["mv", "old.txt", "new.txt"]);
        fs::write(
            root.join("new.txt"),
            body.replace("line 7\n", "line seven\n"),
        )
        .unwrap();
        git_ok(&root, &["add", "-A"]);
        git_ok(&root, &["commit", "-m", "改名"]);

        let info = collect_at(&root).unwrap();
        let renamed = find(&info.files, "new.txt");
        assert_eq!(side(renamed, "committed").unwrap().state, "R");
        assert_eq!(renamed.old_path.as_deref(), Some("old.txt"));

        let diff = file_diff_at(&root, "new.txt", "committed").unwrap();
        assert_eq!(diff.old_path.as_deref(), Some("old.txt"));
        assert_eq!((diff.additions, diff.deletions), (1, 1));
        assert!(diff.hunks.iter().any(|h| !h.lines.is_empty()));

        remove_tree(&root);
    }

    #[test]
    fn reads_a_single_file_diff_in_either_scope() {
        let root = temp_repo("filediff");
        fs::write(root.join("a.txt"), "one\n").unwrap();
        fs::write(root.join("z.txt"), "keep\n").unwrap();
        git_ok(&root, &["add", "-A"]);
        git_ok(&root, &["commit", "-m", "初始化"]);
        git_ok(&root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);

        git_ok(&root, &["checkout", "-b", "feature"]);
        fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
        git_ok(&root, &["add", "-A"]);
        git_ok(&root, &["commit", "-m", "补一行"]);
        fs::write(root.join("z.txt"), "keep\nchanged\n").unwrap();

        let committed = file_diff_at(&root, "a.txt", "committed").unwrap();
        assert_eq!(committed.scope, "committed");
        assert_eq!((committed.additions, committed.deletions), (1, 0));
        assert_eq!(
            committed.old_path, None,
            "普通修改不是重命名，旧路径得留空，否则界面会渲染成「a.txt → a.txt」"
        );
        let added = committed.hunks[0]
            .lines
            .iter()
            .find(|line| line.kind == "added")
            .unwrap();
        assert_eq!((added.text.as_str(), added.new_no), ("two", Some(2)));
        assert_eq!(added.old_no, None);
        assert!(committed.hunks[0].header.starts_with("@@ "));

        // 别的文件的改动不能混进来
        let z = file_diff_at(&root, "z.txt", "committed");
        assert!(z.is_err(), "z.txt 在已提交侧没有差异");

        let working = file_diff_at(&root, "z.txt", "working").unwrap();
        assert_eq!((working.additions, working.deletions), (1, 0));

        fs::write(root.join("fresh.txt"), "x\ny\n").unwrap();
        let untracked = file_diff_at(&root, "fresh.txt", "working").unwrap();
        assert_eq!(untracked.additions, 2);
        assert_eq!(untracked.hunks[0].header, "@@ -0,0 +1,2 @@");
        assert_eq!(untracked.hunks[0].lines[1].new_no, Some(2));

        remove_tree(&root);
    }

    #[test]
    fn refuses_paths_outside_the_repo_and_unknown_scopes() {
        let root = temp_repo("guard");
        fs::write(root.join("a.txt"), "one\n").unwrap();
        git_ok(&root, &["add", "-A"]);
        git_ok(&root, &["commit", "-m", "初始化"]);

        assert!(file_diff_at(&root, "../outside.txt", "working").is_err());
        assert!(file_diff_at(&root, "C:\\Windows\\win.ini", "working").is_err());
        assert!(file_diff_at(&root, "", "working").is_err());
        assert!(file_diff_at(&root, "a.txt", "staged").is_err());
        // 仓库内路径可以问，但没有基线时"相对基线"这一侧要如实说不能问
        assert!(file_diff_at(&root, "a.txt", "committed").is_err());

        remove_tree(&root);
    }

    #[test]
    fn parses_hunk_headers_with_omitted_counts() {
        assert_eq!(parse_hunk_header("-12,7 +12,9 @@ fn foo"), Some((12, 12)));
        assert_eq!(parse_hunk_header("-1 +1 @@"), Some((1, 1)));
        assert_eq!(parse_hunk_header("-0,0 +1,3 @@"), Some((0, 1)));
        assert_eq!(parse_hunk_header("nonsense"), None);
    }

    #[test]
    fn falls_back_to_recent_history_without_a_base() {
        let root = temp_repo("noref");
        fs::write(root.join("a.txt"), "x\n").unwrap();
        git_ok(&root, &["add", "-A"]);
        git_ok(&root, &["commit", "-m", "只有一个提交"]);

        let info = collect_at(&root).unwrap();
        assert_eq!(info.base, "");
        assert_eq!((info.ahead, info.behind), (0, 0));
        // 没有基线时退回列最近提交，而不是整页空白
        assert_eq!(info.commits.len(), 1);
        assert_eq!(info.commits[0].subject, "只有一个提交");

        remove_tree(&root);
    }

    #[test]
    fn rejects_a_directory_that_is_not_a_repo() {
        let root = temp_dir("review-plain");
        assert!(collect_at(&root).is_err());
        remove_tree(&root);
    }
}
