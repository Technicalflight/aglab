//! 一轮结束时的那次决策：纯函数，吃事实吐效果。

use crate::session::mode::{State, Status};

/// 这一轮实际产出了什么。四条失控护栏（design-goal-mode.md §4.2）吃的就是这一格：
/// 停下来的理由是"这几轮什么都没产出"或"这一轮根本没跑成"，不是"跑了第几轮"。
/// 由调用方从这一轮的日志事实里读出来喂进来——机器自己不读日志
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoundOutcome {
    /// 有正文，或有至少一次成功的工具调用。两个计数器由此清零
    Output,
    /// 既无正文也无工具调用：这一轮模型什么都没干
    Empty,
    /// 有工具调用，但一次都没成功。带上最后一次的失败原话——blocked 的 note 要说得出卡在哪
    ExecFailed { last_error: String },
    /// 这一轮根本没跑成（服务商/网络错误）。当场落 `blocked`
    TurnError { message: String },
    /// 额度/限流这一类：不是这一支做错了什么，出路是换档案或等额度。当场落 `usage_limited`
    UsageExhausted { message: String },
}

/// 工具执行失败的结果行都以这一句开头（`tool_result_pair` 的 Failed 分支）。
/// 它是**协议不是文案**：改它就是改护栏的眼睛，两边一起改
pub const TOOL_FAILURE_MARK: &str = "执行失败：";

/// 从这一轮落下的消息行里读出 [`RoundOutcome`]。只认助手行与工具行——
/// 段差分行、上报行、用户插话都不是"这一轮的产出"：
/// - 有一次成功的工具调用，或有正文 → [`RoundOutcome::Output`]
/// - 有工具调用但全失败 → [`RoundOutcome::ExecFailed`]（带最后一次的失败原话）
/// - 什么都没有 → [`RoundOutcome::Empty`]
///
/// `None` 由调用方给：这一轮根本不是目标的自动轮时，调用方不喂行进来
pub fn round_outcome(rows: &[crate::session::entry::Message]) -> Option<RoundOutcome> {
    use crate::session::entry::Message;

    let mut has_text = false;
    let mut has_tool = false;
    let mut has_successful_tool = false;
    let mut last_error = String::new();
    for row in rows {
        match row {
            Message::Assistant(settled) => {
                if !settled.content.trim().is_empty() {
                    has_text = true;
                }
            }
            Message::Tool { content, .. } => {
                has_tool = true;
                if let Some(error) = content.strip_prefix(TOOL_FAILURE_MARK) {
                    last_error = error.trim().to_string();
                } else {
                    has_successful_tool = true;
                }
            }
            Message::User { .. } | Message::System { .. } => {}
        }
    }
    if has_successful_tool {
        Some(RoundOutcome::Output)
    } else if has_tool {
        // 工具跑了、全失败：哪怕夹着正文也算连败——那正是"卡死在同一堵墙上"的形状
        Some(RoundOutcome::ExecFailed { last_error })
    } else if has_text {
        Some(RoundOutcome::Output)
    } else {
        Some(RoundOutcome::Empty)
    }
}

/// 空转与工具连败的两个计数器。**住在调用方的内存里**（按话题一格，跨重启不保留）；
/// 机器吃现值、吐新值（[`Effect::RememberGuard`]），自己不存状态——存了它就不纯了。
/// 形状借 Codex（`consecutive_empty_turns` / `consecutive_execution_failure_turns`
/// 两个独立计数器、各自到 3 触发），不借它的字段
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Guard {
    pub empty_turns: u32,
    pub exec_fail_turns: u32,
}

/// 连续到这一格就落 `blocked`。三不是拍脑袋：一两轮的空转常常是模型在想，
/// 三轮什么都没有就是卡死了
pub const GUARD_LIMIT: u32 = 3;

/// 计数器的算术。单独成函数是因为调用方（`chat.rs` 的护栏登记表）也要用它算新值——
/// 机器与登记表各算一遍会漂，共用这一份就漂不了
pub fn advance_guard(guard: Guard, outcome: Option<&RoundOutcome>) -> Guard {
    match outcome {
        // 不是目标的自动轮（人插话、人排的跟随轮）：不参与计数，也不清零
        None => guard,
        Some(RoundOutcome::Output) => Guard::default(),
        Some(RoundOutcome::Empty) => Guard {
            empty_turns: guard.empty_turns + 1,
            exec_fail_turns: 0,
        },
        Some(RoundOutcome::ExecFailed { .. }) => Guard {
            empty_turns: 0,
            exec_fail_turns: guard.exec_fail_turns + 1,
        },
        // 错误与额度两格当场就停，计数器随之清零
        Some(RoundOutcome::TurnError { .. } | RoundOutcome::UsageExhausted { .. }) => {
            Guard::default()
        }
    }
}

