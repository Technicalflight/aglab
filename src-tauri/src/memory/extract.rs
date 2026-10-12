//! 从对话里提候选记忆。
//!
//! 分工要清楚：模型只负责"说出可能是事实的东西"，能不能落盘、以什么身份落盘，
//! 由这里的门槛决定。把判断交给模型，就等于把用户的数据交给一次采样。

use std::collections::HashSet;
use std::path::Path;

use rusqlite::Connection;
use serde_json::Value;

use super::decay;
use super::govern::{decide, rival_of, set_status, Verdict};
use super::graph;
use super::origin::Origin;
use super::{
    audit_as, display_path, index, leaks_sensitive, locate, marked_do_not_store, now_rfc3339,
    resolve_file, rewrite_file, Hit, MemoryConfig, MemoryKind, MemoryRecord, MemoryScope,
    MemorySource, MemoryStatus, MemoryView, Paths, Stability,
};

/// 提取提示词。要求只输出 JSON，且明说敏感信息不要提——第二道闸在 accept 里，
/// 但先讲清楚能少一堆注定被丢的返回。
/// 类型清单取自 `MemoryKind::ALL`：提示词里列的取值必须和解析器认的一致，
/// 否则模型给出一个新类型就是整条静默丢弃
pub fn prompt_for(transcript: &str) -> String {
    format!(
        "从以下对话里提取值得长期记住的用户事实与偏好。\n\
         只输出一个 JSON 数组，不要解释、不要代码块围栏。每个元素形如：\n\
         {{\"type\":\"{types}\",\
         \"content\":\"一句话陈述，不要引用原文\",\
         \"keep_worthy\":true,\"reason\":\"为什么值得长期记住（一句话）\",\
         \"scope\":\"global|project\",\"importance\":1-5,\"confidence\":0-1,\
         \"stability\":\"stable|volatile\",\"ttl_days\":null,\
         \"occurred_at\":null,\"tags\":[\"标签\"],\"entities\":[\"张三|person\"]}}\n\
         规则：\n\
         1. 只提对未来对话有用、且不是一次性的内容；没有就返回 []。\n\
         2. 密码、密钥、token、证件号、银行卡、手机号这类敏感信息一律不要提。\n\
         3. 用户明确说过\"不要记\"的内容不要提。\n\
         4. content 用中文陈述句，不带\"用户说\"这类转述壳。\n\
         5. occurred_at 只在对话明确说了事情什么时候发生才填（YYYY-MM-DD）；\
         说不清就留 null——「记下来的时间」系统自己知道，不用你猜。\n\
         6. entities 填这条记录真正讲到的东西，最多 5 个，形如「名字|kind」，\
         kind 只能是 person / tool / project / file / concept 之一；\
         说不准 kind 就只写名字。不要为了凑数把顺带提到的词也填进去——\
         实体会被拿去召回，填错等于把不相干的东西递到模型眼前。\n\
         7. 宁缺毋滥：keep_worthy 是你自己的质量自评——拿不准是否值得长期\
         记住的，就填 false 并在 reason 里说明；填 false 的条目会被直接丢弃。\
         一次性任务、寒暄、中间过程都算不值得。凑数比漏记危害大。\n\n\
         对话：\n{transcript}",
        types = MemoryKind::contract()
    )
}

