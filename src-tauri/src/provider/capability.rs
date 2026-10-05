//! 模型能力表：一条线能带什么缓存旋钮，只有声明过才算支持。
//!
//! 为什么需要它：以前"这个服务商认不认 `prompt_cache_key`"是靠 base_url 里有没有
//! "deepseek" 猜的，保温的 TTL 与门槛则是两个写死的常数（300 秒 / 2048 token），
//! 谁都说不清依据。能力表把"不知道"变成可表达的状态：
//!
//! - 缺证据即关闭（`prompt_cache_key: false`、`cache_ttl_seconds: 0`），而不是猜一个；
//! - `cache_ttl_seconds == 0` 表示**存活期未知**，保温据此拒绝运行——花真钱赌命中
//!   的前提是知道赌约的期限；
//! - 用户可以逐条覆盖，覆盖之后仍然不猜：写进来的值就是他给的证据。
//!
//! 存活期（TTL）按三级来源解析，越具体的证据越优先：
//! 1. 配置里 `cacheTtlByModel` 的逐模型条目——用户指着这个型号给的数；
//! 2. 全局 `cacheTtlSeconds` 覆盖；
//! 3. 内置表 [`builtin_model_ttl_seconds`]——只收有官方文档依据的型号档位。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// OpenAI 对 `prompt_cache_key` 的长度上限，超了要截而不是原样发
pub const CACHE_KEY_LIMIT: usize = 64;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Capability {
    /// 能否带显式缓存身份（chat 线的 `prompt_cache_key`）
    pub prompt_cache_key: bool,
    /// 缓存存活期（秒）。0 = 未知，保温不许跑
    pub cache_ttl_seconds: u32,
}

impl Capability {
    /// 保温可用的前提：存活期必须已知
    pub fn warmable(&self) -> bool {
        self.cache_ttl_seconds > 0
    }
}

/// 话题 id 变成缓存身份。截断而不是拒绝：id 的形状由界面决定，
/// 但同一话题必须永远得到同一个 key，否则缓存身份自己就成了漂移源
pub fn cache_key(conversation_id: &str) -> String {
    conversation_id.chars().take(CACHE_KEY_LIMIT).collect()
}

/// 内置 TTL 表。只收**有官方文档依据**的型号档位，其余留 0（未知）。
/// 档位一律按各家最短那档算：保温续的是最短命的那条缓存，
/// 把 1 小时档当 5 分钟用只是多续几次，把 5 分钟当 1 小时用就是在赌
fn builtin_model_ttl_seconds(model: &str, base_url: &str) -> u32 {
    let url = base_url.to_ascii_lowercase();
    let name = model.to_ascii_lowercase();
    if url.contains("api.anthropic.com") || name.starts_with("claude") {
        // Anthropic：ephemeral 断点默认 5 分钟；1 小时档要显式带 ttl:"1h" 才有
        return 300;
    }
    if url.contains("api.openai.com") {
        // OpenAI：官方口径前缀缓存 5 分钟到 1 小时不等，按最短档算
        return 300;
    }
    if name.starts_with("deepseek") || url.contains("deepseek") {
        // 官方没给存活期 → 未知，保温因此不会跑
        return 0;
    }
    0
}

/// 内置条目。只收**有官方文档或实测依据**的，其余走 default（全关）。
/// 中转站的性质不在这里加：同一条 URL 背后的上游可能换，测出来的结论不持久而非持久
fn builtin(model: &str, base_url: &str) -> Capability {
    let url = base_url.to_ascii_lowercase();
    let name = model.to_ascii_lowercase();
    let cache_ttl_seconds = builtin_model_ttl_seconds(model, base_url);
    if url.contains("api.openai.com") {
        // OpenAI 的自动前缀缓存 + prompt_cache_key 是官方语义
        return Capability {
            prompt_cache_key: true,
            cache_ttl_seconds,
        };
    }
    if name.starts_with("claude") || url.contains("api.anthropic.com") {
        // Anthropic 用 cache_control 断点声明缓存，没有 prompt_cache_key 这个参数；
        // 存活期来自 ephemeral 的 5 分钟默认档
        return Capability {
            prompt_cache_key: false,
            cache_ttl_seconds,
        };
    }
    if name.starts_with("deepseek") || url.contains("deepseek") {
        // 官方文档：KV 缓存全自动、无请求参数、无显式断点可打 → 没有身份可带；
        // 存活期官方没给数 → 留 0（未知），保温因此不会跑
        return Capability {
            prompt_cache_key: false,
            cache_ttl_seconds: 0,
        };
    }
    Capability::default()
}

/// 解析一条话题实际生效的能力。用户在配置里写了值就以它为准（含"写 false 关掉内置的 true"）；
/// TTL 三级来源里逐模型条目最具体，压过全局覆盖，全局覆盖压过内置表
pub fn resolve(
    model: &str,
    base_url: &str,
    key_override: Option<bool>,
    ttl_override: Option<u32>,
    ttl_by_model: &BTreeMap<String, u32>,
) -> Capability {
    let mut resolved = builtin(model, base_url);
    if let Some(on) = key_override {
        resolved.prompt_cache_key = on;
    }
    resolved.cache_ttl_seconds = ttl_by_model
        .get(model)
        .copied()
        .or(ttl_override)
        .unwrap_or(resolved.cache_ttl_seconds);
    resolved
}

