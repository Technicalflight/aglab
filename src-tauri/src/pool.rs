//! 模型池：池子在配置里，调度读数在这里。
//!
//! 分工刻意掰成两半：**配置**（`config::ModelPool`）说"池里有哪些成员、用哪种
//! 模式与策略"；**这一刻**（本模块的 [`Hub`]）说"轮询走到谁、谁正被占用、谁连续
//! 失败进了冷却、哪条话题粘着哪个成员"。后者只住内存——重启后冷却与亲和清零
//! 不是丢状态，是本来就不该把"三分钟前失败过"当成现在的负载事实。
//!
//! 接线只有一条路：[`resolve`] 在每发请求开跑前把"这一发给谁"定下来，
//! 把成员档案的连接域抄进回合配置；回合结束 [`TurnGuard`] 释放占用计数。
//! 成败信号从 [`crate::chat::read_events`] 回流——那是全机模型请求唯一的
//! POST 出口，所以池子的账与真实的 HTTP 成败天然一致。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::AppHandle;

use crate::config::{PoolKey, PoolMember};

/// 连续失败多少次进冷却。1–2 次可能是模型抽风，3 次起才像服务商病了
const FAILURE_THRESHOLD: u32 = 3;
/// 冷却基数与倍增：第 3 次失败歇 30s，第 4 次 60s……封顶 10 分钟
const COOLDOWN_BASE: Duration = Duration::from_secs(30);
const COOLDOWN_CAP: Duration = Duration::from_secs(600);

/// 调度读数是这台机器的单例：聊天、定时任务、编排节点共享同一本账。
/// 用 [`std::sync::OnceLock`] 而不是 Tauri 托管态，是因为请求层（read_events）
/// 手里只有 config 没有 AppHandle——成败回报不能依赖拿着句柄才能记账
#[derive(Default)]
pub struct Hub {
    inner: std::sync::Mutex<HubInner>,
}

fn hub() -> &'static Hub {
    static HUB: std::sync::OnceLock<Hub> = std::sync::OnceLock::new();
    HUB.get_or_init(Hub::default)
}

#[derive(Default, Clone)]
struct Entry {
    /// 被调度过的回合总数（占用即计，成败另算）
    total: u64,
    /// 当前正在跑的回合数。它是"最少并发"策略的依据，也是面板上那格负载
    inflight: i64,
    /// 连续失败次数。一次成功就清零——要的是"现在病没病"，不是历史平均
    failures: u32,
    cooldown_until: Option<Instant>,
    /// 最近一次成功回报的时刻。缓存感知首挑的依据：刚成功过的成员，
    /// 它身上的 prompt 缓存（system+工具声明那段前缀）大概率还热着
    last_success: Option<Instant>,
    /// 平滑加权轮询的游标（nginx 同款算法）。跟着成员键走，成员删了它一起没
    current_weight: i64,
}

#[derive(Default)]
struct HubInner {
    /// 加权随机的 xorshift 游标。进程级一颗就够：要的是"别老打同一家"，
    /// 不是可复现的随机序列
    rng: u64,
    entries: HashMap<String, Entry>,
    /// 话题亲和账：conversation_id → 这条话题上一发给了谁。
    /// 前缀缓存按（上游账号 × 模型）分域，话题内换人等于把命中率交给运气——
    /// 所以同一话题只要上次的成员还在场且没在冷却，就一直粘住它。
    /// 和冷却一样只住内存：重启清零，顶多下一次按策略重挑
    affinity: HashMap<String, String>,
}

/// 亲和账的容量上限。桌面应用的话题数远到不了这里；真到了就整体清空——
/// 亲和是优化不是承诺，丢了账顶多下一次按策略重挑，不该为它做逐出算法
const AFFINITY_CAP: usize = 1024;

fn key_of(profile_id: &str, model: &str) -> String {
    format!("{profile_id}\u{0}{model}")
}

impl Hub {
    /// 仅测试在用（读一格账本做断言）。生产路径要的是改账本（claim/release/note_*）
    /// 与整表投影（snapshot），没有"读一格"的消费者
    #[cfg(test)]
    fn entry(&self, key: &str) -> Entry {
        self.inner
            .lock()
            .expect("池账本锁")
            .entries
            .get(key)
            .cloned()
            .unwrap_or_default()
    }

    /// 占一个名额：total +1、inflight +1。成败由请求层事后回报
    fn claim(&self, key: &str) {
        let mut inner = self.inner.lock().expect("池账本锁");
        inner.rng = inner
            .rng
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let entry = inner.entries.entry(key.to_string()).or_default();
        entry.total += 1;
        entry.inflight += 1;
    }

    fn release(&self, key: &str) {
        let mut inner = self.inner.lock().expect("池账本锁");
        if let Some(entry) = inner.entries.get_mut(key) {
            entry.inflight -= 1;
        }
    }

    /// 请求层回报：一次 HTTP 往返成功。连续失败清零、冷却解除
    fn note_success(&self, key: &str) {
        let mut inner = self.inner.lock().expect("池账本锁");
        if let Some(entry) = inner.entries.get_mut(key) {
            entry.failures = 0;
            entry.cooldown_until = None;
            entry.last_success = Some(Instant::now());
        }
    }

    /// 请求层回报：一次 HTTP 往返失败。连到第 3 次起按倍增进冷却
    fn note_failure(&self, key: &str) {
        let mut inner = self.inner.lock().expect("池账本锁");
        let Some(entry) = inner.entries.get_mut(key) else {
            return;
        };
        entry.failures += 1;
        if entry.failures >= FAILURE_THRESHOLD {
            let shift = entry.failures - FAILURE_THRESHOLD;
            let secs = COOLDOWN_BASE
                .as_secs()
                .saturating_mul(1u64 << shift.min(16))
                .min(COOLDOWN_CAP.as_secs());
            entry.cooldown_until = Some(Instant::now() + Duration::from_secs(secs));
        }
    }

