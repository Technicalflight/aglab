//! 话题层：一条只追加的条目日志，加上它唯一的一份投影。
//!
//! 模块分工照 pi 出货版的话题层（`coding-agent/src/core/session-manager.ts`）切，另外借
//! durable harness 的两条构造：身份由存储层铸造、未落定的回答进不了类型。规范与理由见
//! `deliverables/design-conversation-layer.md` §1–§5。
//!
//! `layers` 与 `inspector` 是纯读侧的两块：给投影贴层标签、按层分账、现算一份派生视图。
//! 写路径一个字节都不经过它们——层一旦参与写侧，日志就不再是唯一事实。

pub mod context;
pub mod entry;
pub mod inspector;
pub mod layers;
pub mod legacy;
pub mod log;
pub mod mode;
pub mod prefix;
pub mod sections;
pub mod send;
pub mod store;

pub use context::{project, ModelStamp, Omitted, Projection};
pub use entry::{
    Entry, EntryPayload, Message, NewEntry, PendingAssistant, Role, SettledAssistant, StopReason,
    ToolCall, UsageRecord,
};
pub use inspector::{inspect, InspectorReport};
pub use layers::{
    budget, classify, plan, thread_chars, uses, BreakReason, Budget, BudgetInput, Concession,
    Estimate, EstimateKind, Layer, LayerBudget, LayerUse, Plan, DECLARATIONS_TYPE,
    KEEP_RECENT_CHARS,
};
pub use log::{SessionError, SessionLog};
pub use store::{load, path_for, save, SessionHeader, CURRENT_VERSION};

/// 条目 id 与话题 id 的合法字符集。id 会进文件名，所以这条既是数据校验也是路径校验
pub(crate) fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .as_bytes()
            .iter()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'z' | b'_' | b'-'))
}

/// 墙上时间（毫秒）。条目的 timestamp 由调用方带进来，为的是测试能钉住一个确定值，
/// 而不是让"同一份日志两次投影出不同字节"这种只在深夜复现的漂移有机会存在
pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}
