//! 追加式运行账本：一次运行的事实只写在这里。
//!
//! 为什么用 `append` 而不是像 `task-state.json` 那样整份重写（`session/store.rs` 的教训）：
//! 账本记的是"发生过什么"，重写就等于允许历史被后来的事实抹掉——那正是原来那个
//! "每个任务只有一个槽位"的毛病。定义（想要什么）在 `config.json`，可以整份重写、可以备份；
//! 事实在这里，只追加。派生出来的"上次运行"缓存随时能从账本重建，所以它永远不是真相。

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

/// 账本文件名。一次运行的每一行都是那一行的**当时全貌**，读的时候按 run_id 取最后一行
pub const LEDGER_FILE: &str = "runs.jsonl";
/// 派生缓存：沿用退役前的文件名，外面那些只看"上次跑没跑过"的消费者不必跟着改
pub const CACHE_FILE: &str = "task-state.json";

/// 一次运行的结论。`Running` 只可能出现在"再没有第二行"的那一次运行上——也就是没跑完
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    /// 停在高风险动作上等人点头
    WaitingApproval,
    Succeeded,
    Failed,
    /// 追账追到一半发现这一批超出了每轮上限，剩下的格子作废。
    /// 它**不是一次运行**：没有话题、没有成本、没动过任何东西。它存在的唯一理由是
    /// "这一批欠账被谁、按什么规则销掉了"得有一条账能回答——不记就等于静默少跑了几十发
    Skipped,
}

impl RunStatus {
    /// 还没有结论：崩在半路，或停在待审批上
    pub fn is_open(self) -> bool {
        matches!(self, RunStatus::Running | RunStatus::WaitingApproval)
    }

    /// 退役缓存与 `task-ran` 事件的词汇表。界面的 `lastStatus` 认的就是这几个词：
    /// `waiting` 画成"在等人点头"，`""`（还在跑）不画东西
    pub fn cache_label(self) -> &'static str {
        match self {
            RunStatus::Succeeded => "ok",
            RunStatus::Failed => "error",
            RunStatus::WaitingApproval => "waiting",
            RunStatus::Skipped => "skipped",
            RunStatus::Running => "",
        }
    }
}

/// 这一发是谁起的。`audit::Actor` 只序列化不反序列化，而账本行要能读回来，
/// 所以这里只声明实际会写出来的那几个主体，并给出一条回到审计词汇的映射
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StartedBy {
    Scheduler,
    User,
    /// 本机另一个进程敲了 `POST /hook/<令牌>`（`tasks/inbound.rs`）。
    /// 审计里它仍算 Scheduler：那是"机器引起的"，不是"人点的"
    Webhook,
}

impl StartedBy {
    pub fn actor(self) -> crate::audit::Actor {
        match self {
            StartedBy::Scheduler | StartedBy::Webhook => crate::audit::Actor::Scheduler,
            StartedBy::User => crate::audit::Actor::User,
        }
    }
}

/// 一次 run 烧掉的钱与 token，从用量台账按 `conversation_id` 现算
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Cost {
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: i64,
    /// 与用量台账同样的整数记账：浮点累加会漂
    pub cost_usd_e8: i64,
    /// 没配上价格表的请求数。不报这个数，"这个任务没花钱"和"没人给它定价"长得很像
    pub unpriced_requests: i64,
}

impl Cost {
    pub fn cost_usd(&self) -> f64 {
        self.cost_usd_e8 as f64 / 1e8
    }
}

/// 账本里的一行 = 某次运行在某一时刻的全貌。字段刻意冗余（收尾行不带"只写增量"的假设），
/// 这样"最后一行赢"的合并规则不需要回头看前面的行
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Line {
    pub run_id: String,
    pub task_id: String,
    pub started_by: StartedBy,
    pub status: RunStatus,
    pub started_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
    pub conversation_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
    /// 这一行是任务图里哪一格的检查点。`None` = 这一次运行本身那一行。
    /// 节点状态**只**从这里推导，图自己不留进度——否则就要回答"两份不一致时信谁"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// 子助理那一发属于哪个父 run（T08 用）。老账本里没有这个键，读回来就是 None
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<String>,
    /// 跑完之后那条通知投出去了没。`None` = 这个任务没配通知地址（"没这件事"），
    /// 配了却没送达是 `Some(带原因)`——界面画法不一样，因为"没配"不该被渲染成失败
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery: Option<crate::tasks::hook::Delivery>,
}

impl Line {
    pub fn is_unfinished(&self) -> bool {
        self.status.is_open()
    }

    /// 这一行属于某一发的内部（子助理那一 run）。"跑过几次 / 上次跑到什么样"这类
    /// 派生视图只认不是子 run 的那些，判据放在这一处，别在每张派生视图里再写一遍父链
    pub fn is_sub_run(&self) -> bool {
        self.parent_run_id.is_some()
    }
}

/// 给界面的一行：账本行加上两个派生字段（美元成本、"这发还没完事"的旗标）。
/// 账本里存的是 1e-8 整数，界面上要的是能直接渲染的数
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunView {
    pub run_id: String,
    pub task_id: String,
    pub started_by: StartedBy,
    pub status: RunStatus,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub conversation_id: String,
    pub error: Option<String>,
    pub unfinished: bool,
    pub cost: Option<Cost>,
    pub cost_usd: f64,
    /// 这一次运行里各格的最终样子。老任务（没有图）这里就是空表——
    /// "没有格"与"读不到"是两件事，所以它是空表而不是 null
    pub nodes: Vec<NodeView>,
    pub delivery: Option<crate::tasks::hook::Delivery>,
}

/// 一格跑成什么样：账本里那一节点的投影。"第 2/4 格"这种话只有在这里凑得出来，
/// 所以它跟着运行一起交给界面，而不是让前端再去问一遍账本
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeView {
    pub node_id: String,
    pub status: RunStatus,
    pub conversation_id: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub error: Option<String>,
    pub cost_usd: f64,
    /// 这一格是交出去给子助理跑的。它不改变这一格的成绩，只说明那份产出不是这一发自己做的
    pub delegated: bool,
}

/// 账本行 → 界面的一格。`sub_convs` 是这一发派出去的那些子 run 的话题 id：
/// 格子与子 run 靠同一个 `conversation_id` 对上，所以这里不需要账本再记第二份"我是谁派的"
fn node_view(line: &Line, sub_convs: &BTreeSet<String>) -> NodeView {
    NodeView {
        node_id: line.node.clone().unwrap_or_default(),
        status: line.status,
        conversation_id: line.conversation_id.clone(),
        started_at: line.started_at,
        finished_at: line.finished_at,
        error: line.error.clone(),
        cost_usd: line.cost.map(|cost| cost.cost_usd()).unwrap_or_default(),
        delegated: sub_convs.contains(&line.conversation_id),
    }
}

/// 账本行 → 界面行。运行那一层与它的各格分开拼，因为格子来自账本的另一批行
fn view_of(line: &Line, nodes: Vec<NodeView>) -> RunView {
    RunView {
        run_id: line.run_id.clone(),
        task_id: line.task_id.clone(),
        started_by: line.started_by,
        status: line.status,
        started_at: line.started_at,
        finished_at: line.finished_at,
        conversation_id: line.conversation_id.clone(),
        error: line.error.clone(),
        unfinished: line.is_unfinished(),
        cost: line.cost,
        cost_usd: line.cost.map(|cost| cost.cost_usd()).unwrap_or_default(),
        nodes,
        delivery: line.delivery.clone(),
    }
}

