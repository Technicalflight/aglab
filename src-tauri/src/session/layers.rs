//! 层：读侧给投影贴的标签，加上按层的分配阶梯。
//!
//! **层是投影的属性，不是日志的属性**（§1.1.1）。`EntryPayload` 里没有 `Layer` 这个字段，
//! 写路径一个字节都不经过这里：一旦层参与写侧（按层分文件、按层挑该行不该发），
//! 日志就不再是唯一事实，§6.1 的段序与 §6.2 的定形随之失守。所以这个模块全是纯读函数，
//! 连一次 `append` 都不碰——`budget()` 换成一整套别的数字，日志的字节也一个字不动（§8.1）。
//!
//! 预算是**分配**问题不是阈值问题（§1.1.2）。现状只有一个闸：`context_tokens * 0.9`，
//! 它把"让步给谁"留成了散在装配处的隐式顺序（工具结果夹到 12 000+2 000、AGENTS.md 夹到
//! 8 000、超九成才压历史）。这里把那件事写成显式的两数一序：每层一个 `target`（该坐哪儿）
//! 与一个 `max`（越过它就欠一次让步），层的先后由 [`Concession::LADDER`] 决定。
//! 数字全部来自现状里已经在用的那几个常数，不发明新旋钮（§1.2：P0 先把顺序显式化）。

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use super::context::{latest_custom, project};
use super::entry::{EntryPayload, Message};
use super::log::SessionLog;
use super::prefix::Cause;
use super::sections::{self, MEMORY, MODE, PROJECT_CONTEXT, SKILLS};
use super::SessionError;

/// 工具声明那条 `custom` 条目的名字。定它的地方只该有一处：装配处写它，读侧认它
pub const DECLARATIONS_TYPE: &str = "tool_declarations";

/// 压缩之后历史至少要留下的那段原文（中文口径的字符数）。它原来是装配处选边界时自己的一个
/// 私有的数，现在它进了预算表——"历史这层的底线"本来就是分配的一部分，两处各写一份就是两个真相
pub const KEEP_RECENT_CHARS: usize = 20_000;

/// 一条话题在上下文里被切成的层。声明次序照 §6.1 的固定段序排，但**它不是写顺序**：
/// 差分行永远追加在日志末尾，实发数组里各层是穿插的。这里说的是"要发出去的东西有哪几类"
/// 这个集合，它是编译期事实（§1.2）
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    /// 常驻段：那条永不变更的默认提示词。它不在日志里，只有装配处能交进来
    Identity,
    /// 工具声明。定形之后每轮原样重发，不占 messages 数组
    Tools,
    /// 规则段：钩子补的约定、旧话题里留下的 system 行、认不出名字的段
    Rules,
    /// 工作目录约定（AGENTS.md / CLAUDE.md 那一档）
    Project,
    Skills,
    Memory,
    History,
    /// 本轮那一问本身（含拼进去的附件）。它之后的在途回答与工具结果仍算历史
    Turn,
}

impl Layer {
    /// 报告与统计的行序（§6.1 那几段在前，历史与本轮在后）
    pub const ORDER: [Layer; 8] = [
        Layer::Identity,
        Layer::Tools,
        Layer::Rules,
        Layer::Project,
        Layer::Skills,
        Layer::Memory,
        Layer::History,
        Layer::Turn,
    ];

    /// 这层在让步阶梯上有没有位置。没有的那些超预算时只报 Notice：
    /// 宁可这一轮发不出去，也不静默裁掉模型必须守着的东西（§2.3）
    pub fn protected(self) -> bool {
        !Concession::LADDER
            .iter()
            .any(|step| step.layer() == Some(self))
    }

    /// 分配时给这层留的底线。只有历史有一条，而它是从 `compaction_boundary` 的成立条件
    /// 里来的：保留窗低于这个量，那次压缩什么都没省
    fn floor(self) -> usize {
        match self {
            Layer::History => KEEP_RECENT_CHARS,
            _ => 0,
        }
    }
}

/// 一条条目按**形状**属于哪一层；不进上下文的类别没有层。
/// 位置相关的那一刀（本轮那一问及它之后都算 `Turn`）在 [`uses`] 里补，不在这里猜
pub fn classify(payload: &EntryPayload) -> Option<Layer> {
    match payload {
        EntryPayload::Custom { custom_type, .. } => {
            if custom_type == DECLARATIONS_TYPE {
                Some(Layer::Tools)
            } else {
                None
            }
        }
        EntryPayload::CustomMessage { .. } => Some(match sections::section_of(payload) {
            Some(PROJECT_CONTEXT) => Layer::Project,
            Some(SKILLS) => Layer::Skills,
            Some(MEMORY) => Layer::Memory,
            // 作业模式那一段是规则而不是历史：它说"这一支现在准不准动手"，
            // 被让步阶梯裁掉就等于模型守着一条它看不见的红线，所以它不站在阶梯上
            Some(MODE) => Layer::Rules,
            // 认不出名字的段一律按规则段待。层的集合是编译期事实，留一个"没人认领、
            // 于是谁都能裁"的层就是给静默截断开门
            _ => Layer::Rules,
        }),
        EntryPayload::Message { message } => Some(match message {
            Message::System { .. } => Layer::Rules,
            Message::User { .. } | Message::Assistant(..) | Message::Tool { .. } => Layer::History,
        }),
        // 摘要行是历史唯一的替代形状
        EntryPayload::Compaction { .. } | EntryPayload::BranchSummary { .. } => {
            Some(Layer::History)
        }
        // 这几类零贡献，谈不上占哪一层
        EntryPayload::ContextEdit { .. }
        | EntryPayload::ModelChange { .. }
        | EntryPayload::Usage { .. }
        | EntryPayload::SessionInfo { .. } => None,
    }
}

/// 一层在这一轮里实际占的量。`chars` 是**实测**，口径是发出去的那串 JSON 的长度而不是正文
/// 长度：§4.4 要"重算真实字符数"，能跟实发数组逐字节核对的只有前者
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LayerUse {
    pub layer: Layer,
    /// 这一层由几条条目构成（常驻段不在日志里，所以它是 0）
    pub entries: usize,
    /// 发出几行。工具声明那层恒 0：它在请求体的 `tools` 字段里，不在 messages 数组里
    pub rows: usize,
    pub chars: usize,
}

