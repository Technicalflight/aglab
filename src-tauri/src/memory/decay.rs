//! 遗忘曲线：双时间新鲜度与使用增益。**只算读侧的分数，一个字节都不回写真相源。**
//!
//! 为什么不看 `updated_at`：改个错别字就会把它推到今天，于是一条三年前的旧偏好突然
//! "复活"成最相关的记忆——那是把"我动了动这条记录"当成了"这件事刚发生"。
//! 三个时间是三件事：事情什么时候发生（`occurred_at`）、记录什么时候写下
//! （`created_at`）、上次什么时候真的被用上（`reinforced_at`）。新鲜度只认后两者
//! 的最大值，时间线才按 `occurred_at` 排。
//!
//! 衰减也绝不折进 `importance`：那是用户写下的事实，系统偷偷改它等于篡改。
//! 衰减只影响两件事——分数，以及（P1 之后）是否降级为候选，后者走 `set_status` 带审计。

use chrono::{DateTime, Local};

/// 半衰期。60 天不用，新鲜度对折；趋近 0 但不会归零——那该是蒸馏和手动删除的活
pub const HALF_LIFE_DAYS: f64 = 60.0;

/// 使用增益的饱和点：用到 10 次就拿满这一项。与既有 `weights.usage` 的历史口径一致，
/// 改这个数等于改所有老记忆的相对排序
const USAGE_SATURATION: f64 = 10.0;

/// 认两种写法：完整 RFC3339，以及手编辑时常见的只有年月日（按当天零点算）
pub fn parse_stamp(stamp: &str) -> Option<DateTime<Local>> {
    let text = stamp.trim();
    if let Ok(value) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(value.with_timezone(&chrono::Local));
    }
    chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .ok()?
        .and_hms_opt(0, 0, 0)?
        .and_local_timezone(chrono::Local)
        .single()
}

/// 距今多少天。认不出的时间戳返回 None——拿"读不懂"当"已过期"会误伤记忆
pub fn age_days(stamp: &str) -> Option<i64> {
    let then = parse_stamp(stamp)?;
    Some((Local::now() - then).num_days())
}

