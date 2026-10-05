//! 离线假 provider：按话题身份对**最长公共前缀**计费，让"前缀是否还热"这件事在 CI 里就能红。
//!
//! 它是真服务商的一个模型，不是一个 mock 答案：它只承认两件真机器才有的性质——
//! 命中取决于本轮发出的字节与上轮发出的字节**从头相同多少**，以及这个身份由
//! 话题 + 模型共同决定。所以它能在没有网络、没有账单的情况下，把"中段改一个字
//! 就整段作废"这类结构问题测出来（参照 `pi` 的 `providers/faux.ts`）。
//!
//! 刻意**不做**的：不模拟 TTL 到期、不模拟并发请求互相挤掉缓存、不把 `max_tokens`
//! 之类信封字段算进前缀（服务商是先渲染提示再缓存，JSON 体里的键序与这些旋钮不影响
//! 提示的字节）。

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;

use serde_json::Value;

/// 缓存身份档位。`None` 是"这次请求的前缀没人会来延伸"——按隔离处理，
/// 既不读也不写，且**不会**把自己留下次轮基线（摘要/标题那类一次性调用就靠这条隔离）。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CacheRetention {
    None,
    #[default]
    Short,
    Long,
}

/// 一次请求里构成提示前缀的部分。信封字段不放进来，理由见模块头。
#[derive(Clone, Copy, Debug)]
pub struct FauxRequest<'a> {
    /// 缓存身份的一半；`None` 表示这次请求没有身份（等同隔离）
    pub session: Option<&'a str>,
    pub model: &'a str,
    pub retention: CacheRetention,
    pub thread: &'a [Value],
    pub declared: &'a [Value],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FauxUsage {
    /// 含命中部分的总输入，与真协议里 `prompt_tokens` / `input_tokens` 同义
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// `None` = 这个服务商不上报缓存字段，和"上报了、命中 0"是两件事
    pub cached_tokens: Option<u32>,
    pub cache_write_tokens: u32,
}

#[derive(Clone, Debug)]
pub struct FauxRound {
    pub text: String,
    pub usage: FauxUsage,
}

#[derive(Clone, Debug)]
struct Baseline {
    model: String,
    prompt: String,
}

#[derive(Clone, Debug)]
pub struct FauxProvider {
    scripts: VecDeque<String>,
    baselines: HashMap<String, Baseline>,
    /// 关掉它就是在模拟"服务商不回缓存字段"的那一族
    report_cache: bool,
}

impl FauxProvider {
    pub fn new() -> Self {
        Self { scripts: VecDeque::new(), baselines: HashMap::new(), report_cache: true }
    }

    /// 排入一条预定回答，按排队顺序消费
    pub fn reply(mut self, text: impl Into<String>) -> Self {
        self.scripts.push_back(text.into());
        self
    }

    pub fn report_cache(mut self, on: bool) -> Self {
        self.report_cache = on;
        self
    }

    pub fn round(&mut self, request: FauxRequest<'_>) -> Result<FauxRound, String> {
        let prompt = prompt_stream(request.declared, request.thread);
        let input_chars = prompt.chars().count();

        let mut read_chars = 0;
        let mut write_chars = 0;
        // 没有身份、或者声明了隔离，就整段都不进缓存核算：一次都不算错比一次都不算难
        if request.retention != CacheRetention::None {
            if let Some(session) = request.session {
                let previous = self.baselines.get(session);
                let reusable = match previous {
                    // 换模型即换缓存身份：上一份前缀不作数，整段重写
                    Some(baseline) if baseline.model == request.model => {
                        common_prefix_chars(&baseline.prompt, &prompt)
                    }
                    _ => 0,
                };
                read_chars = reusable;
                write_chars = input_chars - reusable;
                self.baselines.insert(
                    session.to_string(),
                    Baseline { model: request.model.to_string(), prompt: prompt.clone() },
                );
            }
        }

        let Some(text) = self.scripts.pop_front() else {
            return Err("faux: 预定回答队列已空".to_string());
        };
        let usage = FauxUsage {
            input_tokens: tokens(input_chars),
            output_tokens: tokens(text.chars().count()),
            cached_tokens: self.report_cache.then_some(tokens(read_chars)),
            cache_write_tokens: if self.report_cache { tokens(write_chars) } else { 0 },
        };
        Ok(FauxRound { text, usage })
    }
}

