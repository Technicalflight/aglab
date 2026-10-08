//! 本地自记忆系统。
//!
//! 两条不可动摇的规矩：
//! 1. **Markdown 是真相源。** `index.sqlite` 随时可以整份删掉，从 `.md` 重建回来。
//!    任何"只有索引里有"的状态都是 bug。
//! 2. **默认只写本地。** 这里没有任何网络出口。提取用的 LLM 调用走应用已经配好的
//!    推理服务商（跟聊天走同一条路），不额外上传任何记忆文件。

mod decay;
mod extract;
mod govern;
mod graph;
mod index;
mod inject;
mod origin;
pub mod record;
mod reflect;

pub use extract::Accepted;
pub use govern::DistillSummary;
pub use index::{Hit, SearchOptions, TimelineRow};
pub use inject::Injection;
pub use origin::{Origin, SourceView};

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

pub use record::{
    leaks_sensitive, marked_do_not_store, new_id, now_rfc3339, parse_records, render_records,
    MemoryKind, MemoryRecord, MemoryScope, MemorySource, MemoryStatus, Stability,
};

use crate::config;

const RECORD_FILE: &str = "MEMORY.md";
const DAILY_DIR: &str = "daily";
const ARCHIVE_DIR: &str = "archive";
const PROJECT_LOCAL_DIR: &str = ".ai-memory";

/// 检索打分权重：语义 / 重要性 / 新鲜度 / 作用域 / 使用频率
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Weights {
    pub semantic: f64,
    pub importance: f64,
    pub freshness: f64,
    pub scope: f64,
    pub usage: f64,
}

impl Default for Weights {
    fn default() -> Self {
        // 使用频率让 5 个点给语义：注入就计数会形成正反馈（常被用上的更容易再被用上），
        // 0.10 的权重足以把同一批记忆固化在头部；语义多拿的这 5 个点落在
        // 批次内相对分（见 index::search）上，那才是"相关"的主证据
        Self { semantic: 0.50, importance: 0.20, freshness: 0.15, scope: 0.10, usage: 0.05 }
    }
}

/// 记忆系统自己的开关，写在 `memory/config.json`。
/// 它故意不进主 config.json：这一整套要能整目录拷走、整目录删掉
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MemoryConfig {
    /// 总开关。关掉之后不检索、不注入、不提取，命令也拒绝写
    pub enabled: bool,
    /// 每轮按相关性注入
    pub auto_inject: bool,
    /// 每轮结束后异步提取候选
    pub auto_extract: bool,
    /// profile / soul / rules 这类"始终注入区"的 token 预算
    pub always_budget_tokens: u32,
    /// 检索注入的 token 预算
    pub retrieve_budget_tokens: u32,
    /// 检索返回条数上限
    pub search_limit: usize,
    /// 全局 MEMORY.md 的字符上限，超了提示蒸馏
    pub global_limit_chars: usize,
    /// 项目 MEMORY.md 的字符上限
    pub project_limit_chars: usize,
    /// 每日日志保留原文的天数
    pub daily_keep_days: u32,
    /// 超过这个天数就送去蒸馏
    pub distill_after_days: u32,
    /// 新鲜度半衰期（天）：多少天没被用上，新鲜度对折一次。
    /// 设计里那格 `decay.half_life_days` 的落地——它此前是常量，于是"我的记忆该偏新还是偏旧"
    /// 这个问题没人能答
    pub decay_half_life_days: f64,
    /// 候选区自然衰减：模型提的、用户一直没点头的候选，过了这么多天且置信度与
    /// 重要性都还在自动转正线之下，就归档（正文一字不动，可逆）。0 = 关掉这条
    pub candidate_ttl_days: u32,
    /// 空闲自动蒸馏：开着时前端每半小时检查一次 needsDistill，命中且空闲就自动跑，
    /// 至多一天一次。默认关——蒸馏要花一次服务商请求，这件事得用户点头才常态化
    pub auto_distill: bool,
    /// 自动写入的门槛：置信度与重要性都要过线，否则进 candidate
    pub auto_accept_confidence: f64,
    pub auto_accept_importance: u32,
    /// 去重阈值：与现有记忆相似度超过它就更新旧条目
    pub dedupe_similarity: f64,
    pub weights: Weights,
    /// 主动回忆：**只在面板提示**"有这几条相关但本轮没用上"，默认关。
    /// 它不是第二条注入通道——开启前后发给服务商的内容逐字节相同，T08 的变异测试钉的就是这条
    pub proactive_recall: bool,
    /// 反思循环（P2）：定期问一次"最近学到了什么"。默认关，且**关掉就一次服务商都不发**。
    /// 开着时产物也只能进候选区——它说的是推断，不是用户说过的话
    pub reflect_enabled: bool,
    /// 云同步。实现里没有任何上传路径，这个开关存在的意义是"它必须是关的且被写下来"
    pub cloud_sync: bool,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_inject: true,
            auto_extract: true,
            always_budget_tokens: 800,
            retrieve_budget_tokens: 600,
            search_limit: 8,
            global_limit_chars: 3000,
            project_limit_chars: 3000,
            daily_keep_days: 7,
            distill_after_days: 30,
            decay_half_life_days: crate::memory::decay::HALF_LIFE_DAYS,
            candidate_ttl_days: 30,
            auto_distill: false,
            auto_accept_confidence: 0.8,
            auto_accept_importance: 3,
            dedupe_similarity: 0.92,
            weights: Weights::default(),
            proactive_recall: false,
            reflect_enabled: false,
            cloud_sync: false,
        }
    }
}

/// 记忆目录布局。全部从传入的 root 派生，没有一处硬编码用户路径
#[derive(Debug, Clone)]
pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn app(app: &AppHandle) -> Result<Self, String> {
        let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
        Ok(Self::new(dir.join("memory")))
    }

    pub fn profile(&self) -> PathBuf {
        self.root.join("profile.md")
    }

    pub fn soul(&self) -> PathBuf {
        self.root.join("soul.md")
    }

    pub fn rules(&self) -> PathBuf {
        self.root.join("rules.md")
    }

    pub fn global_memory(&self) -> PathBuf {
        self.root.join("global").join(RECORD_FILE)
    }

    pub fn project_dir(&self, project_id: &str) -> PathBuf {
        self.root.join("projects").join(project_id)
    }

    pub fn project_memory(&self, project_id: &str) -> PathBuf {
        self.project_dir(project_id).join(RECORD_FILE)
    }

    pub fn project_daily(&self, project_id: &str) -> PathBuf {
        self.project_dir(project_id).join(DAILY_DIR)
    }

    pub fn project_archive(&self, project_id: &str) -> PathBuf {
        self.project_dir(project_id).join(ARCHIVE_DIR)
    }

    /// 全局记忆的每日流水账。它跟 global/MEMORY.md 平级，蒸馏完搬去 global/archive/
    pub fn global_daily(&self) -> PathBuf {
        self.root.join("global").join(DAILY_DIR)
    }

    pub fn global_archive(&self) -> PathBuf {
        self.root.join("global").join(ARCHIVE_DIR)
    }

    pub fn index_db(&self) -> PathBuf {
        self.root.join("index.sqlite")
    }

    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.json")
    }

    /// 项目仓库里的那份。跟全局是同一套格式，只是住在项目目录里、跟着仓库走
    pub fn workspace_memory(workspace: &Path) -> PathBuf {
        workspace.join(PROJECT_LOCAL_DIR).join(RECORD_FILE)
    }

    pub fn workspace_daily(workspace: &Path) -> PathBuf {
        workspace.join(PROJECT_LOCAL_DIR).join(DAILY_DIR)
    }

    pub fn workspace_archive(workspace: &Path) -> PathBuf {
        workspace.join(PROJECT_LOCAL_DIR).join(ARCHIVE_DIR)
    }
}

pub fn load_config(paths: &Paths) -> MemoryConfig {
    fs::read_to_string(paths.config_file())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn save_config(paths: &Paths, value: &MemoryConfig) -> Result<(), String> {
    fs::create_dir_all(&paths.root).map_err(|e| format!("创建记忆目录失败：{e}"))?;
    let text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    fs::write(paths.config_file(), text).map_err(|e| format!("写入记忆配置失败：{e}"))
}

/// Windows 上没有 POSIX 位，这条只在 unix 生效——但"尽量设为 700"是明确要求，
/// 所以该调就调，失败也不当成错误
fn harden_dir(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = path;
}

pub fn ensure_layout(paths: &Paths) -> Result<(), String> {
    for dir in [
        paths.root.clone(),
        paths.root.join("global"),
        paths.root.join("projects"),
    ] {
        fs::create_dir_all(&dir).map_err(|e| format!("创建 {} 失败：{e}", dir.display()))?;
        harden_dir(&dir);
    }
    for file in [paths.profile(), paths.soul(), paths.rules()] {
        if !file.exists() {
            let header = match file.file_name().and_then(|name| name.to_str()) {
                Some("profile.md") => "# 用户画像\n\n<!-- 始终注入。写你自己的身份与长期偏好，一行的事实也放这里 -->\n",
                Some("soul.md") => "# 助手人格\n\n<!-- 始终注入。写你希望助手一直保持的行为准则 -->\n",
                _ => "# 硬规则\n\n<!-- 始终注入。这里的每一条都会被原样交给模型，优先级高于推断出来的记忆 -->\n",
            };
            fs::write(&file, header).map_err(|e| format!("创建 {} 失败：{e}", file.display()))?;
        }
    }
    if !paths.global_memory().exists() {
        if let Some(parent) = paths.global_memory().parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        fs::write(&paths.global_memory(), "")
            .map_err(|e| format!("创建全局记忆文件失败：{e}"))?;
    }
    Ok(())
}

fn read_text(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

/// 真相源的写互斥。所有「读全文 → 内存改 → 整文件写回」的入口都要拿住它，
/// 否则两路写入交错就是后写覆盖先写——用户手记的那条就是这么丢的。
/// 这把锁只管互斥、不管清算：一次 panic 毒化它之后，后续写入照样要能走。
/// 规矩：拿锁的八处入口（append_record_as / flush_group / merge_into /
/// edit_record / memory_forget / set_status / stamp_records / wipe）内部
/// **不得**再调用另一个拿锁的入口——这把锁不可重入，嵌套就是死锁。
/// 新写入口时先看一眼这张清单
static WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn write_guard() -> std::sync::MutexGuard<'static, ()> {
    WRITE_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 原子写：同目录临时文件 → 刷盘 → 改名顶替。直接 `fs::write` 目标文件，
/// 断电/崩溃会把真相源撕成半截——而读不懂的文件会冻结整个作用域的读写。
/// 临时文件名带进程号：两个实例同时写也不互踩；残留的 `.tmp.*` 不是 `.md`，
/// 不会被当记录扫进任何列表，下次成功写入会原样覆盖
fn atomic_write(file: &Path, body: &str) -> Result<(), String> {
    use std::io::Write;
    let tmp = file.with_extension(format!("tmp.{}", std::process::id()));
    let spill = || -> Result<(), String> {
        let mut handle = std::fs::File::create(&tmp)
            .map_err(|e| format!("创建 {} 失败：{e}", tmp.display()))?;
        handle
            .write_all(body.as_bytes())
            .and_then(|_| handle.sync_all())
            .map_err(|e| format!("写 {} 失败：{e}", tmp.display()))
    };
    if let Err(error) = spill() {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    // Windows 上 rename 会替换已存在的目标；目标被别的程序锁住时才失败，
    // 那时把临时文件清掉，错误如实往外抛
    if let Err(error) = fs::rename(&tmp, file) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("写入 {} 失败：{error}", file.display()));
    }
    Ok(())
}

/// 把一份读不了的记录文件搬进 `archive/corrupt-<时间>/`。搬走而不是删掉：
/// 字节都还在，人工找得回来；跨盘符（仓库里那份 `.ai-memory` 在另一块盘）时
/// rename 会失败，那就复制再删——隔离这件事本身不能再被"搬不动"卡住
fn quarantine_file(paths: &Paths, file: &Path, reason: &str) -> Result<(), String> {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or(0);
    let stamp = now_rfc3339().replace(':', "-").replace('+', "-");
    let dir = paths
        .root
        .join(ARCHIVE_DIR)
        .join(format!("corrupt-{stamp}-{millis}"));
    fs::create_dir_all(&dir).map_err(|e| format!("创建 {} 失败：{e}", dir.display()))?;
    harden_dir(&dir);
    let aside = dir.join(display_path(&paths.root, file).replace('/', "__"));
    if fs::rename(file, &aside).is_err() {
        fs::copy(file, &aside).map_err(|e| format!("复制 {} 失败：{e}", file.display()))?;
        fs::remove_file(file).map_err(|e| format!("删除 {} 失败：{e}", file.display()))?;
    }
    eprintln!("记忆文件读不了，已隔离到 {}：{reason}", aside.display());
    let _ = audit(paths, "quarantine", &display_path(&paths.root, file));
    Ok(())
}

/// 读一份记录文件；读不懂就把坏文件隔离掉，当空文件继续。
/// 旧的语义是报错拒绝——而 sync_all 也吃解析错误，一条坏文件冻结的是
/// 整套记忆功能的**全部读写**，用户看到的只有一句"先修它"。隔离是自愈：
/// 坏文件退出真相源，索引在下一次同步里跟上，写入不再被历史遗留卡死
fn load_records_or_quarantine(paths: &Paths, file: &Path) -> Result<Vec<MemoryRecord>, String> {
    let text = read_text(file);
    match parse_records(&text) {
        Ok(records) => Ok(records),
        Err(error) => {
            quarantine_file(paths, file, &error)?;
            Ok(Vec::new())
        }
    }
}

/// 始终注入区：画像、人格、硬规则。它们是散文不是记录，所以不进索引——
/// 用户改这些文件应当改完就生效，不需要重建任何东西
pub fn standing_text(paths: &Paths) -> String {
    let mut blocks = Vec::new();
    for (label, file) in [
        ("用户画像", paths.profile()),
        ("助手人格", paths.soul()),
        ("硬规则", paths.rules()),
    ] {
        let body = strip_html_comments(&read_text(&file));
        let body = body.trim();
        // 只剩一行标题的空模板不算内容：那句 "# 硬规则" 是建目录时我们自己写的，
        // 每轮都注入等于用零信息换掉一段预算
        let has_substance = body
            .lines()
            .any(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'));
        if has_substance {
            blocks.push(format!("【{label}】\n{body}"));
        }
    }
    blocks.join("\n\n")
}

/// 去掉 `<!-- ... -->` 的说明注释。文件里的注释是写给用户看的，不是给模型的
fn strip_html_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + 3..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// 一个项目要索引哪些文件：全局的、aglab 目录里该项目的、以及项目仓库里那份
pub fn record_files(paths: &Paths, workspace: Option<&Path>, project_id: Option<&str>) -> Vec<PathBuf> {
    let mut files = vec![paths.global_memory()];
    if let Some(id) = project_id {
        files.push(paths.project_memory(id));
        files.extend(daily_files(&paths.project_daily(id)));
    }
    if let Some(workspace) = workspace {
        files.push(Paths::workspace_memory(workspace));
        files.extend(daily_files(&Paths::workspace_daily(workspace)));
    }
    files.retain(|path| path.exists());
    files
}

fn daily_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("md") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

fn display_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// 把磁盘上的记录同步进索引。用户手改、手删文件后重建走的也是这一条：
/// 索引里存在但磁盘上已经没有的 path，整片行一起删掉
pub fn sync_all(
    conn: &rusqlite::Connection,
    paths: &Paths,
    workspace: Option<&Path>,
    project_id: Option<&str>,
) -> Result<usize, String> {
    let mut written = 0usize;
    let mut live: Vec<String> = Vec::new();

    for file in record_files(paths, workspace, project_id) {
        let shown = display_path(&paths.root, &file);
        let records = load_records_or_quarantine(paths, &file)?;
        written += index::sync_file(conn, &shown, &records)?;
        live.push(shown);
    }

    for known in index::paths_in(conn)? {
        if !live.contains(&known) {
            index::sync_file(conn, &known, &[])?;
        }
    }

    Ok(written)
}

/// 该往哪个文件写。项目作用域优先写进项目仓库里那份——跟着仓库走才合直觉
fn target_file(
    paths: &Paths,
    workspace: Option<&Path>,
    record: &MemoryRecord,
) -> Result<PathBuf, String> {
    Ok(match record.scope {
        MemoryScope::Global => paths.global_memory(),
        MemoryScope::Project => match workspace {
            // 绑了工作目录就写进仓库那份：跟着仓库走才合直觉，也才被 git 管着
            Some(workspace) => Paths::workspace_memory(workspace),
            None => {
                let id = record
                    .project_id
                    .as_deref()
                    .filter(|id| !id.is_empty())
                    .ok_or("项目作用域的记忆必须带 project_id，先在工作目录里选一个目录。")?;
                paths.project_memory(id)
            }
        },
        // 话题与临时记忆住在 aglab 目录：它们本来就不该进仓库
        MemoryScope::Session | MemoryScope::Temp => record
            .project_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .map(|id| paths.project_memory(id))
            .unwrap_or_else(|| paths.global_memory()),
    })
}

/// 写入门禁。凡是要往真相源落笔的路径（单条追加、批量导入、就地编辑）都得过这一道：
/// 闸门只有一份，少一个入口就少一次漏记
fn write_gate(record: &MemoryRecord) -> Result<(), String> {
    record.validate()?;
    if let Some(reason) = leaks_sensitive(&record.content) {
        return Err(format!("{reason}——这类内容不写进记忆。"));
    }
    if marked_do_not_store(&record.content) {
        return Err("这句话里带了\"不要记\"的要求，按字面执行了：没有写入。".into());
    }
    Ok(())
}

/// 权限表上那几行 `memory.*` 的执行者。判据只有一份（[`crate::policy::memory_acts`]），
/// 这里负责"问谁"和"拦下来之后留痕"。
///
/// 为什么非要有这一句：表上挂着一行而没有任何运行时判定去 resolve 它，比表上没这一行更坏——
/// 界面上一直显示着"我把清空记忆划成红线了"，而下一次点击照样把整库搬走。
/// `attended` 说清这一下是谁点的：清空/导出/导入是人在界面上点的，注入与提取不是。
/// 拦下来落一行 `Blocked` 审计，只落键，不落任何正文
fn memory_gate(
    app: &AppHandle,
    paths: &Paths,
    mode: crate::policy::MemoryMode,
    attended: bool,
    actor: crate::audit::Actor,
) -> Result<(), String> {
    memory_gate_in(
        &app.path().app_config_dir().map_err(|e| e.to_string())?,
        paths,
        mode,
        attended,
        actor,
    )
}

/// worker 进程的变体（M3 第 2 档）：config_dir 由 Main 传来
fn memory_gate_in(
    config_dir: &std::path::Path,
    paths: &Paths,
    mode: crate::policy::MemoryMode,
    attended: bool,
    actor: crate::audit::Actor,
) -> Result<(), String> {
    let cap = crate::policy::Capability::Memory { mode };
    let level = crate::config::load_from_dir(config_dir)
        .active_policy()
        .resolve(&cap);
    if crate::policy::memory_acts(level, attended) {
        return Ok(());
    }
    let reason = crate::policy::memory_refused(&cap, level);
    let _ = audit_outcome(
        paths,
        actor,
        "memory:refused",
        &cap.key(),
        crate::audit::Outcome::Blocked,
    );
    Err(reason)
}

pub fn append_record(
    conn: &rusqlite::Connection,
    paths: &Paths,
    workspace: Option<&Path>,
    record: &MemoryRecord,
) -> Result<PathBuf, String> {
    append_record_as(conn, paths, workspace, record, None)
}

/// 同上，但审计里"是谁写下这一笔"由调用方给。
///
/// 内容的来源和这次写入的主体不是一回事：蒸馏出来的那条说的话确实来自用户，
/// 而"写"这个动作是系统在反思时做的。让审计两栏混用，事后就分不清
/// "用户说的"与"系统自己记的"——那正是这套记忆系统要能被审查的前提
pub fn append_record_as(
    conn: &rusqlite::Connection,
    paths: &Paths,
    workspace: Option<&Path>,
    record: &MemoryRecord,
    actor: Option<crate::audit::Actor>,
) -> Result<PathBuf, String> {
    write_gate(record)?;
    let file = target_file(paths, workspace, record)?;
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建 {} 失败：{e}", parent.display()))?;
        harden_dir(parent);
    }
    let _guard = write_guard();
    let mut records = load_records_or_quarantine(paths, &file)?;
    if records.iter().any(|existing| existing.id == record.id) {
        return Err(format!("记忆里已经有 {} 了。", record.id));
    }
    records.push(record.clone());
    atomic_write(&file, &render_records(&records))?;
    index::sync_file(conn, &display_path(&paths.root, &file), &records)?;
    // 真相源已经落了，才记这一天的流水账：顺序反了就会出现"账上有、文件里没有"
    govern::append_daily_log(paths, workspace, record)?;
    let actor = actor.unwrap_or_else(|| match record.source {
        MemorySource::User => crate::audit::Actor::User,
        MemorySource::Assistant | MemorySource::Inferred => crate::audit::Actor::Model,
        MemorySource::Import => crate::audit::Actor::Import,
    });
    audit_as(paths, actor, "add", &record.id)?;
    Ok(file)
}

/// 批量落笔，给导入这种"一次几百上千条"的场合用。逐条走 `append_record` 会在同一个
/// 文件上做 n 次"读全文 + 重写全文 + 整片重建索引"，250 条就要跑到分钟级——而换机器
/// 导入恰好是条数最多的那一次。这里按目标文件分组：一个文件只读一次、拼一次、
/// 写一次、同步一次索引。门禁还是那一道，只是不再为每条记录重抄整个文件。
/// 与单条追加的另一处区别：同 id 已存在是跳过而不是报错——导入不该覆盖这一台机器上
/// 用户手改过的那份。
pub fn append_records(
    conn: &rusqlite::Connection,
    paths: &Paths,
    workspace: Option<&Path>,
    records: &[MemoryRecord],
) -> Result<usize, String> {
    let mut groups: Vec<(PathBuf, Vec<&MemoryRecord>)> = Vec::new();
    for record in records {
        write_gate(record)?;
        let file = target_file(paths, workspace, record)?;
        match groups.iter_mut().find(|(path, _)| *path == file) {
            Some((_, bucket)) => bucket.push(record),
            None => groups.push((file, vec![record])),
        }
    }

    let mut imported = 0usize;
    // 一整批一次提交。逐条自动提交在 SQLite 里等于每条记录一次 fsync，1000 条就是
    // 好几秒；失败就整体回滚——已经落盘的 Markdown 不回滚（真相源不是事务的一部分），
    // 下一次任何命令都会按文件重新一遍索引，索引和磁盘对不上这件事本来就会自愈
    conn.execute_batch("BEGIN").map_err(|e| e.to_string())?;
    let mut failure: Option<String> = None;
    for (file, bucket) in groups {
        match flush_group(conn, paths, workspace, &file, bucket) {
            Ok(count) => imported += count,
            Err(error) => {
                failure = Some(error);
                break;
            }
        }
    }
    match failure {
        None => conn
            .execute_batch("COMMIT")
            .map_err(|e| e.to_string())
            .map(|_| imported),
        Some(error) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

/// 一个目标文件一次读写：读全文、把这一批拼上去、整体回写、同步一次索引。
/// 同 id 已存在是跳过而不是报错——导入不该覆盖这一台机器上用户手改过的那份
fn flush_group(
    conn: &rusqlite::Connection,
    paths: &Paths,
    workspace: Option<&Path>,
    file: &Path,
    bucket: Vec<&MemoryRecord>,
) -> Result<usize, String> {
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建 {} 失败：{e}", parent.display()))?;
        harden_dir(parent);
    }
    let _guard = write_guard();
    let mut on_disk = load_records_or_quarantine(paths, file)?;
    let mut seen: std::collections::HashSet<String> =
        on_disk.iter().map(|existing| existing.id.clone()).collect();
    let mut fresh: Vec<MemoryRecord> = Vec::new();
    for record in bucket {
        if !seen.insert(record.id.clone()) {
            continue;
        }
        on_disk.push(record.clone());
        fresh.push(record.clone());
    }
    if fresh.is_empty() {
        return Ok(0);
    }
    atomic_write(file, &render_records(&on_disk))?;
    index::sync_file(conn, &display_path(&paths.root, file), &on_disk)?;
    for record in &fresh {
        // 真相源落了才记流水账：顺序反了就会出现"账上有、文件里没有"
        govern::append_daily_log(paths, workspace, record)?;
        audit_record(paths, "add", record)?;
    }
    Ok(fresh.len())
}

pub fn rewrite_file(
    conn: &rusqlite::Connection,
    paths: &Paths,
    file: &Path,
    records: &[MemoryRecord],
) -> Result<(), String> {
    let shown = display_path(&paths.root, file);
    let body = render_records(records);
    let target = if shown.starts_with('.') {
        // 项目仓库里那份不在 aglab 根下，display_path 会退化成绝对路径
        file.to_path_buf()
    } else {
        paths.root.join(&shown)
    };
    atomic_write(&target, &body)?;
    index::sync_file(conn, &shown, records)?;
    Ok(())
}

/// 找到某条记忆住在哪个文件。索引是入口，找到 path 再回读文件
pub fn locate(
    conn: &rusqlite::Connection,
    paths: &Paths,
    workspace: Option<&Path>,
    id: &str,
) -> Result<(PathBuf, usize), String> {
    let shown: String = conn
        .query_row("SELECT path FROM memories WHERE id = ?1", rusqlite::params![id], |row| row.get(0))
        .map_err(|_| format!("索引里没有记忆 {id}。"))?;
    let file = resolve_file(paths, workspace, &shown);
    let text = fs::read_to_string(&file).map_err(|e| format!("读 {} 失败：{e}", file.display()))?;
    let records = parse_records(&text).map_err(|e| format!("{} 读不了：{e}", file.display()))?;
    let position = records
        .iter()
        .position(|record| record.id == id)
        .ok_or_else(|| format!("{} 在索引里指向 {shown}，但那个文件里没有它。跑一次重建。", id))?;
    Ok((file, position))
}

/// 记忆的审计落到统一 sink（`<app_data_dir>/audit/audit-<日期>.jsonl`）。
/// 这一份是"用户主动做的动作"那一批；由记录来源决定的写入走 `audit_record`
fn audit(paths: &Paths, action: &str, id: &str) -> Result<(), String> {
    audit_as(paths, crate::audit::Actor::User, action, id)
}

fn audit_as(
    paths: &Paths,
    actor: crate::audit::Actor,
    action: &str,
    id: &str,
) -> Result<(), String> {
    audit_outcome(paths, actor, action, id, crate::audit::Outcome::Ok)
}

/// 同上，但要自己挑结论。拦下来的动作不能记成"做完了"，那是账上最省事也最没用的那种谎
fn audit_outcome(
    paths: &Paths,
    actor: crate::audit::Actor,
    action: &str,
    id: &str,
    outcome: crate::audit::Outcome,
) -> Result<(), String> {
    // 审计跟记忆目录分家：它还要覆盖工具与任务，钉在记忆目录里就只剩一半事实
    let root = paths
        .root
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| paths.root.clone());
    crate::audit::record(&root, actor, action, id, outcome)
}

/// 谁写的这条，答案在 `source` 里，不在猜：用户说的、模型推的、导回来的，审计上必须分得开
fn audit_record(paths: &Paths, action: &str, record: &MemoryRecord) -> Result<(), String> {
    let actor = match record.source {
        MemorySource::User => crate::audit::Actor::User,
        MemorySource::Assistant | MemorySource::Inferred => crate::audit::Actor::Model,
        MemorySource::Import => crate::audit::Actor::Import,
    };
    audit_as(paths, actor, action, &record.id)
}

/// 一套配置怎么变成检索参数。只有这一处：`search` 与 `recall_hints` 必须用同一份权重
/// 和同一个条数上限，否则"提示里那条为什么分比注入那条高"就没有答案了
fn search_options<'a>(
    config: &'a MemoryConfig,
    query: &'a str,
    project_id: Option<&'a str>,
) -> SearchOptions<'a> {
    let weights = config.weights;
    SearchOptions {
        query,
        project_id,
        limit: config.search_limit,
        weights: [
            weights.semantic,
            weights.importance,
            weights.freshness,
            weights.scope,
            weights.usage,
        ],
        half_life_days: config.decay_half_life_days,
    }
}

