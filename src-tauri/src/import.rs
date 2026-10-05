//! 从其他 AI 应用导入项目与话题。
//!
//! 目前支持 Claude Code（~/.claude/projects/**.jsonl）、Codex（~/.codex/sessions/**.jsonl）、
//! Gemini CLI（~/.gemini/tmp/**/chats/*.json）、Qwen Code（~/.qwen/tmp/**/chats/*.json，同构）
//! 与 OpenCode（~/.local/share/opencode 的 session/message/part 三棵目录树聚合）。
//! 源文件只读：导入是把解析出的文本对话拷进 aglab 自己的话题存储，源数据一个字节都不动。
//! 工具调用与执行结果不导——那两样离开原 harness 语义就断了，硬导过来只会误导。

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;
use tauri::AppHandle;

use crate::config::{self, AppConfig};
use crate::history;

/// 一条话题保留的正文上限：超出就丢尾巴，导入是恢复语境，不是全量迁移
const MAX_CHARS_PER_MESSAGE: usize = 32_000;
const MAX_MESSAGES_PER_CONVERSATION: usize = 500;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportSource {
    /// "claude" | "codex"，导入时原样传回
    pub kind: String,
    pub name: String,
    pub available: bool,
    /// "12 个项目 · 45 条话题" 或 "未检测到"
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportOutcome {
    pub projects: usize,
    pub imported: usize,
    pub skipped: usize,
    /// 汇总文案，直接给界面展示
    pub note: String,
}

fn home_dir() -> Result<PathBuf, String> {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .map_err(|_| "找不到主目录。".to_string())
}

fn iso_to_ms(raw: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|time| time.timestamp_millis())
}

fn file_mtime_ms(path: &Path) -> i64 {
    path.metadata()
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Claude Code 的 content 可能是字符串，也可能是分段数组（正文/工具调用混在一起），
/// 这里只取纯文本分段——工具调用离开原环境没有意义
fn text_of_content(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    if let Some(items) = content.as_array() {
        return items
            .iter()
            .filter(|item| item["type"].as_str() == Some("text"))
            .filter_map(|item| item["text"].as_str())
            .collect::<Vec<_>>()
            .join("");
    }
    String::new()
}

fn clamp_message(text: &str) -> String {
    if text.chars().count() <= MAX_CHARS_PER_MESSAGE {
        return text.to_string();
    }
    let cut: String = text.chars().take(MAX_CHARS_PER_MESSAGE).collect();
    format!("{cut}\n\n（内容过长，导入时截断）")
}

/// 检测可导入的来源。只数文件，不做解析——解析留给点"导入"那一刻
#[tauri::command]
pub fn import_scan() -> Result<Vec<ImportSource>, String> {
    let home = home_dir()?;

    let claude_projects = home.join(".claude").join("projects");
    let (claude_detail, claude_ok) = if claude_projects.is_dir() {
        let mut sessions = 0usize;
        let mut projects = 0usize;
        if let Ok(entries) = fs::read_dir(&claude_projects) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    projects += 1;
                    sessions += count_files(&entry.path(), "jsonl");
                }
            }
        }
        (
            format!("{projects} 个项目 · {sessions} 条话题"),
            sessions > 0,
        )
    } else {
        ("未检测到".into(), false)
    };

    let codex_sessions = home.join(".codex").join("sessions");
    let (codex_detail, codex_ok) = if codex_sessions.is_dir() {
        let sessions = count_files_recursive(&codex_sessions, "jsonl");
        (
            format!("{sessions} 条话题"),
            sessions > 0,
        )
    } else {
        ("未检测到".into(), false)
    };

    // Gemini CLI 与 Qwen Code 同构：~/.gemini（或 ~/.qwen）/tmp/<项目hash>/chats/*.json
    let (gemini_detail, gemini_ok) = {
        let base = home.join(".gemini").join("tmp");
        if base.is_dir() {
            let sessions = count_gemini_style_sessions(&base);
            (format!("{sessions} 条话题"), sessions > 0)
        } else {
            ("未检测到".into(), false)
        }
    };
    let (qwen_detail, qwen_ok) = {
        let base = home.join(".qwen").join("tmp");
        if base.is_dir() {
            let sessions = count_gemini_style_sessions(&base);
            (format!("{sessions} 条话题"), sessions > 0)
        } else {
            ("未检测到".into(), false)
        }
    };

    let opencode_roots = opencode_storage_roots(&home);
    let (opencode_detail, opencode_ok) = if opencode_roots.is_empty() {
        ("未检测到".into(), false)
    } else {
        let sessions: usize = opencode_roots
            .iter()
            .map(|root| count_opencode_sessions(root))
            .sum();
        (format!("{sessions} 条话题"), sessions > 0)
    };

    Ok(vec![
        ImportSource {
            kind: "claude".into(),
            name: "Claude Code".into(),
            available: claude_ok,
            detail: claude_detail,
        },
        ImportSource {
            kind: "codex".into(),
            name: "Codex".into(),
            available: codex_ok,
            detail: codex_detail,
        },
        ImportSource {
            kind: "gemini".into(),
            name: "Gemini CLI".into(),
            available: gemini_ok,
            detail: gemini_detail,
        },
        ImportSource {
            kind: "qwen".into(),
            name: "Qwen Code".into(),
            available: qwen_ok,
            detail: qwen_detail,
        },
        ImportSource {
            kind: "opencode".into(),
            name: "OpenCode".into(),
            available: opencode_ok,
            detail: opencode_detail,
        },
    ])
}

