//! 待审批队列：高风险动作在**没人可问**的时候停在这里。
//!
//! 与 `approvals::ApprovalHub` 的分工（设计 §7）：队列是持久意图，Hub 是进程内通道，
//! 重启后靠队列重建。Hub 那条 600s 超时在这里不适用——任务的挂起是长期的，
//! 人不来处理就永远停着，不"超时放行"也不"超时拒绝"。
//!
//! 判定不在这里：那发动作是 Allow / Ask / Deny 由 `policy::Policy::check` 说，
//! 本模块只回答另一半问题——"要点头，而现场没有能点头的人，怎么办"。
//! 答案是把这一发挂起，并且**不动手**。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use serde::{Deserialize, Serialize};

use crate::audit::{self, Actor, Outcome};
use crate::policy::Decision;
use crate::tasks::runs::{self, StartedBy};

/// 队列文件。它记的是"接下来该处理什么"，不是事实历史，所以整份重写是它的形状
pub const QUEUE_FILE: &str = "task-approvals.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PendingStatus {
    Waiting,
    Approved,
    Denied,
}

/// 一条挂起的动作。字段够界面把"在等什么"说清楚，也够下次判断"这条点过头没有"
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingApproval {
    pub id: String,
    pub run_id: String,
    pub task_id: String,
    pub conversation_id: String,
    /// `policy::Capability::key()`：拦下它的是权限表的哪一行
    pub capability: String,
    pub target: String,
    /// 确认的就是这一份。换参数就是另一发，不会顺手放行
    pub fingerprint: String,
    pub reason: String,
    pub requested_at: i64,
    pub status: PendingStatus,
    pub decided_at: Option<i64>,
}

/// 一条"要点头"的请求。理由与指纹都在 [`Decision`] 里，这里只带身份与目标：
/// 分成两处就会出现"请求说一个指纹、判定说另一个"的可能
#[derive(Debug, Clone)]
pub struct Request {
    pub run_id: String,
    pub task_id: String,
    pub conversation_id: String,
    pub capability: String,
    pub target: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Queue {
    pub items: Vec<PendingApproval>,
}

/// 这一发要不要动手。只有 `Execute` 那一条分支允许碰磁盘
#[derive(Debug, Clone)]
pub enum Gate {
    Execute,
    /// 已挂起：这一发没执行，等人来
    Parked(PendingApproval),
    /// 权限表拒绝，或这条已经被摇过头
    Refused {
        reason: String,
    },
}

fn queue_path(root: &Path) -> PathBuf {
    root.join(QUEUE_FILE)
}

impl Queue {
    /// 没有文件 = 空队列；有一份但读不动 = **报错**。后者不能当成"没有待审批"，
    /// 那等于一份写坏的 JSON 就把闸门解除了
    pub fn load(root: &Path) -> Result<Self, String> {
        let path = queue_path(root);
        let Ok(text) = fs::read_to_string(&path) else {
            return Ok(Self::default());
        };
        serde_json::from_str(&text)
            .map_err(|e| format!("待审批队列 {} 读不动：{e}", path.display()))
    }

    /// 先写临时文件再改名：挂起的审批是"谁点过头"的证据，不能被半次写入毁掉
    pub fn save(&self, root: &Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        fs::create_dir_all(root).map_err(|e| format!("建队列目录失败：{e}"))?;
        let temp = queue_path(root).with_extension("json.tmp");
        fs::write(&temp, &text).map_err(|e| format!("写队列临时文件失败：{e}"))?;
        fs::rename(&temp, queue_path(root)).map_err(|e| {
            let _ = fs::remove_file(&temp);
            format!("落队列文件失败：{e}")
        })
    }

    pub fn waiting(&self) -> Vec<&PendingApproval> {
        self.items
            .iter()
            .filter(|item| item.status == PendingStatus::Waiting)
            .collect()
    }

    pub fn waiting_for(&self, conversation_id: &str) -> Option<&PendingApproval> {
        self.items.iter().find(|item| {
            item.status == PendingStatus::Waiting && item.conversation_id == conversation_id
        })
    }

    /// 这条动作上次是怎么被表台的。同一发（capability + 指纹）只认最近一次决定
    fn verdict(&self, capability: &str, fingerprint: &str) -> Option<&PendingApproval> {
        self.items
            .iter()
            .rev()
            .find(|item| item.capability == capability && item.fingerprint == fingerprint)
    }

