//! 目标契约与它的证据。
//!
//! 目标从一句话变成三段（design-goal-mode.md §3.1）：**终态**（objective，留在
//! `session_mode::State` 里）、**判据**（每条要么绑一条跑得动的命令、要么显式标
//! "要人看"）、**约束**（推进期间不许动什么）。判据与约束整体是一份 [`Contract`]，
//! 挂在 `State` 上、首轮定形。
//!
//! **证据不进 State**：走一条只追加的 `goal_evidence` 条目（[`Evidence`]）。状态塞进
//! State 就等于每过一条判据都要重写整行（含全部契约文本），一行的字节随判据数量增长；
//! 只追加之后读侧按 `goal_id` 聚合出"哪几条闭合了"。证据带上**判据文本本身**
//! （[`Evidence::criterion_text`]）：与现行契约对不上 = 判据改过、证据作废——
//! 否则改一个字就继承旧证据，那是能骗过完成门的假绿。

use serde::{Deserialize, Serialize};

use crate::session::entry::EntryPayload;
use crate::session::log::SessionLog;

/// 证据条目在 `custom.custom_type` 里的名字
pub const EVIDENCE_TYPE: &str = "goal_evidence";

// ---- 契约的限（§3.1：写入口拒，读侧不裁）----
pub const MAX_OBJECTIVE_CHARS: usize = 2000;
pub const MAX_CRITERIA: usize = 12;
pub const MAX_CRITERION_CHARS: usize = 200;
pub const MAX_CONSTRAINTS: usize = 8;
pub const MAX_CONSTRAINT_CHARS: usize = 200;