/// 只有任务自己起的那些发。子 run 认一个父亲，它是某一次运行的**内部**，
/// 所以"这个任务跑过几次 / 上次跑到什么样"这类派生视图一律不把它算成一次
pub fn root_runs(root: &Path) -> Vec<Line> {
    runs(root)
        .into_iter()
        .filter(|line| !line.is_sub_run())
        .collect()
}

/// 派生缓存的一格：这个任务上次跑到什么样。它是账本的投影，不是账本
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LastRun {
    pub last_run_at: i64,
    /// "" = 没跑过，或最后一次还没跑完
    pub last_status: String,
    pub last_error: String,
    pub last_conversation_id: String,
}

impl From<&Line> for LastRun {
    fn from(line: &Line) -> Self {
        Self {
            last_run_at: line.started_at,
            last_status: line.status.cache_label().to_string(),
            last_error: line.error.clone().unwrap_or_default(),
            last_conversation_id: line.conversation_id.clone(),
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis() as i64)
        .unwrap_or(0)
}

static SEQ: AtomicU64 = AtomicU64::new(0);

/// 一次运行的唯一后缀。同一毫秒里起的两发也要分得开，所以拿纳秒再串一个进程内序号
pub fn token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}-{}", SEQ.fetch_add(1, Ordering::Relaxed))
}

fn ledger_path(root: &Path) -> PathBuf {
    root.join(LEDGER_FILE)
}

fn cache_path(root: &Path) -> PathBuf {
    root.join(CACHE_FILE)
}

/// 一次运行的账本、派生缓存与待审批队列都住在这里。测试直接拿这个目录当参数，
/// 所以模块内的函数一律吃 `&Path`，只有入口那一层碰 AppHandle
pub fn data_root(app: &AppHandle) -> Result<PathBuf, String> {
    let root = app.path().app_data_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    Ok(root)
}

/// 追加一行。**只追加**：不改写、不删除，所以崩溃时留下的半成品行也留在原位
fn append(root: &Path, line: &Line) -> Result<(), String> {
    let text = serde_json::to_string(line).map_err(|e| format!("账本行编码失败：{e}"))?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(ledger_path(root))
        .map_err(|e| format!("打开 {} 失败：{e}", ledger_path(root).display()))?;
    // 一次 write_all：并发的追加若分成两次写，两条记录会粘在同一行上
    let record = format!("{text}\n").into_bytes();
    file.write_all(&record).map_err(|e| format!("写账本失败：{e}"))
}

/// 读原始行。读不动的那一行（写到一半被杀）不静默丢掉，而是当成一次没回来的运行——
/// 半行本身就是一次运行正在发生的证据
pub fn read_lines(root: &Path) -> Vec<Line> {
    let Ok(text) = fs::read_to_string(ledger_path(root)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        if raw.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Line>(raw) {
            Ok(line) => out.push(line),
            Err(_) => out.push(unfinished_marker(raw, index)),
        }
    }
    out
}

/// 解析不动的那一行仍然要"看得见有一次运行没回来"：宁可报一个认不出任务的未完成行，
/// 也不要静默吞掉它——那正是"跑过但没记录"的原始形状。id 用行号，两条半行不会并成一条
fn unfinished_marker(raw: &str, index: usize) -> Line {
    Line {
        run_id: format!("unreadable-line-{index}"),
        task_id: String::new(),
        started_by: StartedBy::Scheduler,
        status: RunStatus::Running,
        started_at: 0,
        finished_at: None,
        conversation_id: String::new(),
        error: Some(format!("账本这一行读不动：{}", raw.chars().take(80).collect::<String>())),
        cost: None,
        node: None,
        parent_run_id: None,
        delivery: None,
    }
}

/// 按 run_id 合并成"每次运行一条"，顺序从新到旧。
/// 同一个 run_id 的后一行赢——收尾行总是全貌，所以合并规则不需要看前一行。
/// 节点检查点行（`node` 非空）不在这里出现：它们属于某一次运行的**内部**，
/// 由 `checkpoints` 单独读。把它们混进来的话，一次四格的运行会在运行列表里
/// 变成"跑过五次"，派生的"上次什么时候跑过"也会跟着跳
pub fn runs(root: &Path) -> Vec<Line> {
    let mut order: Vec<String> = Vec::new();
    let mut latest: BTreeMap<String, Line> = BTreeMap::new();
    for line in read_lines(root) {
        if line.node.is_some() {
            continue;
        }
        if !latest.contains_key(&line.run_id) {
            order.push(line.run_id.clone());
        }
        latest.insert(line.run_id.clone(), line);
    }
    order.reverse();
    order.iter_mut().filter_map(|id| latest.remove(id)).collect()
}

/// 某一格跑完了：追加一行检查点。它带自己那一格的话题、时间与花费，
/// 所以"这一格跑过没有"这件事只从账本就能答，不需要谁再去记一份进度
pub fn checkpoint(
    root: &Path,
    begun: &Line,
    node_id: &str,
    conversation_id: &str,
    started_at: i64,
    status: RunStatus,
    error: Option<String>,
    cost: Option<Cost>,
) -> Result<Line, String> {
    let done = Line {
        run_id: begun.run_id.clone(),
        task_id: begun.task_id.clone(),
        started_by: begun.started_by,
        status,
        started_at,
        finished_at: Some(now_ms()),
        conversation_id: conversation_id.to_string(),
        error,
        cost,
        node: Some(node_id.to_string()),
        parent_run_id: begun.parent_run_id.clone(),
        delivery: None,
    };
    append(root, &done)?;
    Ok(done)
}

/// 这一次运行里各格的最终样子：同一个 run_id 下按格合并，后一行赢
/// （重跑过的一格以最后一次为准）。次序是账本里的先后
pub fn checkpoints(root: &Path, run_id: &str) -> Vec<Line> {
    let mut order: Vec<String> = Vec::new();
    let mut latest: BTreeMap<String, Line> = BTreeMap::new();
    for line in read_lines(root) {
        let Some(node) = line.node.clone() else { continue };
        if line.run_id != run_id {
            continue;
        }
        if !latest.contains_key(&node) {
            order.push(node.clone());
        }
        latest.insert(node, line);
    }
    order.into_iter().filter_map(|node| latest.remove(&node)).collect()
}

/// 跑成的每一格各自那一发话题的 id。下游那一格要读上游说过什么，而"它说了什么"只住在
/// 它自己那一发的话题日志里——所以账本这里只回答"该去读哪一本"。
/// 只认跑成的：失败那一格的产出喂给下游，等于让下游基于一份没通过的东西接着干
pub fn node_conversations(root: &Path, run_id: &str) -> BTreeMap<String, String> {
    checkpoints(root, run_id)
        .into_iter()
        .filter(|line| line.status == RunStatus::Succeeded)
        .filter_map(|line| line.node.map(|node| (node, line.conversation_id)))
        .collect()
}