fn count_files(dir: &Path, extension: &str) -> usize {
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| {
                    entry.path().extension().and_then(|ext| ext.to_str()) == Some(extension)
                })
                .count()
        })
        .unwrap_or(0)
}

fn count_files_recursive(dir: &Path, extension: &str) -> usize {
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| {
                    let path = entry.path();
                    if path.is_dir() {
                        count_files_recursive(&path, extension)
                    } else if path.extension().and_then(|ext| ext.to_str()) == Some(extension) {
                        1
                    } else {
                        0
                    }
                })
                .sum()
        })
        .unwrap_or(0)
}

/// 把一个项目路径并进 aglab 的项目列表（按路径去重，不改变当前选中的项目）。
/// 返回该项目在 aglab 侧的 id。
fn ensure_project(config: &mut AppConfig, path: &str) -> String {
    if let Some(existing) = config.projects.iter().find(|project| project.path == path) {
        return existing.id.clone();
    }
    let name = Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    let id = config::new_project_id();
    config.projects.push(config::Project {
        id: id.clone(),
        name,
        path: path.to_string(),
        ..Default::default()
    });
    id
}

/// 解析 Claude Code 的一个话题文件：一行一条记录，取 user/assistant 的纯文本
fn parse_claude_file(path: &Path) -> Option<(Option<String>, String, i64, i64, Vec<(String, String)>)> {
    let text = fs::read_to_string(path).ok()?;
    let mut cwd: Option<String> = None;
    let mut messages: Vec<(String, String)> = Vec::new();
    let mut earliest = i64::MAX;
    let mut latest = 0i64;

    for line in text.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        // summary/system/meta 行没有可导的对话正文
        let role = record["message"]["role"].as_str().unwrap_or_default().to_string();
        if role != "user" && role != "assistant" {
            continue;
        }
        if record["isMeta"].as_bool() == Some(true) {
            continue;
        }
        let content = text_of_content(&record["message"]["content"]);
        if content.trim().is_empty() {
            continue;
        }
        if cwd.is_none() {
            if let Some(value) = record["cwd"].as_str() {
                cwd = Some(value.to_string());
            }
        }
        let ts = record["timestamp"]
            .as_str()
            .and_then(iso_to_ms)
            .unwrap_or(0);
        if ts > 0 {
            earliest = earliest.min(ts);
            latest = latest.max(ts);
        }
        messages.push((role, content));
    }

    if messages.is_empty() {
        return None;
    }
    Some((
        cwd,
        messages[0].1.chars().take(40).collect(),
        if earliest == i64::MAX { 0 } else { earliest },
        latest,
        messages,
    ))
}

