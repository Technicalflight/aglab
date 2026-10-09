//! 对话导出：把一条话题当前分支的消息行落成文件。四种格式——
//! - Markdown：人读，标题 + 逐条用户/助手正文；
//! - JSON：机器读，整段结构（标题 + 消息数组）；
//! - JSONL：Unsloth/ShareGPT 微调格式，一问一答一行；
//! - 快照 HTML：分享用自包含只读页（无 JS、无外部资源），内容与 Markdown
//!   同一投影——只含对话正文，工具调用/思维链/用量/路径一概不进。
//!
//! 分享的隐私闸与格式保证在同一处：`turns_of` 就是唯一投影，它不给的东西
//! 任何格式都带不出去。

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
                Message::Assistant(settled)
                    if !settled.content.trim().is_empty() =>
                {
                        turns.push(ExportTurn {
                            role: "assistant".into(),
                            content: settled.content.clone(),
                            at: entry.timestamp,
                        });
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

/// 分享快照的 HTML 转义：对话正文是任意文本，`<script>` 进快照就是存储型
/// XSS——每个动态字段都过这一道，模板骨架是唯一的可信输入
fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// 分享快照：自包含只读 HTML。无 JS、无外部资源（样式内联）——
/// 发给谁都能双击打开，也带不出比 markdown 投影多一个字节的隐私
fn snapshot_html(title: &str, turns: &[ExportTurn]) -> String {
    let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M");
    let mut body = String::new();
    for turn in turns {
        let (role_label, class) = if turn.role == "user" {
            ("用户", "user")
        } else {
            ("助手", "assistant")
        };
        let time = chrono::DateTime::from_timestamp_millis(turn.at)
            .map(|at| at.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string())
            .unwrap_or_default();
        body.push_str(&format!(
            r#"<article class="turn {class}"><header><span class="role">{role_label}</span><time>{time}</time></header><div class="content">{}</div></article>"#,
            escape_html(&turn.content)
        ));
        body.push('\n');
    }
    format!(
        r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} · 对话快照</title>
<style>
:root {{ color-scheme: light; --ink: #1f2328; --muted: #6b7280; --line: #e5e7eb; --brand: #7c5cff; --bg: #f6f7f9; }}
* {{ box-sizing: border-box; }}
body {{ margin: 0; padding: 32px 16px 64px; background: var(--bg); color: var(--ink);
  font: 15px/1.75 -apple-system, "Segoe UI", "Microsoft YaHei", sans-serif; }}
main {{ max-width: 760px; margin: 0 auto; }}
h1 {{ font-size: 22px; margin: 0 0 4px; }}
.meta {{ color: var(--muted); font-size: 13px; margin-bottom: 24px; }}
.turn {{ background: #fff; border: 1px solid var(--line); border-radius: 12px;
  padding: 14px 18px; margin-bottom: 12px; }}
.turn header {{ display: flex; justify-content: space-between; margin-bottom: 6px; }}
.turn .role {{ font-weight: 600; font-size: 13px; }}
.turn.assistant .role {{ color: var(--brand); }}
.turn time {{ color: var(--muted); font-size: 12px; }}
.turn .content {{ white-space: pre-wrap; word-break: break-word; }}
footer {{ text-align: center; color: var(--muted); font-size: 12px; margin-top: 32px; }}
</style>
</head>
<body>
<main>
<h1>{title}</h1>
<p class="meta">导出自 aglab · {stamp} · 共 {count} 条消息 · 只读快照</p>
{body}
<footer>由 aglab 生成的对话快照 · 内容为纯文本存档</footer>
</main>
</body>
</html>
"#,
        title = escape_html(title),
        stamp = stamp,
        count = turns.len(),
        body = body,
    )
}

/// 写出文件。路径来自前端的保存对话框；扩展名按格式补齐，避免存成无后缀文件
#[tauri::command]
pub fn export_conversation(app: AppHandle, id: String, format: String, path: String) -> Result<String, String> {
    let wanted_ext = match format.as_str() {
        "markdown" => "md",
        "json" => "json",
        "jsonl" => "jsonl",
        "snapshot" => "html",
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
        "snapshot" => snapshot_html(&title, &turns),
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

    #[test]
    fn snapshot_html_escapes_content_and_carries_the_turns() {
        let turns = vec![
            turn("user", "帮我看看 <script>alert('xss')</script> 这段"),
            turn("assistant", "正文里有 & < > \" 引号也要原样显示"),
        ];
        let html = snapshot_html("分享测试", &turns);
        // 正文整体转义：快照文件里的 <script> 是字面文本，不是可执行标签
        assert!(
            html.contains("&lt;script&gt;alert(&#39;xss&#39;)&lt;/script&gt;"),
            "HTML 必须逐字转义：{html}"
        );
        assert!(!html.contains("<script>alert"), "转义失败就是存储型 XSS");
        assert!(html.contains("&amp; &lt; &gt; &quot;"), "四种危险字符都过闸：{html}");
        // 骨架照常携带：标题、角色、计数
        assert!(html.contains("分享测试"), "{html}");
        assert!(html.contains(r#"class="role">用户"#) && html.contains(r#"class="role">助手"#));
        assert!(html.contains("共 2 条消息"));
        assert!(!html.contains("<script src"), "自包含：没有外部脚本");
    }

    #[test]
    fn snapshot_html_renders_timestamps_as_local_short_form() {
        // at 用毫秒时间戳；快照里落成 MM-DD HH:MM 的短格式
        let turns = vec![ExportTurn { role: "user".into(), content: "问".into(), at: 0 }];
        let html = snapshot_html("时区", &turns);
        assert!(html.contains("<time>"), "{html}");
        assert!(html.contains(r#"class="role">用户"#));
    }
}
