use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension};

use super::{meta_of, Conversation, ConversationMeta, MessageRecord, UsageRecord};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS conversations (
    id                  TEXT PRIMARY KEY,
    project_id          TEXT NOT NULL DEFAULT '',
    title               TEXT NOT NULL DEFAULT '',
    created_at          INTEGER NOT NULL DEFAULT 0,
    updated_at          INTEGER NOT NULL DEFAULT 0,
    pinned              INTEGER NOT NULL DEFAULT 0,
    kind                TEXT NOT NULL DEFAULT 'chat',
    usage_input_tokens  INTEGER,
    usage_output_tokens INTEGER,
    usage_duration_ms   INTEGER
);

CREATE INDEX IF NOT EXISTS idx_conversations_updated ON conversations (updated_at DESC);

CREATE TABLE IF NOT EXISTS messages (
    conversation_id TEXT NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    seq             INTEGER NOT NULL,
    id              TEXT NOT NULL,
    role            TEXT NOT NULL,
    content         TEXT NOT NULL,
    reasoning       TEXT,
    error           TEXT,
    tool_calls      TEXT NOT NULL DEFAULT '[]',
    created_at      INTEGER NOT NULL,
    parent_id       TEXT,
    model           TEXT,
    PRIMARY KEY (conversation_id, seq)
);
"#;

/// attachments 列是后加的（粘贴截图能力）：老库靠这条幂等迁移补上。
/// `CREATE TABLE IF NOT EXISTS` 管不了已存在的表——列在不在只能 ALTER 试一把，
/// 「duplicate column」就是「已经有了」，其余错误照常上报
const ATTACHMENTS_MIGRATION: &str =
    "ALTER TABLE messages ADD COLUMN attachments TEXT NOT NULL DEFAULT '[]';";

/// 分支树加的列。可空：NULL 就是"这一支的根"，不是"还没填"
const PARENT_ID_MIGRATION: &str = "ALTER TABLE messages ADD COLUMN parent_id TEXT;";

/// 老库里每条消息本来就只是一条链上的一环，所以补列之后**紧接着**按 seq 把前一条
/// 的 id 回填进去。这一步只能做、且只能在做列的那一次做：之后再有 NULL 就是
/// "这真是某一支的根"，重跑这段会把兄弟重新链成一条直线
const PARENT_ID_BACKFILL: &str = r#"
WITH previous AS (
    SELECT rowid AS rid,
           lag(id) OVER (PARTITION BY conversation_id ORDER BY seq) AS before_id
    FROM messages
)
UPDATE messages
SET parent_id = (SELECT before_id FROM previous WHERE previous.rid = messages.rowid)
WHERE parent_id IS NULL"#;

fn apply_attachments_migration(conn: &Connection) -> Result<(), String> {
    if let Err(error) = conn.execute_batch(ATTACHMENTS_MIGRATION) {
        if !error.to_string().contains("duplicate column") {
            return Err(format!("迁移 messages.attachments 列失败：{error}"));
        }
    }
    Ok(())
}

/// 生成会话的产物标记（media 管线的气泡）。可空：NULL = 对话轮消息
const MEDIA_MIGRATION: &str = "ALTER TABLE messages ADD COLUMN media TEXT;";

fn apply_media_migration(conn: &Connection) -> Result<(), String> {
    if let Err(error) = conn.execute_batch(MEDIA_MIGRATION) {
        if !error.to_string().contains("duplicate column") {
            return Err(format!("迁移 messages.media 列失败：{error}"));
        }
    }
    Ok(())
}

fn apply_parent_id_migration(conn: &Connection) -> Result<(), String> {
    match conn.execute_batch(PARENT_ID_MIGRATION) {
        Ok(()) => conn
            .execute_batch(PARENT_ID_BACKFILL)
            .map_err(|error| format!("回填 messages.parent_id 失败：{error}")),
        Err(error) if error.to_string().contains("duplicate column") => Ok(()),
        Err(error) => Err(format!("迁移 messages.parent_id 列失败：{error}")),
    }
}

/// 实发模型这一列是后加的（逐条归属）。**不回填**：老库里那条消息当时没人记过用它的是
/// 谁，拿"现在的配置"去补等于给历史编一份读数
const MODEL_MIGRATION: &str = "ALTER TABLE messages ADD COLUMN model TEXT;";

/// 流式内联的步骤章（思考段/工具行的正文位置）。空数组 = 没章，前端按堆叠布局显示
const STEPS_MIGRATION: &str = "ALTER TABLE messages ADD COLUMN steps TEXT NOT NULL DEFAULT '[]';";

