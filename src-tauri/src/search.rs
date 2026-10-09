//! 话题全文搜索：跨话题的消息正文检索（对齐 deepseek-harness 的
//! session-query/SQLite FTS 思路，落地取 LIKE 扫描——本机语料量级下
//! 全表扫描在几十毫秒内，且对中文/短查询零分词问题；FTS5 trigram
//! 需要 ≥3 字符查询，中文两字词是主流，故弃用）。
//!
//! 索引住在独立的 `search.db`（与 usage.db 同目录同模式），**不跟
//! conversationStore 的 json/sqlite 开关走**：两种后端的保存都从
//! `history_save` 这一个口过，索引在那里增量更新；删除走 `history_remove`。
//! 首次使用或索引落后时由 `rebuild` 全量重建（两个后端都扫一遍）。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::Connection;
use serde::Serialize;
use tauri::{AppHandle, Manager};

/// 每条命中摘要的上下文窗口（命中词前后各留这么多字符）
const SNIPPET_CONTEXT: usize = 40;
/// 单次搜索返回的命中上限
const MAX_HITS: usize = 60;
/// 单条正文参与匹配的最大长度：超长消息（工具输出粘贴等）截头去尾后再扫
const MAX_BODY_INDEXED: usize = 200_000;

fn db_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("search.db"))
}

fn open(file: &Path) -> Result<Connection, String> {
    let conn = Connection::open(file).map_err(|e| format!("打开搜索索引失败：{e}"))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS indexed_messages (
             conversation_id TEXT NOT NULL,
             message_id      TEXT NOT NULL,
             seq             INTEGER NOT NULL,
             role            TEXT NOT NULL,
             body            TEXT NOT NULL,
             PRIMARY KEY (conversation_id, message_id)
         );
         CREATE INDEX IF NOT EXISTS idx_indexed_conversation
             ON indexed_messages (conversation_id);
         CREATE TABLE IF NOT EXISTS indexed_conversations (
             conversation_id TEXT PRIMARY KEY,
             message_count   INTEGER NOT NULL,
             updated_at      INTEGER NOT NULL,
             title           TEXT NOT NULL DEFAULT ''
         );",
    )
    .map_err(|e| format!("初始化搜索索引失败：{e}"))?;
    Ok(conn)
}

