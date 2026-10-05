//! 前缀不变量：**在没有被授权的重置事件介入时，第 N+1 次实发的数组必须以第 N 次为纯延长线。**
//!
//! 这条性质为什么值得单独设一个判据函数（而不是在各条测试里各抄一遍比较条件）：抄一遍
//! 就测的不是真跑的那条路，上一轮改造的 F17 就是这么躲过全部单测的（见设计档 §11.3）。
//!
//! "被授权的重置"只有三种，穷尽列在 [`Cause`] 里：**压缩**（第 0 位换成本身就是它的语义）、
//! **编辑/撤回**（用户点名要改中段）、**回溯**（换一条分支走）。除此之外任何断开都是缺陷——
//! 包括那些"看起来无害"的：把收尾回答并进带工具调用的那条（F13）、丢了附件（F14）、
//! 半截回答只活在界面里（F15）、重新生成时整轮重排（F20）。

use serde::Serialize;
use serde_json::Value;

use super::context::project;
use super::log::SessionLog;

/// 这一次实发相对上一次发生了什么
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    /// 第一笔，没有可比对象
    Fresh,
    /// 只是往日志尾部追加了行
    Appended,
    /// 压缩边界换掉了前缀
    Compacted,
    /// context_edit 替换或撤回了中段某条
    Edited,
    /// 分支末端移动过（回溯、重新生成、编辑重发）
    Navigated,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrefixViolation {
    /// 第 `at` 条起不再是同一批字节
    Diverged {
        at: usize,
        baseline_len: usize,
        sent_len: usize,
    },
    /// 追加之后数组反而变短了：一定有行被吞掉
    Shortened {
        baseline_len: usize,
        sent_len: usize,
    },
}

impl std::fmt::Display for PrefixViolation {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Diverged {
                at,
                baseline_len,
                sent_len,
            } => write!(
                out,
                "前缀在第 {at} 条处断开（基线 {baseline_len} 条，本轮发出 {sent_len} 条）"
            ),
            Self::Shortened {
                baseline_len,
                sent_len,
            } => {
                write!(
                    out,
                    "本轮发出的数组比基线还短（{sent_len} < {baseline_len}）"
                )
            }
        }
    }
}

impl std::error::Error for PrefixViolation {}

/// 两条数组之间，从头相同多少条。等于 `earlier.len()` 就意味着后者是前者的纯延长线
pub fn divergence(earlier: &[Value], later: &[Value]) -> usize {
    earlier
        .iter()
        .zip(later.iter())
        .take_while(|(sent, now)| sent == now)
        .count()
}

/// 本次要发出去的数组：日志在**此刻**的投影。没有第二个来源
pub fn sent_array(log: &SessionLog) -> Result<Vec<Value>, super::SessionError> {
    Ok(project(log)?.wire())
}

/// 逐次核对实发的账本
#[derive(Clone, Debug, Default)]
pub struct PrefixLedger {
    baseline: Vec<Value>,
    resets: usize,
}

impl PrefixLedger {
    /// 记下一笔实发并核对。`Appended` 之外的三种是本条性质允许断开的唯一情形
    pub fn observe(&mut self, sent: &[Value], cause: Cause) -> Result<(), PrefixViolation> {
        match cause {
            Cause::Fresh => {
                self.baseline = sent.to_vec();
                Ok(())
            }
            Cause::Appended => {
                // 顺序有讲究：先判变短。追加永远不可能让数组变短，而"少了行"比
                // "第 N 条断开"更可操作（多半是某行被吞了）；反过来若先比公共前缀，
                // 这条永远轮不到——写第一版时就是这样，是那条阳性对照测试把它抓出来的
                if sent.len() < self.baseline.len() {
                    return Err(PrefixViolation::Shortened {
                        baseline_len: self.baseline.len(),
                        sent_len: sent.len(),
                    });
                }
                let same = divergence(&self.baseline, sent);
                if same < self.baseline.len() {
                    return Err(PrefixViolation::Diverged {
                        at: same,
                        baseline_len: self.baseline.len(),
                        sent_len: sent.len(),
                    });
                }
                self.baseline = sent.to_vec();
                Ok(())
            }
            Cause::Compacted | Cause::Edited | Cause::Navigated => {
                self.resets += 1;
                self.baseline = sent.to_vec();
                Ok(())
            }
        }
    }

    /// 发生过几次被授权的重置。测试用它钉住"断开确实是这两种事件造成的，而不是走偏了"
    pub fn resets(&self) -> usize {
        self.resets
    }

