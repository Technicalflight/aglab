//! 话题级 git worktree：勾选后基于所选分支为这一场话题开一棵独立工作树，
//! 文件工具、命令、权限判定全部落在这棵树上——原工作目录一个字节都不动。
//!
//! 四条与 memory 同源的规矩：
//! 1. **注册表是唯一账本。** `worktrees/registry.json`（原子写）记录谁挂在哪棵树上；
//!    重启后凭它恢复绑定，目录丢了就当没挂（`root_for` 只读，不修账）。
//! 2. **detach 不删分支。** 工作树摘掉、分支留下——那里面有没合并的工作，
//!    删掉等于替用户做决定。强制移除脏树要用户点两次。
//! 3. **树住在应用数据目录**（`app_data/worktrees/<话题id>`），不塞进仓库：
//!    塞进去会让 search_text 在原仓库里扫出两份同名文件，还得动用户的 ignore 规则。
//! 4. **派生的子助理继承同一棵树**（spawn 调 `inherit`）——不然"避免修改原目录"
//!    这条约定对子助理失效，父在树上写、子回原目录读，两边说的不是同一个项目。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::config;

/// 注册表读改写与 git 调用都过这一把锁：命令在线程池里并发跑
static LOCK: Mutex<()> = Mutex::new(());

/// Worktree 专用分支前缀。这些分支是本应用管理的账目（规矩 2：detach 不删），
/// 会随使用越积越多，但"拿一棵旧树的分支当新树基座"没有意义——
/// 基座选择器里不再列出，main 与用户自建分支才是合法基座
const WORKTREE_BRANCH_PREFIX: &str = "aglab/wt/";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Binding {
    conversation_id: String,
    /// 拥有者话题：detach 只认它。派生条目的 owner 指向父，摘树随父走
    owner_conversation_id: String,
    repo_path: String,
    dir: String,
    branch: String,
    base_branch: String,
    created_at: u64,
}

