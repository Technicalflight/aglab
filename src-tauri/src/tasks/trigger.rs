//! 触发器：什么时候该跑，以及**停机期间错过的怎么算**。
//!
//! 错过策略原来是隐式的（`watch` 的注释自陈"不补跑错过的次数"，而实际行为是"停机再起来
//! 就补跑一次"）。隐式的选择没人做过决定，所以这里把它变成任务定义上写得出来的一项，
//! 默认档取原来那个行为——老任务不会因为这一版悄悄换语义。
//!
//! 策略写在 `ScheduledTask.kind` 的后缀上（`"interval|skip"`）而不是新加字段：
//! `ScheduledTask` 属于 config.rs，本模块不拥有它。裸写法 = 默认档，与老配置逐字节兼容。

use chrono::{Datelike, Local, TimeZone};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

use crate::config::ScheduledTask;

const DAY_MS: i64 = 86_400_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TriggerKind {
    /// 每隔 everyMinutes 分钟
    Interval,
    /// 每天本地时间 atMinute 分
    Daily,
    /// 每周 atWeekday 星期几的本地时间 atMinute 分
    Weekly,
    /// 按 cron_expr 表达式（标准 5 段：分 时 日 月 周，周日=0；也吃带秒的 6/7 段）。
    /// 出现时刻不均匀（跳过周末、闰年才有的 2/29），所以补账只能靠枚举，不能靠格距算术
    Cron,
}

/// 一轮最多补几格。`CatchUpOnce` 的字面意思是"错过几次补几次"，而那每一发都是一份完整上下文的钱：
/// 合盖一周再打开，一个每小时任务会连发 168 发。所以补账有预算，超出的一部分**作废并记一行**
/// （`runs::void_slots`）——静默少跑与静默多花是同一类问题，两种都不许没有痕迹
pub const CATCH_UP_BUDGET: usize = 8;

/// 停机期间错过的槽位怎么算。三档的差别只在"补几次"，正常运行时都是每格一次
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MissedPolicy {
    /// 一个都不补：错过的整批作废，等下一个未来的槽位
    Skip,
    /// 错过几次补几次，逐格把账追平
    CatchUpOnce,
    /// 只跑最近的那一次，其余作废——原来那个隐式行为，所以它是默认值
    RunLatest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trigger {
    pub kind: TriggerKind,
    pub missed: MissedPolicy,
}

impl MissedPolicy {
    /// 写在 `kind` 后缀上、也给界面照原样显示的名字：默认档同样拼得出来，
    /// 用户不必靠"留空"来表达"就要现在这个行为"
    pub fn as_str(self) -> &'static str {
        match self {
            MissedPolicy::Skip => "skip",
            MissedPolicy::CatchUpOnce => "catch_up_once",
            MissedPolicy::RunLatest => "run_latest",
        }
    }
}

/// 解析 `kind`。认不出的策略后缀退回默认档，与 `config::mode_from_legacy` 同一条规矩：
/// 手写配置打错一个字母，不该让任务变成另一种东西
pub fn of(task: &ScheduledTask) -> Option<Trigger> {
    let (head, suffix) = match task.kind.split_once('|') {
        Some((head, suffix)) => (head, Some(suffix)),
        None => (task.kind.as_str(), None),
    };
    let kind = match head {
        "interval" => TriggerKind::Interval,
        "daily" => TriggerKind::Daily,
        "weekly" => TriggerKind::Weekly,
        "cron" => TriggerKind::Cron,
        _ => return None,
    };
    let missed = match suffix.unwrap_or("") {
        "" | "run_latest" => MissedPolicy::RunLatest,
        "skip" => MissedPolicy::Skip,
        "catch_up_once" => MissedPolicy::CatchUpOnce,
        _ => MissedPolicy::RunLatest,
    };
    Some(Trigger { kind, missed })
}

/// 本地时区的当日 00:00。只借 chrono 取那一瞬间的 UTC 偏移，日期算术自己做
fn local_midnight(now: i64) -> i64 {
    let Some(local) = Local.timestamp_millis_opt(now).single() else {
        return now.div_euclid(DAY_MS) * DAY_MS;
    };
    let offset = local.offset().local_minus_utc() as i64 * 1000;
    (now + offset).div_euclid(DAY_MS) * DAY_MS - offset
}

