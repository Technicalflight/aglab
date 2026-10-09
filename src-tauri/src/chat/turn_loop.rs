//! 一轮对话的正文循环（优化路线 O1-6 从 chat.rs 拆出的最大件）。
//!
//! [`turn_body`] 跑"一次请求-响应-工具执行"的完整回合：读数投影 → wire 请求 →
//! 流式翻译 → 闸链审批 → 派发执行 → 收尾落账。所有新行都只能通过 `send` 进
//! 日志——这里没有"第二份历史"可漂移。
//!
//! 目标续跑的循环（goal 多轮）不在这一层：那住在 [`super::mode_goal`] 与
//! 界面路径里，turn_body 只跑一发。

use super::message_build::{pack_tool_result, settle_failed, tool_result_pair};
use super::mode_goal::{interrupted_at_boundary, Next};
use super::tool_gate::{
    audit_tool_in, emit_hooks, mask_tool_input, park_unattended, parse_arguments, short_label,
    Escalated,
};
use super::{
    auto_review_verdict, calibrated_baseline, chars_of, close_turn, compaction_boundary, config,
    conversation_project_in, is_retryable, request_round, sizing_of, stopped, summarize_history_in,
    tokens_of, tokens_of_chars, tool_dispatch, tool_runtime, tools, AppConfig, ApprovalHub,
    ChatEvent, DeltaCoalescer, EventSink, Send, SteeringHub, ToolCallBuffer, ToolStatus, TurnHost,
    APPROVAL_TIMEOUT, AUTO_REVIEW_PASS, KEEP_RECENT_CHARS, READ_ONLY_PASS, SESSION_RULE_PASS,
    STANDING_GRANT_PASS,
};
use crate::session::entry::{Message, SettledAssistant, StopReason, ToolCall};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

