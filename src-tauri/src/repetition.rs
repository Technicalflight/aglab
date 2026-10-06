//! 重复循环护栏：模型解码退化时会在极短的周期上无限复读（"最终。可以。最终。可以。……"），
//! 不掐的话一直烧到 max_tokens。流式增量在进合帧器之前先过这里，模式一成型
//! （最坏几百字符）就拉闸断流，循环前的正文原样保留。

/// 检测窗口。尾部只留这么多字符：判定只需要"结尾连着自相似了多少块"，
/// 窗口封顶让每次 push 的代价有界
const WINDOW_CHARS: usize = 720;
/// 累计正文少于这个数不启用：开头的几句小重复（礼貌铺陈、排比）离"退化"还远
const MIN_CHARS: usize = 200;

/// 触发线：周期越短，允许的重复次数越多。20×6≈120 字、8×48≈384 字——
/// 宁可多放几百字符过去，不可误杀正常回答
fn threshold(period: usize) -> usize {
    if period <= 6 {
        20
    } else if period <= 16 {
        14
    } else {
        8
    }
}

/// 尾部周期检测：对每个周期 p，数从结尾往前连续自相似的块数 k，
/// `k` 达到该周期的触发线且重复单位里至少有一个字母/汉字（纯标点/空白的
/// 长串是合法排版：分隔线、缩进）才算退化循环
pub fn detect(tail: &str) -> bool {
    let chars: Vec<char> = tail.chars().collect();
    let n = chars.len();
    if n < MIN_CHARS {
        return false;
    }
    for period in 1..=48usize {
        let blocks = n / period;
        let min_k = threshold(period);
        if blocks < min_k {
            continue;
        }
        let unit = &chars[n - period..];
        if !unit.iter().any(|c| c.is_alphanumeric()) {
            continue;
        }
        let mut k = 1;
        while k < blocks && chars[n - (k + 1) * period..n - k * period] == *unit {
            k += 1;
        }
        if k >= min_k {
            return true;
        }
    }
    false
}

/// 一个回合一份。正文与思考两条通道各记各的尾部窗——两条都会烧 token，
/// 复读也两条都出现过。围栏开合增量维护：光标在 ``` 代码块内时挂起检测，
/// 代码里合法地重复字符太常见（分隔注释、缩进、填充）
#[derive(Default)]
pub struct Guard {
    text: String,
    reasoning: String,
    in_fence: bool,
    fired: bool,
}

impl Guard {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂一条增量。返回 true = 这条通道已确认陷入重复循环（此后一直返回 true）
    pub fn push(&mut self, piece: &str, reasoning: bool) -> bool {
        if self.fired {
            return true;
        }
        let buffer = if reasoning {
            &mut self.reasoning
        } else {
            &mut self.text
        };
        // 围栏计数按增量算：``` 成对出现，奇数次跨越就是进出代码块一次
        let fences = piece.matches("```").count();
        if fences % 2 == 1 {
            self.in_fence = !self.in_fence;
        }
        buffer.push_str(piece);
        trim_to_window(buffer);
        if !self.in_fence && detect(buffer) {
            self.fired = true;
        }
        self.fired
    }
}

/// 只留尾部窗口。按字符截断（不劈 UTF-8）
fn trim_to_window(buffer: &mut String) {
    let overflow = buffer.chars().count().saturating_sub(WINDOW_CHARS);
    if overflow > 0 {
        let keep: String = buffer.chars().skip(overflow).collect();
        *buffer = keep;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repeated(unit: &str, times: usize) -> String {
        unit.repeat(times)
    }

    /// 跨过起步门槛的垫底话：让下面的断言只取决于触发线，不取决于长度门槛
    fn head() -> String {
        "一段正常的叙述，用来把累计窗口垫过起步门槛，这样下面的断言只取决于触发线本身，与长度无关。"
            .repeat(3)
    }

    #[test]
    fn a_short_period_loop_fires_at_the_twentieth_repeat() {
        // 截图里的形状："最终。可以。" 6 字符周期。第 19 次还差一点，第 20 次命中
        assert!(!detect(&format!("{}{}", head(), repeated("最终。可以。", 19))));
        assert!(detect(&format!("{}{}", head(), repeated("最终。可以。", 20))));
    }

    #[test]
    fn a_longer_period_needs_fewer_repeats() {
        let unit = "这一句话稍微长一点，作为重复单元刚好。";
        assert!(!detect(&format!("{}{}", head(), repeated(unit, 5))));
        assert!(detect(&format!("{}{}", head(), repeated(unit, 8))));
    }

    #[test]
    fn short_legitimate_repeats_do_not_fire() {
        // 诗文式的几连重复离触发线很远
        assert!(!detect(&format!("{}{}", head(), repeated("可以。", 3))));
        assert!(!detect(&format!("{}{}", head(), repeated("好的，我继续。", 4))));
    }

    #[test]
    fn punctuation_and_whitespace_runs_never_fire() {
        // 分隔线、长横线、缩进：纯标点/空白的单位直接豁免
        assert!(!detect(&format!("{}{}", head(), repeated("-", 300))));
        assert!(!detect(&format!("{}{}", head(), repeated(" ", 300))));
        assert!(!detect(&format!("{}{}", head(), repeated("—— ", 60))));
    }

    #[test]
    fn below_the_floor_the_detector_stays_silent() {
        // 长度门槛：两百字符以内的小抖动一律放行
        assert!(!detect(&repeated("最终。可以。", 30)));
    }

    #[test]
    fn code_fence_suspends_the_detector() {
        let mut guard = Guard::new();
        assert!(!guard.push(&head(), false));
        // 进围栏后即使无限复读字母也不拉闸——这是没有围栏豁免时必然命中的形状
        assert!(!guard.push("```text\n", false));
        for _ in 0..30 {
            assert!(!guard.push(&repeated("最终。可以。", 2), false));
        }
        // 出围栏后恢复检测：补满 20 个对齐的重复块立刻命中
        assert!(!guard.push("```\n", false));
        assert!(!guard.push(&repeated("最终。可以。", 19), false));
        assert!(guard.push(&repeated("最终。可以。", 1), false));
    }

    #[test]
    fn once_fired_the_guard_stays_fired_across_channels() {
        let mut guard = Guard::new();
        assert!(!guard.push(&head(), false));
        assert!(guard.push(&repeated("最终。可以。", 25), false));
        // 已触发后任何通道的后续增量都直接返回 true
        assert!(guard.push("x", false));
        assert!(guard.push("x", true));
    }

    #[test]
    fn reasoning_channel_is_guarded_too() {
        let mut guard = Guard::new();
        assert!(!guard.push(&head(), true));
        assert!(guard.push(&repeated("可以。", 25), true));
    }

    #[test]
    fn window_trimming_keeps_chars_intact() {
        let mut buffer = "汉".repeat(WINDOW_CHARS + 10);
        trim_to_window(&mut buffer);
        assert_eq!(buffer.chars().count(), WINDOW_CHARS);
    }
}
