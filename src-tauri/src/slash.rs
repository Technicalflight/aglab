//! Slash 命令：输入框里 `/` 开头的快捷入口。
//!
//! 三个来源：
//! 1. **内置**——定义在这里，执行在前端。它们多半是现有 IPC 与界面跳转的糖
//!    （开新对话、压缩上下文、跳用量页），不属于能力面，所以不走工具注册表；
//! 2. **个人目录** `app_data/commands/*.md`——跨项目可用的自定义命令；
//! 3. **项目目录** `<项目根>/.aglab/commands/*.md`——跟仓库走的自定义命令。
//!
//! 自定义命令的形状照搬 SKILL.md 的轻 frontmatter：文件名（去 .md）即命令名，
//! `description` 一行说明，`argument-hint` 提示参数怎么填；正文是模板，
//! `$ARGUMENTS` 在发送前替换成用户敲的参数。目录扫描每次发送/按键时现读：
//! 命令不是声明给模型的工具，没有"首轮定形"的约束，改了文件下一发就生效。

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::config;

/// 内置命令的动作 id。前端按它分发；带 `prompt` 的是"展开成一段话再发送"，
/// 不带的是纯界面动作。
pub const BUILTIN_ACTIONS: &[(&str, &str, &str)] = &[
    ("new", "新对话", "开一场新话题"),
    ("compact", "压缩上下文", "把这段历史压成摘要，腾出窗口"),
    ("export", "导出 Markdown", "把当前话题导出成 Markdown 文件"),
    ("init", "生成 AGENTS.md", "让模型勘察项目并写一份 AGENTS.md"),
    ("usage", "打开用量", "跳到 设置 → 用量"),
    ("review", "变更请求", "打开 Git 变更审查页"),
    ("tasks", "定时任务", "打开定时任务页"),
    ("knowledge", "资料库", "打开资料库页"),
    ("plugins", "插件", "打开插件页"),
    ("mcp", "MCP 服务器", "查看运行中的 MCP 服务器，启停与重连"),
    ("settings", "设置", "打开设置页"),
];

/// /init 的展开稿：让模型勘察项目、写 AGENTS.md。走的是正常发送链路，
/// 写文件照常过审批——这是设计好的，不是漏了。
pub const INIT_PROMPT: &str = "请为当前项目生成一份 AGENTS.md，写到项目根目录。先看目录结构与关键配置（包管理器、构建/测试/lint 命令、代码风格线索），再写这几节：项目一句话简介；常用命令（安装/构建/测试/检查）；代码约定；目录导览。只写有依据的内容，不要编造。若 AGENTS.md 已存在，先读它，在其基础上补充修订，不要整篇推翻。";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SlashCommand {
    /// 不带斜杠的命令名：内置是动作 id，自定义是文件名（去 .md）
    pub name: String,
    /// 给菜单看的一行说明
    pub title: String,
    /// 参数提示（可省）
    pub argument_hint: Option<String>,
    /// 来源标签：builtin / user / project / 插件 id
    pub source: String,
    /// 内置动作 id；None = 自定义命令（发送时展开正文模板）
    pub action: Option<String>,
    /// 内置提示词（/init）：发送时把 $ARGUMENTS 换成参数后整段发出
    pub prompt: Option<String>,
}

/// 读一个 .md 命令文件：文件名即命令名，frontmatter 只认 description 与 argument-hint，
/// 其余行原样留在正文里。解析失败不炸——命令文件是用户手写的，宽容读取。
fn parse_command_file(path: &Path) -> Option<SlashCommand> {
    let text = fs::read_to_string(path).ok()?;
    let name = path.file_stem()?.to_string_lossy().into_owned();
    if name.is_empty() {
        return None;
    }

    let mut description = String::new();
    let mut argument_hint: Option<String> = None;
    let mut body = text.as_str();

    // 轻 frontmatter：首行是 --- 时只扫到下一个 --- 为止；不认YAML结构，
    // 只挑这两个已知键。没有 frontmatter 的文件整个正文当模板
    if let Some(rest) = text.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            let front = &rest[..end];
            body = rest[end + 4..].trim_start_matches('\n');
            for line in front.lines() {
                let Some((key, value)) = line.split_once(':') else {
                    continue;
                };
                let value = value
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string();
                match key.trim() {
                    "description" => description = value,
                    "argument-hint" => argument_hint = Some(value).filter(|v| !v.is_empty()),
                    _ => {}
                }
            }
        }
    }
    let body = body.trim();
    if body.is_empty() {
        return None;
    }

    Some(SlashCommand {
        name,
        title: if description.is_empty() {
            "自定义命令".into()
        } else {
            description
        },
        argument_hint,
        source: String::new(), // 由调用方按目录填
        action: None,
        prompt: Some(body.to_string()),
    })
}