/// 解析 Codex 的一个 rollout 文件：首行 session_meta 带 cwd，正文是 response_item
fn parse_codex_file(path: &Path) -> Option<(Option<String>, String, i64, i64, Vec<(String, String)>)> {
    let text = fs::read_to_string(path).ok()?;
    let mut cwd: Option<String> = None;
    let mut messages: Vec<(String, String)> = Vec::new();
    let mut earliest = i64::MAX;
    let mut latest = 0i64;

    for line in text.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if record["type"].as_str() == Some("session_meta") {
            if let Some(value) = record["payload"]["cwd"].as_str() {
                cwd = Some(value.to_string());
            }
            continue;
        }
        if record["type"].as_str() != Some("response_item") {
            continue;
        }
        let payload = &record["payload"];
        if payload["type"].as_str() != Some("message") {
            continue;
        }
        let role = payload["role"].as_str().unwrap_or_default().to_string();
        if role != "user" && role != "assistant" {
            continue;
        }
        let content = payload["content"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter(|part| {
                        matches!(
                            part["type"].as_str(),
                            Some("input_text") | Some("output_text")
                        )
                    })
                    .filter_map(|part| part["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();
        if content.trim().is_empty() {
            continue;
        }
        let ts = record["timestamp"]
            .as_str()
            .and_then(iso_to_ms)
            .unwrap_or(0);
        if ts > 0 {
            earliest = earliest.min(ts);
            latest = latest.max(ts);
        }
        messages.push((role, content));
    }

    if messages.is_empty() {
        return None;
    }
    Some((
        cwd,
        messages[0].1.chars().take(40).collect(),
        if earliest == i64::MAX { 0 } else { earliest },
        latest,
        messages,
    ))
}

fn conversation_id(prefix: &str, stem: &str) -> String {
    // 外部 id 直接拼前缀，不会与 aglab 自生成的 conv_/msg_ 相撞
    format!("{prefix}-{stem}")
}

fn import_from_files(
    app: &AppHandle,
    files: &[(PathBuf, Option<String>, String, i64, i64, Vec<(String, String)>)],
    id_prefix: &str,
) -> ImportOutcome {
    let mut config = config::load(app);

    let mut projects = 0usize;
    let mut imported = 0usize;
    let mut skipped = 0usize;
    let mut failures = 0usize;

    for (path, cwd, first_line, earliest, latest, messages) in files {
        let id = conversation_id(id_prefix, &stem_lossy(path));
        // 已导入过的原样跳过：重复导入不该堆出两份一样的话题
        if history::conversation_exists(app, &id) {
            skipped += 1;
            continue;
        }

        let project_id = match cwd {
            Some(path) if Path::new(path).is_dir() => {
                let before = config.projects.len();
                let id = ensure_project(&mut config, path);
                if config.projects.len() > before {
                    projects += 1;
                }
                id
            }
            _ => String::new(),
        };

        let created = if *earliest > 0 { *earliest } else { file_mtime_ms(path) };
        let updated = if *latest > 0 { *latest } else { created };
        let messages: Vec<(String, String)> = messages
            .iter()
            .rev()
            .take(MAX_MESSAGES_PER_CONVERSATION)
            .rev()
            .map(|(role, content)| (role.clone(), clamp_message(content)))
            .collect();

        let conversation = history::Conversation {
            id: id.clone(),
            project_id,
            title: if first_line.trim().is_empty() {
                "导入的话题".into()
            } else {
                first_line.clone()
            },
            created_at: created,
            updated_at: updated,
            pinned: false,
            kind: "chat".to_string(),
            messages: messages
                .iter()
                .enumerate()
                .map(|(index, (role, content))| history::MessageRecord {
                    id: format!("{id}-m{index}"),
                    role: role.clone(),
                    content: content.clone(),
                    created_at: created + index as i64,
                    reasoning: None,
                    tool_calls: Vec::new(),
                    steps: Vec::new(),
                    error: None,
                    attachments: Vec::new(),
                    // 导入的那份台账没记过"这句是谁答的"，也不该拿现在的配置去补
                    model: None,
                    // 导入的是一份线性台账：它就是这条链，第一条才是根
                    parent_id: if index == 0 { None } else { Some(format!("{id}-m{}", index - 1)) },
                    entry_ids: Vec::new(),
                    goal_round: None,
                    node_id: None,
                    media: None,
                })
                .collect(),
            usage: None,
            video_nodes: Vec::new(),
            video_edges: Vec::new(),
        };

        match history::save_conversation(app, conversation) {
            Ok(_) => imported += 1,
            Err(_) => failures += 1,
        }
    }

    if projects > 0 {
        // 只有真的新增了项目才落 config，避免每次导入都白写一遍
        if let Err(error) = config::save(app, &config) {
            return ImportOutcome {
                projects: 0,
                imported,
                skipped,
                note: format!("项目列表写入失败：{error}；话题已导入 {imported} 条。"),
            };
        }
    }

    let mut note = format!("导入 {imported} 条话题");
    if projects > 0 {
        note.push_str(&format!("、新增 {projects} 个项目"));
    }
    if skipped > 0 {
        note.push_str(&format!("；跳过 {skipped} 条（已导入过）"));
    }
    if failures > 0 {
        note.push_str(&format!("；{failures} 条写入失败"));
    }
    note.push('。');

    ImportOutcome {
        projects,
        imported,
        skipped,
        note,
    }
}

fn stem_lossy(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// 从指定应用导入。解析失败的文件静默跳过——外部数据格式随版本漂移，
/// 一条坏行不值得让整批导入失败
#[tauri::command]
pub fn import_from_app(app: AppHandle, kind: String) -> Result<ImportOutcome, String> {
    let home = home_dir()?;
    let mut files: Vec<(PathBuf, Option<String>, String, i64, i64, Vec<(String, String)>)> =
        Vec::new();

    match kind.as_str() {
        "claude" => {
            let projects = home.join(".claude").join("projects");
            if !projects.is_dir() {
                return Err("没找到 Claude Code 的话题目录（应在 ~/.claude/projects）。".into());
            }
            for project_dir in fs::read_dir(&projects)
                .map_err(|e| format!("读 Claude Code 话题目录失败：{e}"))?
                .flatten()
            {
                let project_dir = project_dir.path();
                if !project_dir.is_dir() {
                    continue;
                }
                for entry in fs::read_dir(&project_dir)
                    .map_err(|e| format!("读话题文件失败：{e}"))?
                    .flatten()
                {
                    let path = entry.path();
                    if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                        continue;
                    }
                    if let Some(parsed) = parse_claude_file(&path) {
                        files.push((path, parsed.0, parsed.1, parsed.2, parsed.3, parsed.4));
                    }
                }
            }
            if files.is_empty() {
                return Err("Claude Code 的话题文件里没有可导入的对话。".into());
            }
            let outcome = import_from_files(&app, &files, "cc");
            Ok(outcome)
        }
        "codex" => {
            let sessions = home.join(".codex").join("sessions");
            if !sessions.is_dir() {
                return Err("没找到 Codex 的话题目录（应在 ~/.codex/sessions）。".into());
            }
            collect_jsonl(&sessions, &mut files, parse_codex_file);
            if files.is_empty() {
                return Err("Codex 的话题文件里没有可导入的对话。".into());
            }
            let outcome = import_from_files(&app, &files, "cx");
            Ok(outcome)
        }
        "gemini" => {
            let base = home.join(".gemini").join("tmp");
            if !base.is_dir() {
                return Err("没找到 Gemini CLI 的话题目录（应在 ~/.gemini/tmp）。".into());
            }
            collect_gemini_style(&base, &mut files);
            if files.is_empty() {
                return Err("Gemini CLI 的话题文件里没有可导入的对话。".into());
            }
            Ok(import_from_files(&app, &files, "gm"))
        }
        "qwen" => {
            let base = home.join(".qwen").join("tmp");
            if !base.is_dir() {
                return Err("没找到 Qwen Code 的话题目录（应在 ~/.qwen/tmp）。".into());
            }
            collect_gemini_style(&base, &mut files);
            if files.is_empty() {
                return Err("Qwen Code 的话题文件里没有可导入的对话。".into());
            }
            Ok(import_from_files(&app, &files, "qw"))
        }
        "opencode" => {
            let roots = opencode_storage_roots(&home);
            if roots.is_empty() {
                return Err(
                    "没找到 OpenCode 的话题数据（应在 ~/.local/share/opencode 或 %LOCALAPPDATA%\\opencode）。"
                        .into(),
                );
            }
            for storage in &roots {
                let info_dir = storage.join("session").join("info");
                let Ok(listing) = fs::read_dir(&info_dir) else {
                    continue;
                };
                for entry in listing.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                        continue;
                    }
                    if let Some(parsed) = parse_opencode_session(storage, &path) {
                        files.push((path, parsed.0, parsed.1, parsed.2, parsed.3, parsed.4));
                    }
                }
            }
            if files.is_empty() {
                return Err("OpenCode 的话题数据里没有可导入的对话。".into());
            }
            Ok(import_from_files(&app, &files, "oc"))
        }
        other => Err(format!("不支持的应用类型：{other}")),
    }
}

