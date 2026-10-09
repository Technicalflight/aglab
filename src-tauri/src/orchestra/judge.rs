//! 质量检查与汇合。
//!
//! 两者都是**纯函数**：给同样的产出，给同样的结论。这一步做成有副作用的东西，
//! "为什么用了这一份答案"就再也问不出来了。
//!
//! 重试的策略在这里只有一条：不合格时**带着检查意见再跑一次**，最多一次。
//! 无上限的重试不是质量保证，是成本漏水。

use serde::{Deserialize, Serialize};

/// 一次产出的最低质量要求。
///
/// 它是**形状检查**而不是语义评判：编排器没法判断"这句话写得好不好"，
/// 但它能判断"该有的部分在不在、有没有把秘密抄进结论里"
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Check {
    pub min_chars: usize,
    pub must_contain: Vec<String>,
    /// 出现即判不合格。用来拦住"结论里贴了 token"这一类，而不是装饰
    pub forbid: Vec<String>,
}

impl Check {
    /// 一份计划默认的形状检查：**非空即可**。这一格是把 `run_node` 以前那句写死的
    /// `Check { min_chars: 1, ..Default::default() }` 搬到类型上，
    /// 所以接上"谁来自定义"这一步之前，任何既有计划的判定都不变
    pub fn plan_default() -> Self {
        Self {
            min_chars: 1,
            must_contain: Vec::new(),
            forbid: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail { why: String },
}

impl Verdict {
    pub fn is_pass(&self) -> bool {
        self == &Verdict::Pass
    }
}

impl Check {
    pub fn judge(&self, output: &str) -> Verdict {
        let mut problems: Vec<String> = Vec::new();
        let chars = output.trim().chars().count();
        if chars < self.min_chars {
            problems.push(format!("产出只有 {chars} 字，要求至少 {}", self.min_chars));
        }
        for needle in &self.must_contain {
            if !output.contains(needle.as_str()) {
                problems.push(format!("少了要求里的那一项「{needle}」"));
            }
        }
        for banned in &self.forbid {
            if output.contains(banned.as_str()) {
                problems.push(format!("出现了不该出现的内容「{}」", mask(banned)));
            }
        }
        if problems.is_empty() {
            Verdict::Pass
        } else {
            Verdict::Fail {
                why: problems.join("；"),
            }
        }
    }

    /// 重试时要对模型说的话。它必须带上"哪里不合格"，否则第二次只是第一次的重播
    pub fn feedback(&self, why: &str) -> String {
        format!("上一次的结果不合格：{why}。请只针对这些缺口重做，不要重复已经对的部分。")
    }
}

/// 被禁内容只回显前三位：错误文案会进话题上下文，等于把用户贴进来的东西再抄一遍
fn mask(text: &str) -> String {
    let head: String = text.chars().take(3).collect();
    if text.chars().count() > 3 {
        format!("{head}…")
    } else {
        head
    }
}

/// 一支贡献：哪个节点、用哪个 profile、产出了什么
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contribution {
    pub node: String,
    pub profile: String,
    pub text: String,
    /// 校验结论。失败的贡献**照样进汇合**，只是被标出来——
    /// 少了一支和静默少一支是两件不同的事
    pub verdict: Verdict,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Merge {
    /// 全部按定序拼接（扇出-汇聚默认）
    Concat,
    /// 同文计票，取多数
    Vote,
    /// 取第一个通过校验的那一支（Best-of-N）
    Best,
    /// 按 profile 优先级取，同优先级按节点 id
    ByProfilePriority { order: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merged {
    pub text: String,
    /// 被采纳的节点
    pub kept: Vec<String>,
    /// 被淘汰的节点（内容仍在 Trace 里，没有被丢掉）
    pub dropped: Vec<String>,
    /// 这一支没有产出（失败或被跳过），汇合结论里必须注明
    pub missing: Vec<String>,
}

/// 汇合。定序一律不依赖完成时刻：同一批贡献两次汇合给出同样的文本，
/// 否则"重跑一次得到不同答案"会变成无法复现的玄学
pub fn merge(strategy: &Merge, contributions: Vec<Contribution>, missing: Vec<String>) -> Merged {
    let mut ordered = contributions;
    match strategy {
        Merge::ByProfilePriority { order } => ordered.sort_by(|a, b| {
            rank(order, &a.profile)
                .cmp(&rank(order, &b.profile))
                .then_with(|| a.node.cmp(&b.node))
        }),
        _ => ordered.sort_by(|a, b| a.node.cmp(&b.node)),
    }

    match strategy {
        Merge::Concat => Merged {
            text: format!(
                "{}{}",
                ordered
                    .iter()
                    .map(|item| format!("【{}】{}\n{}", item.node, item.text, note(&item.verdict)))
                    .collect::<Vec<_>>()
                    .join("\n\n"),
                missing_note(&missing)
            ),
            kept: ordered.iter().map(|item| item.node.clone()).collect(),
            dropped: Vec::new(),
            missing,
        },
        Merge::Vote => {
            // 只给通过校验的计票，失败的那一支不能靠"写得长"赢下选举
            let mut tally: Vec<(String, usize, Vec<String>)> = Vec::new();
            for item in ordered.iter().filter(|item| item.verdict.is_pass()) {
                match tally.iter_mut().find(|(text, _, _)| *text == item.text) {
                    Some((_, count, nodes)) => {
                        *count += 1;
                        nodes.push(item.node.clone());
                    }
                    None => tally.push((item.text.clone(), 1, vec![item.node.clone()])),
                }
            }
            // 票数相同就按最早给出这个答案的节点 id 定序，绝不按到达顺序
            let winner =
                tally
                    .into_iter()
                    .max_by(|(_, a_count, a_nodes), (_, b_count, b_nodes)| {
                        a_count.cmp(b_count).then_with(|| b_nodes.cmp(a_nodes))
                    });
            match winner {
                Some((text, _, nodes)) => Merged {
                    text: format!("{text}{}", missing_note(&missing)),
                    kept: nodes,
                    dropped: ordered
                        .iter()
                        .filter(|item| !item.verdict.is_pass() || !text.contains(&item.text))
                        .map(|item| item.node.clone())
                        .collect(),
                    missing,
                },
                None => Merged {
                    text: "(没有任何一支通过校验)".to_string(),
                    kept: Vec::new(),
                    dropped: ordered.iter().map(|item| item.node.clone()).collect(),
                    missing,
                },
            }
        }
        Merge::Best => {
            let winner = ordered.iter().find(|item| item.verdict.is_pass());
            match winner {
                Some(item) => Merged {
                    text: format!("【{}】{}{}", item.node, item.text, missing_note(&missing)),
                    kept: vec![item.node.clone()],
                    dropped: ordered
                        .iter()
                        .filter(|other| other.node != item.node)
                        .map(|other| other.node.clone())
                        .collect(),
                    missing,
                },
                None => Merged {
                    text: "(没有一支通过校验)".to_string(),
                    kept: Vec::new(),
                    dropped: ordered.iter().map(|item| item.node.clone()).collect(),
                    missing,
                },
            }
        }
        Merge::ByProfilePriority { .. } => Merged {
            text: format!(
                "{}{}",
                ordered
                    .iter()
                    .map(|item| format!(
                        "【{}·{}】{}\n{}",
                        item.node,
                        item.profile,
                        item.text,
                        note(&item.verdict)
                    ))
                    .collect::<Vec<_>>()
                    .join("\n\n"),
                missing_note(&missing)
            ),
            kept: ordered.iter().map(|item| item.node.clone()).collect(),
            dropped: Vec::new(),
            missing,
        },
    }
}

fn rank(order: &[String], profile: &str) -> usize {
    order
        .iter()
        .position(|held| held == profile)
        .unwrap_or(order.len())
}

fn note(verdict: &Verdict) -> String {
    match verdict {
        Verdict::Pass => String::new(),
        Verdict::Fail { why } => format!("（未通过校验：{why}）"),
    }
}

/// 缺一支必须在文本里说出来。一份"看起来完整"的汇合结论比一句"少了一支"危险得多
fn missing_note(missing: &[String]) -> String {
    if missing.is_empty() {
        String::new()
    } else {
        format!("\n\n（这些分支没有产出：{}）", missing.join("、"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contribution(node: &str, text: &str, pass: bool) -> Contribution {
        Contribution {
            node: node.into(),
            profile: "worker".into(),
            text: text.into(),
            verdict: if pass {
                Verdict::Pass
            } else {
                Verdict::Fail {
                    why: "缺结论".into(),
                }
            },
        }
    }

    #[test]
    fn a_check_names_the_missing_part_instead_of_just_saying_short() {
        let check = Check {
            min_chars: 4,
            must_contain: vec!["结论".into()],
            forbid: vec![],
        };
        let verdict = check.judge("这一段还没有给出结论");
        assert!(
            verdict.is_pass(),
            "字数够、也含「结论」，不该被拦：{verdict:?}"
        );
        let verdict = check.judge("随便说两句");
        let Verdict::Fail { why } = verdict else {
            panic!("短且缺项必须不合格")
        };
        assert!(
            why.contains("结论") && why.contains("少"),
            "要说清缺了什么：{why}"
        );
    }

    #[test]
    fn a_forbidden_string_is_masked_when_it_is_reported() {
        let check = Check {
            forbid: vec!["sk-abcdefghijk".into()],
            ..Default::default()
        };
        let Verdict::Fail { why } = check.judge("答案是 sk-abcdefghijk") else {
            panic!("含禁串必须不合格")
        };
        assert!(
            !why.contains("sk-abcdefghijk"),
            "错误文案会回给模型，别把禁串再抄一遍：{why}"
        );
    }

    #[test]
    fn merging_is_ordered_by_node_id_not_by_arrival() {
        let first = merge(
            &Merge::Concat,
            vec![
                contribution("b", "第二支", true),
                contribution("a", "第一支", true),
            ],
            vec![],
        );
        let second = merge(
            &Merge::Concat,
            vec![
                contribution("a", "第一支", true),
                contribution("b", "第二支", true),
            ],
            vec![],
        );
        assert_eq!(
            first.text, second.text,
            "同一批贡献两次汇合给出不同文本，就等于不可复现"
        );
        assert!(first.text.find("第一支").unwrap() < first.text.find("第二支").unwrap());
    }

    #[test]
    fn a_missing_branch_is_stated_in_the_merged_text() {
        // 验收第 2 条的另一半：单支失败不影响整体，但结论里必须看得见少了一支
        let merged = merge(
            &Merge::Concat,
            vec![contribution("a", "还在", true)],
            vec!["b".into()],
        );
        assert!(
            merged.text.contains("b"),
            "缺一支要写进文本：{}",
            merged.text
        );
        assert_eq!(merged.missing, vec!["b".to_string()]);
    }

    #[test]
    fn votes_only_count_contributions_that_passed_and_ties_break_by_node_id() {
        let merged = merge(
            &Merge::Vote,
            vec![
                contribution("a", "同一份答案", true),
                contribution("c", "同一份答案", true),
                contribution("b", "另一份", true),
                contribution("d", "写得最长但没通过校验的答案", false),
            ],
            vec![],
        );
        assert_eq!(merged.text, "同一份答案");
        assert_eq!(merged.kept, vec!["a".to_string(), "c".to_string()]);
        assert!(
            merged.dropped.contains(&"b".to_string()),
            "被淘汰的那一支要报得出名字，内容仍在 Trace 里"
        );
        assert!(
            merged.dropped.contains(&"d".to_string()),
            "没过校验的不参与计票"
        );
    }

    #[test]
    fn best_of_n_takes_the_first_passing_candidate_in_node_order() {
        let merged = merge(
            &Merge::Best,
            vec![
                contribution("cand-2", "第二份", true),
                contribution("cand-0", "第一份", true),
                contribution("cand-1", "不合格的那份", false),
            ],
            vec![],
        );
        assert_eq!(
            merged.kept,
            vec!["cand-0".to_string()],
            "定序后才取第一支通过校验的，不看谁先写完"
        );
        assert_eq!(merged.dropped.len(), 2);
    }

    #[test]
    fn profile_priority_decides_the_order_and_unknown_profiles_go_last() {
        let order = vec!["senior".to_string(), "junior".to_string()];
        let mut items = vec![
            Contribution {
                node: "j".into(),
                profile: "junior".into(),
                text: "j".into(),
                verdict: Verdict::Pass,
            },
            Contribution {
                node: "x".into(),
                profile: "unknown".into(),
                text: "x".into(),
                verdict: Verdict::Pass,
            },
            Contribution {
                node: "s".into(),
                profile: "senior".into(),
                text: "s".into(),
                verdict: Verdict::Pass,
            },
        ];
        items.reverse();
        let merged = merge(&Merge::ByProfilePriority { order }, items, vec![]);
        assert!(
            merged.text.find("【s·senior】").unwrap() < merged.text.find("【j·junior】").unwrap()
        );
        assert!(
            merged.text.find("【j·junior】").unwrap() < merged.text.find("【x·unknown】").unwrap(),
            "没在优先级里的排最后，而不是插到中间"
        );
    }
}
