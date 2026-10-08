//! TurnHost 的重活/通知交互面（M3 收官）。
//!
//! 单独成文件的缘故：这些方法体带 `#[cfg(test)]`/`#[cfg(not(test))]` 门，
//! 而 chat.rs 的守卫测试按"第一个 #[cfg(test)]"切生产面——门留在 chat.rs
//! 中段会把守卫的针脚全部切瞎。挪到这里，chat.rs 的不变量恢复：
//! 测试模块只在文件尾。island.rs 同款先例的延伸。
//!
//! 门控语义：
//! - Main（`app` 有值）：真通知/真子系统/真子助理/真浏览器；
//! - worker（`app` 为 None）：toast 走 ev 由 Main 代发，重活类诚实拒绝；
//! - 测试构建：重活类与真通知都不编译（muda 的 TaskDialogIndirect 是
//!   comctl32 v6-only 入口，测试二进制没有 v6 清单，加载即 0xc0000139）。

use crate::chat::{ChatEvent, EventSink, TurnHost, WorkerTurnParams};
use crate::config::AppConfig;

/// 重活类（子助理/子系统工具/agent 控制/浏览器）在 worker 侧的诚实回话。
/// 它们要在主窗口进程里动真子系统，M3.5 之前不假装能跑
pub(crate) const HEAVY_TOOL_UNAVAILABLE: &str = "这个工具要在主窗口进程里执行，后台 Agent 子进程这一发暂不支持。等它搬进子进程，或在设置里关掉「回合跑在 Agent 子进程」再试。";

impl crate::chat::TurnHost {
    fn emit(&self, event: &str, data: serde_json::Value) {
        if let Some(ev) = &self.ev {
            ev(event, data);
        }
    }

    /// 窗口在后台时系统通知喊人。worker 发 ev，Main 收到后代发真通知。
    /// 测试构建走 ev 形状：真通知的 get_webview_window 链会拉起 muda
    /// （TaskDialogIndirect 是 comctl32 v6-only，测试二进制没有 v6 清单，island.rs 同款先例）
    pub(crate) fn toast_approval_needed(&self, input: &str) {
        #[cfg(not(test))]
        match &self.app {
            Some(app) => crate::toast::approval_needed(app, input),
            None => self.emit("toast", serde_json::json!({ "kind": "approval_needed", "input": input })),
        }
        #[cfg(test)]
        {
            let _ = (input, &self.app);
            self.emit("toast", serde_json::json!({ "kind": "approval_needed" }));
        }
    }

    pub(crate) fn toast_unattended_parked(&self, display: &str) {
        #[cfg(not(test))]
        match &self.app {
            Some(app) => crate::toast::unattended_parked(app, display),
            None => self.emit("toast", serde_json::json!({ "kind": "unattended_parked", "display": display })),
        }
        #[cfg(test)]
        {
            let _ = (display, &self.app);
            self.emit("toast", serde_json::json!({ "kind": "unattended_parked" }));
        }
    }

    pub(crate) fn toast_question_pending(&self, question: &str) {
        #[cfg(not(test))]
        match &self.app {
            Some(app) => crate::toast::question_pending(app, question),
            None => self.emit("toast", serde_json::json!({ "kind": "question_pending", "question": question })),
        }
        #[cfg(test)]
        {
            let _ = (question, &self.app);
            self.emit("toast", serde_json::json!({ "kind": "question_pending" }));
        }
    }

    pub(crate) fn toast_goal_settled(&self, complete: bool, note: &str) {
        #[cfg(not(test))]
        match &self.app {
            Some(app) => crate::toast::goal_settled(app, complete, note),
            None => self.emit("toast", serde_json::json!({ "kind": "goal_settled", "complete": complete, "note": note })),
        }
        #[cfg(test)]
        {
            let _ = (complete, note, &self.app);
            self.emit("toast", serde_json::json!({ "kind": "goal_settled" }));
        }
    }

    /// 保温排程要 app 起线程发预热请求，是 Main 的特权；worker 不保温
    pub(crate) fn warm_schedule(
        &self,
        config: &AppConfig,
        warm: &crate::warm::Hub,
        plan: crate::warm::Plan,
        rows: Vec<serde_json::Value>,
        declared: Vec<serde_json::Value>,
    ) {
        #[cfg(not(test))]
        if let Some(app) = &self.app {
            crate::warm::schedule(app, config, warm, plan, rows, declared);
        }
        #[cfg(test)]
        {
            let _ = (config, warm, plan, rows, declared);
        }
    }

