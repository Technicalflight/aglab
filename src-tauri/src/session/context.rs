//! 唯一投影：条目树 → 这一轮要发出去的消息数组。
//!
//! 这是全模块里唯一能产出"发给模型的东西"的地方，意义在于**分叉写不出来**：上一轮改造
//! 留下前端台账与后端日志两份真相，于是每轮要在形状、文案、顺序三处各猜一次该信哪份；
//! 这里只有一条从 [`SessionLog::path`] 到 [`project`] 的直路，没有第二个出口可漂移。
//!
//! [`project`] 对条目类别是**穷尽匹配、没有 `_` 分支**：新增一类条目而忘了考虑它怎么进
//! 上下文，会在编译期就停住，而不是等上线后静默失忆。

use std::collections::HashMap;

use serde::Serialize;
use serde_json::Value;

use super::entry::{Entry, EntryPayload, Message};
use super::log::SessionLog;

/// 摘要行的前缀标记。它是**协议的一部分**：下一次压缩靠它认出旧摘要、走增量更新，
/// 所以生产方（这里）与解析方（`chat.rs` 的 `summary_prompt`）必须共用同一个常量
pub const SUMMARY_MARKER: &str = "【上下文摘要】";

/// 分支/按层摘要那行的前缀标记。跟上面那个同理，**它是协议不是排版**：改它就是改前缀，
/// 所以拼它的一方（投影）与认它的一方都得从这里取，不许在别处再写一遍字面量
pub const BRANCH_MARKER: &str = "【分支摘要】";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelStamp {
    pub provider: String,
    pub model_id: String,
}

/// 投影没带上某条条目的原因。读侧要回答"这些字节为什么不在"，只能问投影本身：
/// 任何在别处重走一遍选边逻辑的写法，都会在压缩边界上错位一格
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Omitted {
    /// 落在压缩边界之前，此后只以摘要的形状存在
    CompactedAway,
    /// 落在一段按层压缩的摘要里，此后只以那行摘要的形状存在
    SummariedAway,
    /// 保留窗里那句旧的 system：常驻段每轮由装配处按段给出，重发就跟当前段打架
    StaleSystemRow,
    /// 被后来的压缩取代，自身零贡献
    SupersededCompaction,
    /// `context_edit` 把它从上下文里撤掉了。原始条目一行都没改
    Withdrawn,
}

/// 这一轮生效的那一次改写。撤销只能对着它做——被它顶替掉的那些条目自己既看不见也撤不掉，
/// 而"顶替了几行、多少字节"也只有投影知道（别人算一遍就会在边界上错位一格）
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RewriteKind {
    /// 压缩：摘要站在数组头部，整个前缀从此重付一次
    Compaction,
    /// 按层压缩：只换中间那一段，`from` 之前的字节一个都不动
    Span,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Rewrite {
    pub entry_id: String,
    pub kind: RewriteKind,
    pub replaced_rows: usize,
    pub replaced_chars: usize,
}

/// 一次省略：哪条条目、为什么、以及它本来要占多少字节。字节量按它自己的投影形状算，
/// 所以"被省掉的量"跟"发出去的账"是同一个口径
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Omission {
    pub entry_id: String,
    pub omitted: Omitted,
    pub chars: usize,
}

/// 投影结果。`entries` 带着每一段消息的来源条目 id，界面按它渲染（Stage 4），
/// 这样"界面上那条"和"模型见过那条"是同一个东西的两个视图，而不是两份记录
#[derive(Clone, Debug)]
pub struct Projection {
    pub entries: Vec<(String, Vec<Message>)>,
    pub model: Option<ModelStamp>,
    /// 与 `entries` 同源的一份账：路径上那些**没**变成字节的条目。它只能由投影产生，
    /// 所以 Inspector 的"谁被裁了"不可能跟实发的对不上
    pub omissions: Vec<Omission>,
    /// 这一轮生效的那次改写。没有改写时它是 `None`
    pub rewrite: Option<Rewrite>,
}

impl Projection {
    /// 投影的第 i 项：来自哪条条目、发出哪几行。顺序与日志在本轮挑中的那批条目严格一致，
    /// 读侧按条目统计层靠的就是这条对应（`omissions` 走的也是同一次选择）
    pub fn rows(&self) -> impl Iterator<Item = (&str, &[Message])> + '_ {
        self.entries
            .iter()
            .map(|(id, messages)| (id.as_str(), messages.as_slice()))
    }

    pub fn messages(&self) -> Vec<&Message> {
        self.entries
            .iter()
            .flat_map(|(_, messages)| messages.iter())
            .collect()
    }

    pub fn wire(&self) -> Vec<Value> {
        self.messages()
            .iter()
            .map(|message| message.to_wire())
            .collect()
    }

    /// 与 `wire()` **逐行对齐**的条目 id：第 i 个就是第 i 行出自哪条条目。
    ///
    /// 需要它是因为投影行和条目**不是一一对应**的（一条带段快照的压缩条目会展开成两行）。
    /// 按条目数出来的下标去查按行数出来的东西，会整条尾巴错位一格——而错位是静默的
    pub fn row_origins(&self) -> Vec<&str> {
        self.entries
            .iter()
            .flat_map(|(id, messages)| messages.iter().map(move |_| id.as_str()))
            .collect()
    }
}

