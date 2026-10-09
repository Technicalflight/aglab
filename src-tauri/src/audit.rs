//! 统一审计：一行一个动作，带**主体**与**结果**。
//!
//! 三条规矩：
//! 1. 主体是调用点传进来的参数，不从内容猜。缺省值是 `Model`（"最危险的默认"），
//!    不是 `User`——把"这是人干的"变成可查的事实，而不是无记录的推定。
//! 2. 只记标识与结果，不记正文：正文里可能带敏感信息，而这份日志是给人翻的。
//! 3. 写失败要报给调用方。被拦下的动作与"没发生过"必须分得开，
//!    所以 `record` 不吞错。

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub const AUDIT_DIR: &str = "audit";
/// 分片超过这个天数就整片搬进 `audit/archive/`：归档而不是删除。
/// 自动消失的日志不算审计
pub const RETENTION_DAYS: i64 = 90;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Actor {
    /// 用户在界面上点的
    User,
    /// 模型的回合（含它触发的工具调用）
    Model,
    /// 定时任务 / 编排器
    Scheduler,
    /// 反思与元认知（Memory 2.0）
    Reflection,
    /// 从别的机器导回来
    Import,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    /// 放行并且做完了
    Ok,
    /// 闸门拦下：被拒、需审批而无人应答
    Denied,
    /// 想做了但失败
    Failed,
    /// 被上游策略挡住（权限表、敏感出口、熔断、限流）
    Blocked,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Event<'a> {
    pub at: String,
    pub actor: Actor,
    pub action: &'a str,
    pub target: String,
    pub outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

fn stamp() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

fn shard_name(date: &str) -> String {
    format!("audit-{date}.jsonl")
}

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// 当天那一片的路径。目录不在就建，并按记忆目录同样的方式收紧权限
pub fn current_shard(root: &Path) -> Result<PathBuf, String> {
    let dir = root.join(AUDIT_DIR);
    fs::create_dir_all(&dir).map_err(|e| format!("创建 {} 失败：{e}", dir.display()))?;
    harden_dir(&dir);
    Ok(dir.join(shard_name(&today())))
}

/// 记一行。失败原样报出去——调用方必须知道"这一笔没留下痕迹"
pub fn record(
    root: &Path,
    actor: Actor,
    action: &str,
    target: &str,
    outcome: Outcome,
) -> Result<(), String> {
    record_detail(root, actor, action, target, outcome, None)
}

pub fn record_detail(
    root: &Path,
    actor: Actor,
    action: &str,
    target: &str,
    outcome: Outcome,
    detail: Option<String>,
) -> Result<(), String> {
    let event = Event {
        at: stamp(),
        actor,
        action,
        target: target.to_string(),
        outcome,
        // 出口过滤：detail 只是给排查用的短说明，敏感内容一律不留
        detail: detail.filter(|text| crate::secrets::redact_for_audit(text).is_none()),
    };
    let line = serde_json::to_string(&event).map_err(|e| format!("审计行编码失败：{e}"))?;
    let path = current_shard(root)?;
    let mut file = open_shard_with_bounded_retry(&path)?;
    // 一次 write_all，不用 writeln!：`writeln!` 会分两次写（正文、换行符），
    // 并发的追加就会把两条记录粘在同一行上——这条真在我们自己的测试里出现过
    let mut record = line.into_bytes();
    record.push(b'\n');
    file.write_all(&record)
        .map_err(|e| format!("写 {} 失败：{e}", path.display()))
}

/// 打开今天的分片做追加。实时防护（Defender 一类）会在文件刚创建的窗口里短暂持有它，
/// 多个线程同时开那一发就会拿到"拒绝访问"——`concurrent_appends_never_share_a_line`
/// 在这样的机器上稳定红过（8 线程里 7 个首轮 open 被拒；与实现无关，Python 同样复现）。
/// 有界退避重试：防的是瞬时占用，不掩盖真实拒绝——重试耗尽仍失败时原样报错，
/// "这一笔没留下痕迹"的契约不变。
fn open_shard_with_bounded_retry(path: &Path) -> Result<std::fs::File, String> {
    let mut delay = Duration::from_millis(10);
    let mut last = String::new();
    for _ in 0..6 {
        match fs::OpenOptions::new().create(true).append(true).open(path) {
            Ok(file) => return Ok(file),
            Err(error) => last = format!("打开 {} 失败：{error}", path.display()),
        }
        std::thread::sleep(delay);
        delay *= 2;
    }
    Err(last)
}

/// 读某一天的审计。`date` 为空 = 今天。归档过的也能按 `audit/archive/<名字>` 读
pub fn read_day(root: &Path, date: Option<&str>) -> Vec<String> {
    let dir = root.join(AUDIT_DIR);
    let name = shard_name(date.unwrap_or(&today()));
    for candidate in [dir.join(&name), dir.join(ARCHIVE_DIR).join(&name)] {
        if let Ok(text) = fs::read_to_string(&candidate) {
            return text
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(String::from)
                .collect();
        }
    }
    Vec::new()
}

const ARCHIVE_DIR: &str = "archive";

/// 读回来给人看的那一行。与 `Event` 是**同一行 JSON 的两面**：写侧的 `action` 借 `&str`、
/// 读侧要 owned，所以分成两个结构体——`the_two_sides_read_the_same_line` 逐字段钉住它们对得上，
/// 对不上的话写侧改字段名而读侧不知道，界面上就是一片空
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Entry {
    pub at: String,
    pub actor: Actor,
    pub action: String,
    pub target: String,
    pub outcome: Outcome,
    #[serde(default)]
    pub detail: Option<String>,
}

/// 一页审计。
///
/// `skipped` 是**读不懂的行数**，必须说出来：坏行当然跳过（我们的分片是并发追加的），
/// 但"静默少几行"与"那几件事没发生过"在审计里是两件事——那正是同一批行被粘成一行时
/// 我们吃过的亏，不能让读侧再造一次
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    pub date: String,
    pub entries: Vec<Entry>,
    pub skipped: usize,
    /// 这一页被截断了（只留了最新的那 `limit` 条）
    pub truncated: bool,
}

