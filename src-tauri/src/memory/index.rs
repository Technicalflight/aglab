use std::path::Path;

use rusqlite::{params, Connection};

use super::decay;
use super::graph;
use super::origin::{Origin, SourceView};
use super::record::{MemoryRecord, MemoryScope, MemoryStatus};

/// 索引结构版本。改了表就 +1，旧库直接重建而不是写迁移——它随时能从 Markdown 还原。
/// v2：`memories` 多 `occurred_at` / `reinforced_at` / `origin` 三列，`memory_links`
/// 多一种 `conflicts_with` 边。三样都来自 frontmatter，所以删库能原样问回来。
/// v3：图谱那两张派生表（`memory_entities` / `memory_entity_links`）。它们整个由记录的正文、
/// 标签与 `entities` 算出来，删了能重建——所以 `.md` 那边一个字都不加
/// v4：`memories` 多一列 `sensitivity`。它来自 frontmatter 的同一行，读不懂时按最严的那一档算
const SCHEMA_VERSION: i64 = 4;

/// 把文本切成 FTS5 能用的 token。
///
/// unicode61 不分中文词，"沟通风格" 会被当成一个整 token，用户搜「风格」就搜不到。
/// 所以喂进去之前自己做 CJK 切分：每个单字 + 每个相邻二元组，空格分隔。
/// 查询侧走同一个函数，两边才可能对得上
pub fn segment_for_index(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    let mut ascii_word = String::new();
    let mut cjk_run: Vec<char> = Vec::new();

    let flush_ascii = |word: &mut String, out: &mut String| {
        if !word.is_empty() {
            out.push_str(&word.to_ascii_lowercase());
            out.push(' ');
            word.clear();
        }
    };
    let flush_cjk = |run: &mut Vec<char>, out: &mut String| {
        for (index, ch) in run.iter().enumerate() {
            out.push(*ch);
            out.push(' ');
            if let Some(next) = run.get(index + 1) {
                out.push(*ch);
                out.push(*next);
                out.push(' ');
            }
        }
        run.clear();
    };

    for ch in text.chars() {
        if is_cjk(ch) {
            flush_ascii(&mut ascii_word, &mut out);
            cjk_run.push(ch);
        } else if ch.is_alphanumeric() || ch == '_' || ch == '-' {
            flush_cjk(&mut cjk_run, &mut out);
            ascii_word.push(ch);
        } else {
            flush_ascii(&mut ascii_word, &mut out);
            flush_cjk(&mut cjk_run, &mut out);
        }
    }
    flush_ascii(&mut ascii_word, &mut out);
    flush_cjk(&mut cjk_run, &mut out);
    out
}

pub fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x2EBEF)
}

/// 常见虚词单字：它们出现在 MATCH 里只会把半库拖进候选。只在"整条查询凑不出
/// 双字、退回单字兜底"的那条路上过滤——双字为主时它们根本没有出场机会
const STOPWORD_SINGLES: [&str; 14] = [
    "的", "了", "是", "在", "和", "与", "就", "都", "也", "很", "呢", "吗", "吧", "啊",
];

/// 查询侧：切完之后用 OR 连，命中任意 token 就算候选，排序交给 bm25。
/// 用 AND 的话中文两个字的词一多就几乎全被自己卡死。
///
/// 单字不进长查询：查询/索引两侧都是单字+双字，一个单字 token 几乎每条记录都有，
/// 全部 OR 进去等于把 bm25 的区分度按比例稀释掉。双字一个不缺时单字整个让位，
/// 整条查询短到凑不出双字才退回单字（兜底那一档再滤一遍虚词）
pub fn build_match_query(query: &str) -> String {
    let raw: Vec<String> = segment_for_index(query)
        .split_whitespace()
        .map(String::from)
        .collect();
    let is_single_cjk =
        |token: &str| token.chars().count() == 1 && token.chars().next().is_some_and(is_cjk);
    let grams: Vec<&String> = raw.iter().filter(|token| !is_single_cjk(token)).collect();
    let chosen: Vec<&String> = if grams.is_empty() {
        raw.iter()
            .filter(|token| is_single_cjk(token) && !STOPWORD_SINGLES.contains(&token.as_str()))
            .collect()
    } else {
        grams
    };
    if chosen.is_empty() {
        return String::new();
    }
    chosen
        .iter()
        .map(|token| format!("\"{}\"", token.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

pub fn open(path: &Path) -> Result<Connection, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建记忆索引目录失败：{e}"))?;
    }
    let conn = Connection::open(path).map_err(|e| format!("打开记忆索引失败：{e}"))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    conn.execute_batch("PRAGMA journal_mode = WAL;")
        .map_err(|e| format!("开启 WAL 失败：{e}"))?;

    // prepare 只吃批量 SQL 的第一条，所以建表和读版本必须分成两步
    conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);")
        .map_err(|e| format!("建版本表失败：{e}"))?;
    let version: i64 = conn
        .query_row("SELECT version FROM schema_version LIMIT 1", [], |row| {
            row.get(0)
        })
        .unwrap_or(0);

    if version != SCHEMA_VERSION {
        // 索引不是真相源，版本不对就整份丢掉重建，别写迁移脚本背历史包袱。
        // 唯一的例外是用量计数：injections 只住在 memory_usage 这一张表里
        // （reinforced_at 镜像回了 Markdown，次数没有），重建前先抢救出来、
        // 建完再插回去——否则升一次版本，每条记忆的使用频率分集体归零，
        // 检索里那 10% 的"使用频率"从此给不出任何区分
        let rescued: Vec<(String, i64, Option<String>)> =
            match conn.prepare("SELECT id, injections, last_injected_at FROM memory_usage") {
                Ok(mut statement) => statement
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, Option<String>>(2)?,
                        ))
                    })
                    .map_err(|e| e.to_string())
                    .and_then(|rows| {
                        rows.collect::<Result<Vec<_>, _>>()
                            .map_err(|e| e.to_string())
                    })
                    .unwrap_or_default(),
                // 全新库还没有这张表：没什么可抢救的
                Err(_) => Vec::new(),
            };
        conn.execute_batch(
            "DROP TABLE IF EXISTS memories_fts;
             DROP TABLE IF EXISTS memories;
             DROP TABLE IF EXISTS memory_links;
             DROP TABLE IF EXISTS memory_usage;
             DROP TABLE IF EXISTS memory_entity_links;
             DROP TABLE IF EXISTS memory_entities;
             DELETE FROM schema_version;",
        )
        .map_err(|e| e.to_string())?;
        conn.execute_batch(
            "CREATE TABLE memories (
                id TEXT PRIMARY KEY,
                path TEXT NOT NULL,
                type TEXT NOT NULL,
                scope TEXT NOT NULL,
                project_id TEXT,
                status TEXT NOT NULL,
                importance INTEGER NOT NULL,
                confidence REAL NOT NULL,
                stability TEXT NOT NULL,
                source TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                occurred_at TEXT,
                reinforced_at TEXT,
                origin TEXT,
                last_used_at TEXT,
                ttl_days INTEGER,
                tags TEXT NOT NULL,
                hash TEXT NOT NULL,
                body TEXT NOT NULL,
                sensitivity TEXT NOT NULL
            );
            CREATE INDEX idx_memories_path ON memories(path);
            CREATE INDEX idx_memories_scope ON memories(scope, project_id);
            CREATE INDEX idx_memories_status ON memories(status);

            CREATE VIRTUAL TABLE memories_fts USING fts5(
                id UNINDEXED,
                body,
                tokenize = 'unicode61 remove_diacritics 2'
            );

            CREATE TABLE memory_links (
                from_id TEXT NOT NULL,
                to_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                PRIMARY KEY (from_id, to_id, kind)
            );

            CREATE TABLE memory_usage (
                id TEXT PRIMARY KEY,
                injections INTEGER NOT NULL DEFAULT 0,
                last_injected_at TEXT
            );

            CREATE TABLE memory_entities (
                canonical TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                kind TEXT NOT NULL
            );

            CREATE TABLE memory_entity_links (
                from_id TEXT NOT NULL,
                canonical TEXT NOT NULL,
                PRIMARY KEY (from_id, canonical)
            );",
        )
        .map_err(|e| format!("建记忆索引表失败：{e}"))?;
        conn.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            params![SCHEMA_VERSION],
        )
        .map_err(|e| format!("写索引版本失败：{e}"))?;
        // 抢救回来的用量计数放回新表。尽力而为：单行插不上不该让整库打不开，
        // 最坏情况是那一条的使用分从零算起
        for (id, injections, last_injected_at) in &rescued {
            let _ = conn.execute(
                "INSERT OR IGNORE INTO memory_usage (id, injections, last_injected_at) VALUES (?1, ?2, ?3)",
                params![id, injections, last_injected_at],
            );
        }
    }

    Ok(conn)
}

