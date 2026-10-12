//! 一次请求失败的形状，以及"这一发该不该重试"的判定（O1-8 拆分时从 chat.rs 外置）。
//!
//! 失败也带着**已经流出来的那部分**：用户按了停止，那半截他在界面上读过了，不进日志
//! 下一轮模型就不知道它说过这句话（F15）——而它只能是"未写完"的落定形状。
//!
//! 重试判定（O12-2）在这里收口：**种类知道就按种类判**，状态码、掐流、停止、被拒
//! 四类都是结构化的，不看上游写的那句话；种类不知道（服务商在流里自己报的错、
//! 收尾失败）才退回老的字符串判据。字符串那一层是兜底，不再是主判据——上游把
//! "HTTP 500" 换个措辞，以前重试就静默不再发生，而且没人会发现。

use super::{RoundOutcome, STOP_MARK};
use crate::chat::wire::{EgressFail, FailKind, RETRYABLE_STATUS_WORD};

/// 一次回合最多自动重试几次（O12-1：从 2 提到 10）。
///
/// 敢提到 10 的前提是"从锚点重开"：有已提交的工具结果时，重试丢掉的只是
/// **未提交的尾部**，不再等于把用户已经读过的那段回答作废
pub(crate) const RETRY_BUDGET: u32 = 10;

/// 这一发该不该自动重试（O12-1）。
///
/// 三种情况：
/// - 没流出去过正文：照旧重试（预算内、种类可重试就行）；
/// - 流过正文，但这一轮里**有已提交的工具结果**（日志比回合开始时长了）：
///   锚点成立——从那儿重开，丢掉只是未提交的尾部；
/// - 流过正文又没有锚点：整轮就靠这段正文撑着，重发等于把用户读过的内容作废，不重试。
///
/// 判据用"日志涨没涨"而不是另记一个标志位：日志是只追加的，涨了就是有工具结果
/// 落了定，那一格本身就是锚点。返回 true 时调用方还要请界面丢掉已经流出去的那半截
pub(crate) fn should_retry(retryable: bool, attempts: u32, streamed: bool, log_grew: bool) -> bool {
    retryable && attempts < RETRY_BUDGET && (!streamed || log_grew)
}

pub(crate) struct RoundFailure {
    message: String,
    /// 失败的**种类**。只有"出口失败"那一条路知道它——服务商在流里自己报的错、
    /// 收尾失败这些没有种类，判定时退回文案。带到这里为止，
    /// 再往下（界面、审计）只需要那句话
    kind: Option<FailKind>,
    partial: RoundOutcome,
}

impl RoundFailure {
    /// 出口失败：种类随行。字段不外露，构造走这里——判定要用它，
    /// 别的地方不该去猜字段布局
    pub(crate) fn from_egress(fail: EgressFail, partial: RoundOutcome) -> Self {
        Self {
            message: fail.message,
            kind: Some(fail.kind),
            partial,
        }
    }

    /// 没有种类的失败：服务商在流里自己报的错（它是一句话，不是一个状态码）、
    /// 收尾失败（没正文这类）
    pub(crate) fn plain(message: String, partial: RoundOutcome) -> Self {
        Self {
            message,
            kind: None,
            partial,
        }
    }

    /// 失败原因。`stopped()` 与对外报错都读它，字段本身不外露
    pub(crate) fn message(&self) -> &str {
        &self.message
    }

    /// 这一发该不该重试。种类知道就按种类判；不知道才退回老的字符串判据
    pub(crate) fn retryable(&self) -> bool {
        match self.kind {
            Some(kind) => kind.retryable(&self.message),
            None => retryable_by_message(&self.message),
        }
    }

    pub(crate) fn stopped(&self) -> bool {
        self.message == STOP_MARK
    }

    /// 已经流出来的那部分。落库与"这一轮到底产出了什么"都读它
    pub(crate) fn partial(&self) -> &RoundOutcome {
        &self.partial
    }
}

/// 服务商错误的可重试分类：限流、上游故障、网络层失败才值得重试；
/// 鉴权（401/403）和路径错误（404）重试多少次结果都一样。
/// 流中断也在列：输出前断开会被 first_token_ms 门槛放行自动重试，
/// 输出后断开自然被门槛挡住——不会把半截回复重复发一遍。
///
/// 认不出种类时的兜底判据。**不该是主判据**——它认的是上游写的那句话，
/// 改一个措辞就静默失效。结构化判据住在 [`FailKind::retryable`]，
/// 这里只在"失败根本没有种类"时才用得上
fn retryable_by_message(error: &str) -> bool {
    [
        RETRYABLE_STATUS_WORD,
        "HTTP 500",
        "HTTP 502",
        "HTTP 503",
        "HTTP 504",
    ]
    .iter()
    .any(|hint| error.contains(hint))
        || error.starts_with("请求失败：")
        || error.contains("读取流中断")
        || error.contains("Peer disconnected")
        || error.contains("connection reset")
        || error.contains("timed out")
}

#[cfg(test)]
mod retry_tests {
    use super::{should_retry, RETRY_BUDGET};

    /// 没流出去过正文：老规矩，预算内且种类可重试就重试
    #[test]
    fn a_clean_failure_retries_within_budget() {
        assert!(should_retry(true, 0, false, false));
        assert!(should_retry(true, RETRY_BUDGET - 1, false, false));
        assert!(
            !should_retry(true, RETRY_BUDGET, false, false),
            "预算用完就停"
        );
        assert!(!should_retry(false, 0, false, true), "种类不可重试就不重试");
    }

    /// O12-1 的正题：流过正文也能重试——前提是这一轮里有已提交的工具结果当锚点
    #[test]
    fn streamed_output_only_retries_when_there_is_an_anchor() {
        assert!(
            should_retry(true, 0, true, true),
            "有锚点：丢的只是未提交的尾部，值得重开"
        );
        assert!(
            !should_retry(true, 0, true, false),
            "没锚点：整轮就靠这段正文撑着，重发等于把用户读过的内容作废"
        );
        assert!(
            !should_retry(true, RETRY_BUDGET, true, true),
            "预算仍是硬上限"
        );
    }
}