    /// 面板读数。冷却剩多少毫秒一并给出，前端不必再猜绝对时刻
    fn snapshot(&self, keys: &[(String, String)]) -> Vec<MemberStat> {
        let inner = self.inner.lock().expect("池账本锁");
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (profile_id, model) in keys {
            let key = key_of(profile_id, model);
            if !seen.insert(key.clone()) {
                continue;
            }
            let entry = inner.entries.get(&key).cloned().unwrap_or_default();
            let cooling_ms = entry
                .cooldown_until
                .map(|until| until.saturating_duration_since(Instant::now()).as_millis() as u64)
                .unwrap_or(0);
            out.push(MemberStat {
                profile_id: profile_id.clone(),
                model: model.clone(),
                total: entry.total,
                inflight: entry.inflight.max(0) as u64,
                failures: entry.failures,
                cooling_ms,
            });
        }
        out
    }
}

/// 面板上一格成员读数
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemberStat {
    pub profile_id: String,
    pub model: String,
    pub total: u64,
    pub inflight: u64,
    pub failures: u32,
    pub cooling_ms: u64,
}

// 本线程"这一发给谁"（thread_local）。请求层（read_events）据此把成败记到成员头上；
// 池子没接管这一线程时它就是空的，回报进来也只是个 no-op
thread_local! {
    static CURRENT: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// 请求层回流成败（chat.rs 的 read_events 出口处调用）
pub fn note_success() {
    CURRENT.with(|cell| {
        if let Some(key) = cell.borrow().as_ref() {
            hub().note_success(key);
        }
    });
}

/// 请求层回流失败。停止不算失败——那是用户的选择，不是成员病了，
/// 这个判断在 chat.rs 的出口处做（它认得自己的 STOP_MARK）
pub fn note_failure() {
    CURRENT.with(|cell| {
        if let Some(key) = cell.borrow().as_ref() {
            hub().note_failure(key);
        }
    });
}

/// 占用守卫：活着 = 这个成员正被一个回合占着。
/// Drop 里做两件收尾：并发数 -1、本线程"当前成员"清除——
/// 收尾绑在守护上而不是靠调用方记得，失败路径才漏不掉
pub struct TurnGuard {
    key: String,
}

impl TurnGuard {
    fn claim(key: String) -> Self {
        hub().claim(&key);
        CURRENT.with(|cell| *cell.borrow_mut() = Some(key.clone()));
        Self { key }
    }
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        hub().release(&self.key);
        CURRENT.with(|cell| *cell.borrow_mut() = None);
    }
}

/// 一发请求的池子侧包裹：覆写好的回合配置 + 调度读数 + 占用守卫。
/// 调用方把 `config` 拿去跑回合，`guard` 活到回合结束——别提前丢
pub struct Turn {
    /// 本回合真正生效的配置：成员档案的连接域 + 成员的模型名
    pub config: crate::config::AppConfig,
    pub picked: Picked,
    pub guard: TurnGuard,
}

/// 这一发给了谁、以什么身份给的。source 只为被看见：界面要能区分
/// "决策模型挑的"和"调度器兜底的"，不让人对着一格读数猜来路
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Picked {
    pub key: PoolKey,
    /// "pinned" | "strategy" | "decision" | "fallback"
    pub source: String,
}

/// 成员解析成"这一发真正用的连接"。档案 id 空串 = 当前连接：只换模型名；
/// 其余成员：档案的 13 个连接域字段整体抄入 + 模型覆盖。
/// 指向已删档案的成员在这里报错——调用方决定报给界面还是跳过
fn overlay(config: &mut crate::config::AppConfig, member: &PoolMember) -> Result<(), String> {
    if member.profile_id.trim().is_empty() {
        config.model = member.model.trim().to_string();
        crate::config::apply_model_spec(config);
        return Ok(());
    }
    let profile = config
        .profiles
        .iter()
        .find(|profile| profile.id == member.profile_id)
        .cloned()
        .ok_or_else(|| {
            format!(
                "池成员指向的档案已经不存在了（{}）。去「设置 → 模型池」把它移除或换成别的档案。",
                member.profile_id
            )
        })?;
    crate::config::apply_profile_connection(config, &profile);
    config.model = member.model.trim().to_string();
    // 成员换的是**模型**，窗口/最大输出/思考档得跟着换：不盖这一层，
    // grok 的 1.1M 会套在 deepseek 的 128K 头上，压缩阈值与用量百分比一起读假数
    crate::config::apply_model_spec(config);
    Ok(())
}

/// 一名候选：成员 + 它在账本里的键。与成员定义分开，是为了让挑选函数
/// 只认"有权重的人"，不必背着整份配置跑
#[derive(Clone)]
struct Candidate {
    member: PoolMember,
    key: String,
}

impl Candidate {
    fn weight(&self) -> i64 {
        self.member.weight.max(1) as i64
    }
}

/// 平滑加权轮询（nginx smooth WRR）。权重 [5,1,1] 时谁也不会被连发五次，
/// 间隔是均匀的——普通轮询加权做不到这一点
fn smooth_wrr(hub: &mut HubInner, candidates: &[Candidate]) -> usize {
    let total: i64 = candidates.iter().map(Candidate::weight).sum();
    let mut best = 0usize;
    let mut best_weight = i64::MIN;
    for (index, candidate) in candidates.iter().enumerate() {
        let entry = hub.entries.entry(candidate.key.clone()).or_default();
        entry.current_weight += candidate.weight();
        if entry.current_weight > best_weight {
            best_weight = entry.current_weight;
            best = index;
        }
    }
    if let Some(entry) = hub.entries.get_mut(&candidates[best].key) {
        entry.current_weight -= total;
    }
    best
}

fn weighted_random(hub: &mut HubInner, candidates: &[Candidate]) -> usize {
    let total: i64 = candidates.iter().map(Candidate::weight).sum();
    // xorshift64：不引 rand 依赖，确定性还留在测试手里
    hub.rng ^= hub.rng << 13;
    hub.rng ^= hub.rng >> 7;
    hub.rng ^= hub.rng << 17;
    let mut point = (hub.rng % total.max(1) as u64) as i64;
    for (index, candidate) in candidates.iter().enumerate() {
        point -= candidate.weight();
        if point < 0 {
            return index;
        }
    }
    candidates.len() - 1
}

fn least_used(hub: &HubInner, candidates: &[Candidate]) -> usize {
    let mut best = 0usize;
    let mut best_rank = (i64::MAX, u64::MAX);
    for (index, candidate) in candidates.iter().enumerate() {
        let entry = hub.entries.get(&candidate.key).cloned().unwrap_or_default();
        let rank = (entry.inflight, entry.total);
        if rank < best_rank {
            best_rank = rank;
            best = index;
        }
    }
    best
}

