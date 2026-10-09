//! 相关性注入：拼出"这一轮该让模型知道的那几条"，并留下可复盘的为什么。
//!
//! 两条预算线（始终注入区 / 检索区）是这段代码存在的全部理由。没有它们，
//! "有记忆"很快就会变成"每轮都塞进几千 token 的旧话"，那既贵又会让模型跑题。

use std::collections::BTreeMap;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use super::record::injection_line_for;
use super::{index, now_rfc3339, standing_text, MemoryConfig, Paths};

const WHY_FILE: &str = "why.json";
/// 只留最近这么多场对话的注入记录。为什么是要回看的，但没人回看三个月前的那一轮
const WHY_KEEP: usize = 32;
/// 决策层相关性那一问的等待上限。注入挂在用户发消息的路上，等Decision不能
/// 比一次普通的服务商往返还贵——800ms 等不到就照原序走（决策的答案晚到作废）
const RELEVANCE_ASK_TIMEOUT_MS: u64 = 800;

/// 粗算 token。不追求和服务商一致，追求这条预算线可复算：CJK 一个字符算一个 token，
/// 其余四个字符算一个。配置里的 800 / 600 就是这个口径下的数字
pub fn estimate_tokens(text: &str) -> u32 {
    let mut cjk = 0u32;
    let mut other = 0u32;
    for ch in text.chars() {
        if index::is_cjk(ch) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk + other.div_ceil(4)
}

/// 掐到预算以内。二分找边界：逐字试的话，一段 800 token 的正文要重算上千遍
pub(crate) fn clip_to_budget(text: &str, budget: u32) -> String {
    if estimate_tokens(text) <= budget {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut low = 0usize;
    let mut high = chars.len();
    while low < high {
        let mid = (low + high).div_ceil(2);
        let probe: String = chars[..mid].iter().collect();
        if estimate_tokens(&probe) <= budget {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    chars[..low].iter().collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InjectedItem {
    pub id: String,
    pub path: String,
    /// 模型实际看到的那一行。存下来，面板才能显示"就这几个字"而不是重新拼一遍
    pub line: String,
    pub score: f64,
    pub why: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Injection {
    pub body: String,
    pub items: Vec<InjectedItem>,
    pub standing_tokens: u32,
    pub retrieve_tokens: u32,
    pub at: String,
    pub query: String,
}

/// 轻量意图通道：查询关键词 → 该优先的 kind。Qoder 的意图识别是模型级的，
/// 这里是规则级降级实现：零额外请求、零新依赖，只做同分近邻（±0.03）内的
/// 重排——匹配集合一个字不变，why 里的分数照旧成立，面板的解释也就依然成立。
/// 词表只收"提法里真的会出现的.signal"，宁缺毋滥：误加权比不加权难解释得多
fn intent_kind(query: &str) -> Option<&'static str> {
    let q = query.to_lowercase();
    let table: [(&str, &[&str]); 4] = [
        (
            "preference",
            &["偏好", "喜欢", "不喜欢", "习惯", "口味", "风格"],
        ),
        (
            "decision",
            &["决定", "定下来", "选型", "拍板", "方案定了", "决策"],
        ),
        ("rule", &["规范", "约定", "必须", "禁止", "流程", "规矩"]),
        ("event", &["上次", "那天", "那次", "昨天", "上周"]),
    ];
    for (kind, words) in table {
        if words.iter().any(|word| q.contains(word)) {
            return Some(kind);
        }
    }
    None
}

fn intent_bonus(kind: &str, intent: Option<&str>) -> f64 {
    match intent {
        Some(wanted) if kind == wanted => 0.03,
        _ => 0.0,
    }
}

/// 问决策层要这批候选的相关性分（§7.5）。答案不齐整（缺一个候选的分数都算不齐）
/// 就整批放弃，绝不带着半份分排序——那一问的形状与口径见 [`relevance_payload`]
fn decision_scores(app: &AppHandle, query: &str, hits: &[index::Hit]) -> Option<Vec<f64>> {
    let payload = relevance_payload(query, hits)?;
    let answer = crate::decision_bridge::ask(
        app,
        "scoreContextRelevance",
        payload,
        RELEVANCE_ASK_TIMEOUT_MS,
    );
    crate::decision_bridge::parse_scores(answer.as_ref(), hits.len())
}

/// 那一问的 payload：少于 2 条没有"排序"可言，不问；候选 id 是位置序（答案按下标
/// 对齐回来），正文只送每条前 200 字——打分用不着全文，也没必要让它们过桥进 state
/// （同 §7.5 的口径）。抽成纯函数是为了让形状有测试钉着
fn relevance_payload(query: &str, hits: &[index::Hit]) -> Option<serde_json::Value> {
    if hits.len() < 2 {
        return None;
    }
    let candidates: Vec<serde_json::Value> = hits
        .iter()
        .enumerate()
        .map(|(position, hit)| {
            serde_json::json!({
                "id": position,
                "text": hit.content.chars().take(200).collect::<String>(),
            })
        })
        .collect();
    Some(serde_json::json!({ "query": query, "candidates": candidates }))
}

/// 按决策分重排：稳定排序，同分的保持原相对序（检索分的既有解释不被搅动）；
/// 每条的 why 补上决策分——面板与 why 文件仍然解释得了"这条为什么排在这"
fn rerank_by_decision(hits: Vec<index::Hit>, scores: &[f64]) -> Vec<index::Hit> {
    let mut indexed: Vec<(usize, index::Hit)> = hits.into_iter().enumerate().collect();
    indexed.sort_by(|a, b| {
        scores[b.0]
            .partial_cmp(&scores[a.0])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    indexed
        .into_iter()
        .map(|(position, mut hit)| {
            hit.why = format!("{}；决策相关性 {:.1}", hit.why, scores[position]);
            hit
        })
        .collect()
}

/// 这一轮该注入什么。关掉记忆、关掉自动注入、或者什么都没有可注入时返回 None——
/// 调用方据此整段不写，而不是写一段空正文进日志。
/// `app` 是决策层相关性那一问的通道（§7.5 的生产接线）：None = 不问（单测/无从问）
pub fn build(
    conn: &Connection,
    paths: &Paths,
    config: &MemoryConfig,
    query: &str,
    project_id: Option<&str>,
    app: Option<&AppHandle>,
) -> Result<Option<Injection>, String> {
    if !config.enabled || !config.auto_inject {
        return Ok(None);
    }
    let standing = clip_to_budget(standing_text(paths).trim(), config.always_budget_tokens);
    let standing_tokens = estimate_tokens(&standing);

    // 两道闸都要过：先按项目隔离，再按用户划的红线（`secret` 永不出门）。
    // 顺序在预算之前——超线跳过是"放不下"，红线跳过是"不许放"，两件事不能混在一句日志里
    let hits = super::keep_relevant(super::search(conn, config, query, project_id)?, project_id);

    // 注意：这里**没有**把实体召回并进注入——试过，然后删了。实体派生自
    // 可检索文本（tags / 正文 / 反引号），查询点到的实体名必然让那条记录
    // 同时被 FTS 命中，"一跳补充"实测永远拿不到 seen 之外的新东西；
    // 真正有增量的两跳关联只进提示通道（recall_hints），那才是它的位置
    let hits = super::keep_injectable(hits);

    // 排序的两层：决策层相关性（§7.5 接线）答得上就按分排，候选集合一个字不变；
    // 答不上（开关关着 / 桥没人听 / 超时 / 答案畸形）退回轻量意图通道——
    // 那是它的规则级降级实现：零额外请求、零新依赖，只做同分近邻（±0.03）内的重排，
    // why 里的检索分数照旧成立，面板的解释也就依然成立
    let hits = match app.and_then(|handle| decision_scores(handle, query, &hits)) {
        Some(scores) => rerank_by_decision(hits, &scores),
        None => {
            let mut hits = hits;
            let intent = intent_kind(query);
            if intent.is_some() {
                hits.sort_by(|a, b| {
                    let ea = a.score + intent_bonus(&a.kind, intent);
                    let eb = b.score + intent_bonus(&b.kind, intent);
                    eb.partial_cmp(&ea).unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            hits
        }
    };

    let mut lines = String::new();
    let mut items: Vec<InjectedItem> = Vec::new();
    for hit in hits {
        // 来源写文件而不是写作用域：模型要能说出"这条住在哪个 .md 里"，
        // 用户才知道该去改哪一个。索引里存的就是相对记忆根目录的路径
        let line = injection_line_for(
            &hit.kind,
            &hit.path,
            &hit.updated_at,
            hit.occurred_at.as_deref(),
            hit.confidence,
            &hit.content,
        );
        let trial = format!("{lines}{line}\n");
        // 超线就跳过而不是停下：排序靠前的那条可能恰好很长，后面还有能放的
        if estimate_tokens(&trial) > config.retrieve_budget_tokens {
            continue;
        }
        lines = trial;
        items.push(InjectedItem {
            id: hit.id,
            path: hit.path,
            line,
            score: hit.score,
            why: hit.why,
            updated_at: hit.updated_at,
        });
    }

    let retrieve_tokens = estimate_tokens(&lines);
    let mut blocks = Vec::new();
    if !standing.is_empty() {
        blocks.push(standing.clone());
    }
    if !lines.is_empty() {
        // 段标记由调用方带进来（见 chat.rs 的 MEMORY_MARKER）。这里再写一句标签，
        // 模型就会看到两行都在给同一段起名字
        blocks.push(lines);
    }
    if blocks.is_empty() {
        return Ok(None);
    }
    // 用过就要记：使用频率是打分的一项，不记的话这一项永远是 0。
    // 纯重新生成（提法为空）不算用过——它没表达任何需求，照记的话
    // 同一批头部记忆会被空轮次无中生有地加热（强化章那边同一条规矩）
    if !query.trim().is_empty() {
        index::note_injection(
            conn,
            &items.iter().map(|item| item.id.clone()).collect::<Vec<_>>(),
            &now_rfc3339(),
        )?;
    }

    Ok(Some(Injection {
        body: blocks.join("\n\n"),
        items,
        standing_tokens,
        retrieve_tokens,
        at: now_rfc3339(),
        query: query.to_string(),
    }))
}

/// 记下这场对话最近一次的注入。`/memory why` 读的就是这份，它必须落在盘上：
/// 用户可能在回复出来之后隔十分钟才问"你刚才凭什么说这句"
pub fn remember(paths: &Paths, conversation_id: &str, injection: &Injection) -> Result<(), String> {
    let mut store = load_why(paths);
    store.insert(conversation_id.to_string(), injection.clone());
    if store.len() > WHY_KEEP {
        let mut by_age: Vec<(String, String)> = store
            .iter()
            .map(|(id, shot)| (id.clone(), shot.at.clone()))
            .collect();
        // at 是 RFC3339，字典序就是时间序
        by_age.sort_by(|a, b| a.1.cmp(&b.1));
        for (id, _) in by_age.iter().take(store.len() - WHY_KEEP) {
            store.remove(id);
        }
    }
    let text = serde_json::to_string_pretty(&store).map_err(|e| e.to_string())?;
    std::fs::write(paths.root.join(WHY_FILE), text).map_err(|e| format!("写注入记录失败：{e}"))
}

pub fn why_of(paths: &Paths, conversation_id: &str) -> Option<Injection> {
    load_why(paths).remove(conversation_id)
}

fn load_why(paths: &Paths) -> BTreeMap<String, Injection> {
    std::fs::read_to_string(paths.root.join(WHY_FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::record::{MemoryRecord, MemoryScope, MemorySensitivity};
    use crate::memory::{append_record, ensure_layout, index};
    use crate::test_support::{remove_tree, temp_dir};

    fn seeded(count: usize, content_prefix: &str) -> (Paths, Connection) {
        let paths = Paths::new(temp_dir("inject-root"));
        ensure_layout(&paths).unwrap();
        let conn = index::open(&paths.index_db()).unwrap();
        for index in 0..count {
            let record = MemoryRecord::draft(
                MemoryScope::Global,
                &format!("{content_prefix} 第 {index} 条 {}", "很长".repeat(40)),
            );
            append_record(&conn, &paths, None, &record).unwrap();
        }
        (paths, conn)
    }

    /// 决策分重排与 payload 测试用的命中行。判定面读的字段只有 content/why/score，
    /// 其余照索引侧的真实形状填上哑值——形状变了这里会红
    fn hit(id: &str, content: &str) -> index::Hit {
        index::Hit {
            id: id.into(),
            path: format!("global/{id}.md"),
            kind: "fact".into(),
            scope: "global".into(),
            project_id: None,
            status: "active".into(),
            importance: 3,
            confidence: 0.8,
            stability: "steady".into(),
            source: "manual".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            occurred_at: None,
            reinforced_at: None,
            ttl_days: None,
            tags: vec![],
            sensitivity: MemorySensitivity::Private,
            content: content.into(),
            injections: 0,
            score: 1.0,
            why: "检索命中".into(),
        }
    }

    /// 纯重新生成不算"用过"：空提法那几轮没有表达任何需求，
    /// 照记的话同一批头部记忆会被空轮次无中生有地加热
    #[test]
    fn an_empty_query_does_not_count_as_usage() {
        let (paths, conn) = seeded(3, "部署流水线相关的一条长记忆内容");
        let config = crate::memory::MemoryConfig::default();

        let shot = build(&conn, &paths, &config, "   ", None, None)
            .unwrap()
            .expect("空查询也有按重要度排的候选可注入");
        assert!(!shot.items.is_empty(), "预处理：空查询走的是重要度那一支");
        let counted: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(injections), 0) FROM memory_usage",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(counted, 0, "空查询不该给任何记忆加热");

        let shot = build(&conn, &paths, &config, "部署流水线", None, None)
            .unwrap()
            .expect("有提法该注入");
        assert!(!shot.items.is_empty());
        let counted: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(injections), 0) FROM memory_usage",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(counted > 0, "真实提法用过就要记：{counted}");
        remove_tree(&paths.root);
    }

    /// 意图词表只收真的会出现的 signal：误加权比不加权难解释得多
    #[test]
    fn intent_keywords_pick_the_kind_to_boost() {
        assert_eq!(intent_kind("用户喜欢什么样的回答风格"), Some("preference"));
        assert_eq!(intent_kind("部署方案定下来了"), Some("decision"));
        assert_eq!(intent_kind("上次的报错怎么修的"), Some("event"));
        assert_eq!(intent_kind("帮我写个爬虫"), None);
    }

    /// 决策分重排的三条边：按分降序、同分保持原相对序（检索分的既有解释不被搅动）、
    /// why 补上决策分——面板与 why 文件仍然解释得了"这条为什么排在这"
    #[test]
    fn decision_rerank_is_stable_descending_and_annotates_the_why() {
        let hits = vec![
            hit("a", "甲"),
            hit("b", "乙"),
            hit("c", "丙"),
            hit("d", "丁"),
        ];
        let ranked = rerank_by_decision(hits, &[0.5, 3.0, 0.5, 1.0]);
        let ids: Vec<&str> = ranked.iter().map(|item| item.id.as_str()).collect();
        assert_eq!(ids, ["b", "d", "a", "c"], "同分的 a 与 c 保持原相对序");
        assert!(
            ranked[0].why.ends_with("；决策相关性 3.0"),
            "{}",
            ranked[0].why
        );
        assert!(
            ranked[3].why.starts_with("检索命中"),
            "决策分是补注不是改写：{}",
            ranked[3].why
        );
    }

    /// 少于两条没有"排序"可言，一问都不该问出去
    #[test]
    fn fewer_than_two_candidates_means_no_ask_at_all() {
        assert!(relevance_payload("查询", &[hit("a", "甲")]).is_none());
        assert!(relevance_payload("查询", &[]).is_none());
    }

    /// 候选 id 是位置序（答案按下标对齐回来），正文只送前 200 字——
    /// 打分用不着全文，也没必要让整段内容过桥进 state
    #[test]
    fn the_relevance_ask_carries_position_ids_and_previews_only() {
        let hits = vec![hit("a", "甲乙丙丁"), hit("b", &"长".repeat(500))];
        let payload = relevance_payload("部署脚本怎么配", &hits).expect("两条就该问");
        assert_eq!(payload["query"], "部署脚本怎么配");
        let candidates = payload["candidates"].as_array().expect("候选是个数组");
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0]["id"], 0);
        assert_eq!(candidates[0]["text"], "甲乙丙丁");
        assert_eq!(candidates[1]["id"], 1);
        let preview = candidates[1]["text"].as_str().expect("预览是字符串");
        assert_eq!(preview.chars().count(), 200, "预览掐到 200 字");
    }

    #[test]
    fn counts_cjk_as_one_token_and_latin_as_a_quarter() {
        assert_eq!(estimate_tokens("记忆"), 2);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
    }

    #[test]
    fn clipping_never_goes_over_budget_and_keeps_a_prefix() {
        let text = "一二三四五六七八九十".repeat(50);
        let clipped = clip_to_budget(&text, 30);
        assert_eq!(estimate_tokens(&clipped), 30);
        assert!(text.starts_with(&clipped));
        assert_eq!(clip_to_budget("短", 30), "短", "没超线就不该动它");
    }

    #[test]
    fn retrieve_budget_caps_the_injected_lines() {
        let (paths, conn) = seeded(12, "依赖");
        let mut config = MemoryConfig::default();
        config.retrieve_budget_tokens = 120;
        config.search_limit = 50;
        let shot = build(&conn, &paths, &config, "依赖", Some("p1"), None)
            .unwrap()
            .expect("有命中就该有注入");
        assert!(
            shot.retrieve_tokens <= 120,
            "检索区超了预算：{}",
            shot.retrieve_tokens
        );
        assert!(shot.items.len() < 12, "12 条长记录不可能全塞进 120 token");
        assert!(
            shot.body.contains("来源: global"),
            "条目要带来源：{}",
            shot.body
        );
        remove_tree(&paths.root);
    }

    #[test]
    fn a_turn_with_nothing_to_say_injects_nothing() {
        let paths = Paths::new(temp_dir("inject-empty"));
        ensure_layout(&paths).unwrap();
        let conn = index::open(&paths.index_db()).unwrap();
        let config = MemoryConfig::default();
        assert!(
            build(&conn, &paths, &config, "没有这种东西", Some("p1"), None)
                .unwrap()
                .is_none(),
            "空正文不该写进日志"
        );

        let (paths, conn) = seeded(1, "依赖");
        let mut off = MemoryConfig::default();
        off.auto_inject = false;
        assert!(build(&conn, &paths, &off, "依赖", Some("p1"), None)
            .unwrap()
            .is_none());
        remove_tree(&paths.root);
    }

    #[test]
    fn standing_area_is_always_included_and_priced_in_its_own_budget() {
        let paths = Paths::new(temp_dir("inject-standing"));
        ensure_layout(&paths).unwrap();
        std::fs::write(paths.rules(), "# 硬规则\n永远先给结论。\n").unwrap();
        let conn = index::open(&paths.index_db()).unwrap();
        let shot = build(
            &conn,
            &paths,
            &MemoryConfig::default(),
            "无关的查询词",
            Some("p1"),
            None,
        )
        .unwrap()
        .expect("只有硬规则也算有东西注入");
        assert!(shot.body.contains("永远先给结论"));
        assert!(shot.items.is_empty(), "没命中就不该硬凑检索条目");
        assert!(shot.standing_tokens > 0);
        remove_tree(&paths.root);
    }

    #[test]
    fn why_survives_a_restart_and_keeps_only_the_newest_conversations() {
        let paths = Paths::new(temp_dir("inject-why"));
        ensure_layout(&paths).unwrap();
        for index in 0..(WHY_KEEP + 5) {
            let shot = Injection {
                body: format!("正文 {index}"),
                items: vec![],
                standing_tokens: 1,
                retrieve_tokens: 0,
                at: format!("2026-01-{:02}T00:00:00+08:00", index % 28 + 1),
                query: "q".into(),
            };
            remember(&paths, &format!("conv-{index}"), &shot).unwrap();
        }
        assert!(why_of(&paths, "conv-0").is_none(), "最旧的那场该被挤出去");
        let latest = why_of(&paths, &format!("conv-{}", WHY_KEEP + 4)).expect("最新的那场要在");
        assert_eq!(latest.body, format!("正文 {}", WHY_KEEP + 4));
        assert_eq!(
            why_of(&paths, &format!("conv-{}", WHY_KEEP + 4))
                .unwrap()
                .query,
            "q"
        );
        remove_tree(&paths.root);
    }
}