/// 已经跑成的那些格——续跑就是从这里开始算"还差什么"
pub fn done_nodes(root: &Path, run_id: &str) -> Vec<String> {    checkpoints(root, run_id)
        .into_iter()
        .filter(|line| line.status == RunStatus::Succeeded)
        .map(|line| line.node.unwrap_or_default())
        .collect()
}

/// 各格花费之和，再加上**这一发派生出去的子 run**。
///
/// 子助理的钱必须记在父下发：不然"这个任务一共花了多少"就是一个看起来对、
/// 实际上少了一截的数字。子 run 的花费由它自己那次收尾行带着，所以这里不重算，只汇总。
///
/// 但一笔钱只能进来一次。交出去的那一格记的检查点行，用的就是子 run 那个话题，
/// 而花费本来就是按话题聚合的——同一个 `conversation_id` 在两行上是同一个数字。
/// 所以已经按格子算过的那些话题，就不再从子 run 行上加第二遍
pub fn total_cost(root: &Path, run_id: &str) -> Option<Cost> {
    let mut sum = Cost::default();
    let mut counted: BTreeSet<String> = BTreeSet::new();
    for row in checkpoints(root, run_id) {
        if let Some(cost) = row.cost {
            add(&mut sum, cost);
            counted.insert(row.conversation_id);
        }
    }
    for child in children_of(root, run_id) {
        if counted.contains(&child.conversation_id) {
            continue;
        }
        if let Some(cost) = child.cost {
            add(&mut sum, cost);
        }
    }
    (sum.requests > 0).then_some(sum)
}

fn add(into: &mut Cost, more: Cost) {
    into.requests += more.requests;
    into.input_tokens += more.input_tokens;
    into.output_tokens += more.output_tokens;
    into.cached_tokens += more.cached_tokens;
    into.cost_usd_e8 += more.cost_usd_e8;
    into.unpriced_requests += more.unpriced_requests;
}

/// 直接挂在某一发下面的子 run（每一发合并后的最后一行）。没父链的普通运行不会出现在这里
pub fn children_of(root: &Path, parent_run_id: &str) -> Vec<Line> {
    runs(root)
        .into_iter()
        .filter(|line| line.parent_run_id.as_deref() == Some(parent_run_id))
        .collect()
}

/// 这条父链有多深：自己算 1。它是"子助理不许自我扩散"那一条关闭方式的读数——
/// 深度不看内存，看账本，所以重启之后仍然拦得住
pub fn chain_len(root: &Path, run_id: &str) -> usize {
    let mut depth = 0usize;
    let mut cursor = run_id.to_string();
    // 每一跳只认账本里那一行的父 id；父链成环（手改出来的）靠 visited 上限挡住，
    // 20 层之外就不是"链"而是死循环了
    while depth < 20 {
        match find(root, &cursor).and_then(|line| line.parent_run_id) {
            Some(parent) => {
                depth += 1;
                cursor = parent;
            }
            None => break,
        }
    }
    depth + 1
}

/// 账本上某一次运行的那一行（收尾之后的样子）。续跑要先问它还没跑完没有
pub fn find(root: &Path, run_id: &str) -> Option<Line> {
    runs(root).into_iter().find(|line| line.run_id == run_id)
}

/// 界面要的运行列表：一次读盘，把各格挂回它所属的那一发。`task_id` 给了就只列那个任务的。
/// 从新到旧，最多 `limit` 发。运行行与各格行共用"同一 id 后一行赢"这一条合并规则。
/// 子助理那一发不在这里：它属于某一发的内部，混进来的话"这个任务跑过几次"就会多算
pub fn views(root: &Path, task_id: Option<&str>, limit: usize) -> Vec<RunView> {
    let lines = read_lines(root);
    let mut stamps: BTreeMap<String, Vec<Line>> = BTreeMap::new();
    for line in lines.iter().filter_map(|line| line.node.clone().map(|_| line)) {
        let bucket = stamps.entry(line.run_id.clone()).or_default();
        match bucket.iter_mut().find(|held| held.node == line.node) {
            Some(slot) => *slot = line.clone(),
            None => bucket.push(line.clone()),
        }
    }

    // 每一发派出去的那些子 run 用过哪些话题。格子与子 run 靠同一个 conversation_id 对上，
    // 所以账本不必再记第二份"这一格是谁干的"
    let mut subs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for line in lines.iter().filter(|line| line.node.is_none()) {
        if let Some(parent) = line.parent_run_id.as_ref() {
            subs.entry(parent.clone()).or_default().insert(line.conversation_id.clone());
        }
    }
    let none: BTreeSet<String> = BTreeSet::new();

    let mut order: Vec<String> = Vec::new();
    let mut latest: BTreeMap<String, Line> = BTreeMap::new();
    for line in lines.iter().filter(|line| line.node.is_none() && !line.is_sub_run()) {
        if !latest.contains_key(&line.run_id) {
            order.push(line.run_id.clone());
        }
        latest.insert(line.run_id.clone(), line.clone());
    }
    order.reverse();
    order
        .into_iter()
        .filter_map(|id| latest.remove(&id))
        .filter(|line| task_id.is_none_or(|id| line.task_id == id))
        .take(limit)
        .map(|line| {
            let held = subs.get(&line.run_id).unwrap_or(&none);
            let nodes = stamps
                .remove(&line.run_id)
                .unwrap_or_default()
                .iter()
                .map(|node| node_view(node, held))
                .collect();
            view_of(&line, nodes)
        })
        .collect()
}

/// 某个任务的运行历史，从新到旧。生产侧读的是 `last_starts`/`runs`（按界面的口径），
/// 按任务过滤的这条只有测试在用
#[cfg(test)]
pub fn runs_of(root: &Path, task_id: &str) -> Vec<Line> {
    runs(root)
        .into_iter()
        .filter(|line| line.task_id == task_id)
        .collect()
}

/// 每个任务最近一次起跑的时间，一次读盘算全部（调度线程每轮只要这一份）。
/// 上次运行从账本现算，那份派生缓存只给界面读
pub fn last_starts(root: &Path) -> BTreeMap<String, i64> {
    let mut out = BTreeMap::new();
    for line in root_runs(root) {
        // runs() 从新到旧，第一次见到就是最近那次
        out.entry(line.task_id).or_insert(line.started_at);
    }
    out
}

/// 派生缓存的全部内容。没有结论的那一次也占一格，但状态是空的：
/// 它至少证明这一发起过，而"跑成功了"是另一件事
pub fn derived_cache(root: &Path) -> BTreeMap<String, LastRun> {
    let mut out = BTreeMap::new();
    for line in root_runs(root) {
        // runs() 从新到旧，所以每个任务第一次见到就是它最近的那一次
        out.entry(line.task_id.clone()).or_insert_with(|| LastRun::from(&line));
    }
    out
}

/// 把缓存重写成账本的样子。缓存写失败只打日志：它是派生物，账本才是那一行事实
fn write_cache(root: &Path, state: &BTreeMap<String, LastRun>) {
    let Ok(text) = serde_json::to_string_pretty(state) else {
        return;
    };
    if let Err(error) = fs::write(cache_path(root), text) {
        eprintln!("定时任务缓存写入失败（账本不受影响）：{error}");
    }
}