fn hash_of(record: &MemoryRecord) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(record.to_markdown().as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 把一个文件的记录同步进索引：先删掉该 path 名下的所有行再插。
/// 用户手改/手删文件后重建走的也是这一条，所以"删掉一条记录"不需要额外通道
pub fn sync_file(conn: &Connection, path: &str, records: &[MemoryRecord]) -> Result<usize, String> {
    let keep: Vec<String> = records.iter().map(hash_of).collect();
    let unchanged: bool = match conn
        .prepare("SELECT hash FROM memories WHERE path = ?1")
        .map_err(|e| e.to_string())
    {
        Err(_) => false,
        Ok(mut statement) => {
            let rows = statement
                .query_map(params![path], |row| row.get::<_, String>(0))
                .map_err(|e| e.to_string())?
                .collect::<Result<Vec<String>, _>>()
                .map_err(|e| e.to_string())?;
            rows.len() == keep.len() && rows == keep
        }
    };
    if unchanged {
        return Ok(0);
    }

    conn.execute(
        "DELETE FROM memories_fts WHERE id IN (SELECT id FROM memories WHERE path = ?1)",
        params![path],
    )
    .map_err(|e| e.to_string())?;
    conn.execute(
        "DELETE FROM memory_links WHERE from_id IN (SELECT id FROM memories WHERE path = ?1)",
        params![path],
    )
    .map_err(|e| e.to_string())?;
    conn.execute("DELETE FROM memory_entity_links WHERE from_id IN (SELECT id FROM memories WHERE path = ?1)", params![path])
        .map_err(|e| e.to_string())?;
    conn.execute("DELETE FROM memories WHERE path = ?1", params![path])
        .map_err(|e| e.to_string())?;

    for record in records {
        conn.execute(
            "INSERT OR REPLACE INTO memories (
                id, path, type, scope, project_id, status, importance, confidence,
                stability, source, created_at, updated_at, occurred_at, reinforced_at, origin,
                last_used_at, ttl_days, tags, hash, body, sensitivity
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)",
            params![
                record.id,
                path,
                record.kind.as_str(),
                record.scope.as_str(),
                record.project_id,
                record.status.as_str(),
                record.importance as i64,
                record.confidence,
                record.stability.as_str(),
                record.source.as_str(),
                record.created_at,
                record.updated_at,
                record.occurred_at,
                record.reinforced_at,
                record.origin.as_ref().map(Origin::encode),
                record.last_used_at,
                record.ttl_days.map(|value| value as i64),
                record.tags.join(","),
                hash_of(record),
                record.content,
                record.sensitivity.as_str(),
            ],
        )
        .map_err(|e| format!("写入 {} 失败：{e}", record.id))?;
        conn.execute(
            "INSERT INTO memories_fts (id, body) VALUES (?1, ?2)",
            params![record.id, segment_for_index(&record.searchable())],
        )
        .map_err(|e| e.to_string())?;
        for target in &record.supersedes {
            conn.execute(
                "INSERT OR IGNORE INTO memory_links (from_id, to_id, kind) VALUES (?1, ?2, 'supersedes')",
                params![record.id, target],
            )
            .map_err(|e| e.to_string())?;
        }
        // 冲突标记与"用户已经裁决过两个都留"都在 frontmatter 的 extra 里，
        // 所以这两类边同样能整片重建。主键不变，只是多几种 kind
        for (peer, kind) in record
            .conflict_peers()
            .into_iter()
            .map(|peer| (peer, "conflicts_with"))
            .chain(
                record
                    .settled_peers()
                    .into_iter()
                    .map(|peer| (peer, "conflict_settled")),
            )
        {
            conn.execute(
                "INSERT OR IGNORE INTO memory_links (from_id, to_id, kind) VALUES (?1, ?2, ?3)",
                params![record.id, peer, kind],
            )
            .map_err(|e| e.to_string())?;
        }
        conn.execute(
            "INSERT OR IGNORE INTO memory_usage (id, injections, last_injected_at) VALUES (?1, 0, NULL)",
            params![record.id],
        )
        .map_err(|e| e.to_string())?;
        // 实体与"哪条记录讲到它"。它们只活在这里：`.md` 里多写一行就是第二个真相
        for entity in graph::entities_of(record) {
            conn.execute(
                "INSERT INTO memory_entities (canonical, name, kind) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(canonical) DO NOTHING",
                params![entity.canonical, entity.name, entity.kind.as_str()],
            )
            .map_err(|e| e.to_string())?;
            conn.execute(
                "INSERT OR IGNORE INTO memory_entity_links (from_id, canonical) VALUES (?1, ?2)",
                params![record.id, entity.canonical],
            )
            .map_err(|e| e.to_string())?;
        }
    }
    // 没有记录再讲到的实体就此消失。每同步一个文件都顺手扫一遍：它比"记得在删记录时也删实体"
    // 少一个会漏的分支，而两张表本来就是派生的，扫干净不丢任何事实
    conn.execute(
        "DELETE FROM memory_entities WHERE canonical NOT IN (SELECT canonical FROM memory_entity_links)",
        [],
    )
    .map_err(|e| e.to_string())?;

    Ok(records.len())
}