/// 这一轮结束时，花费那一格读到了什么。
///
/// 三种情况必须分开。**设了上限而账读不出来**不等于"没花钱"：花费上限是这支目标
/// 唯一的自动刹车（没有轮次上限），拿不到读数就当它没花，等于静默把闸松开
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spend {
    /// 没设上限，犯不着去开台账
    NotNeeded,
    /// 读出来了。`None` = 这一支还没有起算点（刚定下目标那一趟）
    Read(Option<i64>),
    /// 设了上限而台账读不出来
    Unreadable,
}

/// 决策要吃的全部事实。都由调用方读好了喂进来——这一格一旦需要 `AppHandle`
/// 才跑得动，就只剩实盘那一种测法了
pub struct RoundInput<'a> {
    /// **已经折进排队里那次切档之后**的模式状态。先后是承重的：判据读的就是这一行，
    /// 折晚了会拿旧档位去判下一轮，于是"切到规划档这一轮就该停了"却还是自己接了下去
    pub state: &'a State,
    /// 这一路允不允许自己接下一轮（界面发的与目标续跑轮都是 true）
    pub auto_continue: bool,
    /// 用户在回合中按了暂停（旗子已由调用方取走：它只对该收尾的这一轮生效）
    pub pause_requested: bool,
    pub spend: Spend,
    /// 跟随队列的头一条：人自己排的话。**它永远优先于自动续跑**，
    /// 因为那一格排的是"接下来人要做的事"
    pub queued: Option<String>,
    /// 这一轮是被按停止打断的
    pub interrupted: bool,
    /// 这一轮的产出。`None` = 这一轮不是目标的自动轮（人插话、人排的跟随轮），
    /// 护栏不参与、计数器不清零
    pub outcome: Option<RoundOutcome>,
    /// 计数器的现值（调用方从登记表里读来喂进来的）
    pub guard: Guard,
}

/// 调用方要去执行的事。**顺序有意义**：按这个列表的顺序做，做完就结束本轮
#[derive(Debug, PartialEq)]
pub enum Effect {
    /// 作废跟随队列：那些话是排给刚被打断的这一轮的后续，不是排给目标的
    ClearQueue,
    /// 落一行模式状态（暂停、预算到顶都走它）。落完不再自己接
    AppendModeRow(State),
    /// 说一句说明。用在"按了停止而目标还在往下推"那一种：不说这句就是让用户
    /// 以为目标停了，那比多说一句罗嗦坏得多
    EmitNotice(String),
    /// 跑目标的下一轮。`armed` 是轮数已经加过一格的那份状态——**加这一格只在这里发生**
    RunGoalRound { armed: State },
    /// 跑人排的那一轮。它不占目标的轮数
    RunQueuedTurn { text: String },
    /// 这一轮说完了，也没有接着要跑的东西
    FinishTurn,
    /// 把空转/连败的两个计数器记回调用方的登记表。只有目标的自动轮才发这一条——
    /// 人插话的轮不参与计数，也不许顺手把计数清掉
    RememberGuard(Guard),
    /// 停下并报错：设了上限而账读不出来时不许"当它没花"往下跑
    Abort { message: String },
}

/// 钱到顶那句要写进日志的话。界面那格会随话题切走而消失，而日志里这一行
/// 说得出为什么这一支不往下跑了
fn budget_note(spent_e8: i64, cap_e8: i64) -> String {
    format!(
        "这一支的花费到了上限（{} / {}）。",
        crate::session::mode::usd(spent_e8),
        crate::session::mode::usd(cap_e8)
    )
}

/// 护栏 note 里的错误原话可能很长（一整段编译器输出）。截到能认出是什么错就够——
/// 全文本来就在这一轮的日志里，note 只负责说得出"卡在哪"
fn short_note(text: &str) -> String {
    const LIMIT: usize = 200;
    if text.chars().count() <= LIMIT {
        return text.to_string();
    }
    let cut: String = text.chars().take(LIMIT).collect();
    format!("{cut}…（原文见日志）")
}