    pub(crate) fn spawn_subagent(
        &self,
        conversation_id: &str,
        config: &AppConfig,
        args: &serde_json::Value,
    ) -> Result<String, String> {
        // 测试编译不含重活链：run_from_chat 的窗口/菜单链会拉起 muda 的
        // TaskDialogIndirect（comctl32 v6-only 入口），而测试二进制没有 v6 清单
        // （island.rs 同款先例）。测试里与 worker 模式一样诚实报"不支持"
        #[cfg(test)]
        {
            let _ = (conversation_id, config, args);
            return Err(HEAVY_TOOL_UNAVAILABLE.to_string());
        }
        #[cfg(not(test))]
        match &self.app {
            Some(app) => crate::spawn::run_from_chat(app, conversation_id, config, args),
            None => Err(HEAVY_TOOL_UNAVAILABLE.to_string()),
        }
    }

    pub(crate) fn subsystem_tool(
        &self,
        config: &AppConfig,
        conversation_id: &str,
        name: &str,
        args: &serde_json::Value,
        stop: &std::sync::atomic::AtomicBool,
    ) -> Result<String, String> {
        #[cfg(not(test))]
        match &self.app {
            Some(app) => crate::chat::subsystem_tool_exec(app, config, conversation_id, name, args, stop),
            None => Err(HEAVY_TOOL_UNAVAILABLE.to_string()),
        }
        #[cfg(test)]
        {
            let _ = (config, conversation_id, name, args, stop);
            Err(HEAVY_TOOL_UNAVAILABLE.to_string())
        }
    }

    pub(crate) fn agent_control(&self, action: &str, agent_id: &str, message: &str) -> Result<String, String> {
        #[cfg(not(test))]
        match &self.app {
            Some(app) => crate::chat::agent_control_exec(app, action, agent_id, message),
            None => Err(HEAVY_TOOL_UNAVAILABLE.to_string()),
        }
        #[cfg(test)]
        {
            let _ = (action, agent_id, message);
            Err(HEAVY_TOOL_UNAVAILABLE.to_string())
        }
    }

    pub(crate) fn browser_tool(&self, config: &AppConfig, args: &serde_json::Value) -> Result<String, String> {
        // 测试编译不含浏览器链（CDP WebSocket 同 muda 一起进来）：island.rs 同款先例
        #[cfg(test)]
        {
            let _ = (config, args);
            return Err(HEAVY_TOOL_UNAVAILABLE.to_string());
        }
        #[cfg(not(test))]
        match &self.app {
            Some(app) => crate::browser::handle_tool(app, config, args),
            None => Err(HEAVY_TOOL_UNAVAILABLE.to_string()),
        }
    }
}

pub(crate) fn run_worker_turn(
    config_dir: &std::path::Path,
    data_dir: &std::path::Path,
    runtime: &crate::agent_host::WorkerRuntime,
    params: WorkerTurnParams,
    stop: &std::sync::atomic::AtomicBool,
    emit: std::sync::Arc<dyn Fn(&str, serde_json::Value) + std::marker::Send + std::marker::Sync>,
) -> Result<(), (String, String)> {
    // 测试构建不含全量回合：run_turn 的工具链会拉起 muda 的 TaskDialogIndirect
    // （comctl32 v6-only），而测试二进制没有 v6 清单，加载即 0xc0000139。
    // lib 单测不覆盖 turn.start 的真回合（校验闸有 turn.once 同款测试钉着），
    // 真回合的进程级验收在集成测试与真机
    #[cfg(test)]
    {
        let _ = (config_dir, data_dir, runtime, &params, stop, &emit);
        return Err((
            "test_build".into(),
            "测试构建不含全量回合体。".into(),
        ));
    }
    #[cfg(not(test))]
    run_worker_turn_impl(config_dir, data_dir, runtime, params, stop, emit)
}