/// 这一批候选是**谁**提出来的、从哪儿长出来的。自动提取、蒸馏、反思三条路共用
/// `accept` 和同一道闸门，区别全写在这一个参数里：审计行的主体、落盘的出处。
pub struct Provenance {
    pub actor: crate::audit::Actor,
    /// 提取有对话可指；蒸馏与手记没有，就没有，而不是编一个
    pub origin: Option<Origin>,
    /// 这一批有没有资格直接成为在用记忆。反思没有：它的产物是推断，而 `merge_into`
    /// 会用新正文换掉旧正文、把置信度抬上去——让推断去改写用户说过的事实，
    /// 就是"系统自己给自己喂事实"最顺的那条路。所以这一路宁可留一条重复的候选让人裁决
    pub must_stay_candidate: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Accepted {
    pub stored: Vec<MemoryView>,
    pub merged: usize,
    pub dropped: usize,
    pub candidates: usize,
}

/// 大模型返回的东西要先经得起这些检查才成为记录：越界的数值掐到边界内，
/// 认不出的类型整条丢掉而不是猜一个
pub fn parse_candidates(raw: &str) -> Vec<MemoryRecord> {
    let Some(start) = raw.find('[') else {
        return Vec::new();
    };
    let Some(end) = raw.rfind(']') else {
        return Vec::new();
    };
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&raw[start..=end]) else {
        return Vec::new();
    };
    let mut records = Vec::new();
    for item in items {
        let Some(content) = item.get("content").and_then(Value::as_str).map(str::trim) else {
            continue;
        };
        if content.is_empty() {
            continue;
        }
        // 质量门第一道（模型自评）：keep_worthy=false 的直接丢。字段缺席按 true 处理——
        // 旧格式或模型漏填时宁可多进一道人工门槛，也不能把整批静默吞掉
        if item.get("keep_worthy").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        // 质量门第二道（工程规则）：一句话连 6 个字都凑不齐，几乎装不下任何
        // 可复用的事实——这类残条进候选区只会消耗用户的注意力
        if content.chars().count() < 6 {
            continue;
        }
        let kind: MemoryKind = match item.get("type").and_then(Value::as_str) {
            Some(name) => match name.parse() {
                Ok(kind) => kind,
                Err(_) => continue,
            },
            None => MemoryKind::Fact,
        };
        let scope: MemoryScope = item
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("global")
            .parse()
            .unwrap_or(MemoryScope::Global);
        let mut record = MemoryRecord::draft(scope, content);
        record.kind = kind;
        record.source = MemorySource::Inferred;
        record.confidence = item
            .get("confidence")
            .and_then(Value::as_f64)
            .unwrap_or(0.5)
            .clamp(0.0, 1.0);
        record.importance = item
            .get("importance")
            .and_then(Value::as_u64)
            .unwrap_or(3)
            .clamp(1, 5) as u32;
        record.stability = if item.get("stability").and_then(Value::as_str) == Some("volatile") {
            Stability::Volatile
        } else {
            Stability::Stable
        };
        record.ttl_days = item
            .get("ttl_days")
            .and_then(Value::as_u64)
            .map(|v| v as u32);
        // 「上周三」这种说法当没说：occurred_at 会被时间线拿去排序，也会让整条记录
        // 读不出来。认不出日期就当没人说过事情什么时候发生
        record.occurred_at = item
            .get("occurred_at")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|stamp| decay::parse_stamp(stamp).is_some())
            .map(String::from);
        record.tags = item
            .get("tags")
            .and_then(Value::as_array)
            .map(|tags| {
                tags.iter()
                    .filter_map(Value::as_str)
                    .map(|tag| tag.trim().to_string())
                    .filter(|tag| !tag.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        record.entities = item
            .get("entities")
            .and_then(Value::as_array)
            .map(|names| {
                names
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|name| name.trim().to_string())
                    .filter(|name| !name.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        records.push(record);
    }
    records
}

/// 粗相似度：切成索引同款 token 后取 Jaccard。它只用来判"这大概是同一条"，
/// 所以宁可把阈值调高（宁可重复，不可误并）
pub fn similarity(a: &str, b: &str) -> f64 {
    let tokens = |text: &str| -> HashSet<String> {
        index::segment_for_index(text)
            .split_whitespace()
            .map(String::from)
            .collect()
    };
    let left = tokens(a);
    let right = tokens(b);
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    left.intersection(&right).count() as f64 / left.union(&right).count() as f64
}

/// 落盘。四件事按顺序判：敏感的直接丢、像旧条目的合并更新、过不了门槛的进 candidate、
/// 其余作为 active 写入
pub fn accept(
    conn: &Connection,
    paths: &Paths,
    workspace: Option<&Path>,
    config: &MemoryConfig,
    project_id: Option<&str>,
    records: &[MemoryRecord],
    provenance: &Provenance,
) -> Result<Accepted, String> {
    // 关掉了记忆功能，这一层也得闭嘴。命令层已经拦过一次，但真正落笔的地方
    // 不该指望每个调用者都记得先问一句
    if !config.enabled {
        return Ok(Accepted::default());
    }
    let mut report = Accepted::default();
    for record in records {
        let mut record = record.clone();
        if record.scope == MemoryScope::Project {
            record.project_id = project_id.map(str::to_string);
        }
        // 出处跟着这一批走：同一场对话提出来的东西，来历是同一个
        if record.origin.is_none() {
            record.origin = provenance.origin.clone();
        }
        if leaks_sensitive(&record.content).is_some() || marked_do_not_store(&record.content) {
            report.dropped += 1;
            continue;
        }

        let hits = super::keep_relevant(
            super::search(conn, config, &record.content, project_id)?,
            project_id,
        );
        // 不许转正的那一路也不许合并：`merge_into` 会换掉旧正文并把置信度抬上去，
        // 一次推断就这样悄悄改写了一条用户说过的话
        let twin: Option<&Hit> = if provenance.must_stay_candidate {
            None
        } else {
            hits.iter()
                .filter(|hit| similarity(&record.content, &hit.content) >= config.dedupe_similarity)
                .max_by(|a, b| {
                    a.score
                        .partial_cmp(&b.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        };
        if let Some(twin) = twin {
            merge_into(conn, paths, workspace, twin, &record)?;
            // 合并同样改了真相源，只是没新增一条：账上要看得出是谁把它改成这样的
            audit_as(paths, provenance.actor, "merge", &twin.id)?;
            report.merged += 1;
            report.stored.push(MemoryView {
                path: twin.path.clone(),
                injections: twin.injections,
                record,
            });
            continue;
        }

        // 像但不一样：这是"用户改了主意"最常见的形状，交给取代判定，而不是并排存两条
        let rival = rival_of(&hits, &record.content, config);
        let mut shaky = provenance.must_stay_candidate
            || record.confidence < config.auto_accept_confidence
            || record.importance < config.auto_accept_importance;
        let mut takes_over = false;
        if let Some(rival) = &rival {
            match decide(rival, &record) {
                Verdict::Supersede => {
                    takes_over = true;
                    record.supersedes = vec![rival.id.clone()];
                }
                // 旧的更有话语权：新的这条只能进候选，等用户自己裁决。
                // 用户明说过的话，绝不能因为模型"后来推出一个不一样的"就被改掉
                Verdict::OldWins => shaky = true,
                // 谁也说不过谁：两条都留在 active，但必须标出对手，
                // 免得它冒充成一条无人质疑的独立事实
                Verdict::KeepBoth => {}
            }
            if !takes_over {
                // 标在记录自己身上（frontmatter 一行），索引同步时才长得出一条边
                record.mark_conflict(&rival.id);
            }
        }
        record.status = if shaky {
            MemoryStatus::Candidate
        } else {
            MemoryStatus::Active
        };
        let file =
            super::append_record_as(conn, paths, workspace, &record, Some(provenance.actor))?;
        if let (true, Some(rival)) = (takes_over, &rival) {
            // 先写下新的、再归档旧的：顺序反了的话，中间任何一步出错都会让这条记忆
            // 既不在检索结果里、也没有替代品顶上
            let rival_file = resolve_file(paths, workspace, &rival.path)?;
            set_status(
                conn,
                paths,
                &rival_file,
                std::slice::from_ref(&rival.id),
                MemoryStatus::Archived,
            )?;
        }
        if shaky {
            report.candidates += 1;
        }
        // 主体由这一批的来源决定，而不是事后靠内容猜：提取是模型提的，蒸馏是系统在
        // 反思自己的日志，两件事在账上必须分得开
        audit_as(
            paths,
            provenance.actor,
            if shaky {
                "candidate"
            } else if takes_over {
                "supersede"
            } else {
                "extract"
            },
            &record.id,
        )?;
        report.stored.push(MemoryView {
            path: display_path(&paths.root, &file),
            injections: 0,
            record,
        });
    }
    Ok(report)
}

/// 合并=更新旧条目，而不是并排写两条几乎一样的。旧条目的 id 与创建时间留着，
/// 这样"什么时候开始知道这件事"不会因为说了两遍就变成刚才
fn merge_into(
    conn: &Connection,
    paths: &Paths,
    workspace: Option<&Path>,
    twin: &Hit,
    incoming: &MemoryRecord,
) -> Result<(), String> {
    let _guard = super::write_guard();
    let (file, position) = locate(conn, paths, workspace, &twin.id)?;
    let mut records = super::load_records_or_quarantine(paths, &file)?;
    let existing = &mut records[position];
    // 去重带没有话语权裁决（那是 0.55~0.92 冲突带的事），所以这里守住最基本的一条：
    // 用户亲口说的原话，不被模型转述的版本无声替换——相似度过线只说明"大概是同一件事"，
    // 不说明模型的说法更可信。反向（旧条是推断、这批里有用户原话）与推断之间互并，照旧覆盖
    let user_wrote_it_first =
        existing.source == MemorySource::User && incoming.source != MemorySource::User;
    if !user_wrote_it_first {
        existing.content = incoming.content.clone();
    }
    existing.importance = existing.importance.max(incoming.importance);
    existing.confidence = existing.confidence.max(incoming.confidence);
    for tag in &incoming.tags {
        if !existing.tags.iter().any(|held| held == tag) {
            existing.tags.push(tag.clone());
        }
    }
    // 实体并进来时不重排：`entities` 那行的写法一变，记录的哈希就变，
    // 索引会以为"这条被改过"，于是把每一条都重同步一遍
    for name in &incoming.entities {
        if !existing
            .entities
            .iter()
            .any(|held| graph::canonical_of(held) == graph::canonical_of(name))
        {
            existing.entities.push(name.clone());
        }
    }
    existing.updated_at = now_rfc3339();
    rewrite_file(conn, paths, &file, &records)
}

/// 最近若干轮转写成正文给模型看。倒序取再翻回来，system 行不进转写：
/// 那是我们注入的东西，让它回流成"用户的事实"就闭环了
pub fn transcript_of(messages: &[Value], turns: usize, per_message: usize) -> String {
    let mut out = String::new();
    for message in messages.iter().rev().take(turns).rev() {
        let role = match message.get("role").and_then(Value::as_str) {
            Some("user") => "用户",
            Some("assistant") => "助手",
            _ => continue,
        };
        let Some(content) = message.get("content").and_then(Value::as_str) else {
            continue;
        };
        let content: String = content.chars().take(per_message).collect();
        out.push_str(&format!("{role}：{content}\n"));
    }
    out
}

/// 工具循环的轮预算。两轮查证 + 一轮终稿是常态；给到 6 是给"多查几遍再下笔"留余地，
/// 没有预算的循环等于把话费交给模型的兴致
pub(super) const LOOP_MAX_ROUNDS: usize = 6;

/// 用户的 prose 够不够格花一次请求的阈值（字数）：「好」「嗯」「继续」这种应答
/// 加起来也提不出一条可复用的事实——以内就不值得一次服务商往返
pub(super) const MIN_USER_PROSE_CHARS: usize = 40;

/// 用户的 prose 够不够格花一次请求。阈值定 40 字：「好」「嗯」「继续」这种应答
/// 加起来也提不出一条可复用的事实——以内就不值得一次服务商往返。
/// 返回用户行的正文总字数（trim 后），调用方与阈值比
pub(super) fn user_prose_chars(rows: &[Value]) -> usize {
    rows.iter()
        .filter(|row| row.get("role").and_then(Value::as_str) == Some("user"))
        .filter_map(|row| row.get("content").and_then(Value::as_str))
        .map(|text| text.trim().chars().count())
        .sum()
}

/// 工具循环的系统提示。与 [`prompt_for`] 的分工：那份说"提取什么"，这份说
/// "怎么用工具把候选做准"。落盘闸门（敏感/合并/冲突）不在这层——
/// 循环的产物仍是 parse_candidates 的输入，闸门原样保留
pub(super) fn loop_system_prompt() -> String {
    "你是记忆提取器。任务：从对话转写里找出值得长期记住的用户事实与偏好，产出候选。\n\
     你只有两个工具，都是只读的：\n\
     - memory_search：按关键词检索**现有**记忆（返回 id、状态与正文）。\n\
     - memory_read：读一个记忆文件的完整内容（path 用检索结果里的相对路径）。\n\
     流程：先判断对话里有没有值得记的；拿不准是否与已有记忆重叠时，先 memory_search 查一遍。\n\
     如果用户是在更新或修正已有条目，把 content 写成与那条对齐的说法——落盘侧按相似度\n\
     合并进原条目，而不是另起一条；确认没有重叠就直接给终稿，不必为每条候选都查一遍。\n\
     最终答复必须是**只含一个 JSON 数组**的正文（不要代码块围栏、不要解释），\n\
     元素的字段与规则以用户消息里的要求为准。没有值得记的就输出 []。"
        .to_string()
}

/// 循环的受限工具声明。就两个，都只读——提案里"记忆目录内 Edit/Write/rm"**刻意不做**：
/// 直接写文件会绕过 accept 闸门（敏感检查/去重合并/冲突判定/候选门槛），
/// 在工具执行体里重做一遍就是第二份判据。「更新已有条目」的合法通路是
/// 合并（merge_into）：让模型能查到已有条目、把 content 写成对齐的说法，
/// 相似度过线自然走进合并——判断权仍在闸门手里
pub(super) fn loop_tool_declarations() -> Vec<Value> {
    vec![
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "memory_search",
                "description": "在长期记忆里按关键词检索现有记忆（带 id、状态与正文）。产出候选前先查一遍：与已有条目重叠时把 content 写成对齐的说法，落盘侧会合并而不是另起一条。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "检索词" }
                    },
                    "required": ["query"]
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "memory_read",
                "description": "读一个记忆文件的完整内容（frontmatter 里有 id、出处与状态）。path 用 memory_search 结果里的相对路径，只许在记忆目录内。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "记忆根目录内的相对路径" }
                    },
                    "required": ["path"]
                }
            }
        }),
    ]
}

/// 循环里一个工具调用的执行体。**边界全在这里收**：名字只认声明的两个；
/// 检索走与 memory_search 命令同一条路（同过滤同排序，不另造第二份口径）；
/// 读文件过 [`resolve_file` 的围栏]（O16-1：敏感段黑名单 + 越界拒绝），只读不写。
/// 任何失败都作为文本回给模型——它看到失败才有修正的依据
pub(super) fn loop_tool(
    conn: &Connection,
    paths: &Paths,
    config: &MemoryConfig,
    project_id: Option<&str>,
    name: &str,
    arguments: &str,
) -> String {
    let Ok(parsed) = serde_json::from_str::<Value>(arguments) else {
        return "参数不是合法 JSON。".into();
    };
    match name {
        "memory_search" => {
            let Some(query) = parsed.get("query").and_then(Value::as_str).map(str::trim) else {
                return "memory_search 缺少 query。".into();
            };
            if query.is_empty() {
                return "query 是空的。".into();
            }
            let hits = match super::search(conn, config, query, project_id) {
                Ok(hits) => super::keep_relevant(hits, project_id),
                Err(error) => return format!("检索失败：{error}"),
            };
            if hits.is_empty() {
                return "没有命中的记忆。".into();
            }
            let mut out = String::from("命中（id | 状态 | 相对路径 | 正文）：\n");
            for hit in hits.iter().take(8) {
                out.push_str(&format!(
                    "- {} | {} | {} | {}\n",
                    hit.id, hit.status, hit.path, hit.content
                ));
            }
            out
        }
        "memory_read" => {
            let Some(shown) = parsed.get("path").and_then(Value::as_str).map(str::trim) else {
                return "memory_read 缺少 path。".into();
            };
            let file = match super::resolve_file(paths, None, shown) {
                Ok(file) => file,
                Err(error) => return format!("路径被拒绝：{error}"),
            };
            match std::fs::read_to_string(&file) {
                Ok(text) => {
                    // 全文够模型看清 frontmatter 与正文；超长就头尾都给，别静默给半截
                    const LIMIT: usize = 8000;
                    let count = text.chars().count();
                    if count <= LIMIT {
                        text
                    } else {
                        let head: String = text.chars().take(LIMIT * 3 / 4).collect();
                        let tail: String = text.chars().skip(count - LIMIT / 8).collect();
                        format!(
                            "{head}\n\n……（中间省略约 {} 字符）……\n\n{tail}",
                            count - LIMIT
                        )
                    }
                }
                Err(error) => format!("读不了 {}：{error}", file.display()),
            }
        }
        other => format!("这个循环只认 memory_search 与 memory_read，不认 {other}。"),
    }
}

/// 这次对话已经长出过记忆了吗（自动提取的廉价跳过①）。判据是**记忆自己的出处**：
/// 索引里任何一条的 origin.entries 提到本批最后一条日志行的 id，就说明这批消息的
/// 产出已经被提取落过盘——再采样一遍只是把同样的内容再买一次。origin 在索引里是
/// 一行紧凑 JSON（数组元素带引号），`instr` 对 `"id"` 精确匹配；id 里不该有引号，
/// 万一有也只会漏报（多跑一次提取，安全方向）。写入路径同步索引（append_record_as
/// → sync_file），调用方不必先做全库同步
pub(super) fn conversation_already_extracted(conn: &Connection, entry_id: &str) -> bool {
    let id = entry_id.trim();
    if id.is_empty() {
        return false;
    }
    let needle = format!("\"{id}\"");
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM memories WHERE instr(origin, ?1))",
        [&needle],
        |row| row.get::<_, bool>(0),
    )
    .unwrap_or(false)
}

