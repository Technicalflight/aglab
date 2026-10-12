//! Agent Hub（助手库）：自动发现本机已安装的外部 AI 编程 Agent，只读导入其
//! 配置、会话与记忆，统一存储、检索与（分期）续聊。
//!
//! 设计稿：docs/agent-hub-design.md（第 1、2 步）。原则钉死：
//! 适配器驱动 / 本地优先 / 增量同步 / 只读优先 / 用户授权。
//!
//! 本轮（第 2 步）只落 schema 与打库链路；发现器（discover）、适配器
//! （adapters/）、增量同步（sync，Rust 侧 notify 监听）、网关（gateway）与
//! 续聊（resume）按阶段落地，见模块内 TODO。
//!
//! 命名约定：代码、注释、清单一律中性代称——具体识别目标由数据清单
//! （registry，阶段 A 落地）注入，不进代码。

use rusqlite::Connection;
use serde::Serialize;
// app.path() 需要 Manager trait 在作用域内
use tauri::Manager;

/// Schema 全量：七张业务表 + settings + sync_state + 统一 FTS5 索引。
/// 与 docs/agent-hub-design.md 第 2 节逐字对应；幂等（IF NOT EXISTS），
/// 每次打库都执行，迁移策略（用户版本表 + 分步迁移）等首批真实迁移需求出现再立。
const SCHEMA_SQL: &str = r#"
PRAGMA journal_mode = WAL;

CREATE TABLE IF NOT EXISTS agents (
  id           TEXT PRIMARY KEY,
  name         TEXT NOT NULL,
  install_path TEXT,
  config_path  TEXT,
  enabled      INTEGER NOT NULL DEFAULT 0,
  last_seen_at INTEGER
);

CREATE TABLE IF NOT EXISTS projects (
  id         TEXT PRIMARY KEY,
  agent_id   TEXT NOT NULL REFERENCES agents(id),
  path       TEXT NOT NULL,
  name       TEXT NOT NULL,
  created_at INTEGER,
  UNIQUE (agent_id, path)
);

CREATE TABLE IF NOT EXISTS sessions (
  id                  TEXT PRIMARY KEY,
  agent_id            TEXT NOT NULL REFERENCES agents(id),
  project_id          TEXT REFERENCES projects(id),
  original_session_id TEXT NOT NULL,
  title               TEXT,
  started_at          INTEGER,
  updated_at          INTEGER,
  model               TEXT,
  token_usage         INTEGER,
  raw_path            TEXT,
  UNIQUE (agent_id, original_session_id)
);

