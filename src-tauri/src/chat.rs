use std::io::Read;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::ipc::Channel;
use tauri::Emitter;
use tauri::Manager;
use tauri::{AppHandle, State};

use crate::approvals::ApprovalHub;
use crate::config::{self, AppConfig};
use crate::session::context::SUMMARY_MARKER;
use crate::session::entry::{
    EntryPayload, ImageRef, Message, NewEntry, SettledAssistant, StopReason, ToolCall,
};
use crate::session::sections::Section;
use crate::tool_runtime;
use crate::tools;

/// 停止生成时的特殊错误标记：它不是失败，run_turn 捕获后按正常收尾处理
const STOP_MARK: &str = "__STOPPED__";

/// 定形条目在 `custom.custom_type` 里的名字。声明数组只写一次，之后每轮从日志读回来发
const DECLARATIONS_TYPE: &str = "tool_declarations";

/// 一次回合的发送视图：**日志是唯一发送源**，`rows` 永远是"常驻段 ++ 日志投影"。
///
/// 每次追加都整体重算而不是增量维护，是因为增量维护又要一份"两边怎么对齐"的判定——
/// 那正是上一轮改造双轨真相的形状。重算让"缓存和日志不一致"没有存在的方式，
/// 代价是每条新行重投影一次（对话规模下是本地几微秒的事）
pub(crate) mod mode_goal;

use self::mode_goal::{
    arm_goal_round, close_turn, contract_from_json, contract_view, goal_block_on_turn_error,
    goal_state_from, interrupted_at_boundary, mode_state_from, mode_view, report_goal,
    spent_reading, Next, Step,
};
mod wire;

use crate::net::with_timeouts;
use wire::{
    describe_status, is_pool_swappable_error, DeltaCoalescer, EgressFail, ANTHROPIC_VERSION,
    ENHANCE_SYSTEM, RETRYABLE_STATUS_WORD,
};
pub(crate) use wire::{provider_gate, request_round};

mod tool_gate;

use self::tool_gate::{
    audit_tool_in, emit_hooks, mask_tool_input, park_unattended, parse_arguments, short_label,
    Escalated,
};
mod message_build;

use self::message_build::{clamp_tool_result, pack_tool_result, settle_failed, tool_result_pair};

pub struct Send {
    opened: crate::session::legacy::Migration,
    standing: Vec<Value>,
    /// 这一轮的命名段（§6.1）。跟着发送视图走，是因为写段差分行与写压缩边界的是同一批代码
    sections: Vec<Section>,
    rows: Vec<Value>,
    history: Vec<Value>,
    /// 本次回合里新登记进日志的条目 id，按顺序
    pushed: Vec<String>,
    /// 这一回合真正在用的模型名（池/路由表换过人之后的那一个）。
    /// assistant 行落账时随条目带上——投影恢复"这句是谁答的"读的就是它
    model: String,
    /// microcompact 读侧变换的开关（优化路线 O5-1/O5-2）：开着时**每次派生**
    /// 都把旧工具结果清成占位。日志一行不动；同一份日志派生两次字节一致（守卫三钉之二）。
    /// 让步闸开了它，回合收尾即随 Send 消失——不跨回合记忆
    microcompact: bool,
}

impl Send {
    fn open(
        opened: crate::session::legacy::Migration,
        standing: Vec<Value>,
        sections: Vec<Section>,
    ) -> Result<Self, String> {
        let mut send = Self {
            opened,
            standing,
            sections,
            rows: Vec::new(),
            history: Vec::new(),
            pushed: Vec::new(),
            model: String::new(),
            microcompact: false,
        };
        send.refresh()?;
        Ok(send)
    }

    fn refresh(&mut self) -> Result<(), String> {
        self.history = crate::session::prefix::sent_array(&self.opened.log)
            .map_err(|error| error.to_string())?;
        if self.microcompact {
            // microcompact 读侧变换挂在派生出口：回合内每追加一行都重新派生，
            // 变换跟着重算——新工具结果挤进"最近 5 条"窗口，语义天然正确
            let (cleared, _) = crate::session::context::clear_stale_tool_results(&self.history);
            self.history = cleared;
        }
        self.rows = assemble_thread(&self.history, self.standing.clone());
        Ok(())
    }

    /// 追加一行的唯一出口。返回新条目的 id，事件要带着它回界面
    /// 把分支末端移到某条条目之后（`None` = 退到根之前）。
    /// 这就是"重新生成"和"编辑重发"唯一的机制：移动末端，旧条目一条都不删
    fn rewind(&mut self, target: Option<&str>) -> Result<(), String> {
        self.opened
            .log_mut()
            .navigate(target)
            .map_err(|error| error.to_string())?;
        self.refresh()
    }

    /// 取走本轮新登记的条目 id（发给界面的 Done 用）。取走而不是看一眼，
    /// 是为了第二次 Done 不会把上一次那批再报一遍
    fn take_pushed(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pushed)
    }

    fn push(&mut self, message: crate::session::entry::Message) -> Result<String, String> {
        self.append(crate::session::entry::EntryPayload::Message { message })
    }

    fn append(&mut self, payload: EntryPayload) -> Result<String, String> {
        let id = self.append_quiet(payload)?;
        self.pushed.push(id.clone());
        Ok(id)
    }

    /// 追加但不进 `pushed`：那批 id 是给界面指认"哪一行消息"用的，而段行与定形条目在界面上
    /// 没有对应的那一行，报上去只会让"重新生成"指向一个屏幕上不存在的东西
    fn append_quiet(&mut self, payload: EntryPayload) -> Result<String, String> {
        // assistant 行把实发模型名随条目落账（空的 model 是测试/无回合上下文的路径，不注）：
        // 台账在中途重启/崩溃时会停在旧一拍，"这句是谁答的"以日志这一格为准，
        // 台账认领（restore_models_from_ledger）退居老日志的兜底
        let entry = match &payload {
            EntryPayload::Message {
                message: Message::Assistant(_),
            } if !self.model.is_empty() => NewEntry::new(payload).with_model(self.model.clone()),
            _ => NewEntry::new(payload),
        };
        let entry = self
            .opened
            .log_mut()
            .append(entry, crate::session::now_millis())
            .map_err(|error| error.to_string())?;
        let id = entry.id.clone();
        self.refresh()?;
        Ok(id)
    }

    /// 声明数组**首轮定形**（§6.2）。工具渲染在对话**之前**，所以中途新增或下线一条声明，
    /// 破坏的和"改中段正文"是同一级的：插入点之后的整段历史全部失配。
    /// 于是首轮把当时能声明的全部写进日志，此后每轮原样重发——服务器掉线不会让数组变短
    /// （调用它时回一条 error 工具结果），中途新连上的服务器也只能等下一个话题
    fn declarations(&mut self, fresh: Vec<Value>) -> Result<Vec<Value>, String> {
        let frozen = crate::session::context::latest_custom(&self.opened.log, DECLARATIONS_TYPE)
            .map_err(|error| error.to_string())?
            .and_then(|data| data.as_array().cloned());
        if let Some(items) = frozen {
            return Ok(items);
        }
        let items = fresh.clone();
        self.append_quiet(EntryPayload::Custom {
            custom_type: DECLARATIONS_TYPE.into(),
            data: Some(Value::Array(fresh)),
        })?;
        Ok(items)
    }

    /// 段同步：把"该追加的差分行"落成条目。内容没变就一条都不写（§6.1），
    /// 所以"未变的段沿用已存渲染"是这套机制的默认结果，不需要额外的字节门控代码
    fn sync_sections(&mut self) -> Result<(), String> {
        let entries = {
            let path = self.opened.log.path().map_err(|error| error.to_string())?;
            let effect = crate::session::sections::in_effect(&path);
            crate::session::sections::pending(&self.sections, &effect)
        };
        for payload in entries {
            self.append_quiet(payload)?;
        }
        Ok(())
    }

    /// 压缩边界要带走的 system 快照。没有段就没有快照：边界不该凭空多出一行空正文
    fn section_snapshot(&self) -> Option<Message> {
        crate::session::sections::snapshot(&self.sections)
    }

    /// 装配前那一眼：本轮打算新塞进去、还没写进日志的那几段折进各自所属的层之后，
    /// 让步阶梯有没有点到"这一轮不发记忆段"。
    ///
    /// 判据只有阶梯那一份，这里不许再有一条"看起来快满了"的百分比（§13）。
    /// `deficit > 0` 时直接不判：那是不让步的那几层自己就装不下，裁记忆段既救不了它，
    /// 又把一次该报的 Notice 换成一次静默少发。
    ///
    /// 只管**窗口**这一种理由。"这一段本身太长"是另一种，与窗口无关，在 `memory_over_cap`
    fn must_yield_memory(
        &self,
        sizing: crate::session::layers::BudgetInput,
        input: &str,
    ) -> Result<bool, String> {
        let path = self.opened.log.path().map_err(|error| error.to_string())?;
        let effect = crate::session::sections::in_effect(&path);
        let uses = crate::session::layers::uses(&self.opened.log, &self.standing)
            .map_err(|e| e.to_string())?;
        let mut pending: Vec<(crate::session::layers::Layer, usize)> =
            crate::session::sections::pending(&self.sections, &effect)
                .iter()
                .filter_map(|payload| {
                    Some((
                        crate::session::layers::classify(payload)?,
                        crate::session::context::payload_chars(payload),
                    ))
                })
                .collect();
        if !input.is_empty() {
            let question = EntryPayload::Message {
                message: Message::User {
                    content: input.to_string(),
                    images: Vec::new(),
                    audios: Vec::new(),
                    videos: Vec::new(),
                },
            };
            pending.push((
                crate::session::layers::Layer::Turn,
                crate::session::context::payload_chars(&question),
            ));
        }
        let folded = crate::session::layers::fold_pending(&uses, &pending);
        let budget = crate::session::layers::budget(&folded, sizing);
        if budget.deficit > 0 {
            return Ok(false);
        }
        let plan =
            crate::session::layers::plan(crate::session::layers::estimate(&folded), &budget, None);
        Ok(plan
            .ladder
            .contains(&crate::session::layers::Concession::DropMemorySection))
    }

    /// 检索出来的记忆段**自己**超过 `memorySectionMaxChars` 没有。超了就整段不发，
    /// 返回它实际的字符数，好让那句 Notice 说得出是多少。
    ///
    /// 这是**让**不是**夹**：那段是按本轮提法挑出来的几条记录，截一半等于伪造一条
    /// 从来没存在过的记忆（§14.1）。判据与窗口无关，所以它不看日志、也不看阶梯。
    /// `max = 0` = 不设上限，永远 `None`
    fn memory_over_cap(&self, max: usize) -> Option<usize> {
        if max == 0 {
            return None;
        }
        self.sections
            .iter()
            .find(|section| section.name == crate::session::sections::MEMORY)
            .map(|section| section.row().chars().count())
            .filter(|chars| chars > &max)
    }

    /// 本回合真正发出去的那批字节
    fn rows(&self) -> &[Value] {
        &self.rows
    }

    /// 只有历史（不含常驻段）：压缩估算与"还能不能压"的判据都看这一份
    fn history(&self) -> &[Value] {
        &self.history
    }

    /// 常驻段。压缩前的固定开销要算它，但它不在日志里（日志是历史，常驻段每轮重新装配）
    fn standing(&self) -> &[Value] {
        &self.standing
    }

    /// 当前投影里**每一行**出自哪条条目：与 `history()` 逐行对齐，压缩选边界时用它把
    /// "第几行"翻回"第几条条目"。对齐是这件事的全部意义——按条目的下标查按行的数组，
    /// 一条带段快照的压缩条目就能让整条尾巴错位一格
    fn provenance(&self) -> Result<Vec<String>, String> {
        Ok(crate::session::context::project(&self.opened.log)
            .map_err(|error| error.to_string())?
            .row_origins()
            .into_iter()
            .map(str::to_string)
            .collect())
    }

    /// 这一轮是不是"压缩后的第一笔"。记账要拿它断开白付量的基线：压缩换来的那次低命中
    /// 不是浪费（§7.3），而把它算进去会让面板每次压缩后都报一笔虚构的损失
    fn starts_fresh_chain(&self) -> Result<bool, String> {
        crate::session::context::starts_fresh_chain(&self.opened.log)
            .map_err(|error| error.to_string())
    }

    fn save(&self) {
        // 快照式的"尽力而为"：落盘失败只说一声，绝不能把对话打断
        if let Err(error) = self.opened.save() {
            eprintln!("话题日志没能落盘：{error}");
        }
    }
}

#[allow(dead_code)] // Main 侧封装：worker 直用 _in 变体；M5 chat.rs 拆空时统一清算
/// 话题日志的根目录。分桶在它下面，所以这里只到 sessions 这一层
fn sessions_root(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_config_dir()
        .map(|dir| dir.join("sessions"))
        .map_err(|error| error.to_string())
}

/// 打开一条话题的日志：已经有就读它，没有就从界面台账迁一次。
/// 新话题还没有台账，就是空日志——这不是错误，不该拦下第一条消息
pub(crate) fn open_session(
    app: &AppHandle,
    conversation_id: &str,
) -> Result<crate::session::legacy::Migration, String> {
    open_session_in(
        &app.path().app_config_dir().map_err(|e| e.to_string())?,
        &app.path().app_data_dir().map_err(|e| e.to_string())?,
        conversation_id,
    )
}

/// worker 进程的变体（M2 切片 2）：两个目录由 Main 经 CLI 传来，
/// 台账后端从 config_dir 里的 config.json 现读——与界面同一个真相
pub(crate) fn open_session_in(
    config_dir: &std::path::Path,
    data_dir: &std::path::Path,
    conversation_id: &str,
) -> Result<crate::session::legacy::Migration, String> {
    let root = config_dir.join("sessions");
    let backend = crate::config::load_from_dir(config_dir).conversation_store;
    let ledger = crate::history::current_in(config_dir, data_dir, &backend)
        .and_then(|store| store.load(conversation_id))
        .unwrap_or_else(|_| crate::history::Conversation {
            id: conversation_id.into(),
            created_at: crate::session::now_millis(),
            ..Default::default()
        });
    crate::session::legacy::Migration::open(&root, &ledger)
}

/// 把分支末端移到某条条目之后，**不发任何请求**。
///
/// 为什么需要它：`rewindTo` 是坐在 `chat_send` 上的，也就是"换一支看看"这件事
/// 以前必须附带一次真请求才做得到。切换器要做的恰恰是"我先看看那一支说了什么"，
/// 所以移动末端得是一个自己的动作。`entry_id` 为 None = 退到根之前（下一条追加成新根）
///
/// 只移动末端，一个字节都不删：被放弃的那一支还在树里，还能再走上去
#[tauri::command]
pub fn conversation_navigate(
    app: AppHandle,
    conversation_id: String,
    entry_id: Option<String>,
) -> Result<Option<String>, String> {
    let mut session = open_session(&app, &conversation_id)?;
    session
        .log_mut()
        .navigate(entry_id.as_deref())
        .map_err(|error| error.to_string())?;
    session.save()?;
    Ok(entry_id)
}

/// 话题树的一份只读投影：树本身（id / 父 / 序号 / 类别）加上"当前停在哪一条"。
///
/// 它不新增任何存储，也没有能塞回去的参数——照 Inspector 那条纪律，字段全是 `Serialize`
/// 而没有 `Deserialize`，"界面读数不对、改改报告把它掰回来"这条路在类型上写不出来。
/// 前端那份 `parentId` 是从这里抄的，不是自己编的：父子关系只在"后端铸造 + 前端抄录"
/// 这一条路上产生，两份真相就是这么防住的
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TreeNode {
    pub id: String,
    pub parent_id: Option<String>,
    pub seq: u64,
    /// 条目类别（message / compaction / model_change / usage…）。取自序列化出来的 tag，
    /// 不另抄一份映射表——那张表迟早和 enum 漂移
    pub kind: String,
    /// 只有承载消息的行有角色；边界行、用量行这些是 None
    pub role: Option<String>,
    /// 消息行的正文预览（前 200 字符）。前端"重新生成"的兜底路径用它把
    /// 重问的问题对回日志里已落的那一行（失败回合拿不到条目 id，见前端侧注释）
    pub preview: Option<String>,
    pub at: i64,
    /// 在不在当前分支上（从 tip 沿父链走到根）
    pub on_path: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationTree {
    /// 当前分支末端。None = 这条话题还没写过任何条目
    pub tip: Option<String>,
    pub nodes: Vec<TreeNode>,
}

#[tauri::command]
pub fn conversation_tree(
    app: AppHandle,
    conversation_id: String,
) -> Result<ConversationTree, String> {
    use std::collections::HashSet;

    use crate::session::entry::{EntryPayload, Message};
    let source = open_session(&app, &conversation_id)?;
    let log = &source.log;
    // 当前分支一次走到底做成集合，而不是每个节点各回一遍父链
    let on_path: HashSet<String> = log
        .path()
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|entry| entry.id.clone())
        .collect();
    let nodes = log
        .entries()
        .iter()
        .map(|entry| {
            let role = match entry.payload() {
                EntryPayload::Message { message } => match message {
                    Message::User { .. } => Some("user"),
                    Message::Assistant(_) => Some("assistant"),
                    Message::Tool { .. } => Some("tool"),
                    Message::System { .. } => Some("system"),
                },
                _ => None,
            };
            let preview = match entry.payload() {
                EntryPayload::Message { message } => match message {
                    Message::User { content, .. } => {
                        Some(content.chars().take(200).collect::<String>())
                    }
                    Message::Assistant(settled) => {
                        Some(settled.content.chars().take(200).collect::<String>())
                    }
                    _ => None,
                },
                _ => None,
            };
            TreeNode {
                id: entry.id.clone(),
                parent_id: entry.parent_id.clone(),
                seq: entry.seq,
                kind: serde_json::to_value(entry.payload())
                    .ok()
                    .and_then(|value| value["type"].as_str().map(str::to_string))
                    .unwrap_or_else(|| "unknown".to_string()),
                role: role.map(str::to_string),
                preview,
                at: entry.timestamp,
                on_path: on_path.contains(&entry.id),
            }
        })
        .collect();
    Ok(ConversationTree {
        tip: log.leaf_id().map(str::to_string),
        nodes,
    })
}

/// 一条判据在界面上的投影（design-goal-mode.md §4.5）。`risk` 查 `tools::classify`
/// ——那张"动不动东西"的表是唯一出处，界面不另算；`evidence` 是读侧聚合的
/// 闭合状态，完成门与判据清单看的是同一份
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CriterionView {
    pub id: String,
    pub text: String,
    /// "check" | "judgment"
    pub kind: &'static str,
    pub command: Option<String>,
    /// tools::Risk::as_str()："safe" | "elevated" | "high"。
    /// safe 的 check 收尾时会被运行时复跑，其余只接上报——这个差别要在人写下的那一刻看得见
    pub risk: &'static str,
    /// "open" | "runtime" | "reported" | "failed"
    pub evidence: &'static str,
}

/// 契约的界面投影。判据从 0 条到 N 条，弹框、目标带与完成门读的都是这一份
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContractView {
    pub criteria: Vec<CriterionView>,
    pub constraints: Vec<String>,
}

/// 一条话题作业模式的读数。照 [`TreeNode`] 那条纪律：字段全是 `Serialize` 而没有
/// `Deserialize`——"界面读数不对，改改那份 JSON 把它掰回来"这条路要在类型上写不出来
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModeView {
    /// "chat" | "plan" | "goal"
    pub mode: &'static str,
    pub objective: Option<String>,
    /// 目标模式自己续跑过几轮。它是读数不是配额——目标没有轮次上限。
    /// 用户自己发的消息不占这一格
    pub turns_used: u32,
    /// 花费上限，单位 1e-8 美元（钱不用 float 过 IPC，与编排那格同一把尺）。0 = 不设
    pub max_cost_usd_e8: i64,
    /// 从起目标那一刻算到现在的实花，同样来自台账——日志里不存第二份钱。
    /// `None` = **账读不出来**，界面上要显示"不知道"而不是 `$0.00`（见 [`spent_reading`]）
    pub spent_usd_e8: Option<i64>,
    /// 目标现在处于哪一格："active" | "paused" | "blocked" | "usage_limited" |
    /// "budget_limited" | "complete"。界面那六格的中文与动作按 `§7.1` 那张表查，
    /// 不在前端另算一套
    pub status: &'static str,
    /// 收尾那一句：完成时交付了什么，停住时卡在哪
    pub note: Option<String>,
    /// 目标点名执行的服务商档案 id。None = 跟随当前配置。目标与档案是引用关系：
    /// 档案改了连接，目标跟着新连接走
    pub profile: Option<String>,
    /// 目标的身份。界面拿它给分叉出的同一支目标分组（design-goal-mode.md §5.5）；
    /// 旧行读侧补铸过 `legacy-<id>`，所以这一格对旧话题也有值
    pub goal_id: Option<String>,
    /// 完成契约的投影。None = 旧式目标（无判据），完成门对它退化、界面标「无判据」
    pub contract: Option<ContractView>,
    /// 规划模式且模型已经把话说完了——界面上那条「批准并执行」只看这一格。
    /// 它由日志派生（最新一条用户发言之后有没有一条以文本收尾的回答），
    /// 不是前端猜，也不是新存一格状态
    pub plan_ready: bool,
}

#[tauri::command]
pub fn conversation_fork(
    app: AppHandle,
    conversation_id: String,
    entry_id: String,
) -> Result<String, String> {
    let mut source = open_session(&app, &conversation_id)?;
    let log = branch_to_user_anchor(&mut source, &entry_id)?;

    let source_ledger = crate::history::current(&app)?.load(&conversation_id)?;
    let now = crate::session::now_millis();
    let new_id = format!(
        "conv-{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    crate::history::save_conversation(&app, fork_ledger(&source_ledger, new_id.clone(), now))?;

    let mut forked = open_session(&app, &new_id)?;
    forked.opened.log = log;
    // 谱系落在话题日志自己的 header 上：这是唯一说得出"这一支从哪来"的地方。
    // 标题里那句"（分叉）"是给人看的显示，不是事实——它能被改名，header 不能
    forked.opened.header.parent_session = Some(conversation_id.clone());
    // 分叉继承的目标**默认不接轮**（design-goal-mode.md §5.5）：不落这一行，
    // 同一支目标就有两处各自续跑、各花一份钱，而美元上限按话题聚合、两支互相看不见。
    // 落的是一行 paused——同一支目标的"接手"就是按一次「继续」，语义等价且
    // 不需要第二个条目类型。身份（goal_id）原样跟着走，角落卡靠它把同一支的
    // 几支合成一组
    let inherited = crate::session::mode::in_effect(&forked.opened.log);
    if inherited.goal_held() {
        let parked = crate::session::mode::State {
            status: crate::session::mode::Status::Paused,
            note: Some(format!(
                "分叉继承，未启动：目标抄自「{}」，在这一支按「继续」才往下跑。",
                if source_ledger.title.trim().is_empty() {
                    "未命名话题"
                } else {
                    source_ledger.title.trim()
                }
            )),
            ..inherited
        };
        forked
            .opened
            .log
            .append(NewEntry::new(crate::session::mode::row(&parked)), now)
            .map_err(|error| error.to_string())?;
    }
    forked.save()?;
    Ok(new_id)
}

/// 把源话题切到"那条用户消息为止"的那一段，切不出来的三种情况各报一句。
///
/// 剥成拿 `&mut Migration` 的助手，是因为整条命令里 `AppHandle` 用在四处
/// （两次开话题 + 读账本 + 写账本），于是"锚点是助手消息时到底分不分得出去"
/// 这条判据从来没有命令级测试——而它守的是**把半轮对话当成一轮**这种坏法：
/// 从助手消息处分叉，新话题的最后一句不是用户问的，模型下一发看到的
/// 是一个它自己刚说完的结尾
fn branch_to_user_anchor(
    source: &mut crate::session::legacy::Migration,
    entry_id: &str,
) -> Result<crate::session::SessionLog, String> {
    use crate::session::entry::{Entry, EntryPayload};
    use crate::session::SessionLog;

    source
        .log_mut()
        .navigate(Some(entry_id))
        .map_err(|error| error.to_string())?;
    let entries: Vec<Entry> = source
        .log
        .path()
        .map_err(|error| error.to_string())?
        .into_iter()
        .cloned()
        .collect();

    let anchor_is_user = matches!(
        entries.last().map(|entry| entry.payload()),
        Some(EntryPayload::Message {
            message: crate::session::entry::Message::User { .. }
        })
    );
    if !anchor_is_user {
        return Err("分叉锚点必须是一条用户消息。".into());
    }

    SessionLog::restore(entries).map_err(|error| error.to_string())
}

/// 分叉出去的那本账。剥成纯函数是因为这里每一格都是一个"抄还是清"的决定，
/// 而这些决定以前只有起着应用才看得见：
/// **正文与用量不带过来** —— 它们住在话题日志里，抄一份就是同一件事住两处，
/// 从此以后两边会各说各话；标题要带上那句"（分叉）"（给人看的显示，
/// 谱系那一格在话题日志的 header 上，不靠标题）；空标题要落成一个念得出的名字，
/// 不能抄出一个空的话题名；时间戳是**现在**，不是源话题的出生时间
fn fork_ledger(
    source: &crate::history::Conversation,
    new_id: String,
    now: i64,
) -> crate::history::Conversation {
    crate::history::Conversation {
        id: new_id,
        project_id: source.project_id.clone(),
        title: format!(
            "{}（分叉）",
            if source.title.trim().is_empty() {
                "未命名话题"
            } else {
                source.title.trim()
            }
        ),
        created_at: now,
        updated_at: now,
        // 分叉不继承置顶：它是"这一支的头部复制"，置顶是用户对**原话题**的态度
        pinned: false,
        // 分叉继承能力档：生图会话分出来的还是生图会话
        kind: source.kind.clone(),
        messages: Vec::new(),
        usage: None,
        video_nodes: Vec::new(),
        video_edges: Vec::new(),
    }
}

/// 空输入 = 纯重新生成：末端已经移到要重新回答的那条之后，不该再追加第二个同样的问题
fn should_append_input(input: &str, attachments: &[String]) -> bool {
    !input.is_empty() || !attachments.is_empty()
}

/// 一轮新输入的组装产物：正文 + 三类媒体引用。**只记"用户给了什么"**，
/// "这一发能不能看见/听见"由出站投影 (`project_content`) 按当次的模型决定
#[derive(Default)]
struct PendingInput {
    content: String,
    images: Vec<ImageRef>,
    audios: Vec<crate::session::entry::MediaRef>,
    videos: Vec<crate::session::entry::MediaRef>,
}

/// 把用户挑的附件拼进这一条 user 行，并交出这一行带的媒体引用。
///
/// 分工是刻意的：**这一行只记"用户给了什么"**，"这一发能不能看见"由出站投影
/// (`project_content`) 按当次的模型决定。所以图片不再往正文里写那句"当前话题无法
/// 直接查看图片内容"——池子中途换了个收图的模型时，那句话就会开始说谎。
/// 音频/视频同理：认出来就记引用，正文里只写路径与大小；都不认的照旧读成正文（它本来就是文字）。
fn with_attachments(input: &str, paths: &[String]) -> PendingInput {
    let mut pending = PendingInput {
        content: input.to_string(),
        ..Default::default()
    };
    for path in paths {
        if let Some(mime) = image_mime_of(path) {
            let bytes = file_bytes(path);
            let dimensions = std::fs::read(path)
                .ok()
                .and_then(|raw| png_dimensions(&raw))
                .map(|(w, h)| format!("{w}×{h}"))
                .unwrap_or_else(|| "尺寸未知".into());
            pending.content.push_str(&format!(
                "\n\n附件：图片 {}（{mime}，{dimensions}，{} KB）。",
                path,
                bytes / 1024
            ));
            pending.images.push(ImageRef {
                path: path.clone(),
                mime: mime.to_string(),
                bytes,
            });
            continue;
        }
        if let Some(mime) = tools::audio_mime_of(std::path::Path::new(path)) {
            let bytes = file_bytes(path);
            pending.content.push_str(&format!(
                "\n\n附件：音频 {path}（{mime}，{} KB）。",
                bytes / 1024
            ));
            pending.audios.push(crate::session::entry::MediaRef {
                path: path.clone(),
                mime: mime.to_string(),
                bytes,
            });
            continue;
        }
        if let Some(mime) = tools::video_mime_of(std::path::Path::new(path)) {
            let bytes = file_bytes(path);
            pending.content.push_str(&format!(
                "\n\n附件：视频 {path}（{mime}，{} KB）。",
                bytes / 1024
            ));
            pending.videos.push(crate::session::entry::MediaRef {
                path: path.clone(),
                mime: mime.to_string(),
                bytes,
            });
            continue;
        }
        match tools::read_attachment(path) {
            Ok(attached) => pending.content.push_str(&format!(
                "\n\n附件 {}：\n```text\n{}\n```",
                attached["name"].as_str().unwrap_or(path),
                attached["text"].as_str().unwrap_or("")
            )),
            Err(error) => pending
                .content
                .push_str(&format!("\n\n附件 {path} 读取失败：{error}")),
        }
    }
    pending
}

fn file_bytes(path: &str) -> u64 {
    std::fs::metadata(path)
        .map(|meta| meta.len())
        .unwrap_or_default()
}

/// 扩展名 → MIME。粘贴落盘与附件注入共用这一张表
fn image_mime_of(path: &str) -> Option<&'static str> {
    let lower = path.to_ascii_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or("");
    match ext {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        _ => None,
    }
}

/// PNG 的 IHDR 在固定偏移：字节 16..20 宽、20..24 高（big-endian）。
/// 只为读尺寸不引入 image 全家桶——Win+Shift+S 粘出来的 99% 是 PNG，
/// 其余格式不报尺寸也不丢功能
fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    Some((width, height))
}

/// 字节魔数 → 扩展名。剪贴板数据的**真实格式以字节为准**——WebView 的
/// DataTransferItem 在 paste 事件同步段之后会失效成空串，前端带来的 mime
/// 只是参考，空了或对不上都不能信
fn sniff_image_ext(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("png");
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("jpg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("gif");
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("webp");
    }
    if bytes.starts_with(b"BM") {
        return Some("bmp");
    }
    None
}

fn mime_to_ext(mime: &str) -> Option<&'static str> {
    match mime {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        "image/bmp" => Some("bmp"),
        _ => None,
    }
}

/// Win+Shift+S 的截图没有文件路径，WebView 的 paste 事件里只有位图数据。
/// 这里把它落进临时目录——之后它就是一张普通的路径附件，链路上只有
/// [`with_attachments`] 需要认得「这是图，别当文本读」
#[tauri::command]
pub fn save_clipboard_image(data_base64: String, mime: String) -> Result<Value, String> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_base64.trim())
        .map_err(|e| format!("图片数据解码失败：{e}"))?;
    if bytes.is_empty() {
        return Err("图片数据是空的".into());
    }
    // 格式先看字节魔数（权威），mime 只在字节认不出时兜底参考
    let ext = sniff_image_ext(&bytes)
        .or_else(|| mime_to_ext(&mime))
        .ok_or_else(|| {
            format!(
                "认不出这张图的格式（mime={mime:?}，头 12 字节 {:02x?}）；Win+Shift+S 的截图通常是 PNG，重截一次再贴",
                &bytes[..bytes.len().min(12)]
            )
        })?;
    let resolved_mime = match ext {
        "png" => "image/png",
        "jpg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "image/bmp",
    };
    let dir = std::env::temp_dir().join("aglab").join("paste");
    std::fs::create_dir_all(&dir).map_err(|e| format!("建临时目录失败：{e}"))?;
    // 毫秒级时间戳：同一张图连贴两次不重名，同毫秒内也几乎不撞
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S%3f");
    let name = format!("paste-{stamp}.{ext}");
    let path = dir.join(&name);
    std::fs::write(&path, &bytes).map_err(|e| format!("写图片失败：{e}"))?;
    let (width, height) = match png_dimensions(&bytes) {
        Some((w, h)) => (json!(w), json!(h)),
        None => (Value::Null, Value::Null),
    };
    Ok(json!({
        "path": path.to_string_lossy(),
        "name": name,
        "mime": resolved_mime,
        "bytes": bytes.len(),
        "width": width,
        "height": height,
    }))
}

/// 抓取一个网页的可读正文。出站照过出口名单与全局代理；正文落盘成 txt，
/// 之后它就是一份普通的文本附件——chip、注入、压缩全都复用既有链路
#[tauri::command]
pub async fn fetch_url_text(app: tauri::AppHandle, url: String) -> Result<Value, String> {
    let trimmed = url.trim().to_string();
    if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
        return Err("只支持 http/https 链接".into());
    }
    let config = config::load(&app);
    // 与 decision.rs 同一条闸：名单是「有没有可能问都不该问」的那道门
    crate::egress::guard(&config.net_egress_allow, &trimmed)?;
    let leg = crate::proxy::take_global(&config, &trimmed)?;
    // ureq 是阻塞的，不能占 async 执行器的线程
    tauri::async_runtime::spawn_blocking(move || fetch_url_blocking(&trimmed, leg))
        .await
        .map_err(|error| format!("抓取线程异常：{error}"))?
}

/// 抓取正文上限：2MB 原文足够排出正文页；转出的文本再按附件同款口径截断
const FETCH_MAX_BYTES: usize = 2 * 1024 * 1024;
const FETCH_TEXT_LIMIT_CHARS: usize = 60_000;
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);

/// 抓一个公开网页并折成可读正文：出口名单之外的两个消费方（输入框的链接抓取、
/// 模型的 web_fetch 工具）共用的管线。返回（标题, 正文, 是否截断）
fn fetch_readable(
    url: &str,
    leg: &mut crate::proxy::Leg,
) -> Result<(Option<String>, String, bool), String> {
    let mut builder = ureq::Agent::config_builder().timeout_global(Some(FETCH_TIMEOUT));
    if let Some(proxy_url) = leg.proxy_url() {
        // 地址不合法是我们自己的毛病：话先抄下来，再收这一步的账
        let parsed = match ureq::Proxy::new(proxy_url) {
            Ok(parsed) => parsed,
            Err(error) => {
                let message = format!("代理地址「{proxy_url}」不合法：{error}");
                leg.finish(crate::proxy::Outcome::Neutral);
                return Err(message);
            }
        };
        builder = builder.proxy(Some(parsed));
    }
    let agent = builder.build().new_agent();
    let started = Instant::now();
    let mut response = agent
        .get(url)
        .header(
            "accept",
            "text/html,application/xhtml+xml,text/plain;q=0.9,*/*;q=0.5",
        )
        .header(
            "user-agent",
            "Mozilla/5.0 (compatible; aglab-link-reader/1.0)",
        )
        .call()
        .map_err(|error| {
            // 抓取这一发的归因照同一条尺：拿到状态码就是路通了
            leg.finish(crate::proxy::outcome_of(&error));
            match &error {
                ureq::Error::StatusCode(code) => format!("目标网页返回 HTTP {code}"),
                other => format!("抓取失败：{other}"),
            }
        })?;
    leg.note_head(started.elapsed());
    leg.finish(crate::proxy::Outcome::Reached);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if content_type.contains("image/")
        || content_type.contains("video/")
        || content_type.contains("octet-stream")
    {
        return Err(format!(
            "链接指向的是二进制内容（{content_type}），没有可读正文"
        ));
    }
    let mut body = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(FETCH_MAX_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|e| format!("读取网页失败：{e}"))?;
    let truncated_bytes = body.len() > FETCH_MAX_BYTES;
    if truncated_bytes {
        body.truncate(FETCH_MAX_BYTES);
    }
    let decoded = decode_html_body(&body, &content_type);
    let (title, text) = extract_readable_text(&decoded);
    let mut text = text;
    let mut truncated = truncated_bytes;
    if text.chars().count() > FETCH_TEXT_LIMIT_CHARS {
        text = text.chars().take(FETCH_TEXT_LIMIT_CHARS).collect();
        truncated = true;
    }
    if text.trim().is_empty() {
        return Err("这个页面没有提取出可读正文（可能是纯脚本渲染或非 HTML 内容）".into());
    }
    Ok((title, text, truncated))
}

/// 模型的 web_fetch：同一条抓取管线，正文直接作为工具结果交回——不落盘附件，
/// 那是输入框链路的事。两道闸的顺序不能换：先出口名单（「有没有可能问都不该问」），
/// 再 SSRF（「不许探内网」）。这个函数住在执行循环的同一条线程上，阻塞 20 秒封顶，
/// 与 run_command 的 60 秒同一个量级
fn web_fetch_for_model(config: &AppConfig, args: &Value) -> Result<String, String> {
    let url = args["url"].as_str().unwrap_or_default().trim().to_string();
    if url.is_empty() {
        return Err("web_fetch 缺少 url 参数。".into());
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("只支持 http/https 链接".into());
    }
    crate::egress::guard(&config.net_egress_allow, &url)?;
    crate::egress::refuse_private_target(&url)?;
    let mut leg = crate::proxy::take_global(config, &url)?;
    let (title, text, truncated) = fetch_readable(&url, &mut leg)?;
    let mut out = String::new();
    if let Some(title) = &title {
        out.push_str(&format!("标题：{title}\n\n"));
    }
    out.push_str(&text);
    if truncated {
        out.push_str("\n…（内容超长已截断）");
    }
    Ok(out)
}

/// 自动审查的裁定。approved + 一句理由（拒绝时给模型看）
struct AutoReviewVerdict {
    approved: bool,
    reason: String,
}

/// 审查那一发走哪套连接：点名了审查档案就整体换上它的连接域（base_url、凭据、
/// 代理一起走），点名了模型就再盖那一行、读数跟着换——与子助理点名同一条路。
/// 档案被删了就原样回落：审查跟着当前连接走，而不是审查不了
fn reviewer_connection(config: &AppConfig) -> AppConfig {
    let mut reviewer = config.clone();
    if let Some(profile) = config
        .profiles
        .iter()
        .find(|profile| profile.id == config.auto_review_profile_id)
    {
        crate::config::apply_profile_connection(&mut reviewer, profile);
        // 档案连接抄完先盖一次模型行（与档案切换同一档）：档案默认模型的那一行
        // 可能自带思考档/窗口读数，漏盖就是审查链与聊天链各说各话
        crate::config::apply_model_spec(&mut reviewer);
    }
    if !config.auto_review_model.is_empty() {
        reviewer.model = config.auto_review_model.clone();
        crate::config::apply_model_spec(&mut reviewer);
    }
    reviewer
}

/// 审查模型替人拍板：一次完整 LLM 调用，看工具名 + 入参 + 风险档位，
/// 回 APPROVE 或 DENY: 理由。审查失败 = 拒（fail-closed）——模型是安全闸不是便利闸。
/// 使用 `complete_once`（一次性调用，无工具、无日志），与标题生成同一条路
fn auto_review_verdict(
    config_dir: &std::path::Path,
    config: &AppConfig,
    tool_name: &str,
    input: &str,
    risk: &str,
) -> AutoReviewVerdict {
    const SYSTEM: &str = "你是一个安全审查员。以下是 agent 请求执行的工具调用。\
        评估它是否安全：读操作通常安全；写操作看目标是否合理；命令操作看是否有破坏性或越权。\
        回复格式（严格一行）：\n\
        APPROVE\n或\nDENY: 一句拒绝理由\n\
        不要输出其他任何内容。有疑虑时选 DENY。";
    let user = format!("工具：{tool_name}\n风险：{risk}\n操作内容：{input}");

    let messages = json!([
        { "role": "system", "content": SYSTEM },
        { "role": "user", "content": user },
    ]);

    let reviewer = reviewer_connection(config);
    match complete_once_in(config_dir, &reviewer, messages, "auto_review") {
        Ok(text) => {
            let trimmed = text.trim();
            if trimmed.starts_with("APPROVE") {
                AutoReviewVerdict {
                    approved: true,
                    reason: "审查通过".into(),
                }
            } else if let Some(reason) = trimmed.strip_prefix("DENY:") {
                AutoReviewVerdict {
                    approved: false,
                    reason: reason.trim().to_string(),
                }
            } else if trimmed.starts_with("DENY") {
                AutoReviewVerdict {
                    approved: false,
                    reason: trimmed.to_string(),
                }
            } else {
                AutoReviewVerdict {
                    approved: false,
                    reason: format!(
                        "审查模型回复了无法理解的格式：{}",
                        trimmed.chars().take(80).collect::<String>()
                    ),
                }
            }
        }
        Err(error) => AutoReviewVerdict {
            approved: false,
            reason: format!("审查模型调用失败（fail-closed）：{error}"),
        },
    }
}

/// 子助理控制（list/send/interrupt）。list 扫话题档案里 spawn- 前缀的子助理，
/// 运行状态以停止登记表为准；send/interrupt 动的是**别的 agent 的回合**，
/// 走的是与界面同一套插话/停止闸（SteeringHub/StopHub）——模型没有特权通道
/// 计划任务 / 规划模式 / 等待子助理 / 记忆与历史检索的执行体。
/// 全部是既有子系统的薄包装——包装层只做取参与排版，不改判据
#[allow(dead_code)] // M3.5：执行点随重活三件定去留（O2-1）
pub(crate) fn subsystem_tool_exec(
    app: &AppHandle,
    config: &AppConfig,
    conversation_id: &str,
    name: &str,
    args: &Value,
    stop: &std::sync::atomic::AtomicBool,
) -> Result<String, String> {
    use tauri::Manager;
    match name {
        "memory_search" => {
            let query = args["query"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_string();
            if query.is_empty() {
                return Err("memory_search 缺少 query。".into());
            }
            let hits = crate::memory::memory_search(app.clone(), query.clone())?;
            if hits.is_empty() {
                return Ok(format!("记忆里没有关于「{query}」的条目。"));
            }
            let lines: Vec<String> = hits
                .iter()
                .map(|hit| {
                    format!(
                        "- [{}] {}（{}）\n  {}",
                        hit.status, hit.path, hit.scope, hit.content
                    )
                })
                .collect();
            Ok(format!(
                "记忆检索「{query}」{} 条：\n{}",
                lines.len(),
                lines.join("\n")
            ))
        }
        "memory_timeline" => {
            let limit = args["limit"].as_u64().unwrap_or(30).clamp(1, 200) as usize;
            let rows = crate::memory::memory_timeline(app.clone(), Some(limit))?;
            if rows.is_empty() {
                return Ok("记忆还没有动态。".into());
            }
            let lines: Vec<String> = rows
                .iter()
                .map(|row| format!("- {}", serde_json::to_value(row).unwrap_or(Value::Null)))
                .collect();
            Ok(lines.join("\n"))
        }
        "search_history" => {
            let query = args["query"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_string();
            if query.is_empty() {
                return Err("search_history 缺少 query。".into());
            }
            let limit = args["limit"].as_u64().unwrap_or(20).clamp(1, 50) as usize;
            let hits = crate::search::session_search(app.clone(), query.clone(), Some(limit))?;
            if hits.is_empty() {
                return Ok(format!("历史话题里没有「{query}」。"));
            }
            let lines: Vec<String> = hits
                .iter()
                .map(|hit| {
                    format!(
                        "- [{}] {} · {}：{}",
                        hit.role, hit.title, hit.message_id, hit.snippet
                    )
                })
                .collect();
            Ok(format!(
                "历史检索「{query}」{} 条：\n{}",
                lines.len(),
                lines.join("\n")
            ))
        }
        "cron_list" => {
            if config.tasks.is_empty() {
                return Ok("还没有定时任务。用 cron_create 建一条。".into());
            }
            let lines: Vec<String> = config
                .tasks
                .iter()
                .map(|task| {
                    let trigger = if task.cron_expr.is_empty() {
                        match task.kind.as_str() {
                            "interval" => format!("每 {} 分钟", task.every_minutes),
                            "daily" => format!(
                                "每天 {:02}:{:02}",
                                task.at_minute / 60,
                                task.at_minute % 60
                            ),
                            _ => format!("kind={} at={}", task.kind, task.at_minute),
                        }
                    } else {
                        format!("cron「{}」", task.cron_expr)
                    };
                    format!(
                        "- {} · {} · {}{}",
                        task.id,
                        task.name,
                        trigger,
                        if task.enabled { "" } else { "（停用）" }
                    )
                })
                .collect();
            Ok(format!(
                "定时任务 {} 条：\n{}",
                lines.len(),
                lines.join("\n")
            ))
        }
        "cron_create" => {
            let name = args["name"].as_str().unwrap_or_default().trim().to_string();
            let prompt = args["prompt"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_string();
            let cron_expr = args["cron_expr"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_string();
            if name.is_empty() || prompt.is_empty() || cron_expr.is_empty() {
                return Err("cron_create 需要 name / prompt / cron_expr 三样都非空。".into());
            }
            let mut tasks = config.tasks.clone();
            let id = format!(
                "task-{:x}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            );
            tasks.push(crate::config::ScheduledTask {
                id: id.clone(),
                name,
                prompt,
                kind: "cron".into(),
                cron_expr,
                enabled: true,
                created_at: crate::session::now_millis(),
                ..Default::default()
            });
            // 走 config_patch 的统一入口：坏表达式与非法图会被那里的闸拒在门外
            crate::config::config_patch(app.clone(), serde_json::json!({ "tasks": tasks }))?;
            Ok(format!(
                "定时任务已创建：{id}。到点它会在自己的话题里无人值守执行（审批走待批队列）。"
            ))
        }
        "cron_delete" => {
            let id = args["id"].as_str().unwrap_or_default().trim().to_string();
            if id.is_empty() {
                return Err("cron_delete 缺少 id。".into());
            }
            let kept: Vec<_> = config
                .tasks
                .iter()
                .filter(|task| task.id != id)
                .cloned()
                .collect();
            if kept.len() == config.tasks.len() {
                return Err(format!("没有 id 为「{id}」的定时任务（cron_list 里找）。"));
            }
            crate::config::config_patch(app.clone(), serde_json::json!({ "tasks": kept }))?;
            Ok(format!("已删除定时任务 {id}。"))
        }
        "task_run_now" => {
            let id = args["id"].as_str().unwrap_or_default().trim().to_string();
            if id.is_empty() {
                return Err("task_run_now 缺少 id。".into());
            }
            crate::tasks::tasks_run(app.clone(), id.clone())?;
            Ok(format!(
                "已触发定时任务 {id}，它在自己的话题里跑，跑完看任务页的运行记录。"
            ))
        }
        "plan_mode" => {
            let action = args["action"].as_str().unwrap_or_default();
            let mode = match action {
                "enter" => "plan",
                "exit" => "chat",
                other => {
                    return Err(format!(
                        "plan_mode 的 action 只认 enter / exit，收到「{other}」。"
                    ))
                }
            };
            // 与 session_mode_set 同一条路：校验当场做，回合在跑就寄存到轮次边界
            let mut session = open_session(app, conversation_id)?;
            let held = crate::session::mode::in_effect(&session.log);
            let next = mode_state_from(mode, &held)?;
            let stop_hub = app.state::<StopHub>();
            let mode_hub = app.state::<ModeHub>();
            if stop_hub.is_running(conversation_id) {
                mode_hub
                    .inner()
                    .set(conversation_id, PendingMode::Switch { mode: mode.into() });
                return Ok(format!(
                    "回合还在跑：{} 的请求已寄存，这一轮收尾时落行生效。",
                    if action == "enter" {
                        "规划模式"
                    } else {
                        "对话模式"
                    }
                ));
            }
            session
                .log_mut()
                .append(
                    NewEntry::new(crate::session::mode::row(&next)),
                    crate::session::now_millis(),
                )
                .map_err(|error| error.to_string())?;
            session.save()?;
            Ok(match action {
                "enter" => "已进入规划模式：会动东西的调用一律被拒，查资料写方案不受影响。完成后用 plan_mode exit 退出。",
                _ => "已退回对话模式，执行恢复按权限档走。",
            }
            .to_string())
        }
        "wait_agent" => {
            let agent_id = args["agent_id"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_string();
            if !agent_id.starts_with("spawn-") {
                return Err(
                    "wait_agent 需要 agent_id（agent_control list 里给的子助理话题 id）。".into(),
                );
            }
            let seconds = args["timeout_seconds"].as_u64().unwrap_or(60).clamp(1, 600);
            let stop_hub = app.state::<StopHub>();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
            while stop_hub.is_running(&agent_id) {
                if stopped(stop) {
                    return Err(
                        "已按你的要求停止等待（子助理还在跑，可以稍后再 wait_agent）。".into(),
                    );
                }
                if std::time::Instant::now() > deadline {
                    return Ok(format!(
                        "子助理 {agent_id} 等了 {seconds}s 还在运行。可以再 wait_agent 一次，\
                         或先去干别的——它收尾后结果在 agent_control list 里可见。"
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            Ok(format!("子助理 {agent_id} 已收尾。"))
        }
        _ => Err(format!("subsystem_tool_exec 不认识「{name}」。")),
    }
}

#[allow(dead_code)] // M3.5：同上，agent_control 的执行体待接（O2-1）
pub(crate) fn agent_control_exec(
    app: &AppHandle,
    action: &str,
    agent_id: &str,
    message: &str,
) -> Result<String, String> {
    use tauri::Manager;
    match action {
        "list" => {
            let conversations = crate::history::load_all_for_search(app)?;
            let stop_hub = app.state::<StopHub>();
            let mut lines: Vec<String> = Vec::new();
            let mut count = 0usize;
            for conversation in &conversations {
                if !conversation.id.starts_with("spawn-") {
                    continue;
                }
                count += 1;
                let running = stop_hub.is_running(&conversation.id);
                lines.push(format!(
                    "- {} · {} · {}（{} 条消息）",
                    conversation.id,
                    conversation.title,
                    if running { "运行中" } else { "已收尾" },
                    conversation.messages.len()
                ));
            }
            if count == 0 {
                return Ok("还没有派生过子助理。用 spawn_subagent 派一个。".to_string());
            }
            Ok(format!(
                "子助理 {} 个：
{}",
                count,
                lines.join(
                    "
"
                )
            ))
        }
        "send" => {
            if agent_id.is_empty() || !agent_id.starts_with("spawn-") {
                return Err("send 需要 agent_id（list 结果里给的子助理话题 id）。".into());
            }
            if message.is_empty() {
                return Err("send 的 message 不能为空。".into());
            }
            let stop_hub = app.state::<StopHub>();
            if !stop_hub.is_running(agent_id) {
                return Err(format!(
                    "子助理 {agent_id} 不在运行中：已收尾的话题不能插话，要继续就重新派一个并附上上下文。"
                ));
            }
            let steering = app.state::<SteeringHub>();
            steering.push(agent_id, message)?;
            Ok(format!(
                "已向子助理 {agent_id} 插话，它会在下一个轮间边界看到。"
            ))
        }
        "interrupt" => {
            if agent_id.is_empty() || !agent_id.starts_with("spawn-") {
                return Err("interrupt 需要 agent_id（list 结果里给的子助理话题 id）。".into());
            }
            let stop_hub = app.state::<StopHub>();
            stop_hub.abort(agent_id)?;
            Ok(format!(
                "已向子助理 {agent_id} 发出中断，它的结果会以半截形式收尾。"
            ))
        }
        other => Err(format!(
            "不认识的 agent_control 动作「{other}」：list / send / interrupt 选一个。"
        )),
    }
}

/// 模型的联网搜索。与 web_fetch 同一条安全链：先出口名单，请求经代理池走。
/// 供应商两家：Tavily（REST，要 key）与 SearXNG（实例的 JSON 接口，免费无需 key）。
/// key 在配置里（`WebSearchConfig`），没配时工具整条不声明，
/// 走到这里还配着就只会是配置刚被清掉——照实回错，不装作搜过
fn web_search_for_model(config: &AppConfig, args: &Value) -> Result<String, String> {
    let query = args["query"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string();
    if query.is_empty() {
        return Err("web_search 缺少 query 参数。".into());
    }
    if !config.web_search.enabled() {
        return Err("联网搜索没有配置（搜索服务或凭据/实例地址缺失），无法执行。".into());
    }
    let max_results = config.web_search.max_results.clamp(1, 10);
    let value = match config.web_search.provider.as_str() {
        "searxng" => search_searxng(config, &query, max_results)?,
        _ => search_tavily(config, &query, max_results)?,
    };

    let results = value["results"].as_array().cloned().unwrap_or_default();
    if results.is_empty() {
        return Ok(format!("没有搜到相关结果（query={query}）。换个更具体的搜索词，或改用 web_fetch 直接读已知地址。"));
    }
    let mut out = String::new();
    for (index, item) in results.iter().enumerate() {
        let title = item["title"].as_str().unwrap_or("（无标题）");
        let link = item["url"].as_str().unwrap_or_default();
        let content = item["content"].as_str().unwrap_or_default();
        // 摘要是给"决定要不要读全文"用的：400 字足够判断，再多就是往上下文里灌网页
        let snippet: String = content.chars().take(400).collect();
        out.push_str(&format!(
            "{}. {title}\n   {link}\n   {snippet}\n\n",
            index + 1
        ));
    }
    Ok(out)
}

/// 带超时与代理池的一发搜索请求。两家供应商共用：leg 的归因（到没到、经没经代理）
/// 在这里一次记账，调用方只管收响应体
fn search_request(
    config: &AppConfig,
    url: &str,
    request: impl FnOnce(ureq::Agent) -> Result<ureq::http::Response<ureq::Body>, ureq::Error>,
) -> Result<Value, String> {
    crate::egress::guard(&config.net_egress_allow, url)?;
    let mut leg = crate::proxy::take_global(config, url)?;
    let mut builder = ureq::Agent::config_builder().timeout_global(Some(FETCH_TIMEOUT));
    if let Some(proxy_url) = leg.proxy_url().map(str::to_string) {
        // 先落成 owned：leg 的归因要在闭包里记账，借用不能跨过它
        let parsed = ureq::Proxy::new(&proxy_url).map_err(|error| {
            let message = format!("代理地址「{proxy_url}」不合法：{error}");
            leg.finish(crate::proxy::Outcome::Neutral);
            message
        })?;
        builder = builder.proxy(Some(parsed));
    }
    let agent = builder.build().new_agent();
    let mut response = request(agent).map_err(|error| {
        leg.finish(crate::proxy::outcome_of(&error));
        match &error {
            ureq::Error::StatusCode(code) => format!("搜索接口返回 HTTP {code}"),
            other => format!("搜索失败：{other}"),
        }
    })?;
    leg.finish(crate::proxy::Outcome::Reached);
    let mut body = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(FETCH_MAX_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|e| format!("读取搜索结果失败：{e}"))?;
    serde_json::from_slice(&body).map_err(|e| format!("搜索结果不是合法 JSON：{e}"))
}

fn search_tavily(config: &AppConfig, query: &str, max_results: u32) -> Result<Value, String> {
    let url = "https://api.tavily.com/search";
    // SSRF 闸照常：tavily 是公共端点，没有"自建在私网"这一说
    crate::egress::refuse_private_target(url)?;
    search_request(config, url, |agent| {
        agent
            .post(url)
            .header("content-type", "application/json")
            .send_json(json!({
                "api_key": config.web_search.api_key,
                "query": query,
                "max_results": max_results,
                "search_depth": "basic",
                "include_answer": false,
            }))
    })
}

/// SearXNG 实例的 JSON 搜索（免费，无需 key）。实例地址是**用户配置的可信端点**
/// （与模型 baseUrl 同级）：模型只给查询词，动不了发到哪台实例——所以私网/回环的
/// 自建实例放行，SSRF 闸（refuse_private_target）对它不适用；出口名单照常在
fn search_searxng(config: &AppConfig, query: &str, max_results: u32) -> Result<Value, String> {
    let base = config
        .web_search
        .searxng_url
        .trim()
        .trim_end_matches('/')
        .to_string();
    crate::egress::require_http_url(&base, "SearXNG 实例地址")?;
    let url = format!("{base}/search");
    crate::egress::guard(&config.net_egress_allow, &url)?;
    let api_key = config.web_search.api_key.trim().to_string();
    let mut response = search_request(config, &url, |agent| {
        // ureq 的 query() 负责百分号编码：查询词里有什么都不改变请求的 host
        let mut request = agent
            .get(&url)
            .query("q", query)
            .query("format", "json")
            .query("safesearch", "1")
            .header("accept", "application/json");
        if !api_key.is_empty() {
            // 实例若开了反代鉴权（basic/Bearer），配置里的 key 就作为令牌带上；留空不发
            request = request.header("authorization", &format!("Bearer {api_key}"));
        }
        // 无 body 的 GET 用 call()（send_* 是带 body 那一侧的方法）
        request.call()
    })
    .map_err(|problem| {
        // 公共实例最常见的翻车：没开 JSON 输出（settings.yml: search.formats）。
        // 报错要把这一句带到人眼前，而不是让用户对着一个 403 猜
        if problem.contains("403") {
            format!(
                "{problem}——多数是实例没开启 JSON 输出（settings.yml 的 search.formats 需要 json），或实例要鉴权。换一个实例或在实例设置里开启。"
            )
        } else {
            problem
        }
    })?;
    // SearXNG 回的 results 数组与 Tavily 同形；超过条数的在此截断
    if let Some(results) = response["results"].as_array_mut() {
        results.truncate(max_results as usize);
    }
    Ok(response)
}

fn fetch_url_blocking(url: &str, mut leg: crate::proxy::Leg) -> Result<Value, String> {
    let (title, text, truncated) = fetch_readable(url, &mut leg)?;

    // 落盘：文件名带 host 与内容指纹，同名重抓不互相覆盖
    let dir = std::env::temp_dir().join("aglab").join("fetch");
    std::fs::create_dir_all(&dir).map_err(|e| format!("建临时目录失败：{e}"))?;
    let digest = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(url.as_bytes());
        format!("{:x}", hasher.finalize())[..8].to_string()
    };
    let host = crate::egress::host_of(url);
    let slug: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let name = format!("{slug}-{digest}.txt");
    let path = dir.join(&name);
    let document = format!(
        "来源：{url}\n标题：{}\n抓取时间：{}\n\n{}",
        title.as_deref().unwrap_or("（无标题）"),
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        text
    );
    std::fs::write(&path, document).map_err(|e| format!("写正文文件失败：{e}"))?;
    Ok(json!({
        "path": path.to_string_lossy(),
        "name": name,
        "url": url,
        "title": title,
        "chars": text.chars().count(),
        "truncated": truncated,
    }))
}

/// 把 HTML 折成可读文本。手写轻量版，不为去标签引 html5ever 全家桶：
/// 去 script/style/注释 → 块级标签换行 → 剥标签 → 解常见实体 → 折空白。
/// 排版不需要完美——模型要的是正文，不是 DOM
/// 网页字节 → 文本。UTF-8 优先（现代网站的绝大多数，快速路径）；
/// 解不开再按声明认：先 Content-Type 头里的 charset，再 HTML meta 声明
/// （GBK/GB2312 字节里 meta 标签本身是 ASCII，扫头部 2KB 安全）；
/// 什么都没声明就 UTF-8 与 GB18030 各解一遍、谁的替换字符少信谁——
/// 老中文站最常见的形态就是 GBK 且哪里都不写 charset。encoding_rs 的
/// GB18030 是 GBK/GB2312 的超集，顺手把它们一起接住。
/// 声明了却解出错的（拿 UTF-8 的尺子量 GBK 之类）不用那份，落到兜底
fn decode_html_body(bytes: &[u8], content_type: &str) -> String {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_string();
    }
    let lossy_replacements = String::from_utf8_lossy(bytes).matches('\u{FFFD}').count();
    let fallback = |label: &str| -> Option<String> {
        encoding_rs::Encoding::for_label(label.as_bytes())
            .map(|encoding| {
                let (text, _, had_errors) = encoding.decode(bytes);
                let text = text.into_owned();
                if had_errors {
                    None
                } else {
                    Some(text)
                }
            })
            .unwrap_or(None)
            .filter(|text| text.matches('\u{FFFD}').count() <= lossy_replacements)
    };
    if let Some(label) = charset_from_content_type(content_type) {
        if let Some(text) = fallback(&label) {
            return text;
        }
    }
    if let Some(label) = sniff_meta_charset(bytes) {
        if let Some(text) = fallback(&label) {
            return text;
        }
    }
    fallback("gb18030").unwrap_or_else(|| String::from_utf8_lossy(bytes).into_owned())
}

/// Content-Type 里的 charset 参数：`text/html; charset=gbk` → `gbk`
fn charset_from_content_type(content_type: &str) -> Option<String> {
    content_type
        .split(';')
        .map(str::trim)
        .find_map(|part| part.strip_prefix("charset="))
        .map(|value| {
            value
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string()
        })
}

/// 头部 2KB 里的 meta 声明。meta 标签与属性名是 ASCII，即便字节是 GBK/Shift-JIS
/// 也能安全按 lossy 读出标签结构再抠 charset 值
fn sniff_meta_charset(bytes: &[u8]) -> Option<String> {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(2048)]).to_ascii_lowercase();
    let at = head.find("charset=")?;
    let rest = &head[at + "charset=".len()..];
    let value: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn extract_readable_text(html: &str) -> (Option<String>, String) {
    let mut working = html.to_string();

    let title = {
        let lower = working.to_lowercase();
        let start = lower
            .find("<title")
            .and_then(|s| lower[s..].find('>').map(|e| s + e + 1));
        let end = start.and_then(|s| lower[s..].find("</title").map(|e| s + e));
        match (start, end) {
            (Some(s), Some(e)) if e > s => {
                let decoded = decode_entities(&working[s..e]);
                let cleaned = decoded.split_whitespace().collect::<Vec<_>>().join(" ");
                if cleaned.is_empty() {
                    None
                } else {
                    Some(cleaned)
                }
            }
            _ => None,
        }
    };

    // script/style/注释整段剔除（不区分大小写地找开闭标签）
    for (open, close) in [
        ("<script", "</script>"),
        ("<style", "</style>"),
        ("<noscript", "</noscript>"),
        ("<!--", "-->"),
    ] {
        while let Some(start) = working.to_lowercase().find(open) {
            let Some(relative) = working[start..].to_lowercase().find(close) else {
                working.truncate(start);
                break;
            };
            let end = start + relative + close.len();
            working.replace_range(start..end, "");
        }
    }

    // 块级标签与 <br> 换行：正文段落边界是最有价值的结构信息
    for tag in [
        "<br",
        "</p>",
        "</div>",
        "</li>",
        "</tr>",
        "</h1>",
        "</h2>",
        "</h3>",
        "</h4>",
        "</h5>",
        "</h6>",
        "</section>",
        "</article>",
        "</blockquote>",
    ] {
        working = working.replace(tag, &format!("{tag}\n"));
    }

    // 剥掉全部标签
    let mut text = String::with_capacity(working.len());
    let mut in_tag = false;
    for ch in working.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => text.push(c),
            _ => {}
        }
    }
    let decoded = decode_entities(&text);
    // 空白折叠：行内连续空白并成一个空格；3 连以上换行并成一段空行
    let mut folded = String::with_capacity(decoded.len());
    let mut spaces = 0usize;
    let mut newlines = 0usize;
    for ch in decoded.chars() {
        match ch {
            ' ' | '\t' | '\r' => {
                spaces += 1;
                if spaces == 1 && newlines == 0 {
                    folded.push(' ');
                }
            }
            '\n' => {
                newlines += 1;
                spaces = 0;
                if newlines <= 2 {
                    folded.push('\n');
                }
            }
            c => {
                spaces = 0;
                newlines = 0;
                folded.push(c);
            }
        }
    }
    (title, folded.trim().to_string())
}

/// 最常见的 HTML 实体。数字实体一并处理（&#39; &#x27;），其余不认识的原文保留
fn decode_entities(input: &str) -> String {
    if !input.contains('&') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(position) = rest.find('&') {
        out.push_str(&rest[..position]);
        let tail = &rest[position..];
        let semicolon = match tail.find(';') {
            Some(index) if index <= 10 => index,
            _ => {
                out.push('&');
                rest = &tail[1..];
                continue;
            }
        };
        let entity = &tail[1..semicolon];
        let replacement = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ => entity
                .strip_prefix('#')
                .and_then(|digits| {
                    if let Some(hex) = digits
                        .strip_prefix('x')
                        .or_else(|| digits.strip_prefix('X'))
                    {
                        u32::from_str_radix(hex, 16).ok()
                    } else {
                        digits.parse::<u32>().ok()
                    }
                })
                .and_then(char::from_u32),
        };
        match replacement {
            Some(ch) => {
                out.push(ch);
                rest = &tail[semicolon + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// 保留最近一段原文的长度（中文口径的字符数）
const KEEP_RECENT_CHARS: usize = 20_000;

/// 选压缩边界：从尾部回累计正文字符数，返回"保留窗起点的条目 id + 保留几行"。
///
/// 行号必须翻回条目 id：投影行和条目**不是一一对应**的（一条带段快照的压缩条目占两行），
/// 所以 `origin` 与 `history` 必须逐行等长。对不齐、或者边界落在一条条目中间，都宁可不压——
/// 压错地方省不下钱，只会把还没用的历史换掉
fn compaction_boundary(
    history: &[Value],
    origin: &[String],
    keep_chars: usize,
) -> Option<(String, usize)> {
    if origin.len() != history.len() {
        return None;
    }
    let mut keep_from = history.len();
    let mut acc = 0usize;
    while keep_from > 0 {
        let content = crate::session::entry::content_chars(&history[keep_from - 1]);
        if acc + content > keep_chars && keep_from < history.len() {
            break;
        }
        acc += content;
        keep_from -= 1;
    }
    // 起点只能落在条目边界上：停在一条条目中间，返回的那个 id 会说"这条整体保留"，
    // 而它其实把这条自己的头一行切掉了——"保留几行"和实发的那批行从此不是同一批。
    // 往前退是安全的方向：多留一行，不是多压一行
    // 起点只能落在条目边界上：停在一条条目中间，返回的那个 id 会说"这条整体保留"，
    // 而它其实把这条自己的头一行切掉了——"保留几行"和实发的那批行从此不是同一批。
    // 往前退是安全的方向：多留一行，不是多压一行
    if keep_from < history.len() {
        while keep_from > 0 && origin[keep_from - 1] == origin[keep_from] {
            keep_from -= 1;
        }
    }
    // 至少留 2 行、至少压掉 2 行，不然这次压缩什么都没省
    if keep_from < 2 || history.len() - keep_from < 2 {
        return None;
    }
    Some((origin.get(keep_from)?.clone(), history.len() - keep_from))
}

/// Main 侧的 worker 回合登记表（M3 收官）：话题 → 回合在子进程里跑。
/// 审批决议（tool_decision）、插话（chat_steer）、停止（chat_abort）靠它分流；
/// 值里的 () 只是键位——路由细节由监督者的 pending 表与请求 id 管
static WORKER_TURNS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::OnceLock::new();

fn worker_turns() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    WORKER_TURNS.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// 这条话题的当前回合是不是跑在 Agent 子进程里
pub(crate) fn worker_turn_active(conversation_id: &str) -> bool {
    worker_turns()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(conversation_id)
}

/// 现在有没有任何 worker 回合在跑（审批决议的"送子进程一票"以此为闸）
pub(crate) fn any_worker_turn() -> bool {
    !worker_turns()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_empty()
}

/// 按话题登记的停止开关。chat_send 注册、chat_abort 拉闸：
/// 流式读取逐行检查、工具循环逐轮逐工具检查——同步 IO 里这是响应最快的几处。
/// 内部是 Arc：chat_send 要把整张表 clone 进工作线程做清理
#[derive(Clone, Default)]
pub struct StopHub {
    flags: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<String, std::sync::Arc<std::sync::atomic::AtomicBool>>,
        >,
    >,
}

impl StopHub {
    /// 登记这一话题的停止开关。表里已有旗标（还有一轮没收尾）就报错而不是覆盖：
    /// 先前那轮手里还拿着旧旗标在跑，覆盖等于把它的停止开关整个换掉——用户按
    /// 停止拉的是新旗标，旧那轮从此对停止永久失联。chat_send / 目标续跑两个入口
    /// 都先查过 `is_running`，这里把"查"与"占"并成一步，中间不再有窗口
    pub(crate) fn register(
        &self,
        conversation_id: &str,
    ) -> Result<std::sync::Arc<std::sync::atomic::AtomicBool>, String> {
        let mut table = self
            .flags
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if table.contains_key(conversation_id) {
            return Err("这一话题还有一轮没收尾，停止开关拒绝重复登记".into());
        }
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        table.insert(conversation_id.to_string(), flag.clone());
        Ok(flag)
    }

    pub(crate) fn release(&self, conversation_id: &str) {
        self.flags
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(conversation_id);
    }

    /// 拉起某一发的闸。登记表上没有这一发就报错——返回 `Ok(())` 等于对着一发
    /// 早就不跑的回合说"照办了"，而界面上刚因此多等一段根本没有在跑的流
    pub(crate) fn abort(&self, conversation_id: &str) -> Result<(), String> {
        match self
            .flags
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(conversation_id)
        {
            Some(flag) => {
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
            None => Err("这条话题现在没有正在跑的回合，没什么可以停".to_string()),
        }
    }

    /// 这一条话题现在有没有正在跑的回合。切模式那一条命令用它把这扇窗关上：不是
    /// 因为切了会出什么事，而是那一轮的日志副本收尾时会把这次的写入整片盖掉
    pub(crate) fn is_running(&self, conversation_id: &str) -> bool {
        self.flags
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(conversation_id)
    }
}

/// 用户按下"停止生成"。只把开关拉起来，真正停下来的是 run_turn 的各检查点
/// （SSE 每行之前、工具循环每一轮之前）。没登记就报错，理由见 [`StopHub::abort`]
#[tauri::command]
pub fn chat_abort(
    app: AppHandle,
    state: tauri::State<'_, StopHub>,
    conversation_id: String,
) -> Result<(), String> {
    // worker 回合：拉闸走协议（旗标在子进程的 StopHub 上，主进程这面拉不着）
    if worker_turn_active(&conversation_id) {
        return crate::agent_supervisor::global(&app)
            .request(
                crate::agent_protocol::methods::TURN_STOP,
                serde_json::json!({ "conversationId": conversation_id }),
            )
            .map(|_| ());
    }
    state.abort(&conversation_id)
}

/// 按话题登记的**暂停**旗标。它存在的原因与"切模式在回合中要拒"是同一条：
/// 回合正在跑时往日志另写一行，会被那一轮收尾保存的副本整片盖掉。所以"暂停"在
/// 回合中只能先立旗——回合收尾的续跑判据（`goal_after_round`）读到它，替它把
/// "已暂停"那一行落进日志，然后不再接下一轮。旗子是**取走即清**的：读它的
/// 那一处就是消费它的那一处，迟到的旗子不会在几轮之后突然生效。
///
/// 残留的旗子由回合线程收尾时兜底清掉（错误路径不走收尾判据）。
#[derive(Clone, Default)]
pub struct PauseHub {
    flags: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

impl PauseHub {
    fn set(&self, conversation_id: &str) {
        self.flags
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(conversation_id.to_string());
    }

    /// 取走旗子：原来立着返回 true，并当场撤下
    fn take(&self, conversation_id: &str) -> bool {
        self.flags
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(conversation_id)
    }

    fn clear(&self, conversation_id: &str) {
        self.take(conversation_id);
    }
}

/// 按话题登记的目标护栏计数器（空转、工具连败，design-goal-mode.md §4.2）。
/// **住在内存里，跨重启不保留**——重启清零是接受的取舍：重启本来就停了所有回合，
/// 再跑出的空转从零数起，不会因为丢了一份旧计数就多烧钱。
/// 机器只吃现值吐新值（`Effect::RememberGuard`），这里只管存取，不做任何判断
#[derive(Clone, Default)]
pub struct GoalGuards {
    guards: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, crate::goal::Guard>>>,
}

impl GoalGuards {
    fn get(&self, conversation_id: &str) -> crate::goal::Guard {
        self.guards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(conversation_id)
            .copied()
            .unwrap_or_default()
    }

    fn remember(&self, conversation_id: &str, guard: crate::goal::Guard) {
        self.guards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(conversation_id.to_string(), guard);
    }
}

/// 回合正在跑时按下的**切档 / 结束目标**请求，按话题寄存一格。它存在的原因与暂停旗同源：
/// 那一轮的日志副本开在身上，这时往日志另写一行会被它收尾时整片盖掉。所以回合中只立旗，
/// 由 [`apply_pending_mode`] 在收尾判据**之前**替它把那一行落进当轮的副本。
///
/// 这一格不是方便而是必需：目标的续跑循环横跨一轮又一轮，期间 `StopHub` 恒说"在跑"。
/// 旧的"回合中拒绝切档"于是等于**一个正在推进的目标永远切不动档**——那正是
/// "切去聊两句就非得先把目标停下"这个抱怨的真身。判据那边早就改成认目标不认交互档了，
/// 门却还守着，用户根本走不到那一步
#[derive(Clone, Default)]
pub struct ModeHub {
    pending: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, PendingMode>>>,
}

/// 待落的那一次改写。`Discard` 是"清掉目标"（目标档下顺带回对话档，交互档不变）——
/// 它落的也是同一行日志，所以共用这一格旗子，而不是让"结束目标"另开一套延迟机制。
/// `GoalSet` / `GoalEdit` 是定目标与编辑目标的排队形状——它们落行的校验
/// 在寄存前已经做过一遍，落行时经同一个纯函数再说一遍同一句话
#[derive(Clone)]
pub enum PendingMode {
    Switch {
        mode: String,
    },
    GoalSet {
        objective: String,
        criteria: Option<Value>,
        constraints: Option<Vec<String>>,
        max_cost_usd: Option<String>,
        profile: Option<String>,
        force: bool,
    },
    GoalEdit {
        objective: Option<String>,
        criteria: Option<Value>,
        constraints: Option<Vec<String>>,
    },
    Discard,
}

impl ModeHub {
    fn set(&self, conversation_id: &str, request: PendingMode) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(conversation_id.to_string(), request);
    }

    /// 取走那一格请求：原来立着就返回它，并当场撤下（与暂停旗一样的"消费即清"，
    /// 迟到的旗子不会在几轮之后突然生效）
    fn take(&self, conversation_id: &str) -> Option<PendingMode> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(conversation_id)
    }
}

/// 把一次待落的请求折成该写进日志的那一行。**纯的**：只吃请求与当时的模式状态，
/// 不碰日志也不碰 AppHandle，所以"切到规划该自动暂停、切到对话该照跑"这些结论
/// 能单测，而不是只剩实盘一种测法
fn mode_after_request(
    request: &PendingMode,
    held: &crate::session::mode::State,
    now: i64,
) -> Result<crate::session::mode::State, String> {
    match request {
        PendingMode::Switch { mode } => mode_state_from(mode, held),
        PendingMode::GoalSet {
            objective,
            criteria,
            constraints,
            max_cost_usd,
            profile,
            force,
        } => {
            let contract = contract_from_json(criteria.as_ref(), constraints.as_deref())?;
            goal_state_from(
                objective.clone(),
                contract,
                max_cost_usd.as_deref(),
                now,
                held,
                profile.clone(),
                *force,
            )
        }
        PendingMode::GoalEdit {
            objective,
            criteria,
            constraints,
        } => {
            // 与 session_goal_edit 同一套合并规矩：只改一半时缺的一半沿用旧契约
            let prev = held.contract.clone();
            let new_criteria = match criteria {
                Some(_) => criteria.clone(),
                None => prev.as_ref().map(|contract| {
                    serde_json::to_value(&contract.criteria).unwrap_or(Value::Null)
                }),
            };
            let new_constraints = constraints
                .clone()
                .or_else(|| prev.as_ref().map(|contract| contract.constraints.clone()));
            let contract = contract_from_json(new_criteria.as_ref(), new_constraints.as_deref())?;
            let wanted = objective
                .clone()
                .map(|text| text.trim().to_string())
                .unwrap_or_else(|| held.objective.clone().unwrap_or_default());
            let next = crate::session::mode::State {
                objective: Some(wanted),
                contract: contract.or_else(|| held.contract.clone()),
                ..held.clone()
            };
            if let Some(contract) = &next.contract {
                crate::goal::contract::validate(next.objective.as_deref().unwrap_or(""), contract)?;
            }
            Ok(next)
        }
        // 结束目标：整份清掉。与 `session_goal_discard` 直接写的那一行是同一个出处，
        // 所以"现在按"与"这一轮收尾时按"落下的行不会长成两种形状
        PendingMode::Discard => Ok(crate::session::mode::State {
            // 结束之后停在哪一档：目标档回对话档；交互档（对话/规划）保持不变
            working: if held.working == crate::session::mode::Working::Goal {
                crate::session::mode::Working::Chat
            } else {
                held.working
            },
            ..Default::default()
        }),
    }
}

/// 回合中立起的切档旗，落进**当轮那份开着的副本**里。位置是承重的：必须排在
/// [`goal_after_round`] 之前——续跑判据读的就是这一行，落晚了就会拿旧档位去判下一轮，
/// 于是"切到规划档这一轮就该停了"却还是自己接了下去，而规划档下它一步也动不了
fn apply_pending_mode(
    host: &TurnHost,
    conversation_id: &str,
    send: &mut Send,
) -> Result<(), String> {
    let Some(request) = host.mode_hub.take(conversation_id) else {
        return Ok(());
    };
    let held = crate::session::mode::in_effect(&send.opened.log);
    let next = mode_after_request(&request, &held, crate::session::now_millis())?;
    send.append_quiet(crate::session::mode::row(&next))
        .map(|_| ())
}

/// 没人消费的那一格兜底落盘。走得到的路只有出错与按停止那两条（它们不经过收尾判据），
/// 而那时这一线程已经没有开着的副本——直接写日志是安全的。
/// **丢掉用户按下的那一次切档**才是不安全的：他要的是"这一支别再推了/换个档"，
/// 界面上却看不出按没按下
fn commit_pending_mode(
    app: &AppHandle,
    conversation_id: &str,
    request: &PendingMode,
) -> Result<(), String> {
    use crate::session::entry::NewEntry;

    let mut session = open_session(app, conversation_id)?;
    let held = crate::session::mode::in_effect(&session.log);
    let next = mode_after_request(request, &held, crate::session::now_millis())?;
    session
        .log_mut()
        .append(
            NewEntry::new(crate::session::mode::row(&next)),
            crate::session::now_millis(),
        )
        .map_err(|error| error.to_string())?;
    session.save()
}

fn stopped(flag: &std::sync::atomic::AtomicBool) -> bool {
    flag.load(std::sync::atomic::Ordering::Relaxed)
}

/// 轮间插话（steering）队列：长任务跑着的时候用户又发了消息。
/// 不打断当前流，而是在下一轮请求前把插话拼进上下文——模型因此能"听劝改道"，
/// 而不是等整个回合跑完才看到。参照 pi 的 getSteeringMessages。
#[derive(Clone, Default)]
pub struct SteeringHub {
    queues: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<
                String,
                std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
            >,
        >,
    >,
}

impl SteeringHub {
    /// 回合开跑时登记队列。此前队列是 push 时懒创建的——回合线程退出后
    /// 迟到的插话会把条目**重新造出来**，然后永远没人消费，消息就这么蒸发
    /// （design-steering-followup-fixes.md §1）。条目从此只由 register 生、
    /// 由 release 死，push 找不到条目就是"没有在跑的回合"
    fn register(&self, conversation_id: &str) {
        self.queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(conversation_id.to_string())
            .or_insert_with(|| {
                std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()))
            });
    }

    /// 入队。条目不存在 = 回合已收尾：报错而不是复活队列，
    /// 前端拿这句去把话降级成新消息发送
    pub(crate) fn push(&self, conversation_id: &str, text: &str) -> Result<(), String> {
        let queue = self
            .queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(conversation_id)
            .cloned()
            .ok_or_else(|| {
                "这一回合已经结束了，插话没有落进任何在跑的任务。把它作为新消息重新发送即可。"
                    .to_string()
            })?;
        queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(text.to_string());
        Ok(())
    }

    fn drain(&self, conversation_id: &str) -> Vec<String> {
        let queue = self
            .queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(conversation_id)
            .cloned();
        match queue {
            Some(queue) => queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .drain(..)
                .collect(),
            None => Vec::new(),
        }
    }

    fn release(&self, conversation_id: &str) {
        self.queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(conversation_id);
    }
}

/// 往正在运行的回合里插一句话。队列在后端，run_turn 在轮间消费
#[tauri::command]
pub fn chat_steer(
    app: AppHandle,
    state: tauri::State<'_, SteeringHub>,
    conversation_id: String,
    text: String,
) -> Result<(), String> {
    let text = text.trim().to_string();
    if text.is_empty() {
        return Err("插话内容为空。".into());
    }
    // worker 回合：插话经协议送进子进程的 steering hub（steer_failed 原样传回，
    // 前端据此把话降级成新消息——语义与内联一致）
    if worker_turn_active(&conversation_id) {
        return crate::agent_supervisor::global(&app)
            .request(
                crate::agent_protocol::methods::STEER_PUSH,
                serde_json::json!({ "conversationId": conversation_id, "text": text }),
            )
            .map(|_| ());
    }
    state.push(&conversation_id, &text)
}

/// 跟随队列（pi 的 followUp）：回合跑着的时候用户又发的话，不插进当前回合，
/// 而是排队——这一轮收尾后由调度线程依次取走，作为新的输入自动开下一轮。
/// 与插话（steering）的分工：插话改变"当前正在跑的这件事"，跟随排的是"下一件事"
#[derive(Clone, Default)]
pub struct FollowUpHub {
    queues: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<
                String,
                std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
            >,
        >,
    >,
}

impl FollowUpHub {
    /// 与 SteeringHub 同一条生命周期规则：条目只由 register 生、由 release 死。
    /// 懒创建会让回合收尾后迟到的排队复活出一条永远没人消费的队列
    fn register(&self, conversation_id: &str) {
        self.queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(conversation_id.to_string())
            .or_insert_with(|| {
                std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()))
            });
    }

    /// 入队并返回排队后的长度，前端拿它显示"已排队 N 条"。
    /// 条目不存在 = 回合已收尾：报错，前端把这句降级成新消息
    pub fn push(&self, conversation_id: &str, text: &str) -> Result<usize, String> {
        let queue = self
            .queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(conversation_id)
            .cloned()
            .ok_or_else(|| {
                "这一回合已经结束了，排队没有落进任何在跑的任务。把它作为新消息重新发送即可。"
                    .to_string()
            })?;
        let mut queued = queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queued.push_back(text.to_string());
        Ok(queued.len())
    }

    pub fn pop(&self, conversation_id: &str) -> Option<String> {
        let queue = self
            .queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(conversation_id)
            .cloned()?;
        let mut queued = queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queued.pop_front()
    }

    /// 只往队头看一眼，不取走。续跑判据要在"这一轮该接目标的一轮还是人排的一轮"
    /// 这件事上做决定，而决定要能单测——所以它得先看到事实，再由执行效果那一步真取走
    pub fn peek(&self, conversation_id: &str) -> Option<String> {
        let queue = self
            .queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(conversation_id)
            .cloned()?;
        let queued = queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queued.front().cloned()
    }

    /// 停止与失败都把队列清空："到此为止"的语义必须干净，不能让半个队列
    /// 在用户看不见的地方过夜
    pub fn clear(&self, conversation_id: &str) {
        if let Some(queue) = self
            .queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(conversation_id)
            .cloned()
        {
            queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
        }
    }

    pub fn release(&self, conversation_id: &str) {
        self.queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(conversation_id);
    }
}

/// 往跟随队列排一句话。这一轮收尾后自动作为新输入开下一轮；返回排队后的长度
#[tauri::command]
pub fn chat_follow_up(
    state: tauri::State<'_, FollowUpHub>,
    conversation_id: String,
    text: String,
) -> Result<usize, String> {
    let text = text.trim().to_string();
    if text.is_empty() {
        return Err("排队内容为空。".into());
    }
    state.push(&conversation_id, &text)
}

/// 服务商错误的可重试分类：限流、上游故障、网络层失败才值得重试；
/// 鉴权（401/403）和路径错误（404）重试多少次结果都一样。
/// 流中断也在列：输出前断开会被 first_token_ms 门槛放行自动重试，
/// 输出后断开自然被门槛挡住——不会把半截回复重复发一遍
fn is_retryable(error: &str) -> bool {
    [
        RETRYABLE_STATUS_WORD,
        "HTTP 500",
        "HTTP 502",
        "HTTP 503",
        "HTTP 504",
    ]
    .iter()
    .any(|hint| error.contains(hint))
        || error.starts_with("请求失败：")
        || error.contains("读取流中断")
        || error.contains("Peer disconnected")
        || error.contains("connection reset")
        || error.contains("timed out")
}
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(600);

/// 命中本话题内规则那一条放行，卡片与账上要说的那一句。
/// 文案只有这一个出处：界面与审计读的是同一个常量，漂不出一条写"按规则"另一条写"按话题"
const SESSION_RULE_PASS: &str = "按本话题规则放行";
/// 无人值守那条路上，先前某一次点头留下的 standing 授权顶掉了这一次的询问
const STANDING_GRANT_PASS: &str = "按已登记的长期放行";
/// 自动审查替人拍板那一条放行。它不走话题规则表，收回的旋钮是「设置 → 行为」的自动审查开关
const AUTO_REVIEW_PASS: &str = "自动审查通过";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    /// assistant 声明的工具调用数组（OpenAI 格式原样回传）。
    /// 话题恢复后重放历史用：让模型记得这轮对话里执行过哪些操作
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Value>,
    /// role=tool 的结果消息对应的调用 id
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolStatus {
    /// 已发给服务商、正等用户在界面上批准
    Pending,
    Running,
    Done,
    Denied,
    Failed,
}

/// update_plan 的一步。status 就是 schema 里那三态的原样，前端只画不判
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanStep {
    pub title: String,
    pub status: String,
}

/// ask_user 的一个选项
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AskOption {
    pub label: String,
    pub description: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ChatEvent {
    Delta {
        text: String,
    },
    /// 推理模型的思维链，OpenAI 兼容服务商放在 delta.reasoning_content
    Reasoning {
        text: String,
    },
    // 枚举级 rename_all 不作用于结构体变体字段，必须逐变体声明
    #[serde(rename_all = "camelCase")]
    Tool {
        id: String,
        name: String,
        status: ToolStatus,
        risk: String,
        input: String,
        output: Option<String>,
        /// 原始参数 JSON 文本。前端落盘，话题恢复时用于重放历史
        arguments: Option<String>,
        /// 这一发**该问而没问**：凭什么过的闸，一句人话。`None` = 要么本来就无需
        /// 询问（权限表那一档放行），要么刚刚弹过窗。见 `pass_reason` 那段
        pass_reason: Option<String>,
        /// 这一发声明时，本轮正文已经流出了多少个 **UTF-16 码元**（前端 JS 字符串
        /// 的下标口径）。前端按它把正文切片、把工具行插回原文流——过程因此是
        /// 流式的（文字→工具→文字），不再全部堆在消息顶上
        content_chars: Option<u32>,
    },
    #[serde(rename_all = "camelCase")]
    Done {
        input_tokens: u32,
        output_tokens: u32,
        duration_ms: u64,
        /// 命中服务商缓存的输入部分；null = 服务商没回这个字段。
        /// 前端用它即时改判缓存命中率，不必再查一次台账（第 6 步 setCacheHit 的数据出口）
        cached_tokens: Option<u32>,
        /// 本轮往日志里新登记的条目 id，按登记顺序。界面上"重新生成/编辑重发"要指名
        /// 移到哪条之后，而那个名字只有后端知道
        entry_ids: Vec<String>,
        /// 这一发**实际发出去**的模型名。池子会换人、路由表会改名，所以它和设置里那个
        /// 顶层 model 可以不一样——而界面那行"这句是谁答的"只有这里答得了
        model: String,
        /// 这一发**实际生效**的上下文窗口。分母跟着路由走：别的档案/池成员的模型行
        /// 才是真相（300K 配在 A 档案、当前连接是 B 档案时，顶层读数是假的），
        /// 用量面板拿它当分母，与预算表/压缩闸同一格读数
        context_tokens: u32,
    },
    /// 服务商没报错但这一轮不完整（输出被截断、被安全策略拦下）。
    /// 半句话静静地当完整答案显示出来是最难查的骗人方式
    Notice {
        text: String,
    },
    /// 目标模式每一轮跑完之后的续跑读数。它必须坐在 [`ChatEvent::Done`] **前面**：
    /// 界面在 Done 上就落"这一轮结束了、后面收到的字统统作废"，之后再告诉它
    /// "还有一轮要接"就晚了——那一轮的字节会被整批丢掉。
    /// 带整份读数是因为"跑到第几轮、停在哪儿"只有后端知道，别让前端回来猜一次
    #[serde(rename_all = "camelCase")]
    Mode {
        /// 后端还会不会自己接下一轮
        continuing: bool,
        /// 这一支现在的作业模式
        state: ModeView,
    },
    /// 一次文件快照已落账（write_file/delete_file 的动手前副本）。快照从静默的
    /// 台账写变成显式事件：界面的变更面板可以即时点亮，审计里也有这一笔——
    /// "AI 刚刚动了哪个文件、能不能回滚"不该等人打开面板才知道
    #[serde(rename_all = "camelCase")]
    FileSnapshot {
        /// 相对工作目录根的展示路径
        path: String,
        /// 对应的工具调用 id，与 Tool 事件对得上
        call_id: String,
        additions: u32,
        deletions: u32,
        /// 动手前存了一份可恢复的副本（快照或备份）；false = 太大没存，note 里有原因
        backup: bool,
        /// 没存快照时给人看的说法；存了就是空串
        snapshot_note: String,
    },
    /// 服务商报错了、但这一轮还会自己重来。它既不是"回答的一部分"也不是"本轮残缺"的
    /// 自证——重试成功后接出来的答案是完整的，所以别把它混进正文里冒充模型说的话
    #[serde(rename_all = "camelCase")]
    Retry {
        /// 说清还要等多久、第几次
        text: String,
        /// 失败原因原文，交给告警条的正文
        reason: String,
    },
    /// 上下文自动压缩的两个阶段。start 时界面显示"压缩中"，
    /// done 时用 summary 替换掉被压缩的旧消息（kept 是保留原文的条数）
    #[serde(rename_all = "camelCase")]
    Compaction {
        phase: String,
        summary: Option<String>,
        kept: Option<usize>,
    },
    Error {
        message: String,
    },
    /// 请求链路的阶段探针（输入/载荷/出站/首字节/模型对账）。不进日志、不参与成败：
    /// 它是消息头行那条链路动画的数据源，前端按 key 映射图标与文案。
    /// `tone` 是格子的读色（ok 一致 / info 中性 / warn 要告警），`hint` 是悬停
    /// 展开的完整三元组——两格都缺省，老探针帧一个字节不多
    #[serde(rename_all = "camelCase")]
    Probe {
        key: String,
        detail: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tone: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hint: Option<String>,
    },
    /// 模型整份上报的计划清单（update_plan）。前端拿它画计划卡，不再查、不再算；
    /// 排在工具事件之外，因为它的生命周期是整场话题，不是一次工具调用
    #[serde(rename_all = "camelCase")]
    Plan {
        explanation: Option<String>,
        steps: Vec<PlanStep>,
    },
    /// ask_user 的提问。后端这一发挂起等人；`id` 是回答通道的钥匙（ask_user_respond）。
    /// stop 打断后这里发过的问题随工具结果"用户没有回答"一起作废，前端按 done 清卡
    #[serde(rename_all = "camelCase")]
    Ask {
        id: String,
        question: String,
        options: Vec<AskOption>,
    },
}

#[derive(Clone)]
struct Usage {
    input_tokens: u32,
    output_tokens: u32,
    /// 命中缓存的那部分输入，协议里算在 input_tokens 之内，计价时要先减出去。
    /// None 表示服务商根本没回这个字段——它和"回了 0"是两件事：把没上报当成全灭，
    /// 不支持缓存的服务商会显示成 0% 命中，看着像前缀被改坏了
    cached_tokens: Option<u32>,
    /// 写进缓存的输入，按更贵的单价计费。chat 格式不回这个字段，就是 0
    cache_write_tokens: u32,
    /// 思考消耗的 token，是 output_tokens 的子集：只用于展示，不能再加一遍
    reasoning_tokens: u32,
}

impl Usage {
    /// chat 格式：prompt_tokens 里已含命中缓存的部分。
    /// 字段名两家不同——OpenAI 系放在 `prompt_tokens_details.cached_tokens`，
    /// DeepSeek 系放在顶层 `prompt_cache_hit_tokens`（配套的 miss 字段恒等于
    /// input 减去它，所以不单独存）。两个都取不到才是"未上报"
    fn from_chat(value: &Value) -> Option<Usage> {
        if !value.is_object() {
            return None;
        }
        let cached = value["prompt_tokens_details"]["cached_tokens"]
            .as_u64()
            .or_else(|| value["prompt_cache_hit_tokens"].as_u64());
        Some(Usage {
            input_tokens: value["prompt_tokens"].as_u64().unwrap_or(0) as u32,
            output_tokens: value["completion_tokens"].as_u64().unwrap_or(0) as u32,
            cached_tokens: cached.map(|v| v as u32),
            cache_write_tokens: 0,
            reasoning_tokens: value["completion_tokens_details"]["reasoning_tokens"]
                .as_u64()
                .unwrap_or(0) as u32,
        })
    }

    /// responses 格式：字段名换了，语义没换
    fn from_responses(value: &Value) -> Option<Usage> {
        if !value.is_object() {
            return None;
        }
        Some(Usage {
            input_tokens: value["input_tokens"].as_u64().unwrap_or(0) as u32,
            output_tokens: value["output_tokens"].as_u64().unwrap_or(0) as u32,
            cached_tokens: value["input_tokens_details"]["cached_tokens"]
                .as_u64()
                .map(|v| v as u32),
            cache_write_tokens: value["input_tokens_details"]["cache_write_tokens"]
                .as_u64()
                .unwrap_or(0) as u32,
            reasoning_tokens: value["output_tokens_details"]["reasoning_tokens"]
                .as_u64()
                .unwrap_or(0) as u32,
        })
    }

    /// Gemini 的 usageMetadata。candidatesTokenCount **含**思考消耗
    /// （thoughtsTokenCount 是它的子集，与 OpenAI 的口径相反）；input 不含缓存，
    /// cachedContentTokenCount 单列——归一化与 Anthropic 同一公式：
    /// input = 裸输入 + 缓存命中
    fn from_gemini(value: &Value) -> Option<Usage> {
        if !value.is_object() {
            return None;
        }
        let raw_input = value["promptTokenCount"].as_u64().unwrap_or(0);
        let cached = value["cachedContentTokenCount"].as_u64();
        let output = value["candidatesTokenCount"].as_u64().unwrap_or(0);
        Some(Usage {
            input_tokens: (raw_input + cached.unwrap_or(0)) as u32,
            output_tokens: output as u32,
            cached_tokens: cached.map(|v| v as u32),
            cache_write_tokens: 0,
            reasoning_tokens: value["thoughtsTokenCount"].as_u64().unwrap_or(0) as u32,
        })
    }

    /// Anthropic Messages 格式（message_start 事件里的那份）。
    ///
    /// 它的 `input_tokens` **不含**缓存命中的部分（与 chat/responses 的"含在内"
    /// 相反），而台账与计价的全局不变量是"input_tokens 里含命中缓存的那部分、
    /// 按全价计费前先减出去"。所以在读进来那一刻就归一化：
    /// input = 裸输入 + cache_read + cache_creation，下游一条公式都不用改。
    /// `cache_read_input_tokens` 字段缺失 = 未上报（保持 None），不冒充命中 0
    fn from_anthropic(value: &Value) -> Option<Usage> {
        if !value.is_object() {
            return None;
        }
        let raw_input = value["input_tokens"].as_u64().unwrap_or(0);
        let cache_read = value["cache_read_input_tokens"].as_u64();
        let cache_write = value["cache_creation_input_tokens"].as_u64().unwrap_or(0);
        Some(Usage {
            input_tokens: (raw_input + cache_read.unwrap_or(0) + cache_write) as u32,
            output_tokens: value["output_tokens"].as_u64().unwrap_or(0) as u32,
            cached_tokens: cache_read.map(|v| v as u32),
            cache_write_tokens: cache_write as u32,
            reasoning_tokens: 0,
        })
    }
}

#[derive(Default)]
struct ToolCallBuffer {
    id: String,
    name: String,
    arguments: String,
    /// 声明时本轮正文的 UTF-16 码元数（回合循环在派发前盖章）。0 = 旧路径没盖
    content_chars: u32,
}

pub(crate) struct RoundOutcome {
    text: String,
    /// 推理模型的思维链。它不上 wire（服务商从不收它），但它是用户在界面上读过的一段话，
    /// 必须留在条目里，否则恢复话题时那一屏就空了
    reasoning: Option<String>,
    /// Anthropic 思考块签名：回放凭据，随条目落库、下一轮原样带回
    reasoning_signature: Option<String>,
    /// Responses reasoning 项的 JSON 数组（服务商原样发回的项）：回放凭据
    reasoning_items_json: Option<String>,
    tool_calls: Vec<ToolCallBuffer>,
    usage: Option<Usage>,
    /// 这一发放上 wire 的请求体有多大（字符数）。它与服务商回报的 `prompt_tokens`
    /// 成对记进用量台账，字符↔token 的系数与偏差上界才有实测依据（Context 设计 §0 的 P2）
    sent_chars: usize,
    /// 这一轮输出被 token 上限截断。截断轮里的工具调用参数可能是半截 JSON，不能执行
    truncated: bool,
    /// 模型对账三件套：payload 里实际写出的 model、上游自报的 model（嗅探）、来源路径
    sent_model: String,
    response_model: Option<String>,
    response_model_path: Option<String>,
}

impl RoundOutcome {
    /// 服务商报告的用量。给保温那条记账用：它和对话回合走的是同一个换算
    #[allow(dead_code)] // 保温记账恢复接线时启用
    pub(crate) fn tokens(&self) -> crate::usage::Tokens {
        tokens_of(&self.usage)
    }

    /// 实发的请求体字符数。保温那条记账在另一个模块，字段够不着，只能问
    pub(crate) fn sent_chars(&self) -> usize {
        self.sent_chars
    }

    /// 回合正文（agent 子进程的 turn.once 用）：文本是只读投影，不外借字段
    pub(crate) fn text(&self) -> &str {
        &self.text
    }
}

/// 一次请求失败。失败也带着**已经流出来的那部分**：用户按了停止，那半截他在界面上读过了，
/// 不进日志下一轮模型就不知道它说过这句话（F15）——而它只能是"未写完"的落定形状
pub(crate) struct RoundFailure {
    message: String,
    partial: RoundOutcome,
}

impl RoundFailure {
    /// 失败原因。`stopped()` 与对外报错都读它，字段本身不外露
    pub(crate) fn message(&self) -> &str {
        &self.message
    }

    fn stopped(&self) -> bool {
        self.message == STOP_MARK
    }
}

/// 事件出口。回合正文只通过它说话，所以"谁在收"与"这一轮怎么跑"是分开的两件事：
/// 界面发起的对话用 webview 给的 `Channel`，后台跑的那些年（定时任务、编排出来的节点）
/// 用 `EmitSink`。少这一层，任务就得自己手写一份话题——那正是上一轮的双轨真相
/// 回合的宿主环境（M3 收官）：回合族（run_turn/turn_body/close_turn/goal_after_round/
/// mode_view）只认它，不再摸 AppHandle。Main 从 AppHandle + State 组装（app 有值，
/// 交互全真）；worker 从 CLI 目录 + 自建 hub 组装（app = None，toast 走 ev 由
/// Main 代发，重活类诚实拒绝）
pub(crate) struct TurnHost {
    pub config_dir: std::path::PathBuf,
    pub data_dir: std::path::PathBuf,
    /// Main 专属交互面。None = worker：toast 走 ev、保温跳过、重活类拒绝
    pub app: Option<tauri::AppHandle>,
    /// worker 侧 ev 出口：(事件名, 数据)。Main = None
    pub ev: Option<
        std::sync::Arc<dyn Fn(&str, serde_json::Value) + std::marker::Send + std::marker::Sync>,
    >,
    /// 交互登记表（goal 机器与模式寄存）。Main 克隆自 State，worker 自建——
    /// 全是 Arc 结构，克隆共享同一份状态
    pub pause: PauseHub,
    pub follow_up: FollowUpHub,
    pub guards: GoalGuards,
    pub mode_hub: ModeHub,
}

impl TurnHost {
    pub(crate) fn from_app(app: &AppHandle) -> Self {
        TurnHost {
            config_dir: app.path().app_config_dir().unwrap_or_default(),
            data_dir: app.path().app_data_dir().unwrap_or_default(),
            app: Some(app.clone()),
            ev: None,
            pause: app
                .try_state::<PauseHub>()
                .map(|h| h.inner().clone())
                .unwrap_or_default(),
            follow_up: app
                .try_state::<FollowUpHub>()
                .map(|h| h.inner().clone())
                .unwrap_or_default(),
            guards: app
                .try_state::<GoalGuards>()
                .map(|h| h.inner().clone())
                .unwrap_or_default(),
            mode_hub: app
                .try_state::<ModeHub>()
                .map(|h| h.inner().clone())
                .unwrap_or_default(),
        }
    }

    #[allow(dead_code)] // worker 回合入口：真机/集成路径用，测试构建不达
    pub(crate) fn for_worker(
        config_dir: std::path::PathBuf,
        data_dir: std::path::PathBuf,
        ev: std::sync::Arc<dyn Fn(&str, serde_json::Value) + std::marker::Send + std::marker::Sync>,
        pause: PauseHub,
        follow_up: FollowUpHub,
        guards: GoalGuards,
        mode_hub: ModeHub,
    ) -> Self {
        TurnHost {
            config_dir,
            data_dir,
            app: None,
            ev: Some(ev),
            pause,
            follow_up,
            guards,
            mode_hub,
        }
    }
}

pub trait EventSink: Sync {
    fn send(&self, event: ChatEvent);
}

impl EventSink for Channel<ChatEvent> {
    fn send(&self, event: ChatEvent) {
        let _ = Channel::send(self, event);
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Emitted {
    conversation_id: String,
    event: ChatEvent,
}

/// 后台运行的事件出口：转成一次 `app.emit`，界面按同一个 ChatEvent 形状渲染。
/// 没有人在听也不丢事实——事实住在话题日志里，这里丢的只是实时画面
pub struct EmitSink {
    app: AppHandle,
    conversation_id: String,
}

impl EmitSink {
    pub fn new(app: &AppHandle, conversation_id: &str) -> Self {
        Self {
            app: app.clone(),
            conversation_id: conversation_id.to_string(),
        }
    }
}

impl EventSink for EmitSink {
    fn send(&self, event: ChatEvent) {
        let _ = self.app.emit(
            "chat-event",
            Emitted {
                conversation_id: self.conversation_id.clone(),
                event,
            },
        );
    }
}

/// 目标点名执行的档案：这一支还挂着目标时，整份连接域照它走（池子与路由都不抢）。
/// 每轮重读一次——目标可能在轮间被改、被暂停、被结束。
/// 这里认的是 `goal_held` 而不是 `goal_active`：**暂停与停住都不该把档案换掉**——
/// 从前 paused 只是 active 上的一格旗子，所以暂停中的目标照旧走点名的那张档案；
/// 六值之后要让这件事不变，就得显式认"还挂着账"那一扇门（收尾才算翻篇）
fn goal_profile_of(app: &AppHandle, conversation_id: &str) -> Option<String> {
    let source = open_session(app, conversation_id).ok()?;
    let held = crate::session::mode::in_effect(&source.log);
    if held.goal_held() {
        held.profile
    } else {
        None
    }
}

/// 开一条发送线程。`chat_send`（界面发的每一句话）与 `session_goal_resume`
/// （目标恢复时立刻接的那一轮）共用这一条路：登记、插话、排队、保温、池内换人、
/// 续跑循环全在这里——两处各抄一份，迟早漂成两条对不上账的回路。
///
/// `goal_kick = true` 是目标恢复开的那一轮：首轮按"续跑轮"算（压缩的预防线放宽），
/// 输入恒为空——那句"接着往下做"由调用方先写进日志，这里不再追加一条用户发言。
/// 事件出口由调用方定：界面的对话走 webview 的 `Channel`，恢复的那轮走 `EmitSink`
/// 广播（没有谁在原地等它，目标面板按话题 id 收）。
#[allow(clippy::too_many_arguments)]
fn spawn_send_turn(
    app: AppHandle,
    hub: ApprovalHub,
    stop_hub: StopHub,
    steering_hub: SteeringHub,
    follow_up_hub: FollowUpHub,
    mcp_hub: crate::mcp::Hub,
    warm_hub: crate::warm::Hub,
    input: String,
    attachments: Vec<String>,
    conversation_id: String,
    // 先把分支末端移到这条条目之后再发。`None` = 不回溯（正常发新消息）
    rewind_to: Option<String>,
    // 移到根之前——编辑第一条消息时用它。和 `rewind_to: None` 是两件事，
    // 不能靠同一个 Option 兼职表达
    rewind_to_root: bool,
    // 这一条消息不带记忆注入。用完即弃，不写进配置
    skip_memory: bool,
    // 决策层（System 1）替模型池挑好的成员。只有池子的 decision 模式读它；
    // 跟随轮沿用同一个结果——决策是"这条消息链发给谁"的一次裁定，不是每轮重掷
    pool_pick: Option<crate::config::PoolKey>,
    goal_kick: bool,
    on_event: std::sync::Arc<dyn EventSink + std::marker::Send + std::marker::Sync>,
) -> Result<(), String> {
    let config = config::load(&app);
    // 服务器清单要在开线程前定好：独立配置的加上启用中插件带的
    let mcp_servers = crate::mcp::all_servers(&app, &config);
    // 回合宿主：目录锚点 + 交互面 + 交互登记表，回合族只认它
    let host = TurnHost::from_app(&app);
    let handle = app.clone();
    // 本回合的停止开关与插话队列。线程里只拿 Arc/克隆，登记表由本函数收尾时清理。
    // 停止开关的登记带占位语义：这一话题已有一轮没收尾时在这里被拒（而不是
    // 把人家的旗标顶掉）
    let stop = stop_hub.register(&conversation_id)?;
    // 插话与排队的队列同一条生命周期：这里登记，线程收尾时 release。
    // 迟到的入队会拿到"回合已结束"的报错，前端据此把话降级成新消息
    steering_hub.register(&conversation_id);
    follow_up_hub.register(&conversation_id);
    let steering = steering_hub.clone();
    let stop_for_thread = stop.clone();
    // 用户又发了一次真实请求：上一发待放的保温当场作废——这次请求本身已经把缓存续上了
    warm_hub.cancel(&conversation_id);
    let warm = warm_hub.clone();
    let follow_up = follow_up_hub.clone();
    let conversation_for_release = conversation_id.clone();
    let stop_hub_for_release = stop_hub.clone();
    let steering_hub_for_release = steering_hub.clone();
    let follow_up_hub_for_release = follow_up.clone();

    thread::spawn(move || {
        // 跟随轮循环：这一轮收尾后，跟随队列里有货就接着开下一轮
        // （pi 的 followUpMode）。排队的话作为正常新输入跑，不回溯、不带附件
        let mut next_input = input;
        let mut next_attachments = attachments;
        let mut first_turn = true;
        // 轮内 failover 的让位清单：这一发已经失败的池成员键。
        // 不设次数上限，一路换到某个成员成功或池子里的成员全部耗尽
        let mut pool_excluded: Vec<String> = Vec::new();
        // goal 续跑轮标志：上一发 Next::Go 之后自动接的轮。压缩的预防线只在
        // 这种步骤边界上放宽；人排的话进来（queued）就回到普通轮。
        // 目标恢复开的那一轮同属此类
        let mut continuing_goal = goal_kick;
        loop {
            // 技能正文每轮重读：跟随轮可能隔着好几分钟，这期间用户可能装了新技能
            let skills = match crate::skills::prompt(&handle) {
                Ok(text) => text,
                Err(error) => {
                    eprintln!("技能没能加载：{error}");
                    None
                }
            };
            // 只有第一轮带回溯指令（重新生成/编辑重发）；跟随轮就是正常的新输入
            let (rewind_target, rewind_root) = if first_turn {
                (rewind_to.as_deref(), rewind_to_root)
            } else {
                (None, false)
            };
            // 用户这一发要的模型名（本地映射改写之前的那一份）：模型对账的 requested 格
            let requested_model = config.model.clone();
            // 模型池：每一发都重新问一遍（池子配置可能在上轮之后改过），
            // 亲和账让同一话题粘住上一次的成员（服务商缓存按账号×模型分域，
            // 工具轮里换人等于把命中率交给运气）。
            // 池子明确说走不通（如手动指定的成员被删了）就把话带给界面并停——
            // 悄悄改道等于替用户做决定；池子没接管就照旧用顶层配置。
            // 目标点名了服务商档案时它说了算：整份连接域照那张档案走，
            // 池子与路由都不抢——"指定谁执行目标"就是这一支的裁定，轮轮一致
            let (turn_config, _pool_turn) = match goal_profile_of(&handle, &conversation_id) {
                Some(profile_id) => {
                    match with_connection(config.clone(), None, Some(&profile_id)) {
                        Ok(direct) => (direct, None),
                        Err(message) => {
                            on_event.send(ChatEvent::Error { message });
                            break;
                        }
                    }
                }
                None => match crate::pool::resolve(
                    &handle,
                    &config,
                    &next_input,
                    pool_pick.as_ref(),
                    &conversation_id,
                    // 界面那一发是人起的：池子里谁都能上，"不许派工"不拦人自己的选择
                    false,
                    &pool_excluded,
                ) {
                    Ok(Some(turn)) => {
                        if turn.picked.source == "fallback" {
                            // 决策层没选成的兜底提醒与换人重试同族：只弹贴附提示，
                            // 不进正文——正文是模型说的话，不是调度过程的流水账
                            on_event.send(ChatEvent::Retry {
                                text: "由调度器兜底。".into(),
                                reason: "决策层这次没选成。".into(),
                            });
                        }
                        (turn.config, Some((turn.guard, turn.picked)))
                    }
                    Ok(None) => {
                        // 池子没接管，这一发跟着设置走——路由表在设置直连之前查一遍
                        // （design-model-routing.md：点名 > 池子 > 路由表 > 直连）
                        let mut routed = config.clone();
                        crate::route::apply(&mut routed);
                        (routed, None)
                    }
                    Err(message) => {
                        on_event.send(ChatEvent::Error { message });
                        break;
                    }
                },
            };
            let step = match run_turn(
                &host,
                &turn_config,
                &requested_model,
                &hub,
                &mcp_hub,
                &mcp_servers,
                &stop_for_thread,
                &steering,
                &warm,
                skills,
                continuing_goal,
                // 按值传入但传的是克隆：轮内 failover 的 continue 会跳过循环尾部的
                // 重赋值，原值必须留在变量里供下一轮 resolve 复用
                next_input.clone(),
                next_attachments.clone(),
                rewind_target,
                rewind_root,
                skip_memory,
                // 界面里说话的人就是这一发的预算所在：轮数天花板认设置里那个全局的
                None,
                // 只有这条路会自己接下一轮，目标模式那个续跑循环就在下面
                true,
                &conversation_id,
                &*on_event,
            ) {
                Ok(step) => step,
                Err(message) => {
                    // 池子接管的这发，遇到"服务商病了"类失败（5xx/限流/连不上）且
                    // 还没产出内容时，本轮内换下一个健康成员重试——不设次数上限，
                    // 一路换到某个成员成功，或池子里的成员全部耗尽为止。
                    // 已经流出内容的失败不在此列：重发等于把同一句话重复扣费。
                    // first_turn 保持原值：首轮的重试仍带回溯指令，跟随轮照旧
                    if let Some((_, failed_pick)) = _pool_turn.as_ref() {
                        // 手动指定（pinned）是用户点的名：失败就原样报错，换人等于
                        // 替用户改道——与 resolve 的 Err 语义同一句话。可换的只有
                        // 调度器与决策层挑出来的成员
                        let swappable_pick = failed_pick.source != "pinned";
                        if swappable_pick && is_pool_swappable_error(&message) {
                            pool_excluded.push(format!(
                                "{}\u{0}{}",
                                failed_pick.key.profile_id, failed_pick.key.model
                            ));
                            // 只弹贴附提示不进正文：换人重试成功后接出来的是完整回答，
                            // 正文里夹一条失败告警会让人以为回答本身就是断的
                            on_event.send(ChatEvent::Retry {
                                text: "换池子里下一个健康成员重试。".into(),
                                reason: message.clone(),
                            });
                            continue;
                        }
                    }
                    // 失败不再续跟随轮：对着错误消息猜队列状态，比直接作废难理解得多
                    follow_up.clear(&conversation_id);
                    // 目标还挂着时先把停格落进日志（错误 → blocked、限流 → usage_limited），
                    // 再报错。顺序承重：界面对 Error 的第一反应就是重读读数——
                    // 先报错后落行，屏上会把"推进中"多挂到下一次刷新为止
                    if let Err(error) = goal_block_on_turn_error(&host, &conversation_id, &message)
                    {
                        eprintln!("那支目标没能落进停格：{error}");
                    }
                    on_event.send(ChatEvent::Error { message });
                    break;
                }
            };
            first_turn = false;
            // 用户按的那一次停止只管到手头这一轮。旗子在这里就消费完了：它一设就一直
            // 是 true（直到下一次 `register`），不放下去，目标接的那一轮会在第一个停止
            // 检查点上再撞一次——"只停这一轮"就成了空话。**这一格是粘的，而停止不是**
            // 停止旗是粘的：这里消费掉它，目标接的那一轮才不会在自己的第一个停止检查点上
            // 再撞一次。作废队列与"要不要接着跑"都由判据在那一步定了（`ClearQueue` 效果），
            // 循环这里只负责把旗放下
            if stopped(&stop_for_thread) {
                stop_for_thread.store(false, std::sync::atomic::Ordering::Relaxed);
            }
            match step.into_step() {
                // `state` 已经是加过一轮的那份：轮数那一格由判据算一次，
                // 循环这里不许再 `armed()` 一遍——两处各加就是每轮跑两格账
                Step::GoalRound {
                    armed: state,
                    notice,
                } => {
                    continuing_goal = true;
                    if let Err(message) = arm_goal_round(&host, &conversation_id, &state) {
                        // 这两行落不下去就别跑那一轮：轮数没加一格，唯一的自动刹车成了空话，
                        // 而没人会替一次写失败的话题继续烧钱
                        on_event.send(ChatEvent::Error { message });
                        break;
                    }
                    // 空输入 = 不再追加一条用户发言。那句"接着往下做"由上面那次写进日志，
                    // 投影成 system 行——它不是用户说的话，就不该长成用户说的话
                    next_input = String::new();
                    next_attachments = Vec::new();
                    if let Some(text) = notice {
                        // 这一句必须说，而且要落在 Done 之后：用户按了停止、屏上那句
                        // "已按你的要求停止生成"也出现了，而这一支其实还在往下推。
                        // 什么都不说就是让它以为目标停了——那比多说一句罗嗦坏得多
                        on_event.send(ChatEvent::Notice { text });
                    }
                }
                Step::UserTurn { text } => {
                    continuing_goal = false;
                    next_input = text;
                    next_attachments = Vec::new();
                }
                // Stop 与 Idle：这一轮说完了，也没有接着要跑的东西
                Step::Stop => break,
            }
        }
        stop_hub_for_release.release(&conversation_for_release);
        steering_hub_for_release.release(&conversation_for_release);
        follow_up_hub_for_release.release(&conversation_for_release);
        // 错误路径不走收尾判据，回合中立起来的暂停旗可能没人消费：这里兜底清一遍。
        // 正常收尾的旗在 `goal_after_round` 里已经被取走，这一下是空清
        if let Some(pause_hub) = handle.try_state::<PauseHub>() {
            pause_hub.inner().clear(&conversation_for_release);
        }
        // 切档旗的兜底与暂停那条相反：这里不是清掉而是**补落**。走到这儿还没被消费的
        // 请求，出自出错与按停止那两条路，而此刻这一线程已经没有开着的副本，直接写是
        // 安全的。丢掉它等于让用户按下去的"换个档 / 结束目标"凭空蒸发——
        // 一声不响地不做事，是这一格里最坏的失败形状
        if let Some(mode_hub) = handle.try_state::<ModeHub>() {
            if let Some(request) = mode_hub.inner().take(&conversation_for_release) {
                if let Err(error) =
                    commit_pending_mode(&handle, &conversation_for_release, &request)
                {
                    eprintln!("那一次排队里的切档落不下去：{error}");
                }
            }
        }
    });

    Ok(())
}

#[tauri::command]
pub fn chat_send(
    app: AppHandle,
    hub: State<'_, ApprovalHub>,
    stop_hub: State<'_, StopHub>,
    steering_hub: State<'_, SteeringHub>,
    follow_up_hub: State<'_, FollowUpHub>,
    mcp_hub: State<'_, crate::mcp::Hub>,
    warm_hub: State<'_, crate::warm::Hub>,
    input: String,
    attachments: Vec<String>,
    conversation_id: String,
    rewind_to: Option<String>,
    rewind_to_root: bool,
    skip_memory: bool,
    pool_pick: Option<crate::config::PoolKey>,
    // 原始 Value：worker 回程的 ev data 直接透传（字节形状不变，UI 零改动）；
    // 内联回合经 ChannelSink 包成 EventSink
    on_event: Channel<serde_json::Value>,
) -> Result<(), String> {
    // 一条话题同一时刻只该有一个回合线程。「继续」/自动续跑开出的目标轮没有界面现场
    // （pending 看不见它），此时再 spawn 一条就是两个写者各持一份副本同写一份日志——
    // 后收尾的整片盖掉先收尾的，插话、排队、收尾读数全都对不上账。
    // 目标轮在跑时这句话的正规入口是插话（生成中按回车）或跟随队列（Ctrl+回车），
    // 前端已经分流；这里挡的是竞态
    if stop_hub.is_running(&conversation_id) {
        return Err(
            "这一支还有一轮在跑（可能挂着目标在自动推进）。等它收尾再发，或在生成中用插话。".into(),
        );
    }
    // 分流开关（蓝图 §A7 ④）：开了 agent_worker_turns 且话题没挂目标 → 回合进子进程。
    // 挂目标的话题留在内联路（goal 续跑循环、暂停/切档寄存都住在那条路上）
    let worker_route = {
        let config = config::load(&app);
        if !config.agent_worker_turns {
            false
        } else {
            let session = open_session(&app, &conversation_id)?;
            !crate::session::mode::in_effect(&session.log).goal_held()
        }
    };
    if worker_route {
        return spawn_worker_turn(
            app,
            stop_hub.inner().clone(),
            steering_hub.inner().clone(),
            follow_up_hub.inner().clone(),
            warm_hub.inner().clone(),
            input,
            attachments,
            conversation_id,
            rewind_to,
            rewind_to_root,
            skip_memory,
            on_event,
        );
    }
    spawn_send_turn(
        app,
        hub.inner().clone(),
        stop_hub.inner().clone(),
        steering_hub.inner().clone(),
        follow_up_hub.inner().clone(),
        mcp_hub.inner().clone(),
        warm_hub.inner().clone(),
        input,
        attachments,
        conversation_id,
        rewind_to,
        rewind_to_root,
        skip_memory,
        pool_pick,
        false,
        std::sync::Arc::new(ChannelSink(on_event)),
    )
}

/// worker 分流的发送线程（M3 收官）：Main 侧的回合管理（停止/插话/跟随的登记
/// 与收尾）留在这边，回合本体经 turn.start 进子进程跑真 run_turn。
/// 跟随队列在本线程里继续排队开下一发——排队的话依然作为正常新输入跑
#[allow(clippy::too_many_arguments)]
fn spawn_worker_turn(
    app: AppHandle,
    stop_hub: StopHub,
    steering_hub: SteeringHub,
    follow_up_hub: FollowUpHub,
    warm_hub: crate::warm::Hub,
    input: String,
    attachments: Vec<String>,
    conversation_id: String,
    rewind_to: Option<String>,
    rewind_to_root: bool,
    skip_memory: bool,
    on_event: Channel<serde_json::Value>,
) -> Result<(), String> {
    // Main 侧登记：StopHub 的 is_running 读者（重复发送闸、切档命令）都看得见
    // worker 回合；worker 回合表是审批/插话/停止三条命令的路由键
    let stop = stop_hub.register(&conversation_id)?;
    steering_hub.register(&conversation_id);
    follow_up_hub.register(&conversation_id);
    warm_hub.cancel(&conversation_id);
    worker_turns()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(conversation_id.clone());
    let handle = app.clone();

    thread::spawn(move || {
        let supervisor = crate::agent_supervisor::global(&handle);
        let channel = on_event;
        // 事件转发：chat → Channel 原样透传（字节形状不变）；toast → Main 代发真通知
        let toast_handle = handle.clone();
        let forward_channel = channel.clone();
        let forward =
            std::sync::Arc::new(move |event: &str, data: &serde_json::Value| match event {
                "chat" => {
                    let _ = Channel::send(&forward_channel, data.clone());
                }
                "toast" => {
                    let kind = data["kind"].as_str().unwrap_or_default();
                    match kind {
                        "approval_needed" => crate::toast::approval_needed(
                            &toast_handle,
                            data["input"].as_str().unwrap_or_default(),
                        ),
                        "unattended_parked" => crate::toast::unattended_parked(
                            &toast_handle,
                            data["display"].as_str().unwrap_or_default(),
                        ),
                        "question_pending" => crate::toast::question_pending(
                            &toast_handle,
                            data["question"].as_str().unwrap_or_default(),
                        ),
                        "goal_settled" => crate::toast::goal_settled(
                            &toast_handle,
                            data["complete"].as_bool().unwrap_or(false),
                            data["note"].as_str().unwrap_or_default(),
                        ),
                        _ => {}
                    }
                }
                _ => {}
            });
        let mut next_input = Some(input);
        let mut next_attachments = attachments;
        let mut first_turn = true;
        while let Some(turn_input) = next_input.take() {
            let turn_attachments = std::mem::take(&mut next_attachments);
            let params = serde_json::json!({
                "conversationId": conversation_id,
                "input": turn_input,
                "attachments": turn_attachments,
                "rewindTo": if first_turn { rewind_to.clone() } else { None::<String> },
                "rewindToRoot": if first_turn { rewind_to_root } else { false },
                "skipMemory": skip_memory,
            });
            first_turn = false;
            let forward_for_call = std::sync::Arc::clone(&forward);
            let result = supervisor.request_opts(
                crate::agent_protocol::methods::TURN_START,
                params,
                // 长回合无秒表：长短由停止键与审批超时管；管道断线即刻判孤儿
                None,
                &mut |event, data| forward_for_call(event, data),
            );
            if let Err(message) = result {
                // 回合失败：跟随队列作废 + Error 事件（与内联路同一形状）
                follow_up_hub.clear(&conversation_id);
                let error_event =
                    serde_json::to_value(ChatEvent::Error { message }).unwrap_or_default();
                let _ = Channel::send(&channel, error_event);
                break;
            }
            if stopped(&stop) {
                break;
            }
            // 排队的话作为正常新输入接着跑（与内联跟随轮同一条规矩）
            match follow_up_hub.pop(&conversation_id) {
                Some(text) => {
                    next_input = Some(text);
                    next_attachments = Vec::new();
                }
                None => break,
            }
        }
        stop_hub.release(&conversation_id);
        steering_hub.release(&conversation_id);
        follow_up_hub.release(&conversation_id);
        worker_turns()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&conversation_id);
        // 回合中立起来的暂停旗兜底清一遍（worker 回合不该有旗，清了也无害）
        if let Some(pause_hub) = handle.try_state::<PauseHub>() {
            pause_hub.inner().clear(&conversation_id);
        }
    });
    Ok(())
}

/// 前端通道的原始透传出口：worker 回程的 ev data 原样进 Channel，
/// 内联回合在 Rust 侧先把 ChatEvent 序列化成同一个 Value 形状
struct ChannelSink(Channel<serde_json::Value>);

impl EventSink for ChannelSink {
    fn send(&self, event: ChatEvent) {
        if let Ok(value) = serde_json::to_value(event) {
            let _ = Channel::send(&self.0, value);
        }
    }
}

/// 作业模式那三条会改日志的命令（切档 / 暂停 / 结束目标）共用的读数包。
/// `deferred` 说的是：回合还在跑，请求刚立进登记表，日志里那一行要等这一轮收尾才落——
/// 界面上那一格此刻先自己标着，收尾的 `Mode` 事件会带着真的那一行来对账。
/// 这一格是必需的：`deferred` 与"什么都没发生"在屏幕上必须长得不一样，否则人只会再点一次
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModeOutcome {
    pub view: ModeView,
    pub deferred: bool,
}

/// 暂停 / 恢复一个目标（`paused = true` 暂停）。回合中往日志另写一行会被收尾的副本
/// 整片盖掉，所以暂停在回合中不写日志——立旗，让回合自己收尾时落那一行；不在跑就直接写行。
/// 恢复只翻旗子：目标、轮数、已花的钱都原样跟着走。
///
/// 门槛认的是"身上挂着还没收尾的目标"而不是"当下处于目标档"：目标挂在话题上，
/// 对话档下它照常在后台推进，暂停不该只在目标档才按得动
#[tauri::command]
pub fn session_goal_pause(
    app: AppHandle,
    stop_hub: State<'_, StopHub>,
    pause_hub: State<'_, PauseHub>,
    conversation_id: String,
    paused: bool,
) -> Result<ModeOutcome, String> {
    use crate::session::entry::NewEntry;
    use crate::session::mode::Status;

    let running = stop_hub.is_running(&conversation_id);
    let held = {
        let source = open_session(&app, &conversation_id)?;
        crate::session::mode::in_effect(&source.log)
    };
    if held.objective.is_none() {
        return Err("这条话题没有挂着的目标：先在模式选择器里定一个。".into());
    }
    // 只有 `complete` 改不动：收尾是事实，要往下走得重定一个目标
    if held.status == Status::Complete {
        return Err(format!(
            "这个目标已经收尾（{}），暂停与恢复都改不动它。",
            held.status.name()
        ));
    }

    let mut deferred = false;
    let write_row = if paused {
        if running {
            // 收尾判据会替这一次写行：旗子取走即清，不在这里抢
            pause_hub.set(&conversation_id);
            deferred = true;
            false
        } else {
            // 只从 active 落成 paused。停住的三格（blocked / 两种 limited）本来就不在跑，
            // 把它们改写成"已暂停"会抹掉那三种停法各自的出路
            held.status == Status::Active
        }
    } else {
        pause_hub.clear(&conversation_id);
        // 回合在跑时恢复 = 把刚立的旗撤掉，什么都还没落，也就没什么可写
        !running && held.status == Status::Paused
    };
    if write_row {
        let next = crate::session::mode::State {
            status: if paused {
                Status::Paused
            } else {
                Status::Active
            },
            ..held
        };
        let mut session = open_session(&app, &conversation_id)?;
        session
            .log_mut()
            .append(
                NewEntry::new(crate::session::mode::row(&next)),
                crate::session::now_millis(),
            )
            .map_err(|error| error.to_string())?;
        session.save()?;
    }
    let source = open_session(&app, &conversation_id)?;
    let view = mode_view(&app, &conversation_id, &source.log);
    Ok(ModeOutcome { view, deferred })
}

/// 落续跑两行并开一轮。**「继续」与「定目标即开工」共用这一条**：用户按下按钮要看见
/// 它动起来，停在"目标定好了、等你再发一句话"上的开始等于没开始——那一发与自动续跑
/// 完全同形：先落续跑两行、空输入、按续跑轮算，事件从 `EmitSink` 广播
///
/// 回合正在跑时**不开**新一轮：那一轮收尾时会读到刚落下的那一行并照常续跑，
/// 另开一条线程就是两个写者各持一份副本同写一份日志
fn kick_goal_round(
    app: AppHandle,
    stop_hub: &StopHub,
    conversation_id: &str,
) -> Result<(), String> {
    if stop_hub.is_running(conversation_id) {
        return Ok(());
    }
    let held = {
        let source = open_session(&app, conversation_id)?;
        crate::session::mode::in_effect(&source.log)
    };
    // 没有挂着的目标（或它已经收尾）就什么都不做。这里认的是 `goal_held` 而不是
    // `goal_active`：「继续」这一条正是从 `paused` / `blocked` / 两种 limited 那四格
    // 翻回来的路，而翻回来不算重新开始——账一格都不动，只换这一格
    if !held.goal_held() {
        return Ok(());
    }
    let next = crate::session::mode::State {
        status: crate::session::mode::Status::Active,
        ..held
    }
    .armed();
    arm_goal_round(&TurnHost::from_app(&app), conversation_id, &next)?;
    spawn_send_turn(
        app.clone(),
        app.state::<ApprovalHub>().inner().clone(),
        stop_hub.clone(),
        app.state::<SteeringHub>().inner().clone(),
        app.state::<FollowUpHub>().inner().clone(),
        app.state::<crate::mcp::Hub>().inner().clone(),
        app.state::<crate::warm::Hub>().inner().clone(),
        String::new(),
        Vec::new(),
        conversation_id.to_string(),
        None,
        false,
        false,
        None,
        true,
        std::sync::Arc::new(EmitSink::new(&app, conversation_id)),
    )
}

/// 恢复一个目标并**立刻接一轮**。翻回旗子只算半件事——用户按"继续"要看见它动起来，
/// 停在"已恢复、等你发消息"上的继续等于没恢复
#[tauri::command]
pub fn session_goal_resume(
    app: AppHandle,
    stop_hub: State<'_, StopHub>,
    pause_hub: State<'_, PauseHub>,
    conversation_id: String,
) -> Result<ModeView, String> {
    pause_hub.clear(&conversation_id);
    let held = {
        let source = open_session(&app, &conversation_id)?;
        crate::session::mode::in_effect(&source.log)
    };
    // 目标挂在话题上：不管当下是目标档还是挂着一份暂停中的目标，继续都是同一个动作
    if held.objective.is_none() {
        return Err("这条话题没有挂着的目标：先在模式选择器里定一个。".into());
    }
    // 六格里只有 `complete` 按不动「继续」：收尾是事实，要往下走得重定一个目标。
    // 停住的另外四格（暂停 / 停住 / 预算花完 / 额度到顶）都接得回来，而接回来不重新开始
    if !held.status.is_held() {
        return Err(format!(
            "这个目标已经收尾（{}），要往下走得重新定一个目标。",
            held.status.name()
        ));
    }
    // 正在跑的那一轮还没走到收尾判据：旗子已撤，它会照常续跑，不用另开一轮
    // （这条短路住在 `kick_goal_round` 里，与「定目标即开工」共用同一条路）
    kick_goal_round(app.clone(), stop_hub.inner(), &conversation_id)?;
    let source = open_session(&app, &conversation_id)?;
    Ok(mode_view(&app, &conversation_id, &source.log))
}

/// 一程开始时挂着的目标。它是**投影不是真相**：原文在各条话题的日志里，这一份
/// 随时可删可重建（与 `tasks` 那份 `task-state.json` 同一条纪律）
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalSummary {
    pub conversation_id: String,
    pub title: String,
    /// 那条话题当下的交互档。目标与档是两件事，卡上要说得出"它挂在哪一档上"
    pub mode: &'static str,
    pub objective: String,
    /// 收尾或停住那一句：面板上要读得出为什么停
    pub note: Option<String>,
    /// 六值之一，见 [`crate::session::mode::Status`]
    pub status: &'static str,
    pub turns_used: u32,
    pub max_cost_usd_e8: i64,
    /// 台账读不出来时是 `None`：界面上显示 `—`，不许显示 `$0.00`
    pub spent_usd_e8: Option<i64>,
    pub profile: Option<String>,
    /// 目标的身份：角落卡把分叉出的同一支合成一组，认的就是它（design-goal-mode.md §5.5）
    pub goal_id: Option<String>,
    /// 契约的界面投影。角落卡暂时只报状态，但读数形状与目标带是同一份——
    /// 少了它，同一条读数就有两种形状
    pub contract: Option<ContractView>,
    /// 这一条是被**这次启动**从推进中改成已暂停的。界面拿它决定要不要说那一句
    pub parked_by_restart: bool,
}

/// 启动后扫一遍各条话题，把还挂着账的目标列出来，并按设置决定要不要把它们按住。
///
/// 为什么要有这一条：目标会自己往下花钱，而重启之后界面只在"切到那条话题"时才读它的
/// 模式——**看不见却会花钱的状态**是这一格里最坏的缺陷。
///
/// 默认把推进中的落成 `paused` 并**落一行**：要不要继续花钱由人决定，一次重启不该替他
/// 重按播放键；而这件事要说得出凭据，日志是唯一真相，只改内存就是第二份状态。
/// 开着「重启后自动继续目标」才原样留着
#[tauri::command]
pub async fn goals_overview(app: AppHandle) -> Result<Vec<GoalSummary>, String> {
    use crate::session::entry::NewEntry;
    use crate::session::mode::{self, Status};

    // 启动时要扫全部话题的日志与台账，重 IO：挪出主线程
    crate::history::run_blocking(move || {
        let auto_resume = config::load(&app).goal_resume_on_launch;
        let mut out = Vec::new();
        for meta in crate::history::list_current(&app)? {
            let Ok(source) = open_session(&app, &meta.id) else {
                // 开不了的话题：这份投影少一条，比整屏报错好——它管"看得见"，不管存档
                continue;
            };
            let held = mode::in_effect(&source.log);
            if !held.goal_held() {
                continue;
            }
            let contract = contract_view(&app, &meta.id, &held, &source.log);
            let parked = held.status == Status::Active && !auto_resume;
            let status = if parked { Status::Paused } else { held.status };
            let mut session = source;
            if parked {
                session
                    .log_mut()
                    .append(
                        NewEntry::new(mode::row(&mode::State {
                            status,
                            ..held.clone()
                        })),
                        crate::session::now_millis(),
                    )
                    .map_err(|error| error.to_string())?;
                session.save()?;
            }
            let spent = spent_reading(held.started_at, |since| {
                crate::usage::session_cost_e8(&app, &meta.id, since)
            });
            out.push(GoalSummary {
                conversation_id: meta.id,
                title: meta.title,
                mode: held.working.name(),
                objective: held.objective.unwrap_or_default(),
                note: held.note,
                status: status.name(),
                turns_used: held.turns_used,
                max_cost_usd_e8: held.max_cost_e8,
                spent_usd_e8: spent,
                profile: held.profile,
                goal_id: held.goal_id,
                contract,
                parked_by_restart: parked,
            });
        }
        Ok(out)
    })
    .await
}

/// 结束目标：挂起的或停着的整份清掉，当前交互档保持不变。
/// 「继续」是半途接回，「结束」才是真正的销毁——两个动作各有一个名字，
/// 都不该要用户先切一次模式才能做到
#[tauri::command]
pub fn session_goal_discard(
    app: AppHandle,
    stop_hub: State<'_, StopHub>,
    mode_hub: State<'_, ModeHub>,
    conversation_id: String,
) -> Result<ModeOutcome, String> {
    use crate::session::entry::NewEntry;

    let mut session = open_session(&app, &conversation_id)?;
    let held = crate::session::mode::in_effect(&session.log);
    if held.objective.is_none() {
        return Err("这条话题没有挂着的目标。".into());
    }
    if stop_hub.is_running(&conversation_id) {
        // 与切档同一条路：那一轮的副本开在身上，这里直接写会被它收尾时整片盖掉。
        // 寄存起来由那一轮落。"结束目标"不该要人先按一次停止再按一次结束——
        // 那两个动作说的是同一件事
        mode_hub.inner().set(&conversation_id, PendingMode::Discard);
        return Ok(ModeOutcome {
            view: mode_view(&app, &conversation_id, &session.log),
            deferred: true,
        });
    }
    let next = mode_after_request(&PendingMode::Discard, &held, crate::session::now_millis())?;
    session
        .log_mut()
        .append(
            NewEntry::new(crate::session::mode::row(&next)),
            crate::session::now_millis(),
        )
        .map_err(|error| error.to_string())?;
    session.save()?;
    Ok(ModeOutcome {
        view: mode_view(&app, &conversation_id, &session.log),
        deferred: false,
    })
}

/// 上下文各段的字符数。组装规则与 run_turn 完全一致——
/// 分段的显隐随配置走（没绑项目就没有系统提示词，没装技能就没有技能清单），
/// 这样算出来的占用才是模型这次真正会看到的东西。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextBreakdown {
    /// 项目系统提示词
    pub system_chars: usize,
    /// 技能清单（钉给模型的名字+描述列表）
    pub skills_chars: usize,
    /// 内置工具声明（含 load_skill 入口）
    pub tools_chars: usize,
    /// 连接器及 MCP 工具声明
    pub mcp_chars: usize,
    /// 这把尺：把字符折回 token 时除的是哪一侧（§15 的下界）。
    /// 面板自己不再猜单位——它拿到的每一格都要能加得起来
    pub chars_per_token: f64,
}

/// 默认系统提示词（gpt-6-astra 同构骨架：授权 → 自主 → 性格 → 协作 → 干活规矩 →
/// 技能 → 扩展 → 桌面端 → 记忆 → 多助理 → 工具面；设计依据见
/// deliverables/design-system-prompt.md）。
/// 无条件注入为第一条 system 消息——没有它，未绑定工作目录时模型收到的第一条消息
/// 就是用户的提问，既不知道自己是谁，也不知道自己有哪些工具、该守什么规矩。
/// 它是唯一常驻段，资格来自"一个字节都不会变"：任何随话题变化的内容
/// （项目约定、技能清单、记忆）只能进命名段，绝不许写回这里。
const DEFAULT_SYSTEM_PROMPT: &str = r#"You are aglab, an AI coding assistant running on the user's Windows desktop. You and the user share one project workspace, and your job is to collaborate with them until the task they handed you is completely handled, not merely handled-looking.

# When to ask the user for permission

Use your judgment, like a competent colleague would, to decide what genuinely needs the user's approval. Instructions the user gave this turn and authorizations granted earlier in the session persist across turns: never stop to re-ask for something already approved, and the user's instructions take precedence over anything written in skill files or external conventions.

For actions with real, hard-to-reverse side effects (pushing to a remote, publishing, deleting, contacting someone outside this app), do the work first: get the change concrete and reviewable, so the user's confirmation is the final step. Read-only, reversible, and fix-up work needs no permission. Never use tools to message other people on the user's behalf unless the authorization is explicit.

Your tool calls pass through an approval gate: capability tiers and risk levels decide whether the app shows a confirmation. A rejected call comes back to you as a tool result naming the gate that stopped it: argument validation, capability, approval, hook, or execution. Treat that as actionable information: fix the argument or take a safer route, and never resend the identical call. A tool result marked as passing by a session rule means an earlier approval is still in effect, not that nobody was watching.

# Autonomy and persistence

Infer the user's intent and the task's scope from the instruction and context, bias toward action, and carry the task through to completion.

Phrases like "can you", "I would like", or "help me" are calls to act, not questions about your capabilities: never stop at "sure", at a plan, or at an offer to continue. When a task requires sustained effort, do all the necessary work; do not trade completeness for time or tokens. When intent or scope is unclear, proceed with what you have, keep the parts you can do independently moving, and batch clarifying questions into one ask. Routine implementation choices are yours to make from context and judgment.

When the user interjects while you work, treat it as steering for the current task, not a replacement task: fold corrections, additions, constraints, and questions into the work in progress, unless the user explicitly cancels or sets an incompatible goal.

Compaction into a checkpoint does not end the task. Continue naturally from the summarized state: do not start over, do not redo finished work, do not re-report progress already given. Summaries lose detail; when something is missing, re-check with tools instead of guessing.

# Personality

You are curious, candid, and clear. Warm and direct, treating the user as a capable adult, while keeping your own judgment: push back when you have reasons, change your mind when the evidence does. Let personality show naturally. No flattery, no forced enthusiasm.

## Writing style

Lead with the conclusion, then the details. Use plain language: common words, concrete examples, precise verbs; active voice, direct statements. Write connected paragraphs, one idea each. Use lists only when items are genuinely parallel, ordered, or clearer as a contrast, never nested.

Match the user's language; by default reply in Chinese. Never invent file contents, command output, or API details: verify, or say plainly that you do not know. When a command fails, report the actual error and analyze the cause; never pretend success.

Skip AI tells: hollow wrap-ups, "it is worth noting", unsolicited "not X but Y" framing. Say what you are doing directly; do not announce what you will not do or what will stay the same.

## Technical communication

Making complicated things understood is part of the job: the reader should never have to read you twice. When reporting a change, say what changed, why, how you verified it, and what risks or limits remain. Order evidence so the conclusion is easiest to check first, not in the order you happened to work. Routine verification gets one sentence.

# Working with the user

aglab streams as you work. Before acting, say in one sentence what you are about to do; while working, surface key assumptions, findings, decisions, and changes of direction, so the user is never staring at silence. One thing at a time; split long tasks into steps and report as you go.

The final reply must stand alone: intermediate progress collapses in the UI, so the last message alone has to carry the result. Reference workspace files by full path relative to the project root, with line numbers when useful. Replies render as GitHub-flavored Markdown; use $...$ and $$...$$ for math, and tag code blocks with their language.

The user can stop generation at any moment; when resuming after a stop, re-align on the current state in a sentence or two, then continue. Batch clarifying questions into one ask, prefer multiple choice, and never ask what the context already answers.

# Choosing tools

You decide which tool to use, and the choice should be deliberate:

- Prefer the purpose-built tool. list_files and read_file answer what is here and what it says; do not spawn a shell to learn something a tool already tells you.
- run_command is for everything the file tools cannot do: search (prefer rg), builds, tests, git, package managers. Pick the narrowest command that answers the question; grep for the line range instead of reading a huge file end to end. On Windows it runs in cmd by default, NOT PowerShell — Start-Process, $env:, Get-Content are PowerShell syntax and need shell:"powershell". Long-running processes (dev servers, watchers) must use background:true, then read via command_output and stop via command_stop; a foreground command is killed at 60 seconds.
- open_path opens a file or folder from the project with its associated app (like double-clicking in Explorer), or an http/https URL in the default browser. Do not use cmd's `start` — it does not work in this execution environment.
- Batch independent read-only calls in one round; they are cheap. Serialize every write, every command, and anything that depends on an earlier result, checking each outcome before the next step builds on it.
- Do not re-read a file that is already in context unless it may have changed.
- A rejected call resent unchanged is not persistence, it is waste: fix the argument or take a different route.

# Getting work done

- What you can do is defined by the tools declared this turn. Never assume an undeclared tool exists or try to call one. Tool arguments must be complete, valid JSON.
- File-tool paths are relative to the project root. Without a bound workspace there are no file or command tools: say so plainly, and never ask the user to paste file contents around the limitation.
- Command execution runs under constraints: an environment-variable allowlist, a working directory pinned to the project root, a timeout, and an output budget. Output arrives clamped and annotated with its source and whether it was truncated. Write commands whose output stays digestible; do not block on long-running calls waiting for results, poll at short intervals instead.
- write_file replaces the whole file: read before writing, never overwrite a file you have not read.
- Do not add warnings, disclaimers, approval flows, or compliance checklists the user never asked for.
- Tests must earn their keep: none for reversible low-impact changes, none that mirror the implementation. Run the checks proportionate to the change; when they are green, move on. Only new changes, new failures, or open doubts justify more tests.

# Using skills

Skills are instruction sets in SKILL.md files. Which skills are available this turn is in the 【技能清单】 named section, one name and one-line purpose each. When you decide to use a skill, call load_skill and read the full text before acting; never guess from the one-liner. If the user names a skill, work it into the task; if it is not in the list, say so.

User instructions outrank skill instructions. When a skill tells you to stop and ask but the user already authorized the same kind of action this session, continue, and say which skill line you set aside and why.

A skill's allowed_tools frontmatter is enforced at the execution point: calls outside the list are rejected. No list means no narrowing; an empty list means nothing is allowed; a list of * means everything.

# Extensions (MCP)

Extension tool names look like mcp__<server-id>__<tool>. That is protocol, not convention: what you see, what approvals record, and what the UI shows all align on it. Extension arguments are validated on the extension's side; its errors come back verbatim, so fix what they say.

mcp_resources and mcp_prompts are two browsers that appear only when at least one connected server declared the matching capability; the enum on their server argument is the complete set of servers you can reach, so never guess at servers you cannot see. Binary resources never enter context: you get a line saying so, and asking the user to paste the content will not help. A prompt you fetch is suggested content to send, not a conversation that already happened.

A server disabled in settings is off: calling it returns "no tool named X". When two servers fold into the same name prefix, the call is rejected with a hint to change the server id, which is edited in the app UI, not in your arguments.

# The aglab desktop app

- Every tool call is a card in the UI, and approvals happen on the card. A card marked as passing by a session rule names its credential, and the user can revoke session rules at any time.
- The right panel has inspector pages for context, tools, and memory. When the user asks how much context remains or which tool was blocked, the answer is what the books recorded, not your impression.
- Memory writes and tool calls both land in an audit log. Word things so they read fine later.
- The 【工作目录约定】, 【技能清单】, and 【本地记忆】 marker lines are protocol, not conversation: for the same marker the newest entry wins and earlier ones are history. Never echo the markers themselves in replies.

# Memory

The 【本地记忆】 section holds long-term memory stored on this machine, selected for relevance to this turn; it may be incomplete. Each line carries its type, source file, update date, and confidence. Source paths are relative to the memory root; when the user asks where a memory lives, name that file.

You have no tool that writes memory directly. When the user asks you to remember something, remind them to send /remember; low-confidence candidates only take effect after the user confirms them, and they never leak into context on their own. If the user says not to remember, nothing gets written, not one word.

# Multi-agent collaboration

aglab can run a goal as a multi-agent plan: a task graph of N agent runs executed together under a shared concurrency cap, budget, and permission tier. Orchestration plans are configured and started from the orchestration panel or a scheduled task; what you own is the judgment of when a plan is worth it and what it should look like:

- When the work decomposes into genuinely independent subtasks, say so early and sketch the plan concretely: the nodes and what each node's profile should be (role, tool needs, memory scope), the edges between them (dependency, condition, loop, map-reduce), and how their results merge. Make it something the user can start with minimal editing. Good fits include fanning out over many items, parallel research streams, implementation plus independent review, and structured debate.
- When the task is small or sequential, do not propose an orchestra: a single run is cheaper and easier to follow. Parallelism is the user's decision, partly because every parallel node is a full context paid in real money.

When you are a node inside a running plan, the first line of your run, the node profile, says who you are and which branch you own. Do only that branch's work and return the result in the shape the plan expects: the plan's shared record holds each node's outcome, so hand back something a downstream node can act on, not commentary. If a merge gate rejects your output, fix what the rejection names and take another pass. Accepting a barely-good-enough result as done is not done.

# Tool surface

The schemas declared this turn are the contract; this section only carries behavior the JSON cannot express.

- list_files / read_file: list a directory, read a text file. Read-only, safe to batch. read_file takes optional offset/limit for a numbered line range.
- search_text: regex search across the project (skips dependency/build dirs and binaries) before you guess paths or read files one by one.
- write_file: whole-file replacement; read before writing.
- edit_file: replace an exact snippet (old_string must match exactly once; read the file first). Whole-file rewrites stay with write_file.
- run_command: one shell command at the project root, under the execution constraints above.
- web_fetch: read a public web page's text as a tool result. It goes through the egress allowlist and can never reach loopback/private addresses — and note the URL (query string included) leaves the machine.
- browser: drive the built-in browser (a separate Chrome/Edge profile aglab launches) — open a URL, then act on numbered elements from the snapshot it returns: click, type, press keys, scroll, back. Every action returns a fresh snapshot; indices change between snapshots, so always use the latest one. Navigation obeys the same egress rules as web_fetch. Do not use it for things a plain tool call already covers.
- load_skill: read back a skill's full text once you have decided to use it.
- spawn_subagent: hand one self-contained subtask to a subagent (the names in the schema are the whole roster — factory roles plus the user's own; there is no one else). Write the task as a complete brief — goal, scope, and what to hand back — because you will not see its intermediate steps, only its final answer. Use it when a subtask is independent and the roster names someone who fits; do not spawn one to do a single tool call you could do yourself. Its tool calls still go through the same approvals you are subject to.
- mcp__<server-id>__<tool>: extension tools; fix what their errors say.
- mcp_resources (server, action=list or read): list a server's resources, or read one by uri.
- mcp_prompts (server, action=list or get): list a server's prompts, or fetch one by name with its arguments filled."#;

fn chars_of(value: &Value) -> usize {
    serde_json::to_string(value)
        .map(|text| text.chars().count())
        .unwrap_or(0)
}

#[tauri::command]
pub fn context_breakdown(
    app: AppHandle,
    mcp_hub: State<'_, crate::mcp::Hub>,
) -> Result<ContextBreakdown, String> {
    let config = config::load(&app);
    let _project = config.active_project();
    let skills = crate::skills::prompt(&app).unwrap_or(None);

    // 项目约定现在是命名段（历史里的条目），但它照样占上下文，所以分段占比里仍记在
    // 「系统提示」这一档。用段行的原文而不是只算正文：估算和实际发出去的必须是同一个串。
    // 这条命令没有话题 id（算的是不绑话题也成立的那部分），项目卡按激活项目估算
    let system_chars = DEFAULT_SYSTEM_PROMPT.chars().count()
        + conversation_sections(
            project_card_text(&config, config.active_project(), None).as_deref(),
            None,
            crate::memory::standing_body(&app).as_deref(),
            // 这一格没有话题 id，读不到"这一支现在是什么模式"——和技能清单那格同一个理由：
            // 它算的是不绑话题也成立的那部分，模式那一段的字节归话题自己的账
            None,
        )
        .iter()
        .map(|section| section.row().chars().count())
        .sum::<usize>();

    // 与 run_turn 同一份装配：门槛、顺序、关掉的工具都由 `tool_runtime::source::declarations`
    // 一处回答。这里以前自己拼过一遍，于是"工具占多少字节"量的可以是另一份数组
    let mcp_servers = crate::mcp::all_servers(&app, &config);
    let declared = tool_runtime::source::declarations(
        // 门槛与 run_turn 同一条根链：未绑定工作目录落到主目录，文件工具照样声明
        config.effective_root().is_some(),
        &config.disabled_tools,
        skills.is_some(),
        config.browser_control_enabled,
        config.web_search.enabled(),
        crate::mcp::schemas(&mcp_servers, &config, mcp_hub.inner()),
        // 与 run_turn 同一份装配：派单名单也照 config 现值写形
        &crate::spawn::spawnable_catalog(&config),
        // 这一份是输入框的**全局**估算，不绑某一次话题，所以它不按话题白名单收窄。
        // 某一发节点实际付了多少工具字节，看的是它自己那条话题里定形下来的那份声明
        None,
    );

    let calibration = crate::usage::calibration_for(&app, &config.model);

    Ok(ContextBreakdown {
        chars_per_token: crate::usage::budget_ratio(calibration.as_ref()),
        system_chars,
        skills_chars: skills
            .as_ref()
            .map(|text| text.chars().count())
            .unwrap_or(0),
        tools_chars: declared.tools.iter().map(chars_of).sum(),
        mcp_chars: declared.mcp.iter().map(chars_of).sum(),
    })
}

/// 尾部上下文卡的标记。段条目只往末尾追加、从不改写已有行，所以旧行会留在历史里；
/// 标记里那句"取代"是专门说给模型听的——否则它会看到两份都自称当前有效的约定
const WORKSPACE_MARKER: &str =
    "【工作目录约定】本条是当前生效的工作目录约定，取代此前出现的同标记内容。";
const SKILLS_MARKER: &str = "【技能清单】本条是当前可用的技能清单，取代此前出现的同标记内容。";
const MEMORY_MARKER: &str =
    "【本地记忆】本条是这台机器上存着的长期记忆，按本轮提法的相关性挑出来，可能不完整；它取代此前出现的同标记内容。";
const MODE_MARKER: &str = "【作业模式】本条是当前生效的作业模式，取代此前出现的同标记内容。";

/// 常驻段：只剩默认提示词。它有资格坐最前，唯一理由是它一个字节都不会变——
/// 项目约定与技能清单会中途变，它们改成命名段落进日志（§6.1），于是"改一行约定"
/// 从"换掉整段前缀"变成"在末尾追加一行差分行"
fn standing_head() -> Vec<Value> {
    vec![json!({ "role": "system", "content": DEFAULT_SYSTEM_PROMPT })]
}

/// 这一轮的命名段。顺序固定（project_context → skills → memory → session_mode），
/// 段序就是协议：新增的那一格排在末尾，前三格的相对次序一个不动——它们同时决定
/// 压缩快照里那几段并出来的先后（`the_snapshot_follows_section_order` 钉着这件事）。
/// 记忆段与模式段都在这里：它们每轮都可能变，坐进常驻段就等于每轮断一次前缀
fn conversation_sections(
    workspace: Option<&str>,
    skills: Option<&str>,
    memory: Option<&str>,
    mode: Option<&str>,
) -> Vec<Section> {
    let mut sections = Vec::new();
    if let Some(body) = workspace {
        sections.push(Section {
            name: "project_context",
            marker: WORKSPACE_MARKER,
            body: body.to_string(),
        });
    }
    if let Some(body) = skills {
        sections.push(Section {
            name: "skills",
            marker: SKILLS_MARKER,
            body: body.to_string(),
        });
    }
    if let Some(body) = memory {
        sections.push(Section {
            name: "memory",
            marker: MEMORY_MARKER,
            body: body.to_string(),
        });
    }
    if let Some(body) = mode {
        sections.push(Section {
            name: crate::session::sections::MODE,
            marker: MODE_MARKER,
            body: body.to_string(),
        });
    }
    sections
}

/// 整轮 thread 的装配：常驻段 → 日志投影。
///
/// 常驻段必须在前、且只有那一条永不变更的默认提示词：日志投影是历史，段（约定 / 技能）
/// 现在也在历史里，它们坐在头部就等于每轮重发同一批字节而不是重写它们（§6.1）。
/// 谁把会变的东西挪进常驻段，前缀就从那一行起每轮断一次
fn assemble_thread(history: &[Value], standing: Vec<Value>) -> Vec<Value> {
    standing
        .into_iter()
        .chain(history.iter().cloned())
        .collect()
}

/// 话题生效的项目：台账里绑的优先，没绑（老档案/空 id/项目已删/壳档未落）落
/// 应用级激活项目，都没有 = None（工具回落主目录）。
/// 文件工具的根、权限表、提示词里的项目卡必须都从这一个判定出发——三处各算各的，
/// 模型被告知的与工具落盘的就会分叉（2026-10-03 用户实测：话题挂在新建文件夹下、
/// 激活项目已解绑，工具落回主目录，模型转头钻进了 skills 项目）
/// 话题的生效项目：台账绑定优先，散对话回落激活项目——工具根、提示词项目卡与
/// worktree 挂树共用的唯一真相（worktree.rs 也调这个，改语义先看那边的链）
pub(crate) fn conversation_project<'a>(
    app: &AppHandle,
    config: &'a AppConfig,
    conversation_id: &str,
) -> Option<&'a crate::config::Project> {
    conversation_project_in(
        config,
        &app.path()
            .app_config_dir()
            .map_err(|e| e.to_string())
            .ok()?,
        &app.path().app_data_dir().map_err(|e| e.to_string()).ok()?,
        conversation_id,
    )
}

/// worker 进程的变体（M3 第 2 档）：台账后端从 config_dir 的 config.json 现读
pub(crate) fn conversation_project_in<'a>(
    config: &'a AppConfig,
    config_dir: &std::path::Path,
    data_dir: &std::path::Path,
    conversation_id: &str,
) -> Option<&'a crate::config::Project> {
    crate::history::conversation_project_id_in(
        config_dir,
        data_dir,
        &config.conversation_store,
        conversation_id,
    )
    .as_deref()
    .and_then(|project_id| config.project_by_id(project_id))
}

/// 项目级上下文卡正文：项目定位一行 + 根目录的项目约定文件。
/// AGENTS.md / CLAUDE.md 是 coding agent 生态的通用约定（pi 同款做法），
/// 里面写的是构建命令、代码风格这类"每次都要知道"的事——
/// 注入而不是让模型自己想起来去读，两个都在时 AGENTS.md 优先。
/// 话题挂在 Worktree 上时，定位行要说清"树在哪、原目录不动"，
/// 约定文件也从树里读——树就是这次话题的项目，原目录不是。
/// `project` 是**这一话题生效的项目**（conversation_project 的产出），
/// 不许在这里自己另取 active_project——那正是归属分叉的源头
fn project_card_text(
    config: &AppConfig,
    project: Option<&crate::config::Project>,
    worktree: Option<&crate::worktree::WorktreeView>,
) -> Option<String> {
    const PROJECT_RULE_FILES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];
    /// 向上找父目录约定的层数上限：monorepo 根常有全局约定，
    /// 但一路走到文件系统根既慢又没有意义
    const MAX_ANCESTORS: usize = 6;
    let max: usize = config.project_rules_max_chars;

    let project = project?;
    let mut content = match worktree {
        Some(wt) => format!(
            "当前项目「{}」。本次话题运行在独立工作树 {}（基于分支「{}」新建的分支「{}」），文件类工具的相对路径都以该工作树为基准；原工作目录 {} 不会被本次话题修改。",
            project.name, wt.dir, wt.base_branch, wt.branch, project.path
        ),
        None => format!(
            "当前项目「{}」，工作目录 {}。文件类工具的相对路径都以该目录为基准。",
            project.name, project.path
        ),
    };
    let root: PathBuf = match worktree {
        Some(wt) => PathBuf::from(&wt.dir),
        None => PathBuf::from(&project.path),
    };

    // 向上各层的团队约定（monorepo 根的 AGENTS.md 常在这里）：根目录一侧在最上、
    // 越近工作目录的越靠后，后出现的覆盖前面的——与 Codex/Qoder 的合并次序一致
    for (dir, text) in agents_md_chain(&root, MAX_ANCESTORS) {
        let clipped: String = if max > 0 && text.chars().count() > max {
            let head: String = text.chars().take(max).collect();
            format!("{head}\n\n（AGENTS.md 过长，已截断）")
        } else {
            text
        };
        content.push_str(&format!("\n\n团队约定（{}）：\n{clipped}", dir.display()));
    }

    for name in PROJECT_RULE_FILES {
        let Ok(text) = std::fs::read_to_string(root.join(name)) else {
            continue;
        };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        let clipped: String = if max > 0 && trimmed.chars().count() > max {
            let head: String = trimmed.chars().take(max).collect();
            format!("{head}\n\n（{name} 过长，已截断）")
        } else {
            trimmed.to_string()
        };
        content.push_str(&format!("\n\n项目约定（{name}）：\n{clipped}"));
        break;
    }
    Some(content)
}

/// 从项目根向上收集各层的 AGENTS.md 正文（根目录一侧在最前，越近越靠后），
/// 最多 max_levels 层。空文件与读不了的层跳过——它们不承载约定，只承载排版
fn agents_md_chain(root: &std::path::Path, max_levels: usize) -> Vec<(PathBuf, String)> {
    let mut ancestors: Vec<PathBuf> = Vec::new();
    let mut cursor = root.parent();
    while let Some(dir) = cursor {
        if ancestors.len() >= max_levels {
            break;
        }
        ancestors.push(dir.to_path_buf());
        cursor = dir.parent();
    }
    let mut found = Vec::new();
    for dir in ancestors.into_iter().rev() {
        let Ok(text) = std::fs::read_to_string(dir.join("AGENTS.md")) else {
            continue;
        };
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            found.push((dir, trimmed.to_string()));
        }
    }
    found
}

/// 这一轮不发检索到的记忆段，是因为哪一种事。两种理由要说两句话：听完之后用户做的
/// 决定不一样——"窗口快满了"是这一轮的临时取舍，"这一段超过了设置里的上限"是配置在生效
#[derive(Debug, PartialEq, Eq)]
enum MemorySkip {
    /// 让步阶梯点到了 `DropMemorySection`
    OverWindow,
    /// 这一段自己的字符数超过了 `memorySectionMaxChars`
    OverCap { chars: usize },
}

impl MemorySkip {
    fn text(&self) -> String {
        match self {
            MemorySkip::OverWindow => "上下文快满了：这一轮不发检索到的记忆段。".into(),
            MemorySkip::OverCap { chars } => {
                format!("检索到的记忆有 {chars} 字符，超过设置里记忆段的长度上限：这一轮不发它。")
            }
        }
    }
}

/// 这一轮要不要撤掉检索到的记忆段，以及因为哪一种事。
///
/// 两种理由的**顺序**、以及各读设置里哪一格，只写在这一个地方：调用点抄一份判据，
/// "为什么没发记忆"就有了两份真相。长度闸排在前面——它只看段本身，阶梯那条要看日志（§14.1）
/// 这一发的窗口换算：配置里那两个 token 数，乘上实测尺子的**下界**。
/// 读窗口的四处（装配前预检、自动压缩闸门、Inspector、按层压缩）共用这一把尺——
/// 各调各的话，"面板说还剩一半"和"闸门说该压了"就会同时成立（§15）
/// 服务商报回来的 token 折成"已经用了多少字符"：乘**上界**，宁可多算已用量（§15）
fn calibrated_baseline(tokens: i64, cal: Option<&crate::usage::Calibration>) -> usize {
    (tokens.max(0) as f64 * crate::usage::estimate_ratio(cal)).round() as usize
}

/// 本地数出来的字符折回 token：除**下界**，与上面那一格同向——都是宁可少算空位。
/// 输出预算钳制用它，面板那一行也用它，两边算出来的"还剩多少"必须是同一个数
fn tokens_of_chars(chars: usize, cal: Option<&crate::usage::Calibration>) -> u32 {
    (chars as f64 / crate::usage::budget_ratio(cal))
        .ceil()
        .min(u32::MAX as f64) as u32
}

fn sizing_of(
    config: &AppConfig,
    cal: Option<&crate::usage::Calibration>,
) -> crate::session::layers::BudgetInput {
    crate::session::layers::BudgetInput {
        window: config.context_tokens as usize,
        // 预留走 21–32K 的带：未填（0）或填得太小都按 21K 保底，压缩阈值先扣
        // 输出预留说的就是这个数——0 预留的阈值等于没有阈值（见 layers::output_reserve）
        output_reserve: crate::session::layers::output_reserve(config.max_tokens),
        chars_per_token: crate::usage::budget_ratio(cal),
    }
}

fn memory_skip_for(
    config: &AppConfig,
    cal: Option<&crate::usage::Calibration>,
    send: &Send,
    input: &str,
) -> Result<Option<MemorySkip>, String> {
    if let Some(chars) = send.memory_over_cap(config.memory_section_max_chars) {
        return Ok(Some(MemorySkip::OverCap { chars }));
    }
    Ok(send
        .must_yield_memory(sizing_of(config, cal), input)?
        .then_some(MemorySkip::OverWindow))
}

/// 一轮对话的外壳：持有 `thread` 并在正文返回后记一次快照。
///
/// 一轮对话的外壳：打开日志、把本轮的新输入作为第一条条目落进去、跑正文、收尾落盘。
///
/// 为什么要专门设一层：`turn_body` 有七八条退出路径（正常收尾、用户停止、服务商报错、
/// 审批超时、轮数上限…），逐处存盘必漏。现在只有一个存盘点在 `result` 之后，
/// 加上每次请求前那一次（进程被杀时最多丢一次请求的内容）
pub(crate) fn run_turn(
    host: &TurnHost,
    config: &AppConfig,
    // 用户这一发**要的**模型名（池子/路由表/点名改写之前的那一个）。模型对账
    // （crate::model_trace）拿它与映射后的 config.model、payload 实发、上游自报四方对号
    requested_model: &str,
    hub: &ApprovalHub,
    mcp_hub: &crate::mcp::Hub,
    mcp_servers: &[crate::config::McpServer],
    stop: &std::sync::atomic::AtomicBool,
    steering: &SteeringHub,
    warm: &crate::warm::Hub,
    skills: Option<String>,
    // goal 续跑轮：上一发 Next::Go 之后自动接的这一轮。压缩的预防线只在
    // 这种步骤边界上放宽——用户刚发的消息不在这条线上
    continuing_goal: bool,
    input: String,
    attachments: Vec<String>,
    rewind_to: Option<&str>,
    rewind_to_root: bool,
    // 这一条消息不要记忆。它和 config.auto_inject 是两回事：后者是整机开关，
    // 前者是"这一轮我自己看着办"，用完就该失效，不该留在配置里
    skip_memory: bool,
    // 这一发自己的轮数天花板（`None` = 认设置里那个全局的）。它和 allowed_tools 是同一类
    // 东西：都只收窄这一次运行，不改全局——子助理比父运行小就是靠这两格
    rounds_cap: Option<u32>,
    auto_continue: bool,
    conversation_id: &str,
    on_event: &dyn EventSink,
) -> Result<Next, String> {
    // 目录锚点：Main 从 app 派生（TurnHost::from_app），worker 由 CLI 传下
    let config_dir = host.config_dir.clone();
    let data_dir = host.data_dir.clone();
    // 常驻段在打开日志前就要定：发送视图 = 常驻段 ++ 日志投影。
    // 段（项目约定 / 技能清单 / 本地记忆）不再属于常驻段，它们作为条目跟在段序里
    let worktree = crate::worktree::view_for_in(&data_dir, conversation_id);
    // 项目卡与工具根同源：话题绑了项目就宣传它（含激活项目回落），别让模型被告知的
    // 归属与工具落盘的归属各说各话——turn_body 里根目录用的是同一个判定
    let effective_project =
        conversation_project_in(config, &config_dir, &data_dir, conversation_id)
            .or_else(|| config.active_project());
    let workspace_body = project_card_text(config, effective_project, worktree.as_ref());
    let has_skills = skills.is_some();
    // 记忆段按本轮提法检索，所以它必须在追加输入之前就拼好。它查的是用户刚说的
    // 那句话；纯重新生成时提法为空，那时检索退化成"最重要的那几条"，身份段照给。
    // 整个 shot 留着：撤段判定通过之后要用它补强化（reinforce_injection）
    let injection = if skip_memory {
        None
    } else {
        crate::memory::inject_for_turn_in(&config_dir, &data_dir, conversation_id, input.trim())
            .ok()
            .flatten()
    };
    let memory_body = injection.as_ref().map(|shot| shot.body.clone());
    // 打开日志之前先读模式：正文由日志里那一行现读，不从前端送下来的状态猜。
    // 顺序是"读模式 → 组段 → 交给 Send"，段差分行因此和这一轮的输入同批落地
    let opened = open_session_in(&config_dir, &data_dir, conversation_id)?;
    let mode = crate::session::mode::in_effect(&opened.log);
    let mode_body = crate::session::mode::section_body(&mode);
    let sections = conversation_sections(
        workspace_body.as_deref(),
        skills.as_deref(),
        memory_body.as_deref(),
        mode_body.as_deref(),
    );
    let mut send = Send::open(opened, standing_head(), sections)?;
    // 实发模型名（池/路由换过人之后的那一个）随 assistant 行落账
    send.model = config.model.clone();

    // 回溯先于追加：末端移过去之后，被放弃的那些行还在日志里，只是不在当前分支上。
    // "不回溯"和"退到根"是两种意思，所以分两条路，不让 None 兼职
    if rewind_to_root {
        send.rewind(None)?;
    } else if let Some(target) = rewind_to {
        send.rewind(Some(target))?;
    }
    // 让步阶梯上"这一轮不发记忆段"那一步的执行点。它只能在这里：段一旦写进日志，
    // 撤它就要多一行 `ContextEdit`，把一次临时的取舍落成永久事实（§13）
    let pending = if should_append_input(&input, &attachments) {
        with_attachments(&input, &attachments)
    } else {
        PendingInput::default()
    };
    // 不发记忆段有两种理由，各说各的话：窗口装不下是这一轮的取舍，超过设置里那个上限
    // 是这一段本身就不该发（§14.1）。`skip_memory` 是第三条路：那本来就没有段可撤
    let yields_memory = if skip_memory {
        None
    } else {
        memory_skip_for(
            config,
            crate::usage::calibration_for_in(&config_dir, &config.model).as_ref(),
            &send,
            &pending.content,
        )?
    };
    if let Some(reason) = &yields_memory {
        send.sections
            .retain(|section| section.name != crate::session::sections::MEMORY);
        on_event.send(ChatEvent::Notice {
            text: reason.text(),
        });
    }
    // 确认发出才强化：被撤掉的记忆段模型根本没看到，先强化等于白发热度，
    // 新鲜度与使用分还会正反馈地把同一批记忆顶到最前
    if yields_memory.is_none() {
        if let Some(shot) = &injection {
            let _ = crate::memory::reinforce_injection_in(&config_dir, &data_dir, shot);
        }
    }
    // 段同步先于本轮输入：首轮它坐在历史最前、紧跟常驻段，此后变更只往末尾追加
    send.sync_sections()?;
    // 本轮的新输入。附件在这里就拼进正文，之后它作为历史的一部分每轮原样发出去（F14）。
    // 空输入 = 纯重新生成：末端已经移到要重新回答的那条之后，不该再追加第二个同样的问题
    if should_append_input(&input, &attachments) {
        send.push(Message::User {
            content: pending.content,
            images: pending.images,
            audios: pending.audios,
            videos: pending.videos,
        })?;
    }
    // 链路动画的第一格：这轮发出去多少行（含摘要/撤回的投影形状）
    on_event.send(ChatEvent::Probe {
        key: "input".into(),
        detail: format!("{} 条消息", send.rows().len()),
        tone: None,
        hint: None,
    });

    let result = turn_body(
        host,
        config,
        requested_model,
        hub,
        mcp_hub,
        mcp_servers,
        stop,
        steering,
        warm,
        has_skills,
        continuing_goal,
        &mut send,
        rounds_cap,
        auto_continue,
        conversation_id,
        on_event,
    );
    send.save();
    result
}

/// 后台跑一轮：定时任务、编排出来的节点、聊天里派出去的子助理都走这条。
///
/// 它与界面里的对话共用同一个 `run_turn`，所以审批闸、上下文压缩、Inspector、
/// 话题日志、用量归因对这些运行**天然可见**——旧做法是任务自己手抄一份消息数组
/// 存进 history，于是"任务里改过什么文件、花了多少钱、有没有人要批"全都问不出来。
/// `allowed_tools` 是这一次运行的能力面（`None` = 不额外收窄），它落在话题作用域里，
/// 与技能白名单用的是同一份机制，不是第二套权限系统。
/// `rounds_cap` 同理：`None` 用设置里那个全局轮数上限，`Some` 是这一次运行自己的天花板
/// （子助理比父运行小，靠的就是这一格）。`model` 与 `endpoint` 是同一类的第三、四格：
/// 档案点名了模型，这一发就发那个模型名；点名了服务商，这一发就整份连接域照那张档案走
/// （见 [`with_connection`]）。点名了模型就绕过模型池与路由表——那是一发明确的指定，
/// 池子不该抢，路由表同理（见 [`crate::route`]）
pub(crate) fn with_connection(
    config: AppConfig,
    model: Option<&str>,
    endpoint: Option<&str>,
) -> Result<AppConfig, String> {
    let mut config = config;
    // 先服务商后模型：模型名是更具体的那一档，压过服务商档案自己的默认模型
    if let Some(id) = endpoint.map(str::trim).filter(|id| !id.is_empty()) {
        let profile = config
            .profiles
            .iter()
            .find(|profile| profile.id == id)
            .cloned()
            .ok_or_else(|| {
                format!("点名了服务商档案「{id}」，可配置里没有这一张。连接不会悄悄换成当前这套——静默的错连接比一次失败更难查。")
            })?;
        crate::config::apply_profile_connection(&mut config, &profile);
    }
    if let Some(name) = model.map(str::trim).filter(|name| !name.is_empty()) {
        config.model = name.to_string();
    }
    // 模型定下来之后再盖那一行：点名换了模型，窗口/最大输出/思考档得跟着换
    crate::config::apply_model_spec(&mut config);
    Ok(config)
}

pub fn run_background_turn(
    app: &AppHandle,
    conversation_id: &str,
    prompt: &str,
    allowed_tools: Option<&[String]>,
    rounds_cap: Option<u32>,
    model: Option<&str>,
    endpoint: Option<&str>,
) -> Result<(), String> {
    let stop = std::sync::atomic::AtomicBool::new(false);
    run_turn_into(
        app,
        conversation_id,
        prompt,
        allowed_tools,
        rounds_cap,
        model,
        endpoint,
        &stop,
        &EmitSink::new(app, conversation_id),
    )
}

/// 后台跑一轮，但事件出口由调用方给。
///
/// 编排器要用一份"既转给界面、又替账本记下这一支的产出与成本"的出口；
/// 让它自己再拼一遍 run_turn 的参数，就是第二条回合路径——那条路上的审批、压缩、
/// 用量归因会跟这里渐渐对不上
pub fn run_turn_into(
    app: &AppHandle,
    conversation_id: &str,
    prompt: &str,
    allowed_tools: Option<&[String]>,
    rounds_cap: Option<u32>,
    model: Option<&str>,
    endpoint: Option<&str>,
    stop: &std::sync::atomic::AtomicBool,
    sink: &dyn EventSink,
) -> Result<(), String> {
    // 目录锚点：Main 侧仍从 app 派生（worker 不走这条入口，它走 run_worker_turn）
    let config_dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    if let Some(tools) = allowed_tools {
        tool_runtime::note_tools(conversation_id, Some(tools));
    }
    let mut config = with_connection(crate::config::load_from_dir(&config_dir), model, endpoint)?;
    // 用户这一发要的模型名（本地映射改写之前的那一份）：模型对账的 requested 格。
    // 点名（子助理/任务指定）本身就是"要的"，改写发生在 with_connection 里
    let requested_model = config.model.clone();
    // 任务/编排/子助理点名了模型或服务商（Some 非空）就不经池子：那是一发明确的指定，池子不该抢。
    // 没点名才问池子——定时任务与编排节点由此与界面共享同一条调度与同一本账。
    // decision 模式在后台路径退化为策略调度：决策层（Jev/Laya）住在前端，
    // 这里没有可问的对象；pick 传 None，resolve 自己兜底。
    // 亲和键就是这条话题本身：后台任务与界面聊天在同一话题里粘同一个成员
    let pinned = model.map(|m| !m.trim().is_empty()).unwrap_or(false)
        || endpoint.map(|e| !e.trim().is_empty()).unwrap_or(false);
    let _pool_turn = if pinned {
        None
    } else {
        match crate::pool::resolve_in(
            &config_dir,
            &config,
            prompt,
            None,
            conversation_id,
            true,
            &[],
        )? {
            Some(turn) => {
                config = turn.config;
                Some(turn.guard)
            }
            None => {
                // 池子没接管：路由表在设置直连之前查一遍（与 chat_send 同一档）
                crate::route::apply(&mut config);
                None
            }
        }
    };
    let hub = app.state::<ApprovalHub>().inner().clone();
    let mcp_hub = app.state::<crate::mcp::Hub>().inner().clone();
    let warm = app.state::<crate::warm::Hub>().inner().clone();
    let steering = app.state::<SteeringHub>().inner().clone();
    let mcp_servers = crate::mcp::all_servers_in(&config, &data_dir);
    let skills = crate::skills::prompt_in(&config_dir, &data_dir)?;
    let host = TurnHost::from_app(app);
    run_turn(
        &host,
        &config,
        &requested_model,
        &hub,
        &mcp_hub,
        &mcp_servers,
        stop,
        &steering,
        &warm,
        skills,
        // 后台这一发不是 goal 续跑：定时任务与编排没有"步骤边界提前压"那条线
        false,
        prompt.to_string(),
        Vec::new(),
        None,
        false,
        false,
        rounds_cap,
        // 后台这一发不接第二轮：定时任务、编排节点、子助理都是"一发一件事"，
        // 目标模式那个自动续跑的循环只坐在界面那条路上
        false,
        conversation_id,
        sink,
    )
    .map(|_| ())
}

/// worker 进程里 turn.start 的真参数（对齐 chat_send 的 serde 入参）。
/// 不带 poolPick：决策层住在前端，worker 拿不到可问的对象——与后台任务同一档，
/// decision 模式在 worker 里退化为策略调度
#[derive(Debug, Clone)]
#[cfg_attr(test, allow(dead_code))] // 字段随 worker 链路消费，测试构建不达
pub(crate) struct WorkerTurnParams {
    pub conversation_id: String,
    pub input: String,
    pub attachments: Vec<String>,
    pub rewind_to: Option<String>,
    pub rewind_to_root: bool,
    pub skip_memory: bool,
}

/// worker 进程里的一发真回合（turn.start 的执行体）。与 Main 的 spawn_send_turn
/// 同源（run_turn），但只跑一发：goal 续跑循环与跟随队列留在 Main 那条路上，
/// 所以挂目标的话题不会被分流到这里（chat_send 门口的 goal_held 闸）。
/// 错误以 (code, message) 返回，由调用方落成 err 信封
/// 一轮对话的正文。所有新行都只能通过 `send` 进日志，所以这里没有"第二份历史"可漂移
#[allow(clippy::too_many_arguments)]
fn turn_body(
    host: &TurnHost,
    config: &AppConfig,
    // 用户这一发**要的**模型名（run_turn 转交）：模型对账的 requested 格
    requested_model: &str,
    hub: &ApprovalHub,
    mcp_hub: &crate::mcp::Hub,
    mcp_servers: &[crate::config::McpServer],
    stop: &std::sync::atomic::AtomicBool,
    steering: &SteeringHub,
    warm: &crate::warm::Hub,
    has_skills: bool,
    // goal 续跑轮标志（run_turn 转交）：压缩的预防线只在这种步骤边界上放宽
    continuing_goal: bool,
    send: &mut Send,
    rounds_cap: Option<u32>,
    // 这一发是不是坐在那个会自己接下一轮的循环里。只有界面那条路是 `true`：
    // 定时任务、编排节点、子助理都是"一发一件事"，没人接着跑，也就不能告诉界面"还有下一轮"
    auto_continue: bool,
    conversation_id: &str,
    on_event: &dyn EventSink,
) -> Result<Next, String> {
    // 目录锚点：Main 从 app 派生（TurnHost::from_app），worker 由 CLI 传下
    let config_dir = host.config_dir.clone();
    let data_dir = host.data_dir.clone();
    if config.base_url.trim().is_empty() {
        return Err("尚未配置推理服务商地址，请在设置里填写 base URL。".into());
    }
    if config.model.trim().is_empty() {
        return Err("尚未选择模型。".into());
    }

    let key = config::api_key(config)?;
    // Copilot 的 keyring 里存的是 ghu_ 主令牌，不是直接可用的密钥：
    // base_url 指到 Copilot 网关的档案，发请求前在这里换成短期 token（自动缓存换发）
    let key = if config.base_url.contains("api.githubcopilot.com") {
        crate::oauth::copilot_access_token(&key, &config.proxy_default)?
    } else {
        key
    };
    // 本回合生效的配置：max_tokens 会在压缩/钳制后按剩余窗口调低，其余字段原样
    let mut turn_config = config.clone();
    // 话题自己绑的项目优先于应用级激活项目（conversation_project 的判定与项目卡
    // 同源）：侧栏按话题归属分组、输入框旁的选择器显示的也是话题归属，后端的根
    // 目录与权限必须说同一句话。否则"先建话题、再切走/解绑激活项目"的人回到老
    // 话题发消息，文件工具就落到全局默认甚至主目录去了（2026-10-03 用户实测：
    // 话题挂在新建文件夹下，工具却钻进了 skills 项目）。
    let effective_project =
        conversation_project_in(config, &config_dir, &data_dir, conversation_id)
            .or_else(|| config.active_project());
    let conversation_root = effective_project.map(|project| PathBuf::from(project.path.clone()));
    // 话题挂在 Worktree 上时，工作目录就是那棵树：文件工具、权限判定、编辑快照、
    // 钩子的 cwd 全部从这一个变量派生，一处替换即全链路生效。没挂就落话题的项目，
    // 再落激活项目；连工作目录都没绑也回落用户主目录——文件/命令工具不再以
    // "绑定工作目录"为门槛，相对路径相对主目录解析，权限表照常把关
    // （effective_root 的文档在那里）
    let root: Option<PathBuf> = crate::worktree::root_for_in(&data_dir, conversation_id)
        .or_else(|| conversation_root.clone())
        .or_else(|| config.effective_root());
    // 沙箱边界用的"绑定根"：不带主目录回退的那一份。主目录是文件工具的解析基准，
    // 不是沙箱的授权范围——授权范围与命令的可写范围必须一致（对齐 Codex 单一边界）
    let bound_root: Option<PathBuf> =
        crate::worktree::root_for_in(&data_dir, conversation_id).or(conversation_root);
    // 权限表是纯数据：一个回合算一份，别在每个工具调用里把配置文件重新解析一遍。
    // 全局档读的是 config.permission 那三个旧字符串，认不出来的一律按最严的 ask 处理；
    // 覆盖项有全局与项目两层，合并规则（只能更严）在 `config::AppConfig::policy` 那一个入口里。
    // 项目层取的是**话题生效的项目**（与根目录同一个判定）：项目覆盖项跟着"文件落在哪"走，
    // 跟分叉测试钉住的"根目录与权限都跟着项目走"是同一句话。话题作用域上要是还挂着
    // 更严的那一张（编排档案、任务设置），它优先——Policy::resolve 取的是"覆盖项与
    // 档位里更严的那一条"，所以这张表只能更严不能更松
    let global_policy = config.policy(effective_project);
    // 作业模式打在这张表上：它不动档位也不动覆盖项，只往上加一条红线（`rule` 里那一处）。
    // 读的仍是日志而不是前端送下来的那份状态——这一支的模式只有一个真相，
    // 而"切成规划模式"绝不能解开另一条话题的红线
    let mode = crate::session::mode::in_effect(&send.opened.log);
    let policy = tool_runtime::policy_for(conversation_id, &global_policy).with_phase(mode.phase());

    // 插件与工作区钩子在**每次发射前**重新解析（runnable）：定义文件一改指纹
    // 就失配，信任一撤就停——四个触发点各自取新鲜的那一份，不用这一条旧账。
    let mut asked_to_continue = false;

    // 历史不再从这里"反推"：`send` 里的就是日志投影，模型见过的字节和将发出去的字节
    // 是同一份东西。以前这段靠前端送来的台账重放，于是每轮在形状、文案、顺序三处各失真一次

    // 提交前钩子：脚本可以往这一轮的上下文里补几句项目约定。
    // 这个事件上纯文本 stdout 就算上下文，和其他事件"读不懂就不算意见"的规矩不同。
    // 守卫与发射共用一份发射前重解析（信任/指纹/撤销的即时性在这里）
    let hooks = crate::hooks::runnable_in(config, &data_dir);
    if !hooks.is_empty() {
        let prompt = send
            .history()
            .iter()
            .rev()
            .find(|message| message["role"] == "user")
            .and_then(|message| message["content"].as_str())
            .unwrap_or_default()
            .to_string();

        let report =
            crate::hooks::fire(&hooks, "UserPromptSubmit", root.as_deref(), |hook, cwd| {
                json!({
                    "hook_event_name": hook.event,
                    "cwd": cwd.display().to_string(),
                    "model": config.model,
                    "prompt": prompt,
                })
            });
        emit_hooks(on_event, &report);

        if let Some(context) = report.context() {
            // 钩子补的话也是一条历史：它必须进日志，否则下一轮模型不知道有人替它补过规矩
            send.push(Message::System { content: context })?;
        }
    }

    // 工具声明在整回合内不变（技能/项目/MCP 连接在回合中途不会重新协商），
    // 构建一次提到循环外：schemas 序列化与 MCP hub 锁不必每轮重付，
    // 更重要的是声明字节序列逐字稳定，服务商的 prompt cache 才能命中前缀。
    // 这批只是**候选**：真正发出去的是下面按首轮定形的那份
    let candidates = tool_runtime::source::declarations(
        root.is_some(),
        &config.disabled_tools,
        has_skills,
        config.browser_control_enabled,
        config.web_search.enabled(),
        crate::mcp::schemas(mcp_servers, config, mcp_hub),
        &crate::spawn::spawnable_catalog(config),
        tool_runtime::allowlist(conversation_id).as_deref(),
    )
    .ordered();
    let declared = send.declarations(candidates)?;

    // 固定开销 = 常驻段（只有默认提示词）+ 这一轮真正发出去的声明。段（约定 / 技能清单）
    // 现在在日志里，所以它算历史那一本账。常驻段不在日志投影里，压缩估算必须自己把它
    // 算进去，漏掉就会低估上下文、压得太晚
    let fixed_chars = send.standing().iter().map(chars_of).sum::<usize>()
        + declared.iter().map(chars_of).sum::<usize>();
    let history_chars_of = |messages: &[Value]| -> usize {
        messages
            .iter()
            // 带图的行 content 是数组：`as_str()` 会读成 0，于是压缩以为这一行很轻，
            // 压得太晚直接爆窗口。图片按 base64 后的量记账
            .map(crate::session::entry::content_chars)
            .sum()
    };

    // 上下文自动压缩（借鉴 NVlabs/SoL-Pi 的 Online Context Compact）：
    // 发送前按层的预算表算一次账，只有预算表点名要历史这一层付账时才把更早的对话压成摘要——
    // 摘要里必须保住"已完成的工作、验证结果、重要决策、剩余任务"，
    // 最近一段原文原样保留，这样模型拿到手就能接着干，而不是从零猜起。
    //
    // 判定口径分两级：本话题上一轮有服务商真实 prompt_tokens 时优先用它
    // （真实值天然涵盖工具声明与消息结构的所有细节，比字符估算准得多），
    // 折成字符时乘**上界**（宁可多算已用量）；首轮没有真实值才整体退回本地估算。
    let calibration = crate::usage::calibration_for_in(&config_dir, &config.model);
    if config.auto_compact && send.history().len() >= 4 {
        let real_baseline =
            crate::usage::last_prompt_tokens_for_in(&config_dir, conversation_id).unwrap_or(0);
        let tail_chars = send
            .history()
            .last()
            .and_then(|message| message["content"].as_str())
            .map(|text| text.chars().count())
            .unwrap_or(0);
        // 什么时候压：不再是"总量过了窗口的九成"，而是预算表点名要历史这一层付账。
        // 窗口先减掉本来就要留给输出的那截——旧的 `* 0.9` 想说的就是这个数，
        // 而它明写在配置里（config.max_tokens），不该用一个写死的比例去猜
        let sizing = sizing_of(config, calibration.as_ref());
        let uses = crate::session::layers::uses(&send.opened.log, send.standing())
            .map_err(|error| error.to_string())?;
        let table = crate::session::layers::budget(&uses, sizing);
        let estimate = if real_baseline > 0 {
            crate::session::layers::Estimate {
                // 服务商报的是 token，这张表量的是字符：不换算就等于把 3 万 token 当成 3 万字符
                chars: calibrated_baseline(real_baseline, calibration.as_ref()) + tail_chars,
                kind: crate::session::layers::EstimateKind::Calibrated,
            }
        } else {
            crate::session::layers::estimate(&uses)
        };
        let plan = crate::session::layers::plan(estimate, &table, None);
        // microcompact 先于整段压缩（O5-1/O5-2）：阶梯点了「清旧工具结果」就走读侧
        // 变换——不花摘要请求、不动日志。省下 ≥256 token **且**清完装得进预算，
        // 这一发就用清过的 wire 发；两项有一项不满足，照旧走下面的整段压缩
        let mut microcompacted = false;
        if plan
            .ladder
            .contains(&crate::session::layers::Concession::ClearStaleToolResults)
        {
            let (_cleared, saved_chars) =
                crate::session::context::clear_stale_tool_results(send.history());
            let saved_tokens = tokens_of_chars(saved_chars, calibration.as_ref()) as usize;
            let post_plan = crate::session::layers::plan(
                crate::session::layers::Estimate {
                    chars: estimate.chars.saturating_sub(saved_chars),
                    kind: estimate.kind,
                },
                &table,
                None,
            );
            let fits = !matches!(
                post_plan.reason,
                crate::session::layers::BreakReason::OverBudget { .. }
            );
            if saved_tokens >= crate::session::context::MICROCOMPACT_MIN_SAVED_TOKENS && fits {
                send.microcompact = true;
                send.refresh()?;
                microcompacted = true;
                // 贴附提示不进正文：清了几条、省了多少，跟"压缩推迟"同一档的读数
                on_event.send(ChatEvent::Retry {
                    text: format!(
                        "microcompact：清掉旧工具结果，这一发省下约 {saved_tokens} token。"
                    ),
                    reason: "上下文让步阶梯：先清旧工具结果（读侧变换），日志一行不动。".into(),
                });
            }
        }
        let owed = !microcompacted
            && (plan
                .ladder
                .contains(&crate::session::layers::Concession::CompactHistory)
            // 预防线（SoL-Pi 的步骤边界思想）：目标/计划续跑的轮次是语义干净的
            // 步骤边界——历史层用到硬顶七成就在这里提前压，别等逼近上限时
            // 在任务中间压
            || (continuing_goal
                && table
                    .row(crate::session::layers::Layer::History)
                    .is_some_and(|row| row.chars * 10 >= row.max * 7)));
        if owed {
            // 缓存重写成本项（SoL-Pi 的 cacheWriteReadRatio 精神）：压缩必然作废
            // 粘住成员身上的热前缀缓存，下一发是全价重写。缓存还热、没有更深的
            // 让步点名、且总量仍在窗口容量内（晚一发压不会 400）时，推迟一次——
            // 缓存冷了或更逼近上限时，这里的判定自然放行
            let cache_hot = crate::pool::cache_hot_for(conversation_id);
            let deeper = plan.ladder.iter().any(|step| {
                matches!(
                    step,
                    crate::session::layers::Concession::DropMemorySection
                        | crate::session::layers::Concession::TrimSkills
                )
            });
            let window_chars = (config.context_tokens.saturating_sub(config.max_tokens)) as f64
                * crate::usage::budget_ratio(calibration.as_ref());
            let slack = (estimate.chars as f64) < window_chars * 0.98;
            if cache_hot && !deeper && slack {
                // 只弹贴附提示不进正文：推迟的压缩不是本轮的失败
                on_event.send(ChatEvent::Retry {
                    text: "压缩推迟：当前成员的缓存还热，压一次等于整段重写。".into(),
                    reason: "上下文逼近预算上限，缓存转冷或更逼近上限时会自动压缩。".into(),
                });
            } else {
                on_event.send(ChatEvent::Compaction {
                    phase: "start".into(),
                    summary: None,
                    kept: None,
                });
                let history = send.history().to_vec();
                match summarize_history_in(&config_dir, &data_dir, config, &history) {
                    Ok(summary) => {
                        // 压缩写成一条条目，而不是就地改写一个数组：改写的版本下一轮就没了，
                        // 界面上的条数和模型看到的条数还会各说各话（旧设计里 `kept` 口径不一致
                        // 就是这么来的）。条目进日志之后，"压过了"这个事实本身也是历史的一部分
                        let boundary = send.provenance().ok().and_then(|origin| {
                            compaction_boundary(&history, &origin, KEEP_RECENT_CHARS)
                        });
                        match boundary {
                            Some((first_kept_entry_id, kept)) => {
                                send.append(crate::session::entry::EntryPayload::Compaction {
                                    summary: summary.clone(),
                                    first_kept_entry_id,
                                    tokens_before: crate::session::layers::thread_chars(
                                        send.standing(),
                                        &history,
                                    ),
                                    usage: None,
                                    system_message: send.section_snapshot(),
                                })?;
                                on_event.send(ChatEvent::Compaction {
                                    phase: "done".into(),
                                    summary: Some(summary),
                                    kept: Some(kept),
                                });

                                // 压缩后复验走同一张预算表。真实 baseline 是压缩前的旧值，
                                // 不能再拿它判定，否则会误判"仍超窗"而连环压缩
                                let post_uses =
                                    crate::session::layers::uses(&send.opened.log, send.standing())
                                        .unwrap_or_default();
                                let post_plan = crate::session::layers::plan(
                                    crate::session::layers::estimate(&post_uses),
                                    &crate::session::layers::budget(&post_uses, sizing),
                                    // 这里不报"压缩授权过的那次断开"：本轮要看的是还装不装得下
                                    None,
                                );
                                if matches!(
                                    post_plan.reason,
                                    crate::session::layers::BreakReason::OverBudget { .. }
                                ) {
                                    on_event.send(ChatEvent::Notice {
                                    text: "压缩后上下文仍接近窗口上限，建议调大「上下文窗口」配置或减少保留长度。"
                                        .into(),
                                });
                                }
                            }
                            None => {
                                on_event.send(ChatEvent::Notice {
                                    text:
                                        "上下文接近窗口上限，但可压缩的对话太少，本轮按原样发送。"
                                            .into(),
                                });
                            }
                        }
                    }
                    Err(error) => {
                        // 压缩失败不拦路：降级按原样发送，爆窗口是服务商的事，摘要挂了不该把整轮拖死
                        eprintln!("上下文压缩失败，按原样发送：{error}");
                        on_event.send(ChatEvent::Notice {
                            text: format!("上下文压缩失败（{error}），本轮按原样发送。"),
                        });
                    }
                }
            }
        }
    }

    // 输出预算钳制（压缩之后算，用的才是最终上下文）：
    // max_tokens 设得比"剩余窗口"还大时，有的服务商直接 400，
    // 有的会把输入截一半。钳到剩余空间，至少 1K 保底
    {
        let estimate = fixed_chars + history_chars_of(send.history());
        // 字符折回 token 时**除以下界**：同一条换算，方向上宁可少算空位，
        // 也不要报出一个"还剩 9 万"而服务商其实接不住（§15）
        let remaining = config
            .context_tokens
            .saturating_sub(tokens_of_chars(estimate, calibration.as_ref()));
        if config.max_tokens > remaining {
            turn_config.max_tokens = remaining.max(1024);
        }
    }

    let started = Instant::now();
    let mut input_tokens = 0u32;
    let mut output_tokens = 0u32;
    // 命中缓存的输入取各轮最大值：每轮的 prompt 都是"上轮全部+新增"，
    // 最大值就是最近一轮的真实命中量（与 input_tokens 的累计口径一致）。
    // 某轮没上报就跳过它，但不能让"从未上报"退化成 0——那是两件事
    let mut cached_tokens: Option<u32> = None;

    // 轮数上限可配置：0 = 不设上限（这是设置里能写出来的明确决定，"防无限循环烧 token"
    // 那道闸要不要留着由用户说了算）。自带天花板的那次运行（子助理）照旧按它自己的来，
    // 与全局谁大谁小无关
    let max_rounds = match rounds_cap {
        Some(cap) => cap.max(1) as usize,
        None if config.max_tool_rounds == 0 => usize::MAX,
        None => config.max_tool_rounds.max(1) as usize,
    };
    // 交错偏移的累进基数：本轮之前的所有回答按前端拼接的口径占多少个
    // UTF-16 码元（多条回答在投影里用空行缝成一条消息，缝占 2 个码元）
    let mut content_chars_so_far: usize = 0;
    for _round in 0..max_rounds {
        // 停止检查点 1：轮与轮之间。流式读取中的停止在 read_events 逐行检查
        if stopped(stop) {
            // 回合就此打住：挂着的那发回合内保温没了下一发请求可等，撤掉。
            // 这里不再发停止通知：前端按停止时已弹过"已请求停止"的提示，
            // 半截正文气泡本身也停在原地——正文里再插一句停止说明是重复打扰
            warm.cancel(conversation_id);
            return close_turn(
                host,
                conversation_id,
                auto_continue,
                interrupted_at_boundary(stop),
                continuing_goal,
                send,
                on_event,
                |entry_ids| ChatEvent::Done {
                    input_tokens,
                    output_tokens,
                    duration_ms: started.elapsed().as_millis() as u64,
                    cached_tokens,
                    entry_ids,
                    model: config.model.clone(),
                    context_tokens: config.context_tokens,
                },
            );
        }
        // 没有活动项目就没有文件根目录，此时不声明内置工具，避免模型往任意路径写。
        // 扩展工具不依赖工作目录，所以单独合并进来。
        // 插话检查点：上一轮工具都跑完了，用户中途说的话在这里进入上下文
        for text in steering.drain(conversation_id) {
            send.push(Message::User {
                content: format!("（执行中途的插话）{text}"),
                images: Vec::new(),
                audios: Vec::new(),
                videos: Vec::new(),
            })?;
        }

        // 请求阶段：无输出的可重试错误（限流/上游/网络）自动退避重试最多 2 次；
        // 一旦有输出（first_token_ms 已记）就不再重试，避免把半截回复重复给模型
        let mut attempt_started;
        let mut first_token_ms: Option<u64>;
        // 记一份"这一轮实际发出去的数组"。放在重试循环之前、每个工具回合一次：
        // 重试发的是同一个数组，重写无害；而最后一条记录正好等于最后一次请求。
        // 只有对话回合走这里——complete_once（标题生成、定时任务）发的不是话题历史，
        // 让它写快照会用一次一次性调用覆盖掉真正的对话
        // 每次请求前落一次盘：进程被杀时最多丢一次请求的内容，
        // 而丢掉的也只是"这一轮没记上"，不会让日志和实发分叉
        // 基线是否断开在发请求之前判：失败那条记账也要用它，而那时再问日志，
        // "日志走不通"会冒充成"服务商报错"返回给调用方
        let chain_reset = send.starts_fresh_chain()?;
        send.save();

        // 下一发真实请求马上就会把缓存续上：上一发待放的保温当场作废。
        // 它要真等到点才被顶掉，放出去的就是一次纯浪费的重放
        warm.cancel(conversation_id);
        let mut attempts: u32 = 0;
        let mut outcome = loop {
            attempt_started = Instant::now();
            first_token_ms = None;
            // 增量事件先攒帧再进界面（DeltaCoalescer）：首 token 计时仍按
            // 攒帧前的真实首个增量算，重试闸门与延迟指标都不受合帧影响
            let mut coalescer = DeltaCoalescer::new();
            // 重复循环护栏（每发重试各一份：重发的流从头算）：增量在进合帧器之前
            // 先过检测器，命中即拉起停止旗标——停止通道是全场最老练的断流路径，
            // 半截正文落定、Notice、close_turn 全是现成的
            let mut repetition_guard = crate::repetition::Guard::new();
            let mut loop_hit = false;
            let mut emit = |event: ChatEvent| {
                if matches!(event, ChatEvent::Delta { .. }) && first_token_ms.is_none() {
                    first_token_ms = Some(attempt_started.elapsed().as_millis() as u64);
                }
                if config.repetition_guard && !loop_hit {
                    match &event {
                        ChatEvent::Delta { text } | ChatEvent::Reasoning { text }
                            if repetition_guard
                                .push(text, matches!(event, ChatEvent::Reasoning { .. })) =>
                        {
                            loop_hit = true;
                            stop.store(true, std::sync::atomic::Ordering::Release);
                        }
                        _ => {}
                    }
                }
                if loop_hit {
                    // 已拉闸：模型还在往连接里吐的循环尾巴不再进界面
                    return;
                }
                coalescer.push(event, &mut |event| {
                    on_event.send(event);
                });
            };
            let result = request_round(
                &turn_config,
                &key,
                send.rows(),
                &declared,
                Some(conversation_id),
                stop,
                &mut emit,
            );
            // 流结束（含失败/停止）先冲帧：攒下的正文必须完整走完事件序，
            // 失败半截的落库与"有没有输出过"的重试判定都排在它后面
            coalescer.flush(&mut |event| {
                on_event.send(event);
            });
            match result {
                Ok(outcome) => break outcome,
                Err(failure) if failure.stopped() => {
                    // 回合就此打住：挂着的那发回合内保温没了下一发请求可等，撤掉
                    warm.cancel(conversation_id);
                    // 用户按了停止：不算失败（不记失败账），但那半截他在界面上读过了，
                    // 必须作为"未写完"的落定行进日志，否则下一轮模型以为自己没说过
                    if let Some(row) = settle_failed(&failure.partial, StopReason::Aborted, None) {
                        send.push(row)?;
                        send.save();
                    }
                    // 两种停法在界面上必须分得开：护栏掐的复读要说清是它拦的、内容
                    // 还在、怎么换答案——这条进正文（ toast 只报了"已请求停止"）。
                    // 人按的普通停止不再发正文通知：toast 已覆盖，半截正文气泡停在原地
                    if loop_hit {
                        on_event.send(ChatEvent::Notice {
                            text: "检测到模型输出陷入重复循环，已自动截断：循环前的内容已保留，后续 token 不再消耗。可用「重新生成」换一支答案。".into(),
                        });
                    }
                    // 同上：流被断也只掐这一轮。那半截已经落进行，下一轮模型看得见它
                    return close_turn(
                        host,
                        conversation_id,
                        auto_continue,
                        interrupted_at_boundary(stop),
                        continuing_goal,
                        send,
                        on_event,
                        |entry_ids| ChatEvent::Done {
                            input_tokens,
                            output_tokens,
                            duration_ms: started.elapsed().as_millis() as u64,
                            cached_tokens,
                            entry_ids,
                            model: config.model.clone(),
                            context_tokens: config.context_tokens,
                        },
                    );
                }
                Err(failure)
                    if attempts < 2
                        && first_token_ms.is_none()
                        && is_retryable(&failure.message) =>
                {
                    attempts += 1;
                    let wait = u64::from(attempts) * 2;
                    on_event.send(ChatEvent::Retry {
                        text: format!("将在 {wait} 秒后自动重试（第 {attempts}/2 次）。"),
                        reason: failure.message.clone(),
                    });
                    thread::sleep(Duration::from_secs(wait));
                }
                Err(failure) => {
                    // 失败也记一行：这个服务商今天挂了几次，只有台账答得了
                    crate::usage::record_turn_in(
                        &config_dir,
                        config,
                        "chat",
                        conversation_id,
                        &config.model,
                        &crate::usage::Tokens::default(),
                        // 断掉的那一发没有可信的实发大小：填 0 它就进不了校准样本
                        0,
                        chain_reset,
                        attempt_started.elapsed().as_millis() as u64,
                        first_token_ms,
                        false,
                        &failure.message,
                    );
                    if let Some(row) =
                        settle_failed(&failure.partial, StopReason::Error, Some(&failure.message))
                    {
                        send.push(row)?;
                        send.save();
                    }
                    // 服务商报错后回合终止：待放的保温没有下一发可等，撤掉
                    warm.cancel(conversation_id);
                    return Err(failure.message);
                }
            }
        };

        crate::usage::record_turn_in(
            &config_dir,
            config,
            "chat",
            conversation_id,
            &turn_config.model,
            &tokens_of(&outcome.usage),
            outcome.sent_chars,
            chain_reset,
            attempt_started.elapsed().as_millis() as u64,
            first_token_ms,
            true,
            "",
        );

        // 模型对账：用户要的（requested）→ 本地映射后实发的（mapped/sent）→
        // 上游自报回家的（response）。四格判定落台账（写库失败静默，对账绝不
        // 打断对话），链路条同步亮一格——替换要让人当场看见
        let mapped = turn_config.model.clone();
        let sent = if outcome.sent_model.is_empty() {
            mapped.clone()
        } else {
            outcome.sent_model.clone()
        };
        let response_model = outcome.response_model.clone();
        let verdict = crate::model_trace::classify(
            requested_model,
            &mapped,
            &sent,
            response_model.as_deref(),
            crate::model_trace::whitelist_of(config),
        );
        {
            let reading = match (&verdict.kind, response_model.as_deref()) {
                (crate::model_trace::MismatchKind::None, Some(name))
                    if verdict.variant_of.is_some() =>
                {
                    format!("{name} · 日期变体")
                }
                (crate::model_trace::MismatchKind::None, _) => format!("{mapped} · 一致"),
                (crate::model_trace::MismatchKind::LocalMapping, _) => {
                    format!("{requested_model} → {mapped} · 本地映射")
                }
                (crate::model_trace::MismatchKind::UpstreamReplaced, Some(name)) => {
                    format!("要 {sent} · 上游回 {name}")
                }
                _ => format!("{sent} · 上游未报"),
            };
            let tone = match verdict.kind {
                crate::model_trace::MismatchKind::None => "ok",
                crate::model_trace::MismatchKind::UpstreamReplaced => "warn",
                crate::model_trace::MismatchKind::LocalMapping
                | crate::model_trace::MismatchKind::Unknown => "info",
            };
            let mut hint = format!("请求 {requested_model} · 实发 {sent}");
            hint.push_str(&match response_model.as_deref() {
                Some(name) => format!(" · 上游 {name}"),
                None => " · 上游未报模型名".to_string(),
            });
            if let Some(variant) = &verdict.variant_of {
                hint.push_str(&format!("（{variant} 为日期后缀变体）"));
            }
            on_event.send(ChatEvent::Probe {
                key: "model".into(),
                detail: reading,
                tone: Some(tone.into()),
                hint: Some(hint),
            });
        }
        let provider_host = tauri::Url::parse(&config.base_url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_string))
            .unwrap_or_default();
        crate::model_trace::record_in(
            &config_dir,
            conversation_id,
            &provider_host,
            crate::model_trace::endpoint_label(config),
            requested_model,
            &mapped,
            &sent,
            response_model,
            verdict.kind,
            verdict.variant_of,
            outcome.response_model_path.clone(),
        );

        // 交错偏移：本轮正文在前端是接在之前几轮后面的（空行缝），工具声明的
        // 位置 = 已累计基数 + 缝 + 本轮正文。口径是 UTF-16 码元——前端 JS 字符串
        // 的下标就是它，Rust 的 chars().count() 在 emoji 上会错一位
        // 缝只在两侧都有正文时才存在（投影合并的同一判据）：纯工具轮没有文字，不占缝
        let seam = if content_chars_so_far > 0 && !outcome.text.is_empty() {
            2
        } else {
            0
        };
        let call_content_chars = content_chars_so_far + seam + outcome.text.encode_utf16().count();
        content_chars_so_far = call_content_chars;
        for call in &mut outcome.tool_calls {
            call.content_chars = call_content_chars as u32;
        }

        if let Some(usage) = &outcome.usage {
            input_tokens = input_tokens.max(usage.input_tokens);
            // Option 的 Ord 把 None 排在任何 Some 之前，所以"某轮没上报"不会
            // 把已取到的命中抹零，而全程没上报仍然保持 None（不是 0）
            cached_tokens = cached_tokens.max(usage.cached_tokens);
            output_tokens += usage.output_tokens;
        }

        // 长工具循环的保温（Streaming 档）：模型刚声明的工具可能一跑好几分钟，
        // 没人发请求的间隙里，服务商缓存的整段前缀就过期了，下一发被迫全价重算。
        // 趁 tip 还停在"这一发的输入"上（工具结果还没追加），把保温排出去——
        // 续接概率按 1 算，因为下一发几乎必然要来；真按了停止，下面的退出路径会撤掉它。
        // 下一发真实请求开始时的 cancel、收尾那发空闲保温的 arm，都会顶掉这一发
        if !outcome.tool_calls.is_empty() {
            host.warm_schedule(
                config,
                warm,
                crate::warm::Plan {
                    conversation_id: conversation_id.to_string(),
                    tip: send.opened.log.leaf_id().map(str::to_string),
                    sent_at: crate::session::now_millis(),
                    prompt_tokens: crate::usage::last_prompt_tokens_for_in(
                        &config_dir,
                        conversation_id,
                    )
                    .unwrap_or(0)
                    .max(0) as u64,
                    delay_ms: 0,
                    ttl_ms: 0,
                    phase: crate::warm::Phase::Streaming,
                    refresh_deadline_ms: 0,
                },
                send.rows().to_vec(),
                declared.clone(),
            );
        }

        if outcome.tool_calls.is_empty() {
            // 收尾钩子有机会说"这轮还没交付完"。一条回合只让它续一次：
            // 每次都拒绝收尾的脚本会把对话挂死，参照实现也是靠这个标志位防循环的
            let hooks = crate::hooks::runnable_in(config, &data_dir);
            if !hooks.is_empty() && !asked_to_continue {
                let report = crate::hooks::fire(&hooks, "Stop", root.as_deref(), |hook, cwd| {
                    json!({
                        "hook_event_name": hook.event,
                        "cwd": cwd.display().to_string(),
                        "model": config.model,
                        "stop_hook_active": asked_to_continue,
                        "last_message": &outcome.text,
                    })
                });
                emit_hooks(on_event, &report);

                if let Some(reason) = report.blocked() {
                    asked_to_continue = true;
                    send.push(Message::Assistant(SettledAssistant {
                        content: outcome.text,
                        tool_calls: Vec::new(),
                        stop: StopReason::Stop,
                        reasoning: outcome.reasoning,
                        error: None,
                        thinking_signature: outcome.reasoning_signature,
                        reasoning_items_json: outcome.reasoning_items_json,
                    }))?;
                    send.push(Message::User {
                        content: format!("插件钩子认为这一轮还没做完，请接着往下：\n{reason}"),
                        images: Vec::new(),
                        audios: Vec::new(),
                        videos: Vec::new(),
                    })?;
                    continue;
                }
            }

            // 模型本想说"我做完了"，但用户中途插了话——继续一轮让它回应插话，
            // 而不是把插话晾到回合结束（pi 的 steering 语义：插话改变 agent 的走向）
            let pending_steering = steering.drain(conversation_id);
            if !pending_steering.is_empty() {
                // 不发这条的话，回应会无缝接在上一段答案后面——用户看不出
                // 模型回应了插话，只会觉得"发了没反应"
                on_event.send(ChatEvent::Notice {
                    text: "已收到你的插话，接着往下回应。".into(),
                });
                send.push(Message::Assistant(SettledAssistant {
                    content: outcome.text,
                    tool_calls: Vec::new(),
                    stop: StopReason::Stop,
                    reasoning: outcome.reasoning,
                    error: None,
                    thinking_signature: outcome.reasoning_signature,
                    reasoning_items_json: outcome.reasoning_items_json,
                }))?;
                for text in pending_steering {
                    send.push(Message::User {
                        content: format!("（执行中途的插话）{text}"),
                        images: Vec::new(),
                        audios: Vec::new(),
                        videos: Vec::new(),
                    })?;
                }
                continue;
            }

            // 模型这轮最后说的那句必须进快照：它是"回答"，天然不在任何一次请求数组里，
            // 但下一轮它就是历史。少了这一步，切换读路径后模型会忘掉自己刚回答的内容
            if !outcome.text.is_empty() {
                send.push(Message::Assistant(SettledAssistant {
                    content: outcome.text,
                    tool_calls: Vec::new(),
                    stop: StopReason::Stop,
                    reasoning: outcome.reasoning,
                    error: None,
                    thinking_signature: outcome.reasoning_signature,
                    reasoning_items_json: outcome.reasoning_items_json,
                }))?;
            }

            // 收尾这一轮：落排队里的切档、算续跑判据、按"读数先于 Done"发出去、落盘。
            // 四条断旗的路走的是同一个出口，见 `close_turn`
            let next = close_turn(
                host,
                conversation_id,
                auto_continue,
                interrupted_at_boundary(stop),
                continuing_goal,
                send,
                on_event,
                |entry_ids| ChatEvent::Done {
                    input_tokens,
                    output_tokens,
                    duration_ms: started.elapsed().as_millis() as u64,
                    cached_tokens,
                    entry_ids,
                    model: config.model.clone(),
                    context_tokens: config.context_tokens,
                },
            )?;
            // 保温要续的是"这一发刚刚写进服务商缓存的那条前缀"，所以排在这里而不是下一轮之前。
            // 落盘已经在 `close_turn` 里做完了：到点时保温线程是从磁盘重开日志核对末端的，
            // 没落盘的末端会让它误判"前缀变了"而把这一发作废
            host.warm_schedule(
                config,
                warm,
                crate::warm::Plan {
                    conversation_id: conversation_id.to_string(),
                    tip: send.opened.log.leaf_id().map(str::to_string),
                    sent_at: crate::session::now_millis(),
                    prompt_tokens: crate::usage::last_prompt_tokens_for_in(
                        &config_dir,
                        conversation_id,
                    )
                    .unwrap_or(0)
                    .max(0) as u64,
                    delay_ms: 0,
                    ttl_ms: 0,
                    phase: crate::warm::Phase::Idle,
                    refresh_deadline_ms: 0,
                },
                send.rows().to_vec(),
                declared.clone(),
            );
            return Ok(next);
        }

        // 带调用的那条只装它自己的正文；收尾回答是**另一条**条目。以前两条被并进同一条，
        // 回放出来就是"先回答、后收到工具结果"的倒置因果（F13）
        let calls: Vec<ToolCall> = outcome
            .tool_calls
            .iter()
            .map(|call| ToolCall {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
                content_chars: Some(call.content_chars),
            })
            .collect();

        send.push(Message::Assistant(SettledAssistant {
            content: outcome.text,
            tool_calls: calls,
            // 被输出上限截断的那次"调用"参数可能只有一半，标成 length 而不是 tool_use
            stop: if outcome.truncated {
                StopReason::Length
            } else {
                StopReason::ToolUse
            },
            // 中间轮次的思考也要落库：它是对话历史的一部分（responses/anthropic
            // 的回放凭据就在这里），界面上读得到，下一轮模型也看得到
            reasoning: outcome.reasoning,
            error: None,
            thinking_signature: outcome.reasoning_signature,
            reasoning_items_json: outcome.reasoning_items_json,
        }))?;

        // 输出被 token 上限截断时，流式拼出来的工具参数可能只传了一半——
        // 这样的调用执行了比不执行更危险（读错文件、删错目录）。
        // 全部按失败回传，让模型拿着完整意图重发。参照 pi 的 failToolCallsFromTruncatedMessage。
        if outcome.truncated {
            for call in &outcome.tool_calls {
                let (event, message) = tool_result_pair(
                    call,
                    ToolStatus::Failed,
                    tools::Risk::High.as_str(),
                    call.name.clone(),
                    "助手消息被输出长度上限截断，这次工具调用的参数可能被切断，因此没有执行。请重新发出参数完整的调用。"
                        .into(),
                    // 还没走到闸门：谈不上"跳过询问"
                    None,
                );
                on_event.send(event);
                send.push(message)?;
            }
            continue;
        }

        // ---- 拓扑调度预跑：相邻安全读并排执行（maxConcurrency=10）----
        //
        // 预跑只接"全绿"的批：每个成员都要通过与串行主干同款的纯闸（解析/禁用/
        // 沙箱边界/声明校验/权限 Allow/执行前钩子不拦不问），任何一个成员要问人、
        // 被拒、被拦，整批退回串行主干——调度是增益不是闸门。结果按原顺序走与
        // 串行完全相同的后账（PostToolUse 钩子/归档/事件/推送），界面看到的
        // 顺序与串行一致：批内并行的是执行，不是回填
        let mut consumed_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        // 本轮每个调用的契约：并行预跑的拓扑与串行主干的输出钳制读同一份
        let round_contracts: Vec<crate::tool_contract::Contract> = outcome
            .tool_calls
            .iter()
            .map(|call| {
                let args = serde_json::from_str::<Value>(&call.arguments).unwrap_or(json!({}));
                crate::tool_contract::contract_for(&call.name, &args)
            })
            .collect();
        if !outcome.truncated && !outcome.tool_calls.is_empty() {
            'slots: for slot in crate::tool_scheduler::plan_round(&round_contracts) {
                let indexes = match slot {
                    crate::tool_scheduler::Slot::Parallel(indexes) if indexes.len() >= 2 => indexes,
                    _ => continue,
                };
                if stopped(stop) {
                    break;
                }
                // 预检：与串行主干同款的纯闸，逐成员过；任何一个不过就整批放弃
                struct Member<'a> {
                    call: &'a ToolCallBuffer,
                    args: Value,
                    risk: tools::Risk,
                    input: String,
                }
                let mut members: Vec<Member> = Vec::with_capacity(indexes.len());
                for index in &indexes {
                    let call = &outcome.tool_calls[*index];
                    let args = match parse_arguments(&call.arguments) {
                        Ok(value) => value,
                        Err(_) => continue 'slots,
                    };
                    if tools::is_disabled(&config.disabled_tools, &call.name) {
                        continue 'slots;
                    }
                    let via_mcp = crate::mcp::owns(mcp_servers, &call.name);
                    if via_mcp {
                        // 扩展调用走不了内置执行体，批里出现即退串行
                        continue 'slots;
                    }
                    let scope = tool_runtime::Call::new(&call.name, &args, root.as_deref(), false);
                    if crate::tool_runtime::sandbox::enabled()
                        && crate::tool_runtime::sandbox::boundary_violation(
                            &call.name,
                            &args,
                            bound_root.as_deref(),
                            root.as_deref(),
                        )
                        .is_some()
                    {
                        continue 'slots;
                    }
                    if tool_runtime::check_arguments(&scope).is_err() {
                        continue 'slots;
                    }
                    let risk = tools::classify(&call.name, &args, root.as_deref());
                    if !matches!(risk, tools::Risk::Safe) {
                        // 契约说可并行、classify 却给了更高档：以闸为准，退串行
                        continue 'slots;
                    }
                    let input = mask_tool_input(false, &call.name, &args);
                    let ruling = tool_runtime::rule(
                        &policy,
                        &scope,
                        &input,
                        tool_runtime::allowlist(conversation_id).as_deref(),
                    );
                    if !matches!(ruling.decision, crate::policy::Decision::Allow) {
                        continue 'slots;
                    }
                    // 执行前钩子：拦或问都退串行（串行主干对被拦的成员有完整的
                    // 拒绝回填，预跑不重复那份语义）
                    let hook_report = crate::hooks::fire(
                        &crate::hooks::runnable_in(config, &data_dir),
                        "PreToolUse",
                        root.as_deref(),
                        |hook, cwd| {
                            json!({
                                "hook_event_name": hook.event,
                                "cwd": cwd.display().to_string(),
                                "model": config.model,
                                "tool_name": call.name,
                                "tool_input": &args,
                            })
                        },
                    );
                    emit_hooks(on_event, &hook_report);
                    if hook_report.blocked().is_some() || hook_report.asks().is_some() {
                        continue 'slots;
                    }
                    // 审计与串行同一格：放行记录在 Running 之前
                    if let Err(error) = audit_tool_in(
                        &data_dir,
                        conversation_id,
                        &scope,
                        crate::audit::Outcome::Ok,
                        None,
                    ) {
                        eprintln!("并行批成员审计写不进去，整批退串行：{error}");
                        continue 'slots;
                    }
                    members.push(Member {
                        call,
                        args,
                        risk,
                        input,
                    });
                }
                // Running 事件按原顺序发，卡片位置与串行一致
                for member in &members {
                    on_event.send(ChatEvent::Tool {
                        id: member.call.id.clone(),
                        name: member.call.name.clone(),
                        status: ToolStatus::Running,
                        risk: member.risk.as_str().into(),
                        input: member.input.clone(),
                        output: None,
                        arguments: Some(member.call.arguments.clone()),
                        pass_reason: None,
                        content_chars: Some(member.call.content_chars),
                    });
                }
                // 并行执行：批大小 ≤ MAX_CONCURRENCY，execute_for 是纯内置执行体
                // 契约的 max_output_bytes 在这里生效：与全局钳制取小者
                let caps: Vec<usize> = indexes
                    .iter()
                    .map(|index| {
                        crate::tool_contract::effective_cap(
                            round_contracts[*index].max_output_bytes,
                            config.tool_result_max_chars,
                        )
                    })
                    .collect();
                let outputs: Vec<Result<String, String>> = std::thread::scope(|scope| {
                    let handles: Vec<_> = members
                        .iter()
                        .map(|member| {
                            let root = root.clone();
                            let name = member.call.name.clone();
                            let args = member.args.clone();
                            scope.spawn(move || {
                                tools::execute_for(&name, &args, root.as_deref(), None)
                            })
                        })
                        .collect();
                    handles
                        .into_iter()
                        .map(|handle| {
                            handle
                                .join()
                                .unwrap_or_else(|_| Err("并行工具线程崩了。".into()))
                        })
                        .collect()
                });
                // 后账按原顺序逐成员走：PostToolUse 钩子 → 归档 → 打包 → 标注 → Done → push
                for (member, (output, tool_result_max)) in
                    members.iter().zip(outputs.into_iter().zip(caps))
                {
                    match output {
                        Ok(text) => {
                            let report = crate::hooks::fire(
                                &crate::hooks::runnable_in(config, &data_dir),
                                "PostToolUse",
                                root.as_deref(),
                                |hook, cwd| {
                                    json!({
                                        "hook_event_name": hook.event,
                                        "cwd": cwd.display().to_string(),
                                        "model": config.model,
                                        "tool_name": member.call.name,
                                        "tool_input": &member.args,
                                        "tool_response": &text,
                                    })
                                },
                            );
                            emit_hooks(on_event, &report);
                            let content = match report.context() {
                                Some(extra) => format!("{text}\n\n{extra}"),
                                None => text,
                            };
                            if content.chars().count() > tool_result_max {
                                crate::observations::archive(&member.call.id, &content);
                            }
                            let content =
                                pack_tool_result(&content, tool_result_max, Some(&member.call.id));
                            let content = tool_runtime::annotate(
                                tool_runtime::source::Kind::Builtin,
                                &member.call.name,
                                content,
                            );
                            let (event, message) = tool_result_pair(
                                member.call,
                                ToolStatus::Done,
                                member.risk.as_str(),
                                member.input.clone(),
                                content,
                                None,
                            );
                            on_event.send(event);
                            send.push(message)?;
                        }
                        Err(error) => {
                            let failed_scope = tool_runtime::Call::new(
                                &member.call.name,
                                &member.args,
                                root.as_deref(),
                                false,
                            );
                            let _ = audit_tool_in(
                                &data_dir,
                                conversation_id,
                                &failed_scope,
                                crate::audit::Outcome::Failed,
                                None,
                            );
                            // PostToolUseFailure（并行后账）：与串行同一套形状，
                            // deny 只能转达，拦不回已经发生过的失败
                            {
                                let hooks = crate::hooks::runnable_in(config, &data_dir);
                                if !hooks.is_empty() {
                                    let report = crate::hooks::fire(
                                        &hooks,
                                        "PostToolUseFailure",
                                        root.as_deref(),
                                        |hook, cwd| {
                                            json!({
                                                "hook_event_name": hook.event,
                                                "cwd": cwd.display().to_string(),
                                                "model": config.model,
                                                "tool_name": member.call.name,
                                                    "tool_input": &member.args,
                                                    "error": error.to_string(),
                                            })
                                        },
                                    );
                                    emit_hooks(on_event, &report);
                                }
                            }
                            let (event, message) = tool_result_pair(
                                member.call,
                                ToolStatus::Failed,
                                member.risk.as_str(),
                                member.input.clone(),
                                format!("执行失败：{error}"),
                                None,
                            );
                            on_event.send(event);
                            send.push(message)?;
                        }
                    }
                    consumed_ids.insert(member.call.id.clone());
                }
            }
        }

        for (call_index, call) in outcome.tool_calls.iter().enumerate() {
            if consumed_ids.contains(&call.id) {
                continue;
            }
            let args = match parse_arguments(&call.arguments) {
                Ok(value) => value,
                Err(error) => {
                    // 参数不是合法 JSON 时宁可不执行：空参数硬跑是对着猜的意图动文件
                    let (event, message) = tool_result_pair(
                        call,
                        ToolStatus::Failed,
                        tools::Risk::High.as_str(),
                        call.name.clone(),
                        format!("工具调用参数解析失败，没有执行：{error}"),
                        None,
                    );
                    on_event.send(event);
                    send.push(message)?;
                    continue;
                }
            };

            // 被关掉的能力即使被硬调回来也不执行，但必须给出工具结果，否则这条 tool_call 悬空
            if tools::is_disabled(&config.disabled_tools, &call.name) {
                let (event, message) = tool_result_pair(
                    call,
                    ToolStatus::Denied,
                    tools::Risk::High.as_str(),
                    tools::summary(&call.name, &args),
                    tools::DISABLED_NOTE.into(),
                    None,
                );
                on_event.send(event);
                send.push(message)?;
                continue;
            }

            // 扩展跑的是别人的程序，看不到它会做什么，所以一律按高风险处理
            if stopped(stop) {
                // 回合就此打住：挂着的那发回合内保温没了下一发请求可等，撤掉
                warm.cancel(conversation_id);
                on_event.send(ChatEvent::Notice {
                    text: "已按你的要求停止生成，剩余的工具调用没有执行。".into(),
                });
                // 同上：断在工具之间也只掐这一轮，没执行的那几条不许替目标做决定
                return close_turn(
                    host,
                    conversation_id,
                    auto_continue,
                    interrupted_at_boundary(stop),
                    continuing_goal,
                    send,
                    on_event,
                    |entry_ids| ChatEvent::Done {
                        input_tokens,
                        output_tokens,
                        duration_ms: started.elapsed().as_millis() as u64,
                        cached_tokens,
                        entry_ids,
                        model: config.model.clone(),
                        context_tokens: config.context_tokens,
                    },
                );
            }
            let via_mcp = crate::mcp::owns(mcp_servers, &call.name);
            let risk = if via_mcp {
                tools::Risk::High
            } else {
                tools::classify(&call.name, &args, root.as_deref())
            };
            // 这一串接下来要进三个地方：策略指纹、待审批队列（在盘上、跨重启）、审批界面。
            // 命令行里最常带的就是 token，所以进这三处之前先打码——而**动手用的不是这一串**：
            // 执行走的是模型给的原始参数，打码只改"它被怎么记录与怎么呈现"
            let input = mask_tool_input(via_mcp, &call.name, &args);

            // 沙箱边界先于一切：越界的文件写入是无条件拒绝（对齐 Codex 的
            // "单一边界覆盖一切动作"）——收容与低完整性只管命令的子进程，
            // write_file/edit_file 是本进程直写，必须在这里与命令同一套边界。
            // 犯不着为它跑用户的脚本，更犯不着弹审批
            if !via_mcp && crate::tool_runtime::sandbox::enabled() {
                if let Some(reason) = crate::tool_runtime::sandbox::boundary_violation(
                    &call.name,
                    &args,
                    bound_root.as_deref(),
                    root.as_deref(),
                ) {
                    let scope =
                        tool_runtime::Call::new(&call.name, &args, root.as_deref(), via_mcp);
                    let _ = audit_tool_in(
                        &data_dir,
                        conversation_id,
                        &scope,
                        crate::audit::Outcome::Denied,
                        None,
                    );
                    let (event, message) = tool_result_pair(
                        call,
                        ToolStatus::Denied,
                        tools::Risk::High.as_str(),
                        input.clone(),
                        reason,
                        None,
                    );
                    on_event.send(event);
                    send.push(message)?;
                    continue;
                }
            }

            // 执行前钩子：它要是拦下了，连询问界面都不弹——护栏要的就是"别让用户来判断这个"。
            // 它要是"问一句"（ask），这一次调用哪怕权限表放行，也拉回审批
            let hook_ask_reason: Option<String> = {
                let hooks = crate::hooks::runnable_in(config, &data_dir);
                if hooks.is_empty() {
                    None
                } else {
                    let report =
                        crate::hooks::fire(&hooks, "PreToolUse", root.as_deref(), |hook, cwd| {
                            json!({
                                "hook_event_name": hook.event,
                                "cwd": cwd.display().to_string(),
                                "model": config.model,
                                "tool_name": call.name,
                                "tool_input": &args,
                            })
                        });
                    emit_hooks(on_event, &report);

                    if let Some(reason) = report.blocked() {
                        let (event, message) = tool_result_pair(
                            call,
                            ToolStatus::Denied,
                            risk.as_str(),
                            input.clone(),
                            format!("插件钩子拦下了这次调用，没有执行：\n{reason}"),
                            None,
                        );
                        on_event.send(event);
                        send.push(message)?;
                        continue;
                    }
                    report.asks()
                }
            };

            // 闸门：一次调用先被翻译成 capability，再由权限表决定放行 / 询问 / 拒绝。
            // 这里不再问"风险高不高"——那是同一件事的旧影子，两份真相得合成一份。
            // 白名单来自本轮话题取用过的技能，它在权限表之前生效：技能没给的能力，
            // 不该拿去让用户点头
            let scope = tool_runtime::Call::new(&call.name, &args, root.as_deref(), via_mcp);
            // 入参先过声明校验：不合法的参数不配进审批框，更不能被"猜一个默认值"跑掉
            if let Err(problem) = tool_runtime::check_arguments(&scope) {
                let (event, message) = tool_result_pair(
                    call,
                    ToolStatus::Denied,
                    risk.as_str(),
                    input.clone(),
                    format!("参数不符合工具声明，没有执行：{problem}"),
                    None,
                );
                on_event.send(event);
                send.push(message)?;
                continue;
            }
            let ruling = tool_runtime::rule(
                &policy,
                &scope,
                &input,
                tool_runtime::allowlist(conversation_id).as_deref(),
            );
            let remembered = hub.is_remembered(&ruling.remember_key());

            if let crate::policy::Decision::Deny { reason } = &ruling.decision {
                let _ = audit_tool_in(
                    &data_dir,
                    conversation_id,
                    &scope,
                    crate::audit::Outcome::Denied,
                    None,
                );
                // 被拒也要给出工具结果，否则这条 tool_call 悬空
                let (event, message) = tool_result_pair(
                    call,
                    ToolStatus::Denied,
                    risk.as_str(),
                    input.clone(),
                    reason.clone(),
                    // 表上写着不许：那不是"该问而没问"
                    None,
                );
                on_event.send(event);
                send.push(message)?;
                continue;
            }

            // PermissionRequest 钩子先投票：deny 直接拒（不弹审批），allow 顶掉
            // "该问"（放行凭据写进 pass_reason，卡片同步写明是谁点的头），ask 把
            // 本来放行的调用拉回审批。发射前重解析：与相邻的闸同一规矩
            let permission_report = {
                let hooks = crate::hooks::runnable_in(config, &data_dir);
                if hooks.is_empty() {
                    crate::hooks::Report::default()
                } else {
                    let report = crate::hooks::fire(
                        &hooks,
                        "PermissionRequest",
                        root.as_deref(),
                        |hook, cwd| {
                            json!({
                                "hook_event_name": hook.event,
                                "cwd": cwd.display().to_string(),
                                "model": config.model,
                                "tool_name": call.name,
                                "tool_input": &args,
                                "risk": risk.as_str(),
                            })
                        },
                    );
                    emit_hooks(on_event, &report);
                    report
                }
            };
            if let Some(reason) = permission_report.blocked() {
                // 权限钩子说不许：不执行、不弹审批，原因原样到卡片
                let (event, message) = tool_result_pair(
                    call,
                    ToolStatus::Denied,
                    risk.as_str(),
                    input.clone(),
                    reason,
                    None,
                );
                on_event.send(event);
                send.push(message)?;
                continue;
            }
            let permission_hook_pass = permission_report.approves();

            // 审批等待也响应停止：否则按了停止还要干等满 10 分钟超时。
            // 权限表说"该问"或执行前钩子说"问一句"，都走同一条审批路
            let needs_approval = (matches!(ruling.decision, crate::policy::Decision::Ask { .. })
                && !remembered)
                || hook_ask_reason.is_some();
            // 权限钩子的裁决并进判据（deny 已在上面直接拒了）：allow 顶掉"该问"，
            // ask 把本来放行的调用拉回审批——单数的放行盖不过 block 与 ask
            let needs_approval = (needs_approval && permission_hook_pass.is_none())
                || permission_report.asks().is_some();
            // 后台 run 没有人可问：这一发挂到待审批队列，动作不动手。让它去走 ApprovalHub
            // 那 600s 超时的话，"没人看"就会被记成"用户摇头"，而队列里那条待审批——
            // 也就是"等谁来处理"的事实——根本不会存在
            let escalation =
                if needs_approval && crate::tasks::escalate::is_unattended(conversation_id) {
                    park_unattended(host, conversation_id, &ruling, &input)
                } else {
                    Escalated::Prompt
                };
            // "该问而没问"那两种放行要答得出凭哪一条：命中本话题内的规则，或无人值守
            // 那条路上早已登记的 standing 授权。少了这一句，卡片上那一行与"刚刚点了头"
            // 的那一行长得一模一样——设计把悄悄放行算作缺陷（§4 步骤 3、§8 风险 6）。
            // 自动审查的放行在下面那格补标（mut：审查通过时写"自动审查通过"）
            let mut pass_reason = if !matches!(ruling.decision, crate::policy::Decision::Ask { .. })
            {
                // 权限表本来就放行：那不是"跳过了询问"，标它等于把旋钮说反
                None
            } else if remembered {
                Some(SESSION_RULE_PASS.to_string())
            } else if matches!(escalation, Escalated::Run) {
                Some(STANDING_GRANT_PASS.to_string())
            } else {
                None
            };
            // 权限钩子替用户点的头要答得出凭哪一条：写进同一格，与卡片同文
            if needs_approval {
                // 该问的还是问了（钩子要问或别的钩子没放行），这里不动
            } else if let Some(reason) = permission_hook_pass {
                pass_reason = Some(format!("按 PermissionRequest 钩子放行：{reason}"));
            }
            if let Escalated::Halted { outcome, reason } = escalation {
                // 停在待批队列里等人：这一发没动手，没有"放行"可标
                let _ = audit_tool_in(&data_dir, conversation_id, &scope, outcome, None);
                let (event, message) = tool_result_pair(
                    call,
                    ToolStatus::Denied,
                    risk.as_str(),
                    input.clone(),
                    reason,
                    // 挂在待批队列里等人：这一发根本没动手，没有"放行"可标
                    None,
                );
                on_event.send(event);
                send.push(message)?;
                continue;
            }

            let approved = if matches!(escalation, Escalated::Prompt) && needs_approval {
                if config.auto_review {
                    // 自动审查（对齐 deepseek 的 Auto review）：审查模型替人拍板。
                    // 人工审批卡在这条路上**不发**——发了也是一块死按钮：没人在
                    // ApprovalHub 等人的票，点拒绝石沉大海，然后审查通过照跑（真机踩过）。
                    // **不改沙箱边界**——只处理升级请求，边界内的动作照旧自主执行。
                    // 审查失败 fail-closed：按拒绝处理（模型是安全闸不是便利闸）
                    let verdict =
                        auto_review_verdict(&config_dir, config, &call.name, &input, risk.as_str());
                    let audit_root = data_dir.clone();
                    crate::audit::record(
                        &audit_root,
                        crate::audit::Actor::Model,
                        if verdict.approved {
                            "approval:auto_review"
                        } else {
                            "approval:auto_review_denied"
                        },
                        &ruling.key,
                        if verdict.approved {
                            crate::audit::Outcome::Ok
                        } else {
                            crate::audit::Outcome::Denied
                        },
                    )?;
                    if !verdict.approved {
                        let (event, message) = tool_result_pair(
                            call,
                            ToolStatus::Denied,
                            risk.as_str(),
                            input.clone(),
                            format!("自动审查未通过：{}", verdict.reason),
                            None,
                        );
                        on_event.send(event);
                        send.push(message)?;
                        continue;
                    }
                    // 审查通过 = 放行，卡片与账上都要答得出凭哪一条：凭审查模型那一票，
                    // 不是"用户点过头"——收回的旋钮是设置里的自动审查开关
                    pass_reason = Some(AUTO_REVIEW_PASS.to_string());
                    true
                } else {
                    // 先登记"这一次问的是哪份动作"，界面上的"本话题内允许"才有的可点：
                    // 键由后端算，前端只把它看到的那条 id 换回来。标签用确认框上当初那句话说的是
                    // 同一份文本——用户撤销时能认出自己放过的是哪一下，靠的就是这一格
                    hub.stage(&call.id, &ruling.remember_key(), &short_label(&input));
                    on_event.send(ChatEvent::Tool {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        status: ToolStatus::Pending,
                        risk: risk.as_str().into(),
                        input: input.clone(),
                        output: None,
                        arguments: Some(call.arguments.clone()),
                        // 这一次是真的在问：没有"跳过询问"可标
                        pass_reason: None,
                        content_chars: Some(call.content_chars),
                    });

                    // 窗口在后台时这就是一块看不见的暂停键：系统通知把它喊回来
                    host.toast_approval_needed(&input);
                    let answer = hub.wait(&call.id, APPROVAL_TIMEOUT, stop);
                    // 人的那一次点头单独落一行：它记的是"谁决定的"，而下面那条 `tool:*`
                    // 记的是"做了什么"。超时与按停止都算摇头
                    let audit_root = data_dir.clone();
                    crate::audit::record(
                        &audit_root,
                        crate::audit::Actor::User,
                        if answer {
                            "approval:granted"
                        } else {
                            "approval:refused"
                        },
                        &ruling.key,
                        if answer {
                            crate::audit::Outcome::Ok
                        } else {
                            crate::audit::Outcome::Denied
                        },
                    )?;
                    answer
                }
            } else {
                true
            };

            if stopped(stop) {
                // 回合就此打住：挂着的那发回合内保温没了下一发请求可等，撤掉。
                // 不再发停止通知：前端按停止时已弹过提示，半截正文停在原地
                warm.cancel(conversation_id);
                // 同上：工具跑完才看到旗，那一轮同样只是被打断，不是目标结束了
                return close_turn(
                    host,
                    conversation_id,
                    auto_continue,
                    interrupted_at_boundary(stop),
                    continuing_goal,
                    send,
                    on_event,
                    |entry_ids| ChatEvent::Done {
                        input_tokens,
                        output_tokens,
                        duration_ms: started.elapsed().as_millis() as u64,
                        cached_tokens,
                        entry_ids,
                        model: config.model.clone(),
                        context_tokens: config.context_tokens,
                    },
                );
            }

            if !approved {
                let _ = audit_tool_in(
                    &data_dir,
                    conversation_id,
                    &scope,
                    crate::audit::Outcome::Denied,
                    None,
                );
                let (event, message) = tool_result_pair(
                    call,
                    ToolStatus::Denied,
                    risk.as_str(),
                    input.clone(),
                    "用户拒绝执行该操作。".into(),
                    None,
                );
                on_event.send(event);
                send.push(message)?;
                continue;
            }

            // 落账之后才动手：一个说不出"谁在什么时候对什么做了什么"的客户端，
            // 出了问题没法复盘。写不进去就不执行，而不是"记不上也要跑"
            if let Err(error) = audit_tool_in(
                &data_dir,
                conversation_id,
                &scope,
                crate::audit::Outcome::Ok,
                pass_reason.as_deref(),
            ) {
                let (event, message) = tool_result_pair(
                    call,
                    ToolStatus::Denied,
                    risk.as_str(),
                    input.clone(),
                    format!("审计日志写不进去，因此没有执行：{error}"),
                    pass_reason.clone(),
                );
                on_event.send(event);
                send.push(message)?;
                continue;
            }

            on_event.send(ChatEvent::Tool {
                id: call.id.clone(),
                name: call.name.clone(),
                status: ToolStatus::Running,
                risk: risk.as_str().into(),
                input: input.clone(),
                output: None,
                arguments: Some(call.arguments.clone()),
                pass_reason: pass_reason.clone(),
                content_chars: Some(call.content_chars),
            });

            // 写文件前先取走旧正文：面板要报行数，回滚要靠它。写失败就不落账——
            // 文件根本没动，记一条就是在记假账。edit_file 同闸：替换在快照那一步
            // 就校验过，校验不过不落账，真错误由执行体报给模型。
            // delete_file 每个路径各预记一条，落账时核对存在性（commit_deleted）。
            // 备份（D3）与快照同一时机动手：改动前的正文只读这一遍
            let backup_options = crate::backup::Options {
                enabled: config.backup_enabled,
                total_mb: config.backup_total_mb,
            };
            let pending_edits: Vec<crate::edits::PendingEdit> = if !via_mcp {
                match call.name.as_str() {
                    "write_file" | "edit_file" => crate::edits::snapshot_before_in(
                        &data_dir,
                        conversation_id,
                        &call.id,
                        &call.name,
                        &args,
                        root.as_deref(),
                        backup_options,
                    )
                    .into_iter()
                    .collect(),
                    "delete_file" => crate::edits::snapshot_delete_before_in(
                        &data_dir,
                        conversation_id,
                        &call.id,
                        &args,
                        root.as_deref(),
                        backup_options,
                    ),
                    _ => Vec::new(),
                }
            } else {
                Vec::new()
            };

            // 三条来源（内置 / 扩展 / 技能）认路由与缓存重试规则都在 `tool_runtime::source` 里，
            // 这里只负责把结果接回去。过去这三个分支各答各的"能不能重试、要不要缓存"
            let skill_body = |name: &str| crate::skills::load_body_in(&config_dir, &data_dir, name);
            let registry = tool_runtime::source::Registry::new(
                root.as_deref(),
                mcp_servers,
                config,
                mcp_hub,
                &skill_body,
                conversation_id,
            );
            // 派单在路由外接走：执行要父话题 id 与配置目录，注册表够不着这两样——
            // 这里两样都在手上（load_skill 归技能路是同款先例：声明在注册表，执行看住处）
            let ran = if !via_mcp && call.name == "goal_report" {
                // 上报动的是这一轮自己那份日志，所以它得在 `send` 上写。另开一次话题去写
                // 同一个文件就是两个写者，后收尾的那一份会把前一份整片盖掉。
                // 完成门的复跑也在这儿给：走的就是 run_command 的真执行路
                // （前台那一条自带 60 秒超时），风险分档由 report_goal 内部问 classify
                let rerun = |command: &str| -> Result<String, String> {
                    registry
                        .run(
                            tool_runtime::source::shared_cache(),
                            "run_command",
                            &serde_json::json!({ "command": command }),
                        )
                        .output
                        .map_err(|error| error.to_string())
                };
                // 目标这一支到头了（complete/blocked 都是终点）：窗口在后台时喊一声。
                // 预算烧到顶那类"没走到上报"的停下不打扰——它没有一句能说清的结论可带
                host.toast_goal_settled(
                    args["status"].as_str() == Some("complete"),
                    args["note"].as_str().unwrap_or_default(),
                );
                tool_runtime::source::Executed {
                    output: report_goal(send, &args, root.as_deref(), &rerun, on_event)
                        .map_err(tool_runtime::source::ToolError::content),
                    source: tool_runtime::source::Kind::Builtin,
                    cached: false,
                    attempts: 1,
                }
            } else if !via_mcp && call.name == "spawn_subagent" {
                tool_runtime::source::Executed {
                    output: host
                        .spawn_subagent(conversation_id, config, &args)
                        .map_err(tool_runtime::source::ToolError::content),
                    source: tool_runtime::source::Kind::Builtin,
                    cached: false,
                    attempts: 1,
                }
            } else if !via_mcp
                && matches!(
                    call.name.as_str(),
                    "cron_list"
                        | "cron_create"
                        | "cron_delete"
                        | "task_run_now"
                        | "plan_mode"
                        | "wait_agent"
                        | "memory_search"
                        | "memory_timeline"
                        | "search_history"
                )
            {
                // 定时任务/规划模式/等待子助理/记忆与历史检索：都要 AppHandle 与
                // 各子系统的状态，注册表够不着——agent_control 是同款先例
                tool_runtime::source::Executed {
                    output: host
                        .subsystem_tool(config, conversation_id, &call.name, &args, stop)
                        .map_err(tool_runtime::source::ToolError::content),
                    source: tool_runtime::source::Kind::Builtin,
                    cached: false,
                    attempts: 1,
                }
            } else if !via_mcp && call.name == "agent_control" {
                let action = args["action"].as_str().unwrap_or_default().to_string();
                let agent_id = args["agent_id"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let message = args["message"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let executed = host.agent_control(&action, &agent_id, &message);
                tool_runtime::source::Executed {
                    output: executed.map_err(tool_runtime::source::ToolError::content),
                    source: tool_runtime::source::Kind::Builtin,
                    cached: false,
                    attempts: 1,
                }
            } else if !via_mcp && call.name == "web_fetch" {
                tool_runtime::source::Executed {
                    output: web_fetch_for_model(config, &args)
                        .map_err(tool_runtime::source::ToolError::content),
                    source: tool_runtime::source::Kind::Builtin,
                    cached: false,
                    attempts: 1,
                }
            } else if !via_mcp && call.name == "web_search" {
                // 联网搜索：执行要配置里的 key、出口名单与代理——web_fetch 是同款先例
                tool_runtime::source::Executed {
                    output: web_search_for_model(config, &args)
                        .map_err(tool_runtime::source::ToolError::content),
                    source: tool_runtime::source::Kind::Builtin,
                    cached: false,
                    attempts: 1,
                }
            } else if !via_mcp && call.name == "browser" {
                // 内置浏览器：执行要 BrowserHub 的状态（拉起/复用浏览器进程与 CDP 通道），
                // 注册表够不着——spawn 与 web_fetch 是同款先例。动作之后的新快照
                // 直接当工具结果交回，模型不需要第二次调用就知道页面变成了什么
                tool_runtime::source::Executed {
                    output: host
                        .browser_tool(config, &args)
                        .map_err(tool_runtime::source::ToolError::content),
                    source: tool_runtime::source::Kind::Builtin,
                    cached: false,
                    attempts: 1,
                }
            } else if !via_mcp && call.name == "obs_recall" {
                // 观察召回：存档住在话题线程的内存里，注册表够不着——
                // spawn 与 web_fetch 是同款先例
                tool_runtime::source::Executed {
                    output: crate::observations::recall(
                        args.get("handle")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                        args.get("start").and_then(Value::as_u64).unwrap_or(0) as usize,
                        args.get("limit").and_then(Value::as_u64).unwrap_or(4000) as usize,
                    )
                    .map_err(tool_runtime::source::ToolError::content),
                    source: tool_runtime::source::Kind::Builtin,
                    cached: false,
                    attempts: 1,
                }
            } else if !via_mcp && call.name == "update_plan" {
                // 计划更新是控制信号：整份转给界面，模型这边只要一句确认。
                // 参数形状已经过了 check_arguments 那道闸，这里只做搬运
                let steps: Vec<PlanStep> = args["steps"]
                    .as_array()
                    .map(|items| {
                        items
                            .iter()
                            .map(|item| PlanStep {
                                title: item["title"].as_str().unwrap_or_default().to_string(),
                                status: item["status"].as_str().unwrap_or("pending").to_string(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                on_event.send(ChatEvent::Plan {
                    explanation: args["explanation"].as_str().map(str::to_string),
                    steps: steps.clone(),
                });
                tool_runtime::source::Executed {
                    output: Ok(format!(
                        "计划已更新（{} 步）。状态变化时整份重发；全部完成后不用再调它。",
                        steps.len()
                    )),
                    source: tool_runtime::source::Kind::Builtin,
                    cached: false,
                    attempts: 1,
                }
            } else if !via_mcp && call.name == "ask_user" {
                // 结构化提问：这一发挂起等用户点选（stop 可打断）。
                // 无人值守没有人可问：直接回一句"自己判断"，别让定时任务在按钮上等一夜
                if crate::tasks::escalate::is_unattended(conversation_id) {
                    tool_runtime::source::Executed {
                        output: Ok(
                            "无人值守运行，没有人可以回答这个问题。按你最有把握的选项继续，\
                             并在结果里说明你替用户做了哪个决定。"
                                .into(),
                        ),
                        source: tool_runtime::source::Kind::Builtin,
                        cached: false,
                        attempts: 1,
                    }
                } else {
                    let options: Vec<AskOption> = args["options"]
                        .as_array()
                        .map(|items| {
                            items
                                .iter()
                                .map(|item| AskOption {
                                    label: item["label"].as_str().unwrap_or_default().to_string(),
                                    description: item["description"].as_str().map(str::to_string),
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    on_event.send(ChatEvent::Ask {
                        id: call.id.clone(),
                        question: args["question"].as_str().unwrap_or_default().to_string(),
                        options,
                    });
                    // 后台话题的提问也是"有人等你"：窗口不在前台时喊一声
                    host.toast_question_pending(args["question"].as_str().unwrap_or_default());
                    let answer = hub.wait_answer(&call.id, stop);
                    tool_runtime::source::Executed {
                        output: Ok(match answer {
                            Some(text) => format!("用户选择了：{text}"),
                            None => "用户没有回答（已停止生成）。按当前信息继续，\
                                     或说明还缺什么才能继续。"
                                .into(),
                        }),
                        source: tool_runtime::source::Kind::Builtin,
                        cached: false,
                        attempts: 1,
                    }
                }
            } else if !via_mcp && call.name == "run_program" {
                // PTC code-mode：模型写 Rhai 脚本，脚本内 tool() 调 Safe 工具（写操作走
                // 常规工具调用的审批闸）。扩展工具（mcp__*）也开，但只走"权限表直接放行"
                // 的那扇门：直调要问人的扩展工具在脚本里执行等于绕开那一声问，不开口子。
                // 常驻变量域按话题取：上一发的 let 这一发还在，reset=true 从零开始
                let script = args["script"].as_str().unwrap_or_default().to_string();
                let exec_root = root.clone();
                let exec_owner = conversation_id.to_string();
                let exec_policy = policy.clone();
                let exec_servers = mcp_servers.to_vec();
                let exec_config = config.clone();
                let exec_hub = mcp_hub.clone();
                let exec = move |name: &str, args_json: &str| -> Result<String, String> {
                    let parsed_args: Value = serde_json::from_str(args_json).unwrap_or(json!({}));
                    if name.starts_with("mcp__") {
                        let call =
                            tool_runtime::Call::new(name, &parsed_args, exec_root.as_deref(), true);
                        if tool_runtime::capabilities_for(&call).iter().any(|cap| {
                            matches!(
                                exec_policy.resolve(cap),
                                crate::policy::Level::Ask | crate::policy::Level::Deny
                            )
                        }) {
                            return Err(format!(
                                "PTC 脚本里的 {name} 过不了权限表（要问人或被禁）。\
                                 退出脚本直调它一次把这一步办了，再回脚本组合结果。"
                            ));
                        }
                        let source = tool_runtime::source::McpSource {
                            servers: &exec_servers,
                            config: &exec_config,
                            hub: &exec_hub,
                        };
                        return tool_runtime::source::ToolSource::call(&source, name, &parsed_args)
                            .map_err(|error| error.text);
                    }
                    let risk = tools::classify(name, &parsed_args, exec_root.as_deref());
                    if risk != tools::Risk::Safe {
                        return Err(format!(
                            "PTC 脚本只能调用只读工具（{name} 是 {}）。写操作请退出脚本后用常规工具调用。",
                            risk.as_str()
                        ));
                    }
                    tools::execute_for(name, &parsed_args, exec_root.as_deref(), Some(&exec_owner))
                };
                if args["reset"].as_bool().unwrap_or(false) {
                    tool_runtime::ptc::forget_scope(conversation_id);
                }
                let scope = tool_runtime::ptc::conversation_scope(conversation_id);
                let mut guard = scope
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let run_result = tool_runtime::ptc::run_in(&script, Box::new(exec), &mut guard);
                tool_runtime::source::Executed {
                    output: run_result
                        .map(|result| result.output)
                        .map_err(tool_runtime::source::ToolError::content),
                    source: tool_runtime::source::Kind::Builtin,
                    cached: false,
                    attempts: 1,
                }
            } else {
                registry.run(tool_runtime::source::shared_cache(), &call.name, &args)
            };
            // 先把"这一份是怎么来的"记下来再移走结果：经过与来源也是要说得出口的事实
            let from = ran.source;
            let note = ran.note();
            let executed = ran.output;

            if executed.is_ok() {
                crate::edits::commit_deleted_in(&data_dir, &pending_edits);
            }
            // 快照事件化：台账落了什么这里就广播什么。pending 里每一笔都是
            // "动手前存了副本（或如实说明没存成）"的一次工具写入，
            // 变更面板即时点亮，审计里也有这一笔
            if !pending_edits.is_empty() {
                for edit in crate::edits::committed_snapshots_in(&data_dir, &pending_edits) {
                    on_event.send(ChatEvent::FileSnapshot {
                        path: edit.path,
                        call_id: edit.call_id,
                        additions: edit.additions,
                        deletions: edit.deletions,
                        backup: edit.backup,
                        snapshot_note: edit.snapshot_note,
                    });
                }
            }

            match executed {
                Ok(output) => {
                    // 技能声明的工具白名单在**取用之后**才生效，并且当场说一句：
                    // 它改变的是模型接下来能调什么，悄悄生效等于让用户猜
                    if from == tool_runtime::source::Kind::Skill {
                        let wanted = args["name"].as_str().unwrap_or_default();
                        let declared =
                            crate::skills::declared_tools_in(&config_dir, &data_dir, wanted);
                        if !declared.is_empty() {
                            let merged = tool_runtime::note_tools(conversation_id, Some(&declared));
                            on_event.send(ChatEvent::Notice {
                                text: format!(
                                    "技能「{wanted}」只用这些工具：{}。名单已从这一刻起生效，之外的工具调用会被拒。",
                                    merged.join(" · ")
                                ),
                            });
                        }
                    }

                    // 执行后钩子：副作用已经发生，撤不掉了，它能做的是把检查结果转给模型。
                    // 发射前重解析：工作区钩子的信任/指纹/撤销在这里即时生效
                    let feedback = {
                        let hooks = crate::hooks::runnable_in(config, &data_dir);
                        if hooks.is_empty() {
                            None
                        } else {
                            let report = crate::hooks::fire(
                                &hooks,
                                "PostToolUse",
                                root.as_deref(),
                                |hook, cwd| {
                                    json!({
                                        "hook_event_name": hook.event,
                                        "cwd": cwd.display().to_string(),
                                        "model": config.model,
                                        "tool_name": call.name,
                                        "tool_input": &args,
                                        "tool_response": &output,
                                    })
                                },
                            );
                            emit_hooks(on_event, &report);
                            report.context()
                        }
                    };

                    // 钩子补的话一起进工具结果：模型看到的和用户看到的是同一份，不留暗账
                    let content = match feedback {
                        Some(text) => format!("{output}\n\n{text}"),
                        None => output,
                    };
                    // 超长结果句柄化：全文归档（obs_recall 按需取回），发给服务商的只有
                    // 首尾摘录与句柄——中间大段不再每轮重放，要用的时候召回来。
                    // 上限 = 契约的 max_output_bytes 与全局钳制取小者（并行后账同一把尺）
                    let tool_result_max = crate::tool_contract::effective_cap(
                        round_contracts[call_index].max_output_bytes,
                        config.tool_result_max_chars,
                    );
                    if content.chars().count() > tool_result_max {
                        crate::observations::archive(&call.id, &content);
                    }
                    let content = pack_tool_result(&content, tool_result_max, Some(&call.id));
                    // 来源标注：话题里那段原文是磁盘读回来的、子进程跑出来的，
                    // 还是扩展给的，用户和模型都得看得出来
                    let content = tool_runtime::annotate(from, &call.name, content);
                    // 命中缓存、或者第几次才送达，都要当场说一句：这两件事改变的是
                    // "模型读到的这段来自哪一版"，藏在计数器里等于没告诉任何人
                    let content = match &note {
                        Some(text) => format!("{content}\n{text}"),
                        None => content,
                    };

                    let (event, message) = tool_result_pair(
                        call,
                        ToolStatus::Done,
                        risk.as_str(),
                        input.clone(),
                        content,
                        pass_reason.clone(),
                    );
                    on_event.send(event);
                    send.push(message)?;
                }
                Err(error) => {
                    // 放行与失败是两件事：审计里"跑失败了"和"根本没让跑"必须分得开
                    let _ = audit_tool_in(
                        &data_dir,
                        conversation_id,
                        &scope,
                        crate::audit::Outcome::Failed,
                        pass_reason.as_deref(),
                    );
                    // PostToolUseFailure：失败也是执行后的一个节点。它的 deny 与
                    // PostToolUse 同义（副作用已经发生），只能转达——拦不住任何事
                    {
                        let hooks = crate::hooks::runnable_in(config, &data_dir);
                        if !hooks.is_empty() {
                            let report = crate::hooks::fire(
                                &hooks,
                                "PostToolUseFailure",
                                root.as_deref(),
                                |hook, cwd| {
                                    json!({
                                        "hook_event_name": hook.event,
                                        "cwd": cwd.display().to_string(),
                                        "model": config.model,
                                        "tool_name": call.name,
                                        "tool_input": &args,
                                        "error": error.to_string(),
                                    })
                                },
                            );
                            emit_hooks(on_event, &report);
                        }
                    }
                    let (event, message) = tool_result_pair(
                        call,
                        ToolStatus::Failed,
                        risk.as_str(),
                        input.clone(),
                        format!("执行失败：{error}"),
                        pass_reason.clone(),
                    );
                    on_event.send(event);
                    send.push(message)?;
                }
            }
        }

        // 插话检查点 2：最后一轮工具结果刚落地时插入的插话，
        // 不捞的话要等下一位用户消息才会被看见
        for text in steering.drain(conversation_id) {
            send.push(Message::User {
                content: format!("（执行中途的插话）{text}"),
                images: Vec::new(),
                audios: Vec::new(),
                videos: Vec::new(),
            })?;
        }
    }

    // 轮数到顶强制收摊：待放的保温没有下一发可等，撤掉再报错。
    // 只有有限的天花板才走得到这里——0 = 不设上限的那一路永远轮不到这句报错
    warm.cancel(conversation_id);
    Err(format!(
        "工具调用达到 {} 轮上限，已停止。{}",
        max_rounds,
        if rounds_cap.is_some() {
            "这一发的天花板是它自己带的那个（子运行的收窄），不是设置里那个。"
        } else {
            "可在设置 → 配置里调高，写 0 则不设上限。"
        }
    ))
}

fn tokens_of(usage: &Option<Usage>) -> crate::usage::Tokens {
    match usage {
        Some(usage) => crate::usage::Tokens {
            input: usage.input_tokens,
            output: usage.output_tokens,
            cached: usage.cached_tokens,
            cache_write: usage.cache_write_tokens,
            reasoning: usage.reasoning_tokens,
        },
        None => crate::usage::Tokens::default(),
    }
}

/// 一轮请求：按配置选线协议。工具循环只认 chat 格式的消息数组，翻译发生在发请求这一刻，
/// 所以钩子、审批、工具结果回填这些逻辑不必知道自己面对的是哪种服务商。
/// 把对话历史压成一份衔接用的摘要。SoL-Pi 的压缩指令原则：
/// 保住已完成的工作、验证结果、重要决策、剩余工作——丢掉这些的压缩等于让 agent 失忆。
/// pi 的结构化摘要模板：六段式检查点 + 精确保留路径/函数名/错误信息。
/// 有前次摘要时切换到增量更新模式（保留旧信息、合并新进展），比全量重摘省得多也更稳
const SUMMARY_SYSTEM: &str = "你是对话摘要助手。阅读用户与 AI 助手的对话，输出一份结构化的上下文检查点摘要，另一个 AI 将用它继续这项工作。不要继续对话，不要回答对话里的任何问题，只输出摘要正文。用中文。";

const SUMMARY_BASE: &str = "严格按以下格式输出摘要：\n\
## 目标\n[用户要完成什么？多个任务可分条]\n\
## 约束与偏好\n- [用户提到的约束、偏好或要求；没有则写（无）]\n\
## 进展\n### 已完成\n- [x] [已完成的任务/改动]\n### 进行中\n- [ ] [当前正在做的]\n### 受阻\n- [阻碍进展的问题；没有则删掉本节]\n\
## 关键决策\n- **[决策]**：[简要原因]\n\
## 下一步\n1. [按顺序列出接下来要做的事]\n\
## 关键上下文\n- [继续工作所需的数据、路径、引用；没有则写（无）]\n\n\
每节保持简洁。精确保留文件路径、函数名和错误信息。";

const SUMMARY_UPDATE: &str = "上方 <previous-summary> 是既有摘要，本次消息要合并进它。规则：\
保留既有摘要的全部信息；合并新对话里的进展、决策与上下文；已完成的事项从「进行中」移到「已完成」；\
根据当前状态更新「下一步」；精确保留文件路径、函数名与错误信息；已不再相关的内容可以移除。\
按与既有摘要相同的六段格式输出更新后的完整摘要。";

/// 摘要调用的输入。把待压段拍平成 `<conversation>` 文本是刻意的：这次请求的前缀
/// 永远不会有第二条请求来延伸，复用它只是白写（设计档 §5.2），所以它不共享话题身份
fn summary_prompt(history: &[Value]) -> String {
    // 上一份摘要单独抽出走增量更新：混进 transcript 会让模型把旧摘要当对话重摘一遍
    let mut previous: Option<String> = None;
    let mut transcript = String::new();
    for message in history {
        let role = message["role"].as_str().unwrap_or_default();
        // 摘要模型同样看不见图片，但它要知道这里有过一张图——只认 as_str() 会把
        // 带图的那一问整条从摘要素材里漏掉
        let content = crate::session::entry::content_text(message);
        if content.is_empty() {
            continue;
        }
        if role == "system" && content.starts_with(SUMMARY_MARKER) {
            previous = Some(
                content
                    .trim_start_matches(SUMMARY_MARKER)
                    .trim()
                    .to_string(),
            );
            continue;
        }
        let label = match role {
            "user" => "用户",
            "assistant" => "助手",
            "tool" => "工具结果",
            _ => "系统",
        };
        transcript.push_str(&format!(
            "{label}：{content}

"
        ));
    }

    let mut prompt = format!(
        "<conversation>
{transcript}
</conversation>

"
    );
    if let Some(prev) = &previous {
        prompt.push_str(&format!(
            "<previous-summary>
{prev}
</previous-summary>

"
        ));
        prompt.push_str(SUMMARY_UPDATE);
    } else {
        prompt.push_str(SUMMARY_BASE);
    }
    prompt
}

/// 摘要请求走与对话同一个出口（`complete_once`）：它曾是单次上下文里最贵的一次调用，
/// 原来却自己拼 HTTP——既不记账、错误分类也重复了一份
fn summarize_history(
    app: &AppHandle,
    config: &AppConfig,
    history: &[Value],
) -> Result<String, String> {
    summarize_history_in(
        &app.path().app_config_dir().map_err(|e| e.to_string())?,
        &app.path().app_data_dir().map_err(|e| e.to_string())?,
        config,
        history,
    )
}

/// worker 变体（M3 收官）：目录由 Main 经 CLI 传来
fn summarize_history_in(
    config_dir: &std::path::Path,
    data_dir: &std::path::Path,
    config: &AppConfig,
    history: &[Value],
) -> Result<String, String> {
    // 压缩前钩子：auto 与手动压缩都从这一条路过。它拦不住压缩（这个事件没有
    // 拒绝语义），能做的是在历史被摘要替换前把现场外发或打点
    {
        // 发射前重解析就是这一份：信任/指纹/撤销的即时性都从这里来
        let hooks = crate::hooks::runnable_in(config, data_dir);
        if hooks.iter().any(|hook| hook.event == "PreCompact") {
            let root = config
                .active_project()
                .map(|project| std::path::PathBuf::from(&project.path));
            crate::hooks::fire(&hooks, "PreCompact", root.as_deref(), |hook, cwd| {
                json!({
                    "hook_event_name": hook.event,
                    "cwd": cwd.display().to_string(),
                })
            });
        }
    }
    let mut one_off = config.clone();
    one_off.temperature = 0.2;
    one_off.max_tokens = one_off.max_tokens.min(2048);
    let messages = json!([
        { "role": "system", "content": SUMMARY_SYSTEM },
        { "role": "user", "content": summary_prompt(history) },
    ]);
    complete_once_in(config_dir, &one_off, messages, "summary")
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalResult {
    pub output: String,
}

/// 终端标签的命令执行。复用 run_command 的执行体（60 秒超时、输出截断都现成）。
///
/// 它**不弹审批**，但不是"绕过闸门"：命令是用户自己在终端标签里敲的，他就是审批人，
/// 向自己请求批准只会多一次没有信息量的点击。它仍然要过两道闸——
/// 权限表里被显式设成 Deny 的那一档（比如把 `exec.arbitrary` 关掉），以及审计落账；
/// 落账写不进去就不执行，与模型那条路同一条规矩
#[tauri::command]
pub fn terminal_exec(
    app: AppHandle,
    command: String,
    cwd: Option<String>,
) -> Result<TerminalResult, String> {
    let command = command.trim().to_string();
    if command.is_empty() {
        return Err("命令为空。".into());
    }
    let root: Option<PathBuf> = cwd
        .filter(|path| std::path::Path::new(path).is_dir())
        .map(PathBuf::from)
        .or_else(|| {
            // 终端与 run_command 同一条根链：项目 → 主目录（未绑定工作目录也能跑）
            config::load(&app).effective_root()
        });

    let args = json!({ "command": command });
    let scope = tool_runtime::Call::new("run_command", &args, root.as_deref(), false);
    let app_config = config::load(&app);
    let policy = app_config.active_policy();
    let audit_root = app.path().app_data_dir().map_err(|e| e.to_string())?;
    if let Err(reason) = terminal_ruling(&policy, &scope, &command) {
        // 拦下来也要有一行。P0 的判据是"任意一次调用都在审计里有一行，且拒绝原因可复述"，
        // 而这一支以前直接 return——红线被碰过几次，账上查不出来
        let _ = terminal_audit(&audit_root, &scope, crate::audit::Outcome::Denied);
        return Err(reason);
    }
    // 落账之后才动手：写不进去就不执行，而不是"记不上也要跑"（与工具那条路同一个约定）
    terminal_audit(&audit_root, &scope, crate::audit::Outcome::Ok)?;

    // 走与模型调用同一条执行路：缓存与重试的规则只写在 `tool_runtime::source` 一处。
    // `run_command` 不是幂等的，而重试的第一条门闩就是"没送到执行体"——所以这一手
    // 对终端来说行为与直接调用完全一样，差别只在这里不再有第二份执行入口
    let ran = tool_runtime::source::run_with(
        // 终端这条手没有话题上下文：后台句柄不认主人，面板不会把它数进任何一条话题
        &tool_runtime::source::BuiltinSource {
            root: root.as_deref(),
            owner: None,
        },
        tool_runtime::source::shared_cache(),
        tool_runtime::source::Retry::default(),
        "run_command",
        &args,
    );
    if ran.output.is_err() {
        // 想做了但没做成，也是一件要说得出的事。只记放行的那一半，
        // 账上就只剩成功，而审计存在的理由是复盘
        let _ = terminal_audit(&audit_root, &scope, crate::audit::Outcome::Failed);
    }
    let output = ran.output.map_err(|error| error.text)?;
    Ok(TerminalResult {
        output: clamp_tool_result(&output, app_config.tool_result_max_chars),
    })
}

/// 终端那一行过不过闸门。**只有 `Deny` 算拒绝**是拍板过的（见 `terminal_exec` 里那段注释）：
/// 命令是用户自己在终端里敲的，他就是审批人，所以 `Ask` 在这里等于放行；
/// 而表上的红线仍然生效
fn terminal_ruling(
    policy: &crate::policy::Policy,
    scope: &tool_runtime::Call,
    command: &str,
) -> Result<(), String> {
    match tool_runtime::rule(policy, scope, command, None).decision {
        crate::policy::Decision::Deny { reason } => Err(reason),
        _ => Ok(()),
    }
}

/// 终端那一行的审计行。归属固定是 `User`——这一行不是模型点的
fn terminal_audit(
    root: &std::path::Path,
    scope: &tool_runtime::Call,
    outcome: crate::audit::Outcome,
) -> Result<(), String> {
    crate::audit::record(
        root,
        crate::audit::Actor::User,
        "terminal:run_command",
        &tool_runtime::audit_target(scope),
        outcome,
    )
}

/// 这一轮发出去的东西按层拆开看。派生视图：每次从日志现算，
/// 面板没有任何写回的路——请求体不能有第二份真相
#[tauri::command]
pub fn context_inspect(
    app: AppHandle,
    conversation_id: String,
) -> Result<crate::session::InspectorReport, String> {
    let config = config::load(&app);
    // 校准样本住在用量台账里，而 `inspect` 只吃日志：那把尺与这份报告都由这里填。
    // 量得不够就是 None，界面上要说"还在估"——同一个系数不许一处实测一处瞎猜
    let calibration = crate::usage::calibration_for(&app, &config.model);
    let opened = open_session(&app, &conversation_id)?;
    let mut report = crate::session::inspector::inspect(
        &conversation_id,
        &opened.log,
        &standing_head(),
        sizing_of(&config, calibration.as_ref()),
        crate::usage::last_prompt_tokens_for(&app, &conversation_id)
            .unwrap_or(0)
            .try_into()
            .ok()
            .filter(|tokens: &u32| *tokens > 0),
    )
    .map_err(|error| error.to_string())?;
    // 谱系在话题 header 里，而 `inspect` 只吃日志与常驻段：这一格由这里填
    report.parent_session_id = opened.header.parent_session.clone();
    // 缓存与重试是进程内的账，不在日志里：同样由这里填，界面上那句"命中几次"才有出处
    report.cache = Some(crate::tool_runtime::source::shared_cache().stats());
    report.calibration = calibration;
    Ok(report)
}

/// 手动压缩：用户在上下文用量面板主动点"立即压缩"。
/// 与自动压缩共用同一条路：读话题日志、写一条 compaction 条目，前端不再送历史下来
#[tauri::command]
pub async fn compact_history(app: AppHandle, conversation_id: String) -> Result<String, String> {
    // 读话题日志 + 发一次摘要请求（网络往返）：都是主线程陪不起的活
    crate::history::run_blocking(move || {
        let config = config::load(&app);
        // 手动压缩与自动压缩走同一条路：读话题日志、压完写一条 compaction 条目。
        // 旧做法是前端把自己的消息数组送下来换一份摘要，界面再自己切片——两边各压各的。
        // 段照常渲染，边界才带得走"当时生效的那份 system"（§6.1 的可重建性）；
        // 这条路**不**同步段：只做摘要，不改历史形状，段差分行留给下一轮对话
        // 记忆段不在压缩边界里带走：它是按本轮提法挑出来的，边界要带走的是长期有效的那几段
        let opened = open_session(&app, &conversation_id)?;
        let mode_body =
            crate::session::mode::section_body(&crate::session::mode::in_effect(&opened.log));
        // 段要重放的是"当时生效的 system"：项目卡与对话回合同一个判定（话题的项目 → 激活项目）
        let sections = conversation_sections(
            project_card_text(
                &config,
                conversation_project(&app, &config, &conversation_id)
                    .or_else(|| config.active_project()),
                crate::worktree::view_for(&app, &conversation_id).as_ref(),
            )
            .as_deref(),
            crate::skills::prompt(&app)?.as_deref(),
            None,
            mode_body.as_deref(),
        );
        let mut send = Send::open(opened, standing_head(), sections)?;
        let history = send.history().to_vec();
        if history.len() < 2 {
            return Err("对话太短，没有可压缩的内容。".into());
        }
        let summary = summarize_history(&app, &config, &history)?;
        let origin = send.provenance()?;
        // 压缩条目要带的是"压之前那一共发出去多少"，口径与自动压缩那处同一个函数：
        // 写 0 会让面板上的"省下多少"永远算错
        let before = crate::session::layers::thread_chars(send.standing(), &history);
        match compaction_boundary(&history, &origin, KEEP_RECENT_CHARS) {
            Some((first_kept_entry_id, _)) => {
                send.append(EntryPayload::Compaction {
                    summary: summary.clone(),
                    first_kept_entry_id,
                    tokens_before: before,
                    usage: None,
                    system_message: send.section_snapshot(),
                })?;
                send.save();
                Ok(summary)
            }
            None => Err("可压缩的内容太少，这次没有压缩。".into()),
        }
    })
    .await
}

/// 要摘要的那一段与"实发的行"对得上吗。这一格此前是 `compact_layer` 里的一个内联条件，
/// 而那条命令要 `AppHandle`，于是它**一次都没被测过**——它守的偏偏是"压错地方"这件事：
/// 投影行与条目不是一一对应的（一条条目可以顶多行、一次撤回可以把一行摘掉），
/// 所以按行号切出来的那几行，在实发数组里可能少一条也可能多一条
fn check_compaction_slice(
    wire_entries: usize,
    matched_entries: usize,
    planned_rows: usize,
) -> Result<(), String> {
    if wire_entries == 0 {
        return Err("要压的那一段在实发的行里对不上，这次没有压缩。".into());
    }
    if matched_entries != planned_rows {
        return Err(format!(
            "要压的那一段在实发的行里对不上（计划 {planned_rows} 行，实发里找到 {matched_entries} 条），这次没有压缩。"
        ));
    }
    Ok(())
}

/// 按层压缩：把历史层里最老的那一段换成一行摘要。
///
/// 它与 `compact_history` 的分工在**断开的位置**：那一条从数组头部断，整个前缀重付一次；
/// 这一条只换中间，`from` 之前已经发过的那批字节一个都不动。所以它省得少、动得也少，
/// 适合"最旧那几轮已经没用了、后面的还想接着说"那种形状
#[tauri::command]
pub fn compact_layer(app: AppHandle, conversation_id: String) -> Result<String, String> {
    let config = config::load(&app);
    let opened = open_session(&app, &conversation_id)?;
    let mode_body =
        crate::session::mode::section_body(&crate::session::mode::in_effect(&opened.log));
    let sections = conversation_sections(
        // 项目卡与对话回合同一个判定（话题的项目 → 激活项目），与 run_turn 同源
        project_card_text(
            &config,
            conversation_project(&app, &config, &conversation_id)
                .or_else(|| config.active_project()),
            crate::worktree::view_for(&app, &conversation_id).as_ref(),
        )
        .as_deref(),
        crate::skills::prompt(&app)?.as_deref(),
        None,
        mode_body.as_deref(),
    );
    let mut send = Send::open(opened, standing_head(), sections)?;
    let sizing = sizing_of(
        &config,
        crate::usage::calibration_for(&app, &config.model).as_ref(),
    );
    let uses = crate::session::layers::uses(&send.opened.log, send.standing())
        .map_err(|error| error.to_string())?;
    let target = crate::session::layers::budget(&uses, sizing)
        .row(crate::session::layers::Layer::History)
        .map(|row| row.target)
        .ok_or("预算表里没有历史层那一行。")?;
    let rows = crate::session::layers::history_rows(&send.opened.log)
        .map_err(|error| error.to_string())?;
    let plan = crate::session::layers::layer_compaction(&rows, target)
        .ok_or("历史层没越界，或者最老那几行不够换一次摘要——这次不该压。")?;
    // 在付那一次摘要请求**之前**先问这一格：`still_over` 说的是"最老那段全换成一行
    // 摘要也坐不进预算"，这时候照压就是白花一笔钱、白少一段历史
    let left: usize = rows
        .iter()
        .map(|row| row.chars)
        .sum::<usize>()
        .saturating_sub(plan.chars);
    if let Some(problem) = layer_compaction_blocker(&plan, left, target) {
        return Err(problem);
    }

    // 要摘要的那几行从投影里按条目 id 取，不按行号切：投影行与条目不是一一对应的，
    // 拿行号当条目用会静默压错地方（`compaction_boundary` 那句注释说的是同一件事）
    let projection =
        crate::session::context::project(&send.opened.log).map_err(|error| error.to_string())?;
    let wanted: std::collections::HashSet<String> =
        rows[..plan.rows].iter().map(|row| row.id.clone()).collect();
    let slice: Vec<Value> = projection
        .entries
        .iter()
        .filter(|(id, _)| wanted.contains(id))
        .flat_map(|(_, messages)| messages.iter().map(|message| message.to_wire()))
        .collect();
    check_compaction_slice(
        slice.len(),
        projection
            .entries
            .iter()
            .filter(|(id, _)| wanted.contains(id))
            .count(),
        plan.rows,
    )?;

    let summary = summarize_history(&app, &config, &slice)?;
    write_layer_summary(&mut send, &plan, &summary)?;
    Ok(summary)
}

/// 这次按层压缩到底该不该动手。`CompactPlan::still_over` 从落地起就一直**只有写、没有读**：
/// 它自己的注释写着"这时候正确的动作不是继续压，而是让阶梯往下一步走，或者干脆报 Notice"，
/// 而 `compact_layer` 原先一路走到摘要那一步。剥成纯函数（只吃三个数，不要 `AppHandle`、
/// 也不要那一次请求）就是为了让这一格有地方测
fn layer_compaction_blocker(
    plan: &crate::session::layers::CompactPlan,
    left: usize,
    target: usize,
) -> Option<String> {
    if !plan.still_over {
        return None;
    }
    let rows = plan.rows;
    Some(format!(
        "最老那 {rows} 行全换成一行摘要，历史层仍要约 {left} 字符，坐不进 {target} 的预算——这一次没有压。\n\
         接着压只会白花一次请求、白少一段历史；要腾地方得走下一步（去掉记忆段，或收窄技能）。"
    ))
}

/// 把最老那一段历史换成一行摘要——只到"写这一行"为止，摘要文本从外面进来。
///
/// 剥出来的理由：整条命令里唯一拿不到的只有那一次摘要请求（要他点头才发），
/// 而**"压完到底生效没有"与"摘要写得好不好"是两件事**，不该被同一道门槛连着挡掉
fn write_layer_summary(
    send: &mut Send,
    plan: &crate::session::layers::CompactPlan,
    summary: &str,
) -> Result<(), String> {
    send.append(EntryPayload::BranchSummary {
        from_id: Some(plan.from_id.clone()),
        through_id: Some(plan.through_id.clone()),
        summary: summary.to_string(),
        usage: None,
    })?;
    send.save();
    Ok(())
}

/// 撤销一次改写：追加一行撤回，历史一条都不删。被那次改写顶替掉的条目就此原样回来，
/// 投影回到压之前的那一版——`context.rs` 的测试钉的是"逐字节相同"，不是"差不多"
#[tauri::command]
pub fn context_undo_compaction(
    app: AppHandle,
    conversation_id: String,
    entry_id: String,
) -> Result<String, String> {
    let mut send = Send::open(
        open_session(&app, &conversation_id)?,
        standing_head(),
        Vec::new(),
    )?;
    let id = revocation(&mut send, &entry_id)?;
    send.save();
    Ok(id)
}

/// 撤回那一行本身。剥成拿 `&mut Send` 的助手，是因为那一级命令只有 `AppHandle` 入口，
/// 于是"命令写的到底是 `None` 还是 `Some(\"\")`"没人钉得往——而这两者在投影里是
/// 两件不同的事：`None` 让被顶替的那些条目原样回来，`Some(\"\")` 是把它们换成空正文
fn revocation(send: &mut Send, target_id: &str) -> Result<String, String> {
    send.append(EntryPayload::ContextEdit {
        target_id: target_id.to_string(),
        replacement: None,
    })
}

/// 话题标题自动生成：第一轮对话结束后用一次极小的请求给话题起名。
/// 失败静默降级——标题只是界面便利，不该让用户看到报错
#[tauri::command]
pub fn generate_title(app: AppHandle, messages: Vec<ChatMessage>) -> Result<String, String> {
    let config = config::load(&app);

    let mut transcript = String::new();
    for message in messages.iter().take(6) {
        if message.content.trim().is_empty() {
            continue;
        }
        let label = if message.role == "user" {
            "用户"
        } else {
            "助手"
        };
        let content: String = message.content.chars().take(400).collect();
        transcript.push_str(&format!("{label}：{content}\n\n"));
    }
    if transcript.trim().is_empty() {
        return Err("没有可总结的对话内容。".into());
    }

    let prompt = format!(
        "根据以下对话开头，给这场话题起一个不超过 12 个字的标题。\
         只输出标题本身：不要引号、不要句号、不要任何解释或前缀。用中文。\n\n对话开头：\n{transcript}"
    );

    let title = complete_once(
        &app,
        &config,
        json!([{ "role": "user", "content": prompt }]),
        "title",
    )?;
    // 服务商偶尔会带引号或"标题："前缀，统剥掉
    let cleaned = title
        .trim()
        .trim_matches('"')
        .trim_matches('「')
        .trim_matches('」')
        .trim_start_matches("标题：")
        .trim()
        .to_string();
    let title: String = cleaned.chars().take(24).collect();
    if title.is_empty() {
        return Err("服务商返回了空标题。".into());
    }
    Ok(title)
}

#[tauri::command]
pub fn enhance_prompt(app: AppHandle, text: String) -> Result<String, String> {
    let draft = text.trim();
    if draft.is_empty() {
        return Err("输入框是空的，没有可增强的内容。".into());
    }
    let mut one_off = config::load(&app);
    one_off.temperature = 0.4;
    one_off.max_tokens = one_off.max_tokens.min(4096);
    let messages = json!([
        { "role": "system", "content": ENHANCE_SYSTEM },
        { "role": "user", "content": draft },
    ]);
    let enhanced = complete_once(&app, &one_off, messages, "enhance_prompt")?;
    let enhanced = enhanced.trim();
    if enhanced.is_empty() {
        return Err("模型没有返回内容，请重试。".into());
    }
    Ok(enhanced.to_string())
}

/// 一次性请求：仍然走 SSE，因为服务商对非流式请求会在生成完成前 504。
/// 调用方不关心逐字增量，所以 emit 是空的。
pub fn complete_once(
    app: &AppHandle,
    config: &AppConfig,
    messages: Value,
    scene: &str,
) -> Result<String, String> {
    complete_once_in(
        &app.path().app_config_dir().map_err(|e| e.to_string())?,
        config,
        messages,
        scene,
    )
}

/// worker 变体（M3 收官）：配置目录由 Main 经 CLI 传来（用量记账落在同一份库）
pub fn complete_once_in(
    config_dir: &std::path::Path,
    config: &AppConfig,
    messages: Value,
    scene: &str,
) -> Result<String, String> {
    if config.base_url.trim().is_empty() {
        return Err("尚未配置推理服务商地址。".into());
    }

    let key = config::api_key(config)?;
    // Copilot 的 keyring 里存的是 ghu_ 主令牌，不是直接可用的密钥：
    // base_url 指到 Copilot 网关的档案，发请求前在这里换成短期 token（自动缓存换发）
    let key = if config.base_url.contains("api.githubcopilot.com") {
        crate::oauth::copilot_access_token(&key, &config.proxy_default)?
    } else {
        key
    };
    let thread = messages.as_array().cloned().unwrap_or_default();
    let started = Instant::now();
    // 定时任务的回合暂无停止入口，给一个永不拉闸的开关占位
    let stop = std::sync::atomic::AtomicBool::new(false);
    let outcome = match request_round(config, &key, &thread, &[], None, &stop, &mut |_| {}) {
        Ok(outcome) => outcome,
        // 一次性调用没有话题日志可写：它的半成品不进任何历史，只记账然后报错。
        // chain_reset 恒为 false：它不属于任何话题的前缀链（conversation_id 是空串）
        Err(failure) => {
            crate::usage::record_turn_in(
                config_dir,
                config,
                scene,
                "",
                &config.model,
                &crate::usage::Tokens::default(),
                0,
                false,
                started.elapsed().as_millis() as u64,
                None,
                false,
                &failure.message,
            );
            return Err(failure.message);
        }
    };

    crate::usage::record_turn_in(
        config_dir,
        config,
        scene,
        "",
        &config.model,
        &tokens_of(&outcome.usage),
        outcome.sent_chars,
        false,
        started.elapsed().as_millis() as u64,
        None,
        true,
        "",
    );

    let text = outcome.text.trim().to_string();
    if text.is_empty() {
        return Err("服务商只回了思考过程或工具调用，没有正文。".into());
    }
    Ok(text)
}

/// 一条判据命令的风险档。弹框要在人写下的那一刻就告诉他"收尾时会不会被复跑"
/// （design-goal-mode.md §5.3）——判据只有 `tools::classify` 这一把尺，界面不另算
#[tauri::command]
pub fn command_risk(
    app: AppHandle,
    conversation_id: String,
    command: String,
) -> Result<String, String> {
    // 与 run_command 同一条根链（worktree → 话题的项目 → 激活项目 → 主目录）：
    // 徽章说的必须是真的。话题绑了项目就用话题的，别让它跟着全局默认漂
    let config = config::load(&app);
    let conversation_root = crate::history::conversation_project_id(&app, &conversation_id)
        .as_deref()
        .and_then(|project_id| config.project_by_id(project_id))
        .map(|project| PathBuf::from(project.path.clone()));
    let root = crate::worktree::root_for(&app, &conversation_id)
        .or(conversation_root)
        .or_else(|| config.effective_root());
    Ok(tools::classify(
        "run_command",
        &serde_json::json!({ "command": command }),
        root.as_deref(),
    )
    .as_str()
    .to_string())
}

/// 契约草案的说给模型听的那段。它只要产出一版**草案**：判据是给人改的，不是给模型
/// 执行的——所以这里不发工具、不落日志，采纳与否全在人（design-goal-mode.md §3.1 入口②③）
const CONTRACT_DRAFT_SYSTEM: &str = r#"你在帮用户把一个目标改写成一份可验证的完成契约。只输出一个 JSON 对象，不要输出别的任何字（不要代码围栏、不要解释）：
{"criteria":[{"text":"判据正文","kind":"check","command":"验证命令"}],"constraints":["约束"]}
规矩：
- criteria 至少 1 条、至多 12 条；每条 text 是一句"怎么才算真做到"，不超过 60 字。
- 能用命令验证的判据用 kind="check" 并给 command（跑测试、grep、脚本——要几秒到几分钟内能跑完的）；只能靠人看的（文案、截图、观感）用 kind="judgment"，不要给它 command。
- command 不认识的就别硬编：宁可标 judgment，也不编一条跑不通的命令。
- constraints 至多 8 条：推进期间不许动的东西（目录、文件、不变量）。
- 判据要可判定：照着它，"做到了没有"必须能回答是或否。不要写"优化""改进"这类没有形状的词。"#;

/// 契约草案：让模型补全判据与约束（design-goal-mode.md §3.1 的入口②③）。
/// 一次性旁路请求，**不落日志、不发工具**——产出是草案，人可改可弃，采纳才立目标
/// （`session_goal_set`）。规划档交了方案时，`source` 就是方案全文：
/// `PLAN_BODY` 本来就要求方案写清"每一步用什么验证"，那正是判据的原料
#[tauri::command]
pub fn goal_criteria_draft(
    app: AppHandle,
    source: String,
) -> Result<crate::goal::contract::Contract, String> {
    let material = source.trim();
    if material.is_empty() {
        return Err("没有可提炼的材料：先写下目标，或让规划档交一份方案。".into());
    }
    let mut one_off = config::load(&app);
    one_off.temperature = 0.2;
    one_off.max_tokens = one_off.max_tokens.min(4096);
    let messages = json!([
        { "role": "system", "content": CONTRACT_DRAFT_SYSTEM },
        { "role": "user", "content": material },
    ]);
    let raw = complete_once(&app, &one_off, messages, "goal_criteria_draft")?;
    // 第一轮没按格式回就强制来一轮修复：把它的原话摆回它面前，只准交 JSON。
    // 只修一轮——两轮都不守规矩的模型，第三轮也不值得再等
    let draft = match parse_contract_draft(&raw) {
        Ok(draft) => draft,
        Err(_) => {
            let repair = json!([
                { "role": "system", "content": CONTRACT_DRAFT_SYSTEM },
                { "role": "user", "content": material },
                { "role": "assistant", "content": raw },
                { "role": "user", "content": "你上一条回复不是一个可解析的 JSON 对象。重新输出：\
                   第一个字符必须是 {，最后一个字符必须是 }，中间只有那份 JSON 本身——\
                   没有围栏、没有解释、没有任何别的字。" },
            ]);
            let retry = complete_once(&app, &one_off, repair, "goal_criteria_draft")?;
            parse_contract_draft(&retry).map_err(|_| {
                "模型两轮都没按格式回草案（要一个 JSON 对象）。判据手写也一样能用：\
                 每条写清\"怎么才算真做到\"，能跑命令的把命令填上。"
                    .to_string()
            })?
        }
    };
    Ok(draft)
}

/// 从模型的回话里抠出契约草案。剥的顺序从严到宽：**整段就是 JSON** → **```json 围栏
/// 逐个剥**（围栏外的解释性文字里可能有花括号，先剥围栏再抠括号更准）→ **第一个
/// `{` 到最后一个 `}`** 兜底。id 由后端补铸，空行丢弃。**草案不校验上限**：
/// 超没超是 `session_goal_set` 那道门口的事，人改完自然会知道
fn parse_contract_draft(raw: &str) -> Result<crate::goal::contract::Contract, String> {
    use crate::goal::contract::{Contract, Criterion, CriterionKind};

    let malformed = || "模型没按格式回草案（要一个 JSON 对象）。再试一次，或手写判据。";
    let trimmed = raw.trim();
    let mut parsed: Option<serde_json::Value> = None;
    // 候选从严到宽排队，第一个能解析的赢
    let mut candidates: Vec<&str> = vec![trimmed];
    let mut rest = trimmed;
    while let Some(offset) = rest.find("```") {
        let Some(end_rel) = rest[offset + 3..].find("```") else {
            break;
        };
        let block = rest[offset + 3..offset + 3 + end_rel].trim();
        // 围栏第一行可能是语言名（json / json5 …）：只含字母数字且不带花括号才算
        let body = match block.split_once('\n') {
            Some((first, tail))
                if !first.contains('{')
                    && first.trim().chars().all(|c| c.is_ascii_alphanumeric()) =>
            {
                tail.trim()
            }
            _ => block,
        };
        candidates.push(body);
        rest = &rest[offset + 3 + end_rel + 3..];
    }
    if let (Some(start), Some(end)) = (trimmed.find('{'), trimmed.rfind('}')) {
        if end > start {
            candidates.push(&trimmed[start..=end]);
        }
    }
    for candidate in candidates {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(candidate) {
            parsed = Some(value);
            break;
        }
    }
    let Some(parsed) = parsed else {
        return Err(malformed().into());
    };
    let mut criteria = Vec::new();
    for (index, item) in parsed["criteria"]
        .as_array()
        .ok_or_else(malformed)?
        .iter()
        .enumerate()
    {
        let text = item["text"].as_str().unwrap_or("").trim().to_string();
        if text.is_empty() {
            continue;
        }
        let kind = match item["kind"].as_str() {
            Some("check") => CriterionKind::Check {
                command: item["command"].as_str().unwrap_or("").trim().to_string(),
            },
            _ => CriterionKind::Judgment,
        };
        criteria.push(Criterion {
            id: format!("c{}", index + 1),
            text,
            kind,
        });
    }
    let constraints = parsed["constraints"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|item| item.as_str().map(str::trim))
                .filter(|text| !text.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Ok(Contract {
        criteria,
        constraints,
    })
}

#[cfg(test)]
mod wire_format_tests {
    use super::mode_goal::close_round;
    use super::*;
    use wire::{
        affinity_headers, anthropic_payload, apply_anthropic_event, apply_chat_event,
        apply_gemini_event, apply_responses_event, chat_payload, gemini_payload, partial_of,
        read_events, responses_input, responses_payload, responses_tools, retry_429_delay,
        ChatState, ModalInputs, ResponsesState, StreamItem,
    };

    /// 显式状态机：合法迁移照常翻译，非法迁移当场崩——
    /// "轮数没记账的续跑"与"空正文的排队轮"是判据/队列坏掉的实锤，
    /// 静默放行会把僵局伪装成正常结束
    #[test]
    fn next_translations_crash_on_illegal_shapes() {
        use crate::session::mode::State;

        assert!(matches!(Next::Stop.into_step(), Step::Stop));
        let mut armed = State::default();
        armed.turns_used = 1;
        assert!(matches!(
            Next::Go {
                state: armed,
                notice: Some("接着做".into())
            }
            .into_step(),
            Step::GoalRound {
                notice: Some(_),
                ..
            }
        ));
        assert!(matches!(
            Next::RunUser {
                text: "插一句".into()
            }
            .into_step(),
            Step::UserTurn { .. }
        ));

        let zero_rounds = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Next::Go {
                state: State::default(),
                notice: None,
            }
            .into_step()
        }));
        assert!(
            zero_rounds.is_err(),
            "turns_used = 0 的续跑是非法迁移，必须崩"
        );
        let empty_text = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Next::RunUser { text: "   ".into() }.into_step()
        }));
        assert!(empty_text.is_err(), "空正文的排队轮是非法迁移，必须崩");
    }

    /// 花费读数的三种长相：**没起算点 = 0**（这一支还没开始烧）、**账读不出来 = 不知道**、
    /// **读得出来 = 那个数**。中间那一种不许塌成 0：判据那一头拿不到账会直接停下
    /// （花费上限是唯一的自动刹车），界面若跟着显示 `$0.00` 就是"一侧停下、另一侧说没花"
    /// 草案解析的四层剥法：整段 JSON → 围栏（可带语言名）→ 兜底的花括号跨度。
    /// 围栏外的解释性文字里可能带花括号——先剥围栏再抠括号，不然兜底跨度会把
    /// "以上{纯属解释}"一起吞进去然后解析失败
    #[test]
    fn the_draft_parser_peels_fences_before_it_grabs_braces() {
        let bare = r#"{"criteria":[{"text":"测试全绿","kind":"check","command":"npm test"}],"constraints":["不改 src-tauri/**"]}"#;
        let draft = parse_contract_draft(bare).expect("纯 JSON 该过");
        assert_eq!(draft.criteria.len(), 1);
        assert_eq!(draft.criteria[0].id, "c1", "id 由后端补铸");
        assert_eq!(draft.constraints, vec!["不改 src-tauri/**"]);

        let fenced = format!("```json\n{bare}\n```");
        assert_eq!(parse_contract_draft(&fenced).unwrap(), draft);

        let noisy = format!("好的，这是草案：\n```json\n{bare}\n```\n以上{{纯属解释}}");
        assert_eq!(parse_contract_draft(&noisy).unwrap(), draft);

        let chatty = format!("草案如下 {bare} 以上。");
        assert_eq!(parse_contract_draft(&chatty).unwrap(), draft);

        // 两轮都不守规矩（修复轮也救不回来的形状）：报错，但话说得能让人接着手写
        let junk = parse_contract_draft("我觉得吧，没有 JSON");
        assert!(junk.is_err());
        assert!(junk.unwrap_err().contains("JSON"));
    }

    #[test]
    fn a_ledger_that_cannot_be_read_is_not_zero() {
        let mut touched = false;
        assert_eq!(
            spent_reading(None, |_| {
                touched = true;
                Ok(5)
            }),
            Some(0),
            "没有起算点就是没花钱"
        );
        assert!(!touched, "没起算点就不该去开台账：那是白一次磁盘 IO");
        assert_eq!(spent_reading(Some(7), |_| Ok(123)), Some(123));
        assert_eq!(
            spent_reading(Some(7), |_| Err("台账读不出来".into())),
            None,
            "读不出账要报「不知道」，不许报 0"
        );
    }

    /// 审查那一发走哪套连接：点名档案 = 连接域整体换过去；点名模型 = 模型行再盖、
    /// 读数跟着换；档案被删了 = 原样回落——审查跟着当前连接走，而不是审查不了
    #[test]
    fn the_review_call_rides_the_named_profile_and_model() {
        let mut config = AppConfig::default();
        config.base_url = "https://main.example/v1".into();
        config.model = "main-model".into();
        let mut cheap = AppConfig::default();
        cheap.base_url = "https://cheap.example/v1".into();
        cheap.model = "cheap-fast".into();
        cheap.context_tokens = 32_000;
        let profile = crate::config::profile_from_config("rev-1".to_string(), "审查号", &cheap);
        config.profiles.push(profile);

        // 没点名：审查跟着当前连接走
        let same = reviewer_connection(&config);
        assert_eq!(same.base_url, "https://main.example/v1");
        assert_eq!(same.model, "main-model");

        // 点名档案：连接域整体换过去，档案自己的默认模型与读数跟着走
        config.auto_review_profile_id = "rev-1".into();
        let routed = reviewer_connection(&config);
        assert_eq!(routed.base_url, "https://cheap.example/v1");
        assert_eq!(routed.model, "cheap-fast");
        assert_eq!(routed.context_tokens, 32_000);

        // 再点名模型：模型行盖上，读数（窗口）跟着换——不换的话压缩阈值读的是错的那一行
        cheap.models.push(crate::config::ModelSpec {
            model: "cheap-mini".into(),
            context_tokens: 8_000,
            max_tokens: 0,
            reasoning_effort: None,
            effort_levels: Vec::new(),
            supports_images: false,
            supports_video: false,
            supports_audio: false,
            delegatable: true,
            capabilities: Vec::new(),
        });
        config.profiles[0] =
            crate::config::profile_from_config("rev-1".to_string(), "审查号", &cheap);
        config.auto_review_model = "cheap-mini".into();
        let tuned = reviewer_connection(&config);
        assert_eq!(tuned.model, "cheap-mini");
        assert_eq!(tuned.context_tokens, 8_000, "读数跟着点名的那一行走");

        // 档案被删了：原样回落到当前连接，审查照常
        config.profiles.clear();
        config.auto_review_profile_id = "gone".into();
        config.auto_review_model = String::new();
        let fell_back = reviewer_connection(&config);
        assert_eq!(fell_back.base_url, "https://main.example/v1");
        assert_eq!(fell_back.model, "main-model");
    }

    /// 收尾那两发的先后，与 `continuing` 那一格的映射。反了不会报错，只会让人看到
    /// "它自己停了"而线程还在跑——那种坏法最难查，所以连先后都要有一根针
    #[test]
    fn the_reading_goes_out_before_done() {
        struct Recorder(std::sync::Mutex<Vec<String>>);

        impl EventSink for Recorder {
            fn send(&self, event: ChatEvent) {
                let line = match event {
                    ChatEvent::Mode { continuing, state } => {
                        format!("mode:{}:{}", continuing, state.mode)
                    }
                    ChatEvent::Done { .. } => "done".to_string(),
                    _ => "别的".to_string(),
                };
                self.0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(line);
            }
        }

        let view = || ModeView {
            mode: "goal",
            objective: Some("把三处对账补齐".into()),
            turns_used: 1,
            max_cost_usd_e8: 0,
            spent_usd_e8: Some(0),
            status: "active",
            note: None,
            profile: None,
            goal_id: Some("goal-1".into()),
            contract: None,
            plan_ready: false,
        };
        let done = || ChatEvent::Done {
            input_tokens: 10,
            output_tokens: 5,
            duration_ms: 100,
            cached_tokens: None,
            entry_ids: Vec::new(),
            model: "测试模型".into(),
            context_tokens: 128_000,
        };

        // 要接下一轮：读数先走，并且那一格说"还在跑"
        let going = Recorder(std::sync::Mutex::new(Vec::new()));
        close_round(
            &going,
            &Next::Go {
                state: crate::session::mode::State::default(),
                notice: None,
            },
            Some(view()),
            done(),
        );
        assert_eq!(
            *going
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec!["mode:true:goal", "done"]
        );

        // 停下：读数照样先走，但那一格改成"不再接了"——界面据此决定要不要开幕等下一轮
        let halt = Recorder(std::sync::Mutex::new(Vec::new()));
        close_round(&halt, &Next::Stop, Some(view()), done());
        assert_eq!(
            *halt
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec!["mode:false:goal", "done"]
        );

        // 不是目标模式：一颗读数都不发，Done 单独走——默认档的行为一个字节都没变
        let plain = Recorder(std::sync::Mutex::new(Vec::new()));
        close_round(&plain, &Next::Stop, None, done());
        assert_eq!(
            *plain
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec!["done"]
        );
    }

    /// `ModeView` 是手抄进 `src/types/chat.ts` 的 `ModeState`，两边都不是编译器能看见的
    /// 同一个类型：漂一个字段，界面上那格就是 `undefined` 冒充 0，而 0 在这张表里
    /// 的意思是"不设上限"。所以逐字段对账，和配置合同那条针同一个办法
    #[test]
    fn the_mode_view_matches_the_frontend_interface() {
        // 交互档是对话、身上却挂着一个在推进的目标：这正是改造之后的常态，
        // 读数得能把这件事整份说清楚（目标、轮数、钱、点名的档案）
        let view = ModeView {
            mode: "chat",
            objective: Some("把三处对账补齐".into()),
            turns_used: 3,
            max_cost_usd_e8: 250_000_000,
            spent_usd_e8: Some(40_000_000),
            status: "active",
            note: None,
            profile: Some("档案甲".into()),
            goal_id: Some("goal-abc".into()),
            contract: None,
            plan_ready: false,
        };
        let held = serde_json::to_value(&view).expect("读数序列化该成功");
        crate::test_support::assert_matches_ts(&held, "ModeState");

        // 没点名档案：那一格序列化成 null，键仍然要在——少了键，界面上就是 undefined
        let plain = ModeView {
            objective: None,
            turns_used: 0,
            max_cost_usd_e8: 0,
            spent_usd_e8: Some(0),
            profile: None,
            ..view
        };
        let held = serde_json::to_value(&plain).expect("读数序列化该成功");
        assert!(held["profile"].is_null(), "没点名就该是 null");
        crate::test_support::assert_matches_ts(&held, "ModeState");

        // 事件那一格的 tag 也是手抄的：store 的 switch 认的是 "mode"，写成别的就永远不进那一支
        let event = serde_json::to_value(ChatEvent::Mode {
            continuing: true,
            state: plain.clone(),
        })
        .expect("事件序列化该成功");
        assert_eq!(event["type"], "mode", "界面按这个字面串分派");
        assert_eq!(event["continuing"], true);
        crate::test_support::assert_matches_ts(&event["state"], "ModeState");

        // 那三条会改日志的命令回的是同一个读数包。`deferred` 漂了，界面就不会再说
        // "排在这一轮后面"——用户按下切档、屏幕上什么都没有，正是这一格要拦的形状
        let outcome = serde_json::to_value(ModeOutcome {
            view: plain,
            deferred: true,
        })
        .expect("读数包序列化该成功");
        crate::test_support::assert_matches_ts(&outcome, "ModeOutcome");
        assert_eq!(outcome["deferred"], true, "deferred 是这一格的字面名");
        crate::test_support::assert_matches_ts(&outcome["view"], "ModeState");
    }

    /// 界面交来的目标请求要经得住乱填。空目标会被读成"没有方向"，负数与乱字会被读成
    /// "不设上限"——两种都不是用户的意思。切档那条命令不再收目标：
    /// 一格命令改一件事，goal 走 session_goal_set 的那套（§3.2）
    #[test]
    fn the_goal_command_refuses_every_way_of_not_saying_a_goal() {
        use crate::session::mode::{State, Working};

        let goal = |objective: Option<&str>, cost: Option<&str>| {
            goal_state_from(
                objective.unwrap_or("").to_string(),
                None,
                cost,
                7,
                &State::default(),
                None,
                false,
            )
        };

        // 没写下目标就没有方向，空串与全是空格都算没写
        for blank in ["", "   "] {
            assert!(
                goal(Some(blank), None).is_err(),
                "空目标不许落进日志：{blank:?}"
            );
        }

        // 钱：填不进 f64 的、负的、nan/inf 的都拒；空串才是"不设上限"
        for junk in ["-1", "abc", "nan", "inf", "1.2.3"] {
            assert!(
                goal(Some("x"), Some(junk)).is_err(),
                "{junk} 不该被读成一个上限"
            );
        }
        assert_eq!(
            goal(Some("x"), Some("2.5")).unwrap().max_cost_e8,
            250_000_000
        );
        assert_eq!(goal(Some("x"), Some("  ")).unwrap().max_cost_e8, 0);
        assert_eq!(goal(Some("x"), None).unwrap().max_cost_e8, 0);

        // 起一支新目标是从零开始：轮数 0、推进中、起算点就是这一次、身份是新铸的
        let fresh = goal(Some("补齐三处对账"), None).unwrap();
        assert_eq!(fresh.working, Working::Goal);
        assert_eq!(fresh.turns_used, 0);
        assert_eq!(fresh.started_at, Some(7));
        assert_eq!(fresh.objective.as_deref(), Some("补齐三处对账"));
        assert!(fresh
            .goal_id
            .as_deref()
            .is_some_and(|id| id.starts_with("goal-")));

        // 切档那条命令不再收 goal：目标是一份契约，不是一次切档
        assert!(mode_state_from("goal", &State::default()).is_err());
        // 认不出的模式名要报错，不能悄悄退回对话模式——那等于替用户切了档
        for junk in ["", "yolo", "full", "目标"] {
            assert!(
                mode_state_from(junk, &State::default()).is_err(),
                "{junk} 不是模式名"
            );
        }
    }

    /// 替换要确认：同一支上已有一支**在推进**的目标、这次写的又是另一句时，
    /// 没有 `force` 就不许落——静默替换等于把旧目标的账与结论一起蒸发（§3.2）。
    /// 同一句目标是认领，不走这道闸；停着的目标替换也不走（它没在烧钱）
    #[test]
    fn replacing_a_running_goal_needs_force() {
        use crate::session::mode::{State, Status, Working};

        let held = State {
            working: Working::Goal,
            objective: Some("补齐三处对账".into()),
            started_at: Some(3),
            turns_used: 5,
            status: Status::Active,
            goal_id: Some("goal-old".into()),
            ..Default::default()
        };
        let set = |objective: &str, force: bool| {
            goal_state_from(objective.to_string(), None, None, 9, &held, None, force)
        };
        // 在推进 + 换了目标 + 没确认：拒
        assert!(set("另一件事", false).is_err(), "静默替换不许落");
        // 确认了：另起一支——新身份、新起算点、轮数归零
        let replaced = set("另一件事", true).unwrap();
        assert_eq!(replaced.turns_used, 0);
        assert_eq!(replaced.started_at, Some(9));
        assert_ne!(
            replaced.goal_id.as_deref(),
            Some("goal-old"),
            "替换是新的一支"
        );
        // 同一句目标：认领，不走这道闸（账全留）
        let claimed = set("补齐三处对账", false).unwrap();
        assert_eq!(claimed.turns_used, 5, "认领延续旧账");
        assert_eq!(
            claimed.goal_id.as_deref(),
            Some("goal-old"),
            "认领延续同一支"
        );

        // 停着的目标（暂停/完成）替换不需要确认：它没在往下烧钱
        let paused = State {
            status: Status::Paused,
            ..held.clone()
        };
        let done = State {
            status: Status::Complete,
            note: Some("齐了".into()),
            ..held
        };
        for stopped_held in [paused, done] {
            assert!(
                goal_state_from("另一件事".into(), None, None, 9, &stopped_held, None, false)
                    .is_ok(),
                "停着的 {:?} 不该拦替换",
                stopped_held.status
            );
        }
    }

    /// 切走不销毁，也**不再挂起**：目标留在主格，对话档下它照常在后台推进。
    /// 旧行为是把整份目标挪进挂起格，于是"切去聊两句"等于把目标停了——那正是这次要改掉的
    #[test]
    fn switching_to_chat_keeps_the_goal_running() {
        use crate::session::mode::{State, Status, Working};

        let goal = State {
            working: Working::Goal,
            objective: Some("补齐三处对账".into()),
            started_at: Some(3),
            turns_used: 5,
            max_cost_e8: 250_000_000,
            profile: Some("档案甲".into()),
            ..Default::default()
        };

        let back = mode_state_from("chat", &goal).unwrap();
        assert_eq!(back.working, Working::Chat);
        assert_eq!(
            back.objective.as_deref(),
            Some("补齐三处对账"),
            "目标留在主格"
        );
        assert_eq!(back.turns_used, 5, "轮数是账，切个档不能抹");
        assert_eq!(back.started_at, Some(3), "起算点不能漂——漂了花费就重算");
        assert_eq!(back.max_cost_e8, 250_000_000);
        assert_eq!(
            back.profile.as_deref(),
            Some("档案甲"),
            "点名的档案跟着目标走"
        );
        // 这一条就是改造本身：判据认目标不认交互档，切到对话档它还得算"在跑"
        assert!(back.goal_active(), "切到对话档目标不该停");
        assert!(back.goal_held(), "也没人按暂停，它就该接着自己往下跑");
        // 旧版那一格不再写出：读侧靠它认旧日志，写侧再写一份就是第二个真相
        assert_eq!(back.suspended, None, "切档不再写挂起格");

        // 对话↔规划之间来回切，目标整份跟着走；规划档只把它按住，不销毁
        let planned = mode_state_from("plan", &back).unwrap();
        assert_eq!(planned.working, Working::Plan);
        assert_eq!(
            planned.objective.as_deref(),
            Some("补齐三处对账"),
            "规划档不许把目标弄丢"
        );
        assert_eq!(planned.turns_used, 5);
        assert!(planned.paused(), "只读红线让目标寸步难行，规划档自动转暂停");
        // 这两句同时成立，才是"暂停不是收尾"：还挂着账，但不再自己往下接。
        // 从前它们要靠 outcome + paused 两格互相解释，现在是一格里的事
        assert_eq!(planned.status, Status::Paused, "按继续就接得回来：它没翻篇");
        assert!(
            planned.goal_held() && !planned.goal_active(),
            "goal_held 与 goal_active 分家的地方就在这儿：收尾了的不算挂着，暂停中的算"
        );

        // 从规划切回对话：暂停是规划档替用户按的，切回来也不自动翻回去——
        // "要不要继续推"由人决定，让一档交互设置替用户重按播放键等于偷偷花钱
        let chats = mode_state_from("chat", &planned).unwrap();
        assert!(chats.paused(), "从规划切回对话不该自己接着烧钱");

        // 对话档上没有目标时，什么旗子都不立
        let plain = mode_state_from("chat", &State::default()).unwrap();
        assert_eq!(
            plain,
            State {
                working: Working::Chat,
                ..Default::default()
            }
        );
    }

    /// 切回目标：同一句目标（且还没收尾）整份接上——轮数与起算点延续、暂停翻回去，
    /// 上限与点名的档案以这一次填的为准；换了目标才是重新开始（那要 force，见上条针）
    #[test]
    fn switching_back_claims_the_goal_still_on_the_session() {
        use crate::session::mode::{State, Status, Working};

        // 目标是从对话档的**主格**里读出来的，不再有一份挂起格
        let held = State {
            working: Working::Chat,
            objective: Some("补齐三处对账".into()),
            started_at: Some(3),
            turns_used: 5,
            max_cost_e8: 250_000_000,

            status: Status::Paused,
            profile: Some("档案甲".into()),
            ..Default::default()
        };
        let set = |objective: &str, cost: Option<&str>, profile: Option<&str>| {
            goal_state_from(
                objective.to_string(),
                None,
                cost,
                9,
                &held,
                profile.map(str::to_string),
                false,
            )
            .unwrap()
        };

        // 同一句目标：接上旧账，上限与档案跟这次填的走
        let resumed = set("补齐三处对账", Some("1"), Some("档案乙"));
        assert_eq!(resumed.working, Working::Goal);
        assert_eq!(resumed.turns_used, 5, "轮数是接回来的，不是从零数");
        assert_eq!(
            resumed.started_at,
            Some(3),
            "起算点延续——前面那几发的钱还算数"
        );
        assert_eq!(resumed.max_cost_e8, 100_000_000, "上限以这次填的为准");
        assert_eq!(
            resumed.profile.as_deref(),
            Some("档案乙"),
            "档案以这次点名的为准"
        );
        assert_eq!(resumed.status, Status::Active);
        assert!(
            !resumed.paused(),
            "切回目标就是把暂停翻回去：用户要的就是它往下跑"
        );

        // 这次没点名档案 = 跟随当前配置，不是沿用上一轮的点名
        let follow = set("补齐三处对账", None, None);
        assert_eq!(
            follow.profile, None,
            "没点名就是跟随当前配置，不继承旧的点名"
        );

        // 换了目标：全新的一支，旧账就地作废（held 是停着的，不需要 force）
        let fresh = set("另一件事", None, None);
        assert_eq!(fresh.turns_used, 0);
        assert_eq!(fresh.started_at, Some(9));
        assert_eq!(fresh.max_cost_e8, 0);

        // 已收尾的目标不该被"认领"成续跑——收尾是事实，重定一个才是往下走
        let done = State {
            status: Status::Complete,
            note: Some("齐了".into()),
            ..held.clone()
        };
        let restarted =
            goal_state_from("补齐三处对账".into(), None, None, 9, &done, None, false).unwrap();
        assert_eq!(restarted.turns_used, 0, "收尾过的目标不接旧账");
        assert_eq!(restarted.status, Status::Active);
    }

    /// 回合中立起的那一格切档请求，落行时吃的是**落那一刻**的账。这一条单独钉，是因为
    /// "当场切"与"排队切"必须走同一个 `mode_state_from`——两处各算一遍迟早漂成两种形状，
    /// 而排队那一路只在目标模式的轮次边界上跑，本来就是最测不到的那一条
    #[test]
    fn a_queued_mode_change_lands_with_the_same_rules_as_an_immediate_one() {
        use crate::session::mode::{State, Status, Working};

        let goal = State {
            working: Working::Goal,
            objective: Some("补齐三处对账".into()),
            started_at: Some(3),
            turns_used: 5,
            max_cost_e8: 250_000_000,
            status: Status::Active,
            profile: Some("档案甲".into()),
            ..Default::default()
        };
        let switch = |raw: &str| PendingMode::Switch {
            mode: raw.to_string(),
        };

        // 排到对话档：与当场切逐字节相同——目标留在主格，并且还在往下跑
        let queued = mode_after_request(&switch("chat"), &goal, 9).unwrap();
        let direct = mode_state_from("chat", &goal).unwrap();
        assert_eq!(queued, direct, "排队切与当场切必须落成同一个形状");
        assert!(queued.goal_active(), "对话档下它就该接着自己往下跑");

        // 排到规划档：同一条只读红线，自动按住而不是销毁
        let queued = mode_after_request(&switch("plan"), &goal, 9).unwrap();
        assert_eq!(queued.working, Working::Plan);
        assert!(queued.paused(), "规划档替用户把目标按住");
        assert_eq!(queued.objective, goal.objective, "按住的不是删掉的");

        // 排队那几秒里这一轮自己往前推进过（轮数加过、档案换过），落行时吃的是新账——
        // 按下那一刻算好的那一行若照原样写回去，会把中间那一轮的账抹平
        let advanced = State {
            turns_used: 8,
            profile: Some("档案乙".into()),
            ..goal.clone()
        };
        let chats = mode_after_request(&switch("chat"), &advanced, 9).unwrap();
        assert_eq!(chats.turns_used, 8, "排队不该把轮数退回按下那一刻");
        assert_eq!(
            chats.profile.as_deref(),
            Some("档案乙"),
            "也不该把点名的档案退回旧的"
        );

        // 结束目标：清干净，目标档回对话档
        let cleared = mode_after_request(&PendingMode::Discard, &goal, 9).unwrap();
        assert_eq!(
            cleared,
            State::default(),
            "结束之后身上没有目标，档回到对话"
        );
        // 规划档上结束就停在规划档：那一档是人此刻要的交互方式，不是目标的附属
        let on_plan = State {
            working: Working::Plan,
            ..goal.clone()
        };
        let cleared = mode_after_request(&PendingMode::Discard, &on_plan, 9).unwrap();
        assert_eq!(cleared.working, Working::Plan, "结束目标不该替人改交互档");
        assert_eq!(cleared.objective, None);

        // 认不出的档名即使在排队，也要在落行时拒绝，而不是悄悄切成对话
        assert!(mode_after_request(&switch("yolo"), &goal, 9).is_err());
    }

    /// 续跑只动轮数那一格。别的字段被顺手改掉，目标的账就说不清了——
    /// 起算点跟着漂尤其坏：花费会从中间重算，前面那几发等于没花
    #[test]
    fn arming_a_goal_round_bumps_only_the_round_count() {
        use crate::session::mode::{State, Status, Working};

        let held = State {
            working: Working::Goal,
            objective: Some("把三处对账补齐".into()),
            started_at: Some(7),
            turns_used: 2,
            max_cost_e8: 9,
            status: Status::Active,
            note: Some("留着".into()),
            profile: Some("档案甲".into()),
            goal_id: Some("goal-1".into()),
            contract: None,
            suspended: None,
            outcome: None,
            paused: None,
        };
        let next = held.armed();
        assert_eq!(next.turns_used, 3);
        assert_eq!(next.started_at, held.started_at, "起算点不能漂");
        assert_eq!(next.max_cost_e8, held.max_cost_e8);
        assert_eq!(next.objective, held.objective);
        assert_eq!(next.note, held.note);
        assert_eq!(next.outcome, held.outcome);
        assert_eq!(next.profile, held.profile, "点名的档案得跟着每一轮");
        assert_eq!(
            next.goal_id, held.goal_id,
            "身份也得跟着：换了它，护栏与分叉分组就认不出这一支了"
        );
        assert!(!next.paused(), "续跑那一格与暂停无关，不该顺手翻旗");
    }

    /// 停止闸的登记与拉闸。要钉的是**没登记**那一支：`chat_abort` 以前对不存在的话题
    /// 也返回 `Ok(())`，于是"点了停止"与"这一发早就不跑了"在界面上长得一模一样
    /// 迟到的插话必须被拒绝，而不是把已释放的队列复活出来——
    /// 复活了就永远没人消费，用户看着自己的话蒸发却没有任何报错
    #[test]
    fn late_steering_after_release_is_rejected_not_resurrected() {
        let hub = SteeringHub::default();
        hub.register("conv");
        hub.push("conv", "在跑时插的话")
            .expect("在跑的回合必能入队");
        assert_eq!(hub.drain("conv").len(), 1, "回合内照常消费");
        hub.release("conv");
        assert!(
            hub.push("conv", "迟到的话").is_err(),
            "回合收尾后的插话要报错，前端拿这句去降级成新消息"
        );
        assert!(hub.drain("conv").is_empty(), "报错的话不能还在队列里");
    }

    /// 排队同一条生命周期规则；release 连同没消费的排队一起带走，
    /// 下一回合的 register 从干净状态开始
    #[test]
    fn late_follow_up_after_release_is_rejected_and_earlier_ones_do_not_haunt() {
        let hub = FollowUpHub::default();
        hub.register("conv");
        assert_eq!(hub.push("conv", "排队一").expect("在跑的回合必能入队"), 1);
        hub.release("conv");
        assert!(hub.push("conv", "迟到的排队").is_err());
        assert_eq!(hub.pop("conv"), None, "队列随回合收尾一起走，不留悬案");
        hub.register("conv");
        assert_eq!(
            hub.push("conv", "新回合的排队")
                .expect("重新登记后照常入队"),
            1
        );
        assert_eq!(hub.pop("conv").as_deref(), Some("新回合的排队"));
    }

    #[test]
    fn abort_pulls_the_registered_flag_and_says_so_when_there_is_none() {
        let hub = StopHub::default();
        let flag = hub.register("conv-live").expect("干净的话题必能登记");
        assert!(!stopped(&flag), "刚登记的闸不该是拉起来的");

        assert!(hub.abort("conv-live").is_ok());
        assert!(stopped(&flag), "拉过的闸必须是拉起来的");

        // 没登记过的那一条：报错，不许假报成功，也不许顺手拉起别人的闸
        let other = hub.register("conv-other").expect("另一条话题照常登记");
        assert!(hub.abort("conv-gone").is_err());
        assert!(!stopped(&other), "认错话题名时不该停到别人那一发");

        // 已登记的话题再登记一次必须被拒：覆盖会把旧那轮手里的旗标顶掉，
        // 它从此对停止失联（双发竞态的根）
        assert!(
            hub.register("conv-live").is_err(),
            "重复登记要报错，不是悄悄换旗"
        );
        assert!(stopped(&flag), "被拒绝的登记不许动旧旗标");

        hub.release("conv-live");
        assert!(hub.abort("conv-live").is_err(), "跑完的回合也没得停");
        // 收尾之后登记重新畅通：下一回合从干净状态开始
        assert!(hub.register("conv-live").is_ok());
    }

    /// 审批文案是这一条链上唯一会**跨进程活下来**的一份：策略指纹、待审批队列（在盘上）、
    /// 审批界面都读它。命令行里最常带的就是 token，所以"密钥绝不落到盘上"这条禁令能不能守住，
    /// 看的不是执行侧，而是这三处读的是不是同一串**打过码的**文本。
    /// 这里钉三件：打码只住在文案生成的那一处（两处各写一遍就会有人漏掉）、
    /// 两个持久去处读的都是 `&input`，以及执行用的仍是模型给的原始参数（打码不许改变它做什么）
    #[test]
    fn the_approval_text_is_masked_once_and_every_durable_sink_reads_that_one_copy() {
        // 打码本体住 tool_gate.rs（O1-4），调用点住 chat.rs——两份加总才是完整的生产面
        // （各自先取生产区再拼接：chat.rs 自己的 cfg(test) 标记会把 tool_gate 切掉）
        let chat_production = include_str!("chat.rs")
            .replace('\r', "")
            .split("\n#[cfg(test)]")
            .next()
            .unwrap_or_default()
            .to_string();
        let production = chat_production + include_str!("chat/tool_gate.rs");
        assert_eq!(
            production.matches("crate::secrets::mask_secrets(").count(),
            1,
            "打码只住 mask_tool_input 这一个函数——第二处调用就是第二份真相"
        );
        assert!(
            production.contains(
                "fn mask_tool_input(via_mcp: bool, name: &str, args: &Value) -> String {"
            ),
            "打码助手必须是那个唯一出处本体"
        );
        assert!(
            production.matches("mask_tool_input(").count() >= 3,
            "串行主干与并行预跑（加定义自身）都该走同一个助手"
        );
        assert!(
            production.contains("park_unattended(host, conversation_id, &ruling, &input)"),
            "挂起那一发不再读这串：队列里存的就是没打过码的原文"
        );
        assert!(
            production
                .contains("hub.stage(&call.id, &ruling.remember_key(), &short_label(&input))"),
            "现场审批的弹层不再读这串：同上"
        );
        // 执行侧拿的是模型给的原始参数，不是这一串显示文本：打码不许改变"它做什么"，
        // 只改"它被怎么记录、怎么摆到人眼前"。这一条不是"应该如此"，是照着调用点核过的
        assert!(
            production
                .contains("registry.run(tool_runtime::source::shared_cache(), &call.name, &args)"),
            "派发口不再收原始 args：那打码就可能改变行为，这条前提要重写"
        );
    }

    /// 工具那一行不能把自己是谁写死。`audit_tool` 以前恒记 `Actor::Model`，
    /// 于是编排器或定时任务引起的一次写文件，在审计账上与"用户在聊天里让模型动的"
    /// 长得一模一样——而"这一发有没有人看着"恰恰是无人值守那条路唯一要区分的事。
    /// 这一格要 `AppHandle`，没有行为测试入口，所以钉它读的是那个查得到的归属；
    /// 归属本身对不对由 `tasks::escalate::tests::an_unattended_run_is_audited_as_the_thing_that_ran_it` 判
    #[test]
    fn a_tool_row_asks_who_ran_the_turn() {
        let source = include_str!("chat/tool_gate.rs");
        let body = source
            .split("fn audit_tool_in(")
            .nth(1)
            .expect("工具审计那一行")
            .split("\nfn ")
            .next()
            .expect("到下一个函数为止");
        assert!(
            body.contains("escalate::audit_actor(conversation_id)"),
            "工具行的归属又被写死了：{body}"
        );
    }

    /// 终端那一行过不过闸，以及它留下一行什么样的账。这两件事此前都没有测试：
    /// `Deny` 那一支直接 `return`（红线被碰过几次，账上查不出来），跑失败了也只有
    /// "放行"那一行。P0 的判据写的是"任意一次调用都在审计里有一行，且拒绝原因可复述"
    #[test]
    fn the_terminal_gate_refuses_red_lines_and_leaves_a_row_either_way() {
        let policy = crate::policy::Policy::new(crate::policy::Mode::Ask);
        let doomed = json!({ "command": "rm -rf /" });
        let scope = tool_runtime::Call::new("run_command", &doomed, None, false);
        let reason = terminal_ruling(&policy, &scope, "rm -rf /")
            .expect_err("删根目录对终端同样是红线：他是审批人，不等于他正在批这一条");
        assert!(
            reason.contains("删除根目录"),
            "要说清拦下来的是哪条红线：{reason}"
        );

        let root = crate::test_support::temp_dir("terminal-audit");
        terminal_audit(&root, &scope, crate::audit::Outcome::Denied)
            .expect("拦下的那一次也要写得进行");
        let lines = crate::audit::read_day(&root, None);
        assert_eq!(lines.len(), 1, "拦一次留一行：{lines:?}");
        assert!(
            lines[0].contains("terminal:run_command"),
            "动作名要对得上：{}",
            lines[0]
        );
        assert!(
            lines[0].contains("\"actor\":\"user\""),
            "这一行是用户敲的，不是模型点的：{}",
            lines[0]
        );
        assert!(
            lines[0].contains("\"outcome\":\"denied\""),
            "结果要在行里：{}",
            lines[0]
        );

        // 正对照一：普通命令在 ask 档是放行的——这条判据不许退化成"终端什么都拦"
        let ordinary = json!({ "command": "cargo test" });
        let work = tool_runtime::Call::new("run_command", &ordinary, None, false);
        terminal_ruling(&policy, &work, "cargo test").expect("普通命令不该被红线拦下");
        // 正对照二：想做了但没做成，也要有一行说得出 failed
        terminal_audit(&root, &work, crate::audit::Outcome::Failed).expect("写进行");
        let after = crate::audit::read_day(&root, None);
        assert_eq!(after.len(), 2, "两次动作两行，追加不改写：{after:?}");
        assert!(
            after[1].contains("\"outcome\":\"failed\""),
            "失败要说得出：{}",
            after[1]
        );
        crate::test_support::remove_tree(&root);
    }

    /// 上面那条只证明两个纯函数自己成立。`terminal_exec` 要 `AppHandle`，进不去行为测试，
    /// 所以这里钉**调用点与次序**：放行那一行写在动手之前（写不进去就不执行），
    /// 失败那一行写在跑完之后。次序反过来就成了"还没跑就先报 Ok"，而那正是审计最坏的谎
    #[test]
    fn the_terminal_command_uses_the_one_gate_and_reports_what_actually_happened() {
        let body = include_str!("chat.rs")
            .split("fn terminal_exec(")
            .nth(1)
            .expect("终端那条命令")
            .split("\nfn ")
            .next()
            .expect("到下一个函数为止");
        let gate = body.find("terminal_ruling(").expect("终端没过那一道闸");
        let promised = body
            .find("terminal_audit(&audit_root, &scope, crate::audit::Outcome::Ok)?")
            .expect("放行那一行");
        let ran = body
            .find("run_with(")
            .expect("执行要共用那一条路，不许再有第二份入口");
        let failed = body
            .find("terminal_audit(&audit_root, &scope, crate::audit::Outcome::Failed)")
            .expect("失败那一行");
        assert!(
            gate < promised && promised < ran && ran < failed,
            "次序不对（闸 {gate} / 放行 {promised} / 执行 {ran} / 失败 {failed}）：账要按事情发生的顺序写"
        );
        assert!(
            body.contains("crate::audit::Outcome::Denied"),
            "被红线拦下的那一次不留行，就查不出有人碰过红线"
        );
        // 主体与动作名住在 helper 里（一处写死，三次调用共用），所以那两格去钉 helper
        let row = include_str!("chat.rs")
            .split("fn terminal_audit(")
            .nth(1)
            .expect("终端那一行的审计 helper")
            .split("\nfn ")
            .next()
            .unwrap_or_default();
        assert!(
            row.contains("Actor::User"),
            "终端那一行是人敲的，主体不许漂走：{row}"
        );
        assert!(
            row.contains("\"terminal:run_command\""),
            "动作名要稳定，否则复盘时按名字筛不到：{row}"
        );
    }

    /// 一次工具调用有**五个出口**，每个出口都得留下一行自己的账（P0 判据那句）。
    /// 这五处此前只有一处被别的测试看着：把其余四处里任意一处删掉，账就少一半而全库不红。
    /// 针脚一律 concat! —— 这条测试自己就在被搜的那份文件里
    #[test]
    fn every_exit_of_a_tool_call_leaves_its_own_audit_row() {
        let production = include_str!("chat.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap_or_default();
        // 针脚剥掉全部空白再比：rustfmt 的折行不再影响判据（这条测试自己就在被搜的
        // 那份文件里，needle 前缀仍拆两段防数到自己）
        let production: String = production.chars().filter(|c| !c.is_whitespace()).collect();
        // 后面的 needle 用 format! 拼（`concat!` 只收字面量，不收变量）
        let call = "audit_tool_in(&data_dir,conversation_id,&scope,";
        let has = |needle: &str, want: usize, why: &str| {
            assert_eq!(
                production.matches(needle).count(),
                want,
                "{why}（needle={needle}）"
            );
        };
        has(
            call,
            7,
            "工具调用的七个出口各留一行账（表拒 / 摇头 / 停在待审批 / 自动审查拒 / 放行·无标记 / 放行·补标记 / 跑失败）——M3 审批闭环把五出口扩成了七出口",
        );
        has(
            &format!("{call}crate::audit::Outcome::Denied,None,"),
            3,
            "被表拒、用户摇头与自动审查拒是三回事，但都必须有行——少一处就是那一支悄悄不落账",
        );
        has(
            &format!("{call}crate::audit::Outcome::Failed,pass_reason"),
            1,
            "\"跑失败了\"与\"根本没让跑\"在账上要分得开",
        );
        // 放行那一行是"写不进去就不执行"的那一个约定，只许有一处
        has(
            &format!("{call}crate::audit::Outcome::Ok,None,"),
            1,
            "权限表放行那一支：无理由可标，账照落",
        );
        has(
            &format!("ifletErr(error)={call}crate::audit::Outcome::Ok,pass_reason"),
            1,
            "动手前落账这件事只许有一处，且失败要拦得住",
        );
        // 停在待审批那一支带的是映射出来的 outcome（Blocked / Denied 都可能），不能写死
        has(
            &format!("{call}outcome,None)"),
            1,
            "escalation 那一支要带它自己算出来的 outcome，不是抄一个常量",
        );
        let promised = production
            .find(&format!(
                "ifletErr(error)={call}crate::audit::Outcome::Ok,pass_reason"
            ))
            .expect("放行那一行");
        let failed = production
            .find(&format!("{call}crate::audit::Outcome::Failed,pass_reason"))
            .expect("失败那一行");
        assert!(
            promised < failed,
            "先有\"我让它跑了\"，才可能有\"它跑砸了\""
        );
    }

    /// 按层压缩之前那一道"实发的行对得上吗"。它守的是**压错地方**：投影行与条目不是一一对应的
    /// （一次撤回可以把一行摘掉、一条条目也可以顶多行），按行号切出来的那几段在实发数组里
    /// 可能少一条、也可能多一条。这一格此前是命令里的一个内联条件，一次都没被测过
    #[test]
    fn a_compaction_that_does_not_line_up_with_the_sent_rows_is_refused() {
        // 正对照：对得上的那一次必须放行，否则这条测试会在"永远拒绝"时绿
        assert!(check_compaction_slice(3, 3, 3).is_ok(), "对上了就该让它压");
        let err = check_compaction_slice(2, 2, 3).expect_err("实发里少了一条，不能照这份计划压");
        assert!(
            err.contains("计划 3 行") && err.contains("找到 2 条"),
            "要把那两个数都说给他听：{err}"
        );
        assert!(
            check_compaction_slice(4, 4, 3).is_err(),
            "对不上是双向的，不只是少"
        );
        // 一段都没有：plan.rows 为 0 时 matched 也会是 0，那种"压"是给空段写一次摘要
        assert!(
            check_compaction_slice(0, 0, 0).is_err(),
            "空的这一段不该往下走"
        );
    }

    /// 分叉谱系那两根线：写侧在 `conversation_fork`、读侧在 `context_inspect`，两边都要
    /// `AppHandle`，进不去行为测试。`session/store.rs` 那条往返测试只证明"存得下读得回"，
    /// 证明不了 fork 真的写了那一格——把那一行删掉，全库不红
    #[test]
    fn the_fork_writes_its_parent_and_the_inspector_reads_it_back() {
        let source = include_str!("chat.rs");
        let fork = source
            .split("fn conversation_fork(")
            .nth(1)
            .expect("分叉那条命令")
            .split("\nfn ")
            .next()
            .unwrap_or_default();
        assert!(
            fork.contains("forked.opened.header.parent_session = Some(conversation_id.clone())"),
            "分叉出去的话题没写上自己从哪来，屏上那一格就永远是空的：{fork}"
        );
        let inspect = source
            .split("fn context_inspect(")
            .nth(1)
            .expect("Inspector 那条命令")
            .split("\nfn ")
            .next()
            .unwrap_or_default();
        assert!(
            inspect.contains("report.parent_session_id = opened.header.parent_session.clone()"),
            "写与读两条线断了一条：账上有父，屏上读不出来"
        );
    }

    /// 缓存身份只在"能力表说支持"且"这一笔属于某个话题"时出现。
    /// 三条各自否定一种错法：猜支持、一次性调用带身份、超长不截
    #[test]
    fn the_cache_key_appears_only_when_the_line_declares_it_and_the_request_has_an_identity() {
        let mut config = config_with("chat");
        config.base_url = "https://relay.example.test/v1".into();
        let thread = vec![json!({ "role": "user", "content": "一问" })];

        let silent = chat_payload(&config, &thread, &[], Some("conv_1"));
        assert!(
            silent.get("prompt_cache_key").is_none(),
            "没依据的服务商不能猜它支持"
        );

        config.prompt_cache_key = Some(true);
        let keyed = chat_payload(&config, &thread, &[], Some("conv_1"));
        assert_eq!(keyed["prompt_cache_key"], "conv_1");
        let one_off = chat_payload(&config, &thread, &[], None);
        assert!(
            one_off.get("prompt_cache_key").is_none(),
            "一次性调用不该占用话题的缓存条目"
        );

        let long_id = format!("conv_{}", "y".repeat(120));
        let clamped = chat_payload(&config, &thread, &[], Some(&long_id));
        assert_eq!(
            clamped["prompt_cache_key"]
                .as_str()
                .expect("有 key")
                .chars()
                .count(),
            crate::provider::capability::CACHE_KEY_LIMIT
        );
    }

    /// 摘要输入的拍平规则：旧摘要不能混进 transcript（否则模型把摘要当对话重摘一遍），
    /// 有了它才走增量更新分支
    #[test]
    fn a_previous_summary_is_taken_out_of_the_transcript_into_the_incremental_branch() {
        let history = vec![
            json!({ "role": "system", "content": format!("{SUMMARY_MARKER}
上一轮摘的要点") }),
            json!({ "role": "user", "content": "第一问" }),
            json!({ "role": "assistant", "content": "" }),
            json!({ "role": "tool", "content": "文件清单" }),
        ];
        let prompt = summary_prompt(&history);
        assert!(prompt.contains("用户：第一问"), "缺用户行：{prompt}");
        assert!(
            prompt.contains("工具结果：文件清单"),
            "缺工具结果行：{prompt}"
        );
        assert!(
            !prompt.contains(
                "上一轮摘的要点

用户"
            ),
            "旧摘要不该按对话行重摘"
        );
        assert!(
            prompt.contains(
                "<previous-summary>
上一轮摘的要点
</previous-summary>"
            ),
            "旧摘要该进增量分支：{prompt}"
        );
        assert!(prompt.contains(SUMMARY_UPDATE), "有旧摘要就该走增量指令");
    }

    /// 第一次压缩没有旧摘要，走基础指令
    #[test]
    fn the_first_compaction_uses_the_base_instructions() {
        let prompt = summary_prompt(&[json!({ "role": "user", "content": "只有一问" })]);
        assert!(prompt.contains(SUMMARY_BASE));
        assert!(!prompt.contains("<previous-summary>"));
        assert!(!prompt.contains(SUMMARY_UPDATE));
    }

    /// 翻转后的核心承诺：连续追加时发送视图必须始终是上一次的纯延长线，
    /// 而且常驻段只在头部出现一次。这两件事以前靠"前端别乱改历史"的自觉维持
    #[test]
    fn the_send_view_only_ever_extends_and_repeats_the_standing_head_once() {
        use crate::history::{Conversation, MessageRecord};
        use crate::session::legacy::{from_ledger, Migration};

        let dir = crate::test_support::scoped_temp_dir("send-view");
        let ledger = Conversation {
            id: "conv_send".into(),
            project_id: "proj-1".into(),
            title: String::new(),
            created_at: 1,
            updated_at: 1,
            pinned: false,
            kind: "chat".to_string(),
            messages: vec![MessageRecord {
                id: "msg_seed".into(),
                role: "user".into(),
                content: "已存档的一问".into(),
                ..Default::default()
            }],
            usage: None,
            video_nodes: Vec::new(),
            video_edges: Vec::new(),
        };
        let opened = Migration {
            opened: from_ledger(&ledger),
            path: dir.join("conv_send.jsonl"),
        };
        let mut send = Send::open(opened, standing_head(), Vec::new()).expect("打开发送视图该成功");

        let first = send.rows().to_vec();
        send.push(Message::Assistant(SettledAssistant {
            content: "答复一".into(),
            tool_calls: Vec::new(),
            stop: StopReason::Stop,
            reasoning: None,
            error: None,
            thinking_signature: None,
            reasoning_items_json: None,
        }))
        .expect("追加该成功");
        let second = send.rows().to_vec();
        send.push(Message::User {
            content: "第二问".into(),
            images: Vec::new(),
            audios: Vec::new(),
            videos: Vec::new(),
        })
        .expect("追加该成功");
        let third = send.rows().to_vec();

        assert_eq!(
            crate::session::prefix::divergence(&first, &second),
            first.len()
        );
        assert_eq!(
            crate::session::prefix::divergence(&second, &third),
            second.len()
        );
        let heads = third
            .iter()
            .filter(|row| row["content"].as_str() == Some(DEFAULT_SYSTEM_PROMPT))
            .count();
        assert_eq!(heads, 1, "常驻段被重复发出去了：{heads} 份");
        assert_eq!(third.first().expect("有头行")["role"], "system");
        assert_eq!(third.last().expect("有末行")["content"], "第二问");
    }

    /// 阶梯上"这一轮不发记忆段"那一步终于有人执行了。这条测的是链路而不是公式：
    /// 一个装得下常驻段、装不下记忆段的窗口，得让装配前那一眼真的把记忆段摘掉
    #[test]
    fn the_ladder_can_take_the_memory_section_out_before_it_is_written() {
        use crate::history::{Conversation, MessageRecord};
        use crate::session::legacy::{from_ledger, Migration};

        let dir = crate::test_support::scoped_temp_dir("yield-memory");
        let ledger = Conversation {
            id: "conv_yield".into(),
            project_id: "proj-1".into(),
            title: String::new(),
            created_at: 1,
            updated_at: 1,
            pinned: false,
            kind: "chat".to_string(),
            messages: vec![MessageRecord {
                id: "msg_seed".into(),
                role: "user".into(),
                content: "已存档的一问".into(),
                ..Default::default()
            }],
            usage: None,
            video_nodes: Vec::new(),
            video_edges: Vec::new(),
        };
        let opened = Migration {
            opened: from_ledger(&ledger),
            path: dir.join("conv_yield.jsonl"),
        };
        let memory = "记".repeat(30_000);
        let send = Send::open(
            opened,
            standing_head(),
            conversation_sections(Some("约定"), Some("<skills/>"), Some(&memory), None),
        )
        .expect("打开发送视图该成功");

        let sizing = |window: usize| crate::session::layers::BudgetInput {
            window,
            output_reserve: 1_000,
            chars_per_token: 1.0,
        };
        // 窗口从常驻段的实际长度起算：默认提示词长大时这两个数跟着走，
        // 否则固定层自己就顶爆窗口（deficit > 0 时直接不判），
        // 测的就不再是"阶梯摘不摘记忆"，而是"提示词写得有多长"
        let standing = DEFAULT_SYSTEM_PROMPT.chars().count();
        assert!(
            send.must_yield_memory(sizing(standing + 8_000), "这一问")
                .expect("预检该成功"),
            "记忆段装不下了却什么都没摘——阶梯那一步还是没人执行"
        );
        assert!(
            !send
                .must_yield_memory(sizing(standing + 200_000), "这一问")
                .expect("预检该成功"),
            "窗口松到装得下还摘，那就是白丢上下文"
        );
    }

    /// 档案里"指定模型"这一格终于会动了。`None` 一个字节都不改（今天所有内置档案走的就是这一条），
    /// 全空白的模型名也不算指定——那会发一个空模型出去，服务商回一句看不懂的错，
    /// 而用户在自己的档案里写的明明是一个名字
    #[test]
    fn a_profile_that_names_a_model_is_the_only_one_that_changes_it() {
        let mut base = AppConfig::default();
        base.model = "user-model".into();
        assert_eq!(
            with_connection(base.clone(), None, None).unwrap().model,
            "user-model",
            "没指定就是跟着设置走"
        );
        assert_eq!(
            with_connection(base.clone(), Some("   "), None)
                .unwrap()
                .model,
            "user-model"
        );
        assert_eq!(
            with_connection(base, Some(" cheap-model "), None)
                .unwrap()
                .model,
            "cheap-model"
        );
    }

    /// 出口名单与表上那一行 `net.provider` 都真的接在模型请求那一条上，而不是各自住在自己文件里
    /// （§16 + §19）。正对照给一条**两道闸都不拦**但域名解析不了的地址：那条要红在"网络"上，
    /// 否则这两道闸可能只是永远在响
    #[test]
    fn the_model_exit_asks_both_gates_before_it_connects() {
        use std::sync::atomic::AtomicBool;
        let stop = AtomicBool::new(false);
        let mut ignore = |_item: StreamItem| Ok(());
        let mut listed = AppConfig::default();
        listed.net_egress_allow = vec!["example.com".to_string()];

        let blocked = read_events(
            "https://api.other.test/v1/chat/completions",
            &listed,
            &[],
            &json!({}),
            "aglab/api-key",
            &stop,
            &mut ignore,
        );
        let error = blocked.expect_err("不在名单里的这家不该发出去");
        assert!(error.contains("出口被拦下"), "{error}");
        assert!(
            error.contains("api.other.test"),
            "那句错要说得出拦的是谁：{error}"
        );

        // 第二道闸：名单空着（= 不收紧），表上那一行划成红线
        let mut redline = AppConfig::default();
        redline.permission_overrides = vec![crate::policy::PermissionOverride {
            key: "net.provider".to_string(),
            level: crate::policy::Level::Deny,
        }];
        let held = read_events(
            "https://api.deepseek.test/v1/chat/completions",
            &redline,
            &[],
            &json!({}),
            "aglab/api-key",
            &stop,
            &mut ignore,
        );
        let reason = held.expect_err("红线划在 net.provider 上，这一发不该出去");
        assert!(
            reason.contains("net.provider"),
            "那句错要说得出是哪一行拦的：{reason}"
        );
        assert!(
            !reason.contains("出口被拦下"),
            "两道闸的理由不能混成一句：{reason}"
        );

        let past = read_events(
            "https://nope.invalid/v1/chat/completions",
            &AppConfig::default(),
            &[],
            &json!({}),
            "aglab/api-key",
            &stop,
            &mut ignore,
        );
        let network = past.expect_err("默认配置下它该真的去连，然后死在解析上");
        assert!(!network.contains("出口被拦下"), "空名单也在拦人：{network}");
        assert!(!network.contains("权限表"), "默认表也在拦人：{network}");
    }

    // 假 HTTP 代理的脚手架住在 test_support 里：代理池自己的批量测试也要用它，
    // 两处各写一份就会有一处悄悄不再真的走隧道
    use crate::test_support::fake_http::{proxy as fake_proxy, proxy_mode as fake_proxy_mode};

    /// 经代理打模型出口的那一发：目标是个不必存在的域名（真转发是代理的事），
    /// 名单只放行它，所以这一发不会从别处漏出去
    fn pooled_config(proxy_urls: &[(&str, &str)]) -> AppConfig {
        let mut config = AppConfig::default();
        config.net_egress_allow = vec!["aglab-failover.test".to_string()];
        config.proxy_default = "pool".into();
        config.proxy_pool.proxies = proxy_urls
            .iter()
            .map(|(id, url)| crate::config::ProxyEntry {
                id: (*id).to_string(),
                name: (*id).to_string(),
                url: (*url).to_string(),
                enabled: true,
                weight: 1,
            })
            .collect();
        config
    }

    fn stat_of(config: &AppConfig, id: &str) -> crate::proxy::ProxyStat {
        crate::proxy::snapshot(config)
            .into_iter()
            .find(|stat| stat.id == id)
            .expect("配置里的代理该有一格读数")
    }

    /// 换路的真凭据：池里第一条指向一个没人听的端口，第二条是真的在答话的代理。
    /// 这一发必须成功、正文要收到——旧行为是第一条把整回合打死。
    /// 死的那条进账为「连不上」，活着那条进账为「通路成立」且量到了头耗时
    #[test]
    fn a_dead_proxy_does_not_cost_the_turn_when_the_pool_has_another_route() {
        use std::sync::atomic::AtomicBool;
        let live = fake_proxy(200);
        // 两条等权：轮询的第一条确定是死的那条（这正是"用户按停止前先撞墙"的形状）
        let mut config = pooled_config(&[("fo-dead", "http://127.0.0.1:1"), ("fo-live", &live)]);
        config.model = "模型甲".into();
        let stop = AtomicBool::new(false);
        let mut seen: Vec<String> = Vec::new();
        let mut collect = |item: StreamItem| {
            if let StreamItem::Chunk(chunk) = item {
                seen.push(
                    chunk["choices"][0]["delta"]["content"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                );
            }
            Ok(())
        };
        let outcome = read_events(
            "http://aglab-failover.test/v1/chat/completions",
            &config,
            &[],
            &json!({"model": "模型甲"}),
            "aglab/api-key",
            &stop,
            &mut collect,
        );
        assert!(
            outcome.is_ok(),
            "第一条代理死了不该让整回合失败：{:?}",
            outcome.err()
        );
        assert_eq!(
            seen,
            vec!["你好".to_string()],
            "换路后正文要照常收到：{seen:?}"
        );

        let dead = stat_of(&config, "fo-dead");
        assert_eq!(
            (dead.unreachable, dead.reached, dead.failures),
            (1, 0, 1),
            "死的那条该记成连不上：{dead:?}"
        );
        assert_eq!(dead.inflight, 0, "试完要把占用放回零");
        let live_stat = stat_of(&config, "fo-live");
        assert_eq!(
            (live_stat.unreachable, live_stat.reached),
            (0, 1),
            "活着那条是通路成立：{live_stat:?}"
        );
        assert!(
            live_stat.head_ms.is_some(),
            "换路成功也要量到那条代理的头耗时：{live_stat:?}"
        );
    }

    /// 归因的正对照（那才是原来的 bug）：经代理打出去、服务商回了 429。
    /// 路是通的，代理**不该**进冷却——旧行为把状态码算成代理的失败，
    /// 连吃三次限流就把一条好代理关 30 秒
    #[test]
    fn an_endpoint_status_code_never_cools_the_proxy_that_delivered_it() {
        use std::sync::atomic::AtomicBool;
        let live = fake_proxy(429);
        let config = pooled_config(&[("rate-live", &live)]);
        let stop = AtomicBool::new(false);
        let mut ignore = |_item: StreamItem| Ok(());
        let error = read_events(
            "http://aglab-failover.test/v1/chat/completions",
            &config,
            &[],
            &json!({}),
            "aglab/api-key",
            &stop,
            &mut ignore,
        )
        .expect_err("429 要照原样报给用户");
        assert!(error.contains("429"), "那句错要说得出是限流：{error}");

        let stat = stat_of(&config, "rate-live");
        assert_eq!(
            (stat.reached, stat.unreachable, stat.failures),
            (1, 0, 0),
            "状态码不算代理的错：{stat:?}"
        );
        assert_eq!(
            stat.cooling_ms, 0,
            "一条把 429 送回来的代理不该在冷却：{stat:?}"
        );
        assert!(
            stat.head_ms.is_some(),
            "这一发照样给自适应档留一个样本：{stat:?}"
        );
    }

    /// 退避序列：第 n 次等 2ⁿ 秒、封顶 60 秒。"无限重试"不是"无间隔轰炸"——
    /// 那只会把限流踩得更死，而这一格是这条路上唯一不用真等就能测的东西
    #[test]
    fn the_429_backoff_doubles_and_stops_at_a_minute() {
        assert_eq!(retry_429_delay(1), 2_000);
        assert_eq!(retry_429_delay(2), 4_000);
        assert_eq!(retry_429_delay(5), 32_000);
        // 封顶在第 6 次（2^6 = 64 > 60）：往后不再翻倍，也不许把间隔算成负数
        assert_eq!(retry_429_delay(6), 60_000);
        assert_eq!(retry_429_delay(7), 60_000);
        assert_eq!(
            retry_429_delay(u32::MAX),
            60_000,
            "轮数再大也不许溢出成负数"
        );
    }

    /// 重试闸门是按**字面串**认领限流的（错误爬出传输层时只剩字符串了）。这条针钉住
    /// 它与用户看到的那一句同源：改了 `describe_status` 的措辞而忘了改常量，
    /// "无限重试"会静默退回"一次就报错"——而那正是人打开开关期待它别做的事
    #[test]
    fn the_retry_gate_reads_the_same_429_wording_the_user_sees() {
        let sentence = describe_status(429, "aglab/api-key", "https://api.deepseek.com");
        assert!(
            sentence.contains(RETRYABLE_STATUS_WORD),
            "重试闸门认的是这一串，用户看到的却是另一句：{sentence}"
        );
        // 别的状态码不许被写成限流：5xx 走的是池内换人，401/404 走的是原样报错，
        // 让它们也含上这一串就等于让它们在服务商门口无限干等
        for code in [400u16, 401, 403, 404, 500, 503] {
            let other = describe_status(code, "aglab/api-key", "https://api.deepseek.com");
            assert!(
                !other.contains(RETRYABLE_STATUS_WORD),
                "{code} 不该被写成限流：{other}"
            );
        }
    }

    /// 开着开关时，限流那一发**真的等到了**放行：第一发回 429、第二发回正常 SSE。
    /// 这一条必须看到正文——恒定回 429 的假代理只能证明"这一发失败了"，
    /// 证不了重试到第 k 发会成，而那正是无限重试的全部意义
    #[test]
    fn an_enabled_429_retry_waits_until_the_endpoint_lets_us_through() {
        use crate::test_support::fake_http::proxy_sequence as fake_proxy_sequence;
        use std::sync::atomic::AtomicBool;
        let proxy = fake_proxy_sequence(vec![429, 200]);
        let mut config = pooled_config(&[("rate-once", &proxy)]);
        config.unlimited_retry_429 = true;
        let stop = AtomicBool::new(false);
        let mut seen: Vec<String> = Vec::new();
        let mut notices: Vec<String> = Vec::new();
        let mut collect = |item: StreamItem| {
            match item {
                StreamItem::Chunk(chunk) => {
                    seen.push(
                        chunk["choices"][0]["delta"]["content"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                    );
                }
                StreamItem::Notice(text) => notices.push(text),
                // 探针事件是界面动画的数据源：失败重试的收集器不消费它
                StreamItem::Probe { .. } => {}
            }
            Ok(())
        };
        let outcome = read_events(
            "http://aglab-failover.test/v1/chat/completions",
            &config,
            &[],
            &json!({}),
            "aglab/api-key",
            &stop,
            &mut collect,
        );
        assert!(
            outcome.is_ok(),
            "限流之后放行，这一发该成功：{:?}",
            outcome.err()
        );
        assert_eq!(
            seen,
            vec!["你好".to_string()],
            "放行那一发的正文要照常收到：{seen:?}"
        );
        // 等待要说一句（无限等待不该是无声的），但一次重试只说一句
        assert_eq!(
            notices.len(),
            1,
            "重试进度一句话，不是每 100ms 播报一次：{notices:?}"
        );
        assert!(
            notices[0].contains("429") && notices[0].contains("重试"),
            "那句话说得出卡在哪儿与第几次：{}",
            notices[0]
        );
        // 两发都在同一条代理上成功拿到了头：限流不是这条路的错，不该进冷却
        let stat = stat_of(&config, "rate-once");
        assert_eq!(
            (stat.reached, stat.unreachable, stat.failures),
            (2, 0, 0),
            "重试的两发各记一次通路成立：{stat:?}"
        );
        assert_eq!(stat.cooling_ms, 0, "重试不该把一条好代理关进冷却：{stat:?}");
    }

    /// 关着开关时一个字节都不许变：一次 429 就照原样报错、一句重试也不说。
    /// 这是"默认档零变化"——无限等待是要用户点头的东西，没点头之前不许替他等
    #[test]
    fn a_disabled_429_retry_reports_the_first_one_and_stops() {
        use std::sync::atomic::AtomicBool;
        let proxy = fake_proxy(429);
        let config = pooled_config(&[("rate-off", &proxy)]);
        assert!(
            !config.unlimited_retry_429,
            "默认档就得是关着的：这条针钉的是默认值本身"
        );
        let stop = AtomicBool::new(false);
        let mut notices: Vec<String> = Vec::new();
        let mut collect = |item: StreamItem| {
            if let StreamItem::Notice(text) = item {
                notices.push(text);
            }
            Ok(())
        };
        let error = read_events(
            "http://aglab-failover.test/v1/chat/completions",
            &config,
            &[],
            &json!({}),
            "aglab/api-key",
            &stop,
            &mut collect,
        )
        .expect_err("默认档下 429 该原样报给用户");
        assert!(error.contains("429"), "那句错要说得出是限流：{error}");
        assert!(
            notices.is_empty(),
            "没开开关就不该有重试的进度话：{notices:?}"
        );
        assert_eq!(
            stat_of(&config, "rate-off").reached,
            1,
            "只发了一发，一次重试都没试"
        );
    }

    /// 无限等待必须**按得停**：等的时候每 100ms 看一次旗，按了就带 STOP_MARK 退出。
    /// 拦不住停止的"无限"不叫重试，叫把这一支锁死在服务商门口
    #[test]
    fn stopping_during_a_429_wait_gives_up() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let proxy = fake_proxy(429);
        let mut config = pooled_config(&[("rate-stop", &proxy)]);
        config.unlimited_retry_429 = true;
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        // 第一发 429 之后开始等（2 秒）；150ms 后按停，第二发就不该发出去
        let for_timer = std::sync::Arc::clone(&stop);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            for_timer.store(true, Ordering::Relaxed);
        });
        let mut notices: Vec<String> = Vec::new();
        let mut collect = |item: StreamItem| {
            if let StreamItem::Notice(text) = item {
                notices.push(text);
            }
            Ok(())
        };
        let started = Instant::now();
        let error = read_events(
            "http://aglab-failover.test/v1/chat/completions",
            &config,
            &[],
            &json!({}),
            "aglab/api-key",
            &stop,
            &mut collect,
        )
        .expect_err("按了停止就该退出无限等待");
        assert_eq!(
            error, STOP_MARK,
            "停止要说得出是停止，不是服务商的错：{error}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "按停之后不该把那一整段退避等完： {:?}",
            started.elapsed()
        );
        assert_eq!(notices.len(), 1, "等待期间说过一次就好：{notices:?}");
        assert_eq!(
            stat_of(&config, "rate-stop").reached,
            1,
            "第二发没发出去，通路也只记一次"
        );
    }

    /// 用户按停止不是代理的错：那一发一个字节都没吐给用户，也不该进任何归因
    #[test]
    fn stopping_a_round_leaves_no_blame_on_the_proxy() {
        use std::sync::atomic::AtomicBool;
        let live = fake_proxy(200);
        let config = pooled_config(&[("stop-live", &live)]);
        let stop = AtomicBool::new(true);
        let mut ignore = |_item: StreamItem| Ok(());
        let error = read_events(
            "http://aglab-failover.test/v1/chat/completions",
            &config,
            &[],
            &json!({}),
            "aglab/api-key",
            &stop,
            &mut ignore,
        )
        .expect_err("停止开关已经按下，这一发该带着 STOP_MARK 回来");
        assert!(
            !error.contains("连不上"),
            "停止不该被翻译成代理坏了：{error}"
        );

        let stat = stat_of(&config, "stop-live");
        assert_eq!((stat.total, stat.inflight), (1, 0), "上路一次、占用归零");
        assert_eq!(
            (
                stat.reached,
                stat.unreachable,
                stat.interrupted,
                stat.failures
            ),
            (0, 0, 0, 0),
            "没有结局的那一发不该留下任何归因：{stat:?}"
        );
    }

    /// 换路的重播禁令（这功能是这套设计里最贵的一条）：第一条代理把连接建起来、
    /// 吐了一个事件之后掐流。这一发**不能**再去试第二条——换路会把同一回合的正文
    /// 重播一遍。第二家的 total 必须是 0，用户收到的必须只有那一个事件
    #[test]
    fn a_stream_that_dies_midway_is_not_replayed_on_the_next_proxy() {
        use std::sync::atomic::AtomicBool;
        let dying = fake_proxy_mode(200, true);
        let healthy = fake_proxy(200);
        let mut config = pooled_config(&[("mid-dying", &dying), ("mid-healthy", &healthy)]);
        // 两条等权时轮询的第一条就是那只会掐流的：把它排前面，让这一发必经它
        config.proxy_pool.strategy = "least_used".into();
        let stop = AtomicBool::new(false);
        let mut seen: Vec<String> = Vec::new();
        let mut collect = |item: StreamItem| {
            if let StreamItem::Chunk(chunk) = item {
                seen.push(
                    chunk["choices"][0]["delta"]["content"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                );
            }
            Ok(())
        };
        let error = read_events(
            "http://aglab-failover.test/v1/chat/completions",
            &config,
            &[],
            &json!({}),
            "aglab/api-key",
            &stop,
            &mut collect,
        )
        .expect_err("正文没拿完就断了，这一发是失败的");
        assert!(
            error.contains("读取流中断"),
            "那句错要说得出是掐流：{error}"
        );
        assert_eq!(
            seen,
            vec!["你好".to_string()],
            "半个回合不能被重播第二遍：{seen:?}"
        );

        let stat = stat_of(&config, "mid-dying");
        assert_eq!(
            (stat.interrupted, stat.unreachable, stat.failures),
            (1, 0, 0),
            "掐流不进冷却：{stat:?}"
        );
        assert_eq!(
            stat_of(&config, "mid-healthy").total,
            0,
            "吐过字节就不该再换一条代理：那条路根本没被走过"
        );
    }

    /// §15：读窗口的四处共用同一把尺。这条钉的是"用的是哪一个系数"——把 `sizing_of`
    /// 里那一格换成写死的 1.0 时它当场红，而那正是"量出来了却没人用"的形状
    #[test]
    fn the_window_always_buys_chars_with_the_measured_ruler() {
        let mut config = AppConfig::default();
        config.context_tokens = 100_000;
        config.max_tokens = 4_096;
        let cal = crate::usage::Calibration {
            chars_per_token: 4.0,
            max_deviation_pct: 25.0,
            samples: 9,
        };

        let flat = sizing_of(&config, None);
        assert_eq!(
            flat.chars_per_token, 1.0,
            "没量过就用换算落地之前那把尺：谁都不该因为这一片落地而换行为"
        );
        let measured = sizing_of(&config, Some(&cal));
        assert_eq!(measured.chars_per_token, 3.0, "窗口那一侧要取偏差的下界");
        // 输出预留走 21–32K 的带：4,096 的配置被 21K 保底托起——
        // 压缩阈值先扣的是"回答真实需要的空间"，不是配置里那个偏小的数
        assert_eq!(
            measured.output_reserve,
            crate::session::layers::OUTPUT_RESERVE_FLOOR
        );
        assert_eq!(
            crate::session::layers::budget(&[], measured).limit,
            237_000,
            "天花板 = (窗口 - 输出预留) 折成字符，换算发生在分配之前"
        );
    }

    /// 向上收集 AGENTS.md：根目录一侧在最前、越近越靠后（后出现的覆盖前面的）；
    /// 空层与不存在的层都跳过。工作目录自己的 AGENTS.md 不归这条链管——
    /// 它由 PROJECT_RULE_FILES 那一环（AGENTS.md 优先于 CLAUDE.md）负责
    #[test]
    fn agents_md_is_collected_from_ancestors_nearest_last() {
        let base = std::env::temp_dir().join(format!("aglab-agents-{}", std::process::id()));
        let root = base.join("org").join("repo").join("app");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(base.join("org").join("AGENTS.md"), "组织约定").unwrap();
        std::fs::write(
            base.join("org").join("repo").join("AGENTS.md"),
            "仓库根约定",
        )
        .unwrap();
        std::fs::write(root.join("AGENTS.md"), "应用层约定").unwrap();

        let chain = agents_md_chain(&root, 6);
        assert_eq!(
            chain.len(),
            2,
            "空层与不存在的层都跳过：{:?}",
            chain
                .iter()
                .map(|(dir, _)| dir.display().to_string())
                .collect::<Vec<_>>()
        );
        assert!(chain[0].1.contains("组织约定"), "根目录一侧在最前");
        assert!(chain[1].1.contains("仓库根约定"), "越近的越靠后");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 两处换算各取哪一侧（§15）：多算已用量、少算空位，两侧同向才叫保守。
    /// 这条红掉的两种方式，就是这一格最坏的两个坏法——忘了乘，或者把两侧调反
    #[test]
    fn both_conversions_take_the_side_that_assumes_less_room() {
        let cal = crate::usage::Calibration {
            chars_per_token: 4.0,
            max_deviation_pct: 25.0,
            samples: 9,
        };
        assert_eq!(
            calibrated_baseline(10_000, None),
            10_000,
            "没量过时 token 就当字符用，与换算落地之前一致"
        );
        assert_eq!(
            calibrated_baseline(10_000, Some(&cal)),
            50_000,
            "服务商报的 token 乘上界折成字符：宁可多算已用"
        );
        assert_eq!(tokens_of_chars(50_000, None), 50_000);
        assert_eq!(
            tokens_of_chars(50_000, Some(&cal)),
            16_667,
            "字符折回 token 除以下界：宁可少算空位"
        );
        assert!(
            tokens_of_chars(calibrated_baseline(10_000, Some(&cal)), Some(&cal)) >= 10_000,
            "同一把尺折出去再折回来，不该比服务商真报的那一发还省"
        );
    }

    /// §14.1 的第二种理由：这一段**自己**就超过设置里那个上限。窗口给足，所以它不可能是
    /// 阶梯点到的——两种理由必须分得开，否则那句 Notice 会说出假话（"快满了"其实没满）
    #[test]
    fn a_memory_section_over_the_cap_yields_even_in_a_roomy_window() {
        use crate::history::{Conversation, MessageRecord};
        use crate::session::legacy::{from_ledger, Migration};

        let dir = crate::test_support::scoped_temp_dir("memory-cap");
        let ledger = Conversation {
            id: "conv_cap".into(),
            project_id: "proj-1".into(),
            title: String::new(),
            created_at: 1,
            updated_at: 1,
            pinned: false,
            kind: "chat".to_string(),
            messages: vec![MessageRecord {
                id: "msg_seed".into(),
                role: "user".into(),
                content: "已存档的一问".into(),
                ..Default::default()
            }],
            usage: None,
            video_nodes: Vec::new(),
            video_edges: Vec::new(),
        };
        let opened = Migration {
            opened: from_ledger(&ledger),
            path: dir.join("conv_cap.jsonl"),
        };
        let memory = "记".repeat(9_000);
        let send = Send::open(
            opened,
            standing_head(),
            conversation_sections(Some("约定"), Some("<skills/>"), Some(&memory), None),
        )
        .expect("打开发送视图该成功");
        // 数的是发出去那一行，不是正文：判据要对的是模型真正收到的字节
        let row_chars = MEMORY_MARKER.chars().count() + 1 + memory.chars().count();
        let sizing = crate::session::layers::BudgetInput {
            window: 200_000,
            output_reserve: 1_000,
            chars_per_token: 1.0,
        };

        assert!(
            !send
                .must_yield_memory(sizing, "这一问")
                .expect("预检该成功"),
            "正对照：这个窗口装得下，阶梯不该点到记忆段"
        );
        assert_eq!(
            send.memory_over_cap(row_chars),
            None,
            "刚好等于上限不该算超——那是夹与不夹的边界"
        );
        assert_eq!(
            send.memory_over_cap(row_chars - 1),
            Some(row_chars),
            "超了却不报：设置里那个数成了摆设"
        );

        // 走配置那一格，而不只是走判据函数：字段名接错地方时这条会红，上面几条不会
        let mut config = AppConfig::default();
        config.context_tokens = 200_000;
        config.max_tokens = 1_000;
        config.memory_section_max_chars = row_chars - 1;
        assert_eq!(
            memory_skip_for(&config, None, &send, "这一问").expect("判一次该成功"),
            Some(MemorySkip::OverCap { chars: row_chars }),
            "设置里写了上限却没人读它"
        );
        assert_eq!(
            memory_skip_for(&config, None, &send, "").expect("判一次该成功"),
            Some(MemorySkip::OverCap { chars: row_chars }),
            "长度闸该与这一问多长无关"
        );
        config.memory_section_max_chars = 0;
        assert_eq!(
            memory_skip_for(&config, None, &send, "这一问").expect("判一次该成功"),
            None,
            "默认（0 = 不设上限）必须复现落地前的行为：谁的记忆段都不会突然不发"
        );
        config.memory_section_max_chars = row_chars - 1;
        config.context_tokens = 8_000;
        assert_eq!(
            memory_skip_for(&config, None, &send, "这一问").expect("判一次该成功"),
            Some(MemorySkip::OverCap { chars: row_chars }),
            "两种理由同时成立时要先报那个更具体的：这段本来就超了，不是窗口凑巧紧"
        );
    }

    /// §14 的"夹"：一条工具结果留多少由**单个** `max` 决定，头 3/4 尾 1/8 是派生的。
    /// 默认值 16 000 必须复现改造前那三个常量，逐字符相同——否则这一片落地那天，
    /// 每个人都会发现自己的工具结果突然变了形状
    #[test]
    fn the_tool_result_clamp_is_derived_from_one_max() {
        let text: String = (0..20_000)
            .map(|i| (b'0' + (i % 10) as u8) as char)
            .collect();
        let head_of = |n: usize| -> String { text.chars().take(n).collect() };
        let tail_of =
            |n: usize| -> String { text.chars().skip(text.chars().count() - n).collect() };

        assert_eq!(
            clamp_tool_result(&text, 16_000),
            format!(
                "{}\n\n……（中间省略约 6000 字符，原文过长已截断）……\n\n{}",
                head_of(12_000),
                tail_of(2_000)
            ),
            "默认上限改不动的行为才是这一片的底线"
        );
        assert_eq!(
            clamp_tool_result(&text, 8_000),
            format!(
                "{}\n\n……（中间省略约 13000 字符，原文过长已截断）……\n\n{}",
                head_of(6_000),
                tail_of(1_000)
            ),
            "调小 max 时头尾要按同一比例缩，不是只砍头"
        );
        assert_eq!(
            clamp_tool_result(&text, 1),
            "\n\n……（中间省略约 20000 字符，原文过长已截断）……\n\n",
            "夹到 1 个字符是荒唐配置，但它该把省略说清楚而不是下溢 panic"
        );
        assert_eq!(clamp_tool_result(&text, 0), text, "0 = 不设上限");
        assert_eq!(
            clamp_tool_result(&text, 20_000),
            text,
            "刚好等于上限的原文不该被动一个字符"
        );
    }

    /// §14 的另一半"夹"：项目约定文件超过设置里那个数就只留前面那段，并且**说得出被夹了**。
    /// 默认 8 000 复现改造前那个常量；0 = 不设上限
    #[test]
    fn the_project_rules_are_clipped_by_their_config_number() {
        let dir = crate::test_support::scoped_temp_dir("project-rules");
        std::fs::create_dir_all(&dir.path).expect("建项目目录");
        std::fs::write(
            dir.path.join("AGENTS.md"),
            format!("{}\n尾标", "a".repeat(5_000)),
        )
        .expect("写约定文件");

        let mut config = AppConfig::default();
        config.projects = vec![crate::config::Project {
            id: "p1".into(),
            name: "p1".into(),
            path: dir.path.display().to_string(),
            ..Default::default()
        }];
        config.active_project_id = "p1".into();

        config.project_rules_max_chars = 1_000;
        let clipped =
            project_card_text(&config, config.active_project(), None).expect("该有项目卡");
        assert!(
            clipped.contains(&"a".repeat(1_000)),
            "夹到 1 000，那前 1 000 个字符必须还在"
        );
        assert!(
            !clipped.contains(&"a".repeat(1_001)),
            "夹到 1 000 却留了更多——设置里那个数没被执行"
        );
        assert!(clipped.contains("已截断"), "夹了要说出来，别静默少发");
        assert!(!clipped.contains("尾标"), "被夹掉的那一截不该还在");

        config.project_rules_max_chars = 0;
        let whole = project_card_text(&config, config.active_project(), None).expect("该有项目卡");
        assert!(
            whole.contains(&"a".repeat(5_000)) && whole.contains("尾标"),
            "0 是「不设上限」：整份文件都该原样进来"
        );
    }

    /// 重新生成的形状：回溯到那句问题上、空输入不再追加第二个同样的问题，
    /// 保留下来的条目一条都不重排（Stage 3→5 那个"会重复发一次问题"的退化就在这条测试里关掉）
    #[test]
    fn regenerating_rewinds_the_tip_without_appending_a_second_question() {
        use crate::history::{Conversation, MessageRecord};
        use crate::session::legacy::{from_ledger, Migration};

        let dir = crate::test_support::scoped_temp_dir("regenerate");
        let ledger = Conversation {
            id: "conv_regen".into(),
            project_id: "proj-1".into(),
            title: String::new(),
            created_at: 1,
            updated_at: 1,
            pinned: false,
            kind: "chat".to_string(),
            messages: vec![MessageRecord {
                id: "msg_q1".into(),
                role: "user".into(),
                content: "第一个问题".into(),
                ..Default::default()
            }],
            usage: None,
            video_nodes: Vec::new(),
            video_edges: Vec::new(),
        };
        let opened = Migration {
            opened: from_ledger(&ledger),
            path: dir.join("conv_regen.jsonl"),
        };
        let mut send = Send::open(opened, standing_head(), Vec::new()).expect("打开发送视图该成功");

        let question = send
            .push(Message::User {
                content: "第二个问题".into(),
                images: Vec::new(),
                audios: Vec::new(),
                videos: Vec::new(),
            })
            .expect("追加该成功");
        let before = send.take_pushed();
        assert_eq!(before.len(), 1, "Done 要把刚登记的条目 id 报给界面");
        send.push(Message::Assistant(SettledAssistant {
            content: "第一段回答".into(),
            tool_calls: Vec::new(),
            stop: StopReason::Stop,
            reasoning: None,
            error: None,
            thinking_signature: None,
            reasoning_items_json: None,
        }))
        .expect("追加该成功");
        let rows_before_regen = send.rows().to_vec();
        let entries_before = send.opened.log.len();

        // 重新生成：末端移回那句问题，输入为空 → 不追加
        send.rewind(Some(&question)).expect("回溯该成功");
        // "不回溯"与"退到根"必须能区分：前者保留全部历史，后者一条都不留
        send.rewind(None).expect("退到根该成功");
        assert!(
            send.rows().iter().eq(standing_head().iter()),
            "退到根之后只剩常驻段，历史一条都不该在"
        );
        send.rewind(Some(&question)).expect("还能回到那句问题之后");
        assert!(
            !should_append_input("", &[]),
            "空输入不该被当成第二个同样的问题"
        );
        assert!(should_append_input("再问一次", &[]));
        assert!(should_append_input("", &["a.txt".into()]));
        let rows_after = send.rows().to_vec();

        assert_eq!(
            rows_after.len(),
            rows_before_regen.len() - 1,
            "只少了被放弃的那段回答"
        );
        assert_eq!(
            crate::session::prefix::divergence(&rows_after, &rows_before_regen),
            rows_after.len(),
            "保留下来的部分必须逐字节不动"
        );
        assert_eq!(
            send.opened.log.len(),
            entries_before,
            "回溯不删条目：被放弃的那条仍然在日志里"
        );
        let answer_id = send.take_pushed();
        assert_eq!(answer_id.len(), 1, "只报刚登记的那条");
        assert!(
            !answer_id.contains(&question),
            "上一批取走的 id 不会再报一遍"
        );
        assert_eq!(
            send.take_pushed(),
            Vec::<String>::new(),
            "取走之后不该有残留"
        );
    }

    #[test]
    fn a_stopped_stream_hands_back_what_the_user_already_read() {
        let mut state = ChatState::default();
        let mut emitted = Vec::new();
        apply_chat_event(
            &mut state,
            &json!({"choices":[{"delta":{"reasoning_content":"先想一想"}}]}),
            &mut |event| emitted.push(event),
        );
        apply_chat_event(
            &mut state,
            &json!({"choices":[{"delta":{"content":"写到一半"}}]}),
            &mut |event| emitted.push(event),
        );

        let partial = partial_of(&state);
        assert_eq!(partial.text, "写到一半");
        assert_eq!(
            partial.reasoning.as_deref(),
            Some("先想一想"),
            "思维链也是用户读过的"
        );

        let row = settle_failed(&partial, StopReason::Aborted, None).expect("有正文就该落一条");
        let Message::Assistant(settled) = row else {
            panic!("落定行必须是 assistant")
        };
        assert_eq!(settled.stop, StopReason::Aborted, "不能装作是正常收尾");
        assert_eq!(settled.content, "写到一半");
        assert_eq!(settled.reasoning.as_deref(), Some("先想一想"));
        assert!(!emitted.is_empty(), "过程事件照样要发给界面");
    }

    /// 半截工具调用**不许**落成一条带调用的行：那会永远欠一个工具结果（F9/I7）
    #[test]
    fn a_half_streamed_tool_call_is_never_recorded_as_a_call() {
        let partial = RoundOutcome {
            text: "我先看看文件".into(),
            reasoning: None,
            reasoning_signature: None,
            reasoning_items_json: None,
            tool_calls: vec![ToolCallBuffer {
                id: "call_1".into(),
                name: "terminal_exec".into(),
                arguments: "{\"comma".into(),
                content_chars: 0,
            }],
            usage: None,
            sent_chars: 0,
            truncated: true,
            sent_model: String::new(),
            response_model: None,
            response_model_path: None,
        };
        let row =
            settle_failed(&partial, StopReason::Error, Some("连接中断")).expect("有正文就该落一条");
        let Message::Assistant(settled) = row else {
            panic!("落定行必须是 assistant")
        };
        assert!(
            settled.tool_calls.is_empty(),
            "断在 JSON 中间的调用不能算发出去了"
        );
        assert_eq!(settled.stop, StopReason::Error);
        assert_eq!(settled.error.as_deref(), Some("连接中断"));
    }

    /// 一个字都没流出来时不落条目——否则历史里会出现一条空 assistant
    #[test]
    fn an_empty_failure_leaves_no_row() {
        let partial = RoundOutcome {
            text: String::new(),
            reasoning: None,
            reasoning_signature: None,
            reasoning_items_json: None,
            tool_calls: Vec::new(),
            usage: None,
            sent_chars: 0,
            truncated: false,
            sent_model: String::new(),
            response_model: None,
            response_model_path: None,
        };
        assert!(settle_failed(&partial, StopReason::Error, Some("429 限流")).is_none());
    }

    /// 断在半路的回合也要把它实发的大小带走。当场填 0，台账读作"没量到"，
    /// 校准样本就是这么静默少掉一截的
    #[test]
    fn a_half_way_round_still_carries_the_size_it_sent() {
        let mut state = ChatState::default();
        state.text = "写到一半".into();
        state.sent_chars = 12_345;
        assert_eq!(partial_of(&state).sent_chars, 12_345);
    }

    fn wire_row(text: &str) -> Value {
        json!({ "content": text })
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|id| (*id).to_string()).collect()
    }

    /// 一条带段快照的压缩条目占两行，回计正好停在它们之间。起点不退到条目边界上，
    /// 报出去的那个 id 就会说"e3 整体保留"而它自己已被切掉头一行——账与实发分家
    #[test]
    fn the_kept_window_starts_at_an_entry_edge() {
        let history = vec![
            wire_row("最早的对话内容"),
            wire_row("次早的对话内容"),
            wire_row("段快照"),
            wire_row("摘要行"),
        ];
        let origin = ids(&["e1", "e2", "e3", "e3"]);
        let boundary = compaction_boundary(&history, &origin, 3).expect("这形状够压一次");
        assert_eq!(
            boundary,
            ("e3".to_string(), 2),
            "要报的是 2 行：那两行出自同一条条目，分不开"
        );
    }

    /// 对照表与实发的数组一旦不等长，"第几行"就没有答案了。宁可不压，也不猜一个边界
    #[test]
    fn a_misaligned_provenance_table_picks_no_boundary() {
        let history: Vec<Value> = (0..6).map(|i| wire_row(&format!("第{i}段内容"))).collect();
        let aligned = ids(&["e1", "e2", "e3", "e4", "e5", "e6"]);
        assert_eq!(
            compaction_boundary(&history, &aligned, 12),
            Some(("e5".to_string(), 2)),
            "先确认这个形状本身压得出边界——否则下面那句 None 什么都没说明"
        );
        let extra = ids(&["e1", "e2", "e3", "e4", "e5", "e6", "e7"]);
        assert_eq!(
            compaction_boundary(&history, &extra, 12),
            None,
            "多出来的那一格会把所有边界整体挪一条，这时候没有正确答案"
        );
        // 少一格是同一个错的另一面：不该因为"看着还能取到值"就照常给边界
        assert_eq!(
            compaction_boundary(&history, &ids(&["e1", "e2", "e3"]), 12),
            None
        );
    }

    /// 退到根重写第一轮（编辑最前面那句话）时段不会丢：新分支上要重新落下当轮生效的
    /// 段行，而不是发出一条没有约定的线程。旧形状下段在常驻段里所以天然存在，
    /// 挪进日志之后这条就得有人钉住
    #[test]
    fn rewinding_to_root_replays_the_sections_on_the_new_branch() {
        use crate::history::Conversation;
        use crate::session::legacy::{from_ledger, Migration};

        let dir = crate::test_support::scoped_temp_dir("rewind-sections");
        let opened = Migration {
            opened: from_ledger(&Conversation {
                id: "conv_rewind".into(),
                ..Default::default()
            }),
            path: dir.join("conv_rewind.jsonl"),
        };
        let mut send = Send::open(
            opened,
            standing_head(),
            conversation_sections(Some("旧约定"), Some("<skills/>"), None, None),
        )
        .expect("打开发送视图该成功");
        send.sync_sections().expect("首轮该写入两段");
        send.push(Message::User {
            content: "一句".into(),
            images: Vec::new(),
            audios: Vec::new(),
            videos: Vec::new(),
        })
        .expect("追加该成功");
        send.push(Message::Assistant(SettledAssistant {
            content: "答一句".into(),
            tool_calls: Vec::new(),
            stop: StopReason::Stop,
            reasoning: None,
            error: None,
            thinking_signature: None,
            reasoning_items_json: None,
        }))
        .expect("追加该成功");

        send.rewind(None).expect("退到根该成功");
        send.sync_sections().expect("新分支上该重新写入段");
        send.push(Message::User {
            content: "改过的一句".into(),
            images: Vec::new(),
            audios: Vec::new(),
            videos: Vec::new(),
        })
        .expect("追加该成功");
        let rows = send.rows();
        assert_eq!(rows.len(), 4, "常驻段 + 两段 + 新问题：{rows:?}");
        assert!(rows[1]["content"].as_str().unwrap().contains("旧约定"));
        assert!(rows[2]["content"]
            .as_str()
            .unwrap()
            .starts_with(SKILLS_MARKER));
        assert_eq!(rows[3]["content"], "改过的一句");
    }

    /// §12 Stage 8 的第二条准入门：MCP 掉线时声明数组不变短，中途新连上的也不许插进来。
    /// 为什么必须这样：工具渲染在对话**之前**，动一条声明就等于动整段历史的前缀
    #[test]
    fn the_declaration_array_is_frozen_once_the_session_starts() {
        use crate::history::Conversation;
        use crate::session::legacy::{from_ledger, Migration};

        let dir = crate::test_support::scoped_temp_dir("declarations");
        let opened = Migration {
            opened: from_ledger(&Conversation {
                id: "conv_decl".into(),
                ..Default::default()
            }),
            path: dir.join("conv_decl.jsonl"),
        };
        let mut send = Send::open(opened, standing_head(), Vec::new()).expect("打开发送视图该成功");
        let frozen = vec![
            json!({ "function": { "name": "read_file" } }),
            json!({ "function": { "name": "mcp__a__time" } }),
        ];
        assert_eq!(
            send.declarations(frozen.clone()).expect("首轮该定形"),
            frozen
        );
        assert_eq!(
            send.rows().len(),
            1,
            "定形条目记事实、不进上下文，发出去的该只剩常驻段那一条：{:?}",
            send.rows()
        );

        // 第二轮那台服务器掉了：数组必须还是那两条，顺序也不动
        assert_eq!(
            send.declarations(vec![frozen[0].clone()])
                .expect("第二轮该沿用"),
            frozen,
            "掉线不许把声明数组改短"
        );
        // 中途新连上的也不许挤进来——它只能等下一个话题
        let mut grown = frozen.clone();
        grown.push(json!({ "function": { "name": "mcp__b__time" } }));
        assert_eq!(
            send.declarations(grown).expect("第三轮该沿用"),
            frozen,
            "新增不许插进已经定形的数组"
        );

        // 定形条目只写一次，而且不报给界面（屏幕上没有对应的那一行）
        assert!(
            send.take_pushed().is_empty(),
            "定形条目不该出现在本轮条目 id 里"
        );
        let written = send
            .opened
            .log
            .path()
            .expect("走路径该成功")
            .iter()
            .filter(|entry| {
                matches!(entry.payload(), EntryPayload::Custom { custom_type, .. }
                    if custom_type == DECLARATIONS_TYPE)
            })
            .count();
        assert_eq!(written, 1, "定形只发生一次");
    }

    /// 台账存的那句必须就是模型见过的那句：下一轮按 output 回放历史，
    /// 两句不同就会让前缀从那条 tool 消息处断掉，还让模型看到一句它没说过的话
    #[test]
    fn the_ui_event_and_the_model_message_carry_the_same_tool_text() {
        let call = ToolCallBuffer {
            id: "call_1".into(),
            name: "write_file".into(),
            arguments: "{\"path\":\"a.rs\"}".into(),
            content_chars: 0,
        };
        for (status, text) in [
            (ToolStatus::Denied, "用户拒绝执行该操作。"),
            (ToolStatus::Failed, "执行失败：网络断了"),
            (ToolStatus::Done, "已写入 a.rs"),
        ] {
            let (event, message) = tool_result_pair(
                &call,
                status,
                tools::Risk::High.as_str(),
                "写入 a.rs".into(),
                text.to_string(),
                None,
            );
            let ChatEvent::Tool { output, .. } = event else {
                panic!("工具结果该是 Tool 事件");
            };
            let Message::Tool {
                tool_call_id,
                content,
            } = message
            else {
                panic!("工具结果必须是一条 tool 行");
            };
            assert_eq!(
                output.as_deref(),
                Some(content.as_str()),
                "{status:?} 这条路径两边文本漂移了"
            );
            assert_eq!(tool_call_id, "call_1");
            // 参数也得带上：话题恢复重放时靠它还原成服务商认的嵌套形
            assert_eq!(output.as_deref(), Some(text));
        }
    }

    /// 放行标记跨 IPC 那道边界时只能有一个名字。这里的键是**序列化出来的字符串**：
    /// Rust 改了字段名而 TS 没跟上，编译器与 tsc 都不响，要等到卡片上那一格悄悄
    /// 变成 undefined。所以拿前端原文当断言对象，逐个键比
    #[test]
    fn the_pass_marker_crosses_the_boundary_under_one_name() {
        let event = ChatEvent::Tool {
            id: "call_1".into(),
            name: "run_command".into(),
            status: ToolStatus::Running,
            risk: "high".into(),
            input: "git push origin main".into(),
            output: None,
            arguments: None,
            pass_reason: Some(SESSION_RULE_PASS.into()),
            content_chars: None,
        };
        let row = serde_json::to_value(&event).expect("工具事件总是编得出");
        let keys: Vec<&str> = row
            .as_object()
            .expect("工具事件是一个对象")
            .keys()
            .map(String::as_str)
            .collect();
        let ts = include_str!("../../src/types/chat.ts").replace('\r', "");
        for key in &keys {
            assert!(
                ts.contains(&format!("{key}:")) || ts.contains(&format!("{key}?")),
                "后端发出 {key}，前端那份 tool 变体却不认它（全量键：{keys:?}）"
            );
        }
        assert_eq!(
            row["passReason"].as_str(),
            Some(SESSION_RULE_PASS),
            "键名不是 camelCase 的话，卡片上那一格永远是空的：{row}"
        );

        // 卡片只念后端给的那句：文案在界面上抄一份，就成了同一件事的第二个出处
        let card = include_str!("../../src/components/tool-card.tsx").replace('\r', "");
        assert!(
            card.contains("call.passReason"),
            "卡片要直接显示后端送来的那句凭据：{card}"
        );
        assert!(
            !card.contains(SESSION_RULE_PASS) && !card.contains(STANDING_GRANT_PASS),
            "界面上不许再存一份这两个文案"
        );

        // 中间那一站也得钉：store 把事件摊平成 ToolCall 行，漏掉这个字段的话
        // 后端说了、卡片也在等，可那一格永远是 undefined 而全库不红
        let store = include_str!("../../src/store/chat-store.ts").replace('\r', "");
        let carried = keys
            .iter()
            .filter(|key| store.contains(&format!("{key}: event.{key}")))
            .count();
        assert!(
            store.contains("passReason: event.passReason"),
            "工具行摊平时没把凭据带过去（事件里 {}/{} 个键是原样转手的）",
            carried,
            keys.len()
        );
    }

    /// 标记只上事件，不上台账：模型那一份的字节一旦多出一个字，下一轮回放时前缀
    /// 就从那条 tool 行断掉，整轮重付。这条同时钉住"没有标记"那一格照常发得出去
    #[test]
    fn the_pass_marker_is_the_cards_business_not_the_models() {
        let call = ToolCallBuffer {
            id: "call_1".into(),
            name: "run_command".into(),
            arguments: "{}".into(),
            content_chars: 0,
        };
        for reason in [
            None,
            Some(SESSION_RULE_PASS.to_string()),
            Some(STANDING_GRANT_PASS.to_string()),
            Some(AUTO_REVIEW_PASS.to_string()),
        ] {
            let (event, message) = tool_result_pair(
                &call,
                ToolStatus::Done,
                "high",
                "git push origin main".into(),
                "推送完成".into(),
                reason.clone(),
            );
            let ChatEvent::Tool {
                output,
                pass_reason,
                ..
            } = event
            else {
                panic!("工具结果该是 Tool 事件");
            };
            assert_eq!(
                pass_reason, reason,
                "卡片上那句要原样到达，不许在半路被换成另一种说法"
            );
            let Message::Tool { content, .. } = message else {
                panic!("工具结果必须是一条 tool 行");
            };
            assert_eq!(
                content, "推送完成",
                "模型那一份不许带上界面字段：一个字的漂移就是一次缓存断开"
            );
            assert_eq!(output.as_deref(), Some(content.as_str()));
        }
    }

    /// 这一格要 `AppHandle`，没有行为测试入口，所以钉它读的是那两段的写法。
    /// 钉的是"只有一个出处"：判定问一次闸，文案各读同一个常量
    #[test]
    fn a_pass_that_skipped_the_question_is_said_once_and_written_once() {
        let raw = include_str!("chat.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap_or_default();
        // 针脚剥掉全部空白再比：rustfmt 的折行不再影响判据
        let production: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
        // 闸只问一次：`remembered` 查出来的那一格，就是标记派生出来的那一格
        let needle = concat!("hub.is_remembered(&ruling.remember_ke", "y())");
        assert_eq!(
            production.matches(needle).count(),
            1,
            "第二个地方再查一次同一个键，早晚会给出两个答案"
        );
        assert!(
            production.contains("letneeds_approval=(matches!(ruling.decision,crate::policy::Decision::Ask{..})&&!remembered)||hook_ask_reason.is_some();"),
            "跳过询问的判据与标记的判据必须是同一格，钩子的 ask 也要把这一次拉回审批"
        );
        // 无需询问那一档不给标记：那不是在"跳过闸门"（mut 是给自动审查通过那格补标用的）
        assert!(
            production.contains(
                "letmutpass_reason=if!matches!(ruling.decision,crate::policy::Decision::Ask{..}){"
            ),
            "权限表本来就放行的那一档不该被标成\"该问而没问\""
        );
        // 文案各只有一处定义 + 一处使用；那两句字面量只许住在常量里
        assert_eq!(
            production.matches(SESSION_RULE_PASS).count(),
            1,
            "这句字面量只许出现在它自己的常量那一行"
        );
        for name in [
            "SESSION_RULE_PASS",
            "STANDING_GRANT_PASS",
            "AUTO_REVIEW_PASS",
        ] {
            assert_eq!(
                production.matches(name).count(),
                2,
                "{name} 该有定义那一处与闸门用那一处"
            );
        }
        // 账上那一行：标记写进同一行 JSON 的 detail，而不是另起一行
        // （audit_tool_in 的定义已搬进 tool_gate.rs——本体在哪个文件就从哪份读）
        let body: String = include_str!("chat/tool_gate.rs")
            .split("fn audit_tool_in(")
            .nth(1)
            .expect("工具审计那一行")
            .split("\nfn ")
            .next()
            .expect("到下一个函数为止")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        assert!(
            body.contains("crate::audit::record_detail("),
            "标记走的是同一行的 detail，另起一行会把一次执行记成两次：{body}"
        );
        assert!(
            body.contains("pass_reason.map(|reason|"),
            "放行那一行要把凭据写进去：{body}"
        );
    }

    /// 命令级：撤销一次压缩，被那句摘要顶掉的那几轮要**原样回来**。
    /// 这一格以前只有 `context.rs` 里手搭的 `ContextEdit` 行测着语义，而命令那一头
    /// 写的到底是 `None` 还是 `Some("")` 没人钉——那两者在投影里是两件事：
    /// `None` 让条目回来，`Some("")` 是把它们换成一段空正文（等于把历史擦了）
    #[test]
    fn undoing_a_compaction_brings_the_replaced_rows_back_byte_for_byte() {
        use crate::history::Conversation;
        use crate::session::entry::EntryPayload;
        use crate::session::legacy::{from_ledger, Migration};

        let dir = crate::test_support::scoped_temp_dir("undo-compaction");
        let opened = Migration {
            opened: from_ledger(&Conversation {
                id: "conv_undo".into(),
                ..Default::default()
            }),
            path: dir.join("conv_undo.jsonl"),
        };
        let mut send = Send::open(opened, standing_head(), Vec::new()).expect("打开发送视图该成功");
        let asked = send
            .push(Message::User {
                content: "第一问".into(),
                images: Vec::new(),
                audios: Vec::new(),
                videos: Vec::new(),
            })
            .expect("追加用户行该成功");
        let answered = send
            .push(Message::Assistant(SettledAssistant {
                content: "第一答".into(),
                tool_calls: Vec::new(),
                stop: StopReason::Stop,
                reasoning: None,
                error: None,
                thinking_signature: None,
                reasoning_items_json: None,
            }))
            .expect("追加助手行该成功");
        let before = crate::session::context::project(&send.opened.log)
            .expect("投影该成功")
            .wire();
        assert_eq!(before.len(), 2, "压之前那两轮都该在：{before:?}");

        let boundary = send
            .append(EntryPayload::BranchSummary {
                from_id: Some(asked),
                through_id: Some(answered),
                summary: "（前两轮压成这一句）".into(),
                usage: None,
            })
            .expect("边界行该写得进");
        let during = crate::session::context::project(&send.opened.log)
            .expect("投影该成功")
            .wire();
        assert_ne!(
            during, before,
            "压过之后投影还逐字节和压之前一样，等于那次压缩没生效：{during:?}"
        );

        revocation(&mut send, &boundary).expect("撤回那一行该写得进");
        let after = crate::session::context::project(&send.opened.log)
            .expect("投影该成功")
            .wire();
        assert_eq!(after, before, "撤销一次压缩之后，投影要与压之前逐字节相同");

        // 反面对照：撤回认的是那一行边界，指到不存在的那格要被写门拒掉，
        // 而不是悄悄写下一行永远不起作用的撤回
        assert!(
            revocation(&mut send, "不存在的条目").is_err(),
            "指不到目标就该报错，不是静默收下"
        );

        // 链路那一半也得钉：上面测的是助手，命令要是自己另写一行（不再叫它），
        // 助手照样绿而用户点的那颗按钮已经换了写法
        let command = include_str!("chat.rs")
            .replace('\r', "")
            .split("fn context_undo_compaction(")
            .nth(1)
            .expect("撤销那一条命令")
            .split("\nfn ")
            .next()
            .unwrap_or_default()
            .to_string();
        assert!(
            command.contains("revocation(&mut send, &entry_id)"),
            "命令不再走那一个助手，上面那条测试就只是在测一个没人在用的函数：{command}"
        );
    }

    /// 分叉带过去的那本账里，每一格都是一个"抄还是清"的决定。这些决定以前只有
    /// 起着应用才看得见：抄了正文，同一句对话就从两处各说各话；抄了时间戳，
    /// 新生的一支出生的时间就是它父话题的；空标题抄过去，话题列表上就多一行空白
    #[test]
    fn a_fork_copies_the_ledger_head_but_never_the_transcript() {
        use crate::history::{Conversation, MessageRecord, UsageRecord};

        let source = Conversation {
            id: "conv_src".into(),
            project_id: "proj_a".into(),
            title: "  原来的名字  ".into(),
            created_at: 10,
            updated_at: 20,
            pinned: false,
            kind: "chat".to_string(),
            messages: vec![MessageRecord {
                role: "user".into(),
                content: "一整段历史".into(),
                ..Default::default()
            }],
            usage: Some(UsageRecord {
                input_tokens: 7,
                output_tokens: 3,
                duration_ms: 11,
            }),
            video_nodes: Vec::new(),
            video_edges: Vec::new(),
        };
        let forked = fork_ledger(&source, "conv_new".into(), 999);
        assert_eq!(forked.id, "conv_new", "新话题用自己的 id");
        assert_eq!(
            forked.project_id, "proj_a",
            "分叉还在同一个项目里：根目录与权限都跟着项目走"
        );
        assert_eq!(
            forked.title, "原来的名字（分叉）",
            "标题要 trim 再缀一句，让人认得出它是分叉"
        );
        assert!(
            forked.messages.is_empty(),
            "正文住在话题日志里，抄进账本就是同一件事住两处：{} 条",
            forked.messages.len()
        );
        assert!(forked.usage.is_none(), "上一本的用量不该顶成这一本的");
        assert_eq!(
            (forked.created_at, forked.updated_at),
            (999, 999),
            "分叉是新生的一支，时间戳是现在，不是抄源话题的出生时间"
        );

        // 空标题不许抄出一个空话题名
        let blank = Conversation {
            title: "   ".into(),
            ..Default::default()
        };
        assert_eq!(
            fork_ledger(&blank, "conv_b".into(), 1).title,
            "未命名话题（分叉）"
        );

        // 链路那一半：命令要是自己再把那几格抄一遍，上面这些就只是在一个没人叫的函数上打转
        let command = include_str!("chat.rs")
            .replace('\r', "")
            .split("fn conversation_fork(")
            .nth(1)
            .expect("分叉那条命令")
            .split("\nfn ")
            .next()
            .unwrap_or_default()
            .to_string();
        assert!(
            command.contains("fork_ledger(&source_ledger, new_id.clone(), now)"),
            "分叉命令不再叫这一个助手，账本那几格就换了地方写：{command}"
        );
    }

    /// 命令级：压完之后，被顶替的那几轮要从投影里消失、摘要要顶上、最新那一行必须留下。
    /// 这一格以前测不到，是因为整条命令里唯一拿不到的那一次摘要请求要花钱 —— 而
    /// **"压没压生效"与"摘要写得好不好"是两件事**，不该被同一道门槛连着挡掉
    #[test]
    fn writing_a_layer_summary_replaces_the_oldest_rows_and_keeps_the_newest() {
        use crate::history::Conversation;
        use crate::session::legacy::{from_ledger, Migration};

        let dir = crate::test_support::scoped_temp_dir("layer-summary");
        let mut send = Send::open(
            Migration {
                opened: from_ledger(&Conversation {
                    id: "conv_layer".into(),
                    ..Default::default()
                }),
                path: dir.join("conv_layer.jsonl"),
            },
            standing_head(),
            Vec::new(),
        )
        .expect("打开发送视图该成功");
        let texts = [
            "最老的一问，长到足够占预算",
            "最老的一答，也占",
            "中间一问",
            "最新一问",
        ];
        for (index, text) in texts.iter().enumerate() {
            let message = if index % 2 == 0 {
                Message::User {
                    content: (*text).into(),
                    images: Vec::new(),
                    audios: Vec::new(),
                    videos: Vec::new(),
                }
            } else {
                Message::Assistant(SettledAssistant {
                    content: (*text).into(),
                    tool_calls: Vec::new(),
                    stop: StopReason::Stop,
                    reasoning: None,
                    error: None,
                    thinking_signature: None,
                    reasoning_items_json: None,
                })
            };
            send.push(message).expect("追加该成功");
        }

        let rows = crate::session::layers::history_rows(&send.opened.log).expect("历史层读得出来");
        let total: usize = rows.iter().map(|row| row.chars).sum();
        let plan = crate::session::layers::layer_compaction(&rows, total / 2)
            .expect("越界了就该给出一个计划");
        let before = crate::session::context::project(&send.opened.log)
            .expect("投影该成功")
            .wire();
        assert_eq!(before.len(), texts.len(), "压之前每一轮都该在：{before:?}");

        write_layer_summary(&mut send, &plan, "（最老那几轮换成的摘要）").expect("写这一行该成功");
        let after = crate::session::context::project(&send.opened.log)
            .expect("投影该成功")
            .wire();
        let rendered = serde_json::to_string(&after).expect("投影那份数组总编得出");
        assert!(
            rendered.contains("（最老那几轮换成的摘要）"),
            "摘要没顶上，那一行等于白写：{after:?}"
        );
        assert!(
            after.len() < before.len(),
            "被顶替的行还全在投影里，这次压缩就没生效：{} → {}",
            before.len(),
            after.len()
        );
        assert!(
            rendered.contains(texts[texts.len() - 1]),
            "最新那一行是模型接着往下说的根据，裁不得：{after:?}"
        );
        assert!(
            !rendered.contains(texts[0]),
            "计划说要顶替最老那一行，它却还在投影里：{after:?}"
        );

        // 链路那一半：命令要是自己再把那一行写一遍，上面这些就只是在一个没人叫的函数上打转
        let command = include_str!("chat.rs")
            .replace('\r', "")
            .split("fn compact_layer(")
            .nth(1)
            .expect("按层压缩那条命令")
            .split("\nfn ")
            .next()
            .unwrap_or_default()
            .to_string();
        assert!(
            command.contains("write_layer_summary(&mut send, &plan, &summary)"),
            "命令不再叫这一个助手，\"压没压生效\"就又回到只测不到的那一格里：{command}"
        );
    }

    /// `still_over` 那一格从落地起就只有写、没有读：它自己的注释写着"这时候正确的动作
    /// 不是继续压，而是让阶梯往下一步走，或者干脆报 Notice"，而命令原先一路走到摘要。
    /// 于是那一次的形状是：**照付一次请求、照少一段历史、还是坐不进去**
    #[test]
    fn a_compaction_that_still_would_not_fit_is_refused_before_the_request() {
        use crate::session::layers::CompactPlan;

        let blocked = CompactPlan {
            from_id: "e1".into(),
            through_id: "e2".into(),
            rows: 6,
            chars: 5_000,
            still_over: true,
        };
        let problem = layer_compaction_blocker(&blocked, 9_000, 4_000)
            .expect("全换成一行摘要还坐不进去，就该拦下来");
        // 拦下的同时要说得出「换了几行、还剩多少、预算多少」——只说"不行"等于让人猜
        assert!(
            problem.contains("6 行") && problem.contains("9000") && problem.contains("4000"),
            "话要说得照着能修：{problem}"
        );
        assert!(
            problem.contains("没有压"),
            "要当场说清这一次没动手：{problem}"
        );

        // 正对照：坐得进去的那一次不许被拦
        let fits = CompactPlan {
            still_over: false,
            ..blocked.clone()
        };
        assert_eq!(
            layer_compaction_blocker(&fits, 3_000, 4_000),
            None,
            "判据不许退化成\"按层压缩一概不做\""
        );

        // 链路 + 次序：命令要在付那一次请求**之前**问这一格
        let command = include_str!("chat.rs")
            .replace('\r', "")
            .split("fn compact_layer(")
            .nth(1)
            .expect("按层压缩那条命令")
            .split("\nfn ")
            .next()
            .unwrap_or_default()
            .to_string();
        let asked = command
            .find("layer_compaction_blocker(&plan,")
            .expect("命令得问这一格");
        let paid = command.find("summarize_history(").expect("摘要那一次调用");
        assert!(asked < paid, "拦在请求之前才省下钱，拦在之后只是事后道歉");
    }

    /// 结构体入参那道"认生键"的闸：`Price` 早就有了，`ChatMessage` 当时没有。
    /// 少它一次，一个写错的键名（`toolCall` 而不是 `toolCallId`、snake_case 而不是
    /// camelCase）就被 serde 安静地当成"这条没带那个字段"——于是**话题恢复重放的
    /// 工具历史会少一截，而调用方收到的是一个成功**。宁缺勿假不能建立在"少带就少带"上
    #[test]
    fn a_stray_key_on_an_inbound_message_is_refused_not_silently_dropped() {
        let plain: Vec<ChatMessage> = serde_json::from_str(r#"[{"role":"user","content":"一问"}]"#)
            .expect("前端现在送的形状必须照收");
        assert_eq!(plain[0].content, "一问");
        let _: Vec<ChatMessage> = serde_json::from_str(
            r#"[{"role":"assistant","content":"一答","toolCalls":[{"id":"c","name":"read_file","arguments":"{}"}],"toolCallId":null}]"#,
        )
        .expect("带工具历史的完整形状也得收，话题恢复靠它");

        let stray = serde_json::from_str::<Vec<ChatMessage>>(
            r#"[{"role":"user","content":"一问","tool_call_id":null}]"#,
        );
        assert!(
            stray.is_err(),
            "键名写错不许读成「这条没带那个字段」：{}",
            stray.map(|_| "居然收下了").unwrap_or_default()
        );
        let problem = stray.expect_err("这里该是报错").to_string();
        assert!(
            problem.contains("unknown field") && problem.contains("tool_call_id"),
            "要报得出是哪个键：{problem}"
        );
    }

    /// 命令级：分叉只能切在用户消息上，切出来的那一段要**原样**带上条目 id 与父链。
    /// 这条判据以前一根针都没有：整条命令四处要 `AppHandle`，而那四处与"切在哪儿"无关
    #[test]
    fn a_fork_cuts_at_a_user_message_and_keeps_the_cut_rows_untouched() {
        use crate::history::Conversation;
        use crate::session::legacy::{from_ledger, Migration};

        let dir = crate::test_support::scoped_temp_dir("fork-anchor");
        let mut send = Send::open(
            Migration {
                opened: from_ledger(&Conversation {
                    id: "conv_fork".into(),
                    ..Default::default()
                }),
                path: dir.join("conv_fork.jsonl"),
            },
            standing_head(),
            Vec::new(),
        )
        .expect("打开发送视图该成功");
        let asked = send
            .push(Message::User {
                content: "要分叉的那一问".into(),
                images: Vec::new(),
                audios: Vec::new(),
                videos: Vec::new(),
            })
            .expect("追加用户行该成功");
        let answered = send
            .push(Message::Assistant(SettledAssistant {
                content: "它答过了".into(),
                tool_calls: Vec::new(),
                stop: StopReason::Stop,
                reasoning: None,
                error: None,
                thinking_signature: None,
                reasoning_items_json: None,
            }))
            .expect("追加助手行该成功");
        let again = send
            .push(Message::User {
                content: "下一问".into(),
                images: Vec::new(),
                audios: Vec::new(),
                videos: Vec::new(),
            })
            .expect("再追加一条用户行该成功");
        send.save();
        let ids_of = |log: &crate::session::SessionLog| {
            log.path()
                .expect("这条分支有路径")
                .iter()
                .map(|entry| entry.id.clone())
                .collect::<Vec<String>>()
        };

        // 正对照：切在最后那条用户消息上——前面几轮都得跟着走，id 一个都不许重铸
        let whole = ids_of(
            &branch_to_user_anchor(&mut send.opened, &again).expect("切在用户消息上该分得出去"),
        );
        assert_eq!(
            whole.last().map(String::as_str),
            Some(again.as_str()),
            "锚点就得是那一条用户消息"
        );
        assert!(
            whole.contains(&asked) && whole.contains(&answered),
            "锚点之前的每一行都该在：{whole:?}"
        );

        // 关键那一格：切在助手消息上要拒。那等于把"半轮"当成一轮带走——
        // 新话题最后一句是模型自己说完的结尾，它下一发看到的就是一个没人问过的场面
        let refused =
            branch_to_user_anchor(&mut send.opened, &answered).expect_err("锚点是助手消息就该被拒");
        assert!(
            refused.contains("分叉锚点必须是一条用户消息"),
            "要说清拒的是锚点这件事：{refused}"
        );

        // 指不到的那格也要拒，而不是悄悄分出一个空分支
        assert!(
            branch_to_user_anchor(&mut send.opened, "没这一条").is_err(),
            "锚点不存在时报错，不是分出一个空话题"
        );

        // 切短一档也得分得出去（"退回第一轮重问"正是这个功能的主用途），
        // 而且切短之后后面那两轮不该跟过来
        let short =
            ids_of(&branch_to_user_anchor(&mut send.opened, &asked).expect("切到第一轮该成功"));
        assert_eq!(short.last().map(String::as_str), Some(asked.as_str()));
        assert!(
            !short.contains(&answered) && !short.contains(&again),
            "切短了还带着后面两轮，那是复制而不是分叉：{short:?}"
        );

        // 链路那一半：命令要是自己另写一套切法，上面这些就只是在测一个没人在用的函数
        let command = include_str!("chat.rs")
            .replace('\r', "")
            .split("fn conversation_fork(")
            .nth(1)
            .expect("分叉那条命令")
            .split("\nfn ")
            .next()
            .unwrap_or_default()
            .to_string();
        assert!(
            command.contains("branch_to_user_anchor(&mut source, &entry_id)"),
            "分叉命令不再叫那一个助手，锚点判据就换了地方写：{command}"
        );
    }

    /// 内容真的变了就要认账——但认的是"多一行"这笔账，不是"整段前缀作废"。
    /// §12 Stage 8 的准入门：改 AGENTS.md 只往末尾追加一行差分行，此前发出的字节一个都不动
    #[test]
    fn editing_a_section_appends_one_row_instead_of_breaking_the_prefix() {
        use crate::history::Conversation;
        use crate::session::legacy::{from_ledger, Migration};

        let dir = crate::test_support::scoped_temp_dir("section-edit");
        let opened = Migration {
            opened: from_ledger(&Conversation {
                id: "conv_section".into(),
                ..Default::default()
            }),
            path: dir.join("conv_section.jsonl"),
        };
        let mut send = Send::open(
            opened,
            standing_head(),
            conversation_sections(Some("旧约定"), Some("<skills/>"), None, None),
        )
        .expect("打开发送视图该成功");
        send.sync_sections().expect("首轮该把两段都写进日志");
        send.push(Message::User {
            content: "一句".into(),
            images: Vec::new(),
            audios: Vec::new(),
            videos: Vec::new(),
        })
        .expect("追加该成功");
        let turn_one = send.rows().to_vec();
        assert_eq!(turn_one.len(), 4, "常驻段 + 两段 + 一句问题");
        assert!(
            turn_one[1]["content"]
                .as_str()
                .unwrap()
                .starts_with(WORKSPACE_MARKER)
                && turn_one[2]["content"]
                    .as_str()
                    .unwrap()
                    .starts_with(SKILLS_MARKER),
            "段序不能倒：约定在前、清单在后"
        );

        // 用户中途改了 AGENTS.md：下一轮渲染出来的段内容不一样了
        send.sections = conversation_sections(Some("新约定"), Some("<skills/>"), None, None);
        send.sync_sections().expect("变更该落成一行差分行");
        send.push(Message::User {
            content: "两句".into(),
            images: Vec::new(),
            audios: Vec::new(),
            videos: Vec::new(),
        })
        .expect("追加该成功");
        let turn_two = send.rows().to_vec();

        assert!(
            turn_two.starts_with(&turn_one),
            "改一段约定竟而动到了已发出的字节：\n{turn_one:?}\n{turn_two:?}"
        );
        assert_eq!(
            turn_two.len() - turn_one.len(),
            2,
            "只该多出「新约定」那一行，另一行是本轮的新输入"
        );
        assert!(turn_two[turn_one.len()]["content"]
            .as_str()
            .unwrap()
            .contains("新约定"));
        // 默认提示词是唯一有资格坐在最前的消息，它一个字节都不该动
        assert_eq!(turn_two[0]["content"].as_str(), Some(DEFAULT_SYSTEM_PROMPT));

        // 未变的段再来一轮：一行都不该多写（"沿用已存渲染"不需要字节门控代码）
        let steady = send.rows().to_vec();
        send.sync_sections().expect("同步该成功");
        assert_eq!(send.rows(), steady, "内容没变就不该产生任何新行");

        // 阳性对照：同一笔改动在旧形状（段坐常驻段、每轮重渲染）下会让前缀当场断掉
        let head_old = vec![
            json!({ "role": "system", "content": DEFAULT_SYSTEM_PROMPT }),
            json!({ "role": "system", "content": format!("{WORKSPACE_MARKER}\n旧约定") }),
            json!({ "role": "user", "content": "一句" }),
        ];
        let head_new = [
            json!({ "role": "system", "content": DEFAULT_SYSTEM_PROMPT }),
            json!({ "role": "system", "content": format!("{WORKSPACE_MARKER}\n新约定") }),
            json!({ "role": "user", "content": "一句" }),
            json!({ "role": "user", "content": "两句" }),
        ];
        assert!(
            !head_new.starts_with(&head_old),
            "旧形状若也保得住前缀，上面那条 starts_with 断言什么都没测"
        );
    }

    fn config_with(format: &str) -> AppConfig {
        AppConfig {
            base_url: "https://relay.example/v1".into(),
            model: "glm-5.3".into(),
            api_format: format.into(),
            reasoning_effort: "high".into(),
            ..Default::default()
        }
    }

    /// 录制的事件序列直接喂状态机，不碰网络
    fn feed(events: &[Value]) -> (ResponsesState, Vec<String>) {
        let mut state = ResponsesState::default();
        let mut emitted = Vec::new();
        for event in events {
            apply_responses_event(&mut state, event, &mut |chat| match chat {
                ChatEvent::Delta { text } => emitted.push(format!("delta:{text}")),
                ChatEvent::Reasoning { text } => emitted.push(format!("reasoning:{text}")),
                ChatEvent::Notice { text } => emitted.push(format!("notice:{text}")),
                _ => {}
            });
        }
        (state, emitted)
    }

    fn anthropic_config() -> AppConfig {
        AppConfig {
            base_url: "https://api.anthropic.com".into(),
            model: "claude-sonnet-4-6".into(),
            api_format: "anthropic".into(),
            ..Default::default()
        }
    }

    fn gemini_config() -> AppConfig {
        AppConfig {
            base_url: "https://generativelanguage.googleapis.com/v1beta".into(),
            model: "gemini-2.5-pro".into(),
            api_format: "gemini".into(),
            ..Default::default()
        }
    }

    /// Gemini 的请求体：systemInstruction/contents/tools/thinkingConfig 各归各位，
    /// 参数里的 `$schema` 递归剥掉，functionCall 的 id 按序合成
    #[test]
    fn the_gemini_payload_maps_the_three_wire_shapes() {
        let config = gemini_config();
        let thread = vec![
            json!({ "role": "system", "content": "常驻提示词" }),
            json!({ "role": "user", "content": "第一问" }),
            json!({ "role": "assistant", "content": "", "tool_calls": [
                { "id": "call_a", "function": { "name": "read_file", "arguments": "{\"path\":\"a.txt\"}" } }
            ]}),
            json!({ "role": "tool", "tool_call_id": "call_a", "content": "文件内容" }),
        ];
        let declared = vec![json!({ "type": "function", "function": {
                "name": "read_file", "description": "读文件",
                "parameters": { "type": "object", "$schema": "https://json-schema.org/draft/2020-12/schema", "properties": {} } } })];

        let payload = gemini_payload(&config, &thread, &declared);

        let instruction = payload["systemInstruction"]["parts"][0]["text"]
            .as_str()
            .expect("system 进顶层 systemInstruction");
        assert_eq!(instruction, "常驻提示词");

        let contents = payload["contents"].as_array().expect("contents 数组");
        assert_eq!(contents.len(), 3, "user/assistant(tool_calls)/tool 三条");
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(
            contents[1]["content"][0]["functionCall"]["name"], "read_file",
            "assistant 的 tool_calls 翻成 functionCall（content 就是 parts 数组）"
        );
        // tool 行翻成 user 消息里的 functionResponse，name 按 call_id 反查
        assert_eq!(contents[2]["role"], "user", "工具结果坐在 user 消息里");
        assert_eq!(
            contents[2]["content"][0]["functionResponse"]["name"],
            "read_file"
        );
        assert_eq!(
            contents[2]["content"][0]["functionResponse"]["response"]["result"],
            "文件内容"
        );

        let declarations = &payload["tools"][0]["functionDeclarations"];
        assert_eq!(declarations[0]["name"], "read_file");
        assert!(
            declarations[0]["parameters"].get("$schema").is_none(),
            "参数里的 $schema 要递归剥掉，Gemini 不认"
        );
    }

    /// Gemini 的思考档位：low/medium/high 映射 thinkingBudget，
    /// 刻意不给 0——对 Flash 是关闭、对 Pro 是非法，缺省才是两边都安全的"交给模型"
    #[test]
    fn the_gemini_thinking_budget_maps_efforts_and_never_zero() {
        let mut config = gemini_config();
        for (effort, budget) in [("low", 1024), ("medium", 8192), ("high", 24576)] {
            config.reasoning_effort = effort.into();
            let payload = gemini_payload(&config, &[], &[]);
            assert_eq!(
                payload["generationConfig"]["thinkingConfig"]["thinkingBudget"],
                json!(budget),
                "{effort} → {budget}"
            );
        }
        config.reasoning_effort = "minimal".into();
        let payload = gemini_payload(&config, &[], &[]);
        assert!(
            payload["generationConfig"].get("thinkingConfig").is_none(),
            "minimal 不发 thinkingConfig：0 对 Pro 非法，缺省对两边都安全"
        );
    }

    /// Gemini 的流式事件：text/thought/functionCall（无 id，按 parts 顺序合成
    /// call_N）/usageMetadata 累计读数/MAX_TOKENS 截断
    #[test]
    fn gemini_events_drive_the_chat_state() {
        let mut state = ChatState::default();
        let mut events: Vec<ChatEvent> = Vec::new();
        let mut emit = |event: ChatEvent| {
            events.push(event);
        };

        apply_gemini_event(
            &mut state,
            &json!({
                "candidates": [{ "content": { "parts": [
                    { "text": "先想" , "thought": true },
                    { "text": "答案" },
                    { "functionCall": { "name": "read_file", "args": { "path": "a.txt" } } }
                ]}, "finishReason": "STOP" }],
                "usageMetadata": { "promptTokenCount": 100, "candidatesTokenCount": 30,
                                   "thoughtsTokenCount": 12, "cachedContentTokenCount": 40 }
            }),
            &mut emit,
        );
        assert_eq!(state.text, "答案");
        assert_eq!(state.reasoning, "先想");
        assert_eq!(state.calls.len(), 1, "一个 functionCall 一个槽");
        let call = state.calls.get(&0).expect("槽按序号");
        assert_eq!(call.id, "call_0", "Gemini 没有 call id：按序合成");
        assert_eq!(call.name, "read_file");
        assert_eq!(call.arguments, r#"{"path":"a.txt"}"#);
        let usage = state.usage.as_ref().expect("用量要读出来");
        assert_eq!(
            usage.input_tokens, 140,
            "input = 裸输入 + 缓存命中（归一化）"
        );
        assert_eq!(usage.output_tokens, 30);
        assert_eq!(
            usage.reasoning_tokens, 12,
            "thoughts 是 output 的子集，只展示"
        );
        assert_eq!(usage.cached_tokens, Some(40));

        apply_gemini_event(
            &mut state,
            &json!({ "candidates": [{ "finishReason": "MAX_TOKENS" }] }),
            &mut emit,
        );
        assert!(state.truncated, "MAX_TOKENS 要落截断标记");
        assert!(events
            .iter()
            .any(|event| matches!(event, ChatEvent::Delta { .. })));
        assert!(events
            .iter()
            .any(|event| matches!(event, ChatEvent::Reasoning { .. })));
    }

    /// Gemini 的模型清单：{ models: [{ name: "models/x" }] }，剥掉前缀
    #[test]
    fn the_gemini_models_payload_strips_the_prefix() {
        let body = json!({ "models": [
            { "name": "models/gemini-2.5-pro", "displayName": "Gemini 2.5 Pro" },
            { "name": "models/gemini-2.5-flash", "displayName": "Gemini 2.5 Flash" }
        ]});
        let models: Vec<String> = body["models"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item["name"].as_str().map(str::to_string))
                    .map(|name| name.strip_prefix("models/").unwrap_or(&name).to_string())
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(models, vec!["gemini-2.5-pro", "gemini-2.5-flash"]);
    }

    /// 模型列表地址：OpenAI 系拼 /models；Anthropic 系拼 /v1/models，
    /// 且尊重 base_url 里已有的 /v1（中转站约定）
    #[test]
    fn the_models_endpoint_follows_the_wire_convention() {
        assert_eq!(
            models_endpoint_for("https://api.openai.com/v1", false),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(
            models_endpoint_for("https://api.anthropic.com", true),
            "https://api.anthropic.com/v1/models"
        );
        assert_eq!(
            models_endpoint_for("https://relay.example.test/v1/", true),
            "https://relay.example.test/v1/models"
        );
    }

    /// Anthropic 的事件序列喂同一个 ChatState，不碰网络
    fn feed_anthropic(events: &[Value]) -> (ChatState, Vec<String>) {
        let mut state = ChatState::default();
        let mut emitted = Vec::new();
        for event in events {
            apply_anthropic_event(&mut state, event, &mut |chat| match chat {
                ChatEvent::Delta { text } => emitted.push(format!("delta:{text}")),
                ChatEvent::Reasoning { text } => emitted.push(format!("reasoning:{text}")),
                ChatEvent::Notice { text } => emitted.push(format!("notice:{text}")),
                _ => {}
            });
        }
        (state, emitted)
    }

    /// 断点三处：system 末块、最后一个工具、最后一条 user 消息的末块。
    /// 中间块一律不打——断点多一处就多一份缓存写
    #[test]
    fn the_anthropic_payload_puts_breakpoints_on_system_tools_and_the_last_user_block() {
        let config = anthropic_config();
        let thread = vec![
            json!({ "role": "system", "content": "常驻提示词" }),
            json!({ "role": "system", "content": "【技能清单】技能列表" }),
            json!({ "role": "user", "content": "第一问" }),
            json!({ "role": "assistant", "content": "", "tool_calls": [
                { "id": "call_a", "function": { "name": "read_file", "arguments": "{\"path\":\"a.txt\"}" } }
            ]}),
            json!({ "role": "tool", "tool_call_id": "call_a", "content": "文件内容" }),
        ];
        let declared = vec![
            json!({ "type": "function", "function": {
                "name": "read_file", "description": "读文件", "parameters": { "type": "object" } } }),
            json!({ "type": "function", "function": {
                "name": "list_files", "description": "列目录", "parameters": { "type": "object" } } }),
        ];

        let payload = anthropic_payload(&config, &thread, &declared);

        let system = payload["system"].as_array().expect("system 得是块数组");
        assert_eq!(system.len(), 2);
        assert!(system[0].get("cache_control").is_none(), "中间块不打断点");
        assert_eq!(system[1]["cache_control"]["type"], "ephemeral");

        let tools = payload["tools"].as_array().expect("有工具");
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "read_file");
        assert_eq!(tools[0]["input_schema"]["type"], "object");
        assert!(tools[0].get("cache_control").is_none());
        assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");

        let messages = payload["messages"].as_array().expect("有消息");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"][0]["type"], "tool_use");
        assert_eq!(
            messages[1]["content"][0]["input"]["path"], "a.txt",
            "input 必须是对象，不是 JSON 字符串"
        );
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "call_a");
        assert_eq!(
            messages[2]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        assert!(messages[1]["content"][0].get("cache_control").is_none());

        // anthropic 线没有 prompt_cache_key 这种参数
        assert!(payload.get("prompt_cache_key").is_none());
    }

    /// 连续 user 行（正文、工具结果、插话）必须并成一条消息——
    /// Anthropic 那边 user/assistant 要交替，消息边界画在协议要求的地方
    #[test]
    fn consecutive_user_rows_merge_into_one_anthropic_message() {
        let config = anthropic_config();
        let thread = vec![
            json!({ "role": "user", "content": "第一问" }),
            json!({ "role": "tool", "tool_call_id": "call_a", "content": "结果" }),
            json!({ "role": "user", "content": "（执行中途的插话）先停一下" }),
        ];
        let payload = anthropic_payload(&config, &thread, &[]);
        let messages = payload["messages"].as_array().expect("有消息");
        assert_eq!(messages.len(), 1, "三条 user 行必须并成一条消息");
        let blocks = messages[0]["content"].as_array().expect("内容块");
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "tool_result");
        assert_eq!(blocks[2]["type"], "text");
        // 断点随合并挪到合并后的末块上
        assert_eq!(blocks[2]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn an_anthropic_round_reassembles_text_thinking_and_tool_calls() {
        let (state, emitted) = feed_anthropic(&[
            json!({ "type": "message_start", "message": { "usage": {
                "input_tokens": 100, "cache_read_input_tokens": 80,
                "cache_creation_input_tokens": 20 } } }),
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text" } }),
            json!({ "type": "content_block_delta", "index": 0,
                    "delta": { "type": "text_delta", "text": "答案" } }),
            json!({ "type": "content_block_start", "index": 1,
                    "content_block": { "type": "tool_use", "id": "call_a", "name": "read_file" } }),
            json!({ "type": "content_block_delta", "index": 1,
                    "delta": { "type": "input_json_delta", "partial_json": "{\"path\":\"a" } }),
            json!({ "type": "content_block_delta", "index": 1,
                    "delta": { "type": "input_json_delta", "partial_json": ".txt\"}" } }),
            json!({ "type": "content_block_delta", "index": 2,
                    "delta": { "type": "thinking_delta", "thinking": "想想" } }),
            json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" },
                    "usage": { "output_tokens": 15 } }),
        ]);

        assert_eq!(state.text, "答案");
        assert_eq!(emitted, vec!["delta:答案", "reasoning:想想"]);
        let calls: Vec<&ToolCallBuffer> = state.calls.values().collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments, r#"{"path":"a.txt"}"#);

        // 归一化不变量：input_tokens 含缓存命中（100 + 80 + 20），命中单独可读
        let usage = state.usage.expect("message_start 里该有 usage");
        assert_eq!(usage.input_tokens, 200);
        assert_eq!(usage.cached_tokens, Some(80));
        assert_eq!(usage.cache_write_tokens, 20);
        assert_eq!(usage.output_tokens, 15, "message_delta 把输出涨到全量");
    }

    /// 缓存字段缺失 = 未上报（None），不能冒充命中 0；
    /// 裸输入原样进账，计价时按全价算
    #[test]
    fn an_anthropic_round_without_cache_fields_stays_unreported() {
        let (state, _) = feed_anthropic(&[
            json!({ "type": "message_start", "message": { "usage": { "input_tokens": 50 } } }),
            json!({ "type": "content_block_delta", "index": 0,
                    "delta": { "type": "text_delta", "text": "好" } }),
            json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" },
                    "usage": { "output_tokens": 3 } }),
        ]);
        let usage = state.usage.expect("有 usage");
        assert_eq!(usage.input_tokens, 50);
        assert_eq!(usage.cached_tokens, None);
        assert_eq!(usage.output_tokens, 3);
    }

    #[test]
    fn an_anthropic_round_cut_by_the_token_cap_tells_the_user() {
        let (state, emitted) = feed_anthropic(&[
            json!({ "type": "message_start", "message": { "usage": { "input_tokens": 10 } } }),
            json!({ "type": "message_delta", "delta": { "stop_reason": "max_tokens" },
                    "usage": { "output_tokens": 2048 } }),
        ]);
        assert!(state.truncated);
        assert!(
            emitted
                .iter()
                .any(|line| line.starts_with("notice:") && line.contains("截断")),
            "截断没有告诉用户：{emitted:?}"
        );
    }

    #[test]
    fn an_anthropic_error_event_reports_the_reason() {
        let (state, _) = feed_anthropic(&[json!({ "type": "error",
            "error": { "type": "overloaded_error", "message": "Overloaded" } })]);
        assert_eq!(state.failure.as_deref(), Some("服务商报错：Overloaded"));
    }

    /// responses 线的缓存身份与 chat 线同一套门控：能力表说支持且属于某个话题才带
    #[test]
    fn the_responses_payload_carries_the_cache_identity_like_the_chat_line() {
        let mut config = config_with("responses");
        config.base_url = "https://api.openai.com/v1".into();
        let thread = vec![json!({ "role": "user", "content": "一问" })];

        let keyed = responses_payload(&config, &thread, &[], Some("conv_1"));
        assert_eq!(keyed["prompt_cache_key"], "conv_1");
        assert_eq!(keyed["store"], false, "无状态重放不变");

        let one_off = responses_payload(&config, &thread, &[], None);
        assert!(
            one_off.get("prompt_cache_key").is_none(),
            "一次性调用不该占用话题的缓存条目"
        );

        // 没依据的中转服务商：不猜
        let silent = responses_payload(&config_with("responses"), &thread, &[], Some("conv_1"));
        assert!(silent.get("prompt_cache_key").is_none());
    }

    /// OpenRouter 这类负载均衡网关靠话题亲和头粘上游，缓存才有得命中；
    /// 不认识的服务商一个非标头都不发
    #[test]
    fn only_openrouter_gateways_get_a_session_affinity_header() {
        let headers = affinity_headers("https://openrouter.ai/api/v1", Some("conv_1"));
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0, "x-session-id");
        assert_eq!(headers[0].1, "conv_1");

        assert!(
            affinity_headers("https://api.deepseek.com/v1", Some("conv_1")).is_empty(),
            "别的服务商不发非标头"
        );
        assert!(
            affinity_headers("https://openrouter.ai/api/v1", None).is_empty(),
            "一次性调用没有可粘的话题"
        );
    }

    /// Anthropic 思考档位：effort 映射成 budget_tokens，预算与 max_tokens 共享上限，
    /// temperature 整个省略（与扩展思考不兼容）；没配档位就维持原样
    #[test]
    fn the_anthropic_line_maps_effort_to_a_thinking_budget_and_drops_temperature() {
        let mut config = anthropic_config();
        config.max_tokens = 4096;

        config.reasoning_effort = "medium".into();
        let payload = anthropic_payload(&config, &[], &[]);
        assert_eq!(payload["thinking"]["type"], "enabled");
        assert_eq!(payload["thinking"]["budget_tokens"], 8192);
        assert_eq!(payload["max_tokens"], 4096 + 8192, "预算加进输出上限");
        assert!(payload.get("temperature").is_none(), "思考与温度不兼容");

        config.reasoning_effort = "xhigh".into();
        let payload = anthropic_payload(&config, &[], &[]);
        assert_eq!(
            payload["thinking"]["budget_tokens"], 16384,
            "xhigh 收敛到 high 档"
        );

        config.reasoning_effort = String::new();
        let payload = anthropic_payload(&config, &[], &[]);
        assert!(payload.get("thinking").is_none(), "空档位不发思考参数");
        assert_eq!(payload["max_tokens"], 4096);
        assert_eq!(payload["temperature"], config.temperature);
    }

    /// 合帧：一串快速增量并成一次 IPC；非增量事件是节点，先冲帧再放行；
    /// 流结束时 flush 兜底，一个字都不许丢
    #[test]
    fn rapid_deltas_reach_the_ui_in_one_frame_and_nothing_is_lost() {
        let mut coalescer = DeltaCoalescer::new();
        let mut emitted: Vec<ChatEvent> = Vec::new();

        {
            let mut sink = |event: ChatEvent| emitted.push(event);
            for piece in ["第一", "、第二", "、第三"] {
                coalescer.push(ChatEvent::Delta { text: piece.into() }, &mut sink);
            }
            coalescer.push(
                ChatEvent::Notice {
                    text: "截断了".into(),
                },
                &mut sink,
            );
            coalescer.flush(&mut sink);
        }

        let deltas: Vec<&str> = emitted
            .iter()
            .filter_map(|event| match event {
                ChatEvent::Delta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            deltas,
            vec!["第一、第二、第三"],
            "快速增量必须合成一帧，且 Notice 前冲出来"
        );
        assert!(
            emitted
                .iter()
                .any(|event| matches!(event, ChatEvent::Notice { .. })),
            "非增量事件原样放行"
        );

        // 正文与思考各走各的事件，合帧后依然可分
        {
            let mut sink = |event: ChatEvent| emitted.push(event);
            coalescer.push(ChatEvent::Delta { text: "答".into() }, &mut sink);
            coalescer.push(ChatEvent::Reasoning { text: "想".into() }, &mut sink);
            coalescer.flush(&mut sink);
        }
        assert!(matches!(
            emitted[emitted.len() - 2],
            ChatEvent::Delta { .. }
        ));
        assert!(matches!(emitted.last(), Some(ChatEvent::Reasoning { .. })));
    }

    #[test]
    fn a_responses_round_reassembles_text_and_reads_cache_tokens() {
        let (state, emitted) = feed(&[
            json!({ "type": "response.created" }),
            json!({ "type": "response.output_text.delta", "delta": "第一段" }),
            json!({ "type": "response.reasoning_text.delta", "delta": "先想想" }),
            json!({ "type": "response.output_text.delta", "delta": "、第二段" }),
            json!({"type":"response.completed","response":{"usage":{
                "input_tokens": 13120, "output_tokens": 10,
                "input_tokens_details": {"cached_tokens": 8192, "cache_write_tokens": 0},
                "output_tokens_details": {"reasoning_tokens": 4}, "total_tokens": 13130}}}),
        ]);

        assert_eq!(state.text, "第一段、第二段");
        // 思考过程走自己的事件，不能混进正文
        assert_eq!(
            emitted,
            vec!["delta:第一段", "reasoning:先想想", "delta:、第二段"]
        );

        let usage = state.usage.expect("completed 里该有 usage");
        assert_eq!(usage.input_tokens, 13120);
        assert_eq!(usage.cached_tokens, Some(8192));
        assert_eq!(usage.reasoning_tokens, 4);
    }

    #[test]
    fn function_calls_are_keyed_by_output_index_and_the_done_event_wins() {
        let (state, _) = feed(&[
            json!({"type":"response.output_item.added","output_index":0,"item":{
                "type":"function_call","call_id":"call_a","name":"read_file","arguments":""}}),
            json!({"type":"response.function_call_arguments.delta","output_index":0,
                   "item_id":"item_a","delta":"{\"path\":\"a"}),
            json!({"type":"response.function_call_arguments.delta","output_index":0,
                   "item_id":"item_a","delta":".txt\"}"}),
            json!({"type":"response.output_item.added","output_index":1,"item":{
                "type":"function_call","call_id":"call_b","name":"list_files","arguments":""}}),
            json!({"type":"response.function_call_arguments.delta","output_index":1,
                   "item_id":"item_b","delta":"{}"}),
            // 增量拼出来的和服务商自己拼好的对不上时，以 done 为准
            json!({"type":"response.function_call_arguments.done","output_index":0,
                   "item_id":"item_a","arguments":"{\"path\":\"a.txt\",\"x\":1}"}),
            json!({"type":"response.output_item.done","output_index":0,"item":{
                "type":"function_call","call_id":"call_a","name":"read_file",
                "arguments":"{\"path\":\"a.txt\",\"x\":2}"}}),
        ]);

        let calls: Vec<&ToolCallBuffer> = state.calls.values().collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments, r#"{"path":"a.txt","x":2}"#);
        assert_eq!(calls[1].id, "call_b");
        assert_eq!(calls[1].arguments, "{}");
    }

    /// 有的服务商不流式给参数，只在 output_item.done 里一次给全
    #[test]
    fn a_tool_call_that_only_shows_up_in_the_done_item_is_still_executed() {
        let (state, _) = feed(&[
            json!({"type":"response.output_item.added","output_index":0,"item":{
                "type":"reasoning","id":"rs_1","summary":[]}}),
            json!({"type":"response.output_item.done","output_index":1,"item":{
                "type":"function_call","call_id":"call_c","name":"list_files","arguments":"{}"}}),
        ]);

        let calls: Vec<&ToolCallBuffer> = state.calls.values().collect();
        assert_eq!(calls.len(), 1, "reasoning 项不是工具调用");
        assert_eq!(calls[0].id, "call_c");
        assert_eq!(calls[0].name, "list_files");
        assert_eq!(calls[0].arguments, "{}");
    }

    #[test]
    fn a_failed_response_reports_the_endpoints_reason() {
        let (state, _) = feed(&[json!({"type":"response.failed",
                                       "response":{"error":{"message":"上游超时"}}})]);
        assert_eq!(state.failure.as_deref(), Some("服务商生成失败：上游超时"));
    }

    #[test]
    fn an_error_event_is_not_silently_treated_as_an_empty_answer() {
        let (state, _) = feed(&[json!({"type": "error", "message": "model not found"})]);
        assert_eq!(
            state.failure.as_deref(),
            Some("服务商报错：model not found")
        );
    }

    /// 真机实测到的形状：ai.hybgzs.com 的 /responses 用 response.incomplete 收尾，
    /// 而且照样在 response.usage 里回用量。只认 completed 就会把这一轮的账整个记丢。
    #[test]
    fn an_incomplete_response_keeps_its_usage_and_says_it_was_cut() {
        let (state, emitted) = feed(&[
            json!({"type":"response.output_text.delta","delta":"写到一半"}),
            json!({"type":"response.incomplete","response":{
                "usage": {"input_tokens": 20, "output_tokens": 128,
                          "input_tokens_details": {"cached_tokens": 0}},
                "incomplete_details": {"reason": "max_output_tokens"}}}),
        ]);

        assert_eq!(
            state.usage.expect("incomplete 也带 usage").output_tokens,
            128
        );
        assert!(
            emitted
                .iter()
                .any(|line| line.starts_with("notice:") && line.contains("截断")),
            "截断没有告诉用户：{emitted:?}"
        );
    }

    /// chat 格式没有终态事件，截断只写在最后一个块的 finish_reason 里
    #[test]
    fn a_chat_round_that_hits_the_token_cap_tells_the_user() {
        let mut state = ChatState::default();
        let mut emitted = Vec::new();
        for chunk in [
            json!({"choices":[{"delta":{"content":"半句话"},"finish_reason":null}]}),
            json!({"choices":[{"delta":{},"finish_reason":"length"}]}),
        ] {
            apply_chat_event(&mut state, &chunk, &mut |event| match event {
                ChatEvent::Delta { text } => emitted.push(format!("delta:{text}")),
                ChatEvent::Notice { text } => emitted.push(format!("notice:{text}")),
                _ => {}
            });
        }

        assert_eq!(state.text, "半句话");
        assert_eq!(emitted.len(), 2);
        assert!(emitted[1].contains("上限"), "{}", emitted[1]);
    }

    /// 支持之外的事件必须丢掉：content_part.added 里也带 text，误读就是把半句话当正文
    #[test]
    fn events_about_things_aglab_does_not_support_are_dropped_not_misread() {
        let (state, emitted) = feed(&[
            json!({"type":"response.audio.delta","delta":"AAAA"}),
            json!({"type":"response.web_search_call.completed"}),
            json!({"type":"response.content_part.added","part":{"type":"output_text","text":"别把这段当正文"}}),
            json!({"type":"response.refusal.delta","delta":"不该显示"}),
        ]);
        assert!(state.text.is_empty(), "误读了不支持的事件：{}", state.text);
        assert!(emitted.is_empty());
    }

    #[test]
    fn the_shared_thread_translates_into_responses_input_items() {
        // 与投影测试那份同形：本 mod 里也要有一份（跨 mod 不能共用测试助手）
        let inputs = |images: bool, audios: bool, videos: bool| ModalInputs {
            images,
            audios,
            videos,
        };
        let thread = vec![
            json!({"role":"system","content":"你是助手"}),
            json!({"role":"user","content":"读一下 a.txt"}),
            json!({"role":"assistant","content":"我看一下","tool_calls":[
                {"id":"call_a","type":"function","function":{
                    "name":"read_file","arguments":"{\"path\":\"a.txt\"}"}}]}),
            json!({"role":"tool","tool_call_id":"call_a","content":"文件内容"}),
        ];

        let items = responses_input(&thread, inputs(false, false, false));
        assert_eq!(
            items.len(),
            5,
            "带工具调用的那条回复要拆成 正文 + 调用 两项"
        );
        assert_eq!(items[0], json!({"role": "system", "content": "你是助手"}));
        assert_eq!(
            items[2],
            json!({"role": "assistant", "content": "我看一下"})
        );
        assert_eq!(
            items[3],
            json!({"type":"function_call","call_id":"call_a","name":"read_file",
                   "arguments":"{\"path\":\"a.txt\"}"})
        );
        assert_eq!(
            items[4],
            json!({"type":"function_call_output","call_id":"call_a","output":"文件内容"})
        );
    }

    #[test]
    fn the_responses_request_uses_its_own_field_names() {
        let payload = responses_payload(&config_with("responses"), &[], &[], None);

        assert_eq!(payload["max_output_tokens"], json!(2048));
        assert_eq!(
            payload["reasoning"],
            json!({ "effort": "high", "summary": "auto" })
        );
        // encrypted_content 是 reasoning 项回放的载体，store:false 的多轮离不开它
        assert_eq!(payload["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(payload["store"], json!(false));
        assert!(payload.get("messages").is_none());
        assert!(payload.get("max_tokens").is_none());
        // 没有工具时整段省略，发 "tools": [] 服务商会当成非法请求
        assert!(payload.get("tools").is_none());
    }

    /// reasoning 项必须排在它配对的 function_call 之前（OpenAI 按 id 配对），
    /// 而且原样透传——不改写服务商发回的任何字段
    #[test]
    fn reasoning_items_are_replayed_before_their_function_calls() {
        let inputs = |images: bool, audios: bool, videos: bool| ModalInputs {
            images,
            audios,
            videos,
        };
        let reasoning_item = json!({
            "type": "reasoning",
            "id": "rs_1",
            "summary": [{ "type": "summary_text", "text": "先读文件" }],
            "encrypted_content": "enc-payload",
        });
        let thread = vec![
            json!({ "role": "user", "content": "读一下" }),
            json!({
                "role": "assistant",
                "content": "",
                "reasoning_items": [reasoning_item],
                "tool_calls": [
                    { "id": "fc_1", "function": { "name": "read_file", "arguments": "{}" } }
                ],
            }),
        ];

        let items = responses_input(&thread, inputs(false, false, false));
        // items[0] 是 user 行；reasoning 项紧随其后、必须先于 function_call
        assert_eq!(items.len(), 3, "user + reasoning + function_call");
        assert_eq!(items[1]["type"], "reasoning");
        assert_eq!(items[1]["id"], "rs_1");
        assert_eq!(items[1]["encrypted_content"], "enc-payload");
        assert_eq!(items[2]["type"], "function_call");
        assert_eq!(items[2]["call_id"], "fc_1");
    }

    /// 流式留档 + Azure 回填：done 里缺 encrypted_content 时，从终态按 id 补全；
    /// 没配思考档位的线不发 include，也就不该有回放凭据可攒
    #[test]
    fn the_responses_stream_keeps_reasoning_items_and_backfills_encrypted_content() {
        let mut state = ResponsesState::default();
        for event in [
            json!({"type":"response.output_item.done","item":{
                "type":"reasoning","id":"rs_1","summary":[]}}),
            json!({"type":"response.output_item.done","item":{
                "type":"function_call","call_id":"fc_1","name":"read_file","arguments":"{}"}}),
            json!({"type":"response.completed","response":{
                "output":[{"type":"reasoning","id":"rs_1","encrypted_content":"enc-abc"}],
                "usage":{"input_tokens":10,"output_tokens":5}}}),
        ] {
            apply_responses_event(&mut state, &event, &mut |_| {});
        }

        assert_eq!(state.reasoning_items.len(), 1);
        assert_eq!(state.reasoning_items[0]["id"], "rs_1");
        assert_eq!(
            state.reasoning_items[0]["encrypted_content"], "enc-abc",
            "终态回填必须补上加密载荷"
        );
    }

    /// chat 线剥掉思考回放凭据（DeepSeek 明确不收 reasoning 字段）；
    /// 没有凭据的行原样透传
    #[test]
    fn the_chat_line_strips_thinking_replay_credentials() {
        let config = config_with("chat");
        let carrying = json!({
            "role": "assistant",
            "content": "",
            "thinking": "想一想",
            "thinking_signature": "sig-1",
            "reasoning_items": [{ "type": "reasoning", "id": "rs_1" }],
            "tool_calls": [
                { "id": "call_a", "function": { "name": "read_file", "arguments": "{}" } }
            ],
        });
        let thread = vec![json!({ "role": "user", "content": "问" }), carrying];

        let payload = chat_payload(&config, &thread, &[], None);
        let messages = payload["messages"].as_array().expect("有消息");
        assert_eq!(
            messages[0],
            json!({ "role": "user", "content": "问" }),
            "无凭据的行逐字节不变"
        );
        assert!(messages[1].get("thinking").is_none());
        assert!(messages[1].get("thinking_signature").is_none());
        assert!(messages[1].get("reasoning_items").is_none());
        assert_eq!(
            messages[1]["tool_calls"][0]["function"]["name"], "read_file",
            "剥凭据不伤工具调用"
        );
    }

    /// Anthropic 思考回放：签名随流拼出；带签名的助手行翻成 thinking 块
    /// 且必须坐在最前；没有签名的历史思考不回放
    #[test]
    fn the_anthropic_line_captures_signatures_and_replays_thinking_blocks() {
        let mut state = ChatState::default();
        for event in [
            json!({"type":"content_block_delta","index":0,
                   "delta":{"type":"thinking_delta","thinking":"想想"}}),
            json!({"type":"content_block_delta","index":0,
                   "delta":{"type":"signature_delta","signature":"sig-"}}),
            json!({"type":"content_block_delta","index":0,
                   "delta":{"type":"signature_delta","signature":"part"}}),
        ] {
            apply_anthropic_event(&mut state, &event, &mut |_| {});
        }
        assert_eq!(state.reasoning_signature.as_deref(), Some("sig-part"));

        let config = anthropic_config();
        let thread = vec![
            json!({ "role": "user", "content": "问" }),
            json!({
                "role": "assistant",
                "content": "",
                "thinking": "想想",
                "thinking_signature": "sig-part",
                "tool_calls": [
                    { "id": "call_a", "function": { "name": "read_file", "arguments": "{}" } }
                ],
            }),
            json!({ "role": "tool", "tool_call_id": "call_a", "content": "结果" }),
        ];
        let payload = anthropic_payload(&config, &thread, &[]);
        let messages = payload["messages"].as_array().expect("有消息");
        let blocks = messages[1]["content"].as_array().expect("内容块");
        assert_eq!(blocks[0]["type"], "thinking", "思考块必须坐在最前");
        assert_eq!(blocks[0]["thinking"], "想想");
        assert_eq!(blocks[0]["signature"], "sig-part");
        assert_eq!(blocks[1]["type"], "tool_use");

        // 没有签名的思考（老话题）不回放，也不该伪造一个空签名出去
        let unsigned = vec![
            json!({ "role": "user", "content": "问" }),
            json!({ "role": "assistant", "content": "答复", "reasoning": "想想",
                    "tool_calls": [] }),
        ];
        let payload = anthropic_payload(&config, &unsigned, &[]);
        let messages = payload["messages"].as_array().expect("有消息");
        assert!(messages[1]["content"][0].get("thinking").is_none());
        assert_eq!(messages[1]["content"][0]["type"], "text");
    }

    /// wire 行的条件字段：只有服务商真发回过凭据的行才带，
    /// 老话题的行与历史字节逐字相同
    #[test]
    fn wire_rows_carry_replay_credentials_only_when_present() {
        let bare = SettledAssistant {
            content: "答复".into(),
            tool_calls: vec![],
            stop: StopReason::Stop,
            reasoning: Some("想想".into()),
            error: None,
            thinking_signature: None,
            reasoning_items_json: None,
        };
        let row = Message::Assistant(bare.clone()).to_wire();
        assert!(row.get("thinking").is_none(), "老形状不许变");
        assert!(row.get("thinking_signature").is_none());
        assert!(row.get("reasoning_items").is_none());

        let replaying = SettledAssistant {
            thinking_signature: Some("sig-1".into()),
            reasoning_items_json: Some(r#"[{"type":"reasoning","id":"rs_1"}]"#.into()),
            ..bare
        };
        let row = Message::Assistant(replaying).to_wire();
        assert_eq!(row["thinking"], "想想");
        assert_eq!(row["thinking_signature"], "sig-1");
        assert_eq!(row["reasoning_items"][0]["id"], "rs_1");
    }

    #[test]
    fn chat_tools_are_flattened_for_the_responses_wire() {
        let flat = responses_tools(&[tools::skill_schema()]);

        assert_eq!(flat.len(), 1);
        assert_eq!(flat[0]["type"], "function");
        assert_eq!(flat[0]["name"], "load_skill");
        assert_eq!(flat[0]["strict"], json!(false));
        // chat 格式那层 function 包装必须去掉，否则服务商看不到 name
        assert!(flat[0].get("function").is_none());
    }

    #[test]
    fn chat_stream_still_reassembles_split_ids_and_reads_cache_tokens() {
        let mut state = ChatState::default();
        let mut emitted = Vec::new();
        for chunk in [
            json!({"choices":[{"delta":{"content":"你好"}}]}),
            json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"id":"call_","function":{"name":"read","arguments":""}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"id":"a","function":{"name":"_file","arguments":"{}"}}]}}]}),
            json!({"choices":[{"delta":{}}],"usage":{
                "prompt_tokens": 100, "completion_tokens": 5,
                "prompt_tokens_details": {"cached_tokens": 64},
                "completion_tokens_details": {"reasoning_tokens": 2}}}),
        ] {
            apply_chat_event(&mut state, &chunk, &mut |event| {
                if let ChatEvent::Delta { text } = event {
                    emitted.push(text);
                }
            });
        }

        assert_eq!(state.text, "你好");
        let call = state.calls.values().next().expect("一个调用");
        assert_eq!(call.id, "call_a");
        assert_eq!(call.name, "read_file");
        assert_eq!(emitted, vec!["你好".to_string()]);

        let usage = state.usage.expect("chat 的 usage");
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.cached_tokens, Some(64));
        assert_eq!(usage.reasoning_tokens, 2);
    }

    /// D7 的要害：DeepSeek 系把命中量放在顶层 prompt_cache_hit_tokens，
    /// 只读 OpenAI 系字段的代码会在它上面永远读出"未上报"，
    /// 于是命中率面板与分档计价一起失真
    #[test]
    fn chat_usage_reads_both_caches_field_spellings() {
        let deepseek = Usage::from_chat(&json!({
            "prompt_tokens": 5000, "completion_tokens": 20,
            "prompt_cache_hit_tokens": 4864, "prompt_cache_miss_tokens": 136,
        }))
        .expect("DeepSeek 的 usage 也该解析出来");
        assert_eq!(deepseek.cached_tokens, Some(4864));
        assert_eq!(deepseek.input_tokens, 5000);

        let openai = Usage::from_chat(&json!({
            "prompt_tokens": 5000, "completion_tokens": 20,
            "prompt_tokens_details": {"cached_tokens": 4864},
        }))
        .expect("OpenAI 系的用法不能被我改坏");
        assert_eq!(openai.cached_tokens, Some(4864));
    }

    /// 「真 0」和「没上报」必须是两个可区分的值——否则不支持缓存的服务商
    /// 会一直显示 0% 命中，看着像我们把自己的前缀改坏了
    #[test]
    fn an_absent_cache_field_is_unreported_not_zero() {
        let silent = Usage::from_chat(&json!({"prompt_tokens": 100, "completion_tokens": 5}))
            .expect("只有裸 token 数也要能解析");
        assert_eq!(silent.cached_tokens, None);

        let reported_zero = Usage::from_chat(&json!({"prompt_tokens": 100, "completion_tokens": 5,
                                     "prompt_tokens_details": {"cached_tokens": 0}}))
        .expect("服务商明确回了 0");
        assert_eq!(reported_zero.cached_tokens, Some(0));
    }

    #[test]
    fn the_two_wire_formats_hit_different_paths() {
        let config = config_with("responses");
        assert_eq!(
            config.responses_endpoint(),
            "https://relay.example/v1/responses"
        );
        assert_eq!(
            config.chat_endpoint(),
            "https://relay.example/v1/chat/completions"
        );
        assert!(config.uses_responses());
        // 老配置里没有这个字段，或者值写错了，都不能变成"一条请求都发不出去"
        assert!(!config_with("chat").uses_responses());
        assert!(!config_with("").uses_responses());
        assert!(!config_with("Response").uses_responses());
    }
}

/// 模型列表地址：OpenAI 系 {base}/models；Anthropic 系 {base}/v1/models
/// （官方根地址不带 /v1，中转站常带——与 anthropic_endpoint 同一套约定）
/// 服务商的模型清单 URL。`pool_catalog` 也要它（代理解析按这条 URL 判回环与绕过名单）
pub(crate) fn models_endpoint_for(base_url: &str, anthropic: bool) -> String {
    let base = base_url.trim_end_matches('/');
    if anthropic {
        if base.ends_with("/v1") {
            format!("{base}/models")
        } else {
            format!("{base}/v1/models")
        }
    } else {
        format!("{base}/models")
    }
}

/// 拉一个服务商的模型列表。`net_egress_allow` 是出口名单，`plan` 是这一发要试的代理路
/// （直连就是一条 None 路）。`list_models` 命令与模型池的目录刷新共用这一条路径——
/// 两处各写各的，就会有一处悄悄漏掉出口闸。
///
/// 归因与换路和模型请求同源：服务商回状态码 = 通路成立（`Reached`），只有"一个头都没
/// 拿到"才换下一条并记 `Unreachable`。以前这一发根本不进代理账本，一条坏代理要等到
/// 用户真发一条消息才暴露
pub(crate) fn fetch_models_for(
    base_url: &str,
    wire_format: &str,
    key: &str,
    credential_service: &str,
    net_egress_allow: &[String],
    plan: &mut crate::proxy::Plan,
) -> Result<Vec<String>, String> {
    // 出口名单先问、凭据后取：一家根本不会去连的主机，不该先为它解一次密钥（§16）。
    // 检查的是**真正要 GET 的那条 URL**，不是配置里那个串——两处分开写就会有不一致
    let endpoint = models_endpoint_for(base_url, wire_format == "anthropic");
    crate::egress::guard(net_egress_allow, &endpoint)?;

    let mut tried: Vec<String> = Vec::new();
    let mut last = String::from("模型列表一条路都没试出去。");
    while let Some(mut leg) = plan.next() {
        let via = leg.proxy_url().unwrap_or("直连").to_string();
        let agent = crate::proxy::agent_for(leg.proxy_url())?;
        let mut request = with_timeouts(agent.get(&endpoint), Duration::from_secs(30));
        match wire_format {
            "anthropic" => {
                // OAuth 订阅令牌同请求线的特判：Bearer + beta 头
                request = if key.starts_with("sk-ant-oat01") {
                    request
                        .header("authorization", format!("Bearer {key}"))
                        .header("anthropic-beta", "oauth-2025-04-20")
                } else {
                    request
                        .header("x-api-key", key)
                        .header("anthropic-version", ANTHROPIC_VERSION)
                };
            }
            "gemini" => {
                request = request.header("x-goog-api-key", key);
            }
            _ => {
                request = request.header("authorization", format!("Bearer {key}"));
            }
        }
        let started = Instant::now();
        let failure = match request.call() {
            Ok(response) => {
                leg.note_head(started.elapsed());
                let mut response = response;
                return match response.body_mut().read_json::<Value>() {
                    Ok(body) => {
                        leg.finish(crate::proxy::Outcome::Reached);
                        // Gemini 的清单是 { models: [{ name: "models/x" }] }，剥掉前缀
                        if wire_format == "gemini" {
                            return Ok(body["models"]
                                .as_array()
                                .map(|items| {
                                    items
                                        .iter()
                                        .filter_map(|item| {
                                            item["name"].as_str().map(str::to_string)
                                        })
                                        .map(|name| {
                                            name.strip_prefix("models/")
                                                .unwrap_or(&name)
                                                .to_string()
                                        })
                                        .collect()
                                })
                                .unwrap_or_default());
                        }
                        Ok(body["data"]
                            .as_array()
                            .map(|items| {
                                items
                                    .iter()
                                    .filter_map(|item| item["id"].as_str().map(str::to_string))
                                    .collect()
                            })
                            .unwrap_or_default())
                    }
                    // 头之后读不出 JSON：可能是代理拼坏了字节流，也可能是服务商给了个
                    // 奇怪的东西。归到"掐流"那一类——记读数，不进冷却
                    Err(error) => {
                        leg.finish(crate::proxy::Outcome::Interrupted);
                        return Err(format!("模型列表不是合法 JSON：{error}"));
                    }
                };
            }
            Err(error) => EgressFail {
                outcome: crate::proxy::outcome_of(&error),
                message: match &error {
                    ureq::Error::StatusCode(code) => {
                        // 同上：状态码也是拿到的头，慢样本要留下
                        leg.note_head(started.elapsed());
                        describe_status(*code, credential_service, base_url)
                    }
                    other => format!("请求模型列表失败：{other}"),
                },
            },
        };
        let outcome = failure.outcome;
        let message = failure.message;
        leg.finish(outcome);
        if outcome != crate::proxy::Outcome::Unreachable {
            return Err(message);
        }
        last = message;
        tried.push(format!("{via}：{last}"));
    }
    if tried.len() <= 1 {
        return Err(last);
    }
    Err(format!(
        "代理池的 {} 条路都拉不到模型列表：{}。检查这些代理是否还在跑。",
        tried.len(),
        tried.join("；")
    ))
}

/// 拉取服务商的模型列表。全部参数可缺省：缺省时按当前生效配置拉（对话页的
/// 刷新按钮）；档案弹窗则带上草稿的服务商、凭据目标乃至刚敲的密钥——
/// 草稿还没落盘，不传过去就只能对着错误的服务商拉。
///
/// **async 命令**：这一发是阻塞的网络请求（30 秒超时）加一次凭据库读取，
/// 同步命令跑在主线程上——切一次档案、点一次刷新，整个窗口陪那个服务商冻到超时。
/// 挪进阻塞池（`decision_jev_system_one` / `pool_catalog` 是同款先例）
#[tauri::command]
pub async fn list_models(
    app: AppHandle,
    base_url: Option<String>,
    api_format: Option<String>,
    credential_service: Option<String>,
    credential_user: Option<String>,
    secret: Option<String>,
) -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        list_models_of(
            app,
            base_url,
            api_format,
            credential_service,
            credential_user,
            secret,
        )
    })
    .await
    .map_err(|error| format!("拉取模型列表的线程没能跑完：{error}"))?
}

/// 同步的那一半：真正发请求的部分，签名与原命令一致
fn list_models_of(
    app: AppHandle,
    base_url: Option<String>,
    api_format: Option<String>,
    credential_service: Option<String>,
    credential_user: Option<String>,
    secret: Option<String>,
) -> Result<Vec<String>, String> {
    let config = config::load(&app);
    let base = base_url
        .map(|url| url.trim().to_string())
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| config.base_url.clone());
    if base.trim().is_empty() {
        return Err("尚未配置推理服务商地址。".into());
    }
    let wire_format = api_format
        .as_deref()
        .unwrap_or(&config.api_format)
        .to_string();
    provider_gate(&config)?;

    // 密钥解析：弹窗里刚敲的 > 档案自己的凭据目标 > 当前配置的
    let key = match secret
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        Some(secret) => secret,
        None => {
            let service = credential_service
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(&config.credential_service);
            let user = credential_user
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(&config.credential_user);
            config::api_key_for(service, user)?
        }
    };
    let service = credential_service
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&config.credential_service)
        .to_string();

    // 代理绑定按当前连接解析（逐模型覆盖含在内——这一发就是 config.model）
    let mut plan = crate::proxy::plan(
        &config,
        &models_endpoint_for(&base, wire_format == "anthropic"),
    )?;

    fetch_models_for(
        &base,
        &wire_format,
        &key,
        &service,
        &config.net_egress_allow,
        &mut plan,
    )
}

#[tauri::command]
pub fn read_attachment(path: String) -> Result<Value, String> {
    tools::read_attachment(&path)
}

/// 预览/附件的 asset 协议按需放行：静态 scope 只盖固定目录（粘贴临时目录、
/// gen 产物、备份），用户自选路径与恢复会话里的历史附件在渲染前经这里逐个放行。
/// 安全收窄的配套：与其放开整个 $APPDATA，不如精确到文件
#[tauri::command]
pub fn asset_allow(app: tauri::AppHandle, paths: Vec<String>) {
    let scope = app.asset_protocol_scope();
    for path in &paths {
        if !path.is_empty() {
            let _ = scope.allow_file(path);
        }
    }
}

/// 设置页「内置子助理」名册：出厂定义合并覆盖后的完整视图（停用的也在——
/// 卡片要置灰展示，停用态由 disabled 字段说明，不是从列表里消失）。
/// 定义住 spawn.rs 的代码里，配置只存覆盖；这里是把两份合成一份给界面。
/// **必须是 async**：同步命令在主线程上跑，而它要从盘上读并解析整份
/// config.json——配置越长，进一次子助理页就把 UI 冻一下
#[tauri::command]
pub async fn builtin_subagents_list(
    app: tauri::AppHandle,
) -> Result<Vec<crate::spawn::BuiltinSubagentView>, String> {
    Ok(crate::spawn::builtin_views(&config::load(&app)))
}

/// 点名的连接要真的生效：服务商整份抄写、模型名单格覆盖、找不到档案是错误不是静默。
/// 三格都空 = 完全跟配置走（今天所有内置档案的样子）
#[cfg(test)]
mod connection_override_tests {
    use super::*;
    use wire::{anthropic_payload, project_content, responses_payload, ImageDialect, ModalInputs};

    fn config_with_relay() -> AppConfig {
        let mut config = AppConfig::default();
        config.base_url = "https://current.example.test/v1".into();
        config.model = "current-model".into();
        config.profiles.push(crate::config::EndpointProfile {
            id: "prof-relay".into(),
            name: "中转".into(),
            base_url: "https://relay.example.test/v1".into(),
            model: "relay-default".into(),
            api_format: "chat".into(),
            ..Default::default()
        });
        config
    }

    #[test]
    fn a_named_endpoint_copies_its_connection_and_an_explicit_model_wins_over_its_default() {
        let config = with_connection(config_with_relay(), None, Some("prof-relay")).unwrap();
        assert_eq!(
            config.base_url, "https://relay.example.test/v1",
            "连接域整份照档案抄"
        );
        assert_eq!(
            config.model, "relay-default",
            "没另点模型时，档案默认就是这一发的模型"
        );
        assert_eq!(
            config.active_profile_id, "",
            "这是一发请求的路由，不是切换档案：当前档案不许动"
        );

        let config = with_connection(
            config_with_relay(),
            Some("explicit-model"),
            Some("prof-relay"),
        )
        .unwrap();
        assert_eq!(
            config.model, "explicit-model",
            "模型名是更具体的一档，压过档案默认"
        );
    }

    #[test]
    fn empty_or_missing_names_fall_through_honestly() {
        let config = with_connection(config_with_relay(), Some("  "), Some("")).unwrap();
        assert_eq!(
            config.base_url, "https://current.example.test/v1",
            "空串与空白都是跟着配置走"
        );
        assert_eq!(config.model, "current-model");

        let error = with_connection(config_with_relay(), None, Some("prof-gone")).unwrap_err();
        assert!(
            error.contains("prof-gone"),
            "找不到档案要说出是哪一张：{error}"
        );
    }

    /* ---- 粘贴截图与链接抓取（输入框 composer 的两个新能力）---- */

    /// 最小 PNG 头：签名 8 字节 + IHHR 块头（长度+类型）+ IHDR 里的宽高。
    /// 只造到第 24 字节，够 png_dimensions 读数
    fn fake_png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG 签名
            0x00, 0x00, 0x00, 0x0D, // IHDR 长度
            b'I', b'H', b'D', b'R',
        ];
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes
    }

    #[test]
    fn png_dimensions_reads_ihdr_at_fixed_offset() {
        assert_eq!(png_dimensions(&fake_png(1920, 1080)), Some((1920, 1080)));
        assert_eq!(png_dimensions(b"not a png"), None);
        assert_eq!(png_dimensions(&[]), None);
        // 截断在 IHDR 中间：不炸，只报不可知
        assert_eq!(png_dimensions(&fake_png(1, 1)[..20]), None);
    }

    #[test]
    fn image_mime_table_covers_paste_formats_only() {
        assert_eq!(image_mime_of("C:\\Temp\\paste-1.PNG"), Some("image/png"));
        assert_eq!(image_mime_of("/tmp/x.jpeg"), Some("image/jpeg"));
        assert_eq!(image_mime_of("/tmp/x.webp"), Some("image/webp"));
        assert_eq!(image_mime_of("/tmp/x.txt"), None);
        assert_eq!(image_mime_of("noext"), None);
    }

    #[test]
    fn sniff_image_ext_reads_magic_bytes_not_mime() {
        // Win+Shift+S 的截图是 PNG：mime 就算丢了（DataTransferItem 失效成空串）也能认出
        assert_eq!(sniff_image_ext(&fake_png(10, 10)), Some("png"));
        let jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 0, 0, 0];
        assert_eq!(sniff_image_ext(&jpeg), Some("jpg"));
        assert_eq!(sniff_image_ext(b"GIF89a...."), Some("gif"));
        assert_eq!(sniff_image_ext(b"RIFF1234WEBPVP8 "), Some("webp"));
        assert_eq!(sniff_image_ext(b"BMxx"), Some("bmp"));
        assert_eq!(sniff_image_ext(b"just text"), None);
        // mime 表与魔数表对同一格式的答案一致——返回的 mime 不会说谎
        assert_eq!(mime_to_ext("image/png"), Some("png"));
        assert_eq!(mime_to_ext("image/jxl"), None);
    }

    /// 图片不再被写成"当前话题无法直接查看图片内容"——那句话在模型换成收图的档之后就
    /// 是谎。行上挂的是引用，"这一发能不能看见"由出站投影按当次的模型决定
    #[test]
    fn with_attachments_records_the_image_and_stops_claiming_it_is_unreadable() {
        let dir = std::env::temp_dir().join("aglab-test-with-att");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("paste-test.png");
        std::fs::write(&path, fake_png(800, 600)).unwrap();
        let pending = with_attachments("看这张图", &[path.to_string_lossy().into()]);
        std::fs::remove_file(&path).ok();
        let content = &pending.content;

        assert!(content.starts_with("看这张图"), "{content}");
        assert!(
            content.contains("（image/png，800×600"),
            "mime 与尺寸要说全：{content}"
        );
        assert!(
            !content.contains("无法直接查看"),
            "那句谎不归这一行负责了：{content}"
        );
        // 二进制字节不能被当文本灌进上下文
        assert!(!content.contains('\u{0}'), "{content}");
        let image = pending.images.first().expect("图要记成引用");
        assert_eq!(image.mime, "image/png");
        assert!(image.bytes > 0, "字节数要留给用量与压缩那两本账");
    }

    /// 音频/视频附件要被认成媒体引用（走多模态投影），而不是死在文本读取上
    #[test]
    fn with_attachments_records_audio_and_video_as_refs() {
        let dir = std::env::temp_dir().join("aglab-test-with-av");
        std::fs::create_dir_all(&dir).unwrap();
        let audio = dir.join("clip.mp3");
        std::fs::write(&audio, b"fake-mp3").unwrap();
        let video = dir.join("clip.mp4");
        std::fs::write(&video, b"fake-mp4").unwrap();

        let pending = with_attachments(
            "听这段",
            &[
                audio.to_string_lossy().into_owned(),
                video.to_string_lossy().into_owned(),
            ],
        );
        std::fs::remove_file(&audio).ok();
        std::fs::remove_file(&video).ok();

        assert!(
            pending.content.contains("附件：音频"),
            "{}",
            pending.content
        );
        assert!(
            pending.content.contains("附件：视频"),
            "{}",
            pending.content
        );
        assert!(!pending.content.contains("读取失败"), "{}", pending.content);
        assert_eq!(pending.audios.first().expect("音频引用").mime, "audio/mpeg");
        assert_eq!(pending.videos.first().expect("视频引用").mime, "video/mp4");
        // wire 形状：audio/video 部件在中性行上各归各
        let wire = Message::User {
            content: pending.content,
            images: pending.images,
            audios: pending.audios,
            videos: pending.videos,
        }
        .to_wire();
        let parts = wire["content"].as_array().expect("带媒体的行是数组");
        assert_eq!(parts[1]["type"], "audio");
        assert_eq!(parts[2]["type"], "video");
    }

    /// 纯文本行是绝大多数请求的全部：投影必须**一个字都不动**它。
    /// 服务商的缓存按前缀字节匹配，动一下就是整段作废
    #[test]
    fn a_row_without_images_projects_byte_identical() {
        let row = serde_json::json!({ "role": "user", "content": "就一句话" });
        for dialect in [
            ImageDialect::Chat,
            ImageDialect::Responses,
            ImageDialect::Anthropic,
        ] {
            assert_eq!(
                project_content(&row, dialect, inputs(true, false, false)),
                row
            );
            assert_eq!(
                project_content(&row, dialect, inputs(false, false, false)),
                row
            );
        }
    }

    fn image_row(path: &str) -> Value {
        serde_json::json!({
            "role": "user",
            "content": [
                { "type": "text", "text": "看这张图" },
                { "type": "image", "path": path, "mime": "image/png", "bytes": 120 },
            ],
        })
    }

    /// 三家各自的外壳：同一条带图的行，chat 要 `image_url.url` 的 data URL、
    /// responses 要 `input_image.image_url` 字符串、anthropic 要 `source.base64`
    #[test]
    fn an_image_becomes_the_dialects_own_block() {
        let dir = std::env::temp_dir().join("aglab-test-vision");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shot.png");
        std::fs::write(&path, fake_png(64, 64)).unwrap();
        let lossy = path.to_string_lossy().into_owned();

        let chat = project_content(
            &image_row(&lossy),
            ImageDialect::Chat,
            inputs(true, false, false),
        );
        let parts = chat["content"].as_array().expect("chat 线是数组");
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[1]["type"], "image_url");
        assert!(
            parts[1]["image_url"]["url"]
                .as_str()
                .is_some_and(|url| url.starts_with("data:image/png;base64,")),
            "chat 线要 data URL"
        );

        let responses = project_content(
            &image_row(&lossy),
            ImageDialect::Responses,
            inputs(true, false, false),
        );
        let parts = responses["content"].as_array().expect("responses 线是数组");
        assert_eq!(parts[0]["type"], "input_text");
        assert_eq!(parts[1]["type"], "input_image");
        assert!(parts[1]["image_url"]
            .as_str()
            .is_some_and(|url| url.starts_with("data:image/png;base64,")));

        let anthropic = project_content(
            &image_row(&lossy),
            ImageDialect::Anthropic,
            inputs(true, false, false),
        );
        let parts = anthropic["content"]
            .as_array()
            .expect("anthropic 线是块数组");
        assert_eq!(parts[1]["type"], "image");
        assert_eq!(parts[1]["source"]["type"], "base64");
        assert_eq!(parts[1]["source"]["media_type"], "image/png");
        assert!(parts[1]["source"]["data"]
            .as_str()
            .is_some_and(|data| !data.is_empty()));

        std::fs::remove_file(&path).ok();
        crate::test_support::remove_tree(&dir);
    }

    /// 模型不收图时**不是静默丢掉**：那一块换成一句写在正文里的话，带上路径。
    /// 模型不知道自己被给了一个看不见的东西时，会照着"用户发了张图"把内容编出来
    #[test]
    fn an_unaccepted_image_is_named_in_the_transcript_not_dropped() {
        let projected = project_content(
            &image_row("C:/nope/shot.png"),
            ImageDialect::Chat,
            inputs(false, false, false),
        );
        let parts = projected["content"].as_array().expect("仍然是数组");
        assert_eq!(parts.len(), 2, "图的位置要留下一句话，不能塌成一行");
        assert_eq!(parts[1]["type"], "text");
        let note = parts[1]["text"].as_str().expect("说明是文字块");
        assert!(note.contains("没发出去"), "{note}");
        assert!(note.contains("C:/nope/shot.png"), "路径要留给模型：{note}");
    }

    /// 图发得出去但文件没了：说"已经不在了"，不是发一张空图
    #[test]
    fn a_vanished_image_says_so() {
        let row = image_row("C:/definitely/not/here/aglab.png");
        let projected = project_content(&row, ImageDialect::Chat, inputs(true, false, false));
        let note = projected["content"][1]["text"].as_str().expect("文字块");
        assert!(note.contains("已经不在了"), "{note}");
    }

    /// 音频/视频在收它们的方言里各有各的外壳：chat/responses 用 OpenAI 的
    /// input_audio 与通行的 video_url，Gemini 走 inline_data，anthropic 不收——
    /// 缺口就地说明而不是发非法外壳
    #[test]
    fn audio_and_video_take_their_own_dialect_shapes() {
        let dir = std::env::temp_dir().join("aglab-test-av-input");
        std::fs::create_dir_all(&dir).unwrap();
        let audio_path = dir.join("clip.mp3");
        std::fs::write(&audio_path, b"fake-mp3-bytes").unwrap();
        let video_path = dir.join("clip.mp4");
        std::fs::write(&video_path, b"fake-mp4-bytes").unwrap();
        let audio = audio_path.to_string_lossy().into_owned();
        let video = video_path.to_string_lossy().into_owned();

        let chat = project_content(
            &media_row("audio", "audio/mpeg", &audio),
            ImageDialect::Chat,
            inputs(false, true, false),
        );
        let parts = chat["content"].as_array().expect("chat 线是数组");
        assert_eq!(parts[1]["type"], "input_audio");
        assert_eq!(
            parts[1]["input_audio"]["format"], "mp3",
            "audio/mpeg 要写成 mp3"
        );
        assert!(!parts[1]["input_audio"]["data"]
            .as_str()
            .unwrap_or_default()
            .is_empty());

        let gemini = project_content(
            &media_row("video", "video/mp4", &video),
            ImageDialect::Gemini,
            inputs(false, false, true),
        );
        let parts = gemini["content"].as_array().expect("gemini 线是数组");
        assert_eq!(parts[1]["inline_data"]["mime_type"], "video/mp4");
        assert!(!parts[1]["inline_data"]["data"]
            .as_str()
            .unwrap_or_default()
            .is_empty());

        let anthropic = project_content(
            &media_row("audio", "audio/mpeg", &audio),
            ImageDialect::Anthropic,
            inputs(false, true, false),
        );
        let parts = anthropic["content"]
            .as_array()
            .expect("anthropic 线是块数组");
        assert_eq!(parts[1]["type"], "text");
        assert!(
            parts[1]["text"]
                .as_str()
                .unwrap_or_default()
                .contains("音频没发出去"),
            "方言缺口要写进正文：{}",
            parts[1]["text"]
        );

        std::fs::remove_file(&audio_path).ok();
        std::fs::remove_file(&video_path).ok();
        crate::test_support::remove_tree(&dir);
    }

    /// 模型没勾「收音频/收视频」时同样不是静默丢掉：位置上留一句话，带路径
    #[test]
    fn unaccepted_audio_and_video_are_named_not_dropped() {
        let projected = project_content(
            &media_row("audio", "audio/mpeg", "C:/nope/clip.mp3"),
            ImageDialect::Chat,
            inputs(false, false, false),
        );
        let note = projected["content"][1]["text"].as_str().expect("文字块");
        assert!(note.contains("没发出去"), "{note}");
        assert!(note.contains("收音频"), "{note}");
        assert!(note.contains("C:/nope/clip.mp3"), "{note}");

        let projected = project_content(
            &media_row("video", "video/mp4", "C:/nope/clip.mp4"),
            ImageDialect::Chat,
            inputs(false, false, false),
        );
        let note = projected["content"][1]["text"].as_str().expect("文字块");
        assert!(note.contains("没发出去"), "{note}");
        assert!(note.contains("C:/nope/clip.mp4"), "{note}");
    }

    fn vision_config(accepts: bool) -> AppConfig {
        let mut config = AppConfig::default();
        config.model = "m".into();
        config.models = vec![crate::config::ModelSpec {
            model: "m".into(),
            supports_images: accepts,
            ..Default::default()
        }];
        config
    }

    fn inputs(images: bool, audios: bool, videos: bool) -> ModalInputs {
        ModalInputs {
            images,
            audios,
            videos,
        }
    }

    fn media_row(kind: &str, mime: &str, path: &str) -> Value {
        serde_json::json!({
            "role": "user",
            "content": [
                { "type": "text", "text": "看/听这段" },
                { "type": kind, "path": path, "mime": mime, "bytes": 120 },
            ],
        })
    }

    /// responses 线最容易犯的错：它读 `content.as_str()`，数组形会读成空串，
    /// 于是**带图的那一问整条消失**——用户问了，模型那边根本没这个问题
    #[test]
    fn the_responses_line_does_not_drop_the_row_that_carried_an_image() {
        let dir = std::env::temp_dir().join("aglab-test-resp-vision");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shot.png");
        std::fs::write(&path, fake_png(32, 32)).unwrap();
        let lossy = path.to_string_lossy().into_owned();
        let thread = vec![image_row(&lossy)];

        let items = responses_payload(&vision_config(true), &thread, &[], None)["input"]
            .as_array()
            .expect("input 数组")
            .clone();
        assert_eq!(items.len(), 1, "带图的那一问不能被丢掉");
        let parts = items[0]["content"].as_array().expect("数组形 content");
        assert_eq!(parts[0]["type"], "input_text");
        assert_eq!(parts[1]["type"], "input_image");

        // 不收图的模型：这一行照样在，只是图换成一句说明
        let items = responses_payload(&vision_config(false), &thread, &[], None)["input"]
            .as_array()
            .expect("input 数组")
            .clone();
        let parts = items[0]["content"].as_array().expect("数组形 content");
        assert_eq!(parts[1]["type"], "input_text");
        assert!(
            parts[1]["text"]
                .as_str()
                .is_some_and(|text| text.contains("没发出去")),
            "{parts:?}"
        );

        std::fs::remove_file(&path).ok();
        crate::test_support::remove_tree(&dir);
    }

    /// anthropic 线：图块要并进 user 消息的块数组，且 `source.media_type` 说得清是什么图
    #[test]
    fn the_anthropic_line_puts_the_image_into_the_user_block_array() {
        let dir = std::env::temp_dir().join("aglab-test-anth-vision");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shot.png");
        std::fs::write(&path, fake_png(32, 32)).unwrap();
        let thread = vec![image_row(&path.to_string_lossy())];

        let payload = anthropic_payload(&vision_config(true), &thread, &[]);
        let messages = payload["messages"].as_array().expect("messages 数组");
        let user = messages
            .iter()
            .find(|row| row["role"] == "user")
            .expect("要有一条 user 消息");
        let blocks = user["content"].as_array().expect("块数组");
        assert_eq!(blocks.len(), 2, "文字与图各一块：{blocks:?}");
        assert_eq!(blocks[1]["source"]["media_type"], "image/png");

        std::fs::remove_file(&path).ok();
        crate::test_support::remove_tree(&dir);
    }

    /// 超出条数上限的那几张不是"压一压还能塞"，是不发并说清是第几张
    #[test]
    fn images_past_the_count_limit_are_skipped_by_name() {
        let mut parts = vec![serde_json::json!({ "type": "text", "text": "一屏图" })];
        for index in 0..=crate::session::entry::IMAGE_MAX_COUNT {
            parts.push(serde_json::json!({
                "type": "image", "path": format!("C:/x/{index}.png"),
                "mime": "image/png", "bytes": 10,
            }));
        }
        let row = serde_json::json!({ "role": "user", "content": parts });
        let projected = project_content(&row, ImageDialect::Chat, inputs(true, false, false));
        let blocks = projected["content"].as_array().expect("数组");
        let last = blocks.last().expect("最后一块");
        assert_eq!(
            last["type"],
            "text",
            "第 {} 张应被挡下",
            crate::session::entry::IMAGE_MAX_COUNT + 1
        );
        assert!(
            last["text"].as_str().is_some_and(|text| {
                text.contains(&format!(
                    "第 {} 张",
                    crate::session::entry::IMAGE_MAX_COUNT + 1
                ))
            }),
            "要说清是第几张：{last}"
        );
    }

    #[test]
    fn decode_entities_handles_named_and_numeric() {
        assert_eq!(decode_entities("a &amp; b"), "a & b");
        assert_eq!(decode_entities("&lt;tag&gt; &quot;x&quot;"), "<tag> \"x\"");
        assert_eq!(decode_entities("&#39;"), "'");
        assert_eq!(decode_entities("&#x4E2D;"), "中");
        assert_eq!(decode_entities("plain text"), "plain text");
        // 不认识的实体原样保留——不是丢掉
        assert_eq!(decode_entities("&weird;"), "&weird;");
        assert_eq!(decode_entities("100 & 200"), "100 & 200");
    }

    #[test]
    fn extract_readable_text_strips_scripts_and_tags() {
        let html = "<html><head><title> 测试页面 </title>\
             <style>body { color: red }</style></head>\
             <body><script>alert('x')</script>\
             <h1>标题</h1>\
             <p>第一段 &amp; &lt;符号&gt;</p>\
             <p>第二段</p></body></html>";
        let (title, text) = extract_readable_text(html);
        assert_eq!(title.as_deref(), Some("测试页面"));
        assert!(text.contains("标题"), "{text}");
        assert!(text.contains("第一段 & <符号>"), "{text}");
        assert!(text.contains("第二段"), "{text}");
        assert!(!text.contains("alert"), "script 内容必须被剔除：{text}");
        assert!(!text.contains("color: red"), "style 内容必须被剔除：{text}");
        // 原始标签剥干净；正文里解码出来的 &lt; &gt;（如 "<符号>"）是合法内容，不算残留
        assert!(
            !text.contains("<p>") && !text.contains("<h1>") && !text.contains("</"),
            "标签必须剥干净：{text}"
        );
    }

    #[test]
    fn extract_readable_text_handles_unclosed_script_without_panicking() {
        let (_, text) = extract_readable_text("<p>before</p><script>never closed");
        assert_eq!(text, "before");
    }

    /// 乱码根因的钉子：GBK 页面（4399 这一类的老站）按 UTF-8 解就是一屏替换字符。
    /// 三条路都要通——Content-Type 头里的 charset、HTML meta 声明、什么都没写时的
    /// GB18030 兜底（替换字符比 lossy 少才采信）
    #[test]
    fn gbk_pages_decode_by_declared_charset() {
        let (bytes, _, _) = encoding_rs::GBK.encode("4399小游戏，快乐齐分享！");
        let bytes = &*bytes;
        // ① Content-Type 头声明
        let decoded = decode_html_body(bytes, "text/html; charset=gb2312");
        assert!(decoded.contains("小游戏"), "{decoded}");
        // ② 只有 meta 声明
        let mut with_meta = b"<html><head><meta charset=\"gb2312\"></head><body>".to_vec();
        with_meta.extend_from_slice(bytes);
        with_meta.extend_from_slice(b"</body></html>");
        let decoded = decode_html_body(&with_meta, "text/html");
        assert!(decoded.contains("小游戏"), "{decoded}");
        // ③ 什么都没声明：GB18030 兜底赢过 lossy
        let bare = decode_html_body(bytes, "text/html");
        assert!(bare.contains("小游戏"), "{bare}");
        // 声明了 UTF-8 却真是 UTF-8 的页面不受影响（had_errors → 落回兜底/原样）
        let utf8 = "正常页面内容".as_bytes().to_vec();
        let decoded = decode_html_body(&utf8, "text/html; charset=utf-8");
        assert_eq!(decoded, "正常页面内容");
    }
}
