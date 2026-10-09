//! 作业模式（对话/规划/目标）与目标回合的收发编排（优化路线 O1-3 从 chat.rs 拆出）。
//!
//! 两半职责：
//! - **读数面**：mode_view / contract_view / mode_state_from——面板与续跑判据读的那一份；
//! - **推进面**：Next/Step/goal_after_round/close_turn/report_goal——回合怎么收尾、
//!   目标怎么续、出错了往哪退。
//! 命令（session_mode_* / session_goal_*）也住这里：lib.rs 的 generate_handler
//! 指到 `chat::mode_goal::…`。本层不碰流式 wire（那是 [`super::wire`] 的事），
//! 但持有 Send——回合收尾要落行、要发事件。

use super::{
    apply_pending_mode, config, kick_goal_round, open_session, open_session_in, tools,
    worker_turn_active, ChatEvent, ContractView, CriterionView, EventSink, ModeHub, ModeOutcome,
    ModeView, PendingMode, Send, StopHub, TurnHost, RETRYABLE_STATUS_WORD,
};
use serde_json::Value;
use tauri::{AppHandle, Manager, State};

/// 读数由日志里那一行算出来，再补上台账的那笔钱与"方案交完没"。
/// 前端不参与算，也就没得猜
pub(in crate::chat) fn mode_view(
    app: &AppHandle,
    conversation_id: &str,
    log: &crate::session::SessionLog,
) -> ModeView {
    mode_view_in(
        &app.path().app_config_dir().unwrap_or_default(),
        &app.path().app_data_dir().unwrap_or_default(),
        conversation_id,
        log,
    )
}

/// worker 变体（M3 收官）：目录由调用方给（Main 从 app 派生，worker 从 CLI 传来）
pub(in crate::chat) fn mode_view_in(
    config_dir: &std::path::Path,
    data_dir: &std::path::Path,
    conversation_id: &str,
    log: &crate::session::SessionLog,
) -> ModeView {
    use crate::session::mode::{self, Working};
    let state = mode::in_effect(log);
    ModeView {
        mode: state.working.name(),
        objective: state.objective.clone(),
        turns_used: state.turns_used,
        max_cost_usd_e8: state.max_cost_e8,
        // 目标挂在话题上、与交互档是两件事：起算点在，账就按它聚合——
        // 不管当下是目标档在推还是对话档下照跑
        spent_usd_e8: spent_reading(state.started_at, |since| {
            crate::usage::session_cost_e8_in(config_dir, conversation_id, since)
        }),
        status: state.status.name(),
        note: state.note.clone(),
        profile: state.profile.clone(),
        goal_id: state.goal_id.clone(),
        contract: contract_view_in(config_dir, data_dir, conversation_id, &state, log),
        plan_ready: state.working == Working::Plan && mode::plan_delivered(log),
    }
}

/// 契约的读侧投影：每条判据带风险档（classify 是唯一出处）与闭合状态（证据聚合）。
/// 日志是唯一真相，这一份随时可算——它不是第二份状态
pub(in crate::chat) fn contract_view(
    app: &AppHandle,
    conversation_id: &str,
    state: &crate::session::mode::State,
    log: &crate::session::SessionLog,
) -> Option<ContractView> {
    contract_view_in(
        &app.path().app_config_dir().unwrap_or_default(),
        &app.path().app_data_dir().unwrap_or_default(),
        conversation_id,
        state,
        log,
    )
}

/// worker 变体（M3 收官）：目录由调用方给
pub(in crate::chat) fn contract_view_in(
    config_dir: &std::path::Path,
    data_dir: &std::path::Path,
    conversation_id: &str,
    state: &crate::session::mode::State,
    log: &crate::session::SessionLog,
) -> Option<ContractView> {
    let _ = config_dir;
    use crate::goal::contract::{self, CriterionKind, CriterionState as Closed, Verified};

    let contract = state.contract.as_ref()?;
    let goal_id = state.goal_id.clone().unwrap_or_default();
    let rows: Vec<contract::Evidence> = contract::evidence_in_effect(log)
        .into_iter()
        .filter(|row| row.goal_id == goal_id)
        .collect();
    let root = crate::worktree::root_for_in(data_dir, conversation_id);
    let criteria = contract
        .criteria
        .iter()
        .zip(contract::criterion_states(contract, &rows))
        .map(|(criterion, closed)| CriterionView {
            id: criterion.id.clone(),
            text: criterion.text.clone(),
            kind: match criterion.kind {
                CriterionKind::Check { .. } => "check",
                CriterionKind::Judgment => "judgment",
            },
            command: criterion.command().map(str::to_string),
            risk: match &criterion.kind {
                CriterionKind::Check { command } => tools::classify(
                    "run_command",
                    &serde_json::json!({ "command": command }),
                    root.as_deref(),
                )
                .as_str(),
                CriterionKind::Judgment => "safe",
            },
            evidence: match closed {
                Closed::Open => "open",
                Closed::Passed {
                    verified: Verified::Runtime,
                } => "runtime",
                Closed::Passed {
                    verified: Verified::Reported,
                } => "reported",
                Closed::Failed => "failed",
            },
        })
        .collect();
    Some(ContractView {
        criteria,
        constraints: contract.constraints.clone(),
    })
}

/// 花费读数。**台账读不出来就是"不知道"，不是 0**：判据那一头拿不到账会直接报错停下
/// （花费上限是唯一的自动刹车，不许静默松开），界面若跟着显示 `$0.00` 就是说"这一支
/// 还没花钱"——一侧停下、另一侧说没花，是同一件事的两种说法在打架。
/// 没有起算点才是真的 0：那一支还没开始烧，与"读不出账"是两种长相
pub(in crate::chat) fn spent_reading(
    started_at: Option<i64>,
    read: impl FnOnce(i64) -> Result<i64, String>,
) -> Option<i64> {
    match started_at {
        None => Some(0),
        Some(since) => read(since).ok(),
    }
}

