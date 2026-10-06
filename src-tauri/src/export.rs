//! 对话导出：把一条话题当前分支的消息行落成文件。三种格式——
//! - Markdown：人读，标题 + 逐条用户/助手正文；
//! - JSON：机器读，整段结构（标题 + 消息数组）；
//! - JSONL：Unsloth/ShareGPT 微调格式，一问一答一行。
//!
//! 只导消息行的正文（用户 + 助手 settled 内容）；工具调用、思维链、
//! 用量这些过程记录不是"对话内容"，不进导出物。

use serde::Serialize;
use tauri::AppHandle;

use crate::session::entry::{EntryPayload, Message};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportTurn {
    role: String,
    content: String,
    at: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportDocument<'a> {
    title: &'a str,
    exported_at: i64,
    messages: &'a [ExportTurn],
}

/// 标题取台账；台账没落地（新话题）就退回"对话"。
/// 只收当前分支（从 tip 沿父链到根），分叉被丢弃的行不出现
fn turns_of(app: &AppHandle, id: &str) -> Result<(String, Vec<ExportTurn>), String> {
    let ledger = crate::history::current(app)
        .and_then(|store| store.load(id))
        .ok()
        .filter(|conversation| !conversation.title.trim().is_empty());
    let source = crate::chat::open_session(app, id)?;
    let mut turns: Vec<ExportTurn> = Vec::new();
    for entry in source.log.path().map_err(|e| e.to_string())? {
        if let EntryPayload::Message { message } = entry.payload() {
            match message {
                Message::User { content, .. } => {
                    if !content.trim().is_empty() {
                        turns.push(ExportTurn {
                            role: "user".into(),
                            content: content.clone(),
                            at: entry.timestamp,
                        });
                    }
                }
                Message::Assistant(settled) => {
                    if !settled.content.trim().is_empty() {
                        turns.push(ExportTurn {
                            role: "assistant".into(),
                            content: settled.content.clone(),
                            at: entry.timestamp,
                        });
                    }
                }
                _ => {}
            }
        }
    }
    Ok((ledger.map(|c| c.title).unwrap_or_else(|| "对话".into()), turns))
}

fn markdown(title: &str, turns: &[ExportTurn]) -> String {
    let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M");
    let mut out = format!(
        "# {title}\n\n> 导出自 aglab · {stamp} · 共 {} 条消息\n",
        turns.len()
    );
    for turn in turns {
        let role = if turn.role == "user" { "用户" } else { "助手" };
        out.push_str(&format!("\n---\n\n### {role}\n\n{}\n", turn.content));
    }
    out
}

/// 一问一答一行。孤悬的用户消息（后面没接上助手回复的）不成对，跳过
fn jsonl(turns: &[ExportTurn]) -> String {
    let mut out = String::new();
    let mut pending: Option<&ExportTurn> = None;
    for turn in turns {
        if turn.role == "user" {
            pending = Some(turn);
            continue;
        }
        if let Some(question) = pending.take() {
            let line = serde_json::json!({
                "conversations": [
                    { "from": "human", "value": question.content },
                    { "from": "gpt", "value": turn.content },
                ]
            });
            out.push_str(&line.to_string());
            out.push('\n');
        }
    }
    out
}

/// 写出文件。路径来自前端的保存对话框；扩展名按格式补齐，避免存成无后缀文件
#[tauri::command]
pub fn export_conversation(app: AppHandle, id: String, format: String, path: String) -> Result<String, String> {
    let wanted_ext = match format.as_str() {
        "markdown" => "md",
        "json" => "json",
        "jsonl" => "jsonl",
        other => return Err(format!("不认识的导出格式：{other}")),
    };
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("没有选保存位置。".into());
    }
    let mut target = std::path::PathBuf::from(trimmed);
    let extension_ok = target
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case(wanted_ext));
    if !extension_ok {
        target.set_extension(wanted_ext);
    }
    let (title, turns) = turns_of(&app, &id)?;
    if turns.is_empty() {
        return Err("这条对话还没有可导出的消息。".into());
    }
    let body = match format.as_str() {
        "markdown" => markdown(&title, &turns),
        "json" => {
            let document = ExportDocument {
                title: &title,
                exported_at: crate::session::now_millis(),
                messages: &turns,
            };
            serde_json::to_string_pretty(&document).map_err(|e| format!("{e}"))?
        }
        _ => jsonl(&turns),
    };
    std::fs::write(&target, body).map_err(|e| format!("写入失败：{e}"))?;
    Ok(target.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(role: &str, content: &str) -> ExportTurn {
        ExportTurn { role: role.into(), content: content.into(), at: 0 }
    }

    #[test]
    fn markdown_carries_title_counts_and_roles() {
        let turns = vec![turn("user", "怎么打窝？"), turn("assistant", "发酵玉米，钓远不钓近。")];
        let text = markdown("钓鱼笔记", &turns);
        assert!(text.starts_with("# 钓鱼笔记\n"));
        assert!(text.contains("共 2 条消息"));
        assert!(text.contains("### 用户\n\n怎么打窝？"));
        assert!(text.contains("### 助手\n\n发酵玉米"));
    }

    #[test]
    fn jsonl_pairs_question_with_answer_and_skips_dangling_users() {
        let turns = vec![
            turn("user", "第一问"),
            turn("assistant", "第一答"),
            turn("user", "孤悬的一问"),
        ];
        let text = jsonl(&turns);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1, "{text}");
        let parsed: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed["conversations"][0]["from"], "human");
        assert_eq!(parsed["conversations"][0]["value"], "第一问");
        assert_eq!(parsed["conversations"][1]["from"], "gpt");
        assert_eq!(parsed["conversations"][1]["value"], "第一答");
    }

    #[test]
    fn jsonl_passes_through_multi_turns_in_order() {
        let turns = vec![
            turn("user", "A"),
            turn("assistant", "B"),
            turn("user", "C"),
            turn("assistant", "D"),
        ];
        let parsed: serde_json::Value = serde_json::from_str(jsonl(&turns).lines().nth(1).unwrap()).unwrap();
        assert_eq!(parsed["conversations"][0]["value"], "C");
        assert_eq!(parsed["conversations"][1]["value"], "D");
    }
}
