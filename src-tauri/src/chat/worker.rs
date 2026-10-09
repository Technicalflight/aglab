//! Agent worker 路径（M3 收官）：回合跑进子进程的那一条。
//!
//! Main 侧只保留三样：[`WorkerTurnParams`]（turn.start 的 serde 入参）、
//! [`worker_turns`] 登记表（话题 → 回合在子进程里跑，审批/插话/停止三条命令
//! 的路由键）、[`spawn_worker_turn`]（分流的发送线程：登记、起停、跟随队列
//! 的排程与收尾都留在 Main，回合本体经 turn.start 进子进程）。
//!
//! 分流判据 [`worker_route_wanted`]：开关开了且话题没挂目标才进子进程——
//! goal 轮与跟随队列的消费者全是 Main 进程状态，明确留 Main（O2-3 决议）。
//! worker 侧的重活三件在 [`crate::chat::chat_heavy_tools`] 定去留（O2-1 决议：
//! 全部诚实拒绝，运行态是 Main 进程单例）。

use std::thread;

use tauri::ipc::Channel;
use tauri::{AppHandle, Manager};

use super::{stopped, ChatEvent, FollowUpHub, PauseHub, SteeringHub, StopHub};

/// Main 侧的 worker 回合登记表（M3 收官）：话题 → 回合在子进程里跑。
/// 审批决议（tool_decision）、插话（chat_steer）、停止（chat_abort）靠它分流；
/// 值里的 () 只是键位——路由细节由监督者的 pending 表与请求 id 管
static WORKER_TURNS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::OnceLock::new();

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

pub(in crate::chat) fn worker_turns() -> &'static std::sync::Mutex<std::collections::HashSet<String>>
{
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

/// 分流判据（蓝图 §A7 ④）：开关开了、话题又没挂目标，这一发才进子进程。
/// 挂目标的话题永远留在内联路——goal 续跑循环与它的寄存（暂停/切档/跟随）
/// 全是 Main 进程的状态，worker 只跑"与目标无关的独立一发"。
pub(in crate::chat) fn worker_route_wanted(agent_worker_turns: bool, goal_held: bool) -> bool {
    agent_worker_turns && !goal_held
}

/// worker 分流的发送线程（M3 收官）：Main 侧的回合管理（停止/插话/跟随的登记
/// 与收尾）留在这边，回合本体经 turn.start 进子进程跑真 run_turn。
/// 跟随队列在本线程里继续排队开下一发——排队的话依然作为正常新输入跑
#[allow(clippy::too_many_arguments)]
pub(in crate::chat) fn spawn_worker_turn(
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
