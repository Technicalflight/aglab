//! 治理层：每日流水账、TTL 过期、30 天蒸馏、超限判定、冲突取代。
//!
//! 这几件事共用同一个前提——Markdown 是真相源、索引随时可整份重建——所以任何
//! 状态变更都先改文件、再让 `index::sync_file` 跟上，写入一律走 mod.rs 的
//! `append_record` 那一道闸。绕开它的分支将来一定会出现"文件里没有、索引里却有"。

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::Serialize;

use super::decay;
use super::extract::{accept, similarity, Accepted, Provenance};
use super::{
    audit, audit_as, display_path, now_rfc3339, parse_records, read_text, resolve_file,
    rewrite_file, Hit, MemoryConfig, MemoryRecord, MemoryScope, MemorySource, MemoryStatus,
    MemoryView, Paths, Stability,
};

/// 取代阈值：相似度低于去重阈值但高于这条线，说明"讲的很可能是同一件事、但说法变了"
pub const CONFLICT_FLOOR: f64 = 0.55;

/// 一次蒸馏喂给模型的日志正文上限。超了就只喂最近的整天——
/// 被截断的天压根没进提示词，也就不能被判成"已消费"而归档掉
const MATERIAL_BUDGET: usize = 12_000;

/// 日志行里正文摘要的长度。一天的日志是人翻的，一行一条就够定位
const LOG_SUMMARY_CHARS: usize = 100;

// ---------------------------------------------------------------- 每日日志

/// 这条记录的流水账该写进哪个 daily 目录。跟 `target_file` 同源，否则
/// "项目里加的记忆"记到了全局账上，蒸馏时就找不回它属于哪个项目了
fn daily_dir_for(paths: &Paths, workspace: Option<&Path>, record: &MemoryRecord) -> PathBuf {
    let project = record
        .project_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .map(|id| id.to_string());
    match record.scope {
        MemoryScope::Global => paths.global_daily(),
        MemoryScope::Project => match workspace {
            Some(workspace) => Paths::workspace_daily(workspace),
            None => project
                .map(|id| paths.project_daily(&id))
                .unwrap_or_else(|| paths.global_daily()),
        },
        // 话题与临时记忆住在 aglab 目录：它们的账也记在同一处
        MemoryScope::Session | MemoryScope::Temp => project
            .map(|id| paths.project_daily(&id))
            .unwrap_or_else(|| paths.global_daily()),
    }
}

/// 压成一行并截断。日志里出现换行会把后面所有行的对齐弄坏
fn one_line(text: &str, max: usize) -> String {
    let flat = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let chars: Vec<char> = flat.chars().collect();
    if chars.len() <= max {
        return flat;
    }
    let mut head: String = chars[..max].iter().collect();
    head.push('…');
    head
}

/// 追加一行到当天的日志。只 append：这一天的原始事实比"排版好看"重要得多，
/// 重写就等于给历史记录开口子
pub fn append_daily_log(
    paths: &Paths,
    workspace: Option<&Path>,
    record: &MemoryRecord,
) -> Result<(), String> {
    let dir = daily_dir_for(paths, workspace, record);
    fs::create_dir_all(&dir).map_err(|e| format!("创建 {} 失败：{e}", dir.display()))?;
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let file = dir.join(format!("{today}.md"));
    let line = format!(
        "- {} 追加 [{}/{}] {} ({})\n",
        chrono::Local::now().format("%H:%M"),
        record.kind.as_str(),
        record.scope.as_str(),
        one_line(&record.content, LOG_SUMMARY_CHARS),
        record.id,
    );
    let mut handle = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file)
        .map_err(|e| format!("打开 {} 失败：{e}", file.display()))?;
    // 新建当天第一行前面补个日期抬头，人类读起来才知道这是哪一天
    if handle.metadata().map(|meta| meta.len() == 0).unwrap_or(false) {
        let _ = handle.write_all(format!("# {today}\n\n").as_bytes());
    }
    handle
        .write_all(line.as_bytes())
        .map_err(|e| format!("写 {} 失败：{e}", file.display()))
}

// ---------------------------------------------------------------- 状态改写

/// 把指定 id 改成某个状态。**先写 Markdown 再同步索引**，顺序反了就破规矩。
/// 返回真正被改动的条数，没变过就一个字节都不写
pub fn set_status(
    conn: &Connection,
    paths: &Paths,
    file: &Path,
    ids: &[String],
    status: MemoryStatus,
) -> Result<usize, String> {
    let _guard = super::write_guard();
    let mut records = parse_records(&read_text(file))
        .map_err(|e| format!("{} 本来就读不了，先修它：{e}", file.display()))?;
    let mut touched: Vec<String> = Vec::new();
    for record in &mut records {
        if !ids.iter().any(|id| id == &record.id) || record.status == status {
            continue;
        }
        record.status = status;
        // 时间戳跟着走：这条记录的"最后一次变化"就是现在，不是它上次被编辑的时候
        record.updated_at = now_rfc3339();
        touched.push(record.id.clone());
    }
    if touched.is_empty() {
        return Ok(0);
    }
    rewrite_file(conn, paths, file, &records)?;
    for id in &touched {
        audit(paths, status.as_str(), id)?;
    }
    Ok(touched.len())
}

#[derive(Debug, Clone, Default)]
pub struct Maintenance {
    /// 被 TTL 扫掉的记录 id
    pub archived: Vec<String>,
}