/// 新鲜度的基准时间。编辑不参与：它只证明有人动过笔，不证明这件事是新的
pub fn freshest_at(created_at: &str, reinforced_at: Option<&str>) -> Option<DateTime<Local>> {
    let created = parse_stamp(created_at);
    let reinforced = reinforced_at.and_then(parse_stamp);
    match (created, reinforced) {
        (Some(a), Some(b)) => Some(if a >= b { a } else { b }),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// 0.0..=1.0。两个时间戳都认不出来才给 0——那时候没有任何时间证据。
/// 半衰期由调用方给（配置里那一格 `decay_half_life_days`），这里不读配置：
/// 一次检索的分数要能由那一份 `SearchOptions` 复算出来，`why` 才不会是另一套数字
pub fn freshness(created_at: &str, reinforced_at: Option<&str>, half_life_days: f64) -> f64 {
    match freshest_at(created_at, reinforced_at) {
        Some(then) => {
            0.5f64.powf((Local::now() - then).num_days().max(0) as f64 / half_life_days.max(1.0))
        }
        None => 0.0,
    }
}

/// 使用频率这一路：对数饱和。前几次用得最值钱，第 11 次不比第 10 次更响
pub fn usage_gain(injections: i64) -> f64 {
    if injections <= 0 {
        return 0.0;
    }
    let top = (1.0 + USAGE_SATURATION).ln();
    (((1.0 + injections as f64).ln()) / top).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一条三年前写下、今天刚被编辑过的记录。这是"编辑复活记忆"的标准现场
    const ANCIENT: &str = "2023-06-01T10:00:00+08:00";

    /// 按默认半衰期问一次新鲜度。测试里不写死那个数：改默认值时这几条该跟着动，
    /// 而"半衰期真的可以配"由 [`the_half_life_actually_bends_the_curve`] 单独钉
    fn fresh(created_at: &str, reinforced_at: Option<&str>) -> f64 {
        freshness(created_at, reinforced_at, HALF_LIFE_DAYS)
    }

    /// 半衰期不是装饰：同一条 60 天没被用上的记忆，按 60 天折一半，按 600 天折几乎没折。
    /// 反面对照是同一条曲线上的另一个点，所以这条测试不会因今天与写下的那天差一天而飘
    #[test]
    fn the_half_life_actually_bends_the_curve() {
        let sixty_days_ago = (Local::now() - chrono::Duration::days(60)).to_rfc3339();
        let steep = freshness(&sixty_days_ago, None, 60.0);
        let flat = freshness(&sixty_days_ago, None, 600.0);
        assert!(
            (steep - 0.5).abs() < 0.01,
            "半衰期 60 天时 60 天该对折：{steep}"
        );
        assert!(flat > 0.9, "半衰期 600 天时同一发几乎还是新的：{flat}");
        assert!(flat > steep, "配得长就该比配得短的新");
        // 一份坏配置（0 或负数）不该把分数变成 NaN：读侧退到最保守的那一种
        assert!(freshness(&sixty_days_ago, None, 0.0).is_finite());
        assert!(
            freshness(&sixty_days_ago, None, -5.0) < 1.0,
            "负半衰期不能把旧记忆判成新的"
        );
    }

    #[test]
    fn an_old_record_is_stale_whatever_the_edit_stamp_says() {
        assert!(fresh(ANCIENT, None) < 0.001, "三年前的记录该几乎没有新鲜度");
        assert!(
            fresh(ANCIENT, Some(&crate::memory::record::now_rfc3339())) > 0.9,
            "被用上过一次就该重新变新鲜：这才是强化，而不是编辑"
        );
    }

    #[test]
    fn a_younger_reinforcement_never_pulls_freshness_back() {
        // 强化章只往前走：拿一个比 created_at 更旧的 reinforced_at 去"更新"它，
        // 等于让一次注入反而把记忆判旧
        let old_reinforce = "2020-01-01T00:00:00+08:00";
        assert_eq!(
            fresh(ANCIENT, Some(old_reinforce)),
            fresh(ANCIENT, None),
            "更旧的强化时间不该把新鲜度拉下去"
        );
    }

    #[test]
    fn usage_gain_is_log_saturated_and_bounded() {
        assert_eq!(usage_gain(0), 0.0);
        assert!(
            (usage_gain(10) - 1.0).abs() < 1e-9,
            "到饱和点就该拿满：{}",
            usage_gain(10)
        );
        assert_eq!(usage_gain(1000), 1.0, "用一千次也不能超过 1");
        assert!(usage_gain(1) > usage_gain(0) && usage_gain(2) < usage_gain(10) * 0.9);
        // 对数的意义：第一次的增益必须明显大于第十一次
        let first_step = usage_gain(1) - usage_gain(0);
        let eleventh_step = usage_gain(11) - usage_gain(10);
        assert!(
            first_step > eleventh_step,
            "饱和曲线写反了：{first_step} vs {eleventh_step}"
        );
    }

    #[test]
    fn unparsable_stamps_are_ignored_not_treated_as_fresh_or_dead() {
        // 认不出的那个时间戳不参与，而不是把整条判成"没有新鲜度"
        let recent = crate::memory::record::now_rfc3339();
        assert!(
            fresh("上周三", Some(&recent)) > 0.9,
            "created_at 手写坏了，强化时间还算得出"
        );
        assert_eq!(
            fresh("上周三", Some("前年")),
            0.0,
            "两个都读不懂就是没有时间证据"
        );
        assert!(
            fresh(&recent, Some("上周三")) > 0.9,
            "坏掉的强化章不该把好时间戳顶掉"
        );
        assert_eq!(age_days("昨天"), None, "认不出来要报 None，不能当 0 天");
    }
}