/// 检索用的行。`score` 越大越相关；`why` 是给人看的命中理由
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Hit {
    pub id: String,
    pub path: String,
    pub kind: String,
    pub scope: String,
    pub project_id: Option<String>,
    pub status: String,
    pub importance: u32,
    pub confidence: f64,
    pub stability: String,
    pub source: String,
    pub created_at: String,
    pub updated_at: String,
    /// 事情发生的时间。注入行里有它，模型才分得开"什么时候的事"和"什么时候记的"
    pub occurred_at: Option<String>,
    /// 新鲜度真正读的那一个时间。带上它是为了让 `/memory why` 能解释分数从哪来
    pub reinforced_at: Option<String>,
    pub ttl_days: Option<u32>,
    pub tags: Vec<String>,
    /// 外发分级。它是**读侧唯一判断这段内容能不能出门的地方**，所以它是类型而不是字符串：
    /// 一串文本谁都能拿去 `== "secret"`，写错一个字就是一次泄漏
    pub sensitivity: super::record::MemorySensitivity,
    pub content: String,
    pub injections: i64,
    pub score: f64,
    pub why: String,
}

pub struct SearchOptions<'a> {
    pub query: &'a str,
    pub project_id: Option<&'a str>,
    pub limit: usize,
    /// 权重：语义 / 重要性 / 新鲜度 / 作用域 / 使用频率
    pub weights: [f64; 5],
    /// 新鲜度半衰期（天）。它跟着这一份走而不是让 `decay` 自己读配置：
    /// 一次检索的所有分数必须能由这一份东西复算出来，`why` 才不是另一套数字
    pub half_life_days: f64,
}

fn row_to_hit(row: &rusqlite::Row<'_>) -> rusqlite::Result<Hit> {
    let tags: String = row.get(11)?;
    Ok(Hit {
        id: row.get(0)?,
        path: row.get(1)?,
        kind: row.get(2)?,
        scope: row.get(3)?,
        project_id: row.get(4)?,
        status: row.get(5)?,
        importance: row.get::<_, i64>(6)? as u32,
        confidence: row.get(7)?,
        stability: row.get(8)?,
        source: row.get(9)?,
        created_at: row.get(10)?,
        tags: tags
            .split(',')
            .filter(|item| !item.is_empty())
            .map(String::from)
            .collect(),
        updated_at: row.get(12)?,
        occurred_at: row.get(16)?,
        reinforced_at: row.get(17)?,
        ttl_days: row.get::<_, Option<i64>>(13)?.map(|value| value as u32),
        content: row.get(14)?,
        injections: row.get(15)?,
        sensitivity: super::record::MemorySensitivity::parse_loose(&row.get::<_, String>(18)?),
        // 这两格只是占位，真正的分数与理由在下面按模型重算并覆盖。它们按**列名**读：
        // `SELECT_COLUMNS` 后面接了什么由各条查询自己决定，位次一挪就串位（这次加
        // `sensitivity` 时红过一次），而名字不会
        score: row.get::<_, f64>("rank")?,
        why: row.get::<_, String>("why")?,
    })
}

/// 读一条 `Hit` 的列序。`rank` 与 `why` 由每条查询接在后面（第 19/20 列），
/// 所以这一串末尾加一列就等于给那几条查询各让出一位——位次只在 [`row_to_hit`] 一处解释
const SELECT_COLUMNS: &str =
    "m.id, m.path, m.type, m.scope, m.project_id, m.status, m.importance, \
     m.confidence, m.stability, m.source, m.created_at, m.tags, m.updated_at, m.ttl_days, m.body, \
     COALESCE(u.injections, 0), m.occurred_at, m.reinforced_at, m.sensitivity";

/// 新鲜度与使用增益都在 `decay` 里算：读侧的两个函数只有一处定义，
/// `score_of` 与 `explain` 才不会各拿一套数字（那样 "why" 就不可复算了）
fn fresh_of(hit: &Hit, half_life_days: f64) -> f64 {
    decay::freshness(
        &hit.created_at,
        hit.reinforced_at.as_deref(),
        half_life_days,
    )
}

fn used_of(hit: &Hit) -> f64 {
    decay::usage_gain(hit.injections)
}

