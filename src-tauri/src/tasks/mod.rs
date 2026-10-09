//! 定时任务引擎。三份数据，三种角色，永远不互换：
//! - 定义（想要什么）在 `config.json` 的 `tasks` 里，可以由界面整份重写；
//! - 事实（发生了什么）在 `runs.jsonl` 里，只追加（`runs`）；
//! - 没人可问时该处理什么在待审批队列里（`escalate`）。
//!
//! `task-state.json` 是账本算出来的缓存：它随时可以删掉重建，所以谁也不许把它当真相读。

pub mod escalate;
pub mod graph;

mod hook;
pub(crate) mod inbound;
pub(crate) mod runs;
mod subagent;
pub(crate) mod trigger;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::json;
use tauri::{AppHandle, Emitter, Manager};

use crate::audit::{self, Actor, Outcome};
use crate::chat;
use crate::config::{self, ScheduledTask};
use crate::history;
use crate::quota::{Denied, Priority, Quota, Slot};
use escalate::PendingApproval;
use graph::{Node, OnFailure};
use runs::{Line, RunStatus, RunView, StartedBy};

const TICK: Duration = Duration::from_secs(5);

/// 一轮补几格、剩下的那一截怎么算。分成两件事是全部的要点：
/// **超出预算**才是这批被砍掉（由 `runs::void_slots` 那一行替它销账），
/// 而占不到并发位只是"这一轮没轮到"——欠着就还欠着，不许顺手销掉
fn catch_up_plan(due: usize, budget: usize) -> (usize, usize) {
    (due.min(budget), due.saturating_sub(budget))
}

/// 此刻谁在跑，以及谁被挡住过。它是**进程内**的一格，不是账：崩了就清零，不去恢复——
/// 一旦持久化"谁在跑"，同一发就同时有两个答案说它跑没跑（那件事账本已经管着了）
#[derive(Default)]
pub struct Lanes {
    running: Mutex<BTreeSet<String>>,
    /// 上一轮被全局位挡住的原因。它活着的时间只到那一发真的跑起来或占到位为止
    deferred: Mutex<BTreeMap<String, String>>,
}

pub struct Lane {
    lanes: Arc<Lanes>,
    id: String,
}

/// 起一发之前要占的两样，一起拿一起还：这个任务自己的那一格（同一任务不叠开发）
/// 与全局池里的一手（与编排器共用同一个池子）
pub struct Permit {
    _lane: Lane,
    _slot: Slot,
}

impl Lanes {
    /// lib.rs 建的那一份；调度循环与手动那一下拿到的是同一个
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn claim(self: &Arc<Self>, task_id: &str) -> Option<Lane> {
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        if !running.insert(task_id.to_string()) {
            return None;
        }
        drop(running);
        Some(Lane { lanes: self.clone(), id: task_id.to_string() })
    }

    fn note(&self, task_id: &str, reason: String) {
        self.deferred.lock().unwrap_or_else(PoisonError::into_inner)
            .insert(task_id.to_string(), reason);
    }