/// 一次最多读回多少条。审计是只增的，"把一整天都送来"不是读数而是把界面卡住
pub const PAGE_DEFAULT: usize = 200;

/// 从原始行拼出一页。剥成纯函数是因为：坏行、截断、顺序这三件事都不该要 `AppHandle` 才测得动
pub fn page_of(date: String, lines: Vec<String>, limit: usize) -> Page {
    let mut entries: Vec<Entry> = Vec::new();
    let mut skipped = 0usize;
    for line in lines {
        match serde_json::from_str::<Entry>(&line) {
            Ok(entry) => entries.push(entry),
            Err(_) => skipped += 1,
        }
    }
    let truncated = entries.len() > limit;
    if truncated {
        // 留最新的那一截：审计按时间追加，最旧的那些正是没人要先看的
        let drop = entries.len() - limit;
        entries.drain(..drop);
    }
    Page {
        date,
        entries,
        skipped,
        truncated,
    }
}

/// 到期的分片整片搬进 `audit/archive/`。返回被搬走的路径。
/// 这里绝不删文件：审计的保留策略是"归档"，删除只能由用户点"清空"
pub fn rotate(root: &Path, keep_days: i64) -> Result<Vec<PathBuf>, String> {
    let dir = root.join(AUDIT_DIR);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    let cutoff = chrono::Local::now().date_naive() - chrono::Duration::days(keep_days.max(0));
    let mut moved = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        let Some(date) = date_of(name) else { continue };
        let Ok(day) = chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d") else {
            continue;
        };
        if day >= cutoff {
            continue;
        }
        let archive = dir.join(ARCHIVE_DIR);
        fs::create_dir_all(&archive)
            .map_err(|e| format!("创建 {} 失败：{e}", archive.display()))?;
        let target = archive.join(name);
        // 同名（一天一片，正常不会撞）就让时间戳说话，不覆盖历史
        let target = if target.exists() {
            archive.join(format!(
                "{date}-{}.jsonl",
                chrono::Local::now().timestamp_nanos_opt().unwrap_or(0)
            ))
        } else {
            target
        };
        fs::rename(&path, &target).map_err(|e| format!("搬 {} 失败：{e}", path.display()))?;
        moved.push(target);
    }
    Ok(moved)
}

