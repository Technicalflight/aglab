//! 缓存保温：赶在服务商那条 prompt 缓存过期之前，用一次"最多只回一个 token"的重放把它续上。
//!
//! dead_code 暂态放行：调度器管线的备用件与停用的测试随改版闲置，O5/O2 触碰时清理。
#![allow(dead_code)]
//!
//! 花钱的前提在这里写成三件独立的事，缺一不发（设计文档 §7.5；第 1 条
//! 2026-10-09 改版——见 DEFAULT_CACHE_TTL_SECONDS 的注释）：
//!
//! 1. **存活期有档可依**。声明过的按声明档；**未声明的按默认档（5 分钟）**——
//!    行业最短档当赌约，"赶在到期前"永远有内容。把长档当短档用只是多续几次，
//!    反过来才是在赌；这一改把"未声明即不保温"的旧立场翻成了默认保温。
//! 2. **有钱可省**：`p × missCost − warmCost ≥ $0.05`。价表缺失时**不猜价**，
//!    判"经济不可算"而不发（跟白付量那条"没价表就只报 token 数"同一个纪律）。
//! 3. **前缀还作数**：登记那条前缀没有被一次真实请求续过、也没被用户改道放弃。
//!
//! 保温分两个阶段（照 pi 的口径）：
//!
//! - **Streaming**：回合还在跑、工具正在执行。下一发真实请求几乎一定会来，
//!   续接概率按 1 算；时间窗放宽到 1 小时，前缀判据是"登记条目还在当前分支上"
//!   （工具结果只是**追加**，那条前缀仍然是要发出去的字节的前缀）。
//! - **Idle**：回合收尾之后。续接概率回到实测常数 0.15，时间窗收紧到 30 分钟，
//!   前缀判据回到"末端必须原封不动"。
//!
//! 每一发还有一条**刷新截止线**：线程睡过头（系统睡眠、事件循环卡顿）之后，
//! 只容忍"到点与过期之间剩余余量"的一半——再晚的重放大概率已经过期，
//! 发出去不是续缓存，是一次全价新写。
//!
//! 判据全是纯函数、时间从外面进来，为的是这些都能在 CI 里判红，不用等真服务商。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::Value;
use tauri::AppHandle;

use crate::chat;
use crate::config::{self, AppConfig};
use crate::usage::{self, Price};

/// 一次刷新至少要省下这么多钱才值得发
pub const MIN_EXPECTED_SAVINGS_USD: f64 = 0.05;
/// 空闲期里"到期前还会再发一次真实请求"的概率。照 pi 的口径用常数：
/// 它自己注明"从自家用量测出来，按话题估并不比这个常数准"（`cache-warmer.ts:20-26`）
pub const IDLE_CONTINUATION_PROBABILITY: f64 = 0.15;
/// 回合内的续接概率：工具跑完必然还有下一发请求，除非用户按了停止——
/// 而停止的退出路径会把保温当场作废，轮不到这笔账说谎
pub const STREAMING_CONTINUATION_PROBABILITY: f64 = 1.0;
/// 空闲保温的时间窗。比对话流的 1 小时短，因为越老的续接概率估不准
pub const MAX_IDLE_WARMING_AGE_MS: i64 = 30 * 60 * 1000;
/// 回合内保温的时间窗。工具循环再长也很少超过一小时，
/// 窗外的续接已经说不清是在护哪条前缀了
pub const MAX_STREAMING_WARMING_AGE_MS: i64 = 60 * 60 * 1000;
/// 提前量的下限：贴着到期时间发，网络一抖就白付
const MARGIN_MS: i64 = 10_000;
/// 服务商未声明存活期时的默认档：行业最短档（OpenAI / Anthropic / DeepSeek 的
/// 5 分钟档）。旧立场是"未声明即不保温——花真钱赌命中的前提是知道赌约期限"；
/// 2026-10-09 改为**默认每发都保温**：赌约按最保守的那档假设，把长档当短档
/// 用只是多续几次，反过来才是在赌。钱照旧由经济学门槛（第 2 条）把着——
/// 不划算的照样不发，这里改的只是"没证据就不敢发"那一半。
pub const DEFAULT_CACHE_TTL_SECONDS: u32 = 300;
/// 在存活期的这个比例处刷新
const NEAR_EXPIRY_RATIO: f64 = 0.9;