/// 读这一支现在的作业模式。界面那三格读数都从这儿来，不在前端自己记一份
#[tauri::command]
pub(crate) fn session_mode_get(
    app: AppHandle,
    conversation_id: String,
) -> Result<ModeView, String> {
    let source = open_session(&app, &conversation_id)?;
    Ok(mode_view(&app, &conversation_id, &source.log))
}

/// 切作业模式（对话 / 规划）。写的是话题日志里的一行，不是 `config.json` 那一格——
/// 模式属于这一支，切话题不该解开另一支的红线，反过来也一样。
///
/// **目标不在这条命令上**（design-goal-mode.md §3.2 的命令拆分）：一格命令改一件事，
/// 切档与定目标是两件事。定目标走 [`session_goal_set`]。
///
/// 回合正在跑的时候**不拒**，而是把请求寄存进 [`ModeHub`]，由那一轮收尾时替它落行。
/// 拒是不成立的选项：那一轮的日志副本开在身上，这里另写一份会被它收尾时整个盖掉，
/// 可目标的续跑循环让 `is_running` 一轮接一轮恒为真——守着那声拒绝，等于把
/// **一个正在推进的目标永远锁在当前档位里**，而"切去聊两句、目标照跑"正是这一档要给的
#[tauri::command]
pub(crate) fn session_mode_set(
    app: AppHandle,
    conversation_id: String,
    stop_hub: State<'_, StopHub>,
    mode_hub: State<'_, ModeHub>,
    mode: String,
) -> Result<ModeOutcome, String> {
    use crate::session::entry::NewEntry;

    let mut session = open_session(&app, &conversation_id)?;
    let held = crate::session::mode::in_effect(&session.log);
    // 校验当场做完：认不出的模式名要在人按下那一刻就说出来。
    // 寄存那条路重算一次（落行时账可能已经被 goal_report 往前推过），
    // 两边走的是同一个 `mode_state_from`，所以"现在拒"与"收尾时拒"是同一句话
    let next = mode_state_from(&mode, &held)?;
    if stop_hub.is_running(&conversation_id) {
        if worker_turn_active(&conversation_id) {
            // 收尾落行的义务在跑回合的那一方——worker 还不会替主进程落切档行。
            // 拒是唯一诚实的选择：静默寄存等于让用户按下的"换个档"凭空蒸发
            return Err("这一轮跑在 Agent 子进程里，等它收尾再切档。".into());
        }
        mode_hub
            .inner()
            .set(&conversation_id, PendingMode::Switch { mode });
        return Ok(ModeOutcome {
            view: mode_view(&app, &conversation_id, &session.log),
            deferred: true,
        });
    }
    session
        .log_mut()
        .append(
            NewEntry::new(crate::session::mode::row(&next)),
            crate::session::now_millis(),
        )
        .map_err(|error| error.to_string())?;
    session.save()?;
    let source = open_session(&app, &conversation_id)?;
    Ok(ModeOutcome {
        view: mode_view(&app, &conversation_id, &source.log),
        deferred: false,
    })
}

/// 定目标：立一份完成契约并**立刻开第一轮**（与「继续」共用 `kick_goal_round`）。
///
/// 已有一支**在推进**的目标且这次写的不是同一句时，要 `force` 才落——
/// 替换是另起一支（新身份、新起算点），确认这件事归人。同一句目标是认领：
/// 账全留，只翻回推进。
///
/// 回合在跑时寄存进 [`ModeHub`]，轮次边界落行——校验在这里做完，寄存那条路
/// 重算时走的是同一个 `goal_state_from`，门口与落行说同一句话
#[tauri::command]
pub(crate) fn session_goal_set(
    app: AppHandle,
    conversation_id: String,
    stop_hub: State<'_, StopHub>,
    mode_hub: State<'_, ModeHub>,
    objective: String,
    criteria: Option<Value>,
    constraints: Option<Vec<String>>,
    max_cost_usd: Option<String>,
    profile: Option<String>,
    force: bool,
) -> Result<ModeOutcome, String> {
    use crate::session::entry::NewEntry;

    let contract = contract_from_json(criteria.as_ref(), constraints.as_deref())?;
    // 点名的档案要真的存在：目标按 id 引用它，指向一张不存在的卡片只会让
    // 每一轮都死在同一句报错上。这里挡在门口
    let config = config::load(&app);
    let profile = profile
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty());
    if let Some(id) = &profile {
        if !config.profiles.iter().any(|card| &card.id == id) {
            return Err(format!(
                "点名了服务商档案「{id}」，可配置里没有这一张。先到「设置 → 服务商档案」把它建好。"
            ));
        }
    }
    let mut session = open_session(&app, &conversation_id)?;
    let held = crate::session::mode::in_effect(&session.log);
    let next = goal_state_from(
        objective.clone(),
        contract,
        max_cost_usd.as_deref(),
        crate::session::now_millis(),
        &held,
        profile.clone(),
        force,
    )?;
    if stop_hub.is_running(&conversation_id) {
        mode_hub.inner().set(
            &conversation_id,
            PendingMode::GoalSet {
                objective,
                criteria,
                constraints,
                max_cost_usd,
                profile,
                force,
            },
        );
        return Ok(ModeOutcome {
            view: mode_view(&app, &conversation_id, &session.log),
            deferred: true,
        });
    }
    session
        .log_mut()
        .append(
            NewEntry::new(crate::session::mode::row(&next)),
            crate::session::now_millis(),
        )
        .map_err(|error| error.to_string())?;
    session.save()?;
    drop(session);
    // 定目标即开工：落下的是"一支要往下推的目标"，就当场接第一轮，与「继续」共用
    // `kick_goal_round`
    if next.working == crate::session::mode::Working::Goal && next.goal_active() {
        kick_goal_round(app.clone(), stop_hub.inner(), &conversation_id)?;
    }
    let source = open_session(&app, &conversation_id)?;
    Ok(ModeOutcome {
        view: mode_view(&app, &conversation_id, &source.log),
        deferred: false,
    })
}