fn apply_model_migration(conn: &Connection) -> Result<(), String> {
    if let Err(error) = conn.execute_batch(MODEL_MIGRATION) {
        if !error.to_string().contains("duplicate column") {
            return Err(format!("迁移 messages.model 列失败：{error}"));
        }
    }
    Ok(())
}

fn apply_steps_migration(conn: &Connection) -> Result<(), String> {
    if let Err(error) = conn.execute_batch(STEPS_MIGRATION) {
        if !error.to_string().contains("duplicate column") {
            return Err(format!("迁移 messages.steps 列失败：{error}"));
        }
    }
    Ok(())
}

/// 每个库文件一条常驻连接。原来每次调用都重开连接并重跑整套幂等迁移
/// （SCHEMA + 十来条 ALTER 尝试），保存一次话题的固定开销全花在这上面。
/// Connection 不是 Sync、调用方来自多个线程，所以每条连接一把自己的锁——
/// 同一库的并发 SQL 在这里排队（SQLite 本来也要串行写），不同库互不挡。
/// run 期间绝不持有下面的全局表锁，不然"拿表锁→等连接锁"与"持连接锁→
/// 收尾要表锁"会凑成环
static POOL: std::sync::OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<Connection>>>>> =
    std::sync::OnceLock::new();

fn pool() -> &'static Mutex<HashMap<PathBuf, Arc<Mutex<Connection>>>> {
    POOL.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn with<T>(
    file: &Path,
    run: impl FnOnce(&Connection) -> Result<T, String>,
) -> Result<T, String> {
    // 先试缓存里那条：常驻连接让单次操作只剩 SQL 本身
    if let Some(conn) = {
        let map = pool().lock().unwrap_or_else(|e| e.into_inner());
        map.get(file).cloned()
    } {
        let guard = conn.lock().unwrap_or_else(|e| e.into_inner());
        let result = run(&guard);
        drop(guard);
        match result {
            Ok(value) => return Ok(value),
            Err(error) => {
                // 报错的连接靠不住（库损坏、句柄失效都可能）：摘掉，
                // 下一次调用自然换新的。逻辑性错误（"没有这条话题"）原样返回
                let mut map = pool().lock().unwrap_or_else(|e| e.into_inner());
                if map.get(file).is_some_and(|held| Arc::ptr_eq(held, &conn)) {
                    map.remove(file);
                }
                return Err(error);
            }
        }
    }
    with_fresh(file, run)
}

fn with_fresh<T>(
    file: &Path,
    run: impl FnOnce(&Connection) -> Result<T, String>,
) -> Result<T, String> {
    let conn = Arc::new(Mutex::new(open(file)?));
    let guard = conn.lock().unwrap_or_else(|e| e.into_inner());
    let result = run(&guard);
    drop(guard);
    if result.is_ok() {
        pool()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(file.to_path_buf(), conn);
    }
    result
}

/// 置顶列是后加的（侧栏置顶能力）：老库靠这条幂等迁移补上
const PINNED_MIGRATION: &str =
    "ALTER TABLE conversations ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;";

fn apply_pinned_migration(conn: &Connection) -> Result<(), String> {
    if let Err(error) = conn.execute_batch(PINNED_MIGRATION) {
        if !error.to_string().contains("duplicate column") {
            return Err(format!("迁移 conversations.pinned 列失败：{error}"));
        }
    }
    Ok(())
}

/// 会话能力档（对话/生图/视频）是后加的列：老库靠这条幂等迁移补上，
/// default 'chat' 让老行天然落在对话档
const KIND_MIGRATION: &str =
    "ALTER TABLE conversations ADD COLUMN kind TEXT NOT NULL DEFAULT 'chat';";

/// 视频画布的节点登记（JSON 数组）。老库靠幂等迁移补列；'[]' = 没建过节点
const VIDEO_NODES_MIGRATION: &str =
    "ALTER TABLE conversations ADD COLUMN video_nodes TEXT NOT NULL DEFAULT '[]';";

/// 节点连接（JSON 数组 [{from,to}]）。老库靠幂等迁移补列
const VIDEO_EDGES_MIGRATION: &str =
    "ALTER TABLE conversations ADD COLUMN video_edges TEXT NOT NULL DEFAULT '[]';";

/// 视频画布的消息归属（可空）。NULL = 非视频会话/老消息，前端归到第一个节点
const NODE_ID_MIGRATION: &str = "ALTER TABLE messages ADD COLUMN node_id TEXT;";

