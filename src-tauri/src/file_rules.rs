//! 文件安全规则表（design-security-center.md D2）。
//!
//! 一张"具体目标 → 动作"的表：每条规则是一个路径前缀，配读/写/删三档动作
//! （拒绝｜询问｜放行）。判定入口（`tool_runtime::rule`）先查它再落权限表：
//! **首条命中即停**（项目表在前、全局表在后），未命中的路径保持现行档不变——
//! 规则回答"这个具体目标放不放"，能力表面向"这一档放不放"，两层各答各的问题。
//!
//! 匹配是 Windows 口径的目录前缀语义：大小写不敏感、分隔符归一、剥 `\\?\` 前缀、
//! `%VAR%` 在**匹配时**展开（而不是存进去的时候）——改了环境变量不用重存规则。
//! 不做 glob：前缀语义已覆盖全部出厂预设，glob 的边界情况（`**` 该不该跨目录）
//! 是另一笔账，不在这里欠。
//!
//! 规则里的"放行"是用户的显式手笔（与审批里的"以后都允许"同责）：它能让
//! `%USERPROFILE%\.aws\` 这类**项目根之外**的路径从硬错变成可谈——那正是这张表
//! 存在的理由。规则拒绝的连问都不问；规则放行的仍要过沙箱的物理边界。

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::policy::{FileMode, Level};

/// 命中一条规则后的动作。与权限表的 [`Level`] 一一同映，判定入口拿它当档位用
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleAction {
    /// 直接拒，问都不问
    Deny,
    /// 必须有人点头。默认档：新规则先问再动
    #[default]
    Ask,
    /// 放行
    Allow,
}

impl RuleAction {
    fn level(self) -> Level {
        match self {
            RuleAction::Deny => Level::Deny,
            RuleAction::Ask => Level::Ask,
            RuleAction::Allow => Level::Allow,
        }
    }

    /// 严格的序：Deny > Ask > Allow。段间合并取最严用的是这个秩
    pub fn rank(self) -> u8 {
        match self {
            RuleAction::Deny => 2,
            RuleAction::Ask => 1,
            RuleAction::Allow => 0,
        }
    }
}

/// 一条文件规则：路径前缀 + 读/写/删三个动作。三格都有值（没有"跟随现状"）——
/// "未命中时保持现有策略"由**查不到这条规则**表达，不藏在格子里
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FileRule {
    /// 路径前缀（目录语义，覆盖子树）。可以用 `%USERPROFILE%` 这类环境变量
    pub pattern: String,
    pub read: RuleAction,
    pub write: RuleAction,
    pub delete: RuleAction,
}

impl FileRule {
    fn action_for(&self, mode: FileMode) -> RuleAction {
        match mode {
            FileMode::Read => self.read,
            FileMode::Write => self.write,
            FileMode::Delete => self.delete,
        }
    }
}

