//! 用量与费用台账。
//!
//! 单独一个 usage.db，不跟 conversationStore 的 json/sqlite 开关走：
//! 记账是分析数据，切换对话存储不该把历史花费一起搬走或丢掉。
//!
//! 一次模型请求一行，失败也记一行——"这个服务商今天挂了几次"只有台账答得了。

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::config::AppConfig;

/// 费用按 1e-8 美元存成整数：浮点累加会漂，i64 不会，
/// 而 1e-8 比最便宜的单次请求费用还细好几个数量级
const COST_SCALE: f64 = 1e8;
const PER_MILLION: f64 = 1e6;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS requests (
    id                 INTEGER PRIMARY KEY,
    ts                 INTEGER NOT NULL,
    scene              TEXT NOT NULL DEFAULT 'chat',
    conversation_id    TEXT NOT NULL DEFAULT '',
    base_url           TEXT NOT NULL DEFAULT '',
    model              TEXT NOT NULL,
    api_format         TEXT NOT NULL DEFAULT 'chat',
    input_tokens       INTEGER NOT NULL DEFAULT 0,
    -- 这一发放上 wire 的请求体字符数。它是字符↔token 系数的另一头：没有它，
    -- `input_tokens` 永远配不上对，"估算"就只能一直冒充（Context 设计 §0 的 P2）
    sent_chars         INTEGER NOT NULL DEFAULT 0,
    output_tokens      INTEGER NOT NULL DEFAULT 0,
    cached_tokens      INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    -- 这一笔的 cached_tokens 是服务商真回的，还是"它压根没上报这个字段"。
    -- 没有这一列，未上报的服务商会永久显示 0% 命中，看起来像前缀被改坏了
    cache_reported     INTEGER NOT NULL DEFAULT 0,
    -- 这一笔是否**开启**了一条新的缓存链（压缩边界在它之前、旧的共同前缀已经不被发送）。
    -- 白付量靠它区分"前缀被改坏了"和"压缩本来就要付这一次"，见 wasted_tokens
    chain_reset        INTEGER NOT NULL DEFAULT 0,
    reasoning_tokens   INTEGER NOT NULL DEFAULT 0,
    cost_usd_e8        INTEGER NOT NULL DEFAULT 0,
    priced             INTEGER NOT NULL DEFAULT 0,
    latency_ms         INTEGER NOT NULL DEFAULT 0,
    first_token_ms     INTEGER,
    ok                 INTEGER NOT NULL DEFAULT 1,
    error              TEXT NOT NULL DEFAULT ''
);

CREATE INDEX IF NOT EXISTS idx_requests_ts ON requests (ts DESC);
CREATE INDEX IF NOT EXISTS idx_requests_model ON requests (model);

CREATE TABLE IF NOT EXISTS model_pricing (
    model_id            TEXT PRIMARY KEY,
    display_name        TEXT NOT NULL DEFAULT '',
    input_usd_per_m     TEXT NOT NULL DEFAULT '0',
    output_usd_per_m    TEXT NOT NULL DEFAULT '0',
    cache_read_usd_per_m TEXT NOT NULL DEFAULT '0',
    cache_creation_usd_per_m TEXT NOT NULL DEFAULT '0',
    updated_at          INTEGER NOT NULL DEFAULT 0
);
"#;

#[derive(Clone, Copy, Default)]
pub struct Tokens {
    pub input: u32,
    pub output: u32,
    pub cached: Option<u32>,
    pub cache_write: u32,
    pub reasoning: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct Price {
    pub model_id: String,
    pub display_name: String,
    /// 每百万 token 的美元价。用字符串存：小数文本不会有二进制浮点的表示误差
    pub input_usd_per_m: String,
    pub output_usd_per_m: String,
    pub cache_read_usd_per_m: String,
    pub cache_creation_usd_per_m: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    pub requests: i64,
    pub failed: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: i64,
    pub reasoning_tokens: i64,
    pub cost_usd: f64,
    /// 没有匹配到价格表的请求数。它们按 0 记账，界面必须说出来，
    /// 否则"这个月只花了 3 块"可能只是模型名没对上
    pub unpriced_requests: i64,
    /// 服务商没回缓存字段的请求数。cached_tokens 只统计已上报的那些，
    /// 不报这个数就会让"命中 0"和"没数据"长得一模一样
    pub unreported_cache_requests: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelUsage {
    pub model: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: i64,
    pub cost_usd: f64,
    pub priced: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DayUsage {
    pub date: String,
    pub requests: i64,
    pub cost_usd: f64,
}

/// 按话题聚合的一行。conversation 是后端话题 id；空串 = 那一发没挂在具体话题上
/// （起标题这类边角请求），前端按"未归属"显示
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationUsage {
    pub conversation: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub totals: Totals,
    pub by_model: Vec<ModelUsage>,
    pub daily: Vec<DayUsage>,
}

fn db_path(app: &AppHandle) -> Result<PathBuf, String> {
    db_path_in(&app.path().app_config_dir().map_err(|e| e.to_string())?)
}

fn db_path_in(config_dir: &std::path::Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(config_dir).map_err(|e| e.to_string())?;
    Ok(config_dir.join("usage.db"))
}

/// worker 进程的变体（M2 切片 3）：目录由 Main 经 CLI 传来，
/// 打开 + WAL + SCHEMA + 补列一整套——与主进程读到的同一份库
pub(crate) fn open_in(config_dir: &std::path::Path) -> Result<Connection, String> {
    open(&db_path_in(config_dir)?)
}

fn open(file: &Path) -> Result<Connection, String> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let conn = Connection::open(file).map_err(|e| format!("打开用量台账失败：{e}"))?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    conn.execute_batch("PRAGMA journal_mode = WAL;")
        .map_err(|e| format!("初始化用量台账失败：{e}"))?;
    conn.execute_batch(SCHEMA).map_err(|e| e.to_string())?;
    migrate_columns(&conn).map_err(|e| format!("给用量台账补列失败：{e}"))?;
    Ok(conn)
}

/// 给已存在的表补列。`CREATE TABLE IF NOT EXISTS` 不会加字段，老库必须升上来才能读。
fn ensure_column(conn: &Connection, column: &str, declaration: &str) -> Result<(), String> {
    match conn.execute(
        &format!("ALTER TABLE requests ADD COLUMN {column} {declaration}"),
        [],
    ) {
        Ok(_) => {}
        // 只有"列已存在"这一种报错可以吞掉，别的全当失败——否则会留下一张缺列的表
        Err(rusqlite::Error::SqliteFailure(_, Some(msg)))
            if msg.contains("duplicate column name") => {}
        Err(e) => return Err(e.to_string()),
    }
    Ok(())
}

/// 老库补列 + 回填。
///
/// `cache_reported` 的回填依据是一条永久不变量：写入时 `cached_tokens` 与 `cache_reported`
/// 同源于 `Tokens::cached`，所以 `cached_tokens > 0` 的行必然出自"服务商真的上报了"。
/// 于是这条 UPDATE 可以每次打开都跑——它只会修好 D7 之前留下的老行，
/// 永远不会把新写的行改错，也就覆盖了"列已存在但从没回填过"的那种库。
/// `chain_reset` 没有可回填的依据：老行当时没记"这一轮是不是压缩后第一笔"，
/// 只能留 0（当作链上普通一笔），代价是老话题里每次压缩多报一笔白付
fn migrate_columns(conn: &Connection) -> Result<(), String> {
    ensure_column(conn, "cache_reported", "INTEGER NOT NULL DEFAULT 0")?;
    ensure_column(conn, "chain_reset", "INTEGER NOT NULL DEFAULT 0")?;
    ensure_column(conn, "sent_chars", "INTEGER NOT NULL DEFAULT 0")?;
    conn.execute(
        "UPDATE requests SET cache_reported = 1 WHERE cached_tokens > 0 AND cache_reported = 0",
        [],
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

pub(crate) fn parse_price(raw: &str) -> f64 {
    raw.trim().parse::<f64>().unwrap_or(0.0).max(0.0)
}

/// 一次请求值多少钱。
/// 服务商回的 input_tokens 已经把命中缓存的部分算在里面（chat 和 responses 都这样），
/// 所以按全价计费的输入要先把它减掉，否则命中缓存反而更贵。
/// 服务商没上报命中量时按全价算：宁可多报成本，也不凭空打个不存在的折扣
pub fn cost_e8(price: &Price, tokens: &Tokens) -> i64 {
    let cached = tokens.cached.unwrap_or(0);
    let billable_input = tokens.input.saturating_sub(cached) as f64;
    let raw = billable_input * parse_price(&price.input_usd_per_m)
        + cached as f64 * parse_price(&price.cache_read_usd_per_m)
        + tokens.cache_write as f64 * parse_price(&price.cache_creation_usd_per_m)
        + tokens.output as f64 * parse_price(&price.output_usd_per_m);
    (raw * COST_SCALE / PER_MILLION).round() as i64
}

pub fn price_for(conn: &Connection, model: &str) -> Result<Option<Price>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT model_id, display_name, input_usd_per_m, output_usd_per_m,
                    cache_read_usd_per_m, cache_creation_usd_per_m
             FROM model_pricing
             WHERE model_id = ?1 COLLATE NOCASE OR display_name = ?1 COLLATE NOCASE
             LIMIT 1",
        )
        .map_err(|e| e.to_string())?;
    let row = stmt
        .query_row(params![model], |row| {
            Ok(Price {
                model_id: row.get(0)?,
                display_name: row.get(1)?,
                input_usd_per_m: row.get(2)?,
                output_usd_per_m: row.get(3)?,
                cache_read_usd_per_m: row.get(4)?,
                cache_creation_usd_per_m: row.get(5)?,
            })
        })
        .ok();
    Ok(row)
}

pub fn upsert_price(conn: &Connection, price: &Price, now: i64) -> Result<(), String> {
    conn.execute(
        "INSERT INTO model_pricing (model_id, display_name, input_usd_per_m, output_usd_per_m,
                                    cache_read_usd_per_m, cache_creation_usd_per_m, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(model_id) DO UPDATE SET
            display_name = excluded.display_name,
            input_usd_per_m = excluded.input_usd_per_m,
            output_usd_per_m = excluded.output_usd_per_m,
            cache_read_usd_per_m = excluded.cache_read_usd_per_m,
            cache_creation_usd_per_m = excluded.cache_creation_usd_per_m,
            updated_at = excluded.updated_at",
        params![
            price.model_id.trim(),
            price.display_name.trim(),
            price.input_usd_per_m.trim(),
            price.output_usd_per_m.trim(),
            price.cache_read_usd_per_m.trim(),
            price.cache_creation_usd_per_m.trim(),
            now
        ],
    )
    .map_err(|e| format!("存价格失败：{e}"))?;
    Ok(())
}

/// 给调用方用的入口：路径解析失败也只打日志，记账不该把对话打断
#[allow(clippy::too_many_arguments)]
pub fn record_turn(
    app: &AppHandle,
    config: &AppConfig,
    scene: &str,
    conversation_id: &str,
    model: &str,
    tokens: &Tokens,
    // 这一发放上 wire 的请求体字符数。0 = 当时没量到（失败的一发），不参与校准
    sent_chars: usize,
    chain_reset: bool,
    latency_ms: u64,
    first_token_ms: Option<u64>,
    ok: bool,
    error: &str,
) {
    match app.path().app_config_dir().map_err(|e| e.to_string()) {
        Ok(config_dir) => record_turn_in(&config_dir, config, scene, conversation_id, model, tokens, sent_chars, chain_reset, latency_ms, first_token_ms, ok, error),
        Err(e) => eprintln!("用量台账路径没解析出来，这一笔没记上：{e}"),
    }
}

/// worker 进程的变体（M2/M3）：目录由 Main 经 CLI 传来
#[allow(clippy::too_many_arguments)]
pub fn record_turn_in(
    config_dir: &std::path::Path,
    config: &AppConfig,
    scene: &str,
    conversation_id: &str,
    model: &str,
    tokens: &Tokens,
    sent_chars: usize,
    chain_reset: bool,
    latency_ms: u64,
    first_token_ms: Option<u64>,
    ok: bool,
    error: &str,
) {
    let file = match db_path_in(config_dir) {
        Ok(file) => file,
        Err(e) => {
            eprintln!("用量台账路径没解析出来，这一笔没记上：{e}");
            return;
        }
    };
    record(
        &file,
        config,
        scene,
        conversation_id,
        model,
        tokens,
        sent_chars,
        chain_reset,
        latency_ms,
        first_token_ms,
        ok,
        error,
        now_ms(),
    );
}

/// 记一次模型请求。这里绝不抛错给调用方：记账失败不能把对话打断，
/// 但也不能静默——台账缺行比台账算错更难查，所以打到 stderr
#[allow(clippy::too_many_arguments)]
pub fn record(
    file: &Path,
    config: &AppConfig,
    scene: &str,
    conversation_id: &str,
    model: &str,
    tokens: &Tokens,
    // 实发的请求体字符数，与 `input_tokens` 成对，供 `fit` 用。0 = 没量到
    sent_chars: usize,
    chain_reset: bool,
    latency_ms: u64,
    first_token_ms: Option<u64>,
    ok: bool,
    error: &str,
    now: i64,
) {
    let conn = match open(file) {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("用量台账没打开，这一笔没记上：{e}");
            return;
        }
    };

    let price = match price_for(&conn, model) {
        Ok(price) => price,
        Err(e) => {
            eprintln!("查价格失败，这一笔按未计价记：{e}");
            None
        }
    };
    let cost = price.as_ref().map(|p| cost_e8(p, tokens)).unwrap_or(0);

    if let Err(e) = conn.execute(
        "INSERT INTO requests (ts, scene, conversation_id, base_url, model, api_format,
                               input_tokens, output_tokens, cached_tokens, cache_write_tokens,
                               cache_reported, chain_reset,
                               reasoning_tokens, cost_usd_e8, priced, latency_ms, first_token_ms,
                               ok, error, sent_chars)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",
        params![
            now,
            scene,
            conversation_id,
            config.base_url,
            model,
            config.api_format,
            tokens.input as i64,
            tokens.output as i64,
            tokens.cached.unwrap_or(0) as i64,
            tokens.cache_write as i64,
            tokens.cached.is_some() as i64,
            chain_reset as i64,
            tokens.reasoning as i64,
            cost,
            price.is_some() as i64,
            latency_ms as i64,
            first_token_ms.map(|v| v as i64),
            ok as i64,
            error,
            sent_chars as i64
        ],
    ) {
        eprintln!("用量台账写入失败，这一笔没记上：{e}");
    }
}

