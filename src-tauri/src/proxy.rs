//! 代理：出站 HTTP 走哪条路的唯一裁决处，外加代理池自己的账本。
//!
//! **绑定语法**（三级共用一套值域）：
//!   `""`      继承上一层（全局层 = 直连）
//!   `direct`  直连——上一层配了也明确不要
//!   `pool`    代理池：在**启用的**代理之间按策略均衡
//!   其余      代理 id，必须指向池里一条启用的代理
//!
//! **解析顺序**：服务商内按模型覆盖（`proxy_by_model`，键是模型名）→ 服务商（`proxy`）→
//! 全局（`proxy_default`）。绕过名单与本机回环恒直连——回环豁免不进名单也生效，
//! 因为 sidecar / inbound webhook 这些本机流量走代理等于自断。
//!
//! **归因**：一次出站尝试的结局分四类，只有 `Unreachable` 让代理进冷却。
//!   `Reached`     拿到了响应头，**含服务商回的 4xx/5xx**——通路成立，清冷却
//!   `Unreachable` 一个头都没拿到的传输层失败——这一发经这条代理出不去
//!   `Interrupted` 头之后流被掐——长思考模型的空闲超时是服务商与中转站的行为
//!   `Neutral`     与代理无关：用户停止、出口名单拦截、我们自己的 URL/凭据问题、前端拒收事件
//! 判在错误种类**还知道**的那一层（发出请求处 / 读流处），不在包装层猜错误字符串：
//! 连吃三次服务商的 429 就把一条好代理关进 30 秒冷却，那是替别人受罚。
//!
//! **换路**：池绑定的一次请求最多试 [`MAX_ATTEMPTS`] 条——第一条按策略挑，其余按配置顺序
//! 补在后面。只在"一个字节都还没吐出去且连不上"时换下一条：吐过正文再换路等于把同一回合
//! 重播一遍。点名一条代理与直连各只有一条路，换路是池的语义，指名一条意味着"别给我绕"。
//!
//! **失败语义**：点名的代理不在场或已停用 → 这一发**报错**，不静默直连。用户指名
//! 一条代理，多半是因为"不走它就出不去"或"不想暴露真实地址"——静默的直连比一次
//! 失败更难查（与模型池 pinned 成员被删时的语义同源）。
//!
//! **行为确定性**：模型出口一律经 [`agent_for`] 显式构造 Agent——ureq 的默认 Agent
//! 会读系统 `HTTP_PROXY` 等环境变量，这里不读：aglab 的代理配置说了算，
//! 系统环境里那几只变量不再劫持模型流量。

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::config::{AppConfig, EndpointProfile, ProxyEntry, ProxyPool};

// ---------------------------------------------------------------- 池的账本

/// 连续「连不上」多少次进冷却。1–2 次可能是网络抖动，3 次起才像这条代理病了
const FAILURE_THRESHOLD: u32 = 3;
/// 冷却基数与倍增：第 3 次连不上歇 30s，第 4 次 60s……封顶 10 分钟
const COOLDOWN_BASE: Duration = Duration::from_secs(30);
const COOLDOWN_CAP: Duration = Duration::from_secs(600);
/// 延迟 EWMA 的衰减窗与新鲜度窗，同一个 10 分钟：桌面应用一小时没发请求，
/// 旧样本就该淡掉；超过这个窗的读数不再参与比较（宁可不判，不要拿过期结论压人）
const LATENCY_WINDOW: Duration = Duration::from_secs(600);
/// 一次请求最多试几条路。3 = 换两条还不行就是这一带出去不通，继续换只把一回合拖成十几秒
const MAX_ATTEMPTS: usize = 3;
/// Agent 缓存的有界上限：桌面配置的代理数量远用不完它，攒到顶就整张重开
const AGENT_CACHE_MAX: usize = 16;
/// 权重值域。钳位只住在这一个读侧函数里——写侧宽松（用户爱填几填几），
/// 判据只有一个家，否则同一个数住在两处就会给出两个答案
const WEIGHT_MIN: u32 = 1;
const WEIGHT_MAX: u32 = 100;

/// 一次出站尝试对**代理**意味着什么（见文件头的归因表）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Reached,
    Unreachable,
    Interrupted,
    Neutral,
}

/// 时间衰减的指数加权均值。用 `exp(-Δt/窗)` 而不是固定 α：样本间隔不均匀时
/// 固定 α 会把"三秒前的"和"一小时前的"当成一样新
#[derive(Default, Clone)]
struct Ewma {
    mean: f64,
    updated: Option<Instant>,
}

impl Ewma {
    fn observe(&mut self, ms: u64) {
        let now = Instant::now();
        let kept = match self.updated {
            // 第一次观测：原始数字直接作均值，不做"半个均值"的假设
            Some(prev) => {
                (-(now.duration_since(prev).as_secs_f64()) / LATENCY_WINDOW.as_secs_f64()).exp()
            }
            None => 0.0,
        };
        self.mean = self.mean * kept + ms as f64 * (1.0 - kept);
        self.updated = Some(now);
    }

    /// 新鲜度闸门：过期的样本一律答"没量过"
    fn fresh(&self) -> Option<f64> {
        let prev = self.updated?;
        if Instant::now().duration_since(prev) > LATENCY_WINDOW {
            None
        } else {
            Some(self.mean)
        }
    }
}

#[derive(Default, Clone)]
struct Entry {
    /// 被派出去的请求总数
    total: u64,
    /// 正在跑的请求。"最少使用"策略的依据
    inflight: i64,
    /// 连续**连不上**次数。一次通路成立就清零——要的是"现在通不通"，不是历史平均
    failures: u32,
    cooldown_until: Option<Instant>,
    /// 平滑加权轮询的游标（nginx 同款）
    current_weight: i64,
    /// 三类归因的累计读数：给面板看，判定只看 failures
    reached: u64,
    unreachable: u64,
    interrupted: u64,
    /// 拿到响应头的耗时（相对发出请求）。两条出口都量得到，所以选它作判据
    head: Ewma,
    /// 首个 SSE 事件的耗时。用户真正感到的那个数，只报不判
    ttft: Ewma,
}

/// 代理池的账。与模型池的账分开记：同一个"失败"在两头含义不同
/// （模型失败可能是模型不行，代理失败才是路不通）
#[derive(Default)]
struct Hub {
    /// 随机档的 xorshift 游标。不引 rand 依赖，确定性留在测试手里
    rng: u64,
    entries: HashMap<String, Entry>,
    /// 按代理地址复用的 Agent（各自带着自己的连接池）
    agents: HashMap<String, ureq::Agent>,
}

fn hub() -> &'static std::sync::Mutex<Hub> {
    static HUB: std::sync::OnceLock<std::sync::Mutex<Hub>> = std::sync::OnceLock::new();
    HUB.get_or_init(|| std::sync::Mutex::new(Hub::default()))
}

fn is_fresh(hub: &Hub, id: &str) -> bool {
    match hub.entries.get(id).and_then(|entry| entry.cooldown_until) {
        Some(until) => Instant::now() >= until,
        None => true,
    }
}

fn weight_of(entry: &ProxyEntry) -> i64 {
    entry.weight.clamp(WEIGHT_MIN, WEIGHT_MAX) as i64
}

fn cooldown_remaining(until: &Option<Instant>) -> u64 {
    until
        .map(|until| until.saturating_duration_since(Instant::now()).as_millis() as u64)
        .unwrap_or_default()
}

/// 平滑加权轮询（nginx 同款）：每条每轮加上自己的权重，最大者胜出并把总权重扣掉
fn smooth_wrr(hub: &mut Hub, candidates: &[&ProxyEntry]) -> usize {
    let total_weight: i64 = candidates.iter().map(|entry| weight_of(entry)).sum();
    let mut best = 0usize;
    let mut best_weight = i64::MIN;
    for (index, candidate) in candidates.iter().enumerate() {
        let entry = hub.entries.entry(candidate.id.clone()).or_default();
        entry.current_weight += weight_of(candidate);
        if entry.current_weight > best_weight {
            best_weight = entry.current_weight;
            best = index;
        }
    }
    if let Some(entry) = hub.entries.get_mut(&candidates[best].id) {
        entry.current_weight -= total_weight;
    }
    best
}

/// 加权随机：在权重总长上掷一个点，落在哪段就是哪条
fn weighted_random(hub: &mut Hub, candidates: &[&ProxyEntry]) -> usize {
    let weights: Vec<i64> = candidates.iter().map(|entry| weight_of(entry)).collect();
    let total: i64 = weights.iter().sum();
    if hub.rng == 0 {
        // xorshift 的 0 是吸收态：种子给个非零常数，否则这一档永远挑第一条
        hub.rng = 0x9E37_79B9_7F4A_7C15;
    }
    hub.rng ^= hub.rng << 13;
    hub.rng ^= hub.rng >> 7;
    hub.rng ^= hub.rng << 17;
    let mut point = (hub.rng % total.max(1) as u64) as i64;
    for (index, weight) in weights.iter().enumerate() {
        if point < *weight {
            return index;
        }
        point -= *weight;
    }
    candidates.len() - 1
}

/// 当前并发最少；并列时比累计派单数
fn least_used(hub: &Hub, candidates: &[&ProxyEntry]) -> usize {
    let mut best = 0usize;
    let mut best_rank = (i64::MAX, u64::MAX);
    for (index, candidate) in candidates.iter().enumerate() {
        let rank = load_rank(hub, candidate);
        if rank < best_rank {
            best_rank = rank;
            best = index;
        }
    }
    best
}

fn load_rank(hub: &Hub, candidate: &ProxyEntry) -> (i64, u64) {
    let entry = hub.entries.get(&candidate.id).cloned().unwrap_or_default();
    (entry.inflight, entry.total)
}