/// 这一轮发出去的东西按层统计。`head` 是常驻段——它是唯一不在日志里的那批字节，
/// 因此只能由装配处交进来；这里没有任何一层是"照配置猜出来的"
pub fn uses(log: &SessionLog, head: &[Value]) -> Result<Vec<LayerUse>, SessionError> {
    let mut table: BTreeMap<Layer, LayerUse> = BTreeMap::new();
    for (_id, layer, rows, chars) in shaped(log)? {
        let row = table.entry(layer).or_insert_with(|| zero(layer));
        row.entries += 1;
        row.rows += rows;
        row.chars += chars;
    }

    table.insert(
        Layer::Identity,
        LayerUse {
            layer: Layer::Identity,
            entries: 0,
            rows: head.len(),
            chars: wire_chars(head),
        },
    );

    // 声明数组从定形那条读回来，跟装配处发的是同一批字节：压缩边界截不掉它
    if let Some(data) = latest_custom(log, DECLARATIONS_TYPE)? {
        let row = table
            .entry(Layer::Tools)
            .or_insert_with(|| zero(Layer::Tools));
        row.entries = 1;
        row.chars = chars_of(data);
    }

    Ok(Layer::ORDER
        .iter()
        .map(|layer| table.get(layer).copied().unwrap_or_else(|| zero(*layer)))
        .collect())
}

/// 本轮的估算：各层实测加起来。它明写自己是字符口径（§8.6）——发送那一路可以交一个
/// `Calibrated` 的进来，但那要拿服务商真报过的数当基线，是 P2 的事
pub fn estimate(table: &[LayerUse]) -> Estimate {
    Estimate {
        chars: table.iter().map(|row| row.chars).sum(),
        kind: EstimateKind::Chars,
    }
}

/// 把"这一轮打算新塞进去、还没写进日志的那几段"折进它们各自所属的层。
///
/// 折的是**层**而不是总量，是为了让装配前的预检和发出去之后的判定走同一条路：
/// `owes(Memory)` 问的仍然是"记忆段越过自己的硬顶没有"。换成"总字符数过没过某条线"
/// 就是第二个真相，也正是 §1.1.2 那条"分配不是阈值"要防的事。
/// 行数与条目数不动——那两格是实测读数，预检阶段没有资格替它们先记账
pub fn fold_pending(table: &[LayerUse], pending: &[(Layer, usize)]) -> Vec<LayerUse> {
    let mut folded = table.to_vec();
    for (layer, chars) in pending {
        if let Some(row) = folded.iter_mut().find(|row| row.layer == *layer) {
            row.chars += chars;
        }
    }
    folded
}