/// 解析 Gemini CLI / Qwen Code 的一个话题文件（两者同构、目录不同）：
/// 单文件 JSON，messages 数组里 user/gemini 交替，text 与 content 两种字段名都接
fn parse_gemini_style_file(
    path: &Path,
) -> Option<(Option<String>, String, i64, i64, Vec<(String, String)>)> {
    let value: Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    parse_gemini_style_value(&value)
}

fn parse_gemini_style_value(
    value: &Value,
) -> Option<(Option<String>, String, i64, i64, Vec<(String, String)>)> {
    let messages_raw = value["messages"].as_array()?;
    let mut messages: Vec<(String, String)> = Vec::new();
    for item in messages_raw {
        let role = item["type"].as_str().unwrap_or_default();
        if role != "user" && role != "gemini" {
            continue;
        }
        // 字段随版本漂移：text 与 content 都试
        let content = item["text"]
            .as_str()
            .or_else(|| item["content"].as_str())
            .unwrap_or_default()
            .to_string();
        if content.trim().is_empty() {
            continue;
        }
        messages.push((
            if role == "user" { "user".to_string() } else { "assistant".to_string() },
            content,
        ));
    }
    if messages.is_empty() {
        return None;
    }
    let earliest = value["createdAt"].as_str().and_then(iso_to_ms).unwrap_or(0);
    let latest = value["lastUpdated"].as_str().and_then(iso_to_ms).unwrap_or(earliest);
    Some((
        // 目录名是项目 hash，反推不出真实路径：这类话题不挂项目
        None,
        messages[0].1.chars().take(40).collect(),
        earliest,
        latest,
        messages,
    ))
}