/// 清空活动分片（design-security-center.md D9）：**先把今天的分片归档、再清空活动区**。
/// 账本文化：清空 = 归档后挪走，归档文件仍在磁盘上——永不静默销毁，清空后
/// 界面注明归档位置。返回归档文件的路径
pub fn clear_active(root: &Path) -> Result<Vec<PathBuf>, String> {
    let dir = root.join(AUDIT_DIR);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    let archive = dir.join(ARCHIVE_DIR);
    fs::create_dir_all(&archive).map_err(|e| format!("创建 {} 失败：{e}", archive.display()))?;
    let mut moved = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if date_of(name).is_none() {
            continue; // 只动分片：archive/ 子目录与其余文件不碰
        }
        let stamp = chrono::Local::now().timestamp_nanos_opt().unwrap_or(0);
        let target = archive.join(format!("clear-{}-{name}", stamp));
        fs::rename(&path, &target).map_err(|e| format!("搬 {} 失败：{e}", path.display()))?;
        moved.push(target);
    }
    Ok(moved)
}

/// 导出审计日志（design-security-center.md D9）：把指定日期段的活动分片按序拼成
/// 一份 JSONL 写进 `audit/export-<时刻>.jsonl`，返回路径。分片本来就是按天的文件，
/// 导出 = 按序拼装，不造第二份格式
pub fn export(root: &Path, from: &str, to: &str) -> Result<PathBuf, String> {
    let dir = root.join(AUDIT_DIR);
    let mut lines: Vec<String> = Vec::new();
    let Ok(entries) = fs::read_dir(&dir) else {
        return Err(format!("读 {} 失败：目录不存在", dir.display()));
    };
    let mut days: Vec<String> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str().map(String::from))
        .filter_map(|name| date_of(&name))
        .filter(|date| date.as_str() >= from && date.as_str() <= to)
        .collect();
    days.sort();
    for date in &days {
        lines.extend(read_day(root, Some(date)));
    }
    let dest = dir.join(format!(
        "export-{}.jsonl",
        chrono::Local::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    fs::write(
        &dest,
        lines.join(
            "
",
        ),
    )
    .map_err(|e| format!("写 {} 失败：{e}", dest.display()))?;
    Ok(dest)
}

fn date_of(name: &str) -> Option<String> {
    let rest = name.strip_prefix("audit-")?;
    let (date, _) = rest.split_once('.')?;
    Some(date.to_string())
}

#[cfg(unix)]
fn harden_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn harden_dir(_path: &Path) {}

/// 按配置里的保留天数归档旧分片，返回搬走了几片（0 是常态：没有到期的）。
/// 启动时自己跑一次；设置页改了阈值也可以立刻再跑一次看到结果。
///
/// 这条命令存在的理由就是 `rotate` 之前没有调用方：一个没人调的保留策略等于没有保留策略，
/// 而审计只增不减的代价最后会落在"日志太大所以没人翻"上——那才是审计真正的死法
#[tauri::command]
pub fn audit_rotate(app: tauri::AppHandle) -> Result<usize, String> {
    use tauri::Manager;
    let root = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("拿不到数据目录：{e}"))?;
    let keep = crate::config::load(&app).audit_keep_days;
    rotate(&root, keep).map(|moved| moved.len())
}

/// 把某一天的审计读回来。写侧从第一天就在，**读侧一直没有**：那本账只有机器看得见，
/// 而"事后能回答谁在什么时候对什么做了什么"这句话是写给人的
///
/// 日期先验格式再说"没有记录"——`2026-13-40` 与"这一天确实一条没有"是两件事，
/// 混成一句"0 条"就是在替用户造一个看着像结论的假读数
/// 导出审计日志（design-security-center.md D9）：把 `from..=to`（含）日期段的活动
/// 分片拼成一份 JSONL。返回导出文件路径——分片本来就是按天的文件，导出是拼装不是转换
#[tauri::command]
pub fn audit_export(app: tauri::AppHandle, from: String, to: String) -> Result<String, String> {
    use tauri::Manager;
    let root = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let dest = export(&root, &from, &to)?;
    Ok(dest.to_string_lossy().to_string())
}

