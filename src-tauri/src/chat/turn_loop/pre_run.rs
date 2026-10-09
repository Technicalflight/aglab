//! 拓扑调度预跑：相邻安全读并排执行（maxConcurrency=10）。
//!
//! 预跑只接"全绿"的批：每个成员通过与串行主干同款的纯闸（解析/禁用/沙箱边界/
//! 声明校验/权限 Allow/执行前钩子不拦不问），任何一个要问人、被拒、被拦，
//! 整批退回串行主干——调度是增益不是闸门。返回被预跑吃掉的调用 id 集合，
//! 串行主干用它跳过已执行的成员，其余照旧走完整闸链。

use super::{
    mask_tool_input, parse_arguments, tools, AppConfig, ChatEvent, EventSink, Send, ToolCallBuffer,
    ToolStatus,
};
use crate::chat::message_build::{pack_tool_result, tool_result_pair};
use crate::chat::stopped;
use crate::chat::tool_gate::{audit_tool_in, emit_hooks};
use crate::tool_runtime;
use serde_json::{json, Value};

pub(in crate::chat) fn pre_run_parallel(
    tool_calls: &[ToolCallBuffer],
    truncated: bool,
    stop: &std::sync::atomic::AtomicBool,
    config: &AppConfig,
    mcp_servers: &[crate::config::McpServer],
    root: Option<&std::path::Path>,
    bound_root: Option<&std::path::Path>,
    policy: &crate::policy::Policy,
    conversation_id: &str,
    data_dir: &std::path::Path,
    send: &mut Send,
    on_event: &dyn EventSink,
) -> Result<
    (
        std::collections::HashSet<String>,
        Vec<crate::tool_contract::Contract>,
    ),
    String,