fn from_e8(value: i64) -> f64 {
    value as f64 / COST_SCALE
}

pub fn report(conn: &Connection, since_ms: Option<i64>) -> Result<Report, String> {
    let window = match since_ms {
        Some(since) => ("WHERE ts >= ?1", vec![since]),
        None => ("", Vec::new()),
    };

    let mut totals = Totals {
        requests: 0,
        failed: 0,
        input_tokens: 0,
        output_tokens: 0,
        cached_tokens: 0,
        reasoning_tokens: 0,
        cost_usd: 0.0,
        unpriced_requests: 0,
        unreported_cache_requests: 0,
    };

    {
        let sql = format!(
            "SELECT COUNT(*), SUM(ok = 0), SUM(input_tokens), SUM(output_tokens),
                    SUM(CASE WHEN cache_reported = 1 THEN cached_tokens END),
                    SUM(reasoning_tokens), SUM(cost_usd_e8),
                    SUM(priced = 0), SUM(cache_reported = 0)
             FROM requests {}",
            window.0
        );
        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        stmt.query_row(rusqlite::params_from_iter(window.1.clone()), |row| {
            let cost_e8: i64 = row.get::<_, Option<i64>>(6)?.unwrap_or(0);
            totals = Totals {
                requests: row.get::<_, Option<i64>>(0)?.unwrap_or(0),
                failed: row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                input_tokens: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                output_tokens: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                cached_tokens: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                reasoning_tokens: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
                cost_usd: from_e8(cost_e8),
                unpriced_requests: row.get::<_, Option<i64>>(7)?.unwrap_or(0),
                unreported_cache_requests: row.get::<_, Option<i64>>(8)?.unwrap_or(0),
            };
            Ok(())
        })
        .optional()
        .map_err(|e| e.to_string())?;
    }

    let mut by_model = Vec::new();
    {
        let sql = format!(
            "SELECT model, COUNT(*), SUM(input_tokens), SUM(output_tokens),
                    SUM(CASE WHEN cache_reported = 1 THEN cached_tokens END),
                    SUM(cost_usd_e8), MIN(priced)
             FROM requests {} GROUP BY model COLLATE NOCASE ORDER BY SUM(cost_usd_e8) DESC, COUNT(*) DESC",
            window.0
        );
        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(window.1.clone()), |row| {
                Ok(ModelUsage {
                    model: row.get(0)?,
                    requests: row.get(1)?,
                    input_tokens: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    output_tokens: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    cached_tokens: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    cost_usd: from_e8(row.get::<_, Option<i64>>(5)?.unwrap_or(0)),
                    priced: row.get::<_, Option<i64>>(6)?.unwrap_or(0) != 0,
                })
            })
            .map_err(|e| e.to_string())?;
        for row in rows {
            by_model.push(row.map_err(|e| e.to_string())?);
        }
    }

    let mut daily = Vec::new();
    {
        // 按本地日期分桶：用户心里的"今天"是墙上时间，不是 UTC
        let sql = format!(
            "SELECT date(ts / 1000, 'unixepoch', 'localtime'), COUNT(*), SUM(cost_usd_e8)
             FROM requests {} GROUP BY 1 ORDER BY 1 ASC",
            window.0
        );
        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(window.1.clone()), |row| {
                Ok(DayUsage {
                    date: row.get(0)?,
                    requests: row.get(1)?,
                    cost_usd: from_e8(row.get::<_, Option<i64>>(2)?.unwrap_or(0)),
                })
            })
            .map_err(|e| e.to_string())?;
        for row in rows {
            daily.push(row.map_err(|e| e.to_string())?);
        }
    }

    Ok(Report {
        totals,
        by_model,
        daily,
    })
}

/// 按话题聚合的一页。total 是"这个时间窗里有几场话题"，界面的页数与"共 N 场"都从它来
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationPage {
    pub rows: Vec<ConversationUsage>,
    pub total: i64,
}

pub fn conversations_page(
    conn: &Connection,
    since_ms: Option<i64>,
    offset: i64,
    limit: i64,
) -> Result<ConversationPage, String> {
    let window = match since_ms {
        Some(since) => ("WHERE ts >= ?1", vec![since]),
        None => ("", Vec::new()),
    };

    let total: i64 = {
        let sql = format!("SELECT COUNT(DISTINCT conversation_id) FROM requests {}", window.0);
        conn.query_row(&sql, rusqlite::params_from_iter(window.1.clone()), |row| row.get(0))
            .map_err(|e| e.to_string())?
    };

    let mut rows = Vec::new();
    if offset < total {
        let sql = format!(
            "SELECT conversation_id, COUNT(*), SUM(input_tokens), SUM(output_tokens), SUM(cost_usd_e8)
             FROM requests {} GROUP BY conversation_id
             ORDER BY SUM(cost_usd_e8) DESC, COUNT(*) DESC LIMIT {} OFFSET {}",
            window.0, limit, offset
        );
        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let mapped = stmt
            .query_map(rusqlite::params_from_iter(window.1.clone()), |row| {
                Ok(ConversationUsage {
                    conversation: row.get(0)?,
                    requests: row.get(1)?,
                    input_tokens: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    output_tokens: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    cost_usd: from_e8(row.get::<_, Option<i64>>(4)?.unwrap_or(0)),
                })
            })
            .map_err(|e| e.to_string())?;
        for row in mapped {
            rows.push(row.map_err(|e| e.to_string())?);
        }
    }

    Ok(ConversationPage { rows, total })
}

pub fn list_prices(conn: &Connection) -> Result<Vec<Price>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT model_id, display_name, input_usd_per_m, output_usd_per_m,
                    cache_read_usd_per_m, cache_creation_usd_per_m
             FROM model_pricing ORDER BY model_id COLLATE NOCASE",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |row| {
            Ok(Price {
                model_id: row.get(0)?,
                display_name: row.get(1)?,
                input_usd_per_m: row.get(2)?,
                output_usd_per_m: row.get(3)?,
                cache_read_usd_per_m: row.get(4)?,
                cache_creation_usd_per_m: row.get(5)?,
            })
        })
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| e.to_string())?);
    }
    Ok(out)
}

