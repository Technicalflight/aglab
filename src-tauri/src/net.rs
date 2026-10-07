//! 出站 HTTP 的公共超时装配。原先住在 chat.rs——模型请求的家，
//! 于是知识库 OCR/embedding、探针、模型目录这些"同样要发请求但不是聊天"的
//! 模块都得反过来摸 crate::chat：数据与策略层反向依赖执行引擎，巨石就是这么
//! 一块块长胖的。搬到这里之后，依赖方向恢复成单向：大家 → net → ureq。

use std::time::Duration;

/// 一条流最多等多久。没有这个上限时，一个挂死的请求会把整条调度线程永久冻住——
/// 真机上就是这么发生的：定时任务页一直显示"就在这一轮"，而线程卡在 8 分钟前那次没返回的请求里
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(90);
// 整流上限放宽到 10 分钟：极高思考力度 + 长输出可能超过 5 分钟，
// 误杀正在思考的流比多等几分钟危害更大
const WHOLE_STREAM_TIMEOUT: Duration = Duration::from_secs(600);

/// 整流上限的唯一外部用户是 chat.rs 的流式读取（极高思考力度的长输出要等）
pub(crate) fn whole_stream_timeout() -> Duration {
    WHOLE_STREAM_TIMEOUT
}

pub fn with_timeouts<Any>(
    request: ureq::RequestBuilder<Any>,
    body_timeout: Duration,
) -> ureq::RequestBuilder<Any> {
    request
        .config()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(FIRST_BYTE_TIMEOUT))
        .timeout_recv_body(Some(body_timeout))
        .build()
}