/// 成员此刻能不能接活：没在冷却（或冷却已到期）就是新鲜的
fn is_fresh(hub: &HubInner, key: &str) -> bool {
    match hub.entries.get(key).and_then(|entry| entry.cooldown_until) {
        Some(until) => Instant::now() >= until,
        None => true,
    }
}

/// 策略挑选。冷却中的成员先排开；全都在冷却时挑冷却最早结束的那个——
/// 发给一个可能还在生病的成员，好过让整发请求没得可去
fn pick_strategy(hub: &mut HubInner, strategy: &str, candidates: &[Candidate]) -> usize {
    let fresh: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| is_fresh(hub, &candidate.key))
        .map(|(index, _)| index)
        .collect();
    if fresh.is_empty() {
        // 全员冷却：最早出冷却的先上
        let mut order: Vec<usize> = (0..candidates.len()).collect();
        order.sort_by_key(|&index| {
            hub.entries
                .get(&candidates[index].key)
                .and_then(|entry| entry.cooldown_until)
        });
        return order.first().copied().unwrap_or(0);
    }
    if fresh.len() == 1 {
        return fresh[0];
    }
    let scoped: Vec<Candidate> = fresh.iter().map(|&index| candidates[index].clone()).collect();
    let picked = match strategy {
        "least_used" => least_used(hub, &scoped),
        "random" => weighted_random(hub, &scoped),
        // 优先级转移（cc-switch 的 failover）：永远用列表里第一个健康的成员。
        // 主成员进了冷却，上面的 fresh 过滤已经把它排开，这里要做的只有"取队首"——
        // 恢复（冷却到期）后它自然回到队首，新话题跟着回主位
        "failover" => 0,
        // 认不出的策略退回轮询：错档位顶多是不够聪明，不该是不能用
        _ => smooth_wrr(hub, &scoped),
    };
    fresh[picked]
}

/// 缓存感知首挑的时间窗。provider 的前缀缓存 TTL 多为 5 分钟——窗内的成员，
/// 它身上 system prompt + 工具声明那段前缀的缓存大概率还热着
const CACHE_AWARE_WINDOW: Duration = Duration::from_secs(300);

/// 前缀聚类 + 跨 provider 缓存感知路由的合体（design 图①④）：
/// 新话题的首发与刚刚完成的话题共享同一份 system prompt 与工具声明——
/// 把它发到"最近刚成功过、缓存还热"的成员上，那段前缀直接命中，省一次全价 prefill。
/// 只在 fresh（未冷却）候选里挑，挑"最近成功"的那一个；没有成员在缓存窗内
/// 就返回 None，让调用方落回原策略——没有热缓存可蹭时，策略的分流语义优先
fn cache_aware_pick(hub: &HubInner, candidates: &[Candidate]) -> Option<usize> {
    let now = Instant::now();
    let mut best: Option<(usize, Instant)> = None;
    for (index, candidate) in candidates.iter().enumerate() {
        if !is_fresh(hub, &candidate.key) {
            continue;
        }
        if let Some(entry) = hub.entries.get(&candidate.key) {
            if let Some(at) = entry.last_success {
                if now.duration_since(at) <= CACHE_AWARE_WINDOW {
                    let better = match best {
                        None => true,
                        Some((_, at_best)) => at > at_best,
                    };
                    if better {
                        best = Some((index, at));
                    }
                }
            }
        }
    }
    best.map(|(index, _)| index)
}

/// 话题亲和挑选：这条话题上次用过的成员只要还在场且没在冷却，就继续给它。
/// 策略（轮询/随机/最少并发）只在「新话题第一次发」或「粘的人不在了/病了」时出场；
/// 话题之间照常按策略分流，话题之内永远命中自己的缓存。
/// 空话题身份（不该发生，但配置是用户可以手改的文件）不粘也不记账
fn pick_with_affinity(
    hub: &mut HubInner,
    strategy: &str,
    candidates: &[Candidate],
    conversation_id: &str,
    cache_aware: bool,
) -> usize {
    if !conversation_id.is_empty() {
        if let Some(remembered) = hub.affinity.get(conversation_id).cloned() {
            if let Some(index) = candidates
                .iter()
                .position(|candidate| candidate.key == remembered)
            {
                if is_fresh(hub, &remembered) {
                    return index;
                }
            }
            // 记忆里的人不在场（被移除/停用）或在冷却：作废这条记忆，
            // 按策略重挑，挑中谁就把记忆改指到谁——缓存跟着最新的人走，不回头
            hub.affinity.remove(conversation_id);
        }
    }
    // 缓存感知首挑排在实际策略之前：这是"新话题第一次发"的时刻，
    // 也是它唯一能蹭到别人身上热缓存的时刻——聚到热成员上就是前缀聚类
    let index = if cache_aware {
        cache_aware_pick(hub, candidates).unwrap_or_else(|| pick_strategy(hub, strategy, candidates))
    } else {
        pick_strategy(hub, strategy, candidates)
    };
    if !conversation_id.is_empty() {
        if hub.affinity.len() >= AFFINITY_CAP {
            hub.affinity.clear();
        }
        hub.affinity
            .insert(conversation_id.to_string(), candidates[index].key.clone());
    }
    index
}

/// 这条话题粘住的成员，其缓存是否还在热窗内。压缩的重写成本项用它权衡：
/// 热 = 压一次等于把热前缀整段重写，能推迟就推迟
pub fn cache_hot_for(conversation_id: &str) -> bool {
    let inner = hub().inner.lock().expect("池账本锁");
    let Some(key) = inner.affinity.get(conversation_id) else {
        return false;
    };
    let Some(entry) = inner.entries.get(key) else {
        return false;
    };
    let Some(at) = entry.last_success else {
        return false;
    };
    Instant::now().duration_since(at) <= CACHE_AWARE_WINDOW
}

/// 决策层的挑选结果怎么变成"这一发给谁"：问题本身不在这里问——
/// System 1 的问法（Laya 本地 / Jev 云端、漏斗、敏感性红线）整个住在
/// 前端 `src/lib/decision/`（integrations.ts 的 pickPoolMember），它把选好的
/// 成员键随 chat_send 带下来，本模块只做"认账与兜底"。Rust 侧不再自建
/// 第二个决策通道：同一个问题两处问，迟早各说各话