> {
    let mut consumed_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    // 本轮每个调用的契约：并行预跑的拓扑与串行主干的输出钳制读同一份
    let round_contracts: Vec<crate::tool_contract::Contract> = tool_calls
        .iter()
        .map(|call| {
            let args = serde_json::from_str::<Value>(&call.arguments).unwrap_or(json!({}));
            crate::tool_contract::contract_for(&call.name, &args)
        })
        .collect();
    if !truncated && !tool_calls.is_empty() {
        'slots: for slot in crate::tool_scheduler::plan_round(&round_contracts) {
            let indexes = match slot {
                crate::tool_scheduler::Slot::Parallel(indexes) if indexes.len() >= 2 => indexes,
                _ => continue,
            };
            if stopped(stop) {
                break;
            }
            // 预检：与串行主干同款的纯闸，逐成员过；任何一个不过就整批放弃
            struct Member<'a> {
                call: &'a ToolCallBuffer,
                args: Value,
                risk: tools::Risk,
                input: String,
            }
            let mut members: Vec<Member> = Vec::with_capacity(indexes.len());
            for index in &indexes {
                let call = &tool_calls[*index];
                let args = match parse_arguments(&call.arguments) {
                    Ok(value) => value,
                    Err(_) => continue 'slots,
                };
                if tools::is_disabled(&config.disabled_tools, &call.name) {
                    continue 'slots;
                }
                let via_mcp = crate::mcp::owns(mcp_servers, &call.name);
                if via_mcp {
                    // 扩展调用走不了内置执行体，批里出现即退串行
                    continue 'slots;
                }
                let scope = tool_runtime::Call::new(&call.name, &args, root, false);
                if crate::tool_runtime::sandbox::enabled()
                    && crate::tool_runtime::sandbox::boundary_violation(
                        &call.name, &args, bound_root, root,
                    )
                    .is_some()
                {
                    continue 'slots;
                }
                if tool_runtime::check_arguments(&scope).is_err() {
                    continue 'slots;
                }
                let risk = tools::classify(&call.name, &args, root);
                if !matches!(risk, tools::Risk::Safe) {
                    // 契约说可并行、classify 却给了更高档：以闸为准，退串行
                    continue 'slots;
                }
                let input = mask_tool_input(false, &call.name, &args);
                let ruling = tool_runtime::rule(
                    policy,
                    &scope,
                    &input,
                    tool_runtime::allowlist(conversation_id).as_deref(),
                );
                if !matches!(ruling.decision, crate::policy::Decision::Allow) {
                    continue 'slots;
                }
                // 执行前钩子：拦或问都退串行（串行主干对被拦的成员有完整的
                // 拒绝回填，预跑不重复那份语义）
                let hook_report = crate::hooks::fire(
                    &crate::hooks::runnable_in(config, data_dir),
                    "PreToolUse",
                    root,
                    |hook, cwd| {
                        json!({
                            "hook_event_name": hook.event,
                            "cwd": cwd.display().to_string(),
                            "model": config.model,
                            "tool_name": call.name,
                            "tool_input": &args,
                        })
                    },
                );
                emit_hooks(on_event, &hook_report);
                if hook_report.blocked().is_some() || hook_report.asks().is_some() {
                    continue 'slots;
                }
                // 审计与串行同一格：放行记录在 Running 之前
                if let Err(error) = audit_tool_in(
                    data_dir,
                    conversation_id,
                    &scope,
                    crate::audit::Outcome::Ok,
                    None,
                ) {
                    eprintln!("并行批成员审计写不进去，整批退串行：{error}");
                    continue 'slots;
                }
                members.push(Member {
                    call,
                    args,
                    risk,
                    input,
                });
            }
            // Running 事件按原顺序发，卡片位置与串行一致
            for member in &members {
                on_event.send(ChatEvent::Tool {
                    id: member.call.id.clone(),
                    name: member.call.name.clone(),
                    status: ToolStatus::Running,
                    risk: member.risk.as_str().into(),
                    input: member.input.clone(),
                    output: None,
                    arguments: Some(member.call.arguments.clone()),
                    pass_reason: None,
                    content_chars: Some(member.call.content_chars),
                });
            }
            // 并行执行：批大小 ≤ MAX_CONCURRENCY，execute_for 是纯内置执行体
            // 契约的 max_output_bytes 在这里生效：与全局钳制取小者
            let caps: Vec<usize> = indexes
                .iter()
                .map(|index| {
                    crate::tool_contract::effective_cap(
                        round_contracts[*index].max_output_bytes,
                        config.tool_result_max_chars,
                    )
                })
                .collect();
            let outputs: Vec<Result<String, String>> = std::thread::scope(|scope| {
                let handles: Vec<_> = members
                    .iter()
                    .map(|member| {
                        let name = member.call.name.clone();
                        let args = member.args.clone();
                        scope.spawn(move || tools::execute_for(&name, &args, root, None))
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|handle| {
                        handle
                            .join()
                            .unwrap_or_else(|_| Err("并行工具线程崩了。".into()))
                    })
                    .collect()
            });
            // 后账按原顺序逐成员走：PostToolUse 钩子 → 归档 → 打包 → 标注 → Done → push
            for (member, (output, tool_result_max)) in
                members.iter().zip(outputs.into_iter().zip(caps))
            {
                match output {
                    Ok(text) => {
                        let report = crate::hooks::fire(
                            &crate::hooks::runnable_in(config, data_dir),
                            "PostToolUse",
                            root,
                            |hook, cwd| {
                                json!({
                                    "hook_event_name": hook.event,
                                    "cwd": cwd.display().to_string(),
                                    "model": config.model,
                                    "tool_name": member.call.name,
                                    "tool_input": &member.args,
                                    "tool_response": &text,
                                })
                            },
                        );
                        emit_hooks(on_event, &report);
                        let content = match report.context() {
                            Some(extra) => format!("{text}\n\n{extra}"),
                            None => text,
                        };
                        if content.chars().count() > tool_result_max {
                            crate::observations::archive(&member.call.id, &content);
                        }
                        let content =
                            pack_tool_result(&content, tool_result_max, Some(&member.call.id));
                        let content = tool_runtime::annotate(
                            tool_runtime::source::Kind::Builtin,
                            &member.call.name,
                            content,
                        );
                        let (event, message) = tool_result_pair(
                            member.call,
                            ToolStatus::Done,
                            member.risk.as_str(),
                            member.input.clone(),
                            content,
                            None,
                        );
                        on_event.send(event);
                        send.push(message)?;
                    }
                    Err(error) => {
                        let failed_scope =
                            tool_runtime::Call::new(&member.call.name, &member.args, root, false);
                        let _ = audit_tool_in(
                            data_dir,
                            conversation_id,
                            &failed_scope,
                            crate::audit::Outcome::Failed,
                            None,
                        );
                        // PostToolUseFailure（并行后账）：与串行同一套形状，
                        // deny 只能转达，拦不回已经发生过的失败
                        {
                            let hooks = crate::hooks::runnable_in(config, data_dir);
                            if !hooks.is_empty() {
                                let report = crate::hooks::fire(
                                    &hooks,
                                    "PostToolUseFailure",
                                    root,
                                    |hook, cwd| {
                                        json!({
                                            "hook_event_name": hook.event,
                                            "cwd": cwd.display().to_string(),
                                            "model": config.model,
                                            "tool_name": member.call.name,
                                                "tool_input": &member.args,
                                                "error": error.to_string(),
                                        })
                                    },
                                );
                                emit_hooks(on_event, &report);
                            }
                        }
                        let (event, message) = tool_result_pair(
                            member.call,
                            ToolStatus::Failed,
                            member.risk.as_str(),
                            member.input.clone(),
                            format!("执行失败：{error}"),
                            None,
                        );
                        on_event.send(event);
                        send.push(message)?;
                    }
                }
                consumed_ids.insert(member.call.id.clone());
            }
        }
    }
    Ok((consumed_ids, round_contracts))
}