pub fn search(
    conn: &rusqlite::Connection,
    config: &MemoryConfig,
    query: &str,
    project_id: Option<&str>,
) -> Result<Vec<Hit>, String> {
    index::search(conn, &search_options(config, query, project_id))
}

/// 一条主动回忆的提示。`injected` 说它是**这一轮已经让模型看到的**还是"没派上用场"——
/// 前者不必再提示一遍，后者才是这条提示的全部内容
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecallHint {
    pub record_id: String,
    pub content: String,
    pub score: f64,
    /// 为什么提这条：第几跳、经由哪个实体
    pub reason: String,
    pub injected: bool,
}

/// 主动回忆（T08）。只回答"有这几条相关但本轮没用上"，**一个字都不往正文里加**：
/// 注入通道有且仅有一条（`inject::build`），这里的东西走面板/toast 呈现
pub fn recall_hints(
    conn: &rusqlite::Connection,
    config: &MemoryConfig,
    query: &str,
    project_id: Option<&str>,
    injected: &[String],
) -> Result<Vec<RecallHint>, String> {
    if !config.enabled || !config.proactive_recall {
        return Ok(Vec::new());
    }
    let (_semantic, graph) = index::recall(conn, &search_options(config, query, project_id))?;
    Ok(keep_relevant(
        graph.into_iter().map(|found| found.hit).collect(),
        project_id,
    )
    .into_iter()
    .map(|hit| RecallHint {
        reason: hit.why.clone(),
        injected: injected.iter().any(|id| id == &hit.id),
        record_id: hit.id,
        content: hit.content,
        score: hit.score,
    })
    .collect())
}

/// 项目作用域的记忆不得漏进别的项目。索引层按 project_id 存，这里补一道过滤：
/// 检索结果里凡是 project 作用域且 id 不等于当前项目的，一律丢掉
pub fn keep_relevant(hits: Vec<Hit>, project_id: Option<&str>) -> Vec<Hit> {
    hits
        .into_iter()
        .filter(|hit| {
            hit.scope != "project"
                || (project_id.is_some() && hit.project_id.as_deref() == project_id)
        })
        .collect()
}

/// 允许进入"发给模型"那一段的命中。它和 [`keep_relevant`] 是两道各自独立的闸：
/// 那一条管项目隔离（别把一个项目的东西说给另一个项目），这一条管用户自己划的红线
/// （`secret` 的记录哪儿也不去）。合成一条的话，"改了隔离规则却顺手放开红线"这种
/// 一步两变的改动就再也无从审起
pub fn keep_injectable(hits: Vec<Hit>) -> Vec<Hit> {
    hits
        .into_iter()
        .filter(|hit| hit.sensitivity.injectable())
        .collect()
}

// ---------------------------------------------------------------- 命令层

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryView {
    pub record: MemoryRecord,
    pub path: String,
    pub injections: i64,
}

fn list_all(conn: &rusqlite::Connection) -> Result<Vec<MemoryView>, String> {
    let mut statement = conn.prepare(
        "SELECT m.id, m.path, m.type, m.scope, m.project_id, m.status, m.importance, m.confidence, \
                m.stability, m.source, m.created_at, m.tags, m.updated_at, m.ttl_days, m.body, \
                COALESCE(u.injections, 0), m.occurred_at, m.reinforced_at, m.origin, \
                COALESCE((SELECT group_concat(l.to_id, ',') FROM memory_links l \
                          WHERE l.from_id = m.id AND l.kind = 'supersedes'), ''), \
                m.sensitivity \
         FROM memories m LEFT JOIN memory_usage u ON u.id = m.id \
         ORDER BY m.importance DESC, COALESCE(m.reinforced_at, m.created_at) DESC",
    )
    .map_err(|e| e.to_string())?;
    let rows = statement.query_map([], |row| {
        let tags: String = row.get(11)?;
        let supersedes: String = row.get(19)?;
        let view = MemoryView {
            path: row.get(1)?,
            injections: row.get(15)?,
            record: MemoryRecord {
                id: row.get(0)?,
                kind: row.get::<_, String>(2)?
                    .parse()
                    .unwrap_or(MemoryKind::Fact),
                scope: row.get::<_, String>(3)?.parse().unwrap_or(MemoryScope::Global),
                project_id: row.get(4)?,
                status: row.get::<_, String>(5)?.parse().unwrap_or(MemoryStatus::Active),
                importance: row.get::<_, i64>(6)? as u32,
                confidence: row.get(7)?,
                stability: row.get::<_, String>(8)?
                    .parse()
                    .unwrap_or(Stability::Stable),
                source: row.get::<_, String>(9)?
                    .parse()
                    .unwrap_or(MemorySource::Inferred),
                created_at: row.get(10)?,
                // 面板与"标记为不可外发"的那个动作读的就是这一格：分级不落界面等于没有分级
                sensitivity: record::MemorySensitivity::parse_loose(&row.get::<_, String>(20)?),
                // 视图是读侧的东西：`entities` / `extra` / `last_used_at` 都不从索引行还原
                // （它们要回读 `.md` 才有），这里给空，与 `extra: Vec::new()` 同一条规矩
                tags: tags.split(',').filter(|item| !item.is_empty()).map(String::from).collect(),
                entities: Vec::new(),
                updated_at: row.get(12)?,
                occurred_at: row.get(16)?,
                reinforced_at: row.get(17)?,
                origin: None,
                ttl_days: row.get::<_, Option<i64>>(13)?.map(|value| value as u32),
                content: row.get(14)?,
                supersedes: supersedes.split(',').filter(|item| !item.is_empty()).map(String::from).collect(),
                last_used_at: None,
                extra: Vec::new(),
            },
        };
        // 出处单独解：它读不懂要报"出处读不了"，而不是塞进 rusqlite 的类型错误里
        Ok((view, row.get::<_, Option<String>>(18)?))
    })
    .map_err(|e| e.to_string())?;
    let mut views = Vec::new();
    for row in rows {
        let (mut view, raw_origin) = row.map_err(|e| e.to_string())?;
        view.record.origin = match raw_origin.as_deref() {
            Some(text) => Some(Origin::decode(text)?),
            None => None,
        };
        views.push(view);
    }
    Ok(views)
}

fn active_context(app: &AppHandle) -> Result<(Paths, MemoryConfig, Option<PathBuf>, Option<String>), String> {
    active_context_in(
        &app.path().app_config_dir().map_err(|e| e.to_string())?,
        &app.path().app_data_dir().map_err(|e| e.to_string())?,
    )
}

/// worker 进程的变体（M3 第 2 档）：memory 根 = data_dir/memory，
/// 项目/工作区从 config_dir 的 config.json 现读
fn active_context_in(
    config_dir: &std::path::Path,
    data_dir: &std::path::Path,
) -> Result<(Paths, MemoryConfig, Option<PathBuf>, Option<String>), String> {
    let paths = Paths::new(data_dir.join("memory"));
    ensure_layout(&paths)?;
    let config = load_config(&paths);
    let main = crate::config::load_from_dir(config_dir);
    let project = main.active_project();
    let workspace = project.map(|item| PathBuf::from(&item.path));
    let project_id = project.map(|item| item.id.clone());
    Ok((paths, config, workspace, project_id))
}