/// 这一轮要发出去的消息数组。投影的唯一对外形状
pub fn project(log: &SessionLog) -> Result<Projection, super::SessionError> {
    let path = log.path()?;
    // 编辑要从**整条路径**上算，不能只从挑中的那些里算：这一轮生效的是哪一次改写，
    // 取决于有没有一行把它撤掉，而那一行排在被撤的那条后面
    let mut edits: HashMap<&str, Option<&String>> = HashMap::new();
    for entry in path.iter() {
        if let EntryPayload::ContextEdit {
            target_id,
            replacement,
        } = entry.payload()
        {
            edits.insert(target_id.as_str(), replacement.as_ref());
        }
    }
    let (selected, mut omissions, active) = context_entries(&path, &edits);
    let mut entries = Vec::with_capacity(selected.len());
    for entry in selected.iter() {
        // 同一个目标被编辑多次，后写的赢：撤回再重发不是回滚状态，是追加一条新事实
        let edit = edits.get(entry.id.as_str()).copied();
        let messages = if matches!(edit, Some(None)) {
            // 撤掉一条边界，被它顶替的那些条目就原样回来：这条分支必须在"取代"之前，
            // 否则撤销一次压缩会被记成"被更晚的改写顶掉"，而那两个是完全不同的两件事
            omissions.push(omission(entry, Omitted::Withdrawn));
            Vec::new()
        } else if is_rewrite(&path, entry) && active.as_deref() != Some(entry.id.as_str()) {
            // 被更晚的那次改写取代的旧边界零贡献：反复改写不会套娃
            omissions.push(omission(entry, Omitted::SupersededCompaction));
            Vec::new()
        } else {
            projected_messages(entry.payload(), edit)
        };
        entries.push((entry.id.clone(), messages));
    }

    Ok(Projection {
        entries,
        model: model_of(&path),
        rewrite: rewrite_of(log, &omissions, active.as_deref()),
        omissions,
    })
}

/// 生效的那次改写报什么。被顶替的几行与它们的字节量只能从这份省略账里数——
/// 别人再走一遍选边逻辑就会错位一格，而那一格错掉的是"压错了地方"
fn rewrite_of(log: &SessionLog, omissions: &[Omission], active: Option<&str>) -> Option<Rewrite> {
    let entry_id = active?;
    let replaced: Vec<&Omission> = omissions
        .iter()
        .filter(|item| {
            matches!(
                item.omitted,
                Omitted::CompactedAway | Omitted::SummariedAway
            )
        })
        .collect();
    Some(Rewrite {
        entry_id: entry_id.to_string(),
        kind: match log.entry(entry_id).map(|entry| entry.payload()) {
            Some(EntryPayload::BranchSummary { .. }) => RewriteKind::Span,
            _ => RewriteKind::Compaction,
        },
        replaced_rows: replaced.len(),
        replaced_chars: replaced.iter().map(|item| item.chars).sum(),
    })
}

fn omission(entry: &Entry, omitted: Omitted) -> Omission {
    Omission {
        entry_id: entry.id.clone(),
        omitted,
        chars: projected_messages(entry.payload(), None)
            .iter()
            .map(Message::wire_chars)
            .sum(),
    }
}

/// 这一轮生效的那一次改写。两种形状**互斥**：只有最新的那一条边界站得住，更早的那些零贡献。
/// 两条同时生效的话，"撤销一次改写"就答不出该回到哪一版
enum Boundary<'a> {
    /// 压缩：摘要站在数组头部，`first_kept` 之前的历史从此只以摘要的形状存在
    Compaction(&'a Entry),
    /// 按层压缩：摘要站在 `from` 那一格上，只顶替 `from..=to` 那一段（含两端）。
    /// `from` 之前那批字节一个都不动——这就是它与压缩的分工：压缩从头部断开（整个前缀换），
    /// 这一段只换中间，所以它能压一段旧对话而不把已发过的前缀掀掉
    Span(&'a Entry, usize, usize),
}

/// 能不能在这条路径上解出一段顶替范围。解不出来（两个 id 有一个不在路径上，
/// 比如分叉从别处带过来的那份摘要）它就只是一行摘要，顶替不了任何东西
fn span_bounds(
    path: &[&Entry],
    summary: &Entry,
    from: &str,
    through: &str,
) -> Option<(usize, usize)> {
    let from_index = path.iter().position(|entry| entry.id == from)?;
    let to_index = path.iter().position(|entry| entry.id == through)?;
    let own_index = path.iter().position(|entry| entry.id == summary.id)?;
    // 摘要必须排在它顶替的那一段之后：一段还没写完的对话不能被自己摘要掉
    if from_index > to_index || to_index >= own_index {
        return None;
    }
    Some((from_index, to_index))
}

/// 这条目在这一轮里是不是一次"改写"。见 [`span_bounds`]：分支摘要只有解得出范围时才算
fn is_rewrite(path: &[&Entry], entry: &Entry) -> bool {
    match entry.payload() {
        EntryPayload::Compaction { .. } => true,
        EntryPayload::BranchSummary {
            from_id: Some(from),
            through_id: Some(through),
            ..
        } => span_bounds(path, entry, from, through).is_some(),
        _ => false,
    }
}

/// 生效的那一次改写是谁——从尾往前第一个**没被撤掉**、且解得出范围的边界。
/// 撤掉的那条不当边界：它顶替掉的那些条目就此原样回来，这正是"撤销一次压缩"的全部含义
fn active_boundary<'a>(
    path: &[&'a Entry],
    edits: &HashMap<&str, Option<&String>>,
) -> Option<Boundary<'a>> {
    let withdrawn = |entry: &Entry| matches!(edits.get(entry.id.as_str()).copied(), Some(None));
    for index in (0..path.len()).rev() {
        let entry = path[index];
        if withdrawn(entry) {
            continue;
        }
        match entry.payload() {
            EntryPayload::Compaction { .. } => return Some(Boundary::Compaction(entry)),
            EntryPayload::BranchSummary {
                from_id: Some(from),
                through_id: Some(through),
                ..
            } => {
                if let Some((from_index, to_index)) = span_bounds(path, entry, from, through) {
                    return Some(Boundary::Span(entry, from_index, to_index));
                }
            }
            _ => {}
        }
    }
    None
}

