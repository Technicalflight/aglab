use base64::Engine as _;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tauri::AppHandle;
use tauri::Manager;

/// 单次快照的正文上限。超过就不存，并如实说明原因——悄悄截断一份"能回滚"的假象更坏
const MAX_SNAPSHOT_BYTES: u64 = 2_000_000;
/// 整个台账里快照的总额度。用满之后新记录只记数字，不再存正文
const MAX_SNAPSHOT_BUDGET: u64 = 32_000_000;
/// LCS 的工作量上限。超过就退回粗算并标 approximate，不假装数得准
const MAX_LCS_CELLS: usize = 4_000_000;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EditRecord {
    pub seq: u64,
    pub conversation_id: String,
    /// 哪一次工具调用写的。对话流里那张汇总卡靠它对上本轮
    pub call_id: String,
    /// 相对工作目录根的展示路径
    pub path: String,
    pub abs_path: String,
    pub at: i64,
    pub additions: u32,
    pub deletions: u32,
    /// 改动大到退回粗算时为真，界面要说"约"
    pub approximate: bool,
    pub bytes_before: u64,
    pub bytes_after: u64,
    /// 写完那一刻的指纹。当前文件对不上它，说明后来又被改过（人或别的工具）
    pub hash_after: String,
    /// 只有本次话题第一次写这个文件时才存正文，见模块注释
    pub snapshot: Option<String>,
    /// 没存快照时给用户的说法；有快照时是空串
    pub snapshot_note: String,
    /// 动手前存了一份可恢复的备份副本（design-security-center.md D3）。
    /// 快照管 diff，备份管恢复——两个词不是同义词，各记各的账
    pub backup: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Ledger {
    pub seq: u64,
    pub records: Vec<EditRecord>,
}

/// 一个文件在本次话题里的净形状。数字是"每次写入相对上一次"的累计，
/// 不是"相对话题开始"——后者要在有 git 的工作目录去变更请求页看
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEdit {
    pub path: String,
    pub abs_path: String,
    pub writes: u32,
    pub additions: u32,
    pub deletions: u32,
    pub approximate: bool,
    pub last_at: i64,
    pub call_ids: Vec<String>,
    pub rollbackable: bool,
    /// 不能回滚的原因，可直接显示
    pub reason: String,
    /// 文件在 aglab 写完之后又被别处改过，回滚会连带覆盖掉那部分
    pub drifted: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EditPreview {
    pub path: String,
    /// "text" | "image" | "binary"。只回答"这份内容能不能直接读"，
    /// 用哪种渲染器由前端按扩展名决定——渲染策略住在看得懂它的那一侧
    pub kind: String,
    /// 文本类正文（html / svg 也走这里，它们本质是文本）
    pub content: String,
    /// 图片的 mime
    pub mime: String,
    /// 图片的 base64（不含 data URL 前缀）
    pub data: String,
    pub bytes: u64,
    /// 正文过长，只给了前面一段
    pub clipped: bool,
    /// 无法预览时给用户的说法
    pub note: String,
}

fn ledger_path(app: &AppHandle) -> Result<PathBuf, String> {
    let root = app.path().app_data_dir().map_err(|e| e.to_string())?;
    Ok(root.join("edits.json"))
}

fn load_ledger(app: &AppHandle) -> Ledger {
    fs::read_to_string(ledger_path(app).unwrap_or_default())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// 一次话题动过哪些文件、有没有留下快照。
///
/// **这不是"能不能回"的最终判定**：那一步要看文件此刻的字节，只在真要回滚时判
/// （[`plan_revert`]）。这里只回答设计里那半句——"这一步动过哪些文件、能不能回到那一份"
/// 里的"动过哪些"与"有没有那一份"
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EditTally {
    pub files: std::collections::BTreeSet<String>,
    pub snapshotted: bool,
}

/// 按话题分组的改动账。编排面板每一格都要这一格，而它是**一次状态刷新读一次盘**，
/// 不是每个节点读一次：一份 64 格的计划会把一次刷新变成 64 次全文件读
pub fn edit_tallies(app: &AppHandle) -> std::collections::BTreeMap<String, EditTally> {
    let mut grouped: std::collections::BTreeMap<String, EditTally> = std::collections::BTreeMap::new();
    for record in load_ledger(app).records {
        let entry = grouped.entry(record.conversation_id).or_default();
        entry.files.insert(record.abs_path);
        entry.snapshotted |= record.snapshot.is_some();
    }
    grouped
}

fn save_ledger(app: &AppHandle, ledger: &Ledger) -> Result<(), String> {
    let path = ledger_path(app)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建数据目录失败：{e}"))?;
    }
    let text = serde_json::to_string(ledger).map_err(|e| e.to_string())?;
    fs::write(&path, text).map_err(|e| format!("写入编辑台账失败：{e}"))
}

fn fingerprint(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 行级差量。先掐公共前后缀，只对中间做 LCS——真实编辑几乎都是局部的，掐完
/// 剩下的规模通常几十行。中间部分仍然大到离谱时退回粗算并标 approximate，
/// 而不是给一个看起来精确的错数
fn count_line_diff(old: &str, new: &str) -> (u32, u32, bool) {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();

    let mut prefix = 0usize;
    while prefix < a.len() && prefix < b.len() && a[prefix] == b[prefix] {
        prefix += 1;
    }
    let mut suffix = 0usize;
    while suffix < a.len() - prefix && suffix < b.len() - prefix
        && a[a.len() - 1 - suffix] == b[b.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let mid_a = &a[prefix..a.len() - suffix];
    let mid_b = &b[prefix..b.len() - suffix];

    match lcs_len(mid_a, mid_b) {
        Some(common) => (
            (mid_b.len() - common) as u32,
            (mid_a.len() - common) as u32,
            false,
        ),
        None => (mid_b.len() as u32, mid_a.len() as u32, true),
    }
}

/// 最长公共子序列长度。滚动数组，只留两行。超预算返回 None
fn lcs_len(a: &[&str], b: &[&str]) -> Option<usize> {
    if a.is_empty() || b.is_empty() {
        return Some(0);
    }
    if a.len().checked_mul(b.len()).is_none_or(|cells| cells > MAX_LCS_CELLS) {
        return None;
    }

    let mut previous: Vec<usize> = vec![0; b.len() + 1];
    let mut current: Vec<usize> = vec![0; b.len() + 1];

    for value_a in a {
        for (index_b, value_b) in b.iter().enumerate() {
            current[index_b + 1] = if value_a == value_b {
                previous[index_b] + 1
            } else {
                previous[index_b + 1].max(current[index_b])
            };
        }
        std::mem::swap(&mut previous, &mut current);
        current.iter_mut().for_each(|cell| *cell = 0);
    }

    Some(previous[b.len()])
}

/// 写之前调用。返回的东西只在写成功后才落账——写失败了文件根本没动，
/// 记一条就是在记假账
pub struct PendingEdit {
    record: EditRecord,
}

/// 这一次要不要为"之前的样子"存一份副本，以及不存的时候为什么。四道判据一处写完：
/// `already` 那一格说的是本次话题此前已经为这个文件存过——第二次写再存，存下去的是
/// "上一版"而不是"最初那一版"，回滚就不再回到最初（所以台账里同一文件的多份快照
/// 永远只有第一份有意义，聚合那侧的判据是"只留第一份"）。
/// 两条上限是同一族决定：**宁可明写着回滚不了，也不悄悄占满用户的盘**
fn snapshot_choice(
    already: bool,
    old_text: Option<&str>,
    bytes_before: u64,
    used: u64,
) -> (Option<String>, String) {
    if already {
        return (None, String::new());
    }
    match old_text {
        // 新建的文件没有"之前的样子"，回滚只能变成删文件——那是另一回事，不做
        None => (None, "这是 aglab 新建的文件，回滚等于删掉它，aglab 不代删。".into()),
        Some(_) if bytes_before > MAX_SNAPSHOT_BYTES => {
            (None, "原文件超过 2 MB，没有存回滚用的副本。".into())
        }
        Some(text) if used + text.len() as u64 > MAX_SNAPSHOT_BUDGET => {
            (None, "回滚快照的额度已用满，这次没有存副本。".into())
        }
        Some(text) => (Some(text.to_string()), String::new()),
    }
}

/// 这次要不要跳过备份（D3 的去重判据）：上一条记录为这份文件记过备份、且它的
/// hash_after 等于动手前的指纹——内容自那以后没变过，那份副本就还是这一份。
/// `before_fingerprint` 为 `None`（新文件）时返回 false：没有旧内容谈不上"没变"，
/// 调用方对 `None` 本来就不会去备份
fn should_skip_backup(
    records: &[EditRecord],
    abs_path: &str,
    before_fingerprint: Option<&str>,
) -> bool {
    let (Some(fingerprint), Some(last)) = (
        before_fingerprint,
        records.iter().rev().find(|record| record.abs_path == abs_path),
    ) else {
        return false;
    };
    last.backup && last.hash_after == fingerprint
}

/// 动手前把旧正文取走：面板要报行数，回滚要靠它。write_file 与 edit_file 都走这里——
/// "打算写成什么"由 tools::planned_content 统一回答（edit_file 的替换在那一刻就校验过，
/// 校验不过 = 文件没动 = 不落账，落一条就是在记假账）
pub fn snapshot_before(
    app: &AppHandle,
    conversation_id: &str,
    call_id: &str,
    name: &str,
    args: &Value,
    root: Option<&Path>,
    backup: crate::backup::Options,
) -> Option<PendingEdit> {
    let (target, content) = match crate::tools::planned_content(name, args, root) {
        Some(value) => value,
        None => return None,
    };
    let raw = args.get("path").and_then(Value::as_str).unwrap_or("");
    let display = crate::tools::write_target_display(raw, &target, root);

    let before = fs::read(&target).ok();
    let bytes_before = before.as_ref().map_or(0, |bytes| bytes.len() as u64);
    let old_text = before.as_ref().and_then(|bytes| String::from_utf8(bytes.clone()).ok());

    let (additions, deletions, approximate) = match &old_text {
        Some(old) => count_line_diff(old, &content),
        // 新文件：整份都是新增
        None => (content.lines().count() as u32, 0, false),
    };

    let ledger = load_ledger(app);
    let already_snapshotted = ledger.records.iter().any(|record| {
        record.conversation_id == conversation_id
            && record.abs_path == target.to_string_lossy()
            && record.snapshot.is_some()
    });
    let used = ledger
        .records
        .iter()
        .filter_map(|record| record.snapshot.as_ref())
        .map(|text| text.len() as u64)
        .sum::<u64>();

    let (snapshot, snapshot_note) =
        snapshot_choice(already_snapshotted, old_text.as_deref(), bytes_before, used);

    // 备份（D3）：动手前存一份可恢复的副本。快照只保"最初那一版"，备份保"这一改
    // 之前的那一版"——去重判据：上一条记录备份过且内容没变，就不重复存
    let before_fingerprint = before.as_ref().map(|bytes| fingerprint(bytes));
    let backup_copied = match &before {
        Some(bytes) if !should_skip_backup(&ledger.records, &target.to_string_lossy(), before_fingerprint.as_deref()) => {
            crate::backup::store(app, conversation_id, &target, bytes, backup).is_some()
        }
        _ => false,
    };

    Some(PendingEdit {
        record: EditRecord {
            seq: 0,
            conversation_id: conversation_id.to_string(),
            call_id: call_id.to_string(),
            path: display,
            abs_path: target.to_string_lossy().to_string(),
            at: now_unix(),
            additions,
            deletions,
            approximate,
            bytes_before,
            bytes_after: content.as_bytes().len() as u64,
            hash_after: fingerprint(content.as_bytes()),
            snapshot,
            snapshot_note,
            backup: backup_copied,
        },
    })
}

/// 删除一等操作的预记（design-security-center.md D1）：每个路径一条 PendingEdit，
/// `additions=0 / deletions=原行数 / bytes_after=0`——界面上一眼认出这是"整份没了"。
/// 只在**落账时**核对存在性（`commit_deleted`），所以这里的记录是"打算删"，不是账
pub fn snapshot_delete_before(
    app: &AppHandle,
    conversation_id: &str,
    call_id: &str,
    args: &Value,
    root: Option<&Path>,
    backup: crate::backup::Options,
) -> Vec<PendingEdit> {
    let raws = args
        .get("paths")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    if raws.is_empty() {
        return Vec::new();
    }
    let ledger = load_ledger(app);
    let used = ledger
        .records
        .iter()
        .filter_map(|record| record.snapshot.as_ref())
        .map(|text| text.len() as u64)
        .sum::<u64>();
    let mut pending = Vec::new();
    for raw in raws {
        let target = crate::tools::resolve(raw, root);
        let display = crate::tools::write_target_display(raw, &target, root);
        let before = fs::read(&target).ok();
        let bytes_before = before.as_ref().map_or(0, |bytes| bytes.len() as u64);
        let old_text = before.as_ref().and_then(|bytes| String::from_utf8(bytes.clone()).ok());
        let already = ledger.records.iter().any(|record| {
            record.conversation_id == conversation_id
                && record.abs_path == target.to_string_lossy()
                && record.snapshot.is_some()
        });
        let (snapshot, snapshot_note) =
            snapshot_choice(already, old_text.as_deref(), bytes_before, used);
        let deletions = old_text.as_ref().map_or(0, |text| text.lines().count() as u32);
        // 删除也先备份（D3）：回收站兜一时，备份兜"回收站被清/被组策略禁用"那一手
        let backup_copied = match &before {
            Some(bytes)
                if !should_skip_backup(
                    &ledger.records,
                    &target.to_string_lossy(),
                    before.as_ref().map(|b| fingerprint(b)).as_deref(),
                ) =>
            {
                crate::backup::store(app, conversation_id, &target, bytes, backup).is_some()
            }
            _ => false,
        };
        pending.push(PendingEdit {
            record: EditRecord {
                seq: 0,
                conversation_id: conversation_id.to_string(),
                call_id: call_id.to_string(),
                path: display,
                abs_path: target.to_string_lossy().to_string(),
                at: now_unix(),
                additions: 0,
                deletions,
                approximate: false,
                bytes_before,
                bytes_after: 0,
                hash_after: String::new(),
                snapshot,
                snapshot_note,
                backup: backup_copied,
            },
        });
    }
    pending
}

/// 删除的落账（design-security-center.md D1）：逐条核对"路径现在还在不在"——
/// 只把真的没了的那几条记进台账。delete_file 是逐路径尽力而为的，混着失败
/// 是常态；在动手前预判谁会失败是猜，落账时核对存在性才是账实相符。
/// 回收站里找得回来这件事，快照与审计各记各的
pub fn commit_deleted(app: &AppHandle, pending: Vec<PendingEdit>) {
    if pending.is_empty() {
        return;
    }
    let mut ledger = load_ledger(app);
    for item in pending {
        if Path::new(&item.record.abs_path).exists() {
            continue;
        }
        ledger.seq += 1;
        let mut record = item.record;
        record.seq = ledger.seq;
        ledger.records.push(record);
    }
    let _ = save_ledger(app, &ledger);
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs() as i64)
        .unwrap_or(0)
}

/// 按文件聚合本次话题的编辑。顺序按首次被写的先后，不是字母序——
/// "模型先动了哪个文件"本身就是用户要看的信息。
/// `read` 把"读文件"这个副作用抽出去，漂移判定才能不带 AppHandle 被测
fn file_edits_from(
    ledger: &Ledger,
    conversation_id: &str,
    read: &dyn Fn(&str) -> Option<Vec<u8>>,
) -> Vec<FileEdit> {
    let mut order: Vec<String> = Vec::new();
    let mut grouped: BTreeMap<String, FileEdit> = BTreeMap::new();

    for record in ledger
        .records
        .iter()
        .filter(|record| record.conversation_id == conversation_id)
    {
        if !grouped.contains_key(&record.abs_path) {
            order.push(record.abs_path.clone());
            grouped.insert(
                record.abs_path.clone(),
                FileEdit {
                    path: record.path.clone(),
                    abs_path: record.abs_path.clone(),
                    writes: 0,
                    additions: 0,
                    deletions: 0,
                    approximate: false,
                    last_at: record.at,
                    call_ids: Vec::new(),
                    rollbackable: false,
                    reason: String::new(),
                    drifted: false,
                },
            );
        }
        let entry = grouped.get_mut(&record.abs_path).expect("inserted above");
        entry.writes += 1;
        entry.additions += record.additions;
        entry.deletions += record.deletions;
        entry.approximate |= record.approximate;
        entry.last_at = record.at;
        entry.call_ids.push(record.call_id.clone());
    }

    for entry in grouped.values_mut() {
        let snapshot = ledger
            .records
            .iter()
            .find(|record| record.abs_path == entry.abs_path && record.snapshot.is_some());
        let last = ledger
            .records
            .iter()
            .rev()
            .find(|record| record.abs_path == entry.abs_path)
            .expect("grouped from at least one record");

        let current = read(&last.abs_path);
        entry.drifted = match &current {
            // 文件已经不在了，也算漂移：回滚要建的是一份"当前没有"的东西
            None => last.bytes_after > 0,
            Some(bytes) => fingerprint(bytes) != last.hash_after,
        };

        match snapshot {
            Some(_) if entry.drifted => {
                entry.reason = "文件在 aglab 之后又被改过，回滚会连带覆盖那部分改动。".into();
            }
            Some(_) => entry.rollbackable = true,
            None => {
                entry.reason = if last.snapshot_note.is_empty() {
                    "没有存回滚用的副本。".into()
                } else {
                    last.snapshot_note.clone()
                };
            }
        }
    }

    order
        .into_iter()
        .filter_map(|key| grouped.remove(&key))
        .collect()
}

pub fn file_edits(app: &AppHandle, conversation_id: &str) -> Vec<FileEdit> {
    file_edits_from(&load_ledger(app), conversation_id, &|path| {
        fs::read(path).ok()
    })
}

/// 回滚前唯一的裁判：能不能回、恢复到哪份内容。`current` 是文件此刻的字节，
/// None 表示文件已经没了。把它做成纯函数，是因为这一步判错会真的吃掉用户改动
fn plan_revert(
    ledger: &Ledger,
    conversation_id: &str,
    abs_path: &str,
    current: Option<&[u8]>,
) -> Result<String, String> {
    let source = ledger
        .records
        .iter()
        .find(|record| {
            record.conversation_id == conversation_id
                && record.abs_path == abs_path
                && record.snapshot.is_some()
        })
        .ok_or("这个文件没有可回滚的快照。")?;
    let snapshot = source.snapshot.clone().expect("checked above");

    let last = ledger
        .records
        .iter()
        .rev()
        .find(|record| record.abs_path == abs_path)
        .expect("source implies at least one record");

    let drifted = match current {
        // 文件已经不在了，也算漂移：回滚要建的是一份"当前没有"的东西
        None => last.bytes_after > 0,
        Some(bytes) => fingerprint(bytes) != last.hash_after,
    };
    if drifted {
        return Err(
            "文件在 aglab 写完之后又被改过，回滚会连带覆盖那部分改动。请先自己确认那份改动。"
                .into(),
        );
    }

    Ok(snapshot)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RevertOutcome {
    pub path: String,
    pub restored_bytes: u64,
    /// 回滚本身也算一次改动，记进台账，避免面板上还挂着旧的数字
    pub seq: u64,
}

/// 把文件恢复到本次话题里第一次被写之前的样子。漂移时一律拒绝，
/// 不替用户猜"那部分改动要不要留"
pub fn revert(app: &AppHandle, conversation_id: &str, abs_path: &str) -> Result<RevertOutcome, String> {
    let ledger = load_ledger(app);
    let current = fs::read(abs_path).ok();
    let snapshot = plan_revert(&ledger, conversation_id, abs_path, current.as_deref())?;
    let source = ledger
        .records
        .iter()
        .find(|record| {
            record.conversation_id == conversation_id
                && record.abs_path == abs_path
                && record.snapshot.is_some()
        })
        .expect("plan_revert 只在有快照时才放行");

    let path = Path::new(abs_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("恢复目录失败：{e}"))?;
    }
    fs::write(path, &snapshot).map_err(|e| format!("回滚写入失败：{e}"))?;

    let mut ledger = load_ledger(app);
    ledger.seq += 1;
    let seq = ledger.seq;
    let (additions, deletions, approximate) =
        match current.as_ref().and_then(|bytes| String::from_utf8(bytes.clone()).ok()) {
            Some(text) => count_line_diff(&text, &snapshot),
            None => (snapshot.lines().count() as u32, 0, false),
        };
    ledger.records.push(EditRecord {
        seq,
        conversation_id: conversation_id.to_string(),
        call_id: String::new(),
        path: source.path.clone(),
        abs_path: abs_path.to_string(),
        at: now_unix(),
        additions,
        deletions,
        approximate,
        bytes_before: current.as_ref().map_or(0, |bytes| bytes.len() as u64),
        bytes_after: snapshot.len() as u64,
        hash_after: fingerprint(snapshot.as_bytes()),
        // 回滚之后文件已经就是那份快照，再存一遍只是把磁盘翻倍，换不来任何能力
        snapshot: None,
        snapshot_note: "这是一次回滚，不是模型的编辑。".into(),
        // backup 的语义是"这条编辑动手前存过备份"：回滚这条记录不挂备份，
        // 回滚的对象（被恢复成的那份快照）本身就是它的来路
        backup: false,
    });
    save_ledger(app, &ledger)?;

    Ok(RevertOutcome {
        path: source.path.clone(),
        restored_bytes: snapshot.len() as u64,
        seq,
    })
}

/// 面板内预览。只给本次话题里被 aglab 写过的文件，且必须还在工作目录内
/// 按扩展名认图片。svg 故意不在这里——它是文本，交给前端编成 data URL，
/// 那样 svg 里的脚本不会执行（放进 <img> 也不会执行，放进 innerHTML 就会）
fn image_mime(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        _ => return None,
    })
}

pub fn preview(app: &AppHandle, abs_path: &str, conversation_id: &str) -> Result<EditPreview, String> {
    let ledger = load_ledger(app);
    if !ledger
        .records
        .iter()
        .any(|record| record.conversation_id == conversation_id && record.abs_path == abs_path)
    {
        return Err("这个文件不在本次话题的编辑清单里。".into());
    }

    // 台账本身就是允许清单：这个路径是 aglab 自己写过的，不再叠一层工作目录前缀判断
    const TEXT_CLIP: usize = 400_000;
    const MAX_IMAGE_BYTES: usize = 4_000_000;

    let bytes = fs::read(Path::new(abs_path)).map_err(|e| format!("读不到了：{e}"))?;
    let ext = Path::new(abs_path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let size = bytes.len() as u64;

    if let Some(mime) = image_mime(&ext) {
        if bytes.len() > MAX_IMAGE_BYTES {
            return Ok(EditPreview {
                path: abs_path.to_string(),
                kind: "binary".into(),
                content: String::new(),
                mime: String::new(),
                data: String::new(),
                bytes: size,
                clipped: false,
                note: "图片超过 4 MB，没有内联显示。".into(),
            });
        }
        return Ok(EditPreview {
            path: abs_path.to_string(),
            kind: "image".into(),
            content: String::new(),
            mime: mime.to_string(),
            data: base64::engine::general_purpose::STANDARD.encode(&bytes),
            bytes: size,
            clipped: false,
            note: String::new(),
        });
    }

    if bytes[..bytes.len().min(crate::review::SNIFF_BYTES)].contains(&0) {
        return Ok(EditPreview {
            path: abs_path.to_string(),
            kind: "binary".into(),
            content: String::new(),
            mime: String::new(),
            data: String::new(),
            bytes: size,
            clipped: false,
            note: "二进制文件，没有可预览的正文。".into(),
        });
    }

    let clipped = bytes.len() > TEXT_CLIP;
    let head: Vec<u8> = bytes.iter().copied().take(TEXT_CLIP).collect();
    Ok(EditPreview {
        path: abs_path.to_string(),
        kind: "text".into(),
        content: String::from_utf8_lossy(&head).to_string(),
        mime: String::new(),
        data: String::new(),
        bytes: size,
        clipped,
        note: String::new(),
    })
}

#[tauri::command]
pub fn edits_for_session(app: AppHandle, conversation_id: String) -> Vec<FileEdit> {
    file_edits(&app, &conversation_id)
}

#[tauri::command]
pub fn edit_revert(
    app: AppHandle,
    conversation_id: String,
    path: String,
) -> Result<RevertOutcome, String> {
    revert(&app, &conversation_id, &path)
}

#[tauri::command]
pub fn edit_preview(
    app: AppHandle,
    conversation_id: String,
    path: String,
) -> Result<EditPreview, String> {
    preview(&app, &path, &conversation_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_a_local_edit_without_blaming_the_whole_file() {
        let old = "a\nb\nc\nd\ne\n";
        let new = "a\nb\nC\nd\ne\n";
        assert_eq!(count_line_diff(old, new), (1, 1, false));
    }

    #[test]
    fn counts_pure_insertion_and_deletion() {
        assert_eq!(count_line_diff("a\n", "a\nb\nc\n"), (2, 0, false));
        assert_eq!(count_line_diff("a\nb\nc\n", "a\n"), (0, 2, false));
        assert_eq!(count_line_diff("", "x\ny\n"), (2, 0, false));
    }

    #[test]
    fn keeps_counting_when_the_middle_is_too_big_for_lcs() {
        let old: String = (0..3000).map(|i| format!("old {i}\n")).collect();
        let new: String = (0..3000).map(|i| format!("new {i}\n")).collect();
        let (additions, deletions, approximate) = count_line_diff(&old, &new);
        assert!(approximate, "中间部分超出 LCS 预算时必须承认是粗算");
        assert_eq!((additions, deletions), (3000, 3000));
    }

    #[test]
    fn prefix_and_suffix_trim_stops_interleaved_edits_from_undercounting() {
        // 每行都改：掐完前后缀剩整个中间段，行数要等于文件长度而不是 0
        let old = "1\n2\n3\n4\n";
        let new = "a\nb\nc\nd\n";
        assert_eq!(count_line_diff(old, new), (4, 4, false));
    }

    #[test]
    fn fingerprint_changes_with_content_only() {
        assert_eq!(fingerprint(b"abc"), fingerprint(b"abc"));
        assert_ne!(fingerprint(b"abc"), fingerprint(b"abd"));
    }

    /// 备份去重（D3）：上一条为这份文件记过备份、且它的 hash_after 等于动手前的
    /// 指纹，内容就没变过——那份副本还作数。换个文件、内容变了、或上一条压根
    /// 没备份成功，都要重新备份
    #[test]
    fn backup_dedup_only_skips_when_the_previous_copy_still_holds_the_content() {
        let same = "v1";
        let with_backup = record(1, "/p/a.txt", "v2", Some("old"));
        let mut prev = with_backup.clone();
        prev.backup = true;
        prev.hash_after = fingerprint(same.as_bytes());

        assert!(
            !should_skip_backup(&[prev.clone()], "/p/a.txt", Some(&fingerprint(b"v2-changed"))),
            "内容变了就要重新备份"
        );
        assert!(
            should_skip_backup(&[prev.clone()], "/p/a.txt", Some(&fingerprint(same.as_bytes()))),
            "内容没变就复用上一份副本"
        );
        let no_backup = record(2, "/p/a.txt", "v3", None);
        assert!(
            !should_skip_backup(&[no_backup], "/p/a.txt", Some(&fingerprint(same.as_bytes()))),
            "上一条没备份成功，这次不能装作有"
        );
        assert!(
            !should_skip_backup(&[prev], "/p/other.txt", Some(&fingerprint(same.as_bytes()))),
            "去重只对同一份文件成立"
        );
        assert!(
            !should_skip_backup(&[], "/p/a.txt", Some(&fingerprint(same.as_bytes()))),
            "没有可比的上一条，就不该跳"
        );
        assert!(!should_skip_backup(&[record(3, "/p/a.txt", "x", None)], "/p/a.txt", None), "新文件没有动手前");
    }

    fn record(seq: u64, abs: &str, written: &str, snapshot: Option<&str>) -> EditRecord {
        EditRecord {
            seq,
            conversation_id: "conv".into(),
            call_id: format!("call-{seq}"),
            path: abs.rsplit('/').next().unwrap_or(abs).to_string(),
            abs_path: abs.to_string(),
            at: seq as i64,
            additions: 1,
            deletions: 1,
            approximate: false,
            bytes_before: snapshot.map_or(0, |text| text.len() as u64),
            bytes_after: written.len() as u64,
            hash_after: fingerprint(written.as_bytes()),
            snapshot: snapshot.map(String::from),
            snapshot_note: if snapshot.is_some() {
                String::new()
            } else {
                "没有存回滚用的副本。".into()
            },
            backup: false,
        }
    }

    fn ledger_with(records: Vec<EditRecord>) -> Ledger {
        Ledger {
            seq: records.len() as u64,
            records,
        }
    }

    /// 存不存这份副本、不存的时候说哪一句。这一格决定"回滚"那条验收做不做得成，
    /// 而它以前住在 `snapshot_before` 里——要拿着 `AppHandle` 和真文件才测得动
    #[test]
    fn a_snapshot_is_kept_once_per_file_and_only_when_there_is_something_to_go_back_to() {
        // 正对照：第一次写、没超单文件上限、额度没用满 ⇒ 存，而且不给理由
        let (held, note) = snapshot_choice(false, Some("旧内容"), 9, 0);
        assert_eq!(held.as_deref(), Some("旧内容"), "第一次写该留下可回的副本");
        assert_eq!(note, "", "存成了就不该再塞一句解释");

        // 同一个文件的第二次写：既不存也不解释——第一份才是"最初那一版"
        let (again, quiet) = snapshot_choice(true, Some("上一版"), 9, 0);
        assert_eq!(again, None, "第二次写不许把回滚点悄悄挪近一格");
        assert_eq!(quiet, "");

        // 三种存不成的理由，各说各的那一句（少一句，界面就只剩一个光秃秃的"不能回滚"）
        assert_eq!(
            snapshot_choice(false, None, 0, 0),
            (None, "这是 aglab 新建的文件，回滚等于删掉它，aglab 不代删。".into()),
            "新建的文件没有「之前的样子」"
        );
        assert_eq!(
            snapshot_choice(false, Some("小"), MAX_SNAPSHOT_BYTES + 1, 0).1,
            "原文件超过 2 MB，没有存回滚用的副本。",
            "超的是单文件那一格"
        );
        assert_eq!(
            snapshot_choice(false, Some("小"), 3, MAX_SNAPSHOT_BUDGET).1,
            "回滚快照的额度已用满，这次没有存副本。",
            "超的是整本账那一格"
        );
        // 两格上限都验"恰好等于"那一边：判据是"超过"，不是"到"
        assert_eq!(
            snapshot_choice(false, Some("x"), MAX_SNAPSHOT_BYTES, 0).0.as_deref(),
            Some("x"),
            "恰好 2 MB 不算超单文件上限"
        );
        assert_eq!(
            snapshot_choice(false, Some("abc"), 3, MAX_SNAPSHOT_BUDGET - 3).0.as_deref(),
            Some("abc"),
            "刚好用满不该被当成超出"
        );
    }

    #[test]
    fn revert_restores_the_first_snapshot_only_when_nothing_else_touched_the_file() {
        let ledger = ledger_with(vec![
            record(1, "/p/a.txt", "v1\n", Some("v0\n")),
            record(2, "/p/a.txt", "v2\n", None),
        ]);

        assert_eq!(
            plan_revert(&ledger, "conv", "/p/a.txt", Some(b"v2\n")).unwrap(),
            "v0\n"
        );
    }

    #[test]
    fn revert_refuses_when_the_file_changed_since_aglab_wrote_it() {
        let ledger = ledger_with(vec![record(1, "/p/a.txt", "v1\n", Some("v0\n"))]);

        let error = plan_revert(&ledger, "conv", "/p/a.txt", Some(b"edited by hand\n")).unwrap_err();
        assert!(
            error.contains("又被改过"),
            "漂移必须被拒且说清原因，实际：{error}"
        );
    }

    #[test]
    fn revert_treats_a_deleted_file_as_drift_instead_of_silently_recreating_it() {
        let ledger = ledger_with(vec![record(1, "/p/a.txt", "v1\n", Some("v0\n"))]);
        assert!(plan_revert(&ledger, "conv", "/p/a.txt", None).is_err());
    }

    #[test]
    fn revert_refuses_without_a_snapshot_and_ignores_other_conversations() {
        let ledger = ledger_with(vec![record(1, "/p/a.txt", "v1\n", None)]);
        assert!(plan_revert(&ledger, "conv", "/p/a.txt", Some(b"v1\n")).is_err());

        let ledger = ledger_with(vec![record(1, "/p/a.txt", "v1\n", Some("v0\n"))]);
        assert!(plan_revert(&ledger, "other", "/p/a.txt", Some(b"v1\n")).is_err());
    }

    #[test]
    fn aggregates_every_write_of_a_file_but_keeps_only_the_first_snapshot() {
        let ledger = ledger_with(vec![
            record(1, "/p/a.txt", "v1\n", Some("v0\n")),
            record(2, "/p/a.txt", "v2\n", None),
            record(3, "/p/b.txt", "b1\n", Some("b0\n")),
        ]);
        let edits = file_edits_from(&ledger, "conv", &|path| {
            // 两个文件都停在 aglab 最后一次写入的结果上：没有漂移
            Some(if path.ends_with("a.txt") {
                b"v2\n".to_vec()
            } else {
                b"b1\n".to_vec()
            })
        });

        assert_eq!(edits.len(), 2, "按文件聚合，不是按写入次数");
        assert_eq!(edits[0].path, "a.txt");
        assert_eq!(edits[0].writes, 2, "a.txt 被写了两次");
        assert_eq!((edits[0].additions, edits[0].deletions), (2, 2));
        assert!(edits[0].rollbackable);
        assert!(!edits[0].drifted);
        assert_eq!(edits[0].call_ids, vec!["call-1".to_string(), "call-2".to_string()]);
        assert_eq!(edits[1].path, "b.txt");
        assert_eq!(edits[1].writes, 1);
    }

    #[test]
    fn marks_drift_and_keeps_the_reason_visible() {
        let ledger = ledger_with(vec![record(1, "/p/a.txt", "v1\n", Some("v0\n"))]);
        let edits = file_edits_from(&ledger, "conv", &|_| Some(b"hand edited\n".to_vec()));

        assert_eq!(edits.len(), 1);
        assert!(edits[0].drifted);
        assert!(!edits[0].rollbackable, "漂移的文件不能标成可回滚");
        assert!(edits[0].reason.contains("又被改过"));
    }

    #[test]
    fn a_file_without_snapshot_reports_why_instead_of_a_bare_no() {
        let ledger = ledger_with(vec![record(1, "/p/a.txt", "v1\n", None)]);
        let edits = file_edits_from(&ledger, "conv", &|_| Some(b"v1\n".to_vec()));
        assert!(!edits[0].rollbackable);
        assert_eq!(edits[0].reason, "没有存回滚用的副本。");
    }
}
