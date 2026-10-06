//! 聊天派单：主聊天模型按需派出子助理（`spawn_subagent` 工具的执行侧）。
//! 可派的名册有两个来源：出厂内置（[`builtin_subagents`]）与设置页自定义
//! （`config.subagents`），合并规则在 [`merged_catalog`]——内置在前，撞名内置赢。
//!
//! 它不是第二个执行引擎——走的就是 `chat::run_background_turn`，所以审批闸、上下文压缩、
//! 话题日志、用量归因对子话题天然可见。"子助理绕过审批"那种写法在这里根本没有入口。
//! 子话题的审批卡经全局 ApprovalHub 弹给用户：聊天父是有人看着的，不需要任务那条
//! 无人值守的挂起（`tasks::subagent` 的 `escalate::watch_run`）。
//!
//! 与任务图那条子助理路（`tasks::subagent::narrowed`）的两处分歧，各有一句理由：
//! 空工具名单在这里**放行**——聊天派的子助理允许是纯推理，那是意图不是配错
//! （出厂名册里的 `distill` 就是这么一个纯推理位）；链深不靠 runs 账本——聊天派的
//! 子话题不在任务账本上，嵌套的闸落在白名单本身（`spawn_subagent` 不进任何名单，
//! 子话题就看不见这个工具，内置也不例外）。

use serde::Serialize;
use serde_json::Value;
use tauri::AppHandle;

use crate::config::{AppConfig, SubagentDef, SubagentOverride};