/// 从锚点起已经到期（`<= now`）的那些格子，`limit` 用来在追账时早停
fn elapsed_slots(anchor: i64, every: i64, now: i64, limit: usize) -> Vec<i64> {
    let mut out = Vec::new();
    let mut slot = anchor + every;
    while slot <= now && out.len() < limit {
        out.push(slot);
        slot += every;
    }
    out
}


/// 从 now 的本地星期几到目标星期几的天数差（0=今天就是，1=明天…6=六天后）
fn days_until(now: i64, target_weekday: u32) -> u32 {
    let Some(local) = Local.timestamp_millis_opt(now).single() else {
        return 0;
    };
    let current = local.weekday().num_days_from_sunday() as u32;
    (target_weekday + 7 - current) % 7
}

/// 本周目标星期几的本地 at_minute 时刻（可能已过，可能在未来）
fn weekly_slot(now: i64, target_weekday: u32, at_minute: i64) -> i64 {
    let delta = days_until(now, target_weekday) as i64;
    local_midnight(now) + delta * DAY_MS + at_minute
}

/// cron 表达式 → 日程。空串与解析失败都算没有日程：写入口会把坏表达式拒在门外
/// （`check_task_graphs` 同一道），手写 config.json 绕过校验的就按"不会跑"放着，
/// 不崩溃也不补跑——界面在"下次"那一格如实显示没有下一次
pub(crate) fn cron_schedule(expr: &str) -> Option<cron::Schedule> {
    let expr = expr.trim();
    if expr.is_empty() {
        return None;
    }
    let mut fields: Vec<String> = expr.split_whitespace().map(str::to_string).collect();
    // 这只解析器把**秒**当第一域（6/7 段才收）。人写的是标准 5 段（分 时 日 月 周）：
    // 补一个 0 秒再交进去；6/7 段按"本来就带秒"的原样过
    if fields.len() == 5 {
        fields.insert(0, "0".into());
    }
    // 周几这一域从人的口径（0=周日…6=周六，7 也算周日）换到解析器的口径
    // （1=周日…7=周六）。名字（MON…）原样过——解析器的名字表本来就是 1=周日，
    // 写名字的人点的那几天不会错位；换的不只是数字，还有"1-5 到底是哪几天"
    if let Some(dow) = fields.get_mut(5) {
        *dow = remap_weekday_field(dow);
    }
    cron::Schedule::from_str(&fields.join(" ")).ok()
}

/// 周几域里的一个数字 token：0 与 7 都归周日（1），其余平移一位。
/// 名字与越界的原样过——后者回头由解析器自己报错
fn remap_weekday_token(token: &str) -> String {
    match token.parse::<u32>() {
        Ok(0 | 7) => "1".into(),
        Ok(n) if (1..=6).contains(&n) => (n + 1).to_string(),
        _ => token.to_string(),
    }
}

