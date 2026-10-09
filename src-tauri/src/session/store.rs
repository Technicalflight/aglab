//! JSONL 持久层：一行一个事务，第一行是 header。
//!
//! 与参照实现的两处刻意不同，都记在这里：
//! 1. 每次保存都是**整文件重写**（`tmp` + rename），不像它那样首建后逐行 append。
//!    因此撕裂末行不需要"补一个 `\\n`"那种修复——下一次保存就把它带走了；代价是
//!    每次落盘都重写全文，桌面端的话题规模下不值一提。
//! 2. header 里带一条 `tip`：分支末端**落盘**，装载时按它走（[`SessionLog::restore_at`]）。
//!    早先这里是刻意没有 tip 的——跟出货版一致，末端由物理末行重新推导，代价记在
//!    设计档 §3.2："回溯之后没再追加就重开话题"会回到那条被放弃的路径上。
//!    分支树把这条从"可接受的取舍"变成"用户的位置会自己跳走"：树里每一条分支都是要
//!    能停上去的，末端就是浏览位置，而浏览位置不该在重启后被物理顺序重新决定。
//!    旧存档没有这个字段，照旧退回物理末行——不迁移，行为不变。

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::entry::Entry;
use super::log::SessionLog;
use super::valid_id;

/// 我们写出的格式版本。`1` 留给"台账时代"的旧存档，由 Stage 3 的一次性迁移认领
pub const CURRENT_VERSION: u32 = 2;

/// 解析时应当**跳过**而不是报错的条目类别：前向兼容靠它——新版本加的类别不能让旧版本
/// 读不了整个文件
const KNOWN_KINDS: &[&str] = &[
    "message",
    "compaction",
    "context_edit",
    "branch_summary",
    "custom_message",
    "model_change",
    "usage",
    "session_info",
    "custom",
];

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionHeader {
    pub version: u32,
    pub id: String,
    pub timestamp: i64,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
}

impl SessionHeader {
    pub fn new(id: String, timestamp: i64, cwd: String) -> Self {
        Self {
            version: CURRENT_VERSION,
            id,
            timestamp,
            cwd,
            parent_session: None,
        }
    }
}

/// `<root>/--<编码过的 cwd>--/<ISO 时间>_<话题 id>.jsonl`。按工作目录分桶，
/// 换目录开的话题不会混在一起
pub fn path_for(root: &Path, cwd: &str, timestamp: i64, id: &str) -> Result<PathBuf, String> {
    if !valid_id(id) {
        return Err(format!("话题 id 不合法：{id:?}"));
    }
    let bucket = format!("--{}--", encode_cwd(cwd));
    let name = format!("{}_{id}.jsonl", iso_file_stamp(timestamp));
    Ok(root.join(bucket).join(name))
}

fn encode_cwd(cwd: &str) -> String {
    cwd.trim_start_matches(['/', '\\'])
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '.' => '-',
            other => other,
        })
        .collect()
}

fn iso_file_stamp(timestamp: i64) -> String {
    let moment = DateTime::<Utc>::from_timestamp_millis(timestamp)
        .map(|moment| moment.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| timestamp.to_string());
    moment.replace(':', "-")
}

pub fn save(path: &Path, header: &SessionHeader, log: &SessionLog) -> Result<(), String> {
    let mut body = String::new();
    body.push_str(
        &serde_json::to_string(&json!({
            "type": "session",
            "version": header.version,
            "id": header.id,
            "timestamp": header.timestamp,
            "cwd": header.cwd,
            "parentSession": header.parent_session,
            // 末端跟着每一次保存走：它是"用户停在哪一支"这件事的唯一事实
            "tip": log.leaf_id(),
        }))
        .map_err(|error| format!("写 header 失败：{error}"))?,
    );
    body.push('\n');
    for entry in log.entries() {
        body.push_str(
            &serde_json::to_string(entry).map_err(|error| format!("写条目失败：{error}"))?,
        );
        body.push('\n');
    }

    // 先写临时文件再改名：进程中途被杀不会留下半个话题
    let parent = path
        .parent()
        .ok_or_else(|| "话题路径没有父目录".to_string())?;
    fs::create_dir_all(parent).map_err(|error| format!("建话题目录失败：{error}"))?;
    let temp = temp_path(path);
    fs::write(&temp, &body).map_err(|error| format!("写话题临时文件失败：{error}"))?;
    fs::rename(&temp, path).map_err(|error| {
        let _ = fs::remove_file(&temp);
        format!("落话题文件失败：{error}")
    })
}

