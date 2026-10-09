//! 编排器：一次 plan 的统一入口，也是它唯一的**事实生产者**。
//!
//! 形状是这样：账本（`orchestra/<plan_id>.jsonl`）是事实，内存里的节点状态是它的投影，
//! 前端的 DAG 与成本面板读的是派生值。节点 = 一次 `chat::run_turn_into`，
//! 编排器自己不碰模型、不碰文件、不碰审批——它只决定"什么时候、以什么能力面、跑哪一次"。
//!
//! 两条不能省的原则：
//! - **恢复只读账本，不重跑已经 Done 的节点**；
//! - **回滚不在这**：编排器不撤销副作用（那是 `edits.rs` 的 revert），账本只说清
//!   "这一步用的是哪一段上下文"，界面上不许写"一键回滚整个计划"。

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use crate::audit::{self, Actor, Outcome};
use crate::chat::{self, ChatEvent, EventSink, EmitSink, ToolStatus};
use crate::config::SubagentDef;
use crate::orchestra::graph::{
    followups, replan, verdict_key, verdict_value, Edge, EdgeKind, Node, Plan, SUPERVISOR,
};
use crate::orchestra::judge::{self, Check, Contribution, Merge, Verdict};
use crate::orchestra::profile::AgentProfile;
use crate::orchestra::runtime::{
    report_bodies, Assignment, Blackboard, Cas, Entry, Exchanges, Permits, Pool, Scheduler,
};
use crate::quota::{Denied, Priority, Quota};

/// 节点状态。`Blocked` 与 `Skipped` 是分开的两件事：
/// 前者是"策略没让跑"（熔断、预算），后者是"轮不到它"（上游被取消或失败）。
/// 一律写成 Failed 会让事后复盘看不出到底哪里出了问题
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NodeStatus {
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
    WaitingApproval,
    Blocked,
    Canceled,
    /// 循环边的"这一轮跑完了，循环还开着"。它**不是**落定状态：`ready_set` 因此还会把它派出去，
    /// 而它也不算 `succeeded()`，所以下游不会提前放行。没有这一格，循环节点跑完第一轮就变成
    /// `Done`（落定），于是 `Edge::Loop` 永远只能跑一轮——那正是它此前的样子
    Iterated,
}

impl NodeStatus {
    /// 落定状态：不会再自己变回去。恢复时读到的就是这些。
    /// `Iterated` 故意不在里面——它就是"还没落定，下一轮还要被派"
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            NodeStatus::Done
                | NodeStatus::Failed
                | NodeStatus::Skipped
                | NodeStatus::Canceled
                | NodeStatus::WaitingApproval
        )
    }

    pub fn succeeded(self) -> bool {
        self == NodeStatus::Done
    }
}

/// 账本里的一行。只记标识与结果：**产出正文不进账本**——
/// 要看那一次答了什么，拿 `conversation_id` 回话题日志去读
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceRow {
    pub ts_ms: u64,
    pub plan_id: String,
    pub node: String,
    pub attempt: u8,
    /// queued / started / finished / failed / escalated / blocked / canceled / merged / retry
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<NodeStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    /// 这一发的墙钟耗时。以前只有计划级一个总数，于是"验收第 5 条：每个 agent 的耗时"
    /// 在账本里根本没有依据，界面上那个数只能不报
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// 这一发的花费，单位 1e-8 美元（用量台账那一份的原样）。`None` = 这个模型没价表，
    /// 不是"免费"——两者在界面上必须分开说
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_e8: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub fn orchestra_root(app: &AppHandle) -> Result<PathBuf, String> {
    let root = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("拿不到应用数据目录：{error}"))?
        .join("orchestra");
    fs::create_dir_all(&root).map_err(|error| format!("创建编排目录失败：{error}"))?;
    Ok(root)
}

/// plan_id 会变成文件名，先压成安全字符（与 `tasks.rs` 的 `safe_id` 同一条理由）
pub fn ledger_name(plan_id: &str) -> String {
    let safe: String = plan_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    format!("{safe}.jsonl")
}

pub fn ledger_path(root: &Path, plan_id: &str) -> PathBuf {
    root.join(ledger_name(plan_id))
}

pub fn append_rows(path: &Path, rows: &[TraceRow]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| format!("打不开运行账本：{error}"))?;
    for row in rows {
        let line = serde_json::to_string(row).map_err(|error| error.to_string())?;
        // 一次 write_all：并发的追加若分成两次写，两条记录会粘在同一行上
        let mut record = line.into_bytes();
        record.push(b'\n');
        file.write_all(&record).map_err(|error| format!("写账本失败：{error}"))?;
    }
    file.sync_all().map_err(|error| format!("账本没落盘：{error}"))?;
    Ok(())
}

/// 读账本。坏行跳过不致命：一行写坏不该让整份计划看起来从未跑过
pub fn read_rows(path: &Path) -> Vec<TraceRow> {
    let Ok(text) = fs::read_to_string(path) else { return Vec::new() };
    text.lines().filter_map(|line| serde_json::from_str::<TraceRow>(line).ok()).collect()
}

/// 起跑那一刻的完整快照：图 + 这张图该怎么汇合。恢复要的正是这两样，缺一不可。
///
/// `merge` 不在 `Plan` 里（它是装配那一次的决定），而 `debate` 与 `bestOf` 的图形状一样、
/// 汇合规则不同——少这一格就只能靠"看节点像什么形状"去猜，那是第二个真相
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Checkpoint {
    pub plan: Plan,
    pub merge: Merge,
}

/// 那条 checkpoint 行本身。写它的一方与恢复读它的一方**共用这个构造**：
/// 事件名或 `node` 只要有一处飘了，恢复就安静地看不见这份计划（比报错更难查）
fn planned_row(plan: &Plan, merge: &Merge) -> Result<TraceRow, String> {
    Ok(TraceRow {
        ts_ms: now_ms(),
        plan_id: plan.id.clone(),
        node: PLAN_SCOPE.into(),
        attempt: 0,
        event: "planned".into(),
        status: None,
        tokens: None,
        duration_ms: None,
        cost_e8: None,
        conversation_id: None,
        detail: Some(
            serde_json::to_string(&Checkpoint { plan: plan.clone(), merge: merge.clone() })
                .map_err(|error| format!("图没能编码：{error}"))?,
        ),
    })
}

/// 账本里那条 `planned` 行（最后一条说话）。读不出来就是恢复不了：这份账本早于
/// checkpoint 机制，宁可让这份计划留在界面上看不见，也不凭节点名猜一张图出来
pub fn checkpoint_of(rows: &[TraceRow]) -> Option<Checkpoint> {
    rows.iter()
        .rev()
        .filter(|row| row.event == "planned")
        .find_map(|row| row.detail.as_ref())
        .and_then(|text| serde_json::from_str::<Checkpoint>(text).ok())
}

/// 从账本重放出节点状态。**恢复只靠这一个函数**，别处不再算一遍"上次跑到哪"
pub fn derive_status(rows: &[TraceRow]) -> HashMap<String, NodeStatus> {
    let mut status: HashMap<String, NodeStatus> = HashMap::new();
    for row in rows {
        if let Some(current) = row.status {
            status.insert(row.node.clone(), current);
        }
    }
    status
}

/// 这一行说的是"某一格落定了"，以及它那一次的校验结论。
///
/// 结论没有另存一格：它编在 `detail` 里——有内容就是没过（那句内容正是"为什么没过"），
/// 没有就是过了。写它的一方是 `run_node` 末尾那一行，读它的一方是 [`seed_verdicts`]，
/// 两边都问这一个问题，不再各写一次 `detail.is_some()`
fn settled_verdict(row: &TraceRow) -> Option<&'static str> {
    match row.event.as_str() {
        "finished" | "iterated" => Some(verdict_value(row.detail.is_none())),
        _ => None,
    }
}

/// 恢复时把每一格的校验结论**从账本现推**回黑板。黑板是进程内的，重启之后它是空的；
/// 不补这一步，一条"上游没过校验我才跑"的边会永远等不到，而账本里明明写着它没过。
///
/// 这不是第三份状态：结论本来就落在这些行上，这里只是把它读回它该在的地方
fn seed_verdicts(board: &Blackboard, rows: &[TraceRow]) {
    for row in rows {
        let Some(value) = settled_verdict(row) else { continue };
        let key = verdict_key(&row.node);
        board.compare_swap(&key, board.version_of(&key), value, "ledger");
    }
}

pub fn derive_attempts(rows: &[TraceRow]) -> HashMap<String, u8> {
    let mut attempts: HashMap<String, u8> = HashMap::new();
    for row in rows {
        if row.event == "started" {
            let held = attempts.entry(row.node.clone()).or_insert(0);
            *held = (*held).max(row.attempt);
        }
    }
    attempts
}