/// 给导入这类"要连着做几步"的调用开一条连接，省得每步都重开
pub fn with_connection<T>(
    app: &AppHandle,
    run: impl FnOnce(&Connection) -> Result<T, String>,
) -> Result<T, String> {
    let conn = open(&db_path(app)?)?;
    run(&conn)
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 单个话题的缓存命中聚合。命中率只对"这场对话"有意义——
/// 全局聚合会把其他话题的数字混进来，新话题里看着像凭空冒出 421 个 token。
/// last_* 是最近一轮的输入与缓存命中：全窗口累计会被早期短对话稀释，
/// 诊断"现在缓存有没有生效"要看最近一轮
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCacheUsage {
    /// 累计命中量只统计"服务商上报过"的那些请求，所以它是下界而不是全量
    pub input_tokens: i64,
    pub cached_tokens: i64,
    pub requests: i64,
    /// 上报了缓存字段的请求数。它小于 requests 时，上面的累计命中率不可比
    pub reported_requests: i64,
    pub last_input_tokens: i64,
    /// 最近一笔的命中量。末笔未上报时为 0 —— 必须配 last_cache_reported 一起读
    pub last_cached_tokens: i64,
    pub last_cache_reported: bool,
    /// 最近一轮的输出：它会成为下一轮的输入，所以"当前上下文占用"= last_input + last_output。
    /// 三个 last_* 一律取同一行，否则末笔未上报时会把两笔的数拼成一个上下文占用
    pub last_output_tokens: i64,
    /// 白付的 token：本该命中却没命中的那部分。命中率是个百分比，它不能告诉用户
    /// "这一轮亏了多少"；token 数与折算的钱才说得清
    pub wasted_tokens: i64,
    /// 白付量折算的美元。没有价格表的模型只计 token 不计价（不猜价）
    pub wasted_cost_usd: f64,
}

/// 命中量的噪声地板：比这小的差额基本是 tokenize 的边角，不算白付。
/// 没有这个地板，每一笔都会报出几十上百 token 的"浪费"，面板就成了噪声放大器
pub const WASTE_NOISE_FLOOR: i64 = 1024;

/// 这一笔白付了多少 token：上一笔与这一笔输入的较小者，减去本轮命中量。
///
/// 两个"没有"必须分开：`previous_input` 为 `None` 表示上一笔没有可比的（首轮，或
/// 上一笔根本没上报缓存字段），此时**不计**——把"没上报"当成"全没命中"会凭空造出浪费
pub fn wasted_tokens(previous_input: Option<i64>, input: i64, cached: Option<i64>) -> i64 {
    let (Some(previous), Some(hit)) = (previous_input, cached) else {
        return 0;
    };
    let gap = previous.min(input) - hit;
    if gap < WASTE_NOISE_FLOOR {
        0
    } else {
        gap
    }
}

/// 话题的白付量。按发生顺序逐笔比对，只用**上报了缓存字段**的那些行，
/// 且两笔都要上报才比（参照 pi 的 cache-stats 口径）
fn session_waste(conn: &Connection, conversation_id: &str) -> Result<(i64, f64), String> {
    let mut stmt = conn
        .prepare(
            "SELECT model, input_tokens, cached_tokens, cache_write_tokens, chain_reset, scene,
                    cache_reported
             FROM requests
             WHERE conversation_id = ?1 AND ok = 1
             ORDER BY ts ASC, id ASC",
        )
        .map_err(|e| e.to_string())?;
    let rows: Vec<(String, i64, i64, i64, i64, String, i64)> = stmt
        .query_map(params![conversation_id], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;

    let mut total = 0i64;
    let mut cost = 0f64;
    let mut previous: Option<i64> = None;
    for (model, input, cached, cache_write, reset, scene, reported) in rows {
        // 保温那一发**只刷新基线、不计白付**（照 pi 的 `kind === "cache_warm"` 分支）：
        // 它是我们自己决定要花的钱，按普通一笔去比就会把这笔选择报成服务商漏掉的命中
        if scene == "cache_warm" {
            previous = Some(input);
            continue;
        }
        if reported == 0 {
            // 服务商没上报命中量：既不比也不设基线（两笔都上报才比，见 §7.3）
            continue;
        }
        // 压缩边界那一笔没有"白付"：旧前缀不再被发送是那一次压缩买来的，不是改坏了。
        // 但它仍然是新链的基线，所以下一笔要跟它比
        let missed = if reset == 0 {
            wasted_tokens(previous, input, Some(cached))
        } else {
            0
        };
        previous = Some(input);
        if missed == 0 {
            continue;
        }
        total += missed;
        // 没有价格表就只报 token 数：猜一个价会把"省了多少"变成编的数字
        let Some(price) = price_for(conn, &model)? else {
            continue;
        };
        let paid = (input as f64 * parse_price(&price.input_usd_per_m)
            + cache_write as f64 * parse_price(&price.cache_creation_usd_per_m))
            / PER_MILLION;
        let denominator = (input + cache_write).max(1) as f64;
        let read_per_token = parse_price(&price.cache_read_usd_per_m) / PER_MILLION;
        cost += missed as f64 * (paid / denominator - read_per_token).max(0.0);
    }
    Ok((total, cost))
}

pub fn session_cache_usage(
    conn: &Connection,
    conversation_id: &str,
) -> Result<SessionCacheUsage, String> {
    let mut stmt = conn
        .prepare(
            "WITH last AS (
                 SELECT input_tokens, cached_tokens, output_tokens, cache_reported
                 FROM requests WHERE conversation_id = ?1
                 ORDER BY ts DESC, id DESC LIMIT 1
             )
             SELECT COALESCE(SUM(CASE WHEN cache_reported = 1 THEN input_tokens END), 0),
                    COALESCE(SUM(CASE WHEN cache_reported = 1 THEN cached_tokens END), 0),
                    COUNT(*),
                    COALESCE(SUM(cache_reported), 0),
                    COALESCE((SELECT input_tokens FROM last), 0),
                    COALESCE((SELECT cached_tokens FROM last WHERE cache_reported = 1), 0),
                    COALESCE((SELECT output_tokens FROM last), 0),
                    COALESCE((SELECT cache_reported FROM last), 0)
             FROM requests WHERE conversation_id = ?1",
        )
        .map_err(|e| e.to_string())?;
    let mut usage = stmt
        .query_row(params![conversation_id], |row| {
            Ok(SessionCacheUsage {
                input_tokens: row.get(0)?,
                cached_tokens: row.get(1)?,
                requests: row.get(2)?,
                reported_requests: row.get(3)?,
                last_input_tokens: row.get(4)?,
                last_cached_tokens: row.get(5)?,
                last_output_tokens: row.get(6)?,
                last_cache_reported: row.get::<_, i64>(7)? != 0,
                wasted_tokens: 0,
                wasted_cost_usd: 0.0,
            })
        })
        .map_err(|e| e.to_string())?;
    let (wasted_tokens, wasted_cost_usd) = session_waste(conn, conversation_id)?;
    usage.wasted_tokens = wasted_tokens;
    usage.wasted_cost_usd = wasted_cost_usd;
    Ok(usage)
}

/// 字符↔token 的实测系数。它不是全局常数：中文接近 1 字符 ≈ 1 token，英文约 4 字符 ≈ 1 token，
/// 工具声明的 JSON 又是第三种比例——所以**按模型**算，而且只有本机真的量到足够多对样本时才存在
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Calibration {
    /// 一个 token 平均多少字符。取中位数不是平均数：一次冷启动的短请求就能把平均数拉走
    pub chars_per_token: f64,
    /// 样本里最偏的那一发偏了多少个百分点——这就是设计 §0 要的那个"实测上界"
    pub max_deviation_pct: f64,
    pub samples: usize,
}

/// 少于这个数就不叫实测上界了：5 发以内的"最大值"量的就是噪声本身
pub const MIN_CALIBRATION_SAMPLES: usize = 5;

/// 从 `(实发字符数, 服务商回报的 prompt_tokens)` 配对里拟合。纯函数——SQL 只负责把样本捞上来，
/// 中位数、偏差、最少样本这几条规矩得能在没有台账的环境里被测
pub fn fit(samples: &[(usize, i64)]) -> Option<Calibration> {
    let mut ratios: Vec<f64> = samples
        .iter()
        .filter(|(chars, tokens)| *chars > 0 && *tokens > 0)
        .map(|(chars, tokens)| *chars as f64 / *tokens as f64)
        .collect();
    if ratios.len() < MIN_CALIBRATION_SAMPLES {
        return None;
    }
    ratios.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = ratios.len() / 2;
    let median = if ratios.len() % 2 == 1 {
        ratios[mid]
    } else {
        (ratios[mid - 1] + ratios[mid]) / 2.0
    };
    if !(median > 0.0) {
        return None;
    }
    let worst = ratios
        .iter()
        .map(|ratio| (ratio - median).abs() / median)
        .fold(0.0f64, f64::max);
    Some(Calibration {
        chars_per_token: median,
        max_deviation_pct: worst * 100.0,
        samples: ratios.len(),
    })
}

impl Calibration {
    /// 偏差的比例形式（`max_deviation_pct` 是百分数）
    fn spread(&self) -> f64 {
        self.max_deviation_pct / 100.0
    }

    /// 把**窗口的 token 数**折成"允许多少字符"时用这一侧：中位数**减**去实测偏差。
    /// 少算字符额度只会让让步早一点；反过来就会算出一个服务商给不出的天花板（§15）。
    /// 偏差大到九成以上时那把尺本身已经没有意义，但仍不许折成 0——那会把窗口清零，
    /// 于是每一发都触发压缩
    pub fn budget_chars_per_token(&self) -> f64 {
        self.chars_per_token * (1.0 - self.spread()).max(0.1)
    }

    /// 把**服务商真报的 token 数**折成"已经用了多少字符"时用这一侧：中位数**加**上实测偏差。
    /// 多算已用量同样是偏保守的方向：宁可早压一次，不要"以为还装着"
    pub fn estimate_chars_per_token(&self) -> f64 {
        self.chars_per_token * (1.0 + self.spread())
    }
}

/// 窗口那一侧的尺。没有实测系数（样本不够，或这个模型还没记过账）时它是 **1.0**，
/// 也就是换算这一格落地之前一直隐含的那个假设——没人会因为这一片落地而突然换个行为
pub fn budget_ratio(cal: Option<&Calibration>) -> f64 {
    cal.map(Calibration::budget_chars_per_token).unwrap_or(1.0)
}

/// 已用量那一侧的尺。与 `budget_ratio` 同源（同一份拟合的两个方向），
/// 所以不会出现"两个系数各调各的"
pub fn estimate_ratio(cal: Option<&Calibration>) -> f64 {
    cal.map(Calibration::estimate_chars_per_token).unwrap_or(1.0)
}

/// 从台账里捞这个模型的配对样本再拟合。把 SQL 单拆出来是为了让"哪些行算样本"这条规矩
/// 在没有 `AppHandle` 的地方也测得到——它筛掉的正是失败的那发和别的模型
pub fn calibration_in(conn: &Connection, model: &str) -> Option<Calibration> {
    let mut stmt = conn
        .prepare(
            "SELECT sent_chars, input_tokens FROM requests
             WHERE model = ?1 AND ok = 1 AND sent_chars > 0 AND input_tokens > 0
             ORDER BY ts DESC, id DESC LIMIT 200",
        )
        .ok()?;
    let rows: Vec<(usize, i64)> = stmt
        .query_map(params![model], |row| {
            Ok((row.get::<_, i64>(0)? as usize, row.get::<_, i64>(1)?))
        })
        .ok()?
        .flatten()
        .collect();
    fit(&rows)
}

/// 这台机器上、这个模型的实测系数。没有台账或样本太少就返回 `None`——
/// 那时该继续用字符口径并**承认它是估算**，而不是端出一个看起来精确的假数字
pub fn calibration_for(app: &AppHandle, model: &str) -> Option<Calibration> {
    calibration_for_in(&app.path().app_config_dir().map_err(|e| e.to_string()).ok()?, model)
}

/// worker 进程的变体（M2/M3）：目录由 Main 经 CLI 传来
pub fn calibration_for_in(config_dir: &std::path::Path, model: &str) -> Option<Calibration> {
    let conn = open_in(config_dir).ok()?;
    calibration_in(&conn, model)
}

/// 本话题最近一次请求的真实 prompt_tokens（含缓存命中的全部输入）。
/// auto-compact 的判定用它替代本地估算：真实值天然涵盖工具声明、消息结构等
/// 字符估算盖不到的细节。查询失败按 0 处理，调用方退回本地估算
pub fn last_prompt_tokens_for(app: &AppHandle, conversation_id: &str) -> Result<i64, String> {
    last_prompt_tokens_for_in(&app.path().app_config_dir().map_err(|e| e.to_string())?, conversation_id)
}

/// worker 进程的变体（M2/M3）
pub fn last_prompt_tokens_for_in(config_dir: &std::path::Path, conversation_id: &str) -> Result<i64, String> {
    let conn = open_in(config_dir)?;
    let mut stmt = conn
        .prepare(
            "SELECT input_tokens FROM requests
             WHERE conversation_id = ?1 ORDER BY ts DESC, id DESC LIMIT 1",
        )
        .map_err(|e| e.to_string())?;
    stmt.query_row(params![conversation_id], |row| row.get(0))
        .optional()
        .map(|value: Option<i64>| value.unwrap_or(0))
        .map_err(|e| e.to_string())
}

/// 这一段话题花了多少钱，单位 1e-8 美元。目标模式的花费熔断读它——钱只有台账这一份账，
/// 话题日志里不再存一份金额；两处各记一次，就又要人判断该信哪一处
pub fn session_cost_e8(
    app: &AppHandle,
    conversation_id: &str,
    since_ms: i64,
) -> Result<i64, String> {
    let conn = open(&db_path(app)?)?;
    let mut stmt = conn
        .prepare(
            "SELECT SUM(cost_usd_e8) FROM requests WHERE conversation_id = ?1 AND ts >= ?2",
        )
        .map_err(|e| e.to_string())?;
    stmt.query_row(params![conversation_id, since_ms], |row| {
        row.get::<_, Option<i64>>(0)
    })
    .map(|sum| sum.unwrap_or(0))
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn usage_session_cache(
    app: AppHandle,
    conversation_id: String,
) -> Result<SessionCacheUsage, String> {
    let conn = open(&db_path(&app)?)?;
    session_cache_usage(&conn, &conversation_id)
}

#[tauri::command]
pub fn usage_report(app: AppHandle, days: i64) -> Result<Report, String> {
    let conn = open(&db_path(&app)?)?;
    // days<=0 表示"全部"，不是"零天"
    let since = if days > 0 {
        Some(now_ms() - days * 86_400_000)
    } else {
        None
    };
    report(&conn, since)
}

/// 按话题聚合的分页读数。与 `usage_report` 同一个时间窗口，但按页取：
/// 全量聚合没有上限，"花钱最多的 50 场"之外的长尾也翻得到。
#[tauri::command]
pub fn usage_conversations(
    app: AppHandle,
    days: i64,
    page: i64,
    page_size: i64,
) -> Result<ConversationPage, String> {
    let conn = open(&db_path(&app)?)?;
    let since = if days > 0 {
        Some(now_ms() - days * 86_400_000)
    } else {
        None
    };
    let offset = page.max(0) * page_size.clamp(1, 100);
    conversations_page(&conn, since, offset, page_size.clamp(1, 100))
}

#[tauri::command]
pub fn pricing_list(app: AppHandle) -> Result<Vec<Price>, String> {
    let conn = open(&db_path(&app)?)?;
    list_prices(&conn)
}

/// 一行价目合不合规矩。**判据必须比读侧宽松不得**：`parse_price` 会把读不懂的、
/// 负的、`NaN`/`inf` 一律钳成 0（见 `unparseable_or_negative_prices_count_as_free_not_as_a_credit`），
/// 所以这些值一旦写进表，面板上显示的是用户输入的那个数、算钱时用的是 0——
/// 同一个价住在两处，而只有一个是真的
pub(crate) fn check_price(price: &Price) -> Result<(), String> {
    if price.model_id.trim().is_empty() {
        return Err("模型名不能为空。".into());
    }
    for (label, raw) in [
        ("输入", &price.input_usd_per_m),
        ("输出", &price.output_usd_per_m),
        ("缓存读", &price.cache_read_usd_per_m),
        ("缓存写", &price.cache_creation_usd_per_m),
    ] {
        match raw.trim().parse::<f64>() {
            Ok(value) if value.is_finite() && value >= 0.0 => {}
            Ok(_) => return Err(format!("{label}单价不能是负数或无穷（每百万 token 多少美元）。")),
            Err(_) => return Err(format!("{label}单价得是数字（每百万 token 多少美元）。")),
        }
    }
    Ok(())
}

#[tauri::command]
pub fn pricing_upsert(app: AppHandle, price: Price) -> Result<Vec<Price>, String> {
    check_price(&price)?;

    let conn = open(&db_path(&app)?)?;
    upsert_price(&conn, &price, now_ms())?;
    list_prices(&conn)
}

#[tauri::command]
pub fn pricing_remove(app: AppHandle, model_id: String) -> Result<Vec<Price>, String> {
    let conn = open(&db_path(&app)?)?;
    conn.execute(
        "DELETE FROM model_pricing WHERE model_id = ?1",
        params![model_id],
    )
    .map_err(|e| format!("删价格失败：{e}"))?;
    list_prices(&conn)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRequestRow {
    pub id: i64,
    pub ts: i64,
    pub model: String,
    pub base_url: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: i64,
    /// false 表示服务商没回缓存字段——上面的 cached_tokens 那时不代表「命中 0」
    pub cache_reported: bool,
    pub reasoning_tokens: i64,
    pub cost_usd: f64,
    pub priced: bool,
    pub latency_ms: i64,
    pub first_token_ms: Option<i64>,
    pub ok: bool,
    pub error: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRequestPage {
    pub rows: Vec<UsageRequestRow>,
    /// 同窗口下的总行数，分页控件用
    pub total: i64,
}

/// 最近请求分页。窗口语义与 usage_report 完全一致：days<=0 = 全部。
/// limit 由前端固定传 20，但这里仍按 1..=200 收口——边界是防御性的，
/// 不能指望每个调用方都守约。
pub fn recent(
    conn: &Connection,
    since_ms: Option<i64>,
    offset: i64,
    limit: i64,
) -> Result<UsageRequestPage, String> {
    let limit = limit.clamp(1, 200);
    let offset = offset.max(0);
    let window = match since_ms {
        Some(since) => ("WHERE ts >= ?1", vec![since]),
        None => ("", Vec::new()),
    };

    let total: i64;
    {
        let sql = format!("SELECT COUNT(*) FROM requests {}", window.0);
        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        total = stmt
            .query_row(rusqlite::params_from_iter(window.1.iter()), |row| {
                row.get(0)
            })
            .map_err(|e| e.to_string())?;
    }

    let sql = format!(
        "SELECT id, ts, model, base_url, input_tokens, output_tokens, cached_tokens,
                reasoning_tokens, cost_usd_e8, priced, latency_ms, first_token_ms, ok, error,
                cache_reported
         FROM requests {} ORDER BY ts DESC, id DESC LIMIT {limit} OFFSET {offset}",
        window.0
    );
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(window.1.iter()), |row| {
            Ok(UsageRequestRow {
                id: row.get(0)?,
                ts: row.get(1)?,
                model: row.get(2)?,
                base_url: row.get(3)?,
                input_tokens: row.get(4)?,
                output_tokens: row.get(5)?,
                cached_tokens: row.get(6)?,
                reasoning_tokens: row.get(7)?,
                cost_usd: from_e8(row.get::<_, Option<i64>>(8)?.unwrap_or(0)),
                priced: row.get::<_, i64>(9)? != 0,
                latency_ms: row.get(10)?,
                first_token_ms: row.get(11)?,
                ok: row.get::<_, i64>(12)? != 0,
                error: row.get(13)?,
                cache_reported: row.get::<_, i64>(14)? != 0,
            })
        })
        .map_err(|e| e.to_string())?;

    let mut list = Vec::new();
    for row in rows {
        list.push(row.map_err(|e| e.to_string())?);
    }
    Ok(UsageRequestPage { rows: list, total })
}

/// CSV 落盘。BOM 是给 Excel 的：没有它，用中文列名的 CSV 打开就是乱码。
pub fn export_csv(path: &str, content: &str) -> Result<(), String> {
    std::fs::write(path, format!("\u{FEFF}{content}")).map_err(|e| format!("写 CSV 失败：{e}"))
}

#[tauri::command]
pub fn usage_recent(
    app: AppHandle,
    days: i64,
    offset: i64,
    limit: i64,
) -> Result<UsageRequestPage, String> {
    let conn = open(&db_path(&app)?)?;
    let since = if days > 0 {
        Some(now_ms() - days * 86_400_000)
    } else {
        None
    };
    recent(&conn, since, offset, limit)
}

#[tauri::command]
pub fn usage_export_csv(path: String, content: String) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err("保存路径是空的。".into());
    }
    export_csv(&path, &content)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn priced(model: &str, input: &str, output: &str, cache_read: &str) -> Price {
        Price {
            model_id: model.into(),
            display_name: model.into(),
            input_usd_per_m: input.into(),
            output_usd_per_m: output.into(),
            cache_read_usd_per_m: cache_read.into(),
            cache_creation_usd_per_m: "0".into(),
        }
    }

    fn ledger(label: &str) -> (PathBuf, Connection) {
        let file = crate::test_support::temp_dir(label).join("usage.db");
        let conn = open(&file).expect("建台账");
        (file, conn)
    }

    /// 白付量：首轮没有对照、未上报不当成"全没命中"、地板以下的算噪声
    #[test]
    fn wasted_tokens_ignores_the_first_row_unreported_rows_and_noise() {
        assert_eq!(wasted_tokens(None, 9_000, Some(0)), 0, "首轮没有可比对象");
        assert_eq!(
            wasted_tokens(Some(9_000), 9_500, None),
            0,
            "这轮没上报命中量就不计"
        );
        assert_eq!(
            wasted_tokens(Some(9_000), 9_500, Some(8_600)),
            0,
            "差 400 token 属于 tokenize 的边角，不是白付"
        );
        assert_eq!(wasted_tokens(Some(9_000), 9_500, Some(1_000)), 8_000);
        assert_eq!(
            wasted_tokens(Some(20_000), 9_500, Some(1_000)),
            8_500,
            "上一笔更长时按本轮输入算，否则会把上一笔的长度算成这一笔的浪费"
        );
    }

    fn put_request(
        conn: &Connection,
        conversation: &str,
        model: &str,
        at: i64,
        input: i64,
        cached: i64,
        reported: bool,
    ) {
        conn.execute(
            "INSERT INTO requests (ts, scene, conversation_id, model, input_tokens, cached_tokens, cache_reported, ok)
             VALUES (?1, 'chat', ?2, ?3, ?4, ?5, ?6, 1)",
            params![at, conversation, model, input, cached, reported as i64],
        )
        .expect("插一笔请求");
    }

    /// 白付量必须只算这一条话题。这里曾经把 SQL 参数写成空数组——那样会把所有话题
    /// 的行混在一起，报出一个看起来很像样的假数字
    #[test]
    fn the_waste_count_is_scoped_to_one_conversation() {
        let dir = crate::test_support::scoped_temp_dir("waste-scope");
        let conn = open(&dir.join("usage.db")).expect("建台账");
        put_request(&conn, "conv_a", "m-one", 10, 9_000, 9_000, true);
        put_request(&conn, "conv_a", "m-one", 20, 9_500, 1_000, true);
        // 另一条话题故意放一笔大得多的白付：混算的话 conv_a 的数字会被它污染
        put_request(&conn, "conv_b", "m-one", 30, 90_000, 90_000, true);
        put_request(&conn, "conv_b", "m-one", 40, 91_000, 1_000, true);

        let (tokens, _) = session_waste(&conn, "conv_a").expect("算白付该成功");
        assert_eq!(tokens, 8_000, "只该看见 conv_a 自己那笔：{tokens}");
        let (other, _) = session_waste(&conn, "conv_b").expect("算另一条该成功");
        assert_eq!(
            other, 89_000,
            "conv_b 按它自己的上一笔算：min(90000,91000)-1000"
        );
    }

    /// 上报与否是白付量能否成立的前提：混进未上报的行就会凭空造出浪费
    #[test]
    fn rows_without_a_cache_report_take_no_part_in_the_waste() {
        let dir = crate::test_support::scoped_temp_dir("waste-unreported");
        let conn = open(&dir.join("usage.db")).expect("建台账");
        put_request(&conn, "conv_a", "m-one", 10, 9_000, 0, false);
        put_request(&conn, "conv_a", "m-one", 20, 9_500, 0, false);
        let (tokens, cost) = session_waste(&conn, "conv_a").expect("算白付该成功");
        assert_eq!(tokens, 0, "服务商没上报命中量时无从判定，不能算成白付");
        assert_eq!(cost, 0.0);
    }

    /// 带"链断开"标记的一笔：压缩边界之后的第一发
    fn put_reset_request(
        conn: &Connection,
        conversation: &str,
        model: &str,
        at: i64,
        input: i64,
        cached: i64,
    ) {
        conn.execute(
            "INSERT INTO requests (ts, scene, conversation_id, model, input_tokens, cached_tokens, cache_reported, chain_reset, ok)
             VALUES (?1, 'chat', ?2, ?3, ?4, ?5, 1, 1, 1)",
            params![at, conversation, model, input, cached],
        )
        .expect("插一笔断开基线的请求");
    }

    /// 压缩换来的那次低命中不是白付：那一笔豁免，但它仍然是下一笔的基线
    #[test]
    fn a_chain_reset_row_is_exempt_but_still_sets_the_baseline() {
        let dir = crate::test_support::scoped_temp_dir("waste-reset");
        let conn = open(&dir.join("usage.db")).expect("建台账");
        put_request(&conn, "conv_a", "m-one", 10, 30_000, 30_000, true);
        // 压缩后整段前缀换成摘要：输入变短、命中 0。这是那一次压缩买来的代价，不是漏命中
        put_reset_request(&conn, "conv_a", "m-one", 20, 12_000, 0);
        let (after_reset, _) = session_waste(&conn, "conv_a").expect("算白付该成功");
        assert_eq!(after_reset, 0, "压缩后第一笔的 0 命中不该记成浪费");

        // 再下一笔本该接着这条新链命中却没命中，这才是要报的白付——基线是压缩后那 12k
        put_request(&conn, "conv_a", "m-one", 30, 13_000, 1_000, true);
        let (tokens, _) = session_waste(&conn, "conv_a").expect("算白付该成功");
        assert_eq!(
            tokens, 11_000,
            "基线要取压缩后那一笔而不是压缩前的 30k：{tokens}"
        );
    }

    /// 保温那一发只刷新基线、不计白付（照 pi 的 `kind === "cache_warm"` 分支）：
    /// 那是我们自己决定要花的钱，按普通一笔去比就会报成"服务商漏了命中"
    #[test]
    fn a_cache_warm_row_refreshes_the_baseline_without_being_counted() {
        let dir = crate::test_support::scoped_temp_dir("waste-warm");
        let conn = open(&dir.join("usage.db")).expect("建台账");
        put_request(&conn, "conv_a", "m-one", 10, 30_000, 30_000, true);
        conn.execute(
            "INSERT INTO requests (ts, scene, conversation_id, model, input_tokens, cached_tokens, cache_reported, ok)
             VALUES (20, 'cache_warm', 'conv_a', 'm-one', 30_000, 0, 1, 1)",
            [],
        )
        .expect("插一笔保温");
        put_request(&conn, "conv_a", "m-one", 30, 31_000, 31_000, true);

        let (tokens, _) = session_waste(&conn, "conv_a").expect("算白付该成功");
        assert_eq!(
            tokens, 0,
            "保温那一发若被当成普通一笔，会凭空报出 30000 的白付：{tokens}"
        );
    }

    /// 写入路径以前没有测试：列与占位符错一位，数字会静静串到别的列上，面板照样绿灯。
    /// 这里从 record() 一路走到白付量，两头都钉住
    #[test]
    fn record_lands_every_column_where_it_belongs() {
        let dir = crate::test_support::scoped_temp_dir("record-roundtrip");
        let file = dir.join("usage.db");
        let config = AppConfig {
            model: "m-one".into(),
            ..Default::default()
        };
        record(
            &file,
            &config,
            "chat",
            "conv_a",
            "m-one",
            &Tokens {
                input: 30_000,
                output: 12,
                cached: Some(30_000),
                cache_write: 0,
                reasoning: 3,
            },
            0,
            false,
            1_234,
            Some(56),
            true,
            "",
            10,
        );
        record(
            &file,
            &config,
            "chat",
            "conv_a",
            "m-one",
            &Tokens {
                input: 9_500,
                output: 7,
                cached: Some(1_000),
                cache_write: 0,
                reasoning: 0,
            },
            0,
            true,
            2_345,
            None,
            true,
            "",
            20,
        );

        let conn = open(&file).expect("开台账");
        let row: (String, i64, i64, i64, i64, Option<i64>, i64) = conn
            .query_row(
                "SELECT model, input_tokens, cached_tokens, latency_ms, chain_reset,
                        first_token_ms, reasoning_tokens
                 FROM requests ORDER BY ts ASC LIMIT 1",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .expect("读回第一笔");
        assert_eq!(row.0, "m-one", "模型名串了位");
        assert_eq!((row.1, row.2), (30_000, 30_000), "输入与命中量串了位");
        assert_eq!(row.3, 1_234, "延迟串了位");
        assert_eq!(row.4, 0, "没要求断开的一笔试着被写成断开");
        assert_eq!(row.5, Some(56), "首字延迟串了位");
        assert_eq!(row.6, 3, "思维链 token 串了位");

        // 第二笔带着 chain_reset=1，所以整条链的白付量仍是 0：写入与读取两头都通
        let (tokens, _) = session_waste(&conn, "conv_a").expect("算白付该成功");
        assert_eq!(
            tokens, 0,
            "第二笔若没读到断开标记，min(30000,9500)-1000 会报出 8500"
        );
    }

    /// 有价格表才折算成钱；没有就只报 token 数，不猜价
    #[test]
    fn the_waste_is_priced_only_when_a_price_row_exists() {
        let dir = crate::test_support::scoped_temp_dir("waste-price");
        let conn = open(&dir.join("usage.db")).expect("建台账");
        put_request(&conn, "conv_a", "priced-model", 10, 9_000, 9_000, true);
        put_request(&conn, "conv_a", "priced-model", 20, 9_500, 1_000, true);
        let (tokens, unpriced) = session_waste(&conn, "conv_a").expect("算白付该成功");
        assert_eq!(tokens, 8_000);
        assert_eq!(unpriced, 0.0, "没有价格表就不能编一个美元数出来");

        conn.execute(
            "INSERT INTO model_pricing (model_id, display_name, input_usd_per_m, output_usd_per_m,
                                        cache_read_usd_per_m, cache_creation_usd_per_m)
             VALUES ('priced-model', 'Priced', '10', '50', '1', '12.5')",
            [],
        )
        .expect("放一条价格");
        let (_, priced) = session_waste(&conn, "conv_a").expect("算白付该成功");
        // 全价均摊 10.375/百万，命中价 1/百万 → 8000 token 白付约 $0.000075
        // 全价均摊 (9500×10)/1e6 ÷ 9500 = 10/百万，命中价 1/百万 → 每 token 差 9e-6
        // 8000 token 白付 = $0.072
        assert!(
            (priced - 0.072).abs() < 1e-9,
            "折算的数不对，期望 $0.072，得到 {priced}"
        );
    }

    #[test]
    fn cached_tokens_are_billed_at_the_cache_rate_not_twice() {
        // 13120 输入里有 8192 命中缓存：全价只算 4928，命中部分按 1/百万
        let price = priced("glm-5.3", "10", "50", "1");
        let tokens = Tokens {
            input: 13120,
            output: 10,
            cached: Some(8192),
            ..Default::default()
        };

        // 4928*10 + 8192*1 + 10*50 = 57972 美元·token/百万 → $0.057972
        assert_eq!(cost_e8(&price, &tokens), 5_797_200);
    }

    /// 服务商没回缓存字段时按全价输入算：宁可多报成本，不打不存在的折扣
    #[test]
    fn an_unreported_cache_count_bills_the_full_input_instead_of_guessing_a_discount() {
        let price = priced("glm-5.3", "10", "50", "1");
        let tokens = Tokens {
            input: 13120,
            output: 10,
            cached: None,
            ..Default::default()
        };
        // 13120*10 + 10*50 = 131700 → $0.1317（比命中那笔贵，正是"没数据不打折"）
        assert_eq!(cost_e8(&price, &tokens), 13_170_000);
    }

    /// 服务商把 cached 报得比 input 还大时不能算成负数：那会把整张账单压低
    /// 老库（没有 cache_reported 列）升上来之后：`cached_tokens > 0` 的行必须认作"上报过"。
    /// 这一条是真实数据逼出来的——本机台账有 16 笔带命中量的老行，
    /// 不回填就会被新界面读成"服务商从没上报过"，把已有的命中历史抹成未知
    #[test]
    fn a_legacy_ledger_keeps_its_known_cache_hits_after_the_column_is_added() {
        let file = crate::test_support::temp_dir("legacy-cache").join("usage.db");
        {
            let conn = Connection::open(&file).expect("建老库");
            // D7 之前的表结构：逐列照抄，唯独没有 cache_reported
            conn.execute_batch(
                "CREATE TABLE requests (
                     id INTEGER PRIMARY KEY, ts INTEGER NOT NULL,
                     scene TEXT NOT NULL DEFAULT 'chat',
                     conversation_id TEXT NOT NULL DEFAULT '',
                     base_url TEXT NOT NULL DEFAULT '',
                     model TEXT NOT NULL,
                     api_format TEXT NOT NULL DEFAULT 'chat',
                     input_tokens INTEGER NOT NULL DEFAULT 0,
                     output_tokens INTEGER NOT NULL DEFAULT 0,
                     cached_tokens INTEGER NOT NULL DEFAULT 0,
                     cache_write_tokens INTEGER NOT NULL DEFAULT 0,
                     reasoning_tokens INTEGER NOT NULL DEFAULT 0,
                     cost_usd_e8 INTEGER NOT NULL DEFAULT 0,
                     priced INTEGER NOT NULL DEFAULT 0,
                     latency_ms INTEGER NOT NULL DEFAULT 0,
                     first_token_ms INTEGER,
                     ok INTEGER NOT NULL DEFAULT 1,
                     error TEXT NOT NULL DEFAULT ''
                 )",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO requests (ts, model, cached_tokens) VALUES (1,'m',128)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO requests (ts, model, cached_tokens) VALUES (2,'m',0)",
                [],
            )
            .unwrap();
        }

        let read = |conn: &Connection| -> Vec<(i64, i64)> {
            let mut stmt = conn
                .prepare("SELECT cached_tokens, cache_reported FROM requests ORDER BY ts")
                .unwrap();
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };

        let conn = open(&file).expect("老库要能升到新结构");
        assert_eq!(read(&conn), vec![(128, 1), (0, 0)]);
        drop(conn);

        // 二次打开走"列已存在"分支：不得报错，也不得把 cached=0 那笔翻成已上报
        let conn = open(&file).expect("重复迁移必须幂等");
        assert_eq!(read(&conn), vec![(128, 1), (0, 0)]);
    }

    /// 本机真实踩到的那种状态：列是被更早一次启动补上的、但那次还没有回填逻辑，
    /// 于是有一批 cached_tokens>0 的老行标着"未上报"。打开时要把它们修回来
    #[test]
    fn rows_left_behind_by_an_earlier_column_add_are_healed_on_open() {
        let file = crate::test_support::temp_dir("heal-cache").join("usage.db");
        let conn = open(&file).expect("按新结构建库");
        // 模拟"补了列但没回填"的历史遗留：直接写一行 cached>0 且 reported=0
        conn.execute(
            "INSERT INTO requests (ts, model, input_tokens, cached_tokens, cache_reported)
             VALUES (1,'m',500,128,0)",
            [],
        )
        .unwrap();
        assert_eq!(
            conn.query_row::<i64, _, _>(
                "SELECT cache_reported FROM requests WHERE ts = 1",
                [],
                |row| row.get(0)
            )
            .unwrap(),
            0,
            "写进去时必须真的是未回填态，否则这条测试什么都没测"
        );
        drop(conn);

        let conn = open(&file).expect("重开");
        assert_eq!(
            conn.query_row::<i64, _, _>(
                "SELECT cache_reported FROM requests WHERE ts = 1",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            1,
            "有命中量的行不该继续被读成未上报"
        );
    }

    /// 老台账没这一列：开库时必须补上，否则此后每一笔记账都在静默丢
    #[test]
    fn a_ledger_from_before_the_measured_size_column_gets_it_on_open() {
        let file = crate::test_support::temp_dir("heal-sent-chars").join("usage.db");
        {
            // 手工按"当年那一版"建一张没有 sent_chars 的 requests。
            // 不走 open()，否则建出来的就是新库，那条 ALTER 什么都没测
            let conn = Connection::open(&file).expect("建老台账");
            conn.execute(
                "CREATE TABLE requests (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    ts INTEGER NOT NULL,
                    scene TEXT NOT NULL DEFAULT '',
                    conversation_id TEXT NOT NULL DEFAULT '',
                    base_url TEXT NOT NULL DEFAULT '',
                    model TEXT NOT NULL DEFAULT '',
                    api_format TEXT NOT NULL DEFAULT '',
                    input_tokens INTEGER NOT NULL DEFAULT 0,
                    output_tokens INTEGER NOT NULL DEFAULT 0,
                    cached_tokens INTEGER NOT NULL DEFAULT 0,
                    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
                    cache_reported INTEGER NOT NULL DEFAULT 0,
                    chain_reset INTEGER NOT NULL DEFAULT 0,
                    reasoning_tokens INTEGER NOT NULL DEFAULT 0,
                    cost_usd_e8 INTEGER NOT NULL DEFAULT 0,
                    priced INTEGER NOT NULL DEFAULT 0,
                    latency_ms INTEGER NOT NULL DEFAULT 0,
                    first_token_ms INTEGER,
                    ok INTEGER NOT NULL DEFAULT 1,
                    error TEXT NOT NULL DEFAULT ''
                 )",
                [],
            )
            .expect("建老表");
            conn.execute(
                "INSERT INTO requests (ts, model, input_tokens, cached_tokens) VALUES (1,'m',500,128)",
                [],
            )
            .expect("写一行老账");
        }

        let conn = open(&file).expect("老台账要开得了");
        assert_eq!(
            conn.query_row::<i64, _, _>(
                "SELECT sent_chars FROM requests WHERE ts = 1",
                [],
                |row| row.get(0)
            )
            .expect("这一列得被补出来"),
            0,
            "那一发当时根本没量，读成 0 而不是把整库判死"
        );
    }

    /// 拟合的是中位数：一次冷启动的短请求能把平均数拉到别处，拉不走中位数
    #[test]
    fn one_short_request_cannot_drag_the_coefficient() {
        let samples = [
            (4_000usize, 1_000i64),
            (4_000, 1_000),
            (4_000, 1_000),
            (4_000, 1_000),
            (40_000, 1_000),
        ];
        let fitted = fit(&samples).expect("5 发够拟合");
        assert_eq!(fitted.samples, 5);
        assert!(
            (fitted.chars_per_token - 4.0).abs() < 0.001,
            "系数该停在样本中间（平均数会被拉到 11.2），拿到的是 {}",
            fitted.chars_per_token
        );
    }

    /// 报出去的上界是最偏那一发，不是平均偏多少：验收要的是"不会超过"
    #[test]
    fn the_bound_reported_is_the_worst_pair() {
        let samples = [
            (4_000usize, 1_000i64),
            (4_000, 1_000),
            (4_000, 1_000),
            (4_000, 1_000),
            (6_000, 1_000),
        ];
        let fitted = fit(&samples).expect("5 发够拟合");
        assert!(
            (fitted.max_deviation_pct - 50.0).abs() < 0.001,
            "偏 50% 的那一发就是上界，拿到的是 {}",
            fitted.max_deviation_pct
        );
    }

    /// 5 发以下没有"上界"可言：那量的就是噪声本身，所以宁可说"还没量到"
    #[test]
    fn fewer_than_five_pairs_is_not_a_bound() {
        assert!(fit(&[(4_000usize, 1_000i64); MIN_CALIBRATION_SAMPLES - 1]).is_none());
        assert!(fit(&[(4_000usize, 1_000i64); MIN_CALIBRATION_SAMPLES]).is_some());
    }

    /// 没量到的行不许冒充样本：断掉的那发 sent_chars=0，服务商没报量的行 input_tokens=0
    #[test]
    fn unmeasured_rows_do_not_join_the_sample() {
        let samples = [
            (0usize, 900i64),
            (3_600, 0),
            (3_600, 900),
            (3_600, 900),
            (3_600, 900),
            (3_600, 900),
            (3_600, 900),
        ];
        let fitted = fit(&samples).expect("有效样本够 5 发");
        assert_eq!(fitted.samples, 5, "两行没量到的不该算进样本");
    }

    /// 捞样本那条 SQL 也得有人测：按模型分开、只认成功且量到大小的那些行
    #[test]
    fn calibration_reads_only_this_models_measured_pairs() {
        let (file, conn) = ledger("calibration");
        let config = AppConfig::default();
        drop(conn);
        for _ in 0..MIN_CALIBRATION_SAMPLES {
            record(
                &file,
                &config,
                "chat",
                "",
                "m-a",
                &Tokens {
                    input: 1_000,
                    ..Default::default()
                },
                4_000,
                false,
                1,
                None,
                true,
                "",
                10,
            );
        }
        // 别的模型：一发，而且比例完全不同
        record(
            &file,
            &config,
            "chat",
            "",
            "m-b",
            &Tokens {
                input: 1_000,
                ..Default::default()
            },
            9_000,
            false,
            1,
            None,
            true,
            "",
            11,
        );
        // 断掉的那一发：ok=0 且没量到
        record(
            &file,
            &config,
            "chat",
            "",
            "m-a",
            &Tokens::default(),
            0,
            false,
            1,
            None,
            false,
            "服务商限流",
            12,
        );

        let conn = open(&file).expect("重开台账");
        let fitted = calibration_in(&conn, "m-a").expect("5 发实测样本");
        assert_eq!(fitted.samples, 5, "失败那发和别的模型都不算这个模型的样本");
        assert!(
            (fitted.chars_per_token - 4.0).abs() < 0.001,
            "别把 m-b 那发 9 字符/token 的混进来"
        );
        assert!(calibration_in(&conn, "m-b").is_none(), "1 发不成上界");
        assert!(calibration_in(&conn, "没跑过的模型").is_none());
    }

    /// §15 的那两把尺：同一份拟合，按方向各取一侧，而且两侧都偏保守。
    /// 它们必须同源——两个系数各调各的，早晚会有一个被单独"调准"
    #[test]
    fn the_two_rulers_come_from_one_fit_and_both_are_conservative() {
        let cal = Calibration {
            chars_per_token: 4.0,
            max_deviation_pct: 25.0,
            samples: 9,
        };
        assert_eq!(cal.budget_chars_per_token(), 3.0, "窗口那一侧取下界：宁可少给字符额度");
        assert_eq!(cal.estimate_chars_per_token(), 5.0, "已用量那一侧取上界：宁可多算已用");
        assert!(
            cal.budget_chars_per_token() < cal.estimate_chars_per_token(),
            "两侧同向才谈得上保守"
        );

        // 没有实测系数时它就是换算落地前那个隐含假设：1 字符 = 1 token
        assert_eq!(budget_ratio(None), 1.0);
        assert_eq!(estimate_ratio(None), 1.0);

        let noisy = Calibration {
            chars_per_token: 4.0,
            max_deviation_pct: 300.0,
            samples: 5,
        };
        assert!(
            (noisy.budget_chars_per_token() - 0.4).abs() < 1e-9,
            "偏差超过百分之百时那把尺要停在地板上，而不是折成 0（那等于把窗口清零）：{}",
            noisy.budget_chars_per_token()
        );
    }

    #[test]
    fn a_broken_cache_count_cannot_push_the_bill_negative() {
        let price = priced("m", "10", "10", "1");
        let tokens = Tokens {
            input: 100,
            output: 0,
            cached: Some(500),
            ..Default::default()
        };
        // 全价输入被减成 0，只剩 500 个缓存读 token 按 1/百万 计 → $0.0005
        assert_eq!(cost_e8(&price, &tokens), 50_000);
    }

    #[test]
    fn unparseable_or_negative_prices_count_as_free_not_as_a_credit() {
        let price = priced("m", "-3", "abc", "0");
        let tokens = Tokens {
            input: 1_000_000,
            output: 1_000_000,
            ..Default::default()
        };
        assert_eq!(cost_e8(&price, &tokens), 0);
    }

    /// 读侧那三个钳位（读不懂、负的、`NaN`/`inf` 一律当 0）意味着写侧宽松不得：
    /// 这种值一旦进表，面板上显示的是用户输入的那个数、算钱时用的是 0，
    /// 而"未定价"和"免费"是两件事（见 `an_unpriced_model_is_counted_separately_from_a_free_one`）
    #[test]
    fn a_price_the_scorer_would_have_to_clamp_is_refused_on_the_way_in() {
        for bad in ["-3", "abc", "", "   ", "NaN", "inf", "infinity", "1e400"] {
            let price = Price {
                input_usd_per_m: bad.into(),
                ..priced("m", "1", "1", "1")
            };
            let err = check_price(&price).err().unwrap_or_else(|| panic!("{bad} 不该被当成一个单价"));
            assert!(err.contains("输入"), "要报出是哪一格：{err}");
        }
        // 四格各自点名，不能都算成"输入"
        for (label, price) in [
            (
                "输出",
                Price { output_usd_per_m: "-1".into(), ..priced("m", "1", "1", "1") },
            ),
            (
                "缓存读",
                Price { cache_read_usd_per_m: "x".into(), ..priced("m", "1", "1", "1") },
            ),
            (
                "缓存写",
                Price { cache_creation_usd_per_m: "NaN".into(), ..priced("m", "1", "1", "1") },
            ),
        ] {
            let err = check_price(&price).err().unwrap_or_else(|| panic!("{label}那一格也该守"));
            assert!(err.contains(label), "报的得是那一格，不是别的：{err}");
        }
        // 正对照：0、正常小数、两边留空格都是能写的价
        for good in ["0", "0.000001", " 1.5 ", "12"] {
            let price = Price { input_usd_per_m: good.into(), ..priced("m", "1", "1", "1") };
            assert!(check_price(&price).is_ok(), "{good} 是一个合法的价");
        }
        // 模型名那格：空着或全是空格都不许进表
        for blank in ["", "   "] {
            let price = Price { model_id: blank.into(), ..priced("m", "1", "1", "1") };
            assert!(check_price(&price).unwrap_err().contains("模型名"), "模型名要单独一句");
        }
    }

    /// `Price` 的容器带 `default`，所以打错的键会被静默丢掉、那一格换成默认值。
    /// 前端现在发的是 `ModelPrice` 那六个键（多一个都没有），这条守的是以后
    #[test]
    fn a_price_row_naming_a_field_that_does_not_exist_is_refused() {
        let typo = r#"{"modelId":"m","displayNmae":"GLM","inputUsdPerM":"1",
            "outputUsdPerM":"2","cacheReadUsdPerM":"0","cacheCreationUsdPerM":"0"}"#;
        let err = serde_json::from_str::<Price>(typo)
            .expect_err("差一个字母的键名不能算写了显示名")
            .to_string();
        assert!(err.contains("displayNmae"), "要报出认错的那个键：{err}");

        // 正对照：面板真发的那一份要读得回来，并且六格都对得上
        let row: Price = serde_json::from_str(
            r#"{"modelId":"m","displayName":"GLM","inputUsdPerM":"1",
               "outputUsdPerM":"2","cacheReadUsdPerM":"0","cacheCreationUsdPerM":"0"}"#,
        )
        .expect("面板那份六键要能读回来");
        assert_eq!(row, Price { display_name: "GLM".into(), ..priced("m", "1", "2", "0") });
    }

    /// 判据写在纯函数里成立，**不等于命令真的去问它**——把 `pricing_upsert` 里那一行删掉，
    /// 上面两条测试照样绿（它们直接调 `check_price`），编译器也不报死码（测试就是它的调用方）。
    /// 所以这一条钉的是调用点在不在，两份判据不许长成第二份。
    /// 它量的是**存在**，不是活着：后者要在跑起来的应用里点那一次
    #[test]
    fn both_writers_of_a_price_row_ask_the_one_gate() {
        let production = include_str!("usage.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap_or_default();
        let command = production
            .split("pub fn pricing_upsert")
            .nth(1)
            .expect("命令得在")
            .split("\n}")
            .next()
            .unwrap_or_default();
        assert!(
            command.contains("check_price(&price)?"),
            "面板那条命令没去问判据：{command}"
        );
        assert_eq!(production.matches("fn check_price(").count(), 1, "长第二份判据就等于有两份答案");
        assert_eq!(
            production.matches("单价得是数字").count(),
            1,
            "比较不许在命令里再抄一份"
        );

        // 导入那条路写的是同一张表，过的是同一道闸（cc-switch 那张表是别人写的）
        assert!(
            include_str!("ccswitch.rs").contains("usage::check_price(price)"),
            "导入绕过了判据：一条 -3 会把那个模型说成免费"
        );
    }

    #[test]
    fn a_round_records_tokens_cost_and_the_model_name() {
        let (file, conn) = ledger("record");
        upsert_price(&conn, &priced("glm-5.3", "10", "50", "1"), 1).unwrap();

        let config = AppConfig {
            base_url: "https://relay.example/v1".into(),
            model: "glm-5.3".into(),
            api_format: "responses".into(),
            ..Default::default()
        };
        drop(conn);

        record(
            &file,
            &config,
            "chat",
            "conv-1",
            "glm-5.3",
            &Tokens {
                input: 13120,
                output: 10,
                cached: Some(8192),
                ..Default::default()
            },
            0,
            false,
            1234,
            Some(400),
            true,
            "",
            1_700_000_000_000,
        );

        let conn = open(&file).unwrap();
        let report = report(&conn, None).unwrap();
        assert_eq!(report.totals.requests, 1);
        assert_eq!(report.totals.input_tokens, 13120);
        assert_eq!(report.totals.cached_tokens, 8192);
        assert_eq!(report.totals.unpriced_requests, 0);
        assert!((report.totals.cost_usd - 0.057972).abs() < 1e-12);
        assert_eq!(report.by_model[0].model, "glm-5.3");
        assert!(report.by_model[0].priced);
        crate::test_support::remove_tree(&file.parent().unwrap());
    }

    /// 没匹配到价格的请求按 0 记账可以，但必须能被数出来——
    /// 否则"这个月只花了 3 块"其实只是模型名没对上
    #[test]
    fn an_unpriced_model_is_counted_separately_from_a_free_one() {
        let (file, conn) = ledger("unpriced");
        let config = AppConfig::default();
        drop(conn);

        record(
            &file,
            &config,
            "chat",
            "",
            "某中转站私有模型",
            &Tokens {
                input: 100,
                output: 100,
                ..Default::default()
            },
            0,
            false,
            10,
            None,
            true,
            "",
            1_700_000_000_000,
        );

        let conn = open(&file).unwrap();
        let report = report(&conn, None).unwrap();
        assert_eq!(report.totals.cost_usd, 0.0);
        assert_eq!(report.totals.unpriced_requests, 1);
        assert!(!report.by_model[0].priced);
        crate::test_support::remove_tree(&file.parent().unwrap());
    }

    #[test]
    fn failures_are_kept_and_counted_apart() {
        let (file, conn) = ledger("failures");
        let config = AppConfig::default();
        drop(conn);

        for (ok, error) in [(true, ""), (false, "服务商限流（HTTP 429），稍后重试。")] {
            record(
                &file,
                &config,
                "chat",
                "",
                "m",
                &Tokens::default(),
                0,
                false,
                5,
                None,
                ok,
                error,
                1_700_000_000_000,
            );
        }

        let conn = open(&file).unwrap();
        let report = report(&conn, None).unwrap();
        assert_eq!(report.totals.requests, 2);
        assert_eq!(report.totals.failed, 1);
        crate::test_support::remove_tree(&file.parent().unwrap());
    }

    #[test]
    fn the_price_lookup_ignores_letter_case() {
        let (file, conn) = ledger("case");
        upsert_price(&conn, &priced("GLM-5.3-Flash", "1", "1", "0"), 1).unwrap();
        let found = price_for(&conn, "glm-5.3-flash").unwrap();
        assert!(found.is_some(), "大小写不同就该匹配到同一条价格");
        crate::test_support::remove_tree(&file.parent().unwrap());
    }

    /// 按话题的分页：total 数的是"几场话题"不是几行请求；页内按费用降序；
    /// 越界的页给空行而不炸。长尾翻得到，"花钱最多的 50 场"不再是天花板
    #[test]
    fn conversation_pages_report_the_total_and_order_by_cost() {
        let (file, conn) = ledger("conv-page");
        // 配上单价：没价格时费用全是 0，平局顺序是 SQLite 说了算，测试就成掷硬币
        upsert_price(&conn, &priced("glm-5.3", "10", "50", "1"), 1).unwrap();
        drop(conn);
        let config = AppConfig {
            base_url: "https://relay.example/v1".into(),
            model: "glm-5.3".into(),
            api_format: "responses".into(),
            ..Default::default()
        };

        // 三场话题：conv-big 花得最多，未归属的最少
        for (conversation, input, ts) in [
            ("conv-big", 1_000, 1_700_000_000_000),
            ("conv-big", 2_000, 1_700_000_000_001),
            ("conv-mid", 300, 1_700_000_000_002),
            ("", 100, 1_700_000_000_003),
        ] {
            record(
                &file,
                &config,
                "chat",
                conversation,
                "glm-5.3",
                &Tokens {
                    input,
                    output: 0,
                    ..Default::default()
                },
                0,
                false,
                10,
                None,
                true,
                "",
                ts,
            );
        }

        let conn = open(&file).unwrap();
        let first = conversations_page(&conn, None, 0, 2).unwrap();
        assert_eq!(first.total, 3, "三场话题（空串也占一场）");
        assert_eq!(first.rows.len(), 2);
        assert_eq!(first.rows[0].conversation, "conv-big", "费用降序");
        assert_eq!(first.rows[0].requests, 2, "两发记在同一场名下");
        assert_eq!(first.rows[1].conversation, "conv-mid");

        let second = conversations_page(&conn, None, 2, 2).unwrap();
        assert_eq!(second.total, 3);
        assert_eq!(second.rows.len(), 1);
        assert_eq!(second.rows[0].conversation, "", "未归属的那场排在最后");

        let beyond = conversations_page(&conn, None, 99, 2).unwrap();
        assert_eq!(beyond.total, 3);
        assert!(beyond.rows.is_empty(), "越界的页是空的，不是报错");

        // 时间窗收窄：窗口外的那场不计入 total
        let windowed = conversations_page(&conn, Some(1_700_000_000_002), 0, 2).unwrap();
        assert_eq!(windowed.total, 2, "窗口只留最后两发的那两场");
        crate::test_support::remove_tree(&file.parent().unwrap());
    }

    #[test]
    fn the_window_only_cuts_off_the_older_rows() {
        let (file, conn) = ledger("window");
        let config = AppConfig::default();
        drop(conn);

        let day_ms = 86_400_000i64;
        let now = 1_700_000_000_000;
        for ts in [now - 10 * day_ms, now - day_ms, now] {
            record(
                &file,
                &config,
                "chat",
                "",
                "m",
                &Tokens::default(),
                0,
                false,
                1,
                None,
                true,
                "",
                ts,
            );
        }

        let conn = open(&file).unwrap();
        assert_eq!(
            report(&conn, Some(now - 3 * day_ms))
                .unwrap()
                .totals
                .requests,
            2
        );
        assert_eq!(report(&conn, None).unwrap().totals.requests, 3);
        crate::test_support::remove_tree(&file.parent().unwrap());
    }

    /// 界面读的是这些键名，少一个就是一片 undefined
    #[test]
    fn the_usage_payload_matches_the_frontend_types() {
        let report = Report {
            totals: Totals {
                requests: 0,
                failed: 0,
                input_tokens: 0,
                output_tokens: 0,
                cached_tokens: 0,
                reasoning_tokens: 0,
                cost_usd: 0.0,
                unpriced_requests: 0,
                unreported_cache_requests: 0,
            },
            by_model: vec![ModelUsage {
                model: String::new(),
                requests: 0,
                input_tokens: 0,
                output_tokens: 0,
                cached_tokens: 0,
                cost_usd: 0.0,
                priced: false,
            }],
            daily: vec![DayUsage {
                date: String::new(),
                requests: 0,
                cost_usd: 0.0,
            }],
        };
        let value = serde_json::to_value(&report).unwrap();
        crate::test_support::assert_matches_ts(&value, "UsageReport");
        crate::test_support::assert_matches_ts(&value["totals"], "UsageTotals");
        crate::test_support::assert_matches_ts(&value["byModel"][0], "ModelUsage");
        crate::test_support::assert_matches_ts(&value["daily"][0], "DayUsage");
        // 分页载荷：按话题聚合的表自己拉数据，形状单独钉
        let page = ConversationPage {
            rows: vec![ConversationUsage {
                conversation: String::new(),
                requests: 0,
                input_tokens: 0,
                output_tokens: 0,
                cost_usd: 0.0,
            }],
            total: 0,
        };
        crate::test_support::assert_matches_ts(&serde_json::to_value(&page).unwrap(), "ConversationPage");
        crate::test_support::assert_matches_ts(
            &serde_json::to_value(&page.rows[0]).unwrap(),
            "ConversationUsage",
        );
        crate::test_support::assert_matches_ts(
            &serde_json::to_value(Price::default()).unwrap(),
            "ModelPrice",
        );
        // P1：请求明细分页的两个载荷
        let page = UsageRequestPage {
            rows: vec![UsageRequestRow {
                id: 0,
                ts: 0,
                model: String::new(),
                base_url: String::new(),
                input_tokens: 0,
                output_tokens: 0,
                cached_tokens: 0,
                cache_reported: true,
                reasoning_tokens: 0,
                cost_usd: 0.0,
                priced: false,
                latency_ms: 0,
                first_token_ms: None,
                ok: true,
                error: String::new(),
            }],
            total: 0,
        };
        crate::test_support::assert_matches_ts(
            &serde_json::to_value(&page).unwrap(),
            "UsageRequestPage",
        );
        crate::test_support::assert_matches_ts(
            &serde_json::to_value(&page.rows[0]).unwrap(),
            "UsageRequestRow",
        );
        // 话题缓存聚合以前没人守：加字段时前端只会静默读到 undefined
        crate::test_support::assert_matches_ts(
            &serde_json::to_value(SessionCacheUsage {
                input_tokens: 0,
                cached_tokens: 0,
                requests: 0,
                reported_requests: 0,
                last_input_tokens: 0,
                last_cached_tokens: 0,
                last_cache_reported: true,
                last_output_tokens: 0,
                wasted_tokens: 0,
                wasted_cost_usd: 0.0,
            })
            .unwrap(),
            "SessionCacheUsage",
        );
    }

    /// 明细分页：时间窗只切旧行，总数跟着窗口走，翻页不重不漏。
    #[test]
    fn the_recent_pages_follow_the_window_and_never_skip_rows() {
        let (file, conn) = ledger("recent");
        let config = AppConfig::default();
        drop(conn);

        let day_ms = 86_400_000i64;
        let now = 1_700_000_000_000;
        // 5 行：3 行在窗口内（now、now-1d、now-2d），2 行在窗口外
        for (index, ts) in [
            now,
            now - day_ms,
            now - 2 * day_ms,
            now - 9 * day_ms,
            now - 10 * day_ms,
        ]
        .into_iter()
        .enumerate()
        {
            record(
                &file,
                &config,
                "chat",
                "",
                "m",
                &Tokens::default(),
                0,
                false,
                1,
                None,
                true,
                "",
                ts - index as i64, // 微减一毫秒保证排序稳定
            );
        }

        let conn = open(&file).unwrap();
        let since = Some(now - 3 * day_ms);

        let first = recent(&conn, since, 0, 20).unwrap();
        assert_eq!(first.total, 3, "窗口外的行只影响总数，不该出现");
        assert_eq!(first.rows.len(), 3);
        assert!(first.rows[0].ts >= first.rows[1].ts, "按时间倒序");

        // 每页 2 行翻页：第一页 2 行、第二页 1 行，加起来正好覆盖全部
        let page_one = recent(&conn, since, 0, 2).unwrap();
        let page_two = recent(&conn, since, 2, 2).unwrap();
        assert_eq!(page_one.rows.len(), 2);
        assert_eq!(page_two.rows.len(), 1);
        let mut ids: Vec<i64> = page_one
            .rows
            .iter()
            .chain(page_two.rows.iter())
            .map(|row| row.id)
            .collect();
        ids.sort();
        assert_eq!(ids, vec![1, 2, 3], "三页翻完必须不重不漏");

        // limit 收口：离谱的值被夹回合法区间，不会把整表倒给前端
        assert_eq!(recent(&conn, None, 0, 999).unwrap().rows.len(), 5);
        assert_eq!(recent(&conn, None, 4, 0).unwrap().rows.len(), 1);

        crate::test_support::remove_tree(&file.parent().unwrap());
    }

    /// CSV 带 BOM 落盘：Excel 打开中文列名不乱码就靠它。
    #[test]
    fn the_exported_csv_starts_with_a_bom() {
        let root = crate::test_support::temp_dir("csv");
        let path = root.join("out.csv");
        export_csv(path.to_str().unwrap(), "模型,费用\nm,0.1\n").unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..3], &[0xEF, 0xBB, 0xBF], "UTF-8 BOM 必须在最前面");

        crate::test_support::remove_tree(&root);
    }
}