/// worker 进程里的一发真回合（turn.start 的执行体）。与 Main 的 spawn_send_turn
/// 同源（run_turn），但只跑一发：goal 续跑循环与跟随队列留在 Main 那条路上，
/// 所以挂目标的话题不会被分流到这里（chat_send 门口的 goal_held 闸）。
/// 错误以 (code, message) 返回，由调用方落成 err 信封
/// 一轮对话的正文。所有新行都只能通过 `send` 进日志，所以这里没有"第二份历史"可漂移
#[allow(clippy::too_many_arguments)]
pub(in crate::chat) fn turn_body(
    host: &TurnHost,
    config: &AppConfig,
    // 用户这一发**要的**模型名（run_turn 转交）：模型对账的 requested 格
    requested_model: &str,
    hub: &ApprovalHub,
    mcp_hub: &crate::mcp::Hub,
    mcp_servers: &[crate::config::McpServer],
    stop: &std::sync::atomic::AtomicBool,
    steering: &SteeringHub,
    warm: &crate::warm::Hub,
    has_skills: bool,
    // goal 续跑轮标志（run_turn 转交）：压缩的预防线只在这种步骤边界上放宽
    continuing_goal: bool,
    send: &mut Send,
    rounds_cap: Option<u32>,
    // 这一发是不是坐在那个会自己接下一轮的循环里。只有界面那条路是 `true`：
    // 定时任务、编排节点、子助理都是"一发一件事"，没人接着跑，也就不能告诉界面"还有下一轮"
    auto_continue: bool,
    conversation_id: &str,
    on_event: &dyn EventSink,
) -> Result<Next, String> {
    // 目录锚点：Main 从 app 派生（TurnHost::from_app），worker 由 CLI 传下
    let config_dir = host.config_dir.clone();
    let data_dir = host.data_dir.clone();
    if config.base_url.trim().is_empty() {
        return Err("尚未配置推理服务商地址，请在设置里填写 base URL。".into());
    }
    if config.model.trim().is_empty() {
        return Err("尚未选择模型。".into());
    }

    let key = config::api_key(config)?;
    // Copilot 的 keyring 里存的是 ghu_ 主令牌，不是直接可用的密钥：
    // base_url 指到 Copilot 网关的档案，发请求前在这里换成短期 token（自动缓存换发）
    let key = if config.base_url.contains("api.githubcopilot.com") {
        crate::oauth::copilot_access_token(&key, &config.proxy_default)?
    } else {
        key
    };
    // 本回合生效的配置：max_tokens 会在压缩/钳制后按剩余窗口调低，其余字段原样
    let mut turn_config = config.clone();
    // 话题自己绑的项目优先于应用级激活项目（conversation_project 的判定与项目卡
    // 同源）：侧栏按话题归属分组、输入框旁的选择器显示的也是话题归属，后端的根
    // 目录与权限必须说同一句话。否则"先建话题、再切走/解绑激活项目"的人回到老
    // 话题发消息，文件工具就落到全局默认甚至主目录去了（2026-10-03 用户实测：
    // 话题挂在新建文件夹下，工具却钻进了 skills 项目）。
    let effective_project =
        conversation_project_in(config, &config_dir, &data_dir, conversation_id)
            .or_else(|| config.active_project());
    let conversation_root = effective_project.map(|project| PathBuf::from(project.path.clone()));
    // 话题挂在 Worktree 上时，工作目录就是那棵树：文件工具、权限判定、编辑快照、
    // 钩子的 cwd 全部从这一个变量派生，一处替换即全链路生效。没挂就落话题的项目，
    // 再落激活项目；连工作目录都没绑也回落用户主目录——文件/命令工具不再以
    // "绑定工作目录"为门槛，相对路径相对主目录解析，权限表照常把关
    // （effective_root 的文档在那里）
    let root: Option<PathBuf> = crate::worktree::root_for_in(&data_dir, conversation_id)
        .or_else(|| conversation_root.clone())
        .or_else(|| config.effective_root());
    // 沙箱边界用的"绑定根"：不带主目录回退的那一份。主目录是文件工具的解析基准，
    // 不是沙箱的授权范围——授权范围与命令的可写范围必须一致（对齐 Codex 单一边界）
    let bound_root: Option<PathBuf> =
        crate::worktree::root_for_in(&data_dir, conversation_id).or(conversation_root);
    // 权限表是纯数据：一个回合算一份，别在每个工具调用里把配置文件重新解析一遍。
    // 全局档读的是 config.permission 那三个旧字符串，认不出来的一律按最严的 ask 处理；
    // 覆盖项有全局与项目两层，合并规则（只能更严）在 `config::AppConfig::policy` 那一个入口里。
    // 项目层取的是**话题生效的项目**（与根目录同一个判定）：项目覆盖项跟着"文件落在哪"走，
    // 跟分叉测试钉住的"根目录与权限都跟着项目走"是同一句话。话题作用域上要是还挂着
    // 更严的那一张（编排档案、任务设置），它优先——Policy::resolve 取的是"覆盖项与
    // 档位里更严的那一条"，所以这张表只能更严不能更松
    let global_policy = config.policy(effective_project);
    // 作业模式打在这张表上：它不动档位也不动覆盖项，只往上加一条红线（`rule` 里那一处）。
    // 读的仍是日志而不是前端送下来的那份状态——这一支的模式只有一个真相，
    // 而"切成规划模式"绝不能解开另一条话题的红线
    let mode = crate::session::mode::in_effect(&send.opened.log);
    let policy = tool_runtime::policy_for(conversation_id, &global_policy).with_phase(mode.phase());

    // 插件与工作区钩子在**每次发射前**重新解析（runnable）：定义文件一改指纹
    // 就失配，信任一撤就停——四个触发点各自取新鲜的那一份，不用这一条旧账。
    let mut asked_to_continue = false;

    // 历史不再从这里"反推"：`send` 里的就是日志投影，模型见过的字节和将发出去的字节
    // 是同一份东西。以前这段靠前端送来的台账重放，于是每轮在形状、文案、顺序三处各失真一次

    // 提交前钩子：脚本可以往这一轮的上下文里补几句项目约定。
    // 这个事件上纯文本 stdout 就算上下文，和其他事件"读不懂就不算意见"的规矩不同。
    // 守卫与发射共用一份发射前重解析（信任/指纹/撤销的即时性在这里）
    let hooks = crate::hooks::runnable_in(config, &data_dir);
    if !hooks.is_empty() {
        let prompt = send
            .history()
            .iter()
            .rev()
            .find(|message| message["role"] == "user")
            .and_then(|message| message["content"].as_str())
            .unwrap_or_default()
            .to_string();

        let report =
            crate::hooks::fire(&hooks, "UserPromptSubmit", root.as_deref(), |hook, cwd| {
                json!({
                    "hook_event_name": hook.event,
                    "cwd": cwd.display().to_string(),
                    "model": config.model,
                    "prompt": prompt,
                })
            });
        emit_hooks(on_event, &report);

        if let Some(context) = report.context() {
            // 钩子补的话也是一条历史：它必须进日志，否则下一轮模型不知道有人替它补过规矩
            send.push(Message::System { content: context })?;
        }
    }

    // 工具声明在整回合内不变（技能/项目/MCP 连接在回合中途不会重新协商），
    // 构建一次提到循环外：schemas 序列化与 MCP hub 锁不必每轮重付，
    // 更重要的是声明字节序列逐字稳定，服务商的 prompt cache 才能命中前缀。
    // 这批只是**候选**：真正发出去的是下面按首轮定形的那份
    let candidates = tool_runtime::source::declarations(
        root.is_some(),
        &config.disabled_tools,
        has_skills,
        config.browser_control_enabled,
        config.web_search.enabled(),
        crate::mcp::schemas(mcp_servers, config, mcp_hub),
        &crate::spawn::spawnable_catalog(config),
        tool_runtime::allowlist(conversation_id).as_deref(),
    )
    .ordered();
    let declared = send.declarations(candidates)?;

    // 固定开销 = 常驻段（只有默认提示词）+ 这一轮真正发出去的声明。段（约定 / 技能清单）
    // 现在在日志里，所以它算历史那一本账。常驻段不在日志投影里，压缩估算必须自己把它
    // 算进去，漏掉就会低估上下文、压得太晚
    let fixed_chars = send.standing().iter().map(chars_of).sum::<usize>()
        + declared.iter().map(chars_of).sum::<usize>();
    let history_chars_of = |messages: &[Value]| -> usize {
        messages
            .iter()
            // 带图的行 content 是数组：`as_str()` 会读成 0，于是压缩以为这一行很轻，
            // 压得太晚直接爆窗口。图片按 base64 后的量记账
            .map(crate::session::entry::content_chars)
            .sum()
    };

    // 上下文自动压缩（借鉴 NVlabs/SoL-Pi 的 Online Context Compact）：
    // 发送前按层的预算表算一次账，只有预算表点名要历史这一层付账时才把更早的对话压成摘要——
    // 摘要里必须保住"已完成的工作、验证结果、重要决策、剩余任务"，
    // 最近一段原文原样保留，这样模型拿到手就能接着干，而不是从零猜起。
    //
    // 判定口径分两级：本话题上一轮有服务商真实 prompt_tokens 时优先用它
    // （真实值天然涵盖工具声明与消息结构的所有细节，比字符估算准得多），
    // 折成字符时乘**上界**（宁可多算已用量）；首轮没有真实值才整体退回本地估算。
    let calibration = crate::usage::calibration_for_in(&config_dir, &config.model);
    if config.auto_compact && send.history().len() >= 4 {
        let real_baseline =
            crate::usage::last_prompt_tokens_for_in(&config_dir, conversation_id).unwrap_or(0);
        let tail_chars = send
            .history()
            .last()
            .and_then(|message| message["content"].as_str())
            .map(|text| text.chars().count())
            .unwrap_or(0);
        // 什么时候压：不再是"总量过了窗口的九成"，而是预算表点名要历史这一层付账。
        // 窗口先减掉本来就要留给输出的那截——旧的 `* 0.9` 想说的就是这个数，
        // 而它明写在配置里（config.max_tokens），不该用一个写死的比例去猜
        let sizing = sizing_of(config, calibration.as_ref());
        let uses = crate::session::layers::uses(&send.opened.log, send.standing())
            .map_err(|error| error.to_string())?;
        let table = crate::session::layers::budget(&uses, sizing);
        let estimate = if real_baseline > 0 {
            crate::session::layers::Estimate {
                // 服务商报的是 token，这张表量的是字符：不换算就等于把 3 万 token 当成 3 万字符
                chars: calibrated_baseline(real_baseline, calibration.as_ref()) + tail_chars,
                kind: crate::session::layers::EstimateKind::Calibrated,
            }
        } else {
            crate::session::layers::estimate(&uses)
        };
        let plan = crate::session::layers::plan(estimate, &table, None);
        // microcompact 先于整段压缩（O5-1/O5-2）：阶梯点了「清旧工具结果」就走读侧
        // 变换——不花摘要请求、不动日志。省下 ≥256 token **且**清完装得进预算，
        // 这一发就用清过的 wire 发；两项有一项不满足，照旧走下面的整段压缩
        let mut microcompacted = false;
        if plan
            .ladder
            .contains(&crate::session::layers::Concession::ClearStaleToolResults)
        {
            let (_cleared, saved_chars) =
                crate::session::context::clear_stale_tool_results(send.history());
            let saved_tokens = tokens_of_chars(saved_chars, calibration.as_ref()) as usize;
            let post_plan = crate::session::layers::plan(
                crate::session::layers::Estimate {
                    chars: estimate.chars.saturating_sub(saved_chars),
                    kind: estimate.kind,
                },
                &table,
                None,
            );
            let fits = !matches!(
                post_plan.reason,
                crate::session::layers::BreakReason::OverBudget { .. }
            );
            if saved_tokens >= crate::session::context::MICROCOMPACT_MIN_SAVED_TOKENS && fits {
                send.microcompact = true;
                send.refresh()?;
                microcompacted = true;
                // 贴附提示不进正文：清了几条、省了多少，跟"压缩推迟"同一档的读数
                on_event.send(ChatEvent::Retry {
                    text: format!(
                        "microcompact：清掉旧工具结果，这一发省下约 {saved_tokens} token。"
                    ),
                    reason: "上下文让步阶梯：先清旧工具结果（读侧变换），日志一行不动。".into(),
                });
            }
        }
        let owed = !microcompacted
            && (plan
                .ladder
                .contains(&crate::session::layers::Concession::CompactHistory)
            // 预防线（SoL-Pi 的步骤边界思想）：目标/计划续跑的轮次是语义干净的
            // 步骤边界——历史层用到硬顶七成就在这里提前压，别等逼近上限时
            // 在任务中间压
            || (continuing_goal
                && table
                    .row(crate::session::layers::Layer::History)
                    .is_some_and(|row| row.chars * 10 >= row.max * 7)));
        if owed {
            // 缓存重写成本项（SoL-Pi 的 cacheWriteReadRatio 精神）：压缩必然作废
            // 粘住成员身上的热前缀缓存，下一发是全价重写。缓存还热、没有更深的
            // 让步点名、且总量仍在窗口容量内（晚一发压不会 400）时，推迟一次——
            // 缓存冷了或更逼近上限时，这里的判定自然放行
            let cache_hot = crate::pool::cache_hot_for(conversation_id);
            let deeper = plan.ladder.iter().any(|step| {
                matches!(
                    step,
                    crate::session::layers::Concession::DropMemorySection
                        | crate::session::layers::Concession::TrimSkills
                )
            });
            let window_chars = (config.context_tokens.saturating_sub(config.max_tokens)) as f64
                * crate::usage::budget_ratio(calibration.as_ref());
            let slack = (estimate.chars as f64) < window_chars * 0.98;
            if cache_hot && !deeper && slack {
                // 只弹贴附提示不进正文：推迟的压缩不是本轮的失败
                on_event.send(ChatEvent::Retry {
                    text: "压缩推迟：当前成员的缓存还热，压一次等于整段重写。".into(),
                    reason: "上下文逼近预算上限，缓存转冷或更逼近上限时会自动压缩。".into(),
                });
            } else {
                on_event.send(ChatEvent::Compaction {
                    phase: "start".into(),
                    summary: None,
                    kept: None,
                });
                let history = send.history().to_vec();
                match summarize_history_in(&config_dir, &data_dir, config, &history) {
                    Ok(summary) => {
                        // 压缩写成一条条目，而不是就地改写一个数组：改写的版本下一轮就没了，
                        // 界面上的条数和模型看到的条数还会各说各话（旧设计里 `kept` 口径不一致
                        // 就是这么来的）。条目进日志之后，"压过了"这个事实本身也是历史的一部分
                        let boundary = send.provenance().ok().and_then(|origin| {
                            compaction_boundary(&history, &origin, KEEP_RECENT_CHARS)
                        });
                        match boundary {
                            Some((first_kept_entry_id, kept)) => {
                                send.append(crate::session::entry::EntryPayload::Compaction {
                                    summary: summary.clone(),
                                    first_kept_entry_id,
                                    tokens_before: crate::session::layers::thread_chars(
                                        send.standing(),
                                        &history,
                                    ),
                                    usage: None,
                                    system_message: send.section_snapshot(),
                                })?;
                                on_event.send(ChatEvent::Compaction {
                                    phase: "done".into(),
                                    summary: Some(summary),
                                    kept: Some(kept),
                                });

                                // 压缩后复验走同一张预算表。真实 baseline 是压缩前的旧值，
                                // 不能再拿它判定，否则会误判"仍超窗"而连环压缩
                                let post_uses =
                                    crate::session::layers::uses(&send.opened.log, send.standing())
                                        .unwrap_or_default();
                                let post_plan = crate::session::layers::plan(
                                    crate::session::layers::estimate(&post_uses),
                                    &crate::session::layers::budget(&post_uses, sizing),
                                    // 这里不报"压缩授权过的那次断开"：本轮要看的是还装不装得下
                                    None,
                                );
                                if matches!(
                                    post_plan.reason,
                                    crate::session::layers::BreakReason::OverBudget { .. }
                                ) {
                                    on_event.send(ChatEvent::Notice {
                                    text: "压缩后上下文仍接近窗口上限，建议调大「上下文窗口」配置或减少保留长度。"
                                        .into(),
                                });
                                }
                            }
                            None => {
                                on_event.send(ChatEvent::Notice {
                                    text:
                                        "上下文接近窗口上限，但可压缩的对话太少，本轮按原样发送。"
                                            .into(),
                                });
                            }
                        }
                    }
                    Err(error) => {
                        // 压缩失败不拦路：降级按原样发送，爆窗口是服务商的事，摘要挂了不该把整轮拖死
                        eprintln!("上下文压缩失败，按原样发送：{error}");
                        on_event.send(ChatEvent::Notice {
                            text: format!("上下文压缩失败（{error}），本轮按原样发送。"),
                        });
                    }
                }
            }
        }
    }

    // 输出预算钳制（压缩之后算，用的才是最终上下文）：
    // max_tokens 设得比"剩余窗口"还大时，有的服务商直接 400，
    // 有的会把输入截一半。钳到剩余空间，至少 1K 保底
    {
        let estimate = fixed_chars + history_chars_of(send.history());
        // 字符折回 token 时**除以下界**：同一条换算，方向上宁可少算空位，
        // 也不要报出一个"还剩 9 万"而服务商其实接不住（§15）
        let remaining = config
            .context_tokens
            .saturating_sub(tokens_of_chars(estimate, calibration.as_ref()));
        if config.max_tokens > remaining {
            turn_config.max_tokens = remaining.max(1024);
        }
    }

    let started = Instant::now();
    let mut input_tokens = 0u32;
    let mut output_tokens = 0u32;
    // 命中缓存的输入取各轮最大值：每轮的 prompt 都是"上轮全部+新增"，
    // 最大值就是最近一轮的真实命中量（与 input_tokens 的累计口径一致）。
    // 某轮没上报就跳过它，但不能让"从未上报"退化成 0——那是两件事
    let mut cached_tokens: Option<u32> = None;

    // 轮数上限可配置：0 = 不设上限（这是设置里能写出来的明确决定，"防无限循环烧 token"
    // 那道闸要不要留着由用户说了算）。自带天花板的那次运行（子助理）照旧按它自己的来，
    // 与全局谁大谁小无关
    let max_rounds = match rounds_cap {
        Some(cap) => cap.max(1) as usize,
        None if config.max_tool_rounds == 0 => usize::MAX,
        None => config.max_tool_rounds.max(1) as usize,
    };
    // 交错偏移的累进基数：本轮之前的所有回答按前端拼接的口径占多少个
    // UTF-16 码元（多条回答在投影里用空行缝成一条消息，缝占 2 个码元）
    let mut content_chars_so_far: usize = 0;
    for _round in 0..max_rounds {
        // 停止检查点 1：轮与轮之间。流式读取中的停止在 read_events 逐行检查
        if stopped(stop) {
            // 回合就此打住：挂着的那发回合内保温没了下一发请求可等，撤掉。
            // 这里不再发停止通知：前端按停止时已弹过"已请求停止"的提示，
            // 半截正文气泡本身也停在原地——正文里再插一句停止说明是重复打扰
            warm.cancel(conversation_id);
            return close_turn(
                host,
                conversation_id,
                auto_continue,
                interrupted_at_boundary(stop),
                continuing_goal,
                send,
                on_event,
                |entry_ids| ChatEvent::Done {
                    input_tokens,
                    output_tokens,
                    duration_ms: started.elapsed().as_millis() as u64,
                    cached_tokens,
                    entry_ids,
                    model: config.model.clone(),
                    context_tokens: config.context_tokens,
                },
            );
        }
        // 没有活动项目就没有文件根目录，此时不声明内置工具，避免模型往任意路径写。
        // 扩展工具不依赖工作目录，所以单独合并进来。
        // 插话检查点：上一轮工具都跑完了，用户中途说的话在这里进入上下文
        for text in steering.drain(conversation_id) {
            send.push(Message::User {
                content: format!("（执行中途的插话）{text}"),
                images: Vec::new(),
                audios: Vec::new(),
                videos: Vec::new(),
            })?;
        }

        // 请求阶段：无输出的可重试错误（限流/上游/网络）自动退避重试最多 2 次；
        // 一旦有输出（first_token_ms 已记）就不再重试，避免把半截回复重复给模型
        let mut attempt_started;
        let mut first_token_ms: Option<u64>;
        // 记一份"这一轮实际发出去的数组"。放在重试循环之前、每个工具回合一次：
        // 重试发的是同一个数组，重写无害；而最后一条记录正好等于最后一次请求。
        // 只有对话回合走这里——complete_once（标题生成、定时任务）发的不是话题历史，
        // 让它写快照会用一次一次性调用覆盖掉真正的对话
        // 每次请求前落一次盘：进程被杀时最多丢一次请求的内容，
        // 而丢掉的也只是"这一轮没记上"，不会让日志和实发分叉
        // 基线是否断开在发请求之前判：失败那条记账也要用它，而那时再问日志，
        // "日志走不通"会冒充成"服务商报错"返回给调用方
        let chain_reset = send.starts_fresh_chain()?;
        send.save();

        // 下一发真实请求马上就会把缓存续上：上一发待放的保温当场作废。
        // 它要真等到点才被顶掉，放出去的就是一次纯浪费的重放
        warm.cancel(conversation_id);
        let mut attempts: u32 = 0;
        let mut outcome = loop {
            attempt_started = Instant::now();
            first_token_ms = None;
            // 增量事件先攒帧再进界面（DeltaCoalescer）：首 token 计时仍按
            // 攒帧前的真实首个增量算，重试闸门与延迟指标都不受合帧影响
            let mut coalescer = DeltaCoalescer::new();
            // 重复循环护栏（每发重试各一份：重发的流从头算）：增量在进合帧器之前
            // 先过检测器，命中即拉起停止旗标——停止通道是全场最老练的断流路径，
            // 半截正文落定、Notice、close_turn 全是现成的
            let mut repetition_guard = crate::repetition::Guard::new();
            let mut loop_hit = false;
            let mut emit = |event: ChatEvent| {
                if matches!(event, ChatEvent::Delta { .. }) && first_token_ms.is_none() {
                    first_token_ms = Some(attempt_started.elapsed().as_millis() as u64);
                }
                if config.repetition_guard && !loop_hit {
                    match &event {
                        ChatEvent::Delta { text } | ChatEvent::Reasoning { text }
                            if repetition_guard
                                .push(text, matches!(event, ChatEvent::Reasoning { .. })) =>
                        {
                            loop_hit = true;
                            stop.store(true, std::sync::atomic::Ordering::Release);
                        }
                        _ => {}
                    }
                }
                if loop_hit {
                    // 已拉闸：模型还在往连接里吐的循环尾巴不再进界面
                    return;
                }
                coalescer.push(event, &mut |event| {
                    on_event.send(event);
                });
            };
            let result = request_round(
                &turn_config,
                &key,
                send.rows(),
                &declared,
                Some(conversation_id),
                stop,
                &mut emit,
            );
            // 流结束（含失败/停止）先冲帧：攒下的正文必须完整走完事件序，
            // 失败半截的落库与"有没有输出过"的重试判定都排在它后面
            coalescer.flush(&mut |event| {
                on_event.send(event);
            });
            match result {
                Ok(outcome) => break outcome,
                Err(failure) if failure.stopped() => {
                    // 回合就此打住：挂着的那发回合内保温没了下一发请求可等，撤掉
                    warm.cancel(conversation_id);
                    // 用户按了停止：不算失败（不记失败账），但那半截他在界面上读过了，
                    // 必须作为"未写完"的落定行进日志，否则下一轮模型以为自己没说过
                    if let Some(row) = settle_failed(&failure.partial, StopReason::Aborted, None) {
                        send.push(row)?;
                        send.save();
                    }
                    // 两种停法在界面上必须分得开：护栏掐的复读要说清是它拦的、内容
                    // 还在、怎么换答案——这条进正文（ toast 只报了"已请求停止"）。
                    // 人按的普通停止不再发正文通知：toast 已覆盖，半截正文气泡停在原地
                    if loop_hit {
                        on_event.send(ChatEvent::Notice {
                            text: "检测到模型输出陷入重复循环，已自动截断：循环前的内容已保留，后续 token 不再消耗。可用「重新生成」换一支答案。".into(),
                        });
                    }
                    // 同上：流被断也只掐这一轮。那半截已经落进行，下一轮模型看得见它
                    return close_turn(
                        host,
                        conversation_id,
                        auto_continue,
                        interrupted_at_boundary(stop),
                        continuing_goal,
                        send,
                        on_event,
                        |entry_ids| ChatEvent::Done {
                            input_tokens,
                            output_tokens,
                            duration_ms: started.elapsed().as_millis() as u64,
                            cached_tokens,
                            entry_ids,
                            model: config.model.clone(),
                            context_tokens: config.context_tokens,
                        },
                    );
                }
                Err(failure)
                    if attempts < 2
                        && first_token_ms.is_none()
                        && is_retryable(&failure.message) =>
                {
                    attempts += 1;
                    let wait = u64::from(attempts) * 2;
                    on_event.send(ChatEvent::Retry {
                        text: format!("将在 {wait} 秒后自动重试（第 {attempts}/2 次）。"),
                        reason: failure.message.clone(),
                    });
                    thread::sleep(Duration::from_secs(wait));
                }
                Err(failure) => {
                    // 失败也记一行：这个服务商今天挂了几次，只有台账答得了
                    crate::usage::record_turn_in(
                        &config_dir,
                        config,
                        "chat",
                        conversation_id,
                        &config.model,
                        &crate::usage::Tokens::default(),
                        // 断掉的那一发没有可信的实发大小：填 0 它就进不了校准样本
                        0,
                        chain_reset,
                        attempt_started.elapsed().as_millis() as u64,
                        first_token_ms,
                        false,
                        &failure.message,
                    );
                    if let Some(row) =
                        settle_failed(&failure.partial, StopReason::Error, Some(&failure.message))
                    {
                        send.push(row)?;
                        send.save();
                    }
                    // 服务商报错后回合终止：待放的保温没有下一发可等，撤掉
                    warm.cancel(conversation_id);
                    return Err(failure.message);
                }
            }
        };

        crate::usage::record_turn_in(
            &config_dir,
            config,
            "chat",
            conversation_id,
            &turn_config.model,
            &tokens_of(&outcome.usage),
            outcome.sent_chars,
            chain_reset,
            attempt_started.elapsed().as_millis() as u64,
            first_token_ms,
            true,
            "",
        );

        // 模型对账：用户要的（requested）→ 本地映射后实发的（mapped/sent）→
        // 上游自报回家的（response）。四格判定落台账（写库失败静默，对账绝不
        // 打断对话），链路条同步亮一格——替换要让人当场看见
        let mapped = turn_config.model.clone();
        let sent = if outcome.sent_model.is_empty() {
            mapped.clone()
        } else {
            outcome.sent_model.clone()
        };
        let response_model = outcome.response_model.clone();
        let verdict = crate::model_trace::classify(
            requested_model,
            &mapped,
            &sent,
            response_model.as_deref(),
            crate::model_trace::whitelist_of(config),
        );
        {
            let reading = match (&verdict.kind, response_model.as_deref()) {
                (crate::model_trace::MismatchKind::None, Some(name))
                    if verdict.variant_of.is_some() =>
                {
                    format!("{name} · 日期变体")
                }
                (crate::model_trace::MismatchKind::None, _) => format!("{mapped} · 一致"),
                (crate::model_trace::MismatchKind::LocalMapping, _) => {
                    format!("{requested_model} → {mapped} · 本地映射")
                }
                (crate::model_trace::MismatchKind::UpstreamReplaced, Some(name)) => {
                    format!("要 {sent} · 上游回 {name}")
                }
                _ => format!("{sent} · 上游未报"),
            };
            let tone = match verdict.kind {
                crate::model_trace::MismatchKind::None => "ok",
                crate::model_trace::MismatchKind::UpstreamReplaced => "warn",
                crate::model_trace::MismatchKind::LocalMapping
                | crate::model_trace::MismatchKind::Unknown => "info",
            };
            let mut hint = format!("请求 {requested_model} · 实发 {sent}");
            hint.push_str(&match response_model.as_deref() {
                Some(name) => format!(" · 上游 {name}"),
                None => " · 上游未报模型名".to_string(),
            });
            if let Some(variant) = &verdict.variant_of {
                hint.push_str(&format!("（{variant} 为日期后缀变体）"));
            }
            on_event.send(ChatEvent::Probe {
                key: "model".into(),
                detail: reading,
                tone: Some(tone.into()),
                hint: Some(hint),
            });
        }
        let provider_host = tauri::Url::parse(&config.base_url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_string))
            .unwrap_or_default();
        crate::model_trace::record_in(
            &config_dir,
            conversation_id,
            &provider_host,
            crate::model_trace::endpoint_label(config),
            requested_model,
            &mapped,
            &sent,
            response_model,
            verdict.kind,
            verdict.variant_of,
            outcome.response_model_path.clone(),
        );

        // 交错偏移：本轮正文在前端是接在之前几轮后面的（空行缝），工具声明的
        // 位置 = 已累计基数 + 缝 + 本轮正文。口径是 UTF-16 码元——前端 JS 字符串
        // 的下标就是它，Rust 的 chars().count() 在 emoji 上会错一位
        // 缝只在两侧都有正文时才存在（投影合并的同一判据）：纯工具轮没有文字，不占缝
        let seam = if content_chars_so_far > 0 && !outcome.text.is_empty() {
            2
        } else {
            0
        };
        let call_content_chars = content_chars_so_far + seam + outcome.text.encode_utf16().count();
        content_chars_so_far = call_content_chars;
        for call in &mut outcome.tool_calls {
            call.content_chars = call_content_chars as u32;
        }

        if let Some(usage) = &outcome.usage {
            input_tokens = input_tokens.max(usage.input_tokens);
            // Option 的 Ord 把 None 排在任何 Some 之前，所以"某轮没上报"不会
            // 把已取到的命中抹零，而全程没上报仍然保持 None（不是 0）
            cached_tokens = cached_tokens.max(usage.cached_tokens);
            output_tokens += usage.output_tokens;
        }

        // 长工具循环的保温（Streaming 档）：模型刚声明的工具可能一跑好几分钟，
        // 没人发请求的间隙里，服务商缓存的整段前缀就过期了，下一发被迫全价重算。
        // 趁 tip 还停在"这一发的输入"上（工具结果还没追加），把保温排出去——
        // 续接概率按 1 算，因为下一发几乎必然要来；真按了停止，下面的退出路径会撤掉它。
        // 下一发真实请求开始时的 cancel、收尾那发空闲保温的 arm，都会顶掉这一发
        if !outcome.tool_calls.is_empty() {
            host.warm_schedule(
                config,
                warm,
                crate::warm::Plan {
                    conversation_id: conversation_id.to_string(),
                    tip: send.opened.log.leaf_id().map(str::to_string),
                    sent_at: crate::session::now_millis(),
                    prompt_tokens: crate::usage::last_prompt_tokens_for_in(
                        &config_dir,
                        conversation_id,
                    )
                    .unwrap_or(0)
                    .max(0) as u64,
                    delay_ms: 0,
                    ttl_ms: 0,
                    phase: crate::warm::Phase::Streaming,
                    refresh_deadline_ms: 0,
                },
                send.rows().to_vec(),
                declared.clone(),
            );
        }

        if outcome.tool_calls.is_empty() {
            // 收尾钩子有机会说"这轮还没交付完"。一条回合只让它续一次：
            // 每次都拒绝收尾的脚本会把对话挂死，参照实现也是靠这个标志位防循环的
            let hooks = crate::hooks::runnable_in(config, &data_dir);
            if !hooks.is_empty() && !asked_to_continue {
                let report = crate::hooks::fire(&hooks, "Stop", root.as_deref(), |hook, cwd| {
                    json!({
                        "hook_event_name": hook.event,
                        "cwd": cwd.display().to_string(),
                        "model": config.model,
                        "stop_hook_active": asked_to_continue,
                        "last_message": &outcome.text,
                    })
                });
                emit_hooks(on_event, &report);

                if let Some(reason) = report.blocked() {
                    asked_to_continue = true;
                    send.push(Message::Assistant(SettledAssistant {
                        content: outcome.text,
                        tool_calls: Vec::new(),
                        stop: StopReason::Stop,
                        reasoning: outcome.reasoning,
                        error: None,
                        thinking_signature: outcome.reasoning_signature,
                        reasoning_items_json: outcome.reasoning_items_json,
                    }))?;
                    send.push(Message::User {
                        content: format!("插件钩子认为这一轮还没做完，请接着往下：\n{reason}"),
                        images: Vec::new(),
                        audios: Vec::new(),
                        videos: Vec::new(),
                    })?;
                    continue;
                }
            }

            // 模型本想说"我做完了"，但用户中途插了话——继续一轮让它回应插话，
            // 而不是把插话晾到回合结束（pi 的 steering 语义：插话改变 agent 的走向）
            let pending_steering = steering.drain(conversation_id);
            if !pending_steering.is_empty() {
                // 不发这条的话，回应会无缝接在上一段答案后面——用户看不出
                // 模型回应了插话，只会觉得"发了没反应"
                on_event.send(ChatEvent::Notice {
                    text: "已收到你的插话，接着往下回应。".into(),
                });
                send.push(Message::Assistant(SettledAssistant {
                    content: outcome.text,
                    tool_calls: Vec::new(),
                    stop: StopReason::Stop,
                    reasoning: outcome.reasoning,
                    error: None,
                    thinking_signature: outcome.reasoning_signature,
                    reasoning_items_json: outcome.reasoning_items_json,
                }))?;
                for text in pending_steering {
                    send.push(Message::User {
                        content: format!("（执行中途的插话）{text}"),
                        images: Vec::new(),
                        audios: Vec::new(),
                        videos: Vec::new(),
                    })?;
                }
                continue;
            }

            // 模型这轮最后说的那句必须进快照：它是"回答"，天然不在任何一次请求数组里，
            // 但下一轮它就是历史。少了这一步，切换读路径后模型会忘掉自己刚回答的内容
            if !outcome.text.is_empty() {
                send.push(Message::Assistant(SettledAssistant {
                    content: outcome.text,
                    tool_calls: Vec::new(),
                    stop: StopReason::Stop,
                    reasoning: outcome.reasoning,
                    error: None,
                    thinking_signature: outcome.reasoning_signature,
                    reasoning_items_json: outcome.reasoning_items_json,
                }))?;
            }

            // 收尾这一轮：落排队里的切档、算续跑判据、按"读数先于 Done"发出去、落盘。
            // 四条断旗的路走的是同一个出口，见 `close_turn`
            let next = close_turn(
                host,
                conversation_id,
                auto_continue,
                interrupted_at_boundary(stop),
                continuing_goal,
                send,
                on_event,
                |entry_ids| ChatEvent::Done {
                    input_tokens,
                    output_tokens,
                    duration_ms: started.elapsed().as_millis() as u64,
                    cached_tokens,
                    entry_ids,
                    model: config.model.clone(),
                    context_tokens: config.context_tokens,
                },
            )?;
            // 保温要续的是"这一发刚刚写进服务商缓存的那条前缀"，所以排在这里而不是下一轮之前。
            // 落盘已经在 `close_turn` 里做完了：到点时保温线程是从磁盘重开日志核对末端的，
            // 没落盘的末端会让它误判"前缀变了"而把这一发作废
            host.warm_schedule(
                config,
                warm,
                crate::warm::Plan {
                    conversation_id: conversation_id.to_string(),
                    tip: send.opened.log.leaf_id().map(str::to_string),
                    sent_at: crate::session::now_millis(),
                    prompt_tokens: crate::usage::last_prompt_tokens_for_in(
                        &config_dir,
                        conversation_id,
                    )
                    .unwrap_or(0)
                    .max(0) as u64,
                    delay_ms: 0,
                    ttl_ms: 0,
                    phase: crate::warm::Phase::Idle,
                    refresh_deadline_ms: 0,
                },
                send.rows().to_vec(),
                declared.clone(),
            );
            return Ok(next);
        }

        // 带调用的那条只装它自己的正文；收尾回答是**另一条**条目。以前两条被并进同一条，
        // 回放出来就是"先回答、后收到工具结果"的倒置因果（F13）
        let calls: Vec<ToolCall> = outcome
            .tool_calls
            .iter()
            .map(|call| ToolCall {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
                content_chars: Some(call.content_chars),
            })
            .collect();

        send.push(Message::Assistant(SettledAssistant {
            content: outcome.text,
            tool_calls: calls,
            // 被输出上限截断的那次"调用"参数可能只有一半，标成 length 而不是 tool_use
            stop: if outcome.truncated {
                StopReason::Length
            } else {
                StopReason::ToolUse
            },
            // 中间轮次的思考也要落库：它是对话历史的一部分（responses/anthropic
            // 的回放凭据就在这里），界面上读得到，下一轮模型也看得到
            reasoning: outcome.reasoning,
            error: None,
            thinking_signature: outcome.reasoning_signature,
            reasoning_items_json: outcome.reasoning_items_json,
        }))?;

        // 输出被 token 上限截断时，流式拼出来的工具参数可能只传了一半——
        // 这样的调用执行了比不执行更危险（读错文件、删错目录）。
        // 全部按失败回传，让模型拿着完整意图重发。参照 pi 的 failToolCallsFromTruncatedMessage。
        if outcome.truncated {
            for call in &outcome.tool_calls {
                let (event, message) = tool_result_pair(
                    call,
                    ToolStatus::Failed,
                    tools::Risk::High.as_str(),
                    call.name.clone(),
                    "助手消息被输出长度上限截断，这次工具调用的参数可能被切断，因此没有执行。请重新发出参数完整的调用。"
                        .into(),
                    // 还没走到闸门：谈不上"跳过询问"
                    None,
                );
                on_event.send(event);
                send.push(message)?;
            }
            continue;
        }

        // ---- 拓扑调度预跑：相邻安全读并排执行（maxConcurrency=10）----
        //
        // 预跑只接"全绿"的批：每个成员都要通过与串行主干同款的纯闸（解析/禁用/
        // 沙箱边界/声明校验/权限 Allow/执行前钩子不拦不问），任何一个成员要问人、
        // 被拒、被拦，整批退回串行主干——调度是增益不是闸门。结果按原顺序走与
        // 串行完全相同的后账（PostToolUse 钩子/归档/事件/推送），界面看到的
        // 顺序与串行一致：批内并行的是执行，不是回填
        let mut consumed_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        // 本轮每个调用的契约：并行预跑的拓扑与串行主干的输出钳制读同一份
        let round_contracts: Vec<crate::tool_contract::Contract> = outcome
            .tool_calls
            .iter()
            .map(|call| {
                let args = serde_json::from_str::<Value>(&call.arguments).unwrap_or(json!({}));
                crate::tool_contract::contract_for(&call.name, &args)
            })
            .collect();
        if !outcome.truncated && !outcome.tool_calls.is_empty() {
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
                    let call = &outcome.tool_calls[*index];
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
                    let scope = tool_runtime::Call::new(&call.name, &args, root.as_deref(), false);
                    if crate::tool_runtime::sandbox::enabled()
                        && crate::tool_runtime::sandbox::boundary_violation(
                            &call.name,
                            &args,
                            bound_root.as_deref(),
                            root.as_deref(),
                        )
                        .is_some()
                    {
                        continue 'slots;
                    }
                    if tool_runtime::check_arguments(&scope).is_err() {
                        continue 'slots;
                    }
                    let risk = tools::classify(&call.name, &args, root.as_deref());
                    if !matches!(risk, tools::Risk::Safe) {
                        // 契约说可并行、classify 却给了更高档：以闸为准，退串行
                        continue 'slots;
                    }
                    let input = mask_tool_input(false, &call.name, &args);
                    let ruling = tool_runtime::rule(
                        &policy,
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
                        &crate::hooks::runnable_in(config, &data_dir),
                        "PreToolUse",
                        root.as_deref(),
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
                        &data_dir,
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
                            let root = root.clone();
                            let name = member.call.name.clone();
                            let args = member.args.clone();
                            scope.spawn(move || {
                                tools::execute_for(&name, &args, root.as_deref(), None)
                            })
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
                                &crate::hooks::runnable_in(config, &data_dir),
                                "PostToolUse",
                                root.as_deref(),
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
                            let failed_scope = tool_runtime::Call::new(
                                &member.call.name,
                                &member.args,
                                root.as_deref(),
                                false,
                            );
                            let _ = audit_tool_in(
                                &data_dir,
                                conversation_id,
                                &failed_scope,
                                crate::audit::Outcome::Failed,
                                None,
                            );
                            // PostToolUseFailure（并行后账）：与串行同一套形状，
                            // deny 只能转达，拦不回已经发生过的失败
                            {
                                let hooks = crate::hooks::runnable_in(config, &data_dir);
                                if !hooks.is_empty() {
                                    let report = crate::hooks::fire(
                                        &hooks,
                                        "PostToolUseFailure",
                                        root.as_deref(),
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

        for (call_index, call) in outcome.tool_calls.iter().enumerate() {
            if consumed_ids.contains(&call.id) {
                continue;
            }
            let args = match parse_arguments(&call.arguments) {
                Ok(value) => value,
                Err(error) => {
                    // 参数不是合法 JSON 时宁可不执行：空参数硬跑是对着猜的意图动文件
                    let (event, message) = tool_result_pair(
                        call,
                        ToolStatus::Failed,
                        tools::Risk::High.as_str(),
                        call.name.clone(),
                        format!("工具调用参数解析失败，没有执行：{error}"),
                        None,
                    );
                    on_event.send(event);
                    send.push(message)?;
                    continue;
                }
            };

            // 被关掉的能力即使被硬调回来也不执行，但必须给出工具结果，否则这条 tool_call 悬空
            if tools::is_disabled(&config.disabled_tools, &call.name) {
                let (event, message) = tool_result_pair(
                    call,
                    ToolStatus::Denied,
                    tools::Risk::High.as_str(),
                    tools::summary(&call.name, &args),
                    tools::DISABLED_NOTE.into(),
                    None,
                );
                on_event.send(event);
                send.push(message)?;
                continue;
            }

            // 扩展跑的是别人的程序，看不到它会做什么，所以一律按高风险处理
            if stopped(stop) {
                // 回合就此打住：挂着的那发回合内保温没了下一发请求可等，撤掉
                warm.cancel(conversation_id);
                on_event.send(ChatEvent::Notice {
                    text: "已按你的要求停止生成，剩余的工具调用没有执行。".into(),
                });
                // 同上：断在工具之间也只掐这一轮，没执行的那几条不许替目标做决定
                return close_turn(
                    host,
                    conversation_id,
                    auto_continue,
                    interrupted_at_boundary(stop),
                    continuing_goal,
                    send,
                    on_event,
                    |entry_ids| ChatEvent::Done {
                        input_tokens,
                        output_tokens,
                        duration_ms: started.elapsed().as_millis() as u64,
                        cached_tokens,
                        entry_ids,
                        model: config.model.clone(),
                        context_tokens: config.context_tokens,
                    },
                );
            }
            let via_mcp = crate::mcp::owns(mcp_servers, &call.name);
            let risk = if via_mcp {
                tools::Risk::High
            } else {
                tools::classify(&call.name, &args, root.as_deref())
            };
            // 这一串接下来要进三个地方：策略指纹、待审批队列（在盘上、跨重启）、审批界面。
            // 命令行里最常带的就是 token，所以进这三处之前先打码——而**动手用的不是这一串**：
            // 执行走的是模型给的原始参数，打码只改"它被怎么记录与怎么呈现"
            let input = mask_tool_input(via_mcp, &call.name, &args);

            // 沙箱边界先于一切：越界的文件写入是无条件拒绝（对齐 Codex 的
            // "单一边界覆盖一切动作"）——收容与低完整性只管命令的子进程，
            // write_file/edit_file 是本进程直写，必须在这里与命令同一套边界。
            // 犯不着为它跑用户的脚本，更犯不着弹审批
            if !via_mcp && crate::tool_runtime::sandbox::enabled() {
                if let Some(reason) = crate::tool_runtime::sandbox::boundary_violation(
                    &call.name,
                    &args,
                    bound_root.as_deref(),
                    root.as_deref(),
                ) {
                    let scope =
                        tool_runtime::Call::new(&call.name, &args, root.as_deref(), via_mcp);
                    let _ = audit_tool_in(
                        &data_dir,
                        conversation_id,
                        &scope,
                        crate::audit::Outcome::Denied,
                        None,
                    );
                    let (event, message) = tool_result_pair(
                        call,
                        ToolStatus::Denied,
                        tools::Risk::High.as_str(),
                        input.clone(),
                        reason,
                        None,
                    );
                    on_event.send(event);
                    send.push(message)?;
                    continue;
                }
            }

            // 执行前钩子：它要是拦下了，连询问界面都不弹——护栏要的就是"别让用户来判断这个"。
            // 它要是"问一句"（ask），这一次调用哪怕权限表放行，也拉回审批
            let hook_ask_reason: Option<String> = {
                let hooks = crate::hooks::runnable_in(config, &data_dir);
                if hooks.is_empty() {
                    None
                } else {
                    let report =
                        crate::hooks::fire(&hooks, "PreToolUse", root.as_deref(), |hook, cwd| {
                            json!({
                                "hook_event_name": hook.event,
                                "cwd": cwd.display().to_string(),
                                "model": config.model,
                                "tool_name": call.name,
                                "tool_input": &args,
                            })
                        });
                    emit_hooks(on_event, &report);

                    if let Some(reason) = report.blocked() {
                        let (event, message) = tool_result_pair(
                            call,
                            ToolStatus::Denied,
                            risk.as_str(),
                            input.clone(),
                            format!("插件钩子拦下了这次调用，没有执行：\n{reason}"),
                            None,
                        );
                        on_event.send(event);
                        send.push(message)?;
                        continue;
                    }
                    report.asks()
                }
            };

            // 闸门：一次调用先被翻译成 capability，再由权限表决定放行 / 询问 / 拒绝。
            // 这里不再问"风险高不高"——那是同一件事的旧影子，两份真相得合成一份。
            // 白名单来自本轮话题取用过的技能，它在权限表之前生效：技能没给的能力，
            // 不该拿去让用户点头
            let scope = tool_runtime::Call::new(&call.name, &args, root.as_deref(), via_mcp);
            // 入参先过声明校验：不合法的参数不配进审批框，更不能被"猜一个默认值"跑掉
            if let Err(problem) = tool_runtime::check_arguments(&scope) {
                let (event, message) = tool_result_pair(
                    call,
                    ToolStatus::Denied,
                    risk.as_str(),
                    input.clone(),
                    format!("参数不符合工具声明，没有执行：{problem}"),
                    None,
                );
                on_event.send(event);
                send.push(message)?;
                continue;
            }
            let ruling = tool_runtime::rule(
                &policy,
                &scope,
                &input,
                tool_runtime::allowlist(conversation_id).as_deref(),
            );
            let remembered = hub.is_remembered(&ruling.remember_key());

            if let crate::policy::Decision::Deny { reason } = &ruling.decision {
                let _ = audit_tool_in(
                    &data_dir,
                    conversation_id,
                    &scope,
                    crate::audit::Outcome::Denied,
                    None,
                );
                // 被拒也要给出工具结果，否则这条 tool_call 悬空
                let (event, message) = tool_result_pair(
                    call,
                    ToolStatus::Denied,
                    risk.as_str(),
                    input.clone(),
                    reason.clone(),
                    // 表上写着不许：那不是"该问而没问"
                    None,
                );
                on_event.send(event);
                send.push(message)?;
                continue;
            }

            // PermissionRequest 钩子先投票：deny 直接拒（不弹审批），allow 顶掉
            // "该问"（放行凭据写进 pass_reason，卡片同步写明是谁点的头），ask 把
            // 本来放行的调用拉回审批。发射前重解析：与相邻的闸同一规矩
            let permission_report = {
                let hooks = crate::hooks::runnable_in(config, &data_dir);
                if hooks.is_empty() {
                    crate::hooks::Report::default()
                } else {
                    let report = crate::hooks::fire(
                        &hooks,
                        "PermissionRequest",
                        root.as_deref(),
                        |hook, cwd| {
                            json!({
                                "hook_event_name": hook.event,
                                "cwd": cwd.display().to_string(),
                                "model": config.model,
                                "tool_name": call.name,
                                "tool_input": &args,
                                "risk": risk.as_str(),
                            })
                        },
                    );
                    emit_hooks(on_event, &report);
                    report
                }
            };
            if let Some(reason) = permission_report.blocked() {
                // 权限钩子说不许：不执行、不弹审批，原因原样到卡片
                let (event, message) = tool_result_pair(
                    call,
                    ToolStatus::Denied,
                    risk.as_str(),
                    input.clone(),
                    reason,
                    None,
                );
                on_event.send(event);
                send.push(message)?;
                continue;
            }
            let permission_hook_pass = permission_report.approves();

            // 审批等待也响应停止：否则按了停止还要干等满 10 分钟超时。
            // 权限表说"该问"或执行前钩子说"问一句"，都走同一条审批路
            let needs_approval = (matches!(ruling.decision, crate::policy::Decision::Ask { .. })
                && !remembered)
                || hook_ask_reason.is_some();
            // 权限钩子的裁决并进判据（deny 已在上面直接拒了）：allow 顶掉"该问"，
            // ask 把本来放行的调用拉回审批——单数的放行盖不过 block 与 ask
            let needs_approval = (needs_approval && permission_hook_pass.is_none())
                || permission_report.asks().is_some();
            // O4-5：Ask 档的只读命令免确认。run_command 且 command_policy 判只读时，
            // "问一声"是纯摩擦——看一眼的代价不该弹卡。放行凭据写进 pass_reason
            // （审计照记，与卡片同文），可写命令一格不动。钩子明确要问的仍问：
            // 那是用户自己的自动化在说话，只读豁免不盖过它
            let read_only_pass = needs_approval
                && permission_report.asks().is_none()
                && call.name == "run_command"
                && crate::command_policy::is_read_only(
                    args["command"].as_str().unwrap_or_default(),
                );
            let needs_approval = needs_approval && !read_only_pass;
            // 后台 run 没有人可问：这一发挂到待审批队列，动作不动手。让它去走 ApprovalHub
            // 那 600s 超时的话，"没人看"就会被记成"用户摇头"，而队列里那条待审批——
            // 也就是"等谁来处理"的事实——根本不会存在
            let escalation =
                if needs_approval && crate::tasks::escalate::is_unattended(conversation_id) {
                    park_unattended(host, conversation_id, &ruling, &input)
                } else {
                    Escalated::Prompt
                };
            // "该问而没问"那两种放行要答得出凭哪一条：命中本话题内的规则，或无人值守
            // 那条路上早已登记的 standing 授权。少了这一句，卡片上那一行与"刚刚点了头"
            // 的那一行长得一模一样——设计把悄悄放行算作缺陷（§4 步骤 3、§8 风险 6）。
            // 自动审查的放行在下面那格补标（mut：审查通过时写"自动审查通过"）
            let mut pass_reason = if !matches!(ruling.decision, crate::policy::Decision::Ask { .. })
            {
                // 权限表本来就放行：那不是"跳过了询问"，标它等于把旋钮说反
                None
            } else if remembered {
                Some(SESSION_RULE_PASS.to_string())
            } else if matches!(escalation, Escalated::Run) {
                Some(STANDING_GRANT_PASS.to_string())
            } else if read_only_pass {
                Some(READ_ONLY_PASS.to_string())
            } else {
                None
            };
            // 权限钩子替用户点的头要答得出凭哪一条：写进同一格，与卡片同文
            if needs_approval {
                // 该问的还是问了（钩子要问或别的钩子没放行），这里不动
            } else if let Some(reason) = permission_hook_pass {
                pass_reason = Some(format!("按 PermissionRequest 钩子放行：{reason}"));
            }
            if let Escalated::Halted { outcome, reason } = escalation {
                // 停在待批队列里等人：这一发没动手，没有"放行"可标
                let _ = audit_tool_in(&data_dir, conversation_id, &scope, outcome, None);
                let (event, message) = tool_result_pair(
                    call,
                    ToolStatus::Denied,
                    risk.as_str(),
                    input.clone(),
                    reason,
                    // 挂在待批队列里等人：这一发根本没动手，没有"放行"可标
                    None,
                );
                on_event.send(event);
                send.push(message)?;
                continue;
            }

            let approved = if matches!(escalation, Escalated::Prompt) && needs_approval {
                if config.auto_review {
                    // 自动审查（对齐 deepseek 的 Auto review）：审查模型替人拍板。
                    // 人工审批卡在这条路上**不发**——发了也是一块死按钮：没人在
                    // ApprovalHub 等人的票，点拒绝石沉大海，然后审查通过照跑（真机踩过）。
                    // **不改沙箱边界**——只处理升级请求，边界内的动作照旧自主执行。
                    // 审查失败 fail-closed：按拒绝处理（模型是安全闸不是便利闸）
                    let verdict =
                        auto_review_verdict(&config_dir, config, &call.name, &input, risk.as_str());
                    let audit_root = data_dir.clone();
                    crate::audit::record(
                        &audit_root,
                        crate::audit::Actor::Model,
                        if verdict.approved {
                            "approval:auto_review"
                        } else {
                            "approval:auto_review_denied"
                        },
                        &ruling.key,
                        if verdict.approved {
                            crate::audit::Outcome::Ok
                        } else {
                            crate::audit::Outcome::Denied
                        },
                    )?;
                    if !verdict.approved {
                        let (event, message) = tool_result_pair(
                            call,
                            ToolStatus::Denied,
                            risk.as_str(),
                            input.clone(),
                            format!("自动审查未通过：{}", verdict.reason),
                            None,
                        );
                        on_event.send(event);
                        send.push(message)?;
                        continue;
                    }
                    // 审查通过 = 放行，卡片与账上都要答得出凭哪一条：凭审查模型那一票，
                    // 不是"用户点过头"——收回的旋钮是设置里的自动审查开关
                    pass_reason = Some(AUTO_REVIEW_PASS.to_string());
                    true
                } else {
                    // 先登记"这一次问的是哪份动作"，界面上的"本话题内允许"才有的可点：
                    // 键由后端算，前端只把它看到的那条 id 换回来。标签用确认框上当初那句话说的是
                    // 同一份文本——用户撤销时能认出自己放过的是哪一下，靠的就是这一格
                    hub.stage(&call.id, &ruling.remember_key(), &short_label(&input));
                    on_event.send(ChatEvent::Tool {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        status: ToolStatus::Pending,
                        risk: risk.as_str().into(),
                        input: input.clone(),
                        output: None,
                        arguments: Some(call.arguments.clone()),
                        // 这一次是真的在问：没有"跳过询问"可标
                        pass_reason: None,
                        content_chars: Some(call.content_chars),
                    });

                    // 窗口在后台时这就是一块看不见的暂停键：系统通知把它喊回来
                    host.toast_approval_needed(&input);
                    let answer = hub.wait(&call.id, APPROVAL_TIMEOUT, stop);
                    // 人的那一次点头单独落一行：它记的是"谁决定的"，而下面那条 `tool:*`
                    // 记的是"做了什么"。超时与按停止都算摇头
                    let audit_root = data_dir.clone();
                    crate::audit::record(
                        &audit_root,
                        crate::audit::Actor::User,
                        if answer {
                            "approval:granted"
                        } else {
                            "approval:refused"
                        },
                        &ruling.key,
                        if answer {
                            crate::audit::Outcome::Ok
                        } else {
                            crate::audit::Outcome::Denied
                        },
                    )?;
                    answer
                }
            } else {
                true
            };

            if stopped(stop) {
                // 回合就此打住：挂着的那发回合内保温没了下一发请求可等，撤掉。
                // 不再发停止通知：前端按停止时已弹过提示，半截正文停在原地
                warm.cancel(conversation_id);
                // 同上：工具跑完才看到旗，那一轮同样只是被打断，不是目标结束了
                return close_turn(
                    host,
                    conversation_id,
                    auto_continue,
                    interrupted_at_boundary(stop),
                    continuing_goal,
                    send,
                    on_event,
                    |entry_ids| ChatEvent::Done {
                        input_tokens,
                        output_tokens,
                        duration_ms: started.elapsed().as_millis() as u64,
                        cached_tokens,
                        entry_ids,
                        model: config.model.clone(),
                        context_tokens: config.context_tokens,
                    },
                );
            }

            if !approved {
                let _ = audit_tool_in(
                    &data_dir,
                    conversation_id,
                    &scope,
                    crate::audit::Outcome::Denied,
                    None,
                );
                let (event, message) = tool_result_pair(
                    call,
                    ToolStatus::Denied,
                    risk.as_str(),
                    input.clone(),
                    "用户拒绝执行该操作。".into(),
                    None,
                );
                on_event.send(event);
                send.push(message)?;
                continue;
            }

            // 落账之后才动手：一个说不出"谁在什么时候对什么做了什么"的客户端，
            // 出了问题没法复盘。写不进去就不执行，而不是"记不上也要跑"
            if let Err(error) = audit_tool_in(
                &data_dir,
                conversation_id,
                &scope,
                crate::audit::Outcome::Ok,
                pass_reason.as_deref(),
            ) {
                let (event, message) = tool_result_pair(
                    call,
                    ToolStatus::Denied,
                    risk.as_str(),
                    input.clone(),
                    format!("审计日志写不进去，因此没有执行：{error}"),
                    pass_reason.clone(),
                );
                on_event.send(event);
                send.push(message)?;
                continue;
            }

            on_event.send(ChatEvent::Tool {
                id: call.id.clone(),
                name: call.name.clone(),
                status: ToolStatus::Running,
                risk: risk.as_str().into(),
                input: input.clone(),
                output: None,
                arguments: Some(call.arguments.clone()),
                pass_reason: pass_reason.clone(),
                content_chars: Some(call.content_chars),
            });

            // 写文件前先取走旧正文：面板要报行数，回滚要靠它。写失败就不落账——
            // 文件根本没动，记一条就是在记假账。edit_file 同闸：替换在快照那一步
            // 就校验过，校验不过不落账，真错误由执行体报给模型。
            // delete_file 每个路径各预记一条，落账时核对存在性（commit_deleted）。
            // 备份（D3）与快照同一时机动手：改动前的正文只读这一遍
            let backup_options = crate::backup::Options {
                enabled: config.backup_enabled,
                total_mb: config.backup_total_mb,
            };
            let pending_edits: Vec<crate::edits::PendingEdit> = if !via_mcp {
                match call.name.as_str() {
                    "write_file" | "edit_file" => crate::edits::snapshot_before_in(
                        &data_dir,
                        conversation_id,
                        &call.id,
                        &call.name,
                        &args,
                        root.as_deref(),
                        backup_options,
                    )
                    .into_iter()
                    .collect(),
                    "delete_file" => crate::edits::snapshot_delete_before_in(
                        &data_dir,
                        conversation_id,
                        &call.id,
                        &args,
                        root.as_deref(),
                        backup_options,
                    ),
                    _ => Vec::new(),
                }
            } else {
                Vec::new()
            };

            // 三条来源（内置 / 扩展 / 技能）认路由与缓存重试规则都在 `tool_runtime::source` 里，
            // 这里只负责把结果接回去。过去这三个分支各答各的"能不能重试、要不要缓存"
            let skill_body = |name: &str| crate::skills::load_body_in(&config_dir, &data_dir, name);
            let registry = tool_runtime::source::Registry::new(
                root.as_deref(),
                mcp_servers,
                config,
                mcp_hub,
                &skill_body,
                conversation_id,
            );
            // 派单在路由外接走：执行要父话题 id 与配置目录，注册表够不着这两样——
            // 这里两样都在手上（load_skill 归技能路是同款先例：声明在注册表，执行看住处）
            let ran = tool_dispatch::route(
                send,
                host,
                config,
                root.as_deref(),
                conversation_id,
                hub,
                stop,
                via_mcp,
                call,
                &args,
                &policy,
                mcp_servers,
                mcp_hub,
                &registry,
                on_event,
            );
            // 先把"这一份是怎么来的"记下来再移走结果：经过与来源也是要说得出口的事实
            let from = ran.source;
            let note = ran.note();
            let executed = ran.output;

            if executed.is_ok() {
                crate::edits::commit_deleted_in(&data_dir, &pending_edits);
            }
            // 快照事件化：台账落了什么这里就广播什么。pending 里每一笔都是
            // "动手前存了副本（或如实说明没存成）"的一次工具写入，
            // 变更面板即时点亮，审计里也有这一笔
            if !pending_edits.is_empty() {
                for edit in crate::edits::committed_snapshots_in(&data_dir, &pending_edits) {
                    on_event.send(ChatEvent::FileSnapshot {
                        path: edit.path,
                        call_id: edit.call_id,
                        additions: edit.additions,
                        deletions: edit.deletions,
                        backup: edit.backup,
                        snapshot_note: edit.snapshot_note,
                    });
                }
            }

            match executed {
                Ok(output) => {
                    // 技能声明的工具白名单在**取用之后**才生效，并且当场说一句：
                    // 它改变的是模型接下来能调什么，悄悄生效等于让用户猜
                    if from == tool_runtime::source::Kind::Skill {
                        let wanted = args["name"].as_str().unwrap_or_default();
                        let declared =
                            crate::skills::declared_tools_in(&config_dir, &data_dir, wanted);
                        if !declared.is_empty() {
                            let merged = tool_runtime::note_tools(conversation_id, Some(&declared));
                            on_event.send(ChatEvent::Notice {
                                text: format!(
                                    "技能「{wanted}」只用这些工具：{}。名单已从这一刻起生效，之外的工具调用会被拒。",
                                    merged.join(" · ")
                                ),
                            });
                        }
                    }

                    // 执行后钩子：副作用已经发生，撤不掉了，它能做的是把检查结果转给模型。
                    // 发射前重解析：工作区钩子的信任/指纹/撤销在这里即时生效
                    let feedback = {
                        let hooks = crate::hooks::runnable_in(config, &data_dir);
                        if hooks.is_empty() {
                            None
                        } else {
                            let report = crate::hooks::fire(
                                &hooks,
                                "PostToolUse",
                                root.as_deref(),
                                |hook, cwd| {
                                    json!({
                                        "hook_event_name": hook.event,
                                        "cwd": cwd.display().to_string(),
                                        "model": config.model,
                                        "tool_name": call.name,
                                        "tool_input": &args,
                                        "tool_response": &output,
                                    })
                                },
                            );
                            emit_hooks(on_event, &report);
                            report.context()
                        }
                    };

                    // 钩子补的话一起进工具结果：模型看到的和用户看到的是同一份，不留暗账
                    let content = match feedback {
                        Some(text) => format!("{output}\n\n{text}"),
                        None => output,
                    };
                    // 超长结果句柄化：全文归档（obs_recall 按需取回），发给服务商的只有
                    // 首尾摘录与句柄——中间大段不再每轮重放，要用的时候召回来。
                    // 上限 = 契约的 max_output_bytes 与全局钳制取小者（并行后账同一把尺）
                    let tool_result_max = crate::tool_contract::effective_cap(
                        round_contracts[call_index].max_output_bytes,
                        config.tool_result_max_chars,
                    );
                    if content.chars().count() > tool_result_max {
                        crate::observations::archive(&call.id, &content);
                    }
                    let content = pack_tool_result(&content, tool_result_max, Some(&call.id));
                    // 来源标注：话题里那段原文是磁盘读回来的、子进程跑出来的，
                    // 还是扩展给的，用户和模型都得看得出来
                    let content = tool_runtime::annotate(from, &call.name, content);
                    // 命中缓存、或者第几次才送达，都要当场说一句：这两件事改变的是
                    // "模型读到的这段来自哪一版"，藏在计数器里等于没告诉任何人
                    let content = match &note {
                        Some(text) => format!("{content}\n{text}"),
                        None => content,
                    };

                    let (event, message) = tool_result_pair(
                        call,
                        ToolStatus::Done,
                        risk.as_str(),
                        input.clone(),
                        content,
                        pass_reason.clone(),
                    );
                    on_event.send(event);
                    send.push(message)?;
                }
                Err(error) => {
                    // 放行与失败是两件事：审计里"跑失败了"和"根本没让跑"必须分得开
                    let _ = audit_tool_in(
                        &data_dir,
                        conversation_id,
                        &scope,
                        crate::audit::Outcome::Failed,
                        pass_reason.as_deref(),
                    );
                    // PostToolUseFailure：失败也是执行后的一个节点。它的 deny 与
                    // PostToolUse 同义（副作用已经发生），只能转达——拦不住任何事
                    {
                        let hooks = crate::hooks::runnable_in(config, &data_dir);
                        if !hooks.is_empty() {
                            let report = crate::hooks::fire(
                                &hooks,
                                "PostToolUseFailure",
                                root.as_deref(),
                                |hook, cwd| {
                                    json!({
                                        "hook_event_name": hook.event,
                                        "cwd": cwd.display().to_string(),
                                        "model": config.model,
                                        "tool_name": call.name,
                                        "tool_input": &args,
                                        "error": error.to_string(),
                                    })
                                },
                            );
                            emit_hooks(on_event, &report);
                        }
                    }
                    let (event, message) = tool_result_pair(
                        call,
                        ToolStatus::Failed,
                        risk.as_str(),
                        input.clone(),
                        format!("执行失败：{error}"),
                        pass_reason.clone(),
                    );
                    on_event.send(event);
                    send.push(message)?;
                }
            }
        }

        // 插话检查点 2：最后一轮工具结果刚落地时插入的插话，
        // 不捞的话要等下一位用户消息才会被看见
        for text in steering.drain(conversation_id) {
            send.push(Message::User {
                content: format!("（执行中途的插话）{text}"),
                images: Vec::new(),
                audios: Vec::new(),
                videos: Vec::new(),
            })?;
        }
    }

    // 轮数到顶强制收摊：待放的保温没有下一发可等，撤掉再报错。
    // 只有有限的天花板才走得到这里——0 = 不设上限的那一路永远轮不到这句报错
    warm.cancel(conversation_id);
    Err(format!(
        "工具调用达到 {} 轮上限，已停止。{}",
        max_rounds,
        if rounds_cap.is_some() {
            "这一发的天花板是它自己带的那个（子运行的收窄），不是设置里那个。"
        } else {
            "可在设置 → 配置里调高，写 0 则不设上限。"
        }
    ))
}