/// 受限工具循环的组装（O15-3）：模型可以先查现有记忆再产候选——「更新已有条目
/// 而不是新建」由此被真正走到：查到了、content 写成对齐的说法，落盘侧按相似度
/// 自然合并。循环只编排往返，工具面收窄在 [`loop_tool`]（两个只读工具）；
/// 返回的正文仍是 parse_candidates 的输入，accept 闸门（敏感/合并/冲突/候选）原样保留
pub(super) fn via_tool_loop(
    app: &tauri::AppHandle,
    paths: &Paths,
    memory_config: &MemoryConfig,
    conn: &Connection,
    project_id: Option<&str>,
    transcript: &str,
) -> Result<String, String> {
    let mut thread = vec![
        serde_json::json!({ "role": "system", "content": loop_system_prompt() }),
        serde_json::json!({ "role": "user", "content": prompt_for(transcript) }),
    ];
    let declared = loop_tool_declarations();
    let mut executor = |name: &str, arguments: &str| {
        loop_tool(conn, paths, memory_config, project_id, name, arguments)
    };
    crate::chat::transcript::complete_with_tools(
        app,
        &crate::config::load(app),
        &mut thread,
        "memory",
        &declared,
        &mut executor,
        LOOP_MAX_ROUNDS,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::Actor;
    use crate::memory::{ensure_layout, list_all, parse_records, read_text, search};
    use crate::test_support::{remove_tree, temp_dir};

    fn harness() -> (Paths, Connection) {
        let paths = Paths::new(temp_dir("extract-root"));
        ensure_layout(&paths).unwrap();
        let conn = index::open(&paths.index_db()).unwrap();
        (paths, conn)
    }

    fn record(content: &str, confidence: f64, importance: u32) -> MemoryRecord {
        let mut record = MemoryRecord::draft(MemoryScope::Global, content);
        record.source = MemorySource::Inferred;
        record.confidence = confidence;
        record.importance = importance;
        record
    }

    /// 一次自动提取的来源：某场对话提的，带着那几条消息的 id
    fn from_conversation(id: &str) -> Provenance {
        Provenance {
            actor: Actor::Model,
            origin: Some(Origin {
                conversation_id: id.into(),
                entries: vec![format!("{id}-entry-1")],
                extracted_at: "2026-09-25T10:00:00+08:00".into(),
            }),
            must_stay_candidate: false,
        }
    }

    /// 文件里那一条，不是内存里那一条：真相源才是判据
    fn stored(paths: &Paths, id: &str) -> MemoryRecord {
        parse_records(&read_text(&paths.global_memory()))
            .unwrap()
            .into_iter()
            .find(|record| record.id == id)
            .expect("记录该在文件里")
    }

    #[test]
    fn parses_fenced_json_and_drops_entries_it_cannot_trust() {
        let raw = "```json\n[\
            {\"type\":\"preference\",\"content\":\"回答先给结论\",\"confidence\":0.9,\"importance\":4},\
            {\"type\":\"vibes\",\"content\":\"认不出类型的整条丢掉\"},\
            {\"type\":\"fact\",\"content\":\"   \"},\
            {\"type\":\"fact\",\"content\":\"越界数值该被掐回来\",\"confidence\":9,\"importance\":99}\
        ]\n```";
        let parsed = parse_candidates(raw);
        assert_eq!(parsed.len(), 2, "只该留下能认的两条：{parsed:?}");
        assert_eq!(parsed[1].confidence, 1.0);
        assert_eq!(parsed[1].importance, 5);
        assert_eq!(
            parsed[0].source,
            MemorySource::Inferred,
            "自动提的不能冒充用户说的"
        );
    }

    #[test]
    fn garbage_from_the_model_becomes_nothing_instead_of_an_error() {
        assert!(parse_candidates("我没有发现值得记录的事实。").is_empty());
        assert!(parse_candidates("[{\"content\":123}]").is_empty());
    }

    /// 质量门：模型自评 keep_worthy=false 的直接丢；一句话凑不齐 6 个字的丢；
    /// 字段缺席按 true 处理——旧格式或模型漏填时宁可多进一道人工门槛
    #[test]
    fn the_quality_gate_drops_self_rejected_and_thin_candidates() {
        let raw = "[\
            {\"type\":\"fact\",\"content\":\"用户偏好结论先行的回答方式\",\"keep_worthy\":true},\
            {\"type\":\"fact\",\"content\":\"用户正在调试一个报错\",\"keep_worthy\":false,\"reason\":\"一次性的排查过程\"},\
            {\"type\":\"fact\",\"content\":\"太短\"},\
            {\"type\":\"fact\",\"content\":\"没填自评字段的老格式照常保留\"}\
        ]";
        let parsed = parse_candidates(raw);
        let contents: Vec<&str> = parsed
            .iter()
            .map(|record| record.content.as_str())
            .collect();
        assert_eq!(
            contents,
            vec!["用户偏好结论先行的回答方式", "没填自评字段的老格式照常保留"],
            "自评不要的与太薄的都要拦在门外：{contents:?}"
        );
    }

    #[test]
    fn an_event_time_the_parser_cannot_read_is_dropped_not_stored_as_prose() {
        let raw = "[\
            {\"type\":\"event\",\"content\":\"用户上周搬家了\",\"occurred_at\":\"上周三\"},\
            {\"type\":\"event\",\"content\":\"用户 2026 年 8 月 1 日搬家了\",\"occurred_at\":\"2026-08-01\"}\
        ]";
        let parsed = parse_candidates(raw);
        assert_eq!(parsed.len(), 2);
        assert_eq!(
            parsed[0].occurred_at, None,
            "「上周三」进不了时间线，只能当没说"
        );
        assert_eq!(parsed[1].occurred_at.as_deref(), Some("2026-08-01"));
    }

    #[test]
    fn near_duplicates_merge_instead_of_stacking_up() {
        let (paths, conn) = harness();
        let config = MemoryConfig::default();
        let first = record("回答先给结论，再给理由。", 0.95, 4);
        accept(
            &conn,
            &paths,
            None,
            &config,
            None,
            std::slice::from_ref(&first),
            &from_conversation("c1"),
        )
        .unwrap();
        let again = record("回答先给结论，再给理由。", 0.95, 4);
        let report = accept(
            &conn,
            &paths,
            None,
            &config,
            None,
            &[again],
            &from_conversation("c2"),
        )
        .unwrap();
        assert_eq!(report.merged, 1);
        assert_eq!(
            list_all(&conn).unwrap().len(),
            1,
            "同一句话不该在真相源里躺两份"
        );
        remove_tree(&paths.root);
    }

    /// 合并改的是正文，不是来历：一条事实"来自哪次对话"是它自己的历史，
    /// 后来又被提一遍不该把它改写成第二次
    #[test]
    fn a_merge_keeps_the_first_conversation_as_the_provenance() {
        let (paths, conn) = harness();
        let config = MemoryConfig::default();
        let first = record("回答先给结论，再给理由。", 0.95, 4);
        let first_id = first.id.clone();
        accept(
            &conn,
            &paths,
            None,
            &config,
            None,
            &[first],
            &from_conversation("c1"),
        )
        .unwrap();
        let again = record("回答先给结论，再给理由。", 0.99, 4);
        accept(
            &conn,
            &paths,
            None,
            &config,
            None,
            &[again],
            &from_conversation("c2"),
        )
        .unwrap();

        let kept = stored(&paths, &first_id);
        assert_eq!(
            kept.origin.as_ref().unwrap().conversation_id,
            "c1",
            "出处被第二次提及覆盖了：{:?}",
            kept.origin
        );
        assert_eq!(kept.confidence, 0.99, "置信度该取两次里更高的那个");
        remove_tree(&paths.root);
    }

    /// 去重带没有话语权裁决（那是冲突带的事），所以 `merge_into` 自己守住底线：
    /// 用户亲口说的原话不被模型转述的版本无声替换——哪怕两条像到 0.92 以上。
    /// 反向（旧条是推断、这批里是用户原话）才轮到用户的措辞把正文换过来
    #[test]
    fn a_merge_never_rewrites_what_the_user_said_in_person() {
        let (paths, conn) = harness();
        let config = MemoryConfig::default();

        let mut spoken = record(
            "部署这个服务之前要先把数据库迁移跑完，再启动后端进程，最后检查健康检查接口，别手动改库表，日志里要能对上请求 id，报错必须带上当时那一步的上下文。",
            0.99,
            4,
        );
        spoken.source = MemorySource::User;
        let spoken_id = spoken.id.clone();
        let spoken_text = spoken.content.clone();
        crate::memory::append_record(&conn, &paths, None, &spoken).unwrap();

        // 只换最后一个字：相似度稳落去重带，测的才是"合并时正文以谁为准"
        let mut chars: Vec<char> = spoken_text.chars().collect();
        let last = chars.len() - 1;
        chars[last] = '哈';
        let parroted_text: String = chars.into_iter().collect();
        assert!(
            similarity(&spoken_text, &parroted_text) >= config.dedupe_similarity,
            "预处理：两条必须落在去重带（{:.3}），测的才是合并本身",
            similarity(&spoken_text, &parroted_text)
        );
        let parroted = record(&parroted_text, 0.6, 3);
        accept(
            &conn,
            &paths,
            None,
            &config,
            None,
            &[parroted],
            &from_conversation("c2"),
        )
        .unwrap();

        let kept = stored(&paths, &spoken_id);
        assert_eq!(kept.content, spoken_text, "用户的原话被模型转述盖掉了");
        remove_tree(&paths.root);

        // 反向对照：旧条是推断，用户后来亲口说了更准的版本——用户的措辞该赢。
        // 种子的置信度要过自动转正线：停在 candidate 的记录不进检索，测不到合并
        let (paths2, conn2) = harness();
        let inferred = record("部署前先把数据库迁移跑完，再启动后端进程，最后检查健康检查接口，不要手动改库表，日志里要能对上请求 id，报错必须带上当时那一步的上下文。", 0.95, 3);
        let inferred_id = inferred.id.clone();
        accept(
            &conn2,
            &paths2,
            None,
            &config,
            None,
            &[inferred],
            &from_conversation("c1"),
        )
        .unwrap();
        let mut mine = record("部署前先把数据库迁移跑完，再启动后端进程，最后检查健康检查接口，不要手动改库表，日志里要能对上请求 id，报错必须带上当时那一步的上下文哈。", 0.99, 4);
        mine.source = MemorySource::User;
        let mine_text = mine.content.clone();
        accept(
            &conn2,
            &paths2,
            None,
            &config,
            None,
            &[mine],
            &from_conversation("c2"),
        )
        .unwrap();
        let merged = stored(&paths2, &inferred_id);
        assert_eq!(
            merged.content, mine_text,
            "反向：用户亲口说的该把推断的正文换过来"
        );
        remove_tree(&paths2.root);
    }

    #[test]
    fn extracted_memories_carry_the_conversation_they_came_from() {
        let (paths, conn) = harness();
        let config = MemoryConfig::default();
        let item = record("这个项目用 pnpm 管理依赖。", 0.95, 4);
        let id = item.id.clone();
        let report = accept(
            &conn,
            &paths,
            None,
            &config,
            None,
            &[item],
            &from_conversation("conv-7"),
        )
        .unwrap();
        assert_eq!(report.stored.len(), 1);

        // 判据问的是文件，不是返回值：只有落在 frontmatter 里的出处才谈得上重建
        let landed = stored(&paths, &id);
        let origin = landed.origin.clone().expect("自动提取的记录必须带出处");
        assert_eq!(origin.conversation_id, "conv-7");
        assert_eq!(origin.entries, vec!["conv-7-entry-1".to_string()]);
        assert!(
            landed.to_markdown().contains("origin: {"),
            "origin 要在真相源里：{}",
            landed.to_markdown()
        );
        remove_tree(&paths.root);
    }

    #[test]
    fn extraction_without_a_conversation_id_is_refused_not_silently_unprovened() {
        let (paths, conn) = harness();
        let config = MemoryConfig::default();
        let leaked = Provenance {
            actor: Actor::Model,
            origin: Some(Origin {
                conversation_id: "  ".into(),
                entries: vec![],
                extracted_at: "2026-09-25T10:00:00+08:00".into(),
            }),
            must_stay_candidate: false,
        };
        let error = accept(
            &conn,
            &paths,
            None,
            &config,
            None,
            &[record("这个项目用 pnpm。", 0.95, 4)],
            &leaked,
        )
        .expect_err("漏传对话 id 必须报错，不能悄悄记成一条没有出处的记忆");
        assert!(error.contains("conversation_id"), "报错要指字段：{error}");
        assert!(
            list_all(&conn).unwrap().is_empty(),
            "被拦下的提取一个字都不该落盘"
        );

        // 对照组：没有出处这件事本身是合法的（手记、导入），所以 None 该正常写入
        accept(
            &conn,
            &paths,
            None,
            &config,
            None,
            &[record("这个项目用 pnpm。", 0.95, 4)],
            &Provenance {
                actor: Actor::User,
                origin: None,
                must_stay_candidate: false,
            },
        )
        .unwrap();
        assert_eq!(list_all(&conn).unwrap().len(), 1);
        remove_tree(&paths.root);
    }

    #[test]
    fn uncertain_inferences_wait_for_confirmation() {
        let (paths, conn) = harness();
        let config = MemoryConfig::default();
        let shaky = record("他可能在做一个 Tauri 客户端。", 0.4, 2);
        let report = accept(
            &conn,
            &paths,
            None,
            &config,
            None,
            &[shaky],
            &from_conversation("c1"),
        )
        .unwrap();
        assert_eq!(report.candidates, 1);
        assert_eq!(report.stored[0].record.status, MemoryStatus::Candidate);
        assert!(
            search(&conn, &config, "Tauri 客户端", None)
                .unwrap()
                .is_empty(),
            "没确认过的猜测不该被检索出来注入"
        );
        assert_eq!(
            list_all(&conn).unwrap().len(),
            1,
            "候选仍然要在管理界面里看得见"
        );
        remove_tree(&paths.root);
    }

    #[test]
    fn sensitive_and_refused_content_never_reaches_the_disk() {
        let (paths, conn) = harness();
        let config = MemoryConfig::default();
        let batch = [
            record("我的密码是 hunter2!", 1.0, 5),
            record("不要记这件事", 1.0, 5),
        ];
        let report = accept(
            &conn,
            &paths,
            None,
            &config,
            None,
            &batch,
            &from_conversation("c1"),
        )
        .unwrap();
        assert_eq!(report.dropped, 2);
        assert!(report.stored.is_empty());
        assert!(list_all(&conn).unwrap().is_empty());
        remove_tree(&paths.root);
    }

    #[test]
    fn similarity_is_1_for_identical_and_0_for_unrelated() {
        assert!((similarity("用 pnpm 管理依赖", "用 pnpm 管理依赖") - 1.0).abs() < 1e-9);
        assert!(similarity("今天天气不错", "数据库连接池") < 0.1);
        assert_eq!(similarity("", "任何东西"), 0.0);
    }

    #[test]
    fn the_transcript_takes_the_newest_turns_in_reading_order() {
        let messages = vec![
            serde_json::json!({"role": "user", "content": "第一问"}),
            serde_json::json!({"role": "assistant", "content": "第二答"}),
            serde_json::json!({"role": "user", "content": "第三问"}),
            serde_json::json!({"role": "system", "content": "不该出现的段"}),
        ];
        let text = transcript_of(&messages, 2, 100);
        assert!(
            text.contains("第三问") && !text.contains("第一问"),
            "只取最近 {text}"
        );
        assert!(!text.contains("不该出现"), "system 行不进转写：{text}");
        assert!(
            text.find("第二答") < text.find("第三问"),
            "取出来还得翻回阅读顺序：{text}"
        );
    }

    /// 提取要能说"这条讲到了谁"。这一层不做归一化也不校验 kind——那是 `graph` 的活儿，
    /// 两处各认一遍就会给出两套实体
    #[test]
    fn entities_come_through_with_their_stated_kind() {
        let records = parse_candidates(
            r#"[{"type":"fact","content":"张三负责数据库升级","entities":["张三|person","rusqlite|tool","  "]},{"type":"fact","content":"没有实体的那条"}]"#,
        );
        assert_eq!(records.len(), 2);
        assert_eq!(
            records[0].entities,
            vec!["张三|person".to_string(), "rusqlite|tool".to_string()],
            "空白的那条丢掉，其余原样带回"
        );
        assert!(records[1].entities.is_empty(), "没填就是空，不编一个出来");
    }

    #[test]
    fn the_prompt_names_the_json_shape_and_warns_about_secrets() {
        let prompt = prompt_for("用户：我偏好结论先行");
        assert!(prompt.contains("\"content\""));
        assert!(
            prompt.contains("一律不要提"),
            "敏感信息要在提示词里就挡一次：{prompt}"
        );
        assert!(prompt.contains("我偏好结论先行"));
        // 实体是图谱那一格的唯一生产者：提示词里没说，表就永远是空的
        assert!(
            prompt.contains("\"entities\""),
            "提示词要给出实体这一格：{prompt}"
        );
        assert!(prompt.contains("person / tool / project / file / concept"));
        assert!(
            prompt.contains("最多 5 个"),
            "要限住数量：填错实体会把不相干的东西递到模型眼前"
        );
    }

    /// 提示词里列的类型必须正好是解析器认的那些：两处各写一份的话，加一个 kind
    /// 就会出现"模型说得出来、我们读不懂、整条静默丢掉"
    #[test]
    fn the_prompt_offers_exactly_the_kinds_the_parser_accepts() {
        let prompt = prompt_for("对话");
        for kind in MemoryKind::ALL {
            assert!(
                prompt.contains(kind.as_str()),
                "提示词漏了 {}，模型就永远不会提它",
                kind.as_str()
            );
            assert!(
                kind.as_str().parse::<MemoryKind>().is_ok(),
                "提示词里的 {} 解析器读不出来",
                kind.as_str()
            );
        }
        assert!(
            "sentiment".parse::<MemoryKind>().is_err(),
            "清单外的取值必须被挡住"
        );
        let raw = format!(
            "[{{\"type\":\"sentiment\",\"content\":\"认不出的类型\"}},\
            {{\"type\":\"{}\",\"content\":\"清单里最后那个类型\"}}]",
            MemoryKind::ALL[6].as_str()
        );
        let parsed = parse_candidates(&raw);
        assert_eq!(
            parsed.len(),
            1,
            "清单外的整条丢掉、清单内的留下：{parsed:?}"
        );
        assert_eq!(parsed[0].kind, MemoryKind::ALL[6]);
    }

    /// 工具循环的边界声明与执行体必须对得上：声明只有两个只读工具，
    /// 执行体拒绝名单之外的一切——多认一个名字就多一条没过闸门的写路
    #[test]
    fn the_loop_declares_exactly_two_readonly_tools_and_the_executor_refuses_strangers() {
        let declared = loop_tool_declarations();
        let names: Vec<String> = declared
            .iter()
            .map(|item| {
                item["function"]["name"]
                    .as_str()
                    .expect("声明要有 function.name")
                    .to_string()
            })
            .collect();
        assert_eq!(
            names,
            vec!["memory_search".to_string(), "memory_read".to_string()],
            "循环的工具面就这两件，都是只读：{names:?}"
        );
        assert!(
            loop_system_prompt().contains("都是只读的"),
            "系统提示要把只读边界说明白"
        );

        let (paths, conn) = harness();
        let config = MemoryConfig::default();
        let seed = record("这个项目用 pnpm 管理依赖。", 0.95, 4);
        accept(
            &conn,
            &paths,
            None,
            &config,
            None,
            std::slice::from_ref(&seed),
            &from_conversation("c1"),
        )
        .unwrap();

        let hit = loop_tool(
            &conn,
            &paths,
            &config,
            None,
            "memory_search",
            r#"{"query": "pnpm"}"#,
        );
        assert!(hit.contains("pnpm"), "检索要能查到刚落的记忆：{hit}");
        assert!(
            hit.contains("相对路径"),
            "结果要带路径，模型才能接着读全文：{hit}"
        );

        let refusal = loop_tool(
            &conn,
            &paths,
            &config,
            None,
            "memory_write",
            r#"{"path": "global.md"}"#,
        );
        assert!(refusal.contains("只认"), "名单之外的名字必须拒：{refusal}");
        remove_tree(&paths.root);
    }

    /// memory_read 的围栏是 O16-1 那一道（判据只有一处）：根内的照读，
    /// 越界与敏感段一律拒——工具循环不因为"是后台"就开小门
    #[test]
    fn memory_read_stays_inside_the_memory_root() {
        let (paths, conn) = harness();
        let config = MemoryConfig::default();
        let seed = record("这个项目用 pnpm 管理依赖。", 0.95, 4);
        let view = accept(
            &conn,
            &paths,
            None,
            &config,
            None,
            std::slice::from_ref(&seed),
            &from_conversation("c1"),
        )
        .unwrap();
        let shown = view.stored[0].path.clone();

        let inside = loop_tool(
            &conn,
            &paths,
            &config,
            None,
            "memory_read",
            &format!(r#"{{"path": "{}"}}"#, shown),
        );
        assert!(
            inside.contains("pnpm") && inside.contains("id:"),
            "根内的记忆文件要读得到全文（含 frontmatter）：{inside}"
        );

        for escaped in [
            "../outside.md",
            r"C:\Windows\win.ini",
            ".git/hooks/pre-commit",
        ] {
            let refused = loop_tool(
                &conn,
                &paths,
                &config,
                None,
                "memory_read",
                &format!(r#"{{"path": "{}"}}"#, escaped.replace('\\', "\\\\")),
            );
            assert!(
                refused.contains("拒绝") || refused.contains("读不了"),
                "越界/敏感路径 {escaped} 必须被挡：{refused}"
            );
        }
        remove_tree(&paths.root);
    }

    /// 廉价跳过②的算术：用户行的 trim 后总字数。应答与寒暄到不了阈值，
    /// 正常陈述轻松过线
    #[test]
    fn thin_user_prose_is_counted_honestly() {
        let rows = vec![
            serde_json::json!({"role": "user", "content": "好"}),
            serde_json::json!({"role": "assistant", "content": "需要我继续吗"}),
            serde_json::json!({"role": "user", "content": "嗯"}),
            serde_json::json!({"role": "system", "content": "注入的不算用户的话"}),
            serde_json::json!({"role": "user", "content": "  "}),
        ];
        assert!(user_prose_chars(&rows) < MIN_USER_PROSE_CHARS);
        let rows = vec![
            serde_json::json!({"role": "user", "content": "我习惯先看结论再看论据，报告里要带数据来源，别用形容词堆砌，直接给我可以执行的下一步。"}),
            serde_json::json!({"role": "assistant", "content": "好的"}),
        ];
        assert!(user_prose_chars(&rows) >= MIN_USER_PROSE_CHARS);
    }

    /// 廉价跳过①（O15-3）：同一批消息的产出已经落过盘就不再采样第二遍。
    /// 判据走真实落盘流程：accept 写入的记录带 origin.entries，索引同步即查得到
    #[test]
    fn a_conversation_that_already_grew_a_memory_is_not_sampled_twice() {
        let (paths, conn) = harness();
        let config = MemoryConfig::default();
        let item = record("这个项目用 pnpm 管理依赖。", 0.95, 4);
        let report = accept(
            &conn,
            &paths,
            None,
            &config,
            None,
            std::slice::from_ref(&item),
            // from_conversation 会把 id 拼成 "<id>-entry-1"：出处里记下的那行
            // 就是 "conv-7-entry-1"
            &from_conversation("conv-7"),
        )
        .unwrap();
        assert_eq!(report.stored.len(), 1, "预处理：这一条要真的落下去");

        assert!(
            conversation_already_extracted(&conn, "conv-7-entry-1"),
            "本批最后一条日志行已在 origin.entries 里 → 该跳过"
        );
        assert!(
            !conversation_already_extracted(&conn, "conv-1-entry-99"),
            "新一批消息（新行 id）不该被旧出处误伤——后续对话还能继续提"
        );
        assert!(
            !conversation_already_extracted(&conn, "  "),
            "空 id 不该匹配到任何东西"
        );
        remove_tree(&paths.root);
    }
}
