//! 只追加的条目树，以及"追加"这个动作上仅有的几道门。
//!
//! 这里没有删除历史的 API：`navigate` 只移动分支末端，被放弃的路径一行都不删（它仍然在
//! 文件里，也仍然可以从那里再走上去）。上一轮改造里那些"界面 id 序列是不是只尾部增长"
//! 的判定，前提是有两份真相要对账；这里只有一份，所以没有判定可写。

use std::collections::HashMap;
use std::fmt;

use super::entry::{Entry, EntryPayload, NewEntry};
use super::valid_id;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionError {
    InvalidId(String),
    DuplicateId(String),
    UnknownParent(String),
    UnknownTarget(String),
    TargetNotEditable(String),
    SeqNotMonotonic { previous: u64, found: u64 },
}

impl fmt::Display for SessionError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidId(id) => write!(out, "条目 id 不合法：{id:?}"),
            Self::DuplicateId(id) => write!(out, "条目 id 重复：{id:?}"),
            Self::UnknownParent(id) => write!(out, "条目的父节点不存在：{id:?}"),
            Self::UnknownTarget(id) => write!(out, "要指向的条目不存在：{id:?}"),
            Self::TargetNotEditable(id) => write!(out, "该条目不能被编辑或撤回：{id:?}"),
            Self::SeqNotMonotonic { previous, found } => {
                write!(out, "序号不单调：上一条 {previous}，这一条 {found}")
            }
        }
    }
}

impl std::error::Error for SessionError {}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionLog {
    entries: Vec<Entry>,
    index: HashMap<String, usize>,
    leaf: Option<String>,
    next_seq: u64,
    /// 铸造 id 用的扰动量。没有引第三方随机库：本机话题内唯一即可，
    /// 而唯一性由 `index` 查重 + 重试兜住，这个计数只是让撞车概率再低一档
    nonce: u64,
}

impl SessionLog {
    pub fn new() -> Self {
        Self {
            next_seq: 1,
            ..Self::default()
        }
    }

    /// 从已读出的条目重建日志。所有校验都在这条路上跑一遍：文件是外部输入，
    /// 而"内存里的树从不合法状态开始"是后面每一条断言的前提
    pub fn restore(entries: Vec<Entry>) -> Result<Self, SessionError> {
        let mut log = Self::new();
        let mut previous_seq = 0;
        for entry in entries {
            if !valid_id(&entry.id) {
                return Err(SessionError::InvalidId(entry.id));
            }
            if log.index.contains_key(&entry.id) {
                return Err(SessionError::DuplicateId(entry.id));
            }
            if entry.seq <= previous_seq {
                return Err(SessionError::SeqNotMonotonic {
                    previous: previous_seq,
                    found: entry.seq,
                });
            }
            if let Some(parent) = &entry.parent_id {
                if !log.index.contains_key(parent) {
                    return Err(SessionError::UnknownParent(parent.clone()));
                }
            }
            previous_seq = entry.seq;
            log.index.insert(entry.id.clone(), log.entries.len());
            log.entries.push(entry);
            log.leaf = Some(log.entries.last().expect("刚推进去一条").id.clone());
        }
        log.next_seq = previous_seq + 1;
        Ok(log)
    }