/// 一轮跑完，接下来该做什么。
///
/// 两条承重规矩在这儿落地：
/// 1. **判据只算一次**：这个函数就是发续跑读数的那一处（调用方按返回的
///    `RunGoalRound` 决定 `continuing`），两处各算一遍会出现"界面还以为要接着跑、
///    后端已经停了"那种卡在生成中的僵局。
/// 2. **读数先于 Done**：由调用方保证（`close_round` 那个出口），这里只决定 `continuing`。
pub fn decide_after_round(input: RoundInput<'_>) -> Vec<Effect> {
    let held = input.state;
    let mut out = Vec::new();

    if input.interrupted {
        out.push(Effect::ClearQueue);
    }
    // 人排的话永远优先：那一格排的是"接下来人要做的事"。
    // 打断那一趟除外——上面的 ClearQueue 已经把它作废了，这条不再取
    let queued = if input.interrupted {
        None
    } else {
        input.queued.clone()
    };

    if !held.goal_held() || !input.auto_continue {
        return match queued {
            Some(text) => push(out, Effect::RunQueuedTurn { text }),
            None => push(out, Effect::FinishTurn),
        };
    }

    // 暂停旗：那一行现在才落得下去（这一轮的副本就在身上，落了不会被盖掉），
    // 落完不再接下一轮。已经是 paused 就不重复落一行
    if input.pause_requested && !held.paused() {
        return push(
            out,
            Effect::AppendModeRow(State {
                status: Status::Paused,
                ..held.clone()
            }),
        );
    }

    if held.status != Status::Active {
        // 停着的四格（暂停 / 停住 / 预算花完 / 额度到顶）都不自己接
        return match queued {
            Some(text) => push(out, Effect::RunQueuedTurn { text }),
            None => push(out, Effect::FinishTurn),
        };
    }

    // 这一轮的产出说了什么（§4.2 的四条护栏）。放在预算闸之前：错误与空转是
    // "这一支病了"，钱到顶是"这一支花完了"——两条 note 说的是两件事，别混成一格。
    // 计数器只有目标自动轮才动：`outcome` 是 None（人插话的轮）就原样记回
    let advanced = advance_guard(input.guard, input.outcome.as_ref());
    let last_exec_error = match &input.outcome {
        Some(RoundOutcome::ExecFailed { last_error }) => Some(last_error.clone()),
        _ => None,
    };
    match input.outcome {
        Some(RoundOutcome::TurnError { message }) => {
            out.push(Effect::RememberGuard(Guard::default()));
            return push(
                out,
                Effect::AppendModeRow(State {
                    status: Status::Blocked,
                    note: Some(short_note(&format!("这一轮跑失败了：{message}"))),
                    ..held.clone()
                }),
            );
        }
        Some(RoundOutcome::UsageExhausted { message }) => {
            out.push(Effect::RememberGuard(Guard::default()));
            return push(
                out,
                Effect::AppendModeRow(State {
                    status: Status::UsageLimited,
                    note: Some(short_note(&format!(
                        "服务商或账号不给量了：{message}"
                    ))),
                    ..held.clone()
                }),
            );
        }
        _ => {}
    }
    if advanced.empty_turns >= GUARD_LIMIT {
        out.push(Effect::RememberGuard(Guard::default()));
        return push(
            out,
            Effect::AppendModeRow(State {
                status: Status::Blocked,
                note: Some(format!(
                    "连续 {GUARD_LIMIT} 轮没有任何产出（没有正文、没有工具调用），停在这里等人。"
                )),
                ..held.clone()
            }),
        );
    }
    if advanced.exec_fail_turns >= GUARD_LIMIT {
        let last = last_exec_error.unwrap_or_default();
        out.push(Effect::RememberGuard(Guard::default()));
        return push(
            out,
            Effect::AppendModeRow(State {
                status: Status::Blocked,
                note: Some(short_note(&format!(
                    "连续 {GUARD_LIMIT} 轮工具执行都没有成功：{last}"
                ))),
                ..held.clone()
            }),
        );
    }
    if input.outcome.is_some() {
        out.push(Effect::RememberGuard(advanced));
    }

    if held.max_cost_e8 > 0 {
        match input.spend {
            Spend::Unreadable => {
                return push(
                    out,
                    Effect::Abort {
                        message: "这一支设了花费上限，可台账读不出来。钱是唯一的自动刹车，\
                                  拿不到读数就当它没花等于静默松闸——所以停在这里。"
                            .into(),
                    },
                )
            }
            Spend::Read(Some(spent)) if spent >= held.max_cost_e8 => {
                let note = budget_note(spent, held.max_cost_e8);
                // 落成 `budget_limited` 而不是 `blocked`：两者的出路不同
                // （调上限 vs 改目标），日志里那一行要说得出是哪一种
                return push(
                    out,
                    Effect::AppendModeRow(State {
                        status: Status::BudgetLimited,
                        note: Some(note),
                        ..held.clone()
                    }),
                );
            }
            Spend::Read(_) | Spend::NotNeeded => {}
        }
    }

    // 还要往下接。人排的话在前头时先跑人的，目标那一轮的账不从这里走
    if let Some(text) = queued {
        return push(out, Effect::RunQueuedTurn { text });
    }
    let was_interrupted = input.interrupted;
    out.push(Effect::RunGoalRound {
        armed: held.armed(),
    });
    if was_interrupted {
        // 停止只掐这一轮，掐不到这一支（working-modes §12）。它接着往下推这件事
        // 必须说出来——屏上刚出现过"已按你的要求停止生成"
        out.push(Effect::EmitNotice(
            "已停止这一轮。这一支还挂着目标，它接着往下推——\
             要停下目标请用目标带上的暂停或结束。"
                .into(),
        ));
    }
    out
}