/// 保温挂在回合的哪个阶段。它决定续接概率、时间窗与前缀判据
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    /// 回合收尾之后：末端原封不动才发
    Idle,
    /// 回合内、工具执行中：登记条目还在当前分支上就发
    Streaming,
}

impl Phase {
    fn window_ms(self) -> i64 {
        match self {
            Phase::Idle => MAX_IDLE_WARMING_AGE_MS,
            Phase::Streaming => MAX_STREAMING_WARMING_AGE_MS,
        }
    }

    fn continuation_probability(self) -> f64 {
        match self {
            Phase::Idle => IDLE_CONTINUATION_PROBABILITY,
            Phase::Streaming => STREAMING_CONTINUATION_PROBABILITY,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Warm,
    Stop,
}

/// 一次保温的账。字段全公开：界面与测试看的是同一份，不另算一遍
#[derive(Clone, Debug, PartialEq)]
pub struct Economics {
    /// 价表或 prompt 规模缺失时为假：这时**不能**把"算不出来"当成"省不了钱"或"能省钱"
    pub available: bool,
    pub warm_cost_usd: f64,
    pub miss_cost_usd: f64,
    pub continuation_probability: f64,
    pub expected_savings_usd: f64,
    pub action: Action,
}

/// 保温这笔账划不划算。
///
/// 两条 OpenAI 线都没有"写缓存加价"这一档，所以未命中的代价就是
/// 全价输入与缓存读价之差 × prompt；刷新自己的代价是一次缓存读加一个输出 token。
/// （Anthropic 线的写加价来自它自己的 cache_creation 价：cached 部分照读价算，
/// 未命中部分的差价自然被 miss_cost 收进来，公式不用改）
pub fn economics(
    price: Option<&Price>,
    prompt_tokens: u64,
    continuation_probability: f64,
) -> Economics {
    let empty = Economics {
        available: false,
        warm_cost_usd: 0.0,
        miss_cost_usd: 0.0,
        continuation_probability,
        expected_savings_usd: 0.0,
        action: Action::Stop,
    };
    let Some(price) = price else { return empty };
    if prompt_tokens == 0 {
        return empty;
    }

    let prompt = prompt_tokens as f64;
    let read = usage::parse_price(&price.cache_read_usd_per_m) * prompt / 1_000_000.0;
    let full = usage::parse_price(&price.input_usd_per_m) * prompt / 1_000_000.0;
    let one_output = usage::parse_price(&price.output_usd_per_m) / 1_000_000.0;
    let warm_cost = read + one_output;
    let miss_cost = (full - read).max(0.0);
    // 两个价都是 0 的模型（或者价表里只填了输出价）算不出省不省：别把 0 当成"划算"
    if full <= 0.0 && read <= 0.0 {
        return Economics {
            warm_cost_usd: warm_cost,
            miss_cost_usd: miss_cost,
            ..empty
        };
    }
    let expected = continuation_probability * miss_cost - warm_cost;
    Economics {
        available: true,
        warm_cost_usd: warm_cost,
        miss_cost_usd: miss_cost,
        continuation_probability,
        expected_savings_usd: expected,
        action: if expected >= MIN_EXPECTED_SAVINGS_USD {
            Action::Warm
        } else {
            Action::Stop
        },
    }
}

/// 生效存活期：声明档优先；未声明按默认档（2026-10-09 改版：默认每发都保温）。
/// 纯函数——schedule 的这条决策在这里钉死，不靠线程里的 eprintln 事后诸葛
pub fn effective_ttl_seconds(declared: u32) -> u32 {
    if declared > 0 {
        declared
    } else {
        DEFAULT_CACHE_TTL_SECONDS
    }
}

/// 刷新时刻：存活期的 90%，但至少留出 10 秒余量。存活期短于余量的服务商不保温
pub fn warming_delay_ms(ttl_ms: i64) -> Option<i64> {
    if ttl_ms <= MARGIN_MS {
        return None;
    }
    let near_expiry = (ttl_ms as f64 * NEAR_EXPIRY_RATIO) as i64;
    Some(near_expiry.min(ttl_ms - MARGIN_MS).max(1))
}

/// 本轮刷新的截止线：到点之后再容忍"剩余余量"的一半。
///
/// 线程睡过头只有两种解释——系统睡了，或者线程被什么卡住了。醒得太晚时
/// 那次缓存可能已经过期，此刻的重放不是续命而是一次全价新写；
/// 半个余量是"还赶得上"与"已经晚了"的分界（照 pi 的 refreshDeadline 口径）
pub fn refresh_deadline_ms(cycle_started_at: i64, delay_ms: i64, ttl_ms: i64) -> i64 {
    let remaining = (ttl_ms - delay_ms).max(0);
    cycle_started_at + delay_ms + remaining / 2
}

/// 登记下来等待刷新的那一发真实请求
#[derive(Clone, Debug)]
pub struct Plan {
    pub conversation_id: String,
    /// 发出那一刻的日志末端条目 id。Idle 看它原封不动；Streaming 看它还在当前分支上
    pub tip: Option<String>,
    /// 那一刻这条前缀刚被一次真实请求刷新过，存活期从它算起
    pub sent_at: i64,
    /// 服务商上一发真实请求报告的 prompt 规模（`usage::last_prompt_tokens_for`）
    pub prompt_tokens: u64,
    /// 刷新间隔。由 `schedule` 按当时的存活期算出来填回，`veto` 要用它判截止
    pub delay_ms: i64,
    /// 生效存活期（毫秒）：声明档；未声明时按默认档。`schedule` 填回；截止线的余量从它算
    pub ttl_ms: i64,
    /// 本发挂在哪个阶段
    pub phase: Phase,
    /// 本轮刷新的绝对截止时刻。`schedule` 填第一轮，线程里每发成功后重算下一轮
    pub refresh_deadline_ms: i64,
}

/// Streaming 阶段的前缀判据：登记的末端条目还在当前分支上吗。
///
/// 分支只往前长（追加工具结果、插话）时它仍是"要发出去的字节的前缀"，作数；
/// 回溯重发（navigate 到更早的条目再长出新枝）才说明那条前缀被放弃了。
/// `branch` 是从叶到根的条目 id 链（`SessionLog::path` 的投影）
pub fn tip_on_branch(tip: Option<&str>, branch: &[String]) -> bool {
    match tip {
        None => branch.is_empty(),
        Some(tip) => branch.iter().any(|id| id == tip),
    }
}

/// 到点之后还要不要再发。返回 `None` = 发；`Some(原因)` = 作废。
/// 原因是给人看的字符串而不是枚举：它要么进日志，要么进以后的状态查询
pub fn veto(plan: &Plan, prefix_intact: bool, now: i64) -> Option<&'static str> {
    if !prefix_intact {
        return Some(match plan.phase {
            Phase::Idle => "日志末端已经移动：那次前缀不再是当前前缀",
            Phase::Streaming => "分支已经改道：登记时那条前缀不再是当前分支的前缀",
        });
    }
    if plan.delay_ms > plan.phase.window_ms() {
        return Some("存活期比这个阶段的时间窗还长，这一发不该排");
    }
    if now >= plan.sent_at + plan.phase.window_ms() {
        return Some(match plan.phase {
            Phase::Idle => "超出空闲保温时间窗，续接概率估不准了",
            Phase::Streaming => "超出回合内保温时间窗",
        });
    }
    if now > plan.refresh_deadline_ms {
        return Some("错过了刷新截止线：迟到的重放大概率是一次全价缓存写");
    }
    if now < plan.sent_at + plan.delay_ms {
        return Some("还没到刷新时刻");
    }
    None
}

/// 每条话题最多挂一发待放的刷新。开关是 `Arc<AtomicBool>`：真实请求一开始就把它拉下来
#[derive(Clone, Default)]
pub struct Hub {
    pending: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
}

impl Hub {
    /// 新一轮真实请求开始：上一发保温作废。
    /// 拉闸而不是删记录——已经睡着的那个线程要能自己看见"别发了"
    pub fn cancel(&self, conversation_id: &str) {
        if let Some(flag) = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(conversation_id)
        {
            flag.store(true, Ordering::SeqCst);
        }
    }