/// 挑中这一轮要发的条目、被挑剩的那些为什么没进来，以及生效的那次改写的 id
fn context_entries<'a>(
    path: &[&'a Entry],
    edits: &HashMap<&str, Option<&String>>,
) -> (Vec<&'a Entry>, Vec<Omission>, Option<String>) {
    let Some(boundary) = active_boundary(path, edits) else {
        return (path.to_vec(), Vec::new(), None);
    };
    let (active_id, selected, omitted) = match boundary {
        Boundary::Compaction(entry) => from_compaction(path, entry),
        Boundary::Span(entry, from_index, to_index) => from_span(path, entry, from_index, to_index),
    };
    (selected, omitted, Some(active_id))
}

/// 压缩边界：摘要在第一格，保留窗从 `first_kept` 起，边界之后的原样跟着
fn from_compaction<'a>(
    path: &[&'a Entry],
    boundary: &'a Entry,
) -> (String, Vec<&'a Entry>, Vec<Omission>) {
    let index = path
        .iter()
        .position(|entry| entry.id == boundary.id)
        .expect("刚在这里找到它");
    let first_kept = match boundary.payload() {
        EntryPayload::Compaction {
            first_kept_entry_id,
            ..
        } => first_kept_entry_id,
        _ => unreachable!("调用方已经按压缩条目筛过"),
    };

    let mut selected = vec![boundary];
    let mut omitted = Vec::new();
    let mut keeping = false;
    for entry in &path[..index] {
        if !keeping && entry.id == *first_kept {
            keeping = true;
        }
        // 保留窗里不重发旧的 system：常驻段每轮由装配处按段给出
        if keeping
            && !matches!(
                entry.payload(),
                EntryPayload::Message {
                    message: Message::System { .. }
                }
            )
        {
            selected.push(entry);
        } else if keeping {
            omitted.push(omission(entry, Omitted::StaleSystemRow));
        } else {
            omitted.push(omission(entry, Omitted::CompactedAway));
        }
    }
    selected.extend_from_slice(&path[index + 1..]);
    (boundary.id.clone(), selected, omitted)
}

/// 按层压缩的那一段：摘要站到 `from_index` 那一格上，`from..=to` 之间的那些退场，
/// 其余一切照路径原序。摘要自己那一格不再重复出现——它已经站到前面去了
fn from_span<'a>(
    path: &[&'a Entry],
    summary: &'a Entry,
    from_index: usize,
    to_index: usize,
) -> (String, Vec<&'a Entry>, Vec<Omission>) {
    let mut selected = Vec::with_capacity(path.len());
    let mut omitted = Vec::new();
    for (index, entry) in path.iter().enumerate() {
        if index == from_index {
            // `from` 那一格也是被顶替掉的：摘要站到它的位置上，省略账里也必须它有——
            // 少记这一行，Inspector 的"谁被裁了"就少一条，省下的字节也对不上
            omitted.push(omission(entry, Omitted::SummariedAway));
            selected.push(summary);
        } else if index > from_index && index <= to_index {
            omitted.push(omission(entry, Omitted::SummariedAway));
        } else if entry.id != summary.id {
            // 摘要自己那一格不再重复出现：它已经站到被顶替那一段的第一格上去了
            selected.push(entry);
        }
    }
    (summary.id.clone(), selected, omitted)
}

/// 一条**还没写进日志**的条目如果写进去会占多少字节。口径与 `uses()` 读到的完全同源：
/// 它走的就是投影用来渲染那一条的同一个函数，所以"装配前预检的量"和"写完之后的实测量"
/// 不是两套估算。自己再渲染一遍就是第二个真相，那正是这一轮要堵掉的东西
pub fn payload_chars(payload: &EntryPayload) -> usize {
    projected_messages(payload, None)
        .iter()
        .map(Message::wire_chars)
        .sum()
}

