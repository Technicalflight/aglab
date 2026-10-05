//! Inspector：从日志现算的一份派生视图（§1.1.3）。
//!
//! 它不新增任何存储：这里没有一个 setter、不落盘、也没有能塞进第二真相的参数——报告里每个
//! 数字都是 [`sent_array`] 与 [`Projection`] 的一次读数，字段全是 `Serialize` 而没有
//! `Deserialize`，所以"界面上看着不对、改改报告字段"这条路在这个类型上写不出来。
//! 能被写回的面板就不是 Inspector。
//!
//! 报告里也不复制正文（§8.4）：出行数、字符、占比、生效条目的 id 与一个 `sent_digest` 哈希。
//! 要看得逐条点，走已有的敏感标记渲染，这里不做"整轮导出"。

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::context::{latest_custom, project, starts_fresh_chain, Omitted, Projection};
use super::entry::{Entry, EntryPayload, Message};
use super::layers::{
    self, BudgetInput, Concession, Layer, LayerBudget, LayerUse, Plan, DECLARATIONS_TYPE,
};
use super::log::SessionLog;
use super::prefix::{sent_array, Cause};
use super::sections;
use super::SessionError;

/// 一层在这一轮的账。实测那份 + 本轮分到多少，合成一行才答得出"谁占了位子"
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LayerRow {
    #[serde(flatten)]
    pub used: LayerUse,
    /// 占整轮上下文的比例（0~100）。分母是 `estimate.chars`，所以各层加起来是 100。
    /// 载荷里保留原始 f64 不取舍入——加起来才是精确的 100；直排上屏的取舍入是视图的事
    /// （不取的话"15.0216450…"会在定宽列里溢出去，压住旁边"几行几条"那格字）
    pub share_pct: f64,
    pub target: usize,
    pub max: usize,
    /// 这轮由这层付账的那步让步真的在阶梯上。不让步的层永远是 `false`——
    /// 它挤不挤得下由 `deficit` 与 `reason` 说，不冒充"被裁"
    pub conceded: bool,
}

/// 一个命名段现在是什么状态。段是差分行，所以"历史里几行"与"生效哪一行"要分开报
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SectionRow {
    pub name: String,
    pub layer: Layer,
    /// 生效那行的条目 id。界面上"这条约定从哪来"要点开的就是它
    pub effective_entry_id: String,
    /// 历史里这一段一共留了几行（差分行与撤销行都算：条目只追加，同时留着两份）
    pub written_rows: usize,
    pub chars: usize,
    /// 生效那行是撤销行：模型看见的是"此前那段的正文不再适用"
    pub revoked: bool,
    /// 生效那行本轮到底发不发。压缩边界之前写、又没被快照带走的段行就不发了
    pub projected: bool,
}

/// 被投影丢掉的东西：哪条条目、本来要占多少字节、为什么
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DroppedRow {
    pub entry_id: String,
    pub layer: Option<Layer>,
    /// 它**本来**要占的量，跟实发同一个口径，所以"这次省了多少"是笔能对上的账
    pub chars: usize,
    pub why: Omitted,
}