/// 周几这一域的整体换算：逗号分列表、连字符分区间、斜杠后是步进，
/// 只换数字服务商，`*` 与名字原样保留
fn remap_weekday_field(field: &str) -> String {
    field
        .split(',')
        .map(|part| match part.split_once('-') {
            None => remap_weekday_token(part),
            Some((lo, rest)) => match rest.split_once('/') {
                None => format!("{}-{}", remap_weekday_token(lo), remap_weekday_token(rest)),
                Some((hi, step)) => {
                    format!("{}-{}/{}", remap_weekday_token(lo), remap_weekday_token(hi), step)
                }
            },
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn local_dt(millis: i64) -> Option<chrono::DateTime<Local>> {
    Local.timestamp_millis_opt(millis).single()
}

/// catch_up 一次最多往前枚举多少个出现时刻。表达式可以密到每分钟一次，
/// 不设上限的话停机一年就是 52 万次枚举；扫满之后的事交给 `runs::void_slots` 记账作废
const CRON_SCAN_CAP: usize = 10_000;

/// (anchor, now] 窗口内的出现时刻，按时间次序。严格大于 anchor：anchor 是上一轮
/// 开跑的时刻，而 cron 的出现时刻恰与开跑时刻对齐——含进去就是刚跑完立刻重跑
fn cron_slots(schedule: &cron::Schedule, anchor: i64, now: i64, limit: usize) -> Vec<i64> {
    let Some(start) = local_dt(anchor) else {
        return Vec::new();
    };
    schedule
        .after(&start)
        .map(|dt| dt.timestamp_millis())
        .filter(|slot| *slot > anchor)
        .take_while(|slot| *slot <= now)
        .take(limit)
        .collect()
}

/// 本轮该跑的格子，按该跑的次序。调度线程把这里的每一格都跑一次
pub fn due_slots(task: &ScheduledTask, last_started_at: i64, now: i64) -> Vec<i64> {
    if !task.enabled {
        return Vec::new();
    }
    let Some(trigger) = of(task) else {
        return Vec::new();
    };
    let anchor = if last_started_at > 0 {
        last_started_at
    } else {
        task.created_at
    };
    match trigger.kind {
        TriggerKind::Interval => {
            let every = task.every_minutes.max(1) as i64 * 60_000;
            match trigger.missed {
                // 追账不设上限：这是用户在任务定义上明写要的"错过几次补几次"
                MissedPolicy::CatchUpOnce => elapsed_slots(anchor, every, now, usize::MAX),
                // skip 的作废就落在这里：过去的格子一概不追，未来的那格由 next_run 给出
                MissedPolicy::Skip => Vec::new(),
                MissedPolicy::RunLatest => elapsed_slots(anchor, every, now, 1),
            }
        }
        TriggerKind::Daily => {
            let at = task.at_minute.min(24 * 60 - 1) as i64 * 60_000;
            let today = local_midnight(now) + at;
            match trigger.missed {
                // skip 只追未来不追过去，而"未来的格子"不叫到期：本轮什么都不跑，
                // 下一次由 next_run 给（今天已过点就等明天）
                MissedPolicy::Skip => Vec::new(),
                // 只认今天这一格：昨天的不追，明天的不提前
                MissedPolicy::RunLatest => {
                    if today <= now && today > anchor {
                        vec![today]
                    } else {
                        Vec::new()
                    }
                }
                MissedPolicy::CatchUpOnce => {
                    let back = (today - anchor).div_euclid(DAY_MS).clamp(0, 365);
                    (0..=back)
                        .map(|ago| today - ago * DAY_MS)
                        .filter(|slot| *slot <= now && *slot > anchor)
                        .rev()
                        .collect()
                }
            }
        }
        TriggerKind::Weekly => {
            let at = task.at_minute.min(24 * 60 - 1) as i64 * 60_000;
            let weekday = task.at_weekday.min(6);
            let this_week = weekly_slot(now, weekday, at);
            let week_ms = 7 * DAY_MS;
            match trigger.missed {
                MissedPolicy::Skip => Vec::new(),
                MissedPolicy::RunLatest => {
                    if this_week <= now && this_week > anchor {
                        vec![this_week]
                    } else {
                        Vec::new()
                    }
                }
                MissedPolicy::CatchUpOnce => {
                    let back = ((this_week - anchor).div_euclid(week_ms)).clamp(0, 52);
                    (0..=back)
                        .map(|ago| this_week - ago * week_ms)
                        .filter(|slot| *slot <= now && *slot > anchor)
                        .rev()
                        .collect()
                }
            }
        }
        TriggerKind::Cron => {
            let Some(schedule) = cron_schedule(&task.cron_expr) else {
                return Vec::new();
            };
            match trigger.missed {
                MissedPolicy::Skip => Vec::new(),
                // 只补最后一格：枚举满窗口取末尾。枚举被扫满时拿到的不是真正的最后
                // 一格，但那已经是病理表达式 + 超长停机，跑总比不跑诚实
                MissedPolicy::RunLatest => cron_slots(&schedule, anchor, now, CRON_SCAN_CAP)
                    .pop()
                    .into_iter()
                    .collect(),
                MissedPolicy::CatchUpOnce => cron_slots(&schedule, anchor, now, CRON_SCAN_CAP),
            }
        }
    }
}

/// 下一次该触发的时间戳。停用、或频率配置非法时返回 None。
/// 到期时给的就是本轮那一格（可能是过去的时间），所以 `watch` 判 `<= now` 即跑
pub fn next_run(task: &ScheduledTask, last_started_at: i64, now: i64) -> Option<i64> {
    if !task.enabled {
        return None;
    }
    let trigger = of(task)?;
    if let Some(due) = due_slots(task, last_started_at, now).first().copied() {
        return Some(due);
    }
    let anchor = if last_started_at > 0 {
        last_started_at
    } else {
        task.created_at
    };
    match trigger.kind {
        TriggerKind::Interval => {
            let every = task.every_minutes.max(1) as i64 * 60_000;
            match trigger.missed {
                // skip 的下次必须钉在创建时刻那张网格上：它不跟着补跑漂移，
                // 否则"不补"只是把日程往后推了一点
                MissedPolicy::Skip => {
                    let grid = if task.created_at > 0 { task.created_at } else { anchor };
                    let steps = (now - grid).div_euclid(every).max(0) + 1;
                    Some(grid + steps * every)
                }
                MissedPolicy::CatchUpOnce | MissedPolicy::RunLatest => Some(anchor + every),
            }
        }
        TriggerKind::Daily => {
            let at = task.at_minute.min(24 * 60 - 1) as i64 * 60_000;
            let today = local_midnight(now) + at;
            if today <= now {
                Some(today + DAY_MS)
            } else {
                Some(today)
            }
        }
        TriggerKind::Weekly => {
            let at = task.at_minute.min(24 * 60 - 1) as i64 * 60_000;
            let weekday = task.at_weekday.min(6);
            let this_week = weekly_slot(now, weekday, at);
            if this_week <= now {
                Some(this_week + 7 * DAY_MS)
            } else {
                Some(this_week)
            }
        }
        TriggerKind::Cron => {
            let schedule = cron_schedule(&task.cron_expr)?;
            let start = local_dt(now)?;
            // 严格找未来：恰好压在 now 上的那一格归 due_slots 管（到期判 <= now），
            // 这里再收进来就是同一格跑两次
            schedule
                .after(&start)
                .map(|dt| dt.timestamp_millis())
                .find(|slot| *slot > now)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(kind: &str, every: u32, at: u32, enabled: bool, created_at: i64) -> ScheduledTask {
        ScheduledTask {
            at_weekday: 1,
            id: "t1".into(),
            name: "演练".into(),
            prompt: "说一句话".into(),
            kind: kind.into(),
            every_minutes: every,
            at_minute: at,
            cron_expr: String::new(),
            enabled,
            created_at,
            graph: Default::default(),
            webhook_url: String::new(),
            webhook_token: String::new(),
        }
    }

    const HOUR_MS: i64 = 3_600_000;

    fn wall() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_millis() as i64)
            .unwrap_or(0)
    }

    /// 三档在同一个 3 小时停机上各给出自己的答案——这条就是"错过策略是显式选择"的证据
    #[test]
    fn a_three_hour_outage_produces_its_declared_number_of_runs() {
        let last = 1_000_000;
        // 30 分钟一次，停机 3 小时零 1 毫秒：第 6 格刚过期，积下的就是 6 次
        let now = last + 3 * HOUR_MS + 1;

        assert_eq!(
            due_slots(&task("interval|skip", 30, 0, true, 0), last, now).len(),
            0,
            "skip 的意义就是不补：停三小时一次都不该跑"
        );
        assert_eq!(
            due_slots(&task("interval|run_latest", 30, 0, true, 0), last, now).len(),
            1,
            "run_latest 把六格积压塌缩成一次补跑"
        );
        assert_eq!(
            due_slots(&task("interval|catch_up_once", 30, 0, true, 0), last, now).len(),
            6,
            "catch_up_once 的意思是错过几次补几次，六格就是六次"
        );
    }

    #[test]
    fn the_default_policy_is_the_one_the_scheduler_used_to_have() {
        // 老配置里没有 `|` 后缀。它必须继续跑今天这个形状，否则这一版会悄悄改语义
        let bare = task("interval", 30, 0, true, 0);
        assert_eq!(
            of(&bare).expect("该认得 interval").missed,
            MissedPolicy::RunLatest,
            "没写策略的任务必须落在原来那一档"
        );
        let last = 1_000_000;
        let now = last + 3 * HOUR_MS;
        assert_eq!(
            due_slots(&bare, last, now),
            due_slots(&task("interval|run_latest", 30, 0, true, 0), last, now),
            "裸写法与显式 run_latest 必须一格不差，包括补哪一格"
        );
        assert_eq!(next_run(&bare, last, now), Some(last + 30 * 60_000), "到期就是立刻可跑");
    }

    #[test]
    fn an_unreadable_policy_suffix_falls_back_to_the_default_instead_of_silently_changing_task() {
        // 手写 config.json 打错字是常态：它不该把任务变成另一种错过策略，更不该让它不跑
        let typo = task("interval|skp", 30, 0, true, 0);
        assert_eq!(of(&typo).expect("频率还在").missed, MissedPolicy::RunLatest);
        assert_eq!(
            due_slots(&typo, 1_000_000, 1_000_000 + 3 * HOUR_MS),
            due_slots(&task("interval", 30, 0, true, 0), 1_000_000, 1_000_000 + 3 * HOUR_MS),
            "打错字的结果必须是默认档，不是第三种行为"
        );
    }

    #[test]
    fn skip_realigns_to_the_original_grid_instead_of_crawling_forward_with_each_run() {
        // run_latest 每跑一次就把锚点搬到"这次真正跑的时刻"，网格跟着漂移；skip 停在原网格上。
        // 这条区分的是两档，不是同一个行为的两种拼写
        let created = 1_000_000;
        let now = created + 3 * HOUR_MS + 1;
        let skip = task("interval|skip", 30, 0, true, created);
        let latest = task("interval|run_latest", 30, 0, true, created);

        let skip_next = next_run(&skip, created, now).expect("skip 也该给个下次");
        assert!(skip_next > now, "skip 的下次必须在未来：{skip_next} vs {now}");
        assert_eq!(
            (skip_next - created) % (30 * 60_000),
            0,
            "skip 要停在创建时刻那张网格上，不该被补跑推着走"
        );
        assert_eq!(
            next_run(&latest, created, now),
            Some(created + 30 * 60_000),
            "run_latest 的下次就是到期的那一格"
        );
    }

    #[test]
    fn daily_policies_disagree_only_about_the_missed_days() {
        let midnight = local_midnight(1_700_000_000_000);
        let nine = midnight + 9 * 3_600_000;
        // 前天 08:00 跑过一次，之后三天没开应用，今天已经过了点
        let last = nine - 2 * DAY_MS - 3_600_000;
        let now = nine + 60_000;

        assert!(
            due_slots(&task("daily|skip", 0, 9 * 60, true, 0), last, now).is_empty(),
            "skip 连今天这一格都作废"
        );
        assert_eq!(due_slots(&task("daily|run_latest", 0, 9 * 60, true, 0), last, now), vec![nine],
            "run_latest 只补今天");
        assert_eq!(
            due_slots(&task("daily|catch_up_once", 0, 9 * 60, true, 0), last, now),
            vec![nine - 2 * DAY_MS, nine - DAY_MS, nine],
            "catch_up_once 要按该跑的次序把落下的三天都追回来"
        );
    }

    #[test]
    fn interval_anchors_on_creation_then_on_the_last_run() {
        let every = task("interval", 30, 0, true, 1_000);
        assert_eq!(next_run(&every, 0, 2_000), Some(1_000 + 30 * 60_000));
        assert_eq!(next_run(&every, 5_000, 6_000), Some(5_000 + 30 * 60_000));

        // 应用关了一天：到期只补跑一次，不会积压成几十次
        assert!(next_run(&every, 5_000, 6_000 + DAY_MS).is_some());
    }

    #[test]
    fn daily_rolls_to_tomorrow_after_its_time() {
        let midnight = local_midnight(wall());
        let nine = task("daily", 0, 9 * 60, true, 0);

        let before = midnight + 8 * 3_600_000;
        assert_eq!(next_run(&nine, 0, before), Some(midnight + 9 * 3_600_000));

        // 过了点还没跑过（比如那时应用没开）→ 立刻算到期
        let after = midnight + 9 * 3_600_000 + 60_000;
        assert_eq!(next_run(&nine, 0, after), Some(midnight + 9 * 3_600_000));

        assert_eq!(
            next_run(&nine, midnight + 9 * 3_600_000 + 1_000, after),
            Some(midnight + 9 * 3_600_000 + DAY_MS)
        );
    }

    #[test]
    fn local_midnight_is_stable_within_a_day() {
        let now = wall();
        let midnight = local_midnight(now);
        assert!(midnight <= now);
        assert!(now - midnight < DAY_MS);
        // 刚过零点的一分钟，算出来的应该还是同一个零点
        assert_eq!(local_midnight(midnight + 60_000), midnight);
    }

    #[test]
    fn disabled_and_unknown_kinds_have_no_next_run() {
        assert_eq!(next_run(&task("interval", 5, 0, false, 0), 0, 100), None);
        assert_eq!(next_run(&task("biweekly", 5, 0, true, 0), 0, 100), None);
        assert!(due_slots(&task("biweekly", 5, 0, true, 0), 0, 100).is_empty(),
            "认不出的频率连到期都不该有");
    }

    fn cron_task(expr: &str, missed: &str, created_at: i64) -> ScheduledTask {
        let mut t = task(&format!("cron|{missed}"), 0, 0, true, created_at);
        t.cron_expr = expr.into();
        t
    }

    /// cron 的出现时刻与开跑时刻恰好对齐：窗口必须是开区间，含进 anchor 就是
    /// 刚跑完立刻重跑。到期判 <= now、下次找 > now，同一格只能归一头
    #[test]
    fn cron_fires_at_its_expression_times_and_never_refires_the_anchor() {
        let midnight = local_midnight(wall());
        let nine = midnight + 9 * 3_600_000;
        // 今天零点建的任务，每天 09:00
        let t = cron_task("0 9 * * *", "run_latest", midnight);

        // 还没到点：下次就是今天 09:00
        assert_eq!(next_run(&t, 0, midnight + 8 * HOUR_MS), Some(nine));
        // 过了点没跑过：到期给的就是那一格（可能是过去的时间），watch 判 <= now 即跑
        assert_eq!(next_run(&t, 0, nine + 60_000), Some(nine));
        // 刚跑完这格：窗口 (anchor, now] 里必须空——含进去就是无限重跑
        assert!(due_slots(&t, nine + 1_000, nine + 60_000).is_empty());
        assert_eq!(next_run(&t, nine + 1_000, nine + 60_000), Some(nine + DAY_MS));
    }

    /// 出现时刻不均匀（周末没有格子），补账不能像 interval 那样拿格距乘出来，
    /// 只能逐格枚举——三档的差别仍然只在"补几次"
    #[test]
    fn cron_policies_disagree_only_about_the_missed_occurrences() {
        let midnight = local_midnight(wall());
        let nine = midnight + 9 * 3_600_000;
        // 三天前的零点建的任务，之后没跑过
        let created = midnight - 3 * DAY_MS;
        let now = nine + HOUR_MS;
        // 工作日表达式：周末没有格子，所以窗口里的格数取决于这几天夹着几个周末，
        // 但两档读的是同一串格子
        let latest = due_slots(&cron_task("0 9 * * *", "run_latest", created), 0, now);
        let catchup = due_slots(&cron_task("0 9 * * *", "catch_up_once", created), 0, now);
        assert_eq!(latest, vec![nine], "run_latest 塌缩成最近一次");
        assert_eq!(catchup.len(), 4, "三天三个整天 + 今天：一天一格");
        assert_eq!(*catchup.last().expect("至少今天这一格"), nine, "按该跑的次序，今天在末尾");
        assert!(due_slots(&cron_task("0 9 * * *", "skip", created), 0, now).is_empty());
    }

    #[test]
    fn an_unreadable_cron_expression_never_fires() {
        let created = wall() - DAY_MS;
        for expr in ["", "   ", "每周九点", "0 9 * *", "99 99 * * *"] {
            let t = cron_task(expr, "run_latest", created);
            assert!(due_slots(&t, 0, wall()).is_empty(), "「{expr}」不该有到期的格子");
            assert_eq!(next_run(&t, 0, wall()), None, "「{expr}」不该有下一次");
        }
    }

    /// 星期那一域真的管用：一周的窗口里只落在选中的星期，且都在声明的那个时刻
    #[test]
    fn cron_weekday_fields_only_land_on_those_days() {
        let midnight = local_midnight(wall());
        let t = cron_task("30 9 * * 1-5", "catch_up_once", midnight);
        let slots = due_slots(&t, 0, midnight + 7 * DAY_MS);
        assert!((4..=5).contains(&slots.len()), "一周只有四到五个工作日：{slots:?}");
        for slot in slots {
            let local = Local.timestamp_millis_opt(slot).single().expect("本地时刻");
            assert!(
                local.weekday().num_days_from_monday() < 5,
                "周一到周五之外不该有格子：{local}"
            );
            assert_eq!(slot - local_midnight(slot), (9 * 60 + 30) * 60_000, "落在 09:30");
        }
    }
}
