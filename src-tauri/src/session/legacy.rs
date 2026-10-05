//! 旧台账（界面存档）→ 条目日志的一次性迁移。
//!
//! 为什么需要它：上一版把"老话题存档里没有归属信息，删了 `messages` 入参第一轮就失忆"当作
//! 撤销理由。那条理由成立的前提是**用运行时兜底**去收编实发行；这里换成一次性迁移，做完之后
//! 双来源就没有存在的必要，`chat_send` 的 `messages` 入参才能真删。
//!
//! 迁移的标准是**行为不变换**，不是修数据：迁完之后的投影必须和今天前端那段重放
//! （`chat-store.ts` 的 `send()` 展开 + `chat.rs` 的 `thread_from_history` 收形）产出同一批字节。
//! 已用真机留下的实发快照带外核对过一次（`conv_a8c8202d`，含一次工具调用）：逐字节相同。
//!
//! 两处刻意不等价，都在真实数据上不可触发（探测过 23 个话题 / 98 条消息 / 11 个工具调用）：
//! - 未跑完的工具调用不再被伪造成"跑完了、没输出"发出去（F9）。旧前端的过滤是
//!   `arguments !== undefined && (output !== undefined || status === "pending")`，而
//!   `history::sanitized` 在落盘前已把 pending/running 摘掉，所以那个分支在真存档里取不到值。
//! - 一轮多条 assistant 的真实形状**无法**从台账恢复（界面把收尾回答并进带调用的那条，即 F13）。
//!   旧前端本来也按并好的形状发，所以迁移照此保留——这是不改变现状，不是修好它。

use std::collections::HashSet;

use serde_json::Value;

use crate::history::{Conversation, MessageRecord};

use super::context::project;
use super::entry::{
    Entry, EntryPayload, Message, SettledAssistant, StopReason, ToolCall, UsageRecord,
};
use super::log::{SessionError, SessionLog};
use super::store::SessionHeader;
use super::valid_id;

/// 一次迁移的产物：一条话题的日志 + header + 被丢开什么的备注。它**不知道**自己该落在哪，
/// 所以也不能保存——能保存的是下面 [`Migration`]，只有"打开过"的话题才拿得到路径
#[derive(Clone, Debug)]
pub struct Migrated {
    pub header: SessionHeader,
    pub log: SessionLog,
    /// 每一条被丢开或换掉的东西。迁移不许静默改数据
    pub notes: Vec<String>,
}

/// 打开着的话题：迁移产物加上它的落盘位置
#[derive(Clone, Debug)]
pub struct Migration {
    pub opened: Migrated,
    /// 这条话题该落在哪个文件上。已存在的一律原位重写，不会每存一次换一个名字
    pub path: std::path::PathBuf,
}

impl std::ops::Deref for Migration {
    type Target = Migrated;

    fn deref(&self) -> &Migrated {
        &self.opened
    }
}

impl Migrated {
    /// 迁移产物当下会发出去的那批字节（不含常驻段）
    pub fn sent(&self) -> Result<Vec<Value>, SessionError> {
        Ok(project(&self.log)?.wire())
    }
}

impl Migration {
    /// 打开一条话题：日志已经在就用它，不在就从界面台账迁一次。
    /// 这里**不**落盘——调用方决定什么时候写（回合结束时写，避免只读打开也改文件）
    pub fn open(root: &std::path::Path, conversation: &Conversation) -> Result<Self, String> {
        if let Some(path) = super::store::find(root, &conversation.id) {
            let (header, log) = super::store::load(&path)?;
            return Ok(Migration {
                opened: Migrated {
                    header,
                    log,
                    notes: Vec::new(),
                },
                path,
            });
        }
        let opened = from_ledger(conversation);
        let path = super::store::path_of(root, &opened.header)?;
        Ok(Migration { opened, path })
    }

    /// 追加条目要能改日志。Deref 只给只读视图，所以这里显式开一个可变入口
    pub fn log_mut(&mut self) -> &mut SessionLog {
        &mut self.opened.log
    }

    pub fn save(&self) -> Result<(), String> {
        super::store::save(&self.path, &self.header, &self.log)
    }
}

