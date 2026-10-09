//! 命令安全规则（design-security-center.md D4）：程序黑名单 + 命令前缀规则。
//!
//! 黑名单命中即拒——那批"动系统而不是动项目"的程序（wsl/reg/sc/schtasks/wmic）
//! 没有放行一说，能放行的只有前缀规则。求值顺序：黑名单 → 前缀规则 → 现行
//! `ExecScope` 判定；未命中一字不改地落回现行档。
//!
//! 匹配是拆段后的**保守**口径：命令行按 `&& || ; |` 与换行切段，每段独立过闸——
//! 引号里的分隔符会被误拆，误拆的方向是"多查几段"而不是"漏查"，对黑名单是
//! 更严而不是更松。真正的套娃（`cmd /c "…"` 的内层）一期不递归：内层解析不了
//! 的那部分，防线是沙箱与审批，这条边界写在界面的生效说明里，不装做管得到。
//!
//! 黑名单是**机器级**的（wsl.exe 在哪个项目里都不该由模型跑），所以只有全局一份，
//! 没有项目粒度；前缀规则有项目一份（哪个仓库自动放行哪条构建命令，是项目自己的事）。

use serde::{Deserialize, Serialize};

use crate::file_rules::RuleAction;

/// 一条前缀规则：命令行前缀 + 命中后的动作（询问｜放行）。拒绝不在这张表上——
/// 拒绝的语义由黑名单承担，两张表各管一个方向，不提供第四种组合
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CommandRule {
    /// 命令行前缀（如 `git push`）。按拆段后的整段做前缀匹配
    pub prefix: String,
    pub action: RuleAction,
}

