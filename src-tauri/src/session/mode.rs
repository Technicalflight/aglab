//! 作业模式：这一条话题现在怎么干活——对话、规划（只看不改）、目标（一轮一轮自己推进）。
//!
//! 它住在这里而不是 `config.json`，理由有三个：
//!
//! 1. **它是话题的事实，不是整机的设置。** 权限档位问的是"这一下要不要有人点头"，
//!    而模式问的是"这一支现在准不准动手"。把后者塞进前者那一个全局字段，
//!    就等于让"切个话题"能改变另一条话题的护栏。
//! 2. **日志是唯一真相。** 模式作为一条只追加的 `custom` 条目存在，于是回溯与分叉
//!    天然带回当时的模式——不需要另存一份"这条话题的模式"再想该信哪份。
//! 3. **常驻段不能带它。** 说给模型的那段规矩坐在 [`super::sections`] 的命名段里，
//!    换模式只是在末尾追加一行差分行；塞进默认提示词那个 `const` 就等于换掉整段前缀。
//!
//! 读侧是宽容的：没有那一行、或者读不懂，一律按对话模式——与"这行不存在"同义，
//! 也就是与今天的行为同义。这里不套"认不出来退到最严"那条规矩：最严的那一档是规划，
//! 它会静默地把用户挡在写操作之外，而一条坏掉的 JSON 没有资格做这个决定。

use serde::{Deserialize, Serialize};

use super::context::latest_custom_entry;
use super::entry::{EntryPayload, Message, StopReason};
use super::log::SessionLog;

/// 模式条目在 `custom.custom_type` 里的名字
pub const ENTRY_TYPE: &str = "session_mode";

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Working {
    /// 今天的行为：问不问由权限档位说了算
    #[default]
    Chat,
    /// 只看不改。执行侧的红线，与权限档位无关
    Plan,
    /// 有一个目标，一轮接一轮自己推进
    Goal,
}

/// 目标现在处于哪一格。**六值，而且 `paused` 是其中一格而不是旗子**：
/// 从前 `Status{active,complete,blocked}` 配一个 `paused: bool`，于是"这一支怎么样了"
/// 要两格互相解释才说得清——判据里长出 `goal_active()` 与 `goal_running()` 两扇门、
/// 界面上长出"待命"这种哪儿都不是的第五个词、`Next::Idle` 与 `Next::Stop` 变成
/// 一对不可观测地相同的名字。三件失败也各不相关：模型卡住要改目标、钱到顶要调上限、
/// 账号额度到顶要换档案，混在一格里界面只能说"受阻"
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// 还在推进。**自动续跑只认这一格**
    #[default]
    Active,
    /// 用户按了暂停（或切进规划档被强制暂停）。恢复只翻这一格，账一律不动
    Paused,
    /// 模型说它卡住了。等人改目标，不等人掏钱
    Blocked,
    /// 服务商/账号不给量了。不是这一支做错了什么，出路是换档案或等额度
    UsageLimited,
    /// 花费上限到了。唯一的自动刹车咬下来的就是这一格
    BudgetLimited,
    /// 模型上报做完了。要往下走得重定一个目标
    Complete,
}

/// 旧日志里 `outcome` 那一格的三种写法。**只为读得懂 2026-10-02 之前的行而存在**：
/// 新行写 `status`，读侧在 [`in_effect`] 里折一次，下游从此只见一套
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LegacyOutcome {
    #[default]
    Active,
    Complete,
    Blocked,
}

/// 模式在线上那三个短名。写一次、两处读（读数与命令的入参）；中文标签归界面自己那张表，
/// 与权限档位同一形状，不在 Rust 里拼句子
impl Working {
    pub fn name(self) -> &'static str {
        match self {
            Working::Chat => "chat",
            Working::Plan => "plan",
            Working::Goal => "goal",
        }
    }

    /// 认不出来的写法一律退回对话模式（见模块头那条"读不懂就当没有那一行"）
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "chat" => Some(Working::Chat),
            "plan" => Some(Working::Plan),
            "goal" => Some(Working::Goal),
            _ => None,
        }
    }
}

impl Status {
    /// 线上那个短名。界面按它查自己那张中文表，不在 Rust 里拼句子
    pub fn name(self) -> &'static str {
        match self {
            Status::Active => "active",
            Status::Paused => "paused",
            Status::Blocked => "blocked",
            Status::UsageLimited => "usage_limited",
            Status::BudgetLimited => "budget_limited",
            Status::Complete => "complete",
        }
    }

    /// 认不出来的写法返回 None——**不猜**。猜成 active 会让一支停着的目标自己接下去烧钱，
    /// 猜成 complete 会让人以为它做完了；退到"读不懂就当没有那一行"那条规矩上
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "active" => Some(Status::Active),
            "paused" => Some(Status::Paused),
            "blocked" => Some(Status::Blocked),
            "usage_limited" => Some(Status::UsageLimited),
            "budget_limited" => Some(Status::BudgetLimited),
            "complete" => Some(Status::Complete),
            _ => None,
        }
    }

    /// 还挂着账的（要往下走得由人翻回来，但没被销毁）。`Complete` 是唯一翻不回去的：
    /// 收尾是事实，重定目标才是往下走
    pub fn is_held(self) -> bool {
        self != Status::Complete
    }

    /// 能不能"恢复"。四格停着的都能，而且**恢复不算重新开始**——所以账一律不动
    pub fn is_resumable(self) -> bool {
        matches!(
            self,
            Status::Paused | Status::Blocked | Status::UsageLimited | Status::BudgetLimited
        )
    }
}

/// 钱到顶那一句的记号。旧日志里它与"模型说卡住"共用 `blocked`，只能靠这句话把它们分开
const BUDGET_NOTE_MARK: &str = "花费到了上限";

/// 旧行（`outcome` + `paused` 两格）折成一格 `status`。
///
/// 认不出来的就留在原格，**不猜**：`blocked` 与 `budget_limited` 的差别是"该动哪个旋钮"，
/// 猜错一次会让人去改目标而钱其实没到顶
fn status_from_legacy(outcome: LegacyOutcome, paused: bool, note: Option<&str>) -> Status {
    match outcome {
        LegacyOutcome::Active if paused => Status::Paused,
        LegacyOutcome::Active => Status::Active,
        LegacyOutcome::Complete => Status::Complete,
        LegacyOutcome::Blocked
            if note.is_some_and(|note| note.contains(BUDGET_NOTE_MARK)) =>
        {
            Status::BudgetLimited
        }
        LegacyOutcome::Blocked => Status::Blocked,
    }
}

