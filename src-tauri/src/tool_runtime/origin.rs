//! 运行类型与它的禁用表（O17-3）。
//!
//! `Scheduled` 是无人值守的那一发（定时任务触发的回合）：没有人在场，所以有两类
//! 动作它不该做——改任务表（自己改自己的说明书，下一轮的语义就变了，而且没人看见）
//! 与派子助理（后台繁殖后台，没有一个回合的产出能为另一个回合的花费负责）。
//! 编排节点不在此列：spawn 是编排图的一部分，由图与预算管着，不算"自我派生"。

use std::collections::HashMap;

use super::ACTIVE_CAP;

/// 这一轮**从哪来**。只有定时任务那条路会登记 `Scheduled`：其余一切
/// （界面、编排节点、子助理）都不登记，读出来就是默认的 `Interactive`——
/// 禁用表按"多出来的那一档"收，不按"每个调用点都要记得表态"收，
/// 漏登记的最坏结果是回到今天的形状
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOrigin {
    Interactive,
    Scheduled,
}

static ORIGINS: std::sync::OnceLock<std::sync::Mutex<HashMap<String, TurnOrigin>>> =
    std::sync::OnceLock::new();

fn origins() -> &'static std::sync::Mutex<HashMap<String, TurnOrigin>> {
    ORIGINS.get_or_init(Default::default)
}

/// 登记这一发的来源
pub(crate) fn note_origin(conversation_id: &str, origin: TurnOrigin) {
    let mut map = origins()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if map.len() >= ACTIVE_CAP && !map.contains_key(conversation_id) {
        map.clear();
    }
    map.insert(conversation_id.to_string(), origin);
}

/// 这一轮的来源。没登记过 = 交互轮（默认那一侧永远是放行的那一侧）
pub(crate) fn origin_of(conversation_id: &str) -> TurnOrigin {
    origins()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(conversation_id)
        .copied()
        .unwrap_or(TurnOrigin::Interactive)
}

/// 话题没了，"它是无人值守"这件事也没有继续存在的理由
pub(crate) fn clear_origin(conversation_id: &str) {
    origins()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(conversation_id);
}

/// 按运行类型的禁用表。命中返回给模型看的那句拒绝理由。
/// 只禁内置名：MCP 工具的命名空间不归这张表管，它有自己的服务器开关
pub(crate) fn origin_violation(origin: TurnOrigin, name: &str) -> Option<String> {
    if origin != TurnOrigin::Scheduled {
        return None;
    }
    match name {
        "cron_create" | "cron_delete" | "task_run_now" => Some(
            "无人值守的回合不许改任务表或触发同族任务：那等于自己改自己的说明书，\
             而且没有人在场看见。要调整任务请在应用的任务页里动手。"
                .into(),
        ),
        "spawn_subagent" => Some(
            "无人值守的回合不许再派子助理：后台繁殖后台，没有一个回合的产出\
             能为另一个回合的花费负责。把要派的事写进结论，由人决定派不派。"
                .into(),
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{Decision, Mode, Policy};
    use serde_json::json;

    /// 按运行类型的禁用表——四件"自己管自己"的工具在无人值守的轮次里
    /// 全拒且说得出为什么；同一件在交互轮照常（禁的是"没人看着"，不是工具本身）；
    /// MCP 命名空间不归这张表管
    #[test]
    fn the_scheduled_origin_refuses_its_own_levers() {
        for lever in [
            "cron_create",
            "cron_delete",
            "task_run_now",
            "spawn_subagent",
        ] {
            assert!(
                origin_violation(TurnOrigin::Scheduled, lever).is_some(),
                "{lever} 在无人值守的轮次里该被拒"
            );
            assert!(
                origin_violation(TurnOrigin::Interactive, lever).is_none(),
                "{lever} 在交互轮照常"
            );
        }
        assert!(origin_violation(TurnOrigin::Scheduled, "read_file").is_none());
        assert!(
            origin_violation(TurnOrigin::Scheduled, "mcp__fs__read").is_none(),
            "MCP 命名空间不归这张表管，它有自己的服务器开关"
        );

        // 闸：Scheduled 轮里 rule 对 cron_create 直接 Deny，同一个调用在交互轮不因这张表被拒
        let args = json!({ "name": "x", "cron_expr": "* * * * *" });
        let call = super::super::Call::new("cron_create", &args, None, false);
        let scheduled = super::super::rule(
            &Policy::new(Mode::Full),
            &call,
            "x",
            None,
            TurnOrigin::Scheduled,
        );
        assert!(
            matches!(scheduled.decision, Decision::Deny { .. }),
            "无人值守的轮次不许建任务：{:?}",
            scheduled.decision
        );
        let interactive = super::super::rule(
            &Policy::new(Mode::Full),
            &call,
            "x",
            None,
            TurnOrigin::Interactive,
        );
        assert!(
            !matches!(interactive.decision, Decision::Deny { .. }),
            "交互轮不因这张表被拒：{:?}",
            interactive.decision
        );
    }

    /// 来源表的生命周期与白名单同款：没登记 = 交互轮（默认那一侧是放行的那侧），
    /// 登记与随话题清理各归各位
    #[test]
    fn the_origin_table_follows_the_conversation_scope() {
        let conversation = "conv-origin-o17-3";
        clear_origin(conversation);
        assert_eq!(
            origin_of(conversation),
            TurnOrigin::Interactive,
            "没登记 = 交互轮"
        );
        note_origin(conversation, TurnOrigin::Scheduled);
        assert_eq!(origin_of(conversation), TurnOrigin::Scheduled);
        clear_origin(conversation);
        assert_eq!(
            origin_of(conversation),
            TurnOrigin::Interactive,
            "清了要回到默认，残留会把下一发当成无人值守"
        );
    }

    /// 「库层成立≠链路成立」：定时任务的两处回合入口（单格与多格任务图）都要在
    /// 发起回合**之前**登记来源——登记晚了，声明就带着禁用工具发出去了
    #[test]
    fn the_task_turn_entries_note_their_origin() {
        let source = include_str!("../tasks/mod.rs").replace('\r', "");
        let entries: Vec<usize> = source
            .match_indices("chat::run_background_turn(")
            .map(|(at, _)| at)
            .collect();
        assert!(
            entries.len() >= 2,
            "定时任务有两条发起回合的路，现在只找到 {}",
            entries.len()
        );
        for at in entries {
            let before = &source[..at];
            let noted = before
                .rfind("note_origin(")
                .unwrap_or_else(|| panic!("回合入口（字节 {at}）之前要有来源登记"));
            let window = &before[noted..];
            assert!(
                !window.contains("\n}\n"),
                "note_origin 与 run_background_turn 之间跨了函数边界：登记没跟上这一发"
            );
            assert!(
                before[noted..].contains("Scheduled"),
                "登记的得是 Scheduled"
            );
        }
    }
}