impl Default for FauxProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// 提示的字节流形状：工具声明渲染在对话之前，所以**改动声明集合会连整段历史一起作废**。
/// 这条不是实现细节，是设计档 §6.2 要求声明集合跨轮稳定的全部理由，`faux` 把它变成可断言的东西。
fn prompt_stream(declared: &[Value], thread: &[Value]) -> String {
    let mut prompt = String::from("tools\n");
    for tool in declared {
        let _ = writeln!(prompt, "{}", canonical(tool));
    }
    prompt.push_str("messages\n");
    for message in thread {
        let _ = writeln!(prompt, "{}", canonical(message));
    }
    prompt
}

/// `serde_json::Value` 的映射是按键排序的 `BTreeMap`，所以这条序列化对
/// "同样的内容、不同的插入顺序"给出同一串字节。
fn canonical(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "‹不可序列化的值›".to_string())
}

fn common_prefix_chars(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

/// 字符数换算 token 的粗档，与参照实现的 `len / 4` 同形。
/// 只做**相对比较**用（本轮比上轮少了多少），不当价格表用。
fn tokens(chars: usize) -> u32 {
    (chars / 4) as u32
}

#[cfg(test)]
mod tests {
    use serde_json::Map;

    use super::*;

    const SESSION: &str = "conv_1";
    const MODEL: &str = "some-model";

    /// 一条 wire 形状的 user/assistant 消息
    fn message(role: &str, content: &str) -> Value {
        let mut fields = Map::new();
        fields.insert("role".into(), Value::String(role.into()));
        fields.insert("content".into(), Value::String(content.into()));
        Value::Object(fields)
    }

    /// 一条工具声明，形状照 `tools::schemas_for` 产出的那种
    fn declaration(name: &str) -> Value {
        let mut function = Map::new();
        function.insert("name".into(), Value::String(name.into()));
        function.insert("parameters".into(), Value::Object(Map::new()));
        let mut fields = Map::new();
        fields.insert("type".into(), Value::String("function".into()));
        fields.insert("function".into(), Value::Object(function));
        Value::Object(fields)
    }

    fn request<'a>(session: Option<&'a str>, thread: &'a [Value], declared: &'a [Value]) -> FauxRequest<'a> {
        FauxRequest { session, model: MODEL, retention: CacheRetention::Short, thread, declared }
    }

    fn empty_tools() -> &'static [Value] {
        &[]
    }

    /// 首轮：没有可延伸的东西，读 0、写全部
    #[test]
    fn the_first_round_reads_nothing_and_writes_everything() {
        let history = vec![message("user", "第一段问题，长到能跨过几个 token 档位")];
        let mut provider = FauxProvider::new().reply("好的");
        let round = provider.round(request(Some(SESSION), &history, empty_tools())).unwrap();
        assert_eq!(round.usage.cached_tokens, Some(0));
        assert!(round.usage.cache_write_tokens > 0, "首轮该整段写缓存，否则这条断言是空的");
        assert_eq!(round.usage.cache_write_tokens, round.usage.input_tokens);
    }

    /// 纯延长线：命中必须等于上一轮的全部输入。它同时否证"任何变化都归零"的假实现
    #[test]
    fn a_round_that_only_extends_the_history_reads_the_whole_previous_prompt() {
        let first_turn = vec![message("user", "第一个问题")];
        let second_turn = vec![message("user", "第一个问题"), message("assistant", "答复一"), message("user", "第二个问题")];

        let mut provider = FauxProvider::new().reply("答复一").reply("答复二");
        let before = provider.round(request(Some(SESSION), &first_turn, empty_tools())).unwrap();
        let after = provider.round(request(Some(SESSION), &second_turn, empty_tools())).unwrap();

        assert_eq!(after.usage.cached_tokens, Some(before.usage.input_tokens));
    }

    /// 中段改一个字：命中的确变了，且只保住变化点之前那截。
    /// 与上一条互为非空对照——只测其一，两种偷懒的实现都能骗过绿
    #[test]
    fn rewording_early_in_the_history_demolishes_everything_after_it() {
        let sent = vec![message("user", "甲乙丙丁戊己庚辛壬癸"), message("assistant", "答复")];
        let replayed = vec![message("user", "甲乙丙丁戊己庚辛壬改"), message("assistant", "答复")];

        let mut provider = FauxProvider::new().reply("答复一").reply("答复二").reply("答复三");
        let before = provider.round(request(Some(SESSION), &sent, empty_tools())).unwrap();
        // 同一段再发一次，先看命中是否真能满（排除"永远测不到命中"的假绿）
        let same = provider.round(request(Some(SESSION), &sent, empty_tools())).unwrap();
        let drifted = provider.round(request(Some(SESSION), &replayed, empty_tools())).unwrap();

        assert_eq!(same.usage.cached_tokens, Some(before.usage.input_tokens));
        let hit = drifted.usage.cached_tokens.expect("该服务商上报缓存");
        assert!(hit > 0, "变化点之前的前缀仍应命中");
        assert!(hit < before.usage.input_tokens, "第 10 个字起就失配，不可能满命中");
    }

    /// 隔离档位：既不读也不写
    #[test]
    fn a_request_isolated_from_the_cache_reports_neither_read_nor_write() {
        let history = vec![message("user", "摘要调用的输入")];
        let mut provider = FauxProvider::new().reply("摘要");
        let round = provider
            .round(FauxRequest {
                session: Some(SESSION),
                model: MODEL,
                retention: CacheRetention::None,
                thread: &history,
                declared: empty_tools(),
            })
            .unwrap();
        assert_eq!(round.usage.cached_tokens, Some(0));
        assert_eq!(round.usage.cache_write_tokens, 0);
    }

    /// 隔离的那次请求不许顶掉热的那份基线——否则摘要会把话题的缓存挤走。
    /// 摘要调用的输入是**另起的一份文本**（把历史拍平喂进去），所以它的字节和话题前缀从第一
    /// 个字就不同；这正是"顶掉基线"能被测出来的前提
    #[test]
    fn an_isolated_request_does_not_become_the_next_round_baseline() {
        let hot = vec![message("user", "话题里真正发出去的历史")];
        let aside = vec![message("user", "把下面这段对话总结成中文：<conversation>用户：话题里真正发出去的历史")];

        let mut provider = FauxProvider::new().reply("一").reply("二").reply("三");
        let before = provider.round(request(Some(SESSION), &hot, empty_tools())).unwrap();
        provider
            .round(FauxRequest {
                session: Some(SESSION),
                model: MODEL,
                retention: CacheRetention::None,
                thread: &aside,
                declared: empty_tools(),
            })
            .unwrap();
        let after = provider.round(request(Some(SESSION), &hot, empty_tools())).unwrap();

        assert_eq!(after.usage.cached_tokens, Some(before.usage.input_tokens));
    }

    /// 没有话题身份 = 没有可延伸的对象
    #[test]
    fn a_request_without_session_identity_cannot_cache() {
        let history = vec![message("user", "标题生成用的历史")];
        let mut provider = FauxProvider::new().reply("一").reply("二");
        provider.round(request(None, &history, empty_tools())).unwrap();
        let second = provider.round(request(None, &history, empty_tools())).unwrap();
        assert_eq!(second.usage.cached_tokens, Some(0));
        assert_eq!(second.usage.cache_write_tokens, 0);
    }

    /// 队列按顺序消费，空了要报错而不是静默给空话
    #[test]
    fn the_scripted_queue_runs_in_order_then_refuses() {
        let history = vec![message("user", "问题")];
        let mut provider = FauxProvider::new().reply("第一条").reply("第二条");
        assert_eq!(provider.round(request(Some(SESSION), &history, empty_tools())).unwrap().text, "第一条");
        assert_eq!(provider.round(request(Some(SESSION), &history, empty_tools())).unwrap().text, "第二条");
        assert!(provider.round(request(Some(SESSION), &history, empty_tools())).is_err());
    }

    /// 内容相同、键的插入顺序不同，前缀必须一模一样（`Value` 的映射是排序的 `BTreeMap`）
    #[test]
    fn json_key_insertion_order_does_not_change_the_prefix() {
        let mut sorted = Map::new();
        sorted.insert("role".into(), Value::String("user".into()));
        sorted.insert("content".into(), Value::String("同样的内容".into()));
        let mut shuffled = Map::new();
        shuffled.insert("content".into(), Value::String("同样的内容".into()));
        shuffled.insert("role".into(), Value::String("user".into()));
        let one = vec![Value::Object(sorted)];
        let other = vec![Value::Object(shuffled)];

        let mut provider = FauxProvider::new().reply("一").reply("二");
        let before = provider.round(request(Some(SESSION), &one, empty_tools())).unwrap();
        let after = provider.round(request(Some(SESSION), &other, empty_tools())).unwrap();
        assert_eq!(after.usage.cached_tokens, Some(before.usage.input_tokens));
    }

    /// 不上报缓存字段的服务商族：读出来是"无从判定"，不是 0
    #[test]
    fn an_endpoint_that_does_not_report_cache_reads_as_none_not_zero() {
        let history = vec![message("user", "长到够写缓存的一段输入内容")];
        let mut provider = FauxProvider::new().report_cache(false).reply("一").reply("二");
        provider.round(request(Some(SESSION), &history, empty_tools())).unwrap();
        let second = provider.round(request(Some(SESSION), &history, empty_tools())).unwrap();
        assert_eq!(second.usage.cached_tokens, None);
        assert_eq!(second.usage.cache_write_tokens, 0);
    }

    /// 工具声明渲染在对话之前：加一条就把整段历史的命中打掉
    #[test]
    fn adding_a_tool_declaration_demolishes_the_whole_conversation_prefix() {
        let history = vec![message("user", "带工具的话题历史")];
        let one_tool = vec![declaration("read_file")];
        let two_tools = vec![declaration("read_file"), declaration("write_file")];

        let mut provider = FauxProvider::new().reply("一").reply("二");
        let before = provider.round(request(Some(SESSION), &history, &one_tool)).unwrap();
        let after = provider.round(request(Some(SESSION), &history, &two_tools)).unwrap();
        assert!(
            after.usage.cached_tokens.unwrap_or(0) < before.usage.input_tokens,
            "在对话之前插进新字节，其后整段都不再命中"
        );
    }

    /// 服务器掉线时沿用上次成功列出的声明：命中一分不丢。这是设计档 §6.2"只增不减"的正面证据
    #[test]
    fn keeping_the_previous_declarations_when_a_server_drops_preserves_the_prefix() {
        let history = vec![message("user", "带工具的话题历史")];
        let declared = vec![declaration("read_file"), declaration("mcp_tool")];
        let dropped_short = vec![declaration("read_file")];

        let mut provider = FauxProvider::new().reply("一").reply("二").reply("三");
        let before = provider.round(request(Some(SESSION), &history, &declared)).unwrap();
        let kept = provider.round(request(Some(SESSION), &history, &declared)).unwrap();
        let shortened = provider.round(request(Some(SESSION), &history, &dropped_short)).unwrap();

        assert_eq!(kept.usage.cached_tokens, Some(before.usage.input_tokens));
        assert!(shortened.usage.cached_tokens.unwrap_or(0) < before.usage.input_tokens);
    }

    /// 换模型即换缓存身份
    #[test]
    fn switching_the_model_resets_the_cache_identity() {
        let history = vec![message("user", "同样的历史")];
        let mut provider = FauxProvider::new().reply("一").reply("二");
        provider.round(request(Some(SESSION), &history, empty_tools())).unwrap();
        let switched = provider
            .round(FauxRequest {
                session: Some(SESSION),
                model: "another-model",
                retention: CacheRetention::Short,
                thread: &history,
                declared: empty_tools(),
            })
            .unwrap();
        assert_eq!(switched.usage.cached_tokens, Some(0));
        assert_eq!(switched.usage.cache_write_tokens, switched.usage.input_tokens);
    }
}