    fn arm(&self, conversation_id: &str) -> Arc<AtomicBool> {
        self.cancel(conversation_id);
        let flag = Arc::new(AtomicBool::new(false));
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(conversation_id.to_string(), Arc::clone(&flag));
        flag
    }

    fn disarm(&self, conversation_id: &str) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(conversation_id);
    }
}

/// 排一次保温。所有"该不该发"的判断都先做完，线程里只剩"到点时那几件事还成立吗"
pub fn schedule(
    app: &AppHandle,
    config: &AppConfig,
    hub: &Hub,
    mut plan: Plan,
    rows: Vec<Value>,
    declared: Vec<Value>,
) {
    if !config.cache_warming {
        return;
    }
    let capability = config.capability();
    // 重放安全守卫：预算式思考把缓存键拴在 max_tokens 上，重放等于白付一次全价
    if !crate::provider::capability::replayable(&config.api_format, &config.reasoning_effort) {
        eprintln!("anthropic 线开着思考参数，max_tokens=1 的重放拿不到同一个缓存键，不保温");
        return;
    }
    // 存活期：声明档优先；未声明按默认档（5 分钟）保温——默认每发都续
    let declared_ttl = capability.cache_ttl_seconds;
    if declared_ttl == 0 {
        eprintln!("服务商未声明缓存存活期，按默认 5 分钟档保温（每发都续，账照算）");
    }
    let ttl_ms = i64::from(effective_ttl_seconds(declared_ttl)) * 1000;
    let Some(delay) = warming_delay_ms(ttl_ms) else {
        eprintln!("缓存存活期短于刷新余量，不保温");
        return;
    };
    plan.delay_ms = delay;
    plan.ttl_ms = ttl_ms;
    plan.refresh_deadline_ms = refresh_deadline_ms(plan.sent_at, delay, ttl_ms);
    if !worth_it(app, config, plan.prompt_tokens, plan.phase) {
        return;
    }

    let flag = hub.arm(&plan.conversation_id);
    let hub = hub.clone();
    let handle = app.clone();
    let config = config.clone();
    let conversation_id = plan.conversation_id.clone();
    thread::spawn(move || {
        let mut plan = plan;
        loop {
            thread::sleep(Duration::from_millis(plan.delay_ms as u64));
            // 每一发之前都重看一遍那几件事：登记那一刻成立，不代表五分钟后还成立
            if flag.load(Ordering::SeqCst) {
                eprintln!("保温停了：新的真实请求已经把这层缓存续上");
                hub.disarm(&conversation_id);
                return;
            }
            // 重放必须原样复用那次请求的字节，所以行与声明都是登记时的快照，不重新装配
            let Ok(opened) = chat::open_session(&handle, &conversation_id) else {
                eprintln!("保温前读日志失败，这一发停了");
                hub.disarm(&conversation_id);
                return;
            };
            let branch: Vec<String> = opened
                .log
                .path()
                .map(|chain| chain.iter().map(|entry| entry.id.clone()).collect())
                .unwrap_or_default();
            let prefix_intact = match plan.phase {
                Phase::Idle => opened.log.leaf_id() == plan.tip.as_deref(),
                Phase::Streaming => tip_on_branch(plan.tip.as_deref(), &branch),
            };
            let now = crate::session::now_millis();
            if let Some(reason) = veto(&plan, prefix_intact, now) {
                eprintln!("保温停了：{reason}");
                hub.disarm(&conversation_id);
                return;
            }
            if !worth_it(&handle, &config, plan.prompt_tokens, plan.phase) {
                hub.disarm(&conversation_id);
                return;
            }
            // 发完**不**解除登记：循环还挂着，下一次真实请求才叫得停它
            send(&handle, &config, &conversation_id, &rows, &declared);
            // 这一发之后截止线重算：下次睡过头只容忍剩余余量的一半。
            // 时间窗仍锚在登记那一刻，不会因为循环而越滚越长
            plan.refresh_deadline_ms = refresh_deadline_ms(now, plan.delay_ms, plan.ttl_ms);
        }
    });
}