/// 从账本重建派生缓存。启动时、以及每次跑完都刷一次：除此之外没人该改它
pub fn refresh_cache(root: &Path) -> BTreeMap<String, LastRun> {
    let state = derived_cache(root);
    write_cache(root, &state);
    state
}

/// 子助理那一发的起跑行：与 `begin` 同形，只是它认一个父亲。
/// 父链只记在账本里，别处不存第二份——深度、花费、"这发是谁起的"都从这一份推
pub fn begin_child(
    app: &AppHandle,
    task_id: &str,
    conversation_id: &str,
    started_by: StartedBy,
    parent_run_id: &str,
) -> Result<Line, String> {
    let root = data_root(app)?;
    let line = first_line(task_id, conversation_id, started_by, Some(parent_run_id.to_string()));
    append(&root, &line)?;
    Ok(line)
}

fn first_line(
    task_id: &str,
    conversation_id: &str,
    started_by: StartedBy,
    parent_run_id: Option<String>,
) -> Line {
    Line {
        run_id: format!("run-{}", token()),
        task_id: task_id.to_string(),
        started_by,
        status: RunStatus::Running,
        started_at: now_ms(),
        finished_at: None,
        conversation_id: conversation_id.to_string(),
        error: None,
        cost: None,
        node: None,
        parent_run_id,
        delivery: None,
    }
}

/// 把一批作废的格子记成**一行**：一行代表"这批欠账被每轮上限砍掉了"，而不是 N 行假装它们
/// 各自跑过。它有两个作用：让"我错过了几次、为什么没跑"有条账能回答；以及把 `last_starts`
/// 的锚点推到这一刻——不然每一轮都会重新判一次同一批欠账，那才是真的自我扩散
pub fn void_slots(root: &Path, task_id: &str, voided: usize, budget: usize, now: i64) -> Result<(), String> {
    append(
        root,
        &Line {
            run_id: format!("skip-{}", token()),
            task_id: task_id.to_string(),
            started_by: StartedBy::Scheduler,
            status: RunStatus::Skipped,
            started_at: now,
            finished_at: Some(now),
            conversation_id: String::new(),
            error: Some(format!(
                "错过 {voided} 格没补：一轮最多补 {budget} 格。要改的是任务自己的「错过时怎么办」——\
                 换成「只跑最近一次」或「作废」就不会再攒下这种账"
            )),
            cost: None,
            node: None,
            parent_run_id: None,
            delivery: None,
        },
    )
}

/// 一次开跑：先落一行 `Running`。这一行是"跑过但没回来"的唯一证据，
/// 所以写不进去就不该起跑
pub fn begin(
    app: &AppHandle,
    task_id: &str,
    conversation_id: &str,
    started_by: StartedBy,
) -> Result<Line, String> {
    let root = data_root(app)?;
    // 父链是子助理那一发要写的东西，正常起跑的就是根
    let line = first_line(task_id, conversation_id, started_by, None);
    append(&root, &line)?;
    Ok(line)
}

/// 收尾：再追加一行，把这一次的全貌写完整。原来的行一个字节都不动
pub fn finish(
    root: &Path,
    line: &Line,
    status: RunStatus,
    error: Option<String>,
    cost: Option<Cost>,
) -> Result<Line, String> {
    let done = Line {
        status,
        finished_at: Some(now_ms()),
        error,
        cost,
        ..line.clone()
    };
    append(root, &done)?;
    refresh_cache(root);
    Ok(done)
}

/// 把投递结果追加成同一发的后一行。**运行行本身一个字都不动**：通知是通知，
/// 它成没成不改动"这一发跑成了什么"。后一行赢，所以读出来的那一发带着 delivery
pub fn note_delivery(
    root: &Path,
    line: &Line,
    delivery: crate::tasks::hook::Delivery,
) -> Result<Line, String> {
    let done = Line {
        delivery: Some(delivery),
        ..line.clone()
    };
    append(root, &done)?;
    refresh_cache(root);
    Ok(done)
}

/// 有没有结论、且比这条线更老。两个条件缺一个就不算"旧账"
fn retired(line: &Line, before: i64) -> bool {
    !line.is_unfinished() && line.finished_at.unwrap_or(line.started_at) < before
}

/// 清掉旧的、已经有结论的运行记录。**这是"账本只追加"唯一的一处破例**，所以它被三条
/// 约束钉住，少一条都会把"省地方"变成事故：
/// - 没跑完的那一发不动：那行 `Running` 是"钱已经花了但没回来"的唯一证据，也是续跑的依据；
/// - 每个任务最新那一发不动：它是调度器算欠账的锚点（[`last_starts`]），抽掉它等于让同一批
///   欠账被重新判一遍并补跑——那是真花钱、真副作用；
/// - 读不动的那一行**按原字节**留下：它的字节本身就是证据，把它解析成 `Line` 再写回去，
///   等于把"有一行写到一半被杀"洗成一条正常记录。
///
/// 子 run 跟着父亲走：父亲留着，它的内部那些行就还在。返回抹掉了几发
pub fn purge(root: &Path, before: i64) -> Result<usize, String> {
    let Ok(text) = fs::read_to_string(ledger_path(root)) else {
        return Ok(0);
    };
    let merged = runs(root);

    // runs() 从新到旧，所以每个任务第一次见到的那一发就是最新的
    let mut anchors: BTreeMap<String, String> = BTreeMap::new();
    for line in merged.iter().filter(|line| !line.is_sub_run()) {
        anchors
            .entry(line.task_id.clone())
            .or_insert_with(|| line.run_id.clone());
    }

    let mut drop: BTreeSet<String> = BTreeSet::new();
    for line in merged.iter().filter(|line| !line.is_sub_run()) {
        let newest = anchors.get(&line.task_id).map(String::as_str) == Some(line.run_id.as_str());
        if !newest && retired(line, before) {
            drop.insert(line.run_id.clone());
        }
    }
    for line in merged.iter().filter(|line| line.is_sub_run()) {
        if line
            .parent_run_id
            .as_ref()
            .is_some_and(|parent| drop.contains(parent))
        {
            drop.insert(line.run_id.clone());
        }
    }
    if drop.is_empty() {
        return Ok(0);
    }

    let kept: Vec<&str> = text
        .lines()
        .filter(|raw| match serde_json::from_str::<Line>(raw) {
            // 解析不动的一律留着：这次清除没有资格判定它属于哪一发
            Err(_) => true,
            Ok(line) => !drop.contains(&line.run_id),
        })
        .collect();

    let path = ledger_path(root);
    let tmp = path.with_extension("jsonl.tmp");
    let mut body = String::new();
    for raw in &kept {
        body.push_str(raw);
        body.push('\n');
    }
    fs::write(&tmp, body).map_err(|e| format!("写账本草稿失败：{e}"))?;
    // 换文件而不是就地截断：中途失败的话原账本还是一个完整的老账本
    if let Err(e) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("换上账本失败：{e}"));
    }
    refresh_cache(root);
    Ok(drop.len())
}