fn apply_kind_migration(conn: &Connection) -> Result<(), String> {
    if let Err(error) = conn.execute_batch(KIND_MIGRATION) {
        if !error.to_string().contains("duplicate column") {
            return Err(format!("迁移 conversations.kind 列失败：{error}"));
        }
    }
    Ok(())
}

fn apply_video_nodes_migration(conn: &Connection) -> Result<(), String> {
    if let Err(error) = conn.execute_batch(VIDEO_NODES_MIGRATION) {
        if !error.to_string().contains("duplicate column") {
            return Err(format!("迁移 conversations.video_nodes 列失败：{error}"));
        }
    }
    Ok(())
}

fn apply_video_edges_migration(conn: &Connection) -> Result<(), String> {
    if let Err(error) = conn.execute_batch(VIDEO_EDGES_MIGRATION) {
        if !error.to_string().contains("duplicate column") {
            return Err(format!("迁移 conversations.video_edges 列失败：{error}"));
        }
    }
    Ok(())
}

fn apply_node_id_migration(conn: &Connection) -> Result<(), String> {
    if let Err(error) = conn.execute_batch(NODE_ID_MIGRATION) {
        if !error.to_string().contains("duplicate column") {
            return Err(format!("迁移 messages.node_id 列失败：{error}"));
        }
    }
    Ok(())
}

fn open(file: &Path) -> Result<Connection, String> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let conn = Connection::open(file).map_err(|e| format!("打开数据库失败：{e}"))?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    // foreign_keys 在 SQLite 里默认关闭，不开启则删话题不会级联删消息。
    // WAL 让读不阻塞写，代价是运行时会多出 -wal / -shm 两个边文件。
    // synchronous=NORMAL 是 WAL 的标配搭档：提交不再每次 fsync（掉电最坏丢
    // 最后一笔事务，不会像 OFF 那样损坏库），高频写话题/用量时省下大头等待
    conn.execute_batch(
        "PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;",
    )
    .map_err(|e| format!("初始化数据库失败：{e}"))?;
    conn.execute_batch(SCHEMA).map_err(|e| e.to_string())?;
    apply_attachments_migration(&conn)?;
    apply_parent_id_migration(&conn)?;
    apply_model_migration(&conn)?;
    apply_steps_migration(&conn)?;
    apply_pinned_migration(&conn)?;
    apply_kind_migration(&conn)?;
    apply_video_nodes_migration(&conn)?;
    apply_video_edges_migration(&conn)?;
    apply_node_id_migration(&conn)?;
    apply_media_migration(&conn)?;
    Ok(conn)
}

