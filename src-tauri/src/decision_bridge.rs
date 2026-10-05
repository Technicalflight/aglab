//! 决策层住在 WebView（TS 那一侧的漏斗、缓存、审计都在那里），而编排派工与
//! 记忆注入排序跑在 Rust 线程上——两边隔着一道 IPC。这个桥就是"Rust 拿不准、
//! 问一嘴"的那条通道：
//!
//!   Rust 线程 `ask` → emit `decision://ask` → 前端桥听事件、跑决策层的嵌入函数
//!   → invoke [`decision_bridge_answer`] 把答案递回来 → 线程在 oneshot 通道上等答案。
//!
//! 桥的合同只有一条，与嵌入函数的 fail-open 同源：**没有应答（超时、前端没起、
//! 决策层关着、答案畸形）一律 None，调用方走原来的路**。桥的缺席不能变成
//! 功能的缺席——它只添信息，从不拦截。
//!
//! 为什么不走"Rust 直接问 Laya sidecar"：那是决策层的第二条通道，漏斗、
//! 阈值、审计全被绕开（同一个问题两处问，迟早各说各话）。多一跳 IPC 换
//! 单一真相，是这里最便宜的买法。

use std::collections::HashMap;
use std::sync::mpsc::{channel, Sender};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
use tauri::{AppHandle, Emitter};

/// 待答的请求表：id → 递答案的一次性通道。前端那一发 [`decision_bridge_answer`]
/// 按 id 找到它，把答案（或 null = 决策层说 no）塞进来
fn pending() -> &'static std::sync::Mutex<HashMap<String, Sender<Option<Value>>>> {
    static PENDING: std::sync::OnceLock<std::sync::Mutex<HashMap<String, Sender<Option<Value>>>>> =
        std::sync::OnceLock::new();
    PENDING.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// 请求序号：id 要在同进程内不重不撞
fn seq() -> &'static AtomicU64 {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    &SEQ
}

/// 问一嘴。阻塞调用线程直到应答或超时——只许在后台线程问，
/// 主线程上等一个 WebView 的回话是自找卡顿。None = 没答上来，调用方照旧
pub fn ask(app: &AppHandle, method: &str, payload: Value, timeout_ms: u64) -> Option<Value> {
    let id = format!("bridge-{}-{}", seq().fetch_add(1, Ordering::Relaxed), crate::session::now_millis());
    let (tx, rx) = channel::<Option<Value>>();
    if pending().lock().expect("决策桥待答表锁").insert(id.clone(), tx).is_some() {
        // id 撞了（不可能，但锁表的手滑要看得见）：不覆盖别人的请求
        return None;
    }
    let sent = app.emit(
        "decision://ask",
        serde_json::json!({ "id": id, "method": method, "payload": payload }),
    );
    if let Err(why) = sent {
        pending().lock().expect("决策桥待答表锁").remove(&id);
        eprintln!("决策桥没能把问题递出去（{method}）：{why}");
        return None;
    }
    match rx.recv_timeout(Duration::from_millis(timeout_ms)) {
        Ok(answer) => answer,
        Err(why) => {
            pending().lock().expect("决策桥待答表锁").remove(&id);
            eprintln!("决策桥没有等到应答（{method}）：{why}");
            None
        }
    }
}

/// 前端的应答入口。晚到的答案（等待方已经超时走了）在这里自然蒸发：
/// 通道的对端被丢进待答表移除时一并释放，send 落空是无害的常态
#[tauri::command]
pub fn decision_bridge_answer(id: String, answer: Option<Value>) {
    let Some(tx) = pending().lock().expect("决策桥待答表锁").remove(&id) else {
        return;
    };
    let _ = tx.send(answer);
}

/// 解析前端递回来的相关性答案：null、形状不对、长度对不上、有非数——
/// 任何一样都不够格重排，返回 None 让调用方照原序走
pub fn parse_scores(answer: Option<&Value>, expected: usize) -> Option<Vec<f64>> {
    let scores = answer?.get("scores")?;
    let scores: Vec<f64> = serde_json::from_value(scores.clone()).ok()?;
    if scores.len() != expected || scores.iter().any(|value| !value.is_finite()) {
        return None;
    }
    Some(scores)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_answer_without_scores_or_with_a_mismatch_is_not_worth_ranking() {
        assert!(parse_scores(None, 3).is_none());
        assert!(parse_scores(Some(&Value::Null), 3).is_none());
        assert!(parse_scores(Some(&json!({})), 3).is_none());
        assert!(parse_scores(Some(&json!({ "scores": [1.0, 2.0] })), 3).is_none());
        assert!(parse_scores(Some(&json!({ "scores": [1.0, "x", 3.0] })), 3).is_none());
        assert!(parse_scores(Some(&json!({ "scores": [1.0, 2.0, "x"] })), 3).is_none());
    }

    #[test]
    fn a_well_shaped_answer_yields_its_scores_in_order() {
        let scores = parse_scores(Some(&json!({ "scores": [5.0, 0.5, 2.5] })), 3);
        assert_eq!(scores, Some(vec![5.0, 0.5, 2.5]));
    }

    #[test]
    fn late_answers_evaporate_without_poisoning_the_table() {
        // 没人问就答：待答表里没有这个 id，命令面不该炸，也不该留下半条记录
        decision_bridge_answer("nobody".into(), Some(json!({ "scores": [1.0] })));
        assert!(pending().lock().expect("决策桥待答表锁").is_empty());
    }
}