/// 一次 run 的钱与 token。按话题归因，所以这条查询的存在本身就是 `conversation_id`
/// 必须是真的那件事的下游结论
pub fn cost_of(conn: &Connection, conversation_id: &str) -> Result<Option<Cost>, String> {
    if conversation_id.is_empty() {
        // 空 id 在用量台账里是"没有归属"那一堆，拿它去聚合会把别人的花费算给这个任务
        return Ok(None);
    }
    let mut stmt = conn
        .prepare(
            "SELECT COUNT(*), SUM(input_tokens), SUM(output_tokens),
                    SUM(CASE WHEN cache_reported = 1 THEN cached_tokens END),
                    SUM(cost_usd_e8), SUM(priced = 0)
             FROM requests WHERE conversation_id = ?1",
        )
        .map_err(|e| e.to_string())?;
    let cost = stmt
        .query_row(rusqlite::params![conversation_id], |row| {
            Ok(Cost {
                requests: row.get::<_, Option<i64>>(0)?.unwrap_or(0),
                input_tokens: row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                output_tokens: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                cached_tokens: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                cost_usd_e8: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                unpriced_requests: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
            })
        })
        .optional()
        .map_err(|e| e.to_string())?;
    // 零行 = 这个话题在台账里还没有痕迹，与"有痕迹但算不出钱"是两件事
    Ok(cost.filter(|value| value.requests > 0))
}