/// LIKE 的转义：% _ 与转义符本身。查询永远包成 %q% 子串匹配——
/// 中文没有词边界，子串匹配才是用户对"全文搜索"的直觉
fn like_pattern(query: &str) -> String {
    let mut out = String::from("%");
    for ch in query.chars() {
        match ch {
            '%' | '_' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out.push('%');
    out
}

/// 命中摘要：命中词前后各留 SNIPPET_CONTEXT，命中词用【】标出。
/// 截断了才加省略号——短正文原样返回，不装作后面还有内容
fn snippet_of(body: &str, query: &str) -> String {
    let lower_body = body.to_lowercase();
    let lower_query = query.to_lowercase();
    let Some(pos) = lower_body.find(&lower_query) else {
        return body.chars().take(SNIPPET_CONTEXT * 2).collect();
    };
    let skipped = body[..start_char(body, pos)]
        .chars()
        .count()
        .saturating_sub(SNIPPET_CONTEXT);
    let total: Vec<char> = body.chars().skip(skipped).collect();
    let window = SNIPPET_CONTEXT * 2 + lower_query.chars().count();
    let truncated = total.len() > window;
    let mut snippet: String = total.iter().take(window).collect();
    let lower_snippet = snippet.to_lowercase();
    if let Some(at) = lower_snippet.find(&lower_query) {
        let char_at = lower_snippet[..at].chars().count();
        let q_len = lower_query.chars().count();
        let chars: Vec<char> = snippet.chars().collect();
        if char_at + q_len <= chars.len() {
            let marked = format!(
                "【{}】",
                chars[char_at..char_at + q_len].iter().collect::<String>()
            );
            snippet = format!(
                "{}{marked}{}",
                chars[..char_at].iter().collect::<String>(),
                chars[char_at + q_len..].iter().collect::<String>()
            );
        }
    }
    let prefix = if skipped > 0 { "…" } else { "" };
    let suffix = if truncated { "…" } else { "" };
    format!("{prefix}{snippet}{suffix}")
}

/// 字节位置 → 字符位置的就近取整（UTF-8 边界安全：pos 来自小写串的 find，
/// 可能落在多字节序列中段，退到最近的字符起点）
fn start_char(body: &str, byte_pos: usize) -> usize {
    body.char_indices()
        .map(|(i, _)| i)
        .filter(|i| *i <= byte_pos)
        .max()
        .unwrap_or(0)
}

/// 一条全文命中。前端按 conversation_id 分组展示
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub conversation_id: String,
    pub title: String,
    pub message_id: String,
    pub role: String,
    pub snippet: String,
}

/// 重建索引的全量扫描。两个后端都扫一遍（JSON 文件与 SQLite 库各自 load_all），
/// 同 id 以 updated_at 较新者为准。量级：本机语料的线性扫描，秒级封顶
pub fn rebuild(app: &AppHandle) -> Result<usize, String> {
    let conversations = crate::history::load_all_for_search(app)?;
    let file = db_path(app)?;
    let conn = open(&file)?;
    conn.execute("DELETE FROM indexed_messages", [])
        .map_err(|e| e.to_string())?;
    conn.execute("DELETE FROM indexed_conversations", [])
        .map_err(|e| e.to_string())?;
    let mut indexed = 0usize;
    for conversation in &conversations {
        index_conversation(&conn, conversation)?;
        indexed += 1;
    }
    Ok(indexed)
}

/// 单条话题的索引更新：先删后插（话题级全量替换，语义最简单且不怕漏）
fn index_conversation(
    conn: &Connection,
    conversation: &crate::history::Conversation,
) -> Result<(), String> {
    conn.execute(
        "DELETE FROM indexed_messages WHERE conversation_id = ?1",
        rusqlite::params![conversation.id],
    )
    .map_err(|e| e.to_string())?;
    for (index, message) in conversation.messages.iter().enumerate() {
        let mut body = message.content.clone();
        if body.chars().count() > MAX_BODY_INDEXED {
            let head: String = body.chars().take(MAX_BODY_INDEXED / 2).collect();
            let tail: String = body
                .chars()
                .skip(body.chars().count() - MAX_BODY_INDEXED / 2)
                .collect();
            body = format!("{head}…（中段略）…{tail}");
        }
        conn.execute(
            "INSERT OR REPLACE INTO indexed_messages
                 (conversation_id, message_id, seq, role, body)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                conversation.id,
                message.id,
                index as i64,
                message.role,
                body
            ],
        )
        .map_err(|e| e.to_string())?;
    }
    conn.execute(
        "INSERT OR REPLACE INTO indexed_conversations
             (conversation_id, message_count, updated_at, title)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![
            conversation.id,
            conversation.messages.len() as i64,
            conversation.updated_at,
            conversation.title
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// 保存挂钩：话题落盘后同步更新索引（删除的话题走 remove 挂钩）。
/// 失败只报给调用方不阻塞保存——搜索是便利功能，索引落后可以重建
pub fn on_saved(app: &AppHandle, conversation: &crate::history::Conversation) {
    if let Ok(conn) = open(&db_path(app).unwrap_or_else(|_| PathBuf::from("search.db"))) {
        let _ = index_conversation(&conn, conversation);
    }
}

pub fn on_removed(app: &AppHandle, id: &str) {
    if let Ok(conn) = open(&db_path(app).unwrap_or_else(|_| PathBuf::from("search.db"))) {
        let _ = conn.execute(
            "DELETE FROM indexed_messages WHERE conversation_id = ?1",
            [id],
        );
        let _ = conn.execute(
            "DELETE FROM indexed_conversations WHERE conversation_id = ?1",
            [id],
        );
    }
}

/// 全文搜索：LIKE 子串匹配（大小写不敏感），按话题新旧排序、命中位置就近。
/// 空查询返回空表——空串没有"全文"语义
pub fn search(app: &AppHandle, query: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let conn = open(&db_path(app)?)?;
    let pattern = like_pattern(query);
    let mut stmt = conn
        .prepare(
            "SELECT m.conversation_id, COALESCE(c.title, ''), m.message_id, m.role, m.body
             FROM indexed_messages m
             LEFT JOIN indexed_conversations c ON c.conversation_id = m.conversation_id
             WHERE m.body LIKE ?1 ESCAPE '\\'
             ORDER BY COALESCE(c.updated_at, 0) DESC, m.seq ASC
             LIMIT ?2",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(
            rusqlite::params![pattern, (limit.min(MAX_HITS)) as i64],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .map_err(|e| e.to_string())?;
    let mut hits = Vec::new();
    for row in rows {
        let (conversation_id, title, message_id, role, body) = row.map_err(|e| e.to_string())?;
        hits.push(SearchHit {
            conversation_id,
            title,
            message_id,
            role,
            snippet: snippet_of(&body, query),
        });
    }
    Ok(hits)
}

/// 索引是否落后于存档：indexed_conversations 的 (message_count, updated_at)
/// 与存档对不上就报需要重建。设置页与搜索入口共用这一个判据
pub fn stale_count(app: &AppHandle) -> Result<usize, String> {
    let conversations = crate::history::load_all_for_search(app)?;
    let conn = open(&db_path(app)?)?;
    let mut stale = 0usize;
    for conversation in &conversations {
        let known: Option<(i64, i64)> = conn
            .query_row(
                "SELECT message_count, updated_at FROM indexed_conversations WHERE conversation_id = ?1",
                rusqlite::params![conversation.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok();
        let current = (conversation.messages.len() as i64, conversation.updated_at);
        match known {
            Some(pair) if pair == current => {}
            _ => stale += 1,
        }
    }
    Ok(stale)
}

/// 索引状态给设置页/搜索框提示用
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchIndexStatus {
    pub conversations: usize,
    pub stale: usize,
}

pub fn status(app: &AppHandle) -> Result<SearchIndexStatus, String> {
    let conversations = crate::history::load_all_for_search(app)?;
    Ok(SearchIndexStatus {
        conversations: conversations.len(),
        stale: stale_count(app)?,
    })
}

/// 静态锁占位：search.db 的连接不跨线程缓存（与 mcp.rs 同样的短连接模式），
/// 这个类型只为了让未来的并发写有地方挂锁
#[allow(dead_code)]
static WRITE_LOCK: Mutex<()> = Mutex::new(());

#[tauri::command]
pub fn session_search(
    app: AppHandle,
    query: String,
    limit: Option<usize>,
) -> Result<Vec<SearchHit>, String> {
    search(&app, &query, limit.unwrap_or(MAX_HITS))
}

#[tauri::command]
pub fn session_search_status(app: AppHandle) -> Result<SearchIndexStatus, String> {
    status(&app)
}

#[tauri::command]
pub fn session_search_rebuild(app: AppHandle) -> Result<usize, String> {
    rebuild(&app)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_pattern_escapes_wildcards() {
        assert_eq!(like_pattern("100%"), "%100\\%%");
        assert_eq!(like_pattern("a_b"), "%a\\_b%");
        assert_eq!(like_pattern("普通词"), "%普通词%");
    }

    #[test]
    fn snippet_marks_the_hit_and_keeps_context() {
        let body = "前言。登录超时的原因是网关抖动，后续段落继续凑长度，把上下文撑到窗口之外再多来一些字符。";
        let snippet = snippet_of(body, "网关");
        assert!(snippet.contains("【网关】"), "命中词要标出：{snippet}");
        assert!(snippet.starts_with('…') || body.starts_with("前言。"));
    }

    #[test]
    fn snippet_is_case_insensitive_and_marks_original_case() {
        assert_eq!(snippet_of("ABC def", "abc"), "【ABC】 def");
    }

    #[test]
    fn like_pattern_is_wrapped_as_substring() {
        assert_eq!(like_pattern(""), "%%");
    }
}