/// 每一格到现在为止的读数：token、墙钟毫秒、花费（1e-8 美元）、以及"这一格有没有价表"。
///
/// 判据是"这一行带着 `tokens`"——那正是 `run_node` 每次调用落下的一行终结行（起跑行不带数字），
/// 所以重跑与多轮循环各贡献一行，加起来就是这一格的总账。**不在 `RunState` 里再存一份**：
/// 那份只能答"这一刻"，重启之后就没了，而验收问的是"这个 agent 跑了多久、花了多少"
pub fn node_readout(rows: &[TraceRow]) -> HashMap<String, NodeReadout> {
    let mut out: HashMap<String, NodeReadout> = HashMap::new();
    for row in rows {
        // 计划级那一行（`blocked`）也带着 `tokens`，那是**整份计划**到那一刻的累计，
        // 不是一格的产出。不认出来，界面上就会凭空多出一个叫「(plan)」的工人
        if row.node == PLAN_SCOPE {
            continue;
        }
        let Some(tokens) = row.tokens else { continue };
        let held = out.entry(row.node.clone()).or_default();
        held.tokens += tokens;
        held.duration_ms += row.duration_ms.unwrap_or(0);
        if let Some(cost) = row.cost_e8 {
            held.cost_e8 += cost;
            held.priced = true;
        }
    }
    out
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NodeReadout {
    pub tokens: u64,
    pub duration_ms: u64,
    pub cost_e8: i64,
    pub priced: bool,
}

/// 每一格动过几个文件。节点与文件的对应不猜：**账本行带着 `conversation_id`**，
/// 而改动账本按话题记账，所以这一格是两本账现拼出来的派生值。
///
/// 一个节点多次尝试时按**去重后的文件集合**合起来——同一个文件被两次尝试各改一次，
/// 屏幕上说"1 个文件"而不是 2；那两次改的是同一处地方，报两个数是在虚报工作量
pub fn node_edits(
    rows: &[TraceRow],
    tallies: &std::collections::BTreeMap<String, crate::edits::EditTally>,
) -> HashMap<String, (usize, bool)> {
    let mut merged: HashMap<String, (std::collections::BTreeSet<String>, bool)> = HashMap::new();
    for row in rows.iter().filter(|row| row.node != PLAN_SCOPE) {
        let Some(conversation) = &row.conversation_id else { continue };
        let Some(tally) = tallies.get(conversation) else { continue };
        let entry = merged.entry(row.node.clone()).or_default();
        entry.0.extend(tally.files.iter().cloned());
        entry.1 |= tally.snapshotted;
    }
    merged
        .into_iter()
        .map(|(node, (files, snapshotted))| (node, (files.len(), snapshotted)))
        .collect()
}

/// 每一格**最新那一发**住在哪个话题里。
///
/// 回滚那条路只认话题 id（`edits::edit_revert(app, conversation_id, path)`），而节点行上
/// 那句"有快照可回"如果没有这一格，就是一句没有出口的声明：账上确实留着快照，
/// 界面上却没有任何地方能把用户带到那个话题。取尝试号最大的那一发——重跑与循环留下的
/// 就是它，要回的也是它；同一尝试里后写的行覆盖先写的（账本按时间追加）
pub fn node_conversations(rows: &[TraceRow]) -> HashMap<String, String> {
    let mut latest: HashMap<String, (u8, String)> = HashMap::new();
    for row in rows.iter().filter(|row| row.node != PLAN_SCOPE) {
        let Some(conversation) = &row.conversation_id else { continue };
        if latest
            .get(&row.node)
            .is_some_and(|(attempt, _)| *attempt > row.attempt)
        {
            continue;
        }
        latest.insert(row.node.clone(), (row.attempt, conversation.clone()));
    }
    latest
        .into_iter()
        .map(|(node, (_, conversation))| (node, conversation))
        .collect()
}

/// 这一格最后那一发是不是**降级收下**的（有产出、但没过形状校验）。
///
/// 判据本来就在账上，界面上一直没有：`finished` 那一行的 `detail` 写的就是没过校验的"为什么"
/// （`Verdict::Pass` 时那一格是空的）。验收第 2 条那句"可重试或降级"里，重试看得见（`#retry`、
/// 第 N 次尝试），降级却只会显示成"完成"——一个没过校验的节点与一个干净的节点在面板上长得一样，
/// 等于把"这份不合格"那句花过钱买回来的话吞掉了
pub fn node_degradations(rows: &[TraceRow]) -> HashMap<String, String> {
    // 先挑出"每一格最新那一次终局"，再问它带没带"为什么没过"。顺序反了就错：
    // 只扫那些带着理由的行，重跑过了校验的那一发（detail 是空的）就清不掉上一次留下的降级
    // ——第一版正是这样，被 `a_rerun_that_passed_clears_the_degradation` 当场拦下
    let mut latest: HashMap<String, (u8, Option<String>)> = HashMap::new();
    for row in rows
        .iter()
        .filter(|row| row.node != PLAN_SCOPE && row.event == "finished")
    {
        // 只看 `finished`：`started` 行的 detail 装的是"这一发吃了谁的材料"，另一种意思；
        // `failed` 行那一格是"为什么没成"，面板本来就写着"失败"，不该再标一次降级
        if latest
            .get(&row.node)
            .is_some_and(|(attempt, _)| *attempt > row.attempt)
        {
            continue;
        }
        latest.insert(row.node.clone(), (row.attempt, row.detail.clone()));
    }
    latest
        .into_iter()
        .filter_map(|(node, (_, why))| why.map(|reason| (node, reason)))
        .collect()
}

/// 被 CAS 顶回来的那一次写 → 账本一行；写成功（`Applied`）**没有要说的事**，所以是 `None`。
///
/// 为什么这一行必须存在：黑板是内存里的，进程一停，"这两份意见打过架、谁赢了、输的那份去哪了"
/// 就整个消失——而那是已经花过钱的事实。§5 表 M04 那半句"Trace 里有覆盖关系"等的就是这一行
fn conflict_row(plan_id: &str, node: &str, attempt: u8, cas: &Cas) -> Option<TraceRow> {
    let Cas::Conflict { held, holder, lost_key } = cas else { return None };
    Some(TraceRow {
        ts_ms: now_ms(),
        plan_id: plan_id.to_string(),
        node: node.to_string(),
        attempt,
        event: "conflict".into(),
        // 不带 status：这一格的终局由它自己那一行说，这里再写一份就是两个真相；
        // 也不带 tokens/cost_e8：派生视图按行累加，带一份就是把同一笔钱数两次
        status: None,
        tokens: None,
        duration_ms: None,
        cost_e8: None,
        conversation_id: None,
        detail: Some(format!(
            "这一格的结论被顶回来：现在这一格是 v{held}（作者 {holder}），你那一份另存在 {lost_key}"
        )),
    })
}

/// 面板那一格"冲突双留 N 次"的出处：**账本**，不是内存里那份计数。
/// 于是重启之后这一发仍然答得出打过几次架；而内存那份只留着给 `#lost-N` 编号
fn conflict_count(rows: &[TraceRow]) -> usize {
    rows.iter().filter(|row| row.event == "conflict").count()
}

/// 就绪集：上游全部成功、自己还没落定、条件边成立的那些节点。
///
/// "这个节点已经跑过几轮"住在哪一格。写它的一方（`run_node` 记一轮）与读它的一方
/// （[`loop_open`]）共用这个名字。以前是**两个键**：轮数写在 `until_key` 上、
/// 读却在 `{node}#iters` 上——等于两个人各数各的轮数，谁也不会发现对不上
pub fn iterations_key(node_id: &str) -> String {
    format!("{node_id}#iters")
}

/// 这一条循环边还开不开。**唯一的判据**：`ready_set` 用它决定要不要再派发，
/// `run_node` 用它决定这一轮定成 `Iterated` 还是 `Done`。两处各算一次，就会出现
/// "一个说该跑、一个说不该跑"——那份循环要么死锁要么 runaway
///
/// 两条停止条件：`until_key` 被谁写成了 `until_value`（那是一个**别人**下的决定，
/// 比如裁判节点），或者 `#iters` 用完了 `max_iters`（成本天花板，必填就是为了让它兜得住）
pub fn loop_open(node: &Node, board: &Blackboard) -> bool {
    let Edge::Loop { until_key, until_value, max_iters } = &node.edge else {
        return false;
    };
    if board.get(until_key).is_some_and(|entry| entry.value == *until_value) {
        return false;
    }
    let used = board
        .get(&iterations_key(&node.id))
        .and_then(|entry| entry.value.parse::<u8>().ok())
        .unwrap_or(0);
    used < *max_iters
}

/// 条件边读黑板上的结论键——"要不要跑下游"由此成为一件看得见、能复盘的事实，
/// 而不是藏在某个节点提示词里的一句"如果……就跳过"
pub fn ready_set(plan: &Plan, status: &HashMap<String, NodeStatus>, board: &Blackboard) -> Vec<String> {
    let mut ready: Vec<String> = Vec::new();
    for node in &plan.nodes {
        if status.get(&node.id).map(|held| held.is_terminal()).unwrap_or(false) {
            continue;
        }
        let satisfied = node
            .depends_on
            .iter()
            .all(|upstream| status.get(upstream).map(|held| held.succeeded()).unwrap_or(false));
        if !satisfied {
            continue;
        }
        let gated = match &node.edge {
            Edge::Conditional { key, equals } => {
                board.get(key).map(|entry| entry.value == *equals).unwrap_or(false)
            }
            // 循环边：判据只在 [`loop_open`] 那一处，跑完一轮怎么定状态读的也是它
            Edge::Loop { .. } => loop_open(node, board),
            _ => true,
        };
        if gated {
            ready.push(node.id.clone());
        }
    }
    ready
}

/// 级联：上游没成功（失败/取消/在等人）时，下游是 `Skipped` 而不是 `Failed`——
/// 它自己没做错任何事
pub fn cascade(plan: &Plan, status: &HashMap<String, NodeStatus>) -> Vec<(String, NodeStatus)> {
    let mut changes = Vec::new();
    for node in &plan.nodes {
        if status.contains_key(&node.id) {
            continue;
        }
        let stuck = node.depends_on.iter().any(|upstream| {
            matches!(
                status.get(upstream),
                Some(NodeStatus::Canceled) | Some(NodeStatus::Failed) | Some(NodeStatus::WaitingApproval)
            )
        });
        if stuck {
            changes.push((node.id.clone(), NodeStatus::Skipped));
        }
    }
    changes
}

/// 一次 run 的观察结果。`observe` 是纯函数，所以"没人应答的审批"这件事能被单测
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Tally {
    pub text: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub duration_ms: u64,
    pub error: Option<String>,
    /// 弹过但还没有结论的审批 id：非空就等于"有操作被拦住了，而这里没人在场"
    pub unanswered: Vec<String>,
}

pub fn observe(tally: &mut Tally, event: &ChatEvent) {
    match event {
        ChatEvent::Delta { text } => tally.text.push_str(text),
        ChatEvent::Done { input_tokens, output_tokens, duration_ms, cached_tokens, .. } => {
            tally.input_tokens += *input_tokens as u64;
            tally.output_tokens += *output_tokens as u64;
            tally.duration_ms += *duration_ms;
            tally.cached_tokens += cached_tokens.unwrap_or(0) as u64;
        }
        ChatEvent::Error { message } => tally.error = Some(message.clone()),
        ChatEvent::Tool { id, status, .. } => match status {
            ToolStatus::Pending => {
                if !tally.unanswered.contains(id) {
                    tally.unanswered.push(id.clone());
                }
            }
            ToolStatus::Running | ToolStatus::Done | ToolStatus::Failed | ToolStatus::Denied => {
                tally.unanswered.retain(|held| held != id);
            }
        },
        _ => {}
    }
}

/// 事件既转发给界面，又替编排器记下这一支的产出与成本。
/// 少了这一层，编排器要么读不到节点答了什么，要么自己去重放话题日志——那是第二份真相
struct Recorder {
    forward: EmitSink,
    tally: Mutex<Tally>,
}

impl EventSink for Recorder {
    fn send(&self, event: ChatEvent) {
        observe(&mut self.tally.lock().unwrap_or_else(PoisonError::into_inner), &event);
        self.forward.send(event);
    }
}

impl Recorder {
    fn new(app: &AppHandle, conversation_id: &str) -> Self {
        Self { forward: EmitSink::new(app, conversation_id), tally: Mutex::new(Tally::default()) }
    }

    fn take(&self) -> Tally {
        std::mem::take(&mut *self.tally.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// 熔断。它改的是"要不要派发"，不是"要不要杀掉正在跑的请求"——
/// 后者在服务商侧照样计费，还会留下一段半截话题
#[derive(Debug)]
pub struct Breaker {
    threshold: usize,
    open_for: Duration,
    consecutive: AtomicUsize,
    open_until: Mutex<Option<Instant>>,
}

impl Breaker {
    pub fn new(threshold: usize, open_for: Duration) -> Self {
        Self {
            threshold: threshold.max(1),
            open_for,
            consecutive: AtomicUsize::new(0),
            open_until: Mutex::new(None),
        }
    }

    /// `now` 由调用方给，是为了让"到点了该放一个探针出去"这件事能被测试
    pub fn allows(&self, now: Instant) -> bool {
        match *self.open_until.lock().unwrap_or_else(PoisonError::into_inner) {
            Some(until) => now >= until,
            None => true,
        }
    }

    /// 返回"这一次是否把闸推开了"
    pub fn record_failure(&self) -> bool {
        let next = self.consecutive.fetch_add(1, Ordering::AcqRel) + 1;
        if next >= self.threshold {
            *self.open_until.lock().unwrap_or_else(PoisonError::into_inner) =
                Some(Instant::now() + self.open_for);
            self.consecutive.store(0, Ordering::Release);
            return true;
        }
        false
    }

    pub fn record_success(&self) {
        self.consecutive.store(0, Ordering::Release);
        *self.open_until.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// 断路器的开合可见面。生产侧今天没有读它的地方（开路的行为直接体现在
    /// `can_accept` 的拒绝里），只有守卫测试拿它断言开/合两态——
    /// 注意这里**不能**用 `#[cfg(test)]`：本文件被 `two_of_the_five_bus_channels…`
    /// 守卫测试按“第一个 #[cfg(test)] 之前”切成生产面做断言，中途插一个会
    /// 把后面的生产代码误划出生产面，正向断言当场红
    #[allow(dead_code)]
    pub fn is_open(&self) -> bool {
        self.open_until
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .map(|until| until > Instant::now())
            .unwrap_or(false)
    }

    /// Trace 里那一句由熔断器自己说。以前"熔断 60 秒"是写在调用点的一个字面量，
    /// 而 60 这个数住在 `Breaker::new` 里——把窗口调一次，账本上那句就成了假话，
    /// 且没有任何测试会红。同一判据也用在失败次数上
    pub fn notice(&self, profile: &str) -> String {
        format!(
            "档案「{profile}」连着失败 {} 次，熔断 {} 秒：这期间它的节点不派发",
            self.threshold,
            self.open_for.as_secs()
        )
    }
}

/// 令牌桶限流：数字自己数，因为没有运行时替我们数
#[derive(Debug)]
pub struct TokenBucket {
    capacity: u64,
    per_second: f64,
    state: Mutex<(f64, Instant)>,
}

impl TokenBucket {
    pub fn new(capacity: u64, per_second: f64) -> Self {
        Self {
            capacity,
            per_second: per_second.max(0.001),
            state: Mutex::new((capacity as f64, Instant::now())),
        }
    }

    /// 只放大容量，不缩回：后加入的计划并发更高时桶跟着变宽，更低时不把别人已经攒到的余量抽走。
    /// 这条规则让共享桶的容量**与到达顺序无关**
    pub fn grow_to(&mut self, capacity: u64) {
        self.capacity = self.capacity.max(capacity);
    }

    pub fn try_take(&self, amount: u64, now: Instant) -> bool {
        let mut guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let (held, then) = &mut *guard;
        let elapsed = now.duration_since(*then).as_secs_f64().max(0.0);
        // 回补按时间算，绝不超过容量：攒一晚上的令牌换来一次突发，那不是限流
        *held = (*held + elapsed * self.per_second).min(self.capacity as f64);
        *then = now;
        if *held >= amount as f64 {
            *held -= amount as f64;
            true
        } else {
            false
        }
    }
}

/// 限流突发容量：一只桶攒到"碰过它的那些计划里最大的并发上限 × 4"为止。
/// 4 与旧的那只计划级桶同源（上限 1 的计划允许一次 4 发的突发）
pub const RATE_BURST_PER_SLOT: u64 = 4;
/// 每秒回补半个，即一分钟约 30 发 + 突发。以前它是 `plan_bucket` 里的就地字面量
pub const RATE_REFILL_PER_SEC: f64 = 0.5;

/// 按**服务商档案**分的限流桶，整个进程一份。
///
/// 键是 `AppConfig.active_profile_id`（空串 = 没选档案、走顶层那套字段），**不是**编排里的角色名：
/// 429 是账号给的额度，两个角色打的是同一个账号，按角色分桶等于把请求速率乘上角色数——
/// 那正是限流要做反的事。反过来，切到另一个档案就是另一个账号，让它陪上一个账号共用一只桶只是白白挨限。
/// 容量取"碰过这只桶的计划里最大的那个并发上限 × [`RATE_BURST_PER_SLOT`]"，只放大不缩回，
/// 所以它不随到达顺序变；每一份计划的并发本来另由 `permits` 与全局 [`Quota`] 管着，
/// 这只桶只管"这个账号每秒挨多少发"
#[derive(Debug, Default)]
pub struct Rates {
    buckets: Mutex<HashMap<String, TokenBucket>>,
}

impl Rates {
    /// 这一发准不准走。`slots` = 调用方那份计划的并发上限，只用来定容量
    pub fn permit(&self, endpoint: &str, slots: usize, now: Instant) -> bool {
        let want = (slots.max(1) as u64).saturating_mul(RATE_BURST_PER_SLOT);
        let mut guard = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
        let bucket = guard
            .entry(endpoint.to_string())
            .or_insert_with(|| TokenBucket::new(want, RATE_REFILL_PER_SEC));
        bucket.grow_to(want);
        bucket.try_take(1, now)
    }
}

#[derive(Debug, Clone, Default)]
pub struct RunState {
    pub status: HashMap<String, NodeStatus>,
    pub spent_tokens: u64,
    pub spent_duration_ms: u64,
    /// 这份计划到现在花了多少（1e-8 美元）。它是账本的投影，与 `spent_tokens` 同一种东西：
    /// 预算里那一格「花费」以前收到的是常量 0，所以 $5 那道闸从来没顶住过一次
    pub spent_cost_e8: i64,
    /// 谁在等审批。"有几个节点在等你"必须可见——不可见的等待状态是缺陷
    pub waiting: Vec<(String, String)>,
    pub merged: Option<String>,
    pub blocked_by: Option<String>,
}

/// 本轮看到的图。写它的只有一个地方（驱动 replan 时），读的人各拿一份快照——
/// 一份真相、多个读数。派发出去的节点跑的就是它被派发那一刻的那份定义，
/// 之后图再怎么长都影响不到它
fn plan_of(plan: &Arc<Mutex<Plan>>) -> Plan {
    plan.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

pub struct Handle {
    pub app: AppHandle,
    pub plan_id: String,
    /// 跑到一半会被 replanning 追加节点，所以它是共享可变的。只追加：
    /// 已有节点的定义一个字都不改
    pub plan: Arc<Mutex<Plan>>,
    pub profiles: Arc<HashMap<String, AgentProfile>>,
    pub cancel: Arc<AtomicBool>,
    pub pause: Arc<AtomicBool>,
    pub state: Arc<Mutex<RunState>>,
    pub board: Arc<Blackboard>,
    pub permits: Arc<Permits>,
    /// 跨 plan 的全局并发额度，整个进程一份。`permits` 管的是"这一份计划里几路"，
    /// 它管不到"这台机器上此刻几路"——三份各开 4 路就是 12 路真金白银的并发请求
    pub quota: Arc<Quota>,
    pub pool: Arc<Pool>,
    pub ledger: Arc<PathBuf>,
    driver: Mutex<Option<JoinHandle<()>>>,
}

impl Handle {
    pub fn status(&self) -> RunState {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    pub fn in_flight(&self) -> usize {
        self.permits.in_flight()
    }

    pub fn is_paused(&self) -> bool {
        self.pause.load(Ordering::Acquire)
    }

    pub fn is_canceled(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }

    pub fn is_finished(&self) -> bool {
        self.driver
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(JoinHandle::is_finished)
            .unwrap_or(true)
    }

    /// 等驱动线程收工。测试与"取消后要能读到最终状态"都用它
    pub fn join(&self) {
        if let Some(join) = self.driver.lock().unwrap_or_else(PoisonError::into_inner).take() {
            let _ = join.join();
        }
    }
}

struct NodeOutcome {
    node: String,
    status: NodeStatus,
    contribution: Option<Contribution>,
    failed: bool,
}

/// 上游交给这一发的材料：按 `depends_on` 的次序从黑板取结论，返回
/// `(拼进提示词的那一段, 给账本看的那几个戳)`。没有依赖时返回 `None`（不硬造一段空话）。
///
/// 取不到的那一条也照样写进去（"没有产出可交"）——安静地少一份材料，下游就以为自己看全了，
/// 而那正是一条流水线退化成几次独立问答的方式。戳里带版本，是为了循环与重跑时
/// "这一发吃的是哪一版"问得出来（§5.11）
fn handoff(plan: &Plan, node_id: &str, board: &Blackboard) -> Option<(String, Vec<String>)> {
    let node = plan.find(node_id)?;
    if node.depends_on.is_empty() {
        return None;
    }
    let mut prompt = String::from("\n\n——上游交给这一发的产出——");
    let mut stamps: Vec<String> = Vec::new();
    for upstream in &node.depends_on {
        match board.get(upstream) {
            Some(entry) => {
                prompt.push_str(&format!(
                    "\n\n【来自「{upstream}」v{}】\n{}",
                    entry.version, entry.value
                ));
                stamps.push(format!("{upstream} v{}", entry.version));
            }
            None => {
                prompt.push_str(&format!(
                    "\n\n【来自「{upstream}」】没有产出可交（它没跑成、被跳过，或没往黑板写过结论）"
                ));
                stamps.push(format!("{upstream} (无)"));
            }
        }
    }
    Some((prompt, stamps))
}

/// 跑一个节点：档案的角色 + 这一步的目标 → 一次正常回合 → 校验 → 写黑板。
/// 不合格就带着意见再跑一次，尝试次数用完就是 Failed
fn run_node(
    app: &AppHandle,
    plan: &Plan,
    node_id: &str,
    first_attempt: u8,
    profile: &AgentProfile,
    board: &Arc<Blackboard>,
    state: &Arc<Mutex<RunState>>,
    ledger: &Path,
    cancel: &Arc<AtomicBool>,
) -> NodeOutcome {
    let Some(node) = plan.find(node_id) else {
        return NodeOutcome {
            node: node_id.into(),
            status: NodeStatus::Failed,
            contribution: None,
            failed: true,
        };
    };
    // 形状检查由计划带着走（`build_plan` 从请求里搬进来）。这一格以前是写死的
    // `Check { min_chars: 1, ..Default::default() }`：那份规格在 plan 上没人读，
    // `must_contain` / `forbid` 于是永远为空，`forbid` 上那句"不是装饰"也没人执行
    let check = plan.check.clone();
    // 档案里那一行权限要真的进得了闸门：登记到这份话题的作用域上，
    // 回合体那一侧的判定读的就是这一张。它只能比全局更严（见 AgentProfile::policy_under）
    let global = crate::config::load(app).active_policy();
    let scoped = profile.policy_under(&global);
    let attempts = node.max_attempts.max(1).max(first_attempt);
    let mut feedback: Option<String> = None;
    let mut tokens = 0u64;
    let mut duration_ms = 0u64;
    // 钱要从用量台账现取：那是唯一记了"这一发实际花多少"的地方。`cost_seen` 分清
    // "量到 0"（有价表、没花钱）与"没量到"（台账里没这一发）——把它们混成 $0 是假话
    let mut cost_e8 = 0i64;
    let mut cost_seen = false;
    let mut text = String::new();
    let mut unanswered: Vec<String> = Vec::new();
    let mut verdict = Verdict::Fail { why: "还没有跑过".into() };

    for attempt in first_attempt..=attempts {
        // 每一格自己的预算闸。它挡的是**下一次尝试**，不动已经在跑的那一发——与并发位、
        // 暂停、熔断同一条规矩。第一发永远放行：一进门就被顶住的那一格是数字配错了，
        // 不是这一格真的值那么多钱
        if attempt > first_attempt {
            if let Some(which) = profile.budget.exhausted_per_task(tokens, cost_e8) {
                append_rows(
                    ledger,
                    &[TraceRow {
                        ts_ms: now_ms(),
                        plan_id: plan.id.clone(),
                        node: node_id.to_string(),
                        attempt,
                        event: "blocked".into(),
                        // 状态留给这一格自己的终局行：改了就等于让恢复读成"这一格阻塞着"，
                        // 而它其实是"上一发的结论收下，只是不再重试"。
                        // tokens / cost_e8 也一律留空：`node_readout` 是按行累加的，
                        // 这一行再带一份就是把同一笔钱数了两次
                        status: None,
                        tokens: None,
                        duration_ms: None,
                        cost_e8: None,
                        conversation_id: None,
                        detail: Some(format!(
                            "这一格的「{which}」预算顶住了（已花 {} 微元 / {tokens} token），不再试第 {attempt} 次",
                            cost_e8 / 100
                        )),
                    }],
                )
                .ok();
                break;
            }
        }
        let conversation_id = AgentProfile::conversation_id(&plan.id, node_id, attempt);
        crate::tool_runtime::set_policy(&conversation_id, scoped.clone());
        // 编排的每一发都是无人值守的：登记进"没人看"那张表，高危动作才停得进待批队列。
        // 不登记的话它走的是即时审批那条路——弹一个没有人在看的窗、等满 600 秒、
        // 把"没人答"记成"用户摇头"。按**每一发自己的话题 id** 登记：那张表按话题认人，
        // 而每一发的 id 都不同（重跑与循环同理）。以前整个 orchestra 从没登记过，
        // 于是面板上那句"N 个节点在等你批准"几乎不可能出现（§5.15）
        let _unattended = crate::tasks::escalate::watch_run(
            &conversation_id,
            &plan.id,
            node_id,
            crate::tasks::runs::StartedBy::Scheduler,
        );
        let mut prompt = format!(
            "{}\n\n（本次运行是计划「{}」里的节点「{node_id}」，第 {attempt} 次尝试。{}）",
            node.goal, plan.goal, profile.role
        );
        if let Some(note) = &feedback {
            prompt.push_str("\n\n");
            prompt.push_str(note);
        }
        // 上游交给这一发的产出。每次尝试都重取一遍：循环与重跑时"黑板上最新那一版"
        // 就是这一发该吃的东西，吃旧的那版等于让模型对着已经作废的结论干活
        let handed = handoff(plan, node_id, board);
        if let Some((material, _)) = &handed {
            prompt.push_str(material);
        }
        let recorder = Recorder::new(app, &conversation_id);
        append_rows(
            ledger,
            &[TraceRow {
                ts_ms: now_ms(),
                plan_id: plan.id.clone(),
                node: node_id.to_string(),
                attempt,
                event: "started".into(),
                status: Some(NodeStatus::Running),
                tokens: None,
                duration_ms: None,
                cost_e8: None,
                conversation_id: Some(conversation_id.clone()),
                // 这一发吃了谁的材料、是哪一版。以前这一格是空的，于是"流水线有没有真的传下去"
                // 在账本里根本问不出来（§5.11）
                detail: handed.as_ref().map(|(_, stamps)| stamps.join(" · ")),
            }],
        )
        .ok();
        let outcome = chat::run_turn_into(
            app,
            &conversation_id,
            &prompt,
            Some(&profile.tools),
            // 档案里没有轮数这一格：编排节点跟着设置里那个全局上限走
            None,
            // 档案指定了模型就用它。以前这一格从没被读过：写了也白写，
            // 而那一发的钱还会记到用户当前的模型上
            profile.model.as_deref(),
            // 档案点名了服务商档案，这一发整份连接域照它走（子助理的专属服务商）
            profile.endpoint.as_deref(),
            cancel,
            &recorder,
        );
        let seen = recorder.take();
        tokens += seen.input_tokens + seen.output_tokens;
        duration_ms += seen.duration_ms;
        // 每一发都有自己的话题 id，所以台账里那一格就是这一发的钱
        if let Some(cost) = crate::tasks::runs::cost_for(app, &conversation_id) {
            cost_e8 += cost.cost_usd_e8;
            cost_seen = true;
        }
        unanswered = seen.unanswered.clone();
        text = seen.text.clone();
        verdict = check.judge(&text);
        if outcome.is_ok() && seen.error.is_none() && verdict.is_pass() {
            break;
        }
        feedback = Some(match (&verdict, seen.error.or(outcome.err())) {
            (Verdict::Fail { why }, _) => check.feedback(why),
            (_, Some(message)) => format!("上一次服务商报错了：{message}。请只补上缺的那部分。"),
            _ => "上一次没有产出。请给出结论。".to_string(),
        });
    }

    // 有操作被拦在审批上而没人在场：这是**等待**，不是"没人反对就放行"
    if !unanswered.is_empty() {
        let detail = format!("有 {} 次操作在等审批，后台运行没有人可问", unanswered.len());
        let escalated = TraceRow {
            ts_ms: now_ms(),
            plan_id: plan.id.clone(),
            node: node_id.to_string(),
            attempt: attempts,
            event: "escalated".into(),
            status: Some(NodeStatus::WaitingApproval),
            tokens: Some(tokens),
            duration_ms: Some(duration_ms),
            cost_e8: cost_seen.then_some(cost_e8),
            conversation_id: None,
            detail: Some(detail.clone()),
        };
        append_rows(ledger, &[escalated]).ok();
        if let Ok(root) = app.path().app_data_dir() {
            let _ = audit::record(
                &root,
                Actor::Scheduler,
                "orchestra:escalate",
                &format!("{} / {}", plan.id, node_id),
                Outcome::Blocked,
            );
        }
        let mut guard = state.lock().unwrap_or_else(PoisonError::into_inner);
        guard.status.insert(node_id.to_string(), NodeStatus::WaitingApproval);
        guard.spent_tokens += tokens;
        guard.spent_duration_ms += duration_ms;
        guard.spent_cost_e8 += cost_e8;
        guard.waiting.push((node_id.to_string(), detail));
        return NodeOutcome {
            node: node_id.into(),
            status: NodeStatus::WaitingApproval,
            contribution: None,
            failed: false,
        };
    }

    // 降级这条路要说清：有产出但没过形状检查，节点算"完成了（降级）"，
    // 结论照旧进黑板、照旧进汇合，只是带一句"未通过校验"。
    // 把这种产出直接判死，等于把已经花掉的钱和一段可能确实有用的观察一起扔掉
    let produced = !text.trim().is_empty();
    if produced {
        // 这两次写拿的是**写入那一刻**的版本，是对的：每一格只写自己那一格的结论，
        // 语义是"我这格的上一版，我要覆盖掉"，不是"我照着某一版算出一个新值"。
        // 只有下面那个计数器是照旧值加一的，所以它必须走 `bump`
        let cas = board.compare_swap(node_id, board.version_of(node_id), &text, node_id);
        // 被顶回来的那一次落一行账。内存里那份副本会随进程一起没了，而"这两份意见打过架、
        // 谁赢了、输的那份去哪了"是已经花过钱的事实——它只该住一处，而那处是账本
        if let Some(row) = conflict_row(&plan.id, node_id, attempts, &cas) {
            append_rows(ledger, &[row]).ok();
        }
        // 校验结论也占一格黑板。**这一格就是条件边与循环边要读的那个事实**：
        // 没有它，`Edge::Conditional` 等的键只有测试会写，那条边在链路上永不成立
        let verdict_fact = verdict_value(verdict.is_pass());
        board.compare_swap(
            &verdict_key(node_id),
            board.version_of(&verdict_key(node_id)),
            verdict_fact,
            node_id,
        );
        // 循环边：先把这一轮记上，再问"还开不开"。顺序反了就会多跑一轮。
        // 键与读方共用 [`iterations_key`]——以前轮数写在 `until_key` 上、读在 `#iters` 上，
        // 两个人各数各的，所以这条循环从来没真的转过第二圈
        if matches!(node.edge, Edge::Loop { .. }) {
            board.bump(&iterations_key(node_id), node_id);
        }
    }
    let status = match (produced, loop_open(node, board)) {
        (false, _) => NodeStatus::Failed,
        // 循环还开着：这一轮跑完了，但这一格**不是**落定状态，`ready_set` 会再派它一次
        (true, true) => NodeStatus::Iterated,
        (true, false) => NodeStatus::Done,
    };
    append_rows(
        ledger,
        &[TraceRow {
            ts_ms: now_ms(),
            plan_id: plan.id.clone(),
            node: node_id.to_string(),
            attempt: attempts,
            event: match (produced, status) {
                (false, _) => "failed",
                (true, NodeStatus::Iterated) => "iterated",
                _ => "finished",
            }
            .into(),
            status: Some(status),
            tokens: Some(tokens),
            duration_ms: Some(duration_ms),
            cost_e8: cost_seen.then_some(cost_e8),
            conversation_id: None,
            detail: match &verdict {
                Verdict::Pass => None,
                Verdict::Fail { why } => Some(why.clone()),
            },
        }],
    )
    .ok();
    let mut guard = state.lock().unwrap_or_else(PoisonError::into_inner);
    guard.status.insert(node_id.to_string(), status);
    guard.spent_tokens += tokens;
    guard.spent_duration_ms += duration_ms;
    guard.spent_cost_e8 += cost_e8;
    NodeOutcome {
        node: node_id.into(),
        status,
        // 循环还没停的那一轮：结论已经在黑板上（下游与下一轮都从那里读），但**不进汇合**——
        // 一个还没定下来的节点没有"最终结论"可交，交了就是把中间轮当成答案
        contribution: if status == NodeStatus::Iterated {
            None
        } else {
            Some(Contribution {
                node: node_id.to_string(),
                profile: profile.name.clone(),
                text,
                verdict,
            })
        },
        failed: status == NodeStatus::Failed,
    }
}

/// 那条"图现在长这样"的行。写它的有两条路（自动重规划、用户手工改边），读它的只有一条
/// （`plan_from_ledger`），所以行的形状只允许有一个构造：两处各写一遍，
/// 早晚有一处的 `node` 或事件名飘掉，而飘掉的那一半在重启后**安静地**看不见
fn replan_row(updated: &Plan) -> TraceRow {
    TraceRow {
        ts_ms: now_ms(),
        plan_id: updated.id.clone(),
        node: PLAN_SCOPE.into(),
        attempt: 0,
        event: "replan".into(),
        status: None,
        tokens: None,
        duration_ms: None,
        cost_e8: None,
        conversation_id: None,
        detail: serde_json::to_string(updated).ok(),
    }
}

/// 改边允许碰哪个节点：只有**还没开始**的那一端。已经在跑或跑完的节点，它"当时为什么能派发"
/// 是一条发生过的事实（账本里那一行 `ready` 就是它），改它的依赖等于改写历史（§5.13）
fn edge_gate(status: Option<NodeStatus>) -> Result<(), String> {
    match status {
        None | Some(NodeStatus::Pending) => Ok(()),
        Some(_) => Err(
            "那个节点已经开始了（不再是待跑），改它的依赖等于改写它当时为什么能派发。要改先重跑它。"
                .into(),
        ),
    }
}

/// 账本里最后一条 `replan` 行落的就是"当时真的派发过的那张整图"，所以恢复只要把它套回来。
/// 读不懂的行、别的 plan、以及比手上这张更小的那张都直接跳过：
/// 一份写坏的账本不该把图改小，也不该让已经追加过的节点在一次重启里消失
fn plan_from_ledger(base: Plan, rows: &[TraceRow]) -> Plan {
    let mut live = base.clone();
    for row in rows.iter().filter(|row| row.event == "replan" && row.plan_id == base.id) {
        let Some(detail) = &row.detail else { continue };
        if let Ok(planned) = serde_json::from_str::<Plan>(detail) {
            if planned.id == base.id && planned.nodes.len() >= live.nodes.len() {
                live = planned;
            }
        }
    }
    live
}

/// 一支失败了，先问规划器"这一步该换成什么"。问到就把新图整份落进账本、把新节点登记成
/// Pending，再换掉共享的那张图；问不到（已经重做过一次 / 预算里的节点数顶住 / 图里没这个 id）
/// 就在审计里说清为什么不再试——静默放弃与静默多花钱是同一类问题，都得有人看得见。
///
/// 账本写不进去就**不**改图：没有账本的节点等于没被派发过，界面上多它一颗就是在骗人
/// 把"图长了一份"这件事落到它该在的三个地方：账本、状态表、共享的那张图。
///
/// 候选由调用方算好交进来（失败→`graph::replan`，成功的监督者→`graph::followups`），
/// 因为两条路共用**同一个 `replan` 事件名**写账本——恢复时 `plan_from_ledger` 只认这一个事件，
/// 分两个名字就会有一半的追加在重启后悄悄消失。
///
/// `audit_on_err` 是失败路径才为真：监督者没写「补做：」是**正常收工**，
/// 把它记成一次 blocked 会让人以为有什么被拦下了
fn grow_plan(
    live: &mut Plan,
    shared: &Arc<Mutex<Plan>>,
    grown: Result<Plan, String>,
    from_id: &str,
    reason: &str,
    audit_on_err: bool,
    state: &Arc<Mutex<RunState>>,
    ledger: &Arc<PathBuf>,
    app: &AppHandle,
) {
    let root = app.path().app_data_dir().ok();
    let mut updated = match grown {
        Ok(updated) => updated,
        Err(why) => {
            if audit_on_err {
                if let Some(root) = root {
                    let _ = audit::record_detail(
                        &root,
                        Actor::Scheduler,
                        "orchestra:replan",
                        &format!("{} / {}", live.id, from_id),
                        Outcome::Blocked,
                        Some(why),
                    );
                }
            }
            return;
        }
    };
    let added: Vec<String> = updated
        .nodes
        .iter()
        .filter(|node| live.find(&node.id).is_none())
        .map(|node| node.id.clone())
        .collect();
    if added.is_empty() {
        return;
    }
    // 补做节点的档案分配（§7.3 接线）：问一嘴决策层该派谁。要在写账本**之前**——
    // 账本里落的就是派发时的图，档案换了再落账，恢复出来的计划就跟第一次跑的对不上
    assign_followup_profiles(app, &mut updated, &added);
    let mut rows = vec![replan_row(&updated)];
    rows.extend(added.iter().map(|id| TraceRow {
        ts_ms: now_ms(),
        plan_id: live.id.clone(),
        node: id.clone(),
        attempt: 0,
        event: "queued".into(),
        status: Some(NodeStatus::Pending),
        tokens: None,
        duration_ms: None,
        cost_e8: None,
        conversation_id: None,
        detail: Some(format!("「{from_id}」{reason}")),
    }));
    if append_rows(ledger, &rows).is_err() {
        if let Some(root) = root {
            let _ = audit::record_detail(
                &root,
                Actor::Scheduler,
                "orchestra:replan",
                &format!("{} / {}", live.id, from_id),
                Outcome::Failed,
                Some("账本写不进去，这一次不追加节点".into()),
            );
        }
        return;
    }
    {
        let mut guard = state.lock().unwrap_or_else(PoisonError::into_inner);
        for id in &added {
            guard.status.insert(id.clone(), NodeStatus::Pending);
        }
    }
    if let Some(root) = root {
        let _ = audit::record_detail(
            &root,
            Actor::Scheduler,
            "orchestra:replan",
            &format!("{} / {}", live.id, from_id),
            Outcome::Ok,
            Some(format!("{reason}：追加了 {}", added.join("、"))),
        );
    }
    *shared.lock().unwrap_or_else(PoisonError::into_inner) = updated.clone();
    *live = updated;
}

/// 补做节点的候选执行角色（内置的三种）。描述是决策层判"该派谁"的依据正文，一句话都不能含糊——
/// "不要动任何东西"就是 reader/verifier 与 worker 之间的那条界线。
/// 用户自定义的子助理不经这里：它们由 [`followup_roster`] 按「编排可派」并进花名册
const FOLLOWUP_ROLES: [(&str, &str); 3] = [
    ("reader", "只看不改：把事实读回来，不要动任何东西。"),
    (
        "worker",
        "在指定范围内完成这一步，可以写文件、跑命令，并把结论写成一句可核对的话。",
    ),
    (
        "verifier",
        "只读复核：对照要求检查已有的结论，把对不上号的地方指出来，不动任何东西。",
    ),
];

/// 补做节点的档案分配（决策层 §7.3 的生产接线）。
///
/// 以前监督者「补做：」长出来的节点一律 `"worker"`：读资料、复核、动手一个模子
/// 套到底。现在每一补做问一嘴决策层——任务目标换行不行，花名册（[`followup_roster`]：
/// 内置三种执行角色 + 用户标了「编排可派」的子助理）里谁最合适。决策层没答上（开关关、
/// 桥没人听、超时、答出花名册之外）就保持 worker：**fail-open，桥的缺席不是功能的缺席**。
///
/// 协调位（监督者/规划器/整合者）不在花名册里：把执行活派给它们等于让不干活的人干活。
/// 角色名随后由 [`profile_for_name`] 兑现成真档案，派发处的兜底也认得它们——
/// 新增角色不需要档案表里有存货
fn assign_followup_profiles(app: &AppHandle, updated: &mut Plan, added: &[String]) {
    let custom = crate::config::load(app).subagents;
    assign_followup_profiles_with(
        &mut |method, payload, timeout_ms| crate::decision_bridge::ask(app, method, payload, timeout_ms),
        &followup_roster(&custom),
        updated,
        added,
    );
}

/// 派工花名册：内置三执行角色 + 用户标了「编排可派」的自定义子助理。
/// 三种定义不收：与内置或已有条目撞名的（内置角色赢，与 [`profile_for_name`]
/// 同一条防御）、名字空白的（名字是节点 profile 字符串，空了连账本都对不上）。
/// 抽成纯函数是因为这张名单是决策层看到的"全部人选"，谁进了谁没进要有测试盯着
fn followup_roster(custom: &[SubagentDef]) -> Vec<(String, String)> {
    let mut roster: Vec<(String, String)> = FOLLOWUP_ROLES
        .iter()
        .map(|(role, description)| (role.to_string(), description.to_string()))
        .collect();
    for def in custom {
        if !def.orchestration_assignable
            || def.name.trim().is_empty()
            || roster.iter().any(|(role, _)| *role == def.name)
        {
            continue;
        }
        roster.push((def.name.clone(), def.description.clone()));
    }
    roster
}

/// 判定面与"怎么把问题递出去"分开：`ask` 是注入点，生产传决策桥，测试传假答案——
/// fail-open 的几条边（没答上、答出花名册、不该问的节点）不必起一个真的 WebView 就能钉死
fn assign_followup_profiles_with(
    ask: &mut dyn FnMut(&str, serde_json::Value, u64) -> Option<serde_json::Value>,
    roster: &[(String, String)],
    updated: &mut Plan,
    added: &[String],
) {
    const ASSIGN_ASK_TIMEOUT_MS: u64 = 3000;
    for id in added {
        let Some(node) = updated.nodes.iter_mut().find(|node| &node.id == id) else {
            continue;
        };
        if node.profile != "worker" {
            // 只改默认派工：将来某条路径显式指定了档案，决策层不抢
            continue;
        }
        let task = serde_json::json!({
            // 任务目标截到 400 字：state 是给决策层看的，整段自由文本没有边际收益
            "goal": node.goal.chars().take(400).collect::<String>(),
            "type": "followup",
        });
        let agents: Vec<serde_json::Value> = roster
            .iter()
            .map(|(role, description)| {
                serde_json::json!({ "role": role, "description": description })
            })
            .collect();
        let Some(answer) = ask(
            "assignAgent",
            serde_json::json!({ "task": task, "agents": agents }),
            ASSIGN_ASK_TIMEOUT_MS,
        ) else {
            continue;
        };
        let Some(role) = answer.get("agent").and_then(|value| value.as_str()) else {
            continue;
        };
        if roster.iter().any(|(name, _)| name == role) {
            node.profile = role.to_string();
        }
    }
}

/// 起一份 plan。返回句柄，暂停/恢复/取消/状态/重跑单节点都对着它
pub fn start(
    app: &AppHandle,
    plan: Plan,
    profiles: HashMap<String, AgentProfile>,
    merge: Merge,
    quota: Arc<Quota>,
    rates: Arc<Rates>,
) -> Result<Arc<Handle>, String> {
    start_at(app, plan, profiles, merge, quota, rates, false)
}

/// `paused = true` 是恢复那条路：崩掉的那份计划重新登记时**不该自己接着跑**——
/// 那等于在没人看着的时候花真金白银。它在界面上就是一行"暂停着"，按下恢复才动
fn start_at(
    app: &AppHandle,
    plan: Plan,
    profiles: HashMap<String, AgentProfile>,
    merge: Merge,
    quota: Arc<Quota>,
    rates: Arc<Rates>,
    paused: bool,
) -> Result<Arc<Handle>, String> {
    plan.topo().map_err(|cycles| cycles.to_string())?;
    if plan.nodes.is_empty() {
        return Err("这份计划一个节点都没有，跑它等于什么都不做。".into());
    }
    let root = orchestra_root(app)?;
    let ledger = Arc::new(ledger_path(&root, &plan.id));
    let board = Arc::new(Blackboard::new());
    let state = Arc::new(Mutex::new(RunState::default()));
    let permits = Arc::new(Permits::new(plan.max_parallel));
    let pool = Arc::new(Pool::new(plan.max_parallel));
    let cancel = Arc::new(AtomicBool::new(false));
    let pause = Arc::new(AtomicBool::new(paused));
    let profiles = Arc::new(profiles);
    // 账本里的 replan 行落的是"当时真的派发过的那张整图"，所以起跑前先把它套回来：
    // 崩溃前追加的重做节点不能因为一次重启就当没发生过
    let prior = read_rows(&ledger);
    let plan_id = plan.id.clone();
    // checkpoint 排在任何 `queued` 之前落：崩在第一次派发之前，"有过这张图、它要怎么汇合"
    // 也得有出处。已经有了就不重写——同一个 id 第二次 start 是续跑，不该把图换回旧的那份
    if checkpoint_of(&prior).is_none() {
        append_rows(&ledger, &[planned_row(&plan, &merge)?])?;
    }
    let plan = Arc::new(Mutex::new(plan_from_ledger(plan, &prior)));

    // 先登记再派发：恢复时"这份计划存在过、这些节点排过队"要有出处。
    //
    // 同一个 plan_id 第二次 start()（进程重启后界面上点"再跑一次"）不是从头再来：
    // 账本里已经落定的节点不再登记、也不再派发——验收第 3 条靠的就是这一段
    let seeded = derive_status(&prior);
    seed_verdicts(&board, &prior);
    let fresh: Vec<String> = plan_of(&plan)
        .nodes
        .iter()
        .map(|node| node.id.clone())
        .filter(|id| !seeded.contains_key(id))
        .collect();
    let queued: Vec<TraceRow> = fresh
        .iter()
        .map(|id| TraceRow {
            ts_ms: now_ms(),
            plan_id: plan_id.clone(),
            node: id.clone(),
            attempt: 0,
            event: "queued".into(),
            status: Some(NodeStatus::Pending),
            tokens: None,
            duration_ms: None,
            cost_e8: None,
            conversation_id: None,
            detail: None,
        })
        .collect();
    append_rows(&ledger, &queued)?;
    {
        let mut guard = state.lock().unwrap_or_else(PoisonError::into_inner);
        for (id, status) in &seeded {
            guard.status.insert(id.clone(), *status);
        }
        for row in &queued {
            guard.status.insert(row.node.clone(), NodeStatus::Pending);
        }
    }
    let resumed = seeded.values().filter(|status| status.succeeded()).count();
    audit::record(
        &app.path().app_data_dir().map_err(|error| error.to_string())?,
        Actor::Scheduler,
        if resumed > 0 { "orchestra:resume" } else { "orchestra:start" },
        &format!("{}（跳过已完成的 {resumed} 个节点）", plan_id),
        Outcome::Ok,
    )?;

    let app_for_driver = app.clone();
    let shared_plan = plan.clone();
    let driver_profiles = profiles.clone();
    let driver_state = state.clone();
    let driver_board = board.clone();
    let driver_permits = permits.clone();
    let driver_pool = pool.clone();
    let driver_ledger = ledger.clone();
    let driver_quota = quota.clone();
    let driver_rates = rates.clone();
    let driver_cancel = cancel.clone();
    let driver_pause = pause.clone();
    // 监督者-工作者那一圈的收发台，只由驱动线程喂，所以它不必活到函数外面。
    // 它和黑板不重复：黑板给结论（谁都能读最新版），这里给"谁在什么时候把哪一份
    // 交给了谁"——监督者的材料是由这些回报拼出来的，"哪一份没到"得能回答
    let driver_exchanges = Arc::new(Exchanges::new());
    let driver = thread::spawn(move || {
        // 派发中的那一格带着节点 id：线程 panic 时 join 拿不到 outcome，
        // 但那一格的活必须销账，否则它永远算"还没回来"
        let mut running: Vec<(String, JoinHandle<NodeOutcome>)> = Vec::new();
        let mut scheduler = Scheduler::new();
        let mut contributions: Vec<Contribution> = Vec::new();
        let mut missing: Vec<String> = Vec::new();
        let mut stopped: Vec<String> = Vec::new();
        // 熔断按角色分：一支探索性的读者反复失败，不该把整个计划钉住。
        // 限流按**服务商档案**分，而且是整个进程共享的那几只桶（见 [`Rates`]）——
        // 两个角色打的是同一个账号，按角色分桶就等于把速率乘上角色数
        let mut breakers: HashMap<String, Breaker> = HashMap::new();
        // 上一次落在账本里的 worker 上限：只在真的变了时再补一行
        let mut previous_cap = plan_of(&shared_plan).max_parallel;
        loop {
            if driver_cancel.load(Ordering::Acquire) {
                // 取消要把所有收件箱都关掉：之后迟到的回报投递数是 0，
                // 那正是"已经没人等它"这个事实，而不是一个还能塞进材料的字符串
                driver_exchanges.close(None);
                break;
            }
            if driver_pause.load(Ordering::Acquire) {
                // 暂停不掐正在跑的节点：那一发的钱已经付了，掐掉只会留下一段半截上下文
                thread::sleep(Duration::from_millis(150));
                continue;
            }
            // 本轮看到的图。replanning 追加过节点，下一轮读到的就带着它们；
            // 已经派发出去的节点跑的是它被派发那一刻的那一份定义，不受影响
            let mut driver_plan = plan_of(&shared_plan);
            // 扩缩的两个读数：这一轮死了几个、ready 里有几个没派出去
            let mut round_failures = 0usize;
            let mut alive: Vec<(String, JoinHandle<NodeOutcome>)> = Vec::new();
            for (handed_node, handle) in running.drain(..) {
                if handle.is_finished() {
                    match handle.join() {
                        Ok(outcome) => {
                            // 这一格回来了，调度器那笔"已交出"的账才销得掉：
                            // 「重跑单个节点」靠的就是它排得进第二次
                            scheduler.retire(&outcome.node);
                            let profile = driver_plan
                                .find(&outcome.node)
                                .map(|node| node.profile.clone())
                                .unwrap_or_default();
                            let breaker = breakers
                                .entry(profile.clone())
                                .or_insert_with(|| Breaker::new(3, Duration::from_secs(60)));
                            if outcome.failed {
                                round_failures += 1;
                                if breaker.record_failure() {
                                    append_rows(
                                        &driver_ledger,
                                        &[TraceRow {
                                            ts_ms: now_ms(),
                                            plan_id: driver_plan.id.clone(),
                                            node: PLAN_SCOPE.into(),
                                            attempt: 0,
                                            event: "blocked".into(),
                                            status: Some(NodeStatus::Blocked),
                                            tokens: None,
                                            duration_ms: None,
                                            cost_e8: None,
                                            conversation_id: None,
                                            detail: Some(breaker.notice(&profile)),
                                        }],
                                    )
                                    .ok();
                                }
                            } else {
                                breaker.record_success();
                            }
                            // 有产出就进汇合（没通过校验的那一份仍带着标注）；
                            // 真的没交出东西来的那一支，才算"缺一支"
                            // 失败节点八成没有产出，所以证据不能只看正文：没过校验的那一句"为什么"
                            // 才是规划器真正拿得到的线索
                            let evidence = outcome
                                .contribution
                                .as_ref()
                                .map(|held| {
                                    if !held.text.trim().is_empty() {
                                        return held.text.clone();
                                    }
                                    match &held.verdict {
                                        Verdict::Pass => "它没有给出任何产出。".to_string(),
                                        Verdict::Fail { why } => format!("没有通过校验：{why}"),
                                    }
                                })
                                .unwrap_or_default();
                            match outcome.contribution {
                                Some(contribution) => contributions.push(contribution),
                                None if outcome.status == NodeStatus::WaitingApproval => {}
                                // 循环的中间轮：它没欠汇合一份结论，也还没跑完
                                None if outcome.status == NodeStatus::Iterated => {}
                                None => missing.push(outcome.node.clone()),
                            }
                            // 两种"图该长一份"的情况走同一个落账动作：失败的这一支换一步做，
                            // 或者成功的监督者说"还缺什么"再派一批。两者都只追加，都写同一个
                            // `replan` 事件，所以重启后不会有一半的追加凭空消失
                            let is_supervisor = driver_plan
                                .find(&outcome.node)
                                .map(|node| node.profile == SUPERVISOR)
                                .unwrap_or(false);
                            // 工作者这一格结了，先把回报点对点交给它的监督者。等审批的那一格还没结，
                            // 报上去等于把"没到"记成"到了"，barrier 就会提前判齐
                            if outcome.status != NodeStatus::WaitingApproval {
                                if let Some(boss) = driver_plan.supervisor_of(&outcome.node) {
                                    let expected = driver_plan.workers_of(boss).len();
                                    let body = if evidence.trim().is_empty() {
                                        "这一格没有产出。"
                                    } else {
                                        evidence.as_str()
                                    };
                                    // 报之前先问一次"它到过了没有"：报过之后到了的人里一定带着这一格，
                                    // 那时再问就分不出"重跑不另算"和"收件箱已经关了"这两种 delivered=0
                                    let redo = driver_exchanges
                                        .arrived(boss).contains(&outcome.node);
                                    let (delivered, settled) =
                                        driver_exchanges.report(boss, &outcome.node, body, expected);
                                    let still =
                                        expected.saturating_sub(driver_exchanges.arrived(boss).len());
                                    append_rows(
                                        &driver_ledger,
                                        &[TraceRow {
                                            ts_ms: now_ms(),
                                            plan_id: driver_plan.id.clone(),
                                            node: outcome.node.clone(),
                                            attempt: 0,
                                            event: "bus".into(),
                                            status: None,
                                            tokens: None,
                                            duration_ms: None,
                                            cost_e8: None,
                                            conversation_id: None,
                                            detail: Some(if redo {
                                                format!("→ {boss}：这一格早就报过，重跑不另算一份")
                                            } else if delivered == 0 {
                                                format!("→ {boss}：回报没送到，{boss} 已经决策过了")
                                            } else if settled {
                                                format!("→ {boss}：这一代齐了（{expected} 份）")
                                            } else {
                                                format!("→ {boss}：还差 {still} 份")
                                            }),
                                        }],
                                    )
                                    .ok();
                                }
                            }
                            // 监督者落定前把收件箱读干净：这些回报就是它"还缺什么"的材料。
                            // 取一次少一次，所以必须在 close 之前
                            let material = if is_supervisor {
                                let reports = report_bodies(&driver_exchanges.collect(&outcome.node));
                                let arrived = driver_exchanges.arrived(&outcome.node);
                                let unheard: Vec<&str> = driver_plan
                                    .workers_of(&outcome.node)
                                    .into_iter()
                                    .filter(|worker| {
                                        !arrived.iter().any(|who| who == *worker)
                                    })
                                    .collect();
                                append_rows(
                                    &driver_ledger,
                                    &[TraceRow {
                                        ts_ms: now_ms(),
                                        plan_id: driver_plan.id.clone(),
                                        node: outcome.node.clone(),
                                        attempt: 0,
                                        event: "bus".into(),
                                        status: None,
                                        tokens: None,
                                        duration_ms: None,
                                        cost_e8: None,
                                        conversation_id: None,
                                        detail: match (reports.len(), unheard.len()) {
                                            (0, 0) => Some("读收件箱：0 份回报，一个都没缺".to_string()),
                                            (n, 0) => Some(format!("读收件箱：收齐 {n} 份回报")),
                                            (n, m) => Some(format!(
                                                "读收件箱：{n} 份回报，缺 {m} 份（{}）",
                                                unheard.join("、")
                                            )),
                                        },
                                    }],
                                )
                                .ok();
                                driver_exchanges.close(Some(&outcome.node));
                                if reports.is_empty() {
                                    evidence.clone()
                                } else {
                                    let mut joined = evidence.clone();
                                    for one in &reports {
                                        joined.push('\n');
                                        joined.push_str(one);
                                    }
                                    joined
                                }
                            } else {
                                evidence.clone()
                            };
                            let grown = if outcome.failed {
                                Some((replan(&driver_plan, &outcome.node, &evidence), "失败后由规划器追加", true))
                            } else if is_supervisor {
                                Some((
                                    followups(&driver_plan, &outcome.node, &material),
                                    "监督者说还缺，补的第二轮",
                                    // 没写「补做：」是正常收工，不该记成一次拦下
                                    false,
                                ))
                            } else {
                                None
                            };
                            if let Some((candidate, reason, audit_on_err)) = grown {
                                grow_plan(
                                    &mut driver_plan,
                                    &shared_plan,
                                    candidate,
                                    &outcome.node,
                                    reason,
                                    audit_on_err,
                                    &driver_state,
                                    &driver_ledger,
                                    &app_for_driver,
                                );
                            }
                        }
                        Err(_) => {
                            scheduler.retire(&handed_node);
                            missing.push(format!("(线程panic){handed_node}"));
                        }
                    }
                } else {
                    alive.push((handed_node, handle));
                }
            }
            running = alive;

            let snapshot = driver_state.lock().unwrap_or_else(PoisonError::into_inner).clone();
            // 排序交给调度器（它按关键路径进队），这一格只负责"从现在的状态表里算出谁能跑"
            let ready = ready_set(&driver_plan, &snapshot.status, &driver_board);
            if ready.is_empty() && running.is_empty() && scheduler.is_idle() {
                break;
            }
            // 预算顶住的是"派发"，不是"杀掉正在跑的请求"。花费那一格交的是台账单位
            // （1e-8 美元）的真数——这里以前恒为 0，于是 `max_cost_micros` 那条预算
            // 看着存在、实际一次也没顶住过
            if let Some(which) = driver_plan.budget.exhausted(
                snapshot.spent_tokens,
                snapshot.spent_cost_e8,
                contributions.len() + missing.len(),
            ) {
                {
                    let mut guard = driver_state.lock().unwrap_or_else(PoisonError::into_inner);
                    for id in &ready {
                        guard.status.insert(id.clone(), NodeStatus::Blocked);
                    }
                    guard.blocked_by = Some(which.to_string());
                }
                stopped = ready;
                append_rows(
                    &driver_ledger,
                    &[TraceRow {
                        ts_ms: now_ms(),
                        plan_id: driver_plan.id.clone(),
                        node: PLAN_SCOPE.into(),
                        attempt: 0,
                        event: "blocked".into(),
                        status: Some(NodeStatus::Blocked),
                        tokens: Some(snapshot.spent_tokens),
                        duration_ms: None,
                        cost_e8: None,
                        conversation_id: None,
                        detail: Some(format!("预算里的「{which}」用完了，剩下的节点没有派发")),
                    }],
                )
                .ok();
                break;
            }
            // 派发这一格现在由调度器回答"下一个该派谁"：队列跨轮存活，所以被限流或熔断
            // 挡住的那一支不再顺手停掉别的档案，而"派出去还没回来"的一份也不会被再派一次
            scheduler.retain(&ready);
            // worker 位次跟着池子这一刻的上限走，缩下去时队列自己并回去
            scheduler.offer(&driver_plan, &ready, &|_| driver_pool.max().max(1));
            // 熔断中的档案：排着的那些活撤下来，统一落 Blocked——留在队里不叫等它 healed，
            // 叫"看上去还在跑"
            for (profile_name, breaker) in breakers.iter() {
                if breaker.allows(Instant::now()) {
                    continue;
                }
                let dropped = scheduler.drop_profile(profile_name);
                if !dropped.is_empty() {
                    let mut guard = driver_state.lock().unwrap_or_else(PoisonError::into_inner);
                    for node_id in dropped {
                        guard.status.insert(node_id, NodeStatus::Blocked);
                    }
                }
            }
            loop {
                // 并发位先占住，再问调度器要活：反过来会让空转的那一手扣掉一个限流令牌
                let Some(permit) = driver_permits.try_acquire() else { break };
                let Some(lease) = driver_pool.lease() else {
                    drop(permit);
                    break;
                };
                // 再要一格**全局**位。这一份计划自己的上限管不到"别的计划也在跑"，
                // 而顶住时要分清是"这一刻别人占满"还是"这一档最多占这么多"——
                // 前者等一等就过去，后者不让就永远不过去
                let quota_slot = match driver_quota.clone().try_acquire(driver_plan.priority) {
                    Ok(slot) => slot,
                    Err(denied) => {
                        drop((permit, lease));
                        append_rows(
                            &driver_ledger,
                            &[TraceRow {
                                ts_ms: now_ms(),
                                plan_id: driver_plan.id.clone(),
                                node: PLAN_SCOPE.into(),
                                attempt: 0,
                                event: "quota".into(),
                                status: None,
                                tokens: None,
                                duration_ms: None,
                                cost_e8: None,
                                conversation_id: None,
                                detail: Some(match denied {
                                    Denied::Full => format!(
                                        "全局 {} 格已被别的计划占满（这一档「{}」最多 {} 格）",
                                        driver_quota.total(),
                                        priority_label(driver_plan.priority),
                                        driver_quota.share(driver_plan.priority),
                                    ),
                                    Denied::AtShare => format!(
                                        "这一档「{}」最多占 {} 格，已经占满",
                                        priority_label(driver_plan.priority),
                                        driver_quota.share(driver_plan.priority),
                                    ),
                                }),
                            }],
                        )
                        .ok();
                        // 全局位不是这一轮能等出来的：睡一下再问，别空转烧 CPU
                        thread::sleep(Duration::from_millis(200));
                        break;
                    }
                };
                let Some(assignment) = scheduler.next(&|profile| {
                    breakers
                        .get(profile)
                        .map(|breaker| breaker.allows(Instant::now()))
                        .unwrap_or(true)
                }) else {
                    break;
                };
                if assignment.stolen {
                    // 窃取要在 Trace 里留痕：不然"工作窃取"只是这个函数里的一次局部变量
                    append_rows(
                        &driver_ledger,
                        &[TraceRow {
                            ts_ms: now_ms(),
                            plan_id: driver_plan.id.clone(),
                            node: assignment.node.clone(),
                            attempt: 0,
                            event: "stolen".into(),
                            status: None,
                            tokens: None,
                            duration_ms: None,
                            cost_e8: None,
                            conversation_id: None,
                            detail: Some(format!(
                                "档案「{}」的 {slot} 号位从同伴尾巴上拿到了这一份",
                                assignment.profile,
                                slot = assignment.slot
                            )),
                        }],
                    )
                    .ok();
                }
                // 限流：这一发在它**要去的那个账号**上没有令牌，就把活放回原位、本轮到此为止，不排队堆积
                let endpoint = crate::config::load(&app_for_driver).active_profile_id;
                if !driver_rates.permit(
                    &endpoint,
                    plan_of(&shared_plan).max_parallel,
                    Instant::now(),
                ) {
                    scheduler.put_back(&assignment);
                    break;
                }
                let Assignment { node: node_id, profile: _profile_name, .. } = assignment;
                let Some(node) = driver_plan.find(&node_id) else {
                    // 图和队列对不上：这份活不该再占着队，销账跳过它
                    scheduler.retire(&node_id);
                    continue;
                };
                let profile = driver_profiles
                    .get(node.profile.as_str())
                    .cloned()
                    .unwrap_or_else(|| {
                        // 补做节点带着起步档案表里没有的名字（决策层派的自定义子助理）：
                        // 现读目录按名字兑现。不认得的名字在 profile_for_name 里落回 worker
                        let custom = crate::config::load(&app_for_driver).subagents;
                        profile_for_name(node.profile.as_str(), &custom)
                    });
                let app = app_for_driver.clone();
                let plan = driver_plan.clone();
                let board = driver_board.clone();
                let state = driver_state.clone();
                let ledger = driver_ledger.clone();
                let cancel = driver_cancel.clone();
                running.push((
                    node_id.clone(),
                    thread::spawn(move || {
                        let outcome = run_node(
                            &app,
                            &plan,
                            &node_id,
                            1,
                            &profile,
                            &board,
                            &state,
                            &ledger,
                            &cancel,
                        );
                        // 并发位、worker 租约与全局额度随这个闭包一起归还：忘了 release 在这里不可能发生
                        drop((permit, lease, quota_slot));
                        outcome
                    }),
                ));
            }
            // 动态扩缩：这一轮死了几个就把 worker 上限砍一半，没死而还有活排着就还一手
            // （越不过配置的那个数字）。每次变动都在账本上留一行，否则
            // "当时到底开几路"只有这一刻的人知道
            let backlog = scheduler.pending();
            let cap = driver_pool.scale(backlog, round_failures);
            if cap != previous_cap {
                append_rows(
                    &driver_ledger,
                    &[TraceRow {
                        ts_ms: now_ms(),
                        plan_id: driver_plan.id.clone(),
                        node: PLAN_SCOPE.into(),
                        attempt: 0,
                        event: "scaled".into(),
                        status: None,
                        tokens: None,
                        duration_ms: None,
                        cost_e8: None,
                        conversation_id: None,
                        detail: Some(format!(
                            "worker 上限 {previous_cap} → {cap}（这一轮失败 {round_failures}、没派出去 {backlog}）"
                        )),
                    }],
                )
                .ok();
                previous_cap = cap;
            }
            thread::sleep(Duration::from_millis(80));
        }
        // 收尾看的是最后一轮的图：replanning 追加过的节点也在这张图上，级联要说得清它们
        let driver_plan = plan_of(&shared_plan);

        // 取消：落定之外的下游统一 Skipped，级联要说得出是谁拖住的
        let snapshot = driver_state.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let mut changes: Vec<(String, NodeStatus)> = if driver_cancel.load(Ordering::Acquire) {
            cascade(&driver_plan, &snapshot.status)
        } else {
            stopped.iter().map(|id| (id.clone(), NodeStatus::Blocked)).collect()
        };
        if driver_cancel.load(Ordering::Acquire) {
            changes.extend(
                snapshot
                    .status
                    .iter()
                    .filter(|(_, held)| **held == NodeStatus::Pending || **held == NodeStatus::Running)
                    .map(|(id, _)| (id.clone(), NodeStatus::Canceled)),
            );
        }
        if !changes.is_empty() {
            let rows: Vec<TraceRow> = changes
                .iter()
                .map(|(node, status)| TraceRow {
                    ts_ms: now_ms(),
                    plan_id: driver_plan.id.clone(),
                    node: node.clone(),
                    attempt: 0,
                    event: status_key(*status),
                    status: Some(*status),
                    tokens: None,
                    duration_ms: None,
                    cost_e8: None,
                    conversation_id: None,
                    detail: Some(if driver_cancel.load(Ordering::Acquire) {
                        "上游被取消".into()
                    } else {
                        "预算用尽，没有派发".into()
                    }),
                })
                .collect();
            append_rows(&driver_ledger, &rows).ok();
            let mut guard = driver_state.lock().unwrap_or_else(PoisonError::into_inner);
            for (node, status) in &changes {
                guard.status.insert(node.clone(), *status);
            }
        }

        let merged = judge::merge(&merge, contributions, missing);
        {
            let mut guard = driver_state.lock().unwrap_or_else(PoisonError::into_inner);
            guard.merged = Some(merged.text.clone());
        }
        append_rows(
            &driver_ledger,
            &[TraceRow {
                ts_ms: now_ms(),
                plan_id: driver_plan.id.clone(),
                node: PLAN_SCOPE.into(),
                attempt: 0,
                event: "merged".into(),
                status: None,
                tokens: None,
                duration_ms: None,
                cost_e8: None,
                conversation_id: None,
                detail: Some(format!(
                    "采纳：{}；淘汰：{}；缺一支：{}",
                    merged.kept.join("、"),
                    merged.dropped.join("、"),
                    if merged.missing.is_empty() { "无".to_string() } else { merged.missing.join("、") }
                )),
            }],
        )
        .ok();
    });

    Ok(Arc::new(Handle {
        app: app.clone(),
        plan_id,
        plan,
        profiles,
        cancel,
        pause,
        state,
        board,
        permits,
        quota,
        pool,
        ledger,
        driver: Mutex::new(Some(driver)),
    }))
}

/// 档位要说得出人话。被配额挡住的那一行要说清是"哪一档、最多几格"，
/// 否则用户看到的就只是一句"没动"
fn priority_label(priority: Priority) -> &'static str {
    match priority {
        Priority::Background => "后台",
        Priority::Normal => "常规",
        Priority::Foreground => "前台",
    }
}

fn status_key(status: NodeStatus) -> String {
    match status {
        NodeStatus::Canceled => "canceled".into(),
        NodeStatus::Blocked => "blocked".into(),
        _ => "skipped".into(),
    }
}

// ---- Tauri 命令 ----

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeView {
    pub id: String,
    pub profile: String,
    pub depends_on: Vec<String>,
    pub depth: usize,
    pub status: NodeStatus,
    pub board_version: u64,
    /// 下面三格都是从**账本行**派生的读数（`node_readout`），不是第二份进度表：
    /// 验收那句"每个 agent 的实时输出、成本、耗时"里，成本与耗时以前无处可取
    pub tokens: u64,
    pub duration_ms: u64,
    /// 1e-8 美元。`0` 与"这台模型没价表"是两件事：没价表时账本行里那一格是空的，
    /// 派生出来就是 0，界面上靠 `priced` 那一格分开说
    pub cost_e8: i64,
    /// 这一格的发有没有价表可算（账本行里至少有一格带着花费）
    pub priced: bool,
    /// 这一格动过几个文件（多次尝试合起来去重）。设计里那句"编排器只报这一步动过哪些文件、
    /// 那边有没有快照"以前无处可看：账上记着，面板不说
    pub files_touched: usize,
    /// 那些改动里至少有一处留着快照，因此变更请求页那边有的可回。
    /// **能不能真的回，是点下去那一刻按文件现在的字节判的**（`edits::plan_revert`），
    /// 这一格不冒充那个判定
    pub snapshotted: bool,
    /// 这一格最新那一发所在的话题。上一格说"有的可回"，而回滚只认话题 id——
    /// 没有这一格，面板上那句话就没有出口（用户得自己在话题列表里认出那一发）
    pub conversation_id: Option<String>,
    /// 这一格是**降级收下**的那一份：有产出、但没过校验，值就是没过的理由。
    /// `None` = 最后那一发过了校验（或还没跑）。状态那一格只有"完成"，
    /// 分不出干净的那一份与将就的那一份，所以这一格单独带着
    pub degraded: Option<String>,
    /// 这一格**凭什么被放行**的一句人话，从图上现读（[`Edge::gate_text`]）。
    /// 没有它，面板上那排"换成等某一格的结论"按下去之后看不出任何变化——
    /// 一个没有读数的旋钮不如没有
    /// `None` = 就是默认那一种"跑完就放行"：要不要多说这一句由后端判，
    /// 界面不再去比对一句中文（那会变成两边各写一次的暗号）
    pub gate: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanView {
    pub plan_id: String,
    pub goal: String,
    pub nodes: Vec<NodeView>,
    pub critical_path: Vec<String>,
    pub spent_tokens: u64,
    pub spent_duration_ms: u64,
    /// 这份计划到现在花了多少（1e-8 美元，与预算那条同一单位）。钱不走浮点过 IPC：
    /// 一分一分加的东西换成 float，界面上那个数就会与价表算出来的对不上
    pub spent_cost_e8: i64,
    pub waiting: Vec<(String, String)>,
    pub merged: Option<String>,
    pub blocked_by: Option<String>,
    pub paused: bool,
    pub canceled: bool,
    pub finished: bool,
    pub in_flight: usize,
    pub max_parallel: usize,
    /// 现在占着几个 worker 租约。它与 in_flight 分开，是因为"并发位"和"线程"不是一回事
    pub workers: usize,
    /// 池子这一刻愿意开几路。被失败砍过它就小于 `worker_ceiling`
    pub worker_cap: usize,
    /// 池子自己的天花板。它和 `max_parallel` 一起报出来，界面上才看得出
    /// "并发不超上限"这条是照着哪个数在守
    pub worker_ceiling: usize,
    /// 黑板上被 CAS 拒过几次。冲突双留之后必须有个地方说得清
    pub conflicts: usize,
    /// 这一份计划抢全局位时的那一档
    pub priority: Priority,
    /// 全局池一共几格（配置给的）。它与 `max_parallel` 是两个数：
    /// 一个是"这份计划里同时几路"，一个是"这台机器上同时几路"
    pub quota_total: usize,
    /// 这一档最多能占几格
    pub quota_share: usize,
    /// 此刻全局占了几格（所有档加起来）
    pub quota_used: usize,
}

/// 进程级的编排器状态：在跑的那些计划 + **那一份共用的并发额度**。
/// 额度由外面递进来而不是在这里自建：自建就会有两个"全局"（编排器一个、定时任务一个），
/// 而面板上只写得下一个数
pub struct Hub {
    plans: Mutex<HashMap<String, Arc<Handle>>>,
    quota: Arc<Quota>,
    rates: Arc<Rates>,
}

/// 配置没给出全局上限时的那个数。它比一份计划的默认并发（1）大，所以单跑一份时
/// 它不是那道闸；三份各 4 路同时开跑时才是
pub const DEFAULT_TOTAL_PARALLEL: usize = 6;

/// 计划级那一行的 `node`。它占的是"节点"那一格，所以任何按节点算的读数都要避开它——
/// [`node_readout`] 避开的原因就写在它自己的注释里
const PLAN_SCOPE: &str = "(plan)";

/// 这份账本能不能恢复、恢复成哪张图。`None` = 界面上就该看不见它：
/// 读不出 `planned` 的是这份改动之前落盘的老账本（凭节点名猜一张图，边与汇合规则全都对不上），
/// 每个节点都落定的是跑完的计划（要的是账本可读，不是一个线程陪着）
fn recover_from(rows: &[TraceRow]) -> Option<(Plan, Merge)> {
    let checkpoint = checkpoint_of(rows)?;
    let plan = plan_from_ledger(checkpoint.plan.clone(), rows);
    if plan.nodes.is_empty() {
        return None;
    }
    let status = derive_status(rows);
    let settled = plan
        .nodes
        .iter()
        .all(|node| status.get(&node.id).is_some_and(|state| state.is_terminal()));
    if settled {
        return None;
    }
    Some((plan, checkpoint.merge))
}

impl Hub {
    pub fn with_slots(quota: Arc<Quota>) -> Self {
        Self {
            plans: Mutex::new(HashMap::new()),
            quota,
            rates: Arc::default(),
        }
    }

    /// 拿额度本身（Arc 克隆）。start 用它把配置里那个数套上去
    pub fn quota(&self) -> Arc<Quota> {
        self.quota.clone()
    }

    /// 那几只限流桶本身（Arc 克隆）。所有计划共用同一份，所以两份计划打同一个账号时
    /// 不会各自攒满一桶再各自发一轮
    pub fn rates(&self) -> Arc<Rates> {
        self.rates.clone()
    }

    pub fn put(&self, handle: Arc<Handle>) {
        self.lock().insert(handle.plan_id.clone(), handle);
    }

    pub fn get(&self, plan_id: &str) -> Option<Arc<Handle>> {
        self.lock().get(plan_id).cloned()
    }

    pub fn plan_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.lock().keys().cloned().collect();
        ids.sort();
        ids
    }

    /// 把上一进程留下的、还没跑完的计划重新登记成**暂停着**的句柄，返回登记了几份。
    /// 判定全在 [`recover_from`]，这里只翻目录
    pub fn restore(&self, app: &AppHandle) -> usize {
        let Ok(root) = orchestra_root(app) else { return 0 };
        // 恢复出来的那几份也要照设置那个数走。`Quota` 是进程起来时按默认值建的，
        // 而 `set_total` 以前只在 `orchestra_start` 与定时任务那两条路上调过——
        // 于是"重启后恢复的计划"会按默认 6 格派发，不管用户把上限调成几（§5.15）
        let config = crate::config::load(app);
        self.quota.set_total(config.total_parallel);
        let Ok(entries) = fs::read_dir(&root) else { return 0 };
        let mut restored = 0usize;
        for path in entries.filter_map(|entry| entry.ok()).map(|entry| entry.path()) {
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let rows = read_rows(&path);
            let Some((plan, merge)) = recover_from(&rows) else { continue };
            let plan_id = plan.id.clone();
            // 目录也跟着进来：恢复的图里可能带着决策层当时派的自定义名，
            // 重启之后必须还兑得回同一副能力面（与第一次起跑同源）
            let profiles = profiles_of(&plan, &config.subagents);
            match start_at(app, plan, profiles, merge, self.quota.clone(), self.rates.clone(), true) {
                Ok(handle) => {
                    self.put(handle);
                    restored += 1;
                }
                // 起不来要说一声：一份"本该看得见却没看见"的计划不能静默
                Err(error) => eprintln!("计划 {plan_id} 这次没能恢复：{error}"),
            }
        }
        restored
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Handle>>> {
        self.plans.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StartRequest {
    pub goal: String,
    /// fanout | pipeline | bestOf | debate | hierarchical | mapReduce
    pub shape: String,
    #[serde(default)]
    pub branches: usize,
    #[serde(default)]
    pub max_parallel: usize,
    /// mapReduce 那一批要逐个处理的集合项。
    /// 展开发生在派发之前，展开后的那张图才是账本与调度看见的那一张
    #[serde(default)]
    pub items: Vec<String>,
    /// 跨 plan 抢并发位的那一档："background" | "normal" | "foreground"。
    /// 缺省 normal：让路和抢在前面都该是用户显式说过的话
    #[serde(default)]
    pub priority: Priority,
    /// **每一格**最多花多少（微元，1e-6 美元）。`0` = 不设这一项。
    /// 整份 plan 的上限与这一格的上限是两件事：前者管"这单活总共多少"，
    /// 后者管"不许有一格把前者吃光"
    #[serde(default)]
    pub node_cost_micros: i64,
    /// 每一格产出的形状检查（最少字数 / 必须包含 / 不得出现）。
    /// 三项都没说就走今天的读法：非空即可
    #[serde(default)]
    pub check: Check,
}

/// 把请求里那半份规格收干净：空白条目一条都不留。
/// 一条空的 `must_contain` 会让**每一格**都不合格（没有哪份产出"不包含空串"），
/// 空的 `forbid` 同理——那是把输入噪声读成了"全判不合格"，不是用户说过的话
fn normalize_check(spec: &Check) -> Check {
    let keep = |list: &[String]| {
        list.iter().map(|item| item.trim().to_string()).filter(|item| !item.is_empty()).collect::<Vec<_>>()
    };
    Check {
        min_chars: spec.min_chars,
        must_contain: keep(&spec.must_contain),
        forbid: keep(&spec.forbid),
    }
}

/// 拆分。P0 的 planner 是规则式的：形状决定图，而不是让模型即兴画一张图
/// （即兴画出来的图不可复现，出事时没人能说清当时到底派发了什么）。
/// `custom` 是子助理目录：起步的图只用内置角色名，但档案表必须走
/// [`profiles_of`] 这同一份兑换，所以目录从这里跟进来
pub fn build_plan(
    request: &StartRequest,
    custom: &[SubagentDef],
) -> Result<(Plan, HashMap<String, AgentProfile>, Merge), String> {
    let plan_id = format!("plan-{}", now_ms());
    let branches = request.branches.clamp(3, 8);
    let (mut plan, merge) = match request.shape.as_str() {
        "mapReduce" => {
            let base = Plan::pipeline(
                &plan_id,
                &request.goal,
                &[("read", "reader"), ("each", "worker"), ("reduce", "integrator")],
            );
            let mut mapped = base;
            if let Some(node) = mapped.nodes.iter_mut().find(|node| node.id == "each") {
                node.edge = Edge::MapReduce;
            }
            let items = if request.items.is_empty() {
                vec!["(这次没有给出集合项)".to_string()]
            } else {
                request.items.clone()
            };
            (crate::orchestra::graph::expand_map(&mapped, &items, "each")?, Merge::Concat)
        }
        "pipeline" => (
            Plan::pipeline(
                &plan_id,
                &request.goal,
                &[("read", "reader"), ("work", "worker"), ("check", "verifier")],
            ),
            Merge::Concat,
        ),
        "bestOf" => (Plan::best_of_n(&plan_id, &request.goal, "worker", branches), Merge::Best),
        // 辩论那一格读的是**原始**的分支数：它的意思是"轮数"，而 `Plan::debate` 自己夹在 1..=4
        // （一轮两次请求，那个上限是成本天花板）。上面那个 `branches` 已经被抬到至少 3 了，
        // 用它就等于让"分支=2"的辩论悄悄跑 3 轮
        "debate" => (Plan::debate(&plan_id, &request.goal, request.branches), Merge::Best),
        "hierarchical" => {
            let workers: Vec<String> = (0..branches).map(|index| format!("w{index}")).collect();
            let refs: Vec<&str> = workers.iter().map(String::as_str).collect();
            (
                Plan::hierarchical(&plan_id, &request.goal, &refs),
                Merge::ByProfilePriority {
                    order: vec!["supervisor".into(), "worker".into()],
                },
            )
        }
        _ => {
            let names: Vec<String> = (0..branches)
                .map(|index| ["reader", "worker", "verifier", "worker"][index.min(3)].to_string())
                .collect();
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            (Plan::fanout(&plan_id, &request.goal, &refs), Merge::Concat)
        }
    };
    if request.max_parallel > 0 {
        plan.max_parallel = request.max_parallel.clamp(1, 8);
    }
    plan.priority = request.priority;
    // 负数按"不设"处理：它是输入噪声，不是一个会让每一格都立刻顶住的上限
    plan.node_cost_micros = request.node_cost_micros.max(0);
    // 三项全空 = 用户没说形状，照今天的读法走（非空即可）；说了就一条不留空白地搬过去
    plan.check = if request.check == Check::default() {
        Check::plan_default()
    } else {
        normalize_check(&request.check)
    };
    let profiles = profiles_of(&plan, custom);
    Ok((plan, profiles, merge))
}

/// profile 名字 → 真正的 `AgentProfile`。只有一份，[`profiles_of`] 与派发处的
/// 兜底共用它——补做节点可能带着起步时档案表里没有的角色名（决策层分配的
/// reader/verifier），两处不同源就意味着同一张图跑出两副能力面。
///
/// 自定义子助理的名字在这里兑现：内置角色先认（撞名的定义被无视，内置赢），
/// 再查 `config.subagents`，都不认得才落回 worker+run_command 的老兜底
fn profile_for_name(name: &str, custom: &[SubagentDef]) -> AgentProfile {
    match name {
        "reader" => AgentProfile::reader(name),
        "verifier" => AgentProfile::reader(name),
        "planner" | "supervisor" | "integrator" => AgentProfile::supervisor(name),
        other => {
            if let Some(def) = custom.iter().find(|def| def.name == other) {
                return AgentProfile::custom(
                    &def.name,
                    &def.system_prompt,
                    def.model_override(),
                    def.endpoint_override(),
                    &def.tools,
                );
            }
            let mut worker = AgentProfile::worker(other);
            worker.tools.push("run_command".into());
            worker
        }
    }
}

/// 节点上那个 profile 名字 → 真正的 `AgentProfile`。只有一份。
///
/// 抽出来是因为恢复那条路也要它：复制一份就意味着"重启之后这个节点能用什么工具"
/// 可能和第一次不一样——那是权限面的漂移，比崩溃本身更难查。
/// `custom` 是设置页的子助理目录：恢复出来的图里可能带着决策层当时派的
/// 自定义名，重启之后必须还兑得回同一副能力面
fn profiles_of(plan: &Plan, custom: &[SubagentDef]) -> HashMap<String, AgentProfile> {
    let mut profiles: HashMap<String, AgentProfile> = HashMap::new();
    for node in &plan.nodes {
        let mut profile = profile_for_name(&node.profile, custom);
        // 每格的花费上限是从 plan 上取的，而恢复走的是这同一个函数：所以崩掉之前
        // 与重启之后是同一个钱袋。`0` 保持档案自己那一份（也就是"不设"）
        if plan.node_cost_micros > 0 {
            profile.budget.max_cost_micros = plan.node_cost_micros;
        }
        profiles.insert(node.profile.clone(), profile);
    }
    profiles
}

/// 这个进程里还认得的计划。面板重挂载之后要靠它找回自己刚才在盯的那份——
/// 只把 plan_id 存在组件里，切个标签页就"看不见在跑的东西"了
#[tauri::command]
pub fn orchestra_plans(hub: State<'_, Hub>) -> Vec<String> {
    hub.plan_ids()
}

/// 掏钱之前先看一眼这张图有多大。**面板不许自己算**：它以前写的是 `分支 × 2 + 1`，
/// 那等于把 `Plan::debate` 的形状在另一端抄了一份——构造器哪天改形，界面就在用户
/// 决定要不要花这笔钱的那一刻说一句假话。而且那个式子只对辩论成立，扇出的工人格
/// 是 `max_attempts: 2`，"几次请求"从来不是一个数
///
/// 只收决定**图形状**的那三样。并发上限、每格的钱、形状检查改的是"怎么跑"，
/// 不改变"有几格"
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BriefRequest {
    pub shape: String,
    #[serde(default)]
    pub branches: usize,
    #[serde(default)]
    pub items: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PlanBrief {
    /// 装配出来几格
    pub nodes: usize,
    /// 每格都一次跑成，是几发请求
    pub min_requests: usize,
    /// 每格都把尝试次数用满，是几发请求
    pub max_requests: usize,
}

impl PlanBrief {
    fn of(plan: &Plan) -> Self {
        Self {
            nodes: plan.nodes.len(),
            min_requests: plan.nodes.len(),
            max_requests: plan
                .nodes
                .iter()
                .map(|node| usize::from(node.max_attempts.max(1)))
                .sum(),
        }
    }
}

impl From<BriefRequest> for StartRequest {
    /// 剩下的那些字段不影响图长什么样，所以预览时给默认值就够了。
    /// `goal` 留空是因为它只进节点的提示词，一个格子的存在与否跟它无关
    fn from(brief: BriefRequest) -> Self {
        Self {
            goal: String::new(),
            shape: brief.shape,
            branches: brief.branches,
            max_parallel: 0,
            items: brief.items,
            priority: Priority::default(),
            node_cost_micros: 0,
            check: Check::default(),
        }
    }
}

#[tauri::command]
pub fn orchestra_plan_brief(request: BriefRequest) -> Result<PlanBrief, String> {
    // 预览只管图长什么样，档案表整份被丢掉，所以这里不读配置——
    // 一次"看一眼要花多少"的点击不该背上一次配置读盘
    let (plan, _, _) = build_plan(&request.into(), &[])?;
    Ok(PlanBrief::of(&plan))
}

#[tauri::command]
pub fn orchestra_start(app: AppHandle, hub: State<'_, Hub>, request: StartRequest) -> Result<String, String> {
    let config = crate::config::load(&app);
    let (plan, profiles, merge) = build_plan(&request, &config.subagents)?;
    let plan_id = plan.id.clone();
    // 全局上限跟着配置走：改了不必重启，下一轮派发就认（顶住的从来不是已经在跑的那几手）
    hub.quota()
        .set_total(config.total_parallel);
    let handle = start(&app, plan, profiles, merge, hub.quota(), hub.rates())?;
    hub.put(handle);
    Ok(plan_id)
}

#[tauri::command]
pub fn orchestra_pause(hub: State<'_, Hub>, plan_id: String) -> Result<usize, String> {
    let handle = hub.get(&plan_id).ok_or("没有这份计划（这份账本里没有 checkpoint，恢复不出来）")?;
    handle.pause.store(true, Ordering::Release);
    Ok(handle.status().waiting.len())
}

#[tauri::command]
pub fn orchestra_resume(hub: State<'_, Hub>, plan_id: String) -> Result<(), String> {
    let handle = hub.get(&plan_id).ok_or("没有这份计划")?;
    handle.pause.store(false, Ordering::Release);
    Ok(())
}

#[tauri::command]
pub fn orchestra_cancel(hub: State<'_, Hub>, plan_id: String) -> Result<(), String> {
    let handle = hub.get(&plan_id).ok_or("没有这份计划")?;
    handle.cancel.store(true, Ordering::Release);
    handle.pause.store(false, Ordering::Release);
    // 等驱动线程把这一轮收尾再返回：不然界面立刻读状态，读到的是"还在跑"的旧账
    handle.join();
    audit::record(
        &handle.app.path().app_data_dir().map_err(|error| error.to_string())?,
        Actor::User,
        "orchestra:cancel",
        &plan_id,
        Outcome::Ok,
    )
}

#[tauri::command]
pub fn orchestra_status(hub: State<'_, Hub>, plan_id: String) -> Result<PlanView, String> {
    let handle = hub.get(&plan_id).ok_or("没有这份计划")?;
    let state = handle.status();
    let plan = plan_of(&handle.plan);
    let rows = read_rows(&handle.ledger);
    // 每一格的读数从账本现推：面板问的是"这一格跑了多久、花了多少"，
    // 而那些事只有账本记着（内存那份在重启之后就没有了）
    let readout = node_readout(&rows);
    // 改动账本一次读盘按话题分组，节点这一侧只做并集——逐节点各读一次会把
    // 一次状态刷新变成 N 次全文件读
    let edits = node_edits(&rows, &crate::edits::edit_tallies(&handle.app));
    // 同一份已经读进来的账本再推一次"最新那一发在哪个话题"，不另开一次读盘
    let conversations = node_conversations(&rows);
    // 还有"哪一格是降级收下的"：那句"为什么没过校验"本来就在同一批行里
    let degraded = node_degradations(&rows);
    Ok(PlanView {
        plan_id: handle.plan_id.clone(),
        goal: plan.goal.clone(),
        nodes: plan
            .nodes
            .iter()
            .map(|node| {
                let held = readout.get(&node.id).copied().unwrap_or_default();
                let (files_touched, snapshotted) = edits.get(&node.id).copied().unwrap_or((0, false));
                NodeView {
                    id: node.id.clone(),
                    profile: node.profile.clone(),
                    depends_on: node.depends_on.clone(),
                    depth: plan.depth_of(&node.id),
                    status: state
                        .status
                        .get(&node.id)
                        .copied()
                        .unwrap_or(NodeStatus::Pending),
                    board_version: handle.board.version_of(&node.id),
                    tokens: held.tokens,
                    duration_ms: held.duration_ms,
                    cost_e8: held.cost_e8,
                    priced: held.priced,
                    files_touched,
                    snapshotted,
                    conversation_id: conversations.get(&node.id).cloned(),
                    degraded: degraded.get(&node.id).cloned(),
                    gate: (node.edge != Edge::FinishToStart).then(|| node.edge.gate_text()),
                }
            })
            .collect(),
        critical_path: plan.critical_path(),
        spent_tokens: state.spent_tokens,
        spent_duration_ms: state.spent_duration_ms,
        spent_cost_e8: state.spent_cost_e8,
        waiting: state.waiting.clone(),
        merged: state.merged.clone(),
        blocked_by: state.blocked_by.clone(),
        paused: handle.is_paused(),
        canceled: handle.is_canceled(),
        finished: handle.is_finished(),
        in_flight: handle.in_flight(),
        max_parallel: handle.permits.total(),
        workers: handle.pool.live(),
        worker_cap: handle.pool.max(),
        worker_ceiling: handle.pool.ceiling(),
        conflicts: conflict_count(&rows),
        priority: plan.priority,
        quota_total: handle.quota.total(),
        quota_share: handle.quota.share(plan.priority),
        quota_used: handle.quota.held_total(),
    })
}

/// 黑板那块界面的读数：每格一行，带上"谁写的、第几版"。
/// 剥成纯函数是因为冲突那一份**只有这里会把它交到人眼前**——汇合读的是每一格的结局，
/// 从不读黑板，所以这一行就是输家唯一的读者
fn board_lines(entries: Vec<Entry>) -> Vec<String> {
    entries
        .into_iter()
        .map(|entry| format!("{} v{} · {} · {}", entry.key, entry.version, entry.author, entry.value))
        .collect()
}

#[tauri::command]
pub fn orchestra_board(hub: State<'_, Hub>, plan_id: String) -> Result<Vec<String>, String> {
    let handle = hub.get(&plan_id).ok_or("没有这份计划")?;
    Ok(board_lines(handle.board.snapshot()))
}

/// 重跑单个节点：新一次尝试 = 新的话题 id，所以账本上"上一次那一段"还在
#[tauri::command]
pub fn orchestra_rerun_node(hub: State<'_, Hub>, plan_id: String, node_id: String) -> Result<u8, String> {
    let handle = hub.get(&plan_id).ok_or("没有这份计划")?;
    let rows = read_rows(&handle.ledger);
    let next_attempt = derive_attempts(&rows).get(&node_id).copied().unwrap_or(0) + 1;
    let live = plan_of(&handle.plan);
    let Some(node) = live.find(&node_id) else {
        return Err(format!("这份计划里没有节点「{node_id}」"));
    };
    let profile = handle
        .profiles
        .get(&node.profile)
        .cloned()
        .unwrap_or_else(|| AgentProfile::worker(&node.profile));
    // 重跑也是"再跑一发"，所以它吃的是与自动派发**同一套**位：这一份计划的并发位、
    // worker 租约、进程级的全局额度。这三样以前一个都没占，于是"并发不超过上限"
    // 那条只有驱动在守——用户多点几下"重跑"就能多开几路真金白银的请求
    if handle.is_canceled() {
        return Err("这份计划已经取消了：重跑单个节点只对还没跑完的计划有意义。".into());
    }
    if handle.is_finished() {
        return Err("这份计划已经有结论了。要再来一次请重新起一份——那才是新的一发，账本上也分得开。".into());
    }
    let Some(permit) = handle.permits.try_acquire() else {
        return Err(format!(
            "这份计划的并发位 {}/{} 已满，等哪一发结了再重跑。",
            handle.in_flight(),
            handle.permits.total()
        ));
    };
    let Some(lease) = handle.pool.lease() else {
        drop(permit);
        return Err(format!(
            "worker 池这一刻只开 {} 路（被失败砍过就会小于上限），这一发重跑排不进。",
            handle.pool.max()
        ));
    };
    let slot = match handle.quota.clone().try_acquire(live.priority) {
        Ok(slot) => slot,
        Err(denied) => {
            drop((permit, lease));
            return Err(match denied {
                Denied::Full => format!(
                    "全局 {} 格已被别的计划或定时任务占满，重跑等一等再试。",
                    handle.quota.total()
                ),
                Denied::AtShare => format!(
                    "「{}」这一档最多占 {} 格，此刻已经占满。",
                    priority_label(live.priority),
                    handle.quota.share(live.priority)
                ),
            });
        }
    };
    {
        let mut guard = handle.state.lock().unwrap_or_else(PoisonError::into_inner);
        guard.status.insert(node_id.clone(), NodeStatus::Running);
        guard.waiting.retain(|(held, _)| held != &node_id);
    }
    audit::record(
        &handle.app.path().app_data_dir().map_err(|error| error.to_string())?,
        Actor::User,
        "orchestra:rerun",
        &format!("{plan_id} / {node_id}"),
        Outcome::Ok,
    )?;
    let app = handle.app.clone();
    // 派发的是这一份快照：重跑那一步不该因为图后来又长了而看到别的定义
    let plan = live;
    let board = handle.board.clone();
    let state = handle.state.clone();
    let ledger = handle.ledger.clone();
    let cancel = handle.cancel.clone();
    thread::spawn(move || {
        // 三个位跟着这一发一起活、一起还：绑在闭包里，线程返回时 Drop
        let _held = (permit, lease, slot);
        run_node(&app, &plan, &node_id, next_attempt, &profile, &board, &state, &ledger, &cancel);
    });
    Ok(next_attempt)
}

/// 手工改一条依赖边（design-multi-agent.md §5.13）。它是"DAG 可视化"的写侧：
/// 边能看还不能改，"用户可干预"就只完成了一半。
///
/// 三条硬规矩都在这个函数里：要先暂停（驱动每轮取一次图，跑着改会与自动重规划抢同一格，
/// 抢输的那一半会把用户刚加的边从**运行中的图**里抹掉而账本里留着它）；只能动还没开始的节点；
/// **账本先写、图后改**——没有账本的边等于没发生过，界面上多画一条就是骗人
#[tauri::command]
pub fn orchestra_edit_edge(
    hub: State<'_, Hub>,
    plan_id: String,
    from: String,
    to: String,
    add: bool,
) -> Result<(), String> {
    let handle = hub.get(&plan_id).ok_or("没有这份计划")?;
    if !handle.is_paused() {
        return Err("改依赖要先暂停这份计划：驱动线程每轮取一次图，跑着改会和自动重规划抢同一格。".into());
    }
    let live = plan_of(&handle.plan);
    let updated = live.with_dependency(&from, &to, add)?;
    edge_gate(handle.status().status.get(&to).copied())?;
    append_rows(&handle.ledger, &[replan_row(&updated)])
        .map_err(|error| format!("账本没写进去，这条边没改：{error}"))?;
    *handle.plan.lock().unwrap_or_else(PoisonError::into_inner) = updated;
    audit::record_detail(
        &handle.app.path().app_data_dir().map_err(|error| error.to_string())?,
        Actor::User,
        "orchestra:graph-edit",
        &format!("{plan_id} / {from} → {to}"),
        Outcome::Ok,
        Some(if add { "加上依赖" } else { "去掉依赖" }.into()),
    )
}

/// 把某一格的边换成界面上能选的那一种：跑完放行 / 等某一格的校验结论 / 反复跑到自己过校验。
///
/// 这一条命令补的是"四种边里两种没有写侧"那个洞——判据(`ready_set`/`loop_open`)从第一天起就在读
/// 黑板上的结论键，而生产代码从来不往那个键写东西。现在结论键由 `run_node` 落定时发布，
/// 恢复时从账本现推回来，界面上能选的也就只有**有写者的那几种**
///
/// 四道闸与 [`orchestra_edit_edge`] 同一套：要先暂停、被改那一格必须还没开始、
/// 账本先写图后改、审计留下是谁改的。形状上的拒绝住在 [`Plan::with_edge_kind`]，这里不重述
#[tauri::command]
pub fn orchestra_set_edge_kind(
    hub: State<'_, Hub>,
    plan_id: String,
    node: String,
    kind: EdgeKind,
) -> Result<(), String> {
    let handle = hub.get(&plan_id).ok_or("没有这份计划")?;
    if !handle.is_paused() {
        return Err("换边要先暂停这份计划：驱动线程每轮取一次图，跑着换会和自动重规划抢同一格。".into());
    }
    let live = plan_of(&handle.plan);
    let updated = live.with_edge_kind(&node, kind.clone())?;
    edge_gate(handle.status().status.get(&node).copied())?;
    let label = match &kind {
        EdgeKind::FinishToStart => "跑完就放行".to_string(),
        EdgeKind::WaitForVerdict { node: waited, pass } => {
            format!("等「{waited}」的结论＝{}", verdict_value(*pass))
        }
        EdgeKind::IterateUntilPass { max_iters } => {
            format!("反复跑到自己过校验，最多 {max_iters} 轮")
        }
    };
    append_rows(&handle.ledger, &[replan_row(&updated)])
        .map_err(|error| format!("账本没写进去，这条边没改：{error}"))?;
    *handle.plan.lock().unwrap_or_else(PoisonError::into_inner) = updated;
    audit::record_detail(
        &handle.app.path().app_data_dir().map_err(|error| error.to_string())?,
        Actor::User,
        "orchestra:graph-edit",
        &format!("{plan_id} / {node}"),
        Outcome::Ok,
        Some(label),
    )
}

/// 账本读回来的一份运行记录。恢复与"上周跑了几次"的复盘都从这里出发
#[tauri::command]
pub fn orchestra_ledger(app: AppHandle, plan_id: String) -> Result<Vec<TraceRow>, String> {
    Ok(read_rows(&ledger_path(&orchestra_root(&app)?, &plan_id)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestra::graph::Node;

    fn node(id: &str, deps: &[&str]) -> Node {
        Node {
            id: id.into(),
            goal: format!("{id} 要做的事"),
            profile: "worker".into(),
            depends_on: deps.iter().map(|dep| dep.to_string()).collect(),
            edge: Edge::FinishToStart,
            max_attempts: 1,
        }
    }

    fn row(node: &str, event: &str, status: Option<NodeStatus>, attempt: u8) -> TraceRow {
        TraceRow {
            ts_ms: 1,
            plan_id: "p".into(),
            node: node.into(),
            attempt,
            event: event.into(),
            status,
            tokens: None,
            duration_ms: None,
            cost_e8: None,
            conversation_id: None,
            detail: None,
        }
    }

    fn settled_row(node: &str, event: &str, why: Option<&str>) -> TraceRow {
        TraceRow { detail: why.map(str::to_string), ..row(node, event, Some(NodeStatus::Done), 1) }
    }

    /// 条件边与循环边读的那个结论，**恢复之后还在不在**。
    ///
    /// 结论不另存一份：它就编在账本那句 `detail` 里（有内容＝没过校验）。少这一步，
    /// 重启之后"上游没过校验我才跑"那一格就永远等不到，而账本里明明写着它没过——
    /// 那正是这一族边此前的样子，只是当时谁都没往恢复这一层想
    #[test]
    fn a_recovered_run_still_knows_which_step_failed_its_check() {
        let plan = Plan::pipeline("p", "读完再验", &[("a", "worker"), ("b", "worker")])
            .with_edge_kind("b", EdgeKind::WaitForVerdict { node: "a".into(), pass: false })
            .expect("b 只在 a 没过校验的时候跑");
        let status: HashMap<String, NodeStatus> =
            [("a".to_string(), NodeStatus::Done)].into_iter().collect();

        let failed = Blackboard::new();
        seed_verdicts(&failed, &[settled_row("a", "finished", Some("结论太短"))]);
        assert_eq!(
            ready_set(&plan, &status, &failed),
            vec!["b".to_string()],
            "上游没过校验，等它失败的那一格该放行"
        );

        // 正向对照：同一个格子这次过了——还跑它就是白烧一发
        let passed = Blackboard::new();
        seed_verdicts(&passed, &[settled_row("a", "finished", None)]);
        assert!(
            ready_set(&plan, &status, &passed).is_empty(),
            "上游过了校验，条件边不该放行"
        );

        // 循环那一支：结论还是 fail 就继续转（在天花板之内），成了 pass 就收场
        let looped = Plan::pipeline("p", "自己跑对自己满意", &[("a", "worker")])
            .with_edge_kind("a", EdgeKind::IterateUntilPass { max_iters: 3 })
            .expect("自己等自己的结论");
        let nothing: HashMap<String, NodeStatus> = HashMap::new();
        assert_eq!(ready_set(&looped, &nothing, &failed), vec!["a".to_string()], "没过校验就该再转一圈");
        assert!(ready_set(&looped, &nothing, &passed).is_empty(), "已经过了校验，这一格不该再要一发");

        // `iterated` 行也带着结论：中间轮的那一次 detail 同样要说"没通过"，
        // 否则恢复出来的是一格"过了"的循环，下一轮就再也派不出去
        let mid = Blackboard::new();
        seed_verdicts(&mid, &[settled_row("a", "iterated", Some("还缺一个来源"))]);
        assert_eq!(ready_set(&looped, &nothing, &mid), vec!["a".to_string()]);
    }

    /// 这一族有四个洞是同一个形状："判据在读一个只有测试会写的键"。
    /// 所以钉四处同时成立：落定时写、恢复时推回来、界面上选出来的键就是读的那一把、
    /// 命令真的去问模型那道判据。每一根针脚用 concat! 拼——这条测试自己就在被搜的文件里
    #[test]
    fn the_verdict_fact_is_written_where_the_plan_settles_and_rebuilt_where_it_recovers() {
        // 这份文件整份是 CRLF：`include_str!` 拿到的是 `\r\n`，跨行的针脚必须先把 `\r` 去掉，
        // 否则它会以"这一行不在"的姿态红掉，而那一行明明在盘上
        let source = include_str!("orchestrator.rs").replace('\r', "");
        let settle = source
            .split(concat!("let produced = !text.trim()", ".is_empty();"))
            .nth(1)
            .expect("落定那一段得在")
            .split("\n    }")
            .next()
            .unwrap_or_default();
        assert!(
            settle.contains("verdict_value(verdict.is_pass())"),
            "落定时没把结论发布出去，条件边等的就是一个没人写的键：{settle}"
        );

        let production = source.split("\n#[cfg(test)]").next().unwrap_or_default();
        assert!(
            production.contains(concat!("let seeded = derive_status(&prior);\n    seed_verdicts(", "&board, &prior);")),
            "恢复时没从账本推回结论：重启之后所有条件边与循环边都读到一个空键"
        );
        assert!(
            !production.contains(&format!("{}verdict", '#')),
            "结论键只许由一个函数拼出来；这里出现字面量就是第二把钥匙"
        );

        let command = source
            .split(concat!("pub fn orchestra_set_edge", "_kind("))
            .nth(1)
            .expect("命令得在")
            .split("\n}")
            .next()
            .unwrap_or_default();
        for needle in [
            "handle.is_paused()",
            "live.with_edge_kind(&node, kind.clone())?",
            "replan_row(&updated)",
            "orchestra:graph-edit",
        ] {
            assert!(command.contains(needle), "换边那条命令缺了这一道：{needle}\n{command}");
        }
        assert!(
            command.find("append_rows").expect("账本那一笔") < command.find("handle.plan.lock").expect("改共享图"),
            "顺序倒了：图改了而账本没写，界面上就多出一条没发生过的边"
        );
    }

    /// 起跑写的那一行与恢复读的那一行是同一双眼睛：事件名、`node`、编码方式都要对得上
    #[test]
    fn the_checkpoint_row_is_what_recovery_reads_back() {
        let plan = Plan::debate("p", "辩一轮", 3);
        let row = planned_row(&plan, &Merge::Best).expect("图编得出来");
        assert_eq!(row.event, "planned");
        assert_eq!(row.node, "(plan)");
        let read = checkpoint_of(std::slice::from_ref(&row)).expect("自己写的行该自己读得回来");
        assert_eq!(read.plan.id, plan.id);
        assert_eq!(read.plan.node_count(), plan.node_count());
        assert_eq!(read.merge, Merge::Best);
    }

    /// 恢复靠的是那条 `planned`。没有它就是这份改动之前落盘的老账本——
    /// 凭两个节点名编一张图，边和汇合规则全是猜的，那比看不见这份计划更坏
    #[test]
    fn a_ledger_without_a_checkpoint_is_not_recovered() {
        let rows = vec![
            row("a", "queued", Some(NodeStatus::Pending), 0),
            row("a", "started", Some(NodeStatus::Running), 1),
        ];
        assert!(checkpoint_of(&rows).is_none());
        assert!(recover_from(&rows).is_none());
    }

    /// checkpoint 加上它之后那些 `replan` 行 = 崩之前那张长全了的图，外加它的汇合规则
    #[test]
    fn recovery_gets_the_grown_graph_and_how_it_merges() {
        let base = Plan::best_of_n("p", "同一件事三支", "worker", 3);
        let grown = Plan {
            nodes: vec![node("a", &[]), node("b", &["a"]), node("c", &["a"]), node("d", &["a"])],
            ..base.clone()
        };
        let rows = vec![
            planned_row(&base, &Merge::Best).expect("图编得出来"),
            TraceRow {
                detail: Some(serde_json::to_string(&grown).expect("Plan 总能编码")),
                ..row("(plan)", "replan", None, 0)
            },
            row("a", "finished", Some(NodeStatus::Done), 1),
            row("b", "queued", Some(NodeStatus::Pending), 0),
        ];
        let (plan, merge) = recover_from(&rows).expect("半路的计划该恢复得出来");
        assert_eq!(
            plan.node_count(),
            4,
            "崩溃前追加的那一支不该因为一次重启就当没发生"
        );
        assert_eq!(merge, Merge::Best, "汇合规则只能来自 checkpoint：图上推不出它");
    }

    /// 每个节点都落定的那份不该被复活成"在跑"：它要的是账本可读，不是一个线程陪着
    #[test]
    fn a_finished_plan_is_not_resurrected() {
        let rows = vec![
            planned_row(
                &Plan::new("p", "g", vec![node("a", &[]), node("b", &["a"])]),
                &Merge::Concat,
            )
            .expect("图编得出来"),
            row("a", "finished", Some(NodeStatus::Done), 1),
            row("b", "finished", Some(NodeStatus::Done), 1),
        ];
        assert!(recover_from(&rows).is_none(), "跑完的计划被恢复成暂停着，界面上就是一份假活的");
    }

    /// profile 那套规则只许有一份：恢复路径算出来的那份，要和第一次起跑那份**逐字节相同**。
    /// 两份就意味着权限面会随重启漂移——那比崩溃本身更难查
    #[test]
    fn recovered_profiles_are_the_same_ones_the_first_start_used() {
        let request = StartRequest {
            goal: "g".into(),
            shape: "fanout".into(),
            branches: 3,
            max_parallel: 2,
            items: Vec::new(),
            priority: Priority::Normal,
            node_cost_micros: 0,
            check: Check::plan_default(),
        };
        let (plan, profiles, _) = build_plan(&request, &[]).expect("扇出图该建得出来");
        assert_eq!(profiles, profiles_of(&plan, &[]), "恢复用的 profile 与第一次起的不是同一份");
        // 顺带钉住这条规则本身：worker 能跑命令，reader 不能
        assert!(
            profiles["worker"].tools.iter().any(|tool| tool == "run_command"),
            "worker 该带着 run_command"
        );
        assert!(
            !profiles["reader"].tools.iter().any(|tool| tool == "run_command"),
            "reader 不该能跑命令"
        );
    }

    /// 每一格的花费上限：请求里说了要搬到图上、每一份档案都要带上它、崩掉重启之后还是
    /// 同一个数（不然就是"崩溃前一格 20 美分、重启后一格 5 美元"）。
    /// 没设的时候一切照旧——那是今天所有既有计划的形状，也是这一步不改现有行为的证据
    #[test]
    fn a_per_node_cap_reaches_every_profile_and_survives_the_ledger() {
        let request = |cap: i64| StartRequest {
            goal: "g".into(),
            shape: "fanout".into(),
            branches: 3,
            max_parallel: 3,
            items: Vec::new(),
            priority: Priority::Normal,
            node_cost_micros: cap,
            check: Check::plan_default(),
        };
        let default_cost = crate::orchestra::graph::Budget::default().max_cost_micros;
        let (untouched, profiles_off, _) = build_plan(&request(0), &[]).expect("扇出图该建得出来");
        assert_eq!(untouched.node_cost_micros, 0, "没设就是没设，别造一个数出来");
        assert_eq!(
            profiles_off["worker"].budget.max_cost_micros, default_cost,
            "没设每格上限时，档案那一份预算该原样不动"
        );
        // 负数是输入噪声，不是"一分钱都不许花"：读成后者的话每一格都会在第一发之前顶住
        assert_eq!(build_plan(&request(-5), &[]).expect("负数也该建得出图").0.node_cost_micros, 0);

        let (capped, profiles_on, _) = build_plan(&request(200_000), &[]).expect("带上限的图");
        assert_eq!(capped.node_cost_micros, 200_000, "请求里那一格的花费要照搬到图上");
        assert!(
            profiles_on.values().all(|p| p.budget.max_cost_micros == 200_000),
            "每一份档案都该带着同一个上限：{:?}",
            profiles_on
                .values()
                .map(|p| (p.name.clone(), p.budget.max_cost_micros))
                .collect::<Vec<_>>()
        );
        // 恢复走的是 profiles_of(&plan) 这同一个函数，所以"钱袋跟着图回来"才是真的跟着回来
        assert_eq!(profiles_of(&capped, &[]), profiles_on, "重启之后不该换另一个钱袋");
        // 加这个字段之前落盘的那份图要还读得回来：`replan` 行里存的是整张图
        let mut json = serde_json::to_value(&capped).expect("Plan 总能编码");
        json.as_object_mut().unwrap().remove("nodeCostMicros");
        let older: Plan = serde_json::from_value(json).expect("旧账本那份图要还读得回来");
        assert_eq!(older.node_cost_micros, 0, "缺这一格就是没设，不是拿个随机数");
    }

    /// 形状检查以前是 `run_node` 里写死的一句 `min_chars: 1`，于是 `must_contain` 与 `forbid`
    /// 两条在真实链路上永远为空——`forbid` 上那句"用来拦住结论里贴了 token，而不是装饰"
    /// 没有人执行。规格现在住在 plan 上、请求里能写；这里问三件事：
    /// **搬得过去、空白条目不会把每一格都判死、什么都没说时今天的读法一个字没变**
    #[test]
    fn a_shape_spec_written_by_the_request_reaches_the_nodes_and_drops_blanks() {
        let request = |check: Check| StartRequest {
            goal: "g".into(),
            shape: "fanout".into(),
            branches: 3,
            max_parallel: 2,
            items: Vec::new(),
            priority: Priority::Normal,
            node_cost_micros: 0,
            check,
        };
        let (plan, _, _) = build_plan(&request(Check {
            min_chars: 20,
            // 一条带空白、一条纯空、一条真空：三条都不该留在规格里
            must_contain: vec![" 结论 ".into(), "   ".into(), String::new()],
            forbid: vec!["sk-live-".into()],
        }), &[])
        .expect("带形状的图");
        assert_eq!(plan.check.min_chars, 20, "字数那条要搬过去");
        assert_eq!(
            plan.check.must_contain,
            vec!["结论".to_string()],
            "两边空白要剪掉，空的条目一条都不留：{:?}",
            plan.check.must_contain
        );
        assert_eq!(plan.check.forbid, vec!["sk-live-".to_string()]);
        // 这三条断言问的是"这份规格真的会判"，不是"字段抄对了"
        assert!(
            matches!(plan.check.judge("结论"), Verdict::Fail { .. }),
            "20 字的要求没生效"
        );
        assert!(matches!(
            plan.check.judge("这一段话够长了，可是没有那一项要求里的东西，真的够长了。"),
            Verdict::Fail { why } if why.contains("结论")
        ), "缺词要说得出缺哪一项");
        assert!(
            matches!(
                plan.check.judge("结论在这里，只是顺手贴了 sk-live-abcdefg 一串进去。"),
                Verdict::Fail { why } if !why.contains("sk-live-abcdefg") && why.contains("不该出现")
            ),
            "禁词要判死，但回显要遮起来：那串东西会进话题上下文，抄回去等于再抄一次秘密"
        );
        assert_eq!(
            build_plan(&request(Check::default()), &[]).expect("安静请求").0.check,
            Check::plan_default(),
            "什么都没说时就是今天的读法：非空即可"
        );
    }

    /// 一条"上游 → 自循环节点 → 下游"的图。循环边的判据都拿它测——
    /// 辩论那个形状自己已经不用循环边了（它现在是交替链，见 `graph.rs`）
    fn looped_plan(until_key: &str, until_value: &str, max_iters: u8) -> Plan {
        Plan::new(
            "p",
            "g",
            vec![
                node("up", &[]),
                Node {
                    id: "con".into(),
                    goal: "反复逼近".into(),
                    profile: "worker".into(),
                    depends_on: vec!["up".into()],
                    edge: Edge::Loop {
                        until_key: until_key.into(),
                        until_value: until_value.into(),
                        max_iters,
                    },
                    max_attempts: 1,
                },
                node("down", &["con"]),
            ],
        )
    }

    /// 循环边要真的能转第二圈：跑完一轮记一格轮数，而 `loop_open` 认的就是那一格
    #[test]
    fn a_loop_edge_actually_iterates_instead_of_stopping_at_one() {
        let plan = looped_plan("stop", "3", 3);
        let board = Blackboard::new();
        let con = plan.find("con").expect("图里有那个循环节点");
        assert!(loop_open(con, &board), "一轮都没跑，循环该是开的");
        let key = iterations_key("con");
        board.compare_swap(&key, 0, "1", "con");
        assert!(loop_open(con, &board), "跑了一轮，上限还有");
        let version = board.version_of(&key);
        board.compare_swap(&key, version, "3", "con");
        assert!(!loop_open(con, &board), "用完 max_iters 就该关——那个上限是必填的成本天花板");
    }

    /// 轮数只有一格。旧代码把轮数写在 `until_key` 上、却从 `{node}#iters` 读，
    /// 两个人各数各的，所以那条循环从来没真的转过第二圈
    #[test]
    fn the_iteration_counter_has_one_key() {
        let plan = looped_plan("stop", "3", 3);
        let board = Blackboard::new();
        let con = plan.find("con").expect("图里有那个循环节点");
        board.compare_swap("stop", 0, "9", "con");
        assert!(loop_open(con, &board), "往停止条件那格上写个大数不该被当成已经跑了 9 轮");
        let version = board.version_of("stop");
        board.compare_swap("stop", version, "3", "con");
        assert!(!loop_open(con, &board), "而它真的到了 until_value，那就是别人下的收场决定");
    }

    /// `Iterated` 既不是落定也不算成功：它还会被派一次，而下游不会提前放行
    #[test]
    fn an_iterated_round_is_re_dispatched_without_releasing_downstream() {
        assert!(!NodeStatus::Iterated.is_terminal(), "落定就等于再也不派");
        assert!(!NodeStatus::Iterated.succeeded(), "成功就等于下游放行");

        let plan = looped_plan("stop", "3", 3);
        let board = Blackboard::new();
        let mut status = HashMap::new();
        status.insert("up".to_string(), NodeStatus::Done);
        status.insert("con".to_string(), NodeStatus::Iterated);
        let ready = ready_set(&plan, &status, &board);
        assert!(ready.contains(&"con".to_string()), "循环还开着就该再派 con：{ready:?}");
        assert!(!ready.contains(&"down".to_string()), "con 没落定，下游不该提前跑");

        status.insert("con".to_string(), NodeStatus::Done);
        assert!(
            ready_set(&plan, &status, &board).contains(&"down".to_string()),
            "con 定下来之后下游才该进来"
        );
    }

    /// 上游交给这一发的产出：`work` 的提示词里没有 `read` 的结论，它就只是又一次独立问答
    #[test]
    fn a_downstream_node_is_handed_its_upstreams_output() {
        let plan = Plan::pipeline(
            "p",
            "g",
            &[("read", "reader"), ("work", "worker"), ("check", "verifier")],
        );
        let board = Blackboard::new();
        board.compare_swap("read", 0, "读到的那件事", "read");

        let (prompt, stamps) = handoff(&plan, "work", &board).expect("work 有上游，就该有材料");
        assert!(prompt.contains("读到的那件事"), "上游结论没进材料：{prompt}");
        assert_eq!(stamps, vec!["read v1".to_string()], "戳里要带版本：循环与重跑吃的是哪一版");
        assert!(handoff(&plan, "read", &board).is_none(), "没有上游就不该硬造一段空话");
    }

    /// 取不到的那一条要明写在材料里。安静地少一份上游，下游就以为自己看全了
    #[test]
    fn a_missing_upstream_is_said_out_loud_not_left_silent() {
        let plan = Plan::pipeline("p", "g", &[("read", "reader"), ("work", "worker")]);
        let board = Blackboard::new();
        let (prompt, stamps) = handoff(&plan, "work", &board).expect("有依赖就有这一段");
        assert!(prompt.contains("没有产出可交"), "少一份上游要看得见：{prompt}");
        assert_eq!(stamps, vec!["read (无)".to_string()]);
    }

    /// 崩在"已经重规划过"之后：重启时账本里那条 replan 行要把追加出来的节点带回来，
    /// 否则"从检查点恢复"会悄悄少一步——而少的那一步正是失败那一步的重做
    #[test]
    fn a_replanned_node_comes_back_from_the_ledger_after_a_crash() {
        let base = Plan::new("p", "g", vec![node("a", &[]), node("b", &["a"])]);
        let updated = replan(&base, "a", "它只说了半句话").expect("这一步失败了，就该有重做的路");
        let rows = vec![
            row("a", "failed", Some(NodeStatus::Failed), 1),
            TraceRow {
                detail: Some(serde_json::to_string(&updated).expect("图总能编码")),
                ..row("(plan)", "replan", None, 0)
            },
        ];

        let live = plan_from_ledger(base.clone(), &rows);
        assert!(
            live.find("a#retry").is_some(),
            "重做的那一步要回来，实际节点是：{:?}",
            live.nodes.iter().map(|node| node.id.clone()).collect::<Vec<_>>()
        );
        assert_eq!(
            live.find("b").unwrap().depends_on,
            vec!["a#retry".to_string()],
            "下游等的是重做那一步，这条改写也要一起回来"
        );

        let status = derive_status(&rows);
        assert_eq!(status.get("a"), Some(&NodeStatus::Failed));
        let ready = ready_set(&live, &status, &Blackboard::new());
        assert!(ready.contains(&"a#retry".to_string()), "重做的那一步该排上：{ready:?}");
        assert!(!ready.contains(&"b".to_string()), "重做还没跑，下游不该 ready：{ready:?}");
    }

    /// 一份写坏的账本不该把图改小，别的 plan 的行也不该混进来
    #[test]
    fn a_broken_or_foreign_replan_row_leaves_the_graph_alone() {
        let base = Plan::new("p", "g", vec![node("a", &[]), node("b", &["a"])]);
        let half_written = TraceRow {
            detail: Some(r#"{"id":"p","goal":"g""#.into()),
            ..row("(plan)", "replan", None, 0)
        };
        assert_eq!(
            plan_from_ledger(base.clone(), &[half_written]).node_count(),
            2,
            "半份 JSON 该整行跳过，而不是当成一张空图"
        );

        let foreign = TraceRow {
            plan_id: "other".into(),
            detail: Some(serde_json::to_string(&base).unwrap()),
            ..row("(plan)", "replan", None, 0)
        };
        assert_eq!(plan_from_ledger(base.clone(), &[foreign]).node_count(), 2);
        assert_eq!(plan_from_ledger(base.clone(), &[]).node_count(), 2, "没有 replan 行就该原样");
    }

    /// 档位是用户说过的话，不是我们替他猜的：请求里给了哪一档就照它建图。
    /// 另外两问同样要紧：**加字段之前落盘的那份图还得读得回来**（`replan` 行存的是整张图，
    /// 读不回来等于恢复时丢图），而图长过一份之后档位得跟着走（漏一处就是拿错的档问额度）
    #[test]
    fn a_tier_survives_the_ledger_and_every_growth_of_the_graph() {
        let (plan, _, _) = build_plan(&StartRequest {
            goal: "g".into(),
            shape: "fanout".into(),
            branches: 3,
            max_parallel: 3,
            items: Vec::new(),
            priority: Priority::Background,
            node_cost_micros: 0,
            check: Check::plan_default(),
        }, &[])
        .unwrap();
        assert_eq!(plan.priority, Priority::Background, "请求里那一档要照搬到图上");

        let mut json = serde_json::to_value(&plan).expect("Plan 总能编码");
        json.as_object_mut().unwrap().remove("priority");
        let older: Plan = serde_json::from_value(json).expect("加字段之前落盘的那份图要还读得回来");
        assert_eq!(older.priority, Priority::Normal, "缺这一格就是默认那一档，不报错也不随机");

        let base = Plan { priority: Priority::Background, ..Plan::hierarchical("p", "g", &["w0"]) };
        let grew = followups(&base, "supervisor", "补做：再查一个来源").expect("监督者说了还缺");
        assert_eq!(grew.priority, Priority::Background, "补第二轮不该顺手换个档");
        let redone = replan(&base, "w0", "它没跑成").expect("失败那一支该换来做的节点");
        assert_eq!(redone.priority, Priority::Background, "重做那一轮同样要带着档位走");
        let mut mappable = Plan::pipeline("p", "g", &[("read", "reader"), ("each", "worker")]);
        if let Some(node) = mappable.nodes.iter_mut().find(|node| node.id == "each") {
            node.edge = Edge::MapReduce;
        }
        mappable.priority = Priority::Background;
        let expanded = crate::orchestra::graph::expand_map(
            &mappable,
            &["a".to_string(), "b".to_string()],
            "each",
        )
        .expect("map 展开要能跑");
        assert_eq!(expanded.priority, Priority::Background, "展开出来的那张图也是同一档");
    }

    /// 预算那道闸**读的是哪一格**，纯函数测不到：它能算对，调用方照样可以喂 0——
    /// 而"喂 0"正是这条预算存在以来唯一没被发现的形状（$5 的顶一次也没顶住过）。
    /// 没有 `AppHandle` 就进不去驱动，所以这里退而钉住源码本身：那一次调用的三个入参
    /// 必须是快照里的三个数，一个都不许是常量
    #[test]
    fn the_budget_gate_reads_the_ledgers_money_not_a_constant() {
        let source = include_str!("orchestrator.rs");
        let call: String = source
            .split("budget.exhausted(")
            .nth(1)
            .expect("驱动里就该有一处问预算")
            .split(" {")
            .next()
            .expect("那次调用要以一个代码块收尾")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for wanted in [
            "snapshot.spent_tokens",
            "snapshot.spent_cost_e8",
            "contributions.len() + missing.len()",
        ] {
            assert!(call.contains(wanted), "预算那道闸该读 `{wanted}`，实际是：{call}");
        }
        // 每一发终结行都得带上钱，两处落账也都要把它滚进计划总额——
        // 少任何一处，界面上那一格就是"没价表"或者总额永远比真数小。
        // 针脚用 `concat!` 拼：这条测试自己就在被搜的那份文件里，写成一整串会数到自己
        let written = source.matches(concat!("cost_e8: cost_seen", ".then_some(cost_e8)")).count();
        assert_eq!(written, 2, "终结行有两处（停在待审批 / 跑完），少一处就是那一发没记账");
        let rolled = source.matches(concat!("guard.spent_cost_e8 ", "+= cost_e8;")).count();
        assert_eq!(rolled, 2, "两处落账都要把这一发的钱滚进计划总额");
    }

    /// 编排那一发到底"没人看"登记了没有、重跑那一条有没有走同一套并发位。
    /// 这三件事都在要 `AppHandle` 的路上（驱动线程 / 命令），没有行为测试入口——
    /// 而它们恰恰是"库层成立、链路没成立"的那一类：`escalate` 与 `Quota` 自己都有测试，
    /// 编排这条路一次都没去调。所以这里钉形状：调用点必须存在，删掉就红
    #[test]
    fn the_orchestra_paths_that_should_share_the_gates_actually_do() {
        let source = include_str!("orchestrator.rs");
        // 1) 每一发都要登记进"没人看"那张表，否则高危动作弹的是没人看的窗、600 秒后记成"摇头"。
        //    要切出 `run_node` 那一段来数：只在整份文件里数一次，等于承认"在随便哪调一下"也算
        let node_body = source
            .split("fn run_node")
            .nth(1)
            .expect("跑一格那个函数")
            .split("\n/// ")
            .next()
            .expect("到下一个文档注释为止");
        assert_eq!(
            node_body
                .matches(concat!("crate::tasks::escalate::", "watch_run("))
                .count(),
            1,
            "编排的话题从没登记过 = 验收那条\"危险操作必须审批\"在编排里走的是即时审批那条路"
        );
        // 1b) 每一格自己的预算闸也必须在这条路上。档案里那第六维（`budget`）以前是
        //     一个没人读的字段——判据、请求、面板都能说得通，就是没人执行它（§5.16）
        assert_eq!(
            node_body.matches("exhausted_per_task(").count(),
            1,
            "每一格的花费上限没有执行点，那份档案里的预算就还是装饰"
        );
        // 1c) 形状检查也不能再写死在函数里：写死了，`must_contain` / `forbid` 就没人能设
        assert!(
            node_body.contains("plan.check.clone()"),
            "校验规格必须由计划带着走；写死一句 `min_chars: 1` 等于只有\"非空\"这一种检查"
        );
        // 2) 重跑单节点吃的是与自动派发同一套位：计划并发位 + worker 租约 + 全局额度
        let rerun = source
            .split("fn orchestra_rerun_node")
            .nth(1)
            .expect("重跑那个命令")
            .split("\n#[tauri::command]")
            .next()
            .expect("到下一个命令为止");
        for gate in [
            "handle.permits.try_acquire()",
            "handle.pool.lease()",
            "handle.quota.clone().try_acquire(live.priority)",
            "handle.is_finished()",
            "handle.is_canceled()",
        ] {
            assert!(rerun.contains(gate), "重跑那一条没走 `{gate}`：{rerun}");
        }
        // 3) 恢复出来的计划也要照设置那个全局上限走。同一份现读的配置还要喂
        //    档案兑现（profile_for_name 的自定义子助理目录）：读两遍就是两个时刻
        //    的两份配置，配额与能力面会各说各话
        let restore = source
            .split("pub fn restore(")
            .nth(1)
            .expect("恢复那个函数")
            .split("\n    fn lock(")
            .next()
            .expect("到 lock 为止");
        assert!(
            restore.contains("let config = crate::config::load(app);")
                && restore.contains("set_total(config.total_parallel)")
                && restore.contains("profiles_of(&plan, &config.subagents)"),
            "恢复时没套配置的上限（或没带子助理目录），那几份计划就会按默认跑：{restore}"
        );
        // 4) 面板那一格的"这格在哪个话题"与"为什么是降级收下的"：两份推导都在纯函数里
        //    （它们各自有单元测试），而把它们接进 `NodeView` 的那两条线在要 `State<Hub>`
        //    的路上，行为测不到。删掉任何一条，面板只是少一列，全库一条都不红
        let status = source
            .split("pub fn orchestra_status")
            .nth(1)
            .expect("状态那个命令")
            .split("\n#[tauri::command]")
            .next()
            .expect("到下一个命令为止");
        for wire in [
            "node_conversations(&rows)",
            "node_degradations(&rows)",
            "conversation_id: conversations.get(&node.id).cloned()",
            "degraded: degraded.get(&node.id).cloned()",
            // 放行方式那一句也得从**图上**现读。契约守卫比的是手拼的那个值，
            // 它看不见这一行有没有被接上——删掉之后面板上那一列会永远空白而全库不红
            "gate: (node.edge != Edge::FinishToStart).then(|| node.edge.gate_text())",
        ] {
            assert!(status.contains(wire), "那一格没接上 `{wire}`：面板会静悄悄少一列");
        }
    }

    /// 验收第 5 条那句"实时输出"与 MessageBus 那格的"流式"，落的都是这一条通道：
    /// 每一发的 `Delta` 边到边转给界面，同时替编排器记账（`observe`）。
    /// 记账那一半有测试，**转发那一半此前一根针都没钉**：把它删掉，全库不红，
    /// 面板从此只等得到整发跑完的结果——"实时"两个字就没了
    /// 验收第 4 条"用户可暂停、恢复"此前只有命令与按钮，**一条测试都没有**：
    /// 把驱动循环里那一格暂停判据删掉，计划照跑而界面仍显示"已暂停"——这是这一族里最坏的
    /// 一种（用户以为按住了，钱却继续花）。这里钉四件事：循环在派发**之前**读那个位、
    /// 暂停与恢复翻的是同一个位、取消要把暂停放开（否则循环卡在 sleep 里收不了尾）、
    /// 改边仍然必须先暂停
    #[test]
    fn pausing_a_plan_actually_stops_the_dispatch_loop() {
        let source = include_str!("orchestrator.rs");
        // 暂停那一支：从判据到下一次真正读图之间的那段
        let branch = source
            .split(concat!("driver_", "pause.load("))
            .nth(1)
            .expect("驱动循环里没有暂停判据")
            .split("let mut driver_plan")
            .next()
            .expect("暂停那一支要在派发之前收尾");
        assert!(branch.contains("continue"), "暂停必须跳过整轮派发，不是做做样子：{branch}");
        assert!(
            branch.contains("sleep"),
            "暂停是等，不是让线程退出——线程一退这份计划就再也不会醒：{branch}"
        );

        let paused = source
            .split(concat!("pub fn orchestra_", "pause"))
            .nth(1)
            .expect("暂停那条命令")
            .split("\n#[tauri::command]")
            .next()
            .unwrap_or_default();
        assert!(paused.contains("handle.pause.store(true"), "暂停翻的不是循环在读的那个位：{paused}");
        let resumed = source
            .split(concat!("pub fn orchestra_", "resume"))
            .nth(1)
            .expect("恢复那条命令")
            .split("\n#[tauri::command]")
            .next()
            .unwrap_or_default();
        assert!(resumed.contains("handle.pause.store(false"), "恢复翻的不是同一个位：{resumed}");
        let canceled = source
            .split(concat!("pub fn orchestra_", "cancel"))
            .nth(1)
            .expect("取消那条命令")
            .split("\n#[tauri::command]")
            .next()
            .unwrap_or_default();
        assert!(
            canceled.contains("handle.pause.store(false"),
            "取消之后还留着暂停位，这份计划在界面上就同时是「已取消」与「已暂停」，\
             而恢复出来也是暂停着的——一个再也跑不动的僵尸"
        );
        // 取消必须比暂停先问：否则「暂停 + 取消」这份计划要等一次恢复才收得了尾
        let cancel_at = source
            .find("if driver_cancel.load(")
            .expect("循环里没有取消判据");
        let pause_at = source
            .find(concat!("driver_", "pause.load("))
            .expect("循环里没有暂停判据");
        assert!(
            cancel_at < pause_at,
            "两道判据的顺序反了：取消在 {cancel_at}，暂停在 {pause_at}"
        );
        assert!(
            source.contains("if !handle.is_paused()"),
            "改边不再要求先暂停了：那等于改一份正在派发的图"
        );
    }

    #[test]
    fn the_recorder_forwards_every_event_it_tallies() {
        let body = include_str!("orchestrator.rs")
            .split("impl EventSink for Recorder {")
            .nth(1)
            .expect("那一层包装")
            .split("\n}")
            .next()
            .unwrap_or_default();
        assert!(body.contains("observe("), "记账那一半要在：{body}");
        assert!(
            body.contains(concat!("self.for", "ward.send(event)")),
            "转发那一半被摘掉了，面板只能等整发跑完：{body}"
        );
    }

    /// 报给界面的三份形状与前端类型一字不差。这条测试的存在理由很具体：
    /// `PlanView` 带着 `workers`/`conflicts` 发了很久，而 TS 里没有这两个字段，
    /// 全绿通过——派生值没人读，就等于没说
    /// "这一步动过哪些文件、那边有没有快照"从两本账现拼：账本行给"哪一格用了哪个话题"，
    /// 改动账本给"那个话题碰了哪些文件"。面板的载荷里这一格以前根本没有
    #[test]
    fn node_edits_dedupe_across_attempts_and_say_when_there_is_no_snapshot() {
        let row = |node: &str, conversation: &str| TraceRow {
            ts_ms: 0,
            plan_id: "p".into(),
            node: node.into(),
            attempt: 1,
            event: "finished".into(),
            status: None,
            tokens: Some(1),
            duration_ms: None,
            cost_e8: None,
            conversation_id: Some(conversation.into()),
            detail: None,
        };
        let tally = |files: &[&str], snapshotted: bool| crate::edits::EditTally {
            files: files.iter().map(|path| path.to_string()).collect(),
            snapshotted,
        };
        let mut tallies = std::collections::BTreeMap::new();
        tallies.insert("c1".to_string(), tally(&["/w/a.rs", "/w/b.rs"], true));
        // 第二次尝试又碰了 a.rs：合起来是两个文件，不是三个
        tallies.insert("c2".to_string(), tally(&["/w/a.rs"], false));
        let edits = node_edits(
            &[row("worker-a", "c1"), row("worker-a", "c2"), row("worker-b", "c1")],
            &tallies,
        );
        assert_eq!(edits.get("worker-a"), Some(&(2usize, true)), "同一处改动不该被数两次");
        assert_eq!(edits.get("worker-b"), Some(&(2usize, true)));
        assert!(
            !node_edits(&[row("idle", "never-ran")], &tallies).contains_key("idle"),
            "没跑过的一格不该凭空有文件"
        );
        // 正对着看：话题不在账上时那一格就是没有，而不是报 0 个文件说成"动过"
        assert!(node_edits(&[], &tallies).is_empty(), "空账本拼出空表");
    }

    /// 节点行上那句"有快照可回"要点得动，就得知道该打开哪一发。
    /// 三条各自对应一种坏法：按行序取最后一行（重跑时会跳到被作废的那一发）、
    /// 把计划级那一行当成某一格的（跳到一份根本不是这一发写出来的话题）、
    /// 没有话题时编一个（点开是空的，用户以为面板在骗他）
    #[test]
    fn the_node_row_points_at_its_latest_attempt_not_whatever_row_came_last() {
        let row = |node: &str, attempt: u8, conversation: Option<&str>| TraceRow {
            ts_ms: 0,
            plan_id: "p".into(),
            node: node.into(),
            attempt,
            event: "finished".into(),
            status: None,
            tokens: Some(1),
            duration_ms: None,
            cost_e8: None,
            conversation_id: conversation.map(str::to_string),
            detail: None,
        };
        // 第 2 次的行排在前面：账本重放时先后不等于新旧
        let rows = [
            row("worker-a", 2, Some("plan-p-worker-a-2")),
            row("worker-a", 1, Some("plan-p-worker-a-1")),
            row(PLAN_SCOPE, 1, Some("plan-p-scope")),
            row("worker-b", 1, None),
        ];
        let conversations = node_conversations(&rows);
        assert_eq!(
            conversations.get("worker-a").map(String::as_str),
            Some("plan-p-worker-a-2"),
            "重跑之后要回的是最新那一发，不是第 1 次留下的那份快照"
        );
        assert!(
            !conversations.contains_key(PLAN_SCOPE),
            "计划级那一行不是一格的话题：{:?}",
            conversations
        );
        assert!(
            !conversations.contains_key("worker-b"),
            "没记下话题的一格就该没有，而不是指向别处"
        );
        assert!(node_conversations(&[]).is_empty(), "空账本拼出空表");
    }

    /// "降级收下"与"干净地过了校验"以前在面板上长成同一个样子（都叫「完成」），
    /// 而那一句"为什么不合格"其实一直就写在账本上。这条钉住四种区分：
    /// 过了校验的（detail 空）不算降级、`failed` 那行不算降级（它就是失败，另一格状态会说）、
    /// `started` 那行的 detail（吃了谁的材料）不算降级、计划级那行（熔断那句）不算某一格的
    #[test]
    fn a_node_that_passed_the_check_is_not_the_same_as_one_that_was_degraded() {
        let row = |node: &str, event: &str, attempt: u8, detail: Option<&str>| TraceRow {
            ts_ms: 0,
            plan_id: "p".into(),
            node: node.into(),
            attempt,
            event: event.into(),
            status: None,
            tokens: Some(1),
            duration_ms: None,
            cost_e8: None,
            conversation_id: None,
            detail: detail.map(str::to_string),
        };
        let rows = [
            row("clean", "started", 1, Some("上游 a#1")),
            row("clean", "finished", 1, None),
            row("sloppy", "started", 1, Some("上游 b#1")),
            row("sloppy", "finished", 1, Some("少一段结尾的 ```")),
            row("broken", "failed", 1, Some("没有产出")),
            row(PLAN_SCOPE, "blocked", 0, Some("档案「worker」连着失败 3 次，熔断 60 秒")),
        ];
        let degraded = node_degradations(&rows);
        assert_eq!(
            degraded.get("sloppy").map(String::as_str),
            Some("少一段结尾的 ```"),
            "过了校验与将就收下的那两份，界面上必须分得开：{:?}",
            degraded
        );
        assert!(!degraded.contains_key("clean"), "过了校验不该被说成降级：{:?}", degraded);
        assert!(!degraded.contains_key("broken"), "那一格是失败，不是降级：{:?}", degraded);
        assert!(!degraded.contains_key(PLAN_SCOPE), "熔断那句不是一格的降级");
        assert_eq!(degraded.len(), 1, "只该挑出那一个：{:?}", degraded);
    }

    /// 重跑之后要看的是**最新那一发**过没过：第 1 次将就、第 2 次真的过了，
    /// 那一格就不该再顶着"降级"
    #[test]
    fn a_rerun_that_passed_clears_the_degradation() {
        let row = |event: &str, attempt: u8, detail: Option<&str>| TraceRow {
            ts_ms: 0,
            plan_id: "p".into(),
            node: "worker-a".into(),
            attempt,
            event: event.into(),
            status: None,
            tokens: Some(1),
            duration_ms: None,
            cost_e8: None,
            conversation_id: None,
            detail: detail.map(str::to_string),
        };
        let cleared = node_degradations(&[
            row("finished", 1, Some("少了标题")),
            row("finished", 2, None),
        ]);
        assert!(
            !cleared.contains_key("worker-a"),
            "第 2 次过了校验，那句\"降级\"就得跟着撤掉：{:?}",
            cleared
        );
        // 反向对照：把两行的先后换过来（账本重放时顺序不可信），判据仍然取尝试号大的那个
        let stale = node_degradations(&[
            row("finished", 2, None),
            row("finished", 1, Some("少了标题")),
        ]);
        assert!(!stale.contains_key("worker-a"), "{:?}", stale);
    }

    #[test]
    fn the_panel_payloads_match_the_frontend_types() {
        let view = PlanView {
            plan_id: "p".into(),
            goal: "g".into(),
            nodes: vec![NodeView {
                id: "a".into(),
                profile: "worker".into(),
                depends_on: vec![],
                depth: 0,
                status: NodeStatus::Pending,
                board_version: 1,
                // 每一格的花费与耗时：以前这三格在 TS 里根本没有，界面上也就没有
                tokens: 42,
                duration_ms: 900,
                cost_e8: 45,
                priced: true,
                files_touched: 2,
                snapshotted: true,
                // 守卫只看填了值的可选项，所以这一格给一个真的话题 id
                conversation_id: Some("plan-p-a-1".into()),
                degraded: Some("少一段结尾的 ```".into()),
                // 给一个**不是默认**的值：守卫比的是序列化出来的键与值，
                // 一个大家都长样的默认值藏不住"这一格根本没接"
                gate: Some("等「write#verdict」＝fail".into()),
            }],
            critical_path: vec!["a".into()],
            spent_tokens: 0,
            spent_duration_ms: 0,
            spent_cost_e8: 0,
            waiting: vec![],
            merged: None,
            blocked_by: None,
            paused: false,
            canceled: false,
            finished: false,
            in_flight: 0,
            max_parallel: 1,
            workers: 0,
            worker_cap: 1,
            worker_ceiling: 1,
            conflicts: 0,
            priority: Priority::default(),
            quota_total: 6,
            quota_share: 6,
            quota_used: 0,
        };
        let value = serde_json::to_value(&view).expect("PlanView 总能编码");
        crate::test_support::assert_matches_ts(&value, "PlanView");
        crate::test_support::assert_matches_ts(&serde_json::to_value(&view.nodes[0]).unwrap(), "NodeView");
        let row = TraceRow {
            // 可选项都要有值：`assert_matches_ts` 比的是**序列化出来**的键，
            // 一个 None 的字段在它眼里等于不存在——这是这个守卫自己的边界，别信得太满
            tokens: Some(3),
            duration_ms: Some(1_200),
            cost_e8: Some(45),
            conversation_id: Some("conv-1".into()),
            detail: Some("一行说明".into()),
            ..row("a", "queued", Some(NodeStatus::Pending), 0)
        };
        crate::test_support::assert_matches_ts(&serde_json::to_value(&row).unwrap(), "TraceRow");
    }

    /// **请求这个方向以前没有形状守卫**：面板发的键与 Rust 收的键对不上时，serde 会按
    /// `#[serde(default)]` 安静补上——而"每格花费上限"补的是 0（= 不设），"形状检查"补的是全空。
    /// 静默少一道闸比当场报错难发现得多，尤其少的是**花钱**那道。两头一起钉：
    /// 键名对齐（含嵌套的 `check`）+ 打错一个键要被拒，而不是被补成默认值
    #[test]
    fn the_start_request_shape_is_pinned_from_both_ends() {
        let request = StartRequest {
            goal: "g".into(),
            shape: "fanout".into(),
            branches: 3,
            max_parallel: 3,
            items: vec!["a".into()],
            priority: Priority::Foreground,
            node_cost_micros: 200_000,
            check: Check {
                min_chars: 40,
                must_contain: vec!["结论".into()],
                forbid: vec!["sk-".into()],
            },
        };
        let wire = serde_json::to_value(&request).expect("请求总能编码");
        crate::test_support::assert_matches_ts(&wire, "StartRequest");
        crate::test_support::assert_matches_ts(&wire["check"], "StartCheck");

        let mut misspelt = wire.clone();
        let object = misspelt.as_object_mut().expect("顶层是个对象");
        object.remove("nodeCostMicros");
        object.insert("nodeCostMikros".into(), serde_json::json!(200_000));
        assert!(
            serde_json::from_value::<StartRequest>(misspelt).is_err(),
            "打错一个键被安静补成默认值了——那笔每格上限等于没设"
        );
        assert!(
            serde_json::from_value::<StartRequest>(wire).is_ok(),
            "对齐的那份反而被拒：那这条测试测不到它想测的东西"
        );
    }

    /// 面板那句"这张图几格、几发请求"以前自己写了一个形状算式（轮数翻倍再加一）。
    /// 那是在另一端**抄**了一份 `Plan::debate`：抄的那一刻起，构造器改形不会改注释，
    /// 只会让这句话在用户决定要不要花这笔钱的时候说一句假话。现在两头读同一座构造器
    #[test]
    fn the_brief_reports_the_same_graph_the_start_command_builds() {
        let items = || vec!["一".to_string(), "二".to_string(), "三".to_string(), "四".to_string()];
        for shape in ["fanout", "pipeline", "bestOf", "debate", "hierarchical", "mapReduce"] {
            for branches in [0usize, 1, 2, 3, 5, 9] {
                let brief = orchestra_plan_brief(BriefRequest {
                    shape: shape.into(),
                    branches,
                    items: items(),
                })
                .unwrap_or_else(|error| panic!("{shape} 报不出数：{error}"));
                let (plan, _, _) = build_plan(&StartRequest {
                    goal: "g".into(),
                    shape: shape.into(),
                    branches,
                    max_parallel: 0,
                    items: items(),
                    priority: Priority::default(),
                    node_cost_micros: 0,
                    check: Check::default(),
                }, &[])
                .expect("同一份请求开跑也建得出图");
                assert_eq!(
                    brief,
                    PlanBrief::of(&plan),
                    "{shape} / 分支={branches}：预览看见的图与开跑建出的图不是同一张"
                );
                assert!(brief.nodes >= 1, "{shape} 报出了零格");
                assert!(
                    brief.max_requests >= brief.min_requests,
                    "{shape} 的上限比下限还低：那这两个数没人能读懂"
                );
            }
        }
    }

    /// 那两个服务商**通常不是同一个数**：扇出的工人格是 `max_attempts: 2`，
    /// 所以"这一发几次请求"本来就取决于失败几次。以前面板只报一个数，
    /// 而那个算式只对辩论成立——它连自己只对一种形状成立都没说
    #[test]
    fn the_brief_endpoints_are_the_builders_numbers_not_a_formula() {
        let brief = |shape: &str, branches: usize| {
            orchestra_plan_brief(BriefRequest { shape: shape.into(), branches, items: vec![] })
                .expect("报得出数")
        };

        let debate = brief("debate", 3);
        assert_eq!(
            (debate.nodes, debate.min_requests, debate.max_requests),
            (7, 7, 7),
            "辩论：3 轮交替 6 格 + 1 格裁判，每格一次跑成"
        );
        // 轮数的天花板在构造器那一头，不在界面上那个 input 的 min/max
        assert_eq!(brief("debate", 9).nodes, 9, "辩论的轮数夹在 1..=4：装配那一头的成本天花板");
        assert_eq!(brief("debate", 0).nodes, 3, "轮数最少一轮");

        let fanout = brief("fanout", 3);
        assert_eq!(
            (fanout.nodes, fanout.min_requests, fanout.max_requests),
            (5, 5, 8),
            "扇出：split + 3 个工人 + gather，工人格会补试一次，所以请求数不是一个数"
        );
    }

    /// 前端那一头的三件事一起钉：算式真的没了、三个数真的都被读出来、面板问的那个名字
    /// 真的注册过。最后一条尤其要紧——面板对"问不到"的处理是**一个字也不显示**，
    /// 所以忘了注册、或 invoke 里打错一个字母，都不会报错，只会让这句话安静地消失
    #[test]
    fn the_panel_reads_the_brief_instead_of_recomputing_the_shape() {
        let panel = include_str!("../../../src/components/orchestra-panel.tsx").replace('\r', "");
        assert!(panel.contains("orchestraPlanBrief("), "面板没再去问后端这张图多大");
        assert!(!panel.contains("branches *"), "面板又自己算起形状来了");
        assert!(!panel.contains("2 + 1"), "面板里还留着那个抄来的请求数算式");
        for read in ["brief.nodes", "brief.minRequests", "brief.maxRequests"] {
            assert!(panel.contains(read), "{read} 没人读：这个数就成了只给测试看的");
        }
        // 集合项那一栏只有一处解析，预览与开跑读的是同一个解析
        assert_eq!(panel.matches("items_of(items)").count(), 2, "预览与开跑该读同一个 items 解析");
        assert_eq!(panel.matches("items.split").count(), 0, "第二处手工解析 items：两边可以算出不同的图");

        // 名字从 wrapper 那一头取出来，再拿去问注册表：两头任一边打错都红
        let client = include_str!("../../../src/lib/orchestra.ts").replace('\r', "");
        let called = client
            .split("invoke<PlanBrief>(\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("orchestra.ts 里得有一句 invoke<PlanBrief>(\"命令名\")")
            .to_string();
        let lib = include_str!("../lib.rs");
        assert!(
            lib.contains(&format!("orchestrator::{called},")),
            "面板问的是「{called}」，而注册表里没有这一条：invoke 会失败，失败的样子是这句话根本不出现在界面上"
        );
    }

    /// 预览这个方向也要形状守卫：它收的是 `deny_unknown_fields`，但**面板那一头**
    /// 少写一个键（比如忘了 `items`）后端只会按默认值补——于是面板说的格数
    /// 和真跑的那张图可以差着一批集合项，而没人报错
    #[test]
    fn the_brief_request_shape_is_pinned_from_both_ends() {
        let brief = PlanBrief { nodes: 5, min_requests: 5, max_requests: 8 };
        crate::test_support::assert_matches_ts(
            &serde_json::to_value(&brief).expect("PlanBrief 总能编码"),
            "PlanBrief",
        );

        let wire = serde_json::json!({"shape": "mapReduce", "branches": 3, "items": ["a", "b"]});
        let parsed = serde_json::from_value::<BriefRequest>(wire.clone())
            .expect("面板发的那份键要对上");
        assert_eq!(parsed.items.len(), 2, "集合项被安静地丢掉了：预览会少报一整批格子");

        let mut misspelt = wire.as_object().expect("顶层是个对象").clone();
        misspelt.remove("items");
        misspelt.insert("item".into(), serde_json::json!(["a", "b"]));
        assert!(
            serde_json::from_value::<BriefRequest>(serde_json::Value::Object(misspelt)).is_err(),
            "打错一个键被补成了默认值——那批集合项在预览里凭空消失了"
        );
    }

    /// 每一格的花费/耗时是从账本行推的，不是从内存表里再存一份。
    /// 重跑与循环各落一行终结行，所以这一格的总账是它们的和
    #[test]
    fn a_node_s_totals_are_summed_from_its_own_ledger_rows() {
        let finished = |node: &str, attempt: u8, tokens: u64, ms: u64, cost: Option<i64>| TraceRow {
            tokens: Some(tokens),
            duration_ms: Some(ms),
            cost_e8: cost,
            ..row(node, "finished", Some(NodeStatus::Done), attempt)
        };
        let rows = vec![
            // 起跑行不带数字：它只证明"这一发开始了"，把它算进去就是每发双计
            row("a", "started", Some(NodeStatus::Running), 1),
            finished("a", 1, 100, 900, Some(40)),
            finished("a", 2, 60, 300, Some(10)),
            // 这台模型没价表：那两格是 0 与"没量到"的区别，`priced` 必须留在 false
            finished("b", 1, 80, 500, None),
            // 计划级那一行带着**整份计划**的累计 token，它不是哪个工人的产出
            TraceRow {
                tokens: Some(240),
                ..row("(plan)", "blocked", Some(NodeStatus::Blocked), 0)
            },
        ];
        let readout = node_readout(&rows);
        let a = readout["a"];
        assert_eq!((a.tokens, a.duration_ms, a.cost_e8, a.priced), (160, 1_200, 50, true));
        let b = readout["b"];
        assert_eq!((b.tokens, b.duration_ms, b.cost_e8), (80, 500, 0));
        assert!(!b.priced, "没价表的那一发不该报成\"花了 $0\"");
        assert!(!readout.contains_key("c"), "没跑过的格子不该有个读数");
        assert!(
            !readout.contains_key("(plan)"),
            "计划级那一行不该在界面上凭空造出一个工人"
        );
    }

    #[test]
    fn recovery_from_the_ledger_never_re_runs_a_finished_node() {
        // 验收第 3 条
        let plan = Plan::new("p", "g", vec![node("a", &[]), node("b", &["a"]), node("c", &["a"])]);
        let rows = vec![
            row("a", "queued", Some(NodeStatus::Pending), 0),
            row("a", "started", Some(NodeStatus::Running), 1),
            row("a", "finished", Some(NodeStatus::Done), 1),
        ];
        let status = derive_status(&rows);
        assert_eq!(status.get("a"), Some(&NodeStatus::Done));
        let ready = ready_set(&plan, &status, &Blackboard::new());
        assert!(!ready.contains(&"a".to_string()), "已完成的节点又被派发了，那份恢复就是假的");
        assert_eq!(ready, vec!["b".to_string(), "c".to_string()], "a 的两支下游该一起就绪");
        assert_eq!(derive_attempts(&rows).get("a"), Some(&1u8));
    }

    #[test]
    fn a_conditional_edge_is_gated_by_a_board_fact_not_by_a_prompt() {
        let plan = Plan {
            id: "p".into(),
            goal: "g".into(),
            priority: Priority::default(),
            node_cost_micros: 0,
            check: Check::plan_default(),
            nodes: vec![
                node("check", &[]),
                Node {
                    id: "fix".into(),
                    goal: "改".into(),
                    profile: "worker".into(),
                    depends_on: vec!["check".into()],
                    edge: Edge::Conditional { key: "needs-fix".into(), equals: "yes".into() },
                    max_attempts: 1,
                },
            ],
            budget: Default::default(),
            max_parallel: 1,
        };
        let board = Blackboard::new();
        let mut status = HashMap::new();
        status.insert("check".to_string(), NodeStatus::Done);
        assert!(ready_set(&plan, &status, &board).is_empty(), "黑板上还没有结论，条件边不该放行");
        board.compare_swap("needs-fix", 0, "no", "check");
        assert!(ready_set(&plan, &status, &board).is_empty(), "结论说不用改，就真的不派");
        board.compare_swap("needs-fix", 1, "yes", "check");
        assert_eq!(ready_set(&plan, &status, &board), vec!["fix".to_string()]);
    }

    #[test]
    fn a_looped_edge_stops_at_its_own_limit() {
        let plan = Plan {
            id: "p".into(),
            goal: "g".into(),
            priority: Priority::default(),
            node_cost_micros: 0,
            check: Check::plan_default(),
            nodes: vec![Node {
                id: "con".into(),
                goal: "反驳".into(),
                profile: "worker".into(),
                depends_on: vec![],
                edge: Edge::Loop {
                    until_key: "rounds".into(),
                    until_value: "2".into(),
                    max_iters: 2,
                },
                max_attempts: 1,
            }],
            budget: Default::default(),
            max_parallel: 1,
        };
        let board = Blackboard::new();
        let empty: HashMap<String, NodeStatus> = HashMap::new();
        assert_eq!(ready_set(&plan, &empty, &board), vec!["con".to_string()]);
        board.compare_swap("rounds", 0, "2", "con");
        assert!(ready_set(&plan, &empty, &board).is_empty(), "到了停止条件就不再排这一轮");
    }

    #[test]
    fn cancel_cascades_to_downstream_as_skipped_not_failed() {
        let plan = Plan::new("p", "g", vec![node("a", &[]), node("b", &["a"]), node("c", &["b"])]);
        let mut status = HashMap::new();
        status.insert("a".to_string(), NodeStatus::Canceled);
        let changes = cascade(&plan, &status);
        assert_eq!(
            changes.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
            vec!["b".to_string()],
            "b 自己没做错任何事，它只是轮不到"
        );
        assert!(changes.iter().all(|(_, status)| *status == NodeStatus::Skipped));
    }

    #[test]
    fn one_failing_branch_does_not_take_the_whole_plan_down() {
        // 验收第 2 条：单支失败 → 汇合仍出结论，且缺一支写在结论里
        let rows = vec![
            row("a", "finished", Some(NodeStatus::Done), 1),
            row("b", "failed", Some(NodeStatus::Failed), 1),
        ];
        let status = derive_status(&rows);
        let missing: Vec<String> = status
            .iter()
            .filter(|(_, held)| **held == NodeStatus::Failed)
            .map(|(id, _)| id.clone())
            .collect();
        let merged = judge::merge(
            &Merge::Concat,
            vec![Contribution {
                node: "a".into(),
                profile: "worker".into(),
                text: "还在".into(),
                verdict: Verdict::Pass,
            }],
            missing,
        );
        assert!(merged.text.contains("b"), "缺一支要在结论里说出来：{}", merged.text);
    }

    #[test]
    fn an_unanswered_approval_becomes_a_wait_not_a_pass() {
        let mut tally = Tally::default();
        let pending = ChatEvent::Tool {
            id: "call-1".into(),
            name: "write_file".into(),
            status: ToolStatus::Pending,
            risk: "elevated".into(),
            input: "写入 a.rs".into(),
            output: None,
            arguments: None,
            pass_reason: None,
            content_chars: None,
        };
        observe(&mut tally, &pending);
        assert_eq!(tally.unanswered, vec!["call-1".to_string()], "弹了窗没人答，就是有操作被拦住了");
        observe(
            &mut tally,
            &ChatEvent::Tool {
                id: "call-1".into(),
                name: "write_file".into(),
                status: ToolStatus::Denied,
                risk: "elevated".into(),
                input: "写入 a.rs".into(),
                output: Some("用户拒绝执行该操作。".into()),
                arguments: None,
                pass_reason: None,
                content_chars: None,
            },
        );
        assert!(tally.unanswered.is_empty(), "已经有结论的审批不该还算在等人");
        observe(
            &mut tally,
            &ChatEvent::Done {
                input_tokens: 120,
                output_tokens: 30,
                duration_ms: 900,
                cached_tokens: Some(100),
                entry_ids: Vec::new(),
                model: "测试模型".into(),
                context_tokens: 128_000,
            },
        );
        assert_eq!(tally.input_tokens, 120);
        assert_eq!(tally.cached_tokens, 100);
        // 产出从增量里攒出来：编排器要的是这一次 run 说了什么，不是整段话题历史
        observe(&mut tally, &ChatEvent::Delta { text: "第一半".into() });
        observe(&mut tally, &ChatEvent::Delta { text: "第二半".into() });
        assert_eq!(tally.text, "第一半第二半");
    }

    #[test]
    fn the_breaker_opens_on_consecutive_failures_and_heals_on_success() {
        let breaker = Breaker::new(2, Duration::from_millis(40));
        assert!(breaker.allows(Instant::now()));
        assert!(!breaker.record_failure(), "一次失败不该熔断");
        assert!(breaker.record_failure(), "连续两次就该熔断");
        assert!(breaker.is_open());
        assert!(!breaker.allows(Instant::now()), "熔断期间不派发新节点");
        assert!(
            breaker.allows(Instant::now() + Duration::from_millis(60)),
            "到期要放一个探针出去，而不是永久锁死"
        );
        breaker.record_success();
        assert!(!breaker.is_open(), "成功了就该合上");
    }

    /// 熔断那句要报得出**它自己**被建成什么数。以前"熔断 60 秒"是调用点上的字面量，
    /// 把窗口改成别的数时账本上那句不会跟着变，也没有任何测试会红——
    /// 那是"双轨真相"最小的一个样本：同一个数住在两处
    #[test]
    fn the_breaker_notice_reads_off_the_breaker_not_off_the_call_site() {
        let short = Breaker::new(2, Duration::from_secs(15)).notice("scout");
        assert!(short.contains("连着失败 2 次"), "失败次数得是自己那个数：{short}");
        assert!(short.contains("15 秒"), "窗口得是自己那个窗口：{short}");
        assert!(!short.contains("60"), "调用点那个字面量不该还在：{short}");
        let long = Breaker::new(5, Duration::from_secs(90)).notice("worker");
        assert!(long.contains("连着失败 5 次") && long.contains("90 秒"), "{long}");
    }

    #[test]
    fn the_rate_bucket_hands_out_permits_and_refills_by_elapsed_time() {
        let bucket = TokenBucket::new(3, 1.0);
        let now = Instant::now();
        assert!(bucket.try_take(1, now));
        assert!(bucket.try_take(1, now));
        assert!(bucket.try_take(1, now));
        assert!(!bucket.try_take(1, now), "桶空了就不该再发，限流不是摆设");
        assert!(
            !bucket.try_take(1, now + Duration::from_millis(600)),
            "一秒一个令牌，600ms 只回补 0.6 个，还不够一次放行"
        );
        assert!(
            bucket.try_take(1, now + Duration::from_millis(1200)),
            "补够一个就该放行，不然限流会变成永久闸死"
        );
        // 攒一晚上换一次"超过容量"的突发不是限流，是蓄水池：回补的上界就是容量本身
        let refill = TokenBucket::new(2, 100.0);
        assert!(refill.try_take(2, Instant::now()));
        let later = Instant::now() + Duration::from_secs(600);
        assert!(refill.try_take(2, later), "空闲久了该补满，补满是补到容量而不是补到无限");
        assert!(!refill.try_take(1, later), "补满之后一次突发不能越过容量");
    }

    /// 限流那两个数以前是就地字面量，于是**谁也测不到它**：把 `* 4` 改成 `* 40` 全库不会红。
    /// 现在它们住在 [`RATE_BURST_PER_SLOT`] / [`RATE_REFILL_PER_SEC`]，这里断言容量跟着并发上限走、
    /// 回补是每秒半个。回补是从**上一次到访**算起的，所以下面那两笔时间要连着看
    #[test]
    fn the_rate_bucket_follows_the_parallel_cap_per_endpoint() {
        let rates = Rates::default();
        let now = Instant::now();
        for index in 0..4 {
            assert!(
                rates.permit("deepseek", 1, now),
                "上限 1 的账号第一秒有 1×4 发的余量，第 {index} 发被拒了"
            );
        }
        assert!(!rates.permit("deepseek", 1, now), "第 5 发在余量之外，该被拒——限流不是摆设");
        for index in 0..12 {
            assert!(rates.permit("openai", 3, now), "上限 3 就该有 3×4 发的余量，第 {index} 发被拒了");
        }
        assert!(!rates.permit("openai", 3, now), "多出来的 8 发是并发上限给的，不是凭空多出来的");

        // 正向对照：回补那半个/秒也是这只桶的数
        assert!(!rates.permit("deepseek", 1, now + Duration::from_millis(500)), "半秒只补 0.25 个，还不够放行");
        assert!(
            rates.permit("deepseek", 1, now + Duration::from_millis(2_600)),
            "再等两秒多就该补出一个可用的令牌"
        );
    }

    /// **一个账号一只桶，跨计划共享**：两份计划打同一个服务商档案时各配一只桶，等于把速率乘二。
    /// 反过来，换了一个档案就是另一个账号，让它陪上一个账号共用一只桶只是白白挨限。
    /// 第三格钉"没选档案"：空串也是一个账号，不是"免限流"
    #[test]
    fn one_endpoint_gets_one_bucket_across_plans() {
        let rates = Rates::default();
        let now = Instant::now();
        for _ in 0..4 {
            assert!(rates.permit("p-1", 1, now));
        }
        assert!(!rates.permit("p-1", 1, now), "计划甲把这只桶用空了");
        assert!(
            !rates.permit("p-1", 1, now),
            "计划乙是另一份计划、同一个账号：它不该凭空拿到一只新桶"
        );
        for index in 0..4 {
            assert!(
                rates.permit("p-2", 1, now),
                "别的档案有自己的额度，第 {index} 发被上一个档案拖住就是过紧"
            );
        }
        for _ in 0..4 {
            assert!(rates.permit("", 1, now), "没选档案也要落到一只桶上");
        }
        assert!(!rates.permit("", 1, now), "而且是同一只：再问一次不该又攒出一桶");
    }

    /// 共享桶的容量**不能随到达顺序变**：小计划先来、大计划后到时桶要变宽；
    /// 反过来大计划先来时，小计划不许把已经能攒的余量抽窄
    #[test]
    fn a_shared_bucket_grows_but_never_shrinks() {
        let start = Instant::now();
        let narrow_first = Rates::default();
        for _ in 0..4 {
            assert!(narrow_first.permit("p", 1, start));
        }
        assert!(!narrow_first.permit("p", 1, start), "只有上限 1 的计划碰过时，天花板是 4");
        // 上限 3 的计划进来：天花板长到 12，但余额是按时间回补的，不是凭空多出来的。
        // 20 秒 × 0.5/秒 = 10 个令牌——够把"长到了 12"这件事试出来（天花板还停在 4 的话这里第五发就红）
        let later = start + Duration::from_secs(20);
        for index in 0..10 {
            assert!(
                narrow_first.permit("p", 3, later),
                "长容量是给更大并发用的，第 {index} 发被旧的 4 挡住了"
            );
        }
        assert!(
            !narrow_first.permit("p", 3, later),
            "补到 12 就停：桶变宽不等于给同一个账号加倍速率"
        );

        let wide_first = Rates::default();
        for _ in 0..12 {
            assert!(wide_first.permit("p", 3, start));
        }
        let room = start + Duration::from_secs(30);
        for index in 0..12 {
            assert!(
                wide_first.permit("p", 1, room),
                "上限 1 的计划不许把已经长到 12 的天花板抽回 4，第 {index} 发"
            );
        }
        assert!(!wide_first.permit("p", 1, room), "到 12 为止，多出来的不发");
    }

    /// 派发口只许问一次限流，问的是**共享那几只桶**，而键来自当次派发那一刻的服务商档案。
    /// 一只计划私有的桶就是回到"按计划乘速率"；键换成编排角色名就是"按角色乘速率"
    #[test]
    fn the_dispatcher_asks_the_shared_buckets_once_per_dispatch() {
        let source = include_str!("orchestrator.rs").replace('\r', "");
        let production = source.split("\n#[cfg(test)]").next().unwrap_or_default();
        assert_eq!(
            production.matches("driver_rates.permit(").count(),
            1,
            "派发口只该有一处问限流；两处会各扣一次令牌"
        );
        assert_eq!(
            production.matches("TokenBucket::new(").count(),
            1,
            "造桶那个式子只许住在 Rates::permit 里，别处再写一次就是第二个数"
        );
        assert!(
            production
                .contains("let endpoint = crate::config::load(&app_for_driver).active_profile_id;"),
            "限流的键要来自当次派发那一刻的服务商档案——按角色名分桶等于对同一个账号把速率乘上角色数"
        );
        assert!(
            !production.contains("plan_bucket("),
            "旧的『一份计划一只桶』回来了：它隐含『一份计划一个账号』，而那不成立"
        );
    }

    /// 黑板的 CAS 只在"expected 是作者真读过的那一版"时才是防线。
    /// 循环计数器是链路里唯一一个"照读到的值算一个新值"的写，所以它必须走 `bump`；
    /// 其余三处覆盖的是**自己那一格的上一版**，拿当下版本当 expected 才是对的语义。
    /// 这一条量的是派生值的写法：把 `bump` 换回"get 一次 + swap 当下版本"，单线程测试全都绿，
    /// 而并发下会把别人那一笔顶掉——并且每次都返回 `Applied`（见 runtime 那两条对照）
    #[test]
    fn the_loop_counter_bumps_instead_of_bargaining_with_the_current_version() {
        let source = include_str!("orchestrator.rs").replace('\r', "");
        let production = source.split("\n#[cfg(test)]").next().unwrap_or_default();
        assert_eq!(
            production.matches("board.bump(").count(),
            1,
            "顶账只该有一处：轮数计数器"
        );
        assert!(
            production.contains("board.bump(&iterations_key("),
            "轮数不再走 bump：那它就又是「读一版再顶当下版本」那种每次都必过的写"
        );
        assert!(
            !production.contains("used + 1"),
            "读出来再加一的式子又回到了生产段：这种派生值不能拿 `version_of` 当 expected"
        );
        assert_eq!(
            production.matches("board.compare_swap(").count(),
            3,
            "三处覆盖写：恢复时补的结论、这一格的结论、这一格的校验判断。它们盖的是自己那一格的上一版，不该改成 bump"
        );
    }

    /// 冲突里输掉的那一份存在 `{key}#lost-N` 一格。注释以前写着"等汇合来裁决"，而汇合的材料是
    /// **每一格的结局**（`Contribution`），从不读黑板——那是注释替代码撒的一次谎。这里钉两头：
    /// 人在那块「黑板键」里看得到它（这是它唯一的读者），以及裁决入口仍然只有一个
    #[test]
    fn the_loser_copy_reaches_the_board_view_and_the_aggregator_still_never_reads_the_board() {
        use crate::orchestra::runtime::Cas;

        let board = Blackboard::new();
        board.compare_swap("draft", 0, "甲的第一版", "a");
        board.compare_swap("draft", 1, "乙的第二版", "b");
        let stale = board.compare_swap("draft", 1, "甲的旧改动", "a");
        assert!(matches!(stale, Cas::Conflict { held: 2, .. }), "拿旧版本号写就该被拒：{stale:?}");

        let lines = board_lines(board.snapshot());
        assert!(
            lines
                .iter()
                .any(|line| line.contains("draft#lost-1") && line.contains("甲的旧改动") && line.contains("· a ·")),
            "输的那一份在人眼前也消失了，那它就真的没有读者：{lines:?}"
        );

        let source = include_str!("orchestrator.rs").replace('\r', "");
        let production = source.split("\n#[cfg(test)]").next().unwrap_or_default();
        assert_eq!(production.matches("judge::merge(").count(), 1, "裁决入口只许一个");
        assert_eq!(
            production.matches(".snapshot(").count(),
            1,
            "整份黑板只被那块界面读一次。多一处读它，就是有人在给汇合偷偷造第二个材料来源"
        );
    }

    /// 被顶回来的那一次写要成为账本上的一行。判据两头：写成功**没有要说的事**（所以不该有行），
    /// 而顶回来那一行要说清"现在这一格是谁的、第几版、你那份去哪了"，并且一格数字都不带
    #[test]
    fn a_refused_write_becomes_a_ledger_row_the_panel_can_read_after_a_restart() {
        use crate::orchestra::runtime::Cas;

        assert!(
            conflict_row("p", "a", 1, &Cas::Applied(2)).is_none(),
            "写成功也记一行的话，每一发都会多一行\"冲突\""
        );

        let row = conflict_row(
            "p",
            "a",
            2,
            &Cas::Conflict { held: 3, holder: "b".into(), lost_key: "a#lost-1".into() },
        )
        .expect("顶回来就该有一行");
        assert_eq!(row.event, "conflict");
        assert_eq!(row.attempt, 2, "要说得出是哪一发顶回来的：重跑与循环各占一行");
        assert!(row.status.is_none(), "这一格的终局由它自己那一行说，这里重复就是两份真相");
        // 数字格一格都不带：派生视图按行累加，带一份就是把同一笔数两次
        assert!(row.tokens.is_none() && row.duration_ms.is_none() && row.cost_e8.is_none());
        assert!(row.conversation_id.is_none(), "冲突不是某一发的产出，别把它挂到某个话题上");
        let detail = row.detail.clone().expect("要说清谁赢了、输的那份去哪了");
        assert!(
            detail.contains("v3") && detail.contains("b") && detail.contains("a#lost-1"),
            "光说\"有冲突\"等于没说：{detail}"
        );

        let rows = vec![
            row.clone(),
            TraceRow { event: "finished".into(), ..row.clone() },
            TraceRow { event: "conflict".into(), ..row },
        ];
        assert_eq!(conflict_count(&rows), 2, "那个数只该数 conflict 行");
    }

    /// 链路那一半：视图读账本，而内存那份计数**不再是一个答案**。
    /// 这是 §5.6 那条规矩的第二使用——同一个问题不许有两个问法（那边删的是 `Barrier::arrived`）
    #[test]
    fn the_conflict_count_on_the_panel_comes_from_the_ledger_not_from_memory() {
        let source = include_str!("orchestrator.rs").replace('\r', "");
        let production = source.split("\n#[cfg(test)]").next().unwrap_or_default();
        assert_eq!(production.matches("conflict_row(").count(), 2, "定义 + 派发处那一次落账");
        assert!(
            production.contains("conflict_row(&plan.id, node_id, attempts, &cas)"),
            "落账那一句问的必须是这一次写拿回来的 `cas`：换个别的值就等于什么都没问"
        );
        assert_eq!(production.matches("conflict_count(").count(), 2, "定义 + 视图那一次读数");
        assert!(production.contains("conflicts: conflict_count(&rows)"), "视图那一格没读账本");
        assert!(
            !production.contains("board.conflicts()"),
            "内存那份计数又变成第二个答案了：重启之后两边会给出不同的数"
        );
    }

    /// 账本上会出现的事件名，每一个都得有中文读数。少一条**不会掉行**（未知事件照原样显示是设计），
    /// 但会把一个英文原词摆在一整片中文里——而面板上"没翻译"长得和"没数据"一模一样。
    /// `conflict` 这一行就是被这条扫出来的：加它的时候顺手发现 `iterated` 早就漏在那儿了
    #[test]
    fn every_event_name_that_reaches_the_ledger_has_a_chinese_label() {
        let source = include_str!("orchestrator.rs").replace('\r', "");
        let production = source.split("\n#[cfg(test)]").next().unwrap_or_default();
        let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for prefix in ["event: \"", "=> \""] {
            let mut at = 0usize;
            while let Some(offset) = production[at..].find(prefix) {
                let start = at + offset + prefix.len();
                let rest = &production[start..];
                let Some(end) = rest.find('"') else { break };
                let name = &rest[..end];
                if !name.is_empty() && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
                    names.insert(name.to_string());
                }
                at = start;
            }
        }
        assert!(names.contains("conflict"), "扫不到 conflict：这条测试的扫法要改，不然它什么都量不到");
        assert!(names.len() >= 8, "只扫出 {} 个事件名，扫法大概漏了：{:?}", names.len(), names);

        let client = include_str!("../../../src/lib/orchestra.ts").replace('\r', "");
        let labels = client
            .split("EVENT_LABELS: Record<string, string> = {")
            .nth(1)
            .expect("orchestra.ts 里那张事件名表要在");
        for name in &names {
            assert!(labels.contains(&format!("{name}:")), "{name} 会出现在账本上，界面上却没有中文读数");
        }
    }

    /// 组件 8 那一格写着"四种汇合策略"。数出来确实是四种，而 `build_plan` 只造得出三种：
    /// `Merge::Vote` 从落地那天起**没有生产者**——界面上六个形状选不出它，checkpoint 行里
    /// 也就永远写不出它，只有 judge.rs 那条同名测试会构造它（"形状有、生产者没有"第四次）。
    ///
    /// 为什么不接：Vote 的计票口径是"逐字相同才算同一票"，而节点产出是自由文本，两支逐字相同
    /// 几乎不发生 → 它在真实链路上会退化成"第一支过校验的"，与 `Best` 同义而更贵更难解释。
    /// 要让它真有意义，得先有一层"把意见归一成可比较的票"，那是产品决定，而且多半要再加一次
    /// 模型往返＝钱。所以这一格记成**明账**，并留一条会在"半接"时红的钉：三种各有写者（正对照），
    /// 第四种在链路上仍然是 0
    #[test]
    fn three_of_the_four_merge_strategies_have_a_producer_and_the_fourth_is_an_open_debt() {
        let judge = include_str!("judge.rs").replace('\r', "");
        let body = judge
            .split("pub enum Merge {")
            .nth(1)
            .expect("Merge 是个枚举")
            .split("\n}")
            .next()
            .unwrap_or_default();
        let variants: Vec<&str> = body
            .lines()
            .map(str::trim)
            .filter(|line| !line.starts_with("//") && !line.starts_with('#'))
            .filter_map(|line| line.split([' ', '{', ',']).next())
            .filter(|name| name.chars().next().is_some_and(|c| c.is_ascii_uppercase()))
            .collect();
        assert_eq!(
            variants,
            vec!["Concat", "Vote", "Best", "ByProfilePriority"],
            "枚举长出新策略或改了名：这条会红，而那正是该回头数生产者的时刻"
        );

        let source = include_str!("orchestrator.rs").replace('\r', "");
        let production = source.split("\n#[cfg(test)]").next().unwrap_or_default();
        for written in ["Merge::Concat", "Merge::Best", "Merge::ByProfilePriority {"] {
            assert!(production.contains(written), "{written} 没有生产者：那种汇合在链路上永不成立");
        }
        assert_eq!(
            production.matches("Merge::Vote").count(),
            0,
            "Vote 被接进链上了？那就把这一条与文档 §5.33 一起改掉，别留一句过期的明账"
        );
    }

    #[test]
    fn the_ledger_file_name_survives_a_hostile_plan_id() {
        let name = ledger_name("../../etc/passwd");
        assert!(!name.contains('.') || name.ends_with(".jsonl"), "plan_id 会变成文件名：{name}");
        assert!(!name.contains("..") && !name.contains('/') && !name.contains('\\'), "{name} 还在越界");
        assert!(name.ends_with(".jsonl"));
    }

    /// 窃取那一行只是 Trace：它不带状态，也不该在恢复时改动这张图
    #[test]
    fn a_stolen_dispatch_row_leaves_the_recovered_plan_alone() {
        let rows = vec![
            row("w0", "queued", Some(NodeStatus::Pending), 0),
            row("w0", "stolen", None, 0),
        ];
        assert_eq!(
            derive_status(&rows).get("w0"),
            Some(&NodeStatus::Pending),
            "一个不是状态变更的事件不该把 Pending 改成别的"
        );
        let plan = Plan::new("p", "g", vec![node("w0", &[])]);
        let restored = plan_from_ledger(plan, &rows);
        assert_eq!(restored.nodes.len(), 1, "图里不该因为这一行多出节点");
        assert_eq!(restored.find("w0").map(|held| held.id.as_str()), Some("w0"));
    }

    #[test]
    fn a_broken_line_in_the_ledger_does_not_erase_the_plan() {
        let dir = std::env::temp_dir().join(format!("aglab-orchestra-{}", now_ms()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("p.jsonl");
        append_rows(
            &path,
            &[row("a", "finished", Some(NodeStatus::Done), 1), row("b", "queued", Some(NodeStatus::Pending), 0)],
        )
        .unwrap();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all("这一行不是 JSON\n".as_bytes())
            .unwrap();
        let rows = read_rows(&path);
        assert_eq!(rows.len(), 2, "一行写坏不该让整份计划看起来从未跑过");
        assert_eq!(derive_status(&rows).get("a"), Some(&NodeStatus::Done));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_rule_plan_splits_into_at_least_three_parallel_nodes_for_every_shape() {
        // 验收第 1 条：拆得开，且每一支都有自己的 profile 与工具面
        for shape in ["fanout", "pipeline", "bestOf", "debate", "hierarchical", "mapReduce"] {
            let request = StartRequest {
                goal: "把这个仓库里会改磁盘的地方列出来".into(),
                shape: shape.into(),
                branches: 3,
                max_parallel: 3,
                items: vec!["src/a.rs".into(), "src/b.rs".into(), "src/c.rs".into()],
                priority: Priority::Normal,
                node_cost_micros: 0,
                check: Check::plan_default(),
            };
            let (plan, profiles, _) = build_plan(&request, &[]).expect("形状要能建出图");
            assert!(!plan.nodes.is_empty(), "{shape} 建出了空图");
            assert!(
                plan.nodes.iter().all(|node| profiles.contains_key(&node.profile)),
                "{shape} 里有节点找不到自己的档案：权限面就没了出处"
            );
            assert!(plan.max_parallel >= 1, "{shape} 的并发上限至少是 1");
            plan.topo().unwrap_or_else(|cycles| panic!("{shape} 建出了环：{cycles}"));
        }
        let (fanout, _, _) = build_plan(&StartRequest {
            goal: "g".into(),
            shape: "fanout".into(),
            branches: 3,
            max_parallel: 3,
            items: Vec::new(),
            priority: Priority::Normal,
            node_cost_micros: 0,
            check: Check::plan_default(),
        }, &[])
        .unwrap();
        let first_wave = ready_set(&fanout, &HashMap::new(), &Blackboard::new());
        assert_eq!(first_wave, vec!["split".to_string()], "扇出的第一波是拆分那一步");
        let mut status = HashMap::new();
        status.insert("split".to_string(), NodeStatus::Done);
        assert_eq!(
            ready_set(&fanout, &status, &Blackboard::new()).len(),
            3,
            "拆完之后三支要能并行（验收第 1 条）"
        );
    }
    /// 手工改的那条边**活在账本里**：它写的就是 `replan` 那种行，而恢复只认这一种。
    /// 这条同时钉住"行的形状只有一个构造"——`replan_row` 换个事件名或换个 `node`，这里就读不回来
    #[test]
    fn an_edited_edge_comes_back_from_the_ledger_after_a_crash() {
        let plan = Plan::new("p-edit", "g", vec![node("a", &[]), node("b", &[])]);
        let edited = plan.with_dependency("a", "b", true).expect("加一条合法的边");
        let rows = vec![
            planned_row(&plan, &crate::orchestra::judge::Merge::Concat).expect("checkpoint 行"),
            replan_row(&edited),
        ];
        let checkpoint = checkpoint_of(&rows).expect("checkpoint 该读得回来");
        assert_eq!(
            plan_from_ledger(checkpoint.plan.clone(), &rows),
            edited,
            "改完的那条边没跟着恢复回来：账本与图又是两件事"
        );
        assert_eq!(
            plan_from_ledger(checkpoint.plan, &rows[..1]),
            plan,
            "正对照：账本里没有那一行时，图就该是改之前的样子"
        );
    }

    /// 只有还没开始的节点能被改依赖（§5.13 的第二个不许）。落定的那几个不用逐一枚举：
    /// 判据是"不是 Pending 就不许"，所以任何非待跑的状态都同样被拒
    #[test]
    fn only_a_node_that_has_not_started_may_have_its_dependencies_edited() {
        edge_gate(None).expect("图上有、账本里还没有它：那是刚恢复回来的图，该能改");
        edge_gate(Some(NodeStatus::Pending)).expect("待跑的节点该能改");
        for held in [
            NodeStatus::Running,
            NodeStatus::Done,
            NodeStatus::Failed,
            NodeStatus::Iterated,
            NodeStatus::Blocked,
            NodeStatus::WaitingApproval,
            NodeStatus::Canceled,
            NodeStatus::Skipped,
        ] {
            assert!(
                edge_gate(Some(held)).is_err(),
                "{held:?} 的节点不该被改依赖：那是在改写它当时为什么能派发"
            );
        }
    }

    /// §7.3 判定面的测试夹具：一张全是默认 worker 档案的小图
    fn followup_plan(nodes: Vec<Node>) -> Plan {
        Plan::new("p", "补做长出来的图", nodes)
    }

    /// 决策层答花名册内的角色就兑现成档案。每个补做节点各问一次，答哪个认哪个——
    /// 答 worker 也是一句正经答案，不是"没答上"
    #[test]
    fn a_roster_answer_is_applied_to_each_followup_node() {
        let mut plan = followup_plan(vec![node("a", &[]), node("b", &[]), node("c", &[])]);
        let added = ["a".to_string(), "b".to_string(), "c".to_string()];
        let answers = [
            Some(serde_json::json!({ "agent": "reader" })),
            Some(serde_json::json!({ "agent": "verifier" })),
            Some(serde_json::json!({ "agent": "worker" })),
        ];
        let mut given = answers.into_iter();
        assign_followup_profiles_with(&mut |_, _, _| given.next().flatten(), &followup_roster(&[]), &mut plan, &added);
        assert_eq!(plan.nodes[0].profile, "reader");
        assert_eq!(plan.nodes[1].profile, "verifier");
        assert_eq!(plan.nodes[2].profile, "worker", "答 worker 就是保持默认，照认");
    }

    /// 问出去的形状由这一侧负责：目标截到 400 字、type 是 followup、
    /// 花名册一字不差地带着 [`FOLLOWUP_ROLES`] 的描述——那是决策层判案的依据正文
    #[test]
    fn the_ask_carries_the_truncated_goal_the_followup_type_and_the_roster() {
        let mut plan = followup_plan(vec![node("a", &[])]);
        plan.nodes[0].goal = "很长的目标描述。".repeat(80);
        let mut seen: Vec<serde_json::Value> = Vec::new();
        assign_followup_profiles_with(
            &mut |_method, payload, timeout| {
                seen.push(payload);
                assert_eq!(timeout, 3000, "补做派工的等待上限");
                Some(serde_json::json!({ "agent": "reader" }))
            },
            &followup_roster(&[]),
            &mut plan,
            &["a".to_string()],
        );
        assert_eq!(seen.len(), 1);
        let payload = &seen[0];
        assert_eq!(payload["task"]["type"], "followup");
        let goal = payload["task"]["goal"].as_str().expect("目标是字符串");
        assert_eq!(goal.chars().count(), 400, "任务目标截到 400 字");
        let agents = payload["agents"].as_array().expect("花名册是个数组");
        let roles: Vec<&str> = agents.iter().map(|agent| agent["role"].as_str().expect("角色名")).collect();
        assert_eq!(roles, ["reader", "worker", "verifier"]);
        for (agent, (_, description)) in agents.iter().zip(followup_roster(&[])) {
            assert_eq!(agent["description"], description, "描述走样了，决策层判的就是另一回事");
        }
        assert_eq!(plan.nodes[0].profile, "reader");
    }

    /// 花名册之外的"答案"不算答案：协调位（supervisor）混进来是把执行活派给不干活的人，
    /// 不是字符串与缺字段是畸形答案——三种都保持 worker，绝不指错人
    #[test]
    fn an_answer_outside_the_roster_leaves_the_default_in_place() {
        let mut plan = followup_plan(vec![node("a", &[]), node("b", &[]), node("c", &[])]);
        let added = ["a".to_string(), "b".to_string(), "c".to_string()];
        let answers = [
            Some(serde_json::json!({ "agent": "supervisor" })),
            Some(serde_json::json!({ "agent": 42 })),
            Some(serde_json::json!({})),
        ];
        let mut given = answers.into_iter();
        assign_followup_profiles_with(&mut |_, _, _| given.next().flatten(), &followup_roster(&[]), &mut plan, &added);
        for node in &plan.nodes {
            assert_eq!(node.profile, "worker", "{} 的答案不该被兑现", node.id);
        }
    }

    /// 没答上（开关关、桥没人听、超时）就保持默认动手执行：fail-open 的意思是
    /// "派不出人就照旧"，不是"派不出人就不派了"。账上没有的 id 也不许占一问
    #[test]
    fn no_answer_keeps_the_default_assignment() {
        let mut plan = followup_plan(vec![node("a", &[]), node("b", &[])]);
        let mut asked = 0;
        assign_followup_profiles_with(
            &mut |_, _, _| {
                asked += 1;
                None
            },
            &followup_roster(&[]),
            &mut plan,
            &["a".to_string(), "ghost".to_string(), "b".to_string()],
        );
        assert_eq!(asked, 2, "每个图上找得到的补做节点各问一次，找不到的不问");
        assert!(plan.nodes.iter().all(|node| node.profile == "worker"));
    }

    /// 决策层只分默认派工：显式指定了档案的节点连问都不问——问完不用，
    /// 等于把"这格不是你管的"说成"你的意见我留着不用"
    #[test]
    fn an_explicit_profile_is_not_the_decision_layers_to_take() {
        let mut plan = followup_plan(vec![node("a", &[])]);
        plan.nodes[0].profile = "supervisor".into();
        let mut asked = 0;
        assign_followup_profiles_with(
            &mut |_, _, _| {
                asked += 1;
                Some(serde_json::json!({ "agent": "reader" }))
            },
            &followup_roster(&[]),
            &mut plan,
            &["a".to_string()],
        );
        assert_eq!(asked, 0, "非默认档案的节点不在决策层的管辖范围");
        assert_eq!(plan.nodes[0].profile, "supervisor");
    }

    fn subagent_def(name: &str, description: &str, assignable: bool) -> SubagentDef {
        SubagentDef {
            name: name.into(),
            description: description.into(),
            system_prompt: "你是这一支的执行者。".into(),
            tools: vec!["read_file".into()],
            endpoint_profile_id: String::new(),
            model: String::new(),
            orchestration_assignable: assignable,
            chat_spawnable: false,
        }
    }

    /// 花名册的装配规则：内置三角色永远在，用户只添「编排可派」的那一批。
    /// 这张名单是决策层看到的全部人选——多一个就是决策层能派一个没备案的人，
    /// 少一个就是用户备了案却永远派不出去
    #[test]
    fn the_roster_adds_only_the_assignable_customs_and_keeps_builtins_winning() {
        let custom = vec![
            subagent_def("审查员", "对照要求复核结论", true),
            subagent_def("侦察兵", "只读收集事实", true),
            subagent_def("写手", "负责起草", false),
            subagent_def("reader", "冒充内置的假货", true),
            subagent_def("   ", "名字是空白的", true),
        ];
        let roster = followup_roster(&custom);
        let roles: Vec<&str> = roster.iter().map(|(role, _)| role.as_str()).collect();
        assert_eq!(roles[..3], ["reader", "worker", "verifier"], "内置三角色永远在最前");
        assert!(roles.contains(&"审查员") && roles.contains(&"侦察兵"), "可派的自定义进名单：{roles:?}");
        assert!(!roles.contains(&"写手"), "没标「编排可派」的不进名单");
        assert!(!roles.contains(&"reader_xxx") && roster.iter().filter(|(role, _)| role == "reader").count() == 1,
            "与内置撞名的定义被无视，内置角色赢：{roles:?}");
        assert_eq!(roster.len(), 5, "空白名字的也不收：3 内置 + 2 有效自定义");
        assert_eq!(
            roster.iter().find(|(role, _)| role == "审查员").unwrap().1,
            "对照要求复核结论",
            "描述原文递给决策层，那是它判案的依据"
        );
    }

    /// 自定义名 → 档案的兑现：内置角色先认（撞名的定义被无视），自定义按定义照搬，
    /// 都不认得才落回 worker+run_command。派发兜底与档案表共用这一份，两处不同源
    /// 就是同一张图跑出两副能力面
    #[test]
    fn a_custom_name_resolves_through_its_definition_and_builtins_win_collisions() {
        let custom = [subagent_def("审查员", "对照要求复核结论", true),
            subagent_def("reader", "冒充内置的假货", true)];
        let custom = vec![
            SubagentDef { system_prompt: "对照要求检查结论。".into(), model: "deepseek-chat".into(), ..custom[0].clone() },
            custom[1].clone(),
        ];
        let auditor = profile_for_name("审查员", &custom);
        assert_eq!(auditor.name, "审查员");
        assert_eq!(auditor.role, "对照要求检查结论。");
        assert_eq!(auditor.model.as_deref(), Some("deepseek-chat"), "定义里的模型照搬");
        assert_eq!(auditor.endpoint, None, "空串是继承默认，不是名字叫空串的服务商");
        assert_eq!(auditor.tools, vec!["read_file".to_string()]);

        let builtin = profile_for_name("reader", &custom);
        assert_eq!(builtin.role, AgentProfile::reader("reader").role, "内置角色赢：撞名的定义被无视");

        let stranger = profile_for_name("没有这号人", &custom);
        assert_eq!(stranger.role, AgentProfile::worker("没有这号人").role, "不认得的名字落回老兜底");
    }

    /// 决策层答了自定义名，节点档案就换成它——兑换发生在派发兜底，分配处只认花名册
    #[test]
    fn a_custom_roster_answer_is_applied_like_any_other() {
        let mut plan = followup_plan(vec![node("a", &[])]);
        let custom = vec![subagent_def("审查员", "对照要求复核结论", true)];
        let roster = followup_roster(&custom);
        assign_followup_profiles_with(
            &mut |_, payload, _| {
                let roles: Vec<&str> = payload["agents"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|agent| agent["role"].as_str().unwrap())
                    .collect();
                assert!(roles.contains(&"审查员"), "自定义进花名册：{roles:?}");
                Some(serde_json::json!({ "agent": "审查员" }))
            },
            &roster,
            &mut plan,
            &["a".to_string()],
        );
        assert_eq!(plan.nodes[0].profile, "审查员");
        let profile = profile_for_name("审查员", &custom);
        assert_eq!(profile.tools, vec!["read_file".to_string()], "兑现出的档案带着定义里的白名单");
    }
}