/// 每发请求的池子入口。`Ok(None)` = 池子没接管（关着/空着），请求照旧；
/// `Err` = 用户明确选的路走不通（如手动指定的成员已被删），原样报给界面——
/// 悄悄改道等于替用户做决定。返回的 [`Turn`] 活到回合结束，别提前丢
/// "这一行不许派工"的判定。剥成纯函数是因为 resolve 本体要 AppHandle，单测够不着；
/// 调用点钉在 [`resolve`] 的候选过滤那一行——那条线程改坏了编译器不会响，语义靠这里守。
/// 表里没有这一行 = 没有更具体的证据，按可派工处理；成员指向查无的档案也不拦
/// （那是另一条"档案已删"的既有错误路径，不在这格加戏）
fn blocked_for_delegation(
    loaded: &crate::config::AppConfig,
    member: &PoolMember,
    for_delegation: bool,
) -> bool {
    if !for_delegation {
        return false;
    }
    // 档案 id 空串 = "当前连接"那张看不见的档案，它的模型表住在顶层
    let specs = if member.profile_id.is_empty() {
        &loaded.models
    } else {
        match loaded
            .profiles
            .iter()
            .find(|profile| profile.id == member.profile_id)
        {
            Some(profile) => &profile.models,
            None => return false,
        }
    };
    specs
        .iter()
        .any(|spec| spec.model == member.model && !spec.delegatable)
}

pub fn resolve(
    app: &AppHandle,
    config: &crate::config::AppConfig,
    // 只用于审计与将来的观测面；现在的挑选全靠 pick 与策略
    _prompt: &str,
    // 决策层（System 1：Laya 本地 / Jev 云端）的挑选结果，界面那一发在发送前
    // 问出来带下来的。只有 decision 模式读它；缺席（后台任务没有决策层可问）
    // 或指到花名册之外（池子刚被改过）都退回调度器——决策层不在了，池子不能跟着停摆
    pick: Option<&PoolKey>,
    // 话题身份，亲和账的键：同一话题粘住上一次的成员（前缀缓存按账号×模型
    // 分域，话题内换人就是把命中率交给运气）。策略只在话题第一次发、或粘的
    // 人不在场/进冷却时出场。空串 = 没有话题身份，照旧按策略挑、不记账
    conversation_id: &str,
    // 这一发是不是 AI 起的（子助理/编排/定时任务）。true 时调度器跳过"不许派工"
    // 的档案——那是用户没点头让它担账的服务商；界面聊天与明确点名（手动指定）不受限
    for_delegation: bool,
    // 本轮已经失败过的成员键（轮内 failover 的让位清单）：不再挑它们，
    // 免得对同一个病了的服务商连撞三次。全部被排除时清空排除——总要有人接这一发
    exclude: &[String],
) -> Result<Option<Turn>, String> {
    // 池配置每次现读：设置页改完模式，下一条消息就该按新的走，
    // 不该等"下一次发消息的那个入口"想起重新加载
    let loaded = crate::config::load(app);
    let pool = loaded.model_pool.clone();
    if !pool.enabled() {
        return Ok(None);
    }
    // 这一发是 AI 起的时候，"不许派工"的档案要从候选里摘掉。注意只摘**候选**：
    // 手动指定的成员是用户自己点的名，照旧可用
    let enabled: Vec<PoolMember> = pool
        .members
        .iter()
        .filter(|member| member.enabled && !member.model.trim().is_empty())
        .cloned()
        .collect();
    if enabled.is_empty() {
        return Ok(None);
    }
    let mut candidates: Vec<Candidate> = enabled
        .iter()
        .filter(|member| !blocked_for_delegation(&loaded, member, for_delegation))
        .map(|member| Candidate {
            key: key_of(&member.profile_id, &member.model),
            member: member.clone(),
        })
        .collect();
    // 轮内 failover 不设次数上限：失败一个排除一个，一路换下去。
    // 全部被排除时明确终止——清空排除重撞同一个坑是无底洞，
    // 用户的诉求是"换到有人接活"，没人接活就该说实话
    if !exclude.is_empty() {
        let remaining: Vec<Candidate> = candidates
            .iter()
            .filter(|candidate| !exclude.contains(&candidate.key))
            .cloned()
            .collect();
        if remaining.is_empty() {
            return Err(format!(
                "池子里的 {} 个成员都试过了，全部失败——稍后再试，或检查各成员的服务商配置。",
                candidates.len()
            ));
        }
        candidates = remaining;
    }
    if candidates.is_empty() {
        // 池子还在，但这一发能挑的人全被摘掉了：按"池子没接管"的原路退回去
        return Ok(None);
    }

    let (member, source) = match pool.mode.as_str() {
        "pinned" => {
            let pinned = pool
                .pinned
                .as_ref()
                .ok_or_else(|| "模型池是「手动指定」模式，但还没有指定成员。".to_string())?;
            let member = enabled
                .iter()
                .find(|member| {
                    member.profile_id == pinned.profile_id && member.model == pinned.model
                })
                .ok_or_else(|| {
                    "手动指定的池成员已经不在池里了。去「设置 → 模型池」重新选一个。".to_string()
                })?;
            (member.clone(), "pinned")
        }
        "decision" => {
            let decided = pick.and_then(|key| {
                enabled.iter().find(|member| {
                    member.profile_id == key.profile_id && member.model == key.model
                })
            });
            match decided {
                Some(member) => (member.clone(), "decision"),
                None => {
                    let mut inner = hub().inner.lock().expect("池账本锁");
                    // 决策层没答上来的兜底也认亲和：这条话题上一发用谁，兜底就还回谁那里——
                    // 冷 strategy 挑一个生人，等于把这条话题攒的缓存全废掉
                    let index =
                        pick_with_affinity(
                            &mut inner,
                            &pool.strategy,
                            &candidates,
                            conversation_id,
                            pool.cache_aware_pick,
                        );
                    drop(inner);
                    (candidates[index].member.clone(), "fallback")
                }
            }
        }
        // 认不出的 mode 一律当 auto：配置是用户可以手改的文件，错档位不该让请求没处发
        _ => {
            let mut inner = hub().inner.lock().expect("池账本锁");
            let index =
                pick_with_affinity(
                            &mut inner,
                            &pool.strategy,
                            &candidates,
                            conversation_id,
                            pool.cache_aware_pick,
                        );
            drop(inner);
            (candidates[index].member.clone(), "strategy")
        }
    };

    let mut turn_config = config.clone();
    overlay(&mut turn_config, &member)?;
    // 亲和账补记 pinned 与 decision 的显式挑选（auto 与 fallback 分支里挑选函数
    // 已经记过）：模式切来切去，账上始终有"这条话题上一发给了谁"。
    // 放在 overlay 成功之后——指向已删档案的挑选不该留下亲和记忆
    if !conversation_id.is_empty() {
        hub().inner
            .lock()
            .expect("池账本锁")
            .affinity
            .insert(
                conversation_id.to_string(),
                key_of(&member.profile_id, &member.model),
            );
    }
    let guard = TurnGuard::claim(key_of(&member.profile_id, &member.model));
    Ok(Some(Turn {
        config: turn_config,
        picked: Picked {
            key: PoolKey {
                profile_id: member.profile_id,
                model: member.model,
            },
            source: source.to_string(),
        },
        guard,
    }))
}