/// 出厂内置的子助理名册（设计：deliverables/design-builtin-subagents.md）。
/// 定义住代码不住配置：升级能加新角色、改描述，不被用户配置里的旧拷贝钉死；
/// 配置只存覆盖项（`config.subagent_overrides`：服务商/模型、停用）。
/// 九个名额覆盖 aglab 的五个工具能力域——项目文件（general-purpose/explore/reviewer/
/// fixer）、验证（test-runner）、界面（ui-designer）、网络（researcher）、桌面（operator）、
/// 纯推理（distill）。权限不进定义：仍由名单推导，只收紧不放松
pub fn builtin_subagents() -> Vec<SubagentDef> {
    let tools = |ids: &[&str]| -> Vec<String> { ids.iter().map(|id| id.to_string()).collect() };
    vec![
        SubagentDef {
            name: "general-purpose".into(),
            description: "复合任务一把抓：查资料、改文件、跑命令、查网页，多步做完交一份结论。要动别的程序的窗口时改派 operator。".into(),
            system_prompt: "你是通用执行代理：独立完成一项完整的小任务，查、改、跑皆可。做完交回结论与证据（文件路径、命令输出）；不中途反问，卡住了就如实报告卡在哪。".into(),
            tools: tools(&[
                "list_files",
                "read_file",
                "search_text",
                "write_file",
                "edit_file",
                "run_command",
                "load_skill",
                "web_fetch",
                "agent_control",
            ]),
            chat_spawnable: true,
            ..SubagentDef::default()
        },
        SubagentDef {
            name: "explore".into(),
            description: "只读侦察：在项目里找代码/配置/事实（搜索、读文件、查网页），回答「在哪里、是什么、怎么接」，不改任何东西。全是只读工具，不会弹审批卡。".into(),
            system_prompt: "你是只读侦察代理：只许浏览、搜索与读取，不改任何东西。找到答案就交回，引用具体文件路径与原文作证据；找不到要说清找过哪里，不许猜。".into(),
            tools: tools(&["list_files", "read_file", "search_text", "load_skill", "web_fetch"]),
            chat_spawnable: true,
            ..SubagentDef::default()
        },
        SubagentDef {
            name: "reviewer".into(),
            description: "评审/找茬：对指定的文件或改动给意见，产出判断不动手；跑测试这类事交回父话题。".into(),
            system_prompt: "你是评审代理：只读，对拿到的文件或改动给意见。每条意见指认具体位置（文件+位置+原文），分「必须改/建议改/可以不动」三档；你不动手改。".into(),
            tools: tools(&["list_files", "read_file", "search_text", "load_skill", "web_fetch"]),
            chat_spawnable: true,
            ..SubagentDef::default()
        },
        SubagentDef {
            name: "operator".into(),
            description: "桌面操作员：看窗口、点控件、填表单、敲快捷键，替你操作别的程序。口令/支付类窗口它碰不了（后端硬闸）。".into(),
            system_prompt: "你是桌面操作代理：先用 list_windows 与 inspect_window 摸清目标窗口，再分步 computer_act 操作。每步动手前确认控件还在；口令/支付类窗口会被后端拒绝，碰到就停下交回。完成或卡住都如实报告。".into(),
            tools: tools(&["list_windows", "inspect_window", "computer_act"]),
            chat_spawnable: true,
            ..SubagentDef::default()
        },
        SubagentDef {
            name: "distill".into(),
            description: "纯推理：零工具的总结、提炼、改写、翻译——给它正文，它还你一段干净的话。".into(),
            system_prompt: "你是提炼代理：没有任何工具，只对拿到的正文做总结/提炼/改写。输出就是成品本身，不加前言后语。".into(),
            tools: Vec::new(),
            chat_spawnable: true,
            ..SubagentDef::default()
        },
        SubagentDef {
            name: "test-runner".into(),
            description: "测试执行者：跑指定的测试或构建命令，消化完整输出，只回报「过了没、挂了哪个、为什么」。长输出在这一轮里消化完，父话题只收结论。".into(),
            system_prompt: "你是测试执行代理：跑拿到的测试/构建命令，读完整输出，回报「通过与否、失败用例、关键报错与第一处该看的位置」。输出很长就提炼成证据链；你不改任何代码，跑不动或命令不存在就如实说。".into(),
            tools: tools(&["list_files", "read_file", "search_text", "run_command"]),
            chat_spawnable: true,
            ..SubagentDef::default()
        },
        SubagentDef {
            name: "fixer".into(),
            description: "修复者：按一份明确的改动清单或评审意见做多文件修改，最小 diff 不顺手重构，改完跑相关验证。与 general-purpose 的区别：照单施工，不查网、不自由发挥。".into(),
            system_prompt: "你是修复执行代理：按拿到的清单/spec 做多文件修改。每处改动对应清单里的一条，最小 diff；发现清单之外的问题记下来交回，不顺手修。改完跑与改动相关的验证（编译/测试），回报改了哪些文件、验证结果。".into(),
            tools: tools(&[
                "list_files",
                "read_file",
                "search_text",
                "edit_file",
                "write_file",
                "run_command",
            ]),
            chat_spawnable: true,
            ..SubagentDef::default()
        },
        SubagentDef {
            name: "ui-designer".into(),
            description: "界面实现者：从一句话需求实现或调整前端界面——布局、样式、交互状态，并用内置浏览器自查效果后再交回。".into(),
            system_prompt: "你是界面实现代理：动手前先读相关组件与样式约定（字号走统一的变量，不散落硬编码），再实现或调整界面；改完用 browser 打开预览自查布局与交互、按看到的问题迭代，交回改动清单与自查结论。".into(),
            tools: tools(&[
                "list_files",
                "read_file",
                "search_text",
                "edit_file",
                "write_file",
                "run_command",
                "browser",
            ]),
            chat_spawnable: true,
            ..SubagentDef::default()
        },
        SubagentDef {
            name: "researcher".into(),
            description: "联网研究员：深挖一个网络上的问题，多轮 web_fetch 交叉取证，交一份带来源链接的调研结论。只读，不动项目文件。".into(),
            system_prompt: "你是联网调研代理：用 web_fetch 多轮取证——官方文档优先、多方交叉验证、每条结论注明来源 URL；证据不足就明说哪里存疑，不许编。最后交一份结构化调研报告。".into(),
            tools: tools(&["web_fetch", "list_files", "read_file"]),
            chat_spawnable: true,
            ..SubagentDef::default()
        },
    ]
}

/// 一个内置子助理的设置页视图：出厂定义套上覆盖后的样子（含停用的——
/// 卡片要置灰展示，停用态由 `disabled` 说明，不是从列表里消失）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuiltinSubagentView {
    #[serde(flatten)]
    pub def: SubagentDef,
    pub disabled: bool,
}

/// 覆盖合一步：内置定义 × 用户覆盖 → (套好的定义, 停用)。
/// [`merged_catalog`] 与设置页视图共用这一步——两处读到的必须是同一份事实
fn apply_override(mut def: SubagentDef, overrides: &[SubagentOverride]) -> (SubagentDef, bool) {
    match overrides.iter().find(|item| item.name == def.name) {
        None => (def, false),
        Some(over) => {
            if let Some(model) = over.model_override() {
                def.model = model;
            }
            if let Some(endpoint) = over.endpoint_override() {
                def.endpoint_profile_id = endpoint;
            }
            (def, over.disabled)
        }
    }
}

