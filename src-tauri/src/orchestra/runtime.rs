//! 编排的运行时零件：黑板、消息总线、并发位与工作队列。
//!
//! 这一层不认识模型、不认识文件——它只回答"谁在等谁、谁跟谁说话、同时能跑几个"。
//! 没有 tokio、没有 crossbeam（`Cargo.toml` 的依赖面），所以许可要自己数、
//! 窃取要自己搬、barrier 要自己等。这不是将就：一个能被单测的调度器比一个
//! "看起来更现代"的运行时值钱得多。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use crate::orchestra::graph::Plan;

// ---- 黑板：唯一被允许共享的可变状态 ----

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    pub value: String,
    /// 单调递增的版本。写的人必须先说自己看到的是哪一版
    pub version: u64,
    /// 谁写的（节点 id）。冲突要能报出作者，不然"两边都保留"只是两坨噪音
    pub author: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cas {
    /// 写进去了，新版本的号
    Applied(u64),
    /// 你看到的那一版已经不是当前版本了。`lost_key` 是你那一份现在住在哪——
    /// 顶回来的那一次要在账本上 pointing 得出它，不然人只知道打过架、不知道去哪看
    Conflict {
        held: u64,
        holder: String,
        lost_key: String,
    },
}

#[derive(Default)]
pub struct Blackboard {
    entries: Mutex<HashMap<String, Entry>>,
    /// 已经顶回来几次。只用来给 `#lost-N` 编号：**它是编号器，不是"冲突记录"那份事实**——
    /// 事实落在账本的一行 `conflict` 上（§5.34），所以这里不再另存一份 `Vec<Conflict>`：
    /// 同一个问题问两次（内存问一次、账本问一次）就是双轨真相的开端
    lost: Mutex<usize>,
}