fn run_worker_turn_impl(
    config_dir: &std::path::Path,
    data_dir: &std::path::Path,
    runtime: &crate::agent_host::WorkerRuntime,
    params: WorkerTurnParams,
    stop: &std::sync::atomic::AtomicBool,
    emit: std::sync::Arc<dyn Fn(&str, serde_json::Value) + std::marker::Send + std::marker::Sync>,
) -> Result<(), (String, String)> {
    // 测试构建不含全量回合：run_turn 的工具链会拉起 muda（TaskDialogIndirect，
    // comctl32 v6-only），测试二进制没有 v6 清单。lib 单测不覆盖 turn.start
    // 的真回合（校验闸有 turn.once 的同款测试钉着），真回合走集成测试与真机
    // 校验闸（顺序与 turn.once 同一档：确定性错误先挡，不碰会话）
    let config = crate::config::load_from_dir(config_dir);
    if config.base_url.trim().is_empty() {
        return Err(("no_provider".into(), "配置里没有服务商地址。".into()));
    }
    let _key = crate::config::api_key(&config)
        .map_err(|error| ("no_credentials".into(), format!("密钥解析失败：{error}")))?;
    // input 为空是合法的：带 rewind_to 的重试/重新生成（内联路同一语义——
    // 末端移回那句问题再长出新枝，不追加新的用户发言）。真正不合法的是
    // 两者都空且没有回溯点
    if params.conversation_id.trim().is_empty()
        || (params.input.trim().is_empty() && params.rewind_to.is_none())
    {
        return Err((
            "bad_params".into(),
            "conversationId 与 input 都不能为空（带 rewind_to 的重试除外）。".into(),
        ));
    }
    // 用户这一发要的模型名（池子/路由改写之前）：模型对账的 requested 格
    let requested_model = config.model.clone();
    // 池子与路由：与界面同一条调度，亲和键就是这条话题本身；
    // 决策层缺席（pick=None），decision 模式退化为策略调度
    struct EvForward {
        emit: std::sync::Arc<dyn Fn(&str, serde_json::Value) + std::marker::Send + std::marker::Sync>,
    }
    impl EventSink for EvForward {
        fn send(&self, event: ChatEvent) {
            let value = serde_json::to_value(event).unwrap_or_default();
            (self.emit)("chat", value);
        }
    }
    let forward = EvForward { emit: emit.clone() };
    let (turn_config, _pool_turn) = match crate::pool::resolve_in(
        config_dir,
        &config,
        &params.input,
        None,
        &params.conversation_id,
        // 子进程里的回合对"不许派工"豁免：它由用户在设置里亲手开的闸（同后台任务）
        true,
        &[],
    ) {
        Ok(Some(turn)) => {
            if turn.picked.source == "fallback" {
                let _ = forward.send(ChatEvent::Retry {
                    text: "由调度器兜底。".into(),
                    reason: "决策层这次没选成。".into(),
                });
            }
            (turn.config, Some(turn.guard))
        }
        Ok(None) => {
            // 池子没接管：路由表在设置直连之前查一遍（与 chat_send 同一档）
            let mut routed = config.clone();
            crate::route::apply(&mut routed);
            (routed, None)
        }
        Err(message) => return Err(("turn_failed".into(), message)),
    };
    let mcp_servers = crate::mcp::all_servers_in(&turn_config, data_dir);
    let skills = match crate::skills::prompt_in(config_dir, data_dir) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("技能没能加载：{error}");
            None
        }
    };
    let host = TurnHost::for_worker(
        config_dir.to_path_buf(),
        data_dir.to_path_buf(),
        emit.clone(),
        runtime.pause.clone(),
        runtime.follow_up.clone(),
        runtime.guards.clone(),
        runtime.mode_hub.clone(),
    );
    let result = crate::chat::run_turn(
        &host,
        &turn_config,
        &requested_model,
        &runtime.approvals,
        &runtime.mcp,
        &mcp_servers,
        stop,
        &runtime.steering,
        &runtime.warm,
        skills,
        // worker 这一发不是 goal 续跑轮（挂目标的闸在 Main 门口）
        false,
        params.input,
        params.attachments,
        params.rewind_to.as_deref(),
        params.rewind_to_root,
        params.skip_memory,
        None,
        // 不自己接下一轮：worker 只跑一发，收尾即停
        false,
        &params.conversation_id,
        &forward,
    );
    match result {
        Ok(_next) => Ok(()),
        Err(message) => Err(("turn_failed".into(), message)),
    }
}

