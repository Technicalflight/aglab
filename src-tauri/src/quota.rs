//! 这台机器上的并发额度。整进程一份，由 `lib.rs` 建好并 `manage` 进状态，
//! 编排器与定时任务两边共用。
//!
//! 它从 `orchestra::runtime` 搬出来是因为一条面板上的话："全局 5/6"。那句话说的从来是
//! **这台机器上同时几路**，而不只是"编排器里同时几路"——只要还有一条会花的钱的出口
//! （定时任务那一发完整上下文）不经过这个池子，那个数就是半真话。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

/// 要一格位子的那一方有多急。它调的是"接下来那一手派给谁"，永远不动已经在跑的那一发
/// ——与预算闸、暂停同一条规矩：掐掉正在跑的请求在服务商侧照样计费，还会留下一段半截话题
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum Priority {
    /// 愿意让路：这一档**加起来**最多占全局池的一半
    Background,
    /// 默认那一档
    #[default]
    Normal,
    /// 抢在前面：可以用满全局池（但仍抢不走已经在跑的那一手）
    Foreground,
}

/// 拿不到位的原因。两种 denial 是两件不同的事，界面上也得分开说：
/// `Full` 是"这一刻别人在跑"（会自己过去），`AtShare` 是"这一档最多占这么多"（不让就永远不过去）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denied {
    Full,
    AtShare,
}

/// 全局并发额度。每份计划自己的并发位管不住总量：开三份 `max_parallel=4` 就是 16 路并发请求，
/// 每一路都是一份完整上下文的钱——而"并发不超过配置上限"那条验收说的是**这台机器上**不超过，
/// 不是"每个来源各不超过"。
///
/// 份额按档位**全局累计**（不是每个来源一份）：所有后台加起来最多占一半，所以
/// "后台把池子占满、前台挤不进来"这件事在构造上就不成立。同时低档能借到空位（借的就是
/// 那一半/四分之三的上限），所以也不存在"高档没在跑而额度白扔"。
///
/// 总池写 0 = **不设上限**：总池与份额两道闸都不再看，占一手永远成功——
/// 这是设置里能明确写出来的决定，不是缺省值；缺省照旧是有限池
pub struct Quota {
    total: AtomicUsize,
    /// 每个档位此刻占着几手，索引 = 档位的序（后台 / 常规 / 前台）
    held: Mutex<[usize; 3]>,
}

impl Default for Quota {
    fn default() -> Self {
        Self::new(1)
    }
}

impl Quota {
    /// 0 = 不设上限；非 0 照旧按有限池算
    pub fn new(total: usize) -> Self {
        Self { total: AtomicUsize::new(total), held: Mutex::new([0; 3]) }
    }

    fn index(priority: Priority) -> usize {
        priority as usize
    }

    pub fn total(&self) -> usize {
        self.total.load(Ordering::Acquire)
    }

    /// 0 = 不设上限。两道闸（总池、份额）都不再看，占一手永远成功
    pub fn unlimited(&self) -> bool {
        self.total() == 0
    }

    /// 改上限**不掐已经在跑的那几手**：缩下去只是新的派发进不来（与预算闸同一条规矩）。
    /// 写 0 = 从此刻起不设上限
    pub fn set_total(&self, total: usize) {
        self.total.store(total, Ordering::Release);
    }

    /// 这一档至多占几手。下限是 1：不是 1 的话低档就永远跑不动，那是饿死不是让路。
    /// 上限不会越过此刻的总池（总池可能刚被调小）。
    /// 不限池没有份额这回事，返回 usize::MAX 只是让"永远够"有个形状
    pub fn share(&self, priority: Priority) -> usize {
        let total = self.total();
        if total == 0 {
            return usize::MAX;
        }
        let share = match priority {
            Priority::Background => total / 2,
            Priority::Normal => total - total / 4,
            Priority::Foreground => total,
        };
        share.max(1).min(total)
    }