/// 记账的入口。台账打不开时返回 None 而不是让一次已经跑完的运行报错
pub fn cost_for(app: &AppHandle, conversation_id: &str) -> Option<Cost> {
    crate::usage::with_connection(app, |conn| cost_of(conn, conversation_id))
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{remove_tree, scoped_temp_dir, temp_dir};
    use crate::usage::{self, Tokens};

    /// 作废的那一行是**事实但不是运行**：它没有话题、没有成本、不算"没跑完"（所以续跑列表里
    /// 不该有它），但它把锚点推走了——不然同一批欠账每一轮都会被重新判一次，那才是自我扩散
    #[test]
    fn a_voided_batch_is_a_fact_but_not_a_run() {
        let root = temp_dir("runs-void");
        void_slots(&root, "t1", 160, 8, STAMP).expect("落一行作废");
        let line = runs(&root)
            .into_iter()
            .find(|item| item.task_id == "t1")
            .expect("读得回来");
        assert_eq!(line.status, RunStatus::Skipped);
        assert!(!line.is_unfinished(), "作废不是\"没跑完\"：它不该出现在\"从检查点续跑\"那一份里");
        assert!(line.cost.is_none(), "一行作废不产生成本");
        assert!(line.conversation_id.is_empty(), "它没有话题");
        assert_eq!(
            last_starts(&root).get("t1").copied(),
            Some(STAMP),
            "锚点要推到那一刻，那 160 格才算被处理过"
        );
        remove_tree(&root);
    }

    /// 记账时间要固定：成本是按话题聚合的，别让"哪一笔"变成看运气
    const STAMP: i64 = 1_700_000_000_000;

    fn begun(task_id: &str, started_at: i64, conversation_id: &str) -> Line {
        Line {
            run_id: format!("run-{task_id}-{started_at}"),
            task_id: task_id.to_string(),
            started_by: StartedBy::Scheduler,
            status: RunStatus::Running,
            started_at,
            finished_at: None,
            conversation_id: conversation_id.to_string(),
            error: None,
            cost: None,
            node: None,
            parent_run_id: None,
            delivery: None,
        }
    }

    fn settle_line(root: &Path, line: &Line, status: RunStatus, error: Option<String>) {
        let done = Line {
            status,
            finished_at: Some(line.started_at + 1_000),
            error,
            cost: None,
            ..line.clone()
        };
        append(root, &done).expect("收尾行该写得进去");
    }

    /// 清除是"账本只追加"的唯一破例，所以它该被那三条约束钉住：够老、已有结论、
    /// 且不是某个任务的锚点。锚点被抽走的那次不是省了地方，是让调度器把旧欠账重新补跑一遍
    #[test]
    fn the_purge_drops_old_concluded_runs_but_never_the_anchor_or_an_open_one() {
        let root = temp_dir("runs-purge");
        let old = begun("t1", 900, "conv-900");
        append(&root, &old).expect("旧发起跑");
        let sub = Line {
            run_id: "run-sub-900".to_string(),
            parent_run_id: Some(old.run_id.clone()),
            ..begun("t1", 900, "conv-sub")
        };
        append(&root, &sub).expect("子 run 起跑");
        settle_line(&root, &sub, RunStatus::Succeeded, None);
        settle_line(&root, &old, RunStatus::Succeeded, None);

        let anchor = begun("t1", 1_000, "conv-1000");
        append(&root, &anchor).expect("锚点那发起跑");
        settle_line(&root, &anchor, RunStatus::Succeeded, None);

        // t2 也够老，但那一行还没有结论：它是"钱花了没回来"的唯一证据
        let open = begun("t2", 800, "conv-open");
        append(&root, &open).expect("未了结那发起跑");

        assert_eq!(purge(&root, 5_000).expect("清除该成"), 2, "旧的那发连它派出去的那一发");

        let left: Vec<String> = runs(&root).into_iter().map(|line| line.run_id).collect();
        assert_eq!(
            left,
            vec!["run-t2-800".to_string(), "run-t1-1000".to_string()],
            "剩下的只有\"没跑完的那发\"与\"这个任务最新的那发\""
        );
        let text = fs::read_to_string(ledger_path(&root)).expect("账本该读得回来");
        assert!(!text.contains("run-t1-900"), "那一发的每一行都该跟着走，包括它派出去的那些");
        assert_eq!(
            last_starts(&root).get("t1").copied(),
            Some(1_000),
            "锚点一个字都不该动"
        );
        remove_tree(&root);
    }

    /// 读不动的那一行是这次清除**没有资格处置**的东西：把它解析成 `Line` 再写回去，
    /// 等于把"有一行写到一半被杀"洗成一条正常记录——那正是账本要留着它的理由
    #[test]
    fn the_purge_leaves_the_bytes_of_a_line_it_cannot_read() {
        let root = temp_dir("runs-purge-garbage");
        let old = begun("t1", 900, "conv-old");
        append(&root, &old).expect("旧发起跑");
        settle_line(&root, &old, RunStatus::Failed, None);
        let anchor = begun("t1", 1_000, "conv-new");
        append(&root, &anchor).expect("锚点那发起跑");
        settle_line(&root, &anchor, RunStatus::Succeeded, None);

        const HALF: &str = "{\"run_id\":\"run-t1-800\",\"task_id\":\"t1\",\"status\":\"runn";
        let mut text = fs::read_to_string(ledger_path(&root)).expect("账本该读得回来");
        text.push_str(HALF);
        text.push('\n');
        fs::write(ledger_path(&root), text).expect("落半行");

        // 必须真删掉一点什么：没东西可删时 purge 一个字都不重写，那条路径证不了这件事
        assert_eq!(purge(&root, 5_000).expect("清除该成"), 1, "只有旧的那一发该没");

        let after = fs::read_to_string(ledger_path(&root)).expect("账本该读得回来");
        assert!(after.contains(HALF), "那半行要按原字节留着");
        assert!(!after.contains("run-t1-900"), "旧的那发该被抹掉");
        assert!(after.contains("run-t1-1000"), "锚点那发一个字不动");
        remove_tree(&root);
    }

    /// 检查点行不能混进运行列表：一次四格的运行在列表里还是"一发"，
    /// 否则"这个任务跑过几次"就答错了，而这是这个模块存在的全部理由
    #[test]
    fn node_checkpoints_belong_to_their_run_instead_of_becoming_runs_of_their_own() {
        let root = temp_dir("runs-node-rows");
        let begun = begun("t1", 1_000, "conv-run");
        append(&root, &begun).expect("起跑行");
        for (index, node) in ["一", "二", "三"].iter().enumerate() {
            checkpoint(
                &root,
                &begun,
                node,
                &format!("conv-{index}"),
                1_000 + index as i64,
                RunStatus::Succeeded,
                None,
                None,
            )
            .expect("检查点该写得进去");
        }

        assert_eq!(runs(&root).len(), 1, "三行检查点不该变成三次运行");
        let shots = views(&root, None, 10);
        assert_eq!(shots.len(), 1);
        assert_eq!(
            shots[0].nodes.iter().map(|node| node.node_id.as_str()).collect::<Vec<_>>(),
            vec!["一", "二", "三"],
            "各格要跟着它所属的那一发回来，且保持账本里的先后"
        );
        assert_eq!(
            shots[0].nodes.iter().map(|node| node.conversation_id.as_str()).collect::<Vec<_>>(),
            vec!["conv-0", "conv-1", "conv-2"],
            "每一格用的是自己的话题，成本才归得清楚"
        );
        assert_eq!(done_nodes(&root, &begun.run_id).len(), 3);
        remove_tree(&root);
    }

    /// 契约守卫：账本投影的键要与 `types/chat.ts` 对得上。可选项一律填上值——
    /// 这个守卫比的是序列化出来的键，`None` 在它眼里根本不存在（`PlanView` 少两个字段
    /// 能全绿就是这个原因）
    #[test]
    fn a_delivery_never_rewrites_the_verdict_it_follows() {
        let root = temp_dir("runs-delivery");
        let begun = begun("t1", 1_000, "conv-1");
        append(&root, &begun).expect("begin");
        let done = finish(&root, &begun, RunStatus::Succeeded, None, None).expect("finish");
        note_delivery(
            &root,
            &done,
            crate::tasks::hook::Delivery {
                sent: false,
                status: 0,
                attempts: 3,
                note: "试了三次都没送达".into(),
            },
        )
        .expect("投递结果那行该写得进去");

        let history = runs(&root);
        assert_eq!(history.len(), 1, "补一行投递结果不该把一发变成两发");
        assert_eq!(
            history[0].status,
            RunStatus::Succeeded,
            "投递失败不许改动这一发的结论"
        );
        let shots = views(&root, None, 10);
        assert_eq!(shots[0].delivery.as_ref().map(|value| value.attempts), Some(3));
        assert_eq!(shots[0].status, RunStatus::Succeeded);
        remove_tree(&root);
    }

    /// 账本里那一行的"谁起的"必须写得出去也读得回来。读不回来的行不会报错，
    /// 它会变成一行"读不动"，连它是哪个任务的都不知道——新增一种主体时最容易踩的就是这个
    #[test]
    fn every_subject_a_run_can_be_started_by_survives_the_ledger() {
        for (who, name) in [
            (StartedBy::Scheduler, "scheduler"),
            (StartedBy::User, "user"),
            (StartedBy::Webhook, "webhook"),
        ] {
            let value = serde_json::to_value(first_line("t1", "conv-1", who, None))
                .expect("起跑行该能序列化");
            assert_eq!(value["startedBy"].as_str(), Some(name), "{who:?} 写出来的名字不对");
            let back: Line = serde_json::from_value(value).expect("同一行该读得回来");
            assert_eq!(back.started_by, who, "写出去读回来不该变成另一种主体");
            assert_eq!(back.task_id, "t1", "行读回来了却不认识任务，等于没读回来");
        }
        // 审计词汇里 webhook 仍算机器：那是"被引起的"，不是"人点的"
        assert_eq!(StartedBy::Webhook.actor(), crate::audit::Actor::Scheduler);
    }

    #[test]
    fn the_run_view_and_its_nodes_match_the_frontend_types() {
        let root = temp_dir("runs-contract");
        let begun = begun("t1", 1_000, "conv-run");
        append(&root, &begun).expect("起跑行");
        let cost = Cost {
            requests: 3,
            input_tokens: 100,
            output_tokens: 40,
            cached_tokens: 20,
            cost_usd_e8: 5,
            unpriced_requests: 1,
        };
        checkpoint(&root, &begun, "一", "conv-0", 1_000, RunStatus::Succeeded, None, Some(cost))
            .expect("检查点");
        finish(&root, &begun, RunStatus::Failed, Some("第二格没跑成".into()), Some(cost))
            .expect("收尾");

        let shots = views(&root, Some("t1"), 10);
        assert_eq!(shots.len(), 1);
        let value = serde_json::to_value(&shots[0]).expect("运行行该能序列化");
        crate::test_support::assert_matches_ts(&value, "TaskRun");
        crate::test_support::assert_matches_ts(&value["nodes"][0], "TaskRunNode");
        assert_eq!(value["costUsd"].as_f64(), Some(0.00000005));
        remove_tree(&root);
    }

    /// T07 的判据："第 3 步失败后 resume，只重跑 3 及下游"。
    /// 这里量的正是那一句：已成功的两格不能再回到计划里
    #[test]
    fn a_resumed_run_plans_only_what_the_ledger_does_not_already_show_as_done() {
        use crate::tasks::graph::{Node, TaskGraph};

        let root = temp_dir("runs-resume");
        let begun = begun("t1", 1_000, "conv-run");
        append(&root, &begun).expect("起跑行");
        let plan = TaskGraph {
            nodes: vec![
                Node { id: "一".into(), ..Node::default() },
                Node { id: "二".into(), depends_on: vec!["一".into()], ..Node::default() },
                Node { id: "三".into(), depends_on: vec!["二".into()], ..Node::default() },
                Node { id: "四".into(), depends_on: vec!["三".into()], ..Node::default() },
            ],
            on_failure: Default::default(),
        };

        // 第一次跑到第三格失败：前两格是 Succeeded，第三格 Failed，第四格根本没跑
        checkpoint(&root, &begun, "一", "conv-a", 1_000, RunStatus::Succeeded, None, None).unwrap();
        checkpoint(&root, &begun, "二", "conv-b", 2_000, RunStatus::Succeeded, None, None).unwrap();
        checkpoint(
            &root,
            &begun,
            "三",
            "conv-c",
            3_000,
            RunStatus::Failed,
            Some("服务商超时".into()),
            None,
        )
        .unwrap();

        let done: std::collections::BTreeSet<String> =
            done_nodes(&root, &begun.run_id).into_iter().collect();
        assert_eq!(done.len(), 2, "只有跑成的两格算已办：{done:?}");
        assert!(done.contains("一") && done.contains("二"));
        assert!(!done.contains("三"), "失败的那格不能算已办，否则续跑会跳过它");
        let first_ready: Vec<&str> =
            plan.ready(&done).unwrap().iter().map(|node| node.id.as_str()).collect();
        assert_eq!(first_ready, vec!["三"], "续跑要从失败那一格接上，不是从头再来");
        assert_eq!(
            plan.stranded(&done, &["三".to_string()].into_iter().collect())
                .unwrap(),
            vec!["四"],
            "被挡住的那一格要说得出是谁"
        );

        // 第三格这次跑成了：计划就该往前走，而不是继续把三端上来
        checkpoint(&root, &begun, "三", "conv-c2", 4_000, RunStatus::Succeeded, None, None).unwrap();
        let done: std::collections::BTreeSet<String> =
            done_nodes(&root, &begun.run_id).into_iter().collect();
        assert!(done.contains("三"), "同一格重跑过，赢的是最后一行");
        assert_eq!(
            plan.ready(&done).unwrap().iter().map(|node| node.id.as_str()).collect::<Vec<_>>(),
            vec!["四"]
        );
        // 第四格也跑完了：既没有可跑的，也没有被挡住的，驱动就该在这里停下来
        checkpoint(&root, &begun, "四", "conv-d", 5_000, RunStatus::Succeeded, None, None).unwrap();
        let done: std::collections::BTreeSet<String> =
            done_nodes(&root, &begun.run_id).into_iter().collect();
        assert!(plan.ready(&done).unwrap().is_empty(), "四格都跑完就不该还有可跑的");
        assert!(plan.stranded(&done, &done).unwrap().is_empty(), "四格都跑完就不该有剩下的");
        assert_eq!(done.len(), 4);
        remove_tree(&root);
    }

    /// 这条就是"把 remember 改回覆盖式"必须变红的那条：一次运行一个槽位是原缺陷本身
    fn child_of(parent: &str, run_id: &str, conversation_id: &str) -> Line {
        Line {
            run_id: run_id.to_string(),
            parent_run_id: Some(parent.to_string()),
            ..begun("t1", 2_000, conversation_id)
        }
    }

    fn cost(requests: i64, input_tokens: i64, cost_usd_e8: i64) -> Cost {
        Cost {
            requests,
            input_tokens,
            output_tokens: 1,
            cached_tokens: 0,
            cost_usd_e8,
            unpriced_requests: 0,
        }
    }

    #[test]
    fn a_child_run_costs_the_parent_once_and_only_the_parent_once() {
        let root = temp_dir("runs-rollup");
        let parent = {
            let mut line = begun("t1", 1_000, "");
            line.run_id = "run-parent".into();
            line
        };
        append(&root, &parent).expect("父的起跑行");
        // 交出去的那一格：检查点行用的就是子 run 那个话题
        checkpoint(
            &root,
            &parent,
            "一",
            "conv-sub",
            1_500,
            RunStatus::Succeeded,
            None,
            Some(cost(1, 10, 3)),
        )
        .expect("检查点");
        // 另一格是自己跑的：它没有对应的子 run
        checkpoint(
            &root,
            &parent,
            "二",
            "conv-own",
            1_600,
            RunStatus::Succeeded,
            None,
            Some(cost(1, 5, 2)),
        )
        .expect("自己跑的那一格");
        let delegated = child_of("run-parent", "run-sub", "conv-sub");
        append(&root, &delegated).expect("子 run 起跑行");
        finish(&root, &delegated, RunStatus::Succeeded, None, Some(cost(1, 10, 3)))
            .expect("子 run 收尾：同一笔钱，同一个话题");

        // 另一发子 run 没有对应的格子行（它是这一发里直接派出去的）
        let extra = child_of("run-parent", "run-extra", "conv-extra");
        append(&root, &extra).expect("第二发子 run 起跑行");
        finish(&root, &extra, RunStatus::Succeeded, None, Some(cost(2, 20, 7)))
            .expect("第二发子 run 收尾");

        let unrelated = begun("t2", 3_000, "conv-other");
        append(&root, &unrelated).expect("别人家的运行起跑行");
        finish(&root, &unrelated, RunStatus::Succeeded, None, Some(cost(9, 90, 9)))
            .expect("别人家的运行收尾");

        let sum = total_cost(&root, "run-parent").expect("父下发该有数");
        assert_eq!(
            (sum.requests, sum.cost_usd_e8),
            (4, 12),
            "少一截是错的，算两遍也是错的：{sum:?}"
        );
        assert_eq!(children_of(&root, "run-parent").len(), 2, "两发子 run 都该认这个父");
        assert_eq!(chain_len(&root, "run-sub"), 2, "子 run 在第二层");
        assert_eq!(chain_len(&root, "run-parent"), 1, "父自己那一发只有一层");
        assert!(total_cost(&root, "no-such-run").is_none(), "没跑过的运行不该有个零花钱的结论");

        // 派生视图：子 run 不算"这个任务又跑了一次"，但那一格要看得出是交出去的
        let shots = views(&root, Some("t1"), 10);
        assert_eq!(shots.len(), 1, "两发子 run 不该让这个任务看起来跑过三次");
        assert_eq!(
            shots[0]
                .nodes
                .iter()
                .map(|node| node.delegated)
                .collect::<Vec<_>>(),
            vec![true, false],
            "交出去的那一格要标出来，自己跑的那格不标"
        );
        assert_eq!(shots[0].nodes[0].conversation_id, "conv-sub", "去看子助理说了什么要靠它");

        finish(&root, &parent, RunStatus::Failed, Some("第三格没跑成".into()), Some(sum))
            .expect("父收尾");
        let cached = refresh_cache(&root);
        let held = cached.get("t1").expect("缓存要有这一格");
        assert_eq!(held.last_status, "error", "上次跑到什么样要认父那一发，不是认跑成了的子 run");
        assert_eq!(held.last_run_at, 1_000, "起跑时间是父那一发的，不是子 run 的");
        assert_eq!(last_starts(&root).get("t1"), Some(&1_000));
        remove_tree(&root);
    }

    #[test]
    fn two_runs_of_one_task_leave_two_records_not_just_the_latest_one() {
        let root = temp_dir("runs-two-runs");
        let first = begun("t1", 1_000, "conv-1");
        append(&root, &first).expect("第一行");
        settle_line(&root, &first, RunStatus::Succeeded, None);

        let second = begun("t1", 61_000, "conv-2");
        append(&root, &second).expect("第二行");
        settle_line(&root, &second, RunStatus::Failed, Some("服务商超时".into()));

        let history = runs_of(&root, "t1");
        assert_eq!(history.len(), 2, "跑过两次就得答得出两次，覆盖式写法只会留下一条");
        assert_eq!(history[0].status, RunStatus::Failed, "从新到旧：最近那次在前");
        assert_eq!(history[0].error.as_deref(), Some("服务商超时"));
        assert_eq!(history[1].status, RunStatus::Succeeded);
        assert_ne!(history[0].conversation_id, history[1].conversation_id, "两次运行各有各的话题");
        remove_tree(&root);
    }

    /// 没回来的一次运行不该被读成"没跑过"，也不该被读成跑完了
    #[test]
    fn a_run_that_never_came_back_reads_as_unfinished() {
        let root = temp_dir("runs-crashed");
        let line = begun("t1", 1_000, "conv-1");
        append(&root, &line).expect("起跑行");

        let history = runs_of(&root, "t1");
        assert_eq!(history.len(), 1);
        assert!(history[0].is_unfinished(), "进程被杀的那次要能看出来没跑完");
        assert_eq!(history[0].status, RunStatus::Running);
        assert_eq!(history[0].finished_at, None, "没收尾就不该有个收尾时间");
        assert!(refresh_cache(&root).get("t1").expect("缓存要有这一格").last_status.is_empty(),
            "还没结论的运行不能冒充跑成功过");
        remove_tree(&root);
    }

    #[test]
    fn a_torn_last_line_is_still_reported_as_a_run_that_was_happening() {
        let root = temp_dir("runs-torn");
        let line = begun("t1", 1_000, "conv-1");
        append(&root, &line).expect("起跑行");
        fs::OpenOptions::new()
            .append(true)
            .open(ledger_path(&root))
            .expect("开账本")
            .write_all(b"{\"runId\":\"run-t1-2000\",\"taskId\":\"t1\",\"startedBy\":")
            .expect("人为写半行");

        let history = runs(&root);
        assert_eq!(history.len(), 2, "半截行也是一次运行，静默丢掉它就成了没跑过");
        assert!(history[0].is_unfinished(), "读不动的那一行只能按没回来处理");
        assert!(history[0].error.as_deref().unwrap_or_default().contains("读不动"),
            "说不清是哪次运行，至少要说清这一行是被谁毁的");
        assert_eq!(history[1].run_id, line.run_id, "完整的那行还得照常读出来");
        remove_tree(&root);
    }

    /// 变异用：把 `append` 换成整份重写（也就是"一个任务一个槽位"的旧写法），这条必红
    #[test]
    fn an_earlier_run_line_survives_later_runs_byte_for_byte() {
        let root = temp_dir("runs-append-only");
        let first = begun("t1", 1_000, "conv-1");
        append(&root, &first).expect("第一行");
        let after_first = fs::read_to_string(ledger_path(&root)).expect("读回第一次的样子");

        let second = begun("t1", 61_000, "conv-2");
        append(&root, &second).expect("第二行");
        settle_line(&root, &second, RunStatus::Succeeded, None);
        let after_second = fs::read_to_string(ledger_path(&root)).expect("读回第二次的样子");

        assert!(
            after_second.starts_with(&after_first),
            "追加只能把新行接在后面，早先那行的字节一个都不能变"
        );
        assert_eq!(after_second.matches('\n').count(), 3, "起跑一行 + 第二次起跑一行 + 收尾一行");
        remove_tree(&root);
    }

    /// 缓存能整个从账本重建：这是"派生物不是真相"的可执行定义
    #[test]
    fn the_last_run_cache_rebuilds_from_the_ledger_alone() {
        let root = temp_dir("runs-cache");
        let old = begun("t1", 1_000, "conv-old");
        append(&root, &old).expect("起跑行");
        settle_line(&root, &old, RunStatus::Failed, Some("上次是失败的".into()));
        let fresh = begun("t1", 9_000, "conv-new");
        append(&root, &fresh).expect("起跑行");
        settle_line(&root, &fresh, RunStatus::Succeeded, None);

        fs::write(
            cache_path(&root),
            r#"{"t1":{"lastRunAt":1,"lastStatus":"ok","lastError":"","lastConversationId":"编的"}}"#,
        )
        .expect("先放一份说谎的缓存");

        let rebuilt = refresh_cache(&root);
        let cached = rebuilt.get("t1").expect("缓存里要有这个任务");
        assert_eq!(cached.last_run_at, 9_000, "上次运行取账本里最近那一次");
        assert_eq!(cached.last_status, "ok");
        assert_eq!(cached.last_conversation_id, "conv-new", "缓存说谎就要被账本纠正过来");

        fs::remove_file(cache_path(&root)).expect("删掉缓存");
        let again = refresh_cache(&root);
        assert_eq!(again.get("t1").expect("重建该给出同一格").last_conversation_id, "conv-new");
        remove_tree(&root);
    }

    #[test]
    fn a_parked_run_keeps_its_task_visible_in_the_cache() {
        let root = temp_dir("runs-waiting");
        let line = begun("t1", 1_000, "conv-1");
        append(&root, &line).expect("起跑行");
        settle_line(&root, &line, RunStatus::WaitingApproval, None);

        let cached = refresh_cache(&root);
        assert_eq!(cached.get("t1").expect("缓存要有").last_status, "waiting",
            "停在待审批既不算成功也不算失败，它得能被单独认出来");
        remove_tree(&root);
    }

    /// 归因成立的全部前提就是那个 id 是真的：空的conversation_id 会把无归属的请求整堆借走
    #[test]
    fn a_run_costs_what_its_own_conversation_consumed() {
        let dir = scoped_temp_dir("runs-cost");
        let file = dir.join("usage.db");
        let config = crate::config::AppConfig::default();
        let tokens = || Tokens {
            input: 1_000,
            output: 200,
            cached: Some(800),
            cache_write: 0,
            reasoning: 0,
        };
        usage::record(&file, &config, "chat", "conv-task", "m1", &tokens(), 0, false, 10, None, true, "", STAMP);
        usage::record(&file, &config, "chat", "conv-task", "m1", &tokens(), 0, false, 10, None, true, "", STAMP);
        usage::record(&file, &config, "task", "conv-other", "m1", &tokens(), 0, false, 10, None, true, "", STAMP);

        let conn = Connection::open(&file).expect("开台账");
        let cost = cost_of(&conn, "conv-task").expect("查询该成功").expect("两笔都在");
        assert_eq!(cost.requests, 2, "这个话题跑过两次请求，就该只算这两次");
        assert_eq!(cost.input_tokens, 2_000);
        assert_eq!(cost.cached_tokens, 1_600);
        assert_eq!(cost_of(&conn, "没有这条话题").expect("查询该成功"), None,
            "没记过账的话题不该被报成花了 0 元——那是'没数据'");
        assert_eq!(cost_of(&conn, "").expect("查询该成功"), None,
            "空 id 一旦能聚合，所有没归属的花费都会算到它头上，成本数字就再也没法信");
    }

    #[test]
    fn unpriced_requests_survive_a_zero_cost_run() {
        let dir = scoped_temp_dir("runs-unpriced");
        let file = dir.join("usage.db");
        let config = crate::config::AppConfig::default();
        let tokens = Tokens {
            input: 500,
            output: 100,
            cached: None,
            cache_write: 0,
            reasoning: 0,
        };
        usage::record(&file, &config, "task", "conv-task", "没定价的模型", &tokens, 0, false, 10, None, true, "", STAMP);

        let conn = Connection::open(&file).expect("开台账");
        let cost = cost_of(&conn, "conv-task").expect("查询该成功").expect("有一笔在");
        assert_eq!(cost.cost_usd_e8, 0, "没有价格表就是算不出钱");
        assert_eq!(cost.unpriced_requests, 1,
            "算不出钱与没花钱必须分得开，否则任务详情页会报一个虚构的零");
    }
}
