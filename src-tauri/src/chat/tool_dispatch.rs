//! 工具派发的路由梯子（优化路线 O1-5 从 chat.rs 拆出）。
//!
//! 一条工具调用进来，先过特判（goal_report / spawn_subagent / browser /
//! obs_recall / update_plan / ask_user——这些要么要 `Send`、要么要 `TurnHost`、
//! 要么要进程内的观察存档，注册表够不着），再落 run_program 的 PTC 脚本路，
//! 最后才是 Registry 的三条来源（内置 / 扩展 / 技能）。
//!
//! 只做路由与执行，不做审批：闸链在 [`super::turn_body`] 的调用点之前，
//! 结果的事后处理（edits 落账、快照广播）在调用点之后。

use super::{
    mode_goal::report_goal, tool_runtime, tools, web_fetch_for_model, web_search_for_model,
    AppConfig, ApprovalHub, AskOption, ChatEvent, EventSink, PlanStep, Send, ToolCallBuffer,
    TurnHost,
};
use serde_json::{json, Value};

pub(in crate::chat) fn route(
    send: &mut Send,
    host: &TurnHost,
    config: &AppConfig,
    root: Option<&std::path::Path>,
    conversation_id: &str,
    hub: &ApprovalHub,
    stop: &std::sync::atomic::AtomicBool,
    via_mcp: bool,
    call: &ToolCallBuffer,
    args: &Value,
    policy: &crate::policy::Policy,
    mcp_servers: &[crate::config::McpServer],
    mcp_hub: &crate::mcp::Hub,
    registry: &tool_runtime::source::Registry,
    on_event: &dyn EventSink,
) -> tool_runtime::source::Executed {
    if !via_mcp && call.name == "goal_report" {
        // 上报动的是这一轮自己那份日志，所以它得在 `send` 上写。另开一次话题去写
        // 同一个文件就是两个写者，后收尾的那一份会把前一份整片盖掉。
        // 完成门的复跑也在这儿给：走的就是 run_command 的真执行路
        // （前台那一条自带 60 秒超时），风险分档由 report_goal 内部问 classify
        let rerun = |command: &str| -> Result<String, String> {
            registry
                .run(
                    tool_runtime::source::shared_cache(),
                    "run_command",
                    &serde_json::json!({ "command": command }),
                )
                .output
                .map_err(|error| error.to_string())
        };
        // 目标这一支到头了（complete/blocked 都是终点）：窗口在后台时喊一声。
        // 预算烧到顶那类"没走到上报"的停下不打扰——它没有一句能说清的结论可带
        host.toast_goal_settled(
            args["status"].as_str() == Some("complete"),
            args["note"].as_str().unwrap_or_default(),
        );
        tool_runtime::source::Executed {
            output: report_goal(send, args, root, &rerun, on_event)
                .map_err(tool_runtime::source::ToolError::content),
            source: tool_runtime::source::Kind::Builtin,
            cached: false,
            attempts: 1,
        }
    } else if !via_mcp && call.name == "spawn_subagent" {
        tool_runtime::source::Executed {
            output: host
                .spawn_subagent(conversation_id, config, args)
                .map_err(tool_runtime::source::ToolError::content),
            source: tool_runtime::source::Kind::Builtin,
            cached: false,
            attempts: 1,
        }
    } else if !via_mcp
        && matches!(
            call.name.as_str(),
            "cron_list"
                | "cron_create"
                | "cron_delete"
                | "task_run_now"
                | "plan_mode"
                | "wait_agent"
                | "memory_search"
                | "memory_timeline"
                | "search_history"
        )
    {
        // 定时任务/规划模式/等待子助理/记忆与历史检索：都要 AppHandle 与
        // 各子系统的状态，注册表够不着——agent_control 是同款先例
        tool_runtime::source::Executed {
            output: host
                .subsystem_tool(config, conversation_id, &call.name, args, stop)
                .map_err(tool_runtime::source::ToolError::content),
            source: tool_runtime::source::Kind::Builtin,
            cached: false,
            attempts: 1,
        }
    } else if !via_mcp && call.name == "agent_control" {
        let action = args["action"].as_str().unwrap_or_default().to_string();
        let agent_id = args["agent_id"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_string();
        let message = args["message"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_string();
        let executed = host.agent_control(&action, &agent_id, &message);
        tool_runtime::source::Executed {
            output: executed.map_err(tool_runtime::source::ToolError::content),
            source: tool_runtime::source::Kind::Builtin,
            cached: false,
            attempts: 1,
        }
    } else if !via_mcp && call.name == "web_fetch" {
        tool_runtime::source::Executed {
            output: web_fetch_for_model(config, args)
                .map_err(tool_runtime::source::ToolError::content),
            source: tool_runtime::source::Kind::Builtin,
            cached: false,
            attempts: 1,
        }
    } else if !via_mcp && call.name == "web_search" {
        // 联网搜索：执行要配置里的 key、出口名单与代理——web_fetch 是同款先例
        tool_runtime::source::Executed {
            output: web_search_for_model(config, args)
                .map_err(tool_runtime::source::ToolError::content),
            source: tool_runtime::source::Kind::Builtin,
            cached: false,
            attempts: 1,
        }
    } else if !via_mcp && call.name == "browser" {
        // 内置浏览器：执行要 BrowserHub 的状态（拉起/复用浏览器进程与 CDP 通道），
        // 注册表够不着——spawn 与 web_fetch 是同款先例。动作之后的新快照
        // 直接当工具结果交回，模型不需要第二次调用就知道页面变成了什么
        tool_runtime::source::Executed {
            output: host
                .browser_tool(config, args)
                .map_err(tool_runtime::source::ToolError::content),
            source: tool_runtime::source::Kind::Builtin,
            cached: false,
            attempts: 1,
        }
    } else if !via_mcp && call.name == "obs_recall" {
        // 观察召回：存档住在话题线程的内存里，注册表够不着——
        // spawn 与 web_fetch 是同款先例
        tool_runtime::source::Executed {
            output: crate::observations::recall(
                args.get("handle")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                args.get("start").and_then(Value::as_u64).unwrap_or(0) as usize,
                args.get("limit").and_then(Value::as_u64).unwrap_or(4000) as usize,
            )
            .map_err(tool_runtime::source::ToolError::content),
            source: tool_runtime::source::Kind::Builtin,
            cached: false,
            attempts: 1,
        }
    } else if !via_mcp && call.name == "update_plan" {
        // 计划更新是控制信号：整份转给界面，模型这边只要一句确认。
        // 参数形状已经过了 check_arguments 那道闸，这里只做搬运
        let steps: Vec<PlanStep> = args["steps"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|item| PlanStep {
                        title: item["title"].as_str().unwrap_or_default().to_string(),
                        status: item["status"].as_str().unwrap_or("pending").to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        on_event.send(ChatEvent::Plan {
            explanation: args["explanation"].as_str().map(str::to_string),
            steps: steps.clone(),
        });
        tool_runtime::source::Executed {
            output: Ok(format!(
                "计划已更新（{} 步）。状态变化时整份重发；全部完成后不用再调它。",
                steps.len()
            )),
            source: tool_runtime::source::Kind::Builtin,
            cached: false,
            attempts: 1,
        }
    } else if !via_mcp && call.name == "ask_user" {
        // 结构化提问：这一发挂起等用户点选（stop 可打断）。
        // 无人值守没有人可问：直接回一句"自己判断"，别让定时任务在按钮上等一夜
        if crate::tasks::escalate::is_unattended(conversation_id) {
            tool_runtime::source::Executed {
                output: Ok(
                    "无人值守运行，没有人可以回答这个问题。按你最有把握的选项继续，\
                             并在结果里说明你替用户做了哪个决定。"
                        .into(),
                ),
                source: tool_runtime::source::Kind::Builtin,
                cached: false,
                attempts: 1,
            }
        } else {
            let options: Vec<AskOption> = args["options"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .map(|item| AskOption {
                            label: item["label"].as_str().unwrap_or_default().to_string(),
                            description: item["description"].as_str().map(str::to_string),
                        })
                        .collect()
                })
                .unwrap_or_default();
            on_event.send(ChatEvent::Ask {
                id: call.id.clone(),
                question: args["question"].as_str().unwrap_or_default().to_string(),
                options,
            });
            // 后台话题的提问也是"有人等你"：窗口不在前台时喊一声
            host.toast_question_pending(args["question"].as_str().unwrap_or_default());
            let answer = hub.wait_answer(&call.id, stop);
            tool_runtime::source::Executed {
                output: Ok(match answer {
                    Some(text) => format!("用户选择了：{text}"),
                    None => "用户没有回答（已停止生成）。按当前信息继续，\
                                     或说明还缺什么才能继续。"
                        .into(),
                }),
                source: tool_runtime::source::Kind::Builtin,
                cached: false,
                attempts: 1,
            }
        }
    } else if !via_mcp && call.name == "run_program" {
        // PTC code-mode：模型写 Rhai 脚本，脚本内 tool() 调 Safe 工具（写操作走
        // 常规工具调用的审批闸）。扩展工具（mcp__*）也开，但只走"权限表直接放行"
        // 的那扇门：直调要问人的扩展工具在脚本里执行等于绕开那一声问，不开口子。
        // 常驻变量域按话题取：上一发的 let 这一发还在，reset=true 从零开始
        let script = args["script"].as_str().unwrap_or_default().to_string();
        let exec_root = root.map(|path| path.to_path_buf());
        let exec_owner = conversation_id.to_string();
        let exec_policy = policy.clone();
        let exec_servers = mcp_servers.to_vec();
        let exec_config = config.clone();
        let exec_hub = mcp_hub.clone();
        let exec = move |name: &str, args_json: &str| -> Result<String, String> {
            let parsed_args: Value = serde_json::from_str(args_json).unwrap_or(json!({}));
            if name.starts_with("mcp__") {
                let call = tool_runtime::Call::new(name, &parsed_args, exec_root.as_deref(), true);
                if tool_runtime::capabilities_for(&call).iter().any(|cap| {
                    matches!(
                        exec_policy.resolve(cap),
                        crate::policy::Level::Ask | crate::policy::Level::Deny
                    )
                }) {
                    return Err(format!(
                        "PTC 脚本里的 {name} 过不了权限表（要问人或被禁）。\
                                 退出脚本直调它一次把这一步办了，再回脚本组合结果。"
                    ));
                }
                let source = tool_runtime::source::McpSource {
                    servers: &exec_servers,
                    config: &exec_config,
                    hub: &exec_hub,
                };
                return tool_runtime::source::ToolSource::call(&source, name, &parsed_args)
                    .map_err(|error| error.text);
            }
            let risk = tools::classify(name, &parsed_args, exec_root.as_deref());
            if risk != tools::Risk::Safe {
                return Err(format!(
                    "PTC 脚本只能调用只读工具（{name} 是 {}）。写操作请退出脚本后用常规工具调用。",
                    risk.as_str()
                ));
            }
            tools::execute_for(name, &parsed_args, exec_root.as_deref(), Some(&exec_owner))
        };
        if args["reset"].as_bool().unwrap_or(false) {
            tool_runtime::ptc::forget_scope(conversation_id);
        }
        let scope = tool_runtime::ptc::conversation_scope(conversation_id);
        let mut guard = scope
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let run_result = tool_runtime::ptc::run_in(&script, Box::new(exec), &mut guard);
        tool_runtime::source::Executed {
            output: run_result
                .map(|result| result.output)
                .map_err(tool_runtime::source::ToolError::content),
            source: tool_runtime::source::Kind::Builtin,
            cached: false,
            attempts: 1,
        }
    } else {
        registry.run(tool_runtime::source::shared_cache(), &call.name, args)
    }
}
