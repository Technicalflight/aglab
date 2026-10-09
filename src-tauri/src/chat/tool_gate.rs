//! 工具闸链的辅助件（优化路线 O1-4 从 chat.rs 拆出）。
//!
//! 职责：工具结果出站前的敏感打码（[`mask_tool_input`]）、钩子卡片
//! （[`emit_hooks`]）、无人值守审批的挂起（[`park_unattended`]）、
//! 审计 sink（[`audit_tool`] / [`audit_tool_in`]）、规则标签（[`short_label`]）、
//! 参数解析（[`parse_arguments`]）。
//!
//! 闸链主判定（capability → 权限表 → 钩子 → 审批）仍在 [`super::turn_body`]
//! 的循环体内：它一眼要看的现场变量太多，硬拆等于把现场装箱搬运。

use super::{tool_runtime, tools, ChatEvent, EventSink, ToolStatus, TurnHost};
use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

/// 前端历史 → 服务商消息。收形发生在这一层，所以"重放路径"本身是可断言的，
/// 而不只是辅助函数正确、接入点却可能没调用它。
/// 剥掉每轮由后端重加的常驻段（默认提示词 + 带标记的卡片），只留下真正的历史。
/// 影子核对要先对齐口径：回放里本来就没有这些，留着比一定会报假漂移
/// 工具入参的打码出口：策略指纹、待审批队列、审批界面三处读的都是这一串。
/// **打码只住这一个函数**——执行用的仍是模型给的原始参数，打码不许改变它做什么
pub(in crate::chat) fn mask_tool_input(via_mcp: bool, name: &str, args: &Value) -> String {
    crate::secrets::mask_secrets(&if via_mcp {
        format!("扩展调用 {name}")
    } else {
        tools::summary(name, args)
    })
}

/// 钩子说过话就给它一张卡片：拦下了什么、补了什么、或者自己崩了。
/// 没意见的钩子不占界面，否则每次工具调用都要多出一排空卡片
pub(in crate::chat) fn emit_hooks(on_event: &dyn EventSink, report: &crate::hooks::Report) {
    for (hook, outcome) in &report.notes {
        let (status, text) = match outcome {
            crate::hooks::Outcome::Silent => continue,
            crate::hooks::Outcome::Block(reason) => (ToolStatus::Denied, reason.clone()),
            // ask 的落地在调用方（把这一次拉回审批）；钩子卡片上只说一句它的意图
            crate::hooks::Outcome::Ask(reason) => (ToolStatus::Pending, reason.clone()),
            // 放行也是一句真话：卡片上写明是谁替你点的头，调用方同时在
            // pass_reason 里落同一句——卡片与"刚刚是谁放的"永远对得上
            crate::hooks::Outcome::Approve(reason) => (ToolStatus::Done, reason.clone()),
            crate::hooks::Outcome::AddContext(reason) => (ToolStatus::Done, reason.clone()),
            crate::hooks::Outcome::Broken(detail) => {
                (ToolStatus::Failed, format!("钩子没跑成：{detail}"))
            }
        };

        on_event.send(ChatEvent::Tool {
            id: format!("hook-{}", hook.id),
            name: hook.card_name().to_string(),
            status,
            risk: tools::Risk::High.as_str().into(),
            input: hook.card_input(),
            output: Some(text),
            arguments: None,
            // 钩子那张卡片不是审批闸门的产物：它拦下或补话，都不涉及"该问而没问"
            pass_reason: None,
            content_chars: None,
        });
    }
}

/// 工具调用进统一审计 sink（`<app_data_dir>/audit/audit-<日期>.jsonl`）。
/// 只记动作与标识：正文里可能有口令，而审计不是第二份对话记录
/// 无人值守的回合撞到一个"要点头"的动作之后的下场。分成三档而不是两档，是因为
/// "队列里有人替这一份指纹点过头"与"没人可问所以挂起"必须走不同的路
pub(in crate::chat) enum Escalated {
    /// 这一发放行（先前有人为同一条 capability + 同一份指纹表过态）
    Run,
    /// 交给即时审批：要么本来就在有人看的话题里，要么登记表刚刚才消失
    Prompt,
    /// 不动手。`outcome` 是这一发在审计里的口径，`reason` 是说给模型的那句话
    Halted {
        outcome: crate::audit::Outcome,
        reason: String,
    },
}

