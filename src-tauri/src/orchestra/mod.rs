//! 多助理 并行执行。
//!
//! 落点只有一个：**一次节点运行 = 一次正常回合**。编排器决定什么时候、以什么能力面、
//! 跑哪一次，它自己不碰模型、不碰文件、不碰审批。
//! 设计在 `deliverables/design-multi-agent.md`，验收映射在它的 §9。

pub mod graph;
pub mod judge;
pub mod orchestrator;
pub mod profile;
pub mod runtime;