pub fn search(conn: &Connection, options: &SearchOptions) -> Result<Vec<Hit>, String> {
    let match_query = build_match_query(options.query);
    let asked = !match_query.is_empty();

    // 候选池比最终条数宽一档：SQL 层若按 bm25 直接截到 limit，低字面相关但
    // 高重要/新鲜的记录根本进不了重排——那是检索侧最大的漏召回源。
    // 8×候选、64 起步，重排之后照旧只留 options.limit 条
    let pool: i64 = ((options.limit as i64) * 8).max(64);

    // 项目隔离下推到 SQL，谓词与 `keep_relevant` 同一条（非 project 作用域放行）。
    // 不推的话别的项目的行先把候选池占满，再被后置过滤整批丢掉——白占名额
    let (project_filter, project_arg): (String, Option<&str>) = match options.project_id {
        Some(id) => (
            " AND (m.scope <> 'project' OR m.project_id = ?2)".to_string(),
            Some(id),
        ),
        None => (" AND m.scope <> 'project'".to_string(), None),
    };

    // 有查询词就 JOIN FTS；没有就退化成按元数据取候选（/memory list 走这一支）。
    // 排第二档用的是"有效时间"而不是 updated_at：改个错别字不该把一条旧记忆顶到列表最前。
    // 占位符按参数是否存在动态编号：绑参数是按位次来的，?3 出现而 ?2 缺席就等于绑了个空
    let match_placeholder = if project_arg.is_some() { "?3" } else { "?2" };
    let sql = if asked {
        format!(
            "SELECT {SELECT_COLUMNS}, f.rank AS rank, 'fts' AS why FROM memories m \
             JOIN memories_fts f ON f.id = m.id \
             LEFT JOIN memory_usage u ON u.id = m.id \
             WHERE m.status = 'active'{project_filter} AND memories_fts MATCH {match_placeholder} \
             ORDER BY f.rank LIMIT ?1"
        )
    } else {
        format!(
            "SELECT {SELECT_COLUMNS}, 0.0 AS rank, '' AS why FROM memories m \
             LEFT JOIN memory_usage u ON u.id = m.id \
             WHERE m.status = 'active'{project_filter} \
             ORDER BY m.importance DESC, COALESCE(m.reinforced_at, m.created_at) DESC LIMIT ?1"
        )
    };

    let mut statement = conn.prepare(&sql).map_err(|e| e.to_string())?;
    // 参数要活到 query_map 之后：先在外层接住 &str，再借它的引用进参数表
    let project_slot: Option<&str> = project_arg;
    let mut args: Vec<&dyn rusqlite::ToSql> = vec![&pool];
    if let Some(ref id) = project_slot {
        args.push(id);
    }
    if asked {
        args.push(&match_query);
    }
    let mapped = statement
        .query_map(args.as_slice(), |row| {
            // 按列名读而不是按位次：`SELECT_COLUMNS` 加一列就会把所有位次往后挪一位，
            // 而这里要的是那条查询自己起的别名 `rank`
            Ok((row_to_hit(row)?, row.get::<_, f64>("rank")?))
        })
        .map_err(|e| e.to_string())?;
    let ranked: Vec<(Hit, f64)> = mapped
        .map(|row| row.map_err(|e| e.to_string()))
        .collect::<Result<Vec<_>, _>>()?;

    // 语义档取**批次内相对分**：bm25 的绝对值随查询词数与库规模漂移，
    // 除以常数 20 的标定从来没有依据，跨查询根本不可比。相对分让"这一批里
    // 最相关的那条"恒为 1.0，重要/新鲜/常用三档在同一把尺上才有意义
    let best_rank = ranked
        .iter()
        .map(|(_, rank)| *rank)
        .fold(f64::INFINITY, f64::min);
    let mut hits: Vec<Hit> = Vec::with_capacity(ranked.len());
    for (mut hit, rank) in ranked {
        let relevance = if asked && best_rank < 0.0 {
            (rank / best_rank).clamp(0.0, 1.0)
        } else {
            0.0
        };
        hit.score = score_of(&hit, options, relevance);
        hit.why = explain(&hit, options, relevance);
        hits.push(hit);
    }
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    hits.truncate(options.limit);
    Ok(hits)
}

/// 实体召回的相关度：一跳（查询里直接出现这个实体名）与两跳（一跳那批记录又讲到的实体）。
/// 它们不是"语义分"，所以拿不到 FTS 那一档的满分；两跳明显比一跳弱，这个差是人为定的，
/// 写在这里而不是散在 SQL 里
const ENTITY_RELEVANCE: [f64; 2] = [0.5, 0.2];

/// 每一跳最多扫这么多邻接记录。一个被两百条记忆提到的实体，不该把整张表读一遍才吐出几条提示
const SCAN_CAP: i64 = 64;

/// 实体顺出来的那些记录，与它们为什么被带出来
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphHit {
    pub hit: Hit,
    /// 第几跳（1 起）与是哪个实体把它带出来的。提示要说"因为都提到了张三"，
    /// 不能只说"相关"——说不出为什么的推荐就是打扰
    pub hop: usize,
    pub entity: String,
}

