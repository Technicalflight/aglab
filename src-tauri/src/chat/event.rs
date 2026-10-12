//! 回合事件（O1-8 拆分时从 chat.rs 外置）。
//!
//! 这一层是**出口的形状**：回合里发生的事只有经它才到得了界面与 worker 回程。
//! 它自己不干活——没有函数、没有状态，只有一串 serde 形状。
//! 拆出来是因为它是 chat.rs 里最"只读"的一块，而 chat.rs 是棘轮盯着的那个文件。

use serde::Serialize;

use super::{AskOption, ModeView, PlanStep, ToolStatus};

use crate::tool_runtime::RuleSuggestion;

#[derive(Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ChatEvent {
    Delta {
        text: String,
    },
    /// 推理模型的思维链，OpenAI 兼容服务商放在 delta.reasoning_content
    Reasoning {
        text: String,
    },
    // 枚举级 rename_all 不作用于结构体变体字段，必须逐变体声明
    #[serde(rename_all = "camelCase")]
    Tool {
        id: String,
        name: String,
        status: ToolStatus,
        risk: String,
        input: String,
        output: Option<String>,
        /// 原始参数 JSON 文本。前端落盘，话题恢复时用于重放历史
        arguments: Option<String>,
        /// 这一发**该问而没问**：凭什么过的闸，一句人话。`None` = 要么本来就无需
        /// 询问（权限表那一档放行），要么刚刚弹过窗。见 `pass_reason` 那段
        pass_reason: Option<String>,
        /// 这一发声明时，本轮正文已经流出了多少个 **UTF-16 码元**（前端 JS 字符串
        /// 的下标口径）。前端按它把正文切片、把工具行插回原文流——过程因此是
        /// 流式的（文字→工具→文字），不再全部堆在消息顶上
        content_chars: Option<u32>,
        /// 从这次调用推断的**可复用规则**建议（O14-2）。只有**真的在问**的那一发才带；
        /// 其余状态与老事件都没有这一格
        #[serde(default, skip_serializing_if = "Option::is_none")]
        suggestion: Option<RuleSuggestion>,
    },
    #[serde(rename_all = "camelCase")]
    Done {
        input_tokens: u32,
        output_tokens: u32,
        duration_ms: u64,
        /// 命中服务商缓存的输入部分；null = 服务商没回这个字段。
        /// 前端用它即时改判缓存命中率，不必再查一次台账（第 6 步 setCacheHit 的数据出口）
        cached_tokens: Option<u32>,
        /// 本轮往日志里新登记的条目 id，按登记顺序。界面上"重新生成/编辑重发"要指名
        /// 移到哪条之后，而那个名字只有后端知道
        entry_ids: Vec<String>,
        /// 这一发**实际发出去**的模型名。池子会换人、路由表会改名，所以它和设置里那个
        /// 顶层 model 可以不一样——而界面那行"这句是谁答的"只有这里答得了
        model: String,
        /// 这一发**实际生效**的上下文窗口。分母跟着路由走：别的档案/池成员的模型行
        /// 才是真相（300K 配在 A 档案、当前连接是 B 档案时，顶层读数是假的），
        /// 用量面板拿它当分母，与预算表/压缩闸同一格读数
        context_tokens: u32,
    },
    /// 服务商没报错但这一轮不完整（输出被截断、被安全策略拦下）。
    /// 半句话静静地当完整答案显示出来是最难查的骗人方式
    Notice {
        text: String,
    },
    /// 目标模式每一轮跑完之后的续跑读数。它必须坐在 [`ChatEvent::Done`] **前面**：
    /// 界面在 Done 上就落"这一轮结束了、后面收到的字统统作废"，之后再告诉它
    /// "还有一轮要接"就晚了——那一轮的字节会被整批丢掉。
    /// 带整份读数是因为"跑到第几轮、停在哪儿"只有后端知道，别让前端回来猜一次
    #[serde(rename_all = "camelCase")]
    Mode {
        /// 后端还会不会自己接下一轮
        continuing: bool,
        /// 这一支现在的作业模式
        state: ModeView,
    },
    /// 一次文件快照已落账（write_file/delete_file 的动手前副本）。快照从静默的
    /// 台账写变成显式事件：界面的变更面板可以即时点亮，审计里也有这一笔——
    /// "AI 刚刚动了哪个文件、能不能回滚"不该等人打开面板才知道
    #[serde(rename_all = "camelCase")]
    FileSnapshot {
        /// 相对工作目录根的展示路径
        path: String,
        /// 对应的工具调用 id，与 Tool 事件对得上
        call_id: String,
        additions: u32,
        deletions: u32,
        /// 动手前存了一份可恢复的副本（快照或备份）；false = 太大没存，note 里有原因
        backup: bool,
        /// 没存快照时给人看的说法；存了就是空串
        snapshot_note: String,
    },
    /// 服务商报错了、但这一轮还会自己重来。它既不是"回答的一部分"也不是"本轮残缺"的
    /// 自证——重试成功后接出来的答案是完整的，所以别把它混进正文里冒充模型说的话
    #[serde(rename_all = "camelCase")]
    Retry {
        /// 说清还要等多久、第几次
        text: String,
        /// 失败原因原文，交给告警条的正文
        reason: String,
        /// 这一发**已经流出去的那半截正文要丢掉**（O12-1）：断流时已经吐了一部分，
        /// 而从锚点重开会给出另一个答案——不丢的话新答案接在旧的半截后面就废了。
        /// 没流过正文（或整轮就靠那段正文撑着、不重试）时是 false，界面不用动
        #[serde(default)]
        discarded: bool,
    },
    /// 上下文压缩的时间线（O13-1）。start 时界面显示"压缩中"，done 时用 summary
    /// 替换掉被压缩的旧消息（kept 是保留原文的条数），interrupted 是"没善终"的落定。
    ///
    /// `operation_id` 把同一次压缩的几个阶段串成一条线；`attempt` / `max_attempts`
    /// 是"第几档成的"——摘要请求自己会降级重试（全量 → 只摘后半段 → 逐段截断），
    /// 界面据此能说出"这一次压得有多费劲"。三格都是后加的，老事件缺省为 None
    #[serde(rename_all = "camelCase")]
    Compaction {
        phase: String,
        operation_id: Option<String>,
        attempt: Option<u32>,
        max_attempts: Option<u32>,
        summary: Option<String>,
        kept: Option<usize>,
    },
    Error {
        message: String,
    },
    /// 请求链路的阶段探针（输入/载荷/出站/首字节/模型对账）。不进日志、不参与成败：
    /// 它是消息头行那条链路动画的数据源，前端按 key 映射图标与文案。
    /// `tone` 是格子的读色（ok 一致 / info 中性 / warn 要告警），`hint` 是悬停
    /// 展开的完整三元组——两格都缺省，老探针帧一个字节不多
    #[serde(rename_all = "camelCase")]
    Probe {
        key: String,
        detail: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tone: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hint: Option<String>,
    },
    /// 模型整份上报的计划清单（update_plan）。前端拿它画计划卡，不再查、不再算；
    /// 排在工具事件之外，因为它的生命周期是整场话题，不是一次工具调用
    #[serde(rename_all = "camelCase")]
    Plan {
        explanation: Option<String>,
        steps: Vec<PlanStep>,
    },
    /// ask_user 的提问。后端这一发挂起等人；`id` 是回答通道的钥匙（ask_user_respond）。
    /// stop 打断后这里发过的问题随工具结果"用户没有回答"一起作废，前端按 done 清卡
    #[serde(rename_all = "camelCase")]
    Ask {
        id: String,
        question: String,
        options: Vec<AskOption>,
    },
}