    /// 从已读出的条目重建日志，并把分支末端停在 `tip` 上。
    ///
    /// `None` 沿用"物理末行就是末端"的旧推导（tip 持久化之前写出的存档）。给了 tip 却
    /// 不在树里就报错，而不是悄悄退回末行：那正是"界面停在 A 支、重开后跳到 B 支"的形状，
    /// 而静默修正比一次失败难查得多
    pub fn restore_at(entries: Vec<Entry>, tip: Option<&str>) -> Result<Self, SessionError> {
        let mut log = Self::restore(entries)?;
        if let Some(tip) = tip {
            log.navigate(Some(tip))?;
        }
        Ok(log)
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn leaf_id(&self) -> Option<&str> {
        self.leaf.as_deref()
    }

    pub fn entry(&self, id: &str) -> Option<&Entry> {
        self.index.get(id).map(|at| &self.entries[*at])
    }

    /// 当前分支：从末端走父链到根，再反转。带圈检测——父链成环是文件被改坏的特征，
    /// 而宁可在这里报错也不能在遍历里无限转下去
    pub fn path(&self) -> Result<Vec<&Entry>, SessionError> {
        let mut path = Vec::new();
        let mut cursor = self.leaf.as_deref();
        while let Some(id) = cursor {
            let entry = self
                .entry(id)
                .ok_or_else(|| SessionError::UnknownParent(id.to_string()))?;
            if path.len() > self.entries.len() {
                return Err(SessionError::UnknownParent(
                    entry.parent_id.clone().unwrap_or(entry.id.clone()),
                ));
            }
            cursor = entry.parent_id.as_deref();
            path.push(entry);
        }
        path.reverse();
        Ok(path)
    }

    /// 追加的唯一入口：铸造身份、跑写门、移动末端。父指针永远指向当前末端，
    /// 所以"条目挂上去但末端没跟上"这种半提交状态在这里不可能出现
    pub fn append(&mut self, entry: NewEntry, timestamp: i64) -> Result<&Entry, SessionError> {
        self.check_door(&entry.payload)?;
        let id = self.mint_id();
        let stored = Entry::assemble(
            id.clone(),
            self.leaf.clone(),
            self.next_seq,
            timestamp,
            entry.model,
            entry.payload,
        );
        self.next_seq += 1;
        self.index.insert(id.clone(), self.entries.len());
        self.entries.push(stored);
        self.leaf = Some(id);
        Ok(self.entries.last().expect("刚推进去一条"))
    }

    /// 移动分支末端。`None` 表示退到根之前（下一条追加就成了第二条根）
    pub fn navigate(&mut self, target: Option<&str>) -> Result<(), SessionError> {
        match target {
            None => {
                self.leaf = None;
                Ok(())
            }
            Some(id) => {
                if !self.index.contains_key(id) {
                    return Err(SessionError::UnknownTarget(id.to_string()));
                }
                self.leaf = Some(id.to_string());
                Ok(())
            }
        }
    }

    /// 写门。序号与父链由上面的铸造过程保证，这里只管"内容上不该发生的事"
    fn check_door(&self, payload: &EntryPayload) -> Result<(), SessionError> {
        if let EntryPayload::ContextEdit {
            target_id,
            replacement,
        } = payload
        {
            let target = self
                .entry(target_id)
                .ok_or_else(|| SessionError::UnknownTarget(target_id.clone()))?;
            if !self.on_branch(target)? {
                return Err(SessionError::UnknownTarget(target_id.clone()));
            }
            // 改写只针对承载正文的行；边界行只许被撤回（`replacement: None`），
            // 因为"撤掉一次压缩"要的是让被顶替的那些条目回来，不是换一句摘要
            let allowed = target.payload().editable()
                || (replacement.is_none() && target.payload().revocable());
            if !allowed {
                return Err(SessionError::TargetNotEditable(target_id.clone()));
            }
        }
        Ok(())
    }

    fn on_branch(&self, entry: &Entry) -> Result<bool, SessionError> {
        Ok(self.path()?.iter().any(|row| row.id == entry.id))
    }

    fn mint_id(&mut self) -> String {
        self.nonce = self.nonce.wrapping_add(1);
        for attempt in 0..100 {
            let candidate = self.candidate_id(attempt);
            if !self.index.contains_key(&candidate) {
                return candidate;
            }
        }
        // 撞满 100 次还重（现实中不会发生），退成更长的串，唯一性靠 index 再兜一层
        loop {
            self.nonce = self.nonce.wrapping_add(1);
            let candidate = format!("{:016x}", self.stamp() ^ self.nonce);
            if !self.index.contains_key(&candidate) {
                return candidate;
            }
        }
    }

    fn candidate_id(&self, attempt: u64) -> String {
        let mixed = self.stamp() ^ (self.nonce << 8) ^ attempt;
        format!("{:08x}", (mixed & 0xffff_ffff) as u32)
    }

    fn stamp(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::super::entry::{Message, StopReason};
    use super::*;

    const T0: i64 = 1_700_000_000_000;

    fn user(text: &str) -> NewEntry {
        NewEntry::new(EntryPayload::Message {
            message: Message::User {
                content: text.into(),
                images: Vec::new(),
                audios: Vec::new(),
                videos: Vec::new(),
            },
        })
    }

    fn assistant(text: &str) -> NewEntry {
        NewEntry::new(EntryPayload::Message {
            message: Message::Assistant(
                super::super::entry::PendingAssistant {
                    content: text.into(),
                    tool_calls: vec![],
                }
                .settle(StopReason::Stop),
            ),
        })
    }

    fn push(log: &mut SessionLog, entry: NewEntry) -> String {
        log.append(entry, T0).expect("追加该成功").id.clone()
    }

    /// 写门 I3：序号由存储层持有，恢复时必须严格递增，否则说明文件被动过
    #[test]
    fn restoring_entries_with_a_backwards_sequence_is_refused() {
        let entries = vec![
            Entry::assemble(
                "a1".into(),
                None,
                1,
                T0,
                None,
                EntryPayload::SessionInfo { name: None },
            ),
            Entry::assemble(
                "a2".into(),
                Some("a1".into()),
                1,
                T0,
                None,
                EntryPayload::SessionInfo { name: None },
            ),
        ];
        assert_eq!(
            SessionLog::restore(entries).err(),
            Some(SessionError::SeqNotMonotonic {
                previous: 1,
                found: 1
            })
        );
    }

    /// 写门 I3：同一条目出现两次要拒，不能悄悄覆盖
    #[test]
    fn restoring_duplicate_ids_is_refused() {
        let entries = vec![
            Entry::assemble(
                "a1".into(),
                None,
                1,
                T0,
                None,
                EntryPayload::SessionInfo { name: None },
            ),
            Entry::assemble(
                "a1".into(),
                Some("a1".into()),
                2,
                T0,
                None,
                EntryPayload::SessionInfo { name: None },
            ),
        ];
        assert_eq!(
            SessionLog::restore(entries),
            Err(SessionError::DuplicateId("a1".into()))
        );
    }

    /// 写门 I4：父节点不存在就整份拒掉，不留半棵树
    #[test]
    fn restoring_an_entry_whose_parent_is_never_created_is_refused() {
        let entries = vec![Entry::assemble(
            "a1".into(),
            Some("ghost".into()),
            1,
            T0,
            None,
            EntryPayload::SessionInfo { name: None },
        )];
        assert_eq!(
            SessionLog::restore(entries),
            Err(SessionError::UnknownParent("ghost".into()))
        );
    }

    /// 写门 I9：id 会被拼进文件名，所以越界字符必须在恢复时就拒掉
    #[test]
    fn restoring_an_id_that_could_escape_the_file_name_is_refused() {
        let entries = vec![Entry::assemble(
            "../evil".into(),
            None,
            1,
            T0,
            None,
            EntryPayload::SessionInfo { name: None },
        )];
        assert_eq!(
            SessionLog::restore(entries),
            Err(SessionError::InvalidId("../evil".into()))
        );
    }

    /// 写门 I8：撤回一个不存在的目标要拒——不然编辑会静默地什么都没改
    #[test]
    fn an_edit_pointing_at_nothing_is_refused() {
        let mut log = SessionLog::new();
        let error = log
            .append(
                NewEntry::new(EntryPayload::ContextEdit {
                    target_id: "ghost".into(),
                    replacement: None,
                }),
                T0,
            )
            .expect_err("目标不存在，不该收下");
        assert_eq!(error, SessionError::UnknownTarget("ghost".into()));
        assert!(log.is_empty(), "被拒的追加不该留下任何痕迹");
    }

    /// 写门 I8：压缩条目不是正文，改不了
    #[test]
    fn a_compaction_entry_is_not_editable() {
        let mut log = SessionLog::new();
        let boundary = push(
            &mut log,
            NewEntry::new(EntryPayload::Compaction {
                summary: "摘要".into(),
                first_kept_entry_id: "x".into(),
                tokens_before: 10,
                usage: None,
                system_message: None,
            }),
        );
        let error = log
            .append(
                NewEntry::new(EntryPayload::ContextEdit {
                    target_id: boundary,
                    replacement: Some("改写".into()),
                }),
                T0,
            )
            .expect_err("压缩条目不该可编辑");
        assert!(matches!(error, SessionError::TargetNotEditable(_)));
    }

    /// 写门 I8：编辑必须落在当前分支上。换到别的分支去改旧路径，等于改了个投影里
    /// 根本不存在的东西
    #[test]
    fn an_edit_must_point_at_the_active_branch() {
        let mut log = SessionLog::new();
        let first = push(&mut log, user("第一问"));
        let abandoned = push(&mut log, assistant("被放弃的那条"));
        log.navigate(Some(&first)).expect("回溯该成功");
        push(&mut log, user("改后的第二问"));
        let error = log
            .append(
                NewEntry::new(EntryPayload::ContextEdit {
                    target_id: abandoned.clone(),
                    replacement: None,
                }),
                T0,
            )
            .expect_err("被放弃的路径不在活跃分支上");
        assert_eq!(error, SessionError::UnknownTarget(abandoned));
    }

    /// 父链与末端一起动：追加之后末端必然是新那条，且它父指旧末端
    #[test]
    fn appending_moves_the_tip_and_chains_the_parent() {
        let mut log = SessionLog::new();
        let first = push(&mut log, user("第一问"));
        let second = push(&mut log, assistant("答复"));
        assert_eq!(log.leaf_id(), Some(second.as_str()));
        assert_eq!(
            log.entry(&second).expect("在的").parent_id.as_deref(),
            Some(first.as_str())
        );
        assert_eq!(log.path().expect("无环").len(), 2);
    }

    /// 回溯不删任何东西：两条分支都还在文件里，只是当前分支换了一条
    #[test]
    fn navigating_back_keeps_the_abandoned_entries() {
        let mut log = SessionLog::new();
        let first = push(&mut log, user("第一问"));
        push(&mut log, assistant("旧答复"));
        log.navigate(Some(&first)).expect("回溯该成功");
        push(&mut log, assistant("新答复"));
        assert_eq!(log.len(), 3, "被放弃的那条必须还在");
        assert_eq!(log.path().expect("无环").len(), 2, "当前分支上不该再看见它");
    }

    /// 序号稠密递增、时间戳由参数进来：调用方没有第三条路可走
    #[test]
    fn the_storage_layer_owns_sequence_and_timestamp() {
        let mut log = SessionLog::new();
        let first = push(&mut log, user("第一问"));
        let second = push(&mut log, user("第二问"));
        assert_eq!(
            (
                log.entry(&first).unwrap().seq,
                log.entry(&second).unwrap().seq
            ),
            (1, 2)
        );
        assert_eq!(log.entry(&second).unwrap().timestamp, T0);
    }

    /// 恢复后再追加，序号要接着往上走，不能从 1 重来撞掉自己
    #[test]
    fn a_restored_log_continues_the_sequence() {
        let mut log = SessionLog::new();
        push(&mut log, user("第一问"));
        push(&mut log, assistant("答复"));
        let mut revived =
            SessionLog::restore(log.entries().to_vec()).expect("自己写出来的该能读回");
        let third = revived
            .append(user("第二问"), T0 + 1)
            .expect("追加该成功")
            .clone();
        assert_eq!(third.seq, 3);
    }
}