/// 自适应：占用乘上手上的头耗时，谁小走谁。**可比性规则**——没量过的代理不参与
/// 这场比较，先在没量过的里面挑最闲的量一次。拿"有样本"去压"没样本"，
/// 新加的那条永远出不了局——它只是还没被量过，不是它慢
fn adaptive(hub: &Hub, candidates: &[&ProxyEntry]) -> usize {
    let unmeasured: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| {
            hub.entries
                .get(&candidate.id)
                .and_then(|entry| entry.head.fresh())
                .is_none()
        })
        .map(|(index, _)| index)
        .collect();
    if !unmeasured.is_empty() {
        // 没量过的里面挑当前最闲的那条去量一次
        let mut best = unmeasured[0];
        for &index in &unmeasured {
            if load_rank(hub, candidates[index]) < load_rank(hub, candidates[best]) {
                best = index;
            }
        }
        return best;
    }
    let score = |index: usize| -> f64 {
        let entry = hub
            .entries
            .get(&candidates[index].id)
            .cloned()
            .unwrap_or_default();
        (entry.inflight.max(0) as f64 + 1.0) * entry.head.fresh().unwrap_or(1.0)
    };
    let mut best = 0usize;
    for index in 1..candidates.len() {
        if score(index).total_cmp(&score(best)) == std::cmp::Ordering::Less {
            best = index;
        }
    }
    best
}

/// 策略挑选。冷却中的代理先排开；全都在冷却时挑冷却最早结束的——
/// 走一条可能还在生病的代理，好过让整发请求没得可走
fn pick_strategy(hub: &mut Hub, strategy: &str, candidates: &[&ProxyEntry]) -> usize {
    let fresh: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| is_fresh(hub, &candidate.id))
        .map(|(index, _)| index)
        .collect();
    if fresh.is_empty() {
        let mut order: Vec<usize> = (0..candidates.len()).collect();
        order.sort_by_key(|&index| {
            hub.entries
                .get(&candidates[index].id)
                .and_then(|entry| entry.cooldown_until)
        });
        return order.first().copied().unwrap_or(0);
    }
    if fresh.len() == 1 {
        return fresh[0];
    }
    let scoped: Vec<&ProxyEntry> = fresh.iter().map(|&index| candidates[index]).collect();
    let picked = match strategy {
        "least_used" => least_used(hub, &scoped),
        "adaptive" => adaptive(hub, &scoped),
        "random" => weighted_random(hub, &scoped),
        // 认不出的策略退回轮询：错档位顶多是不够聪明，不该是不能用
        _ => smooth_wrr(hub, &scoped),
    };
    fresh[picked]
}

// ---------------------------------------------------------------- 计划与单步

/// 一次请求的代理裁决结果
#[derive(Debug, Clone, PartialEq)]
pub enum Resolved {
    Direct,
    /// `via` = Some(代理 id)：这条路由池记账（冷却、占用、延迟）；None 是指名的单条
    Proxy {
        url: String,
        via: Option<String>,
    },
}

/// 一次出站要试的路，有序。池绑定给到 [`MAX_ATTEMPTS`] 条供换路；点名与直连只有一条
#[derive(Debug)]
pub struct Plan {
    pending: VecDeque<Resolved>,
}

impl Plan {
    /// 交出下一条路并记上一次占用。None = 计划用尽
    pub fn next(&mut self) -> Option<Leg> {
        Some(begin(self.pending.pop_front()?))
    }

    /// 计划第一条要走的代理地址，不开始它：模型池目录的共享缓存键用它记"从哪条路出发"
    pub fn first_url(&self) -> Option<&str> {
        match self.pending.front()? {
            Resolved::Direct => None,
            Resolved::Proxy { url, .. } => Some(url.as_str()),
        }
    }
}

/// 一条正在试的路。占用在交出这一步时记下，[`Drop`] 里释放——
/// 收尾绑在守护上而不是靠调用方记得：清单、抓取这些消费点以前是挑完就把账忘了，
/// inflight 只涨不落，"最少使用"于是永远绕开被它们用过的那条
#[derive(Debug)]
pub struct Leg {
    route: Resolved,
    head_ms: Option<u64>,
    ttft_ms: Option<u64>,
    finished: bool,
}

impl Leg {
    /// 这一步真正要连的代理地址。None = 直连
    pub fn proxy_url(&self) -> Option<&str> {
        match &self.route {
            Resolved::Direct => None,
            Resolved::Proxy { url, .. } => Some(url.as_str()),
        }
    }

    /// 发出请求到拿到响应头。两条出口都有这个时刻，所以它是可比的判据
    pub fn note_head(&mut self, elapsed: Duration) {
        self.head_ms = Some(elapsed.as_millis() as u64);
    }

    /// 发出请求到首个 SSE 事件。只有流式出口量得到
    pub fn note_ttft(&mut self, elapsed: Duration) {
        self.ttft_ms = Some(elapsed.as_millis() as u64);
    }

    /// 收尾并记账。没调用它的话 [`Drop`] 按 `Neutral` 收尾
    pub fn finish(&mut self, outcome: Outcome) {
        if self.finished {
            return;
        }
        self.finished = true;
        record(
            &self.route,
            outcome,
            self.head_ms.take(),
            self.ttft_ms.take(),
        );
    }
}

impl Drop for Leg {
    fn drop(&mut self) {
        self.finish(Outcome::Neutral);
    }
}

fn begin(route: Resolved) -> Leg {
    if let Resolved::Proxy { via: Some(id), .. } = &route {
        let mut hub = hub()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = hub.entries.entry(id.clone()).or_default();
        entry.total += 1;
        entry.inflight += 1;
    }
    Leg {
        route,
        head_ms: None,
        ttft_ms: None,
        finished: false,
    }
}

fn record(route: &Resolved, outcome: Outcome, head_ms: Option<u64>, ttft_ms: Option<u64>) {
    let Resolved::Proxy { via: Some(id), .. } = route else {
        return;
    };
    let mut hub = hub()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let entry = hub.entries.entry(id.clone()).or_default();
    entry.inflight -= 1;
    if let Some(ms) = head_ms {
        entry.head.observe(ms);
    }
    if let Some(ms) = ttft_ms {
        entry.ttft.observe(ms);
    }
    match outcome {
        // 状态码是服务商的事：头都拿到了，说明这一发的路走通了
        Outcome::Reached => {
            entry.reached += 1;
            entry.failures = 0;
            entry.cooldown_until = None;
        }
        Outcome::Unreachable => {
            entry.unreachable += 1;
            entry.failures += 1;
            if entry.failures >= FAILURE_THRESHOLD {
                let shift = (entry.failures - FAILURE_THRESHOLD).min(16);
                let secs = COOLDOWN_BASE
                    .as_secs()
                    .saturating_mul(1u64 << shift)
                    .min(COOLDOWN_CAP.as_secs());
                entry.cooldown_until = Some(Instant::now() + Duration::from_secs(secs));
            }
        }
        // 掐流不进冷却：长思考模型的空闲超时是服务商与中转站的行为。攒着给人看
        Outcome::Interrupted => entry.interrupted += 1,
        Outcome::Neutral => {}
    }
}

/// 把 ureq 的一次失败归到该谁头上。拿到状态码 = 通路成立；
/// URL 写错、代理地址不合法、重定向这类是**我们自己的**毛病，不该冤枉代理
pub fn outcome_of(error: &ureq::Error) -> Outcome {
    match error {
        ureq::Error::StatusCode(_) => Outcome::Reached,
        ureq::Error::BadUri(_)
        | ureq::Error::InvalidProxyUrl
        | ureq::Error::Http(_)
        | ureq::Error::RedirectFailed
        | ureq::Error::BodyExceedsLimit(_)
        | ureq::Error::TooManyRedirects => Outcome::Neutral,
        // 到这里都是"一个头都没拿到"：连不上、超时、DNS、TLS、协议对不上
        _ => Outcome::Unreachable,
    }
}

// ---------------------------------------------------------------- 解析

pub const BINDING_DIRECT: &str = "direct";
pub const BINDING_POOL: &str = "pool";