/// 一条判据的类型。`Check` 绑一条跑得动的命令——收尾时运行时能自己复跑那一种；
/// `Judgment` 显式标"要人看"（文案说得清不清、截图对不对味），只接受上报。
/// 这两档的分别决定完成门怎么对待它（§4.3），所以要在人写下的那一刻就选好
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CriterionKind {
    Check { command: String },
    Judgment,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct Criterion {
    pub id: String,
    pub text: String,
    #[serde(flatten)]
    pub kind: CriterionKind,
}

impl Criterion {
    /// 判据说的是"跑命令"还是"要人看"。界面上的风险档与复验分档都从它来
    pub fn command(&self) -> Option<&str> {
        match &self.kind {
            CriterionKind::Check { command } => Some(command),
            CriterionKind::Judgment => None,
        }
    }
}

/// 完成契约：判据 + 约束。objective 不在这里——它是 `State.objective`，
/// 那一格早就存在，契约只补"怎么算真做到"与"不许动什么"两段
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct Contract {
    pub criteria: Vec<Criterion>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub constraints: Vec<String>,
}

/// 契约的写入口校验（§3.1 的限）。超限的落点是**报错说太满**，不是"少说两句"：
/// 契约是"人写下的规格"，被裁一半时模型的服从范围就变了，而那件事没有读数。
/// 弹框与 `session_goal_set` 拿的是同一份表——门口与落行说同一句话
pub fn validate(objective: &str, contract: &Contract) -> Result<(), String> {
    let objective = objective.trim();
    if objective.is_empty() {
        return Err("目标模式下得把目标写下来——没有目标，续跑就没有方向。".into());
    }
    if objective.chars().count() > MAX_OBJECTIVE_CHARS {
        return Err(format!(
            "目标写了 {} 字，上限 {MAX_OBJECTIVE_CHARS}。终态是一句话，不是一份文档。",
            objective.chars().count()
        ));
    }
    if contract.criteria.is_empty() {
        return Err("还差一条判据：写它怎么验证。没有判据，'做完了'就没有门可过。".into());
    }
    if contract.criteria.len() > MAX_CRITERIA {
        return Err(format!(
            "判据写了 {} 条，上限 {MAX_CRITERIA}。把同一件事的几条并成一条。",
            contract.criteria.len()
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for criterion in &contract.criteria {
        let text = criterion.text.trim();
        if text.is_empty() {
            return Err(format!("判据 {} 的正文是空的。", criterion.id));
        }
        if text.chars().count() > MAX_CRITERION_CHARS {
            return Err(format!(
                "判据「{}」写了 {} 字，单条上限 {MAX_CRITERION_CHARS}。",
                criterion.id,
                text.chars().count()
            ));
        }
        if !seen.insert(criterion.id.as_str()) {
            return Err(format!(
                "判据 id {} 重复了，每条要有一个自己的名字。",
                criterion.id
            ));
        }
        match &criterion.kind {
            CriterionKind::Check { command } => {
                if command.trim().is_empty() {
                    return Err(format!(
                        "判据「{}」标了「跑命令」，可命令是空的。",
                        criterion.id
                    ));
                }
            }
            CriterionKind::Judgment => {}
        }
    }
    if contract.constraints.len() > MAX_CONSTRAINTS {
        return Err(format!(
            "约束写了 {} 条，上限 {MAX_CONSTRAINTS}。",
            contract.constraints.len()
        ));
    }
    for constraint in &contract.constraints {
        let text = constraint.trim();
        if text.is_empty() {
            return Err("有一条约束的正文是空的。".into());
        }
        if text.chars().count() > MAX_CONSTRAINT_CHARS {
            return Err(format!(
                "约束「{}…」写了 {} 字，单条上限 {MAX_CONSTRAINT_CHARS}。",
                text.chars().take(12).collect::<String>(),
                text.chars().count()
            ));
        }
    }
    Ok(())
}

/// 证据的结论。fail 也是证据：它摆在那里，直到有一条更新的把它盖掉
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Fail,
}

/// 这份证据是谁验的。`runtime` = 运行时自己复跑过命令；`reported` = 模型上报的，
/// 界面上要明标「仅上报」（§4.3：非 Safe 的 check 不放自动执行的路）
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verified {
    Runtime,
    Reported,
}

/// 一条只追加的证据行。它说的是事实（"第 N 轮，这条判据有了这个结论、这段输出"），
/// 不是状态——状态由读侧聚合
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct Evidence {
    pub goal_id: String,
    pub criterion_id: String,
    /// 判据文本落进证据里：聚合时与现行契约对不上 = 判据改过，证据作废
    pub criterion_text: String,
    /// 哪一轮交来的。它是读数不是身份
    pub round: u32,
    pub verdict: Verdict,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    pub verified: Verified,
}

/// 一条判据现在的闭合状态。读侧聚合的唯一出处：完成门（§4.3）与界面判据清单（§5.4）都查它
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CriterionState {
    /// 没有证据，或证据已作废（判据文本变了）
    Open,
    /// 闭合了。两档分开：运行时复验过的与只接上报的，界面上长得不一样
    Passed {
        verified: Verified,
    },
    Failed,
}

/// 一条证据落成日志条目。只追加，从不改写
pub fn evidence_row(evidence: &Evidence) -> EntryPayload {
    EntryPayload::Custom {
        custom_type: EVIDENCE_TYPE.into(),
        data: serde_json::to_value(evidence).ok(),
    }
}

/// 判据清单的人话渲染，续跑行每一轮开头重锚用的就是它（§4.4 的 todo 那一格）。
/// □ = 还没闭合、■ = 复验过、◆ = 仅上报、✗ = 失败——同一套记号界面也在用，
/// 模型与人看到的是同一份进展。空的契约给空串
pub fn criteria_overview(contract: &Contract, rows: &[Evidence]) -> String {
    if contract.criteria.is_empty() {
        return String::new();
    }
    criterion_states(contract, rows)
        .iter()
        .zip(&contract.criteria)
        .map(|(state, criterion)| {
            let mark = match state {
                CriterionState::Open => "□",
                CriterionState::Passed {
                    verified: Verified::Runtime,
                } => "■",
                CriterionState::Passed {
                    verified: Verified::Reported,
                } => "◆",
                CriterionState::Failed => "✗",
            };
            match criterion.command() {
                Some(command) => format!(
                    "{mark} {} {}（跑命令：{command}）",
                    criterion.id, criterion.text
                ),
                None => format!("{mark} {} {}（要人看）", criterion.id, criterion.text),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 当前分支上的全部证据行，按日志顺序。真相仍在日志，这里只是读
pub fn evidence_in_effect(log: &SessionLog) -> Vec<Evidence> {
    let Ok(path) = log.path() else {
        return Vec::new();
    };
    path.iter()
        .filter_map(|entry| match entry.payload() {
            EntryPayload::Custom { custom_type, data } if custom_type == EVIDENCE_TYPE => {
                serde_json::from_value::<Evidence>(data.clone().unwrap_or_default()).ok()
            }
            _ => None,
        })
        .collect()
}

/// 把证据行折成每条判据的闭合状态（契约顺序）。同一条判据多份证据时**后写胜出**；
/// 文本对不上的（判据被改过）作废回 `Open`——不猜，也不继承
pub fn criterion_states(contract: &Contract, rows: &[Evidence]) -> Vec<CriterionState> {
    let mut latest: std::collections::HashMap<&str, &Evidence> = std::collections::HashMap::new();
    for row in rows {
        latest.insert(row.criterion_id.as_str(), row);
    }
    contract
        .criteria
        .iter()
        .map(|criterion| match latest.get(criterion.id.as_str()) {
            None => CriterionState::Open,
            // 文本对不上 = 判据改过：旧证据作废。作废不是失败，是"这一条还没验过"
            Some(row) if row.criterion_text != criterion.text => CriterionState::Open,
            Some(row) => match row.verdict {
                Verdict::Pass => CriterionState::Passed {
                    verified: row.verified,
                },
                Verdict::Fail => CriterionState::Failed,
            },
        })
        .collect()
}

/// 完成门的一次审计（design-goal-mode.md §4.3）。`unproven` 为空 = 过门；
/// 非空时每条说一句"哪条判据、凭什么没过"。审计只认**现行契约文本**下的证据：
/// 改过判据，旧证据作废，门不认
pub struct Audit {
    pub unproven: Vec<String>,
}

pub fn audit(contract: &Contract, rows: &[Evidence]) -> Audit {
    let states = criterion_states(contract, rows);
    let unproven = contract
        .criteria
        .iter()
        .zip(&states)
        .filter_map(|(criterion, state)| {
            let reason = match state {
                CriterionState::Passed { .. } => return None,
                CriterionState::Open => "还没有证据",
                CriterionState::Failed => "最新的证据是失败",
            };
            Some(format!(
                "· {}（{}）：{reason}",
                criterion.id, criterion.text
            ))
        })
        .collect();
    Audit { unproven }
}

/// 复跑排程：合并之后还没闭合的 `Check`，且命令被判为 `Safe` 的，运行时收尾时自己
/// 再跑一遍。`is_safe` 由调用方问 `tools::classify`——那张"动不动东西"的表是唯一
/// 出处，这里不另列一份名单。
///
/// **非 Safe 的不进这一份**：那会撞穿危险动作闸门，`ask` 档用户在立约时没有同意过
/// 执行任何东西。没闭合的非 Safe 判据只接模型上报的证据，界面上明标「仅上报」
pub fn rerun_schedule(
    contract: &Contract,
    rows: &[Evidence],
    is_safe: impl Fn(&str) -> bool,
) -> Vec<Criterion> {
    contract
        .criteria
        .iter()
        .zip(criterion_states(contract, rows))
        .filter(|(criterion, state)| {
            !matches!(state, CriterionState::Passed { .. })
                && criterion.command().is_some_and(&is_safe)
        })
        .map(|(criterion, _)| criterion.clone())
        .collect()
}

/// 从 `goal_report` 的 `evidence` 入参解析模型上报的证据。id 不在契约里、verdict
/// 不认识、summary 是空的，都在门口说清——不是静默收下。`output` 可省
pub fn parse_reported_evidence(
    args: &serde_json::Value,
    contract: &Contract,
    goal_id: &str,
    round: u32,
) -> Result<Vec<Evidence>, String> {
    let Some(items) = args["evidence"].as_array() else {
        return Err(
            "evidence 缺了。complete 要给每条判据一条证据（criterion_id / verdict / summary）\
             ——没有证据，'做完了'过不了门。"
                .into(),
        );
    };
    let known: std::collections::HashSet<&str> = contract
        .criteria
        .iter()
        .map(|criterion| criterion.id.as_str())
        .collect();
    let mut out = Vec::new();
    for item in items {
        let criterion_id = item["criterion_id"].as_str().unwrap_or("").trim();
        if criterion_id.is_empty() {
            return Err("evidence 里有一条没写 criterion_id。".into());
        }
        if !known.contains(criterion_id) {
            return Err(format!(
                "evidence 里的 criterion_id「{criterion_id}」不在契约里。契约的判据是：{}。",
                contract
                    .criteria
                    .iter()
                    .map(|c| c.id.as_str())
                    .collect::<Vec<_>>()
                    .join("、")
            ));
        }
        let verdict = match item["verdict"].as_str() {
            Some("pass") => Verdict::Pass,
            Some("fail") => Verdict::Fail,
            other => {
                return Err(format!(
                    "evidence「{criterion_id}」的 verdict 只认 pass 或 fail，收到 {other:?}。"
                ))
            }
        };
        let summary = item["summary"].as_str().unwrap_or("").trim().to_string();
        if summary.is_empty() {
            return Err(format!(
                "evidence「{criterion_id}」的 summary 是空的：说一句凭什么。"
            ));
        }
        out.push(Evidence {
            goal_id: goal_id.to_string(),
            criterion_id: criterion_id.to_string(),
            // 文本抄现行契约：聚合时对得上才算数，判据改过就作废
            criterion_text: contract
                .criteria
                .iter()
                .find(|c| c.id == criterion_id)
                .map(|c| c.text.clone())
                .unwrap_or_default(),
            round,
            verdict,
            summary,
            output: item["output"]
                .as_str()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string),
            verified: Verified::Reported,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(id: &str, text: &str, command: &str) -> Criterion {
        Criterion {
            id: id.into(),
            text: text.into(),
            kind: CriterionKind::Check {
                command: command.into(),
            },
        }
    }

    fn judgment(id: &str, text: &str) -> Criterion {
        Criterion {
            id: id.into(),
            text: text.into(),
            kind: CriterionKind::Judgment,
        }
    }

    fn row(
        goal_id: &str,
        criterion_id: &str,
        criterion_text: &str,
        verdict: Verdict,
        verified: Verified,
    ) -> Evidence {
        Evidence {
            goal_id: goal_id.into(),
            criterion_id: criterion_id.into(),
            criterion_text: criterion_text.into(),
            round: 1,
            verdict,
            summary: "跑过了".into(),
            output: None,
            verified,
        }
    }

    #[test]
    fn a_contract_with_no_criteria_is_refused_at_the_door() {
        let contract = Contract {
            criteria: vec![],
            constraints: vec![],
        };
        let error = validate("补齐三处对账", &contract).unwrap_err();
        assert!(error.contains("判据"), "空判据要在门口被拒：{error}");
    }

    #[test]
    fn the_limits_are_enforced_with_named_reasons() {
        let long = "长".repeat(MAX_CRITERION_CHARS + 1);
        let fat = Contract {
            criteria: vec![check("c1", &long, "npm test")],
            constraints: vec![],
        };
        assert!(validate("x", &fat).unwrap_err().contains("单条上限"));

        let many = Contract {
            criteria: (0..MAX_CRITERIA + 1)
                .map(|n| check(format!("c{n}").as_str(), "x", "y"))
                .collect(),
            constraints: vec![],
        };
        assert!(validate("x", &many).unwrap_err().contains("上限"));

        let dup = Contract {
            criteria: vec![check("c1", "甲", "y"), judgment("c1", "乙")],
            constraints: vec![],
        };
        assert!(validate("x", &dup).unwrap_err().contains("重复"));

        let no_command = Contract {
            criteria: vec![Criterion {
                id: "c1".into(),
                text: "x".into(),
                kind: CriterionKind::Check {
                    command: "  ".into(),
                },
            }],
            constraints: vec![],
        };
        assert!(validate("x", &no_command)
            .unwrap_err()
            .contains("命令是空的"));

        let blank_objective = Contract {
            criteria: vec![judgment("c1", "x")],
            constraints: vec![],
        };
        assert!(validate("   ", &blank_objective).is_err());

        let long_objective = "长".repeat(MAX_OBJECTIVE_CHARS + 1);
        assert!(validate(&long_objective, &blank_objective)
            .unwrap_err()
            .contains("上限"));
    }

    #[test]
    fn a_valid_contract_passes_the_same_table() {
        let contract = Contract {
            criteria: vec![
                check("c1", "测试全绿", "npm test"),
                judgment("c2", "文案说得清"),
            ],
            constraints: vec!["不改 src-tauri/**".into()],
        };
        validate("把台账补齐", &contract).expect("合规的契约该过");
    }

    // ---- 完成门（§4.3）----

    #[test]
    fn the_gate_passes_only_when_every_criterion_has_passing_evidence() {
        let contract = Contract {
            criteria: vec![
                check("c1", "测试全绿", "npm test"),
                judgment("c2", "文案说得清"),
            ],
            constraints: vec![],
        };
        // 两条全过：过门。verified 两档都算过——仅上报的要在界面上标出来，不是在这里拦
        let all_pass = vec![
            row("goal-1", "c1", "测试全绿", Verdict::Pass, Verified::Runtime),
            row(
                "goal-1",
                "c2",
                "文案说得清",
                Verdict::Pass,
                Verified::Reported,
            ),
        ];
        assert!(audit(&contract, &all_pass).unproven.is_empty());
        // 缺一条：打回，并说出是哪条
        let missing = vec![row(
            "goal-1",
            "c1",
            "测试全绿",
            Verdict::Pass,
            Verified::Runtime,
        )];
        let verdict = audit(&contract, &missing);
        assert_eq!(verdict.unproven.len(), 1);
        assert!(
            verdict.unproven[0].contains("c2"),
            "要说得出是哪条：{:?}",
            verdict.unproven
        );
        // 有一条最新的证据是失败：打回，说法是"失败"不是"没证据"
        let failed = vec![
            row("goal-1", "c1", "测试全绿", Verdict::Pass, Verified::Runtime),
            row(
                "goal-1",
                "c2",
                "文案说得清",
                Verdict::Fail,
                Verified::Reported,
            ),
        ];
        assert!(audit(&contract, &failed).unproven[0].contains("失败"));
    }

    #[test]
    fn the_rerun_schedule_only_takes_unclosed_safe_checks() {
        let contract = Contract {
            criteria: vec![
                check("c1", "测试全绿", "npm test"),
                check("c2", "版本号对上", "git push --force"),
                judgment("c3", "文案说得清"),
            ],
            constraints: vec![],
        };
        // c2 的命令不是 Safe 档（git push 是会动外面世界的）：**不许**进自动复跑
        let is_safe = |command: &str| !command.contains("git push");
        // 全都还没有证据：只有 Safe 的 Check 进排程
        let scheduled = rerun_schedule(&contract, &[], is_safe);
        assert_eq!(
            scheduled.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["c1"],
            "非 Safe 的 Check 与要人看的 Judgment 都不许被自动执行：{scheduled:?}"
        );
        // c1 已闭合：连它也不排了
        let closed = vec![row(
            "goal-1",
            "c1",
            "测试全绿",
            Verdict::Pass,
            Verified::Reported,
        )];
        let scheduled = rerun_schedule(&contract, &closed, is_safe);
        assert!(scheduled.is_empty(), "闭合了就不复跑：{scheduled:?}");
    }

    #[test]
    fn parse_reported_evidence_refuses_junk_at_the_door() {
        let contract = Contract {
            criteria: vec![check("c1", "测试全绿", "npm test")],
            constraints: vec![],
        };
        let parse = |evidence: serde_json::Value| {
            parse_reported_evidence(
                &serde_json::json!({ "evidence": evidence }),
                &contract,
                "goal-1",
                3,
            )
        };
        // 缺数组、id 不认识、verdict 乱写、summary 空——四种都要拒
        assert!(
            parse(serde_json::json!([])).is_ok(),
            "空数组合法：门上判覆盖度"
        );
        assert!(parse(serde_json::json!([{}])).is_err());
        assert!(parse(
            serde_json::json!([{ "criterion_id": "c9", "verdict": "pass", "summary": "x" }])
        )
        .is_err());
        assert!(parse(
            serde_json::json!([{ "criterion_id": "c1", "verdict": "maybe", "summary": "x" }])
        )
        .is_err());
        assert!(parse(serde_json::json!([{ "criterion_id": "c1", "verdict": "pass" }])).is_err());
        // 合规的一条：字段各归各位，文本抄契约、verified 标 reported
        let rows = parse(
            serde_json::json!([{ "criterion_id": "c1", "verdict": "pass", "summary": "412 过", "output": "ok" }]),
        )
        .expect("合规的该过");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].criterion_text, "测试全绿",
            "文本抄现行契约，聚合才对得上"
        );
        assert_eq!(rows[0].verified, Verified::Reported);
        assert_eq!(rows[0].round, 3);
        assert_eq!(rows[0].goal_id, "goal-1");
    }

    #[test]
    fn evidence_folds_last_write_wins_and_voids_stale_text() {
        let contract = Contract {
            criteria: vec![
                check("c1", "测试全绿", "npm test"),
                judgment("c2", "文案说得清"),
            ],
            constraints: vec![],
        };
        let row = |criterion_text: &str, verdict: Verdict, verified: Verified| Evidence {
            goal_id: "goal-1".into(),
            criterion_id: "c1".into(),
            criterion_text: criterion_text.into(),
            round: 1,
            verdict,
            summary: "跑过了".into(),
            output: None,
            verified,
        };
        // 先 fail 后 pass：后写胜出
        let rows = vec![
            row("测试全绿", Verdict::Fail, Verified::Runtime),
            row("测试全绿", Verdict::Pass, Verified::Reported),
        ];
        assert_eq!(
            criterion_states(&contract, &rows),
            vec![
                CriterionState::Passed {
                    verified: Verified::Reported
                },
                CriterionState::Open
            ],
        );
        // 判据文本一改，证据作废回 Open——不是失败，是"还没验过"
        let edited = Contract {
            criteria: vec![
                check("c1", "测试全绿且无警告", "npm test"),
                judgment("c2", "文案说得清"),
            ],
            constraints: vec![],
        };
        assert_eq!(
            criterion_states(&edited, &rows),
            vec![CriterionState::Open, CriterionState::Open],
        );
        // fail 保留为 fail，直到有更新的盖掉它
        let failed = vec![row("测试全绿", Verdict::Fail, Verified::Runtime)];
        assert_eq!(
            criterion_states(&contract, &failed)[0],
            CriterionState::Failed,
        );
        // 别的 goal_id 的证据不串门
        let mut foreign = row("测试全绿", Verdict::Pass, Verified::Runtime);
        foreign.goal_id = "goal-2".into();
        // 聚合不按 goal_id 过滤——过滤是调用方的事（这一支上只会有自己那支的行），
        // 但同一条判据在两支之间本来就靠 goal_id 分组，读侧按文本对账已经够严
        assert_eq!(
            criterion_states(&contract, &[foreign])[0],
            CriterionState::Passed {
                verified: Verified::Runtime
            },
        );
    }

    /// 证据条目要能整份往返：从 `Evidence` 落成 Custom 条目再读回来，一字不差
    #[test]
    fn an_evidence_row_round_trips_through_the_log() {
        use crate::session::entry::NewEntry;

        let evidence = Evidence {
            goal_id: "goal-9".into(),
            criterion_id: "c1".into(),
            criterion_text: "测试全绿".into(),
            round: 3,
            verdict: Verdict::Pass,
            summary: "412 个用例全过".into(),
            output: Some("test result: ok".into()),
            verified: Verified::Runtime,
        };
        let payload = EntryPayload::Custom {
            custom_type: EVIDENCE_TYPE.into(),
            data: Some(serde_json::to_value(&evidence).expect("序列化该成功")),
        };
        let mut log = SessionLog::new();
        log.append(NewEntry::new(payload), 1).expect("追加该成功");
        let rows = evidence_in_effect(&log);
        assert_eq!(rows, vec![evidence], "证据要从日志里原样读回来");
    }
}