/// 检索 + 实体召回（T07）。**注入通道走的是 `search`，不是这里**：主动回忆只提示，
/// 让推导去改发送内容就等于开了第二条注入通道。
///
/// 分两列返回而不是拼成一个 `Vec`：语义命中和"聊到同一个东西"不是同一个问题，
/// 混在一列里就分不清哪条是谁带出来的了。召回只往第二列里加，
/// 第一列一个字都不动
pub fn recall(
    conn: &Connection,
    options: &SearchOptions,
) -> Result<(Vec<Hit>, Vec<GraphHit>), String> {
    let semantic = search(conn, options)?;
    let mut taken = semantic.len();
    let mut seen: Vec<String> = semantic.iter().map(|hit| hit.id.clone()).collect();
    let mut graph: Vec<GraphHit> = Vec::new();

    let query = options.query.to_ascii_lowercase();
    if query.trim().is_empty() {
        return Ok((semantic, graph));
    }
    let mut frontier = entities_named_in(conn, &query)?;
    let mut hop = 0usize;
    while hop < ENTITY_RELEVANCE.len() && !frontier.is_empty() && taken < options.limit {
        let neighbours = records_of_entities(conn, &frontier, SCAN_CAP.max(options.limit as i64))?;
        let mut touched: Vec<String> = Vec::new();
        for (entity, id) in neighbours {
            touched.push(id.clone());
            if taken >= options.limit || seen.iter().any(|held| held == &id) {
                continue;
            }
            let Some(mut hit) = hit_by_id(conn, &id)? else {
                continue;
            };
            hit.score = score_of(&hit, options, ENTITY_RELEVANCE[hop]);
            hit.why = format!("实体命中（第 {} 跳）：{}", hop + 1, entity);
            seen.push(id);
            graph.push(GraphHit {
                hit,
                hop: hop + 1,
                entity,
            });
            taken += 1;
        }
        // 下一跳的 frontier：这一跳扫到的那些记录讲到的、还没走过的实体。同一个实体不回头走，
        // 否则两条记录会互相把对方拖进来，深度限制就只是装饰
        frontier = next_entities(conn, &touched, &frontier)?;
        hop += 1;
    }
    Ok((semantic, graph))
}

/// 按 id 取一条命中的完整行。实体召回带进来的记录要能用**同一套** `score_of` 打分，
/// 所以它得走同一个投影形状，不能另拼一份
fn hit_by_id(conn: &Connection, id: &str) -> Result<Option<Hit>, String> {
    let sql = format!(
        "SELECT {SELECT_COLUMNS}, 0.0 AS rank, '' AS why FROM memories m \
         LEFT JOIN memory_usage u ON u.id = m.id WHERE m.id = ?1"
    );
    let mut statement = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let mut rows = statement
        .query_map(params![id], row_to_hit)
        .map_err(|e| e.to_string())?;
    match rows.next() {
        Some(row) => row.map(Some).map_err(|e| e.to_string()),
        None => Ok(None),
    }
}