/// 主机是不是本机自己。判据与 webhook 的回环豁免同一条（`hook::loopback`），
/// 不另写一份 URL 解析——两处解析就会对同一个地址给出两个答案
fn is_loopback(url: &str) -> bool {
    let host = crate::egress::strip_port(&crate::egress::host_of(url));
    matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

/// 绕过名单：按域后缀匹配（`example.com` 覆盖 `api.example.com`，不覆盖 `notexample.com`），
/// 与出口名单的匹配规则同一条
fn bypassed(url: &str, bypass: &[String]) -> bool {
    let host = crate::egress::host_of(url);
    bypass.iter().any(|rule| {
        let rule = rule.trim().to_ascii_lowercase();
        !rule.is_empty() && (host == rule || host.ends_with(&format!(".{rule}")))
    })
}

fn named_proxy(pool: &ProxyPool, id: &str) -> Result<Resolved, String> {
    let Some(entry) = pool.proxies.iter().find(|entry| entry.id == id) else {
        return Err(format!(
            "代理 id「{id}」已不存在（可能刚被删除）：这一发不静默直连——\
             指名代理多半是因为不走它就出不去，或不想暴露真实地址。\
             去设置 → 代理（或服务商档案）里换一个绑定。"
        ));
    };
    if !entry.enabled {
        return Err(format!(
            "代理「{}」已停用：这一发不静默直连。去设置 → 代理 里重新启用，或换一个绑定。",
            entry.name
        ));
    }
    Ok(Resolved::Proxy {
        url: entry.url.clone(),
        via: None,
    })
}

/// 池绑定的一次出口要试的有序路：第一条按策略挑，其余按配置顺序补在后面当换路候选。
/// 全都冷却时第一条是"最早结束的那个"，后面照样列出来——第 2、3 条就是它的替补
fn routes_from_pool(pool: &ProxyPool, max: usize) -> Result<Vec<Resolved>, String> {
    let candidates: Vec<&ProxyEntry> = pool
        .proxies
        .iter()
        .filter(|entry| entry.enabled && !entry.url.trim().is_empty())
        .collect();
    if candidates.is_empty() {
        return Err(
            "代理池里没有启用的代理：这一发不静默直连——绑定了池还直连，等于配置骗人。\
             去设置 → 代理 里启用至少一条。"
                .into(),
        );
    }
    let index = {
        let mut hub = hub()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pick_strategy(&mut hub, &pool.strategy, &candidates)
    };
    let mut ordered = vec![candidates[index]];
    ordered.extend(
        candidates
            .iter()
            .enumerate()
            .filter(|(other, _)| *other != index)
            .map(|(_, candidate)| *candidate)
            .take(max.saturating_sub(1)),
    );
    Ok(ordered
        .into_iter()
        .map(|entry| Resolved::Proxy {
            url: entry.url.clone(),
            via: Some(entry.id.clone()),
        })
        .collect())
}

/// 三级解析的本体，返回**有序的要试的路**。`url` 是这一发真正要连的目标
/// （本机回环与绕过名单按它判），Jev / webhook / 子进程没有服务商粒度，
/// `endpoint`/`model` 传 `None`
fn routes_for(
    pool: &ProxyPool,
    bypass: &[String],
    global: &str,
    endpoint: Option<&str>,
    model: Option<&str>,
    url: &str,
    max: usize,
) -> Result<Vec<Resolved>, String> {
    if is_loopback(url) || bypassed(url, bypass) {
        return Ok(vec![Resolved::Direct]);
    }
    // 三级链：按模型 → 服务商 → 全局。"" 是"继承上层"；链上全是继承 = 直连
    let bindings = [model, endpoint]
        .into_iter()
        .flatten()
        .chain(std::iter::once(global));
    for binding in bindings {
        match binding.trim() {
            "" => continue,
            BINDING_DIRECT => return Ok(vec![Resolved::Direct]),
            BINDING_POOL => return routes_from_pool(pool, max),
            id => return Ok(vec![named_proxy(pool, id)?]),
        }
    }
    Ok(vec![Resolved::Direct])
}

fn plan_for(
    config: &AppConfig,
    endpoint: Option<&str>,
    model: Option<&str>,
    url: &str,
) -> Result<Plan, String> {
    Ok(Plan {
        pending: VecDeque::from(routes_for(
            &config.proxy_pool,
            &config.proxy_bypass,
            &config.proxy_default,
            endpoint,
            model,
            url,
            MAX_ATTEMPTS,
        )?),
    })
}

/// 模型请求的换路计划：按模型覆盖 → 当前连接绑定 → 全局。模型名取 `config.model`——
/// 模型池/服务商覆盖在那之前已把这一发的连接域（含代理绑定）抄进顶层
pub fn plan(config: &AppConfig, url: &str) -> Result<Plan, String> {
    let model = config
        .proxy_by_model
        .get(config.model.as_str())
        .map(String::as_str);
    plan_for(config, Some(&config.proxy), model, url)
}

/// 模型清单的换路计划：按档案拉目录时没有"这一发的模型"，逐模型覆盖不参与
pub fn plan_profile(
    config: &AppConfig,
    profile: &EndpointProfile,
    url: &str,
) -> Result<Plan, String> {
    plan_for(config, Some(&profile.proxy), None, url)
}

/// 一条直连的计划：给"档案已被删除、没有连接绑定可依"这类调用方——
/// 它该照常拉得到目录，而不是因为找不到档案就报错
pub fn plan_direct() -> Plan {
    Plan {
        pending: VecDeque::from(vec![Resolved::Direct]),
    }
}

/// 只有全局层的单发裁决（链接抓取、决策层、任务 webhook）：只有一条路，
/// 返回的 [`Leg`] 活到那一发结束——占用由它负责释放
pub fn take_global(config: &AppConfig, url: &str) -> Result<Leg, String> {
    let routes = routes_for(
        &config.proxy_pool,
        &config.proxy_bypass,
        &config.proxy_default,
        None,
        None,
        url,
        1,
    )?;
    Ok(begin(routes.into_iter().next().expect("解析至少给一条路")))
}

// ---------------------------------------------------------------- Agent 与地址

/// 模型出口（POST / GET）共用的 Agent 构造。显式 `.proxy(None)` 把 ureq 默认
/// Agent 那份"读系统环境变量"的行为压掉：配了就走配置的，没配就直连，系统环境
/// 里那几只变量不再劫持模型流量。
///
/// Agent 按代理地址复用：每发新建一个就等于每一轮重新做一次 TCP + CONNECT + TLS，
/// 顺带把延迟读数抬高一个握手的时间。地址改了由 [`on_config_changed`] 逐条丢掉
pub fn agent_for(proxy: Option<&str>) -> Result<ureq::Agent, String> {
    agent_of(
        &mut hub()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        proxy,
    )
}

/// 本体拆出来只吃一份账本：缓存命中率这件事要在自己的 Hub 上测，
/// 而不是踩着全进程那一只
fn agent_of(hub: &mut Hub, proxy: Option<&str>) -> Result<ureq::Agent, String> {
    let key = proxy
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .unwrap_or("")
        .to_string();
    if let Some(agent) = hub.agents.get(&key) {
        return Ok(agent.clone());
    }
    let parsed = if key.is_empty() {
        None
    } else {
        Some(ureq::Proxy::new(&key).map_err(|error| format!("代理地址「{key}」不合法：{error}"))?)
    };
    let agent = ureq::Agent::config_builder()
        .proxy(parsed)
        .build()
        .new_agent();
    if hub.agents.len() >= AGENT_CACHE_MAX {
        hub.agents.clear();
    }
    hub.agents.insert(key, agent.clone());
    Ok(agent)
}

/// 代理地址的形状检查（设置页保存时给人话报错；ureq 的 `Proxy::new` 会再验一次）：
/// 协议前缀必带（http / https / socks5），主机必带，可带 `user:pass@`
pub fn parse_proxy_url(url: &str) -> Result<(), String> {
    let trimmed = url.trim();
    let Some((scheme, _rest)) = trimmed.split_once("://") else {
        return Err("代理地址要带协议前缀：http:// 或 socks5://".into());
    };
    match scheme.to_ascii_lowercase().as_str() {
        "http" | "https" | "socks5" | "socks" => {}
        other => {
            return Err(format!(
                "不支持的代理协议「{other}」：用 http:// 或 socks5://"
            ))
        }
    }
    let host = proxy_host_of(trimmed);
    if host.is_empty() || host.starts_with(':') {
        return Err("代理地址缺主机：形如 http://127.0.0.1:7890".into());
    }
    Ok(())
}

// ---------------------------------------------------------------- 配置变更

#[derive(Clone)]
struct ChildProxy {
    bypass: Vec<String>,
    pool: ProxyPool,
    global: String,
}

/// 子进程（MCP / 命令 / 钩子）的代理环境快照。由启动与 config_patch 刷新；
/// `constrained()` 的 child_env 在每次 spawn 时读它——池绑定因此天然在 spawn 间轮换
static CHILD_PROXY: std::sync::OnceLock<std::sync::Mutex<Option<ChildProxy>>> =
    std::sync::OnceLock::new();

/// 按新的配置丢掉已经不在场的东西：旧地址的连接池不该再被复用，被删代理的账也不该
/// 永远占着格子——但还在跑的那条（inflight > 0）留着，它的收尾还得落到同一个格子上
fn prune_to(hub: &mut Hub, config: &AppConfig) {
    let live_urls: Vec<&str> = config
        .proxy_pool
        .proxies
        .iter()
        .map(|entry| entry.url.trim())
        .filter(|url| !url.is_empty())
        .collect();
    let live_ids: Vec<&str> = config
        .proxy_pool
        .proxies
        .iter()
        .map(|entry| entry.id.as_str())
        .collect();
    hub.agents
        .retain(|url, _| live_urls.contains(&url.as_str()));
    hub.entries
        .retain(|id, entry| live_ids.contains(&id.as_str()) || entry.inflight > 0);
}

/// 配置写回之后要做的事：刷新子进程快照，并按新的配置清理连接池与账本
pub fn on_config_changed(config: &AppConfig) {
    refresh_child_proxy(config);
    prune_to(
        &mut hub()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        config,
    );
}

pub fn refresh_child_proxy(config: &AppConfig) {
    let cell = CHILD_PROXY.get_or_init(|| std::sync::Mutex::new(None));
    *cell
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ChildProxy {
        bypass: config.proxy_bypass.clone(),
        pool: config.proxy_pool.clone(),
        global: config.proxy_default.clone(),
    });
}

/// 子进程要额外带上的代理变量。系统继承的那几只由 `constrain::child_env` 摘掉，
/// 这里只补 aglab 配置说的那一份；NO_PROXY 恒含本机回环——sidecar 不被卷进代理。
///
/// 这一步挑路**不进账本**：子进程的 HTTP 结局 aglab 永远看不到，记一次占用就永远还不掉
pub fn child_proxy_env() -> Vec<(String, String)> {
    let Some(snapshot) = CHILD_PROXY
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
    else {
        return Vec::new();
    };
    // 子进程没有"这一发连到哪"的目标：bypass 判定不参与，本机豁免靠 NO_PROXY。
    // 绑定解析失败时子进程只能直连（没法让一次还没发生的请求报错），
    // 但要喊一声——静默地换了条路比失败更难查
    let resolved = match routes_for(&snapshot.pool, &[], &snapshot.global, None, None, "", 1) {
        Ok(mut routes) => routes.remove(0),
        Err(error) => {
            eprintln!("子进程的代理绑定解析失败，这条 spawn 直连：{error}");
            return Vec::new();
        }
    };
    let Resolved::Proxy { url, .. } = resolved else {
        return Vec::new();
    };
    let mut bypass_hosts = vec!["localhost", "127.0.0.1", "::1", "[::1]"];
    bypass_hosts.extend(
        snapshot
            .bypass
            .iter()
            .map(String::as_str)
            .filter(|rule| !rule.trim().is_empty()),
    );
    vec![
        ("HTTP_PROXY".into(), url.clone()),
        ("HTTPS_PROXY".into(), url.clone()),
        ("ALL_PROXY".into(), url.clone()),
        ("NO_PROXY".into(), bypass_hosts.join(",")),
    ]
}