/// 编辑目标：**同一支**换文字（design-goal-mode.md §3.2 的 `edit`）。`goal_id`、
/// `started_at`、`turns_used`、已花的钱全部保留——改目标不是重新开始，
/// 那是 [`session_goal_set`] 带 `force` 的事。
///
/// 判据文本一改，它的证据立刻作废——那不是这里做的事：证据行带着判据文本，
/// 聚合时对不上现行文本就是作废（§4.1），所以这里只管改文字。
///
/// 目标在推进时编辑也照落：寄存进 [`ModeHub`]，轮次边界落行
#[tauri::command]
pub(crate) fn session_goal_edit(
    app: AppHandle,
    conversation_id: String,
    stop_hub: State<'_, StopHub>,
    mode_hub: State<'_, ModeHub>,
    objective: Option<String>,
    criteria: Option<Value>,
    constraints: Option<Vec<String>>,
) -> Result<ModeOutcome, String> {
    use crate::session::entry::NewEntry;

    let mut session = open_session(&app, &conversation_id)?;
    let held = crate::session::mode::in_effect(&session.log);
    // 没有目标就没有可编辑的东西——编辑不是定目标
    let Some(held_objective) = held.objective.clone() else {
        return Err("这条话题身上没有目标，没有什么可编辑的。定目标请用「定一个目标」。".into());
    };
    // 旧式目标（从未有过契约）第一次被编辑就必须补齐判据——门对它退化过一次，
    // 不能退化一辈子（§3.1）
    if held.contract.is_none() && criteria.is_none() {
        return Err("这支目标还没有判据。编辑时把判据补上——至少一条「怎么才算真做到」。".into());
    }
    // 只改一半时，缺的那一半沿用旧契约
    let prev = held.contract.clone();
    let new_criteria = match &criteria {
        Some(_) => criteria.clone(),
        None => prev
            .as_ref()
            .map(|contract| serde_json::to_value(&contract.criteria).unwrap_or(Value::Null)),
    };
    let new_constraints = constraints
        .clone()
        .or_else(|| prev.as_ref().map(|contract| contract.constraints.clone()));
    let contract = contract_from_json(new_criteria.as_ref(), new_constraints.as_deref())?;
    let wanted = objective
        .clone()
        .map(|text| text.trim().to_string())
        .unwrap_or(held_objective);
    if wanted.is_empty() {
        return Err("目标不许改成空的。要整份清掉请用「结束」。".into());
    }
    let next = crate::session::mode::State {
        objective: Some(wanted),
        contract: contract.or_else(|| held.contract.clone()),
        ..held.clone()
    };
    if let Some(contract) = &next.contract {
        crate::goal::contract::validate(next.objective.as_deref().unwrap_or(""), contract)?;
    }
    if stop_hub.is_running(&conversation_id) {
        mode_hub.inner().set(
            &conversation_id,
            PendingMode::GoalEdit {
                objective,
                criteria,
                constraints,
            },
        );
        return Ok(ModeOutcome {
            view: mode_view(&app, &conversation_id, &session.log),
            deferred: true,
        });
    }
    session
        .log_mut()
        .append(
            NewEntry::new(crate::session::mode::row(&next)),
            crate::session::now_millis(),
        )
        .map_err(|error| error.to_string())?;
    session.save()?;
    let source = open_session(&app, &conversation_id)?;
    Ok(ModeOutcome {
        view: mode_view(&app, &conversation_id, &source.log),
        deferred: false,
    })
}