/// 这笔保温重放安不安全。
///
/// Anthropic 的预算式思考（非 adaptive thinking 的 Claude 型号）从 `max_tokens` 推出
/// `budget_tokens`，而消息缓存把预算一起当键——`max_tokens=1` 的重放会拿到一个
/// 不同的预算，缓存键对不上，等于白付一次全价输入还落不着命中。
/// 所以"线上会发思考参数"的配置一律不许重放。aglab 的 anthropic 线不发送思考
/// 参数（`reasoning_effort` 只属于 OpenAI 两条线），此时重放安全；将来那条线
/// 接上 thinking 时，这道闸就拦在花冤枉钱之前
pub fn replayable(api_format: &str, reasoning_effort: &str) -> bool {
    !(api_format == "anthropic" && !reasoning_effort.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_endpoint_declares_nothing_and_therefore_sends_nothing() {
        let capability = resolve(
            "glm-4.6",
            "https://relay.example.test/v1",
            None,
            None,
            &BTreeMap::new(),
        );
        assert_eq!(capability, Capability::default(), "没依据就不许当它支持");
        assert!(!capability.warmable(), "存活期未知时不许花真钱赌命中");
    }

    #[test]
    fn deepseek_gets_no_cache_identity_because_it_has_no_such_parameter() {
        let capability = resolve(
            "deepseek-chat",
            "https://api.deepseek.com/v1",
            None,
            None,
            &BTreeMap::new(),
        );
        assert!(!capability.prompt_cache_key, "官方语义是全自动、无请求参数");
        assert_eq!(capability.cache_ttl_seconds, 0);
    }

    #[test]
    fn openai_allows_the_key_and_a_known_ttl() {
        let capability = resolve(
            "gpt-5.6-mini",
            "https://api.openai.com/v1",
            None,
            None,
            &BTreeMap::new(),
        );
        assert!(capability.prompt_cache_key);
        assert!(capability.warmable());
    }

    /// Claude 走断点缓存：没有 prompt_cache_key 可带，但存活期是官方给过的 5 分钟
    #[test]
    fn claude_declares_breakpoints_instead_of_a_cache_key_and_a_known_ttl() {
        let official = resolve(
            "claude-sonnet-4-6",
            "https://api.anthropic.com",
            None,
            None,
            &BTreeMap::new(),
        );
        assert!(!official.prompt_cache_key);
        assert_eq!(official.cache_ttl_seconds, 300);
        assert!(official.warmable());

        // 中转站上的 claude 也按型号认：用户把 base_url 指到中转、模型名不变
        let relayed = resolve(
            "claude-sonnet-4-6",
            "https://relay.example.test/v1",
            None,
            None,
            &BTreeMap::new(),
        );
        assert_eq!(relayed.cache_ttl_seconds, 300);
    }

    /// 逐模型条目是最具体的证据：同时存在全局覆盖与内置表时它赢
    #[test]
    fn a_per_model_ttl_entry_beats_both_the_global_override_and_the_builtin() {
        let mut by_model = BTreeMap::new();
        by_model.insert("claude-sonnet-4-6".to_string(), 3600);

        let resolved = resolve(
            "claude-sonnet-4-6",
            "https://api.anthropic.com",
            None,
            Some(120),
            &by_model,
        );
        assert_eq!(resolved.cache_ttl_seconds, 3600);

        // 换一个没写条目的型号，全局覆盖照常生效
        let other = resolve(
            "gpt-5.6-mini",
            "https://api.openai.com/v1",
            None,
            Some(120),
            &by_model,
        );
        assert_eq!(other.cache_ttl_seconds, 120);
    }

    /// 没有逐模型条目时，全局覆盖压过内置表
    #[test]
    fn the_global_override_still_applies_without_a_per_model_entry() {
        let resolved = resolve(
            "claude-sonnet-4-6",
            "https://api.anthropic.com",
            None,
            Some(600),
            &BTreeMap::new(),
        );
        assert_eq!(resolved.cache_ttl_seconds, 600);
    }

    /// 覆盖是双向的：写了 false 就该把内置的 true 关掉
    #[test]
    fn an_override_can_turn_a_builtin_capability_off_as_well_as_on() {
        let off = resolve(
            "gpt-5.6-mini",
            "https://api.openai.com/v1",
            Some(false),
            Some(0),
            &BTreeMap::new(),
        );
        assert_eq!(off, Capability::default());
        let on = resolve(
            "glm-4.6",
            "https://relay.example.test/v1",
            Some(true),
            Some(120),
            &BTreeMap::new(),
        );
        assert_eq!(
            on,
            Capability {
                prompt_cache_key: true,
                cache_ttl_seconds: 120
            }
        );
        assert!(on.warmable());
    }

    /// 缓存身份必须稳定且不超过协议上限
    #[test]
    fn the_cache_key_is_stable_and_clamped_to_the_wire_limit() {
        let long = format!("conv_{}", "x".repeat(120));
        let key = cache_key(&long);
        assert_eq!(key.chars().count(), CACHE_KEY_LIMIT);
        assert_eq!(key, cache_key(&long), "同一话题必须永远得到同一个 key");
        assert_eq!(cache_key("conv_abc"), "conv_abc", "短 id 不该被改动");
    }

    /// 重放安全守卫：只有"线上会发思考参数的 anthropic 线"才不安全。
    /// 预算式思考把缓存键拴在 budget_tokens 上，max_tokens=1 的重放拿不到同一个键
    #[test]
    fn an_anthropic_line_with_thinking_enabled_is_not_replayable() {
        assert!(replayable("anthropic", ""), "不发思考参数就安全");
        assert!(replayable("chat", "high"), "思考参数只属于 OpenAI 两条线");
        assert!(replayable("responses", "high"));
        assert!(!replayable("anthropic", "high"));
    }
}
