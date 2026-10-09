//! 界面发送路：[`chat_send`] 在门口分两岔——开着 Agent worker 开关且没挂目标的
//! 话题进子进程（[`spawn_worker_turn`]，回合本体经 turn.start 过河，实现住
//! `super::worker`），其余进内联线程 [`spawn_send_turn`]：跟随轮循环、池内换人、
//! goal 续跑全在这一条线程里。
//!
//! [`goal_profile_of`] 读目标点名执行的服务商档案（整份连接域照它走，池子与
//! 路由都不抢）；[`ChannelSink`] 是前端 Channel 的原始透传出口——worker 回程的
//! ev data 原样进 Channel，内联回合在 Rust 侧先序列化成同一个 Value 形状。

use std::thread;

use tauri::ipc::Channel;
use tauri::{AppHandle, Manager, State};

use super::mode_goal::{arm_goal_round, goal_block_on_turn_error, Step};
use super::wire::is_pool_swappable_error;
use super::worker::{spawn_worker_turn, worker_route_wanted};
use super::{
    commit_pending_mode, config, open_session, run_turn, stopped, with_connection, ApprovalHub,
    ChatEvent, EventSink, FollowUpHub, ModeHub, PauseHub, SteeringHub, StopHub, TurnHost,
};

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
pub(in crate::chat) fn spawn_send_turn(
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
    // ——判据本体在 [`worker_route_wanted`]，三态表在那边测
    let worker_route = {
        let config = config::load(&app);
        let session = open_session(&app, &conversation_id)?;
        worker_route_wanted(
            config.agent_worker_turns,
            crate::session::mode::in_effect(&session.log).goal_held(),
        )
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