/// 数 Gemini CLI / Qwen Code 的话题：tmp/<项目hash>/chats/*.json
fn count_gemini_style_sessions(base: &Path) -> usize {
    let Ok(entries) = fs::read_dir(base) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| count_files(&entry.path().join("chats"), "json"))
        .sum()
}

fn collect_gemini_style(
    base: &Path,
    files: &mut Vec<(PathBuf, Option<String>, String, i64, i64, Vec<(String, String)>)>,
) {
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let chats = entry.path().join("chats");
        if !chats.is_dir() {
            continue;
        }
        let Ok(listing) = fs::read_dir(&chats) else {
            continue;
        };
        for file in listing.flatten() {
            let path = file.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            if let Some(parsed) = parse_gemini_style_file(&path) {
                files.push((path, parsed.0, parsed.1, parsed.2, parsed.3, parsed.4));
            }
        }
    }
}

/// OpenCode 的数据根候选：XDG 风格的 ~/.local/share/opencode 与 Windows 的
/// %LOCALAPPDATA%\opencode。新版把 storage 挪进 project/<hash>/ 一层，旧版直接
/// storage/——递归找 session/info 目录、取其父目录当 storage 根，两种布局都覆盖
fn opencode_storage_roots(home: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![home.join(".local").join("share").join("opencode")];
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        candidates.push(PathBuf::from(local).join("opencode"));
    }
    let mut roots = Vec::new();
    for base in candidates {
        if base.is_dir() {
            find_opencode_storage(&base, 0, &mut roots);
        }
    }
    roots
}