/// `%VAR%` 展开。变量按当前进程环境展开；有变量不认识或展开成空串 → `None`：
/// 这一条本轮作废（判定入口会把作废当没命中，宁可问人也不猜一个路径）
pub fn expand_pattern(pattern: &str) -> Option<String> {
    if !pattern.contains('%') {
        return Some(pattern.to_string());
    }
    let mut out = String::with_capacity(pattern.len());
    let mut rest = pattern;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let end = after.find('%')?; // 只有半个 %：写坏的规则，不当通配符猜
        let name = &after[..end];
        let value = if name.eq_ignore_ascii_case("userprofile") {
            std::env::var("USERPROFILE")
                .ok()
                .or_else(|| std::env::var("HOME").ok())
        } else {
            std::env::var(name).ok()
        };
        match value {
            Some(v) if !v.is_empty() => out.push_str(&v),
            _ => return None,
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    if out.contains('%') {
        return None;
    }
    Some(out)
}

/// 规则条目的规范化：展开环境变量 → 小写 → 分隔符归一 → 剥 `\\?\` → 统一带一个
/// 尾分隔符（前缀边界就长在这个分隔符上）。展开失败 = 该条目不参与匹配
fn normalize_entry(pattern: &str) -> Option<String> {
    let expanded = expand_pattern(pattern.trim())?;
    let stripped = expanded
        .strip_prefix(r"\\?\")
        .unwrap_or(&expanded)
        .to_ascii_lowercase()
        .replace('/', "\\");
    let trimmed = stripped.trim_end_matches('\\');
    if trimmed.is_empty() {
        return None;
    }
    Some(format!("{trimmed}\\"))
}

/// 判定目标的规范化。与条目同一套口径——差一步，"界面以为在拦、闸门以为不在"就有了
fn normalize_target(path: &Path) -> String {
    let text = path.to_string_lossy();
    let stripped = text
        .strip_prefix(r"\\?\")
        .unwrap_or(&text)
        .to_ascii_lowercase()
        .replace('/', "\\");
    format!("{}\\", stripped.trim_end_matches('\\'))
}

/// 首条命中即停。命中 → 该档动作折成的档位；全表未命中 → `None`
/// （调用方落回现行能力表，一字不改）
///
/// 边界由"两边都恰好带一个尾分隔符"保证：条目 `.ssh\` 的前缀命中要求目标里
/// 同一个位置也是分隔符，所以 `.sshx\` 永远不会被 `.ssh\` 吞进去
pub fn hit(rules: &[FileRule], mode: FileMode, path: &Path) -> Option<Level> {
    let target = normalize_target(path);
    if target.is_empty() {
        return None;
    }
    for rule in rules {
        let Some(entry) = normalize_entry(&rule.pattern) else {
            continue; // 展开不了的条目本轮作废，不挡后面的规则
        };
        if target.starts_with(&entry) {
            return Some(rule.action_for(mode).level());
        }
    }
    None
}

/// 入库校验（`is_known_key` 同一条纪律）：展开后必须是绝对路径——
/// 相对路径展开成什么全看当前目录，存一条"看起来在拦"的规则是最坏的那种坏
pub fn validate(rules: &[FileRule]) -> Result<(), String> {
    for (index, rule) in rules.iter().enumerate() {
        let pattern = rule.pattern.trim();
        let where_: String = format!("第 {} 条（{pattern}）", index + 1);
        if pattern.is_empty() {
            return Err(format!("{where_}：路径前缀是空的"));
        }
        let Some(expanded) = expand_pattern(pattern) else {
            return Err(format!(
                "{where_}：环境变量展开不了（变量不存在或展开成空）"
            ));
        };
        let stripped = expanded.strip_prefix(r"\\?\").unwrap_or(&expanded);
        if !Path::new(stripped).is_absolute() {
            return Err(format!("{where_}：展开后不是绝对路径（{expanded}）"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(pattern: &str, read: RuleAction, write: RuleAction, delete: RuleAction) -> FileRule {
        FileRule {
            pattern: pattern.into(),
            read,
            write,
            delete,
        }
    }

    #[test]
    fn first_hit_stops_and_later_rules_never_get_a_word_in() {
        let rules = vec![
            rule(
                r"%USERPROFILE%\.ssh",
                RuleAction::Ask,
                RuleAction::Deny,
                RuleAction::Deny,
            ),
            rule(
                r"%USERPROFILE%",
                RuleAction::Allow,
                RuleAction::Ask,
                RuleAction::Deny,
            ),
        ];
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap();
        let target = Path::new(&home).join(".ssh").join("id_ed25519");
        assert_eq!(
            hit(&rules, FileMode::Write, &target),
            Some(Level::Deny),
            "更具体的第一条说了算：第二条就算写着 Allow 也轮不到"
        );
        // 没进 .ssh 的：落到第二条
        assert_eq!(
            hit(&rules, FileMode::Read, &Path::new(&home).join("notes.txt")),
            Some(Level::Allow)
        );
    }

    #[test]
    fn matching_is_windows_loose_and_boundary_strict() {
        let rules = vec![rule(
            r"C:/Users/Someone/proj",
            RuleAction::Ask,
            RuleAction::Ask,
            RuleAction::Ask,
        )];
        // 大小写、分隔符方向、尾部分隔符都不影响
        assert_eq!(
            hit(
                &rules,
                FileMode::Read,
                Path::new(r"c:\Users\SOMEONE\proj\src\lib.rs")
            ),
            Some(Level::Ask)
        );
        // 差一个字符就是另一家：`.sshx` 不该被 `.ssh` 命中
        let ssh = vec![rule(
            r"%USERPROFILE%\.ssh",
            RuleAction::Deny,
            RuleAction::Deny,
            RuleAction::Deny,
        )];
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap();
        assert_eq!(
            hit(&ssh, FileMode::Read, &Path::new(&home).join(".sshx")),
            None
        );
        assert_eq!(
            hit(
                &ssh,
                FileMode::Read,
                &Path::new(&home).join(".ssh").join("config")
            ),
            Some(Level::Deny)
        );
    }

    #[test]
    fn unknown_variables_void_the_entry_instead_of_guessing() {
        let rules = vec![rule(
            r"%AGLAB_NOPE%\secret",
            RuleAction::Deny,
            RuleAction::Deny,
            RuleAction::Deny,
        )];
        assert_eq!(
            hit(&rules, FileMode::Read, Path::new("C:/anywhere/x")),
            None
        );
        // 半个 % 也一样
        let broken = vec![rule(
            r"C:\Users\50%\off",
            RuleAction::Deny,
            RuleAction::Deny,
            RuleAction::Deny,
        )];
        assert_eq!(
            hit(&broken, FileMode::Read, Path::new("C:/Users/50%/off")),
            None
        );
    }

    #[test]
    fn each_column_answers_its_own_operation() {
        let rules = vec![rule(
            r"C:\proj",
            RuleAction::Allow,
            RuleAction::Ask,
            RuleAction::Deny,
        )];
        let target = Path::new(r"C:\proj\file.txt");
        assert_eq!(hit(&rules, FileMode::Read, target), Some(Level::Allow));
        assert_eq!(hit(&rules, FileMode::Write, target), Some(Level::Ask));
        assert_eq!(hit(&rules, FileMode::Delete, target), Some(Level::Deny));
    }

    #[test]
    fn validation_rejects_relative_and_unexpandable_patterns() {
        assert!(validate(&[rule(
            "relative\\path",
            RuleAction::Ask,
            RuleAction::Ask,
            RuleAction::Ask
        )])
        .is_err());
        assert!(validate(&[rule(
            r"%AGLAB_NOPE%\x",
            RuleAction::Ask,
            RuleAction::Ask,
            RuleAction::Ask
        )])
        .is_err());
        assert!(validate(&[rule("", RuleAction::Ask, RuleAction::Ask, RuleAction::Ask)]).is_err());
        assert!(validate(&[rule(
            r"%USERPROFILE%\.ssh",
            RuleAction::Ask,
            RuleAction::Ask,
            RuleAction::Ask
        )])
        .is_ok());
    }
}