/// 设置页「内置子助理」名册：合并覆盖后的完整视图
pub fn builtin_views(config: &AppConfig) -> Vec<BuiltinSubagentView> {
    builtin_subagents()
        .into_iter()
        .map(|def| {
            let (def, disabled) = apply_override(def, &config.subagent_overrides);
            BuiltinSubagentView { def, disabled }
        })
        .collect()
}

/// 可派名册的全集：内置（剔停用、套覆盖）在前，用户目录在后。
/// 派单查找与声明名单都吃它。出厂名是保留名——撞名的自定义在编译这份目录时
/// 就被丢掉，enum 里才不会出现两个同名候选（与编排侧 `profile_for_name` 的
/// "内置赢"同一条规则，收在单点）。未知名字的覆盖在上面就被无视了，这里不用再防
pub fn merged_catalog(config: &AppConfig) -> Vec<SubagentDef> {
    let mut merged: Vec<SubagentDef> = Vec::new();
    for def in builtin_subagents() {
        let (def, disabled) = apply_override(def, &config.subagent_overrides);
        if !disabled {
            merged.push(def);
        }
    }
    let reserved: Vec<String> = builtin_subagents()
        .into_iter()
        .map(|def| def.name)
        .collect();
    merged.extend(
        config
            .subagents
            .iter()
            .filter(|def| !reserved.contains(&def.name))
            .cloned(),
    );
    merged
}

/// 名单里找一个可被聊天派出的子助理。名字查不到与没标「聊天可调」都要说清楚为什么：
/// 主模型读到的是工具结果，它得能换个问法，而不是猜
pub fn spawnable<'a>(subagents: &'a [SubagentDef], name: &str) -> Result<&'a SubagentDef, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("没有给出子助理的名字。".into());
    }
    let def = subagents
        .iter()
        .find(|def| def.name == name)
        .ok_or_else(|| format!("没有叫「{name}」的子助理：名单在设置页「子助理」。"))?;
    if !def.chat_spawnable {
        return Err(format!(
            "「{name}」没有开放给聊天派单（设置页里「主模型可调」是关的）。"
        ));
    }
    Ok(def)
}

/// 白名单闸：子助理的名单必须是父话题工具面的子集——派出去的不能比父话题大。
/// 父话题没收窄（`allowlist` 为 `None`）= 它有全集，判据就落在定义自己的名单上
pub fn narrowed_for_chat(parent: &[String], asked: &[String]) -> Result<(), String> {
    if parent.is_empty() {
        return Ok(());
    }
    for tool in asked {
        if !parent.iter().any(|held| held == tool) {
            return Err(format!(
                "子助理要了「{tool}」，可这个话题的工具面上没有它：派出去的不能比父话题大。"
            ));
        }
    }
    Ok(())
}

/// 聊天可派的那一批（名字 + 描述）。声明侧拿它把 schema 的描述与 enum 写成真名单。
/// 内置名册让名单**永不为空**——spawn_subagent 因此默认声明（出厂即能派是这功能的
/// 意义所在）；仍受两道既有闸管：`disabled_tools` 关掉就不声明，没绑项目也不声明
pub fn spawnable_catalog(config: &AppConfig) -> Vec<(String, String)> {
    merged_catalog(config)
        .iter()
        .filter(|def| def.chat_spawnable && !def.name.trim().is_empty())
        .map(|def| (def.name.clone(), def.description.clone()))
        .collect()
}