fn push(mut effects: Vec<Effect>, last: Effect) -> Vec<Effect> {
    effects.push(last);
    effects
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::mode::{Status, Working};

    fn goal(turns: u32) -> State {
        State {
            working: Working::Goal,
            objective: Some("把台账那三处对账补齐".into()),
            started_at: Some(1),
            turns_used: turns,
            max_cost_e8: 0,
            status: Status::Active,
            ..Default::default()
        }
    }

    fn capped(cap_e8: i64) -> State {
        State {
            max_cost_e8: cap_e8,
            ..goal(2)
        }
    }

    fn input<'a>(state: &'a State) -> RoundInput<'a> {
        RoundInput {
            state,
            auto_continue: true,
            pause_requested: false,
            spend: Spend::NotNeeded,
            queued: None,
            interrupted: false,
            outcome: None,
            guard: Guard::default(),
        }
    }

    /// 判据只算一次，而且算它的那一处就是发读数的那一处：`RunGoalRound` 是
    /// `continuing = true` 的唯一来源
    #[test]
    fn a_running_goal_arms_exactly_one_more_round() {
        let held = goal(2);
        let effects = decide_after_round(input(&held));
        assert_eq!(
            effects,
            vec![Effect::RunGoalRound {
                armed: held.armed()
            }],
            "接着跑就是这一条效果，多一条都算重复"
        );
        // 轮数加了一格，其余字段一个都不动（起算点漂一下，前面那几发的钱就等于没花过）
        match &effects[0] {
            Effect::RunGoalRound { armed } => {
                assert_eq!(armed.turns_used, 3);
                assert_eq!(armed.started_at, held.started_at);
                assert_eq!(armed.objective, held.objective);
            }
            other => panic!("该是续跑：{other:?}"),
        }
    }

    /// 目标**没有轮次上限**：跑到第 999 轮、钱没到顶，照样接着跑
    #[test]
    fn the_goal_runs_until_it_reports_or_runs_out_of_money() {
        let held = goal(999);
        assert!(decide_after_round(input(&held)).contains(&Effect::RunGoalRound {
            armed: held.armed()
        }));
    }

    #[test]
    fn a_goal_with_no_objective_finishes_the_turn() {
        let idle = State {
            objective: None,
            ..goal(0)
        };
        assert_eq!(decide_after_round(input(&idle)), vec![Effect::FinishTurn]);
    }

    /// 收尾了的目标不再自己接：`complete` 是唯一翻不回去的一格
    #[test]
    fn a_reported_goal_does_not_promise_more_rounds() {
        for status in [
            Status::Complete,
            Status::Blocked,
            Status::UsageLimited,
            Status::BudgetLimited,
        ] {
            let stopped = State {
                status,
                note: Some("留着".into()),
                ..goal(4)
            };
            let effects = decide_after_round(input(&stopped));
            assert_eq!(
                effects,
                vec![Effect::FinishTurn],
                "{status:?} 之后不该再跑，也不该落第二行：{effects:?}"
            );
        }
    }

    /// 暂停要落一行——那一行是"为什么这一支不往下跑了"的凭据。
    /// 反向也钉：已经是 paused 的不许再落一份
    #[test]
    fn a_pause_lands_one_row_and_stops() {
        let held = goal(3);
        let mut ask = input(&held);
        ask.pause_requested = true;
        let effects = decide_after_round(ask);
        assert_eq!(effects.len(), 1, "落完这一行就结束，不许又接一轮：{effects:?}");
        match &effects[0] {
            Effect::AppendModeRow(row) => assert_eq!(row.status, Status::Paused),
            other => panic!("该是落一行 paused：{other:?}"),
        }

        let already = State {
            status: Status::Paused,
            ..goal(3)
        };
        let mut again = input(&already);
        again.pause_requested = true;
        assert_eq!(
            decide_after_round(again),
            vec![Effect::FinishTurn],
            "重复落一行 paused 是第二份真相"
        );
    }

    /// 钱到顶落成 `budget_limited` 并带上那句"到顶了"，不是 `blocked`
    #[test]
    fn running_out_of_money_lands_budget_limited_not_blocked() {
        let held = capped(250_000_000);
        let mut spend = input(&held);
        spend.spend = Spend::Read(Some(250_000_000));
        let effects = decide_after_round(spend);
        assert_eq!(effects.len(), 1);
        match &effects[0] {
            Effect::AppendModeRow(row) => {
                assert_eq!(row.status, Status::BudgetLimited);
                assert!(
                    row.note.as_deref().unwrap_or("").contains("花费到了上限"),
                    "日志里那一行要说得出为什么停：{:?}",
                    row.note
                );
            }
            other => panic!("该是落成预算花完：{other:?}"),
        }
    }

    /// 设了上限而账读不出来 ⇒ 停下报错。这条是"静默松闸"的反面
    #[test]
    fn an_unreadable_ledger_aborts_instead_of_running_free() {
        let held = capped(250_000_000);
        let mut spend = input(&held);
        spend.spend = Spend::Unreadable;
        let effects = decide_after_round(spend);
        assert!(
            matches!(&effects[0], Effect::Abort { .. }),
            "拿不到读数就当它没花，等于松闸：{effects:?}"
        );
        // 没设上限时读不读台账与这一轮无关，不许顺手 abort
        let uncapped = goal(2);
        let mut free = input(&uncapped);
        free.spend = Spend::Unreadable;
        assert!(
            !decide_after_round(free)
                .iter()
                .any(|e| matches!(e, Effect::Abort { .. })),
            "没设上限就不该去开台账，更不该因此停下"
        );
    }

    /// 人排的话永远优先于自动续跑，而且**不占轮数那一格**
    #[test]
    fn a_queued_user_turn_wins_and_does_not_charge_the_goal() {
        let held = goal(5);
        let mut ask = input(&held);
        ask.queued = Some("先帮我看别的".into());
        let effects = decide_after_round(ask);
        assert_eq!(
            effects,
            vec![Effect::RunQueuedTurn {
                text: "先帮我看别的".into()
            }],
            "该跑人排的那一轮，而不是给目标加一格"
        );
    }

    /// 按停止只掐这一轮：队列作废，目标照常往下接，并说一句"它还在跑"
    #[test]
    fn stopping_one_round_does_not_stop_the_goal() {
        let held = goal(2);
        let mut ask = input(&held);
        ask.interrupted = true;
        ask.queued = Some("这句是排给刚被打断那一轮的".into());
        let effects = decide_after_round(ask);
        assert_eq!(
            effects,
            vec![
                Effect::ClearQueue,
                Effect::RunGoalRound {
                    armed: held.armed()
                },
                Effect::EmitNotice(
                    "已停止这一轮。这一支还挂着目标，它接着往下推——\
                     要停下目标请用目标带上的暂停或结束。"
                        .into()
                ),
            ],
            "作废队列、接着跑、说一声——三样缺一不可"
        );
    }

    /// 交互档不许决定这件事：切到对话档目标照跑，切到规划档（那一行已折成 paused）不照跑
    #[test]
    fn the_interaction_tier_does_not_decide_this() {
        let chatting = State {
            working: Working::Chat,
            ..goal(7)
        };
        assert!(
            decide_after_round(input(&chatting)).iter().any(
                |e| matches!(e, Effect::RunGoalRound { .. })
            ),
            "对话档下它就该接着自己往下跑"
        );
        let planned = State {
            working: Working::Plan,
            status: Status::Paused,
            ..goal(7)
        };
        assert_eq!(decide_after_round(input(&planned)), vec![Effect::FinishTurn]);
    }

    /// 这一路不许自己接下一轮时（比如 rewind 之后那发），即便挂着目标也只跑人的话
    #[test]
    fn a_path_that_may_not_self_continue_only_runs_user_turns() {
        let held = goal(1);
        let mut ask = input(&held);
        ask.auto_continue = false;
        ask.queued = Some("人排的".into());
        assert_eq!(
            decide_after_round(ask),
            vec![Effect::RunQueuedTurn {
                text: "人排的".into()
            }]
        );
        let mut bare = input(&held);
        bare.auto_continue = false;
        assert_eq!(decide_after_round(bare), vec![Effect::FinishTurn]);
    }

    /// 差一分就该还能跑；到顶那一格是 `>=` 不是 `>`；上限 0 是"不设"，多少钱都不停
    #[test]
    fn the_budget_gate_is_or_equal_and_zero_means_no_cap() {
        let held = capped(500_000_000);
        let mut just_short = input(&held);
        just_short.spend = Spend::Read(Some(499_999_999));
        assert!(
            decide_after_round(just_short).contains(&Effect::RunGoalRound {
                armed: held.armed()
            }),
            "差一分就把闸咬下去，是提前掐用户的任务"
        );

        let mut at_cap = input(&held);
        at_cap.spend = Spend::Read(Some(500_000_000));
        assert!(
            matches!(decide_after_round(at_cap)[0], Effect::AppendModeRow(_)),
            "到顶就该停：判据是 >="
        );

        let uncapped = goal(3);
        let mut free = input(&uncapped);
        free.spend = Spend::Read(Some(i64::MAX));
        assert!(
            decide_after_round(free).contains(&Effect::RunGoalRound {
                armed: uncapped.armed()
            }),
            "不设上限不该被读成一个很小的数"
        );
    }

    /// 判据不许替模型改写收尾，也不许为已经收尾的目标落第二行
    #[test]
    fn the_machine_never_rewrites_a_reported_outcome() {
        for status in [Status::Complete, Status::Blocked, Status::UsageLimited] {
            let settled = State {
                status,
                note: Some("模型说过的原话".into()),
                ..goal(4)
            };
            assert_eq!(
                decide_after_round(input(&settled)),
                vec![Effect::FinishTurn],
                "{status:?} 之后什么都不该落"
            );
            assert_eq!(settled.status, status, "原话那一格不许被改动");
        }
    }

    // ---- §4.2 的四条失控护栏 ----

    /// 连续 3 轮空响应落 `blocked`，note 说得出是空转。第三轮才停：一两轮的安静
    /// 常常是模型在想，三轮什么都没有就是卡死了
    #[test]
    fn three_empty_rounds_in_a_row_block_the_goal() {
        let held = goal(0);
        let mut guard = Guard::default();
        for round in 1..GUARD_LIMIT {
            let mut ask = input(&held);
            ask.outcome = Some(RoundOutcome::Empty);
            ask.guard = guard;
            let effects = decide_after_round(ask);
            guard = advance_guard(guard, Some(&RoundOutcome::Empty));
            assert!(
                effects.iter().any(|e| matches!(e, Effect::RunGoalRound { .. })),
                "第 {round} 轮空转还不该停"
            );
            assert!(
                !effects.iter().any(|e| matches!(e, Effect::AppendModeRow(_))),
                "没到闸不许落第二行：{effects:?}"
            );
        }
        let mut third = input(&held);
        third.outcome = Some(RoundOutcome::Empty);
        third.guard = guard;
        let effects = decide_after_round(third);
        assert_eq!(effects.len(), 2, "清计数器 + 落一行，两样：{effects:?}");
        match &effects[1] {
            Effect::AppendModeRow(row) => {
                assert_eq!(row.status, Status::Blocked);
                assert!(
                    row.note.as_deref().unwrap_or("").contains("3 轮"),
                    "note 要说得出是空转：{:?}",
                    row.note
                );
            }
            other => panic!("该是落一行 blocked：{other:?}"),
        }
    }

    /// 连续 3 轮工具执行失败落 `blocked`，note 带最后一次的失败原话
    #[test]
    fn three_failed_tool_rounds_block_with_the_last_error() {
        let held = goal(0);
        let mut guard = Guard::default();
        for _ in 0..GUARD_LIMIT - 1 {
            let outcome = RoundOutcome::ExecFailed { last_error: "编译失败".into() };
            let mut ask = input(&held);
            ask.outcome = Some(outcome.clone());
            ask.guard = guard;
            assert!(decide_after_round(ask)
                .iter()
                .any(|e| matches!(e, Effect::RunGoalRound { .. })));
            guard = advance_guard(guard, Some(&outcome));
        }
        let outcome = RoundOutcome::ExecFailed {
            last_error: "error[E0308]: 类型不匹配".into(),
        };
        let mut third = input(&held);
        third.outcome = Some(outcome.clone());
        third.guard = guard;
        let effects = decide_after_round(third);
        match &effects.last().expect("该有效果") {
            Effect::AppendModeRow(row) => {
                assert_eq!(row.status, Status::Blocked);
                let note = row.note.as_deref().unwrap_or("");
                assert!(note.contains("3 轮"), "要说得出是连败：{note}");
                assert!(
                    note.contains("E0308"),
                    "note 要带最后一次的失败原话：{note}"
                );
            }
            other => panic!("该是落一行 blocked：{other:?}"),
        }
    }

    /// 有产出就把两个计数器清零：空两轮、出一轮、再空两轮，永远够不着闸
    #[test]
    fn a_productive_round_resets_both_counters() {
        let mut guard = Guard {
            empty_turns: 2,
            exec_fail_turns: 2,
        };
        guard = advance_guard(guard, Some(&RoundOutcome::Output));
        assert_eq!(guard, Guard::default(), "有产出就是没卡死");
    }

    /// 一次产出打断不了"连败"的另一种：计数器是**连续**的，Empty 与 ExecFailed
    /// 互相把对方归零——两格卡法各自数各自的
    #[test]
    fn the_two_counters_are_independent_and_count_only_consecutive_rounds() {
        let mut guard = advance_guard(Guard::default(), Some(&RoundOutcome::Empty));
        guard = advance_guard(guard, Some(&RoundOutcome::ExecFailed { last_error: "x".into() }));
        assert_eq!(
            guard,
            Guard { empty_turns: 0, exec_fail_turns: 1 },
            "换成另一种卡法就从零数起"
        );
    }

    /// 人插话的轮（outcome = None）不参与计数，也不清零：目标卡没卡死只看它自己的轮
    #[test]
    fn a_user_round_neither_counts_nor_resets_the_counters() {
        let guard = Guard { empty_turns: 2, exec_fail_turns: 0 };
        assert_eq!(advance_guard(guard, None), guard);
        let held = goal(0);
        let mut ask = input(&held);
        ask.outcome = None;
        ask.guard = guard;
        let effects = decide_after_round(ask);
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::RememberGuard(_))),
            "人插话的轮不许动登记表：{effects:?}"
        );
        assert!(
            effects.iter().any(|e| matches!(e, Effect::RunGoalRound { .. })),
            "目标照常往下接"
        );
    }

    /// turn 出错当场落 `blocked`，note 带错误原话；额度那类落 `usage_limited`。
    /// 这两条不等三轮：报错后再接一轮就是死循环烧钱
    #[test]
    fn a_turn_error_blocks_and_exhaustion_lands_usage_limited() {
        let held = goal(2);
        let outcome_row = |outcome: RoundOutcome| {
            let mut ask = input(&held);
            ask.outcome = Some(outcome);
            let effects = decide_after_round(ask);
            match effects.last().expect("该有效果") {
                Effect::AppendModeRow(row) => row.clone(),
                other => panic!("该是落一行：{other:?}"),
            }
        };
        let row = outcome_row(RoundOutcome::TurnError {
            message: "连接被重置".into(),
        });
        assert_eq!(row.status, Status::Blocked);
        assert!(row.note.as_deref().unwrap_or("").contains("连接被重置"));

        let row = outcome_row(RoundOutcome::UsageExhausted {
            message: "HTTP 429".into(),
        });
        assert_eq!(row.status, Status::UsageLimited, "额度不是这一支的错");

        // 两条错误路都不许再接一轮：报错后再 kick 就是死循环烧钱
        for outcome in [
            RoundOutcome::TurnError { message: "连接被重置".into() },
            RoundOutcome::UsageExhausted { message: "HTTP 429".into() },
        ] {
            let mut ask = input(&held);
            ask.outcome = Some(outcome);
            assert!(
                decide_after_round(ask)
                    .iter()
                    .all(|e| !matches!(e, Effect::RunGoalRound { .. })),
                "错误与额度之后不许再接一轮"
            );
        }
    }

    /// 机器吐的 RememberGuard 与 advance_guard 是同一份算术：调用方照它记，
    /// 下一轮喂回来的就是它——这条针钉"两处各算一遍会漂"
    #[test]
    fn the_remembered_guard_is_exactly_what_the_machine_advanced() {
        let held = goal(0);
        let mut ask = input(&held);
        ask.outcome = Some(RoundOutcome::Empty);
        ask.guard = Guard { empty_turns: 1, exec_fail_turns: 0 };
        let effects = decide_after_round(ask);
        let remembered = effects.iter().find_map(|e| match e {
            Effect::RememberGuard(guard) => Some(*guard),
            _ => None,
        });
        assert_eq!(
            remembered,
            Some(advance_guard(
                Guard { empty_turns: 1, exec_fail_turns: 0 },
                Some(&RoundOutcome::Empty)
            )),
            "登记表记的就该是机器算的那一份"
        );
    }

    /// 护栏 note 的错误原话太长要截断——全文在日志里，note 只负责认得出是什么错
    #[test]
    fn a_very_long_error_note_is_cut_to_a_recognizable_length() {
        let long = "错".repeat(500);
        let note = short_note(&format!("这一轮跑失败了：{long}"));
        assert!(note.chars().count() < 300, "截到认得出就够：{}", note.chars().count());
        assert!(note.ends_with("（原文见日志）"));
        let short = short_note("短的");
        assert_eq!(short, "短的", "不长的原话一个字都不动");
    }

    // ---- round_outcome：这一轮产出了什么，从消息行里读 ----

    use crate::session::entry::{Message, SettledAssistant};

    fn assistant_row(content: &str) -> Message {
        Message::Assistant(SettledAssistant {
            content: content.into(),
            tool_calls: Vec::new(),
            stop: crate::session::entry::StopReason::Stop,
            reasoning: None,
            error: None,
            thinking_signature: None,
            reasoning_items_json: None,
        })
    }

    fn tool_row(content: &str) -> Message {
        Message::Tool { tool_call_id: "t1".into(), content: content.into() }
    }

    #[test]
    fn a_round_is_judged_by_what_it_produced() {
        // 什么都没有 → Empty
        assert_eq!(round_outcome(&[]), Some(RoundOutcome::Empty));
        assert_eq!(round_outcome(&[assistant_row("  ")]), Some(RoundOutcome::Empty));
        // 有正文 → Output
        assert_eq!(round_outcome(&[assistant_row("做完了第一步")]), Some(RoundOutcome::Output));
        // 有一次成功的工具 → Output（正文空不空都行）
        assert_eq!(
            round_outcome(&[assistant_row(""), tool_row("ok")]),
            Some(RoundOutcome::Output)
        );
        // 工具全失败 → ExecFailed，带最后一次的原话；哪怕夹着正文也算
        let rows = vec![
            assistant_row("我来跑测试"),
            tool_row(&format!("{TOOL_FAILURE_MARK}第一次：超时")),
            tool_row(&format!("{TOOL_FAILURE_MARK}第二次：编译失败")),
        ];
        assert_eq!(
            round_outcome(&rows),
            Some(RoundOutcome::ExecFailed { last_error: "第二次：编译失败".into() }),
        );
        // 用户与系统行不是产出：只有它们时等于什么都没干
        let noise = vec![
            Message::User {
                content: "插话".into(),
                images: Vec::new(),
                audios: Vec::new(),
                videos: Vec::new(),
            },
            Message::System { content: "段".into() },
        ];
        assert_eq!(round_outcome(&noise), Some(RoundOutcome::Empty));
    }
}