    /// 表台。只接受还在等的条目：投第二次票不改变结果，与 Hub 的"第一票算数"同一条规矩
    pub fn decide(
        &mut self,
        id: &str,
        approved: bool,
        now: i64,
    ) -> Result<PendingApproval, String> {
        let index = self
            .items
            .iter()
            .position(|item| item.id == id)
            .ok_or_else(|| "这条待审批不在了。".to_string())?;
        if self.items[index].status != PendingStatus::Waiting {
            return Err("这条已经表过态了，一次动作只点一次头。".to_string());
        }
        let item = &mut self.items[index];
        item.status = if approved {
            PendingStatus::Approved
        } else {
            PendingStatus::Denied
        };
        item.decided_at = Some(now);
        Ok(item.clone())
    }

    /// 已经表过态的那些，最新的在前。界面上"以前点过头的"那一块读的就是这个：
    /// 一次点头之后同一发不再问（见 [`settle`]），那是一条**standing 授权**，
    /// 而任何 standing 的东西都得有地方把它放下去
    pub fn decided(&self) -> Vec<&PendingApproval> {
        let mut rows: Vec<&PendingApproval> = self
            .items
            .iter()
            .filter(|item| item.status != PendingStatus::Waiting)
            .collect();
        rows.sort_by_key(|item| std::cmp::Reverse(item.decided_at.unwrap_or(item.requested_at)));
        rows
    }

    /// 撤回一次表态：撤的是"以后不用再问"这个先例，**不是已经执行过的那个动作**
    /// （那归回滚管，这里碰不到它）。撤完之后同一发回到"再问一次"。
    ///
    /// 划的是同一发（capability + 指纹）的所有先例而不是用户点的那一行：`verdict` 取最近一条，
    /// 只删最近的那一条会让更早的那一条立刻顶上，用户按了"撤回"而闸门还是老样子。
    /// 还在等的条目不动——那是待办不是先例，顺手清掉等于替用户取消一次排队。
    pub fn forget(&mut self, id: &str) -> Result<(PendingApproval, usize), String> {
        let standing = self
            .items
            .iter()
            .find(|item| item.id == id)
            .ok_or_else(|| "这条表态不在了。".to_string())?
            .clone();
        if standing.status == PendingStatus::Waiting {
            return Err("这一条还在等人表台。要不同意要拒绝，别从这里撤。".to_string());
        }
        let mut gone = 0usize;
        self.items.retain(|item| {
            let same_shot = item.status != PendingStatus::Waiting
                && item.capability == standing.capability
                && item.fingerprint == standing.fingerprint;
            if same_shot {
                gone += 1;
            }
            !same_shot
        });
        Ok((standing, gone))
    }
}

/// 判定 + 入队。只在没人可问的时候调用——"有没有人在"由无人值守登记表说
/// （见 [`park_for_turn`]），所以这里没有第二个判断口径
pub fn settle(queue: &mut Queue, request: &Request, decision: &Decision, now: i64) -> Gate {
    match decision {
        Decision::Allow => Gate::Execute,
        // Deny 不入库：一条权限表禁止的动作挂进待审批，等于给用户一个把它点通过的入口
        Decision::Deny { reason } => Gate::Refused {
            reason: reason.clone(),
        },
        Decision::Ask {
            reason,
            fingerprint,
        } => {
            match queue.verdict(&request.capability, fingerprint) {
                // 点过头的同类动作在下次运行时放行：P0 没有"当场续跑"，
                // 但点头必须有用，否则挂起就等于拒绝。这一条是**可以撤回的**：
                // [`Queue::forget`] 把先例划掉，同一发就回到"再问一次"
                Some(done) if done.status == PendingStatus::Approved => Gate::Execute,
                Some(declined) if declined.status == PendingStatus::Denied => Gate::Refused {
                    reason: format!("这条动作此前已被拒绝：{}", declined.reason),
                },
                // 同一发已经在等人了：再入队就变成两条互相抢票的待审批
                Some(standing) => Gate::Parked(standing.clone()),
                None => {
                    let parked = PendingApproval {
                        id: format!("apr-{}", runs::token()),
                        run_id: request.run_id.clone(),
                        task_id: request.task_id.clone(),
                        conversation_id: request.conversation_id.clone(),
                        capability: request.capability.clone(),
                        target: request.target.clone(),
                        fingerprint: fingerprint.clone(),
                        reason: reason.clone(),
                        requested_at: now,
                        status: PendingStatus::Waiting,
                        decided_at: None,
                    };
                    queue.items.push(parked.clone());
                    Gate::Parked(parked)
                }
            }
        }
    }
}

/// 挂起这一次动作。返回 `None` = 这条话题不是无人值守的后台 run，交给即时审批。
/// 队列读不动时报 Err：那种情况下**谁也不许动手**，而不是"当作没有待审批"
pub fn park_for_turn(
    root: &Path,
    conversation_id: &str,
    capability: &str,
    target: &str,
    decision: &Decision,
    now: i64,
) -> Result<Option<Gate>, String> {
    let Some(watched) = watched_of(conversation_id) else {
        return Ok(None);
    };
    let request = Request {
        run_id: watched.run_id,
        task_id: watched.task_id,
        conversation_id: conversation_id.to_string(),
        capability: capability.to_string(),
        target: target.to_string(),
    };
    let mut queue = Queue::load(root)?;
    let gate = settle(&mut queue, &request, decision, now);
    // 挂起与被拒都留痕：这是"闸门拦下"，不是"没发生过"
    let outcome = match &gate {
        Gate::Parked(_) => {
            queue.save(root)?;
            Outcome::Blocked
        }
        Gate::Refused { .. } => Outcome::Denied,
        _ => return Ok(Some(gate)),
    };
    // 只记标识，不记判定那句话：理由里带着目标路径，而出口过滤一旦判定它像凭据
    // 就把整条 detail 丢掉——那样"在等什么"就从审计里消失了。原文在队列文件里，
    // 界面读的是那份，不必让审计再抄一遍
    let _ = audit::record_detail(
        root,
        watched.actor.actor(),
        "task:escalate",
        &request.target,
        outcome,
        Some(format!("{} · {}", request.capability, request.run_id)),
    );
    Ok(Some(gate))
}

/// 这一次运行的话题处于无人值守状态：它的审批问题没有听众
struct Watched {
    run_id: String,
    task_id: String,
    actor: StartedBy,
}

fn watched() -> &'static Mutex<HashMap<String, Watched>> {
    static WATCHED: OnceLock<Mutex<HashMap<String, Watched>>> = OnceLock::new();
    WATCHED.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock() -> MutexGuard<'static, HashMap<String, Watched>> {
    watched().lock().unwrap_or_else(PoisonError::into_inner)
}