fn commands_in(dir: &Path, source: &str) -> Vec<SlashCommand> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("md") {
            continue;
        }
        if let Some(mut command) = parse_command_file(&path) {
            command.source = source.to_string();
            out.push(command);
        }
    }
    // 同名时后扫的目录不覆盖先扫的：调用方按 user → project → 插件 的顺序喂进来，
    // 项目里那份和插件里那份重名时，更"近"的来源排在前面才算数
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn dedup_by_name(mut commands: Vec<SlashCommand>) -> Vec<SlashCommand> {
    let mut seen = std::collections::HashSet::new();
    commands.retain(|command| seen.insert(command.name.clone()));
    commands
}

/// 一次按键就该出菜单：这条 IPC 要快，只做目录扫描与名字去重，不做搜索。
#[tauri::command]
pub fn slash_commands_list(app: AppHandle) -> Result<Vec<SlashCommand>, String> {
    let mut commands: Vec<SlashCommand> = BUILTIN_ACTIONS
        .iter()
        .map(|(name, title, _hint)| SlashCommand {
            name: (*name).to_string(),
            title: (*title).to_string(),
            argument_hint: None,
            source: "builtin".into(),
            action: Some((*name).to_string()),
            prompt: (*name == "init").then(|| INIT_PROMPT.to_string()),
        })
        .collect();

    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    commands.extend(commands_in(&data_dir.join("commands"), "user"));

    let config = config::load(&app);
    if let Some(project) = config.active_project() {
        commands.extend(commands_in(
            &PathBuf::from(&project.path).join(".aglab").join("commands"),
            "project",
        ));
    }
    // 插件带的命令：与 Claude Code 的 plugins/<id>/commands/ 同一形状。
    // 插件页现在只消费技能与钩子，命令先从这里接上——列出即可用
    for plugin in crate::plugins::enabled(&app) {
        let source = plugin.id.clone();
        commands.extend(commands_in(&plugin.path.join("commands"), &source));
    }

    Ok(dedup_by_name(commands))
}

/// `$ARGUMENTS` / `$1..$9` 的替换住前端（发送那一刻它手上才有草稿里的参数）；
/// 这里只负责把模板原样发下去，替换语义由前端的 vitest 钉住。
#[cfg(test)]
mod tests {
    use super::*;

    fn write_command(dir: &Path, name: &str, body: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(format!("{name}.md")), body).unwrap();
    }

    #[test]
    fn parses_frontmatter_and_keeps_body_as_template() {
        let base = crate::test_support::temp_dir("slash-parse");
        write_command(
            &base,
            "fix-issue",
            "---\ndescription: 修一个 issue\nargument-hint: issue 编号\n---\n请修复 $ARGUMENTS，改完跑测试。",
        );
        let commands = commands_in(&base, "user");
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "fix-issue");
        assert_eq!(commands[0].title, "修一个 issue");
        assert_eq!(commands[0].argument_hint.as_deref(), Some("issue 编号"));
        assert_eq!(
            commands[0].prompt.as_deref(),
            Some("请修复 $ARGUMENTS，改完跑测试。")
        );
        assert_eq!(commands[0].source, "user");
        crate::test_support::remove_tree(&base);
    }

    #[test]
    fn a_body_without_frontmatter_is_the_whole_template() {
        let base = crate::test_support::temp_dir("slash-nofm");
        write_command(&base, "review", "逐文件审查当前 diff。");
        let commands = commands_in(&base, "user");
        assert_eq!(
            commands[0].title, "自定义命令",
            "没有 description 就给个兜底，别空着"
        );
        assert_eq!(commands[0].argument_hint, None);
        assert_eq!(commands[0].prompt.as_deref(), Some("逐文件审查当前 diff。"));
        crate::test_support::remove_tree(&base);
    }

    #[test]
    fn empty_bodies_and_non_md_files_do_not_become_commands() {
        let base = crate::test_support::temp_dir("slash-empty");
        write_command(&base, "ghost", "---\ndescription: 空壳\n---\n");
        write_command(&base, "notes", "");
        fs::write(base.join("plain.txt"), "不是命令").unwrap();
        assert!(commands_in(&base, "user").is_empty());
        crate::test_support::remove_tree(&base);
    }

    #[test]
    fn duplicates_keep_the_earlier_source() {
        let early = SlashCommand {
            name: "ship".into(),
            title: "个人版".into(),
            argument_hint: None,
            source: "user".into(),
            action: None,
            prompt: Some("个人版".into()),
        };
        let late = SlashCommand {
            name: "ship".into(),
            title: "项目版".into(),
            argument_hint: None,
            source: "project".into(),
            action: None,
            prompt: Some("项目版".into()),
        };
        let merged = dedup_by_name(vec![early, late]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].source, "user");
    }

    #[test]
    fn builtin_actions_carry_their_ids_and_init_carries_its_prompt() {
        let names: Vec<&str> = BUILTIN_ACTIONS.iter().map(|(name, _, _)| *name).collect();
        assert!(names.contains(&"init"));
        assert!(INIT_PROMPT.contains("AGENTS.md"));
    }
}