/// 一条话题此刻的作业模式。目标那几格只在 [`Working::Goal`] 下有意义
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", default)]
pub struct State {
    pub working: Working,
    pub objective: Option<String>,
    /// 目标从这一刻起算。花费熔断拿它去台账里聚合，日志不再自己存一份钱——
    /// 同一笔钱住两处就是要人猜该信哪一处
    pub started_at: Option<i64>,
    /// 目标模式**自己发起过**几轮续跑。它是读数不是配额——目标没有轮次上限，
    /// 做到哪儿算哪儿。用户自己发的消息不占这一格
    pub turns_used: u32,
    /// 花费上限，单位 1e-8 美元（与台账同一把尺）。0 = 不设
    pub max_cost_e8: i64,
    /// 目标现在处于哪一格。六值，见 [`Status`]
    #[serde(default)]
    pub status: Status,
    /// 收尾那句：完成时的结论，或停住时的原因
    pub note: Option<String>,
    /// 旧行的 `outcome` 那一格。**只为读得懂历史而存在**：`skip_serializing` 让新行
    /// 永远不写它，[`in_effect`] 读一次就把它折进 `status`，下游不再见第二套。
    /// 新代码不许读它，也不许写它——要问状态只看 [`State::status`]。（`pub` 只是为了
    /// 让 `State { ..held }` 那种结构体更新语法可用，不是给它开的接口）
    #[serde(default, skip_serializing)]
    pub outcome: Option<LegacyOutcome>,
    /// 旧行的 `paused` 旗子。同上，读它请走 [`State::paused()`]
    #[serde(default, skip_serializing)]
    pub paused: Option<bool>,
    /// 目标点名执行的服务商档案（设置页那些卡片之一的 id）。
    /// Some = 目标期间（含用户插话的轮）整份连接域照这张档案走，池子与路由都不抢；
    /// None = 跟随当前配置。存 id 不存快照：档案改了连接，目标跟着新连接走
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// 目标的身份。立约时铸一个（`goal-<hex>`）、之后不改：`edit` 认它（同一支换文字
    /// 不换账）、分叉认它（角落卡把同一支的多支合成一组）、台账聚合也认它。
    /// 旧行没有它：读侧补铸 `legacy-<条目id>`，不伪造创建时间
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_id: Option<String>,
    /// 完成契约（design-goal-mode.md §3.1）：判据 + 约束。`None` = 旧式目标
    /// （一句 objective），完成门对它退化、界面标「无判据」。
    /// 首轮定形，之后只有 `edit` 能动；判据文本一改，它的证据作废
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<crate::goal::contract::Contract>,
    /// 旧版「挂起」机制的落点。挂起已被"切档不清零"取代——切到对话/规划时
    /// 目标字段留在主格继续推进（规划档自动暂停）。这一格只为读得懂旧日志，
    /// [`in_effect`] 会把它归一化回主格，写出时不再出现
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspended: Option<Suspended>,
}

/// 旧版挂起体的形状。只为读懂今天之前写下的那几行日志，见 [`State::suspended`]
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", default)]
pub struct Suspended {
    pub objective: Option<String>,
    pub started_at: Option<i64>,
    pub turns_used: u32,
    pub max_cost_e8: i64,
    pub outcome: LegacyOutcome,
    pub note: Option<String>,
}

impl State {
    /// 这条话题还让不让动手。执行侧只认这一句
    pub fn phase(&self) -> crate::policy::Phase {
        match self.working {
            Working::Plan => crate::policy::Phase::Plan,
            _ => crate::policy::Phase::Chat,
        }
    }

    /// 这支身上挂着一个**正在推进**的目标。**它与交互档是两件事**：目标挂在话题上，
    /// 对话/规划只是当下怎么交互——切到对话档目标照跑，切到规划档才强制暂停
    /// （只读红线让目标寸步难行，让它继续只会烧钱卡死）。
    /// 自动续跑只认这一句：6 值里 `paused` 自己就是一格，不用再去问第二格旗子
    pub fn goal_active(&self) -> bool {
        self.objective.is_some() && self.status == Status::Active
    }

    /// 还挂着账的目标（停着的、被钱咬住的、被额度挡住的都算）。界面上要有那一行——
    /// 只有 `complete` 算翻篇，要往下走得重定一个目标
    pub fn goal_held(&self) -> bool {
        self.objective.is_some() && self.status.is_held()
    }

    /// 是不是"被暂停"那一格。留这个访问器是为了让原先读 `paused` 旗子的那几处
    /// 不改语义：它从前是旗子，现在是 6 值里的一格
    pub fn paused(&self) -> bool {
        self.status == Status::Paused
    }

    /// 自动续跑那一轮该落进日志的那一行：轮数加一格，其余一个字段都不动。
    /// 这一格只是数给界面看的——目标没有轮次上限，所以它是计数器，不是配额
    pub fn armed(&self) -> State {
        State {
            turns_used: self.turns_used + 1,
            ..self.clone()
        }
    }
}

/// 把模式落成一条待追加的条目
pub fn row(state: &State) -> EntryPayload {
    EntryPayload::Custom {
        custom_type: ENTRY_TYPE.into(),
        data: serde_json::to_value(state).ok(),
    }
}

/// 当前分支上生效的模式。判据与段一样是"后写胜出"，但读的是父链而不是整棵树，
/// 所以切到另一支就带回那一支当时的模式。
///
/// 读侧做两次归一化，之后下游只见一套字段：
/// 1. 旧版「挂起」行（切档时把目标挪进 `suspended` 格的那批日志）展开回主格——
///    挂起在当时就是"不在推进"，归一成 `paused` 那一格，语义一字不变。
/// 2. 旧行的两格（`outcome` + `paused` 旗子）折成一格 [`Status`]。新行根本不带
///    `outcome`，所以那一步只在读历史时走。归一化只发生在内存，写回的行不再带这些格
///
/// 还有一件补铸：`goal_id` 是后来才有的格，旧行没有它。挂着目标却没有身份的行，
/// 读侧按那一行的条目 id 补铸 `legacy-<id>`——不为它伪造创建时间，只给分叉分组与
/// `edit` 一个认得出"同一支"的名字
pub fn in_effect(log: &SessionLog) -> State {
    let held = latest_custom_entry(log, ENTRY_TYPE)
        .ok()
        .flatten()
        .map(|(id, data)| (id.to_string(), data.clone()));
    let mut state = match &held {
        Some((_, data)) => serde_json::from_value::<State>(data.clone()).ok(),
        None => None,
    }
    .unwrap_or_default();
    if state.objective.is_some() && state.goal_id.is_none() {
        if let Some((entry_id, _)) = held.as_ref() {
            state.goal_id = Some(format!("legacy-{entry_id}"));
        }
    }
    if let Some(parked) = state.suspended.take() {
        if state.objective.is_none() {
            state.objective = parked.objective.clone();
            state.started_at = parked.started_at;
            state.turns_used = parked.turns_used;
            state.max_cost_e8 = parked.max_cost_e8;
            state.note = parked.note.clone();
            state.status = status_from_legacy(parked.outcome, true, parked.note.as_deref());
        }
    }
    if let Some(outcome) = state.outcome.take() {
        let paused = state.paused.take().unwrap_or(false);
        state.status = status_from_legacy(outcome, paused, state.note.as_deref());
    }
    state
}

/// 规划模式下，模型这一支把话交完了没有。**这不是一格新状态，是从日志派生的**：
/// 最新一条用户发言之后，已经有一条以文本收尾的 assistant 行（`stop == Stop`）。
/// 于是"刚切到规划、还没说话"时它是 false，模型答完一轮才真；用户再追问一句，
/// 它自己退回 false，直到下一份方案交出来——批准按钮因此永远对应最新那一份，
/// 而不是上一份被留在屏上。
///
/// 只看 `stop` 这一条就够：带工具调用的中间行写的是 `ToolUse`，被截断的是 `Length`，
/// 被停止的是 `Aborted`，出错的是 `Error`。四条写入路径（实时收尾、实时中间行、
/// 迁移、让步）都不产出"`Stop` + 带工具调用"那种行，所以再判一次 `tool_calls.is_empty()`
/// 就是表上一条没人读得到的默认值——加了它，改坏也照样绿，那是装饰不是闸门。
///
/// `last_user > 0` 那一半有真实来源：迁移会丢掉正文为空的 user 行，一支可能只剩一条回答。
/// 那种情况下没有提问要回答，不该点亮批准。
///
/// `last_plan > last_mode` 是第三条：收尾行必须晚于"切进这个模式"那一行。少了它，
/// 用户在对话里拿到一份回答、再切到规划，那条**不是规划产物**的旧回答会让批准按钮
/// 立刻亮起来——屏上摆的是一个没按这套规矩交出来的东西
pub fn plan_delivered(log: &SessionLog) -> bool {
    let Ok(path) = log.path() else { return false };
    let mut last_user = 0_u64;
    let mut last_mode = 0_u64;
    let mut last_plan = 0_u64;
    for entry in path {
        match entry.payload() {
            EntryPayload::Message {
                message: Message::User { .. },
            } => last_user = entry.seq,
            EntryPayload::Message {
                message: Message::Assistant(settled),
            } => {
                if settled.stop == StopReason::Stop {
                    last_plan = entry.seq;
                }
            }
            EntryPayload::Custom { custom_type, .. } if custom_type == ENTRY_TYPE => {
                last_mode = entry.seq;
            }
            _ => {}
        }
    }
    last_user > 0 && last_plan > last_user && last_plan > last_mode
}