/// 按父链顺序攒条目：序号稠密、父指针指向前一条，这两件事由这里保证而不是靠调用方自觉
#[derive(Default)]
struct Chain {
    entries: Vec<Entry>,
    parent: Option<String>,
    seq: u64,
    used: HashSet<String>,
}

impl Chain {
    fn new() -> Self {
        Self {
            seq: 1,
            ..Self::default()
        }
    }

    /// 追加一条。`wanted` 是希望沿用的旧 id；不合法或已占用时换新 id 并留话
    fn push(
        &mut self,
        wanted: String,
        conversation: &str,
        timestamp: i64,
        payload: EntryPayload,
        notes: &mut Vec<String>,
    ) {
        let taken = self.used.len() + 1;
        let id = if valid_id(&wanted) && !self.used.contains(&wanted) {
            wanted
        } else {
            let mut candidate = format!("{conversation}-{taken}");
            while self.used.contains(&candidate) || !valid_id(&candidate) {
                candidate = format!("{candidate}x");
            }
            notes.push(format!(
                "旧 id {wanted:?} 不合法或与前面重复，改用 {candidate}"
            ));
            candidate
        };
        self.used.insert(id.clone());
        // 迁移的旧行没有随账模型名：模型名靠 restore_models_from_ledger 从台账认领
        self.entries.push(Entry::assemble(
            id.clone(),
            self.parent.clone(),
            self.seq,
            timestamp,
            None,
            payload,
        ));
        self.seq += 1;
        self.parent = Some(id);
    }
}

/// 把一条旧话题变成日志。纯函数：不读盘、不落盘，所以既能在装载时按需调用，也能被测试直接喂
pub fn from_ledger(conversation: &Conversation) -> Migrated {
    let mut chain = Chain::new();
    let mut notes = Vec::new();

    // 一次事务性的"导入"记账：不进上下文，只让费用面板能把这笔历史和新格式下的花费分开算
    if let Some(usage) = &conversation.usage {
        chain.push(
            format!("{}-usage", conversation.id),
            &conversation.id,
            conversation.created_at,
            EntryPayload::Usage {
                kind: "ledger_import".into(),
                provider: String::new(),
                model: String::new(),
                usage: UsageRecord {
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                    cached_tokens: None,
                    cache_write_tokens: 0,
                },
                note: Some("从界面台账迁移过来的累计值，不是新格式下发生的请求".into()),
            },
            &mut notes,
        );
    }
    if !conversation.title.is_empty() {
        chain.push(
            format!("{}-title", conversation.id),
            &conversation.id,
            conversation.created_at,
            EntryPayload::SessionInfo {
                name: Some(conversation.title.clone()),
            },
            &mut notes,
        );
    }

    for record in &conversation.messages {
        // 一条消息可能映射出多行（带 N 个调用就是 1+N 行）。第一行沿用旧 id，后面的加后缀——
        // 不这样做的话它们会全想去抢同一个 id，被去重兜住之后 id 就没有溯源意义了
        for (offset, payload) in entries_of(record, &mut notes).into_iter().enumerate() {
            let wanted = if offset == 0 {
                record.id.clone()
            } else {
                format!("{}-{}", record.id, offset)
            };
            chain.push(
                wanted,
                &conversation.id,
                record.created_at,
                payload,
                &mut notes,
            );
        }
    }

    let log = SessionLog::restore(chain.entries)
        .expect("迁移产出的日志必须自洽：序号稠密、父链完整、id 合法");
    Migrated {
        header: SessionHeader::new(
            conversation.id.clone(),
            conversation.created_at,
            conversation.project_id.clone(),
        ),
        log,
        notes,
    }
}