/// 这一轮的量是照哪个口径算出来的。凡估算都在类型上带这个位，界面上如实标"估算"（§8.6）
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EstimateKind {
    /// 本地按字符估的。中文口径下它跟 token 同一把尺，偏差没有实测上界
    Chars,
    /// 以服务商真报过的 `prompt_tokens` 为基线，只给本轮新增的部分补字符
    Calibrated,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct Estimate {
    pub chars: usize,
    pub kind: EstimateKind,
}

/// 预算的两个输入，都是配置里本来就写着的数：窗口，和这轮打算留给输出的那截。
/// 后者就是现状那个 `* 0.9` 想说的事——拿一个写死的比例去猜一个配置里明写着的数。
///
/// 那两个数都是 **token**，而这张表里每一格量的都是**字符**，所以中间必须有一把换算的尺
/// （`chars_per_token`）。它此前被静默地当成 1.0：对中文差不多，对英文与代码就是差三四倍——
/// 于是窗口只用到四分之一就开始压历史（§15）
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BudgetInput {
    pub window: usize,
    /// 这轮打算留给输出的那截。超过半个窗口的部分不参与扣减：[`budget`] 对输入侧有保底
    pub output_reserve: usize,
    /// 一个 token 折多少字符。`1.0` = 没有实测系数时的口径，与换算落地之前逐字符相同
    pub chars_per_token: f64,
}

impl BudgetInput {
    /// 那把尺还没量出来的时候用这个：1 字符 = 1 token，也就是这一格落地前的行为
    pub fn uncalibrated(window: usize, output_reserve: usize) -> Self {
        Self { window, output_reserve, chars_per_token: 1.0 }
    }
}

/// 压缩阈值的输出预留带。max_tokens 未填（0）或填得太小都按 21K 保底——
/// 阈值要先给"接下来那发回答"留出真实需要的空间，0 预留会让压缩闸门顶到
/// 整窗才开，一发像样的回答就把窗口撞穿。填得比 32K 还大也只按 32K 计：
/// 多出来的部分由装配处的剩余空间钳制兜底（真顶满了它会先砍输出，不是硬撞 400）
pub const OUTPUT_RESERVE_FLOOR: usize = 21_000;
pub const OUTPUT_RESERVE_CAP: usize = 32_000;

pub fn output_reserve(configured: u32) -> usize {
    (configured as usize).clamp(OUTPUT_RESERVE_FLOOR, OUTPUT_RESERVE_CAP)
}

/// 一层的分配结果。`chars` 是实测，`max` 是硬顶（越过就欠让步），`target` 是
/// "后面的层都保住底线时，这层该坐的位置"。不让步的层两个数相等，那就是"这层没有弹性"的写法
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LayerBudget {
    pub layer: Layer,
    pub chars: usize,
    pub target: usize,
    pub max: usize,
}

/// 一张预算表。`limit` 是输入侧的天花板，`deficit` 是**不让步的那几层**装不进窗口的部分——
/// 它大于 0 时唯一正确的动作是 Notice，不是裁掉谁（§2.3）
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Budget {
    pub limit: usize,
    pub rows: Vec<LayerBudget>,
    pub deficit: usize,
}

/// 本轮各层的 target / max。纯函数：同一份用量加同一份输入永远出同一张表（§5 T01）
pub fn budget(table: &[LayerUse], input: BudgetInput) -> Budget {
    // 天花板是 token 数乘以那把尺，量出来才是这张表用的字符数。`chars_per_token = 1.0`
    // 时这一行与换算落地之前逐字符相同（§15）
    //
    // 预留不许吃掉整个窗口：max_tokens 配得比窗口还大（设置面板允许填到 1_000_000 这类）
    // 时 `saturating_sub` 饱和成 0，历史层配额随之归零，压缩闸门就无视实际用量每轮开闸
    // ——上下文才 1.4% 也照压就是这么来的。输入侧因此保底半个窗口；真发出去的输出上限
    // 另有装配处的钳制按剩余空间收口，这里只管把表算对
    let spare = input
        .window
        .saturating_sub(input.output_reserve)
        .max(input.window / 2);
    let limit = (spare as f64 * input.chars_per_token).floor() as usize;
    let order = allocation_order();
    let mut maxes = [0usize; 8];
    let mut taken = [0usize; 8];
    let mut chars = [0usize; 8];

    // 瀑布：按让位次序的反向走，越晚让步的越早拿位子。历史永远在最后——它拿的是别人挑剩的
    let mut room = limit;
    for (at, layer) in order.iter().enumerate() {
        chars[at] = use_of(table, *layer);
        maxes[at] = room;
        taken[at] = chars[at].min(room);
        room -= taken[at];
    }

    let mut rows: Vec<LayerBudget> = Vec::with_capacity(order.len());
    let mut deficit = 0usize;
    for (at, layer) in order.iter().enumerate() {
        // 后面那些层各自要保住的下限之和：这一层的 target 就是把它扣掉之后还能坐下的位置
        let need_after: usize = order[at + 1..].iter().map(|later| later.floor()).sum();
        let reserved = need_after.min(maxes[at]);
        let target = if layer.protected() {
            taken[at]
        } else {
            chars[at].min(maxes[at] - reserved)
        };
        if layer.protected() {
            deficit += chars[at].saturating_sub(maxes[at]);
        }
        rows.push(LayerBudget {
            layer: *layer,
            chars: chars[at],
            // 不让步的层没有"能借多少"这件事：它占多少就是多少，两个数相等就是"没有弹性"的写法
            max: if layer.protected() { target } else { maxes[at] },
            target,
        });
    }
    Budget {
        limit,
        rows: by_wire_order(&rows),
        deficit,
    }
}

/// 分配次序 = 让位次序的反向。不让步的几层按段序排在最前，其余按阶梯从贵到便宜
fn allocation_order() -> Vec<Layer> {
    let mut order: Vec<Layer> = Layer::ORDER
        .iter()
        .copied()
        .filter(|layer| layer.protected())
        .collect();
    let mut ladder: Vec<Layer> = Concession::LADDER
        .iter()
        .copied()
        .filter_map(Concession::layer)
        .collect();
    ladder.reverse();
    ladder.dedup();
    order.extend(ladder);
    order
}

/// 表按 §6.1 的段序重排：分配要按让位次序算，报告要按发出去的形状读
fn by_wire_order(rows: &[LayerBudget]) -> Vec<LayerBudget> {
    Layer::ORDER
        .iter()
        .filter_map(|layer| rows.iter().find(|row| row.layer == *layer).copied())
        .collect()
}

impl Budget {
    pub fn row(&self, layer: Layer) -> Option<&LayerBudget> {
        self.rows.iter().find(|row| row.layer == layer)
    }

    /// 这层是不是已经越过硬顶
    fn owes(&self, layer: Layer) -> bool {
        self.row(layer).is_some_and(|row| row.chars > row.max)
    }
}

/// 一段可以被按层压缩顶替掉的历史：条目 id 与它本来要占的字节（口径同 [`LayerUse`]，
/// 是发出去那串 JSON 的长度，不是正文长度）
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryRow {
    pub id: String,
    pub chars: usize,
}

/// 按层压缩的那一步要顶替哪一段。
///
/// `chars` 是**被顶替掉**的那些行的字节量；摘要自己占多少不在这里猜——它得等模型答完
/// 再重算一次真实字符数（§4.4：不许拿估算的差值当结果）
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactPlan {
    pub from_id: String,
    pub through_id: String,
    pub rows: usize,
    pub chars: usize,
    /// 顶替到只剩最后一行都还落不到 target 以下。这时候正确的动作不是继续压，
    /// 而是让阶梯往下一步走（`DropMemorySection` / `TrimSkills`），或者干脆报 Notice
    pub still_over: bool,
}

/// 挑出最老的那一段交给摘要：从最旧一行开始累计，直到剩下的量能坐到 `target` 以下。
///
/// 三条底线，都是为了不让"压缩"变成"失忆"：
/// - **至少顶替 2 行**：为换一行摘要压掉一行历史省不出任何东西，还多付一次请求。
/// - **至少留 1 行**：最新那一行是模型接着往下说的根据，裁不得（同 [`Layer::Turn`]）。
/// - **不足 3 行一律不动**：那是上面两条的合取，写出来是为了让调用方拿到 `None`
///   而不是半个计划
pub fn layer_compaction(rows: &[HistoryRow], target: usize) -> Option<CompactPlan> {
    if rows.len() < 3 {
        return None;
    }
    let total: usize = rows.iter().map(|row| row.chars).sum();
    if total <= target {
        return None;
    }

    // `cut < rows.len() - 1` 就是"至少留一行"那条底线
    let mut cut = 0usize;
    let mut removed = 0usize;
    while cut < rows.len() - 1 && total - removed > target {
        removed += rows[cut].chars;
        cut += 1;
    }
    if cut < 2 {
        return None;
    }
    Some(CompactPlan {
        from_id: rows[0].id.clone(),
        through_id: rows[cut - 1].id.clone(),
        rows: cut,
        chars: removed,
        still_over: total - removed > target,
    })
}

/// 投影里每一条贡献给上下文的条目：id、它属于哪一层、发出几行、多少字节
type Shaped = (String, Layer, usize, usize);

/// 按发出的次序给每一条条目贴上层标签。
///
/// [`uses`] 与 [`history_rows`] 都从这一处读数——分层规则只许有一份。两处各判一次的话，
/// "这一层的字节"和"要压的那几行"就不是同一批东西了，那正是压错地方的开始
fn shaped(log: &SessionLog) -> Result<Vec<Shaped>, SessionError> {
    let projection = project(log)?;
    let mut rows: Vec<(String, Layer, usize, usize, bool)> = Vec::new();
    for (id, messages) in projection.rows() {
        let payload = log.entry(id).expect("投影里的条目 id 全是从路径上取的").payload();
        // 不进上下文的类别连层都没有，也就不参与分层统计（Usage、SessionInfo 那几类）
        let Some(layer) = classify(payload) else {
            continue;
        };
        let chars: usize = messages.iter().map(Message::wire_chars).sum();
        rows.push((
            id.to_string(),
            layer,
            messages.len(),
            chars,
            matches!(payload, EntryPayload::Message { message: Message::User { .. } }),
        ));
    }
    // 只有"本轮那一问"那一条算 `Turn`：它是裁不得的那一句。它之后落进来的在途回答与
    // 工具结果仍然是历史——工具轮跑到第十步时，它们得能被压
    if let Some(at) = rows
        .iter()
        .rposition(|(_, _, message_rows, _, is_user)| *message_rows > 0 && *is_user)
    {
        rows[at].1 = Layer::Turn;
    }
    Ok(rows
        .into_iter()
        .map(|(id, layer, message_rows, chars, _)| (id, layer, message_rows, chars))
        .collect())
}

/// 历史层那几行的 id 与字节量，按发出去的次序。按层压缩的计划只读它。
///
/// 行号翻回条目 id 这件事只许有一处会做：`chat.rs` 的压缩边界也靠这张表，两处各翻一遍
/// 就会在边界上错位一格（那是这个模块最贵的一种错——压错了地方，省下的钱是假的）
pub fn history_rows(log: &SessionLog) -> Result<Vec<HistoryRow>, SessionError> {
    Ok(shaped(log)?
        .into_iter()
        .filter(|(_, layer, _, _)| *layer == Layer::History)
        .map(|(id, _, _, chars)| HistoryRow { id, chars })
        .collect())
}

/// 一次让步。`LADDER` 的顺序是协议：它按"每换回一个字符要赔掉多少信息"排，
/// 调换它就等于把"压掉整段旧对话"排在"夹一条超大的工具结果"前面
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Concession {
    /// 什么都不欠
    None,
    /// 夹单条工具结果（已有：12 000 + 2 000）。最便宜：它发生在该行**进日志之前**，
    /// 所以一个已发出去的字节都不动
    ClipToolResult,
    /// 移压缩边界。要一次摘要请求，而且是一次授权的前缀断开
    CompactHistory,
    /// 本轮检索到的记忆段整段不发（常驻段照发）。信息是彻底没了，不是换成摘要
    DropMemorySection,
    /// 技能清单让步。模型从此不知道怎么调用它唯一能调的那个入口，所以排在最后
    TrimSkills,
}

impl Concession {
    /// 从便宜到贵（§4.3）。`Identity` / `Rules` / `Project` / `Turn` 不在这张表上
    pub const LADDER: [Concession; 4] = [
        Concession::ClipToolResult,
        Concession::CompactHistory,
        Concession::DropMemorySection,
        Concession::TrimSkills,
    ];

    /// 这一步由哪层付账
    pub fn layer(self) -> Option<Layer> {
        match self {
            Self::None => None,
            Self::ClipToolResult | Self::CompactHistory => Some(Layer::History),
            Self::DropMemorySection => Some(Layer::Memory),
            Self::TrimSkills => Some(Layer::Skills),
        }
    }

    /// 这一步谁在执行；`None` = 程序不会自动做它，只能人来减。
    ///
    /// 它必须是数据而不是散在各处的 if：面板那句"让步：… → 裁技能"要是没有这一格，
    /// 就是在替一个不会发生的动作说话。判据只有一份，界面读的是它的派生
    pub fn auto_actor(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            // 夹在写侧：那一行进日志之前就已经是夹过的了，所以阶梯上它从不需要谁再去执行
            Self::ClipToolResult => Some("写侧：工具结果进日志前已夹过"),
            Self::CompactHistory => Some("发送前的自动压缩闸门"),
            Self::DropMemorySection => Some("装配前不注入记忆段"),
            // 技能清单是模型知道自己唯一能调的那个入口的地方，程序替用户裁它是另一种静默
            Self::TrimSkills => None,
        }
    }
}

