//! 记忆图谱里"实体"这一格。
//!
//! 载体说清楚，免得"图谱"被理解成要装一个图库：实体与"哪条记录讲到哪个实体"只住在
//! 索引表里（`memory_entities` / `memory_entity_links`），**不是第二个 `.md` 真相**——
//! 删表能从正文、标签与 frontmatter 的 `entities` 原样重建出来。事件本身（事情何时发生）
//! 反过来只能在 `.md` 里：它不是能算出来的东西。
//!
//! 抽取的规则必须只看记录自己的字节。任何依赖当下时刻、依赖外部词典、依赖上一次索引状态的
//! 写法，都会让"重建"变成"第二次抽取"，而那正是双轨真相开始分叉的地方。

use super::record::MemoryRecord;

/// 一个实体属于哪一类。规则那一路只认得出 `file`/`concept` 两种——`person`、`tool`、
/// `project` 要靠提取时模型写明的标注（`entities: [张三|person]`）。
/// 规则不去猜"这个名字像不像人名"：猜错的 kind 会在界面上变成一个看起来很确定的错标签
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntityKind {
    Person,
    Tool,
    Project,
    File,
    Concept,
}

impl EntityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Person => "person",
            Self::Tool => "tool",
            Self::Project => "project",
            Self::File => "file",
            Self::Concept => "concept",
        }
    }

    /// 认不出来的写法退回 `concept`，不报错也不丢：丢一个实体等于丢一条召回路径，
    /// 而标错 kind 只是标签难看——两者里选较轻的那个
    pub fn parse(text: &str) -> Self {
        match text.trim().to_ascii_lowercase().as_str() {
            "person" => Self::Person,
            "tool" => Self::Tool,
            "project" => Self::Project,
            "file" => Self::File,
            _ => Self::Concept,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entity {
    /// 原样写出来的那个名字，界面上显示它
    pub name: String,
    /// 归一化之后的键：ASCII 名不分大小写，中文按原样
    pub canonical: String,
    pub kind: EntityKind,
}

/// 归一化只做两件确定性的事：ASCII 标识符折成小写、两端空白去掉。
/// 不做词形还原、不做同义词表——那些要一份"谁说了算"的词表，而词表一改，
/// 同一个库两次重建就会给出不同的边
pub fn canonical_of(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_ascii() {
        trimmed.to_ascii_lowercase()
    } else {
        trimmed.to_string()
    }
}

/// 这条记录讲到哪些实体：frontmatter 写明的，加上规则从正文与标签里认出来的。
/// 返回按 `canonical` 排好序——表的行序不稳，"重建后逐边一致"这条判据就无从谈起
pub fn entities_of(record: &MemoryRecord) -> Vec<Entity> {
    let mut found: Vec<Entity> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let mut add = |name: &str, kind: Option<EntityKind>| {
        let canonical = canonical_of(name);
        if canonical.is_empty() || seen.iter().any(|held| held == &canonical) {
            return;
        }
        // 同一个名字以两种 kind 出现时（frontmatter 说 person、形状看着像文件），
        // 先到先得：frontmatter 排在最前，所以是写明的那条说话
        seen.push(canonical.clone());
        found.push(Entity {
            name: name.trim().to_string(),
            canonical,
            kind: kind.unwrap_or_else(|| guess_kind(name)),
        });
    };

    for stated in &record.entities {
        match stated.split_once('|') {
            Some((name, kind)) => add(name, Some(EntityKind::parse(kind))),
            None => add(stated, None),
        }
    }
    for tag in &record.tags {
        add(tag, None);
    }
    for token in rule_tokens(&record.content) {
        add(&token, None);
    }

    found.sort_by(|a, b| a.canonical.cmp(&b.canonical));
    found
}

/// kind 没写明时只按形状认，两条规则都只看得到字符：
/// 带路径分隔符的是 `file`；点后面跟 1–5 个**字母**（`MEMORY.md`、`main.rs`）也是 `file`。
/// `1.5` 这种版本号不算——它的点在数字后面，标成文件就是给界面喂一个错标签
fn guess_kind(name: &str) -> EntityKind {
    let text = name.trim();
    if text.contains('/') || text.contains('\\') {
        return EntityKind::File;
    }
    match text.rsplit_once('.') {
        Some((stem, tail))
            if !stem.is_empty()
                && !tail.is_empty()
                && tail.len() <= 5
                && tail.chars().all(|ch| ch.is_ascii_alphabetic()) =>
        {
            EntityKind::File
        }
        _ => EntityKind::Concept,
    }
}

/// 规则那一路从正文里认出来的 token。两类，都是确定性的：
/// 1. 反引号括起来的原样串——markdown 里这就是"这东西是个名字"的写法；
/// 2. 含 `_ - . /` 之一、长度 ≥3、**且至少有一个字母**的 ASCII 片段（`allowed_tools`、
///    `src-tauri`、`MEMORY.md`）。要求有个字母是为了把版本号与日期挡在外面：
///    `1.5`、`2026-01-01` 长得像标识符，但它们不是名字。
///
/// 中文短语**不在这里认**：切词要词典，词典一换，重建出来的边就跟着换。中文的实体走
/// 标签与 frontmatter 那条路（`tags` 里的"沟通风格"本身就是一个实体）
fn rule_tokens(content: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    // 反引号里的内容单独收，不再参与第二段的字符扫描：`allowed_tools` 已经作为整体被认下了
    let mut outside = String::with_capacity(content.len());
    let mut chars = content.chars();
    while let Some(ch) = chars.next() {
        if ch != '`' {
            outside.push(ch);
            continue;
        }
        let mut inner = String::new();
        let mut closed = false;
        for next in chars.by_ref() {
            if next == '`' {
                closed = true;
                break;
            }
            inner.push(next);
        }
        let one_line = inner.trim();
        if !one_line.is_empty() && one_line.chars().count() <= 60 {
            out.push(one_line.to_string());
        }
        if !closed {
            // 没配对的单个反引号（散文里写 ` 而已）不该把后面的内容整个吞掉
            outside.push('`');
            outside.push_str(one_line);
        }
    }

    let mut word = String::new();
    let mut joined = false;
    for ch in outside.chars().chain(std::iter::once(' ')) {
        if ch.is_ascii_alphanumeric() {
            word.push(ch);
        } else if matches!(ch, '_' | '-' | '.' | '/' | '\\') {
            word.push(ch);
            joined = true;
        } else {
            if joined && word.chars().count() >= 3 && word.chars().any(|ch| ch.is_ascii_alphabetic())
            {
                out.push(word.clone());
            }
            word.clear();
            joined = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::record::{MemoryRecord, MemoryScope};

    fn record(content: &str, tags: &[&str], entities: &[&str]) -> MemoryRecord {
        let mut held = MemoryRecord::draft(MemoryScope::Global, content);
        held.tags = tags.iter().map(|tag| tag.to_string()).collect();
        held.entities = entities.iter().map(|name| name.to_string()).collect();
        held
    }

    fn names(record: &MemoryRecord) -> Vec<String> {
        entities_of(record).into_iter().map(|item| item.name).collect()
    }

    #[test]
    fn a_stated_kind_wins_and_an_unreadable_one_falls_back_to_concept() {
        let held = record(
            "张三负责 rusqlite 的升级",
            &[],
            &["张三|person", "rusqlite|tool", "沟通风格|说不清的 kinds"],
        );
        let found = entities_of(&held);
        let by_name: Vec<(&str, EntityKind)> = found
            .iter()
            .map(|item| (item.name.as_str(), item.kind))
            .collect();
        assert!(by_name.contains(&("张三", EntityKind::Person)));
        assert!(by_name.contains(&("rusqlite", EntityKind::Tool)), "写明优先于形状");
        assert!(
            by_name.contains(&("沟通风格", EntityKind::Concept)),
            "认不出的 kind 不报错也不丢，退回 concept：{by_name:?}"
        );
    }

    #[test]
    fn rules_pick_up_backticked_names_and_joined_identifier_tokens() {
        let held = record(
            "索引在 `MEMORY.md` 里，代码在 `src-tauri` 下，闸门叫 allowed_tools，版本 1.5 不算",
            &[],
            &[],
        );
        assert_eq!(
            names(&held),
            vec!["allowed_tools", "MEMORY.md", "src-tauri"],
            "反引号里的原样串与带连接符的 ASCII 片段都认，普通英文词一个都不认（行序按 canonical）"
        );
        let found = entities_of(&held);
        let by_name: Vec<(&str, EntityKind)> = found
            .iter()
            .map(|item| (item.name.as_str(), item.kind))
            .collect();
        assert!(by_name.contains(&("MEMORY.md", EntityKind::File)));
        assert!(by_name.contains(&("src-tauri", EntityKind::Concept)), "带连字符不等于文件");
        assert!(
            !names(&held).iter().any(|name| name == "1.5"),
            "版本号不该成为实体：{by_name:?}"
        );
    }

    #[test]
    fn chinese_phrases_come_from_tags_not_from_a_word_guesser() {
        let held = record("用户的沟通风格是结论先行，不喜欢铺垫", &["沟通风格", "结论先行"], &[]);
        assert_eq!(names(&held), vec!["沟通风格", "结论先行"]);
        // 正文里那一串中文没有被规则切成词：这是选择，不是漏
        assert!(
            !names(&held).iter().any(|name| name.contains("铺垫")),
            "规则不做中文分词，词典一换重建就会给出不同的边"
        );
    }

    #[test]
    fn ascii_names_fold_case_but_the_displayed_name_stays_as_written() {
        let held = record("同一个 `Allowed_Tools` 又写一遍 `allowed_tools`", &[], &[]);
        let found = entities_of(&held);
        assert_eq!(
            found.iter().filter(|item| item.canonical == "allowed_tools").count(),
            1,
            "归一化之后是同一个实体，不该因为大小写各站一行"
        );
        assert_eq!(
            found[0].name, "Allowed_Tools",
            "表里存的名字是第一次出现时的那个写法，界面读它"
        );
    }

    #[test]
    fn the_entity_set_is_a_function_of_the_record_bytes_alone() {
        let held = record(
            "读 `deliverables/design-memory-2.md`，看 memory_entities 表",
            &["图谱", "重建"],
            &["rusqlite|tool"],
        );
        assert_eq!(
            entities_of(&held),
            entities_of(&held),
            "同一份字节两次抽取必须一模一样，否则重建就不是重建"
        );
        let canonicals: Vec<String> = entities_of(&held)
            .iter()
            .map(|item| item.canonical.clone())
            .collect();
        let mut sorted = canonicals.clone();
        sorted.sort();
        assert_eq!(canonicals, sorted, "行序由 canonical 决定，与插入次序无关");
    }

    #[test]
    fn an_unbalanced_backtick_does_not_eat_the_rest_of_the_body() {
        // 散文里写了一个孤零零的反引号：后面的 `allowed_tools` 还得认得出来
        let held = record("这里有个未闭合的 ` 符号，后面是 `allowed_tools`", &[], &[]);
        assert!(names(&held).contains(&"allowed_tools".to_string()));
    }
}