/// 每个命令都要先同步索引：用户可能刚刚手改过文件，不能等重启
fn with_index<T>(
    app: &AppHandle,
    body: impl FnOnce(&rusqlite::Connection, &Paths, &MemoryConfig, Option<&Path>, Option<&str>) -> Result<T, String>,
) -> Result<T, String> {
    let (paths, memory_config, workspace, project_id) = active_context(app)?;
    let conn = index::open(&paths.index_db())?;
    sync_all(&conn, &paths, workspace.as_deref(), project_id.as_deref())?;
    body(
        &conn,
        &paths,
        &memory_config,
        workspace.as_deref(),
        project_id.as_deref(),
    )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AddArgs {
    pub content: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub importance: Option<u32>,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[tauri::command]
pub fn memory_add(app: AppHandle, args: AddArgs) -> Result<MemoryView, String> {
    with_index(&app, |conn, paths, config, workspace, project_id| {
        if !config.enabled {
            return Err("记忆功能是关的。先在设置里打开。".into());
        }
        let content = args.content.trim();
        if content.is_empty() {
            return Err("要记的内容是空的。".into());
        }

        let scope: MemoryScope = args
            .scope
            .clone()
            .unwrap_or_else(|| "global".into())
            .parse()?;
        let now = now_rfc3339();
        let record = MemoryRecord {
            id: new_id(),
            kind: args.kind.clone().unwrap_or_else(|| "preference".into()).parse()?,
            scope,
            project_id: (scope == MemoryScope::Project)
                .then(|| project_id.map(|id| id.to_string()))
                .flatten(),
            status: MemoryStatus::Active,
            importance: args.importance.unwrap_or(4),
            // 用户显式让我记的东西没有"猜错"的余地，置信度就是满的
            confidence: 1.0,
            stability: Stability::Stable,
            source: MemorySource::User,
            // 手记默认是 `public`：要标成不可外发，用列表里那一下"标记"的动作，
            // 而不是让每一次记东西都先回答一个安全问题
            sensitivity: record::MemorySensitivity::default(),
            created_at: now.clone(),
            updated_at: now,
            // 手记没有"被提取"这回事：它说不出来自哪次对话，出处就是空的
            occurred_at: None,
            reinforced_at: None,
            origin: None,
            last_used_at: None,
            ttl_days: None,
            tags: args.tags.clone(),
            // 手记不填实体：写进去的那句话本身会被规则扫一遍，界面上也就不会多出一份要人维护的名单
            entities: Vec::new(),
            supersedes: Vec::new(),
            content: content.to_string(),
            extra: Vec::new(),
        };
        let file = append_record(conn, paths, workspace, &record)?;
        Ok(MemoryView {
            path: display_path(&paths.root, &file),
            injections: 0,
            record,
        })
    })
}

#[tauri::command]
pub fn memory_list(app: AppHandle) -> Result<Vec<MemoryView>, String> {
    with_index(&app, |conn, _paths, _config, _workspace, _project_id| list_all(conn))
}

#[tauri::command]
pub fn memory_search(app: AppHandle, query: String) -> Result<Vec<Hit>, String> {
    with_index(&app, |conn, _paths, config, _workspace, project_id| {
        let hits = search(conn, config, &query, project_id)?;
        Ok(keep_relevant(hits, project_id))
    })
}

/// 主动回忆的提示（T08）。只读：取那一轮已经注入的 id 当对照，把实体图上相关、
/// 本轮却没派上用场的几条报给面板。注入通道不经过这里
#[tauri::command]
pub fn memory_recall_hints(
    app: AppHandle,
    query: String,
    conversation_id: String,
) -> Result<Vec<RecallHint>, String> {
    with_index(&app, |conn, paths, config, _workspace, project_id| {
        let injected: Vec<String> = inject::why_of(paths, &conversation_id)
            .map(|done| done.items.iter().map(|item| item.id.clone()).collect())
            .unwrap_or_default();
        recall_hints(conn, config, &query, project_id, &injected)
    })
}

/// 事件时间线（T06）：说过"什么时候发生"的那些记录，按业务时间倒序给。
/// 条数上限夹一道：这是命令入口，不该让一个手滑的 1e6 变成全表扫描
#[tauri::command]
pub fn memory_timeline(app: AppHandle, limit: Option<usize>) -> Result<Vec<TimelineRow>, String> {
    with_index(&app, |conn, _paths, _config, _workspace, _project_id| {
        index::timeline(conn, limit.unwrap_or(30).clamp(1, 200))
    })
}

/// 现在还有实际影响的冲突，成对给。只读，不写、也不注入任何东西——
/// 呈现走设置页，裁决走 `memory_conflict_resolve`
#[tauri::command]
pub fn memory_conflicts(app: AppHandle) -> Result<Vec<govern::ConflictPair>, String> {
    with_index(&app, |conn, _paths, config, _workspace, _project_id| {
        govern::conflicts(conn, config)
    })
}

/// 人选完的一刻才落盘：败者归档、胜者带上取代边，两条正文都留在 Markdown 里。
/// 关着记忆功能时拒绝写，与 `memory_add` 同一口径
#[tauri::command]
pub fn memory_conflict_resolve(
    app: AppHandle,
    a: String,
    b: String,
    choice: ConflictChoice,
) -> Result<String, String> {
    with_index(&app, |conn, paths, config, workspace, _project_id| {
        if !config.enabled {
            return Err("记忆功能是关的。先在设置里打开。".into());
        }
        govern::resolve(conn, paths, workspace, &a, &b, choice)
    })
}

/// 这条记忆是从哪儿来的：哪次对话、哪几条消息、哪个文件、被注入过几次。
/// 只答标识，正文由 `memory_list` 那份给——出处里不该多落一份原文
#[tauri::command]
pub fn memory_source(app: AppHandle, id: String) -> Result<SourceView, String> {
    with_index(&app, |conn, _paths, _config, _workspace, _project_id| {
        index::source_of(conn, id.trim())
    })
}

/// 忘记哪一条：整句包含的直接删；对不上的不再盲删检索第一名——中文按单字+双字
/// 匹配，几乎任何一句话都能跟某条记忆共享一个字，而删掉的记忆回不来。
/// 把最像的几条报给用户，让他用 id 点名。候选是 (id, 路径, 正文) 三元组，
/// 为的是判据可以直接单测，不必为它拼一整个 Hit
fn pick_forget_target(
    candidates: &[(String, String, String)],
    needle: &str,
) -> Result<(String, String), String> {
    let needle_lower = needle.to_lowercase();
    if let Some((id, path, _)) = candidates
        .iter()
        .find(|(_, _, content)| content.to_lowercase().contains(&needle_lower))
    {
        return Ok((id.clone(), path.clone()));
    }
    let hints: Vec<String> = candidates
        .iter()
        .take(3)
        .map(|(id, _, content)| {
            let preview: String = content.chars().take(40).collect();
            format!("- {id}　{preview}")
        })
        .collect();
    if hints.is_empty() {
        return Err(format!("没找到跟「{needle}」对得上的记忆。"));
    }
    Err(format!(
        "没有整句对得上的记忆。最像的几条：\n{}\n要点名哪一条，带上它的 id 再说一次「忘记」。",
        hints.join("\n")
    ))
}

/// 忘记。给 id 就精确删，给一句话就先检索并只删最像的那条——
/// 用户说"忘记我提过喜欢用 pnpm"时不该被逼着先去查 id
#[tauri::command]
pub fn memory_forget(app: AppHandle, query: String) -> Result<String, String> {
    with_index(&app, |conn, paths, config, workspace, project_id| {
        // 检索、读文件、改写、删索引是一串连续动作，中间不许插进别的写入
        let _guard = write_guard();
        let needle = query.trim();
        if needle.is_empty() {
            return Err("要忘记什么，总得给个说法。".into());
        }

        let direct: Option<String> = conn
            .query_row(
                "SELECT path FROM memories WHERE id = ?1",
                rusqlite::params![needle],
                |row| row.get(0),
            )
            .ok();

        let (target_id, target_path) = match direct {
            Some(path) => (needle.to_string(), path),
            None => {
                let hits = keep_relevant(search(conn, config, needle, project_id)?, project_id);
                let candidates: Vec<(String, String, String)> = hits
                    .iter()
                    .map(|hit| (hit.id.clone(), hit.path.clone(), hit.content.clone()))
                    .collect();
                pick_forget_target(&candidates, needle)?
            }
        };

        let file = resolve_file(paths, workspace, &target_path);
        let mut records = parse_records(&read_text(&file))
            .map_err(|e| format!("{} 读不了：{e}", file.display()))?;
        let before = records.len();
        records.retain(|record| record.id != target_id);
        if records.len() == before {
            return Err(format!("{target_id} 不在 {target_path} 里。"));
        }
        rewrite_file(conn, paths, &file, &records)?;
        index::forget(conn, &target_id)?;
        audit(paths, "forget", &target_id)?;
        Ok(format!("已忘记 {target_id}（{target_path}）"))
    })
}

fn resolve_file(paths: &Paths, workspace: Option<&Path>, shown: &str) -> PathBuf {
    if let Some(workspace) = workspace {
        if shown.contains(PROJECT_LOCAL_DIR) {
            return workspace.join(PROJECT_LOCAL_DIR).join(RECORD_FILE);
        }
    }
    paths.root.join(shown)
}

#[tauri::command]
pub async fn memory_rebuild(app: AppHandle) -> Result<usize, String> {
    tauri::async_runtime::spawn_blocking(move || {
        with_index(&app, |conn, paths, _config, workspace, project_id| {
            conn.execute("DELETE FROM memories", []).map_err(|e| e.to_string())?;
            conn.execute("DELETE FROM memories_fts", []).map_err(|e| e.to_string())?;
            conn.execute("DELETE FROM memory_links", []).map_err(|e| e.to_string())?;
            sync_all(conn, paths, workspace, project_id)
        })
    })
    .await
    .map_err(|e| format!("重建线程没跑完：{e}"))?
}

/// 改一条记忆要动的字段。没给的键保持原样——UI 上"候选转正"只发 status，
/// 不该顺手把正文和置信度重置成默认值。
/// 但**认不出的键名要报错**：`{senstivity: "secret"}` 合出来是一份空补丁，
/// 于是"让这条不再出去"这个动作被静默丢掉，而那条记忆照旧出门
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct EditPatch {
    pub content: Option<String>,
    pub importance: Option<u32>,
    pub confidence: Option<f64>,
    pub tags: Option<Vec<String>>,
    pub status: Option<String>,
    /// `public | private | secret`。这是"把这条划进不出去的那一档"的唯一入口，
    /// 所以它必须在这里——只在 `.md` 里能改，界面上就等于没有这条规则
    pub sensitivity: Option<String>,
}

impl EditPatch {
    /// 六个格子全空。合上 `deny_unknown_fields` 之后，唯一走到这里的方式是调用方
    /// 真的发了一份 `{}`
    fn is_empty(&self) -> bool {
        self.content.is_none()
            && self.importance.is_none()
            && self.confidence.is_none()
            && self.tags.is_none()
            && self.status.is_none()
            && self.sensitivity.is_none()
    }
}

#[tauri::command]
pub fn memory_edit(app: AppHandle, id: String, patch: EditPatch) -> Result<MemoryView, String> {
    with_index(&app, |conn, paths, _config, workspace, _project_id| {
        edit_record(conn, paths, workspace.as_deref(), &id, &patch)
    })
}

fn edit_record(
    conn: &rusqlite::Connection,
    paths: &Paths,
    workspace: Option<&Path>,
    id: &str,
    patch: &EditPatch,
) -> Result<MemoryView, String> {
    // 空补丁不是"改了三十年老账"，是一次没发生的事，而它下游三个副作用都真会跑：
    // `updated_at` 被推到今天（候选区的 TTL 与自动转正那条曲线都读这一格，
    // 见 govern.rs 的 `ttl_days, updated_at`），文件重写给一条审计 "edit"。
    // 所以先拒，别让它变成"每问一次就续一次命"
    if patch.is_empty() {
        return Err("这一份补丁什么都没改。".into());
    }
    let _guard = write_guard();
    let (file, position) = locate(conn, paths, workspace, id)?;
    let mut records = parse_records(&read_text(&file))
        .map_err(|e| format!("{} 读不了：{e}", file.display()))?;
    let mut next = records[position].clone();

    if let Some(content) = patch.content.as_deref() {
        let content = content.trim();
        if content.is_empty() {
            return Err("正文不能改成空的。要它消失就说「忘记」。".into());
        }
        next.content = content.to_string();
    }
    if let Some(importance) = patch.importance {
        next.importance = importance;
    }
    if let Some(confidence) = patch.confidence {
        next.confidence = confidence;
    }
    if let Some(tags) = patch.tags.clone() {
        next.tags = tags;
    }
    if let Some(status) = patch.status.as_deref() {
        next.status = status.parse()?;
    }
    if let Some(level) = patch.sensitivity.as_deref() {
        // 读不懂就报错，不能当成"这次没改这一项"：用户点的是"让这条不再出去"，
        // 系统收下另一个它不认识的值而行为照旧，那条就照常出门——而且是静默的
        next.sensitivity = level.parse()?;
    }
    next.updated_at = now_rfc3339();
    // 改正文等于重写一次记忆，所以编辑也走同一道门禁，而不是在这里自己抄一遍检查：
    // 否则"先记一句干净的、再编辑成密码"就是漏记敏感信息的后门，而且抄的那份会漂移
    write_gate(&next)?;

    records[position] = next.clone();
    rewrite_file(conn, paths, &file, &records)?;
    audit(paths, "edit", &next.id)?;

    // 注入次数住在 memory_usage 里，编辑不该把它清零：一条被用了 20 次的记忆
    // 改个错别字就当从没被用过，使用频率这一项就白算了
    let injections: i64 = conn
        .query_row(
            "SELECT COALESCE(injections, 0) FROM memory_usage WHERE id = ?1",
            rusqlite::params![next.id],
            |row| row.get(0),
        )
        .unwrap_or(0);

    Ok(MemoryView {
        path: display_path(&paths.root, &file),
        injections,
        record: next,
    })
}

/// 用户对冲突的四种选择。除了"用户明说 > 推断"这一条自动规则，其余都得由人来点
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConflictChoice {
    /// 留新的那条，旧的归档
    NewerWins,
    /// 留旧的那条，新的归档
    OlderWins,
    /// 两条都留着：清掉冲突标记，不再打扰
    KeepBoth,
    /// 两条都归档
    ArchiveBoth,
}

/// 就地改一组记录的**元数据**，正文一个字都不动。取代边、强化章都走这一条。
///
/// 它和 `append_record` / `edit_record` 共用同一道 `write_gate`：写入门禁只有一份，
/// 另开一条"只改 frontmatter 所以不用过滤"的路，早晚会长出"文件里没有、索引里却有"
/// 那种状态。返回真正被改动的条数——没变过就一个字节都不写盘（同 `set_status`）
fn stamp_records(
    conn: &rusqlite::Connection,
    paths: &Paths,
    workspace: Option<&Path>,
    ids: &[String],
    actor: crate::audit::Actor,
    action: &str,
    edit: &(dyn Fn(&mut MemoryRecord) -> bool + Sync),
) -> Result<usize, String> {
    // 按文件分组：一个文件只读一次、拼一次、写一次、同步一次索引
    let _guard = write_guard();
    let mut by_file: std::collections::BTreeMap<PathBuf, Vec<String>> = std::collections::BTreeMap::new();
    for id in ids {
        let (file, _position) = locate(conn, paths, workspace, id)?;
        by_file.entry(file).or_default().push(id.clone());
    }

    let mut touched_total = 0usize;
    for (file, targets) in by_file {
        let mut records = parse_records(&read_text(&file))
            .map_err(|e| format!("{} 本来就读不了，先修它：{e}", file.display()))?;
        let mut touched: Vec<String> = Vec::new();
        for record in &mut records {
            if !targets.iter().any(|id| id == &record.id) {
                continue;
            }
            if edit(record) {
                write_gate(record)?;
                touched.push(record.id.clone());
            }
        }
        if touched.is_empty() {
            continue;
        }
        rewrite_file(conn, paths, &file, &records)?;
        for id in &touched {
            audit_as(paths, actor, action, id)?;
        }
        touched_total += touched.len();
    }
    Ok(touched_total)
}

/// 被注入过一次，就是一条记忆"还活着"的证据：把 `reinforced_at` 盖进真相源。
///
/// 一天最多盖一次。同一天里反复用上没有新信息，而每次都重写 MEMORY.md 会把
/// `.ai-memory/` 里那些跟着仓库走的文件刷成一片噪音——用户 diff 出来的该是
/// "记住了什么"，不是"今天又被用了几回"
fn reinforce_records(
    conn: &rusqlite::Connection,
    paths: &Paths,
    workspace: Option<&Path>,
    ids: &[String],
    at: &str,
) -> Result<usize, String> {
    let day = |stamp: &str| stamp.split('T').next().unwrap_or(stamp).to_string();
    let today = day(at);
    stamp_records(
        conn,
        paths,
        workspace,
        ids,
        crate::audit::Actor::Model,
        "reinforce",
        &|record: &mut MemoryRecord| {
            if record.reinforced_at.as_deref().map(day).as_deref() == Some(today.as_str()) {
                return false;
            }
            // 只往前走：拿一个更早的时间去"强化"等于把记忆判旧
            if record.reinforced_at.as_deref().is_some_and(|held| held > at) {
                return false;
            }
            record.reinforced_at = Some(at.to_string());
            true
        },
    )
}

/// 一份导出的全部内容。`version` 是给以后格式变更留的口子：读的时候对不上就报错，
/// 不要拿一个不认识的结构去猜用户的记忆
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportBundle {
    pub version: u32,
    pub exported_at: String,
    pub records: Vec<MemoryRecord>,
}

pub const EXPORT_VERSION: u32 = 1;

/// 导出成 JSON 文本。只读索引，不碰网络、不碰文件
fn bundle_of(conn: &rusqlite::Connection) -> Result<String, String> {
    let bundle = ExportBundle {
        version: EXPORT_VERSION,
        exported_at: now_rfc3339(),
        records: list_all(conn)?
            .into_iter()
            .map(|view| view.record)
            .collect(),
    };
    serde_json::to_string_pretty(&bundle).map_err(|e| e.to_string())
}

/// 把一份导出文本落到当前记忆根目录。批量走 `append_records`，所以导入和手记过的是
/// 同一道门禁；同 id 已存在就跳过——覆盖会吃掉用户在这一台机器上手改过的那份。
/// 项目作用域不带当前工作目录：那条记忆该落回它自己的 projects/<id>/，
/// 而不是被顺手塞进此刻打开的仓库
fn import_bundle(
    conn: &rusqlite::Connection,
    paths: &Paths,
    text: &str,
) -> Result<usize, String> {
    let bundle: ExportBundle = serde_json::from_str(text.trim())
        .map_err(|e| format!("这不像 aglab 导出的记忆文件：{e}"))?;
    if bundle.version != EXPORT_VERSION {
        return Err(format!(
            "这份导出是版本 {}，当前只认版本 {EXPORT_VERSION}。",
            bundle.version
        ));
    }
    let fresh: Vec<MemoryRecord> = bundle
        .records
        .into_iter()
        .filter(|record| {
            // 索引里已经有的 id 一律不带进批量写：跳过而不是覆盖
            !conn
                .query_row(
                    "SELECT 1 FROM memories WHERE id = ?1",
                    rusqlite::params![record.id],
                    |_| Ok(true),
                )
                .unwrap_or(false)
        })
        .collect();
    append_records(conn, paths, None, &fresh)
}

/// 这五条都是"可能要跑上一截"的：批量写文件、整片重建索引、把 10k 条序列化成 JSON。
/// 必须 async + spawn_blocking——Tauri 2 里非 async 命令跑在主线程上，
/// 那会让整扇窗口白屏，而用户此刻唯一能做的就是盯着不动的界面

#[tauri::command]
pub async fn memory_export(app: AppHandle, passphrase: Option<String>) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        with_index(&app, |conn, paths, _config, _workspace, _project_id| {
            // 这一行今天新增的是 Deny 那一档：备份会离开这台机器的目录，
            // 有人想给它划红线是完全合理的
            memory_gate(
                &app,
                paths,
                crate::policy::MemoryMode::Export,
                true,
                crate::audit::Actor::User,
            )?;
            let bundle = bundle_of(conn)?;
            // 口令留空 = 不加密，字节与加这一格之前完全一致：加密是可选的信封，不是新格式
            match passphrase.as_deref().filter(|pass| !pass.is_empty()) {
                Some(pass) => crate::envelope::encrypt(bundle.as_bytes(), pass),
                None => Ok(bundle),
            }
        })
    })
    .await
    .map_err(|e| format!("导出线程没跑完：{e}"))?
}

#[tauri::command]
pub async fn memory_import(
    app: AppHandle,
    text: String,
    passphrase: Option<String>,
) -> Result<usize, String> {
    tauri::async_runtime::spawn_blocking(move || {
        with_index(&app, |conn, paths, _config, _workspace, _project_id| {
            import_gate(&app, paths)?;
            let plain = open_payload(&text, passphrase.as_deref())?;
            import_bundle(conn, paths, &plain)
        })
    })
    .await
    .map_err(|e| format!("导入线程没跑完：{e}"))?
}

/// 从文件导入。设置页只能交一个路径过来——读文件这件事前端做不了，它没有文件系统权限
#[tauri::command]
pub async fn memory_import_file(
    app: AppHandle,
    path: String,
    passphrase: Option<String>,
) -> Result<usize, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let text = fs::read_to_string(&path).map_err(|e| format!("读 {path} 失败：{e}"))?;
        with_index(&app, |conn, paths, _config, _workspace, _project_id| {
            import_gate(&app, paths)?;
            let plain = open_payload(&text, passphrase.as_deref())?;
            import_bundle(conn, paths, &plain)
        })
    })
    .await
    .map_err(|e| format!("导入线程没跑完：{e}"))?
}

/// 加密的备份没有口令就读不开，且**不退回明文**：静默降级等于把"加密"两个字变成口号。
/// 没加密的文本原样通过——导入同时认两种，认哪种由备份自己的头说，不由调用方猜
fn open_payload(text: &str, passphrase: Option<&str>) -> Result<String, String> {
    if !crate::envelope::is_armored(text) {
        return Ok(text.to_string());
    }
    let pass = passphrase
        .filter(|pass| !pass.is_empty())
        .ok_or_else(|| "这份备份是加密的：填上导出时用的口令再导入。".to_string())?;
    let bytes = crate::envelope::decrypt(text, pass)?;
    String::from_utf8(bytes).map_err(|_| "解开之后不是 UTF-8 文本：口令或文件不对。".to_string())
}

/// 导入是两条命令共用的一个动作，所以那一行表也只问一次。它是有人在界面上点的：
/// 表上写「要有人点头」时这一发照样过，写红线时不
fn import_gate(app: &AppHandle, paths: &Paths) -> Result<(), String> {
    memory_gate(app, paths, crate::policy::MemoryMode::Write, true, crate::audit::Actor::Import)
}

/// 记忆根目录下还"活着"的记录文件：MEMORY.md 与 daily/*.md。archive/ 整个跳过——
/// 那已经是历史；profile/soul/rules 也不在此列，它们是常驻区而不是记忆条目，
/// 清空记忆不该把用户自己写的人格设定一起搬走
fn live_record_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|value| value.to_str()).unwrap_or("");
        if path.is_dir() {
            if name != ARCHIVE_DIR {
                live_record_files(&path, out);
            }
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        let in_daily = path
            .parent()
            .and_then(|parent| parent.file_name())
            .and_then(|value| value.to_str())
            == Some(DAILY_DIR);
        if in_daily || name == RECORD_FILE {
            out.push(path);
        }
    }
}