/// 一条台账消息 → 零到多条条目。规则逐条对着旧前端的 `send()` 展开写
fn entries_of(record: &MessageRecord, notes: &mut Vec<String>) -> Vec<EntryPayload> {
    let mut out = Vec::new();
    let calls: Vec<&crate::history::ToolCallRecord> = record
        .tool_calls
        .iter()
        .filter(|call| call.arguments.is_some() && call.output.is_some())
        .collect();

    if calls.len() < record.tool_calls.len() {
        notes.push(format!(
            "{}：{} 个工具调用没有参数或没有结果，按未完成处理（不伪造结果）",
            record.id,
            record.tool_calls.len() - calls.len()
        ));
    }

    if !calls.is_empty() {
        out.push(EntryPayload::Message {
            message: Message::Assistant(SettledAssistant {
                content: record.content.clone(),
                tool_calls: calls
                    .iter()
                    .map(|call| ToolCall {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone().unwrap_or_default(),
                        // 老库里本来就没有偏移：None = 旧话题按堆叠布局显示
                        content_chars: call.content_chars,
                    })
                    .collect(),
                stop: StopReason::ToolUse,
                reasoning: record.reasoning.clone(),
                error: record.error.clone(),
                thinking_signature: None,
                reasoning_items_json: None,
            }),
        });
        for call in calls {
            out.push(EntryPayload::Message {
                message: Message::Tool {
                    tool_call_id: call.id.clone(),
                    // 旧前端对"跑完但没输出"发的是同一串，保留它是为了不发第二份真相
                    content: call.output.clone().unwrap_or_default(),
                },
            });
        }
        return out;
    }

    // 旧前端把"正文去掉空白后为空"的行整条丢掉，这里保持同一个口径
    if record.content.trim().is_empty() {
        notes.push(format!(
            "{}：正文为空且没有工具调用，按旧前端口径丢弃",
            record.id
        ));
        return out;
    }

    match record.role.as_str() {
        "user" => out.push(EntryPayload::Message {
            message: Message::User {
                content: record.content.clone(),
                images: Vec::new(),
                audios: Vec::new(),
                videos: Vec::new(),
            },
        }),
        "system" => out.push(EntryPayload::Message {
            message: Message::System {
                content: record.content.clone(),
            },
        }),
        "assistant" => out.push(EntryPayload::Message {
            message: Message::Assistant(SettledAssistant {
                content: record.content.clone(),
                tool_calls: Vec::new(),
                stop: if record.error.is_some() {
                    StopReason::Error
                } else {
                    StopReason::Stop
                },
                reasoning: record.reasoning.clone(),
                error: record.error.clone(),
                thinking_signature: None,
                reasoning_items_json: None,
            }),
        }),
        other => {
            notes.push(format!(
                "{}：角色 {other:?} 无法映射成条目，已丢开",
                record.id
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::entry::NewEntry;
    use super::super::prefix::{divergence, sent_array, Cause, PrefixLedger};
    use super::*;
    use crate::history::{ToolCallRecord, UsageRecord as LedgerUsage};
    use serde_json::json;

    fn record(id: &str, role: &str, content: &str, at: i64) -> MessageRecord {
        MessageRecord {
            id: id.into(),
            role: role.into(),
            content: content.into(),
            created_at: at,
            ..Default::default()
        }
    }

    fn call(id: &str, name: &str, arguments: Option<&str>, output: Option<&str>) -> ToolCallRecord {
        ToolCallRecord {
            id: id.into(),
            name: name.into(),
            status: "done".into(),
            risk: "low".into(),
            input: String::new(),
            output: output.map(str::to_string),
            arguments: arguments.map(str::to_string),
            content_chars: None,
        }
    }

    fn ledger(messages: Vec<MessageRecord>) -> Conversation {
        Conversation {
            id: "conv_test".into(),
            project_id: "proj-1".into(),
            title: "标题".into(),
            created_at: 1_000,
            updated_at: 2_000,
            pinned: false,
            kind: "chat".to_string(),
            video_nodes: Vec::new(),
            video_edges: Vec::new(),
            messages,
            usage: Some(LedgerUsage {
                input_tokens: 120,
                output_tokens: 30,
                duration_ms: 4_000,
            }),
        }
    }

    fn wire_of(row: &MessageRecord) -> Vec<EntryPayload> {
        entries_of(row, &mut Vec::new())
    }

    #[test]
    fn plain_turns_migrate_in_order_and_keep_their_ids_and_times() {
        let migrated = from_ledger(&ledger(vec![
            record("msg_a", "user", "第一问", 10),
            record("msg_b", "assistant", "答复一", 20),
            record("msg_c", "user", "第二问", 30),
        ]));
        assert_eq!(
            migrated.sent().expect("投影该成功"),
            vec![
                json!({ "role": "user", "content": "第一问" }),
                json!({ "role": "assistant", "content": "答复一" }),
                json!({ "role": "user", "content": "第二问" }),
            ]
        );
        let ids: Vec<&str> = migrated
            .log
            .entries()
            .iter()
            .map(|entry| entry.id.as_str())
            .collect();
        assert!(
            ids.contains(&"msg_a"),
            "界面 id 必须沿用，否则恢复话题时引用全断：{ids:?}"
        );
        assert_eq!(
            migrated
                .log
                .entries()
                .iter()
                .last()
                .expect("有末条")
                .timestamp,
            30
        );
        assert!(
            migrated.notes.is_empty(),
            "干净数据不该产生备注：{:?}",
            migrated.notes
        );
    }

    /// F13 的口径：并好的那条照旧并着发。测试钉的是"迁移不改现状"，不是"迁移修好了它"
    #[test]
    fn a_merged_tool_turn_migrates_to_the_same_bytes_the_frontend_used_to_send() {
        let mut answered = record("msg_b", "assistant", "这个目录里有 2 个文件", 20);
        answered.tool_calls = vec![call(
            "call_1",
            "list_dir",
            Some("{\"path\":\".\"}"),
            Some("a.html\nb.html"),
        )];
        let migrated = from_ledger(&ledger(vec![
            record("msg_a", "user", "列一下目录", 10),
            answered,
        ]));

        assert_eq!(
            migrated.sent().expect("投影该成功"),
            vec![
                json!({ "role": "user", "content": "列一下目录" }),
                json!({
                    "role": "assistant",
                    "content": "这个目录里有 2 个文件",
                    "tool_calls": [{
                        "id": "call_1", "type": "function",
                        "function": { "name": "list_dir", "arguments": "{\"path\":\".\"}" },
                    }],
                }),
                json!({ "role": "tool", "tool_call_id": "call_1", "content": "a.html\nb.html" }),
            ],
            "必须是嵌套形，且调用排在结果之前"
        );
        assert!(
            migrated.notes.is_empty(),
            "干净数据不该靠去重兜底：{:?}",
            migrated.notes
        );
        let ids: Vec<&str> = migrated
            .log
            .entries()
            .iter()
            .map(|entry| entry.id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec![
                "conv_test-usage",
                "conv_test-title",
                "msg_a",
                "msg_b",
                "msg_b-1"
            ],
            "工具结果行的 id 要能从它所属的消息追回去：{ids:?}"
        );
    }

    /// 界面上的思维链与错误文案要留住，但一个字节都不能上 wire
    #[test]
    fn display_only_fields_survive_the_migration_without_touching_the_wire() {
        let mut noisy = record("msg_b", "assistant", "答复", 20);
        noisy.reasoning = Some("先想想".into());
        noisy.error = Some("上游 503".into());
        let clean = record("msg_b", "assistant", "答复", 20);

        let migrated = from_ledger(&ledger(vec![noisy.clone()]));
        assert_eq!(
            migrated.sent().expect("投影该成功"),
            from_ledger(&ledger(vec![clean]))
                .sent()
                .expect("投影该成功"),
            "服务商收到的字节不该因为界面字段而变"
        );
        let kept = migrated
            .log
            .entries()
            .iter()
            .find_map(|entry| match entry.payload() {
                EntryPayload::Message {
                    message: Message::Assistant(settled),
                } => Some((settled.reasoning.clone(), settled.error.clone())),
                _ => None,
            });
        assert_eq!(
            kept,
            Some((Some("先想想".to_string()), Some("上游 503".to_string()))),
            "用户看过的东西不能丢"
        );
    }

    /// 没有结果的调用不许被伪造成"跑完了、没输出"（F9）
    #[test]
    fn an_unfinished_call_is_dropped_instead_of_forged() {
        let mut half = record("msg_b", "assistant", "", 20);
        half.tool_calls = vec![call(
            "call_1",
            "terminal_exec",
            Some("{\"command\":\"ls\"}"),
            None,
        )];
        let migrated = from_ledger(&ledger(vec![record("msg_a", "user", "列目录", 10), half]));
        let sent = migrated.sent().expect("投影该成功");

        assert_eq!(
            sent.len(),
            1,
            "没跑完的调用既不该发调用、也不该配一条假结果：{sent:?}"
        );
        assert_eq!(sent[0]["role"], "user");
        assert!(
            migrated
                .notes
                .iter()
                .any(|note| note.contains("不伪造结果")),
            "丢开调用必须留话：{:?}",
            migrated.notes
        );
    }

    /// 记账与标题条目对上下文零贡献，但数据不丢
    #[test]
    fn bookkeeping_entries_carry_the_ledger_summary_without_entering_context() {
        let migrated = from_ledger(&ledger(vec![record("msg_a", "user", "第一问", 10)]));
        assert_eq!(migrated.sent().expect("投影该成功").len(), 1);

        let kept: Vec<String> = migrated
            .log
            .entries()
            .iter()
            .filter_map(|entry| match entry.payload() {
                EntryPayload::Usage { kind, usage, .. } => {
                    Some(format!("{kind}:{}", usage.input_tokens))
                }
                EntryPayload::SessionInfo { name } => {
                    Some(format!("title:{}", name.as_deref().unwrap_or_default()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(kept, vec!["ledger_import:120", "title:标题"]);
    }

    #[test]
    fn a_blank_row_is_dropped_with_a_note_not_silently() {
        let migrated = from_ledger(&ledger(vec![
            record("msg_a", "user", "第一问", 10),
            record("msg_b", "assistant", "   \n ", 20),
        ]));
        assert_eq!(migrated.sent().expect("投影该成功").len(), 1);
        assert!(
            migrated.notes.iter().any(|note| note.contains("msg_b")),
            "{:?}",
            migrated.notes
        );
    }

    #[test]
    fn an_unknown_role_is_reported_instead_of_guessed() {
        let migrated = from_ledger(&ledger(vec![record("msg_a", "wat", "一段正文", 10)]));
        assert!(migrated.sent().expect("投影该成功").is_empty());
        assert!(
            migrated.notes.iter().any(|note| note.contains("wat")),
            "{:?}",
            migrated.notes
        );
    }

    /// 重复 id（旧存档只保证界面内唯一）要能迁过去，且父链不断
    #[test]
    fn duplicated_ids_are_renamed_without_breaking_the_chain() {
        let migrated = from_ledger(&ledger(vec![
            record("msg_same", "user", "第一问", 10),
            record("msg_same", "assistant", "答复", 20),
            record("msg_third", "user", "第二问", 30),
        ]));
        let sent = migrated.sent().expect("投影该成功");
        assert_eq!(sent.len(), 3, "三条都得在：{sent:?}");
        assert_eq!(
            migrated.log.path().expect("无环").len(),
            5,
            "父链必须完整（含两条记账条目）"
        );
        assert!(
            migrated.notes.iter().any(|note| note.contains("重复")),
            "{:?}",
            migrated.notes
        );
    }

    /// 非法字符的旧 id（比如带点的）不许被原样带进文件名
    #[test]
    fn an_id_that_could_escape_the_directory_is_replaced() {
        let migrated = from_ledger(&ledger(vec![record("../evil", "user", "第一问", 10)]));
        assert_eq!(migrated.sent().expect("投影该成功").len(), 1);
        assert!(migrated
            .log
            .entries()
            .iter()
            .all(|entry| valid_id(&entry.id)));
    }

    /// 接进 `chat_send` 之后的形状：迁移产物 + 本轮新输入 = 纯延长线，这是 Stage 3 的准入门
    #[test]
    fn the_migrated_log_extends_without_rewriting_anything() {
        let mut merged = record("msg_b", "assistant", "有 2 个文件", 20);
        merged.tool_calls = vec![call("call_1", "list_dir", Some("{}"), Some("a\nb"))];
        let migrated = from_ledger(&ledger(vec![record("msg_a", "user", "列目录", 10), merged]));

        let before = migrated.sent().expect("投影该成功");
        let mut ledger_state = PrefixLedger::default();
        ledger_state
            .observe(&before, Cause::Fresh)
            .expect("第一笔没有对照");

        let mut log = migrated.log.clone();
        log.append(
            NewEntry::new(EntryPayload::Message {
                message: Message::User {
                    content: "那看看 a".into(),
                    images: Vec::new(),
                    audios: Vec::new(),
                    videos: Vec::new(),
                },
            }),
            30,
        )
        .expect("追加该成功");
        let after = sent_array(&log).expect("投影该成功");
        ledger_state
            .observe(&after, Cause::Appended)
            .expect("迁移后的话题必须只延长前缀");

        assert_eq!(divergence(&before, &after), before.len());
        assert_eq!(after.len(), before.len() + 1);
    }

    /// 半截回答在旧台账里是"正文非空的一条"，迁移后仍作为落定行发出去（F15 的存档半边）
    #[test]
    fn an_interrupted_answer_kept_in_the_ledger_still_ships() {
        let mut cut = record("msg_b", "assistant", "写到一半就停了", 20);
        cut.error = Some("已停止".into());
        let migrated = from_ledger(&ledger(vec![record("msg_a", "user", "写长文", 10), cut]));
        let sent = migrated.sent().expect("投影该成功");
        assert_eq!(sent[1]["content"], "写到一半就停了");
        let stop = migrated
            .log
            .entries()
            .iter()
            .find_map(|entry| match entry.payload() {
                EntryPayload::Message {
                    message: Message::Assistant(settled),
                } => Some(settled.stop),
                _ => None,
            });
        assert_eq!(
            stop,
            Some(StopReason::Error),
            "带错误的行要标成 error 落定，不能装作正常收尾"
        );
    }

    /// 一条消息映射出的条目数：带 N 个可发调用就是 1+N 条，别的都是 1 条
    #[test]
    fn one_record_maps_to_one_row_per_call_plus_itself() {
        let mut two_calls = record("msg_b", "assistant", "先看看", 20);
        two_calls.tool_calls = vec![
            call("call_1", "read_file", Some("{}"), Some("内容一")),
            call("call_2", "list_dir", Some("{}"), Some("内容二")),
        ];
        assert_eq!(wire_of(&two_calls).len(), 3);
        assert_eq!(wire_of(&record("msg_a", "user", "一句话", 10)).len(), 1);
    }

    /// 第一次打开 = 迁移一次并落盘；再打开读的是日志。台账从此不再权威
    #[test]
    fn reopening_reads_the_log_instead_of_the_stale_ledger() {
        let root = crate::test_support::scoped_temp_dir("legacy-open");
        let conversation = ledger(vec![record("msg_a", "user", "第一问", 10)]);

        let first = Migration::open(&root, &conversation).expect("首次打开该迁出日志");
        assert_eq!(first.log.len(), 3, "两条记账条目 + 一条消息");
        first.save().expect("保存该成功");

        // 往日志里追加一轮，再存
        let mut grown = first.log.clone();
        grown
            .append(
                NewEntry::new(EntryPayload::Message {
                    message: Message::User {
                        content: "第二问".into(),
                        images: Vec::new(),
                        audios: Vec::new(),
                        videos: Vec::new(),
                    },
                }),
                40,
            )
            .expect("追加该成功");
        super::super::store::save(&first.path, &first.header, &grown).expect("再存该成功");

        // 台账还是老样子（前端那份存档没同步更新）——重开必须以日志为准
        let again = Migration::open(&root, &conversation).expect("重开该成功");
        assert_eq!(again.log.len(), 4, "重开读到的必须是日志而不是旧台账");
        assert_eq!(
            again.path, first.path,
            "同一条话题必须原位重写，不能每开一次换个文件"
        );
        assert!(again.notes.is_empty(), "读现成日志不该产生迁移备注");
    }

    /// 反复保存只留一个文件：路径由 header 里的时间戳定，不会漂移
    #[test]
    fn repeated_saves_keep_one_file_per_conversation() {
        let root = crate::test_support::scoped_temp_dir("legacy-one-file");
        let conversation = ledger(vec![record("msg_a", "user", "第一问", 10)]);
        let opened = Migration::open(&root, &conversation).expect("打开该成功");
        for _ in 0..3 {
            opened.save().expect("重复保存该成功");
        }
        let files: Vec<_> = walk(&root)
            .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .collect();
        assert_eq!(files.len(), 1, "一条话题一个文件：{files:?}");
        assert_eq!(files[0], opened.path);
    }

    fn walk(dir: &std::path::Path) -> Box<dyn Iterator<Item = std::path::PathBuf>> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Box::new(std::iter::empty());
        };
        Box::new(entries.flatten().flat_map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                walk(&path)
            } else {
                Box::new(std::iter::once(path)) as Box<dyn Iterator<Item = _>>
            }
        }))
    }

}
