//! 上下文与摘要：压缩检查点、终端工具、一次性补全（优化路线 O1-7 从 chat.rs 拆出）。
//!
//! 三块职责：
//! - **摘要**：summary_prompt / summarize_history（检查点合并与生成，喂给 complete_once_in）；
//! - **上下文命令**：context_inspect / compact_layer / context_undo_compaction（面板那一排按钮）；
//! - **终端工具与一次性补全**：terminal_exec / complete_once（title、auto_review、
//!   goal_criteria_draft 都走它——一条不进对话史的请求路）。
//!
//! 命令住这里，lib.rs 的 generate_handler 指到 `chat::transcript::…`。

use super::{
    clamp_tool_result, compaction_boundary, config, conversation_project, conversation_sections,
    open_session, project_card_text, request_round, sizing_of, standing_head, tokens_of,
    tool_runtime, AppConfig, ChatMessage, EntryPayload, Send, ENHANCE_SYSTEM, KEEP_RECENT_CHARS,
    SUMMARY_MARKER,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Instant;
use tauri::{AppHandle, Manager};
/// 一轮请求：按配置选线协议。工具循环只认 chat 格式的消息数组，翻译发生在发请求这一刻，
/// 所以钩子、审批、工具结果回填这些逻辑不必知道自己面对的是哪种服务商。
/// 把对话历史压成一份衔接用的摘要。SoL-Pi 的压缩指令原则：
/// 保住已完成的工作、验证结果、重要决策、剩余工作——丢掉这些的压缩等于让 agent 失忆。
/// pi 的结构化摘要模板：六段式检查点 + 精确保留路径/函数名/错误信息。
/// 有前次摘要时切换到增量更新模式（保留旧信息、合并新进展），比全量重摘省得多也更稳
pub(in crate::chat) const SUMMARY_SYSTEM: &str = "你是对话摘要助手。阅读用户与 AI 助手的对话，输出一份结构化的上下文检查点摘要，另一个 AI 将用它继续这项工作。不要继续对话，不要回答对话里的任何问题，只输出摘要正文。用中文。";

pub(in crate::chat) const SUMMARY_BASE: &str = "严格按以下格式输出摘要：\n\
## 目标\n[用户要完成什么？多个任务可分条]\n\
## 约束与偏好\n- [用户提到的约束、偏好或要求；没有则写（无）]\n\
## 进展\n### 已完成\n- [x] [已完成的任务/改动]\n### 进行中\n- [ ] [当前正在做的]\n### 受阻\n- [阻碍进展的问题；没有则删掉本节]\n\
## 关键决策\n- **[决策]**：[简要原因]\n\
## 下一步\n1. [按顺序列出接下来要做的事]\n\
## 关键上下文\n- [继续工作所需的数据、路径、引用；没有则写（无）]\n\n\
每节保持简洁。精确保留文件路径、函数名和错误信息。";

pub(in crate::chat) const SUMMARY_UPDATE: &str =
    "上方 <previous-summary> 是既有摘要，本次消息要合并进它。规则：\
保留既有摘要的全部信息；合并新对话里的进展、决策与上下文；已完成的事项从「进行中」移到「已完成」；\
根据当前状态更新「下一步」；精确保留文件路径、函数名与错误信息；已不再相关的内容可以移除。\
按与既有摘要相同的六段格式输出更新后的完整摘要。";

/// 摘要调用的输入。把待压段拍平成 `<conversation>` 文本是刻意的：这次请求的前缀
/// 永远不会有第二条请求来延伸，复用它只是白写（设计档 §5.2），所以它不共享话题身份
pub(in crate::chat) fn summary_prompt(history: &[Value]) -> String {
    // 上一份摘要单独抽出走增量更新：混进 transcript 会让模型把旧摘要当对话重摘一遍
    let mut previous: Option<String> = None;
    let mut transcript = String::new();
    for message in history {
        let role = message["role"].as_str().unwrap_or_default();
        // 摘要模型同样看不见图片，但它要知道这里有过一张图——只认 as_str() 会把
        // 带图的那一问整条从摘要素材里漏掉
        let content = crate::session::entry::content_text(message);
        if content.is_empty() {
            continue;
        }
        if role == "system" && content.starts_with(SUMMARY_MARKER) {
            previous = Some(
                content
                    .trim_start_matches(SUMMARY_MARKER)
                    .trim()
                    .to_string(),
            );
            continue;
        }
        let label = match role {
            "user" => "用户",
            "assistant" => "助手",
            "tool" => "工具结果",
            _ => "系统",
        };
        transcript.push_str(&format!(
            "{label}：{content}

"
        ));
    }

    let mut prompt = format!(
        "<conversation>
{transcript}
</conversation>

"
    );
    if let Some(prev) = &previous {
        prompt.push_str(&format!(
            "<previous-summary>
{prev}
</previous-summary>

"
        ));
        prompt.push_str(SUMMARY_UPDATE);
    } else {
        prompt.push_str(SUMMARY_BASE);
    }
    prompt
}

/// 摘要请求走与对话同一个出口（`complete_once`）：它曾是单次上下文里最贵的一次调用，
/// 原来却自己拼 HTTP——既不记账、错误分类也重复了一份
pub(in crate::chat) fn summarize_history(
    app: &AppHandle,
    config: &AppConfig,
    history: &[Value],
) -> Result<String, String> {
    summarize_history_in(
        &app.path().app_config_dir().map_err(|e| e.to_string())?,
        &app.path().app_data_dir().map_err(|e| e.to_string())?,
        config,
        history,
    )
}

/// worker 变体（M3 收官）：目录由 Main 经 CLI 传来
pub(in crate::chat) fn summarize_history_in(
    config_dir: &std::path::Path,
    data_dir: &std::path::Path,
    config: &AppConfig,
    history: &[Value],
) -> Result<String, String> {
    // 压缩前钩子：auto 与手动压缩都从这一条路过。它拦不住压缩（这个事件没有
    // 拒绝语义），能做的是在历史被摘要替换前把现场外发或打点
    {
        // 发射前重解析就是这一份：信任/指纹/撤销的即时性都从这里来
        let hooks = crate::hooks::runnable_in(config, data_dir);
        if hooks.iter().any(|hook| hook.event == "PreCompact") {
            let root = config
                .active_project()
                .map(|project| std::path::PathBuf::from(&project.path));
            crate::hooks::fire(&hooks, "PreCompact", root.as_deref(), |hook, cwd| {
                json!({
                    "hook_event_name": hook.event,
                    "cwd": cwd.display().to_string(),
                })
            });
        }
    }
    let mut one_off = config.clone();
    one_off.temperature = 0.2;
    one_off.max_tokens = one_off.max_tokens.min(2048);
    let messages = json!([
        { "role": "system", "content": SUMMARY_SYSTEM },
        { "role": "user", "content": summary_prompt(history) },
    ]);
    complete_once_in(config_dir, &one_off, messages, "summary")
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalResult {
    pub output: String,
}

/// 终端标签的命令执行。复用 run_command 的执行体（60 秒超时、输出截断都现成）。
///
/// 它**不弹审批**，但不是"绕过闸门"：命令是用户自己在终端标签里敲的，他就是审批人，
/// 向自己请求批准只会多一次没有信息量的点击。它仍然要过两道闸——
/// 权限表里被显式设成 Deny 的那一档（比如把 `exec.arbitrary` 关掉），以及审计落账；
/// 落账写不进去就不执行，与模型那条路同一条规矩
#[tauri::command]
pub(crate) fn terminal_exec(
    app: AppHandle,
    command: String,
    cwd: Option<String>,
) -> Result<TerminalResult, String> {
    let command = command.trim().to_string();
    if command.is_empty() {
        return Err("命令为空。".into());
    }
    let root: Option<PathBuf> = cwd
        .filter(|path| std::path::Path::new(path).is_dir())
        .map(PathBuf::from)
        .or_else(|| {
            // 终端与 run_command 同一条根链：项目 → 主目录（未绑定工作目录也能跑）
            config::load(&app).effective_root()
        });

    let args = json!({ "command": command });
    let scope = tool_runtime::Call::new("run_command", &args, root.as_deref(), false);
    let app_config = config::load(&app);
    let policy = app_config.active_policy();
    let audit_root = app.path().app_data_dir().map_err(|e| e.to_string())?;
    if let Err(reason) = terminal_ruling(&policy, &scope, &command) {
        // 拦下来也要有一行。P0 的判据是"任意一次调用都在审计里有一行，且拒绝原因可复述"，
        // 而这一支以前直接 return——红线被碰过几次，账上查不出来
        let _ = terminal_audit(&audit_root, &scope, crate::audit::Outcome::Denied);
        return Err(reason);
    }
    // 落账之后才动手：写不进去就不执行，而不是"记不上也要跑"（与工具那条路同一个约定）
    terminal_audit(&audit_root, &scope, crate::audit::Outcome::Ok)?;

    // 走与模型调用同一条执行路：缓存与重试的规则只写在 `tool_runtime::source` 一处。
    // `run_command` 不是幂等的，而重试的第一条门闩就是"没送到执行体"——所以这一手
    // 对终端来说行为与直接调用完全一样，差别只在这里不再有第二份执行入口
    let ran = tool_runtime::source::run_with(
        // 终端这条手没有话题上下文：后台句柄不认主人，面板不会把它数进任何一条话题
        &tool_runtime::source::BuiltinSource {
            root: root.as_deref(),
            owner: None,
        },
        tool_runtime::source::shared_cache(),
        tool_runtime::source::Retry::default(),
        "run_command",
        &args,
    );
    if ran.output.is_err() {
        // 想做了但没做成，也是一件要说得出的事。只记放行的那一半，
        // 账上就只剩成功，而审计存在的理由是复盘
        let _ = terminal_audit(&audit_root, &scope, crate::audit::Outcome::Failed);
    }
    let output = ran.output.map_err(|error| error.text)?;
    Ok(TerminalResult {
        output: clamp_tool_result(&output, app_config.tool_result_max_chars),
    })
}

/// 终端那一行过不过闸门。**只有 `Deny` 算拒绝**是拍板过的（见 `terminal_exec` 里那段注释）：
/// 命令是用户自己在终端里敲的，他就是审批人，所以 `Ask` 在这里等于放行；
/// 而表上的红线仍然生效
pub(in crate::chat) fn terminal_ruling(
    policy: &crate::policy::Policy,
    scope: &tool_runtime::Call,
    command: &str,
) -> Result<(), String> {
    match tool_runtime::rule(policy, scope, command, None).decision {
        crate::policy::Decision::Deny { reason } => Err(reason),
        _ => Ok(()),
    }
}

/// 终端那一行的审计行。归属固定是 `User`——这一行不是模型点的
pub(in crate::chat) fn terminal_audit(
    root: &std::path::Path,
    scope: &tool_runtime::Call,
    outcome: crate::audit::Outcome,
) -> Result<(), String> {
    crate::audit::record(
        root,
        crate::audit::Actor::User,
        "terminal:run_command",
        &tool_runtime::audit_target(scope),
        outcome,
    )
}

/// 这一轮发出去的东西按层拆开看。派生视图：每次从日志现算，
/// 面板没有任何写回的路——请求体不能有第二份真相
#[tauri::command]
pub(crate) fn context_inspect(
    app: AppHandle,
    conversation_id: String,
) -> Result<crate::session::InspectorReport, String> {
    let config = config::load(&app);
    // 校准样本住在用量台账里，而 `inspect` 只吃日志：那把尺与这份报告都由这里填。
    // 量得不够就是 None，界面上要说"还在估"——同一个系数不许一处实测一处瞎猜
    let calibration = crate::usage::calibration_for(&app, &config.model);
    let opened = open_session(&app, &conversation_id)?;
    let mut report = crate::session::inspector::inspect(
        &conversation_id,
        &opened.log,
        &standing_head(),
        sizing_of(&config, calibration.as_ref()),
        crate::usage::last_prompt_tokens_for(&app, &conversation_id)
            .unwrap_or(0)
            .try_into()
            .ok()
            .filter(|tokens: &u32| *tokens > 0),
    )
    .map_err(|error| error.to_string())?;
    // 谱系在话题 header 里，而 `inspect` 只吃日志与常驻段：这一格由这里填
    report.parent_session_id = opened.header.parent_session.clone();
    // 缓存与重试是进程内的账，不在日志里：同样由这里填，界面上那句"命中几次"才有出处
    report.cache = Some(crate::tool_runtime::source::shared_cache().stats());
    report.calibration = calibration;
    Ok(report)
}

/// 手动压缩：用户在上下文用量面板主动点"立即压缩"。
/// 与自动压缩共用同一条路：读话题日志、写一条 compaction 条目，前端不再送历史下来
#[tauri::command]
pub async fn compact_history(app: AppHandle, conversation_id: String) -> Result<String, String> {
    // 读话题日志 + 发一次摘要请求（网络往返）：都是主线程陪不起的活
    crate::history::run_blocking(move || {
        let config = config::load(&app);
        // 手动压缩与自动压缩走同一条路：读话题日志、压完写一条 compaction 条目。
        // 旧做法是前端把自己的消息数组送下来换一份摘要，界面再自己切片——两边各压各的。
        // 段照常渲染，边界才带得走"当时生效的那份 system"（§6.1 的可重建性）；
        // 这条路**不**同步段：只做摘要，不改历史形状，段差分行留给下一轮对话
        // 记忆段不在压缩边界里带走：它是按本轮提法挑出来的，边界要带走的是长期有效的那几段
        let opened = open_session(&app, &conversation_id)?;
        let mode_body =
            crate::session::mode::section_body(&crate::session::mode::in_effect(&opened.log));
        // 段要重放的是"当时生效的 system"：项目卡与对话回合同一个判定（话题的项目 → 激活项目）
        let sections = conversation_sections(
            project_card_text(
                &config,
                conversation_project(&app, &config, &conversation_id)
                    .or_else(|| config.active_project()),
                crate::worktree::view_for(&app, &conversation_id).as_ref(),
            )
            .as_deref(),
            crate::skills::prompt(&app)?.as_deref(),
            None,
            mode_body.as_deref(),
        );
        let mut send = Send::open(opened, standing_head(), sections)?;
        let history = send.history().to_vec();
        if history.len() < 2 {
            return Err("对话太短，没有可压缩的内容。".into());
        }
        let summary = summarize_history(&app, &config, &history)?;
        let origin = send.provenance()?;
        // 压缩条目要带的是"压之前那一共发出去多少"，口径与自动压缩那处同一个函数：
        // 写 0 会让面板上的"省下多少"永远算错
        let before = crate::session::layers::thread_chars(send.standing(), &history);
        match compaction_boundary(&history, &origin, KEEP_RECENT_CHARS) {
            Some((first_kept_entry_id, _)) => {
                send.append(EntryPayload::Compaction {
                    summary: summary.clone(),
                    first_kept_entry_id,
                    tokens_before: before,
                    usage: None,
                    system_message: send.section_snapshot(),
                })?;
                send.save();
                Ok(summary)
            }
            None => Err("可压缩的内容太少，这次没有压缩。".into()),
        }
    })
    .await
}

/// 要摘要的那一段与"实发的行"对得上吗。这一格此前是 `compact_layer` 里的一个内联条件，
/// 而那条命令要 `AppHandle`，于是它**一次都没被测过**——它守的偏偏是"压错地方"这件事：
/// 投影行与条目不是一一对应的（一条条目可以顶多行、一次撤回可以把一行摘掉），
/// 所以按行号切出来的那几行，在实发数组里可能少一条也可能多一条
pub(in crate::chat) fn check_compaction_slice(
    wire_entries: usize,
    matched_entries: usize,
    planned_rows: usize,
) -> Result<(), String> {
    if wire_entries == 0 {
        return Err("要压的那一段在实发的行里对不上，这次没有压缩。".into());
    }
    if matched_entries != planned_rows {
        return Err(format!(
            "要压的那一段在实发的行里对不上（计划 {planned_rows} 行，实发里找到 {matched_entries} 条），这次没有压缩。"
        ));
    }
    Ok(())
}

/// 按层压缩：把历史层里最老的那一段换成一行摘要。
///
/// 它与 `compact_history` 的分工在**断开的位置**：那一条从数组头部断，整个前缀重付一次；
/// 这一条只换中间，`from` 之前已经发过的那批字节一个都不动。所以它省得少、动得也少，
/// 适合"最旧那几轮已经没用了、后面的还想接着说"那种形状
#[tauri::command]
pub(crate) fn compact_layer(app: AppHandle, conversation_id: String) -> Result<String, String> {
    let config = config::load(&app);
    let opened = open_session(&app, &conversation_id)?;
    let mode_body =
        crate::session::mode::section_body(&crate::session::mode::in_effect(&opened.log));
    let sections = conversation_sections(
        // 项目卡与对话回合同一个判定（话题的项目 → 激活项目），与 run_turn 同源
        project_card_text(
            &config,
            conversation_project(&app, &config, &conversation_id)
                .or_else(|| config.active_project()),
            crate::worktree::view_for(&app, &conversation_id).as_ref(),
        )
        .as_deref(),
        crate::skills::prompt(&app)?.as_deref(),
        None,
        mode_body.as_deref(),
    );
    let mut send = Send::open(opened, standing_head(), sections)?;
    let sizing = sizing_of(
        &config,
        crate::usage::calibration_for(&app, &config.model).as_ref(),
    );
    let uses = crate::session::layers::uses(&send.opened.log, send.standing())
        .map_err(|error| error.to_string())?;
    let target = crate::session::layers::budget(&uses, sizing)
        .row(crate::session::layers::Layer::History)
        .map(|row| row.target)
        .ok_or("预算表里没有历史层那一行。")?;
    let rows = crate::session::layers::history_rows(&send.opened.log)
        .map_err(|error| error.to_string())?;
    let plan = crate::session::layers::layer_compaction(&rows, target)
        .ok_or("历史层没越界，或者最老那几行不够换一次摘要——这次不该压。")?;
    // 在付那一次摘要请求**之前**先问这一格：`still_over` 说的是"最老那段全换成一行
    // 摘要也坐不进预算"，这时候照压就是白花一笔钱、白少一段历史
    let left: usize = rows
        .iter()
        .map(|row| row.chars)
        .sum::<usize>()
        .saturating_sub(plan.chars);
    if let Some(problem) = layer_compaction_blocker(&plan, left, target) {
        return Err(problem);
    }

    // 要摘要的那几行从投影里按条目 id 取，不按行号切：投影行与条目不是一一对应的，
    // 拿行号当条目用会静默压错地方（`compaction_boundary` 那句注释说的是同一件事）
    let projection =
        crate::session::context::project(&send.opened.log).map_err(|error| error.to_string())?;
    let wanted: std::collections::HashSet<String> =
        rows[..plan.rows].iter().map(|row| row.id.clone()).collect();
    let slice: Vec<Value> = projection
        .entries
        .iter()
        .filter(|(id, _)| wanted.contains(id))
        .flat_map(|(_, messages)| messages.iter().map(|message| message.to_wire()))
        .collect();
    check_compaction_slice(
        slice.len(),
        projection
            .entries
            .iter()
            .filter(|(id, _)| wanted.contains(id))
            .count(),
        plan.rows,
    )?;

    let summary = summarize_history(&app, &config, &slice)?;
    write_layer_summary(&mut send, &plan, &summary)?;
    Ok(summary)
}

/// 这次按层压缩到底该不该动手。`CompactPlan::still_over` 从落地起就一直**只有写、没有读**：
/// 它自己的注释写着"这时候正确的动作不是继续压，而是让阶梯往下一步走，或者干脆报 Notice"，
/// 而 `compact_layer` 原先一路走到摘要那一步。剥成纯函数（只吃三个数，不要 `AppHandle`、
/// 也不要那一次请求）就是为了让这一格有地方测
pub(in crate::chat) fn layer_compaction_blocker(
    plan: &crate::session::layers::CompactPlan,
    left: usize,
    target: usize,
) -> Option<String> {
    if !plan.still_over {
        return None;
    }
    let rows = plan.rows;
    Some(format!(
        "最老那 {rows} 行全换成一行摘要，历史层仍要约 {left} 字符，坐不进 {target} 的预算——这一次没有压。\n\
         接着压只会白花一次请求、白少一段历史；要腾地方得走下一步（去掉记忆段，或收窄技能）。"
    ))
}

/// 把最老那一段历史换成一行摘要——只到"写这一行"为止，摘要文本从外面进来。
///
/// 剥出来的理由：整条命令里唯一拿不到的只有那一次摘要请求（要他点头才发），
/// 而**"压完到底生效没有"与"摘要写得好不好"是两件事**，不该被同一道门槛连着挡掉
pub(in crate::chat) fn write_layer_summary(
    send: &mut Send,
    plan: &crate::session::layers::CompactPlan,
    summary: &str,
) -> Result<(), String> {
    send.append(EntryPayload::BranchSummary {
        from_id: Some(plan.from_id.clone()),
        through_id: Some(plan.through_id.clone()),
        summary: summary.to_string(),
        usage: None,
    })?;
    send.save();
    Ok(())
}

/// 撤销一次改写：追加一行撤回，历史一条都不删。被那次改写顶替掉的条目就此原样回来，
/// 投影回到压之前的那一版——`context.rs` 的测试钉的是"逐字节相同"，不是"差不多"
#[tauri::command]
pub(crate) fn context_undo_compaction(
    app: AppHandle,
    conversation_id: String,
    entry_id: String,
) -> Result<String, String> {
    let mut send = Send::open(
        open_session(&app, &conversation_id)?,
        standing_head(),
        Vec::new(),
    )?;
    let id = revocation(&mut send, &entry_id)?;
    send.save();
    Ok(id)
}

/// 撤回那一行本身。剥成拿 `&mut Send` 的助手，是因为那一级命令只有 `AppHandle` 入口，
/// 于是"命令写的到底是 `None` 还是 `Some(\"\")`"没人钉得往——而这两者在投影里是
/// 两件不同的事：`None` 让被顶替的那些条目原样回来，`Some(\"\")` 是把它们换成空正文
pub(in crate::chat) fn revocation(send: &mut Send, target_id: &str) -> Result<String, String> {
    send.append(EntryPayload::ContextEdit {
        target_id: target_id.to_string(),
        replacement: None,
    })
}

/// 话题标题自动生成：第一轮对话结束后用一次极小的请求给话题起名。
/// 失败静默降级——标题只是界面便利，不该让用户看到报错
#[tauri::command]
pub(crate) fn generate_title(app: AppHandle, messages: Vec<ChatMessage>) -> Result<String, String> {
    let config = config::load(&app);

    let mut transcript = String::new();
    for message in messages.iter().take(6) {
        if message.content.trim().is_empty() {
            continue;
        }
        let label = if message.role == "user" {
            "用户"
        } else {
            "助手"
        };
        let content: String = message.content.chars().take(400).collect();
        transcript.push_str(&format!("{label}：{content}\n\n"));
    }
    if transcript.trim().is_empty() {
        return Err("没有可总结的对话内容。".into());
    }

    let prompt = format!(
        "根据以下对话开头，给这场话题起一个不超过 12 个字的标题。\
         只输出标题本身：不要引号、不要句号、不要任何解释或前缀。用中文。\n\n对话开头：\n{transcript}"
    );

    let title = complete_once(
        &app,
        &config,
        json!([{ "role": "user", "content": prompt }]),
        "title",
    )?;
    // 服务商偶尔会带引号或"标题："前缀，统剥掉
    let cleaned = title
        .trim()
        .trim_matches('"')
        .trim_matches('「')
        .trim_matches('」')
        .trim_start_matches("标题：")
        .trim()
        .to_string();
    let title: String = cleaned.chars().take(24).collect();
    if title.is_empty() {
        return Err("服务商返回了空标题。".into());
    }
    Ok(title)
}

#[tauri::command]
pub(crate) fn enhance_prompt(app: AppHandle, text: String) -> Result<String, String> {
    let draft = text.trim();
    if draft.is_empty() {
        return Err("输入框是空的，没有可增强的内容。".into());
    }
    let mut one_off = config::load(&app);
    one_off.temperature = 0.4;
    one_off.max_tokens = one_off.max_tokens.min(4096);
    let messages = json!([
        { "role": "system", "content": ENHANCE_SYSTEM },
        { "role": "user", "content": draft },
    ]);
    let enhanced = complete_once(&app, &one_off, messages, "enhance_prompt")?;
    let enhanced = enhanced.trim();
    if enhanced.is_empty() {
        return Err("模型没有返回内容，请重试。".into());
    }
    Ok(enhanced.to_string())
}

/// 一次性请求：仍然走 SSE，因为服务商对非流式请求会在生成完成前 504。
/// 调用方不关心逐字增量，所以 emit 是空的。
pub fn complete_once(
    app: &AppHandle,
    config: &AppConfig,
    messages: Value,
    scene: &str,
) -> Result<String, String> {
    complete_once_in(
        &app.path().app_config_dir().map_err(|e| e.to_string())?,
        config,
        messages,
        scene,
    )
}

/// worker 变体（M3 收官）：配置目录由 Main 经 CLI 传来（用量记账落在同一份库）
pub fn complete_once_in(
    config_dir: &std::path::Path,
    config: &AppConfig,
    messages: Value,
    scene: &str,
) -> Result<String, String> {
    if config.base_url.trim().is_empty() {
        return Err("尚未配置推理服务商地址。".into());
    }

    let key = config::api_key(config)?;
    // Copilot 的 keyring 里存的是 ghu_ 主令牌，不是直接可用的密钥：
    // base_url 指到 Copilot 网关的档案，发请求前在这里换成短期 token（自动缓存换发）
    let key = if config.base_url.contains("api.githubcopilot.com") {
        crate::oauth::copilot_access_token(&key, &config.proxy_default)?
    } else {
        key
    };
    let thread = messages.as_array().cloned().unwrap_or_default();
    let started = Instant::now();
    // 定时任务的回合暂无停止入口，给一个永不拉闸的开关占位
    let stop = std::sync::atomic::AtomicBool::new(false);
    let outcome = match request_round(config, &key, &thread, &[], None, &stop, &mut |_| {}) {
        Ok(outcome) => outcome,
        // 一次性调用没有话题日志可写：它的半成品不进任何历史，只记账然后报错。
        // chain_reset 恒为 false：它不属于任何话题的前缀链（conversation_id 是空串）
        Err(failure) => {
            crate::usage::record_turn_in(
                config_dir,
                config,
                scene,
                "",
                &config.model,
                &crate::usage::Tokens::default(),
                0,
                false,
                started.elapsed().as_millis() as u64,
                None,
                false,
                &failure.message,
            );
            return Err(failure.message);
        }
    };

    crate::usage::record_turn_in(
        config_dir,
        config,
        scene,
        "",
        &config.model,
        &tokens_of(&outcome.usage),
        outcome.sent_chars,
        false,
        started.elapsed().as_millis() as u64,
        None,
        true,
        "",
    );

    let text = outcome.text.trim().to_string();
    if text.is_empty() {
        return Err("服务商只回了思考过程或工具调用，没有正文。".into());
    }
    Ok(text)
}