/// 派一发：开子话题、按定义的能力面与连接跑一轮、把最后一条 assistant 正文交回来。
/// 父话题 id 由执行点（chat 循环）交给它——模型不给、也不该让它给
pub fn run_from_chat(
    app: &AppHandle,
    parent_conversation_id: &str,
    config: &AppConfig,
    args: &Value,
) -> Result<String, String> {
    let task = args["task"].as_str().unwrap_or_default();
    if task.trim().is_empty() {
        return Err("任务描述是空的：子助理不知道要干什么。".into());
    }
    let catalog = merged_catalog(config);
    let def = spawnable(&catalog, args["name"].as_str().unwrap_or_default())?;
    let parent_tools = crate::tool_runtime::allowlist(parent_conversation_id).unwrap_or_default();
    narrowed_for_chat(&parent_tools, &def.tools)?;

    let conversation_id = format!(
        "spawn-{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let now = crate::session::now_millis();
    crate::history::save_and_index(
        app,
        crate::history::Conversation {
            id: conversation_id.clone(),
            project_id: config.active_project_id.clone(),
            title: format!("子助理 · {}", def.name),
            created_at: now,
            updated_at: now,
            pinned: false,
            kind: "chat".to_string(),
            messages: Vec::new(),
            usage: None,
            video_nodes: Vec::new(),
            video_edges: Vec::new(),
        },
    )
    .map_err(|error| format!("子助理的话题没建起来：{error}"))?;

    // Worktree 继承：父话题挂在独立工作树上时，子助理在同一棵树上干活——
    // 不然父在树上写、子回原目录读，两边说的不是同一个项目
    crate::worktree::inherit(app, parent_conversation_id, &conversation_id);

    // 第一句是定义里的角色行（你是谁、这一支只负责什么），随后才是这一发的任务
    let prompt = format!("{}\n\n{}", def.system_prompt.trim(), task.trim());
    crate::chat::run_background_turn(
        app,
        &conversation_id,
        &prompt,
        Some(def.tools.as_slice()),
        // 轮数跟全局上限：子助理的"小"由白名单与任务本身兜着，不另设一格
        None,
        def.model_override().as_deref(),
        def.endpoint_override().as_deref(),
    )?;

    // 产出 = 子话题最后一条 assistant 正文（与任务图的 upstream_answers 同一条读法）。
    // 工具调用与思考过程不算：父话题要的是"它得出了什么"，不是它中间敲了哪些命令
    let conversation = crate::history::load_current(app, &conversation_id)?;
    let answer = conversation
        .messages
        .iter()
        .filter(|row| row.role == "assistant")
        .last()
        .ok_or_else(|| {
            format!(
                "子助理「{}」没有交回正文（话题 {conversation_id}）：可能卡在待审批或中途失败，可以去话题列表看它的现场。",
                def.name
            )
        })?;
    Ok(format!(
        "〔子助理 {} · 话题 {}〕\n{}",
        def.name, conversation_id, answer.content
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(name: &str, tools: &[&str], spawnable_flag: bool) -> SubagentDef {
        SubagentDef {
            name: name.into(),
            description: format!("{name} 的描述"),
            system_prompt: "你是这一支的执行者。".into(),
            tools: tools.iter().map(|item| item.to_string()).collect(),
            endpoint_profile_id: String::new(),
            model: String::new(),
            orchestration_assignable: false,
            chat_spawnable: spawnable_flag,
        }
    }

    /// 名单的判据：查不到、没开放、名字空白，三种都拒且各自说出为什么
    #[test]
    fn a_spawn_must_name_a_listed_and_spawnable_subagent() {
        let catalog = vec![def("审查员", &["read_file"], true), def("写手", &[], false)];
        assert!(spawnable(&catalog, "审查员").is_ok());
        assert!(spawnable(&catalog, "  审查员  ").is_ok(), "名字两侧的空白不算笔误");
        assert!(spawnable(&catalog, "写手").is_err(), "没标「主模型可调」的不在可派名单");
        assert!(spawnable(&catalog, "").is_err());
        assert!(spawnable(&catalog, "陌生人").is_err());
    }

    /// 空名单放行（纯推理是意图），但父话题收窄过的面上没有的工具不许要
    #[test]
    fn the_child_never_exceeds_the_parent_tool_surface() {
        let parent = vec!["read_file".to_string(), "list_files".to_string()];
        assert!(narrowed_for_chat(&parent, &["read_file".to_string()]).is_ok());
        assert!(narrowed_for_chat(&parent, &[]).is_ok(), "纯推理子助理不碰工具，放行");
        let error = narrowed_for_chat(&parent, &["write_file".to_string()]).unwrap_err();
        assert!(error.contains("write_file"), "要点出是哪一项越权：{error}");
        // 父没收窄 = 全集，定义自己的名单说了算
        assert!(narrowed_for_chat(&[], &["write_file".to_string()]).is_ok());
    }

    /// 声明名单 = 出厂名册 + 「主模型可调」的自定义（名字空白的定义不进）。
    /// 自定义目录为空时名单也不空——spawn_subagent 默认声明，出厂即能派
    #[test]
    fn the_catalog_lists_builtins_plus_the_spawnable_custom_ones() {
        let mut config = AppConfig::default();
        config.subagents = vec![
            def("审查员", &["read_file"], true),
            def("写手", &[], false),
            def("   ", &[], true),
        ];
        let catalog = spawnable_catalog(&config);
        assert_eq!(catalog.len(), 10, "九个内置 + 一个可派自定义");
        assert!(catalog.iter().any(|(name, desc)| name == "审查员" && desc.contains("审查员")));
        assert!(!catalog.iter().any(|(name, _)| name == "写手"), "没标「主模型可调」的不进");
        for expected in [
            "general-purpose",
            "explore",
            "reviewer",
            "operator",
            "distill",
            "test-runner",
            "fixer",
            "ui-designer",
            "researcher",
        ] {
            assert!(
                catalog.iter().any(|(name, _)| name == expected),
                "出厂名册少了 {expected}"
            );
        }

        // 空自定义目录：名单仍不空（这是"spawn_subagent 默认声明"的守卫）
        let mut bare = AppConfig::default();
        bare.subagents.clear();
        assert_eq!(spawnable_catalog(&bare).len(), 9);
    }

    /// 撞名内置赢：用户自定义叫了出厂名，消费点只有一个——内置那份
    #[test]
    fn a_custom_def_colliding_with_a_builtin_name_is_ignored() {
        let mut config = AppConfig::default();
        config.subagents = vec![def("explore", &[], true)];
        let merged = merged_catalog(&config);
        let explores: Vec<&SubagentDef> = merged
            .iter()
            .filter(|def| def.name == "explore")
            .collect();
        assert_eq!(explores.len(), 1, "撞名的定义必须被无视，不能出现两份");
        assert!(
            explores[0].description.contains("只读侦察"),
            "留下的必须是内置那份：{}",
            explores[0].description
        );
    }

    /// 覆盖项：停用从名单摘掉；模型/服务商套进定义；没命中的名字安静无视
    #[test]
    fn overrides_disable_and_retarget_builtins() {
        let mut config = AppConfig::default();
        config.subagent_overrides = vec![
            SubagentOverride {
                name: "explore".into(),
                endpoint_profile_id: String::new(),
                model: "deepseek-v4".into(),
                disabled: false,
            },
            SubagentOverride {
                name: "operator".into(),
                endpoint_profile_id: String::new(),
                model: String::new(),
                disabled: true,
            },
            SubagentOverride {
                name: "已经下线的角色".into(),
                endpoint_profile_id: "profile_x".into(),
                model: "ghost".into(),
                disabled: false,
            },
        ];
        let merged = merged_catalog(&config);
        let explore = merged.iter().find(|def| def.name == "explore").unwrap();
        assert_eq!(explore.model, "deepseek-v4", "覆盖的模型要套进定义");
        assert!(!merged.iter().any(|def| def.name == "operator"), "停用的不进可派名册");
        // 未知名字：不影响任何内置，也不报错
        assert_eq!(merged.len(), 8, "停用一个 + 未命中的覆盖被无视：名册剩八个");

        let catalog = spawnable_catalog(&config);
        assert_eq!(catalog.len(), 8, "可派名单里同样没有 operator");

        // 设置页视图含停用的那张卡（置灰展示，不是消失），且模型读到覆盖值
        let views = builtin_views(&config);
        assert_eq!(views.len(), 9);
        let operator = views.iter().find(|view| view.def.name == "operator").unwrap();
        assert!(operator.disabled);
        let explore_view = views.iter().find(|view| view.def.name == "explore").unwrap();
        assert!(!explore_view.disabled);
        assert_eq!(explore_view.def.model, "deepseek-v4");
    }

    /// 内置名单必须是注册表里的真工具，且不带 spawn_subagent（嵌套闸落在白名单本身）
    #[test]
    fn builtin_tool_lists_cite_real_tools_and_never_spawn() {
        for def in builtin_subagents() {
            assert!(
                !def.tools.iter().any(|tool| tool == "spawn_subagent"),
                "内置 {} 不该会把别的子助理派出去",
                def.name
            );
            for tool in &def.tools {
                assert!(
                    crate::tools::is_registered(tool),
                    "内置 {} 要了注册表里没有的「{tool}」",
                    def.name
                );
            }
        }
        // 纯推理位：distill 空名单是意图，派单闸对空名单放行
        let distill = builtin_subagents()
            .into_iter()
            .find(|def| def.name == "distill")
            .unwrap();
        assert!(distill.tools.is_empty());
        assert!(narrowed_for_chat(&["list_files".to_string()], &distill.tools).is_ok());
    }
}