/// 一键清空。索引整片清掉，Markdown 不蒸发：记录文件搬进
/// `archive/wiped-<时间>/`，删错了还能从那里捡回来。可重建的前提是真相源还在
fn wipe(
    conn: &rusqlite::Connection,
    paths: &Paths,
    workspace: Option<&Path>,
    project_id: Option<&str>,
) -> Result<usize, String> {
    // 清点、搬文件、清索引三步不许与别的写入交错：搬到一半时另一路追加，
    // 会往刚被搬空的文件里再写一条，然后又被索引清扫掉
    let _guard = write_guard();
    let cleared = index::count(conn, None)? as usize;
    // 尾巴上的毫秒是给"连点两次清空"留的：同一秒里生成两个同名的归档目录，
    // 第二次就会把第一次搬进去的文件覆盖掉
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or(0);
    let stamp = now_rfc3339().replace(':', "-").replace('+', "-");
    let dir = paths.root.join(ARCHIVE_DIR).join(format!("wiped-{stamp}-{millis}"));
    fs::create_dir_all(&dir).map_err(|e| format!("创建 {} 失败：{e}", dir.display()))?;
    harden_dir(&dir);

    let mut targets: Vec<PathBuf> = Vec::new();
    live_record_files(&paths.root, &mut targets);
    for file in record_files(paths, workspace, project_id) {
        if !targets.contains(&file) {
            targets.push(file);
        }
    }
    for file in &targets {
        let name = display_path(&paths.root, file).replace('/', "__");
        fs::rename(file, dir.join(name))
            .map_err(|e| format!("把 {} 搬走失败：{e}", file.display()))?;
    }

    for sql in [
        "DELETE FROM memories",
        "DELETE FROM memories_fts",
        "DELETE FROM memory_links",
        "DELETE FROM memory_usage",
    ] {
        conn.execute(sql, []).map_err(|e| e.to_string())?;
    }
    audit(paths, "wipe", "all")?;
    Ok(cleared)
}

#[tauri::command]
pub async fn memory_wipe(app: AppHandle) -> Result<usize, String> {
    tauri::async_runtime::spawn_blocking(move || {
        with_index(&app, |conn, paths, _config, workspace, project_id| {
            // 表上这一行默认就是「要有人点头」，而清空那一下正是人点的（界面上还有一句确认）。
            // 这一格真正新增的是红线：划了它，下一次点击搬不走任何东西
            memory_gate(
                &app,
                paths,
                crate::policy::MemoryMode::Wipe,
                true,
                crate::audit::Actor::User,
            )?;
            wipe(conn, paths, workspace.as_deref(), project_id.as_deref())
        })
    })
    .await
    .map_err(|e| format!("清空线程没跑完：{e}"))?
}

#[tauri::command]
pub fn memory_stats(app: AppHandle) -> Result<serde_json::Value, String> {
    with_index(&app, |conn, paths, config, workspace, project_id| {
        // 限额与"该蒸馏了"要在 TTL 扫过之后判：过期的条目还留着，报出来的
        // 字符数就是虚高的，UI 会据此提示一件其实不必要的事
        let expired = govern::maintain(conn, paths, workspace, config)?;
        let over = govern::over_budget(paths, workspace, project_id, config)?;
        let stale_logs = govern::eligible_logs(paths, workspace, project_id, config);
        Ok(serde_json::json!({
            "total": index::count(conn, None)?,
            "active": index::count(conn, Some(MemoryStatus::Active))?,
            "candidates": index::count(conn, Some(MemoryStatus::Candidate))?,
            "archived": index::count(conn, Some(MemoryStatus::Archived))?,
            "root": paths.root.display().to_string(),
            "enabled": config.enabled,
            "cloudSync": config.cloud_sync,
            // 超限额只报事实，不截断：删用户的记忆是最坏的选择，该由人来决定蒸馏
            "overBudget": over.iter().map(govern::OverBudget::shown).collect::<Vec<_>>(),
            "needsDistill": !over.is_empty() || !stale_logs.is_empty(),
            "expiredNow": expired.archived.len(),
            "dailyKeepDays": config.daily_keep_days,
            "distillAfterDays": config.distill_after_days,
        }))
    })
}

#[tauri::command]
pub fn memory_config_get(app: AppHandle) -> Result<MemoryConfig, String> {
    let paths = Paths::app(&app)?;
    ensure_layout(&paths)?;
    Ok(load_config(&paths))
}

/// 补丁往配置上合的这一半不碰磁盘，所以能单独判：三道闸都在这儿，
/// 留在命令里就只有拿得到 `AppHandle` 的人能测到
fn merge_config_patch(
    current: &MemoryConfig,
    patch: &serde_json::Value,
) -> Result<MemoryConfig, String> {
    let changes = patch
        .as_object()
        .ok_or_else(|| "记忆配置补丁得是一个对象。".to_string())?;

    let mut merged = serde_json::to_value(current).map_err(|e| e.to_string())?;
    let target = merged
        .as_object_mut()
        .ok_or_else(|| "当前记忆配置序列化出来不是对象。".to_string())?;

    for (key, value) in changes {
        if !target.contains_key(key) {
            // 和 config_patch 同一条规矩：键名写错不能被静默丢掉，
            // 否则界面拨完开关只会表现为"没反应"
            return Err(format!("记忆配置里没有「{key}」这一项，补丁没有写入。"));
        }
        target.insert(key.clone(), value.clone());
    }

    let next: MemoryConfig =
        serde_json::from_value(merged).map_err(|e| format!("补丁合进去后配置读不回来了：{e}"))?;
    if next.cloud_sync {
        return Err("云同步没有实现，也不打算实现。记忆只留在本机。".into());
    }
    // 半衰期小于一天时整条曲线就是噪声（今天写下的一条到明天已经对折），一年以内是够用的上限。
    // 读侧还有一道 `.max(1.0)`：老配置里如果躺着个 0，那一次检索不该算出 NaN
    if !(1.0..=3650.0).contains(&next.decay_half_life_days) {
        return Err(format!(
            "新鲜度半衰期要在 1 到 3650 天之间，给的是 {}。默认 {} 天。",
            next.decay_half_life_days,
            crate::memory::decay::HALF_LIFE_DAYS
        ));
    }
    Ok(next)
}

#[tauri::command]
pub fn memory_config_set(app: AppHandle, patch: serde_json::Value) -> Result<MemoryConfig, String> {
    let paths = Paths::app(&app)?;
    ensure_layout(&paths)?;
    let next = merge_config_patch(&load_config(&paths), &patch)?;
    save_config(&paths, &next)?;
    Ok(next)
}

// ---------------------------------------------------------------- 对话侧接口

/// 这一轮该注入什么。chat.rs 只该看到这一个入口：它把返回的 body 当成一个命名段
/// 追加进日志，把返回的 items 报给前端显示
pub fn inject_for_turn(
    app: &AppHandle,
    conversation_id: &str,
    query: &str,
) -> Result<Option<Injection>, String> {
    let config_dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    inject_for_turn_in(&config_dir, &data_dir, conversation_id, query)
}

/// worker 进程的变体（M3 第 2 档）：目录由 Main 经 CLI 传来；
/// inject::build 的 app 位传 None——决策增强是可选回退，worker 里不跑
pub fn inject_for_turn_in(
    config_dir: &std::path::Path,
    data_dir: &std::path::Path,
    conversation_id: &str,
    query: &str,
) -> Result<Option<Injection>, String> {
    let (paths, config, workspace, project_id) = active_context_in(config_dir, data_dir)?;
    if !config.enabled || !config.auto_inject {
        return Ok(None);
    }
    // 注入没有弹窗，所以这一路 `attended` 是假的：表上那行写成「要有人点头」就是不再注入。
    // 默认档是 Allow，因此今天一条都不会多挡
    if memory_gate_in(config_dir, &paths, crate::policy::MemoryMode::Read, false, crate::audit::Actor::User)
        .is_err()
    {
        return Ok(None);
    }
    let conn = index::open(&paths.index_db())?;
    // 注入前对一遍盘：用户在记事本里改过的 Markdown，这一轮就该按新的来
    sync_all(&conn, &paths, workspace.as_deref(), project_id.as_deref())?;
    // 再扫一遍 TTL：过期的临时记忆被继续注入，是这套系统最容易惹恼用户的地方
    govern::maintain(&conn, &paths, workspace.as_deref(), &config)?;
    let Some(shot) = inject::build(&conn, &paths, &config, query, project_id.as_deref(), None)? else {
        return Ok(None);
    };
    inject::remember(&paths, conversation_id, &shot)?;
    // 强化（reinforced_at / usage 计数）不在这里做：这一轮的记忆段**还没确认发出**——
    // chat.rs 随后的让步判定（超上限 / 窗口装不下）会整段撤回它，撤掉的记忆模型根本
    // 没看到，先强化就是给它白发热度。确认发出后由 [`reinforce_injection`] 补这一笔
    // 注入是读侧事件：审计万一没落上，不该让用户这一轮发不出去
    let _ = audit(&paths, "inject", conversation_id);
    Ok(Some(shot))
}

/// 确认这轮注入真的发出去了，才把 `reinforced_at` 与使用计数盖进真相源。
/// `reinforced_at` 是新鲜度读的那一个时间，而它必须在 Markdown 里，否则删库重建
/// 之后一条"常被用上"的记忆会突然显得又老又生。失败只降级不拦路：留痕晚一天
/// 到位，好过让一轮已经发出的对话回头报错
pub fn reinforce_injection(app: &AppHandle, shot: &Injection) -> Result<(), String> {
    let config_dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    reinforce_injection_in(&config_dir, &data_dir, shot)
}

/// worker 进程的变体（M3 第 2 档）
pub fn reinforce_injection_in(
    config_dir: &std::path::Path,
    data_dir: &std::path::Path,
    shot: &Injection,
) -> Result<(), String> {
    // 纯重新生成（提法为空）不算"用过"：它没表达任何需求，照记的话
    // 新鲜度会把同一批头部记忆无中生有地加热，热度偏置就是这么长出来的
    if shot.query.trim().is_empty() {
        return Ok(());
    }
    let (paths, _config, workspace, _project_id) = active_context_in(config_dir, data_dir)?;
    let used: Vec<String> = shot.items.iter().map(|item| item.id.clone()).collect();
    if used.is_empty() {
        return Ok(());
    }
    let conn = index::open(&paths.index_db())?;
    reinforce_records(&conn, &paths, workspace.as_deref(), &used, &shot.at)?;
    Ok(())
}

/// 把模型返回的那段 JSON 落地。丢敏感、合并重复、低置信进 candidate。
/// `provenance` 说清这批候选是谁提出来的、从哪次对话里长出来的：
/// 自动提取带对话出处，蒸馏只带主体（它没有对话可指）
pub fn land_extraction(
    app: &AppHandle,
    raw: &str,
    provenance: &extract::Provenance,
) -> Result<Accepted, String> {
    let records = extract::parse_candidates(raw);
    if records.is_empty() {
        return Ok(Accepted::default());
    }
    let (paths, config, workspace, project_id) = active_context(app)?;
    // 提取、蒸馏、反思三条路都汇到这一句之前落笔，所以 `memory.write` 只在这里问一次。
    // 它们是后台生产的，`attended` = 假：那一行写「要有人点头」就等于不再自动记东西
    if let Err(reason) = memory_gate(
        app,
        &paths,
        crate::policy::MemoryMode::Write,
        false,
        provenance.actor,
    ) {
        return Err(reason);
    }
    let conn = index::open(&paths.index_db())?;
    extract::accept(
        &conn,
        &paths,
        workspace.as_deref(),
        &config,
        project_id.as_deref(),
        &records,
        provenance,
    )
}

/// 始终注入区本身。分段占比要算上它——它每轮都在，只是不依赖检索
pub fn standing_body(app: &AppHandle) -> Option<String> {
    let (paths, config, _, _) = active_context(app).ok()?;
    if !config.enabled {
        return None;
    }
    let text = standing_text(&paths);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(inject::clip_to_budget(text, config.always_budget_tokens))
}

/// 这场对话最近一次的注入明细。`/memory why` 读它
#[tauri::command]
pub fn memory_why(app: AppHandle, conversation_id: String) -> Result<Option<Injection>, String> {
    let paths = Paths::app(&app)?;
    Ok(inject::why_of(&paths, &conversation_id))
}


/// 每轮结束后由前端调一次：把最近的对话交给服务商，问它有没有值得长期记住的事。
/// 走的是应用已经配好的推理服务商（跟生成标题同一条路），不额外上传任何记忆文件。
/// 放在命令层而不是塞在 run_turn 后面，是为了让提取慢或服务商挂都不拖累那一轮的回复。
/// 必须是 async + spawn_blocking：`complete_once` 是同步阻塞的，而 Tauri 2 里
/// 非 async 命令跑在主线程上，那会让整扇窗口白屏几秒
///
/// `conversation_id` 与 `entry_ids` 是出处，不是可选装饰：一条说不出来自哪次对话的
/// 自动记忆，事后无从追问、也无从整批撤销。缺了就报错，不静默记成"没出处"
/// `conversation_id` 是出处，不是可选装饰：一条说不出来自哪次对话的自动记忆，
/// 事后无从追问、也无从整批撤销。缺了就报错，不静默记成"没出处"。
///
/// 日志行的 id 那一格**不再由调用方交**：客户端不知道行 id（那是落盘时这边给的），
/// 以前它必须交，于是前端那一句 `invoke("memory_extract", { messages })` 一直缺两个必填参数，
/// 每一次自动提取都在运行时撞墙、被 `.catch` 吞成一条 toast——P0 那句"每条记忆答得出
/// 它来自哪次对话"从来没有成立过。现在这一格由后端按话题现读（[`tail_ids`]）
#[tauri::command]
pub async fn memory_extract(
    app: AppHandle,
    conversation_id: String,
    messages: Vec<crate::chat::ChatMessage>,
) -> Result<ExtractSummary, String> {
    tauri::async_runtime::spawn_blocking(move || extract_now(&app, &conversation_id, messages))
        .await
        .map_err(|e| format!("提取线程没跑完：{e}"))?
}

/// 这份话题日志的最后几行的 id。发送方只说"哪次对话"，不说"哪几行"：
/// 两边各数一遍就会不一致，而客户端本来也数不出行 id
pub fn tail_ids(conversation: &crate::history::Conversation, how_many: usize) -> Vec<String> {
    let rows = &conversation.messages;
    let from = rows.len().saturating_sub(how_many);
    rows[from..].iter().map(|row| row.id.clone()).collect()
}

fn extract_now(
    app: &AppHandle,
    conversation_id: &str,
    messages: Vec<crate::chat::ChatMessage>,
) -> Result<ExtractSummary, String> {
    let (_paths, config, _, _) = active_context(app)?;
    if !config.enabled || !config.auto_extract {
        return Ok(ExtractSummary::default());
    }
    // 出处里的行 id 从日志现读。读不到话题（还没落过盘）就是空的一串：
    // 那条记忆仍然认得自己来自哪次对话，只是指不出具体几行
    let entries = crate::history::load_current(app, conversation_id.trim())
        .map(|conversation| tail_ids(&conversation, messages.len()))
        .unwrap_or_default();
    let origin = Origin {
        conversation_id: conversation_id.trim().to_string(),
        entries,
        extracted_at: now_rfc3339(),
    };
    // 先验出处再花钱：漏传对话 id 是调用方的 bug，不该让它跑完一整次服务商才发现
    origin.check()?;
    let provenance = extract::Provenance {
        actor: crate::audit::Actor::Model,
        origin: Some(origin),
        must_stay_candidate: false,
    };
    let serde_json::Value::Array(rows) = serde_json::to_value(messages).map_err(|e| e.to_string())? else {
        return Ok(ExtractSummary::default());
    };
    let transcript = extract::transcript_of(&rows, 8, 600);
    if transcript.trim().is_empty() {
        return Ok(ExtractSummary::default());
    }
    let raw = crate::chat::complete_once(
        app,
        &config::load(app),
        serde_json::json!([{ "role": "user", "content": extract::prompt_for(&transcript) }]),
        "memory",
    )?;
    let report = land_extraction(app, &raw, &provenance)?;
    Ok(ExtractSummary {
        stored: report.stored.len(),
        merged: report.merged,
        candidates: report.candidates,
        dropped: report.dropped,
    })
}

/// 30 天蒸馏：把到龄的每日日志交给服务商，问它哪些值得转成长期记忆。
/// 同样是 async——这一步比提取更容易慢，日志攒一个月可能有几十篇
#[tauri::command]
pub async fn memory_distill(app: AppHandle) -> Result<DistillSummary, String> {
    tauri::async_runtime::spawn_blocking(move || distill_now(&app))
        .await
        .map_err(|e| format!("蒸馏线程没跑完：{e}"))?
}

/// 蒸馏的编排全在这里，一次 LLM 往返都不进 govern.rs：那部分只要一个 raw 字符串，
/// 就能被测试用假输出跑完（真服务商挂不挂，跟"日志有没有被吃掉"这件事无关）
fn distill_now(app: &AppHandle) -> Result<DistillSummary, String> {
    let (paths, config, workspace, project_id) = active_context(app)?;
    if !config.enabled {
        return Err("记忆功能是关的。先在设置里打开。".into());
    }
    let conn = index::open(&paths.index_db())?;
    sync_all(&conn, &paths, workspace.as_deref(), project_id.as_deref())?;
    let expired = govern::maintain(&conn, &paths, workspace.as_deref(), &config)?;
    let batch = govern::gather(&conn, &paths, workspace.as_deref(), project_id.as_deref(), &config)?;
    let mut summary = if batch.material.trim().is_empty() {
        // 没有到龄的日志、也没有待判定的临时记忆：不花钱问服务商，这也不算失败
        DistillSummary::default()
    } else {
        let raw = crate::chat::complete_once(
            app,
            &config::load(app),
            serde_json::json!([{ "role": "user", "content": govern::prompt_for(&batch.material) }]),
            "memory",
        )?;
        // 上面这一步报错就直接往外抛：日志还留在 daily/ 里，一次失败的蒸馏吃不掉任何历史
        govern::land(
            &conn,
            &paths,
            workspace.as_deref(),
            &config,
            project_id.as_deref(),
            &raw,
            &batch,
        )?
    };
    summary.archived_records = expired.archived.len();
    // 读不了的文件在这里如实报错，而不是被当成"没超限"：静默吞掉解析错误，
    // 超限告警就消失了，UI 会永远以为一切安好
    summary.overbudget =
        !govern::over_budget(&paths, workspace.as_deref(), project_id.as_deref(), &config)?.is_empty();
    Ok(summary)
}