/// 目录条目：一个可加进池子的（档案 × 模型）来源。error 只描述"这个来源这次
/// 没拉到"，不是整批失败——一家服务商挂了不该让别家的目录也看不见
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogEntry {
    pub profile_id: String,
    pub name: String,
    pub base_url: String,
    pub api_format: String,
    pub models: Vec<String>,
    pub error: Option<String>,
}

/// 自动刷新模型目录：逐个档案拉 `/models`，加上"当前连接"这个伪档案。
/// 相同（服务商 × 凭据）的档案只拉一次——结果共享，谁也不多花一次请求。
///
/// **async 命令**：这一发里全是阻塞的网络请求（每条 30 秒超时的 ureq + 凭据库读取），
/// 同步命令跑在主线程上——用户展开一个模型下拉，某个服务商没应答，整个窗口就陪它
/// 冻到超时为止。挪进阻塞池（decision.rs 的 spawn_blocking 是同款先例），下拉自己转圈
#[tauri::command]
pub async fn pool_catalog(app: AppHandle) -> Result<Vec<CatalogEntry>, String> {
    let config = crate::config::load(&app);
    crate::chat::provider_gate(&config)?;
    tauri::async_runtime::spawn_blocking(move || pool_catalog_of(config))
        .await
        .map_err(|error| format!("模型目录的取数线程没能跑完：{error}"))?
}

/// 同步的那一半：拿一份读好的配置串行拉各服务商。搬进阻塞池的只有它
fn pool_catalog_of(config: crate::config::AppConfig) -> Result<Vec<CatalogEntry>, String> {
    let mut sources: Vec<CatalogEntry> = Vec::new();
    // (base_url, api_format, service, user) → sources 里的下标
    let mut fetched: HashMap<(String, String, String, String, Option<String>), usize> = HashMap::new();

    // 当前连接排最前：大多数时候用户想加的就是正在用的这一套
    // （profile_id 空串 = 当前连接，池成员也用同一个约定）
    let mut queue: Vec<(&str, &str, &str, &str, &str, &str)> = vec![(
        "",
        "当前连接",
        &config.base_url,
        &config.api_format,
        &config.credential_service,
        &config.credential_user,
    )];
    for profile in &config.profiles {
        queue.push((
            &profile.id,
            &profile.name,
            &profile.base_url,
            &profile.api_format,
            &profile.credential_service,
            &profile.credential_user,
        ));
    }

    for (profile_id, name, base_url, api_format, service, user) in queue {
        // 模型列表的线格式：Anthropic 换头部与解析形状，其余家共用 OpenAI 口径
        // 直接用 api_format 本身：queue 的元素类型是 Vec<(&str, ...)>,
        // 解构出来就是 &str，原先的 clone() 只复制引用（str 不实现 Clone）。
        let wire_format = api_format;
        // 代理绑定按档案解析（"当前连接"那张走顶层配置的绑定）。解析失败算这一格的错，
        // 不拖垮整个目录；指名的代理不在场时报出来，不静默直连。
        // 同一连接的不同代理绑定各拉各的——共享缓存键里要带上出发的那条代理。
        // 清单这一发也吃换路：一条代理连不上不该让整个目录格报错
        let mut route: Option<crate::proxy::Plan> = None;
        let proxy_url = if base_url.trim().is_empty() {
            None
        } else {
            let models_url = crate::chat::models_endpoint_for(base_url, wire_format == "anthropic");
            let planned = if profile_id.is_empty() {
                crate::proxy::plan(&config, &models_url)
            } else {
                match config.profiles.iter().find(|profile| profile.id == profile_id) {
                    Some(profile) => crate::proxy::plan_profile(&config, profile, &models_url),
                    None => Ok(crate::proxy::plan_direct()),
                }
            };
            match planned {
                Ok(planned) => {
                    let url = planned.first_url().map(str::to_string);
                    route = Some(planned);
                    url
                }
                Err(error) => {
                    sources.push(CatalogEntry {
                        profile_id: profile_id.to_string(),
                        name: name.to_string(),
                        base_url: base_url.to_string(),
                        api_format: api_format.to_string(),
                        models: Vec::new(),
                        error: Some(error),
                    });
                    continue;
                }
            }
        };
        let fingerprint = (
            base_url.to_string(),
            api_format.to_string(),
            service.to_string(),
            user.to_string(),
            proxy_url.clone(),
        );
        if let Some(&index) = fetched.get(&fingerprint) {
            let shared = sources[index].models.clone();
            sources.push(CatalogEntry {
                profile_id: profile_id.to_string(),
                name: name.to_string(),
                base_url: base_url.to_string(),
                api_format: api_format.to_string(),
                models: shared,
                error: None,
            });
            continue;
        }
        let entry = if base_url.trim().is_empty() {
            CatalogEntry {
                profile_id: profile_id.to_string(),
                name: name.to_string(),
                base_url: base_url.to_string(),
                api_format: api_format.to_string(),
                models: Vec::new(),
                error: Some("服务商地址是空的。".into()),
            }
        } else {
            // base_url 非空的那一格在上面已经解析出计划（解析失败那一格已经 continue 掉了）
            let planned = route
                .as_mut()
                .expect("base_url 非空的这一格必定有一条要试的路");
            match crate::config::api_key_for(service, user).and_then(|key| {
                crate::chat::fetch_models_for(
                    base_url,
                    &wire_format,
                    &key,
                    service,
                    &config.net_egress_allow,
                    planned,
                )
            }) {
                Ok(models) => CatalogEntry {
                    profile_id: profile_id.to_string(),
                    name: name.to_string(),
                    base_url: base_url.to_string(),
                    api_format: api_format.to_string(),
                    models,
                    error: None,
                },
                Err(error) => CatalogEntry {
                    profile_id: profile_id.to_string(),
                    name: name.to_string(),
                    base_url: base_url.to_string(),
                    api_format: api_format.to_string(),
                    models: Vec::new(),
                    error: Some(error),
                },
            }
        };
        fetched.insert(fingerprint, sources.len());
        sources.push(entry);
    }
    Ok(sources)
}