/// 自动续跑那一行的 `custom_type`。它是 `custom_message`：投影成 system 行进上下文，
/// 界面上不冒充"用户又问了一次"
pub const CONTINUATION_TYPE: &str = "goal_continue";

/// 会变的读数住这里，不变的计划住命名段（design-goal-mode.md §4.4）。
/// 预算那一行由调用方从台账读好喂进来——这一格一旦去开台账，就只剩实盘一种测法。
/// 0 上限要说成"无上限"而不是"$0.00"：后者会被读成"一分钱都不能花"，意思恰恰相反
pub fn budget_line(spent: Option<i64>, cap_e8: i64) -> String {
    match (cap_e8 > 0, spent) {
        (true, Some(spent)) => format!("已花 {} / 上限 {}", usd(spent), usd(cap_e8)),
        (true, None) => format!("已花读不出来 / 上限 {}（账那一头报错时这一支会停）", usd(cap_e8)),
        (false, Some(spent)) => format!("无上限 · 已花 {}", usd(spent)),
        (false, None) => "无上限（已花读不出来）".to_string(),
    }
}

/// 那一行的正文。它必须自报"这不是用户新问的"——少了这一句，模型读到的就是一句
/// 凭空出现的催促，会以为用户在催它，而不是这一支在自己往下走。
///
/// 分节照 Codex 的 continuation.md 骨架（§4.4）：预算与轮数、判据清单（每轮重锚——
/// todo 那一格）、约束、规矩（跨轮持久 / 以现状为准 / 区分空转 / 完成审计 / blocked 审计）。
/// `budget_line` 与判据清单由调用方喂进来：预算要台账，判据要证据聚合，两者都
/// 不是这一格（纯函数）该去碰的 IO
pub fn continuation_row(state: &State, budget: &str, criteria_block: &str) -> String {
    let objective = state.objective.as_deref().unwrap_or("（没写下目标）");
    let constraints = match &state.contract {
        Some(contract) if !contract.constraints.is_empty() => {
            let lines: Vec<String> = contract
                .constraints
                .iter()
                .map(|constraint| format!("  - {constraint}"))
                .collect();
            format!("约束（推进期间不许动什么）：\n{}\n", lines.join("\n"))
        }
        _ => String::new(),
    };
    let criteria = if criteria_block.is_empty() {
        String::new()
    } else {
        format!("判据（□ = 还没闭合）：\n{criteria_block}\n")
    };
    let n = state.turns_used;
    format!(
        "（目标模式自动续跑，不是用户新问的一句话。）\n\
目标：{objective}\n\
预算：{budget} · 这是第 {n} 轮\n\
{criteria}{constraints}\
规矩：\n\
- 目标跨轮持久：不许缩水目标，不许用更窄更安全的替代方案偷换它。\n\
- 以工作目录现状为准，不凭上一轮的记忆——上一轮做完的部分不要重做，接着往下做。\n\
- 区分 progress / verified wait / no progress：在等一个慢命令不等于没进展，\
观察超时也不等于终止。\n\
- 没有轮次上限，做到完为止；拦得住这一支的只有花费上限、用户暂停、用户结束。\n\
- 每轮收口前做完成审计：把完成视为未证明，对照判据逐条找权威证据\
（命令输出、测试结果、文件状态），证据不足视为未达成；证据齐了才调 goal_report(status=\"complete\")。\n\
- 真卡住了才调 goal_report(status=\"blocked\")，说清卡在哪、需要什么才能往下；\
同一个障碍要连续多轮才行，别为一次失败收手。"
    )
}

/// 说给模型听的那一段正文。放在这里是因为"告诉模型的规矩"与"执行侧拦的东西"
/// 必须同一个出处各写一遍——分开写迟早漂移成两句，而漂移的那一句是模型先撞上。
///
/// 目标段跟着**目标**走而不是跟着交互档走：目标挂在话题上，切到对话档它照常推进
/// （模型照常收到目标段），规划档下不出这段——规划是只读研究，目标在那里被强制暂停
pub fn section_body(state: &State) -> Option<String> {
    match state.working {
        Working::Plan => Some(PLAN_BODY.to_string()),
        Working::Chat | Working::Goal => goal_section(state),
    }
}

/// 目标段：这一支身上挂着目标时说给模型的那几句。没有目标 = 没有段。
///
/// **段只说不变的规矩**（design-goal-mode.md §4.4）：目标、契约、审计规矩、真闸清单。
/// 第几轮与花费读数住续跑行——它们每轮都变，写进段里就是每轮一条差分行
/// （实测：一条跑过 4 轮续跑的日志里 `session_mode` 差分行 4 条，而别的段各 1 条）。
///
/// 六格各说各的出路——`blocked` 要人改目标、`budget_limited` 要人调上限、
/// `usage_limited` 要人换档案或等额度。合成一句"受阻"，模型就只会等着，
/// 而这三件事里只有一件是它能建议的
fn goal_section(state: &State) -> Option<String> {
    let objective = state.objective.as_deref()?;
    Some(match state.status {
        Status::Paused => format!(
            "你同时挂着一个已暂停的目标：{objective}\n这个目标刚被用户暂停：不要再自己往下推进，\
             也不要调 goal_report。用户在这条话题里说的话照常回应；\
             恢复推进由用户决定，不由你宣布。"
        ),
        Status::Active => format!(
            "{body}\n目标：{objective}{contract}\n没有轮次上限——一轮一轮做到完为止，\
不要为了省轮次而半途交差。设了花费上限的话，到顶会把这个目标落成「预算花完」那一格；\
没设的话就没有自动刹车。除此之外只有你上报、或用户把它暂停或结束，这一支才会停。\
用户在输入框上按的那次停止不算——它只掐当前这一轮，这一支照常往下走，\
别把它读成让你停手。",
            body = if state.working == Working::Goal {
                GOAL_BODY
            } else {
                GOAL_BODY_CHAT
            },
            contract = contract_block(state),
        ),
        Status::Complete => format!(
            "你同时挂着一个已经上报收尾（完成）的目标：{objective}——{note}\n不再自动续跑。\
             这一格留在账上只是让你知道来龙去脉，用户接着问什么就答什么，\
             不要自己宣布新一轮目标。",
            note = state.note.as_deref().unwrap_or("（没附上结论）")
        ),
        Status::Blocked => format!(
            "你同时挂着一个停住了的目标：{objective}——{note}\n不再自动续跑。\
             要把它接回去，得由用户改目标、调花费上限或者重新起一轮——不要替他猜该怎么往下走。",
            note = state.note.as_deref().unwrap_or("（没附上原因）")
        ),
        Status::BudgetLimited => format!(
            "你同时挂着一支因为花费到了上限而停住的目标：{objective}——{note}\n它不是做完了，是钱到顶。\
             不再自动续跑：要往下走得由用户调高上限或重定目标，不要替他决定该花多少钱，\
             也不要把它报成完成。",
            note = state.note.as_deref().unwrap_or("（没附上原因）")
        ),
        Status::UsageLimited => format!(
            "你同时挂着一支被额度挡住而停住的目标：{objective}——{note}\n这一支没做错什么，\
             是服务商或账号不给量了。不再自动续跑：出路是用户换一张服务商档案或等额度恢复，\
             不要一轮一轮去撞同一扇门。",
            note = state.note.as_deref().unwrap_or("（没附上原因）")
        ),
    })
}