    /// 界面读的那一份：只报还在欠着的，跑起来之后就清空
    fn deferred(&self) -> BTreeMap<String, String> {
        self.deferred.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

impl Drop for Lane {
    fn drop(&mut self) {
        self.lanes.running.lock().unwrap_or_else(PoisonError::into_inner).remove(&self.id);
        self.lanes.deferred.lock().unwrap_or_else(PoisonError::into_inner).remove(&self.id);
    }
}

/// 任务位已经被占的那一句。它是 `claim` 与 `inbound` 之间唯一的握手：外部敲进来时要分清
/// "这一条正在跑"和"别的错"，而靠散文里找子串会在改文案那天静默失效——
/// 那天到了，webhook 会把"正在跑"报成 404，谁也不会发现
pub(crate) const ALREADY_RUNNING: &str =
    "这个任务还有一发在跑。同一任务不叠开发：两发并发是两份完整上下文与两遍副作用。";

/// 占不到时的原因。手动那一下要给用户看得出口是哪一个闸拦的；调度器那一轮只需要
/// 把同一句话记在任务行上，等下一轮（5 秒后再问），**什么都不落进账本**——
/// 那一格仍然是欠着的，这才是补账该有的样子
fn claim(app: &AppHandle, task_id: &str) -> Result<Permit, String> {
    let lanes = (*app.state::<Arc<Lanes>>()).clone();
    let quota = (*app.state::<Arc<Quota>>()).clone();
    // 上限跟着配置走。这里也套一次是有原因的：编排器那一路只在 start 时套，
    // 只跑定时任务的机器上那个数就会一直停在默认值
    quota.set_total(config::load(app).total_parallel);
    // 任务位在前：一个已经有一发在跑的任务不该顺手占掉一格全局位再立刻还回去
    let Some(lane) = lanes.claim(task_id) else {
        return Err(ALREADY_RUNNING.to_string());
    };
    let priority = Priority::Background;
    match quota.try_acquire(priority) {
        Ok(slot) => Ok(Permit { _lane: lane, _slot: slot }),
        Err(denied) => {
            let reason = match denied {
                Denied::Full => format!(
                    "全局并发位 {}/{} 已被占满（编排器与别的任务在跑），这一发起不了，等下一轮",
                    quota.held_total(),
                    quota.total()
                ),
                Denied::AtShare => format!(
                    "后台这一档最多占 {} 格，此刻已经占满，等下一轮",
                    quota.share(priority)
                ),
            };
            lanes.note(task_id, reason.clone());
            // `lane` 在这里被丢掉：任务位当场还回去
            Err(reason)
        }
    }
}

/// 界面看到的任务：定义加上"上次跑到什么样"。字段沿用退役前那一份的形状，
/// 因为前端与 `TaskView` 的键名是一一对上的
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskView {
    pub id: String,
    pub name: String,
    pub prompt: String,
    pub kind: String,
    pub every_minutes: u32,
    pub at_minute: u32,
    pub at_weekday: u32,
    /// cron 型的表达式。定义的一部分：投影缺了它，"停用/启用"那一按会把它抹成空串
    pub cron_expr: String,
    pub enabled: bool,
    pub created_at: i64,
    /// 下面四格属于**定义**，不是运行状态：界面上"停用/启用"那一下写回的就是这一份投影，
    /// 投影缺哪一格，那一按就把 config 里的哪一格静默抹掉（图被抹掉 = 多步任务变回一发）。
    /// 键集合由 `the_projection_carries_every_field_of_the_definition` 钉住
    pub graph: crate::tasks::graph::TaskGraph,
    /// 往外通知的地址（不是凭据，密钥本来就不在这里）
    pub webhook_url: String,
    /// 反方向的令牌。它是凭据，所以只回到 owns 它的这一份配置里：不进审计、不进日志
    pub webhook_token: String,
    /// 停机期间错过的怎么算：skip / catch_up_once / run_latest。
    /// 空串 = `kind` 本身不是一种触发器（频率那一格认不出）。**停用不在此列**：
    /// `trigger::of` 不看 `enabled`，停用的任务照样报它自己那条策略——界面上要是不分清，
    /// 就会给一张认不出的图读出"错过只补最近一次"这句它没答应过的话
    pub missed_policy: String,
    /// 0 表示停用或配置非法，没有下一次
    pub next_run_at: i64,
    pub last_run_at: i64,
    pub last_status: String,
    pub last_error: String,
    pub last_conversation_id: String,
    /// 这一发被并发闸挡过、此刻还没跑起来的原因。空 = 没被挡住。
    /// 顶住派发必须看得见：静默等下一轮就是"任务页上显示着下次触发时间而其实一步没动"
    pub deferred: String,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis() as i64)
        .unwrap_or(0)
}

fn safe_id(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// 自动运行没有人接着说话，所以这一发的问法要把"给完整结果"说在前面。
/// 这句话进了话题日志，谁看见都能知道它不是人打的字
fn brief(task: &ScheduledTask) -> String {
    format!(
        "这是定时任务「{}」的一次自动运行。直接给出完整结果，不要反问接下来做什么。\n\n{}",
        task.name, task.prompt
    )
}

/// 建一条正常话题：标题与工作目录归它，内容一个字节都不写——内容归 `chat.rs` 那条路径。
/// `label` 是话题标题里那半句：整发跑的是任务名，一格跑的还带上"第几格"
fn open_session(app: &AppHandle, conversation_id: &str, label: &str) -> Result<(), String> {
    let config = config::load(app);
    let now = now_ms();
    history::save_and_index(
        app,
        history::Conversation {
            id: conversation_id.to_string(),
            project_id: config.active_project_id,
            title: format!("定时任务 · {label}"),
            created_at: now,
            updated_at: now,
            pinned: false,
            kind: "chat".to_string(),
            messages: Vec::new(),
            usage: None,
            video_nodes: Vec::new(),
            video_edges: Vec::new(),
        },
    )
    .map(|_| ())
}

/// 跑完一次要留的两件事：审计一行、通知界面一次。账本由 `runs` 在那之前写好
fn remember(app: &AppHandle, task: &ScheduledTask, run: &Line) {
    // 定时任务是一条不需要人在场就能改动世界的路径，事后要能回答"这一次是谁起的"，
    // 所以主体由调用方给进来，不由这里猜
    if let Ok(root) = runs::data_root(app) {
        let _ = audit::record(
            &root,
            run.started_by.actor(),
            "task:run",
            &format!("{} ({})", safe_id(&task.id), task.name),
            match run.status {
                RunStatus::Succeeded => Outcome::Ok,
                RunStatus::Failed => Outcome::Failed,
                RunStatus::WaitingApproval => Outcome::Blocked,
                // 作废的那一批不是一次运行（`void_slots` 不叫 `remember`，它自己落一行
                // `task:catchup:voided`）。真走到这里只说明账本被写坏过：说"被策略挡住"，
                // 不替它编一个"跑成了"
                RunStatus::Skipped => Outcome::Blocked,
                // 收尾之后不会还挂着 Running；走到这里说明账本被写坏过。
                // 报失败而不是替它编一个结论
                RunStatus::Running => Outcome::Failed,
            },
        );
    }
    let _ = app.emit(
        "task-ran",
        json!({
            "id": task.id,
            "name": task.name,
            "status": run.status.cache_label(),
            "error": run.error.clone().unwrap_or_default(),
            "runId": run.run_id,
            "conversationId": run.conversation_id,
        }),
    );
    // 应用内的 toast 对收进托盘的窗口是不可见的：系统通知把"跑完了/失败了/停在审批"喊出来
    crate::toast::task_finished(app, &task.name, run.status.cache_label(), run.error.clone().unwrap_or_default().as_str());
    notify(app, task, run);
}

/// 签名密钥所在的凭据服务名。它和 API 密钥分开存：换 API 密钥不该顺手把通知签名换掉
const WEBHOOK_SERVICE: &str = "aglab-webhook";

/// 往任务自己填的地址投一发通知。三件事它一定不做：
/// 不改这一发的结论、拿不到签名密钥时不发未签名的、不重试对端已经答过非 2xx 的请求。
/// 结果（含"为什么没发"）追加成账本里同一发的后一行，界面读得到
fn notify(app: &AppHandle, task: &ScheduledTask, run: &Line) {
    if !hook::wanted(&task.webhook_url) {
        return;
    }
    let Ok(root) = runs::data_root(app) else { return };
    let app_config = config::load(app);
    // 往外发东西之前先过权限表那一行 `net.configured`。默认它是放行的（这地址是用户在任务里
    // 点过名的），但从此它是一行**可以被收紧**的表项，而不是一条只有写代码的人知道的路径
    let clearance = {
        let policy = app_config.active_policy();
        let mut queue = match escalate::Queue::load(&root) {
            Ok(queue) => queue,
            Err(error) => {
                // 读不动队列就不发：不知道上一发是谁表过台，比"默认没批"更危险
                eprintln!("通知要问权限表，但待审批队列读不动，这条没发出去：{error}");
                return;
            }
        };
        let waiting_before = queue.waiting().len();
        let clearance = hook::preflight(
            &policy,
            &app_config.net_egress_allow,
            &mut queue,
            &run.run_id,
            &task.id,
            &run.conversation_id,
            &task.webhook_url,
            crate::session::now_millis(),
        );
        if queue.waiting().len() > waiting_before {
            // 挂上去的那条得落盘：不落盘就没人能批，"批了下次才发"这条规则当场不成立
            queue.save(&root).ok();
        }
        clearance
    };
    let held_by_table = matches!(clearance, hook::Clearance::Held { .. });
    let shot = match clearance {
        hook::Clearance::Held { reason } => hook::Delivery { note: reason, ..Default::default() },
        hook::Clearance::Go => {
            match config::api_key_for(WEBHOOK_SERVICE, &app_config.credential_user) {
                Err(error) => hook::Delivery {
                    note: format!("没有 webhook 签名密钥，这条没发出去：{error}"),
                    ..Default::default()
                },
                Ok(secret) => {
                    let body = hook::body(
                        &run.run_id,
                        &run.conversation_id,
                        &task.id,
                        &task.name,
                        run.status.cache_label(),
                        run.error.as_deref().unwrap_or_default(),
                    );
                    let proof = hook::signature(&secret, &body);
                    // 代理绑定在投递前解析：点名的代理不在场就记一句能看懂的原因，
                    // 不静默直连——这条地址是用户点过名的，路径也得照它说的走
                    match crate::proxy::take_global(&app_config, &task.webhook_url) {
                        Err(error) => hook::Delivery {
                            note: format!("代理没解析出来，这条没发出去：{error}"),
                            ..Default::default()
                        },
                        Ok(leg) => {
                            // 投递的成败**不进**代理账：deliver 自己会重试，而对端是用户配的
                            // 任意 webhook——拿不到头很可能是那台站死了，不是这条代理发不出去。
                            // 占用则活着等到投完，由 Leg 的 Drop 放回零
                            let proxy_url = leg.proxy_url().map(str::to_string);
                            hook::deliver(
                                &task.webhook_url,
                                &body,
                                &proof,
                                &run.run_id,
                                proxy_url.as_deref(),
                            )
                        }
                    }
                }
            }
        }
    };
    // 审计只记"投没投"与是哪一发：URL 与正文里都可能带着用户关心的东西。
    // 被表拦下的那一发记成 Denied（不是 Failed：它没失败，是这一步没被允许）
    let _ = audit::record_detail(
        &root,
        Actor::Scheduler,
        "task:webhook",
        &run.run_id,
        if shot.sent && shot.note.is_empty() {
            Outcome::Ok
        } else if held_by_table {
            Outcome::Denied
        } else {
            Outcome::Failed
        },
        // detail 只带去处（host）：整条地址的路径与 query 里常挂着 token
        held_by_table.then(|| crate::egress::host_of(&task.webhook_url)),
    );
    if let Err(ledger) = runs::note_delivery(&root, run, shot) {
        eprintln!("通知投完了却没记上投递结果：{ledger}");
    }
}

/// 一次运行的结论。回合自己报错优先于挂起；挂起不是成功
fn verdict(outcome: Result<(), String>, parked: Option<&PendingApproval>) -> (RunStatus, Option<String>) {
    match (outcome, parked) {
        (Err(message), _) => (RunStatus::Failed, Some(message)),
        // 停在待审批上要说不出的话，界面就只显示"没跑完"，谁也不知道在等谁
        (Ok(()), Some(item)) => (
            RunStatus::WaitingApproval,
            Some(format!(
                "停在待审批的「{}」上，等人处理：{}",
                item.capability, item.reason
            )),
        ),
        (Ok(()), None) => (RunStatus::Succeeded, None),
    }
}

/// 走到这里的一次运行（或一格），当前的结论与要说给人的一句话。
/// `conversation_id` 是它自己的那一发话题：交出去给子助理的格子，话题在子 run 那边，
/// 而成本归因和"去看它说了什么"都要这个 id，所以它得跟着结论一起传回去
struct Driving {
    status: RunStatus,
    error: Option<String>,
    conversation_id: String,
}

/// 一格要说的话。它和整发的问法一样把"给完整结果"说在前面——自动运行没有人接着说话，
/// 而这句话进了话题日志，谁看见都能知道它不是人打的字。
///
/// `material` 是它的**前置说过什么**。没有这一段，"多步任务"就只是几发互不相干的问答：
/// 第二格看不见第一格的产出，只能重新猜一遍（design-multi-agent.md §5.11 那条洞的定时任务版）
fn node_brief(task: &ScheduledTask, node: &Node, material: &str) -> String {
    format!(
        "这是定时任务「{}」的图里第「{}」格。直接给出完整结果，不要反问接下来做什么。\n\n{}{}",
        task.name, node.id, node.prompt, material
    )
}

/// 一格能拿到的前置上限。多步任务的正文会互相喂，不设上限就是每一格都比上一格更长
const HANDOFF_CHARS: usize = 4_000;

/// 把"前置的产出"拼成一段能进提示词的话。**取不到的那些必须明写成取不到**：
/// 安静地少一份材料，下游会以为自己看全了
fn handoff_of(node: &Node, answers: &BTreeMap<String, String>) -> String {
    if node.depends_on.is_empty() {
        return String::new();
    }
    let mut text = String::from("\n\n—— 你这几格的前置已经跑完，下面是它们的产出 ——");
    for held in &node.depends_on {
        text.push_str(&format!("\n\n【{held}】\n"));
        match answers.get(held) {
            None => text.push_str("（这一格没有留下可读的产出：它可能是上一次运行跑完的，而话题已经读不到了）"),
            Some(body) if body.trim().is_empty() => text.push_str("（那一发答完了却没留下正文）"),
            Some(body) => {
                let kept: String = body.chars().take(HANDOFF_CHARS).collect();
                text.push_str(&kept);
                if body.chars().count() > HANDOFF_CHARS {
                    text.push_str("\n…（产出太长，这里截断了；要完整的那份去看它自己那一发话题）");
                }
            }
        }
    }
    text
}

/// 这一格的前置各自的产出。两处事实都从盘上现读，不在内存里再记一份：
/// 哪一格跑成了住在账本的检查点行里，它说了什么住在那一发的话题日志里
fn upstream_answers(app: &AppHandle, run_id: &str, node: &Node) -> BTreeMap<String, String> {
    let Ok(root) = runs::data_root(app) else {
        return BTreeMap::new();
    };
    let conversations = runs::node_conversations(&root, run_id);
    let mut answers = BTreeMap::new();
    for held in &node.depends_on {
        let Some(conversation_id) = conversations.get(held) else { continue };
        let Ok(conversation) = crate::history::load_current(app, conversation_id) else {
            continue;
        };
        // 最后一条 assistant 正文就是那一格的产出。工具调用与思考过程不算：
        // 下游要的是"它得出了什么"，不是它中间敲了哪些命令
        if let Some(answer) = conversation
            .messages
            .iter().rfind(|row| row.role == "assistant")
        {
            answers.insert(held.clone(), answer.content.clone());
        }
    }
    answers
}

/// 失败或挂起时要说全的那句：卡在哪儿、还有哪些格因此没跑
fn stalled_message(task: &ScheduledTask, done: &BTreeSet<String>, failed: &BTreeSet<String>, why: Option<String>) -> String {
    let head = why.unwrap_or_else(|| "没跑成，也没给出原因".to_string());
    let stranded = task
        .graph
        .stranded(done, failed)
        .unwrap_or_default()
        .join("、");
    if stranded.is_empty() {
        head
    } else {
        format!("{head}；这些格的前置没跑成，因此没跑：{stranded}")
    }
}

/// 跑一格：自己的话题、自己的工具白名单、自己的一行检查点。
/// 走的是和整发同一个 `run_background_turn`——审批闸、段渲染、审计一样都不少。
///
/// 两种格子：自己开一发，或者把这一格交出去给一个更小的代理开（`node.subagent`）。
/// 交出去的那一发自己认自己的账（见 [`subagent::run`]），这里只把它的成绩记成本格的
/// 检查点——所以"这一格跑过没有"这件事仍然只从账本一处读得出
fn run_node(
    app: &AppHandle,
    task: &ScheduledTask,
    begun: &Line,
    node: &Node,
    started_by: StartedBy,
) -> Driving {
    let root = match runs::data_root(app) {
        Ok(root) => root,
        Err(error) => return Driving { status: RunStatus::Failed, error: Some(error), conversation_id: String::new() },
    };
    let started_at = now_ms();
    let one = match node.subagent.as_ref() {
        Some(spec) => subagent::run(app, task, begun, node, spec, started_by),
        None => run_node_turn(app, &root, task, begun, node, started_by),
    };
    let cost = runs::cost_for(app, &one.conversation_id);
    if let Err(ledger) = runs::checkpoint(
        &root,
        begun,
        &node.id,
        &one.conversation_id,
        started_at,
        one.status,
        one.error.clone(),
        cost,
    ) {
        eprintln!("这一格跑完了却没落下检查点，下一次续跑会把它当成没跑过：{ledger}");
    }
    one
}

/// 这一格自己开一发：建话题 → 盯住无人值守的审批 → 走回合 → 算这一发的结论
fn run_node_turn(
    app: &AppHandle,
    root: &std::path::Path,
    task: &ScheduledTask,
    begun: &Line,
    node: &Node,
    started_by: StartedBy,
) -> Driving {
    let conversation_id = format!("conv-{}", runs::token());
    if let Err(error) = open_session(app, &conversation_id, &format!("{} · 第 {} 格", task.name, node.id)) {
        return Driving { status: RunStatus::Failed, error: Some(error), conversation_id };
    }

    let _unattended = escalate::watch_run(&conversation_id, &begun.run_id, &task.id, started_by);
    let tools = (!node.allowed_tools.is_empty()).then_some(node.allowed_tools.as_slice());
    let outcome = chat::run_background_turn(
        app,
        &conversation_id,
        &node_brief(
            task,
            node,
            &handoff_of(node, &upstream_answers(app, &begun.run_id, node)),
        ),
        tools,
        None,
        // 任务那一发没有"这一格用哪个模型/服务商"可写，跟着设置里那个走
        None,
        None,
    );
    drop(_unattended);

    let parked = escalate::parked_in(root, &conversation_id);
    let (status, error) = verdict(outcome, parked.as_ref());
    Driving { status, error, conversation_id }
}

/// 按拓扑顺序一格一格跑（P1 串行；并发留给有配额与取消语义之后）。
///
/// "哪些格已经跑成"是从账本读的，不是内存里的进度：所以崩在半路、或者停在待审批上，
/// 再进这个函数就是续跑而不是重来。这一格没跑成，它的下游就永远进不了 `ready`——
/// `SkipBranch` 因此不需要任何专门代码，放弃那条分支就是"不再往前推它"
fn drive_graph(
    app: &AppHandle,
    task: &ScheduledTask,
    begun: &Line,
    started_by: StartedBy,
) -> Driving {
    let root = match runs::data_root(app) {
        Ok(root) => root,
        Err(error) => {
            return Driving { status: RunStatus::Failed, error: Some(error), conversation_id: String::new() }
        }
    };
    let mut done: BTreeSet<String> = runs::done_nodes(&root, &begun.run_id).into_iter().collect();
    let mut failed: BTreeSet<String> = BTreeSet::new();

    loop {
        let ready = match task.graph.ready(&done) {
            Ok(ready) => ready,
            Err(error) => {
                return Driving {
                    status: RunStatus::Failed,
                    error: Some(format!("任务图跑不了：{error}")),
                    conversation_id: String::new(),
                }
            }
        };
        let Some(node) = ready.into_iter().find(|node| !failed.contains(&node.id)) else {
            break;
        };
        let one = run_node(app, task, begun, node, started_by);
        if one.status != RunStatus::Succeeded {
            // 整发的结论没有"它自己那一发话题"：话题在每一格身上
            let conversation_id = one.conversation_id;
            failed.insert(node.id.clone());
            if one.status == RunStatus::Failed && task.graph.on_failure == OnFailure::SkipBranch {
                continue;
            }
            return Driving {
                status: one.status,
                error: Some(stalled_message(task, &done, &failed, one.error)),
                conversation_id,
            };
        }
        done.insert(node.id.clone());
    }

    let stranded = task.graph.stranded(&done, &failed).unwrap_or_default();
    if !stranded.is_empty() {
        return Driving {
            status: RunStatus::Failed,
            error: Some(format!(
                "{} 格的前置没跑成，这一发没跑完：{}",
                stranded.len(),
                stranded.join("、")
            )),
            conversation_id: String::new(),
        };
    }
    Driving { status: RunStatus::Succeeded, error: None, conversation_id: String::new() }
}

/// 收尾：把这一发的全貌（含各格花费之和）落到运行行上，再记审计与通知
fn close_out(app: &AppHandle, task: &ScheduledTask, begun: &Line, outcome: Driving) {
    let root = match runs::data_root(app) {
        Ok(root) => root,
        Err(error) => {
            eprintln!("定时任务连数据目录都没拿到，这一发没收尾：{error}");
            return;
        }
    };
    let cost = runs::total_cost(&root, &begun.run_id);
    match runs::finish(&root, begun, outcome.status, outcome.error, cost) {
        Ok(done) => remember(app, task, &done),
        Err(error) => eprintln!("这一次运行跑完了却没能收尾，账本上它还是未完成：{error}"),
    }
}

/// 跑一次。有图就一格一格跑，没图就是老形状：一句 prompt 一发。
/// 两条路共用同一个账本、同一条回合路径、同一套审批，区别只在"这一发有几格"
fn run_task(app: &AppHandle, task: &ScheduledTask, started_by: StartedBy) {
    if task.graph.is_empty() {
        run_plain(app, task, started_by);
        return;
    }
    let Ok(begun) = runs::begin(app, &task.id, "", started_by) else {
        eprintln!("定时任务的账本写不进去，这一发不起跑");
        return;
    };
    // 运行行上的 `conversation_id` 是空的：这一发的话题在每一格自己身上
    let outcome = drive_graph(app, task, &begun, started_by);
    close_out(app, task, &begun, outcome);
}

/// 一句 prompt 跑一发：建正常话题 → 走 `chat.rs` 的回合路径 → 结论进账本
fn run_plain(app: &AppHandle, task: &ScheduledTask, started_by: StartedBy) {
    let root = match runs::data_root(app) {
        Ok(root) => root,
        Err(error) => {
            eprintln!("定时任务连数据目录都没拿到，这一发没起跑：{error}");
            return;
        }
    };
    let conversation_id = format!("conv-{}", runs::token());
    if let Err(error) = open_session(app, &conversation_id, &task.name) {
        // 连话题都没建起来也是"跑过一次并失败在第一步"，账本上要有这一行
        match runs::begin(app, &task.id, "", started_by) {
            Ok(begun) => match runs::finish(&root, &begun, RunStatus::Failed, Some(error.clone()), None) {
                Ok(done) => remember(app, task, &done),
                Err(ledger) => eprintln!("这一发的失败没能收尾：{ledger}"),
            },
            Err(ledger) => eprintln!("这一发的失败没能记账（{ledger}）：{error}"),
        }
        return;
    }
    // 账本先落地：一次没跑完的运行也是跑过，它靠这一行留下"没跑完"
    let Ok(begun) = runs::begin(app, &task.id, &conversation_id, started_by) else {
        eprintln!("定时任务的账本写不进去，这一发不起跑");
        return;
    };

    // 回合里每一个"要点头"的动作都没有人可问：挂起成待审批，
    // 而不是让 ApprovalHub 那 600s 超时把"没人看"变成"被拒绝"
    let _unattended = escalate::watch_run(&conversation_id, &begun.run_id, &task.id, started_by);
    let outcome = chat::run_background_turn(app, &conversation_id, &brief(task), None, None, None, None);
    drop(_unattended);

    let cost = runs::cost_for(app, &conversation_id);
    let parked = escalate::parked_in(&root, &conversation_id);
    let (status, error) = verdict(outcome, parked.as_ref());

    match runs::finish(&root, &begun, status, error, cost) {
        Ok(done) => remember(app, task, &done),
        Err(error) => eprintln!("这一次运行跑完了却没能收尾，账本上它还是未完成：{error}"),
    }
}

/// 调度循环。只在应用运行时生效：开机自启是 P2
pub fn watch(app: AppHandle) {
    // 启动先对一次账：缓存可能停在旧版本，也可能被人手改过
    if let Ok(root) = runs::data_root(&app) {
        runs::refresh_cache(&root);
    }
    loop {
        thread::sleep(TICK);
        let now = now_ms();
        let config = config::load(&app);
        let Ok(root) = runs::data_root(&app) else {
            continue;
        };
        let starts = runs::last_starts(&root);

        for task in config.tasks.iter() {
            // 上次运行从账本现算，一次读盘算全部任务
            let last = starts.get(&task.id).copied().unwrap_or(0);
            // 错过几格就跑几次，跑到这一轮的预算为止：策略是任务自己声明的，但"一轮补几格"
            // 花的是这台机器的钱。超出预算的那一截**作废并记一行**，不静默少跑也不静默多花
            let due = trigger::due_slots(task, last, now);
            let (fire, voided) = catch_up_plan(due.len(), trigger::CATCH_UP_BUDGET);
            // 占不到位就从这一格起整轮停：剩下的欠账留给下一轮，不许顺手把它们作废
            let mut starved = false;
            for _ in 0..fire {
                let Ok(_permit) = claim(&app, &task.id) else {
                    starved = true;
                    break;
                };
                run_task(&app, task, StartedBy::Scheduler);
            }
            if !starved && voided > 0 {
                if let Err(error) =
                    runs::void_slots(&root, &task.id, voided, trigger::CATCH_UP_BUDGET, now)
                {
                    eprintln!("欠的账砍掉了，却没记上作废这一行：{error}");
                    continue;
                }
                let _ = audit::record_detail(
                    &root,
                    Actor::Scheduler,
                    "task:catchup:voided",
                    &task.id,
                    Outcome::Ok,
                    Some(format!("{} 格没补（每轮上限 {} 格）", voided, trigger::CATCH_UP_BUDGET)),
                );
            }
        }
    }
}

/// 定义 + 账本算出来的那一份 → 界面那一份。单独拆出来是为了让
/// `the_projection_carries_every_field_of_the_definition` 能拿真的构造代码去比键集合，
/// 而不是拿一份抄来的字段表
fn view_of(task: &ScheduledTask, record: &runs::LastRun, deferred: &str, now: i64) -> TaskView {
    TaskView {
        id: task.id.clone(),
        name: task.name.clone(),
        prompt: task.prompt.clone(),
        kind: task.kind.clone(),
        every_minutes: task.every_minutes,
        at_minute: task.at_minute,
        at_weekday: task.at_weekday,
        cron_expr: task.cron_expr.clone(),
        enabled: task.enabled,
        created_at: task.created_at,
        graph: task.graph.clone(),
        webhook_url: task.webhook_url.clone(),
        webhook_token: task.webhook_token.clone(),
        missed_policy: trigger::of(task)
            .map(|trigger| trigger.missed.as_str().to_string())
            .unwrap_or_default(),
        next_run_at: trigger::next_run(task, record.last_run_at, now).unwrap_or(0),
        last_run_at: record.last_run_at,
        last_status: record.last_status.clone(),
        last_error: record.last_error.clone(),
        last_conversation_id: record.last_conversation_id.clone(),
        deferred: deferred.to_string(),
    }
}

#[tauri::command]
pub fn tasks_list(app: AppHandle) -> Result<Vec<TaskView>, String> {
    let config = config::load(&app);
    let root = runs::data_root(&app)?;
    // 读之前先按账本重算一遍：缓存一旦与账本不一致，赢的永远是账本
    let state = runs::refresh_cache(&root);
    let deferred = app.state::<Arc<Lanes>>().deferred();
    let now = now_ms();

    Ok(config
        .tasks
        .iter()
        .map(|task| {
            let record = state.get(&task.id).cloned().unwrap_or_default();
            view_of(
                task,
                &record,
                deferred.get(&task.id).map(String::as_str).unwrap_or_default(),
                now,
            )
        })
        .collect())
}

/// 起"新的一发"的唯一入口：界面上的立刻运行、本机另一个进程敲进来的 webhook，
/// 都走这一条。差别只有一格 `started_by`，那格写进账本行。
/// 一次运行可能要等上半分钟，所以占到位就交给后台线程，跑完用事件通知界面
pub(crate) fn run_now(app: &AppHandle, id: &str, started_by: StartedBy) -> Result<(), String> {
    let task = config::load(app)
        .tasks
        .iter()
        .find(|item| item.id == id)
        .cloned()
        .ok_or_else(|| "任务不存在。".to_string())?;

    // 先占位再起跑：占不到就直接回一句原因，而不是起一个"其实没跑"的线程
    let permit = claim(app, id)?;
    let app = app.clone();
    thread::spawn(move || {
        let _permit = permit;
        run_task(&app, &task, started_by);
    });

    Ok(())
}

/// 立刻跑一次
#[tauri::command]
pub fn tasks_run(app: AppHandle, id: String) -> Result<(), String> {
    // 手动"立刻运行"与定时触发是两种主体：这一发是用户点的，不是调度器起的
    run_now(&app, &id, StartedBy::User)
}

/// 这个任务跑过几次、各跑成什么样。`task_id` 为空 = 所有任务混在一起从新到旧
#[tauri::command]
pub fn tasks_runs_list(
    app: AppHandle,
    task_id: Option<String>,
    limit: Option<i64>,
) -> Result<Vec<RunView>, String> {
    let root = runs::data_root(&app)?;
    let limit = limit.unwrap_or(50).max(1) as usize;
    Ok(runs::views(&root, task_id.as_deref(), limit))
}

/// 清掉旧的、已经有结论的运行记录（[`runs::purge`] 上那三条约束就是这条命令的边界）。
/// 天数由界面给，但界面上收不住的那两头在这里收：0 天等于"除锚点外全清"，那是手滑不是意图
#[tauri::command]
pub fn tasks_runs_purge(app: AppHandle, days: i64) -> Result<usize, String> {
    if !(1..=3650).contains(&days) {
        return Err("保留天数要在 1 到 3650 之间。0 天等于只留每个任务最新那一发，那不是清旧账。".to_string());
    }
    let root = runs::data_root(&app)?;
    let gone = runs::purge(&root, now_ms() - days * 86_400_000)?;
    let _ = audit::record_detail(
        &root,
        Actor::User,
        "task:runs:purged",
        &format!("{days} 天前的已了结记录"),
        Outcome::Ok,
        Some(format!(
            "抹掉 {gone} 发。没跑完的那些与各任务最新一发未动——后者是调度器算欠账的锚点"
        )),
    );
    Ok(gone)
}

/// 从检查点续跑：只跑账本没记成"跑成了"的那些格。
///
/// 判据不是内存里的进度而是账本，所以进程重启之后答案一样：已完成的一格不会再进来，
/// 停在待审批的那一格会重跑（人点头之后它的产出才在），失败的下游按图的策略决定
#[tauri::command]
pub fn tasks_run_resume(app: AppHandle, run_id: String) -> Result<(), String> {
    let root = runs::data_root(&app)?;
    let begun = runs::find(&root, &run_id).ok_or_else(|| "账本里没有这一次运行。".to_string())?;
    if begun.is_sub_run() {
        // 子助理那一发的账本上没有格子：它属于父亲那一发。让它续跑，
        // 读到的"已办格子"是空的，于是整张图会在子 run 的 id 下重跑一遍——那是双份的钱与双份的写
        return Err("子助理那一发不能单独续跑：请续它父亲那一发。".to_string());
    }
    if !begun.is_unfinished() {
        // 已经有结论的运行再续一次就是重复花钱、重复改文件。要重来请起一发新的
        return Err("这一发已经有结论了。要再跑一次请用「立刻运行」。".to_string());
    }
    if begun.task_id.is_empty() {
        // 读不动的那一行只能证明"有一次没回来"，它没有任务 id，接不回任何定义
        return Err("这一行账本读不出它属于哪个任务，续不了。".to_string());
    }
    let task = config::load(&app)
        .tasks
        .iter()
        .find(|item| item.id == begun.task_id)
        .cloned()
        .ok_or_else(|| "任务定义已经不在了，续跑没有东西可跑。".to_string())?;
    if task.graph.is_empty() {
        return Err("这条任务没有图：续跑只对一格一格的运行有意义。".to_string());
    }

    // 续跑也占位。这一发的账本行本来就在（没跑完），所以被挡住时不需要新写什么：
    // 它继续是那行"没跑完"，用户再点一次就是再问一次
    let permit = claim(&app, &task.id)?;
    thread::spawn(move || {
        let _permit = permit;
        let outcome = drive_graph(&app, &task, &begun, StartedBy::User);
        close_out(&app, &task, &begun, outcome);
    });
    Ok(())
}

/// 等人点头的动作。重启之后还在，因为它在盘上。已经表过态的不在这里：
/// 那一份是账，不是待办
#[tauri::command]
pub fn tasks_pending_approvals(app: AppHandle) -> Result<Vec<PendingApproval>, String> {
    let root = runs::data_root(&app)?;
    let queue = escalate::Queue::load(&root)?;
    Ok(queue.waiting().into_iter().cloned().collect())
}

/// 处理一条待审批。决定只由这里记账，执行永远在动作自己的那条路上
#[tauri::command]
pub fn tasks_approval_decide(app: AppHandle, id: String, approved: bool) -> Result<PendingApproval, String> {
    let root = runs::data_root(&app)?;
    escalate::decide(&root, &id, approved, Actor::User)
}

/// 以前表过态的那些。每一条都在替它盖过的那一发说话，直到被撤回为止——
/// 所以这一列必须看得见：看不见等于只有当初写它的人知道授权还站着
#[tauri::command]
pub fn tasks_approval_history(app: AppHandle) -> Result<Vec<PendingApproval>, String> {
    let root = runs::data_root(&app)?;
    escalate::history(&root)
}

/// 撤回一次表态：同一发下一次重新回到"问一次"。
/// 撤的是先例，不是已经执行过的那个动作（那由变更请求的回滚管），也不碰还在等的那条
#[tauri::command]
pub fn tasks_approval_forget(app: AppHandle, id: String) -> Result<usize, String> {
    let root = runs::data_root(&app)?;
    escalate::forget(&root, &id, Actor::User)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 预算砍掉的是这一批（有作废行为证）；占不到位时一分钱都不该被销掉
    #[test]
    fn the_catch_up_budget_fires_the_oldest_and_voids_the_rest() {
        assert_eq!(catch_up_plan(3, 8), (3, 0), "没超预算就一格不砍");
        assert_eq!(catch_up_plan(8, 8), (8, 0), "正好用完也不算超");
        assert_eq!(catch_up_plan(168, 8), (8, 160), "合盖一周的账要留下 160 格的作废记录");
        assert_eq!(catch_up_plan(0, 8), (0, 0), "没欠账就没有砍账");
    }

    /// 同一个任务不该叠开发：两发并发跑它是两份完整上下文与两遍副作用
    /// （"回滚这一次变更"跑两遍不是快一点，是事故）
    #[test]
    fn a_task_cannot_have_two_runs_in_flight_at_once() {
        let lanes = Lanes::shared();
        let held = lanes.claim("t1").expect("第一发占得住");
        assert!(lanes.claim("t1").is_none(), "还有一发在跑时，第二发不该起");
        assert!(lanes.claim("t2").is_some(), "别的任务不受影响");
        drop(held);
        assert!(lanes.claim("t1").is_some(), "上一发收尾之后立刻又能起");
    }

    /// 被挡住的原因要留在任务行上，而不是只在那一次调用里返回一下就被丢掉——
    /// 调度器那一轮没有人接电话，不说就等于没发生
    #[test]
    fn a_deferral_reason_stays_on_the_task_row_until_it_clears() {
        let lanes = Lanes::shared();
        lanes.note("t1", "全局并发位 6/6 已被占满".into());
        assert_eq!(
            lanes.deferred().get("t1").map(String::as_str),
            Some("全局并发位 6/6 已被占满"),
            "挡下的那一发要说得出被什么挡住"
        );
        let held = lanes.claim("t1").expect("占位");
        drop(held);
        assert!(lanes.deferred().is_empty(), "占到位的那一刻，那句\"被挡住\"就该消失");
    }

    #[test]
    fn task_ids_are_filed_down_before_becoming_a_filename() {
        let cleaned = safe_id("a/../b*?");
        assert!(!cleaned.contains('/'), "任务 id 会进审计的目标名，不能带路径分隔符");
        assert!(!cleaned.contains('.'));
        assert_eq!(safe_id("task-9f2c1"), "task-9f2c1");
    }

    /// 图里下游吃得到上游的话。取不到的那一份必须明写成取不到——
    /// 安静地少一份材料，下游会以为自己看全了
    #[test]
    fn a_graph_node_is_handed_what_its_dependencies_said() {
        let node = Node {
            id: "b".into(),
            prompt: "接着写".into(),
            // a 有话；z 那一发跑成了但没留下正文；q 根本不在账本里（上一次运行跑的）
            depends_on: vec!["a".into(), "z".into(), "q".into()],
            ..Default::default()
        };
        let answers = BTreeMap::from([
            ("a".to_string(), "上游的结论".to_string()),
            ("z".to_string(), String::new()),
        ]);
        let text = handoff_of(&node, &answers);
        assert!(text.contains("上游的结论"), "前置说过的话要在里面：{text}");
        assert!(text.contains("没留下正文"), "空的那一份要说得出是空的：{text}");
        assert!(
            text.contains("没有留下可读的产出"),
            "账本里没有的那一格要明说，不能安静地少一份：{text}"
        );
        let solo = Node { id: "solo".into(), ..Default::default() };
        assert!(handoff_of(&solo, &answers).is_empty(), "没有前置就不该多出这一段");
    }

    /// 长的产出要截断：多步任务的正文会互相喂，不截就是每一格都比上一格更长
    #[test]
    fn long_upstream_output_is_cut_with_a_marker() {
        let node = Node { id: "b".into(), depends_on: vec!["a".into()], ..Default::default() };
        let answers = BTreeMap::from([("a".to_string(), "字".repeat(HANDOFF_CHARS + 10))]);
        let text = handoff_of(&node, &answers);
        assert!(text.contains("截断"), "截了就得说一声：{text}");
        assert_eq!(text.matches('字').count(), HANDOFF_CHARS, "不该把整份都塞进下一发的提示词");
    }

    fn parked(target: &str) -> PendingApproval {
        PendingApproval {
            id: "apr-1".into(),
            run_id: "run-1".into(),
            task_id: "t1".into(),
            conversation_id: "conv-1".into(),
            capability: "file.write.projectRoot".into(),
            target: target.into(),
            fingerprint: "deadbeef".into(),
            reason: "要执行写文件，先确认这一份".into(),
            requested_at: 1_000,
            status: escalate::PendingStatus::Waiting,
            decided_at: None,
        }
    }

    /// 挂起的一发不能记成成功：那正是"跑在定时任务里的动作比手动话题少一套闸门"的另一种活法
    #[test]
    fn a_run_parked_on_an_unanswered_action_is_not_reported_as_finished() {
        let (status, error) = verdict(Ok(()), Some(&parked("C:/work/notes.md")));
        assert_eq!(status, RunStatus::WaitingApproval);
        let error = error.expect("停在待审批要带着停在哪儿");
        assert!(error.contains("file.write.projectRoot"), "要说得出是哪一行权限拦的：{error}");
        assert!(error.contains("确认"), "要把人要点的那句话带回来：{error}");

        let (status, error) = verdict(Ok(()), None);
        assert_eq!(status, RunStatus::Succeeded);
        assert_eq!(error, None, "跑完了又没人等，就不该有错误");

        let (status, error) = verdict(Err("服务商超时".into()), None);
        assert_eq!(status, RunStatus::Failed);
        assert_eq!(error.as_deref(), Some("服务商超时"), "回合自己的报错要原样进账本");
    }

    /// 界面上"停用/启用"那一下写回的是**投影**，不是定义。投影少一格，那一按就把 config 里
    /// 那一格静默抹成默认值（图被抹掉 = 一条多步任务变回一发，且没有任何地方说它被抹过）。
    /// TS 那句 `TaskView extends ScheduledTask` 声明的正是"投影带着定义"，所以两半都得逐字对齐；
    /// 以后往 `ScheduledTask` 加字段而忘了往 `TaskView` 补，先在这里红
    #[test]
    fn the_projection_carries_every_field_of_the_definition() {
        use crate::tasks::graph::TaskGraph;

        let task = ScheduledTask {
            at_weekday: 1,
            id: "t1".into(),
            name: "每日收尾".into(),
            prompt: "总结一下今天".into(),
            kind: "interval|skip".into(),
            every_minutes: 60,
            at_minute: 0,
            cron_expr: String::new(),
            enabled: true,
            created_at: 1,
            graph: TaskGraph {
                nodes: vec![Node {
                    id: "a".into(),
                    prompt: "第一步".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
            webhook_url: "https://example.invalid/hook".into(),
            webhook_token: "s3cr3t-token".into(),
        };
        let defined = serde_json::to_value(&task).expect("定义序列化");
        let viewed =
            serde_json::to_value(view_of(&task, &runs::LastRun::default(), "", 0)).expect("投影序列化");
        crate::test_support::assert_matches_ts(&defined, "ScheduledTask");
        crate::test_support::assert_matches_ts(&viewed, "TaskView");

        // 键对齐了、值填错更难发现：把最容易写串的那几格原样对一遍
        assert_eq!(viewed["graph"], defined["graph"], "图必须整张过去，不能半张");
        assert_eq!(
            viewed["webhookUrl"], defined["webhookUrl"],
            "往外通知的地址不能在这一趟里丢"
        );
        assert_eq!(
            viewed["webhookToken"], defined["webhookToken"],
            "令牌丢了就表现为\"那个端口忽然敲不开了\""
        );
    }
}
