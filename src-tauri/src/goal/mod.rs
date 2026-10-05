//! 目标模式的引擎。
//!
//! 这里只有**决策**，没有 IO。一轮跑完时该落哪一行、还要不要自己接下一轮、
//! 人排的话与自动续跑谁先走、账读不出来时算不算花了钱——这些从前散在 `chat.rs`
//! 那段续跑循环里，而那段循环需要 `AppHandle` 与真话题日志才跑得动，所以从落地起
//! 就没有一条测试（working-modes §8 自己欠着的那笔债）。搬成"吃事实、吐效果"之后，
//! 它是纯的，每条判据都能单测；`chat.rs` 只负责把效果一条条执行掉。

pub mod contract;
pub mod machine;

pub use machine::{
    decide_after_round, round_outcome, Effect, Guard, RoundInput, RoundOutcome, Spend,
};