/// 契约进段的形状。它是静态的：契约首轮定形，之后只有 edit 能动——
/// 所以这几行不会每轮都变，段也就不会每轮多一条差分行
fn contract_block(state: &State) -> String {
    let Some(contract) = &state.contract else {
        return String::new();
    };
    let mut lines = vec!["\n完成契约（怎么才算真做到）：" .to_string()];
    for criterion in &contract.criteria {
        let kind = match &criterion.kind {
            crate::goal::contract::CriterionKind::Check { command } => {
                format!("跑命令：收尾时会复跑 `{command}`")
            }
            crate::goal::contract::CriterionKind::Judgment => "要人看：只接受你的上报".to_string(),
        };
        lines.push(format!("  - [{}] {}（{kind}）", criterion.id, criterion.text));
    }
    if !contract.constraints.is_empty() {
        lines.push("约束（推进期间不许动什么）：".to_string());
        for constraint in &contract.constraints {
            lines.push(format!("  - {constraint}"));
        }
    }
    format!("\n{}", lines.join("\n"))
}

/// 1e-8 美元折回那句人看得懂的钱。单位与台账同一把尺，不在日志里另存一份金额。
/// 段正文与"停在哪儿"那两句都从这儿走，两处各折一次就会一处说 $2.5 一处说 $2.50
pub fn usd(e8: i64) -> String {
    format!("${:.2}", e8 as f64 / 1e8)
}

const PLAN_BODY: &str = r#"现在是规划模式：只看不改。
可以用的还是那几条读的路——read_file、list_files、search_text、web_fetch、load_skill、list_windows、knowledge_search。其余会动东西的一律被执行侧当场拒掉，包括写文件、编辑文件、执行命令、打开程序或网址、操作别的程序、读窗口控件、动扩展（MCP）提供的工具、派子助理。这条红线与权限档位无关：换成「完全访问」也开不动，所以不要试，也不必为它道歉。
你的产出是一份能直接照着执行的方案，写清这几件事：要改哪几处（文件与函数点名）、按什么顺序、每一步用什么验证它真的成了、你没把握或打算跳过的地方。方案里不要放「我接下来先问一下」这类步骤——用户批准之后你才动手，中间没有人可以问。
把方案交完就停下来。不要在规划模式里开始执行，哪怕只差一步。"#;

const GOAL_BODY: &str = r#"现在是目标模式：有一个目标要一轮一轮自己推进到完成。
用户不在旁边等着，所以不要为每一步回来问一句——问了就等于把这一支卡住。要做的判断自己下，拿不准就往保守的方向做，并把这一处写进最后的结论里。
每做完一轮就接着往下走。**每一轮收口前先做完成审计**：把完成视为未证明，对照契约里的判据逐条找权威证据——命令输出、测试结果、文件状态——证据不足视为未达成；不许用窄检查支撑宽主张。证据齐了才调 goal_report(status="complete", note=…) 交回那句结论；真的卡住了调 goal_report(status="blocked", note=…) 说清卡在哪、需要什么才能往下。**这一句不调，这一支就不会停**——它会一轮一轮接着跑。拦得住它的只有三样：花费到顶、用户按暂停、用户结束这个目标。输入框上那次停止**不算**：它只掐当前这一轮，那一轮停了这一支还会接着往下推，所以别把一次中断当成"用户让我停下来等"。"#;