/// 清空活动分片（design-security-center.md D9）：先归档再清，返回归档文件路径。
/// 账本文化：清空不销毁——归档仍在磁盘上，界面要注明位置
#[tauri::command]
pub fn audit_clear(app: tauri::AppHandle) -> Result<Vec<String>, String> {
    use tauri::Manager;
    let root = app.path().app_data_dir().map_err(|e| e.to_string())?;
    Ok(clear_active(&root)?
        .iter()
        .map(|path| path.to_string_lossy().to_string())
        .collect())
}

#[tauri::command]
pub fn audit_view(app: tauri::AppHandle, date: Option<String>) -> Result<Page, String> {
    use tauri::Manager;
    let root = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("拿不到数据目录：{e}"))?;
    let day = match date.as_deref() {
        None | Some("") => today(),
        Some(wanted) => {
            chrono::NaiveDate::parse_from_str(wanted, "%Y-%m-%d")
                .map_err(|_| format!("日期得写成 2026-09-26 那样，你给的是「{wanted}」"))?;
            wanted.to_string()
        }
    };
    let lines = read_day(&root, Some(&day));
    // 一页多少条是后端的决定，不是界面参数：那是"别把界面卡住"的那道闸，
    // 交给调用方传就等于没有
    Ok(page_of(day, lines, PAGE_DEFAULT))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{remove_tree, temp_dir};

    #[test]
    fn writes_one_line_per_action_with_actor_and_outcome() {
        let root = temp_dir("audit-root");
        record(
            &root,
            Actor::User,
            "tool.run_command",
            "git status",
            Outcome::Ok,
        )
        .unwrap();
        record_detail(
            &root,
            Actor::Model,
            "tool.write_file",
            "src/main.rs",
            Outcome::Denied,
            Some("用户拒绝".into()),
        )
        .unwrap();
        let lines = read_day(&root, None);
        assert_eq!(lines.len(), 2, "两次动作两行，追加不改写：{lines:?}");
        assert!(
            lines[0].contains("\"actor\":\"user\""),
            "主体要在行里：{}",
            lines[0]
        );
        assert!(
            lines[1].contains("\"outcome\":\"denied\""),
            "结果要在行里：{}",
            lines[1]
        );
        assert!(lines[1].contains("用户拒绝"));
        remove_tree(&root);
    }

    /// 写侧与读侧是同一行 JSON 的两面。字段名对不上时，界面上是一片空，而账本看起来完全正常，
    /// 所以"读得回来"这句话不能由 `read_day` 自己说，得让 `Entry` 真的解出来一次
    #[test]
    fn a_written_line_reads_back_as_a_structured_entry() {
        let root = temp_dir("audit-entry");
        record_detail(
            &root,
            Actor::Scheduler,
            "tool.write_file",
            "src/main.rs",
            Outcome::Denied,
            Some("表上禁止".into()),
        )
        .unwrap();
        let page = page_of(today(), read_day(&root, None), PAGE_DEFAULT);
        assert_eq!(
            page.entries.len(),
            1,
            "写进去的一行读不回来：{:?}",
            page.entries
        );
        assert_eq!(
            page.entries[0].actor,
            Actor::Scheduler,
            "主体在读侧也得认得出是谁干的"
        );
        assert_eq!(page.entries[0].outcome, Outcome::Denied);
        assert_eq!(page.entries[0].detail.as_deref(), Some("表上禁止"));
        assert_eq!(page.skipped, 0, "好行不该被算成读不懂");
        crate::test_support::assert_matches_ts(
            &serde_json::to_value(&page.entries[0]).expect("Entry 总能编码"),
            "AuditEntry",
        );
        crate::test_support::assert_matches_ts(
            &serde_json::to_value(&page).expect("Page 总能编码"),
            "AuditPage",
        );
        remove_tree(&root);
    }

    /// 坏行要跳过——但**跳过这件事必须说出来**。静默少几行与"那几件事没发生过"在审计里
    /// 是两件事：这正是当初两笔动作被粘成一行时的老毛病，不能让读侧再犯一次
    #[test]
    fn a_line_that_cannot_be_read_is_counted_not_swallowed() {
        let good = r#"{"at":"今天","actor":"user","action":"tool.run_command","target":"ls","outcome":"ok"}"#;
        let page = page_of(
            "2026-09-26".into(),
            vec![good.into(), "{一条被粘坏的行".into(), good.into()],
            10,
        );
        assert_eq!(page.entries.len(), 2);
        assert_eq!(page.skipped, 1, "读不懂的那一行得报个数");
        assert!(!page.truncated, "没截断就别报截断");
    }

    /// 一天可以有很多行。留最新那一截，并明说"这一页被截断了"——
    /// 一声不响地只显示 200 条，等于让用户以为那就是全部
    #[test]
    fn the_page_keeps_the_newest_and_says_it_truncated() {
        let lines = |count: usize| -> Vec<String> {
            (0..count)
                .map(|index| {
                    format!(r#"{{"at":"第{index}条","actor":"model","action":"a","target":"t","outcome":"ok"}}"#)
                })
                .collect()
        };
        let page = page_of("d".into(), lines(5), 2);
        assert!(page.truncated);
        assert_eq!(page.entries.len(), 2);
        assert_eq!(
            page.entries[0].at, "第3条",
            "该留最新那两行：{:?}",
            page.entries
        );
        let whole = page_of("d".into(), lines(5), 10);
        assert!(!whole.truncated, "没截着就别报截断");
        assert_eq!(whole.entries.len(), 5);
    }

    /// 账上出现的每个主体与每种结果都得有中文读数：审计页是给人翻的，而一个英文原词摆在
    /// 一整片中文里，看起来就像"这一类没翻译 = 这一类不会发生"
    #[test]
    fn every_actor_and_outcome_has_a_chinese_label() {
        let audit = include_str!("audit.rs").replace('\r', "");
        let labels = include_str!("../../src/lib/chat-transport.ts").replace('\r', "");
        for (enum_head, table_head, expected) in [
            (
                "pub enum Actor {",
                "AUDIT_ACTOR_LABELS: Record<AuditActor, string> = {",
                5,
            ),
            (
                "pub enum Outcome {",
                "AUDIT_OUTCOME_LABELS: Record<AuditOutcome, string> = {",
                4,
            ),
        ] {
            let body = audit
                .split(enum_head)
                .nth(1)
                .unwrap_or_default()
                .split("\n}")
                .next()
                .unwrap_or_default();
            let variants: Vec<String> = body
                .lines()
                .map(str::trim)
                .filter(|line| !line.starts_with("//") && !line.starts_with('#'))
                .filter_map(|line| line.split(['{', '(', ',', ' ']).next().map(String::from))
                .filter(|name| name.chars().next().is_some_and(|c| c.is_ascii_uppercase()))
                .collect();
            assert_eq!(
                variants.len(),
                expected,
                "{enum_head} 变了（{variants:?}）：标签表与 TS 联合类型要一起改"
            );
            let table = labels
                .split(table_head)
                .nth(1)
                .unwrap_or_else(|| panic!("{table_head} 这张表没找到"))
                .split("};")
                .next()
                .unwrap_or_default();
            for variant in &variants {
                let key = variant.to_ascii_lowercase();
                assert!(table.contains(&format!("{key}:")), "{variant} 没有中文读数");
            }
        }
    }

    /// 命令注册了却没人调 = 那本账还是只有机器看得见。这条链四段一起钉：
    /// `lib.rs` 注册 → transport 的 wrapper（含入参名）→ 设置页真的调 → 三个读数都被显示出来
    #[test]
    fn the_audit_view_is_reachable_from_the_settings_page() {
        let lib = include_str!("lib.rs");
        assert!(
            lib.contains("audit::audit_view,"),
            "命令没注册：设置页那一点只会报错，而报错的样子像\"没有记录\""
        );
        let client = include_str!("../../src/lib/chat-transport.ts").replace('\r', "");
        assert!(
            client.contains("invoke<AuditPage>(\"audit_view\", { date:"),
            "wrapper 缺一个、命令名飘了，或者入参名与 Rust 的 `date` 对不上"
        );
        // 审计账本住「审计中心」页（D10 把安全组拆成六页时从安全概览里搬出来的那份）
        let page = include_str!("../../src/components/audit-settings.tsx").replace('\r', "");
        for read in [
            "auditView(auditDate)",
            "AUDIT_ACTOR_LABELS[entry.actor]",
            "AUDIT_OUTCOME_LABELS[entry.outcome]",
            "auditPage.skipped",
            "auditPage.truncated",
            "auditPage.entries.length",
        ] {
            assert!(page.contains(read), "设置页少了这一处读数：{read}");
        }
    }

    #[test]
    fn refuses_to_log_sensitive_detail() {
        let root = temp_dir("audit-secret");
        record_detail(
            &root,
            Actor::Model,
            "tool.run_command",
            "curl",
            Outcome::Blocked,
            Some("参数含 password=hunter2abcdefgh".into()),
        )
        .unwrap();
        let lines = read_day(&root, None);
        assert_eq!(lines.len(), 1);
        assert!(
            !lines[0].contains("hunter2"),
            "带凭据的 detail 不许落进审计：{}",
            lines[0]
        );
        assert!(
            !lines[0].contains("\"detail\""),
            "既然不留正文，字段就该整个缺席：{}",
            lines[0]
        );
        remove_tree(&root);
    }

    /// 并发的追加不许把两条记录粘在同一行上。这不是假想的坑：`writeln!` 分两次写
    /// （正文、换行符），另一个线程正好挤在中间就会粘——它在本仓库自己的测试跑动时
    /// 真出现过一次。所以这条守卫是概率性的：它不保证每次都抓到，但抓到那次是真的
    #[test]
    fn concurrent_appends_never_share_a_line() {
        let root = temp_dir("audit-concurrent");
        let writers: Vec<_> = (0..8)
            .map(|index| {
                let root = root.clone();
                std::thread::spawn(move || {
                    for shift in 0..25 {
                        record(
                            &root,
                            Actor::Model,
                            &format!("tool:{index}"),
                            &format!("目标 {index}-{shift}"),
                            Outcome::Ok,
                        )
                        .expect("追加该成功");
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().ok();
        }

        let lines = read_day(&root, None);
        assert_eq!(
            lines.len(),
            8 * 25,
            "一条记录占一行，粘住就会少一行：读到 {} 行",
            lines.len()
        );
        for line in &lines {
            assert!(
                serde_json::from_str::<serde_json::Value>(line).is_ok(),
                "有一行不是完整 JSON：{line}"
            );
        }
        remove_tree(&root);
    }

    #[test]
    fn rotates_aged_shards_without_deleting_them() {
        let root = temp_dir("audit-rotate");
        let dir = root.join(AUDIT_DIR);
        fs::create_dir_all(&dir).unwrap();
        let old = dir.join(shard_name("2020-01-01"));
        fs::write(&old, "{\"at\":\"x\"}\n").unwrap();
        let today_path = current_shard(&root).unwrap();
        fs::write(&today_path, "{\"at\":\"y\"}\n").unwrap();

        let moved = rotate(&root, RETENTION_DAYS).unwrap();
        assert_eq!(moved.len(), 1, "只有过期的那一片该被搬走：{moved:?}");
        assert!(!old.exists(), "原位置不该还有文件");
        assert!(
            moved[0].starts_with(dir.join(ARCHIVE_DIR)),
            "要搬进 archive，不是删掉：{}",
            moved[0].display()
        );
        assert!(today_path.exists(), "当天的片子绝不动");
        assert_eq!(
            read_day(&root, Some("2020-01-01")).len(),
            1,
            "归档了也还读得到"
        );
        remove_tree(&root);
    }

    #[test]
    fn reports_write_failures_instead_of_swallowing_them() {
        // 拿一个"父路径是普通文件"的位置当审计根：create_dir_all 必然失败，
        // 而失败必须报出去——静默成功的审计等于没有审计
        let root = temp_dir("audit-blocked");
        let blocker = root.join("blocker");
        fs::write(&blocker, "我不是目录").unwrap();
        let error = record(
            &blocker,
            Actor::User,
            "config.set",
            "permission",
            Outcome::Ok,
        )
        .expect_err("写不进去必须报错，不能静默成功");
        assert!(error.contains("失败"), "报错要说清是哪一步：{error}");
        remove_tree(&root);
    }
}