/// 铸一个目标身份。与 `conversation_fork` 铸话题 id 同一个办法：纳秒当名字，
/// 要的只是"不撞车"，不是有序
pub(in crate::chat) fn mint_goal_id() -> String {
    format!(
        "goal-{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

/// 把界面交来的那两格拼成该落进日志的一行。校验全在这儿，而且它是纯的——命令那一层
/// 只剩"读日志、写一行、存盘"，这格判据因此不需要一台活应用才测得动。
///
/// 目标挂在**话题**上、与交互档是两件事：切到对话/规划时目标字段**原样保留**——
/// 对话档下它照常在后台推进，规划档只读推不动、自动转暂停（不销毁）。
/// 切回目标档时若仍是同一句目标就恢复推进；换了目标才是重新开始
pub(in crate::chat) fn mode_state_from(
    raw: &str,
    held: &crate::session::mode::State,
) -> Result<crate::session::mode::State, String> {
    use crate::session::mode::{Status, Working};

    let working = Working::parse(raw).ok_or_else(|| {
        format!(
            "认不出这个作业模式：{raw}。界面只会交 chat / plan 两个名字；目标走 session_goal_set。"
        )
    })?;
    // 目标不是"交互档"命令的事：一格命令改一件事（design-goal-mode.md §3.2）。
    // 定目标走 session_goal_set，那里有整套契约校验与替换确认
    if working == Working::Goal {
        return Err("定目标请用 session_goal_set——目标是一份契约，不是一次切档。".into());
    }
    let mut next = held.clone();
    next.working = working;
    // 规划是只读研究：写操作全被红线拦着，目标在那里寸步难行——
    // 自动转暂停而不是让它空转烧钱。切回对话档**不**自动翻回播放：
    // 那一格是"要不要继续花钱"，归人决定，不归一次交互档切换替他重按
    if working == Working::Plan && next.status == Status::Active {
        next.status = Status::Paused;
    }
    // 旧版挂起机制已被"切档不清零"取代：不再写出那一格
    next.suspended = None;
    Ok(next)
}

/// 花费上限的解析。0 = 不设——那时只有模型上报或用户暂停/结束能让这一支停下来
pub(in crate::chat) fn parse_cap_e8(max_cost_usd: Option<&str>) -> Result<i64, String> {
    match max_cost_usd.map(str::trim) {
        None | Some("") => Ok(0),
        Some(text) => {
            let dollars: f64 = text.parse().map_err(|_| {
                format!("花费上限读不出来：{text}。填一个美元数字，留空才是不设上限。")
            })?;
            if !dollars.is_finite() || dollars < 0.0 {
                return Err("花费上限得是一个不小于 0 的美元数字，留空才是不设上限。".into());
            }
            Ok((dollars * 1e8).round() as i64)
        }
    }
}

/// 契约从界面的 JSON 形状折成 `Contract`。id 缺了由后端补铸（c1..cn——
/// 人在弹框里写的是内容，不是名字）；空文本的行直接丢掉。
/// 校验（≤12 条、单条 ≤200 字……）在 [`crate::goal::contract::validate`]，
/// 这里只管形状
pub(in crate::chat) fn contract_from_json(
    criteria: Option<&Value>,
    constraints: Option<&[String]>,
) -> Result<Option<crate::goal::contract::Contract>, String> {
    use crate::goal::contract::{Contract, Criterion, CriterionKind};

    let constraint_list: Vec<String> = constraints
        .map(|list| {
            list.iter()
                .filter(|c| !c.trim().is_empty())
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    // 没给判据（或给了个非数组）：只落约束的那半边，判据留给上层去说"还差一条"
    let Some(items) = criteria.and_then(Value::as_array) else {
        return Ok((!constraint_list.is_empty()).then(|| Contract {
            criteria: Vec::new(),
            constraints: constraint_list,
        }));
    };
    let mut parsed = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let text = item["text"].as_str().unwrap_or("").trim().to_string();
        if text.is_empty() {
            continue;
        }
        let id = match item["id"]
            .as_str()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            Some(id) => id.to_string(),
            // 人在弹框里写的是内容，不是名字：id 缺了由后端补铸
            None => format!("c{}", index + 1),
        };
        let kind = match item["kind"].as_str() {
            Some("check") => CriterionKind::Check {
                command: item["command"].as_str().unwrap_or("").trim().to_string(),
            },
            _ => CriterionKind::Judgment,
        };
        parsed.push(Criterion { id, text, kind });
    }
    if parsed.is_empty() && constraint_list.is_empty() {
        // 有行但全空：当成没写，让上层的"还差一条判据"去说话
        return Ok(None);
    }
    Ok(Some(Contract {
        criteria: parsed,
        constraints: constraint_list,
    }))
}

/// 把界面交来的目标请求拼成该落进日志的一行。校验全在这儿，而且它是纯的——命令那一层
/// 只剩"读日志、写一行、存盘"，这格判据因此不需要一台活应用才测得动。
///
/// 三种落法（design-goal-mode.md §3.2）：
/// - **认领**：同一句目标从别的交互档切回来——账全留（轮数、起算点、身份），只翻回推进。
/// - **替换**：另起一支。**必须有 `force`**：一支还在推进的目标被静默换掉，
///   等于把"开始"变成"覆盖"，旧目标的账与结论一起蒸发。确认这件事归人。
/// - 全新：铸新 `goal_id`、新起算点。
///
/// 替换同时把 `started_at` 从 now 起算——从前重定目标会把起算点漂成 now、
/// 花费从 0 重算，那是把唯一的自动刹车静默松一次（洞 5）；现在"另起一支"
/// 本来就该有新账，旧账留在旧目标那一行里
pub(in crate::chat) fn goal_state_from(
    objective: String,
    contract: Option<crate::goal::contract::Contract>,
    max_cost_usd: Option<&str>,
    now: i64,
    held: &crate::session::mode::State,
    profile: Option<String>,
    force: bool,
) -> Result<crate::session::mode::State, String> {
    use crate::session::mode::{Status, Working};

    let wanted = objective.trim().to_string();
    if wanted.is_empty() {
        return Err("目标模式下得把目标写下来——没有目标，续跑就没有方向。".into());
    }
    // 契约的限在门口说同一句话（§3.1）：新目标强制 ≥1 条判据
    let contract = match contract {
        Some(contract) => {
            crate::goal::contract::validate(&wanted, &contract)?;
            Some(contract)
        }
        None => None,
    };
    let cap_e8 = parse_cap_e8(max_cost_usd)?;
    // 另起一支前的确认闸：同一支上已经有一支**在推进**的目标，且这次写的不是同一句
    let replacing =
        held.goal_active() && held.objective.as_deref().map(str::trim) != Some(wanted.as_str());
    if replacing && !force {
        return Err(format!(
            "已有一支推进中的目标：「{}」。结束或暂停它，或确认替换再试一次。",
            held.objective.as_deref().unwrap_or_default()
        ));
    }
    // 从别的交互档（或就在目标档上重提交同一句）认领同一支目标：轮数与起算点延续，
    // 上限与档案以这次填的为准。**已收尾的不认领**（收尾是事实，重定目标才是往下走）；
    // 停着的四格都认领得回来，而认领就是恢复——账一律不动。
    // 目标档上重提交同一句也认领：它没有换目标，走"全新"分支会把起算点漂成 now、
    // 花费从 0 重算——那是把唯一的自动刹车静默松一次（洞 4 的残根）。
    // 真想从头再来的人先按「结束」再定，那是两个动作不是一次提交
    let claiming = !replacing
        && held.objective.as_deref().map(str::trim) == Some(wanted.as_str())
        && held.status.is_held();
    if claiming {
        Ok(crate::session::mode::State {
            working: Working::Goal,
            status: Status::Active,
            max_cost_e8: cap_e8,
            // 认领延续同一支目标：身份跟着账走。旧行没有身份就补铸一个
            goal_id: Some(held.goal_id.clone().unwrap_or_else(mint_goal_id)),
            // 认领时顺手补契约（旧式目标第一次拿到判据）
            contract: contract.or_else(|| held.contract.clone()),
            profile,
            ..held.clone()
        })
    } else {
        Ok(crate::session::mode::State {
            working: Working::Goal,
            objective: Some(wanted),
            started_at: Some(now),
            turns_used: 0,
            max_cost_e8: cap_e8,
            status: Status::Active,
            note: None,
            // 重定目标就是重新开始：上一轮的停法与点名都不跟着走
            profile,
            goal_id: Some(mint_goal_id()),
            contract,
            suspended: None,
            outcome: None,
            paused: None,
        })
    }
}

/// 一轮跑完之后，目标模式下一步做什么。**判据只在这里算一次**：算它的那一处就是发续跑
/// 读数的那一处（`close_round` 从这一格读 `continuing`），而跑回合的循环照这一步走。
/// 两处各算一遍就会出现"界面还以为要接着跑、后端已经停了"那种卡在生成中的僵局
#[derive(Debug)]
pub(crate) enum Next {
    /// 不再自己往下跑。为什么停的那一句已经写进日志里那一行，`ChatEvent::Mode` 的读数带着它。
    /// （曾经还有一格 `Idle`："压根没在推目标"的那种停。判据收成 `goal_active()` 一门之后
    /// 它再也造不出来——收尾了的目标走 [`Next::Stop`]，没目标也走它，两件事在循环里
    /// 本来就落同一条 `_ => break`，两个名字是给读代码的人添的第二套真相）
    Stop,
    /// 再接一轮目标。`notice` 是"按停止只掐了上一轮，这一支还在往下推"那一句——
    /// 它必须在 Done **之后**发（否则会被接进上一轮的气泡里），所以挂在这一格上
    /// 由循环去发，不在收尾那儿就地发
    Go {
        state: crate::session::mode::State,
        notice: Option<String>,
    },
    /// 跑人自己排的那一轮。它永远优先于自动续跑，也不占目标的轮数那一格
    RunUser { text: String },
}

/// 迁移的执行形状。[`Next`] 是判据吐出的决定，这里把它翻译成循环的动作——
/// 每个出口的先决条件在翻译时钉死，非法组合当场崩而不是悄悄落回 break：
/// 静默 break 会把"判据坏了"伪装成"正常结束"，界面上那种"还在等续跑"的僵局
/// 之前就是这么来的（见 Next 的文档）。判据在 [`goal_after_round`] 里只算一次，
/// 循环照这里翻译出的动作走，不许再自己判一遍
pub(crate) enum Step {
    Stop,
    GoalRound {
        armed: crate::session::mode::State,
        notice: Option<String>,
    },
    UserTurn {
        text: String,
    },
}

impl Next {
    /// 非法迁移即崩的显式状态机。两个不变量都在判据一侧天然成立；
    /// 在这里重新验一遍，违反就是判据或队列坏了的实锤——带着坏状态继续跑，
    /// 最轻是把目标的轮数账清零，最重是无人知晓地停在一半
    pub(crate) fn into_step(self) -> Step {
        match self {
            Next::Stop => Step::Stop,
            Next::Go { state, notice } => {
                // 续跑的先决条件：机器确实给这一轮加了账（turns_used 至少 1）。
                // 轮数为 0 的 armed state 带回去 arm，等于把计数器清零重来
                assert!(
                    state.turns_used >= 1,
                    "非法迁移：Next::Go 携带的 armed state 没有给这一轮记账（turns_used = 0）"
                );
                Step::GoalRound {
                    armed: state,
                    notice,
                }
            }
            Next::RunUser { text } => {
                // 排队轮的正文不能是空的：空输入是 Go 那一格的专门语义（不追加用户发言），
                // RunUser 带空串说明队列登记坏了
                assert!(
                    !text.trim().is_empty(),
                    "非法迁移：Next::RunUser 携带空正文——队列登记坏了"
                );
                Step::UserTurn { text }
            }
        }
    }
}

// 续跑判据与它要做的决定，整体住在 [`crate::goal::machine::decide_after_round`] 里——
// 纯的。下面这一层只负责两件事：把事实读齐喂给它，把它吐的效果执行掉。
//
// 从前判据散在这一层与续跑循环两头，于是那段循环需要一台活应用才跑得动，
// 从落地起没有一条测试（working-modes §8 欠着的那笔债）。现在循环里只剩"照效果做"

/// 这一轮新落的条目里的消息行。护栏判读只认这一轮自己产出的东西，
/// 往返整份日志既慢又会把上一轮的产出算进这一轮
pub(in crate::chat) fn pushed_messages(send: &Send) -> Vec<crate::session::entry::Message> {
    use crate::session::entry::EntryPayload;
    let pushed: std::collections::HashSet<&str> = send.pushed.iter().map(String::as_str).collect();
    let Ok(path) = send.opened.log.path() else {
        return Vec::new();
    };
    path.iter()
        .filter(|entry| pushed.contains(entry.id.as_str()))
        .filter_map(|entry| match entry.payload() {
            EntryPayload::Message { message } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

/// 一轮结束时把事实凑齐，交给纯判据，再把吐出来的效果一条条执行掉。
///
/// **判据只在这里算一次**：算它的那一处就是发续跑读数的那一处（`close_round` 从
/// 返回的 `Next` 读 `continuing`）。两处各算一遍就会出现"界面还以为要接着跑、
/// 后端已经停了"那种卡在生成中的僵局
///
/// `goal_round` 说的是"刚收尾的这一轮是不是目标自己接的"。只有它才参与护栏计数
/// （design-goal-mode.md §4.2）：人插话的轮既不许把计数加一，也不许顺手清零
pub(in crate::chat) fn goal_after_round(
    host: &TurnHost,
    conversation_id: &str,
    auto_continue: bool,
    interrupted: bool,
    goal_round: bool,
    send: &mut Send,
) -> Result<Next, String> {
    use crate::goal::{decide_after_round, Effect, RoundInput, Spend};

    let held = crate::session::mode::in_effect(&send.opened.log);
    // 用户在回合中按了暂停：旗子取走即清——它只对该收尾的这一轮生效
    let pause_requested = host.pause.take(conversation_id);
    // 队列只看不取：取走是下面 `RunQueuedTurn` 那条效果的事
    let queued = host.follow_up.peek(conversation_id);
    let spend = if held.max_cost_e8 > 0 {
        match held.started_at {
            None => Spend::Read(None),
            Some(since) => {
                match crate::usage::session_cost_e8_in(&host.config_dir, conversation_id, since) {
                    Ok(spent) => Spend::Read(Some(spent)),
                    Err(_) => Spend::Unreadable,
                }
            }
        }
    } else {
        Spend::NotNeeded
    };

    // 护栏的两格事实：这一轮的产出 + 计数器现值。只有目标自动轮才喂产出
    // （outcome = None 时机器不动计数器）；登记表读不到就当零——那支是头一轮
    let outcome = if goal_round {
        crate::goal::round_outcome(&pushed_messages(send))
    } else {
        None
    };
    let guard = host.guards.get(conversation_id);

    let effects = decide_after_round(RoundInput {
        state: &held,
        auto_continue,
        pause_requested,
        spend,
        queued,
        interrupted,
        outcome,
        guard,
    });
    let mut next = Next::Stop;
    for effect in effects {
        match effect {
            Effect::ClearQueue => {
                host.follow_up.clear(conversation_id);
            }
            Effect::AppendModeRow(state) => {
                send.append_quiet(crate::session::mode::row(&state))?;
                next = Next::Stop;
            }
            // 护栏计数器记回登记表。机器吐什么记什么——它就是唯一算这份算术的地方
            Effect::RememberGuard(guard) => {
                host.guards.remember(conversation_id, guard);
            }
            // 读数之外的那一句：它要落在两轮中间，所以不在这里发，挂在 `Next::Go` 上
            // 由续跑循环在 Done 之后发出去
            Effect::EmitNotice(text) => {
                if let Next::Go { notice, .. } = &mut next {
                    *notice = Some(text);
                }
            }
            Effect::RunGoalRound { armed } => {
                next = Next::Go {
                    state: armed,
                    notice: None,
                }
            }
            Effect::RunQueuedTurn { text } => {
                host.follow_up.pop(conversation_id);
                next = Next::RunUser { text };
            }
            Effect::FinishTurn => next = Next::Stop,
            Effect::Abort { message } => return Err(message),
        }
    }
    Ok(next)
}

/// 一发的收尾：**先**报续跑读数，**再**发 Done。这一先后是承重的——反了之后界面在 Done
/// 上就把这一轮落定，续跑那一轮的字节会被整批丢掉，而它表现成的样子是"它自己停了"。
/// 写成一个出口而不是相邻的两行，是因为两行之间隔着一次改动就足够把它们换个个儿
///
/// 读数由调用方算好交进来（那一格要读台账，需要 `AppHandle`），这里只管先后与 `continuing`
pub(in crate::chat) fn close_round(
    on_event: &dyn EventSink,
    next: &Next,
    view: Option<ModeView>,
    done: ChatEvent,
) {
    if let Some(state) = view {
        on_event.send(ChatEvent::Mode {
            continuing: matches!(next, Next::Go { .. }),
            state,
        });
    }
    on_event.send(done);
}

/// 收尾**一轮**的那一个出口：落排队里的切档 → 算续跑判据 → 按"读数先于 Done"发出去 → 落盘。
///
/// `turn_body` 里有五条路会走到这一步（正常答完、轮与轮之间断旗、流被断、扩展工具前断旗、
/// 工具跑完断旗），它们从前各自手抄一遍 Notice + Done 然后 `return Ok(Next::Idle)`——
/// 于是**输入框上那一次停止顺手把整支目标掐了**：用户想停的是眼前这一句回答，
/// 丢的却是他定下的目标。收成一个出口之后，四条断旗的路与正常答完走同一条判据，
/// 目标该不该接下一轮只由那一格判据说，不再由"有没有被打断"说
///
/// `done` 收的是"拿到这一轮实发的条目 id 之后怎么拼 Done"而不是一个拼好的 Done：
/// 判据可能就地追加一行（暂停、预算到顶的"受阻"），那一行要在 `entry_ids` 里——
/// 参数是先于函数体算好的，传进来就晚了
///
/// **落盘必须排在发事件之前**：Done 一到，界面就会去读存档对账（刷新在跑的线程
/// 看不见的那条话题、重读模式读数）。先发后存，那次读档读到的就是没有收尾行的旧账——
/// 目标明明报完了，面板却被旧读数盖回"推进中"。追加行都发生在这一步之前
/// （`apply_pending_mode` 与判据），所以"先存后发"对下一轮从磁盘重开日志毫无影响
/// 这一轮是不是被打断的。停止旗是**粘的**（一设就一直 true，直到下一次 `register`），
/// 所以"被打断"要在收尾那一刻现读，不能拿早先抄下的一份
pub(in crate::chat) fn interrupted_at_boundary(stop: &std::sync::atomic::AtomicBool) -> bool {
    stop.load(std::sync::atomic::Ordering::Relaxed)
}

pub(in crate::chat) fn close_turn(
    host: &TurnHost,
    conversation_id: &str,
    auto_continue: bool,
    interrupted: bool,
    // 刚收尾的这一轮是不是目标自己接的。只有它参与护栏计数（§4.2）
    goal_round: bool,
    send: &mut Send,
    on_event: &dyn EventSink,
    done: impl FnOnce(Vec<String>) -> ChatEvent,
) -> Result<Next, String> {
    apply_pending_mode(host, conversation_id, send)?;
    let next = goal_after_round(
        host,
        conversation_id,
        auto_continue,
        interrupted,
        goal_round,
        send,
    )?;
    // 读数这一格由"有没有目标"决定，不由判据决定：规划档要报"方案交完了没"，
    // 对话档下挂着的目标也要把轮数与钱报回来——收尾了的目标同样得报出"已报完 / 已受阻"
    let held_now = crate::session::mode::in_effect(&send.opened.log);
    let view = (held_now.working != crate::session::mode::Working::Chat
        || held_now.objective.is_some())
    .then(|| {
        mode_view_in(
            &host.config_dir,
            &host.data_dir,
            conversation_id,
            &send.opened.log,
        )
    });
    let pushed = send.take_pushed();
    // 先存后发，理由见函数注释：界面对 Done 的第一反应就是读档对账
    send.save();
    close_round(on_event, &next, view, done(pushed));
    Ok(next)
}

/// 一轮根本没跑成（`run_turn` 返回 Err）时，把目标落进对应的停格（§4.2）。
/// 与收尾判据走**同一条机器路**：这里只读事实、喂给纯函数、执行吐出的那一行——
/// "错误落 blocked、限流落 usage_limited"的判据住在机器里，不在这里抄第二份。
///
/// 到这一步时那一轮的 `Send` 已经保存收尾（`run_turn` 在返回 Err 之前存盘），
/// 这里另开一次话题写日志是安全的——与线程收尾补落切档旗是同一件事的同一形状。
/// 返回是否落了行：没落（没有目标、目标已停着）调用方照常报错即可
pub(in crate::chat) fn goal_block_on_turn_error(
    host: &TurnHost,
    conversation_id: &str,
    message: &str,
) -> Result<bool, String> {
    use crate::session::entry::NewEntry;

    let mut session = open_session_in(&host.config_dir, &host.data_dir, conversation_id)?;
    let held = crate::session::mode::in_effect(&session.log);
    if !held.goal_held() {
        return Ok(false);
    }
    // 限流/额度与其它错误是两格：出路不同（换档案或等额度 vs 看错误改东西）。
    // 认法与重试闸门同一句字面串——那边改文案这边就跟着瞎，共用常量就不会
    let outcome = if message.contains(RETRYABLE_STATUS_WORD) {
        crate::goal::RoundOutcome::UsageExhausted {
            message: message.to_string(),
        }
    } else {
        crate::goal::RoundOutcome::TurnError {
            message: message.to_string(),
        }
    };
    let guard = host.guards.get(conversation_id);
    let effects = crate::goal::decide_after_round(crate::goal::RoundInput {
        state: &held,
        auto_continue: true,
        pause_requested: false,
        spend: crate::goal::Spend::NotNeeded,
        queued: None,
        interrupted: false,
        outcome: Some(outcome),
        guard,
    });
    let mut landed = false;
    for effect in effects {
        match effect {
            crate::goal::Effect::AppendModeRow(state) => {
                session
                    .log_mut()
                    .append(
                        NewEntry::new(crate::session::mode::row(&state)),
                        crate::session::now_millis(),
                    )
                    .map_err(|error| error.to_string())?;
                landed = true;
            }
            crate::goal::Effect::RememberGuard(guard) => {
                host.guards.remember(conversation_id, guard);
            }
            _ => {}
        }
    }
    if landed {
        session.save()?;
    }
    Ok(landed)
}

/// 自动续跑之前的那一次写：模式那一行（轮数自己加一格）与那一行提法落在同一次开合里。
/// 这里必须是一次独立的开合——上一轮的副本已经收尾保存过、下一轮又自己重开日志，
/// 中间这一趟没有别人的 `send` 可以搭
pub(in crate::chat) fn arm_goal_round(
    host: &TurnHost,
    conversation_id: &str,
    state: &crate::session::mode::State,
) -> Result<(), String> {
    use crate::session::entry::{EntryPayload, NewEntry};

    let mut session = open_session_in(&host.config_dir, &host.data_dir, conversation_id)?;
    let now = crate::session::now_millis();
    // 续跑行里会变的两样读数：钱从台账来（读不出就照实说"读不出来"——
    // 那一格在判据那头是要停的，不说假话），判据清单从日志里的证据聚合
    let spent = spent_reading(state.started_at, |since| {
        crate::usage::session_cost_e8_in(&host.config_dir, conversation_id, since)
    });
    let budget = crate::session::mode::budget_line(spent, state.max_cost_e8);
    let criteria_block = match &state.contract {
        Some(contract) => {
            let goal_id = state.goal_id.clone().unwrap_or_default();
            let rows: Vec<crate::goal::contract::Evidence> =
                crate::goal::contract::evidence_in_effect(&session.log)
                    .into_iter()
                    .filter(|row| row.goal_id == goal_id)
                    .collect();
            crate::goal::contract::criteria_overview(contract, &rows)
        }
        None => String::new(),
    };
    let log = session.log_mut();
    log.append(NewEntry::new(crate::session::mode::row(state)), now)
        .map_err(|error| error.to_string())?;
    log.append(
        NewEntry::new(EntryPayload::CustomMessage {
            custom_type: crate::session::mode::CONTINUATION_TYPE.into(),
            content: crate::session::mode::continuation_row(state, &budget, &criteria_block),
            display: false,
        }),
        now,
    )
    .map_err(|error| error.to_string())?;
    session.save()
}

/// 模型那一句 goal_report 落在**这一轮的 `send`** 里。不另开一次话题去写同一份日志：
/// 一个文件两个写者，后收尾的那一份会把前一份整片盖掉
///
/// `complete` 要过**完成门**（design-goal-mode.md §4.3）：契约里的每条判据都得有
/// 现行文本下的通过证据——本轮 `evidence` 里给的，或此前已交过且判据没改过的。
/// 合并之后还没闭合的 `Check`，且命令是 `Safe` 档的，运行时自己复跑一遍（`rerun`）；
/// 非 `Safe` 不放自动执行的路——`ask` 档用户在这里没有同意过执行任何东西，只接上报。
/// 打回时不落 complete、状态仍是 active：模型从工具结果里看到缺什么，屏上 toast 一句
/// ——今天这条返回串只有模型看得到，用户屏幕上什么都没发生，那正是"点了没反应"的形状
pub(in crate::chat) fn report_goal(
    send: &mut Send,
    args: &Value,
    root: Option<&std::path::Path>,
    rerun: &dyn Fn(&str) -> Result<String, String>,
    on_event: &dyn EventSink,
) -> Result<String, String> {
    use crate::goal::contract::{self, Evidence, Verdict, Verified};
    use crate::session::mode::{self, Status};

    let held = mode::in_effect(&send.opened.log);
    // 目标挂在话题上，不管当下是目标档还是对话档在推它，上报都认
    if !held.goal_active() {
        return Err(
            "这条话题现在没有在推进的目标，这一句上报没有对象。切模式由用户在界面上做。".into(),
        );
    }
    // 模型只能报这两格。`budget_limited` / `usage_limited` 是运行时的闸，
    // `paused` 归用户按——让它自己挑这六格，它就会挑一个最省事的
    let status = match args["status"].as_str() {
        Some("complete") => Status::Complete,
        Some("blocked") => Status::Blocked,
        other => return Err(format!("status 只认 complete 或 blocked，收到 {other:?}。")),
    };
    let note = args["note"].as_str().unwrap_or("").trim().to_string();
    if note.is_empty() {
        return Err(
            "note 是空的。complete 要写清交付了什么、还剩什么没做；blocked 要写清卡在哪。".into(),
        );
    }

    if status == Status::Blocked {
        let next = mode::State {
            status,
            note: Some(note.clone()),
            ..held
        };
        send.append_quiet(mode::row(&next))?;
        return Ok(format!(
            "已记下：目标停住了。{note}\n这一支不再自动续跑，等用户决定接下来怎么办。"
        ));
    }

    // ---- complete：过门 ----
    // 旧式目标没有契约：门退化成今天的行为（note 非空即过），界面上标「无判据」。
    // 它一旦被 edit 过一次就必须补齐契约，所以这只对从未编辑过的老目标发生
    let (Some(contract), Some(goal_id)) = (&held.contract, &held.goal_id) else {
        let next = mode::State {
            status: Status::Complete,
            note: Some(note.clone()),
            ..held
        };
        send.append_quiet(mode::row(&next))?;
        return Ok(format!("已记下：目标完成。{note}\n这一支不再自动续跑。"));
    };

    let prev_rows: Vec<Evidence> = contract::evidence_in_effect(&send.opened.log)
        .into_iter()
        .filter(|row| &row.goal_id == goal_id)
        .collect();
    let mut new_rows = contract::parse_reported_evidence(args, contract, goal_id, held.turns_used)?;

    // 复跑排程：本轮上报 + 历史证据合并之后还没闭合的 Check，且命令是 Safe 档的，
    // 运行时自己再跑一遍。结果也是证据行（pass/fail 都落），跟着本轮一起进日志。
    // 非 Safe 的不进排程——那半格判据住在 `rerun_schedule` 里，那里有反向钉
    let mut combined = prev_rows.clone();
    combined.extend(new_rows.iter().cloned());
    let is_safe = |command: &str| {
        tools::classify(
            "run_command",
            &serde_json::json!({ "command": command }),
            root,
        ) == tools::Risk::Safe
    };
    for criterion in contract::rerun_schedule(contract, &combined, is_safe) {
        let command = criterion.command().unwrap_or_default();
        let (verdict, summary, output) = match rerun(command) {
            Ok(output) => (
                Verdict::Pass,
                format!("复跑通过：{command}"),
                Some(short_excerpt(&output)),
            ),
            Err(error) => (
                Verdict::Fail,
                format!("复跑失败：{command}"),
                Some(short_excerpt(&error)),
            ),
        };
        new_rows.push(Evidence {
            goal_id: goal_id.clone(),
            criterion_id: criterion.id,
            criterion_text: criterion.text,
            round: held.turns_used,
            verdict,
            summary,
            output,
            verified: Verified::Runtime,
        });
    }

    combined.extend(new_rows.iter().cloned());
    let audit = contract::audit(contract, &combined);
    // 证据先落行：append-only 的事实。过不过门都不改它们——过一条是一条，
    // 失败的摆在那里，直到有一条更新的把它盖掉
    for row in &new_rows {
        send.append_quiet(contract::evidence_row(row))?;
    }
    if audit.unproven.is_empty() {
        let next = mode::State {
            status: Status::Complete,
            note: Some(note.clone()),
            ..held
        };
        send.append_quiet(mode::row(&next))?;
        return Ok(format!("已记下：目标完成。{note}\n这一支不再自动续跑。"));
    }
    on_event.send(ChatEvent::Notice {
        text: format!("没算完成：还差 {} 条判据", audit.unproven.len()),
    });
    Err(format!(
        "没算完成，这一支继续推进。还没过门的判据：\n{}\n继续做；证据齐了再重新上报 complete。\
         审计要证明完成，不是没发现明显没做的就算完。",
        audit.unproven.join("\n")
    ))
}

/// 复跑输出进证据行的摘录上限。全文在命令输出里，证据要的是认得出结果
pub(in crate::chat) fn short_excerpt(text: &str) -> String {
    const LIMIT: usize = 400;
    if text.chars().count() <= LIMIT {
        return text.trim().to_string();
    }
    let cut: String = text.chars().take(LIMIT).collect();
    format!("{}…", cut.trim_end())
}
