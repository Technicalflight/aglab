//! 观察存档（SoL-Pi 的 ObservationPack 精神）：超长工具结果的原文按调用 id 归档，
//! 发给服务商的只是"首尾摘录 + 句柄"，模型要用中间那段时调 obs_recall 分页取回。
//!
//! 存档住线程本地：工具结果产生与 obs_recall 执行都在同一条话题线程里，
//! 不需要跨线程——话题线程收摊，存档跟着消失，句柄本来就是话题内的临时引用。

use std::cell::RefCell;
use std::collections::VecDeque;

/// 存档容量。句柄是话题内的临时引用，最旧的被挤出时 obs_recall 会如实说不在了
const CAP: usize = 64;

thread_local! {
    static STORE: RefCell<VecDeque<(String, String)>> = const { RefCell::new(VecDeque::new()) };
}

/// 把一条工具结果的原文归档。同 id 重来（工具重试）就覆盖旧的那份
pub fn archive(id: &str, text: &str) {
    STORE.with(|store| {
        let mut store = store.borrow_mut();
        store.retain(|(key, _)| key != id);
        while store.len() >= CAP {
            store.pop_front();
        }
        store.push_back((id.to_string(), text.to_string()));
    });
}

/// 按句柄取回原文的一个字符区间（start 起、limit 长，按字符计）。取不到就如实说
pub fn recall(id: &str, start: usize, limit: usize) -> Result<String, String> {
    STORE.with(|store| {
        let store = store.borrow();
        let Some((_, text)) = store.iter().find(|(key, _)| key == id) else {
            return Err(format!(
                "观察 #{id} 不在存档里：要么已被更新的观察挤出容量，要么话题重启过。"
            ));
        };
        let total = text.chars().count();
        let start = start.min(total);
        let end = start.saturating_add(limit).min(total);
        let body: String = text.chars().skip(start).take(end - start).collect();
        Ok(format!(
            "…[{start}..{end}] / 全文共 {total} 字符…\n{body}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_and_recall_round_trip() {
        archive("#t1", "hello 世界");
        let first = recall("#t1", 0, 5).unwrap();
        assert!(first.contains("hello"), "{first}");
        // 字符口径：中文一个字算一个
        assert!(recall("#t1", 6, 2).unwrap().contains("世界"));
        assert!(recall("#t2", 0, 4).is_err(), "没归档过的句柄如实报不在");
    }

    #[test]
    fn capacity_evicts_the_oldest() {
        for i in 0..70 {
            archive(&format!("#old{i}"), "x");
        }
        assert!(recall("#old0", 0, 1).is_err(), "最旧的被挤出");
        archive("#fresh", "kept");
        assert!(recall("#fresh", 0, 4).is_ok());
    }
}