fn temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{name}.tmp"))
}

/// 找一条话题已经落在哪个文件上。路径里的时间戳来自 header，第二次保存必须写回**同一个**
/// 文件，所以这里按 `_<话题 id>.jsonl` 的后缀扫，而不是重新算一遍名字。
/// 只下探一层分桶目录——布局就是 `<root>/--bucket--/<stamp>_<id>.jsonl`
pub fn find(root: &Path, id: &str) -> Option<PathBuf> {
    if !valid_id(id) {
        return None;
    }
    let wanted = format!("{id}.jsonl");
    let buckets = fs::read_dir(root).ok()?;
    for bucket in buckets.flatten() {
        let Ok(children) = fs::read_dir(bucket.path()) else {
            continue;
        };
        for child in children.flatten() {
            let path = child.path();
            let Some(name) = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
            else {
                continue;
            };
            // 文件名是 `<时间戳>_<话题 id>.jsonl`，而时间戳里没有下划线（`:` 与 `.` 都换成了
            // `-`），所以第一个下划线就是唯一的分界。按它切分而不是比后缀：id 自己带下划线时，
            // 后缀匹配会让 id="a" 命中 "..._conv_a.jsonl"（这条被反例测试抓到了）
            let Some((stamp, rest)) = name.split_once('_') else {
                continue;
            };
            if !stamp.is_empty() && rest == wanted {
                return Some(path);
            }
        }
    }
    None
}

/// 一条已存在话题的落盘位置：沿用 header 里的时间与 cwd，保证多次保存写同一个文件
pub fn path_of(root: &Path, header: &SessionHeader) -> Result<PathBuf, String> {
    path_for(root, &header.cwd, header.timestamp, &header.id)
}

pub fn load(path: &Path) -> Result<(SessionHeader, SessionLog), String> {
    let raw = fs::read_to_string(path).map_err(|error| format!("读话题失败：{error}"))?;
    let mut lines = raw.lines().filter(|line| !line.trim().is_empty());

    let first = lines.next().ok_or_else(|| "话题文件是空的".to_string())?;
    let header_value: Value = serde_json::from_str(first)
        .map_err(|_| "第一个话题文件不是有效的话题记录，保持不动".to_string())?;
    if header_value["type"] != "session" {
        return Err("这个文件不是本应用的话题，保持不动".to_string());
    }
    let header: SessionHeader = serde_json::from_value(rename_header(&header_value))
        .map_err(|error| format!("话题 header 读不动：{error}"))?;
    if header.version > CURRENT_VERSION {
        return Err(format!(
            "话题格式版本 {} 比本应用支持的 {CURRENT_VERSION} 更新",
            header.version
        ));
    }

    let mut entries = Vec::new();
    for line in lines {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            // 撕裂的末行（断电、被杀）：丢掉这一条，下一次整文件重写就把它带走了
            continue;
        };
        let kind = value["type"].as_str().unwrap_or_default().to_string();
        if !KNOWN_KINDS.contains(&kind.as_str()) {
            continue;
        }
        match serde_json::from_value::<Entry>(value) {
            Ok(entry) => entries.push(entry),
            Err(error) => return Err(format!("话题条目（{kind}）读不动：{error}")),
        }
    }

    // tip 不在 SessionHeader 这个类型上：那是身份与谱系，而末端是"停在哪"的可变状态。
    // 从原始 header 里单独读，旧存档读不到就是 None → 照旧由物理末行推导
    let tip = header_value["tip"].as_str().map(str::to_string);
    let log = SessionLog::restore_at(entries, tip.as_deref()).map_err(|error| error.to_string())?;
    Ok((header, log))
}