/// 后台 run 开回合前登记，返回的守卫负责 unregister——回合里任何一步 panic 或提前返回
/// 都不该把这条话题永久留在"没人可问"的名单上
#[must_use]
pub struct Unattended {
    conversation_id: String,
}

impl Drop for Unattended {
    fn drop(&mut self) {
        lock().remove(&self.conversation_id);
    }
}

pub fn watch_run(
    conversation_id: &str,
    run_id: &str,
    task_id: &str,
    actor: StartedBy,
) -> Unattended {
    lock().insert(
        conversation_id.to_string(),
        Watched {
            run_id: run_id.to_string(),
            task_id: task_id.to_string(),
            actor,
        },
    );
    Unattended {
        conversation_id: conversation_id.to_string(),
    }
}

fn watched_of(conversation_id: &str) -> Option<Watched> {
    lock().get(conversation_id).map(|watched| Watched {
        run_id: watched.run_id.clone(),
        task_id: watched.task_id.clone(),
        actor: watched.actor,
    })
}

pub fn is_unattended(conversation_id: &str) -> bool {
    lock().contains_key(conversation_id)
}

/// 这一发的工具调用该记在谁头上。
///
/// 没登记过的话题就是"用户在聊天里说话，模型替它动手"（`Model`）。登记过的按 `StartedBy` 走——
/// **编排器或定时任务引起的那一发不能记成"模型的回合"**：`audit_tool` 以前写死 `Actor::Model`，
/// 于是账上问不出"这条写文件有没有人看着"，而那正是无人值守这条路唯一要区分的事。
/// 归属与生命周期行（`orchestra:*` / `task:*`）用的是同一个 `StartedBy::actor()`，
/// 所以两类行不会各说各话
pub fn audit_actor(conversation_id: &str) -> crate::audit::Actor {
    watched_of(conversation_id)
        .map(|watched| watched.actor.actor())
        .unwrap_or(crate::audit::Actor::Model)
}

/// 这条话题是不是停在某个等人点头的动作上。收尾时用它决定账本那行的状态
pub fn parked_in(root: &Path, conversation_id: &str) -> Option<PendingApproval> {
    Queue::load(root)
        .ok()?
        .waiting_for(conversation_id)
        .cloned()
}

/// 人对一条待批表台。主体由命令侧给进来（这里不问"是谁"，只记账），
/// 决定写回盘上：这条动作下次再被无人值守地碰到时按这一份票走
pub fn decide(
    root: &Path,
    id: &str,
    approved: bool,
    actor: Actor,
) -> Result<PendingApproval, String> {
    let mut queue = Queue::load(root)?;
    let decided = queue.decide(id, approved, crate::session::now_millis())?;
    queue.save(root)?;
    let _ = audit::record_detail(
        root,
        actor,
        if approved {
            "task:approval:granted"
        } else {
            "task:approval:refused"
        },
        &decided.target,
        if approved {
            Outcome::Ok
        } else {
            Outcome::Denied
        },
        Some(format!("{} · {}", decided.capability, decided.run_id)),
    );
    Ok(decided)
}

