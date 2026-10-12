//! 压缩抖动闸（O12-3）：记「压完立刻又爆窗、再压再爆」的连续次数。
//!
//! 为什么需要它：整段压缩是这条链路上**最贵的一次调用**——它把整份历史喂给模型
//! 换一份摘要，还要作废热前缀缓存（下一发全价重写）。而"压完仍然超窗"是一种
//! 会自己咬住自己的状态：压 → 仍超 → 再压 → 仍超……每一轮都真金白银地花，
//! 用户看到的却只是"它一直在转"。既有护栏里只有模型输出的复读护栏
//! （[`crate::repetition::Guard`]），对这条烧钱路径一个计数都没有。
//!
//! 判据刻意做得极简：**连续**几次压完还超窗。中途只要有一次压完装得下就清零——
//! 那说明压缩本身是有效的，之前那次只是历史太长。
//!
//! 计数只活在进程内存里，不落盘：它是"这一轮到底怎么了"的现场读数，
//! 重启之后重新数起是对的（重启后窗口、模型、历史都可能是另一副样子）。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// 连续几次压完仍超窗就阻断。取 3：第一次可能只是历史比预期长，第二次值得再试
/// （摘要质量、保留长度都可能刚好差一点），第三次还这样就不是运气问题了
pub const THRASH_LIMIT: u32 = 3;

fn counters() -> &'static Mutex<HashMap<String, u32>> {
    static COUNTERS: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();
    COUNTERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn with<R>(conversation_id: &str, f: impl FnOnce(&mut u32) -> R) -> R {
    let mut map = counters()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // 话题删了、或者长得离谱的历史把表撑大：留个上限，超了整体清一次。
    // 这只是一份现场读数，清掉最坏结果是"重新数"，不会算错什么
    if map.len() > 512 {
        map.clear();
    }
    f(map.entry(conversation_id.to_string()).or_insert(0))
}

/// 压完**仍然**超窗：连续计数 +1，返回新的计数
pub fn note_still_over(conversation_id: &str) -> u32 {
    with(conversation_id, |count| {
        *count += 1;
        *count
    })
}

/// 压完装得下：压缩是有效的，连续计数清零
pub fn note_fits(conversation_id: &str) {
    with(conversation_id, |count| *count = 0);
}

/// 这一支是不是已经抖到该拦了。返回当前连续次数，调用方自己与阈值比
pub fn consecutive_overflows(conversation_id: &str) -> u32 {
    with(conversation_id, |count| *count)
}

/// 该不该拦下这一次压缩。拦下来的意思是**不再花钱去压**，而不是把整轮打回去：
/// 历史照原样发出去，爆窗口是服务商的事——但至少不再一轮一轮地烧摘要请求
pub fn thrashing(conversation_id: &str) -> bool {
    consecutive_overflows(conversation_id) >= THRASH_LIMIT
}

/// 拦下之后要不要再报一次。只在**刚跨过阈值那一次**报：之后每一轮重播同一句话
/// 就成了噪音，而用户已经从那一句里读到了"发生了什么、为什么、怎么办"——
/// 那句话进了话题日志，翻得到
pub fn announce_block_once(conversation_id: &str) -> bool {
    consecutive_overflows(conversation_id) == THRASH_LIMIT
}

/// 拦下来时给用户看的那句话。原因要写全：**发生了什么**（压了 N 次都没压下去）、
/// **为什么不再压**（每次都要真金白银）、**怎么办**（三个可操作的方向）
pub fn blocked_notice(conversation_id: &str) -> String {
    let times = consecutive_overflows(conversation_id);
    format!(
        "上下文压缩已连续 {times} 次没能把历史压进窗口，暂停自动压缩。\
         每次压缩都要把整份历史发一遍并作废缓存，再压下去只会继续花这笔钱而结果不变。\
         建议：调大「上下文窗口」、把「保留最近长度」调小，或开一条新话题继续。"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 连续计数：中途有一次装得下就清零，断掉的那次不该继续累加
    #[test]
    fn only_consecutive_overflows_count() {
        let id = "conv-thrash-probe";
        note_fits(id);
        assert_eq!(consecutive_overflows(id), 0);
        assert!(!thrashing(id));

        assert_eq!(note_still_over(id), 1);
        assert_eq!(note_still_over(id), 2);
        assert!(!thrashing(id), "两次还没到阈值");

        // 中间成功一次：清零
        note_fits(id);
        assert_eq!(consecutive_overflows(id), 0);
        assert_eq!(note_still_over(id), 1);

        // 连到阈值就该拦
        assert_eq!(note_still_over(id), 2);
        assert_eq!(note_still_over(id), 3);
        assert!(thrashing(id), "连续 3 次必须拦下");
        note_fits(id);
    }

    /// 拦下时那句话要把三件事说全：发生了什么、为什么不再压、怎么办
    #[test]
    fn blocked_notice_explains_what_why_and_what_next() {
        let id = "conv-thrash-notice";
        note_fits(id);
        for _ in 0..THRASH_LIMIT {
            note_still_over(id);
        }
        let text = blocked_notice(id);
        assert!(text.contains("连续 3 次"), "{text}");
        assert!(text.contains("暂停自动压缩"), "{text}");
        assert!(text.contains("上下文窗口"), "{text}");
        note_fits(id);
    }

    /// 拦下之后只在刚跨阈值那一次报，不每轮重播
    #[test]
    fn the_block_is_announced_once() {
        let id = "conv-thrash-announce";
        note_fits(id);
        for _ in 0..THRASH_LIMIT - 1 {
            note_still_over(id);
        }
        assert!(!announce_block_once(id), "还没到阈值，不该报");
        assert_eq!(note_still_over(id), THRASH_LIMIT);
        assert!(announce_block_once(id), "刚跨过阈值这一次要报");
        note_still_over(id);
        assert!(!announce_block_once(id), "再往后就闭嘴，别每轮重播");
        note_fits(id);
    }

    /// 话题之间互不干扰：A 抖了不该把 B 也拦下
    #[test]
    fn counters_are_per_conversation() {
        note_fits("conv-thrash-a");
        note_fits("conv-thrash-b");
        for _ in 0..THRASH_LIMIT {
            note_still_over("conv-thrash-a");
        }
        assert!(thrashing("conv-thrash-a"));
        assert!(!thrashing("conv-thrash-b"), "别人的账不该记到它头上");
        note_fits("conv-thrash-a");
    }
}