/// 对话档下的目标段。与 [`GOAL_BODY`] 的差别只有一句：用户就在旁边说话——
/// 他的消息永远优先于目标，答完没有新输入时再接着推目标
const GOAL_BODY_CHAT: &str = r#"这条话题上挂着一个要一轮一轮推进到完成的目标，同时用户会随时在这里说话。
用户的消息永远优先：他问什么就答什么，答完若无新输入就接着推目标——聊天与推进共用这一支，不要把用户的话当成新目标，也不要为了等他把目标晾住。
要做的判断自己下，拿不准就往保守的方向做，并把这一处写进最后的结论里。真的做完时调 goal_report(status="complete", note=…) 交回那句结论，卡住做不下去时调 goal_report(status="blocked", note=…) 说清卡在哪、需要什么才能往下。**这一句不调，这一支就不会停**——它会一轮一轮接着跑。拦得住它的只有花费到顶、用户按暂停、用户结束这个目标；输入框上那次停止只掐当前这一轮，停完这一支照样往下推，所以不要把一次中断读成"用户不要这个目标了"。"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::entry::{NewEntry, SettledAssistant, ToolCall};
    use serde_json::Value;

    const T0: i64 = 1_700_000_000_000;

    fn apply(log: &mut SessionLog, state: &State) {
        log.append(NewEntry::new(row(state)), T0)
            .expect("追加该成功");
    }

    fn push(log: &mut SessionLog, message: Message) {
        log.append(NewEntry::new(EntryPayload::Message { message }), T0)
            .expect("追加该成功");
    }

    fn answered(stop: StopReason, with_call: bool) -> Message {
        Message::Assistant(SettledAssistant {
            content: "一份方案".into(),
            tool_calls: if with_call {
                vec![ToolCall {
                    id: "c1".into(),
                    name: "read_file".into(),
                    arguments: "{}".into(),
                    content_chars: None,
                }]
            } else {
                Vec::new()
            },
            stop,
            reasoning: None,
            error: None,
            thinking_signature: None,
            reasoning_items_json: None,
        })
    }

    /// 批准按钮的判据：模型把话说完了，才算"方案交出来了"。
    /// 切进规划模式那一刻还没有方案——那时按钮该是不存在的
    #[test]
    fn a_plan_counts_only_after_the_model_finishes_its_answer() {
        let mut log = SessionLog::new();
        assert!(!plan_delivered(&log), "空日志没有方案");

        push(&mut log, Message::User {
            content: "怎么做？".into(),
            images: Vec::new(),
            audios: Vec::new(),
            videos: Vec::new(),
        });
        assert!(!plan_delivered(&log), "用户刚问完，方案还没有");

        push(&mut log, answered(StopReason::ToolUse, true));
        assert!(!plan_delivered(&log), "带工具调用的那一行不是「交完了」");

        push(&mut log, answered(StopReason::Stop, false));
        assert!(plan_delivered(&log), "以文本收尾的那一条才算交完");

        // 用户再追问一句：批准该消失，直到下一份方案交出来——
        // 留在屏上的按钮指向的是上一份，那才是真危险
        push(&mut log, Message::User {
            content: "第二步再细点".into(),
            images: Vec::new(),
            audios: Vec::new(),
            videos: Vec::new(),
        });
        assert!(!plan_delivered(&log), "上一份方案不该继续挂着批准");
    }

    /// 半截的回答不算交完：被停止、撞输出上限、流出错都不该让人去批一个没写完的方案
    #[test]
    fn an_incomplete_answer_is_not_a_delivered_plan() {
        for stop in [StopReason::Aborted, StopReason::Length, StopReason::Error] {
            let mut log = SessionLog::new();
            push(&mut log, Message::User {
                content: "问".into(),
                images: Vec::new(),
                audios: Vec::new(),
                videos: Vec::new(),
            });
            push(&mut log, answered(stop, false));
            assert!(!plan_delivered(&log), "{stop:?} 收尾的那一条不该点亮批准");
        }
    }

    /// 一支上没有任何用户发言时不点亮批准。这不是假想：迁移会丢掉正文为空的 user 行，
    /// 于是可能留下"只有回答、没有提问"的一支——那种屏上没有方案可批
    #[test]
    fn an_answer_with_no_question_is_not_a_delivered_plan() {
        let mut log = SessionLog::new();
        push(&mut log, answered(StopReason::Stop, false));
        assert!(!plan_delivered(&log), "没有提问就不该有批准");
    }

    /// 切进规划模式**之前**那份回答不算"方案交出来了"：用户在对话里聊完再切过来，
    /// 屏上那份不是按这套规矩交的，按钮不该对着它亮。只有切进来之后答完的那一份才算
    #[test]
    fn an_answer_from_before_the_mode_switch_is_not_delivered() {
        let mut log = SessionLog::new();
        push(&mut log, Message::User {
            content: "随便聊聊".into(),
            images: Vec::new(),
            audios: Vec::new(),
            videos: Vec::new(),
        });
        push(&mut log, answered(StopReason::Stop, false));
        apply(&mut log, &State { working: Working::Plan, ..Default::default() });
        assert!(!plan_delivered(&log), "那份回答不是规划模式交出来的");

        push(&mut log, Message::User {
            content: "给我一份改造方案".into(),
            images: Vec::new(),
            audios: Vec::new(),
            videos: Vec::new(),
        });
        push(&mut log, answered(StopReason::Stop, false));
        assert!(plan_delivered(&log), "切进来之后答完的那一份才算");
    }

    /// 没有那一行就是对话模式：这是今天所有旧话题的形状
    #[test]
    fn an_absent_row_is_chat() {
        let log = SessionLog::new();
        assert_eq!(in_effect(&log), State::default());
        assert_eq!(in_effect(&log).working, Working::Chat);
        assert!(!in_effect(&log).goal_active());
        assert!(
            section_body(&in_effect(&log)).is_none(),
            "对话模式不该产出任何一段"
        );
    }

    /// 后写胜出：换模式是在末尾追加一行，不是改写那一行
    #[test]
    fn the_last_row_written_wins() {
        let mut log = SessionLog::new();
        apply(
            &mut log,
            &State {
                working: Working::Plan,
                ..Default::default()
            },
        );
        assert_eq!(in_effect(&log).working, Working::Plan);
        assert_eq!(in_effect(&log).phase(), crate::policy::Phase::Plan);
        apply(&mut log, &State::default());
        assert_eq!(in_effect(&log).working, Working::Chat);
    }

    /// 坏掉的那一行读不懂，就当它不存在——而不是把用户静默关在写操作之外
    #[test]
    fn an_unreadable_row_is_chat_not_the_strictest_tier() {
        let mut log = SessionLog::new();
        log.append(
            NewEntry::new(EntryPayload::Custom {
                custom_type: ENTRY_TYPE.into(),
                data: Some(serde_json::json!({ "working": "yolo" })),
            }),
            T0,
        )
        .expect("追加该成功");
        assert_eq!(
            in_effect(&log).working,
            Working::Chat,
            "认不出的写法不许改成规划模式"
        );
    }

    /// 旧行（`outcome` + `paused` 两格）要折回一格 `status`。
    ///
    /// 这一条里最要紧的是**钱到顶那句要认成 `budget_limited`**：它从前与"模型说卡住"
    /// 共用 `blocked`，而两者的出路完全不同——一个是去调花费上限，一个是去改目标。
    /// 认不出的不猜，留在 `blocked`（猜成 `budget_limited` 会让人去动一个没到顶的上限）
    #[test]
    fn a_legacy_two_field_row_folds_into_one_status() {
        let read = |row: serde_json::Value| {
            let mut log = SessionLog::new();
            log.append(
                NewEntry::new(EntryPayload::Custom {
                    custom_type: ENTRY_TYPE.into(),
                    data: Some(row),
                }),
                T0,
            )
            .expect("追加该成功");
            in_effect(&log)
        };
        let legacy = |extra: serde_json::Value| {
            let mut base = serde_json::json!({
                "working": "goal",
                "objective": "把台账那三处对账补齐",
                "turns_used": 3,
                "max_cost_e8": 250_000_000_i64,
            });
            let patch = extra.as_object().expect("旧行补丁得是个对象");
            for (key, value) in patch {
                base[key] = value.clone();
            }
            read(base)
        };
        assert_eq!(
            legacy(serde_json::json!({ "outcome": "active", "paused": false })).status,
            Status::Active,
            "旧行的 active + 没旗子 = 推进中"
        );
        assert_eq!(
            legacy(serde_json::json!({ "outcome": "active", "paused": true })).status,
            Status::Paused,
            "旗子那一格升进 status：暂停从此是一个名字，不用两格互相解释"
        );
        assert_eq!(
            legacy(
                serde_json::json!({ "outcome": "blocked", "note": "这一支的花费到了上限（$2.50 / $2.50）。" })
            )
            .status,
            Status::BudgetLimited,
            "钱到顶那句要认出来：它的出路是调上限，不是改目标"
        );
        assert_eq!(
            legacy(serde_json::json!({ "outcome": "blocked", "note": "缺 Gitee 令牌" })).status,
            Status::Blocked,
            "模型说卡住还是卡住，不许被归成钱的问题"
        );
        assert_eq!(
            legacy(serde_json::json!({ "outcome": "complete", "note": "齐了" })).status,
            Status::Complete
        );
        // 新行只带 status：六格都读得回来
        assert_eq!(
            read(
                serde_json::json!({ "working": "goal", "objective": "x", "status": "usage_limited" })
            )
            .status,
            Status::UsageLimited
        );
        // 认不出的 status：整行读不懂，就按"没有那一行"办——不猜成推进中，
        // 那会让一支停着的目标自己接下去烧钱
        let junk = read(
            serde_json::json!({ "working": "goal", "objective": "x", "status": "nonsense" }),
        );
        assert!(
            !junk.goal_held() && junk.objective.is_none(),
            "读不懂的行不许变成一支挂着的目标：{junk:?}"
        );
    }

    /// 写出的一行**只带 `status`**：旧的两格是读侧为了认历史才留的，写侧再写一份
    /// 就是第二个真相——两处各记一次就要人猜该信哪一份
    #[test]
    fn a_written_row_carries_only_the_new_field() {
        let state = State {
            working: Working::Goal,
            objective: Some("把台账那三处对账补齐".into()),
            status: Status::Paused,
            ..Default::default()
        };
        let EntryPayload::Custom {
            data: Some(value), ..
        } = row(&state)
        else {
            panic!("模式那一行该是 Custom 条目");
        };
        let fields = value.as_object().expect("那一行是个对象");
        assert_eq!(fields["status"], "paused", "新格要写出去");
        assert!(
            !fields.contains_key("outcome"),
            "旧格 outcome 不许再写出：{fields:?}"
        );
        assert!(
            !fields.contains_key("paused"),
            "旧格 paused 旗子不许再写出：{fields:?}"
        );
    }

    /// 目标的字段要能整份往返：轮数、上限、点名的档案、身份、收尾那句都得读得回来
    #[test]
    fn the_goal_fields_round_trip() {
        let held = State {
            working: Working::Goal,
            objective: Some("把台账那三处对账补齐".into()),
            started_at: Some(T0),
            turns_used: 3,
            max_cost_e8: 250_000_000,
            status: Status::Active,
            note: None,
            profile: Some("档案甲".into()),
            goal_id: Some("goal-abc".into()),
            contract: None,
            suspended: None,
            outcome: None,
            paused: None,
        };
        let mut log = SessionLog::new();
        apply(&mut log, &held);
        let read = in_effect(&log);
        assert_eq!(read, held);
        assert!(read.goal_active());
        assert_eq!(read.profile.as_deref(), Some("档案甲"), "点名的档案要跟着整份往返");
        assert_eq!(read.goal_id.as_deref(), Some("goal-abc"), "身份要跟着整份往返");
        // 续跑轮数是读数不是配额：目标没有轮次上限
        assert_eq!(read.armed().turns_used, 4);
        assert_eq!(
            read.armed().profile, held.profile,
            "续跑只动轮数那一格，点名的档案不许被顺手抹掉"
        );
        assert_eq!(
            read.armed().goal_id, held.goal_id,
            "续跑只动轮数那一格，身份不许被顺手换掉——换了它，护栏与分叉分组就认不出这一支"
        );
    }

    /// `goal_id` 是后来才有的格：挂着目标却没有身份的旧行，读侧按**那一行的条目 id**
    /// 补铸一个。不伪造创建时间，只给分叉分组与 `edit` 一个认得出"同一支"的名字；
    /// 没有目标的行不补铸——身份是目标的，不是模式的
    #[test]
    fn a_legacy_goal_row_gets_an_identity_minted_from_its_entry() {
        let read_row = |data: serde_json::Value| {
            let mut log = SessionLog::new();
            log.append(
                NewEntry::new(EntryPayload::Custom {
                    custom_type: ENTRY_TYPE.into(),
                    data: Some(data),
                }),
                T0,
            )
            .expect("追加该成功");
            (in_effect(&log), log.path().expect("走路径该成功")[0].id.clone())
        };
        let (read, entry_id) = read_row(serde_json::json!({
            "working": "goal",
            "objective": "把台账那三处对账补齐",
            "turns_used": 2,
            "status": "active",
        }));
        assert_eq!(
            read.goal_id.as_deref(),
            Some(format!("legacy-{entry_id}").as_str()),
            "补铸的名字认得出是旧行，且钉在那一行上"
        );
        let (bare, _) = read_row(serde_json::json!({ "working": "chat" }));
        assert_eq!(bare.goal_id, None, "没有目标就不该有身份");
    }

    /// 目标是挂在**话题**上的，交互档只是当下怎么交互：切到对话档，目标字段留在主格
    /// 并且照样算"在推进"。这一条钉的是那次改造本身——旧行为是切档就把目标挪进挂起格，
    /// 于是"切去聊两句"等于把目标停了
    #[test]
    fn a_goal_keeps_running_after_the_interaction_tier_changes() {
        let goal = State {
            working: Working::Goal,
            objective: Some("把台账那三处对账补齐".into()),
            started_at: Some(T0),
            turns_used: 5,
            max_cost_e8: 250_000_000,
            status: Status::Active,
            profile: Some("档案甲".into()),
            ..Default::default()
        };
        let mut log = SessionLog::new();
        apply(&mut log, &goal);
        // 切到对话档：只改 `working` 那一格，目标原样留在主格
        apply(
            &mut log,
            &State {
                working: Working::Chat,
                ..goal.clone()
            },
        );
        let read = in_effect(&log);
        assert_eq!(read.working, Working::Chat);
        assert!(read.goal_active(), "切到对话档不该把目标停下");
        assert!(read.goal_active(), "也没人按暂停，它就该接着自己往下跑");
        assert_eq!(read.turns_used, 5, "轮数是账，切个档不能抹");
        assert_eq!(read.started_at, Some(T0), "起算点不能漂——漂了花费就重算");
        assert_eq!(read.profile.as_deref(), Some("档案甲"), "点名的档案跟着目标走");
        // 对话档下模型照样收到目标段，只是换成"用户就在旁边"那一份正文
        let body = section_body(&read).expect("挂着目标就该有一段");
        assert!(body.contains("把台账那三处对账补齐"), "目标本身得在段里：{body}");
        assert!(body.contains("用户的消息永远优先"), "对话档要说清谁优先：{body}");

        // 规划档是只读红线：目标在那里寸步难行，所以段里不出目标、只出规划那一段
        apply(
            &mut log,
            &State {
                working: Working::Plan,
                ..goal.clone()
            },
        );
        let planned = in_effect(&log);
        assert!(planned.goal_active(), "规划档也只是交互档，目标还在账上");
        let body = section_body(&planned).expect("规划模式该有一段");
        assert!(body.contains("只看不改"), "规划档出的是规划那一段：{body}");
        assert!(!body.contains("把台账那三处对账补齐"), "规划档不该催目标：{body}");
    }

    /// 旧日志里那批「挂起」行还得读得懂：展开回主格、归一成 `paused = true`。
    /// 挂起在当时就是"不在推进"，所以语义不变；而写出时不再出现这一格——
    /// 归一化只发生在内存，下游从此只见一套目标字段
    #[test]
    fn a_legacy_suspended_row_is_read_back_into_the_main_fields() {
        let mut log = SessionLog::new();
        apply(
            &mut log,
            &State {
                working: Working::Chat,
                suspended: Some(Suspended {
                    objective: Some("把台账那三处对账补齐".into()),
                    started_at: Some(T0),
                    turns_used: 5,
                    max_cost_e8: 250_000_000,
                    outcome: LegacyOutcome::Active,
                    note: None,
                }),
                ..Default::default()
            },
        );
        let read = in_effect(&log);
        assert_eq!(read.objective.as_deref(), Some("把台账那三处对账补齐"));
        assert_eq!(read.turns_used, 5, "轮数是账，归一化不能抹");
        assert_eq!(read.started_at, Some(T0), "起算点不能漂");
        assert_eq!(read.max_cost_e8, 250_000_000);
        assert!(read.paused(), "挂起归一成暂停：它当时就是「不在推进」");
        assert!(!read.goal_active(), "归一成暂停之后就不该自己往下跑");
        assert!(read.goal_held(), "但目标还在账上，按继续就能接回来");
        assert_eq!(read.suspended, None, "归一化之后主格之外不该还留一份");

        // 主格已经有目标时不覆盖：那一行说的是"目标在这儿"，挂起格是更早的残留
        let mut both = SessionLog::new();
        apply(
            &mut both,
            &State {
                working: Working::Goal,
                objective: Some("新的那件事".into()),
                turns_used: 2,
                suspended: Some(Suspended {
                    objective: Some("旧的那件事".into()),
                    turns_used: 9,
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        let read = in_effect(&both);
        assert_eq!(read.objective.as_deref(), Some("新的那件事"), "主格有目标时以主格为准");
        assert_eq!(read.turns_used, 2);
        assert!(!read.paused(), "主格那份没被暂停，就不该被残留的挂起格改成暂停");
    }

    /// 暂停是 Active 上的一格旗子：整份往返要读得回来，挂着它时目标不算在跑，
    /// 说给模型的正文得讲清"别再自己往下推"
    #[test]
    fn a_paused_goal_stops_promise_and_says_so() {
        let paused = State {
            working: Working::Goal,
            objective: Some("把台账那三处对账补齐".into()),
            goal_id: Some("goal-1".into()),

            status: Status::Paused,
            ..Default::default()
        };
        let mut log = SessionLog::new();
        apply(&mut log, &paused);
        let read = in_effect(&log);
        assert_eq!(read, paused, "暂停那一格要跟着整份往返");
        assert!(!read.goal_active(), "暂停中不该算在跑");
        let body = section_body(&read).expect("目标模式总该有一段");
        assert!(body.contains("暂停"), "正文得说出这个目标被暂停了：{body}");
        assert!(body.contains("不要"), "正文得拦住自动推进：{body}");
        assert!(!body.contains("这是第"), "暂停中不该催下一轮：{body}");
        assert!(
            body.contains("把台账那三处对账补齐"),
            "目标本身还得在段里：{body}"
        );
    }

    /// 规划段点名的工具，必须正好是执行侧放得过去的那几条。
    /// 这条针对的是真闸门：改 `rule` 里的判据或改这段正文，两边有一边先红
    #[test]
    fn the_plan_body_names_exactly_what_the_gate_lets_through() {
        use crate::policy::{Decision, Mode, Phase, Policy};
        use crate::tool_runtime::{rule, Call};

        // 真造一个项目根：classify 认路径，凭空给个不存在的路径会退化成"读不到"那一档
        let root: std::path::PathBuf = std::env::temp_dir().join(format!(
            "aglab-mode-plan-{}",
            std::time::UNIX_EPOCH
                .elapsed()
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(root.join("src")).expect("建临时根该成功");
        std::fs::write(root.join("src/a.rs"), "fn a() {}").expect("造文件该成功");
        let root = root.as_path();

        let names: [(&str, Value); 18] = [
            ("list_files", serde_json::json!({ "path": "src" })),
            ("read_file", serde_json::json!({ "path": "src/a.rs" })),
            (
                "search_text",
                serde_json::json!({ "query": "x", "path": "src" }),
            ),
            (
                "web_fetch",
                serde_json::json!({ "url": "https://example.com" }),
            ),
            ("load_skill", serde_json::json!({ "name": "pdf" })),
            ("list_windows", serde_json::json!({})),
            (
                "write_file",
                serde_json::json!({ "path": "src/a.rs", "content": "x" }),
            ),
            ("edit_file", serde_json::json!({ "path": "src/a.rs" })),
            (
                "run_command",
                serde_json::json!({ "command": "cargo test" }),
            ),
            ("open_path", serde_json::json!({ "path": "src/a.rs" })),
            ("command_output", serde_json::json!({ "handle": "h1" })),
            ("command_stop", serde_json::json!({ "handle": "h1" })),
            ("inspect_window", serde_json::json!({ "window": 1 })),
            ("computer_act", serde_json::json!({ "window": 1 })),
            ("spawn_subagent", serde_json::json!({ "name": "explore" })),
            (
                "browser",
                serde_json::json!({ "url": "https://example.com" }),
            ),
            ("knowledge_search", serde_json::json!({ "query": "x" })),
            ("mcp__notion__create_page", serde_json::json!({})),
        ];
        let policy = Policy::new(Mode::Full).with_phase(Phase::Plan);
        let verdict = |name: &str, args: &Value| {
            let via_mcp = name.starts_with("mcp__");
            let call = Call::new(name, args, Some(root), via_mcp);
            matches!(rule(&policy, &call, name, None).decision, Decision::Allow)
        };

        let mut allowed: Vec<&str> = Vec::new();
        for (name, args) in names.iter() {
            if verdict(name, args) {
                allowed.push(*name);
            }
        }
        assert_eq!(
            allowed,
            vec![
                "list_files",
                "read_file",
                "search_text",
                "web_fetch",
                "load_skill",
                "list_windows",
                "knowledge_search"
            ],
            "规划模式放开的就是这几条只读的路"
        );

        // 正文与闸门得互相交代：闸门放行的那条名字必须出现在正文里，
        // 被拦下的那条不许被正文说成可用
        let body = section_body(&State {
            working: Working::Plan,
            ..Default::default()
        })
        .expect("规划模式该有一段正文");
        for name in &allowed {
            assert!(
                body.contains(name),
                "闸门放过了 {name}，正文却没告诉模型它能用"
            );
        }
        for (name, args) in names.iter() {
            if !verdict(name, args) {
                assert!(!body.contains(name), "正文把 {name} 说成可用，而闸门拦着它");
            }
        }
        std::fs::remove_dir_all(root).ok();
    }

    /// 续跑那一行要自报"不是用户新问的"，并带上预算、第几轮、判据清单与规矩——
    /// 它是这一支自己写进日志的一行，冒充用户发言就是说谎。
    /// 会变的读数全在这一行：段里不再有"第几轮"和花费数字（洞 3 的收口）
    #[test]
    fn the_continuation_row_says_who_is_talking() {
        let state = State {
            working: Working::Goal,
            objective: Some("补齐三处对账".into()),
            turns_used: 2,
            contract: Some(crate::goal::contract::Contract {
                criteria: vec![
                    crate::goal::contract::Criterion {
                        id: "c1".into(),
                        text: "测试全绿".into(),
                        kind: crate::goal::contract::CriterionKind::Check { command: "npm test".into() },
                    },
                ],
                constraints: vec!["不改 src-tauri/**".into()],
            }),
            ..Default::default()
        };
        let row = continuation_row(&state, "已花 $0.42 / 上限 $2.50", "□ c1 测试全绿（跑命令：npm test）");
        assert!(
            row.contains("不是用户新问的"),
            "得说清这句话是谁说的：{row}"
        );
        assert!(row.contains("补齐三处对账"), "目标要在行里：{row}");
        assert!(row.contains("这是第 2 轮"), "第几轮要跟着走：{row}");
        assert!(row.contains("已花 $0.42"), "预算读数要跟着走：{row}");
        assert!(row.contains("□ c1"), "判据清单要每轮重锚：{row}");
        assert!(row.contains("不改 src-tauri/**"), "约束要跟着走：{row}");
        assert!(row.contains("没有轮次上限"), "别让它以为快到头了：{row}");
        assert!(row.contains("goal_report"), "收尾的出口要点名：{row}");
        assert!(row.contains("完成审计"), "审计的规矩要跟着走：{row}");
        // 没有契约的旧式目标：没有判据块也没有约束块，但不许缺了规矩
        let bare = State {
            working: Working::Goal,
            objective: Some("补齐三处对账".into()),
            ..Default::default()
        };
        let row = continuation_row(&bare, "无上限 · 已花 $0.00", "");
        assert!(!row.contains("判据（"), "没契约就不该有判据块：{row}");
        assert!(row.contains("完成审计"), "规矩还在：{row}");
    }

    /// 切回对话模式时模式段整个不在 current 里了，装配处要留下的是一行**撤销**而不是
    /// 悄悄少一行：模型得知道刚才那条规矩作废了，否则它还守着一条已经不在的红线
    #[test]
    fn leaving_a_mode_writes_a_revocation_row_with_the_old_marker() {
        use super::super::sections::{self, Section};

        let mut log = SessionLog::new();
        let plan = Section {
            name: sections::MODE,
            marker: "【作业模式】",
            body: section_body(&State {
                working: Working::Plan,
                ..Default::default()
            })
            .expect("规划模式该有正文"),
        };
        let first = {
            let path = log.path().expect("走路径该成功");
            sections::pending(&[plan], &sections::in_effect(&path))
        };
        assert_eq!(first.len(), 1, "进规划模式该写一行");
        for payload in first {
            log.append(NewEntry::new(payload), T0).expect("追加该成功");
        }

        let after = {
            let path = log.path().expect("走路径该成功");
            sections::pending(&[], &sections::in_effect(&path))
        };
        assert_eq!(after.len(), 1, "段没了也要留一行，不能悄悄少掉");
        for payload in after {
            log.append(NewEntry::new(payload), T0).expect("追加该成功");
        }

        let path = log.path().expect("走路径该成功");
        let row = sections::in_effect(&path)
            .get(sections::MODE)
            .copied()
            .expect("那一格还该读得到");
        assert!(sections::revoked(row), "切回对话要留下撤销行：{row}");
        assert!(row.starts_with("【作业模式】"), "撤销行得沿用原标记：{row}");
    }

    /// 刹车要说真话，而且两档都要说。这一句是给模型看的：它从前写着"用户按停止拦得住"，
    /// 而那次停止现在只掐当前这一轮——模型要是照旧以为一次中断就是用户让它停手，
    /// 它会半途交差然后在那儿等，那一支于是既不推进也不上报。所以双向钉：
    /// 三样真闸点名，一次中断不许被说成闸
    #[test]
    fn the_goal_body_names_the_real_brakes_and_not_the_stop_button() {
        let with_goal = |working: Working| State {
            working,
            objective: Some("把台账那三处对账补齐".into()),
            turns_used: 1,
            ..Default::default()
        };
        for (tier, state) in [("目标档", with_goal(Working::Goal)), ("对话档", with_goal(Working::Chat))] {
            let body = section_body(&state).expect("挂着目标就该有一段");
            assert!(body.contains("goal_report"), "{tier} 得给出上报那个出口：{body}");
            assert!(body.contains("花费"), "{tier} 得说出花费这道闸：{body}");
            assert!(body.contains("暂停"), "{tier} 得说出暂停这道闸：{body}");
            assert!(body.contains("结束"), "{tier} 得说出结束这道闸：{body}");
            assert!(
                body.contains("只掐当前这一轮"),
                "{tier} 得说清输入框上那次停止只管这一轮，否则模型会把一次中断读成收手指令：{body}"
            );
            // 反向那一半才是这条针的牙齿。从前它只钉"真话必须在"，于是段正文的包装句
            // 里那句"只有你上报、或者用户按停止，这一支才会停"照样全绿——GOAL_BODY
            // 自己带着那句真话，正向断言永远看不见邻居在说反话
            assert!(
                !body.contains("或者用户按停止"),
                "{tier} 不许把输入框那次停止说成整支目标的闸，它只掐这一轮：{body}"
            );
        }
    }

    /// 收尾的状态要说得出"不再自动续跑"，否则模型会以为自己还欠着一轮
    #[test]
    fn a_finished_goal_stops_promising_more_rounds() {
        let done = State {
            working: Working::Goal,
            objective: Some("x".into()),
            status: Status::Complete,
            note: Some("三条对账都补齐了".into()),
            ..Default::default()
        };
        let body = section_body(&done).expect("目标模式总该有一段");
        assert!(body.contains("不再自动续跑"));
        assert!(
            body.contains("三条对账都补齐了"),
            "结论要回到模型眼前：{}",
            body
        );
        assert!(!body.contains("没有轮次上限"), "收尾了就不该再催它往下跑：{body}");
        assert!(!done.goal_active());
    }

    /// 花费上限 0 是"不设上限"，不是"一块钱都不许花"——那句话现在住续跑行的预算行。
    /// 段里不再有数字（第几轮、花费都是每轮要变的读数，进段就是每轮一条差分行）
    #[test]
    fn an_uncapped_budget_says_so_instead_of_zero_dollars() {
        assert_eq!(budget_line(Some(0), 0), "无上限 · 已花 $0.00");
        assert_eq!(budget_line(Some(42_000_000), 0), "无上限 · 已花 $0.42");
        assert_eq!(
            budget_line(Some(40_000_000), 250_000_000),
            "已花 $0.40 / 上限 $2.50"
        );
        // 账读不出来照实说——那一格在判据那头是要停的，界面上也不许显示 $0.00
        assert!(budget_line(None, 250_000_000).contains("读不出来"));
        assert!(budget_line(None, 0).contains("无上限"));

        let running = State {
            working: Working::Goal,
            objective: Some("把台账那三处对账补齐".into()),
            turns_used: 3,
            max_cost_e8: 0,
            ..Default::default()
        };
        let body = section_body(&running).expect("目标模式该有一段正文");
        assert!(body.contains("没有自动刹车"), "不封顶要把这句话说给模型：{body}");
        assert!(body.contains("没有轮次上限"), "上限没了要讲明白：{body}");
        assert!(
            !body.contains("这是第"),
            "第几轮是读数，住续跑行不住段：{body}"
        );
        assert!(
            body.contains("把台账那三处对账补齐"),
            "目标本身得在段里：{body}"
        );
    }

    /// 段只说不变的规矩：挂着契约时，契约全文进段（它是静态的），
    /// 而第几轮与花费数字不许进段——那是洞 3 量到的每轮差分行的来源
    #[test]
    fn the_section_carries_the_contract_but_not_per_round_readings() {
        let held = State {
            working: Working::Goal,
            objective: Some("把台账那三处对账补齐".into()),
            turns_used: 7,
            status: Status::Active,
            contract: Some(crate::goal::contract::Contract {
                criteria: vec![
                    crate::goal::contract::Criterion {
                        id: "c1".into(),
                        text: "测试全绿".into(),
                        kind: crate::goal::contract::CriterionKind::Check { command: "npm test".into() },
                    },
                    crate::goal::contract::Criterion {
                        id: "c2".into(),
                        text: "文案说得清".into(),
                        kind: crate::goal::contract::CriterionKind::Judgment,
                    },
                ],
                constraints: vec!["不改 src-tauri/**".into()],
            }),
            ..Default::default()
        };
        let body = section_body(&held).expect("目标模式该有一段");
        assert!(body.contains("完成契约"), "契约要进段：{body}");
        assert!(body.contains("[c1] 测试全绿"), "判据要带 id 与文本：{body}");
        assert!(body.contains("npm test"), "命令要让模型知道：{body}");
        assert!(body.contains("要人看"), "两档判据要说得分得清：{body}");
        assert!(body.contains("不改 src-tauri/**"), "约束要进段：{body}");
        assert!(
            !body.contains("第 7") && !body.contains("已花"),
            "读数不许进段：{body}"
        );
        // **这就是差分行那根针**：只有轮数变时，段正文一个字都不变——
        // `sections::pending` 靠正文相等判"没变"，正文一变就是每轮一条差分行
        // （洞 3 实测：4 轮续跑 4 条差分行，别的段各 1 条）
        let next_round = State {
            turns_used: 8,
            ..held.clone()
        };
        assert_eq!(
            section_body(&held),
            section_body(&next_round),
            "第几轮不许影响段正文：\n{}\n---\n{}",
            section_body(&held).unwrap_or_default(),
            section_body(&next_round).unwrap_or_default()
        );
    }
}