/// 一份 Inspector 报告。它是派生视图，所以没有"更新它"这条路
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectorReport {
    pub conversation_id: String,
    /// 本次投影的叶子，不是"当前 tip"（§8.5）：它等于实发数组真正用到的最后一条条目
    pub leaf_entry_id: Option<String>,
    /// 实发数组的哈希，不是正文：能对齐，但不会在这里被二次暴露（§8.4）
    pub sent_digest: String,
    /// 实发的 messages 数组（常驻段 ++ 日志投影）有几行、多少字节。实测
    pub sent_rows: usize,
    pub sent_chars: usize,
    /// 声明数组那一格：它不在 messages 里，所以单独报
    pub declared_rows: usize,
    pub declared_chars: usize,
    pub layers: Vec<LayerRow>,
    pub budget: Vec<LayerBudget>,
    pub sections: Vec<SectionRow>,
    pub dropped: Vec<DroppedRow>,
    /// 这一轮生效的那次改写：撤销只能对着它做，被它顶替的那些条目自己撤不掉自己
    pub rewrite: Option<crate::session::context::Rewrite>,
    /// 幂等读缓存的读数与自动重试次数。它不在日志里（日志是历史，这是进程内的账），
    /// 所以这一格和 `parent_session_id` 一样是**调用方填的**（`context_inspect`）
    pub cache: Option<crate::tool_runtime::cache::Stats>,
    pub limit: usize,
    /// 不让步的那几层装不进窗口的部分。它大于 0 就是"只能 Notice"的意思（§2.3）
    pub deficit: usize,
    /// 服务商上一次真报的 `prompt_tokens`。它是 P2 校准的原料，不是本轮的估算——
    /// 本轮那个数只有 `estimate`，而它明写自己是字符口径（§4.5）
    pub endpoint_prompt_tokens: Option<u32>,
    /// `plan.ladder` 里**没有自动执行者**的那几步（派生自 `Concession::auto_actor`）。
    /// 它存在的意义是别让那句"让步：… → 裁技能"替一个不会发生的动作说话
    pub unattended: Vec<Concession>,
    /// 本机实测的字符↔token 系数与偏差上界。样本在用量台账里而 `inspect` 只吃日志，
    /// 所以这一格和 `cache` 一样是**调用方填的**。样本不够就是 `None`：
    /// 那时该继续承认口径是估算，而不是端出一个看着精确的假系数
    pub calibration: Option<crate::usage::Calibration>,
    /// 这一支从哪来。只有分叉出来的话题有值；它由话题 header 提供，而 `inspect` 只吃
    /// 日志与常驻段，所以这一格是**调用方填的**（`context_inspect`），默认 `None`
    pub parent_session_id: Option<String>,
    #[serde(flatten)]
    pub plan: Plan,
}

/// 现算一份报告。输入只有四样：日志、常驻段、窗口与输出预留、服务商上回报过的量。
/// 这四样里没有"界面上那份历史"的位置——它想要也得从日志走
pub fn inspect(
    conversation_id: &str,
    log: &SessionLog,
    head: &[Value],
    input: BudgetInput,
    endpoint_prompt_tokens: Option<u32>,
) -> Result<InspectorReport, SessionError> {
    // 发出去的那批字节只有一个来源（§2.3），报告跟它核对而不是另算一遍
    let history = sent_array(log)?;
    let projection = project(log)?;
    let used = layers::uses(log, head)?;
    let table = layers::budget(&used, input);
    let measured = layers::estimate(&used);
    // 断开这件事也读得出来：本轮是压缩后的第一笔，那数组必然跟上一批不一样
    let reset = starts_fresh_chain(log)?.then_some(Cause::Compacted);
    let plan = layers::plan(measured, &table, reset);

    let declared = latest_custom(log, DECLARATIONS_TYPE)?;
    let path = log.path()?;
    Ok(InspectorReport {
        conversation_id: conversation_id.to_string(),
        parent_session_id: None,
        leaf_entry_id: leaf_used_by(&projection),
        sent_digest: sent_digest(head, &history),
        sent_rows: head.len() + history.len(),
        sent_chars: layers::wire_chars(head) + layers::wire_chars(&history),
        declared_rows: declared.and_then(Value::as_array).map_or(0, Vec::len),
        declared_chars: declared.map(layers::chars_of).unwrap_or(0),
        layers: table
            .rows
            .iter()
            .map(|budget| LayerRow {
                used: use_of(&used, budget.layer),
                share_pct: if measured.chars == 0 {
                    0.0
                } else {
                    budget.chars as f64 * 100.0 / measured.chars as f64
                },
                target: budget.target,
                max: budget.max,
                conceded: concedes(&plan.ladder, budget.layer),
            })
            .collect(),
        budget: table.rows.clone(),
        sections: section_rows(&path, &projection),
        dropped: projection
            .omissions
            .iter()
            .map(|omission| DroppedRow {
                entry_id: omission.entry_id.clone(),
                layer: log
                    .entry(&omission.entry_id)
                    .and_then(|entry| layers::classify(entry.payload())),
                chars: omission.chars,
                why: omission.omitted,
            })
            .collect(),
        // 生效的改写从投影里拿：它已经答过"这一轮是谁在顶替"，这里不许再判一遍
        rewrite: projection.rewrite.clone(),
        cache: None,
        calibration: None,
        limit: table.limit,
        deficit: table.deficit,
        endpoint_prompt_tokens,
        // 阶梯上没人自动执行的那几步。面板要说"本轮要让步"，就得同时说清哪一步其实不会发生；
        // 判据只有 `Concession::auto_actor` 那一份，这里把它抄出来，不在界面侧再认一次字符串
        unattended: plan
            .ladder
            .iter()
            .copied()
            .filter(|step| *step != Concession::None && step.auto_actor().is_none())
            .collect(),
        plan,
    })
}

