//! 命名段：系统提示里那几段可变的正文，作为**历史条目**存在，而不是每轮重新渲染的头部。
//!
//! 为什么要挪进来：项目约定（AGENTS.md）和技能清单都会中途变。以前它们坐在常驻段里，
//! 于是"改一行约定"等于把整段前缀换掉——后面每一行都失配，代价按整段历史算。
//! 挪进日志之后形状变成：常驻段只有默认提示词（永不变更），段内容作为条目追加，
//! 中途变更**只在末尾追加一行差分行**，已发出去的字节一个都不动（设计文档 §6.1）。
//!
//! 我们这条线（chat / responses）没有分段字段，所以段在序列化时塌成普通 system 行；
//! 塌的是形状，不塌顺序与"只追加"的性质。

use std::collections::BTreeMap;

use super::entry::{Entry, EntryPayload, Message};

/// 段名。写侧（装配处）与读侧（层的归属）都从这里取：两边各写字面量就是两份真相
pub const PROJECT_CONTEXT: &str = "project_context";
pub const SKILLS: &str = "skills";
pub const MEMORY: &str = "memory";
/// 作业模式那一段（对话 / 规划 / 目标）。它的**事实**在 `session::mode` 那一条只追加的
/// `custom` 条目里，这一段只是把那件事说给模型听——所以换模式永远只是末尾多一行差分行
pub const MODE: &str = "session_mode";

/// 段条目在 `custom_message.custom_type` 里的命名空间。带上段名，所以读侧不需要
/// 再去解析正文猜它是哪一段
pub const ENTRY_PREFIX: &str = "system_section:";

/// 撤销行的固定说法。模型看到它就该把此前那段的正文作废——段落条目只追加不改写，
/// 所以历史上会同时留着两份，必须有一句说明信哪份
pub const REVOKED_SUFFIX: &str = "（本话题该段已撤销，此前出现的正文不再适用。）";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub name: &'static str,
    /// 说给模型听的"本条取代同标记内容"那句话，由调用方带进来：
    /// 标记文本是对话内容的一部分，改它就是改前缀，不该由段机制自己决定
    pub marker: &'static str,
    pub body: String,
}

impl Section {
    /// 这一段渲染成上下文中那一行的正文。追加与估算必须共用这一条拼法
    pub fn row(&self) -> String {
        format!("{}\n{}", self.marker, self.body)
    }

    fn entry(&self, body: String) -> EntryPayload {
        EntryPayload::CustomMessage {
            custom_type: format!("{ENTRY_PREFIX}{}", self.name),
            content: body,
            display: false,
        }
    }
}

/// 段名从 `custom_type` 里读出来。不是段条目的返回 None
pub fn section_of(payload: &EntryPayload) -> Option<&str> {
    match payload {
        EntryPayload::CustomMessage { custom_type, .. } => custom_type.strip_prefix(ENTRY_PREFIX),
        _ => None,
    }
}

/// 日志里当前生效的段正文：同一段后写的那条赢（条目只追加，所以"最后写的"就是"生效的"）
pub fn in_effect<'a>(path: &[&'a Entry]) -> BTreeMap<&'a str, &'a str> {
    last_written(path)
        .into_iter()
        .map(|(name, entry)| {
            let content = match entry.payload() {
                EntryPayload::CustomMessage { content, .. } => content.as_str(),
                _ => unreachable!("section_of 只认 custom_message"),
            };
            (name, content)
        })
        .collect()
}

/// 每个段名**最后写过**的那条条目。"后写胜出"只在这里定义一次：`in_effect` 与 Inspector
/// 都从它出发——界面上那个"生效行"跟模型读到的那行必须是同一条判据挑出来的
pub fn last_written<'a>(path: &[&'a Entry]) -> BTreeMap<&'a str, &'a Entry> {
    let mut written = BTreeMap::new();
    for entry in path {
        if let Some(name) = section_of(entry.payload()) {
            written.insert(name, *entry);
        }
    }
    written
}

/// 这一段现在生效的是不是撤销行。判据放在这里而不是 Inspector 里：写撤销行的
/// `pending` 与读它的地方得共用同一句"什么算撤销"
pub fn revoked(row: &str) -> bool {
    row.ends_with(REVOKED_SUFFIX)
}

/// 这一轮该往日志追加的段差分行。三种情况各一条：没记过的段、正文变了的段、
/// 配置里已经没有但历史上还生效的段（撤销行）。正文没变的段**一条都不产生**——
/// "未变的段沿用已存渲染"是这套机制的全部意义，不需要额外的字节门控代码
pub fn pending(current: &[Section], effect: &BTreeMap<&str, &str>) -> Vec<EntryPayload> {
    let mut entries = Vec::new();
    for section in current {
        let row = section.row();
        if effect.get(section.name).copied() != Some(row.as_str()) {
            entries.push(section.entry(row));
        }
    }
    let names: Vec<&str> = current.iter().map(|section| section.name).collect();
    for (name, previous) in effect {
        if names.contains(name) {
            continue;
        }
        // 撤销行沿用历史里那条的标记（它就是那条的第一行），模型才认得出是在作废哪一段；
        // 段已经不在 current 里了，标记只能从历史拿，不该由这里另写一句
        let marker = previous.split('\n').next().unwrap_or("");
        entries.push(EntryPayload::CustomMessage {
            custom_type: format!("{ENTRY_PREFIX}{name}"),
            content: format!("{marker}\n{REVOKED_SUFFIX}"),
            display: false,
        });
    }
    entries
}