/// 命令行切段：`&&`、`||`、`;`、`|` 与换行都是段界。不考虑引号——
/// 误拆让黑名单多查几段（更严），不会让任何一段被漏掉
pub fn segments(command: &str) -> Vec<String> {
    command
        .split(['\n', '\r', ';', '|', '&'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// 一段的程序名：第一个空白前的词，剥引号、剥 `.exe`、小写。
/// 黑名单条目走同一套归一——两边口径不同就一定有一边在装拦
pub fn program_name(segment: &str) -> String {
    let head = segment.split_whitespace().next().unwrap_or_default();
    head.trim_matches(|c| c == '"' || c == '\'')
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .replace(".exe", "")
}

fn normalize_entry(raw: &str) -> String {
    raw.trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .to_ascii_lowercase()
        .replace(".exe", "")
        .trim()
        .to_string()
}

fn normalize_prefix(raw: &str) -> String {
    raw.trim()
        .to_ascii_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// 黑名单命中：任何一段的程序名在名单里 → 这条命令整个被拒。
/// 条目按程序名整段相等匹配——`wsl` 不命中 `wslconfig`（差一个名字就是另一个程序）
pub fn blocklist_hit(blocklist: &[String], command: &str) -> Option<String> {
    let named: Vec<String> = blocklist
        .iter()
        .map(|e| normalize_entry(e))
        .filter(|e| !e.is_empty())
        .collect();
    if named.is_empty() {
        return None;
    }
    for segment in segments(command) {
        let program = program_name(&segment);
        if program.is_empty() {
            continue;
        }
        if named.iter().any(|entry| entry == &program) {
            return Some(program);
        }
    }
    None
}

/// 前缀规则的首条命中：任何一段以某条规则的前缀开头 → 用那条的动作。
/// 段间取**最严**——一次命令拆出五段，有一段要问就得问，不能被另一段的放行带过去
pub fn prefix_hit(rules: &[CommandRule], command: &str) -> Option<RuleAction> {
    if rules.is_empty() {
        return None;
    }
    let entries: Vec<(String, RuleAction)> = rules
        .iter()
        .map(|rule| (normalize_prefix(&rule.prefix), rule.action))
        .collect();
    let mut worst: Option<RuleAction> = None;
    for segment in segments(command) {
        let segment = normalize_prefix(&segment);
        if segment.is_empty() {
            continue;
        }
        if let Some((_, action)) = entries
            .iter()
            .find(|(prefix, _)| !prefix.is_empty() && segment.starts_with(prefix.as_str()))
        {
            worst = Some(match worst {
                None => *action,
                Some(held) if action.rank() > held.rank() => *action,
                Some(held) => held,
            });
        }
    }
    worst
}

/// 入库校验：黑名单只收程序名（带分隔符的条目永远命不中"第一段程序名"这个判据，
/// 存一条装饰不如当场拒掉）；前缀不能为空
pub fn validate(blocklist: &[String], rules: &[CommandRule]) -> Result<(), String> {
    for entry in blocklist {
        let name = normalize_entry(entry);
        if name.is_empty() {
            return Err(format!("黑名单条目「{entry}」是空的"));
        }
        if name.contains('\\') || name.contains('/') || name.contains(' ') {
            return Err(format!(
                "黑名单条目「{entry}」带了路径或空格：黑名单只收程序名（如 reg.exe，.exe 可省）"
            ));
        }
    }
    for rule in rules {
        if normalize_prefix(&rule.prefix).is_empty() {
            return Err("命令规则的前缀是空的".to_string());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocklist_hits_the_program_whatever_way_it_is_written() {
        let blocklist = vec!["reg.exe".to_string(), "wsl".to_string()];
        assert_eq!(
            blocklist_hit(&blocklist, "reg export HKLM /reg:x"),
            Some("reg".to_string())
        );
        assert_eq!(
            blocklist_hit(&blocklist, "REG.EXE export x"),
            Some("reg".to_string())
        );
        assert_eq!(
            blocklist_hit(&blocklist, "git status && reg.exe export"),
            Some("reg".to_string())
        );
        assert_eq!(blocklist_hit(&blocklist, "git status"), None);
        // `wsl` 不吞掉 `wslconfig`：差一个名字是另一个程序——要拦两个就写两条
        assert_eq!(blocklist_hit(&blocklist, "wslconfig /t ubuntu"), None);
    }

    #[test]
    fn segments_split_on_every_connector_and_the_blocklist_checks_each() {
        let blocklist = vec!["schtasks".to_string()];
        assert_eq!(
            blocklist_hit(&blocklist, "cargo build; schtasks /create /tn x"),
            Some("schtasks".to_string())
        );
        assert_eq!(
            blocklist_hit(&blocklist, "echo a | schtasks /create"),
            Some("schtasks".to_string())
        );
        assert_eq!(
            blocklist_hit(&blocklist, "cargo build\ncargo test"),
            None,
            "换行也是段界，但这两段都没点名黑名单"
        );
    }

    #[test]
    fn prefix_rules_match_whole_segments_and_take_the_strictest_across_them() {
        let rules = vec![
            CommandRule {
                prefix: "cargo test".into(),
                action: RuleAction::Allow,
            },
            CommandRule {
                prefix: "git push".into(),
                action: RuleAction::Ask,
            },
        ];
        assert_eq!(
            prefix_hit(&rules, "cargo test --lib"),
            Some(RuleAction::Allow)
        );
        assert_eq!(
            prefix_hit(&rules, "cargo check"),
            None,
            "前缀没盖住就不命中"
        );
        assert_eq!(
            prefix_hit(&rules, "git push origin main"),
            Some(RuleAction::Ask)
        );
        // 一段放行、一段要问：整条取最严
        assert_eq!(
            prefix_hit(&rules, "cargo test && git push origin main"),
            Some(RuleAction::Ask)
        );
    }

    #[test]
    fn quoted_separators_over_split_toward_stricter_not_looser() {
        // 引号里的 && 被误拆：第二段 `b"` 的程序名不在黑名单里，第一段照常匹配——
        // 误拆的代价是多查几段，不是漏查
        let blocklist = vec!["reg".to_string()];
        assert_eq!(
            blocklist_hit(&blocklist, "git commit -m \"done && reg export\""),
            Some("reg".to_string())
        );
    }

    #[test]
    fn validation_rejects_empty_and_path_shaped_blocklist_entries() {
        assert!(validate(&["".to_string()], &[]).is_err());
        assert!(validate(&["C:\\windows\\reg.exe".to_string()], &[]).is_err());
        assert!(validate(&["reg".to_string()], &[]).is_ok());
        assert!(validate(
            &[],
            &[CommandRule {
                prefix: "  ".into(),
                action: RuleAction::Allow
            }]
        )
        .is_err());
        assert!(validate(
            &[],
            &[CommandRule {
                prefix: "git push".into(),
                action: RuleAction::Ask
            }]
        )
        .is_ok());
    }
}