fn find_opencode_storage(dir: &Path, depth: usize, roots: &mut Vec<PathBuf>) {
    if depth > 6 || roots.len() >= 64 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.file_name().and_then(|name| name.to_str()) == Some("session")
            && path.join("info").is_dir()
        {
            if let Some(storage) = path.parent().map(Path::to_path_buf) {
                if !roots.contains(&storage) {
                    roots.push(storage);
                }
            }
            // storage 根内就是 message/part 等兄弟目录，不再往下找
            continue;
        }
        find_opencode_storage(&path, depth + 1, roots);
    }
}

fn count_opencode_sessions(storage: &Path) -> usize {
    count_files_recursive(&storage.join("session").join("info"), "json")
}

/// 聚合 OpenCode 的一个话题：session/info 的元数据 + message/<sid> 的消息 +
/// part/<mid> 的文本分段，三棵目录树拼成一条线性对话。只取 type=text 的分段，
/// 工具调用离开原 harness 没有意义
fn parse_opencode_session(
    storage: &Path,
    info_path: &Path,
) -> Option<(Option<String>, String, i64, i64, Vec<(String, String)>)> {
    let info: Value = serde_json::from_str(&fs::read_to_string(info_path).ok()?).ok()?;
    let session_id = info["id"].as_str()?.to_string();

    let mut ordered: Vec<(String, String)> = Vec::new(); // (messageID, role)
    let message_dir = storage.join("message").join(&session_id);
    if let Ok(entries) = fs::read_dir(&message_dir) {
        let mut rows: Vec<(String, String, i64)> = entries
            .flatten()
            .filter_map(|entry| {
                let value: Value =
                    serde_json::from_str(&fs::read_to_string(entry.path()).ok()?).ok()?;
                let id = value["id"].as_str()?.to_string();
                let role = value["role"].as_str()?.to_string();
                if role != "user" && role != "assistant" {
                    return None;
                }
                Some((id, role, value["time"]["created"].as_i64().unwrap_or(0)))
            })
            .collect();
        rows.sort_by(|a, b| a.2.cmp(&b.2));
        ordered = rows.into_iter().map(|(id, role, _)| (id, role)).collect();
    }
    if ordered.is_empty() {
        return None;
    }

    let mut messages: Vec<(String, String)> = Vec::new();
    for (message_id, role) in &ordered {
        let part_dir = storage.join("part").join(message_id);
        let mut parts: Vec<(String, String)> = Vec::new(); // (文件名, 文本)
        if let Ok(entries) = fs::read_dir(&part_dir) {
            for entry in entries.flatten() {
                let value: Value =
                    serde_json::from_str(&fs::read_to_string(entry.path()).ok()?).unwrap_or_default();
                if value["type"].as_str() != Some("text") {
                    continue;
                }
                let text = value["text"].as_str().unwrap_or_default().to_string();
                if text.trim().is_empty() {
                    continue;
                }
                parts.push((entry.file_name().to_string_lossy().into_owned(), text));
            }
        }
        // 分段文件名自带序号，按名排序才是原始顺序
        parts.sort_by(|a, b| a.0.cmp(&b.0));
        let content = parts.into_iter().map(|(_, text)| text).collect::<Vec<_>>().join("\n\n");
        if content.trim().is_empty() {
            continue;
        }
        messages.push((role.clone(), content));
    }
    if messages.is_empty() {
        return None;
    }

    let cwd = info["directory"].as_str().map(str::to_string);
    let created = info["time"]["created"].as_i64().unwrap_or(0);
    let updated = info["time"]["updated"].as_i64().unwrap_or(created);
    let title = info["title"].as_str().unwrap_or_default().to_string();
    let first_line = if title.trim().is_empty() {
        messages[0].1.chars().take(40).collect()
    } else {
        title.chars().take(60).collect()
    };
    Some((cwd, first_line, created, updated, messages))
}