CREATE TABLE IF NOT EXISTS messages (
  id                  TEXT PRIMARY KEY,
  session_id          TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  original_message_id TEXT,
  role                TEXT NOT NULL,
  created_at          INTEGER,
  model               TEXT,
  content_hash        TEXT,
  raw_json            TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_dedupe_id
  ON messages (session_id, original_message_id)
  WHERE original_message_id IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_dedupe_hash
  ON messages (session_id, content_hash, created_at)
  WHERE original_message_id IS NULL;

CREATE TABLE IF NOT EXISTS message_parts (
  id          TEXT PRIMARY KEY,
  message_id  TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
  type        TEXT NOT NULL,
  text        TEXT,
  tool_name   TEXT,
  tool_input  TEXT,
  tool_output TEXT,
  mime        TEXT,
  blob_path   TEXT,
  ordinal     INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS memories (
  id         TEXT PRIMARY KEY,
  agent_id   TEXT NOT NULL REFERENCES agents(id),
  project_id TEXT REFERENCES projects(id),
  path       TEXT NOT NULL,
  type       TEXT NOT NULL,
  content    TEXT NOT NULL,
  updated_at INTEGER,
  UNIQUE (agent_id, path)
);

CREATE TABLE IF NOT EXISTS sync_state (
  id             TEXT PRIMARY KEY,
  agent_id       TEXT NOT NULL REFERENCES agents(id),
  path           TEXT NOT NULL,
  mtime          INTEGER,
  size           INTEGER,
  hash           TEXT,
  last_synced_at INTEGER,
  UNIQUE (agent_id, path)
);

CREATE TABLE IF NOT EXISTS settings (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE VIRTUAL TABLE IF NOT EXISTS fts_index USING fts5 (
  ref_id UNINDEXED,
  kind   UNINDEXED,
  text,
  tokenize = 'unicode61'
);
"#;

/// 期望存在的核心表（status 命令据此汇报就绪度）
const CORE_TABLES: [&str; 9] = [
    "agents",
    "projects",
    "sessions",
    "messages",
    "message_parts",
    "memories",
    "sync_state",
    "settings",
    "fts_index",
];

/// 打开（或创建）助手库并确保 schema 就绪。库住在 app 数据目录下，
/// 与会话台账、备份同域，天然不进任何同步盘的提交范围。
fn open_hub(data_dir: &std::path::Path) -> Result<Connection, String> {
    std::fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
    let db_path = data_dir.join("agent-hub.db");
    let conn = Connection::open(&db_path).map_err(|e| e.to_string())?;
    conn.execute_batch(SCHEMA_SQL)
        .map_err(|e| format!("agent hub schema 初始化失败: {e}"))?;
    Ok(conn)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HubStatus {
    db_path: String,
    tables_ready: Vec<String>,
    tables_missing: Vec<String>,
    fts_ready: bool,
}

/// 打库自检：表清单是否齐、FTS 是否可查。前端「助手库」分区进入时调用，
/// 也是阶段 A 之前唯一暴露的命令——后续 discover/import 等命令在各自阶段挂进注册表。
#[tauri::command]
pub fn agent_hub_status(app: tauri::AppHandle) -> Result<HubStatus, String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let conn = open_hub(&data_dir)?;
    let mut tables_ready = Vec::new();
    let mut tables_missing = Vec::new();
    for table in CORE_TABLES {
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type IN ('table','virtual table') AND name = ?1",
                [table],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| count > 0)
            .map_err(|e| e.to_string())?;
        if exists {
            tables_ready.push(table.to_string());
        } else {
            tables_missing.push(table.to_string());
        }
    }
    let fts_ready = conn
        .prepare("SELECT ref_id, kind FROM fts_index LIMIT 1")
        .is_ok();
    Ok(HubStatus {
        db_path: data_dir.join("agent-hub.db").display().to_string(),
        tables_ready,
        tables_missing,
        fts_ready,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Schema 本身必须始终有效：内存库直跑建表 SQL，核心表一个不能少，
    /// FTS 可查，部分唯一索引真的防重（同 original_message_id 第二次插入必须红）。
    #[test]
    fn schema_is_idempotent_and_dedupe_indexes_work() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();
        // 幂等：再跑一遍不炸
        conn.execute_batch(SCHEMA_SQL).unwrap();

        for table in CORE_TABLES {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(count > 0, "核心表 {table} 未建出");
        }

        conn.execute_batch(
            r#"
            INSERT INTO agents (id, name) VALUES ('demo', '示例');
            INSERT INTO sessions (id, agent_id, original_session_id) VALUES ('s1', 'demo', 'raw-1');
            INSERT INTO messages (id, session_id, original_message_id, role)
              VALUES ('m1', 's1', 'raw-m1', 'user');
            INSERT INTO fts_index (ref_id, kind, text) VALUES ('m1', 'message', '你好世界');
            "#,
        )
        .unwrap();

        // FTS 查询通。注意：unicode61 把连续 CJK 当一个词元，中文检索要靠
        // 前缀匹配或换分词器——选型（trigram / 应用层 bigram）留阶段 A
        let hit: String = conn
            .query_row(
                "SELECT ref_id FROM fts_index WHERE fts_index MATCH '你好*'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hit, "m1");

        // 去重索引通：同 original_message_id 再插必须被拒
        let second = conn.execute(
            "INSERT INTO messages (id, session_id, original_message_id, role)
             VALUES ('m2', 's1', 'raw-m1', 'user')",
            [],
        );
        assert!(second.is_err(), "去重索引未生效：同原始 id 的消息不应插得进");

        // 无原始 id 的消息走 content_hash + created_at 兜底
        conn.execute(
            "INSERT INTO messages (id, session_id, role, content_hash, created_at)
             VALUES ('m3', 's1', 'user', 'hash-x', 100)",
            [],
        )
        .unwrap();
        let dup = conn.execute(
            "INSERT INTO messages (id, session_id, role, content_hash, created_at)
             VALUES ('m4', 's1', 'user', 'hash-x', 100)",
            [],
        );
        assert!(dup.is_err(), "内容哈希兜底去重未生效");
    }
}