pub fn list(conn: &Connection) -> Result<Vec<ConversationMeta>, String> {
    // 侧边栏只要标题、条数和一行预览，所以正文从不被解析：
    // 预览用 substr 在 SQL 里截 80 个字符，和 JSON 后端的 chars().take(80) 对齐
    const SQL: &str = r#"
SELECT c.id,
       c.title,
       c.project_id,
       c.updated_at,
       c.pinned,
       c.kind,
       (SELECT COUNT(*) FROM messages m WHERE m.conversation_id = c.id),
       COALESCE(
           (SELECT substr(m.content, 1, 80) FROM messages m
             WHERE m.conversation_id = c.id AND m.role = 'assistant' AND m.content <> ''
             ORDER BY m.seq DESC LIMIT 1),
           (SELECT substr(m.content, 1, 80) FROM messages m
             WHERE m.conversation_id = c.id AND m.content <> ''
             ORDER BY m.seq DESC LIMIT 1),
           '')
FROM conversations c
ORDER BY c.pinned DESC, c.updated_at DESC"#;

    let mut stmt = conn.prepare(SQL).map_err(|e| e.to_string())?;
    let metas = stmt
        .query_map([], |row| {
            Ok(ConversationMeta {
                id: row.get(0)?,
                title: row.get(1)?,
                project_id: row.get(2)?,
                updated_at: row.get(3)?,
                pinned: row.get::<_, i64>(4)? != 0,
                kind: {
                    let raw: String = row.get(5)?;
                    if raw.is_empty() {
                        "chat".to_string()
                    } else {
                        raw
                    }
                },
                message_count: row.get::<_, i64>(6)? as usize,
                preview: row.get(7)?,
            })
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;

    Ok(metas)
}

pub fn load(conn: &Connection, id: &str) -> Result<Conversation, String> {
    let head = conn
        .query_row(
            "SELECT project_id, title, created_at, updated_at, pinned, kind,
                    usage_input_tokens, usage_output_tokens, usage_duration_ms, video_nodes,
                    video_edges
             FROM conversations WHERE id = ?1",
            params![id],
            |row| {
                let usage = match (
                    row.get::<_, Option<i64>>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                    row.get::<_, Option<i64>>(8)?,
                ) {
                    (Some(input), Some(output), Some(duration)) => Some(UsageRecord {
                        input_tokens: input as u32,
                        output_tokens: output as u32,
                        duration_ms: duration as u64,
                    }),
                    _ => None,
                };
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)? != 0,
                    row.get::<_, String>(5)?,
                    usage,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                ))
            },
        )
        .optional()
        .map_err(|e| e.to_string())?;

    let (project_id, title, created_at, updated_at, pinned, kind, usage, raw_nodes, raw_edges) =
        head.ok_or_else(|| "数据库里没有这条话题。".to_string())?;

    let mut stmt = conn
        .prepare(
            "SELECT id, role, content, reasoning, error, tool_calls, created_at, attachments,
                    parent_id, model, steps, node_id, media
             FROM messages WHERE conversation_id = ?1 ORDER BY seq",
        )
        .map_err(|e| e.to_string())?;

    let messages = stmt
        .query_map(params![id], |row| {
            let raw: String = row.get(5)?;
            let raw_attachments: String = row.get(7)?;
            let raw_steps: String = row.get(10)?;
            Ok(MessageRecord {
                id: row.get(0)?,
                role: row.get(1)?,
                content: row.get(2)?,
                reasoning: row.get(3)?,
                error: row.get(4)?,
                tool_calls: serde_json::from_str(&raw).unwrap_or_default(),
                created_at: row.get(6)?,
                attachments: serde_json::from_str(&raw_attachments).unwrap_or_default(),
                parent_id: row.get(8)?,
                model: row.get(9)?,
                steps: serde_json::from_str(&raw_steps).unwrap_or_default(),
                // sqlite 的消息行没有这一列：去重键住在 JSON 列里由 serde 带回（若在）
                entry_ids: Vec::new(),
                goal_round: None,
                node_id: row.get(11)?,
                media: row.get(12)?,
            })
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;

    Ok(Conversation {
        id: id.to_string(),
        project_id,
        title,
        created_at,
        updated_at,
        pinned,
        kind: if kind.is_empty() {
            "chat".to_string()
        } else {
            kind
        },
        video_nodes: serde_json::from_str(&raw_nodes).unwrap_or_default(),
        video_edges: serde_json::from_str(&raw_edges).unwrap_or_default(),
        messages,
        usage,
    })
}

/// 只取话题的项目归属。回合开始时判定文件工具落在哪个项目要用它：
/// 整份 load 会把正文也拖出来，一个字符串不值这份钱
pub fn load_project_id(conn: &Connection, id: &str) -> Result<String, String> {
    conn.query_row(
        "SELECT project_id FROM conversations WHERE id = ?1",
        params![id],
        |row| row.get(0),
    )
    .map_err(|e| e.to_string())
}

pub fn load_all(conn: &Connection) -> Result<Vec<Conversation>, String> {
    let ids = conn
        .prepare("SELECT id FROM conversations ORDER BY updated_at DESC")
        .map_err(|e| e.to_string())?
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;

    ids.iter().map(|id| load(conn, id)).collect()
}

pub fn save(conn: &Connection, conversation: &Conversation) -> Result<ConversationMeta, String> {
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    let usage = conversation.usage.as_ref();

    // 必须是 ON CONFLICT DO UPDATE：INSERT OR REPLACE 会先删后插，
    // 外键级联于是把那场话题的消息全清掉
    let video_nodes =
        serde_json::to_string(&conversation.video_nodes).map_err(|e| e.to_string())?;
    let video_edges =
        serde_json::to_string(&conversation.video_edges).map_err(|e| e.to_string())?;
    tx.execute(
        "INSERT INTO conversations (id, project_id, title, created_at, updated_at, pinned, kind,
                                    usage_input_tokens, usage_output_tokens, usage_duration_ms,
                                    video_nodes, video_edges)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
         ON CONFLICT (id) DO UPDATE SET
             project_id = excluded.project_id,
             title = excluded.title,
             created_at = excluded.created_at,
             updated_at = excluded.updated_at,
             pinned = excluded.pinned,
             kind = excluded.kind,
             usage_input_tokens = excluded.usage_input_tokens,
             usage_output_tokens = excluded.usage_output_tokens,
             usage_duration_ms = excluded.usage_duration_ms,
             video_nodes = excluded.video_nodes,
             video_edges = excluded.video_edges",
        params![
            conversation.id,
            conversation.project_id,
            conversation.title,
            conversation.created_at,
            conversation.updated_at,
            conversation.pinned,
            if conversation.kind.is_empty() {
                "chat"
            } else {
                &conversation.kind
            },
            usage.map(|item| item.input_tokens as i64),
            usage.map(|item| item.output_tokens as i64),
            usage.map(|item| item.duration_ms as i64),
            video_nodes,
            video_edges,
        ],
    )
    .map_err(|e| format!("写入话题失败：{e}"))?;

    // 整段重写消息：前端的这条话题就是权威版本，逐条 diff 没有意义。
    // 主键是 (conversation_id, seq) 而不是消息 id，所以前端重号也不会丢消息。
    tx.execute(
        "DELETE FROM messages WHERE conversation_id = ?1",
        params![conversation.id],
    )
    .map_err(|e| e.to_string())?;

    for (seq, message) in conversation.messages.iter().enumerate() {
        let tool_calls = serde_json::to_string(&message.tool_calls).map_err(|e| e.to_string())?;
        let attachments = serde_json::to_string(&message.attachments).map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO messages (conversation_id, seq, id, role, content, reasoning, error,
                                   tool_calls, created_at, attachments, parent_id, model, steps,
                                   node_id, media)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                conversation.id,
                seq as i64,
                message.id,
                message.role,
                message.content,
                message.reasoning,
                message.error,
                tool_calls,
                message.created_at,
                attachments,
                message.parent_id,
                message.model,
                serde_json::to_string(&message.steps)
                    .map_err(|e| format!("序列化步骤失败：{e}"))?,
                message.node_id,
                message.media,
            ],
        )
        .map_err(|e| format!("写入消息失败：{e}"))?;
    }

    tx.commit().map_err(|e| e.to_string())?;
    Ok(meta_of(conversation))
}