impl Default for Binding {
    fn default() -> Self {
        Self {
            conversation_id: String::new(),
            owner_conversation_id: String::new(),
            repo_path: String::new(),
            dir: String::new(),
            branch: String::new(),
            base_branch: String::new(),
            created_at: 0,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Registry {
    bindings: Vec<Binding>,
}

/// 交给界面的绑定视图（带脏树读数）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeView {
    pub conversation_id: String,
    pub dir: String,
    pub branch: String,
    pub base_branch: String,
    pub repo_path: String,
    pub dirty: bool,
    pub changed_files: u32,
}

/// 分支选择器的数据：工作目录不是 git 仓库时 is_repo 为假，前端据此隐藏控件
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitBranches {
    pub is_repo: bool,
    pub current: String,
    pub branches: Vec<String>,
    /// HEAD 指着 current 但它还没有任何提交（init 后没 commit 过）：选择器上
    /// 显示的名字只是意向，refs/heads 下没有这个引用，开 Worktree 没有基点
    pub unborn: bool,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// 话题 id 要落进目录名与分支名，字符集再挡一次（history/json_store 同款判据）
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
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

    Ok(String::from_utf8_lossy(&output.stdout).trim_end().to_string())
}

// ---- 注册表（显式收路径，测试不碰全局） ----

fn load_registry(file: &Path) -> Registry {
    fs::read_to_string(file)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// 先写临时文件再 rename：进程中途被杀不会留下半个损坏的账本
fn save_registry(file: &Path, registry: &Registry) -> Result<(), String> {
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建 worktrees 目录失败：{e}"))?;
    }
    let text = serde_json::to_string_pretty(registry).map_err(|e| e.to_string())?;
    let temp = file.with_extension("json.tmp");
    fs::write(&temp, text).map_err(|e| e.to_string())?;
    fs::rename(&temp, file).map_err(|e| e.to_string())
}

fn view_of(binding: &Binding, dirty: bool, changed_files: u32) -> WorktreeView {
    WorktreeView {
        conversation_id: binding.conversation_id.clone(),
        dir: binding.dir.clone(),
        branch: binding.branch.clone(),
        base_branch: binding.base_branch.clone(),
        repo_path: binding.repo_path.clone(),
        dirty,
        changed_files,
    }
}

/// 脏树读数：porcelain 行数。目录没了不算错，按"干净"报——摘树那一步会说实话
fn dirty_of(dir: &str) -> (bool, u32) {
    match git(Path::new(dir), &["status", "--porcelain"]) {
        Ok(text) => {
            let lines = text.lines().filter(|l| !l.trim().is_empty()).count() as u32;
            (lines > 0, lines)
        }
        Err(_) => (false, 0),
    }
}

/// 挂上时顺手清账：话题已经不存在的绑定是泄漏（删除话题不自动摘树是刻意的，
/// 但账本里挂着死话题的条目没有任何人能再摘它）
fn prune_dead(registry: &mut Registry, live: &dyn Fn(&str) -> bool) {
    registry
        .bindings
        .retain(|binding| live(&binding.conversation_id) || live(&binding.owner_conversation_id));
}

// ---- 核心操作（显式收 repo / 目录 / 注册表路径，命令与测试共用） ----

fn attach_core(
    repo: &Path,
    wt_root: &Path,
    registry_file: &Path,
    conversation_id: &str,
    base_branch: &str,
) -> Result<WorktreeView, String> {
    if !valid_id(conversation_id) {
        return Err("非法的话题 id。".into());
    }
    if !repo.join(".git").exists() {
        return Err("当前工作目录不是 git 仓库：Worktree 要基于仓库分支才能开。".into());
    }

    let mut registry = load_registry(registry_file);
    if let Some(existing) = registry
        .bindings
        .iter()
        .find(|b| b.conversation_id == conversation_id)
    {
        let (dirty, changed) = dirty_of(&existing.dir);
        return Ok(view_of(existing, dirty, changed));
    }

    let base = if base_branch.trim().is_empty() {
        let current = git(repo, &["branch", "--show-current"])?;
        if current.trim().is_empty() {
            return Err("当前处于分离 HEAD：先在分支选择器里选一个分支再开 Worktree。".into());
        }
        current.trim().to_string()
    } else {
        base_branch.trim().to_string()
    };
    // 基分支必须真实存在，否则 git 报错又长又英文。
    // 两种死法分开说：init 后没提交过（HEAD 还没诞生，`branch --show-current`
    // 也会报出名字，选择器上看着"明明有 main"）与分支真的不存在——前者给一句
    // 能照着做的，后者才让用户换分支
    if git(repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{base}")]).is_err() {
        let unborn = git(repo, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_err();
        return Err(if unborn {
            format!("这个仓库还没有任何提交，分支「{base}」尚未诞生——先提交一次，再来开 Worktree。")
        } else {
            format!("分支「{base}」不存在，先在分支选择器里换一个。")
        });
    }

    // 分支名：同话题反复勾选/摘除复用同一个分支——摘树不删分支，重新挂上
    // 就是接回上次的工作，改动一条不丢
    let tail: String = {
        let chars: Vec<char> = conversation_id.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        let start = chars.len().saturating_sub(8);
        chars[start..].iter().collect()
    };
    let branch = format!("{WORKTREE_BRANCH_PREFIX}{tail}");
    let branch_exists = git(
        repo,
        ["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")].as_slice(),
    )
    .is_ok();

    let dir = wt_root.join(conversation_id);
    let dir_str = dir.display().to_string();
    if dir.exists() {
        // 目录还在但没有绑定（上次进程崩在两步之间）：git 元数据还认它就收编，
        // 不认（半删除状态）就强制摘掉重挂——目录里只有机器开的工作树，没有用户资产
        let adopted = git(&dir, &["rev-parse", "--is-inside-work-tree"]).is_ok()
            && git(repo, ["worktree", "list", "--porcelain"].as_slice())
                .map(|text| text.contains(&format!("worktree {dir_str}")))
                .unwrap_or(false);
        if !adopted {
            let _ = fs::remove_dir_all(&dir);
            let _ = git(repo, &["worktree", "prune"]);
        }
    }

    if branch_exists {
        git(repo, &["worktree", "add", &dir_str, &branch])?;
    } else {
        git(repo, &["worktree", "add", "-b", &branch, &dir_str, &base])?;
    }

    let binding = Binding {
        conversation_id: conversation_id.to_string(),
        owner_conversation_id: conversation_id.to_string(),
        repo_path: repo.display().to_string(),
        dir: dir_str,
        branch,
        base_branch: base,
        created_at: now_ms(),
    };
    let (dirty, changed) = dirty_of(&binding.dir);
    let view = view_of(&binding, dirty, changed);
    registry.bindings.push(binding);
    save_registry(registry_file, &registry)?;
    Ok(view)
}

fn detach_core(
    registry_file: &Path,
    conversation_id: &str,
    force: bool,
) -> Result<(), String> {
    let mut registry = load_registry(registry_file);
    let binding = registry
        .bindings
        .iter()
        .find(|b| b.conversation_id == conversation_id)
        .cloned()
        .ok_or_else(|| "这个话题没有挂着的 Worktree。".to_string())?;

    let is_owner = binding.owner_conversation_id == binding.conversation_id;
    if is_owner {
        let repo = PathBuf::from(&binding.repo_path);
        let mut args = vec!["worktree".to_string(), "remove".to_string()];
        if force {
            args.push("--force".to_string());
        }
        args.push(binding.dir.clone());
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        git(&repo, &arg_refs).map_err(|error| {
            if force {
                format!("移除工作树失败：{error}")
            } else {
                format!(
                    "工作树里有未提交的改动，git 拒绝移除。确认不要了就再点一次（强制移除）；想保住这些改动，先在「{}」分支上提交。原始报错：{error}",
                    binding.branch
                )
            }
        })?;
        let dir = binding.dir.clone();
        registry.bindings.retain(|b| b.dir != dir);
    } else {
        // 子助理条目：只摘账不摘树——树的所有权在父话题手里
        registry
            .bindings
            .retain(|b| b.conversation_id != conversation_id);
    }
    save_registry(registry_file, &registry)
}

fn lookup_core(registry_file: &Path, conversation_id: &str) -> Option<WorktreeView> {
    let registry = load_registry(registry_file);
    let binding = registry
        .bindings
        .iter()
        .find(|b| b.conversation_id == conversation_id)?;
    if !Path::new(&binding.dir).exists() {
        // 目录被人手删了：绑定作废（账不在这条只读路径上修，摘树时会自然清掉）
        return None;
    }
    let (dirty, changed) = dirty_of(&binding.dir);
    Some(view_of(binding, dirty, changed))
}

fn branches_core(repo: &Path) -> Result<GitBranches, String> {
    if !repo.join(".git").exists() {
        return Ok(GitBranches { is_repo: false, current: String::new(), branches: Vec::new(), unborn: false });
    }
    let current = git(repo, &["branch", "--show-current"]).unwrap_or_default();
    let list = git(repo, &["branch", "--format=%(refname:short)"]).unwrap_or_default();
    // unborn 分支：--show-current 有名字、分支列表却是空的——init 之后没提交过，
    // refs/heads 下还没有这个引用。单列出来让前端能说"先提交"，不假装分支存在
    let trimmed = current.trim();
    let unborn =
        !trimmed.is_empty() && git(repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{trimmed}")]).is_err();
    let branches: Vec<String> = list
        .lines()
        .map(str::trim)
        // 应用自管的 worktree 分支不当基座选项：账目式存在（每挂一棵树留一条），
        // 列出来只会把选择器淹掉，用户从分支名上也认不出它们是什么
        .filter(|l| !l.is_empty() && !l.starts_with(WORKTREE_BRANCH_PREFIX))
        .map(String::from)
        .collect();
    Ok(GitBranches {
        is_repo: true,
        current: trimmed.to_string(),
        branches,
        unborn,
    })
}

// ---- 全局路径与查表（turn_body 与命令用） ----

fn paths(app: &AppHandle) -> Result<(PathBuf, PathBuf), String> {
    paths_in(&app.path().app_data_dir().map_err(|e| e.to_string())?)
}

fn paths_in(data_dir: &std::path::Path) -> Result<(PathBuf, PathBuf), String> {
    let root = data_dir.join("worktrees");
    let file = root.join("registry.json");
    Ok((root, file))
}

/// 这一场话题的根目录：挂了树就是树的路径，没挂返回 None（调用方回落 active project）。
/// 只读账本、不碰 git——每轮开头都要走这一趟
pub fn root_for(app: &AppHandle, conversation_id: &str) -> Option<PathBuf> {
    root_for_in(&app.path().app_data_dir().map_err(|e| e.to_string()).ok()?, conversation_id)
}

/// worker 进程的变体（M3 第 2 档）
pub fn root_for_in(data_dir: &std::path::Path, conversation_id: &str) -> Option<PathBuf> {
    let (_root, file) = paths_in(data_dir).ok()?;
    let registry = load_registry(&file);
    let binding = registry
        .bindings
        .iter()
        .find(|b| b.conversation_id == conversation_id)?;
    if !Path::new(&binding.dir).exists() {
        return None;
    }
    Some(PathBuf::from(&binding.dir))
}

/// spawn 的子话题继承父话题的树。尽力而为：父没挂树就是无事发生
pub fn inherit(app: &AppHandle, parent_conversation_id: &str, child_conversation_id: &str) {
    let Ok((_root, file)) = paths(app) else { return };
    let Ok(_guard) = LOCK.lock() else { return };
    let mut registry = load_registry(&file);
    let Some(parent) = registry
        .bindings
        .iter()
        .find(|b| b.conversation_id == parent_conversation_id)
        .cloned()
    else {
        return;
    };
    if registry
        .bindings
        .iter()
        .any(|b| b.conversation_id == child_conversation_id)
    {
        return;
    }
    registry.bindings.push(Binding {
        conversation_id: child_conversation_id.to_string(),
        owner_conversation_id: parent.owner_conversation_id,
        repo_path: parent.repo_path,
        dir: parent.dir,
        branch: parent.branch,
        base_branch: parent.base_branch,
        created_at: now_ms(),
    });
    let _ = save_registry(&file, &registry);
}

/// 带脏树读数的完整视图（界面状态条与 turn_body 的上下文卡用）
pub fn view_for(app: &AppHandle, conversation_id: &str) -> Option<WorktreeView> {
    view_for_in(&app.path().app_data_dir().map_err(|e| e.to_string()).ok()?, conversation_id)
}

/// worker 进程的变体（M3 第 2 档）
pub fn view_for_in(data_dir: &std::path::Path, conversation_id: &str) -> Option<WorktreeView> {
    let (_root, file) = paths_in(data_dir).ok()?;
    lookup_core(&file, conversation_id)
}

// ---- 命令 ----

#[tauri::command]
pub fn worktree_attach(
    app: AppHandle,
    conversation_id: String,
    base_branch: Option<String>,
) -> Result<WorktreeView, String> {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let (wt_root, registry_file) = paths(&app)?;
    let config = config::load(&app);
    // 挂哪棵仓库跟话题走：话题自己绑定的项目优先，散对话才回落激活项目——
    // 与 turn_body 解析工具根同一条链。反例（旧账）：话题绑着 A、激活的是 B，
    // 挂树挂进 B 的仓库，工具根因 worktree 最优先也跟着进 B——归属被激活项目劫走
    let project = crate::chat::conversation_project(&app, &config, &conversation_id)
        .or_else(|| config.active_project())
        .ok_or_else(|| "先绑定一个工作目录，Worktree 才有仓库可挂。".to_string())?;
    let repo = PathBuf::from(&project.path);

    // 挂新树前清一次死话题的旧账：删除话题不自动摘树是刻意的（树里可能有没合并的活），
    // 但没人再话题的条目留在账本里只会越积越多
    let mut registry = load_registry(&registry_file);
    let live: Vec<String> = crate::history::list_current(&app)
        .unwrap_or_default()
        .into_iter()
        .map(|meta| meta.id)
        .collect();
    let before = registry.bindings.len();
    prune_dead(&mut registry, &|id: &str| {
        id == conversation_id || live.iter().any(|live_id| live_id == id)
    });
    if registry.bindings.len() != before {
        save_registry(&registry_file, &registry)?;
    }

    attach_core(
        &repo,
        &wt_root,
        &registry_file,
        &conversation_id,
        base_branch.as_deref().unwrap_or(""),
    )
}

#[tauri::command]
pub fn worktree_detach(app: AppHandle, conversation_id: String, force: Option<bool>) -> Result<(), String> {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let (_root, registry_file) = paths(&app)?;
    detach_core(&registry_file, &conversation_id, force.unwrap_or(false))
}

#[tauri::command]
pub fn worktree_status(app: AppHandle, conversation_id: String) -> Result<Option<WorktreeView>, String> {
    let (_root, registry_file) = paths(&app)?;
    Ok(lookup_core(&registry_file, &conversation_id))
}

#[tauri::command]
pub fn worktree_branches(app: AppHandle, conversation_id: String) -> Result<GitBranches, String> {
    let config = config::load(&app);
    // 分支清单必须跟挂树的目标仓库是同一个：话题绑定项目优先，散对话回落激活项目
    //（与 worktree_attach、turn_body 同链）。清单里不含应用自管的 worktree 分支
    match crate::chat::conversation_project(&app, &config, &conversation_id)
        .or_else(|| config.active_project())
    {
        Some(project) => branches_core(Path::new(&project.path)),
        None => Ok(GitBranches { is_repo: false, current: String::new(), branches: Vec::new(), unborn: false }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::scoped_temp_dir;
    use std::process::Command as SysCommand;

    /// 一个带一次提交的真 git 仓库。git 在测试里真实可用（review.rs 的测试同款前提）
    fn repo_with_commit(tag: &str) -> (crate::test_support::ScopedTempDir, PathBuf) {
        let scoped = scoped_temp_dir(tag);
        let repo = scoped.path.join("repo");
        fs::create_dir_all(&repo).unwrap();
        let run = |args: &[&str]| {
            let out = SysCommand::new("git").args(args).current_dir(&repo).output().unwrap();
            assert!(
                out.status.success(),
                "git {:?} 失败：{}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["-c", "user.email=t@t", "-c", "user.name=t", "add", "-A"]);
        fs::write(repo.join("a.txt"), "hello").unwrap();
        run(&["add", "-A"]);
        run(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "init"]);
        (scoped, repo)
    }

    fn scene(tag: &str) -> (crate::test_support::ScopedTempDir, PathBuf, PathBuf, PathBuf) {
        let scoped = scoped_temp_dir(tag);
        let base = scoped.path.clone();
        let wt_root = scoped.path.join("trees");
        let registry = scoped.path.join("registry.json");
        (scoped, wt_root, registry, base)
    }

    #[test]
    fn attach_creates_tree_on_a_new_branch_and_detaches_cleanly() {
        let (_repo_scope, repo) = repo_with_commit("wt-repo-1");
        let (_scope, wt_root, registry_file, _) = scene("wt-scene-1");

        let view = attach_core(&repo, &wt_root, &registry_file, "conv_alpha1", "").unwrap();
        assert_eq!(view.base_branch, "main", "空基分支默认走当前分支（init 默认 main）");
        assert!(view.branch.starts_with("aglab/wt/"));
        assert!(Path::new(&view.dir).join(".git").exists(), "工作树里该有 git 指针");
        assert!(!view.dirty);

        // 注册表里查得到，root_for 语义（借同一路径函数验证）
        let registry = load_registry(&registry_file);
        assert_eq!(registry.bindings.len(), 1);

        detach_core(&registry_file, "conv_alpha1", false).unwrap();
        assert!(!Path::new(&view.dir).exists(), "干净树摘除后目录该消失");
        // 分支保留：detach 不删分支是明文约定
        assert!(git(&repo, &["rev-parse", "--verify", &format!("refs/heads/{}", view.branch)]).is_ok());
    }

    #[test]
    fn reattach_reuses_the_branch_and_keeps_its_work() {
        let (_repo_scope, repo) = repo_with_commit("wt-repo-2");
        let (_scope, wt_root, registry_file, _) = scene("wt-scene-2");

        let first = attach_core(&repo, &wt_root, &registry_file, "conv_beta1", "").unwrap();
        fs::write(Path::new(&first.dir).join("work.txt"), "树上的活").unwrap();
        git(Path::new(&first.dir), &["add", "-A"]).unwrap();
        git(
            Path::new(&first.dir),
            &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "wip"],
        )
        .unwrap();
        detach_core(&registry_file, "conv_beta1", false).unwrap();

        let second = attach_core(&repo, &wt_root, &registry_file, "conv_beta1", "").unwrap();
        assert_eq!(second.branch, first.branch, "同话题重新挂树复用同一分支");
        assert_eq!(
            fs::read_to_string(Path::new(&second.dir).join("work.txt")).unwrap(),
            "树上的活",
            "上次的工作一条不丢"
        );
    }

    #[test]
    fn dirty_tree_refuses_detach_until_forced() {
        let (_repo_scope, repo) = repo_with_commit("wt-repo-3");
        let (_scope, wt_root, registry_file, _) = scene("wt-scene-3");

        let view = attach_core(&repo, &wt_root, &registry_file, "conv_gamma1", "").unwrap();
        fs::write(Path::new(&view.dir).join("a.txt"), "改过").unwrap();

        let error = detach_core(&registry_file, "conv_gamma1", false).unwrap_err();
        assert!(error.contains("未提交"), "{error}");
        assert!(Path::new(&view.dir).exists(), "拒绝移除时目录原地不动");

        detach_core(&registry_file, "conv_gamma1", true).unwrap();
        assert!(!Path::new(&view.dir).exists());
    }

    #[test]
    fn non_git_dir_and_bad_ids_are_refused() {
        let empty = scoped_temp_dir("wt-not-repo");
        let (_scope, wt_root, registry_file, _) = scene("wt-scene-4");
        assert!(attach_core(empty.path.as_path(), &wt_root, &registry_file, "conv_x1", "").is_err());
        let (_repo_scope, repo) = repo_with_commit("wt-repo-4");
        assert!(attach_core(&repo, &wt_root, &registry_file, "../escape", "").is_err());
        assert!(attach_core(&repo, &wt_root, &registry_file, "", "").is_err());
    }

    #[test]
    fn missing_base_branch_is_reported_in_chinese() {
        let (_repo_scope, repo) = repo_with_commit("wt-repo-5");
        let (_scope, wt_root, registry_file, _) = scene("wt-scene-5");
        let error = attach_core(&repo, &wt_root, &registry_file, "conv_delta1", "no-such-branch").unwrap_err();
        assert!(error.contains("不存在"), "{error}");
    }

    #[test]
    fn unborn_repo_asks_for_the_first_commit() {
        // init 后没提交过：--show-current 照样报 main（选择器上"明明有 main"），
        // 但 refs/heads/main 不存在——报错要教用户先提交，而不是让他去换分支
        let repo_scope = scoped_temp_dir("wt-unborn");
        let repo = repo_scope.path.join("repo");
        fs::create_dir_all(&repo).unwrap();
        let run = |args: &[&str]| {
            let out = SysCommand::new("git").args(args).current_dir(&repo).output().unwrap();
            assert!(out.status.success(), "git {:?} 失败：{}", args, String::from_utf8_lossy(&out.stderr));
        };
        run(&["init", "-q", "-b", "main"]);

        let (_scope, wt_root, registry_file, _) = scene("wt-unborn-scene");
        let error = attach_core(&repo, &wt_root, &registry_file, "conv_eps1", "").unwrap_err();
        assert!(error.contains("还没有任何提交"), "{error}");

        // 选择器读数：current 有名字、unborn 为真——前端据此显示"先提交"而不是假分支
        let view = branches_core(&repo).unwrap();
        assert_eq!(view.current, "main");
        assert!(view.unborn);
    }

    #[test]
    fn the_base_branch_picker_hides_managed_worktree_branches() {
        let (_repo_scope, repo) = repo_with_commit("wt-branch-filter");
        let run = |args: &[&str]| {
            let out = SysCommand::new("git").args(args).current_dir(&repo).output().unwrap();
            assert!(
                out.status.success(),
                "git {:?} 失败：{}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };
        // 模拟历史挂树留下的账目分支 + 一个用户自建分支
        run(&["branch", "aglab/wt/deadbeef"]);
        run(&["branch", "aglab/wt/cafebabe"]);
        run(&["branch", "feature/real"]);

        let view = branches_core(&repo).unwrap();
        assert!(view.branches.iter().any(|name| name == "main"));
        assert!(view.branches.iter().any(|name| name == "feature/real"));
        assert!(
            view.branches.iter().all(|name| !name.starts_with("aglab/wt/")),
            "worktree 账目分支不该出现在基座选择器：{:?}",
            view.branches
        );
    }
}
