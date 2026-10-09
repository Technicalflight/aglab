//! 系统通知（Windows toast）的三个落点：审批等待、定时任务收尾、目标停下。
//!
//! 门控住在这里，调用方只管"发生了什么"：
//! - 配置开关（`notifications`）关着不发——它管的是"要不要打扰"，默认开，
//!   因为无人值守的场景里不弹通知等于让人守着窗口等；
//! - 主窗口可见**且**在前台时不发——人正看着，应用内的提示就够了，
//!   弹到系统层面反而把人从正在做的事上拽走。

use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

pub fn notify(app: &AppHandle, title: &str, body: &str) {
    let config = crate::config::load(app);
    if !config.notifications {
        return;
    }
    if let Some(window) = app.get_webview_window("main") {
        if window.is_visible().unwrap_or(false) && window.is_focused().unwrap_or(false) {
            return;
        }
    }
    let _ = app.notification().builder().title(title).body(body).show();
}

/// 卡住的那句话给人看的长度：toast 是一眼的事，长理由用户会去界面上看
fn brief(text: &str) -> String {
    let one_line: String = text.chars().take(140).collect();
    if text.chars().count() > 140 {
        format!("{one_line}…")
    } else {
        one_line
    }
}

/// 审批在等人：有人在等才能继续的机器动作，界面上是一张卡片，
/// 窗口在后台时它就是一块看不见的暂停键
pub fn approval_needed(app: &AppHandle, display: &str) {
    notify(app, "aglab 在等你批准一个操作", &brief(display));
}

/// 无人值守的运行停在了审批队列：它不会自己继续，等人去 任务页 处理
pub fn unattended_parked(app: &AppHandle, display: &str) {
    notify(app, "定时任务停在审批：需要你处理", &brief(display));
}

/// 无人值守的提问挂起：ask_user 在无人值守的入口就该被拦住，
/// 走到这里说明是非任务的后台话题在问——同一条"有人等你"的通道
pub fn question_pending(app: &AppHandle, question: &str) {
    notify(app, "aglab 有一个问题等你回答", &brief(question));
}

/// 定时任务这一发收尾了。成败分开说， waiting 用任务自己的措辞
pub fn task_finished(app: &AppHandle, task_name: &str, status_label: &str, error: &str) {
    let title = match status_label {
        "succeeded" => format!("定时任务「{task_name}」跑完了"),
        "waitingApproval" => format!("定时任务「{task_name}」停在审批"),
        _ => format!("定时任务「{task_name}」失败了"),
    };
    let body = if error.is_empty() {
        "打开 aglab 看这一发的详情。".to_string()
    } else {
        brief(error)
    };
    notify(app, &title, &body);
}

/// 目标这一支到头了：complete 是做完，blocked 是卡住
pub fn goal_settled(app: &AppHandle, complete: bool, note: &str) {
    let title = if complete {
        "目标已报完".to_string()
    } else {
        "目标停住了".to_string()
    };
    notify(app, &title, &brief(note));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brief_keeps_one_line_within_a_glance() {
        assert_eq!(brief("短句"), "短句");
        let long: String = "啊".repeat(200);
        let cut = brief(&long);
        assert!(cut.chars().count() == 141, "140 字 + 省略号：{cut}");
        assert!(cut.ends_with('…'));
    }
}