fn projected_messages(payload: &EntryPayload, edit: Option<Option<&String>>) -> Vec<Message> {
    if let Some(replacement) = edit {
        // 撤回：投影里没有这条；替换：只换正文，角色与工具配对都保留
        let Some(replacement) = replacement else {
            return Vec::new();
        };
        return match payload {
            EntryPayload::Message { message } => vec![message.with_content(replacement)],
            EntryPayload::CustomMessage { .. } => {
                vec![Message::System {
                    content: replacement.clone(),
                }]
            }
            _ => Vec::new(),
        };
    }
    match payload {
        EntryPayload::Message { message } => vec![message.clone()],
        EntryPayload::CustomMessage { content, .. } => {
            // 钩子补的上下文今天就以 system 身份进线程，投影保持同一个角色，不改语义
            vec![Message::System {
                content: content.clone(),
            }]
        }
        EntryPayload::Compaction {
            summary,
            system_message,
            ..
        } => {
            let mut messages = Vec::new();
            if let Some(system) = system_message {
                messages.push(system.clone());
            }
            messages.push(Message::System {
                content: format!(
                    "{SUMMARY_MARKER}
{summary}"
                ),
            });
            messages
        }
        EntryPayload::BranchSummary { summary, .. } => {
            vec![Message::System {
                content: format!("{BRANCH_MARKER}{summary}"),
            }]
        }
        // 这三类不进上下文：一条不落。新增类别时编译器会把你叫回来
        EntryPayload::ContextEdit { .. }
        | EntryPayload::ModelChange { .. }
        | EntryPayload::Usage { .. }
        | EntryPayload::SessionInfo { .. }
        | EntryPayload::Custom { .. } => Vec::new(),
    }
}

fn model_of(path: &[&Entry]) -> Option<ModelStamp> {
    path.iter().rev().find_map(|entry| match entry.payload() {
        EntryPayload::ModelChange { provider, model_id } => Some(ModelStamp {
            provider: provider.clone(),
            model_id: model_id.clone(),
        }),
        _ => None,
    })
}

/// 路径上最新的某类 `custom` 条目的正文。装配处用它读回"首轮定形"的东西（工具声明数组）。
/// `Custom` 一类不进上下文（见 `projected_messages`），它记的是事实而不是消息
pub fn latest_custom<'a>(
    log: &'a SessionLog,
    custom_type: &str,
) -> Result<Option<&'a Value>, super::SessionError> {
    Ok(latest_custom_entry(log, custom_type)?.map(|(_, data)| data))
}

/// 同上一格，但把那一行的条目 id 一起带出来。`goal_id` 的补铸认它：
/// 旧行没有身份，读侧按"这一条条目"铸一个，不伪造创建时间
pub fn latest_custom_entry<'a>(
    log: &'a SessionLog,
    custom_type: &str,
) -> Result<Option<(&'a str, &'a Value)>, super::SessionError> {
    let path = log.path()?;
    Ok(path.iter().rev().find_map(|entry| match entry.payload() {
        EntryPayload::Custom {
            custom_type: kind,
            data,
        } if kind == custom_type => Some((entry.id.as_str(), data.as_ref()?)),
        _ => None,
    }))
}

/// 这一轮是不是"压缩之后的第一笔"：路径上最新的压缩条目比最新的落定回答还靠后。
///
/// 为什么要单独判这一条：压缩是**授权**的前缀断开，压缩后第一轮的低命中不是浪费，而是
/// 那一次压缩买来的代价。不把它分开，面板会在每次压缩后固定报一笔虚构的白付量（§7.3）。
/// 只看落定回答，不看用户行：本轮刚追加的那句问题在压缩条目之后，但它不意味着旧前缀还在。
pub fn starts_fresh_chain(log: &SessionLog) -> Result<bool, super::SessionError> {
    let path = log.path()?;
    let Some(compaction_seq) = path.iter().rev().find_map(|entry| match entry.payload() {
        EntryPayload::Compaction { .. } => Some(entry.seq),
        _ => None,
    }) else {
        return Ok(false);
    };
    let last_answer = path.iter().rev().find_map(|entry| match entry.payload() {
        EntryPayload::Message {
            message: Message::Assistant(_),
        } => Some(entry.seq),
        _ => None,
    });
    Ok(last_answer.is_none_or(|answer_seq| compaction_seq > answer_seq))
}

#[cfg(test)]
mod tests {
    use super::super::entry::{PendingAssistant, Role, StopReason, ToolCall};
    use super::super::log::SessionLog;
    use super::*;

    const T0: i64 = 1_700_000_000_000;