/// 面板读数：按配置里成员的顺序给（pinned 那一格一并给，哪怕它不在成员表里）。
/// 池子改过之后账本里多出来的旧条目不在这里出现——面板只说配置内成员的事
#[tauri::command]
pub fn pool_stats(app: AppHandle) -> Result<Vec<MemberStat>, String> {
    let config = crate::config::load(&app);
    let pool = &config.model_pool;
    let mut keys: Vec<(String, String)> = pool
        .members
        .iter()
        .map(|member| (member.profile_id.clone(), member.model.clone()))
        .collect();
    if let Some(pinned) = &pool.pinned {
        keys.push((pinned.profile_id.clone(), pinned.model.clone()));
    }
    Ok(hub().snapshot(&keys))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(profile_id: &str, model: &str, weight: u32) -> Candidate {
        Candidate {
            key: key_of(profile_id, model),
            member: PoolMember {
                profile_id: profile_id.to_string(),
                model: model.to_string(),
                weight,
                enabled: true,
            },
        }
    }

    /// 缓存感知首挑（图①④）：窗内有"最近成功"的成员时新话题聚到它身上，
    /// 窗外或全员冷窗时返回 None 落回原策略；同窗内挑最近成功的；冷却的不接活
    #[test]
    fn cache_aware_pick_routes_to_the_warm_member_within_the_window() {
        let mut hub = HubInner::default();
        let candidates = vec![member("p", "m-a", 1), member("p", "m-b", 1)];

        // A 三十秒前成功过（缓存窗内），B 从未——新话题该聚到 A
        hub.entries
            .entry(key_of("p", "m-a"))
            .or_default()
            .last_success = Some(Instant::now() - Duration::from_secs(30));
        let picked = cache_aware_pick(&hub, &candidates).unwrap();
        assert_eq!(candidates[picked].key, key_of("p", "m-a"));

        // A 的成功退到十分钟前（窗外）：没有热缓存可蹭
        hub.entries
            .get_mut(&key_of("p", "m-a"))
            .unwrap()
            .last_success = Some(Instant::now() - Duration::from_secs(600));
        assert!(cache_aware_pick(&hub, &candidates).is_none());

        // 两个都在窗内：挑最近成功的那一个
        hub.entries
            .entry(key_of("p", "m-a"))
            .or_default()
            .last_success = Some(Instant::now() - Duration::from_secs(120));
        hub.entries
            .entry(key_of("p", "m-b"))
            .or_default()
            .last_success = Some(Instant::now() - Duration::from_secs(30));
        let picked = cache_aware_pick(&hub, &candidates).unwrap();
        assert_eq!(candidates[picked].key, key_of("p", "m-b"));

        // 冷却中的成员再热也不接活
        hub.entries
            .entry(key_of("p", "m-b"))
            .or_default()
            .cooldown_until = Some(Instant::now() + Duration::from_secs(60));
        let picked = cache_aware_pick(&hub, &candidates).unwrap();
        assert_eq!(candidates[picked].key, key_of("p", "m-a"));
    }

    /// 开关的行为：开着时新话题首挑落到热成员（蹭缓存），关着时走原策略；
    /// 首挑落定的成员进亲和账，这条话题从此粘住它
    #[test]
    fn cache_aware_flag_switches_new_session_picking() {
        let mut hub = HubInner::default();
        let candidates = vec![member("p", "m-a", 1), member("p", "m-b", 1)];
        hub.entries
            .entry(key_of("p", "m-b"))
            .or_default()
            .last_success = Some(Instant::now() - Duration::from_secs(30));

        // 开：failover 本来会给队首 A，缓存感知把新话题聚到热着的 B
        let opened = pick_with_affinity(&mut hub, "failover", &candidates, "conv-new", true);
        assert_eq!(candidates[opened].key, key_of("p", "m-b"));
        // 亲和账记下 B，这条话题粘住
        let again = pick_with_affinity(&mut hub, "failover", &candidates, "conv-new", true);
        assert_eq!(candidates[again].key, key_of("p", "m-b"));

        // 关：新话题照原策略走 failover 队首
        let closed = pick_with_affinity(&mut hub, "failover", &candidates, "conv-fresh", false);
        assert_eq!(candidates[closed].key, key_of("p", "m-a"));
    }

    /// AI 起的回合要跳过"这一行不许派工"的模型，人起的回合谁都能上；
    /// 档案 id 空串认顶层那张模型表，表里没这一行 = 没有更具体的证据，不拦
    #[test]
    fn delegation_filter_follows_the_model_row() {
        let mut loaded = crate::config::AppConfig::default();
        let mut profile = crate::config::EndpointProfile::default();
        profile.id = "prof-1".into();
        profile.models = vec![
            crate::config::ModelSpec {
                model: "quiet".into(),
                delegatable: false,
                ..Default::default()
            },
            crate::config::ModelSpec {
                model: "loud".into(),
                ..Default::default()
            },
        ];
        loaded.profiles.push(profile);
        loaded.models = vec![crate::config::ModelSpec {
            model: "top-quiet".into(),
            delegatable: false,
            ..Default::default()
        }];

        assert!(
            blocked_for_delegation(&loaded, &member("prof-1", "quiet", 1).member, true),
            "这一行标了不许派工"
        );
        assert!(
            !blocked_for_delegation(&loaded, &member("prof-1", "quiet", 1).member, false),
            "人起的回合不拦：那是用户自己的选择"
        );
        assert!(
            !blocked_for_delegation(&loaded, &member("prof-1", "loud", 1).member, true),
            "没标的照常参选"
        );
        assert!(
            !blocked_for_delegation(&loaded, &member("prof-1", "unlisted", 1).member, true),
            "表里没这一行就没有更具体的证据，别拿缺席当拒绝"
        );
        assert!(
            !blocked_for_delegation(&loaded, &member("ghost", "quiet", 1).member, true),
            "查无档案不在这格拦，宁可不拦也不虚报"
        );
        assert!(
            blocked_for_delegation(&loaded, &member("", "top-quiet", 1).member, true),
            "空档案 id 认顶层那张表"
        );
        assert!(
            !blocked_for_delegation(&loaded, &member("", "top-loud", 1).member, true),
            "顶层表里没列的照常用"
        );
    }

    #[test]
    fn smooth_wrr_spreads_by_weight_without_bursting() {
        let mut hub = HubInner::default();
        let candidates = vec![
            member("p", "big", 5),
            member("p", "small-a", 1),
            member("p", "small-b", 1),
        ];
        let mut counts = [0usize; 3];
        for _ in 0..7 {
            counts[smooth_wrr(&mut hub, &candidates)] += 1;
        }
        assert_eq!(counts, [5, 1, 1], "7 发里 5/1/1，一次不多一次不少");
    }

    #[test]
    fn least_used_prefers_the_idle_member() {
        let mut hub = HubInner::default();
        let candidates = vec![member("p", "a", 1), member("p", "b", 1)];
        hub.entries.entry(key_of("p", "a")).or_default().inflight = 2;
        assert_eq!(least_used(&hub, &candidates), 1, "占用中的成员让位");
    }

    #[test]
    fn cooling_members_are_skipped_and_total_cooldown_falls_back_to_soonest() {
        let mut hub = HubInner::default();
        let candidates = vec![member("p", "a", 1), member("p", "b", 1)];
        let later = Instant::now() + Duration::from_secs(60);
        let sooner = Instant::now() + Duration::from_secs(1);
        hub.entries
            .entry(key_of("p", "a"))
            .or_default()
            .cooldown_until = Some(later);
        hub.entries
            .entry(key_of("p", "b"))
            .or_default()
            .cooldown_until = Some(sooner);
        // a 与 b 都在冷却，但 b 先出冷却：轮到 b，而不是按顺序装死
        assert_eq!(pick_strategy(&mut hub, "round_robin", &candidates), 1);
    }

    /// 优先级转移：队首健康就永远用它；队首病了滑到下一个；恢复后回主位。
    /// 转移发生在发与发之间（冷却把病成员排开），不在一次请求内换家重试
    #[test]
    fn failover_sticks_to_the_head_until_it_cools_down() {
        let mut hub = HubInner::default();
        let candidates = vec![member("p", "primary", 1), member("p", "backup", 1)];
        assert_eq!(
            pick_strategy(&mut hub, "failover", &candidates),
            0,
            "队首健康，轮询与权重都不参与"
        );
        // 主成员连挂 3 次进冷却（失败账本的动作与次数在外层 Hub 的 note_failure
        // 里，这里直接摆出它的结果：冷却中的账）
        let key = key_of("p", "primary");
        hub.entries.entry(key.clone()).or_default().cooldown_until =
            Some(Instant::now() + Duration::from_secs(30));
        assert_eq!(
            pick_strategy(&mut hub, "failover", &candidates),
            1,
            "主成员冷却中，自动滑到下一个"
        );
        // 冷却到期 = 恢复：队首重新接客
        hub.entries.get_mut(&key).unwrap().cooldown_until = None;
        assert_eq!(
            pick_strategy(&mut hub, "failover", &candidates),
            0,
            "恢复后新的一发回到主位"
        );
    }

    #[test]
    fn failover_all_cooling_falls_back_to_soonest_recovery() {
        let mut hub = HubInner::default();
        let candidates = vec![member("p", "a", 1), member("p", "b", 1)];
        let later = Instant::now() + Duration::from_secs(60);
        let sooner = Instant::now() + Duration::from_secs(1);
        hub.entries
            .entry(key_of("p", "a"))
            .or_default()
            .cooldown_until = Some(later);
        hub.entries
            .entry(key_of("p", "b"))
            .or_default()
            .cooldown_until = Some(sooner);
        // 全员都在冷却也必须有去处：最早出冷却的先上
        assert_eq!(pick_strategy(&mut hub, "failover", &candidates), 1);
    }

    #[test]
    fn a_conversation_sticks_to_its_member_across_picks() {
        let mut hub = HubInner::default();
        let candidates = vec![member("p", "a", 1), member("p", "b", 1)];
        let first = pick_with_affinity(&mut hub, "round_robin", &candidates, "conv-1", false);
        let second = pick_with_affinity(&mut hub, "round_robin", &candidates, "conv-1", false);
        assert_eq!(first, second, "同一条话题连续两发不换人：换人就是换缓存域");
    }

    #[test]
    fn different_conversations_are_dispatched_independently() {
        let mut hub = HubInner::default();
        let candidates = vec![member("p", "a", 1), member("p", "b", 1)];
        let first = pick_with_affinity(&mut hub, "round_robin", &candidates, "conv-1", false);
        let second = pick_with_affinity(&mut hub, "round_robin", &candidates, "conv-2", false);
        assert_eq!((first, second), (0, 1), "亲和只管话题内，不抹平话题间的分流");
        assert_eq!(
            pick_with_affinity(&mut hub, "round_robin", &candidates, "conv-1", false),
            0,
            "conv-1 回来仍粘住它的成员"
        );
        assert_eq!(
            pick_with_affinity(&mut hub, "round_robin", &candidates, "conv-2", false),
            1,
            "conv-2 回来仍粘住它的成员"
        );
    }

    #[test]
    fn affinity_steps_aside_when_the_member_cools_down() {
        let mut hub = HubInner::default();
        let candidates = vec![member("p", "a", 1), member("p", "b", 1)];
        let first = pick_with_affinity(&mut hub, "round_robin", &candidates, "conv-1", false);
        // 粘住的成员病了：这一发让别人，别对着一台生病的服务商撞三次
        hub.entries
            .entry(candidates[first].key.clone())
            .or_default()
            .cooldown_until = Some(Instant::now() + Duration::from_secs(60));
        let second = pick_with_affinity(&mut hub, "round_robin", &candidates, "conv-1", false);
        assert_ne!(first, second, "粘着的人进了冷却，这一发给别人");
        // 记忆已改指到新成员：病愈的老成员回来也不回头——缓存攒在新人那里
        hub.entries
            .get_mut(&candidates[first].key)
            .unwrap()
            .cooldown_until = None;
        let third = pick_with_affinity(&mut hub, "round_robin", &candidates, "conv-1", false);
        assert_eq!(third, second, "亲和跟人走，不跟原来的键走");
    }

    #[test]
    fn affinity_forgets_members_that_left_the_pool() {
        let mut hub = HubInner::default();
        let two = vec![member("p", "a", 1), member("p", "b", 1)];
        let _ = pick_with_affinity(&mut hub, "round_robin", &two, "conv-1", false);
        // 成员表缩水，记忆里的人不在候选里：作废记忆，在剩下的人里按策略挑
        let one = vec![member("p", "b", 1)];
        assert_eq!(
            pick_with_affinity(&mut hub, "round_robin", &one, "conv-1", false),
            0,
            "离场成员的接替者照常接活"
        );
        // 成员表复原后，亲和已经指向还在场的接替者，不回去找旧人
        assert_eq!(
            pick_with_affinity(&mut hub, "round_robin", &two, "conv-1", false),
            1,
            "记忆已改指到接替的成员"
        );
    }

    #[test]
    fn empty_conversation_id_skips_affinity() {
        let mut hub = HubInner::default();
        let candidates = vec![member("p", "a", 1), member("p", "b", 1)];
        let first = pick_with_affinity(&mut hub, "round_robin", &candidates, "", false);
        let second = pick_with_affinity(&mut hub, "round_robin", &candidates, "", false);
        assert_ne!(first, second, "没有话题身份就不粘，照常按策略轮流");
        assert!(hub.affinity.is_empty(), "空话题不落亲和账");
    }

    #[test]
    fn failures_cool_down_and_success_forgives() {
        let key = key_of("p", "a");
        // 失败只可能发生在被调度过的成员身上：先占名额，账本里才有这一格
        hub().claim(&key);
        hub().note_failure(&key);
        hub().note_failure(&key);
        assert_eq!(hub().entry(&key).failures, 2);
        assert!(
            hub().entry(&key).cooldown_until.is_none(),
            "两次失败还不到冷却"
        );
        hub().note_failure(&key);
        assert!(hub().entry(&key).cooldown_until.is_some(), "第三次失败进冷却");
        hub().note_success(&key);
        let entry = hub().entry(&key);
        assert_eq!(entry.failures, 0);
        assert!(
            entry.cooldown_until.is_none(),
            "一次成功原谅之前的连续失败"
        );
    }

    #[test]
    fn claim_and_release_move_the_inflight_needle() {
        let key = key_of("p", "x");
        let before = hub().entry(&key).inflight;
        let guard = TurnGuard::claim(key.clone());
        assert_eq!(hub().entry(&key).inflight, before + 1);
        assert_eq!(hub().entry(&key).total, before as u64 + 1);
        drop(guard);
        assert_eq!(hub().entry(&key).inflight, before);
    }

    #[test]
    fn overlay_copies_the_whole_connection_and_overrides_the_model() {
        let mut config = crate::config::AppConfig::default();
        config.base_url = "https://top.example/v1".into();
        config.model = "top-model".into();
        config.temperature = 0.9;
        let mut profile = crate::config::EndpointProfile {
            id: "prof-1".into(),
            name: "档案".into(),
            base_url: "https://pool.example/v1".into(),
            model: "pool-default".into(),
            api_format: "anthropic".into(),
            reasoning_effort: "high".into(),
            temperature: 0.3,
            max_tokens: 4096,
            context_tokens: 200_000,
            auto_compact: false,
            models: vec![crate::config::ModelSpec {
                model: "member-model".into(),
                context_tokens: 128_000,
                max_tokens: 8_192,
                reasoning_effort: Some("low".into()),
                ..Default::default()
            }],
            prompt_cache_key: Some(true),
            cache_ttl_seconds: Some(120),
            cache_ttl_by_model: Default::default(),
            credential_service: "svc".into(),
            credential_user: "user".into(),
            proxy: String::new(),
            proxy_by_model: Default::default(),
        };
        profile.cache_ttl_by_model.insert("m".into(), 60);
        config.profiles.push(profile);

        // 指向档案的成员：13 个连接域字段整体抄入，模型用成员的
        let mut config_with_member = config.clone();
        overlay(
            &mut config_with_member,
            &member("prof-1", "member-model", 1).member,
        )
        .unwrap();
        assert_eq!(config_with_member.base_url, "https://pool.example/v1");
        assert_eq!(config_with_member.model, "member-model");
        assert_eq!(config_with_member.api_format, "anthropic");
        assert_eq!(config_with_member.temperature, 0.3);
        assert_eq!(config_with_member.credential_user, "user");
        assert_eq!(
            config_with_member.context_tokens, 128_000,
            "成员换的是模型，窗口得跟着换成那一行的——留在档案的 200K 就是假读数"
        );
        assert_eq!(config_with_member.max_tokens, 8_192);
        assert_eq!(config_with_member.reasoning_effort, "low");
        assert_eq!(
            config_with_member.cache_ttl_by_model.get("m"),
            Some(&60),
            "逐模型 TTL 也在抄写范围里"
        );
        assert_eq!(
            config_with_member.active_profile_id, config.active_profile_id,
            "池子是路由决定，不是配置切换：当前档案不许被动"
        );

        // 空 profile_id = 当前连接：只换模型名，其余一个字段都不动
        let mut config_with_pseudo = config.clone();
        overlay(
            &mut config_with_pseudo,
            &member("", "pseudo-model", 1).member,
        )
        .unwrap();
        assert_eq!(config_with_pseudo.base_url, "https://top.example/v1");
        assert_eq!(config_with_pseudo.temperature, 0.9);
        assert_eq!(config_with_pseudo.model, "pseudo-model");

        // 指向已删档案的成员：报错而不是悄悄改道
        let mut config_missing = config.clone();
        config_missing.profiles.clear();
        assert!(overlay(&mut config_missing, &member("prof-1", "m", 1).member).is_err());
    }
}