/// 这一轮为什么让步、以及让步之后怎么办。`reason` 回答的是"这批字节跟上一批不一样，是谁批准的"
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum BreakReason {
    Fits,
    /// 装不下了。`limit` 是输入侧的天花板：报 Notice 的那句话要能说出跟谁比
    OverBudget {
        limit: usize,
    },
    /// 断开是授权过的：压缩、编辑、回溯。除此之外任何断开都是缺陷
    Authorized {
        cause: Cause,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub estimate: Estimate,
    /// 欠的让步，从便宜到贵。什么也不欠时它是 `[None]` 而不是空表：
    /// 界面上"本轮没有让步"那一行要有个东西可指
    pub ladder: Vec<Concession>,
    pub reason: BreakReason,
}

/// 给定估算与预算，产出让步阶梯。不落盘、不写日志，也不改任何一个字节（§1.4）
///
/// `reset` 是装配处已经知道的授权断开（本轮刚压过 / 刚编辑过 / 刚回溯过）。
/// 判定次序是**超预算优先**：两者同时成立时，用户要看懂的是"还差多少、谁得让"，
/// 而那次断开本来就是阶梯上的某一步买来的，不需要再报一次
pub fn plan(estimate: Estimate, budget: &Budget, reset: Option<Cause>) -> Plan {
    let ladder: Vec<Concession> = Concession::LADDER
        .iter()
        .copied()
        .filter(|step| step.layer().is_some_and(|layer| budget.owes(layer)))
        .collect();
    let over = budget.deficit > 0 || estimate.chars > budget.limit;
    let reason = if over {
        BreakReason::OverBudget {
            limit: budget.limit,
        }
    } else if let Some(cause) = reset {
        BreakReason::Authorized { cause }
    } else {
        BreakReason::Fits
    };
    Plan {
        estimate,
        ladder: if ladder.is_empty() {
            vec![Concession::None]
        } else {
            ladder
        },
        reason,
    }
}

/// 整批要发出去的字节量（常驻段 ++ 日志投影）。压缩条目那个 `tokens_before` 用它：
/// 两处写同一个字段就必须是同一个算法，一个口径一份账（T04）
pub fn thread_chars(head: &[Value], history: &[Value]) -> u32 {
    (wire_chars(head) + wire_chars(history)).min(u32::MAX as usize) as u32
}

pub fn chars_of(value: &Value) -> usize {
    serde_json::to_string(value)
        .map(|text| text.chars().count())
        .unwrap_or(0)
}

/// 一批行的字节量。Inspector 的每一个字符数都从这里出，包括核对用的那一个
pub fn wire_chars(rows: &[Value]) -> usize {
    rows.iter().map(chars_of).sum()
}

fn use_of(table: &[LayerUse], layer: Layer) -> usize {
    table
        .iter()
        .find(|row| row.layer == layer)
        .map_or(0, |row| row.chars)
}

fn zero(layer: Layer) -> LayerUse {
    LayerUse {
        layer,
        entries: 0,
        rows: 0,
        chars: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::super::entry::{NewEntry, PendingAssistant, StopReason, ToolCall, UsageRecord};
    use super::super::log::SessionLog;
    use super::super::prefix::sent_array;
    use super::super::sections::Section;
    use super::*;
    use serde_json::json;

    const T0: i64 = 1_700_000_000_000;
    const MARK_WORKSPACE: &str = "【工作目录约定】本条是当前生效的工作目录约定。";
    const MARK_SKILLS: &str = "【技能清单】本条是当前可用的技能清单。";
    const MARK_MEMORY: &str = "【本地记忆】本条是这台机器上存着的长期记忆。";

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

    fn tool(text: &str) -> EntryPayload {
        EntryPayload::Message {
            message: Message::Tool {
                tool_call_id: "call_1".into(),
                content: text.into(),
            },
        }
    }

    /// 段行一律走写侧那条唯一的路径落成条目（`pending` + `Section::row`），
    /// 这样层认的是实际发出去的那个串，不是测试另拼一遍的近似物
    fn write_sections(log: &mut SessionLog, current: &[Section]) {
        let delta = {
            let path = log.path().expect("走路径该成功");
            sections::pending(current, &sections::in_effect(&path))
        };
        for payload in delta {
            push(log, payload);
        }
    }

    fn workspace(body: &str) -> Section {
        Section {
            name: PROJECT_CONTEXT,
            marker: MARK_WORKSPACE,
            body: body.into(),
        }
    }

    fn skills(body: &str) -> Section {
        Section {
            name: SKILLS,
            marker: MARK_SKILLS,
            body: body.into(),
        }
    }

    fn memory(body: &str) -> Section {
        Section {
            name: MEMORY,
            marker: MARK_MEMORY,
            body: body.into(),
        }
    }

    fn head() -> Vec<Value> {
        vec![json!({ "role": "system", "content": "你是 aglab" })]
    }

    fn row_of(table: &[LayerUse], layer: Layer) -> LayerUse {
        table
            .iter()
            .find(|row| row.layer == layer)
            .copied()
            .unwrap_or_else(|| panic!("{layer:?} 那行必须在表里：层的集合是编译期事实"))
    }

    fn chars_of_layer(table: &[LayerUse], layer: Layer) -> usize {
        row_of(table, layer).chars
    }

    fn budget_of(table: &[LayerUse], window: usize, reserve: usize) -> Budget {
        budget(
            table,
            BudgetInput {
                window,
                output_reserve: reserve,
                chars_per_token: 1.0,
            },
        )
    }

    /// 窗口是 token、表里每一格量的是字符——换算必须发生在分配**之前**（§15）。
    /// 同一批字，接上实测尺子之后就不该再欠让步：那正是"英文与代码为主的对话
    /// 在全窗口四分之一处就开始压历史"那个坏法的反面
    #[test]
    fn a_calibrated_window_buys_the_chars_it_really_holds() {
        let table = vec![
            LayerUse {
                layer: Layer::Identity,
                entries: 1,
                rows: 1,
                chars: 2_000,
            },
            LayerUse {
                layer: Layer::History,
                entries: 8,
                rows: 8,
                chars: 90_000,
            },
        ];

        let flat = budget(&table, BudgetInput::uncalibrated(60_000, 0));
        assert_eq!(
            flat.limit, 60_000,
            "没有实测系数时逐字符复现换算落地之前的天花板"
        );
        assert!(
            plan(estimate(&table), &flat, None)
                .ladder
                .contains(&Concession::CompactHistory),
            "正对照：按 1 字符 = 1 token 这把尺，这 9 万字符确实挤，让步表点名要历史付账"
        );

        let latin = budget(
            &table,
            BudgetInput {
                window: 60_000,
                output_reserve: 0,
                chars_per_token: 3.0,
            },
        );
        assert_eq!(latin.limit, 180_000, "天花板要按实测那把尺折成字符");
        assert!(
            !plan(estimate(&table), &latin, None)
                .ladder
                .contains(&Concession::CompactHistory),
            "折算之后还空着 8 万多个字符的位置，就不该有人为一次摘要请求付账"
        );
    }

    /// 输出预留比窗口还大时（设置面板允许填到 1_000_000 这类），`window - reserve`
    /// 饱和成 0：旧实现里历史层配额归零，压缩闸门无视实际用量每轮开闸——
    /// 上下文才 1.4% 也照压、以及"可压缩的对话太少"连着报，都是它
    #[test]
    fn an_output_reserve_past_the_window_still_leaves_half_for_input() {
        let flooded = budget(&[], BudgetInput::uncalibrated(128_000, 1_000_000));
        assert_eq!(
            flooded.limit, 64_000,
            "预留吃满窗口时输入侧保底半个窗口，而不是归零"
        );

        let ordinary = budget(&[], BudgetInput::uncalibrated(128_000, 32_000));
        assert_eq!(ordinary.limit, 96_000, "预留没过半时天花板照旧，保底不越权");
    }

    /// 预检折的是**层**，不是一个新的总量：折完之后所有判定仍然由同一张预算表回答
    #[test]
    fn a_pending_section_folds_into_its_own_layer_and_not_into_a_total() {
        let mut log = SessionLog::new();
        write_sections(
            &mut log,
            &[workspace("构建：cargo test"), skills("pdf / xlsx")],
        );
        push(&mut log, user("第一问"));
        let table = uses(&log, &head()).expect("统计该成功");

        let folded = fold_pending(&table, &[(Layer::Memory, 4_000)]);

        assert_eq!(
            chars_of_layer(&folded, Layer::Memory) - chars_of_layer(&table, Layer::Memory),
            4_000
        );
        assert_eq!(
            chars_of_layer(&folded, Layer::History),
            chars_of_layer(&table, Layer::History),
            "别的层一个字都不该动"
        );
        assert_eq!(estimate(&folded).chars - estimate(&table).chars, 4_000);
        let row = row_of(&folded, Layer::Memory);
        assert_eq!(
            (row.entries, row.rows),
            (0, 0),
            "还没写进日志的东西不该被记成日志里的行数与条目数"
        );
    }

    /// 阶梯上每一步都得说清谁在执行——面板那句"本轮要让步"靠这一格才不是空话
    #[test]
    fn each_ladder_step_says_who_runs_it() {
        assert!(Concession::ClipToolResult.auto_actor().is_some());
        assert!(Concession::CompactHistory.auto_actor().is_some());
        assert!(Concession::DropMemorySection.auto_actor().is_some());
        // 裁技能留给人：它是模型认得自己唯一入口的地方，程序替用户裁它是另一种静默
        assert_eq!(Concession::TrimSkills.auto_actor(), None);
        assert_eq!(Concession::None.auto_actor(), None);
    }

    /// 记忆段挤不进去时阶梯必须点到那一步：预检的判据只有这一条，装配处不许自己另算
    #[test]
    fn a_memory_section_that_does_not_fit_is_the_step_that_owes() {
        let mut log = SessionLog::new();
        write_sections(
            &mut log,
            &[workspace("构建：cargo test"), skills("pdf / xlsx")],
        );
        push(&mut log, user("这一问"));
        let table = fold_pending(
            &uses(&log, &head()).expect("统计该成功"),
            &[(Layer::Memory, 20_000), (Layer::Turn, 40)],
        );

        let tight = budget_of(&table, 6_000, 1_000);
        assert_eq!(tight.deficit, 0, "不让步的那几层装得下，才轮到记忆段付账");
        let preflight = plan(estimate(&table), &tight, None);
        assert!(
            preflight.ladder.contains(&Concession::DropMemorySection),
            "阶梯：{:?}",
            preflight.ladder
        );

        // 窗口松一档就该留着它——否则上面那句只是在测"永远为真"
        let roomy = budget_of(&table, 60_000, 1_000);
        assert!(
            !plan(estimate(&table), &roomy, None)
                .ladder
                .contains(&Concession::DropMemorySection),
            "装得下也要摘记忆段，那就是白丢上下文"
        );
    }

    /// T01 的主判据：给同一份 log，层归属与 `sent_array` 逐行对得上——
    /// 各层字符数加起来必须等于实发数组的实测字节，多一个少一个都不行
    #[test]
    fn layer_chars_add_up_to_the_bytes_actually_sent_row_by_row() {
        let mut log = SessionLog::new();
        write_sections(
            &mut log,
            &[
                workspace("构建：cargo test"),
                skills("pdf / xlsx"),
                memory("用户住在杭州"),
            ],
        );
        push(&mut log, user("第一问"));
        push(&mut log, assistant("答复一"));
        push(&mut log, user("第二问"));

        let table = uses(&log, &head()).expect("贴标签该成功");
        let sent = sent_array(&log).expect("投影该成功");
        let measured = wire_chars(&head()) + wire_chars(&sent);
        assert_eq!(
            table.iter().map(|row| row.chars).sum::<usize>(),
            measured,
            "分层加起来跟实发的不是一批字节：{table:?} / {sent:?}"
        );
        assert_eq!(estimate(&table).chars, measured);
        assert_eq!(row_of(&table, Layer::Turn).rows, 1, "只有最后一问算本轮");
        assert_eq!(row_of(&table, Layer::History).rows, 2);
        assert_eq!(row_of(&table, Layer::Project).entries, 1);
    }

    /// T01 的变异对照：记忆段错贴成历史，这一条必须红。它同时钉住"本轮那一问"和
    /// "它之前的历史"是两个层——并成一层，面板就再也答不出"这轮新输入占多少"
    #[test]
    fn each_named_section_lands_on_its_own_layer_not_on_history() {
        let mut log = SessionLog::new();
        write_sections(
            &mut log,
            &[
                workspace("约定"),
                skills("pdf"),
                memory("记忆正文占好长的一段"),
            ],
        );
        push(&mut log, user("一问"));
        let table = uses(&log, &head()).expect("贴标签该成功");

        for layer in [Layer::Project, Layer::Skills, Layer::Memory] {
            let row = row_of(&table, layer);
            assert_eq!(row.entries, 1, "{layer:?} 那段必须自己站一行");
            assert_eq!(row.rows, 1);
            assert!(row.chars > 0);
        }
        assert_eq!(
            chars_of_layer(&table, Layer::History),
            0,
            "段是段、历史是历史：混在一层就再也说不出谁占了位子"
        );
        assert_eq!(row_of(&table, Layer::Turn).entries, 1);
        assert_eq!(chars_of_layer(&table, Layer::Rules), 0);
    }

    /// 认不出名字的段按规则段待，绝不落到"没人认领、于是谁都能裁"的敞口上
    #[test]
    fn an_unnamed_section_row_is_protected_as_a_rule_row() {
        let mut log = SessionLog::new();
        push(
            &mut log,
            EntryPayload::CustomMessage {
                custom_type: format!("{}from_a_newer_build", sections::ENTRY_PREFIX),
                content: "【新构建的段】正文".into(),
                display: false,
            },
        );
        let table = uses(&log, &head()).expect("贴标签该成功");
        assert_eq!(row_of(&table, Layer::Rules).entries, 1);
        assert!(Layer::Rules.protected(), "规则段不在阶梯上（§2.3）");
    }

    /// 不进上下文的类别连层都没有，它们的字节也一分不许算进预算
    #[test]
    fn bookkeeping_entries_neither_take_a_layer_nor_take_chars() {
        let mut log = SessionLog::new();
        push(
            &mut log,
            EntryPayload::Usage {
                kind: "turn".into(),
                provider: "p".into(),
                model: "m".into(),
                usage: UsageRecord {
                    input_tokens: 1,
                    output_tokens: 1,
                    cached_tokens: None,
                    cache_write_tokens: 0,
                },
                note: None,
            },
        );
        push(
            &mut log,
            EntryPayload::SessionInfo {
                name: Some("标题".into()),
            },
        );
        push(&mut log, user("唯一的一问"));
        let table = uses(&log, &head()).expect("贴标签该成功");
        let sent = sent_array(&log).expect("投影该成功");
        assert_eq!(
            table.iter().map(|row| row.chars).sum::<usize>(),
            wire_chars(&head()) + wire_chars(&sent)
        );
        assert_eq!(table.iter().map(|row| row.entries).sum::<usize>(), 1);
    }

    /// 定形条目不占 messages，但它每轮都发：它的字节要算进预算，才算不到历史头上
    #[test]
    fn the_frozen_declarations_are_billed_without_occupying_a_row() {
        let mut log = SessionLog::new();
        push(
            &mut log,
            EntryPayload::Custom {
                custom_type: DECLARATIONS_TYPE.into(),
                data: Some(json!([{"function": {"name": "read_file"}}])),
            },
        );
        push(&mut log, user("一问"));
        let table = uses(&log, &head()).expect("贴标签该成功");
        let tools = row_of(&table, Layer::Tools);
        assert_eq!(tools.entries, 1);
        assert_eq!(tools.rows, 0, "声明不在 messages 数组里");
        assert!(tools.chars > 0, "不占行不等于不占预算");
        let sent = sent_array(&log).expect("投影该成功");
        assert_ne!(
            estimate(&table).chars,
            wire_chars(&head()) + wire_chars(&sent),
            "这一层的字节确实落在请求体的另一格里"
        );
    }

    /// 边界数据之一：窗口比留给输出的那截还小。旧实现里 `limit` 归零、常驻段一行都装不下，
    /// 而归零的表会把压缩闸门变成无视用量每轮开闸（上下文 1.4% 照压那次事故）——
    /// 现在输入侧保底半个窗口。不变的是：装不下报 deficit / OverBudget（只能 Notice），
    /// 不裁常驻段，历史也没资格为一个装不下的常驻段去陪葬
    #[test]
    fn a_window_smaller_than_the_output_reserve_reports_over_budget_without_conceding() {
        let mut log = SessionLog::new();
        push(&mut log, user("一问"));
        let table = uses(&log, &head()).expect("贴标签该成功");
        let plan_table = budget_of(&table, 100, 4_096);
        assert_eq!(
            plan_table.limit, 50,
            "窗口先要留给输出，但减不出位子时保底半个窗口：不许归零，也不许回绕"
        );
        let identity = plan_table.row(Layer::Identity).expect("常驻段那行必须在");
        assert!(identity.chars > 0);
        assert_eq!(
            identity.chars, identity.max,
            "常驻段不让步：占多少就是多少"
        );
        let plan = plan(estimate(&table), &plan_table, None);
        assert_eq!(
            plan.reason,
            BreakReason::OverBudget { limit: 50 },
            "确实装不下，如实报超"
        );
        assert_eq!(
            plan.ladder,
            vec![Concession::None],
            "历史还没越过硬顶，不该为了一个装不下的常驻段去压历史"
        );
    }

    /// 边界数据之二：正好差一个字符。越界的那一层欠自己的让步，没越界的一概不欠
    #[test]
    fn a_layer_one_char_over_its_ceiling_owes_exactly_its_own_rungs() {
        let mut log = SessionLog::new();
        write_sections(&mut log, &[memory("记忆")]);
        push(&mut log, user("一问"));
        let table = uses(&log, &head()).expect("贴标签该成功");
        let roomy = budget_of(&table, 1_000_000, 0);
        assert!(
            roomy
                .rows
                .iter()
                .all(|row| row.chars <= row.max && row.chars <= row.target),
            "窗口大到没有边界时，阶梯上一步都不许欠：{roomy:?}"
        );
        assert_eq!(
            plan(estimate(&table), &roomy, None).reason,
            BreakReason::Fits
        );

        let memory = chars_of_layer(&table, Layer::Memory);
        let tight = budget_of(
            &table,
            chars_of_layer(&table, Layer::Identity) + chars_of_layer(&table, Layer::Turn) + memory
                - 1,
            0,
        );
        let owed = tight.row(Layer::Memory).expect("有");
        assert_eq!(owed.chars, owed.max + 1);
        assert_eq!(
            plan(estimate(&table), &tight, None).ladder,
            vec![Concession::DropMemorySection],
            "越界的是哪一层就该由那一层付账"
        );
    }

    /// 让位的次序：分配按"最后让步的最先拿位子"走，所以历史拿的是别人挑剩的。
    /// 这条测的是 `allocation_order` 与 `LADDER` 是同一件事的两面
    #[test]
    fn history_is_served_last_so_a_bulging_section_pushes_it_onto_the_ladder() {
        let mut log = SessionLog::new();
        write_sections(&mut log, &[skills("pdf"), memory(&"记忆".repeat(20_000))]);
        push(&mut log, user("一问"));
        push(&mut log, assistant(&"答复".repeat(2_000)));
        let table = uses(&log, &head()).expect("贴标签该成功");
        let plan_table = budget_of(&table, 30_000, 0);

        assert!(
            plan_table.row(Layer::Skills).expect("有").target > 0,
            "技能清单先坐下了"
        );
        let memory = plan_table.row(Layer::Memory).expect("有");
        assert!(memory.chars > memory.max, "记忆段把位子吃光了");
        let history = plan_table.row(Layer::History).expect("有");
        assert!(history.chars > history.max);
        assert_eq!(
            plan(estimate(&table), &plan_table, None).ladder,
            vec![
                Concession::ClipToolResult,
                Concession::CompactHistory,
                Concession::DropMemorySection
            ],
            "阶梯只按从便宜到贵的次序认领，谁越过硬顶都不改变这个次序（§4.3）"
        );
    }

    /// T02 的主判据：只有工具结果超限的样本，第一步必须是夹工具结果。
    /// 变异：把 `LADDER` 前两步调换，这一条立刻红
    #[test]
    fn an_oversized_tool_result_owes_the_clip_before_it_owes_a_compaction() {
        let mut log = SessionLog::new();
        push(&mut log, user("读一下这个大文件"));
        push(
            &mut log,
            EntryPayload::Message {
                message: Message::Assistant(
                    PendingAssistant {
                        content: String::new(),
                        tool_calls: vec![ToolCall {
                            id: "call_1".into(),
                            name: "read_file".into(),
                            arguments: "{}".into(),
                            content_chars: None,
                        }],
                    }
                    .settle(StopReason::ToolUse),
                ),
            },
        );
        push(&mut log, tool(&"行".repeat(60_000)));
        let table = uses(&log, &head()).expect("贴标签该成功");
        assert_eq!(
            row_of(&table, Layer::Turn).rows,
            1,
            "本轮那一问之外的都还是历史：工具轮第十步得能被压，不然阶梯上那两步没有对象"
        );
        let plan_table = budget_of(&table, 24_000, 4_000);
        let plan = plan(estimate(&table), &plan_table, None);
        assert_eq!(
            plan.ladder,
            vec![Concession::ClipToolResult, Concession::CompactHistory],
            "越界的是历史那层，就只该轮到它那两步；段层没越界就不该被牵连"
        );
        assert!(matches!(plan.reason, BreakReason::OverBudget { .. }));
        assert_eq!(
            Concession::LADDER,
            [
                Concession::ClipToolResult,
                Concession::CompactHistory,
                Concession::DropMemorySection,
                Concession::TrimSkills,
            ],
            "阶梯顺序本身是协议：便宜的那步先做，因为它一个已发字节都不动"
        );
        assert_eq!(
            Concession::ClipToolResult.layer(),
            Concession::CompactHistory.layer(),
            "同层两步会被一起认领，调用方每做一步重算重问（§4.4）"
        );
    }

    /// 历史的底线来自"压缩省不省得出东西"：它决定了段层能不能往下挤
    #[test]
    fn a_section_layers_target_shrinks_to_zero_before_it_eats_the_retention_window() {
        let mut log = SessionLog::new();
        write_sections(&mut log, &[skills("pdf")]);
        push(&mut log, user("一问"));
        let table = uses(&log, &head()).expect("贴标签该成功");
        let window = chars_of_layer(&table, Layer::Identity)
            + chars_of_layer(&table, Layer::Turn)
            + chars_of_layer(&table, Layer::Skills);
        let plan_table = budget_of(&table, window, 0);
        let skills = plan_table.row(Layer::Skills).expect("有");
        assert_eq!(
            skills.target, 0,
            "该坐 0 的意思是：想让保留窗成立，这层就得让"
        );
        assert_eq!(
            skills.max, skills.chars,
            "但还没到非裁不可，所以硬顶就是它现在的量"
        );
        assert_eq!(
            plan(estimate(&table), &plan_table, None).ladder,
            vec![Concession::None],
            "装得下就不欠让步——target 说的是位置，max 说的才是裁不裁"
        );
    }

    /// 预算表自身的形状：分配不许超过天花板，行集合不许随窗口变，不让步的层两个数相等
    #[test]
    fn every_target_fits_inside_the_window_it_was_allocated_from() {
        let mut log = SessionLog::new();
        write_sections(
            &mut log,
            &[
                workspace("构建：cargo test"),
                skills("pdf / xlsx"),
                memory("用户住在杭州"),
            ],
        );
        push(&mut log, user("一问"));
        push(&mut log, assistant("一答"));
        let table = uses(&log, &head()).expect("贴标签该成功");
        for window in [0usize, 500, 4_096, 20_000, KEEP_RECENT_CHARS * 3, 1_000_000] {
            let plan_table = budget_of(&table, window, 1_024);
            let sum: usize = plan_table.rows.iter().map(|row| row.target).sum();
            assert!(
                sum <= plan_table.limit,
                "窗口 {window} 时分配超了：{plan_table:?}"
            );
            assert_eq!(
                plan_table.rows.len(),
                Layer::ORDER.len(),
                "表的行集合不许随窗口变"
            );
            for row in &plan_table.rows {
                assert!(row.target <= row.chars, "target 不许超过它实际的量");
                assert!(row.target <= row.max, "该坐的位置不许高于硬顶");
                if row.layer.protected() {
                    assert_eq!(row.target, row.max, "不让步的层两个数相等");
                }
            }
        }
    }

    /// §8.1 的那条风险关闭方式：预算表整个换一套，日志的字节一个字都不动
    #[test]
    fn changing_the_budget_never_touches_the_bytes_already_sent() {
        let mut log = SessionLog::new();
        write_sections(&mut log, &[workspace("构建：cargo test")]);
        push(&mut log, user("一问"));
        push(&mut log, assistant("一答"));
        let before = sent_array(&log).expect("投影该成功");
        let entries_before = log.entries().to_vec();

        let table = uses(&log, &head()).expect("贴标签该成功");
        for window in [1usize, 9_000, 900_000] {
            let plan_table = budget_of(&table, window, 512);
            let _ = plan(estimate(&table), &plan_table, Some(Cause::Compacted));
            let _ = estimate(&table).chars + plan_table.limit + plan_table.deficit;
        }

        assert_eq!(
            sent_array(&log).expect("投影该成功"),
            before,
            "读了三层预算之后，实发数组变了形"
        );
        assert_eq!(log.entries().to_vec(), entries_before);
        assert_eq!(log.len(), 3, "层的统计不许往日志里追加任何东西");
    }

    /// 层的全部输入就是日志：同一份 log 两次统计必须给出同一张表
    #[test]
    fn the_layer_table_is_a_function_of_the_log_alone() {
        let mut log = SessionLog::new();
        push(&mut log, user("一问"));
        push(&mut log, assistant("一答"));
        assert_eq!(
            uses(&log, &head()).expect("统计该成功"),
            uses(&log, &head()).expect("统计该成功")
        );
    }

    /// 授权断开沿用 prefix.rs 那三种，不新增第四种；判定次序是超预算优先
    #[test]
    fn an_authorized_reset_explains_the_break_only_while_the_budget_still_fits() {
        let mut log = SessionLog::new();
        push(&mut log, user("一问"));
        let table = uses(&log, &head()).expect("统计该成功");
        let roomy = budget_of(&table, 100_000, 0);
        assert_eq!(
            plan(estimate(&table), &roomy, Some(Cause::Navigated)).reason,
            BreakReason::Authorized {
                cause: Cause::Navigated
            },
            "装得下却断了线，那只能是用户动的手"
        );
        let tight = budget_of(&table, 10, 0);
        assert_eq!(
            plan(estimate(&table), &tight, Some(Cause::Navigated)).reason,
            BreakReason::OverBudget { limit: tight.limit },
            "同时超预算时先说预算：那才是可行动的那一件"
        );
    }

    /// `thread_chars` 是压缩条目 `tokens_before` 的唯一算法（T04 的支持侧）：
    /// 它算的必须是"压缩前实际发出去的那批字节"，包含不在日志里的常驻段
    #[test]
    fn thread_chars_measures_the_whole_outgoing_array_not_just_the_history() {
        let mut log = SessionLog::new();
        push(&mut log, user("一问"));
        push(&mut log, assistant("一答"));
        let history = sent_array(&log).expect("投影该成功");
        let whole = thread_chars(&head(), &history);
        assert_eq!(
            whole as usize,
            wire_chars(&head()) + wire_chars(&history),
            "常驻段不在日志投影里，漏掉它就会低估压缩前的大小"
        );
        assert!(whole as usize > wire_chars(&history));
    }

    fn row(id: &str, chars: usize) -> HistoryRow {
        HistoryRow { id: id.into(), chars }
    }

    /// T07 的那一步挑的是**最老**的那一段，挑到剩下的量能坐到 target 以下就停
    #[test]
    fn layer_compaction_takes_the_oldest_block_that_fits_the_target() {
        let rows = vec![row("a", 4_000), row("b", 3_000), row("c", 2_000), row("d", 1_000)];
        let plan = layer_compaction(&rows, 4_000).expect("这层越界了，该给出一个计划");
        assert_eq!(
            (plan.from_id.as_str(), plan.through_id.as_str(), plan.rows),
            ("a", "b", 2),
            "a 加 b 就把这层送回 4 000 以下，多压一行是白付的信息量"
        );
        assert_eq!(plan.chars, 7_000, "报的是被顶替掉的那些行的字节量");
        assert!(!plan.still_over, "剩下 3 000，坐得进 target");
    }

    /// 底线之一：最新那一行裁不得，哪怕这层还超着 target
    #[test]
    fn layer_compaction_never_eats_the_newest_row() {
        let rows = vec![row("a", 1_000), row("b", 1_000), row("c", 90_000)];
        let plan = layer_compaction(&rows, 1_000).expect("至少能顶替前两行");
        assert_eq!((plan.from_id.as_str(), plan.through_id.as_str(), plan.rows), ("a", "b", 2));
        assert!(
            plan.still_over,
            "90 000 那一行还得留着，这层就是装不下——该往下一步走或报 Notice，而不是继续压"
        );
    }

    /// 这层没越界就不许动手：按层压缩是一次让步，不是例行整理
    #[test]
    fn a_layer_that_sits_inside_its_target_owes_no_compaction() {
        let rows = vec![row("a", 1_000), row("b", 1_000), row("c", 1_000)];
        assert_eq!(layer_compaction(&rows, 3_000), None, "正好坐到线上就不算越界");
        assert_eq!(
            layer_compaction(&rows, 2_999),
            None,
            "为省一个字符付一次摘要请求不值：至少顶替 2 行才动"
        );

        let wider = vec![row("a", 1_000), row("b", 1_000), row("c", 1_000), row("d", 1_000)];
        assert_eq!(
            layer_compaction(&wider, 2_500).map(|plan| plan.rows),
            Some(2),
            "够本了就该给出计划"
        );
    }

    /// 不足 3 行一律不动——那是"至少顶替 2 行"跟"至少留 1 行"的合取，
    /// 调用方要拿到 `None`，而不是一个压完什么都不剩的半个计划
    #[test]
    fn two_rows_of_history_are_not_worth_a_summary_request() {
        assert_eq!(layer_compaction(&[row("a", 9_000), row("b", 9_000)], 100), None);
        assert_eq!(layer_compaction(&[], 100), None);
        assert_eq!(layer_compaction(&[row("a", 9_000)], 100), None);
    }

    /// `history_rows` 与分层统计读的是同一批条目：本轮那一问不在里面，它之前那几行按发出
    /// 的次序在里面。按层压缩的计划只许压它交出来的那些 id——两处不是同一批字节就是压错地方
    #[test]
    fn history_rows_are_the_history_layer_in_the_order_they_are_sent() {
        let mut log = SessionLog::new();
        write_sections(&mut log, &[workspace("构建：cargo test"), memory("用户住在杭州")]);
        let first = push(&mut log, user("第一问"));
        let answer = push(&mut log, assistant("第一答"));
        let second = push(&mut log, user("第二问"));
        let rows = history_rows(&log).expect("取历史行该成功");
        let ids: Vec<&str> = rows.iter().map(|held| held.id.as_str()).collect();
        assert_eq!(
            ids,
            [first.as_str(), answer.as_str()],
            "最后一问算本轮，不在可压的那一段里；段行也不算历史"
        );
        assert_ne!(second.as_str(), rows[0].id.as_str());
        assert!(rows.iter().all(|held| held.chars > 0), "字节量按发出去的那串 JSON 算");
    }
}