/// 表过态的历史。给界面的"以前点过头的"那一块，一条命令读全部，不分页也不隐藏：
/// standing 授权看不全，就等于只有写它的人知道它存在
pub fn history(root: &Path) -> Result<Vec<PendingApproval>, String> {
    Ok(Queue::load(root)?.decided().into_iter().cloned().collect())
}

/// 撤回一次表态。审计里落的是"撤了哪一发"（capability + 目标 + 撤掉几条先例），
/// 不落正文——撤销这件事本身就是要能被核对的
pub fn forget(root: &Path, id: &str, actor: Actor) -> Result<usize, String> {
    let mut queue = Queue::load(root)?;
    let (standing, gone) = queue.forget(id)?;
    queue.save(root)?;
    let _ = audit::record_detail(
        root,
        actor,
        "task:approval:revoked",
        &standing.target,
        Outcome::Ok,
        Some(format!("{} · 撤掉 {} 条先例", standing.capability, gone)),
    );
    Ok(gone)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{Capability, FileMode, PathScope, Policy};
    use crate::test_support::{remove_tree, temp_dir};

    const EVERY: i64 = 1_700_000_000_000;

    /// 无人值守的那一发工具调用，审计里要记成"引起它的那件事"，不是"模型的回合"。
    /// 三种归属各一条，外加**守卫一掉就回到 Model**：编排每一发持有 `Unattended` 到结束，
    /// 结束之后同一 id 再被问到就是另一回事了（`watch_run` 是按话题登记的）
    #[test]
    fn an_unattended_run_is_audited_as_the_thing_that_ran_it() {
        let conversation = "conv-audit-actor";
        assert_eq!(
            audit_actor(conversation),
            crate::audit::Actor::Model,
            "没登记过的话题就是用户在聊天里让模型动的"
        );
        {
            let _scheduled = watch_run(conversation, "run-1", "task-1", StartedBy::Scheduler);
            assert_eq!(audit_actor(conversation), crate::audit::Actor::Scheduler);
        }
        {
            let _webhook = watch_run(conversation, "run-2", "task-1", StartedBy::Webhook);
            assert_eq!(
                audit_actor(conversation),
                crate::audit::Actor::Scheduler,
                "机器引起的，不是人点的——与生命周期行用的是同一个 actor() 映射"
            );
        }
        {
            let _manual = watch_run(conversation, "run-3", "task-1", StartedBy::User);
            assert_eq!(
                audit_actor(conversation),
                crate::audit::Actor::User,
                "用户自己点的立即运行，记成人点的"
            );
        }
        assert_eq!(
            audit_actor(conversation),
            crate::audit::Actor::Model,
            "守卫一掉就不该再有归属跟着这个 id"
        );
    }

    /// 真拿权限表判一遍，而不是手搓一个 Decision：这个测试要钉的是
    /// "ask 档下的项目内写入会走到挂起"这条链路，不是两个结构的拼装
    fn write_decision(target: &str) -> Decision {
        let policy = Policy::new(crate::policy::Mode::Ask);
        let cap = Capability::File {
            scope: PathScope::ProjectRoot,
            mode: FileMode::Write,
        };
        policy.check(
            &cap,
            target,
            &crate::policy::fingerprint(&[cap.key().as_str(), target]),
        )
    }

    fn request(target: &str) -> Request {
        Request {
            run_id: "run-1".into(),
            task_id: "t1".into(),
            conversation_id: "conv-1".into(),
            capability: "file.write.projectRoot".into(),
            target: target.to_string(),
        }
    }

    /// 放行动作的唯一机会在调用方手里，而它只在 `Gate::Execute` 时动手：
    /// 于是"文件有没有被改"成了闸门判定的物证
    fn act_on(gate: &Gate, path: &Path) {
        if matches!(gate, Gate::Execute) {
            fs::write(path, "改了").expect("放行的那发该写得动");
        }
    }

    /// 设计 §5 T04 的后半句：无人应答时动作**没有**执行，用文件是否被改证明
    #[test]
    fn a_parked_action_leaves_the_file_it_was_about_to_change_untouched() {
        let root = temp_dir("escalate-park");
        let target = root.join("notes.md");
        fs::write(&target, "原样").expect("放一个要被改的文件");

        let decision = write_decision(&target.to_string_lossy());
        assert!(
            matches!(decision, Decision::Ask { .. }),
            "ask 档下的项目内写入必须问人：{decision:?}"
        );
        let mut queue = Queue::load(&root).expect("空队列该读得出来");
        let gate = settle(
            &mut queue,
            &request(&target.to_string_lossy()),
            &decision,
            EVERY,
        );
        act_on(&gate, &target);
        queue.save(&root).expect("队列该落得下去");

        assert!(
            matches!(gate, Gate::Parked(_)),
            "没人可问时这一发必须挂起：{gate:?}"
        );
        assert_eq!(
            fs::read_to_string(&target).expect("文件该还在"),
            "原样",
            "状态字符串可以说谎，磁盘不会：挂起的那一发没碰过它"
        );
        remove_tree(&root);
    }

    /// 重启 = 换一份从磁盘读出来的结构，不与上一段共享任何内存
    #[test]
    fn a_parked_item_is_still_waiting_after_a_restart() {
        let root = temp_dir("escalate-restart");
        let target = root.join("notes.md");
        fs::write(&target, "原样").expect("放文件");
        let display = target.to_string_lossy().to_string();
        let decision = write_decision(&display);
        assert!(matches!(decision, Decision::Ask { .. }), "这条动作该问人");

        let mut queue = Queue::load(&root).expect("空队列");
        let gate = settle(&mut queue, &request(&display), &decision, EVERY);
        assert!(matches!(gate, Gate::Parked(_)), "第一次就该挂起：{gate:?}");
        queue.save(&root).expect("落盘");

        let mut revived = Queue::load(&root).expect("重启后该读得回来");
        assert_eq!(revived.waiting().len(), 1, "重启不该把等人点头的东西洗掉");
        assert_eq!(
            revived.waiting()[0].conversation_id,
            "conv-1",
            "得知道是哪个话题停在半路"
        );
        assert_eq!(
            revived.waiting()[0].run_id,
            "run-1",
            "以及是哪一次运行停在半路"
        );

        // 重启后同一条规则还得再判一次，而且仍然不动手
        let again = settle(&mut revived, &request(&display), &decision, EVERY + 60_000);
        act_on(&again, &target);
        assert!(
            matches!(again, Gate::Parked(_)),
            "重启后依然只挂起，不放行：{again:?}"
        );
        assert_eq!(
            revived.waiting().len(),
            1,
            "同一发不该被重复入队成两张待批票"
        );
        assert_eq!(fs::read_to_string(&target).expect("文件该还在"), "原样");
        remove_tree(&root);
    }

    #[test]
    fn a_nod_releases_that_action_and_no_other_fingerprint() {
        let root = temp_dir("escalate-decide");
        let target = root.join("notes.md");
        let other = root.join("other.md");
        fs::write(&target, "原样").expect("放文件");
        fs::write(&other, "原样").expect("放第二个文件");
        let display = target.to_string_lossy().to_string();
        let other_display = other.to_string_lossy().to_string();
        let decision = write_decision(&display);
        let other_decision = write_decision(&other_display);
        assert!(
            matches!(
                (&decision, &other_decision),
                (Decision::Ask { .. }, Decision::Ask { .. })
            ),
            "两个目标各问一次，谁也不该替谁点头"
        );

        let mut queue = Queue::load(&root).expect("空队列");
        let parked = settle(&mut queue, &request(&display), &decision, EVERY);
        let Gate::Parked(record) = parked else {
            panic!("该挂起：{parked:?}");
        };
        queue.save(&root).expect("落盘");

        let mut queue = Queue::load(&root).expect("读回");
        queue
            .decide(&record.id, true, EVERY + 1_000)
            .expect("点头该记下");
        queue.save(&root).expect("落盘");

        // 同一个指纹、下一次无人值守的运行：放行，并且真的动了手
        let mut queue = Queue::load(&root).expect("读回");
        let released = settle(&mut queue, &request(&display), &decision, EVERY + 60_000);
        act_on(&released, &target);
        assert!(
            matches!(released, Gate::Execute),
            "人点过头的那一发要真能跑起来：{released:?}"
        );
        assert_eq!(
            fs::read_to_string(&target).expect("文件该还在"),
            "改了",
            "点头的意义就是这一发可以动手了"
        );

        // 换个目标就是另一发，不能顺手放行
        let neighbour = settle(
            &mut queue,
            &request(&other_display),
            &other_decision,
            EVERY + 60_000,
        );
        act_on(&neighbour, &other);
        assert!(
            matches!(neighbour, Gate::Parked(_)),
            "一次点头只覆盖确认过的那一份：{neighbour:?}"
        );
        assert_eq!(
            fs::read_to_string(&other).expect("文件该还在"),
            "原样",
            "批准了 notes.md 不等于批准了 other.md"
        );
        remove_tree(&root);
    }

    #[test]
    fn a_refused_action_stays_refused_instead_of_queuing_up_again() {
        let root = temp_dir("escalate-denied");
        let display = "C:/work/notes.md".to_string();
        let decision = write_decision(&display);
        assert!(matches!(decision, Decision::Ask { .. }), "这条动作该问人");

        let mut queue = Queue::load(&root).expect("空队列");
        let parked = settle(&mut queue, &request(&display), &decision, EVERY);
        let Gate::Parked(record) = parked else {
            panic!("该挂起：{parked:?}");
        };
        queue
            .decide(&record.id, false, EVERY + 1_000)
            .expect("摇头该记下");

        let gate = settle(&mut queue, &request(&display), &decision, EVERY + 2_000);
        assert!(
            matches!(gate, Gate::Refused { .. }),
            "摇过头的动作不该再挂一次等人回心转意：{gate:?}"
        );
        assert_eq!(queue.waiting().len(), 0, "队列里不该有还活着的这一发");
        assert_eq!(queue.items.len(), 1, "拒绝本身是事实，要留在盘上");
        remove_tree(&root);
    }

    #[test]
    fn a_policy_denied_action_never_becomes_a_pending_approval() {
        let root = temp_dir("escalate-deny-cap");
        let policy = Policy::new(crate::policy::Mode::Ask);
        let decision = policy.check(
            &Capability::File {
                scope: PathScope::Any,
                mode: FileMode::Write,
            },
            "C:\\Windows\\system.ini",
            "deadbeef",
        );
        let mut queue = Queue::load(&root).expect("空队列");
        let gate = settle(
            &mut queue,
            &request("C:\\Windows\\system.ini"),
            &decision,
            EVERY,
        );
        assert!(
            matches!(gate, Gate::Refused { .. }),
            "权限表禁止的动作不能变成一条待批：{gate:?}"
        );
        assert!(
            queue.items.is_empty(),
            "挂进队列就等于给用户一个把它点通过的入口，那红线不算存在"
        );
        remove_tree(&root);
    }

    /// 有人在听的话题由 ApprovalHub 弹窗处理：那条回合没登记成无人值守，
    /// 队列一次都不该插手——否则一次普通审批就被写成了一条永久待办
    #[test]
    fn an_attended_turn_is_left_to_the_approval_hub() {
        let root = temp_dir("escalate-attended");
        let display = "C:/work/notes.md".to_string();
        let decision = write_decision(&display);
        assert!(
            matches!(decision, Decision::Ask { .. }),
            "这条动作本身是要点头的，测试才有意义"
        );
        let gate = park_for_turn(
            &root,
            "conv-in-the-ui",
            "file.write.projectRoot",
            &display,
            &decision,
            EVERY,
        );
        assert!(
            gate.expect("读空队列不该报错").is_none(),
            "没登记成无人值守的话题不该进队列"
        );
        assert!(!queue_path(&root).exists(), "不该被顺手落一份队列文件");
        remove_tree(&root);
    }

    /// 执行侧的入口：登记表 + 落盘 + 审计。被权限表拒绝的一发不该进队列，但它得留一行
    #[test]
    fn a_denied_call_from_a_background_turn_is_audited_without_becoming_a_pending_item() {
        let root = temp_dir("escalate-refused");
        let decision = Policy::new(crate::policy::Mode::Ask).check(
            &Capability::File {
                scope: PathScope::Any,
                mode: FileMode::Write,
            },
            "C:\\Windows\\system.ini",
            "deadbeef",
        );
        let _unattended = watch_run("conv-refused", "run-9", "t9", StartedBy::Scheduler);
        let gate = park_for_turn(
            &root,
            "conv-refused",
            "file.write.any",
            "C:\\Windows\\system.ini",
            &decision,
            EVERY,
        )
        .expect("拒绝不需要读队列")
        .expect("这条话题是无人值守的");
        assert!(
            matches!(gate, Gate::Refused { .. }),
            "根外写入是直接拒，不是等人：{gate:?}"
        );
        assert!(
            !queue_path(&root).exists(),
            "被拒的动作没有可批的余地，队列里不该多出它"
        );
        let lines = audit::read_day(&root, None);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("\"action\":\"task:escalate\"")
                    && line.contains("\"outcome\":\"denied\"")
                    && line.contains("\"actor\":\"scheduler\"")),
            "拦下也要留下是谁拦的：{lines:?}"
        );
        remove_tree(&root);
    }

    /// 回合侧只认 [`park_for_turn`] 这一个入口（`chat.rs::park_unattended` 调的就是它）。
    /// 上面那几条挂起测试走的是 `settle`，所以这一条要钉的是入口自己：登记表要认得出、
    /// run_id 与 task_id 要从登记表带过来、队列要真落盘、审计要真有一行、磁盘不能被动
    #[test]
    fn an_unattended_ask_parks_at_the_entry_the_turn_actually_calls() {
        let root = temp_dir("escalate-park-entry");
        let target = root.join("notes.md");
        fs::write(&target, "原样").expect("放一个要被改的文件");
        let display = target.to_string_lossy().to_string();
        let decision = write_decision(&display);
        let _unattended = watch_run("conv-parked", "run-7", "t7", StartedBy::Scheduler);

        let gate = park_for_turn(
            &root,
            "conv-parked",
            "file.write.projectRoot",
            &display,
            &decision,
            EVERY,
        )
        .expect("空队列读得动")
        .expect("这条话题登记过，是无人值守的");
        act_on(&gate, &target);
        let Gate::Parked(item) = gate else {
            panic!("无人值守撞到 ask 必须挂起：{gate:?}");
        };
        assert_eq!(item.run_id, "run-7", "待批项得知道是哪一次运行停在半路");
        assert_eq!(item.task_id, "t7", "以及是哪个任务的哪一发");
        assert_eq!(
            fs::read_to_string(&target).expect("文件该还在"),
            "原样",
            "挂起的那一发没碰过磁盘"
        );
        // 只有落了盘，重启后 parked_in 才说得出"这条话题停在哪一发"
        let revived = Queue::load(&root).expect("重启后读得回队列");
        assert_eq!(
            revived
                .waiting_for("conv-parked")
                .map(|held| held.id.as_str()),
            Some(item.id.as_str()),
            "挂起没落盘就等于没挂起"
        );
        let lines = audit::read_day(&root, None);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("\"action\":\"task:escalate\"")
                    && line.contains("\"outcome\":\"blocked\"")
                    && line.contains("file.write.projectRoot")
                    && line.contains("run-7")),
            "挂起要留下一行说得清等什么、是哪一次运行停在这：{lines:?}"
        );
        remove_tree(&root);
    }

    /// 执行侧的入口：登记表 + 落盘 + 审计。坏队列必须把动作停下，而不是"当作没有待审批"
    #[test]
    fn an_unreadable_queue_file_stops_the_action_rather_than_forgiving_it() {
        let root = temp_dir("escalate-corrupt");
        fs::write(queue_path(&root), "{\"items\": [ 半份 json").expect("放一份坏队列");
        let display = "C:/work/notes.md".to_string();
        let decision = write_decision(&display);
        let _unattended = watch_run("conv-corrupt", "run-1", "t1", StartedBy::Scheduler);
        let error = park_for_turn(
            &root,
            "conv-corrupt",
            "file.write.projectRoot",
            &display,
            &decision,
            EVERY,
        )
        .expect_err("队列读不动时不能放行这一发");
        assert!(error.contains("读不动"), "报错要说清是队列坏了：{error}");
        assert!(
            !is_unattended("conv-missing"),
            "没登记的话题不是无人值守，别替它挂起"
        );
        remove_tree(&root);
    }

    #[test]
    fn a_finished_turn_stops_being_unattended() {
        let conversation = "conv-guard";
        {
            let _guard = watch_run(conversation, "run-1", "t1", StartedBy::User);
            assert!(is_unattended(conversation), "回合进行中它没人可问");
        }
        assert!(
            !is_unattended(conversation),
            "回合结束还留着名单，下一条话题就会被错挂起"
        );
    }

    /// 待批项发给界面的形状与前端类型一字不差。这条是 `PlanView` 少过两个字段之后补的规矩：
    /// 手抄的 TS 接口只要没人对着序列化结果核一遍，它就一定会漂
    #[test]
    fn a_pending_item_matches_the_frontend_type() {
        let item = PendingApproval {
            id: "apr-1".into(),
            run_id: "run-1".into(),
            task_id: "t1".into(),
            conversation_id: "conv-1".into(),
            capability: "file.write.projectRoot".into(),
            target: "C:/work/notes.md".into(),
            fingerprint: "deadbeef".into(),
            reason: "要执行写文件，先确认这一份".into(),
            requested_at: 1_000,
            status: PendingStatus::Waiting,
            // 有值才看得见：`assert_matches_ts` 比的是序列化出来的键，None 的字段在它眼里不存在
            decided_at: Some(2_000),
        };
        crate::test_support::assert_matches_ts(
            &serde_json::to_value(&item).expect("待批项总能编码"),
            "TaskApproval",
        );
    }

    /// 点头是一条 standing 授权（同一发以后不再问），所以它必须能撤回：
    /// 撤完之后同一发重新回到"问一次"，而不是继续拿当初那一句"我同意过"动手
    #[test]
    fn a_revoked_nod_asks_again_instead_of_running_on_the_old_precedent() {
        let root = temp_dir("escalate-revoke");
        let target = root.join("notes.md");
        fs::write(&target, "原样").expect("放文件");
        let display = target.to_string_lossy().to_string();
        let decision = write_decision(&display);

        let mut queue = Queue::load(&root).expect("空队列");
        let parked = settle(&mut queue, &request(&display), &decision, EVERY);
        let Gate::Parked(record) = parked else {
            panic!("第一次该挂起等人：{parked:?}")
        };
        queue
            .decide(&record.id, true, EVERY + 1_000)
            .expect("点头该记下");
        queue.save(&root).expect("落盘");

        let mut queue = Queue::load(&root).expect("读回");
        assert!(
            matches!(
                settle(&mut queue, &request(&display), &decision, EVERY + 2_000),
                Gate::Execute
            ),
            "撤回之前，那一发按先例放行"
        );
        assert_eq!(queue.decided().len(), 1, "表过态的要说得出一条");

        let (standing, gone) = queue.forget(&record.id).expect("撤回一条表态");
        assert_eq!(
            standing.capability, "file.write.projectRoot",
            "要回报撤的是哪一发"
        );
        assert_eq!(gone, 1);
        assert!(queue.decided().is_empty(), "撤完了就不该还有先例");
        queue.save(&root).expect("落盘");

        let mut again = Queue::load(&root).expect("重启后读回");
        let gate = settle(&mut again, &request(&display), &decision, EVERY + 60_000);
        act_on(&gate, &target);
        assert!(
            matches!(gate, Gate::Parked(_)),
            "撤回之后该重新问，而不是照旧放行：{gate:?}"
        );
        assert_eq!(
            fs::read_to_string(&target).expect("文件该还在"),
            "原样",
            "重新问的那一发没动手"
        );
        remove_tree(&root);
    }

    /// 反方向同样要能撤：误点一次"拒绝"不该把那一步永久钉死
    #[test]
    fn revoking_a_denial_unblocks_that_step_rather_than_locking_it_forever() {
        let root = temp_dir("escalate-revoke-deny");
        let target = root.join("notes.md");
        fs::write(&target, "原样").expect("放文件");
        let display = target.to_string_lossy().to_string();
        let decision = write_decision(&display);

        let mut queue = Queue::load(&root).expect("空队列");
        let parked = settle(&mut queue, &request(&display), &decision, EVERY);
        let Gate::Parked(record) = parked else {
            panic!("第一次该挂起等人：{parked:?}")
        };
        queue
            .decide(&record.id, false, EVERY + 1_000)
            .expect("拒绝也是一次表台");
        let refused = settle(&mut queue, &request(&display), &decision, EVERY + 2_000);
        assert!(
            matches!(refused, Gate::Refused { .. }),
            "被拒的先例该挡住：{refused:?}"
        );

        queue.forget(&record.id).expect("撤回那次拒绝");
        let reopened = settle(&mut queue, &request(&display), &decision, EVERY + 3_000);
        assert!(
            matches!(reopened, Gate::Parked(_)),
            "先例没了就该重新问一次：{reopened:?}"
        );
        assert_eq!(
            queue.waiting().len(),
            1,
            "重新问出来的是那条待办，不是第二条先例"
        );
        remove_tree(&root);
    }

    /// 撤的是"这一发"，不是"这一行"：同一发在盘上叠了先例时，只划掉最上面那条等于
    /// 按下撤回而闸门纹丝不动——`verdict` 取的就是最近一条，底下那条会立刻顶上。
    /// 而还在等的那一条不是先例，不该被顺手清掉
    #[test]
    fn a_revocation_clears_the_whole_shot_and_leaves_the_waiting_ones_alone() {
        let base = PendingApproval {
            id: "apr-1".into(),
            run_id: "run-1".into(),
            task_id: "t1".into(),
            conversation_id: "conv-1".into(),
            capability: "file.write.projectRoot".into(),
            target: "C:/work/notes.md".into(),
            fingerprint: "deadbeef".into(),
            reason: "要执行写文件，先确认这一份".into(),
            requested_at: 1_000,
            status: PendingStatus::Approved,
            decided_at: Some(2_000),
        };
        let newer = PendingApproval {
            id: "apr-2".into(),
            decided_at: Some(3_000),
            ..base.clone()
        };
        let standing = PendingApproval {
            id: "apr-3".into(),
            fingerprint: "别的指纹".into(),
            status: PendingStatus::Waiting,
            decided_at: None,
            ..base.clone()
        };
        let mut queue = Queue {
            items: vec![base, newer, standing],
        };

        assert_eq!(queue.decided().len(), 2);
        assert_eq!(
            queue.forget("apr-2").expect("撤一条先例").1,
            2,
            "同一发的两条先例都要划掉"
        );
        assert_eq!(queue.waiting().len(), 1, "还在等的那一条不该被顺手清掉");
        assert!(
            queue.forget("apr-3").is_err(),
            "撤一条还没表台的该被拒：那等于替用户取消一次排队"
        );
        assert!(
            queue.forget("没这个 id").is_err(),
            "不存在的 id 不能说成撤回了"
        );
        assert_eq!(queue.decided().len(), 0);
    }
}