/// 建窗前给 WebView2 的浏览器参数（代理是 WebView2 环境创建时的一次性决定，改了要重启）。
/// wry 的 `proxy_url` 不带旁路，所以参数整串自己拼：wry 默认那串 + `--proxy-server` +
/// `--proxy-bypass-list`（恒含本机——sidecar 与 inbound webhook 不能被卷进代理）。
/// 代理 URL 里的凭据剥掉：Chromium 的 `--proxy-server` 不吃 `user:pass@`，要认证时它自己弹窗。
/// 池绑定在启动时挑定一支——WebView 是单例，不逐请求轮换。与子进程同理：挑定即结束，
/// 渲染层的流量结局 aglab 拿不到，所以这一步不进账本
pub fn webview_browser_args(config: &AppConfig) -> String {
    let mut args = String::from("--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection");
    // 渲染层同样是"没法对一次还没发生的请求报错"的消费者：解析失败喊一声再直连
    let resolved = match routes_for(
        &config.proxy_pool,
        &config.proxy_bypass,
        &config.proxy_default,
        None,
        None,
        "",
        1,
    ) {
        Ok(mut routes) => routes.remove(0),
        Err(error) => {
            eprintln!("渲染层的代理绑定解析失败，这次启动直连：{error}");
            return args;
        }
    };
    if let Resolved::Proxy { url, .. } = resolved {
        // 剥凭据只剥 authority 里的 user:pass@，协议前缀要留着（Chromium 认它）
        let stripped = match url.trim().split_once("://") {
            Some((scheme, rest)) => {
                let host = rest.rsplit_once('@').map(|(_, host)| host).unwrap_or(rest);
                format!("{scheme}://{host}")
            }
            None => url.trim().to_string(),
        };
        args.push_str(&format!(" --proxy-server={stripped}"));
        let mut hosts = vec!["localhost", "127.0.0.1", "[::1]"];
        hosts.extend(
            config
                .proxy_bypass
                .iter()
                .map(String::as_str)
                .filter(|rule| !rule.trim().is_empty()),
        );
        args.push_str(&format!(" --proxy-bypass-list={}", hosts.join(";")));
    }
    args
}

// ---------------------------------------------------------------- 面板读数

/// 设置页一格代理读数：这条代理此刻的调度状态
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyStat {
    pub id: String,
    /// 被派出去过多少次（含换路里的每一次尝试）
    pub total: u64,
    pub inflight: u64,
    /// 连续连不上次数。一次通路成立清零
    pub failures: u32,
    /// 剩余冷却毫秒。0 = 没在冷却
    pub cooling_ms: u64,
    pub reached: u64,
    pub unreachable: u64,
    pub interrupted: u64,
    /// 响应头耗时 EWMA。None = 没量过或已过新鲜期
    pub head_ms: Option<u64>,
    /// 首字耗时 EWMA。同上
    pub ttft_ms: Option<u64>,
}

impl Hub {
    fn snapshot(&self, ids: &[String]) -> Vec<ProxyStat> {
        ids.iter()
            .map(|id| {
                let entry = self.entries.get(id).cloned().unwrap_or_default();
                ProxyStat {
                    id: id.clone(),
                    total: entry.total,
                    inflight: entry.inflight.max(0) as u64,
                    failures: entry.failures,
                    cooling_ms: cooldown_remaining(&entry.cooldown_until),
                    reached: entry.reached,
                    unreachable: entry.unreachable,
                    interrupted: entry.interrupted,
                    head_ms: entry.head.fresh().map(|mean| mean.round() as u64),
                    ttft_ms: entry.ttft.fresh().map(|mean| mean.round() as u64),
                }
            })
            .collect()
    }
}

/// 面板读数：按配置里代理的顺序给。配置里没有的代理不出现——面板只说配置内代理的事。
/// 也供出口那一侧的端到端测试读账（命令那一格只是它的 AppHandle 包装）
pub fn snapshot(config: &AppConfig) -> Vec<ProxyStat> {
    let ids: Vec<String> = config
        .proxy_pool
        .proxies
        .iter()
        .map(|entry| entry.id.clone())
        .collect();
    hub()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .snapshot(&ids)
}

#[tauri::command]
pub fn proxy_pool_stats(app: tauri::AppHandle) -> Result<Vec<ProxyStat>, String> {
    Ok(snapshot(&crate::config::load(&app)))
}

// ---------------------------------------------------------------- 测试连通性

/// 一次真探测的结局：note 是给人看的那句话（成功与失败都有）
struct Probe {
    ok: bool,
    ms: u64,
    note: String,
}

/// 一条「全部测试」的结果，按代理给
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyTestOutcome {
    pub id: String,
    pub name: String,
    pub ok: bool,
    pub ms: u64,
    pub note: String,
}

/// 批量测试的并发宽度：一条一个线程，一百条一起冲出去会先把自家代理打懵
const PROBE_CONCURRENCY: usize = 8;

/// 经一条代理 GET 一次活动服务商（不是任意外网——出口名单不为测试开洞）。
/// 任何 HTTP 状态码都算"通"（401 也是路通了），只有连接层失败才算不通。
///
/// 这一次是真尝试，所以**进账本**：手动「测试」是这台机器上唯一的半开探测手段——
/// 冷却中的代理被点一下测通，就该立刻回到池子里，而不是等冷却走完再拿真请求去赌
fn probe_through(entry: &ProxyEntry, target: &str) -> Probe {
    let host = crate::egress::host_of(target);
    let mut leg = begin(Resolved::Proxy {
        url: entry.url.clone(),
        via: Some(entry.id.clone()),
    });
    let agent = match agent_for(leg.proxy_url()) {
        Ok(agent) => agent,
        Err(error) => {
            leg.finish(Outcome::Neutral);
            return Probe {
                ok: false,
                ms: 0,
                note: error,
            };
        }
    };
    let started = Instant::now();
    let outcome = agent
        .get(target)
        .config()
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_global(Some(Duration::from_secs(20)))
        .build()
        .call();
    let elapsed = started.elapsed();
    leg.note_head(elapsed);
    let ms = elapsed.as_millis() as u64;
    match outcome {
        Ok(_) => {
            leg.finish(Outcome::Reached);
            Probe {
                ok: true,
                ms,
                note: format!("经「{}」到达 {host}：{ms}ms，代理通路正常。", entry.name),
            }
        }
        Err(error @ ureq::Error::StatusCode(code)) => {
            leg.finish(outcome_of(&error));
            Probe {
                ok: true,
                ms,
                note: format!(
                    "经「{}」到达 {host}：{ms}ms，服务商回了 HTTP {code}——路是通的（状态码是服务商的事，与代理无关）。",
                    entry.name
                ),
            }
        }
        Err(error) => {
            // 通没通与"该谁负责"是两件事：URL 写错这一发也是没成，只是不该记到代理头上
            leg.finish(outcome_of(&error));
            Probe {
                ok: false,
                ms,
                note: format!("经「{}」连不上 {host}：{error}", entry.name),
            }
        }
    }
}

/// 测的是"经它到活动服务商"那条通路：服务商没配就没有可测的目标，这一句要说清
fn probe_target(config: &AppConfig) -> Result<String, String> {
    if config.base_url.trim().is_empty() {
        return Err("当前服务商地址是空的：先在模型设置里配好服务商再测代理。".into());
    }
    Ok(config.base_url.clone())
}

#[tauri::command]
pub async fn proxy_test(app: tauri::AppHandle, proxy_id: String) -> Result<String, String> {
    let config = crate::config::load(&app);
    tauri::async_runtime::spawn_blocking(move || {
        let entry = config
            .proxy_pool
            .proxies
            .iter()
            .find(|entry| entry.id == proxy_id)
            .ok_or_else(|| format!("没有 id 为「{proxy_id}」的代理。"))?;
        let probe = probe_through(entry, &probe_target(&config)?);
        if probe.ok {
            Ok(probe.note)
        } else {
            Err(probe.note)
        }
    })
    .await
    .map_err(|error| format!("测试线程没能跑完：{error}"))?
}

/// 把池里每条**启用的**代理各测一次。每 `PROBE_CONCURRENCY` 条一批：批内并发、批间排队。
/// 结果逐条进账本，所以这一格同时是"把冷却中的代理立刻捞回池里"的那次半开探测
fn test_all_of(config: &AppConfig) -> Result<Vec<ProxyTestOutcome>, String> {
    let target = probe_target(config)?;
    let entries: Vec<&ProxyEntry> = config
        .proxy_pool
        .proxies
        .iter()
        .filter(|entry| entry.enabled && !entry.url.trim().is_empty())
        .collect();
    let mut out: Vec<ProxyTestOutcome> = Vec::with_capacity(entries.len());
    for chunk in entries.chunks(PROBE_CONCURRENCY) {
        std::thread::scope(|scope| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|entry| {
                    let entry = (*entry).clone();
                    let target = target.clone();
                    scope.spawn(move || {
                        let probe = probe_through(&entry, &target);
                        ProxyTestOutcome {
                            id: entry.id,
                            name: entry.name,
                            ok: probe.ok,
                            ms: probe.ms,
                            note: probe.note,
                        }
                    })
                })
                .collect();
            for (index, handle) in handles.into_iter().enumerate() {
                out.push(handle.join().unwrap_or_else(|_| ProxyTestOutcome {
                    id: chunk[index].id.clone(),
                    name: chunk[index].name.clone(),
                    ok: false,
                    ms: 0,
                    note: "测试线程自己崩了，这一条没有读数。".into(),
                }));
            }
        });
    }
    Ok(out)
}