/// 压缩边界要带走的那份 system 快照：把当前生效的各段按段序并成一条。
/// 没有段就没有快照——边界不必凭空带一条空的 system 行
pub fn snapshot(current: &[Section]) -> Option<Message> {
    let text = current
        .iter()
        .map(Section::row)
        .collect::<Vec<_>>()
        .join("\n\n");
    if text.is_empty() {
        return None;
    }
    Some(Message::System { content: text })
}

#[cfg(test)]
mod tests {
    use super::super::entry::NewEntry;
    use super::super::log::SessionLog;
    use super::*;

    const T0: i64 = 1_700_000_000_000;

    fn section(name: &'static str, marker: &'static str, body: &str) -> Section {
        Section {
            name,
            marker,
            body: body.to_string(),
        }
    }

    fn apply(log: &mut SessionLog, entries: Vec<EntryPayload>) {
        for payload in entries {
            log.append(NewEntry::new(payload), T0).expect("追加该成功");
        }
    }

    fn effect(log: &SessionLog) -> BTreeMap<&str, &str> {
        in_effect(&log.path().expect("走路径该成功"))
    }

    /// 没变过就一条都不产生：这是整套机制的意义，不是某种优化
    #[test]
    fn sections_are_written_once_and_then_produce_nothing() {
        let mut log = SessionLog::new();
        let first = vec![
            section("project_context", "【工作目录约定】", "构建：cargo test"),
            section("skills", "【技能清单】", "pdf / xlsx"),
        ];
        let written = pending(&first, &effect(&log));
        apply(&mut log, written);
        assert_eq!(effect(&log).len(), 2, "两段都该生效");
        assert!(
            pending(&first, &effect(&log)).is_empty(),
            "内容没变就不该再写任何一行"
        );
    }

    /// 准入条件：改约定只往末尾追加一行差分行，此前那行一个字都不动
    #[test]
    fn editing_a_section_appends_a_row_and_leaves_the_old_one_intact() {
        let mut log = SessionLog::new();
        let old = vec![section(
            "project_context",
            "【工作目录约定】",
            "构建：cargo test",
        )];
        let written = pending(&old, &effect(&log));
        apply(&mut log, written);
        let before = super::super::prefix::sent_array(&log).expect("投影该成功");

        let edited = vec![section(
            "project_context",
            "【工作目录约定】",
            "构建：cargo test --release",
        )];
        let delta = pending(&edited, &effect(&log));
        assert_eq!(delta.len(), 1, "只该多出一行");
        apply(&mut log, delta);

        let after = super::super::prefix::sent_array(&log).expect("投影该成功");
        assert!(
            after.starts_with(&before),
            "改一段约定不该动已发出去的字节：{before:?} / {after:?}"
        );
        assert_eq!(after.len(), before.len() + 1);
        assert_eq!(
            effect(&log).get("project_context").copied(),
            Some("【工作目录约定】\n构建：cargo test --release"),
            "生效的必须是后写那一条"
        );
    }

    /// 段整个没了（比如解绑工作目录）要留下撤销行：只追加不改写，历史上就同时有两份
    #[test]
    fn a_vanished_section_leaves_a_revocation_row_with_its_own_marker() {
        let mut log = SessionLog::new();
        let written = pending(&[section("skills", "【技能清单】", "pdf")], &effect(&log));
        apply(&mut log, written);
        let delta = pending(&[], &effect(&log));
        assert_eq!(delta.len(), 1);
        apply(&mut log, delta);
        let rows = super::super::context::project(&log)
            .expect("投影该成功")
            .messages()
            .iter()
            .map(|message| match message {
                Message::System { content } => content.clone(),
                other => format!("{:?}", other.role()),
            })
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 2, "旧清单和撤销行都在上下文里：{rows:?}");
        assert!(
            rows[1].starts_with("【技能清单】") && rows[1].contains(REVOKED_SUFFIX),
            "撤销行要沿用原标记，模型才知道作废的是哪一段：{}",
            rows[1]
        );
    }

    /// 写撤销行的那处与读它的判据必须认同一句话：`revoked` 判不出来，Inspector 就会把
    /// 一句"这段已经作废"当成还在生效的约定报给用户
    #[test]
    fn the_revocation_row_is_the_one_the_revoked_predicate_recognizes() {
        let mut log = SessionLog::new();
        let written = pending(&[section("skills", "【技能清单】", "pdf")], &effect(&log));
        apply(&mut log, written);
        let delta = pending(&[], &effect(&log));
        apply(&mut log, delta);

        assert!(!revoked("【技能清单】\npdf"), "正文行不该被读成撤销");
        let path = log.path().expect("走路径该成功");
        let row = effect(&log).get("skills").copied().expect("段还在生效");
        assert!(revoked(row), "撤销行必须被自己的判据认出来：{row}");
        assert_eq!(
            last_written(&path)
                .get("skills")
                .map(|entry| entry.id.clone()),
            path.last().map(|entry| entry.id.clone()),
            "后写胜出读到的那条，就是撤销那一条"
        );
    }

    /// 压缩边界带走的快照：段序里的正文并成一条 system，没段就不硬造一行
    #[test]
    fn the_snapshot_follows_section_order_or_stays_absent() {
        assert!(snapshot(&[]).is_none());
        let message = snapshot(&[
            section("project_context", "【工作目录约定】", "构建：cargo test"),
            section("skills", "【技能清单】", "pdf"),
        ])
        .expect("有段就该有快照");
        match message {
            Message::System { content } => {
                assert!(
                    content.find("【工作目录约定】").unwrap() < content.find("【技能清单】").unwrap(),
                    "段序不能倒：{content}"
                );
            }
            other => panic!("快照该是 system 行，拿到 {:?}", other.role()),
        }
    }
}