/// 现在这一刻这笔账还划不划算。价表可能在等待期间被改过，所以每发都重查一次
fn worth_it(app: &AppHandle, config: &AppConfig, prompt_tokens: u64, phase: Phase) -> bool {
    let price = usage::with_connection(app, |conn| usage::price_for(conn, &config.model))
        .ok()
        .flatten();
    let economics = economics(
        price.as_ref(),
        prompt_tokens,
        phase.continuation_probability(),
    );
    if economics.action == Action::Warm {
        return true;
    }
    eprintln!(
        "保温没有发出去：{}（预计省 ${:.4}，门槛 ${:.2}，续接概率 {:.0}%）",
        if economics.available {
            "算过账，不划算"
        } else {
            "价表算不出账"
        },
        economics.expected_savings_usd,
        MIN_EXPECTED_SAVINGS_USD,
        economics.continuation_probability * 100.0,
    );
    false
}

/// 那一次重放：`max_tokens=1`、不重试（重试循环在 `run_turn` 那边，这里只走一次
/// `request_round`）。失败只记账不报错——保温是尽力而为，挂了也不该让用户觉得对话出了事
fn send(
    app: &AppHandle,
    config: &AppConfig,
    conversation_id: &str,
    rows: &[Value],
    declared: &[Value],
) {
    let key = match config::api_key(config) {
        Ok(key) => key,
        Err(error) => {
            eprintln!("保温没有发出：{error}");
            return;
        }
    };
    let mut warm_config = config.clone();
    warm_config.max_tokens = 1;
    let stop = AtomicBool::new(false);
    let started = std::time::Instant::now();
    let outcome = chat::request_round(
        &warm_config,
        &key,
        rows,
        declared,
        Some(conversation_id),
        &stop,
        &mut |_| {},
    );
    let duration_ms = started.elapsed().as_millis() as u64;
    match outcome {
        Ok(outcome) => usage::record_turn(
            app,
            config,
            "cache_warm",
            conversation_id,
            &config.model,
            &outcome.tokens(),
            outcome.sent_chars(),
            false,
            duration_ms,
            None,
            true,
            "",
        ),
        Err(failure) => usage::record_turn(
            app,
            config,
            "cache_warm",
            conversation_id,
            &config.model,
            &usage::Tokens::default(),
            0,
            false,
            duration_ms,
            None,
            false,
            failure.message(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn price(input: &str, output: &str, read: &str) -> Price {
        Price {
            model_id: "m-one".into(),
            display_name: String::new(),
            input_usd_per_m: input.into(),
            output_usd_per_m: output.into(),
            cache_read_usd_per_m: read.into(),
            cache_creation_usd_per_m: "0".into(),
        }
    }

    /// 刷新时刻：存活期的 90%，但永远留出 10 秒余量；短到留不出余量的服务商不保温
    /// 2026-10-09 改版：未声明存活期按默认档保温，不再拒绝。
    /// 声明档原样生效——两种来源的 ttl 都要在 schedule 里长成同一种延迟
    #[test]
    fn an_undeclared_ttl_falls_back_to_the_default_tier_and_declared_wins() {
        assert_eq!(
            effective_ttl_seconds(0),
            DEFAULT_CACHE_TTL_SECONDS,
            "未声明 → 行业最短档（5 分钟），默认每发都保温"
        );
        assert_eq!(
            effective_ttl_seconds(3600),
            3600,
            "声明档原样生效，默认档不抢戏"
        );
        // 默认档要能长出一个合法的刷新间隔（超过 10 秒余量，否则每发都会被拒）
        let delay = warming_delay_ms(i64::from(DEFAULT_CACHE_TTL_SECONDS) * 1000)
            .expect("默认档必须留得出刷新余量");
        assert!(delay > 0);
    }

    fn the_refresh_fires_near_expiry_but_never_inside_the_margin() {
        assert_eq!(
            warming_delay_ms(300_000),
            Some(270_000),
            "5 分钟档在 4.5 分钟处续"
        );
        assert_eq!(
            warming_delay_ms(11_000),
            Some(1_000),
            "贴着余量时取余量那侧"
        );
        assert_eq!(warming_delay_ms(10_000), None, "留不出余量就谈不上到期前");
        assert_eq!(warming_delay_ms(0), None);
    }

    /// 截止线：到点之后只容忍剩余余量的一半。5 分钟档在 4.5 分钟处续、
    /// 4 分 45 秒就是底线——再晚的重放宁可不放
    #[test]
    fn the_refresh_deadline_keeps_half_of_the_remaining_margin() {
        assert_eq!(refresh_deadline_ms(1_000, 270_000, 300_000), 286_000);
        assert_eq!(
            refresh_deadline_ms(1_000, 1_000, 11_000),
            7_000,
            "11 秒过期、1 秒到点：余量 10 秒的一半（5 秒）是迟到容限"
        );
        // delay 不可能超过 ttl，但公式自己得站得住
        assert_eq!(refresh_deadline_ms(1_000, 300_000, 300_000), 301_000);
    }

    /// 经济门槛：至少省下 $0.05 才发。小 prompt 在空闲档不值得为不确定的续接花钱
    #[test]
    fn an_idle_refresh_needs_to_clear_the_expected_savings_floor() {
        // 输入 $2.5/M、输出 $10/M、缓存读 $0.25/M
        let small = economics(
            Some(&price("2.5", "10", "0.25")),
            40_000,
            IDLE_CONTINUATION_PROBABILITY,
        );
        assert!(small.available);
        // miss = 40k × 2.25/M = 0.09；warm = 40k × 0.25/M + 10/M ≈ 0.01001
        // expected = 0.15 × 0.09 − 0.01001 ≈ 0.00349
        assert_eq!(
            small.action,
            Action::Stop,
            "40k token 的话题空闲期不值得保温"
        );
        assert!(
            (small.miss_cost_usd - 0.09).abs() < 1e-9,
            "missCost 算法漂移：{}",
            small.miss_cost_usd
        );

        let large = economics(
            Some(&price("2.5", "10", "0.25")),
            2_000_000,
            IDLE_CONTINUATION_PROBABILITY,
        );
        // miss = 2M × 2.25/M = 4.5；warm ≈ 0.5；expected = 0.15×4.5 − 0.5 = 0.175
        assert_eq!(large.action, Action::Warm);
        assert!(
            large.expected_savings_usd >= MIN_EXPECTED_SAVINGS_USD,
            "过线的判据自己得先过线：{}",
            large.expected_savings_usd
        );
    }

    /// 回合内续接概率按 1 算：同一笔账，空闲档放弃的 prompt 在工具循环里该续
    #[test]
    fn a_streaming_refresh_counts_on_the_next_request_coming() {
        let small = economics(
            Some(&price("2.5", "10", "0.25")),
            40_000,
            STREAMING_CONTINUATION_PROBABILITY,
        );
        // expected = 1.0 × 0.09 − 0.01001 ≈ 0.08 ≥ 0.05
        assert_eq!(small.action, Action::Warm);
        assert_eq!(small.continuation_probability, 1.0);
    }

    /// "算不出来"必须是个可表达的状态，而不是 0：把未知当成"不划算"和当成"划算"一样糟
    #[test]
    fn no_price_row_or_no_prompt_size_is_not_a_decision() {
        let missing = economics(None, 40_000, IDLE_CONTINUATION_PROBABILITY);
        assert!(!missing.available);
        assert_eq!(missing.action, Action::Stop);

        let empty_prompt = economics(
            Some(&price("2.5", "10", "0.25")),
            0,
            IDLE_CONTINUATION_PROBABILITY,
        );
        assert!(!empty_prompt.available, "prompt 规模不知道就没法算");

        // 价表里只填了输出价的模型：输入与缓存都按 0 算，省不出钱，也不该假装省了钱
        let output_only = economics(
            Some(&price("0", "10", "0")),
            40_000,
            IDLE_CONTINUATION_PROBABILITY,
        );
        assert!(!output_only.available, "两个价都是 0 属于没填，不是免费");
        assert_eq!(output_only.action, Action::Stop);
    }

    fn idle_plan(tip: Option<&str>, sent_at: i64, delay_ms: i64) -> Plan {
        Plan {
            conversation_id: "conv_a".into(),
            tip: tip.map(str::to_string),
            sent_at,
            prompt_tokens: 2_000_000,
            delay_ms,
            ttl_ms: 300_000,
            phase: Phase::Idle,
            refresh_deadline_ms: refresh_deadline_ms(sent_at, delay_ms, 300_000),
        }
    }

    /// 到点核对：末端一动就作废——那说明中间又发过真实请求，缓存已经被续上了
    #[test]
    fn a_moved_branch_or_a_missed_deadline_calls_the_refresh_off() {
        let plan = idle_plan(Some("e5"), 1_000, 270_000);
        assert_eq!(
            veto(&plan, false, 1_000 + 270_000),
            Some("日志末端已经移动：那次前缀不再是当前前缀")
        );
        assert_eq!(veto(&plan, true, 1_000 + 270_000), None, "该发的时候得发");
        assert_eq!(veto(&plan, true, 1_000 + 269_999), Some("还没到刷新时刻"));
        assert_eq!(
            veto(&plan, true, 1_000 + MAX_IDLE_WARMING_AGE_MS),
            Some("超出空闲保温时间窗，续接概率估不准了")
        );

        let too_long = idle_plan(Some("e5"), 1_000, 60 * 60 * 1000);
        assert_eq!(
            veto(&too_long, true, 1_000 + too_long.delay_ms),
            Some("存活期比这个阶段的时间窗还长，这一发不该排")
        );
    }

    /// 刷新截止线：睡过头发底线的那一刻，这一发就叫停——
    /// 迟到的重放大概率落在缓存过期之后，发出去是一次全价新写
    #[test]
    fn a_wake_up_past_the_refresh_deadline_is_not_sent() {
        // 5 分钟档：4.5 分钟到点，4 分 45 秒是截止线
        let plan = idle_plan(Some("e5"), 1_000, 270_000);
        assert_eq!(plan.refresh_deadline_ms, 286_000);
        assert_eq!(veto(&plan, true, 285_999), None, "底线之内还赶得上");
        assert_eq!(
            veto(&plan, true, 286_001),
            Some("错过了刷新截止线：迟到的重放大概率是一次全价缓存写")
        );
    }

    /// Streaming 阶段的前缀判据：登记条目还在当前分支上就作数。
    /// 工具结果追加让分支往前长，那条前缀仍然是要发出去的字节的前缀；
    /// 回溯重发让分支改道，它才真的被放弃了
    #[test]
    fn a_streaming_plan_survives_appends_but_not_a_rewind() {
        // 分支 e1 → e2 → e3（e3 是叶），登记在 e2
        let branch = vec!["e1".to_string(), "e2".to_string(), "e3".to_string()];
        assert!(tip_on_branch(Some("e2"), &branch), "追加不破坏前缀");
        assert!(
            tip_on_branch(Some("e1"), &branch),
            "登记在更早的条目上同样作数"
        );
        assert!(tip_on_branch(Some("e3"), &branch), "叶自己也是分支上的点");

        // 回溯重发后的分支：e1 → e4。e2/e3 不在链上，前缀被放弃
        let rewound = vec!["e1".to_string(), "e4".to_string()];
        assert!(!tip_on_branch(Some("e2"), &rewound));
        assert!(!tip_on_branch(Some("e3"), &rewound));

        // 空日志对空登记
        assert!(tip_on_branch(None, &[]));
        assert!(!tip_on_branch(Some("e1"), &[]));
    }

    /// Streaming 的时间窗比空闲档宽（1 小时对 30 分钟），判据文案也要分得清
    #[test]
    fn the_streaming_window_is_wider_and_says_so() {
        let mut plan = idle_plan(Some("e5"), 1_000, 270_000);
        plan.phase = Phase::Streaming;
        plan.refresh_deadline_ms = refresh_deadline_ms(1_000, 270_000, 300_000);

        assert_eq!(
            veto(&plan, false, 1_000 + 270_000),
            Some("分支已经改道：登记时那条前缀不再是当前分支的前缀")
        );
        // 截止线只容忍剩余余量的一半：40 分钟后早就过了 4 分 45 秒的底线
        assert_eq!(
            veto(&plan, true, 1_000 + 40 * 60 * 1000),
            Some("错过了刷新截止线：迟到的重放大概率是一次全价缓存写")
        );

        // 真正的出窗时刻在 1 小时处，而不是 30 分钟处。
        // delay 必须小于窗（veto 会先拦"不该排"），取 65 分钟档的 90%≈58 分钟
        let mut long_lived = plan;
        long_lived.ttl_ms = 65 * 60 * 1000;
        long_lived.delay_ms = 58 * 60 * 1000;
        long_lived.refresh_deadline_ms = refresh_deadline_ms(1_000, 58 * 60 * 1000, 65 * 60 * 1000);
        assert_eq!(
            veto(&long_lived, true, 1_000 + 59 * 60 * 1000),
            None,
            "回合内 59 分钟仍在窗内"
        );
        assert_eq!(
            veto(&long_lived, true, 1_000 + MAX_STREAMING_WARMING_AGE_MS),
            Some("超出回合内保温时间窗")
        );
    }

    /// 登记表：同一话题只挂一发，取消是让睡着的线程自己看见，不是删了记录就完事
    #[test]
    fn arming_a_refresh_cancels_the_previous_one() {
        let hub = Hub::default();
        let first = hub.arm("conv_a");
        assert!(!first.load(Ordering::SeqCst));
        let second = hub.arm("conv_a");
        assert!(first.load(Ordering::SeqCst), "旧的保温必须被叫停");
        assert!(!second.load(Ordering::SeqCst));

        hub.cancel("conv_a");
        assert!(second.load(Ordering::SeqCst));

        let other = hub.arm("conv_b");
        hub.cancel("conv_a");
        assert!(!other.load(Ordering::SeqCst), "别的话题不受影响");
    }
}