#[tauri::command]
pub async fn proxy_pool_test_all(app: tauri::AppHandle) -> Result<Vec<ProxyTestOutcome>, String> {
    let config = crate::config::load(&app);
    tauri::async_runtime::spawn_blocking(move || test_all_of(&config))
        .await
        .map_err(|error| format!("测试线程没能跑完：{error}"))?
}

// ---------------------------------------------------------------- 批量导入

/// 批量导入里的一行：解析出来的地址与名字，加上"为什么没让它进池"
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ImportRow {
    pub url: String,
    pub name: String,
    /// None = 合格；Some = 被挡下的人话原因
    pub reason: Option<String>,
}

/// 一次最多导入多少条：设置页一屏一行地渲染、还带着 3 秒心跳读数，几百条会把那一页压死。
/// 真需要几百条的形状是订阅，aglab 不接（ureq 只吃 http/https/socks5）
const MAX_IMPORT: usize = 100;

/// 地址里的"主机:端口"那一段（凭据剥掉），用来起默认名字
fn proxy_host_of(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    authority
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(authority)
        .to_ascii_lowercase()
}

/// 去重用的规范化：协议与主机小写（DNS 本来就不区分大小写），**凭据与端口原样**——
/// 只有密码大小不同的两条是两个代理，不该并成一条
fn dedup_key(url: &str) -> String {
    let trimmed = url.trim();
    let Some((scheme, rest)) = trimmed.split_once("://") else {
        return trimmed.to_ascii_lowercase();
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let (credentials, host) = match authority.rsplit_once('@') {
        Some((user, host)) => (format!("{user}@"), host),
        None => (String::new(), authority),
    };
    format!(
        "{}://{}{}{}",
        scheme.to_ascii_lowercase(),
        credentials,
        host.to_ascii_lowercase(),
        tail
    )
}

/// 一行 → (地址, 名字)。名字可以是 `URL#名称` 或 `URL 名称`（订阅链接常用的那两种写法）
fn parse_import_line(line: &str) -> (String, Option<String>) {
    let line = line.trim();
    if let Some((head, rest)) = line.split_once(char::is_whitespace) {
        return (head.trim().to_string(), Some(rest.trim().to_string()));
    }
    match line.rsplit_once('#') {
        Some((head, tag)) if head.contains("://") => (head.to_string(), Some(tag.to_string())),
        _ => (line.to_string(), None),
    }
}

/// 解析 + 校验 + 去重。地址形状用的还是 `parse_proxy_url` 那一把尺，不另写一份；
/// 重复的判据是规范化地址——与池里已有的比，也与本批里先到的比
fn import_rows(text: &str, config: &AppConfig) -> Vec<ImportRow> {
    let mut taken: Vec<String> = config
        .proxy_pool
        .proxies
        .iter()
        .map(|entry| dedup_key(&entry.url))
        .collect();
    let mut rows = Vec::new();
    let mut accepted = 0usize;
    for line in text.lines() {
        let line = line.trim();
        // 空行与"整行是注释"（# 开头又没有协议前缀）跳过：那不是待导入的东西
        if line.is_empty() || (line.starts_with('#') && !line.contains("://")) {
            continue;
        }
        let (url, name) = parse_import_line(line);
        let reason = if url.is_empty() {
            Some("这一行没有地址。".to_string())
        } else if name.as_deref().is_some_and(|rest| rest.contains("://")) {
            Some("一行只能一条地址。".to_string())
        } else if let Err(error) = parse_proxy_url(&url) {
            Some(error)
        } else {
            let key = dedup_key(&url);
            if taken.contains(&key) {
                Some("池里已经有同一条地址。".to_string())
            } else if accepted >= MAX_IMPORT {
                Some(format!("一次最多导入 {MAX_IMPORT} 条，这一条没进。"))
            } else {
                taken.push(key);
                accepted += 1;
                None
            }
        };
        let name = name
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| proxy_host_of(&url));
        rows.push(ImportRow { url, name, reason });
    }
    rows
}