/// header 在文件里是 camelCase（`parentSession`），内存结构是 snake_case
fn rename_header(value: &Value) -> Value {
    json!({
        "version": value["version"].clone(),
        "id": value["id"].clone(),
        "timestamp": value["timestamp"].clone(),
        "cwd": value["cwd"].clone(),
        "parent_session": value["parentSession"].clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::super::entry::{EntryPayload, Message, NewEntry};
    use super::*;
    use crate::test_support::scoped_temp_dir;

    const T0: i64 = 1_700_000_000_000;

    fn session_log() -> SessionLog {
        let mut log = SessionLog::new();
        log.append(
            NewEntry::new(EntryPayload::Message {
                message: Message::User {
                    content: "第一问".into(),
                    images: Vec::new(),
                    audios: Vec::new(),
                    videos: Vec::new(),
                },
            }),
            T0,
        )
        .expect("追加该成功");
        log.append(
            NewEntry::new(EntryPayload::Message {
                message: Message::Assistant(
                    super::super::entry::PendingAssistant {
                        content: "答复".into(),
                        tool_calls: vec![],
                    }
                    .settle(super::super::entry::StopReason::Stop),
                ),
            }),
            T0,
        )
        .expect("追加该成功");
        log
    }

    /// 分叉出去的话题重启后还得答得出"我从哪来"。这条钉三件事：写出去的键名是
    /// camelCase 的那一个、`rename_header` 把它接回了 Rust 字段、没父的那条读回来是
    /// `None` 而不是报错（`save` 对空格子写的是显式 `null`，所以别指望键会缺席）
    #[test]
    fn a_parent_survives_the_round_trip_and_an_absent_one_reads_back_as_none() {
        let dir = scoped_temp_dir("store-parent");
        let log = session_log();

        let plain_path = path_for(&dir, "C:/work", T0, "conv-plain").expect("路径该合法");
        save(
            &plain_path,
            &SessionHeader::new("conv-plain".into(), T0, "C:/work".into()),
            &log,
        )
        .expect("存无父的该成功");
        let (plain, _) = load(&plain_path).expect("无父的该读得回来");
        assert_eq!(
            plain.parent_session, None,
            "没父就是没父，不该读成空串或报错"
        );

        let mut header = SessionHeader::new("conv-child".into(), T0, "C:/work".into());
        header.parent_session = Some("conv-plain".into());
        let child_path = path_for(&dir, "C:/work", T0, "conv-child").expect("路径该合法");
        save(&child_path, &header, &log).expect("存带父的该成功");
        let raw = fs::read_to_string(&child_path).expect("读原文");
        assert!(
            raw.contains("\"parentSession\":\"conv-plain\""),
            "谱系要真的落到 header 那一行上：{raw}"
        );
        let (revived, _) = load(&child_path).expect("带父的该读得回来");
        assert_eq!(
            revived.parent_session.as_deref(),
            Some("conv-plain"),
            "谱系活过一次写读：分叉出的话题答得出它从哪来"
        );
        assert_eq!(revived.id, "conv-child", "抄父不该动自己的 id");
    }

    /// 分支末端要落盘。早先这里没有 tip：末端由**物理末行**重新推导，于是"回溯到早期节点、
    /// 没再追加任何东西、重启"会让用户发现自己站在那条他放弃的分支上——一行都没错，
    /// 位置却自己跳走了。整文件重写让"跟着保存走一个 tip 字段"几乎免费
    #[test]
    fn a_rewound_tip_survives_a_restart_instead_of_reverting_to_the_abandoned_branch() {
        let dir = scoped_temp_dir("store-tip");
        let path = path_for(&dir, "C:/work", T0, "conv-tip").expect("路径该合法");
        let mut log = session_log(); // a -> b，末端在 b
        let root = log.entries()[0].id.clone();
        let tip = log.entries()[1].id.clone();
        log.append(
            NewEntry::new(EntryPayload::Message {
                message: Message::User {
                    content: "被放弃的那一支".into(),
                    images: Vec::new(),
                    audios: Vec::new(),
                    videos: Vec::new(),
                },
            }),
            T0,
        )
        .expect("追加该成功");
        let abandoned = log.entries()[2].id.clone();
        assert_ne!(
            log.leaf_id(),
            Some(tip.as_str()),
            "前置：末端此刻在新一支上"
        );
        log.navigate(Some(tip.as_str())).expect("回到 b 该成");
        save(
            &path,
            &SessionHeader::new("conv-tip".into(), T0, "C:/work".into()),
            &log,
        )
        .expect("存该成功");

        let raw = fs::read_to_string(&path).expect("读原文");
        assert!(
            raw.contains("\"tip\":\""),
            "末端要真的写进 header 那一行：{raw}"
        );
        let (_, revived) = load(&path).expect("读该成功");
        assert_eq!(
            revived.leaf_id(),
            Some(tip.as_str()),
            "重启后还要停在用户选的那一支上"
        );
        let walked: Vec<&str> = revived
            .path()
            .expect("路径算得出")
            .iter()
            .map(|entry| entry.id.as_str())
            .collect();
        assert_eq!(
            walked,
            vec![root.as_str(), tip.as_str()],
            "被放弃那一支不在路径上"
        );
        assert_eq!(revived.len(), 3, "但一行都没删——它还在树里，还能再走上去");
        assert!(revived.entry(&abandoned).is_some());
    }

    /// tip 是这一版新加的字段，之前的存档没有它。没有就该照旧由物理末行推导——
    /// 老话题的行为一个字节都不变
    #[test]
    fn an_older_file_without_a_tip_still_resumes_the_physical_last_line() {
        let dir = scoped_temp_dir("store-legacy-tip");
        let path = path_for(&dir, "C:/work", T0, "conv-old").expect("路径该合法");
        let log = session_log();
        let last = log.entries().last().expect("有末行").id.clone();
        save(
            &path,
            &SessionHeader::new("conv-old".into(), T0, "C:/work".into()),
            &log,
        )
        .expect("存该成功");
        let needle = format!(",\"tip\":\"{last}\"");
        let raw = fs::read_to_string(&path).expect("读原文");
        assert!(raw.contains(&needle), "header 里该有末端那一段：{raw}");
        fs::write(&path, raw.replace(&needle, "")).expect("抹成旧格式该成功");

        let (_, revived) = load(&path).expect("旧存档该照样读得回来");
        assert_eq!(
            revived.leaf_id(),
            Some(last.as_str()),
            "没有 tip 就退回物理末行"
        );
    }

    /// tip 指着不存在的东西 = 文件被改过。宁可开不了这条话题，也不能悄悄挑一个末端
    /// 接着写——那等于把用户的分支位置交给一次猜
    #[test]
    fn a_tip_pointing_at_nothing_fails_loudly_instead_of_quietly_moving_the_branch() {
        let dir = scoped_temp_dir("store-bad-tip");
        let path = path_for(&dir, "C:/work", T0, "conv-bad").expect("路径该合法");
        let log = session_log();
        let last = log.entries().last().expect("有末行").id.clone();
        save(
            &path,
            &SessionHeader::new("conv-bad".into(), T0, "C:/work".into()),
            &log,
        )
        .expect("存该成功");
        let broken = fs::read_to_string(&path).expect("读原文").replacen(
            &format!("\"tip\":\"{last}\""),
            "\"tip\":\"ghost\"",
            1,
        );
        fs::write(&path, broken).expect("写坏 tip 该成功");

        let error = load(&path).expect_err("指着不存在的末端不能算读成功");
        assert!(error.contains("ghost"), "那句错要说得出是哪个 id：{error}");
    }

    fn round_trips(log: &SessionLog) -> SessionLog {
        let dir = scoped_temp_dir("session-store");
        let path = path_for(&dir, "C:/work/demo", T0, "sess-1").expect("路径该合法");
        save(
            &path,
            &SessionHeader::new("sess-1".into(), T0, "C:/work/demo".into()),
            log,
        )
        .expect("保存该成功");
        let (header, revived) = load(&path).expect("读回该成功");
        assert_eq!(header.id, "sess-1");
        assert_eq!(header.cwd, "C:/work/demo");
        assert_eq!(revived.len(), log.len());
        assert_eq!(revived.leaf_id(), log.leaf_id());
        revived
    }

    #[test]
    fn a_saved_session_round_trips_with_its_sequence_and_parents() {
        let log = session_log();
        let revived = round_trips(&log);
        let original: Vec<(u64, Option<String>)> = log
            .entries()
            .iter()
            .map(|entry| (entry.seq, entry.parent_id.clone()))
            .collect();
        let back: Vec<(u64, Option<String>)> = revived
            .entries()
            .iter()
            .map(|entry| (entry.seq, entry.parent_id.clone()))
            .collect();
        assert_eq!(original, back, "序号与父链必须一字不动地回来");
    }

    /// 末端在装载时由物理末行推导：分支形状能整个活着回来
    #[test]
    fn a_branch_shape_survives_the_round_trip() {
        let mut log = SessionLog::new();
        let first = log
            .append(
                NewEntry::new(EntryPayload::Message {
                    message: Message::User {
                        content: "第一问".into(),
                        images: Vec::new(),
                        audios: Vec::new(),
                        videos: Vec::new(),
                    },
                }),
                T0,
            )
            .expect("追加该成功")
            .id
            .clone();
        log.append(
            NewEntry::new(EntryPayload::Message {
                message: Message::Assistant(
                    super::super::entry::PendingAssistant {
                        content: "旧答复".into(),
                        tool_calls: vec![],
                    }
                    .settle(super::super::entry::StopReason::Aborted),
                ),
            }),
            T0,
        )
        .expect("追加该成功");
        log.navigate(Some(&first)).expect("回溯该成功");
        log.append(
            NewEntry::new(EntryPayload::Message {
                message: Message::Assistant(
                    super::super::entry::PendingAssistant {
                        content: "新答复".into(),
                        tool_calls: vec![],
                    }
                    .settle(super::super::entry::StopReason::Stop),
                ),
            }),
            T0,
        )
        .expect("追加该成功");

        let revived = round_trips(&log);
        assert_eq!(revived.len(), 3);
        assert_eq!(
            revived.path().expect("无环").len(),
            2,
            "当前分支应该是回溯后的那条"
        );
        assert_eq!(
            super::super::context::project(&revived)
                .expect("投影该成功")
                .messages()
                .len(),
            2,
            "投影里不该混进被放弃的那条答复"
        );
    }

    /// 撕裂的末行只丢那一条，前面完整的条目照常可用
    #[test]
    fn a_torn_last_line_is_dropped_instead_of_poisoning_the_file() {
        let dir = scoped_temp_dir("session-torn-line");
        let path = path_for(&dir, "C:/work/demo", T0, "sess-1").expect("路径该合法");
        save(
            &path,
            &SessionHeader::new("sess-1".into(), T0, "C:/work/demo".into()),
            &session_log(),
        )
        .expect("保存该成功");
        let mut raw = fs::read_to_string(&path).expect("刚写过该读得到");
        raw.push_str(
            "{\"type\":\"message\",\"id\":\"zz1\",\"seq\":9,\"timestamp\":1,\"parent_id\":",
        );
        fs::write(&path, raw).expect("人为撕裂末行");

        let (_, revived) = load(&path).expect("半截末行不该让整份文件读不动");
        assert_eq!(revived.len(), 2);
    }

    /// 未知类别要跳过而不是拒读，否则加了新一类之后旧版本连老话题都开不了
    #[test]
    fn an_unknown_entry_kind_is_skipped_not_fatal() {
        let dir = scoped_temp_dir("session-unknown-kind");
        let path = path_for(&dir, "C:/work/demo", T0, "sess-1").expect("路径该合法");
        save(
            &path,
            &SessionHeader::new("sess-1".into(), T0, "C:/work/demo".into()),
            &session_log(),
        )
        .expect("保存该成功");
        let mut raw = fs::read_to_string(&path).expect("刚写过该读得到");
        raw.push_str("{\"type\":\"quantum_state\",\"id\":\"q1\",\"seq\":99,\"timestamp\":1,\"parent_id\":null}\n");
        fs::write(&path, raw).expect("人为追加未来类别");

        let (_, revived) = load(&path).expect("未知类别不该让文件读不动");
        assert_eq!(revived.len(), 2);
    }

    /// 不是本应用的文件就原样留着：拒读时不许改动它
    #[test]
    fn a_foreign_file_is_refused_without_being_touched() {
        let dir = scoped_temp_dir("session-foreign");
        let path = dir.join("somebody-elses.jsonl");
        let original = "not a session at all\n".to_string();
        fs::write(&path, &original).expect("写 foreign 文件");
        assert!(load(&path).is_err());
        assert_eq!(
            fs::read_to_string(&path).expect("该还在"),
            original,
            "拒读不许顺手改人家的文件"
        );
    }

    #[test]
    fn a_session_id_that_could_escape_the_directory_is_refused() {
        let dir = scoped_temp_dir("session-id-guard");
        assert!(path_for(&dir, "C:/work", T0, "../evil").is_err());
        assert!(path_for(&dir, "C:/work", T0, "").is_err());
    }

    /// 路径按工作目录分桶，且文件名带可比时间前缀。`:` 与 `/` 各换一个 `-`
    /// （`C:/x` → `C--x`），与参照实现的分桶名规则一致
    #[test]
    fn the_file_lands_in_its_cwd_bucket() {
        let dir = scoped_temp_dir("session-bucket");
        let path = path_for(&dir, "C:/Users/dev/project", T0, "sess-1").expect("路径该合法");
        let rendered = path.to_string_lossy().replace('\\', "/");
        assert!(
            rendered.contains("--C--Users-dev-project--"),
            "分桶名没对上：{rendered}"
        );
        assert!(
            rendered.ends_with("_sess-1.jsonl"),
            "文件名没对上：{rendered}"
        );
    }

    /// `find` 的三条判据各配一个反例。它最初用的是全等比较，而真实文件名带时间戳前缀，
    /// 于是永远找不到自己的文件——这个 bug 是靠上层"重开必须读日志"的测试才暴露的，
    /// 说明缺了这层反例测试的正是它自己
    #[test]
    fn finding_a_session_file_ignores_lookalikes() {
        let root = scoped_temp_dir("session-find");
        let header = SessionHeader::new("conv_a".into(), T0, "C:/w".into());
        let path = path_for(&root, "C:/w", T0, "conv_a").expect("路径该合法");
        save(&path, &header, &session_log()).expect("保存该成功");
        assert_eq!(find(&root, "conv_a").as_deref(), Some(path.as_path()));

        // 没有这条话题
        assert_eq!(find(&root, "conv_missing"), None);
        // id 恰好落在别人文件名的中段，下划线分隔符对不上就不该命中
        assert_eq!(find(&root, "a"), None);
        // 裸 "_conv_a.jsonl"（没有时间戳前缀）不是本应用写的文件，不能抢走真文件。
        // 分桶目录名从真文件的父目录取，不在测试里复刻一遍编码规则
        let decoy = path
            .parent()
            .expect("真文件有个父目录")
            .join("_conv_a.jsonl");
        fs::write(&decoy, "{}\n").expect("放一个诱饵文件");
        assert_eq!(find(&root, "conv_a").as_deref(), Some(path.as_path()));
    }

    /// 定形条目与段行必须原样从磁盘回来：每个回合都是重开日志的，读不到定形条目
    /// 就等于每轮重新定形一次——那正是 Stage 8 要防的事
    #[test]
    fn frozen_declarations_and_sections_survive_the_disk() {
        use super::super::entry::EntryPayload;
        use super::super::entry::NewEntry;
        let mut log = session_log();
        log.append(
            NewEntry::new(EntryPayload::Custom {
                custom_type: "tool_declarations".into(),
                data: Some(json!([{ "function": { "name": "read_file" } }])),
            }),
            T0,
        )
        .expect("追加该成功");
        log.append(
            NewEntry::new(EntryPayload::CustomMessage {
                custom_type: "system_section:project_context".into(),
                content: "【工作目录约定】
构建：cargo test"
                    .into(),
                display: false,
            }),
            T0,
        )
        .expect("追加该成功");

        let revived = round_trips(&log);
        assert_eq!(
            super::super::context::latest_custom(&revived, "tool_declarations")
                .expect("读定形该成功")
                .cloned(),
            Some(json!([{ "function": { "name": "read_file" } }])),
            "声明数组读回来变了形"
        );
        let path = revived.path().expect("走路径该成功");
        assert_eq!(
            super::super::sections::in_effect(&path)
                .get("project_context")
                .copied(),
            Some(
                "【工作目录约定】
构建：cargo test"
            ),
            "段行读丢了：那会让下一轮把同一段再写一遍"
        );
    }
}