/// 蒸馏前给用户看的那一页：会喂进服务商哪些日志、各多少字。
/// 纯读——"先看看"绝不能变成"先斩后奏"，任何搬运都只发生在确认后的蒸馏里
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DistillPreviewLog {
    pub name: String,
    pub chars: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DistillPreview {
    pub logs: Vec<DistillPreviewLog>,
    pub material_chars: usize,
}

#[tauri::command]
pub fn memory_distill_preview(app: AppHandle) -> Result<DistillPreview, String> {
    let (paths, config, workspace, project_id) = active_context(&app)?;
    if !config.enabled {
        return Err("记忆功能是关的。先在设置里打开。".into());
    }
    let conn = index::open(&paths.index_db())?;
    sync_all(&conn, &paths, workspace.as_deref(), project_id.as_deref())?;
    let batch = govern::gather(&conn, &paths, workspace.as_deref(), project_id.as_deref(), &config)?;
    let logs = batch
        .logs
        .iter()
        .map(|file| DistillPreviewLog {
            name: file
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("未知日期")
                .to_string(),
            chars: read_text(file).chars().count(),
        })
        .collect();
    Ok(DistillPreview { logs, material_chars: batch.material.chars().count() })
}

/// 把在用的记忆渲染成 AGENTS.md 的一节。Codex/Qoder 的分工哲学是：稳定规则
/// 归仓库里随 git 走的文件，记忆只是回忆层——这一条就是两层之间的桥：用户审完
/// 这段文字，自己粘进仓库的 AGENTS.md，一条记忆就算"毕业"成了正式约定。
/// 只有 active 且 public 的条目有资格出场；private/secret 在哪儿都不出门
pub fn render_agents_md(views: &[MemoryView], stamp: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for view in views {
        let record = &view.record;
        if record.status != MemoryStatus::Active
            || record.sensitivity != record::MemorySensitivity::Public
        {
            continue;
        }
        // 正文压成一行：AGENTS.md 是给人审的约定清单，不是记忆库的镜像
        let one_line: String = record.content.chars().take(120).collect();
        lines.push(format!("- [{}] {}", record.kind.as_str(), one_line));
    }
    if lines.is_empty() {
        return String::new();
    }
    format!(
        "## 记忆（aglab 导出于 {stamp}）\n\n以下来自 aglab 本地记忆库的在用条目，审阅后请改写成正式约定：\n\n{}\n",
        lines.join("\n")
    )
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentsExport {
    pub markdown: String,
    pub count: usize,
}

#[tauri::command]
pub fn memory_export_agents_md(app: AppHandle) -> Result<AgentsExport, String> {
    with_index(&app, |conn, _paths, _config, _workspace, _project_id| {
        let views = list_all(conn)?;
        let stamp = chrono::Local::now().format("%Y-%m-%d").to_string();
        let markdown = render_agents_md(&views, &stamp);
        let count = markdown.lines().filter(|line| line.starts_with("- [")).count();
        Ok(AgentsExport { markdown, count })
    })
}

/// 反思一次（P2，默认关）。它和提取、蒸馏共用 `extract::accept` 那一道闸门，
/// 区别只在来路：产物是推断，所以只能落在候选区，等谁点头
#[tauri::command]
pub async fn memory_reflect(app: AppHandle) -> Result<ExtractSummary, String> {
    tauri::async_runtime::spawn_blocking(move || reflect_now(&app))
        .await
        .map_err(|e| format!("反思线程没跑完：{e}"))?
}

fn reflect_now(app: &AppHandle) -> Result<ExtractSummary, String> {
    let (paths, config, workspace, project_id) = active_context(app)?;
    if !config.enabled {
        return Err("记忆功能是关的。先在设置里打开。".into());
    }
    if !reflect::should_ask(&config) {
        // 关着就是不花钱：一次服务商都不发，回一个空的汇总而不是报错
        return Ok(ExtractSummary::default());
    }
    let conn = index::open(&paths.index_db())?;
    sync_all(&conn, &paths, workspace.as_deref(), project_id.as_deref())?;
    let material = reflect::material_of(&paths, &conn, workspace.as_deref())?;
    let raw = crate::chat::complete_once(
        app,
        &config::load(app),
        serde_json::json!([{ "role": "user", "content": reflect::prompt_for(&material) }]),
        "reflect",
    )?;
    let report = reflect::land(
        &conn,
        &paths,
        workspace.as_deref(),
        &config,
        project_id.as_deref(),
        &raw,
    )?;
    Ok(ExtractSummary {
        stored: report.stored.len(),
        merged: report.merged,
        candidates: report.candidates,
        dropped: report.dropped,
    })
}

/// 一次批量落地的计数：提取与反思形状一样，就一份，不复制第二个类型
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtractSummary {
    pub stored: usize,
    pub merged: usize,
    pub candidates: usize,
    pub dropped: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{remove_tree, temp_dir};

    /// 四个动作各问一次权限表。这一条量的是**调用点在不在**，不是"它真的挡住了"——
    /// 后者要 `AppHandle`，判据本身由 `policy.rs` 里 `memory_acts` 那三条测试钉住。
    /// 三道闸此前只在纸上：错键名、云同步、越界半衰期。每条都要一个正对照，
    /// 否则"全都返回 Err"也能把这条测试喂绿
    #[test]
    fn the_memory_config_patch_refuses_what_it_promises_and_takes_the_rest() {
        let base = MemoryConfig::default();

        // 闸一：键名写错不能被静默丢掉，而且要把认错的那个键说给他听
        let err = merge_config_patch(&base, &serde_json::json!({"autoInjectt": false}))
            .expect_err("打错一个字母的开关不能算拨过了");
        assert!(err.contains("autoInjectt"), "要报出认错的那个键：{err}");

        // 闸二：补丁不是对象就得说清楚，不能"什么也没改但返回 Ok"
        for not_object in [
            serde_json::json!(null),
            serde_json::json!("autoInject=false"),
            serde_json::json!([{"autoInject": false}]),
            serde_json::json!(1),
        ] {
            let err = merge_config_patch(&base, &not_object)
                .expect_err("非对象补丁要的是拒绝，不是静默成功");
            assert!(err.contains("对象"), "要说的是形状不对：{err}");
        }

        // 闸三：云同步必须关着，且这条得写在判据里而不是只写在注释里
        let err = merge_config_patch(&base, &serde_json::json!({"cloudSync": true}))
            .expect_err("云同步没有实现，开关就不能拨得动");
        assert!(err.contains("云同步"), "要给他一句中文原因：{err}");

        // 闸四：半衰期两端都收，且收的是"合完之后的值"而不是"这次给的那一个"
        for half_life in [0.0f64, -1.0, 3651.0] {
            assert!(
                merge_config_patch(&base, &serde_json::json!({"decayHalfLifeDays": half_life}))
                    .is_err(),
                "半衰期 {half_life} 天不该过"
            );
        }

        // 正对照：合法的单键补丁要真的合上，其余字段保持原样
        let next = merge_config_patch(&base, &serde_json::json!({"autoInject": false}))
            .expect("合法的补丁要合得上");
        assert!(!next.auto_inject);
        assert_eq!(next.enabled, base.enabled, "没点名的开关不该被顺手改掉");
        assert!(!next.cloud_sync, "默认值不该把自己写成开着");
        // 嵌套那一层靠 serde 报缺字段，不是静默重置：半份权重要能被拒绝
        assert!(
            merge_config_patch(&base, &serde_json::json!({"weights": {"semantic": 0.9}})).is_err(),
            "只给一个权重不能把其余四个悄悄换回默认值"
        );
        let full = serde_json::json!({"weights": {"semantic": 0.9, "importance": 0.2,
            "freshness": 0.15, "scope": 0.1, "usage": 0.05}});
        assert_eq!(
            merge_config_patch(&base, &full).expect("整份权重该合得上").weights.semantic,
            0.9
        );
    }

    /// 上一那条测的是纯函数自己守不守得住。**命令有没有去问它**是另一件事：
    /// 测试本身就是 `merge_config_patch` 的调用方，所以把 `memory_config_set` 里那一行删掉，
    /// 编译器一声不响、上面那四条断言一条不红——"库层成立≠链路成立"（`check_price` 那一族同一次抓出来的）
    #[test]
    fn the_memory_config_command_asks_its_own_merge_gate() {
        let source = include_str!("mod.rs");
        // 针脚一律 concat! 拼：这条测试自己就在被搜的那份文件里，写成一整串会数到自己
        let command = source
            .split(concat!("pub fn memory", "_config_set"))
            .nth(1)
            .expect("命令得在")
            .split("\n}")
            .next()
            .unwrap_or_default();
        assert!(
            command.contains(concat!("merge_config_patch(&load_config", "(&paths), &patch)?")),
            "记忆配置那条命令没去问判据：{command}"
        );
        assert!(
            command.contains("save_config(&paths, &next)?"),
            "合完不写盘，返回给界面的那份就是盘上没有的：{command}"
        );
        assert_eq!(
            source.matches(concat!("fn merge_config_", "patch(")).count(),
            1,
            "合补丁的规则只许有一份"
        );
        assert_eq!(
            source.matches(concat!("if next.", "cloud_sync {")).count(),
            1,
            "那条禁止事项只许有一处执行点，抄第二份就会有两份答案"
        );
    }

    /// 只数 `#[cfg(test)]` 之前的部分，所以这条测试不会把它想证明的东西喂给自己
    #[test]
    fn every_memory_action_asks_the_policy_table() {
        let production =
            include_str!("mod.rs").split("#[cfg(test)]").next().unwrap_or_default();
        for action in ["Read", "Write", "Export", "Wipe"] {
            let needle = format!("crate::policy::MemoryMode::{action}");
            assert!(production.contains(&needle), "表上 {needle} 那一行没有执行者：这一族动作里没人问过它");
        }
        // 判据只许有一份：四行表问出四个答案，就等于没有答案
        assert_eq!(
            production.matches("fn memory_gate(").count(),
            1,
            "长第二份判断就等于给四行表两个答案"
        );
    }

    /// 出处里"哪几行"是从话题日志现读的：客户端交不出行 id（那是落盘时这边给的）。
    /// 以前这个参数由前端填，而它从来没填过——于是每次自动提取都在缺参数上撞墙
    #[test]
    fn the_provenance_rows_are_the_tail_of_that_conversation() {
        let rows = |n: usize| -> crate::history::Conversation {
            crate::history::Conversation {
                id: "conv-1".into(),
                messages: (0..n)
                    .map(|index| crate::history::MessageRecord {
                        id: format!("e{index}"),
                        role: "user".into(),
                        content: "一句话".into(),
                        created_at: 0,
                        reasoning: None,
                        tool_calls: Vec::new(),
                        steps: Vec::new(),
                        error: None,
                        attachments: Vec::new(),
                        parent_id: None,
                        model: None,
                        entry_ids: Vec::new(),
                        goal_round: None,
                        node_id: None,
                        media: None,
                    })
                    .collect(),
                ..Default::default()
            }
        };
        assert_eq!(tail_ids(&rows(5), 2), vec!["e3", "e4"], "取最后两行，按原顺序");
        assert_eq!(tail_ids(&rows(5), 99), vec!["e0", "e1", "e2", "e3", "e4"], "比日志还长就给整份");
        assert_eq!(tail_ids(&rows(5), 0), Vec::<String>::new(), "要 0 行就给空");
        assert!(tail_ids(&rows(0), 3).is_empty(), "空日志不该凭空造出行 id");
    }

    /// IPC 的**入参**这一侧此前没有任何机器管过：`invoke("cmd", { … })` 的键是自由字符串，
    /// Rust 那边加一个必填参数、TS 这边一个字都不用改，编译与 `tsc` 全绿，
    /// 坏在运行时那一句"缺参数"上。这里把 `memory_*` 这几条钉住，其余命令按同一形状扩
    #[test]
    fn every_memory_command_gets_the_arguments_it_asks_for() {
        let commands = include_str!("mod.rs");
        let calls = include_str!("../../../src/lib/memory.ts");
        for name in [
            "memory_extract",
            "memory_why",
            "memory_distill",
            "memory_reflect",
            "memory_recall_hints",
            "memory_timeline",
            // 这三条以前是"注册了没人用"：P0 判据写着"两条冲突记忆在 UI 里看得见"，
            // 而整个前端搜不到它们的名字
            "memory_conflicts",
            "memory_conflict_resolve",
            "memory_source",
        ] {
            let signature = commands
                .split(&format!("fn {name}("))
                .nth(1)
                .unwrap_or_else(|| panic!("命令 {name} 不见了"))
                .split(") ->")
                .next()
                .expect("命令签名要有结尾");
            // 参数名 → camelCase：Tauri 那侧的入参键名就是这么来的
            let wanted: Vec<String> = signature
                .split(',')
                .filter_map(|param| param.split(':').next())
                .map(str::trim)
                .filter(|arg| !matches!(*arg, "app" | "hub" | "state" | "db"))
                .map(|arg| {
                    let mut parts = arg.split('_');
                    let head = parts.next().unwrap_or_default();
                    let rest: String = parts.map(|part| {
                        let mut chars = part.chars();
                        let upper = chars.next().map_or(String::new(), |c| c.to_uppercase().to_string());
                        upper + chars.as_str()
                    }).collect();
                    format!("{head}{rest}")
                })
                .collect();
            let call = calls
                .split(&format!("\"{name}\""))
                .nth(1)
                .unwrap_or_else(|| panic!("前端没人调 {name}：命令注册≠有人用"))
                .split(')')
                .next()
                .expect("那次 invoke 要有结尾");
            for key in wanted {
                assert!(
                    call.contains(&key) || call.contains(&format!("\"{key}\"")),
                    "`{name}` 要 `{key}`，而前端那一句只写了：{call}"
                );
            }
        }
    }
    use rusqlite::Connection;

    fn record(content: &str, scope: MemoryScope, project: Option<&str>) -> MemoryRecord {
        MemoryRecord {
            id: new_id(),
            kind: MemoryKind::Preference,
            scope,
            project_id: project.map(String::from),
            status: MemoryStatus::Active,
            importance: 4,
            confidence: 1.0,
            stability: Stability::Stable,
            source: MemorySource::User,
            sensitivity: record::MemorySensitivity::default(),
            created_at: "2026-09-25T10:00:00+08:00".into(),
            updated_at: "2026-09-25T10:00:00+08:00".into(),
            occurred_at: None,
            reinforced_at: None,
            origin: None,
            last_used_at: None,
            ttl_days: None,
            tags: vec!["沟通风格".into()],
            entities: Vec::new(),
            supersedes: Vec::new(),
            content: content.into(),
            extra: Vec::new(),
        }
    }

    /// 一条三年前的记忆：新鲜度必须接近 0，编辑复活它的时候才看得见
    fn ancient(content: &str) -> MemoryRecord {
        let mut item = record(content, MemoryScope::Global, None);
        item.created_at = "2023-01-01T10:00:00+08:00".into();
        item.updated_at = item.created_at.clone();
        item
    }

    /// `/memory why` 里新鲜度那一项的数字。判新鲜度就得读它——它和分数是同一处算出来的，
    /// 两处不一致本身就是 bug
    fn freshness_term(hit: &Hit) -> String {
        hit.why
            .split("· 新鲜 ")
            .nth(1)
            .unwrap_or("")
            .split(" ·")
            .next()
            .unwrap_or("")
            .split('×')
            .next_back()
            .unwrap_or("")
            .to_string()
    }

    /// 一个跑完整套读写的沙盒：目录 + 索引 + 已同步
    struct Harness {
        paths: Paths,
        conn: Connection,
        workspace: PathBuf,
    }

    fn harness() -> Harness {
        let root = temp_dir("memory-root");
        let workspace = temp_dir("memory-ws");
        let paths = Paths::new(root);
        ensure_layout(&paths).unwrap();
        let conn = index::open(&paths.index_db()).unwrap();
        Harness { paths, conn, workspace }
    }

    fn teardown(harness: &Harness) {
        remove_tree(&harness.paths.root);
        remove_tree(&harness.workspace);
    }

    /// 坏文件不再冻结作用域：读不了的真相源被隔离进 archive/corrupt-*，写入照常落笔，
    /// 之后一次 sync_all 把索引对齐到"只剩活下来的那条"。坏文件本体搬走不删——字节还找得回
    #[test]
    fn a_corrupt_truth_source_is_quarantined_and_writing_heals_it() {
        let h = harness();
        let broken = h.paths.global_memory();
        // 未闭合的 frontmatter 块：parse_records 对块外散文容忍（daily 日志有抬头），
        // 但块读到文件尾还没等到 `---` 就该报错——这是真实断电最可能留下的形状
        fs::write(
            &broken,
            "---\nid: 断掉的半截\nimportance: 3\n这是没有闭合的 frontmatter，读到文件尾也不见闭合线。",
        )
        .unwrap();

        let fresh = record("坏文件之后新写的一条。", MemoryScope::Global, None);
        append_record(&h.conn, &h.paths, None, &fresh).unwrap();

        let healed = fs::read_to_string(&broken).unwrap();
        assert!(healed.contains("坏文件之后新写的一条。"), "写入要照常落笔：{healed}");
        assert!(!healed.contains("断掉的半截"), "坏内容不该还留在原文件里");

        let mut quarantined = Vec::new();
        let archive = h.paths.root.join(ARCHIVE_DIR);
        for entry in fs::read_dir(&archive).unwrap().flatten() {
            if entry.file_name().to_string_lossy().starts_with("corrupt-") {
                for inner in fs::read_dir(entry.path()).unwrap().flatten() {
                    quarantined.push(inner.path());
                }
            }
        }
        assert_eq!(quarantined.len(), 1, "坏文件要被搬进 corrupt-* 里一份");
        assert!(
            fs::read_to_string(&quarantined[0]).unwrap().contains("断掉的半截"),
            "隔离的是字节，不是抹掉"
        );
        // 索引自愈：sync_all 不再报错，检索里只有活下来的那条
        sync_all(&h.conn, &h.paths, None, None).unwrap();
        assert_eq!(list_all(&h.conn).unwrap().len(), 1);
        teardown(&h);
    }

    /// 原子写的落地证据：写完不留临时文件，真相源随时是完整可解析的——
    /// 断电最坏留下一个 .tmp.*，而不是半截 MEMORY.md
    #[test]
    fn writing_leaves_no_tmp_files_behind() {
        let h = harness();
        for index in 0..3 {
            let item = record(&format!("第 {index} 条原子写样本。"), MemoryScope::Global, None);
            append_record(&h.conn, &h.paths, None, &item).unwrap();
        }
        let leftovers: Vec<String> = fs::read_dir(&h.paths.root)
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(leftovers.is_empty(), "临时文件该被改名顶替掉：{leftovers:?}");
        parse_records(&read_text(&h.paths.global_memory())).expect("写完的文件必须是完整可解析的");
        teardown(&h);
    }

    /// 兜底不再盲删第一名：对不上的整句要报出最像的几条让人点名，删错记忆不可逆
    #[test]
    fn a_fuzzy_forget_asks_for_the_id_instead_of_deleting_the_top_hit() {
        let candidates = vec![
            ("mem-1".to_string(), "global/MEMORY.md".to_string(), "部署流水线要用 pnpm，不要用 npm。".to_string()),
            ("mem-2".to_string(), "global/MEMORY.md".to_string(), "测试库在 CI 里每次重建。".to_string()),
        ];

        // 整句包含：直接删，原行为不变
        let (id, path) = pick_forget_target(&candidates, "pnpm").unwrap();
        assert_eq!((id.as_str(), path.as_str()), ("mem-1", "global/MEMORY.md"));

        // 对不上：报错点名候选，而不是顺手删掉排第一的那条
        let error = pick_forget_target(&candidates, "数据库迁移").unwrap_err();
        assert!(error.contains("mem-1"), "最像的要报出来：{error}");
        assert!(error.contains("带上它的 id"), "要告诉用户下一步怎么点名：{error}");

        // 空候选：如实说没有
        let error = pick_forget_target(&[], "随便什么").unwrap_err();
        assert!(error.contains("没找到"), "{error}");
    }

    #[test]
    fn round_trips_a_record_through_markdown() {
        let h = harness();
        let original = record("回答先给结论，再给理由。", MemoryScope::Global, None);
        append_record(&h.conn, &h.paths, None, &original).unwrap();

        let text = fs::read_to_string(h.paths.global_memory()).unwrap();
        let parsed = parse_records(&text).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0], original, "写出去再读回来必须一模一样");

        teardown(&h);
    }

    #[test]
    fn keeps_unknown_frontmatter_fields_on_round_trip() {
        let h = harness();
        let mut original = record("带自定义字段的记忆。", MemoryScope::Global, None);
        original.extra = vec![("owner".into(), "me".into())];
        append_record(&h.conn, &h.paths, None, &original).unwrap();

        let parsed = parse_records(&fs::read_to_string(h.paths.global_memory()).unwrap()).unwrap();
        assert_eq!(parsed[0].extra, vec![("owner".into(), "me".into())]);

        teardown(&h);
    }

    #[test]
    fn indexes_several_records_in_one_file() {
        let h = harness();
        for content in ["第一条偏好", "第二条偏好", "第三条偏好"] {
            append_record(&h.conn, &h.paths, None, &record(content, MemoryScope::Global, None)).unwrap();
        }
        let parsed = parse_records(&fs::read_to_string(h.paths.global_memory()).unwrap()).unwrap();
        assert_eq!(parsed.len(), 3);

        let hits = search(&h.conn, &MemoryConfig::default(), "偏好", None).unwrap();
        assert_eq!(hits.len(), 3, "中文查询要能在 FTS 里命中");

        teardown(&h);
    }

    #[test]
    fn chinese_substring_queries_match_without_word_segmentation() {
        // 默认 unicode61 不分中文词。这条测试就是那个坑的守门人
        let h = harness();
        append_record(
            &h.conn,
            &h.paths,
            None,
            &record("用户偏好结论先行，不要长篇铺垫。", MemoryScope::Global, None),
        )
        .unwrap();

        for query in ["结论", "先行", "铺垫", "偏好"] {
            let hits = search(&h.conn, &MemoryConfig::default(), query, None).unwrap();
            assert!(!hits.is_empty(), "搜「{query}」应该命中，实际一条都没有");
        }

        teardown(&h);
    }

    #[test]
    fn project_memory_does_not_leak_into_another_project() {
        let h = harness();
        let mut mine = record("这个项目用 pnpm，不要换成 npm。", MemoryScope::Project, Some("proj-a"));
        mine.kind = MemoryKind::Fact;
        append_record(&h.conn, &h.paths, None, &mine).unwrap();
        append_record(&h.conn, &h.paths, None, &record("全局偏好：中文回答。", MemoryScope::Global, None)).unwrap();

        let in_a = search(&h.conn, &MemoryConfig::default(), "pnpm", Some("proj-a")).unwrap();
        let in_a = keep_relevant(in_a, Some("proj-a"));
        assert_eq!(in_a.len(), 1, "项目 A 里应该看得到那条项目记忆");

        let in_b = keep_relevant(
            search(&h.conn, &MemoryConfig::default(), "pnpm", Some("proj-b")).unwrap(),
            Some("proj-b"),
        );
        assert!(in_b.is_empty(), "项目 A 的记忆漏进了项目 B");

        // 项目作用域漏了 id 就是坏数据，写的时候就得挡住
        let broken = record("没有 id 的项目记忆", MemoryScope::Project, None);
        assert!(append_record(&h.conn, &h.paths, None, &broken).is_err());

        teardown(&h);
    }

    #[test]
    fn forget_removes_from_both_file_and_index() {
        let h = harness();
        let kept = record("要留下的", MemoryScope::Global, None);
        let gone = record("用户提过他喜欢用 pnpm", MemoryScope::Global, None);
        append_record(&h.conn, &h.paths, None, &kept).unwrap();
        append_record(&h.conn, &h.paths, None, &gone).unwrap();

        let message = {
            let needle = "pnpm";
            let hits = keep_relevant(search(&h.conn, &MemoryConfig::default(), needle, None).unwrap(), None);
            let target = hits.first().expect("该找到那条 pnpm 记忆");
            let file = resolve_file(&h.paths, None, &target.path);
            let mut records = parse_records(&fs::read_to_string(&file).unwrap()).unwrap();
            records.retain(|record| record.id != target.id);
            rewrite_file(&h.conn, &h.paths, &file, &records).unwrap();
            index::forget(&h.conn, &target.id).unwrap();
            format!("已忘记 {}", target.id)
        };
        assert!(message.starts_with("已忘记"));

        let parsed = parse_records(&fs::read_to_string(h.paths.global_memory()).unwrap()).unwrap();
        assert_eq!(parsed.len(), 1, "文件里也该少一条");
        assert_eq!(parsed[0].id, kept.id);
        assert!(search(&h.conn, &MemoryConfig::default(), "pnpm", None).unwrap().is_empty());

        teardown(&h);
    }

    #[test]
    fn hand_edited_files_are_picked_up_by_a_resync() {
        let h = harness();
        append_record(&h.conn, &h.paths, None, &record("用户写下的第一条", MemoryScope::Global, None)).unwrap();
        append_record(&h.conn, &h.paths, None, &record("用户会手动删掉的那条", MemoryScope::Global, None)).unwrap();

        // 模拟用户用编辑器打开文件：改一条、删一条、再加一条
        let mut text = fs::read_to_string(h.paths.global_memory()).unwrap();
        text = text.replace("用户写下的第一条", "用户改过了的第一条");
        let records = parse_records(&text).unwrap();
        let survivor = records[0].clone();
        let mut rewritten = vec![survivor];
        let mut extra = record("用户手动新增的一条", MemoryScope::Global, None);
        extra.source = MemorySource::Import;
        rewritten.push(extra);
        fs::write(h.paths.global_memory(), render_records(&rewritten)).unwrap();

        sync_all(&h.conn, &h.paths, None, None).unwrap();

        let listed = list_all(&h.conn).unwrap();
        assert_eq!(listed.len(), 2, "手改后重建索引应该反映文件现状");
        assert!(listed.iter().any(|item| item.record.content.contains("改过了")));
        assert!(!listed.iter().any(|item| item.record.content.contains("手动删掉")));

        teardown(&h);
    }

    #[test]
    fn deleting_a_file_forgets_everything_it_held() {
        let h = harness();
        append_record(&h.conn, &h.paths, None, &record("只在文件存在时可搜", MemoryScope::Global, None)).unwrap();
        assert_eq!(list_all(&h.conn).unwrap().len(), 1);

        fs::remove_file(h.paths.global_memory()).unwrap();
        sync_all(&h.conn, &h.paths, None, None).unwrap();

        assert!(list_all(&h.conn).unwrap().is_empty(), "文件没了，索引里不该还留着");
        teardown(&h);
    }

    #[test]
    fn refuses_sensitive_content_and_says_why() {
        let h = harness();
        for (content, hint) in [
            ("我的密码是 hunter2!", "凭据"),
            ("api_key: sk-1234567890abcdef", "凭据"),
            ("卡号 6222021234567890123", "银行卡"),
            ("手机 13800138000 联系我", "手机号"),
        ] {
            let error = append_record(&h.conn, &h.paths, None, &record(content, MemoryScope::Global, None))
                .err()
                .unwrap_or_else(|| panic!("「{content}」本该被拒绝"));
            assert!(error.contains(hint), "拒绝理由该提到 {hint}，实际：{error}");
        }
        assert!(list_all(&h.conn).unwrap().is_empty(), "被拒的内容一个字都不该落盘");
        teardown(&h);
    }

    #[test]
    fn standing_text_skips_comment_boilerplate() {
        let h = harness();
        fs::write(h.paths.rules(), "# 硬规则\n<!-- 给用户的说明 -->\n永远先给结论。\n").unwrap();
        let text = standing_text(&h.paths);
        assert!(text.contains("永远先给结论"));
        assert!(!text.contains("给用户的说明"), "注释是写给人看的，不该进提示词");
        teardown(&h);
    }

    #[test]
    fn index_is_rebuildable_from_markdown_alone() {
        let Harness { paths, conn, workspace } = harness();
        for index in 0..5 {
            append_record(
                &conn,
                &paths,
                None,
                &record(&format!("第 {index} 条长期记忆"), MemoryScope::Global, None),
            )
            .unwrap();
        }
        let snapshot = fs::read_to_string(paths.global_memory()).unwrap();

        // 把索引整个丢掉，只留 Markdown
        drop(conn);
        remove_tree_file(&paths.index_db());
        let rebuilt = index::open(&paths.index_db()).unwrap();
        sync_all(&rebuilt, &paths, None, None).unwrap();

        assert_eq!(list_all(&rebuilt).unwrap().len(), 5);
        assert_eq!(fs::read_to_string(paths.global_memory()).unwrap(), snapshot, "重建不该动真相源");
        drop(rebuilt);
        remove_tree(&paths.root);
        remove_tree(&workspace);
    }

    fn remove_tree_file(path: &Path) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(format!("{}-wal", path.display()));
        let _ = fs::remove_file(format!("{}-shm", path.display()));
    }

    #[test]
    fn fts5_is_actually_compiled_in() {
        let root = temp_dir("memory-fts");
        let conn = index::open(&root.join("index.sqlite")).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM memories_fts WHERE memories_fts MATCH ?1", rusqlite::params!["测试"], |_| Ok(0))
            .unwrap_or(-1);
        assert_eq!(count, 0, "FTS5 查询能跑通才说明 bundled SQLite 真的带了 FTS5");
        drop(conn);
        remove_tree(&root);
    }

    #[test]
    fn validation_rejects_out_of_range_fields() {
        let mut bad = record("内容", MemoryScope::Global, None);
        bad.importance = 9;
        assert!(bad.validate().is_err());
        let mut bad = record("内容", MemoryScope::Global, None);
        bad.confidence = 1.7;
        assert!(bad.validate().is_err());
        let mut bad = record("   ", MemoryScope::Global, None);
        bad.content = "   ".into();
        assert!(bad.validate().is_err());
    }

    #[test]
    fn unknown_enum_values_are_reported_not_swallowed() {
        let text = "---\nid: x\ntype: vibes\nscope: global\nstatus: active\nimportance: 3\nconfidence: 0.5\nstability: stable\nsource: user\ncreated_at: 2026-01-01T00:00:00+08:00\nupdated_at: 2026-01-01T00:00:00+08:00\ntags: []\nsupersedes: []\n---\n\n内容\n";
        let error = parse_records(text).err().expect("未知类型必须报错");
        assert!(error.contains("vibes"), "报错里要带上那个写错的取值：{error}");
    }

    #[test]
    fn injection_line_carries_source_and_date() {
        let mut item = record("用户偏好结论先行。", MemoryScope::Global, None);
        item.confidence = 0.92;
        let line = item.injection_line();
        assert!(line.contains("来源: global"));
        assert!(line.contains("更新: 2026-09-25"));
        assert!(line.contains("置信: 0.92"));
        assert!(line.contains("用户偏好结论先行"));
    }

    #[test]
    fn segmenter_splits_cjk_and_keeps_ascii_words() {
        let text = index::segment_for_index("用 pnpm 管理依赖");
        assert!(text.contains("pnpm"), "ASCII 词该整词留着");
        assert!(text.contains('用'));
        assert!(text.contains("管理"), "二元组该在");
        assert!(
            index::build_match_query("管理依赖").contains("\"管理\" OR"),
            "查询侧每个 token 要加引号再 OR 起来：{}",
            index::build_match_query("管理依赖")
        );
    }

    /// 批量导入必须是"一个文件读写一次"，不是"每条把整个文件重抄一遍"。
    /// 退化的代价不是慢一点，而是"换机器恢复备份"这件事根本跑不完：逐条写的时候
    /// 250 条在这台机器上 10 分钟没结束，批量+单事务之后 1000 条 release 态 0.67s
    #[test]
    #[ignore = "1000 条批量导入基准，只在专门测性能时跑"]
    fn bulk_import_stays_linear_at_1000_records() {
        use std::time::{Duration, Instant};
        let h = harness();
        let records: Vec<MemoryRecord> = (0..1000)
            .map(|index| {
                record(
                    &format!("第 {index} 条偏好：回答先给结论，再给理由，别绕。"),
                    MemoryScope::Global,
                    None,
                )
            })
            .collect();
        let text = serde_json::to_string(&ExportBundle {
            version: EXPORT_VERSION,
            exported_at: now_rfc3339(),
            records,
        })
        .unwrap();

        let started = Instant::now();
        assert_eq!(import_bundle(&h.conn, &h.paths, &text).unwrap(), 1000);
        let elapsed = started.elapsed();
        // 门槛给到实测的 5~7 倍余量：它要挡的是"退化成逐条全量重写"那种数量级的
        // 崩塌，不是机器今天忙不忙。发布态那一档才是对外承诺的形状
        let target = if cfg!(debug_assertions) {
            Duration::from_secs(30)
        } else {
            Duration::from_secs(5)
        };
        println!("1000 条导入用时 {elapsed:?}（目标 {target:?}）");
        assert!(elapsed < target, "批量导入退化成了逐条全量重写：{elapsed:?}");

        // 快而没真的落盘不算过：文件里 1000 条，索引里也 1000 条
        let on_disk = parse_records(&fs::read_to_string(h.paths.global_memory()).unwrap()).unwrap();
        assert_eq!(on_disk.len(), 1000);
        assert_eq!(index::count(&h.conn, None).unwrap(), 1000);
        teardown(&h);
    }

    /// 导出→导入走一整趟，中间换一棵目录树：这就是"换一台机器"的最小模型
    #[test]
    fn export_round_trips_into_a_fresh_memory_root() {
        let source = harness();
        let global = record("回答先给结论，再给理由。", MemoryScope::Global, None);
        let project = record("这个仓库用 pnpm 管理依赖。", MemoryScope::Project, Some("proj-a"));
        append_record(&source.conn, &source.paths, None, &global).unwrap();
        append_record(&source.conn, &source.paths, Some(&source.workspace), &project).unwrap();
        let text = bundle_of(&source.conn).unwrap();

        let target = Paths::new(temp_dir("memory-target"));
        ensure_layout(&target).unwrap();
        let conn = index::open(&target.index_db()).unwrap();
        assert_eq!(import_bundle(&conn, &target, &text).unwrap(), 2);

        // 项目作用域那条要落回它自己的 projects/<id>/，切到该项目才看得见
        sync_all(&conn, &target, None, Some("proj-a")).unwrap();
        let config = MemoryConfig::default();
        let own = keep_relevant(search(&conn, &config, "pnpm", Some("proj-a")).unwrap(), Some("proj-a"));
        assert!(
            own.iter().any(|hit| hit.id == project.id),
            "导入进来的项目记忆该找得回来：{own:?}"
        );
        // 同一份索引、换一个问题项目：检索层本身不按 project_id 过滤，
        // 拦住它的必须是 keep_relevant，所以这一步才真的在测隔离
        let other = keep_relevant(search(&conn, &config, "pnpm", Some("proj-b")).unwrap(), Some("proj-b"));
        assert!(
            !other.iter().any(|hit| hit.id == project.id),
            "项目 A 的记忆漏进了项目 B"
        );
        // 全局那条不受项目限制。换个问法查它：FTS 只认正文里真有的词，
        // 上面那句 "pnpm" 本来就匹配不到"结论先行"，拿它断言全局存在是测试写错了
        let as_other_project =
            keep_relevant(search(&conn, &config, "结论先行", Some("proj-b")).unwrap(), Some("proj-b"));
        assert!(
            as_other_project.iter().any(|hit| hit.id == global.id),
            "全局记忆在项目 B 里也该检索得到"
        );

        // 再导一次：同 id 跳过。重复导入把用户在这一台机器上手改的那份覆盖掉，
        // 是这台机器上的记忆被另一台的旧副本无声赢掉
        assert_eq!(import_bundle(&conn, &target, &text).unwrap(), 0);
        let on_disk = fs::read_to_string(target.global_memory()).unwrap();
        assert!(on_disk.contains(&global.id), "全局那条还该在文件里");

        teardown(&source);
        remove_tree(&target.root);
    }

    /// 信封在导入这一侧的三条规矩：没加密的原样通过（"关掉加密字节不变"在导入侧的对应物）、
    /// 加密了没给口令要说清缺什么、口令错了就是错了——**不许退回读明文**
    #[test]
    fn an_encrypted_bundle_never_falls_back_to_plaintext() {
        const PLAIN: &str = "{\"version\":3,\"records\":[]}";

        assert_eq!(open_payload(PLAIN, None).unwrap(), PLAIN);
        assert_eq!(
            open_payload(PLAIN, Some("随手填的")).unwrap(),
            PLAIN,
            "明文备份不该因为多填了一格口令就报错"
        );

        let armored = crate::envelope::encrypt(PLAIN.as_bytes(), "right one").unwrap();
        let missing = open_payload(&armored, None).expect_err("没口令该读不开加密备份");
        assert!(missing.contains("口令"), "拒绝要说清缺什么：{missing}");
        let wrong = open_payload(&armored, Some("wrong one")).expect_err("错口令该读不开");
        assert!(wrong.contains("口令不对"), "错口令的理由要写在脸上：{wrong}");
        assert_eq!(open_payload(&armored, Some("right one")).unwrap(), PLAIN);
    }

    #[test]
    fn import_refuses_a_bundle_from_an_unknown_version() {
        let h = harness();
        let text = "{\"version\":99,\"exportedAt\":\"2026-09-25T10:00:00+08:00\",\"records\":[]}";
        let error = import_bundle(&h.conn, &h.paths, text)
            .err()
            .expect("不认识的版本必须报错，不能猜着导");
        assert!(error.contains("99"), "报错里要带上那个版本号：{error}");
        assert!(import_bundle(&h.conn, &h.paths, "不是 JSON").is_err());
        teardown(&h);
    }

    /// 一键清空：索引整片清掉，但真相源要还能捡回来
    #[test]
    fn wipe_empties_the_index_and_keeps_the_markdown_recoverable() {
        let h = harness();
        append_record(&h.conn, &h.paths, None, &record("回答先给结论。", MemoryScope::Global, None)).unwrap();
        append_record(&h.conn, &h.paths, None, &record("这个项目用 pnpm。", MemoryScope::Project, Some("proj-a"))).unwrap();
        assert_eq!(wipe(&h.conn, &h.paths, None, None).unwrap(), 2);
        assert_eq!(index::count(&h.conn, None).unwrap(), 0, "清空之后索引里不该有行");
        assert!(!h.paths.global_memory().exists(), "MEMORY.md 不该还留在原位");

        let archived = fs::read_dir(h.paths.root.join(ARCHIVE_DIR))
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .find(|path| path.is_dir() && path.file_name().unwrap().to_string_lossy().starts_with("wiped-"))
            .expect("要留下一个 archive/wiped-* 目录");
        let moved: Vec<String> = fs::read_dir(&archived)
            .unwrap()
            .flatten()
            .map(|entry| entry.path().file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(
            moved.iter().any(|name| name.ends_with("global__MEMORY.md")),
            "全局那份 MEMORY.md 要在归档里，删错了才捡得回来：{moved:?}"
        );

        // 原地再同步一次：没有文件可索引，行也不该凭空回来
        sync_all(&h.conn, &h.paths, None, None).unwrap();
        assert_eq!(index::count(&h.conn, None).unwrap(), 0);
        teardown(&h);
    }

    /// 编辑一条记录：候选转正、就地改正文，改的都是那个 .md 本身
    #[test]
    fn edit_rewrites_the_marked_record_not_just_the_index_row() {
        let h = harness();
        let mut item = record("用户偏好结论先行。", MemoryScope::Global, None);
        item.status = MemoryStatus::Candidate;
        item.importance = 2;
        append_record(&h.conn, &h.paths, None, &item).unwrap();

        let view = edit_record(
            &h.conn,
            &h.paths,
            None,
            &item.id,
            &EditPatch {
                status: Some("active".into()),
                importance: Some(5),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(view.record.status, MemoryStatus::Active);
        assert_eq!(view.record.importance, 5);
        assert_eq!(view.record.content, "用户偏好结论先行。", "没给的字段不该被动过");

        let on_disk = parse_records(&fs::read_to_string(h.paths.global_memory()).unwrap()).unwrap();
        assert_eq!(on_disk.len(), 1);
        assert_eq!(on_disk[0], view.record, "文件里的那条要跟返回的一模一样");

        // 编辑同样是写入：把一条干净的记忆改成密码，不能因为走了 edit 就绕开闸门
        let sneaky = edit_record(
            &h.conn,
            &h.paths,
            None,
            &item.id,
            &EditPatch {
                content: Some("我的密码是hunter2abcdefgh".into()),
                ..Default::default()
            },
        );
        assert!(sneaky.is_err(), "敏感内容不该改得进去");
        assert!(index::count(&h.conn, Some(MemoryStatus::Active)).unwrap() >= 1);
        assert!(fs::read_to_string(h.paths.global_memory())
            .unwrap()
            .contains("结论先行"));
        teardown(&h);
    }

    /// 关掉记忆功能要关的是整件事：注入停了，自动提取那一笔也不能落
    #[test]
    fn turning_memory_off_stops_injection_and_writing_alike() {
        let h = harness();
        append_record(&h.conn, &h.paths, None, &record("回答先给结论。", MemoryScope::Global, None)).unwrap();
        let mut off = MemoryConfig::default();
        off.enabled = false;

        assert!(
            inject::build(&h.conn, &h.paths, &off, "回答", None, None).unwrap().is_none(),
            "关掉之后还在往上下文里塞记忆"
        );
        let report = extract::accept(
            &h.conn,
            &h.paths,
            None,
            &off,
            None,
            &[record("改用 bun。", MemoryScope::Global, None)],
            &extract::Provenance { actor: crate::audit::Actor::Model, origin: None, must_stay_candidate: false },
        )
        .unwrap();
        assert_eq!(report.stored.len(), 0, "关掉之后自动提取还是写进去了");
        assert!(!fs::read_to_string(h.paths.global_memory())
            .unwrap()
            .contains("改用 bun"));

        // 对照组：开着的时候这两件事确实会发生，否则上面那两个断言只是恒真
        let on = MemoryConfig::default();
        assert!(inject::build(&h.conn, &h.paths, &on, "回答", None, None).unwrap().is_some());
        extract::accept(
            &h.conn,
            &h.paths,
            None,
            &on,
            None,
            &[record("改用 bun。", MemoryScope::Global, None)],
            &extract::Provenance { actor: crate::audit::Actor::Model, origin: None, must_stay_candidate: false },
        )
        .unwrap();
        assert!(fs::read_to_string(h.paths.global_memory())
            .unwrap()
            .contains("改用 bun"));
        teardown(&h);
    }

    // ------------------------------------------------------------ P0：双时间新鲜度（T02）

    #[test]
    fn an_edit_leaves_the_freshness_of_a_memory_exactly_where_it_was() {
        let h = harness();
        let stale = ancient("用户偏好结论先行，不要长篇铺垫。");
        let id = stale.id.clone();
        append_record(&h.conn, &h.paths, None, &stale).unwrap();
        let before = search(&h.conn, &MemoryConfig::default(), "结论", None).unwrap();
        let before_fresh = freshness_term(&before[0]);
        assert_eq!(before_fresh, "0.00", "三年前的记忆本来就该是：{before_fresh}");

        // 只改重要性：正文没动，语义那一项不该变，于是两处对比能干净地落在新鲜度上
        edit_record(
            &h.conn,
            &h.paths,
            None,
            &id,
            &EditPatch { importance: Some(5), ..Default::default() },
        )
        .unwrap();
        let after = search(&h.conn, &MemoryConfig::default(), "结论", None).unwrap();
        assert_ne!(after[0].updated_at, stale.updated_at, "编辑确实把 updated_at 推到今天了");
        assert_eq!(
            freshness_term(&after[0]),
            before_fresh,
            "改一条记忆把它判成「刚发生」了——双时间白设了：{:?}",
            after[0].why
        );
        assert!(after[0].reinforced_at.is_none(), "编辑不该顺手盖强化章");
        teardown(&h);
    }

    #[test]
    fn being_injected_once_makes_a_memory_look_fresher_without_looking_edited() {
        let h = harness();
        let stale = ancient("用户偏好结论先行，不要长篇铺垫。");
        let id = stale.id.clone();
        append_record(&h.conn, &h.paths, None, &stale).unwrap();
        let before = freshness_term(&search(&h.conn, &MemoryConfig::default(), "结论", None).unwrap()[0]);

        let at = now_rfc3339();
        index::note_injection(&h.conn, std::slice::from_ref(&id), &at).unwrap();
        assert_eq!(reinforce_records(&h.conn, &h.paths, None, std::slice::from_ref(&id), &at).unwrap(), 1);

        let hits = search(&h.conn, &MemoryConfig::default(), "结论", None).unwrap();
        assert!(
            freshness_term(&hits[0]) > before,
            "用过一次都不算，那强化这一路是写给谁看的：{} → {}",
            before,
            freshness_term(&hits[0])
        );
        let on_disk = parse_records(&fs::read_to_string(h.paths.global_memory()).unwrap()).unwrap();
        assert_eq!(on_disk[0].updated_at, stale.updated_at, "被用上不是被编辑：写下的时间不许动");
        assert_eq!(on_disk[0].reinforced_at.as_deref(), Some(at.as_str()), "强化章要落在真相源里");
        teardown(&h);
    }

    /// 一天一次是这套设计的成本闸门：每轮都重写 MEMORY.md 会把跟着仓库走的
    /// `.ai-memory/` 刷成噪音，用户 diff 出来的该是"记住了什么"
    #[test]
    fn reinforcement_stamps_the_markdown_at_most_once_a_day() {
        let h = harness();
        let item = record("用户偏好结论先行。", MemoryScope::Global, None);
        let id = item.id.clone();
        append_record(&h.conn, &h.paths, None, &item).unwrap();
        let snapshot = fs::read_to_string(h.paths.global_memory()).unwrap();

        let morning = format!("{}T08:00:00+08:00", chrono::Local::now().format("%Y-%m-%d"));
        let ids = std::slice::from_ref(&id);
        assert_eq!(reinforce_records(&h.conn, &h.paths, None, ids, &morning).unwrap(), 1);
        assert_eq!(reinforce_records(&h.conn, &h.paths, None, ids, &morning).unwrap(), 0, "同一天第二次注入不该再写盘");
        let stamped = fs::read_to_string(h.paths.global_memory()).unwrap();
        assert!(stamped.contains(&format!("reinforced_at: {morning}")), "章要盖在文件里：{stamped}");
        let kept = |text: &str| -> String {
            text.lines().filter(|line| !line.starts_with("reinforced_at:")).collect::<Vec<_>>().join("\n")
        };
        assert_eq!(kept(&stamped), kept(&snapshot), "除了那一行时间戳，正文与其余字段一个字都不许变");

        // 隔天：换一个日期就该再盖一次
        let tomorrow = "2027-01-01T09:00:00+08:00".to_string();
        assert_eq!(reinforce_records(&h.conn, &h.paths, None, ids, &tomorrow).unwrap(), 1);
        // 时间只往前走：拿一个更早的"强化"去覆盖，等于把记忆判旧
        assert_eq!(reinforce_records(&h.conn, &h.paths, None, ids, &morning).unwrap(), 0);
        assert!(fs::read_to_string(h.paths.global_memory())
            .unwrap()
            .contains(&format!("reinforced_at: {tomorrow}")));
        teardown(&h);
    }

    #[test]
    fn an_edit_does_not_jump_an_old_memory_to_the_top_of_the_list() {
        let h = harness();
        let older = ancient("第一条旧偏好。");
        let newer = record("后来记下的那条偏好。", MemoryScope::Global, None);
        append_record(&h.conn, &h.paths, None, &older).unwrap();
        append_record(&h.conn, &h.paths, None, &newer).unwrap();
        let first = || list_all(&h.conn).unwrap().remove(0).record.id;
        assert_eq!(first(), newer.id, "两条都不重要到分胜负时，有效时间新的在前");

        edit_record(&h.conn, &h.paths, None, &older.id, &EditPatch { importance: Some(4), ..Default::default() })
            .unwrap()
            .record;
        assert_eq!(first(), newer.id, "编辑把一条三年前的记忆顶到了列表最前——它看起来又像刚发生了");
        teardown(&h);
    }

    /// 衰减是读侧的函数。谁想把"久不用"折进 `importance`，这条就红：
    /// 那等于系统偷偷改用户写下的事实
    #[test]
    fn scoring_and_decay_never_rewrite_the_stored_importance() {
        let h = harness();
        let stale = ancient("用户偏好结论先行，不要长篇铺垫。");
        let id = stale.id.clone();
        append_record(&h.conn, &h.paths, None, &stale).unwrap();
        let at = now_rfc3339();
        index::note_injection(&h.conn, std::slice::from_ref(&id), &at).unwrap();
        reinforce_records(&h.conn, &h.paths, None, std::slice::from_ref(&id), &at).unwrap();
        let after_writes = fs::read_to_string(h.paths.global_memory()).unwrap();
        assert!(after_writes.contains("importance: 4"), "importance 得是用户写的那个值：{after_writes}");

        for _ in 0..3 {
            search(&h.conn, &MemoryConfig::default(), "结论", None).unwrap();
        }
        let rebuilt = index::open(&h.paths.index_db()).unwrap();
        sync_all(&rebuilt, &h.paths, None, None).unwrap();
        assert_eq!(
            fs::read_to_string(h.paths.global_memory()).unwrap(),
            after_writes,
            "算分、强化、重建都不许动真相源一个字"
        );
        assert_eq!(parse_records(&after_writes).unwrap()[0].importance, 4);
        remove_tree(&h.paths.root);
        remove_tree(&h.workspace);
    }

    // ------------------------------------------------------------ P0：溯源（T01 / T04）

    #[test]
    fn provenance_answers_which_conversation_without_repeating_the_body() {
        let h = harness();
        let mut item = record("用户偏好结论先行，不要长篇铺垫。", MemoryScope::Global, None);
        item.origin = Some(Origin {
            conversation_id: "conv-9".into(),
            entries: vec!["entry-1".into(), "entry-2".into()],
            extracted_at: "2026-09-25T10:00:00+08:00".into(),
        });
        let id = item.id.clone();
        append_record(&h.conn, &h.paths, None, &item).unwrap();
        index::note_injection(&h.conn, std::slice::from_ref(&id), &now_rfc3339()).unwrap();

        let view = index::source_of(&h.conn, &id).expect("刚写的那条该问得出来历");
        assert_eq!(view.origin.as_ref().unwrap().conversation_id, "conv-9");
        assert_eq!(view.origin.as_ref().unwrap().entries, vec!["entry-1", "entry-2"]);
        assert_eq!(view.file, "global/MEMORY.md", "来源要用相对记忆根目录的路径：{}", view.file);
        assert_eq!(view.injections, 1, "被注入过几次也是来历的一部分");

        let text = serde_json::to_string(&view).unwrap();
        assert!(!text.contains("不要长篇铺垫"), "来历视图里不许出现正文：{text}");
        assert!(!text.contains("\"content\""), "连字段名都不该出现：出处只存标识：{text}");

        // 没出处的记录（手记、导入）答"没有出处"，而不是答一个编出来的
        let plain = record("用户自己记的一条。", MemoryScope::Global, None);
        let plain_id = plain.id.clone();
        append_record(&h.conn, &h.paths, None, &plain).unwrap();
        assert!(index::source_of(&h.conn, &plain_id).unwrap().origin.is_none());
        teardown(&h);
    }

    #[test]
    fn an_unreadable_origin_line_is_refused_rather_than_forgotten() {
        // 读不懂就当"没出处"，等于让一条来历不明的记录冒充用户自己写的
        let text = format!(
            "---\nid: mem_x\ntype: fact\nscope: global\nproject_id: null\nstatus: active\n\
             importance: 3\nconfidence: 0.9\nstability: stable\nsource: inferred\n\
             created_at: 2026-09-25T10:00:00+08:00\nupdated_at: 2026-09-25T10:00:00+08:00\n\
             occurred_at: null\nreinforced_at: null\norigin: 上周那场对话\n\
             last_used_at: null\nttl_days: null\ntags: []\nsupersedes: []\n---\n\n内容\n"
        );
        let error = parse_records(&text).err().expect("认不出的出处必须报错");
        assert!(error.contains("出处"), "报错要说清是出处读不懂：{error}");
    }

    #[test]
    fn event_time_and_write_time_both_reach_the_line_the_model_reads() {
        let h = harness();
        let mut item = ancient("用户 2022 年 11 月就把默认包管理器换成了 pnpm。");
        item.occurred_at = Some("2022-11-20T00:00:00+08:00".into());
        append_record(&h.conn, &h.paths, None, &item).unwrap();

        let line = item.injection_line();
        assert!(line.contains("发生: 2022-11-20"), "记录自己那行要带事情发生的时间：{line}");
        assert!(line.contains("更新: 2023-01-01"), "还要带记录被写下的时间：{line}");

        let shot = inject::build(&h.conn, &h.paths, &MemoryConfig::default(), "pnpm", None, None)
            .unwrap()
            .expect("检索该命中那条");
        assert!(shot.body.contains("发生: 2022-11-20"), "模型读到的那一行也得有时间语义：{}", shot.body);

        // 没说过什么时候发生的，就别硬凑一段"发生: 未知"
        let plain = record("用户偏好结论先行。", MemoryScope::Global, None);
        assert!(!plain.injection_line().contains("发生:"), "{}", plain.injection_line());
        teardown(&h);
    }

    // ------------------------------------------------------------ P0：索引仍是可重建的投影

    fn links_of(conn: &rusqlite::Connection) -> Vec<(String, String, String)> {
        conn.prepare("SELECT from_id, to_id, kind FROM memory_links ORDER BY kind, from_id, to_id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    /// 新加进索引的每一样（出处、事件时间、强化时间、冲突边）都必须能从 Markdown 问回来。
    /// 唯一问不回来的是注入次数——那是遥测，重建后归零，而归零只会让分数**变低**：
    /// 一条记忆绝不因为删过一次索引就假装自己更新更鲜
    #[test]
    fn the_rebuilt_index_carries_provenance_time_and_edges_back() {
        let h = harness();
        let mut stale = ancient("用户偏好结论先行，不要长篇铺垫。");
        stale.occurred_at = Some("2022-11-20".into());
        stale.origin = Some(Origin {
            conversation_id: "conv-9".into(),
            entries: vec!["entry-1".into()],
            extracted_at: "2023-01-01T10:00:00+08:00".into(),
        });
        let stale_id = stale.id.clone();
        append_record(&h.conn, &h.paths, None, &stale).unwrap();

        let mut rival = ancient("用户偏好结论先行，不要长篇大论。");
        rival.mark_conflict(&stale_id);
        append_record(&h.conn, &h.paths, None, &rival).unwrap();

        let at = now_rfc3339();
        index::note_injection(&h.conn, std::slice::from_ref(&stale_id), &at).unwrap();
        reinforce_records(&h.conn, &h.paths, None, std::slice::from_ref(&stale_id), &at).unwrap();

        let records_before: Vec<MemoryRecord> =
            list_all(&h.conn).unwrap().into_iter().map(|view| view.record).collect();
        let edges_before = links_of(&h.conn);
        let scores_before: Vec<Hit> = search(&h.conn, &MemoryConfig::default(), "结论", None).unwrap();
        let snapshot = fs::read_to_string(h.paths.global_memory()).unwrap();

        drop(h.conn);
        remove_tree_file(&h.paths.index_db());
        let rebuilt = index::open(&h.paths.index_db()).unwrap();
        sync_all(&rebuilt, &h.paths, None, None).unwrap();

        let records_after: Vec<MemoryRecord> =
            list_all(&rebuilt).unwrap().into_iter().map(|view| view.record).collect();
        assert_eq!(records_after, records_before, "出处、事件时间、强化章、取代与冲突边都要原样问回来");
        assert_eq!(links_of(&rebuilt), edges_before, "边重建后必须逐条一致，否则索引就成了第二真相");
        assert_eq!(fs::read_to_string(h.paths.global_memory()).unwrap(), snapshot, "重建不许动真相源");

        let scores_after = search(&rebuilt, &MemoryConfig::default(), "结论", None).unwrap();
        assert_eq!(scores_after.len(), scores_before.len());
        for hit in &scores_after {
            let was = scores_before.iter().find(|item| item.id == hit.id).unwrap();
            assert!(
                hit.score <= was.score + f64::EPSILON,
                "{} 删库重建后从 {:.4} 涨到 {:.4}：注入次数归零只能让分数降，不能让它复活",
                hit.id,
                was.score,
                hit.score
            );
            assert_eq!(hit.injections, 0, "注入次数是遥测，跟着索引一起清零");
        }
        assert_eq!(
            scores_after.iter().find(|hit| hit.id == stale_id).unwrap().reinforced_at.as_deref(),
            Some(at.as_str()),
            "强化章是事实，不许跟着遥测一起丢"
        );
        remove_tree(&h.paths.root);
        remove_tree(&h.workspace);
    }

    /// 图谱那两张派生表的内容，按 (记录, 实体) 排序
    fn entity_rows(conn: &rusqlite::Connection) -> Vec<(String, String, String, String)> {
        conn.prepare(
            "SELECT l.from_id, e.canonical, e.name, e.kind FROM memory_entity_links l \
             JOIN memory_entities e ON e.canonical = l.canonical \
             ORDER BY l.from_id, e.canonical",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<Result<Vec<(String, String, String, String)>, _>>()
        .unwrap()
    }

    /// T05 的判据：实体表整个删掉、从 `.md` 重抽，必须逐行一致，
    /// 而真相源一个字都不许多出来
    #[test]
    fn the_entity_tables_rebuild_row_for_row_from_the_records() {
        let h = harness();
        let mut held = ancient("索引在 `MEMORY.md` 里，闸门叫 `allowed_tools`，仓库在 src-tauri 下");
        held.tags = vec!["沟通风格".into()];
        held.entities = vec!["张三|person".into()];
        append_record(&h.conn, &h.paths, None, &held).unwrap();
        let snapshot = fs::read_to_string(h.paths.global_memory()).unwrap();

        let before = entity_rows(&h.conn);
        let kinds: Vec<&str> = before.iter().map(|(_, _, _, kind)| kind.as_str()).collect();
        assert!(
            before.iter().any(|(_, canonical, _, kind)| canonical == "张三" && kind == "person"),
            "写明的人名要按写明的 kind 落表：{before:?}"
        );
        assert!(
            before.iter().any(|(_, canonical, _, kind)| canonical == "memory.md" && kind == "file"),
            "形状认出来的文件要标成 file：{before:?}"
        );
        assert!(before.iter().any(|(_, canonical, ..)| canonical == "沟通风格"), "标签就是实体");
        assert_eq!(
            kinds.iter().filter(|kind| **kind == "person").count(),
            1,
            "person 只能来自写明的那一条：规则不许自己猜出一个人名"
        );

        drop(h.conn);
        remove_tree_file(&h.paths.index_db());
        let rebuilt = index::open(&h.paths.index_db()).unwrap();
        sync_all(&rebuilt, &h.paths, None, None).unwrap();

        assert_eq!(entity_rows(&rebuilt), before, "删库重建之后实体必须逐行一致，否则它就是第二份真相");
        assert_eq!(
            fs::read_to_string(h.paths.global_memory()).unwrap(),
            snapshot,
            "实体不许写进 .md：那是索引的活儿"
        );
    }

    /// 一条讲张三、但正文里只写"他"的记忆：全文检索碰不到它，实体图碰得到
    fn about_zhang(content: &str) -> MemoryRecord {
        let mut item = ancient(content);
        item.tags = Vec::new();
        item.entities = vec!["张三|person".into()];
        item
    }

    fn ids(hits: &[Hit]) -> Vec<String> {
        hits.iter().map(|hit| hit.id.clone()).collect()
    }

    /// 配置里那一格走到打分的地方了吗：`search_options` 是那座唯一的桥，
    /// 所以"配了等于没配"这种毛病只需要在这一个点上问一次
    #[test]
    fn the_tuned_half_life_reaches_the_scorer() {
        let default = MemoryConfig::default();
        assert_eq!(
            search_options(&default, "问一句", None).half_life_days,
            crate::memory::decay::HALF_LIFE_DAYS,
            "默认值必须是那条曲线原本的半衰期，不然落地那天就改了排序"
        );
        let mut tuned = MemoryConfig::default();
        tuned.decay_half_life_days = 7.0;
        assert_eq!(search_options(&tuned, "问一句", None).half_life_days, 7.0);
    }

    #[test]
    fn a_memory_about_zhang_surfaces_though_the_query_never_hits_its_body() {
        let h = harness();
        let direct = ancient("部署脚本要幂等，跑两遍不许改结果。");
        let direct_id = direct.id.clone();
        append_record(&h.conn, &h.paths, None, &direct).unwrap();
        let aside = about_zhang("下周由他值班，别再临时催他。");
        let aside_id = aside.id.clone();
        append_record(&h.conn, &h.paths, None, &aside).unwrap();

        let config = MemoryConfig::default();
        let options = search_options(&config, "张三 部署脚本", None);
        let semantic = index::search(&h.conn, &options).unwrap();
        assert_eq!(ids(&semantic), vec![direct_id.clone()], "正文里没有「张三」的那条不该被全文捞到");

        let (same_column, graph) = index::recall(&h.conn, &options).unwrap();
        assert_eq!(ids(&same_column), ids(&semantic), "召回不许动语义那一列一个字");
        assert_eq!(graph.len(), 1, "实体图该把另一条带出来：{graph:?}");
        assert_eq!(graph[0].hit.id, aside_id);
        assert_eq!(graph[0].hop, 1);
        assert_eq!(graph[0].entity, "张三", "提示要说是哪个实体把它带出来的");
        assert_eq!(graph[0].hit.why, "实体命中（第 1 跳）：张三");

        teardown(&h);
    }

    #[test]
    fn a_second_hop_arrives_through_a_shared_topic_rather_than_the_query() {
        let h = harness();
        let mut bridge = ancient("部署脚本要幂等，跑两遍不许改结果。");
        bridge.tags = Vec::new();
        bridge.entities = vec!["张三|person".into(), "发布流水线".into()];
        append_record(&h.conn, &h.paths, None, &bridge).unwrap();

        let mut far = ancient("那条每周五夜里跑，跑完会自己清掉工作目录。");
        far.tags = Vec::new();
        far.entities = vec!["发布流水线".into()];
        let far_id = far.id.clone();
        append_record(&h.conn, &h.paths, None, &far).unwrap();

        let config = MemoryConfig::default();
        let options = search_options(&config, "张三 部署脚本", None);
        let (semantic, graph) = index::recall(&h.conn, &options).unwrap();
        assert_eq!(graph.len(), 1, "两跳只该带出那一条：{graph:?}");
        assert_eq!(graph[0].hit.id, far_id);
        assert_eq!(graph[0].hop, 2, "经由中间那条才碰到的，算第二跳");
        assert_eq!(graph[0].entity, "发布流水线");
        assert!(
            !ids(&semantic).contains(&far_id),
            "两跳带出来的那条本来不该在语义列里，否则这测试没在量它声称的东西"
        );

        teardown(&h);
    }

    /// 预算是那条不能越的线：宁可少提示，也不让一次推导把注入的额度吃掉。
    /// 那两条的正文刻意躲开查询里的每个字——中文按单字+双字切词、命中任一 token 即候选，
    /// 少躲一个字它们就成全文命中了，这一测的就不再是预算
    #[test]
    fn entity_recall_stops_at_the_limit_it_was_given() {
        let h = harness();
        let direct = ancient("部署脚本要幂等，跑两遍不许改结果。");
        append_record(&h.conn, &h.paths, None, &direct).unwrap();
        append_record(&h.conn, &h.paths, None, &about_zhang("下周由他值班，别再临时催他。")).unwrap();
        append_record(&h.conn, &h.paths, None, &about_zhang("他的年假还没用完，别都排给他。")).unwrap();

        let mut config = MemoryConfig::default();
        config.search_limit = 1;
        let options = search_options(&config, "张三 部署脚本", None);
        let (semantic, graph) = index::recall(&h.conn, &options).unwrap();
        assert_eq!(semantic.len() + graph.len(), 1, "两列加起来不许超过预算：{graph:?}");

        config.search_limit = 8;
        let options = search_options(&config, "张三 部署脚本", None);
        let (semantic, graph) = index::recall(&h.conn, &options).unwrap();
        assert_eq!(graph.len(), 2, "预算放开后那两条都该顺出来：{graph:?}");
        assert_eq!(semantic.len(), 1, "语义那一列不随预算变多");

        teardown(&h);
    }

    #[test]
    fn hints_stay_silent_until_proactive_recall_is_switched_on() {
        let h = harness();
        append_record(&h.conn, &h.paths, None, &ancient("部署脚本要幂等，跑两遍不许改结果。")).unwrap();
        let aside = about_zhang("下周由他值班，别再临时催他。");
        let aside_id = aside.id.clone();
        append_record(&h.conn, &h.paths, None, &aside).unwrap();

        let query = "张三 部署脚本";
        let mut config = MemoryConfig::default();
        assert!(
            recall_hints(&h.conn, &config, query, None, &[]).unwrap().is_empty(),
            "默认关着就一条提示都不该有"
        );

        config.proactive_recall = true;
        let hints = recall_hints(&h.conn, &config, query, None, &[]).unwrap();
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].record_id, aside_id);
        assert!(!hints[0].injected, "没注入过才叫「本轮没用上」");
        assert_eq!(hints[0].reason, "实体命中（第 1 跳）：张三");

        let injected = recall_hints(&h.conn, &config, query, None, std::slice::from_ref(&aside_id)).unwrap();
        assert!(injected[0].injected, "已经让模型看到的那条要标出来，别再提示一遍");

        teardown(&h);
    }

    /// 这条是关键的那个反证：开启主动回忆之后，实际发出去的内容必须逐字节不变。
    /// 一旦有人把提示接进注入通道，前缀缓存就整片作废，而这是设计里明令禁止的第二条通道
    #[test]
    fn switching_on_proactive_recall_leaves_the_sent_bytes_identical() {
        let h = harness();
        append_record(&h.conn, &h.paths, None, &ancient("部署脚本要幂等，跑两遍不许改结果。")).unwrap();
        append_record(&h.conn, &h.paths, None, &about_zhang("下周由他值班，别再临时催他。")).unwrap();

        let query = "张三 部署脚本";
        let mut config = MemoryConfig::default();
        let off = inject::build(&h.conn, &h.paths, &config, query, None, None).unwrap().unwrap();

        config.proactive_recall = true;
        let hints = recall_hints(&h.conn, &config, query, None, &[]).unwrap();
        assert!(!hints.is_empty(), "开启后确实召回到了东西，否则这条测试什么都没否证");
        let on = inject::build(&h.conn, &h.paths, &config, query, None, None).unwrap().unwrap();

        assert_eq!(on.body, off.body, "提示不是注入：正文一个字都不许变");
        assert_eq!(
            on.items.iter().map(|item| item.line.clone()).collect::<Vec<_>>(),
            off.items.iter().map(|item| item.line.clone()).collect::<Vec<_>>(),
        );

        teardown(&h);
    }

    /// T09 的判据：`secret` 那条**能检索到、能在面板看，但注入数组里不出现**。
    /// 反证臂是同一份数据标成 `public` 时它必须进得去——否则这条测试量的是"根本没检索到"，
    /// 而不是这道闸
    #[test]
    fn a_secret_record_is_found_and_listed_but_never_injected() {
        let h = harness();
        // 同一话题的两条：一条在红线内，一条照常。只剩红线内那条的时候注入整段都不建，
        // 那样测出来的是"没内容可注入"而不是这道闸在起作用
        let peer = ancient("网关地址写在部署手册第一页。");
        append_record(&h.conn, &h.paths, None, &peer).unwrap();
        let mut kept = ancient("网关口令每季轮换一次。");
        kept.sensitivity = record::MemorySensitivity::Secret;
        append_record(&h.conn, &h.paths, None, &kept).unwrap();

        let config = MemoryConfig::default();
        let found = search(&h.conn, &config, "网关 口令", None).unwrap();
        assert!(
            found.iter().any(|hit| hit.id == kept.id),
            "标成 secret 不该把它从检索里抹掉：{found:?}"
        );
        let listed = list_all(&h.conn).unwrap();
        let shown = listed.iter().find(|view| view.record.id == kept.id).expect("面板要列得出它");
        assert_eq!(shown.record.sensitivity, record::MemorySensitivity::Secret, "分级要看得见，不然没人知道自己标过");

        let injection = inject::build(&h.conn, &h.paths, &config, "网关 口令", None, None)
            .unwrap()
            .expect("有一条 public 在，注入段就该建起来");
        assert!(
            !injection.items.iter().any(|item| item.id == kept.id),
            "红线内的记录不许进注入数组：{:?}",
            injection.items.iter().map(|item| item.line.clone()).collect::<Vec<_>>()
        );
        assert!(
            !injection.body.contains("轮换"),
            "整段发出去的字节里都不该有它：{}",
            injection.body
        );

        // 反证臂：同一份内容改回 public 就该进来，否则上面那两句什么都没量到
        let back = EditPatch { sensitivity: Some("public".into()), ..Default::default() };
        edit_record(&h.conn, &h.paths, None, &kept.id, &back).unwrap();
        let again = inject::build(&h.conn, &h.paths, &config, "网关 口令", None, None).unwrap().expect("同上");
        assert!(
            again.items.iter().any(|item| item.id == kept.id),
            "改回 public 之后它就该能被注入——上面那条断言量的是这道闸，不是检索坏了"
        );

        teardown(&h);
    }

    /// 默认档不落那一行。落了会改到每条既有记录的正文哈希，
    /// 于是"升级一次"在索引眼里就是"整库记忆都被改过一次"
    #[test]
    fn only_a_tightened_level_writes_a_frontmatter_line() {
        let plain = ancient("部署脚本要幂等，跑两遍不许改结果。");
        assert!(
            !plain.to_markdown().contains("sensitivity:"),
            "public 不该落那一行：{}",
            plain.to_markdown()
        );

        let mut kept = ancient("部署脚本要幂等，跑两遍不许改结果。");
        kept.sensitivity = record::MemorySensitivity::Private;
        let text = kept.to_markdown();
        assert!(text.contains("sensitivity: private"), "{text}");
        let back = parse_records(&text).unwrap().remove(0);
        assert_eq!(back.sensitivity, record::MemorySensitivity::Private, "写出去要读得回来");

        // 拼错的写法报错，不是当成没写：那等于把红名单读成空白
        let typo = text.replace("sensitivity: private", "sensitivity: privat");
        assert!(parse_records(&typo).is_err(), "读不懂的敏感度必须报错，不能悄悄退回 public");
    }

    /// 标记这个动作的入口在面板，所以它必须立刻可查：写进 `.md` 而读回来还是 public，
    /// 界面上那一格就永远是灰的
    #[test]
    fn marking_a_record_lands_in_the_file_and_refuses_a_typo() {
        let h = harness();
        let kept = ancient("上线窗口定在周二凌晨。");
        let id = kept.id.clone();
        append_record(&h.conn, &h.paths, None, &kept).unwrap();

        let patch = EditPatch { sensitivity: Some("secret".into()), ..Default::default() };
        let view = edit_record(&h.conn, &h.paths, None, &id, &patch).unwrap();
        assert_eq!(view.record.sensitivity, record::MemorySensitivity::Secret);
        assert!(
            read_text(&h.paths.global_memory()).contains("sensitivity: secret"),
            "标记必须落进真相源，不能只活在索引里"
        );

        let typo = EditPatch { sensitivity: Some("privat".into()), ..Default::default() };
        assert!(
            edit_record(&h.conn, &h.paths, None, &id, &typo).is_err(),
            "读不懂的档位要报错——当成\"这次没改这一项\"就是静默收下用户以为生效的红线"
        );
        teardown(&h);
    }

    /// 值打错有 `marking_a_record_lands_in_the_file_and_refuses_a_typo` 钉着，这一条钉的是
    /// **键名**打错。`EditPatch` 有容器 `default`，所以 `{senstivity:"secret"}` 合出来是一份
    /// 空补丁：用户点的是"让这条不再出去"，系统收下一个它不认识的键、行为照旧，那条就照常出门
    #[test]
    fn a_memory_request_naming_a_field_that_does_not_exist_is_refused() {
        let typo: Result<EditPatch, serde_json::Error> =
            serde_json::from_str(r#"{"senstivity":"secret"}"#);
        let err = typo.expect_err("差一个字母的键名不能算拨过那一档").to_string();
        assert!(err.contains("senstivity"), "要把认错的那个键说给他听：{err}");

        // 正对照：真键要合得上，否则这条测试只是在"永远报错"时绿
        let ok: EditPatch =
            serde_json::from_str(r#"{"sensitivity":"secret"}"#).expect("真键该过");
        assert_eq!(ok.sensitivity.as_deref(), Some("secret"));
        let both: EditPatch = serde_json::from_str(r#"{"tags":["a"],"importance":4}"#)
            .expect("多键补丁该过");
        assert_eq!(both.tags, Some(vec!["a".to_string()]));
        assert_eq!(both.importance, Some(4));

        // 写入侧同一件事：认错的键名不能变成"用默认值记一条"
        let typo_add: Result<AddArgs, serde_json::Error> =
            serde_json::from_str(r#"{"contnet":"一句话"}"#);
        let err = typo_add.expect_err("打错一个字母的正文不能算没给").to_string();
        assert!(err.contains("contnet"), "要报出认错的那个键：{err}");
        let ok_add: AddArgs =
            serde_json::from_str(r#"{"content":"一句话"}"#).expect("只给必填项的写法该过");
        assert_eq!(ok_add.content, "一句话");
        assert!(ok_add.tags.is_empty(), "没给的 tags 该是空表，不是报错");
    }

    /// 空补丁不是中性的一次请求：`updated_at` 一跳，候选区的 TTL 与自动转正那条曲线
    /// 都被推回今天（`govern.rs` 读的是 `ttl_days, updated_at`），文件重写一遍，
    /// 审计还要落一条 "edit"。所以它得在动手之前被拒
    #[test]
    fn an_empty_patch_cannot_give_a_candidate_a_free_new_life() {
        let h = harness();
        let mut item = record("这条候选一直没人点头。", MemoryScope::Global, None);
        item.status = MemoryStatus::Candidate;
        append_record(&h.conn, &h.paths, None, &item).unwrap();
        let before = fs::read_to_string(h.paths.global_memory()).unwrap();

        let err = edit_record(&h.conn, &h.paths, None, &item.id, &EditPatch::default())
            .expect_err("什么都没改的补丁不该被记成一次编辑");
        assert!(err.contains("什么都没改"), "要给他一句人话：{err}");
        assert_eq!(
            fs::read_to_string(h.paths.global_memory()).unwrap(),
            before,
            "拒了就不该动过文件——那一格里写着 updated_at"
        );

        // 正对照：给一个字段的补丁要正常落地，不然这条也只是在"永远报错"时绿
        edit_record(
            &h.conn,
            &h.paths,
            None,
            &item.id,
            &EditPatch { importance: Some(5), ..Default::default() },
        )
        .unwrap();
        assert_ne!(fs::read_to_string(h.paths.global_memory()).unwrap(), before);
        teardown(&h);
    }

    /// 反思那份材料是**没人问也往外发**的一路，所以它读的是比注入更严的一档：
    /// `private` 与 `secret` 都不进去。少了这一道，"标成不外发"就只挡住了注入那一路
    #[test]
    fn reflection_material_keeps_out_everything_that_is_not_public() {
        let h = harness();
        let open = ancient("回答先给结论，不要长篇铺垫。");
        let mut held = ancient("周报每周五下班前交。");
        held.sensitivity = record::MemorySensitivity::Private;
        let mut secret = ancient("网关口令每季轮换一次。");
        secret.sensitivity = record::MemorySensitivity::Secret;
        for item in [&open, &held, &secret] {
            append_record(&h.conn, &h.paths, None, item).unwrap();
        }

        let material = reflect::material_of(&h.paths, &h.conn, None).unwrap();
        let listed = material
            .split_once("已经记着的")
            .expect("材料里要有那份清单")
            .1;
        assert!(listed.contains("先给结论"), "public 的那条是该出去的：{listed}");
        assert!(!listed.contains("周报每周五"), "private 的不进后台材料");
        assert!(!listed.contains("轮换"), "secret 的更不进后台材料");
        teardown(&h);
    }

    /// 实体图是跨项目的：两条记忆讲同一个人，一条住在这个仓库、一条住在另一个。
    /// 顺边带出来之后必须再过一次项目过滤，否则图谱就成了隔离墙上的洞
    #[test]
    fn entity_recall_does_not_leak_another_projects_memory() {
        let h = harness();
        let mut mine = record("部署脚本要幂等，跑两遍不许改结果。", MemoryScope::Project, Some("proj-a"));
        mine.tags = Vec::new();
        mine.entities = vec!["张三|person".into()];
        append_record(&h.conn, &h.paths, None, &mine).unwrap();

        let mut theirs = record("下周由他值班，别再临时催他。", MemoryScope::Project, Some("proj-b"));
        theirs.tags = Vec::new();
        theirs.entities = vec!["张三|person".into()];
        append_record(&h.conn, &h.paths, None, &theirs).unwrap();

        let mut config = MemoryConfig::default();
        config.proactive_recall = true;
        let hints = recall_hints(&h.conn, &config, "张三 部署脚本", Some("proj-a"), &[]).unwrap();
        assert!(hints.is_empty(), "另一个项目的记忆不许出现在这里的提示里：{hints:?}");

        teardown(&h);
    }

    /// 时间线量的是"事情什么时候发生"，不是"这条什么时候被写下"：
    /// 去年发生的事今年才记下来，它该待在去年的那一格
    #[test]
    fn timeline_sorts_by_when_it_happened_not_by_when_it_was_written() {
        let h = harness();
        let mut late_note = ancient("上线评审定在周五。");
        late_note.occurred_at = Some("2024-05-05".into());
        append_record(&h.conn, &h.paths, None, &late_note).unwrap();

        let mut long_ago = record("迁移方案敲定了。", MemoryScope::Global, None);
        long_ago.occurred_at = Some("2020-01-01".into());
        append_record(&h.conn, &h.paths, None, &long_ago).unwrap();

        let mut no_date = ancient("没说清是哪天，这种不上时间线。");
        no_date.occurred_at = None;
        append_record(&h.conn, &h.paths, None, &no_date).unwrap();

        let rows = index::timeline(&h.conn, 10).unwrap();
        assert_eq!(
            rows.iter().map(|row| row.at.as_str()).collect::<Vec<_>>(),
            vec!["2024-05-05", "2020-01-01"],
            "按业务时间倒序，且没填 occurred_at 的那条不进时间线：{rows:?}"
        );
        assert_eq!(rows[0].record_id, late_note.id);
        assert_eq!(rows[0].content, "上线评审定在周五。", "时间线得说清发生过什么，不是只给一个日期");
        assert_eq!(rows[1].entity.as_deref(), Some("沟通风格"), "标签就是实体，时间线用它做标注");

        teardown(&h);
    }

    #[test]
    fn timeline_stops_at_the_number_of_rows_it_was_asked_for() {
        let h = harness();
        for index in 0..5 {
            let mut item = ancient(&format!("第 {index} 次发布顺利完成。"));
            item.occurred_at = Some(format!("2021-0{}-10", index + 1));
            append_record(&h.conn, &h.paths, None, &item).unwrap();
        }

        let rows = index::timeline(&h.conn, 2).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].at, "2021-05-10", "最近的排最前");

        teardown(&h);
    }

    /// 真相源里删掉标记，重建后那条边必须消失：证明边是投影，不是第二个真相
    #[test]
    fn deleting_the_marker_from_markdown_deletes_the_edge_on_rebuild() {
        let h = harness();
        let one = ancient("部署用 docker compose 起。");
        let one_id = one.id.clone();
        let mut other = ancient("部署用 podman compose 起。");
        other.mark_conflict(&one_id);
        append_record(&h.conn, &h.paths, None, &one).unwrap();
        append_record(&h.conn, &h.paths, None, &other).unwrap();
        assert!(links_of(&h.conn)
            .iter()
            .any(|(_, _, kind)| kind == "conflicts_with"), "标记要长成边");

        let mut on_disk = parse_records(&fs::read_to_string(h.paths.global_memory()).unwrap()).unwrap();
        let position = on_disk.iter().position(|item| item.id == other.id).unwrap();
        on_disk[position].clear_conflicts();
        rewrite_file(&h.conn, &h.paths, &h.paths.global_memory(), &on_disk).unwrap();
        assert!(
            links_of(&h.conn).iter().all(|(_, _, kind)| kind != "conflicts_with"),
            "正文里已经没有的标记，索引里不许自己活着"
        );
        teardown(&h);
    }

    #[test]
    fn an_exported_memory_keeps_its_provenance_and_event_time() {
        let source = harness();
        let mut item = ancient("用户 2022 年把默认包管理器换成了 pnpm。");
        item.occurred_at = Some("2022-11-20".into());
        item.origin = Some(Origin {
            conversation_id: "conv-9".into(),
            entries: vec!["entry-1".into()],
            extracted_at: "2023-01-01T10:00:00+08:00".into(),
        });
        append_record(&source.conn, &source.paths, None, &item).unwrap();
        let text = bundle_of(&source.conn).unwrap();
        assert!(text.contains("conv-9"), "导出要带上出处，换机器才谈得上追问：{text}");

        let target = Paths::new(temp_dir("memory-target"));
        ensure_layout(&target).unwrap();
        let conn = index::open(&target.index_db()).unwrap();
        assert_eq!(import_bundle(&conn, &target, &text).unwrap(), 1);
        let moved = list_all(&conn).unwrap().remove(0).record;
        assert_eq!(moved.occurred_at.as_deref(), Some("2022-11-20"));
        assert_eq!(moved.origin, item.origin, "换一台机器，来历还是同一条");

        teardown(&source);
        remove_tree(&target.root);
    }
}