/// 把这一发动作挂到 durable 待审批队列。判定不在这里重复一遍：权限表已经说过 `Ask`，
/// 这里只回答"没有能点头的人，那就停在检查点"。队列自己那行 `task:escalate` 审计由
/// `escalate::park_for_turn` 落，这里补的是"这次工具调用停在哪儿"那一行
pub(in crate::chat) fn park_unattended(
    host: &TurnHost,
    conversation_id: &str,
    ruling: &tool_runtime::Ruling,
    display: &str,
) -> Escalated {
    use crate::tasks::escalate::Gate;
    let root = host.data_dir.clone();
    match crate::tasks::escalate::park_for_turn(
        &root,
        conversation_id,
        &ruling.key,
        display,
        &ruling.decision,
        crate::session::now_millis(),
    ) {
        // 队列读不动时不能"当作没有待审批"——那一发写坏的 JSON 就把闸门解除了
        Err(problem) => Escalated::Halted {
            outcome: crate::audit::Outcome::Blocked,
            reason: problem,
        },
        Ok(None) => Escalated::Prompt,
        Ok(Some(Gate::Execute)) => Escalated::Run,
        Ok(Some(Gate::Parked(item))) => {
            // 挂进队列的下一步是"等人"：窗口在后台时没人知道它停了，系统通知喊一声
            host.toast_unattended_parked(display);
            Escalated::Halted {
                outcome: crate::audit::Outcome::Blocked,
                reason: format!(
                    "这一步要人点头，已经挂成待审批（{}）。本轮没有执行它，请等人处理后再跑。",
                    item.capability
                ),
            }
        }
        Ok(Some(Gate::Refused { reason })) => Escalated::Halted {
            outcome: crate::audit::Outcome::Denied,
            reason,
        },
    }
}

/// 放行规则上那行可读标签：确认框当初给用户看的是哪句话，撤销列表里就还是哪句话。
/// 压成一行并截断——一条规则不该把整份文件正文搬进设置页
pub(in crate::chat) fn short_label(text: &str) -> String {
    let one_line = text
        .char_indices()
        .map(|(_, ch)| if ch == '\n' || ch == '\r' { ' ' } else { ch })
        .collect::<String>();
    let mut label: String = one_line.chars().take(120).collect();
    if one_line.chars().count() > 120 {
        label.push('…');
    }
    label.trim().to_string()
}

#[allow(dead_code)] // Main 侧封装：worker 直用 _in 变体；M5 chat.rs 拆空时统一清算
pub(in crate::chat) fn audit_tool(
    app: &AppHandle,
    conversation_id: &str,
    call: &tool_runtime::Call,
    outcome: crate::audit::Outcome,
    pass_reason: Option<&str>,
) -> Result<(), String> {
    audit_tool_in(
        &app.path().app_data_dir().map_err(|e| e.to_string())?,
        conversation_id,
        call,
        outcome,
        pass_reason,
    )
}

pub(in crate::chat) fn audit_tool_in(
    data_dir: &std::path::Path,
    conversation_id: &str,
    call: &tool_runtime::Call,
    outcome: crate::audit::Outcome,
    pass_reason: Option<&str>,
) -> Result<(), String> {
    let root = data_dir.to_path_buf();
    crate::audit::record_detail(
        &root,
        // 以前这一格写死 `Actor::Model`，于是编排器/定时任务引起的那一发写文件，
        // 在账上与"用户在聊天里让模型动的"长得一模一样
        crate::tasks::escalate::audit_actor(conversation_id),
        &format!("tool:{}", call.name),
        &tool_runtime::audit_target(call),
        outcome,
        // 卡片上那句话是此刻的，账上这一行是重启之后唯一还能问出"这一发有没有人
        // 点头"的地方。文案与卡片同源（同一个 `pass_reason`），不另写一遍
        pass_reason.map(|reason| format!("这一发没有再问：{reason}")),
    )
}

/// 工具参数解析。空参数按 `{}`（部分服务商回空串），但格式坏了必须报出来——
/// 静默用空参数执行等于对着猜的意图动文件
pub(in crate::chat) fn parse_arguments(raw: &str) -> Result<Value, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(trimmed).map_err(|error| format!("工具参数不是合法 JSON：{error}"))
}