pub fn remove(conn: &Connection, id: &str) -> Result<(), String> {
    conn.execute("DELETE FROM conversations WHERE id = ?1", params![id])
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn count(conn: &Connection) -> Result<usize, String> {
    conn.query_row("SELECT COUNT(*) FROM conversations", params![], |row| {
        row.get::<_, i64>(0)
    })
    .map(|value| value as usize)
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn file(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("aglab-sqlite-{label}-{nanos}.db"))
    }

    fn conversation(id: &str, updated_at: i64) -> Conversation {
        Conversation {
            id: id.into(),
            project_id: String::new(),
            title: "t".into(),
            created_at: 1,
            updated_at,
            pinned: false,
            kind: "chat".to_string(),
            messages: Vec::new(),
            usage: None,
            video_nodes: Vec::new(),
            video_edges: Vec::new(),
        }
    }

    #[test]
    fn orders_by_newest_first() {
        let path = file("order");
        with(&path, |conn| {
            save(conn, &conversation("a", 100)).unwrap();
            save(conn, &conversation("b", 300)).unwrap();
            save(conn, &conversation("c", 200)).unwrap();
            let ids = list(conn)
                .unwrap()
                .into_iter()
                .map(|m| m.id)
                .collect::<Vec<_>>();
            assert_eq!(ids, vec!["b", "c", "a"]);
            Ok(())
        })
        .unwrap();
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn empty_usage_round_trips_as_none() {
        let path = file("usage");
        with(&path, |conn| {
            save(conn, &conversation("a", 1)).unwrap();
            assert!(load(conn, "a").unwrap().usage.is_none());
            Ok(())
        })
        .unwrap();
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn removing_conversation_cascades_to_messages() {
        let path = file("cascade");
        with(&path, |conn| {
            let mut record = conversation("a", 1);
            record.messages.push(MessageRecord {
                id: "msg_1".into(),
                role: "user".into(),
                content: "x".into(),
                created_at: 1,
                ..Default::default()
            });
            save(conn, &record).unwrap();
            remove(conn, "a").unwrap();

            // foreign_keys 默认是关闭的，没打开就会留下孤儿消息并在同名话题回来时复活
            let orphan: i64 = conn
                .query_row("SELECT COUNT(*) FROM messages", params![], |row| row.get(0))
                .unwrap();
            assert_eq!(orphan, 0);
            Ok(())
        })
        .unwrap();
        std::fs::remove_file(path).ok();
    }
}