fn collect_jsonl(
    dir: &Path,
    files: &mut Vec<(PathBuf, Option<String>, String, i64, i64, Vec<(String, String)>)>,
    parse: fn(&Path) -> Option<(Option<String>, String, i64, i64, Vec<(String, String)>)>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl(&path, files, parse);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            if let Some(parsed) = parse(&path) {
                files.push((path, parsed.0, parsed.1, parsed.2, parsed.3, parsed.4));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gemini_session_file_parses_into_user_and_assistant_lines() {
        let value: Value = serde_json::json!({
            "messages": [
                { "type": "user", "text": "帮我写个冒泡排序" },
                { "type": "gemini", "content": "好的，这是实现：" },
                { "type": "info", "text": "这条不是对话，跳过" }
            ],
            "createdAt": "2026-01-02T03:04:05.000Z",
            "lastUpdated": "2026-01-02T03:05:05.000Z"
        });
        let parsed = parse_gemini_style_value(&value).expect("gemini 话题该解析出来");
        assert_eq!(parsed.0, None);
        assert_eq!(parsed.2, 1_767_323_045_000);
        assert_eq!(parsed.3, 1_767_323_105_000);
        assert_eq!(parsed.4.len(), 2);
        assert_eq!(parsed.4[0], ("user".to_string(), "帮我写个冒泡排序".to_string()));
        assert_eq!(parsed.4[1].0, "assistant");
    }

    #[test]
    fn an_opencode_storage_tree_aggregates_into_one_linear_conversation() {
        let root = std::env::temp_dir().join(format!("aglab-import-test-{}", std::process::id()));
        let storage = root.join("project").join("abc").join("storage");
        let info_dir = storage.join("session").join("info");
        fs::create_dir_all(&info_dir).expect("建目录");
        fs::create_dir_all(storage.join("message").join("ses_1")).expect("建目录");
        fs::create_dir_all(storage.join("part").join("msg_1")).expect("建目录");
        fs::create_dir_all(storage.join("part").join("msg_2")).expect("建目录");

        fs::write(
            info_dir.join("ses_1.json"),
            r#"{"id":"ses_1","directory":"C:\\work\\demo","title":"冒泡","time":{"created":1000,"updated":3000}}"#,
        )
        .expect("写 session");
        fs::write(
            storage.join("message").join("ses_1").join("msg_1.json"),
            r#"{"id":"msg_1","sessionID":"ses_1","role":"user","time":{"created":1000}}"#,
        )
        .expect("写 message");
        fs::write(
            storage.join("message").join("ses_1").join("msg_2.json"),
            r#"{"id":"msg_2","sessionID":"ses_1","role":"assistant","time":{"created":2000}}"#,
        )
        .expect("写 message");
        // part 文件名故意乱序：聚合要按名排序还原原始分段顺序
        fs::write(
            storage.join("part").join("msg_2").join("part_002.json"),
            r#"{"id":"part_2","messageID":"msg_2","type":"text","text":"第二段"}"#,
        )
        .expect("写 part");
        fs::write(
            storage.join("part").join("msg_2").join("part_001.json"),
            r#"{"id":"part_1","messageID":"msg_2","type":"text","text":"第一段"}"#,
        )
        .expect("写 part");
        // 非 text 分段（工具调用）不导
        fs::write(
            storage.join("part").join("msg_1").join("part_001.json"),
            r#"{"id":"part_0","messageID":"msg_1","type":"tool","state":"..."}"#,
        )
        .expect("写 part");

        let parsed =
            parse_opencode_session(&storage, &info_dir.join("ses_1.json")).expect("该聚合出来");
        assert_eq!(parsed.0.as_deref(), Some("C:\\work\\demo"));
        assert_eq!(parsed.1, "冒泡");
        assert_eq!(parsed.2, 1000);
        assert_eq!(parsed.3, 3000);
        // user 那条只有工具分段没有文本 → 跳过；assistant 两条 part 按名序拼接
        assert_eq!(parsed.4, vec![("assistant".to_string(), "第一段\n\n第二段".to_string())]);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn opencode_roots_are_found_under_project_scoped_layouts() {
        let root = std::env::temp_dir().join(format!("aglab-import-roots-{}", std::process::id()));
        // 数据根必须是 opencode_storage_roots 认的那两个固定位置之一
        let base = root.join(".local").join("share").join("opencode");
        let storage = base.join("project").join("xyz").join("storage");
        fs::create_dir_all(storage.join("session").join("info")).expect("建目录");
        // 不写文件，目录形状对就能被识别
        let roots = opencode_storage_roots(&root);
        assert!(roots.iter().any(|candidate| candidate.ends_with("storage")));
        let _ = fs::remove_dir_all(&root);
    }
}