/// 发出去的那批字节的指纹。长度前缀是为了让 `["ab","c"]` 与 `["a","bc"]` 不撞成同一份
/// ——跟 `policy::fingerprint` 同一条规矩：能拿来对齐的东西必须自己无歧义
pub fn sent_digest(head: &[Value], history: &[Value]) -> String {
    let mut hasher = Sha256::new();
    for row in head.iter().chain(history.iter()) {
        let text = row.to_string();
        hasher.update(text.len().to_le_bytes());
        hasher.update(text.as_bytes());
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 实发数组真正用到的最后一条条目。取的是**最后一个还有行的**：末尾挂着用量、标题这类
/// 账目行是常态，把它们当叶子就是 tip 冒充 leaf（§8.5）
fn leaf_used_by(projection: &Projection) -> Option<String> {
    projection
        .entries
        .iter()
        .rev()
        .find(|(_, messages)| !messages.is_empty())
        .map(|(id, _)| id.clone())
}

/// 生效段那一行的账。"哪一条生效"由 `sections::last_written` 说，这里只把它读成报告
fn section_rows(path: &[&Entry], projection: &Projection) -> Vec<SectionRow> {
    let mut rows = Vec::new();
    for (name, entry) in sections::last_written(path) {
        let content = match entry.payload() {
            EntryPayload::CustomMessage { content, .. } => content.as_str(),
            _ => unreachable!("last_written 只挑段条目"),
        };
        rows.push(SectionRow {
            name: name.to_string(),
            layer: layers::classify(entry.payload()).expect("段条目总归有一层"),
            effective_entry_id: entry.id.clone(),
            written_rows: path
                .iter()
                .filter(|row| sections::section_of(row.payload()) == Some(name))
                .count(),
            // 段行发出去就是那条 system，所以量按它的 wire 形算
            chars: Message::System {
                content: content.to_string(),
            }
            .wire_chars(),
            revoked: sections::revoked(content),
            projected: projected_rows(projection, &entry.id) > 0,
        });
    }
    rows
}

/// 这条条目在投影里真的发出了几行。零贡献的条目也在 `entries` 里（那是投影的逐条对应），
/// 所以"投影里有它"不等于"发了它"
fn projected_rows(projection: &Projection, id: &str) -> usize {
    projection
        .entries
        .iter()
        .find(|(entry_id, _)| entry_id == id)
        .map_or(0, |(_, messages)| messages.len())
}

/// 这层是不是被阶梯点名付账。`conceded` 只从这里出，别在 UI 上按 `chars > max` 再判一遍
fn concedes(ladder: &[Concession], layer: Layer) -> bool {
    ladder.iter().any(|step| step.layer() == Some(layer))
}

fn use_of(used: &[LayerUse], layer: Layer) -> LayerUse {
    used.iter()
        .find(|row| row.layer == layer)
        .copied()
        .unwrap_or(LayerUse {
            layer,
            entries: 0,
            rows: 0,
            chars: 0,
        })
}

#[cfg(test)]
mod tests {
    use super::super::entry::{NewEntry, PendingAssistant, StopReason};
    use super::super::layers::BreakReason;
    use super::super::log::SessionLog;
    use super::super::prefix::{divergence, PrefixLedger};
    use super::super::sections::Section;
    use super::super::sections::{MEMORY, SKILLS};
    use super::super::store::{path_for, save, SessionHeader};
    use super::*;
    use serde_json::json;

    const T0: i64 = 1_700_000_000_000;
    const MARK_MEMORY: &str = "【本地记忆】本条是这台机器上存着的长期记忆。";
    const MARK_SKILLS: &str = "【技能清单】本条是当前可用的技能清单。";

    fn head() -> Vec<Value> {
        vec![json!({ "role": "system", "content": "你是 aglab" })]
    }

    fn roomy() -> BudgetInput {
        BudgetInput::uncalibrated(100_000, 4_096)
    }

    fn push(log: &mut SessionLog, payload: EntryPayload) -> String {
        log.append(NewEntry::new(payload), T0)
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

    fn settled(text: &str) -> Message {
        Message::Assistant(
            PendingAssistant {
                content: text.into(),
                tool_calls: vec![],
            }
            .settle(StopReason::Stop),
        )
    }

    fn assistant(text: &str) -> EntryPayload {
        EntryPayload::Message {
            message: settled(text),
        }
    }

    /// 段行照写侧那条唯一的路径拼（`Section::row()`），报告认的就是实发那个串
    fn section(name: &'static str, marker: &'static str, body: &str) -> EntryPayload {
        let row = Section {
            name,
            marker,
            body: body.to_string(),
        }
        .row();
        EntryPayload::CustomMessage {
            custom_type: format!("{}{name}", sections::ENTRY_PREFIX),
            content: row,
            display: false,
        }
    }

    fn report(log: &SessionLog) -> InspectorReport {
        inspect("conv-1", log, &head(), roomy(), None).expect("现算该成功")
    }

    /// Inspector 的每个读数都得有人读得到它叫什么。这份 TS 类型是**照着序列化实测的键表**
    /// 写的（不是照 struct 猜的：`Plan` 与 `LayerUse` 都是 `#[serde(flatten)]` 进来的），
    /// 所以这条测试红的时候先改前端，别删后端字段来迁就界面。
    /// 可选项一律填上值——`assert_matches_ts` 只看得见序列化出来的键，None 的字段在它眼里不存在
    #[test]
    fn the_report_matches_the_frontend_types() {
        let mut seen = report(&sample());
        seen.leaf_entry_id = Some("entry-1".into());
        seen.endpoint_prompt_tokens = Some(1_234);
        seen.parent_session_id = Some("conv-parent".into());
        seen.rewrite = Some(crate::session::context::Rewrite {
            entry_id: "entry-9".into(),
            kind: crate::session::context::RewriteKind::Span,
            replaced_rows: 2,
            replaced_chars: 4_000,
        });
        seen.cache = Some(crate::tool_runtime::cache::Stats {
            hits: 3,
            misses: 5,
            entries: 4,
            retries: 1,
        });
        let value = serde_json::to_value(&seen).expect("报告总能编码");
        crate::test_support::assert_matches_ts(&value, "ContextInspector");
        crate::test_support::assert_matches_ts(
            &serde_json::to_value(&seen.rewrite).expect("改写总能编码"),
            "ContextRewrite",
        );
        crate::test_support::assert_matches_ts(
            &serde_json::to_value(&seen.layers[0]).unwrap(),
            "ContextLayerRow",
        );
        crate::test_support::assert_matches_ts(
            &serde_json::to_value(&seen.budget[0]).unwrap(),
            "ContextBudgetRow",
        );
        if let Some(section) = seen.sections.first() {
            crate::test_support::assert_matches_ts(
                &serde_json::to_value(section).unwrap(),
                "ContextSectionRow",
            );
        }
    }

    fn row<'a>(seen: &'a InspectorReport, layer: Layer) -> &'a LayerRow {        seen.layers
            .iter()
            .find(|item| item.used.layer == layer)
            .expect("八层永远成表")
    }

    /// 一份"首轮定形之后"的话题：声明条目在里面，它不进 messages 却照样占预算
    fn sample() -> SessionLog {
        let mut log = SessionLog::new();
        push(
            &mut log,
            EntryPayload::Custom {
                custom_type: DECLARATIONS_TYPE.into(),
                data: Some(json!([
                    {"function": {"name": "read_file"}},
                    {"function": {"name": "run_command"}},
                ])),
            },
        );
        push(&mut log, section(MEMORY, MARK_MEMORY, "用户住在杭州"));
        push(&mut log, section(SKILLS, MARK_SKILLS, "pdf / xlsx"));
        push(&mut log, user("第一问"));
        push(&mut log, assistant("答复一"));
        push(&mut log, user("第二问"));
        log
    }

    /// T05 的主判据：Inspector 报的总字符数就是 `sent_array` 的实测，一个字节都不许差
    #[test]
    fn the_reported_totals_are_measured_from_the_same_array_the_sender_uses() {
        let log = sample();
        let seen = report(&log);
        let history = sent_array(&log).expect("投影该成功");
        assert_eq!(
            seen.sent_chars,
            layers::wire_chars(&head()) + layers::wire_chars(&history),
            "Inspector 报的量跟实发的不是一批字节，那它就不是派生视图"
        );
        assert_eq!(seen.sent_rows, head().len() + history.len());
        assert_eq!(
            seen.sent_digest,
            sent_digest(&head(), &history),
            "指纹要能被实发数组独立复算出来，否则它对齐不了任何东西"
        );
        let layers_sum: usize = seen.layers.iter().map(|item| item.used.chars).sum();
        assert_eq!(layers_sum, seen.plan.estimate.chars);
        assert_eq!(seen.declared_rows, 2, "声明那一格也得数得出来");
        assert!(
            seen.declared_chars > 0 && seen.plan.estimate.chars > seen.sent_chars,
            "各层加起来只等于 messages 数组的话，声明那一格就没被算进账"
        );
    }

    /// 常驻段也是实发的一部分：换掉它就得换指纹。只从历史那半截算出来的指纹过不了这条
    #[test]
    fn a_different_standing_head_is_a_different_set_of_sent_bytes() {
        let log = sample();
        let seen = report(&log);
        let mut other = head();
        other[0]["content"] = json!("你是 aglab，跑在另一台机器上，说的是另一种话");
        let alt = inspect("conv-1", &log, &other, roomy(), None).expect("现算该成功");
        assert_ne!(
            alt.sent_digest, seen.sent_digest,
            "常驻段没进指纹，那它对齐不了任何实发的东西"
        );
        assert_eq!(
            alt.sent_chars - seen.sent_chars,
            layers::wire_chars(&other) - layers::wire_chars(&head()),
            "常驻段的字节没算进总量，压缩前的估算就会系统性偏小"
        );
    }

    /// 同一份日志两次现算必须给同一份报告：报告里不许掺进时钟、随机数或缓存的请求体
    #[test]
    fn the_same_log_always_produces_the_same_report() {
        let log = sample();
        let first = serde_json::to_value(report(&log)).expect("报告该能序列化");
        let second = serde_json::to_value(report(&log)).expect("报告该能序列化");
        assert_eq!(
            first, second,
            "同一份日志两次现算给出两份账，那就还是两份真相"
        );
        assert_eq!(
            first["estimate"]["kind"], "chars",
            "估算必须如实标自己是字符"
        );
        assert!(
            first.get("messages").is_none() && first.get("body").is_none(),
            "报告里不许有正文（§8.4）"
        );
        assert_eq!(
            first["layers"].as_array().map(Vec::len),
            Some(Layer::ORDER.len())
        );
    }

    /// T05 的变异对照之一：让一段进来生效，它自己那行与指纹必须跟着变
    #[test]
    fn a_section_entering_the_context_moves_its_own_row_and_the_digest() {
        let mut log = SessionLog::new();
        push(&mut log, section(MEMORY, MARK_MEMORY, "用户住在杭州"));
        push(&mut log, user("一问"));
        let before = report(&log);
        assert_eq!(row(&before, Layer::Memory).used.entries, 1);
        assert_eq!(row(&before, Layer::Skills).used.entries, 0);
        assert_eq!(before.sections.len(), 1);
        assert!(!before.sections[0].revoked);

        push(&mut log, section(SKILLS, MARK_SKILLS, "pdf / xlsx"));
        let after = report(&log);
        assert_eq!(row(&after, Layer::Skills).used.entries, 1);
        assert_eq!(after.sections.len(), 2);
        assert_ne!(
            after.sent_digest, before.sent_digest,
            "多了一段却没换指纹，说明指纹不是从实发数组算的"
        );
        assert_eq!(
            row(&after, Layer::Memory).used.chars,
            row(&before, Layer::Memory).used.chars,
            "别段的字节不该算到记忆段头上"
        );
    }

    /// T05 的变异对照之二：段整个没了要留下撤销行，报告要能分清"生效的是作废那句"
    #[test]
    fn a_vanished_section_is_reported_as_revoked_without_losing_its_history() {
        let mut log = SessionLog::new();
        push(&mut log, section(SKILLS, MARK_SKILLS, "pdf"));
        push(&mut log, user("一问"));
        let delta = {
            let path = log.path().expect("走路径该成功");
            sections::pending(&[], &sections::in_effect(&path))
        };
        assert_eq!(delta.len(), 1, "该只多出一行撤销");
        for payload in delta {
            push(&mut log, payload);
        }
        let seen = report(&log);
        let skills = seen
            .sections
            .iter()
            .find(|item| item.name == SKILLS)
            .expect("段名要查得到");
        assert!(skills.revoked, "生效的已经是撤销行");
        assert_eq!(skills.written_rows, 2, "撤销不改写：旧行必须还算在历史里");
        assert_eq!(row(&seen, Layer::Skills).used.entries, 2, "两行都还在发");
    }

    /// 压缩之后，被边界丢掉的行要出现在 `dropped` 里，而不是静悄悄少一批
    #[test]
    fn a_boundary_shows_what_it_left_out_instead_of_just_getting_shorter() {
        let mut log = SessionLog::new();
        let stale = push(
            &mut log,
            EntryPayload::Message {
                message: Message::System {
                    content: "旧约定".into(),
                },
            },
        );
        push(&mut log, user("窗口里的一问"));
        push(&mut log, user("压之前的一问"));
        push(
            &mut log,
            EntryPayload::Compaction {
                summary: "前面聊了 X".into(),
                first_kept_entry_id: stale,
                tokens_before: 10,
                usage: None,
                system_message: None,
            },
        );
        let seen = report(&log);
        assert_eq!(
            seen.dropped.len(),
            1,
            "被丢掉的东西必须数得出来：{:?}",
            seen.dropped
        );
        let lost = &seen.dropped[0];
        assert_eq!(lost.why, Omitted::StaleSystemRow);
        assert_eq!(
            lost.layer,
            Some(Layer::Rules),
            "丢掉那行属于哪层也要说得出来"
        );
        assert!(lost.chars > 0);
        assert_eq!(
            row(&seen, Layer::Rules).used.rows,
            0,
            "它没发出去，就不该同时出现在实发的那几层里"
        );
        assert_eq!(
            seen.plan.reason,
            BreakReason::Authorized {
                cause: Cause::Compacted
            },
            "刚压过又装得下：数组不一样是授权过的，不是预算逼的"
        );
    }

    /// §8.5：叶子是投影用到的最后一条，不是文件末行反推的 tip
    #[test]
    fn the_leaf_reported_is_the_last_row_the_array_actually_used_not_the_tip() {
        let mut log = sample();
        let asked = {
            let path = log.path().expect("走路径该成功");
            path[path.len() - 1].id.clone()
        };
        let tip = push(
            &mut log,
            EntryPayload::SessionInfo {
                name: Some("标题".into()),
            },
        );
        let seen = report(&log);
        assert_eq!(log.leaf_id(), Some(tip.as_str()), "tip 是那条例目行");
        assert_eq!(
            seen.leaf_entry_id.as_deref(),
            Some(asked.as_str()),
            "账目行不是实发的一部分，把它当叶子就是 tip 冒充 leaf"
        );

        // 回溯之后末端换了一条分支，叶子跟着投影走
        let back = {
            let path = log.path().expect("走路径该成功");
            path[3].id.clone()
        };
        log.navigate(Some(&back)).expect("回溯该成功");
        let rewound = report(&log);
        assert_eq!(rewound.leaf_entry_id.as_deref(), Some(back.as_str()));
        assert_ne!(rewound.sent_digest, seen.sent_digest);
    }

    /// Inspector 只读：现算一份报告不许动话题文件（§1.1.3 不落盘）
    #[test]
    fn inspecting_writes_nothing_to_the_session_file() {
        let dir = crate::test_support::scoped_temp_dir("inspector-readonly");
        let log = sample();
        let path = path_for(&dir, "C:/work/demo", T0, "conv-1").expect("路径该合法");
        save(
            &path,
            &SessionHeader::new("conv-1".into(), T0, "C:/work/demo".into()),
            &log,
        )
        .expect("存该成功");
        let stored = std::fs::read_to_string(&path).expect("刚写过该读得到");

        let seen = report(&log);
        assert_eq!(
            std::fs::read_to_string(&path).expect("读该成功"),
            stored,
            "现算一份报告就把话题改了，那 Inspector 就是第二个写入口"
        );
        assert!(seen.leaf_entry_id.is_some());
    }

    /// 报告只能由日志解释：追加一行，报告的增量必须正好等于那一行的字节
    #[test]
    fn the_report_follows_the_log_one_row_at_a_time() {
        let mut log = sample();
        let before = report(&log);
        push(&mut log, assistant("答复二"));
        let after = report(&log);
        assert_eq!(
            after.sent_chars - before.sent_chars,
            settled("答复二").wire_chars(),
            "日志动了一行而报告没跟着动，那它就不是从日志算的"
        );
        assert_eq!(after.sent_rows, before.sent_rows + 1);
        assert_eq!(
            after.dropped, before.dropped,
            "没有边界移动也没有编辑，不该凭空多出被裁的行"
        );
    }

    /// 让步那一格只由阶梯认领：不让步的层永远不报"被裁"
    #[test]
    fn only_layers_named_by_the_ladder_are_ever_reported_as_conceding() {
        let mut log = SessionLog::new();
        push(&mut log, user("一问"));
        push(&mut log, assistant(&"答".repeat(40_000)));
        let seen = inspect(
            "conv-1",
            &log,
            &head(),
            BudgetInput {
                window: 8_000,
                output_reserve: 1_024,
        chars_per_token: 1.0,
            },
            Some(1_234),
        )
        .expect("现算该成功");
        assert!(matches!(seen.plan.reason, BreakReason::OverBudget { .. }));
        assert!(concedes(&seen.plan.ladder, Layer::History));
        assert!(row(&seen, Layer::History).conceded);
        assert!(
            !row(&seen, Layer::Identity).conceded,
            "常驻段不在阶梯上：宁可报 Notice，也不静默裁它（§2.3）"
        );
        assert!(!row(&seen, Layer::Turn).conceded, "本轮那一问也不在阶梯上");
        assert_eq!(seen.endpoint_prompt_tokens, Some(1_234));
        assert_eq!(
            seen.plan.estimate.kind,
            layers::EstimateKind::Chars,
            "服务商报过数不等于本轮那个数是实测：估算照样得标自己是估算"
        );
    }

    /// 主判据的最后一件：把 Inspector 插在每一笔实发之间，前缀仍然只由追加延长
    #[test]
    fn interleaving_the_inspector_never_breaks_the_prefix_invariant() {
        let mut log = SessionLog::new();
        let mut ledger = PrefixLedger::default();

        push(&mut log, user("第一问"));
        let first = sent_array(&log).expect("投影该成功");
        ledger
            .observe(&first, Cause::Fresh)
            .expect("第一笔没有可比对象");
        let _ = report(&log);

        push(&mut log, assistant("答复一"));
        push(&mut log, user("第二问"));
        let second = sent_array(&log).expect("投影该成功");
        ledger
            .observe(&second, Cause::Appended)
            .expect("读了 Inspector 之后这笔追加必须仍然合法");
        let _ = report(&log);

        push(&mut log, assistant("答复二"));
        let third = sent_array(&log).expect("投影该成功");
        ledger.observe(&third, Cause::Appended).expect("同上");
        assert_eq!(divergence(&second, &third), second.len());
        assert_eq!(
            ledger.resets(),
            0,
            "Inspector 一次都不该造成授权之外的断开——它没有那个权力"
        );
    }
}