/// TTL 到期清扫。检索前与蒸馏前都要跑一次：过期的临时记忆继续被注入，
/// 是这套系统最容易让用户恼火的地方
pub fn maintain(
    conn: &Connection,
    paths: &Paths,
    workspace: Option<&Path>,
    config: &super::MemoryConfig,
) -> Result<Maintenance, String> {
    // 索引里此刻只有全局与当前项目的行（调用方刚 sync 过），所以不必再按 project_id 过滤
    let mut by_file: BTreeMap<String, Vec<String>> = BTreeMap::new();
    {
        let mut statement = conn
            .prepare(
                "SELECT path, id, ttl_days, updated_at FROM memories \
                 WHERE ttl_days IS NOT NULL AND status != 'archived' AND status != 'deleted'",
            )
            .map_err(|e| e.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        for row in rows {
            let (path, id, ttl, updated_at) = row.map_err(|e| e.to_string())?;
            // 天数语法与新鲜度共用 decay 那一份：两处各认一套的话，
            // "读不懂时间戳"在一边是"不动它"、在另一边就成了"归档掉"
            let expired = decay::age_days(&updated_at).is_some_and(|age| age >= ttl);
            if expired {
                by_file.entry(path).or_default().push(id);
            }
        }
    }

    // 候选区的自然衰减：模型提的、用户一直没点头的候选，过了 TTL 且两道自动转正
    // 门槛都还在线下，归档而不是永远挂在列表里。归档可逆、正文一字不动——
    // "静默删用户的记忆"是最坏的选择，但让三十天前的低置信候选永远占着候选区，
    // 等于把"要不要这条"的决定权从用户手里偷走后还把账单留给他
    let mut stale_candidates: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if config.candidate_ttl_days > 0 {
        let mut statement = conn
            .prepare(
                "SELECT path, id, confidence, importance, updated_at FROM memories \
                 WHERE status = 'candidate'",
            )
            .map_err(|e| e.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, f64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        for row in rows {
            let (path, id, confidence, importance, updated_at) = row.map_err(|e| e.to_string())?;
            let aged = decay::age_days(&updated_at)
                .is_some_and(|age| age >= config.candidate_ttl_days as i64);
            // 三条同时成立才动：到期，且置信度与重要性都还在自动转正线之下。
            // 有任何一项过线，说明用户可能只是还没看到——归档它等于替用户做决定
            let below_bar = confidence < config.auto_accept_confidence
                && importance < config.auto_accept_importance as i64;
            if aged && below_bar {
                stale_candidates.entry(path).or_default().push(id);
            }
        }
    }

    let mut report = Maintenance::default();
    for (shown, ids) in by_file {
        let file = resolve_file(paths, workspace, &shown);
        let changed = set_status(conn, paths, &file, &ids, MemoryStatus::Archived)?;
        if changed > 0 {
            report.archived.extend(ids);
        }
    }
    for (shown, ids) in stale_candidates {
        let file = resolve_file(paths, workspace, &shown);
        let changed = set_status(conn, paths, &file, &ids, MemoryStatus::Archived)?;
        if changed > 0 {
            report.archived.extend(ids);
        }
    }
    Ok(report)
}

// ---------------------------------------------------------------- 限额

fn chars_of(text: &str) -> usize {
    text.chars().count()
}

/// 长期记忆文件超了字符上限。只报事实，不截断：静默删用户的记忆是最坏的选择，
/// 该由 UI 提示去蒸馏
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OverBudget {
    pub path: String,
    pub chars: usize,
    pub limit: usize,
}

impl OverBudget {
    /// 给界面的一句话：哪一份超了、超了多少。只报文件名的话，用户没法判断
    /// 这一点超限值不值得为它跑一次蒸馏
    pub fn shown(&self) -> String {
        format!("{}（正文 {} 字 / 上限 {}）", self.path, self.chars, self.limit)
    }
}

pub fn over_budget(
    paths: &Paths,
    workspace: Option<&Path>,
    project_id: Option<&str>,
    config: &MemoryConfig,
) -> Result<Vec<OverBudget>, String> {
    let mut over = Vec::new();
    for file in super::record_files(paths, workspace, project_id) {
        let global = file == paths.global_memory();
        let limit = if global {
            config.global_limit_chars
        } else {
            config.project_limit_chars
        };
        // 量的是正文：frontmatter 是我们自己写的记账字段，用它撑爆上限毫无意义
        // 读不了的文件如实报错：静默当成"没超限"，超限告警就永远不响了
        let body: String = parse_records(&read_text(&file))
            .map_err(|e| format!("{} 读不了：{e}——真相源修好之前，超限告警不可信", file.display()))?
            .iter()
            .filter(|record| record.status == MemoryStatus::Active)
            .map(|record| record.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let chars = chars_of(&body);
        if chars > limit {
            over            .push(OverBudget {
                path: display_path(&paths.root, &file),
                chars,
                limit,
            });
        }
    }
    Ok(over)
}

// ---------------------------------------------------------------- 冲突取代

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// 新的取代旧的：旧的转 archived
    Supersede,
    /// 旧的更有话语权：新的只能进 candidate 等用户裁决
    OldWins,
    /// 话语权相当又拿不准：两条都留着，标上冲突
    KeepBoth,
}

/// 话语权：用户显式说过的最高，助手写下的与导入的次之，自动推断最低；
/// 同一来源里再比作用域——贴在项目/话题上的压过全局偏好。乘 2 是给作用域留一位
fn authority(source: MemorySource, scope: MemoryScope) -> u8 {
    let source_rank = match source {
        MemorySource::User => 3,
        MemorySource::Assistant => 2,
        MemorySource::Import => 2,
        MemorySource::Inferred => 1,
    };
    let scope_rank = u8::from(scope != MemoryScope::Global);
    source_rank * 2 + scope_rank
}

/// 一个记录在冲突判定里的"身份"：谁说的、说在哪、有多确定。
/// 冲突视图要成对判，两边都是它，所以把这三样单独拎出来
#[derive(Debug, Clone, Copy)]
pub struct Voice {
    pub source: MemorySource,
    pub scope: MemoryScope,
    pub confidence: f64,
}

/// 同一件事两种说法时谁说了算。用户明说过的，绝不因为模型"后来推出来一个不一样的"
/// 就被悄悄改掉。`older` 是场面上先站着的那条，`newer` 是后来主张的那条
pub fn verdict_between(older: Voice, newer: Voice) -> Verdict {
    let mine = authority(newer.source, newer.scope);
    let theirs = authority(older.source, older.scope);
    match mine.cmp(&theirs) {
        std::cmp::Ordering::Greater => Verdict::Supersede,
        std::cmp::Ordering::Less => Verdict::OldWins,
        // 同作用域内新且高置信 > 旧；再拿不平就两条都留着让人来看
        std::cmp::Ordering::Equal if newer.confidence > older.confidence => Verdict::Supersede,
        std::cmp::Ordering::Equal => Verdict::KeepBoth,
    }
}

/// 同一件事两种说法时谁说了算。写侧的形状：手里只有旧条目的一行命中和新记录本体
pub fn decide(rival: &Hit, incoming: &MemoryRecord) -> Verdict {
    verdict_between(
        Voice {
            source: rival.source.parse().unwrap_or(MemorySource::Inferred),
            scope: rival.scope.parse().unwrap_or(MemoryScope::Global),
            confidence: rival.confidence,
        },
        Voice {
            source: incoming.source,
            scope: incoming.scope,
            confidence: incoming.confidence,
        },
    )
}

/// 找出"大概讲的同一件事但说法不同"的那条。取最像的，避免一次改动牵动多条
pub fn rival_of(hits: &[Hit], content: &str, config: &MemoryConfig) -> Option<Hit> {
    if config.dedupe_similarity <= CONFLICT_FLOOR {
        return None;
    }
    hits.iter()
        .filter(|hit| {
            let score = similarity(content, &hit.content);
            score >= CONFLICT_FLOOR && score < config.dedupe_similarity
        })
        .max_by(|a, b| {
            similarity(content, &a.content)
                .partial_cmp(&similarity(content, &b.content))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .cloned()
}

// ---------------------------------------------------------------- 冲突呈现（人选，不自动合）

/// 一份文件里最多两两比多少条。冲突带的判定是 O(n²) 的，设置页点开不能卡住；
/// 一个文件的上限本来就按 3000 字设计，撞不到这个数
const CONFLICT_SCAN_CAP: usize = 200;

/// 一对撞上的记忆：谁和谁、在哪个带上撞的、系统建议怎么选。
/// 建议只是**预选**——除了"用户明说 > 推断"那一条规则，其余一律人来裁决
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictPair {
    /// 场面先站着的那条（按 created_at）
    pub a: MemoryView,
    /// 后来主张的那条
    pub b: MemoryView,
    pub similarity: f64,
    pub floor: f64,
    /// 一句人话：两条像到什么程度、这条结论是从哪儿来的
    pub why: String,
    pub recommendation: super::ConflictChoice,
}

fn voice_of(record: &MemoryRecord) -> Voice {
    Voice {
        source: record.source,
        scope: record.scope,
        confidence: record.confidence,
    }
}

/// 索引里已经站着的两条记录是不是一对冲突（以及撞在哪个带）。
/// 只在 [冲突带, 去重阈值) 这一格里才算"同一件事两种说法"：
/// 低于带的是两件事，高于阈值的是同一句话，都不该打扰用户
fn band_of(a: &MemoryView, b: &MemoryView, config: &MemoryConfig) -> Option<f64> {
    if config.dedupe_similarity <= CONFLICT_FLOOR {
        return None;
    }
    let score = similarity(&a.record.content, &b.record.content);
    (score >= CONFLICT_FLOOR && score < config.dedupe_similarity).then_some(score)
}

fn pair_key(a: &str, b: &str) -> (String, String) {
    if a <= b { (a.into(), b.into()) } else { (b.into(), a.into()) }
}

/// 成对拉出现在还咬得动的冲突。
///
/// 只列**有实际影响**的：两条都还 active（都会进注入），或者取代边已经画了、
/// 败者却还 active（归档没落地）。归档了的、已经没人再提的不打扰用户
pub fn conflicts(conn: &Connection, config: &MemoryConfig) -> Result<Vec<ConflictPair>, String> {
    let views = super::list_all(conn)?;
    let active: Vec<&MemoryView> = views
        .iter()
        .filter(|view| view.record.status == MemoryStatus::Active)
        .collect();
    let find = |id: &str| active.iter().find(|view| view.record.id == id).cloned();

    // 一对 (id, id)（按 id 排好，双向都归一到同一个键）→ 这条结论是从哪儿来的
    let mut evidence: std::collections::BTreeMap<(String, String), &'static str> =
        std::collections::BTreeMap::new();

    // 先收边：那是"提取时判定过、写进了 frontmatter"的证据，重建索引也还在
    let mut statement = conn
        .prepare(
            "SELECT from_id, to_id, kind FROM memory_links \
             WHERE kind IN ('conflicts_with','supersedes')",
        )
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })
        .map_err(|e| e.to_string())?;
    for row in rows {
        let (from, to, kind) = row.map_err(|e| e.to_string())?;
        let note = if kind == "supersedes" {
            "取代边已经写下、对面那条却还 active"
        } else {
            "提取时标的冲突标记"
        };
        if find(&from).is_some() && find(&to).is_some() {
            evidence.entry(pair_key(&from, &to)).or_insert(note);
        }
    }

    // 已经裁决过"两条都留"的那些对：tombstone 边就在同一张 links 表里。
    // 这个视图回答的是"有没有要人决定的事"，不是"内容像不像"——
    // 不读它的话，选完"都要"之后同一对还会一直挂在设置页上
    let mut settled: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    {
        let mut statement = conn
            .prepare("SELECT from_id, to_id FROM memory_links WHERE kind = 'conflict_settled'")
            .map_err(|e| e.to_string())?;
        let rows = statement
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
            .map_err(|e| e.to_string())?;
        for row in rows {
            let (from, to) = row.map_err(|e| e.to_string())?;
            settled.insert(pair_key(&from, &to));
        }
    }

    // 再补现算：用户手写的两条互相打脸的记忆没有边，但它照样会一起被注入
    let mut by_file: std::collections::BTreeMap<String, Vec<&MemoryView>> = std::collections::BTreeMap::new();
    for view in &active {
        by_file.entry(view.path.clone()).or_default().push(view);
    }
    for (_file, rows) in by_file {
        let rows: Vec<&MemoryView> = rows.into_iter().take(CONFLICT_SCAN_CAP).collect();
        for (index, a) in rows.iter().enumerate() {
            for b in rows.iter().skip(index + 1) {
                if band_of(a, b, config).is_some() {
                    // 已经有边说它俩撞了，就别用"现算"覆盖那条更准的依据
                    evidence
                        .entry(pair_key(&a.record.id, &b.record.id))
                        .or_insert("同一份文件里现算出来的");
                }
            }
        }
    }

    let mut pairs = Vec::new();
    for ((first, second), note) in &evidence {
        let (Some(one), Some(other)) = (find(first), find(second)) else { continue };
        let (older, newer) = if one.record.created_at <= other.record.created_at { (one, other) } else { (other, one) };
        // 用户已经裁决过"两条都留"的一对不再报：视图问的是"谁需要决定"，不是"内容像不像"
        if settled.contains(&pair_key(&older.record.id, &newer.record.id)) {
            continue;
        }
        // 边可能来自更早一次判定，中间正文被人改过或两条早就各奔东西。
        // 现在不像了就不是冲突，别再拿去烦人
        let Some(score) = band_of(older, newer, config) else { continue };
        let recommendation = match verdict_between(voice_of(&older.record), voice_of(&newer.record)) {
            Verdict::Supersede => super::ConflictChoice::NewerWins,
            Verdict::OldWins => super::ConflictChoice::OlderWins,
            Verdict::KeepBoth => super::ConflictChoice::KeepBoth,
        };
        pairs.push(ConflictPair {
            why: format!(
                "两条说法相似 {score:.2}（≥ 冲突带 {CONFLICT_FLOOR}、< 去重阈值 {:.2}）· 依据：{note}",
                config.dedupe_similarity
            ),
            similarity: score,
            floor: CONFLICT_FLOOR,
            a: older.clone(),
            b: newer.clone(),
            recommendation,
        });
    }
    pairs.sort_by(|x, y| y.similarity.partial_cmp(&x.similarity).unwrap_or(std::cmp::Ordering::Equal));
    Ok(pairs)
}

/// 用户裁决完一对冲突。落盘只有既有那两条路：改 status 走 `set_status`（带审计），
/// 写取代边走 `stamp_records`（带门禁与审计）——**没有一条会删正文**，
/// 历史留在 Markdown 里，`/memory list` 仍看得见归档了什么
pub fn resolve(
    conn: &Connection,
    paths: &Paths,
    workspace: Option<&Path>,
    a: &str,
    b: &str,
    choice: super::ConflictChoice,
) -> Result<String, String> {
    use super::ConflictChoice;
    let views = super::list_all(conn)?;
    let find = |id: &str| {
        views
            .iter()
            .find(|view| view.record.id == id)
            .cloned()
            .ok_or_else(|| format!("索引里没有记忆 {id}。先跑一次重建。"))
    };
    let (one, other) = (find(a)?, find(b)?);
    let (older, newer) = if one.record.created_at <= other.record.created_at { (one, other) } else { (other, one) };

    match choice {
        ConflictChoice::NewerWins | ConflictChoice::OlderWins => {
            let (winner, loser) = if matches!(choice, ConflictChoice::NewerWins) {
                (&newer, &older)
            } else {
                (&older, &newer)
            };
            let loser_id = loser.record.id.clone();
            let winner_id = winner.record.id.clone();
            // 先写下胜者的 supersedes，再归档败者：顺序反了的话，中间任何一步出错
            // 都会留下"两条都 active、谁也没被取代"的场面
            super::stamp_records(
                conn,
                paths,
                workspace,
                std::slice::from_ref(&winner_id),
                crate::audit::Actor::User,
                "supersede",
                &|record: &mut MemoryRecord| {
                    if record.supersedes.iter().any(|held| held == &loser_id) {
                        return false;
                    }
                    record.supersedes.push(loser_id.clone());
                    // 选完就不是"待裁决"了：标记留着，设置页会一直报同一件事
                    record.clear_conflicts();
                    true
                },
            )?;
            let file = resolve_file(paths, workspace, &loser.path);
            set_status(conn, paths, &file, &[loser_id.clone()], MemoryStatus::Archived)?;
            Ok(format!(
                "已留下 {winner_id}，归档 {loser_id}。两条正文都还在文件里。"
            ))
        }
        ConflictChoice::KeepBoth => {
            let older_id = older.record.id.clone();
            let newer_id = newer.record.id.clone();
            let ids = vec![older_id.clone(), newer_id.clone()];
            let changed = super::stamp_records(
                conn,
                paths,
                workspace,
                &ids,
                crate::audit::Actor::User,
                "keep_both",
                &|record: &mut MemoryRecord| {
                    // 对手就是这一对里的另一条。两边各记一份"已裁决"，视图才不用猜方向；
                    // 只清待裁决的标记是不够的——正文还是那两条像的话，现算又会把它们捞回来
                    let peer = if record.id == older_id { &newer_id } else { &older_id };
                    let had_marker = !record.conflict_peers().is_empty();
                    let fresh = !record.settled_with(peer);
                    record.settle_with(peer);
                    had_marker || fresh
                },
            )?;
            Ok(format!("两条都留着（{changed} 条记下了这一对已经裁决过）。谁也没被归档。"))
        }
        ConflictChoice::ArchiveBoth => {
            let mut moved = 0usize;
            for view in [&older, &newer] {
                let file = resolve_file(paths, workspace, &view.path);
                moved += set_status(
                    conn,
                    paths,
                    &file,
                    std::slice::from_ref(&view.record.id),
                    MemoryStatus::Archived,
                )?;
            }
            Ok(format!("两条都归档了（{moved} 条动了）。文件里的正文一条都没删。"))
        }
    }
}

// ---------------------------------------------------------------- 蒸馏

/// 一次蒸馏要吃的东西：进过提示词的日志文件，以及拼好的素材
#[derive(Debug, Clone, Default)]
pub struct Batch {
    /// 内容真的被喂给模型的那些日志文件。只有这些才允许被归档
    pub logs: Vec<PathBuf>,
    pub material: String,
}

/// 哪些目录有日志、消费完归档去哪。成对给，免得到处猜目录布局
fn daily_dirs(
    paths: &Paths,
    workspace: Option<&Path>,
    project_id: Option<&str>,
) -> Vec<(PathBuf, PathBuf)> {
    let mut dirs = vec![(paths.global_daily(), paths.global_archive())];
    if let Some(id) = project_id {
        dirs.push((paths.project_daily(id), paths.project_archive(id)));
    }
    if let Some(workspace) = workspace {
        dirs.push((Paths::workspace_daily(workspace), Paths::workspace_archive(workspace)));
    }
    dirs
}

/// 到该被蒸馏的日志，按日期从新到旧。文件名不是 `YYYY-MM-DD` 的一律不碰：
/// 判不出日期的东西不能被"消费"掉
pub fn eligible_logs(
    paths: &Paths,
    workspace: Option<&Path>,
    project_id: Option<&str>,
    config: &MemoryConfig,
) -> Vec<PathBuf> {
    let today = chrono::Local::now().date_naive();
    let mut found: Vec<(i64, PathBuf)> = Vec::new();
    for (dir, _) in daily_dirs(paths, workspace, project_id) {
        for file in super::daily_files(&dir) {
            let Some(stem) = file.file_stem().and_then(|name| name.to_str()) else {
                continue;
            };
            let Ok(day) = chrono::NaiveDate::parse_from_str(stem, "%Y-%m-%d") else {
                continue;
            };
            let age = (today - day).num_days();
            if age >= config.distill_after_days as i64 {
                found.push((age, file));
            }
        }
    }
    // 新的先喂：预算不够时先丢的是最老的那些，而不是最近刚发生的事
    found.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    found.into_iter().map(|(_, file)| file).collect()
}

/// 还活着的临时/话题记忆：它们的措辞值得被判一次"该不该转成长期的"
fn promotable_lines(conn: &Connection) -> Result<String, String> {
    let mut statement = conn
        .prepare(
            "SELECT type, scope, stability, body FROM memories \
             WHERE status = 'active' AND (stability = 'volatile' OR scope IN ('session','temp')) \
             ORDER BY updated_at DESC LIMIT 200",
        )
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    let mut out = String::new();
    for row in rows {
        let (kind, scope, stability, body) = row.map_err(|e| e.to_string())?;
        out.push_str(&format!(
            "- [{kind}/{scope}/{stability}] {}\n",
            one_line(&body, LOG_SUMMARY_CHARS)
        ));
    }
    Ok(out)
}

pub fn gather(
    conn: &Connection,
    paths: &Paths,
    workspace: Option<&Path>,
    project_id: Option<&str>,
    config: &MemoryConfig,
) -> Result<Batch, String> {
    let mut batch = Batch::default();
    let mut used = 0usize;
    for file in eligible_logs(paths, workspace, project_id, config) {
        let text = read_text(&file);
        let size = chars_of(&text);
        if used + size > MATERIAL_BUDGET && !batch.logs.is_empty() {
            break;
        }
        let shown = file.file_stem().and_then(|name| name.to_str()).unwrap_or("未知日期");
        batch.material.push_str(&format!("=== 日志 {shown} ===\n{text}\n"));
        used += size;
        batch.logs.push(file);
    }
    let promotable = promotable_lines(conn)?;
    if !promotable.is_empty() {
        batch.material.push_str(&format!("=== 待判定的临时记忆 ===\n{promotable}\n"));
    }
    Ok(batch)
}

/// 蒸馏提示词。产品定的那四句要求原样保留，后面补的是机器要的格式契约：
/// frontmatter 少一个字段整份输出就读不了，而读不懂的输出绝不能被当成"没有可记的"。
/// 类型清单与提取同源（`MemoryKind::ALL`），两处各列一份迟早会一边长一边短
pub fn prompt_for(material: &str) -> String {
    format!(
        "将以下 30 天日志按主题蒸馏为不超过 20 条长期记忆。\
要求：1. 只保留可复用、稳定、对未来的对话有帮助的信息。2. 合并重复项。3. 删除临时情绪、一次性任务、敏感信息。4. 输出 Markdown frontmatter + 正文。\n\
每条记忆写成一个 frontmatter 块，字段一个都不能少：\n\
---\n\
id: <唯一字符串>\n\
type: {}\n\
scope: global|project\n\
status: active\n\
importance: 1-5\n\
confidence: 0-1\n\
stability: stable\n\
source: user|assistant|inferred|import\n\
created_at: <RFC3339>\n\
updated_at: <RFC3339>\n\
tags: [\"标签\"]\n\
supersedes: []\n\
---\n\
正文（一句中文陈述，不要引用原文）\n\n\
没有值得长期保留的就只输出「无」。不要输出代码块围栏以外的解释。\n\n\
日志：\n{material}",
        super::MemoryKind::contract()
    )
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DistillSummary {
    /// TTL 到期归档的记录数
    pub archived_records: usize,
    /// 搬进 archive/ 的日志文件数
    pub archived_logs: usize,
    /// 新写进长期记忆的条数（合并进旧条目的不算）
    pub distilled: usize,
    /// 蒸馏之后仍有文件超过字符上限
    pub overbudget: bool,
}

/// 剥掉模型爱加的外层 ``` 围栏。不剥的话收尾那三反引号会混进正文，
/// 被当成一条长期记忆存下去
fn strip_code_fence(raw: &str) -> String {
    let text = raw.trim();
    let Some(rest) = text.strip_prefix("```") else {
        return text.to_string();
    };
    let rest = match rest.find('\n') {
        Some(at) => &rest[at + 1..],
        None => rest,
    };
    match rest.trim_end().strip_suffix("```") {
        Some(inner) => inner.trim().to_string(),
        None => rest.trim().to_string(),
    }
}

/// 把模型的蒸馏结果落地，然后把**确实喂进去过**的日志搬去 archive/。
/// 顺序不能换：任何一步报错都直接返回，日志留在原地——
/// 一次失败的蒸馏把原始历史吃掉是不可接受的
pub fn land(
    conn: &Connection,
    paths: &Paths,
    workspace: Option<&Path>,
    config: &MemoryConfig,
    project_id: Option<&str>,
    raw: &str,
    batch: &Batch,
) -> Result<DistillSummary, String> {
    let records = parse_records(&strip_code_fence(raw))
        .map_err(|e| format!("蒸馏输出读不了，日志一个字都没动：{e}"))?;

    let mut landing: Vec<MemoryRecord> = Vec::with_capacity(records.len());
    for mut record in records {
        // id 与时间戳由本机决定：模型给的 id 第二轮就会撞车，
        // 而它给的时间戳只会把"什么时候知道的"写成它以为的那个
        record.id = super::new_id();
        record.created_at = now_rfc3339();
        record.updated_at = record.created_at.clone();
        record.source = MemorySource::Assistant;
        // 蒸馏产物带 TTL 就自相矛盾；supersedes 与冲突标记由冲突判定自己算
        record.ttl_days = None;
        record.supersedes = Vec::new();
        record.clear_conflicts();
        record.stability = Stability::Stable;
        // 来历只能是日志，不能是模型自己在 frontmatter 里"声称"的某次对话
        record.origin = None;
        landing.push(record);
    }

    // 蒸馏是系统在自己反思日志：主体记 Reflection，不记成"用户说的"，也不记成
    // "模型这一轮说的"。转正规则一条不放宽——过不了门槛的照样停在候选区
    let provenance = Provenance { actor: crate::audit::Actor::Reflection, origin: None, must_stay_candidate: false };
    let report = if landing.is_empty() {
        Accepted::default()
    } else {
        accept(conn, paths, workspace, config, project_id, &landing, &provenance)?
    };

    let mut moved = 0usize;
    for file in &batch.logs {
        let Some(daily) = file.parent() else { continue };
        let Some(owner) = daily.parent() else { continue };
        let archive = owner.join(super::ARCHIVE_DIR);
        fs::create_dir_all(&archive).map_err(|e| format!("创建 {} 失败：{e}", archive.display()))?;
        let Some(name) = file.file_name() else { continue };
        let target = archive.join(name);
        fs::rename(file, &target).map_err(|e| {
            format!("把 {} 搬进 archive 失败：{e}。日志还在原地，下次再蒸馏。", file.display())
        })?;
        moved += 1;
    }

    let summary = DistillSummary {
        archived_logs: moved,
        // 合并进旧条目的不算新增：这一项是给"这次多记住了几条"用的
        distilled: report.stored.len().saturating_sub(report.merged),
        ..Default::default()
    };
    // 蒸馏这一笔的账要指得回具体的条目：只记"几条"的话，事后拿着条目 id 查不到它从哪来。
    // 记 id 不记正文——审计日志是给人翻的，正文里可能带敏感信息
    let landed_ids: Vec<String> = report.stored.iter().map(|view| view.record.id.clone()).collect();
    let head: Vec<String> = landed_ids.iter().take(8).cloned().collect();
    let more = landed_ids.len().saturating_sub(head.len());
    audit_as(
        paths,
        crate::audit::Actor::Reflection,
        "distill",
        &format!(
            "{} 篇日志 / {} 条记忆 · {}",
            moved,
            summary.distilled,
            if head.is_empty() {
                "(没有新条目)".to_string()
            } else {
                format!("{}{}", head.join(","), if more > 0 { format!(" +{more}") } else { String::new() })
            }
        ),
    )?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{
        append_record, ensure_layout, list_all, render_records, search, ConflictChoice, MemoryKind,
    };
    use crate::test_support::{remove_tree, temp_dir};

    struct Harness {
        paths: Paths,
        conn: Connection,
        workspace: PathBuf,
    }

    fn harness() -> Harness {
        let paths = Paths::new(temp_dir("govern-root"));
        ensure_layout(&paths).unwrap();
        let conn = crate::memory::index::open(&paths.index_db()).unwrap();
        Harness { paths, conn, workspace: temp_dir("govern-ws") }
    }

    fn teardown(harness: &Harness) {
        remove_tree(&harness.paths.root);
        remove_tree(&harness.workspace);
    }

    fn draft(content: &str) -> MemoryRecord {
        MemoryRecord::draft(MemoryScope::Global, content)
    }

    /// 手记与蒸馏这类没有对话可指的落地：主体写在审计行上，出处留空
    fn unprovened() -> Provenance {
        Provenance { actor: crate::audit::Actor::User, origin: None, must_stay_candidate: false }
    }

    /// 直接写一份带指定时间戳的记录文件，再让索引跟上
    fn seed(conn: &Connection, paths: &Paths, record: &MemoryRecord) {
        let file = paths.global_memory();
        let mut records = parse_records(&read_text(&file)).unwrap();
        records.push(record.clone());
        fs::write(&file, render_records(&records)).unwrap();
        super::super::sync_all(conn, paths, None, None).unwrap();
    }

    #[test]
    fn daily_log_appends_and_never_rewrites_what_is_already_there() {
        let h = harness();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let log = h.paths.global_daily().join(format!("{today}.md"));
        fs::create_dir_all(h.paths.global_daily()).unwrap();
        fs::write(&log, "# 用户早上手写的抬头\n- 08:00 手写的一行\n").unwrap();

        append_record(&h.conn, &h.paths, None, &draft("回答先给结论，再给理由。")).unwrap();
        append_record(&h.conn, &h.paths, None, &draft("用 pnpm 管理依赖。")).unwrap();

        let text = fs::read_to_string(&log).unwrap();
        assert!(text.starts_with("# 用户早上手写的抬头\n- 08:00 手写的一行\n"), "已有的行必须原样在前：{text}");
        assert_eq!(text.matches("追加 [preference/global]").count(), 2, "两条都该落账：{text}");
        assert!(text.contains("用 pnpm 管理依赖"), "摘要要看得出记了什么：{text}");
        assert!(text.contains("mem_"), "行尾要带 id 好回溯：{text}");
        assert_eq!(
            parse_records(&text).unwrap().len(),
            0,
            "日志行是散文，不该被当成记录索引进来"
        );
        assert_eq!(list_all(&h.conn).unwrap().len(), 2);
        teardown(&h);
    }

    #[test]
    fn volatile_and_temp_records_still_hit_the_log() {
        let h = harness();
        let mut temp = draft("这次先在临时分支上试一下。");
        temp.scope = MemoryScope::Temp;
        temp.kind = MemoryKind::Event;
        temp.stability = Stability::Volatile;
        append_record(&h.conn, &h.paths, None, &temp).unwrap();

        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let log = h.paths.global_daily().join(format!("{today}.md"));
        let text = fs::read_to_string(&log).unwrap();
        assert!(text.contains("[event/temp]"), "原始历史连临时的也要记：{text}");
        teardown(&h);
    }

    #[test]
    fn a_project_record_with_a_workspace_logs_into_the_repo_not_the_app_dir() {
        let h = harness();
        let mut record = MemoryRecord::draft(MemoryScope::Project, "这个项目用 pnpm，不要换成 npm。");
        record.project_id = Some("proj-a".into());
        append_record(&h.conn, &h.paths, Some(&h.workspace), &record).unwrap();

        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let in_repo = Paths::workspace_daily(&h.workspace).join(format!("{today}.md"));
        assert!(in_repo.exists(), "跟着仓库走的那份账要写在 .ai-memory/daily/ 里");
        assert!(!h.paths.project_daily("proj-a").exists(), "同一个 id 不该在两处各记一遍");
        // 日志进了仓库目录，也就会被同一套 sync 看见，但不该被当成记录
        super::super::sync_all(&h.conn, &h.paths, Some(&h.workspace), Some("proj-a")).unwrap();
        assert_eq!(list_all(&h.conn).unwrap().len(), 1);
        teardown(&h);
    }

    #[test]
    fn ttl_sweep_archives_exactly_the_expired_one() {
        let h = harness();
        let mut stale = draft("这条临时结论早该过期了。");
        stale.ttl_days = Some(3);
        stale.updated_at = "2020-01-01T00:00:00+08:00".into();
        let stale_id = stale.id.clone();

        let mut fresh = draft("这条还在有效期内。");
        fresh.ttl_days = Some(3650);
        fresh.updated_at = now_rfc3339();
        let fresh_id = fresh.id.clone();

        seed(&h.conn, &h.paths, &stale);
        seed(&h.conn, &h.paths, &fresh);

        let report = maintain(&h.conn, &h.paths, None, &MemoryConfig::default()).unwrap();
        assert_eq!(report.archived, vec![stale_id.clone()], "只该扫掉过期那条");

        let listed = list_all(&h.conn).unwrap();
        let status_of = |id: &str| listed.iter().find(|item| item.record.id == id).unwrap().record.status;
        assert_eq!(status_of(&stale_id), MemoryStatus::Archived, "索引要跟着 Markdown 走");
        assert_eq!(status_of(&fresh_id), MemoryStatus::Active);

        let file_records = parse_records(&fs::read_to_string(h.paths.global_memory()).unwrap()).unwrap();
        assert_eq!(file_records.len(), 2, "归档不删文件：Markdown 才是真相源");
        assert_eq!(
            file_records.iter().find(|item| item.id == stale_id).unwrap().status,
            MemoryStatus::Archived
        );

        // 再扫一次不该有动作
        assert!(maintain(&h.conn, &h.paths, None, &MemoryConfig::default()).unwrap().archived.is_empty());
        teardown(&h);
    }

    #[test]
    fn unparsable_timestamps_are_left_alone() {
        let h = harness();
        let mut odd = draft("时间戳被人手写坏了的过期记录。");
        odd.ttl_days = Some(1);
        odd.updated_at = "上周三".into();
        seed(&h.conn, &h.paths, &odd);
        assert!(maintain(&h.conn, &h.paths, None, &MemoryConfig::default()).unwrap().archived.is_empty());
        teardown(&h);
    }

    #[test]
    fn over_budget_memory_md_is_flagged_not_truncated() {
        let h = harness();
        let mut config = MemoryConfig { global_limit_chars: 40, ..Default::default() };
        let long = draft(&"很长的一条长期记忆正文".repeat(8));
        append_record(&h.conn, &h.paths, None, &long).unwrap();
        let snapshot = fs::read_to_string(h.paths.global_memory()).unwrap();

        let over = over_budget(&h.paths, None, None, &config).unwrap();
        assert_eq!(over.len(), 1, "超限要报出来给 UI 提示蒸馏");
        assert_eq!(over[0].limit, 40);
        assert!(over[0].chars > 40);
        let shown = over[0].shown();
        assert!(shown.contains("global/MEMORY.md") && shown.contains("40"), "报给界面的是哪一份、超到多少：{shown}");
        assert_eq!(fs::read_to_string(h.paths.global_memory()).unwrap(), snapshot, "判定超限不许动文件");
        assert_eq!(list_all(&h.conn).unwrap().len(), 1, "内容还在，检索也还认");

        config.global_limit_chars = 100_000;
        assert!(over_budget(&h.paths, None, None, &config).unwrap().is_empty(), "没超就不该报警");
        teardown(&h);
    }

    /// 候选区自然衰减：过期且两道自动转正门槛都不过线的候选归档（正文不动，可逆）；
    /// 新鲜的、或有任何一项过线的，都留着——它们可能只是还没被用户看到
    #[test]
    fn stale_low_bar_candidates_archive_but_the_borderline_ones_stay() {
        let h = harness();
        let mut config = MemoryConfig::default();
        config.candidate_ttl_days = 30;

        let mut stale = draft("过期又双低的候选，早该有人来裁决它。");
        stale.status = MemoryStatus::Candidate;
        stale.confidence = 0.3;
        stale.importance = 1;
        stale.updated_at = "2020-01-01T00:00:00+08:00".into();
        let stale_id = stale.id.clone();
        append_record(&h.conn, &h.paths, None, &stale).unwrap();

        let mut fresh = draft("新鲜的候选，用户可能还没看到。");
        fresh.status = MemoryStatus::Candidate;
        fresh.confidence = 0.3;
        fresh.importance = 1;
        let fresh_id = fresh.id.clone();
        append_record(&h.conn, &h.paths, None, &fresh).unwrap();

        let mut borderline = draft("过期但置信度过线的候选，值得留下来等人点头。");
        borderline.status = MemoryStatus::Candidate;
        borderline.confidence = 0.95;
        borderline.importance = 4;
        borderline.updated_at = "2020-01-01T00:00:00+08:00".into();
        let borderline_id = borderline.id.clone();
        append_record(&h.conn, &h.paths, None, &borderline).unwrap();

        let report = maintain(&h.conn, &h.paths, None, &config).unwrap();
        assert_eq!(report.archived, vec![stale_id.clone()], "只有过期又双低的那条该归档");

        let status_of = |id: &str| {
            list_all(&h.conn)
                .unwrap()
                .into_iter()
                .find(|view| view.record.id == id)
                .unwrap()
                .record
                .status
        };
        assert_eq!(status_of(&stale_id), MemoryStatus::Archived);
        assert_eq!(status_of(&fresh_id), MemoryStatus::Candidate, "新鲜的候选不该被扫掉");
        assert_eq!(
            status_of(&borderline_id),
            MemoryStatus::Candidate,
            "置信度过线的候选不该被扫掉：用户可能正要点它"
        );
        teardown(&h);
    }

    #[test]
    fn only_dated_logs_become_distill_batches_and_newest_come_first() {
        let h = harness();
        let old_dir = h.paths.global_daily();
        fs::create_dir_all(&old_dir).unwrap();
        fs::write(old_dir.join("2020-01-01.md"), "- 09:00 追加 [fact/global] 很早的一条 (mem_a)\n").unwrap();
        fs::write(old_dir.join("2020-02-01.md"), "- 09:00 追加 [fact/global] 晚一点的一条 (mem_b)\n").unwrap();
        fs::write(old_dir.join("随手记.md"), "文件名没有日期，永远不算老\n").unwrap();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        fs::write(old_dir.join(format!("{today}.md")), "- 09:00 追加 [fact/global] 今天的一条 (mem_c)\n").unwrap();

        let config = MemoryConfig::default();
        let logs = eligible_logs(&h.paths, None, None, &config);
        assert_eq!(logs.len(), 2, "只认带日期且真到了天数的：{logs:?}");
        assert_eq!(logs[0].file_name().unwrap(), "2020-02-01.md", "新的先进提示词");

        let batch = gather(&h.conn, &h.paths, None, None, &config).unwrap();
        assert_eq!(batch.logs.len(), 2);
        assert!(batch.material.contains("晚一点的一条"));
        assert!(batch.material.contains("很早的一条"));
        assert!(!batch.material.contains("随手记"), "认不出日期的东西不该被消费");
        teardown(&h);
    }

    #[test]
    fn a_successful_distillation_lands_records_and_archives_the_logs() {
        let h = harness();
        let config = MemoryConfig::default();
        let daily = h.paths.global_daily();
        fs::create_dir_all(&daily).unwrap();
        let log = daily.join("2020-01-01.md");
        fs::write(&log, "- 09:00 追加 [preference/global] 用户说要先给结论 (mem_x)\n").unwrap();
        let batch = gather(&h.conn, &h.paths, None, None, &config).unwrap();
        assert_eq!(batch.logs.len(), 1);

        let raw = "---
id: model-gave-this
type: preference
scope: global
status: active
importance: 4
confidence: 0.95
stability: stable
source: user
created_at: 2019-01-01T00:00:00+08:00
updated_at: 2019-01-01T00:00:00+08:00
tags: [\"沟通风格\"]
supersedes: []
---

回答先给结论，再给理由。
";
        let summary = land(&h.conn, &h.paths, None, &config, None, raw, &batch).unwrap();
        assert_eq!(summary.distilled, 1);
        assert_eq!(summary.archived_logs, 1);
        assert!(!log.exists(), "消费过的日志该离开 daily");
        assert!(daily.parent().unwrap().join("archive").join("2020-01-01.md").exists(), "搬走不是删掉");

        let listed = list_all(&h.conn).unwrap();
        assert_eq!(listed.len(), 1, "蒸馏结果要经 append_record 落进真相源");
        assert_eq!(listed[0].record.source, MemorySource::Assistant, "蒸馏产物不是用户原话");
        assert!(
            !listed[0].record.created_at.starts_with("2019"),
            "什么时候知道的，以本机为准：{}",
            listed[0].record.created_at
        );
        assert_eq!(listed[0].record.updated_at, listed[0].record.created_at);
        assert_eq!(listed[0].record.tags, vec!["沟通风格".to_string()], "模型给的标签要留着");
        assert!(!listed[0].record.id.starts_with("model-"), "id 由本机决定，不然第二轮就撞车");
        teardown(&h);
    }

    #[test]
    fn a_failed_distillation_leaves_the_logs_where_they_are() {
        let h = harness();
        let config = MemoryConfig::default();
        let daily = h.paths.global_daily();
        fs::create_dir_all(&daily).unwrap();
        let log = daily.join("2020-01-01.md");
        fs::write(&log, "- 09:00 追加 [fact/global] 原始事实 (mem_x)\n").unwrap();
        let batch = gather(&h.conn, &h.paths, None, None, &config).unwrap();

        // 未闭合的 frontmatter：parse_records 会报错，此时一个字节都不许动
        let broken = "---\nid: only-one-side\ntype: fact\n";
        let outcome = land(&h.conn, &h.paths, None, &config, None, broken, &batch);
        assert!(outcome.is_err(), "读不懂的蒸馏输出必须报错");
        let error = outcome.unwrap_err();
        assert!(error.contains("日志一个字都没动"), "报错要说清日志还在：{error}");
        assert!(log.exists());
        assert!(!daily.parent().unwrap().join("archive").join("2020-01-01.md").exists());
        assert!(list_all(&h.conn).unwrap().is_empty());

        // 模型说"没有值得留的"：这不算失败，日志可以被消费掉
        let summary = land(&h.conn, &h.paths, None, &config, None, "无", &batch).unwrap();
        assert_eq!(summary.distilled, 0);
        assert_eq!(summary.archived_logs, 1);
        assert!(!log.exists());
        teardown(&h);
    }

    #[test]
    fn the_distillation_prompt_keeps_the_product_rules_and_the_field_contract() {
        let prompt = prompt_for("- 09:00 追加 [fact/global] 素材 (mem_x)");
        assert!(prompt.contains("将以下 30 天日志按主题蒸馏为不超过 20 条长期记忆"));
        assert!(prompt.contains("删除临时情绪、一次性任务、敏感信息"));
        assert!(prompt.contains("输出 Markdown frontmatter + 正文"));
        assert!(prompt.contains("supersedes: []"), "格式契约要写清，少一个字段整份读不了");
        assert!(prompt.contains("追加 [fact/global] 素材"));
    }

    /// 从真相源回读一条记录。索引里没有 supersedes 与自定义字段这两列，
    /// 所以判取代关系与冲突标记必须回文件，不能问 `list_all`
    fn stored(paths: &Paths, id: &str) -> MemoryRecord {
        parse_records(&read_text(&paths.global_memory()))
            .unwrap()
            .into_iter()
            .find(|record| record.id == id)
            .expect("记录该在文件里")
    }

    #[test]
    fn a_user_stated_memory_wins_against_a_later_inference() {
        let h = harness();
        let config = MemoryConfig::default();
        let old = draft("回答先给结论，不要长篇铺垫。");
        append_record(&h.conn, &h.paths, None, &old).unwrap();

        let mut incoming = MemoryRecord::draft(MemoryScope::Global, "回答先给结论，不要重复铺垫。");
        incoming.source = MemorySource::Inferred;
        incoming.confidence = 1.0;
        incoming.importance = 5;
        let band = similarity(&old.content, &incoming.content);
        assert!(
            band >= CONFLICT_FLOOR && band < config.dedupe_similarity,
            "这对样例的相似度 {band} 该落在冲突带 [{CONFLICT_FLOOR}, {})",
            config.dedupe_similarity
        );
        let new_id = incoming.id.clone();

        accept(&h.conn, &h.paths, None, &config, None, &[incoming], &unprovened()).unwrap();
        let listed = list_all(&h.conn).unwrap();
        let held = listed.iter().find(|item| item.record.id == old.id).expect("用户说过的还在");
        assert_eq!(held.record.status, MemoryStatus::Active, "用户明说的不能因为一次推断就被归档");
        let newcomer = listed.iter().find(|item| item.record.id == new_id).expect("新说法要留下");
        assert_eq!(newcomer.record.status, MemoryStatus::Candidate, "输了的推断只能进候选等人裁决");
        let written = stored(&h.paths, &new_id);
        assert_eq!(
            written.extra,
            vec![("conflicts_with".to_string(), old.id.clone())],
            "新那条要挂着对手，否则过一阵谁也想不清它在跟谁打架"
        );
        assert!(
            search(&h.conn, &config, "不要长篇铺垫", None).unwrap().iter().all(|hit| hit.id == old.id),
            "候选状态的推断不该被检索出来注入"
        );
        teardown(&h);
    }

    #[test]
    fn an_explicit_new_statement_supersedes_an_inferred_one() {
        let h = harness();
        let config = MemoryConfig::default();
        let mut old = draft("回答先给结论，不要长篇铺垫。");
        old.source = MemorySource::Inferred;
        old.confidence = 0.7;
        append_record(&h.conn, &h.paths, None, &old).unwrap();

        let incoming = draft("回答先给结论，不要重复铺垫。");
        accept(&h.conn, &h.paths, None, &config, None, std::slice::from_ref(&incoming), &unprovened()).unwrap();

        let listed = list_all(&h.conn).unwrap();
        let held = listed.iter().find(|item| item.record.id == old.id).unwrap();
        assert_eq!(held.record.status, MemoryStatus::Archived, "旧的推断被用户的说法取代了");
        let newcomer = listed.iter().find(|item| item.record.id == incoming.id).unwrap();
        assert_eq!(newcomer.record.status, MemoryStatus::Active);
        // 管理面板读的就是 list_all：取代关系要是回不来，这条规则等于没生效
        assert_eq!(newcomer.record.supersedes, vec![old.id.clone()]);
        assert_eq!(stored(&h.paths, &incoming.id).supersedes, vec![old.id.clone()], "取代关系要写进 frontmatter");
        assert!(
            search(&h.conn, &config, "不要长篇铺垫", None).unwrap().iter().all(|hit| hit.id == incoming.id),
            "被取代的那条不能再被检索出来，否则两条打架的说法同时注入"
        );

        // 取代关系也得能从索引里看出来
        let (old_id, new_id) = (old.id.clone(), incoming.id.clone());
        let links: Vec<(String, String, String)> = h.conn
            .prepare("SELECT from_id, to_id, kind FROM memory_links")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(links, vec![(new_id.clone(), old_id.clone(), "supersedes".to_string())]);

        // 索引整份丢掉后从 Markdown 重建，取代关系与状态都要回来
        drop(h.conn);
        let rebuilt = crate::memory::index::open(&h.paths.index_db()).unwrap();
        super::super::sync_all(&rebuilt, &h.paths, None, None).unwrap();
        let listed = list_all(&rebuilt).unwrap();
        assert_eq!(stored(&h.paths, &new_id).supersedes, vec![old_id.clone()], "重建不许弄丢取代关系");
        assert_eq!(
            listed.iter().find(|item| item.record.id == old_id).unwrap().record.status,
            MemoryStatus::Archived,
            "重建不许把归档过的又变回 active"
        );
        remove_tree(&h.paths.root);
        remove_tree(&h.workspace);
    }

    #[test]
    fn equal_authority_and_confidence_keeps_both_and_marks_them() {
        let rival = Hit {
            sensitivity: crate::memory::record::MemorySensitivity::Public,
            id: "mem_old".into(),
            path: "global/MEMORY.md".into(),
            kind: "preference".into(),
            scope: "global".into(),
            project_id: None,
            status: "active".into(),
            importance: 4,
            confidence: 0.9,
            stability: "stable".into(),
            source: "user".into(),
            created_at: "2026-01-01T00:00:00+08:00".into(),
            updated_at: "2026-01-01T00:00:00+08:00".into(),
            occurred_at: None,
            reinforced_at: None,
            ttl_days: None,
            tags: vec![],
            content: "回答先给结论，不要长篇铺垫。".into(),
            injections: 0,
            score: 0.0,
            why: String::new(),
        };
        let mut same = MemoryRecord::draft(MemoryScope::Global, "另一种说法。");
        same.confidence = 0.9;
        assert_eq!(decide(&rival, &same), Verdict::KeepBoth);
        same.confidence = 0.95;
        assert_eq!(decide(&rival, &same), Verdict::Supersede, "同作用域内新且高置信压过旧的");

        let mut project_rule = MemoryRecord::draft(MemoryScope::Project, "另一种说法。");
        project_rule.project_id = Some("p".into());
        project_rule.source = MemorySource::Assistant;
        project_rule.confidence = 1.0;
        assert_eq!(
            decide(&rival, &project_rule),
            Verdict::OldWins,
            "项目规则也压不过用户显式说过的话"
        );

        let mut inferred = MemoryRecord::draft(MemoryScope::Global, "另一种说法。");
        inferred.source = MemorySource::Inferred;
        inferred.confidence = 1.0;
        assert_eq!(decide(&rival, &inferred), Verdict::OldWins);

        let mut other_user = rival.clone();
        other_user.source = "assistant".into();
        other_user.scope = "project".into();
        assert_eq!(decide(&other_user, &same), Verdict::Supersede, "显式用户指令 > 助手写下的项目规则");
    }

    #[test]
    fn set_status_reports_nothing_when_the_target_is_already_there() {
        let h = harness();
        let record = draft("一条没有竞争者的记忆。");
        append_record(&h.conn, &h.paths, None, &record).unwrap();
        let snapshot = fs::read_to_string(h.paths.global_memory()).unwrap();
        assert_eq!(
            set_status(
                &h.conn,
                &h.paths,
                &h.paths.global_memory(),
                std::slice::from_ref(&"不存在的 id".to_string()),
                MemoryStatus::Archived,
            )
            .unwrap(),
            0
        );
        assert_eq!(fs::read_to_string(h.paths.global_memory()).unwrap(), snapshot, "没改着就不该写盘");
        teardown(&h);
    }

    // ------------------------------------------------------------ 冲突呈现与人选（P0 T03）

    /// 造一对"两条都 active、说法不同但讲同一件事"的记忆。
    /// 用 accept 而不是两次 append_record：撞上的那条会带上 conflicts_with 标记，
    /// 那正是索引里那条边的来源
    fn keep_both_pair(h: &Harness) -> (MemoryRecord, MemoryRecord, MemoryConfig) {
        let config = MemoryConfig::default();
        let first = draft("回答先给结论，不要长篇铺垫。");
        append_record(&h.conn, &h.paths, None, &first).unwrap();
        let mut rival = MemoryRecord::draft(MemoryScope::Global, "回答先给结论，不要长篇大论。");
        rival.confidence = first.confidence;
        assert_eq!(decide(&hit_of(&first), &rival), Verdict::KeepBoth, "这一对必须势均力敌才留在台面上");
        accept(&h.conn, &h.paths, None, &config, None, &[rival.clone()], &unprovened()).unwrap();
        (first, rival, config)
    }

    /// 把一条已落盘的记录还原成判定用的命中行。判定函数吃的是索引侧的形状，测试也照那条路走
    fn hit_of(record: &MemoryRecord) -> Hit {
        Hit {
            sensitivity: record.sensitivity,
            id: record.id.clone(),
            path: "global/MEMORY.md".into(),
            kind: record.kind.as_str().into(),
            scope: record.scope.as_str().into(),
            project_id: record.project_id.clone(),
            status: record.status.as_str().into(),
            importance: record.importance,
            confidence: record.confidence,
            stability: record.stability.as_str().into(),
            source: record.source.as_str().into(),
            created_at: record.created_at.clone(),
            updated_at: record.updated_at.clone(),
            occurred_at: record.occurred_at.clone(),
            reinforced_at: record.reinforced_at.clone(),
            ttl_days: record.ttl_days,
            tags: record.tags.clone(),
            content: record.content.clone(),
            injections: 0,
            score: 0.0,
            why: String::new(),
        }
    }

    #[test]
    fn two_active_rivals_show_up_as_one_pair_with_the_band_they_hit() {
        let h = harness();
        let (older, newer, config) = keep_both_pair(&h);
        let pairs = conflicts(&h.conn, &config).unwrap();
        assert_eq!(pairs.len(), 1, "一对冲突只该出现一次，不该正反各列一遍：{pairs:?}");
        let pair = &pairs[0];
        assert_eq!(pair.a.record.id, older.id, "a 该是场面上先站着的那条");
        assert_eq!(pair.b.record.id, newer.id);
        assert_eq!(pair.floor, CONFLICT_FLOOR);
        assert!(pair.similarity >= CONFLICT_FLOOR, "报出来的相似度要在带内：{}", pair.similarity);
        assert!(pair.why.contains("相似"), "why 要说清撞在哪个带：{}", pair.why);
        assert!(pair.why.contains("冲突标记"), "边的依据要写出来，让用户知道这不是现算猜的：{}", pair.why);
        assert_eq!(pair.recommendation, ConflictChoice::KeepBoth, "势均力敌时系统不许替用户选边");
        teardown(&h);
    }

    /// 没有边的两条也要看得见：用户手写的两句互相打脸的话同样会一起被注入
    #[test]
    fn hand_written_rivals_are_paired_without_any_marked_edge() {
        let h = harness();
        let config = MemoryConfig::default();
        append_record(&h.conn, &h.paths, None, &draft("部署用 docker compose 起。")).unwrap();
        let second = draft("部署用 podman compose 起。");
        append_record(&h.conn, &h.paths, None, &second).unwrap();

        let pairs = conflicts(&h.conn, &config).unwrap();
        assert_eq!(pairs.len(), 1, "两条都 active 又落在冲突带里，就该出现在视图里：{pairs:?}");
        assert!(pairs[0].why.contains("现算"), "依据要写明是现算的：{}", pairs[0].why);

        // 对照组：不像的两条不算冲突，别把不相干的东西塞进视图
        append_record(&h.conn, &h.paths, None, &draft("用户喜欢喝无糖乌龙茶。")).unwrap();
        let after = conflicts(&h.conn, &config).unwrap();
        assert_eq!(after.len(), 1, "多了一条不相干的记忆，冲突视图不该跟着变多：{after:?}");
        teardown(&h);
    }

    #[test]
    fn choosing_the_newer_side_archives_the_loser_and_keeps_both_bodies() {
        let h = harness();
        let (older, newer, config) = keep_both_pair(&h);
        let message = resolve(&h.conn, &h.paths, None, &older.id, &newer.id, ConflictChoice::NewerWins).unwrap();
        assert!(message.contains("已留下"), "回执要说清选了谁：{message}");

        let winner = stored(&h.paths, &newer.id);
        assert_eq!(winner.supersedes, vec![older.id.clone()], "胜者要带上取代边");
        assert!(winner.conflict_peers().is_empty(), "裁决过了还挂着冲突标记，视图就会一直 nag");
        assert_eq!(stored(&h.paths, &older.id).status, MemoryStatus::Archived);

        let on_disk = parse_records(&read_text(&h.paths.global_memory())).unwrap();
        assert_eq!(on_disk.len(), 2, "选完仍是 Markdown 事实：历史一条都不许删");
        assert!(on_disk.iter().any(|item| item.content == older.content), "败者的正文还要留在文件里");

        let listed = list_all(&h.conn).unwrap();
        assert_eq!(
            listed.iter().find(|item| item.record.id == older.id).unwrap().record.status,
            MemoryStatus::Archived,
            "索引要跟着 Markdown 走"
        );
        assert!(conflicts(&h.conn, &config).unwrap().is_empty(), "已经裁决过的一对不该再出现在视图里");
        assert!(
            search(&h.conn, &config, "不要长篇", None).unwrap().iter().all(|hit| hit.id == newer.id),
            "归档了的不能再被检索出来注入"
        );
        teardown(&h);
    }

    /// 选"都要"是把两条都留在台面上，不是把它们偷偷合并——所以只能动标记，
    /// 不能动正文、也不能动状态
    #[test]
    fn choosing_both_clears_the_marker_without_archiving_anything() {
        let h = harness();
        let (older, newer, config) = keep_both_pair(&h);
        let message = resolve(&h.conn, &h.paths, None, &older.id, &newer.id, ConflictChoice::KeepBoth).unwrap();
        assert!(message.contains("都留着"), "回执要说清什么都没归档：{message}");

        for id in [&older.id, &newer.id] {
            let kept = stored(&h.paths, id);
            assert_eq!(kept.status, MemoryStatus::Active, "选了都要却有记录被归档：{id}");
            assert!(kept.conflict_peers().is_empty(), "标记要清掉：{id}");
            assert!(kept.supersedes.is_empty(), "没选边就不该长出取代边：{id}");
        }
        assert!(conflicts(&h.conn, &config).unwrap().is_empty());
        teardown(&h);
    }

    #[test]
    fn choosing_older_wins_supersedes_in_the_right_direction() {
        let h = harness();
        let (older, newer, _config) = keep_both_pair(&h);
        resolve(&h.conn, &h.paths, None, &newer.id, &older.id, ConflictChoice::OlderWins).unwrap();
        assert_eq!(stored(&h.paths, &older.id).supersedes, vec![newer.id.clone()], "取代边必须从胜者指向败者");
        assert_eq!(stored(&h.paths, &newer.id).status, MemoryStatus::Archived);
        assert!(stored(&h.paths, &newer.id).supersedes.is_empty(), "败者不该反过来取代谁");
        teardown(&h);
    }

    #[test]
    fn archiving_both_moves_two_records_and_deletes_nothing() {
        let h = harness();
        let (older, newer, config) = keep_both_pair(&h);
        let message = resolve(&h.conn, &h.paths, None, &older.id, &newer.id, ConflictChoice::ArchiveBoth).unwrap();
        assert!(message.contains("2 条动了"), "两条都该被动到：{message}");
        for id in [&older.id, &newer.id] {
            assert_eq!(stored(&h.paths, id).status, MemoryStatus::Archived);
        }
        assert_eq!(parse_records(&read_text(&h.paths.global_memory())).unwrap().len(), 2, "归档不是删除");
        assert!(conflicts(&h.conn, &config).unwrap().is_empty());
        teardown(&h);
    }

    /// 选完的取代关系也得扛得住"删库重建"：边是 frontmatter 的投影，不是第二真相
    #[test]
    fn a_resolved_conflict_rebuilds_its_edges_from_markdown() {
        let h = harness();
        let (older, newer, _config) = keep_both_pair(&h);
        resolve(&h.conn, &h.paths, None, &older.id, &newer.id, ConflictChoice::NewerWins).unwrap();

        drop(h.conn);
        let rebuilt = crate::memory::index::open(&h.paths.index_db()).unwrap();
        super::super::sync_all(&rebuilt, &h.paths, None, None).unwrap();
        let rebuilt_edges = edges(&rebuilt);
        assert_eq!(
            rebuilt_edges,
            vec![(newer.id.clone(), older.id.clone(), "supersedes".to_string())],
            "边整片重建后必须逐条一致，否则索引就成了第二真相"
        );

        // 反向判据：把取代关系从真相源里删掉，重建后那条边必须跟着消失
        let mut on_disk = parse_records(&read_text(&h.paths.global_memory())).unwrap();
        let winner = on_disk.iter().position(|item| item.id == newer.id).unwrap();
        on_disk[winner].supersedes.clear();
        fs::write(h.paths.global_memory(), render_records(&on_disk)).unwrap();
        super::super::sync_all(&rebuilt, &h.paths, None, None).unwrap();
        assert!(edges(&rebuilt).is_empty(), "正文里没有的边，索引里不许自己活着");
        remove_tree(&h.paths.root);
        remove_tree(&h.workspace);
    }

    fn edges(conn: &Connection) -> Vec<(String, String, String)> {
        conn.prepare("SELECT from_id, to_id, kind FROM memory_links ORDER BY kind, from_id, to_id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    /// 冲突视图是给人看的，不是第二条注入通道：它算完之后，交给模型的那一段
    /// 必须一个字节都没变。新开一条"系统觉得冲突就塞进正文"的路，
    /// `/memory why` 与面板立刻失真
    #[test]
    fn resolving_and_listing_conflicts_never_touch_the_injected_body() {
        let h = harness();
        let (older, newer, config) = keep_both_pair(&h);
        let before = crate::memory::inject::build(&h.conn, &h.paths, &config, "结论", None, None)
            .unwrap()
            .expect("这一对有两条 active，总该注入点什么");
        assert!(!before.body.contains("冲突"), "注入正文里出现了裁决用的话术：{}", before.body);

        conflicts(&h.conn, &config).unwrap();
        crate::memory::index::source_of(&h.conn, &newer.id).unwrap();
        resolve(&h.conn, &h.paths, None, &older.id, &newer.id, ConflictChoice::NewerWins).unwrap();

        // 裁决之后注入的条目集变了（败者归档了），但格式与通道还是那一条：
        // 胜者那一行的写法与之前逐字相同，没有多出第二条通道的产物
        let after = crate::memory::inject::build(&h.conn, &h.paths, &config, "结论", None, None)
            .unwrap()
            .expect("胜者还在，注入不该整段消失");
        assert_eq!(after.items.len(), before.items.len() - 1, "只少了被归档的那一条");
        let line_of = |shot: &crate::memory::Injection, id: &str| {
            shot.items.iter().find(|item| item.id == id).map(|item| item.line.clone())
        };
        assert_eq!(
            line_of(&after, &newer.id),
            line_of(&before, &newer.id),
            "裁决过的胜者，注入行被改写了——它不该因为一次裁决就换一副样子"
        );
        assert_eq!(after.items.iter().filter(|item| item.id == older.id).count(), 0);
        teardown(&h);
    }

    /// 蒸馏的账要记在"反思"名下：用户只是按了按钮，说这句话的主体是系统自己。
    /// 记成 User 等于冒充显式指令，记成 Model 又说不出这是哪一轮说的
    #[test]
    fn distilled_memories_are_audited_as_reflection() {
        let h = harness();
        let config = MemoryConfig::default();
        let daily = h.paths.global_daily();
        fs::create_dir_all(&daily).unwrap();
        fs::write(daily.join("2020-01-01.md"), "- 09:00 追加 [preference/global] 用户说要先给结论 (mem_x)\n").unwrap();
        let batch = gather(&h.conn, &h.paths, None, None, &config).unwrap();
        let raw = "---
id: model-gave-this
type: preference
scope: global
status: active
importance: 4
confidence: 0.95
stability: stable
source: user
created_at: 2019-01-01T00:00:00+08:00
updated_at: 2019-01-01T00:00:00+08:00
tags: []
supersedes: []
---

回答先给结论，再给理由。
";
        let summary = land(&h.conn, &h.paths, None, &config, None, raw, &batch).unwrap();
        assert_eq!(summary.distilled, 1);
        let landed = list_all(&h.conn).unwrap()[0].record.id.clone();

        let root = h.paths.root.parent().unwrap();
        let lines = crate::audit::read_day(root, None);
        let mine: Vec<&String> = lines.iter().filter(|line| line.contains(&landed)).collect();
        assert!(!mine.is_empty(), "蒸馏落的那条要在账上：{landed}");
        assert!(
            mine.iter().any(|line| line.contains("\"actor\":\"reflection\"")),
            "蒸馏的写入要记在反思名下：{mine:?}"
        );
        assert!(
            !mine.iter().any(|line| line.contains("\"actor\":\"user\"")),
            "蒸馏产物不许记成用户说的：{mine:?}"
        );
        teardown(&h);
    }

    #[test]
    fn distillation_cannot_invent_its_own_provenance() {
        let h = harness();
        let config = MemoryConfig::default();
        let daily = h.paths.global_daily();
        fs::create_dir_all(&daily).unwrap();
        fs::write(daily.join("2020-01-01.md"), "- 09:00 追加 [fact/global] 素材 (mem_x)\n").unwrap();
        let batch = gather(&h.conn, &h.paths, None, None, &config).unwrap();
        // 模型在 frontmatter 里自称"这条来自某次对话"：那是编的，出处只能是日志
        let raw = "---
id: model-claims-an-origin
type: fact
scope: global
status: active
importance: 4
confidence: 0.95
stability: stable
source: user
created_at: 2019-01-01T00:00:00+08:00
updated_at: 2019-01-01T00:00:00+08:00
tags: []
supersedes: []
origin: {\"conversationId\":\"conv-fake\",\"entries\":[\"entry-1\"],\"extractedAt\":\"2019-01-01T00:00:00+08:00\"}
---

用户提过他每周跑一次备份。
";
        land(&h.conn, &h.paths, None, &config, None, raw, &batch).unwrap();
        let landed = &list_all(&h.conn).unwrap()[0];
        assert!(landed.record.origin.is_none(), "蒸馏不许自带出处：{:?}", landed.record.origin);
        teardown(&h);
    }

    #[test]
    fn the_distillation_prompt_lists_the_same_kinds_the_parser_accepts() {
        let prompt = prompt_for("素材");
        for kind in MemoryKind::ALL {
            assert!(prompt.contains(kind.as_str()), "蒸馏提示词漏了 {}", kind.as_str());
        }
    }
}