impl Blackboard {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, key: &str) -> Option<Entry> {
        self.entries().get(key).cloned()
    }

    pub fn version_of(&self, key: &str) -> u64 {
        self.entries()
            .get(key)
            .map(|entry| entry.version)
            .unwrap_or(0)
    }

    /// 当前全部结论，按 key 排序。汇合与 DAG 视图都读它，不各读各的
    pub fn snapshot(&self) -> Vec<Entry> {
        let mut list: Vec<Entry> = self.entries().values().cloned().collect();
        list.sort_by(|a, b| a.key.cmp(&b.key));
        list
    }

    /// 带版本的写。**这是黑板唯一的入口**：留一个"总是成功"的快捷方法，
    /// 等于让冲突重新变成看不见的事
    pub fn compare_swap(&self, key: &str, expected: u64, value: &str, author: &str) -> Cas {
        let mut entries = self.entries();
        let held_version = entries.get(key).map(|entry| entry.version).unwrap_or(0);
        let holder = entries
            .get(key)
            .map(|entry| entry.author.clone())
            .unwrap_or_default();
        if held_version != expected {
            // 输的那一份不丢：另存一格，让**人**在那块「黑板键」里看得到它。
            // 别再往下多写一句"等汇合来裁决"——汇合的材料是每一格的结局，不是黑板，
            // 它从不读这一格（那条链路由 orchestrator 的结构钉看着）。静默覆盖是这里最坏的失败方式：
            // 它把"两个 agent 意见不一致"抹平了，而抹平之后没人知道该找谁裁
            let mut lost = self.lost();
            *lost += 1;
            let loser_key = format!("{key}#lost-{}", *lost);
            entries.insert(
                loser_key.clone(),
                Entry {
                    key: loser_key.clone(),
                    value: value.to_string(),
                    version: 1,
                    author: author.to_string(),
                },
            );
            return Cas::Conflict {
                held: held_version,
                holder,
                lost_key: loser_key,
            };
        }
        let version = held_version + 1;
        entries.insert(
            key.to_string(),
            Entry {
                key: key.to_string(),
                value: value.to_string(),
                version,
                author: author.to_string(),
            },
        );
        Cas::Applied(version)
    }

    /// 把一格往上顶 1。**它故意不走 `compare_swap`**。
    ///
    /// 计数器不是"两份意见争同一格"：要写的值是从**自己刚读到的那一版**算出来的，
    /// 而调用方手里那份读，天生就比锁旧。拿 `version_of(key)` 当 expected 去 swap，
    /// 等于每一次都递一个必然同意的门闩——CAS 看着在用，其实一次也拦不住，
    /// 别人在这两步中间顶上去的那一笔会被安静地覆盖掉（版本还在涨，值却是旧的）。
    /// 读与写在同一把锁里做完，所以顶 N 次之后格上就是 N
    pub fn bump(&self, key: &str, author: &str) -> u8 {
        let mut entries = self.entries();
        let held = entries.get(key);
        let used = held
            .and_then(|entry| entry.value.parse::<u8>().ok())
            .unwrap_or(0);
        let version = held.map(|entry| entry.version).unwrap_or(0);
        let next = used + 1;
        entries.insert(
            key.to_string(),
            Entry {
                key: key.to_string(),
                value: next.to_string(),
                version: version + 1,
                author: author.to_string(),
            },
        );
        next
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lost(&self) -> std::sync::MutexGuard<'_, usize> {
        self.lost.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

// ---- 消息总线 ----

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// 只有这一个节点收得到
    Point(String),
    /// 所有订阅者
    // 测试钉形、生产未接线：见文件底部 two_of_the_five… 守卫与 design-multi-agent.md §5.29。
    // 这里不能换 #[cfg(test)]——那会把枚举形状在生产构建里裁掉，且守卫测试按
    // "第一个 #[cfg(test)] 之前"切生产面，中途插会截断它
    #[allow(dead_code)]
    Broadcast,
    /// 一次请求-响应：回信只回到发起者那一侧
    #[allow(dead_code)] // 同上：请求-响应整条链今天只有测试在说
    Reply(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageKind {
    /// 结论/进展，单向
    Notice,
    /// 要一个回音
    #[allow(dead_code)] // 总线五通道里没接线的两条，见底部守卫测试
    Request,
    #[allow(dead_code)] // 同上
    Response,
    /// 这一代节点齐了（监督者-工作者模式里由 barrier 发出）
    BarrierPass,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    pub from: String,
    pub to: Destination,
    pub kind: MessageKind,
    pub body: String,
    /// 代数：请求-响应只认同一代里的回信，隔代的回音不能被当成答案
    pub gen: u64,
}

impl Envelope {
    pub fn notice(from: &str, to: Destination, body: &str, gen: u64) -> Self {
        Self {
            from: from.to_string(),
            to,
            kind: MessageKind::Notice,
            body: body.to_string(),
            gen,
        }
    }

    #[allow(dead_code)] // 生产侧没有点对点信封之外的读取者（构造回信的那一侧也只在测试里）
    pub fn to_point(&self) -> Option<&str> {
        match &self.to {
            Destination::Point(id) | Destination::Reply(id) => Some(id),
            Destination::Broadcast => None,
        }
    }

    /// 这条回信是不是这一代、是不是回给我的
    #[allow(dead_code)] // 请求-响应通道生产未接线，见底部 two_of_the_five… 守卫
    pub fn answers(&self, ask: &Envelope, me: &str) -> bool {
        self.kind == MessageKind::Response
            && self.gen == ask.gen
            && self.to == Destination::Reply(me.to_string())
    }
}

/// std mpsc 的订阅表。它不是运行时，只是一根能广播的管子
#[derive(Default)]
pub struct Bus {
    // 订阅者掉线（节点跑完、面板关了）时发送会失败：那时把它从表里摘掉，
    // 而不是让一条已经没人收的消息把整个 plan 卡住
    sinks: Mutex<Vec<(String, Sender<Envelope>)>>,
}

impl Bus {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn subscribe(&self, id: &str) -> Receiver<Envelope> {
        let (tx, rx) = mpsc::channel();
        self.sinks().push((id.to_string(), tx));
        rx
    }

    pub fn unsubscribe(&self, id: &str) {
        self.sinks().retain(|(held, _)| held != id);
    }

    #[allow(dead_code)] // 只被测试当观察口用；生产侧订阅表不对外报
    pub fn subscribers(&self) -> Vec<String> {
        self.sinks().iter().map(|(id, _)| id.clone()).collect()
    }

    /// 投递。点对点只给那一个订阅者，广播给全部。返回实际送达的人数
    pub fn deliver(&self, envelope: Envelope) -> usize {
        let mut delivered = 0;
        let mut dead: Vec<String> = Vec::new();
        {
            let sinks = self.sinks();
            for (id, tx) in sinks.iter() {
                let wanted = match &envelope.to {
                    Destination::Point(target) | Destination::Reply(target) => target == id,
                    Destination::Broadcast => true,
                };
                if !wanted {
                    continue;
                }
                match tx.send(envelope.clone()) {
                    Ok(()) => delivered += 1,
                    Err(_) => dead.push(id.clone()),
                }
            }
        }
        if !dead.is_empty() {
            let mut sinks = self.sinks();
            sinks.retain(|(id, _)| !dead.iter().any(|gone| gone == id));
        }
        delivered
    }

    // 请求-响应的发起侧。生产没接线（每 worker 多一次往返＝钱，见 §5.29），形状由测试钉着
    #[allow(dead_code)]
    pub fn request(&self, from: &str, to: &str, body: &str, gen: u64) -> Envelope {
        Envelope {
            from: from.to_string(),
            to: Destination::Point(to.to_string()),
            kind: MessageKind::Request,
            body: body.to_string(),
            gen,
        }
    }

    /// 回信回到发起者那一侧，代数照抄——改代数就等于另起一次问答
    #[allow(dead_code)] // 请求-响应通道生产未接线，见底部守卫测试
    pub fn reply_for(&self, ask: &Envelope, body: &str) -> Envelope {
        Envelope {
            from: ask.to_point().unwrap_or_default().to_string(),
            to: Destination::Reply(ask.from.clone()),
            kind: MessageKind::Response,
            body: body.to_string(),
            gen: ask.gen,
        }
    }

    fn sinks(&self) -> std::sync::MutexGuard<'_, Vec<(String, Sender<Envelope>)>> {
        self.sinks.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

// ---- barrier：等这一批全部落定 ----

/// 计数式 barrier。`arrive` 是"我这一份结了"，`wait` 只等一段时间：
/// 一个永远不 arrive 的节点不该把整个 plan 钉死，超时要让调度器看得见
#[derive(Clone)]
pub struct Barrier {
    // 不是构造时定死的字段：图会被 `grow_plan` 追加，"这一代该到几份"跟着变大。
    // 定死的话要么报"齐了"而实际差一份，要么永远等不到
    needed: Arc<AtomicUsize>,
    state: Arc<(Mutex<usize>, Condvar)>,
}

impl Barrier {
    pub fn new(needed: usize) -> Self {
        Self {
            needed: Arc::new(AtomicUsize::new(needed)),
            state: Arc::new((Mutex::new(0), Condvar::new())),
        }
    }

    pub fn arrive(&self) {
        let (lock, cvar) = &*self.state;
        let mut arrived = lock.lock().unwrap_or_else(PoisonError::into_inner);
        *arrived += 1;
        cvar.notify_all();
    }

    pub fn needed(&self) -> usize {
        self.needed.load(Ordering::Acquire)
    }

    /// 把该到的份数抬到 `expected`（只抬不降）。降下来等于把已经到过的算成没到
    pub fn raise(&self, expected: usize) {
        self.needed.fetch_max(expected, Ordering::AcqRel);
    }

    /// 齐了返回 true。需要 0 个人的 barrier 天生就是过了
    pub fn wait(&self, timeout: Duration) -> bool {
        if self.needed() == 0 {
            return true;
        }
        let (lock, cvar) = &*self.state;
        let guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        // 醒来先分清"等够了"和"等超时了"：半个批次不能当成整批通过
        // `_guard` 是有名字的绑定：它让锁活到这个表达式结束，而不是当场被丢掉
        let needed = self.needed();
        let (_guard, waited) = cvar
            .wait_timeout_while(guard, timeout, |count| *count < needed)
            .unwrap_or_else(PoisonError::into_inner);
        !waited.timed_out()
    }
}

/// 监督者-工作者那一圈的收发台：一代工作者把回报**点对点**投给它们的监督者，
/// barrier 记"这一代该到的到了没有"，齐的那一刻补一条 `BarrierPass`。
///
/// 它不是第二个黑板。黑板给的是结论（键 + 版本 + CAS，谁都可以去读），这里给的是
/// "谁在什么时候把哪一份交给了谁"：监督者的材料由这些回报拼出来，所以"哪一份没到"
/// 与"迟到的那一份还算不算答案"都是能回答的问题，而不是只能相信最后快照的那个人
pub struct Exchange {
    bus: Bus,
    supervisor: String,
    /// 收件箱。`None` = 监督者已经决策过，之后的回报不能再被当成它的答案
    inbox: Mutex<Option<Receiver<Envelope>>>,
    /// 投过报的是谁。判"齐了"要用集合而不是计数器：一个节点重跑过一次，
    /// 计数就会把它算成两份
    arrived: Mutex<BTreeSet<String>>,
    barrier: Barrier,
    gen: u64,
}

impl Exchange {
    pub fn new(supervisor: &str, expected: usize, gen: u64) -> Self {
        let bus = Bus::new();
        let inbox = bus.subscribe(supervisor);
        Self {
            bus,
            supervisor: supervisor.to_string(),
            inbox: Mutex::new(Some(inbox)),
            arrived: Mutex::new(BTreeSet::new()),
            barrier: Barrier::new(expected),
            gen,
        }
    }

    /// 这一代该到几份。图被追加时它会变大
    pub fn expect(&self, expected: usize) {
        self.barrier.raise(expected);
    }

    /// 工作者结了这一份。返回 `(送达几份, 这一代齐了没有)`——
    /// 送达 0 份是一件要记进账的事：那说明收件的那一方已经不听了
    pub fn report(&self, from: &str, text: &str) -> (usize, bool) {
        let fresh = self
            .arrived
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(from.to_string());
        if !fresh {
            // 同一个节点再报一次不算再齐一份：它大概是重跑过的那一格
            return (0, self.barrier.wait(Duration::ZERO));
        }
        self.barrier.arrive();
        let delivered = self.bus.deliver(Envelope::notice(
            from,
            Destination::Point(self.supervisor.clone()),
            text,
            self.gen,
        ));
        let settled = self.barrier.wait(Duration::ZERO);
        if settled && delivered > 0 {
            // 齐了这个事实本身也要是一条消息：监督者读收件箱就能看见"这一代完了"，
            // 不用再去问一个数
            self.bus.deliver(Envelope {
                from: "barrier".to_string(),
                to: Destination::Point(self.supervisor.clone()),
                kind: MessageKind::BarrierPass,
                body: format!("第 {} 代齐了", self.gen),
                gen: self.gen,
            });
        }
        (delivered, settled)
    }

    /// 把收到的消息按到达次序取走。取一次少一次：同一份回报不该被算两遍
    pub fn collect(&self) -> Vec<Envelope> {
        let mut held = self.inbox.lock().unwrap_or_else(PoisonError::into_inner);
        match held.as_mut() {
            Some(rx) => rx.try_iter().collect(),
            None => Vec::new(),
        }
    }

    /// 到过的都有谁。缺的那一份要说得出是谁没来
    pub fn arrived(&self) -> Vec<String> {
        self.arrived
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }

    #[allow(dead_code)] // 只被测试断"齐没齐"用；生产侧的齐了走 barrier 的 arrive/count 通路
    pub fn settled(&self) -> bool {
        self.barrier.wait(Duration::ZERO)
    }

    /// 监督者决策完了（或者这一发被取消）：关掉收件箱。
    /// 之后投进来的回报投递数是 0——那是一条要落账的事实，而不是一个可以忽略的返回值
    pub fn close(&self) {
        self.bus.unsubscribe(&self.supervisor);
        self.inbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }
}

/// 一个 plan 里所有监督者各自的收发台。按监督者 id 存，因为层级模式下
/// 每一层都有自己的"这一代"
#[derive(Default)]
pub struct Exchanges {
    inner: Mutex<BTreeMap<String, Arc<Exchange>>>,
}

impl Exchanges {
    pub fn new() -> Self {
        Self::default()
    }

    fn held(&self, supervisor: &str) -> Arc<Exchange> {
        let mut map = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        map.entry(supervisor.to_string())
            .or_insert_with(|| Arc::new(Exchange::new(supervisor, 0, 0)))
            .clone()
    }

    /// 工作者投报。`expected` 由调用方从图现算（"现在挂在它下面几个工作者"）
    pub fn report(
        &self,
        supervisor: &str,
        from: &str,
        text: &str,
        expected: usize,
    ) -> (usize, bool) {
        let one = self.held(supervisor);
        one.expect(expected);
        one.report(from, text)
    }

    pub fn collect(&self, supervisor: &str) -> Vec<Envelope> {
        self.held(supervisor).collect()
    }

    pub fn arrived(&self, supervisor: &str) -> Vec<String> {
        self.held(supervisor).arrived()
    }

    /// 关掉某个监督者的收件箱；传 `None` 是全关（取消那一圈要的就是这个）
    pub fn close(&self, supervisor: Option<&str>) {
        let map = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        match supervisor {
            Some(boss) => {
                if let Some(one) = map.get(boss) {
                    one.close();
                }
            }
            None => {
                for one in map.values() {
                    one.close();
                }
            }
        }
    }
}

/// 这条消息是回报、还是"这一代齐了"的公告。收件箱里两种都要，
/// 但只有前者能进监督者的材料：把公告当结论，等于让总线自己给自己派活
pub fn report_bodies(messages: &[Envelope]) -> Vec<String> {
    messages
        .iter()
        .filter(|item| item.kind == MessageKind::Notice)
        .map(|item| format!("{}：{}", item.from, item.body))
        .collect()
}

// ---- 并发上限、优先级与工作窃取 ----

/// 并发位。它存在的唯一理由：**没有运行时替我们数"现在有几个在跑"**
pub struct Permits {
    total: usize,
    held: Arc<AtomicUsize>,
}

impl Permits {
    pub fn new(total: usize) -> Self {
        Self {
            total: total.max(1),
            held: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn total(&self) -> usize {
        self.total
    }

    pub fn in_flight(&self) -> usize {
        self.held.load(Ordering::Acquire)
    }

    pub fn try_acquire(&self) -> Option<Permit> {
        let mut current = self.held.load(Ordering::Acquire);
        loop {
            if current >= self.total {
                return None;
            }
            match self.held.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(Permit {
                        held: self.held.clone(),
                    })
                }
                Err(latest) => current = latest,
            }
        }
    }
}

/// 一个并发位。它随 `Drop` 归还——所以"忘了归还"这件事在这里不可能发生，
/// 而不是靠调用一个 release() 的自觉
pub struct Permit {
    held: Arc<AtomicUsize>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.held.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 每个 worker 一条队列。`take_own` 是自己的活，`steal` 是偷别人的活：
/// 从尾巴偷能减少与宿主线程的碰撞，也偷不到刚刚压进去的那一个
#[derive(Default)]
pub struct Queue {
    pending: VecDeque<String>,
}

impl Queue {
    pub fn push(&mut self, id: &str) {
        self.pending.push_back(id.to_string());
    }

    pub fn take_own(&mut self) -> Option<String> {
        self.pending.pop_front()
    }

    pub fn steal(&mut self) -> Option<String> {
        self.pending.pop_back()
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

/// 一次派发：优先从自己的队列拿，空了就去偷别人尾巴上那一个。
/// worker 数不超过上限时窃取本来就是空转，所以那条路径也要有测试（不许它偷出自己的活）
pub fn dispatch(own: &mut Queue, others: &mut [&mut Queue]) -> Option<String> {
    if let Some(id) = own.take_own() {
        return Some(id);
    }
    for other in others.iter_mut() {
        if let Some(id) = other.steal() {
            return Some(id);
        }
    }
    None
}

/// 派出去的那一份活。`slot` 是它落在哪个 worker 位上，`stolen` 说它是不是偷来的——
/// 这两个数要能进账本，否则"当时到底是谁在跑这一份"就只有调度器自己知道
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub node: String,
    pub profile: String,
    pub slot: usize,
    pub stolen: bool,
}

/// 关键路径优先 + 工作窃取的那个组件。
///
/// 每个档案一组队列，组里一个 worker 位一条。**窃取不跨档案**：档案定的是用哪个模型、
/// 哪套工具与权限，把"读者"的活偷给"写代码的"等于换了一个没资格的人做这件事。
///
/// 它同时补上了派发的那笔账（[`Scheduler::handed_out`]）：`Running` 是 worker 线程自己写进
/// 状态表的，编排器派完那一刻并没有写，所以"这一轮 ready 里还有它"和"它已经在跑了"
/// 会同时成立一次——那份重复就是一笔白花掉的请求费
pub struct Scheduler {
    groups: Vec<Group>,
    /// 已经交出去、还没回来的活
    handed_out: HashSet<String>,
    /// 还在队列里的活。有了它，同一轮反复 offer 同一个 id 不会排出一串分身
    queued: HashSet<String>,
}

struct Group {
    profile: String,
    queues: Vec<Queue>,
    /// 下一次从哪条队列开始问。没有它，每轮都从 0 号位问起，第一个空位会把别人的活
    /// 一路偷光，而 `Assignment::slot` 退化成一个恒为 0 的读数
    cursor: usize,
}

impl Scheduler {
    pub fn new() -> Self {
        Self {
            groups: Vec::new(),
            handed_out: HashSet::new(),
            queued: HashSet::new(),
        }
    }

    /// 把每组的队列条数对到这一刻的上限上。缩容不许丢活：多出来的那几条队列并进留下的最后一条
    fn align(&mut self, slots: &dyn Fn(&str) -> usize) {
        for group in self.groups.iter_mut() {
            let wanted = slots(&group.profile).max(1);
            while group.queues.len() < wanted {
                group.queues.push(Queue::default());
            }
            if group.queues.len() > wanted {
                let mut moved: Vec<String> = Vec::new();
                for queue in group.queues[wanted..].iter_mut() {
                    moved.extend(drain(queue));
                }
                group.queues.truncate(wanted);
                // 这些 id 本来就在 `queued` 里记着，换个位次继续排，不必重记
                let last = group.queues.last_mut().expect("wanted 至少是 1");
                for id in moved {
                    last.push(&id);
                }
            }
        }
    }

    /// 收一轮 ready：先按关键路径排序，再按档案分进各自的组，活进的是当前最短的那条。
    /// `slots` 给的是这一刻该档案开几路（池子缩了它就跟着缩）
    pub fn offer(&mut self, plan: &Plan, ready: &[String], slots: &dyn Fn(&str) -> usize) {
        self.align(slots);
        for node_id in prioritize(plan, ready) {
            if self.handed_out.contains(&node_id) || self.queued.contains(&node_id) {
                continue;
            }
            let Some(profile_name) = plan.find(&node_id).map(|node| node.profile.clone()) else {
                // 图里没有的 id 不派发，也不留下"排过队"的痕迹：
                // 一份不存在的活占着账，回来的是"这个节点永远不动"
                continue;
            };
            let index = match self
                .groups
                .iter()
                .position(|group| group.profile == profile_name)
            {
                Some(found) => found,
                None => {
                    // 组的先后就是它们第一次出现的先后，而那个顺序来自关键路径排序——
                    // 所以"哪一组先拿到派发位"这件事是可复现的，不看 HashMap 的脸色
                    let wanted = slots(&profile_name).max(1);
                    self.groups.push(Group {
                        profile: profile_name.clone(),
                        queues: (0..wanted).map(|_| Queue::default()).collect(),
                        cursor: 0,
                    });
                    self.groups.len() - 1
                }
            };
            let group = &mut self.groups[index];
            let shortest = group
                .queues
                .iter()
                .enumerate()
                .min_by_key(|(_, queue)| queue.len())
                .map(|(slot, _)| slot)
                .unwrap_or(0);
            group.queues[shortest].push(&node_id);
            self.queued.insert(node_id);
        }
    }

    /// 下一个该派发的活。`allows` 说这个档案这一刻能不能碰（熔断就返回 false）——
    /// 跳过它是**跳过一组**，不是停下整轮：读者被熔断时，写代码的那一组照样该拿到派发位
    ///
    /// 每组从自己的游标问起，一位问完再问下一位。问到空位时 [`dispatch`] 会走到窃取分支，
    /// 从同伴尾巴上取一份——池子缩过容之后最容易撞见这种不均（并进留下的那条队列比同伴长）
    pub fn next(&mut self, allows: &dyn Fn(&str) -> bool) -> Option<Assignment> {
        for group in self.groups.iter_mut() {
            if !allows(&group.profile) {
                continue;
            }
            let slots = group.queues.len();
            group.cursor %= slots;
            for step in 0..slots {
                let slot = (group.cursor + step) % slots;
                let (peers_before, rest) = group.queues.split_at_mut(slot);
                let (own, peers_after) = rest.split_first_mut().expect("每组至少一条队列");
                let before = own.len();
                let mut others: Vec<&mut Queue> = peers_before
                    .iter_mut()
                    .chain(peers_after.iter_mut())
                    .collect();
                let taken = dispatch(own, &mut others);
                // 自己那条没短，说明拿到的那一份是从同伴尾巴上来的
                let stolen = taken.is_some() && own.len() == before;
                if let Some(node) = taken {
                    self.queued.remove(&node);
                    self.handed_out.insert(node.clone());
                    group.cursor = (slot + 1) % slots;
                    return Some(Assignment {
                        node,
                        profile: group.profile.clone(),
                        slot,
                        stolen,
                    });
                }
            }
        }
        None
    }

    /// 这一份活回来了（落定、失败、panic 都算回来）。它是"重跑单个节点"能再进队列的前提：
    /// 不销账，那个 id 就永远挂在 handed_out 里，第二次派发等于没实现
    pub fn retire(&mut self, id: &str) {
        self.handed_out.remove(id);
        self.queued.remove(id);
    }

    /// 把刚拿到手的活放回它原来那条队列。令牌不够、派发位没抢到都走这里——
    /// 不还的话它就算"已经交出去了"，那一格活从此只在账本上存在过
    pub fn put_back(&mut self, assignment: &Assignment) {
        self.handed_out.remove(&assignment.node);
        let mut returned = false;
        if let Some(group) = self
            .groups
            .iter_mut()
            .find(|held| held.profile == assignment.profile)
        {
            if let Some(queue) = group.queues.get_mut(assignment.slot) {
                queue.push(&assignment.node);
                returned = true;
            }
        }
        // 位次已经被缩容撤掉了：那就当它没排过队，下一轮的 ready 会重新把它排进来
        if returned {
            self.queued.insert(assignment.node.clone());
        }
    }

    /// 撤掉某个档案还在排队的活，把 id 交回调用方去落定状态——熔断走的就是这一手。
    /// 留在队里不叫"等它 healed"，叫"看上去还在跑"
    pub fn drop_profile(&mut self, profile: &str) -> Vec<String> {
        let mut dropped: Vec<String> = Vec::new();
        if let Some(group) = self.groups.iter_mut().find(|held| held.profile == profile) {
            for queue in group.queues.iter_mut() {
                dropped.extend(drain(queue));
            }
        }
        for id in &dropped {
            self.queued.remove(id);
        }
        dropped
    }

    /// 队列里只留下还在 `ready` 里的那些，返回被撤掉的。
    ///
    /// 每一轮都要对一次账：`ready` 是从现在的状态表算出来的，队列是过去某一刻排好的。
    /// 上游被跳过、被取消、被条件边判死的那些活会留在队里，而"反正下一轮还会再算一遍"
    /// 不能替它做决定——那一份留在队里就是一颗还会被派出去的雷（也就是白付的一次请求）
    pub fn retain(&mut self, ready: &[String]) -> Vec<String> {
        let kept: HashSet<&String> = ready.iter().collect();
        let mut dropped: Vec<String> = Vec::new();
        for group in self.groups.iter_mut() {
            for queue in group.queues.iter_mut() {
                for id in drain(queue) {
                    if kept.contains(&id) {
                        queue.push(&id);
                    } else {
                        self.queued.remove(&id);
                        dropped.push(id);
                    }
                }
            }
        }
        dropped
    }

    /// 还在队列里排着的份数。池子扩缩读的是它，不是"ready 有几个"
    pub fn pending(&self) -> usize {
        self.groups
            .iter()
            .map(|group| group.queues.iter().map(Queue::len).sum::<usize>())
            .sum()
    }

    /// 有没有派发中的活。`orchestra_rerun_node` 绕开调度器起线程，所以这个数会骗人——
    /// 编排器的收尾判据用的是 [`Scheduler::is_idle`] 加它自己那份 `running`。
    /// 生产因此不读它，只留测试钉"派发计数"的语义
    #[allow(dead_code)]
    pub fn handed_out_len(&self) -> usize {
        self.handed_out.len()
    }

    /// 队列全空。收尾时它还非空，说明有活排着却没人派——那不能叫"跑完了"
    pub fn is_idle(&self) -> bool {
        self.groups
            .iter()
            .all(|group| group.queues.iter().all(Queue::is_empty))
    }

    /// 每个档案的队列深度，按组顺序。给账本和界面看"谁在排队"用。
    /// 界面今天读的是别的口径（面板有自己的计数），这条先只活在测试里
    #[allow(dead_code)]
    pub fn backlog(&self) -> Vec<(String, usize)> {
        self.groups
            .iter()
            .map(|group| {
                (
                    group.profile.clone(),
                    group.queues.iter().map(Queue::len).sum(),
                )
            })
            .collect()
    }
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

/// 把一条队列清空并把倒出来的 id 按原顺序交出去
fn drain(queue: &mut Queue) -> Vec<String> {
    let mut out = Vec::new();
    while let Some(id) = queue.take_own() {
        out.push(id);
    }
    out
}

/// 每个 profile 一组 worker 的租约表。上限是硬数出来的：
/// "每条边一个线程"的写法会在扇出 200 个节点时把机器变成一台取暖器。
///
/// 当前的上限（`cap`）可以在 `ceiling` 以下动，这就是"动态扩缩"：配置里的
/// `max_parallel` 永远是天花板，`cap` 只是这一刻愿意开几路
#[derive(Default)]
pub struct Pool {
    ceiling: usize,
    cap: AtomicUsize,
    live: Arc<AtomicUsize>,
}

impl Pool {
    pub fn new(max: usize) -> Self {
        let cap = max.max(1);
        Self {
            ceiling: cap,
            cap: AtomicUsize::new(cap),
            live: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn lease(&self) -> Option<WorkerLease> {
        let limit = self.cap.load(Ordering::Acquire);
        let mut current = self.live.load(Ordering::Acquire);
        loop {
            if current >= limit {
                return None;
            }
            match self.live.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(WorkerLease {
                        live: self.live.clone(),
                    })
                }
                Err(latest) => current = latest,
            }
        }
    }

    pub fn live(&self) -> usize {
        self.live.load(Ordering::Acquire)
    }

    /// 这一刻的上限。它可能已经被 `scale` 砍过，所以报"现在几个 / 上限几个"要连着
    /// [`Pool::ceiling`] 一起报，否则界面上看不出池子缩过
    pub fn max(&self) -> usize {
        self.cap.load(Ordering::Acquire)
    }

    /// 配置给的那个数：池子再怎么动都不许越过它
    pub fn ceiling(&self) -> usize {
        self.ceiling
    }

    /// 按这一轮的队列深度与失败数调一次上限。两条规则，都只动一步以内，
    /// 而且**缩的理由永远压过扩的理由**：
    /// - 这一轮有节点失败：上限砍一半（至少留 1）。服务商在拒我们时多开几路只会更多笔废钱。
    /// - 没失败、还有活排着：加一手，最多加到配置的上限——也就是把上一次砍掉的那一手还回来。
    ///
    /// 所以池子的常态是"贴着天花板"，它真正做的事是**失败驱动的退让 + 有活时的回升**。
    /// 它只在派发之前被调用，因此永远不会把已经在跑的那几路掐掉：顶住的是派发，
    /// 不是正在跑的请求（与预算闸同一条规矩）。返回调整后的上限，调用方要落账就用它
    pub fn scale(&self, backlog: usize, failures: usize) -> usize {
        let current = self.cap.load(Ordering::Acquire);
        let next = if failures > 0 {
            (current / 2).max(1)
        } else if backlog > 0 {
            (current + 1).min(self.ceiling)
        } else {
            current
        };
        self.cap.store(next, Ordering::Release);
        next
    }
}

pub struct WorkerLease {
    live: Arc<AtomicUsize>,
}

impl Drop for WorkerLease {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 关键路径优先：剩余链最长的先跑，同深度按 id 定序（可复现，不看完成时刻）
pub fn prioritize(plan: &Plan, ready: &[String]) -> Vec<String> {
    let mut sorted = ready.to_vec();
    sorted.sort_by(|a, b| {
        plan.depth_of(b)
            .cmp(&plan.depth_of(a))
            .then_with(|| a.cmp(b))
    });
    sorted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestra::graph::{Edge, Node};

    fn node(id: &str, deps: &[&str]) -> Node {
        Node {
            id: id.into(),
            goal: format!("{id} 要做的事"),
            profile: "worker".into(),
            depends_on: deps.iter().map(|dep| dep.to_string()).collect(),
            edge: Edge::FinishToStart,
            max_attempts: 1,
        }
    }

    /// 冲突留下的那几份副本。**注意这不是"冲突记录"那份事实**——事实现在住在账本的一行
    /// `conflict` 上（orchestrator 那边），这里只数黑板上还看得见几格 `#lost-N`
    fn lost_copies(board: &Blackboard) -> usize {
        board
            .snapshot()
            .into_iter()
            .filter(|entry| entry.key.contains("#lost"))
            .count()
    }

    #[test]
    fn a_stale_cas_write_loses_but_stays_on_the_board() {
        let board = Blackboard::new();
        assert_eq!(
            board.compare_swap("plan", 0, "第一版", "a"),
            Cas::Applied(1)
        );
        // b 后来写成了第二版，a 拿着一版的版本号再写就必须撞
        assert_eq!(
            board.compare_swap("plan", 1, "b 的第二版", "b"),
            Cas::Applied(2)
        );
        let decision = board.compare_swap("plan", 1, "a 的旧改动", "a");
        assert_eq!(
            decision,
            Cas::Conflict {
                held: 2,
                holder: "b".into(),
                lost_key: "plan#lost-1".into()
            },
            "输家要知道是谁赢了、自己那一份去哪了，否则界面上没法解释这一条为什么没生效"
        );
        assert_eq!(
            board.get("plan").unwrap().value,
            "b 的第二版",
            "输的那一笔不该改动当前结论"
        );
        assert_eq!(lost_copies(&board), 1, "冲突要留痕，不能抹平");
        let kept = board
            .snapshot()
            .into_iter()
            .find(|entry| entry.value == "a 的旧改动")
            .expect("输的那一份必须还在黑板上");
        assert_eq!(kept.author, "a");
    }

    /// 计数器那一类写走 `bump`，不走"读一版再拿当下版本去 swap"。
    /// 下面那段对照不是假想：它就是旧写法——两个作者各自读到 1、各自算出 2，
    /// **两次都返回 `Applied`**，格上只涨了一格，冲突表还是空的。
    /// CAS 在用，但它每次收到的都是"必然同意"的门闩
    #[test]
    fn a_bump_adds_up_where_swapping_the_current_version_silently_drops_one() {
        let board = Blackboard::new();
        assert_eq!(board.bump("rounds", "seed"), 1);
        assert_eq!(board.bump("rounds", "a"), 2, "顶一次就该多一格");
        assert_eq!(board.bump("rounds", "b"), 3);
        assert_eq!(board.get("rounds").unwrap().value, "3");
        assert_eq!(lost_copies(&board), 0, "顶账不是两份意见打架，不该另存一份");

        let old = Blackboard::new();
        old.compare_swap("rounds", old.version_of("rounds"), "1", "seed");
        let seen_by_a = old.get("rounds").unwrap().value.clone();
        let seen_by_b = old.get("rounds").unwrap().value.clone();
        let first = old.compare_swap(
            "rounds",
            old.version_of("rounds"),
            &(seen_by_a.parse::<u8>().unwrap() + 1).to_string(),
            "a",
        );
        let second = old.compare_swap(
            "rounds",
            old.version_of("rounds"),
            &(seen_by_b.parse::<u8>().unwrap() + 1).to_string(),
            "b",
        );
        assert!(
            matches!(first, Cas::Applied(_)) && matches!(second, Cas::Applied(_)),
            "旧写法的每一步都该是「同意」——这正是它拦不住东西的证据：{first:?} / {second:?}"
        );
        assert_eq!(
            old.get("rounds").unwrap().value,
            "2",
            "两位各顶一次，格上却只有一次：那一笔是静默丢的"
        );
        assert_eq!(lost_copies(&old), 0, "静默丢账连一份副本都不留");
    }

    /// 真并发下顶 32 次，格上就是 32。对**正确的实现**这条是确定的（读与写在同一把锁里）；
    /// 只要有人把 `bump` 拆回"先读、放开锁、再 swap 当下版本"，它就会掉数——
    /// 掉的还不是随机值，而是每次都返回 `Applied` 的那种掉法
    #[test]
    fn concurrent_bumps_add_up() {
        let board = std::sync::Arc::new(Blackboard::new());
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let board = std::sync::Arc::clone(&board);
                std::thread::spawn(move || {
                    for _ in 0..8 {
                        board.bump("rounds", "worker");
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().expect("顶账的线程不该 panic");
        }
        assert_eq!(
            board.get("rounds").unwrap().value,
            "32",
            "32 次顶账被吃成了更少的格"
        );
        assert_eq!(lost_copies(&board), 0, "计数器不是两个意见打架");
    }

    #[test]
    fn a_first_write_needs_no_prior_version() {
        let board = Blackboard::new();
        assert_eq!(board.version_of("fresh"), 0, "还没有过的 key 的版本是 0");
        assert_eq!(board.compare_swap("fresh", 0, "v1", "a"), Cas::Applied(1));
        assert_eq!(lost_copies(&board), 0, "顺序写不是冲突");
    }

    #[test]
    fn point_to_point_reaches_one_and_broadcast_reaches_all() {
        let bus = Bus::new();
        let one = bus.subscribe("one");
        let two = bus.subscribe("two");
        assert_eq!(
            bus.deliver(Envelope::notice(
                "one",
                Destination::Point("two".into()),
                "只给 two",
                1
            )),
            1
        );
        assert!(one.try_recv().is_err(), "点对点不该被第三个人听见");
        assert!(two.try_recv().is_ok());
        assert_eq!(
            bus.deliver(Envelope::notice("one", Destination::Broadcast, "大家", 1)),
            2
        );
        assert!(one.try_recv().is_ok());
    }

    #[test]
    fn a_reply_is_accepted_only_from_the_same_generation() {
        let bus = Bus::new();
        let ask = bus.request("supervisor", "worker", "查一下", 3);
        let answer = bus.reply_for(&ask, "查到了");
        assert!(answer.answers(&ask, "supervisor"), "同一代的回信要认");
        let stale = Envelope {
            gen: 4,
            ..answer.clone()
        };
        assert!(
            !stale.answers(&ask, "supervisor"),
            "隔代的回音不能当答案，否则 replanning 会拿旧结论决定新节点"
        );
        assert!(!answer.answers(&ask, "someone-else"), "回信只属于发起者");
    }

    #[test]
    fn a_dead_subscriber_is_dropped_instead_of_stalling_the_plan() {
        let bus = Bus::new();
        let gone = bus.subscribe("gone");
        let alive = bus.subscribe("alive");
        drop(gone);
        let delivered = bus.deliver(Envelope::notice("x", Destination::Broadcast, "hi", 0));
        assert_eq!(delivered, 1, "掉线的订阅者不该被算成送达");
        assert!(alive.try_recv().is_ok());
        assert_eq!(
            bus.subscribers(),
            vec!["alive".to_string()],
            "掉线的要被摘掉"
        );
    }

    #[test]
    fn a_barrier_holds_until_the_whole_generation_arrives() {
        let barrier = Barrier::new(2);
        barrier.arrive();
        assert!(
            !barrier.wait(Duration::from_millis(20)),
            "2 个人里只到了 1 个，不算过"
        );
        barrier.arrive();
        assert!(barrier.wait(Duration::from_millis(20)));
        assert!(
            Barrier::new(0).wait(Duration::from_millis(1)),
            "不需要等人的 barrier 天生是过了"
        );
    }

    #[test]
    fn permits_never_let_more_than_the_cap_run_and_return_on_drop() {
        let permits = Permits::new(2);
        let first = permits.try_acquire().expect("1");
        let second = permits.try_acquire().expect("2");
        assert_eq!(permits.in_flight(), 2);
        assert!(
            permits.try_acquire().is_none(),
            "上限=2 时第三个必须拿不到位——验收第 7 条就靠这一句"
        );
        drop(first);
        assert_eq!(
            permits.in_flight(),
            1,
            "并发位随作用域归还，不靠记得调 release"
        );
        let third = permits.try_acquire().expect("3");
        assert_eq!(permits.in_flight(), 2);
        drop((second, third));
        assert_eq!(permits.in_flight(), 0);
    }

    #[test]
    fn workers_take_from_the_head_of_their_own_and_steal_from_the_tail() {
        let mut own = Queue::default();
        let mut neighbor = Queue::default();
        for id in ["n1", "n2", "n3"] {
            neighbor.push(id);
        }
        assert_eq!(
            dispatch(&mut own, &mut [&mut neighbor]),
            Some("n3".to_string()),
            "偷尾巴：刚压进去的那一个留给宿主，减少抢同一件活"
        );
        assert_eq!(neighbor.len(), 2);
        assert_eq!(
            dispatch(&mut own, &mut [&mut neighbor]),
            Some("n2".to_string())
        );

        own.push("mine");
        assert_eq!(
            dispatch(&mut own, &mut [&mut neighbor]),
            Some("mine".to_string()),
            "自己有活时不该去偷，否则窃取会把顺序打乱"
        );
        assert_eq!(neighbor.len(), 1, "自己有的活不该顺手拿走别人的");
        let mut idle = Queue::default();
        let mut empty = Queue::default();
        assert_eq!(
            dispatch(&mut idle, &mut [&mut empty]),
            None,
            "没人有活就是没活"
        );
    }

    #[test]
    fn the_critical_path_wins_the_dispatch_order() {
        let plan = Plan::new(
            "p",
            "g",
            vec![
                node("a", &[]),
                node("b", &["a"]),
                node("c", &["b"]),
                node("d", &["a"]),
            ],
        );
        let ready = vec!["d".to_string(), "c".to_string()];
        assert_eq!(
            prioritize(&plan, &ready),
            vec!["c".to_string(), "d".to_string()],
            "剩余链最长的那一支先派，plan 的总时长才不会被拖到最后才发现"
        );
        let flat = Plan::new("p", "g", vec![node("y", &[]), node("z", &[])]);
        assert_eq!(
            prioritize(&flat, &["z".to_string(), "y".to_string()]),
            vec!["y".to_string(), "z".to_string()],
            "同深度时按 id 定序：两次运行给出同一个派发顺序"
        );
    }

    #[test]
    fn the_pool_refuses_more_workers_than_the_cap() {
        let pool = Pool::new(2);
        let one = pool.lease().expect("1");
        let two = pool.lease().expect("2");
        assert_eq!(pool.live(), 2);
        assert!(pool.lease().is_none(), "线程数不能自己长出来");
        drop(one);
        assert_eq!(pool.live(), 1);
        assert!(pool.lease().is_some());
        let _ = two;
    }

    /// 动态扩缩：缩是失败驱动的（砍一半、至少留一手），扩是把砍掉的还回来，
    /// 而且永远越不过配置给的那个数。开局贴着上限——验收里"≥3 个并行"
    /// 不该被一个慢启动挡掉
    #[test]
    fn the_pool_backs_off_on_failures_and_recovers_one_step_at_a_time() {
        let pool = Pool::new(4);
        assert_eq!(pool.max(), 4, "开局就按配置的上限跑");
        assert_eq!(pool.scale(3, 0), 4, "已经在天花板上，扩无可扩");
        assert_eq!(pool.scale(0, 3), 2, "这一轮有失败就砍一半");
        assert_eq!(pool.scale(0, 1), 1, "再砍也要留一手，全砍等于不跑了");

        let first = pool.lease();
        assert!(first.is_some(), "剩下的一手还得给得出");
        assert!(pool.lease().is_none(), "缩下去的上限要真的挡住派发");
        drop(first);

        assert_eq!(pool.scale(5, 0), 2, "没失败又有积压，就还一手");
        assert_eq!(pool.scale(5, 0), 3);
        assert_eq!(pool.scale(5, 0), 4);
        assert_eq!(pool.scale(5, 0), 4, "回到天花板之后不再长");
    }

    /// 配对断言：一边钉"连续扩越不过 ceiling"，一边钉"砍上限不回收已经在跑的租约"
    #[test]
    fn scaling_never_crosses_the_ceiling_or_recalls_a_running_lease() {
        let pool = Pool::new(3);
        let held = pool.lease().expect("一路");
        assert_eq!(pool.scale(9, 5), 1, "砍上限不该影响已经握着的那一路");
        assert_eq!(pool.live(), 1, "在跑的那一路还在计数里，租约不能被回收");
        drop(held);
        assert_eq!(pool.live(), 0, "守卫归还之后才算少一路");

        for _ in 0..10 {
            pool.scale(50, 0);
        }
        assert_eq!(pool.max(), 3, "连续扩也越不过配置的那个数");
        assert_eq!(pool.ceiling(), 3, "天花板自己不动");
    }

    fn node_as(id: &str, profile: &str, deps: &[&str]) -> Node {
        Node {
            id: id.into(),
            goal: format!("{id} 要做的事"),
            profile: profile.into(),
            depends_on: deps.iter().map(|dep| dep.to_string()).collect(),
            edge: Edge::FinishToStart,
            max_attempts: 1,
        }
    }

    #[test]
    fn a_handed_out_node_waits_to_be_retired_before_it_queues_again() {
        // 编排器每一轮都从状态表重算 ready，而 `Running` 是 worker 线程自己写的：
        // 没有这笔账，同一个节点会被派两次，也就是两次付费请求
        let plan = Plan::new("p", "g", vec![node("a", &[]), node("b", &[])]);
        let mut sched = Scheduler::new();
        sched.offer(&plan, &["a".into(), "b".into()], &|_| 2);
        assert_eq!(sched.pending(), 2);

        let first = sched.next(&|_| true).expect("有活就该派出去");
        sched.offer(&plan, &["a".into(), "b".into()], &|_| 2);
        assert_eq!(sched.pending(), 1, "已经交出去的活不该再排一遍队");

        let second = sched.next(&|_| true).expect("另一份还在等");
        assert_ne!(second.node, first.node);
        assert!(sched.next(&|_| true).is_none(), "两份都出去了");
        assert_eq!(sched.handed_out_len(), 2);

        sched.retire(&first.node);
        sched.offer(&plan, &["a".into()], &|_| 2);
        assert_eq!(
            sched.next(&|_| true).map(|held| held.node),
            Some("a".to_string()),
            "销过账的活才可能重跑——「重跑单个节点」靠的是这一手"
        );
    }

    #[test]
    fn a_gated_profile_skips_its_group_instead_of_stopping_the_round() {
        let plan = Plan::new(
            "p",
            "g",
            vec![node_as("r1", "reader", &[]), node_as("w1", "writer", &[])],
        );
        let mut sched = Scheduler::new();
        sched.offer(&plan, &["r1".into(), "w1".into()], &|_| 1);
        let taken = sched
            .next(&|profile| profile != "reader")
            .expect("读者被熔断，写代码的那个照样该拿到派发位");
        assert_eq!(taken.profile, "writer");
        assert_eq!(taken.node, "w1");
    }

    #[test]
    fn an_idle_slot_takes_the_tail_of_its_busiest_peer() {
        let nodes: Vec<Node> = (0..4)
            .map(|index| node_as(&format!("n{index}"), "worker", &[]))
            .collect();
        let plan = Plan::new("p", "g", nodes);
        let mut sched = Scheduler::new();
        sched.offer(
            &plan,
            &["n0".into(), "n1".into(), "n2".into(), "n3".into()],
            &|_| 4,
        );
        // 池子缩到 2 路：被撤掉的那两条队列并进留下的，一份活都不许丢
        sched.offer(&plan, &[], &|_| 2);

        let mut got: Vec<Assignment> = Vec::new();
        while let Some(assignment) = sched.next(&|_| true) {
            got.push(assignment);
        }
        assert_eq!(got.len(), 4, "缩容不该让任何一份活消失");
        let stolen: Vec<&Assignment> = got.iter().filter(|held| held.stolen).collect();
        assert_eq!(stolen.len(), 1, "只该抹平一次不均");
        assert_eq!(
            stolen[0].node, "n3",
            "偷的是同伴尾巴上那一个，不是它队头的那一个"
        );
        assert_eq!(stolen[0].slot, 0, "是空着的那个位次拿到了这份活");
        assert!(
            got.iter().all(|held| held.profile == "worker"),
            "窃取不跨档案"
        );
        assert_eq!(
            got.iter().filter(|held| held.slot == 0).count(),
            2,
            "位次轮着问，两份活落在 0 号位、另两份落在 1 号位——不然 slot 这个读数没意义"
        );
    }

    #[test]
    fn the_critical_path_decides_who_takes_the_first_slot() {
        let plan = Plan::new(
            "p",
            "g",
            vec![
                node("a", &[]),
                node("b", &["a"]),
                node("c", &["b"]),
                node("x", &[]),
            ],
        );
        let mut sched = Scheduler::new();
        // 两个都能跑：c 后面还挂着链，x 是孤零零的一格
        sched.offer(&plan, &["c".into(), "x".into()], &|_| 1);
        assert_eq!(
            sched.next(&|_| true).map(|held| held.node),
            Some("c".to_string()),
            "剩余链最长的那一支先占派发位"
        );
    }

    #[test]
    fn the_backlog_readings_add_up_and_an_unknown_id_never_queues() {
        let plan = Plan::new("p", "g", vec![node("a", &[]), node("b", &[])]);
        let mut sched = Scheduler::new();
        assert!(sched.is_idle(), "空的调度器不该被当成有活");

        sched.offer(&plan, &["ghost".into()], &|_| 2);
        assert!(sched.is_idle(), "图里没有的 id 不该占着队列");
        assert_eq!(sched.next(&|_| true).map(|held| held.node), None);

        sched.offer(&plan, &["a".into(), "b".into()], &|_| 2);
        assert_eq!(sched.pending(), 2);
        assert_eq!(
            sched.backlog(),
            vec![("worker".to_string(), 2usize)],
            "谁在排队要报得出来"
        );
        assert!(!sched.is_idle(), "还排着活就不能叫跑完了");
        sched.next(&|_| true);
        assert_eq!(sched.pending(), 1);
        assert_eq!(sched.handed_out_len(), 1);
    }

    #[test]
    fn a_work_item_that_stops_being_ready_leaves_the_queue() {
        let mut nodes = vec![
            node("a", &[]),
            node("b", &["a"]),
            node("c", &["a"]),
            node("d", &[]),
        ];
        for held in nodes.iter_mut() {
            if held.id == "b" || held.id == "c" {
                held.profile = "writer".into();
            }
        }
        let plan = Plan::new("p", "g", nodes);
        let mut sched = Scheduler::new();
        // b、c 的上游 a 还没落定，ready 里只有 d
        sched.offer(&plan, &["d".into()], &|_| 1);
        assert_eq!(
            sched.drop_profile("reader"),
            Vec::<String>::new(),
            "没排过队的档案撤不出活"
        );
        assert_eq!(
            sched.drop_profile("worker"),
            vec!["d".to_string()],
            "熔断要把排着的活交回去落状态"
        );
        assert!(sched.is_idle());

        // 先排进队，然后上游死了：b 不再 ready，它不该还躺在队里等一次白付的请求
        sched.offer(&plan, &["b".into(), "c".into()], &|_| 1);
        assert_eq!(sched.pending(), 2);
        assert_eq!(
            sched.retain(&[]),
            vec!["b".to_string(), "c".to_string()],
            "撤掉的要说得出来是哪两份"
        );
        assert!(sched.is_idle());
        // 撤掉的没进"已交出"那笔账，所以它还能被重新排进来
        sched.offer(&plan, &["b".into()], &|_| 1);
        assert_eq!(
            sched.next(&|_| true).map(|held| held.node),
            Some("b".to_string())
        );
    }

    #[test]
    fn a_taken_work_item_goes_back_when_there_is_no_slot_for_it() {
        let plan = Plan::new(
            "p",
            "g",
            vec![node("a", &[]), node("b", &[]), node("c", &[])],
        );
        let mut sched = Scheduler::new();
        sched.offer(&plan, &["a".into(), "b".into(), "c".into()], &|_| 2);
        let taken = sched.next(&|_| true).expect("先拿到一份");
        assert_eq!(taken.node, "a");
        assert_eq!(sched.handed_out_len(), 1, "拿到手就算交出去了");

        sched.put_back(&taken);
        assert_eq!(sched.handed_out_len(), 0, "还回来就不算交出");
        assert_eq!(sched.pending(), 3, "三份都该在队里，一份都不能少");

        // 位次被缩容撤掉之后，还回来的活不占队列，但也别占着"已交出"那笔账
        let second = sched.next(&|_| true).expect("下一个位次上的那份");
        assert_eq!((second.node.as_str(), second.slot), ("b", 1));
        sched.offer(&plan, &[], &|_| 1);
        sched.put_back(&second);
        assert_eq!(sched.handed_out_len(), 0, "撤掉的位次不该把它锁死");
        assert_eq!(sched.pending(), 2, "它这一下确实不在队里");
        sched.offer(&plan, &["b".into()], &|_| 1);
        assert_eq!(sched.pending(), 3, "但下一轮的 ready 能把它重新排进来");
    }

    /// 点对点回报：只有它那位监督者收得到。广播式回报会让层级模式里每一层
    /// 都收到别人那一份，那不是"共享工作目录"该有的样子
    #[test]
    fn a_workers_report_lands_only_in_its_own_supervisors_inbox() {
        let ex = Exchange::new("boss", 2, 0);
        let eavesdrop = ex.bus.subscribe("别人家的节点");
        assert_eq!(ex.report("w1", "第一份").0, 1, "该送到一位");
        assert_eq!(eavesdrop.try_iter().count(), 0, "旁听的收不到点对点那一份");

        let got = ex.collect();
        assert_eq!(got.len(), 1);
        assert_eq!(
            (got[0].from.as_str(), got[0].body.as_str()),
            ("w1", "第一份")
        );
        assert!(
            ex.collect().is_empty(),
            "同一份回报不能被读两遍：读两遍就会算两遍"
        );
    }

    /// 齐了的那一刻要是一条消息，不是只是一个数；没齐的时候要报得出谁还没来
    #[test]
    fn the_barrier_announces_the_generation_settling() {
        let ex = Exchange::new("boss", 2, 3);
        assert!(!ex.report("w1", "一份").1, "两份里只到一份，不能报齐");
        assert_eq!(ex.arrived(), vec!["w1".to_string()], "缺的是谁要说得出来");

        let (delivered, settled) = ex.report("w2", "两份");
        assert_eq!(delivered, 1);
        assert!(settled);
        let kinds: Vec<MessageKind> = ex.collect().iter().map(|item| item.kind.clone()).collect();
        assert_eq!(
            kinds,
            vec![
                MessageKind::Notice,
                MessageKind::Notice,
                MessageKind::BarrierPass
            ],
            "两份回报都留着（监督者一次读干净，不用回来第二次），公告排在最后"
        );
    }

    /// 同一个节点再报一次不算再齐一份——它多半就是被重跑的那一格
    #[test]
    fn a_worker_that_reports_twice_is_still_one_arrival() {
        let ex = Exchange::new("boss", 1, 0);
        assert!(ex.report("w1", "第一次").1);
        let again = ex.report("w1", "第二次");
        assert_eq!(again.0, 0, "重复的那一份不该再投进收件箱");
        assert_eq!(
            ex.collect()
                .iter()
                .filter(|item| item.kind == MessageKind::Notice)
                .count(),
            1,
            "监督者只能看到一份 w1"
        );
    }

    /// 图被 `grow_plan` 追加时"该到几份"要抬得上去。定死在构造时的话，
    /// 补做的第二批永远等不到，而收件箱那边已经报过一次"齐了"
    #[test]
    fn a_grown_generation_raises_the_barrier_instead_of_lying_that_it_finished() {
        let ex = Exchange::new("boss", 1, 0);
        assert!(ex.report("w1", "第一批").1);
        ex.expect(2);
        assert!(!ex.settled(), "该到 2 份而只到了 1 份，不能说齐");
        assert!(ex.report("w2", "第二批").1);
        assert!(ex.arrived().contains(&"w2".to_string()));
    }

    /// 监督者决策过之后的迟到回报，投递数就是 0。这个 0 要落进账：
    /// 它说的是"有一份产出没人接"，而不是"送出去了"
    #[test]
    fn a_late_report_after_the_supervisor_decided_is_not_delivered() {
        let ex = Exchange::new("boss", 1, 0);
        ex.close();
        assert_eq!(ex.report("w1", "来晚了").0, 0, "关掉之后不该算成送到");
        assert!(ex.collect().is_empty(), "收件箱已经关了，不该还有东西可读");
        assert_eq!(
            ex.arrived(),
            vec!["w1".to_string()],
            "谁来过仍然要记得：缺的那一份靠它算"
        );
    }

    /// 每一层有自己的一代：层级模式下 boss 与 boss 的 boss 不共用一个收件箱
    #[test]
    fn each_supervisor_gets_its_own_generation_and_a_cancel_closes_them_all() {
        let all = Exchanges::new();
        // 驱动线程读的就是 report 返回的这两个数（送达几份、这一代齐了没有），没有第三个入口
        assert_eq!(all.report("boss-a", "w1", "给 a", 1), (1, true));
        assert_eq!(all.report("boss-b", "w2", "给 b", 1), (1, true));
        assert_eq!(report_bodies(&all.collect("boss-a")), vec!["w1：给 a"]);
        assert_eq!(
            report_bodies(&all.collect("boss-b")),
            vec!["w2：给 b"],
            "两份报的是不同的监督者，就不该出现在同一个收件箱里"
        );

        // 级联取消要的就是"全关"：之后任何一份在途的回报都不该再被当成答案
        all.close(None);
        assert_eq!(all.report("boss-a", "w3", "迟了", 9), (0, false));
        assert_eq!(all.report("boss-b", "w4", "迟了", 9), (0, false));
    }

    /// 原始要求给 MessageBus 列了五条：点对点、广播、请求-响应、流式、barrier。
    /// 数过生产代码的调用点，**五条里两条没人说**：广播与请求-响应有形状、有单测，
    /// 但没有任何一处生产代码构造 `Destination::Broadcast`，也没有一处调 `request` / `reply_for`。
    ///
    /// 这一条断言的是**今天还没有**，不是"永远不许有"：谁把它们接上，这里就红，
    /// 并把人指回 design-multi-agent.md §5.29——那里写清了当时为什么不接
    /// （广播会变成 hand-off 的第二份真相；请求-响应是每 worker 多一次往返＝钱，要点头）。
    /// **正向对照也钉着**：另外两条真的在跑，否则这条钉会退化成"什么都没测"的空断言
    #[test]
    fn two_of_the_five_bus_channels_still_have_no_speaker_and_says_so() {
        // 这个仓库的 .rs 整份是 CRLF：单行针脚也要先把行尾回车去掉再数
        let only_production = |source: &str| {
            source
                .replace('\r', "")
                .split("\n#[cfg(test)]")
                .next()
                .unwrap_or_default()
                .to_string()
        };
        let production = format!(
            "{}\n{}\n{}",
            only_production(include_str!("runtime.rs")),
            only_production(include_str!("orchestrator.rs")),
            only_production(include_str!("mod.rs")),
        );

        // 正向对照 1：点对点确实在跑——worker 的回报是发给它那位监督者的
        assert!(
            production.contains("Destination::Point(self.supervisor.clone())"),
            "点对点那条路不见了：现在谁在收 worker 的回报？"
        );
        // 正向对照 2：barrier 与投递在同一条路上
        assert!(
            production.contains("self.barrier.arrive()")
                && production.contains("self.bus.deliver("),
            "barrier 或投递不见了：监督者拿什么判断这一批齐了"
        );

        // 反向 1：广播只有"读的那两个分支"，没有发送方
        assert_eq!(
            production.matches("Destination::Broadcast").count(),
            2,
            "广播多出了一处：§5.29 那句'没人说'要跟着改，先想清楚它是不是第二份 hand-off"
        );
        // 反向 2：请求-响应那三个函数（发起、回应、判定"这条是在答我"）都只有定义那一份
        for name in ["request(", "reply_for", "answers("] {
            assert_eq!(
                production.matches(name).count(),
                1,
                "「{name}」多出了一处：先读 §5.29 再动——请求-响应是每 worker 多一次往返，方向朝上的那一格"
            );
        }
    }
}