/// 一批实体的邻接记录，按 (实体, 记录) 的稳定次序返回
fn records_of_entities(
    conn: &Connection,
    entities: &[String],
    limit: i64,
) -> Result<Vec<(String, String)>, String> {
    if entities.is_empty() {
        return Ok(Vec::new());
    }
    let marks: Vec<String> = entities.iter().map(|_| "?".to_string()).collect();
    let sql = format!(
        "SELECT l.canonical, l.from_id FROM memory_entity_links l \
         JOIN memories m ON m.id = l.from_id \
         WHERE l.canonical IN ({}) AND m.status = 'active' \
         ORDER BY l.canonical, l.from_id LIMIT ?{}",
        marks.join(","),
        entities.len() + 1
    );
    let mut statement = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let mut args: Vec<&dyn rusqlite::ToSql> = entities
        .iter()
        .map(|held| held as &dyn rusqlite::ToSql)
        .collect();
    args.push(&limit);
    let rows = statement
        .query_map(args.as_slice(), |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<(String, String)>, _>>()
        .map_err(|e| e.to_string())
}

/// 查询串里出现过的实体名（≥2 个字符）。单字不算：那会把大半库拖进候选，
/// 而"召回变多"不等于"召回变准"。
///
/// 子串命中的误报要滤一道：问"张三丰"的事不该把实体「张三」顺带点亮。
/// 规矩是**长名优先认领**——短名的每一处出现都被某个已认领的长名盖住时才让位。
/// 中文没有词边界，这是不引分词器前提下能守住的一条线；长名没登记时短名照常命中
fn entities_named_in(conn: &Connection, query: &str) -> Result<Vec<String>, String> {
    let mut statement = conn
        .prepare(
            "SELECT canonical FROM memory_entities \
             WHERE length(name) >= 2 AND instr(?1, lower(name)) > 0 ORDER BY canonical",
        )
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map(params![query], |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    let mut candidates = rows
        .collect::<Result<Vec<String>, _>>()
        .map_err(|e| e.to_string())?;
    candidates.sort_by_key(|name| std::cmp::Reverse(name.chars().count()));

    let lowered = query.to_lowercase();
    let mut kept_spans: Vec<(usize, usize)> = Vec::new();
    let mut kept: Vec<String> = Vec::new();
    for name in candidates {
        let needle = name.to_lowercase();
        let mut spans: Vec<(usize, usize)> = Vec::new();
        let mut at = 0usize;
        while let Some(found) = lowered[at..].find(&needle) {
            let start = at + found;
            at = start + needle.len();
            spans.push((start, at));
        }
        if spans.is_empty() {
            // SQL 依大小写折叠过的规则命中了而这里找不到：放行，别把真命中滤丢
            kept.push(name);
            continue;
        }
        let covered_everywhere = spans
            .iter()
            .all(|span| kept_spans.iter().any(|(s, e)| span.0 >= *s && span.1 <= *e));
        if covered_everywhere {
            continue;
        }
        kept_spans.extend(spans);
        kept.push(name);
    }
    kept.sort();
    Ok(kept)
}

/// 一批记录讲到的、本轮还没走过的实体
fn next_entities(
    conn: &Connection,
    records: &[String],
    walked: &[String],
) -> Result<Vec<String>, String> {
    if records.is_empty() {
        return Ok(Vec::new());
    }
    let marks: Vec<String> = records.iter().map(|_| "?".to_string()).collect();
    let sql = format!(
        "SELECT DISTINCT canonical FROM memory_entity_links WHERE from_id IN ({}) \
         ORDER BY canonical",
        marks.join(",")
    );
    let mut statement = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let args: Vec<&dyn rusqlite::ToSql> = records
        .iter()
        .map(|held| held as &dyn rusqlite::ToSql)
        .collect();
    let rows = statement
        .query_map(args.as_slice(), |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    let all = rows
        .collect::<Result<Vec<String>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(all
        .into_iter()
        .filter(|held| !walked.iter().any(|done| done == held))
        .collect())
}

/// 时间线上的一格。`at` 是事情发生的时间，不是记录被写下的时间——T06 的判据就是这两个
/// 不一致时按前者排
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TimelineRow {
    pub at: String,
    pub record_id: String,
    pub kind: String,
    /// 这条记录讲到的第一个实体（按 canonical 排）。没有就是 `None`，不拿记录标题冒充实体
    pub entity: Option<String>,
    /// 那条说过什么。只有日期和类型的时间线读不出"发生过什么事"
    pub content: String,
}

/// 说过"什么时候发生"的那些记录，按 `occurred_at` 排。
///
/// `precedes` 就是这份投影的**次序本身**，不落 `memory_links`：谁在谁之前取决于全库，
/// 而 `sync_file` 是按文件重算的——把全局关系写成逐文件维护的边，就是给"两个真相"开门
pub fn timeline(conn: &Connection, limit: usize) -> Result<Vec<TimelineRow>, String> {
    let mut statement = conn
        .prepare(
            "SELECT m.id, m.type, m.occurred_at, \
                    (SELECT MIN(l.canonical) FROM memory_entity_links l WHERE l.from_id = m.id), \
                    m.body \
             FROM memories m WHERE m.occurred_at IS NOT NULL AND m.status = 'active' \
             ORDER BY m.occurred_at DESC, m.id LIMIT ?1",
        )
        .map_err(|e| e.to_string())?;
    let limit = limit as i64;
    let rows = statement
        .query_map(params![limit], |row| {
            Ok(TimelineRow {
                record_id: row.get(0)?,
                kind: row.get(1)?,
                at: row.get(2)?,
                entity: row.get(3)?,
                content: row.get(4)?,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<TimelineRow>, _>>()
        .map_err(|e| e.to_string())
}

fn score_of(hit: &Hit, options: &SearchOptions, relevance: f64) -> f64 {
    let [w_semantic, w_importance, w_fresh, w_scope, w_used] = options.weights;
    let importance = hit.importance as f64 / 5.0;
    let scope = scope_weight(hit, options.project_id);
    w_semantic * relevance
        + w_importance * importance
        + w_fresh * fresh_of(hit, options.half_life_days)
        + w_scope * scope
        + w_used * used_of(hit)
}

fn scope_weight(hit: &Hit, active_project: Option<&str>) -> f64 {
    let parsed: MemoryScope = hit.scope.parse().unwrap_or(MemoryScope::Global);
    match parsed {
        // 别的项目的记忆永远拿不到项目作用域的权重——这正是"项目 A 不出现在项目 B"
        MemoryScope::Project => {
            if active_project.is_some() && hit.project_id.as_deref() == active_project {
                parsed.weight()
            } else {
                0.0
            }
        }
        other => other.weight(),
    }
}

fn explain(hit: &Hit, options: &SearchOptions, relevance: f64) -> String {
    let [w_semantic, w_importance, w_fresh, w_scope, w_used] = options.weights;
    format!(
        "语义 {:.2}×{:.2} · 重要 {:.2}×{:.2} · 新鲜 {:.2}×{:.2} · 作用域 {:.2}×{:.2} · 常用 {:.2}×{:.2}",
        w_semantic,
        relevance,
        w_importance,
        hit.importance as f64 / 5.0,
        w_fresh,
        fresh_of(hit, options.half_life_days),
        w_scope,
        scope_weight(hit, options.project_id),
        w_used,
        used_of(hit),
    )
}

pub fn forget(conn: &Connection, id: &str) -> Result<usize, String> {
    let deleted = conn
        .execute("DELETE FROM memories WHERE id = ?1", params![id])
        .map_err(|e| e.to_string())?;
    conn.execute("DELETE FROM memories_fts WHERE id = ?1", params![id])
        .map_err(|e| e.to_string())?;
    conn.execute("DELETE FROM memory_usage WHERE id = ?1", params![id])
        .map_err(|e| e.to_string())?;
    Ok(deleted)
}

pub fn note_injection(conn: &Connection, ids: &[String], at: &str) -> Result<(), String> {
    for id in ids {
        conn.execute(
            "INSERT INTO memory_usage (id, injections, last_injected_at) VALUES (?1, 1, ?2)
             ON CONFLICT(id) DO UPDATE SET injections = injections + 1, last_injected_at = excluded.last_injected_at",
            params![id, at],
        )
        .map_err(|e| e.to_string())?;
        // reinforced_at 在这里只是索引镜像：真正让下次重建还认得它的那一份写在
        // Markdown 的 frontmatter 里（mod.rs 的强化章），一天一次
        conn.execute(
            "UPDATE memories SET last_used_at = ?2, reinforced_at = ?2 WHERE id = ?1",
            params![id, at],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 一条记忆的来历：哪次对话、哪几条消息、住在哪个文件、被注入过几次。
/// 问的是索引，因为索引就是那份 frontmatter 的投影；正文一个字都不从这里出
pub fn source_of(conn: &Connection, id: &str) -> Result<SourceView, String> {
    let row = conn
        .query_row(
            "SELECT m.path, m.origin, m.created_at, m.occurred_at, m.reinforced_at, \
                COALESCE(u.injections, 0), u.last_injected_at \
         FROM memories m LEFT JOIN memory_usage u ON u.id = m.id WHERE m.id = ?1",
            params![id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            },
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => format!("索引里没有记忆 {id}。先跑一次重建。"),
            other => other.to_string(),
        })?;
    let (file, raw_origin, created_at, occurred_at, reinforced_at, injections, last_injected_at) =
        row;
    let origin = match raw_origin.as_deref() {
        Some(text) => Some(Origin::decode(text)?),
        None => None,
    };
    Ok(SourceView {
        record_id: id.to_string(),
        file,
        origin,
        created_at,
        occurred_at,
        reinforced_at,
        injections,
        last_injected_at,
    })
}

pub fn paths_in(conn: &Connection) -> Result<Vec<String>, String> {
    let mut statement = conn
        .prepare("SELECT DISTINCT path FROM memories")
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<_, _>>().map_err(|e| e.to_string())
}

pub fn count(conn: &Connection, status: Option<MemoryStatus>) -> Result<i64, String> {
    match status {
        None => conn
            .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))
            .map_err(|e| e.to_string()),
        Some(status) => conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE status = ?1",
                params![status.as_str()],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::record::{MemoryRecord, MemoryScope};
    use std::time::{Duration, Instant};

    /// 目标数字是产品要求，它承诺的是发布态的应用。debug 构建里 SQLite 的每次自动提交
    /// 都要 fsync，这部分不随优化等级变——所以门槛按构建模式给：同一个数字在两种模式下
    /// 不是同一个承诺。发布态那一档由 `cargo test --release` 真量一次，不是靠推测
    const TARGET: Duration = if cfg!(debug_assertions) {
        Duration::from_millis(300)
    } else {
        Duration::from_millis(100)
    };
    const BUILD_TARGET: Duration = if cfg!(debug_assertions) {
        Duration::from_millis(150)
    } else {
        Duration::from_millis(50)
    };

    /// 版本不符触发整库重建时，用量计数要活着出来：injections 只住在 memory_usage
    /// 这一张表里（reinforced_at 镜像回了 Markdown，次数没有），丢了它，检索里
    /// 那一档"使用频率"在升级后集体归零，热度偏置还要反过来固化头部那批
    #[test]
    fn a_schema_rebuild_keeps_the_usage_counts() {
        let dir = std::env::temp_dir().join(format!("aglab-index-usage-{}", std::process::id()));
        let db = dir.join("index.sqlite");
        {
            let conn = open(&db).expect("首建该成功");
            conn.execute(
                "INSERT INTO memory_usage (id, injections, last_injected_at) VALUES ('mem-1', 7, '2026-09-26T09:00:00+08:00')",
                [],
            )
            .expect("记一笔用量该成功");
        }
        // 直接把版本号改坏，模拟一次 schema 升级
        {
            let raw = Connection::open(&db).expect("裸开该成功");
            raw.execute("UPDATE schema_version SET version = 999", [])
                .expect("改版本该成功");
        }
        let conn = open(&db).expect("重建该成功");
        let injections: i64 = conn
            .query_row(
                "SELECT injections FROM memory_usage WHERE id = 'mem-1'",
                [],
                |row| row.get(0),
            )
            .expect("用量行要在重建后活着");
        assert_eq!(injections, 7, "重建吃掉用量计数，使用频率分就全废了");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 候选池比最终条数宽：12 条字面全中但又老又不重要的记录，不能把一条
    /// 字面只沾一半、却重要且新鲜的记忆挤出重排——旧实现里它根本进不了候选
    #[test]
    fn a_high_importance_record_enters_the_pool_even_when_its_words_rank_low() {
        let dir = std::env::temp_dir().join(format!("aglab-index-pool-{}", std::process::id()));
        let conn = open(&dir.join("index.sqlite")).expect("建索引该成功");
        let mut records: Vec<MemoryRecord> = (0..12)
            .map(|index| {
                let mut item = MemoryRecord::draft(
                    MemoryScope::Global,
                    &format!("部署迁移手册 第 {index} 版，部署迁移之前先备份。"),
                );
                item.importance = 1;
                item.created_at = "2020-01-01T00:00:00+08:00".into();
                item
            })
            .collect();
        let mut star =
            MemoryRecord::draft(MemoryScope::Global, "部署迁移之前要跑一遍数据库迁移演练。");
        star.importance = 5;
        let star_id = star.id.clone();
        records.push(star);
        sync_file(&conn, "global/MEMORY.md", &records).expect("入库该成功");

        let options = SearchOptions {
            query: "部署迁移",
            project_id: None,
            limit: 8,
            weights: [0.50, 0.20, 0.15, 0.10, 0.05],
            half_life_days: crate::memory::decay::HALF_LIFE_DAYS,
        };
        let hits = search(&conn, &options).expect("检索该成功");
        assert!(
            hits.iter().any(|hit| hit.id == star_id),
            "又重要又新鲜的那条要进得来：{:?}",
            hits.iter()
                .map(|hit| (hit.id.clone(), hit.score))
                .collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 项目隔离下推到候选阶段：别的项目的行不许先把候选池占满再被整批丢掉，
    /// 那等于让当前项目自己的记忆陪跑
    #[test]
    fn another_projects_records_do_not_eat_the_candidate_pool() {
        let dir = std::env::temp_dir().join(format!("aglab-index-scope-{}", std::process::id()));
        let conn = open(&dir.join("index.sqlite")).expect("建索引该成功");
        let mut records: Vec<MemoryRecord> = (0..12)
            .map(|index| {
                let mut item = MemoryRecord::draft(
                    MemoryScope::Project,
                    &format!("部署流水线手册 第 {index} 版，部署流水线要绿了才能合并。"),
                );
                item.project_id = Some("p1".into());
                item.importance = 5;
                item
            })
            .collect();
        for index in 0..3 {
            let mut item = MemoryRecord::draft(
                MemoryScope::Global,
                &format!("部署流水线备忘 第 {index} 条。"),
            );
            item.importance = 1;
            item.created_at = "2020-01-01T00:00:00+08:00".into();
            records.push(item);
        }
        let mut star = MemoryRecord::draft(
            MemoryScope::Global,
            "部署流水线这件事要先跑一遍数据库迁移。",
        );
        star.importance = 5;
        let star_id = star.id.clone();
        records.push(star);
        sync_file(&conn, "global/MEMORY.md", &records).expect("入库该成功");

        let options = SearchOptions {
            query: "部署流水线",
            project_id: Some("p2"),
            limit: 8,
            weights: [0.50, 0.20, 0.15, 0.10, 0.05],
            half_life_days: crate::memory::decay::HALF_LIFE_DAYS,
        };
        let hits = search(&conn, &options).expect("检索该成功");
        assert!(
            hits.iter().any(|hit| hit.id == star_id),
            "本项目的候选不该被别的项目挤光：{:?}",
            hits.iter()
                .map(|hit| (hit.scope.clone(), hit.project_id.clone()))
                .collect::<Vec<_>>()
        );
        assert!(
            hits.iter().all(|hit| hit.scope != "project"),
            "p1 的行不该出现在 p2 的检索里"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 长查询里单字不再进 MATCH（它们几乎命中全库，稀释 bm25 区分度）；
    /// 整条查询凑不出双字才退回单字，兜底那一档滤掉虚词
    #[test]
    fn long_queries_go_bigram_first_and_single_chars_fall_back() {
        let query = build_match_query("部署流水线");
        assert!(!query.contains("\"部\""), "单字不该出现在长查询里：{query}");
        assert!(
            query.contains("\"部署\"") && query.contains("\"流水\""),
            "{query}"
        );

        assert_eq!(build_match_query("的"), "", "虚词单字不该独自成查询");
        assert_eq!(
            build_match_query("记"),
            "\"记\"",
            "实词单字在无双字可用时兜底"
        );
    }

    /// 短名被长名盖住时让位：问张三丰的事不该把「张三」的记忆全带出来；
    /// 长名不在场时短名照常命中
    #[test]
    fn a_short_entity_yields_to_a_longer_name_that_covers_it() {
        let dir = std::env::temp_dir().join(format!("aglab-index-entity-{}", std::process::id()));
        let conn = open(&dir.join("index.sqlite")).expect("建索引该成功");
        conn.execute(
            "INSERT INTO memory_entities (canonical, name, kind) VALUES ('张三', '张三', 'person')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO memory_entities (canonical, name, kind) VALUES ('张三丰', '张三丰', 'person')",
            [],
        )
        .unwrap();

        let named = entities_named_in(&conn, "帮张三丰排一下下周的班").expect("该成功");
        assert_eq!(named, vec!["张三丰"], "短名被长名覆盖时让位：{named:?}");

        let named = entities_named_in(&conn, "张三的请假记录").expect("该成功");
        assert_eq!(named, vec!["张三"], "长名不在场时短名照常命中：{named:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 10k 条记忆的检索基准。量的是**最坏那一次**而不是平均值：用户在意的是一次卡顿，
    /// 不是一百次的均摊。默认不跑——它要建一万行，塞进常规套件里会把每个无关改动
    /// 的人都拖慢一次。手动跑：`cargo test --lib -- --ignored`
    #[test]
    #[ignore = "10k 条基准，只在专门测性能时跑"]
    fn search_stays_under_100ms_at_10k_records() {
        let dir = std::env::temp_dir().join(format!("aglab-memory-bench-{}", std::process::id()));
        let conn = open(&dir.join("index.sqlite")).expect("建索引该成功");
        let records: Vec<MemoryRecord> = (0..10_000)
            .map(|index| {
                MemoryRecord::draft(
                    MemoryScope::Global,
                    &format!("第 {index} 条：用户偏好结论先行，构建 {index} 号流水线的部署脚本"),
                )
            })
            .collect();

        // 种数据整段包在一个事务里：一万次自动提交会先把建索引变成几分钟，
        // 而这一轮要量的是读，不是写。写路径自己的开销由 sync_file 的调用方负责
        conn.execute_batch("BEGIN").expect("开事务该成功");
        let seeded = Instant::now();
        sync_file(&conn, "global/MEMORY.md", &records).expect("入库该成功");
        conn.execute_batch("COMMIT").expect("提交该成功");
        assert_eq!(count(&conn, None).expect("数一下该成功"), 10_000);
        eprintln!("建索引 10k 条用时 {:?}", seeded.elapsed());

        // 闭包会把 &str 的生命周期跟自己的调用绑在一起，返回带引用的结构体过不去；
        // 写成嵌套 fn 让生命周期回到正常的 elision 规则上
        fn options_for(query: &str) -> SearchOptions<'_> {
            SearchOptions {
                query,
                project_id: None,
                limit: 8,
                weights: [0.45, 0.20, 0.15, 0.10, 0.10],
                half_life_days: crate::memory::decay::HALF_LIFE_DAYS,
            }
        }
        let mut slowest = Duration::ZERO;
        for round in 0..20 {
            for query in ["部署脚本", "结论先行", "流水线", "构建", "用户偏好"] {
                let started = Instant::now();
                let hits = search(&conn, &options_for(query)).expect("检索该成功");
                let elapsed = started.elapsed();
                assert!(
                    !hits.is_empty(),
                    "第 {round} 轮查「{query}」一条都没命中，那这个基准没在量它声称的东西"
                );
                slowest = slowest.max(elapsed);
            }
        }
        // 空查询那一支（/memory list 与纯重新生成走的）也在同一份数据上过一遍
        let started = Instant::now();
        let listed = search(&conn, &options_for("")).expect("按元数据取候选该成功");
        assert_eq!(listed.len(), 8, "限定 8 条就该回 8 条");
        slowest = slowest.max(started.elapsed());

        eprintln!("最坏一次检索 {slowest:?}（目标 {TARGET:?}），数据量 10k");
        assert!(
            slowest < TARGET,
            "10k 条记忆下单次检索最坏 {slowest:?}，超过 {TARGET:?} 的目标"
        );
        // 另一半目标：注入构建。它比裸检索多一遍常驻段拼装和预算裁剪，
        // 所以不能拿检索的数当它的数
        let paths = crate::memory::Paths::new(dir.clone());
        let config = crate::memory::MemoryConfig::default();
        let mut build_worst = Duration::ZERO;
        for _ in 0..20 {
            let started = Instant::now();
            let shot = crate::memory::inject::build(&conn, &paths, &config, "部署脚本", None, None)
                .expect("注入构建该成功");
            assert!(
                shot.is_some(),
                "一万条 active 记录却什么都没构建出来，那这一路根本没被测到"
            );
            build_worst = build_worst.max(started.elapsed());
        }
        eprintln!("最坏一次注入构建 {build_worst:?}（目标 {BUILD_TARGET:?}），数据量 10k");
        assert!(
            build_worst < BUILD_TARGET,
            "10k 条记忆下注入构建最坏 {build_worst:?}，超过 {BUILD_TARGET:?} 的目标"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