/// 批量导入：只答"这些行会怎么样"，**不写配置**——落盘仍走前端那一条 config_patch，
/// 于是"池里有哪些代理"这件事只有一个写入口
#[tauri::command]
pub fn proxy_import(app: tauri::AppHandle, text: String) -> Result<Vec<ImportRow>, String> {
    Ok(import_rows(&text, &crate::config::load(&app)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProxyEntry;

    fn entry(id: &str, url: &str, enabled: bool) -> ProxyEntry {
        ProxyEntry {
            id: id.into(),
            name: format!("代理{id}"),
            url: url.into(),
            enabled,
            weight: 1,
        }
    }

    fn pool(entries: Vec<ProxyEntry>) -> ProxyPool {
        ProxyPool {
            strategy: "round_robin".into(),
            proxies: entries,
        }
    }

    fn config_with(
        global: &str,
        endpoint: &str,
        model_map: Option<(&str, &str)>,
        proxies: ProxyPool,
    ) -> AppConfig {
        let mut config = AppConfig::default();
        config.proxy_default = global.into();
        config.proxy = endpoint.into();
        config.proxy_pool = proxies;
        config.model = "模型甲".into();
        if let Some((model, binding)) = model_map {
            config.proxy_by_model.insert(model.into(), binding.into());
        }
        config
    }

    /// 一条已开始的池路由：占用记上了，收尾由 Drop 兜住
    fn pooled_leg(id: &str) -> Leg {
        begin(Resolved::Proxy {
            url: format!("http://127.0.0.1/{id}"),
            via: Some(id.into()),
        })
    }

    /// 走生产那条路（换路计划取第一条），测试不另开一条只有测试在用的解析入口
    fn first_leg(config: &AppConfig, url: &str) -> Leg {
        plan(config, url)
            .expect("解析得出计划")
            .next()
            .expect("计划至少给一条路")
    }

    /// 模拟"经这条代理跑了一发并回报归因"：与真实出口一样先占后放
    fn settle(id: &str, outcome: Outcome) {
        pooled_leg(id).finish(outcome);
    }

    fn entry_of(id: &str) -> Entry {
        hub()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entries
            .get(id)
            .cloned()
            .unwrap_or_default()
    }

    /// 三级链：按模型覆盖 > 服务商 > 全局；"" 是继承，链上全是继承就是直连
    #[test]
    fn bindings_resolve_through_the_three_layers_in_order() {
        let proxies = pool(vec![
            entry("p1", "http://127.0.0.1:1", true),
            entry("p2", "http://127.0.0.1:2", true),
        ]);
        // 全局直连
        let config = config_with("", "", None, proxies.clone());
        assert_eq!(
            first_leg(&config, "https://api.example.test/v1").proxy_url(),
            None
        );

        // 全局点名 → 服务商继承 → 生效的是全局那条
        let config = config_with("p1", "", None, proxies.clone());
        assert_eq!(
            first_leg(&config, "https://api.example.test/v1").proxy_url(),
            Some("http://127.0.0.1:1")
        );

        // 服务商覆盖全局
        let config = config_with("p1", "direct", None, proxies.clone());
        assert_eq!(
            first_leg(&config, "https://api.example.test/v1").proxy_url(),
            None
        );

        // 按模型覆盖服务商：只有点名的那只模型走
        let config = config_with("p1", "direct", Some(("模型甲", "p2")), proxies);
        assert_eq!(
            first_leg(&config, "https://api.example.test/v1").proxy_url(),
            Some("http://127.0.0.1:2")
        );
    }

    /// 绑定 "pool" 时由池挑：三条代理轮着来（round_robin 等权即顺序轮换），且带 via 记账
    #[test]
    fn a_pool_binding_rotates_across_enabled_proxies() {
        // 独立 id：池账本是全进程单例，别的测试动过同键条目就是共享状态
        let proxies = pool(vec![
            entry("rot-1", "http://127.0.0.1:1", true),
            entry("rot-2", "http://127.0.0.1:2", true),
            entry("rot-3", "http://127.0.0.1:3", true),
        ]);
        let config = config_with("pool", "", None, proxies);
        let urls: Vec<String> = (0..3)
            .map(|_| {
                let leg = first_leg(&config, "https://api.example.test/v1");
                assert!(
                    matches!(leg.route, Resolved::Proxy { via: Some(_), .. }),
                    "池挑出来的要带 id 记账"
                );
                leg.proxy_url().unwrap().to_string()
            })
            .collect();
        let unique: std::collections::BTreeSet<&String> = urls.iter().collect();
        assert_eq!(unique.len(), 3, "三条代理都要轮到：{urls:?}");
    }

    /// 回环与绕过名单恒直连——在三级链**之前**判，服务商配了代理也压不过本机豁免
    #[test]
    fn loopback_and_bypassed_hosts_go_direct_regardless_of_bindings() {
        let proxies = pool(vec![entry("cool-1", "http://127.0.0.1:1", true)]);
        let mut config = config_with("pool", "pool", None, proxies);
        config.proxy_bypass = vec!["example.test".into()];
        assert_eq!(
            first_leg(&config, "http://127.0.0.1:8787/health").proxy_url(),
            None
        );
        assert_eq!(
            first_leg(&config, "https://api.example.test/v1").proxy_url(),
            None
        );
        assert!(
            plan(&config, "https://other.example.test/v1").is_ok(),
            "名单外照常走绑定"
        );
    }

    /// 点名的代理停用/被删 → 报错，绝不静默直连：指名它就是为了不暴露真实地址
    #[test]
    fn a_missing_or_disabled_proxy_fails_the_request_loudly() {
        // 独立 id：不与冷却测试共享全局账本的键
        let proxies = pool(vec![entry("miss-1", "http://127.0.0.1:1", false)]);
        let config = config_with("miss-1", "", None, proxies);
        let error = plan(&config, "https://api.example.test/v1").unwrap_err();
        assert!(error.contains("停用"), "{error}");
        assert!(error.contains("不静默直连"), "{error}");

        let config = config_with("幽灵", "", None, pool(vec![]));
        let error = plan(&config, "https://api.example.test/v1").unwrap_err();
        assert!(error.contains("不存在"), "{error}");

        // 池绑定但没有启用的代理：同一句话术
        let config = config_with(
            "pool",
            "",
            None,
            pool(vec![entry("miss-2", "http://127.0.0.1:1", false)]),
        );
        let error = plan(&config, "https://api.example.test/v1").unwrap_err();
        assert!(error.contains("没有启用"), "{error}");
    }

    /// 池的失败冷却：连**错向** 3 次进冷却，冷却中的被排开；通路成立清零
    #[test]
    fn a_pool_proxy_cools_down_after_repeated_failures_and_recovers_on_success() {
        let proxies = pool(vec![
            entry("cool-1", "http://127.0.0.1:1", true),
            entry("cool-2", "http://127.0.0.1:2", true),
        ]);
        let config = config_with("pool", "", None, proxies);

        let first = first_leg(&config, "https://api.example.test/v1");
        let first_id = match &first.route {
            Resolved::Proxy { via: Some(id), .. } => id.clone(),
            other => panic!("{other:?}"),
        };
        // 第一次连不上（1 < 3）不进冷却
        settle(&first_id, Outcome::Unreachable);
        settle(&first_id, Outcome::Reached);
        assert!(is_fresh(
            &hub()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            &first_id
        ));

        // 连不上 3 次 → 冷却 → 下一次挑选换另一支
        for _ in 0..3 {
            settle(&first_id, Outcome::Unreachable);
        }
        assert!(
            !is_fresh(
                &hub()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                &first_id
            ),
            "3 连不上该进冷却"
        );
        let next = first_leg(&config, "https://api.example.test/v1");
        let next_id = match &next.route {
            Resolved::Proxy { via: Some(id), .. } => id.clone(),
            other => panic!("{other:?}"),
        };
        assert_ne!(next_id, first_id, "冷却中的那支不该再被挑到");
        settle(&next_id, Outcome::Reached);
    }

    /// 归因（本次改造的核心）：服务商回状态码、用户停止、名单拦截都不能让代理进冷却；
    /// 只有"一个头都没拿到"才攒 failures
    #[test]
    fn only_a_route_that_never_delivered_a_head_counts_against_the_proxy() {
        let dying = "attr-dying";
        // 先攒 2 次连不上——差一次就进冷却
        settle(dying, Outcome::Unreachable);
        settle(dying, Outcome::Unreachable);
        assert_eq!(entry_of(dying).failures, 2);

        // 掐流 5 次：不进冷却，只进读数（那是服务商/中转站的空闲超时）
        for _ in 0..5 {
            settle(dying, Outcome::Interrupted);
        }
        let after_stalls = entry_of(dying);
        assert_eq!(after_stalls.failures, 2, "掐流不该替服务商受罚");
        assert_eq!(after_stalls.interrupted, 5);

        // 停止与本地拦截：中性，三类归因一个都不碰，但占用照常放回零
        settle(dying, Outcome::Neutral);
        settle(dying, Outcome::Neutral);
        let after_stops = entry_of(dying);
        assert_eq!(after_stops.failures, 2, "中性收尾不碰冷却判据");
        assert_eq!(
            (
                after_stops.reached,
                after_stops.unreachable,
                after_stops.interrupted
            ),
            (0, 2, 5),
            "中性也不该攒任何归因读数"
        );
        assert_eq!(after_stops.inflight, 0, "没有结局的那几步也要把占用放回零");
        assert_eq!(
            after_stops.total, 9,
            "total 记上路过的次数：2 连不上 + 5 掐流 + 2 中性"
        );

        // 服务商回了 429：头拿到了 = 通路成立 → failures 清零，也不会被冷却
        settle(dying, Outcome::Reached);
        let settled = entry_of(dying);
        assert_eq!(settled.failures, 0, "状态码是服务商的事");
        assert_eq!(settled.reached, 1);
        assert_eq!(settled.cooldown_until, None);
        // 补上第 3 次连不上：这才该进冷却
        settle(dying, Outcome::Unreachable);
        settle(dying, Outcome::Unreachable);
        settle(dying, Outcome::Unreachable);
        assert!(
            entry_of(dying).cooldown_until.is_some(),
            "3 次连不上该关进冷却"
        );
    }

    /// 池里没有启用的代理时，池绑定是一句说得出原因的错，不是直连
    #[test]
    fn an_empty_pool_says_so_instead_of_going_direct() {
        let config = config_with("pool", "", None, pool(vec![]));
        let error = plan(&config, "https://api.example.test/v1").unwrap_err();
        assert!(error.contains("没有启用"), "{error}");
    }

    /// 换路合同：池绑定给到最多 MAX_ATTEMPTS 条且互不重复；点名与直连各只有一条——
    /// 换路是池的语义，指名一条代理意味着"别给我绕"
    #[test]
    fn a_pool_plan_offers_distinct_routes_while_naming_offers_exactly_one() {
        let proxies = pool(vec![
            entry("plan-1", "http://127.0.0.1:1", true),
            entry("plan-2", "http://127.0.0.1:2", true),
            entry("plan-3", "http://127.0.0.1:3", true),
            entry("plan-4", "http://127.0.0.1:4", false),
        ]);
        let config = config_with("pool", "", None, proxies.clone());
        let urls: Vec<String> = {
            let mut plan = plan(&config, "https://api.example.test/v1").unwrap();
            let mut out = Vec::new();
            while let Some(leg) = plan.next() {
                out.push(leg.proxy_url().unwrap().to_string());
            }
            out
        };
        assert_eq!(
            urls.len(),
            MAX_ATTEMPTS,
            "池计划最多 {MAX_ATTEMPTS} 条：{urls:?}"
        );
        let unique: std::collections::BTreeSet<&String> = urls.iter().collect();
        assert_eq!(
            unique.len(),
            urls.len(),
            "同一条代理不该在一份计划里出现两次：{urls:?}"
        );
        assert!(
            urls.iter().all(|url| !url.ends_with(":4")),
            "停用的那条不能进计划：{urls:?}"
        );

        // 点名只有一条
        let config = config_with("plan-1", "", None, proxies.clone());
        let mut named = plan(&config, "https://api.example.test/v1").unwrap();
        assert_eq!(
            named
                .next()
                .map(|leg| leg.proxy_url().unwrap().to_string())
                .as_deref(),
            Some("http://127.0.0.1:1")
        );
        assert!(named.next().is_none(), "点名的那条之外不该再换路");

        // 直连也只有一条
        let config = config_with("direct", "", None, proxies);
        let mut direct = plan(&config, "https://api.example.test/v1").unwrap();
        assert_eq!(
            direct.next().map(|leg| leg.proxy_url().map(str::to_string)),
            Some(None)
        );
        assert!(direct.next().is_none(), "直连没有第二条路");
    }

    /// 占用配对：交出一步就记 inflight，调用方忘了 finish 也要由 Drop 落回零，
    /// 且这份"没有结局"不记成任何归因。清单/抓取/决策这些消费点以前只记不放，
    /// inflight 只涨不落，"最少使用"就永远绕开被它们用过的那条
    #[test]
    fn a_dropped_leg_releases_its_occupancy_and_blames_nobody() {
        let id = "drop-1";
        {
            let leg = pooled_leg(id);
            assert_eq!(entry_of(id).inflight, 1);
            assert_eq!(entry_of(id).total, 1, "上路那一步就记一次派单");
            drop(leg); // 没有 finish：这一步该由守护按「没有结局」放掉占用
        }
        let after = entry_of(id);
        assert_eq!(after.inflight, 0, "Drop 必须把占用放回去");
        assert_eq!(after.total, 1, "上路一次记一次，与有没有收尾无关");
        assert_eq!(after.failures, 0, "没有结局就不该有归因");
        assert_eq!(after.reached + after.unreachable + after.interrupted, 0);
        assert_eq!(after.cooldown_until, None);
    }

    /// 地址形状检查：协议前缀必带、协议认得出、主机必带；凭据段不掺判
    #[test]
    fn proxy_urls_are_shape_checked_before_saving() {
        assert!(parse_proxy_url("http://127.0.0.1:7890").is_ok());
        assert!(parse_proxy_url("socks5://user:pass@proxy.example.test:1080").is_ok());
        assert!(parse_proxy_url("127.0.0.1:7890").is_err(), "没有协议前缀");
        assert!(parse_proxy_url("ftp://127.0.0.1:21").is_err(), "协议不认");
        assert!(parse_proxy_url("http://").is_err(), "没有主机");
    }

    /// agent_for 的两个半边：带代理走 Proxy::new（不合法要说出为什么），不带代理
    /// 也要**显式**压掉 ureq 默认那份"读系统环境变量"的行为
    #[test]
    fn the_agent_is_built_explicitly_so_system_proxy_env_never_hijacks() {
        assert!(agent_for(None).is_ok());
        assert!(agent_for(Some("http://127.0.0.1:7890")).is_ok());
        let error = agent_for(Some("::::")).unwrap_err();
        assert!(error.contains("不合法"), "{error}");
    }

    /// Agent 按地址复用：同一地址两次只建一次（也就是只有一份连接池），
    /// 换个地址才另开一格
    #[test]
    fn agents_are_reused_per_proxy_url() {
        let mut local = Hub::default();
        let url = "http://127.0.0.1:17890";
        assert!(agent_of(&mut local, Some(url)).is_ok());
        assert!(agent_of(&mut local, Some(url)).is_ok());
        assert_eq!(
            local.agents.len(),
            1,
            "同一地址该拿回同一个 Agent，而不是每发重建连接池"
        );
        assert!(agent_of(&mut local, Some("http://127.0.0.1:17891")).is_ok());
        assert_eq!(local.agents.len(), 2, "不同地址各一份");
        assert!(agent_of(&mut local, None).is_ok());
        assert_eq!(
            local.agents.len(),
            3,
            "直连也占一格：它同样是可复用的连接池"
        );
    }

    /// 配置变更后的清理：离场地址的连接池与被删代理的账都要掉，但还在跑的那条留着
    /// （它的收尾还得落在同一个格子上）。在自己的账本上测——全局那只会被并发测试踩
    #[test]
    fn pruning_drops_gone_urls_and_gone_proxies_but_keeps_the_busy_one() {
        let mut local = Hub::default();
        local.entries.insert("gone".into(), Entry::default());
        local.entries.insert(
            "busy".into(),
            Entry {
                inflight: 1,
                ..Default::default()
            },
        );
        local.agents.insert(
            "http://127.0.0.1:19001".into(),
            ureq::Agent::config_builder().build().new_agent(),
        );
        let mut config = AppConfig::default();
        let mut kept = entry("busy", "http://127.0.0.1:19002", true);
        kept.weight = 0; // 顺手确认默认构造出来的那条不会被当成"不存在"
        config.proxy_pool.proxies = vec![kept];
        prune_to(&mut local, &config);
        assert!(
            !local.entries.contains_key("gone"),
            "被删的代理不该永远占着格子"
        );
        assert!(
            local.entries.contains_key("busy"),
            "还有占用 in flight 的不能被清掉"
        );
        assert!(
            !local.agents.contains_key("http://127.0.0.1:19001"),
            "离场地址的 Agent 要被丢掉"
        );
        assert_eq!(
            weight_of(&config.proxy_pool.proxies[0]),
            1,
            "权重 0 归到最小档"
        );
    }

    /// 子进程环境：直连快照不给变量；代理快照给全四只，且 NO_PROXY 恒含本机回环。
    /// 全局绑定是**池里的代理 id**，不是裸 URL——地址只住在池里一处
    #[test]
    fn child_proxy_env_carries_the_global_binding_with_loopback_exempt() {
        let mut config = AppConfig::default();
        config.proxy_pool.proxies = vec![entry("p1", "http://127.0.0.1:7890", true)];
        refresh_child_proxy(&config);
        assert!(
            child_proxy_env().is_empty(),
            "直连就是不给子进程任何代理变量"
        );

        config.proxy_default = "p1".into();
        config.proxy_bypass = vec!["example.test".into()];
        refresh_child_proxy(&config);
        let env = child_proxy_env();
        let get = |name: &str| {
            env.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
                .expect("该有这一只")
        };
        assert_eq!(get("HTTP_PROXY"), "http://127.0.0.1:7890");
        assert_eq!(get("HTTPS_PROXY"), "http://127.0.0.1:7890");
        let no_proxy = get("NO_PROXY");
        for host in ["localhost", "127.0.0.1", "::1", "example.test"] {
            assert!(no_proxy.contains(host), "NO_PROXY 要含 {host}：{no_proxy}");
        }
    }

    /// 渲染层参数：不代理时就是 wry 默认那串；代理时追加 server 与 bypass（凭据剥掉）
    #[test]
    fn webview_args_carry_the_proxy_and_an_implicit_loopback_bypass() {
        let mut config = AppConfig::default();
        let args = webview_browser_args(&config);
        assert!(
            args.starts_with("--disable-features="),
            "wry 默认那串要保住：{args}"
        );
        assert!(
            !args.contains("proxy-server"),
            "没配代理就不该有 proxy-server：{args}"
        );

        config.proxy_pool.proxies = vec![entry("p1", "http://user:secret@127.0.0.1:7890", true)];
        config.proxy_default = "p1".into();
        let args = webview_browser_args(&config);
        assert!(
            args.contains("--proxy-server=http://127.0.0.1:7890"),
            "{args}"
        );
        assert!(!args.contains("secret"), "凭据不进浏览器参数：{args}");
        assert!(args.contains("--proxy-bypass-list=localhost"), "{args}");
    }

    /// 权重落地：轮询按权重分配（3:1 的四轮里重的该占三格），随机档也要真的偏过去。
    /// 以前 smooth_wrr 硬写 weight=1、weighted_random 连权重都不看（名不副实的均匀随机）
    #[test]
    fn weights_bias_both_the_round_robin_and_the_random_draw() {
        let mut heavy = entry("w-heavy", "http://127.0.0.1:1", true);
        heavy.weight = 3;
        let light = entry("w-light", "http://127.0.0.1:2", true);
        let config = config_with("pool", "", None, pool(vec![heavy, light]));

        let mut counts = std::collections::BTreeMap::new();
        for _ in 0..4 {
            let leg = first_leg(&config, "https://api.example.test/v1");
            let id = match &leg.route {
                Resolved::Proxy { via: Some(id), .. } => id.clone(),
                other => panic!("{other:?}"),
            };
            *counts.entry(id).or_insert(0) += 1;
        }
        assert_eq!(
            counts.get("w-heavy"),
            Some(&3),
            "3:1 权重的一轮该是 3 与 1：{counts:?}"
        );
        assert_eq!(counts.get("w-light"), Some(&1));

        let mut random = config_with(
            "pool",
            "",
            None,
            pool(vec![
                {
                    let mut e = entry("r-heavy", "http://127.0.0.1:1", true);
                    e.weight = 9;
                    e
                },
                {
                    let mut e = entry("r-light", "http://127.0.0.1:2", true);
                    e.weight = 1;
                    e
                },
            ]),
        );
        random.proxy_pool.strategy = "random".into();
        let mut drawn = std::collections::BTreeMap::new();
        for _ in 0..60 {
            let leg = first_leg(&random, "https://api.example.test/v1");
            let id = match &leg.route {
                Resolved::Proxy { via: Some(id), .. } => id.clone(),
                other => panic!("{other:?}"),
            };
            *drawn.entry(id).or_insert(0) += 1;
        }
        assert!(
            drawn.get("r-heavy").copied().unwrap_or_default()
                > drawn.get("r-light").copied().unwrap_or_default(),
            "权重 9:1 的随机档该明显偏过去：{drawn:?}"
        );
    }

    /// 权重 0 与超界在读侧被钳进值域——钳位只住在读侧这一处，写侧不管
    #[test]
    fn weights_are_clamped_where_they_are_read() {
        let mut zero = entry("clamp-0", "http://127.0.0.1:1", true);
        zero.weight = 0;
        assert_eq!(weight_of(&zero), 1, "权重 0 不能变成「永不被挑」");

        let mut huge = entry("clamp-huge", "http://127.0.0.1:1", true);
        huge.weight = u32::MAX;
        assert_eq!(weight_of(&huge), WEIGHT_MAX as i64);
    }

    /// 自适应档的可比性规则：没量过的代理先量一次，不会被"有样本"压死；
    /// 都有样本时才比 (并发+1)×头耗时
    #[test]
    fn the_adaptive_strategy_measures_everyone_before_it_ranks_them() {
        let slow = "ad-slow";
        let fast = "ad-fast";
        let fresh = "ad-fresh";
        settle(slow, Outcome::Reached);
        settle(fast, Outcome::Reached);
        {
            let mut hub = hub()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            hub.entries.get_mut(slow).unwrap().head.observe(900);
            hub.entries.get_mut(fast).unwrap().head.observe(100);
        }
        let mut entries = Vec::new();
        for (id, url) in [
            (slow, "http://127.0.0.1:1"),
            (fast, "http://127.0.0.1:2"),
            (fresh, "http://127.0.0.1:3"),
        ] {
            entries.push(entry(id, url, true));
        }
        let mut config = config_with("pool", "", None, pool(entries));
        config.proxy_pool.strategy = "adaptive".into();

        // 第三条还没量过：它该先被派一次，而不是拿 0 去跟别人比
        let first = first_leg(&config, "https://api.example.test/v1");
        assert!(
            matches!(&first.route, Resolved::Proxy { via: Some(id), .. } if id == fresh),
            "没量过的要先量：{first:?}"
        );
        drop(first);
        settle(fresh, Outcome::Reached);
        {
            let mut hub = hub()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            hub.entries.get_mut(fresh).unwrap().head.observe(500);
        }

        // 三条都有样本了：走最快的那条
        let picks: Vec<String> = (0..3)
            .map(|_| {
                let leg = first_leg(&config, "https://api.example.test/v1");
                match &leg.route {
                    Resolved::Proxy { via: Some(id), .. } => id.clone(),
                    other => panic!("{other:?}"),
                }
            })
            .collect();
        assert!(
            picks.iter().all(|id| id == fast),
            "都有样本时该挑最快的：{picks:?}"
        );
    }

    /// 过期的样本不再参与判定：新鲜度窗外 = 视同没量过
    #[test]
    fn a_stale_latency_sample_stops_counting() {
        let id = "stale-1";
        settle(id, Outcome::Reached);
        {
            let mut hub = hub()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let entry = hub.entries.get_mut(id).unwrap();
            entry.head.observe(120);
            entry.head.updated = Some(Instant::now() - LATENCY_WINDOW * 2);
        }
        assert!(
            entry_of(id).head.fresh().is_none(),
            "过窗的读数要答「没量过」"
        );
        let shown = &hub()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot(&[id.into()])[0];
        assert_eq!(shown.head_ms, None, "面板也不能把过期的读数当成现在的快慢");
    }

    /// 快照：面板那一格看到的形状——冷却倒计时、占用、两个延迟数
    #[test]
    fn the_snapshot_reports_what_the_panel_shows() {
        let id = "snap-1";
        let mut leg = begin(Resolved::Proxy {
            url: "http://127.0.0.1:1".into(),
            via: Some(id.into()),
        });
        leg.note_head(Duration::from_millis(42));
        leg.note_ttft(Duration::from_millis(310));
        leg.finish(Outcome::Reached);
        for _ in 0..FAILURE_THRESHOLD {
            settle(id, Outcome::Unreachable);
        }
        let stats = hub()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot(&[id.into(), "snap-absent".into()]);
        assert_eq!(stats.len(), 2);
        let shown = &stats[0];
        assert_eq!(shown.head_ms, Some(42), "拿到的头耗时要进读数");
        assert_eq!(shown.ttft_ms, Some(310));
        assert_eq!(shown.inflight, 0);
        assert!(
            shown.cooling_ms > 0,
            "冷却中的那条要说得出还剩多久：{shown:?}"
        );
        assert_eq!(shown.unreachable, FAILURE_THRESHOLD as u64);
        // 配置里有、账本里还没有的代理：给一格全零，而不是缺席
        assert_eq!(stats[1].total, 0);
        assert_eq!(stats[1].head_ms, None);
        assert_eq!(stats[1].cooling_ms, 0);
    }

    /// ureq 的错误种类归到该谁头上：我们自己的毛病不能记到代理账上
    #[test]
    fn our_own_mistakes_are_not_charged_to_the_proxy() {
        assert_eq!(outcome_of(&ureq::Error::StatusCode(429)), Outcome::Reached);
        assert_eq!(
            outcome_of(&ureq::Error::BadUri("no scheme".into())),
            Outcome::Neutral
        );
        assert_eq!(outcome_of(&ureq::Error::InvalidProxyUrl), Outcome::Neutral);
        assert_eq!(
            outcome_of(&ureq::Error::Io(std::io::Error::other("refused"))),
            Outcome::Unreachable
        );
        assert_eq!(outcome_of(&ureq::Error::HostNotFound), Outcome::Unreachable);
    }

    /// 读数的 IPC 形状：设置页那格直接吃 ProxyStat，后端少一个字段就是界面上
    /// 静默一格 undefined（编译器和 tsc 都不会响）
    #[test]
    fn the_proxy_stat_payload_matches_the_frontend_type() {
        let value = serde_json::to_value(ProxyStat {
            id: "shape-1".into(),
            total: 0,
            inflight: 0,
            failures: 0,
            cooling_ms: 0,
            reached: 0,
            unreachable: 0,
            interrupted: 0,
            head_ms: None,
            ttft_ms: None,
        })
        .expect("读数序列化得出去");
        crate::test_support::assert_matches_ts(&value, "ProxyStat");
    }

    /// 去重键：主机大小写并成一条（DNS 本来就不区分），凭据原样——
    /// 只有密码大小不同的两条是两个代理，不该被并掉
    #[test]
    fn the_dedup_key_folds_host_case_but_not_credentials() {
        assert_eq!(
            dedup_key("HTTP://API.Example.test:7890"),
            dedup_key("http://api.example.test:7890")
        );
        assert_ne!(
            dedup_key("http://u:Pass@h.test:1"),
            dedup_key("http://u:pass@h.test:1"),
            "密码大小写不同就是两个代理"
        );
        assert_ne!(
            dedup_key("http://h.test:1"),
            dedup_key("http://h.test:2"),
            "端口不能并掉"
        );
        assert_eq!(
            proxy_host_of("http://u:pass@API.Example.test:7890"),
            "api.example.test:7890"
        );
    }

    /// 批量粘贴的解析：一行一条、认 `#名称` 与空格名称、与池里已有的去重、
    /// 批内也去重、形状不对的要说得出为什么、注释与空行不算一行
    #[test]
    fn a_pasted_batch_is_parsed_validated_and_deduplicated() {
        let mut config = AppConfig::default();
        config.proxy_pool.proxies = vec![entry("have", "http://127.0.0.1:7890", true)];
        let text = "
# 这一行是注释，不是地址
http://127.0.0.1:7890
http://127.0.0.1:7891#东京
  http://127.0.0.1:7892 新加坡
HTTP://127.0.0.1:7890
http://127.0.0.1:7893
127.0.0.1:7894
http://a.test http://b.test
";
        let rows = import_rows(text, &config);
        assert_eq!(rows.len(), 7, "空行与注释行不算一格：{rows:?}");
        let accepted: Vec<&ImportRow> = rows.iter().filter(|row| row.reason.is_none()).collect();
        assert_eq!(accepted.len(), 3, "只有那三条该进池：{rows:?}");
        assert_eq!(accepted[0].name, "东京", "{accepted:?}");
        assert_eq!(accepted[1].name, "新加坡");
        assert_eq!(accepted[2].name, "127.0.0.1:7893", "没给名字就用主机:端口");
        assert!(
            rows[0]
                .reason
                .as_deref()
                .is_some_and(|r| r.contains("已经有")),
            "与池里已有的重复：{:?}",
            rows[0]
        );
        assert!(
            rows[3]
                .reason
                .as_deref()
                .is_some_and(|r| r.contains("已经有")),
            "大小写不同的同一条也算重复：{:?}",
            rows[3]
        );
        assert!(
            rows[5]
                .reason
                .as_deref()
                .is_some_and(|r| r.contains("协议前缀")),
            "{:?}",
            rows[5]
        );
        assert!(
            rows[6]
                .reason
                .as_deref()
                .is_some_and(|r| r.contains("一行只能一条")),
            "一行塞两条不该猜：{:?}",
            rows[6]
        );
    }

    /// 一次导入有上限：设置页一屏一行地渲染还带着 3 秒心跳，几百条会把那一页压死
    #[test]
    fn a_pasted_batch_stops_at_the_import_cap() {
        let text: String = (0..=MAX_IMPORT)
            .map(|index| format!("http://127.0.0.1:{}", 20000 + index))
            .collect::<Vec<_>>()
            .join("\n");
        let rows = import_rows(&text, &AppConfig::default());
        assert_eq!(rows.len(), MAX_IMPORT + 1);
        assert_eq!(
            rows.iter().filter(|row| row.reason.is_none()).count(),
            MAX_IMPORT,
            "封顶那么多条"
        );
        assert!(
            rows[MAX_IMPORT]
                .reason
                .as_deref()
                .is_some_and(|r| r.contains("最多")),
            "多出来的那条要说清为什么没进"
        );
    }

    /// 「全部测试」是真探测，而且每条的结果都进账本：冷却中的代理被测通要**立刻**回到池里
    /// ——这是这台机器上唯一的半开探测手段。停用的那条不测
    #[test]
    fn testing_them_all_probes_each_enabled_proxy_and_clears_a_cool_down_that_passes() {
        let live = crate::test_support::fake_http::proxy(200);
        let mut config = AppConfig::default();
        config.base_url = "http://aglab-probe.test/v1".into();
        config.proxy_pool.proxies = vec![
            entry("all-dead", "http://127.0.0.1:1", true),
            entry("all-live", &live, true),
            entry("all-off", "http://127.0.0.1:2", false),
        ];
        // 前置：把活的那条打进冷却（不然"测通即回池"这件事没被问到）
        for _ in 0..FAILURE_THRESHOLD {
            settle("all-live", Outcome::Unreachable);
        }
        assert!(
            !is_fresh(
                &hub()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                "all-live"
            ),
            "前置条件没成立：它现在不在冷却里"
        );

        let outcomes = test_all_of(&config).expect("这一批该测完");
        assert_eq!(outcomes.len(), 2, "停用的那条不该被测：{outcomes:?}");
        let dead = outcomes
            .iter()
            .find(|outcome| outcome.id == "all-dead")
            .expect("有死那条的读数");
        let alive = outcomes
            .iter()
            .find(|outcome| outcome.id == "all-live")
            .expect("有活那条的读数");
        assert!(!dead.ok, "没人听的那个端口该报连不上：{dead:?}");
        // 耗时别断言 ms > 0：耗时按整毫秒计，这条隐含"回环连接也要 1ms+"——那是
        // O0 时代的假设。dev 依赖升到 O2 后（profile.dev.package."*"），回环探测
        // 快于 1ms，0ms 是诚实读数；"给出耗时"由 note 里那截读数体现
        assert!(
            alive.ok && alive.note.contains("代理通路正常"),
            "活着那条该通并给出读数：{alive:?}"
        );

        let cooled = entry_of("all-live");
        assert_eq!(
            cooled.cooldown_until, None,
            "测通了就立刻回池，不必等冷却走完"
        );
        assert_eq!(
            cooled.unreachable, FAILURE_THRESHOLD as u64,
            "历史读数不被抹掉，只是不再冷却"
        );
        assert_eq!(cooled.reached, 1, "这一次探测算一条通路成立");
        assert_eq!(
            entry_of("all-dead").unreachable,
            1,
            "死的那条攒下一次连不上"
        );
    }

    /// 服务商没配就没有可测的目标，这一句要替用户说清（而不是静默测出 N 个失败）
    #[test]
    fn testing_them_all_without_an_endpoint_says_so() {
        let mut config = AppConfig::default();
        config.proxy_pool.proxies = vec![entry("no-target", "http://127.0.0.1:1", true)];
        let error = test_all_of(&config).unwrap_err();
        assert!(error.contains("服务商地址是空的"), "{error}");
    }

    /// 批量导入与批量测试的 IPC 形状：前端那两格直接吃这两个类型，
    /// 后端少一个字段就是界面上静默一格 undefined
    #[test]
    fn the_batch_payloads_match_the_frontend_types() {
        let row = serde_json::to_value(ImportRow {
            url: "http://127.0.0.1:1".into(),
            name: "本机".into(),
            reason: None,
        })
        .expect("导入行序列化得出去");
        crate::test_support::assert_matches_ts(&row, "ProxyImportRow");
        let outcome = serde_json::to_value(ProxyTestOutcome {
            id: "shape-2".into(),
            name: "本机".into(),
            ok: true,
            ms: 12,
            note: String::new(),
        })
        .expect("测试结果序列化得出去");
        crate::test_support::assert_matches_ts(&outcome, "ProxyTestOutcome");
    }
}