    fn push(log: &mut SessionLog, payload: EntryPayload) -> String {
        log.append(super::super::entry::NewEntry::new(payload), T0)
            .expect("追加该成功")
            .id
            .clone()
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

    /// 一条以单次 read_file 调用收尾的答复
    fn assistant_call() -> EntryPayload {
        EntryPayload::Message {
            message: Message::Assistant(
                PendingAssistant {
                    content: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "call_1".into(),
                        name: "read_file".into(),
                        arguments: "{\"path\":\"a\"}".into(),
                        content_chars: None,
                    }],
                }
                .settle(StopReason::ToolUse),
            ),
        }
    }

    fn contents(projection: &Projection) -> Vec<String> {
        projection
            .messages()
            .iter()
            .map(|message| match message {
                Message::System { content } | Message::User { content, .. } => content.clone(),
                Message::Assistant(settled) => settled.content.clone(),
                Message::Tool { content, .. } => content.clone(),
            })
            .collect()
    }

    #[test]
    fn a_plain_history_projects_in_order_with_its_roles() {
        let mut log = SessionLog::new();
        push(&mut log, user("第一问"));
        push(&mut log, assistant("答复"));
        push(&mut log, user("第二问"));
        let projection = project(&log).expect("投影该成功");
        assert_eq!(contents(&projection), vec!["第一问", "答复", "第二问"]);
        assert_eq!(
            projection
                .messages()
                .iter()
                .map(|m| m.role())
                .collect::<Vec<_>>(),
            vec![Role::User, Role::Assistant, Role::User]
        );
    }

    /// 不进上下文的类别必须一分不占
    #[test]
    fn bookkeeping_entries_contribute_nothing() {
        let mut log = SessionLog::new();
        push(
            &mut log,
            EntryPayload::Usage {
                kind: "turn".into(),
                provider: "p".into(),
                model: "m".into(),
                usage: super::super::entry::UsageRecord {
                    input_tokens: 10,
                    output_tokens: 2,
                    cached_tokens: None,
                    cache_write_tokens: 0,
                },
                note: None,
            },
        );
        push(&mut log, user("第一问"));
        push(
            &mut log,
            EntryPayload::SessionInfo {
                name: Some("标题".into()),
            },
        );
        let projection = project(&log).expect("投影该成功");
        assert_eq!(contents(&projection), vec!["第一问"]);
    }

    /// 撤回只活在投影里：原始条目一行都不改，重读日志还能看见它
    #[test]
    fn withdrawing_a_message_removes_it_from_the_context_only() {
        let mut log = SessionLog::new();
        let asked = push(&mut log, user("要被撤回的那问"));
        push(&mut log, assistant("答复"));
        push(
            &mut log,
            EntryPayload::ContextEdit {
                target_id: asked,
                replacement: None,
            },
        );
        let projection = project(&log).expect("投影该成功");
        assert_eq!(contents(&projection), vec!["答复"]);
        assert_eq!(log.len(), 3, "原始条目必须还在");
    }

    /// 替换保留角色与工具配对，只换正文
    #[test]
    fn replacing_content_keeps_the_role_and_the_call_pairing() {
        let mut log = SessionLog::new();
        let called = push(&mut log, assistant_call());
        push(
            &mut log,
            EntryPayload::ContextEdit {
                target_id: called,
                replacement: Some("改过的正文".into()),
            },
        );
        let projection = project(&log).expect("投影该成功");
        let Message::Assistant(settled) = projection.messages()[0] else {
            panic!("角色不该被编辑改掉：{:?}", projection.messages()[0])
        };
        assert_eq!(settled.content, "改过的正文");
        assert_eq!(settled.tool_calls.len(), 1, "工具调用不能因为改正文而丢");
        assert_eq!(settled.stop, StopReason::ToolUse);
    }

    /// 同一个目标编辑两次，后写的赢
    #[test]
    fn the_last_edit_on_a_target_wins() {
        let mut log = SessionLog::new();
        let asked = push(&mut log, user("原话"));
        push(
            &mut log,
            EntryPayload::ContextEdit {
                target_id: asked.clone(),
                replacement: Some("第一次改".into()),
            },
        );
        push(
            &mut log,
            EntryPayload::ContextEdit {
                target_id: asked,
                replacement: Some("第二次改".into()),
            },
        );
        let projection = project(&log).expect("投影该成功");
        assert_eq!(contents(&projection), vec!["第二次改"]);
    }

    /// 压缩边界：摘要在头部，保留窗是真条目，边界与被放弃的旧历史都不再出现
    #[test]
    fn a_compaction_replaces_the_prefix_but_keeps_the_tail_as_real_entries() {
        let mut log = SessionLog::new();
        push(&mut log, user("早就被压掉的一问"));
        let kept = push(&mut log, user("保留窗里的一问"));
        push(&mut log, assistant("保留窗里的一答"));
        let boundary = push(
            &mut log,
            EntryPayload::Compaction {
                summary: "前面聊了 X".into(),
                first_kept_entry_id: kept.clone(),
                tokens_before: 900,
                usage: None,
                system_message: Some(Message::System {
                    content: "你是助手".into(),
                }),
            },
        );
        push(&mut log, user("压完之后的一问"));
        let projection = project(&log).expect("投影该成功");
        assert_eq!(
            contents(&projection),
            vec![
                "你是助手",
                "【上下文摘要】
前面聊了 X",
                "保留窗里的一问",
                "保留窗里的一答",
                "压完之后的一问"
            ]
        );
        assert_eq!(log.len(), 5, "压缩不删条目：被压掉的历史仍然在日志里");
        // 那一条压缩带了段快照，所以它占两行——行与条目就是这么错开第一格的。
        // 拿按条目的下标去查按行的数组（旧 `Send::provenance` 干的正是这件事），
        // 从这里开始每一格都往前偏一个，压缩边界就会挑到上一条条目上
        let origins = projection.row_origins();
        assert_eq!(
            origins.len(),
            projection.wire().len(),
            "对照表不逐行对齐，它就没有存在的意义"
        );
        assert_eq!(
            (origins[0], origins[1]),
            (boundary.as_str(), boundary.as_str()),
            "一条压缩条目展开成的两行不能算两个来源"
        );
        assert_eq!(
            origins[2],
            kept.as_str(),
            "保留窗第一行的来源必须是保留窗那条条目，不是它前面那条"
        );
    }

    /// 保留窗里的旧 system 不重发：常驻段每轮由装配处给，历史里留着它会和当前段打架
    /// 保留窗里的旧 system 不重发：常驻段每轮由装配处按段给出，历史里留着它就跟当前段打架。
    /// 压缩边界要落在**那条 system 行上**才走得到这条过滤——设计档 §6.1 把常驻段写进日志之后，
    /// 这正是选边的常态，所以它不是理论分支
    #[test]
    fn the_retained_window_does_not_carry_an_old_system_message() {
        let mut log = SessionLog::new();
        let stale = push(&mut log, system("旧约定"));
        push(&mut log, user("窗口里的第二问"));
        push(&mut log, user("保留的第三问"));
        push(
            &mut log,
            EntryPayload::Compaction {
                summary: "摘要".into(),
                first_kept_entry_id: stale,
                tokens_before: 10,
                usage: None,
                system_message: None,
            },
        );
        let projection = project(&log).expect("投影该成功");
        assert_eq!(
            contents(&projection),
            vec![
                "【上下文摘要】
摘要",
                "窗口里的第二问",
                "保留的第三问"
            ],
            "边界之后的行都该保留，但那句旧 system 不该跟着回来"
        );
    }

    fn system(text: &str) -> EntryPayload {
        EntryPayload::Message {
            message: Message::System {
                content: text.into(),
            },
        }
    }

    /// 反复压缩不套娃：旧压缩若落在保留窗里，会被选中但**零贡献**。
    /// 故意把第二次压缩的边界摆在第一次之前，让这条规则真的被走到
    #[test]
    fn a_superseded_compaction_contributes_nothing() {
        let mut log = SessionLog::new();
        let first = push(&mut log, user("第一问"));
        push(
            &mut log,
            EntryPayload::Compaction {
                summary: "第一次摘要".into(),
                first_kept_entry_id: first.clone(),
                tokens_before: 10,
                usage: None,
                system_message: None,
            },
        );
        push(&mut log, user("第二问"));
        push(
            &mut log,
            EntryPayload::Compaction {
                summary: "第二次摘要".into(),
                first_kept_entry_id: first,
                tokens_before: 20,
                usage: None,
                system_message: None,
            },
        );
        let projection = project(&log).expect("投影该成功");
        assert_eq!(
            contents(&projection),
            vec![
                "【上下文摘要】
第二次摘要",
                "第一问",
                "第二问"
            ]
        );
        assert!(
            !contents(&projection)
                .iter()
                .any(|text| text.contains("第一次摘要")),
            "被取代的旧压缩不能把自己的摘要也发出去"
        );
    }

    /// 边界若被截到已放弃的分支上，它就整体失效：投影不该因此崩掉
    #[test]
    fn a_compaction_off_the_current_path_simply_does_not_apply() {
        let mut log = SessionLog::new();
        let root = push(&mut log, user("共同的一点"));
        push(
            &mut log,
            EntryPayload::Compaction {
                summary: "只在另一条分支上成立的摘要".into(),
                first_kept_entry_id: root.clone(),
                tokens_before: 10,
                usage: None,
                system_message: None,
            },
        );
        log.navigate(Some(&root)).expect("回溯该成功");
        push(&mut log, user("换分支后的一问"));
        let projection = project(&log).expect("投影该成功");
        assert_eq!(contents(&projection), vec!["共同的一点", "换分支后的一问"]);
    }

    /// 换模型是话题事实，扫整条路径，不受压缩截断影响
    #[test]
    fn the_model_comes_from_the_whole_path() {
        let mut log = SessionLog::new();
        let kept = push(
            &mut log,
            EntryPayload::ModelChange {
                provider: "deepseek".into(),
                model_id: "v3".into(),
            },
        );
        push(&mut log, user("第一问"));
        push(
            &mut log,
            EntryPayload::ModelChange {
                provider: "openai".into(),
                model_id: "gpt".into(),
            },
        );
        push(
            &mut log,
            EntryPayload::Compaction {
                summary: "摘要".into(),
                first_kept_entry_id: kept,
                tokens_before: 10,
                usage: None,
                system_message: None,
            },
        );
        let projection = project(&log).expect("投影该成功");
        assert_eq!(
            projection.model,
            Some(ModelStamp {
                provider: "openai".into(),
                model_id: "gpt".into()
            })
        );
    }

    /// 出站字节：工具调用必须是嵌套形，UI 字段一个都不能漏上去
    #[test]
    fn the_wire_shape_nests_tool_calls_and_carries_no_ui_fields() {
        let mut log = SessionLog::new();
        push(&mut log, assistant_call());
        push(
            &mut log,
            EntryPayload::Message {
                message: Message::Tool {
                    tool_call_id: "call_1".into(),
                    content: "文件内容".into(),
                },
            },
        );
        let wire = project(&log).expect("投影该成功").wire();
        assert_eq!(wire[0]["tool_calls"][0]["function"]["name"], "read_file");
        assert_eq!(wire[0]["tool_calls"][0]["type"], "function");
        assert_eq!(wire[1]["role"], "tool");
        assert_eq!(wire[1]["tool_call_id"], "call_1");
    }

    /// 压缩只让**紧接的那一轮**免计白付，往后新前缀就成立了。判据只看日志形状，不碰服务商
    #[test]
    fn only_the_first_round_after_a_compaction_starts_a_fresh_chain() {
        let probe = |log: &SessionLog| starts_fresh_chain(log).expect("路径该能走通");
        let mut log = SessionLog::new();
        push(&mut log, user("第一问"));
        let answer = push(&mut log, assistant("答复"));
        assert!(!probe(&log), "没压缩就没有授权断开");

        push(
            &mut log,
            EntryPayload::Compaction {
                summary: "前面聊了 X".into(),
                first_kept_entry_id: answer.clone(),
                tokens_before: 900,
                usage: None,
                system_message: None,
            },
        );
        assert!(probe(&log), "压缩后的第一笔该断开基线");

        // 本轮刚追加的那句问题也排在压缩条目之后，但它不代表旧前缀还在
        push(&mut log, user("第二问"));
        assert!(probe(&log), "只有落定回答才把链接回去");

        push(&mut log, assistant("答复二"));
        assert!(
            !probe(&log),
            "压缩之后的新前缀已经建立，再漏命中就是真白付了"
        );
    }

    fn span(from: &str, through: &str, summary: &str) -> EntryPayload {
        EntryPayload::BranchSummary {
            from_id: Some(from.into()),
            through_id: Some(through.into()),
            summary: summary.into(),
            usage: None,
        }
    }

    fn omitted_for(projection: &Projection, id: &str) -> Option<Omitted> {
        projection
            .omissions
            .iter()
            .find(|item| item.entry_id == id)
            .map(|item| item.omitted)
    }

    /// T07 判据之一：按层压缩只顶替那一段，`from` 之前的字节一个都不动。
    /// 这正是它与压缩的分工——压缩从数组头部断开，那一段只换中间
    #[test]
    fn a_span_summary_replaces_only_its_span_and_leaves_the_head_untouched() {
        let mut log = SessionLog::new();
        push(&mut log, user("第一问"));
        let from = push(&mut log, assistant("中间那一段的答复"));
        let through = push(&mut log, user("中间那一段的再问"));
        push(&mut log, assistant("最后一答"));
        let before = project(&log).expect("投影该成功").wire();

        let summary = push(&mut log, span(&from, &through, "两段讲完的事，一行写完"));
        let projection = project(&log).expect("投影该成功");
        assert_eq!(
            contents(&projection),
            vec![
                "第一问".to_string(),
                format!("{BRANCH_MARKER}两段讲完的事，一行写完"),
                "最后一答".to_string(),
            ],
            "摘要该站到 from 那一格上，前后都照原样"
        );
        let after = projection.wire();
        assert_eq!(before[0], after[0], "from 之前那一格必须逐字节相同");
        assert_eq!(before[3], after[2], "尾上那一格也是原样，只是往前挪了一格");
        assert_eq!(
            omitted_for(&projection, &through),
            Some(Omitted::SummariedAway)
        );
        assert_eq!(
            omitted_for(&projection, &from),
            Some(Omitted::SummariedAway)
        );
        // 省下的字节要能从省略账里数出来：少了的那一截 + 摘要自己占的 == 被顶掉的那两行。
        // 少记一行（比如把 from 那一格悄悄换掉却不落账），这条就会红
        let before_chars: usize = before.iter().map(crate::session::layers::chars_of).sum();
        let after_chars: usize = after.iter().map(crate::session::layers::chars_of).sum();
        let summary_chars: usize = projection
            .entries
            .iter()
            .find(|(id, _)| *id == summary)
            .expect("摘要那一行要在投影里")
            .1
            .iter()
            .map(Message::wire_chars)
            .sum();
        let replaced: usize = projection.omissions.iter().map(|item| item.chars).sum();
        assert_eq!(
            before_chars - after_chars + summary_chars,
            replaced,
            "省略账与实发的差额对不上，那就是少记了一行"
        );
    }

    /// T07 判据之二：撤销一次压缩 = 追加一行撤回，再投影与该次压缩之前逐字节相同
    #[test]
    fn withdrawing_a_span_projects_the_exact_bytes_from_before_it() {
        let mut log = SessionLog::new();
        push(&mut log, user("第一问"));
        let from = push(&mut log, assistant("中间答复"));
        let through = push(&mut log, user("中间一问"));
        push(&mut log, assistant("最后一答"));
        let before = project(&log).expect("投影该成功").wire();

        let summary = push(&mut log, span(&from, &through, "一行摘要"));
        assert_ne!(project(&log).expect("压过之后该短一截").wire(), before);

        push(
            &mut log,
            EntryPayload::ContextEdit {
                target_id: summary.clone(),
                replacement: None,
            },
        );
        let projection = project(&log).expect("撤销之后再投影该成功");
        assert_eq!(
            projection.wire(),
            before,
            "撤销一次压缩该回到逐字节相同的那一版"
        );
        assert_eq!(log.len(), 6, "撤回只追加一行，被顶替的那些条目一条都没删");
        assert_eq!(omitted_for(&projection, &summary), Some(Omitted::Withdrawn));
        assert!(
            !projection
                .omissions
                .iter()
                .any(|item| item.omitted == Omitted::SummariedAway),
            "撤销之后不该还有条目被那段摘要顶着"
        );
    }

    /// 压缩边界也一样能撤。这一条同时钉住一个真实缺陷：撤回的压缩以前仍被当成生效边界，
    /// 于是摘要行自己零贡献、被它藏起来的历史也回不来——那一段就这么凭空消失
    #[test]
    fn withdrawing_a_compaction_bring_back_the_prefix_it_hid() {
        let mut log = SessionLog::new();
        push(&mut log, user("第一问"));
        push(&mut log, assistant("第一答"));
        let kept = push(&mut log, user("第二问"));
        push(&mut log, assistant("第二答"));
        let before = project(&log).expect("投影该成功").wire();

        let boundary = push(
            &mut log,
            EntryPayload::Compaction {
                summary: "第一轮的来回".into(),
                first_kept_entry_id: kept,
                tokens_before: 1_000,
                usage: None,
                system_message: None,
            },
        );
        let compacted = project(&log).expect("投影该成功");
        assert_eq!(compacted.wire().len(), 3, "摘要一行加保留窗两行");

        push(
            &mut log,
            EntryPayload::ContextEdit {
                target_id: boundary,
                replacement: None,
            },
        );
        assert_eq!(
            project(&log).expect("撤销之后再投影该成功").wire(),
            before,
            "撤掉压缩边界，被它顶掉的那一段该原样回来"
        );
    }

    /// 两个 id 解不出范围的那一份摘要（分叉从别处带过来的那种）就只是一行摘要：
    /// 它顶替不了任何东西，也不该把别人的条目藏起来
    #[test]
    fn a_summary_that_resolves_to_no_span_is_just_a_row() {
        let mut log = SessionLog::new();
        push(&mut log, user("第一问"));
        push(&mut log, assistant("第一答"));
        push(
            &mut log,
            EntryPayload::BranchSummary {
                from_id: Some("不在这条路径上".into()),
                through_id: Some("也不在".into()),
                summary: "别处那一段的摘要".into(),
                usage: None,
            },
        );
        let projection = project(&log).expect("投影该成功");
        assert_eq!(
            contents(&projection),
            vec![
                "第一问".to_string(),
                "第一答".to_string(),
                format!("{BRANCH_MARKER}别处那一段的摘要"),
            ]
        );
        assert!(
            projection.omissions.is_empty(),
            "解不出范围就不该有东西被顶替"
        );
    }

    /// 生效的改写要报得出是谁、哪一种、顶替了几行多少字节：撤销按钮只有对着它才按得下去，
    /// 而被顶替掉的那些条目自己既看不见也撤不掉自己
    #[test]
    fn the_projection_names_the_rewrite_in_effect() {
        let mut log = SessionLog::new();
        assert!(
            project(&log).expect("投影该成功").rewrite.is_none(),
            "没压过就没有改写"
        );
        push(&mut log, user("一"));
        let from = push(&mut log, assistant("二"));
        let through = push(&mut log, user("三"));
        push(&mut log, assistant("四"));
        let summary = push(&mut log, span(&from, &through, "中间那段"));

        let rewrite = project(&log)
            .expect("投影该成功")
            .rewrite
            .expect("压过一段就该有生效的改写");
        assert_eq!(rewrite.entry_id, summary, "要撤销就得点得出是哪一条边界");
        assert_eq!(
            rewrite.kind,
            RewriteKind::Span,
            "只换中间那一段，不是从头断开"
        );
        assert_eq!(rewrite.replaced_rows, 2);
        assert!(
            rewrite.replaced_chars > 0,
            "顶替掉的字节量就是界面上那句省下多少的出处"
        );
    }

    /// 只有一条边界能生效：更旧的那些零贡献，改写不会套娃
    #[test]
    fn the_newer_boundary_wins_and_the_older_ones_contribute_nothing() {
        let mut log = SessionLog::new();
        push(&mut log, user("一"));
        let from = push(&mut log, assistant("二"));
        let through = push(&mut log, user("三"));
        let kept = push(&mut log, assistant("四"));
        let span_id = push(&mut log, span(&from, &through, "中间那段"));
        assert_eq!(project(&log).expect("投影该成功").wire().len(), 3);

        push(
            &mut log,
            EntryPayload::Compaction {
                summary: "整段的摘要".into(),
                first_kept_entry_id: kept,
                tokens_before: 800,
                usage: None,
                system_message: None,
            },
        );
        let projection = project(&log).expect("投影该成功");
        assert_eq!(
            contents(&projection),
            vec![format!("{SUMMARY_MARKER}\n整段的摘要"), "四".to_string()],
            "新的压缩站到头部，前面那段按层压缩的摘要从此零贡献"
        );
        assert_eq!(
            omitted_for(&projection, &span_id),
            Some(Omitted::SupersededCompaction)
        );
    }
}
