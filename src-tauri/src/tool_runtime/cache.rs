//! 话题进程内的幂等读缓存。
//!
//! 它只管一件事：**同一份内容在同一次进程活着的时间里不必读第二次**。所以它的门闩只有两条，
//! 两条都过才配有一个键——
//! 1. 这个工具说自己幂等（`ToolSource::idempotent`）；
//! 2. 拿得出一个内容指纹（`ToolSource::stamp`，实测是"解析后的绝对路径 + mtime + 字节数"）。
//!
//! 第二条不是装饰：没有指纹的缓存会在文件被改过之后继续交旧内容，而模型看不出来它读的是
//! 哪一版。拿不出指纹的东西（比如扩展工具，它的语义我们并不知道）就干脆不进缓存。
//! 写类工具连键都不该被构造出来，`EntryKey::of` 对它是 `None`——这条有测试直接断言。

use std::collections::HashMap;
use std::sync::Mutex;

/// 一个缓存键。`stamp` 里已经含解析后的绝对路径，所以"同一个 path 参数在两个项目根下"
/// 天然是两个键，不需要再单列 cwd
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct EntryKey {
    pub tool: String,
    pub args: String,
    pub stamp: String,
}

impl EntryKey {
    /// 两条门闩都过才有键。任一不成立返回 `None`，调用方就该走"老老实实读一遍"那条路
    pub fn of(tool: &str, args: &str, idempotent: bool, stamp: Option<String>) -> Option<Self> {
        if !idempotent {
            return None;
        }
        Some(Self { tool: tool.into(), args: args.into(), stamp: stamp? })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    pub hits: usize,
    pub misses: usize,
    pub entries: usize,
    /// 自动重试一共发生过几次。它与命中数一样是"这一轮为什么是这样"的一部分，
    /// 不该只活在返回给调用方的那个结构体里
    pub retries: usize,
}

/// 一张表加三个读数。读数不是给监控看的：Inspector 上要回答"这次为什么没重读盘"，
/// 它只能从这张表自己说清楚
#[derive(Default)]
pub struct ReadCache {
    entries: Mutex<HashMap<EntryKey, String>>,
    stats: Mutex<Stats>,
}

impl ReadCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, key: &EntryKey) -> Option<String> {
        let found = self.entries().get(key).cloned();
        let mut held = self.counters();
        // 命中与未中是两笔账，都得记：Inspector 上要回答"这次为什么没重读盘"
        if found.is_some() {
            held.hits += 1;
        } else {
            held.misses += 1;
        }
        found
    }

    pub fn put(&self, key: EntryKey, text: &str) {
        let held = {
            let mut table = self.entries();
            table.insert(key, text.to_string());
            table.len()
        };
        // 条目数以表为准，不另加一个计数器：两个真相早晚会对不上
        self.counters().entries = held;
    }

    /// 进程内清一次。撤销、关掉话题都可以调；它不删任何磁盘上的东西。
    /// 今天只有测试在调（生产的失效走逐条 `forget`，不清表）
    #[cfg(test)]
    pub fn clear(&self) -> usize {
        let removed = {
            let mut table = self.entries();
            let count = table.len();
            table.clear();
            count
        };
        self.counters().entries = 0;
        removed
    }

    /// 记一次自动重试。由 [`crate::tool_runtime::source::run_with`] 在真重试的那一刻调，
    /// 不是事后估算
    pub fn record_retry(&self) {
        self.counters().retries += 1;
    }

    /// 给读的一侧（Inspector、界面）看的那三个数
    pub fn stats(&self) -> Stats {
        *self.counters()
    }

    fn counters(&self) -> std::sync::MutexGuard<'_, Stats> {
        self.stats.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<EntryKey, String>> {
        self.entries.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_write_tool_never_even_gets_a_key() {
        // T06 的判据：`write_file` 永不出现在缓存键里。这里断言的是键都造不出来，
        // 而不是"造出来了但没人查"
        assert_eq!(
            EntryKey::of("write_file", "{\"path\":\"a\"}", false, Some("a@1".into())),
            None,
            "写类工具不进缓存，一次都不许"
        );
        // 幂等但拿不出指纹，同样不进缓存：没有指纹就是拿旧内容骗模型
        assert_eq!(EntryKey::of("load_skill", "{\"name\":\"x\"}", true, None), None);
        assert!(EntryKey::of("read_file", "{\"path\":\"a\"}", true, Some("a@1".into())).is_some());
    }

    #[test]
    fn a_second_read_of_the_same_stamp_is_a_hit_and_a_new_stamp_is_not() {
        let cache = ReadCache::new();
        let first = EntryKey::of("read_file", "{\"path\":\"a\"}", true, Some("a@mtime1".into()))
            .expect("这条该有键");
        assert_eq!(cache.get(&first), None, "第一次是未命中");
        cache.put(first.clone(), "正文第一版");
        assert_eq!(cache.get(&first).as_deref(), Some("正文第一版"));

        let edited =
            EntryKey::of("read_file", "{\"path\":\"a\"}", true, Some("a@mtime2".into()))
                .expect("这条也该有键");
        assert_eq!(cache.get(&edited), None, "指纹变了就不是同一份内容");

        let stats = cache.stats();
        assert_eq!((stats.hits, stats.misses, stats.entries), (1, 2, 1), "读数要能报出这三次");
    }

    #[test]
    fn clearing_the_cache_removes_every_entry_and_says_how_many() {
        let cache = ReadCache::new();
        for path in ["a", "b"] {
            let key = EntryKey::of("read_file", path, true, Some(format!("{path}@1"))).unwrap();
            cache.put(key, "x");
        }
        assert_eq!(cache.stats().entries, 2);
        assert_eq!(cache.clear(), 2, "清掉的条数要说出来，不然界面没法说'缓存空了'");
        assert_eq!(cache.stats().entries, 0);
    }
}