    pub fn baseline_len(&self) -> usize {
        self.baseline.len()
    }
}

#[cfg(test)]
mod tests {
    use super::super::entry::{
        EntryPayload, Message, NewEntry, PendingAssistant, StopReason, ToolCall,
    };
    use super::*;

    const T0: i64 = 1_700_000_000_000;

    fn push(log: &mut SessionLog, payload: EntryPayload) -> String {
        log.append(NewEntry::new(payload), T0)
            .expect("追加该成功")
            .id
            .clone()
    }

    fn log_of(rows: &[EntryPayload]) -> SessionLog {
        let mut log = SessionLog::new();
        for row in rows {
            push(&mut log, row.clone());
        }
        log
    }

    fn user(text: &str) -> EntryPayload {
        EntryPayload::Message {
            message: Message::User {
                content: text.into(),
                images: Vec::new(),
                audios: Vec::new(),
                videos: Vec::new(),
            },
        }
    }

    fn assistant(text: &str) -> EntryPayload {
        EntryPayload::Message {
            message: Message::Assistant(
                PendingAssistant {
                    content: text.into(),
                    tool_calls: vec![],
                }
                .settle(StopReason::Stop),
            ),
        }
    }

    fn assistant_calling(stop: StopReason) -> EntryPayload {
        EntryPayload::Message {
            message: Message::Assistant(
                PendingAssistant {
                    content: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "call_1".into(),
                        name: "read_file".into(),
                        arguments: "{\"path\":\"a.txt\"}".into(),
                        content_chars: None,
                    }],
                }
                .settle(stop),
            ),
        }
    }

    fn tool_result(text: &str) -> EntryPayload {
        EntryPayload::Message {
            message: Message::Tool {
                tool_call_id: "call_1".into(),
                content: text.into(),
            },
        }
    }

    /// 走完一笔实发：取当前投影、按 `cause` 核对、返回发出去的那批字节
    fn send(log: &SessionLog, ledger: &mut PrefixLedger, cause: Cause) -> Vec<Value> {
        let sent = sent_array(log).expect("投影该成功");
        ledger.observe(&sent, cause).expect("这笔实发不该破坏前缀");
        sent
    }

    /// 三条轮次只往尾部加行：每一次实发都必须以上一次为纯延长线
    #[test]
    fn a_plain_conversation_only_ever_extends() {
        let mut log = SessionLog::new();
        let mut ledger = PrefixLedger::default();

        push(&mut log, user("第一问"));
        let first = send(&log, &mut ledger, Cause::Fresh);
        push(&mut log, assistant("答复一"));

        push(&mut log, user("第二问"));
        let second = send(&log, &mut ledger, Cause::Appended);
        push(&mut log, assistant("答复二"));

        push(&mut log, user("第三问"));
        let third = send(&log, &mut ledger, Cause::Appended);

        assert_eq!(divergence(&first, &second), first.len());
        assert_eq!(divergence(&second, &third), second.len());
        assert_eq!(third.len(), 5, "三条问、两条答");
        assert_eq!(ledger.resets(), 0, "纯追加不该有任何重置");
        // 共同部分必须逐字节相同，不是"看起来差不多"
        assert_eq!(&third[..first.len()], &first[..]);
    }

    /// F13 的形状：一轮里模型可以发多条 assistant（带调用的那条 + 收尾那条），
    /// 而界面把它们并成一条。日志按行存，所以回放出来的因果顺序是对的
    #[test]
    fn a_turn_with_tool_calls_ships_the_call_before_the_result_and_the_reply_last() {
        let mut log = SessionLog::new();
        let mut ledger = PrefixLedger::default();

        push(&mut log, user("读一下 a.txt"));
        let opened = send(&log, &mut ledger, Cause::Fresh);
        // 回合内第二次请求：上一行是带调用的 assistant，它已经落库
        push(&mut log, assistant_calling(StopReason::ToolUse));
        let awaiting_tool = send(&log, &mut ledger, Cause::Appended);
        push(&mut log, tool_result("文件内容"));
        let after_tool = send(&log, &mut ledger, Cause::Appended);
        // 收尾回答单独一行——这正是界面台账做不到的那一步
        push(&mut log, assistant("文件里写的是 X"));

        assert_eq!(divergence(&opened, &awaiting_tool), opened.len());
        assert_eq!(divergence(&awaiting_tool, &after_tool), awaiting_tool.len());

        let roles: Vec<&str> = after_tool
            .iter()
            .map(|row| row["role"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(
            roles,
            vec!["user", "assistant", "tool"],
            "顺序必须是问→调用→结果，不能倒过来"
        );
        assert!(
            after_tool[1]["tool_calls"][0]["function"]["name"].is_string(),
            "调用得是嵌套形"
        );
    }

    /// F15 的形状：用户按了停止，那半截他在界面上读过了，就必须也在模型的历史里
    #[test]
    fn the_abandoned_half_of_an_interrupted_answer_still_ships() {
        let mut log = SessionLog::new();
        let mut ledger = PrefixLedger::default();

        push(&mut log, user("写一段长文"));
        send(&log, &mut ledger, Cause::Fresh);
        push(
            &mut log,
            EntryPayload::Message {
                message: Message::Assistant(
                    PendingAssistant {
                        content: "写到一半就停了".into(),
                        tool_calls: vec![],
                    }
                    .settle(StopReason::Aborted),
                ),
            },
        );
        push(&mut log, user("继续"));
        let sent = send(&log, &mut ledger, Cause::Appended);

        let contents: Vec<&str> = sent
            .iter()
            .map(|row| row["content"].as_str().unwrap_or(""))
            .collect();
        assert!(
            contents.contains(&"写到一半就停了"),
            "半截回答不能只活在界面上"
        );
        assert_eq!(contents, vec!["写一段长文", "写到一半就停了", "继续"]);
    }

    /// F14 的形状：附件正文要在实发那一次进历史，之后每轮都还带着它
    #[test]
    fn an_attachment_inline_in_the_user_row_keeps_shipping_next_turn() {
        let mut log = SessionLog::new();
        let mut ledger = PrefixLedger::default();

        push(
            &mut log,
            user("看看这个文件\n--- a.txt ---\n第一行内容\n第二行内容"),
        );
        let with_file = send(&log, &mut ledger, Cause::Fresh);
        push(&mut log, assistant("看到了"));
        push(&mut log, user("第三行是什么"));
        let next = send(&log, &mut ledger, Cause::Appended);

        assert_eq!(
            next[0], with_file[0],
            "带附件那一问必须逐字节原样回来，否则模型第二轮就看不见那个文件"
        );
    }

    /// 压缩是唯一"改第 0 位"的授权事件；它之后仍然只能追加
    #[test]
    fn compaction_is_an_authorized_reset_and_after_it_only_growth_is_allowed() {
        let mut log = SessionLog::new();
        let mut ledger = PrefixLedger::default();

        let kept = push(&mut log, user("要被压进摘要之外的一问"));
        push(&mut log, assistant("答复一"));
        push(
            &mut log,
            EntryPayload::Compaction {
                summary: "前面聊过一件事".into(),
                first_kept_entry_id: kept,
                tokens_before: 5_000,
                usage: None,
                system_message: None,
            },
        );
        let after_compaction = send(&log, &mut ledger, Cause::Compacted);
        push(&mut log, user("压完之后的一问"));
        let grown = send(&log, &mut ledger, Cause::Appended);

        assert_eq!(
            after_compaction[0]["content"].as_str().unwrap(),
            "【上下文摘要】
前面聊过一件事"
        );
        assert_eq!(
            divergence(&after_compaction, &grown),
            after_compaction.len()
        );
        assert_eq!(ledger.resets(), 1);
    }

    /// F20 的形状：回溯之后重新生成，断开只能发生在被保留的那段**之后**
    #[test]
    fn regenerating_on_a_branch_diverges_only_after_the_kept_point() {
        let mut log = SessionLog::new();
        let mut ledger = PrefixLedger::default();

        push(&mut log, user("第一问"));
        let answered = push(&mut log, assistant("旧答复"));
        push(&mut log, user("第二问"));
        let before_rewind = send(&log, &mut ledger, Cause::Fresh);

        // 回溯到"第一问"之后（旧答复是最后一条要保留的），再重新生成
        log.navigate(Some(&answered)).expect("回溯该成功");
        push(&mut log, assistant("新答复"));
        push(&mut log, user("第二问"));
        let after_rewind = send(&log, &mut ledger, Cause::Navigated);

        // 被保留的那一段一个字节都没重排：断开正好发生在它的末尾之后
        let kept = divergence(&before_rewind, &after_rewind);
        assert_eq!(
            kept, 2,
            "前两条（问、保留的答）必须原封不动，实盘上这里曾整轮重排"
        );
        assert_eq!(&after_rewind[..kept], &before_rewind[..kept]);
        assert_eq!(ledger.resets(), 1);
    }

    /// 撤回一段内容也是授权断开
    #[test]
    fn withdrawing_a_row_is_an_authorized_break() {
        let mut log = SessionLog::new();
        let mut ledger = PrefixLedger::default();

        let slip = push(&mut log, user("这句不该发出去"));
        push(&mut log, assistant("答复"));
        send(&log, &mut ledger, Cause::Fresh);
        push(
            &mut log,
            EntryPayload::ContextEdit {
                target_id: slip,
                replacement: None,
            },
        );
        let sent = send(&log, &mut ledger, Cause::Edited);

        assert_eq!(sent.len(), 1, "撤回的那条不该再发");
        assert_eq!(ledger.resets(), 1);
    }

    /// 门禁本身：判据必须**能红**。中段偷偷改一个字节、却不声明任何授权事件时，
    /// 这笔实发要被拒——否则整个不变量只是装饰
    #[test]
    fn a_silent_reword_in_the_middle_is_rejected() {
        let mut log = SessionLog::new();
        let mut ledger = PrefixLedger::default();

        push(&mut log, user("第一问"));
        push(&mut log, assistant("答复一"));
        push(&mut log, user("第二问"));
        let honest = sent_array(&log).expect("投影该成功");
        ledger
            .observe(&honest, Cause::Fresh)
            .expect("第一笔不该有可比对象");

        // 装作"只追加了一行"，实际把中段那条答复改了（就是界面台账会做的事）
        let mut tampered = honest.clone();
        tampered[1] = serde_json::json!({ "role": "assistant", "content": "答复一改了字" });
        let violation = ledger
            .observe(&tampered, Cause::Appended)
            .expect_err("中段改写必须被抓到");

        assert_eq!(
            violation,
            PrefixViolation::Diverged {
                at: 1,
                baseline_len: honest.len(),
                sent_len: tampered.len()
            }
        );
        assert_eq!(
            violation.to_string(),
            "前缀在第 1 条处断开（基线 3 条，本轮发出 3 条）"
        );
    }

    /// 只追加却变短，意味着有行被吞掉（工具结果最容易掉在这）
    #[test]
    fn an_appended_round_that_gets_shorter_is_rejected() {
        let with_answer = log_of(&[user("第一问"), assistant("答复一")]);
        let lost_the_answer = log_of(&[user("第一问")]);
        let mut ledger = PrefixLedger::default();
        ledger
            .observe(&sent_array(&with_answer).expect("投影该成功"), Cause::Fresh)
            .expect("第一笔不该有可比对象");

        let violation = ledger
            .observe(
                &sent_array(&lost_the_answer).expect("投影该成功"),
                Cause::Appended,
            )
            .expect_err("数组变短必须被抓到");
        assert_eq!(
            violation,
            PrefixViolation::Shortened {
                baseline_len: 2,
                sent_len: 1
            }
        );
    }

    /// 判据本身：锁步前进、在第一个不同处停下。用"内容重复出现"的形状钉住它——
    /// 若有人把延长线写成"在前面那批里找相同行"或"取集合交集"，这个形状就会算错
    #[test]
    fn the_criterion_walks_in_lockstep_and_stops_at_the_first_difference() {
        let row =
            |role: &str, content: &str| serde_json::json!({ "role": role, "content": content });
        let baseline = vec![
            row("user", "同一句"),
            row("assistant", "答"),
            row("user", "同一句"),
        ];

        // 纯延长线（第三条与第一条内容重复，仍算相同）
        let extended = vec![
            row("user", "同一句"),
            row("assistant", "答"),
            row("user", "同一句"),
            row("assistant", "再答一次"),
        ];
        assert_eq!(divergence(&baseline, &extended), baseline.len());

        // 中段被换：只认出改点之前那截
        let tampered = vec![
            row("user", "同一句"),
            row("assistant", "改了字"),
            row("user", "同一句"),
        ];
        assert_eq!(divergence(&baseline, &tampered), 1);

        // 反过来变短：相同前缀长度封顶在较短那条上，由 Shortened 另外判
        assert_eq!(divergence(&baseline, &baseline[..2]), 2);
        assert_eq!(divergence(&baseline, &baseline), baseline.len());
    }
}