    /// 在跑多少只被测试读（生产侧"还有几个活的"看的是运行时自己的账，不是配额表）。
    /// 归还语义本身靠 Drop，这条 getter 就是给测试断言用的
    #[cfg(test)]
    pub fn held(&self, priority: Priority) -> usize {
        let held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        held[Self::index(priority)]
    }

    /// 所有档位加起来占了几手——面板要说"全局 5/8"就得问这个
    pub fn held_total(&self) -> usize {
        let held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        held.iter().sum()
    }

    /// 占一手。拿不到就返回原因，调用方落账——**顶住派发这件事必须看得见**，
    /// 静默等下一轮就是"看上去在跑而其实没动"
    pub fn try_acquire(self: &Arc<Self>, priority: Priority) -> Result<Slot, Denied> {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        let index = Self::index(priority);
        if !self.unlimited() {
            if held.iter().sum::<usize>() >= self.total() {
                return Err(Denied::Full);
            }
            if held[index] >= self.share(priority) {
                return Err(Denied::AtShare);
            }
        }
        held[index] += 1;
        drop(held);
        Ok(Slot { quota: self.clone(), priority })
    }
}

/// 一格全局位。随 `Drop` 归还——所以"忘了归还"在这里不可能发生，而不是靠谁记得调 release
pub struct Slot {
    quota: Arc<Quota>,
    priority: Priority,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut held = self.quota.held.lock().unwrap_or_else(PoisonError::into_inner);
        let index = Quota::index(self.priority);
        held[index] = held[index].saturating_sub(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 全局池顶住的是**接下来的那一手**，不是已经在跑的
    #[test]
    fn the_global_pool_stops_the_fourth_dispatch_and_not_the_three_running() {
        let quota = Arc::new(Quota::new(3));
        assert_eq!(quota.share(Priority::Foreground), 3);
        let a = quota.try_acquire(Priority::Foreground).expect("第 1 手");
        let b = quota.try_acquire(Priority::Normal).expect("第 2 手");
        let c = quota.try_acquire(Priority::Background).expect("第 3 手");
        assert_eq!(quota.held_total(), 3, "面板上那句 3/8 问的是所有档加起来");
        assert_eq!(quota.try_acquire(Priority::Foreground).err(), Some(Denied::Full), "第 4 手该被挡下");
        drop(b);
        assert_eq!(quota.held(Priority::Normal), 0, "归还靠 Drop，不靠谁记得调 release");
        assert!(quota.try_acquire(Priority::Foreground).is_ok(), "空出来的那一手立刻能再派");
        drop((a, c));
    }

    /// "后台把池子占满、前台挤不进来"必须在构造上就不成立——所以份额是按档全局累计的，
    /// 不是每个来源各发一份
    #[test]
    fn background_tiers_cannot_fill_the_pool_even_when_they_act_together() {
        let quota = Arc::new(Quota::new(4));
        assert_eq!(quota.share(Priority::Background), 2);
        assert_eq!(quota.share(Priority::Normal), 3);
        assert_eq!(quota.share(Priority::Foreground), 4);
        let one = quota.try_acquire(Priority::Background).expect("后台第 1 手");
        let two = quota.try_acquire(Priority::Background).expect("后台第 2 手");
        assert_eq!(
            quota.try_acquire(Priority::Background).err(),
            Some(Denied::AtShare),
            "第三手后台是被**份额**挡的，不是被池子：这两件事在界面上必须分开说"
        );
        // 池子里还剩两格，而它们是属于前台的
        let three = quota.try_acquire(Priority::Foreground).expect("前台拿得到后台借不走的那一格");
        let four = quota.try_acquire(Priority::Foreground).expect("另一格同理");
        assert_eq!(quota.held_total(), 4);
        drop((one, two, three, four));
    }

    /// 低档不是"等高档跑完才轮到"：没有别人在跑时它能借满自己那半。
    /// 而单格池子上份额下限必须是 1，否则后台永远排不进去——那是饿死，不是让路
    #[test]
    fn a_lower_tier_borrows_the_idle_half_and_is_never_starved() {
        let idle = Arc::new(Quota::new(4));
        let held: Vec<_> = (0..2)
            .map(|_| idle.try_acquire(Priority::Background).expect("没人在跑时后台能占到一半"))
            .collect();
        assert_eq!(idle.held(Priority::Background), 2);
        drop(held);

        let single = Arc::new(Quota::new(1));
        assert_eq!(single.share(Priority::Background), 1, "一半向上取到 1，不是取到 0");
        // 绑住它：不绑的话这一手在下一行之前就随临时值还回去了，测出来的是"没人占着"
        let only = single.try_acquire(Priority::Background).expect("单格池子上后台也该跑得动");
        assert_eq!(
            single.try_acquire(Priority::Foreground).err(),
            Some(Denied::Full),
            "只有一格时先到先得，前台也不该挤掉已经发出去的那一手"
        );
        drop(only);
        assert!(single.try_acquire(Priority::Foreground).is_ok(), "还回去之后立刻有人拿得到");
    }

    /// 把上限调小**不收回已经发出去的手**：那几发在服务商侧已经计过费了
    #[test]
    fn shrinking_the_ceiling_stops_dispatch_rather_than_recalling_a_running_slot() {
        let quota = Arc::new(Quota::new(4));
        let held: Vec<_> = (0..4)
            .map(|_| quota.try_acquire(Priority::Foreground).expect("先按 4 手占满"))
            .collect();
        quota.set_total(1);
        assert_eq!(quota.held_total(), 4, "调小上限不该把在跑的那四手抹掉");
        assert_eq!(quota.try_acquire(Priority::Background).err(), Some(Denied::Full));
        drop(held);
        assert_eq!(quota.held_total(), 0);
        assert!(quota.try_acquire(Priority::Foreground).is_ok(), "退到 1 之后仍然发得出一手");
    }

    /// 面板那句"全局"要连定时任务一起算：两边占的是同一个池子，才只有一个数
    #[test]
    fn one_pool_serves_both_the_plans_and_the_scheduled_tasks() {
        let quota = Arc::new(Quota::new(2));
        let plan = quota.try_acquire(Priority::Normal).expect("编排器那一手");
        let task = quota.try_acquire(Priority::Background).expect("定时任务那一手");
        assert_eq!(quota.held_total(), 2, "两个来源各占一手，池子上就该是 2");
        assert_eq!(
            quota.try_acquire(Priority::Background).err(),
            Some(Denied::Full),
            "满了之后不管来的是谁都一样满"
        );
        drop((plan, task));
    }

    /// 0 = 不设上限：总池与份额两道闸都不再看，谁的档位都随便进；
    /// 任何时刻写回有限数，闸门照旧回来。`new(0)` 与 `set_total(0)` 是同一件事
    #[test]
    fn a_zero_pool_has_no_ceiling_and_the_gate_comes_back_when_a_number_is_written() {
        let quota = Arc::new(Quota::new(0));
        assert!(quota.unlimited(), "new(0) 就是不限");
        let held: Vec<_> = (0..10)
            .map(|_| quota.try_acquire(Priority::Background).expect("不限池里后台随便拿"))
            .collect();
        assert_eq!(quota.held_total(), 10, "第十手也没被总池或份额挡住");
        drop(held);

        quota.set_total(0);
        assert!(quota.unlimited(), "set_total(0) 与 new(0) 同一语义");
        let wide: Vec<_> = (0..3)
            .map(|_| quota.try_acquire(Priority::Background).expect("改成 0 之后照旧不限"))
            .collect();
        assert_eq!(quota.held_total(), 3);

        // 写回有限数，闸门照旧：已经在跑的不收回（3 手 > 新上限 2），超出的那一手被挡
        quota.set_total(2);
        assert!(!quota.unlimited());
        assert_eq!(
            quota.try_acquire(Priority::Background).err(),
            Some(Denied::Full),
            "写回 2 之后下一手该被挡下"
        );
        drop(wide);
    }
}
